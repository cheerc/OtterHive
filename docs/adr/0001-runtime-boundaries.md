# ADR 0001：執行期邊界與首版選型

狀態：採用（2026-10-12）

## 背景

`agend-terminal` 把 instance 管理、訊息、task／review／CI、Telegram、TUI 與 git 防護都放在同一個 daemon，`src/` 頂層超過一百個檔案。operator 需要的是其中一部分：起 team、讓 agent 互相溝通、restart、終端操作，之後再接 task 與 decision。OtterHive 從這些需求重新建構，不追求與舊版逐欄位相容。

## 決策

1. **daemon 只負責協調，不碰模型迴圈。** backend CLI（Claude Code 等）自行與模型互動；daemon 管程序、身分、訊息、終端與 team。不引入任何 LLM SDK。
2. **daemon 與終端 client 分開。** daemon 是 PTY、程序與狀態的唯一擁有者；`otterhive attach` 與日後的 TUI 只是 client，關閉不影響 agent。舊版的 app 模式把 daemon 與 TUI 放在同一程序，不採用。
3. **自有 PTY 與 VT，不依賴 tmux。** 依舊 source（靜態推論，見[證據索引](../reverse-engineering/evidence-index.md#靜態-source-推論不是行為證據)），舊版並未呼叫 tmux，而是 portable-pty 加 alacritty_terminal，tmux 只是操作風格。OtterHive 沿用同一組套件；reattach 與 `pane_snapshot` 需要 VT 畫面狀態，raw log 無法提供。
4. **單一 SQLite 作為狀態權威。** instance、generation、訊息 lease、deployment journal 需要跨列原子更新。以 WAL 與 `synchronous=FULL` 取代舊版的「每檔加鎖 append 再 fsync」。代價是 schema migration 與一個新依賴。
5. **身分由 daemon 發放。** sender 由憑證推導，不信任呼叫端參數；每個 generation 換一組 token，舊程序的 bridge 自動失效。`role` 只是描述。
6. **明確 ack 取代隱式 ack。** 依舊 source（靜態推論），舊版再次 drain 會結算上一批；OtterHive 要求帶 `delivery_token` 的 ack，換取「未處理的訊息一定會重送」。這是刻意的契約變更。
7. **停止必須有證據才能取代。** restart、delete、teardown 都先確認舊 process group 已結束；無法確認就回報 `recovery_required`，不起第二個程序、不對身分不明的 pid 發 signal。
8. **不支援的能力明確拒絕。** 未實作的工具不出現在 `tools/list`；舊欄位與 `request_kind: task` 在任何變更前回 `unsupported_in_phase`，不靜默忽略。
9. **create 與 deploy 只給 operator。** customization 的 L1 本來就禁止 agent 建立 instance；OtterHive 把它變成 daemon 規則。權限另外只來自設定檔的 `managers` 與 team 的 `orchestrator`。
10. **同步 I/O 加執行緒，不用 async runtime。** PTY 讀取與等待子程序本來就是阻塞呼叫；連線數量很少（CLI 與每個 instance 一個 bridge）。以執行緒處理可少一組依賴，鎖的順序也比較容易推理。最初的計畫列了 Tokio，以本決策為準。
11. **單一 crate、清楚的模組。** P1a 不拆成多個 crate；模組邊界見 [架構](../architecture.md)。需要獨立發布或編譯時間成為問題時再拆。

## 不做的事

hostile-seat 等級的 OS 隔離（agent 與 operator 是同一個 OS 使用者，主動讀取 operator 憑證的惡意 agent 不在防範範圍）、透明的 daemon 熱升級、自動 crashloop 重啟、Telegram／Discord、通用 workflow DSL、在 P1a 接 task／review／CI。

## 後果

- 首版完成後，customization 的 lead／impl／review 流程仍要等 P2（task）與 P3（review、CI）才能完整運作。
- 真 backend 的 resume、喚醒與 skills 載入，要在 operator 授權的 canary 驗證後才算支援；fake backend 的測試只證明 daemon 這一側的契約。
- 平台先以 macOS 驗證；Linux 與 Windows 未驗證。
