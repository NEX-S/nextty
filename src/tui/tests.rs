use super::*;
use crate::core::Core;
use crate::panel::rect;
use crate::screen::Update;
use crate::ui::tests::{actions, pair, pane, size};
use crate::vt::Parser;
use crate::workspace::TabActivity;
use std::os::fd::OwnedFd;

/// The bytes that repaint pane row 0 over a host row holding stale text.
fn draw_row(screen: &Screen, pane: &PaneView, host_columns: u16) -> Vec<u8> {
    let rect = rect(0, 0, host_columns, pane.rect.y + pane.rect.rows);
    let mut previous = crate::panel::Grid::new(rect, 0xcccccc, 0x202020);
    previous.text(0, pane.rect.y, 1, "?", 0xcccccc, 0x202020);
    let mut grid = crate::panel::Grid::new(rect, 0xcccccc, 0x202020);
    paint_row(&mut grid, screen, pane, 0, host_columns, (false, None), 0);
    let mut output = Vec::new();
    grid.ansi(&mut output, Some(&previous)).unwrap();
    output
}

fn pty() -> (OwnedFd, OwnedFd) {
    let mut master = -1;
    let mut slave = -1;
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 640,
        ws_ypixel: 480,
    };
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        },
        0
    );
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

#[test]
fn stdout_hangup_child() {
    let Some(socket) = std::env::var_os("NEXTTY_TEST_OUTPUT_HANGUP_SOCKET") else {
        return;
    };
    run(
        Client::connect(std::path::Path::new(&socket)).unwrap(),
        crate::config::Config::default(),
    )
    .unwrap();
    std::process::exit(0);
}

#[test]
fn stdout_hangup_detaches_even_while_stdin_remains_open() {
    use crate::wire::Peer;
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};

    let socket = std::env::current_dir().unwrap().join(format!(
        "target/tui-output-hangup-{}.sock",
        std::process::id()
    ));
    let listener = UnixListener::bind(&socket).unwrap();
    let (input_master, input_slave) = pty();
    let (output_master, output_slave) = pty();
    for fd in [&input_master, &input_slave, &output_master, &output_slave] {
        assert_eq!(
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tui::tests::stdout_hangup_child", "--nocapture"])
        .env("NEXTTY_TEST_OUTPUT_HANGUP_SOCKET", &socket)
        .stdin(input_slave)
        .stdout(output_slave)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut server = Peer::new(listener.accept().unwrap().0).unwrap();
    let mut output = std::fs::File::from(output_master);
    let mut bytes = Vec::new();
    while !bytes.windows(ENTER.len()).any(|part| part == ENTER) {
        let mut pollfd = libc::pollfd {
            fd: output.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut pollfd, 1, 5000) }, 1);
        let mut buffer = [0; 4096];
        let count = output.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    drop(output);
    let status = (0..500).find_map(|_| {
        std::thread::sleep(Duration::from_millis(1));
        child.try_wait().unwrap()
    });
    if status.is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    std::fs::remove_file(socket).unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status
            .expect("TUI kept running after stdout hung up")
            .success(),
        "{stderr}"
    );
    assert!(matches!(
        server.next_action().unwrap(),
        Some(Action::Resize(_))
    ));
    let mut action = server.next_action().unwrap();
    while matches!(action, Some(Action::ScreenRequest { .. })) {
        action = server.next_action().unwrap();
    }
    assert!(matches!(action, Some(Action::Detach)));
    drop(input_master);
}

