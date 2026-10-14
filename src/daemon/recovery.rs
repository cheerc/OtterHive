//! T4 Lane A 的 daemon 重啟收斂骨架（T4 RED，契約 §4.6）。
//!
//! PTY 檔案描述元隨舊 daemon 消失，無法重新接上。daemon 啟動時對記錄為
//! `starting`／`running`／`stopping`，或仍帶程序紀錄的 `failed`／`stopped`
//! instance 依 §4.3 確認並停止仍存活的程序：成功則為 `stopped`
//! （原因 `daemon_restarted`），否則為 `recovery_required`。既有的
//! `recovery_required` 保留，不因 daemon 重啟就解除；P1a 不自動重新啟動。

/// 重啟後一個 instance 的收斂判定（契約 §4.6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// 已確認停止 → `stopped`（原因 `daemon_restarted`）。
    Stopped { reason: &'static str },
    /// 停止無法確認 → `recovery_required`，殘留 pid 一併回報。
    RecoveryRequired { residual_pids: Vec<i32> },
}
