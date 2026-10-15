//! T5 Lane A RED：訊息的投遞與 ACK 契約（§6.1–§6.3）。
//!
//! 依 `docs/plans/p1a-implementation.md` T5。驗收重點：
//! - `send` 的參數形狀（三擇一收件人、`message` 64 KiB、識別字串 256 字元）與
//!   「欄位超限時沒有寫入任何訊息」。
//! - 部分失敗回 `partial` 且 `value.recipients` 逐項列出結果。
//! - `inbox` 的 drain／ack 往返、至少一次投遞、lease 逾時重送、48 KiB 批次預算。
//! - §6.2 收件權限：`team_acl_denied` 與 `accept_from`。
//! - §4.4：`fresh` restart 把 `delivering` 退回 `unread` 並使舊 token 失效；
//!   `resume` 保留 lease。
//!
//! 全檔只在 macOS 編譯；所有等待有上限並回報最後觀測狀態；使用暫存的
//! `HOME`／`OTTERHIVE_HOME`／cwd。backend 一律用 `fixture()` 的絕對路徑。
//!
//! lease 長度用 `OTTERHIVE_TEST_LEASE_MS` 縮短（§6.3 的 600 秒）；daemon 只在
//! 啟動時讀一次，所以 lease 相關的測試都以 `Daemon::start_with_env` 帶入。
//!
//! RED 說明：`send`／`inbox` 的 daemon 端行為由 T5 GREEN 建立。在此之前
//! `perform` 對兩者回 `unsupported_in_phase`，因此本檔是「編譯通過、執行
//! 失敗」的行為 RED。測試只透過 socket 打真 daemon，不直接引用 `src/teams.rs`
//! 或 `src/messaging.rs` 的函式簽章（那些由 coordinator 在 GREEN 接線）。

#![cfg(target_os = "macos")]

use serde_json::{json, Value};

mod support {
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use otterhive::home::Home;
    use otterhive::protocol::{Auth, OperationResult, Request};
    use rusqlite::params;
    use serde_json::{json, Value};
    use tempfile::TempDir;

    pub const POLL_STEP: Duration = Duration::from_millis(20);
    pub const LONG_TIMEOUT: Duration = Duration::from_secs(20);
    pub const RECV_TIMEOUT: Duration = Duration::from_secs(10);

