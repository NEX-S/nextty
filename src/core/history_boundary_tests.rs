use super::*;
use crate::history::{HistoryBudget, HistoryConfig};
use crate::vt::Parser;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(1);
fn core() -> Core {
    let root =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/workspace-history");
    std::fs::create_dir_all(&root).unwrap();
    let budget = HistoryBudget::new(
        root.join(format!(
            "boundary-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )),
        16 * 1024 * 1024,
    )
    .unwrap();
    let mut core = Core::new(
        WindowSize {
            num_cols: 12,
            num_lines: 4,
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
        let history = core.history.as_ref().unwrap();
        if history.spool.is_empty()
            && history.stored_end == history.hot_start
            && !core.history_loading()
        {
            return;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
fn copy(mut result: HistoryCopy) -> String {
    let mut value = String::new();
    result.file.read_to_string(&mut value).unwrap();
    value
}
#[test]
fn clear_saved_while_archive_queue_is_full_preserves_pending_resize_and_new_history() {
    let mut core = core();
    let mut parser = Parser::new();
    parser.advance(&mut core, "old\r\n".repeat(5000).as_bytes());
    assert!(!core.history.as_ref().unwrap().spool.is_empty());
    core.resize(WindowSize {
        num_cols: 20,
        ..core.size
    });
    core.clear_screen(ClearMode::Saved);
    settle(&mut core);
    assert_eq!(core.columns(), 20);
    assert_eq!(core.history_size(), 0);
    parser.advance(&mut core, "new\r\n".repeat(100).as_bytes());
    settle(&mut core);
    assert_eq!(core.history_size(), 100);
    core.scroll_display(Scroll::Top);
    settle(&mut core);
    assert_eq!(core.row(0).cells[0].c, 'o');
    assert_eq!(core.row(3).cells[0].c, 'n');
}
#[test]
fn cold_line_and_semantic_selection_expand_across_uncached_wrapped_rows() {
    let mut core = core();
    let mut parser = Parser::new();
    let word = "中\u{301}a".repeat(100);
    parser.advance(
        &mut core,
        format!("prefix\r\n{word} stop\r\nend").as_bytes(),
    );
    settle(&mut core);
    let line = -(core.history_size() as i32) + 5;
    for kind in [SelectionType::Semantic, SelectionType::Lines] {
        core.selection_start(Point { line, column: 3 }, Side::Left, kind);
        core.selection_update(Point { line, column: 3 }, Side::Right);
        let value = copy(core.copy_selection().unwrap().recv().unwrap().unwrap());
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
fn cold_object_selection_expands_quotes_outside_the_loaded_page() {
    let mut core = core();
    let mut parser = Parser::new();
    let object = format!("\"{} file.rs\"", "中\u{301}a".repeat(100));
    parser.advance(
        &mut core,
        format!("prefix\r\n{object} stop\r\nend").as_bytes(),
    );
    settle(&mut core);
    core.scroll_display(Scroll::Top);
    settle(&mut core);
    let line = -(core.history_size() as i32) + 3;
    core.selection_start(Point { line, column: 3 }, Side::Left, SelectionType::Object);
    assert_eq!(
        copy(core.copy_selection().unwrap().recv().unwrap().unwrap()),
        object
    );
}
#[test]
fn semantic_selection_at_the_bottom_of_a_loaded_cold_page_stops_at_the_cache_boundary() {
    let mut core = core();
    let mut parser = Parser::new();
    let word = "abcdefghij".repeat(100);
    parser.advance(&mut core, format!("{word}\r\nend").as_bytes());
    settle(&mut core);
    core.scroll_display(Scroll::Top);
    settle(&mut core);
    let line = -(core.history_size() as i32) + core.screen_lines() as i32 - 1;
    assert!(core.absolute_row(line).unwrap().wrapped);
    assert!(core.absolute_row(line + 1).is_none());
    core.selection_start(
        Point { line, column: 3 },
        Side::Left,
        SelectionType::Semantic,
    );
    core.selection_update(Point { line, column: 3 }, Side::Right);
    assert!(core.selection_range().is_some());
    assert_eq!(
        copy(core.copy_selection().unwrap().recv().unwrap().unwrap()),
        word
    );
}

#[test]
#[cfg(feature = "tui")]
fn alternate_clear_and_resize_keep_main_command_boundaries_and_output() {
    let mut core = core();
    let mut parser = Parser::new();
    let output = "中\u{301} abc\r\n".repeat(70);
    parser.advance(
        &mut core,
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
    core.resize(WindowSize {
        num_cols: 9,
        ..core.size
    });
    settle(&mut core);
    assert_eq!(core.history_size(), 0);
    assert!(core.visible_blocks().is_empty());
    assert_eq!(core.recent_command_blocks().last().unwrap().status, Some(3));
    parser.advance(&mut core, b"\x1b[?1049l");
    settle(&mut core);
    assert_eq!(core.blocks.back().unwrap().status, Some(3));
    assert_eq!(
        copy(core.copy_block(id).unwrap().recv().unwrap().unwrap()),
        output.replace("\r\n", "\n")
    );
    assert!(core.prompt_jump(true));
    settle(&mut core);
    assert_eq!(core.display_offset(), core.history_size());
}
#[test]
#[cfg(feature = "tui")]
fn osc_directory_validates_host_percent_encoding_and_preserves_raw_paths() {
    let mut core = core();
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
    for uri in [
        b"file://remote.invalid/path".as_slice(),
        b"file://localhost/bad%GG",
        b"file://localhost/bad%00",
    ] {
        let before = core.current_directory().unwrap().to_owned();
        let mut bytes = b"\x1b]7;".to_vec();
        bytes.extend_from_slice(uri);
        bytes.push(7);
        parser.advance(&mut core, &bytes);
        assert_eq!(core.current_directory(), Some(before.as_path()));
    }
}
