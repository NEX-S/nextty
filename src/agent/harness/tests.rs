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
            wake: Arc::new(crate::eventfd::new().unwrap()),
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
                let body: Value = serde_json::from_slice(&body).unwrap();
                if body["tool_choice"]["type"] == "none" {
                    assert!(body.get("cache_control").is_none());
                } else {
                    assert_eq!(body["cache_control"], json!({"type":"ephemeral"}));
                }
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
        let mut input = input.clone();
        if input["source"] != "workspace" && input.get("pane").is_none() {
            input["pane"] = json!(17);
        }
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

fn goal_response(status: &str, reason: &str) -> Vec<Value> {
    response(&json!({"status":status,"reason":reason}).to_string(), &[])
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

fn truncated_chunked_sse(stream: &mut TcpStream, events: Vec<Value>) {
    let body = events
        .into_iter()
        .map(|event| format!("data: {event}\r\n\r\n"))
        .collect::<String>();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n",
        body.len()
    )
    .unwrap();
}

fn settled(agent: &Harness, mut terminal: impl FnMut(ToolRequest)) -> Vec<Event> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut events = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "agent did not settle");
        wait(agent.fd(), 100).unwrap();
        let mut done = false;
        for event in agent.poll() {
            if let Event::Request(request) = event {
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
fn completed_terminal_result_precedes_cancellation() {
    let (tx, rx) = mpsc::sync_channel(1);
    let wake = crate::eventfd::new().unwrap();
    let canceled = AtomicBool::new(false);
    tx.send(Ok("unique executed result".into())).unwrap();
    canceled.store(true, Ordering::Release);
    assert_eq!(
        receive_tool(rx, wake.as_raw_fd(), &canceled).unwrap(),
        "unique executed result"
    );
    let (_tx, rx) = mpsc::sync_channel(1);
    let error = receive_tool(rx, wake.as_raw_fd(), &canceled).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(error.to_string(), "Stopped by user");
}

#[test]
fn completed_tool_result_survives_a_full_event_queue() {
    let server = Server::new(3, |index, body, stream| {
        paired(body["messages"].as_array().unwrap());
        if index == 2 {
            sse(stream, goal_response("complete", "Recovered."));
            return;
        }
        if index == 0 {
            sse(
                stream,
                response(
                    "",
                    &[
                        ("completed", "shell", json!({"command":"one side effect"})),
                        ("unstarted", "input", json!({"text":"must not run\n"})),
                    ],
                ),
            );
        } else {
            let messages = body["messages"].as_array().unwrap();
            let results = messages
                .iter()
                .find_map(|message| {
                    message["content"].as_array().filter(|blocks| {
                        blocks
                            .first()
                            .is_some_and(|block| block["type"] == "tool_result")
                    })
                })
                .unwrap();
            assert_eq!(results[0]["content"], "unique executed result");
            assert_eq!(results[0]["is_error"], false);
            assert_eq!(results[1]["tool_use_id"], "unstarted");
            assert_eq!(results[1]["is_error"], true);
            sse(stream, response("Recovered after backpressure.", &[]));
        }
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent.send("execute once".into(), 17, json!({})).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let request = loop {
        assert!(Instant::now() < deadline);
        wait(agent.fd(), 50).unwrap();
        if let Some(request) = agent.poll().into_iter().find_map(|event| {
            if let Event::Request(request) = event {
                Some(request)
            } else {
                None
            }
        }) {
            break request;
        }
    };
    assert_eq!(request.tool, Tool::Shell);
    for _ in 0..1024 {
        emit(&agent.shared, Event::Notice(String::new())).unwrap();
    }
    request
        .respond(Ok("unique executed result".into()))
        .unwrap();
    while agent.shared.state.lock().unwrap().active.is_some()
        && !agent.worker.as_ref().unwrap().is_finished()
    {
        assert!(
            Instant::now() < deadline,
            "turn did not settle under backpressure"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        !agent.worker.as_ref().unwrap().is_finished(),
        "backpressure killed the session"
    );
    let events = agent.poll();
    assert!(events.iter().any(|event| matches!(event, Event::Settled)));
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, Event::Request(_)))
    );
    agent.send("continue".into(), 17, json!({})).unwrap();
    let events = settled(agent, |_| panic!("unstarted tool was replayed"));
    assert!(events.iter().any(
        |event| matches!(event, Event::Text(text) if text == "Recovered after backpressure.")
    ));
    for turn in worker.finish().0 {
        paired(&turn.messages);
    }
    server.finish();
}

#[test]
fn shell_input_and_history_reads_share_the_target_and_preserve_later_turns() {
    let server = Server::new(7, |index, body, stream| {
        assert_eq!(body["model"], "local-fixture");
        paired(body["messages"].as_array().unwrap());
        let names: Vec<_> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "shell",
                "input",
                "read",
                "open_terminal",
                "read_file",
                "search_files",
                "edit_file"
            ]
        );
        for branch in body["tools"][2]["input_schema"]["anyOf"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
        {
            assert_eq!(branch["properties"]["wait"]["maximum"], MAX_READ_WAIT);
        }
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
                            json!({"source":"command","command":7,"wait":5}),
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
                4 | 6 => goal_response("complete", "Verified."),
                5 => {
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
        assert_eq!(request.pane, 17);
        calls.push(request.tool);
        let text = match request.tool {
            Tool::Shell => json!({"command":7,"state":"running"}).to_string(),
            Tool::Read => json!({"command":7,"exit_code":0,"text":"same-shell","cursor":{"epoch":1,"row":1,"offset":10},"has_more":false,"incomplete":false})
                .to_string(),
            Tool::Input => json!({"screen":"#0 main"}).to_string(),
            _ => panic!("unexpected workspace tool"),
        };
        request.respond(Ok(text)).unwrap();
    });
    assert_eq!(calls, [Tool::Shell, Tool::Read, Tool::Input]);
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
            if action == "drop" { 1 } else { 3 },
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
                                json!({"source":"command","command":7,"wait":1800}),
                            )],
                        )
                    } else if index == 1 {
                        response("Recovered.", &[])
                    } else {
                        goal_response("complete", "Recovered.")
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
            if let Some(request) = agent.poll().into_iter().find_map(|event| match event {
                Event::Request(request) => Some(request),
                _ => None,
            }) {
                break request;
            }
        };
        assert_eq!(pending.tool, Tool::Read);
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
                    for event in agent.poll() {
                        match event {
                            Event::Rewound { turn: id, error } => {
                                assert_eq!(id, turn);
                                assert!(error.is_none());
                                rewound = true;
                            }
                            Event::Request(_) => panic!("rewind issued another terminal action"),
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
fn truncated_model_response_retries_without_replaying_completed_or_partial_tools() {
    let mut interrupted = None;
    let mut review = None;
    let server = Server::new(6, move |index, body, stream| {
        paired(body["messages"].as_array().unwrap());
        match index {
            0 => sse(
                stream,
                response("", &[("first", "shell", json!({"command":"first"}))]),
            ),
            1 => {
                assert!(body["messages"].to_string().contains("first-completed"));
                interrupted = Some(body.clone());
                let mut events = response(
                    "Discarded partial response.",
                    &[("partial", "shell", json!({"command":"must-not-run"}))],
                );
                events.pop();
                truncated_chunked_sse(stream, events);
            }
            2 => {
                assert_eq!(Some(body), interrupted.as_ref());
                sse(
                    stream,
                    response("", &[("second", "shell", json!({"command":"second"}))]),
                );
            }
            3 => {
                let text = body["messages"].to_string();
                assert!(text.contains("first-completed") && text.contains("second-completed"));
                assert!(
                    !text.contains("Discarded partial response") && !text.contains("must-not-run")
                );
                sse(stream, response("Both actions verified.", &[]));
            }
            4 => {
                review = Some(body.clone());
                stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 8\r\nConnection: close\r\n\r\ntrylater").unwrap();
            }
            5 => {
                assert_eq!(Some(body), review.as_ref());
                sse(stream, goal_response("complete", "Both actions verified."));
            }
            _ => unreachable!(),
        }
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent
        .send("perform two actions".into(), 17, json!({}))
        .unwrap();
    let mut commands = Vec::new();
    let events = settled(agent, |request| {
        let command = request.input["command"].as_str().unwrap().to_owned();
        request.respond(Ok(format!("{command}-completed"))).unwrap();
        commands.push(command);
    });
    assert_eq!(commands, ["first", "second"]);
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::ResetText))
            .count(),
        1
    );
    let (history, _) = worker.finish();
    paired(&history[0].messages);
    server.finish();
}

#[test]
fn completed_model_message_does_not_wait_for_a_missing_http_chunk_trailer() {
    for hold_open in [false, true] {
        let server = Server::new(3, move |index, body, stream| match index {
            0 => {
                truncated_chunked_sse(
                    stream,
                    response("", &[("once", "shell", json!({"command":"once"}))]),
                );
                if hold_open {
                    let mut byte = [0];
                    // Closing with unread HTTP framing may send a reset instead of EOF.
                    match stream.read(&mut byte) {
                        Ok(0) => (),
                        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => (),
                        result => panic!("expected peer closure, got {result:?}"),
                    }
                }
            }
            1 => {
                assert!(body["messages"].to_string().contains("once-completed"));
                sse(stream, response("Verified.", &[]));
            }
            2 => sse(stream, goal_response("complete", "Verified.")),
            _ => unreachable!(),
        });
        let worker = Worker::new(server.url.clone());
        let agent = worker.agent();
        agent
            .send("perform one action".into(), 17, json!({}))
            .unwrap();
        let mut calls = 0;
        let events = settled(agent, |request| {
            assert_eq!(request.input["command"], "once");
            calls += 1;
            request.respond(Ok("once-completed".into())).unwrap();
        });
        assert_eq!(calls, 1);
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, Event::Error(_) | Event::ResetText))
        );
        worker.finish();
        server.finish();
    }
}

