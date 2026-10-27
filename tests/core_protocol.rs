//! Behavioral contracts for the terminal replacement: screen contents and protocol replies.
//! Escape semantics follow https://invisible-island.net/xterm/ctlseqs/ctlseqs.html.
use nextty::core::{Cell, Core, Event, Flags, Point, Scroll, SelectionType, Side, WindowSize};
use nextty::vt::{Color, NamedColor, Parser, Rgb};

struct Terminal {
    core: Core,
    parser: Parser,
}

impl Terminal {
    fn new(columns: u16, lines: u16) -> Self {
        Self {
            core: Core::new(window_size(columns, lines), 0xd8dee9, 0x14161b),
            parser: Parser::new(),
        }
    }

    fn write(&mut self, bytes: impl AsRef<[u8]>) {
        self.parser.advance(&mut self.core, bytes.as_ref());
    }

    fn line(&self, row: usize) -> String {
        self.core.row(row).cells.iter().map(|cell| cell.c).collect()
    }

    fn replies(&mut self) -> Vec<u8> {
        self.core
            .take_events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Write(bytes) => Some(bytes),
                _ => None,
            })
            .flatten()
            .collect()
    }
}

fn window_size(columns: u16, lines: u16) -> WindowSize {
    WindowSize {
        num_cols: columns,
        num_lines: lines,
        cell_width: 10,
        cell_height: 22,
    }
}

fn point(line: i32, column: usize) -> Point {
    Point { line, column }
}

#[test]
fn utf8_and_escape_sequences_can_cross_every_byte_boundary() {
    let mut terminal = Terminal::new(10, 3);
    for byte in "\x1b[31mA中文e\u{301}".as_bytes() {
        terminal.write([*byte]);
    }
    let row = terminal.core.row(0);
    assert_eq!(row.cells[0].c, 'A');
    assert_eq!(row.cells[1].c, '中');
    assert_eq!(row.cells[3].c, '文');
    assert_eq!(row.cells[5].c, 'e');
    assert_eq!(row.extra(5).unwrap().zerowidth, ['\u{301}']);
    assert_eq!(terminal.core.cursor().0, point(0, 6));
}

#[test]
fn wrapping_waits_for_the_next_printable_and_carriage_return_cancels_it() {
    let mut terminal = Terminal::new(5, 2);
    terminal.write("abcde");
    assert_eq!(terminal.line(0), "abcde");
    assert_eq!(terminal.line(1), "     ");
    assert!(!terminal.core.row(0).wrapped);
    terminal.write("\rX");
    assert_eq!(terminal.line(0), "Xbcde");
    assert_eq!(terminal.line(1), "     ");
    terminal.write("\x1b[HabcdeF");
    assert_eq!(terminal.line(0), "abcde");
    assert_eq!(terminal.line(1), "F    ");
    assert!(terminal.core.row(0).wrapped);
}

#[test]
fn overwriting_half_a_wide_cell_and_erasing_combining_text_leaves_no_ghost() {
    let mut terminal = Terminal::new(8, 2);
    terminal.write("A中e\u{301}Z");
    terminal.write("\x1b[1;3Hx");
    assert_eq!(terminal.line(0), "A xeZ   ");
    assert_eq!(
        terminal.core.row(0).extra(3).unwrap().zerowidth,
        ['\u{301}']
    );
    terminal.write("\x1b[1;4H\x1b[X");
    assert_eq!(terminal.line(0), "A x Z   ");
    assert!(
        terminal
            .core
            .row(0)
            .extra(3)
            .is_none_or(|extra| extra.zerowidth.is_empty())
    );
    terminal.write("\x1b[1;2H中");
    assert_eq!(terminal.line(0), "A中  Z   ");
}

#[test]
fn insert_delete_and_erase_use_the_current_background() {
    let mut terminal = Terminal::new(8, 2);
    terminal.write("\x1b[48;2;1;2;3m\x1b[2Jabcdef\x1b[1;3H\x1b[2@");
    assert_eq!(terminal.line(0), "ab  cdef");
    terminal.write("\x1b[3P");
    assert_eq!(terminal.line(0), "abdef   ");
    terminal.write("\x1b[1;3H\x1b[2X");
    assert_eq!(terminal.line(0), "ab  f   ");
    for column in [2, 3, 5, 6, 7] {
        let bg = terminal.core.row(0).cells[column].bg;
        assert_eq!(terminal.core.color(bg), Rgb { r: 1, g: 2, b: 3 });
    }
}

