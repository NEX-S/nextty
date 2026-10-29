use super::*;
use crate::history::HistoryBudget;
use crate::vt::Parser;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(1);
fn budget(capacity: u64) -> std::sync::Arc<HistoryBudget> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/workspace-history");
    std::fs::create_dir_all(&root).unwrap();
    HistoryBudget::new(
        root.join(format!(
            "test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )),
        capacity,
    )
    .unwrap()
}
fn core(columns: u16, lines: u16, budget: std::sync::Arc<HistoryBudget>) -> Core {
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
    core.enable_history(HistoryConfig {
        budget,
        hot_rows: 8,
    })
    .unwrap();
    core
}
fn settle(core: &mut Core) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        core.poll_history(true).unwrap();
        let h = core.history.as_ref().unwrap();
        if h.spool.is_empty() && h.stored_end == h.hot_start && !core.history_loading() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "disk worker stalled");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
fn feed(core: &mut Core, parser: &mut Parser, bytes: &[u8]) {
    for chunk in bytes.chunks(128) {
        parser.advance_queued(core, chunk);
        while parser.has_queued() || !core.history_read_ready() {
            core.poll_history(true).unwrap();
            if core.history_read_ready() {
                parser.advance_queued(core, &[]);
            } else {
                std::thread::yield_now();
            }
        }
    }
    settle(core);
}

