//! T3 Lane A RED：terminal Session 的訂閱／seq／resize（`tests/terminal_session.rs`）。
//!
//! 依 `docs/plans/p1a-implementation.md` T3 與契約 §7.1，只覆蓋 Session 顯示
//! 部分；frame 編解碼屬 coordinator 的 lane（`tests/frame.rs` 不在本 lane）。
//! Screen 的私有單元測試由 Screen lane 負責；此處只經 `Session` 公開介面驗收。
//!
//! 全檔只在 macOS 編譯。等待一律有上限並回報最後狀態。

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use otterhive::procs::{stop_group, GroupEvidence, StopOutcome};
use otterhive::terminal::session::{Session, SessionConfig, StreamEvent};
use otterhive::terminal::Snapshot;
use tempfile::TempDir;

const POLL_STEP: Duration = Duration::from_millis(20);
const LONG_TIMEOUT: Duration = Duration::from_secs(10);
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

mod support {
    use super::*;

    pub fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/backends")
            .join(name)
    }

    pub fn poll_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        loop {
            if cond() {
                return true;
            }
            if start.elapsed() >= timeout {
                return cond();
            }
            std::thread::sleep(POLL_STEP);
        }
    }

    pub struct FixtureSession {
        pub session: Session,
        cleaned: bool,
        _tmp: TempDir,
    }
    impl FixtureSession {
        pub fn spawn(program: &str, extra_env: &[(&str, String)]) -> Self {
            Self::spawn_with_queue(program, extra_env, 256)
        }

        pub fn spawn_with_queue(
            program: &str,
            extra_env: &[(&str, String)],
            subscriber_queue: usize,
        ) -> Self {
            let tmp = TempDir::new().expect("繁中說明：測試需建立隔離暫存目錄");
            let mut env = vec![
                ("HOME".to_owned(), tmp.path().to_string_lossy().into_owned()),
                (
                    "OTTERHIVE_HOME".to_owned(),
                    tmp.path().to_string_lossy().into_owned(),
                ),
            ];
            for (key, value) in extra_env {
                env.push(((*key).to_owned(), (*value).clone()));
            }
            let cfg = SessionConfig {
                program: fixture(program).to_string_lossy().into_owned(),
                args: Vec::new(),
                cwd: tmp.path().to_path_buf(),
                env,
                size: (120, 40),
                generation: 1,
                subscriber_queue,
            };
            let session = Session::spawn(cfg).expect("繁中說明：fixture 應可經 Session 啟動");
            Self {
                session,
                cleaned: false,
                _tmp: tmp,
            }
        }

        /// 以 owned lease 停止；stop 核心完成回收後標記 Child 已釋放。
        /// Drop 之後不再重複清理（會再做一次已釋放檢查，不發 signal）。
        pub fn stop_owned(&mut self) -> StopOutcome {
            let outcome = stop_group(
                self.session.pid(),
                self.session.start_time(),
                GroupEvidence::OwnedLeader(self.session.owned_leader_lease()),
                Duration::from_secs(2),
                Duration::from_secs(1),
            );
            if matches!(outcome, StopOutcome::Stopped { .. }) {
                self.session.mark_child_released();
            }
            self.cleaned = true;
            outcome
        }
    }

    /// Drop：以 owned lease 做短等待 stop，失敗中途也有回收（不斷言）。
    impl Drop for FixtureSession {
        fn drop(&mut self) {
            if self.cleaned {
                return;
            }
            let _ = stop_group(
                self.session.pid(),
                self.session.start_time(),
                GroupEvidence::OwnedLeader(self.session.owned_leader_lease()),
                Duration::from_secs(1),
                Duration::from_secs(1),
            );
        }
    }

    /// 向 Session 寫入一行（含換行）。
    pub fn write_line(session: &Session, line: &str) {
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        session
            .write_input(&bytes)
            .expect("繁中說明：測試應可向 Session 寫入輸入");
    }

    /// 讀到第一個含 `needle` 的 Output bytes；回傳其 seq。
    pub fn recv_output_containing(
        sub: &otterhive::terminal::session::Subscriber,
        needle: &[u8],
        what: &str,
    ) -> u64 {
        let start = Instant::now();
        loop {
            let event = sub
                .recv_timeout(RECV_TIMEOUT)
                .expect("繁中說明：測試應在期限內收到 Output");
            if let StreamEvent::Output { seq, bytes } = event {
                if bytes
                    .windows(needle.len().max(1))
                    .any(|window| window == needle)
                {
                    return seq;
                }
            }
            assert!(start.elapsed() < LONG_TIMEOUT, "繁中說明：等待 {what} 逾時");
        }
    }
}

