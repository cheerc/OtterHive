# P1a 協作契約

本頁是 P1a 對外行為的唯一規格：instance 生命週期、訊息、終端、team、deployment、operator CLI 與 MCP bridge。功能取捨的理由在 [ADR 0001](../adr/0001-runtime-boundaries.md)，與舊實作的對照在 [feature map](../reverse-engineering/feature-map.md)，逐欄位的支援／拒絕在 [`mcp-catalog.json`](../reverse-engineering/mcp-catalog.json)，實作順序與測試在 [P1a 實作計畫](../plans/p1a-implementation.md)。

## 1. Home 與程序

- `OTTERHIVE_HOME` 未設定時用 `~/.otterhive`。OtterHive 不讀寫 `~/.agend`。
- 設定：`$OTTERHIVE_HOME/otterhive.yaml`，只放 `templates` 與 `managers`，daemon 只讀不寫；檔案不存在等同空設定。daemon 啟動時讀取；其他頂層鍵或格式錯誤使 daemon 拒絕啟動並列出原因。`managers` 的變更要重啟 daemon 才生效；deploy 每次重新讀取 `templates`，此時讀到壞檔回 `invalid_argument` 並保留原設定。
- 狀態：`state.sqlite3` 是 instance、team、訊息、deployment 的唯一權威；這些只透過操作建立與修改。
- IPC：`run/daemon.sock`；鎖：`run/daemon.lock`；operator 憑證：`secrets/operator.token`。
- 請求是一行 JSON：`{"v":1,"auth":{"operator":"<token>"} 或 {"instance":"<token>"},"op":"<操作>","args":{…},"operation_id":"…"}`；回應是一行 `OperationResult`（§3）。一個連線一個請求，`attach` 例外（§7）。
- 操作名稱與 MCP 工具名稱相同（catalog 中 P1a 的 11 個工具），另有兩個只經 socket 的操作：`attach`（§7）與 `tools_list`（§10）。
- `run/` 與 `secrets/` 權限 0700，socket 與憑證檔 0600。同一 home 只允許一個 daemon；第二個在 stderr 印出一行 `{"error":{"code":"daemon_already_running",…}}` 並以 exit code 3 結束。
- `otterhive-mcp-bridge` 必須與 `otterhive` 位於同一目錄；daemon 啟動時找不到它就拒絕啟動。
- 時間一律是 Unix epoch 毫秒整數，欄位名以 `_ms` 結尾。
- 平台：macOS 已驗證範圍以測試為準；Linux 與 Windows 未驗證，不宣稱支援。

## 2. 身分與授權

- daemon 發出 `InstanceId`（UUID）與 `Generation`（每次 spawn 加一）。name 只用來查詢；同名新 instance 取得新的 `InstanceId`。
- operator 以 `secrets/operator.token` 認證。instance 以每個 generation 一組的 token 認證，token 由 daemon 寫入該 instance 的 MCP 設定；舊 generation 的 token 一律失效。
- 呼叫端不能自報 sender、role 或 team。sender 一律由憑證推導。
- `role` 是描述文字，不授予任何權限。權限只來自：operator、`managers`（設定檔列出的 instance 名稱）、team 的 `orchestrator`、以及「對自己」。
- `managers` 與 `accept_from`（§6.2）以名稱表達 operator 的意圖：刪除後以同名重建的 instance 會取得同樣的權限。這是刻意的；要撤銷就改設定或 team。
- 威脅模型：daemon、agent 與 operator 都以同一個 OS 使用者執行。operator 憑證不放進 agent 的環境或 MCP 設定，避免 agent 誤用；但 agent 若主動讀取 `secrets/operator.token`，就能以 operator 身分操作。P1a 不防範惡意 agent（見 ADR 0001「不做的事」）。

| 動作 | operator | manager | orchestrator | instance |
|---|---|---|---|---|
| create／start／delete instance、team 異動、deployment（含 list） | 可 | 否 | 否 | 否 |
| restart | 任何 | 任何 | 只限自己 | 只限自己 |
| interrupt | 任何 | 任何 | 自己 team 的成員 | 否 |
| pane_snapshot | 任何 | 任何 | 自己 team 的成員與自己 | 只限自己 |
| list_instances、team list | 可 | 可 | 可 | 可 |
| send | 任何 | 任何 | 見 §6.2 | 見 §6.2 |
| inbox | 否 | 只限自己 | 只限自己 | 只限自己 |

