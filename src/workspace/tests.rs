use super::*;

fn finish_search(client: &mut Client, search: &mut crate::search::Search) {
    while search.pending {
        let mut found = None;
        wait(client, |_, events| {
            for event in events {
                if let ClientEvent::Error(error) = event {
                    panic!("search failed: {error}");
                }
                if let ClientEvent::Search {
                    pane,
                    request,
                    matches,
                    truncated,
                } = event
                {
                    found = Some((*pane, *request, matches.clone(), *truncated));
                }
            }
            found.is_some()
        });
        let (pane, request, matches, truncated) = found.unwrap();
        search
            .result(client, pane, request, matches, truncated)
            .unwrap();
    }
}

#[test]
fn pane_search_roundtrip_reads_cold_history_and_restores_view_and_selection() {
    let server = TestServer::start();
    let mut client = Client::connect(&server.socket()).unwrap();
    client.send(Action::Create {name:"search".into(),options:CreateOptions {
            size:WindowSize {num_cols:30,num_lines:6,cell_width:0,cell_height:0},
            program:Some(Shell::new("/usr/bin/python3".into(),vec!["-c".into(),"import os,tty; tty.setraw(0); [os.write(1,(('normal-only NEEDLE' if i==50 else f'line-{i:04d}')+'\\r\\n').encode()) for i in range(3000)]; os.write(1,b'SEARCH_READY\\r\\n'); os.read(0,1); os.write(1,b'\\x1b[?1049hALT_ONLY'); os.read(0,1)".into()])),
            ..CreateOptions::default()
        }}).unwrap();
    wait(&mut client, |client, _| {
        client.snapshot().is_some_and(|s| {
            client.screens().get(&s.focused).is_some_and(|screen| {
                screen.rows.iter().any(|row| {
                    row.cells
                        .iter()
                        .map(|cell| cell.c)
                        .collect::<String>()
                        .contains("SEARCH_READY")
                })
            })
        })
    });
    let pane = client.snapshot().unwrap().focused;
    let ready = client.screens()[&pane]
        .rows
        .iter()
        .position(|row| {
            row.cells
                .iter()
                .map(|cell| cell.c)
                .collect::<String>()
                .contains("SEARCH_READY")
        })
        .unwrap() as i32;
    client
        .send(Action::SelectionStart {
            pane,
            point: Point {
                line: ready,
                column: 0,
            },
            side: Side::Left,
            kind: SelectionType::Simple,
        })
        .unwrap();
    client
        .send(Action::SelectionUpdate {
            pane,
            point: Point {
                line: ready,
                column: 5,
            },
            side: Side::Right,
        })
        .unwrap();
    wait(&mut client, |client, _| {
        client.screens()[&pane].selection_range().is_some()
    });
    let original = client.screens()[&pane].selection_range();
    let mut search = crate::search::Search::default();
    search.begin(&mut client, pane).unwrap();
    search.edit(&mut client, "NEEDLE").unwrap();
    finish_search(&mut client, &mut search);
    wait(&mut client, |client, _| {
        client.screens()[&pane].rows.iter().any(|row| {
            row.cells
                .iter()
                .map(|cell| cell.c)
                .collect::<String>()
                .contains("NEEDLE")
        })
    });
    assert_eq!(search.matches.len(), 1);
    assert!(client.screens()[&pane].display_offset() > 2500);
    for symbol in [0xff50, 0xff08, 0xff57, 0xffff] {
        search.key(&mut client, symbol, false, false).unwrap();
        assert!(!search.pending);
    }
    for bytes in [b"\x1b[H".as_slice(), b"\x7f", b"\x1b[F", b"\x1b[3~"] {
        search.bytes(&mut client, bytes, false).unwrap();
        assert!(!search.pending);
    }
    search.edit(&mut client, "\n\t").unwrap();
    assert!(!search.pending);
    assert_eq!(search.matches.len(), 1);
    search
        .key(&mut client, u32::from('u'), false, true)
        .unwrap();
    search.edit(&mut client, "line-").unwrap();
    search
        .key(&mut client, u32::from('u'), false, true)
        .unwrap();
    search.edit(&mut client, "NEEDLE").unwrap();
    finish_search(&mut client, &mut search);
    assert_eq!(search.matches.len(), 1);
    search.close(&mut client).unwrap();
    wait(&mut client, |client, _| {
        client.screens()[&pane].display_offset() == 0
            && client.screens()[&pane].selection_range() == original
    });
    client
        .send(Action::Input {
            pane,
            data: vec![b'a'],
        })
        .unwrap();
    wait(&mut client, |client, _| {
        client.screens()[&pane].mode().contains(Mode::ALT_SCREEN)
    });
    search.begin(&mut client, pane).unwrap();
    search.edit(&mut client, "normal-only").unwrap();
    finish_search(&mut client, &mut search);
    assert!(search.matches.is_empty());
    search.close(&mut client).unwrap();
    search
        .bytes(&mut client, "late paste".as_bytes(), true)
        .unwrap();
    assert!(!search.open);
    search.begin(&mut client, pane).unwrap();
    search.edit(&mut client, "中abc").unwrap();
    search.key(&mut client, 0xff50, false, false).unwrap();
    search.key(&mut client, 0xffff, false, false).unwrap();
    assert_eq!(search.query, "abc");
    search.key(&mut client, 0xff53, false, false).unwrap();
    search.edit(&mut client, "界").unwrap();
    assert_eq!(search.query, "a界bc");
    search.key(&mut client, 0xff08, false, false).unwrap();
    assert_eq!(search.query, "abc");
    assert_eq!(search.cursor, 1);
    search.bytes(&mut client, b"\x1b[99~", false).unwrap();
    assert!(search.open);
    assert_eq!(search.query, "abc");
    let pasted = "中a".repeat(1024);
    search.bytes(&mut client, pasted.as_bytes(), true).unwrap();
    assert_eq!(search.query, format!("a{pasted}bc"));
    assert_eq!(search.cursor, 1 + pasted.len());
    search.bytes(&mut client, &[0xe7, 0x95], true).unwrap();
    search.bytes(&mut client, &[0x8c], true).unwrap();
    search.bytes(&mut client, b"x\n\t\x7fy", true).unwrap();
    assert_eq!(search.query, format!("a{pasted}界xybc"));
    assert_eq!(search.cursor, 1 + pasted.len() + "界xy".len());
    search.close(&mut client).unwrap();
}

#[test]
fn independent_clients_search_without_cancelling_each_other_or_selecting_read_only() {
    let server = TestServer::start();
    let mut viewer = Client::connect(&server.socket()).unwrap();
    let options = CreateOptions {
            program: Some(Shell::new("/usr/bin/python3".into(), vec!["-c".into(),
                "import os,tty; tty.setraw(0); [os.write(1,b'INDEPENDENT\\r\\n') for _ in range(3000)]; os.write(1,b'READY\\r\\n'); os.read(0,1)".into()])),
            ..CreateOptions::default()
        };
    viewer
        .send(Action::Create {
            name: "independent-search".into(),
            options: options.clone(),
        })
        .unwrap();
    wait(&mut viewer, |client, _| grid_contains(client, "READY"));
    let pane = viewer.snapshot().unwrap().focused;
    let mut viewer_search = crate::search::Search::default();
    viewer_search.begin(&mut viewer, pane).unwrap();
    let mut controller = Client::connect(&server.socket()).unwrap();
    controller
        .send(Action::Attach {
            name: "independent-search".into(),
            size: options.size,
        })
        .unwrap();
    wait(&mut controller, |client, _| {
        client.snapshot().is_some_and(|s| s.controller)
    });
    wait(&mut viewer, |client, _| {
        !client.snapshot().unwrap().controller
    });
    assert!(viewer_search.observe(&mut viewer).unwrap());
    assert!(!viewer_search.open);
    viewer_search.begin(&mut viewer, pane).unwrap();
    let mut controller_search = crate::search::Search::default();
    controller_search.begin(&mut controller, pane).unwrap();
    viewer_search.edit(&mut viewer, "INDEPENDENT").unwrap();
    controller_search
        .edit(&mut controller, "INDEPENDENT")
        .unwrap();
    finish_search(&mut viewer, &mut viewer_search);
    finish_search(&mut controller, &mut controller_search);
    assert_eq!(viewer_search.matches.len(), 3000);
    assert_eq!(controller_search.matches, viewer_search.matches);
    assert!(viewer_search.label("").contains("read-only"));
    let index = viewer_search.index;
    viewer_search.next(&mut viewer, false).unwrap();
    assert_eq!(viewer_search.index, index);
    viewer_search
        .key(&mut viewer, u32::from('u'), false, true)
        .unwrap();
    let query = "界".repeat(2000);
    viewer_search
        .bytes(&mut viewer, query.as_bytes(), true)
        .unwrap();
    finish_search(&mut viewer, &mut viewer_search);
    assert_eq!(viewer_search.query.len(), 6000);
    assert!(viewer_search.matches.is_empty());
    viewer_search
        .key(&mut viewer, u32::from('u'), false, true)
        .unwrap();
    viewer_search.edit(&mut viewer, "INDEPENDENT").unwrap();
    finish_search(&mut viewer, &mut viewer_search);
    assert_eq!(viewer_search.matches.len(), 3000);
    controller_search.close(&mut controller).unwrap();
    controller
        .send(Action::Split {
            pane,
            axis: Axis::LeftRight,
        })
        .unwrap();
    wait(&mut controller, |client, _| {
        client.snapshot().unwrap().panes.len() == 2
    });
    controller.send(Action::ClosePane(pane)).unwrap();
    wait(&mut viewer, |client, _| {
        !client
            .snapshot()
            .unwrap()
            .panes
            .iter()
            .any(|p| p.id == pane)
    });
    assert!(viewer_search.observe(&mut viewer).unwrap());
    assert!(!viewer_search.open);
}