    pub fn daemon_bin() -> &'static str {
        match option_env!("CARGO_BIN_EXE_otterhive") {
            Some(path) => path,
            None => panic!(
                "繁中說明：otterhive binary 尚未建立（T4 GREEN 才加入 [[bin]]；本測試為 RED）"
            ),
        }
    }

    pub fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/backends")
            .join(name)
    }

    /// §6.3：drain 一次最多取 48 KiB 的 body。
    pub const DRAIN_BUDGET_BYTES: usize = 48 * 1024;

    /// 隔離的暫存 home。
    pub struct TestHome {
        pub home: Home,
        /// 與測試同壽命；Drop 時刪除隔離目錄。
        _root: TempDir,
    }

    impl TestHome {
        pub fn new() -> Self {
            let root = TempDir::new().expect("繁中說明：測試需建立隔離暫存目錄");
            let home =
                Home::initialize(&root.path().join("h")).expect("繁中說明：暫存 home 應可初始化");
            Self { home, _root: root }
        }

        pub fn root(&self) -> &Path {
            self.home.root()
        }

        pub fn socket_path(&self) -> PathBuf {
            self.home.run_dir().join("daemon.sock")
        }

        pub fn operator_token(&self) -> String {
            fs::read_to_string(self.home.operator_token_path())
                .expect("繁中說明：應可讀取 operator 憑證")
                .trim()
                .to_owned()
        }

        pub fn workspace(&self, name: &str) -> PathBuf {
            self.home.root().join("workspace").join(name)
        }

        pub fn operator(&self) -> Auth {
            Auth::Operator(self.operator_token())
        }

        /// 從 instance 的 MCP 設定讀出該 generation 的 token（§5.1）。
        pub fn instance_auth(&self, name: &str) -> Auth {
            let path = self.workspace(name).join(".claude/otterhive-mcp.json");
            let raw = fs::read_to_string(&path)
                .unwrap_or_else(|_| panic!("繁中說明：應可讀取 {} 的 MCP 設定", path.display()));
            let parsed: Value = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("繁中說明：MCP 設定應是合法 JSON：{e}；實際 {raw}"));
            let token = parsed["mcpServers"]["otterhive"]["env"]["OTTERHIVE_INSTANCE_TOKEN"]
                .as_str()
                .unwrap_or_else(|| panic!("繁中說明：MCP 設定應含 instance token；實際 {raw}"));
            Auth::Instance(token.to_owned())
        }

        /// 把 instance 名稱列進設定檔的 `managers`（§2 的權限來源之一）。
        ///
        /// §1：`managers` 的變更要重啟 daemon 才生效，所以必須在
        /// `Daemon::start` **之前**呼叫。
        pub fn set_managers(&self, names: &[&str]) {
            let listed = names
                .iter()
                .map(|name| format!("  - {name}"))
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(self.home.config_path(), format!("managers:\n{listed}\n"))
                .expect("繁中說明：應可寫入測試用 otterhive.yaml");
        }
    }

    pub struct Daemon {
        child: Option<Child>,
    }

    impl Daemon {
        pub fn start(home: &TestHome) -> Self {
            Self::start_with_env(home, &[])
        }

        pub fn start_with_env(home: &TestHome, extra_env: &[(&str, &str)]) -> Self {
            let root = home.root().to_path_buf();
            let mut command = Command::new(daemon_bin());
            command
                .arg("daemon")
                .env("HOME", &root)
                .env("OTTERHIVE_HOME", &root)
                .env("OTTERHIVE_TEST_STARTUP_GRACE_MS", "600")
                .env("OTTERHIVE_TEST_STOP_TERM_MS", "1000")
                .env("OTTERHIVE_TEST_STOP_KILL_MS", "500")
                .current_dir(&root)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            for (key, value) in extra_env {
                command.env(key, value);
            }
            let child = command.spawn().expect("繁中說明：daemon 應可啟動");
            let daemon = Self { child: Some(child) };
            let socket = home.socket_path();
            let deadline = Instant::now() + LONG_TIMEOUT;
            loop {
                if socket.exists() && UnixStream::connect(&socket).is_ok() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "繁中說明：daemon 應在期限內可服務 {}（啟動自檢或收斂失敗？）",
                    socket.display()
                );
                std::thread::sleep(POLL_STEP);
            }
            daemon
        }

        /// daemon 的 pid。
        pub fn pid(&self) -> i32 {
            self.child
                .as_ref()
                .map(|child| child.id() as i32)
                .unwrap_or(-1)
        }

        /// 有界停止（先 SIGTERM、逾時才 KILL）並回傳被停止的 daemon pid。
        pub fn terminate(&mut self) -> i32 {
            let Some(mut child) = self.child.take() else {
                return -1;
            };
            let pid = child.id() as i32;
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            let start = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return pid,
                    Ok(None) => {}
                    Err(_) => break,
                }
                if start.elapsed() >= Duration::from_secs(8) {
                    break;
                }
                std::thread::sleep(POLL_STEP);
            }
            let _ = child.kill();
            let _ = child.wait();
            pid
        }

        pub fn call(&self, home: &TestHome, auth: &Auth, op: &str, args: Value) -> OperationResult {
            let request = Request {
                v: 1,
                auth: auth.clone(),
                op: op.to_owned(),
                args,
                operation_id: None,
            };
            let socket = home.socket_path();
            let mut stream = UnixStream::connect(&socket)
                .unwrap_or_else(|e| panic!("繁中說明：應能連到 {}：{e}", socket.display()));
            let _ = stream.set_read_timeout(Some(LONG_TIMEOUT));
            let _ = stream.set_write_timeout(Some(RECV_TIMEOUT));
            let mut line =
                serde_json::to_string(&request).expect("繁中說明：請求應可序列化為 JSON");
            line.push('\n');
            stream
                .write_all(line.as_bytes())
                .expect("繁中說明：應能送出請求");
            let mut reader = BufReader::new(stream);
            let mut response = String::new();
            reader
                .read_line(&mut response)
                .unwrap_or_else(|e| panic!("繁中說明：應可讀回一行回應：{e}"));
            assert!(
                !response.trim().is_empty(),
                "繁中說明：daemon 應回一行 OperationResult"
            );
            serde_json::from_str(response.trim()).unwrap_or_else(|e| {
                panic!("繁中說明：回應應是合法 OperationResult JSON：{e}；實際 {response}")
            })
        }

        pub fn operator_call(&self, home: &TestHome, op: &str, args: Value) -> OperationResult {
            let auth = home.operator();
            self.call(home, &auth, op, args)
        }

        pub fn instance_call(
            &self,
            home: &TestHome,
            instance: &str,
            op: &str,
            args: Value,
        ) -> OperationResult {
            let auth = home.instance_auth(instance);
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

    /// 在 `cond` 為真前輪詢到 `timeout`；逾時回最後一次結果（不 panic）。
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

    /// 逾時 panic 並附最後狀態（供「前提必須成立」用）。
    pub fn require_until(timeout: Duration, what: &str, mut cond: impl FnMut() -> String) {
        let mut last = String::from("（尚未取得狀態）");
        let start = Instant::now();
        loop {
            let current = cond();
            if current.is_empty() {
                return;
            }
            let _ = std::mem::replace(&mut last, current);
            if start.elapsed() >= timeout {
                panic!("繁中說明：{what} 應在期限內成立，最後狀態：{last}");
            }
            std::thread::sleep(POLL_STEP);
        }
    }

    /// 以 `echo_agent.sh`（絕對路徑）建立並啟動一個 instance。
    pub fn create_instance(daemon: &Daemon, home: &TestHome, name: &str) {
        let created = daemon.operator_call(
            home,
            "create_instance",
            json!({"name": name, "command": fixture("echo_agent.sh").to_string_lossy()}),
        );
        assert_eq!(
            created.outcome,
            otterhive::protocol::Outcome::Completed,
            "繁中說明：create_instance {name} 應完成（測試前提），實際為 {created:?}"
        );
    }

    /// 建立 team（測試前提）。
    pub fn create_team(daemon: &Daemon, home: &TestHome, name: &str, args: Value) {
        let mut payload = args;
        payload["action"] = json!("create");
        payload["name"] = json!(name);
        let created = daemon.operator_call(home, "team", payload);
        assert_eq!(
            created.outcome,
            otterhive::protocol::Outcome::Completed,
            "繁中說明：team create {name} 應完成（測試前提），實際為 {created:?}"
        );
    }

    /// operator 送一則訊息給單一 instance。
    pub fn operator_send(
        daemon: &Daemon,
        home: &TestHome,
        recipient: &str,
        message: &str,
    ) -> OperationResult {
        daemon.operator_call(
            home,
            "send",
            json!({"instance": recipient, "message": message, "request_kind": "query"}),
        )
    }

    /// `inbox` drain 的結果：`(OperationResult, delivery_token, messages)`。
    pub fn drain(
        daemon: &Daemon,
        home: &TestHome,
        instance: &str,
    ) -> (OperationResult, String, Vec<Value>) {
        let result = daemon.instance_call(home, instance, "inbox", json!({}));
        assert_eq!(
            result.outcome,
            otterhive::protocol::Outcome::Completed,
            "繁中說明：drain 應完成（測試前提），實際為 {result:?}"
        );
        let token = result.value["delivery_token"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let messages = result.value["messages"]
            .as_array()
            .unwrap_or_else(|| panic!("繁中說明：drain 應回 messages 陣列，實際 {}", result.value))
            .clone();
        (result, token, messages)
    }

    /// 以指定 token ack 一則訊息。
    pub fn ack(
        daemon: &Daemon,
        home: &TestHome,
        instance: &str,
        message_id: &str,
        token: &str,
    ) -> OperationResult {
        daemon.instance_call(
            home,
            instance,
            "inbox",
            json!({"action": "ack", "message_id": message_id, "delivery_token": token}),
        )
    }

    /// 錯誤碼斷言。
    pub fn assert_code(result: &OperationResult, expected: otterhive::protocol::ErrorCode) {
        assert_eq!(
            result.outcome,
            otterhive::protocol::Outcome::Refused,
            "繁中說明：應為 refused，實際為 {result:?}"
        );
        assert_eq!(
            result.error.as_ref().map(|error| error.code),
            Some(expected),
            "繁中說明：錯誤碼應為 {expected:?}，實際為 {result:?}"
        );
    }

    /// `messages` 表中某收件 instance 的一列（`message_rows` 的元素）。
    pub type MessageRow = (String, String, String, i64, Option<String>, Option<i64>);

    /// `messages` 表中某收件 instance 的列摘要：`id`、`body`、`state`、
    /// `delivery_count`、`delivery_token`、`lease_expires_ms`，依建立順序。
    pub fn message_rows(home: &TestHome, recipient: &str) -> Vec<MessageRow> {
        let path = home.home.root().join("state.sqlite3");
        let connection = rusqlite::Connection::open(&path)
            .unwrap_or_else(|e| panic!("繁中說明：應可開啟 {}：{e}", path.display()));
        let recipient_id: String = connection
            .query_row(
                "SELECT id FROM instances WHERE name = ?1 AND deleted_ms IS NULL",
                params![recipient],
                |row| row.get(0),
            )
            .unwrap_or_else(|e| panic!("繁中說明：instance {recipient} 應存在（測試前提）：{e}"));
        let mut statement = connection
            .prepare(
                "SELECT id, body, state, delivery_count, delivery_token, lease_expires_ms \
                 FROM messages WHERE recipient_id = ?1 ORDER BY seq",
            )
            .unwrap_or_else(|e| panic!("繁中說明：應可準備 messages 查詢：{e}"));
        statement
            .query_map(params![recipient_id], |row| {
                let delivery_token: Option<String> = row.get(4)?;
                let lease_expires_ms: Option<i64> = row.get(5)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    delivery_token,
                    lease_expires_ms,
                ))
            })
            .unwrap_or_else(|e| panic!("繁中說明：應可查詢 messages：{e}"))
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|e| panic!("繁中說明：應可收集 messages 列：{e}"))
    }

    /// §5.4 的 resume 證據：在 `$HOME/.claude/projects/<編碼>/` 放一個含
    /// `"type":"user"` 的 `.jsonl`。
    ///
    /// 編碼取自 **canonical** working directory（§5.4）；`TempDir` 在 macOS
    /// 給的是 `/var/folders/...`，而 `/var` 是 symlink，所以這裡要正規化。
    pub fn write_resume_evidence(home: &TestHome, workdir: &Path) {
        let canonical = fs::canonicalize(workdir).unwrap_or_else(|_| workdir.to_path_buf());
        let encoded: String = canonical
            .to_string_lossy()
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' {
                    ch
                } else {
                    '-'
                }
            })
            .collect();
        let project_dir = home.root().join(".claude/projects").join(encoded);
        fs::create_dir_all(&project_dir).expect("繁中說明：應可建立 projects 目錄");
        fs::write(
            project_dir.join("session.jsonl"),
            "{\"type\":\"user\",\"message\":{\"role\":\"user\"}}\n",
        )
        .expect("繁中說明：應可寫入 resume 證據");
    }

    /// pid 是否仍在運行（zombie 不算）。
    pub fn pid_running(pid: i32) -> bool {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if ret != 0 {
            return true;
        }
        info.si_pid == 0
    }
}

