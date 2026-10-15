//! T5 Lane A：兩個 agent 透過真 bridge 交換 query／report（plan T5 的
//! `two_agents_exchange_query_and_report_through_the_bridge`）。
//!
//! 這是 T5 最末端的整合驗收：兩個 `mcp_agent.sh` instance（各跑
//! `python3 -I <fixture>`）、真 `otterhive-mcp-bridge` binary、真 daemon 的
//! `send`／`inbox`／喚醒排程。**沒有任何 stub**：agent 收到 `[OTTERHIVE-MSG]`
//! 提示後真的啟動 bridge、真的 drain、真的逐則 ack。
//!
//! 流程（§6.1 的 `relay:<target>:<text>` 與 §6.3 的 drain／ack）：
//!
//! ```text
//! operator ──send("relay:b:ping")──▶ a ──send(query "ping", parent_id)──▶ b
//!                                                                          │
//!                                       a ◀──send(report "pong:ping", ──────┘
//!                                            parent_id = 剛收到的 query 的 id)
//! ```
//!
//! ## 為什麼斷言要容忍重複
//!
//! §6.3 承諾的是**至少一次**投遞：lease 逾時、fresh restart、daemon 重啟都會讓
//! 未 ack 的訊息再次被 drain，接收端可能看到同一則兩次。所以斷言寫成「a 的 log
//! 裡有 body 含 `pong:ping` 的 report」與「b 的 log 裡有 body 含 `ping` 的
//! query」，不斷言「恰好一則」。那個「恰好」的條件留給 §6.3 專屬的
//! `message_delivery.rs`（它用 `delivery_count` 直接讀 store）。
//!
//! ## 為什麼多了一層 `mcp_agent.sh` wrapper
//!
//! 契約 §5.1 的 `claude` backend 只把 `command` 當**可執行檔的覆寫**，argv 仍照
//! claude 的形狀組成（`--dangerously-skip-permissions`、
//! `--append-system-prompt-file`、`--mcp-config`）；而 §5.2 的「只執行 `command`
//! 加 `args`」是 `backend: command`，在 P1a 回 `unsupported_in_phase`。python 在
//! 腳本路徑**之前**解析自己的選項，遇到這些旗標會 `Unknown option` 並 exit 2。
//! wrapper 自己 `exec python3 -I mcp_agent.py`，argv 完全由它決定；agent 需要的
//! bridge 路徑與 token 都來自 §5.3 的**環境變數**，不需要 argv。
//!
//! ## backend 一律用絕對路徑
//!
//! T4 的 `backend_contract.rs` 曾因省略 `command` 而在 CI 走到 PATH 上的真 `claude`
//! 而紅掉（`HANDOFF` 的待辦 #3）。這裡 `command` 是 `mcp_agent.sh` 的**絕對路徑**，
//! 而它內部的直譯器預設是 `/usr/bin/python3`（可用 `OTTERHIVE_TEST_PYTHON` 覆寫成
//! 別的絕對路徑）——`python3` 與 `claude` 都不從 PATH 查找。
//!
//! 全檔只在 macOS 編譯（契約 §1）；所有等待都有上限並回報最後觀測狀態。

#![cfg(target_os = "macos")]

mod support {
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use otterhive::home::Home;
    use otterhive::protocol::{Auth, OperationResult, Request};
    use serde_json::{json, Value};
    use tempfile::TempDir;

    pub const POLL_STEP: Duration = Duration::from_millis(20);
    pub const LONG_TIMEOUT: Duration = Duration::from_secs(60);
    pub const RECV_TIMEOUT: Duration = Duration::from_secs(20);

    /// plan T5 的端到端驗收會走多輪 spawn／bridge 子程序；給它比單元測試寬鬆的
    /// 期限，但仍有上限（逾時時報出最後觀測到的 log）。
    pub const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(45);

    /// 單次 socket／process 回應的等待上限。
    pub const SHORT_TIMEOUT: Duration = Duration::from_secs(5);

