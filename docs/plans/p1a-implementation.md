# P1a 實作計畫

**Source of truth：**[P1a 協作契約](../specs/p1-coordination.md)（行為）、[ADR 0001](../adr/0001-runtime-boundaries.md)（取捨）、[`mcp-catalog.json`](../reverse-engineering/mcp-catalog.json)（欄位）。三者與本計畫衝突時，以契約為準，並回報差異。

**Goal：**在暫存的 `OTTERHIVE_HOME` 中，operator 以 template deploy 一個 team；成員經真正的 MCP bridge 互相送 query／report、被喚醒、drain 與 ack；成員可 fresh restart，終端可重新 attach；teardown 後沒有殘留程序。以上全部用 fake backend 自動化驗證。

**Architecture：**單一 crate `otterhive`，兩個 binary：`otterhive`（daemon、operator CLI、attach client）與 `otterhive-mcp-bridge`。daemon 擁有 PTY、子程序與 SQLite；同步 I/O 加執行緒。

**Tech stack：**Rust stable（`rust-version = "1.88"`；開發機 rustc 1.98.1）。依賴與 2026-10-12 在 crates.io 查到的版本：`rusqlite 0.40`（`bundled`）、`portable-pty 0.9`、`alacritty_terminal 0.26`、`serde 1`、`serde_json 1`、`serde_yaml_ng 0.10`、`thiserror 2`、`uuid 1`（`v4`）、`libc 0.2`、`getrandom 0.3`、`sha2 0.11`、`clap 4`（`derive`）、`crossterm 0.29`；dev：`tempfile 3`。

## 目前狀態

- 已完成：P0 與 T2（protocol、admission、home、config、store）；T2 已經由 PR #1 合併。
- T3 基準：`main`／`origin/main` 為 `b1a42b45f9f65bc2d4ccfc26f7f3378768f90840`；本包使用獨立整合分支，不直接修改 main。
- T3 已集中 RED：程序／Session、Screen／Frame 的測試與四個 fixtures，尚無行為實作。`cargo test --locked --lib --test process_stop --test terminal_session --test frame` 及不含 `--lib` 的三組整合測試命令皆實際 exit 101（compile-RED：缺少待實作的型別／函式）；Screen 與 procs 私有測試已接入模組編譯，不將零測試執行視為 RED。`cargo fmt --check` 通過。
- T3 共 73 個測試宣告（35 個整合、38 個單元）；尚未執行成功，不宣稱 H1／H2／H3／H8 已驗證。macOS CI 與 Rust 1.88 編譯驗收沿用 T2 jobs，完整 GREEN 後才發 PR。

## 全域約束

- 不修改 `~/.agend`、正式 fleet、`agend-terminal` 或 `agend-customization`。舊 source 可唯讀參考（GitHub `cheerc/agend-terminal`，commit `593da97c6f94d874f793777b4550d5df2ae4f43c`）。所有測試用 `tempfile` 建的 `OTTERHIVE_HOME`、`HOME` 與 cwd。
- 未經 operator 授權不執行付費 backend。backend 行為只用 `tests/fixtures/backends/` 的 fake executable。
- 每個工作包先 commit 會失敗的測試（RED），再 commit 實作（GREEN）。編譯失敗的 RED 要標明是 compile-RED。
- 每個工作包結束前跑：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test --all-targets`，全部通過。
- 禁用 `unwrap()`（`[lints.clippy] unwrap_used = "deny"`，`clippy --all-targets` 也會檢查測試）。測試改用附說明的 `expect("…")`。
- 每個 `thread::spawn` 要保存 `JoinHandle` 並在關閉時 join，或在旁註明 `// fire-and-forget: <理由>`。
- 測試不靠任意 `sleep` 碰運氣：等待條件時用有上限的輪詢（例如每 20 毫秒檢查、最多 10 秒），失敗時印出最後觀察到的狀態。
- 時間與容量常數只在測試中可調整，透過 daemon 啟動時讀的環境變數：`OTTERHIVE_TEST_STARTUP_GRACE_MS`（3000）、`OTTERHIVE_TEST_STOP_TERM_MS`（5000）、`OTTERHIVE_TEST_STOP_KILL_MS`（2000）、`OTTERHIVE_TEST_LEASE_MS`（600000）、`OTTERHIVE_TEST_WAKE_INPUT_MS`（3000）、`OTTERHIVE_TEST_WAKE_QUIET_MS`（500）、`OTTERHIVE_TEST_SUBSCRIBER_QUEUE`（256）。括號內是契約的預設值，未設定時一律使用；這些變數不寫進對外文件。
- 未實作的工具、action、欄位一律依契約回 `unsupported_in_phase` 或 `invalid_argument`，不靜默忽略。

## 證據與待驗證假設

已確立，分兩類（見 [證據索引](../reverse-engineering/evidence-index.md)）：

- REA 觀測：舊版在 PTY 中 spawn fake backend，CJK 與 ANSI 正確進入 VT，只寫入 `AGEND_HOME/captures`（`ev_33e9c28b…`、`ev_b0853814…`）。
- 靜態 source 推論：未呼叫 tmux；終端用 portable-pty 加 alacritty_terminal；Claude adapter 的參數與 resume 判斷在 `agend-terminal@593da97c:src/backend.rs:417–421`、`:854–881`、`:1100–1184`。這些只能當參考，不是觀測到的行為。

待驗證（實作者在對應工作包的第一個測試驗證；不成立就停下回報，不自行改設計）：

| ID | 假設 | 驗證方式 | 工作包 |
|---|---|---|---|
| H1 | portable-pty 0.9 在 macOS 讓子程序成為 session leader（`pgid == pid`） | 測試 `spawned_child_leads_its_process_group` | T3 |
| H2 | `libc` 在 macOS 提供 `proc_pidinfo`／`proc_bsdinfo`，可取得程序啟動時間 | 測試 `process_start_time_is_stable_for_a_live_pid` | T3 |
| H3 | alacritty_terminal 0.26 的 `Term` 與 `vte::ansi::Processor` 可在不依賴 test 模組的情況下建立 | 編譯並通過 T3 畫面測試；可參考 `agend-terminal@593da97c:src/vterm.rs` | T3 |
| H4 | 真 Claude Code 在 idle 時收到提示行加 `\r` 會當成使用者輸入 | native canary（T8） | T8 |
| H5 | 真 Claude Code 能以 stdio 連上 bridge（回傳它要求的 `protocolVersion`） | native canary | T8 |
| H6 | 安裝中的 Claude 版本支援 `--append-system-prompt-file`、`--mcp-config`、`--continue`、`--effort` | native canary | T8 |
| H7 | 提示注入不會破壞 Claude 輸入框中的既有內容 | native canary | T8 |
| H8 | macOS 可用 `waitid(WEXITED \| WNOHANG \| WNOWAIT)` 觀測 leader exit 而不 reap，保留 child 身分到 group 清理完成；portable-pty 的 child API 不會在其他路徑提前 reap | T3 查核 macOS waitid 文件與 portable-pty 0.9 的 wait／try_wait／Drop 路徑；以 `leader_exit_observation_keeps_the_owned_child_unreaped`、`stop_after_leader_exit_cleans_remaining_group_members`、`revoked_leader_lease_cannot_signal_a_reused_group` 驗證。不能保留或重新驗證 lease 就停止回報，不以「非空 group」假設替代證據 | T3 |