use support::*;

/// §6.1／§6.3：`send` → drain → ack 的完整往返，欄位齊全；重複 ack 同一組回
/// `completed` 且 `value.already_acked: true`。
#[test]
fn send_drain_ack_round_trip() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");

    let sent = operator_send(&daemon, &home, "alpha", "請回報進度");
    assert_eq!(
        sent.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：send 應完成，實際為 {sent:?}"
    );
    let recipients = sent.value["recipients"]
        .as_array()
        .unwrap_or_else(|| panic!("繁中說明：send 應回 recipients 陣列，實際 {}", sent.value))
        .clone();
    assert_eq!(
        recipients.len(),
        1,
        "繁中說明：應只有一個收件人，實際 {recipients:?}"
    );
    assert_eq!(recipients[0]["instance"], json!("alpha"));
    assert_eq!(
        recipients[0]["outcome"],
        json!("queued"),
        "繁中說明：收件人應為 queued，實際 {recipients:?}"
    );
    let message_id = recipients[0]["message_id"]
        .as_str()
        .unwrap_or_else(|| panic!("繁中說明：queued 應帶 message_id，實際 {recipients:?}"))
        .to_owned();

    let (drained, token, messages) = drain(&daemon, &home, "alpha");
    assert_eq!(
        drained.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：drain 應完成，實際為 {drained:?}"
    );
    assert!(
        !token.is_empty(),
        "繁中說明：drain 有訊息時應發 delivery_token"
    );
    assert!(
        drained.value["lease_expires_ms"]
            .as_i64()
            .is_some_and(|expires| expires > now_ms()),
        "繁中說明：lease_expires_ms 應是未來的 epoch 毫秒，實際 {}",
        drained.value
    );
    assert_eq!(
        drained.value["remaining"],
        json!(0),
        "繁中說明：只有一則時 remaining 應為 0，實際 {}",
        drained.value
    );
    assert_eq!(messages.len(), 1, "繁中說明：應取到一則，實際 {messages:?}");

    let message = &messages[0];
    assert_eq!(message["id"], json!(message_id));
    assert_eq!(
        message["from"],
        json!({"kind": "operator", "name": Value::Null})
    );
    assert_eq!(
        message["kind"],
        json!("query"),
        "繁中說明：kind 應是送出的 request_kind，實際 {message}"
    );
    assert_eq!(message["body"], json!("請回報進度"));
    assert_eq!(
        message["correlation_id"],
        Value::Null,
        "繁中說明：未指定時 correlation_id 應為 null，實際 {message}"
    );
    assert_eq!(message["parent_id"], Value::Null);
    assert_eq!(message["thread_id"], Value::Null);
    assert!(
        message["created_ms"].as_i64().is_some_and(|ms| ms > 0),
        "繁中說明：created_ms 應是 epoch 毫秒，實際 {message}"
    );
    assert_eq!(
        message["delivery_count"],
        json!(1),
        "繁中說明：首次 drain 後 delivery_count 應為 1，實際 {message}"
    );

    let acked = ack(&daemon, &home, "alpha", &message_id, &token);
    assert_eq!(
        acked.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：ack 應完成，實際為 {acked:?}"
    );

    // 重複 ack 同一組：completed 且 already_acked。
    let again = ack(&daemon, &home, "alpha", &message_id, &token);
    assert_eq!(
        again.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：重複 ack 應為 completed，實際為 {again:?}"
    );
    assert_eq!(
        again.value["already_acked"],
        json!(true),
        "繁中說明：重複 ack 應回 already_acked: true，實際 {}",
        again.value
    );

    // 訊息不再可 drain。
    let (after, _, remaining) = drain(&daemon, &home, "alpha");
    assert_eq!(
        after.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：ack 後再 drain 應完成，實際為 {after:?}"
    );
    assert!(
        remaining.is_empty(),
        "繁中說明：ack 後不應再取到訊息，實際 {remaining:?}"
    );
    assert!(
        after.value["delivery_token"].is_null(),
        "繁中說明：沒有訊息時不發 token，實際 {}",
        after.value
    );
}

/// §6.1：`request_kind` 省略時記為 `message`。
#[test]
fn an_omitted_request_kind_is_recorded_as_message() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");

    let sent = daemon.operator_call(
        &home,
        "send",
        json!({"instance": "alpha", "message": "只是通知"}),
    );
    assert_eq!(
        sent.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：send 應完成，實際為 {sent:?}"
    );

    let (_, _, messages) = drain(&daemon, &home, "alpha");
    assert_eq!(messages.len(), 1, "繁中說明：應取到一則，實際 {messages:?}");
    assert_eq!(
        messages[0]["kind"],
        json!("message"),
        "繁中說明：省略 request_kind 應記為 message，實際 {}",
        messages[0]
    );
}

