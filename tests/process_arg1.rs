//! T3 Lane A RED 測試化：arg=0 vs arg=1 zombie 查詢＋disposition 前提。
//!
//! 純 OS 層（raw `Command`＋`libc`），不依賴 `Session`／`Screen`；
//! 在本工作樹可直接執行（非 compile-RED）。契約行為不變，只驗證查詢參數方向：
//! 同一 zombie 上 `PROC_PIDTBSDINFO arg=0` 回 ESRCH、`arg=1` 回 start／pgid；
//! 只有後者可用於重驗。disposition 前提在隔離 helper 內驗證，不動本行程。
//!
//! 全部等待有界、cleanup 核對 Some==Some、不用歷史 pgid／killall。
//! 全檔只在 macOS 編譯。

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const POLL_STEP: Duration = Duration::from_millis(20);
const SINGLE_LIMIT: Duration = Duration::from_secs(10);

mod support {
    use super::*;

    /// 以 flavor＋arg 查詢，只記錄 ret／errno／start／pgid，不推論未測欄位。
    /// 呼叫前先清 errno：成功時 errno 保持 0，失敗時為當次錯誤。
    pub fn query_bsdinfo(pid: i32, arg: u64) -> (i32, i32, Option<u64>, Option<u32>) {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        unsafe {
            *libc::__error() = 0;
        }
        let ret = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                arg,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        let errno = unsafe { *libc::__error() };
        if ret == size {
            let start = Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec);
            (ret, errno, start, Some(info.pbi_pgid))
        } else {
            (ret, errno, None, None)
        }
    }

    pub fn pid_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

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

    /// RAII 守衛：接管只移動 handle（無 OS 呼叫、無 panic）；fingerprint 回
    /// Result；Drop 全程無 panic，只在 Some==Some 相符時清理。
    pub struct RawGuard {
        child: Option<std::process::Child>,
        pub pid: i32,
        start: Option<u64>,
        _tmp: TempDir,
    }

    impl RawGuard {
        pub fn take_over(child: std::process::Child, tmp: TempDir) -> Self {
            let pid = child.id() as i32;
            Self {
                child: Some(child),
                pid,
                start: None,
                _tmp: tmp,
            }
        }

        /// 正面 fingerprint（arg=1）：讀到才存入；讀不到回 Err，主體 assert。
        pub fn fingerprint(&mut self) -> Result<u64, &'static str> {
            match query_bsdinfo(self.pid, 1).2 {
                Some(start) => {
                    self.start = Some(start);
                    Ok(start)
                }
                None => Err("fingerprint 不可讀，無法建立正面身分"),
            }
        }

        /// 有界回收（test 持有、可合法 reap）；失敗回 false，主體 assert。
        pub fn reap_bounded(&mut self, timeout: Duration) -> bool {
            let begin = Instant::now();
            loop {
                if let Some(child) = self.child.as_mut() {
                    match child.try_wait() {
                        Ok(Some(_)) => {
                            self.child = None;
                            return true;
                        }
                        Ok(None) => {}
                        Err(_) => return false,
                    }
                } else {
                    return true;
                }
                if begin.elapsed() >= timeout {
                    return false;
                }
                std::thread::sleep(POLL_STEP);
            }
        }
    }

    impl Drop for RawGuard {
        fn drop(&mut self) {
            if self.child.is_some() {
                let owned = match self.start {
                    Some(start) => query_bsdinfo(self.pid, 1).2 == Some(start),
                    None => false,
                };
                if owned {
                    unsafe {
                        libc::kill(self.pid, libc::SIGKILL);
                    }
                }
                let _ = self.reap_bounded(Duration::from_secs(2));
            }
        }
    }
}

use support::*;