## 檔案與模組

| 路徑 | 責任 | 工作包 |
|---|---|---|
| `Cargo.toml` | 套件、依賴、lint | T2 |
| `src/lib.rs` | 模組宣告 | T2 |
| `src/protocol.rs` | `Request`、`Auth`、`OperationResult`、`Outcome`、`OpError`、`ErrorCode`、`Subject` | T2 |
| `src/admission.rs` | 每個操作允許的欄位、action、列舉值；`admit`、`op_spec`、`deferred_phase` | T2 |
| `src/home.rs` | home 目錄配置與權限、`DaemonLock`、operator token、隨機 token、token 雜湊 | T2 |
| `src/config.rs` | 讀 `otterhive.yaml`（templates、managers） | T2 |
| `src/store/mod.rs`、`src/store/schema.sql` | SQLite 開啟、migration、交易、各表的讀寫 | T2 |
| `src/procs.rs` | 程序啟動時間、group 是否還有成員、有證據的停止 | T3 |
| `src/terminal/screen.rs` | VT 包裝：餵入 bytes、ANSI 重繪 snapshot、純文字列 | T3 |
| `src/terminal/session.rs` | PTY spawn、讀取執行緒、訂閱者、input、resize、等待結束 | T3 |
| `src/terminal/frame.rs` | §7.2 frame 編解碼 | T3 |
| `src/backend/{mod,claude,command}.rs` | 啟動參數、協調提示與 MCP 設定物化、skills symlink、resume 證據 | T4 |
| `src/daemon/{mod,auth,lifecycle,recovery}.rs` | socket 服務、授權、lifecycle 操作、daemon 重啟後收斂 | T4 |
| `src/client.rs` | 送請求、讀回應；CLI、bridge、測試共用 | T4 |
| `src/cli.rs`、`src/bin/otterhive.rs` | §9 operator CLI | T4 起，T5–T7 擴充 |
| `src/messaging.rs`、`src/wake.rs` | send、收件權限、inbox、喚醒排程 | T5 |
| `src/mcp.rs`、`src/bin/otterhive-mcp-bridge.rs` | 工具定義與 `tools_list`、stdio bridge；T4 完成 transport 與 lifecycle 工具，T5 擴充通訊／team 工具 | T4 起 |
| `src/teams.rs` | team 操作（收件權限依賴它） | T5 |
| `src/deployments.rs` | deploy、teardown | T6 |
| `src/terminal/attach.rs` | attach client（raw mode、`Ctrl+B d`、輪詢終端大小） | T7 |
| `scripts/smoke-p1.sh` | 以 CLI 跑完整情境，供 REA 取證 | T7 |
| `tests/common/mod.rs` | 測試共用：在暫存 home 啟動與停止 daemon、`call`、原始 frame client、有上限的輪詢 | T4 起 |

### SQLite schema（內部，可在實作時調整，但要保住契約；`instances.pid` 與 `instances.pid_start` 的欄位名固定，測試會直接修改它們來模擬身分不符）

```sql
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);          -- schema_version = 1
CREATE TABLE instances(
  id TEXT PRIMARY KEY, name TEXT NOT NULL, spec TEXT NOT NULL,         -- spec 為 JSON
  state TEXT NOT NULL, reason TEXT, generation INTEGER NOT NULL DEFAULT 0,
  pid INTEGER, pid_start INTEGER, token_hash TEXT,
  deployment TEXT, deleted_ms INTEGER, created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL);
CREATE UNIQUE INDEX instances_live_name ON instances(name) WHERE deleted_ms IS NULL;
CREATE TABLE teams(name TEXT PRIMARY KEY, orchestrator_id TEXT NOT NULL, description TEXT,
  accept_from TEXT NOT NULL DEFAULT '[]', deployment TEXT);
CREATE TABLE team_members(team TEXT NOT NULL, instance_id TEXT NOT NULL UNIQUE,
  PRIMARY KEY(team, instance_id));
CREATE TABLE messages(
  seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, recipient_id TEXT NOT NULL,
  sender_kind TEXT NOT NULL, sender_id TEXT, sender_name TEXT NOT NULL, kind TEXT NOT NULL,
  body TEXT NOT NULL, correlation_id TEXT, parent_id TEXT, thread_id TEXT,
  state TEXT NOT NULL,                                                  -- unread|delivering|acked|archived
  delivery_token TEXT, lease_expires_ms INTEGER, delivery_count INTEGER NOT NULL DEFAULT 0,
  created_ms INTEGER NOT NULL, acked_ms INTEGER);
CREATE TABLE operations(id TEXT PRIMARY KEY, op TEXT NOT NULL, caller TEXT NOT NULL,
  result TEXT NOT NULL, created_ms INTEGER NOT NULL);
CREATE TABLE deployments(name TEXT PRIMARY KEY, template TEXT NOT NULL, team TEXT NOT NULL,
  residuals TEXT NOT NULL DEFAULT '[]', created_ms INTEGER NOT NULL);
CREATE TABLE journal(id INTEGER PRIMARY KEY AUTOINCREMENT, instance_id TEXT, generation INTEGER,
  kind TEXT NOT NULL, detail TEXT, at_ms INTEGER NOT NULL);
```

開啟時設定 `journal_mode=WAL`、`synchronous=FULL`、`foreign_keys=ON`，交易用 `BEGIN IMMEDIATE`。token 只存 SHA-256 雜湊。

### Fixture backends（`tests/fixtures/backends/`）

