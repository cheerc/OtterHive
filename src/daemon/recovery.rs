//! T4 Lane A GREEN：daemon 重啟收斂（契約 §4.6）。
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

/// `daemon_restarted` 的原因字串（契約 §4.6 固定）。
pub const RESTART_REASON: &str = "daemon_restarted";

/// 該 instance 是否需要在 daemon 啟動時收斂（契約 §4.6）。
///
/// 涵蓋記錄為 `starting`／`running`／`stopping`，或仍帶程序紀錄的
/// `failed`／`stopped`；不在契約列舉內的狀態也必須收斂——它同樣可能是個
/// 已經留下殘留程序的 instance，daemon 沒有理由假設它「不需要處理」。
///
/// `recovery_required` 回 `false`：既有的收斂狀態不因重啟而解除，也不自動
/// 重新啟動。
pub fn needs_convergence(state: &str) -> bool {
    state != "recovery_required"
}