manager 能 restart 其他 instance，對應 `agend-customization` 的 `instructions/general.md:56`。

## 3. 請求與結果

每個操作帶可選的 `operation_id`（呼叫端未給時由 daemon 產生）。同一呼叫端以同一 `operation_id` 重送時，回傳第一次記錄的結果，不重複執行副作用；不同呼叫端使用同一 `operation_id` 回 `invalid_argument`。呼叫端身分記為 `operator` 或 `instance:<InstanceId>`，與 generation 無關，所以 instance restart 後以新 token 重送仍視為同一呼叫端。

```json
{
  "operation_id": "…",
  "outcome": "completed | queued | partial | refused | unsupported",
  "subject": {"kind": "instance", "id": "…", "name": "…", "generation": 3},
  "value": {},
  "effects": ["process_stopped", "process_started"],
  "error": {"code": "…", "message": "…", "retryable": false, "next": "…"}
}
```

- `queued`：已持久接手、尚未執行完（例如 self restart）。不代表完成。
- `partial`：部分成功；`value` 逐項列出每個子結果。teardown 是清理型操作的明示例外（§8.4）：有 residuals 就回 partial，表示清理尚未收斂，不代表已刪除任何資源。
- `refused`：請求沒有達成。多數拒絕在任何變更前發生、沒有副作用；少數在執行中失敗（例如 start 時子程序立刻結束），已發生的狀態變更列在 `effects`。`error.next` 說明誰能做什麼下一步。
- `unsupported`：本期不支援的工具、action、欄位或值，在任何變更前拒絕，code 為 `unsupported_in_phase`。`error.message` 指出被拒的工具與 action／欄位／值，以及「P1a 不支援」。整個工具延後時另列 catalog 的 `phase`；P1a 工具的個別欄位不得借用工具的 `phase` 宣稱移植期別（例如 `create_instance.team` 只說 P1a 不支援，不說會在 P1a 實作）。
- 未知的欄位、action 或值（舊版也沒有的）回 `invalid_argument`。

錯誤碼：`unauthorized`、`not_found`、`name_conflict`、`invalid_argument`、`stale_generation`、`operation_in_progress`、`spawn_failed`、`resume_unavailable`、`recovery_required`、`delivery_token_stale`、`never_delivered`、`team_acl_denied`、`unsupported_in_phase`、`daemon_already_running`、`internal`。

常見對應：instance、team、template、deployment 不存在 → `not_found`；instance 或 team 名稱已被使用 → `name_conflict`；instance 不在需要的狀態（例如 interrupt 或 attach 一個未運行的 instance）→ `invalid_argument`。

## 4. Instance 生命週期

### 4.1 規格

`InstanceSpec`：`name`、`backend`（`claude` 或 `command`，省略時為 `claude`）、`command`（覆寫可執行檔）、`args`、`model`、`effort`、`working_directory`、`instructions`（檔案路徑）、`skills_path`（目錄路徑）、`env`、`description`、`role`。team 成員資格只經 §8.1 的 team 操作設定。

- `name`：1–64 字元，只含 `[A-Za-z0-9_-]`；不得為 `operator`。
- `working_directory`、`instructions`、`skills_path` 必須是絕對路徑（CLI 會先把相對路徑換成絕對路徑），而且必須存在；前者與 skills 必須是目錄，instructions 必須是可讀的檔案。`command` 可以是絕對路徑，或在 daemon 的 `PATH` 中查找的名稱。
- `backend: command` 必須提供非空的 `command`，缺少或空字串在任何變更前回 `invalid_argument`；`backend: claude` 省略 `command` 時執行 `claude`。
- 未給 `working_directory` 時使用 `$OTTERHIVE_HOME/workspace/<name>`，不存在則建立；明確給的目錄不會被建立。delete 不刪除任何 working directory。
- `model`、`effort`、`instructions`、`skills_path` 只適用 `backend: claude`；搭配 `command` 時回 `invalid_argument`。
- 以上檢查都在任何變更之前完成。
- `create_instance` 建立定義後立即依 §4.2 start。start 失敗時保留定義，確認沒有殘留程序才為 `failed`；停止無法確認則為 `recovery_required`。operator 可對 `failed` 再 start 或 delete；`recovery_required` 必須先人工確認／處理殘留，不自動重開。

