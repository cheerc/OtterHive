//! T5 Lane A GREEN：訊息的投遞與收取（契約 §6.1–§6.3）。
//!
//! 對外兩個函式：[`send`] 與 [`inbox`]；daemon 的 dispatch 端負責授權（§2）、
//! outcome 映射與喚醒排程（§6.4）。
//!
//! ## 交易的邊界（§6.1）
//!
//! 「寫入在同一交易內完成並 fsync 後才回應。喚醒在寫入之後進行；喚醒失敗不
//! 撤回訊息。」所以 [`send`] 只做 store 交易，**完全不碰 PTY**，並把待喚醒的
//! 收件人（`message_id` 為 `Some` 者）交給 dispatch 端在提交後使用。
//!
//! ## `Err` 與 `Ok` 的分界
//!
//! [`send`] 的 `Err` 只有**形狀層**的錯誤（收件人三擇一沒給或都給、`message`
//! 空或超過 64 KiB、識別字串超過 256 字元）與 `internal`：這些是 §3 的
//! 「在任何變更前拒絕」，本來就沒有收件人清單可言，`value` 為 `null` 是正確
//! 的。
//!
//! 只要收件人清單解析成功就回 `Ok`——**包括全部被拒**。此時回傳的向量已完整
//! （`queued` 與 `refused` 都在裡面），outcome 由 dispatch 端依 `queued` 的筆數
//! 決定：全部→`completed`、部分→`partial`、0 筆→`refused` 並取第一個被拒收件人
//! 的錯誤碼。
//!
//! 這個分界線不是風格選擇而是結構要求：`Err` 通道只有
//! `OpError { code, message, retryable, next }`，**沒有收件人清單**。走 `Err`
//! 就會迫使 dispatch 端重新解析一次參數去湊 `value.recipients`，把「收件人選擇／
//! 去重／排除 sender」複製一份，而兩份必然漂移。

use serde_json::{json, Value};

use crate::config::Config;
use crate::daemon::auth::Caller;
use crate::protocol::{ErrorCode, OpError};
use crate::store::{AckOutcome, MessageRow, NewMessage, Store, StoreError, TeamRow, Tx};

/// §6.1：`message` 的上限是 64 KiB（**bytes**，不是字符數）。
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// §6.3：drain 一次最多取 48 KiB 的 body bytes（至少一則）。
pub const DRAIN_BUDGET_BYTES: usize = 48 * 1024;
/// §6.1：`correlation_id`／`parent_id`／`thread_id` 的上限。
pub const MAX_IDENTIFIER_CHARS: usize = 256;

/// `send` 的收件人結果。
///
/// `message_id` 與 `error` 互斥：`queued` 前者為 `Some`、後者為 `None`；
/// `refused` 反之。
#[derive(Debug, Clone, PartialEq)]
pub struct SendRecipient {
    /// 收件 instance 的名稱（已去重、已排除 sender）。
    pub instance: String,
    /// 成功寫入時的訊息 ID。
    pub message_id: Option<String>,
    /// 被拒絕的原因（契約的錯誤碼）。
    pub error: Option<OpError>,
    /// 契約 §6.1 的 `wake` 欄位（值為 §6.4 的四個之一）。
    ///
    /// **`messaging` 本身不填這個欄位。** 本模組到這裡為止完全不知道 §6.4 的
    /// 存在：訊息寫入與喚醒是兩件事，喚醒發生在交易提交**之後**（§6.1「喚醒
    /// 失敗不撤回訊息」），而那時資料庫鎖已經釋放。
    ///
    /// 填值者是 daemon 的 dispatch 端：成功寫入的收件人填 [`crate::wake`] 的
    /// 排程結果，被拒的收件人填 `not_running`——§6.4 把 `not_running` 定義成
    /// 「收件人不在 `running`」，而被拒的收件人（`not_found`／`team_acl_denied`
    /// …）確實不在 `running`。把它留成 `None` 會讓呼叫端無法區分「被拒所以
    /// 沒有 wake 值」與「喚醒機制本身壞了」，而 §10 的設計重點正是「不假裝
    /// 成功、不靜默忽略」。
    ///
    /// 這個欄位在 `messaging` 內恆為 `None`；它是**接收端**的槽，不是本模組
    /// 的輸出。
    pub wake: Option<String>,
}

