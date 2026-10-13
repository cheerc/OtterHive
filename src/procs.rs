//! T3 Lane A RED：`src/procs.rs` 目前僅含 test-only 私有單元測試。
//!
//! GREEN 業務實作（`process_start_time`、`stop_group`、`OwnedLeaderLease` 等）
//! 尚未授權撰寫。本檔在 `lib.rs` 接入 `mod procs;` 之前是孤立法，不參與編譯；
//! 接入後，這些測試即為 compile-RED（引用的 `super::` 項目尚不存在）。
//!
//! ## 對 GREEN 的內部 seam 提案 v2（非公開契約變更）
//!
//! coordinator 查核後要求：`probe_child` 必須獨立於 start／pgid 查詢
//! （ECHILD 是 ownership 證據，不是 lookup 值）；group 列舉與 reap 必須是
//! fallible（查詢失敗只能 `Unproven`，不可 `Stopped`）；併發測試必須用可逾時
//! 的 gate／channel，不能用無界 `Barrier::wait` 掛住整個 suite。
//!
//! ```rust,ignore
//! /// waitid WNOWAIT 觀測結果（不 reap）。
//! #[derive(Debug, Clone, PartialEq)]
//! pub(crate) enum ChildProbe {
//!     Alive,
//!     Exited { code: Option<i32>, signal: Option<i32> },
//! }
//!
//! /// 同一個 stop 核心的可注入 OS 面。ECHILD（child 已被外部 reap 或根本
//! /// 非本程序 child）以 `ErrorKind::NotFound` 回傳，與暫時性查詢失敗區分；
//! /// 兩者都使 lease 永久失效（fail-closed），但測試分別覆蓋。
//! pub(crate) trait OsAdapter: Send + Sync {
//!     fn start_time(&self, pid: i32) -> std::io::Result<u64>;
//!     fn pgid(&self, pid: i32) -> std::io::Result<i32>;
//!     fn probe_child(&self, pid: i32) -> std::io::Result<ChildProbe>;
//!     fn list_group(&self, pgid: i32) -> std::io::Result<Vec<i32>>;
//!     fn kill_group(&self, pgid: i32, sig: libc::c_int) -> std::io::Result<()>;
//!     fn reap(&self, pid: i32) -> std::io::Result<()>;
//! }
//! pub(crate) fn stop_group_with_adapter(
//!     pid: i32,
//!     expected_start: u64,
//!     evidence: GroupEvidence<'_>,
//!     term_wait: Duration,
//!     kill_wait: Duration,
//!     os: &dyn OsAdapter,
//! ) -> StopOutcome;
//! impl OwnedLeaderLease {
//!     #[cfg(test)]
//!     pub(crate) fn for_test(pid: i32, start: u64, pgid: i32) -> Self;
//!     #[cfg(test)]
//!     pub(crate) fn revoke(&self); // 僅測試用；production 不得暴露任意重建 lease
//! }
//! ```
//!
//! 核心順序提案（測試據此斷言事件全序）：驗證（lease 有效＋綁定相符＋
//! start／pgid／probe 一致）→ SIGTERM → 等待 → 列舉 group（失敗即 `Unproven`；
//! 只排除已確認退出的 owned leader，不盲目排除存活 leader）→ 仍有成員則
//! SIGKILL → 仍有成員則 `Unproven{residual_pids}` → 撤銷 lease → reap
//!（reap 失敗即 `Unproven`，不宣稱 `Stopped`）。
//! `stop_group` 公開入口以真實 adapter 呼叫 `stop_group_with_adapter`；
//! lease 需為 `Sync`（併發測試跨執行緒共用同一 lease）。
//! 若 coordinator 凍結不同的 seam，本檔測試隨 GREEN 調整，不擅改契約。

#[cfg(test)]
mod ownership_tests {
    use super::{ChildProbe, GroupEvidence, OsAdapter, OwnedLeaderLease, StopOutcome};
    use std::collections::VecDeque;
    use std::io::ErrorKind;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    const SHORT_WAIT: Duration = Duration::from_millis(10);
    const POLL_STEP: Duration = Duration::from_millis(20);
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    fn echild() -> std::io::Error {
        std::io::Error::new(ErrorKind::NotFound, "ECHILD：child 已被外部 reap 或非本程序持有")
    }

    fn transient() -> std::io::Error {
        std::io::Error::new(ErrorKind::TimedOut, "暫時性查詢失敗")
    }

