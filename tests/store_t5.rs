//! T5 coordinator：store 層的 team 與訊息語意（契約 §6.3、§4.4、§8.1）。
//!
//! 這裡只驗 store 層的**原子性與計數語意**；端對端的授權、喚醒與結果形狀由
//! `tests/team_ops.rs` 與 `tests/message_delivery.rs` 驗。

use otterhive::store::{AckOutcome, NewInstance, NewMessage, Store, StoreError};
use rusqlite::params;
use serde_json::json;
use tempfile::TempDir;

/// §6.3：一次 drain 的 `body` bytes 預算。
const BUDGET: usize = 48 * 1024;

/// §6.3 的 lease 預設（測試用毫秒）。
const LEASE_MS: u64 = 600_000;

fn temporary_database() -> (TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().expect("繁中說明：建立隔離的暫存資料目錄應成功");
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

fn open_store(path: &std::path::Path) -> Store {
    Store::open(path).expect("繁中說明：應能開啟暫存 SQLite store")
}

/// 建立 instance 並回傳其 ID。
fn instance_id(store: &mut Store, name: &str) -> String {
    store
        .transaction(|tx| tx.insert_instance(&new_instance(name)))
        .expect("繁中說明：應能建立暫存 instance")
        .id
}

/// 寫入一則訊息並回傳其 ID。
fn send(store: &mut Store, recipient: &str, body: &str) -> String {
    store
        .transaction(|tx| tx.insert_message(&new_message(recipient, body)))
        .expect("繁中說明：應能寫入訊息")
}

/// §6.3：drain 依建立順序取訊息、`body` 的 **UTF-8 bytes** 總和不超過預算
/// （至少一則），`remaining` 是尚未 drain 的筆數。
///
/// 每則 body = 1 字元 index + 30×1024 個 `x` = 30721 bytes；兩則 61442 >
/// 48 KiB（49152），所以三則必須分三批。
#[test]
fn drain_respects_the_byte_budget_and_reports_remaining() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let alpha = instance_id(&mut store, "alpha");
    for index in 0..3 {
        let body = format!("{index}{}", "x".repeat(30 * 1024));
        send(&mut store, &alpha, &body);
    }
    assert_eq!(
        30 * 1024 + 1,
        30721,
        "繁中說明：每則 body 應是 30721 bytes（兩則 61442 > 49152）"
    );

    let first = store
        .transaction(|tx| tx.drain_messages(&alpha, BUDGET, 1_000, LEASE_MS))
        .expect("繁中說明：第一次 drain 應成功");
    assert_eq!(first.messages.len(), 1, "兩則放不進 48 KiB，應只取一則");
    assert_eq!(
        first.remaining, 2,
        "繁中說明：remaining 應是尚未 drain 的兩則"
    );
    assert_eq!(first.messages[0].delivery_count, 1);
    assert_eq!(
        first.lease_expires_ms,
        Some(1_000 + LEASE_MS as i64),
        "繁中說明：lease_expires_ms 應是 now + lease"
    );

    let second = store
        .transaction(|tx| tx.drain_messages(&alpha, BUDGET, 1_000, LEASE_MS))
        .expect("繁中說明：第二次 drain 應成功");
    assert_eq!(second.messages.len(), 1, "剩下兩則同樣放不進 48 KiB");
    assert_eq!(second.remaining, 1);

    let third = store
        .transaction(|tx| tx.drain_messages(&alpha, BUDGET, 1_000, LEASE_MS))
        .expect("繁中說明：第三次 drain 應成功");
    assert_eq!(third.messages.len(), 1);
    assert_eq!(third.remaining, 0, "繁中說明：全部取完後 remaining 應為 0");

    let fourth = store
        .transaction(|tx| tx.drain_messages(&alpha, BUDGET, 1_000, LEASE_MS))
        .expect("繁中說明：第四次 drain 應成功");
    assert!(
        fourth.messages.is_empty(),
        "繁中說明：lease 未過期時沒有可投遞的訊息"
    );
    assert!(
        fourth.delivery_token.is_none(),
        "繁中說明：§6.3 沒有訊息時不發 token"
    );
    assert_eq!(fourth.remaining, 0);
}

