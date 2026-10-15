//! T4 Lane A GREEN：daemon 主迴圈（契約 §1–§5、§4.7、§7 daemon 端）。
//!
//! - socket 服務：一行 JSON 的 `Request` → 一行 JSON 的 `OperationResult`；
//!   `attach` 例外（§7），送出後改為 frame 串流。
//! - 單一 home 單一 daemon：第二個以 exit 3 與 stderr 的
//!   `daemon_already_running` 結束（§1）。
//! - 啟動自檢：`otterhive-mcp-bridge` 必須與 `otterhive` 同目錄；SIGCHLD
//!   必須可 wait（否則 §4.3 的「觀測退出而不 reap」前提不成立）。任一自檢
//!   失敗都拒絕啟動。
//! - 時間／容量常數由測試環境變數縮短（`OTTERHIVE_TEST_STARTUP_GRACE_MS`、
//!   `OTTERHIVE_TEST_STOP_TERM_MS`、`OTTERHIVE_TEST_STOP_KILL_MS`、
//!   `OTTERHIVE_TEST_SUBSCRIBER_QUEUE`），未設定時用契約預設
//!   3000／5000／2000／256。
//!
//! `pane_snapshot` 在 daemon 重啟後回空字串、`live: false`、
//! `reason: "no_screen"`（§4.7）；停止後但記憶體中還有畫面時回最後畫面並標
//! `live: false`。

pub mod auth;
pub mod lifecycle;
pub mod recovery;

use crate::procs;

pub use crate::procs::stop_group;

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::admission::admit;
use crate::backend::claude;
use crate::daemon::auth::{Action, Caller, Subject};
use crate::daemon::lifecycle::{InstanceState, SpecError, ValidatedSpec};
use crate::daemon::recovery::RESTART_REASON;
use crate::home::Home;
use crate::mcp;
use crate::procs::{process_start_time, GroupEvidence, StopOutcome};
use crate::protocol::{
    Auth, ErrorCode, OpError, OperationResult, Outcome, Request, Subject as ResultSubject,
};
use crate::store::{InstanceRow, NewInstance, Store, StoreError};
use crate::terminal::frame;
use crate::terminal::session::{Session, SessionConfig, StreamEvent};

/// daemon 的啟動／運行錯誤。
#[derive(Debug)]
pub struct DaemonError {
    /// 是否為「同一 home 已有 daemon」（以 exit 3 + stderr JSON 辨識，§1）。
    pub already_running: bool,
    pub message: String,
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

fn error(message: impl Into<String>) -> DaemonError {
    DaemonError {
        already_running: false,
        message: message.into(),
    }
}

/// 讀測試可調的時間／容量常數（契約預設值；plan「全域約束」）。
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    pub startup_grace: Duration,
    pub stop_term: Duration,
    pub stop_kill: Duration,
    pub subscriber_queue: usize,
}

impl Tuning {
    /// 從環境讀取；未設定時一律使用契約預設值。
    pub fn from_env() -> Self {
        Tuning {
            startup_grace: millis("OTTERHIVE_TEST_STARTUP_GRACE_MS", 3000),
            stop_term: millis("OTTERHIVE_TEST_STOP_TERM_MS", 5000),
            stop_kill: millis("OTTERHIVE_TEST_STOP_KILL_MS", 2000),
            subscriber_queue: std::env::var("OTTERHIVE_TEST_SUBSCRIBER_QUEUE")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(256),
        }
    }
}

fn millis(key: &str, default: u64) -> Duration {
    Duration::from_millis(
        std::env::var(key)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default),
    )
}

/// 一個正在執行的 instance 的記憶體狀態。
struct Running {
    session: Session,
    /// 最近一次 `pane_snapshot` 的畫面；停止後仍可回（標 `live: false`）。
    last_screen: Option<Vec<String>>,
}

/// daemon 的可變狀態（socket 執行緒與 exit observer 共用）。
struct Inner {
    store: Store,
    running: HashMap<String, Running>,
    /// 每個 instance 的進行中 lifecycle 操作（同一 instance 一次一個）。
    busy: HashMap<String, Instant>,
    /// manager 名稱（設定檔 `managers`）。
    managers: Vec<String>,
    /// daemon 重啟後沒有記憶體畫面；此表讓停止後仍可回最後畫面，直到
    /// process 紀錄被清掉為止。
    last_screens: HashMap<String, Vec<String>>,
}

/// 共用的 daemon 狀態。
#[derive(Clone)]
pub struct DaemonState {
    inner: Arc<Mutex<Inner>>,
    home: PathBuf,
    bridge: PathBuf,
    tuning: Tuning,
}

impl DaemonState {
    /// 取得狀態鎖；中毒時取回內容（daemon 不因 panic 中斷而停擺）。
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|guard| guard.into_inner())
    }
}

/// 啟動自檢（契約 §1 的 bridge 需求與 §4.3 的 wait 前提），回 bridge 路徑。
///
/// - bridge：`<otterhive 同目錄>/otterhive-mcp-bridge` 必須存在。
/// - `waitid(WNOWAIT)`：spawn 一個立即結束的子程序，用
///   `waitid(WEXITED|WNOHANG|WNOWAIT)` 取 `si_pid`，再用 `try_wait` 收掉它。
///   §4.3 的全部前提都建立在「能觀測退出而不 reap」上，所以自檢必須走**那
///   條路徑本身**並確認觀測到的就是剛 spawn 的子程序，不能只驗「wait 得到」
///   ——`try_wait` 成功並不能證明 `WNOWAIT` 可用。任何一步不成立都拒絕啟動。
pub fn check_startup(exe: &Path) -> Result<PathBuf, DaemonError> {
    let directory = exe
        .parent()
        .ok_or_else(|| error("無法判定 otterhive binary 的所在目錄"))?;
    let bridge = directory.join("otterhive-mcp-bridge");
    if !bridge.is_file() {
        return Err(error(format!(
            "找不到 otterhive-mcp-bridge（必須與 otterhive 同目錄）：{}",
            bridge.display()
        )));
    }
    let mut probe = std::process::Command::new("/usr/bin/true")
        .spawn()
        .map_err(|spawn_error| error(format!("無法啟動 wait 自檢子程序：{spawn_error}")))?;
    let pid = probe.id() as i32;
    let start = Instant::now();
    loop {
        match procs::waitid_si_pid(pid) {
            // 尚無 exit 事件（si_pid == 0）→ 仍在執行，繼續等。
            Ok(0) if start.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(0) => {
                let _ = probe.kill();
                let _ = probe.wait();
                return Err(error(
                    "SIGCHLD 不可 wait：自檢子程序始終沒有 exit 事件，daemon 拒絕啟動",
                ));
            }
            // 觀測到 exit，且確認是剛 spawn 的那個子程序。
            Ok(observed) if observed == pid => break,
            Ok(observed) => {
                let _ = probe.wait();
                return Err(error(format!(
                    "waitid 觀測到非預期的 si_pid {observed}（自檢子程序是 {pid}），daemon 拒絕啟動"
                )));
            }
            Err(wait_error) => {
                return Err(error(format!(
                    "waitid(WEXITED|WNOHANG|WNOWAIT) 自檢失敗（errno {}）：daemon 拒絕啟動",
                    wait_error
                        .raw_os_error()
                        .map_or_else(|| wait_error.to_string(), |code| code.to_string())
                )));
            }
        }
    }
    match probe.try_wait() {
        Ok(Some(_)) => Ok(bridge),
        Ok(None) => {
            let _ = probe.kill();
            let _ = probe.wait();
            Err(error(
                "SIGCHLD 不可 wait：WNOWAIT 觀測到 exit 後仍無法 waitpid，daemon 拒絕啟動",
            ))
        }
        Err(wait_error) => Err(error(format!("wait 自檢失敗：{wait_error}"))),
    }
}

/// 建立 daemon 狀態：開啟 store、讀設定、做重啟收斂（契約 §4.6）。
pub fn prepare(
    home_root: &Path,
    bridge: &Path,
    tuning: Tuning,
) -> Result<DaemonState, DaemonError> {
    let home = Home::initialize(home_root)
        .map_err(|init_error| error(format!("初始化 home 失敗：{init_error}")))?;
    let config = crate::config::Config::load(home.config_path())
        .map_err(|config_error| error(config_error.to_string()))?;
    let managers = config.managers.clone();

    let state_path = home.root().join("state.sqlite3");
    let mut store = Store::open(&state_path)
        .map_err(|store_error| error(format!("開啟 store 失敗：{store_error}")))?;
    converge_on_start(&mut store, &tuning)?;

    Ok(DaemonState {
        inner: Arc::new(Mutex::new(Inner {
            store,
            running: HashMap::new(),
            busy: HashMap::new(),
            managers,
            last_screens: HashMap::new(),
        })),
        home: home_root.to_path_buf(),
        bridge: bridge.to_path_buf(),
        tuning,
    })
}

