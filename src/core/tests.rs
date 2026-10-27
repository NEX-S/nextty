use super::*;
use crate::vt::Parser;

fn terminal(columns: u16, lines: u16) -> (Core, Parser) {
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
fn line(core: &Core, y: usize) -> String {
    core.row(y).cells.iter().map(|cell| cell.c).collect()
}
fn feed(core: &mut Core, processor: &mut Parser, text: &str) {
    processor.advance(core, text.as_bytes());
}

#[test]
fn cells_are_sixteen_bytes_without_extra_allocation() {
    assert_eq!(std::mem::size_of::<Cell>(), 16);
    let (core, _) = terminal(80, 24);
    assert!(core.active.rows.iter().all(|row| row.extras().is_empty()));
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
            core.resize(WindowSize {
                num_cols: 2,
                num_lines: 2,
                cell_width: 8,
                cell_height: 16,
            });
            assert_eq!(snapshot.cells, cells);
            assert_eq!(snapshot.extras(), extras, "resize: {alternate} {edit:?}");
        }
    }
}

#[test]
fn row_parts_keep_unordered_extras_addressable_and_reject_duplicate_columns() {
    let accent = Extra {
        zerowidth: vec!['\u{301}'],
        underline_color: None,
    };
    let underline = Extra {
        zerowidth: Vec::new(),
        underline_color: Some(Color::Indexed(42)),
    };
    let row = Row::from_parts(
        vec![Cell::default(); 8],
        false,
        0,
        vec![(6, accent.clone()), (1, underline.clone())],
    )
    .unwrap();
    assert_eq!(row.extra(6), Some(&accent));
    assert_eq!(row.extra(1), Some(&underline));
    assert_eq!(row.extra(3), None);
    assert!(
        Row::from_parts(
            vec![Cell::default(); 8],
            false,
            0,
            vec![(6, accent.clone()), (1, underline), (6, accent)],
        )
        .is_none()
    );
}

#[test]
fn pending_wrap_combining_and_wide_overwrite() {
    let (mut core, mut parser) = terminal(6, 3);
    feed(&mut core, &mut parser, "abcde界\u{301}");
    assert!(core.row(0).wrapped);
    assert_eq!(line(&core, 1), "界     ");
    assert_eq!(core.row(1).extra(0).unwrap().zerowidth, vec!['\u{301}']);
    feed(&mut core, &mut parser, "\x1b[2;2HX");
    assert_eq!(line(&core, 1), " X    ");
    assert!(core.row(1).extra(0).is_none());
    feed(&mut core, &mut parser, "\x1b[3;1H123456\u{308}");
    assert_eq!(core.cursor().0, Point { line: 2, column: 5 });
    assert_eq!(core.row(2).extra(5).unwrap().zerowidth, vec!['\u{308}']);
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
            .all(|cell| cell.bg == Color::Named(NamedColor::Blue))
    );
    feed(&mut core, &mut parser, "\x1b[2;2H\x1b[2X");
    assert_eq!(line(&core, 1), "t  ee");
}

#[test]
fn alternate_screen_preserves_main_and_its_saved_cursor() {
    let (mut core, mut parser) = terminal(8, 3);
    feed(
        &mut core,
        &mut parser,
        "main\x1b7\x1b[2;3H\x1b[?1049halternate\x1b7\x1b[3;3H\x1b8\x1b[?1049l",
    );
    assert_eq!(line(&core, 0), "main    ");
    assert_eq!(core.cursor().0, Point { line: 1, column: 2 });
    assert!(!core.mode().contains(Mode::ALT_SCREEN));
    feed(&mut core, &mut parser, "\x1b[?47h");
    assert_eq!(line(&core, 0), "alternat");
    feed(&mut core, &mut parser, "\x1b[?47l");
    assert_eq!(line(&core, 0), "main    ");
}

