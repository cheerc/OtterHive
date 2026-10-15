//! T4 Lane B RED：MCP bridge 的 stdio transport 與 lifecycle 工具（契約 §10）。
//!
//! 以 `env!("CARGO_BIN_EXE_otterhive-mcp-bridge")` 啟動**真的** bridge binary，
//! 走 stdio JSON Lines，不做 fixture 替身。bridge 的 HOME／TOKEN 前提由測試
//! 自己在子行程上設定；daemon 未啟動的暫存 home 即「連不上」的真實狀態。
//!
//! 兩類測試：
//! 1. transport：initialize／initialized／ping／未知方法／缺變數／daemon
//!    不可連（契約 §10 的六條）。
//! 2. lifecycle 工具：`tools_list` 的 T4 範圍（一般 instance 3 個、manager
//!    4 個）與 schema 欄位必須等於 catalog 的 `supported_fields ∪ added_fields`。
//!    這部分直接對 `otterhive::mcp` 的工具清單求值，不需啟動 daemon；daemon
//!    端的 `tools_list` 授權（誰看得到哪些）由 Lane A 的測試檔覆蓋。
//!
//! 全檔只在 macOS 編譯。所有等待有上限，且讀取 bridge 的阻塞 `read_line`
//! 一律放在獨立執行緒 + `recv_timeout`，橋接行程卡死時測試仍會以逾時失敗，
//! 不會掛住整個 suite。

#![cfg(target_os = "macos")]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use common::TempHome;
use otterhive::mcp::{tool_names, P1A_INSTANCE_TOOLS, P1A_PRIVILEGED_TOOLS};
use serde_json::{json, Value};

/// bridge 讀不到 daemon 時的 JSON-RPC 錯誤碼（契約 §10）。
const ERR_DAEMON_UNREACHABLE: i64 = -32001;
/// bridge 缺少 HOME／TOKEN 時的 JSON-RPC 錯誤碼（契約 §10）。
const ERR_CONFIG: i64 = -32000;
/// 未知方法的 JSON-RPC 標準碼。
const ERR_METHOD_NOT_FOUND: i64 = -32601;

/// 一般回應的等待上限。
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
/// 斷言「不該有回應」的觀察窗口。取小值是為了讓測試快；bridge 的回應是
/// 逐行同步寫出的，若它真的會回，這個窗口綽有餘裕。
const SILENCE_TIMEOUT: Duration = Duration::from_millis(400);
/// bridge 因前提缺失而退出的等待上限。
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// T5 之後一般 instance 看得到的工具（契約 §10，恰 6 個）。
const P1A_INSTANCE: [&str; 6] = [
    "list_instances",
    "restart_instance",
    "send",
    "inbox",
    "team",
    "pane_snapshot",
];
/// T5 之後 manager／orchestrator 看得到的工具（契約 §10，恰 7 個）。
const P1A_PRIVILEGED: [&str; 7] = [
    "list_instances",
    "restart_instance",
    "send",
    "inbox",
    "team",
    "pane_snapshot",
    "interrupt",
];
/// P1a 已實作但 `tools_list` 不列出的工具（契約 §10 明文）。
const NOT_LISTED_IN_P1A: [&str; 8] = [
    "create_instance",
    "delete_instance",
    "start_instance",
    "deployment",
    "decision",
    "task",
    "set_metadata",
    "schedule",
];

struct Bridge {
    child: Child,
    stdin: ChildStdin,
    reader: Option<std::thread::JoinHandle<()>>,
    rx: mpsc::Receiver<String>,
}

