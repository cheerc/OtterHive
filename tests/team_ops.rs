//! T5 Lane A RED：team 操作的契約（§8.1）。
//!
//! 依 `docs/plans/p1a-implementation.md` T5。驗收重點：
//! - `create` 的必填、成員存在性與單一 team 歸屬；`update` 的 add／remove／
//!   orchestrator；`delete` 不刪 instance。
//! - **所有檢查在同一交易內、任何 team 異動前完成**：拒絕時不留下部分變更。
//! - §2 的授權：team 異動（含 `list`）只有 operator，instance 只能 `list`。
//! - §4.5：以 `delete_instance` 刪除某 team 的 orchestrator 回 `invalid_argument`。
//!
//! 全檔只在 macOS 編譯；所有等待有上限並回報最後觀測狀態；使用暫存的
//! `HOME`／`OTTERHIVE_HOME`／cwd。backend 一律用 `fixture()` 的絕對路徑，
//! **不依賴 PATH 上的真實程式**（T4 `backend_contract.rs` 的 CI 事故）。
//!
//! RED 說明：`team` 的 daemon 端行為由 T5 GREEN 建立。在此之前 `perform`
//! 對 `team` 回 `unsupported_in_phase`，因此本檔是「編譯通過、執行失敗」的
//! 行為 RED；`tests/team_ops.rs` 不直接引用 `otterhive::store` 的 team 查詢
//! 介面（那些由 coordinator 在 GREEN 前落地），只透過 socket 打真 daemon。

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

    /// 隔離的暫存 home：`HOME`／`OTTERHIVE_HOME`／workspace 都落在這裡。
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
            // 不能只等 socket 檔存在：daemon 重啟時上一個行程留下的 socket
            // 檔還在，而新的 daemon 可能仍在 prepare 階段。等待條件必須是
            // 「真的能送出請求並收到回應」。
            let deadline = Instant::now() + LONG_TIMEOUT;
            loop {
                if socket.exists() && UnixStream::connect(&socket).is_ok() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "繁中說明：daemon 應在期限內可服務 {}（啟動自檢失敗？）",
                    socket.display()
                );
                std::thread::sleep(POLL_STEP);
            }
            daemon
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

    /// 以 `echo_agent.sh`（絕對路徑）建立並啟動一個 instance。
    pub fn create_instance(daemon: &Daemon, home: &TestHome, name: &str) -> OperationResult {
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
        created
    }

    /// `team` 操作的薄封裝。
    pub fn team(daemon: &Daemon, home: &TestHome, args: Value) -> OperationResult {
        daemon.operator_call(home, "team", args)
    }

    /// `team` 的 `action: "list"` 結果裡的 `teams` 陣列。
    pub fn team_list(daemon: &Daemon, home: &TestHome) -> Vec<Value> {
        let listed = team(daemon, home, json!({"action": "list"}));
        assert_eq!(
            listed.outcome,
            otterhive::protocol::Outcome::Completed,
            "繁中說明：team list 應完成，實際為 {listed:?}"
        );
        listed.value["teams"]
            .as_array()
            .unwrap_or_else(|| panic!("繁中說明：team list 應回 teams 陣列，實際 {}", listed.value))
            .clone()
    }

    /// 在 `teams` 中找出指定名稱的項目（以 `value.teams` 為準）。
    pub fn find_team(daemon: &Daemon, home: &TestHome, name: &str) -> Option<Value> {
        team_list(daemon, home)
            .into_iter()
            .find(|item| item["name"] == json!(name))
    }

    /// 錯誤碼斷言：`OperationResult` 的 `error.code` 應為指定值。
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

    /// 直接把某個 team 的 `deployment` 欄位設成非 null（測試 §8.1「屬於某
    /// deployment 的 team 只能 teardown 刪」；T6 才有真正的 deployment 資料）。
    pub fn mark_team_owned_by_deployment(home: &TestHome, team: &str, deployment: &str) {
        let path = home.home.root().join("state.sqlite3");
        let connection = rusqlite::Connection::open(&path)
            .unwrap_or_else(|e| panic!("繁中說明：應可開啟 {}：{e}", path.display()));
        let changed = connection
            .execute(
                "UPDATE teams SET deployment = ?2 WHERE name = ?1",
                params![team, deployment],
            )
            .unwrap_or_else(|e| panic!("繁中說明：應可更新 teams.deployment：{e}"));
        assert_eq!(changed, 1, "繁中說明：team {team} 應存在（測試前提）");
    }

    /// `messages` 表中某收件 instance 的列：`(id, body, state, delivery_count)`，
    /// 依建立順序排列。
    ///
    /// 直接讀 SQLite 而不是用 `inbox` drain，是為了在不改變投遞狀態（drain 會
    /// 把 `unread` 變成 `delivering`）的前提下觀察「到底寫了幾筆」。
    pub fn message_rows(home: &TestHome, recipient: &str) -> Vec<(String, String, String, i64)> {
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
                "SELECT id, body, state, delivery_count FROM messages \
                 WHERE recipient_id = ?1 ORDER BY seq",
            )
            .unwrap_or_else(|e| panic!("繁中說明：應可準備 messages 查詢：{e}"));
        let rows = statement
            .query_map(params![recipient_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .unwrap_or_else(|e| panic!("繁中說明：應可查詢 messages：{e}"))
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|e| panic!("繁中說明：應可收集 messages 列：{e}"));
        rows
    }
}

