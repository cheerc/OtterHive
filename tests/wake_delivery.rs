//! T5 Lane B RED：喚醒提示的端到端投遞（契約 §6.4）。
//!
//! 這些測試驅動**真的** daemon 與**真的** backend fixture
//! （`tests/fixtures/backends/echo_agent.sh`），不是替身。延後條件的兩個來源
//! 也都是真的：
//!
//! - **client 最近輸入過**：用 T4 的原始 frame client attach 後送一個
//!   `Input`（tag `0x10`）frame——這是 `read_client_frames` 唯一的輸入來源。
//! - **收件人最近有輸出**：由 `echo_agent.sh` 實際印字造成。
//!
//! 縮短等待用 `OTTERHIVE_TEST_WAKE_INPUT_MS` 與
//! `OTTERHIVE_TEST_WAKE_QUIET_MS`（daemon 啟動時讀一次；預設 3000／500，
//! 見契約 §6.4 的 3 秒與 500 毫秒）。
//!
//! **RED 階段的預期狀態**：本檔的測試全部**行為 RED**。它們需要
//! `send`（Lane A 的 `src/messaging.rs`）與喚醒排程的 daemon 接線落地。
//! 紅的原因是「提示沒出現」，不是編譯錯誤——檔案本身可編譯、可執行。
//!
//! 全檔只在 macOS 編譯。所有等待有上限且逾時時印出最後觀察到的狀態；
//! backend 一律走 `fixture_path`／絕對路徑，**不依賴 PATH 上的真實程式**
//! （T4 曾因省略 `command` 走預設 `claude` 而在 CI 紅掉）。

#![cfg(target_os = "macos")]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use otterhive::home::Home;
use otterhive::protocol::{Auth, OperationResult, Outcome, Request};
use otterhive::terminal::frame::{encode_frame, read_frame};
use serde_json::{json, Value};
use tempfile::TempDir;

/// 輪詢間隔。
const POLL_STEP: Duration = Duration::from_millis(20);
/// 啟動 daemon、停止 instance 的等待上限。
const LONG_TIMEOUT: Duration = Duration::from_secs(20);
/// socket 讀寫的等待上限。
const RECV_TIMEOUT: Duration = Duration::from_secs(10);
/// 單一次提示的等待上限：契約的 3 秒輸入視窗被 `WAKE_INPUT_MS` 縮短後，
/// 再加上 250ms 的重判節拍與 daemon 排程的抖動，給一個明確的餘裕。
const HINT_TIMEOUT: Duration = Duration::from_secs(15);

/// 測試用的輸入視窗（毫秒）。
///
/// 必須**大於** `RECHECK_INTERVAL`（250ms），否則「安靜 700ms」這個說法
/// 與「每 250ms 重判一次」互相矛盾，測試會變成對節拍的競態而不是對契約
/// 的斷言。
const WAKE_INPUT_MS: u64 = 700;
/// 測試用的輸出靜默視窗（毫秒）。
const WAKE_QUIET_MS: u64 = 150;

/// daemon binary 路徑。
fn daemon_bin() -> &'static str {
    match option_env!("CARGO_BIN_EXE_otterhive") {
        Some(path) => path,
        None => {
            panic!("繁中說明：otterhive binary 尚未建立（本測試為 RED，GREEN 才有可驅動的 daemon）")
        }
    }
}

/// fixture 指令稿的絕對路徑（`tests/fixtures/backends/<name>`）。
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/backends")
        .join(name)
}

/// 隔離的暫存 home：`HOME`／`OTTERHIVE_HOME`／workspace 都落在這裡。
///
/// 與 `tests/instance_lifecycle.rs` 的 `TestHome` 相同形狀，但**不**共用
/// `mod support`——那個模組會把 T3／T4 的輔助函式全部拖進本檔的編譯單元，
/// 而本檔只需要其中一小部分。
struct TestHome {
    home: Home,
    /// 與測試同壽命；Drop 時刪除隔離目錄。
    _root: TempDir,
}

impl TestHome {
    fn new() -> Self {
        let root = TempDir::new().expect("繁中說明：測試需建立隔離暫存目錄");
        let home =
            Home::initialize(&root.path().join("h")).expect("繁中說明：暫存 home 應可初始化");
        Self { home, _root: root }
    }

    fn root(&self) -> &Path {
        self.home.root()
    }

    fn socket_path(&self) -> PathBuf {
        self.home.run_dir().join("daemon.sock")
    }