impl Bridge {
    /// 啟動 bridge binary。`extra` 用來刻意設定或省略 HOME／TOKEN。
    fn start(home: &Path, extra: &[(&str, &str)]) -> Bridge {
        let mut command = Command::new(env!("CARGO_BIN_EXE_otterhive-mcp-bridge"));
        // 不繼承測試 runner 的環境：bridge 的 HOME／TOKEN 前提必須由測試自己
        // 決定，否則「缺少變數」的分支取決於跑測試的機器。
        command.env_clear();
        command
            .env("HOME", home)
            .env("OTTERHIVE_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (key, value) in extra {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("繁中說明：bridge binary 應可啟動");
        let stdin = child.stdin.take().expect("繁中說明：bridge stdin 應可取得");
        let stdout = child
            .stdout
            .take()
            .expect("繁中說明：bridge stdout 應可取得");

        // 讀取端放在獨立執行緒：blocking read 不佔住測試主執行緒，
        // 因此「bridge 卡死」會變成逾時失敗而不是掛住 suite。
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut buffered = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match buffered.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Bridge {
            child,
            stdin,
            reader: Some(reader),
            rx,
        }
    }

    fn send_line(&mut self, message: &Value) {
        let line = format!("{message}\n");
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .expect("繁中說明：應可向 bridge 寫入一行 JSON-RPC");
    }

    /// 有上限地讀下一個非空行並解析成 JSON。
    fn read_line(&mut self, timeout: Duration) -> Result<Value, String> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(format!("{timeout:?} 內未讀到 bridge 回應"));
            }
            let line = match self.rx.recv_timeout(remaining) {
                Ok(line) => line,
                Err(_) => return Err(format!("{timeout:?} 內未讀到 bridge 回應")),
            };
            let text = line.trim();
            if text.is_empty() {
                continue;
            }
            return serde_json::from_str(text)
                .map_err(|error| format!("bridge 回應不是有效 JSON（{text:?}）：{error}"));
        }
    }

    fn call(&mut self, message: &Value) -> Value {
        self.send_line(message);
        self.read_line(RESPONSE_TIMEOUT)
            .unwrap_or_else(|reason| panic!("繁中說明：{reason}"))
    }

    /// 完成 initialize handshake（多數測試的前提）。
    fn handshake(&mut self) -> Value {
        let response = self.initialize();
        assert!(
            rpc_error(&response).is_none(),
            "繁中說明：前提齊全時 initialize 應成功，實際為 {response}"
        );
        self.send_line(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        response
    }

    fn initialize(&mut self) -> Value {
        self.call(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "otterhive-red-test", "version": "0"}
            }
        }))
    }

    /// 送一行之後觀察一小段時間，確認沒有回應。
    fn expect_no_reply(&mut self, what: &str) {
        if let Ok(line) = self.rx.recv_timeout(SILENCE_TIMEOUT) {
            panic!("繁中說明：{what} 不應有回應，但收到 {line:?}");
        }
    }
}

/// 有上限地等待行程結束；逾時即 kill 並 panic（不無限等待）。
#[track_caller]
fn wait_for_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) => {}
            Err(error) => panic!("繁中說明：等待 bridge 結束失敗：{error}"),
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("繁中說明：bridge 未在 {timeout:?} 內結束");
        }
        std::thread::sleep(common::POLL_STEP);
    }
}

impl Drop for Bridge {
    /// 收斂：不等讀取執行緒（它會隨 stdout 關閉而結束），只確保行程被回收。
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// 本測試使用的 protocolVersion；initialize 必須原樣回傳。
const PROTOCOL_VERSION: &str = "2025-06-18";

fn rpc_error(message: &Value) -> Option<&Value> {
    message.get("error")
}

fn rpc_result(message: &Value) -> Option<&Value> {
    message.get("result")
}

/// 斷言是 JSON-RPC 錯誤回應且錯誤碼相符；回傳錯誤物件。
fn assert_error_code(message: &Value, expected: i64, what: &str) -> Value {
    let error = rpc_error(message)
        .unwrap_or_else(|| panic!("繁中說明：{what} 應為 JSON-RPC 錯誤回應，實際為 {message}"));
    assert_eq!(
        error["code"].as_i64(),
        Some(expected),
        "繁中說明：{what} 的錯誤碼應為 {expected}，實際回應為 {message}"
    );
    error.clone()
}

/// 斷言錯誤訊息指名了缺少的環境變數（契約 §10）。
fn assert_error_names_variable(error: &Value, variable: &str, what: &str) {
    let message = error["message"]
        .as_str()
        .unwrap_or_else(|| panic!("繁中說明：{what} 的錯誤應有字串 message，實際為 {error}"));
    assert!(
        message.contains(variable),
        "繁中說明：{what} 的訊息應指出缺少 {variable}，實際為 {message:?}"
    );
}

/// catalog 中某個 P1a 工具的 `supported_fields ∪ added_fields`（排序後）。
fn catalog_schema_fields(catalog: &Value, name: &str) -> Vec<String> {
    let tools = catalog["tools"]
        .as_array()
        .expect("繁中說明：catalog tools 必須是陣列");
    let entry = tools
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("繁中說明：catalog 必須含工具 {name}"));
    assert_eq!(
        entry["phase"], "P1a",
        "繁中說明：{name} 在 catalog 中應為 P1a 工具，實際 phase 為 {}",
        entry["phase"]
    );
    let mut fields: Vec<String> = Vec::new();
    for key in ["supported_fields", "added_fields"] {
        for field in entry["p1a"][key]
            .as_array()
            .unwrap_or_else(|| panic!("繁中說明：{name}.{key} 必須是陣列"))
        {
            fields.push(
                field
                    .as_str()
                    .unwrap_or_else(|| panic!("繁中說明：{name}.{key} 的欄位必須是字串"))
                    .to_owned(),
            );
        }
    }
    fields.sort();
    fields.dedup();
    fields
}

