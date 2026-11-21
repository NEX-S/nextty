use std::{
    fs::File,
    io::{self, BufWriter, Write},
};

/// The facts `tools/latency.py gui` reads: the background colour of each frame's first cell, which
/// the fixture uses as an output marker, and when each frame reached the screen.
pub(crate) struct Trace {
    file: BufWriter<File>,
    frame: u32,
}

impl Trace {
    pub(crate) fn from_env() -> io::Result<Option<Self>> {
        std::env::var_os("NEXTTY_TRACE")
            .map(|path| {
                Ok(Self {
                    file: BufWriter::new(File::create(path)?),
                    frame: 0,
                })
            })
            .transpose()
    }

    pub(crate) fn event(&mut self, event: &super::Event) -> io::Result<()> {
        match event.kind {
            super::PRESENTED => writeln!(self.file, "presented {} {:.9}", event.code, event.x),
            super::FOCUS => {
                writeln!(self.file, "focus {}", event.code)?;
                // The latency runner waits for focus before it can send input.
                self.file.flush()
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn frame(&mut self, marker: u32, focused: bool) -> io::Result<()> {
        self.frame += 1;
        writeln!(
            self.file,
            "frame {} {marker} {}",
            self.frame,
            u8::from(focused)
        )
    }
}
