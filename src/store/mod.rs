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
    #[error("SQLite 錯誤: {0}")]
    Sqlite(#[from] rusqlite::Error),
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
