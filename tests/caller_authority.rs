//! T4 Lane B RED：caller 授權（契約 §2；lifecycle 部分，`tests/caller_authority.rs`）。
//!
//! 授權的三條不變量：
//! 1. **憑證決定身分**——bridge 從環境變數的 token 推導 caller，`tools/call` 的
//!    參數不得自報 sender、role 或 team（契約 §2）。
//! 2. **未實作的 lifecycle 操作一律 `unsupported_in_phase`**——T4 的
//!    `tools_list` 只列已完成的 lifecycle 工具；create／delete／start／
//!    deployment 不列出，直接呼叫也必須被明確拒絕（契約 §3／§10）。
//! 3. **`role` 是描述文字，不授予任何權限**——`role` 是 `create_instance` 的
//!    合法欄位，但不得出現在任何授權輸入的位置。
//!
//! daemon 在線時的授權結果屬 Lane A 的 `tests/instance_lifecycle.rs`；本檔
//! 只做 (a) 授權輸入的**形狀**層級（`admission` 層的確定性行為）與 (b) bridge
//! 端「不可偽裝」的形狀。全檔只在 macOS 編譯，所有等待有上限，bridge 行程
//! 在 Drop 收斂。

#![cfg(target_os = "macos")]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use common::TempHome;
use otterhive::admission::admit;
use otterhive::protocol::ErrorCode;
use serde_json::{json, Value};

/// bridge 讀不到 daemon 時的 JSON-RPC 錯誤碼（契約 §10）。
const ERR_DAEMON_UNREACHABLE: i64 = -32001;

/// 一般回應的等待上限。
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// 測試內的 token 字串；不是任何真實憑證，只用來讓 bridge 有東西可讀。
const INSTANCE_TOKEN: &str = "token-instance-authority";

/// bridge 回覆的兩層（契約 §10）：
///
/// - [`Reply::Rpc`]：JSON-RPC 層錯誤。daemon 不可連時的工具呼叫屬於這一層。
/// - [`Reply::Tool`]：MCP 工具結果（`{content, isError}`），內含
///   `OperationResult`。授權被拒（`refused`）屬於這一層。
///
/// 把兩層混成同一種回覆，就是 §10 要防的「假裝成功」。
#[derive(Debug)]
enum Reply {
    Rpc { code: i64, message: String },
    Tool { is_error: bool, result: Value },
}

impl Reply {
    /// 本檔所有 bridge 斷言的前置：daemon 不在線，工具呼叫一律是
    /// JSON-RPC 的 -32001。任何看起來像「授權被行使過」的結果都是偽裝。
    #[track_caller]
    fn expect_daemon_unreachable(&self, what: &str) {
        match self {
            Reply::Rpc { code, message } => assert_eq!(
                *code,
                ERR_DAEMON_UNREACHABLE,
                "繁中說明：{what} 在 daemon 不可連時應回 {ERR_DAEMON_UNREACHABLE}，訊息為 {message:?}"
            ),
            Reply::Tool { result, .. } => panic!(
                "繁中說明：{what} 在 daemon 不可連時不得偽裝成工具結果，實際為 {result}"
            ),
        }
    }

    /// 工具結果的 `isError`；JSON-RPC 錯誤沒有這個欄位，回 `None`。
    ///
    /// daemon 在線時 `refused`／`unsupported` 才是 `true`（契約 §10）。本檔
    /// 在 daemon 不在線的狀態下不會走到 `Tool`，但形狀仍固定下來。
    fn is_error(&self) -> Option<bool> {
        match self {
            Reply::Rpc { .. } => None,
            Reply::Tool { is_error, .. } => Some(*is_error),
        }
    }
}

struct Bridge {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<String>,
    next_id: i64,
}

