use super::*;
use std::collections::HashSet;
use std::io::Read;

fn test_client(stream: UnixStream) -> Client {
    Client::new(
        Peer::new(stream).unwrap(),
        None,
        ClientEndpoint::Local(PathBuf::new()),
    )
}

fn wait(client: &mut Client, mut condition: impl FnMut(&Client, &[Response]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let events = client.poll().unwrap();
        if condition(client, &events) {
            return;
        }
        client.request_screen().unwrap();
        assert!(Instant::now() < deadline, "workspace condition timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn wait_snapshot(client: &mut Client, condition: impl Fn(&Snapshot) -> bool) {
    wait(client, |client, _| {
        client.snapshot().is_some_and(&condition)
    });
}
fn wait_response(client: &mut Client, found: impl Fn(&Response) -> bool) {
    wait(client, |_, events| events.iter().any(&found));
}
fn wait_detached(client: &mut Client) {
    wait_response(client, |event| matches!(event, Response::Detached));
}
fn wait_error(client: &mut Client) {
    wait_response(client, |event| matches!(event, Response::Error(_)));
}
fn wait_error_containing(client: &mut Client, text: &str) {
    wait_response(
        client,
        |event| matches!(event, Response::Error(error) if error.contains(text)),
    );
}
/// Waits for the next copy and returns its text.
fn wait_copy(client: &mut Client) -> String {
    let mut text = None;
    wait(client, |_, events| {
        for event in events {
            if let Response::Clipboard {
                file, incomplete, ..
            } = event
            {
                assert!(!incomplete);
                let mut copied = String::new();
                file.try_clone()
                    .unwrap()
                    .read_to_string(&mut copied)
                    .unwrap();
                text = Some(copied);
            }
        }
        text.is_some()
    });
    text.unwrap()
}
/// Starts a search and returns its match count once the daemon answers.
fn search(client: &mut Client, pane: PaneId, request: u64, query: &str) -> u32 {
    let query = query.into();
    client
        .send(Action::Search {
            pane,
            request,
            query,
        })
        .unwrap();
    let mut count = None;
    wait(client, |_, events| {
        for event in events {
            if let Response::Search {
                request: answered,
                count: found,
                ..
            } = event
                && *answered == request
            {
                count = Some(*found);
            }
        }
        count.is_some()
    });
    count.unwrap()
}
fn input(client: &mut Client, pane: PaneId, data: impl Into<Vec<u8>>) {
    let data = data.into();
    client.send(Action::Input { pane, data }).unwrap();
}
fn select(client: &mut Client, pane: PaneId, from: Point, to_column: usize) {
    for action in [
        Action::SelectionStart {
            pane,
            point: from,
            side: Side::Left,
            kind: SelectionType::Simple,
        },
        Action::SelectionUpdate {
            pane,
            point: Point {
                column: to_column,
                ..from
            },
            side: Side::Right,
        },
    ] {
        client.send(action).unwrap();
    }
}
fn rect(client: &Client, pane: PaneId) -> Rect {
    client.snapshot().unwrap().pane(pane).unwrap().rect
}
fn row_text(screen: &Screen, row: usize) -> String {
    screen.row(row).cells.iter().map(|cell| cell.c).collect()
}
fn grid_contains(client: &Client, needle: &str) -> bool {
    client
        .screens()
        .values()
        .any(|screen| (0..screen.screen_lines()).any(|row| row_text(screen, row).contains(needle)))
}
fn program(path: &str, args: &[&str]) -> CreateOptions {
    CreateOptions {
        program: Some(Shell::new(
            path.into(),
            args.iter().map(|arg| arg.to_string()).collect(),
        )),
        ..CreateOptions::default()
    }
}
fn python(script: &str) -> CreateOptions {
    python_with(script, &[])
}
fn python_with(script: &str, args: &[&str]) -> CreateOptions {
    program("python3", &[&["-c", script], args].concat())
}
fn create(client: &mut Client, name: &str, options: CreateOptions) {
    let name = name.into();
    client.send(Action::Create { name, options }).unwrap();
    wait_snapshot(client, |_| true);
}
fn start(server: &TestServer, name: &str, options: CreateOptions) -> Client {
    let mut client = Client::connect(&server.socket()).unwrap();
    create(&mut client, name, options);
    client
}
fn attach(server: &TestServer, name: &str) -> Client {
    let mut client = Client::connect(&server.socket()).unwrap();
    client.send(Action::Attach { name: name.into() }).unwrap();
    wait_snapshot(&mut client, |_| true);
    client
}
fn terminate(client: &mut Client, name: &str) {
    client.send(Action::TerminateName(name.into())).unwrap();
    wait_detached(client);
}

#[test]
fn search_reads_cold_history_and_restores_view_and_selection() {
    let server = TestServer::start();
    let size = WindowSize {
        num_cols: 30,
        num_lines: 6,
        ..CreateOptions::default().size
    };
    let script = "import os,tty; tty.setraw(0); [os.write(1,(('normal-only NEEDLE' if i==50 else f'line-{i:04d}')+'\\r\\n').encode()) for i in range(3000)]; os.write(1,b'SEARCH_READY\\r\\n'); os.read(0,1); os.write(1,b'\\x1b[?1049hALT_ONLY'); os.read(0,1)";
    let mut client = start(
        &server,
        "search",
        CreateOptions {
            size,
            ..python(script)
        },
    );
    wait(&mut client, |client, _| {
        grid_contains(client, "SEARCH_READY")
    });
    let pane = client.snapshot().unwrap().focused;
    let ready = (0..6)
        .find(|&row| row_text(&client.screens()[&pane], row).contains("SEARCH_READY"))
        .unwrap();
    select(
        &mut client,
        pane,
        Point {
            line: ready as i32,
            column: 0,
        },
        5,
    );
    wait(&mut client, |client, _| {
        client.screens()[&pane].selection_range().is_some()
    });
    let original = client.screens()[&pane].selection_range();

    search(&mut client, pane, 1, "");
    assert_eq!(search(&mut client, pane, 2, "NEEDLE"), 1);
    let select_match = Action::SearchSelect {
        pane,
        request: 2,
        index: 0,
    };
    client.send(select_match).unwrap();
    wait(&mut client, |client, _| {
        grid_contains(client, "NEEDLE") && client.screens()[&pane].display_offset() > 2500
    });
    client
        .send(Action::SearchCancel { pane, request: 2 })
        .unwrap();
    wait(&mut client, |client, _| {
        client.screens()[&pane].display_offset() == 0
            && client.screens()[&pane].selection_range() == original
    });

    // The alternate screen has no history to search.
    input(&mut client, pane, "a");
    wait(&mut client, |client, _| {
        client.screens()[&pane].mode().contains(Mode::ALT_SCREEN)
    });
    search(&mut client, pane, 3, "");
    assert_eq!(search(&mut client, pane, 4, "normal-only"), 0);
}

#[test]
fn clients_search_independently_and_read_only_viewers_cannot_select() {
    let server = TestServer::start();
    let script = "import os,tty; tty.setraw(0); [os.write(1,b'INDEPENDENT\\r\\n') for _ in range(3000)]; os.write(1,b'READY\\r\\n'); os.read(0,1)";
    let mut viewer = start(&server, "independent", python(script));
    wait(&mut viewer, |client, _| grid_contains(client, "READY"));
    let pane = viewer.snapshot().unwrap().focused;
    let mut controller = attach(&server, "independent");
    wait_snapshot(&mut viewer, |snapshot| !snapshot.controller);
    assert_eq!(search(&mut viewer, pane, 1, "INDEPENDENT"), 3000);
    assert_eq!(search(&mut controller, pane, 1, "INDEPENDENT"), 3000);
    let select_match = Action::SearchSelect {
        pane,
        request: 1,
        index: 0,
    };
    viewer.send(select_match.clone()).unwrap();
    wait_error(&mut viewer);
    assert_eq!(search(&mut viewer, pane, 2, &"界".repeat(2000)), 0);
    controller.send(select_match).unwrap();
    wait(&mut controller, |client, _| {
        client.screens()[&pane].display_offset() > 2500
    });
}

#[test]
fn cursor_metadata_repaints_frontend_rows_without_transferring_cell_data() {
    let size = WindowSize {
        num_cols: 8,
        num_lines: 3,
        ..CreateOptions::default().size
    };
    let mut core = crate::core::Core::new(size, 0xffffff, 0);
    let mut parser = crate::vt::Parser::new();
    parser.advance(&mut core, b"abcd");
    let mut screen = Screen::default();
    screen.apply(Update::capture(&core, true));
    for (input, dirty) in [
        ("\x1b[3;2H", vec![0, 2]),
        ("\x1b[?25l", vec![2]),
        ("\x1b[5 q", vec![]),
        ("\x1b[?25h", vec![2]),
        ("\x1b7\x1b[2;1H", vec![1, 2]),
        ("\x1b8", vec![1, 2]),
        ("\x1b[1;1H", vec![0, 2]),
        ("\n", vec![0, 1]),
        ("\r", vec![]),
        ("\x1b[?1000h", vec![]),
    ] {
        core.reset_damage();
        screen.reset_damage();
        parser.advance(&mut core, input.as_bytes());
        let update = Update::capture(&core, false);
        assert!(update.rows.is_empty());
        screen.apply(update);
        assert_eq!(screen.cursor(), core.cursor());
        assert_eq!(screen.mode(), core.mode());
        assert_eq!(screen.dirty_rows().collect::<Vec<_>>(), dirty);
        assert_eq!(screen.row(0).cells[0].c, 'a');
    }
}

#[test]
fn cancelling_pending_wrap_updates_hover_output_without_moving_cursor() {
    let server = TestServer::start();
    let size = WindowSize {
        num_cols: 7,
        num_lines: 4,
        ..CreateOptions::default().size
    };
    let script = "import os,tty,time; tty.setraw(0); os.write(1,b'\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07ABCDEFG'); os.read(0,1); os.write(1,b'\x1b[1;7H'); time.sleep(60)";
    let mut client = start(
        &server,
        "hover",
        CreateOptions {
            size,
            ..python(script)
        },
    );
    let state = |client: &Client| {
        let pane = client.snapshot()?.panes.first()?;
        let screen = client.screens().get(&pane.id)?;
        Some((
            screen.cursor().0,
            screen.row_output(pane, 0).is_some(),
            screen.row(0).cells[6].c,
        ))
    };
    let cursor = Point { line: 0, column: 6 };
    wait(&mut client, |client, _| {
        state(client) == Some((cursor, true, 'G'))
    });
    let pane = client.snapshot().unwrap().focused;
    client.screen_mut(pane).unwrap().reset_damage();
    input(&mut client, pane, "x");
    wait(&mut client, |client, _| {
        state(client) == Some((cursor, false, 'G'))
    });
    assert_eq!(
        client.screens()[&pane].dirty_rows().collect::<Vec<_>>(),
        [0]
    );
}

#[test]
fn remote_handshake_reaps_failed_commands_and_rejects_versions() {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 23"]);
        sender
            .send(
                Client::connect_remote(command, "test-host".into())
                    .err()
                    .unwrap(),
            )
            .unwrap();
    });
    let error = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("failed SSH command kept its socket open");
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    let mut command = Command::new("sh");
    command.arg("-c").arg(r"printf '\001\000\000\000\377'");
    let error = Client::connect_remote(command, "test-host".into())
        .err()
        .unwrap();
    assert!(error.to_string().contains("version 255"));
}