#[test]
fn file_paste_edits_only_local_query_and_enter_inserts_the_path() {
    let directory = std::env::current_dir()
        .unwrap()
        .join(format!("target/tui-file-input-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("quoted 'name 中文");
    std::fs::write(&file, b"preview").unwrap();
    let (client, mut peer) = pair(&[(1, "shell", TabActivity::Idle)], Some(directory.clone()));
    let mut frontend =
        Frontend::new(client, size(80, 24), crate::config::Config::default()).unwrap();
    frontend.ui.files = Some(crate::files::FileNav::new(directory.clone()).unwrap());
    frontend.ui.files_open = true;
    frontend.ui.focus = Focus::Files;
    let pasted = "/hjkl\x1bq\n中文".repeat(1024);
    frontend
        .panel_paste(pasted.as_bytes(), PasteTarget::Files)
        .unwrap();
    let files = frontend.ui.files.as_ref().unwrap();
    assert_eq!(
        files.query.as_ref().unwrap().text,
        "/hjklq中文".repeat(1024)
    );
    assert!(frontend.ui.files_open);
    assert!(actions(&mut frontend.client, &mut peer).is_empty());

    let files = frontend.ui.files.as_mut().unwrap();
    files.stop_search();
    let deadline = Instant::now() + Duration::from_secs(5);
    while files.searching() {
        files.poll();
        assert!(Instant::now() < deadline, "file navigation did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(files.selected().unwrap().path, file);
    frontend.panel_bytes(b"l").unwrap();
    assert!(frontend.ui.file_preview_open());
    frontend
        .input(Input::Bytes(smallvec::smallvec![0x1b]))
        .unwrap();
    assert!(!frontend.ui.file_preview_open());
    frontend.panel_bytes(b"\r").unwrap();
    assert!(frontend.ui.files_open);
    let [Action::Input { data, .. }] = &actions(&mut frontend.client, &mut peer)[..] else {
        panic!("Enter should insert the selected path once");
    };
    assert_eq!(data, file.to_string_lossy().as_bytes());
    drop(frontend);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn right_prompt_reaches_pane_edge_and_repaint_clears_its_old_position() {
    for (right, start) in [("main", 17), ("中a\u{301}", 18)] {
        let mut core = Core::new(size(21, 3), 0xaaaaaa, 0x202020);
        let mut parser = Parser::new();
        parser.advance(
            &mut core,
            format!("\x1b]133;A\x07 • \x1b]133;B\x07cmd\x1b[1;{}H\x1b]133;P;2\x07{right}\x1b]133;P;0\x07\x1b[1;4H", start + 1).as_bytes(),
        );
        core.selection_start(Point::default(), Side::Left, SelectionType::Simple);
        core.selection_update(
            Point {
                line: 0,
                column: 20,
            },
            Side::Right,
        );
        let pane = pane(1, 1, rect(2, 1, 21, 3));
        let mut screen = Screen::default();
        screen.apply(Update::capture(&core, true));
        let mut host = Core::new(size(28, 5), 0xaaaaaa, 0x202020);
        let mut host_parser = Parser::new();
        host_parser.advance(&mut host, &draw_row(&screen, &pane, 28));
        assert_eq!(host.row(1).cells[3].c, '•');
        assert_eq!(
            host.row(1).cells[22].c,
            if right == "main" { 'n' } else { 'a' }
        );
        assert_eq!(
            host.row(1).cells[21].bg,
            Color::Spec(Rgb {
                r: 32,
                g: 32,
                b: 32
            })
        );
        if right != "main" {
            assert_eq!(
                host.row(1).extra(22).unwrap().zerowidth.as_slice(),
                &['\u{301}']
            );
        }
        parser.advance(&mut core, format!("\x1b[1;{}H\x1b[K", start + 1).as_bytes());
        screen.apply(Update::capture(&core, true));
        host_parser.advance(&mut host, &draw_row(&screen, &pane, 28));
        assert!(host.row(1).cells[19..23].iter().all(|cell| cell.c == ' '));
    }
}

#[test]
fn sparse_redraw_erases_old_text_and_keeps_styled_trailing_spaces() {
    use crate::core::{Cell, Row};
    let core = Core::new(size(40, 2), 0xcccccc, 0x202020);
    let mut update = Update::capture(&core, true);
    let mut cells = vec![Cell::default(); 40];
    cells[0].c = 'x';
    cells[39].bg = Color::Indexed(1);
    update.rows[0].1 = Row::from_parts(cells, false, 1, Vec::new());
    let mut screen = Screen::default();
    screen.apply(update);
    let pane = pane(1, 1, rect(0, 0, 40, 2));
    let mut host = Core::new(size(40, 2), 0xcccccc, 0x202020);
    let mut parser = Parser::new();
    parser.advance(&mut host, b"old contents must disappear");
    parser.advance(&mut host, &draw_row(&screen, &pane, 40));
    assert_eq!(host.row(0).cells[0].c, 'x');
    assert!(host.row(0).cells[1..].iter().all(|cell| cell.c == ' '));
    assert_eq!(
        host.row(0).cells[39].bg,
        Color::Spec(screen.color(Color::Indexed(1)))
    );

    screen.apply(Update::capture(&core, true));
    let output = draw_row(&screen, &pane, 40);
    assert!(
        output.len() < 80,
        "empty rows should not emit a screenful of spaces"
    );
    parser.advance(&mut host, &output);
    let background = Color::Spec(screen.color(Color::Named(NamedColor::Background)));
    assert!(
        host.row(0)
            .cells
            .iter()
            .all(|cell| cell.c == ' ' && cell.bg == background)
    );
}

#[test]
fn raw_guard_restores_termios_and_receives_resize_notifications() {
    let (_master, slave) = pty();
    let mut original: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut original) },
        0
    );
    {
        let guard = TerminalGuard::enter(slave.as_raw_fd(), slave.as_raw_fd()).unwrap();
        let mut raw: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut raw) }, 0);
        assert_eq!(raw.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG), 0);
        assert_eq!(
            unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGWINCH) },
            0
        );
        assert_eq!(guard.next_signal().unwrap(), Some(libc::SIGWINCH as u32));
        assert_eq!(guard.next_signal().unwrap(), None);
    }
    let mut restored: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut restored) },
        0
    );
    assert_eq!(restored.c_iflag, original.c_iflag);
    assert_eq!(restored.c_oflag, original.c_oflag);
    assert_eq!(restored.c_lflag, original.c_lflag);
    assert_eq!(restored.c_cc, original.c_cc);
}