fn load_catalog() -> Value {
    serde_json::from_str(include_str!("../docs/reverse-engineering/mcp-catalog.json"))
        .expect("繁中說明：遷移 catalog 必須是有效 JSON")
}

// ---------------------------------------------------------------- transport

#[test]
fn initialize_echoes_the_requested_protocol_version() {
    let home = TempHome::new("initialize_version");
    let mut bridge = Bridge::start(home.home(), &[("OTTERHIVE_INSTANCE_TOKEN", "token-wire")]);

    let response = bridge.initialize();
    assert_eq!(
        response["id"],
        json!(1),
        "繁中說明：initialize 回應的 id 應與請求相符，實際為 {response}"
    );
    assert!(
        rpc_error(&response).is_none(),
        "繁中說明：前提齊全時 initialize 應成功，實際為 {response}"
    );
    let result = rpc_result(&response).expect("initialize 應有 result");

    assert_eq!(
        result["protocolVersion"], PROTOCOL_VERSION,
        "繁中說明：initialize 應原樣回傳 client 要求的 protocolVersion，實際為 {result}"
    );
    assert_eq!(
        result["capabilities"]["tools"],
        json!({}),
        "繁中說明：capabilities.tools 應為空物件，實際為 {result}"
    );
    assert_eq!(
        result["serverInfo"]["name"], "otterhive",
        "繁中說明：serverInfo.name 應為 otterhive，實際為 {result}"
    );
    assert_eq!(
        result["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION"),
        "繁中說明：serverInfo.version 應為 crate 版本，實際為 {result}"
    );
}

#[test]
fn initialize_echoes_a_different_requested_protocol_version() {
    // 不是把某個版本寫死，而是「回 client 要的那個」：用不同版本再確認一次，
    // 避免實作只對 2025-06-18 生效。
    let home = TempHome::new("initialize_other_version");
    let mut bridge = Bridge::start(home.home(), &[("OTTERHIVE_INSTANCE_TOKEN", "token-wire")]);

    let response = bridge.call(&json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "initialize",
        "params": {
            "protocolVersion": "2099-01-01",
            "capabilities": {},
            "clientInfo": {"name": "otterhive-red-test", "version": "0"}
        }
    }));
    let result = rpc_result(&response)
        .unwrap_or_else(|| panic!("繁中說明：initialize 應成功，實際回應為 {response}"));
    assert_eq!(
        result["protocolVersion"], "2099-01-01",
        "繁中說明：initialize 應回傳請求的 protocolVersion 而非固定值，實際為 {result}"
    );
}

#[test]
fn initialized_notification_gets_no_reply() {
    let home = TempHome::new("initialized_silent");
    let mut bridge = Bridge::start(home.home(), &[("OTTERHIVE_INSTANCE_TOKEN", "token-wire")]);
    bridge.handshake();

    bridge.expect_no_reply("notifications/initialized");

    // 緊接著的 ping 仍要有回應：確認上面的沉默不是 bridge 卡死。
    let response = bridge.call(&json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}));
    assert_eq!(
        response["id"],
        json!(7),
        "繁中說明：bridge 仍應回應後續請求，實際為 {response}"
    );
}

