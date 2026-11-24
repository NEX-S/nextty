use super::*;
use crate::history::HistoryBudget;
use crate::vt::Parser;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

impl Core {
    /// Rows changed since `reset_damage`.
    pub(crate) fn dirty_rows(&self) -> impl Iterator<Item = usize> + '_ {
        let all = self.full_revision == self.revision;
        (0..self.screen_lines()).filter(move |&y| {
            !self.revision_pending && (all || self.row(y).revision == self.revision)
        })
    }

    pub(crate) fn palette_dirty(&self) -> bool {
        !self.revision_pending && self.palette_revision == self.revision
    }
}

pub(crate) fn selection_to_string(core: &Core) -> Option<String> {
    let range = core.selection_range()?;
    let mut text = String::new();
    for line in range.start.line..=range.end.line {
        let row = core.absolute_row(line)?;
        let start = if line == range.start.line {
            range.start.column
        } else {
            0
        };
        let end = if line == range.end.line {
            range.end.column + 1
        } else {
            core.columns()
        };
        text.extend(row.text(start, end, true));
        if line != range.end.line && !row.wrapped {
            text.push('\n');
        }
    }
    Some(text)
}

fn terminal_with_history(columns: u16, lines: u16) -> (Core, Parser) {
    (disk_core(columns, lines, 1 << 20), Parser::new())
}

pub(crate) fn terminal(columns: u16, lines: u16) -> (Core, Parser) {
    (
        Core::new(
            WindowSize {
                num_cols: columns,
                num_lines: lines,
                cell_width: 8,
                cell_height: 16,
            },
            0xd8dee9,
            0x14161b,
        ),
        Parser::new(),
    )
}

static NEXT_BUDGET: AtomicU64 = AtomicU64::new(1);

/// A core whose scrolled-out rows go through the real history writer.
pub(crate) fn disk_core(columns: u16, lines: u16, capacity: u64) -> Core {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/workspace-history");
    std::fs::create_dir_all(&root).unwrap();
    let budget = HistoryBudget::new(
        root.join(format!(
            "test-{}-{}",
            std::process::id(),
            NEXT_BUDGET.fetch_add(1, Ordering::Relaxed)
        )),
        capacity,
    )
    .unwrap();
    let mut core = Core::new(
        WindowSize {
            num_cols: columns,
            num_lines: lines,
            cell_width: 0,
            cell_height: 0,
        },
        0xcccccc,
        0x202020,
    );
    core.enable_history(budget);
    core
}

/// Parses all of `bytes`, waiting for the history writer whenever the parser stops for it.
pub(crate) fn parse_all(core: &mut Core, parser: &mut Parser, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        bytes = &bytes[parser.advance(core, bytes).0..];
        if !bytes.is_empty() {
            core.poll_history();
            std::thread::yield_now();
        }
    }
}

/// Returns once the writer has stored every row sent so far and the core has seen its replies.
pub(crate) fn settle(core: &mut Core) {
    let cursor = HistoryCursor {
        epoch: core.history.as_ref().unwrap().layout_epoch,
        row: core.block_origin + core.main_screen().cursor.point.line as u64,
        offset: 0,
    };
    let (receiver, _) = core.read_history(None, Some(cursor), None);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        core.poll_history();
        match receiver.try_recv() {
            Ok(page) => {
                page.unwrap();
                core.poll_history();
                return;
            }
            Err(mpsc::TryRecvError::Empty) => {
                assert!(Instant::now() < deadline, "history writer stalled");
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(mpsc::TryRecvError::Disconnected) => panic!("history request dropped"),
        }
    }
}

pub(crate) fn resize(core: &mut Core, columns: u16, lines: u16) {
    core.resize(WindowSize {
        num_cols: columns,
        num_lines: lines,
        ..core.size
    });
}

/// Selects the cells from `start` through `end`, both inclusive.
pub(crate) fn select(core: &mut Core, start: (i32, usize), end: (i32, usize)) {
    let point = |(line, column)| Point { line, column };
    core.selection_start(point(start), Side::Left, SelectionType::Simple);
    core.selection_update(point(end), Side::Right);
}