use support::*;

/// 建立兩個 instance，測試再自行 `team create`。
fn two_instances(daemon: &Daemon, home: &TestHome) {
    create_instance(daemon, home, "alpha");
    create_instance(daemon, home, "beta");
}

/// §6.1：team 廣播的收件人是該 team **除 sender 以外**的所有成員；收件人
/// 名單去重；sender 不會收到自己的訊息。
#[test]
fn team_broadcast_excludes_the_sender_and_deduplicates() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);
    create_instance(&daemon, &home, "gamma");

    let created = team(
        &daemon,
        &home,
        // beta 重複出現：去重後仍只有一份。
        json!({"action": "create", "name": "squad",
               "members": ["alpha", "beta", "beta", "gamma"], "orchestrator": "alpha"}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );

    let sent = daemon.call(
        &home,
        &home.instance_auth("alpha"),
        "send",
        json!({"team": "squad", "message": "廣播", "request_kind": "update"}),
    );
    assert_eq!(
        sent.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：team 廣播應完成，實際為 {sent:?}"
    );

    let recipients = sent.value["recipients"]
        .as_array()
        .unwrap_or_else(|| panic!("繁中說明：send 應回 recipients 陣列，實際 {}", sent.value))
        .clone();
    let mut names: Vec<String> = recipients
        .iter()
        .map(|item| item["instance"].as_str().unwrap_or_default().to_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["beta".to_owned(), "gamma".to_owned()],
        "繁中說明：廣播收件人應是除 sender 外的成員且去重，實際 {recipients:?}"
    );

    // beta 恰好收到一則（去重），alpha 沒有。
    assert_eq!(
        message_rows(&home, "beta").len(),
        1,
        "繁中說明：beta 應只收到一則，實際 {:?}",
        message_rows(&home, "beta")
    );
    assert!(
        message_rows(&home, "alpha").is_empty(),
        "繁中說明：sender 不會收到自己的訊息，實際 {:?}",
        message_rows(&home, "alpha")
    );

    // 順帶確認每個收件人的 outcome 與 message_id。
    for recipient in &recipients {
        assert_eq!(
            recipient["outcome"],
            json!("queued"),
            "繁中說明：收件人應為 queued，實際 {recipient}"
        );
        assert!(
            recipient["message_id"].is_string(),
            "繁中說明：queued 的收件人應帶 message_id，實際 {recipient}"
        );
    }
}

