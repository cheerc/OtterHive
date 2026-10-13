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

use otterhive::procs::{stop_group, GroupEvidence};
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
            Self { session, _tmp: tmp }
        }
    }

    /// Drop：以 owned lease 做短等待 stop，失敗中途也有回收（不斷言）。
    impl Drop for FixtureSession {
        fn drop(&mut self) {
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
    // 恢復讀取停滯者：應先收到 Resync。
    write_line(session, "stop");
    let start = Instant::now();
    let mut saw_resync = false;
    let mut resync_seq = 0u64;
    while start.elapsed() < LONG_TIMEOUT {
        match stalled.recv_timeout(RECV_TIMEOUT) {
            Ok(StreamEvent::Resync { snapshot }) => {
                saw_resync = true;
                resync_seq = snapshot.seq;
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert!(saw_resync, "繁中說明：佇列滿的停滯者恢復後應先收到 Resync");
    let next = stalled
        .recv_timeout(RECV_TIMEOUT)
        .expect("繁中說明：Resync 後應有後續 Output");
    match next {
        StreamEvent::Output { seq, .. } => {
            assert_eq!(
                seq,
                resync_seq + 1,
                "繁中說明：Resync 後 Output 應為該 Snapshot.seq 加一"
            );
        }
        other => panic!("繁中說明：Resync 後應為 Output，實際為 {other:?}"),
    }
    // log_dir 在此 Drop，隔離暫存檔隨測試結束刪除。
    let _ = &log_dir;
}

#[test]
fn resize_updates_pty_and_screen() {
    let holder = FixtureSession::spawn("echo_agent.sh", &[]);
    let session = &holder.session;
    session.resize(100, 30).expect("繁中說明：resize 應成功");
    write_line(session, "size");
    let (_snapshot, sub) = session.subscribe();
    // PTY 輸出可能分 chunk 到達，累積後再比對，避免跨 chunk 邊界漏判。
    let start = Instant::now();
    let mut buffered: Vec<u8> = Vec::new();
    let mut ok = false;
    while start.elapsed() < LONG_TIMEOUT {
        match sub.recv_timeout(RECV_TIMEOUT) {
            Ok(StreamEvent::Output { bytes, .. }) => {
                buffered.extend_from_slice(&bytes);
                let text = String::from_utf8_lossy(&buffered);
                if text.contains("30") && text.contains("100") {
                    ok = true;
                    break;
                }
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert!(
        ok,
        "繁中說明：resize 到 100x30 後 echo size 應回 30 100（行 列）"
    );
}