impl SendRecipient {
    fn queued(instance: &str, message_id: String) -> SendRecipient {
        SendRecipient {
            instance: instance.to_owned(),
            message_id: Some(message_id),
            error: None,
            wake: None,
        }
    }

    fn refused(instance: &str, error: OpError) -> SendRecipient {
        SendRecipient {
            instance: instance.to_owned(),
            message_id: None,
            error: Some(error),
            wake: None,
        }
    }

    /// 這個收件人是否成功寫入（也就是要不要喚醒它）。
    pub fn is_queued(&self) -> bool {
        self.message_id.is_some()
    }
}

/// 呼叫端的身分（§2：sender 一律由憑證推導，不從請求讀）。
struct Sender {
    kind: &'static str,
    id: Option<String>,
    name: String,
    /// 呼叫端名稱在設定檔的 `managers` 裡（§2）。
    is_manager: bool,
    /// 呼叫端所屬的 team 與該 team 的 orchestrator 名稱（§6.2）。
    team: Option<(String, String)>,
}

impl Sender {
    fn from(store: &mut Store, caller: &Caller, config: &Config) -> Result<Sender, OpError> {
        match caller {
            Caller::Operator => Ok(Sender {
                kind: "operator",
                id: None,
                name: String::from("operator"),
                is_manager: true,
                team: None,
            }),
            Caller::Instance(id) => {
                // 依 ID 查呼叫端是**交易外**的唯讀查詢（`Store::instance`）；
                // §2 的身分資料不需要和寫入放在同一個交易裡。
                let instance = store
                    .instance(id)
                    .map_err(|error| internal(format!("讀取呼叫端失敗：{error}")))?
                    .filter(|row| row.deleted_ms.is_none())
                    .ok_or_else(|| {
                        internal(format!("呼叫端 instance {id} 不存在（認證後不應發生）"))
                    })?;
                let name = instance.name;
                let is_manager = config.managers.iter().any(|item| item == &name);
                let team = store
                    .transaction(|tx| tx.team_of_instance_name(&name))
                    .map_err(|error| store_error("查詢呼叫端所屬 team 失敗", error))?;
                Ok(Sender {
                    kind: "instance",
                    id: Some(instance.id),
                    name,
                    is_manager,
                    team,
                })
            }
        }
    }
}

/// 收件人的選擇子（§6.1：`instance`／`instances`／`team` 三擇一）。
struct RecipientSelectors {
    instance: Option<String>,
    instances: Option<Vec<String>>,
    team: Option<String>,
}

impl RecipientSelectors {
    /// 三擇一：都沒給或都給都是形狀錯誤（`Err` 通道，`value: null`）。
    fn parse(args: &Value) -> Result<RecipientSelectors, OpError> {
        let present: Vec<&str> = ["instance", "instances", "team"]
            .into_iter()
            .filter(|field| args.get(*field).is_some())
            .collect();
        if present.len() != 1 {
            return Err(invalid(String::from(
                "send 需要 instance／instances／team 其中之一（且只能一個）",
            )));
        }
        let instance = match args.get("instance") {
            Some(value) => Some(required_name(value, "instance")?),
            None => None,
        };
        let instances = match args.get("instances") {
            Some(value) => Some(string_list(value, "instances")?),
            None => None,
        };
        let team = match args.get("team") {
            Some(value) => Some(required_name(value, "team")?),
            None => None,
        };
        Ok(RecipientSelectors {
            instance,
            instances,
            team,
        })
    }

    /// 解析成實際的收件人名稱（已去重）。
    ///
    /// - `instance`：單一收件人。
    /// - `instances`：明確清單，去重並保留第一次出現的順序（§6.1）。
    /// - `team`：team 的**所有**成員。operator 不是成員所以全部收；instance
    ///   caller 排除自己（§6.1「sender 不會收到自己的訊息」）。
    fn resolve(&self, tx: &Tx<'_>) -> Result<Vec<String>, OpError> {
        if let Some(team) = &self.team {
            let exists = tx
                .team(team)
                .map_err(|error| internal(format!("讀取 team {team} 失敗：{error}")))?
                .is_some();
            if !exists {
                return Err(not_found(format!("team {team} 不存在")));
            }
            let members = tx
                .team_members(team)
                .map_err(|error| internal(format!("讀取 team {team} 成員失敗：{error}")))?;
            return Ok(dedup(&members));
        }
        if let Some(single) = &self.instance {
            return Ok(vec![single.clone()]);
        }
        Ok(dedup(&self.instances.clone().unwrap_or_default()))
    }