    /// 可逾時的 gate：Drop 時自動開啟，測試失敗也不會永久阻塞 suite。
    struct Gate {
        open: Mutex<bool>,
        cvar: Condvar,
    }

    impl Gate {
        fn closed() -> Self {
            Self {
                open: Mutex::new(false),
                cvar: Condvar::new(),
            }
        }

        fn open(&self) {
            *self.open.lock().expect("繁中說明：gate 鎖不應中毒") = true;
            self.cvar.notify_all();
        }

        fn wait_open(&self, timeout: Duration) -> bool {
            let (guard, _) = self
                .cvar
                .wait_timeout_while(
                    self.open.lock().expect("繁中說明：gate 鎖不應中毒"),
                    timeout,
                    |is_open| !**is_open,
                )
                .expect("繁中說明：gate 鎖不應中毒");
            *guard
        }
    }

    impl Drop for Gate {
        fn drop(&mut self) {
            self.open();
        }
    }

    /// 可程式化的 fake OS adapter：照腳本回應查詢，記錄所有 signal／reap。
    /// 所有腳本用盡後重複最後一個值。
    struct FakeOs {
        starts: Mutex<VecDeque<std::io::Result<u64>>>,
        pgids: Mutex<VecDeque<std::io::Result<i32>>>,
        probes: Mutex<VecDeque<std::io::Result<ChildProbe>>>,
        lists: Mutex<VecDeque<std::io::Result<Vec<i32>>>>,
        reaps: Mutex<VecDeque<std::io::Result<()>>>,
        events: Mutex<Vec<String>>,
        kill_calls: AtomicUsize,
        reap_calls: AtomicUsize,
        /// 在第 `gate_at` 次 `kill_group` 前等待 gate（併發測試用）。
        gate: Mutex<Option<(Arc<Gate>, usize)>>,
        kill_count: AtomicUsize,
    }

    impl FakeOs {
        fn new(
            starts: Vec<std::io::Result<u64>>,
            pgids: Vec<std::io::Result<i32>>,
            probes: Vec<std::io::Result<ChildProbe>>,
            lists: Vec<std::io::Result<Vec<i32>>>,
            reaps: Vec<std::io::Result<()>>,
        ) -> Self {
            Self {
                starts: Mutex::new(starts.into()),
                pgids: Mutex::new(pgids.into()),
                probes: Mutex::new(probes.into()),
                lists: Mutex::new(lists.into()),
                reaps: Mutex::new(reaps.into()),
                events: Mutex::new(Vec::new()),
                kill_calls: AtomicUsize::new(0),
                reap_calls: AtomicUsize::new(0),
                gate: Mutex::new(None),
                kill_count: AtomicUsize::new(0),
            }
        }

        fn happy() -> Self {
            Self::new(
                vec![Ok(9_999_000_000)],
                vec![Ok(4242)],
                vec![Ok(ChildProbe::Alive)],
                vec![Ok(Vec::new())],
                vec![Ok(())],
            )
        }

        fn pop_or_last<T: Clone>(queue: &Mutex<VecDeque<T>>) -> T {
            let mut guard = queue.lock().expect("繁中說明：fake adapter 鎖不應中毒");
            if guard.len() > 1 {
                guard.pop_front().expect("繁中說明：腳本不應為空")
            } else {
                guard.front().cloned().expect("繁中說明：腳本不應為空")
            }
        }

        fn events_snapshot(&self) -> Vec<String> {
            self.events
                .lock()
                .expect("繁中說明：fake adapter 鎖不應中毒")
                .clone()
        }
    }

    impl OsAdapter for FakeOs {
        fn start_time(&self, _pid: i32) -> std::io::Result<u64> {
            Self::pop_or_last(&self.starts)
        }

        fn pgid(&self, _pid: i32) -> std::io::Result<i32> {
            Self::pop_or_last(&self.pgids)
        }

        fn probe_child(&self, _pid: i32) -> std::io::Result<ChildProbe> {
            Self::pop_or_last(&self.probes)
        }

        fn list_group(&self, _pgid: i32) -> std::io::Result<Vec<i32>> {
            Self::pop_or_last(&self.lists)
        }

