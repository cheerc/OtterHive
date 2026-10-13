//! 契約 §7.2 的畫面重繪（Snapshot 的 ANSI 部分）與純文字輸出。
//!
//! `Screen` 是「位元組進、畫面出」的純函式式容器：它持有 alacritty 的 VT
//! 終端模擬器，把 PTY 輸出餵進去，並能把目前的可見畫面重繪成一段 ANSI
//! 序列（Snapshot 的 payload）或純文字（`pane_snapshot` 的 `value.text`）。
//!
//! 它**不**帶 generation、seq、PTY 或訂閱狀態 —— 那些屬於 `Session`。
//!
//! # 游標語意（重要）
//!
//! 重繪使用**原始寫入游標** `Term::grid().cursor.point`，不是
//! `renderable_content().cursor` 的顯示調整值。後者會把落在寬字元 spacer
//! 上的游標回退一格，那是給「顯示」用的；若拿來寫 CUP 定位，重播後游標會
//! 停在錯誤的格上（round-trip 失敗）。游標**可見性**則沒有原始值可讀，只能
//! 取自 `renderable_content().cursor.shape`。

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color, CursorShape, Processor, Rgb};

/// 終端畫面狀態：把 PTY 位元組餵進 VT，並重繪可見畫面。
///
/// 只接受正尺寸；尺寸的邊界檢查由 `Session` 的 spawn／resize 負責。
pub(crate) struct Screen {
    term: Term<VoidListener>,
    processor: Processor,
    cols: u16,
    rows: u16,
}

/// `Screen` 自有的螢幕尺寸（不使用 `alacritty_terminal::term::test` 的 `TermSize`）。
#[derive(Clone, Copy)]
struct GridSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

impl Screen {
    /// 建立一張 `cols` × `rows` 的空白畫面。
    ///
    /// `scrolling_history` 設為 0：只需要可見區，這也讓 `display_iter` 與
    /// 行索引一致（見模組層說明）。
    pub(crate) fn new(cols: u16, rows: u16) -> Self {
        let config = Config {
            scrolling_history: 0,
            ..Config::default()
        };
        let size = GridSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        Screen {
            term: Term::new(config, &size, VoidListener),
            processor: Processor::new(),
            cols,
            rows,
        }
    }

