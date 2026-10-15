//! T5 Lane A GREEN：team 操作（契約 §8.1）。
//!
//! 對外只有 [`team_operation`]：daemon 的 dispatch 端完成授權（§2）後，把
//! `team` 的參數交給它，由它回 §8.1 的 `value` 或 `OpError`。
//!
//! ## 交易的邊界（§8.1）
//!
//! 「所有檢查在同一交易內、任何 team 異動前完成，拒絕時不留下部分
//! add／remove」是本模組的核心不變量。`Store::transaction` 以 `BEGIN
//! IMMEDIATE` 序列化，閉包回 `Err` 會整筆回滾——**包括** `insert_team` 與
//! `set_team_members` 這種「先寫 team 再寫成員」的兩段式寫入。
//!
//! 這也是為什麼契約層的拒絕要包成 [`StoreError`] 再穿過閉包（[`messaging`]
//! 裡的 `abort`／`resume`）：只有走 `Err` 才會觸發回滾。
//!
//! ## 授權不在這裡
//!
//! §2 的矩陣（team 異動只有 operator、instance 只能 `list`）由 daemon 的
//! `require_permission` 判斷；本模組假設呼叫端已經通過該檢查。

use serde_json::{json, Value};

use crate::daemon::lifecycle::validate_name;
use crate::messaging::{abort, resume};
use crate::protocol::{ErrorCode, OpError};
use crate::store::{Store, StoreError, TeamRow, Tx};

/// §8.1 的四個 action。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamAction {
    Create,
    Update,
    Delete,
    List,
}

impl TeamAction {
    /// 解析 `args.action`。
    ///
    /// §8.1 的每個 action 都要明寫，**沒有預設值**：省略 action 回
    /// `invalid_argument`（§3：未知的值不是 `unsupported`）。
    pub fn parse(args: &Value) -> Result<TeamAction, OpError> {
        match args.get("action").and_then(Value::as_str) {
            Some("create") => Ok(TeamAction::Create),
            Some("update") => Ok(TeamAction::Update),
            Some("delete") => Ok(TeamAction::Delete),
            Some("list") => Ok(TeamAction::List),
            Some(other) => Err(invalid(format!("無效的 team.action 值 {other}"))),
            None => Err(invalid(String::from(
                "team 需要 action（create／update／delete／list）",
            ))),
        }
    }
}

/// `team` 的請求。`action` 由呼叫端從 `args` 解析後傳入。
pub struct TeamRequest<'a> {
    pub action: TeamAction,
    pub name: &'a str,
    pub args: &'a Value,
}

/// §8.1：team 操作，回 §8.1 的 `value`；失敗回 `OpError`。
///
/// `store` 是**交易外**的 handle：team 的每個 action 都是「讀取 → 驗證 →
/// 寫入」三段，驗證與寫入必須落在同一個交易裡（§8.1），所以由本函式自己開
/// 交易，而不是接受一個已經在交易內的 `&Tx`。
pub fn team_operation(store: &mut Store, request: TeamRequest<'_>) -> Result<Value, OpError> {
    match request.action {
        TeamAction::List => list_teams(store),
        TeamAction::Create => create_team(store, request),
        TeamAction::Update => update_team(store, request),
        TeamAction::Delete => delete_team(store, request),
    }
}

/// §8.1 的 `value.teams` 一項。整段在**一個交易**內完成，讓列出的欄位來自
/// 一致的快照。
fn team_value(store: &mut Store, name: &str) -> Result<Value, OpError> {
    let value = store
        .transaction(|tx| {
            let row = tx
                .team(name)
                .map_err(|error| store_error("讀取 team 失敗", error))?
                .ok_or_else(|| abort(internal(format!("team {name} 有成員卻沒有紀錄"))))?;
            let orchestrator = orchestrator_name(tx, &row).map_err(abort)?;
            let members = tx
                .team_members(name)
                .map_err(|error| store_error("讀取 team 成員失敗", error))?;
            Ok::<Value, StoreError>(json!({
                "name": row.name,
                "orchestrator": orchestrator,
                "members": members,
                "description": row.description,
                "accept_from": row.accept_from_names(),
                "deployment": row.deployment,
            }))
        })
        .map_err(resume)?;
    Ok(value)
}