狀態：`stopped` → `starting` → `running` → `stopping` → `stopped`；另有 `failed`（附原因）與 `recovery_required`。

### 4.2 start

1. 已在 `running` 的 instance 再 start 回 `completed`，`value.already_running: true`，不重開、generation 不變。`starting` 或 `stopping` 回 `operation_in_progress`；`recovery_required` 回同名錯誤，不 spawn。`stopped` 或 `failed` 若仍有程序紀錄，先依 §4.3 確認 group 已空；無法確認則回 `recovery_required`，不增加 generation、不 spawn。
2. 確認沒有前一代程序後，在交易中寫入 journal intent，generation 加一，發新 token，狀態 `starting`。
3. 物化 instructions 與 skills（§5），在 PTY 中 spawn。PTY 子程序是 session leader，`pgid == pid`；記錄 pid 與程序啟動時間。PTY 初始大小 120×40。
4. 無法執行（例如找不到可執行檔）：狀態 `failed`（原因 `exec_failed`），結果 `refused`（`spawn_failed`），`effects` 至少為 `["generation_advanced", "state_failed"]`。已產生 child 的失敗仍須依 §4.3 清理；清理無法確認則改為 `recovery_required`。
5. 回應前等待啟動寬限期（3 秒）結束或子程序結束，先發生者為準：
   - 寬限期內 leader 結束：先依 §4.3 清理同一 group 的殘留成員，並保留 exit status。確認停止才記為 `failed`（原因 `exited_during_startup`），結果 `refused`（`spawn_failed`，`value.reason` 同上）。`effects` 至少列 `generation_advanced`、`process_started`、`process_stopped`、`state_failed`。清理無法確認則為 `recovery_required`，結果 `refused`（`recovery_required`），列出 residuals，不標成已清理的 `failed`。不自動重試、不 crashloop。
   - 仍在執行：狀態 `running`，結果 `completed`，`effects: ["generation_advanced", "process_started"]`，`subject.generation` 為新值。
6. 寬限期後 leader 才結束：daemon 對該 process group 剩下的成員執行 §4.3；group 已空或清理成功則為 `stopped`（原因 `exited`，附 exit status），清理無法確認則為 `recovery_required`。

`effects` 累積本次操作實際發生的變更；清理前一代程序成功時另列 `process_stopped`，建立定義時另列 `instance_created`。未確認停止不得列 `process_stopped`。

### 4.3 stop（start 前清理、退出後清理、restart、delete、teardown 共用）

1. 先確認 group 的身分：
   - leader 仍在時，記錄的 pid 啟動時間必須相符，且 `pgid == pid`；身分不符或查詢失敗就不發任何 signal，直接視為無法確認。
   - 本 daemon 擁有的 child 以每個 generation 一份 `OwnedLeaderLease` 保留 leader 身分。觀測退出使用 `waitid(..., WEXITED | WNOHANG | WNOWAIT)`，取得 exit status 但不 reap；不得先呼叫會 reap 的 `wait`／`try_wait` 再靠舊 pgid 清理。lease 綁定最初取得的 pid、啟動時間與 pgid，每次 signal 前都要重新驗證 child 仍由本 daemon 持有且尚未 reap、身分與 pgid 相符。驗證與 reap 使用同一把 lifecycle 鎖，不能有另一個執行緒在驗證後先 reap。
   - leader 已結束但 group 仍有存活成員：只有上述尚有效、可重新驗證的 lease 才可授權清理。保留 Session 物件、曾觀測到 exit 或 group 目前非空，都不是證據。leader 一旦 reap、lease 被撤銷、child ownership 遺失或身分查詢失敗，lease 永久失效，不可再用舊 pgid 發 signal。T3 的 H8 必須驗證此 macOS 不提前 reap 的路徑。
   - 只有 SQLite 的歷史 pid／啟動時間時（例如 daemon 重啟），leader 不存在、group 卻非空的情況一律為 `recovery_required`，不發 signal。歷史 pgid 本身不足以證明現有成員的歸屬。