每個 fixture 由第一個需要它的工作包建立（表中「建立於」）。

| 檔案 | 行為 | 建立於 |
|---|---|---|
| `capture_stub.sh` | 已存在：印出 ready 標記與 CJK／ANSI 後結束 | P0 |
| `echo_agent.sh` | 印 `ready>`；逐行讀 stdin，把每行（含 ESC 等原始 bytes 的十六進位）附加到 `$OTTERHIVE_FIXTURE_LOG`，並回印 `echo: <行>`；收到 `size` 時印出 `stty size`；收到 `flood` 時持續輸出直到收到 `stop` | T3 |
| `crash_on_start.sh` | 立即 `exit 3` | T3 |
| `ignore_term.sh` | `trap '' TERM`；印 ready 後無限等待 | T3 |
| `spawns_child.sh` | 在背景啟動 `sleep 600`（同一 process group），把子程序 pid 寫入 `$OTTERHIVE_FIXTURE_LOG`，然後等待；設了 `FIXTURE_LEADER_EXIT_AFTER_MS` 就在該毫秒數後自己結束，留下子程序 | T3 |
| `fake_claude.sh` | 把收到的 argv 與 `OTTERHIVE_*` 環境變數名稱寫入 `$OTTERHIVE_FIXTURE_LOG`，之後行為同 `echo_agent.sh` | T4 |
| `mcp_agent.py` | 以 `python3 -I` 執行。讀 PTY 輸入；遇到 `[OTTERHIVE-MSG]` 就啟動真的 `otterhive-mcp-bridge`，走 `initialize` → `tools/call inbox` → 逐則 ack。訊息內容為 `relay:<target>:<text>` 時，送 query `<text>` 給 target 並帶 `parent_id`；收到 query 時回 report `pong:<text>` 給 sender 並帶 `parent_id`；所有收到的訊息與自己的 pid 寫入 `$OTTERHIVE_FIXTURE_LOG`。bridge 路徑與 token 從 `OTTERHIVE_MCP_BRIDGE`、`OTTERHIVE_INSTANCE_TOKEN` 取得 | T5 |

## 工作包

依序進行；每包是一個可獨立驗收的 PR 單位（見「PR 邊界」）。

### T2：protocol、admission、home、config、store

**Interfaces（產出）：**

```rust
// protocol
pub struct Request { pub v: u32, pub auth: Auth, pub op: String, pub args: Value, pub operation_id: Option<String> }
pub enum Auth { Operator(String), Instance(String) }               // JSON: {"operator": "…"} / {"instance": "…"}
pub enum Outcome { Completed, Queued, Partial, Refused, Unsupported }
pub enum ErrorCode { /* 契約 §3 的 15 個，snake_case 序列化 */ }
pub struct OpError { pub code: ErrorCode, pub message: String, pub retryable: bool, pub next: Option<String> }
pub struct OperationResult { pub operation_id: String, pub outcome: Outcome, pub subject: Option<Subject>,
                             pub value: Value, pub effects: Vec<String>, pub error: Option<OpError> }
impl OperationResult { pub fn completed(id: &str, value: Value) -> Self; pub fn refused(id: &str, code: ErrorCode, msg: &str) -> Self; }
// admission
pub struct OpSpec { pub name: &'static str, pub fields: &'static [&'static str], pub legacy_fields: &'static [&'static str], /* actions、列舉值 */ }
pub fn admit(op: &str, args: &Value) -> Result<(), OpError>;
pub fn op_spec(op: &str) -> Option<&'static OpSpec>;
pub fn deferred_phase(tool: &str) -> Option<&'static str>;          // 非 P1a 工具的 catalog phase
// home
pub struct DaemonLock;  impl DaemonLock { pub fn acquire(run_dir: &Path) -> Result<DaemonLock, LockError>; }
pub enum LockError { AlreadyHeld, Io(std::io::Error) }
// store
pub struct Store;  impl Store {
  pub fn open(path: &Path) -> Result<Store, StoreError>;
  pub fn transaction<T>(&mut self, f: impl FnOnce(&Tx) -> Result<T, StoreError>) -> Result<T, StoreError>;
  pub fn recorded_operation(&self, id: &str) -> Result<Option<RecordedOperation>, StoreError>;
  pub fn instance(&self, id: &str) -> Result<Option<InstanceRow>, StoreError>;
  pub fn live_instance_by_name(&self, name: &str) -> Result<Option<InstanceRow>, StoreError>;
  pub fn message_count(&self, recipient_id: &str) -> Result<u64, StoreError>; }
// Tx: insert_instance(&NewInstance) -> InstanceRow, mark_instance_deleted(id), bump_generation(id, expected: Option<u64>) -> u64,
//     insert_message(&NewMessage) -> String, record_operation(id, op, caller, &OperationResult)
pub enum StoreError { NameConflict(String), StaleGeneration { id: String, current: u64 }, DuplicateOperation(String),
                      UnsupportedSchema { found: i64, supported: i64 }, Invalid(String), Sqlite(rusqlite::Error) }
```

**測試：**`tests/protocol_admission.rs`、`tests/store_recovery.rs`、`tests/home_and_config.rs`

