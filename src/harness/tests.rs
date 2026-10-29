use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct Worker {
    harness: Option<Harness>,
    done: mpsc::Receiver<io::Result<(VecDeque<Turn>, String)>>,
}

impl Worker {
    fn new(url: String) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: crate::eventfd::new().unwrap(),
            notification: crate::eventfd::new().unwrap(),
        });
        let control = Arc::clone(&shared);
        let (done, receiver) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let mut session = Session {
                http: Http::new(),
                url,
                headers: vec!["anthropic-version: 2023-06-01".into()],
                model: "local-fixture".into(),
                history: VecDeque::new(),
                summary: String::new(),
            };
            let result = session
                .run(&shared)
                .map(|()| (session.history, session.summary));
            done.send(result).unwrap();
        });
        Self {
            harness: Some(Harness {
                shared: control,
                worker: Some(thread),
            }),
            done: receiver,
        }
    }

    fn agent(&self) -> &Harness {
        self.harness.as_ref().unwrap()
    }

    fn finish(mut self) -> (VecDeque<Turn>, String) {
        drop(self.harness.take());
        self.done
            .recv_timeout(Duration::from_secs(4))
            .unwrap()
            .unwrap()
    }
}

struct Server {
    url: String,
    done: mpsc::Receiver<Vec<Value>>,
    thread: JoinHandle<()>,
}

impl Server {
    fn new(
        count: usize,
        mut reply: impl FnMut(usize, &Value, &mut TcpStream) + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
        let (done, receiver) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..count {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "model request {index} never arrived"
                            );
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("accept model request: {error}"),
                    }
                };
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
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                let body = serde_json::from_slice(&body).unwrap();
                reply(index, &body, &mut stream);
                requests.push(body);
            }
            done.send(requests).unwrap();
        });
        Self {
            url,
            done: receiver,
            thread,
        }
    }

    fn finish(self) -> Vec<Value> {
        let requests = self.done.recv_timeout(Duration::from_secs(6)).unwrap();
        self.thread.join().unwrap();
        requests
    }
}

fn response(text: &str, calls: &[(&str, &str, Value)]) -> Vec<Value> {
    let mut events = vec![json!({"type":"message_start","message":{"role":"assistant"}})];
    let mut index = 0;
    if !text.is_empty() {
        events.extend([
            json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}),
            json!({"type":"content_block_stop","index":index}),
        ]);
        index += 1;
    }
    for (id, name, input) in calls {
        let input = input.to_string();
        let middle = input
            .char_indices()
            .nth(input.chars().count() / 2)
            .map_or(0, |(i, _)| i);
        events.extend([
            json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
            json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":&input[..middle]}}),
            json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":&input[middle..]}}),
            json!({"type":"content_block_stop","index":index}),
        ]);
        index += 1;
    }
    events.extend([
        json!({"type":"message_delta","delta":{"stop_reason":if calls.is_empty(){"end_turn"}else{"tool_use"}}}),
        json!({"type":"message_stop"}),
    ]);
    events
}

fn sse(stream: &mut TcpStream, events: Vec<Value>) {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    let body = events
        .into_iter()
        .map(|event| format!("data: {event}\r\n\r\n"))
        .collect::<String>();
    for chunk in body.as_bytes().chunks(23) {
        stream.write_all(chunk).unwrap();
    }
}

fn settled(agent: &Harness, mut terminal: impl FnMut(TerminalRequest)) -> Vec<Event> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut events = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "agent did not settle");
        wait(agent.fd(), 100).unwrap();
        let mut done = false;
        for event in agent.poll().unwrap() {
            if let Event::Terminal(request) = event {
                terminal(request);
            } else {
                done |= matches!(event, Event::Settled);
                events.push(event);
            }
        }
        if done {
            return events;
        }
    }
}