impl Bridge {
    fn start(home: &Path, token: &str) -> Bridge {
        let mut command = Command::new(env!("CARGO_BIN_EXE_otterhive-mcp-bridge"));
        // 不繼承測試 runner 的環境：bridge 的 HOME／TOKEN 前提必須由測試自己
        // 決定，否則結果取決於跑測試的機器。
        command.env_clear();
        command
            .env("HOME", home)
            .env("OTTERHIVE_HOME", home)
            .env("OTTERHIVE_INSTANCE_TOKEN", token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("繁中說明：bridge binary 應可啟動");
        let stdin = child.stdin.take().expect("bridge stdin 應可取得");
        let stdout = child.stdout.take().expect("bridge stdout 應可取得");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffered = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match buffered.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Bridge {
            child,
            stdin,
            rx,
            next_id: 1,
        }
    }

    fn send(&mut self, message: &Value) {
        self.stdin
            .write_all(format!("{message}\n").as_bytes())
            .and_then(|()| self.stdin.flush())
            .expect("應可向 bridge 寫入一行 JSON-RPC");
    }

    fn read(&mut self) -> Value {
        let line = self
            .rx
            .recv_timeout(RESPONSE_TIMEOUT)
            .expect("繁中說明：bridge 應在期限內回應");
        let text = line.trim();
        serde_json::from_str(text)
            .unwrap_or_else(|error| panic!("bridge 回應應為 JSON（{text:?}）：{error}"))
    }

    fn handshake(&mut self) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "otterhive-red-test", "version": "0"}
            }
        }));
        self.next_id += 1;
        let response = self.read();
        assert!(
            response.get("error").is_none(),
            "繁中說明：前提齊全時 initialize 應成功，實際為 {response}"
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    /// 以 bridge 由環境推導出的 caller 身分呼叫一個工具。
    ///
    /// 注意 `arguments` 裡**沒有**身分欄位：caller 只能來自憑證（契約 §2）。
    fn call_tool(&mut self, tool: &str, args: Value) -> Reply {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": args}
        }));
        self.next_id += 1;
        let response = self.read();
        if let Some(error) = response.get("error") {
            return Reply::Rpc {
                code: error["code"].as_i64().unwrap_or_default(),
                message: error["message"].as_str().unwrap_or_default().to_owned(),
            };
        }
        let result = response
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("tools/call 應有 result 或 error，實際為 {response}"));
        Reply::Tool {
            is_error: result["isError"].as_bool().unwrap_or(false),
            result,
        }
    }

    /// `tools/list` 回傳的工具名稱；daemon 不可連時回傳空 vec 之外的錯誤由呼叫端處理。
    fn tool_names(&mut self) -> Result<Vec<String>, Reply> {
        self.send(&json!({"jsonrpc": "2.0", "id": self.next_id, "method": "tools/list"}));
        self.next_id += 1;
        let response = self.read();
        if let Some(error) = response.get("error") {
            return Err(Reply::Rpc {
                code: error["code"].as_i64().unwrap_or_default(),
                message: error["message"].as_str().unwrap_or_default().to_owned(),
            });
        }
        let names = response["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("tools/list 應回 tools 陣列，實際為 {response}"))
            .iter()
            .map(|tool| {
                tool["name"]
                    .as_str()
                    .unwrap_or_else(|| panic!("工具定義應有字串名稱，實際為 {tool}"))
                    .to_owned()
            })
            .collect();
        Ok(names)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ------------------------------------------------------------ admission 層

#[test]
fn tools_whose_handler_has_not_landed_are_not_silently_ignored() {
    // T4 的 `tools_list` 只列已完成的 lifecycle 工具；這些不列出。直接呼叫
    // 時不得被靜默忽略，也不得假裝成功——daemon 在線時必須回
    // `unsupported_in_phase`（daemon 端的行為由 Lane A 的 instance_lifecycle
    // 覆蓋），daemon 不在線時 bridge 回 -32001（下面另有測試）。
    //
    // 注意 `create_instance`／`delete_instance`／`start_instance` 是 catalog 中
    // 的 P1a 工具，它們的**規格存在**，所以 `admission` 的形狀檢查是通過的；
    // 「這期還沒做」是執行期（daemon 端 handler）的判斷，不是形狀問題。
    // 這裡驗的是這個區分：形狀合法，但不得因為形狀合法就假設可呼叫。
    for tool in ["create_instance", "delete_instance", "start_instance"] {
        let args = match tool {
            "create_instance" => json!({"name": "worker"}),
            _ => json!({"instance": "worker"}),
        };
        assert!(
            admit(tool, &args).is_ok(),
            "繁中說明：{tool} 是 catalog 中的 P1a 工具，形狀檢查應通過"
        );
        assert!(
            !otterhive::mcp::tool_names(true).contains(&tool.to_owned()),
            "繁中說明：{tool} 的 handler 未落地，不得出現在 T4 的工具清單裡"
        );
    }

    // 真正在形狀層就被拒的是**非 P1a 工具**：它們連規格都沒有。
    for tool in ["decision", "task", "set_metadata", "schedule"] {
        let error = admit(tool, &json!({})).expect_err("繁中說明：非 P1a 工具應被 admission 拒絕");
        assert_eq!(
            error.code,
            ErrorCode::UnsupportedInPhase,
            "繁中說明：{tool} 應回 unsupported_in_phase，實際為 {error:?}"
        );
        assert!(
            error.message.contains("P1a 不支援"),
            "繁中說明：{tool} 的訊息應明確表示 P1a 不支援，實際為 {:?}",
            error.message
        );
        assert!(
            !error.retryable,
            "繁中說明：同一請求永遠同樣被拒，重試無意義（{tool}）"
        );
    }
}

#[test]
fn a_caller_cannot_self_report_sender() {
    // 契約 §2：呼叫端不能自報 sender；sender 一律由憑證推導。
    //
    // `from`／`sender` 不在任何 P1a 工具的 `supported_fields ∪ added_fields`，
    // 也不在任何工具的 `refused_fields`（舊版根本沒有這兩個欄位），因此
    // 應回 `invalid_argument` —— 不是 `unsupported_in_phase`。
    //
    // 注意 `team` **不在**這個清單裡：`send.team` 是「寄給這個 team」的收件者
    // 選取器，不是「我屬於這個 team」的自報。成員身分由設定與憑證推導，
    // 沒有任何欄位能表達它。見 `send_team_is_a_recipient_not_a_self_report`。
    for field in ["from", "sender"] {
        let mut args = json!({"message": "內容"});
        args.as_object_mut()
            .expect("繁中說明：args 應為物件")
            .insert(field.to_owned(), json!("worker"));
        let error = admit("send", &args).expect_err("繁中說明：自報 sender 的欄位應被拒絕");
        assert_eq!(
            error.code,
            ErrorCode::InvalidArgument,
            "繁中說明：send.{field} 是舊版也沒有的欄位，應回 invalid_argument 而非 unsupported，實際為 {error:?}"
        );
        assert!(
            error.message.contains(field),
            "繁中說明：拒絕訊息應指出無效欄位 {field}，實際為 {:?}",
            error.message
        );

        // 跨工具：這兩個欄位在每個 P1a 工具上都是「從來不存在」，
        // 不該有任何工具把它歸類成「舊版有、P1a 延期」。
        for tool in [
            "inbox",
            "list_instances",
            "pane_snapshot",
            "team",
            "interrupt",
        ] {
            let mut args = json!({});
            args.as_object_mut()
                .expect("繁中說明：args 應為物件")
                .insert(field.to_owned(), json!("worker"));
            let error = admit(tool, &args).expect_err("繁中說明：自報 sender 的欄位應被拒絕");
            assert_eq!(
                error.code,
                ErrorCode::InvalidArgument,
                "繁中說明：{tool}.{field} 應回 invalid_argument 而非 unsupported_in_phase，實際為 {error:?}"
            );
        }
    }
}

#[test]
fn send_team_is_a_recipient_not_a_self_report() {
    // `send.team` 是收件者選取器（契約 §6.1：`instance`、`instances`、`team`
    // 三擇一），不是自報身分。接受它不違反「呼叫端不能自報 team」——
    // 因為它表達的是「寄給誰」，而「我是誰」永遠來自憑證。
    assert!(
        admit("send", &json!({"message": "內容", "team": "platform"})).is_ok(),
        "繁中說明：send.team 是合法的收件者選取器"
    );

    // 真正會構成自報 team 的形狀——同時指定收件者與身分——沒有任何欄位能
    // 承載：`send` 的欄位清單裡沒有任何一項表示「發送者所屬的 team」。
    let supported: Vec<&str> = otterhive::admission::op_spec("send")
        .expect("send 應有 OpSpec")
        .fields
        .to_vec();
    for candidate in [
        "from_team",
        "my_team",
        "as_team",
        "sender_team",
        "member_of",
    ] {
        assert!(
            !supported.contains(&candidate),
            "繁中說明：send 不應有 {candidate} 這類表達成員身分的欄位"
        );
    }
}

#[test]
fn role_is_a_descriptive_field_of_create_instance_only() {
    // 契約 §2：`role` 是描述文字，不授予任何權限。因此 `role` 只在
    // `create_instance` 是合法欄位，其他工具帶 `role` 都是自報身分。
    assert!(
        admit(
            "create_instance",
            &json!({"name": "worker", "role": "manager"})
        )
        .is_ok(),
        "繁中說明：create_instance.role 是描述欄位，admission 應接受"
    );
    for tool in [
        "restart_instance",
        "pane_snapshot",
        "interrupt",
        "list_instances",
    ] {
        let error = admit(tool, &json!({"instance": "worker", "role": "manager"}))
            .expect_err("繁中說明：其他 lifecycle 工具不應接受 role 欄位");
        assert_eq!(
            error.code,
            ErrorCode::InvalidArgument,
            "繁中說明：{tool}.role 是自報身分，應回 invalid_argument，實際為 {error:?}"
        );
    }
}

#[test]
fn unsupported_and_invalid_argument_are_not_interchangeable() {
    // 契約 §3：本期不做的工具／欄位 → `unsupported_in_phase`；
    // 從未存在的欄位／值 → `invalid_argument`。兩者都不得被當成授權問題。
    // 這裡刻意在「工具」與「欄位」兩層各取一例。
    let deferred_tool = admit("decision", &json!({})).expect_err("decision 是非 P1a 工具");
    assert_eq!(deferred_tool.code, ErrorCode::UnsupportedInPhase);
    assert!(
        deferred_tool.message.contains("P2"),
        "繁中說明：非 P1a 工具應另列 catalog phase，實際為 {:?}",
        deferred_tool.message
    );

    let refused_field = admit("restart_instance", &json!({"instance": "w", "force": true}))
        .expect_err("force 在 P1a 拒收");
    assert_eq!(refused_field.code, ErrorCode::UnsupportedInPhase);
    assert!(
        refused_field.message.contains("force"),
        "繁中說明：拒收欄位的訊息應指出欄位，實際為 {:?}",
        refused_field.message
    );
    assert!(
        !refused_field.message.contains("P2") && !refused_field.message.contains("P1b"),
        "繁中說明：P1a 工具的欄位不得借用工具的 phase 宣稱移植期別（契約 §3），實際為 {:?}",
        refused_field.message
    );

    let unknown_field =
        admit("pane_snapshot", &json!({"instance": "w", "depth": 3})).expect_err("depth 從未存在");
    assert_eq!(unknown_field.code, ErrorCode::InvalidArgument);

    let unknown_value = admit(
        "restart_instance",
        &json!({"instance": "w", "mode": "warm"}),
    )
    .expect_err("mode=warm 從未存在");
    assert_eq!(unknown_value.code, ErrorCode::InvalidArgument);

    for error in [deferred_tool, refused_field, unknown_field, unknown_value] {
        assert_ne!(
            error.code,
            ErrorCode::Unauthorized,
            "繁中說明：分期與形狀問題不是授權問題（契約 §3）"
        );
    }
}

#[test]
fn every_contract_error_code_serializes_as_its_snake_case_name() {
    // 契約 §3 列舉的 15 個錯誤碼；線路上必須是可辨識的 snake_case 名稱，
    // CLI 與 bridge 才有辦法依碼分支。
    for (code, wire) in [
        (ErrorCode::Unauthorized, "unauthorized"),
        (ErrorCode::NotFound, "not_found"),
        (ErrorCode::NameConflict, "name_conflict"),
        (ErrorCode::InvalidArgument, "invalid_argument"),
        (ErrorCode::StaleGeneration, "stale_generation"),
        (ErrorCode::OperationInProgress, "operation_in_progress"),
        (ErrorCode::SpawnFailed, "spawn_failed"),
        (ErrorCode::ResumeUnavailable, "resume_unavailable"),
        (ErrorCode::RecoveryRequired, "recovery_required"),
        (ErrorCode::DeliveryTokenStale, "delivery_token_stale"),
        (ErrorCode::NeverDelivered, "never_delivered"),
        (ErrorCode::TeamAclDenied, "team_acl_denied"),
        (ErrorCode::UnsupportedInPhase, "unsupported_in_phase"),
        (ErrorCode::DaemonAlreadyRunning, "daemon_already_running"),
        (ErrorCode::Internal, "internal"),
    ] {
        let error = otterhive::protocol::OpError {
            code,
            message: "測試".to_owned(),
            retryable: false,
            next: None,
        };
        assert_eq!(
            serde_json::to_value(&error).expect("OpError 應可序列化")["code"],
            json!(wire),
            "繁中說明：{code:?} 應序列化為 {wire}"
        );
    }
}

// ------------------------------------------------------------ bridge 端形狀

#[test]
fn lifecycle_calls_are_not_disguised_as_results_when_the_daemon_is_unreachable() {
    // 每個 T4 列出的 lifecycle 工具都要在 daemon 不可連時回 -32001，不得
    // 偽裝成 `unsupported`／`refused` 的工具結果——那會讓 backend 以為
    // 授權已被行使過（契約 §10「不假裝成功」）。
    let home = TempHome::new("authority_unreachable");
    let mut bridge = Bridge::start(home.home(), INSTANCE_TOKEN);
    bridge.handshake();

    for tool in [
        "list_instances",
        "pane_snapshot",
        "restart_instance",
        "interrupt",
    ] {
        let reply = bridge.call_tool(tool, json!({"instance": "worker"}));
        reply.expect_daemon_unreachable(&format!("呼叫 {tool}"));
        // `-32001` 是 JSON-RPC 層的錯誤，不是工具結果，因此沒有 `isError`。
        // daemon 在線時 `refused` 才會是 `isError: true`（契約 §10）——把這個
        // 區分釘住，才不會有人日後把它改成「一律 isError」。
        assert_eq!(
            reply.is_error(),
            None,
            "繁中說明：daemon 不可連的回覆不該有 isError（{tool}）"
        );
    }
}

#[test]
fn an_unlisted_tool_cannot_be_invoked_by_raw_call() {
    // T4 不列出 create／delete／start／deployment，但 raw `tools/call`
    // 也不該讓它們看起來可用。daemon 不可連時這裡只能證明「不是工具結果」；
    // daemon 在線時的回 `unsupported_in_phase` 由 Lane A 的測試覆蓋。
    let home = TempHome::new("authority_unlisted");
    let mut bridge = Bridge::start(home.home(), INSTANCE_TOKEN);
    bridge.handshake();

    for tool in [
        "create_instance",
        "delete_instance",
        "start_instance",
        "deployment",
    ] {
        let args = match tool {
            "create_instance" => json!({"name": "worker"}),
            "delete_instance" | "start_instance" => json!({"instance": "worker"}),
            _ => json!({"action": "list"}),
        };
        let reply = bridge.call_tool(tool, args);
        reply.expect_daemon_unreachable(&format!("raw 呼叫未列出的 {tool}"));
    }
}

#[test]
fn tools_list_for_an_instance_is_not_available_without_a_daemon() {
    // `tools_list` 的內容由 daemon 依 caller 推導（§10），因此 bridge 不得
    // 在本地猜一份清單。daemon 不可連時必須回 -32001，而不是空清單。
    let home = TempHome::new("authority_tools_list");
    let mut bridge = Bridge::start(home.home(), INSTANCE_TOKEN);
    bridge.handshake();

    match bridge.tool_names() {
        Ok(names) => {
            panic!("繁中說明：daemon 不可連時 tools/list 不得回本地猜的清單，實際為 {names:?}")
        }
        Err(reply) => reply.expect_daemon_unreachable("tools/list"),
    }
}

// ------------------------------------------------ §6.2 收件權限（四條分支）

/// 契約 §6.2 的四條分支在 `admission` 層的**形狀**前提。
///
/// 這裡不啟動 daemon：`admission` 只做形狀檢查（§3），收件權限的判定是
/// daemon 端依憑證與 team 定義做出的執行期判斷——那屬於 Lane A 的
/// `tests/message_delivery.rs`。本檔能確定地驗的是：
///
/// 1. 跨 team 的拒絕碼是 `team_acl_denied`，**不是** `unauthorized`
///    （§6.2 明文「其他：`team_acl_denied`」）。
/// 2. 授權相關的拒絕碼在 `ErrorCode` 中存在且可序列化（§3 的 15 個碼）。
/// 3. inbox 的欄位形狀：只有 `action`／`message_id`／`delivery_token`
///    （§6.3），**沒有** `instance`——它無法指定要 drain 誰的。
#[test]
fn cross_team_denial_has_its_own_error_code_not_unauthorized() {
    // §6.2：recipient 不屬任何 team → 允許；同 team → 允許；
    // recipient 是某 team 的 orchestrator 且 sender 在 accept_from → 允許；
    // **其他** → `team_acl_denied`。
    //
    // 「其他」包括：不同 team、非同 team 的 orchestrator、sender 不在
    // accept_from。這些是 ACL 的判定結果，不是身分認證失敗——把它們折成
    // `unauthorized` 會讓 caller 無從判斷該換 team 關係還是該換憑證。
    let denied = otterhive::protocol::OpError {
        code: otterhive::protocol::ErrorCode::TeamAclDenied,
        message: "recipient 不在允許的 team 範圍內".to_owned(),
        retryable: false,
        next: Some("請調整 team 關係或 accept_from，或改由 operator 送出".to_owned()),
    };
    assert_ne!(
        denied.code,
        otterhive::protocol::ErrorCode::Unauthorized,
        "繁中說明：跨 team 拒絕不得與 unauthorized 合併（契約 §6.2／§3）"
    );
    let wire = serde_json::to_value(&denied).expect("OpError 應可序列化");
    assert_eq!(
        wire["code"],
        json!("team_acl_denied"),
        "繁中說明：線上代碼必須是 team_acl_denied，實際為 {wire}"
    );
    assert!(
        !denied.retryable,
        "繁中說明：team 關係不會因重試而改變，重試無意義"
    );
}

/// §6.2 的「允許」分支所依賴的欄位必須真的在 `send` 的規格裡。
///
/// 三個允許分支各自需要一個可比較的東西：recipient 的 team 歸屬、sender 的
/// team 歸屬、以及 `accept_from` 的名稱比對。前兩個由 daemon 從 team 定義
/// 推導（§2：呼叫端不能自報 team），第三個是 `team` 操作的欄位。
/// 因此 `send` 本身**不該**有 `my_team` 這類自報欄位。
#[test]
fn send_carries_no_self_reported_team_field() {
    let fields = otterhive::mcp::input_schema_fields("send");
    assert!(
        !fields.contains(&"my_team".to_owned()),
        "繁中說明：send 不應有 my_team 自報欄位，實際為 {fields:?}"
    );
    assert!(
        !fields.contains(&"accept_from".to_owned()),
        "繁中說明：accept_from 是 team 定義的欄位（§8.1），不是每次 send 附帶的（§6.2），實際為 {fields:?}"
    );
    // §6.1 的收件人三擇一都在（那是「寄給誰」，不是「我是誰」）。
    for field in ["instance", "instances", "team"] {
        assert!(
            fields.contains(&field.to_owned()),
            "繁中說明：send 的收件人三擇一應含 {field}（契約 §6.1），實際為 {fields:?}"
        );
    }
}

/// §6.3 的 inbox：欄位只有 action／message_id／delivery_token。
///
/// **特別是沒有 `instance`**——`inbox` 的 `instance` 在 catalog 的
/// `refused_fields` 裡，因此「指定要 drain 誰的 inbox」在形狀層就被擋掉。
/// 這是「inbox 只有自己」的**第一道**防線；第二道是 daemon 端的授權判定。
#[test]
fn inbox_cannot_be_addressed_to_another_instance() {
    let error = admit("inbox", &json!({"instance": "worker"}))
        .expect_err("繁中說明：inbox 帶 instance 應被拒絕");
    assert_eq!(
        error.code,
        ErrorCode::UnsupportedInPhase,
        "繁中說明：inbox.instance 是舊版有、P1a 拒收的欄位，應回 unsupported_in_phase，實際為 {error:?}"
    );
    assert!(
        error.message.contains("instance"),
        "繁中說明：訊息應指出被拒的欄位 instance，實際為 {:?}",
        error.message
    );
    assert!(
        !error.message.contains("P2") && !error.message.contains("P1b"),
        "繁中說明：P1a 工具的欄位不得借用工具的 phase 宣稱移植期別（契約 §3），實際為 {:?}",
        error.message
    );
}

/// §6.3 的 inbox 欄位集合恰好三個。
#[test]
fn inbox_schema_is_exactly_action_message_id_and_delivery_token() {
    assert_eq!(
        otterhive::mcp::input_schema_fields("inbox"),
        vec![
            "action".to_owned(),
            "delivery_token".to_owned(),
            "message_id".to_owned()
        ],
        "繁中說明：inbox 的 schema 應只有 §6.3 的三個欄位"
    );
}

/// §6.3 的 action 值：`drain`（省略時的預設）與 `ack`；舊版的 `clear`／
/// `discharge` 拒收。
#[test]
fn inbox_accepts_drain_and_ack_and_refuses_the_legacy_actions() {
    for action in ["drain", "ack"] {
        assert!(
            admit("inbox", &json!({"action": action})).is_ok(),
            "繁中說明：inbox action={action} 應通過形狀檢查（契約 §6.3）"
        );
    }
    for legacy in ["clear", "discharge"] {
        let error =
            admit("inbox", &json!({"action": legacy})).expect_err("繁中說明：舊版 action 應被拒絕");
        assert_eq!(
            error.code,
            ErrorCode::UnsupportedInPhase,
            "繁中說明：inbox action={legacy} 應回 unsupported_in_phase，實際為 {error:?}"
        );
    }
    // 從未存在的 action 是 invalid_argument，不是 unsupported（§3）。
    let unknown = admit("inbox", &json!({"action": "purge"})).expect_err("purge 從未存在");
    assert_eq!(
        unknown.code,
        ErrorCode::InvalidArgument,
        "繁中說明：從未存在的 action 應回 invalid_argument（契約 §3），實際為 {unknown:?}"
    );
}

/// §6.1 的 `request_kind`：接受 `query`／`report`／update`／省略，
/// 拒收 `task`（回 `unsupported_in_phase`，§6.1 明文）。
///
/// 省略時記為 `message` 這件事不需要任何欄位——CLI 不送、bridge 也不送，
/// 由 daemon 端補上。因此「省略」在形狀上就是 `admit("send", {message})`
/// 通過。
#[test]
fn send_request_kind_accepts_the_contract_values_and_refuses_task() {
    for kind in ["query", "report", "update"] {
        assert!(
            admit("send", &json!({"message": "內容", "request_kind": kind})).is_ok(),
            "繁中說明：request_kind={kind} 應通過（契約 §6.1）"
        );
    }
    // 省略即 `message`：不需要任何欄位。
    assert!(
        admit("send", &json!({"message": "內容"})).is_ok(),
        "繁中說明：省略 request_kind 應通過（daemon 記為 message，契約 §6.1）"
    );
    let task = admit("send", &json!({"message": "內容", "request_kind": "task"}))
        .expect_err("繁中說明：request_kind=task 應被拒絕");
    assert_eq!(
        task.code,
        ErrorCode::UnsupportedInPhase,
        "繁中說明：request_kind=task 應回 unsupported_in_phase（契約 §6.1），實際為 {task:?}"
    );
    // 從未存在的值是 invalid_argument。
    let unknown = admit(
        "send",
        &json!({"message": "內容", "request_kind": "urgent"}),
    )
    .expect_err("urgent 從未存在");
    assert_eq!(
        unknown.code,
        ErrorCode::InvalidArgument,
        "繁中說明：從未存在的 request_kind 值應回 invalid_argument（契約 §3）"
    );
}

/// §2：sender 一律由憑證推導。`send` 與 `inbox` 都不得有任何自報身分的
/// 欄位；跨工具的 `from`／`sender` 一律 `invalid_argument`。
///
/// 這是「operator 不能假冒別人送訊息」與「agent 不能假冒 operator 送訊息」
/// 的形狀層基礎。
#[test]
fn no_message_tool_accepts_a_self_reported_identity_field() {
    for field in ["from", "sender", "role", "author", "operator"] {
        for (tool, args) in [("send", json!({"message": "內容"})), ("inbox", json!({}))] {
            let mut args = args;
            args.as_object_mut()
                .expect("繁中說明：args 應為物件")
                .insert(field.to_owned(), json!("worker"));
            let error = admit(tool, &args)
                .err()
                .unwrap_or_else(|| panic!("繁中說明：{tool}.{field} 應被拒絕，卻通過了"));
            assert_eq!(
                error.code,
                ErrorCode::InvalidArgument,
                "繁中說明：{tool}.{field} 是自報身分且從未存在，應回 invalid_argument（契約 §2／§3），實際為 {error:?}"
            );
            assert_ne!(
                error.code,
                ErrorCode::UnsupportedInPhase,
                "繁中說明：{tool}.{field} 從未存在，不是「舊版有但延期」"
            );
        }
    }
}

/// §6.1 的訊息大小上限（64 KiB）是 daemon 端的檢查；`admission` 不做內容
/// 長度判斷。這裡只釘住 `message` 是 `send` 的合法欄位，且**必須**是字串
/// 形狀的一部分存在——若 GREEN 把 `message` 從規格拿掉，下列斷言會立刻紅。
#[test]
fn send_requires_a_message_field_in_its_shape() {
    let fields = otterhive::mcp::input_schema_fields("send");
    assert!(
        fields.contains(&"message".to_owned()),
        "繁中說明：send 的規格必須含 message（契約 §6.1），實際為 {fields:?}"
    );
    for field in ["correlation_id", "parent_id", "thread_id"] {
        assert!(
            fields.contains(&field.to_owned()),
            "繁中說明：send 的規格必須含 {field}（契約 §6.1 的不透明字串），實際為 {fields:?}"
        );
    }
    assert!(
        admit("send", &json!({"message": "內容"})).is_ok(),
        "繁中說明：只有 message 的 send 應通過形狀檢查"
    );
}

/// §6.4 的 `wake` 值是 `send` **結果**的欄位，不是請求欄位。
///
/// 因此 `wake` 既不得出現在 `send` 的 schema，也不得被當成請求參數接受——
/// 呼叫端不能自報「我已經喚醒過對方」。
#[test]
fn wake_is_a_result_field_and_cannot_be_self_reported() {
    assert!(
        !otterhive::mcp::input_schema_fields("send").contains(&"wake".to_owned()),
        "繁中說明：wake 是 §6.4 的結果欄位，不該出現在請求 schema 裡"
    );
    let error = admit("send", &json!({"message": "內容", "wake": "hinted"}))
        .expect_err("繁中說明：自報 wake 應被拒絕");
    assert_eq!(
        error.code,
        ErrorCode::InvalidArgument,
        "繁中說明：wake 從未是請求欄位，應回 invalid_argument，實際為 {error:?}"
    );
    // 四個值都要在 `WakeOutcome` 的線上表示裡（wake.rs 的單元測試覆蓋
    // 字串本身）；這裡確認模組確實公開了這四個。
    for expected in ["hinted", "deferred", "not_running", "failed"] {
        let rendered = [
            otterhive::wake::WakeOutcome::Hinted,
            otterhive::wake::WakeOutcome::Deferred,
            otterhive::wake::WakeOutcome::NotRunning,
            otterhive::wake::WakeOutcome::Failed,
        ]
        .iter()
        .map(|outcome| outcome.as_wire())
        .collect::<Vec<_>>();
        assert!(
            rendered.contains(&expected),
            "繁中說明：wake 的四個值應含 {expected}（契約 §6.4），實際為 {rendered:?}"
        );
    }
}

/// §8.1 的 team action：`create`／`update`／`delete`／`list`。
/// `accept_from` 是 team 定義的欄位，也是 §6.2 的收件權限依據——它必須在
/// `team` 的規格裡，否則 §6.2 第三個分支無從判定。
#[test]
fn team_accepts_the_contract_actions_and_carries_accept_from() {
    for action in ["create", "update", "delete", "list"] {
        assert!(
            admit("team", &json!({"action": action})).is_ok(),
            "繁中說明：team action={action} 應通過（契約 §8.1）"
        );
    }
    let fields = otterhive::mcp::input_schema_fields("team");
    for field in [
        "action",
        "name",
        "members",
        "orchestrator",
        "description",
        "add",
        "remove",
        "accept_from",
    ] {
        assert!(
            fields.contains(&field.to_owned()),
            "繁中說明：team 的規格應含 {field}（契約 §8.1），實際為 {fields:?}"
        );
    }
}