#[test]
fn history_is_bounded_and_clear_saved_only_removes_history() {
    let (mut core, mut parser) = terminal(8, 2);
    for _ in 0..HISTORY_LIMIT + 50 {
        feed(&mut core, &mut parser, "line\r\n");
    }
    assert_eq!(core.history_size(), HISTORY_LIMIT);
    core.scroll_display(Scroll::Top);
    assert_eq!(core.display_offset(), HISTORY_LIMIT);
    core.reset_damage();
    feed(&mut core, &mut parser, "\x1b[3J");
    assert_eq!(core.dirty_rows().count(), 2);
    assert_eq!(core.history_size(), 0);
    assert_eq!(core.display_offset(), 0);
    assert_eq!(line(&core, 0), "line    ");
    core.reset_damage();
    feed(&mut core, &mut parser, "\x1b[3J");
    assert_eq!(core.dirty_rows().count(), 0);
}

#[test]
fn reflow_preserves_explicit_spaces_colors_wide_and_combining() {
    let (mut core, mut parser) = terminal(8, 4);
    feed(
        &mut core,
        &mut parser,
        "ab  界\u{301}cdef\r\n\x1b[41m  \x1b[0m",
    );
    core.resize(WindowSize {
        num_cols: 5,
        num_lines: 4,
        cell_width: 8,
        cell_height: 16,
    });
    core.selection_start(
        Point {
            line: -(core.history_size() as i32),
            column: 0,
        },
        Side::Left,
        SelectionType::Simple,
    );
    core.selection_update(
        Point {
            line: core.cursor().0.line,
            column: 2,
        },
        Side::Left,
    );
    let text = core.selection_to_string().unwrap();
    assert!(text.starts_with("ab  界\u{301}cdef\n"), "{text:?}");
    core.resize(WindowSize {
        num_cols: 10,
        num_lines: 4,
        cell_width: 8,
        cell_height: 16,
    });
    assert_eq!(line(&core, 0), "ab  界 cdef");
    assert_eq!(core.row(0).extra(4).unwrap().zerowidth, vec!['\u{301}']);
    assert_eq!(core.row(1).cells[0].bg, Color::Named(NamedColor::Red));
    assert_eq!(core.row(1).cells[1].bg, Color::Named(NamedColor::Red));
}

#[test]
fn reflow_moves_combining_and_underline_colors_after_out_of_order_edits() {
    let (mut core, mut parser) = terminal(6, 5);
    feed(
        &mut core,
        &mut parser,
        "\x1b[58;5;2ma\u{301}b\u{302}界\u{303}cd\u{304}e\u{305}f\u{306}\x1b[1;1H\x1b[58;5;3mA\u{307}",
    );
    for columns in [3, 1, 12] {
        core.resize(WindowSize {
            num_cols: columns,
            num_lines: 12,
            cell_width: 8,
            cell_height: 16,
        });
    }
    core.selection_start(
        Point { line: 0, column: 0 },
        Side::Left,
        SelectionType::Lines,
    );
    assert_eq!(
        core.selection_to_string().as_deref(),
        Some("A\u{307}b\u{302}界\u{303}cd\u{304}e\u{305}f\u{306}")
    );
    assert_eq!(
        core.row(0).extra(0).unwrap().underline_color,
        Some(Color::Indexed(3))
    );
    for column in 1..7 {
        assert_eq!(
            core.row(0).extra(column).unwrap().underline_color,
            Some(Color::Indexed(2))
        );
    }
}

#[test]
fn reflow_does_not_turn_selection_padding_into_written_spaces() {
    let (mut core, _) = terminal(9, 3);
    core.selection_start(
        Point { line: 0, column: 0 },
        Side::Left,
        SelectionType::Simple,
    );
    core.selection_update(Point { line: 2, column: 8 }, Side::Right);
    assert_eq!(core.selection_to_string().as_deref(), Some("\n\n"));
    core.resize(WindowSize {
        num_cols: 2,
        num_lines: 3,
        cell_width: 8,
        cell_height: 16,
    });
    assert!(core.active.rows.iter().all(|row| row.used == 0));
    assert!(!core.selection_to_string().unwrap().contains(' '));
}

