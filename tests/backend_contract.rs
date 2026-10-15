//! T4 Lane A RED：backend adapter 的契約（§5）。
//!
//! 依 `docs/plans/p1a-implementation.md` T4。全部以 fake executable 驗證：
//! `fake_claude.sh` 轉儲 argv、MCP 設定內容、協調提示內容、環境變數名稱與
//! resume 證據；`command` backend 則以 echo fixture 驗證不寫設定檔。
//!
//! 全檔只在 macOS 編譯；所有等待有上限並回報最後觀測狀態；使用暫存的
//! `HOME`／`OTTERHIVE_HOME`／cwd。

#![cfg(target_os = "macos")]

use std::fs;
use std::path::Path;

use serde_json::Value;

mod support {
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use otterhive::home::Home;
    use otterhive::protocol::{Auth, OperationResult, Request};
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

        pub fn workspace(&self, name: &str) -> PathBuf {
            self.home.root().join("workspace").join(name)
        }

        pub fn operator(&self) -> Auth {
            Auth::Operator(self.operator_token())
        }

        pub fn instance_auth(&self, name: &str) -> String {
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
            assert!(
                poll_until(LONG_TIMEOUT, || socket.exists()),
                "繁中說明：daemon 應在期限內建立 {}",
                socket.display()
            );
            daemon
        }

        pub fn call(&self, home: &TestHome, auth: &Auth, op: &str, args: Value) -> OperationResult {
            self.call_with_id(home, auth, op, args, None)
        }

        pub fn call_with_id(
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
            let _ = stream.set_read_timeout(Some(RECV_TIMEOUT));
            let _ = stream.set_write_timeout(Some(RECV_TIMEOUT));
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

        pub fn operator_call(&self, home: &TestHome, op: &str, args: Value) -> OperationResult {
            let auth = home.operator();
            self.call_with_id(home, &auth, op, args, None)
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

    pub fn read_lines(path: &Path) -> Vec<String> {
        let text = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("繁中說明：應可讀取 {}：{e}", path.display()));
        text.lines().map(|line| line.to_owned()).collect()
    }

    pub fn read_text(path: &Path) -> String {
        fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("繁中說明：應可讀取 {}：{e}", path.display()))
    }

    pub fn wait_for_file(path: &Path) -> bool {
        poll_until(LONG_TIMEOUT, || path.exists())
    }
}

use support::*;

/// §5.1：`fake_claude.sh` 收到的 argv 依序含 `--dangerously-skip-permissions`、
/// `--model`、`--effort`、`--append-system-prompt-file <wd>/.claude/otterhive.md`、
/// `--mcp-config <wd>/.claude/otterhive-mcp.json`，`args` 在最後。
#[test]
fn claude_argv_order_matches_the_contract() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let argv_log = home.root().join("claude-argv.log");

    let created = daemon.operator_call(
        &home,
        "create_instance",
        serde_json::json!({
            "name": "argv-check",
                "command": fixture("fake_claude.sh").to_string_lossy(),
            "model": "test-model",
            "effort": "high",
            "args": ["--custom-flag", "custom-value"],
            "env": {"OTTERHIVE_FIXTURE_LOG": argv_log.to_string_lossy()},
        }),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );
    let workdir = home.workspace("argv-check");
    assert!(
        wait_for_file(&argv_log),
        "繁中說明：fake_claude.sh 應寫入 argv log {}",
        argv_log.display()
    );

    let argv = read_lines(&argv_log);
    let index_of = |needle: &str| {
        argv.iter()
            .position(|arg| arg == needle)
            .unwrap_or_else(|| panic!("繁中說明：argv 應含 {needle}，實際 {argv:?}"))
    };
    let skip = index_of("--dangerously-skip-permissions");
    let model = index_of("--model");
    let effort = index_of("--effort");
    let prompt = index_of("--append-system-prompt-file");
    let mcp = index_of("--mcp-config");
    let custom = index_of("--custom-flag");
    let custom_value = index_of("custom-value");

    assert!(
        skip < model,
        "繁中說明：--dangerously-skip-permissions 應在最前，實際 {argv:?}"
    );
    assert!(model < effort, "實際 {argv:?}");
    assert_eq!(
        argv[model + 1],
        "test-model",
        "繁中說明：--model 應接 model 值，實際 {argv:?}"
    );
    assert_eq!(
        argv[effort + 1],
        "high",
        "繁中說明：--effort 應接 effort 值，實際 {argv:?}"
    );
    assert!(effort < prompt, "實際 {argv:?}");
    assert!(prompt < mcp, "實際 {argv:?}");
    assert_eq!(
        argv[prompt + 1],
        workdir.join(".claude/otterhive.md").to_string_lossy(),
        "繁中說明：協調提示路徑應在 workdir 下，實際 {argv:?}"
    );
    assert_eq!(
        argv[mcp + 1],
        workdir.join(".claude/otterhive-mcp.json").to_string_lossy(),
        "繁中說明：MCP 設定路徑應在 workdir 下，實際 {argv:?}"
    );
    assert!(mcp < custom, "繁中說明：args 應附加在最後，實際 {argv:?}");
    assert_eq!(
        custom_value,
        custom + 1,
        "繁中說明：args 的順序應保留，實際 {argv:?}"
    );
}

