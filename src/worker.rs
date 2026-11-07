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
                if !std::mem::replace(&mut lock.lock().unwrap().closed, true) {
                    notify();
                }
            });
            let publish = |result| {
                let mut state = lock.lock().unwrap();
                let closed = state.closed;
                let previous = if closed {
                    state.result.take()
                } else {
                    state.result.replace(result)
                };
                drop(state);
                if !closed {
                    notify();
                }
                drop(previous);
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

    #[cfg(any(feature = "gui", feature = "tui", test))]
    pub fn ready(&self, matches: impl FnOnce(&Output) -> bool) -> bool {
        let state = self.shared.0.lock().unwrap();
        state.closed || state.result.as_ref().is_some_and(matches)
    }

    pub fn submit(&self, request: impl FnOnce(Option<&mut Request>) -> Request) {
        let (lock, changed) = &*self.shared;
        let mut state = lock.lock().unwrap();
        let request = request(state.pending.as_mut());
        let previous = state.pending.replace(request);
        drop(state);
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
        lock.lock().unwrap().closed = true;
        changed.notify_one();
    }
}

struct WakeOnDrop<Finish: Fn()>(Finish);

impl<Finish: Fn()> Drop for WakeOnDrop<Finish> {
    fn drop(&mut self) {
        (self.0)();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn replaced_result_cleanup_does_not_delay_ready_notification() {
        enum Output {
            Stale(mpsc::Sender<()>, mpsc::Receiver<()>),
            Current,
        }
        impl Drop for Output {
            fn drop(&mut self) {
                if let Self::Stale(entered, release) = self {
                    entered.send(()).unwrap();
                    release.recv().unwrap();
                }
            }
        }
        let notification = Arc::new(crate::eventfd::new().unwrap());
        let mut worker = Worker::new("publish-test", notification, |result, publish| {
            publish(result);
        })
        .unwrap();
        let (entered, dropping) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        worker.submit(|_| Output::Stale(entered, blocked));
        let mut fds = [libc::pollfd {
            fd: worker.fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        crate::poll::poll(&mut fds, 5000).unwrap();
        assert_ne!(fds[0].revents & libc::POLLIN, 0);
        crate::eventfd::consume_notification(worker.fd()).unwrap();
        worker.submit(|_| Output::Current);
        dropping.recv_timeout(Duration::from_secs(5)).unwrap();
        crate::poll::poll(&mut fds, 1000).unwrap();
        let notified = fds[0].revents & libc::POLLIN != 0;
        let result = worker.take();
        release.send(()).unwrap();
        assert!(notified);
        assert!(matches!(result, Some(Output::Current)));
    }
}
