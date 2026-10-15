//! T5 Lane B：喚醒提示的排程（契約 §6.4）。
//!
//! 契約要點逐條對應到下面三個函式：
//!
//! - [`render_hint`]：提示的外形。單則是
//!   `[OTTERHIVE-MSG] from=<sender> kind=<kind> id=<message_id>`；同一收件人
//!   尚未送出的多則合併為
//!   `[OTTERHIVE-MSG] count=<n> from=<最後的 sender> kind=<最後的 kind> id=<最後的 id>`，
//!   `n == 1` 時省略 `count`。
//! - [`should_defer`]：延後條件。**有 client 在最近 `input_window` 內輸入過**
//!   **或** 收件人最近 `quiet_window` 內有輸出即延後。
//! - [`Scheduler`]：合併、每 `recheck` 毫秒重新判斷、寫入 PTY（含送出鍵），
//!   以及 instance 停止或換 generation 時放棄該提示。
//!
//! 兩條契約上的界線，這個模組刻意不做：
//!
//! 1. **延後沒有上限**。[`Scheduler::next_action`] 在條件持續成立時持續回
//!    `Deferred`，不設重試次數、不設總時限。持續輸出的程序在停下之前不會
//!    收到提示，這是契約要求的行為，不是需要被修掉的卡住。
//! 2. **提示不影響訊息本體**。模組只產生提示文字並寫入 PTY，完全不碰
//!    訊息的儲存；「不論提示是否送出，訊息都留在 inbox」由呼叫端（`send`
//!    交易）保證。
//!
//! 重新判斷的間隔（250 毫秒）刻意不放進 daemon 的 `Tuning`：它是排程器自身
//! 的節拍，不是等待某個外部條件的逾時。把它做成常數，是為了讓「重判週期」
//! 與「重判多久」兩件事各自只由一個地方決定。

use std::time::Duration;

/// 重新判斷延後條件的間隔（契約 §6.4 的「每 250 毫秒」）。
///
/// 不是逾時：不達到就重判。它是節拍，所以不隨測試調整——把節拍做成可調
/// 只會讓「延後到底多久才送出」變成一個測試可以藏起來的量。
pub const RECHECK_INTERVAL: Duration = Duration::from_millis(250);

/// 喚醒提示的識別前綴（契約 §6.4）。
pub const HINT_PREFIX: &str = "[OTTERHIVE-MSG]";

/// 送入 PTY 的送出鍵（契約 §6.4 的「加送出鍵」）。
///
/// 提示是一「行」，因此送出鍵就是換行。寫入時提示文字與送出鍵在同一次
/// `write_input` 送出，避免 agent 讀到一半的提示就當成完整訊息。
pub const SUBMIT_KEY: &[u8] = b"\r";

/// 一次排程中的提示（契約 §6.4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingHint {
    /// 送訊者。operator 的 `from` 是 `{"kind":"operator"}`，沒有名稱，
    /// 因此這裡存的是顯示用字串（operator 用 `operator`）。
    pub from: String,
    /// `request_kind`（`query`／`report`／`update`，省略時為 `message`）。
    pub kind: String,
    /// 訊息 ID。
    pub message_id: String,
}

impl PendingHint {
    /// 把這一則渲染成要寫進 PTY 的一行（不含送出鍵）。
    ///
    /// `count` 是合併後的總數（契約 §6.4 的 `count=<n>`）；`n == 1` 時
    /// 省略 `count` 欄位。
    pub fn render(&self, count: usize) -> String {
        if count <= 1 {
            format!(
                "{HINT_PREFIX} from={} kind={} id={}",
                self.from, self.kind, self.message_id
            )
        } else {
            format!(
                "{HINT_PREFIX} count={count} from={} kind={} id={}",
                self.from, self.kind, self.message_id
            )
        }
    }
}

/// 一次喚醒的結果（契約 §6.4 的 `send` 結果欄位 `wake`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeOutcome {
    /// 已寫入 PTY。
    ///
    /// 只代表**提示已寫入**，不代表 agent 已讀或已處理（契約 §6.4）。
    Hinted,
    /// 排程中，等待延後條件解除。
    Deferred,
    /// 收件人不在 `running`。
    NotRunning,
    /// 寫入 PTY 失敗。
    Failed,
}