#[test]
fn reflow_does_not_turn_a_saved_cursor_in_a_wide_wrap_gap_into_text() {
    let (mut core, mut parser) = terminal(2, 3);
    feed(&mut core, &mut parser, "a中\x1b[1;2H\x1b7\x1b[2;1H");
    assert!(core.row(0).wrapped);
    assert_eq!(core.row(0).used, 1);
    core.resize(WindowSize {
        num_cols: 3,
        num_lines: 3,
        cell_width: 8,
        cell_height: 16,
    });
    assert_eq!(line(&core, 0), "a中 ");
    core.selection_start(
        Point { line: 0, column: 0 },
        Side::Left,
        SelectionType::Lines,
    );
    assert_eq!(core.selection_to_string().as_deref(), Some("a中"));
}

#[test]
fn damage_tracks_content_changes_separately_from_display_metadata() {
    let (mut core, mut parser) = terminal(8, 3);
    core.reset_damage();
    feed(&mut core, &mut parser, "\x1b[3;1H");
    assert_eq!(core.dirty_rows().count(), 0);
    assert_eq!(core.cursor().0, Point { line: 2, column: 0 });
    core.reset_damage();
    core.selection_start(
        Point { line: 1, column: 0 },
        Side::Left,
        SelectionType::Lines,
    );
    assert_eq!(core.dirty_rows().count(), 0);
    core.reset_damage();
    assert!(!core.selection_update(Point { line: 1, column: 0 }, Side::Left));
    assert_eq!(core.dirty_rows().count(), 0);
    assert!(core.selection_update(Point { line: 1, column: 0 }, Side::Right));
    assert_eq!(core.dirty_rows().count(), 0);
    core.reset_damage();
    feed(&mut core, &mut parser, "\x1b]11;#123456\x07");
    assert_eq!(core.dirty_rows().count(), 3);
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
fn resize_preserves_insertion_after_a_full_line() {
    for columns in [3, 8] {
        let (mut core, mut parser) = terminal(6, 4);
        feed(&mut core, &mut parser, "abcdef");
        core.resize(WindowSize {
            num_cols: columns,
            num_lines: 4,
            cell_width: 8,
            cell_height: 16,
        });
        feed(&mut core, &mut parser, "Z");
        core.selection_start(
            Point { line: 0, column: 0 },
            Side::Left,
            SelectionType::Simple,
        );
        core.selection_update(core.cursor().0, Side::Left);
        assert_eq!(core.selection_to_string().as_deref(), Some("abcdefZ"));
    }
}

#[test]
fn alternate_resize_truncates_without_reflowing() {
    let (mut core, mut parser) = terminal(8, 3);
    feed(&mut core, &mut parser, "main\x1b[?1049habcdefgh\r\nsecond");
    core.resize(WindowSize {
        num_cols: 4,
        num_lines: 3,
        cell_width: 8,
        cell_height: 16,
    });
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
    assert_eq!(core.selection_to_string().as_deref(), Some("mainZ"));
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
    assert_eq!(core.selection_to_string().as_deref(), Some("ab界cdef"));
    core.selection_start(
        Point { line: 1, column: 1 },
        Side::Left,
        SelectionType::Lines,
    );
    assert_eq!(core.selection_to_string().as_deref(), Some("ab界cdef end"));
    core.selection_start(
        Point { line: 0, column: 0 },
        Side::Right,
        SelectionType::Simple,
    );
    core.selection_update(Point { line: 0, column: 0 }, Side::Left);
    assert_eq!(core.selection_to_string().as_deref(), Some("a"));
}

#[test]
fn object_selection_preserves_urls_locations_and_quoted_unicode() {
    for (text, column, expected) in [
        ("see (https://host/a_(b)?x=1)", 15, "https://host/a_(b)?x=1"),
        ("error: src/main.rs:42:7", 15, "src/main.rs:42:7"),
        (
            "open \"中\u{301}文 file.rs\" now",
            7,
            "\"中\u{301}文 file.rs\"",
        ),
    ] {
        let (mut core, mut parser) = terminal(40, 3);
        feed(&mut core, &mut parser, text);
        core.selection_start(Point { line: 0, column }, Side::Left, SelectionType::Object);
        assert_eq!(core.selection_to_string().as_deref(), Some(expected));
    }
    let (mut core, mut parser) = terminal(8, 5);
    feed(&mut core, &mut parser, "see \"中\u{301}文 file.rs\" now");
    core.selection_start(
        Point { line: 1, column: 2 },
        Side::Left,
        SelectionType::Object,
    );
    assert_eq!(
        core.selection_to_string().as_deref(),
        Some("\"中\u{301}文 file.rs\"")
    );
    let (mut core, mut parser) = terminal(100, 100);
    feed(&mut core, &mut parser, &"a".repeat(10_000));
    core.selection_start(
        Point {
            line: 50,
            column: 0,
        },
        Side::Left,
        SelectionType::Object,
    );
    assert_eq!(core.selection_to_string().as_deref(), Some("a"));
}

#[test]
fn height_resize_moves_view_border_without_rebuilding_rows() {
    let (mut core, mut parser) = terminal(5, 3);
    feed(&mut core, &mut parser, "one\r\ntwo\r\nthree\r\nfour");
    let first_allocation = core.active.rows[0].cells.as_ptr();
    core.selection_start(
        Point {
            line: -1,
            column: 0,
        },
        Side::Left,
        SelectionType::Simple,
    );
    core.selection_update(Point { line: 0, column: 3 }, Side::Left);
    core.resize(WindowSize {
        num_cols: 5,
        num_lines: 5,
        cell_width: 8,
        cell_height: 16,
    });
    assert_eq!(core.active.rows[0].cells.as_ptr(), first_allocation);
    assert_eq!(core.history_size(), 0);
    assert_eq!(core.cursor().0, Point { line: 3, column: 4 });
    assert_eq!(core.selection_to_string().as_deref(), Some("one\ntwo"));
    core.resize(WindowSize {
        num_cols: 5,
        num_lines: 2,
        cell_width: 8,
        cell_height: 16,
    });
    assert_eq!(core.active.rows[0].cells.as_ptr(), first_allocation);
    assert_eq!(core.history_size(), 2);
    assert_eq!(core.cursor().0, Point { line: 1, column: 4 });
    assert_eq!(line(&core, 0), "three");
    assert_eq!(line(&core, 1), "four ");
    assert_eq!(core.selection_to_string().as_deref(), Some("one\ntwo"));
}

#[test]
fn restoring_origin_cursor_clamps_to_the_current_margins_before_reporting() {
    let (mut core, mut parser) = terminal(8, 8);
    feed(
        &mut core,
        &mut parser,
        "\x1b[2;6r\x1b[?6h\x1b7\x1b[5;8r\x1b8\x1b[6n",
    );
    assert_eq!(core.cursor().0, Point { line: 4, column: 0 });
    let replies: Vec<_> = core
        .take_events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Write(bytes) => Some(bytes),
            _ => None,
        })
        .collect();
    assert_eq!(replies, [b"\x1b[1;1R"]);
    feed(
        &mut core,
        &mut parser,
        "\x1b[4;4H\x1b7\x1b[2;4r\x1b8\x1b[6n",
    );
    assert_eq!(core.cursor().0, Point { line: 3, column: 3 });
    let replies: Vec<_> = core
        .take_events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Write(bytes) => Some(bytes),
            _ => None,
        })
        .collect();
    assert_eq!(replies, [b"\x1b[3;4R"]);
}

