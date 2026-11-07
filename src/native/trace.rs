use super::Event;
use crate::screen::Screen;
use std::{
    fs::File,
    io::{self, BufWriter, Write},
};

/// Frame and presentation records for tools/native_perf.py and tools/wayland_input.py.
pub(crate) struct Trace {
    output: BufWriter<File>,
    frame: u32,
    pending: Option<String>,
}

impl Trace {
    pub(crate) fn from_env() -> io::Result<Option<Self>> {
        std::env::var_os("NEXTTY_TRACE")
            .map(|path| {
                Ok(Self {
                    output: BufWriter::new(File::create(path)?),
                    frame: 0,
                    pending: None,
                })
            })
            .transpose()
    }

    /// Counts frames in step with the native presentation feedback ids.
    pub(crate) fn frame(
        &mut self,
        begin: f64,
        prepared: f64,
        marker: u32,
        (width, height, scale): (i32, i32, f64),
        focused: bool,
    ) {
        self.frame = self.frame.wrapping_add(1);
        self.pending = Some(format!(
            "frame {} {begin:.9} {prepared:.9} {marker} {width} {height} {scale:.9} {}",
            self.frame,
            u8::from(focused)
        ));
    }

    pub(crate) fn record_swap(&mut self, time: f64) -> io::Result<()> {
        if let Some(frame) = self.pending.take() {
            writeln!(self.output, "{frame} {time:.9}")?;
        }
        Ok(())
    }

    pub(crate) fn presented(&mut self, event: &Event) -> io::Result<()> {
        writeln!(
            self.output,
            "presented {} {:.9} {} {} {} {:.0}",
            event.code, event.x, event.start, event.end, event.modifiers, event.y
        )
    }

    pub(crate) fn focus(&mut self, focused: bool) -> io::Result<()> {
        writeln!(self.output, "focus {:.9} {}", monotonic(), u8::from(focused))?;
        // The input probe waits for focus before frames fill the buffer.
        self.output.flush()
    }

    pub(crate) fn finish(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

/// The measurement fixtures encode their output marker in the first cell's background.
pub(crate) fn marker(screen: &Screen) -> u32 {
    let color = screen.color(screen.row(0).cells[0].bg);
    (u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b)
}

pub(crate) fn monotonic() -> f64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time);
    }
    time.tv_sec as f64 + time.tv_nsec as f64 * 1e-9
}