#[test]
fn zombie_arg0_esrch_but_arg1_returns_identity() {
    // 同一 zombie：arg=0 回 ESRCH（不可用於重驗），arg=1 回 start／pgid。
    let tmp = TempDir::new().expect("繁中說明：測試需建立隔離暫存目錄");
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "exit 7"]);
    cmd.env("HOME", tmp.path());
    cmd.env("OTTERHIVE_HOME", tmp.path());
    cmd.current_dir(tmp.path());
    let child = cmd.spawn().expect("繁中說明：測試程序應可啟動");
    let mut guard = RawGuard::take_over(child, tmp);
    let pid = guard.pid;
    // 立即記錄（arg=1 在活體與 zombie 皆可查，值應穩定）。注意裸 Command
    // 未 setsid：child 的 pgid 是父行程的 group 而非自己（與 PTY 情境不同），
    // 故 pgid 只斷言「與記錄一致」，不斷言等於 pid。
    let recorded = guard
        .fingerprint()
        .expect("繁中說明：spawn 後 fingerprint 應可讀");
    let recorded_pgid = unsafe { libc::getpgid(pid) };
    println!("DIAG1 LIVE arg1: pid={pid} start=Some({recorded}) getpgid={recorded_pgid}");

    // waitid WNOWAIT 觀測 exit（有界），重複 3 次確認仍 waitable。
    let observed = poll_until(SINGLE_LIMIT, || {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        ret == 0 && info.si_pid == pid
    });
    assert!(observed, "繁中說明：應在 10s 內觀測到退出");
    for round in 0..3 {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        unsafe {
            *libc::__error() = 0;
        }
        let ret = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        let errno = unsafe { *libc::__error() };
        assert_eq!(ret, 0, "繁中說明：第 {round} 次 WNOWAIT 重複觀測應成功");
        assert_eq!(info.si_pid, pid, "繁中說明：應為同一 pid");
        assert!(
            info.si_code == libc::CLD_EXITED
                || info.si_code == libc::CLD_KILLED
                || info.si_code == libc::CLD_DUMPED,
            "繁中說明：si_code 應屬退出類"
        );
        assert_eq!(info.si_status, 7, "繁中說明：exit status 應為 7");
        assert_eq!(errno, 0, "繁中說明：成功觀測後 errno 應為 0");
    }

    // arg=0：不可用於重驗（ret 非完整、ESRCH、無身分）。
    let (ret0, errno0, start0, pgid0) = query_bsdinfo(pid, 0);
    let full = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    assert_ne!(
        ret0, full,
        "繁中說明：arg=0 對 zombie 應回非完整（不可用於重驗）"
    );
    assert_eq!(errno0, libc::ESRCH, "繁中說明：arg=0 對 zombie 應回 ESRCH");
    assert_eq!(start0, None, "繁中說明：arg=0 不得給出 start");
    assert_eq!(pgid0, None, "繁中說明：arg=0 不得給出 pgid");

    // arg=1：可用於重驗（完整、errno 0、start／pgid 與記錄一致）。
    let (ret1, errno1, start1, pgid1) = query_bsdinfo(pid, 1);
    assert_eq!(ret1, full, "繁中說明：arg=1 對 zombie 應回完整");
    assert_eq!(errno1, 0, "繁中說明：arg=1 成功時 errno 應為 0");
    assert_eq!(
        start1,
        Some(recorded),
        "繁中說明：arg=1 的 start 應與記錄一致"
    );
    assert_eq!(
        pgid1,
        Some(recorded_pgid as u32),
        "繁中說明：arg=1 的 pgid 應與記錄一致"
    );

    // 清理：test 持有、可合法 reap；有界收斂。
    assert!(
        guard.reap_bounded(SINGLE_LIMIT),
        "繁中說明：測試結束應回收 child"
    );
    assert!(
        !pid_alive(pid) || query_bsdinfo(pid, 1).2.is_none(),
        "繁中說明：回收後 pid 應消失或不可查"
    );
}

#[test]
fn sigign_disposition_auto_reaps_in_isolated_helper() {
    // SIGCHLD=SIG_IGN 下退出的 child 被自動回收：隔離 helper
    //（`sigign_probe.py`，以 `python3 -I` 執行；`dash` 的 `trap "" CHLD`
    // 是 no-op，不能用 shell 驗證此前提）內驗證，不動本行程 disposition。
    // helper 回報 ECHILD_AFTER_IGN 即前提成立，production 必須 fail-closed。
    let tmp = TempDir::new().expect("繁中說明：測試需建立隔離暫存目錄");
    let probe = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/backends")
        .join("sigign_probe.py");
    let mut cmd = Command::new("python3");
    cmd.args(["-I", &probe.to_string_lossy()]);
    cmd.env("HOME", tmp.path());
    cmd.env("OTTERHIVE_HOME", tmp.path());
    cmd.current_dir(tmp.path());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    let mut helper = cmd.spawn().expect("繁中說明：helper 應可啟動");
    // 有界等待 helper 結束（其自身約 1s；test 持有可合法 wait）。
    let helper_done = poll_until(SINGLE_LIMIT, || match helper.try_wait() {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(_) => true,
    });
    assert!(helper_done, "繁中說明：helper 應在 10s 內結束");
    let output = {
        use std::io::Read;
        let mut text = String::new();
        helper
            .stdout
            .take()
            .expect("繁中說明：helper stdout 應可取得")
            .read_to_string(&mut text)
            .expect("繁中說明：helper 輸出應可讀取");
        text
    };
    let status = helper.wait().expect("繁中說明：helper 應可回收");
    assert!(
        output.contains("ECHILD_AFTER_IGN"),
        "繁中說明：SIG_IGN 下 waitpid 應報 ECHILD，helper 輸出為 {output:?}"
    );
    assert!(
        status.success(),
        "繁中說明：前提成立時 helper 應 exit 0，實際為 {status:?}"
    );
    assert!(
        !output.contains("WAITED"),
        "繁中說明：自動回收後 waitpid 不得成功，helper 輸出為 {output:?}"
    );
}