#[test]
fn stopping_during_network_backoff_prevents_another_request_or_tool() {
    let server = Server::new(1, |_, _, stream| {
        let mut events = response("Partial.", &[]);
        events.pop();
        truncated_chunked_sse(stream, events);
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent
        .send("wait for a response".into(), 17, json!({}))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < deadline, "retry notice never arrived");
        wait(agent.fd(), 100).unwrap();
        let events = agent.poll();
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, Event::Request(_)))
        );
        if events
            .iter()
            .any(|event| matches!(event, Event::Notice(text) if text.contains("Retrying")))
        {
            break;
        }
    }
    let start = Instant::now();
    agent.abort().unwrap();
    let events = settled(agent, |_| panic!("tool executed after cancellation"));
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    worker.finish();
    server.finish();
}

#[test]
fn summary_network_recovery_is_bounded_and_keeps_the_request() {
    let mut original = None;
    let server = Server::new(6, move |index, body, stream| {
        if let Some(original) = &original {
            assert_eq!(body, original);
        } else {
            original = Some(body.clone());
        }
        if index < 4 {
            stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 8\r\nConnection: close\r\n\r\ntrylater").unwrap();
        } else if index == 4 {
            sse(
                stream,
                vec![json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}})],
            );
        } else {
            sse(stream, response("Recovered summary.", &[]));
        }
    });
    let shared = Shared {
        state: Mutex::new(State::default()),
        wake: Arc::new(crate::eventfd::new().unwrap()),
        notification: crate::eventfd::new().unwrap(),
    };
    let mut session = Session {
        http: Http::new(),
        url: server.url.clone(),
        headers: vec![],
        model: "local-fixture".into(),
        history: VecDeque::new(),
        summary: String::new(),
    };
    let messages = [json!({"role":"user","content":"Keep completed command 123."})];
    let body = session.summary_request(&messages).unwrap();
    let canceled = AtomicBool::new(false);
    let error = session.summarize(&shared, &body, &canceled).unwrap_err();
    assert!(error.to_string().contains("503"));
    assert_eq!(
        session.summarize(&shared, &body, &canceled).unwrap(),
        "Recovered summary."
    );
    server.finish();
}