/// §8.1：`list`。每項含 `name`、`orchestrator`、`members`、`description`、
/// `accept_from`、`deployment`。
///
/// 沒有「列出所有 team」的 store 查詢，所以從**成員**反推 team 名稱：每個
/// instance 的 team 由 `team_name_of_instance` 決定，去重後排序。
fn list_teams(store: &mut Store) -> Result<Value, OpError> {
    let teams = store
        .transaction(|tx| {
            let instances = tx
                .all_live_instances()
                .map_err(|error| store_error("列出 instance 失敗", error))?;
            let mut names: Vec<String> = Vec::new();
            for instance in &instances {
                let Some(name) = tx
                    .team_name_of_instance(&instance.name)
                    .map_err(|error| store_error("查詢 instance 所屬 team 失敗", error))?
                else {
                    continue;
                };
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            names.sort();
            let mut teams = Vec::with_capacity(names.len());
            for name in names {
                let row = tx
                    .team(&name)
                    .map_err(|error| store_error("讀取 team 失敗", error))?
                    .ok_or_else(|| abort(internal(format!("team {name} 有成員卻沒有紀錄"))))?;
                let orchestrator = orchestrator_name(tx, &row).map_err(abort)?;
                let members = tx
                    .team_members(&name)
                    .map_err(|error| store_error("讀取 team 成員失敗", error))?;
                teams.push(json!({
                    "name": row.name,
                    "orchestrator": orchestrator,
                    "members": members,
                    "description": row.description,
                    "accept_from": row.accept_from_names(),
                    "deployment": row.deployment,
                }));
            }
            Ok::<Value, StoreError>(json!({ "teams": teams }))
        })
        .map_err(resume)?;
    Ok(teams)
}

/// §8.1：`create`。
///
/// 檢查順序刻意是「先全部驗完，再開始寫」：`members` 必須是非空的已存在
/// instance 集合、`orchestrator` 必須在其中、且沒有任何一個成員已屬於另一個
/// team。任何一項不成立都在 `insert_team` 之前返回。
fn create_team(store: &mut Store, request: TeamRequest<'_>) -> Result<Value, OpError> {
    let name = request.name.to_owned();
    validate_name(&name).map_err(invalid)?;
    // 形狀層（純參數檢查）在開交易之前完成。
    let description = optional_string(request.args, "description")?;
    let accept_from = name_list(request.args, "accept_from")?;
    let members = name_list(request.args, "members")?;
    if members.is_empty() {
        return Err(invalid(String::from("members 不得為空")));
    }
    let orchestrator = required_string(request.args, "orchestrator")?;

    store
        .transaction(|tx| {
            if tx
                .team(&name)
                .map_err(|error| store_error("查詢 team 失敗", error))?
                .is_some()
            {
                return Err(abort(name_conflict(format!("team {name} 已被使用"))));
            }
            let ids = resolve_members(tx, &members, None)?;
            let orchestrator_id = ids
                .iter()
                .zip(&members)
                .find(|(_, member)| *member == &orchestrator)
                .map(|(id, _)| id.clone())
                .ok_or_else(|| {
                    abort(invalid(format!(
                        "orchestrator {orchestrator} 不在 members 裡"
                    )))
                })?;

            tx.insert_team(
                &name,
                &orchestrator_id,
                description.as_deref(),
                &accept_from,
                None,
            )
            .map_err(|error| store_error("建立 team 失敗", error))?;
            tx.set_team_members(&name, &ids)
                .map_err(|error| store_error("寫入 team 成員失敗", error))?;
            Ok::<(), StoreError>(())
        })
        .map_err(resume)?;

    team_value(store, &name)
}

/// §8.1：`update`。
///
/// `add`／`remove`／`orchestrator`／`description`／`accept_from` 在同一次操作內
/// 合併成一組**變更後**的成員，再一次驗證：orchestrator 必須在變更後的 members
/// 裡。移除原 orchestrator 時若沒有同時指定新 orchestrator，這一步就以
/// `invalid_argument` 拒絕，而 `set_team_members` 尚未執行——所以不會留下部分
/// add／remove。
///
/// 省略 `description`／`accept_from` 代表**沿用現值**（§8.1 的 update 是
/// 「取代指定的欄位」，不是清空）。
fn update_team(store: &mut Store, request: TeamRequest<'_>) -> Result<Value, OpError> {
    let name = request.name.to_owned();
    validate_name(&name).map_err(invalid)?;
    let args = request.args;
    // 形狀層在開交易之前完成。
    let add = name_list(args, "add")?;
    let remove = name_list(args, "remove")?;
    let new_orchestrator = optional_string(args, "orchestrator")?;
    let new_description = match args.get("description") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(invalid(String::from("description 必須是字串"))),
        None => None,
    };
    let new_accept_from = match args.get("accept_from") {
        Some(_) => Some(name_list(args, "accept_from")?),
        None => None,
    };

    store
        .transaction(|tx| {
            let row = tx
                .team(&name)
                .map_err(|error| store_error("讀取 team 失敗", error))?
                .ok_or_else(|| abort(not_found(format!("team {name} 不存在"))))?;

            let mut members = tx
                .team_members(&name)
                .map_err(|error| store_error("讀取 team 成員失敗", error))?;
            // 先移除、再加入：同一個名字同時出現時以 `remove` 為準，避免
            // 「移除後又被加回來」的隱形語意。
            members.retain(|member| !remove.contains(member));
            for member in &add {
                if !members.contains(member) {
                    members.push(member.clone());
                }
            }

            let orchestrator = match &new_orchestrator {
                Some(orchestrator) => orchestrator.clone(),
                None => orchestrator_name(tx, &row).map_err(abort)?,
            };
            if !members.contains(&orchestrator) {
                // §8.1：「update 若 remove 原 orchestrator，須在同一次操作指定
                // 仍在成員中的新 orchestrator」——移除與指定矛盾這兩種情況在這裡
                // 是同一個錯誤。
                return Err(abort(invalid(format!(
                    "orchestrator {orchestrator} 不在變更後的 members 裡"
                ))));
            }

            let ids = resolve_members(tx, &members, Some(&name))?;
            let orchestrator_id = ids
                .iter()
                .zip(&members)
                .find(|(_, member)| *member == &orchestrator)
                .map(|(id, _)| id.clone())
                .ok_or_else(|| abort(internal(String::from("orchestrator 不在成員清單裡"))))?;

            let description = new_description.clone().or(row.description.clone());
            let accept_from = new_accept_from
                .clone()
                .unwrap_or_else(|| row.accept_from_names());

            tx.update_team(
                &name,
                &orchestrator_id,
                description.as_deref(),
                &accept_from,
            )
            .map_err(|error| store_error("更新 team 失敗", error))?;
            tx.set_team_members(&name, &ids)
                .map_err(|error| store_error("寫入 team 成員失敗", error))?;
            Ok::<(), StoreError>(())
        })
        .map_err(resume)?;

    team_value(store, &name)
}

