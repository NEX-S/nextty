use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

struct Worker {
    harness: Option<Harness>,
    done: mpsc::Receiver<io::Result<(VecDeque<Turn>, String)>>,
}

impl Worker {
    fn new(url: String) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Arc::new(crate::sys::new().unwrap()),
            notification: crate::sys::new().unwrap(),
        });
        let control = Arc::clone(&shared);
        let (done, receiver) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let mut session = session(url);
            let result = session.run(&shared);
            done.send(result.map(|()| (session.history, session.summary)))
                .unwrap();
        });
        let harness = Harness {
            shared: control,
            worker: Some(thread),
        };
        Self {
            harness: Some(harness),
            done: receiver,
        }
    }

    fn agent(&self) -> &Harness {
        self.harness.as_ref().unwrap()
    }

    fn finish(mut self) -> (VecDeque<Turn>, String) {
        drop(self.harness.take());
        let result = self.done.recv_timeout(Duration::from_secs(4)).unwrap();
        result.unwrap()
    }
}

fn session(url: String) -> Session {
    Session {
        http: Http::new(),
        url,
        headers: Vec::new(),
        model: "local-fixture".into(),
        history: VecDeque::new(),
        summary: String::new(),
    }
}

/// A model endpoint that answers `count` requests in order and returns their bodies.
struct Server {
    url: String,
    done: mpsc::Receiver<Vec<Value>>,
}

impl Server {
    fn new(
        count: usize,
        mut reply: impl FnMut(usize, &Value, &mut TcpStream) + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
        let (done, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                let body = crate::agent::http::read_request(&mut stream);
                let body: Value = serde_json::from_slice(&body).unwrap();
                // Only execution requests ask for prompt caching.
                let tools_disabled = body["tool_choice"]["type"] == "none";
                assert_eq!(body.get("cache_control").is_none(), tools_disabled);
                reply(index, &body, &mut stream);
                requests.push(body);
            }
            done.send(requests).unwrap();
        });
        Self {
            url,
            done: receiver,
        }
    }

    fn finish(self) -> Vec<Value> {
        let requests = self.done.recv_timeout(Duration::from_secs(10));
        requests.expect("the expected model requests never arrived")
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
        let (first, second) = input.split_at(input.floor_char_boundary(input.len() / 2));
        events.extend([
            json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
            json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":first}}),
            json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":second}}),
            json!({"type":"content_block_stop","index":index}),
        ]);
        index += 1;
    }
    let stop = if calls.is_empty() {
        "end_turn"
    } else {
        "tool_use"
    };
    events.extend([
        json!({"type":"message_delta","delta":{"stop_reason":stop}}),
        json!({"type":"message_stop"}),
    ]);
    events
}

fn goal_response(status: &str, reason: &str) -> Vec<Value> {
    response(&json!({"status":status,"reason":reason}).to_string(), &[])
}

fn sse_body(events: Vec<Value>) -> String {
    let events = events.into_iter();
    events
        .map(|event| format!("data: {event}\r\n\r\n"))
        .collect()
}

fn sse(stream: &mut TcpStream, events: Vec<Value>) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
    stream.write_all(head.as_bytes()).unwrap();
    for chunk in sse_body(events).as_bytes().chunks(23) {
        stream.write_all(chunk).unwrap();
    }
}

/// A chunked response whose final chunk never arrives.
fn truncated_chunked_sse(stream: &mut TcpStream, events: Vec<Value>) {
    let body = sse_body(events);
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n",
        body.len()
    )
    .unwrap();
}

fn unavailable(stream: &mut TcpStream) {
    let reply = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 8\r\nConnection: close\r\n\r\ntrylater";
    stream.write_all(reply.as_bytes()).unwrap();
}

/// Waits for the client to close the connection, which may also reset it.
fn closed(stream: &mut TcpStream) {
    match stream.read(&mut [0]) {
        Ok(0) => (),
        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => (),
        result => panic!("expected the client to close the connection, got {result:?}"),
    }
}

fn settled(agent: &Harness, mut terminal: impl FnMut(ToolRequest)) -> Vec<Event> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut events = Vec::new();
    while !events.iter().any(|event| matches!(event, Event::Settled)) {
        assert!(Instant::now() < deadline, "agent did not settle");
        wait(agent.fd(), 100).unwrap();
        for event in agent.poll() {
            match event {
                Event::Request(request) => terminal(request),
                event => events.push(event),
            }
        }
    }
    events
}

