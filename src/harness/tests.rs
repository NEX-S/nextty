use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[test]
fn monitored_command_and_isolated_subagent_wake_goal_without_model_polling() {
    let mut parent_round = 0;
    let mut child_round = 0;
    let server = Server::new(7, move |_, body, stream| {
        paired(body["messages"].as_array().unwrap());
        let child = body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("Independent delegated task.");
        let events = if child {
            assert!(!body["tools"].as_array().unwrap().iter().any(|t| matches!(
                t["name"].as_str(),
                Some("send_terminal" | "read_terminal" | "spawn_agent")
            )));
            assert!(!body["messages"].to_string().contains("monitor-proof"));
            child_round += 1;
            match child_round {
                1 => response(
                    "private child text",
                    &[(
                        "child-command",
                        "run_command",
                        json!({"command":"printf child-proof"}),
                    )],
                ),
                2 => {
                    assert!(body["messages"].to_string().contains("child-proof"));
                    response(
                        "",
                        &[(
                            "child-done",
                            "finish_goal",
                            json!({"summary":"child complete","evidence":"child-proof"}),
                        )],
                    )
                }
                _ => panic!("unexpected child model call"),
            }
        } else {
            parent_round += 1;
            match parent_round {
                1 => response(
                    "",
                    &[(
                        "watch",
                        "monitor",
                        json!({"command":"sleep 0.05; printf monitor-proof; exit 7"}),
                    )],
                ),
                2 => response("Waiting for command.", &[]),
                3 => {
                    let history = body["messages"].to_string();
                    assert!(
                        history.contains("monitor-proof") && history.contains("exit status: 7")
                    );
                    response(
                        "",
                        &[(
                            "delegate",
                            "spawn_agent",
                            json!({"task":"Run printf child-proof and report its actual output."}),
                        )],
                    )
                }
                4 => response("Waiting for child.", &[]),
                5 => {
                    assert!(body["messages"].to_string().contains("child complete"));
                    response(
                        "",
                        &[(
                            "done",
                            "finish_goal",
                            json!({"summary":"All checked","evidence":"command exited 7; child reported child-proof"}),
                        )],
                    )
                }
                _ => panic!("unexpected parent model call"),
            }
        };
        sse(stream, events);
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    worker
        .agent()
        .send("monitor then delegate".into(), 1, true, json!({}))
        .unwrap();
    let events = settled(worker.agent(), |_| {
        panic!("independent child accessed shared PTY")
    });
    assert!(events.iter().all(|e| !matches!(e, Event::Error(_))));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e,Event::Text(t) if t.contains("private child text")))
    );
    assert!(events.iter().any(|e|matches!(e,Event::Tool{name,text,done:true,is_error:true,..} if name=="monitor"&&text.contains("exit status: 7"))));
    assert!(events.iter().any(|e|matches!(e,Event::Tool{name,text,done:true,..} if name=="spawn_agent"&&text.contains("child complete"))));
    let history = worker.finish();
    assert_eq!(history.len(), 1);
    paired(&history[0].messages);
    assert_eq!(server.finish().len(), 7);
}

#[test]
fn stopping_a_goal_waiting_on_monitor_allows_the_next_turn() {
    let waiting = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&waiting);
    let server = Server::new(3, move |index, _, stream| {
        let events = match index {
            0 => response("", &[("long", "monitor", json!({"command":"sleep 120"}))]),
            1 => response("Waiting.", &[]),
            2 => response("New turn works.", &[]),
            _ => unreachable!(),
        };
        sse(stream, events);
        if index == 1 {
            seen.store(true, Ordering::Release);
        }
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    worker
        .agent()
        .send("wait for long command".into(), 1, true, json!({}))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !waiting.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
    let start = Instant::now();
    worker.agent().abort().unwrap();
    settled(worker.agent(), |_| panic!("no PTY needed"));
    assert!(start.elapsed() < Duration::from_secs(1));
    worker
        .agent()
        .send("next turn".into(), 1, false, json!({}))
        .unwrap();
    let events = settled(worker.agent(), |_| panic!("no PTY needed"));
    assert!(
        events
            .iter()
            .any(|e| matches!(e,Event::Text(t) if t=="New turn works."))
    );
    worker.finish();
    server.finish();
}

#[test]
fn dropping_harness_kills_command_descendants() {
    for tool in ["run_command", "monitor"] {
        let path =
            std::env::temp_dir().join(format!("nextty-owner-cleanup-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let command = format!(
            "sleep 120 & printf '%s %s' $$ $! > '{}'; wait",
            path.display()
        );
        let waiting = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&waiting);
        let server = Server::new(
            if tool == "monitor" { 2 } else { 1 },
            move |index, _, stream| {
                if index == 0 {
                    sse(
                        stream,
                        response("", &[("command", tool, json!({"command":command}))]),
                    );
                } else {
                    sse(stream, response("Waiting.", &[]));
                    seen.store(true, Ordering::Release);
                }
            },
        );
        let worker = Worker::new(server.url.clone(), VecDeque::new());
        worker
            .agent()
            .send("wait".into(), 1, true, json!({}))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let pids = loop {
            if let Ok(pids) = std::fs::read_to_string(&path)
                && pids.split_whitespace().count() == 2
                && (tool == "run_command" || waiting.load(Ordering::Acquire))
            {
                break pids;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        };
        worker.finish();
        let deadline = Instant::now() + Duration::from_secs(3);
        for pid in pids.split_whitespace() {
            while let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                if matches!(stat.split_whitespace().nth(2), Some("Z" | "X")) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "live descendant {pid} survived owner shutdown"
                );
                thread::sleep(Duration::from_millis(5));
            }
        }
        server.finish();
        std::fs::remove_file(path).unwrap();
    }
}

struct Worker {
    harness: Option<Harness>,
    done: mpsc::Receiver<io::Result<VecDeque<Turn>>>,
}

