# T6 開工前的工作包：PTY 寫入會阻塞（§6.4／§7）

**狀態**：已知缺陷、有證據、有裁決，**刻意不併入 T5**。記錄於 2026-10-15，T5 PR #4 合併時一併進 dispatch 佇列。

## 現象

daemon 的 PTY 寫入會阻塞，而三條呼叫路徑都走它：

- `src/wake.rs` 的喚醒提示（`src/daemon/mod.rs:2126`）
- `interrupt`（`src/daemon/mod.rs:2578`）
- attach 的 Input frame（`src/daemon/mod.rs:2940`）

三者共用 `Session::write_input`（`src/terminal/session.rs:283`），而它持 `core.writer`（`Mutex<Box<dyn Write + Send>>`）呼叫 `write_all`。

## 證據

### 1. CI 兩次因此紅（`p1_message_e2e` 的 `a_non_reading_agent_does_not_stop_the_daemon_from_serving_other_callers`）

```
應可讀回一行回應：Resource temporarily unavailable (os error 35)
```

`EAGAIN` 在設有 `SO_RCVTIMEO` 的 socket 上就是 read timeout（harness 的 `RECV_TIMEOUT` 為 20 秒）——所以是真的讀不到回應，不是連線層的 flaky。

### 2. 執行緒堆疊（`otterhive-0a` 以 `sample` 在卡住的當下取得）

```
spawn_wake_recheck
  write_hint                            <-- otterhive
    Session::write_input
      portable_pty::UnixMasterWriter::write_all
        write  (libsystem_kernel)        ← 阻塞於 write(2)
```

同一份 sample 中，處理該次 `send` 的執行緒也在等同一把鎖。

### 3. 重現門檻

`200` 次 `send` 在本機 12 輪全綠；`2000` 次本機 3 輪全紅。CI 是 3 核 runner。

## 已被推翻的兩個錯誤結論

這個缺陷的判斷過程有兩次「量測結論錯了」，記下來避免重蹈：

- **我（coordinator）**量測「PTY 在讀端不動時接受 64 MiB 無阻塞」，據此判定「不可達」。量測在 Python + `pty.openpty` 的模型裡，**不是 daemon 本身**。
- **`otterhive-0a`**量測「單次 1 MiB 寫入最慢 42 ms、400 MiB 內無阻塞」，同樣據此判定不可達。
- 兩人都量到「寫很多都沒阻塞」，但 0a 補的控制組顯示累積到約 200 MiB 就會 STOPPED。**提示行長度雖小（§6.4 的 `count` + 36 字元 UUID），但會隨 `send` 次數無界累積。**

共同的教訓：**在模型裡量到「沒阻塞」不等於在產品裡不會阻塞。** 執行緒堆疊是直接觀察 daemon，可信度不同。

## 已裁決的修法：writer 改 `O_NONBLOCK`

寫入 PTY 時若回 `EAGAIN`，回報 §6.4 已有的 `failed`：

- 語意乾淨：「提示沒寫進去，但**訊息仍在 inbox**」——正是 §6.4 明文。
- 不引入新的無界結構（相較於「單一 writer 執行緒 + 有界佇列」，後者需要在「延後沒有上限」之下再引入一個上限）。
- 呼叫端不被阻塞。

### 技術障礙（動工前必讀）

`Session::spawn`（`src/terminal/session.rs:181`）呼叫 `pair.master.take_writer()`，而 portable-pty 0.9 的 `take_writer`（`unix.rs:357`）回傳 `Box<dyn Write + Send>`，**內部 fd 私有、無法事後 `fcntl(F_SETFL, O_NONBLOCK)`**。`UnixMasterPty` 的公開方法只有 `resize`／`get_size`／`try_clone_reader`／`take_writer`。

因此有兩條路，**都動到 T3 的核心不變量**：

1. **自己呼叫 `libc::openpty`**：放棄 portable-pty 的 PTY 建立。要重做 T3 的命令組裝、環境過濾、session leader 設定與 `OwnedLeaderLease` 建立——特別是 H8（`waitid(WEXITED|WNOHANG|WNOWAIT)` 與 pgid 不可提前 reap），那是 T3 花最多輪次才驗證的不變量。
2. **換 writer 的來源**（例如改用 `try_clone_reader` 之外的路徑取得 fd）——需要確認 portable-pty 是否提供任何間接途徑；目前看沒有。

**動工前必須先選定路線，並把選擇理由寫進 commit。**

## 範圍

- **動**：`src/terminal/session.rs` 的 writer 取得與 `write_input`；受影響的三個呼叫點（喚醒／interrupt／attach Input）確認 `EAGAIN` 的處理一致。
- **不動**：訊息投遞語意（§6.1／§6.3）、喚醒的延後條件（§6.4）、attach 的 frame 格式（§7.2）。
- **測試**：需要一條**確定性**觸發的測試（不讀 stdin 的收件人 ＋ 足以填滿 PTY 輸入緩衝區的 `send` 次數），以及 `write_input` 回 `EAGAIN` 時三個呼叫點各自的行為。

## 驗收

- 不讀 stdin 的 agent 不會讓 `list_instances`／`pane_snapshot`／attach 對其他呼叫端失效（在全量負載下）。
- `send` 在寫入被拒時回 `wake: "failed"`，且訊息仍可 drain（§6.4）。
- `interrupt` 與 attach Input 在 writer 忙碌時的行為明確（契約未規定，需在本工作包內定義並寫進 PR）。

## 相關

- T5 PR #4 的 Lessons 一至三記錄了 D-1 的診斷過程與被推翻的結論。
- `tests/fixtures/backends/non_reader.sh` 是本缺陷的 fixture（T5 建立）。