#[test]
fn cursor_metadata_repaints_frontend_rows_without_transferring_cell_data() {
    let mut core = crate::core::Core::new(
        WindowSize {
            num_cols: 8,
            num_lines: 3,
            cell_width: 0,
            cell_height: 0,
        },
        0xffffff,
        0,
    );
    let mut parser = crate::vt::Parser::new();
    parser.advance(&mut core, b"abcd");
    let mut screen = Screen::default();
    screen.apply(Update::capture(&core, true)).unwrap();
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
        screen.apply(update).unwrap();
        assert_eq!(screen.cursor(), core.cursor());
        assert_eq!(screen.mode(), core.mode());
        assert_eq!(screen.dirty_rows().collect::<Vec<_>>(), dirty);
        assert_eq!(screen.row(0).cells[0].c, 'a');
    }
}

#[test]
fn cancelling_pending_wrap_updates_hover_output_without_moving_cursor() {
    let server = TestServer::start();
    let mut client = Client::connect(&server.socket()).unwrap();
    client
            .send(Action::Create {
                name: "hover".into(),
                options: CreateOptions {
                    size: WindowSize {
                        num_cols: 7,
                        num_lines: 4,
                        ..CreateOptions::default().size
                    },
                    program: Some(Shell::new(
                        "/usr/bin/python3".into(),
                        vec![
                            "-c".into(),
                            "import os,tty,time; tty.setraw(0); os.write(1,b'\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07ABCDEFG'); os.read(0,1); os.write(1,b'\x1b[1;7H'); time.sleep(60)".into(),
                        ],
                    )),
                    ..CreateOptions::default()
                },
            })
            .unwrap();
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
    client
        .send(Action::Input {
            pane,
            data: vec![b'x'],
        })
        .unwrap();
    wait(&mut client, |client, _| {
        state(client) == Some((cursor, false, 'G'))
    });
    assert_eq!(
        client.screens()[&pane].dirty_rows().collect::<Vec<_>>(),
        [0]
    );
}

#[test]
fn tab_positions_preserve_padding_and_active_visibility() {
    let mut snapshot = Snapshot {
        name: "dev".into(),
        controller: true,
        focused: 1,
        active_tab: 1,
        tabs: ["shell", "logs", "editor"]
            .into_iter()
            .enumerate()
            .map(|(index, title)| TabView {
                id: index as u64 + 1,
                title: title.into(),
            })
            .collect(),
        panes: Vec::new(),
    };
    assert_eq!(
        tab_layout(&snapshot, 104, "CPU 10%  MEM 20%  DSK 30%"),
        [(0, 3, 13), (1, 17, 12), (2, 30, 14)]
    );
    let mut overflow = snapshot.clone();
    overflow.tabs = (0..12)
        .map(|i| TabView {
            id: i + 1,
            title: format!("tab-{i}"),
        })
        .collect();
    for active in [1, 6, 12] {
        overflow.active_tab = active;
        let (layout, label, before, after) = tab_chrome(&overflow, 80, "");
        assert!(
            layout
                .iter()
                .any(|&(index, _, _)| index as u64 + 1 == active)
        );
        assert_eq!(before + layout.len() + after, overflow.tabs.len());
        assert!(!label.is_empty());
        let (_, x, width) = layout.last().unwrap();
        assert!(
            x + width + crate::width::text_width(&label) as u16 + 2 < 80,
            "overflow label retains space"
        );
    }
    snapshot.tabs[0].title = "directory-with-a-long-name".into();
    snapshot.tabs[1].id = 3;
    snapshot.tabs[2].id = 2;
    snapshot.active_tab = 2;
    let narrow = tab_layout(&snapshot, 40, "CPU 100%  MEM 100%  DSK 100%");
    assert!(narrow.iter().any(|&(index, _, _)| index == 2));
    snapshot.tabs.truncate(1);
    snapshot.active_tab = 1;
    snapshot.controller = false;
    let readonly = tab_layout(&snapshot, 40, "Read-only  CPU 100%  MEM 100%  DSK 100%");
    let (_, x, width) = readonly[0];
    assert!(
        40 - (x + width + 1) - 4 >= 9,
        "read-only indicator remains visible"
    );
    snapshot.controller = true;
    snapshot.tabs[0].title = "\n".into();
    assert_eq!(
        tab_layout(&snapshot, 104, "CPU 10%  MEM 20%  DSK 30%"),
        [(0, 3, 9)]
    );
}
#[test]
fn split_layout_and_removal_preserve_remaining_leaf() {
    let mut root = Node::Pane(1);
    assert!(root.split(1, 2, Axis::LeftRight));
    assert!(root.split(2, 3, Axis::TopBottom));
    let mut layout = Vec::new();
    root.layout(
        Rect {
            x: 0,
            y: 1,
            cols: 81,
            rows: 24,
        },
        &mut layout,
    );
    assert_eq!(
        layout,
        vec![
            (
                1,
                Rect {
                    x: 0,
                    y: 1,
                    cols: 40,
                    rows: 24
                }
            ),
            (
                2,
                Rect {
                    x: 41,
                    y: 1,
                    cols: 40,
                    rows: 11
                }
            ),
            (
                3,
                Rect {
                    x: 41,
                    y: 13,
                    cols: 40,
                    rows: 12
                }
            )
        ]
    );
    let root = root.remove(2).unwrap();
    let mut layout = Vec::new();
    root.layout(
        Rect {
            x: 0,
            y: 1,
            cols: 81,
            rows: 24,
        },
        &mut layout,
    );
    assert_eq!(layout[1].0, 3);
    assert_eq!(layout[1].1.rows, 24);
}
#[test]
fn socket_boundary_rejects_invalid_sizes_and_names() {
    assert!(validate_name("main").is_ok());
    assert!(validate_name("../../x").is_err());
    assert!(validate_size(CreateOptions::default().size).is_ok());
    assert!(
        validate_size(WindowSize {
            num_cols: u16::MAX,
            ..CreateOptions::default().size
        })
        .is_err()
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
    command.arg("-c").arg(format!(
        r"printf '\003\000\000\000\{:03o}\012\377'",
        crate::wire::VERSION
    ));
    let error = Client::connect_remote(command, "test-host".into())
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("version 255"));
}
#[test]
fn dropping_remote_frontend_reaps_its_transport() {
    let mut command = Command::new("sh");
    command.arg("-c").arg(format!(
        r"printf '\003\000\000\000\{:03o}\012\{:03o}'; exec sleep 60",
        crate::wire::VERSION,
        crate::bridge::BRIDGE_VERSION
    ));
    let client = Client::connect_remote(command, "test-host".into()).unwrap();
    assert!(client.is_remote());
    let pid = client.ssh.as_ref().unwrap().id();
    drop(client);
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}
#[test]
fn server_child() {
    let Some(directory) = std::env::var_os("NEXTTY_TEST_SERVER_DIR").map(PathBuf::from) else {
        return;
    };
    run_server(
        &directory.join("server.sock"),
        ServerOptions {
            history_directory: directory.join("history"),
        },
    )
    .unwrap();
}

