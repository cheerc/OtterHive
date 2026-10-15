//! T5 Lane B：MCP 工具清單與 input schema（契約 §10）。
//!
//! 兩條規則：
//! 1. `tools_list` 只列 P1a 已實作、且該 instance 至少能執行一個 action 的
//!    工具。T5 之後是一般 instance 恰 6 個（`list_instances`、
//!    `restart_instance`、`send`、`inbox`、`team`、`pane_snapshot`），
//!    manager 與 orchestrator 另外看得到 `interrupt`（恰 7 個）。
//!    `create_instance`／`start_instance`／`delete_instance`／`deployment`
//!    在 P1a **不實作**（operator 走 CLI，§9），因此不列出；直接呼叫依
//!    §2、§3 拒絕。
//! 2. 工具的 input schema **只含** catalog 中該工具的 `supported_fields`
//!    與 `added_fields`——不得多（`refused_fields`）也不得少。
//!
//! schema 的欄位來源是 [`crate::admission::op_spec`] 的 `fields`，那份資料由
//! `tests/protocol_admission.rs::admission_matches_the_migration_catalog`
//! 對過 catalog；本模組的測試再以 catalog 獨立復算一次，兩個來源必須一致。
//!
//! **T4 → T5 的常數更名**：`T4_LIFECYCLE_TOOLS`（4 個）與
//! `DEFERRED_TO_LATER_PHASE`（7 個）描述的是「T4 只做完 lifecycle」這個
//! 階段狀態，會隨 `send`／`inbox`／`team` 落地而過時。新的
//! [`P1A_INSTANCE_TOOLS`]／[`P1A_PRIVILEGED_TOOLS`]／[`NOT_LISTED_IN_P1A`]
//! 描述的是契約本身。T4 的常數與其單元測試已移除；T4 的 integration 語意
//! （transport 六條、`-32601`、`-32000`、`-32001`、schema 等於 catalog）
//! 由 `tests/mcp_wire.rs` 原樣保留，只有「工具清單」那幾條被改寫成 T5 的
//! 契約集合——那是契約更新，不是把測試拿掉。

use serde_json::{json, Value};

use crate::admission::op_spec;

/// T5 之後一般 instance 看得到的工具（契約 §10，恰 6 個）。
///
/// 順序即 `tools/list` 的輸出順序：契約 §10 的敘述順序，讓測試可以直接比較
/// 陣列而不必假設 daemon 的排序。
pub const P1A_INSTANCE_TOOLS: [&str; 6] = [
    "list_instances",
    "restart_instance",
    "send",
    "inbox",
    "team",
    "pane_snapshot",
];

/// manager 與 orchestrator 另外看得到的工具（契約 §2／§10，恰 7 個）。
///
/// `interrupt` 對一般 instance 是「否」，因此不出現在
/// [`P1A_INSTANCE_TOOLS`] 裡。
pub const P1A_PRIVILEGED_TOOLS: [&str; 7] = [
    "list_instances",
    "restart_instance",
    "send",
    "inbox",
    "team",
    "pane_snapshot",
    "interrupt",
];

/// P1a 已實作，但 `tools_list` **不列出**的工具（契約 §10）。
///
/// 分兩類，這個區分是契約要求的：
///
/// - **未實作**：P1a 的 operator 動作走 CLI（§9 的表格），instance 端沒有
///   對應工具。直接呼叫依 §2 拒絕（`unauthorized`），依 §3 對不存在於本期的
///   內容回 `unsupported_in_phase`。
/// - **非 P1a**：`decision`／`task` 等在 catalog 裡屬於後續期別，連規格都
///   不在 P1a。
///
/// 「不列出」不等於「可靜默忽略」。
pub const NOT_LISTED_IN_P1A: [&str; 8] = [
    "create_instance",
    "start_instance",
    "delete_instance",
    "deployment",
    "decision",
    "task",
    "set_metadata",
    "schedule",
];

/// `DEFERRED_TO_LATER_PHASE` 的相容別名。
///
/// **T5 沿用**：T4 的 `tests/backend_contract.rs`
/// （`daemon_tools_list_matches_mcp_tool_names_for_every_caller_kind`，不在
/// Lane B 的檔案所有權內）仍引用這個名字，而那條對撞測試的語意——「不列入
/// 工具清單的工具不得出現在 wire 清單裡」——在 T5 之後必須繼續跑。
///
/// 因此這裡保留舊名作為別名，而不是去改別人的檔案。內容刻意**不同**（舊名
/// 7 項、不含 `schedule`；新名 8 項），這樣一旦有人只改其中一個，這段文件的
/// 意圖仍然成立。
///
/// 這個常數可以在 `backend_contract.rs` 遷移之後移除。
pub const DEFERRED_TO_LATER_PHASE: [&str; 7] = [
    "create_instance",
    "start_instance",
    "delete_instance",
    "deployment",
    "decision",
    "task",
    "set_metadata",
];

