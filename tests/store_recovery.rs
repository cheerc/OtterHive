use otterhive::protocol::{ErrorCode, OperationResult};
use otterhive::store::{NewInstance, NewMessage, Store, StoreError};
use rusqlite::Connection;
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;

fn temporary_database() -> (TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().expect("建立隔離的暫存資料目錄應成功");
    let path = directory.path().join("state.sqlite3");
    (directory, path)
}

fn new_instance(name: &str) -> NewInstance {
    NewInstance {
        name: name.to_owned(),
        spec: json!({"backend": "command", "command": "fixture"}),
    }
}

fn new_message(recipient_id: &str, body: &str) -> NewMessage {
    NewMessage {
        recipient_id: recipient_id.to_owned(),
        sender_kind: "operator".to_owned(),
        sender_id: None,
        sender_name: "operator".to_owned(),
        kind: "message".to_owned(),
        body: body.to_owned(),
        correlation_id: None,
        parent_id: None,
        thread_id: None,
    }
}

fn completed_result(operation_id: &str, value: serde_json::Value) -> OperationResult {
    OperationResult::completed(operation_id, value)
}

fn open_store(path: &Path) -> Store {
    Store::open(path).expect("應能開啟暫存 SQLite store")
}

#[test]
fn rolled_back_transaction_leaves_no_partial_message() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);

    let recipient = store
        .transaction(|tx| tx.insert_instance(&new_instance("rollback-recipient")))
        .expect("應先以成功交易建立訊息收件 instance");
    let result: Result<(), StoreError> = store.transaction(|tx| {
        tx.insert_message(&new_message(&recipient.id, "不應留在資料庫的訊息"))?;
        Err(StoreError::Invalid("測試刻意觸發交易回滾".to_owned()))
    });

    assert!(
        matches!(result, Err(StoreError::Invalid(_))),
        "交易中的刻意拒絕應原樣回傳，並觸發整筆回滾"
    );
    assert_eq!(
        store
            .message_count(&recipient.id)
            .expect("查詢收件 instance 的訊息數應成功"),
        0,
        "回滾後不得留下部分訊息"
    );
}

#[test]
fn retried_operation_returns_the_recorded_result() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let original = completed_result("operation-retry", json!({"message_id": "msg-original"}));
    let attempted_replacement = OperationResult::refused(
        "operation-retry",
        ErrorCode::InvalidArgument,
        "重試不得取代已記錄結果",
    );

    store
        .transaction(|tx| tx.record_operation("operation-retry", "send", "operator", &original))
        .expect("第一次記錄 operation 應成功");
    let retry = store.transaction(|tx| {
        tx.record_operation(
            "operation-retry",
            "send",
            "operator",
            &attempted_replacement,
        )
    });

    assert!(
        matches!(retry, Err(StoreError::DuplicateOperation(id)) if id == "operation-retry"),
        "重複 operation ID 必須回 DuplicateOperation"
    );
    let recorded = store
        .recorded_operation("operation-retry")
        .expect("查詢已記錄 operation 應成功")
        .expect("第一次記錄的 operation 必須仍存在");
    assert_eq!(recorded.op, "send", "重試不得改寫原操作名稱");
    assert_eq!(recorded.caller, "operator", "重試不得改寫原呼叫者");
    assert_eq!(
        serde_json::to_value(recorded.result).expect("原結果應可序列化"),
        serde_json::to_value(original).expect("期望結果應可序列化"),
        "DuplicateOperation 後必須保留第一次記錄的完整結果"
    );
}

#[test]
fn stale_generation_is_rejected_without_writing() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let inserted = store
        .transaction(|tx| tx.insert_instance(&new_instance("generation-cas")))
        .expect("建立 CAS 測試 instance 應成功");

    let current = store
        .transaction(|tx| tx.bump_generation(&inserted.id, Some(0)))
        .expect("以正確 generation 更新應成功");
    assert_eq!(current, 1, "generation 應由零遞增為一");

    let stale = store.transaction(|tx| tx.bump_generation(&inserted.id, Some(0)));
    assert!(
        matches!(stale, Err(StoreError::StaleGeneration { id, current: 1 }) if id == inserted.id),
        "過期的 expected generation 必須回報目前值並拒絕更新"
    );
    let persisted = store
        .instance(&inserted.id)
        .expect("查詢 CAS instance 應成功")
        .expect("CAS instance 必須仍存在");
    assert_eq!(
        persisted.generation, 1,
        "拒絕 stale generation 後不得再寫入 generation"
    );
}

