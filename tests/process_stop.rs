//! T3 Lane A RED：程序停止的所有權與證據（`tests/process_stop.rs`）。
//!
//! 依 `docs/plans/p1a-implementation.md` T3 與契約 §4.3，只覆蓋 process／stop
//! 部分；terminal 顯示在 `tests/terminal_session.rs`。`revoked lease`、
//! `ECHILD 永久失效`、`validation/reap barrier` 與 fake-adapter 正面案例放在
//! `src/procs.rs` 的 test-only 私有單元測試。
//!
//! 全檔只在 macOS 編譯（契約 §1 平台範圍）。等待一律有上限並回報最後狀態，
//! 不靠任意 sleep；cleanup 在斷言前安排，隔離 HOME／OTTERHIVE_HOME／cwd。

#![cfg(target_os = "macos")]

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use otterhive::procs::{process_start_time, stop_group, GroupEvidence, StopOutcome};
use otterhive::terminal::session::{Session, SessionConfig};
use tempfile::TempDir;

const TERM_WAIT: Duration = Duration::from_secs(2);
const KILL_WAIT: Duration = Duration::from_secs(1);
const POLL_STEP: Duration = Duration::from_millis(20);
const LONG_TIMEOUT: Duration = Duration::from_secs(10);

mod support {
    use super::*;

    /// fixture 指令稿的絕對路徑。
    pub fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/backends")
            .join(name)
    }

    /// 在 `cond()` 為真前輪詢到 `timeout`；回傳最後觀測結果，不靠任意 sleep。
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

    /// pid 是否存活（`kill(pid, 0)` 探測，不發 signal）。
    pub fn pid_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// 以 `proc_pidinfo(PROC_PIDTBSDINFO)` 讀 pid 的啟動時間（秒＋微秒）。
    pub fn proc_start_raw(pid: i32) -> Option<u64> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let ret = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if ret == size {
            Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
        } else {
            None
        }
    }

    /// 緊急清理：只有在 leader 存活、啟動時間相符、`pgid == pid` 三者皆成立
    /// （契約 §4.3 的證據）時，才對該 group 發 SIGKILL；否則什麼都不做。
    /// 這是測試的最後防線，不是 `stop_group` 的替代路徑。
    pub fn emergency_kill_group(pid: i32, expected_start: u64) {
        let start = match proc_start_raw(pid) {
            Some(start) => start,
            None => return,
        };
        if start != expected_start {
            return;
        }
        let pgid = unsafe { libc::getpgid(pid) };
        if pgid != pid {
            return;
        }
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }

    /// 用 Session 啟動的 fixture 程序。Drop 時若尚未清理，以短等待做一次
    /// `stop_group`，保證失敗中途也有回收（不斷言，只做清理）。
    pub struct FixtureProc {
        pub session: Session,
        pub pid: i32,
        pub start: u64,
        pub log: PathBuf,
        cleaned: bool,
        // 隔離目錄與測試同壽命，Drop 時自動刪除。
        _tmp: TempDir,
    }

    impl FixtureProc {
        pub fn spawn(program: &str, extra_env: &[(&str, String)], subscriber_queue: usize) -> Self {
            let tmp = TempDir::new().expect("繁中說明：測試需建立隔離暫存目錄");
            let log = tmp.path().join("fixture.log");
            fs::write(&log, "").expect("繁中說明：測試需建立 fixture log 檔");
            let mut env = vec![
                (
                    "OTTERHIVE_FIXTURE_LOG".to_owned(),
                    log.to_string_lossy().into_owned(),
                ),
                ("HOME".to_owned(), tmp.path().to_string_lossy().into_owned()),
                (
                    "OTTERHIVE_HOME".to_owned(),
                    tmp.path().to_string_lossy().into_owned(),
                ),
            ];
            for (key, value) in extra_env {
                env.push(((*key).to_owned(), (*value).clone()));
            }
            let cfg = SessionConfig {
                program: fixture(program).to_string_lossy().into_owned(),
                args: Vec::new(),
                cwd: tmp.path().to_path_buf(),
                env,
                size: (120, 40),
                generation: 1,
                subscriber_queue,
            };
            let session = Session::spawn(cfg).expect("繁中說明：fixture 程序應可經 Session 啟動");
            let pid = session.pid();
            let start = session.start_time();
            Self {
                session,
                pid,
                start,
                log,
                cleaned: false,
                _tmp: tmp,
            }
        }

        /// spawns_child 是否已寫入子程序 pid（可解析為整數）。
        pub fn child_pid_ready(&self) -> bool {
            self.try_child_pid().is_some()
        }

        pub fn try_child_pid(&self) -> Option<i32> {
            let text = fs::read_to_string(&self.log).ok()?;
            text.trim().lines().next()?.trim().parse::<i32>().ok()
        }

        pub fn log_child_pid(&self) -> i32 {
            self.try_child_pid()
                .expect("繁中說明：spawns_child 應寫入子程序 pid")
        }

        /// 以 owned lease 停止；呼叫後 Drop 不再重複清理。
        pub fn stop_owned(&mut self) -> StopOutcome {
            let outcome = stop_group(
                self.pid,
                self.start,
                GroupEvidence::OwnedLeader(self.session.owned_leader_lease()),
                TERM_WAIT,
                KILL_WAIT,
            );
            self.cleaned = true;
            outcome
        }

        /// 外部已處理停止（如 RecordedLeader 路徑），Drop 不再重複清理。
        pub fn mark_cleaned(&mut self) {
            self.cleaned = true;
        }
    }

    impl Drop for FixtureProc {
        fn drop(&mut self) {
            if !self.cleaned {
                let _ = stop_group(
                    self.pid,
                    self.start,
                    GroupEvidence::OwnedLeader(self.session.owned_leader_lease()),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                );
            }
            emergency_kill_group(self.pid, self.start);
        }
    }
}

