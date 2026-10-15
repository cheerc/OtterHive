//! T4 Lane A RED：daemon 的啟動自檢與重啟收斂（契約 §1、§4.6）。
//!
//! 依 `docs/plans/p1a-implementation.md` T4。全檔只在 macOS 編譯；所有等待
//! 有上限並回報最後觀測狀態；使用暫存的 `HOME`／`OTTERHIVE_HOME`／cwd。
//!
//! 兩類驗收：
//! - 啟動自檢：bridge 必須與 `otterhive` 同目錄（§1）、同一 home 只允許一個
//!   daemon（第二個 exit 3 與 stderr 的 `daemon_already_running`）、壞設定檔
//!   拒絕啟動。
//! - 重啟收斂（§4.6）：daemon 重啟後對殘留程序依 §4.3 收斂，成功為
//!   `stopped`（原因 `daemon_restarted`），否則 `recovery_required` 保留。

#![cfg(target_os = "macos")]

use std::process::{Command, Stdio};

use serde_json::{json, Value};

mod support {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use otterhive::home::Home;
    use otterhive::protocol::Request;
    use serde_json::Value;
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
    }

    pub struct Daemon {
        child: Option<Child>,
    }

    impl Daemon {
        pub fn start(home: &TestHome) -> Self {
            let root = home.root().to_path_buf();
            let child = Command::new(daemon_bin())
                .arg("daemon")
                .env("HOME", &root)
                .env("OTTERHIVE_HOME", &root)
                .env("OTTERHIVE_TEST_STARTUP_GRACE_MS", "600")
                .env("OTTERHIVE_TEST_STOP_TERM_MS", "1000")
                .env("OTTERHIVE_TEST_STOP_KILL_MS", "500")
                .current_dir(&root)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("繁中說明：daemon 應可啟動");
            let daemon = Self { child: Some(child) };
            let socket = home.socket_path();
            // 不能只等 socket 檔存在：daemon 重啟時上一個行程留下的 socket
            // 檔還在，而新的 daemon 此刻可能仍在 `prepare`（讀設定、開 store、
            // 依 §4.6 收斂殘留程序）——那段時間裡檔案存在但沒有監聽，連線會
            // 得到 `Connection refused`。等待條件必須是「真的能送出請求並收到
            // 回應」。
            let deadline = Instant::now() + LONG_TIMEOUT;
            loop {
                if socket.exists() && std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "daemon 應在期限內可服務 {}（啟動自檢或收斂失敗？）",
                    socket.display()
                );
                std::thread::sleep(POLL_STEP);
            }
            daemon
        }

        pub fn pid(&self) -> i32 {
            self.child
                .as_ref()
                .map(|child| child.id() as i32)
                .unwrap_or(-1)
        }

        /// 有界停止並等待結束；回 (exit code, stdout, stderr)。
        pub fn stop_and_wait(&mut self) -> (Option<i32>, String, String) {
            let Some(mut child) = self.child.take() else {
                return (None, String::new(), String::new());
            };
            let pid = child.id() as i32;
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            let start = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return (status.code(), String::new(), String::new()),
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
            // 逾時被迫 SIGKILL：不是正常結束，以 `None` 表示無乾淨 exit code。
            (None, String::new(), String::new())
        }
    }

    impl Drop for Daemon {
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

    /// 以獨立設定執行到結束；回 (exit code, stdout, stderr)。
    pub fn run_to_completion(
        home: &TestHome,
        args: &[&str],
        binary: &str,
        extra_env: &[(&str, &str)],
    ) -> (Option<i32>, String, String) {
        let root = home.root().to_path_buf();
        let mut command = Command::new(binary);
        command
            .args(args)
            .env("HOME", &root)
            .env("OTTERHIVE_HOME", &root)
            .env("OTTERHIVE_TEST_STARTUP_GRACE_MS", "600")
            .env("OTTERHIVE_TEST_STOP_TERM_MS", "1000")
            .env("OTTERHIVE_TEST_STOP_KILL_MS", "500")
            .current_dir(&root)
            .stdin(Stdio::null());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let output = command.output().expect("繁中說明：應可執行 binary");
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// 依契約 §1 對 daemon 送一行 JSON 請求、收一行 JSON 回應。
    pub fn call(home: &TestHome, auth: &otterhive::protocol::Auth, op: &str, args: Value) -> Value {
        use std::io::{BufRead, BufReader, Write};

        let socket = home.socket_path();
        let mut stream = std::os::unix::net::UnixStream::connect(&socket)
            .unwrap_or_else(|e| panic!("繁中說明：應能連到 {}：{e}", socket.display()));
        let _ = stream.set_read_timeout(Some(RECV_TIMEOUT));
        let _ = stream.set_write_timeout(Some(RECV_TIMEOUT));
        let request = Request {
            v: 1,
            auth: auth.clone(),
            op: op.to_owned(),
            args,
            operation_id: None,
        };
        let mut line = serde_json::to_string(&request).expect("繁中說明：請求應可序列化");
        line.push('\n');
        stream
            .write_all(line.as_bytes())
            .expect("繁中說明：應可送出請求");
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .expect("繁中說明：應可讀回一行回應");
        serde_json::from_str(response.trim())
            .unwrap_or_else(|e| panic!("繁中說明：回應應是合法 JSON：{e}；實際 {response}"))
    }

    /// operator 身分的 `call`。
    pub fn operator_call(home: &TestHome, op: &str, args: Value) -> Value {
        let auth = otterhive::protocol::Auth::Operator(home.operator_token());
        call(home, &auth, op, args)
    }

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

    /// 該 pid 的程序狀態（macOS `proc_bsdinfo.pbi_status`，`SZOMB` = 5）。
    ///
    /// 這是**本測試行程**對該 pid 的直接觀測；daemon 之後能不能收斂，取決於
    /// 它自己以 `RecordedLeader` 走 `stop_group` 時讀到什麼。
    pub fn proc_state(pid: i32) -> &'static str {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return "gone";
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let ret = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                1,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if ret != size {
            return "unqueryable";
        }
        match info.pbi_status as u32 {
            libc::SZOMB => "zombie",
            libc::SRUN | libc::SSLEEP => "running",
            _ => "other",
        }
    }

    /// 該 instance 記錄的 pid 的程序狀態。
    ///
    /// pid 可能已經被清掉（daemon 順利收斂過），那時沒有程序紀錄可比對；
    /// 這時仍以 `proc_state` 直接觀察該 pid（測試行程在 `create` 的回應裡
    /// 拿到了它）。
    pub fn proc_state_of(pid: i32) -> &'static str {
        proc_state(pid)
    }

    /// 讀 fixture 寫出的 pid 檔。
    pub fn read_pid_file(path: &str) -> Option<i32> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| raw.trim().parse::<i32>().ok())
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

    /// 啟動自檢所用的觀測：`waitid(WNOWAIT)` 回傳**該 pid** 才算觀測到
    /// exit 事件。daemon 的自檢刻意**不**呼叫 `try_wait`——只靠 `try_wait`
    /// 成功並不能證明 `WNOWAIT` 可用，而 `WNOWAIT` 才是 §4.3 的前提。
    pub fn observe_exit_without_reaping(pid: i32) -> bool {
        matches!(
            otterhive::procs::waitid_si_pid(pid),
            Ok(observed) if observed == pid
        )
    }

    /// 自己的子程序：確認 `WNOWAIT` 可重複觀測同一個 exit 事件。
    pub fn spawn_probe() -> std::process::Child {
        std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("繁中說明：自檢子程序應可啟動")
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
                rusqlite::params![name],
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

    /// 等待 instance 的狀態成為 `expected`。
    pub fn wait_state(home: &TestHome, name: &str, expected: &str) -> bool {
        poll_until(LONG_TIMEOUT, || {
            instance_row(home, name)
                .map(|row| row.2 == expected)
                .unwrap_or(false)
        })
    }

    pub fn fs_write(path: &Path, contents: &str) {
        fs::write(path, contents)
            .unwrap_or_else(|e| panic!("繁中說明：應可寫入 {}：{e}", path.display()));
    }
}

