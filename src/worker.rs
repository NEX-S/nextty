use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

struct Work<Request, Output> {
    pending: Option<Request>,
    result: Option<Output>,
    closed: bool,
}

/// A thread that runs the latest request it was given and keeps the latest result, waking
/// `fd` for each.
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
        let work = Work {
            pending: None,
            result: None,
            closed: false,
        };
        let shared = Arc::new((Mutex::new(work), Condvar::new()));
        let (worker, wakeup) = (Arc::clone(&shared), Arc::clone(&notification));
        let handle = thread::Builder::new().name(name.into()).spawn(move || {
            let (lock, changed) = &*worker;
            let notify = || crate::sys::notify(wakeup.as_raw_fd());
            // An end, even by a panic, wakes the owner, whose `take` then reports it.
            let _exit = OnDrop(|| {
                lock.lock().unwrap().closed = true;
                notify();
            });
            // A replaced result is freed after the owner is woken.
            let publish = |result| {
                let previous = lock.lock().unwrap().result.replace(result);
                notify();
                drop(previous);
            };
            loop {
                let state = lock.lock().unwrap();
                let wait = |state: &mut Work<_, _>| state.pending.is_none() && !state.closed;
                let mut state = changed.wait_while(state, wait).unwrap();
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
        changed.notify_one();
        drop(previous);
    }

    pub fn take(&mut self) -> Option<Output> {
        let mut state = self.shared.0.lock().unwrap();
        if !state.closed {
            return state.result.take();
        }
        drop(state);
        let ended = self.handle.take().unwrap().join();
        ended.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        unreachable!("background worker exited while its owner is alive");
    }
}

impl<Request, Output> Drop for Worker<Request, Output> {
    fn drop(&mut self) {
        let (lock, changed) = &*self.shared;
        lock.lock().unwrap().closed = true;
        changed.notify_one();
    }
}

struct OnDrop<Finish: FnMut()>(Finish);

impl<Finish: FnMut()> Drop for OnDrop<Finish> {
    fn drop(&mut self) {
        (self.0)();
    }
}
