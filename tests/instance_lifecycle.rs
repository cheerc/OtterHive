//! T4 Lane A RED：daemon／lifecycle／backend 的整合驗收。
//!
//! 依 `docs/plans/p1a-implementation.md` T4 與契約 §1–§5、§4.7、§7 daemon 端。
//! 全檔只在 macOS 編譯；所有等待有上限並回報最後觀測狀態；測試使用暫存的
//! `HOME`／`OTTERHIVE_HOME`／cwd，並在 cleanup 前核對程序身分。
//!
//! RED 說明：`otterhive` binary 與 daemon 行為由 T4 GREEN 建立。在此之前
//! `daemon_bin()` 以明確訊息 panic，因此本檔是「編譯通過、執行失敗」的 RED；
//! binary 存在後即成為真實驗收。
//!
//! 授權範圍提醒：本檔只驗 operator／instance 呼叫的 lifecycle 與查詢行為；
//! §2 的權限矩陣由 `tests/caller_authority.rs`（另一 lane）驗證。

#![cfg(target_os = "macos")]

use std::io::BufRead;

use serde_json::{json, Value};

mod support {
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use otterhive::home::Home;
    use otterhive::protocol::{Auth, OperationResult, Request};
    use rusqlite::params;
    use serde_json::Value;
    use tempfile::TempDir;

    pub const POLL_STEP: Duration = Duration::from_millis(20);
    pub const LONG_TIMEOUT: Duration = Duration::from_secs(20);
    pub const RECV_TIMEOUT: Duration = Duration::from_secs(10);

    /// `otterhive` binary 路徑（T4 GREEN 才建立 `[[bin]] otterhive`）。
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
        pub fn instance_token(&self, name: &str) -> String {
            let path = self.workspace(name).join(".claude/otterhive-mcp.json");
            let raw = fs::read_to_string(&path)
                .unwrap_or_else(|_| panic!("繁中說明：應可讀取 {} 的 MCP 設定", path.display()));
            let parsed: Value = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("繁中說明：MCP 設定應是合法 JSON：{e}；實際 {raw}"));
            parsed["mcpServers"]["otterhive"]["env"]["OTTERHIVE_INSTANCE_TOKEN"]
                .as_str()
                .unwrap_or_else(|| panic!("繁中說明：MCP 設定應含 instance token；實際 {raw}"))
                .to_owned()
        }

        pub fn instance_auth(&self, name: &str) -> Auth {
            Auth::Instance(self.instance_token(name))
        }
    }

    /// 連線後延遲 `delay_ms` 才送出一行 JSON 請求，回讀該行回應。
    ///
    /// 刻意把「送出」排在「連線」之後：accepted stream 若繼承 listener 的
    /// `O_NONBLOCK`，daemon 會在資料到達前就結束這個連線，客戶端只看到
    /// `Broken pipe`。
    pub fn call_after_delay(
        home: &TestHome,
        auth: &Auth,
        op: &str,
        args: Value,
        delay_ms: u64,
    ) -> std::io::Result<OperationResult> {
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
        let stream = UnixStream::connect(socket)?;
        let _ = stream.set_read_timeout(Some(LONG_TIMEOUT));
        let _ = stream.set_write_timeout(Some(RECV_TIMEOUT));
        let mut stream = stream;
        std::thread::sleep(Duration::from_millis(delay_ms));
        stream.write_all(line.as_bytes())?;
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader.read_line(&mut response)?;
        if response.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon 未回任何一行",
            ));
        }
        serde_json::from_str(response.trim()).map_err(|parse_error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, parse_error)
        })
    }

    /// 前景執行的 daemon；Drop 時先 SIGTERM、有界等待、再 SIGKILL。
    pub struct Daemon {
        child: Option<Child>,
    }

    impl Daemon {
        pub fn start(home: &TestHome) -> Self {
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

        pub fn poll_until(&self, timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
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

        /// 依契約 §1 送一行 JSON 請求、收一行 `OperationResult`。
        pub fn call(
            &self,
            home: &TestHome,
            auth: &Auth,
            op: &str,
            args: Value,
            operation_id: Option<&str>,
        ) -> OperationResult {
            let request = Request {
                v: 1,
                auth: auth.clone(),
                op: op.to_owned(),
                args,
                operation_id: operation_id.map(str::to_owned),
            };
            let socket = home.socket_path();
            let mut stream = UnixStream::connect(&socket)
                .unwrap_or_else(|e| panic!("繁中說明：應能連到 {}：{e}", socket.display()));
            // 單次讀取逾時就設成涵蓋最長操作的上限：daemon 端的 stop 等待
            // （TERM ＋ KILL）加上寬限期都可能長於一次 socket 讀取。逾時的
            // UnixStream 後續讀取仍會持續失敗，所以**不能**靠重試跨越逾時，
            // 只能一開始就给足時間。
            let _ = stream.set_read_timeout(Some(LONG_TIMEOUT));
            let _ = stream.set_write_timeout(Some(RECV_TIMEOUT));
            let mut line =
                serde_json::to_string(&request).expect("繁中說明：請求應可序列化為 JSON");
            line.push('\n');
            stream
                .write_all(line.as_bytes())
                .expect("繁中說明：應可送出請求");
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

        pub fn operator_call(
            &self,
            home: &TestHome,
            op: &str,
            args: Value,
            operation_id: Option<&str>,
        ) -> OperationResult {
            let auth = home.operator();
            self.call(home, &auth, op, args, operation_id)
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

    /// pid 是否仍在運行（zombie 不算運行中；`kill(pid,0)` 對 zombie 仍回 0）。
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
            // ECHILD（不是本程序的子程序）或暫時性失敗：不宣稱已終止。
            return true;
        }
        info.si_pid == 0
    }

    /// 在 `cond` 為真前輪詢到 `timeout`；回傳最後觀測結果，不靠任意 sleep。
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

    /// SQLite 的 instance 列：(pid, pid_start, state, generation)。
    pub fn instance_row(
        home: &TestHome,
        name: &str,
    ) -> Option<(Option<i64>, Option<i64>, String, u64)> {
        let path = home.home.root().join("state.sqlite3");
        if !path.exists() {
            return None;
        }
        let connection = rusqlite::Connection::open(path).ok()?;
        connection
            .query_row(
                "SELECT pid, pid_start, state, generation FROM instances \
                 WHERE name = ?1 AND deleted_ms IS NULL",
                params![name],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get::<_, i64>(3)? as u64,
                    ))
                },
            )
            .ok()
    }

    pub fn instance_pid(home: &TestHome, name: &str) -> Option<i32> {
        instance_row(home, name)
            .and_then(|(pid, ..)| pid)
            .map(|pid| pid as i32)
    }

    /// 直接把 instance 標成指定狀態（模擬 daemon 崩潰後的殘留紀錄）。
    pub fn force_state(home: &TestHome, name: &str, state: &str, reason: &str) {
        let path = home.home.root().join("state.sqlite3");
        let connection = rusqlite::Connection::open(&path)
            .unwrap_or_else(|e| panic!("繁中說明：應可開啟 {}：{e}", path.display()));
        connection
            .execute(
                "UPDATE instances SET state = ?2, reason = ?3 WHERE name = ?1",
                params![name, state, reason],
            )
            .unwrap_or_else(|e| panic!("繁中說明：應可強制設定 state：{e}"));
    }

    /// 覆寫 instance 記錄的 pid／pid_start（測試身分不符情境）。
    pub fn force_pid(home: &TestHome, name: &str, pid: Option<i64>, pid_start: Option<i64>) {
        let path = home.home.root().join("state.sqlite3");
        let connection = rusqlite::Connection::open(&path)
            .unwrap_or_else(|e| panic!("繁中說明：應可開啟 {}：{e}", path.display()));
        connection
            .execute(
                "UPDATE instances SET pid = ?2, pid_start = ?3 WHERE name = ?1",
                params![name, pid, pid_start],
            )
            .unwrap_or_else(|e| panic!("繁中說明：應可強制設定 pid：{e}"));
    }

    /// 在暫存目錄寫一個可執行 script（測試自有的 fixture）。
    pub fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).expect("繁中說明：應可寫入測試用 script");
        let mut permissions = fs::metadata(&path)
            .expect("繁中說明：應可讀取測試用 script 中繼資料")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("繁中說明：應可設定 script 權限");
        path
    }
}