#[test]
fn dropping_remote_frontend_reaps_its_transport() {
    let mut command = Command::new("sh");
    command.arg("-c").arg(format!(
        r"printf '\001\000\000\000\{:03o}'; exec sleep 60",
        crate::wire::VERSION
    ));
    let client = Client::connect_remote(command, "test-host".into()).unwrap();
    let pid = client.ssh.as_ref().unwrap().0.id();
    drop(client);
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

#[test]
fn server_child() {
    let Some(directory) = std::env::var_os("NEXTTY_TEST_SERVER_DIR").map(PathBuf::from) else {
        return;
    };
    run_server(&directory.join("server.sock")).unwrap();
}

struct TestServer {
    child: std::process::Child,
    directory: PathBuf,
}
impl TestServer {
    fn start() -> Self {
        Self::start_with_fd_limit(None)
    }
    fn start_with_fd_limit(fd_limit: Option<u64>) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Sockets need a short path, whatever the checkout depth.
        let directory =
            std::env::temp_dir().join(format!("nextty-server-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let home = directory.join("home");
        fs::create_dir(&home).unwrap();
        let log = File::create(directory.join("server.log")).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "workspace::tests::server_child", "--nocapture"])
            .env("NEXTTY_TEST_SERVER_DIR", &directory)
            .env("XDG_CACHE_HOME", &directory)
            .env("HOME", &home)
            .env("ZDOTDIR", &home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        if let Some(limit) = fd_limit {
            unsafe {
                command.pre_exec(move || {
                    let limit = libc::rlimit {
                        rlim_cur: limit,
                        rlim_max: limit,
                    };
                    match libc::setrlimit(libc::RLIMIT_NOFILE, &limit) {
                        0 => Ok(()),
                        _ => Err(io::Error::last_os_error()),
                    }
                });
            }
        }
        let child = command.spawn().unwrap();
        let result = Self { child, directory };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !result.socket().exists() || !result.directory.join("nextty/history").is_dir() {
            assert!(Instant::now() < deadline, "server did not finish starting");
            std::thread::sleep(Duration::from_millis(5));
        }
        result
    }
    fn socket(&self) -> PathBuf {
        self.directory.join("server.sock")
    }
    fn home(&self) -> PathBuf {
        self.directory.join("home")
    }
    fn history_entries(&self) -> usize {
        fs::read_dir(self.directory.join("nextty/history"))
            .unwrap()
            .count()
    }
    fn wait_for_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(Instant::now() < deadline, "server did not finish cleanup");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn server_ignores_probes_and_exits_when_the_first_workspace_fails() {
    let mut server = TestServer::start();
    drop(UnixStream::connect(server.socket()).unwrap());
    let mut client = Client::connect(&server.socket()).unwrap();
    let options = CreateOptions {
        directory: Some(server.directory.join("missing")),
        ..CreateOptions::default()
    };
    let name = "failing".into();
    client.send(Action::Create { name, options }).unwrap();
    wait_error(&mut client);
    drop(client);
    server.wait_for_exit();
}

#[test]
fn actions_queued_behind_closing_the_last_pane_do_not_crash_the_server() {
    let mut server = TestServer::start();
    let mut client = start(&server, "closing", program("/bin/sleep", &["30"]));
    let pane = client.snapshot().unwrap().focused;
    client.send(Action::ClosePane(pane)).unwrap();
    client.send(Action::Tab(1)).unwrap();
    wait_detached(&mut client);
    drop(client);
    server.wait_for_exit();
}

#[test]
fn closing_a_hup_ignoring_child_does_not_block_the_server() {
    let mut server = TestServer::start();
    let mut client = start(
        &server,
        "closing",
        program("/bin/sh", &["-c", "trap '' HUP; while :; do :; done"]),
    );
    let pane = client.snapshot().unwrap().focused;
    client.send(Action::ClosePane(pane)).unwrap();
    wait_detached(&mut client);
    drop(client);
    server.wait_for_exit();
}

#[test]
fn server_survives_accept_fd_exhaustion() {
    let mut server = TestServer::start_with_fd_limit(Some(40));
    let _client = start(&server, "persist", program("/bin/sh", &["-i"]));
    let connections: Vec<_> = (0..64)
        .map(|_| UnixStream::connect(server.socket()).unwrap())
        .collect();
    let log = server.directory.join("server.log");
    let failures = || {
        let text = fs::read_to_string(&log).unwrap();
        text.matches("workspace accept failed").count()
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while failures() == 0 {
        assert!(server.child.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "accept did not hit the fd limit");
        std::thread::sleep(Duration::from_millis(5));
    }
    let logged = failures();
    std::thread::sleep(Duration::from_millis(250));
    assert!(failures() <= logged + 2, "accept retried without backoff");
    drop(connections);

    let mut probe = Client::connect(&server.socket()).unwrap();
    probe.send(Action::List).unwrap();
    wait_response(
        &mut probe,
        |event| matches!(event, Response::List(list) if list.iter().any(|workspace| workspace.name == "persist")),
    );
}

fn drain_agent(client: &mut Client) -> Vec<AgentEvent> {
    client.agent_events.drain(..).collect()
}
/// Queues `request` and returns the token the daemon hands out for starting the owner.
fn agent_start(frontend: &mut Client, request: AgentRequest) -> Vec<u8> {
    frontend.send(Action::AgentRequest(request)).unwrap();
    wait(frontend, |client, _| !client.agent_events.is_empty());
    drain_agent(frontend)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::Start(token) => Some(token),
            _ => None,
        })
        .unwrap()
}
fn agent_attach(server: &TestServer, name: &str, token: Vec<u8>) -> Client {
    let mut owner = Client::connect(&server.socket()).unwrap();
    let name = name.into();
    owner.send(Action::AgentAttach { name, token }).unwrap();
    owner
}
fn attach_agent_owner(server: &TestServer, frontend: &mut Client, name: &str) -> Client {
    let token = agent_start(frontend, AgentRequest::Abort { session: 0 });
    let mut owner = agent_attach(server, name, token);
    wait_snapshot(&mut owner, |_| true);
    owner
}
fn agent_write(
    client: &mut Client,
    pane: PaneId,
    request: u64,
    generation: u64,
    prompt: Option<u64>,
    data: &[u8],
) {
    let data = data.to_vec();
    let write = Action::AgentInput {
        pane,
        request,
        generation,
        prompt,
        data,
    };
    client.send(write).unwrap();
}
fn agent_input(owner: &mut Client, pane: PaneId, request: u64, data: &[u8]) {
    let generation = owner
        .snapshot()
        .unwrap()
        .pane(pane)
        .unwrap()
        .input_generation;
    agent_write(owner, pane, request, generation, None, data);
}

#[test]
fn agent_owner_restores_state_and_keeps_background_pty_without_control() {
    let server = TestServer::start();
    let options = program("/bin/sh", &["-i"]);
    let mut frontend = start(&server, "persist", options.clone());
    let pane = frontend.snapshot().unwrap().focused;
    let draft_view = AgentDrafts {
        next_id: 1,
        complete: true,
        ..AgentDrafts::default()
    };
    let edited_view = AgentDrafts {
        next_id: 2,
        ..AgentDrafts::default()
    };
    let stored_view = AgentDrafts {
        complete: true,
        ..edited_view.clone()
    };
    let first_request = AgentRequest::Abort { session: 1 };
    let next_request = AgentRequest::Rewind {
        session: 1,
        turn: 2,
    };
    frontend
        .send(Action::AgentView(draft_view.clone()))
        .unwrap();
    let token = agent_start(&mut frontend, first_request.clone());
    let mut owner = agent_attach(&server, "persist", token.clone());
    wait_snapshot(&mut owner, |_| true);
    assert_eq!(
        drain_agent(&mut owner),
        [
            AgentEvent::Attached,
            AgentEvent::Draft(draft_view),
            AgentEvent::Request(first_request)
        ]
    );
    assert!(!owner.snapshot().unwrap().controller);
    assert!(owner.can_agent_input());
    assert!(frontend.snapshot().unwrap().controller);
    // The owner may only use the Agent actions.
    owner.send(Action::NewTab { pane }).unwrap();
    wait_error(&mut owner);
    frontend
        .send(Action::AgentView(edited_view.clone()))
        .unwrap();
    frontend
        .send(Action::AgentRequest(next_request.clone()))
        .unwrap();
    frontend.flush().unwrap();
    // Draft edits stay between frontends; the owner only receives the request.
    wait(&mut owner, |client, _| !client.agent_events.is_empty());
    assert_eq!(drain_agent(&mut owner), [AgentEvent::Request(next_request)]);
    wait_error(&mut agent_attach(&server, "persist", token.clone()));

    let message = |text: &str| AgentMessage {
        turn: 1,
        role: "Agent".into(),
        text: text.into(),
        id: None,
    };
    let initial = AgentModelPatch {
        session: 1,
        revision: 1,
        changes: Some(
            vec![AgentChange {
                turn: 1,
                tool_id: "1:0:0:edit".into(),
                path: "project/a.rs".into(),
                added: 1,
                removed: 1,
                diff: "-old\n+new\n".into(),
                ..AgentChange::default()
            }]
            .into(),
        ),
        messages: vec![message("live-")],
        ..AgentModelPatch::default()
    };
    let tail = AgentModelPatch {
        session: 1,
        revision: 1,
        prefix_bytes: 5,
        messages: vec![message("models")],
        ..AgentModelPatch::default()
    };
    let mut terminal_only = attach(&server, "persist");
    owner.send(Action::AgentUpdate(initial.clone())).unwrap();
    owner.send(Action::AgentUpdate(tail.clone())).unwrap();
    owner.flush().unwrap();
    wait(&mut frontend, |client, _| {
        client.agent_events.back() == Some(&AgentEvent::Model(tail.clone()))
    });
    assert_eq!(
        drain_agent(&mut frontend),
        [
            AgentEvent::Model(initial.clone()),
            AgentEvent::Model(tail.clone())
        ]
    );
    let mut full_model = initial;
    tail.apply_messages(&mut full_model.messages).unwrap();
    // A viewer hears about the Agent only once its panel is open.
    terminal_only.send(Action::List).unwrap();
    wait_response(&mut terminal_only, |event| {
        matches!(event, Response::List(_))
    });
    assert!(terminal_only.agent_events.is_empty());
    let open_events = [
        AgentEvent::Draft(stored_view.clone()),
        AgentEvent::Model(full_model),
        AgentEvent::Ready,
    ];
    terminal_only.send(Action::AgentOpen).unwrap();
    wait(&mut terminal_only, |client, _| {
        client.agent_events.contains(&AgentEvent::Ready)
    });
    assert_eq!(drain_agent(&mut terminal_only), open_events);
    // Frontends showing the panel share every draft edit.
    terminal_only
        .send(Action::AgentView(edited_view.clone()))
        .unwrap();
    terminal_only.flush().unwrap();
    wait(&mut frontend, |client, _| !client.agent_events.is_empty());
    assert_eq!(drain_agent(&mut frontend), [AgentEvent::Draft(edited_view)]);
    drop(terminal_only);

    wait_snapshot(&mut frontend, |snapshot| snapshot.controller);
    frontend.send(Action::NewTab { pane }).unwrap();
    wait_snapshot(&mut frontend, |snapshot| {
        snapshot.tabs.len() == 2 && snapshot.panes.len() == 1
    });
    wait_snapshot(&mut owner, |snapshot| snapshot.panes.len() == 2);
    agent_input(
        &mut owner,
        pane,
        9,
        b"printf '\\142ackground-agent-ok\\n'\n",
    );
    wait(&mut owner, |client, _| {
        grid_contains(client, "background-agent-ok")
    });
    assert!(!grid_contains(&frontend, "background-agent-ok"));

    let attached = attach(&server, "persist");
    wait_snapshot(&mut frontend, |snapshot| !snapshot.controller);
    for action in [
        Action::AgentView(AgentDrafts::default()),
        Action::AgentRequest(AgentRequest::Abort { session: 0 }),
        Action::Input {
            pane,
            data: b"bad".to_vec(),
        },
    ] {
        frontend.send(action).unwrap();
        wait_error(&mut frontend);
    }
    frontend.send(Action::AgentOpen).unwrap();
    wait(&mut frontend, |client, _| {
        client.agent_events.contains(&AgentEvent::Ready)
    });
    assert_eq!(drain_agent(&mut frontend), open_events);
    drop(attached);
    wait_snapshot(&mut frontend, |snapshot| snapshot.controller);
    drop(frontend);
    agent_input(&mut owner, pane, 9, b"printf '\\144etached-agent-ok\\n'\n");
    wait(&mut owner, |client, _| {
        grid_contains(client, "detached-agent-ok")
    });
    assert!(!owner.snapshot().unwrap().controller);

    // Ending the workspace ends the owner, and a new workspace of the same name starts clean.
    let mut keeper = start(&server, "keeper", options.clone());
    keeper
        .send(Action::TerminateName("persist".into()))
        .unwrap();
    keeper.flush().unwrap();
    wait_detached(&mut owner);
    let mut fresh = start(&server, "persist", options);
    fresh.send(Action::AgentOpen).unwrap();
    wait(&mut fresh, |client, _| !client.agent_events.is_empty());
    assert_eq!(drain_agent(&mut fresh), [AgentEvent::Ready]);
    wait_error(&mut agent_attach(&server, "persist", token));
    terminate(&mut keeper, "keeper");
}

#[test]
fn main_and_alternate_screens_keep_full_pty_width() {
    let server = TestServer::start();
    let mut size = WindowSize {
        num_cols: 80,
        num_lines: 24,
        ..CreateOptions::default().size
    };
    let script = r#"
import os, signal, tty
tty.setraw(0)
def report(*args):
    cols, rows = os.get_terminal_size(0)
    os.write(1, f'\x1b[2J\x1b[H{cols}x{rows}\x1b[1;{cols}H#'.encode())
signal.signal(signal.SIGWINCH, report)
report()
while True:
    command = os.read(0, 1)
    os.write(1, b'\x1b[?1049h' if command == b'a' else b'\x1b[?1049l')
    report()
"#;
    let mut client = Client::connect(&server.socket()).unwrap();
    client
        .send(Action::Create {
            name: "persist".into(),
            options: CreateOptions {
                size,
                ..python(script)
            },
        })
        .unwrap();
    let check = |client: &mut Client, size: WindowSize, alternate: bool| {
        let (columns, rows) = (size.num_cols as usize, size.num_lines as usize - 1);
        wait(client, |client, _| {
            let Some(screen) = client
                .snapshot()
                .and_then(|s| client.screens().get(&s.focused))
            else {
                return false;
            };
            screen.columns() == columns
                && screen.screen_lines() == rows
                && screen.mode().contains(Mode::ALT_SCREEN) == alternate
                && screen.row(0).cells[columns - 1].c == '#'
                && grid_contains(client, &format!("{columns}x{rows}"))
        });
    };
    check(&mut client, size, false);
    let pane = client.snapshot().unwrap().focused;
    for _ in 0..2 {
        input(&mut client, pane, "a");
        check(&mut client, size, true);
        size.num_cols += 10;
        size.num_lines += 2;
        client.send(Action::Resize(size)).unwrap();
        check(&mut client, size, true);
        input(&mut client, pane, "m");
        check(&mut client, size, false);
    }
}

fn assert_same_screen(left: &Client, right: &Client, pane: PaneId) {
    let (left, right) = (&left.screens()[&pane], &right.screens()[&pane]);
    assert_eq!(left.palette, right.palette);
    assert_eq!(left.cursor(), right.cursor());
    assert_eq!(left.mode(), right.mode());
    assert_eq!(left.screen_lines(), right.screen_lines());
    for row in 0..left.screen_lines() {
        let (a, b) = (left.row(row), right.row(row));
        assert_eq!(a.cells, b.cells, "row {row}");
        assert_eq!(
            (a.extras(), a.used(), a.wrapped),
            (b.extras(), b.used(), b.wrapped)
        );
    }
}

#[test]
fn automatic_session_names_are_atomic_reusable_and_attachable() {
    let server = TestServer::start();
    let options = program(
        "/bin/sh",
        &["-c", "printf 'numbered-session-marker\\n'; exec /bin/sh"],
    );
    let _named = start(&server, "2", options.clone());

    let barrier = Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (socket, options, barrier) = (server.socket(), options.clone(), barrier.clone());
            std::thread::spawn(move || {
                let mut client = Client::connect(&socket).unwrap();
                barrier.wait();
                let name = String::new();
                client.send(Action::Create { name, options }).unwrap();
                wait(&mut client, |client, _| {
                    grid_contains(client, "numbered-session-marker")
                });
                client
            })
        })
        .collect();
    let clients: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let mut names: Vec<usize> = clients
        .iter()
        .map(|client| client.snapshot().unwrap().name.parse().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, [0, 1, 3, 4, 5, 6, 7, 8]);

    let mut control = Client::connect(&server.socket()).unwrap();
    control
        .send(Action::Create {
            name: "0".into(),
            options: options.clone(),
        })
        .unwrap();
    wait_error_containing(&mut control, "workspace already exists");
    terminate(&mut control, "1");
    create(&mut control, "", options.clone());
    assert_eq!(control.snapshot().unwrap().name, "1");

    drop(clients);
    let mut attached = attach(&server, "0");
    wait(&mut attached, |client, _| {
        grid_contains(client, "numbered-session-marker")
    });
    assert!(attached.snapshot().unwrap().controller);
    let _custom = start(&server, "dev", options.clone());
    let mut takeover = attach(&server, "0");
    wait_snapshot(&mut attached, |snapshot| !snapshot.controller);
    attached.send(Action::Terminate).unwrap();
    attached.flush().unwrap();
    wait_detached(&mut takeover);

    let mut reused = start(&server, "", options);
    assert_eq!(reused.snapshot().unwrap().name, "0");
    // The old frontend has not polled Detached and still displays the old 0.
    attached.send(Action::Terminate).unwrap();
    wait_detached(&mut attached);
    let pane = reused.snapshot().unwrap().focused;
    input(&mut reused, pane, "printf '\\162eused-session-alive\\n'\n");
    wait(&mut reused, |client, _| {
        grid_contains(client, "reused-session-alive")
    });
}

