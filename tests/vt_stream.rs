//! Screen contents, protocol replies and streaming contracts from xterm's control-sequence
//! reference and kitty's underlines protocol.
//! https://invisible-island.net/xterm/ctlseqs/ctlseqs.html
//! https://sw.kovidgoyal.net/kitty/underlines/

use nextty::core::{
    Cell, Core, Event, Extra, Flags, Mode, Point, Scroll, SelectionType, Side, WindowSize,
};
use nextty::history::HistoryBudget;
use nextty::vt::{Color, CursorShape, NamedColor, Parser, Rgb};

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

    fn with_history(columns: u16, lines: u16) -> Self {
        let mut terminal = Self::new(columns, lines);
        let directory = std::env::temp_dir().join(format!(
            "nextty-vt-stream-{}-{columns}x{lines}",
            std::process::id()
        ));
        terminal
            .core
            .enable_history(HistoryBudget::new(directory, 1 << 20).unwrap());
        terminal
    }

    fn write(&mut self, input: impl AsRef<[u8]>) -> bool {
        self.parser.advance(&mut self.core, input.as_ref()).1
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

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            rows: (0..self.core.screen_lines())
                .map(|y| {
                    let row = self.core.row(y);
                    RowSnapshot {
                        cells: row.cells.clone(),
                        wrapped: row.wrapped,
                        extras: (0..row.cells.len())
                            .map(|x| row.extra(x).cloned())
                            .collect(),
                    }
                })
                .collect(),
            cursor: self.core.cursor(),
            mode: self.core.mode(),
            history: self.core.history_size(),
        }
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

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    rows: Vec<RowSnapshot>,
    cursor: (Point, CursorShape),
    mode: Mode,
    history: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct RowSnapshot {
    cells: Vec<Cell>,
    wrapped: bool,
    extras: Vec<Option<Extra>>,
}

fn assert_all_splits(input: &[u8], check: impl Fn(&mut Terminal)) {
    let mut complete = Terminal::new(16, 3);
    complete.write(input);
    check(&mut complete);
    let expected = complete.snapshot();
    for split in 0..=input.len() {
        let mut terminal = Terminal::new(16, 3);
        terminal.write(&input[..split]);
        terminal.write(&input[split..]);
        check(&mut terminal);
        assert_eq!(terminal.snapshot(), expected, "split at byte {split}");
    }
    let mut bytewise = Terminal::new(16, 3);
    for byte in input {
        bytewise.write([*byte]);
    }
    check(&mut bytewise);
    assert_eq!(bytewise.snapshot(), expected, "one byte per advance");
}

fn assert_line(terminal: &Terminal, row: usize, expected: &str) {
    assert_eq!(
        terminal.line(row).trim_end_matches(' '),
        expected,
        "row {row}"
    );
}

#[test]
fn plain_text_runs_clear_both_wide_boundaries_and_old_combining_marks() {
    for (input, expected) in [
        (
            "ab界\u{301}cd\x1b[1;4HXYZ\x1b[1;1He\u{308}\x1b[1;1Hab   ",
            "ab   Z",
        ),
        ("abcd界\u{301}\x1b[1;2HXYZW", "aXYZW"),
    ] {
        assert_all_splits(input.as_bytes(), |terminal| {
            assert_line(terminal, 0, expected);
            assert!((0..16).all(|column| terminal.core.row(0).extra(column).is_none()));
        });
    }
}

#[test]
fn chinese_combining_csi_osc_and_replies_survive_every_split() {
    let input = concat!(
        "\x1b]2;中文标题\x1b\\",
        "\x1b[1;38;2;12;34;56mA中文e\u{301}\x1b[0m",
        "\x1b[2;3HZ\x1b[6n"
    );
    assert_all_splits(input.as_bytes(), |terminal| {
        assert_line(terminal, 0, "A中 文 e");
        assert_line(terminal, 1, "  Z");
        assert_line(terminal, 2, "");
        let row = terminal.core.row(0);
        assert!(row.cells[1].flags.contains(Flags::WIDE_CHAR));
        assert!(row.cells[2].flags.contains(Flags::WIDE_CHAR_SPACER));
        assert_eq!(row.extra(5).unwrap().zerowidth.as_slice(), &['\u{301}']);
        for cell in &row.cells[..6] {
            assert_eq!(
                cell.fg,
                Color::Spec(Rgb {
                    r: 12,
                    g: 34,
                    b: 56
                })
            );
            assert!(cell.flags.contains(Flags::BOLD));
        }
        assert_eq!(terminal.core.cursor().0, Point { line: 1, column: 3 });
        let events = terminal.core.take_events();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Event::Title(Some(title)) if title == "中文标题"));
        assert!(matches!(&events[1], Event::Write(bytes) if bytes == b"\x1b[2;4R"));
    });
}