#[test]
fn ping_returns_an_empty_object() {
    let home = TempHome::new("ping_empty");
    let mut bridge = Bridge::start(home.home(), &[("OTTERHIVE_INSTANCE_TOKEN", "token-wire")]);
    bridge.handshake();

    let response = bridge.call(&json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}));
    let result = rpc_result(&response)
        .unwrap_or_else(|| panic!("繁中說明：ping 應有 result，實際回應為 {response}"));
    assert_eq!(
        *result,
        json!({}),
        "繁中說明：ping 應回空物件（契約 §10），實際為 {response}"
    );
}

#[test]
fn unknown_method_is_method_not_found() {
    let home = TempHome::new("unknown_method");
    let mut bridge = Bridge::start(home.home(), &[("OTTERHIVE_INSTANCE_TOKEN", "token-wire")]);
    bridge.handshake();

    for method in [
        "resources/list",
        "tools/invoke",
        "shutdown",
        "completion/complete",
    ] {
        let response = bridge.call(&json!({"jsonrpc": "2.0", "id": 11, "method": method}));
        assert_error_code(
            &response,
            ERR_METHOD_NOT_FOUND,
            &format!("未知方法 {method}"),
        );
    }
}

#[test]
fn missing_instance_token_is_config_error_on_initialize() {
    let home = TempHome::new("missing_token");
    // HOME 明確存在、TOKEN 刻意不設：訊息必須指出缺的是哪一個。
    let mut bridge = Bridge::start(home.home(), &[]);

    let response = bridge.initialize();
    let error = assert_error_code(&response, ERR_CONFIG, "缺少 OTTERHIVE_INSTANCE_TOKEN");
    assert_error_names_variable(&error, "OTTERHIVE_INSTANCE_TOKEN", "缺少 token");
}

#[test]
fn missing_home_is_config_error_on_initialize() {
    // 刻意不設 OTTERHIVE_HOME：契約 §1 的 `~/.otterhive` 預設值必須在
    // bridge 層就被拒收，不得默默落到某個正式 home。
    let home = TempHome::new("missing_home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_otterhive-mcp-bridge"));
    command
        .env_clear()
        .env("HOME", home.home())
        .env("OTTERHIVE_INSTANCE_TOKEN", "token-wire")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().expect("繁中說明：bridge binary 應可啟動");
    let mut stdin = child.stdin.take().expect("bridge stdin 應可取得");
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().expect("bridge stdout 應可取得");
    let reader = std::thread::spawn(move || {
        let mut buffered = BufReader::new(stdout);
        let mut line = String::new();
        let _ = buffered.read_line(&mut line);
        let _ = tx.send(line);
    });

    let request = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "otterhive-red-test", "version": "0"}
        }
    });
    stdin
        .write_all(format!("{request}\n").as_bytes())
        .expect("應可向 bridge 寫入 initialize");
    stdin.flush().expect("應可 flush bridge stdin");

    let line = rx
        .recv_timeout(RESPONSE_TIMEOUT)
        .expect("繁中說明：bridge 應對 initialize 回一行");
    let response: Value = serde_json::from_str(line.trim())
        .unwrap_or_else(|error| panic!("bridge 回應應為 JSON（{line:?}）：{error}"));
    let error = assert_error_code(&response, ERR_CONFIG, "缺少 OTTERHIVE_HOME");
    assert_error_names_variable(&error, "OTTERHIVE_HOME", "缺少 HOME");

    // 前提缺失不得靜默存活：回完錯誤就該結束，讓 backend 看見失敗。
    let status = wait_for_exit(&mut child, EXIT_TIMEOUT);
    assert!(
        status.success(),
        "繁中說明：前提缺失的 bridge 應正常結束而非留下失敗行程，實際為 {status:?}"
    );
    let _ = reader.join();
}

#[test]
fn tools_list_is_daemon_unreachable_when_no_daemon_is_running() {
    let home = TempHome::new("tools_list_offline");
    let mut bridge = Bridge::start(home.home(), &[("OTTERHIVE_INSTANCE_TOKEN", "token-wire")]);
    bridge.handshake();

    let response = bridge.call(&json!({"jsonrpc": "2.0", "id": 20, "method": "tools/list"}));
    let error = assert_error_code(&response, ERR_DAEMON_UNREACHABLE, "tools/list");
    let message = error["message"].as_str().unwrap_or_default();
    assert!(
        !message.is_empty(),
        "繁中說明：daemon 不可連的錯誤應有說明，實際為 {error}"
    );
    // 不得偽裝成空工具清單：連不上就說連不上，不假裝成功（契約 §10）。
    assert!(
        rpc_result(&response).is_none(),
        "繁中說明：daemon 不可連應是 JSON-RPC 錯誤而非 result，實際為 {response}"
    );
}