fn paired(messages: &[Value]) {
    let mut pending = Vec::new();
    for message in messages {
        if message["role"] == "assistant" {
            assert!(pending.is_empty(), "tool_use without its next tool_result");
            for block in message["content"].as_array().unwrap() {
                if block["type"] == "tool_use" {
                    pending.push(block["id"].clone());
                }
            }
        } else if let Some(blocks) = message["content"].as_array() {
            let results: Vec<Value> = blocks
                .iter()
                .filter(|block| block["type"] == "tool_result")
                .map(|block| block["tool_use_id"].clone())
                .collect();
            assert_eq!(results, pending);
            pending.clear();
        } else {
            assert!(
                pending.is_empty(),
                "user input interrupted an unmatched tool_use"
            );
        }
    }
    assert!(pending.is_empty(), "history ends with unmatched tool_use");
}
#[test]
fn shell_input_and_history_reads_share_the_target_and_preserve_later_turns() {
    let server = Server::new(5, |index, body, stream| {
        assert_eq!(body["model"], "local-fixture");
        paired(body["messages"].as_array().unwrap());
        let names: Vec<_> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["shell", "input", "read"]);
        sse(
            stream,
            match index {
                0 => response(
                    "I will inspect this shell.",
                    &[("submit", "shell", json!({"command":"printf 'same-shell'"}))],
                ),
                1 => {
                    let content = body["messages"].as_array().unwrap().last().unwrap()["content"]
                        [0]["content"]
                        .as_str()
                        .unwrap();
                    let result: Value = serde_json::from_str(content).unwrap();
                    assert_eq!(result["command"], 7);
                    response(
                        "",
                        &[(
                            "output",
                            "read",
                            json!({"source":"command","command":7,"wait":true}),
                        )],
                    )
                }
                2 => {
                    assert!(body["messages"].to_string().contains("same-shell"));
                    response(
                        "",
                        &[
                            ("interactive", "input", json!({"text":"bt\n"})),
                            (
                                "unavailable",
                                "run_command",
                                json!({"command":"touch should-not-run"}),
                            ),
                        ],
                    )
                }
                3 => {
                    let messages = body["messages"].as_array().unwrap();
                    let results = messages.last().unwrap()["content"].as_array().unwrap();
                    assert_eq!(results[1]["is_error"], true);
                    response("Observed the requested result.", &[])
                }
                4 => {
                    let history = body["messages"].to_string();
                    assert!(history.contains("Observed the requested result."));
                    assert!(history.contains("next request"));
                    response("Next turn works.", &[])
                }
                _ => unreachable!(),
            },
        );
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent
        .send(
            "Inspect current shell".into(),
            17,
            json!({"cwd":"/project"}),
        )
        .unwrap();
    let mut calls = Vec::new();
    let events = settled(agent, |request| {
        assert_eq!(request.input["pane"], 17);
        calls.push(request.name.clone());
        let text = match request.name.as_str() {
            "shell" => json!({"command":7,"state":"running"}).to_string(),
            "read" => json!({"command":7,"exit_code":0,"text":"same-shell","next_cursor":null})
                .to_string(),
            "input" => json!({"screen":"#0 main"}).to_string(),
            name => panic!("unexpected terminal tool {name}"),
        };
        request.respond(Ok(text)).unwrap();
    });
    assert_eq!(calls, ["shell", "read", "input"]);
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    agent.send("next request".into(), 17, json!({})).unwrap();
    let events = settled(agent, |_| panic!("no further terminal operation"));
    assert!(
        events
            .iter()
            .any(|event| matches!(event,Event::Text(text) if text == "Next turn works."))
    );
    let (history, summary) = worker.finish();
    assert_eq!(history.len(), 2);
    assert!(summary.is_empty());
    for turn in history {
        paired(&turn.messages);
    }
    server.finish();
}

#[test]
fn abort_rewind_and_drop_cancel_waits_without_sending_terminal_input() {
    for action in ["abort", "rewind", "drop"] {
        let server = Server::new(
            if action == "drop" { 1 } else { 2 },
            |index, body, stream| {
                paired(body["messages"].as_array().unwrap());
                sse(
                    stream,
                    if index == 0 {
                        response(
                            "",
                            &[(
                                "waiting",
                                "read",
                                json!({"source":"command","command":7,"wait":true}),
                            )],
                        )
                    } else {
                        response("Recovered.", &[])
                    },
                );
            },
        );
        let worker = Worker::new(server.url.clone());
        let agent = worker.agent();
        let turn = agent
            .send("wait for command".into(), 17, json!({}))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let pending = loop {
            assert!(Instant::now() < deadline);
            wait(agent.fd(), 50).unwrap();
            if let Some(request) = agent
                .poll()
                .unwrap()
                .into_iter()
                .find_map(|event| match event {
                    Event::Terminal(request) => Some(request),
                    _ => None,
                })
            {
                break request;
            }
        };
        assert_eq!(pending.name, "read");
        let started = Instant::now();
        if action == "drop" {
            worker.finish();
            assert!(pending.canceled.load(Ordering::Acquire));
            pending.respond(Ok("late completion".into())).unwrap();
        } else {
            if action == "abort" {
                agent.abort().unwrap();
                settled(agent, |_| panic!("cancel issued another terminal action"));
            } else {
                agent.rewind(turn).unwrap();
                let mut rewound = false;
                while !rewound {
                    assert!(Instant::now() < deadline);
                    wait(agent.fd(), 50).unwrap();
                    for event in agent.poll().unwrap() {
                        match event {
                            Event::Rewound { turn: id, error } => {
                                assert_eq!(id, turn);
                                assert!(error.is_none());
                                rewound = true;
                            }
                            Event::Terminal(_) => panic!("rewind issued another terminal action"),
                            _ => (),
                        }
                    }
                }
            }
            assert!(pending.canceled.load(Ordering::Acquire));
            pending.respond(Ok("late completion".into())).unwrap();
            agent.send("resume".into(), 17, json!({})).unwrap();
            let events = settled(agent, |_| panic!("old wait revived"));
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event,Event::Text(text) if text == "Recovered."))
            );
            let (history, _) = worker.finish();
            assert_eq!(history.len(), if action == "rewind" { 1 } else { 2 });
            for turn in history {
                paired(&turn.messages);
            }
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        server.finish();
    }
}