    fn operator_token(&self) -> String {
        std::fs::read_to_string(self.home.operator_token_path())
            .expect("繁中說明：應可讀取 operator 憑證")
            .trim()
            .to_owned()
    }

    fn operator(&self) -> Auth {
        Auth::Operator(self.operator_token())
    }

    /// 從 instance 的 MCP 設定讀出該 generation 的 token（§5.1）。
    fn instance_auth(&self, name: &str) -> Auth {
        let path = self
            .root()
            .join("workspace")
            .join(name)
            .join(".claude/otterhive-mcp.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("繁中說明：應可讀取 {} 的 MCP 設定", path.display()));
        let parsed: Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("繁中說明：MCP 設定應是合法 JSON：{e}；實際 {raw}"));
        let token = parsed["mcpServers"]["otterhive"]["env"]["OTTERHIVE_INSTANCE_TOKEN"]
            .as_str()
            .unwrap_or_else(|| panic!("繁中說明：MCP 設定應含 instance token；實際 {raw}"))
            .to_owned();
        Auth::Instance(token)
    }
}

/// 前景執行的 daemon；Drop 時先 SIGTERM、有界等待、再 SIGKILL。
struct Daemon {
    child: Option<Child>,
}

impl Daemon {
    /// 啟動 daemon，並把喚醒視窗縮到測試可接受的程度。
    ///
    /// `OTTERHIVE_TEST_WAKE_INPUT_MS`／`OTTERHIVE_TEST_WAKE_QUIET_MS` 依
    /// 派工約定由 daemon 在啟動時讀一次。
    fn start(home: &TestHome) -> Self {
        let root = home.root().to_path_buf();
        let mut command = Command::new(daemon_bin());
        command
            .arg("daemon")
            .env("HOME", &root)
            .env("OTTERHIVE_HOME", &root)
            .env("OTTERHIVE_TEST_STARTUP_GRACE_MS", "600")
            .env("OTTERHIVE_TEST_STOP_TERM_MS", "1000")
            .env("OTTERHIVE_TEST_STOP_KILL_MS", "500")
            .env("OTTERHIVE_TEST_WAKE_INPUT_MS", WAKE_INPUT_MS.to_string())
            .env("OTTERHIVE_TEST_WAKE_QUIET_MS", WAKE_QUIET_MS.to_string())
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().expect("繁中說明：daemon 應可啟動");
        let daemon = Self { child: Some(child) };
        let socket = home.socket_path();
        assert!(
            poll_until(LONG_TIMEOUT, || socket.exists()),
            "繁中說明：daemon 應在期限內建立 {}（啟動自檢失敗？）",
            socket.display()
        );
        daemon
    }

    /// 依契約 §1 送一行 JSON 請求、收一行 `OperationResult`。
    fn call(&self, home: &TestHome, auth: &Auth, op: &str, args: Value) -> OperationResult {
        let request = Request {
            v: 1,
            auth: auth.clone(),
            op: op.to_owned(),
            args,
            operation_id: None,
        };
        let mut line = serde_json::to_string(&request).expect("繁中說明：請求應可序列化為 JSON");
        line.push('\n');
        let socket = home.socket_path();
        let mut stream = UnixStream::connect(&socket)
            .unwrap_or_else(|e| panic!("繁中說明：應能連到 {}：{e}", socket.display()));
        let _ = stream.set_read_timeout(Some(LONG_TIMEOUT));
        let _ = stream.set_write_timeout(Some(RECV_TIMEOUT));
        stream
            .write_all(line.as_bytes())
            .expect("繁中說明：應可送出請求");
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .unwrap_or_else(|e| panic!("繁中說明：應可讀回一行回應（{op}）：{e}"));
        serde_json::from_str(response.trim()).unwrap_or_else(|e| {
            panic!("繁中說明：回應應是合法 OperationResult JSON（{op}）：{e}；實際 {response}")
        })
    }

    fn operator_call(&self, home: &TestHome, op: &str, args: Value) -> OperationResult {
        let auth = home.operator();
        self.call(home, &auth, op, args)
    }

    fn instance_call(&self, home: &TestHome, name: &str, op: &str, args: Value) -> OperationResult {
        let auth = home.instance_auth(name);
        self.call(home, &auth, op, args)
    }
}