#[test]
fn origin_mode_follows_the_restored_cursor_and_active_screen() {
    let (mut core, mut parser) = terminal(8, 5);
    feed(
        &mut core,
        &mut parser,
        "\x1b[2;4r\x1b[?6h\x1b7\x1b[?6l\x1b8\x1b[?6$p\x1b[6n",
    );
    assert!(core.mode().contains(Mode::ORIGIN));
    feed(&mut core, &mut parser, "\x1b[?47h\x1b[?6$p");
    assert!(!core.mode().contains(Mode::ORIGIN));
    feed(&mut core, &mut parser, "\x1b[?6h\x1b[?47l\x1b[?6$p");
    assert!(core.mode().contains(Mode::ORIGIN));
    feed(
        &mut core,
        &mut parser,
        "\x1b[?6l\x1b[?6$p\x1b[?47h\x1b[?6$p",
    );
    assert!(core.mode().contains(Mode::ORIGIN));
    let replies: Vec<_> = core
        .take_events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Write(bytes) => Some(bytes),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(
        replies,
        b"\x1b[?6;1$y\x1b[1;1R\x1b[?6;2$y\x1b[?6;1$y\x1b[?6;2$y\x1b[?6;1$y"
    );
}

#[test]
fn top_anchored_scroll_region_preserves_history_and_fixed_footer() {
    for scroll in ["\n", "\x1b[S"] {
        let (mut core, mut parser) = terminal(5, 4);
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
        assert_eq!(core.selection_to_string().as_deref(), Some("fixed"));
        core.scroll_display(Scroll::Top);
        assert_eq!(line(&core, 0), "one  ");
    }
}