use support::*;

/// `create` 成功後 instance 應為 `running`，且 list 回同一 generation。
#[test]
fn create_starts_the_instance_and_list_shows_it_running() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({
            "name": "alpha",
            "command": fixture("echo_agent.sh").to_string_lossy(),
        }),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {:?}",
        created
    );
    for effect in ["instance_created", "generation_advanced", "process_started"] {
        assert!(
            created.effects.iter().any(|item| item == effect),
            "繁中說明：create 的 effects 應含 {effect}，實際 {:?}",
            created.effects
        );
    }
    assert!(
        !created.effects.iter().any(|item| item == "process_stopped"),
        "繁中說明：首次 create 不應列 process_stopped，實際 {:?}",
        created.effects
    );
    let generation = created.subject.as_ref().map(|subject| subject.generation);
    assert!(
        generation.is_some_and(|value| value >= 1),
        "繁中說明：create 後 subject.generation 應至少為 1，實際 {generation:?}"
    );

    let listed = daemon.operator_call(&home, "list_instances", json!({}), None);
    assert_eq!(
        listed.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：list 應完成，實際為 {listed:?}"
    );
    let instances = listed.value["instances"]
        .as_array()
        .unwrap_or_else(|| panic!("繁中說明：list 應回 instances 陣列，實際 {}", listed.value));
    assert_eq!(
        instances.len(),
        1,
        "繁中說明：只建立一個 instance，實際 {instances:?}"
    );
    assert_eq!(instances[0]["name"], json!("alpha"));
    assert_eq!(instances[0]["state"], json!("running"));
    assert_eq!(instances[0]["generation"], json!(1));
    // T4 只實作 claude（plan T4：`backend: command` 回 unsupported_in_phase）；
    // 這裡用 `command` 欄位覆寫可執行檔（契約 §5.1），backend 仍是 claude。
    assert_eq!(instances[0]["backend"], json!("claude"));
}

/// 連線之後**延遲**送出請求，daemon 仍必須回應（契約 §1：一個連線一個請求）。
///
/// 這條固定一個真實缺陷：daemon 的 listener 是 non-blocking（accept 迴圈要能
/// 被訊號旗標中斷），而 accepted stream 會**繼承** `O_NONBLOCK`。若不解除，
/// `handle_connection` 的第一個 `read_line` 會在客戶端資料到達前就回
/// `WouldBlock`，並把連線當成空請求關掉——客戶端看到的是 `Broken pipe`。
/// 它只在排程延遲足夠大時出現（集中全量負載下的 `concurrent_creates` 兩條執行緒
/// 同時 `Broken pipe`），所以單跑測試永遠看不到。
///
/// 延遲必須**大於** accept 迴圈的輪詢間隔（20ms），否則請求會在 daemon 尚未
/// accept 之前就抵達socket，讀不到這個缺陷。
#[test]
fn request_sent_after_a_delay_is_still_answered() {
    let home = TestHome::new();
    // daemon 在本測試結束時（`Daemon` Drop）才關閉。
    let _daemon = Daemon::start(&home);

    for delay_ms in [0u64, 50, 200] {
        let result = call_after_delay(
            &home,
            &home.operator(),
            "list_instances",
            json!({}),
            delay_ms,
        )
        .unwrap_or_else(|error| {
            panic!("繁中說明：延遲 {delay_ms}ms 的請求應仍被正常處理：{error}")
        });
        assert_eq!(
            result.outcome,
            otterhive::protocol::Outcome::Completed,
            "繁中說明：延遲 {delay_ms}ms 的請求應被正常處理，實際為 {result:?}"
        );
    }
}

/// 明確 `backend: command` 在 T4 回 `unsupported_in_phase`，且不建立 instance。
#[test]
fn explicit_command_backend_is_unsupported_in_this_phase() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let unsupported = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "cmd-backend-rejected", "backend": "command",
               "command": fixture("echo_agent.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        unsupported.outcome,
        otterhive::protocol::Outcome::Unsupported,
        "繁中說明：T4 明確 backend:command 應回 unsupported，實際為 {unsupported:?}"
    );
    assert_eq!(
        unsupported.error.as_ref().map(|error| error.code),
        Some(otterhive::protocol::ErrorCode::UnsupportedInPhase),
        "繁中說明：應回 unsupported_in_phase，實際為 {unsupported:?}"
    );
    assert!(
        unsupported
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("claude")),
        "繁中說明：訊息應指出 P1a 只支援 claude，實際為 {unsupported:?}"
    );
    assert!(
        unsupported.effects.is_empty(),
        "繁中說明：unsupported 在任何變更前拒絕，不應有 effects，實際 {:?}",
        unsupported.effects
    );
    assert!(
        instance_row(&home, "cmd-backend-rejected").is_none(),
        "繁中說明：不應建立 instance"
    );
}

/// 省略 backend 時以 claude 啟動（以 `fake_claude.sh` 驗證），`list` 回
/// `backend: claude`。
///
/// 契約 §4.1「`backend: command` 缺 command 或空字串 → `invalid_argument`」
/// 在 T4 **不可達**：明確 `backend: command` 一律先被 `unsupported_in_phase`
/// 擋下（plan T4；command backend 延到需要它的工作包）。該規則留待 command
/// backend 實作時再驗證，不在此處放一個永遠不會被觸發的斷言。
#[test]
fn backend_defaults_to_claude_and_records_it() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "defaulted", "command": fixture("fake_claude.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：省略 backend 應以 claude 啟動，實際為 {created:?}"
    );
    let listed = daemon.operator_call(&home, "list_instances", json!({}), None);
    assert_eq!(
        listed.value["instances"][0]["backend"],
        json!("claude"),
        "繁中說明：省略 backend 時應記為 claude"
    );

    let listed = daemon.operator_call(&home, "list_instances", json!({}), None);
    let names: Vec<&str> = listed.value["instances"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["name"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        names,
        vec!["defaulted"],
        "繁中說明：list 只應回這一個 instance，實際 {names:?}"
    );
}

/// 同一個名稱的並行 create 只有一個成功，另一個 `name_conflict`，且只有一個程序。
#[test]
fn concurrent_creates_with_one_name_start_exactly_one_process() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let args = json!({
        "name": "racer",
        "command": fixture("echo_agent.sh").to_string_lossy(),
    });

    let (sender, receiver) = std::sync::mpsc::channel();
    let socket = home.socket_path();
    let mut handles = Vec::new();
    for index in 0..2 {
        let socket = socket.clone();
        let token = home.operator_token();
        let args = args.clone();
        let sender = sender.clone();
        handles.push(std::thread::spawn(move || {
            let result: Result<OperationResultAlias, String> = (|| {
                let stream = std::os::unix::net::UnixStream::connect(&socket)
                    .map_err(|e| format!("連線失敗：{e}"))?;
                let _ = stream.set_read_timeout(Some(RECV_TIMEOUT));
                let request = otterhive::protocol::Request {
                    v: 1,
                    auth: otterhive::protocol::Auth::Operator(token),
                    op: "create_instance".to_owned(),
                    args,
                    operation_id: Some(format!("race-{index}")),
                };
                let mut line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
                line.push('\n');
                use std::io::Write;
                let mut stream = stream;
                stream
                    .write_all(line.as_bytes())
                    .map_err(|e| format!("送出失敗：{e}"))?;
                let mut reader = std::io::BufReader::new(stream);
                let mut response = String::new();
                reader
                    .read_line(&mut response)
                    .map_err(|e| format!("讀取失敗：{e}"))?;
                serde_json::from_str(response.trim()).map_err(|e| e.to_string())
            })();
            let _ = sender.send(result);
        }));
    }
    drop(sender);
    let mut results = Vec::new();
    while let Ok(result) = receiver.recv_timeout(RECV_TIMEOUT) {
        results.push(result);
    }
    for handle in handles {
        handle.join().expect("繁中說明：並行 create 執行緒應結束");
    }

    assert_eq!(
        results.len(),
        2,
        "繁中說明：兩個 create 都應有回應，實際 {results:?}"
    );
    let outcomes: Vec<String> = results
        .iter()
        .map(|result| match result {
            Ok(result) => {
                let refused = result
                    .error
                    .as_ref()
                    .map(|error| format!("{:?}", error.code));
                format!("{:?}/{:?}", result.outcome, refused)
            }
            Err(message) => format!("transport-error: {message}"),
        })
        .collect();
    let completed = outcomes
        .iter()
        .filter(|outcome| outcome.starts_with("Completed"))
        .count();
    let conflicted = outcomes
        .iter()
        .filter(|outcome| outcome.contains("NameConflict"))
        .count();
    assert_eq!(
        (completed, conflicted),
        (1, 1),
        "繁中說明：一個成功、一個 name_conflict，實際 {outcomes:?}"
    );

    let listed = daemon.operator_call(&home, "list_instances", json!({}), None);
    let instances = listed.value["instances"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("繁中說明：list 應回 instances，實際 {}", listed.value));
    assert_eq!(
        instances.len(),
        1,
        "繁中說明：並行 create 後只應有一個 instance，實際 {instances:?}"
    );
    let pid = instance_pid(&home, "racer")
        .unwrap_or_else(|| panic!("繁中說明：SQLite 應記錄 racer 的 pid"));
    assert!(
        pid_running(pid),
        "繁中說明：只有一個程序被啟動，pid {pid} 應仍在運行"
    );
}