#[test]
fn clipboard_stream_crosses_chunk_boundaries_without_truncation() {
    let plain = vec![b'x'; OSC52_CHUNK_SIZE * 3 + 2];
    let mut transfer = ClipboardTransfer {
        reader: Box::new(io::Cursor::new(plain.clone())),
        target: b'c',
        incomplete: true,
        bytes: 0,
    };
    let mut output = Vec::new();
    assert!(transfer.step(&mut output).unwrap().is_none());
    let message = loop {
        if let Some(message) = transfer.step(&mut output).unwrap() {
            break message;
        }
    };
    assert!(message.contains(&format!("{} bytes", plain.len())));
    assert!(message.contains("incomplete"));
    assert!(message.contains("host may reject"));
    assert!(output.starts_with(b"\x1b]52;c;"));
    assert!(output.ends_with(b"\x1b\\"));
    assert_eq!(
        crate::base64::decode_utf8(&output[7..output.len() - 2])
            .unwrap()
            .as_bytes(),
        plain
    );
}

mod agent {
    use super::*;

    #[test]
    fn agent_editor_keeps_utf8_fragments_and_recovers_after_invalid_bytes() {
        let (client, _server) = pair(&[(1, "shell", TabActivity::Idle)], None);
        let mut frontend =
            Frontend::new(client, size(80, 24), crate::config::Config::default()).unwrap();
        frontend.ui.focus = Focus::Editor;
        frontend.ui.agent.open = true;
        frontend.panel_bytes(b"a\xff\xe4").unwrap();
        assert_eq!(frontend.ui.agent.active().model.input, "a�");
        frontend.panel_bytes(b"\xb8\xadb").unwrap();
        assert_eq!(frontend.ui.agent.active().model.input, "a�中b");
        let mut decoder = Decoder::default();
        for event in decoder.feed("\x1béx".as_bytes(), Instant::now()) {
            frontend.input(event).unwrap();
        }
        assert_eq!(frontend.ui.agent.active().model.input, "a�中béx");
        assert!(frontend.running);
    }

