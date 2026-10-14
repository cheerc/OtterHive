//! T4 Lane A 的 daemon 骨架（T4 RED）。
//!
//! 只放模組宣告與契約說明，行為由 T4 GREEN 實作：
//! - `mod`：socket 服務（`run/daemon.sock`）、單一行 JSON 請求／回應、單一 home
//!   單一 daemon（第二個以 exit 3 與 stderr 的 `daemon_already_running` 結束）、
//!   啟動自檢（bridge 同目錄、SIGCHLD 可 wait）。
//! - `auth`：operator／instance token 認證與 §2 的權限表。
//! - `lifecycle`：§4 的 create／start／restart／delete／query／control。
//! - `recovery`：§4.6 daemon 重啟後對殘留程序的收斂。

pub mod auth;
pub mod lifecycle;
pub mod recovery;