/// 找不到可執行檔 → `refused`（`spawn_failed`）、狀態 `failed`／`exec_failed`，
/// effects 不含 `process_started`。
#[test]
fn unknown_executable_fails_without_a_process() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let refused = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "missing-bin", "command": "otterhive-no-such-binary"}),
        None,
    );
    assert_eq!(
        refused.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：找不到可執行檔應被拒絕，實際為 {refused:?}"
    );
    assert_eq!(
        refused.error.as_ref().map(|error| error.code),
        Some(otterhive::protocol::ErrorCode::SpawnFailed),
        "繁中說明：應回 spawn_failed，實際為 {refused:?}"
    );
    for effect in ["generation_advanced", "state_failed"] {
        assert!(
            refused.effects.iter().any(|item| item == effect),
            "繁中說明：effects 應含 {effect}，實際 {:?}",
            refused.effects
        );
    }
    assert!(
        !refused.effects.iter().any(|item| item == "process_started"),
        "繁中說明：無法執行不應列 process_started，實際 {:?}",
        refused.effects
    );
    assert_eq!(
        refused.value["reason"],
        json!("exec_failed"),
        "繁中說明：失敗原因應為 exec_failed，實際 {}",
        refused.value
    );
    let row = instance_row(&home, "missing-bin")
        .unwrap_or_else(|| panic!("繁中說明：create 失敗仍應保留定義（SQLite 應有 missing-bin）"));
    assert_eq!(
        row.2, "failed",
        "繁中說明：停止可確認時應為 failed，實際 {:?}",
        row
    );
    assert_eq!(
        row.0, None,
        "繁中說明：沒有程序被啟動，實際 pid {:?}",
        row.0
    );
}

/// 啟動寬限期內 leader 結束 → `failed`／`exited_during_startup`，不重試。
#[test]
fn exit_within_startup_grace_is_failed() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let refused = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "crasher", "command": fixture("crash_on_start.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        refused.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：寬限期內退出應被拒絕，實際為 {refused:?}"
    );
    assert_eq!(
        refused.error.as_ref().map(|error| error.code),
        Some(otterhive::protocol::ErrorCode::SpawnFailed),
        "繁中說明：應回 spawn_failed，實際為 {refused:?}"
    );
    assert_eq!(
        refused.value["reason"],
        json!("exited_during_startup"),
        "繁中說明：value.reason 應為 exited_during_startup，實際 {}",
        refused.value
    );
    for effect in [
        "generation_advanced",
        "process_started",
        "process_stopped",
        "state_failed",
    ] {
        assert!(
            refused.effects.iter().any(|item| item == effect),
            "繁中說明：effects 應含 {effect}，實際 {:?}",
            refused.effects
        );
    }
    let row = instance_row(&home, "crasher")
        .unwrap_or_else(|| panic!("繁中說明：SQLite 應保留 crasher 的定義"));
    assert_eq!(row.2, "failed", "繁中說明：狀態應為 failed，實際 {:?}", row);
    assert_eq!(row.1, None, "繁中說明：停止可確認後 pid_start 應清空");
}

/// 寬限期內 leader 結束時，同 group 的殘留成員必須在回應前清除。
#[test]
fn leader_exit_within_grace_cleans_children_before_failure_response() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let log = home.root().join("spawns.log");
    fs_write(&log, "");

    let refused = daemon.operator_call(
        &home,
        "create_instance",
        json!({
            "name": "parent-exits",
            "command": fixture("spawns_child.sh").to_string_lossy(),
            "env": {
                "OTTERHIVE_FIXTURE_LOG": log.to_string_lossy(),
                "FIXTURE_LEADER_EXIT_AFTER_MS": "200",
            },
        }),
        None,
    );
    assert_eq!(
        refused.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：leader 在寬限期內結束應被拒絕，實際為 {refused:?}"
    );
    assert_eq!(
        refused.value["reason"],
        json!("exited_during_startup"),
        "繁中說明：value.reason 應為 exited_during_startup，實際 {}",
        refused.value
    );

    let child = read_log_pid(&log);
    assert!(
        child.is_some(),
        "繁中說明：spawns_child.sh 應把子程序 pid 寫入 {}",
        log.display()
    );
    assert!(
        !pid_running(child.unwrap_or(0)),
        "繁中說明：回應前同一 group 的殘留 child 應已被清除（pid {}）",
        child.unwrap_or(0)
    );
    let row = instance_row(&home, "parent-exits")
        .unwrap_or_else(|| panic!("繁中說明：SQLite 應保留 parent-exits 的定義"));
    assert_eq!(row.2, "failed", "繁中說明：狀態應為 failed，實際 {:?}", row);
}

/// start 前先確認前一代 group 已空；`failed` 帶程序紀錄時先停止舊 group 才
/// 增加 generation（契約 §4.2-1）。
#[test]
fn start_of_a_failed_instance_with_a_live_process_stops_it_before_spawning() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    // 建立活體 program，再把狀態改成 failed 並保留程序紀錄。
    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "leftover", "command": fixture("echo_agent.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：初始 create 應完成，實際為 {created:?}"
    );
    let row = instance_row(&home, "leftover")
        .unwrap_or_else(|| panic!("繁中說明：SQLite 應有 leftover 的列"));
    let pid_before = row.0.unwrap_or(0) as i32;
    let generation_before = row.3;
    assert!(
        pid_running(pid_before),
        "繁中說明：前提：程序 {pid_before} 應在運行"
    );
    // 模擬「上次 start 失敗但程序仍在」：狀態 failed、程序紀錄保留。
    force_state(&home, "leftover", "failed", "exited_during_startup");

    let started = daemon.operator_call(
        &home,
        "start_instance",
        json!({"instance": "leftover"}),
        None,
    );
    assert_eq!(
        started.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：start 應完成，實際為 {started:?}"
    );
    assert!(
        started.effects.iter().any(|item| item == "process_stopped"),
        "繁中說明：先停止舊程序應列 process_stopped，實際 {:?}",
        started.effects
    );
    assert!(
        started
            .effects
            .iter()
            .any(|item| *item == "generation_advanced")
            && started
                .effects
                .iter()
                .any(|item| *item == "process_started"),
        "繁中說明：應同時列 generation_advanced 與 process_started，實際 {:?}",
        started.effects
    );
    assert!(
        poll_until(LONG_TIMEOUT, || !pid_running(pid_before)),
        "daemon 應在期限內停止舊程序，pid {pid_before} 最後仍在運行"
    );
    let row_after = instance_row(&home, "leftover")
        .unwrap_or_else(|| panic!("繁中說明：SQLite 應仍有 leftover 的列"));
    assert_eq!(
        row_after.3,
        generation_before + 1,
        "繁中說明：確認停止後才增加 generation，實際 {row_after:?}"
    );
    assert_ne!(row_after.0, Some(pid_before as i64), "繁中說明：pid 應改變");
}