#[test]
fn tools_call_is_daemon_unreachable_when_no_daemon_is_running() {
    let home = TempHome::new("tools_call_offline");
    let mut bridge = Bridge::start(home.home(), &[("OTTERHIVE_INSTANCE_TOKEN", "token-wire")]);
    bridge.handshake();

    let response = bridge.call(&json!({
        "jsonrpc": "2.0",
        "id": 21,
        "method": "tools/call",
        "params": {"name": "list_instances", "arguments": {}}
    }));
    assert_error_code(&response, ERR_DAEMON_UNREACHABLE, "tools/call");
    assert!(
        rpc_result(&response).is_none(),
        "繁中說明：daemon 不可連應是 JSON-RPC 錯誤而非工具結果，實際為 {response}"
    );
}

#[test]
fn daemon_unreachable_is_reported_for_every_calling_method() {
    // 契約 §10：無法連到 daemon 時，`tools/list`／`tools/call` 都回 -32001。
    // 這裡對 T4 列出的每個 lifecycle 工具各送一次 tools/call。
    let home = TempHome::new("tools_call_all_offline");
    let mut bridge = Bridge::start(home.home(), &[("OTTERHIVE_INSTANCE_TOKEN", "token-wire")]);
    bridge.handshake();

    for (index, tool) in P1A_PRIVILEGED.iter().enumerate() {
        let id = 100 + index as i64;
        let response = bridge.call(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": {"instance": "worker"}}
        }));
        assert_error_code(
            &response,
            ERR_DAEMON_UNREACHABLE,
            &format!("tools/call {tool}"),
        );
    }
}

// ------------------------------------------------- lifecycle 工具（registry）

#[test]
fn lists_exactly_six_tools_for_an_instance() {
    // 契約 §10：instance 一律看得到 list_instances／restart_instance／send／
    // inbox／team／pane_snapshot。取代 T4 的「只列三個 lifecycle 工具」。
    let names = tool_names(false);
    assert_eq!(
        names, P1A_INSTANCE,
        "繁中說明：一般 instance 的工具清單應恰為契約 §10 的 6 個"
    );
}

#[test]
fn adds_interrupt_only_for_a_manager_or_orchestrator() {
    let names = tool_names(true);
    assert_eq!(
        names, P1A_PRIVILEGED,
        "繁中說明：manager 的工具清單應恰為契約 §10 的 7 個"
    );
}

#[test]
fn lists_the_three_tools_whose_handlers_landed_in_t5() {
    // T4 時 send／inbox／team 的 handler 未落地，因此不在清單裡；T5 落地後
    // 三個都必須看得到（契約 §10 明文點名）。
    let mut listed = tool_names(false);
    listed.sort();
    for tool in ["send", "inbox", "team"] {
        assert!(
            listed.contains(&tool.to_owned()),
            "繁中說明：{tool} 的 handler 已在 T5 落地，一般 instance 應看得到"
        );
    }
}

#[test]
fn does_not_list_create_delete_start_or_deployment() {
    // 契約 §10：create／delete／start／deployment 不列出。
    let mut listed = tool_names(true);
    listed.sort();
    for tool in [
        "create_instance",
        "delete_instance",
        "start_instance",
        "deployment",
    ] {
        assert!(
            !listed.contains(&tool.to_owned()),
            "繁中說明：{tool} 不應出現在 T4 的工具清單"
        );
    }
}

#[test]
fn the_p1a_tool_set_is_pinned_by_the_contract() {
    // 把模組公開的契約清單與測試認定的清單綁在一起，避免兩處各自漂移。
    assert_eq!(
        P1A_INSTANCE_TOOLS, P1A_INSTANCE,
        "繁中說明：模組公開的一般 instance 清單應等於測試認定的契約集合"
    );
    assert_eq!(
        P1A_PRIVILEGED_TOOLS, P1A_PRIVILEGED,
        "繁中說明：模組公開的 manager 清單應等於測試認定的契約集合"
    );
    assert_eq!(
        P1A_INSTANCE.len(),
        6,
        "繁中說明：契約 §10 的一般 instance 工具數是 6"
    );
    assert_eq!(
        P1A_PRIVILEGED.len(),
        7,
        "繁中說明：契約 §10 的 manager／orchestrator 工具數是 7"
    );
}