struct TestServer {
    child: std::process::Child,
    directory: PathBuf,
}
impl TestServer {
    fn start() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::current_dir().unwrap().join(format!(
            "target/workspace-server/test-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let home = directory.join("home");
        fs::create_dir(&home).unwrap();
        let log = File::create(directory.join("server.log")).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "workspace::tests::server_child", "--nocapture"])
            .env("NEXTTY_TEST_SERVER_DIR", &directory)
            .env("HOME", &home)
            .env("ZDOTDIR", &home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        let result = Self { child, directory };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !result.socket().exists() || !result.directory.join("history").is_dir() {
            assert!(Instant::now() < deadline, "server did not finish starting");
            std::thread::sleep(Duration::from_millis(5));
        }
        result
    }
    fn socket(&self) -> PathBuf {
        self.directory.join("server.sock")
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
        if let Ok(mut client) = Client::connect(&self.socket()) {
            let _ = client.send(Action::TerminateName("persist".into()));
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Ok(events) = client.poll()
                    && events
                        .iter()
                        .any(|event| matches!(event, ClientEvent::Detached | ClientEvent::Error(_)))
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}
fn wait(client: &mut Client, mut condition: impl FnMut(&Client, &[ClientEvent]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let events = client.poll().unwrap();
        if condition(client, &events) {
            return;
        }
        assert!(Instant::now() < deadline, "workspace condition timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn grid_contains(client: &Client, needle: &str) -> bool {
    client.screens().values().any(|screen| {
        (0..screen.screen_lines()).any(|row| {
            screen
                .row(row)
                .cells
                .iter()
                .map(|cell| cell.c)
                .collect::<String>()
                .contains(needle)
        })
    })
}
#[test]
fn agent_owner_restores_state_and_keeps_background_pty_without_control() {
    let server = TestServer::start();
    let mut frontend = Client::connect(&server.socket()).unwrap();
    let options = CreateOptions {
        program: Some(Shell::new("/bin/sh".into(), vec!["-i".into()])),
        ..CreateOptions::default()
    };
    frontend
        .send(Action::Create {
            name: "persist".into(),
            options: options.clone(),
        })
        .unwrap();
    wait(&mut frontend, |client, _| client.snapshot().is_some());
    let pane = frontend.snapshot().unwrap().focused;
    frontend
        .send(Action::AgentView(b"draft-view".to_vec()))
        .unwrap();
    frontend
        .send(Action::AgentRequest(b"first-command".to_vec()))
        .unwrap();
    wait(&mut frontend, |client, _| !client.agent_events.is_empty());
    let token = frontend
        .take_agent_events()
        .into_iter()
        .find(|(kind, _)| *kind == 3)
        .unwrap()
        .1;
    let mut owner = Client::connect(&server.socket()).unwrap();
    owner
        .send(Action::AgentAttach {
            name: "persist".into(),
            token: token.clone(),
        })
        .unwrap();
    wait(&mut owner, |client, _| client.snapshot().is_some());
    assert_eq!(
        owner.take_agent_events(),
        vec![
            (4, vec![]),
            (0, b"draft-view".to_vec()),
            (2, b"first-command".to_vec())
        ]
    );
    assert!(!owner.snapshot().unwrap().controller);
    assert!(owner.can_agent_input());
    assert!(frontend.snapshot().unwrap().controller);
    for action in [
        Action::NewTab { pane },
        Action::Resize(WindowSize {
            num_cols: 100,
            ..options.size
        }),
        Action::Terminate,
    ] {
        owner.send(action).unwrap();
        wait(&mut owner, |_, events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::Error(_)))
        });
    }
    frontend
        .send(Action::AgentView(b"edited-view".to_vec()))
        .unwrap();
    frontend
        .send(Action::AgentRequest(b"next-command".to_vec()))
        .unwrap();
    wait(&mut owner, |client, _| client.agent_events.len() == 2);
    assert_eq!(
        owner.take_agent_events(),
        vec![(0, b"edited-view".to_vec()), (2, b"next-command".to_vec())]
    );
    let mut duplicate = Client::connect(&server.socket()).unwrap();
    duplicate
        .send(Action::AgentAttach {
            name: "persist".into(),
            token: token.clone(),
        })
        .unwrap();
    wait(&mut duplicate, |_, events| {
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Error(_)))
    });
    owner
        .send(Action::AgentUpdate {
            kind: 0,
            payload: b"live-view".to_vec(),
        })
        .unwrap();
    owner
        .send(Action::AgentUpdate {
            kind: 1,
            payload: b"live-models".to_vec(),
        })
        .unwrap();
    wait(&mut frontend, |client, _| client.agent_events.len() == 2);
    frontend.take_agent_events();
    frontend.send(Action::NewTab { pane }).unwrap();
    wait(&mut frontend, |client, _| {
        client.snapshot().unwrap().tabs.len() == 2
    });
    assert_eq!(frontend.snapshot().unwrap().panes.len(), 1);
    wait(&mut owner, |client, _| {
        client.snapshot().unwrap().panes.len() == 2
    });
    owner
        .send(Action::Input {
            pane,
            data: b"printf '\\142ackground-agent-ok\\n'\n".to_vec(),
        })
        .unwrap();
    wait(&mut owner, |client, _| {
        grid_contains(client, "background-agent-ok")
    });
    assert!(!grid_contains(&frontend, "background-agent-ok"));
    let mut attached = Client::connect(&server.socket()).unwrap();
    attached
        .send(Action::Attach {
            name: "persist".into(),
            size: options.size,
        })
        .unwrap();
    wait(&mut attached, |client, _| client.snapshot().is_some());
    wait(&mut frontend, |client, _| {
        !client.snapshot().unwrap().controller
    });
    for action in [
        Action::AgentView(b"bad".to_vec()),
        Action::AgentRequest(b"bad".to_vec()),
        Action::Input {
            pane,
            data: b"bad".to_vec(),
        },
    ] {
        frontend.send(action).unwrap();
        wait(&mut frontend, |_, events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::Error(_)))
        });
    }
    frontend.send(Action::AgentOpen).unwrap();
    wait(&mut frontend, |client, _| {
        client.agent_events.iter().any(|(kind, _)| *kind == 5)
    });
    assert_eq!(
        frontend.take_agent_events(),
        vec![
            (0, b"live-view".to_vec()),
            (1, b"live-models".to_vec()),
            (5, vec![])
        ]
    );
    drop(attached);
    wait(&mut frontend, |client, _| {
        client.snapshot().unwrap().controller
    });
    drop(frontend);
    owner
        .send(Action::Input {
            pane,
            data: b"printf '\\144etached-agent-ok\\n'\n".to_vec(),
        })
        .unwrap();
    wait(&mut owner, |client, _| {
        grid_contains(client, "detached-agent-ok")
    });
    assert!(!owner.snapshot().unwrap().controller);
    let mut keeper = Client::connect(&server.socket()).unwrap();
    keeper
        .send(Action::Create {
            name: "keeper".into(),
            options: options.clone(),
        })
        .unwrap();
    wait(&mut keeper, |client, _| client.snapshot().is_some());
    keeper
        .send(Action::TerminateName("persist".into()))
        .unwrap();
    wait(&mut owner, |_, events| {
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Detached))
    });
    let mut fresh = Client::connect(&server.socket()).unwrap();
    fresh
        .send(Action::Create {
            name: "persist".into(),
            options,
        })
        .unwrap();
    wait(&mut fresh, |client, _| client.snapshot().is_some());
    fresh.send(Action::AgentOpen).unwrap();
    wait(&mut fresh, |client, _| !client.agent_events.is_empty());
    assert_eq!(fresh.take_agent_events(), vec![(5, vec![])]);
    let mut impostor = Client::connect(&server.socket()).unwrap();
    impostor
        .send(Action::AgentAttach {
            name: "persist".into(),
            token,
        })
        .unwrap();
    wait(&mut impostor, |_, events| {
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Error(_)))
    });
    keeper.send(Action::TerminateName("keeper".into())).unwrap();
    wait(&mut keeper, |_, events| {
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Detached))
    });
}
#[test]
fn main_and_alternate_screens_keep_full_pty_width() {
    let server = TestServer::start();
    let mut client = Client::connect(&server.socket()).unwrap();
    let mut size = WindowSize {
        num_cols: 80,
        num_lines: 24,
        ..CreateOptions::default().size
    };
    let program = r#"
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
    client
        .send(Action::Create {
            name: "persist".into(),
            options: CreateOptions {
                size,
                program: Some(Shell::new(
                    "/usr/bin/python3".into(),
                    vec!["-c".into(), program.into()],
                )),
                ..CreateOptions::default()
            },
        })
        .unwrap();
    let check = |client: &mut Client, columns: usize, rows: usize, alternate: bool| {
        wait(client, |client, _| {
            let Some(snapshot) = client.snapshot() else {
                return false;
            };
            let Some(screen) = client.screens().get(&snapshot.focused) else {
                return false;
            };
            screen.columns() == columns
                && screen.screen_lines() == rows
                && screen.mode().contains(Mode::ALT_SCREEN) == alternate
                && screen.row(0).cells[columns - 1].c == '#'
                && grid_contains(client, &format!("{columns}x{rows}"))
        });
    };
    check(&mut client, 80, 23, false);
    let pane = client.snapshot().unwrap().focused;
    for _ in 0..2 {
        client
            .send(Action::Input {
                pane,
                data: vec![b'a'],
            })
            .unwrap();
        check(
            &mut client,
            size.num_cols as usize,
            size.num_lines as usize - 1,
            true,
        );
        size.num_cols += 10;
        size.num_lines += 2;
        client.send(Action::Resize(size)).unwrap();
        check(
            &mut client,
            size.num_cols as usize,
            size.num_lines as usize - 1,
            true,
        );
        client
            .send(Action::Input {
                pane,
                data: vec![b'm'],
            })
            .unwrap();
        check(
            &mut client,
            size.num_cols as usize,
            size.num_lines as usize - 1,
            false,
        );
    }
}

fn assert_same_screen(left: &Client, right: &Client, pane: PaneId) {
    let left = &left.screens()[&pane];
    let right = &right.screens()[&pane];
    assert_eq!(left.columns(), right.columns());
    assert_eq!(left.screen_lines(), right.screen_lines());
    assert_eq!(left.palette, right.palette);
    assert_eq!(left.cursor(), right.cursor());
    assert_eq!(left.mode(), right.mode());
    for row in 0..left.screen_lines() {
        assert_eq!(left.row(row).cells, right.row(row).cells, "row {row}");
        assert_eq!(left.row(row).extras(), right.row(row).extras());
        assert_eq!(left.row(row).used(), right.row(row).used());
        assert_eq!(left.row(row).wrapped, right.row(row).wrapped);
    }
}