#[test]
fn incomplete_streams_and_terminal_errors_do_not_poison_the_next_turn() {
    let server = Server::new(3, |index, body, stream| {
        paired(body["messages"].as_array().unwrap());
        if index == 0 {
            let mut events = response(
                "Partial response.",
                &[("partial", "shell", json!({"command":"must-not-run"}))],
            );
            events.truncate(events.len() - 2);
            sse(stream, events);
        } else if index == 1 {
            assert!(body["messages"].to_string().contains("Turn interrupted"));
            sse(
                stream,
                response(
                    "",
                    &[("busy", "shell", json!({"command":"printf expected"}))],
                ),
            );
        } else {
            let results = body["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_array()
                .unwrap();
            assert_eq!(results[0]["is_error"], true);
            assert!(
                results[0]["content"]
                    .as_str()
                    .unwrap()
                    .contains("Shell is busy")
            );
            sse(stream, response("The current shell is busy.", &[]));
        }
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent.send("first request".into(), 17, json!({})).unwrap();
    let events = settled(agent, |_| panic!("incomplete tool call executed"));
    assert!(events.iter().any(|event| matches!(event, Event::Error(_))));
    agent.send("next request".into(), 17, json!({})).unwrap();
    let events = settled(agent, |request| {
        request
            .respond(Err(io::Error::other("Shell is busy")))
            .unwrap();
    });
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    for turn in worker.finish().0 {
        paired(&turn.messages);
    }
    server.finish();
}

#[test]
fn long_work_summarizes_complete_exchanges_privately_and_rolls_old_turns_forward() {
    let mut summaries = 0;
    let mut rounds = 0;
    let server = Server::new(12, move |_, body, stream| {
        let messages = body["messages"].as_array().unwrap();
        paired(messages);
        if body["tool_choice"]["type"] == "none" {
            summaries += 1;
            assert!(json!(messages).to_string().contains("original objective"));
            sse(
                stream,
                response(
                    &format!(
                        "PRIVATE SUMMARY {summaries}: original objective; command 7 remains relevant."
                    ),
                    &[],
                ),
            );
        } else {
            assert!(serde_json::to_vec(messages).unwrap().len() <= CONTEXT_LIMIT);
            let events = if rounds < 8 {
                let id = format!("read-{rounds}");
                response(
                    "",
                    &[(
                        &id,
                        "read",
                        json!({"source":"command","command":7,"cursor":{"epoch":1,"row":rounds,"offset":0}}),
                    )],
                )
            } else if rounds == 8 {
                assert_eq!(summaries, 1);
                assert!(json!(messages).to_string().contains("PRIVATE SUMMARY 1"));
                response("Long request complete.", &[])
            } else {
                assert_eq!(summaries, 2);
                assert!(json!(messages).to_string().contains("PRIVATE SUMMARY 2"));
                assert!(json!(messages).to_string().contains("new constraint"));
                response("Continued from the historical summary.", &[])
            };
            rounds += 1;
            sse(stream, events);
        }
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent
        .send(
            "original objective: inspect command 7".into(),
            17,
            json!({}),
        )
        .unwrap();
    let events = settled(agent, |request| {
        request.respond(Ok("x".repeat(60_000))).unwrap();
    });
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    assert!(events.iter().any(|event| matches!(event, Event::Notice(_))));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event,Event::Text(text) if text.contains("PRIVATE SUMMARY")))
    );
    agent
        .send(
            format!("new constraint {}", "n".repeat(70_000)),
            17,
            json!({}),
        )
        .unwrap();
    let events = settled(agent, |_| panic!("summary cannot execute a tool"));
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    let (history, summary) = worker.finish();
    assert_eq!(history.len(), 1);
    assert!(summary.contains("PRIVATE SUMMARY 2"));
    paired(&history[0].messages);
    assert_eq!(server.finish().len(), 12);
}
