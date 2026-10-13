use std::collections::{BTreeMap, BTreeSet};

use otterhive::admission::{admit, deferred_phase, op_spec};
use otterhive::protocol::{Auth, ErrorCode, OperationResult, Request};
use serde_json::{json, Value};

fn assert_admission_error(
    op: &str,
    args: Value,
    expected: ErrorCode,
) -> otterhive::protocol::OpError {
    let error = admit(op, &args).expect_err("預期 admission 拒絕此請求");
    assert_eq!(error.code, expected, "錯誤碼應符合 admission 契約");
    error
}

#[test]
fn governed_send_fields_are_unsupported_before_mutation() {
    for field in [
        "branch",
        "task_id",
        "terminal",
        "review_assignment",
        "next_after_ci",
        "force",
    ] {
        let mut args = json!({"message": "請檢查"});
        args.as_object_mut()
            .expect("繁中說明：測試 send args 應為物件")
            .insert(field.to_owned(), json!("值"));
        let error = assert_admission_error("send", args, ErrorCode::UnsupportedInPhase);
        assert!(
            error.message.contains(field),
            "繁中說明：拒絕訊息應指出不支援的欄位 {field}"
        );
        assert!(
            error.message.contains("P1a 不支援"),
            "繁中說明：欄位錯誤應明確表示 P1a 不支援"
        );
    }
}

#[test]
fn unsupported_fields_name_the_field_not_the_tool_phase() {
    for (op, field) in [("create_instance", "team"), ("send", "branch")] {
        let mut args = if op == "send" {
            json!({"message": "內容"})
                .as_object()
                .expect("繁中說明：send 基礎 args 應為物件")
                .clone()
        } else {
            json!({"name": "worker"})
                .as_object()
                .expect("繁中說明：create_instance 基礎 args 應為物件")
                .clone()
        };
        args.insert(field.to_owned(), json!("值"));
        let error = assert_admission_error(op, Value::Object(args), ErrorCode::UnsupportedInPhase);
        assert!(
            error.message.contains(field),
            "繁中說明：拒絕訊息應指出 {op} 的欄位 {field}"
        );
        assert!(
            error.message.contains("P1a 不支援"),
            "繁中說明：P1a 工具的欄位訊息應表示 P1a 不支援"
        );
        assert!(
            !error.message.contains("P1a 實作"),
            "繁中說明：不可把工具 phase 誤寫成欄位的實作期別"
        );
    }
}

#[test]
fn request_kind_task_is_unsupported_and_unknown_kind_is_invalid() {
    let error = assert_admission_error(
        "send",
        json!({"message": "工作", "request_kind": "task"}),
        ErrorCode::UnsupportedInPhase,
    );
    assert!(
        error.message.contains("request_kind") && error.message.contains("task"),
        "繁中說明：拒絕訊息應指出不支援的 request_kind 值"
    );

    assert_admission_error(
        "send",
        json!({"message": "喊話", "request_kind": "shout"}),
        ErrorCode::InvalidArgument,
    );

    for kind in ["query", "report", "update"] {
        assert!(
            admit("send", &json!({"message": "內容", "request_kind": kind})).is_ok(),
            "繁中說明：catalog 支援的 request_kind 值 {kind} 應通過 admission"
        );
    }
}

#[test]
fn unknown_field_is_invalid_and_non_object_args_are_invalid() {
    assert_admission_error(
        "list_instances",
        json!({"unknown_field": true}),
        ErrorCode::InvalidArgument,
    );
    assert_admission_error(
        "list_instances",
        json!(["不是物件"]),
        ErrorCode::InvalidArgument,
    );
    assert!(
        admit("list_instances", &Value::Null).is_ok(),
        "繁中說明：null args 應依契約視為空物件"
    );
}