/// §6.3：單一訊息即使超過預算也要取到（上限不套用於單則）。
#[test]
fn drain_returns_at_least_one_message_even_when_it_exceeds_the_budget() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let alpha = instance_id(&mut store, "alpha");
    send(&mut store, &alpha, &"y".repeat(60 * 1024));

    let drained = store
        .transaction(|tx| tx.drain_messages(&alpha, BUDGET, 1_000, LEASE_MS))
        .expect("繁中說明：drain 應成功");
    assert_eq!(drained.messages.len(), 1, "繁中說明：至少要取到一則");
    assert_eq!(drained.remaining, 0);
}

/// §6.3：lease 未過期時前一批**不會**被結算；lease 過期後以新 token 重送，
/// `delivery_count` 為 2，舊 token 失效。
#[test]
fn an_expired_lease_redelivers_and_an_unexpired_one_does_not() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let alpha = instance_id(&mut store, "alpha");
    let id = send(&mut store, &alpha, "會過期");

    let first = store
        .transaction(|tx| tx.drain_messages(&alpha, BUDGET, 1_000, LEASE_MS))
        .expect("繁中說明：首次 drain 應成功");
    let first_token = first.delivery_token.expect("繁中說明：非空批次應發 token");

    let during = store
        .transaction(|tx| tx.drain_messages(&alpha, BUDGET, 1_500, LEASE_MS))
        .expect("繁中說明：lease 內 drain 應成功");
    assert!(
        during.messages.is_empty(),
        "繁中說明：lease 未過期時不應重送，實際 {:?}",
        during.messages
    );

    let after = store
        .transaction(|tx| tx.drain_messages(&alpha, BUDGET, 602_000, LEASE_MS))
        .expect("繁中說明：lease 過期後 drain 應成功");
    assert_eq!(after.messages.len(), 1);
    assert_eq!(after.messages[0].id, id);
    assert_eq!(
        after.messages[0].delivery_count, 2,
        "繁中說明：重送的 delivery_count 應為 2"
    );
    let second_token = after.delivery_token.expect("繁中說明：重送應發新的 token");
    assert_ne!(second_token, first_token);

    let stale = store
        .transaction(|tx| tx.ack_message(&alpha, &id, &first_token, 603_000))
        .expect("繁中說明：舊 token 的 ack 應執行");
    assert_eq!(stale, AckOutcome::Stale, "繁中說明：舊 token 應是 stale");

    let fresh = store
        .transaction(|tx| tx.ack_message(&alpha, &id, &second_token, 603_000))
        .expect("繁中說明：新 token 的 ack 應執行");
    assert_eq!(fresh, AckOutcome::Acked);
}