use support::*;

/// 把 `otterhive` 複製到沒有 bridge 的暫存目錄後執行 `daemon` → 非零結束並
/// 說明原因（契約 §1）。
#[test]
fn daemon_refuses_to_start_without_the_bridge_binary() {
    let home = TestHome::new();

    // 隔離目錄：只放 `otterhive`，不放 `otterhive-mcp-bridge`。
    let solo = tempfile::TempDir::new().expect("繁中說明：需建立隔離暫存目錄");
    let copied = solo.path().join("otterhive");
    std::fs::copy(daemon_bin(), &copied)
        .unwrap_or_else(|e| panic!("繁中說明：應可複製 otterhive binary：{e}"));
    let mut permissions = std::fs::metadata(&copied)
        .expect("繁中說明：應可讀取複製後的 binary")
        .permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o755);
    std::fs::set_permissions(&copied, permissions).expect("繁中說明：應可設定權限");

    let root = home.root().to_path_buf();
    let output = Command::new(&copied)
        .arg("daemon")
        .env("HOME", &root)
        .env("OTTERHIVE_HOME", &root)
        .current_dir(&root)
        .stdin(Stdio::null())
        .output()
        .expect("繁中說明：應可執行複製後的 otterhive");

    assert_ne!(
        output.status.code(),
        Some(0),
        "繁中說明：缺少 bridge 時 daemon 應拒絕啟動，stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bridge") || stderr.contains("otterhive-mcp-bridge"),
        "繁中說明：stderr 應指出缺少 bridge，實際：{stderr}"
    );
    assert!(
        !home.socket_path().exists(),
        "繁中說明：拒絕啟動時不應建立 socket"
    );
}