fn first_request(agent: &Harness) -> ToolRequest {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < deadline, "no tool request arrived");
        wait(agent.fd(), 50).unwrap();
        let request = agent.poll().into_iter().find_map(|event| match event {
            Event::Request(request) => Some(request),
            _ => None,
        });
        if let Some(request) = request {
            return request;
        }
    }
}

fn errored(events: &[Event]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, Event::Message("Error", _)))
}

fn notices(events: &[Event]) -> impl Iterator<Item = &str> {
    events.iter().filter_map(|event| match event {
        Event::Message("Context", text) => Some(text.as_str()),
        _ => None,
    })
}

/// Every tool_use is answered by the next message's tool_results, in order.
fn paired(messages: &[Value]) {
    let mut pending = Vec::new();
    for message in messages {
        let blocks = message["content"].as_array();
        if message["role"] == "assistant" {
            assert!(pending.is_empty(), "tool_use without its next tool_result");
            let calls = blocks
                .unwrap()
                .iter()
                .filter(|block| block["type"] == "tool_use");
            pending.extend(calls.map(|block| block["id"].clone()));
        } else {
            let results = blocks.into_iter().flatten();
            let results = results.filter(|block| block["type"] == "tool_result");
            let answered: Vec<_> = results.map(|block| block["tool_use_id"].clone()).collect();
            assert_eq!(answered, pending, "tool results do not answer the calls");
            pending.clear();
        }
    }
    assert!(pending.is_empty(), "history ends with unmatched tool_use");
}