- `governed_send_fields_are_unsupported_before_mutation`：`send` 帶 `branch`、`task_id`、`terminal`、`review_assignment`、`next_after_ci`、`force` 任一 → `unsupported_in_phase`。
- `unsupported_fields_name_the_field_not_the_tool_phase`：`create_instance.team`、`send.branch` 的錯誤指出該欄位與 P1a 不支援，不以工具的 P1a phase 宣稱欄位將在 P1a 實作；整個 deferred 工具仍顯示 catalog phase。
- `request_kind_task_is_unsupported_and_unknown_kind_is_invalid`：`task` → unsupported；`shout` → invalid；`query`／`report`／`update` 通過。
- `unknown_field_is_invalid_and_non_object_args_are_invalid`：未知欄位、陣列 args → invalid；`null` args 視為空物件。
- `legacy_actions_outside_p1a_are_unsupported`：inbox `clear`／`discharge` → unsupported，`explode` → invalid；drain 與 ack 通過；restart `mode: warm` → invalid。
- `deferred_tools_are_unsupported_and_name_their_phase`：`task`→P2、`ci`→P3、`schedule`→P4、`set_model`→deferred-operator、`reply`→deferred-channel、`move_pane`→P1b；未知工具 → invalid。
- `admission_matches_the_migration_catalog`：讀 `docs/reverse-engineering/mcp-catalog.json`；每個 P1a 工具的 `OpSpec.fields` 等於 catalog 中 `p1a.supported_fields ∪ p1a.added_fields`，`p1a.refused_fields` 都在 `OpSpec.legacy_fields`，`p1a.supported_values` 等於 admission 接受的列舉值；非 P1a 工具的 `deferred_phase` 等於 catalog 的 phase。
- `request_and_result_round_trip_as_json_lines`：契約 §1 的請求格式可解析；refused 結果序列化為單行且 `error.code` 為 snake_case。
- `rolled_back_transaction_leaves_no_partial_message`、`retried_operation_returns_the_recorded_result`（重複記錄回 `DuplicateOperation`，原結果不變）、`stale_generation_is_rejected_without_writing`、`live_names_are_unique_and_deleted_names_are_reusable`、`state_survives_reopen`。
- `newer_schema_is_refused_and_left_untouched`：預先建立 `schema_version = 99` 的資料庫 → `UnsupportedSchema`，且沒有建立任何新表。
- `second_daemon_lock_on_same_home_is_refused`：第二次 `acquire` → `AlreadyHeld`；釋放後可再取得。
- `home_directories_and_secrets_have_owner_only_permissions`：`run/`、`secrets/` 為 0700，`operator.token` 為 0600，重複初始化不改變 token。
- `config_accepts_legacy_template_format_and_reports_ignored_fields`：含 `source_repo`、`project_id`、`github_login` 的 template 可讀，這些鍵列為 ignored。
- `config_rejects_unknown_top_level_keys`：含 `channel` 的設定檔被拒，錯誤訊息列出允許的鍵；沒有設定檔時視為空設定。設定檔路徑為 `$OTTERHIVE_HOME/otterhive.yaml`。

**命令：**RED／GREEN 皆為 `cargo test --test protocol_admission --test store_recovery --test home_and_config`。

**驗收：**上列測試全過；全域檢查通過。

### T3：程序與終端 session（尚無 daemon）

**Interfaces：**

```rust
pub fn process_start_time(pid: i32) -> Option<u64>;
pub struct OwnedLeaderLease;                                      // 私有欄位；Session spawn 成功後建立，綁定 pid/start/pgid 與尚未 reap 的 child
pub enum GroupEvidence<'a> { OwnedLeader(&'a OwnedLeaderLease), RecordedLeader } // recovery 只能使用 RecordedLeader
pub enum StopOutcome { Stopped { escalated_to_kill: bool }, Unproven { residual_pids: Vec<i32> }, IdentityMismatch }
pub fn stop_group(pid: i32, expected_start: u64, evidence: GroupEvidence<'_>,
                  term_wait: Duration, kill_wait: Duration) -> StopOutcome;
pub struct SessionConfig { pub program: String, pub args: Vec<String>, pub cwd: PathBuf, pub env: Vec<(String, String)>,
                           pub size: (u16, u16), pub generation: u64, pub subscriber_queue: usize }
pub struct Snapshot { pub generation: u64, pub seq: u64, pub cols: u16, pub rows: u16, pub ansi: Vec<u8> }
pub struct Session;  impl Session {
  pub fn spawn(cfg: SessionConfig) -> Result<Session, SpawnError>;
  pub fn pid(&self) -> i32;  pub fn start_time(&self) -> u64;
  pub fn owned_leader_lease(&self) -> &OwnedLeaderLease;
  pub fn write_input(&self, bytes: &[u8]) -> io::Result<()>;
  pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()>;
  pub fn subscribe(&self) -> (Snapshot, Subscriber);                 // 同一把鎖內取 snapshot 並登記
  pub fn plain_text(&self) -> Vec<String>;
  pub fn last_output_at(&self) -> Instant;
  pub fn exit_status(&self) -> Option<ExitInfo>;                     // waitid + WNOWAIT 觀測執行緒寫入，不 reap
  pub fn wait_exit(&self, timeout: Duration) -> Option<ExitInfo>; }   // 等待已觀測的 exit status，不 reap
pub enum StreamEvent { Output { seq: u64, bytes: Vec<u8> }, Resync { snapshot: Snapshot }, Ended }
pub fn encode_frame(tag: u8, payload: &[u8]) -> Vec<u8>;  pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)>;
```

`subscriber_queue` 必須大於 0，production 預設 256；daemon 從測試設定傳入，Session 本身不讀環境變數。`Resync { snapshot }` 在同一把 Session 鎖內擷取畫面並重設訂閱邊界；socket writer 將它依序編碼成 Resync 與 Snapshot 兩個 frame。reader 的 seq 計數與 Snapshot.seq 定義依契約 §7.1，不以送到某個 client 的次數計數。

`OwnedLeaderLease` 不是 LiveSession 布林旗標：只在本 daemon 確認持有 spawn 的 child 時建立，不可由 SQLite 重建。`stop_group` 每次 signal 前，在與 reap 共用的 lifecycle 鎖內驗證 lease 未撤銷、child 未被 reap、pid／啟動時間／pgid 相符；owned 路徑必須與傳入的 pid／expected_start 相符。kernel 回 ECHILD、查詢失敗或任何 reap 都使 lease 永久失效，回 Unproven／IdentityMismatch 而不 signal。group 清理完成後先撤銷 lease，再在鎖內 reap，釋放鎖後 join 執行緒；因停止無法證實而逾時時保留 Session，不無限等待 join。清理後保留畫面／Session 物件不恢復 lease。exit observer 不呼叫 portable-pty 的 wait／try_wait；H8 若不能證明 API 路徑，停在 spike，不改成舊 pgid 清理。

**Fixture：**建立 `echo_agent.sh`、`crash_on_start.sh`、`ignore_term.sh`、`spawns_child.sh`（見上方 fixture 表）。

**測試：**`tests/process_stop.rs`、`tests/terminal_session.rs`、`tests/frame.rs`

