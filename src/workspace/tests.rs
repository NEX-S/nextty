use super::*;
use std::collections::HashSet;
use std::io::Read;

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
/// Queues `request`, which asks the frontend to start the owner.
fn agent_start(frontend: &mut Client, request: AgentRequest) {
    frontend.send(Action::AgentRequest(request)).unwrap();
    wait(frontend, |client, _| !client.agent_events.is_empty());
    assert!(drain_agent(frontend).contains(&AgentEvent::Start));
}
fn agent_attach(server: &TestServer, name: &str) -> Client {
    let mut owner = Client::connect(&server.socket()).unwrap();
    let name = name.into();
    owner.send(Action::AgentAttach { name }).unwrap();
    owner
}
fn attach_agent_owner(server: &TestServer, frontend: &mut Client, name: &str) -> Client {
    agent_start(frontend, AgentRequest::Abort { session: 0 });
    let mut owner = agent_attach(server, name);
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
    agent_start(&mut frontend, first_request.clone());
    let mut owner = agent_attach(&server, "persist");
    wait_snapshot(&mut owner, |_| true);
    assert_eq!(
        drain_agent(&mut owner),
        [
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
    // One owner per workspace.
    wait_error(&mut agent_attach(&server, "persist"));

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
fn automatic_session_names_reuse_the_lowest_free_number() {
    let server = TestServer::start();
    let options = program("/bin/sh", &[]);
    let _named = start(&server, "1", options.clone());
    let mut first = start(&server, "", options.clone());
    let mut second = start(&server, "", options.clone());
    assert_eq!(first.snapshot().unwrap().name, "0");
    assert_eq!(second.snapshot().unwrap().name, "2");
    let name = "1".into();
    let duplicate = options.clone();
    second
        .send(Action::Create {
            name,
            options: duplicate,
        })
        .unwrap();
    wait_error_containing(&mut second, "workspace already exists");

    // A frontend that lost control can still end the workspace.
    let mut takeover = attach(&server, "0");
    wait_snapshot(&mut first, |snapshot| !snapshot.controller);
    first.send(Action::Terminate).unwrap();
    first.flush().unwrap();
    wait_detached(&mut takeover);
    let mut reused = start(&server, "", options);
    assert_eq!(reused.snapshot().unwrap().name, "0");
    // The old frontend has not polled Detached and still displays the old 0.
    first.send(Action::Terminate).unwrap();
    wait_detached(&mut first);
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
fn osc52_stores_reach_the_controller_as_a_streamed_copy() {
    let server = TestServer::start();
    let script = r"import os, time; os.write(1, b'\x1b]52;c;5Lit5paH\x07'); time.sleep(5)";
    let mut client = start(&server, "osc52", python(script));
    assert_eq!(wait_copy(&mut client), "中文");
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
        |event| matches!(event, Response::Error(error) if error.contains("no selection")),
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
    assert_eq!(client.snapshot().unwrap().tabs[0].title, "home");
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
    assert_eq!(
        client.snapshot().unwrap().pane(split).unwrap().title,
        "split-application"
    );
}

#[test]
fn closing_inactive_tabs_keeps_the_active_one_and_ends_their_panes() {
    let mut server = TestServer::start();
    let mut client = start(&server, "persist", program("/bin/sh", &["-i"]));
    let first = client.snapshot().unwrap().focused;
    split(&mut client, Axis::LeftRight);
    let (second, third) = (new_tab(&mut client, first), new_tab(&mut client, first));
    client.send(Action::Tab(-1)).unwrap();
    wait_snapshot(&mut client, |snapshot| snapshot.active_tab == second);
    let focused = client.snapshot().unwrap().focused;
    // A shell that exits in a hidden tab closes it as the user would.
    input(&mut client, third, "exit\n");
    wait_snapshot(&mut client, |snapshot| snapshot.tabs.len() == 2);
    client.send(Action::CloseTab(first)).unwrap();
    wait_snapshot(&mut client, |snapshot| snapshot.tabs.len() == 1);
    let snapshot = client.snapshot().unwrap();
    assert_eq!((snapshot.active_tab, snapshot.focused), (second, focused));
    client.send(Action::List).unwrap();
    wait_response(
        &mut client,
        |event| matches!(event, Response::List(items) if items[0].panes == 1),
    );
    client.send(Action::CloseTab(second)).unwrap();
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
        client.peer.pending(),
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
    drop(first);
    second.send(Action::List).unwrap();
    second.flush().unwrap();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
    wait_response(&mut second, |event| matches!(event, Response::List(_)));
}

#[test]
fn input_waiting_for_the_pty_arrives_whole() {
    let server = TestServer::start();
    let script = r"
import os, time, tty
tty.setraw(0)
os.write(1, b'READY')
time.sleep(0.3)
total = 0
while total < 4194304:
    total += len(os.read(0, 65536))
os.write(1, b' read-%d' % total)
time.sleep(30)
";
    let mut client = start(&server, "paste", python(script));
    wait(&mut client, |client, _| grid_contains(client, "READY"));
    let pane = client.snapshot().unwrap().focused;
    input(&mut client, pane, vec![b'x'; 4 * 1024 * 1024]);
    wait(&mut client, |client, _| {
        grid_contains(client, "read-4194304")
    });
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
    assert!(!client.peer.pending());
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
    assert!(!client.peer.pending());
}

/// A client of a real workspace, whose daemon side the test then plays itself.
fn fake_daemon(server: &TestServer) -> (Client, Peer) {
    let mut client = start(server, "fake", program("/bin/sleep", &["30"]));
    let (left, right) = UnixStream::pair().unwrap();
    client.peer = Peer::new(left).unwrap();
    (client, Peer::new(right).unwrap())
}

#[test]
fn queued_frames_all_apply_and_merge_into_one_notification() {
    let server = TestServer::start();
    let (mut client, mut daemon) = fake_daemon(&server);
    let pane = client.snapshot().unwrap().focused;
    let size = WindowSize {
        num_cols: 2,
        num_lines: 2,
        ..CreateOptions::default().size
    };
    let mut core = Core::new(size, 0xffffff, 0);
    let mut parser = crate::vt::Parser::new();
    parser.advance(&mut core, b"A");
    let full = [(pane, Update::capture(&core, true))];
    daemon.send_state(None, &full, 0).unwrap();
    core.reset_damage();
    parser.advance(&mut core, b"\x1b[2;1HB");
    let delta = [(pane, Update::capture(&core, false))];
    daemon.send_state(None, &delta, 0).unwrap();
    let marker = Response::Error("marker".into());
    daemon.send_response(marker).unwrap();
    daemon.send_state(None, &[], 0).unwrap();
    daemon.flush().unwrap();
    let events = client.poll().unwrap();
    assert!(
        matches!(events.as_slice(), [Response::ScreenDirty, Response::Error(error), Response::ScreenDirty] if error == "marker")
    );
    assert_eq!(client.screens()[&pane].row(0).cells[0].c, 'A');
    assert_eq!(client.screens()[&pane].row(1).cells[0].c, 'B');
}

#[test]
fn forced_screen_read_waits_for_its_own_reply() {
    let server = TestServer::start();
    let (mut client, mut daemon) = fake_daemon(&server);
    let snapshot = client.snapshot().unwrap().clone();
    let mut next_request = |client: &mut Client, force: bool| {
        if force {
            client.refresh_screen().unwrap();
        } else {
            client.request_screen().unwrap();
        }
        client.flush().unwrap();
        let Action::ScreenRequest { request, wait } = daemon.next_action().unwrap().unwrap() else {
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
        daemon.send_response(state).unwrap();
        daemon.flush().unwrap();
        client.poll().unwrap();
        client.screen_stale()
    };
    assert!(answer(&mut client, old));
    assert!(answer(&mut client, fresh));
    assert!(!answer(&mut client, newest));
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

    // Replies pair with their requests, and a missing block fails only its own.
    let read = |request, block| Action::ReadHistory {
        pane: pane.id,
        request,
        block: Some(block),
        cursor: None,
        query: None,
    };
    let missing = Action::CopyBlock {
        pane: pane.id,
        block: u64::MAX,
    };
    for action in [read(12, u64::MAX), read(13, block), missing] {
        client.peer.send_action(&action).unwrap();
    }
    client.flush().unwrap();
    let mut received = HashSet::new();
    wait(&mut client, |_, events| {
        for event in events {
            match event {
                // The failed copy reports through the general error.
                Response::Error(error) => {
                    assert!(error.contains("no longer retained"), "{error}");
                    received.insert(0);
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
                    let command = page.command.as_ref().unwrap();
                    assert_eq!((command.id, command.status), (block, Some(0)));
                    assert!(command.complete && command.has_output);
                    received.insert(*request);
                }
                _ => (),
            };
        }
        received.len() == 3
    });
    assert_eq!(received, HashSet::from([0, 12, 13]));
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
    let operation = FileOperation::Read { offset: 0 };
    let path = "value.txt".into();
    let read = Action::AgentFile {
        pane,
        request: 41,
        path,
        operation,
    };
    owner.send(read).unwrap();
    wait_response(&mut owner, |event| {
        matches!(event, Response::AgentFile { request: 41, result: Ok(FileResult::Read { path, text, .. }) }
            if *path == project.join("value.txt") && text == "selected needle\n")
    });

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
