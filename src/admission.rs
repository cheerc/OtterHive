//! P1a 協調契約 §3／§6／§8：admission（操作准入）。
//!
//! `admit` 只做形狀檢查（工具、欄位、action、列舉值），不檢查必填與內容語意：
//! - 非 P1a 工具 → `unsupported_in_phase`，訊息另列 catalog 的 `phase`。
//! - P1a 工具的舊版欄位 → `unsupported_in_phase`，訊息指出欄位並說明 P1a 不支援，
//!   不得借用工具的 phase 宣稱欄位的移植期別。
//! - 未知的工具、欄位、action 或值（舊版也沒有的）→ `invalid_argument`。
//! - `null` args 視為空物件；非物件的 args → `invalid_argument`。

use serde_json::Value;

use crate::protocol::{ErrorCode, OpError};

/// 單一 P1a 工具的准入規格，對應 `mcp-catalog.json` 的 `p1a` 區段。
pub struct OpSpec {
    /// 工具名稱（與操作名稱相同）。
    pub name: &'static str,
    /// catalog `supported_fields ∪ added_fields`。
    pub fields: &'static [&'static str],
    /// catalog `refused_fields`（舊版有、P1a 拒收的欄位）。
    pub legacy_fields: &'static [&'static str],
    /// P1a 接受的 action 值（`action` 省略視為 drain，只用於 inbox／team／deployment）。
    pub supported_actions: &'static [&'static str],
    /// 舊版有、P1a 拒收的 action 值。
    pub legacy_actions: &'static [&'static str],
    /// P1a 接受的列舉值：`(欄位, 允許值)`。
    pub supported_values: &'static [(&'static str, &'static [&'static str])],
    /// 舊版有、P1a 拒收的列舉值：`(欄位, 拒收值)`。
    pub refused_values: &'static [(&'static str, &'static [&'static str])],
}

// --- send ---

const SEND_FIELDS: &[&str] = &[
    "instance",
    "instances",
    "team",
    "message",
    "request_kind",
    "correlation_id",
    "parent_id",
    "thread_id",
];
const SEND_LEGACY_FIELDS: &[&str] = &[
    "tags",
    "message_from_file",
    "report_purpose",
    "code_review",
    "success_criteria",
    "context",
    "task_id",
    "force",
    "force_reason",
    "second_reviewer",
    "second_reviewer_reason",
    "review_class",
    "governing_decision_id",
    "plan_ack_required",
    "plan_ack_reason",
    "review_assignment",
    "repository",
    "pr_number",
    "review_author",
    "reviewed_head",
    "expected_head",
    "artifacts",
    "branch",
    "eta_minutes",
    "reporting_cadence",
    "bind",
    "worktree_binding_required",
    "expect_reply_within_secs",
    "next_after_ci",
    "terminal",
    "no_report_expected",
    "ack_inbox",
    "triaged",
];
const SEND_SUPPORTED_VALUES: &[(&str, &[&str])] =
    &[("request_kind", &["query", "report", "update"])];
const SEND_REFUSED_VALUES: &[(&str, &[&str])] = &[("request_kind", &["task"])];

// --- inbox ---

const INBOX_FIELDS: &[&str] = &["action", "message_id", "delivery_token"];
const INBOX_LEGACY_FIELDS: &[&str] = &["reason", "thread_id", "instance"];
const INBOX_SUPPORTED_ACTIONS: &[&str] = &["drain", "ack"];
const INBOX_LEGACY_ACTIONS: &[&str] = &["clear", "discharge"];

// --- list_instances ---

const LIST_INSTANCES_FIELDS: &[&str] = &["instance"];
const LIST_INSTANCES_LEGACY_FIELDS: &[&str] = &["verbose", "include_evidence"];

// --- create_instance ---

const CREATE_INSTANCE_FIELDS: &[&str] = &[
    "name",
    "backend",
    "args",
    "model",
    "effort",
    "working_directory",
    "description",
    "role",
    "env",
    "command",
    "instructions",
    "skills_path",
];
const CREATE_INSTANCE_LEGACY_FIELDS: &[&str] = &[
    "model_tier",
    "branch",
    "task",
    "layout",
    "target_pane",
    "count",
    "team",
    "backends",
    "topic_binding",
];
const CREATE_INSTANCE_SUPPORTED_VALUES: &[(&str, &[&str])] = &[("backend", &["claude", "command"])];

// --- delete_instance ---

const DELETE_INSTANCE_FIELDS: &[&str] = &["instance"];
const DELETE_INSTANCE_LEGACY_FIELDS: &[&str] = &["force", "force_reason"];

// --- start_instance ---

const START_INSTANCE_FIELDS: &[&str] = &["instance"];
const START_INSTANCE_LEGACY_FIELDS: &[&str] = &[];

// --- restart_instance ---

const RESTART_INSTANCE_FIELDS: &[&str] = &["instance", "mode", "reason", "expected_generation"];
const RESTART_INSTANCE_LEGACY_FIELDS: &[&str] = &["force"];
const RESTART_INSTANCE_SUPPORTED_VALUES: &[(&str, &[&str])] = &[("mode", &["fresh", "resume"])];