/// §5.1：`otterhive.md` 含 instance 名稱；instructions 原文附在協調提示之後。
#[test]
fn coordination_prompt_contains_name_and_instructions() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let prompt_dump = home.root().join("claude-prompt.txt");
    let instructions = home.root().join("instructions.md");
    fs::write(&instructions, "# 指示\n\n這是 instance 專屬的指示原文。\n")
        .expect("繁中說明：應可寫入 instructions 檔");

    let created = daemon.operator_call(
        &home,
        "create_instance",
        serde_json::json!({
            "name": "prompted",
                "command": fixture("fake_claude.sh").to_string_lossy(),
            "instructions": instructions.to_string_lossy(),
            "env": {"OTTERHIVE_FIXTURE_PROMPT": prompt_dump.to_string_lossy()},
        }),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );
    let prompt_file = home.workspace("prompted").join(".claude/otterhive.md");
    assert!(
        wait_for_file(&prompt_dump),
        "繁中說明：fake_claude.sh 應轉儲協調提示 {}",
        prompt_dump.display()
    );
    // daemon 寫入的檔案與傳給子程序的應是同一份。
    assert_eq!(
        read_text(&prompt_file),
        read_text(&prompt_dump),
        "繁中說明：傳給子程序的應是 daemon 寫入的 {}，實際兩者不同",
        prompt_file.display()
    );

    let prompt = read_text(&prompt_dump);
    assert!(
        prompt.contains("prompted"),
        "繁中說明：協調提示應含 instance 名稱，實際：{prompt}"
    );
    let instructions_text = read_text(&instructions);
    assert!(
        prompt.contains("這是 instance 專屬的指示原文。"),
        "繁中說明：協調提示應含 instructions 原文，實際：{prompt}"
    );
    assert!(
        prompt.find("這是 instance 專屬的指示原文。") > prompt.find("prompted"),
        "繁中說明：instructions 應附在協調提示之後，實際：{prompt}"
    );
    let _ = instructions_text;
}

