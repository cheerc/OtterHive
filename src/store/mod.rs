//! SQLite 狀態層：`state.sqlite3` 是 instance、team、訊息、deployment 的唯一權威
//! （契約 §1）。schema 見 `schema.sql`，契約邊界見 docs/specs/p1-coordination.md。
//!
//! 所有寫入都透過 [`Store::transaction`]，以 `BEGIN IMMEDIATE` 序列化並在閉包
//! 回傳 `Err` 時整筆回滾。

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::protocol::OperationResult;

/// 本層支援的 schema 版本（`meta.schema_version`）。
const SUPPORTED_SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = include_str!("schema.sql");

/// `instances` 的完整欄位清單，供三處查詢共用。
const INSTANCE_COLUMNS: &str = "id, name, spec, state, reason, generation, pid, pid_start, token_hash, deployment, deleted_ms, created_ms, updated_ms";

/// `instances` 的查詢式（依參數指定的欄位）。
fn instance_query(predicate: &str) -> String {
    format!("SELECT {INSTANCE_COLUMNS} FROM instances WHERE {predicate}")
}

/// Store 層可回報的錯誤。
///
/// [`StoreError::Contract`] 讓契約層的拒絕（§6.1／§8.1 的
/// `invalid_argument`／`not_found`／`name_conflict`／`team_acl_denied`）能穿過
/// `Store::transaction` 的閉包回傳型別。它們描述的正是「這筆交易不能成立」，
/// 是 store 交易的一部分，不是外部塞進來的東西——所以不需要任何序列化。
///
/// 刻意**不** derive `Clone`／`PartialEq`／`Eq`：`rusqlite::Error` 兩者都沒有，
/// 而為了讓一個變體可比較去泛化整個型別的 derives 沒有任何好處（現有 call site
/// 都只做 pattern match）。
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("live instance 名稱已被使用: {0}")]
    NameConflict(String),
    #[error("instance {id} 的 generation 已過期，目前為 {current}")]
    StaleGeneration { id: String, current: u64 },
    #[error("operation ID 已記錄過結果: {0}")]
    DuplicateOperation(String),
    #[error("資料庫 schema 版本 {found} 不受支援（本層支援 {supported}）")]
    UnsupportedSchema { found: i64, supported: i64 },
    #[error("{0}")]
    Invalid(String),
    /// 契約層的拒絕（`ErrorCode` 與契約 §3 一一對應）。
    ///
    /// 與 `Invalid(String)` 的差別是**可預期性**：`Invalid` 是本層內部的
    /// 不變量被破壞（呼叫端給了不該給的東西），而 `Contract` 是「請求本身
    /// 合法但不成立」（例如 §8.1 的成員已在另一個 team）。兩者都讓交易回滾，
    /// 但只有 `Contract` 會原樣回到呼叫端。
    #[error("{0}")]
    Contract(crate::protocol::OpError),
    #[error("SQLite 錯誤: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

impl From<crate::protocol::OpError> for StoreError {
    fn from(op_error: crate::protocol::OpError) -> Self {
        StoreError::Contract(op_error)
    }
}

/// 建立 instance 所需的輸入（`spec` 為 JSON 物件）。
#[derive(Debug, Clone, PartialEq)]
pub struct NewInstance {
    pub name: String,
    pub spec: Value,
}

/// `instances` 一列的完整欄位（契約 §4、§6）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceRow {
    pub id: String,
    pub name: String,
    pub spec: Value,
    pub state: String,
    pub reason: Option<String>,
    pub generation: u64,
    pub pid: Option<i64>,
    pub pid_start: Option<i64>,
    pub token_hash: Option<String>,
    pub deployment: Option<String>,
    pub deleted_ms: Option<i64>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// 寫入一則訊息所需的欄位（契約 §6.1）。
#[derive(Debug, Clone, PartialEq)]
pub struct NewMessage {
    pub recipient_id: String,
    pub sender_kind: String,
    pub sender_id: Option<String>,
    pub sender_name: String,
    pub kind: String,
    pub body: String,
    pub correlation_id: Option<String>,
    pub parent_id: Option<String>,
    pub thread_id: Option<String>,
}

/// `teams` 一列的完整欄位（契約 §8.1）。
#[derive(Debug, Clone, PartialEq)]
pub struct TeamRow {
    pub name: String,
    /// orchestrator 的 instance ID。
    pub orchestrator_id: String,
    pub description: Option<String>,
    /// `accept_from` 以 JSON 陣列字串存放；用 [`TeamRow::accept_from_names`] 解析。
    pub accept_from: String,
    /// 所屬 deployment（`Some` 表示只能由 teardown 刪除，§8.4）。
    pub deployment: Option<String>,
}

impl TeamRow {
    /// `accept_from` 的 instance 名稱清單。
    ///
    /// 壞掉的 JSON 回空 vec（而不是讓 team 讀取整個失敗）：`accept_from`
    /// 只會**放寬**收件權限，把它讀成空清單是 fail-closed 的方向。
    pub fn accept_from_names(&self) -> Vec<String> {
        serde_json::from_str::<Vec<String>>(&self.accept_from).unwrap_or_default()
    }
}