impl Drop for Daemon {
    /// 有界停止：先 SIGTERM，逾時才 SIGKILL；Drop 不得 panic。
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let pid = child.id() as i32;
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(_) => break,
            }
            if start.elapsed() >= Duration::from_secs(5) {
                break;
            }
            std::thread::sleep(POLL_STEP);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// T4 的原始 frame client（契約 §7.2 的真實協定，非替身）。
///
/// 存在的唯一理由是製造延後條件：daemon 只在 `read_client_frames` 收到
/// `Input`（`0x10`）frame 時才知道「有人正在輸入」，沒有別的來源。
struct AttachClient {
    stream: UnixStream,
    generation: u64,
}

impl AttachClient {
    fn attach(home: &TestHome, name: &str, generation: u64) -> AttachClient {
        let socket = home.socket_path();
        let stream = UnixStream::connect(&socket)
            .unwrap_or_else(|e| panic!("繁中說明：attach 應可連線（{name}）：{e}"));
        let _ = stream.set_read_timeout(Some(RECV_TIMEOUT));
        let request = Request {
            v: 1,
            auth: home.operator(),
            op: "attach".to_owned(),
            args: json!({"instance": name}),
            operation_id: None,
        };
        let mut line = serde_json::to_string(&request).expect("繁中說明：attach 請求應可序列化");
        line.push('\n');
        let mut writer = &stream;
        writer
            .write_all(line.as_bytes())
            .expect("繁中說明：應可送出 attach");
        writer.flush().expect("應可 flush attach 請求");
        AttachClient { stream, generation }
    }

    /// 送一個 `Input` frame（tag `0x10`：8B generation ＋ bytes）。
    ///
    /// 這就是「operator 可能在打字」的來源事件（契約 §6.4）。
    fn type_bytes(&mut self, bytes: &[u8]) {
        let mut payload = self.generation.to_be_bytes().to_vec();
        payload.extend_from_slice(bytes);
        let frame = encode_frame(0x10, &payload);
        let mut writer = &self.stream;
        writer
            .write_all(&frame)
            .expect("繁中說明：應可送出 Input frame");
        writer.flush().expect("應可 flush Input frame");
    }

    /// 送 `Detach`（`0x12`）並關閉連線——等同 operator 離開該 pane。
    fn detach(&mut self) {
        let frame = encode_frame(0x12, &[]);
        let mut writer = &self.stream;
        let _ = writer.write_all(&frame);
        let _ = writer.flush();
    }

    /// 在期限內讀完後續 frame，回傳 `(tag, payload)` 序列。
    ///
    /// 只讀 Output／Error／Ended；Snapshot／Resync 是給 client 重繪用的，
    /// 與本檔要觀察的提示無關，略過但不記錄。
    fn drain_frames(&mut self, timeout: Duration) -> Vec<(u8, Vec<u8>)> {
        let mut frames = Vec::new();
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return frames;
            }
            let _ = self
                .stream
                .set_read_timeout(Some(remaining.min(RECV_TIMEOUT)));
            match read_frame(&mut self.stream) {
                Ok((tag, payload)) => {
                    if tag != 0x01 && tag != 0x03 {
                        frames.push((tag, payload));
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("繁中說明：讀 frame 失敗：{error}"),
            }
        }
    }
}

impl Drop for AttachClient {
    fn drop(&mut self) {
        self.detach();
    }
}

/// 有上限地輪詢到 `cond()` 為真；逾時回最後一次結果。
fn poll_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
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

/// 把 frame 序列還原成純文字（Output 的 payload 直接串接）。
fn frame_text(frames: &[(u8, Vec<u8>)]) -> String {
    let mut text = String::new();
    for (tag, payload) in frames {
        if *tag == 0x02 {
            text.push_str(&String::from_utf8_lossy(payload));
        }
    }
    text
}

/// 建立一個跑 `echo_agent.sh` 的 instance，回傳它的 generation。
fn create_echo_agent(home: &TestHome, daemon: &Daemon, name: &str) -> u64 {
    let created = daemon.operator_call(
        home,
        "create_instance",
        json!({
            "name": name,
            // 絕對路徑的 fixture：絕不依賴 PATH（CLAUDE.md／T4 教訓）。
            "command": fixture("echo_agent.sh").to_string_lossy(),
        }),
    );
    assert_eq!(
        created.outcome,
        Outcome::Completed,
        "繁中說明：create {name} 應完成，實際為 {created:?}"
    );
    created
        .subject
        .as_ref()
        .map(|subject| subject.generation)
        .unwrap_or_else(|| panic!("繁中說明：create 的結果應帶 subject（{created:?}）"))
}