    pub fn daemon_bin() -> &'static str {
        match option_env!("CARGO_BIN_EXE_otterhive") {
            Some(path) => path,
            None => panic!("繁中說明：otterhive binary 尚未建立（本測試為 integration 測試）"),
        }
    }

    pub fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/backends")
            .join(name)
    }

    pub struct TestHome {
        pub home: Home,
        _root: TempDir,
    }

    impl TestHome {
        pub fn new() -> Self {
            let root = TempDir::new().expect("繁中說明：測試需建立隔離暫存目錄");
            let home =
                Home::initialize(&root.path().join("h")).expect("繁中說明：暫存 home 應可初始化");
            Self { home, _root: root }
        }

        pub fn root(&self) -> &Path {
            self.home.root()
        }

        pub fn socket_path(&self) -> PathBuf {
            self.home.run_dir().join("daemon.sock")
        }

        pub fn operator_token(&self) -> String {
            fs::read_to_string(self.home.operator_token_path())
                .expect("繁中說明：應可讀取 operator 憑證")
                .trim()
                .to_owned()
        }

        pub fn operator(&self) -> Auth {
            Auth::Operator(self.operator_token())
        }

        /// `mcp_agent.py` 的 log 路徑（§fixture：每個 instance 一份）。
        pub fn fixture_log(&self, name: &str) -> PathBuf {
            self.home.root().join(format!("{name}-fixture.log"))
        }
    }

    pub struct Daemon {
        child: Option<Child>,
    }

    impl Daemon {
        pub fn start(home: &TestHome) -> Self {
            Self::start_with_env(home, &[])
        }

        pub fn start_with_env(home: &TestHome, extra_env: &[(&str, &str)]) -> Self {
            let root = home.root().to_path_buf();
            let mut command = Command::new(daemon_bin());
            command
                .arg("daemon")
                .env("HOME", &root)
                .env("OTTERHIVE_HOME", &root)
                .env("OTTERHIVE_TEST_STARTUP_GRACE_MS", "600")
                .env("OTTERHIVE_TEST_STOP_TERM_MS", "1000")
                .env("OTTERHIVE_TEST_STOP_KILL_MS", "500")
                .current_dir(&root)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            for (key, value) in extra_env {
                command.env(key, value);
            }
            let child = command.spawn().expect("繁中說明：daemon 應可啟動");
            let daemon = Self { child: Some(child) };
            let socket = home.socket_path();
            let deadline = Instant::now() + LONG_TIMEOUT;
            loop {
                if socket.exists() && UnixStream::connect(&socket).is_ok() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "繁中說明：daemon 應在期限內可服務 {}（啟動自檢失敗？）",
                    socket.display()
                );
                std::thread::sleep(POLL_STEP);
            }
            daemon
        }

        pub fn call(&self, home: &TestHome, auth: &Auth, op: &str, args: Value) -> OperationResult {
            let request = Request {
                v: 1,
                auth: auth.clone(),
                op: op.to_owned(),
                args,
                operation_id: None,
            };
            let socket = home.socket_path();
            let mut stream = UnixStream::connect(&socket)
                .unwrap_or_else(|e| panic!("繁中說明：應能連到 {}：{e}", socket.display()));
            let _ = stream.set_read_timeout(Some(RECV_TIMEOUT));
            let _ = stream.set_write_timeout(Some(RECV_TIMEOUT));
            let mut line =
                serde_json::to_string(&request).expect("繁中說明：請求應可序列化為 JSON");
            line.push('\n');
            stream
                .write_all(line.as_bytes())
                .expect("繁中說明：應能送出請求");
            let mut reader = BufReader::new(stream);
            let mut response = String::new();
            reader
                .read_line(&mut response)
                .unwrap_or_else(|e| panic!("繁中說明：應可讀回一行回應：{e}"));
            serde_json::from_str(response.trim())
                .unwrap_or_else(|e| panic!("繁中說明：回應應是合法 JSON：{e}；實際 {response}"))
        }

        pub fn operator_call(&self, home: &TestHome, op: &str, args: Value) -> OperationResult {
            let auth = home.operator();
            self.call(home, &auth, op, args)
        }
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            let Some(mut child) = self.child.take() else {
                return;
            };
            let pid = child.id() as i32;
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            let start = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => {}
                    Err(_) => break,
                }
                if start.elapsed() >= Duration::from_secs(5) {
                    break;
                }
                std::thread::sleep(POLL_STEP);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// 在 `cond()` 為真前輪詢到 `timeout`；回傳最後一次結果（不 panic）。
    pub fn poll_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        loop {
            if cond() {
                return true;
            }
            if start.elapsed() >= timeout {
                return cond();
            }
            std::thread::sleep(POLL_STEP);
        }
    }

    /// 以 `mcp_agent.py`（絕對路徑的 `python3 -I`）建立並啟動一個 instance。
    ///
    /// `env` 帶 `OTTERHIVE_FIXTURE_LOG`；§5.3 的 `OTTERHIVE_HOME`／token／bridge
    /// 由 daemon 在 spawn 時加上（§fixture 不需要自己設定）。
    /// 以 `mcp_agent.sh`（絕對路徑）建立並啟動一個 agent instance。
    ///
    /// **為什麼是 `.sh` 而不是直接 `python3 -I <py>`**：契約 §5.1 的 `claude`
    /// backend 只把 `command` 當**可執行檔的覆寫**，argv 仍照 claude 的形狀組成
    /// （`--dangerously-skip-permissions`、`--append-system-prompt-file`、
    /// `--mcp-config`）；`backend: command`（§5.2 的「只執行 command 加 args」）在
    /// P1a 回 `unsupported_in_phase`。python 在**腳本路徑之前**解析自己的選項，
    /// 遇到這些旗標會以 `Unknown option` / exit 2 結束，於是 `create_instance` 回
    /// `spawn_failed`／`exited_during_startup`。wrapper 自己 exec `python3 -I
    /// <script>`，argv 完全由它決定；agent 需要的 bridge 路徑與 token 都來自
    /// §5.3 的**環境變數**，不需要 argv。
    pub fn create_agent(daemon: &Daemon, home: &TestHome, name: &str) -> OperationResult {
        let created = daemon.operator_call(
            home,
            "create_instance",
            json!({
                "name": name,
                "command": fixture("mcp_agent.sh").to_string_lossy(),
                "env": {"OTTERHIVE_FIXTURE_LOG": home.fixture_log(name).to_string_lossy()},
            }),
        );
        assert_eq!(
            created.outcome,
            otterhive::protocol::Outcome::Completed,
            "繁中說明：create_instance {name} 應完成（測試前提），實際為 {created:?}；\
             fixture log：{:?}",
            read_log(&home.fixture_log(name))
        );
        created
    }

    /// 讀一份 fixture log：每行一個 JSON 物件（§fixture）。
    ///
    /// 讀到一半的行（fixture 正在寫）會被忽略——`read_to_string` + split 只取
    /// 能解析的那些行。
    pub fn read_log(path: &Path) -> Vec<Value> {
        let Ok(text) = fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
            .collect()
    }

    /// log 中 `event` 為指定值的記錄。
    pub fn events_named<'a>(log: &'a [Value], event: &str) -> Vec<&'a Value> {
        log.iter()
            .filter(|item| item["event"] == json!(event))
            .collect()
    }

    /// `received` 事件裡 body 含 `needle` 的訊息（§6.3 的至少一次投遞：可能多則）。
    pub fn received_bodies(log: &[Value], needle: &str) -> Vec<String> {
        events_named(log, "received")
            .iter()
            .filter_map(|item| item["message"]["body"].as_str())
            .filter(|body| body.contains(needle))
            .map(str::to_owned)
            .collect()
    }
}