#[test]
fn every_not_listed_tool_is_absent_for_both_caller_kinds() {
    // 「不列出」對兩種 caller 都成立：manager 多一個 interrupt，不代表
    // create／delete／start／deployment 就浮出來。
    for absent in NOT_LISTED_IN_P1A {
        for is_manager in [false, true] {
            assert!(
                !tool_names(is_manager).contains(&absent.to_owned()),
                "繁中說明：{absent} 對 is_manager={is_manager} 都不應被列出（契約 §10）"
            );
        }
    }
}

#[test]
fn does_not_list_any_non_p1a_tool() {
    // 契約 §10：「所有非 P1a 工具不列出」。`NOT_LISTED_IN_P1A` 裡的後半段
    // （decision／task／set_metadata／schedule）屬於 catalog 的後續期別。
    let mut listed = tool_names(true);
    listed.sort();
    for tool in ["decision", "task", "set_metadata", "schedule"] {
        assert!(
            !listed.contains(&tool.to_owned()),
            "繁中說明：非 P1a 工具 {tool} 不應出現在工具清單"
        );
    }
}

#[test]
fn schema_properties_equal_the_catalog_supported_and_added_fields() {
    let catalog = load_catalog();
    for tool in P1A_PRIVILEGED {
        let expected = catalog_schema_fields(&catalog, tool);
        let actual = otterhive::mcp::input_schema_fields(tool);
        assert_eq!(
            actual, expected,
            "繁中說明：{tool} 的 input schema 欄位應恰為 catalog 的 supported_fields ∪ added_fields"
        );
    }
}

#[test]
fn schema_omits_every_refused_field() {
    // 契約 §10：schema 只含支援的欄位；舊版有但 P1a 拒收的欄位不得出現在
    // schema 裡，否則 backend 會照著 schema 送、`admission` 再拒一次。
    let catalog = load_catalog();
    for tool in P1A_PRIVILEGED {
        let entry = catalog["tools"]
            .as_array()
            .expect("catalog tools 必須是陣列")
            .iter()
            .find(|entry| entry["name"] == tool)
            .unwrap_or_else(|| panic!("catalog 必須含 {tool}"));
        let refused: Vec<&str> = entry["p1a"]["refused_fields"]
            .as_array()
            .expect("refused_fields 必須是陣列")
            .iter()
            .map(|field| field.as_str().expect("拒絕欄位必須是字串"))
            .collect();
        let fields = otterhive::mcp::input_schema_fields(tool);
        for field in refused {
            assert!(
                !fields.iter().any(|listed| listed == field),
                "繁中說明：{tool} 的 schema 不應含拒收欄位 {field}"
            );
        }
    }
}

#[test]
fn interrupt_schema_is_only_the_instance_field() {
    // 目視可查的契約細節：`interrupt` 只接受 instance（reason／snapshot
    // 在 P1a 拒收）。這條寫死是為了讓 schema 漂移立刻被看見。
    assert_eq!(
        otterhive::mcp::input_schema_fields("interrupt"),
        vec!["instance".to_owned()],
        "繁中說明：interrupt 的 schema 應只含 instance"
    );
}

#[test]
fn restart_schema_keeps_expected_generation_because_it_is_added_in_p1a() {
    let catalog = load_catalog();
    let expected = catalog_schema_fields(&catalog, "restart_instance");
    let actual = otterhive::mcp::input_schema_fields("restart_instance");
    assert_eq!(
        actual, expected,
        "restart_instance 的 schema 應等於 catalog 欄位"
    );
    assert!(
        actual.contains(&"expected_generation".to_owned()),
        "繁中說明：restart_instance 的 schema 應含 P1a 新增的 expected_generation，實際為 {actual:?}"
    );
    assert!(
        !actual.contains(&"force".to_owned()),
        "繁中說明：restart_instance 的 schema 不應含 P1a 拒收的 force"
    );
}

// ------------------------------------------------- refused 與 isError（§10）