#[test]
fn attached_frontends_agree_after_sparse_text_and_palette_changes() {
    use crate::vt::{Color, Rgb};
    let server = TestServer::start();
    let script =
        "stty raw -echo; printf 'ready'; while IFS= read -r line; do printf '%b' \"$line\"; done";
    let mut first = start(&server, "persist", program("/bin/sh", &["-c", script]));
    wait(&mut first, |client, _| grid_contains(client, "ready"));
    let pane = first.snapshot().unwrap().focused;
    let color = |client: &Client| client.screens()[&pane].color(Color::Indexed(1));
    input(
        &mut first,
        pane,
        b"\\033[H\\033[2Jpalette-base\\033]4;1;#112233\\007\n",
    );
    wait(&mut first, |client, _| {
        grid_contains(client, "palette-base")
            && color(client)
                == Rgb {
                    r: 0x11,
                    g: 0x22,
                    b: 0x33,
                }
    });
    input(&mut first, pane, b"\\033[3;1Hsparse-before-attach\n");
    wait(&mut first, |client, _| {
        grid_contains(client, "sparse-before-attach")
    });
    let mut second = attach(&server, "persist");
    wait(&mut second, |client, _| {
        grid_contains(client, "sparse-before-attach")
    });
    wait_snapshot(&mut first, |snapshot| !snapshot.controller);
    assert_same_screen(&first, &second, pane);
    input(
        &mut second,
        pane,
        b"\\033[4;1Hafter-attach\\033]4;1;#445566\\007\n",
    );
    for client in [&mut second, &mut first] {
        wait(client, |client, _| {
            grid_contains(client, "after-attach")
                && color(client)
                    == Rgb {
                        r: 0x44,
                        g: 0x55,
                        b: 0x66,
                    }
        });
    }
    assert_same_screen(&first, &second, pane);
}

