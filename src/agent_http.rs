use crate::eventfd::consume_notification;
use curl::easy::{Easy2, Handler, List, WriteError};
use curl::multi::{Multi, WaitFd};
use serde_json::Value;
use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const BUFFER_LIMIT: usize = 256 * 1024;

#[derive(Default)]
struct Response {
    bytes: Vec<u8>,
    overflow: bool,
}

impl Handler for Response {
    fn write(&mut self, bytes: &[u8]) -> Result<usize, WriteError> {
        if self.bytes.len() + bytes.len() > BUFFER_LIMIT {
            self.overflow = true;
            return Ok(0);
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
}

#[derive(Default)]
struct Sse {
    pending: Vec<u8>,
    data: Vec<u8>,
}

impl Sse {
    fn feed(
        &mut self,
        bytes: &[u8],
        emit: &mut impl FnMut(Value) -> io::Result<()>,
    ) -> io::Result<()> {
        // Retained bytes have already been scanned and contain no newline.
        let scanned = self.pending.len();
        self.pending.extend_from_slice(bytes);
        let mut start = 0;
        for end in scanned..self.pending.len() {
            if self.pending[end] != b'\n' {
                continue;
            }
            let line = &self.pending[start..end];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                if !self.data.is_empty() {
                    emit(serde_json::from_slice(&self.data)?)?;
                    self.data.clear();
                }
            } else if let Some(data) = line.strip_prefix(b"data:") {
                let data = data.strip_prefix(b" ").unwrap_or(data);
                if self.data.len() + data.len() + 1 > BUFFER_LIMIT {
                    return Err(io::Error::other("Agent SSE event exceeds 256 KiB"));
                }
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(data);
            }
            start = end + 1;
        }
        self.pending.drain(..start);
        if self.pending.len() + self.data.len() > BUFFER_LIMIT {
            return Err(io::Error::other("Agent SSE event exceeds 256 KiB"));
        }
        Ok(())
    }

    fn finish(&self) -> io::Result<()> {
        if !self.pending.is_empty() || !self.data.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Agent SSE stream ended within an event",
            ));
        }
        Ok(())
    }
}

pub(crate) struct Http {
    multi: Multi,
}

impl Http {
    pub fn new() -> Self {
        Self {
            multi: Multi::new(),
        }
    }