use support::*;

#[test]
fn spawned_child_leads_its_process_group() {
    let proc_ = FixtureProc::spawn("echo_agent.sh", &[], 256);
    let pgid = unsafe { libc::getpgid(proc_.pid) };
    assert_eq!(
        pgid, proc_.pid,
        "繁中說明：H1 portable-pty 子程序應為 session leader（pgid == pid）"
    );
    let mut proc_ = proc_;
    let outcome = proc_.stop_owned();
    assert!(
        matches!(outcome, StopOutcome::Stopped { .. }),
        "繁中說明：H1 測試結束應清理程序，實際為 {outcome:?}"
    );
}

#[test]
fn process_start_time_is_stable_for_a_live_pid() {
    let mut proc_ = FixtureProc::spawn("echo_agent.sh", &[], 256);
    let first = process_start_time(proc_.pid);
    let second = process_start_time(proc_.pid);
    assert!(
        first.is_some() && first == second,
        "繁中說明：H2 存活 pid 的啟動時間應穩定且兩次一致"
    );
    assert_eq!(
        first,
        Some(proc_.session.start_time()),
        "繁中說明：H2 process_start_time 應與 Session 記錄一致"
    );
    let outcome = proc_.stop_owned();
    assert!(
        matches!(outcome, StopOutcome::Stopped { .. }),
        "繁中說明：H2 測試結束應清理程序，實際為 {outcome:?}"
    );
}

#[test]
fn child_exit_is_observed() {
    let proc_ = FixtureProc::spawn("crash_on_start.sh", &[], 256);
    let exit = proc_
        .session
        .wait_exit(LONG_TIMEOUT)
        .expect("繁中說明：crash fixture 的 exit 應在期限內被觀測到");
    assert_eq!(
        exit.code,
        Some(3),
        "繁中說明：crash_on_start.sh 應回報 exit code 3"
    );
    assert_eq!(exit.signal, None, "繁中說明：正常 exit 不應帶 signal");
}

#[test]
fn stop_terminates_and_reaps() {
    let mut proc_ = FixtureProc::spawn("echo_agent.sh", &[], 256);
    let outcome = proc_.stop_owned();
    assert!(
        matches!(
            outcome,
            StopOutcome::Stopped {
                escalated_to_kill: false
            }
        ),
        "繁中說明：配合 SIGTERM 的程序應直接 Stopped 且不升級，實際為 {outcome:?}"
    );
    assert!(
        poll_until(LONG_TIMEOUT, || !pid_alive(proc_.pid)),
        "繁中說明：stop 後 leader pid 應消失（已 reap）"
    );
}

#[test]
fn stop_escalates_to_kill_when_term_is_ignored() {
    let mut proc_ = FixtureProc::spawn("ignore_term.sh", &[], 256);
    let outcome = stop_group(
        proc_.pid,
        proc_.start,
        GroupEvidence::OwnedLeader(proc_.session.owned_leader_lease()),
        Duration::from_secs(1),
        Duration::from_secs(1),
    );
    let _ = proc_.stop_owned();
    assert!(
        matches!(
            outcome,
            StopOutcome::Stopped {
                escalated_to_kill: true
            }
        ),
        "繁中說明：忽略 SIGTERM 的程序應升級到 SIGKILL，實際為 {outcome:?}"
    );
}