fn line(core: &Core, y: usize) -> String {
    core.row(y).cells.iter().map(|cell| cell.c).collect()
}
pub(crate) fn replies(core: &mut Core) -> Vec<u8> {
    core.take_events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Write(bytes) => Some(bytes),
            _ => None,
        })
        .flatten()
        .collect()
}
fn feed(core: &mut Core, processor: &mut Parser, text: &str) {
    processor.advance(core, text.as_bytes());
}

#[test]
fn bulk_writes_match_character_writes() {
    for columns in [1, 2, 7, 120] {
        for prefix in [
            "\x1b[?7h\x1b[4;58;5;2m",
            "\x1b[?7h\x1b[4;44m\x1b[1;2H",
            "\x1b[?7h\x1b[0m",
            "\x1b[?7h\x1b[0m\x1b]133;A\x07",
            "\x1b[?7l\x1b[0m",
        ] {
            let (mut bulk, mut bulk_parser) = terminal(columns, 3);
            let (mut scalar, mut scalar_parser) = terminal(columns, 3);
            for text in [
                "abcdef".repeat(40),
                "a\u{301}中b".repeat(40),
                "中文终端".repeat(40),
                "界\u{17d8}\u{301}🙂".repeat(40),
            ] {
                for (core, parser) in [
                    (&mut bulk, &mut bulk_parser),
                    (&mut scalar, &mut scalar_parser),
                ] {
                    feed(core, parser, "\x1b[H");
                    feed(core, parser, prefix);
                    core.reset_damage();
                }
                let snapshot = bulk.row(0).clone();
                let old_cells = snapshot.cells.clone();
                let old_extras = snapshot.extras().to_vec();
                bulk.input_text(&text);
                for ch in text.chars() {
                    scalar.input(ch);
                }
                assert_eq!(bulk.cursor(), scalar.cursor());
                assert_eq!(
                    bulk.active.cursor.wrap_pending,
                    scalar.active.cursor.wrap_pending
                );
                assert_eq!(bulk.history_size(), scalar.history_size());
                assert_eq!(
                    bulk.dirty_rows().collect::<Vec<_>>(),
                    scalar.dirty_rows().collect::<Vec<_>>()
                );
                for row in 0..3 {
                    let left = bulk.row(row);
                    let right = scalar.row(row);
                    assert_eq!(left.cells, right.cells);
                    assert_eq!(left.extras(), right.extras());
                    assert_eq!((left.wrapped, left.used()), (right.wrapped, right.used()));
                }
                assert_eq!(snapshot.cells, old_cells);
                assert_eq!(snapshot.extras(), old_extras);
            }
        }
    }
}

#[test]
fn row_snapshots_keep_extras_across_edits_clear_and_reflow() {
    for alternate in [false, true] {
        for edit in [
            "\x1b[1;2H\u{308}",
            "\x1b[H\x1b[@",
            "\x1b[H\x1b[P",
            "\x1b[1;2H\x1b[X",
            "\x1b[2J",
        ] {
            let (mut core, mut parser) = terminal(12, 2);
            if alternate {
                feed(&mut core, &mut parser, "\x1b[?1049h");
            }
            feed(
                &mut core,
                &mut parser,
                "\x1b[58;5;2m\x1b[4mA\u{301}中\u{308}z",
            );
            let snapshot = core.row(0).clone();
            let cells = snapshot.cells.clone();
            let extras = snapshot.extras().to_vec();
            feed(&mut core, &mut parser, edit);
            assert_eq!(snapshot.cells, cells);
            assert_eq!(snapshot.extras(), extras, "{alternate} {edit:?}");
            resize(&mut core, 2, 2);
            assert_eq!(snapshot.cells, cells);
            assert_eq!(snapshot.extras(), extras, "resize: {alternate} {edit:?}");
        }
    }
}

#[test]
fn pending_wrap_combining_and_wide_overwrite() {
    let (mut core, mut parser) = terminal_with_history(6, 3);
    feed(&mut core, &mut parser, "abcde界\u{301}");
    assert!(core.row(0).wrapped);
    assert_eq!(line(&core, 1), "界     ");
    assert_eq!(
        core.row(1).extra(0).unwrap().zerowidth.as_slice(),
        &['\u{301}']
    );
    feed(&mut core, &mut parser, "\x1b[2;2HX");
    assert_eq!(line(&core, 1), " X    ");
    assert!(core.row(1).extra(0).is_none());
    feed(&mut core, &mut parser, "\x1b[3;1H123456\u{308}");
    assert_eq!(core.cursor().0, Point { line: 2, column: 5 });
    assert_eq!(
        core.row(2).extra(5).unwrap().zerowidth.as_slice(),
        &['\u{308}']
    );
    assert!(!core.row(2).wrapped);
    feed(&mut core, &mut parser, "Z");
    assert_eq!(core.history_size(), 1);
    assert!(core.active.rows[2].wrapped);
    assert_eq!(line(&core, 2), "Z     ");
}