/// §6.1：`correlation_id`／`parent_id`／`thread_id` 逐字保留。
#[test]
fn opaque_identifiers_are_carried_through_verbatim() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");

    let sent = daemon.operator_call(
        &home,
        "send",
        json!({"instance": "alpha", "message": "帶識別字串",
               "request_kind": "update",
               "correlation_id": "corr-1", "parent_id": "parent-1", "thread_id": "thread-1"}),
    );
    assert_eq!(
        sent.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：send 應完成，實際為 {sent:?}"
    );

    let (_, _, messages) = drain(&daemon, &home, "alpha");
    assert_eq!(messages.len(), 1, "繁中說明：應取到一則，實際 {messages:?}");
    assert_eq!(messages[0]["correlation_id"], json!("corr-1"));
    assert_eq!(messages[0]["parent_id"], json!("parent-1"));
    assert_eq!(messages[0]["thread_id"], json!("thread-1"));
    assert_eq!(messages[0]["kind"], json!("update"));
}

/// §6.1：超過 64 KiB 的 `message`、超過 256 字元的識別字串 → `invalid_argument`，
/// 而且**沒有寫入任何訊息**。
#[test]
fn oversize_message_and_identifiers_are_invalid() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");

    // 多位元組字元：64 KiB 的上限是 **bytes**，不是字元數。
    let just_under = "é".repeat((64 * 1024) / 2);
    let just_over = "é".repeat((64 * 1024) / 2 + 1);

    let too_big = daemon.operator_call(
        &home,
        "send",
        json!({"instance": "alpha", "message": just_over}),
    );
    assert_code(&too_big, otterhive::protocol::ErrorCode::InvalidArgument);
    assert!(
        message_rows(&home, "alpha").is_empty(),
        "繁中說明：被拒的 send 不得留下訊息，實際 {:?}",
        message_rows(&home, "alpha")
    );

    let at_limit = daemon.operator_call(
        &home,
        "send",
        json!({"instance": "alpha", "message": just_under}),
    );
    assert_eq!(
        at_limit.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：剛好 64 KiB（bytes）應被接受，實際為 {at_limit:?}"
    );

    for field in ["correlation_id", "parent_id", "thread_id"] {
        let rejected = daemon.operator_call(
            &home,
            "send",
            json!({"instance": "alpha", "message": "ok", field: "x".repeat(257)}),
        );
        assert_code(&rejected, otterhive::protocol::ErrorCode::InvalidArgument);

        let accepted = daemon.operator_call(
            &home,
            "send",
            json!({"instance": "alpha", "message": "ok", field: "x".repeat(256)}),
        );
        assert_eq!(
            accepted.outcome,
            otterhive::protocol::Outcome::Completed,
            "繁中說明：256 字元的 {field} 應被接受，實際為 {accepted:?}"
        );
    }
}

/// §6.1：`message` 必須非空。
#[test]
fn an_empty_message_is_invalid() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");

    for message in ["", " "] {
        let rejected = daemon.operator_call(
            &home,
            "send",
            json!({"instance": "alpha", "message": message}),
        );
        assert_code(&rejected, otterhive::protocol::ErrorCode::InvalidArgument);
    }
    assert!(
        message_rows(&home, "alpha").is_empty(),
        "繁中說明：被拒的 send 不得留下訊息"
    );
}

/// §6.1：`instance`／`instances`／`team` 三擇一。
#[test]
fn the_recipient_selector_is_exclusive() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");
    create_instance(&daemon, &home, "beta");
    create_team(
        &daemon,
        &home,
        "squad",
        json!({"members": ["alpha", "beta"], "orchestrator": "alpha"}),
    );

    let none = daemon.operator_call(&home, "send", json!({"message": "沒收件人"}));
    assert_code(&none, otterhive::protocol::ErrorCode::InvalidArgument);

    let both = daemon.operator_call(
        &home,
        "send",
        json!({"instance": "alpha", "team": "squad", "message": "兩個收件人"}),
    );
    assert_code(&both, otterhive::protocol::ErrorCode::InvalidArgument);

    assert!(
        message_rows(&home, "alpha").is_empty() && message_rows(&home, "beta").is_empty(),
        "繁中說明：形狀不符的 send 不得留下訊息"
    );
}

/// §6.1：部分收件人失敗回 `partial`，`value.recipients` 逐項列出結果。
#[test]
fn partial_broadcast_reports_each_recipient() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");

    let sent = daemon.operator_call(
        &home,
        "send",
        json!({"instances": ["alpha", "ghost"], "message": "部分投遞"}),
    );
    assert_eq!(
        sent.outcome,
        otterhive::protocol::Outcome::Partial,
        "繁中說明：一成一敗應為 partial，實際為 {sent:?}"
    );
    let recipients = sent.value["recipients"]
        .as_array()
        .unwrap_or_else(|| panic!("繁中說明：send 應回 recipients 陣列，實際 {}", sent.value))
        .clone();
    assert_eq!(
        recipients.len(),
        2,
        "繁中說明：兩個收件人都要回報，實際 {recipients:?}"
    );
    assert_eq!(recipients[0]["instance"], json!("alpha"));
    assert_eq!(recipients[0]["outcome"], json!("queued"));
    assert!(recipients[0]["message_id"].is_string());
    assert_eq!(
        recipients[1]["instance"],
        json!("ghost"),
        "繁中說明：順序應與請求一致，實際 {recipients:?}"
    );
    assert_eq!(
        recipients[1]["outcome"],
        json!("refused"),
        "繁中說明：不存在的收件人應為 refused，實際 {recipients:?}"
    );
    assert_eq!(
        recipients[1]["error"]["code"],
        json!("not_found"),
        "繁中說明：ghost 應為 not_found，實際 {recipients:?}"
    );
    assert!(
        recipients[1]["message_id"].is_null(),
        "繁中說明：refused 不應帶 message_id，實際 {recipients:?}"
    );

    // 成功的收件人確實收到訊息。
    assert_eq!(message_rows(&home, "alpha").len(), 1);
}

/// §6.1：全部收件人失敗回 `refused`（operation 層）。
#[test]
fn a_send_with_no_viable_recipient_is_refused() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let sent = daemon.operator_call(
        &home,
        "send",
        json!({"instances": ["ghost", "phantom"], "message": "沒有人"}),
    );
    assert_code(&sent, otterhive::protocol::ErrorCode::NotFound);
    let recipients = sent.value["recipients"]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "繁中說明：refused 也應逐項列出 recipients，實際 {}",
                sent.value
            )
        })
        .clone();
    assert_eq!(
        recipients.len(),
        2,
        "繁中說明：兩個收件人都要回報，實際 {recipients:?}"
    );
    for recipient in &recipients {
        assert_eq!(
            recipient["outcome"],
            json!("refused"),
            "實際 {recipients:?}"
        );
        assert_eq!(recipient["error"]["code"], json!("not_found"));
    }
}

/// §2：呼叫端不能自報 sender。`from`／`sender` 這類欄位在 admission 就被擋。
#[test]
fn a_cannot_self_report_sender_field_is_rejected() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");

    for field in ["from", "sender"] {
        let rejected = daemon.operator_call(
            &home,
            "send",
            json!({"instance": "alpha", "message": "假冒", field: "operator"}),
        );
        assert_code(&rejected, otterhive::protocol::ErrorCode::InvalidArgument);
    }
    assert!(
        message_rows(&home, "alpha").is_empty(),
        "繁中說明：帶自報 sender 的 send 不得留下訊息"
    );
}