    /// 從 team 廣播排除 sender（§6.1）。
    ///
    /// `instance`／`instances` **不**排除：sender 送給自己是一則合法訊息，
    /// `the_sender_is_derived_from_the_credential` 與
    /// `ack_without_delivery_and_ack_of_foreign_message_are_refused` 都靠這點。
    fn exclude_team_sender(&self, names: &mut Vec<String>, sender: &Sender) {
        if self.team.is_none() || sender.kind != "instance" {
            return;
        }
        names.retain(|name| name != &sender.name);
    }
}

/// §6.1：`send`。
///
/// 回傳的 `Ok` **不代表**訊息送達：全部收件人被拒時仍然是 `Ok`（向量非空、
/// 每項都帶 `error`），由 dispatch 端映射成 `refused`。只有形狀層錯誤與
/// `internal` 走 `Err`。
pub fn send(
    store: &mut Store,
    caller: &Caller,
    args: &Value,
    config: &Config,
) -> Result<Vec<SendRecipient>, OpError> {
    // --- 形狀層：任何 store 存取之前完成（§3「多數拒絕在任何變更前」）---
    let body = required_message(args)?;
    let correlation_id = optional_identifier(args, "correlation_id")?;
    let parent_id = optional_identifier(args, "parent_id")?;
    let thread_id = optional_identifier(args, "thread_id")?;
    let kind = match args.get("request_kind").and_then(Value::as_str) {
        // §6.1：省略時記為 `message`（`admit` 已擋掉其他值）。
        None => String::from("message"),
        Some(other) => other.to_owned(),
    };
    let selectors = RecipientSelectors::parse(args)?;

    let sender = Sender::from(store, caller, config)?;

    store
        .transaction(|tx| {
            let mut names = selectors.resolve(tx).map_err(abort)?;
            selectors.exclude_team_sender(&mut names, &sender);

            // --- 逐收件人：先判收件權限（§6.2），再準備寫入 ---
            let mut outcomes: Vec<SendRecipient> = Vec::with_capacity(names.len());
            let mut messages: Vec<NewMessage> = Vec::with_capacity(names.len());
            let mut queued: Vec<SendRecipient> = Vec::new();
            for name in &names {
                if let Err(error) = check_recipient_permission(tx, &sender, name) {
                    outcomes.push(SendRecipient::refused(name, error));
                    continue;
                }
                let recipient_id = match tx
                    .live_instance_id(name)
                    .map_err(|error| internal(format!("查詢收件人 {name} 失敗：{error}")))
                {
                    Ok(Some(id)) => id,
                    Ok(None) => {
                        outcomes.push(SendRecipient::refused(
                            name,
                            not_found(format!("instance {name} 不存在或已刪除")),
                        ));
                        continue;
                    }
                    Err(error) => return Err(abort(error)),
                };
                messages.push(NewMessage {
                    recipient_id,
                    sender_kind: sender.kind.to_owned(),
                    sender_id: sender.id.clone(),
                    sender_name: sender.name.clone(),
                    kind: kind.clone(),
                    body: body.clone(),
                    correlation_id: correlation_id.clone(),
                    parent_id: parent_id.clone(),
                    thread_id: thread_id.clone(),
                });
                queued.push(SendRecipient::queued(name, String::new()));
            }

            // §6.1：寫入在同一交易內完成。`insert_message_batch` 回傳的 ID 依
            // 順序對應 `messages`，所以逐一回填到 `queued`。
            let ids = tx
                .insert_message_batch(&messages)
                .map_err(|error| store_error("寫入訊息失敗", error))
                .map_err(abort)?;
            for (recipient, id) in queued.iter_mut().zip(&ids) {
                recipient.message_id = Some(id.clone());
            }
            // 單一向量回傳：每個收件人一筆結果，`wake` 欄位由 dispatch 端在
            // 交易提交後填入。
            //
            // 這裡**不是** `(outcomes, queued.clone())`——那是把同一份資料回傳
            // 兩次，讓 dispatch 端必須再查一次把它對回去（`Vec::append` 會清空
            // 來源，所以「把 queued 搬到 outcomes 之外的同時保留它」根本做不到，
            // 舊的 `SendOutcome` tuple 別名正是因此恆回空向量，讓 `wake_targets`
            // 永遠是空的）。`wake` 現在是每一筆自己的欄位，不需要第二份清單。
            outcomes.append(&mut queued);
            Ok::<Vec<SendRecipient>, StoreError>(outcomes)
        })
        // 交易層的失敗（`BEGIN`／`COMMIT`／回滾）與被 `abort` 包起來的契約錯誤
        // 都在這裡還原：前者是 `internal`，後者取回原本的錯誤碼。
        .map_err(resume)
}