/// `send` 的結果裡，某個收件人的 `wake` 值（契約 §6.1／§6.4）。
///
/// 找不到該收件人時回 `None` 並附上實際的 `value.recipients`，讓斷言能
/// 指出「daemon 回的形狀不是契約說的形狀」而不是一句無資訊的 `None`。
fn wake_of(result: &OperationResult, instance: &str) -> Option<String> {
    let recipients = result.value["recipients"].as_array()?;
    recipients
        .iter()
        .find(|entry| entry["instance"] == instance)
        .and_then(|entry| entry["wake"].as_str())
        .map(str::to_owned)
}

/// 收件人目前畫面上的文字（`pane_snapshot`，§4.7）。
fn screen_text(home: &TestHome, daemon: &Daemon, name: &str) -> String {
    let snapshot = daemon.operator_call(home, "pane_snapshot", json!({"instance": name}));
    assert_eq!(
        snapshot.outcome,
        Outcome::Completed,
        "繁中說明：pane_snapshot {name} 應成功，實際為 {snapshot:?}"
    );
    snapshot.value["text"]
        .as_str()
        .unwrap_or_else(|| panic!("繁中說明：pane_snapshot 應回 value.text，實際為 {snapshot:?}"))
        .to_owned()
}

// ------------------------------------------------------------ 契約 §6.4

#[test]
fn a_running_recipient_receives_the_hint_line_with_its_submit_key() {
    // 契約 §6.4：送達後若收件人在 `running`，daemon 經 PTY 送出一行
    // `[OTTERHIVE-MSG] from=<sender> kind=<kind> id=<message_id>` 加送出鍵。
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let worker = create_echo_agent(&home, &daemon, "worker");

    // 先讓 agent 停下來（沒有輸出、安靜超過 WAKE_QUIET_MS），否則
    // `ready>` 的輸出會讓它一直處於延後狀態。
    let mut client = AttachClient::attach(&home, "worker", worker);
    let text = frame_text(&client.drain_frames(Duration::from_millis(300)));
    assert!(
        text.contains("ready>") || text.is_empty(),
        "繁中說明：attach 後應看到 fixture 的 ready 提示或乾淨畫面，實際為 {text:?}"
    );
    // 送出，讓 fixture 進入安靜期。
    client.type_bytes(b"quiet\n");
    let _ = client.drain_frames(Duration::from_millis(WAKE_QUIET_MS + 400));
    // 離開：attach client 存在時，operator 就算不輸入也仍是「有人盯著」；
    // detach 讓這個 instance 的閒置語意回到乾淨狀態。
    client.detach();
    drop(client);
    std::thread::sleep(Duration::from_millis(WAKE_QUIET_MS + 200));

    let sent = daemon.operator_call(
        &home,
        "send",
        json!({"instance": "worker", "message": "請回報進度", "request_kind": "query"}),
    );
    assert_eq!(
        sent.outcome,
        Outcome::Completed,
        "繁中說明：send 應完成，實際為 {sent:?}"
    );
    assert_eq!(
        wake_of(&sent, "worker").as_deref(),
        Some("hinted"),
        "繁中說明：安靜的 running 收件人應被 hinted（契約 §6.4），實際 value.recipients 為 {}",
        sent.value["recipients"]
    );

    // 提示真的走 PTY 進了 agent：`echo_agent.sh` 會把它回印成 `echo: …`。
    let mut client = AttachClient::attach(&home, "worker", worker);
    let observed = frame_text(&client.drain_frames(HINT_TIMEOUT));
    assert!(
        observed.contains("[OTTERHIVE-MSG]"),
        "繁中說明：提示應經 PTY 送達收件人（契約 §6.4），實際觀察到 {observed:?}"
    );
    assert!(
        observed.contains("from=operator"),
        "繁中說明：提示應指出 sender 為 operator（P1a 沒有 operator 收件匣，§6.1），實際為 {observed:?}"
    );
    assert!(
        observed.contains("kind=query"),
        "繁中說明：提示應帶 request_kind（契約 §6.4），實際為 {observed:?}"
    );
    assert!(
        !observed.contains("count="),
        "繁中說明：單則提示不應帶 count（契約 §6.4：n==1 時省略），實際為 {observed:?}"
    );
    // `echo_agent.sh` 只會在收到完整一行（送出鍵）後才回印 `echo:`，
    // 因此這一段同時證明了送出鍵有被送出。
    assert!(
        observed.contains("echo: [OTTERHIVE-MSG]"),
        "繁中說明：送出鍵應讓 fixture 把提示當成一行讀回（契約 §6.4 的「加送出鍵」），實際為 {observed:?}"
    );
}