impl Worker {
    fn new(url: String, history: VecDeque<Vec<Value>>) -> Self {
        let next_turn = history.len() as u64;
        let history = history
            .into_iter()
            .enumerate()
            .map(|(index, messages)| Turn {
                id: index as u64 + 1,
                messages,
                facts_before: Vec::new(),
            })
            .collect();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                next_turn,
                ..State::default()
            }),
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
                cwd: None,
                remote: false,
                history,
                archive: Archive::new().unwrap(),
                facts: Vec::new(),
                delegated: false,
            };
            let result = session.run(&shared).map(|()| session.history);
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

    fn finish(mut self) -> VecDeque<Turn> {
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
fn streams_executes_tools_and_preserves_the_conversation_between_turns() {
    let server = Server::new(3, |index, body, stream| {
        assert_eq!(body["model"], "local-fixture");
        match index {
            0 => sse(
                stream,
                response(
                    "检查现场。",
                    &[
                        (
                            "shell",
                            "run_command",
                            json!({"command":"printf 'shell fixture'"}),
                        ),
                        ("screen", "read_terminal", json!({})),
                    ],
                ),
            ),
            1 => {
                let messages = body["messages"].as_array().unwrap();
                paired(messages);
                let results = messages.last().unwrap()["content"].as_array().unwrap();
                assert_eq!(results[0]["tool_use_id"], "shell");
                assert!(
                    results[0]["content"]
                        .as_str()
                        .unwrap()
                        .contains("shell fixture")
                );
                assert_eq!(results[0]["is_error"], false);
                assert_eq!(results[1]["tool_use_id"], "screen");
                assert_eq!(
                    serde_json::from_str::<Value>(results[1]["content"].as_str().unwrap()).unwrap()
                        ["content"],
                    "pane=17 alternate=true\nrip 0x1234 中文"
                );
                sse(stream, response("现场已核对。", &[]));
            }
            2 => {
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(messages.len(), 5);
                assert!(
                    messages[0]["content"]
                        .as_str()
                        .unwrap()
                        .contains("first task")
                );
                assert_eq!(messages[3]["content"][0]["text"], "现场已核对。");
                assert!(
                    messages[4]["content"]
                        .as_str()
                        .unwrap()
                        .contains("second task")
                );
                paired(messages);
                sse(stream, response("继续。", &[]));
            }
            _ => unreachable!(),
        }
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    agent
        .send("first task".into(), 17, false, json!({"pane":17}))
        .unwrap();
    let events = settled(agent, |request| {
        assert_eq!(request.name, "read_terminal");
        assert_eq!(request.input["pane"], 17);
        assert!(!request.canceled.load(Ordering::Acquire));
        agent
            .respond(
                request,
                Ok("pane=17 alternate=true\nrip 0x1234 中文".into()),
            )
            .unwrap();
    });
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    let text: String = events
        .iter()
        .filter_map(|event| {
            if let Event::Text(text) = event {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(text, "检查现场。现场已核对。");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::Tool {
                    done: true,
                    is_error: false,
                    ..
                }
            ))
            .count(),
        2
    );
    agent
        .send("second task".into(), 17, false, json!({"pane":17}))
        .unwrap();
    settled(agent, |_| panic!("unexpected terminal tool"));
    let history = worker.finish();
    assert_eq!(history.len(), 2);
    for turn in history {
        paired(&turn.messages);
    }
    assert_eq!(server.finish().len(), 3);
}

#[test]
fn cancels_streams_and_pending_terminal_requests_then_accepts_a_new_turn() {
    let server = Server::new(3, |index, body, stream| match index {
        0 => {
            let events = response("partial", &[]);
            sse(stream, events[..3].to_vec());
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        }
        1 => sse(
            stream,
            response(
                "",
                &[("late", "send_terminal", json!({"text":"ignored input\n"}))],
            ),
        ),
        2 => {
            paired(body["messages"].as_array().unwrap());
            sse(stream, response("recovered", &[]));
        }
        _ => unreachable!(),
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    agent
        .send("cancel the stream".into(), 17, true, json!({"pane":17}))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < deadline);
        wait(agent.fd(), 100).unwrap();
        if agent
            .poll()
            .unwrap()
            .iter()
            .any(|event| matches!(event, Event::Text(text) if text == "partial"))
        {
            break;
        }
    }
    let start = Instant::now();
    agent.abort().unwrap();
    settled(agent, |_| panic!("unexpected terminal request"));
    assert!(start.elapsed() < Duration::from_millis(500));
    agent
        .send(
            "cancel the terminal request".into(),
            17,
            false,
            json!({"pane":17}),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let request = loop {
        assert!(Instant::now() < deadline);
        wait(agent.fd(), 100).unwrap();
        if let Some(request) = agent.poll().unwrap().into_iter().find_map(|event| {
            if let Event::Terminal(request) = event {
                Some(request)
            } else {
                None
            }
        }) {
            break request;
        }
    };
    agent.abort().unwrap();
    assert!(request.canceled.load(Ordering::Acquire));
    settled(agent, |_| {
        panic!("extra terminal request after cancellation")
    });
    agent
        .respond(request, Ok("late frontend reply".into()))
        .unwrap();
    agent
        .send("new turn".into(), 17, false, json!({"pane":17}))
        .unwrap();
    let events = settled(agent, |_| panic!("unexpected terminal request"));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Text(text) if text == "recovered"))
    );
    let history = worker.finish();
    assert_eq!(history.len(), 3);
    for turn in history {
        paired(&turn.messages);
    }
    assert_eq!(server.finish().len(), 3);
}

#[test]
fn http_truncation_and_bad_tool_arguments_do_not_poison_later_turns() {
    let server = Server::new(4, |index, body, stream| {
        match index {
        0 => stream.write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 12\r\nConnection: close\r\n\r\ngateway down").unwrap(),
        1 => {
            let mut events = response("unfinished", &[("partial", "run_command", json!({"command":"printf never"}))]);
            events.truncate(events.len() - 3);
            sse(stream, events);
        }
        2 => {
            let mut events = response("", &[("invalid", "run_command", json!({}))]);
            let delta = events.iter_mut().find(|event| event["delta"]["type"] == "input_json_delta").unwrap();
            delta["delta"]["partial_json"] = json!("broken-json");
            sse(stream, events);
        }
        3 => {
            paired(body["messages"].as_array().unwrap());
            sse(stream, response("still usable", &[]));
        }
        _ => unreachable!(),
    }
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    for index in 0..3 {
        agent
            .send(format!("failure {index}"), 17, false, json!({"pane":17}))
            .unwrap();
        let events = settled(agent, |_| panic!("invalid response ran a terminal tool"));
        assert!(events.iter().any(|event| matches!(event, Event::Error(_))));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Tool { .. }))
        );
    }
    agent
        .send("recover".into(), 17, false, json!({"pane":17}))
        .unwrap();
    let events = settled(agent, |_| panic!("unexpected terminal tool"));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Text(text) if text == "still usable"))
    );
    let history = worker.finish();
    assert_eq!(history.len(), 4);
    for turn in history {
        paired(&turn.messages);
    }
    server.finish();
}

#[test]
fn sending_terminal_input_returns_an_observation_in_the_same_tool_result() {
    let server = Server::new(2, |index, body, stream| {
        if index == 0 {
            sse(
                stream,
                response("", &[("step", "send_terminal", json!({"text":"next\n"}))]),
            );
        } else {
            paired(body["messages"].as_array().unwrap());
            let text =
                body["messages"].as_array().unwrap().last().unwrap()["content"][0]["content"]
                    .as_str()
                    .unwrap();
            assert!(
                text.contains("Queued 5 bytes")
                    && text.contains("updated screen")
                    && text.contains("completion_signal")
            );
            sse(stream, response("Observed the step.", &[]));
        }
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    agent
        .send("step once".into(), 17, false, json!({"pane":17}))
        .unwrap();
    let mut names = Vec::new();
    let events = settled(agent, |request| {
        names.push(request.name.clone());
        let text = if request.name == "send_terminal" {
            "Queued 5 bytes"
        } else {
            "{\"screen\":\"updated screen\"}"
        };
        agent.respond(request, Ok(text.into())).unwrap();
    });
    assert_eq!(names, ["send_terminal", "read_terminal"]);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Text(text) if text == "Observed the step."))
    );
    worker.finish();
    server.finish();
}