/// §2／§6.3：`inbox` 只能讀自己的 inbox——operator 沒有收件匣，別的 instance
/// 也讀不到。
#[test]
fn inbox_is_only_readable_by_its_owner() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");
    create_instance(&daemon, &home, "beta");
    operator_send(&daemon, &home, "alpha", "給 alpha 的");

    let beta_drain = daemon.instance_call(&home, "beta", "inbox", json!({}));
    assert_eq!(
        beta_drain.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：beta drain 應完成（只是空的），實際為 {beta_drain:?}"
    );
    assert!(
        beta_drain.value["messages"]
            .as_array()
            .is_some_and(|messages| messages.is_empty()),
        "繁中說明：beta 不應看到 alpha 的訊息，實際 {}",
        beta_drain.value
    );

    let operator_drain = daemon.operator_call(&home, "inbox", json!({}));
    assert_code(
        &operator_drain,
        otterhive::protocol::ErrorCode::Unauthorized,
    );
}

/// §6.3：沒有訊息時 `messages` 為空陣列且不發 token。
#[test]
fn draining_an_empty_inbox_returns_no_token() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");

    let (drained, token, messages) = drain(&daemon, &home, "alpha");
    assert_eq!(
        drained.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：drain 應完成，實際為 {drained:?}"
    );
    assert!(
        messages.is_empty(),
        "繁中說明：空 inbox 的 messages 應是空陣列，實際 {messages:?}"
    );
    assert!(
        token.is_empty() && drained.value["delivery_token"].is_null(),
        "繁中說明：沒有訊息時不發 token，實際 {}",
        drained.value
    );
    assert_eq!(
        drained.value["remaining"],
        json!(0),
        "繁中說明：remaining 應為 0，實際 {}",
        drained.value
    );
}

/// §6.3：前一批未 ack 的訊息**不會**因再次 drain 而被結算。
#[test]
fn second_drain_does_not_settle_the_first_batch() {
    let home = TestHome::new();
    // lease 拉長，讓「lease 過期」不可能成為重新投遞的原因。
    let daemon = Daemon::start_with_env(&home, &[("OTTERHIVE_TEST_LEASE_MS", "600000")]);
    create_instance(&daemon, &home, "alpha");
    operator_send(&daemon, &home, "alpha", "第一則");

    let (_, token, messages) = drain(&daemon, &home, "alpha");
    assert_eq!(
        messages.len(),
        1,
        "繁中說明：首次應取到一則，實際 {messages:?}"
    );
    let first_id = messages[0]["id"].as_str().unwrap_or_default().to_owned();

    // 第二次 drain：在 lease 未過期前只會拿到空的批次。
    let (_, second_token, second_messages) = drain(&daemon, &home, "alpha");
    assert!(
        second_messages.is_empty(),
        "繁中說明：lease 未過期時不應重送，實際 {second_messages:?}"
    );
    assert!(
        second_token.is_empty(),
        "繁中說明：空的第二次 drain 不應發 token"
    );

    // 第一批仍是 delivering、delivery_count 仍為 1。
    let rows = message_rows(&home, "alpha");
    assert_eq!(rows.len(), 1, "繁中說明：應只有一則，實際 {rows:?}");
    assert_eq!(rows[0].0, first_id);
    assert_eq!(
        rows[0].2, "delivering",
        "繁中說明：未 ack 的訊息應仍是 delivering，實際 {rows:?}"
    );
    assert_eq!(
        rows[0].3, 1,
        "繁中說明：delivery_count 應仍是 1，實際 {rows:?}"
    );

    // 原 token 仍然可用（lease 還沒到）。
    let acked = ack(&daemon, &home, "alpha", &first_id, &token);
    assert_eq!(
        acked.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：第一批的 token 應仍可 ack，實際為 {acked:?}"
    );
}

/// §6.3：lease 過期後同一則訊息會以新 token 重送，`delivery_count` 為 2，
/// 舊 token ack → `delivery_token_stale`。
#[test]
fn expired_lease_redelivers_with_a_new_token() {
    let home = TestHome::new();
    let daemon = Daemon::start_with_env(&home, &[("OTTERHIVE_TEST_LEASE_MS", "600")]);
    create_instance(&daemon, &home, "alpha");
    operator_send(&daemon, &home, "alpha", "會過期的訊息");

    let (_, first_token, messages) = drain(&daemon, &home, "alpha");
    assert_eq!(
        messages.len(),
        1,
        "繁中說明：首次應取到一則，實際 {messages:?}"
    );
    let message_id = messages[0]["id"].as_str().unwrap_or_default().to_owned();
    assert_eq!(messages[0]["delivery_count"], json!(1));

    // 有界輪詢：等 lease 過期（以 daemon 的時間為準，§6.3 的至少一次投遞）。
    let mut last_observation = String::from("（尚未觀察到任何 drain 結果）");
    let redelivered = poll_until(LONG_TIMEOUT, || {
        let (_, token, batch) = drain(&daemon, &home, "alpha");
        last_observation = format!("token={token:?} batch={batch:?}");
        !token.is_empty() && batch.len() == 1 && batch[0]["id"] == json!(message_id)
    });
    assert!(
        redelivered,
        "繁中說明：lease 過期後應以新 token 重送同一則訊息，最後觀測：{last_observation}"
    );

    // 舊 token 已失效。
    let stale = ack(&daemon, &home, "alpha", &message_id, &first_token);
    assert_code(&stale, otterhive::protocol::ErrorCode::DeliveryTokenStale);

    // 新 token 與 delivery_count。
    let rows = message_rows(&home, "alpha");
    assert_eq!(rows.len(), 1, "繁中說明：應仍只有一則，實際 {rows:?}");
    assert_eq!(
        rows[0].3, 2,
        "繁中說明：重送後 delivery_count 應為 2，實際 {rows:?}"
    );
    let new_token = rows[0]
        .4
        .clone()
        .unwrap_or_else(|| panic!("繁中說明：重送後應有新的 delivery_token，實際 {rows:?}"));
    assert_ne!(
        new_token, first_token,
        "繁中說明：重送應發新的 delivery_token"
    );
    let acked = ack(&daemon, &home, "alpha", &message_id, &new_token);
    assert_eq!(
        acked.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：新 token 應可 ack，實際為 {acked:?}"
    );
}

