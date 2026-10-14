//! T4 Lane B RED：MCP 工具清單與 schema 的**凍結介面**骨架（無行為）。
//!
//! 契約 §10 的兩條規則由本模組承載：
//! 1. `tools_list` 只列 P1a 已實作、且該 instance 至少能執行一個 action 的
//!    工具。T4 只實作 lifecycle，所以一般 instance 看到三個、manager 多
//!    `interrupt`；`send`／`inbox`／`team` 的 handler 在 T5。
//! 2. 工具的 input schema **只含** catalog 中該工具的 `supported_fields`
//!    與 `added_fields`——不得多（`refused_fields`）也不得少。
//!
//! RED 骨架：清單為空、schema 為空。GREEN 從 `src/admission.rs` 的
//! `OpSpec.fields`（本身已對過 catalog）產生，測試端再以 catalog 獨立復算。
//! 兩個來源必須一致——這是本模組存在的理由。

/// T4 已完成的 lifecycle 工具（plan T4）。
///
/// 索引 0–2 是一般 instance 的清單；索引 3（`interrupt`）只有 manager 與
/// orchestrator 看得到（契約 §2／§10）。
pub const T4_LIFECYCLE_TOOLS: [&str; 4] = [
    "list_instances",
    "pane_snapshot",
    "restart_instance",
    "interrupt",
];

/// T4 的 handler 尚未實作、因此不列入工具清單的 P1a 工具。
///
/// 「不列出」不等於「可靜默忽略」：直接呼叫仍須回 `unsupported_in_phase`
/// （契約 §3）。
pub const DEFERRED_TO_LATER_PHASE: [&str; 7] = [
    "create_instance",
    "start_instance",
    "delete_instance",
    "send",
    "inbox",
    "team",
    "deployment",
];

/// 該 caller 看得到的工具名稱。
///
/// `is_manager` 對應契約 §2 的 `managers` 設定與 team 的 `orchestrator`；
/// 這兩者額外看得到 `interrupt`。orchestrator 另有限定自己 team 成員的
/// 規則，那是 daemon 端的授權（Lane A），本函式只負責「看得見」。
///
/// RED 骨架：永遠回空清單。
pub fn tool_names(_is_manager: bool) -> Vec<String> {
    Vec::new()
}

/// 工具的 input schema 欄位（排序後）。
///
/// GREEN 的契約：等於 catalog 的 `supported_fields ∪ added_fields`。
/// RED 骨架：永遠回空。
pub fn input_schema_fields(_tool: &str) -> Vec<String> {
    Vec::new()
}

/// 完整的工具定義（`tools/list` 的 `result.tools[]` 元素）。
///
/// 排序依 `T4_LIFECYCLE_TOOLS`，讓 `tools/list` 的輸出是穩定的（測試因此可以
/// 直接比較陣列，而不必假設 daemon 的回應順序）。
///
/// RED 骨架：永遠回空。
pub fn tool_definitions(_is_manager: bool) -> Vec<serde_json::Value> {
    Vec::new()
}