- `spawned_child_leads_its_process_group`（H1）、`process_start_time_is_stable_for_a_live_pid`（H2）。
- `output_reaches_screen_and_plain_text`：`capture_stub.sh` 的 CJK 與 ANSI 出現在 `plain_text`，ANSI 已去除。
- `snapshot_then_output_sequence_has_no_gap_or_duplicate`：訂閱後的 `Output.seq` 從 snapshot 的 seq 加一連續遞增。
- `sequence_advances_without_subscribers_and_snapshot_includes_it`：未訂閱時先輸出，再 subscribe，Snapshot.seq 大於 0 且畫面含該輸出；之後 Output 從 Snapshot.seq 加一接續。
- `slow_subscriber_gets_resync_and_reader_never_blocks`：`SessionConfig.subscriber_queue` 設小（daemon 從 `OTTERHIVE_TEST_SUBSCRIBER_QUEUE` 傳入）；`echo_agent.sh` 的 `flood` 讓不讀取的訂閱者佇列滿；另一個訂閱者與 `plain_text` 仍持續更新；停滯的訂閱者恢復讀取後先收到含 Snapshot 的 `Resync` 事件，後續 Output 是該 Snapshot.seq 加一；Resync 不增加全域 seq。
- `resize_updates_pty_and_screen`：resize 到 100×30 後，`echo_agent.sh` 對 `size` 回 `30 100`。
- `stop_terminates_and_reaps`：`echo_agent.sh` → `Stopped { escalated_to_kill: false }`。
- `stop_escalates_to_kill_when_term_is_ignored`：`ignore_term.sh` → `Stopped { escalated_to_kill: true }`。
- `stop_removes_children_in_the_same_group`：`spawns_child.sh` 的子程序在 stop 後不存在。
- `leader_exit_observation_keeps_the_owned_child_unreaped`（H8）：`spawns_child.sh` 設 `FIXTURE_LEADER_EXIT_AFTER_MS`；wait_exit 取得 exit status 後仍能以 WNOWAIT 觀測同一 waitable child，lease 的 pid／啟動時間／pgid 可重新驗證；child 未在 observer／try_wait 路徑被 reap。
- `stop_after_leader_exit_cleans_remaining_group_members`：以 `GroupEvidence::OwnedLeader(session.owned_leader_lease())` 清理剩下的 group；child 消失後才撤銷 lease 與 reap leader，stop 回 Stopped。
- `recorded_leader_without_an_owned_lease_cannot_authorize_a_nonempty_group`：先清理／reap fixture leader 但保留它的子程序，再用歷史 pid／啟動時間與 `GroupEvidence::RecordedLeader` → Unproven，child 未收到 signal；測試 cleanup 使用另外保存並核對的 fixture child 身分，不以歷史 group 編號清理。
- `revoked_leader_lease_cannot_signal_a_reused_group`：保留已完成清理的舊 Session；用測試用 OS probe／signal adapter 讓同一舊 pgid 回報無關的新 group（不實際等待 PID 重用）。由真正 stop_group 入口帶入已撤銷的 lease → Unproven，signal adapter 的呼叫數為 0；持有物件或查到非空 group 都不能恢復 lease。
- `lease_validation_and_reap_are_serialized`：以 barrier 讓 stop 驗證／signal 與清理執行緒的 reap 競爭；同一 lifecycle 鎖保證不會在驗證後、signal 前 reap，lease 撤銷後不得發任何新 signal。
- `stop_refuses_when_start_time_does_not_match`：傳入錯誤的啟動時間 → `IdentityMismatch`，程序仍存活（測試自行清理）。
- `child_exit_is_observed`：`crash_on_start.sh` 的 exit code 3 可由 `wait_exit` 取得。
- `frame_round_trip_and_oversize_is_rejected`：所有 tag 編解碼一致；超過 1 MiB 的長度回錯誤。

**命令：**`cargo test --test process_stop --test terminal_session --test frame`。

**參考：**`agend-terminal@593da97c` 的 `src/vterm.rs:1005–1089`（畫面重繪）、`src/agent/broadcast.rs:47–88`（非阻塞廣播）、`src/agent/mod.rs:1331–1586`（PTY spawn）、`src/framing.rs`。只參考做法，契約以本 repo 為準。

### T4：daemon、授權、lifecycle、backend adapter、bridge transport、CLI 基礎

**範圍：**契約 §1–§5、§4.7、§7 的 daemon 端（`attach` 操作與 frame 串流）、§10 的完整 stdio transport，以及 §9 中 lifecycle 相關指令與 `call`。操作：`create_instance`、`start_instance`、`delete_instance`、`restart_instance`、`list_instances`、`interrupt`、`pane_snapshot`、`attach`、`tools_list`。互動式 attach client 留到 T7；T5 的喚醒測試會用到這裡的 daemon 端 attach。

T4 建立真正可用的 `otterhive-mcp-bridge`，不是空 bin 或 fixture 替身：完成 initialize／initialized／ping／tools/list／tools/call、錯誤碼與 daemon 轉接。此工作包的 `tools_list` 只列已完成的 lifecycle 工具：一般 instance 為 `list_instances`、`restart_instance`、`pane_snapshot`，manager 再加 `interrupt`；send／inbox／team 的 handler 與 schema 在 T5 加入。T4 不假稱完成整個 P1a；尚未實作的操作不列入工具列表，直接呼叫回 `unsupported_in_phase`。如此 T4 的兩個 binary 可一起建置、測試、交付，且 daemon 的同目錄 bridge 檢查不依賴未合併的 T5。

**Interfaces：**`otterhive daemon`；`client::call(home: &Path, auth: Auth, op: &str, args: Value, operation_id: Option<String>) -> Result<OperationResult, ClientError>`；daemon 內部依 operation 與 instance 序列化 lifecycle 操作（同一 instance 一次一個，其餘回 `operation_in_progress`）。

**Fixture 與共用：**建立 `fake_claude.sh` 與 `tests/common/mod.rs`（以 `env!("CARGO_BIN_EXE_otterhive")` 在暫存 home 啟動 daemon、測試結束時停止；`call`；原始 frame client；有上限的輪詢）。`tests/mcp_wire.rs` 以 `env!("CARGO_BIN_EXE_otterhive-mcp-bridge")` 啟動真 bridge，兩個 binary 都由 T4 建立、位於同一個 Cargo 輸出目錄。T5–T7 共用它。

**測試：**`tests/instance_lifecycle.rs`、`tests/caller_authority.rs`（lifecycle 部分）、`tests/backend_contract.rs`、`tests/daemon_recovery.rs`、`tests/mcp_wire.rs`（transport 與 lifecycle 工具；T5 擴充同檔）。