/// §6.3：`ack` 的四種失敗：`delivery_token_stale`、`never_delivered`、
/// `not_found`。
#[test]
fn ack_without_delivery_and_ack_of_foreign_message_are_refused() {
    let home = TestHome::new();
    let daemon = Daemon::start_with_env(&home, &[("OTTERHIVE_TEST_LEASE_MS", "600000")]);
    create_instance(&daemon, &home, "alpha");
    create_instance(&daemon, &home, "beta");

    // 送一則給 beta（alpha 拿不到它的 id 以外的資訊，但 id 可以從 send 回來）。
    let sent = operator_send(&daemon, &home, "beta", "給 beta 的");
    let recipients = sent.value["recipients"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let foreign_id = recipients
        .first()
        .and_then(|item| item["message_id"].as_str())
        .unwrap_or_else(|| panic!("繁中說明：send 應回 message_id，實際 {}", sent.value))
        .to_owned();

    // alpha 從未 drain 過 beta 的訊息：ack 一則不屬於自己的訊息 → not_found。
    let not_mine = ack(&daemon, &home, "alpha", &foreign_id, "任何 token");
    assert_code(&not_mine, otterhive::protocol::ErrorCode::NotFound);

    // beta 送一則給自己，取得真 token。
    let self_sent = daemon.instance_call(
        &home,
        "beta",
        "send",
        json!({"instance": "beta", "message": "給自己"}),
    );
    assert_eq!(
        self_sent.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：instance 應可送給自己，實際為 {self_sent:?}"
    );
    let self_id = self_sent.value["recipients"][0]["message_id"]
        .as_str()
        .unwrap_or_else(|| panic!("繁中說明：send 應回 message_id，實際 {}", self_sent.value))
        .to_owned();

    // 從未投遞（還沒 drain 過）→ never_delivered。
    let never = ack(&daemon, &home, "beta", &self_id, "尚未取得過的 token");
    assert_code(&never, otterhive::protocol::ErrorCode::NeverDelivered);
}

/// §6.3：token 不符（用同一則訊息，但給錯 token）→ `delivery_token_stale`。
#[test]
fn acking_with_the_wrong_token_is_stale() {
    let home = TestHome::new();
    let daemon = Daemon::start_with_env(&home, &[("OTTERHIVE_TEST_LEASE_MS", "600000")]);
    create_instance(&daemon, &home, "alpha");
    operator_send(&daemon, &home, "alpha", "一則");

    let (_, token, messages) = drain(&daemon, &home, "alpha");
    assert_eq!(messages.len(), 1, "繁中說明：應取到一則，實際 {messages:?}");
    let message_id = messages[0]["id"].as_str().unwrap_or_default().to_owned();

    let wrong = ack(&daemon, &home, "alpha", &message_id, "wrong-token");
    assert_code(&wrong, otterhive::protocol::ErrorCode::DeliveryTokenStale);

    // 正確的 token 仍然有效。
    let right = ack(&daemon, &home, "alpha", &message_id, &token);
    assert_eq!(
        right.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：錯的 token 不應使正確 token 失效，實際為 {right:?}"
    );
}

/// §6.3：drain 一次最多取 48 KiB（至少一則）。三則 30 KiB 的訊息，第一次
/// drain 只取到上限內的筆數，`remaining` 正確。
#[test]
fn drain_batch_respects_the_byte_budget() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");

    for index in 0..3 {
        let sent = daemon.operator_call(
            &home,
            "send",
            json!({"instance": "alpha", "message": format!("{index}{}", "x".repeat(30 * 1024))}),
        );
        assert_eq!(
            sent.outcome,
            otterhive::protocol::Outcome::Completed,
            "繁中說明：send 應完成，實際為 {sent:?}"
        );
    }

    let (drained, _, messages) = drain(&daemon, &home, "alpha");
    assert_eq!(
        drained.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：drain 應完成，實際為 {drained:?}"
    );
    let total: usize = messages
        .iter()
        .map(|message| message["body"].as_str().unwrap_or_default().len())
        .sum();
    assert!(
        total <= DRAIN_BUDGET_BYTES,
        "繁中說明：一次 drain 的 body 總和應不超過 48 KiB，實際 {total} bytes（{} 則）",
        messages.len()
    );
    assert!(
        !messages.is_empty(),
        "繁中說明：至少要取到一則（48 KiB 上限不套用於單則）"
    );
    assert!(
        messages.len() < 3,
        "繁中說明：30 KiB × 3 放不進 48 KiB，實際取到 {} 則",
        messages.len()
    );
    assert_eq!(
        drained.value["remaining"],
        json!(3 - messages.len() as u64),
        "繁中說明：remaining 應是尚未 drain 的筆數，實際 {}",
        drained.value
    );

    // 剩下兩則在第二次 drain 全部取回，且 bytes 總和不超過上限。
    let (second, _, rest) = drain(&daemon, &home, "alpha");
    assert_eq!(
        second.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：第二次 drain 應完成，實際為 {second:?}"
    );
    let rest_total: usize = rest
        .iter()
        .map(|message| message["body"].as_str().unwrap_or_default().len())
        .sum();
    assert!(
        rest_total <= DRAIN_BUDGET_BYTES,
        "繁中說明：第二次 drain 也不應超過 48 KiB，實際 {rest_total} bytes"
    );
    assert_eq!(
        second.value["remaining"],
        json!(0),
        "繁中說明：全部取完後 remaining 應為 0，實際 {}",
        second.value
    );
}