/// daemon 在線時，`tools/call` 的工具結果一律是
/// `{content: [{type:"text", text: <OperationResult JSON>}], isError}`；
/// `isError` 只在 `refused`／`unsupported` 時為 `true`（契約 §10）。
///
/// 本檔的 bridge harness 啟動時**不**帶 daemon，因此線上行為走不到。
/// 這裡驗的是 bridge 端形狀不變的那一層：`-32001` 是 JSON-RPC 錯誤、沒有
/// `isError`；一旦有人在 bridge 裡把「daemon 不可連」改寫成
/// `isError: true` 的工具結果，就會被這個測試擋下。
#[test]
fn daemon_unreachable_is_an_rpc_error_not_a_tool_result_with_is_error() {
    let home = TempHome::new("is_error_shape");
    let mut bridge = Bridge::start(home.home(), &[("OTTERHIVE_INSTANCE_TOKEN", "token-wire")]);
    bridge.handshake();

    let response = bridge.call(&json!({
        "jsonrpc": "2.0",
        "id": 30,
        "method": "tools/call",
        "params": {"name": "send", "arguments": {"message": "hi"}}
    }));
    assert_error_code(&response, ERR_DAEMON_UNREACHABLE, "send");
    assert!(
        rpc_result(&response).is_none(),
        "繁中說明：daemon 不可連不得偽裝成工具結果（契約 §10「不假裝成功」），實際為 {response}"
    );
    assert!(
        response.get("result").is_none() || response["result"].get("isError").is_none(),
        "繁中說明：JSON-RPC 錯誤層沒有 isError，實際為 {response}"
    );
}

/// `unsupported` 的工具／欄位在 admission 層就回 `unsupported_in_phase`
/// （契約 §3），因此 `isError: true` 的內容必須帶著可行動的訊息。
///
/// 這裡以 `admission` 對**非 P1a 工具**做一次端到端形狀檢查：訊息必須同時
/// 指出被拒的工具、並說明「P1a 不支援」，讓 backend 讀到 `isError` 的內容
/// 時知道下一步是什麼，而不是只看到一個失敗旗標。
#[test]
fn an_unsupported_tool_result_carries_a_message_naming_it_and_the_phase() {
    let error = otterhive::admission::admit("decision", &json!({}))
        .expect_err("繁中說明：非 P1a 工具應被 admission 拒絕");
    assert_eq!(
        error.code,
        otterhive::protocol::ErrorCode::UnsupportedInPhase,
        "繁中說明：decision 應回 unsupported_in_phase（契約 §3）"
    );
    assert!(
        error.message.contains("P1a 不支援"),
        "繁中說明：訊息應說明 P1a 不支援，實際為 {:?}",
        error.message
    );
    assert!(
        error.message.contains("decision"),
        "繁中說明：訊息應指出被拒的工具，實際為 {:?}",
        error.message
    );
    assert!(
        !error.retryable,
        "繁中說明：同一請求永遠同樣被拒，重試無意義（契約 §3）"
    );
}

/// 契約 §10 明文：「`create_instance`、`delete_instance`、
/// `start_instance`、`deployment` 與所有非 P1a 工具不列出；**直接呼叫依 §2、
/// §3 拒絕**」。
///
/// 因此「不列出」必須配對「直接呼叫不沉默」。`admission` 對這四個工具的
/// 形狀檢查是**通過**的（它們在 catalog 中是 P1a 工具，規格存在），所以這
/// 條斷言它們**不得**因為形狀合法就被當成可呼叫——判斷必須發生在 daemon
/// 端的 handler。
#[test]
fn unlisted_p1a_tools_pass_shape_but_are_not_callable() {
    for (tool, args) in [
        ("create_instance", json!({"name": "worker"})),
        ("delete_instance", json!({"instance": "worker"})),
        ("start_instance", json!({"instance": "worker"})),
        ("deployment", json!({"action": "list"})),
    ] {
        assert!(
            otterhive::admission::admit(tool, &args).is_ok(),
            "繁中說明：{tool} 是 catalog 中的 P1a 工具，形狀檢查應通過"
        );
        assert!(
            !tool_names(true).contains(&tool.to_owned()),
            "繁中說明：{tool} 不得出現在工具清單（契約 §10）"
        );
        assert!(
            !tool_names(false).contains(&tool.to_owned()),
            "繁中說明：{tool} 對一般 instance 也不得出現在工具清單"
        );
    }
}
