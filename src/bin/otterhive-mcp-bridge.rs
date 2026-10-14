//! T4 Lane B RED：`otterhive-mcp-bridge` 的 **test-only 空骨架**（無行為）。
//!
//! 這個 binary 在 T4 GREEN 之前只做兩件事：讓
//! `CARGO_BIN_EXE_otterhive-mcp-bridge` 存在（`tests/mcp_wire.rs` 靠它啟動
//! 真行程），以及不讀 stdin 就結束。
//!
//! 刻意不做的事：不讀 `OTTERHIVE_HOME`／`OTTERHIVE_INSTANCE_TOKEN`、不解析
//! JSON-RPC、不連 daemon、不寫任何檔案。契約 §10 的行為（initialize 回
//! protocolVersion、缺變數回 -32000、daemon 不可連回 -32001…）全在 GREEN。
//!
//! stdout 一個位元組都不寫：這讓 `tests/mcp_wire.rs` 的「不得有回應」與
//! 「應回 -32001」斷言在 RED 階段明確失敗，而不是靠空回應假綠。

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!("otterhive-mcp-bridge 尚未實作（T4 GREEN 補上契約 §10 的 stdio transport）");
    ExitCode::from(1)
}
