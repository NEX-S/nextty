//! Streaming contracts from xterm's control-sequence reference and kitty's underlines protocol.
//! https://invisible-island.net/xterm/ctlseqs/ctlseqs.html
//! https://sw.kovidgoyal.net/kitty/underlines/

use nextty::core::{Cell, Core, Event, Extra, Flags, Mode, Point, WindowSize};
use nextty::vt::{Color, CursorShape, NamedColor, Parser, Rgb};

struct Terminal {
    core: Core,
    parser: Parser,
}

impl Terminal {
    fn new() -> Self {
        Self {
            core: Core::new(
                WindowSize {
                    num_cols: 16,
                    num_lines: 3,
                    cell_width: 10,
                    cell_height: 22,
                },
                0xd8dee9,
                0x14161b,
            ),
            parser: Parser::new(),
        }
    }

    fn write(&mut self, input: &[u8]) -> bool {
        self.parser.advance(&mut self.core, input)
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
    let mut complete = Terminal::new();
    complete.write(input);
    check(&mut complete);
    let expected = complete.snapshot();
    for split in 0..=input.len() {
        let mut terminal = Terminal::new();
        terminal.write(&input[..split]);
        terminal.write(&input[split..]);
        check(&mut terminal);
        assert_eq!(terminal.snapshot(), expected, "split at byte {split}");
    }
    let mut bytewise = Terminal::new();
    for byte in input {
        bytewise.write(&[*byte]);
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
        assert_eq!(row.extra(5).unwrap().zerowidth, ['\u{301}']);
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
        assert_eq!(row.cells[2].fg, Color::Named(NamedColor::Red));
        assert_eq!(row.cells[5].fg, Color::Named(NamedColor::Red));
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
        assert_eq!(row.cells[3].fg, Color::Named(NamedColor::Green));
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
fn synchronized_output_is_invisible_until_the_entire_esu_arrives() {
    let end = b"\x1b[?2026l";
    for split in 0..=end.len() {
        let mut terminal = Terminal::new();
        assert!(terminal.write(b"old"));
        terminal.write(b"\x1b[?2026h");
        assert!(terminal.parser.deadline().is_some());
        terminal.core.reset_damage();
        assert!(!terminal.write("\r\x1b[2K新".as_bytes()));
        assert_line(&terminal, 0, "old");
        assert!(terminal.core.dirty_rows().next().is_none());
        let changed = terminal.write(&end[..split]);
        if split < end.len() {
            assert!(!changed);
            assert_line(&terminal, 0, "old");
            assert!(terminal.write(&end[split..]));
        } else {
            assert!(changed);
        }
        assert_line(&terminal, 0, "新");
        assert!(terminal.parser.deadline().is_none());
        assert!(!terminal.parser.finish_sync(&mut terminal.core));
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
            assert!(terminal.parser.deadline().is_none());
            assert!(!terminal.core.mode().contains(Mode::SYNC_UPDATE));
            assert_eq!(terminal.replies(), b"\x1b[?2026;2$y");
            terminal.write(b"\x1b[?2026h\r\x1b[2Knext");
            assert_line(terminal, 0, "fresh");
            assert!(terminal.write(b"\x1b[?2026l"));
            assert_line(terminal, 0, "next");
        },
    );
}

#[test]
fn forced_sync_flush_preserves_partial_csi_and_is_idempotent() {
    let mut terminal = Terminal::new();
    terminal.write(b"old\x1b[?2026h\r\x1b[2Knew\x1b[31");
    assert_line(&terminal, 0, "old");
    assert!(terminal.parser.deadline().is_some());
    assert!(terminal.parser.finish_sync(&mut terminal.core));
    assert_line(&terminal, 0, "new");
    assert!(terminal.parser.deadline().is_none());
    assert!(!terminal.parser.finish_sync(&mut terminal.core));
    terminal.write(b"mX");
    assert_line(&terminal, 0, "newX");
    assert_eq!(
        terminal.core.row(0).cells[3].fg,
        Color::Named(NamedColor::Red)
    );
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