#[test]
fn daemon_preserves_pty_and_transfers_control_across_frontends() {
    let mut server = TestServer::start();
    let options = program(
        "/bin/sh",
        &["-c", "printf 'persisted-marker\\n'; exec /bin/sh"],
    );
    let mut first = start(&server, "persist", options);
    wait(&mut first, |client, _| {
        grid_contains(client, "persisted-marker")
    });
    let pane = first.snapshot().unwrap().focused;
    let mut second = attach(&server, "persist");
    wait_snapshot(&mut second, |snapshot| snapshot.controller);
    wait_snapshot(&mut first, |snapshot| !snapshot.controller);
    input(&mut first, pane, "denied-input");
    wait_error_containing(&mut first, "read-only");
    let outside = Action::SelectionStart {
        pane,
        point: Point {
            line: 0,
            column: 4096,
        },
        side: Side::Left,
        kind: SelectionType::Semantic,
    };
    second.send(outside).unwrap();
    wait_error_containing(&mut second, "outside the pane");
    second
        .send(Action::Split {
            axis: Axis::LeftRight,
        })
        .unwrap();
    wait_snapshot(&mut second, |snapshot| snapshot.panes.len() == 2);
    let split = second.snapshot().unwrap().focused;
    second.send(Action::NewTab { pane: split }).unwrap();
    wait_snapshot(&mut second, |snapshot| {
        snapshot.tabs.len() == 2 && snapshot.panes.len() == 1
    });
    drop(second);
    wait_snapshot(&mut first, |snapshot| snapshot.controller);
    first.send(Action::Tab(-1)).unwrap();
    wait_snapshot(&mut first, |snapshot| snapshot.panes.len() == 2);
    drop(first);
    let mut third = attach(&server, "persist");
    wait(&mut third, |client, _| {
        grid_contains(client, "persisted-marker")
    });
    terminate(&mut third, "persist");
    drop(third);
    server.wait_for_exit();
    assert_eq!(
        server.history_entries(),
        0,
        "termination must clean workspace history"
    );
}

fn split(client: &mut Client, axis: Axis) -> PaneId {
    let panes = client.snapshot().unwrap().panes.len() + 1;
    client.send(Action::Split { axis }).unwrap();
    wait_snapshot(client, |snapshot| snapshot.panes.len() == panes);
    client.snapshot().unwrap().focused
}
/// Opens a tab; its id is that of its first pane.
fn new_tab(client: &mut Client, pane: PaneId) -> PaneId {
    let tabs = client.snapshot().unwrap().tabs.len() + 1;
    client.send(Action::NewTab { pane }).unwrap();
    wait_snapshot(client, |snapshot| snapshot.tabs.len() == tabs);
    client.snapshot().unwrap().focused
}

#[test]
fn scrolling_during_history_resize_survives_the_new_layout() {
    let server = TestServer::start();
    let size = WindowSize {
        num_cols: 100,
        num_lines: 25,
        ..CreateOptions::default().size
    };
    let script = "import os,tty; tty.setraw(0); os.write(1, b'history '+b'x'*70+b'\\r\\n'); [os.write(1,(str(i)+' '+ 'x'*70+'\\r\\n').encode()) for i in range(100000)]; os.write(1,b'READY'); os.read(0,1)";
    let mut client = start(
        &server,
        "resize-scroll",
        CreateOptions {
            size,
            ..python(script)
        },
    );
    wait(&mut client, |client, _| grid_contains(client, "READY"));
    let pane = client.snapshot().unwrap().focused;
    let resize = Action::Resize(WindowSize {
        num_cols: 101,
        ..size
    });
    client.send(resize).unwrap();
    let scroll = Scroll::Delta(10);
    client.send(Action::Scroll { pane, scroll }).unwrap();
    wait(&mut client, |client, _| {
        let screen = &client.screens()[&pane];
        screen.columns() == 101 && screen.display_offset() == 10
    });
}

#[test]
fn dragging_nested_dividers_resizes_pty_and_preserves_other_splits() {
    let server = TestServer::start();
    let size = WindowSize {
        num_cols: 101,
        num_lines: 32,
        ..CreateOptions::default().size
    };
    let options = CreateOptions {
        size,
        ..program("/bin/sleep", &["60"])
    };
    let mut client = start(&server, "drag", options);
    let first = client.snapshot().unwrap().focused;
    let second = split(&mut client, Axis::LeftRight);
    let third = split(&mut client, Axis::TopBottom);
    let vertical = client.snapshot().unwrap().divider_at(50, 25).unwrap();
    assert_eq!((vertical.first, vertical.second), (first, third));
    client.send(vertical.action(30, 25)).unwrap();
    wait(&mut client, |c, _| rect(c, first).cols == 30);
    assert_eq!(rect(&client, second).x, 31);
    assert_eq!(rect(&client, third).x, 31);
    let horizontal = client.snapshot().unwrap().divider_at(60, 16).unwrap();
    client.send(horizontal.action(60, 10)).unwrap();
    wait(&mut client, |c, _| {
        rect(c, second).rows == 9
            && c.screens()
                .get(&second)
                .is_some_and(|screen| screen.screen_lines() == 9)
    });
    assert_eq!(rect(&client, first).cols, 30);
    assert_eq!(rect(&client, third).y, 11);
    client.send(vertical.action(0, 0)).unwrap();
    wait(&mut client, |c, _| {
        rect(c, first).cols == 2
            && c.screens()
                .get(&first)
                .is_some_and(|screen| screen.columns == 2)
    });
    client.send(vertical.action(30, 25)).unwrap();
    wait(&mut client, |c, _| rect(c, first).cols == 30);
    client
        .send(Action::Resize(WindowSize {
            num_cols: 201,
            ..size
        }))
        .unwrap();
    wait(&mut client, |c, _| rect(c, first).cols == 61);
    assert_eq!(rect(&client, second).rows, 9);
}

#[test]
fn focused_splits_follow_queued_actions() {
    let server = TestServer::start();
    let size = WindowSize {
        num_cols: 103,
        num_lines: 31,
        ..CreateOptions::default().size
    };
    let options = CreateOptions {
        size,
        ..program("/bin/sleep", &["60"])
    };
    let mut client = start(&server, "persist", options);
    let first = client.snapshot().unwrap().focused;
    let original = rect(&client, first);
    // The next action is queued behind a focus change that is not yet visible.
    let queue_split = |client: &mut Client, axis| {
        client.send(Action::Split { axis }).unwrap();
        let panes = client.snapshot().unwrap().panes.len() + 1;
        wait_snapshot(client, |snapshot| snapshot.panes.len() == panes);
        client.snapshot().unwrap().focused
    };
    for axis in [Axis::LeftRight, Axis::TopBottom] {
        client.send(Action::Split { axis }).unwrap();
    }
    wait_snapshot(&mut client, |snapshot| snapshot.panes.len() == 3);
    let third = client.snapshot().unwrap().focused;
    let others = &client.snapshot().unwrap().panes;
    let second = others
        .iter()
        .find(|pane| pane.id != first && pane.id != third)
        .unwrap()
        .id;
    assert_eq!(rect(&client, first).rows, original.rows);
    assert_eq!(
        (rect(&client, second).x, rect(&client, second).rows),
        (52, 14)
    );
    assert_eq!((rect(&client, third).x, rect(&client, third).y), (52, 16));
    let (right_top, right_bottom) = (rect(&client, second), rect(&client, third));

    client.send(Action::Focus(first)).unwrap();
    let fourth = queue_split(&mut client, Axis::TopBottom);
    assert_eq!(
        (rect(&client, second), rect(&client, third)),
        (right_top, right_bottom)
    );
    assert_eq!(rect(&client, first).rows, 14);
    let left_bottom = rect(&client, fourth);
    assert_eq!((left_bottom.x, left_bottom.y), (0, 16));

    client.send(Action::Navigate(Direction::Right)).unwrap();
    let fifth = queue_split(&mut client, Axis::LeftRight);
    assert_eq!(rect(&client, fourth), left_bottom);
    assert_eq!(rect(&client, second), right_top);
    assert_eq!(
        (rect(&client, third).x, rect(&client, third).cols),
        (right_bottom.x, 25)
    );
    assert_eq!(rect(&client, fifth).x, 78);
    let fifth_rect = rect(&client, fifth);

    client.send(Action::Focus(second)).unwrap();
    let sixth = queue_split(&mut client, Axis::TopBottom);
    assert_eq!(rect(&client, fifth), fifth_rect);
    assert_eq!(rect(&client, second).rows, 6);
    let sixth = rect(&client, sixth);
    assert_eq!((sixth.x, sixth.y, sixth.rows), (right_top.x, 8, 7));
}

#[test]
fn focus_changes_are_reported_to_applications_that_ask() {
    let server = TestServer::start();
    let script = "import os,tty; tty.setraw(0); os.write(1,b'\x1b[?1004hREADY'); [os.write(1,b' got-'+os.read(0,3).hex().encode()) for _ in range(2)]; os.read(0,1)";
    let mut client = start(&server, "focus", python(script));
    wait(&mut client, |client, _| grid_contains(client, "READY"));
    let first = client.snapshot().unwrap().focused;
    split(&mut client, Axis::LeftRight);
    wait(&mut client, |client, _| grid_contains(client, "got-1b5b4f"));
    client.send(Action::Focus(first)).unwrap();
    wait(&mut client, |client, _| grid_contains(client, "got-1b5b49"));
}

#[test]
fn queued_selection_survives_history_resize() {
    let server = TestServer::start();
    let size = WindowSize {
        num_cols: 100,
        num_lines: 25,
        ..CreateOptions::default().size
    };
    let script = "import time; print(('history line ' + 'x' * 70 + '\\n') * 6000, end=''); print('READY', flush=True); time.sleep(60)";
    let mut client = start(
        &server,
        "persist",
        CreateOptions {
            size,
            ..python(script)
        },
    );
    wait(&mut client, |client, _| grid_contains(client, "READY"));
    let pane = client.snapshot().unwrap().focused;
    assert!(client.screens()[&pane].history_size() > 4000);
    // Sent back to back, without a frame request between them.
    for action in [
        Action::Resize(WindowSize {
            num_cols: 80,
            ..size
        }),
        Action::SelectionStart {
            pane,
            point: Point { line: 0, column: 0 },
            side: Side::Left,
            kind: SelectionType::Simple,
        },
        Action::SelectionUpdate {
            pane,
            point: Point { line: 0, column: 8 },
            side: Side::Right,
        },
    ] {
        client.peer.send_action(&action).unwrap();
    }
    client.flush().unwrap();
    wait(&mut client, |client, _| {
        client.screens()[&pane].columns() == 80
            && client.screens()[&pane].selection_range().is_some()
    });
    for action in [
        Action::Resize(WindowSize {
            num_cols: 90,
            ..size
        }),
        Action::Input {
            pane,
            data: b"x".to_vec(),
        },
    ] {
        client.peer.send_action(&action).unwrap();
    }
    client.flush().unwrap();
    wait(&mut client, |client, _| {
        client.screens()[&pane].columns() == 90
    });
    assert!(client.screens()[&pane].selection_range().is_none());
}