2. 對已確認身分的 process group 發 SIGTERM，等 5 秒；仍有存活成員則 SIGKILL，等 2 秒。各次發 signal 前重新驗證上述證據；證據不符就停止清理並回 `recovery_required`。期限到了但停止未證實時保留 Session，不以無限等待 join 冒充有界停止。
3. 確認同一 group 除保留的已退出 leader 外已無其他成員後，先撤銷 lease，再 reap 本 daemon 擁有的 leader、join 執行緒，才算停止；這個保留的 zombie 不當成還在執行的程序。lease 撤銷後不再對舊 pgid 做 signal 或把新出現的 group 視為原 group。無法確認時狀態為 `recovery_required`，回報殘留 pid，拒絕同一 instance 的新 spawn。daemon 不對身分不明的 pid 發 signal。
4. 已離開 process group 的 descendant（例如自行 `setsid`）不在追蹤範圍，P1a 不保證清除；這是已知限制。

### 4.4 restart

- `mode`：`fresh`（預設）或 `resume`。`reason` 是給人看的說明，寫入 journal。
- `resume` 先檢查 backend 的 session 證據（§5.4）。沒有證據回 `resume_unavailable`，原程序與狀態不變。
- `fresh` 先把該 instance 所有 `delivering` 訊息退回 `unread`（舊 token 失效）；`resume` 保留 lease。
- 依 §4.3 停止舊程序；確認停止後才以新 generation spawn。停止無法確認時回 `recovery_required`，不 spawn。
- 呼叫端帶 `expected_generation` 而不符時回 `stale_generation`。同一 instance 已有進行中的生命週期操作時回 `operation_in_progress`。
- 被 restart 的是呼叫者自己時，daemon 先回 `queued` 再執行；新程序可用 `list_instances` 確認新 generation。
- `stopped`、`failed` 狀態的 instance 也可 restart，效果等同以指定 mode start。

### 4.5 delete

依 §4.3 停止後，標記 identity 為已刪除、移出所屬 team，封存其 inbox（訊息改為 `archived`，不再可 drain）。以 `delete_instance` 刪除某 team 的 orchestrator 時回 `invalid_argument`，要求先更換 orchestrator 或刪除 team；teardown 依 §8.4 處理，不受此限。停止無法確認時回 `recovery_required`，不刪除。

### 4.6 daemon 重啟後

PTY 檔案描述元隨舊 daemon 消失，無法重新接上。daemon 啟動時，對記錄為 `starting`、`running`、`stopping`，或仍帶程序紀錄的 `failed`／`stopped` instance 依 §4.3 確認並停止仍存活的程序：成功則為 `stopped`（原因 `daemon_restarted`），否則為 `recovery_required`。已有的 `recovery_required` 保留，不因 daemon 重啟就解除。P1a 不自動重新啟動；operator 以 start、restart 或 deploy 再起。

### 4.7 查詢與控制

- `list_instances`：`value.instances` 為陣列，每項含 `name`、`id`、`state`、`reason`、`generation`、`backend`、`team`、`role`、`description`、`updated_ms`；帶 `instance` 時只回該項。對 instance 呼叫端回同樣欄位。
- `interrupt`：對目前 generation 的 PTY 寫入一個 ESC（`0x1b`），`effects: ["esc_sent"]`。不確認 agent 是否真的停下。instance 不在 `running` 回 `invalid_argument`。
- `pane_snapshot`：`value.text` 為目前畫面去除 ANSI 後的純文字，每列去掉行尾空白；`lines`（預設 100，上限 10000）只取最後幾列；`value.generation` 標明畫面屬於哪個 generation。instance 不在 `running` 時回 daemon 記憶體中最後一次的畫面並標 `value.live: false`；daemon 重啟後沒有舊畫面，回空字串、`live: false`、`value.reason: "no_screen"`。

## 5. Backend adapter

### 5.1 `claude`

- 可執行檔：`command` 或 `claude`。預設參數 `--dangerously-skip-permissions`；`model` → `--model <v>`；`effort` → `--effort <v>`；`args` 附加在後。
- 協調提示：daemon 寫入 `<workdir>/.claude/otterhive.md`，以 `--append-system-prompt-file` 傳入。內容至少包含：instance 名稱、所屬 team 與 orchestrator、可用的 OtterHive 工具、`[OTTERHIVE-MSG]` 提示代表要呼叫 `inbox` drain、處理完每則訊息要帶 `delivery_token` ack、回覆用 `send` 並帶 `parent_id`。`instructions` 檔若有給，其內容原樣附在協調提示之後。
- MCP 設定寫入 `<workdir>/.claude/otterhive-mcp.json`（權限 0600），以 `--mcp-config` 傳入；內容為：

  ```json
  {"mcpServers": {"otterhive": {"type": "stdio", "command": "<otterhive-mcp-bridge 的絕對路徑>", "args": [],
    "env": {"OTTERHIVE_HOME": "<home>", "OTTERHIVE_INSTANCE_TOKEN": "<該 generation 的 token>"}}}}
  ```