#[test]
fn stop_removes_children_in_the_same_group() {
    let mut proc_ = FixtureProc::spawn("spawns_child.sh", &[], 256);
    assert!(
        poll_until(LONG_TIMEOUT, || proc_.child_pid_ready()),
        "繁中說明：spawns_child 應先寫入子程序 pid"
    );
    let child = proc_.log_child_pid();
    assert!(pid_alive(child), "繁中說明：stop 前子程序應存活");
    let outcome = proc_.stop_owned();
    assert!(
        matches!(outcome, StopOutcome::Stopped { .. }),
        "繁中說明：同 group 停止應回 Stopped，實際為 {outcome:?}"
    );
    assert!(
        poll_until(LONG_TIMEOUT, || !pid_alive(child)),
        "繁中說明：stop 後同 group 的子程序應不存在"
    );
}

#[test]
fn leader_exit_observation_keeps_the_owned_child_unreaped() {
    let mut proc_ = FixtureProc::spawn(
        "spawns_child.sh",
        &[("FIXTURE_LEADER_EXIT_AFTER_MS", "1500".to_owned())],
        256,
    );
    let exit = proc_
        .session
        .wait_exit(LONG_TIMEOUT)
        .expect("繁中說明：H8 leader 結束應在期限內被觀測到");
    assert_eq!(exit.code, Some(0), "繁中說明：leader 應以 exit 0 結束");

    // H8 核心：以 WNOWAIT 重複觀測同一 child，必須成功且非 ECHILD。
    for _ in 0..2 {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            libc::waitid(
                libc::P_PID,
                proc_.pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        assert_eq!(
            ret, 0,
            "繁中說明：H8 WNOWAIT 重複觀測不得回 ECHILD（未被提前 reap）"
        );
        assert_eq!(
            info.si_pid, proc_.pid,
            "繁中說明：H8 觀測到的應為同一 leader pid"
        );
        assert_eq!(
            info.si_code,
            libc::CLD_EXITED,
            "繁中說明：H8 應觀測到 CLD_EXITED"
        );
        assert_eq!(info.si_status, 0, "繁中說明：H8 exit status 應為 0");
    }

    // lease 證據仍可重新驗證：pid／啟動時間／pgid 相符。
    assert_eq!(
        process_start_time(proc_.pid),
        Some(proc_.start),
        "繁中說明：H8 leader zombie 的啟動時間應仍可查證"
    );
    assert_eq!(
        unsafe { libc::getpgid(proc_.pid) },
        proc_.pid,
        "繁中說明：H8 zombie leader 的 pgid 應仍等於 pid"
    );

    let outcome = proc_.stop_owned();
    assert!(
        matches!(outcome, StopOutcome::Stopped { .. }),
        "繁中說明：H8 清理剩餘 group 後應回 Stopped，實際為 {outcome:?}"
    );
}

#[test]
fn stop_after_leader_exit_cleans_remaining_group_members() {
    let mut proc_ = FixtureProc::spawn(
        "spawns_child.sh",
        &[("FIXTURE_LEADER_EXIT_AFTER_MS", "500".to_owned())],
        256,
    );
    assert!(
        poll_until(LONG_TIMEOUT, || proc_.child_pid_ready()),
        "繁中說明：測試需先取得殘留子程序 pid"
    );
    let child = proc_.log_child_pid();
    let _ = proc_
        .session
        .wait_exit(LONG_TIMEOUT)
        .expect("繁中說明：leader 結束應先被觀測到");
    let outcome = proc_.stop_owned();
    assert!(
        matches!(outcome, StopOutcome::Stopped { .. }),
        "繁中說明：有效 lease 應授權清理剩餘 group，實際為 {outcome:?}"
    );
    assert!(
        poll_until(LONG_TIMEOUT, || !pid_alive(child)),
        "繁中說明：child 消失後才算清理完成"
    );
}

#[test]
fn recorded_leader_with_matching_live_leader_can_stop() {
    // 校正：RecordedLeader 在 leader 仍存活且 pid／啟動時間／pgid 相符時，
    // 可授權 recovery stop（契約 §4.3），不是一律拒絕。
    let mut proc_ = FixtureProc::spawn("echo_agent.sh", &[], 256);
    let outcome = stop_group(
        proc_.pid,
        proc_.start,
        GroupEvidence::RecordedLeader,
        TERM_WAIT,
        KILL_WAIT,
    );
    proc_.mark_cleaned();
    assert!(
        matches!(outcome, StopOutcome::Stopped { .. }),
        "繁中說明：身分相符的 recorded leader 應可授權 stop，實際為 {outcome:?}"
    );
    assert!(
        poll_until(LONG_TIMEOUT, || !pid_alive(proc_.pid)),
        "繁中說明：recovery stop 後程序應消失"
    );
}

#[test]
fn recorded_leader_without_an_owned_lease_cannot_authorize_a_nonempty_group() {
    // 歷史 leader 已清理／reap 但子程序殘留：只有 SQLite 歷史身分，
    // leader 不存在、group 非空 → 一律 Unproven，不發 signal。
    // fixture child 身分另外保存並核對（讀 log 的 pid＋啟動時間），cleanup
    // 只對該 child 發 signal，不以歷史 group 編號清理。
    let tmp = TempDir::new().expect("繁中說明：測試需建立隔離暫存目錄");
    let log = tmp.path().join("fixture.log");
    fs::write(&log, "").expect("繁中說明：測試需建立 fixture log 檔");
    let mut leader_cmd = Command::new(fixture("spawns_child.sh"));
    unsafe {
        leader_cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut leader = leader_cmd
        .env("OTTERHIVE_FIXTURE_LOG", &log)
        .env("HOME", tmp.path())
        .env("OTTERHIVE_HOME", tmp.path())
        .current_dir(tmp.path())
        .spawn()
        .expect("繁中說明：歷史 leader 應可啟動");
    let leader_pid = leader.id() as i32;
    let leader_start = proc_start_raw(leader_pid).expect("繁中說明：歷史 leader 啟動時間應可讀");
    assert!(
        poll_until(LONG_TIMEOUT, || {
            fs::read_to_string(&log)
                .ok()
                .and_then(|text| text.trim().lines().next()?.trim().parse::<i32>().ok())
                .is_some()
        }),
        "繁中說明：歷史 leader 應先留下子程序 pid"
    );
    let child: i32 = fs::read_to_string(&log)
        .expect("繁中說明：fixture log 應可讀取")
        .trim()
        .lines()
        .next()
        .expect("繁中說明：log 應含子程序 pid")
        .trim()
        .parse::<i32>()
        .expect("繁中說明：子程序 pid 應為整數");
    let child_start = proc_start_raw(child).expect("繁中說明：子程序啟動時間應可讀");

    // 模擬 daemon 重啟後的狀態：歷史 leader 已結束並被 reap（測試自己持有
    // Command 故合法 wait），只剩 SQLite 記的 pid／啟動時間。
    unsafe {
        libc::kill(leader_pid, libc::SIGKILL);
    }
    let _ = leader.wait();
    assert!(
        !pid_alive(leader_pid),
        "繁中說明：歷史 leader 應已消失（被 reap）"
    );
    assert!(pid_alive(child), "繁中說明：殘留子程序應仍存活");

    let outcome = stop_group(
        leader_pid,
        leader_start,
        GroupEvidence::RecordedLeader,
        Duration::from_secs(1),
        Duration::from_secs(1),
    );
    assert!(
        matches!(outcome, StopOutcome::Unproven { .. }),
        "繁中說明：無 owned lease 的非空歷史 group 應回 Unproven，實際為 {outcome:?}"
    );
    assert!(
        pid_alive(child),
        "繁中說明：Unproven 不得對殘留 child 發 signal"
    );

    // cleanup：核對保存的 child 身分（pid＋啟動時間）後才清理。
    assert_eq!(
        proc_start_raw(child),
        Some(child_start),
        "繁中說明：cleanup 前應核對子程序身分未變"
    );
    unsafe {
        libc::kill(child, libc::SIGKILL);
    }
    assert!(
        poll_until(LONG_TIMEOUT, || !pid_alive(child)),
        "繁中說明：測試結束應清理殘留子程序"
    );
}

#[test]
fn stop_refuses_when_start_time_does_not_match() {
    let mut proc_ = FixtureProc::spawn("echo_agent.sh", &[], 256);
    let outcome = stop_group(
        proc_.pid,
        proc_.start.wrapping_add(1),
        GroupEvidence::OwnedLeader(proc_.session.owned_leader_lease()),
        TERM_WAIT,
        KILL_WAIT,
    );
    assert!(
        matches!(outcome, StopOutcome::IdentityMismatch),
        "繁中說明：啟動時間不符應回 IdentityMismatch 且不發 signal，實際為 {outcome:?}"
    );
    assert!(
        pid_alive(proc_.pid),
        "繁中說明：IdentityMismatch 後程序必須仍存活（測試自行清理）"
    );
    let cleanup = proc_.stop_owned();
    assert!(
        matches!(cleanup, StopOutcome::Stopped { .. }),
        "繁中說明：測試結束應清理程序，實際為 {cleanup:?}"
    );
}