#[test]
fn starting_selection_in_a_second_pane_clears_the_first_and_copies_the_new_text() {
    let server = TestServer::start();
    let options = program("/bin/sh", &["-c", "printf 'firstcopy\\n'; exec /bin/sh"]);
    let mut client = start(&server, "persist", options);
    wait(&mut client, |client, _| grid_contains(client, "firstcopy"));
    let first = client.snapshot().unwrap().focused;
    let second = split(&mut client, Axis::LeftRight);
    input(&mut client, second, "printf '\\163econdcopy\\n'\n");
    wait(&mut client, |client, _| grid_contains(client, "secondcopy"));
    let screen = &client.screens()[&second];
    let line = (0..screen.screen_lines())
        .find(|&line| row_text(screen, line).contains("secondcopy"))
        .unwrap();
    let point = Point {
        line: line as i32 - screen.display_offset() as i32,
        column: row_text(screen, line).find("secondcopy").unwrap(),
    };
    select(&mut client, first, Point::default(), 8);
    wait(&mut client, |client, _| {
        client.screens()[&first].selection_range().is_some()
    });
    select(&mut client, second, point, point.column + 9);
    wait(&mut client, |client, _| {
        client.screens()[&first].selection_range().is_none()
            && client.screens()[&second].selection_range().is_some()
    });
    client
        .send(Action::CopySelection {
            pane: second,
            primary: false,
        })
        .unwrap();
    assert_eq!(wait_copy(&mut client), "secondcopy");
    client
        .send(Action::CopySelection {
            pane: first,
            primary: true,
        })
        .unwrap();
    wait_response(
        &mut client,
        |event| matches!(event, Response::CopyError(error) if error.contains("no selection")),
    );
}

#[test]
fn tab_titles_follow_the_focused_pane_directory_instead_of_application_titles() {
    let server = TestServer::start();
    let home = server.home();
    fs::write(home.join(".zshrc"), "").unwrap();
    fs::create_dir(home.join("alpha")).unwrap();
    fs::create_dir(home.join("beta")).unwrap();
    let options = program(
        "/bin/sh",
        &["-c", "printf 'directory-ready\\n'; exec /bin/sh"],
    );
    let mut client = start(&server, "persist", options);
    wait(&mut client, |client, _| {
        grid_contains(client, "directory-ready")
    });
    let tab_title =
        |client: &Client, tab: usize| client.snapshot().unwrap().tabs[tab].title.clone();
    assert_eq!(tab_title(&client, 0), "home");
    let first = client.snapshot().unwrap().focused;
    let change_directory = |client: &mut Client, pane, directory, application, title: &str| {
        let command =
            format!("cd {directory}; printf '\\033]2;{application}\\007{application}\\n'\n");
        input(client, pane, command);
        wait_snapshot(client, |snapshot| {
            // The shell can report its application title before its next cwd update.
            snapshot
                .pane(pane)
                .is_some_and(|view| view.title == application)
                && snapshot
                    .tabs
                    .iter()
                    .any(|tab| tab.id == snapshot.active_tab && tab.title == title)
        });
    };
    change_directory(&mut client, first, "alpha", "first-application", "alpha");
    let split = split(&mut client, Axis::LeftRight);
    wait_snapshot(&mut client, |snapshot| snapshot.tabs[0].title == "alpha");
    change_directory(&mut client, split, "../beta", "split-application", "beta");
    client.send(Action::Focus(first)).unwrap();
    wait_snapshot(&mut client, |snapshot| {
        snapshot.focused == first && snapshot.tabs[0].title == "alpha"
    });
    client.send(Action::NewTab { pane: split }).unwrap();
    wait_snapshot(&mut client, |snapshot| {
        snapshot.tabs.len() == 2 && snapshot.tabs[1].title == "beta"
    });
    let second = client.snapshot().unwrap().focused;
    assert_eq!(tab_title(&client, 0), "alpha");
    change_directory(&mut client, second, "/", "root-application", "/");
    client.send(Action::Tab(-1)).unwrap();
    client.send(Action::Focus(split)).unwrap();
    wait_snapshot(&mut client, |snapshot| snapshot.focused == split);
    assert_eq!(
        (tab_title(&client, 0), tab_title(&client, 1)),
        ("beta".into(), "/".into())
    );
    assert_eq!(
        client.snapshot().unwrap().pane(split).unwrap().title,
        "split-application"
    );
}

#[test]
fn closing_an_inactive_pane_preserves_the_middle_active_tab() {
    for natural_exit in [true, false] {
        let server = TestServer::start();
        let mut client = start(&server, "persist", program("/bin/sh", &["-i"]));
        let first = client.snapshot().unwrap().focused;
        let (second, third) = (new_tab(&mut client, first), new_tab(&mut client, first));
        client.send(Action::Tab(-1)).unwrap();
        wait_snapshot(&mut client, |snapshot| snapshot.active_tab == second);
        let focused = client.snapshot().unwrap().focused;
        if natural_exit {
            input(&mut client, first, "exit\n");
        } else {
            client.send(Action::ClosePane(first)).unwrap();
        }
        wait_snapshot(&mut client, |snapshot| snapshot.tabs.len() == 2);
        let snapshot = client.snapshot().unwrap();
        assert_eq!(
            snapshot.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            [second, third]
        );
        assert_eq!((snapshot.active_tab, snapshot.focused), (second, focused));
    }
}

#[test]
fn closing_an_inactive_tab_terminates_its_panes_and_preserves_active_tab() {
    let mut server = TestServer::start();
    let mut client = start(&server, "persist", program("/bin/sh", &[]));
    let first = client.snapshot().unwrap().active_tab;
    let split = split(&mut client, Axis::LeftRight);
    let second = new_tab(&mut client, split);
    let active = new_tab(&mut client, second);
    client.send(Action::CloseTab(first)).unwrap();
    wait_snapshot(&mut client, |snapshot| snapshot.tabs.len() == 2);
    assert_eq!(client.snapshot().unwrap().active_tab, active);
    client.send(Action::List).unwrap();
    wait_response(
        &mut client,
        |event| matches!(event, Response::List(items) if items.len() == 1 && items[0].panes == 2),
    );
    client.send(Action::CloseTab(second)).unwrap();
    wait_snapshot(&mut client, |snapshot| snapshot.tabs.len() == 1);
    assert_eq!(client.snapshot().unwrap().active_tab, active);
    client.send(Action::CloseTab(active)).unwrap();
    wait_detached(&mut client);
    drop(client);
    server.wait_for_exit();
    assert_eq!(server.history_entries(), 0);
}

#[test]
fn command_copy_streams_full_output_across_cold_history() {
    let server = TestServer::start();
    let script = "printf '\\033]133;A\\aprompt \\033]133;B\\acommand\\r\\n\\033]133;C\\a'; i=0; while [ \"$i\" -lt 700 ]; do printf 'line-%04d 中\\n' \"$i\"; i=$((i+1)); done; printf '\\033]133;D;0\\a\\033]133;A\\aprompt \\033]133;B\\a'; exec /bin/sh";
    let mut client = start(&server, "persist", program("/bin/sh", &["-c", script]));
    wait(&mut client, |client, _| grid_contains(client, "line-0699"));
    wait_snapshot(&mut client, |snapshot| {
        snapshot.panes.iter().any(|pane| {
            pane.command_blocks
                .iter()
                .any(|block| block.status == Some(0))
        })
    });
    let pane = &client.snapshot().unwrap().panes[0];
    let block = pane
        .blocks
        .iter()
        .filter(|block| block.output.is_some())
        .max_by_key(|block| block.id)
        .unwrap()
        .id;
    assert!(
        pane.command_blocks
            .iter()
            .any(|command| command.id == block && command.complete && command.has_output)
    );
    let pane = pane.id;
    // A resize queued ahead of the copy must not change what is copied.
    for action in [
        Action::Resize(WindowSize {
            num_cols: 64,
            ..CreateOptions::default().size
        }),
        Action::CopyBlock { pane, block },
    ] {
        client.peer.send_action(&action).unwrap();
    }
    client.flush().unwrap();
    let output = wait_copy(&mut client);
    assert!(output.starts_with("line-0000 中\n"));
    assert!(output.ends_with("line-0699 中\n"));
    assert_eq!(output.lines().count(), 700);
}

fn blocked_input_client(server: &TestServer) -> Client {
    let script = "stty raw -echo; printf 'blocked-ready\\n'; exec sleep 30";
    let mut client = start(server, "persist", program("/bin/sh", &["-c", script]));
    wait(&mut client, |client, _| {
        grid_contains(client, "blocked-ready")
    });
    let pane = client.snapshot().unwrap().focused;
    input(&mut client, pane, vec![b'x'; 8 * 1024 * 1024]);
    let deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < deadline {
        client.flush().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        client.wants_write(),
        "a child that never reads must apply bounded backpressure"
    );
    client
}
fn process_cpu_ticks(pid: u32) -> u64 {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<_> = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

#[test]
fn failed_full_broadcast_disconnects_without_another_terminal_wakeup() {
    let mut server = TestServer::start();
    let ready = server.directory.join("oversized-ready");
    // Keep a multi-chunk clipboard frame pending while an oversized title arrives.
    let script = r"
import base64, os, sys, time
os.write(1, b'\x1b]52;c;' + base64.b64encode(b'x' * (11 * 128 * 1024)) + b'\x07')
os.write(1, b'\x1b]2;' + b't' * (33 * 1024 * 1024) + b'\x07')
open(sys.argv[1], 'w').write('ready')
time.sleep(60)
";
    let mut client = Client::connect(&server.socket()).unwrap();
    let options = python_with(script, &[ready.to_str().unwrap()]);
    client
        .send(Action::Create {
            name: "oversized".into(),
            options,
        })
        .unwrap();
    client.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "oversized output fixture stalled"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    // Let the terminal become idle while its outgoing clipboard frame is backpressured.
    std::thread::sleep(Duration::from_millis(100));
    let deadline = Instant::now() + Duration::from_secs(1);
    let disconnected = loop {
        let _ = client.request_screen();
        if client.poll().is_err() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
    };
    drop(client);
    let mut control = Client::connect(&server.socket()).unwrap();
    terminate(&mut control, "oversized");
    drop(control);
    server.wait_for_exit();
    let log = fs::read_to_string(server.directory.join("server.log")).unwrap();
    assert!(log.contains("frame limit"), "{log}");
    assert!(
        disconnected,
        "failed broadcast waited for another wakeup: {log}"
    );
}

#[test]
fn disconnecting_first_client_preserves_later_poll_results() {
    let server = TestServer::start();
    let mut first = Client::connect(&server.socket()).unwrap();
    let mut second = Client::connect(&server.socket()).unwrap();
    for client in [&mut first, &mut second] {
        client.send(Action::List).unwrap();
        wait_response(client, |event| matches!(event, Response::List(_)));
    }
    let pid = server.child.id() as libc::pid_t;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) },
        pid
    );
    assert!(libc::WIFSTOPPED(status));
    first.send(Action::Detach).unwrap();
    first.flush().unwrap();
    second.send(Action::List).unwrap();
    second.flush().unwrap();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
    wait_response(&mut second, |event| matches!(event, Response::List(_)));
}

