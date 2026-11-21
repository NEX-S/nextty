//! Run with `cargo run --release --example bench` on an otherwise idle machine.

use std::hint::black_box;
use std::io::Result;
use std::time::{Duration, Instant};

use nextty::core::{Core, WindowSize};
use nextty::history::HistoryBudget;
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
    let rates: [f64; 5] = std::array::from_fn(|_| {
        let mut term = Core::new(size(), 0xd8dee9, 0x14161b);
        let mut parser = Parser::new();
        parser.advance(&mut term, data.as_bytes());
        let start = Instant::now();
        for _ in 0..iterations {
            parser.advance(&mut term, black_box(data.as_bytes()));
        }
        let elapsed = start.elapsed();
        black_box(term.row(0));
        bytes as f64 / 1048576. / elapsed.as_secs_f64()
    });
    println!("parser+grid only (no PTY, UI, font shaping or rendering)");
    println!("  workload: {bytes} bytes/run, 100x30 grid, no history, 5 runs");
    println!("  MiB/s: {rates:?}; median {:.2}", median(rates));
}

fn median(mut rates: [f64; 5]) -> f64 {
    rates.sort_unstable_by(f64::total_cmp);
    rates[2]
}

/// One iteration of the daemon's loop for a single pane: wait for what the terminal asks for.
fn drive(terminal: &mut terminal::Terminal, budget: &HistoryBudget, timeout: Duration) {
    let mut events = if terminal.reading() { libc::POLLIN } else { 0 };
    if terminal.writing() {
        events |= libc::POLLOUT;
    }
    let mut fds = [
        libc::pollfd {
            fd: if events == 0 { -1 } else { terminal.pty_fd() },
            events,
            revents: 0,
        },
        libc::pollfd {
            fd: terminal.pidfd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: budget.fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let timeout = terminal.deadline().map_or(timeout, |deadline| {
        timeout.min(deadline.saturating_duration_since(Instant::now()))
    });
    nextty::sys::poll(&mut fds, nextty::sys::timeout_millis(timeout)).unwrap();
    if fds[2].revents != 0 {
        // Clears the eventfd counter, as the daemon does before it steps any pane.
        let mut count = 0u64;
        unsafe { libc::read(fds[2].fd, (&raw mut count).cast(), 8) };
    }
    terminal.step(fds[0].revents, fds[1].revents).unwrap();
}

fn wait_for(terminal: &mut terminal::Terminal, budget: &HistoryBudget, expected: char) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while terminal.core.row(0).cells[0].c != expected {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for echo {expected:?}"
        );
        drive(terminal, budget, remaining);
        for event in terminal.events.drain(..) {
            assert!(
                !matches!(
                    event,
                    Event::Exit(_) | Event::Error(_) | Event::HistoryError(_)
                ),
                "{event:?}"
            );
        }
    }
}

fn percentile(samples: &[f64], percent: f64) -> f64 {
    samples[(percent * samples.len() as f64).ceil() as usize - 1]
}

fn pty() -> Result<()> {
    let budget = HistoryBudget::new(
        std::env::temp_dir().join(format!("nextty-bench-{}", std::process::id())),
        1 << 30,
    )?;
    let spawn = || {
        let shell = Shell::new(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "stty raw -echo; printf '\\033[H!'; exec /bin/cat".into(),
            ],
        );
        let (foreground, background) = (0xd8dee9, 0x14161b);
        let mut terminal = terminal::Terminal::new_with_history(
            Some(shell),
            None,
            size(),
            foreground,
            background,
            budget.clone(),
        )?;
        wait_for(&mut terminal, &budget, '!');
        Result::Ok(terminal)
    };

    let line = "0123456789".repeat(6) + "ab\r\n";
    let data = line.repeat(30 * 1024 * 1024 / line.len());
    let mut rates = [0.; 5];
    for rate in &mut rates {
        let mut terminal = spawn()?;
        let start = Instant::now();
        terminal.write(&data)?;
        terminal.write(b"\x1b[H~")?;
        wait_for(&mut terminal, &budget, '~');
        *rate = data.len() as f64 / 1048576. / start.elapsed().as_secs_f64();
    }
    println!("PTY cat to parsed grid with production disk history (no GUI/display latency)");
    println!(
        "  workload: {} bytes/run, 100x30 grid, 512 hot rows, 1 GiB budget, 5 runs",
        data.len()
    );
    println!("  MiB/s: {rates:?}; median {:.2}", median(rates));

    let mut terminal = spawn()?;
    let mut samples = Vec::with_capacity(2000);
    for sample in 0..2100 {
        let expected = if sample % 2 == 0 { 'a' } else { 'b' };
        let start = Instant::now();
        terminal.write([b'\r', expected as u8])?;
        wait_for(&mut terminal, &budget, expected);
        if sample >= 100 {
            samples.push(start.elapsed().as_secs_f64() * 1e6);
        }
    }
    samples.sort_unstable_by(f64::total_cmp);
    println!("raw PTY echo to parsed grid with production disk history (no GUI/display latency)");
    println!("  path: Terminal::write → PTY → raw /bin/cat → event loop → grid observation");
    println!("  observer: PTY-driven poll; 100 warmups, 2000 samples, two bytes/input");
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