#[test]
fn history_acknowledgements_preserve_live_rows_while_paging_damages_the_view() {
    let mut core = core(20, 4, budget(8 * 1024 * 1024));
    let mut parser = Parser::new();
    let text = (0..100)
        .map(|line| format!("{line:03}\r\n"))
        .collect::<String>();
    parser.advance(&mut core, text.as_bytes());
    core.reset_damage();
    settle(&mut core);
    assert_eq!(core.dirty_rows().count(), 0);
    assert!(core.history_size() > 8);
    core.scroll_display(Scroll::Top);
    core.reset_damage();
    settle(&mut core);
    assert_eq!(core.dirty_rows().count(), 4);
    assert_eq!(
        core.row(0).cells[..3]
            .iter()
            .map(|cell| cell.c)
            .collect::<String>(),
        "000"
    );
    core.scroll_display(Scroll::Bottom);
    let visible = (0..4).map(|row| core.row(row).clone()).collect::<Vec<_>>();
    core.reset_damage();
    parser.advance(&mut core, b"\x1b[3J");
    assert_eq!(core.dirty_rows().count(), 0);
    settle(&mut core);
    assert_eq!(core.dirty_rows().count(), 0);
    assert_eq!(core.history_size(), 0);
    for (row, original) in visible.iter().enumerate() {
        assert_eq!(core.row(row).cells, original.cells);
    }
}
fn text(mut copy: HistoryCopy) -> String {
    let mut text = String::new();
    copy.file.read_to_string(&mut text).unwrap();
    text
}
#[test]
#[cfg(feature = "tui")]
fn prompt_scopes_are_excluded_from_hot_cold_and_reflowed_selection() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "target/runtime-blocks/prompt-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(root.parent().unwrap()).unwrap();
    let budget = HistoryBudget::new(root, 8 * 1024 * 1024).unwrap();
    let mut core = core(20, 4, budget);
    let mut parser = Parser::new();
    let mut expected = String::new();
    for i in 0..80 {
        let command = format!("cmd-{i:03}");
        let output = format!("result-{i:03} 中 •");
        feed(&mut core, &mut parser, format!(
                "\x1b]133;A\x07 • \x1b[0m\x1b]133;B\x07{command}\r\n\x1b]133;C\x07{output}\r\n\x1b]133;D;0\x07"
            ).as_bytes());
        expected.push_str(&format!("{command}\n{output}\n"));
    }
    for (columns, lines) in [
        (20, 4),
        (20, 3),
        (20, 6),
        (20, 40),
        (20, 4),
        (9, 4),
        (31, 4),
    ] {
        core.resize(WindowSize {
            num_cols: columns,
            num_lines: lines,
            ..core.size
        });
        settle(&mut core);
        core.selection_start(
            Point {
                line: -(core.history_size() as i32),
                column: 0,
            },
            Side::Left,
            SelectionType::Simple,
        );
        core.selection_update(core.cursor().0, Side::Left);
        assert_eq!(
            text(core.copy_selection().unwrap().recv().unwrap().unwrap()),
            expected.strip_suffix('\n').unwrap()
        );
    }
}
#[test]
#[cfg(feature = "tui")]
fn prompt_scope_tracks_secondary_right_prompt_padding_and_overwrites() {
    let mut core = Core::new(
        WindowSize {
            num_cols: 24,
            num_lines: 3,
            cell_width: 0,
            cell_height: 0,
        },
        0xcccccc,
        0x202020,
    );
    let mut parser = Parser::new();
    parser.advance(&mut core, b"\x1b]133;A\x07 \x1b[31m> \x1b[0m\x1b]133;B\x07cmd\x1b[1;15H\x1b]133;P;1\x07right\x1b]133;P;0\x07\x1b[1;4Hecho");
    core.selection_start(
        Point { line: 0, column: 0 },
        Side::Left,
        SelectionType::Simple,
    );
    core.selection_update(
        Point {
            line: 0,
            column: 23,
        },
        Side::Right,
    );
    assert_eq!(core.selection_to_string().as_deref(), Some("echo"));
    assert!(core.row(0).cells[1].flags.contains(Flags::PROMPT));
    assert!(!core.row(0).cells[6].flags.contains(Flags::PROMPT));
    parser.advance(
        &mut core,
        b"\r\n\x1b]133;P;1\x07> \x1b]133;P;0\x07continued",
    );
    core.selection_start(
        Point { line: 1, column: 0 },
        Side::Left,
        SelectionType::Simple,
    );
    core.selection_update(
        Point {
            line: 1,
            column: 23,
        },
        Side::Right,
    );
    assert_eq!(core.selection_to_string().as_deref(), Some("continued"));
    parser.advance(&mut core, b"\x1b[2K\rplain");
    assert!(
        core.row(1)
            .cells
            .iter()
            .all(|cell| !cell.flags.contains(Flags::PROMPT))
    );
    core.reset_damage();
    parser.advance(&mut core, b"\x1b[3;10H\x1b]133;P;2\x07");
    assert_eq!(core.dirty_rows().collect::<Vec<_>>(), [2]);
    assert!(
        core.row(2).cells[..9]
            .iter()
            .all(|cell| cell.flags.contains(Flags::PROMPT))
    );
    core.reset_damage();
    for marker in [b"A".as_slice(), b"B", b"C", b"D;1"] {
        parser.advance(&mut core, b"\x1b]133;");
        parser.advance(&mut core, marker);
        parser.advance(&mut core, b"\x07");
        assert_eq!(core.dirty_rows().count(), 0);
    }
    assert_eq!(core.visible_blocks().last().unwrap().status, Some(1));
}
#[test]
fn styled_history_pages_from_disk_and_cross_boundary_copy_is_exact() {
    let mut core = core(20, 4, budget(8 * 1024 * 1024));
    let mut parser = Parser::new();
    let source = (0..400)
        .map(|i| format!("\x1b[31m{i:04}中\u{301}\x1b[0m\r\n"))
        .collect::<String>();
    feed(&mut core, &mut parser, source.as_bytes());
    assert_eq!(core.history_size(), 397);
    assert!(core.active.rows.len() <= 12);
    core.scroll_display(Scroll::Top);
    settle(&mut core);
    assert_eq!(core.row(0).cells[0].c, '0');
    assert_eq!(core.row(0).cells[4].c, '中');
    assert_eq!(
        core.row(0).extra(4).unwrap().zerowidth.as_slice(),
        &['\u{301}']
    );
    assert_eq!(core.row(0).cells[0].fg, Color::Named(NamedColor::Red));
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
            line: 2,
            column: 19,
        },
        Side::Right,
    );
    let copy = core.copy_selection().unwrap().recv().unwrap().unwrap();
    let expected = (0..400)
        .map(|i| format!("{i:04}中\u{301}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(text(copy), expected);
}
#[test]
fn simple_copy_preserves_cold_hot_and_cross_boundary_text() {
    let mut core = core(20, 4, budget(8 * 1024 * 1024));
    let mut parser = Parser::new();
    let source = (0..80)
        .map(|i| format!("line-{i:03}中\u{301}\r\n"))
        .collect::<String>();
    feed(&mut core, &mut parser, source.as_bytes());
    let base = core.history_size() as i32;
    for (start, end) in [(5, 5), (67, 72), (79, 79)] {
        core.selection_start(
            Point {
                line: start - base,
                column: 0,
            },
            Side::Left,
            SelectionType::Simple,
        );
        core.selection_update(
            Point {
                line: end - base,
                column: 9,
            },
            Side::Right,
        );
        let copy = core.copy_selection().unwrap().recv().unwrap().unwrap();
        assert!(!copy.incomplete, "range {start}..={end}");
        assert_eq!(
            text(copy),
            (start..=end)
                .map(|i| format!("line-{i:03}中\u{301}"))
                .collect::<Vec<_>>()
                .join("\n"),
            "range {start}..={end}",
        );
    }
}
#[test]
fn alternate_expanded_copy_does_not_read_normal_history() {
    let mut core = core(20, 4, budget(8 * 1024 * 1024));
    let mut parser = Parser::new();
    feed(
        &mut core,
        &mut parser,
        "normal-secret".repeat(100).as_bytes(),
    );
    assert!(core.history_size() > 8);
    feed(&mut core, &mut parser, b"\x1b[?1049h\x1b[3;1HALT_ONLY");
    for kind in [
        SelectionType::Semantic,
        SelectionType::Lines,
        SelectionType::Object,
    ] {
        let point = Point { line: 2, column: 3 };
        core.selection_start(point, Side::Left, kind);
        core.selection_update(point, Side::Right);
        assert_eq!(
            text(core.copy_selection().unwrap().recv().unwrap().unwrap()),
            "ALT_ONLY"
        );
    }
}
#[test]
fn clearing_selection_during_history_resize_does_not_restore_it() {
    let mut core = core(17, 5, budget(8 * 1024 * 1024));
    feed(
        &mut core,
        &mut Parser::new(),
        "one two\r\n".repeat(32).as_bytes(),
    );
    core.selection_start(
        Point { line: 0, column: 0 },
        Side::Left,
        SelectionType::Simple,
    );
    core.selection_update(Point { line: 0, column: 2 }, Side::Right);
    core.resize(WindowSize {
        num_cols: 11,
        ..core.size
    });
    assert!(core.history.as_ref().unwrap().resizing.is_some());
    assert!(core.clear_selection());
    settle(&mut core);
    assert!(core.selection_range().is_none());
}

#[test]
fn disk_reflow_matches_the_existing_grid_including_a_long_soft_wrapped_cjk_line() {
    let mut disk = core(17, 5, budget(8 * 1024 * 1024));
    let mut plain = Core::new(disk.size, 0xcccccc, 0x202020);
    let mut first = Parser::new();
    let mut second = Parser::new();
    let source = format!(
        "abcdefghij\x1b[1;8Hh\u{301}\x1b[1;2Hb\u{308}\x1b[1;5H\x1b[58;5;42me\u{302}\x1b[59m\r\n{}\r\nlast",
        "a中\u{301}b  ".repeat(1300)
    );
    feed(&mut disk, &mut first, source.as_bytes());
    second.advance(&mut plain, source.as_bytes());
    for (width, height) in [
        (17, 3),
        (17, 6),
        (17, 40),
        (17, 5),
        (9, 5),
        (33, 5),
        (2, 5),
        (1, 5),
        (40, 5),
        (17, 5),
    ] {
        let size = WindowSize {
            num_cols: width,
            num_lines: height,
            ..disk.size
        };
        disk.resize(size);
        settle(&mut disk);
        plain.resize(size);
        assert_eq!(
            disk.history_size(),
            plain.history_size(),
            "size {width}x{height}"
        );
        assert_eq!(disk.cursor(), plain.cursor(), "size {width}x{height}");
        let total = disk.history_size() + disk.screen_lines();
        for start in (0..total).step_by(5) {
            let offset = disk.history_size().saturating_sub(start);
            disk.scroll_display(Scroll::Delta(offset as i32 - disk.display_offset() as i32));
            settle(&mut disk);
            plain.active.offset = disk.display_offset();
            for row in 0..disk.screen_lines() {
                assert_eq!(
                    disk.row(row).cells,
                    plain.row(row).cells,
                    "size {width}x{height}, source {start}, row {row}"
                );
                assert_eq!(disk.row(row).wrapped, plain.row(row).wrapped);
                assert_eq!(disk.row(row).extras(), plain.row(row).extras());
            }
        }
    }
}
#[test]
#[cfg(feature = "tui")]
fn running_block_copy_is_empty_after_the_cursor_moves_above_its_output() {
    let mut core = core(20, 4, budget(8 * 1024 * 1024));
    let mut parser = Parser::new();
    feed(
        &mut core,
        &mut parser,
        b"\x1b]133;A\x07$ \x1b]133;B\x07run\r\n\r\n\r\n\x1b]133;C\x07output\x1b[3A",
    );
    let block = core.blocks.back().unwrap();
    assert!(block.end.is_none());
    assert_eq!(block.output.unwrap().row, 3);
    assert_eq!(core.cursor().0.line, 0);
    let copy = core.copy_block(block.id).unwrap().recv().unwrap().unwrap();
    assert!(!copy.incomplete);
    assert_eq!(text(copy), "");
}
#[test]
#[cfg(feature = "tui")]
fn command_copy_survives_spilling_resize_and_reports_capacity_eviction() {
    let mut core = core(16, 4, budget(12_000));
    let mut parser = Parser::new();
    feed(
        &mut core,
        &mut parser,
        b"\x1b]133;A\x07$ \x1b]133;B\x07printf\r\n\x1b]133;C\x07",
    );
    let id = core.blocks.back().unwrap().id;
    let source = (0..300)
        .map(|i| format!("line-{i:04}\r\n"))
        .collect::<String>();
    feed(&mut core, &mut parser, source.as_bytes());
    feed(&mut core, &mut parser, b"\x1b]133;D;7\x07");
    assert!(!core.visible_blocks().last().unwrap().complete);
    let copy = core.copy_block(id).unwrap().recv().unwrap().unwrap();
    assert!(copy.incomplete);
    let before = text(copy);
    assert!(before.ends_with("line-0299\n"));
    core.resize(WindowSize {
        num_cols: 11,
        ..core.size
    });
    settle(&mut core);
    let copy = core.copy_block(id).unwrap().recv().unwrap().unwrap();
    assert!(copy.incomplete);
    let after = text(copy);
    assert!(after.ends_with("line-0299\n"));
}
#[test]
fn synchronized_replay_obeys_disk_backpressure_without_exposing_partial_commits() {
    let mut core = core(8, 3, budget(8 * 1024 * 1024));
    let mut parser = Parser::new();
    let source = format!("\x1b[?2026h{}\x1b[?2026l", "x\r\n".repeat(20_000));
    let mut visible_commits = 0;
    for chunk in source.as_bytes().chunks(128) {
        visible_commits += usize::from(parser.advance_queued(&mut core, chunk));
        while parser.has_queued() {
            core.poll_history(true).unwrap();
            if core.history_read_ready() {
                visible_commits += usize::from(parser.advance_queued(&mut core, &[]));
            } else {
                std::thread::yield_now();
            }
        }
        assert!(core.history.as_ref().unwrap().spool.len() <= 512);
    }
    settle(&mut core);
    assert_eq!(visible_commits, 1);
    assert_eq!(core.history_size(), 19998);
    assert!(!core.mode().contains(Mode::SYNC_UPDATE));
}

#[test]
fn history_and_command_reads_keep_main_screen_contents_while_alternate_is_active() {
    let mut core = core(30, 4, budget(8 * 1024 * 1024));
    let mut parser = Parser::new();
    feed(
        &mut core,
        &mut parser,
        b"\x1b]133;A\x07$ \x1b]133;B\x07build\r\n\x1b]133;C\x07",
    );
    let block = core.blocks.back().unwrap().id;
    let output = (0..700)
        .map(|line| format!("line-{line:04} 中\r\n"))
        .collect::<String>();
    feed(&mut core, &mut parser, output.as_bytes());
    feed(
        &mut core,
        &mut parser,
        b"\x1b]133;D;7\x07\x1b]133;A\x07$ \x1b]133;B\x07",
    );
    assert!(core.shell_prompt().is_some());
    feed(&mut core, &mut parser, b"\x1b[?1049hALT-ONLY");
    let cursor = core.cursor();
    let selection = core.selection_range();
    let command = core.copy_block(block).unwrap().recv().unwrap().unwrap();
    assert!(!command.incomplete);
    let command = text(command);
    assert_eq!(command.lines().count(), 700);
    assert!(command.starts_with("line-0000 中\n"));
    assert!(command.ends_with("line-0699 中\n"));
    let all = core
        .read_history(None, None, None)
        .unwrap()
        .0
        .recv()
        .unwrap()
        .unwrap();
    assert!(!all.incomplete);
    let all = all.text;
    assert!(all.contains("line-0000 中\n") && all.contains("line-0699 中\n"));
    assert!(!all.contains("ALT-ONLY"));
    assert_eq!(core.cursor(), cursor);
    assert_eq!(core.selection_range(), selection);
    assert!(core.mode().contains(Mode::ALT_SCREEN));
    assert!(core.shell_prompt().is_none());
}

#[test]
fn history_read_pages_utf8_searches_across_rows_and_rejects_stale_cursors() {
    let mut core = core(64, 4, budget(8 * 1024 * 1024));
    let mut parser = Parser::new();
    feed(
        &mut core,
        &mut parser,
        b"\x1b]133;A\x07$ \x1b]133;B\x07build\r\n\x1b]133;C\x07",
    );
    let block = core.blocks.back().unwrap().id;
    let source = format!(
        "{}中😀boundary-proof\r\nend-proof\r\n",
        "x".repeat(16 * 1024 - 1)
    );
    feed(&mut core, &mut parser, source.as_bytes());
    feed(
        &mut core,
        &mut parser,
        b"\x1b]133;D;7\x07\x1b]133;A\x07$ \x1b]133;B\x07",
    );
    let first = core
        .read_history(Some(block), None, None)
        .unwrap()
        .0
        .recv()
        .unwrap()
        .unwrap();
    assert_eq!(first.text.len(), 16 * 1024 - 1);
    assert!(!first.incomplete);
    assert_eq!(first.command.as_ref().unwrap().status, Some(7));
    let cursor = first.next.unwrap();
    core.scroll_display(Scroll::Top);
    settle(&mut core);
    let offset = core.display_offset();
    let next = core
        .read_history(Some(block), Some(cursor), None)
        .unwrap()
        .0
        .recv()
        .unwrap()
        .unwrap();
    assert!(next.text.starts_with("中😀boundary-proof\n"));
    assert_eq!(
        format!("{}{}", first.text, next.text),
        source.replace("\r\n", "\n")
    );
    assert_eq!(core.display_offset(), offset);
    let found = core
        .read_history(Some(block), None, Some("x中😀boundary-proof"))
        .unwrap()
        .0
        .recv()
        .unwrap()
        .unwrap();
    assert!(found.text.starts_with("x中😀boundary-proof\n"));
    assert!(found.text.contains("end-proof"));
    core.resize(WindowSize {
        num_cols: 80,
        ..core.size
    });
    settle(&mut core);
    let error = core
        .read_history(Some(block), Some(cursor), None)
        .unwrap_err();
    assert!(error.to_string().contains("stale"));
}

#[test]
fn history_query_cursor_skips_ascii_and_unicode_prefixes_without_repeating_results() {
    for prefix in ["prefix ", "中😀 "] {
        let mut core = core(80, 4, budget(8 * 1024 * 1024));
        let mut parser = Parser::new();
        feed(
            &mut core,
            &mut parser,
            b"\x1b]133;A\x07$ \x1b]133;B\x07build\r\n\x1b]133;C\x07",
        );
        let block = core.blocks.back().unwrap().id;
        let tail = format!("MATCH {}\r\nend-proof\r\n", "z".repeat(20 * 1024));
        feed(&mut core, &mut parser, format!("{prefix}{tail}").as_bytes());
        feed(&mut core, &mut parser, b"\x1b]133;D;0\x07");
        let mut page = core
            .read_history(Some(block), None, Some("MATCH"))
            .unwrap()
            .0
            .recv()
            .unwrap()
            .unwrap();
        assert_eq!(page.text.len(), 16 * 1024);
        let mut text = page.text;
        while let Some(cursor) = page.next {
            page = core
                .read_history(Some(block), Some(cursor), None)
                .unwrap()
                .0
                .recv()
                .unwrap()
                .unwrap();
            text.push_str(&page.text);
        }
        assert_eq!(text, tail.replace("\r\n", "\n"));
    }
}
