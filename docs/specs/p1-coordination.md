# P1a 協作契約

本頁是 P1a 對外行為的唯一規格：instance 生命週期、訊息、終端、team 與 deployment。功能取捨的理由在 [ADR 0001](../adr/0001-runtime-boundaries.md)，與舊實作的對照在 [feature map](../reverse-engineering/feature-map.md)，逐欄位的支援／拒絕在 [`mcp-catalog.json`](../reverse-engineering/mcp-catalog.json)。

## 1. Home 與程序

- `OTTERHIVE_HOME` 未設定時用 `~/.otterhive`。OtterHive 不讀寫 `~/.agend`。
- 配置：`otterhive.yaml`（instances、teams、templates、managers）；狀態：`state.sqlite3`；IPC：`run/daemon.sock`；鎖：`run/daemon.lock`；operator 憑證：`secrets/operator.token`。
- `run/` 與 `secrets/` 權限 0700，socket 與憑證檔 0600。同一 home 只允許一個 daemon；第二個以 `daemon_already_running` 結束。
- 平台：macOS 已驗證範圍以測試為準；Linux 與 Windows 未驗證，不宣稱支援。

## 2. 身分與授權

- daemon 發出 `InstanceId`（UUID）與 `Generation`（每次 spawn 加一）。name 只用來查詢；同名新 instance 取得新的 `InstanceId`。
- operator 以 `secrets/operator.token` 認證。instance 以每個 generation 一組的 token 認證，token 由 daemon 寫入該 instance 的 MCP 設定；舊 generation 的 token 一律失效。
- 呼叫端不能自報 sender、role 或 team。sender 一律由憑證推導。
- `role` 是描述文字，不授予任何權限。權限只來自：operator、`managers`（設定檔列名）、team 的 `orchestrator`、以及「對自己」。

| 動作 | operator | manager | orchestrator | instance |
|---|---|---|---|---|
| create／start／delete instance、team 異動、deploy／teardown | 可 | 否 | 否 | 否 |
| restart | 任何 instance | 否 | 否 | 只限自己 |
| interrupt | 任何 | 任何 | 自己 team 的成員 | 否 |
| pane_snapshot、list_instances | 任何 | 任何 | 自己 team 的成員 | 自己（list 可看全部名稱與狀態） |
| send | 任何 | 任何 | 見 §6.2 | 見 §6.2 |
| inbox | 否 | 只限自己 | 只限自己 | 只限自己 |

## 3. 請求與結果

每個操作帶可選的 `operation_id`（呼叫端未給時由 daemon 產生）。同一 `operation_id` 重送時回傳第一次的記錄結果，不重複執行副作用。

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
- `partial`：部分成功；`value` 逐項列出每個子結果。
- `refused`：沒有任何副作用；`error.next` 說明誰能做什麼下一步。
- `unsupported`：本期不支援的工具、action、欄位或值，在任何變更前拒絕，code 為 `unsupported_in_phase`。

錯誤碼：`unauthorized`、`not_found`、`name_conflict`、`invalid_argument`、`stale_generation`、`operation_in_progress`、`spawn_failed`、`resume_unavailable`、`recovery_required`、`delivery_token_stale`、`never_delivered`、`team_acl_denied`、`unsupported_in_phase`、`daemon_already_running`、`internal`。

## 4. Instance 生命週期

### 4.1 規格

`InstanceSpec`：`name`、`backend`（`claude` 或 `command`）、`command`（覆寫可執行檔）、`args`、`model`、`effort`、`working_directory`、`instructions`、`skills_path`、`env`、`team`、`description`、`role`。

未給 `working_directory` 時使用 `$OTTERHIVE_HOME/workspace/<name>`。delete 不刪除任何 working directory。

狀態：`stopped` → `starting` → `running` → `stopping` → `stopped`；另有 `failed`（附原因）與 `recovery_required`。

### 4.2 start

1. 在交易中寫入 journal intent，generation 加一，發新 token。
2. 物化 instructions 與 skills（§5），在 PTY 中 spawn。PTY 子程序是 session leader，`pgid == pid`；記錄 pid 與程序啟動時間。
3. 子程序在啟動寬限期（3 秒）內結束：狀態 `failed`，原因 `exited_during_startup`，附 exit status。不自動重試、不 crashloop。

### 4.3 stop（restart、delete、teardown 共用）