/// 把契約錯誤包進 [`StoreError::Invalid`]，讓它穿過 `Store::transaction` 的閉包。
///
/// 這是刻意的：閉包回 `Err` 會整筆回滾（§6.1／§8.1 的「拒絕時不留下部分變更」
/// 正是靠它），所以契約層的拒絕也必須走 `Err` 才會觸發回滾。`StoreError` 只有
/// `Invalid(String)` 能承載 `OpError` 而不被誤判成別種錯誤，所以把 `OpError`
/// **完整序列化**（保留 code／message／retryable／next）加上一個不可能與他人
/// 衝突的前綴，由 [`resume`] 還原。
///
/// `pub(crate)` 是因為 `teams` 的 create／update／delete 有同樣的需求——共用
/// 同一份實作，而不是複製一份會漂移的。
pub(crate) const ABORT_PREFIX: &str = "otterhive-op-error\u{1}";

pub(crate) fn abort(error: OpError) -> StoreError {
    let encoded = serde_json::to_string(&error)
        .unwrap_or_else(|_| format!("{{\"code\":\"internal\",\"message\":{:?}}}", error.message));
    StoreError::Invalid(format!("{ABORT_PREFIX}{encoded}"))
}

/// [`abort`] 的逆操作：還原完整的契約錯誤。
///
/// 前綴對不上就當成一般的 store 錯誤（`internal`）——那代表 abort 從未被使用，
/// 是真正的內部失敗，不該被誤標成某個契約碼。序列化本身壞掉也走 `internal`
/// （fail-closed：不猜）。
pub(crate) fn resume(error: StoreError) -> OpError {
    let StoreError::Invalid(message) = error else {
        return store_error("交易失敗", error);
    };
    let Some(encoded) = message.strip_prefix(ABORT_PREFIX) else {
        return internal(message);
    };
    match serde_json::from_str::<OpError>(encoded) {
        Ok(op_error) => op_error,
        Err(_) => internal(String::from("交易錯誤無法還原（內部失敗）")),
    }
}

/// §6.2：收件權限。
///
/// operator 與 manager 可送給任何 instance。instance sender 的規則依序是：
/// 1. recipient 不屬於任何 team → 允許；
/// 2. 同一 team → 允許；
/// 3. recipient 是某 team 的 orchestrator，且 **sender 的名稱**在該 team 的
///    `accept_from` → 允許（以送出當下的名稱比對，§2 的刻意設計）；
/// 4. 其他 → `team_acl_denied`（不是 `unauthorized`）。
fn check_recipient_permission(
    tx: &Tx<'_>,
    sender: &Sender,
    recipient: &str,
) -> Result<(), OpError> {
    if sender.kind != "instance" || sender.is_manager {
        return Ok(());
    }
    let Some(recipient_team) = tx
        .team_name_of_instance(recipient)
        .map_err(|error| internal(format!("查詢收件人 {recipient} 所屬 team 失敗：{error}")))?
    else {
        // 規則 1：不屬於任何 team → 允許。
        return Ok(());
    };
    if let Some((own_team, _)) = &sender.team {
        if *own_team == recipient_team {
            // 規則 2：同一 team → 允許。
            return Ok(());
        }
    }
    let row = tx
        .team(&recipient_team)
        .map_err(|error| internal(format!("讀取 team {recipient_team} 失敗：{error}")))?
        .ok_or_else(|| internal(format!("team {recipient_team} 有成員卻沒有紀錄")))?;
    let orchestrator = orchestrator_name(tx, &row)?;
    if orchestrator == recipient
        && row
            .accept_from_names()
            .iter()
            .any(|name| name == &sender.name)
    {
        // 規則 3：orchestrator 的 accept_from 內 → 允許。
        return Ok(());
    }
    // 規則 4。
    Err(acl_denied(format!(
        "instance {} 不可送給 {recipient}（不同 team，且 {recipient} 不是 accept_from 允許的對象）",
        sender.name
    )))
}