#[test]
fn sgr_truecolor_indexed_background_and_individual_style_resets_are_independent() {
    let mut terminal = Terminal::new(8, 2);
    terminal.write("\x1b[1;3;4;38;2;10;20;30;48;5;196mX\x1b[22;23;24mY\x1b[0mZ");
    let row = terminal.core.row(0);
    assert_eq!(
        terminal.core.color(row.cells[0].fg),
        Rgb {
            r: 10,
            g: 20,
            b: 30
        }
    );
    assert_eq!(
        terminal.core.color(row.cells[0].bg),
        Rgb { r: 255, g: 0, b: 0 }
    );
    assert!(
        row.cells[0]
            .flags
            .contains(Flags::BOLD | Flags::ITALIC | Flags::UNDERLINE)
    );
    assert!(
        !row.cells[1]
            .flags
            .intersects(Flags::BOLD | Flags::ITALIC | Flags::ALL_UNDERLINES)
    );
    assert_eq!(row.cells[1].fg, row.cells[0].fg);
    assert_eq!(row.cells[1].bg, row.cells[0].bg);
    assert_eq!(row.cells[2].fg, Color::Named(NamedColor::Foreground));
    assert_eq!(row.cells[2].bg, Color::Named(NamedColor::Background));
}

#[test]
fn tab_stops_can_be_set_and_cleared_without_wrapping() {
    let mut terminal = Terminal::new(12, 2);
    terminal.write("a\tb");
    assert_eq!(terminal.line(0), "a       b   ");
    terminal.write("\x1b[3g\x1b[1;4H\x1bH\x1b[2;1Hx\tY");
    assert_eq!(terminal.line(1), "x  Y        ");
    terminal.write("\x1b[1;4H\x1b[g\x1b[2;1Hz\tQ");
    assert_eq!(terminal.line(1), "z  Y       Q");
    assert!(!terminal.core.row(1).wrapped);
}

#[test]
fn origin_coordinates_and_partial_scrolling_preserve_the_fixed_header() {
    let mut terminal = Terminal::new(5, 4);
    terminal.write("1\x1b[2;1H2\x1b[3;1H3\x1b[4;1H4");
    terminal.write("\x1b[2;4r\x1b[?6h\x1b[1;1HA\x1b[3;1HX\n");
    assert_eq!(terminal.line(0), "1    ");
    assert_eq!(terminal.line(1), "3    ");
    assert_eq!(terminal.line(2), "X    ");
    assert_eq!(terminal.line(3), "     ");
    assert_eq!(terminal.core.history_size(), 0);
    terminal.write("\x1b[?6l\x1b[r\x1b[4;1H\n");
    assert_eq!(terminal.core.history_size(), 1);
}

#[test]
fn history_scroll_and_selection_use_absolute_content_coordinates() {
    let mut terminal = Terminal::new(4, 3);
    terminal.write("0000\r\n1111\r\n2222\r\n3333");
    assert_eq!(terminal.core.history_size(), 1);
    terminal.core.scroll_display(Scroll::Top);
    assert_eq!(terminal.core.display_offset(), 1);
    assert_eq!(terminal.line(0), "0000");
    assert_eq!(terminal.line(1), "1111");
    terminal
        .core
        .selection_start(point(-1, 0), Side::Left, SelectionType::Simple);
    terminal.core.selection_update(point(0, 3), Side::Right);
    assert_eq!(
        terminal.core.selection_to_string().as_deref(),
        Some("0000\n1111")
    );
    terminal.core.scroll_display(Scroll::Bottom);
    assert_eq!(terminal.line(0), "1111");
    assert_eq!(
        terminal.core.selection_to_string().as_deref(),
        Some("0000\n1111")
    );
}

#[test]
fn widening_reflows_soft_lines_without_changing_selection_or_next_write() {
    let mut terminal = Terminal::new(5, 3);
    terminal.write("abcdefghij");
    terminal
        .core
        .selection_start(point(0, 0), Side::Left, SelectionType::Simple);
    terminal.core.selection_update(point(1, 4), Side::Right);
    assert_eq!(
        terminal.core.selection_to_string().as_deref(),
        Some("abcdefghij")
    );
    terminal.core.resize(window_size(10, 3));
    assert_eq!(terminal.line(0), "abcdefghij");
    assert_eq!(
        terminal.core.selection_to_string().as_deref(),
        Some("abcdefghij")
    );
    terminal.write("K");
    assert_eq!(terminal.line(0), "abcdefghij");
    assert_eq!(terminal.line(1), "K         ");
}

#[test]
fn narrowing_preserves_the_logical_cursor_position() {
    let mut terminal = Terminal::new(10, 3);
    terminal.write("abcdefgh");
    terminal.core.resize(window_size(4, 4));
    terminal.write("X");
    assert_eq!(terminal.line(0), "abcd");
    assert_eq!(terminal.line(1), "efgh");
    assert_eq!(terminal.line(2), "X   ");
}

#[test]
fn alternate_1049_restores_the_shell_and_reenters_a_clean_screen() {
    let mut terminal = Terminal::new(8, 3);
    terminal
        .write("main\x1b[?1049h\x1b[31;44;58:2::1:2:3m\x1b[Heditor界\u{301}\x1b(0\x1b7\x1b[?1049l");
    assert_eq!(terminal.line(0), "main    ");
    assert_eq!(terminal.core.cursor().0, point(0, 4));
    terminal.write("\x1b[?1049h");
    for y in 0..terminal.core.screen_lines() {
        let row = terminal.core.row(y);
        assert!(row.cells.iter().all(|cell| *cell == Cell::default()));
        assert!(!row.wrapped);
        assert!((0..row.cells.len()).all(|column| row.extra(column).is_none()));
    }
    assert_eq!(terminal.core.history_size(), 0);
    assert_eq!(terminal.core.cursor().0, point(0, 0));
    terminal.write("\x1b8q");
    assert_eq!(
        terminal.core.row(0).cells[0],
        Cell {
            c: 'q',
            ..Cell::default()
        }
    );
    assert!(terminal.core.row(0).extra(0).is_none());
}

