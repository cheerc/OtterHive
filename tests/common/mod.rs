//! T4 Lane B RED：測試共用骨架（暫存隔離的 home、有上限的輪詢、command 執行）。
//!
//! 這是**提案**，不是行為實作：只提供各 T4 測試檔共用的一次性 fixture 與
//! 有界等待，不含 daemon 啟停、socket 連線或任何生命週期行為。T5–T7 直接
//! 沿用同一骨架擴充。
//!
//! 全模組只在 macOS 編譯（契約 §1 平台範圍）。

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// 輪詢間隔：所有等待都用「有上限 + 回報最後狀態」，不靠任意 sleep。
pub const POLL_STEP: Duration = Duration::from_millis(20);

/// 長操作（啟動 daemon、stop 升級 KILL）的等待上限。
pub const LONG_TIMEOUT: Duration = Duration::from_secs(10);

/// 一般 socket／process 回應的等待上限。
pub const SHORT_TIMEOUT: Duration = Duration::from_secs(5);

/// 輪詢到 `cond()` 為真；逾時回最後一次結果（不 panic，呼叫端自行斷言）。
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

/// 在 `cond()` 為真前輪詢；逾時 panic 並附最後狀態（供「前提必須成立」用）。
///
/// `last_state` 在每次輪詢前更新，逾時時一併回報，避免只看到一句無資訊的
/// 「逾時」。
pub fn require_until(timeout: Duration, what: &str, mut cond: impl FnMut() -> String) {
    let mut last = String::from("（尚未取得狀態）");
    let start = Instant::now();
    loop {
        let current = cond();
        if current.is_empty() {
            return;
        }
        let _ = std::mem::replace(&mut last, current);
        if start.elapsed() >= timeout {
            panic!("繁中說明：{what} 應在期限內成立，最後狀態：{last}");
        }
        std::thread::sleep(POLL_STEP);
    }
}

/// 暫存 home：建立後即隔離 `OTTERHIVE_HOME`、`HOME` 與測試工作目錄。
///
/// Drop 時 `TempDir` 自動刪除；測試不需要（也不允許）碰正式的 `~/.otterhive`
/// 或 `~/.agend`（CLAUDE.md 邊界）。
pub struct TempHome {
    temp: TempDir,
    home: PathBuf,
    work: PathBuf,
}

impl TempHome {
    /// 建立隔離 home；`name` 只是子目錄名，避免測試間互相看到對方的目錄。
    pub fn new(name: &str) -> TempHome {
        let temp = TempDir::new().expect("繁中說明：測試需建立隔離暫存目錄");
        let home = temp.path().join(name).join("otterhive");
        let work = temp.path().join(name).join("work");
        std::fs::create_dir_all(&home).expect("繁中說明：測試需建立暫存 home");
        std::fs::create_dir_all(&work).expect("繁中說明：測試需建立暫存工作目錄");
        TempHome { temp, home, work }
    }

    /// 暫存目錄根（只給需要直接清理或檢查的測試）。
    pub fn root(&self) -> &Path {
        self.temp.path()
    }

    /// `OTTERHIVE_HOME` 應指向的 home 根目錄。
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// 測試工作目錄：與 home 隔離，fixture 寫檔案用。
    pub fn work(&self) -> &Path {
        &self.work
    }

    /// 暫存環境基底：隔離 `HOME` 與 `OTTERHIVE_HOME`。
    ///
    /// 不含任何 operator 憑證；需要 instance 憑證的測試另外從 bridge 環境
    /// 變數取得（契約 §5.3 不得把 operator 憑證放進 agent 環境）。
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                "HOME".to_owned(),
                self.temp.path().to_string_lossy().into_owned(),
            ),
            (
                "OTTERHIVE_HOME".to_owned(),
                self.home.to_string_lossy().into_owned(),
            ),
        ]
    }
}

/// 執行一支命令並等待結束（測試側的薄封裝）。
///
/// 回傳 `(exit_code, stdout, stderr)`；不 panic、不 unwrap，呼叫端自行斷言。
/// 啟不動命令時回 `(-1, "", 錯誤訊息)`，讓測試可以斷言「啟不動」而不是
/// 在輔助函式裡 panic。
pub fn run_command(
    program: &Path,
    args: &[&str],
    env: &[(String, String)],
) -> (i32, String, String) {
    let mut command = Command::new(program);
    command.current_dir(env!("CARGO_MANIFEST_DIR"));
    command.args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    command.env_remove("OTTERHIVE_OPERATOR_TOKEN");
    match command.output() {
        Ok(output) => (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ),
        Err(error) => (-1, String::new(), format!("無法執行命令：{error}")),
    }
}

/// fixture 指令稿的絕對路徑（`tests/fixtures/backends/<name>`）。
pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/backends")
        .join(name)
}

/// 讀取 fixture log 的第一行（`spawns_child.sh` 等寫入子程序 pid 的 fixture）。
pub fn fixture_log_first_line(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.trim()
        .lines()
        .next()
        .map(|line| line.trim().to_owned())
}
