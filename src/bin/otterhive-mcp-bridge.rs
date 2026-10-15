//! T4 Lane B：`otterhive-mcp-bridge` 的 stdio transport（契約 §10）。
//!
//! 由 backend 以 stdio 啟動。每行一則 JSON-RPC 2.0 訊息：
//! - `initialize`：回 client 要求的 `protocolVersion`、`capabilities: {"tools": {}}`、
//!   `serverInfo`。
//! - `notifications/initialized`：不回應。
//! - `ping`：回 `{}`。
//! - `tools/list`／`tools/call`：轉呼叫 daemon 的 `tools_list`／同名操作。
//! - 其他方法回 `-32601`。
//!
//! 兩條不可混淆的失敗（契約 §10「不假裝成功」）：
//! - 缺 `OTTERHIVE_HOME` 或 `OTTERHIVE_INSTANCE_TOKEN` → `-32000`，message
//!   指出缺哪個變數。兩個都缺時兩者都要指出。
//! - 連不上 daemon → `-32001`，且**不是**工具結果。
//!
//! 呼叫端身分由環境變數的 token 推導，不從 `tools/call` 的參數讀（契約 §2）。
//! `tools/call` 的結果是一個 `type: "text"` 的 content，內容是
//! `OperationResult` 的 JSON；outcome 為 `refused`／`unsupported` 時
//! `isError: true`。

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use otterhive::client::{self, CallOptions};
use otterhive::protocol::Auth;

/// JSON-RPC 標準錯誤碼。
mod code {
    /// 解析錯誤（不是合法 JSON）。
    pub const PARSE_ERROR: i64 = -32700;
    /// 請求物件形狀不合法。
    pub const INVALID_REQUEST: i64 = -32600;
    /// 未知方法。
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// 缺少 `OTTERHIVE_HOME`／`OTTERHIVE_INSTANCE_TOKEN`。
    pub const CONFIG: i64 = -32000;
    /// 連不上 daemon。
    pub const DAEMON_UNREACHABLE: i64 = -32001;
}

/// 一行 JSON-RPC 訊息。
///
/// `id` 缺席即為 notification（不回應）。`method` 與 `params` 都是選項——
/// 回應只有 `id` 與 `result`／`error`。
#[derive(Debug, serde::Deserialize)]
struct Message {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Value,
}

/// bridge 由環境變數取得的前提。
struct Config {
    home: std::path::PathBuf,
    token: String,
}

fn main() {
    let config = match read_config() {
        Ok(config) => config,
        Err(missing) => {
            // 前提缺失：initialize 回 -32000 並結束，不得帶著殘缺前提繼續服務
            // （backend 需要看見失敗，否則會以為 bridge 可用）。
            let mut stdout = std::io::stdout();
            let _ = writeln!(
                stdout,
                "{}",
                error_response(
                    &json!(1),
                    code::CONFIG,
                    &format!("bridge 缺少必要的環境變數：{}", missing.join("、"))
                )
            );
            let _ = stdout.flush();
            return;
        }
    };

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let message: Message = match serde_json::from_str(text) {
            Ok(message) => message,
            Err(error) => {
                // 壞掉的行不得中斷 bridge（backend 可能只是多送了一行 log）。
                let _ = writeln!(
                    stdout,
                    "{}",
                    error_response(
                        &Value::Null,
                        code::PARSE_ERROR,
                        &format!("不是合法的 JSON：{error}")
                    )
                );
                let _ = stdout.flush();
                continue;
            }
        };
        if let Some(response) = handle(&message, &config) {
            if writeln!(stdout, "{response}").is_err() {
                break;
            }
            if stdout.flush().is_err() {
                break;
            }
        }
    }
}