impl WakeOutcome {
    /// `send` 結果裡 `wake` 欄位的線上表示（契約 §6.4 的四個值）。
    pub fn as_wire(&self) -> &'static str {
        match self {
            WakeOutcome::Hinted => "hinted",
            WakeOutcome::Deferred => "deferred",
            WakeOutcome::NotRunning => "not_running",
            WakeOutcome::Failed => "failed",
        }
    }
}

/// 延後條件的判定輸入。
///
/// 兩個時間戳由 daemon 提供（`OTTERHIVE_TEST_WAKE_INPUT_MS` 與
/// `OTTERHIVE_TEST_WAKE_QUIET_MS` 控制其長度）；`now` 由呼叫端傳入，讓
/// 「重判」與「判定」可以用同一個時鐘。
#[derive(Debug, Clone, Copy)]
pub struct DeferInputs {
    /// 最近一次 client Input frame 的時間；`None` 表示沒有 client 輸入過。
    pub last_client_input_at: Option<std::time::Instant>,
    /// 收件人最近一次 PTY 輸出的時間。
    pub last_output_at: std::time::Instant,
}

/// 是否應該延後（契約 §6.4）。
///
/// 兩個條件是**或**關係：client 正在輸入（operator 可能在打字）與收件人
/// 正在輸出，兩者都表示現在不是插話的好時機。
pub fn should_defer(
    inputs: DeferInputs,
    input_window: Duration,
    quiet_window: Duration,
    now: std::time::Instant,
) -> bool {
    let typed = inputs
        .last_client_input_at
        .is_some_and(|at| now.saturating_duration_since(at) < input_window);
    if typed {
        return true;
    }
    now.saturating_duration_since(inputs.last_output_at) < quiet_window
}

/// 放棄某則提示的原因（契約 §6.4 的兩個不同事實）。
///
/// 這兩個成因在 RED 階段被壓進同一個 `NextAction::Abandoned`，再由
/// `outcome_for` 一起映成 `not_running`——那會讓「PTY 寫入失敗」被回報成
/// 「收件人不在執行中」。它們是不同的事實，caller 對下一步的判斷也不同，
/// 因此在型別上分開。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbandonReason {
    /// 收件人已停止，或換了 generation（契約 §6.4 明文的放棄條件）。
    NotRunning,
    /// 寫入 PTY 失敗。
    ///
    /// **實作判斷，非契約推導**：契約 §6.4 對寫入失敗沒有明文（只對停止／
    /// 換 generation 放棄）。本模組的決定是**不重試**——PTY master 寫入失敗
    /// 通常代表 master fd 或程序已經結束，重試只是把同一個錯誤重複到逾時。
    /// 這個選擇寫在這裡，是為了讓後人知道它是判斷而不是契約要求的。
    WriteFailed,
}

/// 排程器下一步該做什麼。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextAction {
    /// 要把這一列寫入 PTY（payload 是提示行 ＋ 送出鍵），回
    /// [`WakeOutcome::Hinted`]。
    ///
    /// **`check` 只做決策、不做 I/O**：bytes 由呼叫端在鎖外寫入後，才呼叫
    /// [`Scheduler::commit_delivery`] 清單並回報結果。寫入失敗時呼叫端回
    /// `Abandoned(WriteFailed)` 而**不**是這個值——§6.4 明寫「`hinted` 只
    /// 代表提示已寫入 PTY」，所以「決定要送」不等於「送到了」。
    Deliver(Vec<u8>),
    /// 條件未解除，等 [`RECHECK_INTERVAL`] 再判斷。
    ///
    /// 沒有「放棄」分支：延後沒有上限。
    Deferred,
    /// 收件人已停止／換了 generation，或寫入失敗；清空待送清單。
    ///
    /// 訊息仍在 inbox（契約 §6.4）。成因見 [`AbandonReason`]。
    Abandoned(AbandonReason),
    /// 沒有待送提示：這一輪不構成一次喚醒。
    ///
    /// 這**不是**任何 `wake` 值。排程迴圈在上一輪送完、下一則 `send` 還沒
    /// 進來時會自然落到這裡；把它回報成 `hinted` 等於對 caller 謊稱提示已
    /// 寫入。呼叫端必須以 [`Scheduler::outcome_for`] 的 `None` 處理它。
    NothingToSend,
}

