//! T3 Lane A GREEN：程序啟動時間、有證據的停止（契約 §4.3）。
//!
//! 契約行為不變，只修查詢參數：`process_start_time` 與驗證一律以
//! `PROC_PIDTBSDINFO arg=1` 查詢（同一 zombie 上 arg=0 回 ESRCH、arg=1 回
//! start／pgid；只有後者可用於重驗）。`waitid(WEXITED|WNOHANG|WNOWAIT)`
//! 觀測 exit 而不 reap；絕不呼叫 portable-pty 的 `wait`／`try_wait`、
//! `ChildKiller::kill`／`clone_killer` 做觀測或清理。
//!
//! 核心順序：撤銷檢查優先 → 綁定相符 → start(arg=1)／pgid／probe 一致 →
//! SIGTERM → 有界等待 → 列舉（失敗即 `Unproven`；只排除已確認退出的 owned
//! leader；空列舉但 probe 仍 Alive 屬證據矛盾）→ 仍有成員則 SIGKILL →
//! 有界等待 → 列舉 → 仍有成員則 `Unproven{residual_pids}` → 撤銷 lease →
//! reap（失敗即 `Unproven`）。等待期間每次 signal 前重驗；ECHILD／查詢失敗
//! 永久失效（fail-closed）。`RecordedLeader` 永不授權 zombie 清理。
//! reap 由 Session 在撤銷後對持有的 Child 執行——公開 `stop_group` 的真實
//! 路徑以 `waitpid(WNOHANG)` 做最終回收（一次性；ECHILD 即 fail-closed）；
//! `Session` 內部停止在同一 lifecycle 鎖內走同一核心，成功後標記 Child
//! 已釋放、不再觸碰，鎖外 join 執行緒。

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `waitid` 輪詢間隔（等待期間的重驗粒度）。
const POLL_STEP: Duration = Duration::from_millis(20);

/// 同一把 lifecycle 鎖：驗證、每次 signal 前重驗、撤銷、reap 全程序列化。
/// `stop_group_with_adapter` 進入即持有；`Session` 內部停止取用同一把鎖，
/// 保證驗證後、signal 前不可 reap（RED `lease_validation_and_reap_are_serialized`
/// 的 production 對應；單元併發測試直接競爭此鎖）。
static LIFECYCLE_LOCK: Mutex<()> = Mutex::new(());

/// 取同一把 lifecycle 鎖（`Session` 內部停止用；`Session` 層見 `session.rs`）。
pub(crate) fn lifecycle_guard() -> std::sync::MutexGuard<'static, ()> {
    LIFECYCLE_LOCK
        .lock()
        .expect("繁中說明：lifecycle 鎖不應中毒")
}

/// `waitid WNOWAIT` 觀測結果（不 reap）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ChildProbe {
    Alive,
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

/// 同一個 stop 核心的可注入 OS 面（seam v4；非公開契約）。
///
/// `start_time` 的真實實作必須以 `PROC_PIDTBSDINFO arg=1` 查詢；
/// `probe_child` 必須把 `si_pid==0`（尚無 exit 事件）映射為 `Alive`；
/// ECHILD 必須是 raw errno == ECHILD 的錯誤。
pub(crate) trait OsAdapter: Send + Sync {
    fn start_time(&self, pid: i32) -> std::io::Result<u64>;
    fn pgid(&self, pid: i32) -> std::io::Result<i32>;
    fn probe_child(&self, pid: i32) -> std::io::Result<ChildProbe>;
    fn list_group(&self, pgid: i32) -> std::io::Result<Vec<i32>>;
    fn kill_group(&self, pgid: i32, sig: libc::c_int) -> std::io::Result<()>;
    fn reap(&self, pid: i32) -> std::io::Result<()>;
}

/// 停止操作的證據：`OwnedLeader` 為本 daemon 持有的 child；
/// `RecordedLeader` 僅為 SQLite 歷史，只能用於存活 leader 的 recovery。
#[derive(Debug)]
pub enum GroupEvidence<'a> {
    OwnedLeader(&'a OwnedLeaderLease),
    RecordedLeader,
}

/// 停止結果。
#[derive(Debug)]
pub enum StopOutcome {
    Stopped { escalated_to_kill: bool },
    Unproven { residual_pids: Vec<i32> },
    IdentityMismatch,
}

/// 本 daemon 擁有的 leader 身分：綁定 spawn 時的 pid／啟動時間／pgid，
/// 且 child 尚未被 reap。欄位私有，不可由 SQLite 重建；一旦失效永久有效。
#[derive(Debug)]
pub struct OwnedLeaderLease {
    pid: i32,
    start: u64,
    pgid: i32,
    state: Mutex<LeaseState>,
}

#[derive(Debug)]
struct LeaseState {
    revoked: bool,
    /// ECHILD 或任何查詢失敗後永久失效（fail-closed）。
    invalidated: bool,
}

