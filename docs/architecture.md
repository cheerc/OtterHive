# 架構

## 程序

| 程序 | 角色 |
|---|---|
| `otterhive daemon` | 唯一擁有 PTY、backend 子程序、`state.sqlite3` 與 `run/daemon.sock` |
| `otterhive <command>` | operator CLI：instance、team、deployment、attach |
| `otterhive-mcp-bridge` | 每個 instance 的 MCP stdio server，把 tool call 轉給 daemon，帶該 generation 的 token |

backend CLI 由 daemon 在 PTY 中啟動，經 MCP 設定連到 bridge。client 斷線不影響 daemon 與 instance。所有 I/O 都是同步呼叫加執行緒，不使用 async runtime（ADR 0001 第 10 點）。

## 模組

| 模組 | 責任 |
|---|---|
| `protocol` | 身分型別、`OperationResult`、錯誤碼、請求格式 |
| `store` | SQLite schema、交易、journal |
| `config` | `$OTTERHIVE_HOME/otterhive.yaml`：templates、managers（instance 與 team 只存在 SQLite） |
| `daemon` | socket 服務、授權、lifecycle、daemon 重啟後的收斂 |
| `backend` | adapter（`claude`、`command`）、參數、instructions／skills 物化、resume 證據、喚醒提示 |
| `terminal` | PTY session、VT 畫面、snapshot 與訂閱、attach 串流協定 |
| `messaging` | send、收件權限、inbox drain／ack |
| `teams`、`deployments` | team 異動、template 部署與 teardown |
| `mcp` | 工具 registry 與 bridge；`tools/list` 只列已實作且呼叫端可用的工具 |

規格見 [P1a 協作契約](specs/p1-coordination.md)；取捨見 [ADR 0001](adr/0001-runtime-boundaries.md)。

## 實作狀態

| 範圍 | 狀態 |
|---|---|
| P0：功能對照、MCP catalog、REA 行為基線 | 完成 |
| P1a：lifecycle、訊息、終端 attach、team、deployment | T2（PR #1）、T3 程序／終端 Session（PR #2）、T4 daemon、授權、lifecycle、claude backend、bridge transport 與 CLI（PR #3）、T5 team／訊息（send／inbox）／喚醒排程／完整 P1a MCP 工具集合／CLI 的 `send`／`team`（PR #4，merge `9681a3d`）完成並合併。T5 之後、T6 之前另完成一批修正：**PR #5**（merge `0c8a1ed`）——PTY 寫入不再阻塞 daemon（`write_input` 改 `try_lock`、`interrupt` 與 attach Input 的寫入移出 daemon 鎖、`interrupt` 由 `internal` 改為 `operation_in_progress`），以及三條「契約要求、實作少做、測試零覆蓋」的缺陷（`list_instances` 的 `team` 改讀 `team_members`、`spec.env` 與 `SessionConfig::env` 的 `OTTERHIVE_OPERATOR` 前綴過濾、協調提示補上 team／orchestrator 與完整工具清單）；**PR #8**（merge `aceead9`）——測試等待條件的敘述與 needle 依據修正（「needle 取在斷言對象之前」的同型缺陷三處，其中兩處改用整行錨定 `"\nTERM\n"`）。診斷過程與**被推翻的結論**見 [PTY 寫入阻塞](worklog-pty-write-block.md)。全量 `cargo test --locked --all-targets` 365 passed／0 failed。已知未修見 [PTY 寫入阻塞](worklog-pty-write-block.md) 的「仍未修（需要 REA）」與 issue #6／#7（`daemon_recovery` 的 flake **未定性**）。T6 deployment 起未開始。見 [P1a 實作計畫](plans/p1a-implementation.md) |
| P1b：TUI、resume／handoff 觸發、`set_metadata` 等 | 未開始 |
| P2 以後：task、decision、worktree、review、CI、channel | 未開始；清單在 [`mcp-catalog.json`](reverse-engineering/mcp-catalog.json) |