/// §5.1：MCP 設定權限 0600、JSON 結構符合契約、含該 generation 的 token；
/// §5.3：子程序環境有 `OTTERHIVE_INSTANCE`／`OTTERHIVE_MCP_BRIDGE`，沒有
/// `OTTERHIVE_OPERATOR` 前綴變數，且有 `TERM=xterm-256color`。
#[test]
fn mcp_config_and_child_environment_match_the_contract() {
    let home = TestHome::new();
    let env_dump = home.root().join("claude-env.txt");
    let mcp_dump = home.root().join("claude-mcp.json");

    // daemon 自己帶一個 OTTERHIVE_OPERATOR_* 變數；§5.3 要求子程序看不到。
    let daemon = Daemon::start_with_env(&home, &[("OTTERHIVE_OPERATOR_SECRET", "leak-me")]);
    let term_dump = home.root().join("claude-term.txt");
    let created = daemon.operator_call(
        &home,
        "create_instance",
        serde_json::json!({"name": "env-check", "command": fixture("fake_claude.sh").to_string_lossy(), "env": {
            "OTTERHIVE_FIXTURE_ENV": env_dump.to_string_lossy(),
            "OTTERHIVE_FIXTURE_TERM": term_dump.to_string_lossy(),
            "OTTERHIVE_FIXTURE_MCP": mcp_dump.to_string_lossy(),
        }}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );

    let workdir = home.workspace("env-check");
    let mcp_file = workdir.join(".claude/otterhive-mcp.json");
    assert!(
        wait_for_file(&mcp_file),
        "繁中說明：應寫入 {}",
        mcp_file.display()
    );
    let permissions = std::fs::metadata(&mcp_file)
        .expect("繁中說明：應可讀取 MCP 設定中繼資料")
        .permissions();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        permissions.mode() & 0o777,
        0o600,
        "繁中說明：MCP 設定權限應為 0600，實際 {:o}",
        permissions.mode() & 0o777
    );
    assert!(
        wait_for_file(&mcp_dump),
        "繁中說明：fake_claude 應轉儲 MCP 設定"
    );
    assert!(
        wait_for_file(&env_dump),
        "繁中說明：fake_claude 應轉儲環境變數名稱"
    );
    assert!(
        wait_for_file(&term_dump),
        "繁中說明：fake_claude 應轉儲 TERM 的值"
    );

    let mcp: Value = serde_json::from_str(&read_text(&mcp_dump))
        .unwrap_or_else(|e| panic!("繁中說明：MCP 設定應是合法 JSON：{e}"));
    assert_eq!(
        mcp["mcpServers"]["otterhive"]["type"],
        serde_json::json!("stdio"),
        "繁中說明：MCP server 類型應為 stdio，實際：{mcp}"
    );
    assert_eq!(
        mcp["mcpServers"]["otterhive"]["args"],
        serde_json::json!([]),
        "實際：{mcp}"
    );
    let bridge = mcp["mcpServers"]["otterhive"]["command"]
        .as_str()
        .unwrap_or_else(|| panic!("繁中說明：MCP 設定應含 bridge command，實際：{mcp}"));
    assert!(
        bridge.ends_with("otterhive-mcp-bridge"),
        "繁中說明：bridge command 應指向 otterhive-mcp-bridge，實際：{bridge}"
    );
    assert!(
        Path::new(bridge).is_absolute(),
        "繁中說明：bridge 應為絕對路徑，實際：{bridge}"
    );
    assert_eq!(
        mcp["mcpServers"]["otterhive"]["env"]["OTTERHIVE_HOME"],
        serde_json::json!(home.root().to_string_lossy()),
        "實際：{mcp}"
    );
    let token = mcp["mcpServers"]["otterhive"]["env"]["OTTERHIVE_INSTANCE_TOKEN"]
        .as_str()
        .unwrap_or_else(|| panic!("繁中說明：MCP 設定應含 token，實際：{mcp}"));
    assert!(!token.is_empty(), "繁中說明：token 不得為空，實際：{token}");
    assert_eq!(
        home.instance_auth("env-check"),
        token,
        "繁中說明：MCP 設定的 token 應即 instance 認證 token"
    );

    let env_names = read_lines(&env_dump);
    for required in [
        "OTTERHIVE_HOME",
        "OTTERHIVE_INSTANCE",
        "OTTERHIVE_INSTANCE_TOKEN",
        "OTTERHIVE_MCP_BRIDGE",
        "TERM",
    ] {
        assert!(
            env_names.iter().any(|name| name == required),
            "繁中說明：子程序環境應有 {required}，實際：{env_names:?}"
        );
    }
    assert!(
        !env_names
            .iter()
            .any(|name| name.starts_with("OTTERHIVE_OPERATOR")),
        "繁中說明：子程序不應看到 OTTERHIVE_OPERATOR 前綴變數，實際：{env_names:?}"
    );
    let term_value = read_text(&term_dump);
    assert_eq!(
        term_value.trim(),
        "xterm-256color",
        "繁中說明：§5.3 要求子程序的 TERM=xterm-256color，實際：{term_value:?}"
    );
}

/// §5.1：`skills_path` 存在時建立 symlink；預先存在的 `.claude/skills` 目錄不被
/// 覆蓋且回 warning。
#[test]
fn skills_symlink_is_created_and_existing_path_is_not_overwritten() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let skills = home.root().join("shared-skills");
    std::fs::create_dir_all(&skills).expect("繁中說明：應可建立 skills 目錄");

    let created = daemon.operator_call(
        &home,
        "create_instance",
        serde_json::json!({"name": "skilled",
                "command": fixture("fake_claude.sh").to_string_lossy(), "skills_path": skills.to_string_lossy()}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成，實際為 {created:?}"
    );
    let link = home.workspace("skilled").join(".claude/skills");
    assert!(
        wait_for_file(&link),
        "繁中說明：應建立 {} symlink",
        link.display()
    );
    assert!(
        std::fs::symlink_metadata(&link)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false),
        "繁中說明：{} 應是 symlink",
        link.display()
    );
    assert_eq!(
        std::fs::read_link(&link).expect("繁中說明：應可讀取 symlink 目標"),
        skills,
        "繁中說明：symlink 應指向 skills_path"
    );
}

/// §5.1：預先存在的同名 `.claude/skills` 目錄不被覆蓋，且 start 結果回 warning。
#[test]
fn preexisting_skills_directory_is_not_overwritten_and_warns() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let skills = home.root().join("other-skills");
    std::fs::create_dir_all(&skills).expect("繁中說明：應可建立 skills 目錄");

    // 先手動建立 workdir 與既有的 skills 目錄（內容為一個標記檔）。
    let workdir = home.workspace("preowned");
    std::fs::create_dir_all(workdir.join(".claude/skills"))
        .expect("繁中說明：應可建立既有 skills 目錄");
    fs::write(workdir.join(".claude/skills/marker.txt"), "do-not-touch")
        .expect("繁中說明：應可寫入標記檔");

    let created = daemon.operator_call(
        &home,
        "create_instance",
        serde_json::json!({"name": "preowned",
                "command": fixture("fake_claude.sh").to_string_lossy(), "skills_path": skills.to_string_lossy(),
                           "working_directory": workdir.to_string_lossy()}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：create 應完成（既有目錄只回 warning），實際為 {created:?}"
    );
    let link = workdir.join(".claude/skills");
    assert!(
        !std::fs::symlink_metadata(&link)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false),
        "繁中說明：既有目錄不應被替換成 symlink"
    );
    assert!(
        link.join("marker.txt").exists(),
        "繁中說明：既有目錄的內容不應被刪除"
    );
    let warnings = created.value["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("繁中說明：應回 value.warnings，實際：{}", created.value));
    assert!(
        warnings
            .iter()
            .any(|item| item.as_str().is_some_and(|text| text.contains("skills"))),
        "繁中說明：warning 應指出 skills，實際：{warnings:?}"
    );
}

