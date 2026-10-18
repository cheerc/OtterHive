# PTY 寫入阻塞：診斷與修法（已實作，待 PR）

**狀態**：**已修正**（2026-10-17，分支 `worktree-p1a-pty-write-fix`，RED `80bd7c4` → GREEN `72d3059`）。本檔原為「T6 開工前的工作包」，記載了診斷過程中**多數錯誤**的結論；那些結論全部保留在下方，因為它們的錯法比結論本身有用。

## 現象與真正的機制

daemon 的 PTY 寫入會阻塞，三條呼叫路徑都走 `Session::write_input`：喚醒提示、`interrupt`、attach 的 Input frame。

**真正的機制（三人獨立量測後定案）**：

- `write(2)` 在 **master→slave 的輸入佇列**滿時阻塞。該佇列**只由子程序讀 stdin 才會排空**——master 端的 reader 碰不到它。所以不讀 stdin 的 agent（`non_reader.sh` = `exec sleep 600`）**一定**踩到且不會自己好；`cat` 型子程序 25 秒寫完 118.8 MB 毫無問題。
- 門檻是 **1022 bytes**（單行輸入佇列上限），**不是累積量**。
- **只有含行結束字元的寫入才會 EAGAIN／阻塞。** 喚醒提示行以 `SUBMIT_KEY = b"\r"` 結尾（`wake.rs:43`），正是會踩到的那種寫入。

## 已推翻的結論（保留，因為錯法有用）

這個判別點翻轉了**四次**，四個人各自都產出過有結構缺陷的量測：

| 主張 | 誰 | 錯在哪 |
|---|---|---|
| 「PTY 接受 64 MiB 無阻塞，不可達」 | coordinator | 量測在 Python + `pty.openpty` 的**模型**裡，不是 daemon |
| 「單次 1 MiB 最慢 42 ms、400 MiB 內無阻塞」 | otterhive-0a | 同上：資料不含行結束字元 |
| 「約 200 MiB 是門檻」 | worklog 原文 | 量測被**總時限截斷**（「寫到 8 秒為止」），不是寫到阻塞 |
| 「`O_NONBLOCK` 對 PTY 寫入無效」 | otterhive-0a | 用「總時間超過 N 秒」判定，把「還在寫」誤判成「卡住」 |
| 「dup 一個 fd 再把 `O_NONBLOCK` 設在它上面就能只影響 writer」 | 曾被寫成記憶 | dup **不產生新的 open file description**，旗標必然共用 → reader 把 EAGAIN 當 EOF 自殺 |
| 「daemon 持有 slave fd 會讓 master 永遠看不到 EOF」 | otterhive-69 | 沒有控制「child 是否為 session leader」這個變因 |
| 「只有不含 `\r` 的寫入永不 EAGAIN，所以要靠 `\r`」到「`\r` 只是必要條件」 | 多人 | 反覆在「條件」與「必要條件」之間混用 |

**共同點**：用時間、模型或假設代替「**真的到達那個狀態**」。定案靠的不是任何單一推論，是三個獨立向量測到同一組數字（1022 B、EAGAIN 前有 partial write、blocking 卡 >120 s）。

## 已裁決並實作的修法

| # | 修正 | 依據 |
|---|---|---|
| 1 | `write_input` 改用 `try_lock`，取不到回 `WouldBlock` | §6.4／§4.7 的可用性 |
| 2 | `interrupt` 與 attach Input 的寫入**移出 daemon 鎖** | `state.lock()` 全域唯一，鎖內寫入會讓全部操作停擺 |
| 3 | `interrupt` 的錯誤碼 `internal` → `operation_in_progress` | §4.7 加一句；writer busy 是預期的併發狀態 |
| 4 | attach Input 維持靜默丟棄，但累計 `INPUT_WRITES_REJECTED` | §7.2 沒有 Input 的錯誤通道；靜默要可觀察 |

poison 用 `unwrap_or_else(|p| p.into_inner())`（與 `Drop`、`mark_ended`、`DaemonState::lock` 一致）。

### 為什麼不做 `O_NONBLOCK`（原本裁決的路線）

兩個獨立的致命傷，都經實測：

1. **位址不可得。** dup 不隔離旗標（`O_NONBLOCK` 屬 open file description），會讓 reader 自殺；`open(tty_name)` 雖能隔離，但**寫 slave fd 的資料送不到 child stdin**，只回灌成 master 的輸出（污染 §7.1 的 `seq`）。送 stdin **只有寫 master 一途**。
2. **`write_all` 會切斷提示行。** 非阻塞下單次 `write` 回短寫（三人各自量到 2/60、46/61、2/12），而 `write_all` 在阻塞模式下靠「反正最終會寫完」維持整行原子。改用非阻塞是把 hang 換成靜默的資料損壞。

## 測試

| 測試 | 位置 | 舊版上的表現 |
|---|---|---|
| `write_input_returns_would_block_without_waiting_when_writer_is_held` | `src/terminal/session.rs` 單元 | **hang**（不是紅） |
| `write_input_succeeds_when_writer_is_free` | 同上 | 綠 |
| `spec_env_cannot_smuggle_operator_prefixed_variables` | `src/backend/claude.rs` 單元 | 紅 |
| `spawn_removes_operator_prefixed_keys_from_config_env` | `src/terminal/session.rs` 單元 | 紅 |
| `list_instances_reports_the_contract_field_set_and_real_team_membership` | `tests/backend_contract.rs` | 紅 |
| `coordination_prompt_reports_team_and_privileged_tools` | `tests/backend_contract.rs` | 紅 |

`write_input` 那兩支是**確定性**的：直接佔住 `Core::writer`，不填 1022 B 佇列。壞版本上會 hang（`lock()` 沒有上限、`cargo test` 沒有 per-test timeout），所以判準是「有界時間內回 `Err`」——**它證明的東西與「紅」相同，但描述必須精確**。

## 順帶修掉的契約層缺陷

同一批診斷揭露了四條「契約要求、實作少做、測試零覆蓋」的缺陷。共同特徵是**綠燈與缺陷同時成立**（沒有任何測試讀過那個欄位／那條行為）：

- **W4**：`list_instances` 的 `team` 恆為 `null`——讀 `spec.get("team")`，而 §4.1 的 `InstanceSpec` 欄位清單不含 `team`。改讀 `team_members`。
- **N1**：`spec.env` 與 `SessionConfig::env` 可放 `OTTERHIVE_OPERATOR*` 繞過 §5.3——過濾只做了繼承環境。
- **N3**：協調提示缺 team／orchestrator，且工具清單漏 `interrupt`（§5.1）。

## 仍未修（需要 REA，留給 T7）

- `resume_evidence` 傳 `OTTERHIVE_HOME`，而 §5.4 明文是 `$HOME/.claude/projects/`。測試把兩者設成同值（`tests/backend_contract.rs:112-113`）所以結構上測不出。
- `encode_project_dir` 只檢查 ASCII alphanumeric，契約寫「非 `[A-Za-z0-9-]`」——對非 ASCII 路徑（CJK 的 `working_directory`）兩者不同。

**兩者都不改**：§5.4 的編碼規則來自舊 adapter 的靜態移植，而 `docs/reverse-engineering/evidence-index.md` 自己標明「**不證明真 Claude 的相容性**」。改錯會讓 OtterHive 找不到原生 session，比現況更糟。

## 相關

- `tests/fixtures/backends/non_reader.sh` 是本缺陷的 fixture。
- T5 PR #4 的 Lessons 記錄了 D-1 的診斷過程。
