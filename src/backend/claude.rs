//! T4 Lane A 的 `backend: claude` 骨架（T4 RED，契約 §5.1）。
//!
//! 介面在此凍結，行為由 T4 GREEN 實作：
//! - 參數組裝：`--dangerously-skip-permissions`、`--model <v>`、`--effort <v>`、
//!   `--append-system-prompt-file <workdir>/.claude/otterhive.md`、
//!   `--mcp-config <workdir>/.claude/otterhive-mcp.json`，`args` 附加在最後。
//! - resume 證據（§5.4）：`$HOME/.claude/projects/<編碼>/` 下至少一個 `.jsonl`
//!   含 `"type":"user"`；編碼為 canonical working directory 中非 `[A-Za-z0-9-]`
//!   的字元換成 `-`。共享 working directory → `shared_working_directory`。

/// `backend: claude` 的 adapter。
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeBackend;