use support::*;

#[test]
fn output_reaches_screen_and_plain_text() {
    let holder = FixtureSession::spawn("capture_stub.sh", &[]);
    let session = &holder.session;
    // capture_stub 印 ready 標記＋CJK／ANSI 後結束；等待畫面出現內容。
    assert!(
        poll_until(LONG_TIMEOUT, || {
            session
                .plain_text()
                .iter()
                .any(|line| line.contains("ANSI"))
        }),
        "繁中說明：capture_stub 的 ANSI 行應出現在 plain_text，最後畫面為 {:?}",
        session.plain_text()
    );
    let lines = session.plain_text();
    assert!(
        lines.iter().any(|line| line.contains("中文")),
        "繁中說明：CJK 寬字元應出現在 plain_text"
    );
    assert!(
        !lines.iter().any(|line| line.contains('\x1b')),
        "繁中說明：plain_text 必須去除 ANSI 轉義序列"
    );
}

#[test]
fn snapshot_then_output_sequence_has_no_gap_or_duplicate() {
    let holder = FixtureSession::spawn("echo_agent.sh", &[]);
    let session = &holder.session;
    let (snapshot, sub): (Snapshot, _) = session.subscribe();
    assert_eq!(
        snapshot.generation, 1,
        "繁中說明：Snapshot 應標示正確 generation"
    );
    write_line(session, "hello-seq");
    // 第一個 Output seq 必須是 snapshot.seq + 1，之後連續遞增。
    let mut expected = snapshot.seq + 1;
    for _ in 0..2 {
        let event = sub
            .recv_timeout(RECV_TIMEOUT)
            .expect("繁中說明：訂閱後應收到 Output");
        match event {
            StreamEvent::Output { seq, .. } => {
                assert_eq!(seq, expected, "繁中說明：Output seq 應連續無缺口不重複");
                expected += 1;
            }
            StreamEvent::Resync { .. } => {
                panic!("繁中說明：正常訂閱不應收到 Resync");
            }
            StreamEvent::Ended => {
                panic!("繁中說明：程序不應在此結束");
            }
        }
    }
}

#[test]
fn sequence_advances_without_subscribers_and_snapshot_includes_it() {
    let holder = FixtureSession::spawn("echo_agent.sh", &[]);
    let session = &holder.session;
    // 無訂閱時先輸出：seq 仍前進（計數點在 Session 鎖，不等 client）。
    write_line(session, "before-subscribe");
    assert!(
        poll_until(LONG_TIMEOUT, || {
            session
                .plain_text()
                .iter()
                .any(|line| line.contains("echo: before-subscribe"))
        }),
        "繁中說明：無訂閱時的輸出應仍進入畫面"
    );
    let (snapshot, sub): (Snapshot, _) = session.subscribe();
    assert!(
        snapshot.seq > 0,
        "繁中說明：無訂閱期間的輸出應讓 Snapshot.seq 大於 0"
    );
    write_line(session, "after-subscribe");
    let event = sub
        .recv_timeout(RECV_TIMEOUT)
        .expect("繁中說明：訂閱後應收到 Output");
    match event {
        StreamEvent::Output { seq, .. } => {
            assert_eq!(
                seq,
                snapshot.seq + 1,
                "繁中說明：之後 Output 應從 Snapshot.seq 加一接續"
            );
        }
        other => panic!("繁中說明：應收到 Output，實際為 {other:?}"),
    }
}