#[test]
fn completed_terminal_result_precedes_cancellation() {
    let (tx, rx) = mpsc::sync_channel(1);
    let wake = crate::sys::new().unwrap();
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
fn tools_run_in_order_on_their_panes_and_unknown_tools_never_run() {
    let server = Server::new(5, |index, body, stream| {
        let messages = body["messages"].as_array().unwrap();
        paired(messages);
        let events = match index {
            0 => response(
                "I will inspect this workspace.",
                &[
                    ("inventory", "read", json!({"source":"workspace"})),
                    (
                        "unknown",
                        "run_command",
                        json!({"command":"touch should-not-run"}),
                    ),
                    (
                        "edit",
                        "edit_file",
                        json!({"pane":31,"path":"a.rs","old_text":"a","new_text":"b"}),
                    ),
                ],
            ),
            1 => {
                let results = messages.last().unwrap()["content"].as_array().unwrap();
                let errors: Vec<_> = results.iter().map(|result| &result["is_error"]).collect();
                assert_eq!(errors, [false, true, false]);
                response("Observed the requested result.", &[])
            }
            3 => {
                let history = body["messages"].to_string();
                assert!(history.contains("Observed the requested result."));
                response("Next turn works.", &[])
            }
            _ => goal_response("complete", "Verified."),
        };
        sse(stream, events);
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent.send("Inspect".into(), 17, json!({})).unwrap();
    let mut calls = Vec::new();
    let events = settled(agent, |request| {
        calls.push((request.tool, request.pane, request.id.clone()));
        request.respond(Ok("observed".into()));
    });
    let routes: Vec<_> = calls.iter().map(|(tool, pane, _)| (*tool, *pane)).collect();
    assert_eq!(routes, [(Tool::Read, 17), (Tool::EditFile, 31)]);
    // Edit records attach to the history entry carrying this identity.
    assert!(events.iter().any(|event| matches!(event,
        Event::Tool { id, role, .. } if role == "edit_file · done" && *id == calls[1].2)));
    assert!(!errored(&events));
    agent.send("next request".into(), 17, json!({})).unwrap();
    settled(agent, |_| panic!("no further terminal operation"));
    assert_eq!(worker.finish().0.len(), 2);
    server.finish();
}

#[test]
fn abort_rewind_and_drop_cancel_waits_without_sending_terminal_input() {
    for action in ["abort", "rewind", "drop"] {
        let server = Server::new(
            if action == "drop" { 1 } else { 3 },
            |index, body, stream| {
                paired(body["messages"].as_array().unwrap());
                let wait = json!({"source":"command","command":7,"wait":1800});
                sse(
                    stream,
                    match index {
                        0 => response("", &[("waiting", "read", wait)]),
                        1 => response("Recovered.", &[]),
                        _ => goal_response("complete", "Recovered."),
                    },
                );
            },
        );
        let worker = Worker::new(server.url.clone());
        let agent = worker.agent();
        let turn = agent
            .send("wait for command".into(), 17, json!({}))
            .unwrap();
        let pending = first_request(agent);
        assert_eq!(pending.tool, Tool::Read);
        let started = Instant::now();
        if action == "drop" {
            worker.finish();
            assert!(pending.canceled.load(Ordering::Acquire));
            pending.respond(Ok("late completion".into()));
        } else {
            if action == "abort" {
                agent.abort();
                settled(agent, |_| panic!("cancel issued another terminal action"));
            } else {
                agent.rewind(turn).unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut rewound = false;
                while !rewound {
                    assert!(Instant::now() < deadline);
                    wait(agent.fd(), 50).unwrap();
                    for event in agent.poll() {
                        match event {
                            Event::Rewound { turn: id, error } => {
                                assert_eq!((id, error), (turn, None));
                                rewound = true;
                            }
                            Event::Request(_) => panic!("rewind issued another terminal action"),
                            _ => (),
                        }
                    }
                }
            }
            assert!(pending.canceled.load(Ordering::Acquire));
            pending.respond(Ok("late completion".into()));
            agent.send("resume".into(), 17, json!({})).unwrap();
            let events = settled(agent, |_| panic!("old wait revived"));
            let recovered =
                |event: &Event| matches!(event, Event::Text(text) if text == "Recovered.");
            assert!(events.iter().any(recovered));
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
fn interrupted_responses_retry_the_same_request_without_replaying_tools() {
    let mut interrupted = None;
    let mut review = None;
    let server = Server::new(6, move |index, body, stream| {
        paired(body["messages"].as_array().unwrap());
        match index {
            // A finished message needs no HTTP trailer, even while the connection stays open.
            0 => {
                let first = response("", &[("first", "shell", json!({"command":"first"}))]);
                truncated_chunked_sse(stream, first);
                closed(stream);
            }
            1 => {
                assert!(body["messages"].to_string().contains("first-completed"));
                interrupted = Some(body.clone());
                let partial = ("partial", "shell", json!({"command":"must-not-run"}));
                let mut events = response("Discarded partial response.", &[partial]);
                events.pop();
                truncated_chunked_sse(stream, events);
            }
            2 => {
                assert_eq!(Some(body), interrupted.as_ref());
                let second = response("", &[("second", "shell", json!({"command":"second"}))]);
                sse(stream, second);
            }
            3 => {
                let text = body["messages"].to_string();
                assert!(text.contains("first-completed") && text.contains("second-completed"));
                assert!(!text.contains("Discarded partial") && !text.contains("must-not-run"));
                sse(stream, response("Both actions verified.", &[]));
            }
            4 => {
                review = Some(body.clone());
                unavailable(stream);
            }
            _ => {
                assert_eq!(Some(body), review.as_ref());
                sse(stream, goal_response("complete", "Both actions verified."));
            }
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
        request.respond(Ok(format!("{command}-completed")));
        commands.push(command);
    });
    assert_eq!(commands, ["first", "second"]);
    assert!(!errored(&events));
    let resets = events
        .iter()
        .filter(|event| matches!(event, Event::ResetText));
    assert_eq!(resets.count(), 1);
    let (history, _) = worker.finish();
    paired(&history[0].messages);
    server.finish();
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
            !events
                .iter()
                .any(|event| matches!(event, Event::Request(_)))
        );
        if notices(&events).any(|text| text.contains("Retrying")) {
            break;
        }
    }
    let start = Instant::now();
    agent.abort();
    let events = settled(agent, |_| panic!("tool executed after cancellation"));
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(!errored(&events));
    worker.finish();
    server.finish();
}

#[test]
fn summary_requests_retry_three_times_with_the_same_body() {
    let mut original = None;
    let server = Server::new(6, move |index, body, stream| {
        assert_eq!(body, original.get_or_insert_with(|| body.clone()));
        match index {
            0..4 => unavailable(stream),
            4 => sse(
                stream,
                vec![json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}})],
            ),
            _ => sse(stream, response("Recovered summary.", &[])),
        }
    });
    let shared = Shared {
        state: Mutex::new(State::default()),
        wake: Arc::new(crate::sys::new().unwrap()),
        notification: crate::sys::new().unwrap(),
    };
    let mut session = session(server.url.clone());
    let messages = [json!({"role":"user","content":"Keep completed command 123."})];
    let canceled = AtomicBool::new(false);
    let error = session.summarize(&shared, &messages, &canceled);
    assert!(error.unwrap_err().to_string().contains("503"));
    let summary = session.summarize(&shared, &messages, &canceled);
    assert_eq!(summary.unwrap(), "Recovered summary.");
    server.finish();
}

#[test]
fn failed_responses_never_run_their_tools_and_leave_paired_history() {
    // One response stops at the output limit; another breaks off with malformed data.
    for malformed in [false, true] {
        let server = Server::new(3, move |index, body, stream| {
            paired(body["messages"].as_array().unwrap());
            match index {
                0 => {
                    let call = ("limited", "shell", json!({"command":"must-not-run"}));
                    let mut events = response("Partial.", &[call]);
                    let stop = events.len() - 2;
                    if malformed {
                        events.truncate(stop);
                        sse(stream, events);
                        stream.write_all(b"data: {\"type\":\r\n\r\n").unwrap();
                    } else {
                        events[stop]["delta"]["stop_reason"] = json!("max_tokens");
                        sse(stream, events);
                    }
                }
                1 => {
                    assert!(body["messages"].to_string().contains("Turn interrupted"));
                    sse(
                        stream,
                        response("The previous action was not executed.", &[]),
                    );
                }
                _ => sse(stream, goal_response("complete", "Explained.")),
            }
        });
        let worker = Worker::new(server.url.clone());
        let agent = worker.agent();
        agent.send("first request".into(), 17, json!({})).unwrap();
        assert!(errored(&settled(agent, |_| panic!(
            "a failed response ran its tool"
        ))));
        agent.send("continue".into(), 17, json!({})).unwrap();
        assert!(!errored(&settled(agent, |_| panic!(
            "unexpected tool call"
        ))));
        for turn in worker.finish().0 {
            paired(&turn.messages);
        }
        server.finish();
    }
}

#[test]
fn compaction_keeps_the_request_and_latest_result_and_rolls_old_turns_forward() {
    let objective = "检查 command 7；保留用户草稿；最终停在调试器，不要退出。";
    let (mut summaries, mut rounds) = (0, 0);
    let server = Server::new(27, move |_, body, stream| {
        let messages = body["messages"].as_array().unwrap();
        let text = serde_json::to_string(messages).unwrap();
        paired(messages);
        let summary = body["tool_choice"]["type"] == "none" && body["system"] != GOAL_REVIEW;
        assert!(summary || text.len() <= CONTEXT_LIMIT);
        let events = if body["system"] == GOAL_REVIEW {
            goal_response("complete", "Verified.")
        } else if summary {
            summaries += 1;
            // Summaries build on earlier ones, which never repeat the request.
            if summaries > 1 {
                assert!(text.contains(&format!("Historical result {}:", summaries - 1)));
            }
            let summary = format!("Historical result {summaries}: command 7 remains running.");
            response(&summary, &[])
        } else {
            rounds += 1;
            if rounds <= 19 {
                assert!(messages[0]["content"].as_str().unwrap().contains(objective));
            }
            if (2..=19).contains(&rounds) {
                let result = &messages.last().unwrap()["content"][0];
                assert_eq!(result["tool_use_id"], format!("read-{}", rounds - 2));
                let observation = format!("Observation {}:", rounds - 2);
                assert!(
                    result["content"]
                        .as_str()
                        .unwrap()
                        .starts_with(&observation)
                );
            }
            match rounds {
                1..=18 => {
                    let id = format!("read-{}", rounds - 1);
                    response(
                        "",
                        &[(&id, "read", json!({"source":"command","command":7}))],
                    )
                }
                19 => response("Stopped at the requested debugger state.", &[]),
                _ => {
                    assert_eq!(summaries, 5);
                    assert!(
                        text.contains("Historical result 5") && text.contains("new constraint")
                    );
                    response("Continued from the historical summary.", &[])
                }
            }
        };
        sse(stream, events);
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent.send(objective.into(), 17, json!({})).unwrap();
    let mut observations = 0;
    let events = settled(agent, |request| {
        let text = format!("Observation {observations}: {}", "x".repeat(60_000));
        observations += 1;
        request.respond(Ok(text));
    });
    assert_eq!(observations, 18);
    assert!(!errored(&events));
    assert_eq!(notices(&events).count(), 4);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Text(text) if text.contains("Historical result")))
    );
    let constraint = format!("new constraint {}", "n".repeat(150_000));
    agent.send(constraint, 17, json!({})).unwrap();
    let events = settled(agent, |_| panic!("summary cannot execute a tool"));
    assert!(!errored(&events));
    let (history, summary) = worker.finish();
    assert_eq!(history.len(), 1);
    assert!(summary.contains("Historical result 5"));
    paired(&history[0].messages);
    assert_eq!(server.finish().len(), 27);
}

#[test]
fn goal_review_compaction_keeps_the_latest_tool_result_as_original_evidence() {
    let server = Server::new(7, |index, body, stream| {
        let messages = body["messages"].as_array().unwrap();
        paired(messages);
        let events = match index {
            0..4 => {
                let id = format!("read-{index}");
                response(
                    "",
                    &[(&id, "read", json!({"source":"command","command":7}))],
                )
            }
            4 => response(&"Final analysis. ".repeat(2000), &[]),
            5 => {
                assert_eq!(body["tool_choice"]["type"], "none");
                assert_ne!(body["system"], GOAL_REVIEW);
                response("Historical work before the latest verification.", &[])
            }
            _ => {
                assert_eq!(body["system"], GOAL_REVIEW);
                assert!(serde_json::to_vec(messages).unwrap().len() <= CONTEXT_LIMIT);
                let result = messages
                    .iter()
                    .filter_map(|message| message["content"].as_array())
                    .flatten()
                    .find(|block| {
                        block["type"] == "tool_result" && block["tool_use_id"] == "read-3"
                    })
                    .expect("goal review lost the latest original tool result");
                let content = result["content"].as_str().unwrap();
                assert!(content.starts_with("verified-command-7-exit-0:"));
                goal_response("complete", "The verification result supports completion.")
            }
        };
        sse(stream, events);
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    let objective = "analyze and verify command 7";
    agent.send(objective.into(), 17, json!({})).unwrap();
    let mut calls = 0;
    let events = settled(agent, |request| {
        let label = if calls == 3 {
            "verified-command-7-exit-0:"
        } else {
            "historical-output:"
        };
        calls += 1;
        request.respond(Ok(format!("{label}{}", "x".repeat(60_000))));
    });
    assert_eq!(calls, 4);
    assert!(!errored(&events));
    paired(&worker.finish().0[0].messages);
    server.finish();
}

#[test]
fn failed_compaction_preserves_executed_results_for_the_next_turn() {
    let server = Server::new(9, |index, body, stream| {
        let messages = body["messages"].as_array().unwrap();
        let text = serde_json::to_string(messages).unwrap();
        paired(messages);
        match index {
            0..5 => {
                let id = format!("action-{index}");
                let call = (id.as_str(), "shell", json!({"command": id}));
                sse(stream, response("", &[call]));
            }
            5 | 6 => {
                assert_eq!(body["tool_choice"]["type"], "none");
                for action in 0..index - 1 {
                    assert!(text.contains(&format!("executed-{action}")));
                }
                if index == 5 {
                    let reply = "HTTP/1.1 400 Bad Request\r\nContent-Length: 8\r\nConnection: close\r\n\r\nrejected";
                    stream.write_all(reply.as_bytes()).unwrap();
                } else {
                    sse(stream, response("All five actions already executed.", &[]));
                }
            }
            7 => {
                assert!(text.len() <= CONTEXT_LIMIT);
                assert!(text.contains("All five actions already executed."));
                assert!(text.contains("continue without repeating actions"));
                sse(
                    stream,
                    response("Recovered without replaying actions.", &[]),
                );
            }
            _ => sse(stream, goal_response("complete", "Recovered.")),
        }
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent
        .send("perform five actions".into(), 17, json!({}))
        .unwrap();
    let mut actions = 0;
    let events = settled(agent, |request| {
        request.respond(Ok(format!("executed-{actions}: {}", "x".repeat(60_000))));
        actions += 1;
    });
    assert_eq!(actions, 5);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Message("Error", text) if text.contains("400")))
    );
    let next = "continue without repeating actions";
    agent.send(next.into(), 17, json!({})).unwrap();
    let events = settled(agent, |_| panic!("completed action was replayed"));
    assert!(!errored(&events));
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
        let history = json!(messages).to_string();
        assert!(history.contains("verify command 7"));
        let events = match index {
            0 => response("The task is finished.", &[]),
            1 => {
                assert_eq!(body["system"], GOAL_REVIEW);
                goal_response("continue", "Read command 7 and verify its exit status.")
            }
            2 => {
                let reason = messages.last().unwrap()["content"].as_str().unwrap();
                assert!(reason.contains("Read command 7"));
                let read = json!({"source":"command","command":7});
                response("", &[("verify", "read", read)])
            }
            3 => {
                assert!(history.contains("verified-result"));
                response("Verified the requested result.", &[])
            }
            _ => {
                assert_eq!(body["system"], GOAL_REVIEW);
                assert!(history.contains("verified-result"));
                // Models sometimes fence the requested JSON.
                let review = json!({"status":"complete","reason":"Command 7 exited successfully."});
                response(&format!("```json\n{review}\n```"), &[])
            }
        };
        sse(stream, events);
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
        let result = json!({"exit_code":0,"text":"verified-result"});
        request.respond(Ok(result.to_string()));
    });
    assert_eq!(calls, 1);
    assert!(!errored(&events));
    assert!(notices(&events).any(|text| text.contains("Read command 7")));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Text(text) if text.contains("\"status\"")))
    );
    let (history, _) = worker.finish();
    assert_eq!(history.len(), 1);
    paired(&history[0].messages);
    let history = json!(history[0].messages).to_string();
    assert!(!history.contains("Harness goal review: the active request"));
    server.finish();
}