#[test]
fn dropping_a_backpressured_frontend_leaves_the_daemon_idle_and_attachable() {
    let server = TestServer::start();
    drop(blocked_input_client(&server));
    std::thread::sleep(Duration::from_millis(50));
    let before = process_cpu_ticks(server.child.id());
    std::thread::sleep(Duration::from_millis(200));
    let ticks = process_cpu_ticks(server.child.id()) - before;
    assert!(
        ticks <= 2,
        "closed backpressured frontend spun the daemon: {ticks} CPU ticks in 200 ms"
    );
    let mut replacement = attach(&server, "persist");
    wait_snapshot(&mut replacement, |snapshot| snapshot.controller);
    terminate(&mut replacement, "persist");
}

#[test]
fn terminating_a_workspace_cancels_old_paste_after_detached_is_received() {
    let server = TestServer::start();
    let mut client = blocked_input_client(&server);
    let mut control = Client::connect(&server.socket()).unwrap();
    terminate(&mut control, "persist");
    client.flush().unwrap();
    wait_detached(&mut client);
    assert!(!client.wants_write());
    assert!(client.snapshot().is_none());
}

#[test]
fn a_complete_control_request_is_processed_even_if_its_socket_closes() {
    let server = TestServer::start();
    let mut client = blocked_input_client(&server);
    let mut control = Client::connect(&server.socket()).unwrap();
    control
        .send(Action::TerminateName("persist".into()))
        .unwrap();
    control.flush().unwrap();
    drop(control);
    wait_detached(&mut client);
    assert!(!client.wants_write());
}