/// §4.4：`fresh` restart 把該 instance 所有的 `delivering` 退回 `unread`
/// （舊 token 失效）；`resume` 保留 lease。
#[test]
fn fresh_restart_requeues_and_resume_keeps_the_lease() {
    // fresh：舊 token 失效、delivery_count 不變（仍是 1）、狀態回到 unread。
    let fresh_home = TestHome::new();
    let daemon = Daemon::start_with_env(&fresh_home, &[("OTTERHIVE_TEST_LEASE_MS", "600000")]);
    create_instance(&daemon, &fresh_home, "alpha");
    operator_send(&daemon, &fresh_home, "alpha", "restart 前的訊息");
    let (_, token, messages) = drain(&daemon, &fresh_home, "alpha");
    assert_eq!(messages.len(), 1, "繁中說明：應取到一則，實際 {messages:?}");
    let message_id = messages[0]["id"].as_str().unwrap_or_default().to_owned();

    let restarted = daemon.operator_call(
        &fresh_home,
        "restart_instance",
        json!({"instance": "alpha", "mode": "fresh"}),
    );
    assert!(
        matches!(
            restarted.outcome,
            otterhive::protocol::Outcome::Completed | otterhive::protocol::Outcome::Queued
        ),
        "繁中說明：fresh restart 應成功，實際為 {restarted:?}"
    );

    let stale = ack(&daemon, &fresh_home, "alpha", &message_id, &token);
    assert_code(&stale, otterhive::protocol::ErrorCode::DeliveryTokenStale);

    let rows = message_rows(&fresh_home, "alpha");
    assert_eq!(rows.len(), 1, "繁中說明：訊息應仍在，實際 {rows:?}");
    assert_eq!(
        rows[0].2, "unread",
        "繁中說明：fresh restart 應把 delivering 退回 unread，實際 {rows:?}"
    );

    // 重送時 delivery_count 加一（至少一次投遞，§6.3）。
    let (_, new_token, redelivered) = drain(&daemon, &fresh_home, "alpha");
    assert_eq!(
        redelivered.len(),
        1,
        "繁中說明：fresh restart 後應可再 drain，實際 {redelivered:?}"
    );
    assert_eq!(redelivered[0]["delivery_count"], json!(2));
    assert!(!new_token.is_empty() && new_token != token);

    // ack 新 token 成功。
    let acked = ack(&daemon, &fresh_home, "alpha", &message_id, &new_token);
    assert_eq!(
        acked.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：新 token 應可 ack，實際為 {acked:?}"
    );
    drop(daemon);

    // resume：保留 lease，舊 token 仍然有效。
    let resume_home = TestHome::new();
    let daemon = Daemon::start_with_env(&resume_home, &[("OTTERHIVE_TEST_LEASE_MS", "600000")]);
    create_instance(&daemon, &resume_home, "beta");
    operator_send(&daemon, &resume_home, "beta", "resume 前的訊息");
    let (_, token, messages) = drain(&daemon, &resume_home, "beta");
    assert_eq!(messages.len(), 1, "繁中說明：應取到一則，實際 {messages:?}");
    let message_id = messages[0]["id"].as_str().unwrap_or_default().to_owned();

    // §5.4：resume 需要 session 證據。
    write_resume_evidence(&resume_home, &resume_home.workspace("beta"));
    let resumed = daemon.operator_call(
        &resume_home,
        "restart_instance",
        json!({"instance": "beta", "mode": "resume"}),
    );
    assert!(
        matches!(
            resumed.outcome,
            otterhive::protocol::Outcome::Completed | otterhive::protocol::Outcome::Queued
        ),
        "繁中說明：有證據時 resume 應成功，實際為 {resumed:?}"
    );

    let still_valid = ack(&daemon, &resume_home, "beta", &message_id, &token);
    assert_eq!(
        still_valid.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：resume 保留 lease，舊 token 應仍可 ack，實際為 {still_valid:?}"
    );

    let rows = message_rows(&resume_home, "beta");
    assert_eq!(
        rows[0].2, "acked",
        "繁中說明：ack 後應是 acked，實際 {rows:?}"
    );
}

/// §6.3：daemon 被殺掉之後，未投遞與未 ack 的訊息都還在。
#[test]
fn undelivered_and_unacked_messages_survive_daemon_kill() {
    let home = TestHome::new();
    let mut daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");
    operator_send(&daemon, &home, "alpha", "未投遞");
    operator_send(&daemon, &home, "alpha", "已投遞未 ack");
    let (_, _token, messages) = drain(&daemon, &home, "alpha");
    assert_eq!(messages.len(), 1, "繁中說明：應取到一則，實際 {messages:?}");
    let acked_id = messages[0]["id"].as_str().unwrap_or_default().to_owned();
    assert_eq!(
        messages[0]["body"],
        json!("已投遞未 ack"),
        "繁中說明：依建立順序先取到第二則，實際 {messages:?}"
    );

    daemon.terminate();
    assert!(
        !pid_running(daemon.pid()),
        "繁中說明：daemon 應已停止（pid {}）",
        daemon.pid()
    );

    // 重啟 daemon：兩個訊息都還在，lease 已過期的那則可重送。
    let daemon = Daemon::start_with_env(&home, &[("OTTERHIVE_TEST_LEASE_MS", "600")]);
    require_until(
        LONG_TIMEOUT,
        "daemon 重啟後應收斂殘留程序",
        || {
            let listed = daemon.operator_call(&home, "list_instances", json!({}));
            if listed.outcome != otterhive::protocol::Outcome::Completed {
                return format!("list_instances 失敗：{listed:?}");
            }
            let state = listed.value["instances"][0]["state"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if state == "recovery_required" {
                return String::from(
                    "instance 落在 recovery_required，daemon 重啟無法收斂（§4.3 的證據路徑）",
                );
            }
            if state == "stopped" {
                String::new()
            } else {
                format!("state 仍是 {state}")
            }
        },
    );

    // 未 ack 的那則（lease 已過期）可重送；未投遞的那則也在同一批之前。
    let (_, _, after) = drain(&daemon, &home, "alpha");
    let bodies: Vec<&str> = after
        .iter()
        .map(|message| message["body"].as_str().unwrap_or_default())
        .collect();
    assert!(
        bodies.contains(&"未投遞"),
        "繁中說明：daemon 被殺後未投遞的訊息應仍在，實際 {bodies:?}"
    );
    assert!(
        bodies.contains(&"已投遞未 ack"),
        "繁中說明：daemon 被殺後未 ack 的訊息應重送，實際 {bodies:?}"
    );

    let rows = message_rows(&home, "alpha");
    assert_eq!(rows.len(), 2, "繁中說明：應有兩則訊息，實際 {rows:?}");
    let still_delivering = rows
        .iter()
        .find(|row| row.0 == acked_id)
        .map(|row| row.2.clone())
        .unwrap_or_default();
    assert_eq!(
        still_delivering, "delivering",
        "繁中說明：daemon 重啟不應結算未 ack 的訊息（lease 尚在），實際 {rows:?}"
    );
}

/// §6.2：跨 team 拒絕回 `team_acl_denied`（不是 `unauthorized`）。
#[test]
fn cross_team_delivery_is_team_acl_denied() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");
    create_instance(&daemon, &home, "beta");
    create_instance(&daemon, &home, "outsider");
    create_team(
        &daemon,
        &home,
        "left",
        json!({"members": ["alpha"], "orchestrator": "alpha"}),
    );
    create_team(
        &daemon,
        &home,
        "right",
        json!({"members": ["beta"], "orchestrator": "beta"}),
    );

    let refused = daemon.instance_call(
        &home,
        "alpha",
        "send",
        json!({"instance": "beta", "message": "跨隊"}),
    );
    assert_code(&refused, otterhive::protocol::ErrorCode::TeamAclDenied);
    assert!(
        message_rows(&home, "beta").is_empty(),
        "繁中說明：被拒的 send 不得留下訊息，實際 {:?}",
        message_rows(&home, "beta")
    );

    // 同隊允許。
    create_instance(&daemon, &home, "gamma");
    let widened = daemon.operator_call(
        &home,
        "team",
        json!({"action": "update", "name": "left", "add": ["gamma"]}),
    );
    assert_eq!(
        widened.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：update 應完成，實際為 {widened:?}"
    );
    let allowed = daemon.instance_call(
        &home,
        "alpha",
        "send",
        json!({"instance": "gamma", "message": "同隊"}),
    );
    assert_eq!(
        allowed.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：同隊應允許，實際為 {allowed:?}"
    );
}