#[test]
fn input_clears_selection_only_after_it_is_accepted() {
    let server = TestServer::start();
    let (sender, _receiver) = UnixStream::pair().unwrap();
    let mut clients = vec![Connection::new(1, sender).unwrap()];
    let mut workspaces = HashMap::new();
    let mut dirty = HashSet::new();
    apply_action(
        0,
        Action::Create {
            name: "input".into(),
            options: CreateOptions {
                directory: Some(server.directory.join("home")),
                program: Some(Shell::new("/bin/sleep".into(), vec!["30".into()])),
                ..CreateOptions::default()
            },
        },
        &mut clients,
        &mut workspaces,
        &mut dirty,
        &server.directory.join("history"),
        &mut ClosingTerminals::default(),
    )
    .unwrap();
    let pane = workspaces["input"].focused();
    let core = workspaces["input"].panes[&pane].terminal.core.clone();
    {
        let mut core = core.lock().unwrap();
        core.selection_start(
            Point { line: 0, column: 0 },
            Side::Left,
            SelectionType::Simple,
        );
        core.selection_update(Point { line: 0, column: 1 }, Side::Right);
    }
    apply_action(
        0,
        Action::Input {
            pane,
            data: b"x".to_vec(),
        },
        &mut clients,
        &mut workspaces,
        &mut dirty,
        &server.directory.join("history"),
        &mut ClosingTerminals::default(),
    )
    .unwrap();
    assert!(core.lock().unwrap().selection_range().is_none());
    assert!(dirty.contains("input"));
    {
        let mut core = core.lock().unwrap();
        core.selection_start(
            Point { line: 0, column: 0 },
            Side::Left,
            SelectionType::Simple,
        );
        core.selection_update(Point { line: 0, column: 1 }, Side::Right);
    }
    apply_action(
        0,
        Action::Input {
            pane,
            data: b"\x1b[I".to_vec(),
        },
        &mut clients,
        &mut workspaces,
        &mut dirty,
        &server.directory.join("history"),
        &mut ClosingTerminals::default(),
    )
    .unwrap();
    assert!(core.lock().unwrap().selection_range().is_some());
    let rejected = apply_action(
        0,
        Action::Input {
            pane,
            data: vec![b'x'; 256 * 1024 + 1],
        },
        &mut clients,
        &mut workspaces,
        &mut dirty,
        &server.directory.join("history"),
        &mut ClosingTerminals::default(),
    );
    assert_eq!(rejected.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    assert!(core.lock().unwrap().selection_range().is_some());
    drop(workspaces);
}

#[test]
fn automatic_session_names_are_atomic_reusable_and_attachable() {
    let server = TestServer::start();
    let options = CreateOptions {
        directory: Some(server.directory.join("home")),
        program: Some(Shell::new(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "printf 'numbered-session-marker\\n'; exec /bin/sh".into(),
            ],
        )),
        ..CreateOptions::default()
    };
    let mut named = Client::connect(&server.socket()).unwrap();
    named
        .send(Action::Create {
            name: "2".into(),
            options: options.clone(),
        })
        .unwrap();
    wait(&mut named, |client, _| client.snapshot().is_some());

    let barrier = Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let socket = server.socket();
            let options = options.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut client = Client::connect(&socket).unwrap();
                barrier.wait();
                client
                    .send(Action::Create {
                        name: String::new(),
                        options,
                    })
                    .unwrap();
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
    let mut names: Vec<_> = clients
        .iter()
        .map(|client| client.snapshot().unwrap().name.parse::<usize>().unwrap())
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
    wait(&mut control, |_, events| {
        events.iter().any(|event| {
                matches!(event, ClientEvent::Error(error) if error.contains("workspace already exists"))
            })
    });
    control.send(Action::TerminateName("1".into())).unwrap();
    wait(&mut control, |_, events| {
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Detached))
    });
    control
        .send(Action::Create {
            name: String::new(),
            options: options.clone(),
        })
        .unwrap();
    wait(&mut control, |client, _| client.snapshot().is_some());
    assert_eq!(control.snapshot().unwrap().name, "1");

    drop(clients);
    let mut attached = Client::connect(&server.socket()).unwrap();
    attached
        .send(Action::Attach {
            name: "0".into(),
            size: options.size,
        })
        .unwrap();
    wait(&mut attached, |client, _| {
        grid_contains(client, "numbered-session-marker")
    });
    assert_eq!(attached.snapshot().unwrap().name, "0");
    assert!(attached.snapshot().unwrap().controller);

    let mut custom = Client::connect(&server.socket()).unwrap();
    custom
        .send(Action::Create {
            name: "dev".into(),
            options: options.clone(),
        })
        .unwrap();
    wait(&mut custom, |client, _| client.snapshot().is_some());
    assert_eq!(custom.snapshot().unwrap().name, "dev");

    let mut takeover = Client::connect(&server.socket()).unwrap();
    takeover
        .send(Action::Attach {
            name: "0".into(),
            size: options.size,
        })
        .unwrap();
    wait(&mut takeover, |client, _| client.snapshot().is_some());
    wait(&mut attached, |client, _| {
        !client.snapshot().unwrap().controller
    });
    attached.send(Action::Terminate).unwrap();
    wait(&mut takeover, |_, events| {
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Detached))
    });

    let mut reused = Client::connect(&server.socket()).unwrap();
    reused
        .send(Action::Create {
            name: String::new(),
            options,
        })
        .unwrap();
    wait(&mut reused, |client, _| client.snapshot().is_some());
    assert_eq!(reused.snapshot().unwrap().name, "0");
    // The old frontend has not polled Detached and still displays the old 0.
    attached.send(Action::Terminate).unwrap();
    wait(&mut attached, |_, events| {
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Detached))
    });
    reused
        .send(Action::Input {
            pane: reused.snapshot().unwrap().focused,
            data: b"printf '\\162eused-session-alive\\n'\n".to_vec(),
        })
        .unwrap();
    wait(&mut reused, |client, _| {
        grid_contains(client, "reused-session-alive")
    });

    let mut cleanup = Client::connect(&server.socket()).unwrap();
    for name in (0..=8)
        .map(|number| number.to_string())
        .chain(["dev".into()])
    {
        cleanup.send(Action::TerminateName(name)).unwrap();
        wait(&mut cleanup, |_, events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::Detached))
        });
    }
}

#[test]
fn attached_frontends_agree_after_sparse_text_and_palette_changes() {
    use crate::vt::{Color, Rgb};
    let server = TestServer::start();
    let mut first = Client::connect(&server.socket()).unwrap();
    let options = CreateOptions {
            directory: Some(server.directory.join("home")),
            program: Some(Shell::new(
                "/bin/sh".into(),
                vec!["-c".into(), "stty raw -echo; printf 'ready'; while IFS= read -r line; do printf '%b' \"$line\"; done".into()],
            )),
            ..CreateOptions::default()
        };
    first
        .send(Action::Create {
            name: "persist".into(),
            options: options.clone(),
        })
        .unwrap();
    wait(&mut first, |client, _| grid_contains(client, "ready"));
    let pane = first.snapshot().unwrap().focused;
    first
        .send(Action::Input {
            pane,
            data: b"\\033[H\\033[2Jpalette-base\\033]4;1;#112233\\007\n".to_vec(),
        })
        .unwrap();
    wait(&mut first, |client, _| {
        grid_contains(client, "palette-base")
            && client.screens()[&pane].color(Color::Indexed(1))
                == Rgb {
                    r: 0x11,
                    g: 0x22,
                    b: 0x33,
                }
    });
    first
        .send(Action::Input {
            pane,
            data: b"\\033[3;1Hsparse-before-attach\n".to_vec(),
        })
        .unwrap();
    wait(&mut first, |client, _| {
        grid_contains(client, "sparse-before-attach")
    });
    let mut second = Client::connect(&server.socket()).unwrap();
    second
        .send(Action::Attach {
            name: "persist".into(),
            size: options.size,
        })
        .unwrap();
    wait(&mut second, |client, _| {
        grid_contains(client, "sparse-before-attach")
    });
    wait(&mut first, |client, _| {
        !client.snapshot().unwrap().controller
    });
    assert_same_screen(&first, &second, pane);
    second
        .send(Action::Input {
            pane,
            data: b"\\033[4;1Hafter-attach\\033]4;1;#445566\\007\n".to_vec(),
        })
        .unwrap();
    wait(&mut second, |client, _| {
        grid_contains(client, "after-attach")
            && client.screens()[&pane].color(Color::Indexed(1))
                == Rgb {
                    r: 0x44,
                    g: 0x55,
                    b: 0x66,
                }
    });
    wait(&mut first, |client, _| {
        grid_contains(client, "after-attach")
            && client.screens()[&pane].color(Color::Indexed(1))
                == Rgb {
                    r: 0x44,
                    g: 0x55,
                    b: 0x66,
                }
    });
    assert_same_screen(&first, &second, pane);
}