#[test]
fn region_scrolling_does_not_enter_history_and_uses_bce() {
    let (mut core, mut parser) = terminal(5, 4);
    feed(
        &mut core,
        &mut parser,
        "one\r\ntwo\r\nthree\r\nfour\x1b[2;3r\x1b[3;1H\x1b[44m",
    );
    core.reset_damage();
    feed(&mut core, &mut parser, "\n");
    assert_eq!(core.dirty_rows().collect::<Vec<_>>(), [1, 2]);
    assert_eq!(core.history_size(), 0);
    assert_eq!(line(&core, 0), "one  ");
    assert_eq!(line(&core, 1), "three");
    assert_eq!(line(&core, 2), "     ");
    assert_eq!(line(&core, 3), "four ");
    assert!(
        core.row(2)
            .cells
            .iter()
            .all(|cell| cell.bg == Color::Indexed(4))
    );
    feed(&mut core, &mut parser, "\x1b[2;2H\x1b[2X");
    assert_eq!(line(&core, 1), "t  ee");
}

#[test]
fn automatic_wrap_damages_the_previous_rows_metadata() {
    for (prefix, next) in [("abcd", "E"), ("abc", "界")] {
        let (mut core, mut parser) = terminal(4, 3);
        feed(&mut core, &mut parser, prefix);
        assert!(!core.row(0).wrapped);
        core.reset_damage();
        feed(&mut core, &mut parser, next);
        assert!(core.row(0).wrapped);
        assert_eq!(core.dirty_rows().collect::<Vec<_>>(), [0, 1]);
    }
}

#[test]
fn alternate_resize_truncates_without_reflowing() {
    let (mut core, mut parser) = terminal(8, 3);
    feed(&mut core, &mut parser, "main\x1b[?1049habcdefgh\r\nsecond");
    resize(&mut core, 4, 3);
    assert_eq!(line(&core, 0), "abcd");
    assert_eq!(line(&core, 1), "seco");
    assert_eq!(core.history_size(), 0);
    feed(&mut core, &mut parser, "\x1b[?1049lZ");
    core.selection_start(
        Point { line: 0, column: 0 },
        Side::Left,
        SelectionType::Simple,
    );
    core.selection_update(core.cursor().0, Side::Left);
    assert_eq!(selection_to_string(&core).as_deref(), Some("mainZ"));
}

#[test]
fn word_and_line_selection_follow_soft_wraps_and_wide_cells() {
    let (mut core, mut parser) = terminal(5, 4);
    feed(&mut core, &mut parser, "ab界cdef end");
    core.selection_start(
        Point { line: 0, column: 3 },
        Side::Left,
        SelectionType::Semantic,
    );
    assert_eq!(selection_to_string(&core).as_deref(), Some("ab界cdef"));
    core.selection_start(
        Point { line: 1, column: 1 },
        Side::Left,
        SelectionType::Lines,
    );
    assert_eq!(selection_to_string(&core).as_deref(), Some("ab界cdef end"));
    core.selection_start(
        Point { line: 0, column: 0 },
        Side::Right,
        SelectionType::Simple,
    );
    core.selection_update(Point { line: 0, column: 0 }, Side::Left);
    assert_eq!(selection_to_string(&core).as_deref(), Some("a"));
}

#[test]
fn top_anchored_scroll_region_preserves_history_and_fixed_footer() {
    for scroll in ["\n", "\x1b[S"] {
        let (mut core, mut parser) = terminal_with_history(5, 4);
        feed(
            &mut core,
            &mut parser,
            "one\r\ntwo\r\nthree\r\nfixed\x1b[1;3r\x1b[3;1H",
        );
        core.selection_start(
            Point { line: 3, column: 0 },
            Side::Left,
            SelectionType::Lines,
        );
        core.reset_damage();
        feed(&mut core, &mut parser, scroll);
        assert_eq!(core.dirty_rows().count(), core.screen_lines());
        assert_eq!(core.history_size(), 1);
        assert_eq!(line(&core, 0), "two  ");
        assert_eq!(line(&core, 1), "three");
        assert_eq!(line(&core, 2), "     ");
        assert_eq!(line(&core, 3), "fixed");
        assert_eq!(selection_to_string(&core).as_deref(), Some("fixed"));
        core.scroll_display(Scroll::Top);
        assert_eq!(line(&core, 0), "one  ");
    }
}