/// §6.2：recipient 不屬於任何 team → 允許。
#[test]
fn a_recipient_without_a_team_is_allowed() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");
    create_instance(&daemon, &home, "loner");
    create_team(
        &daemon,
        &home,
        "left",
        json!({"members": ["alpha"], "orchestrator": "alpha"}),
    );

    let sent = daemon.instance_call(
        &home,
        "alpha",
        "send",
        json!({"instance": "loner", "message": "無team的收件人"}),
    );
    assert_eq!(
        sent.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：收件人不屬任何 team 時應允許，實際為 {sent:?}"
    );
    assert_eq!(message_rows(&home, "loner").len(), 1);
}

/// §6.2：recipient 是某 team 的 orchestrator，且 **sender 的名稱**在該 team 的
/// `accept_from` → 允許；不在則拒絕。
#[test]
fn accept_from_gates_delivery_to_an_orchestrator() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "lead");
    create_instance(&daemon, &home, "peer");
    create_instance(&daemon, &home, "stranger");
    create_team(
        &daemon,
        &home,
        "squad",
        json!({"members": ["lead", "peer"], "orchestrator": "lead",
               "accept_from": ["peer"]}),
    );

    // 在 accept_from 裡 → 允許。
    let allowed = daemon.instance_call(
        &home,
        "peer",
        "send",
        json!({"instance": "lead", "message": "回報"}),
    );
    assert_eq!(
        allowed.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：accept_from 內的 sender 應可送給 orchestrator，實際為 {allowed:?}"
    );

    // 不在 accept_from 裡 → team_acl_denied。
    let refused = daemon.instance_call(
        &home,
        "stranger",
        "send",
        json!({"instance": "lead", "message": "越權"}),
    );
    assert_code(&refused, otterhive::protocol::ErrorCode::TeamAclDenied);
    assert!(
        message_rows(&home, "lead").len() == 1,
        "繁中說明：被拒的 send 不得留下訊息，實際 {:?}",
        message_rows(&home, "lead")
    );

    // accept_from 取代後，改用新名單比對。
    let updated = daemon.operator_call(
        &home,
        "team",
        json!({"action": "update", "name": "squad", "accept_from": ["stranger"]}),
    );
    assert_eq!(
        updated.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：update 應完成，實際為 {updated:?}"
    );
    let now_allowed = daemon.instance_call(
        &home,
        "stranger",
        "send",
        json!({"instance": "lead", "message": "現在可以了"}),
    );
    assert_eq!(
        now_allowed.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：accept_from 更新後應允許，實際為 {now_allowed:?}"
    );
    let now_refused = daemon.instance_call(
        &home,
        "peer",
        "send",
        json!({"instance": "lead", "message": "現在不行了"}),
    );
    assert_code(&now_refused, otterhive::protocol::ErrorCode::TeamAclDenied);
}

/// §6.2：manager 可送給任何 instance。
#[test]
fn a_manager_may_send_to_any_instance() {
    let home = TestHome::new();
    // §1：managers 的變更要重啟 daemon 才生效，所以先寫設定再啟動。
    home.set_managers(&["alpha"]);
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");
    create_instance(&daemon, &home, "beta");
    create_team(
        &daemon,
        &home,
        "left",
        json!({"members": ["alpha"], "orchestrator": "alpha"}),
    );
    create_team(
        &daemon,
        &home,
        "right",
        json!({"members": ["beta"], "orchestrator": "beta"}),
    );

    let sent = daemon.instance_call(
        &home,
        "alpha",
        "send",
        json!({"instance": "beta", "message": "manager 跨隊"}),
    );
    assert_eq!(
        sent.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：manager 應可送給任何 instance，實際為 {sent:?}"
    );
    assert_eq!(message_rows(&home, "beta").len(), 1);
}

/// §2／§6.1：sender 一律由憑證推導——instance 用自己的 token 送，
/// `inbox` 看到的 `from` 就是那個 instance 的名稱。
#[test]
fn the_sender_is_derived_from_the_credential() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");
    create_instance(&daemon, &home, "beta");

    let sent = daemon.instance_call(
        &home,
        "alpha",
        "send",
        json!({"instance": "beta", "message": "你好"}),
    );
    assert_eq!(
        sent.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：instance 之間（都沒有 team）應允許，實際為 {sent:?}"
    );

    let (_, _, messages) = drain(&daemon, &home, "beta");
    assert_eq!(messages.len(), 1, "繁中說明：應取到一則，實際 {messages:?}");
    assert_eq!(
        messages[0]["from"],
        json!({"kind": "instance", "name": "alpha"}),
        "繁中說明：from 應由 token 推導為 alpha，實際 {}",
        messages[0]
    );
}

/// §6.1：team 廣播到不存在的 team 回 `not_found`。
#[test]
fn sending_to_an_unknown_team_is_not_found() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");

    let refused = daemon.operator_call(
        &home,
        "send",
        json!({"team": "ghost-team", "message": "沒人"}),
    );
    assert_code(&refused, otterhive::protocol::ErrorCode::NotFound);
    assert!(
        message_rows(&home, "alpha").is_empty(),
        "繁中說明：被拒的 team 廣播不得留下訊息"
    );
}

/// §6.4 的一個可觀測前提：喚醒提示**不影響**訊息投遞。收件人在 `running`
/// 時提示會寫入 PTY，訊息仍留在 inbox（無論提示是否送出）。
///
/// 這一條是 Lane B（wake）的驗收前提；這裡只驗「send 完成後訊息一定可 drain」。
#[test]
fn a_hinted_message_is_still_drainable() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");

    let sent = operator_send(&daemon, &home, "alpha", "會被提示的訊息");
    assert_eq!(
        sent.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：send 應完成，實際為 {sent:?}"
    );

    let (_, _, messages) = drain(&daemon, &home, "alpha");
    assert_eq!(
        messages.len(),
        1,
        "繁中說明：無論提示是否送出，訊息都應可 drain，實際 {messages:?}"
    );
    assert_eq!(messages[0]["body"], json!("會被提示的訊息"));
}

/// §6.4：`value.recipients` 每項帶 `wake` 欄位（`hinted`／`deferred`／
/// `not_running`／`failed`）。
///
/// TODO(T5-Lane-B)：`wake` 的**取值**與排程行為由 Lane B 的喚醒測試驗收；
/// 這裡只確認欄位存在且是四個契約值之一。
#[test]
fn recipients_report_a_wake_field() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_instance(&daemon, &home, "alpha");

    let sent = operator_send(&daemon, &home, "alpha", "喚醒");
    let recipients = sent.value["recipients"]
        .as_array()
        .unwrap_or_else(|| panic!("繁中說明：send 應回 recipients 陣列，實際 {}", sent.value))
        .clone();
    let wake = recipients[0]["wake"]
        .as_str()
        .unwrap_or_else(|| panic!("繁中說明：queued 的收件人應帶 wake 欄位，實際 {recipients:?}"));
    assert!(
        ["hinted", "deferred", "not_running", "failed"].contains(&wake),
        "繁中說明：wake 應是契約列出的四個值之一，實際 {wake}"
    );
}

/// Unix epoch 毫秒（契約 §1：時間一律是 epoch 毫秒整數）。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}