#[test]
fn local_scrolling_damages_only_the_visible_region_and_preserves_fixed_rows() {
    for (offset, expected) in [(0, vec![1, 2]), (2, vec![3]), (6, vec![])] {
        for scroll in ["\x1b[S", "\x1b[T"] {
            let (mut core, mut parser) = terminal(10, 4);
            feed(
                &mut core,
                &mut parser,
                "0\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\r\n9\x1b[2;3r",
            );
            let history = core.history_size();
            core.scroll_display(Scroll::Delta(offset));
            core.reset_damage();
            feed(&mut core, &mut parser, scroll);
            assert_eq!(core.dirty_rows().collect::<Vec<_>>(), expected);
            assert_eq!(core.display_offset(), offset as usize);
            assert_eq!(core.history_size(), history);
            core.scroll_display(Scroll::Bottom);
            assert_eq!(line(&core, 0).trim_end(), "6");
            assert_eq!(line(&core, 3).trim_end(), "9");
        }
    }
}

#[test]
fn history_cleared_event_only_reports_a_real_main_history_clear() {
    let (mut core, mut parser) = terminal(5, 2);
    feed(
        &mut core,
        &mut parser,
        "one\r\ntwo\r\nthree\x1b[?1049h\x1b[3J\x1b[?1049l",
    );
    assert!(
        !core
            .take_events()
            .iter()
            .any(|event| matches!(event, Event::HistoryCleared))
    );
    feed(&mut core, &mut parser, "\x1b[3J");
    assert_eq!(
        core.take_events()
            .iter()
            .filter(|event| matches!(event, Event::HistoryCleared))
            .count(),
        1
    );
    feed(&mut core, &mut parser, "\x1b[3J");
    assert!(
        !core
            .take_events()
            .iter()
            .any(|event| matches!(event, Event::HistoryCleared))
    );
}