- `second_daemon_on_same_home_exits_with_daemon_already_running`：exit code 3，stderr 有一行含 `daemon_already_running` 的 JSON。
- `daemon_refuses_to_start_without_the_bridge_binary`：把 `otterhive` 複製到沒有 bridge 的暫存目錄後執行 `daemon` → 非零結束並說明原因。
- `create_starts_the_instance_and_list_shows_it_running`：結果含 `instance_created`、`generation_advanced`、`process_started`，list 的 generation 與結果一致。
- `backend_defaults_to_claude_and_command_requires_a_program`：省略 backend 時 fake Claude 被啟動；`backend: command` 缺 command 或空字串 → `invalid_argument`，沒有 instance／generation／spawn。
- `concurrent_creates_with_one_name_start_exactly_one_process`：兩個執行緒同時 create 同名 → 一個成功、一個 `name_conflict`；只有一個子程序。
- `unknown_executable_fails_without_a_process`：結果 `refused`（`spawn_failed`），`effects` 含 `generation_advanced`、`state_failed`，不含 `process_started`；create 路徑另有 `instance_created`，狀態 `failed`／`exec_failed`。
- `exit_within_startup_grace_is_failed`：`crash_on_start.sh` → 結果 `refused`（`spawn_failed`，`value.reason: exited_during_startup`），狀態 `failed`，effects 含 generation、process_started／process_stopped、state_failed，不重試。
- `leader_exit_within_grace_cleans_children_before_failure_response`：`spawns_child.sh` 的 leader 在寬限期內結束 → 回應前 child 已不存在；`failed`／`exited_during_startup`，不留下殘留 group。
- `start_checks_a_failed_instances_previous_process_before_spawning`：測試先用 fixture 取得真 pid／pid_start，再把 instance 記為 failed 並保留程序紀錄；start 必先停止舊 group 才增加 generation。pid_start 不符時回 `recovery_required`，generation 不變，原程序未被 signal，沒有第二個程序；測試自行清理。
- `start_responds_after_the_startup_grace`：存活的程序在寬限期結束後才回 `completed`，effects 含 `generation_advanced` 與 `process_started`，狀態 `running`。
- `exit_after_startup_grace_is_stopped_with_exit_status`：程序在寬限期後結束 → `stopped`／`exited`，附 exit status。
- `leader_exit_after_grace_cleans_lingering_group_members`：`spawns_child.sh` 設 `FIXTURE_LEADER_EXIT_AFTER_MS` 大於寬限期 → leader 結束後子程序也被清除，狀態 `stopped`。
- `invalid_spec_is_refused_before_any_change`：不存在的 `instructions`、`skills_path` 或明確給的 `working_directory`、相對路徑、`backend: command` 搭配 `model` → `invalid_argument`，沒有建立 instance、generation 不變。
- `restart_of_a_stopped_or_failed_instance_starts_it`：等同以指定 mode start。
- `invalid_names_are_rejected`：超過 64 字元、含 `/` 或空白、以及名稱 `operator` → `invalid_argument`。
- `operation_id_reused_by_another_caller_is_invalid`。
- `start_on_running_instance_reports_already_running`：generation 不變。
- `fresh_restart_replaces_the_process_and_revokes_the_old_token`：generation 加一、pid 改變、舊 pid 不存在、舊 token 呼叫回 `unauthorized`。
- `restart_with_stale_expected_generation_changes_nothing`。
- `resume_without_evidence_is_refused_and_leaves_the_process`：`command` backend → `resume_unavailable`，pid 不變。
- `second_lifecycle_operation_on_the_same_instance_is_refused`：`ignore_term.sh` 讓 stop 需要數秒，期間再 restart → `operation_in_progress`。
- `delete_stops_archives_and_frees_the_name`：重建同名得到新 id。（刪除 orchestrator 的限制要有 team，測試放在 T5。）
- `operation_id_replay_does_not_spawn_again`。
- `interrupt_writes_escape`：`echo_agent.sh` 的 log 出現 `1b`。
- `pane_snapshot_returns_plain_text_and_honours_lines`；停止後的 instance 回最後畫面並標 `live: false`；daemon 重啟後回空字串與 `reason: no_screen`。
- `interrupt_and_attach_on_a_stopped_instance_are_invalid`。
- `attach_stream_delivers_snapshot_then_output`：以測試內的原始 frame client 連線，先收到 Snapshot，之後 Output 的 seq 連續；generation 不符的 Input 回 `Error`。
- 授權：instance 呼叫 create／delete／start → `unauthorized`；restart 自己 → `queued`，隨後新 generation；restart 別人 → `unauthorized`，但列在 `managers` 的 instance 可以；instance 只能 snapshot 自己；請求帶 `from`、`sender` 欄位 → `invalid_argument`。
- backend：`fake_claude.sh` 收到的 argv 依序含 `--dangerously-skip-permissions`、`--model`、`--effort`、`--append-system-prompt-file <wd>/.claude/otterhive.md`、`--mcp-config <wd>/.claude/otterhive-mcp.json`，`args` 在最後；`otterhive.md` 含 instance 名稱與 instructions 原文；MCP 設定檔權限 0600 並含該 generation 的 token；`.claude/skills` symlink 建立；預先存在的 `.claude/skills` 目錄不被覆蓋且回 warning；子程序環境有 `OTTERHIVE_INSTANCE`、`OTTERHIVE_MCP_BRIDGE`，沒有名稱以 `OTTERHIVE_OPERATOR` 開頭的變數；MCP 設定的 JSON 結構符合契約 §5.1。
- `skills_symlink_ownership_is_verified_before_replacement`：首次建立時 journal 有 `skills_link_created`；同 instance、同 link／target 可更新；無 journal 的既有 symlink（即使 target 相同）、被使用者改過的 target、同名新 instance 都不覆蓋且有 warning。
- MCP transport（`tests/mcp_wire.rs`）：initialize 回要求的 protocolVersion；initialized 無回覆；ping 回 `{}`；未知方法 `-32601`；缺 HOME／TOKEN 任一回 `-32000`；daemon 無法連線回 `-32001`；tools/call lifecycle 經真 daemon 執行，refused／unsupported 設 isError；工具列表只列 T4 已完成的 3 個（manager 4 個），schema 欄位符合 catalog；send／inbox／team 不列入且不能經原始 call 假成功。
- resume 證據：暫存 `HOME` 內放 `.claude/projects/<編碼>/s.jsonl`（含 `"type":"user"`）→ resume 以 `--continue` spawn；沒有該檔 → `resume_unavailable`；另一個 instance 使用同一目錄 → `resume_unavailable`（`shared_working_directory`）。
- `daemon_kill_9_then_restart_stops_orphaned_instances`：`kill -9` daemon 後再起，instance 為 `stopped`／`daemon_restarted`，舊程序已不存在。
- `daemon_restart_with_mismatched_start_time_requires_recovery`：把資料庫中的 `pid_start` 改錯 → `recovery_required`，該程序未收到 signal（測試自行清理）。