#[test]
fn evicts_complete_turns_and_continues_past_32_rounds() {
    let server = Server::new(36, |index, body, stream| {
        let messages = body["messages"].as_array().unwrap();
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .starts_with("retained ")
        );
        paired(messages);
        if index == 35 {
            assert!(body.get("tool_choice").is_none());
            sse(
                stream,
                response(
                    "Observed the viewport; further investigation is unfinished.",
                    &[],
                ),
            );
            return;
        }
        sse(
            stream,
            response(
                "",
                &[(&format!("read-{index}"), "read_terminal", json!({}))],
            ),
        );
    });
    let history = VecDeque::from([
        vec![
            json!({"role":"user","content":format!("evicted {}", "a".repeat(140 * 1024))}),
            json!({"role":"assistant","content":[{"type":"text","text":"old answer"}]}),
        ],
        vec![
            json!({"role":"user","content":format!("retained {}", "b".repeat(140 * 1024))}),
            json!({"role":"assistant","content":[{"type":"text","text":"recent answer"}]}),
        ],
    ]);
    let worker = Worker::new(server.url.clone(), history);
    let agent = worker.agent();
    agent
        .send("inspect repeatedly".into(), 17, false, json!({"pane":17}))
        .unwrap();
    let mut reads = 0;
    let events = settled(agent, |request| {
        reads += 1;
        agent
            .respond(request, Ok("unchanged viewport".into()))
            .unwrap();
    });
    assert_eq!(reads, 35);
    assert!(events.iter().any(|event| matches!(event, Event::Notice(text) if text.contains("removed 1 oldest complete turn"))));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Text(text) if text.contains("further investigation is unfinished")))
    );
    let history = worker.finish();
    assert_eq!(history.len(), 2);
    assert_eq!(
        history[0].messages[1]["content"][0]["text"],
        "recent answer"
    );
    for turn in history {
        paired(&turn.messages);
    }
    assert_eq!(server.finish().len(), 36);
}

fn rewound(agent: &Harness, expected: u64) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < deadline, "agent did not rewind");
        wait(agent.fd(), 100).unwrap();
        for event in agent.poll().unwrap() {
            if let Event::Rewound { turn, error } = event {
                assert_eq!(turn, expected);
                return error;
            }
        }
    }
}

#[test]
fn rewind_removes_the_selected_and_later_turns_before_resending() {
    let server = Server::new(4, |index, body, stream| {
        if index == 3 {
            let messages = body["messages"].as_array().unwrap();
            paired(messages);
            assert_eq!(messages.len(), 3);
            assert!(messages[0]["content"].as_str().unwrap().ends_with("first"));
            assert_eq!(messages[1]["content"][0]["text"], "answer 0");
            assert!(
                messages[2]["content"]
                    .as_str()
                    .unwrap()
                    .ends_with("replacement")
            );
        }
        sse(stream, response(&format!("answer {index}"), &[]));
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    let first = agent
        .send("first".into(), 17, false, json!({"pane":17}))
        .unwrap();
    settled(agent, |_| panic!("unexpected terminal request"));
    let second = agent
        .send("second".into(), 17, false, json!({"pane":17}))
        .unwrap();
    settled(agent, |_| panic!("unexpected terminal request"));
    agent
        .send("third".into(), 17, false, json!({"pane":17}))
        .unwrap();
    settled(agent, |_| panic!("unexpected terminal request"));
    agent.rewind(second).unwrap();
    assert!(rewound(agent, second).is_none());
    let replacement = agent
        .send("replacement".into(), 17, false, json!({"pane":17}))
        .unwrap();
    assert!(replacement > second);
    settled(agent, |_| panic!("unexpected terminal request"));
    let history = worker.finish();
    assert_eq!(
        history.iter().map(|turn| turn.id).collect::<Vec<_>>(),
        [first, replacement]
    );
    server.finish();
}

#[test]
fn rewind_cancels_a_stream_and_a_pending_terminal_request_without_reviving_it() {
    let server = Server::new(4, |index, body, stream| match index {
        0 => sse(stream, response("original answer", &[])),
        1 => {
            sse(stream, response("partial", &[])[..3].to_vec());
            let mut byte = [0];
            match stream.read(&mut byte) {
                Ok(0) => (),
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => (),
                result => panic!("rewound stream stayed open: {result:?}"),
            }
        }
        2 => {
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 3);
            assert_eq!(messages[1]["content"][0]["text"], "original answer");
            sse(
                stream,
                response(
                    "",
                    &[(
                        "late",
                        "send_terminal",
                        json!({"text":"must not execute\n"}),
                    )],
                ),
            );
        }
        3 => {
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 1);
            paired(messages);
            assert!(
                messages[0]["content"]
                    .as_str()
                    .unwrap()
                    .ends_with("replacement")
            );
            sse(stream, response("recovered", &[]));
        }
        _ => unreachable!(),
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    let first = agent
        .send("first".into(), 17, false, json!({"pane":17}))
        .unwrap();
    settled(agent, |_| panic!("unexpected terminal request"));
    let streaming = agent
        .send("stream".into(), 17, false, json!({"pane":17}))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < deadline);
        wait(agent.fd(), 100).unwrap();
        if agent
            .poll()
            .unwrap()
            .iter()
            .any(|event| matches!(event, Event::Text(text) if text == "partial"))
        {
            break;
        }
    }
    agent.rewind(streaming).unwrap();
    assert!(rewound(agent, streaming).is_none());
    agent
        .send("terminal request".into(), 17, false, json!({"pane":17}))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let pending = loop {
        assert!(Instant::now() < deadline);
        wait(agent.fd(), 100).unwrap();
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
    agent.rewind(first).unwrap();
    assert!(pending.canceled.load(Ordering::Acquire));
    assert!(rewound(agent, first).is_none());
    agent
        .respond(pending, Ok("late frontend result".into()))
        .unwrap();
    let replacement = agent
        .send("replacement".into(), 17, false, json!({"pane":17}))
        .unwrap();
    settled(agent, |_| panic!("late terminal request was revived"));
    let history = worker.finish();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, replacement);
    paired(&history[0].messages);
    server.finish();
}

#[test]
fn an_evicted_rewind_target_reports_failure_without_deleting_retained_context() {
    let server = Server::new(2, |index, body, stream| {
        let messages = body["messages"].as_array().unwrap();
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .starts_with("retained ")
        );
        if index == 1 {
            assert_eq!(messages.len(), 5);
            assert_eq!(messages[3]["content"][0]["text"], "live answer");
        }
        paired(messages);
        sse(stream, response("live answer", &[]));
    });
    let history = VecDeque::from([
        vec![
            json!({"role":"user","content":format!("evicted {}", "a".repeat(140 * 1024))}),
            json!({"role":"assistant","content":[{"type":"text","text":"old"}]}),
        ],
        vec![
            json!({"role":"user","content":format!("retained {}", "b".repeat(140 * 1024))}),
            json!({"role":"assistant","content":[{"type":"text","text":"recent"}]}),
        ],
    ]);
    let worker = Worker::new(server.url.clone(), history);
    let agent = worker.agent();
    agent
        .send("live turn".into(), 17, false, json!({"pane":17}))
        .unwrap();
    settled(agent, |_| panic!("unexpected terminal request"));
    agent.rewind(1).unwrap();
    assert!(rewound(agent, 1).unwrap().contains("no longer available"));
    agent
        .send("keep going".into(), 17, false, json!({"pane":17}))
        .unwrap();
    settled(agent, |_| panic!("unexpected terminal request"));
    assert_eq!(
        worker
            .finish()
            .iter()
            .map(|turn| turn.id)
            .collect::<Vec<_>>(),
        [2, 3, 4]
    );
    server.finish();
}