#[test]
fn legacy_actions_outside_p1a_are_unsupported() {
    for action in ["clear", "discharge"] {
        let error = assert_admission_error(
            "inbox",
            json!({"action": action}),
            ErrorCode::UnsupportedInPhase,
        );
        assert!(
            error.message.contains(action),
            "繁中說明：拒絕訊息應指出不支援的 inbox action {action}"
        );
    }

    assert_admission_error(
        "inbox",
        json!({"action": "explode"}),
        ErrorCode::InvalidArgument,
    );

    // Catalog defines drain as action omitted; do not require a literal "drain" value.
    assert!(
        admit("inbox", &json!({})).is_ok(),
        "繁中說明：inbox 省略 action 的 drain 應通過 admission"
    );
    assert!(
        admit(
            "inbox",
            &json!({"action": "ack", "message_id": "message-1", "delivery_token": "token-1"})
        )
        .is_ok(),
        "繁中說明：catalog 支援的 inbox ack action 應通過 admission"
    );

    assert_admission_error(
        "restart_instance",
        json!({"instance": "worker", "mode": "warm"}),
        ErrorCode::InvalidArgument,
    );
}

#[test]
fn deferred_tools_are_unsupported_and_name_their_phase() {
    for (tool, phase) in [
        ("task", "P2"),
        ("ci", "P3"),
        ("schedule", "P4"),
        ("set_model", "deferred-operator"),
        ("reply", "deferred-channel"),
        ("move_pane", "P1b"),
    ] {
        assert_eq!(
            deferred_phase(tool),
            Some(phase),
            "繁中說明：{tool} 應回報 catalog phase {phase}"
        );
        let error = assert_admission_error(tool, json!({}), ErrorCode::UnsupportedInPhase);
        assert!(
            error.message.contains(tool) && error.message.contains(phase),
            "繁中說明：整個 deferred 工具的訊息應指出工具及 phase"
        );
    }
    assert_eq!(
        deferred_phase("not_a_tool"),
        None,
        "繁中說明：未知工具不可被分類為 deferred"
    );
    assert_admission_error("not_a_tool", json!({}), ErrorCode::InvalidArgument);
}

#[test]
fn admission_matches_the_migration_catalog() {
    let catalog: Value =
        serde_json::from_str(include_str!("../docs/reverse-engineering/mcp-catalog.json"))
            .expect("繁中說明：遷移 catalog 必須是有效 JSON");
    let tools = catalog["tools"]
        .as_array()
        .expect("繁中說明：遷移 catalog tools 必須是陣列");

    for tool in tools {
        let name = tool["name"]
            .as_str()
            .expect("繁中說明：catalog 工具必須有字串名稱");
        let phase = tool["phase"]
            .as_str()
            .expect("繁中說明：catalog 工具必須有字串 phase");
        if phase != "P1a" {
            assert_eq!(
                deferred_phase(name),
                Some(phase),
                "繁中說明：非 P1a 工具 {name} 的 deferred_phase 應與 catalog 一致"
            );
            continue;
        }

        let spec =
            op_spec(name).unwrap_or_else(|| panic!("繁中說明：P1a 工具 {name} 必須有 OpSpec"));
        assert_eq!(spec.name, name, "繁中說明：OpSpec 名稱應與 catalog 一致");

        let p1a = &tool["p1a"];
        let mut catalog_fields = BTreeSet::new();
        for key in ["supported_fields", "added_fields"] {
            for field in p1a[key]
                .as_array()
                .unwrap_or_else(|| panic!("繁中說明：{name}.{key} 必須是陣列"))
            {
                catalog_fields.insert(
                    field
                        .as_str()
                        .unwrap_or_else(|| panic!("繁中說明：{name}.{key} 欄位必須是字串"))
                        .to_owned(),
                );
            }
        }
        let spec_fields: BTreeSet<String> = spec
            .fields
            .iter()
            .map(|field| (*field).to_owned())
            .collect();
        assert_eq!(
            spec_fields, catalog_fields,
            "繁中說明：{name} OpSpec.fields 應等於 catalog supported_fields ∪ added_fields"
        );

        for field in p1a["refused_fields"]
            .as_array()
            .unwrap_or_else(|| panic!("繁中說明：{name}.refused_fields 必須是陣列"))
        {
            let field = field
                .as_str()
                .unwrap_or_else(|| panic!("繁中說明：{name} 拒絕欄位必須是字串"));
            assert!(
                spec.legacy_fields.contains(&field),
                "繁中說明：{name} 的 legacy_fields 應包含 catalog 拒絕欄位 {field}"
            );
        }

        let mut catalog_values = BTreeMap::new();
        for (field, values) in p1a["supported_values"]
            .as_object()
            .unwrap_or_else(|| panic!("繁中說明：{name}.supported_values 必須是物件"))
        {
            let values = values
                .as_array()
                .unwrap_or_else(|| panic!("繁中說明：{name}.{field} 支援值必須是陣列"))
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .unwrap_or_else(|| panic!("繁中說明：{name}.{field} enum 必須是字串"))
                        .to_owned()
                })
                .collect::<BTreeSet<_>>();
            catalog_values.insert(field.clone(), values);
        }
        let spec_values = spec
            .supported_values
            .iter()
            .map(|(field, values)| {
                (
                    (*field).to_owned(),
                    values.iter().map(|value| (*value).to_owned()).collect(),
                )
            })
            .collect::<BTreeMap<String, BTreeSet<String>>>();
        assert_eq!(
            spec_values, catalog_values,
            "繁中說明：{name} 的 OpSpec supported_values 應與 catalog 完全一致"
        );
    }
}