#[test]
fn backpressured_broadcast_recovers_latest_grid_and_palette_with_a_full_update() {
    use crate::vt::{Color, Parser, Rgb};
    let server = TestServer::start();
    let mut clients = Vec::new();
    let mut frontends = Vec::new();
    for id in 1..=2 {
        let (sender, receiver) = UnixStream::pair().unwrap();
        if id == 2 {
            let limit: libc::c_int = 4096;
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        sender.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_SNDBUF,
                        (&limit as *const libc::c_int).cast(),
                        std::mem::size_of_val(&limit) as libc::socklen_t,
                    )
                },
                0
            );
        }
        clients.push(Connection::new(id, sender).unwrap());
        frontends.push(Client {
            peer: Peer::new(receiver).unwrap(),
            #[cfg(feature = "agent")]
            endpoint: ClientEndpoint::Local(PathBuf::new()),
            agent_events: Vec::new(),
            agent_delegate: false,
            snapshot: None,
            screens: HashMap::new(),
            write_error: None,
            ssh: None,
            remote_target: None,
        });
    }
    let mut workspaces = HashMap::new();
    apply_action(
        0,
        Action::Create {
            name: "flow".into(),
            options: CreateOptions {
                size: WindowSize {
                    num_cols: 200,
                    num_lines: 61,
                    cell_width: 0,
                    cell_height: 0,
                },
                directory: Some(server.directory.join("home")),
                program: Some(Shell::new("/bin/sleep".into(), vec!["30".into()])),
                ..CreateOptions::default()
            },
        },
        &mut clients,
        &mut workspaces,
        &mut HashSet::new(),
        &server.directory.join("history"),
        &mut ClosingTerminals::default(),
    )
    .unwrap();
    let workspace = workspaces.get_mut("flow").unwrap();
    let pane = workspace.focused();
    let mut parser = Parser::new();
    {
        let mut core = workspace.panes[&pane].terminal.core.lock().unwrap();
        for row in 0..core.screen_lines() {
            let text = format!("\x1b[{};1H{}", row + 1, "x".repeat(core.columns()));
            parser.advance(&mut core, text.as_bytes());
        }
        parser.advance(&mut core, b"\x1b]4;1;#112233\x07");
    }
    broadcast(workspace, &mut clients).unwrap();
    clients[0].peer.flush().unwrap();
    wait(&mut frontends[0], |client, _| {
        client.screens().contains_key(&pane)
    });
    assert!(!clients[0].full && clients[0].known.contains_key(&pane));
    // A continues with sparse rows and no palette; B needs its first full grid.
    clients[1].workspace = Some("flow".into());
    {
        let mut core = workspace.panes[&pane].terminal.core.lock().unwrap();
        parser.advance(&mut core, b"\x1b[1;1Hmixed-update");
        assert!(core.dirty_rows().count() < core.screen_lines());
    }
    broadcast(workspace, &mut clients).unwrap();
    for client in &mut clients {
        client.peer.flush().unwrap();
    }
    assert!(
        clients[1].peer.pending(),
        "the small sender buffer must leave B's full frame pending"
    );
    wait(&mut frontends[0], |client, _| {
        grid_contains(client, "mixed-update")
    });
    {
        let mut core = workspace.panes[&pane].terminal.core.lock().unwrap();
        parser.advance(&mut core, b"\x1b[1;1Hlatest-grid!\x1b]4;1;#445566\x07");
    }
    broadcast(workspace, &mut clients).unwrap();
    assert!(
        clients[1].full && clients[1].peer.pending(),
        "a skipped pending frontend must request the latest full capture"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        for client in &mut clients {
            client.peer.flush().unwrap();
        }
        for frontend in &mut frontends {
            frontend.poll().unwrap();
        }
        if clients[1].full && !clients[1].peer.pending() {
            broadcast(workspace, &mut clients).unwrap();
        }
        if !clients[1].full
            && !clients[1].peer.pending()
            && frontends.iter().all(|client| {
                grid_contains(client, "latest-grid!")
                    && client.screens()[&pane].color(Color::Indexed(1))
                        == Rgb {
                            r: 0x44,
                            g: 0x55,
                            b: 0x66,
                        }
            })
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "backpressured frontend did not catch up"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_same_screen(&frontends[0], &frontends[1], pane);
    clients[1].full = true;
    {
        let mut core = workspace.panes[&pane].terminal.core.lock().unwrap();
        parser.advance(&mut core, b"\x1b]4;1;#778899\x07");
    }
    broadcast(workspace, &mut clients).unwrap();
    for (connection, frontend) in clients.iter_mut().zip(&mut frontends) {
        wait(frontend, |client, _| {
            connection.peer.flush().unwrap();
            client.screens()[&pane].color(Color::Indexed(1))
                == Rgb {
                    r: 0x77,
                    g: 0x88,
                    b: 0x99,
                }
        });
    }
    assert_same_screen(&frontends[0], &frontends[1], pane);
}

#[test]
fn daemon_preserves_pty_and_transfers_control_across_frontends() {
    let mut server = TestServer::start();
    let mut first = Client::connect(&server.socket()).unwrap();
    let options = CreateOptions {
        directory: Some(server.directory.join("home")),
        program: Some(Shell::new(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "printf 'persisted-marker\\n'; exec /bin/sh".into(),
            ],
        )),
        ..CreateOptions::default()
    };
    first
        .send(Action::Create {
            name: "persist".into(),
            options: options.clone(),
        })
        .unwrap();
    wait(&mut first, |client, _| {
        grid_contains(client, "persisted-marker")
    });
    let pane = first.snapshot().unwrap().focused;
    let mut second = Client::connect(&server.socket()).unwrap();
    second
        .send(Action::Attach {
            name: "persist".into(),
            size: options.size,
        })
        .unwrap();
    wait(&mut second, |client, _| {
        client
            .snapshot()
            .is_some_and(|snapshot| snapshot.controller)
    });
    wait(&mut first, |client, _| {
        client
            .snapshot()
            .is_some_and(|snapshot| !snapshot.controller)
    });
    first
        .send(Action::Input {
            pane,
            data: b"denied-input".to_vec(),
        })
        .unwrap();
    wait(&mut first, |_, events| {
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Error(error) if error.contains("read-only")))
    });
    second
        .send(Action::SelectionStart {
            pane,
            point: Point {
                line: 0,
                column: 4096,
            },
            side: Side::Left,
            kind: SelectionType::Semantic,
        })
        .unwrap();
    wait(&mut second, |_, events| {
        events.iter().any(|event| matches!(event, ClientEvent::Error(error) if error.contains("outside the pane")))
    });
    second
        .send(Action::Split {
            pane,
            axis: Axis::LeftRight,
        })
        .unwrap();
    wait(&mut second, |client, _| {
        client
            .snapshot()
            .is_some_and(|snapshot| snapshot.panes.len() == 2)
    });
    let split = second.snapshot().unwrap().focused;
    second.send(Action::NewTab { pane: split }).unwrap();
    wait(&mut second, |client, _| {
        client
            .snapshot()
            .is_some_and(|snapshot| snapshot.tabs.len() == 2 && snapshot.panes.len() == 1)
    });
    drop(second);
    wait(&mut first, |client, _| {
        client
            .snapshot()
            .is_some_and(|snapshot| snapshot.controller)
    });
    first.send(Action::Tab(-1)).unwrap();
    wait(&mut first, |client, _| {
        client
            .snapshot()
            .is_some_and(|snapshot| snapshot.panes.len() == 2)
    });
    drop(first);
    let mut third = Client::connect(&server.socket()).unwrap();
    third
        .send(Action::Attach {
            name: "persist".into(),
            size: options.size,
        })
        .unwrap();
    wait(&mut third, |client, _| {
        grid_contains(client, "persisted-marker")
    });
    third.send(Action::TerminateName("persist".into())).unwrap();
    wait(&mut third, |_, events| {
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Detached))
    });
    server.wait_for_exit();
    assert_eq!(
        fs::read_dir(server.directory.join("history"))
            .unwrap()
            .count(),
        0,
        "explicit termination must clean workspace history"
    );
}

