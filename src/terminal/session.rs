//! T3 Lane A GREEN：terminal Session（契約 §7.1 的 daemon 端；attach client 在 T7）。
//!
//! PTY spawn、讀取執行緒、訂閱者、input、resize、exit 觀測。契約行為：
//! - seq 是每 generation 的 PTY 輸出計數：reader 每取一個非空 chunk，先在
//!   Session 鎖內餵入 VT，再 seq 加一並建立 `Output` 事件（計數點，不等送出）。
//! - Snapshot 與訂閱同一把鎖：之後 Output 從 snapshot.seq 加一開始。
//! - 每訂閱者最多排隊 `subscriber_queue` 個 Output；滿時丟棄該訂閱者已排隊
//!   的輸出，之後先送 `Resync`（含新 Snapshot）；Resync／Snapshot 不加 seq；
//!   PTY 讀取永不因訂閱者阻塞。
//! - `OwnedLeaderLease` 由 spawn 建立；exit 觀測用 `waitid WNOWAIT`（不 reap）；
//!   絕不呼叫 portable-pty 的 `wait`／`try_wait`／`kill`。
//! - 所有執行緒 JoinHandle 皆保存，Session Drop 時有界 join（不無限等待）。
//! - `subscriber_queue == 0` 或尺寸任一為 0 即拒絕（daemon 傳入正數；測試可調小）。
//! - stop 核心完成回收後由呼叫端標記 `child_released`；此後任何觸及 Child 的
//!   路徑一律經 `with_child` 檢查（過渡防護，實際呼叫在 T4）。

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::procs::{process_start_time, OwnedLeaderLease};
use crate::terminal::screen::Screen;
use crate::terminal::Snapshot;

/// spawn 參數。`subscriber_queue` 必須大於 0；production 預設 256。
///
/// `env` 是子程序的**完整環境**，不是「額外加上去的變數」：daemon 端
/// （契約 §5.3）先繼承自己的環境、移除 `OTTERHIVE_OPERATOR` 前綴的變數，再
/// 加上 instance 專屬的變數，把結果整份交過來。若這裡改成「在繼承的環境上
/// 疊加」，被移除的 operator 變數會因為 `CommandBuilder` 的繼承行為重新
/// 出現——operator 憑證就會漏進 agent 的環境（契約 §2 明文禁止）。
pub struct SessionConfig {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub size: (u16, u16),
    pub generation: u64,
    pub subscriber_queue: usize,
}

/// 觀測到的退出狀態（`waitid WNOWAIT` 寫入，不 reap）。
#[derive(Debug, Clone, PartialEq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// 訂閱者事件。`Resync` 攜帶新 Snapshot；socket writer 將其編碼為
/// Resync 與 Snapshot 兩個 frame（見 plan T3）。
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Output { seq: u64, bytes: Vec<u8> },
    Resync { snapshot: Snapshot },
    Ended,
}

/// spawn 失敗。
#[derive(Debug)]
pub enum SpawnError {
    InvalidConfig(String),
    Spawn(String),
    Identity(String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::InvalidConfig(reason) => {
                write!(formatter, "無效的 session 設定：{reason}")
            }
            SpawnError::Spawn(reason) => write!(formatter, "PTY 啟動失敗：{reason}"),
            SpawnError::Identity(reason) => {
                write!(formatter, "程序身分無法確認：{reason}")
            }
        }
    }
}

impl std::error::Error for SpawnError {}

/// 單個訂閱者的佇列（Session 鎖內，以 deque 自管以便滿時丟棄已排隊輸出）。
struct SubscriberQueue {
    events: VecDeque<StreamEvent>,
    /// 滿時已丟棄、待送 Resync（下一個事件位置先放 Resync）。
    resync_pending: bool,
}

/// Session 鎖內的可變狀態：畫面、seq、訂閱者佇列。
struct Inner {
    screen: Screen,
    seq: u64,
    subscribers: Vec<SubscriberQueue>,
}