    /// 把 PTY 輸出餵進終端模擬器。
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    /// 改變畫面尺寸（終端會 reflow 內容）。
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(GridSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        });
        self.cols = cols;
        self.rows = rows;
    }

    /// 目前欄數。
    pub(crate) fn cols(&self) -> u16 {
        self.cols
    }

    /// 目前列數。
    pub(crate) fn rows(&self) -> u16 {
        self.rows
    }

    /// 重繪整個可見畫面與游標位置的 ANSI 序列（契約 §7.2 的 Snapshot 畫面 bytes）。
    ///
    /// 輸出是一段可重播的序列：重置屬性、清畫面、逐列重繪、游標定位。
    /// 重播進另一個 Screen 後，每格的字元／樣式／前景背景色／游標位置都應
    /// 與原本等價。
    ///
    /// 寬字元的「後接空格」（`WIDE_CHAR_SPACER`）不輸出 —— 它只是寬字元佔用的
    /// 第二格，不是獨立文字；重播時終端會自行重建。
    pub(crate) fn render_ansi(&self) -> Vec<u8> {
        let mut out = Vec::new();

        // 重置所有屬性 → 游標定位 → 清除畫面（清除時用預設背景色）。
        out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");

        // 逐列重繪。每列都先明確定位到該列起點，不依賴清屏後的游標位置。
        for row in 0..self.rows {
            out.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
            // 每列第一格一定重發 SGR（`None` 與任何樣式都不同）。
            let mut current: Option<CellStyle> = None;

            for col in 0..self.cols {
                let cell = &self.term.grid()[Point {
                    line: Line(row as i32),
                    column: Column(col as usize),
                }];

                // 寬字元的後接空格不輸出；由重播端自行重建。
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }

                let style = CellStyle::of(cell);

                // 只在樣式改變時重發 SGR；列首與樣式邊界都會重發。
                if current != Some(style) {
                    // 屬性變更一律從完整 reset 起算，避免殘留上一次的樣式。
                    out.extend_from_slice(b"\x1b[0");
                    if cell.flags.contains(Flags::BOLD) {
                        out.extend_from_slice(b";1");
                    }
                    if cell.flags.contains(Flags::DIM) {
                        out.extend_from_slice(b";2");
                    }
                    if cell.flags.contains(Flags::ITALIC) {
                        out.extend_from_slice(b";3");
                    }
                    if cell.flags.contains(Flags::UNDERLINE) {
                        out.extend_from_slice(b";4");
                    }
                    if cell.flags.contains(Flags::INVERSE) {
                        out.extend_from_slice(b";7");
                    }
                    if cell.flags.contains(Flags::STRIKEOUT) {
                        out.extend_from_slice(b";9");
                    }
                    out.push(b'm');

                    push_color(&mut out, cell.fg, true);
                    push_color(&mut out, cell.bg, false);
                    current = Some(style);
                }

                // 主字元，再接零寬字元（合併音符等）。
                let mut buffer = [0u8; 4];
                out.extend_from_slice(cell.c.encode_utf8(&mut buffer).as_bytes());
                if let Some(zerowidth) = cell.zerowidth() {
                    for ch in zerowidth {
                        out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
                    }
                }
            }
        }

        // 游標定位用**原始寫入游標**（不是顯示調整值）。
        let cursor = self.term.grid().cursor.point;
        out.extend_from_slice(
            format!("\x1b[{};{}H", cursor.line.0 + 1, cursor.column.0 + 1).as_bytes(),
        );

        // 游標可見性：沒有原始值可讀，取自 renderable 的 shape。
        match self.term.renderable_content().cursor.shape {
            CursorShape::Hidden => out.extend_from_slice(b"\x1b[?25l"),
            _ => out.extend_from_slice(b"\x1b[?25h"),
        }

        out
    }

    /// 可見畫面的純文字，逐行一筆（契約 §4.7 `pane_snapshot` 的 `value.text`）。
    ///
    /// 每列去掉行尾空白；行首空白保留（游標定位留下的前導空白是畫面上真實的
    /// 內容）。寬字元的後接空格不輸出。
    pub(crate) fn plain_lines(&self) -> Vec<String> {
        (0..self.rows)
            .map(|row| {
                let mut line = String::new();
                for col in 0..self.cols {
                    let cell = &self.term.grid()[Point {
                        line: Line(row as i32),
                        column: Column(col as usize),
                    }];
                    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        continue;
                    }
                    line.push(cell.c);
                    if let Some(zerowidth) = cell.zerowidth() {
                        line.extend(zerowidth);
                    }
                }
                line.trim_end().to_string()
            })
            .collect()
    }
}

/// 單格的樣式快照，用來判斷是否需要重發 SGR。
#[derive(Clone, Copy, PartialEq, Eq)]
struct CellStyle {
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
    strikethrough: bool,
    fg: Color,
    bg: Color,
}

impl CellStyle {
    fn of(cell: &Cell) -> Self {
        CellStyle {
            bold: cell.flags.contains(Flags::BOLD),
            dim: cell.flags.contains(Flags::DIM),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: cell.flags.contains(Flags::UNDERLINE),
            inverse: cell.flags.contains(Flags::INVERSE),
            strikethrough: cell.flags.contains(Flags::STRIKEOUT),
            fg: cell.fg,
            bg: cell.bg,
        }
    }
}

/// 輸出一個前景／背景色的 SGR 序列。
fn push_color(out: &mut Vec<u8>, color: Color, foreground: bool) {
    match color {
        Color::Spec(Rgb { r, g, b }) => {
            let prefix = if foreground { 38 } else { 48 };
            out.extend_from_slice(format!("\x1b[{prefix};2;{r};{g};{b}m").as_bytes());
        }
        Color::Indexed(index) => {
            let prefix = if foreground { 38 } else { 48 };
            out.extend_from_slice(format!("\x1b[{prefix};5;{index}m").as_bytes());
        }
        Color::Named(named) => match named_sgr_code(named, foreground) {
            Some(code) => out.extend_from_slice(format!("\x1b[{code}m").as_bytes()),
            None => out.extend_from_slice(
                format!("\x1b[{}m", if foreground { 39 } else { 49 }).as_bytes(),
            ),
        },
    }
}