/// §6.3：ack 的五種結果——重複 ack、token 不符、從未投遞、不是自己的訊息、
/// 訊息不存在。`not_found` 同時涵蓋「不存在」與「不是自己的」，不因知道 ID
/// 就洩漏別人訊息的存在性。
#[test]
fn ack_distinguishes_token_delivery_and_ownership() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let alpha = instance_id(&mut store, "alpha");
    let beta = instance_id(&mut store, "beta");
    let id = send(&mut store, &alpha, "投遞");
    let never = send(&mut store, &alpha, "未投遞");

    let never_delivered = store
        .transaction(|tx| tx.ack_message(&alpha, &never, "任何 token", 1_000))
        .expect("繁中說明：ack 應執行");
    assert_eq!(
        never_delivered,
        AckOutcome::NeverDelivered,
        "繁中說明：從未 drain 過的訊息是 never_delivered，不是 not_found"
    );

    let missing = store
        .transaction(|tx| tx.ack_message(&alpha, "不存在的 id", "token", 1_000))
        .expect("繁中說明：ack 應執行");
    assert_eq!(missing, AckOutcome::NotFound);

    let drained = store
        .transaction(|tx| tx.drain_messages(&alpha, BUDGET, 1_000, LEASE_MS))
        .expect("繁中說明：drain 應成功");
    let token = drained
        .delivery_token
        .expect("繁中說明：非空批次應發 token");

    let foreign = store
        .transaction(|tx| tx.ack_message(&beta, &id, &token, 1_100))
        .expect("繁中說明：ack 應執行");
    assert_eq!(
        foreign,
        AckOutcome::NotFound,
        "繁中說明：ack 別人的訊息是 not_found"
    );

    let wrong = store
        .transaction(|tx| tx.ack_message(&alpha, &id, "wrong-token", 1_100))
        .expect("繁中說明：ack 應執行");
    assert_eq!(wrong, AckOutcome::Stale);
    let right = store
        .transaction(|tx| tx.ack_message(&alpha, &id, &token, 1_100))
        .expect("繁中說明：ack 應執行");
    assert_eq!(
        right,
        AckOutcome::Acked,
        "繁中說明：錯的 ack 不應使正確 token 失效"
    );

    let repeat = store
        .transaction(|tx| tx.ack_message(&alpha, &id, &token, 1_200))
        .expect("繁中說明：重複 ack 應執行");
    assert_eq!(
        repeat,
        AckOutcome::AlreadyAcked,
        "繁中說明：同一組 token 重複 ack 回 already_acked"
    );
    let other = store
        .transaction(|tx| tx.ack_message(&alpha, &id, "other-token", 1_200))
        .expect("繁中說明：ack 應執行");
    assert_eq!(
        other,
        AckOutcome::Stale,
        "繁中說明：不同 token 對已 ack 的訊息仍是 stale（否則舊 token 會復活）"
    );
}

/// §6.3：lease 過期後 ack → `Stale`，即使 token 相符。
#[test]
fn ack_after_the_lease_expires_is_stale() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let alpha = instance_id(&mut store, "alpha");
    let id = send(&mut store, &alpha, "租約");
    let drained = store
        .transaction(|tx| tx.drain_messages(&alpha, BUDGET, 1_000, 1_000))
        .expect("繁中說明：drain 應成功");
    let token = drained.delivery_token.expect("繁中說明：應發 token");
    assert_eq!(drained.lease_expires_ms, Some(2_000));

    let expired = store
        .transaction(|tx| tx.ack_message(&alpha, &id, &token, 2_001))
        .expect("繁中說明：ack 應執行");
    assert_eq!(
        expired,
        AckOutcome::Stale,
        "繁中說明：lease 過期後 token 相符也是 stale"
    );
}

/// §4.4：`delete_instance` 封存的訊息不可 ack。
#[test]
fn an_archived_message_can_not_be_acked() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let alpha = instance_id(&mut store, "alpha");
    let id = send(&mut store, &alpha, "封存");

    store
        .transaction(|tx| tx.archive_messages(&alpha))
        .expect("繁中說明：封存應成功");
    let outcome = store
        .transaction(|tx| tx.ack_message(&alpha, &id, "任何 token", 1_000))
        .expect("繁中說明：ack 應執行");
    assert_eq!(
        outcome,
        AckOutcome::Archived,
        "繁中說明：§4.5 archived 的訊息不再可 drain，也不可 ack"
    );
    let drained = store
        .transaction(|tx| tx.drain_messages(&alpha, BUDGET, 1_000, LEASE_MS))
        .expect("繁中說明：drain 應執行");
    assert!(
        drained.messages.is_empty(),
        "繁中說明：archived 的訊息不再可 drain"
    );
}

