//! P1a 協調契約 §1／§3：請求與結果的線路格式。
//!
//! 請求與回應皆為單行 JSON（JSON Lines），經 socket 傳遞。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// socket 請求（一行 JSON）：`{"v":1,"auth":{…},"op":"…","args":{…},"operation_id":"…"}`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub auth: Auth,
    pub op: String,
    #[serde(default)]
    pub args: Value,
    #[serde(default)]
    pub operation_id: Option<String>,
}

/// 呼叫端身分：JSON `{"operator":"…"}` 或 `{"instance":"…"}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Auth {
    #[serde(rename = "operator")]
    Operator(String),
    #[serde(rename = "instance")]
    Instance(String),
}

/// 操作結果的處置（契約 §3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Completed,
    Queued,
    Partial,
    Refused,
    Unsupported,
}

/// 契約 §3 的 15 個錯誤碼，snake_case 序列化。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    NotFound,
    NameConflict,
    InvalidArgument,
    StaleGeneration,
    OperationInProgress,
    SpawnFailed,
    ResumeUnavailable,
    RecoveryRequired,
    DeliveryTokenStale,
    NeverDelivered,
    TeamAclDenied,
    UnsupportedInPhase,
    DaemonAlreadyRunning,
    Internal,
}

/// 操作錯誤：錯誤碼、繁體中文說明、是否可重試與下一步。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    pub next: Option<String>,
}

impl std::fmt::Display for OpError {
    /// 只印錯誤碼與說明。
    ///
    /// `retryable` 與 `next` 不在字串裡：它們是**機器可讀**的欄位（§3 的線路
    /// 形狀），把它們混進給人看的訊息只會讓同一件事有兩個真相。線路上的完整
    /// 形狀仍然是 `OperationResult.error` 的 JSON。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}: {}", self.code, self.message)
    }
}

/// 結果的 subject（契約 §3）：目前只有 instance 一種。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    pub kind: String,
    pub id: String,
    pub name: String,
    pub generation: u64,
}

/// socket 回應（一行 JSON 的 `OperationResult`）。
///
/// 所有欄位一律序列化（`subject: null`、`effects: []`、`error: null`
/// 都保留），以維持固定的線路形狀。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationResult {
    pub operation_id: String,
    pub outcome: Outcome,
    pub subject: Option<Subject>,
    pub value: Value,
    pub effects: Vec<String>,
    pub error: Option<OpError>,
}

impl OperationResult {
    /// 成功的結果。
    pub fn completed(id: &str, value: Value) -> Self {
        Self {
            operation_id: id.to_owned(),
            outcome: Outcome::Completed,
            subject: None,
            value,
            effects: Vec::new(),
            error: None,
        }
    }

    /// 被拒的結果（`refused`）。
    pub fn refused(id: &str, code: ErrorCode, msg: &str) -> Self {
        Self {
            operation_id: id.to_owned(),
            outcome: Outcome::Refused,
            subject: None,
            value: Value::Null,
            effects: Vec::new(),
            error: Some(OpError {
                code,
                message: msg.to_owned(),
                retryable: false,
                next: None,
            }),
        }
    }
}