/// §8.1：`create` 必須有非空 `members` 與 `orchestrator`，orchestrator 必須在
/// members 裡，member 必須是已存在、未刪除的 instance。
#[test]
fn create_requires_members_and_an_orchestrator_among_them() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);

    // 缺 members。
    let missing_members = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "t1", "orchestrator": "alpha"}),
    );
    assert_code(
        &missing_members,
        otterhive::protocol::ErrorCode::InvalidArgument,
    );

    // 缺 orchestrator。
    let missing_orchestrator = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "t2", "members": ["alpha"]}),
    );
    assert_code(
        &missing_orchestrator,
        otterhive::protocol::ErrorCode::InvalidArgument,
    );

    // 空 members。
    let empty_members = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "t3", "members": [], "orchestrator": "alpha"}),
    );
    assert_code(
        &empty_members,
        otterhive::protocol::ErrorCode::InvalidArgument,
    );

    // orchestrator 不在 members 裡。
    let orchestrator_outside = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "t4",
               "members": ["beta"], "orchestrator": "alpha"}),
    );
    assert_code(
        &orchestrator_outside,
        otterhive::protocol::ErrorCode::InvalidArgument,
    );

    // 以上四次都不得留下 team（拒絕時不留下部分變更）。
    assert_eq!(
        team_list(&daemon, &home).len(),
        0,
        "繁中說明：被拒的 create 不應留下任何 team，實際 {:?}",
        team_list(&daemon, &home)
    );
}

/// §8.1：名稱規則同 instance 名稱（§4.1：1–64 字元、`[A-Za-z0-9_-]`）。
#[test]
fn team_name_follows_the_instance_name_rules() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);

    for bad_name in ["bad name", "bad.name", "", "斜線/name", "emoji🐝"] {
        let rejected = team(
            &daemon,
            &home,
            json!({"action": "create", "name": bad_name,
                   "members": ["alpha"], "orchestrator": "alpha"}),
        );
        assert_code(&rejected, otterhive::protocol::ErrorCode::InvalidArgument);
    }

    let accepted = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "Team_A-1",
               "members": ["alpha"], "orchestrator": "alpha"}),
    );
    assert_eq!(
        accepted.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：合法的 team 名稱應被接受，實際為 {accepted:?}"
    );
}

/// §8.1：成員不存在回 `not_found`；成員已在另一個 team 回 `name_conflict`。
#[test]
fn unknown_member_is_not_found_and_a_member_belongs_to_one_team() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);

    let unknown = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "squad",
               "members": ["alpha", "ghost"], "orchestrator": "alpha"}),
    );
    assert_code(&unknown, otterhive::protocol::ErrorCode::NotFound);

    let first = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "squad",
               "members": ["alpha", "beta"], "orchestrator": "alpha"}),
    );
    assert_eq!(
        first.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：第一個 team 應建立成功，實際為 {first:?}"
    );

    // beta 已屬於 squad，不能再屬於另一個 team。
    let second = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "other",
               "members": ["beta"], "orchestrator": "beta"}),
    );
    assert_code(&second, otterhive::protocol::ErrorCode::NameConflict);
    assert!(
        find_team(&daemon, &home, "other").is_none(),
        "繁中說明：被拒的 create 不應留下 team other"
    );
}

/// §8.1：不存在的 team（update／delete）回 `not_found`；重複名稱回
/// `name_conflict`。
#[test]
fn unknown_team_is_not_found_and_duplicate_names_conflict() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);

    let missing_update = team(
        &daemon,
        &home,
        json!({"action": "update", "name": "ghost-team", "add": ["beta"]}),
    );
    assert_code(&missing_update, otterhive::protocol::ErrorCode::NotFound);

    let missing_delete = team(
        &daemon,
        &home,
        json!({"action": "delete", "name": "ghost-team"}),
    );
    assert_code(&missing_delete, otterhive::protocol::ErrorCode::NotFound);

    let created = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "squad",
               "members": ["alpha"], "orchestrator": "alpha"}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );

    let duplicate = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "squad",
               "members": ["beta"], "orchestrator": "beta"}),
    );
    assert_code(&duplicate, otterhive::protocol::ErrorCode::NameConflict);
}

