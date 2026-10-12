# 功能對照：workflow → 舊實作 → OtterHive 分期

本頁回答「OtterHive 要移植哪些能力、為什麼、在哪一期」。欄位層級的支援與拒絕清單只記在 [`mcp-catalog.json`](mcp-catalog.json)；行為證據記在 [`evidence-index.md`](evidence-index.md)。

來源版本：舊 daemon `agend-terminal@593da97c`（下稱 AT），workflow `agend-customization@d49735d`（下稱 AC）。行號以這兩個 commit 為準。

## 判斷依據

AC 實際呼叫的 MCP 工具，以 `AC:skills/general/agend-changelog/references/dependency-surface.md:10` 的清單為準，共 20 個。AT 的 34 個工具全部列在 catalog；不在這 20 個裡的工具不代表要刪除，只是排序靠後。

## 對照表

| 能力 | AC 的使用位置 | AT 實作入口 | 分期 | OtterHive 契約重點 |
|---|---|---|---|---|
| fleet／template 設定：`command`、`backend`、`model`、`effort`、`instructions`、`skills_path`、`working_directory` | `~/.agend/fleet.yaml` 的 `instances` 與 `templates`（例如 `ac-team`） | `src/fleet/` | P1a | 設定檔層級的 `model`／`effort` 是 spawn 參數，首版支援；改既有 instance 的 `set_model` 工具延後 |
| 建立、啟動、刪除 instance | operator 操作；agent 禁用：`AC:instructions/lead.md:7`、`AC:instructions/commander.md:5`、`AC:skills/team-discuss/SKILL.md:46` | `src/mcp/handlers/instance_state/mod.rs:59`、`src/agent_ops/spawn.rs:175`、`src/mcp/handlers/instance_state/lifecycle.rs:180` | P1a | 只有 operator 可呼叫；agent 被拒是 daemon 的規則，不只靠 L1 |
| fresh／resume restart | 自己：`AC:skills/handoff/SKILL.md:38`、`AC:skills/handoff/references/full-restart.md:13`；General 對他人：`AC:instructions/general.md:56`；B11 `dependency-surface.md:36` | `instance_state/mod.rs:335–614`、`instance_state/restart_prep.rs` | P1a | instance 可 restart 自己，manager 可 restart 任何 instance；resume 需 session 證據，否則拒絕且不動原程序；舊程序確認停止後才起新的 |
| instructions 與 skills 物化 | 每個 fleet instance 的 `instructions:`、`skills_path:` | `src/agent_ops/spawn.rs:207`、`src/agent_ops.rs:1446`、`src/skills.rs:336`、`src/backend.rs:854–881` | P1a | spawn 與 restart 時重讀；Claude 用 `--append-system-prompt-file` 與 `.claude/skills` symlink；不改 AC 原檔 |
| team 與 deployment | `AC:instructions/general.md:38`：只在 operator 指定 template 時 deploy | `src/deployments.rs:827`、`:959–976`、`:1144–1182`；`src/team_ops.rs:104` | P1a | 逐成員回報結果；部分成功為 partial、全失敗為 refused；teardown 只處理自己擁有的 instance |
| 單播、多播、team 廣播訊息 | `dependency-surface.md:10`；`AC:instructions/general.md:69`：送達即持久，不重送、不先 interrupt | `src/mcp/handlers/comms.rs:32`、`src/agent_ops/messaging.rs:188`、`:643–709` | P1a | sender 由 daemon 推導；每個收件人各有 message ID 與結果；task／review／branch 類欄位一律拒絕。AC 的「送出端不重送」仍成立，但投遞是至少一次，接收端要以 message id 去重 |
| inbox drain 與 ack | `AC:docs/team-workflow/lead.md:60`：merge 前 drain | `src/inbox/storage.rs:366–390`、`:782`、`:1096` | P1a | drain 發 delivery token，不隱式 ack；ack 帶 token；逾時或 fresh restart 後重送 |
| interrupt 與 pane_snapshot | `AC:instructions/general.md:69`；`AC:skills/self-retro/references/daemon-evidence.md:12` | `src/vterm.rs:1005`、`src/agent/mod.rs:3145` | P1a | snapshot 來自 VT 畫面，不是 raw log |
| 終端 attach／detach／input／resize | operator 日常操作 | `src/tui.rs:28`、`src/daemon/tui_bridge.rs:342–790`、`src/framing.rs:8–18` | P1a | client 與 daemon 分離；reattach 先拿畫面再接續輸出；有 gap 就重新同步 |
| tab／split／move 的 TUI | operator 日常操作（tmux 風格前綴鍵） | `src/app/`、`src/layout/`、`src/keybinds.rs` | P1b | 只投影 daemon 狀態，不擁有 process；不依賴 tmux |
| `[AGEND-RESUME]`／`[AGEND-HANDOFF]` 觸發 | `AC:skills/handoff/SKILL.md`（full restart 後恢復） | AT daemon 在 fresh restart 後注入 | P1b | 需先有可驗證的 wake adapter |
| `set_metadata`、`set_waiting_on`、`instance` 別名 | `dependency-surface.md:10`（前兩者） | `src/mcp/tools.rs:199–241` | P1b | 低風險，跟 TUI 一起做 |
| task board | `AC:docs/team-workflow/lead.md:20`：create 不派工 | `src/tasks/`、`src/task_events.rs` | P2 | 這期才接管 AC actor 的正常 board 操作 |
| decision board | 治理決策、operator 問答 | `src/decisions.rs` | P2 | |
| worktree binding 與 `repo` | `AC:docs/team-workflow/lead.md`、Fleet Protocol §10 | `src/binding/`、`src/worktree.rs`、`src/mcp/handlers/ci/checkout.rs` | P2 | |
| typed review、CI、merge、closure | `AC:docs/team-workflow/lead.md:30`、`:62`；`AC:docs/team-workflow/reviewer.md:25` | `src/review_receipt.rs`、`src/merge_receipt.rs`、`src/daemon/ci_watch/` | P3 | 完整 lead／impl／review 流程要到這期才相容 |
| health、schedule、config、restart_daemon、usage_limit_takeover | `AC:instructions/general.md:68`（health） | `src/health.rs`、`src/schedules.rs` 等 | P4 | |
| `set_model` | — | `src/mcp/tools.rs:169` | 延後（operator 決定） | 設定檔 `model` 仍在 P1a 支援 |
| Telegram／Discord 與 `reply`、`operator_page`、`download_attachment`、`bind_topic` | `AC:instructions/general.md:68`（channel-reply-missing） | `src/channel/` | 延後（operator 決定） | 首版沒有 channel；這四個工具不出現在 `tools/list` |
| agent 送訊給 operator | AC 透過 `reply`（Telegram）回報 operator | `reply` 與 channel | 未定 | P1a 沒有 operator 收件匣，agent 只能在 pane 輸出；是否在 P1b 加入不經 channel 的 operator 收件匣，待 operator 決定 |

## 首版完成的定義

P1a 完成時，operator 能在獨立的 `OTTERHIVE_HOME` 用 template 起一個 team；成員之間能互相送訊、被喚醒、drain 與 ack；instance 能 fresh restart 或有證據地 resume，終端可重新 attach；任何失敗都回報 partial 或 refused，不假裝成功。

P1a 完成**不代表** AC 的 lead／impl／review 流程能整條跑：那需要 P2 的 task board 與 P3 的 review／CI。在那之前，AC 的 L1 需要一份不引用 task board 的 P1 相容版本，原檔不改。