**命令：**`cargo test --test instance_lifecycle --test caller_authority --test backend_contract --test daemon_recovery --test mcp_wire`。

### T5：team、訊息、喚醒、MCP 工具擴充

**範圍：**契約 §6、§8.1；擴充 T4 的 §10 registry 與 bridge 測試，使工具列表達到完整 P1a 集合；CLI 的 `send` 與 `team`。team 放在這裡，因為收件權限與 team 廣播都依賴它；不重建 bridge transport。

**測試：**`tests/team_ops.rs`、`tests/message_delivery.rs`、`tests/caller_authority.rs`（訊息部分）、`tests/mcp_wire.rs`

**Fixture：**建立 `mcp_agent.py`。

- team：缺 members／orchestrator、空 members、orchestrator 不在成員中 → `invalid_argument`；成員不存在 → `not_found`；成員已在另一個 team → `name_conflict`；`update` 的 `add`／`remove`／`orchestrator` 生效，remove 原 orchestrator 沒有同次換人 → `invalid_argument`，拒絕不留下部分變更；`list` 欄位齊全；instance 呼叫端只能 `list`；以 `delete_instance` 刪除 orchestrator → `invalid_argument`；不存在的 team → `not_found`，重複名稱 → `name_conflict`。
- `send_drain_ack_round_trip`：drain 得到 token、訊息欄位齊全；ack 後再 ack 回 `already_acked`。
- `second_drain_does_not_settle_the_first_batch`：未 ack 前再 drain 不會把第一批標成 acked。
- `expired_lease_redelivers_with_a_new_token`（`OTTERHIVE_TEST_LEASE_MS`）：`delivery_count` 為 2，舊 token ack → `delivery_token_stale`。
- `ack_without_delivery_and_ack_of_foreign_message_are_refused`：`never_delivered`、`not_found`。
- `fresh_restart_requeues_and_resume_keeps_the_lease`。
- `undelivered_and_unacked_messages_survive_daemon_kill_9`。
- `drain_batch_respects_the_byte_budget`：三則 30 KiB 訊息，第一次 drain 只取到上限內的筆數，`remaining` 正確。
- `partial_broadcast_reports_each_recipient`：`instances: ["a","ghost"]` → `partial`，ghost 為 `not_found`。
- `team_broadcast_excludes_the_sender_and_deduplicates`。
- `oversize_message_and_identifiers_are_invalid`：超過 64 KiB 的 `message`、超過 256 字元的 `correlation_id` → `invalid_argument`，沒有寫入任何訊息。
- ACL：跨 team 拒絕；`accept_from` 送給 orchestrator 允許；無 team 的收件人允許；manager 可送任何人；sender 由 token 決定。
- 喚醒：`echo_agent.sh` 收到 `[OTTERHIVE-MSG] from=operator kind=query id=<id>`；有 attach client（T4 的原始 frame client）剛輸入時為 `deferred`，安靜後送出；兩則延後的提示合併為 `count=2`；`echo_agent.sh` 在 `flood` 期間提示保持 `deferred`，`stop` 後送出；instance 在提示送出前 restart，提示被放棄、訊息仍可 drain。測試以 `OTTERHIVE_TEST_WAKE_INPUT_MS` 與 `OTTERHIVE_TEST_WAKE_QUIET_MS` 縮短等待。
- MCP：`initialize` 回傳要求的 `protocolVersion`；一般 instance 的 `tools/list` 恰為 6 個工具，orchestrator 多 `interrupt`；每個工具 schema 的欄位等於 catalog 的 `supported_fields ∪ added_fields`；refused 結果 `isError: true`；未知方法 `-32601`；缺 `OTTERHIVE_INSTANCE_TOKEN` 時 `initialize` 回 `-32000`；daemon 停止時 `tools/call` 回 `-32001`。
- `two_agents_exchange_query_and_report_through_the_bridge`：兩個 `mcp_agent.py` instance；operator 送 `relay:b:ping` 給 a → a 送 query 給 b → b 回 report `pong:ping` 給 a；兩邊 log 都有對方訊息，所有訊息最終為 acked。

**命令：**`cargo test --test team_ops --test message_delivery --test caller_authority --test mcp_wire`。

### T6：deployment

**範圍：**契約 §8.2–§8.4；CLI 的 `deploy`、`teardown`、`deployments`。

**測試：**`tests/team_deployment.rs`

- deployment 擁有的 team 不能直接以 `team delete` 刪除。
- `deploy_two_members_starts_both_and_creates_the_team`：成員名 `<name>-<member>`、orchestrator 正確、`ignored_template_fields` 含 `source_repo`；每個成員的 working directory 不同。
- `deploy_with_a_failing_member_is_partial`：其他成員保持運行。
- `deploy_with_all_members_failing_is_refused_and_retains_definitions`：全部啟動失敗 → refused／spawn_failed，逐成員 failed 與 effects 齊全，deployment／team／instance 定義保留供重試；不回 completed 或 partial。
- `invalid_template_members_or_orchestrator_are_rejected_before_mutation`：空 instances、無 orchestrator、orchestrator 不在 member keys → `invalid_argument`，沒有目錄、定義或程序變更。
- `redeploy_starts_only_members_that_are_not_running`：運行中成員的 pid 與 generation 不變。
- `deploy_into_a_name_owned_by_someone_else_has_no_side_effects`：`name_conflict`，沒有新 instance 或 team。
- `deploy_with_too_long_member_names_is_invalid`：組出的名稱超過 64 字元 → `invalid_argument`，沒有副作用。
- `deploy_with_unknown_template_or_broken_config`：不存在的 template → `not_found`；設定檔壞掉 → `invalid_argument`，daemon 仍用原設定。
- `deploy_with_branch_is_unsupported`。
- `teardown_removes_members_team_and_record`：包括 orchestrator 在內的成員都被刪除；沒有殘留程序。
- `teardown_with_an_unprovable_stop_deletes_nothing`：以錯誤的 `pid_start` 模擬 → `partial`，`residuals` 列出該成員；其他成員為 `stopped` 但未刪除，team 與記錄保留；修正後再 teardown 成功。
- `directory_argument_gives_each_member_its_own_subdirectory`：既有 directory 下不存在的 member 子目錄由 deploy 建立；已有目錄沿用；teardown 後目錄與使用者檔案保留。
- `deploy_directory_validation_precedes_definition_and_spawn`：directory 不存在／非目錄、member 明確給的 cwd 不存在、衍生子路徑已是檔案 → `invalid_argument`，不建立定義、不 spawn。
- `deploy_directory_creation_failure_reports_effects_without_spawning`：建立衍生目錄遇到 I/O 失敗 → `internal`，已建立目錄列入 effects，沒有 instance／team 定義與程序。
- `explicit_shared_working_directories_are_not_claimed_to_be_unique`：template 明確指定共用 cwd 可 deploy，但 resume 回 `shared_working_directory`。