/// §8.1：`delete` 移除 team 與成員關係，但**不刪除 instance**。
///
/// 屬於某 deployment 的 team 只能由 teardown 刪（§8.4），直接 delete 回
/// `invalid_argument`。
fn delete_team(store: &mut Store, request: TeamRequest<'_>) -> Result<Value, OpError> {
    let name = request.name.to_owned();
    validate_name(&name).map_err(invalid)?;
    store
        .transaction(|tx| {
            let row = tx
                .team(&name)
                .map_err(|error| store_error("讀取 team 失敗", error))?
                .ok_or_else(|| abort(not_found(format!("team {name} 不存在"))))?;
            if let Some(deployment) = row.deployment.clone() {
                return Err(abort(invalid(format!(
                    "team {name} 屬於 deployment {deployment}，只能由 teardown 刪除（§8.4）"
                ))));
            }
            tx.delete_team(&name)
                .map_err(|error| store_error("刪除 team 失敗", error))?;
            Ok::<(), StoreError>(())
        })
        .map_err(resume)?;
    Ok(json!({ "name": name }))
}

/// 把成員名稱解析成 instance ID，並驗證 §8.1 的兩條成員規則。
///
/// - 成員必須是**已存在、未刪除**的 instance，否則 `not_found`。
/// - 一個 instance 最多屬於一個 team：已在**另一個** team 的成員回
///   `name_conflict`。`current_team` 是這次操作的目標 team（`None` = create），
///   在該 team 裡的成員不算衝突。
fn resolve_members(
    tx: &Tx<'_>,
    members: &[String],
    current_team: Option<&str>,
) -> Result<Vec<String>, StoreError> {
    let mut ids = Vec::with_capacity(members.len());
    for member in members {
        let Some(id) = tx
            .live_instance_id(member)
            .map_err(|error| store_error("查詢成員失敗", error))?
        else {
            return Err(abort(not_found(format!("成員 {member} 不存在或已刪除"))));
        };
        if let Some(existing) = tx
            .team_name_of_instance(member)
            .map_err(|error| store_error("查詢成員所屬 team 失敗", error))?
        {
            if Some(existing.as_str()) != current_team {
                return Err(abort(name_conflict(format!(
                    "instance {member} 已屬於 team {existing}"
                ))));
            }
        }
        ids.push(id);
    }
    Ok(ids)
}