#[test]
fn request_and_result_round_trip_as_json_lines() {
    let request_line = r#"{"v":1,"auth":{"operator":"token"},"op":"send","args":{"message":"你好"},"operation_id":"op-1"}"#;
    let request: Request =
        serde_json::from_str(request_line).expect("繁中說明：契約請求 JSON 行應可解析為 Request");
    assert!(matches!(request.auth, Auth::Operator(ref token) if token == "token"));
    assert_eq!(request.op, "send");
    let encoded_request =
        serde_json::to_string(&request).expect("繁中說明：Request 應可序列化為 JSON 行");
    assert!(
        !encoded_request.contains('\n'),
        "繁中說明：序列化的 Request 必須維持單行"
    );
    let decoded_request: Request =
        serde_json::from_str(&encoded_request).expect("繁中說明：序列化後的 Request 應可重新解析");
    assert_eq!(decoded_request.op, "send");
    assert_eq!(decoded_request.operation_id.as_deref(), Some("op-1"));

    let result = OperationResult::refused(
        "op-1",
        ErrorCode::UnsupportedInPhase,
        "P1a 不支援 send.branch",
    );
    let mut result_line =
        serde_json::to_string(&result).expect("繁中說明：OperationResult 應可序列化為 JSON 行");
    result_line.push('\n');
    let result_line = result_line
        .strip_suffix('\n')
        .expect("繁中說明：JSON Lines 結果應有且可移除行尾換行");
    assert!(
        !result_line.contains('\n'),
        "繁中說明：一個 OperationResult 必須恰為一個 JSON 行"
    );
    let encoded: Value =
        serde_json::from_str(result_line).expect("繁中說明：序列化結果行應是有效 JSON");
    assert_eq!(encoded["outcome"], "refused");
    assert_eq!(encoded["error"]["code"], "unsupported_in_phase");
    let decoded: OperationResult = serde_json::from_str(result_line)
        .expect("繁中說明：結果 JSON 行應可反序列化為 OperationResult");
    assert_eq!(decoded.operation_id, "op-1");
    assert_eq!(
        decoded.error.expect("繁中說明：refused 結果應帶錯誤").code,
        ErrorCode::UnsupportedInPhase
    );
}
