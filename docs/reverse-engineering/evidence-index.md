# 行為證據索引

OtterHive 對舊 `agend-terminal` 的行為主張，以本頁列出的證據為準。source 閱讀只算靜態推論；下表的 REA capture 才是受控重播的觀測。

## 來源身分

| 項目 | 值 |
|---|---|
| 舊 daemon source | `cheerc/agend-terminal@593da97c6f94d874f793777b4550d5df2ae4f43c` |
| 舊 daemon binary | `target/release/agend-terminal`，SHA-256 `80951250b2ee213ac9c263581fc0a904cb4354fb842f8887da68f7e6edb0bd7d` |
| workflow source | `agend-customization@d49735d89ae97c58a09cc243428dd629d013a650` |
| REA | `rea-agents@6.4.0`，provider `rea-process` v3，darwin/arm64，node-pty |
| 證據檔 | `docs/reverse-engineering/evidence/rea-legacy-baseline.json`（MCP session 匯出：5 records、3 unknowns，含 `ev_d4b324c2…` 與 `ev_33e9c28b…`）；`docs/reverse-engineering/evidence/rea-baseline-rerun.json`（`scripts/research/rea-baseline.sh` 經 REA CLI 產生的 `ev_b0853814…` 完整 Evidence） |
| 舊 source 取得 | GitHub `cheerc/agend-terminal`，checkout 上列 commit；`scripts/research/build_mcp_catalog.py` 也需要它 |

binary 與 source 的對應只依 `~/.cargo/bin/agend-terminal` 和 `target/release/agend-terminal` SHA-256 相同推得，未重新 build 驗證。

## Captures

| Evidence ID | 情境 | 觀測到 | 未證明 |
|---|---|---|---|
| `ev_d4b324c2081a8fd94bdae8450ce1832c151a52b2c4d67a514d662dc928487a03` | 隔離 `AGEND_HOME` 下執行 `--help` | exit 0、quiesced；觀測目錄無變更；列出 20 個 CLI 子命令 | 任何 daemon、instance 或訊息行為 |
| `ev_33e9c28b1ce04b59d736b93cc163b141589fbd23c0c69a0bf1b94f462727b5d4` | `env -i` 加 fake `claude`（`tests/fixtures/backends/capture_stub.sh`）執行 `capture backend --backend claude --seconds 1` | 見下方「capture 行為」 | daemon、MCP、send/inbox、restart、真 Claude CLI |
| `ev_b085381429c0f6baba7d6af434f2b4b2b5e154af9dd792d01837acdf1e1e9d16` | 以 `scripts/research/rea-baseline.sh` 重跑同一情境 | VTerm 內容與檔案影響和上一筆一致 | 同上 |

### capture 行為（`ev_33e9c28b…`）

- 以 `PATH` 解析 `claude`，在 PTY 中 spawn；fake backend 輸出的 CJK 與 ANSI 樣式正確進入 VTerm，dump 為 120×40。
- 只在 `AGEND_HOME/captures/capture-claude/` 建立 `.cap` 與 `.cap.meta.json`；隔離的 `HOME` 與 cwd 都沒有變更。
- log 顯示 `AGEND_ENV_ISOLATION off`：backend 繼承 daemon 的完整環境。
- 子程序結束後記錄 `refusing to signal a shared process group pid=<pid> pgid=-1`：無法確認 process group 時不發 signal。
- 程序在無輸入下過快結束時判為 `startup failure (exited too quickly, no user input)`。

## 靜態 source 推論（不是行為證據）

以下主張來自閱讀 `agend-terminal@593da97c` 的原始碼，沒有以 REA 觀測其執行。設計時可作為參考，但不能當成已觀測的行為。

| 主張 | 依據 |
|---|---|
| runtime 不呼叫 tmux；tmux 只是操作風格 | `Command::new("tmux"` 在 `src/` 出現 0 次；`tmux` 只出現在註解：`src/keybinds.rs:1`、`:192`、`:200`，`src/bootstrap/daemon_spawn.rs:1`，`src/layout/split.rs:203` |
| 終端以 portable-pty 加 alacritty_terminal 實作 | `Cargo.toml` 依賴；`src/agent/mod.rs:1331–1586`；`src/vterm.rs:1005` |
| 再次 drain 會隱式 ack 上一批 | `src/inbox/storage.rs:55`、`:850–851` |
| restart 不等舊程序結束就 spawn | `src/mcp/handlers/instance_state/mod.rs:534–535` |
| deploy 的成員 spawn 失敗只寫 log，仍回 `deployed` | `src/deployments.rs:633`、`:646`、`:994` |
| 慢的訂閱者會被丟 chunk | `src/agent/broadcast.rs:64–88` |
| Claude preset 執行 `claude --dangerously-skip-permissions`，resume 用 cwd 的 `--continue` | `src/backend.rs:417–421` |
| Claude spawn 只在檔案存在時加 instructions／MCP flags | `src/backend.rs:854–881`；`spawn_flags` 檢查檔案存在後才加入 `--append-system-prompt-file`、`--mcp-config` |
| 舊版以編碼後的 canonical cwd 找 session，至少一筆 `"type":"user"` 才認為可 resume | `src/backend.rs:1100–1184`；`has_resumable`、`encode_project_dir`、`jsonl_has_user_entry`。這是舊 adapter 的判斷，不證明真 Claude 的相容性 |

## 已知限制與事件

- REA 未記錄繼承的宿主環境、未觀測網路；process tree 是取樣，可能漏掉短命 descendants。
- 以 REA CLI 第一次執行 `rea-baseline.sh` 時回報 `cleanup_incomplete`，指向 PID 15273。`ps` 顯示該程序是 Chrome renderer（pgid 874），在擷取期間啟動，與情境無關；當時沒有任何 `agend-terminal` 或 fixture 程序存活。之後重跑成功（`ev_b0853814…`）。script 會保留 REA 的 exit code 與部分觀測，不吞掉這類錯誤。
- `HistoricalSourceGraph`（`import-reference-source`）只是 historical-reference，`inventory_state: partial`，不是行為證據。

## 尚未取得的證據

| 主題 | 需要的證據 | 狀態 |
|---|---|---|
| 舊 daemon 的 create/send/inbox/restart | 隔離 home 中起舊 daemon，以 fake backend 走 MCP 流程 | 未執行；P1a 契約不依賴與舊版逐字等價 |
| 真 Claude CLI 的 wake、resume、skills 載入 | operator 授權的付費 canary | 未授權、未執行 |
| Linux、Windows | 對應平台實跑 | 未執行 |