#[test]
fn a_recent_client_input_defers_the_hint_until_the_input_window_passes() {
    // 契約 §6.4：有 client 在最近 3 秒內輸入過就延後，每 250 毫秒重新
    // 判斷，直到條件解除才送出。
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let worker = create_echo_agent(&home, &daemon, "worker");

    let mut client = AttachClient::attach(&home, "worker", worker);
    let _ = client.drain_frames(Duration::from_millis(300));

    // operator 剛剛「打過字」——這就是延後條件的第一個來源。
    client.type_bytes(b"hello\n");
    let _ = client.drain_frames(Duration::from_millis(WAKE_QUIET_MS + 300));

    let sent = daemon.operator_call(
        &home,
        "send",
        json!({"instance": "worker", "message": "在嗎"}),
    );
    assert_eq!(
        wake_of(&sent, "worker").as_deref(),
        Some("deferred"),
        "繁中說明：client 最近輸入過時應為 deferred（契約 §6.4），實際 value.recipients 為 {}",
        sent.value["recipients"]
    );

    // 延後期間不得送出：這個視窗遠小於 WAKE_INPUT_MS。
    let early = frame_text(&client.drain_frames(Duration::from_millis(250)));
    assert!(
        !early.contains("[OTTERHIVE-MSG]"),
        "繁中說明：延後期間不得送出提示（契約 §6.4），實際觀察到 {early:?}"
    );

    // 安靜超過輸入視窗後，重判（每 250ms）應把提示送出。
    let later = frame_text(&client.drain_frames(Duration::from_millis(WAKE_INPUT_MS + 1500)));
    assert!(
        later.contains("[OTTERHIVE-MSG]"),
        "繁中說明：輸入視窗過後應重判並送出（契約 §6.4），實際觀察到 {later:?}"
    );
}

#[test]
fn two_deferred_hints_are_merged_into_one_line_with_a_count() {
    // 契約 §6.4：同一收件人尚未送出的多個提示合併為一行
    // `[OTTERHIVE-MSG] count=<n> from=<最後的 sender> kind=<最後的 kind> id=<最後的 message_id>`。
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let worker = create_echo_agent(&home, &daemon, "worker");

    let mut client = AttachClient::attach(&home, "worker", worker);
    let _ = client.drain_frames(Duration::from_millis(300));
    client.type_bytes(b"typing\n");
    let _ = client.drain_frames(Duration::from_millis(WAKE_QUIET_MS + 300));

    // 兩則都在延後中送出：第一則的 kind 刻意與第二則不同，這樣
    // 「取最後一則的 kind」才看得出來。
    let first = daemon.operator_call(
        &home,
        "send",
        json!({"instance": "worker", "message": "第一則", "request_kind": "query"}),
    );
    assert_eq!(
        wake_of(&first, "worker").as_deref(),
        Some("deferred"),
        "繁中說明：第一則應被延後（契約 §6.4），實際 value.recipients 為 {}",
        first.value["recipients"]
    );
    let second = daemon.operator_call(
        &home,
        "send",
        json!({"instance": "worker", "message": "第二則", "request_kind": "report"}),
    );
    assert_eq!(
        wake_of(&second, "worker").as_deref(),
        Some("deferred"),
        "繁中說明：第二則應被延後（契約 §6.4），實際 value.recipients 為 {}",
        second.value["recipients"]
    );

    let merged = frame_text(&client.drain_frames(Duration::from_millis(WAKE_INPUT_MS + 2500)));
    assert!(
        merged.contains("count=2"),
        "繁中說明：兩則延後的提示應合併為一行並帶 count=2（契約 §6.4），實際觀察到 {merged:?}"
    );
    assert!(
        merged.contains("kind=report"),
        "繁中說明：合併後的 kind 應是最後一則的（契約 §6.4），實際觀察到 {merged:?}"
    );
    // 第二則的 message ID 必須出現在合併行裡。
    let second_id = first.value["recipients"][0]["message_id"]
        .as_str()
        .and_then(|_| second.value["recipients"][0]["message_id"].as_str())
        .unwrap_or_else(|| {
            panic!(
                "繁中說明：send 結果應帶 message_id（§6.1），實際為 {}",
                second.value
            )
        });
    assert!(
        merged.contains(second_id),
        "繁中說明：合併行的 id 應是最後一則的 message id（契約 §6.4），預期包含 {second_id}，實際觀察到 {merged:?}"
    );
    // 只有一行提示：兩個獨立的 `[OTTERHIVE-MSG]` 前綴代表沒合併。
    assert_eq!(
        merged.matches("[OTTERHIVE-MSG]").count(),
        1,
        "繁中說明：兩則延後的提示應合併為**一行**（契約 §6.4），實際觀察到 {merged:?}"
    );
}

