//! T4 Lane A 的 `backend: command` 骨架（T4 RED，契約 §5.2）。
//!
//! 只執行 `command` 加 `args`：不加任何參數、不寫任何設定檔、不支援 resume
//! （`resume` 一律 `resume_unavailable`）。需要 MCP 的程式從 §5.3 的環境
//! 變數取得 bridge 路徑與 token。

/// `backend: command` 的 adapter。
#[derive(Debug, Clone, Copy, Default)]
pub struct CommandBackend;
