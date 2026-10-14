//! T4 Lane A 的 daemon 授權骨架（T4 RED，契約 §2）。
//!
//! 介面在此凍結，行為由 T4 GREEN 實作：
//! - operator 以 `secrets/operator.token` 認證；instance 以每個 generation
//!   一組的 token 認證，舊 generation 的 token 一律失效。
//! - 呼叫端不能自報 sender／role／team：sender 一律由憑證推導。
//! - 權限只來自 operator、`managers`、team 的 `orchestrator` 與「對自己」；
//!   `role` 是描述文字，不授予任何權限。

/// 依憑證推導出的呼叫端身分（契約 §2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// operator（`secrets/operator.token`）。
    Operator,
    /// instance 呼叫端；`id` 為 `InstanceId`。
    Instance(String),
}
