//! Run with `cargo run --release --example bench` on an otherwise idle machine.

use std::hint::black_box;
use std::time::{Duration, Instant};

use nextty::core::{Core, WindowSize};
use nextty::error::{Error, Result};
use nextty::terminal::{self, Event, Shell};
use nextty::vt::Parser;

fn size() -> WindowSize {
    WindowSize {
        num_cols: 100,
        num_lines: 30,
        cell_width: 10,
        cell_height: 22,
    }
}

fn parser() {
    // Exercise printable UTF-8, wide cells, SGR, cursor motion and scrolling.
    let line = "\x1b[32mterminal benchmark\x1b[0m 中文 UTF-8 λ → 0123456789\r\n\x1b[2K";
    let data = line.repeat(1024);
    let iterations = (64 * 1024 * 1024) / data.len();
    let bytes = iterations * data.len();
    let mut rates = Vec::new();
    for _ in 0..5 {
        let mut term = Core::new(size(), 0xd8dee9, 0x14161b);
        let mut parser = Parser::new();
        parser.advance(&mut term, data.as_bytes());
        let start = Instant::now();
        for _ in 0..iterations {
            parser.advance(&mut term, black_box(data.as_bytes()));
        }
        let elapsed = start.elapsed();
        black_box(term.row(0));
        rates.push(bytes as f64 / 1048576. / elapsed.as_secs_f64());
    }
    let mut ordered = rates.clone();
    ordered.sort_by(f64::total_cmp);
    println!("parser+grid only (no PTY, UI, font shaping or rendering)");
    println!("  workload: {bytes} bytes/run, 100x30 grid, default scrollback, 5 runs");
    println!("  MiB/s: {rates:?}; median {:.2}", ordered[2]);
}

fn wait_for(terminal: &terminal::Terminal, expected: char) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match terminal.events.try_recv() {
            Ok(Event::Wakeup) => {
                terminal.acknowledge_wakeup();
                if terminal.core.lock().unwrap().row(0).cells[0].c == expected {
                    return Ok(());
                }
            }
            Ok(Event::ChildExit(status)) => {
                return Err(Error::msg(format!("echo child exited: {status}")));
            }
            Ok(Event::Error(error)) => return Err(Error::context("echo PTY reader", error)),
            Ok(Event::Exit) => return Err(Error::msg("echo terminal exited before reply")),
            Ok(_) => (),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err(Error::msg("terminal event channel closed"));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => std::thread::yield_now(),
        }
        if Instant::now() >= deadline {
            return Err(Error::msg(format!(
                "timed out waiting for echo {expected:?}"
            )));
        }
    }
}

fn percentile(samples: &[f64], percent: f64) -> f64 {
    samples[(percent * samples.len() as f64).ceil() as usize - 1]
}

fn pty() -> Result<()> {
    let terminal = terminal::Terminal::new(
        Some(Shell::new(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "stty raw -echo; printf '\\033[H!'; exec /bin/cat".into(),
            ],
        )),
        None,
        size(),
        0xd8dee9,
        0x14161b,
    )
    .map_err(|error| Error::context("start raw echo PTY", error))?;
    wait_for(&terminal, '!')?;
    let mut samples = Vec::with_capacity(2000);
    for sample in 0..2100 {
        let expected = if sample % 2 == 0 { 'a' } else { 'b' };
        let start = Instant::now();
        terminal
            .write(vec![b'\r', expected as u8])
            .map_err(|error| Error::context("write PTY input", error))?;
        wait_for(&terminal, expected)?;
        if sample >= 100 {
            samples.push(start.elapsed().as_secs_f64() * 1e6);
        }
    }
    samples.sort_by(f64::total_cmp);
    println!("raw PTY echo to parsed grid (no GUI/display latency)");
    println!("  path: Terminal::write → PTY → raw /bin/cat → event loop → grid observation");
    println!("  observer: event polling + yield_now; 100 warmups, 2000 samples, two bytes/input");
    println!(
        "  microseconds: p50 {:.2}, p95 {:.2}, p99 {:.2}, min {:.2}, max {:.2}",
        percentile(&samples, 0.50),
        percentile(&samples, 0.95),
        percentile(&samples, 0.99),
        samples[0],
        samples[samples.len() - 1]
    );
    Ok(())
}

fn main() -> Result<()> {
    parser();
    pty()
}