/// §8.1：`update` 的 `add`／`remove`／`orchestrator` 都生效。
#[test]
fn update_adds_removes_and_switches_the_orchestrator() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);
    create_instance(&daemon, &home, "gamma");

    let created = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "squad",
               "members": ["alpha", "beta"], "orchestrator": "alpha"}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );

    let added = team(
        &daemon,
        &home,
        json!({"action": "update", "name": "squad", "add": ["gamma"]}),
    );
    assert_eq!(
        added.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：update add 應完成，實際為 {added:?}"
    );

    // 換 orchestrator：同一次操作先換人，再移除原 orchestrator。
    let switched = team(
        &daemon,
        &home,
        json!({"action": "update", "name": "squad",
               "orchestrator": "beta", "remove": ["alpha"]}),
    );
    assert_eq!(
        switched.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：update orchestrator＋remove 應完成，實際為 {switched:?}"
    );

    let current = find_team(&daemon, &home, "squad")
        .unwrap_or_else(|| panic!("繁中說明：squad 應該還在 list 裡"));
    assert_eq!(
        current["orchestrator"],
        json!("beta"),
        "繁中說明：orchestrator 應換成 beta，實際 {current}"
    );
    let mut members = current["members"]
        .as_array()
        .unwrap_or_else(|| panic!("繁中說明：list 應回 members 陣列，實際 {current}"))
        .clone();
    members.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    assert_eq!(
        members,
        vec![json!("beta"), json!("gamma")],
        "繁中說明：成員應是 beta 與 gamma（alpha 已移除），實際 {current}"
    );
}

/// §8.1：`update` 若 remove 原 orchestrator，**必須在同一操作**指定仍在成員中的
/// 新 orchestrator，否則 `invalid_argument`。
#[test]
fn removing_the_orchestrator_without_a_replacement_is_rejected_atomically() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);
    create_instance(&daemon, &home, "gamma");

    let created = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "squad",
               "members": ["alpha", "beta"], "orchestrator": "alpha"}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );

    // 同一次操作：先 add gamma，再 remove orchestrator alpha，卻沒有指定新的
    // orchestrator → 整筆拒絕。
    let rejected = team(
        &daemon,
        &home,
        json!({"action": "update", "name": "squad",
               "add": ["gamma"], "remove": ["alpha"]}),
    );
    assert_code(&rejected, otterhive::protocol::ErrorCode::InvalidArgument);

    // 正面驗證「拒絕時不留下部分 add／remove」：list 必須與操作前完全相同。
    let current = find_team(&daemon, &home, "squad")
        .unwrap_or_else(|| panic!("繁中說明：squad 應該還在 list 裡"));
    assert_eq!(
        current["orchestrator"],
        json!("alpha"),
        "繁中說明：被拒的 update 不得換掉 orchestrator，實際 {current}"
    );
    let mut members = current["members"]
        .as_array()
        .unwrap_or_else(|| panic!("繁中說明：list 應回 members 陣列，實際 {current}"))
        .clone();
    members.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    assert_eq!(
        members,
        vec![json!("alpha"), json!("beta")],
        "繁中說明：被拒的 update 不得留下部分 add（gamma 不應出現），實際 {current}"
    );
}