/// 具名色對應的 SGR 碼；`None` 代表「預設色」，用 39／49 表達。
///
/// 前 16 色與亮色有直接對應的 SGR。`Dim*` 系列在終端協定裡沒有獨立碼，
/// 以其亮色變體近似 —— 這是顯示近似，會讓重播後的具名色不同於原值；
/// 本 Screen 的測試不涵蓋 dim 具名色（dim 以 `Flags::DIM` 屬性表達，而非
/// 以 `Dim*` 顏色表達）。
fn named_sgr_code(
    named: alacritty_terminal::vte::ansi::NamedColor,
    foreground: bool,
) -> Option<u8> {
    use alacritty_terminal::vte::ansi::NamedColor;

    // 預設色不對應具體碼。
    match named {
        NamedColor::Foreground
        | NamedColor::Background
        | NamedColor::BrightForeground
        | NamedColor::DimForeground
        | NamedColor::Cursor => {
            let _ = foreground;
            return None;
        }
        _ => {}
    }

    let code = match named {
        NamedColor::Black => 30,
        NamedColor::Red => 31,
        NamedColor::Green => 32,
        NamedColor::Yellow => 33,
        NamedColor::Blue => 34,
        NamedColor::Magenta => 35,
        NamedColor::Cyan => 36,
        NamedColor::White => 37,
        NamedColor::BrightBlack => 90,
        NamedColor::BrightRed => 91,
        NamedColor::BrightGreen => 92,
        NamedColor::BrightYellow => 93,
        NamedColor::BrightBlue => 94,
        NamedColor::BrightMagenta => 95,
        NamedColor::BrightCyan => 96,
        NamedColor::BrightWhite => 97,
        // dim 系列沒有對應碼，以亮色近似。
        NamedColor::DimBlack => 90,
        NamedColor::DimRed => 91,
        NamedColor::DimGreen => 92,
        NamedColor::DimYellow => 93,
        NamedColor::DimBlue => 94,
        NamedColor::DimMagenta => 95,
        NamedColor::DimCyan => 96,
        NamedColor::DimWhite => 97,
        // 預設色已在前面處理。
        NamedColor::Foreground
        | NamedColor::Background
        | NamedColor::Cursor
        | NamedColor::BrightForeground
        | NamedColor::DimForeground => return None,
    };

    if foreground {
        Some(code)
    } else {
        Some(code + 10)
    }
}

#[cfg(test)]
mod tests {
    use super::Screen;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::Point;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;

    /// 測試用的最小 Dimensions 實作（不使用 `alacritty_terminal::term::test`）。
    #[derive(Clone, Copy)]
    struct GridSize {
        columns: usize,
        screen_lines: usize,
    }

    impl Dimensions for GridSize {
        fn total_lines(&self) -> usize {
            self.screen_lines
        }

        fn screen_lines(&self) -> usize {
            self.screen_lines
        }

        fn columns(&self) -> usize {
            self.columns
        }
    }