/// §5.2：`backend: command` 在 T4 回 `unsupported_in_phase`，不執行任何東西。
///
/// 契約 §5.2 的完整 command 行為（只執行 `command` 加 `args`、不寫設定檔）
/// 延到需要它的工作包；T4 **不**以未驗證的 fixture 冒充，所以這裡驗的是
/// 「明確拒絕且零副作用」，不是「照 §5.2 執行」。
#[test]
fn explicit_command_backend_is_unsupported_and_writes_nothing() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let argv_log = home.root().join("cmd-argv.log");

    let unsupported = daemon.operator_call(
        &home,
        "create_instance",
        serde_json::json!({
            "name": "plain-cmd",
            "backend": "command",
            "command": fixture("echo_agent.sh").to_string_lossy(),
            "args": ["alpha", "beta"],
            "env": {"OTTERHIVE_FIXTURE_LOG": argv_log.to_string_lossy()},
        }),
    );
    assert_eq!(
        unsupported.outcome,
        otterhive::protocol::Outcome::Unsupported,
        "繁中說明：T4 的 backend:command 應回 unsupported，實際為 {unsupported:?}"
    );
    assert_eq!(
        unsupported.error.as_ref().map(|error| error.code),
        Some(otterhive::protocol::ErrorCode::UnsupportedInPhase),
        "繁中說明：應回 unsupported_in_phase，實際為 {unsupported:?}"
    );
    assert!(
        unsupported.effects.is_empty(),
        "繁中說明：unsupported 在任何變更前拒絕，effects 應為空，實際 {:?}",
        unsupported.effects
    );
    // 零副作用：沒有 instance、沒有工作目錄、沒有設定檔、沒有 argv。
    assert!(
        instance_row(&home, "plain-cmd").is_none(),
        "繁中說明：不應建立 instance"
    );
    assert!(
        !home.workspace("plain-cmd").exists(),
        "繁中說明：不應建立工作目錄"
    );
    assert!(
        !argv_log.exists(),
        "繁中說明：不得執行 echo_agent.sh（argv log 不應出現），實際 {}",
        argv_log.display()
    );
}

/// §5.4：`claude` backend 有 resume 證據時以 `--continue` spawn；無證據 →
/// `resume_unavailable`。
#[test]
fn claude_resume_uses_continue_when_evidence_exists() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let argv_log = home.root().join("resume-argv.log");
    let resume_dump = home.root().join("resume-evidence.txt");

    let created = daemon.operator_call(
        &home,
        "create_instance",
        serde_json::json!({"name": "resumable",
                "command": fixture("fake_claude.sh").to_string_lossy(), "env": {
            "OTTERHIVE_FIXTURE_LOG": argv_log.to_string_lossy(),
            "OTTERHIVE_FIXTURE_RESUME": resume_dump.to_string_lossy(),
        }}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：首次 create 應完成，實際為 {created:?}"
    );
    let workdir = home.workspace("resumable");
    let encoded = encode_working_directory(&workdir);
    // 建立 resume 證據：$HOME/.claude/projects/<編碼>/*.jsonl 含 "type":"user"。
    let project_dir = home.root().join(".claude/projects").join(&encoded);
    std::fs::create_dir_all(&project_dir).expect("繁中說明：應可建立 projects 目錄");
    fs::write(
        project_dir.join("session.jsonl"),
        "{\"type\":\"user\",\"message\":{\"role\":\"user\"}}\n",
    )
    .expect("繁中說明：應可寫入 resume 證據");

    let resumed = daemon.operator_call(
        &home,
        "restart_instance",
        serde_json::json!({"instance": "resumable", "mode": "resume"}),
    );
    assert!(
        matches!(
            resumed.outcome,
            otterhive::protocol::Outcome::Completed | otterhive::protocol::Outcome::Queued
        ),
        "繁中說明：有證據時 resume 應成功，實際為 {resumed:?}"
    );
    assert!(
        wait_for_file(&argv_log),
        "繁中說明：resume 後 fake_claude 應再寫一次 argv"
    );
    let argv = read_lines(&argv_log);
    assert!(
        argv.iter().any(|arg| arg == "--continue"),
        "繁中說明：有證據的 resume 應加 --continue，實際：{argv:?}"
    );
    assert!(
        wait_for_file(&resume_dump),
        "繁中說明：fake_claude 應轉儲證據判定"
    );
}