#[test]
fn goal_continues_after_end_turn_and_finishes_explicitly_with_archived_initial_context() {
    let server = Server::new(3, |index, body, stream| {
        paired(body["messages"].as_array().unwrap());
        match index {
            0 => {
                let initial = body["messages"][0]["content"].as_str().unwrap();
                assert!(initial.contains("raw_context_id") && !initial.contains("raw_text"));
                assert!(!initial.contains("raw initial rip"));
                assert!(initial.contains("pane") && initial.contains("inspect the program"));
                sse(stream, response("I will inspect it.", &[]));
            }
            1 => {
                assert!(
                    body["messages"].as_array().unwrap().last().unwrap()["content"]
                        .as_str()
                        .unwrap()
                        .contains("Continue the active goal")
                );
                assert!(
                    body["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|t| t["name"] == "finish_goal")
                );
                sse(
                    stream,
                    response("", &[("original", "read_context", json!({"context_id":1}))]),
                );
            }
            2 => {
                let result =
                    &body["messages"].as_array().unwrap().last().unwrap()["content"][0]["content"];
                assert!(result.as_str().unwrap().contains("raw initial rip 0x1234"));
                sse(
                    stream,
                    response(
                        "",
                        &[(
                            "finished",
                            "finish_goal",
                            json!({"summary":"Inspected.","evidence":"RIP 0x1234 in supplied observation."}),
                        )],
                    ),
                );
            }
            _ => unreachable!(),
        }
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    worker.agent().send("inspect the program".into(),17,true,json!({"target":{"pane":17},"screen":{"rows":[[0,"rip 0x1234"]]},"raw_text":"raw initial rip 0x1234"})).unwrap();
    let events = settled(worker.agent(), |_| {
        panic!("initial metadata should suffice")
    });
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Text(text) if text == "Inspected.\n\nRIP 0x1234 in supplied observation."))
    );
    for turn in worker.finish() {
        paired(&turn.messages);
    }
    server.finish();
}

#[test]
fn long_goal_checkpoints_complete_exchanges_without_streaming_private_summary() {
    let mut calls = 0;
    let mut checkpoints = 0;
    let server = Server::new(9, move |_, body, stream| {
        let messages = body["messages"].as_array().unwrap();
        paired(messages);
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .ends_with("Find the exact RIP; do not modify files.")
        );
        if body["system"]
            .as_str()
            .unwrap()
            .starts_with("Summarize historical")
        {
            checkpoints += 1;
            assert_eq!(body["tool_choice"], json!({"type":"none"}));
            assert_eq!(body["max_tokens"], 8192);
            assert!(body.get("thinking").is_none());
            assert!(
                messages.last().unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .contains("600 words")
            );
            sse(
                stream,
                response(
                    &format!(
                        "PRIVATE CHECKPOINT {checkpoints}: RIP remains 0x1234. No files modified."
                    ),
                    &[],
                ),
            );
            return;
        }
        if calls == 6 {
            assert_eq!(checkpoints, 2);
            assert!(
                messages
                    .iter()
                    .any(|m| m.to_string().contains("Historical checkpoint"))
            );
            sse(stream, response("Observed RIP 0x1234.", &[]));
        } else {
            let id = format!("read-{calls}");
            sse(
                stream,
                response(&"a".repeat(20 * 1024), &[(&id, "read_terminal", json!({}))]),
            );
        }
        calls += 1;
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    // A normal tool-using request exercises the same continuous context path as /goal.
    agent
        .send(
            "Find the exact RIP; do not modify files.".into(),
            17,
            false,
            json!({"pane":17}),
        )
        .unwrap();
    let events = settled(agent, |request| {
        agent
            .respond(request, Ok(format!("rip 0x1234 {}", "v".repeat(40 * 1024))))
            .unwrap();
    });
    assert!(
        events.iter().all(|event| !matches!(event, Event::Error(_))),
        "{:?}",
        events
            .iter()
            .filter_map(|event| if let Event::Error(e) = event {
                Some(e)
            } else {
                None
            })
            .collect::<Vec<_>>()
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event,Event::Notice(text) if text.contains("checkpointing")))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event,Event::Text(text) if text.contains("PRIVATE CHECKPOINT")))
    );
    for turn in worker.finish() {
        paired(&turn.messages);
    }
    server.finish();
}