/// §4.4：`fresh` restart 把 `delivering` 退回 `unread` 且舊 token 失效；
/// `acked` 與未投遞的不動。
#[test]
fn fresh_restart_requeues_delivering_but_leaves_acked_alone() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let alpha = instance_id(&mut store, "alpha");
    let acked_id = send(&mut store, &alpha, "已確認");
    let in_flight = send(&mut store, &alpha, "在途");
    let untouched = send(&mut store, &alpha, "未投遞");

    // 第一則單獨 drain 後 ack。預算 1 byte：§6.3 的上限不套用於單則，所以批次裡
    // 只有一則——這同時把「至少一則」變成本測試的前置。
    let drained = store
        .transaction(|tx| tx.drain_messages(&alpha, 1, 1_000, LEASE_MS))
        .expect("繁中說明：drain 應成功");
    let token = drained
        .delivery_token
        .expect("繁中說明：非空批次應發 token");
    assert_eq!(
        drained.messages.len(),
        1,
        "繁中說明：預算 1 byte 仍應取到一則（至少一則）"
    );
    assert_eq!(
        drained.messages[0].id, acked_id,
        "繁中說明：依建立順序取第一則"
    );
    store
        .transaction(|tx| tx.ack_message(&alpha, &acked_id, &token, 1_100))
        .expect("繁中說明：ack 應執行");

    // 只 drain 第二則，讓它停在 delivering。用極小的預算讓批次裡只有一則
    //（§6.3 的上限不套用於單則），這樣第三則仍是 unread。
    let second = store
        .transaction(|tx| tx.drain_messages(&alpha, 1, 2_000, LEASE_MS))
        .expect("繁中說明：drain 應成功");
    assert_eq!(
        second.messages.len(),
        1,
        "繁中說明：預算 1 byte 仍應取到一則（至少一則）"
    );
    assert_eq!(second.messages[0].id, in_flight);
    assert_eq!(second.remaining, 1, "繁中說明：第三則仍未 drain");

    let requeued = store
        .transaction(|tx| tx.requeue_delivering_on_fresh(&alpha))
        .expect("繁中說明：fresh restart 的重新排隊應成功");
    assert_eq!(requeued, 1, "繁中說明：只有一則 delivering 需要退回");

    let (in_flight_row, acked_row, untouched_row) = store
        .transaction(|tx| {
            Ok((
                tx.message_by_id(&in_flight)?,
                tx.message_by_id(&acked_id)?,
                tx.message_by_id(&untouched)?,
            ))
        })
        .expect("繁中說明：讀取訊息應成功");
    let in_flight_row = in_flight_row.expect("繁中說明：在途的訊息應存在");
    let acked_row = acked_row.expect("繁中說明：已確認的訊息應存在");
    let untouched_row = untouched_row.expect("繁中說明：未投遞的訊息應存在");

    assert_eq!(in_flight_row.state, "unread", "delivering 應退回 unread");
    assert!(
        in_flight_row.delivery_token.is_none(),
        "繁中說明：退回時舊 token 應失效"
    );
    assert!(in_flight_row.lease_expires_ms.is_none());
    assert_eq!(
        in_flight_row.delivery_count, 1,
        "繁中說明：退回不重置 delivery_count（至少一次投遞要看得見重送）"
    );

    // 退回期間用**舊 token** ack → `Stale`，不是 `NeverDelivered`。§6.3 的
    // `never_delivered` 是「從未投遞」，而這則被投遞過一次；回它會讓 agent 誤
    // 以為這則從沒送到過（§4.4 明說舊 token 失效）。
    let stale = store
        .transaction(|tx| tx.ack_message(&alpha, &in_flight, "過期的 token", 2_500))
        .expect("繁中說明：ack 應執行");
    assert_eq!(
        stale,
        AckOutcome::Stale,
        "繁中說明：退回期間的舊 token 是 stale，不是 never_delivered"
    );

    // 對照組：真的**從未**投遞的訊息仍是 `NeverDelivered`（`delivery_count == 0`）。
    let never = store
        .transaction(|tx| tx.ack_message(&alpha, &untouched, "任何 token", 2_500))
        .expect("繁中說明：ack 應執行");
    assert_eq!(
        never,
        AckOutcome::NeverDelivered,
        "繁中說明：delivery_count 為 0 的訊息從未投遞，應是 never_delivered"
    );
    assert_eq!(acked_row.state, "acked", "已確認的不應被退回");
    assert_eq!(untouched_row.state, "unread", "未投遞的應仍是 unread");

    // 退回後可再 drain，且 delivery_count 遞增。
    let again = store
        .transaction(|tx| tx.drain_messages(&alpha, BUDGET, 3_000, LEASE_MS))
        .expect("繁中說明：退回後應可再 drain");
    let redelivered = again
        .messages
        .iter()
        .find(|message| message.id == in_flight)
        .expect("繁中說明：退回的訊息應可再投遞");
    assert_eq!(redelivered.delivery_count, 2);
}