/// 同一 home 的第二個 daemon：exit code 3，stderr 有一行含
/// `daemon_already_running` 的 JSON（契約 §1）。
///
/// 判準不只看數字：必須以 stderr 的 JSON 辨識原因。
#[test]
fn second_daemon_on_same_home_exits_with_daemon_already_running() {
    let home = TestHome::new();
    let _first = Daemon::start(&home);

    let root = home.root().to_path_buf();
    let output = Command::new(daemon_bin())
        .arg("daemon")
        .env("HOME", &root)
        .env("OTTERHIVE_HOME", &root)
        .current_dir(&root)
        .stdin(Stdio::null())
        .output()
        .expect("繁中說明：應可執行第二個 daemon");

    assert_eq!(
        output.status.code(),
        Some(3),
        "繁中說明：第二個 daemon 應以 exit 3 結束，stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let json_line = stderr
        .lines()
        .find(|line| line.contains("daemon_already_running"))
        .unwrap_or_else(|| {
            panic!("繁中說明：stderr 應有一行含 daemon_already_running 的 JSON，實際：{stderr}")
        });
    let parsed: Value = serde_json::from_str(json_line.trim())
        .unwrap_or_else(|e| panic!("繁中說明：該行應是合法 JSON：{e}；實際：{json_line}"));
    assert_eq!(
        parsed["error"]["code"],
        json!("daemon_already_running"),
        "繁中說明：JSON 的 error.code 應為 daemon_already_running，實際：{parsed}"
    );
}

/// 壞設定檔（不允許的頂層鍵）使 daemon 拒絕啟動並列出原因（契約 §1）。
#[test]
fn daemon_refuses_to_start_with_invalid_config() {
    let home = TestHome::new();
    fs_write(home.home.config_path(), "channel:\n  - broken\n");

    let (code, stdout, stderr) = run_to_completion(&home, &["daemon"], daemon_bin(), &[]);
    assert_ne!(
        code,
        Some(0),
        "繁中說明：設定檔有未知頂層鍵時 daemon 應拒絕啟動，stdout={stdout} stderr={stderr}"
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("channel"),
        "繁中說明：錯誤訊息應列出不允許的鍵 channel，實際：{combined}"
    );
    assert!(
        !home.socket_path().exists(),
        "繁中說明：拒絕啟動時不應建立 socket"
    );
}

/// SIGCHLD 可被 wait：daemon 啟動後其子程序的退出狀態可被觀察，且不會留下
/// 未回收的 zombie（daemon 自檢；§4.6 的收斂前提）。
#[test]
fn daemon_reaps_its_exited_children() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let created = operator_call(
        &home,
        "create_instance",
        json!({"name": "crashy", "command": fixture("crash_on_start.sh").to_string_lossy()}),
    );
    assert_eq!(
        created["outcome"],
        json!("refused"),
        "繁中說明：crash_on_start 應被拒絕，實際：{created}"
    );
    assert!(
        wait_state(&home, "crashy", "failed"),
        "繁中說明：狀態應為 failed，最後為 {:?}",
        instance_row(&home, "crashy")
    );

    // daemon 不應保留未回收的子程序（zombie 會讓 waitid 一直看得到）。
    let zombies = zombie_children(daemon.pid());
    assert!(
        zombies.is_empty(),
        "繁中說明：daemon 應回收已退出的子程序，不應留下 zombie {zombies:?}"
    );
}