#[test]
fn slow_subscriber_gets_resync_and_reader_never_blocks() {
    // 小佇列＋flood：不讀取的訂閱者佇列滿；另一個訂閱者與 plain_text 仍前進；
    // 停滯者恢復讀取後先收到 Resync（含 Snapshot），後續 Output 從其 seq+1；
    // Resync 不增加全域 seq。
    let log_dir = TempDir::new().expect("繁中說明：測試需建立隔離暫存目錄");
    let log_path = log_dir
        .path()
        .join("flood.log")
        .to_string_lossy()
        .into_owned();
    let holder = FixtureSession::spawn_with_queue(
        "echo_agent.sh",
        &[("OTTERHIVE_FIXTURE_LOG", log_path)],
        4,
    );
    let session = &holder.session;
    let (_stalled_snapshot, stalled) = session.subscribe();
    let (_fast_snapshot, fast) = session.subscribe();
    write_line(session, "flood");
    // 等 flood 開始（fast 訂閱者看到 flood-output）。
    let _ = recv_output_containing(&fast, b"flood-output", "flood 輸出");
    // 停滯者不讀：持續一段時間讓其佇列溢出；期間 fast 仍收到輸出（reader 未阻塞）。
    let fast_seq_before = recv_output_containing(&fast, b"flood-output", "flood 持續輸出");
    assert!(
        fast_seq_before > 0,
        "繁中說明：fast client 應持續收到輸出（reader 未被慢訂閱者阻塞）"
    );
    assert!(
        !session.plain_text().is_empty(),
        "繁中說明：flood 期間 plain_text 應持續更新"
    );

    // 恢復讀取停滯者。
    //
    // 原測試在這裡 `write_line("stop")` 再 `write_line("post-resync-probe")`，
    // 然後期待「Resync 之後的 Output」。它低頻 flaky 的原因是**前提依賴排程**。
    // 實作的規則（契約 §7.1）是：佇列曾滿的訂閱者，在 `Resync` 被取出之前到達的
    // 每個 chunk 都只把既有 Resync 的邊界更新為最新、**不排** `Output`。所以若
    // `stop` 造成的最後一批 flood 輸出與探測行的回音都在停滯者讀走 Resync
    // **之前**到達，`resync_pending` 就會一直為 true 直到最後一次更新為止；
    // 之後沒有新資料，就永遠不會有「Resync.seq + 1」的 Output。分岔點是 stop
    // 與 probe 誰先被 reader 讀到——那是排程，不是實作的行為。
    //
    // 修法是拿掉「先停 flood」這一步，讓 flood 持續輸出。這不只是「很可能」，
    // 而是恆等式：`resync_pending` 為 true 時，每個到達的 chunk 都在同一個鎖
    // 區塊內把待送 Resync 的 Snapshot 更新成當下的 `state.seq`；因此取出 Resync
    // 的那一刻 `snapshot.seq == state.seq`，而 `resync_pending` 也同時被清除，
    // 於是下一個 chunk 排出的 Output 必然是 `snapshot.seq + 1`。停止 flood 反而
    // 會破壞這個不變量（停止造成的最後一批輸出若在 Resync 被取出前到達，就會
    // 把邊界推走、之後又沒有新資料）。
    //
    // 判準不變且不放寬：Output 必須是「最後一個 Resync.seq 加一」，且必須先收到
    // 過 Resync。引流迴圈保留——flood 期間停滯者還可能再次溢出成新邊界。
    let mut resync_seq = 0u64;
    let mut saw_resync = false;
    let start = Instant::now();
    loop {
        assert!(
            start.elapsed() < LONG_TIMEOUT,
            "繁中說明：應在期限內收到接續 Output，最後邊界為 {resync_seq}"
        );
        match stalled.recv_timeout(RECV_TIMEOUT) {
            Ok(StreamEvent::Resync { snapshot }) => {
                saw_resync = true;
                resync_seq = snapshot.seq;
            }
            Ok(StreamEvent::Output { seq, .. }) => {
                assert!(
                    saw_resync,
                    "繁中說明：佇列滿的停滯者恢復後應先收到 Resync 才收到 Output"
                );
                assert_eq!(
                    seq,
                    resync_seq + 1,
                    "繁中說明：Resync 後 Output 應為該 Snapshot.seq 加一"
                );
                break;
            }
            Ok(StreamEvent::Ended) => {
                panic!("繁中說明：fixture 不應在此結束");
            }
            // 逾時：flood 持續輸出時不該發生（`RECV_TIMEOUT` 5 秒內應有新
            // chunk）；發生就繼續等，總期限由上面的 `LONG_TIMEOUT` 守住。
            Err(_) => {
                assert!(
                    saw_resync,
                    "繁中說明：停滯者逾時前應已收到 Resync（佇列曾滿的邊界）"
                );
            }
        }
    }
    // log_dir 在此 Drop，隔離暫存檔隨測試結束刪除。
    let _ = &log_dir;
}