#[test]
fn malformed_utf8_replaces_invalid_subparts_and_reprocesses_escape() {
    assert_all_splits(b"A\xe4\xb8\x1b[31mB\xc0\xafC\xf0\x9f\x1b[0mD", |terminal| {
        assert_line(terminal, 0, "A\u{fffd}B\u{fffd}\u{fffd}C\u{fffd}D");
        let row = terminal.core.row(0);
        assert_eq!(row.cells[2].fg, Color::Indexed(1));
        assert_eq!(row.cells[5].fg, Color::Indexed(1));
        assert_eq!(row.cells[7].fg, Color::Named(NamedColor::Foreground));
    });
}

#[test]
fn utf8_does_not_join_across_can_sub_or_del() {
    for (control, expected) in [
        (0x18, "A\u{fffd}\u{fffd}\u{fffd}B"),
        (0x1a, "A\u{fffd}\u{fffd}\u{fffd}\u{fffd}B"),
        (0x7f, "A\u{fffd}\u{fffd}\u{fffd}B"),
    ] {
        assert_all_splits(&[b'A', 0xe4, control, 0xb8, 0xad, b'B'], |terminal| {
            assert_line(terminal, 0, expected);
        });
    }
}

#[test]
fn c0_inside_csi_executes_without_losing_the_parameter() {
    assert_all_splits(b"abc\x1b[2\rCX\x1b[6n", |terminal| {
        assert_line(terminal, 0, "abX");
        assert_eq!(terminal.replies(), b"\x1b[1;4R");
    });
}

#[test]
fn can_sub_and_escape_cancel_the_current_sequence_and_recover() {
    assert_all_splits(
        b"abc\x1b[31\x18mX\x1b[32\x1amY\x1b[999\x1b[1GZ",
        |terminal| {
            // SUB retains the terminal's visible replacement-character behavior.
            assert_line(terminal, 0, "ZbcmX\u{fffd}mY");
            for cell in &terminal.core.row(0).cells[..8] {
                assert_eq!(cell.fg, Color::Named(NamedColor::Foreground));
            }
        },
    );
}

#[test]
fn overflowing_csi_is_ignored_as_a_whole_then_normal_parsing_resumes() {
    let input = format!(
        "A\x1b[9999999999999999999999999999CX\x1b[31;{}1mY\x1b[32mZ",
        "1;".repeat(64)
    );
    assert_all_splits(input.as_bytes(), |terminal| {
        assert_line(terminal, 0, "AXYZ");
        let row = terminal.core.row(0);
        assert_eq!(row.cells[1].fg, Color::Named(NamedColor::Foreground));
        assert_eq!(row.cells[2].fg, Color::Named(NamedColor::Foreground));
        assert!(!row.cells[2].flags.contains(Flags::BOLD));
        assert_eq!(row.cells[3].fg, Color::Indexed(2));
    });
}

#[test]
fn osc_accepts_bel_and_split_st_with_chinese_titles() {
    assert_all_splits(
        "\x1b]0;标题一\x07A\x1b]2;标题二\x1b\\B".as_bytes(),
        |terminal| {
            assert_line(terminal, 0, "AB");
            let titles: Vec<_> = terminal
                .core
                .take_events()
                .into_iter()
                .filter_map(|event| match event {
                    Event::Title(title) => title,
                    _ => None,
                })
                .collect();
            assert_eq!(titles, ["标题一", "标题二"]);
        },
    );
}

#[test]
fn unsupported_control_strings_do_not_print_their_payload_or_end_at_bel() {
    let input = concat!(
        "A\x1bP1;2q隐藏\x07payload\x1b\\B",
        "\x1b_隐藏\x07payload\x1b\\C",
        "\x1b^隐藏\x07payload\x1b\\D",
        "\x1bX隐藏\x07payload\x1b\\E"
    );
    assert_all_splits(input.as_bytes(), |terminal| {
        assert_line(terminal, 0, "ABCDE");
        assert_line(terminal, 1, "");
        assert!(terminal.core.take_events().is_empty());
    });
}

#[test]
fn colon_sgr_keeps_color_subparameters_and_underline_style_separate() {
    let input = concat!(
        "\x1b[38:2::1:2:3;48:2:0:4:5:6;4:3;58:2::7:8:9mX",
        "\x1b[4:0;59mY\x1b[;1mZ\x1b[mW"
    );
    assert_all_splits(input.as_bytes(), |terminal| {
        assert_line(terminal, 0, "XYZW");
        let row = terminal.core.row(0);
        assert_eq!(row.cells[0].fg, Color::Spec(Rgb { r: 1, g: 2, b: 3 }));
        assert_eq!(row.cells[0].bg, Color::Spec(Rgb { r: 4, g: 5, b: 6 }));
        assert_eq!(row.cells[0].flags & Flags::ALL_UNDERLINES, Flags::UNDERCURL);
        assert_eq!(
            row.extra(0).unwrap().underline_color,
            Some(Color::Spec(Rgb { r: 7, g: 8, b: 9 }))
        );
        assert!(!row.cells[1].flags.intersects(Flags::ALL_UNDERLINES));
        assert!(
            row.extra(1)
                .is_none_or(|extra| extra.underline_color.is_none())
        );
        assert_eq!(row.cells[2].fg, Color::Named(NamedColor::Foreground));
        assert_eq!(row.cells[2].bg, Color::Named(NamedColor::Background));
        assert_eq!(row.cells[2].flags, Flags::BOLD);
        assert_eq!(row.cells[3].flags, Flags::empty());
    });
}

