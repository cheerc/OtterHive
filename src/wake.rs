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

use crate::terminal::session::Session;

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

impl DeferInputs {
    /// 目前 client 輸入視窗（契約 §6.4 的 3 秒；測試可縮短）。
    pub const fn input_window() -> Duration {
        Duration::from_millis(3000)
    }

    /// 目前輸出靜默視窗（契約 §6.4 的 500 毫秒；測試可縮短）。
    pub const fn quiet_window() -> Duration {
        Duration::from_millis(500)
    }
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

/// 排程器下一步該做什麼。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextAction {
    /// 把這一列寫入 `session`，回 [`WakeOutcome::Hinted`]。
    Deliver,
    /// 條件未解除，等 [`RECHECK_INTERVAL`] 再判斷。
    ///
    /// 沒有「放棄」分支：延後沒有上限。
    Deferred,
    /// 收件人已停止或換了 generation，放棄該提示。
    ///
    /// 訊息仍在 inbox（契約 §6.4）。
    Abandoned,
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

    /// 依目前狀態決定下一步。
    ///
    /// - `session` 為 `None`（收件人已停止或換 generation）→ `Abandoned`，
    ///   並清空待送清單（該提示就此放棄，訊息仍在 inbox）。
    /// - `inputs` 顯示延後條件成立 → `Deferred`（不動清單，稍後重判）。
    /// - 否則 → `Deliver`：把合併後那一行連同送出鍵寫入 PTY，並清空清單。
    pub fn check(
        &mut self,
        session: Option<&Session>,
        generation: u64,
        inputs: DeferInputs,
        input_window: Duration,
        quiet_window: Duration,
        now: std::time::Instant,
    ) -> NextAction {
        if session.is_none() || generation != self.generation {
            self.pending.clear();
            return NextAction::Abandoned;
        }
        if should_defer(inputs, input_window, quiet_window, now) {
            return NextAction::Deferred;
        }
        let Some(session) = session else {
            // `is_none` 已在上面擋掉；這裡只是讓編譯器滿意。
            return NextAction::Abandoned;
        };
        let Some(line) = self.pending_line() else {
            // 沒有待送提示時不該寫入空行；回 `Deliver` 讓呼叫端看到「這輪
            // 沒事可做」，但沒有任何 bytes 離開本模組。
            return NextAction::Deliver;
        };
        let mut bytes = line.into_bytes();
        bytes.extend_from_slice(SUBMIT_KEY);
        let written = session.write_input(&bytes).is_ok();
        self.pending.clear();
        if written {
            NextAction::Deliver
        } else {
            // 寫入失敗與「收件人已不在」是不同的事實：清單已清空（提示不再
            // 重試），但結果必須如實回報失敗而不是假裝送達。
            NextAction::Abandoned
        }
    }

    /// `check` 的結果對應的 `WakeOutcome`。
    ///
    /// `Abandoned` 依「收件人在提示送出前停止或換了 generation」與「寫入
    /// 失敗」兩種情形分別回報：排程器用 [`NextAction::Abandoned`] 統一口
    /// 徑處理，但 `wake` 欄位必須區分（契約 §6.4 的四個值）。
    pub fn outcome_for(action: NextAction) -> WakeOutcome {
        match action {
            NextAction::Deliver => WakeOutcome::Hinted,
            NextAction::Deferred => WakeOutcome::Deferred,
            NextAction::Abandoned => WakeOutcome::NotRunning,
        }
    }
}

/// 產生 `send` 結果中每個收件人的 `wake` 欄位（契約 §6.1／§6.4）。
///
/// 這裡只負責把排程結果放進 `value.recipients[]` 的形狀裡；訊息本體的交易
/// 由呼叫端在**提交之後**才呼叫本函式，確保「喚醒失敗不撤回訊息」。
pub fn recipients_value(entries: &[(String, WakeOutcome, String)]) -> serde_json::Value {
    serde_json::Value::Array(
        entries
            .iter()
            .map(|(instance, wake, message_id)| {
                serde_json::json!({
                    "instance": instance,
                    "outcome": "queued",
                    "message_id": message_id,
                    "wake": wake.as_wire(),
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        should_defer, DeferInputs, NextAction, PendingHint, Scheduler, WakeOutcome, HINT_PREFIX,
        RECHECK_INTERVAL, SUBMIT_KEY,
    };
    use std::time::{Duration, Instant};

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
    fn default_windows_match_the_contract() {
        assert_eq!(DeferInputs::input_window(), Duration::from_secs(3));
        assert_eq!(DeferInputs::quiet_window(), Duration::from_millis(500));
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
                DeferInputs::input_window(),
                DeferInputs::quiet_window(),
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
                DeferInputs::input_window(),
                DeferInputs::quiet_window(),
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
                DeferInputs::input_window(),
                DeferInputs::quiet_window(),
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
                    DeferInputs::input_window(),
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
        // 收件人還活著（`session` 是 `Some`），但 generation 從 7 變成 8：
        // 提示必須放棄，不能被送到新程序上（契約 §6.4）。
        assert_eq!(
            scheduler.check(
                None,
                8,
                quiet,
                DeferInputs::input_window(),
                DeferInputs::quiet_window(),
                Instant::now(),
            ),
            NextAction::Abandoned,
            "繁中說明：換 generation 應放棄提示（契約 §6.4）"
        );
        assert!(
            !scheduler.is_pending(),
            "繁中說明：放棄後清單應清空，該提示不再重試"
        );
        assert_eq!(
            Scheduler::outcome_for(NextAction::Abandoned),
            WakeOutcome::NotRunning,
            "繁中說明：換 generation 放棄提示時的 wake 應為 not_running"
        );
    }
}
