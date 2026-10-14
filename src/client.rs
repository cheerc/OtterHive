//! T4 Lane B RED：socket client 的**凍結介面**骨架（無行為）。
//!
//! 這是 test-only 的空骨架：型別與簽章先固定，讓 GREEN 有明確的目標，GREEN
//! 之前呼叫端會因「回傳值永遠是 placeholder」而失敗——這是刻意的 compile-RED。
//!
//! 凍結的介面（plan T4 Interfaces 與契約 §1／§3）：
//! - 一行 JSON 的 `Request` → 一行 JSON 的 `OperationResult`。
//! - 一個連線一個請求；`attach` 是例外（§7，本期不在此介面）。
//! - 連不上 daemon 與請求格式錯誤都回 `ClientError`，不假裝成功。
//!
//! 授權由 daemon 依 `auth` 內的 token 判定；client 只負責送出與讀回應。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::protocol::{Auth, OperationResult, Request};

/// `run/daemon.sock` 的檔名（契約 §1）。
pub const SOCKET_FILE: &str = "daemon.sock";

/// client 連線與讀回應的上限。daemon 不回應時必須逾時，不得無限等待。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// client 層的失敗。兩種成因分開，讓 CLI 能給不同 exit code（契約 §9）。
#[derive(Debug)]
pub enum ClientError {
    /// 無法連到 daemon（socket 不存在、連線被拒、逾時未回應）。
    Unreachable(String),
    /// 請求或回應的線路格式錯誤（不是合法 JSON、欄位缺漏等）。
    Malformed(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Unreachable(reason) => {
                write!(formatter, "無法連到 daemon：{reason}")
            }
            ClientError::Malformed(reason) => {
                write!(formatter, "請求或回應格式錯誤：{reason}")
            }
        }
    }
}

impl std::error::Error for ClientError {}

/// 一次 client 呼叫所需的全部輸入。
///
/// CLI、bridge 與測試共用同一組參數，避免三處各自拼 socket 路徑與逾時。
#[derive(Debug, Clone)]
pub struct CallOptions {
    /// `OTTERHIVE_HOME` 或 CLI 的 `--home`；決定 socket 路徑。
    pub home: PathBuf,
    /// 呼叫端身分（契約 §1：`{"operator":…}` 或 `{"instance":…}`）。
    pub auth: Auth,
    /// 操作名稱（與 MCP 工具同名，另有只經 socket 的 `attach`／`tools_list`）。
    pub op: String,
    /// 操作參數。
    pub args: Value,
    /// 可選的 `operation_id`；`None` 由 daemon 產生。
    pub operation_id: Option<String>,
    /// 連線與讀回應的上限。
    pub timeout: Duration,
}

impl CallOptions {
    /// 以預設逾時建立呼叫選項。
    pub fn new(home: &Path, auth: Auth, op: &str, args: Value) -> Self {
        Self {
            home: home.to_path_buf(),
            auth,
            op: op.to_owned(),
            args,
            operation_id: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// 指定 `operation_id`（契約 §3：同一呼叫端重送同一 id 回第一次的結果）。
    pub fn with_operation_id(mut self, operation_id: &str) -> Self {
        self.operation_id = Some(operation_id.to_owned());
        self
    }

    /// 送出前的請求形狀（單行 JSON，`v` 固定為 1）。
    pub fn request(&self) -> Request {
        Request {
            v: 1,
            auth: self.auth.clone(),
            op: self.op.clone(),
            args: self.args.clone(),
            operation_id: self.operation_id.clone(),
        }
    }
}

/// 該 home 的 daemon socket 路徑（`$OTTERHIVE_HOME/run/daemon.sock`）。
pub fn socket_path(home: &Path) -> PathBuf {
    home.join("run").join(SOCKET_FILE)
}

/// 送出一個操作並讀回 `OperationResult`。
///
/// GREEN 之前這是佔位實作：**永遠回 `Unreachable`**。這是刻意的——
/// Lane A 的 daemon 端與本檔的 client 端會同時是 compile-RED，
/// 由 T4 GREEN 一次把兩邊接起來。不可為了讓某個測試變綠而在這裡塞假回應。
pub fn call(_options: &CallOptions) -> Result<OperationResult, ClientError> {
    Err(ClientError::Unreachable(
        "client 尚未實作（T4 GREEN 補上 socket 連線與讀回應）".to_owned(),
    ))
}
