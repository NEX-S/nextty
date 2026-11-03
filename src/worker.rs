use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

struct Work<Request, Output> {
    pending: Option<Request>,
    result: Option<Output>,
    closed: bool,
}

pub(crate) struct Worker<Request, Output> {
    shared: Arc<(Mutex<Work<Request, Output>>, Condvar)>,
    notification: Arc<OwnedFd>,
    handle: Option<JoinHandle<()>>,
}

impl<Request: Send + 'static, Output: Send + 'static> Worker<Request, Output> {
    pub fn new(
        name: &'static str,
        notification: Arc<OwnedFd>,
        mut run: impl FnMut(Request, &dyn Fn(Output)) + Send + 'static,
    ) -> io::Result<Self> {
        let shared = Arc::new((
            Mutex::new(Work {
                pending: None,
                result: None,
                closed: false,
            }),
            Condvar::new(),
        ));
        let worker = Arc::clone(&shared);
        let wakeup = Arc::clone(&notification);
        let handle = thread::Builder::new().name(name.into()).spawn(move || {
            let (lock, changed) = &*worker;
            let notify = || {
                if let Err(error) = crate::eventfd::notify(wakeup.as_raw_fd()) {
                    eprintln!("notify {name}: {error}");
                }
            };
            let _exit = WakeOnDrop(|| {
                lock.lock().unwrap().closed = true;
                notify();
            });
            let publish = |result| {
                let previous = lock.lock().unwrap().result.replace(result);
                drop(previous);
                notify();
            };
            loop {
                let mut state = lock.lock().unwrap();
                while state.pending.is_none() && !state.closed {
                    state = changed.wait(state).unwrap();
                }
                if state.closed {
                    return;
                }
                let request = state.pending.take().unwrap();
                drop(state);
                run(request, &publish);
            }
        })?;
        Ok(Self {
            shared,
            notification,
            handle: Some(handle),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.notification.as_raw_fd()
    }

    pub fn ready(&self, matches: impl FnOnce(&Output) -> bool) -> bool {
        let state = self.shared.0.lock().unwrap();
        state.closed || state.result.as_ref().is_some_and(matches)
    }

    pub fn submit(&self, request: Request) {
        let (lock, changed) = &*self.shared;
        let previous = lock.lock().unwrap().pending.replace(request);
        if previous.is_none() {
            changed.notify_one();
        }
        drop(previous);
    }

    pub fn take(&mut self) -> Option<Output> {
        let mut state = self.shared.0.lock().unwrap();
        if !state.closed {
            return state.result.take();
        }
        drop(state);
        self.handle
            .take()
            .unwrap()
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        unreachable!("background worker exited while its owner is alive");
    }
}

impl<Request, Output> Drop for Worker<Request, Output> {
    fn drop(&mut self) {
        let (lock, changed) = &*self.shared;
        let mut state = lock.lock().unwrap();
        state.closed = true;
        changed.notify_one();
    }
}

struct WakeOnDrop<Finish: Fn()>(Finish);

impl<Finish: Fn()> Drop for WakeOnDrop<Finish> {
    fn drop(&mut self) {
        (self.0)();
    }
}