/// 啟動自檢必須走 `waitid(WEXITED|WNOHANG|WNOWAIT)` 這條觀測路徑本身，
/// 而不只是「wait 得到」——§4.3 的全部前提都建立在「能觀測退出而不 reap」
/// 上，`try_wait` 成功證明不了這件事（它本身就是會 reap 的那條路徑）。
#[test]
fn startup_self_check_uses_waitid_wnowait_on_its_own_child() {
    let mut probe = spawn_probe();
    let pid = probe.id() as i32;
    assert!(
        poll_until(LONG_TIMEOUT, || observe_exit_without_reaping(pid)),
        "繁中說明：waitid(WNOWAIT) 應觀測到自檢子程序 {pid} 的 exit 事件"
    );
    // 觀測是**不 reap**：`WNOWAIT` 之下同一個 exit 事件可以被觀測第二次。
    assert!(
        observe_exit_without_reaping(pid),
        "繁中說明：WNOWAIT 觀測不得 reap，第二次觀測仍應看到同一個 exit 事件"
    );
    // 而它仍是本程序可 wait 的 child（自檢最後一步 `try_wait` 的前提）。
    assert!(
        probe
            .try_wait()
            .expect("繁中說明：WNOWAIT 觀測後仍應可 wait")
            .is_some(),
        "繁中說明：觀測 exit 後 try_wait 應回 Some（自檢的最後一步）"
    );
}

/// daemon 重啟後對殘留程序收斂：仍存活的程序被停止、狀態為 `stopped`
/// （原因 `daemon_restarted`）（契約 §4.6）。
#[test]
fn daemon_restart_stops_lingering_processes() {
    let home = TestHome::new();
    let mut first = Daemon::start(&home);

    let created = operator_call(
        &home,
        "create_instance",
        // `spawns_child.sh` 忽略 SIGHUP：daemon 結束時 PTY master 關閉會讓子程序
        // 收到 SIGHUP，`echo_agent.sh` 會隨之退出——那樣就沒有「殘留程序」可供
        // 重啟收斂。忽略 SIGHUP 才能真的模擬「daemon 崩潰／結束後程序仍在」。
        json!({"name": "lingering", "command": fixture("spawns_child.sh").to_string_lossy()}),
    );
    assert_eq!(
        created["outcome"],
        json!("completed"),
        "繁中說明：create 應完成，實際：{created}"
    );
    let pid = instance_pid(&home, "lingering")
        .unwrap_or_else(|| panic!("繁中說明：SQLite 應記錄 lingering 的 pid"));
    assert!(pid_running(pid), "繁中說明：前提：程序 {pid} 應在運行");

    // 停止 daemon（不經 stop_group，模擬 daemon 崩潰後程序仍留著）。
    // daemon 收到 SIGTERM 後正常結束（exit 0）才是預期；重點是「確實結束」
    // 且「結束時未經 stop_group」，讓程序留下來供重啟收斂。
    let (code, ..) = first.stop_and_wait();
    assert_eq!(
        code,
        Some(0),
        "繁中說明：daemon 應正常結束，實際 exit={code:?}"
    );
    assert!(
        pid_running(pid),
        "繁中說明：前提：daemon 結束後程序 {pid} 應仍在運行（未被 stop）"
    );

    // 重啟 daemon：應收斂殘留程序。
    let _second = Daemon::start(&home);
    assert!(
        poll_until(LONG_TIMEOUT, || !pid_running(pid)),
        "繁中說明：daemon 重啟後應停止殘留程序 {pid}，最後仍在運行"
    );
    assert!(
        wait_state(&home, "lingering", "stopped"),
        "繁中說明：收斂後狀態應為 stopped，最後為 {:?}",
        instance_row(&home, "lingering")
    );

    let snapshot = operator_call(&home, "pane_snapshot", json!({"instance": "lingering"}));
    assert_eq!(
        snapshot["outcome"],
        json!("completed"),
        "繁中說明：停止後的 snapshot 應仍可查（回最後畫面），實際：{snapshot}"
    );
    assert_eq!(
        snapshot["value"]["live"],
        json!(false),
        "繁中說明：daemon 重啟後沒有舊畫面，應標 live:false，實際：{}",
        snapshot["value"]
    );
    assert_eq!(
        snapshot["value"]["reason"],
        json!("no_screen"),
        "繁中說明：daemon 重啟後應回 reason:no_screen，實際：{}",
        snapshot["value"]
    );
}