/// §4.5：刪除 instance 要**移出所屬 team**，且不留孤兒列。
///
/// 這條斷言**必須直接查底層表**。`team_members`、`team_name_of_instance`、
/// `team_of_instance_name` 三個讀取端都 join `instances.deleted_ms IS NULL`，
/// 所以孤兒列在所有對外查詢裡都是隱形的——任何 `team list`／`team_members()`
/// 層的斷言在 buggy 版本上**都會綠**，那等於沒測。
///
/// 孤兒列的實際危害：§8.1 的「一個 instance 最多屬於一個 team」靠
/// `team_members.instance_id` 的 UNIQUE 實作，而 `set_team_members` 的 OR IGNORE
/// 逐行檢查查的是「**本 team 裡**有沒有」——孤兒列會讓它回 `already_here == 1`
/// 而靜默接受，該 ID 的單一歸屬約束就 **fail-open**。
#[test]
fn deleting_an_instance_removes_it_from_its_team() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let alpha = instance_id(&mut store, "alpha");
    let beta = instance_id(&mut store, "beta");

    store
        .transaction(|tx| {
            tx.insert_team("squad", &alpha, None, &[], None)?;
            tx.set_team_members("squad", &[alpha.clone(), beta.clone()])
        })
        .expect("繁中說明：建立 team 應成功");
    assert_eq!(
        store
            .transaction(|tx| tx.team_members("squad"))
            .expect("繁中說明：讀取成員應成功"),
        vec!["alpha".to_owned(), "beta".to_owned()],
        "繁中說明：刪除前兩人都應是 squad 的成員（測試前提）"
    );

    store
        .transaction(|tx| {
            tx.archive_messages(&beta)?;
            tx.remove_from_team(&beta)?;
            tx.mark_instance_deleted(&beta)
        })
        .expect("繁中說明：刪除 beta 應成功");

    // 關鍵斷言：底層列必須不存在。孤兒列在對外查詢裡隱形，所以只能這樣查。
    let connection = rusqlite::Connection::open(&path).expect("繁中說明：應能開啟資料庫");
    let orphans: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM team_members WHERE instance_id = ?1",
            params![beta],
            |row| row.get(0),
        )
        .expect("繁中說明：應能直接查 team_members");
    assert_eq!(
        orphans, 0,
        "繁中說明：刪除的 instance 不得留下孤兒的 team_members 列——\
         孤兒列會讓 §8.1 的單一歸屬約束對該 ID fail-open"
    );

    // 對照組：未刪除的 alpha 仍在 team 裡（不該被連帶移除）。
    assert_eq!(
        store
            .transaction(|tx| tx.team_members("squad"))
            .expect("繁中說明：讀取成員應成功"),
        vec!["alpha".to_owned()],
        "繁中說明：刪除一個成員不該影響其他成員"
    );
}

/// §8.1：一個 instance 最多屬於一個 team；被拒的加入整批回滾。
#[test]
fn an_instance_belongs_to_at_most_one_team() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let alpha = instance_id(&mut store, "alpha");
    let beta = instance_id(&mut store, "beta");

    store
        .transaction(|tx| {
            tx.insert_team("left", &alpha, None, &[], None)?;
            tx.set_team_members("left", std::slice::from_ref(&alpha))?;
            tx.insert_team("right", &beta, None, &[], None)?;
            // beta 必須**真的**加入 right，否則它不屬於任何 team，下面那個
            // 斷言就沒有測到 §8.1 的約束（`insert_team` 只建 team 紀錄，不動
            // 成員關係）。
            tx.set_team_members("right", std::slice::from_ref(&beta))
        })
        .expect("繁中說明：建立兩個 team 應成功");

    let failure = store.transaction(|tx| {
        // beta 已在 right；把它加進 left 應被 team_members.instance_id 的
        // UNIQUE 擋下。
        tx.set_team_members("left", &[alpha.clone(), beta.clone()])
    });
    assert!(
        failure.is_err(),
        "繁中說明：同一 instance 加進第二個 team 應失敗"
    );

    let members = store
        .transaction(|tx| tx.team_members("left"))
        .expect("繁中說明：讀取成員應成功");
    assert_eq!(
        members,
        vec!["alpha".to_owned()],
        "繁中說明：被拒的 set_team_members 應整批回滾，不留下 beta"
    );
}