        fn kill_group(&self, pgid: i32, sig: libc::c_int) -> std::io::Result<()> {
            let count = self.kill_count.fetch_add(1, Ordering::SeqCst);
            if let Some((gate, at)) =
                self.gate.lock().expect("繁中說明：fake adapter 鎖不應中毒").clone()
            {
                if count == at && !gate.wait_open(TEST_TIMEOUT) {
                    return Err(std::io::Error::new(ErrorKind::TimedOut, "gate 等待逾時"));
                }
            }
            self.kill_calls.fetch_add(1, Ordering::SeqCst);
            let name = if sig == libc::SIGTERM { "TERM" } else { "KILL" };
            self.events
                .lock()
                .expect("繁中說明：fake adapter 鎖不應中毒")
                .push(format!("signal {name} pgid={pgid}"));
            Ok(())
        }

        fn reap(&self, _pid: i32) -> std::io::Result<()> {
            self.reap_calls.fetch_add(1, Ordering::SeqCst);
            self.events
                .lock()
                .expect("繁中說明：fake adapter 鎖不應中毒")
                .push("reap".to_owned());
            Self::pop_or_last(&self.reaps)
        }
    }

    fn lease_for_test() -> OwnedLeaderLease {
        OwnedLeaderLease::for_test(4242, 9_999_000_000, 4242)
    }

    fn stop_once(
        pid: i32,
        start: u64,
        evidence: GroupEvidence<'_>,
        os: &dyn OsAdapter,
    ) -> StopOutcome {
        super::stop_group_with_adapter(pid, start, evidence, SHORT_WAIT, SHORT_WAIT, os)
    }