#[test]
fn omitted_and_zero_csi_parameters_use_the_documented_defaults() {
    assert_all_splits(b"abc\x1b[;HX\x1b[0CY\x1b[0;0HZ\x1b[6n", |terminal| {
        assert_line(terminal, 0, "ZbY");
        assert_eq!(terminal.replies(), b"\x1b[1;2R");
    });
}

#[test]
fn synchronized_output_defers_presentation_until_the_entire_esu_arrives() {
    let end = b"\x1b[?2026l";
    for split in 0..=end.len() {
        let mut terminal = Terminal::new(16, 3);
        assert!(terminal.write(b"old"));
        terminal.write(b"\x1b[?2026h");
        assert!(terminal.core.sync_deadline().is_some());
        terminal.core.reset_damage();
        assert!(!terminal.write("\r\x1b[2K新".as_bytes()));
        assert_line(&terminal, 0, "新");
        assert!(terminal.core.mode().contains(Mode::SYNC_UPDATE));
        let changed = terminal.write(&end[..split]);
        if split < end.len() {
            assert!(!changed);
            assert_line(&terminal, 0, "新");
            assert!(terminal.write(&end[split..]));
        } else {
            assert!(changed);
        }
        assert_line(&terminal, 0, "新");
        assert!(terminal.core.sync_deadline().is_none());
        assert!(!terminal.core.finish_sync());
        terminal.write(b"!");
        assert_line(&terminal, 0, "新 !");
    }
}

#[test]
fn ris_ends_synchronized_output_and_the_next_frame_still_waits_for_esu() {
    assert_all_splits(
        b"old\x1b[?2026hbuffered\x1bcfresh\x1b[?2026$p",
        |terminal| {
            assert_line(terminal, 0, "fresh");
            assert!(terminal.core.sync_deadline().is_none());
            assert!(!terminal.core.mode().contains(Mode::SYNC_UPDATE));
            assert_eq!(terminal.replies(), b"\x1b[?2026;2$y");
            terminal.write(b"\x1b[?2026h\r\x1b[2Knext");
            assert_line(terminal, 0, "next");
            assert!(terminal.write(b"\x1b[?2026l"));
            assert_line(terminal, 0, "next");
        },
    );
}

#[test]
fn forced_sync_flush_preserves_partial_csi_and_is_idempotent() {
    let mut terminal = Terminal::new(16, 3);
    terminal.write(b"old\x1b[?2026h\r\x1b[2Knew\x1b[31");
    assert_line(&terminal, 0, "new");
    assert!(terminal.core.sync_deadline().is_some());
    assert!(terminal.core.finish_sync());
    assert_line(&terminal, 0, "new");
    assert!(terminal.core.sync_deadline().is_none());
    assert!(!terminal.core.finish_sync());
    terminal.write(b"mX");
    assert_line(&terminal, 0, "newX");
    assert_eq!(terminal.core.row(0).cells[3].fg, Color::Indexed(1));
}

#[test]
fn rep_repeats_unicode_and_dec_graphics_across_chunk_boundaries() {
    assert_all_splits(
        "中\x1b[2bé\x1b[0b\x1b(0q\x1b[2b\x1b(B".as_bytes(),
        |terminal| {
            assert_line(terminal, 0, "中 中 中 éé───");
        },
    );
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
fn history_scroll_and_selection_use_absolute_content_coordinates() {
    let mut terminal = Terminal::with_history(4, 3);
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
    let selection = terminal.core.selection_range();
    assert_eq!(
        selection.map(|range| (range.start, range.end)),
        Some((point(-1, 0), point(0, 3)))
    );
    terminal.core.scroll_display(Scroll::Bottom);
    assert_eq!(terminal.line(0), "1111");
    assert_eq!(terminal.core.selection_range(), selection);
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
        terminal
            .core
            .selection_range()
            .map(|range| (range.start, range.end)),
        Some((point(0, 0), point(1, 4)))
    );
    terminal.core.resize(window_size(10, 3));
    assert_eq!(terminal.line(0), "abcdefghij");
    assert_eq!(
        terminal
            .core
            .selection_range()
            .map(|range| (range.start, range.end)),
        Some((point(0, 0), point(0, 9)))
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
    terminal.write("\x1b]4;1;#123;2;rgb:0000/8000/ffff\x07\x1b]4;1;?;2;?\x1b\\");
    assert_eq!(
        terminal.replies(),
        b"\x1b]4;1;rgb:1010/2020/3030\x1b\\\x1b]4;2;rgb:0000/7f7f/ffff\x1b\\"
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