use serde_json::json;

use support::*;

/// plan T5 的端到端驗收：operator 送 `relay:b:ping` 給 a → a 送 query `ping` 給 b
/// → b 回 report `pong:ping` 給 a。
#[test]
fn two_agents_exchange_query_and_report_through_the_bridge() {
    let home = TestHome::new();
    let daemon = Daemon::start(&home);
    create_agent(&daemon, &home, "a");
    create_agent(&daemon, &home, "b");

    // 兩個 agent 的 log 都應先有 `started`，證明 bridge 路徑與 token 到位
    // （§5.3 的環境變數由 daemon 注入；fixture 自己 log 出來供觀察）。
    let log_a = home.fixture_log("a");
    let log_b = home.fixture_log("b");
    let started = poll_until(LONG_TIMEOUT, || {
        !events_named(&read_log(&log_a), "started").is_empty()
            && !events_named(&read_log(&log_b), "started").is_empty()
    });
    assert!(
        started,
        "繁中說明：兩個 mcp_agent 都應記錄 started，最後觀測：a={:?} b={:?}",
        read_log(&log_a),
        read_log(&log_b)
    );

    // operator 送 relay 指令（§6.1：request_kind 省略記為 message）。
    let sent = daemon.operator_call(
        &home,
        "send",
        json!({"instance": "a", "message": "relay:b:ping"}),
    );
    assert_eq!(
        sent.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：send relay 應完成，實際為 {sent:?}"
    );

    // 等待完整往返：a 的 log 有 report `pong:ping`（b 的回覆），且 a 記錄自己
    // 依 relay 指令送出的 query。**`pong` 事件在 b 的 log，不在 a 的**——收到
    // query 並回 report 的是 b（§6.2 的規則 2 同隊允許，fixture 的
    // `elif kind == "query"` 分支）。先前把 `pong` 當成 a 的事件來斷言是錯的。
    let mut last_seen = String::new();
    let exchanged = poll_until(EXCHANGE_TIMEOUT, || {
        let log = read_log(&log_a);
        last_seen = format!(
            "a 的事件：{:?}\n a 的 received body：{:?}",
            log.iter()
                .filter_map(|item| item["event"].as_str())
                .collect::<Vec<_>>(),
            events_named(&log, "received")
                .iter()
                .filter_map(|item| item["message"]["body"].as_str())
                .collect::<Vec<_>>()
        );
        !received_bodies(&log, "pong:ping").is_empty() && !events_named(&log, "relay").is_empty()
    });
    let log_a_final = read_log(&log_a);
    let log_b_final = read_log(&log_b);
    assert!(
        exchanged,
        "繁中說明：a 應收到 report pong:ping 且記錄 relay。\n\
         觀察摘要：{last_seen}\n\
         a 的完整 log：{log_a_final:#?}\n\
         b 的完整 log：{log_b_final:#?}"
    );

    // b 收到 query 後回 report，所以 `pong` 事件在 **b** 的 log（§fixture 的
    // `elif kind == "query"` 分支）。斷言它是「事件的預期 body」，不是欄位集合。
    assert!(
        events_named(&log_b_final, "pong")
            .iter()
            .any(|item| item["text"] == json!("ping")),
        "繁中說明：b 應記錄收到 query 的 pong 事件，實際 b 的 log：{log_b_final:#?}"
    );

    // a 收到 b 的 report。
    let report_bodies = received_bodies(&log_a_final, "pong:ping");
    assert!(
        !report_bodies.is_empty(),
        "繁中說明：a 應收到 body 含 pong:ping 的 report，實際 log：{log_a_final:#?}"
    );
    let report = events_named(&log_a_final, "received")
        .into_iter()
        .find(|item| {
            item["message"]["body"]
                .as_str()
                .is_some_and(|body| body.contains("pong:ping"))
        })
        .expect("繁中說明：exchange 成立時應有對應的 received 記錄")["message"]
        .clone();
    assert_eq!(
        report["kind"],
        json!("report"),
        "繁中說明：b 的回覆應是 report，實際 {report}"
    );
    assert_eq!(
        report["from"],
        json!({"kind": "instance", "name": "b"}),
        "繁中說明：from 應由 b 的 token 推導為 instance b，實際 {report}"
    );

    // b 收到 a 的 query（§6.3：至少一次，所以是「有」而不是「恰好一則」）。
    //
    // 找的是 body **完全等於** `ping` 的那則：`received_bodies(.., "ping")` 用
    // `contains`，會把 b 自己收到的 report `pong:ping` 也算進來（它的 body 含有
    // `ping`）——那樣會挑到錯誤的訊息，後面的 `kind == "query"` 斷言就成了運氣。
    let query = events_named(&log_b_final, "received")
        .into_iter()
        .find(|item| item["message"]["body"] == json!("ping"))
        .map(|item| item["message"].clone())
        .unwrap_or_else(|| {
            panic!("繁中說明：b 應收到 body 為 ping 的 query，實際 b 的 log：{log_b_final:#?}")
        });
    assert!(
        !received_bodies(&log_b_final, "ping").is_empty(),
        "繁中說明：b 應收到含 ping 的 query，實際 log：{log_b_final:#?}"
    );
    assert_eq!(
        query["kind"],
        json!("query"),
        "繁中說明：a 送出的應是 query，實際 {query}"
    );
    assert_eq!(
        query["from"],
        json!({"kind": "instance", "name": "a"}),
        "繁中說明：from 應由 a 的 token 推導為 instance a，實際 {query}"
    );
    assert_eq!(
        query["body"],
        json!("ping"),
        "繁中說明：relay 的 text 應原樣成為 query body，實際 {query}"
    );

    // `parent_id` 串起一問一答：b 的 report 的 parent 應是它**收到的那則 query 的
    // id**（fixture 填收到的訊息 id；§6.1 的 `parent_id` 由發送者填）。所以方向是
    // `report.parent_id == query.id`。
    assert_eq!(
        report["parent_id"], query["id"],
        "繁中說明：b 的 report 的 parent_id 應指向它收到的 query id，實際 report={report} query={query}"
    );

    // 同時驗證 a 送出的 query 也帶 parent（指向 operator 給它的 relay 訊息 id），
    // 這樣一問一答的兩個方向都有可追溯的 parent。
    assert!(
        !query["parent_id"].is_null(),
        "繁中說明：a 送出的 query 應帶 parent_id，實際 {query}"
    );

    // 最終：所有訊息都被 ack（§6.3 的流程在 agent 端是「drain 後逐則 ack」）。
    let acked = poll_until(EXCHANGE_TIMEOUT, || {
        let log = read_log(&log_a);
        events_named(&log, "acked")
            .iter()
            .filter(|item| item["result"]["outcome"] == json!("completed"))
            .count()
            >= 2
    });
    assert!(
        acked,
        "繁中說明：a 的兩則訊息（relay 與 pong）最終都應被 ack，\
         最後觀測：\n a 的 log：{:#?}",
        read_log(&log_a)
    );
}