/// 契約 §4.6 的重啟收斂：對仍帶程序紀錄的 instance 依 §4.3 停止。
fn converge_on_start(store: &mut Store, tuning: &Tuning) -> Result<(), DaemonError> {
    let rows = store
        .transaction(|tx| tx.all_live_instances())
        .map_err(|store_error| error(format!("列出 instance 失敗：{store_error}")))?;
    for row in rows {
        if row.state == "recovery_required" {
            // 既有的收斂狀態不因重啟而解除。
            continue;
        }
        if !recovery::needs_convergence(&row.state) {
            continue;
        }
        let Some(pid) = row.pid else {
            // 沒有程序紀錄：`failed`／`starting`／`running`／`stopping` 一併
            // 收斂為 `stopped`（PTY 已隨舊 daemon 消失，契約不自動重新啟動）。
            if row.state != "stopped" {
                store
                    .transaction(|tx| {
                        tx.set_lifecycle_state(&row.id, "stopped", Some(RESTART_REASON))
                    })
                    .map_err(|store_error| {
                        error(format!("收斂 {} 的狀態失敗：{store_error}", row.name))
                    })?;
            }
            continue;
        };
        let pid = pid as i32;
        let pid_start = row.pid_start.unwrap_or_default() as u64;
        let stopped = match process_start_time(pid) {
            // 程序已不存在：確認停止成立（RecordedLeader 不 reap；該 child 由
            // init 接手）。
            None => true,
            Some(current) if current == pid_start => matches!(
                stop_group(
                    pid,
                    pid_start,
                    GroupEvidence::RecordedLeader,
                    tuning.stop_term,
                    tuning.stop_kill,
                ),
                StopOutcome::Stopped { .. }
            ),
            // 身分不符：無法確認，不發 signal。
            Some(_) => false,
        };
        if stopped {
            store
                .transaction(|tx| {
                    tx.clear_process(&row.id)?;
                    tx.set_lifecycle_state(&row.id, "stopped", Some(RESTART_REASON))
                })
                .map_err(|store_error| error(format!("收斂 {} 失敗：{store_error}", row.name)))?;
        } else {
            store
                .transaction(|tx| tx.set_lifecycle_state(&row.id, "recovery_required", None))
                .map_err(|store_error| {
                    error(format!(
                        "標記 {} 為 recovery_required 失敗：{store_error}",
                        row.name
                    ))
                })?;
        }
    }
    Ok(())
}

/// 執行 daemon：取得獨佔鎖、bind socket、處理請求，直到 SIGTERM／SIGINT。
///
/// 鎖的取得順序刻意排在 `Home::initialize` **之前**。initialize 會建立目錄並
/// （在需要時）寫出 operator token；它是一次有外部可見副作用的操作，必須
/// 由「確定自己拿得到這個 home」的那一方來做。順序顛倒會讓兩個同時啟動的
/// daemon 都先寫完 home，第二個才在鎖上被擋下來。
pub fn run(home_root: &Path, tuning: Tuning) -> Result<(), DaemonError> {
    let run_dir = home_root.join("run");
    let _lock =
        crate::home::DaemonLock::acquire(&run_dir).map_err(|lock_error| match lock_error {
            crate::home::LockError::AlreadyHeld => DaemonError {
                already_running: true,
                message: String::from("同一 home 只允許一個 daemon"),
            },
            other => error(format!("取得 daemon 鎖失敗：{other}")),
        })?;

    let home = Home::initialize(home_root)
        .map_err(|init_error| error(format!("初始化 home 失敗：{init_error}")))?;
    let bridge = check_startup(&current_exe()?)?;
    let state = prepare(home_root, &bridge, tuning)?;

    let socket_path = home.run_dir().join("daemon.sock");
    // 清掉上次殘留的 socket 再 bind（鎖已確保沒有其他 daemon 在跑）。
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }
    let listener = UnixListener::bind(&socket_path).map_err(|bind_error| {
        error(format!("綁定 {} 失敗：{bind_error}", socket_path.display()))
    })?;
    // non-blocking accept 讓訊號旗標可被讀取（否則 accept 會一直睡住）。
    listener
        .set_nonblocking(true)
        .map_err(|mode_error| error(format!("設定 socket 為 non-blocking 失敗：{mode_error}")))?;

    install_signal_handlers();
    loop {
        if shutdown_requested() {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let state = state.clone();
                // 每個連線一個執行緒；attach 的長連線在自己的執行緒裡跑到結束。
                std::thread::spawn(move || {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handle_connection(&state, stream);
                    }));
                });
            }
            Err(accept_error) if accept_error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
    Ok(())
}

fn current_exe() -> Result<PathBuf, DaemonError> {
    std::env::current_exe().map_err(|exe_error| error(format!("無法取得執行檔路徑：{exe_error}")))
}

/// SIGTERM／SIGINT 使 accept 迴圈結束；SIGPIPE 忽略（斷線不得殺 daemon）。
///
/// 訊號處理器只做 atomic store（async-signal-safe）；socket 的 accept 迴圈
/// 以 non-blocking 輪詢讀取旗標，不在訊號上下文做任何 I/O。
fn install_signal_handlers() {
    // clippy::function_casts_as_integer：先轉成 `*const ()` 再轉成
    // `sighandler_t`（`libc::sighandler_t` 是 usize 寬的整數型別）。
    let handler = handle_terminate as *const () as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

/// 收到終止訊號的旗標。
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_terminate(_signal: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// 處理一個連線：一行 JSON 請求 → 一行 JSON 回應（`attach` 走 frame 串流）。
fn handle_connection(state: &DaemonState, stream: UnixStream) {
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut writer = write_half;
    let mut reader = BufReader::new(stream);

    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let request: Request = match serde_json::from_str(line.trim()) {
        Ok(request) => request,
        Err(parse_error) => {
            let result = OperationResult::refused(
                "",
                ErrorCode::InvalidArgument,
                &format!("請求不是合法的 JSON Lines 格式：{parse_error}"),
            );
            let _ = write_result(&mut writer, &result);
            return;
        }
    };
    if request.v != 1 {
        let result = OperationResult::refused(
            &request.operation_id.clone().unwrap_or_default(),
            ErrorCode::InvalidArgument,
            &format!("不支援的協定版本 {}（應為 1）", request.v),
        );
        let _ = write_result(&mut writer, &result);
        return;
    }

    if request.op == "attach" {
        handle_attach(state, &request, &mut reader, writer);
        return;
    }
    // §4.4：被 restart 的是呼叫者自己時，daemon 先回 `queued` 再執行——呼叫端
    // 會在讀到結果之前就被自己的舊程序連同 PTY 一起收掉，等結果等於等一個
    // 永遠不會到的回應。判定在認證之後（要知道呼叫者是誰）、
    // admission／去重之前（`queued` 本身就要走同一條回應路徑）。
    let self_restart = matches!(request.op.as_str(), "restart_instance" | "delete_instance")
        && targets_the_caller(state, &request)
        && !operation_is_recorded(state, &request);
    if self_restart {
        let operation_id = request
            .operation_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let subject = caller_subject(state, &request.auth);
        let _ = write_result(
            &mut writer,
            &OperationResult {
                operation_id,
                outcome: Outcome::Queued,
                subject,
                value: json!({"reason": "self_restart"}),
                effects: Vec::new(),
                error: None,
            },
        );
    }
    let result = dispatch(state, &request);
    if self_restart {
        // 真正的工作在背景執行：self-restart 的呼叫端隨時會死掉，絕不能把
        // 它的回應掛在 socket 上等。是否成功由新程序以 `list_instances` 確認
        // （§4.4）。
        let state = state.clone();
        std::thread::spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                dispatch(&state, &request);
            }));
        });
        return;
    }
    let _ = write_result(&mut writer, &result);
}

/// 請求的目標 instance 就是認證後的呼叫者本人（§2 的「自己」）。
fn targets_the_caller(state: &DaemonState, request: &Request) -> bool {
    let Ok(caller) = authenticate(state, &request.auth) else {
        return false;
    };
    let Ok(instance) = require_target(request) else {
        return false;
    };
    caller
        .instance_id()
        .is_some_and(|id| instance == caller_name_of(state, id))
}

/// 該 `operation_id` 是不是已被記錄的（不論是哪個呼叫端）。
///
/// §3 的跨 caller 去重必須在 `queued` 之前生效：重用別人用過的
/// operation_id 必須回 `invalid_argument`，不能因為目標是呼叫者自己就先回
/// `queued`——那會把「這次呼叫根本無效」講成「已排程」。
fn operation_is_recorded(state: &DaemonState, request: &Request) -> bool {
    let Some(operation_id) = request.operation_id.as_ref() else {
        return false;
    };
    state
        .lock()
        .store
        .recorded_operation(operation_id)
        .unwrap_or(None)
        .is_some()
}

/// `args.instance` 的 instance 名稱（沒有或不是字串回 `Err`）。
fn require_target(request: &Request) -> Result<String, ()> {
    request
        .args
        .get("instance")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(())
}

fn caller_name_of(state: &DaemonState, instance_id: &str) -> String {
    state
        .lock()
        .store
        .instance(instance_id)
        .unwrap_or(None)
        .map(|row| row.name)
        .unwrap_or_default()
}