#[test]
fn malformed_streams_and_terminal_errors_do_not_poison_the_next_turn() {
    let server = Server::new(4, |index, body, stream| {
        paired(body["messages"].as_array().unwrap());
        if index == 0 {
            let mut events = response(
                "Partial response.",
                &[("partial", "shell", json!({"command":"must-not-run"}))],
            );
            events.truncate(events.len() - 2);
            sse(stream, events);
            stream.write_all(b"data: {\"type\":\r\n\r\n").unwrap();
        } else if index == 1 {
            assert!(body["messages"].to_string().contains("Turn interrupted"));
            sse(
                stream,
                response(
                    "",
                    &[("busy", "shell", json!({"command":"printf expected"}))],
                ),
            );
        } else if index == 2 {
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
        } else {
            sse(
                stream,
                goal_response("blocked", "The current shell is busy."),
            );
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
fn output_limit_never_executes_even_a_parseable_tool_call() {
    let server = Server::new(3, |index, body, stream| {
        if index == 0 {
            let mut events = response(
                "",
                &[("limited", "shell", json!({"command":"must-not-run"}))],
            );
            let stop = events.len() - 2;
            events[stop]["delta"]["stop_reason"] = json!("max_tokens");
            sse(stream, events);
        } else if index == 1 {
            paired(body["messages"].as_array().unwrap());
            assert!(body["messages"].to_string().contains("Result unavailable"));
            sse(
                stream,
                response("The previous action was not executed.", &[]),
            );
        } else {
            sse(
                stream,
                goal_response("complete", "Explained the interruption."),
            );
        }
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent.send("first request".into(), 17, json!({})).unwrap();
    let events = settled(agent, |_| panic!("output-limited tool call executed"));
    assert!(events.iter().any(|event| matches!(event, Event::Error(_))));
    agent.send("continue".into(), 17, json!({})).unwrap();
    let events = settled(agent, |_| panic!("unexpected tool call"));
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    worker.finish();
    server.finish();
}

#[test]
fn long_work_summarizes_complete_exchanges_privately_and_rolls_old_turns_forward() {
    let mut summaries = 0;
    let mut rounds = 0;
    let server = Server::new(14, move |_, body, stream| {
        let messages = body["messages"].as_array().unwrap();
        paired(messages);
        if body["system"] == GOAL_REVIEW {
            sse(stream, goal_response("complete", "Verified."));
            return;
        }
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
    assert_eq!(server.finish().len(), 14);
}

#[test]
fn repeated_compaction_keeps_the_request_when_summaries_omit_it() {
    let objective = "检查 command 7；保留用户草稿；最终停在调试器，不要退出。";
    let mut summaries = 0;
    let mut rounds = 0;
    let server = Server::new(24, move |_, body, stream| {
        let messages = body["messages"].as_array().unwrap();
        let text = serde_json::to_string(messages).unwrap();
        paired(messages);
        assert!(messages[0]["content"].as_str().unwrap().contains(objective));
        if body["system"] == GOAL_REVIEW {
            assert!(text.len() <= CONTEXT_LIMIT);
            sse(stream, goal_response("complete", "Verified."));
            return;
        }
        if body["tool_choice"]["type"] == "none" {
            summaries += 1;
            if summaries > 1 {
                assert!(text.contains(&format!(
                    "Historical result {}: command 7 remains running.",
                    summaries - 1
                )));
            }
            sse(
                stream,
                response(
                    &format!("Historical result {summaries}: command 7 remains running."),
                    &[],
                ),
            );
        } else {
            assert!(text.len() <= CONTEXT_LIMIT);
            if rounds > 0 {
                let result = &messages.last().unwrap()["content"][0];
                assert_eq!(result["tool_use_id"], format!("read-{}", rounds - 1));
                assert!(
                    result["content"]
                        .as_str()
                        .unwrap()
                        .starts_with(&format!("Observation {}:", rounds - 1))
                );
            }
            let events = if rounds < 18 {
                response(
                    "",
                    &[(
                        &format!("read-{rounds}"),
                        "read",
                        json!({"source":"command","command":7}),
                    )],
                )
            } else {
                assert_eq!(summaries, 4);
                response("Stopped at the requested debugger state.", &[])
            };
            rounds += 1;
            sse(stream, events);
        }
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent.send(objective.into(), 17, json!({})).unwrap();
    let mut observations = 0;
    let events = settled(agent, |request| {
        let text = format!("Observation {observations}: {}", "x".repeat(60_000));
        observations += 1;
        request.respond(Ok(text)).unwrap();
    });
    assert_eq!(observations, 18);
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Notice(_)))
            .count(),
        4
    );
    let (history, _) = worker.finish();
    assert_eq!(history.len(), 1);
    paired(&history[0].messages);
    assert_eq!(server.finish().len(), 24);
}

#[test]
fn goal_review_compaction_keeps_the_latest_tool_result_as_original_evidence() {
    let server = Server::new(7, |index, body, stream| {
        let messages = body["messages"].as_array().unwrap();
        paired(messages);
        if index < 4 {
            sse(
                stream,
                response(
                    "",
                    &[(
                        &format!("read-{index}"),
                        "read",
                        json!({"source":"command","command":7}),
                    )],
                ),
            );
        } else if index == 4 {
            sse(stream, response(&"Final analysis. ".repeat(2000), &[]));
        } else if index == 5 {
            assert_eq!(body["tool_choice"]["type"], "none");
            assert_ne!(body["system"], GOAL_REVIEW);
            sse(
                stream,
                response("Historical work before the latest verification.", &[]),
            );
        } else {
            assert_eq!(body["system"], GOAL_REVIEW);
            assert!(serde_json::to_vec(messages).unwrap().len() <= CONTEXT_LIMIT);
            let result = messages
                .iter()
                .filter_map(|message| message["content"].as_array())
                .flatten()
                .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == "read-3")
                .expect("goal review lost the latest original tool result");
            assert!(
                result["content"]
                    .as_str()
                    .unwrap()
                    .starts_with("verified-command-7-exit-0:")
            );
            sse(
                stream,
                goal_response(
                    "complete",
                    "The original verification result supports completion.",
                ),
            );
        }
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent
        .send("analyze and verify command 7".into(), 17, json!({}))
        .unwrap();
    let mut calls = 0;
    let events = settled(agent, |request| {
        let label = if calls == 3 {
            "verified-command-7-exit-0:"
        } else {
            "historical-output:"
        };
        calls += 1;
        request
            .respond(Ok(format!("{label}{}", "x".repeat(60_000))))
            .unwrap();
    });
    assert_eq!(calls, 4);
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    let (history, _) = worker.finish();
    paired(&history[0].messages);
    server.finish();
}

#[test]
fn failed_compaction_preserves_executed_results_for_the_next_turn() {
    let server = Server::new(9, |index, body, stream| {
        let messages = body["messages"].as_array().unwrap();
        let text = serde_json::to_string(messages).unwrap();
        paired(messages);
        if index < 5 {
            sse(
                stream,
                response(
                    "",
                    &[(
                        &format!("action-{index}"),
                        "shell",
                        json!({"command":format!("action-{index}")}),
                    )],
                ),
            );
        } else if index < 7 {
            assert_eq!(body["tool_choice"]["type"], "none");
            for action in 0..if index == 5 { 4 } else { 5 } {
                assert!(text.contains(&format!("executed-{action}")));
            }
            if index == 5 {
                stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 8\r\nConnection: close\r\n\r\nrejected").unwrap();
            } else {
                sse(stream, response("All five actions already executed.", &[]));
            }
        } else if index == 7 {
            assert!(text.len() <= CONTEXT_LIMIT);
            assert!(text.contains("All five actions already executed."));
            assert!(text.contains("continue without repeating actions"));
            sse(
                stream,
                response("Recovered without replaying actions.", &[]),
            );
        } else {
            sse(stream, goal_response("complete", "Recovered."));
        }
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent
        .send("perform five actions".into(), 17, json!({}))
        .unwrap();
    let mut actions = 0;
    let events = settled(agent, |request| {
        request
            .respond(Ok(format!("executed-{actions}: {}", "x".repeat(60_000))))
            .unwrap();
        actions += 1;
    });
    assert_eq!(actions, 5);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Error(text) if text.contains("400")))
    );
    agent
        .send("continue without repeating actions".into(), 17, json!({}))
        .unwrap();
    let events = settled(agent, |_| panic!("completed action was replayed"));
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    let (history, summary) = worker.finish();
    assert!(summary.contains("All five actions already executed."));
    assert_eq!(history.len(), 1);
    paired(&history[0].messages);
    server.finish();
}

#[test]
fn default_goal_continues_after_a_premature_final_and_verifies_the_result() {
    let server = Server::new(5, |index, body, stream| {
        let messages = body["messages"].as_array().unwrap();
        paired(messages);
        assert!(json!(messages).to_string().contains("verify command 7"));
        sse(
            stream,
            match index {
                0 => response("The task is finished.", &[]),
                1 => {
                    assert_eq!(body["system"], GOAL_REVIEW);
                    assert_eq!(body["tool_choice"]["type"], "none");
                    goal_response("continue", "Read command 7 and verify its exit status.")
                }
                2 => {
                    assert!(
                        messages.last().unwrap()["content"]
                            .as_str()
                            .unwrap()
                            .contains("Read command 7")
                    );
                    response(
                        "",
                        &[("verify", "read", json!({"source":"command","command":7}))],
                    )
                }
                3 => {
                    assert!(json!(messages).to_string().contains("verified-result"));
                    response("Verified the requested result.", &[])
                }
                4 => {
                    assert_eq!(body["system"], GOAL_REVIEW);
                    assert!(json!(messages).to_string().contains("verified-result"));
                    goal_response(
                        "complete",
                        "Command 7 exited successfully with the requested result.",
                    )
                }
                _ => unreachable!(),
            },
        );
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent
        .send("verify command 7".into(), 17, json!({}))
        .unwrap();
    let mut calls = 0;
    let events = settled(agent, |request| {
        calls += 1;
        assert_eq!(request.tool, Tool::Read);
        request
            .respond(Ok(
                json!({"exit_code":0,"text":"verified-result"}).to_string()
            ))
            .unwrap();
    });
    assert_eq!(calls, 1);
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Notice(text) if text.contains("Read command 7")))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Text(text) if text.contains("\"status\"")))
    );
    let (history, _) = worker.finish();
    assert_eq!(history.len(), 1);
    paired(&history[0].messages);
    assert!(
        !json!(history[0].messages)
            .to_string()
            .contains("Harness goal review: the active request")
    );
    server.finish();
}

#[test]
fn blocked_goal_reports_the_missing_input_and_accepts_the_next_submission() {
    let server = Server::new(4, |index, body, stream| {
        sse(
            stream,
            match index {
                0 => response("Which directory should I use?", &[]),
                1 => goal_response("blocked", "Please provide the target directory."),
                2 => {
                    assert!(
                        body["messages"]
                            .to_string()
                            .contains("Goal blocked: Please provide")
                    );
                    assert!(body["messages"].to_string().contains("Use /project"));
                    response("The supplied directory is /project.", &[])
                }
                3 => goal_response("complete", "Answered the user's question."),
                _ => unreachable!(),
            },
        );
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent
        .send("What is my target directory?".into(), 17, json!({}))
        .unwrap();
    let events = settled(agent, |_| {
        panic!("missing input must not cause terminal actions")
    });
    assert!(events.iter().any(|event| matches!(event, Event::Blocked(text) if text.contains("provide the target directory"))));
    agent.send("Use /project".into(), 17, json!({})).unwrap();
    let events = settled(agent, |_| {
        panic!("an answer does not require a terminal action")
    });
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    assert_eq!(worker.finish().0.len(), 2);
    server.finish();
}

#[test]
fn stopping_an_idle_goal_review_closes_the_request_without_more_actions() {
    let (started, review_started) = mpsc::sync_channel(1);
    let server = Server::new(2, move |index, body, stream| {
        if index == 0 {
            sse(stream, response("Candidate answer.", &[]));
        } else {
            assert_eq!(body["system"], GOAL_REVIEW);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
            started.send(()).unwrap();
            match stream.read(&mut [0]) {
                Ok(0) => (),
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => (),
                result => panic!("canceled connection remained open: {result:?}"),
            }
        }
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent
        .send("answer this request".into(), 17, json!({}))
        .unwrap();
    review_started.recv_timeout(Duration::from_secs(3)).unwrap();
    let started = Instant::now();
    agent.abort().unwrap();
    let events = settled(agent, |_| panic!("a canceled review executed a tool"));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    let (history, _) = worker.finish();
    assert!(
        json!(history[0].messages)
            .to_string()
            .contains("Candidate answer.")
    );
    server.finish();
}

#[test]
fn malformed_goal_review_preserves_completed_actions_without_retrying_them() {
    let server = Server::new(5, |index, body, stream| {
        paired(body["messages"].as_array().unwrap());
        sse(
            stream,
            match index {
                0 => response(
                    "",
                    &[("once", "shell", json!({"command":"one side effect"}))],
                ),
                1 => response("The action finished.", &[]),
                2 => response("not JSON", &[]),
                3 => {
                    assert!(body["messages"].to_string().contains("executed-once"));
                    response("Recovered with the recorded result.", &[])
                }
                4 => goal_response("complete", "The recorded result satisfies the request."),
                _ => unreachable!(),
            },
        );
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent
        .send("perform the action".into(), 17, json!({}))
        .unwrap();
    let mut calls = 0;
    let events = settled(agent, |request| {
        calls += 1;
        request.respond(Ok("executed-once".into())).unwrap();
    });
    assert_eq!(calls, 1);
    assert!(events.iter().any(|event| matches!(event, Event::Error(_))));
    agent
        .send("continue using the existing result".into(), 17, json!({}))
        .unwrap();
    let events = settled(agent, |_| panic!("completed action was replayed"));
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    for turn in worker.finish().0 {
        paired(&turn.messages);
    }
    server.finish();
}

#[test]
fn workspace_tools_route_explicit_panes_and_keep_committed_results() {
    let server = Server::new(3, |index, body, stream| {
        paired(body["messages"].as_array().unwrap());
        match index {
            0 => sse(
                stream,
                response(
                    "",
                    &[
                        ("inventory", "read", json!({"source":"workspace"})),
                        ("screen", "read", json!({"pane":31,"source":"screen"})),
                        ("file", "read_file", json!({"pane":31,"path":"source.rs"})),
                        (
                            "search",
                            "search_files",
                            json!({"pane":31,"path":".","query":"broken"}),
                        ),
                        (
                            "edit",
                            "edit_file",
                            json!({"pane":31,"path":"source.rs","old_text":"broken","new_text":"fixed"}),
                        ),
                        ("open", "open_terminal", json!({"pane":31})),
                    ],
                ),
            ),
            1 => {
                let results = body["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_array()
                    .unwrap();
                assert_eq!(results.len(), 6);
                assert!(results.iter().all(|result| result["is_error"] == false));
                assert_eq!(
                    serde_json::from_str::<Value>(results[4]["content"].as_str().unwrap()).unwrap()
                        ["written"],
                    true
                );
                assert_eq!(
                    serde_json::from_str::<Value>(results[5]["content"].as_str().unwrap()).unwrap()
                        ["pane"],
                    32
                );
                sse(stream, response("The repair is verified.", &[]));
            }
            2 => sse(stream, goal_response("complete", "Verified.")),
            _ => unreachable!(),
        }
    });
    let worker = Worker::new(server.url.clone());
    worker
        .agent()
        .send("repair in another pane".into(), 17, json!({}))
        .unwrap();
    let mut calls = Vec::new();
    let mut edit_id = String::new();
    let events = settled(worker.agent(), |request| {
        calls.push((request.tool, request.pane));
        if request.tool == Tool::EditFile {
            edit_id.clone_from(&request.id);
        }
        let result = match request.tool {
            Tool::EditFile => json!({"path":"/project/source.rs","written":true}),
            Tool::OpenTerminal => json!({"created":true,"pane":32}),
            _ => json!({"text":"original evidence"}),
        };
        request.respond(Ok(result.to_string())).unwrap();
    });
    assert_eq!(
        calls,
        [
            (Tool::Read, 17),
            (Tool::Read, 31),
            (Tool::ReadFile, 31),
            (Tool::SearchFiles, 31),
            (Tool::EditFile, 31),
            (Tool::OpenTerminal, 31)
        ]
    );
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    assert!(!edit_id.is_empty());
    assert!(events.iter().any(|event| matches!(event,
        Event::Tool { id, name, done: true, .. } if name == "edit_file" && id == &edit_id)));
    worker.finish();
    server.finish();
}

fn terminal_client() -> (crate::workspace::Client, crate::wire::Peer) {
    let path = std::env::temp_dir().join(format!(
        "nextty-open-test-{}-{}.sock",
        std::process::id(),
        crate::agent::NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
    ));
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let client = crate::workspace::Client::connect(&path).unwrap();
    let (stream, _) = listener.accept().unwrap();
    std::fs::remove_file(path).unwrap();
    (client, crate::wire::Peer::new(stream).unwrap())
}

fn tool_request(
    tool: Tool,
    pane: u64,
    input: Value,
) -> (ToolRequest, mpsc::Receiver<io::Result<String>>) {
    let (reply, receiver) = mpsc::sync_channel(1);
    (
        ToolRequest {
            id: "test-edit".into(),
            tool,
            pane,
            input,
            canceled: Arc::new(AtomicBool::new(false)),
            reply,
            wake: Arc::new(crate::eventfd::new().unwrap()),
        },
        receiver,
    )
}

fn terminal_frame(
    client: &mut crate::workspace::Client,
    peer: &mut crate::wire::Peer,
    prompt: Option<u64>,
) {
    use crate::workspace::{Action, PaneView, Rect, Snapshot};
    client.request_screen().unwrap();
    client.flush().unwrap();
    let mut request = None;
    while let Some(action) = peer.next_action().unwrap() {
        if let Action::ScreenRequest { request: id, .. } = action {
            request = Some(id);
        }
    }
    let core = crate::core::Core::new(
        crate::core::WindowSize {
            num_cols: 20,
            num_lines: 4,
            cell_width: 0,
            cell_height: 0,
        },
        0,
        0,
    );
    peer.send_response(crate::wire::Response::State(
        Snapshot {
            name: "test".into(),
            controller: true,
            focused: 1,
            active_tab: 1,
            tabs: Vec::new(),
            panes: vec![PaneView {
                id: 7,
                tab: 7,
                rect: Rect {
                    x: 0,
                    y: 0,
                    cols: 20,
                    rows: 4,
                },
                title: "shell".into(),
                cwd: None,
                process: Arc::default(),
                blocks: Vec::new(),
                command_blocks: Vec::new(),
                input_generation: 4,
                shell_prompt: prompt,
            }],
        },
        vec![(7, crate::screen::Update::capture_live(&core))],
        request.unwrap(),
    ))
    .unwrap();
    peer.flush().unwrap();
    client.poll().unwrap();
    assert!(!client.screen_stale());
}

#[test]
fn new_terminal_waits_for_prompt_then_authorizes_shell_without_another_read() {
    use crate::agent::{Agent, Stage};
    use crate::workspace::{Action, ClientEvent};
    let (mut client, mut peer) = terminal_client();
    let mut agent = Agent::default();
    let (request, result) = tool_request(Tool::OpenTerminal, 1, json!({"pane":1}));
    agent.begin(request, &mut client).unwrap();
    let Stage::NewTerminal { id } = agent.operation.as_ref().unwrap().stage else {
        panic!()
    };
    agent
        .handle_event(
            &mut ClientEvent::AgentNewTab {
                request: id,
                result: Ok(7),
            },
            &mut client,
        )
        .unwrap();
    terminal_frame(&mut client, &mut peer, None);
    agent.advance(&mut client).unwrap();
    assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert!(agent.deadline(&client).is_some());
    terminal_frame(&mut client, &mut peer, Some(9));
    agent.advance(&mut client).unwrap();
    let value: Value = serde_json::from_str(&result.recv().unwrap().unwrap()).unwrap();
    assert_eq!(value["created"], true);
    assert_eq!(value["pane"], 7);
    assert_eq!(value["shell_prompt"], 9);
    assert!(value["screen"].is_object());
    assert_eq!(agent.observed_input.get(&7), Some(&4));
    assert!(!agent.observed_input.contains_key(&1));

    let (request, _result) = tool_request(Tool::Shell, 7, json!({"command":"printf verified"}));
    agent.begin_ready(request, &mut client).unwrap();
    client.flush().unwrap();
    let Some(Action::AgentInput {
        pane,
        generation,
        prompt,
        ..
    }) = peer.next_action().unwrap()
    else {
        panic!()
    };
    assert_eq!((pane, generation, prompt), (7, 4, Some(9)));
}

#[test]
fn new_terminal_timeout_does_not_invent_prompt_and_cancel_keeps_creation() {
    use crate::agent::{Agent, Stage};
    use crate::workspace::ClientEvent;
    for finish in ["timeout", "cancel", "close", "missing-screen"] {
        let (mut client, mut peer) = terminal_client();
        let mut agent = Agent::default();
        let (request, result) = tool_request(Tool::OpenTerminal, 1, json!({"pane":1}));
        agent.begin(request, &mut client).unwrap();
        let Stage::NewTerminal { id } = agent.operation.as_ref().unwrap().stage else {
            panic!()
        };
        agent
            .handle_event(
                &mut ClientEvent::AgentNewTab {
                    request: id,
                    result: Ok(7),
                },
                &mut client,
            )
            .unwrap();
        terminal_frame(&mut client, &mut peer, None);
        match finish {
            "timeout" => {
                agent.operation.as_mut().unwrap().stage = Stage::NewTerminalOutput {
                    until: Instant::now(),
                }
            }
            "cancel" => agent
                .operation
                .as_ref()
                .unwrap()
                .request
                .canceled
                .store(true, Ordering::Release),
            "close" => agent.close(&mut client).unwrap(),
            "missing-screen" => {
                client.snapshot_and_screens_mut().unwrap().1.clear();
                agent.operation.as_mut().unwrap().stage = Stage::NewTerminalOutput {
                    until: Instant::now(),
                };
            }
            _ => unreachable!(),
        }
        agent.advance(&mut client).unwrap();
        let value: Value = serde_json::from_str(&result.recv().unwrap().unwrap()).unwrap();
        assert_eq!(value["created"], true);
        assert_eq!(value["pane"], 7);
        if finish == "timeout" {
            assert_eq!(value["shell_prompt"], Value::Null);
            assert_eq!(value["outcome"], "timeout");
            let (request, result) =
                tool_request(Tool::Shell, 7, json!({"command":"printf too-early"}));
            agent.begin_ready(request, &mut client).unwrap();
            assert!(result.recv().unwrap().is_err());
        } else {
            assert!(value["observation_error"].is_string());
        }
    }
}

#[test]
fn committed_file_diff_keeps_its_tool_identity_without_expanding_model_result() {
    use crate::agent::{Agent, Operation, Stage};
    use crate::workspace::{AgentChange, ClientEvent, FileResult};
    let (mut client, _peer) = terminal_client();
    let mut agent = Agent {
        turn: 5,
        ..Agent::default()
    };
    let (request, result) = tool_request(
        Tool::EditFile,
        7,
        json!({"pane":7,"path":"a.rs","old_text":"old","new_text":"new"}),
    );
    agent.operation = Some(Operation {
        request,
        stage: Stage::File { id: 42 },
    });
    agent
        .handle_event(
            &mut ClientEvent::AgentFile {
                request: 42,
                result: Ok(FileResult::Edited {
                    change: AgentChange {
                        path: "a.rs".into(),
                        added: 1,
                        removed: 1,
                        diff: "@@ -1 +1 @@\n-old\n+new\n".into(),
                        ..AgentChange::default()
                    },
                }),
            },
            &mut client,
        )
        .unwrap();
    assert_eq!(agent.changes[0].tool_id, "test-edit");
    assert_eq!(agent.changes[0].turn, 5);
    let returned: Value = serde_json::from_str(&result.recv().unwrap().unwrap()).unwrap();
    assert_eq!(returned, json!({"path":"a.rs","written":true}));
}
