//! T4 Lane B：socket client（契約 §1／§3）。
//!
//! 一行 JSON 的 `Request` → 一行 JSON 的 `OperationResult`：
//! `run/daemon.sock` 上送一行、讀一行。授權由 daemon 依 `auth` 內的 token
//! 判定；client 只負責送出與讀回應。
//!
//! 兩種失敗分開回報，讓 CLI 能給不同的 exit code（契約 §9）：
//! - [`ClientError::Unreachable`]：連不上 daemon（socket 不存在、連線被拒、
//!   逾時未回應）。exit 1。
//! - [`ClientError::Malformed`]：請求或回應的線路格式錯誤。exit 1。
//!
//! 逾時一律有上限：`read` 設 `SO_RCVTIMEO`、期限以 `Instant` 計時，不因
//! daemon 不回應而無限等待（契約 §9）。

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::protocol::{Auth, OperationResult, Request};

/// `run/daemon.sock` 的檔名（契約 §1）。
pub const SOCKET_FILE: &str = "daemon.sock";

/// client 連線與讀回應的上限。daemon 不回應時必須逾時，不因 Slack 而無限等待。
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
            ClientError::Unreachable(reason) => write!(formatter, "無法連到 daemon：{reason}"),
            ClientError::Malformed(reason) => write!(formatter, "請求或回應格式錯誤：{reason}"),
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

    /// 覆寫逾時。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
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
pub fn call(options: &CallOptions) -> Result<OperationResult, ClientError> {
    let path = socket_path(&options.home);
    let deadline = Instant::now() + options.timeout;

    let mut stream = UnixStream::connect(&path)
        .map_err(|error| ClientError::Unreachable(format!("{}：{error}", path.display())))?;
    let _ = stream.set_read_timeout(Some(options.timeout));
    let _ = stream.set_write_timeout(Some(options.timeout));

    let line = serde_json::to_string(&options.request())
        .map_err(|error| ClientError::Malformed(format!("請求無法序列化：{error}")))?;
    let mut payload = line.into_bytes();
    payload.push(b'\n');
    stream
        .write_all(&payload)
        .and_then(|()| stream.flush())
        .map_err(|error| ClientError::Unreachable(format!("寫入 socket 失敗：{error}")))?;

    // 一行回應；daemon 之後關閉連線（§1：一個連線一個請求）。讀到 newline
    // 或 EOF 都算一行完整，不假設 daemon 會關閉連線。
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ClientError::Unreachable(format!(
                "{:?} 內未讀到 daemon 回應",
                options.timeout
            )));
        }
        let _ = stream.set_read_timeout(Some(remaining));
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.contains(&b'\n') {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(ClientError::Unreachable(format!(
                    "{:?} 內未讀到 daemon 回應",
                    options.timeout
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(ClientError::Unreachable(format!(
                    "讀取 socket 失敗：{error}"
                )))
            }
        }
    }

    parse_result(&buffer)
}

/// 由回應的位元組解析 `OperationResult`。
///
/// daemon 回的是**一行** JSON；多餘的空白與換行忽略，回應內部的換行（若
/// daemon 的 JSON 含有轉義的換行）由 `serde_json` 處理。
fn parse_result(buffer: &[u8]) -> Result<OperationResult, ClientError> {
    let text = String::from_utf8_lossy(buffer);
    let line = text.trim();
    if line.is_empty() {
        return Err(ClientError::Unreachable(
            "daemon 關閉連線但沒有回應".to_owned(),
        ));
    }
    serde_json::from_str(line)
        .map_err(|error| ClientError::Malformed(format!("回應不是有效的 OperationResult：{error}")))
}

#[cfg(test)]
mod tests {
    use super::{parse_result, socket_path, CallOptions, ClientError};
    use crate::protocol::Auth;
    use std::path::Path;

    #[test]
    fn socket_path_is_under_the_run_directory() {
        assert_eq!(
            socket_path(Path::new("/tmp/h")),
            Path::new("/tmp/h/run/daemon.sock"),
            "繁中說明：socket 路徑應為 $OTTERHIVE_HOME/run/daemon.sock"
        );
    }

    #[test]
    fn request_carries_version_one_and_the_given_auth() {
        let options = CallOptions::new(
            Path::new("/tmp/h"),
            Auth::Operator("tok".to_owned()),
            "list_instances",
            serde_json::json!({}),
        );
        let request = options.request();
        assert_eq!(request.v, 1, "繁中說明：線路版本固定為 1");
        assert_eq!(request.auth, Auth::Operator("tok".to_owned()));
        assert_eq!(request.op, "list_instances");
        assert!(request.operation_id.is_none());
    }

    #[test]
    fn operation_id_is_carried_when_given() {
        let options = CallOptions::new(
            Path::new("/tmp/h"),
            Auth::Operator("t".to_owned()),
            "create_instance",
            serde_json::json!({"name": "w"}),
        )
        .with_operation_id("op-1");
        assert_eq!(options.request().operation_id.as_deref(), Some("op-1"));
    }

    #[test]
    fn a_result_line_is_parsed() {
        let line = br#"{"operation_id":"op-1","outcome":"completed","subject":null,"value":{},"effects":[],"error":null}"#;
        let result = parse_result(line).expect("繁中說明：合法的 OperationResult 應解析成功");
        assert_eq!(result.outcome, crate::protocol::Outcome::Completed);
    }

    #[test]
    fn trailing_newline_and_spaces_are_tolerated() {
        let line = b"  {\"operation_id\":\"x\",\"outcome\":\"queued\",\"subject\":null,\"value\":null,\"effects\":[],\"error\":null}  \n";
        let result = parse_result(line).expect("應可解析");
        assert_eq!(result.outcome, crate::protocol::Outcome::Queued);
    }

    #[test]
    fn garbage_is_malformed_not_unreachable() {
        let error = parse_result(b"not json at all").expect_err("應回錯誤");
        assert!(
            matches!(error, ClientError::Malformed(_)),
            "繁中說明：回應不是 JSON 屬格式錯誤（exit 1），不是連線問題，實際為 {error:?}"
        );
    }

    #[test]
    fn an_empty_response_is_unreachable() {
        let error = parse_result(b"").expect_err("應回錯誤");
        assert!(
            matches!(error, ClientError::Unreachable(_)),
            "繁中說明：daemon 關閉連線但沒回應屬連線失敗，實際為 {error:?}"
        );
    }

    #[test]
    fn a_missing_socket_is_unreachable() {
        // 暫存 home 底下不可能有 daemon：連線必然失敗，且不得 panic。
        let options = CallOptions::new(
            Path::new("/tmp/otterhive-nonexistent-home-for-client-test"),
            Auth::Operator("t".to_owned()),
            "list_instances",
            serde_json::json!({}),
        );
        let error = super::call(&options).expect_err("應回連線錯誤");
        assert!(
            matches!(error, ClientError::Unreachable(_)),
            "繁中說明：socket 不存在應回 Unreachable，實際為 {error:?}"
        );
    }
}
