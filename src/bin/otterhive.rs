//! T4 Lane B RED：`otterhive` 的 **test-only 空骨架**（無行為）。
//!
//! 這個 binary 在 T4 GREEN 之前只做兩件事：讓 `CARGO_BIN_EXE_otterhive`
//! 這個環境變數存在（測試要靠它啟動 bridge 與 CLI），以及在收到任何參數時
//! 以非零碼結束。GREEN 之後它才成為契約 §9 的 operator CLI。
//!
//! 刻意不做的事：不解析參數、不連線、不讀 operator 憑證、不碰 `~/.agend`
//! 或任何正式 home。

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!("otterhive CLI 尚未實作（T4 GREEN 補上契約 §9 的指令表）");
    ExitCode::from(1)
}