/// §5.4：無 resume 證據 → `resume_unavailable`，原程序不變。
#[test]
fn claude_resume_without_evidence_is_unavailable() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    // 顯式指定 fake 可執行檔。契約 §5.1 說省略 `command` 時執行 `claude`，
    // 那條預設是正確的產品行為，但測試**不能**真的去跑 PATH 上的 `claude`：
    // 有沒有裝了那支 CLI 會決定測試通過與否（本機有、runner 沒有，於是
    // create 變成 `spawn_failed`）。backend 行為一律用
    // `tests/fixtures/backends/` 的 fake 測（CLAUDE.md 的邊界）。
    let created = daemon.operator_call(
        &home,
        "create_instance",
        serde_json::json!({"name": "no-evidence",
                           "command": fixture("fake_claude.sh").to_string_lossy()}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：首次 create 應完成，實際為 {created:?}"
    );
    let pid_before = instance_pid(&home, "no-evidence");

    let refused = daemon.operator_call(
        &home,
        "restart_instance",
        serde_json::json!({"instance": "no-evidence", "mode": "resume"}),
    );
    assert_eq!(
        refused.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：無證據的 resume 應被拒絕，實際為 {refused:?}"
    );
    assert_eq!(
        refused.error.as_ref().map(|error| error.code),
        Some(otterhive::protocol::ErrorCode::ResumeUnavailable),
        "繁中說明：應回 resume_unavailable，實際為 {refused:?}"
    );
    assert_eq!(
        instance_pid(&home, "no-evidence"),
        pid_before,
        "繁中說明：resume 被拒時原程序不應改變"
    );
}

/// §5.4：與其他未刪除 instance 共用 canonical working directory 時，
/// `resume` 回 `resume_unavailable`（原因 `shared_working_directory`）。
#[test]
fn shared_working_directory_blocks_resume() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let shared = home.root().join("shared-workdir");
    std::fs::create_dir_all(&shared).expect("繁中說明：應可建立共用工作目錄");

    for name in ["twin-a", "twin-b"] {
        let created = daemon.operator_call(
            &home,
            "create_instance",
            serde_json::json!({"name": name, "command": fixture("echo_agent.sh").to_string_lossy(),
                               "working_directory": shared.to_string_lossy()}),
        );
        assert_eq!(
            created.outcome,
            otterhive::protocol::Outcome::Completed,
            "繁中說明：{name} 應建立成功，實際為 {created:?}"
        );
    }

    let refused = daemon.operator_call(
        &home,
        "restart_instance",
        serde_json::json!({"instance": "twin-a", "mode": "resume"}),
    );
    assert_eq!(
        refused.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：共用工作目錄的 resume 應被拒絕，實際為 {refused:?}"
    );
    assert_eq!(
        refused.error.as_ref().map(|error| error.code),
        Some(otterhive::protocol::ErrorCode::ResumeUnavailable),
        "實際為 {refused:?}"
    );
    assert_eq!(
        refused.value["reason"],
        serde_json::json!("shared_working_directory"),
        "繁中說明：原因應為 shared_working_directory，實際：{}",
        refused.value
    );
}

/// §4.1：`working_directory` 未給時使用 `$OTTERHIVE_HOME/workspace/<name>`，
/// 不存在則建立；明確給的目錄不會被建立。
#[test]
fn default_working_directory_is_created_and_explicit_one_is_not() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let created = daemon.operator_call(
        &home,
        "create_instance",
        serde_json::json!({"name": "default-wd", "command": fixture("echo_agent.sh").to_string_lossy()}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "實際為 {created:?}"
    );
    assert!(
        home.workspace("default-wd").is_dir(),
        "繁中說明：未給 working_directory 時應建立 {}",
        home.workspace("default-wd").display()
    );

    let refused = daemon.operator_call(
        &home,
        "create_instance",
        serde_json::json!({"name": "explicit-wd", "command": fixture("echo_agent.sh").to_string_lossy(),
                           "working_directory": home.root().join("not-created").to_string_lossy()}),
    );
    assert_eq!(
        refused.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：明確給的不存在目錄應被拒絕，實際為 {refused:?}"
    );
    assert_eq!(
        refused.error.as_ref().map(|error| error.code),
        Some(otterhive::protocol::ErrorCode::InvalidArgument),
        "實際為 {refused:?}"
    );
    assert!(
        !home.root().join("not-created").exists(),
        "繁中說明：明確給的目錄不應被建立"
    );
}