/// §8.1：新 orchestrator 不在變更後的 members（即使有指定）也是
/// `invalid_argument`。
#[test]
fn the_new_orchestrator_must_be_in_the_resulting_members() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);

    let created = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "squad",
               "members": ["alpha", "beta"], "orchestrator": "alpha"}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );

    // 指定 beta 為 orchestrator，但同一次操作把 beta 移除了。
    let contradictory = team(
        &daemon,
        &home,
        json!({"action": "update", "name": "squad",
               "orchestrator": "beta", "remove": ["beta"]}),
    );
    assert_code(
        &contradictory,
        otterhive::protocol::ErrorCode::InvalidArgument,
    );

    // 指定一個不在任何 members 裡的 instance。
    let outsider = team(
        &daemon,
        &home,
        json!({"action": "update", "name": "squad", "orchestrator": "ghost"}),
    );
    assert_code(&outsider, otterhive::protocol::ErrorCode::InvalidArgument);

    let current = find_team(&daemon, &home, "squad")
        .unwrap_or_else(|| panic!("繁中說明：squad 應該還在 list 裡"));
    assert_eq!(
        current["orchestrator"],
        json!("alpha"),
        "繁中說明：被拒的 update 不得改變 team，實際 {current}"
    );
}

/// §8.1：`list` 每項含 `name`、`orchestrator`、`members`、`description`、
/// `accept_from`、`deployment`。
#[test]
fn list_reports_every_contracted_field() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);

    let created = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "squad",
               "members": ["beta", "alpha"], "orchestrator": "beta",
               "description": "合約上的說明", "accept_from": ["alpha"]}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );

    let listed = team_list(&daemon, &home);
    assert_eq!(
        listed.len(),
        1,
        "繁中說明：應只有一個 team，實際 {listed:?}"
    );
    let entry = &listed[0];
    assert_eq!(entry["name"], json!("squad"));
    assert_eq!(entry["orchestrator"], json!("beta"));
    assert_eq!(entry["description"], json!("合約上的說明"));
    assert_eq!(
        entry["accept_from"],
        json!(["alpha"]),
        "繁中說明：accept_from 應原樣列出，實際 {entry}"
    );
    assert_eq!(
        entry["deployment"],
        Value::Null,
        "繁中說明：非 deployment 的 team 應回 deployment: null，實際 {entry}"
    );
    for field in [
        "name",
        "orchestrator",
        "members",
        "description",
        "accept_from",
        "deployment",
    ] {
        assert!(
            entry.get(field).is_some(),
            "繁中說明：list 項目應含欄位 {field}，實際 {entry}"
        );
    }
    let mut members = entry["members"]
        .as_array()
        .unwrap_or_else(|| panic!("繁中說明：members 應是陣列，實際 {entry}"))
        .clone();
    members.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    assert_eq!(
        members,
        vec![json!("alpha"), json!("beta")],
        "繁中說明：members 應是 alpha 與 beta，實際 {entry}"
    );
}

/// §8.1：`description` 省略時 list 應回 `null`；`accept_from` 省略時應是空陣列。
#[test]
fn omitted_description_and_accept_from_have_specified_shapes() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);

    let created = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "squad",
               "members": ["alpha"], "orchestrator": "alpha"}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );

    let entry = find_team(&daemon, &home, "squad")
        .unwrap_or_else(|| panic!("繁中說明：squad 應該在 list 裡"));
    assert_eq!(
        entry["description"],
        Value::Null,
        "繁中說明：省略 description 應回 null，實際 {entry}"
    );
    assert_eq!(
        entry["accept_from"],
        json!([]),
        "繁中說明：省略 accept_from 應回空陣列，實際 {entry}"
    );
}

