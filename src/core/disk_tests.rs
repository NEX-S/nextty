use super::tests::{disk_core, parse_all, resize, select, selection_to_string, settle, terminal};
use super::*;
use crate::vt::Parser;
use std::io::Read;

const MIB: u64 = 1 << 20;

fn feed(core: &mut Core, parser: &mut Parser, bytes: &[u8]) {
    parse_all(core, parser, bytes);
    settle(core);
}

fn read(
    core: &mut Core,
    block: Option<u64>,
    cursor: Option<HistoryCursor>,
    query: Option<&str>,
) -> HistoryPage {
    core.read_history(block, cursor, query)
        .0
        .recv()
        .unwrap()
        .unwrap()
}

#[test]
fn history_acknowledgements_preserve_live_rows_while_paging_damages_the_view() {
    let mut core = disk_core(20, 4, 8 * MIB);
    let mut parser = Parser::new();
    let text = (0..600)
        .map(|line| format!("{line:03}\r\n"))
        .collect::<String>();
    parse_all(&mut core, &mut parser, text.as_bytes());
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
fn prompt_scopes_are_excluded_from_hot_cold_and_reflowed_selection() {
    let mut core = disk_core(20, 4, 8 * MIB);
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
        resize(&mut core, columns, lines);
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
            text(core.copy_selection().recv().unwrap().unwrap()),
            expected.strip_suffix('\n').unwrap()
        );
    }
}
#[test]
fn prompt_scope_tracks_secondary_right_prompt_padding_and_overwrites() {
    let (mut core, mut parser) = terminal(24, 3);
    parser.advance(&mut core, b"\x1b]133;A\x07 \x1b[31m> \x1b[0m\x1b]133;B\x07cmd\x1b[1;15H\x1b]133;P;1\x07right\x1b]133;P;0\x07\x1b[1;4Hecho");
    select(&mut core, (0, 0), (0, 23));
    assert_eq!(selection_to_string(&core).as_deref(), Some("echo"));
    assert!(core.row(0).cells[1].flags.contains(Flags::PROMPT));
    assert!(!core.row(0).cells[6].flags.contains(Flags::PROMPT));
    parser.advance(
        &mut core,
        b"\r\n\x1b]133;P;1\x07> \x1b]133;P;0\x07continued",
    );
    select(&mut core, (1, 0), (1, 23));
    assert_eq!(selection_to_string(&core).as_deref(), Some("continued"));
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
    let mut core = disk_core(20, 4, 8 * MIB);
    let mut parser = Parser::new();
    let source = (0..600)
        .map(|i| format!("\x1b[31m{i:04}中\u{301}\x1b[0m\r\n"))
        .collect::<String>();
    feed(&mut core, &mut parser, source.as_bytes());
    assert_eq!(core.history_size(), 597);
    core.scroll_display(Scroll::Top);
    settle(&mut core);
    assert_eq!(core.row(0).cells[0].c, '0');
    assert_eq!(core.row(0).cells[4].c, '中');
    assert_eq!(
        core.row(0).extra(4).unwrap().zerowidth.as_slice(),
        &['\u{301}']
    );
    assert_eq!(core.row(0).cells[0].fg, Color::Named(NamedColor::Red));
    let top = -(core.history_size() as i32);
    select(&mut core, (top, 0), (2, 19));
    let copy = core.copy_selection().recv().unwrap().unwrap();
    let expected = (0..600)
        .map(|i| format!("{i:04}中\u{301}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(text(copy), expected);
}
#[test]
fn simple_copy_preserves_cold_hot_and_cross_boundary_text() {
    let mut core = disk_core(20, 4, 8 * MIB);
    let mut parser = Parser::new();
    let source = (0..80)
        .map(|i| format!("line-{i:03}中\u{301}\r\n"))
        .collect::<String>();
    feed(&mut core, &mut parser, source.as_bytes());
    let base = core.history_size() as i32;
    for (start, end) in [(5, 5), (67, 72), (79, 79)] {
        select(&mut core, (start - base, 0), (end - base, 9));
        let copy = core.copy_selection().recv().unwrap().unwrap();
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
    let mut core = disk_core(20, 4, 8 * MIB);
    let mut parser = Parser::new();
    feed(
        &mut core,
        &mut parser,
        "normal-secret".repeat(100).as_bytes(),
    );
    assert!(core.history_size() > 8);
    feed(&mut core, &mut parser, b"\x1b[?1049h\x1b[3;1HALT_ONLY");
    for kind in [SelectionType::Semantic, SelectionType::Lines] {
        let point = Point { line: 2, column: 3 };
        core.selection_start(point, Side::Left, kind);
        core.selection_update(point, Side::Right);
        assert_eq!(
            text(core.copy_selection().recv().unwrap().unwrap()),
            "ALT_ONLY"
        );
    }
}
#[test]
fn alternate_resize_return_and_reset_use_the_latest_geometry() {
    let mut core = disk_core(20, 4, 8 * MIB);
    let mut parser = Parser::new();
    feed(&mut core, &mut parser, "main\r\n".repeat(30).as_bytes());
    feed(&mut core, &mut parser, b"\x1b[?1049h");
    resize(&mut core, 8, 2);
    resize(&mut core, 20, 4);
    parse_all(&mut core, &mut parser, b"\x1b[?1049lX\x1b[3J");
    feed(&mut core, &mut parser, b"\x1b[?1049h");
    resize(&mut core, 10, 6);
    feed(&mut core, &mut parser, b"\x1bcRESET");
    assert_eq!((core.columns(), core.screen_lines()), (10, 6));
    assert_eq!(core.history_size(), 0);
    assert_eq!(
        &core.row(0).cells[..5]
            .iter()
            .map(|cell| cell.c)
            .collect::<String>(),
        "RESET"
    );
}

#[test]
fn running_block_copy_is_empty_after_the_cursor_moves_above_its_output() {
    let mut core = disk_core(20, 4, 8 * MIB);
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
    let copy = core.copy_block(block.id).recv().unwrap().unwrap();
    assert!(!copy.incomplete);
    assert_eq!(text(copy), "");
}
#[test]
fn command_copy_survives_spilling_resize_and_reports_capacity_eviction() {
    let mut core = disk_core(16, 4, 4096);
    let mut parser = Parser::new();
    feed(
        &mut core,
        &mut parser,
        b"\x1b]133;A\x07$ \x1b]133;B\x07printf\r\n\x1b]133;C\x07",
    );
    let id = core.blocks.back().unwrap().id;
    let source = (0..1200)
        .map(|i| format!("line-{i:04}\r\n"))
        .collect::<String>();
    feed(&mut core, &mut parser, source.as_bytes());
    feed(&mut core, &mut parser, b"\x1b]133;D;7\x07");
    let copy = core.copy_block(id).recv().unwrap().unwrap();
    assert!(copy.incomplete);
    let before = text(copy);
    assert!(before.ends_with("line-1199\n"));
    resize(&mut core, 11, 4);
    settle(&mut core);
    let copy = core.copy_block(id).recv().unwrap().unwrap();
    assert!(copy.incomplete);
    let after = text(copy);
    assert!(after.ends_with("line-1199\n"));
}
#[test]
fn synchronized_parsing_obeys_disk_backpressure_without_presenting_partial_frames() {
    let mut core = disk_core(8, 3, 8 * MIB);
    let mut parser = Parser::new();
    let source = format!("\x1b[?2026h{}\x1b[?2026l", "x\r\n".repeat(20_000));
    let mut visible_commits = 0;
    for mut chunk in source.as_bytes().chunks(128) {
        while !chunk.is_empty() {
            let (taken, changed) = parser.advance(&mut core, chunk);
            visible_commits += usize::from(changed);
            chunk = &chunk[taken..];
            core.poll_history();
        }
    }
    settle(&mut core);
    assert_eq!(visible_commits, 1);
    assert_eq!(core.history_size(), 19998);
    assert!(!core.mode().contains(Mode::SYNC_UPDATE));
}

#[test]
fn history_and_command_reads_keep_main_screen_contents_while_alternate_is_active() {
    let mut core = disk_core(30, 4, 8 * MIB);
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
    let command = core.copy_block(block).recv().unwrap().unwrap();
    assert!(!command.incomplete);
    let command = text(command);
    assert_eq!(command.lines().count(), 700);
    assert!(command.starts_with("line-0000 中\n"));
    assert!(command.ends_with("line-0699 中\n"));
    let all = read(&mut core, None, None, None);
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
    let mut core = disk_core(64, 4, 8 * MIB);
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
    let first = read(&mut core, Some(block), None, None);
    assert_eq!(first.text.len(), 16 * 1024 - 1);
    assert!(!first.incomplete);
    assert_eq!(first.command.as_ref().unwrap().status, Some(7));
    assert!(first.has_more);
    let cursor = first.cursor;
    core.scroll_display(Scroll::Top);
    settle(&mut core);
    let offset = core.display_offset();
    let next = read(&mut core, Some(block), Some(cursor), None);
    assert!(next.text.starts_with("中😀boundary-proof\n"));
    assert_eq!(
        format!("{}{}", first.text, next.text),
        source.replace("\r\n", "\n")
    );
    assert_eq!(core.display_offset(), offset);
    let found = read(&mut core, Some(block), None, Some("x中😀boundary-proof"));
    assert_eq!(found.matched, Some(true));
    assert!(found.text.contains("x中😀boundary-proof\n"));
    assert!(found.text.contains("end-proof"));
    resize(&mut core, 80, 4);
    settle(&mut core);
    let error = core
        .read_history(Some(block), Some(cursor), None)
        .0
        .recv()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("stale"));
}

#[test]
fn history_queries_return_one_matching_excerpt_and_resume_after_its_context() {
    for prefix in ["prefix ", "中😀 "] {
        let mut core = disk_core(80, 4, 8 * MIB);
        let mut parser = Parser::new();
        feed(
            &mut core,
            &mut parser,
            b"\x1b]133;A\x07$ \x1b]133;B\x07build\r\n\x1b]133;C\x07",
        );
        let block = core.blocks.back().unwrap().id;
        let source = format!(
            "before\r\n{prefix}MATCH\r\nafter\r\n{}\r\nMATCH later\r\nlast\r\n",
            "z".repeat(20 * 1024)
        );
        feed(&mut core, &mut parser, source.as_bytes());
        feed(&mut core, &mut parser, b"\x1b]133;D;0\x07");
        let first = read(&mut core, Some(block), None, Some("MATCH"));
        assert_eq!(first.text, format!("before\n{prefix}MATCH\nafter\n"));
        assert_eq!(first.matched, Some(true));
        assert!(first.has_more);
        let second = read(&mut core, Some(block), Some(first.cursor), Some("MATCH"));
        assert_eq!(second.matched, Some(true));
        assert!(second.text.ends_with("MATCH later\nlast\n"));
        assert!(!second.text.contains(prefix));
        assert!(second.text.len() < 200);
        let absent = read(&mut core, Some(block), Some(second.cursor), Some("MATCH"));
        assert_eq!(absent.matched, Some(false));
        assert_eq!(absent.text, "");
        assert!(!absent.has_more);
    }
}

#[test]
fn history_eof_cursor_keeps_appends_newlines_and_unicode_soft_wraps() {
    let mut core = disk_core(8, 5, 8 * MIB);
    let mut parser = Parser::new();
    let mut cursor = None;
    let mut combined = String::new();
    for (input, expected) in [
        ("ab中", "ab中"),
        ("😀\u{301}", "😀\u{301}"),
        ("\r\nnext", "\nnext"),
        ("12中😀Z", "12中😀Z"),
    ] {
        feed(&mut core, &mut parser, input.as_bytes());
        let page = read(&mut core, None, cursor, None);
        assert_eq!(page.text, expected);
        assert!(!page.has_more);
        assert!(!page.incomplete);
        assert_eq!(page.matched, None);
        combined.push_str(&page.text);
        cursor = Some(page.cursor);
        let again = read(&mut core, None, cursor, None);
        assert_eq!(again.text, "");
        assert!(!again.has_more);
        assert_eq!(again.cursor, page.cursor);
    }
    assert_eq!(combined, "ab中😀\u{301}\nnext12中😀Z");
}

#[test]
fn command_eof_cursor_remains_usable_after_completion_without_a_newline() {
    let mut core = disk_core(40, 5, 8 * MIB);
    let mut parser = Parser::new();
    feed(
        &mut core,
        &mut parser,
        "\x1b]133;A\x07$ \x1b]133;B\x07run\r\n\x1b]133;C\x07中😀".as_bytes(),
    );
    let block = core.blocks.back().unwrap().id;
    let first = read(&mut core, Some(block), None, None);
    assert_eq!(first.text, "中😀");
    assert_eq!(first.cursor.offset, "中😀".len() as u64);
    assert!(!first.has_more);
    assert!(!first.command.unwrap().complete);
    feed(
        &mut core,
        &mut parser,
        b"\x1b]133;D;7\x07\r\n\x1b]133;A\x07$ ",
    );
    let finished = read(&mut core, Some(block), Some(first.cursor), None);
    assert_eq!(finished.text, "");
    assert_eq!(finished.cursor, first.cursor);
    assert!(!finished.has_more);
    let command = finished.command.unwrap();
    assert!(command.complete);
    assert_eq!(command.status, Some(7));
}

#[test]
fn cold_line_and_semantic_selection_expand_across_uncached_wrapped_rows() {
    let mut core = disk_core(12, 4, 16 << 20);
    let mut parser = Parser::new();
    let word = "中\u{301}a".repeat(100);
    parse_all(
        &mut core,
        &mut parser,
        format!("prefix\r\n{word} stop\r\nend").as_bytes(),
    );
    settle(&mut core);
    let line = -(core.history_size() as i32) + 5;
    for kind in [SelectionType::Semantic, SelectionType::Lines] {
        core.selection_start(Point { line, column: 3 }, Side::Left, kind);
        core.selection_update(Point { line, column: 3 }, Side::Right);
        let value = text(core.copy_selection().recv().unwrap().unwrap());
        assert_eq!(
            value,
            if kind == SelectionType::Semantic {
                word.clone()
            } else {
                format!("{word} stop")
            }
        );
    }
}

#[test]
fn semantic_selection_at_the_edge_of_the_cached_cold_rows_still_copies_the_whole_word() {
    let mut core = disk_core(12, 4, 16 << 20);
    let mut parser = Parser::new();
    let word = "abcdefghij".repeat(10_000);
    parse_all(&mut core, &mut parser, format!("{word}\r\nend").as_bytes());
    settle(&mut core);
    core.scroll_display(Scroll::Top);
    settle(&mut core);
    let mut line = -(core.history_size() as i32);
    while core.absolute_row(line + 1).is_some() {
        line += 1;
    }
    assert!(core.absolute_row(line).unwrap().wrapped);
    core.selection_start(
        Point { line, column: 3 },
        Side::Left,
        SelectionType::Semantic,
    );
    core.selection_update(Point { line, column: 3 }, Side::Right);
    assert!(core.selection_range().is_some());
    assert_eq!(text(core.copy_selection().recv().unwrap().unwrap()), word);
}

#[test]
fn alternate_clear_and_resize_keep_main_command_boundaries_and_output() {
    let mut core = disk_core(12, 4, 16 << 20);
    let mut parser = Parser::new();
    let output = "中\u{301} abc\r\n".repeat(70);
    parse_all(
        &mut core,
        &mut parser,
        format!("\x1b]133;A\x07$ \x1b]133;B\x07echo\r\n\x1b]133;C\x07{output}\x1b]133;D;3\x07")
            .as_bytes(),
    );
    settle(&mut core);
    let id = core.blocks.back().unwrap().id;
    let command_blocks = core.recent_command_blocks();
    core.scroll_display(Scroll::Top);
    assert_eq!(core.recent_command_blocks(), command_blocks);
    core.scroll_display(Scroll::Bottom);
    parser.advance(&mut core, b"\x1b[?1049hALT\x1b[3J");
    assert_eq!(core.recent_command_blocks(), command_blocks);
    resize(&mut core, 9, 4);
    settle(&mut core);
    assert_eq!(core.history_size(), 0);
    assert!(core.visible_blocks().is_empty());
    assert_eq!(core.recent_command_blocks().last().unwrap().status, Some(3));
    parser.advance(&mut core, b"\x1b[?1049l");
    settle(&mut core);
    assert_eq!(core.blocks.back().unwrap().status, Some(3));
    assert_eq!(
        text(core.copy_block(id).recv().unwrap().unwrap()),
        output.replace("\r\n", "\n")
    );
    assert!(core.prompt_jump(true));
    settle(&mut core);
    assert_eq!(core.display_offset(), core.history_size());
}
#[test]
fn osc_directory_decodes_percent_encoding_to_raw_path_bytes() {
    let mut core = disk_core(12, 4, 16 << 20);
    let mut parser = Parser::new();
    parser.advance(
        &mut core,
        b"\x1b]7;file://localhost/home/user/space%20%E4%B8%AD%FF\x07",
    );
    use std::os::unix::ffi::OsStrExt;
    assert_eq!(
        core.current_directory().unwrap().as_os_str().as_bytes(),
        b"/home/user/space \xe4\xb8\xad\xff"
    );
}