- skills：`skills_path` 存在時建立 `<workdir>/.claude/skills` symlink，並在 SQLite journal 記錄 `skills_link_created`（instance ID、link 絕對路徑、target 絕對路徑）。只有 journal 記錄屬於同一 instance，且現有路徑仍是 symlink、`read_link` 與所記 target 完全一致，才算 OtterHive 擁有、可在 restart 時更新；同名但不同 ID 不繼承擁有權。其他既有檔案、目錄或 symlink 一律不覆蓋，並在 start 結果的 `value.warnings` 回報；即使碰巧指向相同 skills 目錄也不推定擁有權。
- 送出鍵 `\r`。

### 5.2 `command`

只執行 `command` 加 `args`，不加任何參數、不寫任何設定檔、不支援 resume。供 fixture 與 shell 使用；需要 MCP 的程式從 §5.3 的環境變數取得 bridge 路徑與 token。

### 5.3 共同的子程序環境

子程序繼承 daemon 的環境，移除名稱以 `OTTERHIVE_OPERATOR` 開頭的變數；再加上 `env`、`OTTERHIVE_HOME`、`OTTERHIVE_INSTANCE`（名稱）、`OTTERHIVE_INSTANCE_TOKEN`、`OTTERHIVE_MCP_BRIDGE`（bridge 的絕對路徑）、`TERM=xterm-256color`。

### 5.4 resume 證據

`claude`：`$HOME/.claude/projects/<編碼>/` 下至少一個 `.jsonl` 含 `"type":"user"` 紀錄。編碼為 canonical working directory 中非 `[A-Za-z0-9-]` 的字元換成 `-`。有證據時以 `--continue` spawn。

`--continue` 接續的是「該目錄最新的 session」，不分 instance。因此若有其他未刪除的 instance 使用同一個 canonical working directory，`resume` 回 `resume_unavailable`（原因 `shared_working_directory`）。

`command` 一律 `resume_unavailable`。

這些規則以 fake executable 測試；真 Claude CLI 的行為在 operator 授權的 canary 驗證前，標記為未驗證。

## 6. 訊息

### 6.1 send

- 參數：`instance`、`instances`、`team` 三擇一；`message`（非空，最多 64 KiB）；`request_kind` 為 `query`、`report`、`update` 或省略（記為 `message`）；`correlation_id`、`parent_id`、`thread_id` 為不透明字串（各最多 256 字元）。
- 其他欄位與 `request_kind: task` 回 `unsupported_in_phase`（清單見 catalog）。
- team 廣播的收件人是該 team 除 sender 以外的所有成員。收件人名單去重；sender 不會收到自己的訊息。
- 對每個收件人：寫入一筆 `unread` 訊息並給 message ID，或回報拒絕原因。全部成功為 `completed`，部分為 `partial`，全部失敗為 `refused`。
- 寫入在同一交易內完成並 fsync 後才回應。喚醒（§6.4）在寫入之後進行；喚醒失敗不撤回訊息。
- 結果：`value.recipients` 為陣列，每項含 `instance`、`outcome`（`queued` 或 `refused`）、`message_id` 或 `error`、`wake`（§6.4）。
- P1a 沒有 operator 收件匣：agent 無法送訊給 operator，只能在自己的 pane 輸出。

### 6.2 收件權限

operator 與 manager 可送給任何 instance。instance sender 送給 instance recipient 時：

- recipient 不屬於任何 team：允許。
- 同一 team：允許。
- recipient 是某 team 的 orchestrator，且 sender 的名稱在該 team 的 `accept_from`：允許。`accept_from` 以送出當下的 instance 名稱比對。
- 其他：`team_acl_denied`。

### 6.3 inbox

