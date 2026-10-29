use std::{
    ffi::OsString,
    fs::File,
    io::{self, BufWriter, Write},
    sync::mpsc::{SyncSender, TrySendError, sync_channel},
    thread::{self, JoinHandle},
};

pub(crate) struct Trace {
    pub(crate) output: BufWriter<TraceWriter>,
    pub(crate) frame: u32,
    pub(crate) pending: Option<(String, String)>,
}
const TRACE_BLOCK: usize = 8192;
pub(crate) struct TraceWriter {
    sender: Option<SyncSender<Vec<u8>>>,
    worker: Option<JoinHandle<io::Result<()>>>,
    pub(crate) draining: bool,
}
impl TraceWriter {
    fn new(path: OsString) -> io::Result<Self> {
        let mut file = File::create(path)?;
        let (sender, receiver) = sync_channel::<Vec<u8>>(8);
        let worker = thread::Builder::new()
            .name("nextty-trace".into())
            .spawn(move || {
                for bytes in receiver {
                    file.write_all(&bytes)?;
                }
                Ok(())
            })?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
            draining: false,
        })
    }
    fn finish(&mut self) -> io::Result<()> {
        self.sender.take();
        self.worker
            .take()
            .map_or(Ok(()), |worker| worker.join().unwrap())
    }
}
impl Write for TraceWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "trace writer stopped"))?;
        let count = bytes.len().min(TRACE_BLOCK);
        let block = bytes[..count].to_vec();
        if self.draining {
            sender
                .send(block)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "trace writer stopped"))?;
        } else {
            sender.try_send(block).map_err(|error| match error {
                TrySendError::Full(_) => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "trace queue is full; recording incomplete",
                ),
                TrySendError::Disconnected(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "trace writer stopped")
                }
            })?;
        }
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Trace {
    pub(crate) fn from_env() -> io::Result<Option<Self>> {
        std::env::var_os("NEXTTY_TRACE")
            .map(|path| {
                TraceWriter::new(path).map(|output| Self {
                    output: BufWriter::with_capacity(TRACE_BLOCK, output),
                    frame: 0,
                    pending: None,
                })
            })
            .transpose()
    }

    pub(crate) fn record_swap(&mut self, time: f64) -> io::Result<()> {
        if let Some((prefix, suffix)) = self.pending.take() {
            writeln!(self.output, "{prefix} {time:.9}{suffix}")?;
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self) -> io::Result<()> {
        if self.output.get_ref().worker.is_none() {
            return Ok(());
        }
        // Only teardown may wait for storage, so successful traces retain every byte.
        self.output.get_mut().draining = true;
        let flushed = self.output.flush();
        let completed = self.output.get_mut().finish();
        completed.and(flushed)
    }
}
impl Drop for Trace {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            eprintln!("trace: {error}");
        }
    }
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