**命令：**`cargo test --test team_deployment`。

### T7：attach client 與端到端

**範圍：**§9 的互動式 `otterhive attach`（daemon 端已在 T4 完成）、重新連線行為的完整測試、`scripts/smoke-p1.sh`。

**測試：**`tests/attach_reconnect.rs`、`tests/p1_e2e.rs`

- `attach_starts_with_a_snapshot_then_contiguous_output`。
- `input_and_resize_frames_reach_the_backend`。
- `detach_leaves_the_instance_running_and_reattach_shows_the_current_screen`。
- `restart_ends_old_connections_and_rejects_stale_input`：舊連線收到 `Ended`；舊 generation 的 Input → `Error`（`stale_generation`）。
- `stalled_client_does_not_block_the_daemon`：不讀取的 attach client 存在時，`list_instances` 與 `pane_snapshot` 仍在 1 秒內回應；client 恢復讀取後收到 `Resync` 與 `Snapshot`。
- `p1_end_to_end`：以 fixture template deploy 兩個 `mcp_agent.py` 成員 → relay 交換成功 → fresh restart 其中一個 → 之後送給它的訊息由新程序 drain → teardown → log 中記錄的所有 fixture pid 都已不存在。
- `scripts/smoke-p1.sh`：用 CLI 在暫存 home 跑同一情境；先設 cleanup trap 再啟動 daemon；結束時列出殘留程序並以非零 exit code 回報。完成後以 REA `capture_process_scenario` 執行一次，把 Evidence ID 加進 [證據索引](../reverse-engineering/evidence-index.md)。

**命令：**`cargo test --test attach_reconnect --test p1_e2e`；`scripts/smoke-p1.sh`。

### T8：P1 相容 instructions 與 native canary（需要 operator 授權才執行）

- `examples/instructions/p1-member.md`：不引用 task board、review、CI 的角色中性指引；說明 `[OTTERHIVE-MSG]`、drain、ack、以 `send` 回覆。agend-customization 的原 L1 不修改。
- `docs/runbooks/native-claude-canary.md`：在暫存 home 起兩個真 Claude instance，驗證 H4–H7、self restart、resume、skills 載入；列出預估成本、停止條件與要記錄的證據。
- 只有在 operator 明示授權與預算後才執行 canary；結果寫進證據索引。未執行前，所有真 backend 行為標為未驗證。

## PR 邊界

| PR | 內容 | 驗收 | 建議 review class |
|---|---|---|---|
| P0 | 本 repo 目前的 `docs/`、`scripts/research/`、fixture | 文件互相一致、連結有效 | single |
| T2 | protocol、admission、home、config、store | T2 測試 | single |
| T3 | procs、terminal session、frame | T3 測試 | dual（程序停止與並行） |
| T4 | daemon、授權、lifecycle、backend、daemon 端 attach、bridge transport、CLI 基礎 | T4 測試（包含真 bridge） | dual（授權與程序生命週期） |
| T5 | team、訊息、喚醒、MCP 工具擴充 | T5 測試 | dual（身分、投遞保證） |
| T6 | deployment | T6 測試 | single |
| T7 | attach client、E2E、smoke | T7 測試與 REA 證據 | single |
| T8 | 相容 instructions、canary runbook | 文件審閱；canary 另行授權 | single |

每個 PR 從 `main` 開分支，合併後下一個再從新的 `main` 開始；PR 不疊在未合併的 PR 上。每個 PR 結束時更新 [架構](../architecture.md#實作狀態) 的狀態表，並在 PR 說明寫 `docs-check:` 一行。

## 文件影響

- T2–T7：若實作發現契約需要改，先改 [契約](../specs/p1-coordination.md) 與必要的 catalog 產生器，再改程式；不留兩份不一致的說法。
- T7：證據索引加入 smoke 的 REA 證據。
- T8：新增 runbook 與範例 instructions。

## 需要 operator 決定的事

1. **推送 `main`**：T2 已合併，T3 開始時 `origin/main` 與本地 `main`（`b1a42b4`）同步，不再是開工阻塞。各 PR 從最新 `main` 開分支推進。
2. **由誰實作**：本 session 不實作；可交給既有 team 的 impl／reviewer，或日後由 OtterHive 自己的 team 接手。
3. **review 安排**：上表的 review class 是建議；dual 需要兩位不同的 reviewer。
4. **native canary**：是否授權、預算上限。
5. **operator 收件匣**：P1a 的 agent 無法送訊給 operator；是否在 P1b 加入不經 channel 的收件匣。

## 自我檢查

- 契約每一節都有對應的工作包與測試：§1–§3（T2、T4）、§4（T3、T4）、§5（T4）、§6（T5）、§7（frame 在 T3、daemon 端在 T4、client 在 T7）、§8.1（T5）、§8.2–§8.4（T6）、§9（T4–T7）、§10（T4 transport／lifecycle，T5 完整工具集合）。
- 依賴順序：T4 自己建立兩個 binary，啟動 daemon 不依賴 T5；T5 的喚醒測試用到 T4 的 daemon 端 attach 與 bridge transport，收件權限與「不能刪 orchestrator」用到 T5 自己的 team 操作；T6 用到 T4 的 lifecycle 與 T5 的 team；T7 用到 T4–T6。
- 每個 fixture 與 `tests/common/mod.rs` 都由第一個用到它的工作包建立。
- 契約中可觀察的測試輸入（佇列大小、喚醒視窗、寬限期、lease）都有對應的測試環境變數。
- 沒有未定義的介面：後續工作包使用的 `Store`、`Session`、`client::call`、`stop_group` 都在前面的工作包定義。
- 待驗證假設都有負責的工作包；真 backend 相關的只在 T8 驗證，不在 fake 測試中宣稱。