impl OwnedLeaderLease {
    /// Session spawn 成功後建立（綁定當時的 pid／start／pgid 與未 reap 的 child）。
    pub(crate) fn new(pid: i32, start: u64, pgid: i32) -> Self {
        Self {
            pid,
            start,
            pgid,
            state: Mutex::new(LeaseState {
                revoked: false,
                invalidated: false,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(pid: i32, start: u64, pgid: i32) -> Self {
        Self::new(pid, start, pgid)
    }

    /// 僅測試用：模擬清理完成後的撤銷狀態。
    #[cfg(test)]
    pub(crate) fn revoke(&self) {
        self.state
            .lock()
            .expect("繁中說明：lease 鎖不應中毒")
            .revoked = true;
    }

    /// 清理完成後撤銷（production 路徑；`&` 方法，改內部 Mutex）。
    fn revoke_production(&self) {
        self.state
            .lock()
            .expect("繁中說明：lease 鎖不應中毒")
            .revoked = true;
    }

    fn invalidate(&self) {
        self.state
            .lock()
            .expect("繁中說明：lease 鎖不應中毒")
            .invalidated = true;
    }

    /// 撤銷或失效其一即不可用。檢查本身不做 OS 呼叫。
    fn is_usable(&self) -> bool {
        let state = self.state.lock().expect("繁中說明：lease 鎖不應中毒");
        !state.revoked && !state.invalidated
    }

    /// 綁定是否與傳入參數相符（production 不得以傳入參數覆蓋綁定）。
    fn binds(&self, pid: i32, expected_start: u64) -> bool {
        self.pid == pid && self.start == expected_start
    }
}

/// 以 `PROC_PIDTBSDINFO arg=1` 讀 pid 的啟動時間（秒＋微秒）。
/// arg=0 對 zombie 回 ESRCH，不可用於重驗，故一律用 arg=1。
fn proc_start_arg1(pid: i32) -> std::io::Result<u64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let ret = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            1,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if ret == size {
        Ok(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// 以 `PROC_PIDTBSDINFO arg=1` 讀 pid 的 pgid（同一次查詢的身分附帶值）。
fn proc_pgid_arg1(pid: i32) -> std::io::Result<i32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let ret = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            1,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if ret == size {
        Ok(info.pbi_pgid as i32)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// `waitid(P_PID, pid, WEXITED|WNOHANG|WNOWAIT)` 觀測（不 reap）。
/// `ret==0 && si_pid==0` 表示尚無 exit 事件 → `Alive`。
fn waitid_probe(pid: i32) -> std::io::Result<ChildProbe> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if info.si_pid == 0 {
        return Ok(ChildProbe::Alive);
    }
    if info.si_pid != pid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "waitid 回傳非目標 pid",
        ));
    }
    match info.si_code {
        c if c == libc::CLD_EXITED => Ok(ChildProbe::Exited {
            code: Some(info.si_status),
            signal: None,
        }),
        c if c == libc::CLD_KILLED || c == libc::CLD_DUMPED => Ok(ChildProbe::Exited {
            code: None,
            signal: Some(info.si_status),
        }),
        _ => Ok(ChildProbe::Alive),
    }
}

/// 以 `proc_listpgrppids` 列舉 group 成員。
fn proc_group_members(pgid: i32) -> std::io::Result<Vec<i32>> {
    // 單一 group 極少超過此數；若截斷，測試以 residuals 語意覆蓋（不靜默）。
    let mut buffer = vec![0 as libc::pid_t; 4096];
    let bytes = (buffer.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    let count =
        unsafe { libc::proc_listpgrppids(pgid, buffer.as_mut_ptr() as *mut libc::c_void, bytes) };
    if count < 0 {
        return Err(std::io::Error::last_os_error());
    }
    buffer.truncate(count as usize);
    Ok(buffer)
}

/// 真實 OS adapter（production 用；只做 pid 數字的查詢與 signal，
/// 不觸碰 portable-pty 的 `Child` 物件——絕不呼叫 `wait`／`try_wait`／`kill`）。
struct RealOs;

impl OsAdapter for RealOs {
    fn start_time(&self, pid: i32) -> std::io::Result<u64> {
        proc_start_arg1(pid)
    }

    fn pgid(&self, pid: i32) -> std::io::Result<i32> {
        proc_pgid_arg1(pid)
    }

    fn probe_child(&self, pid: i32) -> std::io::Result<ChildProbe> {
        waitid_probe(pid)
    }

    fn list_group(&self, pgid: i32) -> std::io::Result<Vec<i32>> {
        proc_group_members(pgid)
    }

    fn kill_group(&self, pgid: i32, sig: libc::c_int) -> std::io::Result<()> {
        let ret = unsafe { libc::kill(-pgid, sig) };
        if ret == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// 最終回收：`waitpid(pid, WNOHANG)` 一次性回收 zombie。
    /// ECHILD 回 Err（fail-closed）：正常流程中不應出現（Session 持有 Child，
    /// 未經此處即被 reap）；Session 內部路徑見 `session.rs`。
    fn reap(&self, pid: i32) -> std::io::Result<()> {
        let mut status = 0;
        let ret = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if ret == pid {
            Ok(())
        } else if ret == 0 {
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "child 尚未退出，不可 reap",
            ))
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

/// 讀 pid 的啟動時間（契約 §4.3 的身分要素；arg=1）。
pub fn process_start_time(pid: i32) -> Option<u64> {
    proc_start_arg1(pid).ok()
}

/// 同一個 stop 核心（production 與測試共用）。
///
/// 進入即持有全域 lifecycle 鎖全程：驗證、signal、撤銷、reap 序列化；
/// 併發的第二個 stop 在鎖外等待，前一個撤銷後它見到失效即拒絕。
pub(crate) fn stop_group_with_adapter(
    pid: i32,
    expected_start: u64,
    evidence: GroupEvidence<'_>,
    term_wait: Duration,
    kill_wait: Duration,
    os: &dyn OsAdapter,
) -> StopOutcome {
    let _lifecycle = lifecycle_guard();
    // 0. 撤銷檢查優先於一切驗證（即使 zombie 仍 waitable 也零 signal）。
    // 1. RecordedLeader 永不授權 zombie 清理；存活 leader 才可 recovery stop。
    let owned: Option<&OwnedLeaderLease> = match evidence {
        GroupEvidence::OwnedLeader(lease) => {
            if !lease.is_usable() {
                return StopOutcome::Unproven {
                    residual_pids: Vec::new(),
                };
            }
            if !lease.binds(pid, expected_start) {
                return StopOutcome::IdentityMismatch;
            }
            Some(lease)
        }
        GroupEvidence::RecordedLeader => None,
    };

    // 2. 起始驗證：start(arg=1) 相符。
    let start = match os.start_time(pid) {
        Ok(start) => start,
        Err(_) => {
            if let Some(lease) = owned {
                lease.invalidate();
            }
            return StopOutcome::Unproven {
                residual_pids: Vec::new(),
            };
        }
    };
    if start != expected_start {
        return StopOutcome::IdentityMismatch;
    }

    // 3. pgid 驗證：活體須 pgid==pid；zombie 的 ESRCH（raw errno）為預期；
    //    暫時性失敗 → Unproven。ECHILD 以外一律 fail-closed。
    let pgid = match os.pgid(pid) {
        Ok(pgid) => pgid,
        Err(error) => {
            if error.raw_os_error() == Some(libc::ECHILD) {
                // zombie 的 getpgid 回 ESRCH 為預期：以綁定的 pgid 繼續，
                // 後續 probe 必須為 Exited，否則仍拒絕。
                match owned {
                    Some(lease) => lease.pgid,
                    None => {
                        return StopOutcome::Unproven {
                            residual_pids: Vec::new(),
                        };
                    }
                }
            } else {
                if let Some(lease) = owned {
                    lease.invalidate();
                }
                return StopOutcome::Unproven {
                    residual_pids: Vec::new(),
                };
            }
        }
    };
    if let Some(lease) = owned {
        if pgid != lease.pgid {
            return StopOutcome::IdentityMismatch;
        }
    } else {
        // RecordedLeader：活體須 pgid==pid，否則不授權。
        if pgid != pid {
            return StopOutcome::Unproven {
                residual_pids: Vec::new(),
            };
        }
    }

    // 4. probe：ECHILD（raw errno）→ 永久失效；其它失敗 → Unproven＋失效。
    let probe = match os.probe_child(pid) {
        Ok(probe) => probe,
        Err(_) => {
            if let Some(lease) = owned {
                lease.invalidate();
            }
            return StopOutcome::Unproven {
                residual_pids: Vec::new(),
            };
        }
    };

    // RecordedLeader：只有 probe Alive 的存活 leader 可 recovery stop；
    // zombie（Exited）永不授權——OS 查得到身分也不構成 ownership。
    if owned.is_none() {
        match probe {
            ChildProbe::Alive => {}
            ChildProbe::Exited { .. } => {
                return StopOutcome::Unproven {
                    residual_pids: Vec::new(),
                };
            }
        }
    }

    let exited_already = matches!(probe, ChildProbe::Exited { .. });

    // 5. 發 SIGTERM（發前重驗：lease 可用＋start 相符；每次 signal 前皆然）。
    if let Some(lease) = owned {
        if !lease.is_usable() {
            return StopOutcome::Unproven {
                residual_pids: Vec::new(),
            };
        }
        match os.start_time(pid) {
            Ok(current) if current == expected_start => {}
            Ok(_) => return StopOutcome::IdentityMismatch,
            Err(_) => {
                lease.invalidate();
                return StopOutcome::Unproven {
                    residual_pids: Vec::new(),
                };
            }
        }
    }
    if os.kill_group(pgid, libc::SIGTERM).is_err() {
        if let Some(lease) = owned {
            lease.invalidate();
        }
        return StopOutcome::Unproven {
            residual_pids: Vec::new(),
        };
    }
    let mut escalated = false;

    // 6. 有界等待 TERM 生效 → 列舉。列舉失敗 → Unproven。
    wait_quiet(term_wait);
    let probe_after_term = match os.probe_child(pid) {
        Ok(probe) => probe,
        Err(_) => {
            if let Some(lease) = owned {
                lease.invalidate();
            }
            return StopOutcome::Unproven {
                residual_pids: Vec::new(),
            };
        }
    };
    let members = match os.list_group(pgid) {
        Ok(members) => members,
        Err(_) => {
            if let Some(lease) = owned {
                lease.invalidate();
            }
            return StopOutcome::Unproven {
                residual_pids: Vec::new(),
            };
        }
    };
    // 只排除「已確認退出」的 owned leader：probe 必須為 Exited，否則 leader
    // 本人仍算成員（si_pid==0 尚無 exit 事件時絕不排除）。
    let leader_gone = matches!(probe_after_term, ChildProbe::Exited { .. });
    let mut remaining: Vec<i32> = members
        .into_iter()
        .filter(|member| if *member == pid { !leader_gone } else { true })
        .collect();
    // 空列舉但 probe 仍 Alive 屬證據矛盾 → Unproven。
    if remaining.is_empty() {
        if matches!(probe_after_term, ChildProbe::Alive) && !exited_already {
            if let Some(lease) = owned {
                lease.invalidate();
            }
            return StopOutcome::Unproven {
                residual_pids: Vec::new(),
            };
        }
        return finish_stopped(owned, pid, os, escalated);
    }

    // 6b. leader 已確認退出、但 group 仍有其他成員：TERM 已送達 group，
    //     child 可能正在退出途中。給一次 grace（等 kill_wait、不升級），
    //     重列舉；仍有成員才升級 KILL。probe 仍 Alive（leader 存活）時
    //     直接升級——符合契約「仍有存活成員則 SIGKILL」。
    if leader_gone {
        wait_quiet(kill_wait);
        let probe_grace = os.probe_child(pid).unwrap_or(ChildProbe::Alive);
        let members = match os.list_group(pgid) {
            Ok(members) => members,
            Err(_) => {
                if let Some(lease) = owned {
                    lease.invalidate();
                }
                return StopOutcome::Unproven {
                    residual_pids: remaining,
                };
            }
        };
        let leader_gone = matches!(probe_grace, ChildProbe::Exited { .. });
        remaining = members
            .into_iter()
            .filter(|member| if *member == pid { !leader_gone } else { true })
            .collect();
        if remaining.is_empty() {
            return finish_stopped(owned, pid, os, escalated);
        }
    }

    // 7. 仍有成員 → 重驗後 SIGKILL → 有界等待 → 列舉。
    if let Some(lease) = owned {
        if !lease.is_usable() {
            return StopOutcome::Unproven {
                residual_pids: remaining,
            };
        }
        match os.start_time(pid) {
            Ok(current) if current == expected_start => {}
            Ok(_) => return StopOutcome::IdentityMismatch,
            Err(_) => {
                lease.invalidate();
                return StopOutcome::Unproven {
                    residual_pids: remaining,
                };
            }
        }
    }
    if os.kill_group(pgid, libc::SIGKILL).is_err() {
        if let Some(lease) = owned {
            lease.invalidate();
        }
        return StopOutcome::Unproven {
            residual_pids: remaining,
        };
    }
    escalated = true;
    wait_quiet(kill_wait);
    let probe_after_kill = os.probe_child(pid).unwrap_or(ChildProbe::Alive);
    let members = match os.list_group(pgid) {
        Ok(members) => members,
        Err(_) => {
            if let Some(lease) = owned {
                lease.invalidate();
            }
            return StopOutcome::Unproven {
                residual_pids: remaining,
            };
        }
    };
    let leader_gone = matches!(probe_after_kill, ChildProbe::Exited { .. });
    let remaining: Vec<i32> = members
        .into_iter()
        .filter(|member| if *member == pid { !leader_gone } else { true })
        .collect();
    if !remaining.is_empty() {
        return StopOutcome::Unproven {
            residual_pids: remaining,
        };
    }
    finish_stopped(owned, pid, os, escalated)
}

/// 清空確認後：撤銷 lease（如有）→ reap。reap 失敗即 `Unproven`，不宣稱 `Stopped`。
/// `RecordedLeader` 活體成功路徑同樣 reap：recovery 的 daemon 本就是該 child
/// 的父程序，回收是其職責（zombie 情境仍在步驟 4 直接 `Unproven`，不到此處）。
fn finish_stopped(
    owned: Option<&OwnedLeaderLease>,
    pid: i32,
    os: &dyn OsAdapter,
    escalated_to_kill: bool,
) -> StopOutcome {
    if let Some(lease) = owned {
        lease.revoke_production();
    }
    if os.reap(pid).is_err() {
        if let Some(lease) = owned {
            lease.invalidate();
        }
        return StopOutcome::Unproven {
            residual_pids: Vec::new(),
        };
    }
    StopOutcome::Stopped { escalated_to_kill }
}

/// 有界等待（Instant 迴圈＋POLL 間隔；fake 層以靜態腳本模擬已等待後狀態）。
fn wait_quiet(duration: Duration) {
    let begin = Instant::now();
    while begin.elapsed() < duration {
        std::thread::sleep(POLL_STEP);
    }
}

/// 公開入口：同一 production core，以真實 adapter 執行。
pub fn stop_group(
    pid: i32,
    expected_start: u64,
    evidence: GroupEvidence<'_>,
    term_wait: Duration,
    kill_wait: Duration,
) -> StopOutcome {
    let os = RealOs;
    stop_group_with_adapter(pid, expected_start, evidence, term_wait, kill_wait, &os)
}
#[cfg(test)]
mod ownership_tests {
    use super::{ChildProbe, GroupEvidence, OsAdapter, OwnedLeaderLease, StopOutcome};
    use std::collections::VecDeque;
    use std::io::ErrorKind;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    const SHORT_WAIT: Duration = Duration::from_millis(10);
    const POLL_STEP: Duration = Duration::from_millis(20);
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// 腳本錯誤（可 Clone）：ECHILD 攜帶 raw errno，production 以此判定
    /// ownership loss；暫時性錯誤另行覆蓋。
    #[derive(Debug, Clone, Copy)]
    enum StepErr {
        Echild,
        Transient,
    }

    impl StepErr {
        fn into_io(self, ctx: &'static str) -> std::io::Error {
            match self {
                StepErr::Echild => std::io::Error::from_raw_os_error(libc::ECHILD),
                StepErr::Transient => std::io::Error::new(ErrorKind::TimedOut, ctx),
            }
        }
    }

    /// 可 Clone 的腳本值（`io::Error` 不實作 `Clone`，不可直接存 Result）。
    #[derive(Debug, Clone)]
    enum Script<T> {
        Ok(T),
        Err(StepErr),
    }

    impl<T> Script<T> {
        fn into_io(self, ctx: &'static str) -> std::io::Result<T> {
            match self {
                Script::Ok(value) => Ok(value),
                Script::Err(error) => Err(error.into_io(ctx)),
            }
        }
    }

    /// 可逾時的 gate。注意：`FakeOs` 仍持有 `Arc<Gate>` 時本體的 `Drop`
    /// 不會觸發，測試另用 `GateOpener` 在 panic 路徑主動放行。
    struct Gate {
        open: Mutex<bool>,
        cvar: Condvar,
    }

    impl Gate {
        fn closed() -> Self {
            Self {
                open: Mutex::new(false),
                cvar: Condvar::new(),
            }
        }

        fn open(&self) {
            *self.open.lock().expect("繁中說明：gate 鎖不應中毒") = true;
            self.cvar.notify_all();
        }

        fn wait_open(&self, timeout: Duration) -> bool {
            let (guard, _) = self
                .cvar
                .wait_timeout_while(
                    self.open.lock().expect("繁中說明：gate 鎖不應中毒"),
                    timeout,
                    |is_open| !*is_open,
                )
                .expect("繁中說明：gate 鎖不應中毒");
            *guard
        }
    }

    /// 作用域放行守衛：持有 `Arc<Gate>` 的複本，自身 Drop 時主動 `open`。
    /// 測試函式內以 `let _opener = GateOpener(...)` 持有，任何 assert panic
    /// 展開時都會放行被 gate 擋住的執行緒，不掛住整個 suite。
    struct GateOpener(Arc<Gate>);

    impl Drop for GateOpener {
        fn drop(&mut self) {
            self.0.open();
        }
    }

    /// 可程式化的 fake OS adapter：照腳本回應查詢，記錄所有 signal／reap。
    /// 所有腳本用盡後重複最後一個值。
    struct FakeOs {
        starts: Mutex<VecDeque<Script<u64>>>,
        pgids: Mutex<VecDeque<Script<i32>>>,
        probes: Mutex<VecDeque<Script<ChildProbe>>>,
        lists: Mutex<VecDeque<Script<Vec<i32>>>>,
        reaps: Mutex<VecDeque<Script<()>>>,
        events: Mutex<Vec<String>>,
        kill_calls: AtomicUsize,
        reap_calls: AtomicUsize,
        /// 在第 `gate_at` 次 `kill_group` 前等待 gate（併發測試用）。
        gate: Mutex<Option<(Arc<Gate>, usize)>>,
        kill_count: AtomicUsize,
        /// 到達 signal 擋點的次數（阻塞之前累計；與「已發 signal」分開記，
        /// 不混淆事件語意）。
        entered_gate: AtomicUsize,
    }

    impl FakeOs {
        fn new(
            starts: Vec<Script<u64>>,
            pgids: Vec<Script<i32>>,
            probes: Vec<Script<ChildProbe>>,
            lists: Vec<Script<Vec<i32>>>,
            reaps: Vec<Script<()>>,
        ) -> Self {
            Self {
                starts: Mutex::new(starts.into()),
                pgids: Mutex::new(pgids.into()),
                probes: Mutex::new(probes.into()),
                lists: Mutex::new(lists.into()),
                reaps: Mutex::new(reaps.into()),
                events: Mutex::new(Vec::new()),
                kill_calls: AtomicUsize::new(0),
                reap_calls: AtomicUsize::new(0),
                gate: Mutex::new(None),
                kill_count: AtomicUsize::new(0),
                entered_gate: AtomicUsize::new(0),
            }
        }

        /// 一致的正面腳本：驗證時 Alive，TERM 後 Exited，group 已空。
        /// reap 前 probe 必已退出，不鼓勵對存活 child reap。
        fn happy() -> Self {
            Self::new(
                vec![Script::Ok(9_999_000_000)],
                vec![Script::Ok(4242)],
                vec![
                    Script::Ok(ChildProbe::Alive),
                    Script::Ok(ChildProbe::Exited {
                        code: Some(0),
                        signal: None,
                    }),
                ],
                vec![Script::Ok(Vec::new())],
                vec![Script::Ok(())],
            )
        }

        fn pop_script<T: Clone>(queue: &Mutex<VecDeque<Script<T>>>) -> Script<T> {
            let mut guard = queue.lock().expect("繁中說明：fake adapter 鎖不應中毒");
            if guard.len() > 1 {
                guard.pop_front().expect("繁中說明：腳本不應為空")
            } else {
                guard.front().cloned().expect("繁中說明：腳本不應為空")
            }
        }

        fn events_snapshot(&self) -> Vec<String> {
            self.events
                .lock()
                .expect("繁中說明：fake adapter 鎖不應中毒")
                .clone()
        }
    }

    impl OsAdapter for FakeOs {
        fn start_time(&self, _pid: i32) -> std::io::Result<u64> {
            Self::pop_script(&self.starts).into_io("start_time")
        }

        fn pgid(&self, _pid: i32) -> std::io::Result<i32> {
            Self::pop_script(&self.pgids).into_io("pgid")
        }

        fn probe_child(&self, _pid: i32) -> std::io::Result<ChildProbe> {
            Self::pop_script(&self.probes).into_io("probe_child")
        }

        fn list_group(&self, _pgid: i32) -> std::io::Result<Vec<i32>> {
            Self::pop_script(&self.lists).into_io("list_group")
        }

        fn kill_group(&self, pgid: i32, sig: libc::c_int) -> std::io::Result<()> {
            let count = self.kill_count.fetch_add(1, Ordering::SeqCst);
            if let Some((gate, at)) = self
                .gate
                .lock()
                .expect("繁中說明：fake adapter 鎖不應中毒")
                .clone()
            {
                if count == at {
                    // 先記到達（阻塞之前），再等待放行；到達計數不等於已發 signal。
                    self.entered_gate.fetch_add(1, Ordering::SeqCst);
                    if !gate.wait_open(TEST_TIMEOUT) {
                        return Err(std::io::Error::new(ErrorKind::TimedOut, "gate 等待逾時"));
                    }
                }
            }
            self.kill_calls.fetch_add(1, Ordering::SeqCst);
            let name = if sig == libc::SIGTERM { "TERM" } else { "KILL" };
            self.events
                .lock()
                .expect("繁中說明：fake adapter 鎖不應中毒")
                .push(format!("signal {name} pgid={pgid}"));
            Ok(())
        }

        fn reap(&self, _pid: i32) -> std::io::Result<()> {
            self.reap_calls.fetch_add(1, Ordering::SeqCst);
            self.events
                .lock()
                .expect("繁中說明：fake adapter 鎖不應中毒")
                .push("reap".to_owned());
            Self::pop_script(&self.reaps).into_io("reap")
        }
    }

    fn lease_for_test() -> OwnedLeaderLease {
        OwnedLeaderLease::for_test(4242, 9_999_000_000, 4242)
    }

    fn stop_once(
        pid: i32,
        start: u64,
        evidence: GroupEvidence<'_>,
        os: &dyn OsAdapter,
    ) -> StopOutcome {
        super::stop_group_with_adapter(pid, start, evidence, SHORT_WAIT, SHORT_WAIT, os)
    }

    #[test]
    fn valid_lease_signals_term_and_reaps() {
        // 正面案例：身分相符、TERM 後 probe 已退出、group 已空 → Stopped（不升級）。
        let os = FakeOs::happy();
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(
                outcome,
                StopOutcome::Stopped {
                    escalated_to_kill: false
                }
            ),
            "繁中說明：有效 lease 配合 TERM 應回 Stopped 且不升級，實際為 {outcome:?}"
        );
        assert_eq!(
            os.events_snapshot(),
            vec!["signal TERM pgid=4242".to_owned(), "reap".to_owned()],
            "繁中說明：事件全序應為 validation→signal→revoke→reap"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：清理完成應 reap 恰一次"
        );
    }

    #[test]
    fn revoked_lease_on_reused_group_makes_zero_signals() {
        // 同一舊 pgid 回報無關的新 group（啟動時間不同），且 lease 已撤銷：
        // 由真正 stop 核心入口帶入 → Unproven，signal 與 reap 呼叫數皆為 0。
        let mut os = FakeOs::happy();
        os.starts = Mutex::new(vec![Script::Ok(1_111_000_000)].into());
        let lease = lease_for_test();
        lease.revoke();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(outcome, StopOutcome::Unproven { .. }),
            "繁中說明：已撤銷 lease 遇重用 group 應回 Unproven，實際為 {outcome:?}"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：撤銷後不得發任何 signal"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：撤銷後不得 reap"
        );
    }

    #[test]
    fn external_reap_with_matching_start_and_pgid_permanently_revokes() {
        // 關鍵：start／pgid 始終相符，但 probe 回 ECHILD（child 已被外部 reap）。
        // ECHILD 映射攜帶 raw errno（production 以 raw errno 判定 ownership，
        // 不可把所有 NotFound 混為一談）；lease 永久撤銷；之後 probe 恢復
        // Alive，同一 lease 再次 stop 仍拒絕且零新 signal。
        assert_eq!(
            StepErr::Echild.into_io("probe").raw_os_error(),
            Some(libc::ECHILD),
            "繁中說明：ECHILD 測試映射必須攜帶 raw errno"
        );
        let os = FakeOs::new(
            vec![Script::Ok(9_999_000_000)],
            vec![Script::Ok(4242)],
            vec![Script::Err(StepErr::Echild), Script::Ok(ChildProbe::Alive)],
            vec![Script::Ok(vec![4242])],
            vec![Script::Ok(())],
        );
        let lease = lease_for_test();
        let first = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(first, StopOutcome::Unproven { .. }),
            "繁中說明：probe ECHILD 應回 Unproven（不可 signal 無主 child），實際為 {first:?}"
        );
        let second = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(
                second,
                StopOutcome::Unproven { .. } | StopOutcome::IdentityMismatch
            ),
            "繁中說明：失效是永久的，probe 恢復後仍應拒絕，實際為 {second:?}"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：失效路徑全程不得發 signal"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：失效路徑不得 reap"
        );
    }

    #[test]
    fn start_and_pgid_lookup_failures_are_unproven_not_stopped() {
        // start／pgid 暫時性查詢失敗同樣 fail-closed：Unproven、零 signal，
        // 且不得回 Stopped；查詢恢復後再次 stop 仍拒絕且零新 signal（永久撤銷）。
        let os_start = FakeOs::new(
            vec![Script::Err(StepErr::Transient), Script::Ok(9_999_000_000)],
            vec![Script::Ok(4242)],
            vec![Script::Ok(ChildProbe::Alive)],
            vec![Script::Ok(Vec::new())],
            vec![Script::Ok(())],
        );
        let lease = lease_for_test();
        let outcome = stop_once(
            4242,
            9_999_000_000,
            GroupEvidence::OwnedLeader(&lease),
            &os_start,
        );
        assert!(
            matches!(outcome, StopOutcome::Unproven { .. }),
            "繁中說明：start 查詢失敗應回 Unproven 而非 Stopped，實際為 {outcome:?}"
        );
        let retry = stop_once(
            4242,
            9_999_000_000,
            GroupEvidence::OwnedLeader(&lease),
            &os_start,
        );
        assert!(
            matches!(
                retry,
                StopOutcome::Unproven { .. } | StopOutcome::IdentityMismatch
            ),
            "繁中說明：查詢恢復後仍應拒絕（永久撤銷），實際為 {retry:?}"
        );
        assert_eq!(
            os_start.kill_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：start 查詢失敗路徑全程不得發 signal"
        );

        let os_pgid = FakeOs::new(
            vec![Script::Ok(9_999_000_000)],
            vec![Script::Err(StepErr::Transient), Script::Ok(4242)],
            vec![Script::Ok(ChildProbe::Alive)],
            vec![Script::Ok(Vec::new())],
            vec![Script::Ok(())],
        );
        let lease = lease_for_test();
        let outcome = stop_once(
            4242,
            9_999_000_000,
            GroupEvidence::OwnedLeader(&lease),
            &os_pgid,
        );
        assert!(
            matches!(outcome, StopOutcome::Unproven { .. }),
            "繁中說明：pgid 查詢失敗應回 Unproven 而非 Stopped，實際為 {outcome:?}"
        );
        let retry = stop_once(
            4242,
            9_999_000_000,
            GroupEvidence::OwnedLeader(&lease),
            &os_pgid,
        );
        assert!(
            matches!(
                retry,
                StopOutcome::Unproven { .. } | StopOutcome::IdentityMismatch
            ),
            "繁中說明：查詢恢復後仍應拒絕（永久撤銷），實際為 {retry:?}"
        );
        assert_eq!(
            os_pgid.kill_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：pgid 查詢失敗路徑全程不得發 signal"
        );
    }

    #[test]
    fn lease_binding_mismatch_makes_zero_signals() {
        // lease 綁定 (4242／start／4242)，傳入 pid 4243：綁定不符 → IdentityMismatch，
        // 即使 adapter 回傳相符值也不得 signal（production 不得以傳入參數覆蓋綁定）。
        let os = FakeOs::happy();
        let lease = lease_for_test();
        let outcome = stop_once(4243, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(outcome, StopOutcome::IdentityMismatch),
            "繁中說明：lease 綁定 pid 與傳入不符應回 IdentityMismatch，實際為 {outcome:?}"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：綁定不符不得發任何 signal"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：綁定不符不得 reap"
        );
    }

    #[test]
    fn group_enumeration_failure_is_unproven_with_term_already_sent() {
        // TERM 已發、等待後列舉 group 失敗：Unproven（不可 Stopped），不 reap。
        let os = FakeOs::new(
            vec![Script::Ok(9_999_000_000)],
            vec![Script::Ok(4242)],
            vec![Script::Ok(ChildProbe::Alive)],
            vec![Script::Err(StepErr::Transient)],
            vec![Script::Ok(())],
        );
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(outcome, StopOutcome::Unproven { .. }),
            "繁中說明：group 列舉失敗應回 Unproven 而非 Stopped，實際為 {outcome:?}"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：列舉失敗前 TERM 已發一次"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：列舉失敗不得 reap"
        );
    }

    #[test]
    fn empty_list_with_live_probe_is_unproven() {
        // 列舉為空但 probe 仍 Alive：證據矛盾 → Unproven、不 reap（有界等待內判定）。
        let os = FakeOs::new(
            vec![Script::Ok(9_999_000_000)],
            vec![Script::Ok(4242)],
            vec![Script::Ok(ChildProbe::Alive)],
            vec![Script::Ok(Vec::new())],
            vec![Script::Ok(())],
        );
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(outcome, StopOutcome::Unproven { .. }),
            "繁中說明：空列舉但 probe 存活屬證據矛盾，應回 Unproven，實際為 {outcome:?}"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：證據矛盾不得 reap 存活 child"
        );
    }