#[test]
fn partial_screen_erases_damage_only_visible_erased_rows_and_clear_pending_wrap() {
    for (offset, erase, expected) in [
        (0, "\x1b[J", vec![2, 3]),
        (0, "\x1b[1J", vec![0, 1, 2]),
        (2, "\x1b[J", vec![]),
        (2, "\x1b[1J", vec![2, 3]),
        (6, "\x1b[J", vec![]),
        (6, "\x1b[1J", vec![]),
    ] {
        let (mut core, mut parser) = terminal(10, 4);
        feed(
            &mut core,
            &mut parser,
            "0\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\r\n9\x1b[3;1Habcdefghij",
        );
        core.scroll_display(Scroll::Delta(offset));
        core.reset_damage();
        feed(&mut core, &mut parser, erase);
        assert_eq!(core.dirty_rows().collect::<Vec<_>>(), expected);
        feed(&mut core, &mut parser, "Z");
        assert_eq!(core.cursor().0.line, 2);
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
        assert_eq!(core.row(0).cells[6].bg, Color::Named(NamedColor::Blue));
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
        ("\x1b[0;44m", Color::Named(NamedColor::Blue)),
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
fn clear_resets_bce_blanks_inserted_beyond_the_text_or_deleted_at_the_tail() {
    for edit in ["\x1b[1;6H\x1b[2@", "\x1b[1;1H\x1b[2P"] {
        let (mut core, mut parser) = terminal(8, 3);
        feed(&mut core, &mut parser, "abc\x1b[44m");
        feed(&mut core, &mut parser, edit);
        assert!(
            core.row(0)
                .cells
                .iter()
                .any(|cell| cell.bg == Color::Named(NamedColor::Blue))
        );
        feed(&mut core, &mut parser, "\x1b[0m\x1b[2J");
        assert!(
            core.row(0)
                .cells
                .iter()
                .all(|cell| *cell == Cell::default())
        );
    }
}

#[test]
fn line_edit_repair_keeps_styled_blanks_before_the_cursor() {
    let (mut core, _) = terminal(80, 2);
    let row = core.screen_row_mut(0);
    row.cells[10].bg = Color::Indexed(1);
    row.used = 3;
    core.goto(0, 70);
    core.insert_blank(1);
    assert_eq!(core.row(0).used(), 11);
    assert_eq!(core.row(0).cells[10].bg, Color::Indexed(1));
}

#[test]
fn delete_char_moves_the_colored_wide_gap_into_the_clearable_prefix() {
    let (mut core, mut parser) = terminal(5, 3);
    feed(
        &mut core,
        &mut parser,
        "\x1b[41m\x1b[1;5H界\x1b[0m\x1b[1;1H\x1b[P",
    );
    assert_eq!(core.row(0).cells[3].bg, Color::Named(NamedColor::Red));
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

#[test]
fn disabling_1047_on_the_main_screen_clears_only_the_inactive_alternate() {
    let (mut core, mut parser) = terminal(8, 3);
    feed(
        &mut core,
        &mut parser,
        "shell\x1b[?47hkept\x1b[?47l\x1b[?1047l",
    );
    assert_eq!(line(&core, 0), "shell   ");
    feed(&mut core, &mut parser, "\x1b[?47h");
    assert_eq!(line(&core, 0), "        ");
    feed(&mut core, &mut parser, "\x1b[?47l");
    assert_eq!(line(&core, 0), "shell   ");
}

#[test]
fn alternate_roundtrip_restores_the_main_history_viewport_and_cursor() {
    let (mut core, mut parser) = terminal(8, 3);
    feed(
        &mut core,
        &mut parser,
        "one\r\ntwo\r\nthree\r\nfour\r\nfive",
    );
    let cursor = core.cursor().0;
    core.scroll_display(Scroll::Top);
    assert_eq!(core.display_offset(), 2);
    assert_eq!(line(&core, 0), "one     ");
    feed(&mut core, &mut parser, "\x1b[?1049ha\r\nb\r\nc\r\nd");
    assert_eq!(core.history_size(), 0);
    assert_eq!(core.display_offset(), 0);
    feed(&mut core, &mut parser, "\x1b[?1049l");
    assert_eq!(core.history_size(), 2);
    assert_eq!(core.display_offset(), 2);
    assert_eq!(line(&core, 0), "one     ");
    assert_eq!(core.cursor().0, cursor);
}
