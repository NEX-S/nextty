use crate::eventfd::{consume_notification, notify};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

pub(super) struct Completion {
    pub id: u64,
    pub name: &'static str,
    pub text: String,
    pub is_error: bool,
}

struct Job {
    id: u64,
    canceled: Arc<AtomicBool>,
    wake: Arc<OwnedFd>,
    thread: JoinHandle<()>,
}

pub(super) struct Tasks {
    jobs: Vec<Job>,
    completed: Option<Arc<OwnedFd>>,
    sender: mpsc::Sender<Completion>,
    receiver: mpsc::Receiver<Completion>,
    next_id: u64,
}

impl Default for Tasks {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            jobs: Vec::new(),
            completed: None,
            sender,
            receiver,
            next_id: 0,
        }
    }
}

impl Tasks {
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub fn spawn(
        &mut self,
        name: &'static str,
        work: impl FnOnce(Arc<AtomicBool>, Arc<OwnedFd>) -> io::Result<(String, bool)> + Send + 'static,
    ) -> io::Result<u64> {
        if self.completed.is_none() {
            self.completed = Some(Arc::new(crate::eventfd::new()?));
        }
        let completed = Arc::clone(self.completed.as_ref().unwrap());
        let canceled = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(crate::eventfd::new()?);
        let worker_canceled = Arc::clone(&canceled);
        let worker_wake = Arc::clone(&wake);
        let sender = self.sender.clone();
        let id = self.next_id + 1;
        let thread = std::thread::Builder::new()
            .name(format!("nextty-agent-task-{id}"))
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| work(worker_canceled, worker_wake)));
                let (text, is_error) = match result {
                    Ok(Ok(result)) => result,
                    Ok(Err(error)) => (error.to_string(), true),
                    Err(panic) => {
                        let message = panic
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| panic.downcast_ref::<&str>().copied())
                            .unwrap_or("non-string panic");
                        (format!("Background task panicked: {message}"), true)
                    }
                };
                if sender
                    .send(Completion {
                        id,
                        name,
                        text,
                        is_error,
                    })
                    .is_ok()
                    && let Err(error) = notify(completed.as_raw_fd())
                {
                    eprintln!("notify background task: {error}");
                }
            })?;
        self.next_id = id;
        self.jobs.push(Job {
            id,
            canceled,
            wake,
            thread,
        });
        Ok(id)
    }

    pub fn take_ready(&mut self) -> io::Result<Vec<Completion>> {
        if let Some(completed) = &self.completed {
            consume_notification(completed.as_raw_fd())?;
        }
        let mut results = Vec::new();
        for completion in self.receiver.try_iter() {
            let index = self
                .jobs
                .iter()
                .position(|job| job.id == completion.id)
                .unwrap();
            let job = self.jobs.swap_remove(index);
            job.thread
                .join()
                .map_err(|_| io::Error::other("Background task notification panicked"))?;
            results.push(completion);
        }
        Ok(results)
    }

    pub fn wait(
        &mut self,
        canceled: &AtomicBool,
        parent_wake: RawFd,
    ) -> io::Result<Vec<Completion>> {
        loop {
            super::check_cancel(canceled)?;
            let ready = self.take_ready()?;
            if !ready.is_empty() || self.is_empty() {
                return Ok(ready);
            }
            let mut fds = [
                libc::pollfd {
                    fd: self.completed.as_ref().unwrap().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: parent_wake,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if fds[1].revents != 0 {
                consume_notification(parent_wake)?;
            }
        }
    }
}

impl Drop for Tasks {
    fn drop(&mut self) {
        for job in &self.jobs {
            job.canceled.store(true, Ordering::Release);
            if let Err(error) = notify(job.wake.as_raw_fd()) {
                eprintln!("stop background task: {error}");
            }
        }
        for job in self.jobs.drain(..) {
            if job.thread.join().is_err() {
                eprintln!("Background task notification panicked");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn command_completion_wakes_waiter_and_reports_failures() {
        let mut tasks = Tasks::default();
        let parent_wake = crate::eventfd::new().unwrap();
        let id = tasks
            .spawn("monitor", |canceled, wake| {
                crate::agent_command::monitor(
                    "sleep 0.05; printf 'finished'; exit 7",
                    None,
                    wake.as_raw_fd(),
                    &canceled,
                )
            })
            .unwrap();
        let results = tasks
            .wait(&AtomicBool::new(false), parent_wake.as_raw_fd())
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, id);
        assert_eq!(results[0].name, "monitor");
        assert!(results[0].text.contains("finished"));
        assert!(results[0].text.contains("exit status: 7"));
        assert!(results[0].is_error);
        assert!(tasks.is_empty());
        assert!(tasks.take_ready().unwrap().is_empty());
    }

    #[test]
    fn panic_is_a_completion_instead_of_an_infinite_wait() {
        let mut tasks = Tasks::default();
        tasks
            .spawn("subagent", |_, _| panic!("fixture failure"))
            .unwrap();
        let wake = crate::eventfd::new().unwrap();
        let results = tasks
            .wait(&AtomicBool::new(false), wake.as_raw_fd())
            .unwrap();
        assert!(results[0].is_error);
        assert!(results[0].text.contains("fixture failure"));
        assert!(tasks.is_empty());
    }

    #[test]
    fn cancellation_wakes_parent_and_drop_joins_canceled_command() {
        let mut tasks = Tasks::default();
        let path = std::env::temp_dir().join(format!("nextty-task-cancel-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let command = format!("printf '%s' $$ > '{}'; exec sleep 120", path.display());
        tasks
            .spawn("monitor", move |canceled, wake| {
                crate::agent_command::monitor(&command, None, wake.as_raw_fd(), &canceled)
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let pid = loop {
            if let Ok(pid) = std::fs::read_to_string(&path)
                && !pid.is_empty()
            {
                break pid;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        let parent_wake = Arc::new(crate::eventfd::new().unwrap());
        let canceled = Arc::new(AtomicBool::new(false));
        let worker_wake = Arc::clone(&parent_wake);
        let worker_canceled = Arc::clone(&canceled);
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            worker_canceled.store(true, Ordering::Release);
            notify(worker_wake.as_raw_fd()).unwrap();
        });
        let started = Instant::now();
        assert_eq!(
            tasks
                .wait(&canceled, parent_wake.as_raw_fd())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Interrupted
        );
        drop(tasks);
        worker.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            assert_eq!(stat.split_whitespace().nth(2), Some("Z"));
        }
        std::fs::remove_file(path).unwrap();
    }
}