    #[test]
    fn reap_failure_is_unproven_not_stopped() {
        // group 已確認清空（probe 已退出）但 reap 失敗：不可宣稱 Stopped；
        // lease 同樣 fail-closed，之後重試仍拒絕且無新 signal。
        let os = FakeOs::new(
            vec![Script::Ok(9_999_000_000)],
            vec![Script::Ok(4242)],
            vec![
                Script::Ok(ChildProbe::Alive),
                Script::Ok(ChildProbe::Exited {
                    code: Some(0),
                    signal: None,
                }),
            ],
            vec![Script::Ok(Vec::new())],
            vec![Script::Err(StepErr::Transient)],
        );
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(outcome, StopOutcome::Unproven { .. }),
            "繁中說明：reap 失敗應回 Unproven 而非 Stopped，實際為 {outcome:?}"
        );
        let retry = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(
                retry,
                StopOutcome::Unproven { .. } | StopOutcome::IdentityMismatch
            ),
            "繁中說明：reap 失敗後 lease 應永久失效，實際為 {retry:?}"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：重試不得發新 signal"
        );
    }

    #[test]
    fn live_leader_is_never_excluded_from_members() {
        // probe 始終 Alive，列舉含 leader 本人：不可盲目排除存活 leader；
        // TERM＋KILL 後仍有成員 → Unproven，residuals 含 leader 與 child，不 reap。
        let os = FakeOs::new(
            vec![Script::Ok(9_999_000_000)],
            vec![Script::Ok(4242)],
            vec![Script::Ok(ChildProbe::Alive)],
            vec![Script::Ok(vec![4242, 5000])],
            vec![Script::Ok(())],
        );
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        match outcome {
            StopOutcome::Unproven { residual_pids } => {
                assert!(
                    residual_pids.contains(&4242) && residual_pids.contains(&5000),
                    "繁中說明：residuals 應含存活 leader 與 child，實際為 {residual_pids:?}"
                );
            }
            other => panic!("繁中說明：存活 leader 未清空應回 Unproven，實際為 {other:?}"),
        }
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            2,
            "繁中說明：應發 TERM 後升級 KILL"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：group 未清空不得 reap"
        );
    }

    #[test]
    fn exited_owned_leader_is_excluded_after_cleanup() {
        // probe 確認 leader 已退出：列舉第二次只剩 zombie leader 時，排除它即算
        // 清空 → Stopped（不升級）並 reap。
        let os = FakeOs::new(
            vec![Script::Ok(9_999_000_000)],
            vec![Script::Ok(4242)],
            vec![Script::Ok(ChildProbe::Exited {
                code: Some(0),
                signal: None,
            })],
            vec![Script::Ok(vec![4242, 5001]), Script::Ok(vec![4242])],
            vec![Script::Ok(())],
        );
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(
                outcome,
                StopOutcome::Stopped {
                    escalated_to_kill: false
                }
            ),
            "繁中說明：只剩已退出 leader 時應回 Stopped 且不升級，實際為 {outcome:?}"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：應只發一次 TERM"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：清理完成應 reap 恰一次"
        );
    }

    #[test]
    fn concurrent_stops_serialize_on_one_lock_with_exactly_one_reap() {
        // 真正 production 同一 lifecycle lock 的 revoke／reap 競爭：stop A 在
        // validation 之後、signal 之前被可控 seam（gate）擋住並持有鎖；stop B
        // 以同一 lease 進入必須序列化等待。放行後事件全序為 signal→reap，
        // reap 恰一次，A 得 Stopped、B 因 lease 已撤銷得 Unproven，撤銷後零新
        // signal。全程用可逾時 gate／channel；`GateOpener` 保證任何 panic 路徑
        // 都主動放行（不依賴 `Gate::drop`，因 `FakeOs` 仍持有 `Arc`）。
        let os = Arc::new(FakeOs::happy());
        let gate = Arc::new(Gate::closed());
        *os.gate.lock().expect("繁中說明：fake adapter 鎖不應中毒") = Some((Arc::clone(&gate), 0));
        let _opener = GateOpener(Arc::clone(&gate));
        let lease = Arc::new(lease_for_test());

        let spawn_stop = |os: Arc<FakeOs>, lease: Arc<OwnedLeaderLease>| {
            let (tx, rx) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                let outcome = super::stop_group_with_adapter(
                    4242,
                    9_999_000_000,
                    GroupEvidence::OwnedLeader(&lease),
                    SHORT_WAIT,
                    SHORT_WAIT,
                    &*os,
                );
                let _ = tx.send(outcome);
            });
            (handle, rx)
        };

        let (handle_a, rx_a) = spawn_stop(Arc::clone(&os), Arc::clone(&lease));
        // 以獨立到達計數確認 A 已進入 signal 擋點（持有鎖），再啟動 B 保證重疊；
        // 到達計數不等於已發 signal，不混淆事件語意。
        let checkpoint = Instant::now();
        while os.entered_gate.load(Ordering::SeqCst) < 1 {
            assert!(
                checkpoint.elapsed() < TEST_TIMEOUT,
                "繁中說明：stop A 應在期限內到達 signal 擋點"
            );
            std::thread::sleep(POLL_STEP);
        }
        let (handle_b, rx_b) = spawn_stop(Arc::clone(&os), Arc::clone(&lease));
        gate.open();

        let outcome_a = rx_a
            .recv_timeout(TEST_TIMEOUT)
            .expect("繁中說明：stop A 應在期限內完成");
        let outcome_b = rx_b
            .recv_timeout(TEST_TIMEOUT)
            .expect("繁中說明：stop B 應在期限內完成（鎖不可死鎖）");
        handle_a.join().expect("繁中說明：執行緒 A 應正常結束");
        handle_b.join().expect("繁中說明：執行緒 B 應正常結束");

        assert!(
            matches!(
                outcome_a,
                StopOutcome::Stopped {
                    escalated_to_kill: false
                }
            ),
            "繁中說明：先驗證的 A 應回 Stopped 且不升級，實際為 {outcome_a:?}"
        );
        assert!(
            matches!(outcome_b, StopOutcome::Unproven { .. }),
            "繁中說明：後到的 B 應因 lease 已撤銷回 Unproven，實際為 {outcome_b:?}"
        );
        assert_eq!(
            os.events_snapshot(),
            vec!["signal TERM pgid=4242".to_owned(), "reap".to_owned()],
            "繁中說明：全序應為 validation→signal→revoke→reap，無交錯 signal"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：全程應只發一次 signal"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：reap 應恰一次"
        );
    }

    #[test]
    fn no_exit_event_never_excludes_leader() {
        // probe 始終 Alive，即 waitid 回 0 但 si_pid==0（尚無 exit 事件）：
        // 即使列舉僅含 leader 本人，也不得把它當已退出排除 → Unproven，
        // residuals 含 leader，零 reap。
        let os = FakeOs::new(
            vec![Script::Ok(9_999_000_000)],
            vec![Script::Ok(4242)],
            vec![Script::Ok(ChildProbe::Alive)],
            vec![Script::Ok(vec![4242])],
            vec![Script::Ok(())],
        );
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        match outcome {
            StopOutcome::Unproven { residual_pids } => {
                assert!(
                    residual_pids.contains(&4242),
                    "繁中說明：無 exit 事件時 residuals 應含 leader，實際為 {residual_pids:?}"
                );
            }
            other => panic!("繁中說明：無 exit 事件不得授權排除，實際為 {other:?}"),
        }
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            2,
            "繁中說明：應發 TERM 後升級 KILL（leader 未清空）"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：無 exit 事件不得 reap"
        );
    }

    #[test]
    fn revocation_beats_waitability() {
        // probe 已 Exited（zombie 仍 waitable），但 lease 已撤銷：
        // 撤銷檢查優先於一切驗證 → Unproven，零 signal、零 reap。
        let os = FakeOs::new(
            vec![Script::Ok(9_999_000_000)],
            vec![Script::Ok(4242)],
            vec![Script::Ok(ChildProbe::Exited {
                code: Some(0),
                signal: None,
            })],
            vec![Script::Ok(vec![4242])],
            vec![Script::Ok(())],
        );
        let lease = lease_for_test();
        lease.revoke();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(outcome, StopOutcome::Unproven { .. }),
            "繁中說明：已撤銷 lease 即使 zombie 可 wait 仍應回 Unproven，實際為 {outcome:?}"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：撤銷後不得發任何 signal"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：撤銷後不得 reap"
        );
    }

    #[test]
    fn recorded_leader_never_owns_zombie_cleanup() {
        // RecordedLeader＋probe Exited＋start 相符＋列舉僅 zombie → 仍 Unproven。
        // OS 查得到身分也不構成 ownership：只有 owned lease 可授權清理。
        let os = FakeOs::new(
            vec![Script::Ok(9_999_000_000)],
            vec![Script::Ok(4242)],
            vec![Script::Ok(ChildProbe::Exited {
                code: Some(0),
                signal: None,
            })],
            vec![Script::Ok(vec![4242])],
            vec![Script::Ok(())],
        );
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::RecordedLeader, &os);
        assert!(
            matches!(outcome, StopOutcome::Unproven { .. }),
            "繁中說明：RecordedLeader 永不授權 zombie 清理，實際為 {outcome:?}"
        );
        assert_eq!(
            os.kill_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：RecordedLeader 不得發任何 signal"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：RecordedLeader 不得 reap（非本 daemon 持有的 child）"
        );
    }

    #[test]
    fn zombie_branch_treats_esrch_pgid_as_expected() {
        // probe 已退出、pgid 查回 raw-errno ECHILD → 仍可 Stopped（不升級）。
        // zombie 的 getpgid 回 ESRCH 為預期，不當查詢失敗；production 必須以
        // raw errno 區分 ECHILD 與暫時性失敗，不可把所有錯誤混為一談。
        assert_eq!(
            StepErr::Echild.into_io("pgid").raw_os_error(),
            Some(libc::ECHILD),
            "繁中說明：pgid 的 ECHILD 映射必須攜帶 raw errno"
        );
        let os = FakeOs::new(
            vec![Script::Ok(9_999_000_000)],
            vec![Script::Err(StepErr::Echild)],
            vec![Script::Ok(ChildProbe::Exited {
                code: Some(0),
                signal: None,
            })],
            vec![Script::Ok(vec![4242])],
            vec![Script::Ok(())],
        );
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(
                outcome,
                StopOutcome::Stopped {
                    escalated_to_kill: false
                }
            ),
            "繁中說明：zombie 分支 pgid ESRCH 為預期，應回 Stopped 且不升級，實際為 {outcome:?}"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            1,
            "繁中說明：清理完成應 reap 恰一次"
        );
    }

    #[test]
    fn zombie_branch_with_transient_pgid_failure_is_unproven() {
        // 對照：同上但 pgid 為暫時性失敗 → Unproven（不可與 ESRCH 混為一談）。
        let os = FakeOs::new(
            vec![Script::Ok(9_999_000_000)],
            vec![Script::Err(StepErr::Transient)],
            vec![Script::Ok(ChildProbe::Exited {
                code: Some(0),
                signal: None,
            })],
            vec![Script::Ok(vec![4242])],
            vec![Script::Ok(())],
        );
        let lease = lease_for_test();
        let outcome = stop_once(4242, 9_999_000_000, GroupEvidence::OwnedLeader(&lease), &os);
        assert!(
            matches!(outcome, StopOutcome::Unproven { .. }),
            "繁中說明：pgid 暫時性失敗應回 Unproven 而非 Stopped，實際為 {outcome:?}"
        );
        assert_eq!(
            os.reap_calls.load(Ordering::SeqCst),
            0,
            "繁中說明：pgid 暫時性失敗不得 reap"
        );
    }
}