#[test]
fn unchanged_pointer_and_view_actions_do_not_schedule_broadcasts() {
    let server = TestServer::start();
    let (sender, _receiver) = UnixStream::pair().unwrap();
    let mut clients = vec![Connection::new(1, sender).unwrap()];
    let mut workspaces = HashMap::new();
    let mut dirty = HashSet::new();
    let mut closing = ClosingTerminals::default();
    apply_action(
        0,
        Action::Create {
            name: "quiet".into(),
            options: CreateOptions {
                program: Some(Shell::new("/bin/sleep".into(), vec!["60".into()])),
                ..CreateOptions::default()
            },
        },
        &mut clients,
        &mut workspaces,
        &mut dirty,
        &server.directory.join("history"),
        &mut closing,
    )
    .unwrap();
    let pane = workspaces["quiet"].focused();
    apply_action(
        0,
        Action::Split {
            pane,
            axis: Axis::LeftRight,
        },
        &mut clients,
        &mut workspaces,
        &mut dirty,
        &server.directory.join("history"),
        &mut closing,
    )
    .unwrap();
    let second = workspaces["quiet"].focused();
    let point = Point { line: 0, column: 0 };
    for (action, changed) in [
        (Action::Focus(second), false),
        (Action::Focus(pane), true),
        (Action::Focus(pane), false),
        (Action::Navigate(Direction::Left), false),
        (Action::Navigate(Direction::Right), true),
        (Action::Tab(0), false),
        (Action::Tab(1), false),
        (Action::Resize(CreateOptions::default().size), false),
        (
            Action::ResizeSplit {
                first: pane,
                second,
                position: 25,
            },
            true,
        ),
        (
            Action::ResizeSplit {
                first: pane,
                second,
                position: 25,
            },
            false,
        ),
        (
            Action::ResizeSplit {
                first: pane,
                second,
                position: 0,
            },
            true,
        ),
        (
            Action::ResizeSplit {
                first: pane,
                second,
                position: 0,
            },
            false,
        ),
        (
            Action::SelectionUpdate {
                pane,
                point,
                side: Side::Left,
            },
            false,
        ),
        (
            Action::SelectionStart {
                pane,
                point,
                side: Side::Left,
                kind: SelectionType::Simple,
            },
            true,
        ),
        (
            Action::SelectionUpdate {
                pane,
                point,
                side: Side::Left,
            },
            false,
        ),
        (
            Action::SelectionUpdate {
                pane,
                point,
                side: Side::Right,
            },
            true,
        ),
        (
            Action::SelectionUpdate {
                pane,
                point,
                side: Side::Right,
            },
            false,
        ),
        (Action::SelectionClear { pane }, true),
        (Action::SelectionClear { pane }, false),
        (
            Action::Scroll {
                pane,
                scroll: Scroll::Bottom,
            },
            false,
        ),
    ] {
        dirty.clear();
        clients[0].full = false;
        apply_action(
            0,
            action.clone(),
            &mut clients,
            &mut workspaces,
            &mut dirty,
            &server.directory.join("history"),
            &mut closing,
        )
        .unwrap();
        assert_eq!(!dirty.is_empty(), changed, "{action:?}");
        if changed && matches!(action, Action::ResizeSplit { .. } | Action::Resize(_)) {
            let deadline = Instant::now() + Duration::from_secs(5);
            for pane in workspaces["quiet"].panes.values() {
                while pane.terminal.core.lock().unwrap().history_loading() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        if !changed {
            assert!(!clients[0].full);
        }
    }
}

#[test]
fn dragging_nested_dividers_resizes_pty_and_preserves_other_splits() {
    let server = TestServer::start();
    let mut client = Client::connect(&server.socket()).unwrap();
    let size = WindowSize {
        num_cols: 101,
        num_lines: 32,
        ..CreateOptions::default().size
    };
    client
        .send(Action::Create {
            name: "drag".into(),
            options: CreateOptions {
                size,
                program: Some(Shell::new("/bin/sleep".into(), vec!["60".into()])),
                ..CreateOptions::default()
            },
        })
        .unwrap();
    wait(&mut client, |c, _| c.snapshot().is_some());
    let first = client.snapshot().unwrap().focused;
    client
        .send(Action::Split {
            pane: first,
            axis: Axis::LeftRight,
        })
        .unwrap();
    wait(&mut client, |c, _| c.snapshot().unwrap().panes.len() == 2);
    let second = client.snapshot().unwrap().focused;
    client
        .send(Action::Split {
            pane: second,
            axis: Axis::TopBottom,
        })
        .unwrap();
    wait(&mut client, |c, _| c.snapshot().unwrap().panes.len() == 3);
    let third = client.snapshot().unwrap().focused;
    let rect = |c: &Client, id| {
        c.snapshot()
            .unwrap()
            .panes
            .iter()
            .find(|p| p.id == id)
            .unwrap()
            .rect
    };
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
                .is_some_and(|screen| screen.lines == 9)
    });
    assert_eq!(rect(&client, first).cols, 30);
    assert_eq!(rect(&client, third).y, 11);
    assert_eq!(client.screens()[&second].lines, 9);
    client.send(vertical.action(0, 0)).unwrap();
    wait(&mut client, |c, _| {
        rect(c, first).cols == 2
            && c.screens()
                .get(&first)
                .is_some_and(|screen| screen.columns == 2)
    });
    assert_eq!(client.screens()[&first].columns, 2);
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
fn focused_splits_follow_queued_actions_and_explicit_splits_keep_their_target() {
    let server = TestServer::start();
    let mut client = Client::connect(&server.socket()).unwrap();
    client
        .send(Action::Create {
            name: "persist".into(),
            options: CreateOptions {
                size: WindowSize {
                    num_cols: 103,
                    num_lines: 31,
                    ..CreateOptions::default().size
                },
                directory: Some(server.directory.join("home")),
                program: Some(Shell::new("/bin/sleep".into(), vec!["60".into()])),
                ..CreateOptions::default()
            },
        })
        .unwrap();
    wait(&mut client, |client, _| client.snapshot().is_some());
    let first = client.snapshot().unwrap().focused;
    let rect = |client: &Client, id| {
        client
            .snapshot()
            .unwrap()
            .panes
            .iter()
            .find(|pane| pane.id == id)
            .unwrap()
            .rect
    };
    let original = rect(&client, first);
    for axis in [Axis::LeftRight, Axis::TopBottom] {
        client.send(Action::Split { pane: 0, axis }).unwrap();
    }
    wait(&mut client, |client, _| {
        client.snapshot().unwrap().panes.len() == 3
    });
    let third = client.snapshot().unwrap().focused;
    let second = client
        .snapshot()
        .unwrap()
        .panes
        .iter()
        .find(|pane| pane.id != first && pane.id != third)
        .unwrap()
        .id;
    assert_eq!(rect(&client, first).rows, original.rows);
    assert_eq!(rect(&client, second).x, 52);
    assert_eq!(rect(&client, second).rows, 14);
    assert_eq!(rect(&client, third).x, 52);
    assert_eq!(rect(&client, third).y, 16);
    let right_top = rect(&client, second);
    let right_bottom = rect(&client, third);

    client.send(Action::Focus(first)).unwrap();
    client
        .send(Action::Split {
            pane: 0,
            axis: Axis::TopBottom,
        })
        .unwrap();
    wait(&mut client, |client, _| {
        client.snapshot().unwrap().panes.len() == 4
    });
    let fourth = client.snapshot().unwrap().focused;
    assert_eq!(rect(&client, second), right_top);
    assert_eq!(rect(&client, third), right_bottom);
    assert_eq!(rect(&client, first).rows, 14);
    let left_bottom = rect(&client, fourth);
    assert_eq!(left_bottom.x, 0);
    assert_eq!(left_bottom.y, 16);

    client.send(Action::Navigate(Direction::Right)).unwrap();
    client
        .send(Action::Split {
            pane: 0,
            axis: Axis::LeftRight,
        })
        .unwrap();
    wait(&mut client, |client, _| {
        client.snapshot().unwrap().panes.len() == 5
    });
    let fifth = client.snapshot().unwrap().focused;
    assert_eq!(rect(&client, fourth), left_bottom);
    assert_eq!(rect(&client, second), right_top);
    assert_eq!(rect(&client, third).x, right_bottom.x);
    assert_eq!(rect(&client, third).cols, 25);
    assert_eq!(rect(&client, fifth).x, 78);
    let fifth_rect = rect(&client, fifth);

    assert_ne!(client.snapshot().unwrap().focused, second);
    client
        .send(Action::Split {
            pane: second,
            axis: Axis::TopBottom,
        })
        .unwrap();
    wait(&mut client, |client, _| {
        client.snapshot().unwrap().panes.len() == 6
    });
    let sixth = client.snapshot().unwrap().focused;
    assert_eq!(rect(&client, fifth), fifth_rect);
    assert_eq!(rect(&client, second).rows, 6);
    assert_eq!(rect(&client, sixth).x, right_top.x);
    assert_eq!(rect(&client, sixth).y, 8);
    assert_eq!(rect(&client, sixth).rows, 7);
}

#[test]
fn queued_selection_survives_history_resize() {
    let server = TestServer::start();
    let mut client = Client::connect(&server.socket()).unwrap();
    let size = WindowSize {
        num_cols: 100,
        num_lines: 25,
        ..CreateOptions::default().size
    };
    client
            .send(Action::Create {
                name: "persist".into(),
                options: CreateOptions {
                    size,
                    directory: Some(server.directory.join("home")),
                    program: Some(Shell::new(
                        "/usr/bin/python3".into(),
                        vec![
                            "-c".into(),
                            "import time; print(('history line ' + 'x' * 70 + '\\n') * 6000, end=''); print('READY', flush=True); time.sleep(60)".into(),
                        ],
                    )),
                    ..CreateOptions::default()
                },
            })
            .unwrap();
    wait(&mut client, |client, _| grid_contains(client, "READY"));
    let pane = client.snapshot().unwrap().focused;
    assert!(client.screens()[&pane].history_size() > 4000);
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
    use std::io::Read;
    let server = TestServer::start();
    let mut client = Client::connect(&server.socket()).unwrap();
    client
        .send(Action::Create {
            name: "persist".into(),
            options: CreateOptions {
                directory: Some(server.directory.join("home")),
                program: Some(Shell::new(
                    "/bin/sh".into(),
                    vec!["-c".into(), "printf 'firstcopy\\n'; exec /bin/sh".into()],
                )),
                ..CreateOptions::default()
            },
        })
        .unwrap();
    wait(&mut client, |client, _| grid_contains(client, "firstcopy"));
    let first = client.snapshot().unwrap().focused;
    client
        .send(Action::Split {
            pane: first,
            axis: Axis::LeftRight,
        })
        .unwrap();
    wait(&mut client, |client, _| {
        client.snapshot().unwrap().panes.len() == 2
    });
    let second = client.snapshot().unwrap().focused;
    client
        .send(Action::Input {
            pane: second,
            data: b"printf '\\163econdcopy\\n'\n".to_vec(),
        })
        .unwrap();
    wait(&mut client, |client, _| grid_contains(client, "secondcopy"));
    let screen = &client.screens()[&second];
    let point = (0..screen.screen_lines())
        .find_map(|line| {
            let text = screen
                .row(line)
                .cells
                .iter()
                .map(|cell| cell.c)
                .collect::<String>();
            text.find("secondcopy").map(|column| Point {
                line: line as i32 - screen.display_offset() as i32,
                column,
            })
        })
        .unwrap();
    client
        .send(Action::SelectionStart {
            pane: first,
            point: Point::default(),
            side: Side::Left,
            kind: SelectionType::Simple,
        })
        .unwrap();
    client
        .send(Action::SelectionUpdate {
            pane: first,
            point: Point { line: 0, column: 8 },
            side: Side::Right,
        })
        .unwrap();
    wait(&mut client, |client, _| {
        client.screens()[&first].selection_range().is_some()
    });
    client
        .send(Action::SelectionStart {
            pane: second,
            point,
            side: Side::Left,
            kind: SelectionType::Simple,
        })
        .unwrap();
    client
        .send(Action::SelectionUpdate {
            pane: second,
            point: Point {
                column: point.column + 9,
                ..point
            },
            side: Side::Right,
        })
        .unwrap();
    wait(&mut client, |client, _| {
        client.screens()[&first].selection_range().is_none()
            && client.screens()[&second].selection_range().is_some()
    });
    client
        .send(Action::CopySelection {
            pane: second,
            request: 77,
        })
        .unwrap();
    let mut copied = None;
    wait(&mut client, |_, events| {
        for event in events {
            if let ClientEvent::Clipboard {
                request: 77,
                file,
                incomplete,
            } = event
            {
                assert!(!incomplete);
                let mut text = String::new();
                file.try_clone().unwrap().read_to_string(&mut text).unwrap();
                copied = Some(text);
            }
        }
        copied.is_some()
    });
    assert_eq!(copied.as_deref(), Some("secondcopy"));
    client
        .send(Action::CopySelection {
            pane: first,
            request: 78,
        })
        .unwrap();
    wait(&mut client, |_, events| {
        events.iter().any(
            |event| matches!(event, ClientEvent::Error(error) if error.contains("no selection")),
        )
    });
}

#[test]
fn tab_titles_follow_the_focused_pane_directory_instead_of_application_titles() {
    let server = TestServer::start();
    let home = server.directory.join("home");
    fs::write(home.join(".zshrc"), "").unwrap();
    fs::create_dir(home.join("alpha")).unwrap();
    fs::create_dir(home.join("beta")).unwrap();
    let mut client = Client::connect(&server.socket()).unwrap();
    client
        .send(Action::Create {
            name: "persist".into(),
            options: CreateOptions {
                directory: Some(home),
                program: Some(Shell::new(
                    "/bin/sh".into(),
                    vec![
                        "-c".into(),
                        "printf 'directory-ready\\n'; exec /bin/sh".into(),
                    ],
                )),
                ..CreateOptions::default()
            },
        })
        .unwrap();
    wait(&mut client, |client, _| {
        grid_contains(client, "directory-ready")
    });
    assert_eq!(client.snapshot().unwrap().tabs[0].title, "home");
    let first = client.snapshot().unwrap().focused;
    let change_directory = |client: &mut Client, pane, directory, application, tab_title| {
        client
            .send(Action::Input {
                pane,
                data: format!(
                    "cd {directory}; printf '\\033]2;{application}\\007{application}\\n'\n"
                )
                .into_bytes(),
            })
            .unwrap();
        wait(client, |client, _| {
            let snapshot = client.snapshot().unwrap();
            // The shell can report its application title before its next cwd update.
            snapshot
                .panes
                .iter()
                .any(|view| view.id == pane && view.title == application)
                && snapshot
                    .tabs
                    .iter()
                    .any(|tab| tab.id == snapshot.active_tab && tab.title == tab_title)
        });
    };
    change_directory(&mut client, first, "alpha", "first-application", "alpha");
    assert_eq!(client.snapshot().unwrap().tabs[0].title, "alpha");
    client
        .send(Action::Split {
            pane: first,
            axis: Axis::LeftRight,
        })
        .unwrap();
    wait(&mut client, |client, _| {
        let snapshot = client.snapshot().unwrap();
        snapshot.panes.len() == 2 && snapshot.tabs[0].title == "alpha"
    });
    let split = client.snapshot().unwrap().focused;
    change_directory(&mut client, split, "../beta", "split-application", "beta");
    assert_eq!(client.snapshot().unwrap().tabs[0].title, "beta");
    client.send(Action::Focus(first)).unwrap();
    wait(&mut client, |client, _| {
        client.snapshot().unwrap().focused == first
    });
    assert_eq!(client.snapshot().unwrap().tabs[0].title, "alpha");
    client.send(Action::NewTab { pane: split }).unwrap();
    wait(&mut client, |client, _| {
        let snapshot = client.snapshot().unwrap();
        snapshot.tabs.len() == 2 && snapshot.tabs[1].title == "beta"
    });
    let second = client.snapshot().unwrap().focused;
    assert_eq!(client.snapshot().unwrap().tabs[0].title, "alpha");
    change_directory(&mut client, second, "/", "root-application", "/");
    assert_eq!(client.snapshot().unwrap().tabs[1].title, "/");
    client.send(Action::Tab(-1)).unwrap();
    wait(&mut client, |client, _| {
        client.snapshot().unwrap().focused == first
    });
    client.send(Action::Focus(split)).unwrap();
    wait(&mut client, |client, _| {
        client.snapshot().unwrap().focused == split
    });
    let snapshot = client.snapshot().unwrap();
    assert_eq!(snapshot.tabs[0].title, "beta");
    assert_eq!(snapshot.tabs[1].title, "/");
    assert_eq!(
        snapshot
            .panes
            .iter()
            .find(|pane| pane.id == split)
            .unwrap()
            .title,
        "split-application"
    );
}

#[test]
fn closing_an_inactive_tab_terminates_its_panes_and_preserves_active_tab() {
    let mut server = TestServer::start();
    let mut client = Client::connect(&server.socket()).unwrap();
    client
        .send(Action::Create {
            name: "persist".into(),
            options: CreateOptions {
                directory: Some(server.directory.join("home")),
                program: Some(Shell::new("/bin/sh".into(), Vec::new())),
                ..CreateOptions::default()
            },
        })
        .unwrap();
    wait(&mut client, |client, _| client.snapshot().is_some());
    let first = client.snapshot().unwrap().active_tab;
    client
        .send(Action::Split {
            pane: first,
            axis: Axis::LeftRight,
        })
        .unwrap();
    wait(&mut client, |client, _| {
        client.snapshot().unwrap().panes.len() == 2
    });
    let split = client.snapshot().unwrap().focused;
    client.send(Action::NewTab { pane: split }).unwrap();
    wait(&mut client, |client, _| {
        client.snapshot().unwrap().tabs.len() == 2
    });
    let second = client.snapshot().unwrap().focused;
    client.send(Action::NewTab { pane: second }).unwrap();
    wait(&mut client, |client, _| {
        client.snapshot().unwrap().tabs.len() == 3
    });
    let active = client.snapshot().unwrap().active_tab;
    client.send(Action::CloseTab(first)).unwrap();
    wait(&mut client, |client, _| {
        client.snapshot().unwrap().tabs.len() == 2
    });
    assert_eq!(client.snapshot().unwrap().active_tab, active);
    client.send(Action::List).unwrap();
    wait(&mut client, |_, events| {
        events.iter().any(|event| {
            matches!(event, ClientEvent::List(items) if items.len() == 1 && items[0].panes == 2)
        })
    });
    client.send(Action::CloseTab(second)).unwrap();
    wait(&mut client, |client, _| {
        client.snapshot().unwrap().tabs.len() == 1
    });
    assert_eq!(client.snapshot().unwrap().active_tab, active);
    client.send(Action::CloseTab(active)).unwrap();
    wait(&mut client, |_, events| {
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Detached))
    });
    server.wait_for_exit();
    assert_eq!(
        fs::read_dir(server.directory.join("history"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn command_copy_streams_full_output_across_cold_history() {
    use std::io::Read;
    let server = TestServer::start();
    let mut client = Client::connect(&server.socket()).unwrap();
    let script = "printf '\\033]133;A\\aprompt \\033]133;B\\acommand\\r\\n\\033]133;C\\a'; i=0; while [ \"$i\" -lt 700 ]; do printf 'line-%04d 中\\n' \"$i\"; i=$((i+1)); done; printf '\\033]133;D;0\\a\\033]133;A\\aprompt \\033]133;B\\a'; exec /bin/sh";
    let options = CreateOptions {
        directory: Some(server.directory.join("home")),
        program: Some(Shell::new(
            "/bin/sh".into(),
            vec!["-c".into(), script.into()],
        )),
        ..CreateOptions::default()
    };
    client
        .send(Action::Create {
            name: "persist".into(),
            options,
        })
        .unwrap();
    wait(&mut client, |client, _| grid_contains(client, "line-0699"));
    wait(&mut client, |client, _| {
        client.snapshot().unwrap().panes.iter().any(|pane| {
            pane.command_blocks
                .iter()
                .any(|block| block.status == Some(0))
        })
    });
    let pane = &client.snapshot().unwrap().panes[0];
    let block = pane
        .blocks
        .iter()
        .filter(|block| block.has_output)
        .max_by_key(|block| block.id)
        .unwrap()
        .id;
    assert!(
        pane.command_blocks
            .iter()
            .any(|command| { command.id == block && command.complete && command.has_output })
    );
    let pane = pane.id;
    client
        .peer
        .send_action(&Action::Resize(WindowSize {
            num_cols: 64,
            ..CreateOptions::default().size
        }))
        .unwrap();
    client
        .peer
        .send_action(&Action::CopyBlock {
            pane,
            block,
            request: 1,
        })
        .unwrap();
    client.flush().unwrap();
    let mut output = None;
    wait(&mut client, |_, events| {
        for event in events {
            if let ClientEvent::Clipboard {
                request: 1,
                file,
                incomplete,
            } = event
            {
                assert!(!incomplete);
                let mut text = String::new();
                file.try_clone().unwrap().read_to_string(&mut text).unwrap();
                output = Some(text);
                return true;
            }
        }
        false
    });
    let output = output.unwrap();
    assert!(output.starts_with("line-0000 中\n"));
    assert!(output.ends_with("line-0699 中\n"));
    assert_eq!(output.lines().count(), 700);
}
fn blocked_input_client(server: &TestServer) -> Client {
    let mut client = Client::connect(&server.socket()).unwrap();
    let options = CreateOptions {
        directory: Some(server.directory.join("home")),
        program: Some(Shell::new(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "stty raw -echo; printf 'blocked-ready\\n'; exec sleep 30".into(),
            ],
        )),
        ..CreateOptions::default()
    };
    client
        .send(Action::Create {
            name: "persist".into(),
            options,
        })
        .unwrap();
    wait(&mut client, |client, _| {
        grid_contains(client, "blocked-ready")
    });
    let pane = client.snapshot().unwrap().focused;
    client
        .send(Action::Input {
            pane,
            data: vec![b'x'; 8 * 1024 * 1024],
        })
        .unwrap();
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
    client
        .send(Action::Create {
            name: "oversized".into(),
            options: CreateOptions {
                directory: Some(server.directory.join("home")),
                program: Some(Shell::new(
                    "python3".into(),
                    vec!["-c".into(), script.into(), ready.to_str().unwrap().into()],
                )),
                ..CreateOptions::default()
            },
        })
        .unwrap();
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
        if client.poll().is_err() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
    };
    drop(client);
    let mut control = Client::connect(&server.socket()).unwrap();
    control
        .send(Action::TerminateName("oversized".into()))
        .unwrap();
    wait(&mut control, |_, events| {
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Detached))
    });
    server.wait_for_exit();
    let log = fs::read_to_string(server.directory.join("server.log")).unwrap();
    assert!(log.contains("send queue is full"), "{log}");
    assert!(
        disconnected,
        "failed broadcast waited for another wakeup: {log}"
    );
}