1. 確認記錄的 pid 仍是同一程序（啟動時間相符），對 process group 發 SIGTERM，等 5 秒。
2. 仍存活則 SIGKILL，等 2 秒；回收子程序並 join 讀取執行緒。
3. 確認 process group 已無成員才算停止。無法確認時狀態為 `recovery_required`，回報殘留 pid，拒絕同一 instance 的新 spawn。daemon 不對身分不明的 pid 發 signal。

### 4.4 restart

- `mode`：`fresh`（預設）或 `resume`。
- `resume` 先檢查 backend 的 session 證據（§5.3）。沒有證據回 `resume_unavailable`，原程序與狀態不變。
- `fresh` 先把該 instance 所有 `delivering` 訊息退回 `unread`（舊 token 失效）；`resume` 保留 lease。
- 依 §4.3 停止舊程序；確認停止後才以新 generation spawn。停止無法確認時回 `recovery_required`，不 spawn。
- 呼叫端帶 `expected_generation` 而不符時回 `stale_generation`。同一 instance 已有進行中的生命週期操作時回 `operation_in_progress`。
- instance 重啟自己時，daemon 先回 `queued` 再執行；新程序可用 `list_instances` 確認新 generation。

### 4.5 delete

依 §4.3 停止後，標記 identity 為已刪除，封存其 inbox；config 中的定義一併移除。停止無法確認時回 `recovery_required`，不刪除。

### 4.6 daemon 重啟後

PTY 檔案描述元隨舊 daemon 消失，無法重新接上。daemon 啟動時，對記錄為 `starting`、`running`、`stopping` 的 instance 依 §4.3 停止仍存活的程序：成功則為 `stopped`（原因 `daemon_restarted`），否則為 `recovery_required`。P1a 不自動重新啟動；operator 以 start 或 deploy 再起。

## 5. Backend adapter

### 5.1 `claude`

- 可執行檔：`command` 或 `claude`。預設參數 `--dangerously-skip-permissions`；`model` → `--model <v>`；`effort` → `--effort <v>`；`args` 附加在後。
- instructions：daemon 把 P1 協調提示與 `instructions` 檔內容寫入 `<workdir>/.claude/otterhive.md`，以 `--append-system-prompt-file` 傳入；MCP 設定寫入 `<workdir>/.claude/otterhive-mcp.json`，以 `--mcp-config` 傳入。
- skills：`skills_path` 存在時建立 `<workdir>/.claude/skills` symlink。該路徑若已是非 OtterHive 建立的檔案或目錄，不覆蓋並在結果中回報。
- 送出鍵 `\r`。

### 5.2 `command`

只執行 `command` 加 `args`，不加任何參數、不支援 resume。供 fixture 與 shell 使用。

### 5.3 resume 證據

`claude`：`$HOME/.claude/projects/<編碼>/` 下至少一個 `.jsonl` 含 `"type":"user"` 紀錄。編碼為 canonical working directory 中非 `[A-Za-z0-9-]` 的字元換成 `-`。有證據時以 `--continue` spawn。`command` 一律 `resume_unavailable`。

這些規則以 fake executable 測試；真 Claude CLI 的行為在 operator 授權的 canary 驗證前，標記為未驗證。

## 6. 訊息

### 6.1 send

- 參數：`instance`、`instances`、`team` 三擇一；`message`（非空，最多 64 KiB）；`request_kind` 為 `query`、`report`、`update` 或省略；`correlation_id`、`parent_id`、`thread_id` 為不透明字串（各最多 256 字元）。
- 其他欄位與 `request_kind: task` 回 `unsupported_in_phase`（清單見 catalog）。
- team 廣播的收件人是該 team 除 sender 以外的所有成員。
- 對每個收件人：寫入一筆 `unread` 訊息並給 message ID，或回報拒絕原因。全部成功為 `completed`，部分為 `partial`，全部失敗為 `refused`。
- 寫入在同一交易內完成並 fsync 後才回應。喚醒（§6.4）在寫入之後進行；喚醒失敗不撤回訊息。

### 6.2 收件權限

operator 與 manager 可送給任何 instance。instance sender 送給 instance recipient 時：

- recipient 不屬於任何 team：允許。
- 同一 team：允許。
- recipient 是某 team 的 orchestrator，且 sender 在該 team 的 `accept_from`：允許。
- 其他：`team_acl_denied`。

### 6.3 inbox