/// 認證並取回呼叫端的 subject（`queued` 回應要帶 `subject`；認證失敗回
/// `None`，真正的錯誤由後續 `dispatch` 回報）。
fn caller_subject(state: &DaemonState, request_auth: &Auth) -> Option<ResultSubject> {
    let caller = authenticate(state, request_auth).ok()?;
    let id = caller.instance_id()?;
    let row = state.lock().store.instance(id).unwrap_or(None)?;
    Some(subject_of(&row))
}

fn write_result(writer: &mut UnixStream, result: &OperationResult) -> std::io::Result<()> {
    let mut line = serde_json::to_string(result).map_err(|encode_error| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, encode_error)
    })?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()
}

/// 依憑證判定呼叫端身分（契約 §2）。
fn authenticate(state: &DaemonState, auth: &Auth) -> Result<Caller, (ErrorCode, String)> {
    let token = match auth {
        Auth::Operator(token) | Auth::Instance(token) => token.as_str(),
    };
    let operator =
        std::fs::read_to_string(state.home.join("secrets/operator.token")).unwrap_or_default();
    if !operator.trim().is_empty() && token == operator.trim() {
        return Ok(Caller::Operator);
    }
    let hash = auth::token_hash(token);
    let matched = state
        .lock()
        .store
        .transaction(|tx| tx.live_instance_by_token_hash(&hash))
        .ok()
        .flatten();
    match matched {
        Some(row) => Ok(Caller::Instance(row.id)),
        None => Err((
            ErrorCode::Unauthorized,
            String::from("憑證無效（operator token 或該 generation 的 instance token）"),
        )),
    }
}

/// 處理一個操作請求：admission → 認證 → operation_id 去重 → 行為 → 記錄。
fn dispatch(state: &DaemonState, request: &Request) -> OperationResult {
    let operation_id = request
        .operation_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // 契約 §3：形狀檢查在任何變更之前；未支援／未知欄位在此拒絕。
    // `attach`／`tools_list` 是只經 socket 的兩個操作（§1），不在 MCP 工具
    // 清單裡，因此不經過形狀檢查——它們的授權由 §2 與 §7／§10 處理。
    if !matches!(request.op.as_str(), "attach" | "tools_list") {
        if let Err(op_error) = admit(&request.op, &request.args) {
            return OperationResult {
                operation_id: operation_id.clone(),
                outcome: match op_error.code {
                    ErrorCode::UnsupportedInPhase => Outcome::Unsupported,
                    _ => Outcome::Refused,
                },
                subject: None,
                value: Value::Null,
                effects: Vec::new(),
                error: Some(op_error),
            };
        }
    }

    let caller = match authenticate(state, &request.auth) {
        Ok(caller) => caller,
        Err((code, message)) => return OperationResult::refused(&operation_id, code, &message),
    };

    // 契約 §3：同一 caller 的同一 operation_id 重送回記錄結果；跨 caller
    // 使用同一 id 回 invalid_argument。
    let caller_key = caller.record_key();
    if let Some(recorded) = state
        .lock()
        .store
        .recorded_operation(&operation_id)
        .unwrap_or(None)
    {
        return if recorded.caller == caller_key {
            recorded.result
        } else {
            OperationResult::refused(
                &operation_id,
                ErrorCode::InvalidArgument,
                "此 operation_id 已由另一個呼叫端使用",
            )
        };
    }

    let result = perform(state, &operation_id, &caller, &request.op, &request.args);

    let recorded = state
        .lock()
        .store
        .transaction(|tx| tx.record_operation(&operation_id, &request.op, &caller_key, &result));
    match recorded {
        Ok(()) => result,
        Err(StoreError::DuplicateOperation(_)) => state
            .lock()
            .store
            .recorded_operation(&operation_id)
            .unwrap_or(None)
            .map(|record| record.result)
            .unwrap_or(result),
        Err(store_error) => OperationResult::refused(
            &operation_id,
            ErrorCode::Internal,
            &format!("記錄 operation 失敗：{store_error}"),
        ),
    }
}

/// 依操作分派到具體行為。
fn perform(
    state: &DaemonState,
    operation_id: &str,
    caller: &Caller,
    op: &str,
    args: &Value,
) -> OperationResult {
    match op {
        "create_instance" => create_instance(state, operation_id, caller, args),
        "start_instance" => start_instance(state, operation_id, caller, args),
        "delete_instance" => delete_instance(state, operation_id, caller, args),
        "restart_instance" => restart_instance(state, operation_id, caller, args),
        "list_instances" => list_instances(state, operation_id, args),
        "interrupt" => interrupt_instance(state, operation_id, caller, args),
        "pane_snapshot" => pane_snapshot(state, operation_id, caller, args),
        "tools_list" => tools_list(state, operation_id, caller),
        other => OperationResult {
            operation_id: operation_id.to_owned(),
            outcome: Outcome::Unsupported,
            subject: None,
            value: Value::Null,
            effects: Vec::new(),
            error: Some(OpError {
                code: ErrorCode::UnsupportedInPhase,
                message: format!("操作 {other} 在 P1a 不支援（尚未實作）"),
                retryable: false,
                next: None,
            }),
        },
    }
}