/// §8.1：`delete_team` 清掉成員關係但不刪 instance；同名可重建。
#[test]
fn deleting_a_team_frees_its_members_for_a_new_team() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let alpha = instance_id(&mut store, "alpha");
    store
        .transaction(|tx| {
            tx.insert_team("squad", &alpha, Some("說明"), &["peer".to_owned()], None)?;
            tx.set_team_members("squad", std::slice::from_ref(&alpha))
        })
        .expect("繁中說明：建立 team 應成功");

    let team = store
        .transaction(|tx| tx.team("squad"))
        .expect("繁中說明：讀取 team 應成功")
        .expect("繁中說明：squad 應存在");
    assert_eq!(team.description.as_deref(), Some("說明"));
    assert_eq!(team.accept_from_names(), vec!["peer".to_owned()]);
    assert!(team.deployment.is_none());
    assert_eq!(
        store
            .transaction(|tx| tx.team_members("squad"))
            .expect("繁中說明：讀取成員應成功"),
        vec!["alpha".to_owned()]
    );

    assert!(
        store
            .transaction(|tx| tx.delete_team("squad"))
            .expect("繁中說明：刪除 team 應成功"),
        "既有 team 應刪得到"
    );
    assert!(
        store
            .transaction(|tx| Ok(tx.team("squad")?.is_none()))
            .expect("繁中說明：讀取 team 應成功"),
        "刪除後不應再找得到 team"
    );
    assert!(
        store
            .transaction(|tx| Ok(tx.live_instance_id("alpha")?.is_some()))
            .expect("繁中說明：讀取 instance 應成功"),
        "刪除 team 不得刪除 instance"
    );
    assert!(
        store
            .transaction(|tx| Ok(tx.team_name_of_instance("alpha")?.is_none()))
            .expect("繁中說明：讀取 team 應成功"),
        "成員關係應一併清掉"
    );
    assert!(
        !store
            .transaction(|tx| tx.delete_team("squad"))
            .expect("繁中說明：刪除 team 應成功"),
        "刪除不存在的 team 應回 false（呼叫端翻成 not_found）"
    );

    // 同名可重建（§4.5 刪除 instance 後名稱可重用，team 同理）。
    store
        .transaction(|tx| tx.insert_team("squad", &alpha, None, &[], None))
        .expect("繁中說明：同名重建應成功");
}

/// §8.1：team 名稱已被使用時回 `NameConflict`（§3），不取代原有 team。
#[test]
fn a_duplicate_team_name_is_a_name_conflict() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let alpha = instance_id(&mut store, "alpha");
    let beta = instance_id(&mut store, "beta");
    store
        .transaction(|tx| tx.insert_team("squad", &alpha, None, &[], None))
        .expect("繁中說明：建立 team 應成功");

    let duplicate = store.transaction(|tx| tx.insert_team("squad", &beta, None, &[], None));
    let is_name_conflict = matches!(
        duplicate,
        Err(StoreError::NameConflict(ref name)) if name == "squad"
    );
    assert!(
        is_name_conflict,
        "繁中說明：重複的 team 名稱應是 NameConflict，實際 {duplicate:?}"
    );

    let team = store
        .transaction(|tx| tx.team("squad"))
        .expect("繁中說明：讀取 team 應成功")
        .expect("繁中說明：squad 應仍存在");
    assert_eq!(
        team.orchestrator_id, alpha,
        "繁中說明：被拒的建立不得取代原有的 orchestrator"
    );
}