#[test]
fn blocked_goal_reports_the_missing_input_and_accepts_the_next_submission() {
    let server = Server::new(4, |index, body, stream| {
        let events = match index {
            0 => response("Which directory should I use?", &[]),
            1 => goal_response("blocked", "Please provide the target directory."),
            2 => {
                let history = body["messages"].to_string();
                assert!(history.contains("Goal blocked: Please provide"));
                assert!(history.contains("Use /project"));
                response("The supplied directory is /project.", &[])
            }
            _ => goal_response("complete", "Answered the user's question."),
        };
        sse(stream, events);
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    let question = "What is my target directory?";
    agent.send(question.into(), 17, json!({})).unwrap();
    let events = settled(agent, |_| panic!("missing input caused a terminal action"));
    assert!(events.iter().any(|event| matches!(event,
        Event::Message("Blocked", text) if text.contains("provide the target directory"))));
    agent.send("Use /project".into(), 17, json!({})).unwrap();
    let events = settled(agent, |_| panic!("an answer needs no terminal action"));
    assert!(!errored(&events));
    assert_eq!(worker.finish().0.len(), 2);
    server.finish();
}

#[test]
fn malformed_goal_review_preserves_completed_actions_without_retrying_them() {
    let server = Server::new(5, |index, body, stream| {
        paired(body["messages"].as_array().unwrap());
        let events = match index {
            0 => response("", &[("once", "shell", json!({"command":"side effect"}))]),
            1 => response("The action finished.", &[]),
            2 => response("not JSON", &[]),
            3 => {
                assert!(body["messages"].to_string().contains("executed-once"));
                response("Recovered with the recorded result.", &[])
            }
            _ => goal_response("complete", "The recorded result satisfies the request."),
        };
        sse(stream, events);
    });
    let worker = Worker::new(server.url.clone());
    let agent = worker.agent();
    agent
        .send("perform the action".into(), 17, json!({}))
        .unwrap();
    let mut calls = 0;
    let events = settled(agent, |request| {
        calls += 1;
        request.respond(Ok("executed-once".into()));
    });
    assert_eq!(calls, 1);
    assert!(errored(&events));
    let next = "continue using the existing result";
    agent.send(next.into(), 17, json!({})).unwrap();
    let events = settled(agent, |_| panic!("completed action was replayed"));
    assert!(!errored(&events));
    for turn in worker.finish().0 {
        paired(&turn.messages);
    }
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
    let request = ToolRequest {
        id: "test-edit".into(),
        tool,
        pane,
        input,
        canceled: Arc::new(AtomicBool::new(false)),
        reply,
        wake: Arc::new(crate::sys::new().unwrap()),
    };
    (request, receiver)
}

fn result(receiver: &mpsc::Receiver<io::Result<String>>) -> Value {
    serde_json::from_str(&receiver.recv().unwrap().unwrap()).unwrap()
}

/// Answers the client's frame request with pane 7, optionally at an idle shell prompt.
fn terminal_frame(
    client: &mut crate::workspace::Client,
    peer: &mut crate::wire::Peer,
    prompt: Option<u64>,
) {
    use crate::workspace::{Action, PaneView, Snapshot};
    client.request_screen().unwrap();
    client.flush().unwrap();
    let mut request = None;
    while let Some(action) = peer.next_action().unwrap() {
        if let Action::ScreenRequest { request: id, .. } = action {
            request = Some(id);
        }
    }
    let size = crate::core::WindowSize {
        num_cols: 20,
        num_lines: 4,
        cell_width: 0,
        cell_height: 0,
    };
    let core = crate::core::Core::new(size, 0, 0);
    let pane = PaneView {
        id: 7,
        tab: 7,
        rect: crate::panel::rect(0, 0, 20, 4),
        title: "shell".into(),
        cwd: None,
        process: Arc::default(),
        blocks: Vec::new(),
        command_blocks: Vec::new(),
        input_generation: 4,
        shell_prompt: prompt,
    };
    let snapshot = Snapshot {
        name: "test".into(),
        controller: true,
        focused: 1,
        active_tab: 1,
        tabs: Vec::new(),
        panes: vec![pane],
    };
    let screens = vec![(7, crate::screen::Update::capture_live(&core))];
    let state = crate::wire::Response::State(Some(snapshot), screens, request.unwrap());
    peer.send_response(state).unwrap();
    peer.flush().unwrap();
    client.poll().unwrap();
    assert!(!client.screen_stale());
}

// Starts open_terminal from pane 1, acknowledges new pane 7 and delivers its first frame.
fn open_terminal(
    client: &mut crate::workspace::Client,
    peer: &mut crate::wire::Peer,
) -> (crate::agent::Agent, mpsc::Receiver<io::Result<String>>) {
    use crate::agent::{Agent, Stage};
    let mut agent = Agent::default();
    let (request, result) = tool_request(Tool::OpenTerminal, 1, json!({"pane":1}));
    agent.begin(request, client);
    let Stage::NewTerminal { id } = agent.operation.as_ref().unwrap().stage else {
        panic!()
    };
    let ack = &mut crate::workspace::Response::AgentNewTab {
        request: id,
        result: Ok(7),
    };
    agent.handle_event(ack, client);
    terminal_frame(client, peer, None);
    (agent, result)
}

#[test]
fn new_terminal_waits_for_prompt_then_authorizes_shell_without_another_read() {
    use crate::workspace::Action;
    let (mut client, mut peer) = terminal_client();
    let (mut agent, receiver) = open_terminal(&mut client, &mut peer);
    agent.advance(&mut client);
    assert!(matches!(
        receiver.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert!(agent.deadline().is_some());
    terminal_frame(&mut client, &mut peer, Some(9));
    agent.advance(&mut client);
    let value = result(&receiver);
    assert_eq!(
        (&value["created"], &value["pane"]),
        (&json!(true), &json!(7))
    );
    assert_eq!(value["shell_prompt"], 9);
    assert!(value["screen"].is_object());
    assert_eq!(agent.observed_input.get(&7), Some(&4));
    assert!(!agent.observed_input.contains_key(&1));

    let (request, _result) = tool_request(Tool::Shell, 7, json!({"command":"printf verified"}));
    agent.begin_ready(request, &mut client);
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
    use crate::agent::Stage;
    for finish in ["timeout", "cancel", "close"] {
        let (mut client, mut peer) = terminal_client();
        let (mut agent, receiver) = open_terminal(&mut client, &mut peer);
        let operation = agent.operation.as_mut().unwrap();
        match finish {
            "timeout" => {
                operation.stage = Stage::NewTerminalOutput {
                    until: Instant::now(),
                }
            }
            "cancel" => operation.request.canceled.store(true, Ordering::Release),
            _ => agent.close(&mut client).unwrap(),
        }
        agent.advance(&mut client);
        assert!(agent.operation.is_none());
        let value = result(&receiver);
        assert_eq!(
            (&value["created"], &value["pane"]),
            (&json!(true), &json!(7))
        );
        if finish == "timeout" {
            assert_eq!(value["shell_prompt"], Value::Null);
            assert_eq!(value["outcome"], "timeout");
            let shell = json!({"command":"printf too-early"});
            let (request, receiver) = tool_request(Tool::Shell, 7, shell);
            agent.begin_ready(request, &mut client);
            assert!(receiver.recv().unwrap().is_err());
        } else {
            assert!(value["observation_error"].is_string());
        }
    }
}

#[test]
fn accepted_input_observation_ends_without_a_fresh_frame() {
    use crate::agent::{Agent, Operation, Stage};
    use crate::workspace::Response;
    for cancel in [false, true] {
        let (mut client, _peer) = terminal_client();
        let mut agent = Agent::default();
        let (request, receiver) = tool_request(Tool::Input, 7, json!({"text":"x"}));
        let stage = Stage::Input {
            id: 42,
            generation: 4,
            before: 0,
        };
        agent.operation = Some(Operation { request, stage });
        let accepted = &mut Response::AgentInput {
            request: 42,
            result: Ok(None),
        };
        agent.handle_event(accepted, &mut client);
        assert!(client.screen_stale());
        let operation = agent.operation.as_mut().unwrap();
        if cancel {
            operation.request.canceled.store(true, Ordering::Release);
        } else if let Stage::InputOutput { until, .. } = &mut operation.stage {
            *until = Instant::now();
        }
        agent.advance(&mut client);
        let value = result(&receiver);
        assert_eq!(value["input_queued"], true);
        assert!(value["observation_error"].is_string());
        assert!(value.get("screen").is_none());
        assert!(agent.operation.is_none());
    }
}

#[test]
fn terminal_wait_deadlines_do_not_depend_on_pending_frames() {
    use crate::agent::{Agent, Operation, Stage};
    for (tool, source, command) in [
        (Tool::Read, "screen", None),
        (Tool::Read, "history", None),
        (Tool::Read, "command", Some(37)),
        (Tool::Shell, "command", Some(37)),
    ] {
        let (mut client, mut peer) = terminal_client();
        terminal_frame(&mut client, &mut peer, Some(9));
        let mut agent = Agent::default();
        agent.observed_input.insert(7, 1);
        agent.observed_activity.insert(7, 2);
        let (request, receiver) = tool_request(tool, 7, json!({"source":source,"wait":1}));
        let until = Instant::now() + Duration::from_secs(1);
        let stage = match command {
            Some(command) => Stage::Command { command, until },
            None => Stage::Change { until },
        };
        agent.operation = Some(Operation { request, stage });
        client.refresh_screen().unwrap();
        assert_eq!(agent.deadline(), Some(until));
        agent.advance(&mut client);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        let (Stage::Command { until, .. } | Stage::Change { until }) =
            &mut agent.operation.as_mut().unwrap().stage
        else {
            panic!()
        };
        *until = Instant::now();
        agent.advance(&mut client);
        if tool == Tool::Shell {
            let value = result(&receiver);
            assert_eq!(
                (&value["input_queued"], &value["command"]),
                (&json!(true), &json!(37))
            );
            assert!(value["observation_error"].is_string());
        } else {
            let error = receiver.recv().unwrap().unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        }
        assert_eq!(agent.observed_input.get(&7), Some(&1));
        assert_eq!(agent.observed_activity.get(&7), Some(&2));
        assert!(agent.operation.is_none());
    }
}

#[test]
fn stopping_a_shell_observation_keeps_the_accepted_command() {
    use crate::agent::{Agent, Operation, Stage};
    let (mut client, _peer) = terminal_client();
    let mut agent = Agent::default();
    let (request, receiver) = tool_request(Tool::Shell, 7, json!({"command":"make"}));
    let until = Instant::now() + Duration::from_secs(60);
    let stage = Stage::Command { command: 37, until };
    agent.operation = Some(Operation { request, stage });
    agent.close(&mut client).unwrap();
    let value = result(&receiver);
    assert_eq!(
        (&value["input_queued"], &value["command"]),
        (&json!(true), &json!(37))
    );
    assert!(value["observation_error"].is_string());
}

#[test]
fn waiting_without_an_activity_baseline_does_not_report_a_change() {
    let (mut client, mut peer) = terminal_client();
    terminal_frame(&mut client, &mut peer, Some(9));
    let mut agent = crate::agent::Agent::default();
    let wait = json!({"source":"screen","wait":30});
    let (request, receiver) = tool_request(Tool::Read, 7, wait);
    agent.begin_ready(request, &mut client);
    agent.advance(&mut client);
    assert!(matches!(
        receiver.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
}

#[test]
fn committed_file_diff_keeps_its_tool_identity_without_expanding_model_result() {
    use crate::agent::{Agent, Operation, Stage};
    use crate::workspace::{AgentChange, FileResult, Response};
    let (mut client, _peer) = terminal_client();
    let mut agent = Agent {
        turn: 5,
        ..Agent::default()
    };
    let edit = json!({"pane":7,"path":"a.rs","old_text":"old","new_text":"new"});
    let (request, receiver) = tool_request(Tool::EditFile, 7, edit);
    let stage = Stage::File { id: 42 };
    agent.operation = Some(Operation { request, stage });
    let change = AgentChange {
        path: "a.rs".into(),
        added: 1,
        removed: 1,
        diff: "@@ -1 +1 @@\n-old\n+new\n".into(),
        ..AgentChange::default()
    };
    let edited = &mut Response::AgentFile {
        request: 42,
        result: Ok(FileResult::Edited { change }),
    };
    agent.handle_event(edited, &mut client);
    assert_eq!(
        (agent.changes[0].tool_id.as_str(), agent.changes[0].turn),
        ("test-edit", 5)
    );
    assert_eq!(result(&receiver), json!({"path":"a.rs","written":true}));
}
