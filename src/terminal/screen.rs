//! 契約 §7.2 的畫面重繪（Snapshot 的 ANSI 部分）與純文字輸出。
//!
//! 這是 RED：Screen 尚無實作，因此本檔以 compile-RED 為主。測試刻意**不寫死**
//! ANSI byte spelling（不固定哪個 SGR 碼代表粗體），而以「語意等價」驗證：
//! 把 `render_ansi` 的輸出餵回一個新 Screen，畫面必須與原 Screen 等價。
//!
//! Screen 是 `pub(crate)`，測試放在同一個 crate 內的 unit test，因此可以直接
//! 讀 Term 的 cell 做內部等價驗證 —— 不需要為測試擴張任何 API。

#![cfg(test)]

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

use crate::terminal::screen::Screen;

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
    let (mut term, mut processor) =
        (Term::new(config, &size, VoidListener), Processor::new());
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
                        wide_spacer: cell.flags.contains(
                            alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER,
                        ),
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
                        underline: cell.flags.contains(
                            alacritty_terminal::term::cell::Flags::UNDERLINE,
                        ),
                        inverse: cell.flags.contains(
                            alacritty_terminal::term::cell::Flags::INVERSE,
                        ),
                        dim: cell.flags.contains(
                            alacritty_terminal::term::cell::Flags::DIM,
                        ),
                        strikethrough: cell.flags.contains(
                            alacritty_terminal::term::cell::Flags::STRIKEOUT,
                        ),
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
        screen.plain_lines()[0], "unchanged",
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

    assert_eq!(
        screen.plain_lines()[0], "hi",
        "行尾空格沒有語意，應被 trim"
    );
}

#[test]
fn plain_lines_trims_only_the_end_and_keeps_leading_blanks() {
    let mut screen = Screen::new(20, 3);
    screen.feed(b"\x1b[2;3H  pad  ");

    assert_eq!(
        screen.plain_lines()[1], "  pad",
        "行首空格有意義應保留，行尾應被 trim，實際：{:?}",
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
    let mut screen = Screen::new(10, 4);
    screen.feed(b"only one line\r\n");

    let lines = screen.plain_lines();
    assert_eq!(lines.len(), 4, "行數應等於 rows");
    assert_eq!(lines[0], "only one line", "第 1 行");
    for row in 1..4 {
        assert_eq!(
            lines[row], "",
            "沒有內容的第 {} 行應為空字串，實際：{:?}",
            row + 1,
            lines[row]
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
          \x1b[7minverse\x1b[0m \x1b[2mdim\x1b[0m \
          \x1b[38;2;10;20;30mfg-rgb\x1b[0m \x1b[48;2;40;50;60mbg-rgb\x1b[0m",
    );
    assert_render_round_trips(&screen, 30, 4, "SGR 顏色與基本樣式");
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
    assert!(
        !space.bold,
        "reset 後應恢復非粗體，實際：{space:?}"
    );
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
    // 游標停在寬字元的 spacer 格上。spacer 不是可獨立定位的 cell，
    // 顯示上應落在該寬字元本身。
    let mut screen = Screen::new(20, 3);
    screen.feed("中中".as_bytes());

    let original_cursor = term_of(&screen).grid().cursor.point;
    let rendered = screen.render_ansi();

    let replay = reference(20, 3, &rendered);
    let replay_cursor = replay.grid().cursor.point;

    assert_eq!(
        replay_cursor, original_cursor,
        "游標位於寬字元 spacer 時仍須正確往返：原 {original_cursor:?}，還原後 {replay_cursor:?}"
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