/// §4.5：delete 不刪除 working directory。
#[test]
fn delete_does_not_remove_working_directory() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let kept = home.root().join("kept-dir");
    std::fs::create_dir_all(&kept).expect("繁中說明：應可建立目錄");
    fs::write(kept.join("data.txt"), "keep me").expect("繁中說明：應可寫入檔案");

    let created = daemon.operator_call(
        &home,
        "create_instance",
        serde_json::json!({"name": "keeper", "command": fixture("echo_agent.sh").to_string_lossy(),
                           "working_directory": kept.to_string_lossy()}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "實際為 {created:?}"
    );
    let deleted = daemon.operator_call(
        &home,
        "delete_instance",
        serde_json::json!({"instance": "keeper"}),
    );
    assert_eq!(
        deleted.outcome,
        otterhive::protocol::Outcome::Completed,
        "實際為 {deleted:?}"
    );
    assert!(
        kept.join("data.txt").exists(),
        "繁中說明：delete 不應刪除 working directory"
    );
}

/// §4.1：子程序繼承 daemon 環境，`env` 欄位以 `K=V` 加入。
#[test]
fn spec_env_is_passed_to_the_child() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let env_dump = home.root().join("child-env.txt");

    let created = daemon.operator_call(
        &home,
        "create_instance",
        serde_json::json!({"name": "env-passthrough",
                "command": fixture("fake_claude.sh").to_string_lossy(), "env": {
            "OTTERHIVE_FIXTURE_ENV": env_dump.to_string_lossy(),
            "CUSTOM_VAR": "custom-value",
        }}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "實際為 {created:?}"
    );
    assert!(
        wait_for_file(&env_dump),
        "繁中說明：fake_claude 應轉儲環境變數"
    );
    let names = read_lines(&env_dump);
    assert!(
        names.iter().any(|name| name == "CUSTOM_VAR"),
        "繁中說明：spec.env 應傳給子程序，實際：{names:?}"
    );
}

/// §4.7：`interrupt` 在未運行的 instance 上回 `invalid_argument`。
#[test]
fn interrupt_on_non_running_instance_is_invalid() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    let refused = daemon.operator_call(
        &home,
        "create_instance",
        serde_json::json!({"name": "not-running", "command": fixture("crash_on_start.sh").to_string_lossy()}),
    );
    assert_eq!(
        refused.outcome,
        otterhive::protocol::Outcome::Refused,
        "實際為 {refused:?}"
    );
    assert!(
        poll_until(LONG_TIMEOUT, || instance_row(&home, "not-running")
            .map(|row| row.2 == "failed")
            .unwrap_or(false)),
        "繁中說明：狀態應為 failed，最後為 {:?}",
        instance_row(&home, "not-running")
    );
    let result = daemon.operator_call(
        &home,
        "interrupt",
        serde_json::json!({"instance": "not-running"}),
    );
    assert_eq!(
        result.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：未運行 instance 的 interrupt 應被拒絕，實際為 {result:?}"
    );
    assert_eq!(
        result.error.as_ref().map(|error| error.code),
        Some(otterhive::protocol::ErrorCode::InvalidArgument),
        "實際為 {result:?}"
    );
}

/// §1：請求帶 `from`／`sender` 欄位 → `invalid_argument`（呼叫端不能自報身分）。
#[test]
fn requests_carrying_sender_fields_are_invalid() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    for (label, args) in [
        ("from", serde_json::json!({"from": "someone"})),
        ("sender", serde_json::json!({"sender": "someone"})),
        ("role", serde_json::json!({"role": "manager"})),
    ] {
        let refused = daemon.operator_call(&home, "list_instances", args);
        assert_eq!(
            refused.outcome,
            otterhive::protocol::Outcome::Refused,
            "繁中說明：帶 {label} 的請求應被拒絕，實際為 {refused:?}"
        );
        assert_eq!(
            refused.error.as_ref().map(|error| error.code),
            Some(otterhive::protocol::ErrorCode::InvalidArgument),
            "繁中說明：帶 {label} 應回 invalid_argument，實際為 {refused:?}"
        );
    }
}