/// §4.6 的「孤兒 zombie」：daemon 結束時 leader 已經退出、留下**沒有任何
/// 存活成員**的 group。重啟後的收斂應把這視為「停止已成立」（§4.3-3）並
/// 收斂為 `stopped`，而不是因為無法確認而永久停在 `recovery_required`。
///
/// 這是本 daemon **不是**那個 zombie 的父程序（`waitid` 必然回 ECHILD、
/// `pbi_status` 明確是 `SZOMB`）的情況——舊的 guardrail 正是把這裡判成
/// `Unproven`，於是每次 daemon 崩潰都會把已經死掉的 instance 永久卡住。
#[test]
fn daemon_restart_converges_an_orphan_zombie_leader() {
    let home = TestHome::new();
    let mut first = Daemon::start(&home);

    // leader 在寬限期內結束、留下同 group 的子程序；daemon 的 §4.2-5 清理
    // 會把那個子程序收掉，於是 group 只剩 leader 自己。leader 把自己的 pid
    // 寫進 home（`failed` 路徑會清掉 SQLite 的 pid 紀錄，所以 pid 從這裡拿）。
    let leader_pid = format!("{}/orphan-zombie.pid", home.home.root().display());
    let created = operator_call(
        &home,
        "create_instance",
        json!({
            "name": "orphan-zombie",
            "command": fixture("spawns_child.sh").to_string_lossy(),
            "env": {
                "FIXTURE_LEADER_EXIT_AFTER_MS": "50",
                "FIXTURE_LEADER_PID_FILE": leader_pid,
            },
        }),
    );
    assert_eq!(
        created["outcome"],
        json!("refused"),
        "繁中說明：寬限期內 leader 結束應回 refused，實際：{created}"
    );
    let pid = read_pid_file(&leader_pid)
        .unwrap_or_else(|| panic!("繁中說明：fixture 應把 leader pid 寫進 {leader_pid}"));
    assert!(
        poll_until(LONG_TIMEOUT, || !pid_running(pid)),
        "繁中說明：前提：leader 應已退出且被收乾淨（pid {pid} 不應再活著）"
    );

    // 停止 daemon。此時記憶體裡的 group 已空，沒有留下任何子程序。
    let (code, ..) = first.stop_and_wait();
    assert_eq!(
        code,
        Some(0),
        "繁中說明：daemon 應正常結束，實際 exit={code:?}"
    );
    // 前提檢查：這個 pid 要嘛是沒有存活成員的孤兒 zombie，要嘛已經被 init
    // 回收。父程序是已結束的 daemon，init(1) 接手後會立刻 reap，所以不能
    // 要求它一定是 zombie——那取決於競速，不是這條規則。
    let observed = proc_state_of(pid);
    assert!(
        matches!(observed, "zombie" | "gone"),
        "繁中說明：重啟前的程序狀態應為 zombie（孤兒）或 gone（init 已回收），\
         實際為 {observed:?}"
    );

    // 重啟 daemon：收斂應以 §4.3-3 的判準（group 除已退出的 leader 外沒有
    // 成員）判定停止已成立 → `stopped`。
    let _second = Daemon::start(&home);
    assert!(
        wait_state(&home, "orphan-zombie", "stopped"),
        "繁中說明：孤兒 zombie 的收斂應為 stopped，最後為 {:?}",
        instance_row(&home, "orphan-zombie")
    );
}

