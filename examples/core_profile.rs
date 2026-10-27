//! Isolate terminal-core storage and parsing from the UI and GPU driver.
use std::hint::black_box;
use std::mem::size_of;
use std::time::Instant;

use nextty::core::{Cell, Core, WindowSize};
use nextty::vt::Parser;

fn memory(stage: &str, term: Option<&Core>) {
    let memory = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap();
    let values: Vec<_> = memory
        .lines()
        .filter(|line| {
            line.starts_with("Rss:")
                || line.starts_with("Pss:")
                || line.starts_with("Private_Dirty:")
        })
        .collect();
    println!(
        "{stage}: {}; history_lines={}; malloc_live_bytes={}; malloc_free_arena_bytes={}",
        values.join("; "),
        term.map_or(0, Core::history_size),
        unsafe { libc::mallinfo2() }.uordblks,
        unsafe { libc::mallinfo2() }.fordblks,
    );
}

fn main() {
    println!(
        "Cell size={} bytes; grid=100x30; configured_history=10000",
        size_of::<Cell>()
    );
    memory("before_term", None);
    let mut term = Core::new(
        WindowSize {
            num_cols: 100,
            num_lines: 30,
            cell_width: 10,
            cell_height: 22,
        },
        0xd8dee9,
        0x14161b,
    );
    let mut parser = Parser::new();
    memory("empty_term", Some(&term));
    let line = "\x1b[32mterminal benchmark\x1b[0m 中文 UTF-8 λ → 0123456789\r\n\x1b[2K";
    let chunk = line.repeat(1024);
    let iterations = 64 * 1024 * 1024 / chunk.len();
    let start = Instant::now();
    for _ in 0..iterations {
        parser.advance(&mut term, black_box(chunk.as_bytes()));
    }
    let elapsed = start.elapsed();
    println!(
        "parser_bytes={}; elapsed_seconds={:.6}; mib_per_second={:.3}",
        iterations * chunk.len(),
        elapsed.as_secs_f64(),
        (iterations * chunk.len()) as f64 / 1048576. / elapsed.as_secs_f64()
    );
    assert_eq!(
        term.history_size(),
        10000,
        "workload must fill retained history"
    );
    memory("after_64mib", Some(&term));
    parser.advance(&mut term, b"\x1b[?1049h");
    memory("alternate_screen", Some(&term));
    parser.advance(&mut term, b"\x1b[?1049l\x1b[3J");
    memory("history_cleared", Some(&term));
    unsafe { libc::malloc_trim(0) };
    memory("history_cleared_after_malloc_trim", Some(&term));
    drop(term);
    memory("term_dropped", None);
}