#[test]
fn clearing_a_shortened_row_also_clears_the_colored_wide_wrap_gap() {
    for erase in ["\x1b[2J", "\x1b[2K", "\x1b[K", "\x1b[1;7H\x1b[1K"] {
        let (mut core, mut parser) = terminal(7, 3);
        feed(
            &mut core,
            &mut parser,
            "abcdef\x1b[44m界\x1b[0m\x1b[1;1H\x1b[6X",
        );
        assert_eq!(core.row(0).used, 0);
        assert_eq!(core.row(0).cells[6].bg, Color::Indexed(4));
        feed(&mut core, &mut parser, erase);
        assert!(
            core.row(0)
                .cells
                .iter()
                .all(|cell| *cell == Cell::default())
        );
    }
}

#[test]
fn full_line_erase_clears_wide_extras_and_wrap_with_the_current_background() {
    for (sgr, bg) in [
        ("\x1b[0m", Color::Named(NamedColor::Background)),
        ("\x1b[0;44m", Color::Indexed(4)),
        ("\x1b[0;48;5;0m", Color::Indexed(0)),
        ("\x1b[0;48;2;1;2;3m", Color::Spec(Rgb { r: 1, g: 2, b: 3 })),
    ] {
        let (mut core, mut parser) = terminal(7, 3);
        feed(&mut core, &mut parser, "\x1b[4;58;5;2m界\u{301}abcdeX");
        assert!(core.row(0).wrapped);
        assert!(!core.row(0).extras().is_empty());
        feed(&mut core, &mut parser, "\x1b[1;4H");
        feed(&mut core, &mut parser, sgr);
        core.reset_damage();
        feed(&mut core, &mut parser, "\x1b[2K");
        let row = core.row(0);
        let blank = Cell {
            bg,
            ..Cell::default()
        };
        assert!(row.cells.iter().all(|cell| *cell == blank));
        assert!(row.extras().is_empty());
        assert!(!row.wrapped);
        assert_eq!(row.used, if bg == Cell::default().bg { 0 } else { 7 });
        assert_eq!(core.cursor().0, Point { line: 0, column: 3 });
        assert_eq!(core.dirty_rows().collect::<Vec<_>>(), vec![0]);
        assert_eq!(line(&core, 1), "X      ");
    }
    let (mut core, mut parser) = terminal(7, 3);
    feed(&mut core, &mut parser, "1234567\x1b[2KQ");
    assert_eq!(line(&core, 0), "      Q");
    assert_eq!(line(&core, 1), "       ");
}

#[test]
fn delete_char_moves_the_colored_wide_gap_into_the_clearable_prefix() {
    let (mut core, mut parser) = terminal(5, 3);
    feed(
        &mut core,
        &mut parser,
        "\x1b[41m\x1b[1;5H界\x1b[0m\x1b[1;1H\x1b[P",
    );
    assert_eq!(core.row(0).cells[3].bg, Color::Indexed(1));
    assert_eq!(core.row(0).used, 4);
    feed(&mut core, &mut parser, "\x1b[2J");
    assert!(
        core.row(0)
            .cells
            .iter()
            .all(|cell| *cell == Cell::default())
    );
}

#[test]
fn repeated_1049_enable_keeps_the_current_alternate_and_original_restore_point() {
    let (mut core, mut parser) = terminal(8, 3);
    feed(&mut core, &mut parser, "shell\x1b[?1049halt\x1b[?1049h");
    assert_eq!(line(&core, 0), "alt     ");
    assert_eq!(core.cursor().0, Point { line: 0, column: 3 });
    feed(&mut core, &mut parser, "\x1b[?1049l");
    assert_eq!(line(&core, 0), "shell   ");
    assert_eq!(core.cursor().0, Point { line: 0, column: 5 });
}