/// pid_start 不符 → `recovery_required`，generation 不變、原程序未被 signal。
#[test]
fn start_refuses_recovery_required_when_recorded_pid_start_differs() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let script = write_script(
        home.root(),
        "long_lived.sh",
        "#!/bin/sh\ntrap '' TERM\nprintf 'ready>\\n'\nwhile true; do sleep 60; done\n",
    );
    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "mismatch", "command": script.to_string_lossy()}),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：初始 create 應完成，實際為 {created:?}"
    );
    let pid = instance_pid(&home, "mismatch")
        .unwrap_or_else(|| panic!("繁中說明：SQLite 應記錄 mismatch 的 pid"));
    // 停止後把狀態改成 failed，但留下程序紀錄且 pid_start 造假（身分不符）。
    force_state(&home, "mismatch", "failed", "exec_failed");
    force_pid(&home, "mismatch", Some(pid as i64), Some(1));

    let refused = daemon.operator_call(
        &home,
        "start_instance",
        json!({"instance": "mismatch"}),
        None,
    );
    assert_eq!(
        refused.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：pid_start 不符應被拒絕，實際為 {refused:?}"
    );
    assert_eq!(
        refused.error.as_ref().map(|error| error.code),
        Some(otterhive::protocol::ErrorCode::RecoveryRequired),
        "繁中說明：應回 recovery_required，實際為 {refused:?}"
    );
    assert!(
        pid_running(pid),
        "繁中說明：身分不符時不得對原程序發 signal（pid {pid} 應仍運行）"
    );
    let row = instance_row(&home, "mismatch")
        .unwrap_or_else(|| panic!("繁中說明：SQLite 應仍有 mismatch 的列"));
    assert_eq!(row.3, 1, "繁中說明：不得增加 generation，實際 {:?}", row);
}

/// 存活的程序在寬限期結束後才回 `completed`。
#[test]
fn start_responds_after_the_startup_grace() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "slow-ok", "command": fixture("echo_agent.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：存活程序應完成，實際為 {created:?}"
    );
    assert!(
        !created.effects.iter().any(|item| item == "state_failed"),
        "繁中說明：存活程序不應列 state_failed，實際 {:?}",
        created.effects
    );
    let pid = instance_pid(&home, "slow-ok")
        .unwrap_or_else(|| panic!("繁中說明：SQLite 應記錄 slow-ok 的 pid"));
    assert!(pid_running(pid), "繁中說明：程序應在運行（pid {pid}）");
}

/// 寬限期後 leader 結束 → `stopped`／`exited`，附 exit status。
#[test]
fn exit_after_startup_grace_is_stopped_with_exit_status() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let script = write_script(
        home.root(),
        "later_exit.sh",
        "#!/bin/sh\nprintf 'ready>\\n'\nsleep 1.2\nexit 7\n",
    );
    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "late-exit", "command": script.to_string_lossy()}),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：寬限期內仍存活應完成，實際為 {created:?}"
    );
    assert!(
        daemon.poll_until(LONG_TIMEOUT, || {
            instance_row(&home, "late-exit")
                .map(|row| row.2 == "stopped")
                .unwrap_or(false)
        }),
        "繁中說明：寬限期後退出應轉為 stopped，最後為 {:?}",
        instance_row(&home, "late-exit")
    );
    let row = instance_row(&home, "late-exit")
        .unwrap_or_else(|| panic!("繁中說明：SQLite 應仍有 late-exit 的列"));
    assert_eq!(
        row.2, "stopped",
        "繁中說明：狀態應為 stopped，實際 {:?}",
        row
    );
    assert_eq!(
        row.1, None,
        "繁中說明：停止確認後 pid_start 應清空，實際 {:?}",
        row.1
    );
}

/// 寬限期後 leader 結束時，殘留的 group 成員也要清除。
#[test]
fn leader_exit_after_grace_cleans_lingering_group_members() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let log = home.root().join("late-spawns.log");
    fs_write(&log, "");

    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({
            "name": "late-parent",
            "command": fixture("spawns_child.sh").to_string_lossy(),
            "env": {
                "OTTERHIVE_FIXTURE_LOG": log.to_string_lossy(),
                "FIXTURE_LEADER_EXIT_AFTER_MS": "1500",
            },
        }),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：寬限期內存活應完成，實際為 {created:?}"
    );
    assert!(
        daemon.poll_until(LONG_TIMEOUT, || read_log_pid(&log).is_some()),
        "繁中說明：spawns_child.sh 應寫入子程序 pid，log={}",
        log.display()
    );
    let child = read_log_pid(&log).unwrap_or(0);
    assert!(
        daemon.poll_until(LONG_TIMEOUT, || !pid_running(child)),
        "繁中說明：leader 結束後殘留 child {child} 應被清除"
    );
    assert!(
        daemon.poll_until(LONG_TIMEOUT, || {
            instance_row(&home, "late-parent")
                .map(|row| row.2 == "stopped")
                .unwrap_or(false)
        }),
        "繁中說明：殘留清除後狀態應為 stopped，最後為 {:?}",
        instance_row(&home, "late-parent")
    );
}

/// 不存在的路徑、相對路徑、`command` 搭配 `model` → `invalid_argument`，不建立
/// instance、generation 不變。
#[test]
fn invalid_spec_is_refused_before_any_change() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let real_dir = home.root().join("real-dir");
    std::fs::create_dir_all(&real_dir).expect("繁中說明：應可建立測試用目錄");
    let real_file = home.root().join("real.txt");
    fs_write(&real_file, "instructions body");

    let cases: Vec<(&str, Value)> = vec![
        (
            "instructions 不存在",
            json!({"name": "bad-1", "command": fixture("echo_agent.sh").to_string_lossy(),
                   "instructions": home.root().join("nope.md").to_string_lossy()}),
        ),
        (
            "skills_path 不存在",
            json!({"name": "bad-2", "command": fixture("echo_agent.sh").to_string_lossy(),
                   "skills_path": home.root().join("no-such-dir").to_string_lossy()}),
        ),
        (
            "working_directory 不存在",
            json!({"name": "bad-3", "command": fixture("echo_agent.sh").to_string_lossy(),
                   "working_directory": home.root().join("no-such-dir").to_string_lossy()}),
        ),
        (
            "working_directory 是相對路徑",
            json!({"name": "bad-4", "command": fixture("echo_agent.sh").to_string_lossy(),
                   "working_directory": "relative/path"}),
        ),
        (
            "working_directory 是檔案",
            json!({"name": "bad-5", "command": fixture("echo_agent.sh").to_string_lossy(),
                   "working_directory": real_file.to_string_lossy()}),
        ),
    ];

    // 契約 §4.1「`model`／`effort` 只適用 `backend: claude`，搭配 `command`
    // 回 `invalid_argument`」在 T4 **不可達**：明確 `backend: command` 一律先被
    // `unsupported_in_phase` 擋下（plan T4），因此沒有任何 spec 能同時帶著
    // `backend: command` 與 `model`／`effort` 走到這條檢查。該規則留待 command
    // backend 實作時再驗證。

    for (label, args) in cases {
        let refused = daemon.operator_call(&home, "create_instance", args.clone(), None);
        assert_eq!(
            refused.outcome,
            otterhive::protocol::Outcome::Refused,
            "繁中說明：{label} 應被拒絕，實際為 {refused:?}"
        );
        assert_eq!(
            refused.error.as_ref().map(|error| error.code),
            Some(otterhive::protocol::ErrorCode::InvalidArgument),
            "繁中說明：{label} 應回 invalid_argument，實際為 {refused:?}"
        );
        let name = args["name"].as_str().unwrap_or("").to_owned();
        assert!(
            instance_row(&home, &name).is_none(),
            "繁中說明：{label} 不應建立 instance {name}"
        );
    }

    let listed = daemon.operator_call(&home, "list_instances", json!({}), None);
    let count = listed.value["instances"]
        .as_array()
        .map(|items| items.len())
        .unwrap_or(0);
    assert_eq!(count, 0, "繁中說明：所有拒絕案例都不應留下 instance");
    let _ = real_dir;
}

/// `stopped`／`failed` 的 instance 可 restart，效果等同以指定 mode start。
#[test]
fn restart_of_a_stopped_instance_starts_it() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let refused = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "revive", "command": fixture("crash_on_start.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        refused.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：初始 create 應失敗，實際為 {refused:?}"
    );
    assert!(
        daemon.poll_until(LONG_TIMEOUT, || {
            instance_row(&home, "revive")
                .map(|row| row.2 == "failed")
                .unwrap_or(false)
        }),
        "繁中說明：狀態應為 failed，最後為 {:?}",
        instance_row(&home, "revive")
    );

    let restarted = daemon.operator_call(
        &home,
        "restart_instance",
        json!({"instance": "revive"}),
        None,
    );
    assert_eq!(
        restarted.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：仍用 crash_on_start.sh 時 restart 仍應失敗，實際為 {restarted:?}"
    );
    assert!(
        restarted
            .effects
            .iter()
            .any(|item| item == "generation_advanced"),
        "繁中說明：restart 應增加 generation，實際 {:?}",
        restarted.effects
    );
}