/// `messages` 一列的完整欄位（契約 §6.3 的 drain 結果）。
#[derive(Debug, Clone, PartialEq)]
pub struct MessageRow {
    pub id: String,
    pub recipient_id: String,
    pub sender_kind: String,
    pub sender_id: Option<String>,
    pub sender_name: String,
    pub kind: String,
    pub body: String,
    pub correlation_id: Option<String>,
    pub parent_id: Option<String>,
    pub thread_id: Option<String>,
    /// `unread`｜`delivering`｜`acked`｜`archived`。
    pub state: String,
    pub delivery_token: Option<String>,
    pub lease_expires_ms: Option<i64>,
    pub delivery_count: i64,
    pub created_ms: i64,
}

/// `inbox` ack 的結果（契約 §6.3 的四種失敗與兩種成功）。
///
/// 對應到契約的錯誤碼由 [`Tx::ack_message`] 的呼叫端翻譯：
/// `Stale` → `delivery_token_stale`、`NeverDelivered` → `never_delivered`、
/// `NotFound` → `not_found`、`Internal` → `internal`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    /// token 相符且 lease 未過期 → `acked`。
    Acked,
    /// 同一組 token 對已 ack 的訊息重複 ack → `completed`（`already_acked`）。
    AlreadyAcked,
    /// token 不符或 lease 已過期。
    Stale,
    /// 該訊息從未被投遞（`state` 仍是 `unread`，沒有 token）。
    NeverDelivered,
    /// 訊息 id 不存在，或屬於別人（§6.3「不是自己的訊息回 `not_found`」）。
    NotFound,
    /// 訊息已被 `delete_instance` 封存（§4.5：archived 不再可 drain，也不可 ack）。
    Archived,
    /// 寫入失敗。
    Internal,
}

/// 一次 drain 的結果（契約 §6.3）。
#[derive(Debug, Clone, PartialEq)]
pub struct DrainOutcome {
    /// 本批被標為 `delivering` 的訊息（`delivery_count` 已是遞增後的值）。
    pub messages: Vec<MessageRow>,
    /// 本批的 `delivery_token`；批次為空時 `None`（§6.3「沒有訊息時不發 token」）。
    pub delivery_token: Option<String>,
    pub lease_expires_ms: Option<i64>,
    /// 仍未 drain 的筆數（`unread` 或 lease 已過期的 `delivering`）。
    pub remaining: u64,
}

/// `operations` 一列（契約 §3 的去重記錄）。
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedOperation {
    pub id: String,
    pub op: String,
    pub caller: String,
    pub result: OperationResult,
    pub created_ms: i64,
}

/// 已開啟的 store。
pub struct Store {
    connection: Connection,
}