/// §8.1：`delete` 移除 team 與成員關係，但**不刪除 instance**。
#[test]
fn delete_removes_the_team_but_keeps_the_instances() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);

    let created = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "squad",
               "members": ["alpha", "beta"], "orchestrator": "alpha"}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );

    let deleted = team(&daemon, &home, json!({"action": "delete", "name": "squad"}));
    assert_eq!(
        deleted.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：delete 應完成，實際為 {deleted:?}"
    );
    assert!(
        find_team(&daemon, &home, "squad").is_none(),
        "繁中說明：delete 後不應再列到 squad"
    );

    let listed = daemon.operator_call(&home, "list_instances", json!({}));
    let names: Vec<String> = listed.value["instances"]
        .as_array()
        .unwrap_or_else(|| panic!("繁中說明：list_instances 應回陣列，實際 {}", listed.value))
        .iter()
        .map(|item| item["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        names,
        vec!["alpha".to_owned(), "beta".to_owned()],
        "繁中說明：delete team 不得刪除 instance，實際 {names:?}"
    );

    // 成員關係真的被清掉：同一批 instance 可以組成新的 team。
    let recreated = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "squad",
               "members": ["alpha", "beta"], "orchestrator": "beta"}),
    );
    assert_eq!(
        recreated.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：delete 後應能以同名 team 重建，實際為 {recreated:?}"
    );
}

/// §8.1：屬於某 deployment 的 team 只能經 teardown 刪，直接 `delete` 回
/// `invalid_argument`。
///
/// T6 才有 deployment 資料，所以這裡以「把 `teams.deployment` 欄位設成非
/// null」模擬 deployment 擁有的 team，驗證的是契約句：`deployment` 非 null
/// 的 team 不得直接刪除。
#[test]
fn a_team_owned_by_a_deployment_cannot_be_deleted_directly() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);

    let created = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "squad",
               "members": ["alpha", "beta"], "orchestrator": "alpha"}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );

    // 測試直接改 store，讓 squad 看起來由某個 deployment 擁有。
    mark_team_owned_by_deployment(&home, "squad", "deployment-x");

    let deleted = team(&daemon, &home, json!({"action": "delete", "name": "squad"}));
    assert_code(&deleted, otterhive::protocol::ErrorCode::InvalidArgument);
    assert!(
        find_team(&daemon, &home, "squad").is_some(),
        "繁中說明：被拒的 delete 不得移除 deployment 擁有的 team"
    );
}

/// §4.5：以 `delete_instance` 刪除某 team 的 orchestrator 回 `invalid_argument`，
/// 要求先更換 orchestrator 或刪除 team。
#[test]
fn deleting_a_team_orchestrator_is_rejected() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);
    create_instance(&daemon, &home, "gamma");

    let created = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "squad",
               "members": ["alpha", "beta", "gamma"], "orchestrator": "alpha"}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );

    let refused = daemon.operator_call(&home, "delete_instance", json!({"instance": "alpha"}));
    assert_code(&refused, otterhive::protocol::ErrorCode::InvalidArgument);

    // instance 仍在（未被刪除）。
    let listed = daemon.operator_call(&home, "list_instances", json!({"instance": "alpha"}));
    let instances = listed.value["instances"]
        .as_array()
        .unwrap_or_else(|| panic!("繁中說明：list 應回陣列，實際 {}", listed.value));
    assert_eq!(
        instances.len(),
        1,
        "繁中說明：被拒的 delete_instance 不得刪掉 instance，實際 {instances:?}"
    );

    // 非 orchestrator 的成員應可刪除（§4.5 只限制 orchestrator）。
    let member_deleted =
        daemon.operator_call(&home, "delete_instance", json!({"instance": "gamma"}));
    assert_eq!(
        member_deleted.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：非 orchestrator 的成員應可刪除，實際為 {member_deleted:?}"
    );

    // 改換 orchestrator 之後，原 orchestrator 也可以刪除。
    let switched = team(
        &daemon,
        &home,
        json!({"action": "update", "name": "squad", "orchestrator": "beta"}),
    );
    assert_eq!(
        switched.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：換 orchestrator 的 update 應完成，實際為 {switched:?}"
    );
    let now_deletable =
        daemon.operator_call(&home, "delete_instance", json!({"instance": "alpha"}));
    assert_eq!(
        now_deletable.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：不再是 orchestrator 的 alpha 應可刪除，實際為 {now_deletable:?}"
    );
}