/// 執行緒間共用的核心（`Arc`；`Child` 本體與 `MasterPty` 由 `Session`
/// 持有，執行緒不碰——`MasterPty` 不是 `Sync`，不可進 `Arc`）。
struct Core {
    state: Mutex<Inner>,
    changed: Condvar,
    writer: Mutex<Box<dyn Write + Send>>,
    pid: i32,
    start: u64,
    generation: u64,
    queue_limit: usize,
    lease: OwnedLeaderLease,
    exit: Mutex<Option<ExitInfo>>,
    last_output: Mutex<Instant>,
    ended: Mutex<bool>,
    /// stop 核心完成回收後設 true。此後任何觸及 Child 的路徑都必須先經
    /// [`Session::with_child`] 檢查，避免在 Child 已釋放後再碰它。
    child_released: Mutex<bool>,
    /// observer 失去 exit 觀測權（`waitid` 回 `ECHILD`）。此時沒有觀測到退出
    /// **不等於**沒有退出——呼叫端不可把它當成「還活著」或「已乾淨退出」。
    exit_ownership_lost: Mutex<bool>,
    /// observer 已停止觀測（成功觀測到退出，或失去觀測權）。
    stop: Mutex<bool>,
}

/// Session：PTY、子程序、讀取／觀測執行緒的擁有者。
pub struct Session {
    core: Arc<Core>,
    /// PTY master（resize 用；`Mutex` 包裝以便多執行緒共用）。
    master: Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    /// 持有的 child（絕不呼叫其 `wait`／`try_wait`／`kill`；Drop 時僅釋放句柄）。
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    /// 保存的 JoinHandle（reader＋observer）；Drop 時有界 join。
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

/// 因 join 逾時而被 detach 的執行緒數（決策條件 4：可觀察計數）。
///
/// detach 代表該執行緒可能仍在跑（reader 卡在 `read()` 等 PTY 資料），它的
/// `Arc<Core>` 因此會活得比 Session 久。Session 的鎖設計讓這種殘留不會破壞
/// 狀態，但呼叫端需要知道「這次停止的 join 並未真正收斂」——所以 [`DroppedThread`]
/// 之類的觀察點以本計數為依據。
pub static DETACHED_THREADS: AtomicUsize = AtomicUsize::new(0);

/// 本次 join 逾時而被 detach 的執行緒數。
pub fn detached_threads() -> usize {
    DETACHED_THREADS.load(Ordering::SeqCst)
}

impl Session {
    /// spawn 一個 PTY session 並啟動 reader／observer 執行緒。
    pub fn spawn(cfg: SessionConfig) -> Result<Session, SpawnError> {
        if cfg.subscriber_queue == 0 {
            return Err(SpawnError::InvalidConfig(
                "subscriber_queue 必須大於 0".to_owned(),
            ));
        }
        if cfg.size.0 == 0 || cfg.size.1 == 0 {
            return Err(SpawnError::InvalidConfig("PTY 尺寸必須為正".to_owned()));
        }
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: cfg.size.1,
                cols: cfg.size.0,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| SpawnError::Spawn(format!("{error:?}")))?;
        let mut builder = CommandBuilder::new(cfg.program.clone());
        builder.args(cfg.args.iter().map(String::as_str));
        builder.cwd(&cfg.cwd);
        for (key, value) in &cfg.env {
            builder.env(key, value);
        }
        // 契約 §5.3：子程序不得看到名稱以 `OTTERHIVE_OPERATOR` 開頭的變數。
        // `CommandBuilder` 預設繼承本程序環境，因此這裡逐一移除繼承來的那些
        // key（daemon 端也過濾一次；這裡是 spawn 層的最後一道，確保任何呼叫端
        // ——包括直接用 `SessionConfig` 的測試——都不會把 operator 憑證漏進去）。
        for key in operator_prefixed_keys() {
            builder.env_remove(key);
        }
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| SpawnError::Spawn(format!("{error:?}")))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| SpawnError::Spawn(format!("{error:?}")))?;
        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|error| SpawnError::Spawn(format!("{error:?}")))?;
        let pid = child
            .process_id()
            .ok_or_else(|| SpawnError::Identity("無法取得 child pid".to_owned()))?
            as i32;
        let start = process_start_time(pid)
            .ok_or_else(|| SpawnError::Identity("無法讀取 child 啟動時間".to_owned()))?;
        let pgid = unsafe { libc::getpgid(pid) };
        if pgid != pid {
            return Err(SpawnError::Identity(
                "child 不是 session leader（pgid != pid）".to_owned(),
            ));
        }
        let core = Arc::new(Core {
            state: Mutex::new(Inner {
                screen: Screen::new(cfg.size.0, cfg.size.1),
                seq: 0,
                subscribers: Vec::new(),
            }),
            changed: Condvar::new(),
            writer: Mutex::new(writer),
            pid,
            start,
            generation: cfg.generation,
            queue_limit: cfg.subscriber_queue,
            lease: OwnedLeaderLease::new(pid, start, pgid),
            exit: Mutex::new(None),
            last_output: Mutex::new(Instant::now()),
            ended: Mutex::new(false),
            child_released: Mutex::new(false),
            exit_ownership_lost: Mutex::new(false),
            stop: Mutex::new(false),
        });
        let threads = vec![
            spawn_reader(reader, Arc::clone(&core)),
            spawn_observer(pid, Arc::clone(&core)),
        ];
        Ok(Session {
            core,
            master: Mutex::new(pair.master),
            _child: child,
            threads: Mutex::new(threads),
        })
    }

    pub fn pid(&self) -> i32 {
        self.core.pid
    }

    pub fn start_time(&self) -> u64 {
        self.core.start
    }

    pub fn owned_leader_lease(&self) -> &OwnedLeaderLease {
        &self.core.lease
    }

    /// Child 是否已被 stop 核心回收並釋放（此後不得再觸及 Child）。
    pub fn child_is_released(&self) -> bool {
        *self
            .core
            .child_released
            .lock()
            .expect("繁中說明：child_released 槽不應中毒")
    }

    /// 在 Child 尚未被釋放時把它交給 `body`；已釋放則回
    /// [`SpawnError::Identity`]。這是 T4 內部停止路徑的過渡防護：
    /// Child 釋放後任何再觸及 Child 的呼叫都在此擋下，不落到
    /// portable-pty 的 `wait`／`try_wait`／`kill`。
    pub fn with_child<T>(
        &self,
        body: impl FnOnce(&dyn portable_pty::Child) -> T,
    ) -> Result<T, SpawnError> {
        if self.child_is_released() {
            return Err(SpawnError::Identity(
                "child 已於停止完成後釋放，不可再觸及".to_owned(),
            ));
        }
        Ok(body(self._child.as_ref()))
    }

    /// 標記 Child 已被 stop 核心回收並釋放；之後的 Child 操作一律拒絕。
    pub fn mark_child_released(&self) {
        *self
            .core
            .child_released
            .lock()
            .expect("繁中說明：child_released 槽不應中毒") = true;
    }

    pub fn write_input(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.core
            .writer
            .lock()
            .expect("繁中說明：Session 寫端不應中毒")
            .write_all(bytes)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        if cols == 0 || rows == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "PTY 尺寸必須為正",
            ));
        }
        self.master
            .lock()
            .expect("繁中說明：Session master 不應中毒")
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
        self.core
            .state
            .lock()
            .expect("繁中說明：Session 鎖不應中毒")
            .screen
            .resize(cols, rows);
        Ok(())
    }

    /// 同一把鎖內取 snapshot 並登記訂閱者。
    pub fn subscribe(&self) -> (Snapshot, Subscriber) {
        let mut state = self
            .core
            .state
            .lock()
            .expect("繁中說明：Session 鎖不應中毒");
        let snapshot = Snapshot {
            generation: self.core.generation,
            seq: state.seq,
            cols: state.screen.cols(),
            rows: state.screen.rows(),
            ansi: state.screen.render_ansi(),
        };
        state.subscribers.push(SubscriberQueue {
            events: VecDeque::new(),
            resync_pending: false,
        });
        let index = state.subscribers.len() - 1;
        (
            snapshot,
            Subscriber {
                core: Arc::clone(&self.core),
                index,
            },
        )
    }

    pub fn plain_text(&self) -> Vec<String> {
        self.core
            .state
            .lock()
            .expect("繁中說明：Session 鎖不應中毒")
            .screen
            .plain_lines()
    }

    pub fn last_output_at(&self) -> Instant {
        *self
            .core
            .last_output
            .lock()
            .expect("繁中說明：Session 時間戳不應中毒")
    }

    /// 已觀測的 exit status（不 reap）。
    pub fn exit_status(&self) -> Option<ExitInfo> {
        self.core
            .exit
            .lock()
            .expect("繁中說明：Session exit 槽不應中毒")
            .clone()
    }

    /// 等待已觀測的 exit status（有界；不 reap）。
    pub fn wait_exit(&self, timeout: Duration) -> Option<ExitInfo> {
        let begin = Instant::now();
        loop {
            if let Some(exit) = self.exit_status() {
                return Some(exit);
            }
            // observer 已停止觀測（成功觀測到退出，或失去觀測權）→ 不會再有
            // exit status，此時回 `None` 是確定的答案而非「還沒發生」。
            if self.has_observer_stopped() {
                return self.exit_status();
            }
            if begin.elapsed() >= timeout {
                return self.exit_status();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// observer 是否已停止觀測 exit（觀測到退出，或失去觀測權）。
    pub fn has_observer_stopped(&self) -> bool {
        *self
            .core
            .stop
            .lock()
            .expect("繁中說明：Session stop 槽不應中毒")
    }

    /// observer 是否因 `ECHILD` 失去 exit 觀測權。
    ///
    /// 為 true 代表「沒有觀測到退出」**不能**推論成「還活著」或「已乾淨退出」——
    /// child 已不由本程序持有，exit status 與 waitable 狀態都取不到。
    pub fn exit_ownership_lost(&self) -> bool {
        *self
            .core
            .exit_ownership_lost
            .lock()
            .expect("繁中說明：Session exit_ownership_lost 槽不應中毒")
    }
}

impl Drop for Session {
    /// **真・有界** join（決策條件 1、2）：deadline 只設一次，以 `is_finished`
    /// 輪詢到 deadline 為止；逾時的 handle 直接 detach（不 join），並累加
    /// [`DETACHED_THREADS`] 計數。
    ///
    /// 先用 `mem::take` 取出 handle 清單再 join，因此整段 join 過程**不持有**
    /// `threads` 的鎖——持有鎖 join 會讓同時 drop 兩個 Session 的執行緒互相
    /// 等待。
    ///
    /// 為什麼需要 detach：reader 執行緒會卡在 `read()` 等 PTY 資料，而 PTY 只在
    /// 子程序結束時才會 EOF。若停止一個仍存活、忽略 SIGTERM 的程序，
    /// `handle.join()` 會永久阻塞——那正是契約 §4.3-2 禁止的「以無限等待 join
    /// 冒充有界停止」。
    fn drop(&mut self) {
        let mut threads = std::mem::take(
            &mut *self
                .threads
                .lock()
                .unwrap_or_else(|guard| guard.into_inner()),
        );
        // deadline 只設一次（決策條件 1）：第二個 handle 不因第一個已耗時而
        // 各自取得完整期限。
        let deadline = Instant::now() + Duration::from_secs(2);
        for handle in threads.drain(..) {
            while !handle.is_finished() {
                if Instant::now() >= deadline {
                    // 逾時：放棄這個 handle（detach），不阻塞。
                    drop(handle);
                    DETACHED_THREADS.fetch_add(1, Ordering::SeqCst);
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// 繼承環境中名稱以 `OTTERHIVE_OPERATOR` 開頭的變數鍵（契約 §5.3）。
fn operator_prefixed_keys() -> Vec<String> {
    std::env::vars()
        .map(|(key, _)| key)
        .filter(|key| key.starts_with("OTTERHIVE_OPERATOR"))
        .collect()
}

/// PTY 讀取執行緒：每取一個非空 chunk，在鎖內餵 VT→seq 加一→廣播。
/// 回傳 JoinHandle（由 `Session::spawn` 保存）。
fn spawn_reader(reader: Box<dyn Read + Send>, core: Arc<Core>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => push_output(&core, &chunk[..n]),
            }
        }
        mark_ended(&core);
    })
}

/// exit 觀測執行緒：`waitid WNOWAIT` 輪詢，只寫入 `ExitInfo`，不 reap。
/// ECHILD（已被外部回收）即退出；回傳 JoinHandle。
fn spawn_observer(pid: i32, core: Arc<Core>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if ret == 0 && info.si_pid == pid {
            let exit = match info.si_code {
                c if c == libc::CLD_EXITED => ExitInfo {
                    code: Some(info.si_status),
                    signal: None,
                },
                c if c == libc::CLD_KILLED || c == libc::CLD_DUMPED => ExitInfo {
                    code: None,
                    signal: Some(info.si_status),
                },
                _ => ExitInfo {
                    code: None,
                    signal: None,
                },
            };
            *core.exit.lock().expect("繁中說明：Session exit 槽不應中毒") = Some(exit);
            *core.stop.lock().expect("繁中說明：Session stop 槽不應中毒") = true;
            break;
        }
        if ret != 0 {
            let errno = unsafe { *libc::__error() };
            if errno == libc::ECHILD {
                // `ECHILD`：這個 child 已不由本程序擁有（被外部回收，或 parent
                // 關係已改變）。exit 觀測的前提消失——無法再取得 exit status，
                // 也不能再證明 child 未被 reap。標記 `exit_ownership_lost` 讓
                // 呼叫端知道「沒有觀測到退出不等於沒有退出」，並補上 stop flag
                // 讓 wait_exit 不再無限等待。
                *core
                    .exit_ownership_lost
                    .lock()
                    .expect("繁中說明：Session exit_ownership_lost 槽不應中毒") = true;
                *core.stop.lock().expect("繁中說明：Session stop 槽不應中毒") = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    })
}

/// reader 計數點：鎖內餵 VT → seq 加一 → 非阻塞廣播。
/// 滿的訂閱者丟棄已排隊輸出並在下個位置放 `Resync`；reader 永不阻塞。
fn push_output(core: &Core, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let mut state = core.state.lock().expect("繁中說明：Session 鎖不應中毒");
    state.screen.feed(bytes);
    state.seq += 1;
    let seq = state.seq;
    // 先算好 Resync 用的 snapshot（借用結束後再進可變迴圈）。
    let resync_snapshot = Snapshot {
        generation: core.generation,
        seq: state.seq,
        cols: state.screen.cols(),
        rows: state.screen.rows(),
        ansi: state.screen.render_ansi(),
    };
    for sub in state.subscribers.iter_mut() {
        // 佇列曾滿：已有未取走的 Resync 時，只把它的 snapshot 更新為最新，
        // 不追加第二個（恢復讀取時第一個即最新 Resync，之後即 Output）。
        // 當前 chunk 的 Output 跳過（其內容已含在 Snapshot 畫面中）；下一個
        // Output 即 Snapshot.seq 加一（契約 §7.1）。Resync 不加 seq。
        if sub.resync_pending {
            let mut replaced = false;
            if let Some(StreamEvent::Resync { snapshot }) = sub.events.back_mut() {
                *snapshot = resync_snapshot.clone();
                replaced = true;
            }
            if !replaced {
                sub.events.clear();
                sub.events.push_back(StreamEvent::Resync {
                    snapshot: resync_snapshot.clone(),
                });
            }
            continue;
        }
        if sub.events.len() >= core.queue_limit {
            // 丟棄已排隊輸出；標記待 Resync（下一個新 Output 到達時送最新畫面）。
            sub.events.clear();
            sub.resync_pending = true;
            continue;
        }
        sub.events.push_back(StreamEvent::Output {
            seq,
            bytes: bytes.to_vec(),
        });
    }
    drop(state);
    *core
        .last_output
        .lock()
        .expect("繁中說明：Session 時間戳不應中毒") = Instant::now();
    core.changed.notify_all();
}

/// reader 結束（EOF／錯誤）：各訂閱者補 `Ended` 並標記。
/// 佇列曾滿（`resync_pending`）時先補一個含最新 Snapshot 的 `Resync`，
/// 再補 `Ended`——否則 client 只收到 `Ended` 而從未取得被丟棄輸出之後的
/// 畫面邊界，重連後無法知道最後狀態（契約 §7.1 的 Resync 邊界語意）。
fn mark_ended(core: &Core) {
    let mut state = core.state.lock().expect("繁中說明：Session 鎖不應中毒");
    *core.ended.lock().unwrap_or_else(|guard| guard.into_inner()) = true;
    // 刻意**不**在此設 `stop`：`stop` 的語意是「observer 已停止觀測 exit」。
    // reader 的 EOF 與 observer 的 exit 觀測是兩件事——child 退出時 reader
    // 先看到 PTY EOF，observer 才寫入 exit status。若這裡也設 stop，
    // `wait_exit` 會在 observer 寫入 exit 之前就回 `None`，讓
    // `child_exit_is_observed` 之類的等待看不到 exit status。
    // 先算好最新 Snapshot（借用結束後再進可變迴圈），供待 Resync 的訂閱者使用。
    let final_snapshot = Snapshot {
        generation: core.generation,
        seq: state.seq,
        cols: state.screen.cols(),
        rows: state.screen.rows(),
        ansi: state.screen.render_ansi(),
    };
    for sub in state.subscribers.iter_mut() {
        if sub.resync_pending {
            // 已丟棄排隊輸出：邊界必須是最新 Snapshot，且 Resync 不加 seq。
            sub.events.clear();
            sub.events.push_back(StreamEvent::Resync {
                snapshot: final_snapshot.clone(),
            });
            sub.resync_pending = false;
        }
        sub.events.push_back(StreamEvent::Ended);
    }
    drop(state);
    core.changed.notify_all();
}

/// 訂閱端：`recv_timeout` 依序取事件；佇列曾滿時先收到 `Resync`。
pub struct Subscriber {
    core: Arc<Core>,
    index: usize,
}

impl Subscriber {
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<StreamEvent, std::sync::mpsc::RecvTimeoutError> {
        let begin = Instant::now();
        let mut state = self
            .core
            .state
            .lock()
            .expect("繁中說明：Session 鎖不應中毒");
        loop {
            // 訂閱索引在 Session 生命週期內穩定（不移除訂閱者）。
            let event = state.subscribers.get_mut(self.index).and_then(|sub| {
                let event = sub.events.pop_front()?;
                // 取出 Resync 後清除標記，後續 Output 從新邊界接續。
                if matches!(event, StreamEvent::Resync { .. }) {
                    sub.resync_pending = false;
                }
                Some(event)
            });
            if let Some(event) = event {
                return Ok(event);
            }
            let remaining = timeout.saturating_sub(begin.elapsed());
            if remaining.is_zero() {
                return Err(std::sync::mpsc::RecvTimeoutError::Timeout);
            }
            let (guard, result) = self
                .core
                .changed
                .wait_timeout(state, remaining)
                .expect("繁中說明：Session 條件變數不應中毒");
            state = guard;
            if result.timed_out() {
                // 最後再取一次，避免喚醒競爭漏事件。
                if let Some(event) = state
                    .subscribers
                    .get_mut(self.index)
                    .and_then(|sub| sub.events.pop_front())
                {
                    return Ok(event);
                }
                return Err(std::sync::mpsc::RecvTimeoutError::Timeout);
            }
        }
    }
}
