use std::{
    collections::VecDeque,
    ffi::OsString,
    fs::File,
    io::{self, BufWriter, Write},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};

pub(crate) struct Trace {
    pub(crate) output: BufWriter<TraceWriter>,
    pub(crate) frame: u32,
    pub(crate) pending: Option<(String, String)>,
}
pub(crate) const TRACE_BLOCK: usize = 8192;
struct TraceQueue {
    bytes: VecDeque<Vec<u8>>,
    opened: bool,
    closed: bool,
    error: Option<io::Error>,
}
struct TraceShared {
    queue: Mutex<TraceQueue>,
    changed: Condvar,
}
pub(crate) struct TraceWriter {
    shared: Arc<TraceShared>,
    worker: Option<JoinHandle<()>>,
    pub(crate) draining: bool,
}
impl TraceWriter {
    pub(crate) fn new(path: OsString) -> io::Result<Self> {
        let shared = Arc::new(TraceShared {
            queue: Mutex::new(TraceQueue {
                bytes: VecDeque::new(),
                opened: false,
                closed: false,
                error: None,
            }),
            changed: Condvar::new(),
        });
        let queue = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("nextty-trace".into())
            .spawn(move || {
                let result = (|| {
                    let mut file = File::create(path)?;
                    {
                        let mut state = queue.queue.lock().unwrap();
                        state.opened = true;
                        queue.changed.notify_all();
                    }
                    loop {
                        let bytes = {
                            let mut state = queue.queue.lock().unwrap();
                            while state.bytes.is_empty() && !state.closed {
                                state = queue.changed.wait(state).unwrap();
                            }
                            let Some(bytes) = state.bytes.pop_front() else {
                                return Ok(());
                            };
                            queue.changed.notify_all();
                            bytes
                        };
                        file.write_all(&bytes)?;
                    }
                })();
                let mut state = queue.queue.lock().unwrap();
                state.error = result.err();
                state.opened = true;
                state.closed = true;
                queue.changed.notify_all();
            })?;
        // Preserve construction-time path errors before the input loop starts.
        let mut state = shared.queue.lock().unwrap();
        while !state.opened {
            state = shared.changed.wait(state).unwrap();
        }
        if let Some(error) = state.error.take() {
            return Err(error);
        }
        drop(state);
        Ok(Self {
            shared,
            worker: Some(worker),
            draining: false,
        })
    }
    pub(crate) fn finish(&mut self) -> io::Result<()> {
        self.shared.queue.lock().unwrap().closed = true;
        self.shared.changed.notify_all();
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
        self.shared
            .queue
            .lock()
            .unwrap()
            .error
            .take()
            .map_or(Ok(()), Err)
    }
}
impl Write for TraceWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self.shared.queue.lock().unwrap();
        while self.draining && state.bytes.len() == 8 && !state.closed {
            state = self.shared.changed.wait(state).unwrap();
        }
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "trace writer stopped",
            ));
        }
        if state.bytes.len() == 8 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "trace queue is full; recording incomplete",
            ));
        }
        let count = bytes.len().min(TRACE_BLOCK);
        state.bytes.push_back(bytes[..count].to_vec());
        self.shared.changed.notify_all();
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        // BufWriter has queued its bytes; the worker writes directly to File.
        Ok(())
    }
}
impl Trace {
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