/// §2 矩陣：team 異動（含 `list`）只有 operator；instance 只能 `list`。
#[test]
fn instance_callers_may_only_list_teams() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);

    let created = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "squad",
               "members": ["alpha", "beta"], "orchestrator": "alpha"}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );

    let auth = home.instance_auth("alpha");

    // list 允許（§2 表格：list_instances、team list 對 instance 可）。
    let listed = daemon.call(&home, &auth, "team", json!({"action": "list"}));
    assert_eq!(
        listed.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：instance 應可 team list，實際為 {listed:?}"
    );
    assert_eq!(
        listed.value["teams"]
            .as_array()
            .map(|teams| teams.len())
            .unwrap_or(usize::MAX),
        1,
        "繁中說明：instance 應看到同一份 team 清單，實際 {}",
        listed.value
    );

    for args in [
        json!({"action": "create", "name": "sneaky", "members": ["alpha"], "orchestrator": "alpha"}),
        json!({"action": "update", "name": "squad", "description": "被 instance 改的"}),
        json!({"action": "delete", "name": "squad"}),
    ] {
        let refused = daemon.call(&home, &auth, "team", args.clone());
        assert_code(&refused, otterhive::protocol::ErrorCode::Unauthorized);
    }

    // 被拒的異動都沒有生效。
    let entry = find_team(&daemon, &home, "squad")
        .unwrap_or_else(|| panic!("繁中說明：squad 應該還在 list 裡"));
    assert_eq!(
        entry["description"],
        Value::Null,
        "繁中說明：instance 的 update 不得生效，實際 {entry}"
    );
    assert!(
        find_team(&daemon, &home, "sneaky").is_none(),
        "繁中說明：instance 的 create 不得生效"
    );
}

/// §2：team 異動需要 operator 憑證；無效憑證同樣被拒。
#[test]
fn an_invalid_credential_cannot_mutate_teams() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);

    let bogus = otterhive::protocol::Auth::Operator("not-the-operator-token".to_owned());
    let refused = daemon.call(
        &home,
        &bogus,
        "team",
        json!({"action": "create", "name": "squad", "members": ["alpha"], "orchestrator": "alpha"}),
    );
    assert_code(&refused, otterhive::protocol::ErrorCode::Unauthorized);
    assert_eq!(
        team_list(&daemon, &home).len(),
        0,
        "繁中說明：無效憑證不得建立 team"
    );
}

/// §8.1：`accept_from` 與 `description` 可在 `update` 取代。
#[test]
fn update_replaces_description_and_accept_from() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);

    let created = team(
        &daemon,
        &home,
        json!({"action": "create", "name": "squad",
               "members": ["alpha", "beta"], "orchestrator": "alpha",
               "description": "第一版", "accept_from": ["beta"]}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );

    let updated = team(
        &daemon,
        &home,
        json!({"action": "update", "name": "squad",
               "description": "第二版", "accept_from": []}),
    );
    assert_eq!(
        updated.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：update 應完成，實際為 {updated:?}"
    );

    let entry = find_team(&daemon, &home, "squad")
        .unwrap_or_else(|| panic!("繁中說明：squad 應該在 list 裡"));
    assert_eq!(
        entry["description"],
        json!("第二版"),
        "繁中說明：description 應被取代，實際 {entry}"
    );
    assert_eq!(
        entry["accept_from"],
        json!([]),
        "繁中說明：accept_from 應被取代為空陣列，實際 {entry}"
    );
}

/// §3：未知的 action 回 `invalid_argument`（不是 `unsupported`）。
#[test]
fn an_unknown_team_action_is_invalid_argument() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    two_instances(&daemon, &home);

    let rejected = team(
        &daemon,
        &home,
        json!({"action": "reticulate", "name": "squad"}),
    );
    assert_code(&rejected, otterhive::protocol::ErrorCode::InvalidArgument);
}
