//! T4 Lane B RED：operator CLI 的**凍結介面**骨架（無行為）。
//!
//! 這是 test-only 的空骨架：指令形狀與 exit code 對應先固定，GREEN 之前解析器
//! 永遠回「未知指令」，行為測試因此是 compile-RED 而非假綠。
//!
//! 契約 §9 的硬規則（GREEN 必須滿足，RED 只鎖定常數與映射）：
//! - 全域選項 `--home <dir>` 優先於 `OTTERHIVE_HOME`。
//! - 除 `daemon` 與 `attach` 外，每個指令把 `OperationResult` 以 JSON 印到 stdout。
//! - exit code：`0` completed／queued、`2` partial、`3` refused、`4` unsupported、
//!   `1` 無法連到 daemon 或請求格式錯誤。
//! - `daemon` 的第二程序衝突也用 3，但以 stderr 的 `daemon_already_running`
//!   JSON 辨識；**測試不能只檢查數字就判定原因**。

use std::path::PathBuf;
use std::time::Duration;

/// CLI 的預設逾時（與 [`crate::client::DEFAULT_TIMEOUT`] 同源）。
pub const DEFAULT_TIMEOUT: Duration = crate::client::DEFAULT_TIMEOUT;

/// 契約 §9 的 exit code。
pub mod exit_code {
    /// `completed`／`queued`。
    pub const OK: i32 = 0;
    /// 無法連到 daemon 或請求格式錯誤。
    pub const UNREACHABLE_OR_MALFORMED: i32 = 1;
    /// `partial`。
    pub const PARTIAL: i32 = 2;
    /// `refused`（含第二個 daemon 的衝突，以 stderr JSON 辨識）。
    pub const REFUSED: i32 = 3;
    /// `unsupported`。
    pub const UNSUPPORTED: i32 = 4;
}

/// CLI 認得的操作指令（契約 §9 的表格）。
///
/// T4 只實作 lifecycle 部分；`send`／`team`／`deploy` 系列在對應工作包
/// （T5／T6）加入。這份清單是 GREEN 的目標，不是已實作的聲明。
pub const OPERATIONS: [&str; 7] = [
    "create",
    "start",
    "delete",
    "restart",
    "list",
    "interrupt",
    "snapshot",
];

/// 只經 socket、不經 CLI 表格的兩個操作（契約 §1）。
pub const SOCKET_ONLY_OPERATIONS: [&str; 2] = ["attach", "tools_list"];

/// 一次 CLI 解析的結果。
///
/// 這是 RED 骨架的唯一可觀察行為：尚未實作的指令一律回
/// [`Parsed::Unknown`]。GREEN 補上參數解析後，`Parsed::Call` 才會出現。
#[derive(Debug)]
pub enum Parsed {
    /// `otterhive daemon`（契約 §1：前景執行 daemon）。
    Daemon { home: Option<PathBuf> },
    /// 一個操作指令（契約 §9 的表格之一）。
    Call {
        home: Option<PathBuf>,
        op: String,
        args: serde_json::Value,
    },
    /// `otterhive call <op> [--json <object>]`：任何操作的原始呼叫。
    RawCall {
        home: Option<PathBuf>,
        op: String,
        args: serde_json::Value,
    },
    /// 未認得的指令；GREEN 之後不應在合法輸入上出現。
    Unknown { argv0: String },
}

/// 解析 argv（不含程式名）。
///
/// RED 骨架：只認得 `daemon`，其餘一律 `Unknown`。GREEN 在此補上契約 §9 的
/// 表格與全域 `--home`。
pub fn parse(_args: &[String]) -> Parsed {
    Parsed::Unknown {
        argv0: String::new(),
    }
}

/// CLI 的 exit code 映射（契約 §9）。
///
/// 把映射做成純函式，讓 CLI、bridge 與測試共用同一份規則；T4 的測試直接
/// 對這個函式斷言，不依賴 binary 的實際退出碼。
pub fn exit_code_for(outcome: crate::protocol::Outcome) -> i32 {
    use crate::protocol::Outcome;
    match outcome {
        Outcome::Completed | Outcome::Queued => exit_code::OK,
        Outcome::Partial => exit_code::PARTIAL,
        Outcome::Refused => exit_code::REFUSED,
        Outcome::Unsupported => exit_code::UNSUPPORTED,
    }
}

/// 讀 operator 憑證（契約 §9：CLI 從 `secrets/operator.token` 讀）。
///
/// RED 骨架：回 `None`。GREEN 讀檔並處理「檔案不存在／權限過寬」。
pub fn read_operator_token(_home: &std::path::Path) -> Option<String> {
    None
}