#[test]
fn stalled_subscriber_gets_resync_before_ended_when_program_exits() {
    // 組合情境：佇列曾滿（`resync_pending`）之後程式結束。reader 的 EOF 走
    // `mark_ended`；若那裡不先補 Resync，這個停滯者只會收到 Ended，從未取得
    // 被丟棄輸出之後的畫面邊界，重連後無從知道最後狀態。
    //
    // 判準：停滯者（容量 1、期間完全不讀）在收到 Ended 之前至少要拿到一個
    // Resync（其 Snapshot 帶最後畫面已含的 seq），且 Ended 是最後一個事件。
    // 殘留的 Output 合法出現：溢出前已排隊的 Output 仍會先被讀出。
    let holder = FixtureSession::spawn_with_queue("echo_agent.sh", &[], 1);
    let session = &holder.session;
    let (_snapshot, stalled) = session.subscribe();
    // flood 持續輸出，容量 1 的佇列只要有兩個 chunk 送達就必然溢出成
    // resync_pending。門檻刻意取得低（3 行）：這裡要證明的是「溢出發生過」，
    // 不是 flood 能跑多快；行數門檻過高只會讓本前提在負載下變成 flake。
    write_line(session, "flood");
    assert!(
        poll_until(LONG_TIMEOUT, || {
            session
                .plain_text()
                .iter()
                .filter(|line| line.contains("flood-output-line"))
                .count()
                >= 3
        }),
        "繁中說明：flood 應持續輸出到足以讓佇列溢出，最後畫面為 {:?}",
        session.plain_text()
    );
    // 收掉 flood（主迴圈仍活著），再停止整個程序 → reader EOF → mark_ended。
    write_line(session, "stop");
    assert!(
        poll_until(LONG_TIMEOUT, || {
            session
                .plain_text()
                .iter()
                .any(|line| line.contains("echo: stop"))
        }),
        "繁中說明：stop 後應看到 echo: stop，最後畫面為 {:?}",
        session.plain_text()
    );
    let outcome = stop_group(
        session.pid(),
        session.start_time(),
        GroupEvidence::OwnedLeader(session.owned_leader_lease()),
        Duration::from_secs(2),
        Duration::from_secs(1),
    );
    assert!(
        matches!(outcome, StopOutcome::Stopped { .. }),
        "繁中說明：echo_agent 應可正常停止，實際為 {outcome:?}"
    );
    assert!(
        poll_until(LONG_TIMEOUT, || session.exit_status().is_some()),
        "繁中說明：停止後應在期限內觀測到 exit（不 reap）"
    );

    // 停滯者現在才開始讀取：Ended 之前必須至少出現一個 Resync。
    let start = Instant::now();
    let mut saw_resync = false;
    let mut resync_seq = 0u64;
    loop {
        assert!(
            start.elapsed() < LONG_TIMEOUT,
            "繁中說明：應在期限內收到 Ended，最後邊界為 {resync_seq}"
        );
        match stalled.recv_timeout(RECV_TIMEOUT) {
            Ok(StreamEvent::Resync { snapshot }) => {
                saw_resync = true;
                resync_seq = snapshot.seq;
                assert_eq!(
                    snapshot.generation, 1,
                    "繁中說明：Ended 前的 Resync 應帶正確 generation"
                );
            }
            Ok(StreamEvent::Output { .. }) => {}
            Ok(StreamEvent::Ended) => break,
            Err(_) => continue,
        }
    }
    assert!(
        saw_resync,
        "繁中說明：佇列曾滿又遇程式結束時，停滯者必須在 Ended 之前收到 Resync"
    );
    assert!(
        resync_seq > 0,
        "繁中說明：Ended 前的 Resync 應帶最後畫面已含的 seq，實際 {resync_seq}"
    );
    // Ended 之後沒有更多事件（訂閱串流已收尾）。
    assert!(
        matches!(
            stalled.recv_timeout(RECV_TIMEOUT),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "繁中說明：Ended 應是最後一個事件，之後只有 timeout"
    );
}

#[test]
fn child_release_is_recorded_and_later_child_touch_is_refused() {
    // B1：stop 核心完成回收後標記 Child 已釋放；此後任何再觸及 Child 的
    // 路徑（`with_child`）一律拒絕，不再落到 portable-pty 的
    // wait／try_wait／kill。
    let mut holder = FixtureSession::spawn("echo_agent.sh", &[]);
    assert!(
        !holder.session.child_is_released(),
        "繁中說明：spawn 後 Child 未被釋放"
    );
    // 釋放前 `with_child` 可通行（Child 仍由 Session 持有），並傳回 body 的值。
    let before = holder
        .session
        .with_child(|_child| 42u32)
        .expect("繁中說明：釋放前應可觸及 Child");
    assert_eq!(before, 42, "繁中說明：with_child 應把 body 的結果傳回");

    let outcome = holder.stop_owned();
    assert!(
        matches!(outcome, StopOutcome::Stopped { .. }),
        "繁中說明：echo_agent 應可正常停止，實際為 {outcome:?}"
    );
    assert!(
        holder.session.child_is_released(),
        "繁中說明：Stopped 後 Child 應標記為已釋放"
    );
    // 已釋放：with_child 必須拒絕（不 panic、不觸及 Child）。
    let refused = holder
        .session
        .with_child(|_child| panic!("繁中說明：不應觸及已釋放的 Child"));
    assert!(
        refused.is_err(),
        "繁中說明：Child 已釋放後 with_child 必須回 Err"
    );
}

#[test]
fn resize_updates_pty_and_screen() {
    let holder = FixtureSession::spawn("echo_agent.sh", &[]);
    let session = &holder.session;
    session.resize(100, 30).expect("繁中說明：resize 應成功");
    // 先 resize 再 subscribe：斷言 Snapshot 已反映新尺寸；之後才寫 size，
    // 避免回覆先被 snapshot 包含而再無 Output（正確實作也會 timeout）。
    // Snapshot 形狀依 plan T3（generation／seq／cols／rows／ansi）。
    let (snapshot, sub): (Snapshot, _) = session.subscribe();
    assert_eq!(
        snapshot.cols, 100,
        "繁中說明：Snapshot 應反映 resize 後的 cols"
    );
    assert_eq!(
        snapshot.rows, 30,
        "繁中說明：Snapshot 應反映 resize 後的 rows"
    );
    write_line(session, "size");
    // PTY 輸出可能分 chunk 到達：累積後以完整行比對，trim 後恰為 "30 100"
    //（stty size 印「行 列」），不是兩個不相干的數字子字串。
    let start = Instant::now();
    let mut buffered: Vec<u8> = Vec::new();
    let mut matched = false;
    while start.elapsed() < LONG_TIMEOUT {
        match sub.recv_timeout(RECV_TIMEOUT) {
            Ok(StreamEvent::Output { bytes, .. }) => {
                buffered.extend_from_slice(&bytes);
                let text = String::from_utf8_lossy(&buffered);
                if text.lines().any(|line| line.trim() == "30 100") {
                    matched = true;
                    break;
                }
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert!(
        matched,
        "繁中說明：resize 到 100x30 後應收到恰為「30 100」的完整行，最後觀測為 {:?}",
        String::from_utf8_lossy(&buffered)
    );
}