/// 讀 `OTTERHIVE_HOME` 與 `OTTERHIVE_INSTANCE_TOKEN`；缺任何一個回缺少的變數名稱。
///
/// 刻意**不**採用契約 §1 的 `~/.otterhive` 預設值：bridge 由 backend 啟動，
/// 必須明確知道自己在服務哪個 home，猜錯會讓 agent 操作到別的 fleet。
fn read_config() -> Result<Config, Vec<String>> {
    let mut missing = Vec::new();
    let home = std::env::var("OTTERHIVE_HOME").ok();
    if home.is_none() {
        missing.push("OTTERHIVE_HOME".to_owned());
    }
    let token = std::env::var("OTTERHIVE_INSTANCE_TOKEN").ok();
    if token.is_none() {
        missing.push("OTTERHIVE_INSTANCE_TOKEN".to_owned());
    }
    match (home, token) {
        (Some(home), Some(token)) => Ok(Config {
            home: std::path::PathBuf::from(home),
            token,
        }),
        _ => Err(missing),
    }
}

/// 處理一則訊息；`None` 表示這是 notification，不回應。
fn handle(message: &Message, config: &Config) -> Option<Value> {
    // notification（沒有 id）一律不回應——即使方法未知。
    let id = message.id.clone()?;
    let method = message.method.clone().unwrap_or_default();

    match method.as_str() {
        "initialize" => Some(success(&id, initialize_result(&message.params))),
        "ping" => Some(success(&id, json!({}))),
        "tools/list" => Some(tools_list(&id, config)),
        "tools/call" => Some(tools_call(&id, config, &message.params)),
        _ => Some(error_response(
            &id,
            code::METHOD_NOT_FOUND,
            &format!("未知的 JSON-RPC 方法：{method}"),
        )),
    }
}

/// `initialize` 的結果：原樣回 client 要求的 protocolVersion。
fn initialize_result(params: &Value) -> Value {
    let requested = params["protocolVersion"].as_str().unwrap_or_default();
    json!({
        "protocolVersion": requested,
        "capabilities": {"tools": {}},
        "serverInfo": {
            "name": "otterhive",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

fn tools_list(id: &Value, config: &Config) -> Value {
    // 先問 daemon 這位 caller 看得到什麼；daemon 不可連就回 -32001，
    // 不用本地副本冒充（契約 §10）。
    match daemon_call(config, "tools_list", json!({})) {
        Ok(result) => success(id, result),
        Err(reason) => error_response(id, code::DAEMON_UNREACHABLE, &reason),
    }
}

fn tools_call(id: &Value, config: &Config, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or_default().to_owned();
    if name.is_empty() {
        return error_response(id, code::INVALID_REQUEST, "tools/call 缺少 name 參數");
    }
    let args = match params.get("arguments") {
        Some(Value::Null) | None => json!({}),
        Some(value) => value.clone(),
    };

    // 呼叫端身分由 token 推導（契約 §2）：instance token 不帶 InstanceId，
    // 因此這裡送的是 instance 身分本身，由 daemon 依 token 對照資料庫。
    match daemon_call(config, &name, args) {
        Ok(result) => {
            // refused／unsupported 是「操作被拒」，仍是一個成功的工具回應，
            // 只是帶 isError（契約 §10）。
            let outcome = result["outcome"].as_str().unwrap_or_default();
            let is_error = outcome == "refused" || outcome == "unsupported";
            success(
                id,
                json!({
                    "content": [{"type": "text", "text": result.to_string()}],
                    "isError": is_error,
                }),
            )
        }
        Err(reason) => error_response(id, code::DAEMON_UNREACHABLE, &reason),
    }
}

/// 轉呼叫 daemon；連不上回可讀的原因。
fn daemon_call(config: &Config, op: &str, args: Value) -> Result<Value, String> {
    let options = CallOptions::new(&config.home, Auth::Instance(config.token.clone()), op, args);
    match client::call(&options) {
        Ok(result) => Ok(serde_json::to_value(result)
            .map_err(|error| format!("daemon 回應無法序列化：{error}"))?),
        Err(error) => Err(error.to_string()),
    }
}

fn success(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
}