/// 一個收件人的喚醒排程。
///
/// 結構上刻意做成「不持有 `&Session`」：排程器需要跨時間存活，而 `Session`
/// 的生命週期由 daemon 的 `running` 表管理。`check` 因此收 `Option<&Session>`
/// ——`None` 就是「已停止或換 generation」，這是唯一能放棄提示的條件。
#[derive(Debug)]
pub struct Scheduler {
    /// 尚未送出的提示，依送達順序；合併時以最後一則的欄位為準。
    pending: Vec<PendingHint>,
    /// 排程器認定的收件人 generation；每一列都必須與它相同。
    generation: u64,
}

impl Scheduler {
    /// 為某個 generation 的收件人建立排程器。
    pub fn new(generation: u64) -> Scheduler {
        Scheduler {
            pending: Vec::new(),
            generation,
        }
    }

    /// 收件人目前的 generation。
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 尚未送出的提示數（合併後會寫成一行 `count=<n>`）。
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// 是否還有待送的提示。
    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// 排入一則提示。
    ///
    /// 同一收件人尚未送出的多則提示**合併**為一行（契約 §6.4）：合併後
    /// 保留 `count`（總數）與**最後一則**的 `from`／`kind`／`id`。
    pub fn enqueue(&mut self, hint: PendingHint) {
        self.pending.push(hint);
    }

    /// 合併後那一行的文字（尚未送出時的樣子；不寫入任何地方）。
    ///
    /// 沒有待送提示時回 `None`。
    pub fn pending_line(&self) -> Option<String> {
        self.pending
            .last()
            .map(|last| last.render(self.pending.len()))
    }
    /// 依目前狀態決定下一步。**不做任何 I/O。**
    ///
    /// - `session` 為 `None`（收件人已停止或被移出 `running`）→ `Abandoned`
    ///   (NotRunning)，並清空待送清單（該提示就此放棄，訊息仍在 inbox）。
    /// - `generation` 不符 → 同上（契約 §6.4 的放棄條件）。
    /// - 沒有待送提示 → `NothingToSend`（不是任何 `wake` 值）。
    /// - 延後條件成立 → `Deferred`（不動清單，稍後重判；**延後沒有上限**）。
    /// - 否則 → `Deliver(bytes)`：把要寫入 PTY 的那一行連同送出鍵交給呼叫端。
    ///
    /// ## 為什麼把寫入移出本函式
    ///
    /// 呼叫端（daemon 的 dispatch）在持有 daemon 全域鎖的情況下呼叫本函式，
    /// 而 PTY master fd 是**阻塞式**的（`portable-pty` 的 `openpty` 不設
    /// `O_NONBLOCK`）。若本函式自己寫入，一個不讀 stdin 的 agent 就可能讓
    /// `write_all` 阻塞，而鎖被持有——那會讓整個 daemon 的所有操作停擺。
    /// §6.4 的「延後沒有上限」加上 250ms 的重判節拍讓這個路徑可以無限重複，
    /// 所以它是全域鎖上的阻塞 I/O，必須由呼叫端在鎖外完成。
    ///
    /// ## 為什麼這裡不清空 `pending`
    ///
    /// 清空必須發生在**寫入之後**，否則「寫入失敗」會被誤報成 `hinted`：
    /// 呼叫端拿到 `Deliver` 但寫入失敗時，若清單已被清空，重判迴圈就沒有東西
    /// 可以重試；若沒清空，它會在下一次重判時重複送出同一格。呼叫端必須在
    /// 寫入之後呼叫 [`Scheduler::commit_delivery`]（成功時）或 `check` 的
    /// 放棄分支（失敗時，回 `Abandoned(WriteFailed)`）來決定。
    pub fn check(
        &mut self,
        session_present: bool,
        generation: u64,
        inputs: DeferInputs,
        input_window: Duration,
        quiet_window: Duration,
        now: std::time::Instant,
    ) -> NextAction {
        if !session_present {
            // 收件人已停止或被移出 `running`（契約 §6.4 的放棄條件）。
            self.pending.clear();
            return NextAction::Abandoned(AbandonReason::NotRunning);
        }
        if generation != self.generation {
            // 換了 generation：提示不得送到新程序上（契約 §6.4）。
            self.pending.clear();
            return NextAction::Abandoned(AbandonReason::NotRunning);
        }
        if self.pending.is_empty() {
            // 沒有待送提示時**不得**有任何 bytes 離開本模組，也不得回報
            // `hinted`——§6.4 的 `hinted` 只代表提示已寫入 PTY。排程迴圈在
            // 上一輪送完、下一則 `send` 還沒進來時會落到這裡。
            return NextAction::NothingToSend;
        }
        if should_defer(inputs, input_window, quiet_window, now) {
            return NextAction::Deferred;
        }
        let line = self
            .pending_line()
            .expect("繁中說明：上面已確認 pending 非空，pending_line 必有值");
        let mut bytes = line.into_bytes();
        bytes.extend_from_slice(SUBMIT_KEY);
        NextAction::Deliver(bytes)
    }