// --- interrupt ---

const INTERRUPT_FIELDS: &[&str] = &["instance"];
const INTERRUPT_LEGACY_FIELDS: &[&str] = &["reason", "snapshot"];

// --- pane_snapshot ---

const PANE_SNAPSHOT_FIELDS: &[&str] = &["instance", "lines"];
const PANE_SNAPSHOT_LEGACY_FIELDS: &[&str] = &["to_file", "head"];

// --- team ---

const TEAM_FIELDS: &[&str] = &[
    "action",
    "name",
    "members",
    "orchestrator",
    "description",
    "add",
    "remove",
    "accept_from",
];
const TEAM_LEGACY_FIELDS: &[&str] = &["repository_path", "project_id"];
const TEAM_SUPPORTED_ACTIONS: &[&str] = &["create", "delete", "list", "update"];

// --- deployment ---

const DEPLOYMENT_FIELDS: &[&str] = &["action", "template", "name", "directory"];
const DEPLOYMENT_LEGACY_FIELDS: &[&str] = &["branch"];
const DEPLOYMENT_SUPPORTED_ACTIONS: &[&str] = &["deploy", "teardown", "list"];

const EMPTY_STRS: &[&str] = &[];
const EMPTY_VALUES: &[(&str, &[&str])] = &[];

const OP_SPECS: &[OpSpec] = &[
    OpSpec {
        name: "send",
        fields: SEND_FIELDS,
        legacy_fields: SEND_LEGACY_FIELDS,
        supported_actions: EMPTY_STRS,
        legacy_actions: EMPTY_STRS,
        supported_values: SEND_SUPPORTED_VALUES,
        refused_values: SEND_REFUSED_VALUES,
    },
    OpSpec {
        name: "inbox",
        fields: INBOX_FIELDS,
        legacy_fields: INBOX_LEGACY_FIELDS,
        supported_actions: INBOX_SUPPORTED_ACTIONS,
        legacy_actions: INBOX_LEGACY_ACTIONS,
        supported_values: EMPTY_VALUES,
        refused_values: EMPTY_VALUES,
    },
    OpSpec {
        name: "list_instances",
        fields: LIST_INSTANCES_FIELDS,
        legacy_fields: LIST_INSTANCES_LEGACY_FIELDS,
        supported_actions: EMPTY_STRS,
        legacy_actions: EMPTY_STRS,
        supported_values: EMPTY_VALUES,
        refused_values: EMPTY_VALUES,
    },
    OpSpec {
        name: "create_instance",
        fields: CREATE_INSTANCE_FIELDS,
        legacy_fields: CREATE_INSTANCE_LEGACY_FIELDS,
        supported_actions: EMPTY_STRS,
        legacy_actions: EMPTY_STRS,
        supported_values: CREATE_INSTANCE_SUPPORTED_VALUES,
        refused_values: EMPTY_VALUES,
    },
    OpSpec {
        name: "delete_instance",
        fields: DELETE_INSTANCE_FIELDS,
        legacy_fields: DELETE_INSTANCE_LEGACY_FIELDS,
        supported_actions: EMPTY_STRS,
        legacy_actions: EMPTY_STRS,
        supported_values: EMPTY_VALUES,
        refused_values: EMPTY_VALUES,
    },
    OpSpec {
        name: "start_instance",
        fields: START_INSTANCE_FIELDS,
        legacy_fields: START_INSTANCE_LEGACY_FIELDS,
        supported_actions: EMPTY_STRS,
        legacy_actions: EMPTY_STRS,
        supported_values: EMPTY_VALUES,
        refused_values: EMPTY_VALUES,
    },
    OpSpec {
        name: "restart_instance",
        fields: RESTART_INSTANCE_FIELDS,
        legacy_fields: RESTART_INSTANCE_LEGACY_FIELDS,
        supported_actions: EMPTY_STRS,
        legacy_actions: EMPTY_STRS,
        supported_values: RESTART_INSTANCE_SUPPORTED_VALUES,
        refused_values: EMPTY_VALUES,
    },
    OpSpec {
        name: "interrupt",
        fields: INTERRUPT_FIELDS,
        legacy_fields: INTERRUPT_LEGACY_FIELDS,
        supported_actions: EMPTY_STRS,
        legacy_actions: EMPTY_STRS,
        supported_values: EMPTY_VALUES,
        refused_values: EMPTY_VALUES,
    },
    OpSpec {
        name: "pane_snapshot",
        fields: PANE_SNAPSHOT_FIELDS,
        legacy_fields: PANE_SNAPSHOT_LEGACY_FIELDS,
        supported_actions: EMPTY_STRS,
        legacy_actions: EMPTY_STRS,
        supported_values: EMPTY_VALUES,
        refused_values: EMPTY_VALUES,
    },
    OpSpec {
        name: "team",
        fields: TEAM_FIELDS,
        legacy_fields: TEAM_LEGACY_FIELDS,
        supported_actions: TEAM_SUPPORTED_ACTIONS,
        legacy_actions: EMPTY_STRS,
        supported_values: EMPTY_VALUES,
        refused_values: EMPTY_VALUES,
    },
    OpSpec {
        name: "deployment",
        fields: DEPLOYMENT_FIELDS,
        legacy_fields: DEPLOYMENT_LEGACY_FIELDS,
        supported_actions: DEPLOYMENT_SUPPORTED_ACTIONS,
        legacy_actions: EMPTY_STRS,
        supported_values: EMPTY_VALUES,
        refused_values: EMPTY_VALUES,
    },
];