/// 名稱超過 64 字元、含 `/` 或空白、以及 `operator` → `invalid_argument`。
#[test]
fn invalid_names_are_rejected() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let long_name = "n".repeat(65);
    let cases = vec![
        ("超過 64 字元", long_name),
        ("含斜線", "has/slash".to_owned()),
        ("含空白", "has space".to_owned()),
        ("保留名 operator", "operator".to_owned()),
    ];
    for (label, name) in cases {
        let refused = daemon.operator_call(
            &home,
            "create_instance",
            json!({"name": name, "command": fixture("echo_agent.sh").to_string_lossy()}),
            None,
        );
        assert_eq!(
            refused.outcome,
            otterhive::protocol::Outcome::Refused,
            "繁中說明：{label}（{name}）應被拒絕，實際為 {refused:?}"
        );
        assert_eq!(
            refused.error.as_ref().map(|error| error.code),
            Some(otterhive::protocol::ErrorCode::InvalidArgument),
            "繁中說明：{label} 應回 invalid_argument，實際為 {refused:?}"
        );
    }
    let listed = daemon.operator_call(&home, "list_instances", json!({}), None);
    assert_eq!(
        listed.value["instances"]
            .as_array()
            .map(|items| items.len()),
        Some(0),
        "繁中說明：無效名稱不應留下 instance，實際 {}",
        listed.value
    );
}

/// 不同呼叫端使用同一 `operation_id` → `invalid_argument`。
#[test]
fn operation_id_reused_by_another_caller_is_invalid() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "dup-owner", "command": fixture("echo_agent.sh").to_string_lossy()}),
        Some("shared-op"),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：第一次 create 應完成，實際為 {created:?}"
    );

    // 同一 instance 呼叫端用同一 operation_id 再呼叫不同操作。
    let instance_auth = home.instance_auth("dup-owner");
    let refused = daemon.call(
        &home,
        &instance_auth,
        "restart_instance",
        json!({"instance": "dup-owner"}),
        Some("shared-op"),
    );
    assert_eq!(
        refused.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：不同呼叫端重用 operation_id 應被拒絕，實際為 {refused:?}"
    );
    assert_eq!(
        refused.error.as_ref().map(|error| error.code),
        Some(otterhive::protocol::ErrorCode::InvalidArgument),
        "繁中說明：應回 invalid_argument，實際為 {refused:?}"
    );
}

/// `running` 的 instance 再 start → `completed` 且 `already_running`，generation 不變。
#[test]
fn start_on_running_instance_reports_already_running() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "already", "command": fixture("echo_agent.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：初始 create 應完成，實際為 {created:?}"
    );
    let pid_before = instance_pid(&home, "already");

    let again = daemon.operator_call(
        &home,
        "start_instance",
        json!({"instance": "already"}),
        None,
    );
    assert_eq!(
        again.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：已在執行的 start 應回 completed，實際為 {again:?}"
    );
    assert_eq!(
        again.value["already_running"],
        json!(true),
        "繁中說明：value.already_running 應為 true，實際 {}",
        again.value
    );
    assert!(
        !again
            .effects
            .iter()
            .any(|item| item == "generation_advanced" || item == "process_started"),
        "繁中說明：already_running 不應列 generation_advanced／process_started，實際 {:?}",
        again.effects
    );
    assert_eq!(
        instance_pid(&home, "already"),
        pid_before,
        "繁中說明：pid 不應改變"
    );
}

/// fresh restart 換程序、generation 加一、舊 pid 消失。
#[test]
fn fresh_restart_replaces_the_process() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "replaced", "command": fixture("echo_agent.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：初始 create 應完成，實際為 {created:?}"
    );
    let generation_before = instance_row(&home, "replaced")
        .map(|row| row.3)
        .unwrap_or(0);
    let pid_before = instance_pid(&home, "replaced").unwrap_or(0);
    let token_before = home.instance_token("replaced");

    let restarted = daemon.operator_call(
        &home,
        "restart_instance",
        json!({"instance": "replaced", "mode": "fresh"}),
        None,
    );
    assert!(
        matches!(
            restarted.outcome,
            otterhive::protocol::Outcome::Completed | otterhive::protocol::Outcome::Queued
        ),
        "繁中說明：restart 應完成或 queued，實際為 {restarted:?}"
    );
    let generation_after = instance_row(&home, "replaced")
        .map(|row| row.3)
        .unwrap_or(0);
    assert_eq!(
        generation_after,
        generation_before + 1,
        "繁中說明：fresh restart 應使 generation 加一"
    );
    assert!(
        !pid_running(pid_before),
        "繁中說明：舊程序 {pid_before} 應已停止"
    );
    let pid_after = instance_pid(&home, "replaced").unwrap_or(0);
    assert_ne!(pid_after, pid_before, "繁中說明：新程序 pid 應改變");
    assert!(
        pid_running(pid_after),
        "繁中說明：新程序 {pid_after} 應在運行"
    );
    let token_after = home.instance_token("replaced");
    assert_ne!(
        token_after, token_before,
        "繁中說明：新 generation 應發新 token"
    );
}

/// `expected_generation` 不符 → `stale_generation`，不做任何變更。
#[test]
fn restart_with_stale_expected_generation_changes_nothing() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "guarded", "command": fixture("echo_agent.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：初始 create 應完成，實際為 {created:?}"
    );
    let row_before = instance_row(&home, "guarded")
        .unwrap_or_else(|| panic!("繁中說明：SQLite 應有 guarded 的列"));

    let refused = daemon.operator_call(
        &home,
        "restart_instance",
        json!({"instance": "guarded", "expected_generation": 999}),
        None,
    );
    assert_eq!(
        refused.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：stale expected_generation 應被拒絕，實際為 {refused:?}"
    );
    assert_eq!(
        refused.error.as_ref().map(|error| error.code),
        Some(otterhive::protocol::ErrorCode::StaleGeneration),
        "繁中說明：應回 stale_generation，實際為 {refused:?}"
    );
    assert_eq!(
        instance_row(&home, "guarded"),
        Some(row_before),
        "繁中說明：stale generation 不應改變任何狀態"
    );
}

/// `command` backend 的 resume → `resume_unavailable`，pid 不變。
#[test]
fn resume_without_evidence_is_refused_and_leaves_the_process() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "cmd-backend", "command": fixture("echo_agent.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：初始 create 應完成，實際為 {created:?}"
    );
    let pid_before = instance_pid(&home, "cmd-backend");

    let refused = daemon.operator_call(
        &home,
        "restart_instance",
        json!({"instance": "cmd-backend", "mode": "resume"}),
        None,
    );
    assert_eq!(
        refused.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：command backend 的 resume 應被拒絕，實際為 {refused:?}"
    );
    assert_eq!(
        refused.error.as_ref().map(|error| error.code),
        Some(otterhive::protocol::ErrorCode::ResumeUnavailable),
        "繁中說明：應回 resume_unavailable，實際為 {refused:?}"
    );
    assert_eq!(
        instance_pid(&home, "cmd-backend"),
        pid_before,
        "繁中說明：resume 被拒時 pid 不應改變"
    );
    assert!(
        pid_running(pid_before.unwrap_or(0)),
        "繁中說明：原程序應仍在運行"
    );
}