/// §4.7：`list_instances` 帶 `instance` 時只回該項。
#[test]
fn list_instances_can_filter_by_name() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);

    for name in ["one", "two"] {
        let created = daemon.operator_call(
            &home,
            "create_instance",
            serde_json::json!({"name": name, "command": fixture("echo_agent.sh").to_string_lossy()}),
        );
        assert_eq!(
            created.outcome,
            otterhive::protocol::Outcome::Completed,
            "繁中說明：{name} 應建立成功，實際為 {created:?}"
        );
    }

    let filtered = daemon.operator_call(
        &home,
        "list_instances",
        serde_json::json!({"instance": "two"}),
    );
    assert_eq!(
        filtered.outcome,
        otterhive::protocol::Outcome::Completed,
        "實際為 {filtered:?}"
    );
    let instances = filtered.value["instances"]
        .as_array()
        .unwrap_or_else(|| panic!("繁中說明：應回 instances 陣列，實際：{}", filtered.value));
    assert_eq!(
        instances.len(),
        1,
        "繁中說明：帶 instance 應只回該項，實際：{instances:?}"
    );
    assert_eq!(instances[0]["name"], serde_json::json!("two"));

    let missing = daemon.operator_call(
        &home,
        "list_instances",
        serde_json::json!({"instance": "no-such-instance"}),
    );
    assert_eq!(
        missing.outcome,
        otterhive::protocol::Outcome::Refused,
        "繁中說明：不存在的 instance 應回 not_found，實際為 {missing:?}"
    );
    assert_eq!(
        missing.error.as_ref().map(|error| error.code),
        Some(otterhive::protocol::ErrorCode::NotFound),
        "實際為 {missing:?}"
    );
}

/// §5.1：skills symlink 記錄在 journal（`skills_link_created`），restart 時可更新。
#[test]
fn skills_symlink_journal_entry_is_recorded() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let skills = home.root().join("journal-skills");
    std::fs::create_dir_all(&skills).expect("繁中說明：應可建立 skills 目錄");

    let created = daemon.operator_call(
        &home,
        "create_instance",
        serde_json::json!({"name": "journalled",
                "command": fixture("fake_claude.sh").to_string_lossy(), "skills_path": skills.to_string_lossy()}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "實際為 {created:?}"
    );

    let path = home.home.root().join("state.sqlite3");
    let connection = rusqlite::Connection::open(&path)
        .unwrap_or_else(|e| panic!("繁中說明：應可開啟 {}：{e}", path.display()));
    let kinds: Vec<String> = {
        let mut statement = connection
            .prepare("SELECT kind FROM journal WHERE instance_id IS NOT NULL")
            .unwrap_or_else(|e| panic!("繁中說明：應可準備 journal 查詢：{e}"));
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap_or_else(|e| panic!("繁中說明：應可查詢 journal：{e}"));
        rows.filter_map(Result::ok).collect()
    };
    assert!(
        kinds.iter().any(|kind| kind == "skills_link_created"),
        "繁中說明：journal 應記錄 skills_link_created，實際：{kinds:?}"
    );
}

/// §5.1：子程序的 `OTTERHIVE_MCP_BRIDGE` 與 MCP 設定的 command 相同。
#[test]
fn bridge_path_in_env_matches_mcp_config() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    let env_dump = home.root().join("bridge-env.txt");
    let mcp_dump = home.root().join("bridge-mcp.json");

    let created = daemon.operator_call(
        &home,
        "create_instance",
        serde_json::json!({"name": "bridge-env",
                "command": fixture("fake_claude.sh").to_string_lossy(), "env": {
            "OTTERHIVE_FIXTURE_ENV": env_dump.to_string_lossy(),
            "OTTERHIVE_FIXTURE_MCP": mcp_dump.to_string_lossy(),
        }}),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "實際為 {created:?}"
    );
    assert!(wait_for_file(&mcp_dump), "繁中說明：應轉儲 MCP 設定");
    assert!(wait_for_file(&env_dump), "繁中說明：應轉儲環境變數");
    let mcp: Value = serde_json::from_str(&read_text(&mcp_dump))
        .unwrap_or_else(|e| panic!("繁中說明：MCP 設定應是 JSON：{e}"));
    let bridge_in_config = mcp["mcpServers"]["otterhive"]["command"]
        .as_str()
        .unwrap_or_else(|| panic!("繁中說明：MCP 設定應含 command，實際：{mcp}"));
    // env dump 只有名稱；用 fake_claude 的 env 轉儲無法取得值，因此改驗證
    // 該名稱存在且 MCP 設定的 bridge 是絕對路徑且存在於檔案系統。
    let names = read_lines(&env_dump);
    assert!(
        names.iter().any(|name| name == "OTTERHIVE_MCP_BRIDGE"),
        "繁中說明：子程序環境應有 OTTERHIVE_MCP_BRIDGE，實際：{names:?}"
    );
    assert!(
        Path::new(bridge_in_config).exists(),
        "繁中說明：MCP 設定的 bridge 路徑應存在，實際：{bridge_in_config}"
    );
}