impl Store {
    /// 開啟（必要時建立）狀態資料庫。
    ///
    /// 先讀 `meta.schema_version` 再決定是否建表：版本存在且不相容時，在寫入任何
    /// DDL 之前回 [`StoreError::UnsupportedSchema`]，既有資料表與資料原封不動
    /// （`newer_schema_is_refused_and_left_untouched`）。
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        let mut connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match read_schema_version(&transaction)? {
            None => {
                transaction.execute_batch(SCHEMA)?;
                transaction.execute(
                    "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)",
                    params![SUPPORTED_SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(found) if found == SUPPORTED_SCHEMA_VERSION => {}
            Some(found) => {
                // 不相容：放棄交易（回滾），因此不會建立任何新表。
                return Err(StoreError::UnsupportedSchema {
                    found,
                    supported: SUPPORTED_SCHEMA_VERSION,
                });
            }
        }
        transaction.commit()?;
        Ok(Store { connection })
    }

    /// 在單一 `BEGIN IMMEDIATE` 交易內執行 `body`。
    ///
    /// `body` 回傳 `Err` 時整筆回滾，資料庫不留任何部分寫入
    /// （`rolled_back_transaction_leaves_no_partial_message`）。
    pub fn transaction<T>(
        &mut self,
        body: impl FnOnce(&Tx) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = {
            let handle = Tx {
                connection: &transaction,
            };
            body(&handle)
        };
        match outcome {
            Ok(value) => {
                transaction.commit()?;
                Ok(value)
            }
            // transaction 於離開作用域時回滾。
            Err(error) => Err(error),
        }
    }

    /// 依 operation ID 查回第一次記錄的結果（契約 §3 的重送去重）。
    pub fn recorded_operation(&self, id: &str) -> Result<Option<RecordedOperation>, StoreError> {
        self.connection
            .query_row(
                "SELECT id, op, caller, result, created_ms FROM operations WHERE id = ?1",
                params![id],
                |row| {
                    let encoded: String = row.get(3)?;
                    let result = serde_json::from_str(&encoded).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                    Ok(RecordedOperation {
                        id: row.get(0)?,
                        op: row.get(1)?,
                        caller: row.get(2)?,
                        result,
                        created_ms: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// 依 ID 查 instance；已刪除的歷史列仍會回傳。
    pub fn instance(&self, id: &str) -> Result<Option<InstanceRow>, StoreError> {
        self.connection
            .query_row(&instance_query("id = ?1"), params![id], read_instance_row)
            .optional()
            .map_err(StoreError::from)
    }

    /// 依名稱查尚未刪除的 instance（契約 §2：同名新建者取得新的 InstanceId）。
    pub fn live_instance_by_name(&self, name: &str) -> Result<Option<InstanceRow>, StoreError> {
        self.connection
            .query_row(
                &instance_query("name = ?1 AND deleted_ms IS NULL"),
                params![name],
                read_instance_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// 某收件 instance 的訊息數。
    pub fn message_count(&self, recipient_id: &str) -> Result<u64, StoreError> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE recipient_id = ?1",
                params![recipient_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as u64)
            .map_err(StoreError::from)
    }
}

/// 交易內可用的寫入操作。
pub struct Tx<'connection> {
    connection: &'connection Connection,
}

impl Tx<'_> {
    /// 建立 instance 定義。名稱已被未刪除的 instance 使用時回
    /// [`StoreError::NameConflict`]，且不取代原有實例。
    pub fn insert_instance(&self, new: &NewInstance) -> Result<InstanceRow, StoreError> {
        if self.existing_live_id(&new.name)?.is_some() {
            return Err(StoreError::NameConflict(new.name.clone()));
        }
        let id = Uuid::new_v4().to_string();
        let spec = serde_json::to_string(&new.spec)
            .map_err(|error| StoreError::Invalid(format!("序列化 instance spec 失敗: {error}")))?;
        let now = now_ms();
        let changed = self
            .connection
            .execute(
                "INSERT INTO instances(id, name, spec, state, reason, generation, created_ms, updated_ms) VALUES (?1, ?2, ?3, 'stopped', NULL, 0, ?4, ?4)",
                params![id, new.name, spec, now],
            )
            .map_err(|error| name_taken(error, &new.name))?;
        if changed != 1 {
            return Err(StoreError::NameConflict(new.name.clone()));
        }
        self.append_journal(Some(&id), Some(0), "instance_created", Some(&new.name))?;
        self.instance(&id)?
            .ok_or_else(|| StoreError::Invalid(format!("新建的 instance 查不到: {id}")))
    }

    /// 更新 lifecycle 狀態與原因（契約 §4.1 的狀態機）。
    ///
    /// `state` 為契約定義的字串（`stopped`／`starting`／`running`／
    /// `stopping`／`failed`／`recovery_required`）；`reason` 為附帶原因
    /// （`exited_during_startup`／`exec_failed`／`exited`／`daemon_restarted`），
    /// 無原因時傳 `None` 以清空。instance 不存在時回 [`StoreError::Invalid`]。
    pub fn set_lifecycle_state(
        &self,
        id: &str,
        state: &str,
        reason: Option<&str>,
    ) -> Result<(), StoreError> {
        if self.instance(id)?.is_none() {
            return Err(StoreError::Invalid(format!("instance 不存在: {id}")));
        }
        let changed = self.connection.execute(
            "UPDATE instances SET state = ?2, reason = ?3, updated_ms = ?4 WHERE id = ?1",
            params![id, state, reason, now_ms()],
        )?;
        if changed != 1 {
            return Err(StoreError::Invalid(format!(
                "設定 lifecycle 狀態未影響任何列: {id}"
            )));
        }
        Ok(())
    }

    /// 記錄本 generation 的程序紀錄與 token 雜湊（契約 §4.2-3）。
    ///
    /// 只在 spawn 成功後呼叫；與 `set_lifecycle_state` 分開，讓「程序已存在」
    /// 與「程序已確認停止」兩件事在資料庫上可分別觀察。
    pub fn record_process(
        &self,
        id: &str,
        pid: i32,
        pid_start: u64,
        token_hash: &str,
    ) -> Result<(), StoreError> {
        if self.instance(id)?.is_none() {
            return Err(StoreError::Invalid(format!("instance 不存在: {id}")));
        }
        let changed = self.connection.execute(
            "UPDATE instances SET pid = ?2, pid_start = ?3, token_hash = ?4, updated_ms = ?5 \
             WHERE id = ?1",
            params![id, pid as i64, pid_start as i64, token_hash, now_ms()],
        )?;
        if changed != 1 {
            return Err(StoreError::Invalid(format!(
                "記錄程序紀錄未影響任何列: {id}"
            )));
        }
        Ok(())
    }

    /// 清掉程序紀錄（停止已確認、spawn 失敗或 delete 後）。
    ///
    /// 同時清掉 `token_hash`：舊 generation 的 token 一律失效（契約 §2）。
    pub fn clear_process(&self, id: &str) -> Result<(), StoreError> {
        if self.instance(id)?.is_none() {
            return Err(StoreError::Invalid(format!("instance 不存在: {id}")));
        }
        let changed = self.connection.execute(
            "UPDATE instances SET pid = NULL, pid_start = NULL, token_hash = NULL, \
             updated_ms = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        if changed != 1 {
            return Err(StoreError::Invalid(format!(
                "清除程序紀錄未影響任何列: {id}"
            )));
        }
        Ok(())
    }

    /// 依名稱查 instance，**包含已刪除的歷史列**。
    ///
    /// 只用於契約 §2 的名稱重用判斷（「同名新 instance 取得新的
    /// `InstanceId`」）與重啟收斂的歷史紀錄查核；是否為 live 請看
    /// `deleted_ms`。
    pub fn instance_by_name(&self, name: &str) -> Result<Option<InstanceRow>, StoreError> {
        self.connection
            .query_row(
                &instance_query("name = ?1 ORDER BY created_ms DESC LIMIT 1"),
                params![name],
                read_instance_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// 列出所有尚未刪除的 instance（契約 §4.7 的 `list_instances` 與 §4.6 的
    /// 重啟收斂都需要）。
    pub fn all_live_instances(&self) -> Result<Vec<InstanceRow>, StoreError> {
        let mut statement = self.connection.prepare(&instance_query(
            "deleted_ms IS NULL ORDER BY created_ms ASC",
        ))?;
        let rows = statement.query_map([], read_instance_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// 依 token 雜湊查尚未刪除的 instance（契約 §2 的 instance 認證：
    /// 舊 generation 的 token 因 `token_hash` 被覆寫而自然失效）。
    pub fn live_instance_by_token_hash(
        &self,
        hash: &str,
    ) -> Result<Option<InstanceRow>, StoreError> {
        self.connection
            .query_row(
                &instance_query("token_hash = ?1 AND deleted_ms IS NULL"),
                params![hash],
                read_instance_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// 依名稱查 instance 所屬的 team 與該 team 的 orchestrator 名稱
    /// （契約 §2：orchestrator 對自己 team 的成員有 interrupt／snapshot 權限）。
    ///
    /// 未屬任何 team 時回 `None`。
    pub fn team_of_instance_name(
        &self,
        name: &str,
    ) -> Result<Option<(String, String)>, StoreError> {
        self.connection
            .query_row(
                "SELECT teams.name, (SELECT i.name FROM instances i WHERE i.id = teams.orchestrator_id) \
                 FROM team_members JOIN teams ON teams.name = team_members.team \
                 JOIN instances ON instances.id = team_members.instance_id \
                 WHERE instances.name = ?1 AND instances.deleted_ms IS NULL",
                params![name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// 是否存在**其他**未刪除的 instance 使用同一 `working_directory`
    /// （契約 §5.4：`--continue` 接續的是該目錄最新的 session，不分 instance，
    /// 因此共享 working directory 時 `resume` 回 `resume_unavailable`）。
    pub fn other_instances_using_workdir(
        &self,
        id: &str,
        workdir: &str,
    ) -> Result<bool, StoreError> {
        // 契約 §5.4 判定的是「同一個 **canonical** working directory」。
        // 呼叫端已把路徑正規化，但正規化只做一次、資料可能是更早寫入的：這裡
        // 再比一次 `canonicalize` 後的路徑，`/var` 與 `/private/var` 才不會被
        // 當成兩個目錄。
        //
        // `json_extract` 對不是合法 JSON 的 spec 回 NULL，因此比較結果是
        // NULL（不等於 `?1`）——壞資料不會被當成「共用了目錄」。
        let canonical = match std::fs::canonicalize(workdir) {
            Ok(canonical) => canonical.to_string_lossy().into_owned(),
            // 目錄已經不存在：只能照原樣比。
            Err(_) => workdir.to_owned(),
        };
        self.connection
            .query_row(
                "SELECT 1 FROM instances WHERE id != ?1 AND deleted_ms IS NULL \
                 AND json_extract(spec, '$.working_directory') = ?2 LIMIT 1",
                params![id, canonical],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
            .map_err(StoreError::from)
    }

    /// 將某 instance 的未讀／發送中訊息封存（契約 §4.5 delete：訊息改為
    /// `archived`，不再可 drain）。
    pub fn archive_messages(&self, id: &str) -> Result<(), StoreError> {
        self.connection.execute(
            "UPDATE messages SET state = 'archived' WHERE recipient_id = ?1 \
             AND state IN ('unread', 'delivering')",
            params![id],
        )?;
        Ok(())
    }

    /// 寫入 journal（契約 §5.1 的 `skills_link_created` 需要 instance ID、
    /// link 路徑與 target）。
    pub fn append_public_journal(
        &self,
        instance_id: &str,
        generation: u64,
        kind: &str,
        detail: &str,
    ) -> Result<(), StoreError> {
        self.append_journal(Some(instance_id), Some(generation), kind, Some(detail))
    }

    /// 標記 instance 已刪除（契約 §4.5）；列本身保留以維持歷史與名稱重用規則。
    pub fn mark_instance_deleted(&self, id: &str) -> Result<(), StoreError> {
        let now = now_ms();
        let changed = self.connection.execute(
            "UPDATE instances SET deleted_ms = ?2, updated_ms = ?2, state = 'stopped', reason = 'deleted' WHERE id = ?1 AND deleted_ms IS NULL",
            params![id, now],
        )?;
        if changed == 1 {
            return Ok(());
        }
        match self.instance(id)? {
            Some(row) => Err(StoreError::Invalid(format!(
                "instance {id} 已被刪除（deleted_ms = {}）",
                row.deleted_ms.unwrap_or_default()
            ))),
            None => Err(StoreError::Invalid(format!("instance 不存在: {id}"))),
        }
    }

    /// 以 compare-and-swap 遞增 generation，回傳新值。
    ///
    /// `expected` 為 `Some` 且與目前值不符時回 [`StoreError::StaleGeneration`] 並帶
    /// 目前值；此時不遞增 `generation`，也不更新 `updated_ms`。更新本身以
    /// `WHERE generation = ?` 執行，讓預先檢查與寫入之間的漂移同樣被擋下。
    pub fn bump_generation(&self, id: &str, expected: Option<u64>) -> Result<u64, StoreError> {
        let row = self
            .instance(id)?
            .ok_or_else(|| StoreError::Invalid(format!("instance 不存在: {id}")))?;
        if let Some(expected) = expected {
            if row.generation != expected {
                return Err(StoreError::StaleGeneration {
                    id: id.to_owned(),
                    current: row.generation,
                });
            }
        }
        let next = row.generation + 1;
        let changed = self.connection.execute(
            "UPDATE instances SET generation = ?2, updated_ms = ?3 WHERE id = ?1 AND generation = ?4",
            params![id, next as i64, now_ms(), row.generation as i64],
        )?;
        if changed != 1 {
            let current = self
                .instance(id)?
                .map(|row| row.generation)
                .unwrap_or(row.generation);
            return Err(StoreError::StaleGeneration {
                id: id.to_owned(),
                current,
            });
        }
        self.append_journal(Some(id), Some(next), "generation_advanced", None)?;
        Ok(next)
    }

    /// 寫入一則訊息並回傳其 ID。
    pub fn insert_message(&self, message: &NewMessage) -> Result<String, StoreError> {
        let id = Uuid::new_v4().to_string();
        let changed = self.connection.execute(
            "INSERT INTO messages(id, recipient_id, sender_kind, sender_id, sender_name, kind, body, correlation_id, parent_id, thread_id, state, created_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'unread', ?11)",
            params![
                id,
                message.recipient_id,
                message.sender_kind,
                message.sender_id,
                message.sender_name,
                message.kind,
                message.body,
                message.correlation_id,
                message.parent_id,
                message.thread_id,
                now_ms(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Invalid("訊息寫入未影響任何列".to_owned()));
        }
        self.append_journal(
            Some(&message.recipient_id),
            None,
            "message_inserted",
            Some(&id),
        )?;
        Ok(id)
    }

    /// 記錄 operation 的結果。同一 ID 已存在時回
    /// [`StoreError::DuplicateOperation`]，且不覆寫原結果（契約 §3）。
    pub fn record_operation(
        &self,
        id: &str,
        op: &str,
        caller: &str,
        result: &OperationResult,
    ) -> Result<(), StoreError> {
        let encoded = serde_json::to_string(result).map_err(|error| {
            StoreError::Invalid(format!("序列化 operation result 失敗: {error}"))
        })?;
        let changed = self
            .connection
            .execute(
                "INSERT INTO operations(id, op, caller, result, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, op, caller, encoded, now_ms()],
            )
            .map_err(|error| duplicate_key(error, id))?;
        if changed != 1 {
            return Err(StoreError::DuplicateOperation(id.to_owned()));
        }
        Ok(())
    }

    /// 依名稱查一列 team（§8.1）。
    pub fn team(&self, name: &str) -> Result<Option<TeamRow>, StoreError> {
        self.connection
            .query_row(
                "SELECT name, orchestrator_id, description, accept_from, deployment \
                 FROM teams WHERE name = ?1",
                params![name],
                |row| {
                    Ok(TeamRow {
                        name: row.get(0)?,
                        orchestrator_id: row.get(1)?,
                        description: row.get(2)?,
                        accept_from: row.get(3)?,
                        deployment: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// 某 team 的成員 instance 名稱（排序後，讓測試與結果可預期）。
    pub fn team_members(&self, team: &str) -> Result<Vec<String>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT instances.name FROM team_members \
             JOIN instances ON instances.id = team_members.instance_id \
             WHERE team_members.team = ?1 AND instances.deleted_ms IS NULL \
             ORDER BY instances.name",
        )?;
        let rows = statement
            .query_map(params![team], |row| row.get::<_, String>(0))
            .map_err(StoreError::from)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        Ok(rows)
    }

    /// 依名稱查 instance 的 ID（`None` 表示不存在或已刪除）。
    pub fn live_instance_id(&self, name: &str) -> Result<Option<String>, StoreError> {
        self.existing_live_id(name)
    }

    /// 某 instance 所屬的 team 名稱（`None` 表示不屬任何 team）。
    pub fn team_name_of_instance(&self, name: &str) -> Result<Option<String>, StoreError> {
        self.connection
            .query_row(
                "SELECT team FROM team_members \
                 JOIN instances ON instances.id = team_members.instance_id \
                 WHERE instances.name = ?1 AND instances.deleted_ms IS NULL",
                params![name],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// 建立一列 team。
    ///
    /// 名稱已被使用時回 [`StoreError::NameConflict`]（§3）。成員關係由
    /// [`Tx::set_team_members`] 在同一交易內寫入——**兩者必須同交易**，否則會
    /// 留下沒有成員的 team（§8.1 要求檢查先於任何異動）。
    pub fn insert_team(
        &self,
        name: &str,
        orchestrator_id: &str,
        description: Option<&str>,
        accept_from: &[String],
        deployment: Option<&str>,
    ) -> Result<(), StoreError> {
        let encoded = serde_json::to_string(accept_from)
            .map_err(|error| StoreError::Invalid(format!("序列化 accept_from 失敗: {error}")))?;
        self.connection
            .execute(
                "INSERT INTO teams(name, orchestrator_id, description, accept_from, deployment) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![name, orchestrator_id, description, encoded, deployment],
            )
            .map_err(|error| name_taken(error, name))?;
        self.append_journal(None, None, "team_created", Some(name))?;
        Ok(())
    }

    /// 更新 team 的 orchestrator、description 與 `accept_from`。
    ///
    /// 保留 `deployment`（那由 deploy／teardown 擁有，§8.3／§8.4）。
    pub fn update_team(
        &self,
        name: &str,
        orchestrator_id: &str,
        description: Option<&str>,
        accept_from: &[String],
    ) -> Result<bool, StoreError> {
        let encoded = serde_json::to_string(accept_from)
            .map_err(|error| StoreError::Invalid(format!("序列化 accept_from 失敗: {error}")))?;
        let changed = self.connection.execute(
            "UPDATE teams SET orchestrator_id = ?2, description = ?3, accept_from = ?4 \
             WHERE name = ?1",
            params![name, orchestrator_id, description, encoded],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        self.append_journal(None, None, "team_updated", Some(name))?;
        Ok(true)
    }

    /// 刪除 team 與其成員關係（§8.1：刪 team 不刪 instance）。
    ///
    /// 回 `false` 表示 team 不存在。
    pub fn delete_team(&self, name: &str) -> Result<bool, StoreError> {
        let changed = self
            .connection
            .execute("DELETE FROM teams WHERE name = ?1", params![name])?;
        // 成員關係隨 team 一起清掉；即使 team 已不存在（`changed == 0`）也要
        // 清乾淨，避免孤兒列讓 instance 永遠「已屬於某個 team」。
        self.connection
            .execute("DELETE FROM team_members WHERE team = ?1", params![name])?;
        if changed == 0 {
            return Ok(false);
        }
        self.append_journal(None, None, "team_deleted", Some(name))?;
        Ok(true)
    }

    /// 取代某 team 的成員集合。
    ///
    /// `team_members.instance_id` 是 UNIQUE（§8.1「一個 instance 最多屬於一個
    /// team」），所以先把不在新集合裡的關係刪掉，再插入新的。
    pub fn set_team_members(&self, team: &str, instance_ids: &[String]) -> Result<(), StoreError> {
        // 順序是**先刪本 team 多餘的列，再插入**，而且刪除只限本 team。
        //
        // §8.1「一個 instance 最多屬於一個 team」在這裡有兩條不同的失敗要區分，
        // 而 `team_members` 的 PRIMARY KEY `(team, instance_id)` 把兩者混成同一個
        // UNIQUE 違規：插入一個**已經是本 team 成員**的 instance 會撞 PK；而插入一個
        // **已在別的 team** 的 instance 撞的是「一個 instance 只屬於一個 team」。
        // `INSERT OR IGNORE` 會把兩者一起吞掉，所以不能單靠它。
        //
        // 修法：插入用 `OR IGNORE`（容忍「已經是成員」這個無害的重複），然後
        // **逐行檢查有沒有真的進來**——沒進來且它不是本 team 的成員，就是它已在
        // 別的 team，該回 `name_conflict`（§8.1），讓交易回滾。
        let encoded = serde_json::to_string(instance_ids)
            .map_err(|error| StoreError::Invalid(format!("序列化成員清單失敗: {error}")))?;
        let mut statement = self.connection.prepare(
            "DELETE FROM team_members WHERE team = ?1 AND NOT EXISTS ( \
               SELECT 1 FROM json_each(?2) WHERE json_each.value = team_members.instance_id \
             )",
        )?;
        statement.execute(params![team, encoded])?;
        drop(statement);
        for id in instance_ids {
            let changed = self.connection.execute(
                "INSERT OR IGNORE INTO team_members(team, instance_id) VALUES (?1, ?2)",
                params![team, id],
            )?;
            if changed == 0 {
                // 沒插進來：兩種可能——它已是本 team 成員（無害，維持原狀），或
                // 它已在別的 team（契約違規）。
                let already_here = self
                    .connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM team_members WHERE team = ?1 AND instance_id = ?2)",
                        params![team, id],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(StoreError::from)?;
                if already_here == 0 {
                    // §3：instance 名稱／歸屬已被使用是 `name_conflict`。這裡是
                    // 「這個 instance 已經屬於某個 team」，語意上同類——它已被
                    // 別的 team 佔用，不能再被另一個 team 收編。
                    return Err(StoreError::NameConflict(format!(
                        "instance {id} 已屬於另一個 team（§8.1 的單一 team 歸屬）"
                    )));
                }
            }
        }
        self.append_journal(None, None, "team_members_set", Some(team))?;
        Ok(())
    }

    /// 依訊息 ID 查一列（不檢查收件人）。
    pub fn message_by_id(&self, id: &str) -> Result<Option<MessageRow>, StoreError> {
        self.connection
            .query_row(MESSAGE_QUERY, params![id], read_message_row)
            .optional()
            .map_err(StoreError::from)
    }

    /// 原子寫入多則訊息（§6.1：對每個收件人寫入一筆 `unread`，全部在同一交易內）。
    ///
    /// 回傳與 `messages` 同順序的 ID。任何一筆失敗都讓整批失敗（呼叫端的
    /// `Tx` 交易會回滾），不會留下半批。
    pub fn insert_message_batch(&self, messages: &[NewMessage]) -> Result<Vec<String>, StoreError> {
        let mut ids = Vec::with_capacity(messages.len());
        for message in messages {
            ids.push(self.insert_message(message)?);
        }
        Ok(ids)
    }

    /// 依建立順序取一批可投遞的訊息並標為 `delivering`（契約 §6.3 drain）。
    ///
    /// 可投遞 = `state = 'unread'`，或 `state = 'delivering'` 但 lease 已過期
    /// （`lease_expires_ms` 不在未來）。批次至少一則（`budget_bytes` 不套用於
    /// 單則），且 `body` 的 **UTF-8 bytes** 總和不超過 `budget_bytes`（48 KiB）。
    ///
    /// 前一批未 ack 的訊息**不會**被本操作結算（lease 未過期就不選它），
    /// 這正是「至少一次投遞」所需的語意。
    pub fn drain_messages(
        &self,
        recipient_id: &str,
        budget_bytes: usize,
        now_ms: i64,
        lease_ms: u64,
    ) -> Result<DrainOutcome, StoreError> {
        let candidates = self.deliverable_messages(recipient_id, now_ms)?;
        let mut batch: Vec<MessageRow> = Vec::new();
        let mut used = 0usize;
        for row in candidates {
            let bytes = row.body.len();
            if !batch.is_empty() && used.saturating_add(bytes) > budget_bytes {
                break;
            }
            // §6.3 的上限是「總和上限」，但它不能把**第一則**也擋掉：批次至少
            // 一則，否則 drain 會對任何訊息都回空，接收端永遠收不到東西。
            // 單則本身超過預算時照樣取（`test drain_returns_at_least_one_...`）。
            used = used.saturating_add(bytes);
            batch.push(row);
        }

        let token = if batch.is_empty() {
            None
        } else {
            Some(Uuid::new_v4().to_string())
        };
        let lease_expires = token
            .as_ref()
            .map(|_| now_ms.saturating_add(lease_ms as i64));

        for row in &batch {
            self.connection.execute(
                "UPDATE messages SET state = 'delivering', delivery_token = ?2, \
                 lease_expires_ms = ?3, delivery_count = delivery_count + 1 WHERE id = ?1",
                params![row.id, token, lease_expires],
            )?;
        }

        let remaining = self.remaining_count(recipient_id, now_ms)?;
        let messages = batch
            .into_iter()
            .map(|mut row| {
                row.state = String::from("delivering");
                row.delivery_token = token.clone();
                row.lease_expires_ms = lease_expires;
                row.delivery_count += 1;
                row
            })
            .collect();

        Ok(DrainOutcome {
            messages,
            delivery_token: token,
            lease_expires_ms: lease_expires,
            remaining,
        })
    }

    /// 確認一則訊息（契約 §6.3 ack）。
    ///
    /// `not_found` 同時涵蓋「訊息不存在」與「不是自己的訊息」——§6.3 對這兩種
    /// 情況指定同一個碼，不因為呼叫方知道 ID 就洩漏別人的訊息存在性。
    pub fn ack_message(
        &self,
        recipient_id: &str,
        message_id: &str,
        token: &str,
        now_ms: i64,
    ) -> Result<AckOutcome, StoreError> {
        let Some(row) = self.message_by_id(message_id)? else {
            return Ok(AckOutcome::NotFound);
        };
        if row.recipient_id != recipient_id {
            return Ok(AckOutcome::NotFound);
        }
        match row.state.as_str() {
            "acked" => {
                // 重複 ack 同一組 token 回 `completed`（`already_acked`）；不同
                // token 對已 ack 的訊息仍是 stale（否則舊 token 會復活）。
                return if row.delivery_token.as_deref() == Some(token) {
                    Ok(AckOutcome::AlreadyAcked)
                } else {
                    Ok(AckOutcome::Stale)
                };
            }
            "archived" => return Ok(AckOutcome::Archived),
            "delivering" => {}
            _ => return Ok(AckOutcome::NeverDelivered),
        }
        let lease_valid = row.lease_expires_ms.is_some_and(|expires| expires > now_ms);
        if row.delivery_token.as_deref() != Some(token) || !lease_valid {
            return Ok(AckOutcome::Stale);
        }
        self.connection.execute(
            "UPDATE messages SET state = 'acked', acked_ms = ?2 WHERE id = ?1",
            params![message_id, now_ms],
        )?;
        Ok(AckOutcome::Acked)
    }

    /// §4.4：`fresh` restart 把該 instance 所有 `delivering` 退回 `unread`
    /// （舊 token 失效）；回傳受影響的筆數。
    ///
    /// `acked` 與 `archived` 不動——已送達的不重送，封存的不可 drain（§4.5）。
    pub fn requeue_delivering_on_fresh(&self, instance_id: &str) -> Result<u64, StoreError> {
        let changed = self.connection.execute(
            "UPDATE messages SET state = 'unread', delivery_token = NULL, lease_expires_ms = NULL \
             WHERE recipient_id = ?1 AND state = 'delivering'",
            params![instance_id],
        )?;
        if changed > 0 {
            self.append_journal(
                Some(instance_id),
                None,
                "messages_requeued",
                Some(&changed.to_string()),
            )?;
        }
        Ok(changed as u64)
    }

    /// 可投遞的訊息（`unread` 或 lease 已過期的 `delivering`），依建立順序。
    fn deliverable_messages(
        &self,
        recipient_id: &str,
        now_ms: i64,
    ) -> Result<Vec<MessageRow>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE recipient_id = ?1 AND ( \
               state = 'unread' OR (state = 'delivering' AND (lease_expires_ms IS NULL OR lease_expires_ms <= ?2)) \
             ) ORDER BY seq"
        ))?;
        let rows = statement
            .query_map(params![recipient_id, now_ms], read_message_row)
            .map_err(StoreError::from)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        Ok(rows)
    }

    /// 仍未 drain 的筆數（§6.3 的 `remaining`：口徑與 [`Tx::deliverable_messages`]
    /// 完全一致——同一個「可投遞」定義，不同批次大小才會讓兩者對不上）。
    fn remaining_count(&self, recipient_id: &str, now_ms: i64) -> Result<u64, StoreError> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM messages WHERE recipient_id = ?1 AND ( \
               state = 'unread' OR (state = 'delivering' AND (lease_expires_ms IS NULL OR lease_expires_ms <= ?2)) \
             )",
            params![recipient_id, now_ms],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    fn existing_live_id(&self, name: &str) -> Result<Option<String>, StoreError> {
        self.connection
            .query_row(
                "SELECT id FROM instances WHERE name = ?1 AND deleted_ms IS NULL",
                params![name],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    fn instance(&self, id: &str) -> Result<Option<InstanceRow>, StoreError> {
        self.connection
            .query_row(&instance_query("id = ?1"), params![id], read_instance_row)
            .optional()
            .map_err(StoreError::from)
    }

    fn append_journal(
        &self,
        instance_id: Option<&str>,
        generation: Option<u64>,
        kind: &str,
        detail: Option<&str>,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO journal(instance_id, generation, kind, detail, at_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![instance_id, generation.map(|value| value as i64), kind, detail, now_ms()],
        )?;
        Ok(())
    }
}

fn read_instance_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<InstanceRow> {
    let encoded_spec: String = row.get(2)?;
    let spec = serde_json::from_str(&encoded_spec).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(InstanceRow {
        id: row.get(0)?,
        name: row.get(1)?,
        spec,
        state: row.get(3)?,
        reason: row.get(4)?,
        generation: row.get::<_, i64>(5)? as u64,
        pid: row.get(6)?,
        pid_start: row.get(7)?,
        token_hash: row.get(8)?,
        deployment: row.get(9)?,
        deleted_ms: row.get(10)?,
        created_ms: row.get(11)?,
        updated_ms: row.get(12)?,
    })
}

/// `messages` 的完整欄位清單，兩處查詢共用。
const MESSAGE_COLUMNS: &str = "id, recipient_id, sender_kind, sender_id, sender_name, kind, body, correlation_id, parent_id, thread_id, state, delivery_token, lease_expires_ms, delivery_count, created_ms";

/// 依主鍵取單列 `messages` 的查詢式（欄位順序對應 [`read_message_row`]）。
const MESSAGE_QUERY: &str = "SELECT id, recipient_id, sender_kind, sender_id, sender_name, kind, body, correlation_id, parent_id, thread_id, state, delivery_token, lease_expires_ms, delivery_count, created_ms FROM messages WHERE id = ?1";

fn read_message_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow {
        id: row.get(0)?,
        recipient_id: row.get(1)?,
        sender_kind: row.get(2)?,
        sender_id: row.get(3)?,
        sender_name: row.get(4)?,
        kind: row.get(5)?,
        body: row.get(6)?,
        correlation_id: row.get(7)?,
        parent_id: row.get(8)?,
        thread_id: row.get(9)?,
        state: row.get(10)?,
        delivery_token: row.get(11)?,
        lease_expires_ms: row.get(12)?,
        delivery_count: row.get(13)?,
        created_ms: row.get(14)?,
    })
}

/// 讀取既有 `meta.schema_version`；沒有 `meta` 表或沒有該鍵時回 `None`。
fn read_schema_version(transaction: &Transaction<'_>) -> Result<Option<i64>, StoreError> {
    let has_meta: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|present| present != 0)?;
    if !has_meta {
        return Ok(None);
    }
    let stored: Option<String> = transaction
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match stored {
        None => Ok(None),
        Some(value) => value.trim().parse::<i64>().map(Some).map_err(|error| {
            StoreError::Invalid(format!("meta.schema_version 不是整數: {value}（{error}）"))
        }),
    }
}

/// `instances_live_name` 唯一索引的最終防線：把約束衝突翻成契約的 `name_conflict`
/// （契約 §3），而不是漏出原始 SQLite 錯誤。
fn name_taken(error: rusqlite::Error, name: &str) -> StoreError {
    if is_constraint_violation(&error) {
        StoreError::NameConflict(name.to_owned())
    } else {
        StoreError::Sqlite(error)
    }
}

/// `operations` 主鍵衝突翻成契約 §3 的重送去重訊號。
fn duplicate_key(error: rusqlite::Error, id: &str) -> StoreError {
    if is_constraint_violation(&error) {
        StoreError::DuplicateOperation(id.to_owned())
    } else {
        StoreError::Sqlite(error)
    }
}

fn is_constraint_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}