#[test]
fn live_names_are_unique_and_deleted_names_are_reusable() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let first = store
        .transaction(|tx| tx.insert_instance(&new_instance("reusable-name")))
        .expect("首次使用 instance 名稱應成功");

    let conflicting = store.transaction(|tx| tx.insert_instance(&new_instance("reusable-name")));
    assert!(
        matches!(conflicting, Err(StoreError::NameConflict(name)) if name == "reusable-name"),
        "live instance 名稱重複時必須明確回 NameConflict"
    );
    assert_eq!(
        store
            .live_instance_by_name("reusable-name")
            .expect("查詢原 live instance 應成功")
            .map(|row| row.id),
        Some(first.id.clone()),
        "名稱衝突不得取代原有 live instance"
    );

    store
        .transaction(|tx| tx.mark_instance_deleted(&first.id))
        .expect("標記原 instance 刪除應成功");
    let replacement = store
        .transaction(|tx| tx.insert_instance(&new_instance("reusable-name")))
        .expect("刪除後應能重用相同名稱");

    assert_ne!(
        replacement.id, first.id,
        "重用名稱必須建立新的 instance identity"
    );
    let old_row = store
        .instance(&first.id)
        .expect("查詢舊 instance 應成功")
        .expect("已刪除 instance 的歷史記錄仍須保留");
    assert!(
        old_row.deleted_ms.is_some(),
        "刪除原 instance 後應保留其 deleted_ms 標記"
    );
    assert_eq!(
        store
            .live_instance_by_name("reusable-name")
            .expect("查詢重用名稱的 live instance 應成功")
            .map(|row| row.id),
        Some(replacement.id),
        "live 名稱查詢應回傳刪除後建立的新 instance"
    );
}

#[test]
fn state_survives_reopen() {
    let (_directory, path) = temporary_database();
    let (instance_id, message_id) = {
        let mut store = open_store(&path);
        let instance = store
            .transaction(|tx| tx.insert_instance(&new_instance("persistent-instance")))
            .expect("建立持久化測試 instance 應成功");
        let message_id = store
            .transaction(|tx| tx.insert_message(&new_message(&instance.id, "持久化訊息")))
            .expect("建立持久化訊息應成功");
        let result = completed_result("operation-persist", json!({"instance_id": instance.id}));
        store
            .transaction(|tx| {
                tx.record_operation("operation-persist", "create_instance", "operator", &result)
            })
            .expect("記錄持久化 operation 應成功");
        (instance.id, message_id)
    };

    let store = open_store(&path);
    let instance = store
        .instance(&instance_id)
        .expect("重開後查詢 instance 應成功")
        .expect("instance 必須跨重開保留");
    assert_eq!(
        instance.name, "persistent-instance",
        "重開後 instance 欄位應保持原值"
    );
    assert_eq!(
        store
            .message_count(&instance_id)
            .expect("重開後查詢訊息數應成功"),
        1,
        "訊息必須跨重開保留"
    );
    let operation = store
        .recorded_operation("operation-persist")
        .expect("重開後查詢 operation 應成功")
        .expect("operation 必須跨重開保留");
    assert_eq!(
        serde_json::to_value(operation.result).expect("持久化結果應可序列化"),
        json!({
            "operation_id": "operation-persist",
            "outcome": "completed",
            "subject": null,
            "value": {"instance_id": instance_id},
            "effects": [],
            "error": null
        }),
        "重開後應保留原 operation result"
    );
    assert!(!message_id.is_empty(), "訊息 ID 應有效且非空");
}

#[test]
fn newer_schema_is_refused_and_left_untouched() {
    let (_directory, path) = temporary_database();
    {
        let connection = Connection::open(&path).expect("建立 schema fixture 資料庫應成功");
        connection
            .execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta (key, value) VALUES ('schema_version', '99');
                 CREATE TABLE preserved_data (value TEXT NOT NULL);
                 INSERT INTO preserved_data (value) VALUES ('keep-this-row');",
            )
            .expect("建立較新 schema 與舊資料 fixture 應成功");
    }

    let result = Store::open(&path);
    assert!(
        matches!(
            result,
            Err(StoreError::UnsupportedSchema {
                found: 99,
                supported: 1
            })
        ),
        "遇到 schema 99 必須回 UnsupportedSchema，並指出支援版本"
    );

    let connection = Connection::open(&path).expect("拒絕 schema 後資料庫仍應可讀");
    let tables = {
        let mut statement = connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .expect("檢視資料表清單應成功");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("讀取資料表清單應成功")
            .collect::<Result<Vec<_>, _>>()
            .expect("收集資料表名稱應成功")
    };
    assert_eq!(
        tables,
        vec!["meta".to_owned(), "preserved_data".to_owned()],
        "拒絕較新 schema 不得新增任何資料表"
    );
    let version: String = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("讀取原 schema 版本應成功");
    assert_eq!(version, "99", "拒絕較新 schema 不得改寫既有版本資料");
    let preserved: String = connection
        .query_row("SELECT value FROM preserved_data", [], |row| row.get(0))
        .expect("讀取既有 sentinel 資料應成功");
    assert_eq!(preserved, "keep-this-row", "拒絕較新 schema 不得改動舊資料");
}