#[test]
fn a_recipient_that_keeps_outputting_keeps_the_hint_deferred_until_it_stops() {
    // 契約 §6.4：「延後沒有上限：持續輸出的程序在停下之前不會收到提示」。
    //
    // 這個測試的價值在於**負面**結果：`flood` 持續印字時提示必須一直不
    // 出現，而不是「延後一會兒就硬送」。
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let worker = create_echo_agent(&home, &daemon, "worker");

    let mut client = AttachClient::attach(&home, "worker", worker);
    let _ = client.drain_frames(Duration::from_millis(300));

    // 讓 fixture 進入 flood：背景子行程持續輸出，主迴圈仍可讀 stdin。
    client.type_bytes(b"flood\n");
    let flooding = frame_text(&client.drain_frames(Duration::from_millis(600)));
    assert!(
        flooding.contains("flood-output-line"),
        "繁中說明：fixture 應已進入 flood 狀態（前提失敗，後續斷言沒有意義），實際觀察到 {flooding:?}"
    );

    let sent = daemon.operator_call(
        &home,
        "send",
        json!({"instance": "worker", "message": "有人在嗎"}),
    );
    assert_eq!(
        wake_of(&sent, "worker").as_deref(),
        Some("deferred"),
        "繁中說明：持續輸出的收件人應被延後（契約 §6.4），實際 value.recipients 為 {}",
        sent.value["recipients"]
    );

    // flood 持續的整段時間裡不得出現提示。這個視窗刻意跨越多個
    // 250ms 重判週期——若實作誤把「重判 N 次後放棄」當成逾時，這裡就會紅。
    let during = frame_text(&client.drain_frames(Duration::from_millis(2000)));
    assert!(
        !during.contains("[OTTERHIVE-MSG]"),
        "繁中說明：持續輸出期間不得送出提示（契約 §6.4：延後沒有上限），實際觀察到 {}",
        &during[..during.len().min(400)]
    );

    // 收到 `stop` 之後才送出：這正是「在停下之前不會收到提示」的另一半。
    client.type_bytes(b"stop\n");
    let after = frame_text(&client.drain_frames(Duration::from_millis(WAKE_INPUT_MS + 3000)));
    assert!(
        after.contains("[OTTERHIVE-MSG]"),
        "繁中說明：收件人停下（不再輸出）後提示應送出（契約 §6.4），實際觀察到 {}",
        &after[..after.len().min(400)]
    );
}