    pub fn stream(
        &mut self,
        url: &str,
        headers: &[String],
        body: &Value,
        wake: RawFd,
        canceled: &AtomicBool,
        mut emit: impl FnMut(Value) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut easy = Easy2::new(Response::default());
        easy.url(url)?;
        easy.post(true)?;
        easy.post_fields_copy(&serde_json::to_vec(body)?)?;
        easy.connect_timeout(Duration::from_secs(15))?;
        easy.low_speed_limit(1)?;
        easy.low_speed_time(Duration::from_secs(120))?;
        let mut list = List::new();
        list.append("Content-Type: application/json")?;
        list.append("Accept: text/event-stream")?;
        for header in headers {
            list.append(header)?;
        }
        easy.http_headers(list)?;
        let mut handle = self.multi.add2(easy)?;
        let result = (|| {
            let mut sse = Sse::default();
            loop {
                if canceled.load(Ordering::Acquire) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "Agent stopped"));
                }
                self.multi.perform()?;
                let status = handle.response_code()?;
                let response = handle.get_mut();
                if (200..300).contains(&status) {
                    if response.overflow {
                        return Err(io::Error::other("Agent unread response exceeds 256 KiB"));
                    }
                    sse.feed(&response.bytes, &mut emit)?;
                    response.bytes.clear();
                }
                let mut completed = None;
                self.multi.messages(|message| {
                    if let Some(result) = message.result_for2(&handle) {
                        completed = Some(result);
                    }
                });
                if let Some(completed) = completed {
                    if status != 0 && !(200..300).contains(&status) {
                        let response = handle.get_ref();
                        let bytes = &response.bytes[..response.bytes.len().min(8192)];
                        return Err(io::Error::other(format!(
                            "Agent HTTP {status}: {}",
                            String::from_utf8_lossy(bytes)
                        )));
                    }
                    completed?;
                    return sse.finish();
                }
                let mut fd = WaitFd::new();
                fd.set_fd(wake);
                fd.poll_on_read(true);
                self.multi
                    .wait(std::slice::from_mut(&mut fd), Duration::from_secs(1))?;
                if fd.received_read() {
                    consume_notification(wake)?;
                }
            }
        })();
        self.multi.remove2(handle)?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsRawFd;
    use std::sync::{Arc, mpsc};
    use std::thread::{self, JoinHandle};
    use std::time::Instant;

    fn server(reply: impl FnOnce(TcpStream) + Send + 'static) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/messages", listener.local_addr().unwrap());
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            let header = String::from_utf8(header).unwrap();
            let length: usize = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .map(str::to_owned)
                })
                .unwrap()
                .parse()
                .unwrap();
            stream.read_exact(&mut vec![0; length]).unwrap();
            reply(stream);
        });
        (url, thread)
    }

    #[test]
    fn delivers_fragmented_multiline_json_before_the_response_ends() {
        let (emitted, received) = mpsc::channel();
        let (url, thread) = server(move |mut stream| {
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
            let bytes = ": heartbeat\r\nevent: text\r\ndata: {\"type\":\"text\",\r\ndata: \"text\":\"你😀\"}\r\n\r\n".as_bytes();
            for chunk in bytes.chunks(3) {
                stream.write_all(chunk).unwrap();
            }
            received.recv_timeout(Duration::from_secs(2)).unwrap();
            stream.write_all(b"data: {\"type\":\"end\"}\n\n").unwrap();
        });
        let mut events = Vec::new();
        Http::new()
            .stream(
                &url,
                &[],
                &serde_json::json!({}),
                crate::eventfd::new().unwrap().as_raw_fd(),
                &AtomicBool::new(false),
                |event| {
                    if events.is_empty() {
                        emitted.send(()).unwrap();
                    }
                    events.push(event);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(
            events,
            [
                serde_json::json!({"type":"text","text":"你😀"}),
                serde_json::json!({"type":"end"})
            ]
        );
        thread.join().unwrap();
    }

    #[test]
    fn errors_detach_the_transfer_and_preserve_http_error_bodies() {
        let mut http = Http::new();
        let wake = crate::eventfd::new().unwrap();
        let canceled = AtomicBool::new(false);
        let (url, thread) = server(|mut stream| {
            stream.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 19\r\n\r\ngateway unavailable").unwrap();
        });
        let error = http
            .stream(
                &url,
                &[],
                &serde_json::json!({}),
                wake.as_raw_fd(),
                &canceled,
                |_| panic!("HTTP error emitted as SSE"),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "Agent HTTP 429: gateway unavailable");
        thread.join().unwrap();
        let (url, thread) = server(|mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\ndata: {}\n\n")
                .unwrap();
        });
        let error = http
            .stream(
                &url,
                &[],
                &serde_json::json!({}),
                wake.as_raw_fd(),
                &canceled,
                |_| Err(io::Error::other("receiver full")),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "receiver full");
        thread.join().unwrap();
        let (url, thread) = server(|mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\ndata: {\"unfinished\":")
                .unwrap();
        });
        let error = http
            .stream(
                &url,
                &[],
                &serde_json::json!({}),
                wake.as_raw_fd(),
                &canceled,
                |_| Ok(()),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        thread.join().unwrap();
        let (url, thread) = server(|mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\ndata: {}\n\n")
                .unwrap();
        });
        http.stream(
            &url,
            &[],
            &serde_json::json!({}),
            wake.as_raw_fd(),
            &canceled,
            |_| Ok(()),
        )
        .unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn cancellation_wakes_and_closes_an_idle_transfer() {
        let wake = Arc::new(crate::eventfd::new().unwrap());
        let canceled = Arc::new(AtomicBool::new(false));
        let (ready, received) = mpsc::channel();
        let (url, server) = server(move |mut stream| {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n: waiting\n\n",
                )
                .unwrap();
            ready.send(()).unwrap();
            let mut byte = [0];
            match stream.read(&mut byte) {
                Ok(0) => (),
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => (),
                result => panic!("canceled connection remained open: {result:?}"),
            }
        });
        let cancel = Arc::clone(&canceled);
        let notify = Arc::clone(&wake);
        let canceler = thread::spawn(move || {
            received.recv_timeout(Duration::from_secs(2)).unwrap();
            let start = Instant::now();
            cancel.store(true, Ordering::Release);
            let value = 1u64;
            assert_eq!(
                unsafe { libc::write(notify.as_raw_fd(), (&value as *const u64).cast(), 8) },
                8
            );
            start
        });
        let error = Http::new()
            .stream(
                &url,
                &[],
                &serde_json::json!({}),
                wake.as_raw_fd(),
                &canceled,
                |_| Ok(()),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(canceler.join().unwrap().elapsed() < Duration::from_millis(500));
        server.join().unwrap();
    }
}