/// `orchestrator_id` 對應的 instance 名稱。
///
/// store 層只有 `live_instance_id`（名稱→ID）而沒有 ID→名稱的公開查詢
/// （`Tx::instance` 是私有的），所以在交易內以 `team_members` 的名稱清單逐一
/// 比對 ID。team 的成員數是個位數，這點成本可以忽略，而它讓 §6.2 的判斷留在
/// 同一個交易裡。
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

/// §6.3：`inbox`。`action` 省略或 `drain` → drain；`ack` → ack。
pub fn inbox(
    store: &mut Store,
    caller: &Caller,
    args: &Value,
    lease_ms: u64,
    now_ms: i64,
) -> Result<Value, OpError> {
    let Caller::Instance(id) = caller else {
        // §2 的表格最後一列：inbox 對 operator 是「否」，而且 §6.1 明說 P1a
        // 沒有 operator 收件匣。
        return Err(unauthorized(String::from(
            "operator 沒有收件匣（契約 §2／§6.1）",
        )));
    };
    match args.get("action").and_then(Value::as_str) {
        Some("ack") => ack(store, id, args, now_ms),
        // §6.3：`action` 省略即 drain（`admit` 已擋掉其他值）。
        _ => drain(store, id, lease_ms, now_ms),
    }
}

/// §6.3：drain。只能讀自己的 inbox。
fn drain(
    store: &mut Store,
    instance_id: &str,
    lease_ms: u64,
    now_ms: i64,
) -> Result<Value, OpError> {
    let outcome = store
        .transaction(|tx| tx.drain_messages(instance_id, DRAIN_BUDGET_BYTES, now_ms, lease_ms))
        .map_err(|error| store_error("drain 失敗", error))?;
    let messages: Vec<Value> = outcome.messages.iter().map(message_value).collect();
    Ok(json!({
        "delivery_token": outcome.delivery_token,
        "lease_expires_ms": outcome.lease_expires_ms,
        "remaining": outcome.remaining,
        "messages": messages,
    }))
}

/// 一則訊息在 drain 結果裡的形狀（§6.3）。
fn message_value(message: &MessageRow) -> Value {
    json!({
        "id": message.id,
        "from": {"kind": message.sender_kind, "name": sender_name_value(message)},
        "kind": message.kind,
        "body": message.body,
        "correlation_id": message.correlation_id,
        "parent_id": message.parent_id,
        "thread_id": message.thread_id,
        "created_ms": message.created_ms,
        "delivery_count": message.delivery_count,
    })
}

/// §6.3 的 `from.name`：`instance` 送出者帶名稱；`operator` 送出者沒有可用的
/// instance 名稱，回 `null`。
fn sender_name_value(message: &MessageRow) -> Value {
    if message.sender_kind == "instance" {
        Value::String(message.sender_name.clone())
    } else {
        Value::Null
    }
}

