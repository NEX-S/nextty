//! Run with `cargo run --release --example bench` on an otherwise idle machine.

use std::hint::black_box;
use std::io::Result;
use std::time::{Duration, Instant};

use nextty::core::{Core, Event, WindowSize};
use nextty::history::HistoryBudget;
use nextty::terminal::{Shell, Terminal};
use nextty::vt::Parser;

const SIZE: WindowSize = WindowSize {
    num_cols: 100,
    num_lines: 30,
    cell_width: 10,
    cell_height: 22,
};
const COLORS: (u32, u32) = (0xd8dee9, 0x14161b);

fn median(mut rates: [f64; 5]) -> f64 {
    rates.sort_unstable_by(f64::total_cmp);
    rates[2]
}

fn parser() {
    // Exercise printable UTF-8, wide cells, SGR, cursor motion and scrolling.
    let line = "\x1b[32mterminal benchmark\x1b[0m 中文 UTF-8 λ → 0123456789\r\n\x1b[2K";
    let data = line.repeat(1024);
    let iterations = (64 << 20) / data.len();
    let rates = std::array::from_fn(|_| {
        let mut core = Core::new(SIZE, COLORS.0, COLORS.1);
        let mut parser = Parser::new();
        parser.advance(&mut core, data.as_bytes());
        let start = Instant::now();
        for _ in 0..iterations {
            parser.advance(&mut core, black_box(data.as_bytes()));
        }
        black_box(core.row(0));
        (iterations * data.len()) as f64 / 1048576. / start.elapsed().as_secs_f64()
    });
    println!(
        "parser+grid, no PTY or history, 64 MiB: median {:.2} MiB/s of {rates:.1?}",
        median(rates)
    );
}

/// One iteration of the daemon's loop for a single pane: wait for what the terminal asks for.
fn drive(terminal: &mut Terminal, budget: &HistoryBudget, timeout: Duration) {
    let watch = |fd, events| libc::pollfd {
        fd: if events == 0 { -1 } else { fd },
        events,
        revents: 0,
    };
    let events = if terminal.reading() { libc::POLLIN } else { 0 }
        | if terminal.writing() { libc::POLLOUT } else { 0 };
    let mut fds = [
        watch(terminal.pty_fd(), events),
        watch(terminal.pidfd(), libc::POLLIN),
        watch(budget.fd(), libc::POLLIN),
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

fn wait_for(terminal: &mut Terminal, budget: &HistoryBudget, expected: char) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while terminal.core.row(0).cells[0].c != expected {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for {expected:?}");
        drive(terminal, budget, remaining);
        for event in terminal.core.take_events() {
            let failed = matches!(
                event,
                Event::Exit(_) | Event::Error(_) | Event::HistoryError(_)
            );
            assert!(!failed, "{event:?}");
        }
    }
}

fn pty() -> Result<()> {
    let directory = std::env::temp_dir().join(format!("nextty-bench-{}", std::process::id()));
    let budget = HistoryBudget::new(directory, 1 << 30)?;
    let spawn = || {
        let script = "stty raw -echo; printf '\\033[H!'; exec /bin/cat";
        let shell = Shell::new("/bin/sh".into(), vec!["-c".into(), script.into()]);
        let mut terminal =
            Terminal::new(Some(shell), None, SIZE, COLORS.0, COLORS.1, budget.clone())?;
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
    println!(
        "PTY cat to parsed grid with disk history, 30 MiB: median {:.2} MiB/s of {rates:.1?}",
        median(rates)
    );

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
    let percentile = |percent: f64| samples[(percent * samples.len() as f64).ceil() as usize - 1];
    println!(
        "PTY echo to parsed grid with disk history, 2000 samples: p50 {:.2} µs, p95 {:.2}, p99 {:.2}, max {:.2}",
        percentile(0.50),
        percentile(0.95),
        percentile(0.99),
        percentile(1.0),
    );
    Ok(())
}

fn main() -> Result<()> {
    parser();
    pty()
}
