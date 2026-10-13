//! T3 Lane A RED：`src/procs.rs` 目前僅含 test-only 私有單元測試。
//!
//! GREEN 業務實作（`process_start_time`、`stop_group`、`OwnedLeaderLease` 等）
//! 尚未授權撰寫。本檔在 `lib.rs` 接入 `mod procs;` 之前是孤立法，不參與編譯；
//! 接入後，這些測試即為 compile-RED（引用的 `super::` 項目尚不存在）。
//!
//! ## 對 GREEN 的內部 seam 提案（非公開契約變更）
//!
//! 以下單元測試要求 production 提供同一個 stop 核心的可注入版本，以便用
//! fake OS adapter 驗證所有權語意，而不發真 signal、不測另一條簡化 stub：
//!
//! ```rust,ignore
//! pub(crate) trait OsAdapter: Send + Sync {
//!     fn start_time(&self, pid: i32) -> Option<u64>;
//!     fn pgid(&self, pid: i32) -> Option<i32>;
//!     fn kill_group(&self, pgid: i32, sig: libc::c_int) -> std::io::Result<()>;
//!     fn group_has_members(&self, pgid: i32, exclude_pid: i32) -> bool;
//!     fn reap(&self, pid: i32) -> bool;
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
//!     pub(crate) fn revoke(&self); // 測試用：模擬清理完成後的撤銷狀態
//! }
//! ```
//!
//! `stop_group` 公開入口以真實 adapter 呼叫 `stop_group_with_adapter`；
//! lease 需為 `Sync`（barrier 測試跨執行緒共用同一 lease）。
//! 若 coordinator 凍結不同的 seam，本檔測試隨 GREEN 調整，不擅改契約。

#[cfg(test)]
mod ownership_tests {
    use super::{GroupEvidence, OsAdapter, OwnedLeaderLease, StopOutcome};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::Duration;

    const TERM_WAIT: Duration = Duration::from_secs(2);
    const KILL_WAIT: Duration = Duration::from_secs(1);

    /// 可程式化的 fake OS adapter：照腳本回應查詢，記錄所有 signal／reap。
    struct FakeOs {
        /// 每次 `start_time` 呼叫依序回傳的值（用盡後重複最後一個）。
        start_script: Mutex<VecDeque<Option<u64>>>,
        /// 每次 `pgid` 呼叫依序回傳的值。
        pgid_script: Mutex<VecDeque<Option<i32>>>,
        /// `group_has_members` 依序回傳的值。
        members_script: Mutex<VecDeque<bool>>,
        /// 事件日誌：`signal TERM`／`signal KILL`／`reap`，驗證全序用。
        events: Mutex<Vec<String>>,
        kill_calls: AtomicUsize,
        reap_calls: AtomicUsize,
        /// 在第 N 次 `kill_group` 前等待的 barrier（barrier 測試用）。
        kill_barrier: Mutex<Option<(Arc<Barrier>, usize)>>,
        kill_count: AtomicUsize,
    }

    impl FakeOs {
        fn new(
            starts: Vec<Option<u64>>,
            pgids: Vec<Option<i32>>,
            members: Vec<bool>,
        ) -> Self {
            Self {
                start_script: Mutex::new(starts.into()),
                pgid_script: Mutex::new(pgids.into()),
                members_script: Mutex::new(members.into()),
                events: Mutex::new(Vec::new()),
                kill_calls: AtomicUsize::new(0),
                reap_calls: AtomicUsize::new(0),
                kill_barrier: Mutex::new(None),
                kill_count: AtomicUsize::new(0),
            }
        }

        fn pop_or_last(queue: &Mutex<VecDeque<Option<u64>>>) -> Option<u64> {
            let mut guard = queue.lock().expect("繁中說明：fake adapter 鎖不應中毒");
            if guard.len() > 1 {
                guard.pop_front().expect("繁中說明：腳本不應為空")
            } else {
                guard.front().copied().expect("繁中說明：腳本不應為空")
            }
        }
    }

    impl OsAdapter for FakeOs {
        fn start_time(&self, _pid: i32) -> Option<u64> {
            Self::pop_or_last(&self.start_script)
        }

        fn pgid(&self, _pid: i32) -> Option<i32> {
            let mut guard = self
                .pgid_script
                .lock()
                .expect("繁中說明：fake adapter 鎖不應中毒");
            if guard.len() > 1 {
                guard.pop_front().expect("繁中說明：腳本不應為空")
            } else {
                guard.front().copied().expect("繁中說明：腳本不應為空")
            }
        }