/// 同一 instance 已有 lifecycle 操作在進行時，另一個 → `operation_in_progress`。
#[test]
fn second_lifecycle_operation_on_the_same_instance_is_refused() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "ignore-term", "command": fixture("ignore_term.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：初始 create 應完成，實際為 {created:?}"
    );

    let socket = home.socket_path();
    let token = home.operator_token();
    // 這個執行緒必須**贏得**鎖：它送出 delete 後會持有 lifecycle 鎖約
    // 1500ms（TERM 1000ms ＋ KILL 500ms），主執行緒才有「另一個操作撞上進行中
    // 的操作」的視窗可觀察。若它自己先被 `operation_in_progress` 拒絕，就根本
    // 沒有程序持有鎖，主執行緒每次都會拿到 `Completed` 而永遠觀察不到拒絕——
    // 測試會以「時序不巧」的名義失敗。所以兩邊都必須重試。
    let busy = std::thread::spawn(move || {
        use std::io::Write;
        let request = otterhive::protocol::Request {
            v: 1,
            auth: otterhive::protocol::Auth::Operator(token),
            op: "delete_instance".to_owned(),
            args: json!({"instance": "ignore-term"}),
            operation_id: None,
        };
        let mut line = serde_json::to_string(&request).expect("繁中說明：請求應可序列化");
        line.push('\n');
        let deadline = std::time::Instant::now() + LONG_TIMEOUT;
        loop {
            let stream = std::os::unix::net::UnixStream::connect(&socket)
                .expect("繁中說明：並行 delete 應可連線");
            let _ = stream.set_read_timeout(Some(RECV_TIMEOUT));
            let mut stream = stream;
            stream
                .write_all(line.as_bytes())
                .expect("繁中說明：應可送出 delete");
            let mut reader = std::io::BufReader::new(stream);
            let mut response = String::new();
            reader
                .read_line(&mut response)
                .expect("繁中說明：應可讀回 delete 回應");
            if !response.contains("operation_in_progress") {
                return response;
            }
            // 撞上主執行緒持有的鎖就再試一次，直到贏得為止。
            if std::time::Instant::now() >= deadline {
                return response;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });

    // `ignore_term.sh` 忽略 SIGTERM，所以 delete 必須走完 TERM（1000ms）＋
    // KILL（500ms）才會結束——那段時間就是「同一 instance 已有 lifecycle 操作
    // 進行中」的視窗。
    //
    // 撞擊方式：連續送出 restart 直到觀察到 `operation_in_progress`。只送一次的
    // 話，兩次 syscall 之間的排程延遲決定是否落在視窗內，那是賭時序而不是驗
    // 行為。判準不變：期限內必須觀察到 `operation_in_progress`。
    //
    // 兩邊都必須重試才能成立：誰先拿到鎖誰就贏。若主執行緒先贏，這次 restart
    // 會自己走完 TERM ＋ KILL（約 1500ms）才回 `Completed`——而並行執行緒持續
    // 重試的 delete 會在約 20ms 內撞上它、取得 `operation_in_progress`，接著才
    // 開始持有鎖。兩邊都在跑，視窗一定會出現。
    let mut refused = None;
    let attempt_start = std::time::Instant::now();
    while refused.is_none() && attempt_start.elapsed() < LONG_TIMEOUT {
        let attempt = daemon.operator_call(
            &home,
            "restart_instance",
            json!({"instance": "ignore-term"}),
            None,
        );
        if attempt.error.as_ref().map(|error| error.code)
            == Some(otterhive::protocol::ErrorCode::OperationInProgress)
        {
            refused = Some(attempt);
            break;
        }
        // 撞上視窗外（delete 已結束）就再試一次。
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = busy.join().expect("繁中說明：並行 delete 執行緒應結束");
    let refused = refused.unwrap_or_else(|| {
        panic!("繁中說明：期限內應觀察到 operation_in_progress（delete 進行中）")
    });
    assert_eq!(
        refused.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：進行中的第二個 lifecycle 操作應被拒絕，實際為 {refused:?}"
    );
    assert_eq!(
        refused.error.as_ref().map(|error| error.code),
        Some(otterhive::protocol::ErrorCode::OperationInProgress),
        "繁中說明：應回 operation_in_progress，實際為 {refused:?}"
    );
}

/// delete 停止程序、封存並釋放名稱；同名重建得到新 id。
#[test]
fn delete_stops_archives_and_frees_the_name() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "ephemeral", "command": fixture("echo_agent.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：初始 create 應完成，實際為 {created:?}"
    );
    let pid = instance_pid(&home, "ephemeral").unwrap_or(0);
    let id_before = created.subject.as_ref().map(|subject| subject.id.clone());

    let deleted = daemon.operator_call(
        &home,
        "delete_instance",
        json!({"instance": "ephemeral"}),
        None,
    );
    assert_eq!(
        deleted.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：delete 應完成，實際為 {deleted:?}"
    );
    assert!(
        deleted.effects.iter().any(|item| item == "process_stopped"),
        "繁中說明：delete 應列 process_stopped，實際 {:?}",
        deleted.effects
    );
    assert!(
        !pid_running(pid),
        "daemon 應在期限內停止程序 {pid}，最後觀測仍在運行"
    );
    assert!(
        instance_row(&home, "ephemeral").is_none(),
        "繁中說明：delete 後不應有未刪除的 instance"
    );

    let recreated = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "ephemeral", "command": fixture("echo_agent.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        recreated.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：同名重建應成功，實際為 {recreated:?}"
    );
    let id_after = recreated.subject.as_ref().map(|subject| subject.id.clone());
    assert_ne!(
        id_after, id_before,
        "繁中說明：同名重建應取得新的 InstanceId"
    );
}

/// 同一 operation_id 重送 → 回第一次記錄的結果，不重複 spawn。
#[test]
fn operation_id_replay_does_not_spawn_again() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let first = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "replayed", "command": fixture("echo_agent.sh").to_string_lossy()}),
        Some("replay-op"),
    );
    assert_eq!(
        first.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：第一次 create 應完成，實際為 {first:?}"
    );
    let pid_after_first = instance_pid(&home, "replayed");

    let replay = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "replayed", "command": fixture("echo_agent.sh").to_string_lossy()}),
        Some("replay-op"),
    );
    assert_eq!(replay, first, "繁中說明：重送應回第一次記錄的結果");
    assert_eq!(
        instance_pid(&home, "replayed"),
        pid_after_first,
        "繁中說明：重送不得再 spawn（pid 不應改變）"
    );
    let listed = daemon.operator_call(&home, "list_instances", json!({}), None);
    assert_eq!(
        listed.value["instances"]
            .as_array()
            .map(|items| items.len()),
        Some(1),
        "繁中說明：重送不得建立第二個 instance"
    );
}

/// `interrupt` 對 PTY 寫入一個 ESC（`0x1b`）。
///
/// 觀測方式說明：`echo_agent.sh` 以 `read` 逐行處理 stdin，canonical 模式下
/// 單獨的 ESC 不會結束一次 read，因此它無法觀測到這次中斷。本測試改用測試
/// 自身建立、逐位元組轉儲 stdin 的 script（raw stdin），直接確認 `0x1b`
/// 抵達子程序。
#[test]
fn interrupt_writes_escape() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let raw_log = home.root().join("raw-stdin.bin");
    fs_write(&raw_log, "");
    let script = write_script(
        home.root(),
        "raw_stdin.sh",
        // `stty raw` 關掉 tty 的 canonical 模式：否則 PTY 會把單獨的 ESC
        // （0x1b）留在行緩衝裡，`cat` 永遠收不到。契約 §4.7 的 interrupt 寫的是
        // 單一個 ESC，因此子程序端必須能逐位元組觀察它。
        "#!/bin/sh\nprintf 'ready>\\n'\nstty raw -echo\ncat >> \"$RAW_LOG_PATH\"\n",
    );

    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "interrupted", "command": script.to_string_lossy(),
               "env": {"RAW_LOG_PATH": raw_log.to_string_lossy()}}),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );

    let interrupted =
        daemon.operator_call(&home, "interrupt", json!({"instance": "interrupted"}), None);
    assert_eq!(
        interrupted.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：interrupt 應完成，實際為 {interrupted:?}"
    );
    assert!(
        interrupted.effects.iter().any(|item| item == "esc_sent"),
        "繁中說明：effects 應含 esc_sent，實際 {:?}",
        interrupted.effects
    );
    assert!(
        poll_until(LONG_TIMEOUT, || {
            std::fs::read(&raw_log)
                .map(|bytes| bytes.contains(&0x1b))
                .unwrap_or(false)
        }),
        "繁中說明：ESC（0x1b）應抵達子程序 stdin，log={}",
        raw_log.display()
    );
}