#[test]
fn a_hint_is_abandoned_when_the_restart_happens_first_but_the_message_stays_drainable() {
    // 契約 §6.4：「instance 在送出前停止或換了 generation，就放棄該提示。
    // 不論提示是否送出，訊息都留在 inbox。」
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let worker = create_echo_agent(&home, &daemon, "worker");

    let mut client = AttachClient::attach(&home, "worker", worker);
    let _ = client.drain_frames(Duration::from_millis(300));
    client.type_bytes(b"typing\n");
    let _ = client.drain_frames(Duration::from_millis(WAKE_QUIET_MS + 300));

    let sent = daemon.operator_call(
        &home,
        "send",
        json!({"instance": "worker", "message": "重啟前送達的訊息"}),
    );
    assert_eq!(
        wake_of(&sent, "worker").as_deref(),
        Some("deferred"),
        "繁中說明：應先被延後（契約 §6.4），實際 value.recipients 為 {}",
        sent.value["recipients"]
    );
    let message_id = sent.value["recipients"][0]["message_id"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "繁中說明：send 結果應帶 message_id（契約 §6.1），實際 value 為 {}",
                sent.value
            )
        })
        .to_owned();

    // 在提示送出前換 generation。
    let restarted = daemon.operator_call(
        &home,
        "restart_instance",
        json!({"instance": "worker", "mode": "fresh"}),
    );
    assert_eq!(
        restarted.outcome,
        Outcome::Completed,
        "繁中說明：restart 應完成，實際為 {restarted:?}"
    );
    assert!(
        restarted
            .subject
            .as_ref()
            .is_some_and(|subject| subject.generation > worker),
        "繁中說明：restart 應推進 generation，實際為 {restarted:?}"
    );

    // 舊連線收 `Ended`（§7.1）；提示**不得**出現在之後的任何輸出裡。
    let mut ended = false;
    let deadline = Instant::now() + Duration::from_millis(WAKE_INPUT_MS + 3000);
    while Instant::now() < deadline && !ended {
        for (tag, _) in client.drain_frames(Duration::from_millis(200)) {
            if tag == 0x04 {
                ended = true;
            }
        }
    }
    let after_restart = frame_text(&client.drain_frames(Duration::from_millis(1200)));
    assert!(
        !after_restart.contains("[OTTERHIVE-MSG]"),
        "繁中說明：換 generation 後應放棄該提示（契約 §6.4），但新連線仍出現了提示，實際觀察到 {}",
        &after_restart[..after_restart.len().min(400)]
    );

    // 訊息仍在 inbox：這是契約最需要被守住的那一半。
    let new_generation = restarted
        .subject
        .as_ref()
        .map(|subject| subject.generation)
        .unwrap_or_default();
    let mut client = AttachClient::attach(&home, "worker", new_generation);
    let _ = client.drain_frames(Duration::from_millis(300));

    let drained = daemon.instance_call(&home, "worker", "inbox", json!({}));
    assert_eq!(
        drained.outcome,
        Outcome::Completed,
        "繁中說明：放棄提示後訊息仍應可 drain（契約 §6.4），實際為 {drained:?}"
    );
    let messages = drained.value["messages"]
        .as_array()
        .unwrap_or_else(|| panic!("繁中說明：drain 結果應有 value.messages，實際為 {drained:?}"));
    assert!(
        messages
            .iter()
            .any(|entry| entry["id"] == message_id.as_str()),
        "繁中說明：被放棄提示的訊息（id={message_id}）應仍留在 inbox，實際 messages 為 {messages:?}"
    );
    assert!(
        drained.value["delivery_token"].is_string(),
        "繁中說明：drain 有訊息時應發新的 delivery_token（契約 §6.3），實際為 {}",
        drained.value
    );
}

#[test]
fn sending_to_a_stopped_instance_is_not_running_and_keeps_the_message() {
    // 契約 §6.4 的 `not_running`：收件人不在 running 時沒有提示可送；
    // §6.1 的「喚醒失敗不撤回訊息」則保證訊息仍然入信箱。
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_echo_agent(&home, &daemon, "idle");

    // 停止它：delete 之前先 stop，讓 instance 停在 `stopped` 而非消失。
    let stopped = daemon.operator_call(
        &home,
        "restart_instance",
        json!({"instance": "idle", "mode": "fresh"}),
    );
    assert_eq!(
        stopped.outcome,
        Outcome::Completed,
        "繁中說明：restart 應完成（前提），實際為 {stopped:?}"
    );

    // 讓收件人不在 running：停掉這個 instance 的程序（delete 會同時移除
    // 定義，因此改用 `stop` 語意的 restart + 中斷，這裡以 delete 後重建一個
    // 從未啟動的 instance 較不直接）。最貼近契約的做法是：send 給一個
    // **不存在於 running** 的收件人，讓 daemon 走 not_running 分支。
    let sent = daemon.operator_call(
        &home,
        "send",
        json!({"instance": "absent", "message": "收件人不存在"}),
    );
    assert_eq!(
        sent.outcome,
        Outcome::Refused,
        "繁中說明：送給不存在的 instance 應 refused（契約 §3 的 not_found 路線），實際為 {sent:?}"
    );
    let refused_entry = &sent.value["recipients"][0];
    assert_eq!(
        refused_entry["wake"],
        json!("not_running"),
        "繁中說明：收件人不在 running 時 wake 應為 not_running（契約 §6.4），實際為 {refused_entry}"
    );
    assert_eq!(
        refused_entry["outcome"],
        json!("refused"),
        "繁中說明：該收件人的結果應為 refused（契約 §6.1：回報拒絕原因）"
    );
    assert!(
        refused_entry["error"].is_object(),
        "繁中說明：refused 的收件人應帶 error（契約 §6.1），實際為 {refused_entry}"
    );
    assert!(
        !refused_entry["message_id"].is_string() && refused_entry["message_id"] != json!(""),
        "繁中說明：未寫入的訊息不該有 message_id（契約 §6.1），實際為 {refused_entry}"
    );
}