        fn kill_group(&self, pgid: i32, sig: libc::c_int) -> std::io::Result<()> {
            let count = self.kill_count.fetch_add(1, Ordering::SeqCst);
            if let Some((barrier, at)) = self
                .kill_barrier
                .lock()
                .expect("繁中說明：fake adapter 鎖不應中毒")
                .clone()
            {
                if count == at {
                    barrier.wait();
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

        fn group_has_members(&self, _pgid: i32, _exclude_pid: i32) -> bool {
            let mut guard = self
                .members_script
                .lock()
                .expect("繁中說明：fake adapter 鎖不應中毒");
            if guard.len() > 1 {
                guard.pop_front().expect("繁中說明：腳本不應為空")
            } else {
                *guard.front().expect("繁中說明：腳本不應為空")
            }
        }

        fn reap(&self, _pid: i32) -> bool {
            self.reap_calls.fetch_add(1, Ordering::SeqCst);
            self.events
                .lock()
                .expect("繁中說明：fake adapter 鎖不應中毒")
                .push("reap".to_owned());
            true
        }
    }

    fn lease_for_test() -> OwnedLeaderLease {
        OwnedLeaderLease::for_test(4242, 9_999_000_000, 4242)
    }

    #[test]
    fn valid_lease_signals_term_and_reaps() {
        // 正面案例：身分相符、TERM 後 group 已空 → Stopped（不升級）。
        let os = FakeOs::new(vec![Some(9_999_000_000)], vec![Some(4242)], vec![false]);
        let lease = lease_for_test();
        let outcome = super::stop_group_with_adapter(
            4242,
            9_999_000_000,
            GroupEvidence::OwnedLeader(&lease),
            TERM_WAIT,
            KILL_WAIT,
            &os,
        );
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
            os.kill_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：應恰發一次 SIGTERM"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：清理完成應 reap 一次"
        );
    }

    #[test]
    fn revoked_lease_on_reused_group_makes_zero_signals() {
        // 同一舊 pgid 回報無關的新 group（啟動時間不同），且 lease 已撤銷：
        // 由真正 stop 核心入口帶入 → Unproven，signal 呼叫數為 0。
        let os = FakeOs::new(vec![Some(1_111_000_000)], vec![Some(4242)], vec![true]);
        let lease = lease_for_test();
        lease.revoke();
        let outcome = super::stop_group_with_adapter(
            4242,
            9_999_000_000,
            GroupEvidence::OwnedLeader(&lease),
            TERM_WAIT,
            KILL_WAIT,
            &os,
        );
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
    fn echild_and_lookup_failure_permanently_invalidate() {
        // 首次查詢成功、之後 ECHILD（None）：lease 永久失效；即使 adapter
        // 恢復回傳相符值，同一 lease 再次 stop 仍拒絕，且全程零 signal。
        let os = FakeOs::new(
            vec![Some(9_999_000_000), None, Some(9_999_000_000)],
            vec![Some(4242)],
            vec![true],
        );
        let lease = lease_for_test();
        let first = super::stop_group_with_adapter(
            4242,
            9_999_000_000,
            GroupEvidence::OwnedLeader(&lease),
            TERM_WAIT,
            KILL_WAIT,
            &os,
        );
        assert!(
            matches!(
                first,
                StopOutcome::Unproven { .. } | StopOutcome::IdentityMismatch
            ),
            "繁中說明：查詢失敗應拒絕 stop，實際為 {first:?}"
        );
        let second = super::stop_group_with_adapter(
            4242,
            9_999_000_000,
            GroupEvidence::OwnedLeader(&lease),
            TERM_WAIT,
            KILL_WAIT,
            &os,
        );
        assert!(
            matches!(
                second,
                StopOutcome::Unproven { .. } | StopOutcome::IdentityMismatch
            ),
            "繁中說明：失效是永久的，恢復查詢後仍應拒絕，實際為 {second:?}"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：失效路徑全程不得發 signal"
        );
    }

    #[test]
    fn validation_signal_and_reap_are_serialized() {
        // 同一 lifecycle 鎖：驗證後、signal 前不可 reap；撤銷後不再 signal。
        // 以 barrier 讓第二個 stop 呼叫與第一個的 signal 競爭，用事件全序驗證。
        let os = Arc::new(FakeOs::new(
            vec![Some(9_999_000_000)],
            vec![Some(4242)],
            vec![true, false],
        ));
        let barrier = Arc::new(Barrier::new(2));
        *os.kill_barrier
            .lock()
            .expect("繁中說明：fake adapter 鎖不應中毒") = Some((Arc::clone(&barrier), 0));
        let lease = Arc::new(lease_for_test());

        let os_clone = Arc::clone(&os);
        let lease_clone = Arc::clone(&lease);
        let competing = std::thread::spawn(move || {
            barrier.wait();
            super::stop_group_with_adapter(
                4242,
                9_999_000_000,
                GroupEvidence::OwnedLeader(&lease_clone),
                TERM_WAIT,
                KILL_WAIT,
                &*os_clone,
            )
        });
        let first = super::stop_group_with_adapter(
            4242,
            9_999_000_000,
            GroupEvidence::OwnedLeader(&lease),
            TERM_WAIT,
            KILL_WAIT,
            &*os,
        );
        let second = competing.join().expect("繁中說明：競爭執行緒應正常結束");

        let events = os
            .events
            .lock()
            .expect("繁中說明：fake adapter 鎖不應中毒");
        let reap_pos = events.iter().position(|event| event == "reap");
        let signals_after_reap = match reap_pos {
            Some(pos) => events[pos + 1..]
                .iter()
                .filter(|event| event.starts_with("signal"))
                .count(),
            None => 0,
        };
        assert_eq!(
            signals_after_reap, 0,
            "繁中說明：reap（撤銷）後不得有新 signal，事件為 {events:?}"
        );
        assert!(
            os.reap_calls.load(Ordering::SeqCst) <= 1,
            "繁中說明：reap 至多一次，實際 {} 次",
            os.reap_calls.load(Ordering::SeqCst)
        );
        assert!(
            matches!(first, StopOutcome::Stopped { .. } | StopOutcome::Unproven { .. }),
            "繁中說明：第一個 stop 應有確定結果，實際為 {first:?}"
        );
        assert!(
            matches!(
                second,
                StopOutcome::Stopped { .. } | StopOutcome::Unproven { .. }
            ),
            "繁中說明：競爭 stop 應有確定結果（不死鎖、不崩潰），實際為 {second:?}"
        );
    }
}