/// 該 caller 看得到的工具名稱（已固定順序，讓 `tools/list` 的輸出可預期）。
///
/// `is_manager` 對應契約 §2 的 `managers` 設定與 team 的 `orchestrator`；
/// 這兩者額外看得到 `interrupt`。orchestrator 另有「只限自己 team 成員」的
/// 規則，那是 daemon 端的授權，本函式只負責「看得見」。
pub fn tool_names(is_manager: bool) -> Vec<String> {
    // 兩份清單長度不同（6 與 7），不能讓編譯器把它們當同一型別；這裡以
    // 切片繞過，並在測試釘住「只差 interrupt」這個不變量。
    let source: &[&str] = if is_manager {
        &P1A_PRIVILEGED_TOOLS
    } else {
        &P1A_INSTANCE_TOOLS
    };
    source.iter().map(|name| (*name).to_owned()).collect()
}

/// 工具的 input schema 欄位（排序後）。
///
/// 等於 catalog 的 `supported_fields ∪ added_fields`。工具不在 P1a 的清單裡
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
/// `inputSchema.type` 固定為 `object`；`required` 保留為空陣列——P1a 的工具
/// 沒有必填欄位（instance 名稱與 generation 由 daemon 從憑證與語境推導，
/// 其餘省略時走預設），但保留欄位讓 backend 不必假設它不存在。
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
/// 順序固定為 [`P1A_PRIVILEGED_TOOLS`]（manager 的集合），讓測試可以直接
/// 比較陣列而不必假設 daemon 的回應順序。
pub fn tool_definitions(is_manager: bool) -> Vec<Value> {
    tool_names(is_manager)
        .iter()
        .map(|tool| tool_definition(tool))
        .collect()
}

/// 欄位的 JSON 型別。
///
/// P1a 的欄位只有兩種形狀：識別用的字串（`instance`／`mode`／`kind`／
/// `name`…）與數值（`lines` 是行數、`expected_generation` 是世代數，
/// §4.7 規定預設 100、上限 10000）。不去猜其他形狀。
fn json_type_for_field(field: &str) -> &'static str {
    match field {
        "lines" | "expected_generation" => "integer",
        "members" | "instances" | "add" | "remove" | "accept_from" => "array",
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
        "send" => "依契約 §6.1 送訊息給 instance、instances 或 team。",
        "inbox" => "依契約 §6.3 drain 自己的 inbox 或 ack 已投遞的訊息。",
        "team" => "依契約 §8.1 建立、更新、刪除或列出 team。",
        _ => "OtterHive 工具。",
    }
}