    /// 建立一個只餵過位元組的參考 Term，作為比對用的參考實作。
    fn reference(cols: u16, rows: u16, feed: &[u8]) -> Term<VoidListener> {
        // scrolling_history = 0：只需要可見區，display 就是 viewport。
        let config = Config {
            scrolling_history: 0,
            ..Config::default()
        };
        let size = GridSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        // Processor 的 Timeout 型別參數必須顯式指定為預設的 StdSyncHandler；
        // 放在 tuple 中無法推導，會造成 E0283。
        let mut term = Term::new(config, &size, VoidListener);
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, feed);
        term
    }

    /// 螢幕上每個 cell 的語意描述，用來比較兩張畫面是否等價。
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct CellView {
        char: char,
        /// 寬字元的「後接空格」——語意上不是真的空格，dump 時必須跳過。
        wide_spacer: bool,
        leading_wide_spacer: bool,
        /// cell.c 之外的零寬字元（合併音符等）。
        zerowidth: String,
        bold: bool,
        italic: bool,
        underline: bool,
        inverse: bool,
        dim: bool,
        strikethrough: bool,
        fg: ColorView,
        bg: ColorView,
    }

    /// 顏色的可比較表示：保留 named / indexed / truecolor 的區別。
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum ColorView {
        Named(String),
        Indexed(u8),
        Rgb(u8, u8, u8),
    }

    impl ColorView {
        fn of(color: alacritty_terminal::vte::ansi::Color) -> Self {
            use alacritty_terminal::vte::ansi::{Color, Rgb};
            match color {
                Color::Named(named) => ColorView::Named(format!("{named:?}")),
                Color::Indexed(index) => ColorView::Indexed(index),
                Color::Spec(Rgb { r, g, b }) => ColorView::Rgb(r, g, b),
            }
        }

        /// 判斷是否為非預設顏色（Named(Foreground/Background) 視為預設）。
        fn is_non_default(&self) -> bool {
            match self {
                ColorView::Named(name) => {
                    !name.contains("Foreground") && !name.contains("Background")
                }
                ColorView::Indexed(_) | ColorView::Rgb(..) => true,
            }
        }
    }

    /// 把 `Term` 的每格畫成 `CellView`；`term` 由呼叫端持有。
    ///
    /// 讀 `term` 是刻意的：Screen 的私有 Term 欄位在同一個 crate 內可直接取用，
    /// 因此不需要「為測試而公開」的存取器。
    fn cell_views(term: &Term<VoidListener>, cols: u16, rows: u16) -> Vec<Vec<CellView>> {
        (0..rows)
            .map(|row| {
                (0..cols)
                    .map(|col| {
                        let cell = &term.grid()[Point {
                            line: alacritty_terminal::index::Line(row as i32),
                            column: alacritty_terminal::index::Column(col as usize),
                        }];
                        CellView {
                            char: cell.c,
                            wide_spacer: cell
                                .flags
                                .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER),
                            leading_wide_spacer: cell.flags.contains(
                                alacritty_terminal::term::cell::Flags::LEADING_WIDE_CHAR_SPACER,
                            ),
                            zerowidth: cell
                                .zerowidth()
                                .map(|chars| chars.iter().collect())
                                .unwrap_or_default(),
                            bold: cell
                                .flags
                                .contains(alacritty_terminal::term::cell::Flags::BOLD),
                            italic: cell
                                .flags
                                .contains(alacritty_terminal::term::cell::Flags::ITALIC),
                            underline: cell
                                .flags
                                .contains(alacritty_terminal::term::cell::Flags::UNDERLINE),
                            inverse: cell
                                .flags
                                .contains(alacritty_terminal::term::cell::Flags::INVERSE),
                            dim: cell
                                .flags
                                .contains(alacritty_terminal::term::cell::Flags::DIM),
                            strikethrough: cell
                                .flags
                                .contains(alacritty_terminal::term::cell::Flags::STRIKEOUT),
                            fg: ColorView::of(cell.fg),
                            bg: ColorView::of(cell.bg),
                        }
                    })
                    .collect()
            })
            .collect()
    }

    /// Screen 內部的 Term（test-private 直接讀私有欄位）。
    fn term_of(screen: &Screen) -> &Term<VoidListener> {
        &screen.term
    }

    /// 驗證 render_ansi 的輸出餵回同尺寸新畫面後，cell 語意完全等價。
    ///
    /// 這是「不固定 byte spelling」的驗證方式：只要求語意正確，不要求位元組相同。
    fn assert_render_round_trips(screen: &Screen, cols: u16, rows: u16, what: &str) {
        let original = cell_views(term_of(screen), cols, rows);
        let rendered = screen.render_ansi();

        let replay = reference(cols, rows, &rendered);
        let replayed = cell_views(&replay, cols, rows);

        assert_eq!(
            replayed, original,
            "{what}：把 render_ansi 餵回新畫面後，每格語意必須等價"
        );
    }

    // --- 基本尺寸 ---

    #[test]
    fn new_screen_reports_requested_dimensions() {
        let screen = Screen::new(80, 24);
        assert_eq!(screen.cols(), 80, "cols 應回報建立時的欄數");
        assert_eq!(screen.rows(), 24, "rows 應回報建立時的列數");
    }

    #[test]
    fn resize_updates_reported_dimensions() {
        let mut screen = Screen::new(80, 24);
        screen.resize(100, 30);
        assert_eq!(screen.cols(), 100, "resize 後 cols 應回更新");
        assert_eq!(screen.rows(), 30, "resize 後 rows 應回更新");
    }

    #[test]
    fn resize_to_same_dimensions_keeps_content() {
        let mut screen = Screen::new(80, 24);
        screen.feed(b"unchanged");
        screen.resize(80, 24);
        assert_eq!(
            screen.plain_lines()[0],
            "unchanged",
            "同尺寸 resize 不應清空畫面"
        );
    }

    // --- 純文字 ---
    //
    // `plain_lines` 的語意：逐行回報螢幕可見文字，**每行去掉尾端空白**後回傳。
    // 尾端空格在終端畫面上沒有語意，而呼叫端（例如 pane_snapshot）要的正是
    // 「這行看起來有什麼」，因此 trim 是 Screen 的責任而非呼叫端。
    // 螢幕上的全形空白格是 wide spacer，不是文字，不會出現在輸出中。

    #[test]
    fn plain_lines_reports_fed_text_without_ansi_escapes() {
        let mut screen = Screen::new(20, 3);
        screen.feed(b"\x1b[1;32mgreen\x1b[0m plain");

        let lines = screen.plain_lines();
        assert_eq!(
            lines[0], "green plain",
            "ANSI 控制序列不應出現在純文字輸出，實際：{:?}",
            lines[0]
        );
        assert_eq!(
            lines.len(),
            3,
            "plain_lines 應回傳 rows 行，實際：{}",
            lines.len()
        );
    }

    #[test]
    fn plain_lines_trims_trailing_blanks() {
        let mut screen = Screen::new(20, 3);
        screen.feed(b"hi");

        assert_eq!(screen.plain_lines()[0], "hi", "行尾空格沒有語意，應被 trim");
    }

    #[test]
    fn plain_lines_trims_only_the_end_and_keeps_leading_blanks() {
        let mut screen = Screen::new(20, 3);
        // CUP 2;1H 移到第 2 行最左，再寫入兩個前導空格。
        // （若用 2;3H，畫面會保留前兩格空白，結果會是四個前導空格。）
        screen.feed(b"\x1b[2;1H  pad  ");

        assert_eq!(
            screen.plain_lines()[1],
            "  pad",
            "行首空格有意義應保留，行尾應被 trim，實際：{:?}",
            screen.plain_lines()[1]
        );
    }

    #[test]
    fn plain_lines_keeps_blanks_left_by_cursor_positioning() {
        // CUP 2;3H 跳過前兩格，那兩格是畫面上真實的空白，不該被當成「不存在」。
        let mut screen = Screen::new(20, 3);
        screen.feed(b"\x1b[2;3H  pad  ");

        assert_eq!(
            screen.plain_lines()[1],
            "    pad",
            "游標定位留下的前導空白也要保留，實際：{:?}",
            screen.plain_lines()[1]
        );
    }

    #[test]
    fn plain_lines_keeps_cjk_and_drops_wide_spacers() {
        let mut screen = Screen::new(20, 3);
        screen.feed("中文 寬字元".as_bytes());

        let lines = screen.plain_lines();
        assert_eq!(
            lines[0], "中文 寬字元",
            "寬字元不應多出一個空格（每個寬字元佔兩格），實際：{:?}",
            lines[0]
        );
    }

    #[test]
    fn plain_lines_preserves_combining_marks() {
        // 結合重音符號是零寬字元，存在於 cell 的 zero-width 欄位而非主字元。
        let mut screen = Screen::new(20, 3);
        screen.feed("e\u{0301}".as_bytes()); // e + COMBINING ACUTE ACCENT

        let lines = screen.plain_lines();
        assert_eq!(
            lines[0], "e\u{0301}",
            "合併音符必須保留（e + U+0301），實際：{:?}",
            lines[0]
        );
    }

    #[test]
    fn plain_lines_splits_on_rows() {
        let mut screen = Screen::new(10, 3);
        screen.feed(b"first\r\nsecond\r\nthird");

        let lines = screen.plain_lines();
        assert_eq!(lines[0], "first", "第 1 行");
        assert_eq!(lines[1], "second", "第 2 行");
        assert_eq!(lines[2], "third", "第 3 行");
    }

    #[test]
    fn plain_lines_reports_empty_rows_for_unused_lines() {
        // 欄數必須大於文字長度，否則文字會自動換行，測試會自行製造出預期外的內容。
        let mut screen = Screen::new(20, 4);
        screen.feed(b"only one line\r\n");

        let lines = screen.plain_lines();
        assert_eq!(lines.len(), 4, "行數應等於 rows");
        assert_eq!(lines[0], "only one line", "第 1 行");
        for (row, line) in lines.iter().enumerate().skip(1) {
            assert_eq!(
                line,
                "",
                "沒有內容的第 {} 行應為空字串，實際：{line:?}",
                row + 1
            );
        }
    }

    // --- ANSI snapshot 的語意等價（不固定 byte spelling） ---

    #[test]
    fn render_ansi_round_trips_plain_text() {
        let mut screen = Screen::new(20, 3);
        screen.feed(b"hello world");
        assert_render_round_trips(&screen, 20, 3, "純文字");
    }

    #[test]
    fn render_ansi_round_trips_cjk_wide_and_combining() {
        let mut screen = Screen::new(20, 3);
        screen.feed("中文 e\u{0301} ＡＢ".as_bytes());
        assert_render_round_trips(&screen, 20, 3, "CJK/寬字元/合併音符");
    }

    #[test]
    fn render_ansi_round_trips_sgr_colors_and_basic_styles() {
        // 前景色、背景色與基本樣式。只驗語意，不固定 SGR 碼。
        let mut screen = Screen::new(30, 4);
        screen.feed(
            b"\x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m \x1b[4munderline\x1b[0m \
              \x1b[7minverse\x1b[0m \x1b[2mdim\x1b[0m \x1b[9mstrike\x1b[0m \
              \x1b[38;2;10;20;30mfg-rgb\x1b[0m \x1b[48;2;40;50;60mbg-rgb\x1b[0m",
        );
        assert_render_round_trips(&screen, 30, 4, "SGR 顏色與基本樣式");
    }

    #[test]
    fn render_ansi_preserves_every_basic_style_flag() {
        // 六種基本樣式旗標都要能被 dump 與重播保留。這裡逐項斷言來源畫面的旗標，
        // 確保 round-trip 測試不會因為某一項從未被設定而假通過。
        let mut screen = Screen::new(60, 4);
        screen.feed(
            b"\x1b[1mb\x1b[0m\x1b[3mi\x1b[0m\x1b[4mu\x1b[0m\
              \x1b[7mv\x1b[0m\x1b[2md\x1b[0m\x1b[9ms",
        );

        let views = cell_views(term_of(&screen), 60, 4);
        let row = &views[0];
        assert_eq!(row[0].char, 'b', "col0 應為 bold");
        assert_eq!(row[1].char, 'i', "col1 應為 italic");
        assert_eq!(row[2].char, 'u', "col2 應為 underline");
        assert_eq!(row[3].char, 'v', "col3 應為 inverse");
        assert_eq!(row[4].char, 'd', "col4 應為 dim");
        assert_eq!(row[5].char, 's', "col5 應為 strikethrough");

        assert!(row[0].bold, "bold 旗標應被設定");
        assert!(row[1].italic, "italic 旗標應被設定");
        assert!(row[2].underline, "underline 旗標應被設定");
        assert!(row[3].inverse, "inverse 旗標應被設定");
        assert!(row[4].dim, "dim 旗標應被設定");
        assert!(
            row[5].strikethrough,
            "strikethrough 旗標應被設定（此測試不可漏掉 SGR 9）"
        );

        // 每種樣式都不該污染到下一格。
        assert!(!row[1].bold, "bold 不應外溢到下一格");
        assert!(!row[5].italic, "strikethrough 之外不應殘留其他樣式");

        assert_render_round_trips(&screen, 60, 4, "六種基本樣式旗標");
    }

    #[test]
    fn render_ansi_restores_default_attributes_after_reset() {
        let mut screen = Screen::new(20, 3);
        screen.feed(b"\x1b[1;31mstyled\x1b[0m unstyled");

        let views = cell_views(term_of(&screen), 20, 3);
        let styled = &views[0][0];
        assert_eq!(styled.char, 's', "第一格應是 'styled' 的 s");
        assert!(styled.bold, "第一格應為粗體");
        assert!(
            styled.fg.is_non_default(),
            "第一格應有非預設前景色，實際：{:?}",
            styled.fg
        );

        // "styled" 佔 0..5，" " 是第 6 格，"unstyled" 從第 7 格起。
        let space = &views[0][6];
        assert!(!space.bold, "reset 後應恢復非粗體，實際：{space:?}");
        assert!(
            !space.fg.is_non_default(),
            "reset 後前景色應回到預設，實際：{:?}",
            space.fg
        );

        let unstyled = &views[0][7];
        assert_eq!(unstyled.char, 'u', "第 7 格應是 'unstyled' 的 u");
        assert!(
            !unstyled.bold && !unstyled.fg.is_non_default(),
            "reset 後的文字不應沿用前一段的樣式，實際：{unstyled:?}"
        );
    }

    #[test]
    fn render_ansi_round_trips_reset_across_the_replay_boundary() {
        // 純樣式 snapshot 的重繪必須跨過「樣式結束處」恢復預設，否則下一段文字
        // 會被前一段的樣式污染。
        let mut screen = Screen::new(20, 3);
        screen.feed(b"\x1b[1;31mstyled\x1b[0m unstyled");

        let rendered = screen.render_ansi();
        let replay = reference(20, 3, &rendered);
        let replayed = cell_views(&replay, 20, 3);

        // 重播後，前段仍帶樣式，後段回到預設。
        assert!(replayed[0][0].bold, "重播後前段仍應為粗體");
        assert!(
            replayed[0][0].fg.is_non_default(),
            "重播後前段仍應有非預設前景色，實際：{:?}",
            replayed[0][0].fg
        );
        assert!(
            !replayed[0][6].bold && !replayed[0][6].fg.is_non_default(),
            "重播後樣式必須已被 reset：第 7 格應回到預設，實際：{:?}",
            replayed[0][6]
        );
        assert!(
            replayed[0][0].fg != replayed[0][6].fg,
            "重播後前段與 reset 後的格子前景色應不同（樣式確實有被 reset）"
        );
        assert!(
            !replayed[0][7].bold,
            "重播後後段不應沿用前段的粗體，實際：{:?}",
            replayed[0][7]
        );
        assert!(
            !replayed[0][7].fg.is_non_default(),
            "重播後後段前景色應回到預設，實際：{:?}",
            replayed[0][7].fg
        );
    }

    #[test]
    fn cell_view_distinguishes_named_indexed_and_truecolor() {
        // 索引色（SGR 31）與 truecolor（SGR 38;2;r;g;b）在 alacritty 內部是不同的
        // Color 變體；round-trip 驗證必須能把兩者都保留下來。
        let mut screen = Screen::new(20, 3);
        screen.feed(b"\x1b[31midx\x1b[0m \x1b[38;2;10;20;30mrgb");

        let views = cell_views(term_of(&screen), 20, 3);
        assert!(
            matches!(views[0][0].fg, ColorView::Indexed(_) | ColorView::Named(..)),
            "SGR 31 應落在 Indexed 或 Named，實際：{:?}",
            views[0][0].fg
        );
        assert_eq!(
            views[0][4].fg,
            ColorView::Rgb(10, 20, 30),
            "truecolor 前景色應精確還原為 Rgb(10,20,30)"
        );
    }

    #[test]
    fn render_ansi_round_trips_after_cursor_movement() {
        let mut screen = Screen::new(20, 4);
        screen.feed(b"top\r\n\x1b[3;5Hbottom");
        assert_render_round_trips(&screen, 20, 4, "游標移動後的畫面");
    }

    // --- 游標 ---

    #[test]
    fn render_ansi_round_trips_cursor_position_after_movement() {
        let mut screen = Screen::new(20, 4);
        screen.feed(b"abc\x1b[3;7H");

        let original_cursor = term_of(&screen).grid().cursor.point;
        let rendered = screen.render_ansi();

        let replay = reference(20, 4, &rendered);
        let replay_cursor = replay.grid().cursor.point;

        assert_eq!(
            replay_cursor, original_cursor,
            "render_ansi 必須還原游標位置：原 {original_cursor:?}，還原後 {replay_cursor:?}"
        );
    }

    #[test]
    fn render_ansi_round_trips_cursor_over_a_wide_char() {
        // 讓寬字元剛好填滿整列，游標會停在最後一個寬字元的 spacer 格上。
        // 先斷言這個前提確實成立，否則這個邊界等於沒測到。
        let mut screen = Screen::new(4, 3);
        screen.feed("中中".as_bytes());

        let original_cursor = term_of(&screen).grid().cursor.point;
        assert!(
            term_of(&screen).grid()[original_cursor]
                .flags
                .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER),
            "前提：寬字元剛好填滿整列時，原始游標應停在 spacer 格上，實際游標在 \
             {original_cursor:?}"
        );

        let rendered = screen.render_ansi();

        let replay = reference(4, 3, &rendered);
        let replay_cursor = replay.grid().cursor.point;

        assert_eq!(
            replay_cursor, original_cursor,
            "游標位於寬字元 spacer 時仍須正確往返：原 {original_cursor:?}，還原後 {replay_cursor:?}"
        );
    }

    #[test]
    fn render_ansi_round_trips_cursor_not_on_a_spacer() {
        // 反向情境：寬字元後還有空欄時，游標落在一般空格，不在 spacer 上。
        // 這個測試確保實作不會無條件把游標回退一格。
        let mut screen = Screen::new(20, 3);
        screen.feed("中中".as_bytes());

        let original_cursor = term_of(&screen).grid().cursor.point;
        assert!(
            !term_of(&screen).grid()[original_cursor]
                .flags
                .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER),
            "前提：欄數足夠時游標不應停在 spacer，實際游標在 {original_cursor:?}"
        );

        let replay = reference(20, 3, &screen.render_ansi());

        assert_eq!(
            replay.grid().cursor.point,
            original_cursor,
            "游標不在 spacer 上時應原樣往返，不得被回退"
        );
    }

    #[test]
    fn render_ansi_preserves_cursor_visibility() {
        // 隱藏游標（DECTCEM 复位）之後，畫面重繪不應讓游標又出現。
        let mut screen = Screen::new(20, 3);
        screen.feed(b"text\x1b[?25l");

        let rendered = screen.render_ansi();
        let replay = reference(20, 3, &rendered);

        assert_eq!(
            replay.renderable_content().cursor.shape,
            alacritty_terminal::vte::ansi::CursorShape::Hidden,
            "render_ansi 必須保留游標隱藏狀態"
        );
    }

    // --- 參考實作本身的健全性（H3 的前提） ---

    #[test]
    fn reference_term_can_be_built_without_test_modules() {
        // H3 的前提：Term + Processor 可以只用公開 API 建立，不依賴 test 模組。
        let term = reference(10, 3, b"h3");
        let views = cell_views(&term, 10, 3);
        let first_row: String = views[0].iter().map(|view| view.char).collect();
        assert_eq!(
            first_row.trim_end(),
            "h3",
            "只靠公開 API 建立的 Term 應能解析並寫入字元"
        );
    }

    #[test]
    fn reference_screen_uses_scrolling_history_zero() {
        // scrolling_history = 0 確保 display 就是可見 viewport。
        let term = reference(10, 3, b"");
        assert_eq!(
            term.grid().display_offset(),
            0,
            "scrolling_history = 0 時不應有 scrollback 偏移"
        );
    }
}