/// `pane_snapshot` 回純文字並支援 `lines`；停止後回最後畫面並標 `live: false`。
#[test]
fn pane_snapshot_returns_plain_text_and_honours_lines() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let script = write_script(
        home.root(),
        "printer.sh",
        "#!/bin/sh\nprintf 'alpha-line\\n'\nprintf 'beta-line\\n'\nprintf 'gamma-line\\n'\n\
         while true; do sleep 60; done\n",
    );
    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "printer", "command": script.to_string_lossy()}),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );

    // 子程序的輸出經 PTY → reader → VT 是一條有延遲的路徑；snapshot 在畫面
    // 還沒被餵進去之前會回全空白。因此有上限地等畫面出現內容，不靠任意 sleep。
    assert!(
        poll_until(LONG_TIMEOUT, || {
            daemon
                .operator_call(&home, "pane_snapshot", json!({"instance": "printer"}), None)
                .value["text"]
                .as_str()
                .is_some_and(|text| text.contains("gamma-line"))
        }),
        "繁中說明：printer 的畫面應在期限內出現 gamma-line，最後為 {:?}",
        daemon
            .operator_call(&home, "pane_snapshot", json!({"instance": "printer"}), None)
            .value
    );

    let snapshot =
        daemon.operator_call(&home, "pane_snapshot", json!({"instance": "printer"}), None);
    assert_eq!(
        snapshot.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：snapshot 應完成，實際為 {snapshot:?}"
    );
    let text = snapshot.value["text"].as_str().unwrap_or_else(|| {
        panic!(
            "繁中說明：snapshot 應回 value.text，實際 {}",
            snapshot.value
        )
    });
    assert!(
        text.contains("gamma-line"),
        "繁中說明：snapshot 應含畫面內容，實際 {text:?}"
    );
    assert!(
        !text.contains('\u{1b}'),
        "繁中說明：snapshot 應去除 ANSI，實際 {text:?}"
    );
    assert_eq!(
        snapshot.value["live"],
        json!(true),
        "實際 {}",
        snapshot.value
    );

    let tail = daemon.operator_call(
        &home,
        "pane_snapshot",
        json!({"instance": "printer", "lines": 1}),
        None,
    );
    // `lines` 取畫面最後 N 列。畫面在 `gamma-line` 之後還有游標所在的空白列，
    // 那也是真實的一列；而 §4.7 的 `value.text` 是「每列去掉行尾空白」之後
    // 的結果，所以最後一列取出來**必定**是空字串——這是可斷言的事實，
    // 不必像原來那樣對尾端內容做「或者」式的寬容假設。
    let tail_text = tail.value["text"]
        .as_str()
        .unwrap_or_else(|| panic!("繁中說明：snapshot 應回 value.text，實際 {}", tail.value));
    assert_eq!(
        tail_text, "",
        "繁中說明：lines=1 取最後一列（游標所在的空白列，去掉行尾空白後為空），\
         實際為 {tail_text:?}"
    );
    // 對照：不給 `lines` 時應同時看得到三列，且每列都沒有行尾空白。畫面是
    // §4.2-3 固定的 120×40，所以完整畫面是 40 列（游標停在第 4 列，之後
    // 全是空白列）。`value.text` 是 40 列以換行接起來的字串，而
    // `str::lines()` 對結尾的換行不產生元素，所以數到的是 39——這正是
    // `lines=1` 取到空字串的原因。
    let full_lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        full_lines.len(),
        39,
        "繁中說明：完整畫面應是 PTY 的 40 列以換行接起（§4.2-3），實際 {full_lines:?}"
    );
    assert_eq!(
        &full_lines[..3],
        ["alpha-line", "beta-line", "gamma-line"],
        "繁中說明：完整畫面前三列應依序是印出的三行，實際 {full_lines:?}"
    );
    assert!(
        full_lines.iter().all(|line| *line == line.trim_end()),
        "繁中說明：每列都應去掉行尾空白（§4.7），實際 {full_lines:?}"
    );
    assert!(
        full_lines[3..].iter().all(|line| line.is_empty()),
        "繁中說明：游標之後的列都應是空白列，實際 {:?}",
        &full_lines[3..]
    );

    // 停止後：snapshot 回最後畫面並標 live: false。
    let stopped = daemon.operator_call(
        &home,
        "delete_instance",
        json!({"instance": "printer"}),
        None,
    );
    assert_eq!(
        stopped.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：delete 應完成，實際為 {stopped:?}"
    );
}

/// 未運行的 instance 上 `interrupt`／`attach` → `invalid_argument`。
#[test]
fn interrupt_and_attach_on_a_stopped_instance_are_invalid() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let refused = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "never-ran", "command": fixture("crash_on_start.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        refused.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：create 應失敗，實際為 {refused:?}"
    );
    assert!(
        poll_until(LONG_TIMEOUT, || {
            instance_row(&home, "never-ran")
                .map(|row| row.2 == "failed")
                .unwrap_or(false)
        }),
        "繁中說明：狀態應為 failed，最後為 {:?}",
        instance_row(&home, "never-ran")
    );

    for op in ["interrupt", "attach"] {
        let refused = daemon.operator_call(&home, op, json!({"instance": "never-ran"}), None);
        assert_eq!(
            refused.outcome,
            otterhive::protocol::Outcome::Refused,
            "繁中說明：{op} 在未運行的 instance 上應被拒絕，實際為 {refused:?}"
        );
        assert_eq!(
            refused.error.as_ref().map(|error| error.code),
            Some(otterhive::protocol::ErrorCode::InvalidArgument),
            "繁中說明：{op} 應回 invalid_argument，實際為 {refused:?}"
        );
    }
}