/// 已標 `recovery_required` 的 instance 在 daemon 重啟後不被自動解除，也不被
/// 重新啟動（契約 §4.6）。
#[test]
fn existing_recovery_required_is_not_cleared_by_restart() {
    let home = TestHome::new();
    let mut first = Daemon::start(&home);

    let created = operator_call(
        &home,
        "create_instance",
        json!({"name": "stuck", "command": fixture("spawns_child.sh").to_string_lossy()}),
    );
    assert_eq!(
        created["outcome"],
        json!("completed"),
        "繁中說明：create 應完成，實際：{created}"
    );
    let pid = instance_pid(&home, "stuck")
        .unwrap_or_else(|| panic!("繁中說明：SQLite 應記錄 stuck 的 pid"));

    // 直接把狀態設為 recovery_required（模擬先前無法確認停止）。
    force_state(&home, "stuck", "recovery_required", "cleanup_unproven");
    let (code, ..) = first.stop_and_wait();
    assert_eq!(
        code,
        Some(0),
        "繁中說明：daemon 應正常結束，實際 exit={code:?}"
    );

    let _second = Daemon::start(&home);
    // 不應被自動重新啟動，也不應變回 stopped／running。
    assert!(
        wait_state(&home, "stuck", "recovery_required"),
        "繁中說明：既有的 recovery_required 應保留，最後為 {:?}",
        instance_row(&home, "stuck")
    );
    assert!(
        pid_running(pid),
        "繁中說明：recovery_required 不應被自動重新啟動（pid {pid} 應仍存在）"
    );

    let started = operator_call(&home, "start_instance", json!({"instance": "stuck"}));
    assert_eq!(
        started["outcome"],
        json!("refused"),
        "繁中說明：recovery_required 的 start 應被拒絕，實際：{started}"
    );
    assert_eq!(
        started["error"]["code"],
        json!("recovery_required"),
        "繁中說明：應回 recovery_required，實際：{started}"
    );
    assert_ne!(
        instance_pid(&home, "stuck"),
        Some(0),
        "繁中說明：不得產生新程序"
    );
}

/// daemon 的直接子程序中是否有 zombie。
fn zombie_children(daemon_pid: i32) -> Vec<i32> {
    let output = Command::new("ps")
        .args(["-o", "pid=,stat=,ppid=", "-ax"])
        .output()
        .expect("繁中說明：應可執行 ps");
    let text = String::from_utf8_lossy(&output.stdout);
    let mut zombies = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 {
            continue;
        }
        let Ok(pid) = fields[0].parse::<i32>() else {
            continue;
        };
        let Ok(ppid) = fields[2].parse::<i32>() else {
            continue;
        };
        if ppid == daemon_pid && fields[1].starts_with('Z') {
            zombies.push(pid);
        }
    }
    zombies
}

/// 直接把 instance 標成指定狀態。
fn force_state(home: &TestHome, name: &str, state: &str, reason: &str) {
    let path = home.home.root().join("state.sqlite3");
    let connection = rusqlite::Connection::open(&path)
        .unwrap_or_else(|e| panic!("繁中說明：應可開啟 {}：{e}", path.display()));
    connection
        .execute(
            "UPDATE instances SET state = ?2, reason = ?3 WHERE name = ?1",
            rusqlite::params![name, state, reason],
        )
        .unwrap_or_else(|e| panic!("繁中說明：應可強制設定 state：{e}"));
}