/// 必填欄位。
///
/// P1a 的工具沒有在 schema 層標為必填的欄位：instance 名稱與 generation 由
/// daemon 從憑證與語境推導，其餘省略時走預設；`send`／`team` 的必填性由
/// daemon 端以 `invalid_argument` 表達（§3、§8.1），不是由 schema 表達。
/// 這裡回空陣列而非省略鍵，讓 schema 的形狀固定。
fn required_for(_tool: &str) -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::{
        input_schema_fields, tool_definitions, tool_names, P1A_INSTANCE_TOOLS, P1A_PRIVILEGED_TOOLS,
    };

    #[test]
    fn an_instance_sees_exactly_six_tools_and_a_manager_sees_seven() {
        assert_eq!(
            P1A_INSTANCE_TOOLS.len(),
            6,
            "繁中說明：契約 §10 的一般 instance 工具數是 6"
        );
        assert_eq!(
            P1A_PRIVILEGED_TOOLS.len(),
            7,
            "繁中說明：契約 §10 的 manager／orchestrator 工具數是 7"
        );
        assert_eq!(
            tool_names(false),
            P1A_INSTANCE_TOOLS
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
            "繁中說明：一般 instance 的工具清單應恰為契約 §10 的 6 個"
        );
        assert_eq!(
            tool_names(true),
            P1A_PRIVILEGED_TOOLS
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
            "繁中說明：manager 的工具清單應恰為契約 §10 的 7 個"
        );
    }

    #[test]
    fn the_only_difference_between_the_two_lists_is_interrupt() {
        let instance: Vec<&str> = P1A_INSTANCE_TOOLS.to_vec();
        let privileged: Vec<&str> = P1A_PRIVILEGED_TOOLS.to_vec();
        let extra: Vec<&str> = privileged
            .iter()
            .filter(|name| !instance.contains(name))
            .copied()
            .collect();
        assert_eq!(
            extra,
            vec!["interrupt"],
            "繁中說明：兩份清單只差 interrupt，契約 §2／§10"
        );
    }

    #[test]
    fn every_listed_tool_is_one_of_the_contract_p1a_tools() {
        for name in tool_names(true) {
            assert!(
                P1A_PRIVILEGED_TOOLS.contains(&name.as_str()),
                "繁中說明：{name} 不在 P1a 的工具清單裡"
            );
        }
    }

    #[test]
    fn unlisted_and_non_p1a_tools_are_never_listed() {
        for absent in super::NOT_LISTED_IN_P1A {
            assert!(
                !tool_names(true).contains(&absent.to_owned()),
                "繁中說明：{absent} 在 P1a 不應被列出（契約 §10）"
            );
            assert!(
                !tool_names(false).contains(&absent.to_owned()),
                "繁中說明：{absent} 對一般 instance 也不應被列出"
            );
        }
    }

    #[test]
    fn interrupt_is_hidden_from_a_plain_instance() {
        assert!(
            !tool_names(false).contains(&"interrupt".to_owned()),
            "繁中說明：契約 §2 的 interrupt 對 instance 為「否」，不該看得到工具"
        );
        assert!(
            tool_names(true).contains(&"interrupt".to_owned()),
            "繁中說明：manager 應看得到 interrupt"
        );
    }

    #[test]
    fn the_three_tools_whose_handlers_land_in_t5_are_now_listed() {
        // T4 時這三個不在清單裡（handler 未落地）；T5 落地後必須看得到。
        for tool in ["send", "inbox", "team"] {
            assert!(
                tool_names(false).contains(&tool.to_owned()),
                "繁中說明：{tool} 的 handler 已在 T5 落地，一般 instance 應看得到"
            );
        }
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
    fn schema_fields_of_the_t5_tools_match_the_contract() {
        // 契約 §6.1：send 的收件人三擇一 ＋ message ＋ request_kind ＋
        // 三個不透明字串。這裡只斷言「集合等於 admission 的權威資料」，
        // 欄位內容的逐項對應由 `tests/mcp_wire.rs` 對 catalog 復算。
        assert_eq!(
            input_schema_fields("inbox"),
            vec![
                "action".to_owned(),
                "delivery_token".to_owned(),
                "message_id".to_owned()
            ],
            "繁中說明：inbox 的 schema 應只有 action／message_id／delivery_token（契約 §6.3）"
        );
        assert!(
            !input_schema_fields("send").contains(&"tags".to_owned()),
            "繁中說明：send 的 schema 不應含 P1a 拒收的 tags"
        );
        assert!(
            input_schema_fields("team").contains(&"accept_from".to_owned()),
            "繁中說明：team 的 schema 應含 accept_from（契約 §6.2／§8.1）"
        );
    }

    #[test]
    fn an_unknown_tool_has_no_schema() {
        assert!(
            input_schema_fields("reply").is_empty(),
            "繁中說明：未實作的工具不應有 schema"
        );
        assert!(
            input_schema_fields("create_instance").is_empty()
                || !tool_names(true).contains(&"create_instance".to_owned()),
            "繁中說明：create_instance 不在工具清單裡"
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
    fn definitions_are_emitted_in_the_fixed_contract_order() {
        let names: Vec<String> = tool_definitions(true)
            .iter()
            .map(|definition| definition["name"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(names, tool_names(true));
        let instance_names: Vec<String> = tool_definitions(false)
            .iter()
            .map(|definition| definition["name"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(instance_names, tool_names(false));
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

    #[test]
    fn team_list_members_are_typed_as_arrays() {
        let definition = super::tool_definition("team");
        for field in ["members", "add", "remove", "accept_from"] {
            assert_eq!(
                definition["inputSchema"]["properties"][field]["type"], "array",
                "繁中說明：team.{field} 是名稱陣列，應標為 array（契約 §8.1）"
            );
        }
        assert_eq!(
            definition["inputSchema"]["properties"]["action"]["type"], "string",
            "繁中說明：team.action 是列舉值（create／update／delete／list），應標為 string"
        );
    }

    #[test]
    fn send_recipients_are_typed_as_arrays_where_the_contract_is_plural() {
        let definition = super::tool_definition("send");
        assert_eq!(
            definition["inputSchema"]["properties"]["instances"]["type"], "array",
            "繁中說明：send.instances 是名稱陣列（契約 §6.1 的三擇一），應標為 array"
        );
        for field in ["instance", "team", "message", "request_kind"] {
            assert_eq!(
                definition["inputSchema"]["properties"][field]["type"], "string",
                "繁中說明：send.{field} 應標為 string"
            );
        }
    }
}