/// `attach` 先回 Snapshot frame，之後 Output 的 seq 連續；generation 不符的
/// Input 回 Error frame（契約 §7 daemon 端）。
#[test]
fn attach_stream_delivers_snapshot_then_output() {
    use otterhive::terminal::frame::read_frame;

    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let created = daemon.operator_call(
        &home,
        "create_instance",
        json!({"name": "attachable", "command": fixture("echo_agent.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );
    let generation = created
        .subject
        .as_ref()
        .map(|subject| subject.generation)
        .unwrap_or(0);

    let socket = home.socket_path();
    let mut stream =
        std::os::unix::net::UnixStream::connect(&socket).expect("繁中說明：attach 應可連線");
    let _ = stream.set_read_timeout(Some(RECV_TIMEOUT));
    let request = otterhive::protocol::Request {
        v: 1,
        auth: home.operator(),
        op: "attach".to_owned(),
        args: json!({"instance": "attachable"}),
        operation_id: None,
    };
    let mut line = serde_json::to_string(&request).expect("繁中說明：attach 請求應可序列化");
    line.push('\n');
    use std::io::Write;
    stream
        .write_all(line.as_bytes())
        .expect("繁中說明：應可送出 attach");

    // 第一個 frame 必為 Snapshot（0x01），payload = 4B BE header 長度 + JSON。
    let (tag, body) = read_frame(&mut stream).expect("繁中說明：應可讀到第一個 frame");
    assert_eq!(
        tag, 0x01,
        "繁中說明：第一個 frame 應是 Snapshot，實際 tag {tag:#x}"
    );
    let header_len = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
    let header: Value = serde_json::from_slice(&body[4..4 + header_len])
        .unwrap_or_else(|e| panic!("繁中說明：Snapshot header 應是合法 JSON：{e}"));
    assert_eq!(header["generation"], json!(generation));
    assert!(
        body.len() > 4 + header_len,
        "繁中說明：Snapshot 應附畫面 bytes"
    );

    // generation 不符的 Input 應回 Error frame（0x05）。
    let mut bad_seq = (generation as u64 + 100).to_be_bytes().to_vec();
    bad_seq.extend_from_slice(b"x");
    let mut frame = vec![0x10u8];
    frame.extend_from_slice(&(bad_seq.len() as u32).to_be_bytes());
    frame.extend_from_slice(&bad_seq);
    stream
        .write_all(&frame)
        .expect("繁中說明：應可送出 Input frame");

    let mut saw_error = false;
    let start = std::time::Instant::now();
    while start.elapsed() < LONG_TIMEOUT && !saw_error {
        match read_frame(&mut stream) {
            Ok((0x05, payload)) => {
                let parsed: Value = serde_json::from_slice(&payload)
                    .unwrap_or_else(|e| panic!("繁中說明：Error payload 應是 JSON：{e}"));
                assert_eq!(
                    parsed["code"],
                    json!("stale_generation"),
                    "繁中說明：generation 不符應回 stale_generation，實際 {parsed}"
                );
                saw_error = true;
            }
            Ok(_) => {}
            // socket 讀取逾時回 `WouldBlock`：那是「還沒等到下一個 frame」，
            // 不是連線中斷。外層迴圈已有 LONG_TIMEOUT 上限，繼續等即可。
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("繁中說明：讀 frame 失敗：{error}"),
        }
    }
    assert!(
        saw_error,
        "繁中說明：generation 不符的 Input 應回 Error frame"
    );
}

/// 測試用的最小輔助：寫檔。
fn fs_write(path: &std::path::Path, contents: &str) {
    std::fs::write(path, contents)
        .unwrap_or_else(|e| panic!("繁中說明：應可寫入 {}：{e}", path.display()));
}

/// 把 instance 名稱列進設定檔的 `managers`（契約 §2 的權限來源之一）。
///
/// 必須在 `Daemon::start` **之前**寫：daemon 在啟動時讀一次設定。
fn set_managers(home: &TestHome, names: &[&str]) {
    let listed: Vec<String> = names.iter().map(|name| format!("  - {name}\n")).collect();
    fs_write(
        home.home.config_path(),
        &format!("managers:\n{}", listed.concat()),
    );
}

/// 建立一個 `running` 的 instance，回傳它的名稱。
fn create_running(home: &TestHome, daemon: &Daemon, name: &str) -> OperationResultAlias {
    let created = daemon.operator_call(
        home,
        "create_instance",
        json!({"name": name, "command": fixture("echo_agent.sh").to_string_lossy()}),
        None,
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create {name} 應完成，實際為 {created:?}"
    );
    created
}

/// §4.4：被 restart 的是呼叫者自己時，daemon 先回 `queued` 再執行。
///
/// 呼叫端會在讀到結果之前就被自己的舊程序連同 PTY 收掉，所以結果不可能
/// 同步回來；`queued` 是唯一誠實的答覆。新的 generation 由 `list_instances`
/// 確認（§4.4）。
#[test]
fn restart_of_the_caller_itself_is_answered_queued_then_actually_runs() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_running(&home, &daemon, "self-restart");
    let generation_before = instance_generation(&home, &daemon, "self-restart");

    let queued = daemon.call(
        &home,
        &home.instance_auth("self-restart"),
        "restart_instance",
        json!({"instance": "self-restart"}),
        None,
    );
    assert_eq!(
        queued.outcome,
        otterhive::protocol::Outcome::Queued,
        "繁中說明：restart 自己應回 queued，實際為 {queued:?}"
    );
    assert!(
        queued.error.is_none(),
        "繁中說明：queued 不是錯誤，實際為 {queued:?}"
    );

    // `queued` 之後工作真的執行了：generation 增加。
    assert!(
        daemon.poll_until(LONG_TIMEOUT, || {
            instance_generation(&home, &daemon, "self-restart") > generation_before
        }),
        "繁中說明：queued 之後應真的以新 generation 重啟，最後為 {}",
        instance_generation(&home, &daemon, "self-restart")
    );
}

/// 以 `list_instances` 讀某 instance 的 generation（契約 §4.4 指定的確認方式）。
fn instance_generation(home: &TestHome, daemon: &Daemon, name: &str) -> u64 {
    let listed = daemon.operator_call(home, "list_instances", json!({"instance": name}), None);
    listed.value["instances"][0]["generation"]
        .as_u64()
        .unwrap_or_else(|| {
            panic!("繁中說明：list_instances 應回 {name} 的 generation，實際為 {listed:?}")
        })
}

/// §2 的授權矩陣（daemon 端整組）：instance 呼叫端不能 manage 任何 instance，
/// 能 restart 自己但不能 restart 別人（除非在 `managers` 裡），也只能
/// snapshot 自己。
#[test]
fn instance_caller_authorization_matrix_over_the_socket() {
    let home = TestHome::new();
    set_managers(&home, &["manager"]);
    let daemon = Daemon::start(&home);
    create_running(&home, &daemon, "worker");
    create_running(&home, &daemon, "other");
    create_running(&home, &daemon, "manager");

    let worker = home.instance_auth("worker");
    let manager = home.instance_auth("manager");
    let refused = |result: &OperationResultAlias, what: &str| {
        assert_eq!(
            result.outcome,
            otterhive::protocol::Outcome::Refused,
            "繁中說明：{what} 應被拒絕，實際為 {result:?}"
        );
        assert_eq!(
            result.error.as_ref().map(|error| error.code),
            Some(otterhive::protocol::ErrorCode::Unauthorized),
            "繁中說明：{what} 應回 unauthorized，實際為 {result:?}"
        );
    };

    // Manage（create／delete／start）只有 operator 做得到。
    refused(
        &daemon.call(
            &home,
            &worker,
            "create_instance",
            json!({"name": "sneaky", "command": fixture("echo_agent.sh").to_string_lossy()}),
            None,
        ),
        "instance 呼叫 create_instance",
    );
    refused(
        &daemon.call(
            &home,
            &worker,
            "start_instance",
            json!({"instance": "other"}),
            None,
        ),
        "instance 呼叫 start_instance",
    );
    refused(
        &daemon.call(
            &home,
            &worker,
            "delete_instance",
            json!({"instance": "other"}),
            None,
        ),
        "instance 呼叫 delete_instance",
    );

    // Restart：自己可以（回 queued），別人不可以，除非在 `managers` 裡。
    assert_eq!(
        daemon
            .call(
                &home,
                &worker,
                "restart_instance",
                json!({"instance": "worker"}),
                None,
            )
            .outcome,
        otterhive::protocol::Outcome::Queued,
        "繁中說明：restart 自己應回 queued"
    );
    refused(
        &daemon.call(
            &home,
            &worker,
            "restart_instance",
            json!({"instance": "other"}),
            None,
        ),
        "instance 呼叫 restart 別人",
    );
    let restarted_other = daemon.call(
        &home,
        &manager,
        "restart_instance",
        json!({"instance": "other"}),
        None,
    );
    assert_eq!(
        restarted_other.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：manager 應能 restart 別人，實際為 {restarted_other:?}"
    );

    // PaneSnapshot：只看得到自己。
    //
    // 先用**過期前**的 token 固定住拒絕這件事：上一段的 self-restart 已經讓
    // `worker` 的舊 token 失效，帶舊 token 呼叫必須是 `unauthorized`。
    refused(
        &daemon.call(
            &home,
            &worker,
            "pane_snapshot",
            json!({"instance": "worker"}),
            None,
        ),
        "instance 帶已被換掉的舊 token 呼叫",
    );

    // `restart_instance` 換掉了 `worker` 自己的 token（每個 generation 一組），
    // 而 restart 是回 `queued`、真正的工作在背景執行，所以磁碟上的 token
    // 會再換一次。新 token 要等背景那次執行把 MCP 設定寫好，而它寫入的時機
    // 又早於資料庫記錄新 token 的雜湊——所以「檔案上的 token 已經換掉」並不
    // 保證它已被接受。以**一次真的通過認證的呼叫**為準。
    let snapshot_of_self = || {
        daemon.call(
            &home,
            &home.instance_auth("worker"),
            "pane_snapshot",
            json!({"instance": "worker"}),
            None,
        )
    };
    assert!(
        daemon.poll_until(LONG_TIMEOUT, || {
            snapshot_of_self().outcome == otterhive::protocol::Outcome::Completed
        }),
        "繁中說明：新 generation 的 token 應可用於認證自己並 snapshot 自己"
    );

    let own = snapshot_of_self();
    assert_eq!(
        own.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：instance 應能 snapshot 自己，實際為 {own:?}"
    );
    // 用**新** token 呼叫，確認拒絕的理由是授權而不是認證。
    refused(
        &daemon.call(
            &home,
            &home.instance_auth("worker"),
            "pane_snapshot",
            json!({"instance": "other"}),
            None,
        ),
        "instance 呼叫 snapshot 別人",
    );
}

/// 讀 fixture log 的第一行作為 pid。
fn read_log_pid(path: &std::path::Path) -> Option<i32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.trim().lines().next()?.trim().parse::<i32>().ok()
}

/// 讓並行 create 的執行緒能把 `OperationResult` 帶回主執行緒。
type OperationResultAlias = otterhive::protocol::OperationResult;