/// `orchestrator_id` 對應的 instance 名稱。
///
/// store 層只有 `live_instance_id`（名稱→ID）而沒有 ID→名稱的公開查詢
/// （`Tx::instance` 是私有的），所以在交易內以 `team_members` 的名稱清單逐一
/// 比對 ID。team 的成員數是個位數，這點成本可以忽略。
fn orchestrator_name(tx: &Tx<'_>, row: &TeamRow) -> Result<String, OpError> {
    let members = tx
        .team_members(&row.name)
        .map_err(|error| internal(format!("讀取 team {} 成員失敗：{error}", row.name)))?;
    for member in members {
        let id = tx
            .live_instance_id(&member)
            .map_err(|error| internal(format!("查詢成員 {member} 失敗：{error}")))?;
        if id.as_deref() == Some(row.orchestrator_id.as_str()) {
            return Ok(member);
        }
    }
    Err(internal(format!(
        "team {} 的 orchestrator 不在成員清單裡",
        row.name
    )))
}

/// §8.1 的名稱陣列欄位（`members`／`accept_from`／`add`／`remove`）。
fn name_list(args: &Value, field: &str) -> Result<Vec<String>, OpError> {
    match args.get(field) {
        Some(value) => string_list(value, field),
        None => Ok(Vec::new()),
    }
}

/// 名稱陣列：去重並保留第一次出現的順序（§6.1 的去重語意；§8.1 的成員清單同理，
/// `members: ["a","a","b"]` 與 `["a","b"]` 等價）。
fn string_list(value: &Value, field: &str) -> Result<Vec<String>, OpError> {
    let Some(array) = value.as_array() else {
        return Err(invalid(format!("{field} 必須是字串陣列")));
    };
    let mut names: Vec<String> = Vec::with_capacity(array.len());
    for item in array {
        let text = item
            .as_str()
            .ok_or_else(|| invalid(format!("{field} 的每一項都必須是字串")))?;
        if !text.is_empty() && !names.iter().any(|existing| existing == text) {
            names.push(text.to_owned());
        }
    }
    Ok(names)
}

fn required_string(args: &Value, field: &str) -> Result<String, OpError> {
    match args.get(field) {
        Some(Value::String(text)) if !text.is_empty() => Ok(text.clone()),
        Some(Value::String(_)) => Err(invalid(format!("{field} 不得為空"))),
        Some(_) => Err(invalid(format!("{field} 必須是字串"))),
        None => Err(invalid(format!("缺少必填的 {field}"))),
    }
}

fn optional_string(args: &Value, field: &str) -> Result<Option<String>, OpError> {
    match args.get(field) {
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(invalid(format!("{field} 必須是字串"))),
        None => Ok(None),
    }
}

fn invalid(message: String) -> OpError {
    OpError {
        code: ErrorCode::InvalidArgument,
        message,
        retryable: false,
        next: None,
    }
}

fn not_found(message: String) -> OpError {
    OpError {
        code: ErrorCode::NotFound,
        message,
        retryable: false,
        next: None,
    }
}

fn name_conflict(message: String) -> OpError {
    OpError {
        code: ErrorCode::NameConflict,
        message,
        retryable: false,
        next: None,
    }
}

fn internal(message: String) -> OpError {
    OpError {
        code: ErrorCode::Internal,
        message,
        retryable: false,
        next: None,
    }
}

fn store_error(context: &str, error: StoreError) -> StoreError {
    match error {
        StoreError::NameConflict(name) => {
            abort(name_conflict(format!("{context}：{name} 已被使用")))
        }
        other => abort(internal(format!("{context}：{other}"))),
    }
}