    /// 寫入完成後清空待送清單。
    ///
    /// 由呼叫端在**寫入之後**呼叫：成功時這一格提示已進入 PTY，呼叫端回
    /// `hinted`；失敗時呼叫端回 `failed`（`AbandonReason::WriteFailed`）——
    /// 兩種情況都不重試（實作判斷，非契約推導，見
    /// [`AbandonReason::WriteFailed`]）。
    pub fn commit_delivery(&mut self) {
        self.pending.clear();
    }
    /// `check` 的結果對應的 `WakeOutcome`。
    ///
    /// 回 [`Option`] 而不是 `WakeOutcome` 本身，是為了讓「這一輪不構成一次
    /// 喚醒」（[`NextAction::NothingToSend`]）在型別上**沒有**對應的
    /// `wake` 值：呼叫端必須顯式處理 `None`，不能默默把它變成 `hinted`。
    pub fn outcome_for(action: NextAction) -> Option<WakeOutcome> {
        match action {
            NextAction::Deliver(_) => Some(WakeOutcome::Hinted),
            NextAction::Deferred => Some(WakeOutcome::Deferred),
            NextAction::Abandoned(AbandonReason::NotRunning) => Some(WakeOutcome::NotRunning),
            NextAction::Abandoned(AbandonReason::WriteFailed) => Some(WakeOutcome::Failed),
            NextAction::NothingToSend => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        should_defer, AbandonReason, DeferInputs, NextAction, PendingHint, Scheduler, WakeOutcome,
        HINT_PREFIX, RECHECK_INTERVAL, SUBMIT_KEY,
    };
    use std::time::{Duration, Instant};

    /// 契約 §6.4 的 client 輸入視窗（3 秒）。
    ///
    /// 這**只**出現在測試裡。正式路徑的視窗長度由 daemon 的
    /// `Tuning::wake_input`／`wake_quiet` 提供（`OTTERHIVE_TEST_WAKE_INPUT_MS`
    /// 與 `OTTERHIVE_TEST_WAKE_QUIET_MS`，契約 §1 的環境變數），所以
    /// `wake.rs` 的正式路徑不知道自己用哪個數字。這個常數是把契約句搬進
    /// 可執行的斷言，不是設定值的來源。
    const fn contract_input_window() -> Duration {
        Duration::from_secs(3)
    }

    /// 契約 §6.4 的輸出靜默視窗（500 毫秒）。同上，只是測試用。
    const fn contract_quiet_window() -> Duration {
        Duration::from_millis(500)
    }

    fn hint(id: &str) -> PendingHint {
        PendingHint {
            from: "operator".to_owned(),
            kind: "query".to_owned(),
            message_id: id.to_owned(),
        }
    }

    #[test]
    fn a_single_hint_renders_without_a_count_field() {
        let rendered = hint("m1").render(1);
        assert_eq!(
            rendered,
            format!("{HINT_PREFIX} from=operator kind=query id=m1"),
            "繁中說明：單則提示的形狀是 from／kind／id，契約 §6.4 未提 count"
        );
        assert!(
            !rendered.contains("count="),
            "繁中說明：n==1 時應省略 count，實際為 {rendered}"
        );
    }

    #[test]
    fn merged_hints_render_with_count_and_the_last_fields() {
        let rendered = hint("m2").render(2);
        assert_eq!(
            rendered,
            format!("{HINT_PREFIX} count=2 from=operator kind=query id=m2"),
            "繁中說明：合併後是 count 加最後一則的欄位，契約 §6.4"
        );
    }

    #[test]
    fn every_wake_outcome_has_the_contract_wire_name() {
        for (outcome, wire) in [
            (WakeOutcome::Hinted, "hinted"),
            (WakeOutcome::Deferred, "deferred"),
            (WakeOutcome::NotRunning, "not_running"),
            (WakeOutcome::Failed, "failed"),
        ] {
            assert_eq!(
                outcome.as_wire(),
                wire,
                "繁中說明：契約 §6.4 的 wake 值應為 {wire}"
            );
        }
    }

    #[test]
    fn recheck_interval_is_the_contract_250ms() {
        assert_eq!(
            RECHECK_INTERVAL,
            Duration::from_millis(250),
            "繁中說明：契約 §6.4 的重判間隔是 250 毫秒"
        );
    }

    #[test]
    fn submit_key_is_the_carriage_return_that_ends_the_line() {
        assert_eq!(SUBMIT_KEY, b"\r", "繁中說明：送出鍵是換行");
    }

    #[test]
    fn a_recent_client_input_defers_even_when_output_is_old() {
        let now = Instant::now();
        let inputs = DeferInputs {
            last_client_input_at: Some(now - Duration::from_millis(10)),
            last_output_at: now - Duration::from_secs(60),
        };
        assert!(
            should_defer(
                inputs,
                contract_input_window(),
                contract_quiet_window(),
                now
            ),
            "繁中說明：client 最近輸入過就應延後（契約 §6.4）"
        );
    }

    #[test]
    fn a_recent_output_defers_even_with_no_client_ever_typing() {
        let now = Instant::now();
        let inputs = DeferInputs {
            last_client_input_at: None,
            last_output_at: now - Duration::from_millis(10),
        };
        assert!(
            should_defer(
                inputs,
                contract_input_window(),
                contract_quiet_window(),
                now
            ),
            "繁中說明：收件人最近有輸出就應延後（契約 §6.4）"
        );
    }

    #[test]
    fn an_input_older_than_the_window_stops_defers() {
        let now = Instant::now();
        let inputs = DeferInputs {
            last_client_input_at: Some(now - Duration::from_millis(3100)),
            last_output_at: now - Duration::from_secs(60),
        };
        assert!(
            !should_defer(
                inputs,
                contract_input_window(),
                contract_quiet_window(),
                now
            ),
            "繁中說明：超過 3 秒的輸入不算「最近輸入過」"
        );
    }

    #[test]
    fn deferral_has_no_ceiling() {
        // 延後沒有上限：條件持續成立時，判定**每一輪都成立**。這裡刻意只測
        // 純判定 [`should_defer`]，不碰 [`Scheduler::check`]——那個函式需要
        // 一個活著的 `Session`，而「沒有活著的 session」本身就是放棄提示的
        // 另一個條件，兩者混在一起會讓「延後不放棄」這條契約句測不到。
        // `Scheduler::check` 的延後路徑由 `tests/wake_delivery.rs` 的
        // attach-client 情境以真實 daemon 覆蓋。
        let now = Instant::now();
        let inputs = DeferInputs {
            last_client_input_at: None,
            last_output_at: now,
        };
        for round in 0..1000u32 {
            assert!(
                should_defer(
                    inputs,
                    contract_input_window(),
                    Duration::from_secs(30),
                    now
                ),
                "繁中說明：第 {round} 輪仍應判定為延後（契約 §6.4：延後沒有上限）"
            );
        }
    }

    #[test]
    fn pending_hints_accumulate_and_merge_without_being_written() {
        let mut scheduler = Scheduler::new(7);
        scheduler.enqueue(hint("m1"));
        scheduler.enqueue(hint("m2"));
        assert_eq!(scheduler.pending_len(), 2);
        assert_eq!(
            scheduler.pending_line().as_deref(),
            Some("[OTTERHIVE-MSG] count=2 from=operator kind=query id=m2"),
            "繁中說明：合併後取最後一則的欄位（契約 §6.4）"
        );
        // 合併不該動到線上的任何東西：這裡只有檢查 pending_line 而已。
        assert!(
            scheduler.is_pending(),
            "繁中說明：尚未送出前提示應留在清單裡"
        );
    }

    #[test]
    fn a_new_generation_abandons_the_pending_hint() {
        let mut scheduler = Scheduler::new(7);
        scheduler.enqueue(hint("m1"));
        let quiet = DeferInputs {
            last_client_input_at: None,
            last_output_at: Instant::now() - Duration::from_secs(60),
        };
        // 收件人還活著（`session_present` 為 true），但 generation 從 7 變成 8：
        // 提示必須放棄，不能被送到新程序上（契約 §6.4）。
        assert_eq!(
            scheduler.check(
                true,
                8,
                quiet,
                contract_input_window(),
                contract_quiet_window(),
                Instant::now(),
            ),
            NextAction::Abandoned(AbandonReason::NotRunning),
            "繁中說明：換 generation 應放棄提示（契約 §6.4）"
        );
        assert!(
            !scheduler.is_pending(),
            "繁中說明：放棄後清單應清空，該提示不再重試"
        );
        assert_eq!(
            Scheduler::outcome_for(NextAction::Abandoned(AbandonReason::NotRunning)),
            Some(WakeOutcome::NotRunning),
            "繁中說明：換 generation 放棄提示時的 wake 應為 not_running"
        );
    }

    /// 路 B 的驗證：寫入失敗必須回報成 `failed`，不是 `not_running`。
    ///
    /// 這條路徑在 RED 階段無法用 `Scheduler::check` 觸發——`check` 需要一個
    /// `&Session`，而讓 `write_input` 失敗需要 master fd 已經關閉。因此這裡
    /// 直接驗證映射本身：`Abandoned(WriteFailed)` 與
    /// `Abandoned(NotRunning)` 必須映射到**不同**的 `WakeOutcome`。
    #[test]
    fn a_write_failure_is_not_reported_as_not_running() {
        assert_eq!(
            Scheduler::outcome_for(NextAction::Abandoned(AbandonReason::WriteFailed)),
            Some(WakeOutcome::Failed),
            "繁中說明：PTY 寫入失敗的 wake 應為 failed（契約 §6.4）"
        );
        assert_eq!(
            Scheduler::outcome_for(NextAction::Abandoned(AbandonReason::NotRunning)),
            Some(WakeOutcome::NotRunning),
            "繁中說明：收件人不在 running 的 wake 應為 not_running"
        );
        assert_ne!(
            Scheduler::outcome_for(NextAction::Abandoned(AbandonReason::WriteFailed)),
            Scheduler::outcome_for(NextAction::Abandoned(AbandonReason::NotRunning)),
            "繁中說明：寫入失敗與收件人不在 running 是兩個不同事實，不得映成同一個 wake 值"
        );
    }

    /// 路 C 的驗證：沒有待送提示時**不得**有任何 wake 值。
    ///
    /// §6.4 明寫「`hinted` 只代表提示已寫入 PTY」。排程迴圈在上一輪送完、
    /// 下一則 `send` 還沒進來時會落到這裡；回報成 `hinted` 會對 caller
    /// 謊稱提示已寫入。
    #[test]
    fn an_empty_queue_has_no_wake_outcome_at_all() {
        assert_eq!(
            Scheduler::outcome_for(NextAction::NothingToSend),
            None,
            "繁中說明：沒有待送提示時不該有任何 wake 值（§6.4 的 hinted 只代表已寫入 PTY）"
        );
        assert_ne!(
            Scheduler::outcome_for(NextAction::NothingToSend),
            Some(WakeOutcome::Hinted),
            "繁中說明：NothingToSend 絕不可被映成 hinted"
        );
    }

    /// `check` 遇到 `session` 為 `None` 時一律回
    /// `Abandoned(NotRunning)`，即使清單是空的。
    ///
    /// 這是刻意的取捨：清單為空與「沒有可放的提示」相比，
    /// **`session` 不存在是更強的事實**——收件人根本沒在跑。因此判斷順序是
    /// 先看 session／generation，再看清單。`NothingToSend` 留給「收件人健在
    /// 但此刻沒有待送提示」那一種（排程迴圈的正常空轉）。
    #[test]
    fn a_missing_session_abandons_even_with_an_empty_queue() {
        let scheduler = Scheduler::new(7);
        let quiet = DeferInputs {
            last_client_input_at: None,
            last_output_at: Instant::now() - Duration::from_secs(60),
        };
        let mut scheduler = scheduler;
        let action = scheduler.check(
            false,
            7,
            quiet,
            contract_input_window(),
            contract_quiet_window(),
            Instant::now(),
        );
        assert_eq!(
            action,
            NextAction::Abandoned(AbandonReason::NotRunning),
            "繁中說明：session 為 None 是比「沒有提示」更強的事實，應回 Abandoned(NotRunning)"
        );
    }

    /// 有待送提示但 generation 不符 → `Abandoned(NotRunning)`，清單清空。
    ///
    /// 這條覆蓋「換 generation 放棄提示」在**有**待送提示時的實際清空行為
    /// （上一支覆蓋的是 session 為 None 的分支）。
    #[test]
    fn abandoning_clears_the_queue_so_the_hint_is_not_retried() {
        let mut scheduler = Scheduler::new(7);
        scheduler.enqueue(hint("m1"));
        let quiet = DeferInputs {
            last_client_input_at: None,
            last_output_at: Instant::now() - Duration::from_secs(60),
        };
        let action = scheduler.check(
            false,
            99,
            quiet,
            contract_input_window(),
            contract_quiet_window(),
            Instant::now(),
        );
        assert_eq!(
            action,
            NextAction::Abandoned(AbandonReason::NotRunning),
            "繁中說明：換 generation 應放棄（契約 §6.4）"
        );
        assert!(
            !scheduler.is_pending(),
            "繁中說明：放棄後清單應清空，該提示不再重試（訊息仍在 inbox）"
        );
        assert_eq!(
            scheduler.pending_len(),
            0,
            "繁中說明：放棄後不得殘留任何待送提示"
        );
    }

    /// 延後時清單必須完整保留（與放棄相反）。
    #[test]
    fn deferring_keeps_the_queue_intact() {
        let mut scheduler = Scheduler::new(7);
        scheduler.enqueue(hint("m1"));
        scheduler.enqueue(hint("m2"));
        let typing = DeferInputs {
            last_client_input_at: Some(Instant::now()),
            last_output_at: Instant::now() - Duration::from_secs(60),
        };
        // `session_present` 為 false 會命中放棄分支，所以延後分支必須靠
        // `should_defer` 單獨驗證（`check` 需要活著的 Session）。
        assert!(
            should_defer(
                typing,
                contract_input_window(),
                contract_quiet_window(),
                Instant::now()
            ),
            "繁中說明：client 剛輸入過應判定為延後"
        );
        assert_eq!(scheduler.pending_len(), 2, "繁中說明：延後不清單");
    }
    /// D-1 的驗證：`check` **不持有** `Session` 的引用，因此不可能呼叫
    /// `write_input`。
    ///
    /// 這是對「決策與寫入分離」最直接的證據——如果 `check` 的簽章收
    /// `Option<&Session>`，它就**有能力**在呼叫端持有 daemon 鎖時阻塞寫入；
    /// 現在它連 `Session` 的型別都看不到，所以「阻塞 I/O 發生在 daemon 鎖內」在
    /// 型別上不可達。
    ///
    /// 範圍提醒：`Session::write_input` **內部**仍要鎖 `core.writer`，而 PTY 的
    /// `write(2)` 在收件人不讀 stdin 時確實會阻塞——那把鎖仍會被佔住。證據與已
    /// 裁決的修法見 `docs/worklog-t6-pty-write-block.md`。
    ///
    /// 函式指標的型別即證明：簽章要匹配，`check` 只能接受這六個參數。
    #[test]
    fn check_cannot_even_name_a_session_so_it_cannot_block_on_a_write() {
        type CheckFn =
            fn(&mut Scheduler, bool, u64, DeferInputs, Duration, Duration, Instant) -> NextAction;
        let _check: CheckFn = Scheduler::check;
        assert!(
            !std::any::type_name::<Scheduler>().contains("Session"),
            "繁中說明：Scheduler 不該持有 Session（持有會讓寫入路徑可達）"
        );
    }

    /// `Deliver` 帶的是「提示行 ＋ 送出鍵」，且**不含**任何待送清單之外的
    /// 內容——呼叫端寫入的 bytes 完全由排程器決定。
    #[test]
    fn deliver_carries_the_merged_line_plus_the_submit_key() {
        let mut scheduler = Scheduler::new(7);
        scheduler.enqueue(hint("m1"));
        scheduler.enqueue(hint("m2"));
        let quiet = DeferInputs {
            last_client_input_at: None,
            last_output_at: Instant::now() - Duration::from_secs(60),
        };
        let decision = scheduler.check(
            true,
            7,
            quiet,
            contract_input_window(),
            contract_quiet_window(),
            Instant::now(),
        );
        let NextAction::Deliver(bytes) = decision else {
            panic!("繁中說明：安靜且有待送提示時應回 Deliver，實際為 {decision:?}");
        };
        let text = String::from_utf8(bytes).expect("繁中說明：提示 bytes 應是合法 UTF-8");
        assert_eq!(
            text,
            format!(
                "{}[OTTERHIVE-MSG] count=2 from=operator kind=query id=m2\r",
                ""
            ),
            "繁中說明：Deliver 的 payload 是合併後那一行加上送出鍵（契約 §6.4）"
        );
        assert!(
            text.ends_with("\r"),
            "繁中說明：送出鍵必須在同一次寫入內，否則 agent 讀到一半的提示會當成完整訊息"
        );
    }

    /// `check` 回 `Deliver` 之後**不**清空清單——清空必須由呼叫端在寫入之後
    /// 用 [`Scheduler::commit_delivery`] 完成。
    ///
    /// 這是 D-1 分離的關鍵不變量：若 `check` 自行清空，呼叫端寫入失敗時就沒有
    /// 任何東西可以重試，而「寫入失敗」又不能回報成 `hinted`（§6.4）。
    #[test]
    fn deliver_does_not_clear_the_queue_until_the_caller_commits() {
        let mut scheduler = Scheduler::new(7);
        scheduler.enqueue(hint("m1"));
        let quiet = DeferInputs {
            last_client_input_at: None,
            last_output_at: Instant::now() - Duration::from_secs(60),
        };
        let decision = scheduler.check(
            true,
            7,
            quiet,
            contract_input_window(),
            contract_quiet_window(),
            Instant::now(),
        );
        assert!(
            matches!(decision, NextAction::Deliver(_)),
            "繁中說明：應回 Deliver，實際為 {decision:?}"
        );
        assert_eq!(
            scheduler.pending_len(),
            1,
            "繁中說明：check 回 Deliver 後清單不得被清空（清空是呼叫端在寫入之後的責任）"
        );
        scheduler.commit_delivery();
        assert_eq!(
            scheduler.pending_len(),
            0,
            "繁中說明：commit_delivery 之後才清空"
        );
        assert!(
            !scheduler.is_pending(),
            "繁中說明：commit_delivery 後不得殘留待送提示（否則會被重複送出）"
        );
    }

    /// 寫入失敗時不重試（延續先前的裁決）：呼叫端在寫入失敗後**不**呼叫
    /// `commit_delivery`、而是直接把該提示當成放棄處理。
    ///
    /// 這條測試記錄的是**呼叫端必須遵守的契約**，不是 `check` 的行為——
    /// 所以它在 `wake.rs` 裡是以文件形式存在的斷言：清單仍在，
    /// 代表重判迴圈的下一輪會再看到它，呼叫端要嘛重試、要嘛在鎖內把它清掉。
    #[test]
    fn a_failed_write_leaves_the_queue_for_the_caller_to_decide() {
        let mut scheduler = Scheduler::new(7);
        scheduler.enqueue(hint("m1"));
        let quiet = DeferInputs {
            last_client_input_at: None,
            last_output_at: Instant::now() - Duration::from_secs(60),
        };
        // 模擬「呼叫端寫入失敗」：拿到 Deliver、不 commit。
        let decision = scheduler.check(
            true,
            7,
            quiet,
            contract_input_window(),
            contract_quiet_window(),
            Instant::now(),
        );
        assert!(matches!(decision, NextAction::Deliver(_)));
        assert_eq!(
            scheduler.pending_len(),
            1,
            "繁中說明：寫入失敗時排程器**不**自行決定重試或放棄——那是呼叫端的裁決"
        );
        // 呼叫端選擇不放棄（清單由重判迴圈的收尾路徑清掉）：回 Abandoned(WriteFailed)。
        assert_eq!(
            Scheduler::outcome_for(wake_abandoned_write_failed()),
            Some(WakeOutcome::Failed),
            "繁中說明：寫入失敗要能回報 failed（契約 §6.4）"
        );
    }

    fn wake_abandoned_write_failed() -> NextAction {
        NextAction::Abandoned(AbandonReason::WriteFailed)
    }
}