#[test]
fn dropping_a_backpressured_frontend_leaves_the_daemon_idle_and_attachable() {
    let server = TestServer::start();
    let client = blocked_input_client(&server);
    drop(client);
    std::thread::sleep(Duration::from_millis(50));
    let before = process_cpu_ticks(server.child.id());
    std::thread::sleep(Duration::from_millis(200));
    let ticks = process_cpu_ticks(server.child.id()) - before;
    assert!(
        ticks <= 2,
        "closed backpressured frontend spun the daemon: {ticks} CPU ticks in 200 ms"
    );
    let mut replacement = Client::connect(&server.socket()).unwrap();
    replacement
        .send(Action::Attach {
            name: "persist".into(),
            size: CreateOptions::default().size,
        })
        .unwrap();
    wait(&mut replacement, |client, _| {
        client
            .snapshot()
            .is_some_and(|snapshot| snapshot.controller)
    });
    replacement
        .send(Action::TerminateName("persist".into()))
        .unwrap();
    wait(&mut replacement, |_, events| {
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Detached))
    });
}
#[test]
fn terminating_a_workspace_cancels_old_paste_after_detached_is_received() {
    let server = TestServer::start();
    let mut client = blocked_input_client(&server);
    let mut control = Client::connect(&server.socket()).unwrap();
    control
        .send(Action::TerminateName("persist".into()))
        .unwrap();
    wait(&mut control, |_, events| {
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Detached))
    });
    client.flush().unwrap();
    wait(&mut client, |_, events| {
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Detached))
    });
    assert!(!client.wants_write());
    assert!(client.snapshot().is_none());
}
#[test]
fn detached_behind_a_large_response_supersedes_a_known_write_failure() {
    let (left, right) = UnixStream::pair().unwrap();
    let mut sender = Peer::new(left).unwrap();
    assert_eq!(unsafe { libc::shutdown(sender.fd(), libc::SHUT_RD) }, 0);
    sender
        .send_response(Response::ClipboardStore {
            primary: false,
            text: "x".repeat(600000),
        })
        .unwrap();
    sender.send_response(Response::Detached).unwrap();
    let worker = std::thread::spawn(move || {
        while sender.pending() {
            sender.flush().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let mut client = Client {
        peer: Peer::new(right).unwrap(),
        #[cfg(feature = "agent")]
        endpoint: ClientEndpoint::Local(PathBuf::new()),
        agent_events: Vec::new(),
        agent_delegate: false,
        snapshot: None,
        screens: HashMap::new(),
        write_error: None,
        ssh: None,
        remote_target: None,
    };
    client.peer.send_input(1, b"pending-input").unwrap();
    client.flush().unwrap();
    assert!(client.write_error.is_some());
    assert!(!client.wants_write());
    let mut copied = false;
    wait(&mut client, |_, events| {
        copied |= events.iter().any(|event| matches!(event, ClientEvent::ClipboardStore { text, .. } if text.len() == 600000));
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Detached))
    });
    assert!(copied);
    assert!(!client.peer.pending());
    assert!(client.write_error.is_none());
    worker.join().unwrap();
}
#[test]
fn consecutive_state_notifications_merge_without_skipping_deltas_or_control_events() {
    let (left, right) = UnixStream::pair().unwrap();
    let mut sender = Peer::new(left).unwrap();
    let mut client = Client {
        peer: Peer::new(right).unwrap(),
        #[cfg(feature = "agent")]
        endpoint: ClientEndpoint::Local(PathBuf::new()),
        agent_events: Vec::new(),
        agent_delegate: false,
        snapshot: None,
        screens: HashMap::new(),
        write_error: None,
        ssh: None,
        remote_target: None,
    };
    let size = WindowSize {
        num_cols: 2,
        num_lines: 2,
        cell_width: 0,
        cell_height: 0,
    };
    let snapshot = Snapshot {
        name: "burst".into(),
        controller: true,
        focused: 1,
        active_tab: 1,
        tabs: Vec::new(),
        panes: vec![PaneView {
            id: 1,
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
        }],
    };
    let mut core = crate::core::Core::new(size, 0xffffff, 0);
    let mut parser = crate::vt::Parser::new();
    parser.advance(&mut core, b"A");
    sender
        .send_state(
            &snapshot,
            &[(1, &Update::capture(&core, true))],
            &HashMap::new(),
        )
        .unwrap();
    core.reset_damage();
    parser.advance(&mut core, b"\x1b[2;1HB");
    sender
        .send_state(
            &snapshot,
            &[(1, &Update::capture(&core, false))],
            &HashMap::new(),
        )
        .unwrap();
    sender.send_state(&snapshot, &[], &HashMap::new()).unwrap();
    sender
        .send_response(Response::Error("marker".into()))
        .unwrap();
    sender.send_state(&snapshot, &[], &HashMap::new()).unwrap();
    sender.flush().unwrap();
    let events = client.poll().unwrap();
    assert!(
        matches!(events.as_slice(), [ClientEvent::Updated, ClientEvent::Error(error), ClientEvent::Updated] if error == "marker")
    );
    assert_eq!(client.screens()[&1].row(0).cells[0].c, 'A');
    assert_eq!(client.screens()[&1].row(1).cells[0].c, 'B');
}
#[test]
fn write_failure_without_detached_still_reports_the_original_error() {
    let (left, right) = UnixStream::pair().unwrap();
    drop(left);
    let mut client = Client {
        peer: Peer::new(right).unwrap(),
        #[cfg(feature = "agent")]
        endpoint: ClientEndpoint::Local(PathBuf::new()),
        agent_events: Vec::new(),
        agent_delegate: false,
        snapshot: None,
        screens: HashMap::new(),
        write_error: None,
        ssh: None,
        remote_target: None,
    };
    client.peer.send_input(1, b"input").unwrap();
    client.flush().unwrap();
    assert!(matches!(client.poll(), Err(error) if error.kind() == io::ErrorKind::BrokenPipe));
}
#[test]
fn a_complete_control_request_is_processed_even_if_its_socket_closes() {
    let server = TestServer::start();
    let mut client = blocked_input_client(&server);
    let mut control = Client::connect(&server.socket()).unwrap();
    control
        .send(Action::TerminateName("persist".into()))
        .unwrap();
    drop(control);
    wait(&mut client, |_, events| {
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::Detached))
    });
    assert!(!client.wants_write());
}
