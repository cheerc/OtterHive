//! T4 Lane A 的 instance lifecycle 骨架（T4 RED，契約 §4）。
//!
//! 介面在此凍結，行為由 T4 GREEN 實作：
//! - `create_instance` 建立定義後立即 start；start 失敗保留定義、確認沒有殘留
//!   程序才為 `failed`，停止無法確認則 `recovery_required`。
//! - `start`／`restart`／`delete` 共用 §4.3 的有證據停止。
//! - 同一 instance 一次一個 lifecycle 操作，其餘回 `operation_in_progress`。

/// 一個 instance 的 lifecycle 狀態（契約 §4.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceState {
    Stopped,
    Starting,
    Running,
    Stopping,
    /// 附原因（`exited_during_startup`／`exec_failed`／`exited`／`daemon_restarted`）。
    Failed,
    RecoveryRequired,
}