/// §6.3：ack。
fn ack(store: &mut Store, instance_id: &str, args: &Value, now_ms: i64) -> Result<Value, OpError> {
    let message_id = match args.get("message_id").and_then(Value::as_str) {
        Some(text) if !text.is_empty() => text.to_owned(),
        Some(_) => return Err(invalid(String::from("message_id 不得為空"))),
        None => return Err(invalid(String::from("ack 需要 message_id"))),
    };
    let token = match args.get("delivery_token").and_then(Value::as_str) {
        Some(text) if !text.is_empty() => text.to_owned(),
        Some(_) => return Err(invalid(String::from("delivery_token 不得為空"))),
        None => return Err(invalid(String::from("ack 需要 delivery_token"))),
    };
    let outcome = store
        .transaction(|tx| tx.ack_message(instance_id, &message_id, &token, now_ms))
        .map_err(|error| store_error("ack 失敗", error))?;
    match outcome {
        AckOutcome::Acked => Ok(json!({"message_id": message_id, "acked": true})),
        // §6.3：重複 ack 同一組回 `completed`，並帶 `value.already_acked: true`。
        AckOutcome::AlreadyAcked => Ok(json!({
            "message_id": message_id, "acked": true, "already_acked": true,
        })),
        AckOutcome::Stale => Err(OpError {
            code: ErrorCode::DeliveryTokenStale,
            message: String::from("delivery_token 不符或 lease 已過期"),
            retryable: false,
            next: None,
        }),
        AckOutcome::NeverDelivered => Err(OpError {
            code: ErrorCode::NeverDelivered,
            message: String::from("該訊息從未被投遞"),
            retryable: false,
            next: None,
        }),
        AckOutcome::NotFound => Err(OpError {
            code: ErrorCode::NotFound,
            message: String::from("訊息不存在或不屬於自己"),
            retryable: false,
            next: None,
        }),
        AckOutcome::Archived => Err(OpError {
            code: ErrorCode::NotFound,
            message: String::from("訊息已封存（§4.5），不可 ack"),
            retryable: false,
            next: None,
        }),
        AckOutcome::Internal => Err(internal(String::from("ack 寫入失敗"))),
    }
}

fn required_message(args: &Value) -> Result<String, OpError> {
    let text = match args.get("message").and_then(Value::as_str) {
        Some(text) if !text.is_empty() => text.to_owned(),
        Some(_) => return Err(invalid(String::from("message 不得為空"))),
        None => return Err(invalid(String::from("send 需要 message"))),
    };
    // §6.1 的上限是 **bytes**：多字元 UTF-8 的訊息在字符數遠小於 64 KiB 時就
    // 已經超標。
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(invalid(format!(
            "message 不得超過 {MAX_MESSAGE_BYTES} bytes（實際 {}）",
            text.len()
        )));
    }
    Ok(text)
}

fn optional_identifier(args: &Value, field: &str) -> Result<Option<String>, OpError> {
    match args.get(field) {
        Some(Value::String(text)) => {
            // §6.1：以**字符**數為上限（不同於 `message` 的 bytes）。
            if text.chars().count() > MAX_IDENTIFIER_CHARS {
                return Err(invalid(format!(
                    "{field} 不得超過 {MAX_IDENTIFIER_CHARS} 字元（實際 {}）",
                    text.chars().count()
                )));
            }
            Ok(Some(text.clone()))
        }
        Some(_) => Err(invalid(format!("{field} 必須是字串"))),
        None => Ok(None),
    }
}

/// 收件人名稱（單數選擇子）。
fn required_name(value: &Value, field: &str) -> Result<String, OpError> {
    match value.as_str() {
        Some(text) if !text.is_empty() => Ok(text.to_owned()),
        Some(_) => Err(invalid(format!("{field} 不得為空"))),
        None => Err(invalid(format!("{field} 必須是字串"))),
    }
}

/// 名稱陣列：去重並保留第一次出現的順序（§6.1「收件人名單去重」）。
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

fn dedup(names: &[String]) -> Vec<String> {
    let mut result: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        if !result.contains(name) {
            result.push(name.clone());
        }
    }
    result
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

fn acl_denied(message: String) -> OpError {
    OpError {
        code: ErrorCode::TeamAclDenied,
        message,
        retryable: false,
        next: None,
    }
}

fn unauthorized(message: String) -> OpError {
    OpError {
        code: ErrorCode::Unauthorized,
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

fn store_error(context: &str, error: StoreError) -> OpError {
    match error {
        StoreError::NameConflict(name) => OpError {
            code: ErrorCode::NameConflict,
            message: format!("{context}：{name} 已被使用"),
            retryable: false,
            next: None,
        },
        other => internal(format!("{context}：{other}")),
    }
}