/// 回傳 P1a 工具的准入規格；非 P1a 工具回 `None`。
pub fn op_spec(op: &str) -> Option<&'static OpSpec> {
    OP_SPECS.iter().find(|spec| spec.name == op)
}

/// 回傳非 P1a 工具的 catalog phase；P1a 工具與未知工具回 `None`。
pub fn deferred_phase(tool: &str) -> Option<&'static str> {
    match tool {
        "reply" | "operator_page" | "download_attachment" | "bind_topic" => {
            Some("deferred-channel")
        }
        "set_model" => Some("deferred-operator"),
        "set_metadata" | "set_waiting_on" | "move_pane" | "instance" => Some("P1b"),
        "decision" | "task" | "repo" | "bind_self" | "release_worktree" | "binding_state" => {
            Some("P2")
        }
        "ci" | "revoke_review_assignment" | "correct_review_class" => Some("P3"),
        "restart_daemon" | "schedule" | "config" | "health" | "usage_limit_takeover" => Some("P4"),
        _ => None,
    }
}

fn invalid(msg: String) -> OpError {
    OpError {
        code: ErrorCode::InvalidArgument,
        message: msg,
        retryable: false,
        next: None,
    }
}

fn unsupported(msg: String) -> OpError {
    OpError {
        code: ErrorCode::UnsupportedInPhase,
        message: msg,
        retryable: false,
        next: None,
    }
}

fn str_value(value: &Value) -> Option<&str> {
    if let Value::String(text) = value {
        Some(text.as_str())
    } else {
        None
    }
}

/// 檢查操作與參數的形狀是否符合 P1a 契約。
///
/// 成功回 `Ok(())`；形狀不符回 `OpError`（`unsupported_in_phase` 或
/// `invalid_argument`）。不檢查必填欄位與內容語意，那是執行期的事。
pub fn admit(op: &str, args: &Value) -> Result<(), OpError> {
    if let Some(phase) = deferred_phase(op) {
        return Err(unsupported(format!("P1a 不支援 {op}（{phase}）")));
    }
    let spec = match op_spec(op) {
        Some(spec) => spec,
        None => return Err(invalid(format!("未知的操作 {op}"))),
    };

    let map = if args.is_null() {
        None
    } else if let Some(object) = args.as_object() {
        Some(object)
    } else {
        return Err(invalid(format!("{op} 的參數必須是物件")));
    };

    if let Some(map) = map {
        for field in map.keys() {
            if spec.fields.contains(&field.as_str()) {
                continue;
            }
            if spec.legacy_fields.contains(&field.as_str()) {
                return Err(unsupported(format!("P1a 不支援 {op}.{field}")));
            }
            return Err(invalid(format!("無效的欄位 {op}.{field}")));
        }
    }

    // action 值：省略視為 drain（只對 inbox 有意義）；舊版 action 拒收，未知值無效。
    if let Some(action) = map.and_then(|map| map.get("action")) {
        match str_value(action) {
            Some(value)
                if spec.supported_actions.contains(&value)
                    || (spec.supported_actions.is_empty() && value == "drain") =>
            {
                // 接受。
            }
            Some(value) if spec.legacy_actions.contains(&value) => {
                return Err(unsupported(format!("P1a 不支援 {op}.action={value}")));
            }
            Some(value) => {
                return Err(invalid(format!("無效的 {op}.action 值 {value}")));
            }
            None => {
                return Err(invalid(format!("無效的 {op}.action 值")));
            }
        }
    }

    // 列舉值：拒收值 → unsupported，接受值 → 通過，其他 → invalid。
    let check_value = |field: &str| -> Result<(), OpError> {
        let value = match map.and_then(|map| map.get(field)) {
            Some(value) => value,
            None => return Ok(()),
        };
        for (refused_field, refused) in spec.refused_values {
            if *refused_field == field {
                if let Some(text) = str_value(value) {
                    if refused.contains(&text) {
                        return Err(unsupported(format!("P1a 不支援 {op}.{field}={text}")));
                    }
                }
            }
        }
        for (supported_field, allowed) in spec.supported_values {
            if *supported_field == field {
                let accepted = match str_value(value) {
                    Some(text) => allowed.contains(&text),
                    None => false,
                };
                if !accepted {
                    return Err(invalid(format!("無效的 {op}.{field} 值")));
                }
            }
        }
        Ok(())
    };

    for (field, _) in spec.supported_values {
        check_value(field)?;
    }
    for (field, _) in spec.refused_values {
        if !spec.supported_values.iter().any(|(name, _)| name == field) {
            check_value(field)?;
        }
    }

    Ok(())
}
