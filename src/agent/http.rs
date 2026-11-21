use crate::sys::consume_notification;
use curl::easy::{Easy2, Handler, List, WriteError};
use curl::multi::{Multi, WaitFd};
use serde_json::Value;
use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(crate) fn retryable(error: &io::Error) -> bool {
    if error
        .get_ref()
        .is_some_and(|error| error.is::<serde_json::Error>())
    {
        return false;
    }
    if let Some(error) = error
        .get_ref()
        .and_then(|error| error.downcast_ref::<curl::Error>())
    {
        return error.is_partial_file()
            || error.is_couldnt_resolve_proxy()
            || error.is_couldnt_resolve_host()
            || error.is_couldnt_connect()
            || error.is_operation_timedout()
            || error.is_got_nothing()
            || error.is_send_error()
            || error.is_recv_error()
            || error.is_http2_error()
            || error.is_http2_stream_error();
    }
    matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof | io::ErrorKind::WouldBlock
    )
}

#[derive(Default)]
struct Response {
    bytes: Vec<u8>,
}

impl Handler for Response {
    fn write(&mut self, bytes: &[u8]) -> Result<usize, WriteError> {
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
        emit: &mut impl FnMut(Value) -> io::Result<bool>,
    ) -> io::Result<bool> {
        // Retained bytes have already been scanned and contain no newline.
        let scanned = self.pending.len();
        self.pending.extend_from_slice(bytes);
        let mut start = 0;
        for end in memchr::memchr_iter(b'\n', &self.pending[scanned..]).map(|end| scanned + end) {
            let line = &self.pending[start..end];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() && !self.data.is_empty() {
                let complete = emit(serde_json::from_slice(&self.data)?)?;
                self.data.clear();
                if complete {
                    self.pending.clear();
                    return Ok(true);
                }
            } else if let Some(data) = line.strip_prefix(b"data:") {
                let data = data.strip_prefix(b" ").unwrap_or(data);
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(data);
            }
            start = end + 1;
        }
        self.pending.drain(..start);
        Ok(false)
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
        body: &[u8],
        wake: RawFd,
        canceled: &AtomicBool,
        mut emit: impl FnMut(Value) -> io::Result<bool>,
    ) -> io::Result<()> {
        let mut easy = Easy2::new(Response::default());
        easy.url(url)?;
        easy.post(true)?;
        easy.post_fields_copy(body)?;
        easy.connect_timeout(Duration::from_secs(15))?;
        easy.low_speed_limit(1)?;
        // Model reasoning and gateway queues can stay silent for several minutes.
        easy.low_speed_time(Duration::from_secs(600))?;
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
                let complete = if (200..300).contains(&status) {
                    let complete = sse.feed(&response.bytes, &mut emit)?;
                    response.bytes.clear();
                    complete
                } else {
                    false
                };
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
                        let kind = if matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529) {
                            io::ErrorKind::WouldBlock
                        } else {
                            io::ErrorKind::Other
                        };
                        return Err(io::Error::new(
                            kind,
                            format!("Agent HTTP {status}: {}", String::from_utf8_lossy(bytes)),
                        ));
                    }
                    completed?;
                    return sse.finish();
                }
                if complete {
                    return Ok(());
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
pub(crate) fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    use std::io::Read;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
    let length: usize = String::from_utf8(header)
        .unwrap()
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap();
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    body
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
            read_request(&mut stream);
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
                b"{}",
                crate::sys::new().unwrap().as_raw_fd(),
                &AtomicBool::new(false),
                |event| {
                    if events.is_empty() {
                        emitted.send(()).unwrap();
                    }
                    events.push(event);
                    Ok(false)
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
    fn completed_chunked_http_responses_reuse_the_same_connection() {
        let (url, server) = server(|mut stream| {
            let body = b"data: {\"type\":\"end\"}\n\n";
            let mut reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                body.len()
            )
            .into_bytes();
            reply.extend_from_slice(body);
            reply.extend_from_slice(b"\r\n0\r\n\r\n");
            stream.write_all(&reply).unwrap();
            assert_eq!(read_request(&mut stream), b"{}");
            stream.write_all(&reply).unwrap();
        });
        let mut http = Http::new();
        let wake = crate::sys::new().unwrap();
        let canceled = AtomicBool::new(false);
        let mut events = Vec::new();
        for _ in 0..2 {
            http.stream(&url, &[], b"{}", wake.as_raw_fd(), &canceled, |event| {
                events.push(event);
                Ok(true)
            })
            .unwrap();
        }
        assert_eq!(events, vec![serde_json::json!({"type":"end"}); 2]);
        server.join().unwrap();
    }

    #[test]
    fn errors_detach_the_transfer_and_preserve_http_error_bodies() {
        let mut http = Http::new();
        let mut fetch = |reply: &'static [u8], emit: fn(Value) -> io::Result<bool>| {
            let (url, thread) = server(move |mut stream| stream.write_all(reply).unwrap());
            let wake = crate::sys::new().unwrap();
            let result = http.stream(
                &url,
                &[],
                b"{}",
                wake.as_raw_fd(),
                &AtomicBool::new(false),
                emit,
            );
            thread.join().unwrap();
            result
        };
        let error = fetch(
            b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 19\r\n\r\ngateway unavailable",
            |_| panic!("HTTP error emitted as SSE"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "Agent HTTP 429: gateway unavailable");
        let ok = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\ndata: {}\n\n";
        let error = fetch(ok, |_| Err(io::Error::other("receiver full"))).unwrap_err();
        assert_eq!(error.to_string(), "receiver full");
        let unfinished = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\ndata: {\"unfinished\":";
        let error = fetch(unfinished, |_| Ok(false)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        fetch(ok, |_| Ok(false)).unwrap();
    }

    #[test]
    fn cancellation_wakes_and_closes_an_idle_transfer() {
        let wake = Arc::new(crate::sys::new().unwrap());
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
            .stream(&url, &[], b"{}", wake.as_raw_fd(), &canceled, |_| Ok(false))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(canceler.join().unwrap().elapsed() < Duration::from_millis(500));
        server.join().unwrap();
    }
}
