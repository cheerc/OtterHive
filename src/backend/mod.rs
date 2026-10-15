//! T4 Lane A 的 backend adapter 骨架（T4 RED）。
//!
//! 只放型別宣告與契約說明，行為由 T4 GREEN 實作：
//! - `claude`：契約 §5.1（參數組裝、協調提示、MCP 設定、skills symlink、resume 證據）。
//! - `command`：契約 §5.2（只執行 `command` 加 `args`，不寫設定檔、不支援 resume）。
//!
//! 共用子程序環境見 §5.3（移除 `OTTERHIVE_OPERATOR` 前綴變數，加上 `env`、
//! `OTTERHIVE_HOME`、`OTTERHIVE_INSTANCE`、`OTTERHIVE_INSTANCE_TOKEN`、
//! `OTTERHIVE_MCP_BRIDGE`、`TERM=xterm-256color`）。

pub mod claude;
pub mod command;