    /// A frontend over one pane whose screen shows `rows` lines of text.
    fn frontend_with_text(host: WindowSize, rows: u16) -> (Frontend, crate::wire::Peer, Core) {
        let (client, mut server) = pair(&[(1, "shell", TabActivity::Running)], None);
        let mut terminal = Core::new(size(80, rows), 0xcccccc, 0x202020);
        for row in 0..rows {
            let text = format!("\x1b[{};1Hrow {row:02} unchanged text", row + 1);
            Parser::new().advance(&mut terminal, text.as_bytes());
        }
        let state =
            crate::wire::Response::State(None, vec![(1, Update::capture(&terminal, true))], 0);
        server.send_response(state).unwrap();
        server.flush().unwrap();
        let mut frontend = Frontend::new(client, host, crate::config::Config::default()).unwrap();
        frontend.poll().unwrap();
        (frontend, server, terminal)
    }

    #[test]
    fn modal_overlays_restore_terminal_and_agent_rows_without_clearing_the_screen() {
        let host = size(100, 40);
        let (mut frontend, _server, _) = frontend_with_text(host, 22);
        frontend.ui.agent.open = true;
        frontend
            .ui
            .agent
            .active_mut()
            .model
            .insert("keep this Agent draft")
            .unwrap();
        let mut screen = Core::new(host, 0xcccccc, 0x202020);
        let mut parser = Parser::new();
        let mut paint = |frontend: &mut Frontend| {
            let mut bytes = Vec::new();
            frontend.draw(&mut bytes).unwrap();
            assert!(!bytes.windows(4).any(|part| part == b"\x1b[2J"));
            assert!(bytes.starts_with(b"\x1b[?2026h") && bytes.ends_with(b"\x1b[?2026l"));
            parser.advance(&mut screen, &bytes);
            (0..host.num_lines)
                .map(|row| screen.row(usize::from(row)).cells.clone())
                .collect::<Vec<_>>()
        };
        let baseline = paint(&mut frontend);
        frontend.input(Input::Alt(b'/')).unwrap();
        paint(&mut frontend);
        frontend
            .input(Input::Bytes(smallvec::smallvec![0x1b]))
            .unwrap();
        assert_eq!(paint(&mut frontend), baseline);
        let tab = frontend.client.snapshot().unwrap().tabs[0].clone();
        assert!(!frontend.ui.request_close_tab(&tab));
        paint(&mut frontend);
        frontend.input(Input::Bytes(b"\t".to_vec().into())).unwrap();
        paint(&mut frontend);
        frontend
            .input(Input::Bytes(smallvec::smallvec![0x1b]))
            .unwrap();
        assert_eq!(paint(&mut frontend), baseline);
    }

    #[test]
    fn output_only_draw_repaints_damaged_rows_and_matches_a_full_draw() {
        let host = size(80, 24);
        let (mut frontend, mut server, mut terminal) = frontend_with_text(host, 22);
        let mut screen = Core::new(host, 0xcccccc, 0x202020);
        let mut parser = Parser::new();
        let mut bytes = Vec::new();
        frontend.draw(&mut bytes).unwrap();
        parser.advance(&mut screen, &bytes);
        // The second full draw leaves the previous frame for incremental draws to diff against.
        bytes.clear();
        frontend.draw(&mut bytes).unwrap();
        parser.advance(&mut screen, &bytes);

        terminal.reset_damage();
        Parser::new().advance(&mut terminal, b"\x1b[4;1Hhello\x1b[2;1H\x1b[K");
        let state =
            crate::wire::Response::State(None, vec![(1, Update::capture(&terminal, false))], 0);
        server.send_response(state).unwrap();
        server.flush().unwrap();
        frontend.poll().unwrap();
        frontend.dirty = false;
        bytes.clear();
        frontend.draw(&mut bytes).unwrap();
        assert!(!bytes.windows(3).any(|part| part == b"row"), "{bytes:?}");
        parser.advance(&mut screen, &bytes);

        frontend.frame = None;
        bytes.clear();
        frontend.draw(&mut bytes).unwrap();
        let mut fresh = Core::new(host, 0xcccccc, 0x202020);
        Parser::new().advance(&mut fresh, &bytes);
        for row in 0..usize::from(host.num_lines) {
            let shown = |core: &Core| -> Vec<_> {
                core.row(row)
                    .cells
                    .iter()
                    .map(|cell| (cell.c, cell.bg))
                    .collect()
            };
            assert_eq!(shown(&screen), shown(&fresh), "row {row}");
        }
    }
}