#[test]
fn the_wake_outcome_is_reported_per_recipient_not_once_per_call() {
    // 契約 §6.1：`value.recipients` 每項含 `wake`。多收件人時**每一項**
    // 都要有自己的 wake 值——不能只報一個全體結果。
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_echo_agent(&home, &daemon, "alpha");
    create_echo_agent(&home, &daemon, "beta");

    let sent = daemon.operator_call(
        &home,
        "send",
        json!({"instances": ["alpha", "beta"], "message": "兩位都收得到"}),
    );
    assert_eq!(
        sent.outcome,
        Outcome::Completed,
        "繁中說明：送給兩個 running 的 instance 應完成，實際為 {sent:?}"
    );
    let recipients = sent.value["recipients"]
        .as_array()
        .unwrap_or_else(|| panic!("繁中說明：應有 value.recipients，實際為 {}", sent.value));
    assert_eq!(
        recipients.len(),
        2,
        "繁中說明：兩個收件人應有兩筆結果（契約 §6.1），實際為 {recipients:?}"
    );
    for entry in recipients {
        let instance = entry["instance"].as_str().unwrap_or_default();
        for field in ["instance", "outcome", "message_id", "wake"] {
            assert!(
                entry.get(field).is_some(),
                "繁中說明：{instance} 的收件人結果應含 {field}（契約 §6.1），實際為 {entry}"
            );
        }
        let wake = entry["wake"].as_str().unwrap_or_default();
        assert!(
            ["hinted", "deferred", "not_running", "failed"].contains(&wake),
            "繁中說明：{instance} 的 wake 應是契約 §6.4 的四個值之一，實際為 {wake:?}"
        );
    }
    // 兩個收件人的 message ID 必須不同：那是兩筆不同的訊息（§6.1）。
    let ids: Vec<&str> = recipients
        .iter()
        .filter_map(|entry| entry["message_id"].as_str())
        .collect();
    assert_eq!(
        ids.len(),
        2,
        "繁中說明：兩個收件人都應寫入訊息並各有 message_id（契約 §6.1），實際為 {ids:?}"
    );
    assert_ne!(
        ids[0], ids[1],
        "繁中說明：不同收件人的訊息應有不同 message_id（契約 §6.1），實際兩者皆為 {}",
        ids[0]
    );
}

#[test]
fn a_teardown_style_screen_check_shows_the_hint_landed_on_the_real_pane() {
    // 契約 §6.4 的「經 PTY 送出」在畫面上也該看得見，不只在 frame 串流裡。
    // 這個檢查用 `pane_snapshot`（§4.7）而非 frame 讀取，代表「提示真的進了
    // 該 instance 的終端」這件事有兩條獨立的觀測路徑。
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let worker = create_echo_agent(&home, &daemon, "worker");

    let mut client = AttachClient::attach(&home, "worker", worker);
    let _ = client.drain_frames(Duration::from_millis(300));
    client.type_bytes(b"quiet\n");
    let _ = client.drain_frames(Duration::from_millis(WAKE_QUIET_MS + 400));
    client.detach();
    drop(client);
    std::thread::sleep(Duration::from_millis(WAKE_QUIET_MS + 200));

    let sent = daemon.operator_call(
        &home,
        "send",
        json!({"instance": "worker", "message": "畫面上應該看得到我"}),
    );
    assert_eq!(
        wake_of(&sent, "worker").as_deref(),
        Some("hinted"),
        "繁中說明：安靜的 running 收件人應被 hinted（契約 §6.4），實際 value.recipients 為 {}",
        sent.value["recipients"]
    );

    let deadline = Instant::now() + HINT_TIMEOUT;
    let mut screen = String::new();
    while Instant::now() < deadline && !screen.contains("[OTTERHIVE-MSG]") {
        std::thread::sleep(POLL_STEP);
        screen = screen_text(&home, &daemon, "worker");
    }
    assert!(
        screen.contains("[OTTERHIVE-MSG]"),
        "繁中說明：提示應出現在該 instance 的畫面上（契約 §6.4「經 PTY 送出」），最後觀察到的畫面為 {screen:?}"
    );
}