    #[test]
    fn valid_lease_signals_term_and_reaps() {
        // 正面案例：身分相符、TERM 後 group 已空 → Stopped（不升級）。
        let os = FakeOs::happy();
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(
                outcome,
                StopOutcome::Stopped {
                    escalated_to_kill: false
                }
            ),
            "繁中說明：有效 lease 配合 TERM 應回 Stopped 且不升級，實際為 {outcome:?}"
        );
        assert_eq!(
            os.events_snapshot(),
            vec!["signal TERM pgid=4242".to_owned(), "reap".to_owned()],
            "繁中說明：事件全序應為 validation→signal→revoke→reap"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：清理完成應 reap 恰一次"
        );
    }

    #[test]
    fn revoked_lease_on_reused_group_makes_zero_signals() {
        // 同一舊 pgid 回報無關的新 group（啟動時間不同），且 lease 已撤銷：
        // 由真正 stop 核心入口帶入 → Unproven，signal 與 reap 呼叫數皆為 0。
        let mut os = FakeOs::happy();
        os.starts = Mutex::new(vec![Ok(1_111_000_000)].into());
        let lease = lease_for_test();
        lease.revoke();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(outcome, StopOutcome::Unproven { .. }),
            "繁中說明：已撤銷 lease 遇重用 group 應回 Unproven，實際為 {outcome:?}"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：撤銷後不得發任何 signal"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：撤銷後不得 reap"
        );
    }

    #[test]
    fn external_reap_with_matching_start_and_pgid_permanently_revokes() {
        // 關鍵：start／pgid 始終相符，但 probe 回 ECHILD（child 已被外部 reap）：
        // lease 永久撤銷；之後 probe 恢復 Alive，同一 lease 再次 stop 仍拒絕，
        // 全程零 signal。這不用 start_time=None 冒充 ECHILD。
        let os = FakeOs::new(
            vec![Ok(9_999_000_000)],
            vec![Ok(4242)],
            vec![Err(echild()), Ok(ChildProbe::Alive)],
            vec![Ok(vec![4242])],
            vec![Ok(())],
        );
        let lease = lease_for_test();
        let first = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(first, StopOutcome::Unproven { .. }),
            "繁中說明：probe ECHILD 應回 Unproven（不可 signal 無主 child），實際為 {first:?}"
        );
        let second = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(
                second,
                StopOutcome::Unproven { .. } | StopOutcome::IdentityMismatch
            ),
            "繁中說明：失效是永久的，probe 恢復後仍應拒絕，實際為 {second:?}"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：失效路徑全程不得發 signal"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：失效路徑不得 reap"
        );
    }

    #[test]
    fn start_and_pgid_lookup_failures_are_unproven_not_stopped() {
        // start／pgid 查詢失敗（非 ECHILD，是暫時性錯誤）同樣 fail-closed：
        // Unproven、零 signal，且不得回 Stopped。
        let os_start = FakeOs::new(
            vec![Err(transient())],
            vec![Ok(4242)],
            vec![Ok(ChildProbe::Alive)],
            vec![Ok(Vec::new())],
            vec![Ok(())],
        );
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os_start);
        assert!(
            matches!(outcome, StopOutcome::Unproven { .. }),
            "繁中說明：start 查詢失敗應回 Unproven 而非 Stopped，實際為 {outcome:?}"
        );
        assert_eq!(
            os_start.kill_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：start 查詢失敗不得發 signal"
        );

        let os_pgid = FakeOs::new(
            vec![Ok(9_999_000_000)],
            vec![Err(transient())],
            vec![Ok(ChildProbe::Alive)],
            vec![Ok(Vec::new())],
            vec![Ok(())],
        );
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os_pgid);
        assert!(
            matches!(outcome, StopOutcome::Unproven { .. }),
            "繁中說明：pgid 查詢失敗應回 Unproven 而非 Stopped，實際為 {outcome:?}"
        );
        assert_eq!(
            os_pgid.kill_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：pgid 查詢失敗不得發 signal"
        );
    }

    #[test]
    fn lease_binding_mismatch_makes_zero_signals() {
        // lease 綁定 (4242／start／4242)，傳入 pid 4243：綁定不符 → IdentityMismatch，
        // 即使 adapter 回傳相符值也不得 signal（production 不得以傳入參數覆蓋綁定）。
        let os = FakeOs::happy();
        let lease = lease_for_test();
        let outcome = stop_once(4243, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(outcome, StopOutcome::IdentityMismatch),
            "繁中說明：lease 綁定 pid 與傳入不符應回 IdentityMismatch，實際為 {outcome:?}"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：綁定不符不得發任何 signal"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：綁定不符不得 reap"
        );
    }

    #[test]
    fn group_enumeration_failure_is_unproven_with_term_already_sent() {
        // TERM 已發、等待後列舉 group 失敗：Unproven（不可 Stopped），不 reap。
        let os = FakeOs::new(
            vec![Ok(9_999_000_000)],
            vec![Ok(4242)],
            vec![Ok(ChildProbe::Alive)],
            vec![Err(transient())],
            vec![Ok(())],
        );
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(outcome, StopOutcome::Unproven { .. }),
            "繁中說明：group 列舉失敗應回 Unproven 而非 Stopped，實際為 {outcome:?}"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：列舉失敗前 TERM 已發一次"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：列舉失敗不得 reap"
        );
    }

    #[test]
    fn reap_failure_is_unproven_not_stopped() {
        // group 已確認清空但 reap 失敗：不可宣稱 Stopped；lease 同樣 fail-closed，
        // 之後重試仍拒絕且無新 signal。
        let os = FakeOs::new(
            vec![Ok(9_999_000_000)],
            vec![Ok(4242)],
            vec![Ok(ChildProbe::Alive)],
            vec![Ok(Vec::new())],
            vec![Err(transient())],
        );
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(outcome, StopOutcome::Unproven { .. }),
            "繁中說明：reap 失敗應回 Unproven 而非 Stopped，實際為 {outcome:?}"
        );
        let retry = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(
                retry,
                StopOutcome::Unproven { .. } | StopOutcome::IdentityMismatch
            ),
            "繁中說明：reap 失敗後 lease 應永久失效，實際為 {retry:?}"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：重試不得發新 signal"
        );
    }

    #[test]
    fn live_leader_is_never_excluded_from_members() {
        // probe 始終 Alive，列舉含 leader 本人：不可盲目排除存活 leader；
        // TERM＋KILL 後仍有成員 → Unproven，residuals 含 leader 與 child，不 reap。
        let os = FakeOs::new(
            vec![Ok(9_999_000_000)],
            vec![Ok(4242)],
            vec![Ok(ChildProbe::Alive)],
            vec![Ok(vec![4242, 5000])],
            vec![Ok(())],
        );
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        match outcome {
            StopOutcome::Unproven { residual_pids } => {
                assert!(
                    residual_pids.contains(&4242) && residual_pids.contains(&5000),
                    "繁中說明：residuals 應含存活 leader 與 child，實際為 {residual_pids:?}"
                );
            }
            other => panic!("繁中說明：存活 leader 未清空應回 Unproven，實際為 {other:?}"),
        }
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            2,
            "繁中說明：應發 TERM 後升級 KILL"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：group 未清空不得 reap"
        );
    }

    #[test]
    fn exited_owned_leader_is_excluded_after_cleanup() {
        // probe 確認 leader 已退出：列舉第二次只剩 zombie leader 時，排除它即算
        // 清空 → Stopped（不升級）並 reap。
        let os = FakeOs::new(
            vec![Ok(9_999_000_000)],
            vec![Ok(4242)],
            vec![Ok(ChildProbe::Exited {
                code: Some(0),
                signal: None
            })],
            vec![Ok(vec![4242, 5001]), Ok(vec![4242])],
            vec![Ok(())],
        );
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(
                outcome,
                StopOutcome::Stopped {
                    escalated_to_kill: false
                }
            ),
            "繁中說明：只剩已退出 leader 時應回 Stopped 且不升級，實際為 {outcome:?}"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：應只發一次 TERM"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：清理完成應 reap 恰一次"
        );
    }

    #[test]
    fn concurrent_stops_serialize_on_one_lock_with_exactly_one_reap() {
        // 真正 production 同一 lifecycle lock 的 revoke／reap 競爭（不是兩個
        // 獨立 stop 各做各的）：stop A 在 validation 之後、signal 之前被可控
        // seam（gate）擋住並持有鎖；stop B 以同一 lease 進入必須序列化等待，
        // 不可並行驗證／signal。放行後事件全序為 signal→reap，reap 恰一次，
        // A 得 Stopped、B 因 lease 已撤銷得 Unproven，撤銷後零新 signal。
        // 全程用可逾時 channel／gate，不用無界 Barrier；執行緒以 channel 回傳
        // 結果並 join，assert 失敗時 gate 的 Drop 也會放行，不掛 suite。
        let os = Arc::new(FakeOs::happy());
        let gate = Arc::new(Gate::closed());
        *os.gate.lock().expect("繁中說明：fake adapter 鎖不應中毒") =
            Some((Arc::clone(&gate), 0));
        let lease = Arc::new(lease_for_test());

        let spawn_stop = |os: Arc<FakeOs>, lease: Arc<OwnedLeaderLease>| {
            let (tx, rx) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                let outcome = super::stop_group_with_adapter(
                    4242,
                    9_999_000_000,
                    GroupEvidence::OwnedLeader(&lease),
                    SHORT_WAIT,
                    SHORT_WAIT,
                    &*os,
                );
                let _ = tx.send(outcome);
            });
            (handle, rx)
        };

        let (handle_a, rx_a) = spawn_stop(Arc::clone(&os), Arc::clone(&lease));
        // 確認 A 已到達 signal 點（持有鎖、卡在 gate），再啟動 B，保證重疊。
        let checkpoint = Instant::now();
        while os.kill_calls.load(Ordering::SeqCst) < 1 {
            assert!(
                checkpoint.elapsed() < TEST_TIMEOUT,
                "繁中說明：stop A 應在期限內到達 signal 點"
            );
            std::thread::sleep(POLL_STEP);
        }
        let (handle_b, rx_b) = spawn_stop(Arc::clone(&os), Arc::clone(&lease));
        gate.open();

        let outcome_a = rx_a
            .recv_timeout(TEST_TIMEOUT)
            .expect("繁中說明：stop A 應在期限內完成");
        let outcome_b = rx_b
            .recv_timeout(TEST_TIMEOUT)
            .expect("繁中說明：stop B 應在期限內完成（鎖不可死鎖）");
        handle_a.join().expect("繁中說明：執行緒 A 應正常結束");
        handle_b.join().expect("繁中說明：執行緒 B 應正常結束");

        assert!(
            matches!(
                outcome_a,
                StopOutcome::Stopped {
                    escalated_to_kill: false
                }
            ),
            "繁中說明：先驗證的 A 應回 Stopped 且不升級，實際為 {outcome_a:?}"
        );
        assert!(
            matches!(outcome_b, StopOutcome::Unproven { .. }),
            "繁中說明：後到的 B 應因 lease 已撤銷回 Unproven，實際為 {outcome_b:?}"
        );
        assert_eq!(
            os.events_snapshot(),
            vec!["signal TERM pgid=4242".to_owned(), "reap".to_owned()],
            "繁中說明：全序應為 validation→signal→revoke→reap，無交錯 signal"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：全程應只發一次 signal"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：reap 應恰一次"
        );
    }
}