fn target_name(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn subject_of(row: &InstanceRow) -> ResultSubject {
    ResultSubject {
        kind: String::from("instance"),
        id: row.id.clone(),
        name: row.name.clone(),
        generation: row.generation,
    }
}

/// 組出被操作對象的資訊（全部來自 store／設定，不來自請求；§2）。
fn subject_for(state: &DaemonState, caller: &Caller, target: Option<&str>) -> Subject {
    let mut inner = state.lock();
    let caller_row = caller
        .instance_id()
        .and_then(|id| inner.store.instance(id).unwrap_or(None));
    let caller_name = caller_row.as_ref().map(|row| row.name.clone());
    let caller_is_manager = caller_name
        .as_ref()
        .is_some_and(|name| inner.managers.iter().any(|item| item == name));
    let team = target.and_then(|name| {
        inner
            .store
            .transaction(|tx| tx.team_of_instance_name(name))
            .unwrap_or(None)
    });
    Subject {
        instance_name: target.map(str::to_owned),
        team,
        caller_name,
        caller_is_manager,
    }
}

/// 契約 §2 的授權檢查；不通過回 `unauthorized`。
fn require_permission(
    state: &DaemonState,
    operation_id: &str,
    caller: &Caller,
    action: Action,
    target: Option<&str>,
) -> Option<OperationResult> {
    let subject = subject_for(state, caller, target);
    if auth::authorize(caller, action, &subject) == auth::Permission::Allow {
        None
    } else {
        Some(OperationResult::refused(
            operation_id,
            ErrorCode::Unauthorized,
            "此身分不可執行該操作（契約 §2）",
        ))
    }
}

/// 佔住 instance 的 lifecycle 鎖（同一 instance 一次一個）。
///
/// 逾期（`BUSY_TIMEOUT`）的鎖視為失效——否則 daemon 崩潰後操作會永久被鎖死。
/// 但**失效的鎖絕不自動放行**：那個持有者可能只是還在跑（只是比預期久）。
/// 自動放行等於讓兩個 lifecycle 操作同時作用在同一個 instance 上，而契約
/// §4.3 的證據式前提（同一把 lifecycle 鎖內的驗證／signal／reap）正是靠這
/// 把鎖成立的。逾時因此回 `internal`，把它變成需要人看的狀態，而不是被
/// daemon 自己悄悄蓋掉。
const BUSY_TIMEOUT: Duration = Duration::from_secs(120);

fn acquire_busy(
    state: &DaemonState,
    operation_id: &str,
    name: &str,
) -> Result<(), Box<OperationResult>> {
    let mut inner = state.lock();
    if let Some(since) = inner.busy.get(name) {
        if since.elapsed() < BUSY_TIMEOUT {
            return Err(Box::new(OperationResult::refused(
                operation_id,
                ErrorCode::OperationInProgress,
                "此 instance 已有進行中的生命週期操作",
            )));
        }
        // 逾期：撤掉這把殘留的鎖（它已經沒有合法的持有者），但本次操作
        // 仍然拒絕——見 `BUSY_TIMEOUT` 的說明。
        inner.busy.remove(name);
        return Err(Box::new(OperationResult::refused(
            operation_id,
            ErrorCode::Internal,
            &format!(
                "此 instance 的生命週期鎖已逾期（{} 秒）仍被持有，daemon 已撤掉殘留鎖；\
                 請確認沒有殘留程序後重試",
                BUSY_TIMEOUT.as_secs()
            ),
        )));
    }
    inner.busy.insert(name.to_owned(), Instant::now());
    Ok(())
}

fn release_busy(state: &DaemonState, name: &str) {
    state.lock().busy.remove(name);
}

/// 由 args 取出 instance 名稱或回 `invalid_argument`。
fn require_name(operation_id: &str, args: &Value) -> Result<String, Box<OperationResult>> {
    target_name(args, "instance").ok_or_else(|| {
        Box::new(OperationResult::refused(
            operation_id,
            ErrorCode::InvalidArgument,
            "instance 必填",
        ))
    })
}

/// `create_instance`：驗證規格 → 建定義 → 依 §4.2 start。
fn create_instance(
    state: &DaemonState,
    operation_id: &str,
    caller: &Caller,
    args: &Value,
) -> OperationResult {
    if let Some(denied) = require_permission(state, operation_id, caller, Action::Manage, None) {
        return denied;
    }
    let spec = match lifecycle::validate_spec(args, &state.home) {
        Ok(spec) => spec,
        Err(SpecError::InvalidArgument(message)) => {
            return OperationResult::refused(operation_id, ErrorCode::InvalidArgument, &message);
        }
        Err(SpecError::UnsupportedInPhase { backend, message }) => {
            return OperationResult {
                operation_id: operation_id.to_owned(),
                outcome: Outcome::Unsupported,
                subject: None,
                value: json!({ "backend": backend }),
                effects: Vec::new(),
                error: Some(OpError {
                    code: ErrorCode::UnsupportedInPhase,
                    message,
                    retryable: false,
                    next: None,
                }),
            };
        }
    };
    // 名稱唯一性由資料庫的 live-name 唯一索引決定（契約 §2：同名回
    // `name_conflict`），因此**不**先佔 lifecycle 鎖——否則兩個並行 create
    // 會競爭同一把鎖，第二個拿到 `operation_in_progress`，而契約對「同名」要
    // 的是 `name_conflict`。鎖只保護已存在的 instance 的生命週期操作。
    create_and_start(state, operation_id, &spec)
}

fn create_and_start(
    state: &DaemonState,
    operation_id: &str,
    spec: &ValidatedSpec,
) -> OperationResult {
    let row = {
        let mut inner = state.lock();
        let spec_json = lifecycle::spec_json(spec);
        match inner.store.transaction(|tx| {
            tx.insert_instance(&NewInstance {
                name: spec.name.clone(),
                spec: spec_json,
            })
        }) {
            Ok(row) => row,
            Err(StoreError::NameConflict(name)) => {
                return OperationResult::refused(
                    operation_id,
                    ErrorCode::NameConflict,
                    &format!("instance 名稱 {name} 已被使用"),
                );
            }
            Err(store_error) => {
                return OperationResult::refused(
                    operation_id,
                    ErrorCode::Internal,
                    &format!("建立 instance 失敗：{store_error}"),
                );
            }
        }
    };
    let mut effects = vec![
        String::from("instance_created"),
        String::from("generation_advanced"),
    ];
    match spawn_instance(state, &row.id, spec, false) {
        Ok(started) => {
            effects.push(String::from("process_started"));
            effects.extend(started.extra_effects);
            OperationResult {
                operation_id: operation_id.to_owned(),
                outcome: started.outcome,
                subject: started.subject,
                value: started.value,
                effects,
                error: started.error,
            }
        }
        Err(failed) => {
            let failed = *failed;
            effects.extend(failed.effects);
            OperationResult {
                operation_id: operation_id.to_owned(),
                outcome: Outcome::Refused,
                subject: failed.subject,
                value: failed.value,
                effects,
                error: Some(OpError {
                    code: ErrorCode::SpawnFailed,
                    message: failed.message,
                    retryable: false,
                    next: None,
                }),
            }
        }
    }
}

/// spawn 成功。
struct SpawnSuccess {
    outcome: Outcome,
    subject: Option<ResultSubject>,
    value: Value,
    extra_effects: Vec<String>,
    error: Option<OpError>,
}

/// spawn 失敗（保留定義，標 `failed`／`recovery_required`）。
struct SpawnFailure {
    subject: Option<ResultSubject>,
    value: Value,
    effects: Vec<String>,
    message: String,
}

/// 依 §4.2 spawn：物化 §5.1 → 記錄 generation／token → PTY spawn → 寬限期 → 狀態。
fn spawn_instance(
    state: &DaemonState,
    instance_id: &str,
    spec: &ValidatedSpec,
    resume: bool,
) -> Result<SpawnSuccess, Box<SpawnFailure>> {
    // 契約 §4.1：未給 working_directory 時建立；明確給的不建立。
    if !spec.explicit_working_directory {
        if let Err(create_error) = std::fs::create_dir_all(&spec.working_directory) {
            return Err(Box::new(mark_spawn_failed(
                state,
                instance_id,
                "exec_failed",
                vec![String::from("state_failed")],
                json!({"reason": "exec_failed"}),
                format!("無法建立 working_directory：{create_error}"),
            )));
        }
    }

    // §4.2-2：generation 加一、發新 token。
    let token = auth::new_instance_token().map_err(|token_error| {
        Box::new(SpawnFailure {
            subject: None,
            value: json!({"reason": "exec_failed"}),
            effects: vec![String::from("state_failed")],
            message: format!("無法產生 instance token：{token_error}"),
        })
    })?;
    let token_digest = auth::token_hash(&token);
    let generation = {
        let mut inner = state.lock();
        match inner
            .store
            .transaction(|tx| tx.bump_generation(instance_id, None))
        {
            Ok(generation) => generation,
            Err(store_error) => {
                return Err(Box::new(SpawnFailure {
                    subject: None,
                    value: json!({"reason": "exec_failed"}),
                    effects: vec![String::from("state_failed")],
                    message: format!("增加 generation 失敗：{store_error}"),
                }));
            }
        }
    };

    // §5.1：物化協調提示、MCP 設定、skills symlink。
    let mut warnings: Vec<String> = Vec::new();
    let _ = std::fs::create_dir_all(spec.working_directory.join(".claude"));
    let prompt = claude::coordination_prompt(
        &spec.name,
        None,
        &[
            "list_instances",
            "restart_instance",
            "pane_snapshot",
            "send",
            "inbox",
            "team",
        ],
        spec.instructions_text.as_deref(),
    );
    let prompt_path = match claude::write_prompt(&spec.working_directory, &prompt) {
        Ok(path) => path,
        Err(write_error) => {
            return Err(Box::new(mark_spawn_failed(
                state,
                instance_id,
                "exec_failed",
                vec![String::from("state_failed")],
                json!({"reason": "exec_failed"}),
                format!("寫入協調提示失敗：{write_error}"),
            )));
        }
    };
    let mcp_value = claude::mcp_config_value(&state.home, &state.bridge, &token);
    let mcp_path = match claude::write_mcp_config(&spec.working_directory, &mcp_value) {
        Ok(path) => path,
        Err(write_error) => {
            return Err(Box::new(mark_spawn_failed(
                state,
                instance_id,
                "exec_failed",
                vec![String::from("state_failed")],
                json!({"reason": "exec_failed"}),
                format!("寫入 MCP 設定失敗：{write_error}"),
            )));
        }
    };
    if let Some(skills) = &spec.skills_path {
        match claude::create_skills_link(&spec.working_directory, skills, None) {
            Ok(()) => {
                let detail = json!({
                    "instance_id": instance_id,
                    "link": spec.working_directory.join(".claude/skills").to_string_lossy(),
                    "target": skills.to_string_lossy(),
                })
                .to_string();
                let mut inner = state.lock();
                let _ = inner.store.transaction(|tx| {
                    tx.append_public_journal(
                        instance_id,
                        generation,
                        "skills_link_created",
                        &detail,
                    )
                });
            }
            Err(conflict) => warnings.push(conflict.to_string()),
        }
    }

    // §4.2-3：在 PTY 中 spawn。
    let executable = spec
        .command
        .clone()
        .unwrap_or_else(|| String::from("claude"));
    let argv = claude::build_argv(
        &executable,
        spec.model.as_deref(),
        spec.effort.as_deref(),
        &prompt_path,
        &mcp_path,
        &spec.args,
        resume,
    );
    let base_env: BTreeMap<String, String> = std::env::vars().collect();
    let spec_env: BTreeMap<String, String> = spec.env.iter().cloned().collect();
    let child_env = claude::child_environment(
        &base_env,
        &spec_env,
        &state.home,
        &spec.name,
        &token,
        &state.bridge,
    );
    let config = SessionConfig {
        program: executable.clone(),
        args: argv.iter().skip(1).cloned().collect(),
        cwd: spec.working_directory.clone(),
        env: child_env.into_iter().collect(),
        size: (120, 40),
        generation,
        subscriber_queue: state.tuning.subscriber_queue,
    };
    let session = match Session::spawn(config) {
        Ok(session) => session,
        Err(_) => {
            return Err(Box::new(mark_spawn_failed(
                state,
                instance_id,
                "exec_failed",
                vec![String::from("state_failed")],
                json!({"reason": "exec_failed"}),
                format!("無法執行 {executable}"),
            )));
        }
    };
    let pid = session.pid();
    let pid_start = session.start_time();

    {
        let mut inner = state.lock();
        if let Err(store_error) = inner.store.transaction(|tx| {
            tx.record_process(instance_id, pid, pid_start, &token_digest)?;
            tx.set_lifecycle_state(instance_id, "starting", None)
        }) {
            let outcome = stop_group(
                pid,
                pid_start,
                GroupEvidence::OwnedLeader(session.owned_leader_lease()),
                state.tuning.stop_term,
                state.tuning.stop_kill,
            );
            if matches!(outcome, StopOutcome::Stopped { .. }) {
                session.mark_child_released();
            }
            return Err(Box::new(SpawnFailure {
                subject: None,
                value: json!({"reason": "exec_failed"}),
                effects: vec![String::from("state_failed")],
                message: format!("記錄程序紀錄失敗：{store_error}"),
            }));
        }
    }

    // §4.2-5：回應前等寬限期結束或子程序結束（先發生者為準）。
    let deadline = Instant::now() + state.tuning.startup_grace;
    let mut exited = false;
    while Instant::now() < deadline {
        if session.exit_status().is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    if exited {
        // 寬限期內 leader 結束：先依 §4.3 清理同一 group 的殘留成員。
        let exit = session.exit_status();
        let outcome = stop_group(
            pid,
            pid_start,
            GroupEvidence::OwnedLeader(session.owned_leader_lease()),
            state.tuning.stop_term,
            state.tuning.stop_kill,
        );
        let mut effects = vec![String::from("process_started")];
        let (state_name, value) = match outcome {
            StopOutcome::Stopped { .. } => {
                // 停止已確認：Child 已釋放（與 stop 同一 lifecycle 鎖區塊）。
                session.mark_child_released();
                effects.push(String::from("process_stopped"));
                effects.push(String::from("state_failed"));
                (
                    "failed",
                    json!({
                        "reason": "exited_during_startup",
                        "exit_code": exit.and_then(|status| status.code),
                    }),
                )
            }
            StopOutcome::Unproven { residual_pids } => {
                effects.push(String::from("state_failed"));
                (
                    "recovery_required",
                    json!({
                        "reason": "exited_during_startup",
                        "exit_code": exit.and_then(|status| status.code),
                        "residual_pids": residual_pids,
                    }),
                )
            }
            StopOutcome::IdentityMismatch => {
                effects.push(String::from("state_failed"));
                (
                    "recovery_required",
                    json!({"reason": "exited_during_startup", "residual_pids": []}),
                )
            }
        };
        let mut inner = state.lock();
        let subject = inner
            .store
            .instance(instance_id)
            .unwrap_or(None)
            .map(|row| subject_of(&row));
        let _ = inner.store.transaction(|tx| {
            if state_name == "failed" {
                tx.clear_process(instance_id)?;
            }
            tx.set_lifecycle_state(instance_id, state_name, Some("exited_during_startup"))
        });
        return Err(Box::new(SpawnFailure {
            subject,
            value,
            effects,
            message: String::from("啟動寬限期內 leader 結束"),
        }));
    }

    // 仍在執行 → running（§4.2-5）。
    let row = {
        let mut inner = state.lock();
        let recorded = inner
            .store
            .transaction(|tx| tx.set_lifecycle_state(instance_id, "running", None));
        let row = inner.store.instance(instance_id).unwrap_or(None);
        inner.running.insert(
            spec.name.clone(),
            Running {
                session,
                last_screen: None,
            },
        );
        let _ = recorded;
        row
    };
    spawn_exit_observer(
        state.clone(),
        instance_id.to_owned(),
        spec.name.clone(),
        pid,
        pid_start,
    );

    let mut value = json!({"pid": pid});
    if !warnings.is_empty() {
        value["warnings"] = json!(warnings);
    }
    Ok(SpawnSuccess {
        outcome: Outcome::Completed,
        subject: row.as_ref().map(subject_of),
        value,
        extra_effects: Vec::new(),
        error: None,
    })
}

/// 標記 spawn 失敗並回 `SpawnFailure`。
fn mark_spawn_failed(
    state: &DaemonState,
    instance_id: &str,
    reason: &str,
    effects: Vec<String>,
    value: Value,
    message: String,
) -> SpawnFailure {
    let mut inner = state.lock();
    let _ = inner.store.transaction(|tx| {
        tx.clear_process(instance_id)?;
        tx.set_lifecycle_state(instance_id, "failed", Some(reason))
    });
    let subject = inner
        .store
        .instance(instance_id)
        .unwrap_or(None)
        .map(|row| subject_of(&row));
    SpawnFailure {
        subject,
        value,
        effects,
        message,
    }
}

/// 寬限期後的 exit observer：leader 結束時依 §4.3 清理 group（契約 §4.2-6）。
fn spawn_exit_observer(
    state: DaemonState,
    instance_id: String,
    name: String,
    pid: i32,
    pid_start: u64,
) {
    std::thread::spawn(move || {
        // 最多等 10 分鐘（daemon 的 instance 不會無限期存活）；超時後退出，
        // 不無限 join。
        let deadline = Instant::now() + Duration::from_secs(600);
        while Instant::now() < deadline {
            let alive = {
                let inner = state.lock();
                match inner.running.get(&name) {
                    Some(running) => running.session.exit_status().is_none(),
                    None => false,
                }
            };
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let session = {
            let mut inner = state.lock();
            let removed = inner.running.remove(&name);
            if let Some(running) = &removed {
                // 停止後仍可回最後畫面（§4.7 的 `live: false`）。
                inner
                    .last_screens
                    .insert(name.clone(), running.session.plain_text());
            }
            removed.map(|running| running.session)
        };
        let Some(session) = session else {
            return;
        };
        if session.exit_status().is_none() {
            // 逾時仍未退出：保留 Session，不做未證實的清理。
            return;
        }
        let outcome = stop_group(
            pid,
            pid_start,
            GroupEvidence::OwnedLeader(session.owned_leader_lease()),
            state.tuning.stop_term,
            state.tuning.stop_kill,
        );
        let mut inner = state.lock();
        match outcome {
            StopOutcome::Stopped { .. } => {
                // 停止已確認：Child 已釋放（與 stop 同一 lifecycle 鎖區塊）。
                session.mark_child_released();
                // 決策條件 5：drop 後若 join 逾時 detach 了執行緒，這次停止並未
                // 收斂，不得記成 `stopped`。
                let detached_before = crate::terminal::session::detached_threads();
                drop(session);
                if crate::terminal::session::detached_threads() > detached_before {
                    let _ = inner.store.transaction(|tx| {
                        tx.set_lifecycle_state(&instance_id, "recovery_required", None)
                    });
                } else {
                    let _ = inner.store.transaction(|tx| {
                        tx.clear_process(&instance_id)?;
                        tx.set_lifecycle_state(&instance_id, "stopped", Some("exited"))
                    });
                }
            }
            _ => {
                drop(session);
                let _ = inner.store.transaction(|tx| {
                    tx.set_lifecycle_state(&instance_id, "recovery_required", None)
                });
            }
        }
    });
}

/// `start_instance`：依 §4.2 對 stopped／failed 的 instance 啟動。
fn start_instance(
    state: &DaemonState,
    operation_id: &str,
    caller: &Caller,
    args: &Value,
) -> OperationResult {
    let name = match require_name(operation_id, args) {
        Ok(name) => name,
        Err(result) => return *result,
    };
    if let Some(denied) =
        require_permission(state, operation_id, caller, Action::Manage, Some(&name))
    {
        return denied;
    }
    if let Err(busy) = acquire_busy(state, operation_id, &name) {
        return *busy;
    }
    let outcome = start_existing(state, operation_id, &name);
    release_busy(state, &name);
    outcome
}

fn start_existing(state: &DaemonState, operation_id: &str, name: &str) -> OperationResult {
    let row = match lookup(state, operation_id, name) {
        Ok(row) => row,
        Err(result) => return *result,
    };
    match InstanceState::from_state(&row.state) {
        InstanceState::Running => {
            return OperationResult {
                operation_id: operation_id.to_owned(),
                outcome: Outcome::Completed,
                subject: Some(subject_of(&row)),
                value: json!({"already_running": true}),
                effects: Vec::new(),
                error: None,
            };
        }
        InstanceState::Starting | InstanceState::Stopping => {
            return OperationResult::refused(
                operation_id,
                ErrorCode::OperationInProgress,
                "此 instance 正在轉換狀態",
            );
        }
        InstanceState::RecoveryRequired => {
            return OperationResult::refused(
                operation_id,
                ErrorCode::RecoveryRequired,
                "此 instance 需人工確認殘留程序後才能重啟",
            );
        }
        // 不在契約列舉內的狀態：不 spawn、不發任何 signal，直接標
        // `recovery_required`。猜成 `stopped` 會在沒有確認任何事的情況下改變
        // 一個身分不明的 instance 的處境；猜成可 start 同理。
        InstanceState::Unknown => {
            let mut inner = state.lock();
            let _ = inner
                .store
                .transaction(|tx| tx.set_lifecycle_state(&row.id, "recovery_required", None));
            return OperationResult::refused(
                operation_id,
                ErrorCode::RecoveryRequired,
                &format!(
                    "此 instance 的狀態 {:?} 不在契約列舉內，已標為 recovery_required；\
                     不 spawn、不發 signal",
                    row.state
                ),
            );
        }
        InstanceState::Stopped | InstanceState::Failed => {}
    }

    // §4.2-1：仍有程序紀錄時先確認 group 已空。
    let mut effects = Vec::new();
    if row.pid.is_some() {
        match stop_previous(state, &row) {
            PreviousOutcome::Stopped => effects.push(String::from("process_stopped")),
            PreviousOutcome::Gone => {}
            PreviousOutcome::Unproven { residual_pids } => {
                return recovery_required(
                    state,
                    &row,
                    operation_id,
                    &residual_pids,
                    "無法確認前一代程序已停止，不啟動新程序",
                );
            }
        }
    }
    let spec = match spec_from_row(state, operation_id, &row) {
        Ok(spec) => spec,
        Err(result) => return *result,
    };
    finish_spawn(state, operation_id, &row, &spec, effects, false)
}

/// 依操作查 instance；不存在回 `not_found`。
fn lookup(
    state: &DaemonState,
    operation_id: &str,
    name: &str,
) -> Result<InstanceRow, Box<OperationResult>> {
    let result = state.lock().store.live_instance_by_name(name);
    match result {
        Ok(Some(row)) => Ok(row),
        Ok(None) => Err(Box::new(OperationResult::refused(
            operation_id,
            ErrorCode::NotFound,
            &format!("instance {name} 不存在"),
        ))),
        Err(store_error) => Err(Box::new(OperationResult::refused(
            operation_id,
            ErrorCode::Internal,
            &format!("查詢 instance 失敗：{store_error}"),
        ))),
    }
}

/// 依既有列重建規格並 spawn，統一組出結果。
fn finish_spawn(
    state: &DaemonState,
    operation_id: &str,
    row: &InstanceRow,
    spec: &ValidatedSpec,
    mut effects: Vec<String>,
    resume: bool,
) -> OperationResult {
    effects.push(String::from("generation_advanced"));
    match spawn_instance(state, &row.id, spec, resume) {
        Ok(started) => {
            effects.push(String::from("process_started"));
            effects.extend(started.extra_effects);
            OperationResult {
                operation_id: operation_id.to_owned(),
                outcome: started.outcome,
                subject: started.subject.or_else(|| Some(subject_of(row))),
                value: started.value,
                effects,
                error: started.error,
            }
        }
        Err(failed) => {
            let failed = *failed;
            effects.extend(failed.effects);
            OperationResult {
                operation_id: operation_id.to_owned(),
                outcome: Outcome::Refused,
                subject: failed.subject.or_else(|| Some(subject_of(row))),
                value: failed.value,
                effects,
                error: Some(OpError {
                    code: ErrorCode::SpawnFailed,
                    message: failed.message,
                    retryable: false,
                    next: None,
                }),
            }
        }
    }
}

enum PreviousOutcome {
    Stopped,
    Gone,
    /// 無法確認停止（殘留 pid 一併回報，契約 §4.3-3）。
    Unproven {
        residual_pids: Vec<i32>,
    },
}

/// 依 §4.3 停止前一 generation 的程序。
fn stop_previous(state: &DaemonState, row: &InstanceRow) -> PreviousOutcome {
    let pid = row.pid.unwrap_or_default() as i32;
    let pid_start = row.pid_start.unwrap_or_default() as u64;
    // 記憶體中持有（本 daemon 的 owned lease）：走 owned 路徑。
    let session = {
        let mut inner = state.lock();
        inner
            .running
            .remove(&row.name)
            .map(|running| running.session)
    };
    if let Some(session) = session {
        let outcome = stop_group(
            pid,
            pid_start,
            GroupEvidence::OwnedLeader(session.owned_leader_lease()),
            state.tuning.stop_term,
            state.tuning.stop_kill,
        );
        let residual_pids = match &outcome {
            StopOutcome::Unproven { residual_pids } => residual_pids.clone(),
            StopOutcome::Stopped { .. } | StopOutcome::IdentityMismatch => Vec::new(),
        };
        if matches!(outcome, StopOutcome::Stopped { .. }) {
            // 停止成功：在同一 lifecycle 鎖區塊內標記 Child 已釋放。
            session.mark_child_released();
            // 決策條件 5：join 逾時被 detach 就代表停止**未收斂**——那個執行緒
            // 還活著（reader 卡在 PTY read）。此時不得把這次停止宣稱為
            // `Stopped`（契約 §4.3-2：不以未收斂的停止冒充已完成）。
            let detached_before = crate::terminal::session::detached_threads();
            drop(session);
            if crate::terminal::session::detached_threads() > detached_before {
                return PreviousOutcome::Unproven {
                    residual_pids: vec![pid],
                };
            }
        } else {
            drop(session);
        }
        return match outcome {
            StopOutcome::Stopped { .. } => PreviousOutcome::Stopped,
            _ => PreviousOutcome::Unproven { residual_pids },
        };
    }
    // 只有歷史紀錄（daemon 重啟後）：身分相符才可停止（§4.3-1）。
    match process_start_time(pid) {
        Some(current) if current == pid_start => {
            match stop_group(
                pid,
                pid_start,
                GroupEvidence::RecordedLeader,
                state.tuning.stop_term,
                state.tuning.stop_kill,
            ) {
                StopOutcome::Stopped { .. } => PreviousOutcome::Stopped,
                StopOutcome::Unproven { residual_pids } => {
                    PreviousOutcome::Unproven { residual_pids }
                }
                StopOutcome::IdentityMismatch => PreviousOutcome::Unproven {
                    residual_pids: vec![pid],
                },
            }
        }
        // 身分不符：連 start time 都對不上，無法確認是同一個程序，但也不能
        // 說它不存在——回報該 pid 讓人判斷。
        Some(_) => PreviousOutcome::Unproven {
            residual_pids: vec![pid],
        },
        None => PreviousOutcome::Gone,
    }
}

/// 把「停止無法確認」變成結果：標記 `recovery_required` 並回報殘留 pid
/// （契約 §4.3-3、§4.4）。
fn recovery_required(
    state: &DaemonState,
    row: &InstanceRow,
    operation_id: &str,
    residual_pids: &[i32],
    message: &str,
) -> OperationResult {
    {
        let mut inner = state.lock();
        let _ = inner
            .store
            .transaction(|tx| tx.set_lifecycle_state(&row.id, "recovery_required", None));
    }
    let mut refused = OperationResult::refused(operation_id, ErrorCode::RecoveryRequired, message);
    refused.value = json!({ "residual_pids": residual_pids });
    refused
}

/// 由 SQLite 的 spec JSON 重建規格（start／restart 用）。
fn spec_from_row(
    state: &DaemonState,
    operation_id: &str,
    row: &InstanceRow,
) -> Result<ValidatedSpec, Box<OperationResult>> {
    let mut args = row.spec.clone();
    if let Value::Object(object) = &mut args {
        object.insert("name".to_owned(), Value::String(row.name.clone()));
        // 儲存的 backend 一律是 claude（T4 只實作它）。
        object.insert("backend".to_owned(), Value::String("claude".to_owned()));
    }
    lifecycle::validate_spec(&args, &state.home).map_err(|spec_error| {
        Box::new(OperationResult::refused(
            operation_id,
            ErrorCode::InvalidArgument,
            &format!("已儲存的規格不再符合契約：{spec_error}"),
        ))
    })
}

/// `delete_instance`：依 §4.5 停止後封存。
fn delete_instance(
    state: &DaemonState,
    operation_id: &str,
    caller: &Caller,
    args: &Value,
) -> OperationResult {
    let name = match require_name(operation_id, args) {
        Ok(name) => name,
        Err(result) => return *result,
    };
    if let Some(denied) =
        require_permission(state, operation_id, caller, Action::Manage, Some(&name))
    {
        return denied;
    }
    if let Err(busy) = acquire_busy(state, operation_id, &name) {
        return *busy;
    }
    let outcome = (|| {
        let row = match lookup(state, operation_id, &name) {
            Ok(row) => row,
            Err(result) => return *result,
        };
        let mut effects = Vec::new();
        if row.pid.is_some() {
            match stop_previous(state, &row) {
                PreviousOutcome::Stopped => effects.push(String::from("process_stopped")),
                PreviousOutcome::Gone => {}
                PreviousOutcome::Unproven { residual_pids } => {
                    return recovery_required(
                        state,
                        &row,
                        operation_id,
                        &residual_pids,
                        "無法確認程序已停止，不刪除",
                    );
                }
            }
        }
        let archived = {
            let mut inner = state.lock();
            inner
                .store
                .transaction(|tx| {
                    tx.clear_process(&row.id)?;
                    tx.archive_messages(&row.id)?;
                    tx.mark_instance_deleted(&row.id)
                })
                .is_ok()
        };
        if !archived {
            return OperationResult::refused(
                operation_id,
                ErrorCode::Internal,
                "刪除 instance 失敗",
            );
        }
        {
            let mut inner = state.lock();
            inner.last_screens.remove(&row.name);
        }
        OperationResult {
            operation_id: operation_id.to_owned(),
            outcome: Outcome::Completed,
            subject: Some(subject_of(&row)),
            value: json!({}),
            effects,
            error: None,
        }
    })();
    release_busy(state, &name);
    outcome
}

/// `restart_instance`：依 §4.4 停止舊程序後以新 generation spawn。
fn restart_instance(
    state: &DaemonState,
    operation_id: &str,
    caller: &Caller,
    args: &Value,
) -> OperationResult {
    let name = match require_name(operation_id, args) {
        Ok(name) => name,
        Err(result) => return *result,
    };
    if let Some(denied) =
        require_permission(state, operation_id, caller, Action::Restart, Some(&name))
    {
        return denied;
    }
    let mode = args
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("fresh")
        .to_owned();
    if let Err(busy) = acquire_busy(state, operation_id, &name) {
        return *busy;
    }
    let outcome = (|| {
        let row = match lookup(state, operation_id, &name) {
            Ok(row) => row,
            Err(result) => return *result,
        };
        if let Some(expected) = args.get("expected_generation").and_then(Value::as_u64) {
            if expected != row.generation {
                return OperationResult::refused(
                    operation_id,
                    ErrorCode::StaleGeneration,
                    &format!("expected_generation 不符（目前 {}）", row.generation),
                );
            }
        }
        let spec = match spec_from_row(state, operation_id, &row) {
            Ok(spec) => spec,
            Err(result) => return *result,
        };
        // §5.4：resume 的證據檢查在停止舊程序之前。
        if mode == "resume" {
            let shared = {
                let mut inner = state.lock();
                inner
                    .store
                    .transaction(|tx| {
                        tx.other_instances_using_workdir(
                            &row.id,
                            &spec.working_directory.to_string_lossy(),
                        )
                    })
                    .unwrap_or(false)
            };
            if shared {
                let mut refused = OperationResult::refused(
                    operation_id,
                    ErrorCode::ResumeUnavailable,
                    "此 working directory 被其他未刪除的 instance 共用",
                );
                refused.value = json!({"reason": "shared_working_directory"});
                return refused;
            }
            if claude::resume_evidence(&state.home, &spec.working_directory)
                != claude::ResumeEvidence::Found
            {
                let mut refused = OperationResult::refused(
                    operation_id,
                    ErrorCode::ResumeUnavailable,
                    "沒有 resume 所需的 session 證據",
                );
                refused.value = json!({"reason": "no_evidence"});
                return refused;
            }
        }
        let mut effects = Vec::new();
        if row.pid.is_some() {
            match stop_previous(state, &row) {
                PreviousOutcome::Stopped => effects.push(String::from("process_stopped")),
                PreviousOutcome::Gone => {}
                PreviousOutcome::Unproven { residual_pids } => {
                    return recovery_required(
                        state,
                        &row,
                        operation_id,
                        &residual_pids,
                        "無法確認舊程序已停止，不 spawn",
                    );
                }
            }
        }
        finish_spawn(state, operation_id, &row, &spec, effects, mode == "resume")
    })();
    release_busy(state, &name);
    outcome
}

/// `list_instances`：§4.7 的欄位集。
fn list_instances(state: &DaemonState, operation_id: &str, args: &Value) -> OperationResult {
    let filter = target_name(args, "instance");
    let rows = {
        let mut inner = state.lock();
        inner.store.transaction(|tx| tx.all_live_instances())
    };
    let rows = match rows {
        Ok(rows) => rows,
        Err(store_error) => {
            return OperationResult::refused(
                operation_id,
                ErrorCode::Internal,
                &format!("列出 instance 失敗：{store_error}"),
            );
        }
    };
    if let Some(name) = &filter {
        if !rows.iter().any(|row| &row.name == name) {
            return OperationResult::refused(
                operation_id,
                ErrorCode::NotFound,
                &format!("instance {name} 不存在"),
            );
        }
    }
    let instances: Vec<Value> = rows
        .iter()
        .filter(|row| filter.as_ref().is_none_or(|name| *name == row.name))
        .map(|row| {
            json!({
                "name": row.name,
                "id": row.id,
                "state": row.state,
                "reason": row.reason,
                "generation": row.generation,
                "backend": row.spec.get("backend").cloned().unwrap_or(json!("claude")),
                "team": row.spec.get("team").cloned().unwrap_or(Value::Null),
                "role": row.spec.get("role").cloned().unwrap_or(Value::Null),
                "description": row.spec.get("description").cloned().unwrap_or(Value::Null),
                "updated_ms": row.updated_ms,
            })
        })
        .collect();
    OperationResult {
        operation_id: operation_id.to_owned(),
        outcome: Outcome::Completed,
        subject: None,
        value: json!({ "instances": instances }),
        effects: Vec::new(),
        error: None,
    }
}

/// `interrupt`：對目前 generation 的 PTY 寫入一個 ESC（§4.7）。
fn interrupt_instance(
    state: &DaemonState,
    operation_id: &str,
    caller: &Caller,
    args: &Value,
) -> OperationResult {
    let name = match require_name(operation_id, args) {
        Ok(name) => name,
        Err(result) => return *result,
    };
    if let Some(denied) =
        require_permission(state, operation_id, caller, Action::Interrupt, Some(&name))
    {
        return denied;
    }
    let inner = state.lock();
    let Some(running) = inner.running.get(&name) else {
        return OperationResult::refused(
            operation_id,
            ErrorCode::InvalidArgument,
            "此 instance 未在執行中",
        );
    };
    match running.session.write_input(&[0x1b]) {
        Ok(()) => OperationResult {
            operation_id: operation_id.to_owned(),
            outcome: Outcome::Completed,
            subject: None,
            value: json!({}),
            effects: vec![String::from("esc_sent")],
            error: None,
        },
        Err(write_error) => OperationResult::refused(
            operation_id,
            ErrorCode::Internal,
            &format!("寫入 ESC 失敗：{write_error}"),
        ),
    }
}

/// `pane_snapshot`：§4.7 的純文字、`lines`、`live` 與 `no_screen`。
fn pane_snapshot(
    state: &DaemonState,
    operation_id: &str,
    caller: &Caller,
    args: &Value,
) -> OperationResult {
    let name = match require_name(operation_id, args) {
        Ok(name) => name,
        Err(result) => return *result,
    };
    if let Some(denied) = require_permission(
        state,
        operation_id,
        caller,
        Action::PaneSnapshot,
        Some(&name),
    ) {
        return denied;
    }
    let lines_limit = args.get("lines").and_then(Value::as_u64).unwrap_or(100);
    if lines_limit > 10_000 {
        return OperationResult::refused(
            operation_id,
            ErrorCode::InvalidArgument,
            "lines 上限為 10000",
        );
    }
    let mut inner = state.lock();
    let row = match inner.store.live_instance_by_name(&name) {
        Ok(Some(row)) => row,
        Ok(None) => {
            return OperationResult::refused(
                operation_id,
                ErrorCode::NotFound,
                &format!("instance {name} 不存在"),
            );
        }
        Err(store_error) => {
            return OperationResult::refused(
                operation_id,
                ErrorCode::Internal,
                &format!("查詢 instance 失敗：{store_error}"),
            );
        }
    };
    let subject = subject_of(&row);
    let live = inner.running.contains_key(&name);
    let screen = if live {
        let text: Option<Vec<String>> = inner
            .running
            .get(&name)
            .map(|running| running.session.plain_text());
        if let Some(text) = &text {
            if let Some(running) = inner.running.get_mut(&name) {
                running.last_screen = Some(text.clone());
            }
        }
        text
    } else {
        inner
            .running
            .get(&name)
            .and_then(|running| running.last_screen.clone())
            .or_else(|| inner.last_screens.get(&name).cloned())
    };
    drop(inner);
    let Some(text) = screen else {
        // daemon 重啟後沒有舊畫面（§4.7）。
        return OperationResult {
            operation_id: operation_id.to_owned(),
            outcome: Outcome::Completed,
            subject: Some(subject),
            value: json!({
                "text": "",
                "live": false,
                "reason": "no_screen",
                "generation": row.generation,
            }),
            effects: Vec::new(),
            error: None,
        };
    };
    let start = text.len().saturating_sub(lines_limit as usize);
    OperationResult {
        operation_id: operation_id.to_owned(),
        outcome: Outcome::Completed,
        subject: Some(subject),
        value: json!({
            "text": text[start..].join("\n"),
            "live": live,
            "generation": row.generation,
        }),
        effects: Vec::new(),
        error: None,
    }
}

/// `tools_list`（契約 §10）：只列 P1a 已實作且該 instance 可執行的工具。
fn tools_list(state: &DaemonState, operation_id: &str, caller: &Caller) -> OperationResult {
    let is_manager = match caller {
        Caller::Operator => true,
        Caller::Instance(id) => {
            let inner = state.lock();
            inner
                .store
                .instance(id)
                .unwrap_or(None)
                .map(|row| inner.managers.contains(&row.name))
                .unwrap_or(false)
        }
    };
    OperationResult {
        operation_id: operation_id.to_owned(),
        outcome: Outcome::Completed,
        subject: None,
        value: json!({
            "tools": mcp::tool_definitions(is_manager),
            "tool_names": mcp::tool_names(is_manager),
        }),
        effects: Vec::new(),
        error: None,
    }
}

/// `attach`（契約 §7 daemon 端）：先回一行 `OperationResult`，之後改為 frame 串流。
///
/// 超限 Snapshot 的處置（明確定義）：Snapshot payload 超過
/// `frame::MAX_FRAME_BYTES` 時**關閉連線，不送半截畫面，也不送 Error
/// frame**——畫面是必須完整重播的內容，截斷會讓 client 的終端狀態損毀；
/// client 重新 attach 即可取得完整 Snapshot。
fn handle_attach(
    state: &DaemonState,
    request: &Request,
    reader: &mut BufReader<UnixStream>,
    mut writer: UnixStream,
) {
    let operation_id = request
        .operation_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let caller = match authenticate(state, &request.auth) {
        Ok(caller) => caller,
        Err((code, message)) => {
            let _ = write_result(
                &mut writer,
                &OperationResult::refused(&operation_id, code, &message),
            );
            return;
        }
    };
    let name = match target_name(&request.args, "instance") {
        Some(name) => name,
        None => {
            let _ = write_result(
                &mut writer,
                &OperationResult::refused(
                    &operation_id,
                    ErrorCode::InvalidArgument,
                    "instance 必填",
                ),
            );
            return;
        }
    };
    if let Some(denied) = require_permission(
        state,
        &operation_id,
        &caller,
        Action::PaneSnapshot,
        Some(&name),
    ) {
        let _ = write_result(&mut writer, &denied);
        return;
    }

    let (generation, subscriber) = {
        let inner = state.lock();
        let row = inner.store.live_instance_by_name(&name).unwrap_or(None);
        let running = inner.running.get(&name);
        match (row, running) {
            (Some(row), Some(running)) => (row.generation, running.session.subscribe()),
            (_, _) => {
                let refused = OperationResult::refused(
                    &operation_id,
                    ErrorCode::InvalidArgument,
                    "此 instance 未在執行中",
                );
                drop(inner);
                let _ = write_result(&mut writer, &refused);
                return;
            }
        }
    };
    let (snapshot, subscriber) = subscriber;

    let snapshot_frame = match encode_snapshot_frame(&snapshot, generation) {
        Some(frame) => frame,
        None => {
            // 明確定義：超限 → 關閉連線（不送半截畫面）。
            let _ = writer.flush();
            return;
        }
    };
    if writer.write_all(&snapshot_frame).is_err() {
        return;
    }
    let _ = writer.flush();

    let writer = Arc::new(Mutex::new(writer));
    let sender = spawn_frame_writer(writer.clone(), subscriber, generation);
    let _ = read_client_frames(state, &name, generation, reader, writer);
    // 連線結束：writer 執行緒以 recv_timeout 輪詢，session 被 drop 時自然結束；
    // 這裡 join 只是為了不留下孤兒執行緒（有界：recv_timeout 每 200ms 醒一次）。
    let _ = sender.join();
}

/// 編碼 Snapshot frame（4B BE header 長度 + JSON header + 畫面 bytes）。
/// 超過 `MAX_FRAME_BYTES` 時回 `None`（呼叫方關閉連線）。
fn encode_snapshot_frame(snapshot: &crate::terminal::Snapshot, generation: u64) -> Option<Vec<u8>> {
    let header = json!({
        "generation": generation,
        "seq": snapshot.seq,
        "cols": snapshot.cols,
        "rows": snapshot.rows,
    });
    let header_bytes = serde_json::to_vec(&header).ok()?;
    let mut payload = Vec::with_capacity(4 + header_bytes.len() + snapshot.ansi.len());
    payload.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
    payload.extend_from_slice(&header_bytes);
    payload.extend_from_slice(&snapshot.ansi);
    frame::try_encode_frame(0x01, &payload).ok()
}

/// 持續送出 Output／Resync／Ended frame 的執行緒。
fn spawn_frame_writer(
    writer: Arc<Mutex<UnixStream>>,
    subscriber: crate::terminal::session::Subscriber,
    generation: u64,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        match subscriber.recv_timeout(Duration::from_millis(200)) {
            Ok(StreamEvent::Output { seq, bytes }) => {
                let mut payload = Vec::with_capacity(8 + bytes.len());
                payload.extend_from_slice(&seq.to_be_bytes());
                payload.extend_from_slice(&bytes);
                match frame::try_encode_frame(0x02, &payload) {
                    Ok(framed) => {
                        if write_frame(&writer, &framed).is_err() {
                            return;
                        }
                    }
                    // Output 超限：關閉連線（與 Snapshot 同樣的理由）。
                    Err(_) => return,
                }
            }
            Ok(StreamEvent::Resync { snapshot }) => {
                let Some(snapshot_frame) = encode_snapshot_frame(&snapshot, generation) else {
                    return;
                };
                // Resync（空 payload）之後緊接 Snapshot（契約 §7.2）。
                let Ok(resync_frame) = frame::try_encode_frame(0x03, &[]) else {
                    return;
                };
                let mut framed = resync_frame;
                framed.extend_from_slice(&snapshot_frame);
                if write_frame(&writer, &framed).is_err() {
                    return;
                }
            }
            Ok(StreamEvent::Ended) => {
                let payload = json!({"generation": generation, "reason": "exited"});
                if let Ok(bytes) = serde_json::to_vec(&payload) {
                    if let Ok(framed) = frame::try_encode_frame(0x04, &bytes) {
                        let _ = write_frame(&writer, &framed);
                    }
                }
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
    })
}

fn write_frame(writer: &Arc<Mutex<UnixStream>>, bytes: &[u8]) -> std::io::Result<()> {
    let mut guard = writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.write_all(bytes)?;
    guard.flush()
}

/// 讀取 client 的 Input（0x10）／Resize（0x11）／Detach（0x12）。
fn read_client_frames(
    state: &DaemonState,
    name: &str,
    generation: u64,
    reader: &mut BufReader<UnixStream>,
    writer: Arc<Mutex<UnixStream>>,
) -> std::io::Result<()> {
    loop {
        let (tag, payload) = match frame::read_frame(reader) {
            Ok(frame) => frame,
            // 讀取逾時（`WouldBlock`／`TimedOut`）只是「client 暫時沒送新
            // frame」，不是連線結束——直接 return 會讓這條 attach 串流提前
            // 關掉，client 之後送來的 Input 就收不到回應。其餘錯誤才是斷線。
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(_) => return Ok(()),
        };
        match tag {
            0x10 => {
                if payload.len() < 8 {
                    continue;
                }
                let mut generation_bytes = [0u8; 8];
                generation_bytes.copy_from_slice(&payload[..8]);
                let input_generation = u64::from_be_bytes(generation_bytes);
                if input_generation != generation {
                    // 契約 §7.2：generation 不符的 Input 被丟棄並回 Error。
                    let error = json!({"code": "stale_generation", "message": "generation 不符"});
                    if let Ok(bytes) = serde_json::to_vec(&error) {
                        if let Ok(framed) = frame::try_encode_frame(0x05, &bytes) {
                            let _ = write_frame(&writer, &framed);
                        }
                    }
                    continue;
                }
                let inner = state.lock();
                if let Some(running) = inner.running.get(name) {
                    let _ = running.session.write_input(&payload[8..]);
                }
            }
            0x11 => {
                let Ok(parsed) = serde_json::from_slice::<Value>(&payload) else {
                    continue;
                };
                let resize_generation = parsed
                    .get("generation")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                if resize_generation != generation {
                    let error = json!({"code": "stale_generation", "message": "generation 不符"});
                    if let Ok(bytes) = serde_json::to_vec(&error) {
                        if let Ok(framed) = frame::try_encode_frame(0x05, &bytes) {
                            let _ = write_frame(&writer, &framed);
                        }
                    }
                    continue;
                }
                let cols = parsed.get("cols").and_then(Value::as_u64).unwrap_or(0);
                let rows = parsed.get("rows").and_then(Value::as_u64).unwrap_or(0);
                if cols == 0 || rows == 0 {
                    continue;
                }
                let inner = state.lock();
                if let Some(running) = inner.running.get(name) {
                    let _ = running.session.resize(cols as u16, rows as u16);
                }
            }
            0x12 => return Ok(()),
            _ => continue,
        }
    }
}

/// `otterhive daemon` 的進入點（`src/bin/otterhive.rs` 的 `run_daemon` 呼叫）。
///
/// `home` 是 CLI 已解析的 `--home`（契約 §9：全域選項優先於
/// `OTTERHIVE_HOME`）；`None` 才退回讀 `OTTERHIVE_HOME`，再退到
/// `~/.otterhive`（契約 §1）。
///
/// 退出碼由 daemon 端負責（契約 §1、§9）：
/// - 第二個 daemon：`3` ＋ stderr 一行 `daemon_already_running` JSON。
/// - 啟動自檢（bridge 同目錄、SIGCHLD 可 wait）或設定檔失敗：`1` ＋ 一行說明。
///   自檢是 fail-closed 的，沒有任何環境變數可以跳過它。
pub fn main_run(home: Option<PathBuf>) -> ExitCode {
    let home_root = match resolve_home(home) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(1);
        }
    };
    match run(&home_root, Tuning::from_env()) {
        Ok(()) => ExitCode::from(0),
        Err(run_error) if run_error.already_running => {
            let payload = json!({
                "error": {
                    "code": "daemon_already_running",
                    "message": run_error.message,
                }
            });
            eprintln!("{payload}");
            ExitCode::from(3)
        }
        Err(run_error) => {
            eprintln!("{run_error}");
            ExitCode::from(1)
        }
    }
}

/// 決定 home：CLI 已解析的 `--home` 優先，其次 `OTTERHIVE_HOME`，最後
/// `~/.otterhive`（契約 §9／§1）。
fn resolve_home(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if let Some(path) = std::env::var_os("OTTERHIVE_HOME") {
        return Ok(PathBuf::from(path));
    }
    crate::home::home_path().map_err(|home_error| format!("無法決定 home：{home_error}"))
}
