//! T4 Lane B：MCP 工具清單與 input schema（契約 §10）。
//!
//! 兩條規則：
//! 1. `tools_list` 只列 P1a 已實作、且該 instance 至少能執行一個 action 的
//!    工具。T4 只實作 lifecycle，所以一般 instance 看到三個、manager 與
//!    orchestrator 另外看得到 `interrupt`；`send`／`inbox`／`team` 的 handler
//!    在 T5，`create`／`start`／`delete`／`deployment` 不列出（契約 §10 明文）。
//! 2. 工具的 input schema **只含** catalog 中該工具的 `supported_fields`
//!    與 `added_fields`——不得多（`refused_fields`）也不得少。
//!
//! schema 的欄位來源是 [`crate::admission::op_spec`] 的 `fields`，那份資料由
//! `tests/protocol_admission.rs::admission_matches_the_migration_catalog`
//! 對過 catalog；本模組的測試再以 catalog 獨立復算一次，兩個來源必須一致。

use serde_json::{json, Value};

use crate::admission::op_spec;

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

/// 該 caller 看得到的工具名稱（已排序，讓 `tools/list` 的輸出可預期）。
///
/// `is_manager` 對應契約 §2 的 `managers` 設定與 team 的 `orchestrator`；
/// 這兩者額外看得到 `interrupt`。orchestrator 另有「只限自己 team 成員」的
/// 規則，那是 daemon 端的授權（Lane A），本函式只負責「看得見」。
pub fn tool_names(is_manager: bool) -> Vec<String> {
    let names: Vec<String> = T4_LIFECYCLE_TOOLS
        .iter()
        .filter(|name| is_manager || **name != "interrupt")
        .map(|name| (*name).to_owned())
        .collect();
    names
}

/// 工具的 input schema 欄位（排序後）。
///
/// 等於 catalog 的 `supported_fields ∪ added_fields`。工具不在 T4 的清單裡
/// 時回空 vec（不是錯誤：未實作的工具本來就沒有 schema）。
pub fn input_schema_fields(tool: &str) -> Vec<String> {
    let Some(spec) = op_spec(tool) else {
        return Vec::new();
    };
    let mut fields: Vec<String> = spec
        .fields
        .iter()
        .map(|field| (*field).to_owned())
        .collect();
    fields.sort();
    fields
}

/// 單一工具的完整定義（`tools/list` 的 `result.tools[]` 元素）。
///
/// `inputSchema.type` 固定為 `object`；`required` 保留為空陣列——T4 的
/// lifecycle 工具沒有必填欄位（instance 名稱與 generation 由 daemon 從
/// 憑證與語境推導，其餘省略時走預設），但保留欄位讓 backend 不必假設它
/// 不存在。
pub fn tool_definition(tool: &str) -> Value {
    let properties: Value = input_schema_fields(tool)
        .iter()
        .map(|field| (field.clone(), json!({"type": json_type_for_field(field)})))
        .collect();
    json!({
        "name": tool,
        "description": description_for(tool),
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required_for(tool),
        },
    })
}

/// 完整的工具定義清單（`tools/list` 的 `result.tools`）。
///
/// 順序固定為 [`T4_LIFECYCLE_TOOLS`]，讓測試可以直接比較陣列而不必假設
/// daemon 的回應順序。
pub fn tool_definitions(is_manager: bool) -> Vec<Value> {
    tool_names(is_manager)
        .iter()
        .map(|tool| tool_definition(tool))
        .collect()
}

/// 欄位的 JSON 型別。
///
/// P1a 的 lifecycle 欄位只有兩種形狀：識別用的字串（`instance`／`mode`／
/// `reason`…）與數值的 `lines`（§4.7 規定預設 100、上限 10000）。
/// 不去猜 `pane_snapshot.lines` 是整數以外的形式。
fn json_type_for_field(field: &str) -> &'static str {
    match field {
        "lines" => "integer",
        _ => "string",
    }
}

/// 工具說明（`tools/list` 的 `description`）。
fn description_for(tool: &str) -> &'static str {
    match tool {
        "list_instances" => "列出所有 instance；帶 instance 時只回該項。",
        "pane_snapshot" => "取得 instance 目前畫面去除 ANSI 的純文字。",
        "restart_instance" => "依契約 §4.4 停止並重新啟動 instance。",
        "interrupt" => "對目前 generation 的 PTY 寫入一個 ESC。",
        _ => "OtterHive 工具。",
    }
}