#[test]
fn masked_observations_remain_retrievable_without_losing_recent_results() {
    let server = Server::new(7, |index, body, stream| {
        let messages = body["messages"].as_array().unwrap();
        paired(messages);
        match index {
            0..=4 => sse(
                stream,
                response(
                    "",
                    &[(&format!("read-{index}"), "read_terminal", json!({}))],
                ),
            ),
            5 => {
                let oldest: Value =
                    serde_json::from_str(messages[2]["content"][0]["content"].as_str().unwrap())
                        .unwrap();
                assert_eq!(oldest, json!({"context_id":1,"archived":true}));
                assert!(
                    messages.last().unwrap()["content"][0]["content"]
                        .as_str()
                        .unwrap()
                        .contains("observation-4: exact RIP 0x1234")
                );
                sse(
                    stream,
                    response(
                        "",
                        &[("retrieve-first", "read_context", json!({"context_id":1}))],
                    ),
                );
            }
            6 => {
                let result: Value = serde_json::from_str(
                    messages.last().unwrap()["content"][0]["content"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap();
                assert!(
                    result["text"]
                        .as_str()
                        .unwrap()
                        .starts_with("observation-0: exact RIP 0x1234")
                );
                assert_eq!(result["next_offset"], 16 * 1024);
                sse(
                    stream,
                    response("Recovered the exact older observation.", &[]),
                );
            }
            _ => unreachable!(),
        }
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    agent
        .send(
            "Compare observations.".into(),
            17,
            false,
            json!({"pane":17}),
        )
        .unwrap();
    let mut observations = 0;
    let events = settled(agent, |request| {
        agent
            .respond(
                request,
                Ok(format!(
                    "observation-{observations}: exact RIP 0x1234 {}",
                    "v".repeat(30 * 1024)
                )),
            )
            .unwrap();
        observations += 1;
    });
    assert_eq!(observations, 5);
    assert!(events.iter().all(|e| !matches!(e, Event::Error(_))));
    for turn in worker.finish() {
        paired(&turn.messages);
    }
    server.finish();
}

#[test]
fn unchanged_viewports_reference_archived_baseline_and_register_changes_remain_visible() {
    fn viewport(rip: &str) -> Value {
        json!({"target":{"pane":17,"tab":1,"connection":"local","title":"pwndbg"},"screen":{"rows":[[0,format!("rip {rip}")]],"cursor":{"row":1,"column":0},"columns":80,"lines":24},"raw_text":format!("rip {rip}"),"incomplete":false})
    }
    let mut baseline = 0;
    let server = Server::new(4, move |index, body, stream| {
        let messages = body["messages"].as_array().unwrap();
        paired(messages);
        match index {
            0 => {
                let initial: Value = serde_json::from_str(
                    messages[0]["content"]
                        .as_str()
                        .unwrap()
                        .lines()
                        .nth(1)
                        .unwrap(),
                )
                .unwrap();
                baseline = initial["screen_context_id"].as_u64().unwrap();
                assert_eq!(initial["screen"]["rows"][0][1], "rip 0x1234");
                sse(
                    stream,
                    response("", &[("repeat", "read_terminal", json!({}))]),
                );
            }
            1 => {
                let result: Value = serde_json::from_str(
                    messages.last().unwrap()["content"][0]["content"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap();
                let content = &result["content"];
                assert_eq!(content["unchanged"], true);
                assert_eq!(content["screen_context_id"], baseline);
                assert!(content.get("screen").is_none() && content.get("raw_text").is_none());
                assert_eq!(content["target"], viewport("0x1234")["target"]);
                sse(
                    stream,
                    response("", &[("step", "send_terminal", json!({"text":"ni\n"}))]),
                );
            }
            2 => {
                let result: Value = serde_json::from_str(
                    messages.last().unwrap()["content"][0]["content"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap();
                let content = &result["content"];
                assert_eq!(content["screen"]["rows"][0][1], "rip 0x1235");
                assert_ne!(content["screen_context_id"], baseline);
                assert_ne!(content["unchanged"], true);
                assert_eq!(content["queued"], "Queued 3 bytes");
                assert_eq!(content["observation_delay_ms"], 100);
                assert_eq!(content["completion_signal"], false);
                sse(
                    stream,
                    response(
                        "",
                        &[("baseline", "read_context", json!({"context_id":baseline}))],
                    ),
                );
            }
            3 => {
                let result: Value = serde_json::from_str(
                    messages.last().unwrap()["content"][0]["content"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap();
                let original: Value =
                    serde_json::from_str(result["text"].as_str().unwrap()).unwrap();
                assert_eq!(original["screen"]["rows"][0][1], "rip 0x1234");
                assert_eq!(original["target"], viewport("0x1234")["target"]);
                assert_eq!(original["incomplete"], false);
                assert!(original["raw_context_id"].as_u64().is_some());
                sse(
                    stream,
                    response("Verified RIP changed from 0x1234 to 0x1235.", &[]),
                );
            }
            _ => unreachable!(),
        }
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    agent
        .send(
            "Step and compare exact RIP.".into(),
            17,
            false,
            viewport("0x1234"),
        )
        .unwrap();
    let mut observations = 0;
    let events = settled(agent, |request| {
        let result = if request.name == "send_terminal" {
            "Queued 3 bytes".into()
        } else {
            let rip = if observations == 0 {
                "0x1234"
            } else {
                "0x1235"
            };
            observations += 1;
            viewport(rip).to_string()
        };
        agent.respond(request, Ok(result)).unwrap();
    });
    assert_eq!(observations, 2);
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    for turn in worker.finish() {
        paired(&turn.messages);
    }
    server.finish();
}

fn debugger_viewport() -> Value {
    let rows: Vec<Value> = (0..22).filter(|row| !matches!(row,3|4)).map(|row|json!([row,format!("0x55555555{row:04x}: mov rax, [rbp - 0x18] │ exact register and instruction text")])).collect();
    json!({
        "target":{"workspace":"debug","pane":17,"connection":"local","title":"pwndbg","cwd":"/tmp"},
        "screen":{
            "columns":120,"lines":24,"alternate_screen":true,"display_offset":0,
            "row_coordinates":"zero-based viewport","cursor":{"row":21,"column":0},
            "rows":rows,"wrapped_rows":[],
            "omitted":[{"start_row":3,"end_row":3,"reason":"blank"},{"start_row":4,"end_row":4,"reason":"separator"},{"start_row":22,"end_row":23,"reason":"blank"}]
        },
        "raw_text":"original unmodified terminal text","incomplete":false
    })
}

#[test]
fn row_diffs_reconstruct_exact_screens_from_the_immutable_initial_baseline() {
    let original = debugger_viewport();
    let mut archive = Archive::new().unwrap();
    let initial = archive.target(original.clone(), true).unwrap();
    let base_id = initial["screen_context_id"].as_u64().unwrap();
    let mut current = original.clone();
    let rows = current["screen"]["rows"].as_array_mut().unwrap();
    rows.retain(|row| row[0] != 2 && row[0] != 5);
    rows.iter_mut().find(|row| row[0] == 1).unwrap()[1] =
        json!("RIP 0x55555555abcd │ 中\u{301}文 e\u{301}");
    rows.extend([
        json!([3, "new value in previously blank row"]),
        json!([4, "0xdeadbeef: ret; replaced separator"]),
    ]);
    rows.sort_unstable_by_key(|row| row[0].as_u64());
    current["screen"]["omitted"] = json!([{"start_row":2,"end_row":2,"reason":"blank"},{"start_row":5,"end_row":5,"reason":"separator"},{"start_row":22,"end_row":23,"reason":"blank"}]);
    current["screen"]["wrapped_rows"] = json!([1]);
    current["screen"]["cursor"] = json!({"row":4,"column":8});
    current["raw_text"] = json!("changed exact register text");
    current["target"]["title"] = json!("pwndbg: stepped");
    current["queued"] = json!("Queued 3 bytes");
    current["completion_signal"] = json!(false);
    for step in 0..2 {
        if step == 1 {
            current["screen"]["rows"][0][1] = json!("RSP 0x7fffffffe000: second step");
            current["raw_text"] = json!("second changed terminal text");
        }
        let result: Value =
            serde_json::from_str(&archive.observe(&current.to_string(), true).unwrap()).unwrap();
        let delta = &result["content"];
        assert_eq!(delta["screen"]["base_screen_context_id"], base_id);
        assert!(delta["screen"].get("rows").is_none());
        assert_eq!(delta["target"], current["target"]);
        assert_eq!(delta["queued"], current["queued"]);
        let updates = delta["screen"]["row_updates"].as_array().unwrap();
        assert!(updates.contains(&json!([2, null])) && updates.contains(&json!([5, null])));
        assert!(
            updates
                .iter()
                .any(|row| row[0] == 1 && row[1] == "RIP 0x55555555abcd │ 中\u{301}文 e\u{301}")
        );
        let mut rows: std::collections::BTreeMap<u64, Value> = initial["screen"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| (row[0].as_u64().unwrap(), row[1].clone()))
            .collect();
        for update in updates {
            let index = update[0].as_u64().unwrap();
            if update[1].is_null() {
                rows.remove(&index);
            } else {
                rows.insert(index, update[1].clone());
            }
        }
        let mut reconstructed = delta["screen"].clone();
        reconstructed
            .as_object_mut()
            .unwrap()
            .remove("base_screen_context_id");
        reconstructed.as_object_mut().unwrap().remove("row_updates");
        reconstructed["rows"] = json!(
            rows.into_iter()
                .map(|(row, text)| json!([row, text]))
                .collect::<Vec<_>>()
        );
        assert_eq!(reconstructed, current["screen"]);
        let archived: Value = serde_json::from_str(
            &archive
                .read(&json!({"context_id":delta["screen_context_id"]}))
                .unwrap(),
        )
        .unwrap();
        let archived: Value = serde_json::from_str(archived["text"].as_str().unwrap()).unwrap();
        assert_eq!(archived["screen"], current["screen"]);
        assert!(archived["screen"].get("row_updates").is_none());
    }
}

#[test]
fn incompatible_viewports_and_full_frame_changes_keep_complete_rows() {
    let original = debugger_viewport();
    let mut archive = Archive::new().unwrap();
    archive.target(original.clone(), true).unwrap();
    for (section, key, value) in [
        ("target", "pane", json!(18)),
        ("target", "workspace", json!("another-workspace")),
        ("target", "connection", json!("ssh")),
        ("screen", "columns", json!(121)),
        ("screen", "lines", json!(25)),
        ("screen", "alternate_screen", json!(false)),
        ("screen", "display_offset", json!(1)),
    ] {
        let mut changed = original.clone();
        changed[section][key] = value;
        let result = archive.target(changed.clone(), false).unwrap();
        assert_eq!(result["screen"], changed["screen"], "{section}.{key}");
        assert!(result["screen"].get("row_updates").is_none());
    }
    let mut changed = original;
    for row in changed["screen"]["rows"].as_array_mut().unwrap() {
        row[1] = json!(format!("entirely changed row {}", row[0]));
    }
    let result = archive.target(changed.clone(), false).unwrap();
    assert_eq!(result["screen"], changed["screen"]);
    assert!(result["screen"].get("row_updates").is_none());
}

#[test]
fn later_observations_cannot_become_an_unpinned_diff_baseline() {
    let mut archive = Archive::new().unwrap();
    archive.target(json!({"target":{"pane":17}}), true).unwrap();
    let first = debugger_viewport();
    archive.target(first.clone(), false).unwrap();
    let mut changed = first;
    changed["screen"]["rows"][0][1] = json!("RIP 0x1234");
    let result = archive.target(changed.clone(), false).unwrap();
    assert_eq!(result["screen"], changed["screen"]);
    assert!(result["screen"].get("row_updates").is_none());
}

#[test]
fn checkpoint_output_limits_keep_goal_running_with_explicit_loss_and_archive_recovery() {
    for partial in [
        None,
        Some("PARTIAL CHECKPOINT: confirmed RIP 0x1234; remaining facts omitted"),
    ] {
        let mut checkpoint_id = 0;
        let server = Server::new(7, move |index, body, stream| {
            let messages = body["messages"].as_array().unwrap();
            paired(messages);
            assert!(
                messages[0]["content"]
                    .as_str()
                    .unwrap()
                    .ends_with("Find exact RIP; no writes.")
            );
            match index {
                0..=3 => sse(
                    stream,
                    response(
                        &"a".repeat(20 * 1024),
                        &[(&format!("read-{index}"), "read_terminal", json!({}))],
                    ),
                ),
                4 => {
                    assert!(
                        body["system"]
                            .as_str()
                            .unwrap()
                            .starts_with("Summarize historical")
                    );
                    assert_eq!(body["max_tokens"], 8192);
                    assert!(body.get("thinking").is_none());
                    let mut events = if let Some(text) = partial {
                        response(text, &[])
                    } else {
                        vec![
                            json!({"type":"message_start","message":{"role":"assistant"}}),
                            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
                            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"PRIVATE reasoning exhausted the checkpoint output budget"}}),
                            json!({"type":"content_block_stop","index":0}),
                            json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"}}),
                            json!({"type":"message_stop"}),
                        ]
                    };
                    events
                        .iter_mut()
                        .find(|event| event["type"] == "message_delta")
                        .unwrap()["delta"]["stop_reason"] = json!("max_tokens");
                    sse(stream, events);
                }
                5 => {
                    let checkpoint = messages[1]["content"][0]["text"].as_str().unwrap();
                    assert!(checkpoint.contains("TRUNCATED"));
                    if let Some(text) = partial {
                        assert!(checkpoint.contains(text));
                    } else {
                        assert!(checkpoint.contains("no summary text"));
                    }
                    checkpoint_id = checkpoint
                        .split("original prefix: context_id ")
                        .nth(1)
                        .unwrap()
                        .split(')')
                        .next()
                        .unwrap()
                        .parse::<u64>()
                        .unwrap();
                    sse(
                        stream,
                        response(
                            "",
                            &[(
                                "recover",
                                "read_context",
                                json!({"context_id":checkpoint_id}),
                            )],
                        ),
                    );
                }
                6 => {
                    let result: Value = serde_json::from_str(
                        messages.last().unwrap()["content"][0]["content"]
                            .as_str()
                            .unwrap(),
                    )
                    .unwrap();
                    assert_eq!(result["context_id"], checkpoint_id);
                    assert!(result["text"].as_str().unwrap().contains("\"text\":\"aaaa"));
                    sse(
                        stream,
                        response(
                            "",
                            &[(
                                "done",
                                "finish_goal",
                                json!({"summary":"RIP verified","evidence":"Exact archived observations plus recent originals."}),
                            )],
                        ),
                    );
                }
                _ => unreachable!(),
            }
        });
        let worker = Worker::new(server.url.clone(), VecDeque::new());
        let agent = worker.agent();
        agent
            .send(
                "Find exact RIP; no writes.".into(),
                17,
                true,
                json!({"pane":17}),
            )
            .unwrap();
        let events = settled(agent, |request| {
            agent
                .respond(request, Ok(format!("RIP 0x1234 {}", "v".repeat(40 * 1024))))
                .unwrap()
        });
        assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
        assert!(events.iter().any(|event|matches!(event,Event::Notice(text) if text.contains("checkpoint output limit reached"))));
        assert!(!events.iter().any(|event|matches!(event,Event::Text(text) if text.contains("PRIVATE reasoning") || text.contains("PARTIAL CHECKPOINT"))));
        assert!(events.iter().any(
            |event| matches!(event,Event::Text(text) if text == "RIP verified\n\nExact archived observations plus recent originals.")
        ));
        for turn in worker.finish() {
            paired(&turn.messages);
        }
        server.finish();
    }
}

#[test]
fn finish_goal_delivers_submitted_answer_verbatim_and_blocks_later_batch_actions() {
    let summary = "运行观察：sudo_check_suid 提前退出。\n静态分析：尚未执行 parse_args。";
    let evidence = "RIP 0x1234；推断与已验证结果分开，未证明系统插件不可用。";
    let expected = format!("{summary}\n\n{evidence}");
    let server = Server::new(1, move |_, body, stream| {
        assert!(body["tools"].as_array().unwrap().iter().any(|tool| {
            tool["name"] == "finish_goal"
                && tool["description"]
                    .as_str()
                    .unwrap()
                    .contains("shown verbatim")
        }));
        sse(
            stream,
            response(
                "",
                &[
                    (
                        "finished",
                        "finish_goal",
                        json!({"summary":summary,"evidence":evidence}),
                    ),
                    (
                        "late-input",
                        "send_terminal",
                        json!({"text":"must not execute\n"}),
                    ),
                    (
                        "late-finish",
                        "finish_goal",
                        json!({"summary":"must not replace the submitted answer","evidence":"invented evidence"}),
                    ),
                ],
            ),
        );
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    agent
        .send(
            "Inspect; do not rewrite the verified conclusion.".into(),
            17,
            true,
            json!({"pane":17}),
        )
        .unwrap();
    let events = settled(agent, |_| {
        panic!("an action after finish_goal reached the terminal")
    });
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    let text: String = events
        .iter()
        .filter_map(|event| {
            if let Event::Text(text) = event {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(text, expected);
    let final_index = events
        .iter()
        .position(|event| matches!(event,Event::Text(text) if text == &expected))
        .unwrap();
    assert_eq!(
        events[..final_index]
            .iter()
            .filter(|event| matches!(event, Event::Tool { done: true, .. }))
            .count(),
        3
    );
    assert!(
        !events[final_index..]
            .iter()
            .any(|event| matches!(event, Event::Tool { .. }))
    );
    let history = worker.finish();
    let messages = &history[0].messages;
    paired(messages);
    assert_eq!(messages.len(), 4);
    let results = messages[2]["content"].as_array().unwrap();
    assert_eq!(results[0]["is_error"], false);
    for result in &results[1..] {
        assert_eq!(result["is_error"], true);
        assert!(
            result["content"]
                .as_str()
                .unwrap()
                .contains("no further action was executed")
        );
    }
    assert_eq!(messages[3]["content"][0]["text"], expected);
    assert_eq!(server.finish().len(), 1);
}

fn memory_fact(id: &str, statement: &str, context_id: u64, quote: &str) -> Value {
    json!({"id":id,"statement":statement,"kind":"runtime","verified":true,"sources":[{"context_id":context_id,"quote":quote}]})
}

fn working_facts(body: &Value) -> Option<Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|message| {
            message["content"]
                .as_str()
                .filter(|text| text.starts_with("Working memory ("))
                .map(|text| serde_json::from_str(text.split_once('\n').unwrap().1).unwrap())
        })
}

#[test]
fn cited_memory_survives_masking_and_private_checkpoints() {
    let server = Server::new(7, |index, body, stream| {
        paired(body["messages"].as_array().unwrap());
        match index {
            0 => sse(
                stream,
                response(
                    "",
                    &[(
                        "remember",
                        "update_memory",
                        json!({"facts":[memory_fact("entry-pc", "Entry RIP was 0x1234", 1, "RIP 0x1234")]}),
                    )],
                ),
            ),
            1..=4 => {
                assert_eq!(
                    working_facts(body).unwrap()["facts"][0]["statement"],
                    "Entry RIP was 0x1234"
                );
                sse(
                    stream,
                    response(
                        &"a".repeat(20 * 1024),
                        &[(&format!("read-{index}"), "read_terminal", json!({}))],
                    ),
                );
            }
            5 => {
                assert!(
                    body["system"]
                        .as_str()
                        .unwrap()
                        .starts_with("Summarize historical")
                );
                assert_eq!(
                    working_facts(body).unwrap()["facts"][0]["sources"][0]["context_id"],
                    1
                );
                sse(
                    stream,
                    response(
                        "Older observations archived; entry PC source remains 1.",
                        &[],
                    ),
                );
            }
            6 => {
                assert_eq!(
                    working_facts(body).unwrap()["facts"][0]["statement"],
                    "Entry RIP was 0x1234"
                );
                assert!(
                    body["messages"]
                        .to_string()
                        .contains("Historical checkpoint")
                );
                sse(
                    stream,
                    response(
                        "",
                        &[(
                            "finish",
                            "finish_goal",
                            json!({"summary":"Entry PC verified.","evidence":"Historical source 1: RIP 0x1234."}),
                        )],
                    ),
                );
            }
            _ => unreachable!(),
        }
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    agent
        .send(
            "Inspect entry PC.".into(),
            17,
            true,
            json!({"target":{"pane":17},"raw_text":"RIP 0x1234"}),
        )
        .unwrap();
    let events = settled(agent, |request| {
        agent.respond(request, Ok("v".repeat(40 * 1024))).unwrap()
    });
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    assert!(events.iter().any(|event| matches!(event, Event::Text(text) if text == "Entry PC verified.\n\nHistorical source 1: RIP 0x1234.")));
    for turn in worker.finish() {
        paired(&turn.messages);
    }
    server.finish();
}

#[test]
fn memory_rejects_fabricated_quotes_self_citations_and_verified_inference_atomically() {
    let server = Server::new(5, |index, body, stream| {
        let last = body["messages"].as_array().unwrap().last().unwrap();
        match index {
            0 => sse(
                stream,
                response(
                    "",
                    &[(
                        "bad",
                        "update_memory",
                        json!({"facts":[memory_fact("pc", "PC", 1, "RIP 0x1234"),memory_fact("fabricated", "Fake PC", 1, "RIP 0x9999")]}),
                    )],
                ),
            ),
            1 => {
                assert!(
                    working_facts(body).is_none(),
                    "failed batch committed partial memory"
                );
                assert_eq!(last["content"][0]["is_error"], true);
                assert!(last.to_string().contains("does not match"));
                sse(
                    stream,
                    response(
                        "",
                        &[(
                            "valid",
                            "update_memory",
                            json!({"facts":[memory_fact("pc", "PC 0x1234", 1, "RIP 0x1234")]}),
                        )],
                    ),
                );
            }
            2 => {
                assert_eq!(
                    working_facts(body).unwrap()["facts"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
                let acknowledgment: Value =
                    serde_json::from_str(last["content"][0]["content"].as_str().unwrap()).unwrap();
                sse(
                    stream,
                    response(
                        "",
                        &[(
                            "circular",
                            "update_memory",
                            json!({"facts":[memory_fact("circular", "Host verified me", acknowledgment["context_id"].as_u64().unwrap(), "updated")]}),
                        )],
                    ),
                );
            }
            3 => {
                assert_eq!(last["content"][0]["is_error"], true);
                assert_eq!(working_facts(body).unwrap()["facts"][0]["id"], "pc");
                sse(
                    stream,
                    response(
                        "",
                        &[(
                            "infer",
                            "update_memory",
                            json!({"facts":[{"id":"inference","statement":"A branch ran","kind":"inference","verified":true,"sources":[{"context_id":1,"quote":"RIP 0x1234"}]}]}),
                        )],
                    ),
                );
            }
            4 => {
                assert_eq!(last["content"][0]["is_error"], true);
                assert_eq!(
                    working_facts(body).unwrap()["facts"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
                sse(stream, response("Only the sourced PC remains.", &[]));
            }
            _ => unreachable!(),
        }
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    agent
        .send(
            "Remember PC.".into(),
            17,
            false,
            json!({"raw_text":"RIP 0x1234"}),
        )
        .unwrap();
    let events = settled(agent, |_| panic!("unexpected terminal query"));
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    worker.finish();
    server.finish();
}

#[test]
fn rewinding_restores_memory_before_the_selected_user_turn() {
    let server = Server::new(6, |index, body, stream| match index {
        0 => sse(
            stream,
            response(
                "",
                &[(
                    "remember-old",
                    "update_memory",
                    json!({"facts":[memory_fact("pc", "old PC 0x1234", 1, "RIP 0x1234")]}),
                )],
            ),
        ),
        1 => sse(stream, response("Old PC recorded.", &[])),
        2 => sse(
            stream,
            response(
                "",
                &[(
                    "remember-new",
                    "update_memory",
                    json!({"facts":[memory_fact("pc", "new PC 0x1235", 3, "RIP 0x1235")]}),
                )],
            ),
        ),
        3 => {
            assert_eq!(
                working_facts(body).unwrap()["facts"][0]["statement"],
                "new PC 0x1235"
            );
            sse(stream, response("New PC recorded.", &[]));
        }
        4 => {
            assert_eq!(
                working_facts(body).unwrap()["facts"][0]["statement"],
                "old PC 0x1234"
            );
            sse(stream, response("Restored old memory.", &[]));
        }
        5 => {
            assert!(working_facts(body).is_none());
            sse(stream, response("No historical memory.", &[]));
        }
        _ => unreachable!(),
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    let first = agent
        .send("Old.".into(), 17, false, json!({"raw_text":"RIP 0x1234"}))
        .unwrap();
    settled(agent, |_| panic!("unexpected terminal query"));
    let second = agent
        .send("New.".into(), 17, false, json!({"raw_text":"RIP 0x1235"}))
        .unwrap();
    settled(agent, |_| panic!("unexpected terminal query"));
    agent.rewind(second).unwrap();
    assert!(rewound(agent, second).is_none());
    agent
        .send(
            "Replace second.".into(),
            17,
            false,
            json!({"raw_text":"RIP 0x1236"}),
        )
        .unwrap();
    settled(agent, |_| panic!("unexpected terminal query"));
    agent.rewind(first).unwrap();
    assert!(rewound(agent, first).is_none());
    agent
        .send("Fresh.".into(), 17, false, json!({"raw_text":"RIP 0x1237"}))
        .unwrap();
    settled(agent, |_| panic!("unexpected terminal query"));
    worker.finish();
    server.finish();
}

#[test]
fn memory_quotes_decode_json_escapes_without_normalizing_observed_text() {
    let mut archive = Archive::new().unwrap();
    let text = "RIP \"寄存器\"\t0x1234";
    let snapshot = archive
        .target(
            json!({"target":{"pane":17},"screen":{"rows":[[0,text]]}}),
            true,
        )
        .unwrap();
    let id = snapshot["screen_context_id"].as_u64().unwrap() as usize;
    archive.check_quote(id, text).unwrap();
    assert!(archive.check_quote(id, "RIP \"寄存器\" 0x1234").is_err());
    let synthetic = archive.store(text).unwrap();
    assert!(archive.check_quote(synthetic, text).is_err());
}

#[test]
fn observation_expiry_prompts_memory_then_acknowledgment_clears_the_ephemeral_reminder() {
    let server = Server::new(5, |index, body, stream| {
        paired(body["messages"].as_array().unwrap());
        match index {
            0..=2 => sse(
                stream,
                response(
                    "",
                    &[(&format!("read-{index}"), "read_terminal", json!({}))],
                ),
            ),
            3 => {
                let last = body["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(last["content"][0]["type"], "tool_result");
                assert!(last["content"].as_array().unwrap().iter().any(|block| {
                    block["text"]
                        .as_str()
                        .is_some_and(|text| text.starts_with("Recent observations will soon"))
                }));
                sse(
                    stream,
                    response(
                        "",
                        &[(
                            "remember",
                            "update_memory",
                            json!({"facts":[memory_fact("pc", "Observed PC", 3, "RIP 0x1234")]}),
                        )],
                    ),
                );
            }
            4 => {
                assert!(working_facts(body).is_some());
                assert!(
                    !body["messages"]
                        .to_string()
                        .contains("Recent observations will soon")
                );
                sse(stream, response("Source-backed memory retained.", &[]));
            }
            _ => unreachable!(),
        }
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    agent
        .send("Inspect the PC.".into(), 17, false, json!({"pane":17}))
        .unwrap();
    let events = settled(agent, |request| {
        agent.respond(request, Ok("RIP 0x1234".into())).unwrap()
    });
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    for turn in worker.finish() {
        assert!(
            !serde_json::to_string(&turn.messages)
                .unwrap()
                .contains("Recent observations will soon")
        );
        paired(&turn.messages);
    }
    server.finish();
}

#[test]
fn retrieved_observations_keep_the_original_citation_id_for_memory() {
    let server = Server::new(3, |index, body, stream| match index {
        0 => sse(
            stream,
            response("", &[("retrieve", "read_context", json!({"context_id":1}))]),
        ),
        1 => {
            let last = body["messages"].as_array().unwrap().last().unwrap();
            let original: Value =
                serde_json::from_str(last["content"][0]["content"].as_str().unwrap()).unwrap();
            assert_eq!(original["context_id"], 1);
            assert_eq!(original["text"], "RIP 0x1234");
            assert!(original.get("content").is_none());
            sse(
                stream,
                response(
                    "",
                    &[(
                        "remember",
                        "update_memory",
                        json!({"facts":[memory_fact("pc", "Sourced PC 0x1234", 1, "RIP 0x1234")]}),
                    )],
                ),
            );
        }
        2 => {
            assert_eq!(
                working_facts(body).unwrap()["facts"][0]["sources"][0]["context_id"],
                1
            );
            sse(stream, response("Remembered original source.", &[]));
        }
        _ => unreachable!(),
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    agent
        .send(
            "Remember retrieved PC.".into(),
            17,
            false,
            json!({"raw_text":"RIP 0x1234"}),
        )
        .unwrap();
    let events = settled(agent, |_| panic!("unexpected terminal read"));
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    worker.finish();
    server.finish();
}

#[test]
fn memory_row_citations_extract_original_unicode_and_spacing_without_model_copying() {
    let server = Server::new(2, |index, body, stream| match index {
        0 => sse(
            stream,
            response(
                "",
                &[(
                    "remember",
                    "update_memory",
                    json!({"facts":[{"id":"rip","statement":"The observed RIP was 0x1234","kind":"runtime","verified":true,"sources":[{"context_id":2,"row":5}]}]}),
                )],
            ),
        ),
        1 => {
            let fact = &working_facts(body).unwrap()["facts"][0];
            assert_eq!(fact["sources"][0]["row"], 5);
            assert_eq!(fact["sources"][0]["quote"], "RIP \"寄存器\"\t0x1234");
            sse(stream, response("Original row retained.", &[]));
        }
        _ => unreachable!(),
    });
    let worker = Worker::new(server.url.clone(), VecDeque::new());
    let agent = worker.agent();
    agent.send("Remember RIP.".into(), 17, false, json!({"target":{"pane":17},"screen":{"rows":[[5,"RIP \"寄存器\"\t0x1234"]]},"raw_text":"raw RIP"})).unwrap();
    let events = settled(agent, |_| panic!("unexpected terminal query"));
    assert!(events.iter().all(|event| !matches!(event, Event::Error(_))));
    worker.finish();
    server.finish();
}