#[test]
fn alternate_47_persists_but_1047_clears_on_exit_and_1048_only_saves_cursor() {
    let mut terminal = Terminal::new(8, 3);
    terminal.write("main\x1b[?47h\x1b[Hsaved\x1b[?47l");
    assert_eq!(terminal.line(0), "main    ");
    terminal.write("\x1b[?47h");
    assert_eq!(terminal.line(0), "saved   ");
    terminal.write("\x1b[?1047l\x1b[?47h");
    assert_eq!(terminal.line(0), "        ");
    terminal.write("\x1b[?47l\x1b[2;3H\x1b[?1048h\x1b[HXX\x1b[?1048lY");
    assert_eq!(terminal.line(0), "XXin    ");
    assert_eq!(terminal.line(1), "  Y     ");
}

#[test]
fn status_cursor_and_mode_queries_reply_with_current_state() {
    let mut terminal = Terminal::new(10, 4);
    terminal.write("\x1b[3;4H\x1b[5n\x1b[6n");
    assert_eq!(terminal.replies(), b"\x1b[0n\x1b[3;4R");
    terminal.write("\x1b[2;4r\x1b[?6h\x1b[1;2H\x1b[6n");
    assert_eq!(terminal.replies(), b"\x1b[1;2R");
    terminal.write("\x1b[?2004h\x1b[?2004$p\x1b[?2004l\x1b[?2004$p\x1b[?9999$p");
    assert_eq!(
        terminal.replies(),
        b"\x1b[?2004;1$y\x1b[?2004;2$y\x1b[?9999;0$y"
    );
}

#[test]
fn device_identity_and_window_queries_are_consistent_with_the_backend() {
    let mut terminal = Terminal::new(100, 30);
    terminal.write("\x1b[c\x1b[>c");
    assert_eq!(terminal.replies(), b"\x1b[?6c\x1b[>0;1;0c");
    terminal.write("\x1b[14t\x1b[18t");
    assert_eq!(terminal.replies(), b"\x1b[4;660;1000t\x1b[8;30;100t");
}

#[test]
fn dynamic_colors_roundtrip_and_reset_to_the_configured_defaults() {
    let mut terminal = Terminal::new(8, 3);
    terminal.write("\x1b]10;?\x07\x1b]11;?\x1b\\");
    assert_eq!(
        terminal.replies(),
        b"\x1b]10;rgb:d8d8/dede/e9e9\x07\x1b]11;rgb:1414/1616/1b1b\x1b\\"
    );
    terminal.write("\x1b]11;#010203\x07");
    assert_eq!(
        terminal.core.color(Color::Named(NamedColor::Background)),
        Rgb { r: 1, g: 2, b: 3 }
    );
    terminal.write("\x1b]111\x07");
    assert_eq!(
        terminal.core.color(Color::Named(NamedColor::Background)),
        Rgb {
            r: 20,
            g: 22,
            b: 27
        }
    );
}

#[test]
fn clipboard_and_title_events_decode_utf8_without_sending_clipboard_contents() {
    let mut terminal = Terminal::new(8, 3);
    terminal.write("\x1b]2;中文标题\x07\x1b]52;c;5Lit5paH\x07\x1b]52;p;cHJpbWFyeQ==\x1b\\");
    let events = terminal.core.take_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Title(Some(title)) if title == "中文标题"))
    );
    assert!(events.iter().any(
        |event| matches!(event, Event::ClipboardStore { primary: false, text } if text == "中文")
    ));
    assert!(events.iter().any(
        |event| matches!(event, Event::ClipboardStore { primary: true, text } if text == "primary")
    ));
    terminal.write("\x1b]52;c;?\x07");
    assert_eq!(terminal.replies(), b"\x1b]52;c;\x07");
}

#[test]
fn insert_delete_lines_and_dec_graphics_keep_a_tui_frame_intact() {
    let mut terminal = Terminal::new(8, 4);
    terminal.write("\x1b(0lqqk\x1b(B\x1b[2;1Hone\x1b[3;1Htwo\x1b[4;1Hthree");
    assert_eq!(terminal.line(0), "┌──┐    ");
    terminal.write("\x1b[2;4r\x1b[3;1H\x1b[L");
    assert_eq!(terminal.line(0), "┌──┐    ");
    assert_eq!(terminal.line(1), "one     ");
    assert_eq!(terminal.line(2), "        ");
    assert_eq!(terminal.line(3), "two     ");
    terminal.write("\x1b[M");
    assert_eq!(terminal.line(2), "two     ");
    assert_eq!(terminal.line(3), "        ");
    assert_eq!(terminal.core.history_size(), 0);
}