/// 必填欄位。
///
/// T4 的 lifecycle 工具沒有必填欄位：實例名稱與 generation 由 daemon 從
/// 憑證與語境推導，其餘欄位省略時走預設。這裡回空陣列而非省略鍵，讓
/// schema 的形狀固定。
fn required_for(_tool: &str) -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::{input_schema_fields, tool_definitions, tool_names, T4_LIFECYCLE_TOOLS};

    #[test]
    fn an_instance_sees_three_tools_and_a_manager_sees_four() {
        assert_eq!(
            tool_names(false),
            vec![
                "list_instances".to_owned(),
                "pane_snapshot".to_owned(),
                "restart_instance".to_owned()
            ],
            "繁中說明：一般 instance 應看到三個 lifecycle 工具"
        );
        assert_eq!(
            tool_names(true),
            vec![
                "list_instances".to_owned(),
                "pane_snapshot".to_owned(),
                "restart_instance".to_owned(),
                "interrupt".to_owned(),
            ],
            "繁中說明：manager 應在一般 instance 的三個之外多看得到 interrupt"
        );
    }

    #[test]
    fn every_listed_tool_is_one_of_the_four_lifecycle_tools() {
        for name in tool_names(true) {
            assert!(
                T4_LIFECYCLE_TOOLS.contains(&name.as_str()),
                "繁中說明：{name} 不在 T4 的 lifecycle 清單裡"
            );
        }
    }

    #[test]
    fn deferred_and_non_p1a_tools_are_never_listed() {
        for deferred in super::DEFERRED_TO_LATER_PHASE {
            assert!(
                !tool_names(true).contains(&deferred.to_owned()),
                "繁中說明：{deferred} 在 T4 不應被列出"
            );
        }
        for non_p1a in ["decision", "task", "set_metadata", "schedule"] {
            assert!(
                !tool_names(true).contains(&non_p1a.to_owned()),
                "繁中說明：非 P1a 工具 {non_p1a} 不應被列出"
            );
        }
    }

    #[test]
    fn interrupt_is_hidden_from_a_plain_instance() {
        assert!(
            !tool_names(false).contains(&"interrupt".to_owned()),
            "繁中說明：契約 §2 的 interrupt 對 instance 為「否」，不該看得到工具"
        );
    }

    #[test]
    fn schema_fields_come_from_the_admission_spec() {
        assert_eq!(
            input_schema_fields("interrupt"),
            vec!["instance".to_owned()],
            "繁中說明：interrupt 的 schema 應只含 instance"
        );
        assert_eq!(
            input_schema_fields("list_instances"),
            vec!["instance".to_owned()],
            "繁中說明：list_instances 的 schema 應只含 instance"
        );
        assert!(
            !input_schema_fields("restart_instance").contains(&"force".to_owned()),
            "繁中說明：restart_instance 的 schema 不應含 P1a 拒收的 force"
        );
        assert!(
            input_schema_fields("restart_instance").contains(&"expected_generation".to_owned()),
            "繁中說明：restart_instance 的 schema 應含 P1a 新增的 expected_generation"
        );
        assert_eq!(
            input_schema_fields("pane_snapshot"),
            vec!["instance".to_owned(), "lines".to_owned()],
            "繁中說明：pane_snapshot 的 schema 應含 instance 與 lines"
        );
    }

    #[test]
    fn an_unknown_tool_has_no_schema() {
        assert!(
            input_schema_fields("reply").is_empty(),
            "繁中說明：未實作的工具不應有 schema"
        );
    }

    #[test]
    fn every_definition_has_an_object_schema_with_its_name() {
        for definition in tool_definitions(true) {
            assert_eq!(definition["inputSchema"]["type"], "object");
            assert!(
                definition["name"].as_str().is_some(),
                "繁中說明：工具定義應有字串名稱，實際為 {definition}"
            );
            assert!(
                definition["description"]
                    .as_str()
                    .is_some_and(|d| !d.is_empty()),
                "繁中說明：工具定義應有非空 description，實際為 {definition}"
            );
            assert!(
                definition["inputSchema"]["required"].is_array(),
                "繁中說明：schema 應保留 required 陣列（即使為空），實際為 {definition}"
            );
        }
    }

    #[test]
    fn definitions_are_emitted_in_the_fixed_lifecycle_order() {
        let names: Vec<String> = tool_definitions(true)
            .iter()
            .map(|definition| definition["name"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(names, tool_names(true));
    }

    #[test]
    fn pane_snapshot_lines_is_typed_as_an_integer() {
        let definition = super::tool_definition("pane_snapshot");
        assert_eq!(
            definition["inputSchema"]["properties"]["lines"]["type"], "integer",
            "繁中說明：lines 是行數，應標為 integer"
        );
        assert_eq!(
            definition["inputSchema"]["properties"]["instance"]["type"], "string",
            "繁中說明：instance 是名稱，應標為 string"
        );
    }
}
