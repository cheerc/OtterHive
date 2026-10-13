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
| P1a：lifecycle、訊息、終端 attach、team、deployment | T2（protocol、admission、home、config、store）實作完成，T3 待進行；見 [P1a 實作計畫](plans/p1a-implementation.md) |
| P1b：TUI、resume／handoff 觸發、`set_metadata` 等 | 未開始 |
| P2 以後：task、decision、worktree、review、CI、channel | 未開始；清單在 [`mcp-catalog.json`](reverse-engineering/mcp-catalog.json) |