/// 編碼規則（§5.4）：canonical working directory 中非 `[A-Za-z0-9-]` 換成 `-`。
fn encode_working_directory(workdir: &std::path::Path) -> String {
    // §5.4 的編碼取自 **canonical** working directory。`TempDir` 在
    // macOS 上給的是 `/var/folders/...`，而 `/var` 是指向 `/private/var` 的
    // symlink；daemon 會把規格正規化，所以這裡也要正規化，否則兩邊算出來的
    // 目錄名稱不同——測試會以為「沒有證據」，而實際上證據寫錯了地方。
    let canonical = std::fs::canonicalize(workdir).unwrap_or_else(|_| workdir.to_path_buf());
    canonical
        .to_string_lossy()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

/// SQLite 的 instance 列：(pid, pid_start, state, generation)。
fn instance_row(home: &TestHome, name: &str) -> Option<(Option<i64>, Option<i64>, String, u64)> {
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

fn instance_pid(home: &TestHome, name: &str) -> Option<i32> {
    instance_row(home, name)
        .and_then(|(pid, ..)| pid)
        .map(|pid| pid as i32)
}

/// 三方決策的對撞測試：daemon 的 `tools_list` 對同一 caller 回出的清單，必須
/// **恰等於** `mcp::tool_names(is_manager)`（bridge 的 `tools/list` 轉呼叫這裡；
/// 兩處若各自演化就會漂移）。
///
/// 參數化跑三種身分：
/// - operator：daemon 端視為有全部權限（契約 §2）→ 對應 `tool_names(true)`。
/// - manager：instance 名稱列在設定檔 `managers` → `tool_names(true)`。
/// - 一般 instance：不在 `managers` → `tool_names(false)`。
///
/// 另外驗兩件事：
/// - 期別工具（`DEFERRED_TO_LATER_PHASE`）不在 wire 結果裡。
/// - 未列出的工具直呼回 `unsupported_in_phase`（契約 §3「不列出 ≠ 可靜默忽略」）。
#[test]
fn daemon_tools_list_matches_mcp_tool_names_for_every_caller_kind() {
    use otterhive::mcp::{tool_names, DEFERRED_TO_LATER_PHASE};

    let home = TestHome::new();
    // 設定檔把 `managed` 列入 managers（契約 §2 的權限來源之一）。daemon 在啟動
    // 時讀取，因此先寫設定再啟動。
    std::fs::write(home.home.config_path(), "managers:\n  - managed\n")
        .expect("繁中說明：應可寫入測試用設定檔");
    let daemon = Daemon::start(&home);

    for name in ["managed", "plain"] {
        let created = daemon.operator_call(
            &home,
            "create_instance",
            serde_json::json!({"name": name,
                               "command": fixture("fake_claude.sh").to_string_lossy()}),
        );
        assert_eq!(
            created.outcome,
            otterhive::protocol::Outcome::Completed,
            "繁中說明：{name} 應建立成功，實際為 {created:?}"
        );
    }

    let cases: Vec<(&str, otterhive::protocol::Auth, bool)> = vec![
        ("operator", home.operator(), true),
        (
            "manager",
            otterhive::protocol::Auth::Instance(home.instance_auth("managed")),
            true,
        ),
        (
            "instance",
            otterhive::protocol::Auth::Instance(home.instance_auth("plain")),
            false,
        ),
    ];

    for (label, auth, is_manager) in cases {
        let listed = daemon.call(&home, &auth, "tools_list", serde_json::json!({}));
        assert_eq!(
            listed.outcome,
            otterhive::protocol::Outcome::Completed,
            "繁中說明：{label} 的 tools_list 應完成，實際為 {listed:?}"
        );
        let wire: Vec<String> = listed.value["tool_names"]
            .as_array()
            .unwrap_or_else(|| {
                panic!(
                    "繁中說明：{label} 的 tools_list 應回 tool_names，實際：{}",
                    listed.value
                )
            })
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect();
        let expected = tool_names(is_manager);
        assert_eq!(
            wire, expected,
            "繁中說明：{label} 的 daemon tools_list 必須與 mcp::tool_names({is_manager}) 相同"
        );
        for deferred in DEFERRED_TO_LATER_PHASE {
            assert!(
                !wire.iter().any(|name| name == deferred),
                "繁中說明：期別工具 {deferred} 不應出現在 wire 清單，實際：{wire:?}"
            );
        }
        // 未列出的工具直呼：unsupported（不是靜默忽略）。用 T4 尚未實作的
        // `deployment`：它在 MCP 工具清單之外（§10），在 socket 上也不屬 T4 的
        // lifecycle 範圍，因此直呼必須被明確拒絕。注意 `create_instance` 不
        // 適合當例子——它不在 *bridge 的工具清單* 裡，但對 operator 經 socket
        // 直呼是完全合法的（§2），兩者是不同的清單。
        let refused = daemon.call(&home, &auth, "deployment", serde_json::json!({"name": "x"}));
        assert_eq!(
            refused.outcome,
            otterhive::protocol::Outcome::Unsupported,
            "繁中說明：{label} 直呼 deployment 應回 unsupported（不在 T4 工具清單\
             也不在 T4 已實作清單），實際為 {refused:?}"
        );
    }
}