- drain（`action` 省略）：只能讀自己的 inbox。依建立順序取 `unread` 與 lease 已過期的 `delivering` 訊息，`body` 的 UTF-8 bytes 總和上限 48 KiB（至少一筆）；改為 `delivering`，發一個新的 `delivery_token`，lease 600 秒，`delivery_count` 加一。前一批未 ack 的訊息**不會**因再次 drain 而被結算。
- drain 結果：`value.delivery_token`、`value.lease_expires_ms`、`value.remaining`（仍未 drain 的筆數）、`value.messages`；每則含 `id`、`from`（`{"kind":"operator"|"instance","name":…}`）、`kind`、`body`、`correlation_id`、`parent_id`、`thread_id`、`created_ms`、`delivery_count`。沒有訊息時 `messages` 為空陣列，不發 token。
- ack：`message_id` 加 `delivery_token`。token 相符則為 `acked`；重複 ack 同一組回 `completed`（`value.already_acked: true`）。token 不符或 lease 已過期回 `delivery_token_stale`；從未投遞回 `never_delivered`；不是自己的訊息回 `not_found`。
- 保證是至少一次投遞：lease 逾時、fresh restart、daemon 重啟都會讓未 ack 的訊息再次被 drain。不承諾恰好一次；接收端可能看到同一則訊息兩次，應以 message `id` 去重。

### 6.4 喚醒

送達後若收件人在 `running`，daemon 經 PTY 送出一行提示：`[OTTERHIVE-MSG] from=<sender> kind=<kind> id=<message_id>` 加送出鍵，提示 agent 呼叫 inbox。

- 有 client 在最近 3 秒內輸入過（operator 可能在打字），或收件人最近 500 毫秒內有輸出，就延後提示，每 250 毫秒重新判斷，直到條件解除才送出。延後沒有上限：持續輸出的程序在停下之前不會收到提示。instance 在送出前停止或換了 generation，就放棄該提示。不論提示是否送出，訊息都留在 inbox。
- 同一收件人尚未送出的多個提示合併為一行：`[OTTERHIVE-MSG] count=<n> from=<最後的 sender> kind=<最後的 kind> id=<最後的 message_id>`。
- send 結果中的 `wake` 為 `hinted`（已寫入 PTY）、`deferred`（排程中）、`not_running` 或 `failed`。`hinted` 只代表提示已寫入 PTY，不代表 agent 已讀或已處理。

## 7. 終端

### 7.1 流程

- 連線送 `attach`（`instance`、可選 `expected_generation`）。daemon 先回一行 `OperationResult`；成功時之後改為 frame 串流，失敗則關閉連線。instance 不在 `running` 時回 `invalid_argument`。
- `seq` 是每個 instance generation 的 PTY 輸出計數，從 0 開始。PTY reader 每取得一個非空 chunk，先在 Session 鎖內餵入 VT，再將 seq 加一，並以該 seq 建立一個 `Output` 事件；此時就是計數點，不等 socket 送出。無 client、佇列丟棄或 client 送出失敗都不影響計數。
- Snapshot header 的 `seq` 是畫面已包含的最後一個 Output seq（尚無輸出時為 0）。Snapshot 與訂閱在同一把鎖內完成：之後的 `Output` seq 從 snapshot 的 seq 加一開始，不重複也不遺漏。
- 每個 client 最多排隊 256 個 `Output`。佇列滿時 daemon 丟棄該 client 排隊的輸出，之後送 `Resync` 與新的 `Snapshot`；該 client 可跨過被丟棄的 seq，但以新 Snapshot.seq 為接續邊界，下一個 Output 是它加一。Resync／Snapshot 不增加 seq。PTY 讀取永不因 client 阻塞。
- instance restart 或結束後，連線收到 `Ended` 並關閉；client 以新 generation 重新 attach，舊程序的輸出不會出現在新連線上。
- 所有 client 中最後一個送 `Resize` 的決定 PTY 大小。
- detach 或 client 結束不影響 instance 與 daemon。

### 7.2 frame

frame = 1 byte tag + 4 byte big-endian 長度 + 內容；內容上限 1 MiB，超過時 daemon 關閉連線。

| tag | 方向 | 名稱 | 內容 |
|---|---|---|---|
| `0x01` | daemon → client | Snapshot | 4 byte big-endian header 長度 + JSON header `{"generation","seq","cols","rows"}` + 畫面 bytes（重繪整個畫面與游標位置的 ANSI 序列） |
| `0x02` | daemon → client | Output | 8 byte big-endian seq + PTY 輸出 bytes |
| `0x03` | daemon → client | Resync | 空；下一個 frame 必為 Snapshot |
| `0x04` | daemon → client | Ended | JSON `{"generation","reason"}` |
| `0x05` | daemon → client | Error | JSON `{"code","message"}` |
| `0x10` | client → daemon | Input | 8 byte big-endian generation + bytes |
| `0x11` | client → daemon | Resize | JSON `{"generation","cols","rows"}` |
| `0x12` | client → daemon | Detach | 空 |