- drain（`action` 省略）：只能讀自己的 inbox。依建立順序取 `unread` 與 lease 已過期的 `delivering` 訊息，總量上限 48 KiB（至少一筆）；改為 `delivering`，發一個新的 `delivery_token`，lease 600 秒，`delivery_count` 加一。前一批未 ack 的訊息**不會**因再次 drain 而被結算。
- ack：`message_id` 加 `delivery_token`。token 相符則為 `acked`；重複 ack 同一組回 `completed`（`already_acked`）。token 不符或已過期回 `delivery_token_stale`；從未投遞回 `never_delivered`。
- 保證是至少一次投遞：lease 逾時、fresh restart、daemon 重啟都會讓未 ack 的訊息再次被 drain。不承諾恰好一次。

### 6.4 喚醒

送達後若收件人在 `running`，daemon 經 PTY 送出一行提示：`[OTTERHIVE-MSG] from=<sender> kind=<kind> id=<message_id>` 加送出鍵，提示 agent 呼叫 inbox。

- 有 client 在最近 3 秒內輸入過（operator 可能在打字），或收件人最近 500 毫秒內有輸出，就延後提示，最多延後 30 秒後再判斷一次；同一收件人的多個提示合併為一個。
- 喚醒結果記為 `hinted`、`deferred`、`not_running` 或 `failed`，回報在 send 的逐收件人結果中。`hinted` 只代表提示已寫入 PTY，不代表 agent 已讀或已處理。

## 7. 終端

- 連線送 `attach`（instance、可選 `expected_generation`），之後改為 frame 串流。frame：1 byte tag、4 byte big-endian 長度、內容。
- daemon → client：`Snapshot`（generation、seq、cols、rows、畫面 bytes）、`Output`（seq、bytes）、`Resync`（接著一個新的 Snapshot）、`Ended`（generation、原因）、`Error`。
- client → daemon：`Input`（generation、bytes）、`Resize`（generation、cols、rows）、`Detach`。generation 不符的 Input／Resize 被丟棄並回 `Error`。
- Snapshot 與訂閱在同一把鎖內完成：之後的 `Output` seq 從 snapshot 的 seq 加一開始，不重複也不遺漏。
- client 跟不上時，daemon 丟棄排隊的輸出並送 `Resync`；PTY 讀取永不因 client 阻塞。
- instance restart 後舊連線收到 `Ended`；client 需以新 generation 重新 attach，舊程序的輸出不會出現在新連線上。
- detach 或 client 結束不影響 instance 與 daemon。
- `pane_snapshot` 回傳目前畫面的純文字（去除 ANSI），`lines` 限制最後幾行。

## 8. Team 與 deployment

### 8.1 team

`create`（`name`、`members`、`orchestrator`、`description`、`accept_from`）、`update`（`add`、`remove`、`orchestrator`、`description`、`accept_from`）、`delete`、`list`。`orchestrator` 必須是成員。一個 instance 最多屬於一個 team。刪除 team 不刪除 instance。

### 8.2 template

`otterhive.yaml` 的 `templates.<name>` 接受舊 fleet.yaml 的格式：`orchestrator`、`instances.<member>` 下的 `role`、`backend`、`command`、`model`、`effort`、`args`、`instructions`、`skills_path`、`env`、`working_directory`。其他鍵（例如 `source_repo`、`project_id`、`github_login`）不生效，並列在 deploy 結果的 `ignored_template_fields`。

### 8.3 deploy

- 參數：`template`、`name`、可選 `directory`（作為所有成員的 working directory）；`branch` 回 `unsupported_in_phase`。
- 成員 instance 名稱為 `<name>-<member>`，team 名稱為 `<name>`，orchestrator 為 `<name>-<orchestrator>`。
- 先在交易中建立 deployment 記錄、team 與所有成員定義，再逐一 start。每個成員回報 `started`、`already_running` 或 `failed`（含錯誤碼）。有任何失敗即為 `partial`，已啟動的成員保持運行。
- 對同名 deployment 再次 deploy：只 start 未在運行的成員，不重開已在運行的成員。名稱已被非此 deployment 擁有的 instance 或 team 使用時，整個 deploy 回 `name_conflict`，沒有副作用。

### 8.4 teardown

依 §4.3 停止並刪除該 deployment 擁有的每個成員。全部成功則刪除 team 與 deployment 記錄。任何成員停止無法確認時，結果為 `partial`，deployment 記錄保留並列出殘留，可在處理後再次 teardown。