/// 建立一個**不讀 stdin** 的 instance（[`non_reader.sh`]）。
fn create_non_reader(daemon: &support::Daemon, home: &support::TestHome, name: &str) {
    let created = daemon.operator_call(
        home,
        "create_instance",
        json!({
            "name": name,
            "command": support::fixture("non_reader.sh").to_string_lossy(),
        }),
    );
    assert_eq!(
        created.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：不讀 stdin 的 instance 應能啟動（測試前提），實際為 {created:?}"
    );
}

/// 一個不讀 stdin 的 agent 收到喚醒提示時，daemon 對**其他**呼叫端仍然可用。
///
/// 這條測試**不是** D-1（阻塞式 PTY 寫入持有全域鎖）的迴歸防護，而且證明不了
/// 那件事：實測在讀端不動時，macOS PTY 的寫端接受 64 MiB 都沒有阻塞，所以「寫入
/// 阻塞 → 鎖死 daemon」沒有可達路徑。`Arc<Session>` 的改動仍值得保留（它讓寫入
/// 不持鎖成為型別保證而非註解紀律），但它防的不是一個會發生的問題。
///
/// 這條測試驗的是另一件事：§6.4 的延後**沒有上限**，所以一個永遠安靜的收件人會讓
/// 重判迴圈無限期地重判。它不該讓 `list_instances`／`pane_snapshot`／attach 對其他
/// 呼叫端失效。
#[test]
fn a_non_reading_agent_does_not_stop_the_daemon_from_serving_other_callers() {
    let home = support::TestHome::new();
    let daemon = support::Daemon::start(&home);
    create_non_reader(&daemon, &home, "mute");

    // 反覆送：單一提示約 70 bytes，量測顯示 PTY 寫端即使完全不阻塞，累積起來也
    // 需要足夠的量才能讓這個情境有代表性；而送多少都不會被「太多」擋下
    // （§6.4 的延後沒有上限）。
    for index in 0..200 {
        let sent = daemon.operator_call(
            &home,
            "send",
            json!({"instance": "mute", "message": format!("填充 {index}")}),
        );
        assert_eq!(
            sent.outcome,
            otterhive::protocol::Outcome::Completed,
            "繁中說明：send 應完成（訊息寫入與喚醒分開，§6.1），實際為 {sent:?}"
        );
    }

    // 關鍵斷言：daemon 仍然能回應。
    let responsive = support::poll_until(support::SHORT_TIMEOUT, || {
        daemon
            .operator_call(&home, "list_instances", json!({}))
            .outcome
            == otterhive::protocol::Outcome::Completed
    });
    assert!(
        responsive,
        "繁中說明：不讀 stdin 的 agent 收到提示時，daemon 仍必須能回應其他請求"
    );

    let snapshot = daemon.operator_call(&home, "pane_snapshot", json!({"instance": "mute"}));
    assert_eq!(
        snapshot.outcome,
        otterhive::protocol::Outcome::Completed,
        "繁中說明：pane_snapshot 應仍能執行，實際為 {snapshot:?}"
    );
}