generation 不符的 Input／Resize 被丟棄並回 `Error`（`stale_generation`）。

## 8. Team 與 deployment

### 8.1 team

- `create`（`name`、`members`、`orchestrator`、`description`、`accept_from`）、`update`（`name` 加 `add`、`remove`、`orchestrator`、`description`、`accept_from`）、`delete`（`name`）、`list`。
- team 名稱規則同 instance 名稱。`create` 必須提供非空的 `members` 與 `orchestrator`；member 名稱去重後必須是已存在、未刪除的 instance，`orchestrator` 必須是成員。一個 instance 最多屬於一個 team。
- 缺少必填參數、空 members、名稱格式錯誤、orchestrator 不在變更後的 members → `invalid_argument`；成員不存在 → `not_found`；成員已屬於另一個 team → `name_conflict`。`update` 若 remove 原 orchestrator，須在同一次操作指定仍在成員中的新 orchestrator。所有檢查在同一交易內、任何 team 異動前完成，拒絕時不留下部分 add／remove。
- 刪除 team 不刪除 instance。屬於某 deployment 的 team 只能經 teardown 刪除，直接 delete 回 `invalid_argument`。
- `list`：`value.teams` 每項含 `name`、`orchestrator`、`members`、`description`、`accept_from`、`deployment`。

### 8.2 template

`otterhive.yaml` 的 `templates.<name>` 接受舊 fleet.yaml 的格式：`orchestrator`、`instances.<member>` 下的 `role`、`backend`、`command`、`model`、`effort`、`args`、`instructions`、`skills_path`、`env`、`working_directory`。`instances` 必須是非空物件、member key 遵守 §4.1 名稱規則、`orchestrator` 必須是其中一個 key；缺漏或不符回 `invalid_argument`，不 spawn。成員 spec 依 §4.1 驗證。其他鍵（例如 `source_repo`、`project_id`、`github_login`）不生效，並列在 deploy 結果的 `value.ignored_template_fields`。daemon 每次 deploy 時重新讀取設定檔。

### 8.3 deploy

- 參數：`template`、`name`、可選 `directory`；`branch` 回 `unsupported_in_phase`。
- 成員 instance 名稱為 `<name>-<member>`，team 名稱為 `<name>`，orchestrator 為 `<name>-<orchestrator>`。任何組出來的名稱違反 §4.1 的名稱規則時，在任何變更前回 `invalid_argument`。
- working directory 依序取：成員的 `working_directory`、`<directory>/<member>`（有給 `directory` 時）、`$OTTERHIVE_HOME/workspace/<instance 名稱>`。成員明確給的 `working_directory` 依 §4.1 必須已存在，不建立。`directory` 本身必須是已存在的絕對目錄，CLI 可轉換相對路徑；從它衍生的 member 子目錄與預設 workspace 目錄由 deploy 建立，已存在且為目錄則沿用，既有非目錄回 `invalid_argument`。生成路徑前先完成參數與名稱衝突檢查；建立失敗回 `internal`，列出已建立的目錄 effects，不建立任何 instance／team 定義、不 spawn。delete 與 teardown 都不刪這些目錄。
- 預設與 `directory` 衍生的路徑每個成員各自一個目錄；template 若明確指定共用目錄，仍接受，但 §5.4 的 resume 會拒絕。
- 先在交易中建立 deployment 記錄、team 與所有成員定義，再同時 start 所有需要啟動的成員（各自依 §4.2 等待寬限期）。每個成員回報 `started`、`already_running` 或 `failed`（含錯誤碼與實際狀態，包含 `recovery_required`）。全部 started／already_running 為 `completed`；部分成功為 `partial`；全部失敗為 `refused`（`spawn_failed`；任一停止無法確認則為 `recovery_required`）。失敗不回滾已建立的定義；`effects` 列出定義建立與各成員實際變更，已啟動的成員保持運行。
- 對同名 deployment 再次 deploy：只 start 未在運行的成員，不重開已在運行的成員。名稱已被非此 deployment 擁有的 instance 或 team 使用時，整個 deploy 回 `name_conflict`，沒有副作用。
- `list`：`value.deployments` 每項含 `name`、`template`、`team`、`members`（`instance`、`state`）、`residuals`。