#[test]
fn detached_behind_a_large_response_supersedes_a_known_write_failure() {
    let (left, right) = UnixStream::pair().unwrap();
    let mut sender = Peer::new(left).unwrap();
    assert_eq!(unsafe { libc::shutdown(sender.fd(), libc::SHUT_RD) }, 0);
    let large = Response::ClipboardStore {
        primary: false,
        text: "x".repeat(600000),
    };
    sender.send_response(large).unwrap();
    sender.send_response(Response::Detached).unwrap();
    let worker = std::thread::spawn(move || {
        while sender.pending() {
            sender.flush().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let mut client = test_client(right);
    client.peer.send_input(1, b"pending-input").unwrap();
    client.flush().unwrap();
    assert!(client.write_error.is_some());
    assert!(!client.wants_write());
    let mut copied = false;
    wait(&mut client, |_, events| {
        copied |= events.iter().any(
            |event| matches!(event, Response::ClipboardStore { text, .. } if text.len() == 600000),
        );
        events
            .iter()
            .any(|event| matches!(event, Response::Detached))
    });
    assert!(copied);
    assert!(!client.peer.pending());
    assert!(client.write_error.is_none());
    worker.join().unwrap();
}

#[test]
fn write_failure_without_detached_still_reports_the_original_error() {
    let (left, right) = UnixStream::pair().unwrap();
    drop(left);
    let mut client = test_client(right);
    client.peer.send_input(1, b"input").unwrap();
    client.flush().unwrap();
    assert!(matches!(client.poll(), Err(error) if error.kind() == io::ErrorKind::BrokenPipe));
}

#[test]
fn consecutive_state_notifications_merge_without_skipping_deltas_or_control_events() {
    let (left, right) = UnixStream::pair().unwrap();
    let mut sender = Peer::new(left).unwrap();
    let mut client = test_client(right);
    let size = WindowSize {
        num_cols: 2,
        num_lines: 2,
        ..CreateOptions::default().size
    };
    let snapshot = Snapshot {
        name: "burst".into(),
        controller: true,
        focused: 1,
        active_tab: 1,
        tabs: Vec::new(),
        panes: vec![PaneView {
            id: 1,
            tab: 1,
            rect: Rect {
                cols: 2,
                rows: 2,
                ..Rect::default()
            },
            title: String::new(),
            cwd: None,
            process: Default::default(),
            blocks: Vec::new(),
            command_blocks: Vec::new(),
            input_generation: 0,
            shell_prompt: None,
        }],
    };
    let mut core = crate::core::Core::new(size, 0xffffff, 0);
    let mut parser = crate::vt::Parser::new();
    parser.advance(&mut core, b"A");
    sender
        .send_state(Some(&snapshot), &[(1, Update::capture(&core, true))], 0)
        .unwrap();
    core.reset_damage();
    parser.advance(&mut core, b"\x1b[2;1HB");
    sender
        .send_state(Some(&snapshot), &[(1, Update::capture(&core, false))], 0)
        .unwrap();
    sender.send_state(Some(&snapshot), &[], 0).unwrap();
    sender
        .send_response(Response::Error("marker".into()))
        .unwrap();
    sender.send_state(Some(&snapshot), &[], 0).unwrap();
    sender.flush().unwrap();
    let events = client.poll().unwrap();
    assert!(
        matches!(events.as_slice(), [Response::ScreenDirty, Response::Error(error), Response::ScreenDirty] if error == "marker")
    );
    assert_eq!(client.screens()[&1].row(0).cells[0].c, 'A');
    assert_eq!(client.screens()[&1].row(1).cells[0].c, 'B');
}

#[test]
fn input_queues_a_fresh_screen_request_before_each_payload() {
    let (left, right) = UnixStream::pair().unwrap();
    let mut client = test_client(left);
    let mut peer = Peer::new(right).unwrap();
    client.request_screen().unwrap();
    client.send_input(7, b"\x1b[I").unwrap();
    client
        .send(Action::Input {
            pane: 7,
            data: b"z".to_vec(),
        })
        .unwrap();
    assert!(
        peer.next_action().unwrap().is_none(),
        "nothing is sent before an explicit flush"
    );
    client.flush().unwrap();
    let payloads: [Option<&[u8]>; 3] = [None, Some(b"\x1b[I"), Some(b"z")];
    let mut previous = 0;
    for payload in payloads {
        let Action::ScreenRequest {
            request,
            wait: true,
        } = peer.next_action().unwrap().unwrap()
        else {
            panic!("expected a fresh screen request before input");
        };
        assert!(request > previous);
        previous = request;
        if let Some(payload) = payload {
            let Action::Input { pane: 7, data } = peer.next_action().unwrap().unwrap() else {
                panic!("expected input after its screen request");
            };
            assert_eq!(data, payload);
        }
    }
    assert!(peer.next_action().unwrap().is_none());
}

#[test]
fn forced_screen_read_waits_for_its_own_reply() {
    let server = TestServer::start();
    let mut client = start(&server, "fresh", program("/bin/sleep", &["30"]));
    let snapshot = client.snapshot().unwrap().clone();
    let (left, right) = UnixStream::pair().unwrap();
    client.peer = Peer::new(left).unwrap();
    let mut peer = Peer::new(right).unwrap();
    let mut next_request = |client: &mut Client, force: bool| {
        if force {
            client.refresh_screen().unwrap();
        } else {
            client.request_screen().unwrap();
        }
        client.flush().unwrap();
        let Action::ScreenRequest { request, wait } = peer.next_action().unwrap().unwrap() else {
            panic!("expected a frame request");
        };
        assert_eq!(wait, !force);
        request
    };
    let old = next_request(&mut client, false);
    let fresh = next_request(&mut client, true);
    let newest = next_request(&mut client, true);
    // Only the answer to the newest request settles a forced read.
    let mut answer = |client: &mut Client, request| {
        let state = Response::State(Some(snapshot.clone()), Vec::new(), request);
        peer.send_response(state).unwrap();
        peer.flush().unwrap();
        client.poll().unwrap();
        client.screen_stale()
    };
    assert!(answer(&mut client, old));
    assert!(answer(&mut client, fresh));
    assert!(!answer(&mut client, newest));
}

#[test]
fn initial_pull_attach_error_reports_without_a_snapshot() {
    let server = TestServer::start();
    let mut client = Client::connect(&server.socket()).unwrap();
    client
        .send(Action::Attach {
            name: "missing".into(),
        })
        .unwrap();
    wait_error(&mut client);
    assert!(client.snapshot().is_none());
    assert!(!client.screen_pending);
}

#[test]
fn agent_input_keeps_shell_state_rejects_conflicts_and_pairs_queued_history_results() {
    let server = TestServer::start();
    let home = server.home();
    fs::write(home.join(".zshrc"), "").unwrap();
    fs::write(home.join(".bashrc"), "").unwrap();
    fs::create_dir(home.join("project")).unwrap();
    let mut client = start(&server, "agent-shell", CreateOptions::default());
    wait_snapshot(&mut client, |snapshot| {
        snapshot.panes[0].shell_prompt.is_some()
    });
    let view = |client: &Client| client.snapshot().unwrap().panes[0].clone();
    let pane = view(&client);
    let block = pane.shell_prompt.unwrap();
    let command = b"cd project; export NEXTTY_AGENT_VALUE=same-shell; printf '%s\\n' \"$NEXTTY_AGENT_VALUE\"\n";
    agent_write(
        &mut client,
        pane.id,
        1,
        pane.input_generation,
        Some(block),
        command,
    );
    let mut accepted = false;
    wait(&mut client, |client, events| {
        accepted |= events.iter().any(|event| {
            matches!(event, Response::AgentInput { request: 1, result: Ok(Some(id)) } if *id == block)
        });
        let pane = view(client);
        accepted
            && pane.shell_prompt.is_some()
            && pane
                .command_blocks
                .iter()
                .any(|command| command.id == block && command.status == Some(0))
    });
    assert_eq!(view(&client).cwd, Some(home.join("project")));
    assert!(grid_contains(&client, "same-shell"));
    // The prompt the Agent observed is gone.
    agent_write(
        &mut client,
        pane.id,
        2,
        pane.input_generation,
        Some(block),
        b"printf wrong\n",
    );
    wait_response(&mut client, |event| {
        matches!(
            event,
            Response::AgentInput {
                request: 2,
                result: Err(_)
            }
        )
    });
    input(&mut client, pane.id, "printf user-draft");
    wait_snapshot(&mut client, |snapshot| {
        snapshot.panes[0].input_generation > pane.input_generation + 1
    });
    let generation = view(&client).input_generation;
    assert!(view(&client).shell_prompt.is_none());
    agent_write(
        &mut client,
        pane.id,
        3,
        generation,
        None,
        b"; printf wrong\n",
    );
    wait_response(&mut client, |event| {
        matches!(
            event,
            Response::AgentInput {
                request: 3,
                result: Err(_)
            }
        )
    });
    input(&mut client, pane.id, b"\x15\n");
    wait_snapshot(&mut client, |snapshot| {
        snapshot.panes[0].shell_prompt.is_some()
    });
    for _ in 0..5 {
        let previous = view(&client).shell_prompt;
        input(&mut client, pane.id, ":\n");
        wait_snapshot(&mut client, |snapshot| {
            snapshot.panes[0]
                .shell_prompt
                .is_some_and(|id| Some(id) != previous)
        });
    }
    assert!(
        view(&client)
            .command_blocks
            .iter()
            .all(|command| command.id != block)
    );

    // Replies pair with their requests, in any order, and a missing block fails only its own.
    let read = |request, block, query: Option<&str>| Action::ReadHistory {
        pane: pane.id,
        request,
        block,
        cursor: None,
        query: query.map(str::to_owned),
    };
    let copy = |block| Action::CopyBlock {
        pane: pane.id,
        block,
    };
    for action in [
        read(11, None, Some("same-shell")),
        read(12, Some(u64::MAX), None),
        read(13, Some(block), None),
        copy(block),
        copy(u64::MAX),
    ] {
        client.peer.send_action(&action).unwrap();
    }
    client.flush().unwrap();
    let mut received = HashSet::new();
    let mut copies = 0;
    let mut command_metadata = false;
    wait(&mut client, |_, events| {
        for event in events {
            match event {
                Response::Clipboard {
                    file, incomplete, ..
                } => {
                    assert!(!incomplete);
                    let mut output = String::new();
                    file.try_clone()
                        .unwrap()
                        .read_to_string(&mut output)
                        .unwrap();
                    assert!(output.contains("same-shell"));
                    copies += 1;
                }
                Response::CopyError(error) => {
                    assert!(error.contains("no longer retained"));
                    copies += 1;
                }
                Response::History {
                    request: 12,
                    result,
                } => {
                    assert!(result.as_ref().unwrap_err().contains("no longer retained"));
                    received.insert(12);
                }
                Response::History { request, result } => {
                    let page = result.as_ref().unwrap();
                    assert!(!page.incomplete && page.text.contains("same-shell"));
                    if *request == 13 {
                        let command = page.command.as_ref().unwrap();
                        assert_eq!((command.id, command.status), (block, Some(0)));
                        assert!(command.complete && command.has_output);
                        command_metadata = true;
                    }
                    received.insert(*request);
                }
                Response::Error(error) => panic!("uncorrelated error: {error}"),
                _ => continue,
            };
        }
        received.len() == 3 && copies == 2
    });
    assert_eq!(received, HashSet::from([11, 12, 13]));
    assert!(command_metadata);
}

#[test]
fn switching_workspaces_preserves_pending_history_read_source() {
    let server = TestServer::start();
    let mut client = start(&server, "read-source", CreateOptions::default());
    let pane = client.snapshot().unwrap().focused;
    input(&mut client, pane, "printf 'original-history-source\\n'\n");
    wait(&mut client, |client, _| {
        grid_contains(client, "original-history-source")
    });
    let target = start(&server, "read-target", CreateOptions::default());
    assert!(target.snapshot().unwrap().pane(pane).is_none());
    let read = Action::ReadHistory {
        pane,
        request: 99,
        block: None,
        cursor: None,
        query: Some("original-history-source".into()),
    };
    client.peer.send_action(&read).unwrap();
    client
        .peer
        .send_action(&Action::Attach {
            name: "read-target".into(),
        })
        .unwrap();
    client.flush().unwrap();
    let mut finished = false;
    wait(&mut client, |client, events| {
        for event in events {
            if let Response::History {
                request: 99,
                result,
            } = event
            {
                assert!(
                    result
                        .as_ref()
                        .unwrap()
                        .text
                        .contains("original-history-source")
                );
                finished = true;
            }
        }
        finished
            && client
                .snapshot()
                .is_some_and(|snapshot| snapshot.name == "read-target")
    });
}

#[test]
fn closing_a_pane_completes_its_pending_history_read_without_more_terminal_events() {
    let server = TestServer::start();
    let script = "i=0; while [ $i -lt 4000 ]; do printf 'history-%04d long-query-source\\n' $i; i=$((i+1)); done; printf READY; exec sleep 120";
    let mut client = start(&server, "read-close", program("/bin/sh", &["-c", script]));
    wait(&mut client, |client, _| grid_contains(client, "READY"));
    let pane = client.snapshot().unwrap().focused;
    split(&mut client, Axis::LeftRight);
    let read = Action::ReadHistory {
        pane,
        request: 99,
        block: None,
        cursor: None,
        query: Some("no-such-history-match".into()),
    };
    client.peer.send_action(&read).unwrap();
    client.peer.send_action(&Action::ClosePane(pane)).unwrap();
    client.flush().unwrap();
    let mut finished = false;
    wait(&mut client, |client, events| {
        for event in events {
            if let Response::History {
                request: 99,
                result,
            } = event
            {
                assert!(result.as_ref().unwrap_err().contains("no longer exists"));
                finished = true;
            }
        }
        finished && client.snapshot().unwrap().panes.len() == 1
    });
}

#[test]
fn disk_history_failure_reports_without_closing_workspace_pane() {
    let server = TestServer::start();
    let script = "import os; print('READY'); input(); os.write(1,b'x\\r\\n'*12000); input(); print('ALIVE'); input()";
    let mut client = start(
        &server,
        "history-failure",
        program("python3", &["-u", "-c", script]),
    );
    wait(&mut client, |client, _| grid_contains(client, "READY"));
    let pane = client.snapshot().unwrap().focused;
    let history = fs::read_dir(server.directory.join("nextty/history"))
        .unwrap()
        .next()
        .unwrap();
    fs::remove_dir(history.unwrap().path()).unwrap();
    input(&mut client, pane, "start\n");
    wait_error_containing(&mut client, "disk history disabled");
    input(&mut client, pane, "continue\n");
    wait(&mut client, |client, _| grid_contains(client, "ALIVE"));
    assert_eq!(client.snapshot().unwrap().panes.len(), 1);
    assert_eq!(client.snapshot().unwrap().focused, pane);
}

#[test]
fn actions_for_a_closed_pane_report_an_error_and_the_daemon_keeps_serving() {
    let server = TestServer::start();
    let mut client = start(&server, "closed-pane", program("/bin/sh", &["-i"]));
    let pane = client.snapshot().unwrap().focused;
    split(&mut client, Axis::LeftRight);
    client.send(Action::ClosePane(pane)).unwrap();
    wait_snapshot(&mut client, |snapshot| snapshot.panes.len() == 1);
    for action in [
        Action::Input {
            pane,
            data: b"x".to_vec(),
        },
        Action::Search {
            pane,
            request: 1,
            query: "x".into(),
        },
    ] {
        client.send(action).unwrap();
        wait_error_containing(&mut client, "pane does not exist");
    }
    assert_eq!(client.snapshot().unwrap().panes.len(), 1);
}

#[test]
fn input_capacity_recovery_does_not_publish_unchanged_state() {
    let server = TestServer::start();
    let script = r#"import os,sys,time,tty
from pathlib import Path
root=Path(sys.argv[1])
capacity=int(sys.argv[2])
tty.setraw(0)
def read_n(left):
    while left:
        data=os.read(0,min(left,65536))
        assert data and data==b'x'*len(data)
        left-=len(data)
def wait_for(name):
    while not (root/name).exists():
        time.sleep(.001)
os.write(1,b'READY')
read_n(1)
(root/'armed').touch()
wait_for('drain')
read_n(capacity//4-1)
(root/'drained').touch()
wait_for('finish')
read_n(capacity*3//4+capacity)
os.write(1,b'\r\nDONE')
assert os.read(0,1)==b'y'
os.write(1,b'\r\nAFTER')
time.sleep(30)
"#;
    let (root, capacity) = (
        server.directory.to_str().unwrap(),
        INPUT_CAPACITY.to_string(),
    );
    let mut client = start(&server, "persist", python_with(script, &[root, &capacity]));
    wait(&mut client, |client, _| grid_contains(client, "READY"));
    let pane = client.snapshot().unwrap().focused;
    input(&mut client, pane, vec![b'x'; INPUT_CAPACITY]);
    let deadline = Instant::now() + Duration::from_secs(5);
    // Pumps the connection until `done`, forcing the daemon to publish whatever it has.
    let pump = |client: &mut Client, done: &dyn Fn(&Client) -> bool| {
        while !done(client) {
            client.flush().unwrap();
            client.poll().unwrap();
            client.refresh_screen().unwrap();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    let generation = |client: &Client| client.snapshot().unwrap().panes[0].input_generation;
    pump(&mut client, &|client| {
        generation(client) == 4 && server.directory.join("armed").exists()
    });
    let mut quiet = Instant::now();
    client.refresh_screen().unwrap();
    while quiet.elapsed() < Duration::from_millis(50) {
        if client
            .poll()
            .unwrap()
            .iter()
            .any(|event| matches!(event, Response::ScreenDirty))
        {
            quiet = Instant::now();
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    fs::write(server.directory.join("drain"), b"").unwrap();
    let mut drained_at = None;
    let mut updates = 0;
    while drained_at.is_none_or(|time: Instant| time.elapsed() < Duration::from_millis(100)) {
        let events = client.poll().unwrap();
        updates += events
            .iter()
            .filter(|event| matches!(event, Response::ScreenDirty))
            .count();
        if server.directory.join("drained").exists() {
            drained_at.get_or_insert_with(Instant::now);
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        updates, 0,
        "input capacity recovery published unchanged state"
    );
    input(&mut client, pane, vec![b'x'; INPUT_CAPACITY]);
    pump(&mut client, &|client| generation(client) != 4);
    fs::write(server.directory.join("finish"), b"").unwrap();
    pump(&mut client, &|client| grid_contains(client, "DONE"));
    input(&mut client, pane, "y");
    wait(&mut client, |client, _| grid_contains(client, "AFTER"));
}

#[test]
fn tab_activity_tracks_commands_even_when_the_tab_is_hidden() {
    let server = TestServer::start();
    let script = "import os,tty,subprocess; tty.setraw(0); child=subprocess.Popen(['/bin/sleep','30']); os.write(1,b'\x1b]133;A\x07$ \x1b]133;B\x07'); os.read(0,1); os.write(1,b'\x1b]133;C\x07running'); os.read(0,1); os.write(1,b'\x1b]133;D;7\x07\x1b]133;A\x07$ \x1b]133;B\x07'); os.read(0,1); child.terminate(); child.wait()";
    let mut client = start(&server, "tab-activity", python(script));
    let activity = |snapshot: &Snapshot| snapshot.tabs[0].activity;
    wait_snapshot(&mut client, |snapshot| {
        activity(snapshot) == TabActivity::Idle
    });
    let pane = client.snapshot().unwrap().focused;
    let tab = client.snapshot().unwrap().active_tab;
    input(&mut client, pane, "r");
    wait_snapshot(&mut client, |snapshot| {
        activity(snapshot) == TabActivity::Running
    });
    new_tab(&mut client, pane);
    assert_ne!(client.snapshot().unwrap().active_tab, tab);
    assert_eq!(activity(client.snapshot().unwrap()), TabActivity::Running);
    input(&mut client, pane, "f");
    wait_snapshot(&mut client, |snapshot| {
        activity(snapshot) == TabActivity::Failed
    });
}

#[test]
fn background_child_keeps_tab_running_until_it_exits() {
    let server = TestServer::start();
    let script = "import os,tty,subprocess; tty.setraw(0); child=subprocess.Popen(['/bin/sleep','30']); os.write(1,b'\x1b]133;A\x07$ \x1b]133;B\x07'); os.read(0,1); child.terminate(); child.wait(); os.write(1,b'\x1b]133;A\x07$ \x1b]133;B\x07'); os.read(0,1)";
    let mut client = start(&server, "background-child", python(script));
    wait_snapshot(&mut client, |snapshot| {
        snapshot.tabs[0].activity == TabActivity::Running
    });
    let pane = client.snapshot().unwrap().focused;
    assert_eq!(
        client.snapshot().unwrap().panes[0].process.has_children,
        Some(true)
    );
    input(&mut client, pane, "x");
    wait_snapshot(&mut client, |snapshot| {
        snapshot.panes[0].process.has_children == Some(false)
    });
    assert_eq!(
        client.snapshot().unwrap().tabs[0].activity,
        TabActivity::Idle
    );
}

#[test]
fn agent_workspace_files_and_new_tab_use_selected_pane_without_changing_user_view() {
    let server = TestServer::start();
    let source = server.home().join("source");
    let project = source.join("project");
    let other = server.directory.join("other");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir(&other).unwrap();
    fs::write(project.join("value.txt"), "selected needle\n").unwrap();
    fs::write(other.join("value.txt"), "user data\n").unwrap();
    let options = CreateOptions {
        directory: Some(source),
        ..program("/bin/sh", &["-i"])
    };
    let mut frontend = start(&server, "persist", options);
    let pane = frontend.snapshot().unwrap().focused;
    input(&mut frontend, pane, "cd project; printf 'cwd-ready\\n'\n");
    wait_snapshot(&mut frontend, |snapshot| {
        snapshot.pane(pane).unwrap().cwd.as_ref() == Some(&project)
    });
    let mut owner = attach_agent_owner(&server, &mut frontend, "persist");
    let focused = new_tab(&mut frontend, pane);
    let active_tab = frontend.snapshot().unwrap().active_tab;
    let command = format!(
        "cd '{}'; i=0; while [ $i -lt 60 ]; do printf 'other-%s\\n' $i; i=$((i+1)); done\n",
        other.display()
    );
    input(&mut frontend, focused, command);
    wait(&mut frontend, |client, _| grid_contains(client, "other-59"));
    wait_snapshot(&mut owner, |snapshot| snapshot.panes.len() == 2);
    assert_eq!(owner.snapshot().unwrap().pane(pane).unwrap().tab, pane);
    assert_eq!(
        owner.snapshot().unwrap().pane(focused).unwrap().tab,
        active_tab
    );

    // Relative paths resolve in the selected pane's directory, not the user's current tab.
    let value = project.join("value.txt");
    for (request, path, operation) in [
        (41, "value.txt".into(), FileOperation::Read { offset: 0 }),
        (
            42,
            ".".into(),
            FileOperation::Search {
                query: "needle".into(),
            },
        ),
        (
            43,
            "value.txt".into(),
            FileOperation::Edit {
                old_text: Some("selected".into()),
                new_text: "edited".into(),
            },
        ),
        (44, value.clone(), FileOperation::Read { offset: 0 }),
    ] {
        let file = Action::AgentFile {
            pane,
            request,
            path,
            operation,
        };
        owner.send(file).unwrap();
    }
    let mut received = Vec::new();
    wait(&mut owner, |_, events| {
        for event in events {
            let Response::AgentFile { request, result } = event else {
                continue;
            };
            match (request, result.as_ref().unwrap()) {
                (41, FileResult::Read { path, text, .. }) => {
                    assert_eq!((path, text.as_str()), (&value, "selected needle\n"));
                }
                (
                    42,
                    FileResult::Search {
                        matches, truncated, ..
                    },
                ) => {
                    assert!(!truncated);
                    assert_eq!(matches.len(), 1);
                    assert_eq!(
                        (matches[0].line, matches[0].text.as_str()),
                        (1, "selected needle")
                    );
                }
                (43, FileResult::Edited { change }) => {
                    assert_eq!(change.path, value);
                    assert_eq!((change.added, change.removed), (1, 1));
                    assert!(change.diff.contains("+edited needle"));
                }
                (44, FileResult::Read { text, .. }) => assert_eq!(text, "edited needle\n"),
                (request, result) => panic!("unexpected file result: {request} {result:?}"),
            }
            received.push(*request);
        }
        received.len() == 4
    });
    assert_eq!(received, [41, 42, 43, 44]);
    assert_eq!(
        fs::read_to_string(other.join("value.txt")).unwrap(),
        "user data\n"
    );

    // A new Agent tab leaves the user's scrolled view and selection alone.
    frontend
        .send(Action::Scroll {
            pane: focused,
            scroll: Scroll::Top,
        })
        .unwrap();
    wait(&mut frontend, |client, _| {
        client.screens()[&focused].display_offset() > 0
    });
    let top = -(frontend.screens()[&focused].display_offset() as i32);
    select(
        &mut frontend,
        focused,
        Point {
            line: top,
            column: 0,
        },
        4,
    );
    wait(&mut frontend, |client, _| {
        client.screens()[&focused].selection.is_some()
    });
    let before = frontend.screens()[&focused].clone();
    owner
        .send(Action::AgentNewTab { pane, request: 52 })
        .unwrap();
    let mut created = None;
    wait(&mut owner, |_, events| {
        for event in events {
            if let Response::AgentNewTab {
                request: 52,
                result,
            } = event
            {
                created = Some(*result.as_ref().unwrap());
            }
        }
        created.is_some()
    });
    let created = created.unwrap();
    wait_snapshot(&mut owner, |snapshot| snapshot.pane(created).is_some());
    let snapshot = owner.snapshot().unwrap();
    let new = snapshot.pane(created).unwrap();
    assert_eq!((new.cwd.as_ref(), new.tab), (Some(&project), created));
    assert_eq!(
        (snapshot.focused, snapshot.active_tab),
        (focused, active_tab)
    );
    wait_snapshot(&mut frontend, |snapshot| snapshot.tabs.len() == 3);
    let snapshot = frontend.snapshot().unwrap();
    assert_eq!(
        (snapshot.focused, snapshot.active_tab, snapshot.panes.len()),
        (focused, active_tab, 1)
    );
    let after = &frontend.screens()[&focused];
    assert_eq!(after.display_offset(), before.display_offset());
    assert_eq!(after.selection, before.selection);
    assert_eq!(after.columns, before.columns);
    assert!(
        after
            .rows
            .iter()
            .map(|row| &row.cells)
            .eq(before.rows.iter().map(|row| &row.cells))
    );
}

#[test]
fn agent_file_cancellation_preserves_fifo_results_and_committed_edits() {
    let server = TestServer::start();
    let path = server.home().join("value.txt");
    fs::write(&path, "original").unwrap();
    let mut frontend = start(&server, "persist", program("/bin/sh", &["-i"]));
    let pane = frontend.snapshot().unwrap().focused;
    let mut owner = attach_agent_owner(&server, &mut frontend, "persist");
    let file = |request, operation| Action::AgentFile {
        pane,
        request,
        path: "value.txt".into(),
        operation,
    };
    let read = FileOperation::Read { offset: 0 };
    let edit = |old_text: Option<&str>, new_text: &str| FileOperation::Edit {
        old_text: old_text.map(str::to_owned),
        new_text: new_text.into(),
    };
    for action in [
        file(1, edit(None, "cancelled")),
        Action::CancelAgentFile { request: 1 },
        file(2, read.clone()),
        file(3, edit(Some("original"), "committed")),
    ] {
        owner.send(action).unwrap();
    }
    let mut received = Vec::new();
    wait(&mut owner, |_, events| {
        for event in events {
            if let Response::AgentFile { request, result } = event {
                match request {
                    1 => assert!(result.as_ref().unwrap_err().contains("cancel")),
                    2 => assert!(
                        matches!(result, Ok(FileResult::Read { text, .. }) if text == "original")
                    ),
                    3 => assert!(matches!(result, Ok(FileResult::Edited { .. }))),
                    _ => panic!("unexpected request"),
                }
                received.push(*request);
            }
        }
        received.len() == 3
    });
    // Cancelling a finished edit does not undo it.
    owner.send(Action::CancelAgentFile { request: 3 }).unwrap();
    owner.send(file(4, read)).unwrap();
    wait_response(
        &mut owner,
        |event| matches!(event, Response::AgentFile { request: 4, result: Ok(FileResult::Read { text, .. }) } if text == "committed"),
    );
    assert_eq!(received, [1, 2, 3]);
    assert_eq!(fs::read_to_string(path).unwrap(), "committed");
}