/// §8.1：`update_team` 取代 description 與 `accept_from`，保留 `deployment`。
#[test]
fn updating_a_team_replaces_description_and_accept_from() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let alpha = instance_id(&mut store, "alpha");
    store
        .transaction(|tx| {
            tx.insert_team(
                "squad",
                &alpha,
                Some("第一版"),
                &["peer".to_owned()],
                Some("deployment-x"),
            )
        })
        .expect("繁中說明：建立 team 應成功");

    assert!(
        store
            .transaction(|tx| tx.update_team("squad", &alpha, Some("第二版"), &[]))
            .expect("繁中說明：update 應成功"),
        "既有 team 應更新得到"
    );
    let team = store
        .transaction(|tx| tx.team("squad"))
        .expect("繁中說明：讀取 team 應成功")
        .expect("繁中說明：squad 應存在");
    assert_eq!(team.description.as_deref(), Some("第二版"));
    assert!(
        team.accept_from_names().is_empty(),
        "繁中說明：accept_from 應被取代為空"
    );
    assert_eq!(
        team.deployment.as_deref(),
        Some("deployment-x"),
        "繁中說明：deployment 由 deploy／teardown 擁有，update 不得清掉"
    );
    assert!(
        !store
            .transaction(|tx| tx.update_team("ghost", &alpha, None, &[]))
            .expect("繁中說明：update 應成功"),
        "更新不存在的 team 應回 false"
    );
}

/// §8.1：`accept_from` 是壞掉的 JSON 時讀成空清單——`accept_from` 只會**放寬**
/// 收件權限，讀成空是 fail-closed 的方向。
#[test]
fn a_malformed_accept_from_reads_as_empty() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let alpha = instance_id(&mut store, "alpha");
    store
        .transaction(|tx| tx.insert_team("squad", &alpha, None, &[], None))
        .expect("繁中說明：建立 team 應成功");
    {
        let connection = rusqlite::Connection::open(&path).expect("繁中說明：應能開啟資料庫");
        let changed = connection
            .execute(
                "UPDATE teams SET accept_from = '不是 JSON' WHERE name = 'squad'",
                params![],
            )
            .expect("繁中說明：應能寫入壞掉的 accept_from");
        assert_eq!(changed, 1, "繁中說明：squad 應存在（測試前提）");
    }
    let team = store
        .transaction(|tx| tx.team("squad"))
        .expect("繁中說明：讀取 team 應成功")
        .expect("繁中說明：squad 應存在");
    assert!(
        team.accept_from_names().is_empty(),
        "繁中說明：壞掉的 accept_from 應讀成空清單而非讓 team 讀取失敗"
    );
}

/// §6.1：`insert_message_batch` 原子寫入多則訊息，回傳與輸入同順序的 ID。
#[test]
fn a_message_batch_is_written_atomically_in_order() {
    let (_directory, path) = temporary_database();
    let mut store = open_store(&path);
    let alpha = instance_id(&mut store, "alpha");
    let beta = instance_id(&mut store, "beta");
    let batch = vec![
        new_message(&alpha, "第一"),
        new_message(&beta, "第二"),
        new_message(&alpha, "第三"),
    ];
    let ids = store
        .transaction(|tx| tx.insert_message_batch(&batch))
        .expect("繁中說明：批次寫入應成功");
    assert_eq!(ids.len(), 3);
    assert_ne!(ids[0], ids[1], "繁中說明：每則訊息應有各自的 ID");

    let rows = store
        .transaction(|tx| {
            Ok((
                tx.message_by_id(&ids[0])?,
                tx.message_by_id(&ids[1])?,
                tx.message_by_id(&ids[2])?,
            ))
        })
        .expect("繁中說明：讀取訊息應成功");
    let first = rows.0.expect("繁中說明：第一則應存在");
    let second = rows.1.expect("繁中說明：第二則應存在");
    let third = rows.2.expect("繁中說明：第三則應存在");
    assert_eq!(first.body, "第一");
    assert_eq!(second.body, "第二");
    assert_eq!(third.body, "第三");
    assert_eq!(first.recipient_id, alpha);
    assert_eq!(second.recipient_id, beta);
    assert_eq!(first.state, "unread", "繁中說明：新訊息應是 unread");
    assert_eq!(first.delivery_count, 0);
    assert!(first.delivery_token.is_none());
}