### 8.4 teardown

1. 依 §4.3 停止該 deployment 擁有的每個成員。
2. 全部確認停止：在同一交易中刪除所有成員（包括 orchestrator）、team 與 deployment 記錄，結果 `completed`。
3. 任何成員停止無法確認：不刪除任何成員、team 或記錄；已停止的成員保持 `stopped`，無法確認的為 `recovery_required` 並列在 `residuals`，結果 `partial`。處理後可再次 teardown。

## 9. Operator CLI

- 全域選項 `--home <dir>` 優先於 `OTTERHIVE_HOME`。CLI 從 `secrets/operator.token` 讀 operator 憑證。
- 除 `daemon` 與 `attach` 外，每個指令把 `OperationResult` 以 JSON 印到 stdout。
- 操作 CLI 的 exit code：`0` completed／queued、`2` partial、`3` refused、`4` unsupported、`1` 無法連到 daemon 或請求格式錯誤。`daemon` 的第二程序衝突也使用 3（§1），但以 stderr 的 `daemon_already_running` JSON 辨識；測試不能只檢查數字就判定原因。

| 指令 | 對應 |
|---|---|
| `otterhive daemon` | 前景執行 daemon |
| `otterhive create <name> [--backend claude\|command] [--command <path>] [--arg <a>]… [--model <m>] [--effort <e>] [--cwd <dir>] [--instructions <file>] [--skills-path <dir>] [--env K=V]… [--role <r>] [--description <d>]` | `create_instance` |
| `otterhive start <name>`、`delete <name>`、`restart <name> [--mode fresh\|resume] [--expected-generation <n>]` | 同名操作 |
| `otterhive list [<name>]`、`interrupt <name>`、`snapshot <name> [--lines <n>]` | `list_instances`、`interrupt`、`pane_snapshot` |
| `otterhive send (--to <name>… \| --team <team>) [--kind query\|report\|update] <message>` | `send` |
| `otterhive team create <name> --members <a,b> --orchestrator <a> [--accept-from <x,y>] [--description <d>]`、`team update <name> [--add <a,b>] [--remove <a,b>] [--orchestrator <a>] [--accept-from <x,y>] [--description <d>]`、`team delete <name>`、`team list` | `team` |
| `otterhive deploy <template> <name> [--directory <dir>]`、`teardown <name>`、`deployments` | `deployment` |
| `otterhive attach <name>` | `attach`；`Ctrl+B` 再按 `d` 離開 |
| `otterhive call <op> [--json <object>]` | 任何操作的原始呼叫，供除錯 |

## 10. MCP bridge

- `otterhive-mcp-bridge` 由 backend 以 stdio 啟動，從環境變數讀 `OTTERHIVE_HOME` 與 `OTTERHIVE_INSTANCE_TOKEN`。兩者缺一時，`initialize` 回 JSON-RPC 錯誤 `-32000`，message 指出缺哪個變數。無法連到 daemon 時，`tools/list`／`tools/call` 回 JSON-RPC 錯誤 `-32001`，不假裝成功。
- 協定：每行一則 JSON-RPC 2.0 訊息。
  - `initialize`：回傳 client 要求的 `protocolVersion`、`capabilities: {"tools": {}}`、`serverInfo: {"name": "otterhive", "version": <crate 版本>}`。
  - `notifications/initialized`：不回應。`ping`：回 `{}`。
  - `tools/list`：轉呼叫 daemon 的 `tools_list`，回傳該 instance 可用的工具定義。
  - `tools/call`：把 `name` 當操作、`arguments` 當 `args` 轉給 daemon；結果為一個 `type: "text"` 的 content，內容是 `OperationResult` 的 JSON；outcome 為 `refused` 或 `unsupported` 時 `isError: true`。
  - 其他方法回 JSON-RPC `-32601`。
- `tools_list` 規則：只列 P1a 已實作、且該 instance 至少能執行一個 action 的工具。instance 一律看得到 `list_instances`、`restart_instance`、`send`、`inbox`、`team`、`pane_snapshot`；manager 與 orchestrator 另外看得到 `interrupt`。`create_instance`、`delete_instance`、`start_instance`、`deployment` 與所有非 P1a 工具不列出；直接呼叫依 §2、§3 拒絕。
- 工具的 input schema 只含 catalog 中該工具的 `supported_fields` 與 `added_fields`。
