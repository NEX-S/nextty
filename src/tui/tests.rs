use super::*;

#[cfg(feature = "agent")]
#[test]
fn agent_view_sync_tracks_keys_layout_rewind_and_controller_recovery() {
    use crate::wire::{Peer, Response};
    use serde_json::json;
    use std::os::unix::net::UnixListener;
    let path =
        std::path::PathBuf::from(format!("target/tui-agent-sync-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&path).unwrap();
    let client = Client::connect(&path).unwrap();
    let mut server = Peer::new(listener.accept().unwrap().0).unwrap();
    let size = WindowSize {
        num_cols: 140,
        num_lines: 40,
        cell_width: 0,
        cell_height: 0,
    };
    let mut snapshot = Snapshot {
        name: "sync".into(),
        controller: true,
        focused: 1,
        active_tab: 1,
        tabs: Vec::new(),
        panes: Vec::new(),
    };
    server
        .send_response(Response::State(snapshot.clone(), Vec::new()))
        .unwrap();
    server
        .send_response(Response::Agent {
            kind: 5,
            payload: Vec::new(),
        })
        .unwrap();
    server.flush().unwrap();
    let mut frontend = Frontend::new(client, size, crate::config::Config::default()).unwrap();
    frontend.poll().unwrap();
    let sync = |frontend: &mut Frontend, server: &mut Peer| {
        frontend
            .ui
            .poll_agent(&mut frontend.client, &frontend.config)
            .unwrap();
        frontend.client.flush().unwrap();
        let mut views = Vec::<serde_json::Value>::new();
        while let Some(action) = server.next_action().unwrap() {
            if let Action::AgentView(payload) = action {
                views.push(serde_json::from_slice(&payload).unwrap());
            }
        }
        views
    };
    assert_eq!(sync(&mut frontend, &mut server).len(), 1);
    assert!(sync(&mut frontend, &mut server).is_empty());
    frontend.input(Input::Alt(b'a')).unwrap();
    frontend.input(Input::Bytes(b"draft".to_vec())).unwrap();
    let mut view = sync(&mut frontend, &mut server).pop().unwrap();
    assert_eq!(view["open"], true);
    assert_eq!(view["sessions"][0]["input"], "draft");

    let attachment = json!(["registers", "pane 1", "Screen", "rax 0x1234", false]);
    view["sessions"][0]["basket"] = json!([attachment]);
    view["sessions"][0]["preview"] = json!([attachment, 999]);
    view["focus"] = json!("controls");
    view["keyboard"] = json!([9, 0]);
    server
        .send_response(Response::Agent {
            kind: 0,
            payload: serde_json::to_vec(&view).unwrap(),
        })
        .unwrap();
    server.flush().unwrap();
    frontend.poll().unwrap();
    assert!(sync(&mut frontend, &mut server).is_empty());
    assert_eq!(frontend.ui.agent.active().context.basket.len(), 1);
    let mut output = Vec::new();
    frontend.draw_agent(&mut output).unwrap();
    assert_eq!(
        sync(&mut frontend, &mut server)[0]["sessions"][0]["preview"][1],
        0
    );
    frontend.draw_agent(&mut output).unwrap();
    assert!(!frontend.ui.agent.view_dirty);
    frontend.input(Input::Escape).unwrap();
    assert!(sync(&mut frontend, &mut server)[0]["sessions"][0]["preview"].is_null());
    frontend.input(Input::Bytes(b"\r".to_vec())).unwrap();
    let view = sync(&mut frontend, &mut server).pop().unwrap();
    assert_eq!(view["sessions"][0]["basket"], json!([]));

    frontend.input(Input::Bytes(b"\x1b[Z".to_vec())).unwrap();
    assert!(frontend.ui.focus == Focus::Output);
    frontend.input(Input::Bytes(b"\x1b[5~".to_vec())).unwrap();
    assert_eq!(
        sync(&mut frontend, &mut server)[0]["sessions"][0]["scroll"],
        10
    );
    frontend.draw_agent(&mut output).unwrap();
    assert_eq!(
        sync(&mut frontend, &mut server)[0]["sessions"][0]["scroll"],
        0
    );
    frontend.draw_agent(&mut output).unwrap();
    assert!(!frontend.ui.agent.view_dirty);
    assert!(sync(&mut frontend, &mut server).is_empty());

    let mut models = json!([[0, {
        "messages": [["You", "earlier", null, 1]],
        "busy": false, "revision": 1, "turn": 1
    }]]);
    server
        .send_response(Response::Agent {
            kind: 1,
            payload: serde_json::to_vec(&models).unwrap(),
        })
        .unwrap();
    server.flush().unwrap();
    frontend.poll().unwrap();
    assert!(sync(&mut frontend, &mut server).is_empty());
    frontend.input(Input::Bytes(b"\x1b[Z".to_vec())).unwrap();
    frontend.input(Input::Escape).unwrap();
    assert_eq!(
        sync(&mut frontend, &mut server)[0]["sessions"][0]["rewind_target"],
        1
    );
    frontend.input(Input::Bytes(b"\r".to_vec())).unwrap();
    sync(&mut frontend, &mut server);
    models[0][1]["rewind_restore"] = json!([1, "earlier", [attachment]]);
    server
        .send_response(Response::Agent {
            kind: 1,
            payload: serde_json::to_vec(&models).unwrap(),
        })
        .unwrap();
    server.flush().unwrap();
    frontend.poll().unwrap();
    let view = sync(&mut frontend, &mut server).pop().unwrap();
    assert_eq!(view["sessions"][0]["input"], "earlier");
    assert_eq!(view["sessions"][0]["basket"], json!([attachment]));
    assert_eq!(view["sessions"][0]["rewind_applied"], 1);

    snapshot.controller = false;
    server
        .send_response(Response::State(snapshot.clone(), Vec::new()))
        .unwrap();
    server.flush().unwrap();
    frontend.poll().unwrap();
    frontend.input(Input::Bytes(b" edited".to_vec())).unwrap();
    assert!(sync(&mut frontend, &mut server).is_empty());
    assert!(frontend.ui.agent.view_dirty);
    snapshot.controller = true;
    server
        .send_response(Response::State(snapshot, Vec::new()))
        .unwrap();
    server.flush().unwrap();
    frontend.poll().unwrap();
    assert_eq!(
        sync(&mut frontend, &mut server)[0]["sessions"][0]["input"],
        "earlier edited"
    );
    frontend.input(Input::Bytes(b"\x1b[C".to_vec())).unwrap();
    assert!(sync(&mut frontend, &mut server).is_empty());
    assert!(!frontend.ui.agent.view_dirty);
    drop(frontend);
    drop(server);
    drop(listener);
    std::fs::remove_file(path).unwrap();
}

#[cfg(feature = "agent")]
#[test]
fn agent_markdown_renders_and_selects_the_visible_text() {
    use crate::core::Core;
    use std::os::unix::net::UnixListener;
    let path = std::path::PathBuf::from(format!("target/tui-markdown-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&path).unwrap();
    let client = Client::connect(&path).unwrap();
    let _peer = listener.accept().unwrap();
    let size = WindowSize {
        num_cols: 140,
        num_lines: 42,
        cell_width: 0,
        cell_height: 0,
    };
    let mut frontend = Frontend::new(client, size, crate::config::Config::default()).unwrap();
    frontend.ui.agent.open = true;
    let source = "## 标题\n\n**bold** normal _italic_ and `x*y`\n\n> quoted\n\n- first\n- second\n\n```gdb\n  x/4gx $sp\n\n  0x123\n```\n\n[docs](https://example.com)";
    frontend
        .ui
        .agent
        .active_mut()
        .model
        .messages
        .push(crate::agent::Message {
            turn: 1,
            role: "Agent".into(),
            text: source.into(),
            id: None,
        });
    frontend
        .ui
        .agent
        .wait_layout(usize::from(frontend.editor_rect().cols));
    let mut bytes = Vec::new();
    frontend.draw_agent(&mut bytes).unwrap();
    let mut host = Core::new(size, 0xcccccc, 0x202020);
    let mut parser = crate::vt::Parser::new();
    parser.advance(&mut host, &bytes);
    let line = |needle: &str| {
        let session = frontend.ui.agent.active();
        session
            .lines
            .iter()
            .position(|line| match line {
                AgentLine::Text { message, range } => session.message_text(*message as usize)
                    [range.start as usize..range.end as usize]
                    .contains(needle),
                _ => false,
            })
            .unwrap()
    };
    let heading = line("标题");
    let bold = line("bold");
    let code = line("  x/4gx");
    let row = |line: usize| usize::from(frontend.agent_chat.y) + line - frontend.agent_first;
    let x = usize::from(frontend.agent_chat.x);
    assert_eq!(host.row(row(heading)).cells[x].c, '标');
    assert!(host.row(row(heading)).cells[x].flags.contains(Flags::BOLD));
    assert!(host.row(row(bold)).cells[x].flags.contains(Flags::BOLD));
    assert!(!host.row(row(bold)).cells[x + 5].flags.contains(Flags::BOLD));
    assert_eq!(host.row(row(code)).cells[x + 2].c, 'x');
    assert_eq!(
        host.color(host.row(row(code)).cells[x + 2].bg),
        Rgb {
            r: 43,
            g: 48,
            b: 45
        }
    );
    assert!(
        matches!(&frontend.ui.agent.active().lines[code + 1], AgentLine::Text { range, .. } if range.is_empty())
    );
    let session = frontend.ui.agent.active_mut();
    let start = session.point(bold, 0).unwrap();
    let end = session.point(bold, 4).unwrap();
    session.begin_selection(start, false);
    session.extend_selection(end);
    assert_eq!(session.selected_text().as_deref(), Some("bold"));
    assert_eq!(session.model.messages[0].text, source);
    bytes.clear();
    frontend.draw_agent(&mut bytes).unwrap();
    parser.advance(&mut host, &bytes);
    let y = usize::from(frontend.agent_chat.y) + bold - frontend.agent_first;
    assert_eq!(
        host.color(host.row(y).cells[x].bg),
        Rgb {
            r: 82,
            g: 99,
            b: 76
        }
    );
    std::fs::remove_file(path).unwrap();
}

#[cfg(feature = "agent")]
#[test]
fn agent_tabs_keep_drafts_and_drag_selection_copies_across_messages() {
    use std::os::unix::net::UnixListener;
    let path =
        std::path::PathBuf::from(format!("target/tui-agent-tabs-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&path).unwrap();
    let client = Client::connect(&path).unwrap();
    let mut peer = crate::wire::Peer::new(listener.accept().unwrap().0).unwrap();
    let mut frontend = Frontend::new(
        client,
        WindowSize {
            num_cols: 110,
            num_lines: 30,
            cell_width: 0,
            cell_height: 0,
        },
        crate::config::Config::default(),
    )
    .unwrap();
    frontend.input(Input::Alt(b'a')).unwrap();
    let model = &mut frontend.ui.agent.active_mut().model;
    model.insert("first draft").unwrap();
    model.messages.extend([
        crate::agent::Message {
            role: "You".into(),
            text: "hello 中 world".into(),
            id: None,
            turn: 1,
        },
        crate::agent::Message {
            role: "Agent".into(),
            text: "agent reply".into(),
            id: None,
            turn: 1,
        },
    ]);
    frontend
        .ui
        .agent
        .wait_layout(usize::from(frontend.editor_rect().cols));
    let mut rendered = Vec::new();
    frontend.draw_agent(&mut rendered).unwrap();
    let rendered = String::from_utf8(rendered).unwrap();
    assert!(rendered.contains(crate::ui::REWIND_LABEL));
    assert!(!rendered.contains("↶"));
    let plus = frontend
        .agent_hits
        .iter()
        .find(|(_, action)| *action == AgentAction::NewSession)
        .unwrap()
        .0;
    let panel_close = frontend
        .agent_hits
        .iter()
        .find(|(_, action)| *action == AgentAction::ClosePanel)
        .unwrap()
        .0;
    let tabs_end = frontend
        .agent_hits
        .iter()
        .filter_map(|(rect, action)| {
            matches!(action, AgentAction::Session(_)).then_some(rect.x + rect.cols)
        })
        .max()
        .unwrap();
    assert_eq!(plus.x, tabs_end);
    assert_eq!(plus.cols, 3);
    assert_eq!(panel_close.cols, 3);
    assert_eq!(panel_close.x + panel_close.cols, frontend.size.num_cols);
    assert!(plus.x + plus.cols <= panel_close.x);
    let rewind_hits: Vec<_> = frontend
        .agent_hits
        .iter()
        .filter(|(_, action)| *action == AgentAction::Rewind(0))
        .collect();
    assert_eq!(rewind_hits.len(), 1);
    let rewind_button = rewind_hits[0].0;
    assert!(
        frontend
            .agent_chat
            .contains(rewind_button.x, rewind_button.y)
    );
    assert!(
        !frontend
            .editor_rect()
            .contains(rewind_button.x, rewind_button.y)
    );
    assert_eq!(
        frontend.editor_layout().lines,
        frontend
            .ui
            .agent
            .active()
            .editor_layout(usize::from(frontend.editor_rect().cols), 3)
            .lines
    );
    let mut host = crate::core::Core::new(frontend.size, 0xcccccc, 0x202020);
    let mut parser = crate::vt::Parser::new();
    let tab_buttons: Vec<_> = frontend
        .agent_hits
        .iter()
        .filter(|(_, action)| {
            matches!(
                action,
                AgentAction::Session(_)
                    | AgentAction::CloseSession(_)
                    | AgentAction::NewSession
                    | AgentAction::ClosePanel
            )
        })
        .copied()
        .collect();
    for (rect, action) in tab_buttons {
        for hovered in [true, false] {
            frontend.pointer = hovered.then_some((rect.x, rect.y));
            let mut bytes = Vec::new();
            frontend.draw_agent(&mut bytes).unwrap();
            parser.advance(&mut host, &bytes);
            let cell = host.row(usize::from(rect.y)).cells[usize::from(rect.x + 1)];
            let active = matches!(
                action,
                AgentAction::Session(0) | AgentAction::CloseSession(0)
            );
            assert_eq!(
                cell.flags.contains(Flags::UNDERLINE),
                active || hovered && !matches!(action, AgentAction::Session(_))
            );
            if hovered && !matches!(action, AgentAction::Session(_)) {
                let rgb = if action == AgentAction::NewSession {
                    crate::vt::Rgb {
                        r: 181,
                        g: 210,
                        b: 162,
                    }
                } else {
                    crate::vt::Rgb {
                        r: 232,
                        g: 117,
                        b: 117,
                    }
                };
                assert_eq!(cell.fg, Color::Spec(rgb));
                assert_eq!(
                    host.row(usize::from(rect.y))
                        .extra(usize::from(rect.x + 1))
                        .and_then(|extra| extra.underline_color),
                    Some(Color::Spec(rgb))
                );
            }
            let bg = match action {
                AgentAction::Session(_) | AgentAction::CloseSession(_) if active => 32,
                AgentAction::Session(_) | AgentAction::CloseSession(_) => 28,
                _ => 25,
            };
            assert_eq!(
                cell.bg,
                Color::Spec(crate::vt::Rgb {
                    r: bg,
                    g: bg,
                    b: bg
                })
            );
        }
    }
    frontend.ui.focus = Focus::Controls;
    frontend.ui.agent.keyboard = AgentAction::NewSession;
    frontend.pointer = None;
    let mut bytes = Vec::new();
    frontend.draw_agent(&mut bytes).unwrap();
    parser.advance(&mut host, &bytes);
    let focused_plus = host.row(usize::from(plus.y)).cells[usize::from(plus.x + 1)];
    assert!(focused_plus.flags.contains(Flags::UNDERLINE));
    assert_eq!(
        focused_plus.bg,
        Color::Spec(crate::vt::Rgb {
            r: 25,
            g: 25,
            b: 25
        })
    );
    frontend.ui.focus = Focus::Editor;
    for hovered in [true, false] {
        frontend.pointer = hovered.then_some((rewind_button.x, rewind_button.y));
        let mut bytes = Vec::new();
        frontend.draw_agent(&mut bytes).unwrap();
        parser.advance(&mut host, &bytes);
        let cell = host.row(usize::from(rewind_button.y)).cells[usize::from(rewind_button.x + 1)];
        assert!(!cell.flags.contains(Flags::UNDERLINE));
        assert_eq!(
            cell.bg,
            Color::Spec(crate::vt::Rgb {
                r: if hovered { 52 } else { 48 },
                g: if hovered { 60 } else { 48 },
                b: if hovered { 52 } else { 48 }
            })
        );
    }
    let original = frontend.ui.agent.active().model.messages[0].text.clone();
    frontend.ui.agent.active_mut().model.messages[0].text = "wrapped 中🙂 text ".repeat(200);
    frontend.ui.agent.active_mut().columns = 0;
    frontend.draw_agent(&mut Vec::new()).unwrap();
    assert!(frontend.agent_first > 0);
    let visible_rewind: Vec<_> = frontend
        .agent_hits
        .iter()
        .filter(|(_, action)| *action == AgentAction::Rewind(0))
        .collect();
    assert_eq!(visible_rewind.len(), 1);
    let last_user_line = frontend
        .ui
        .agent
        .active()
        .lines
        .iter()
        .rposition(|line| matches!(line, AgentLine::Text { message: 0, .. }))
        .unwrap();
    assert!(last_user_line > frontend.agent_first);
    assert_eq!(
        visible_rewind[0].0.y,
        frontend.agent_chat.y + (last_user_line - frontend.agent_first) as u16
    );
    let session = frontend.ui.agent.active();
    for line in &session.lines {
        if let AgentLine::Text { message: 0, range } = line {
            assert!(
                crate::width::text_width(
                    &session.model.messages[0].text[range.start as usize..range.end as usize]
                ) <= usize::from(
                    frontend.agent_chat.cols.saturating_sub(
                        1 + crate::width::text_width(crate::ui::REWIND_LABEL) as u16
                    )
                )
            );
        }
    }
    frontend.ui.agent.active_mut().model.messages[0].text = original;
    frontend.ui.agent.active_mut().columns = 0;
    frontend.draw_agent(&mut Vec::new()).unwrap();
    frontend
        .agent_mouse(Mouse {
            code: 0,
            column: rewind_button.x,
            row: rewind_button.y,
            release: false,
        })
        .unwrap();
    assert_eq!(frontend.ui.agent.active().rewind_target, Some(1));
    assert_eq!(frontend.ui.agent.active().model.input, "first draft");
    frontend.ui.agent_rewind_key(0);
    frontend.input(Input::Escape).unwrap();
    frontend.input(Input::Escape).unwrap();
    assert_eq!(frontend.ui.agent.active().rewind_target, Some(1));
    assert_eq!(frontend.ui.agent.active().model.input, "first draft");
    assert_eq!(frontend.ui.agent.active().model.messages.len(), 2);
    let mut rewind = Vec::new();
    frontend.draw_agent(&mut rewind).unwrap();
    let rewind = String::from_utf8(rewind).unwrap();
    assert!(rewind.contains("48;2;57;69;53m"));
    assert!(rewind.contains("Enter confirm"));
    frontend.input(Input::Bytes(b"X".to_vec())).unwrap();
    assert!(frontend.ui.agent.active().rewind_target.is_none());
    assert_eq!(frontend.ui.agent.active().model.input, "first draftX");
    frontend.ui.agent.active_mut().model.key(0xff08, false);
    let chat = frontend.agent_chat;
    for mouse in [
        Mouse {
            code: 0,
            column: chat.x + 7,
            row: chat.y,
            release: false,
        },
        Mouse {
            code: 32,
            column: chat.x + 5,
            row: chat.y + 2,
            release: false,
        },
        Mouse {
            code: 0,
            column: chat.x + 5,
            row: chat.y + 2,
            release: true,
        },
    ] {
        frontend.agent_mouse(mouse).unwrap();
    }
    assert_eq!(
        frontend.ui.agent.active().selected_text().as_deref(),
        Some("中 world\n\nagent")
    );
    let mut selected = Vec::new();
    frontend.draw_agent(&mut selected).unwrap();
    assert!(
        String::from_utf8(selected)
            .unwrap()
            .contains("48;2;82;99;76m中 world")
    );
    frontend.input(Input::Bytes(vec![3])).unwrap();
    assert!(
        matches!(frontend.clipboard.as_ref().map(|copy| &copy.reader), Some(ClipboardSource::Memory(bytes)) if bytes == "中 world\n\nagent".as_bytes())
    );
    let editor = frontend.editor_rect();
    frontend
        .agent_mouse(Mouse {
            code: 2,
            column: editor.x,
            row: editor.y,
            release: false,
        })
        .unwrap();
    assert!(frontend.ui.agent.active().context.menu);
    frontend.ui.agent.active_mut().context.menu = false;
    frontend
        .agent_mouse(Mouse {
            code: 0,
            column: chat.x,
            row: chat.y + 4,
            release: false,
        })
        .unwrap();
    assert!(frontend.ui.agent.active().selected_text().is_none());
    frontend.input(Input::Bytes(vec![20])).unwrap();
    assert_eq!(frontend.ui.agent.sessions.len(), 1);
    frontend.activate_agent(AgentAction::NewSession).unwrap();
    assert_eq!(frontend.ui.agent.sessions.len(), 2);
    assert!(frontend.ui.agent.active().model.messages.is_empty());
    frontend
        .input(Input::Bytes(b"second draft".to_vec()))
        .unwrap();
    frontend.input(Input::Bytes(b"\x1b[5;5~".to_vec())).unwrap();
    assert_eq!(frontend.ui.agent.current, 1);
    frontend.activate_agent(AgentAction::Session(0)).unwrap();
    assert_eq!(frontend.ui.agent.current, 0);
    assert_eq!(frontend.ui.agent.active().model.input, "first draft");
    frontend.input(Input::Bytes(vec![23])).unwrap();
    assert_eq!(frontend.ui.agent.sessions.len(), 2);
    frontend
        .activate_agent(AgentAction::CloseSession(0))
        .unwrap();
    assert_eq!(frontend.ui.agent.sessions.len(), 1);
    assert_eq!(frontend.ui.agent.active().model.input, "second draft");
    frontend
        .activate_agent(AgentAction::CloseSession(0))
        .unwrap();
    assert_eq!(frontend.ui.agent.sessions.len(), 1);
    assert!(frontend.ui.agent.active().model.input.is_empty());
    frontend.input(Input::PasteStart).unwrap();
    frontend.input(Input::PasteData(vec![0xe4])).unwrap();
    frontend.activate_agent(AgentAction::NewSession).unwrap();
    frontend.input(Input::PasteData(vec![0xb8, 0xad])).unwrap();
    frontend.input(Input::PasteEnd).unwrap();
    assert_eq!(frontend.ui.agent.sessions[0].model.input, "中");
    assert!(frontend.ui.agent.active().model.input.is_empty());
    frontend.activate_agent(AgentAction::Session(0)).unwrap();
    frontend.input(Input::PasteStart).unwrap();
    frontend
        .activate_agent(AgentAction::CloseSession(0))
        .unwrap();
    frontend
        .input(Input::PasteData(b"discard".to_vec()))
        .unwrap();
    frontend.input(Input::PasteEnd).unwrap();
    assert!(frontend.ui.agent.active().model.input.is_empty());
    assert!(
        frontend
            .ui
            .status
            .as_ref()
            .unwrap()
            .0
            .contains("Conversation was closed")
    );
    frontend.ui.agent.active_mut().model.input = "preserved draft".into();
    frontend.ui.agent.active_mut().model.busy = true;
    let session = frontend.ui.agent.active().id;
    let conversations = frontend.ui.agent.sessions.len();
    frontend.draw_agent(&mut Vec::new()).unwrap();
    let close = frontend
        .agent_hits
        .iter()
        .find(|(_, action)| *action == AgentAction::ClosePanel)
        .unwrap()
        .0;
    frontend
        .agent_mouse(Mouse {
            code: 0,
            column: close.x,
            row: close.y,
            release: false,
        })
        .unwrap();
    assert!(!frontend.ui.agent.open);
    assert_eq!(frontend.ui.agent.sessions.len(), conversations);
    assert_eq!(frontend.ui.agent.active().id, session);
    assert_eq!(frontend.ui.agent.active().model.input, "preserved draft");
    assert!(frontend.ui.agent.active().model.busy);
    frontend.ui.agent.active_mut().model.busy = false;
    assert!(peer.next_action().unwrap().is_none());
    std::fs::remove_file(path).unwrap();
}

#[cfg(all(feature = "agent", feature = "files", feature = "monitor"))]
#[test]
fn global_shortcuts_and_layered_escape_keep_focus_and_drafts() {
    use std::{fs, os::unix::net::UnixListener};
    let path = std::path::PathBuf::from(format!(
        "target/tui-global-focus-{}.sock",
        std::process::id()
    ));
    let listener = UnixListener::bind(&path).unwrap();
    let client = Client::connect(&path).unwrap();
    let mut peer = crate::wire::Peer::new(listener.accept().unwrap().0).unwrap();
    let snapshot = Snapshot {
        name: "focus".into(),
        controller: true,
        focused: 1,
        active_tab: 1,
        tabs: vec![crate::workspace::TabView {
            id: 1,
            title: "test".into(),
        }],
        panes: vec![PaneView {
            process: Default::default(),
            id: 1,
            rect: Rect {
                x: 0,
                y: 1,
                cols: 80,
                rows: 23,
            },
            title: "test".into(),
            cwd: Some(std::env::current_dir().unwrap()),
            blocks: Vec::new(),
            command_blocks: Vec::new(),
            input_generation: 0,
            shell_prompt: None,
        }],
    };
    peer.send_state(&snapshot, &[], &Default::default())
        .unwrap();
    peer.flush().unwrap();
    let mut frontend = Frontend::new(
        client,
        WindowSize {
            num_cols: 80,
            num_lines: 24,
            cell_width: 0,
            cell_height: 0,
        },
        crate::config::Config::default(),
    )
    .unwrap();
    frontend.poll().unwrap();
    frontend.input(Input::Alt(b'a')).unwrap();
    frontend
        .ui
        .agent
        .active_mut()
        .model
        .insert("keep draft")
        .unwrap();
    while peer.next_action().unwrap().is_some() {}
    frontend.input(Input::Alt(b'v')).unwrap();
    assert!(matches!(
        peer.next_action().unwrap(),
        Some(Action::Split {
            axis: Axis::LeftRight,
            ..
        })
    ));
    assert!(frontend.ui.focus == Focus::Terminal);
    assert_eq!(frontend.ui.agent.active().model.input, "keep draft");
    frontend.ui.focus = Focus::Editor;
    frontend.input(Input::Alt(b'f')).unwrap();
    assert!(frontend.ui.files_open);
    assert!(frontend.ui.focus == Focus::Files);
    let files = frontend.ui.files.as_mut().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let entry = loop {
        files.poll();
        if let Some(index) = files.entries().iter().position(|entry| !entry.is_dir) {
            break index;
        }
        assert!(
            Instant::now() < deadline,
            "file listing did not become available"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    files.move_selection(entry as isize - files.selection() as isize);
    assert!(files.focus_preview());
    frontend.input(Input::Escape).unwrap();
    assert!(frontend.ui.files_open);
    assert!(frontend.ui.files.as_ref().unwrap().focus() == crate::files::FileFocus::List);
    frontend.input(Input::Alt(b'm')).unwrap();
    assert!(frontend.ui.resources.open);
    assert!(!frontend.ui.files_open);
    frontend.input(Input::Bytes(b"\x1b[D".to_vec())).unwrap();
    assert!(frontend.ui.resources.open);
    frontend.input(Input::Escape).unwrap();
    assert!(!frontend.ui.resources.open);
    assert!(frontend.ui.focus == Focus::Terminal);
    drop(frontend);
    drop(peer);
    drop(listener);
    fs::remove_file(path).unwrap();
}

#[cfg(feature = "agent")]
#[test]
fn agent_draft_selection_and_captured_paste_never_reach_the_pane() {
    use std::{fs, os::unix::net::UnixListener};
    let path = std::path::PathBuf::from(format!(
        "target/tui-agent-editor-{}.sock",
        std::process::id()
    ));
    let listener = UnixListener::bind(&path).unwrap();
    let client = Client::connect(&path).unwrap();
    let mut peer = crate::wire::Peer::new(listener.accept().unwrap().0).unwrap();
    let mut frontend = Frontend::new(
        client,
        WindowSize {
            num_cols: 100,
            num_lines: 30,
            cell_width: 0,
            cell_height: 0,
        },
        crate::config::Config::default(),
    )
    .unwrap();
    frontend.input(Input::Alt(b'a')).unwrap();
    frontend
        .input(Input::Bytes("alpha 中".as_bytes().to_vec()))
        .unwrap();
    frontend.input(Input::Bytes(vec![1])).unwrap();
    frontend.input(Input::Bytes(vec![3])).unwrap();
    assert!(
        matches!(frontend.clipboard.as_ref().map(|copy| &copy.reader), Some(ClipboardSource::Memory(bytes)) if bytes == "alpha 中".as_bytes())
    );
    frontend.input(Input::Bytes(vec![24])).unwrap();
    assert_eq!(frontend.ui.agent.active().model.input, "alpha 中");
    frontend.clipboard = None;
    frontend
        .input(Input::Bytes(b"replacement".to_vec()))
        .unwrap();
    assert_eq!(frontend.ui.agent.active().model.input, "replacement");
    frontend.input(Input::Bytes(vec![3])).unwrap();
    assert!(frontend.ui.agent.active().model.input.is_empty());
    frontend.input(Input::PasteStart).unwrap();
    frontend
        .input(Input::PasteData(b"draft ".to_vec()))
        .unwrap();
    frontend.ui.focus = Focus::Output;
    frontend.input(Input::PasteData(b"kept".to_vec())).unwrap();
    frontend.input(Input::PasteEnd).unwrap();
    frontend.input(Input::Bytes(b"ignored".to_vec())).unwrap();
    frontend.input(Input::Bytes(vec![3])).unwrap();
    assert_eq!(frontend.ui.agent.active().model.input, "draft kept");
    assert!(peer.next_action().unwrap().is_none());
    frontend.ui.focus = Focus::Editor;
    frontend.ui.agent.active_mut().model.clear_input();
    frontend
        .input(Input::Bytes(b"batch\t\x03".to_vec()))
        .unwrap();
    assert!(frontend.ui.focus == Focus::Output);
    assert_eq!(frontend.ui.agent.active().model.input, "batch");
    frontend.ui.focus = Focus::Editor;
    frontend
        .input(Input::Bytes(b"\x01\x7fafter\r".to_vec()))
        .unwrap();
    assert_eq!(frontend.ui.agent.active().model.input, "after");
    assert!(frontend.running);
    assert!(
        frontend
            .ui
            .status
            .as_ref()
            .unwrap()
            .0
            .contains("No workspace")
    );
    frontend.ui.agent.active_mut().model.clear_input();
    frontend
        .ui
        .agent
        .active_mut()
        .model
        .insert(
            &(0..12)
                .map(|i| format!("draft line {i:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
    frontend.ui.agent.active_mut().editor_scroll = Some(6);
    frontend.draw_agent(&mut Vec::new()).unwrap();
    let rect = frontend.editor_rect();
    frontend
        .agent_mouse(Mouse {
            code: 0,
            column: rect.x,
            row: rect.y,
            release: false,
        })
        .unwrap();
    frontend
        .agent_mouse(Mouse {
            code: 32,
            column: rect.x + 10,
            row: rect.y,
            release: false,
        })
        .unwrap();
    frontend
        .agent_mouse(Mouse {
            code: 0,
            column: rect.x + 10,
            row: rect.y,
            release: true,
        })
        .unwrap();
    let selection = frontend.ui.agent.active().model.selection().unwrap();
    assert_eq!(
        &frontend.ui.agent.active().model.input[selection],
        "draft line"
    );
    frontend.ui.agent.active_mut().context.preview = Some(crate::ui::ContextPreview::new(
        crate::ui::ContextItem {
            label: "Screen".into(),
            source: "test pane".into(),
            kind: "Screen".into(),
            snapshot: crate::context::Snapshot {
                text: "snapshot".into(),
                incomplete: false,
            },
        },
        0,
    ));
    let mut output = Vec::new();
    frontend.draw_agent(&mut output).unwrap();
    assert!(
        frontend.agent_hits.iter().all(|(_, action)| matches!(
            action,
            AgentAction::ClosePreview | AgentAction::CopyPreview
        ))
    );
    let mut core = crate::core::Core::new(frontend.size, 0xcccccc, 0x202020);
    let mut parser = crate::vt::Parser::new();
    for row in 1..frontend.size.num_lines {
        parser.advance(
            &mut core,
            format!(
                "\x1b[{};1Hl\x1b[{};{}Hr",
                row + 1,
                row + 1,
                frontend.size.num_cols
            )
            .as_bytes(),
        );
    }
    parser.advance(&mut core, &output);
    let copy = frontend
        .agent_hits
        .iter()
        .find(|(_, action)| *action == AgentAction::CopyPreview)
        .unwrap()
        .0;
    assert_eq!(copy.cols, 3);
    assert_eq!(
        core.row(usize::from(copy.y)).cells[usize::from(copy.x + 1)].c,
        '⧉'
    );
    for row in 1..usize::from(frontend.size.num_lines - 1) {
        assert_eq!(core.row(row).cells[0].c, ' ');
        assert_eq!(
            core.row(row).cells[usize::from(frontend.size.num_cols - 1)].c,
            ' '
        );
    }
    drop(frontend);
    drop(peer);
    drop(listener);
    fs::remove_file(path).unwrap();
}

#[cfg(feature = "monitor")]
#[test]
fn resource_braille_keeps_missing_history_and_sampling_gaps_empty() {
    let graph = braille_trend(
        &[(60., Some(50.)), (30., None), (0., Some(50.))],
        20,
        2,
        60.,
        100.,
    );
    assert!(graph.chunks_exact(20).any(|line| line[0] != 0));
    assert!(graph.chunks_exact(20).any(|line| line[19] != 0));
    assert!(
        graph
            .chunks_exact(20)
            .all(|line| line[1..19].iter().all(|&bits| bits == 0))
    );
    assert!(
        braille_trend(&[], 20, 2, 60., 100.)
            .iter()
            .all(|&bits| bits == 0)
    );
}

#[test]
fn shortening_selection_preserves_text_and_clears_unselected_padding() {
    use crate::{core::Core, screen::Update};
    let size = WindowSize {
        num_cols: 40,
        num_lines: 3,
        cell_width: 0,
        cell_height: 0,
    };
    let mut core = Core::new(size, 0xcccccc, 0x202020);
    crate::vt::Parser::new().advance(
        &mut core,
        "hi\r\n\x1b[44mcolored\x1b[0m\r\n中a\u{301}".as_bytes(),
    );
    core.selection_start(
        Point { line: 0, column: 1 },
        Side::Left,
        SelectionType::Simple,
    );
    let pane = PaneView {
        process: Default::default(),
        id: 1,
        rect: Rect {
            x: 0,
            y: 0,
            cols: 40,
            rows: 3,
        },
        title: String::new(),
        cwd: None,
        blocks: Vec::new(),
        command_blocks: Vec::new(),
        input_generation: 0,
        shell_prompt: None,
    };
    let mut screen = Screen::default();
    let mut host = Core::new(size, 0xcccccc, 0x202020);
    let mut parser = crate::vt::Parser::new();
    for end in [
        Some(Point { line: 1, column: 9 }),
        Some(Point { line: 0, column: 3 }),
        None,
    ] {
        if let Some(end) = end {
            core.selection_update(end, Side::Right);
        } else {
            core.clear_selection();
        }
        screen.apply(Update::capture(&core, false)).unwrap();
        let mut bytes = Vec::new();
        for row in screen.dirty_rows() {
            draw_row(&mut bytes, &screen, &pane, row, 40, false).unwrap();
        }
        parser.advance(&mut host, &bytes);
        for row in 0..3 {
            for column in 0..40 {
                let original = core.row(row).cells[column];
                let rendered = host.row(row).cells[column];
                let selected = core.selection_range().is_some_and(|range| {
                    range.contains(Point {
                        line: row as i32,
                        column,
                    })
                });
                let background = if selected {
                    Rgb {
                        r: 55,
                        g: 55,
                        b: 55,
                    }
                } else {
                    core.color(original.bg)
                };
                assert_eq!(rendered.c, original.c);
                assert_eq!(host.color(rendered.bg), background);
            }
        }
        assert_eq!(
            host.row(2).extra(2).map(|extra| &extra.zerowidth),
            core.row(2).extra(2).map(|extra| &extra.zerowidth),
        );
        core.reset_damage();
        screen.reset_damage();
    }
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
        .stdin(Stdio::from(input_slave))
        .stdout(Stdio::from(output_slave))
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
    let deadline = Instant::now() + Duration::from_millis(500);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
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
    assert!(matches!(
        server.next_action().unwrap(),
        Some(Action::Detach)
    ));
    drop(input_master);
}

#[test]
fn metadata_updates_remove_hover_from_unchanged_output_rows() {
    use crate::{core::Core, screen::Update, wire::Peer, workspace::Snapshot};
    use std::{fs, os::unix::net::UnixListener};
    let socket = std::path::PathBuf::from(format!("target/tui-hover-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&socket).unwrap();
    let client = Client::connect(&socket).unwrap();
    let mut server = Peer::new(listener.accept().unwrap().0).unwrap();
    let size = WindowSize {
        num_cols: 7,
        num_lines: 4,
        cell_width: 0,
        cell_height: 0,
    };
    let mut core = Core::new(
        WindowSize {
            num_cols: 7,
            num_lines: 3,
            ..size
        },
        0xcccccc,
        0x202020,
    );
    let mut parser = crate::vt::Parser::new();
    parser.advance(
        &mut core,
        b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07top\r\nABCDEFG",
    );
    let mut snapshot = Snapshot {
        name: "hover".into(),
        controller: true,
        focused: 1,
        active_tab: 1,
        tabs: Vec::new(),
        panes: vec![PaneView {
            process: Default::default(),
            id: 1,
            rect: Rect {
                x: 0,
                y: 1,
                cols: 7,
                rows: 3,
            },
            title: String::new(),
            cwd: None,
            blocks: core.visible_blocks(),
            command_blocks: core.recent_command_blocks(),
            input_generation: 0,
            shell_prompt: None,
        }],
    };
    server
        .send_state(
            &snapshot,
            &[(1, &Update::capture(&core, true))],
            &Default::default(),
        )
        .unwrap();
    server.flush().unwrap();
    let mut frontend = Frontend::new(client, size, crate::config::Config::default()).unwrap();
    frontend.pointer = Some((6, 2));
    frontend.poll().unwrap();
    let mut host = Core::new(size, 0xcccccc, 0x202020);
    let mut host_parser = crate::vt::Parser::new();
    let mut bytes = Vec::new();
    frontend.draw(&mut bytes).unwrap();
    host_parser.advance(&mut host, &bytes);
    assert_eq!(host.row(1).cells[0].c, 't');
    assert_eq!(
        host.row(1).cells[3].bg,
        Color::Spec(Rgb {
            r: 38,
            g: 38,
            b: 38
        })
    );
    frontend
        .mouse(Mouse {
            column: 0,
            row: 0,
            code: 35,
            release: false,
        })
        .unwrap();
    assert!(frontend.hovered_block().is_none());
    bytes.clear();
    frontend.draw(&mut bytes).unwrap();
    host_parser.advance(&mut host, &bytes);
    assert_eq!(
        host.row(1).cells[3].bg,
        Color::Spec(Rgb {
            r: 32,
            g: 32,
            b: 32
        })
    );
    frontend
        .mouse(Mouse {
            column: 6,
            row: 2,
            code: 35,
            release: false,
        })
        .unwrap();
    assert!(frontend.hovered_block().is_some());
    core.reset_damage();
    parser.advance(&mut core, b"\x1b[2;7H");
    snapshot.panes[0].blocks = core.visible_blocks();
    server
        .send_state(
            &snapshot,
            &[(1, &Update::capture(&core, false))],
            &Default::default(),
        )
        .unwrap();
    server.flush().unwrap();
    frontend.poll().unwrap();
    assert!(frontend.hovered_block().is_none());
    bytes.clear();
    frontend.draw(&mut bytes).unwrap();
    host_parser.advance(&mut host, &bytes);
    assert_eq!(
        host.row(1).cells[3].bg,
        Color::Spec(Rgb {
            r: 32,
            g: 32,
            b: 32
        })
    );
    core.reset_damage();
    parser.advance(&mut core, b"\x1b]133;D;1\x07");
    let update = Update::capture(&core, false);
    assert!(update.rows.is_empty());
    snapshot.panes[0].blocks = core.visible_blocks();
    server
        .send_state(&snapshot, &[(1, &update)], &Default::default())
        .unwrap();
    server.flush().unwrap();
    frontend.poll().unwrap();
    assert_eq!(
        frontend.client.screens()[&1]
            .dirty_rows()
            .collect::<Vec<_>>(),
        [0]
    );
    bytes.clear();
    frontend.draw(&mut bytes).unwrap();
    host_parser.advance(&mut host, &bytes);
    assert_eq!(
        host.row(1).cells[1].fg,
        Color::Spec(Rgb {
            r: 204,
            g: 204,
            b: 204
        })
    );
    assert!(host.row(1).cells.iter().all(|cell| cell.c != '│'));
    drop(frontend);
    drop(server);
    drop(listener);
    fs::remove_file(socket).unwrap();
}

#[test]
fn retained_header_updates_titles_status_control_and_full_redraws() {
    use crate::core::Core;
    use crate::wire::{Peer, Response};
    use crate::workspace::TabView;
    use std::{fs, os::unix::net::UnixListener};
    let socket = std::path::PathBuf::from(format!("target/tui-header-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&socket).unwrap();
    let client = Client::connect(&socket).unwrap();
    let mut server = Peer::new(listener.accept().unwrap().0).unwrap();
    let size = WindowSize {
        num_cols: 100,
        num_lines: 4,
        cell_width: 0,
        cell_height: 0,
    };
    let mut snapshot = Snapshot {
        name: "header".into(),
        controller: true,
        focused: 1,
        active_tab: 1,
        tabs: vec![TabView {
            id: 1,
            title: "alpha".into(),
        }],
        panes: Vec::new(),
    };
    server
        .send_response(Response::State(snapshot.clone(), Vec::new()))
        .unwrap();
    server.flush().unwrap();
    let mut frontend = Frontend::new(client, size, crate::config::Config::default()).unwrap();
    frontend.poll().unwrap();
    let mut host = Core::new(size, 0xcccccc, 0x202020);
    let mut parser = crate::vt::Parser::new();
    let mut output = Vec::new();
    let header_text = |host: &Core| {
        host.row(0)
            .cells
            .iter()
            .filter(|cell| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
            .map(|cell| cell.c)
            .collect::<String>()
    };
    frontend.draw(&mut output).unwrap();
    parser.advance(&mut host, &output);
    assert!(header_text(&host).contains("alpha"));
    let previous = host.row(0).clone();
    output.clear();
    frontend.draw(&mut output).unwrap();
    parser.advance(&mut host, &output);
    assert_eq!(host.row(0).cells, previous.cells);

    snapshot.tabs[0].title = "beta".into();
    snapshot.controller = false;
    server
        .send_response(Response::State(snapshot, Vec::new()))
        .unwrap();
    server.flush().unwrap();
    frontend.poll().unwrap();
    output.clear();
    frontend.draw(&mut output).unwrap();
    parser.advance(&mut host, &output);
    assert!(header_text(&host).contains("beta"));
    assert!(header_text(&host).contains("Read-only"));

    frontend.message("Copied 123 bytes");
    output.clear();
    frontend.draw(&mut output).unwrap();
    parser.advance(&mut host, &output);
    assert!(header_text(&host).contains("Copied 123 bytes"));
    frontend.ui.status = None;
    output.clear();
    frontend.draw(&mut output).unwrap();
    parser.advance(&mut host, &output);
    assert!(!header_text(&host).contains("Copied"));
    let previous = host.row(0).clone();
    frontend.full = true;
    output.clear();
    frontend.draw(&mut output).unwrap();
    parser.advance(&mut host, &output);
    assert_eq!(host.row(0).cells, previous.cells);
    drop(frontend);
    drop(listener);
    fs::remove_file(socket).unwrap();
}

#[cfg(feature = "files")]
#[test]
fn file_paste_edits_only_local_query_and_enter_inserts_a_quoted_path() {
    use crate::wire::{Peer, Response};
    use crate::workspace::TabView;
    use std::{fs, os::unix::net::UnixListener};
    let directory = std::env::current_dir()
        .unwrap()
        .join(format!("target/tui-file-input-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let file = directory.join("quoted 'name 中文");
    fs::write(&file, b"preview").unwrap();
    let socket = directory
        .strip_prefix(std::env::current_dir().unwrap())
        .unwrap()
        .join("test.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let client = Client::connect(&socket).unwrap();
    let mut server = Peer::new(listener.accept().unwrap().0).unwrap();
    let size = WindowSize {
        num_cols: 80,
        num_lines: 24,
        cell_width: 0,
        cell_height: 0,
    };
    server
        .send_response(Response::State(
            Snapshot {
                name: "files".into(),
                controller: true,
                focused: 1,
                active_tab: 1,
                tabs: vec![TabView {
                    id: 1,
                    title: "shell".into(),
                }],
                panes: vec![PaneView {
                    process: Default::default(),
                    id: 1,
                    rect: Rect {
                        x: 0,
                        y: 1,
                        cols: 80,
                        rows: 23,
                    },
                    title: "shell".into(),
                    cwd: Some(directory.clone()),
                    blocks: Vec::new(),
                    command_blocks: Vec::new(),
                    input_generation: 0,
                    shell_prompt: None,
                }],
            },
            Vec::new(),
        ))
        .unwrap();
    server.flush().unwrap();
    let mut frontend = Frontend::new(client, size, crate::config::Config::default()).unwrap();
    frontend.poll().unwrap();
    frontend.ui.files = Some(crate::files::FileNav::new(directory.clone()).unwrap());
    frontend.ui.files_open = true;
    let pasted = "/hjkl\x1bq\n中文".repeat(1024);
    frontend.file_input(pasted.as_bytes(), true).unwrap();
    assert_eq!(frontend.ui.files.as_ref().unwrap().query(), pasted);
    assert!(frontend.ui.files_open);
    assert!(server.next_action().unwrap().is_none());
    frontend.ui.files.as_mut().unwrap().stop_search();
    frontend.file_input(&[0xc3], true).unwrap();
    frontend.file_input(&[0xa9], true).unwrap();
    assert_eq!(frontend.ui.files.as_ref().unwrap().query(), "é");
    frontend.file_input(b"ab\x7f", false).unwrap();
    assert_eq!(frontend.ui.files.as_ref().unwrap().query(), "éa");
    frontend.ui.files.as_mut().unwrap().stop_search();
    let deadline = Instant::now() + Duration::from_secs(5);
    while frontend.ui.files.as_ref().unwrap().searching() {
        frontend.ui.files.as_mut().unwrap().poll();
        assert!(Instant::now() < deadline, "file navigation did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        frontend.ui.files.as_ref().unwrap().selected().unwrap().path,
        file
    );
    frontend.file_input(b"\r", false).unwrap();
    assert!(!frontend.ui.files_open);
    assert!(matches!(
        server.next_action().unwrap(),
        Some(Action::Input { pane: 1, data }) if data == crate::files::shell_quote(file.as_os_str())
    ));
    assert!(server.next_action().unwrap().is_none());
    server
        .send_response(Response::State(
            frontend.client.snapshot().unwrap().clone(),
            Vec::new(),
        ))
        .unwrap();
    server.send_response(Response::Detached).unwrap();
    server.flush().unwrap();
    let events = frontend.client.poll().unwrap();
    assert!(matches!(
        events.as_slice(),
        [ClientEvent::Updated, ClientEvent::Detached]
    ));
    for event in events {
        frontend.event(event).unwrap();
    }
    assert!(!frontend.running);
    drop(frontend);
    drop(listener);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn result_rows_keep_full_width_and_hover_preserves_selection() {
    use crate::core::Core;
    use crate::screen::Update;
    let size = WindowSize {
        num_cols: 9,
        num_lines: 4,
        cell_width: 0,
        cell_height: 0,
    };
    let mut core = Core::new(size, 0xaaaaaa, 0x202020);
    let mut vt = crate::vt::Parser::new();
    vt.advance(&mut core, b"\x1b]133;A\x07 ");
    vt.advance(&mut core, "• ".as_bytes());
    vt.advance(
        &mut core,
        b"\x1b]133;B\x07cmd\r\n\x1b]133;C\x07123456789\r\n\x1b]133;D;1\x07",
    );
    core.selection_start(
        Point { line: 0, column: 0 },
        Side::Left,
        SelectionType::Simple,
    );
    core.selection_update(Point { line: 1, column: 7 }, Side::Right);
    let pane = PaneView {
        process: Default::default(),
        id: 1,
        rect: Rect {
            x: 0,
            y: 0,
            cols: 9,
            rows: 4,
        },
        title: String::new(),
        cwd: None,
        blocks: core.visible_blocks(),
        command_blocks: core.recent_command_blocks(),
        input_generation: 0,
        shell_prompt: None,
    };
    let mut screen = Screen::default();
    screen.apply(Update::capture(&core, true)).unwrap();
    let mut bytes = Vec::new();
    draw_row(&mut bytes, &screen, &pane, 0, 9, false).unwrap();
    draw_row(&mut bytes, &screen, &pane, 1, 9, true).unwrap();
    let mut host = Core::new(
        WindowSize {
            num_cols: 9,
            ..size
        },
        0xaaaaaa,
        0x202020,
    );
    crate::vt::Parser::new().advance(&mut host, &bytes);
    assert_eq!(host.row(0).cells[1].c, '•');
    assert_eq!(host.row(1).cells[0].c, '1');
    assert_eq!(host.row(1).cells[8].c, '9');
    assert_eq!(
        host.row(0).cells[1].bg,
        Color::Spec(Rgb {
            r: 32,
            g: 32,
            b: 32
        })
    );
    assert_eq!(
        host.row(0).cells[3].bg,
        Color::Spec(Rgb {
            r: 55,
            g: 55,
            b: 55
        })
    );
    assert_eq!(
        host.row(1).cells[8].bg,
        Color::Spec(Rgb {
            r: 38,
            g: 38,
            b: 38
        })
    );
}

#[test]
fn right_prompt_reaches_pane_edge_and_repaint_clears_its_old_position() {
    use crate::core::Core;
    use crate::screen::Update;
    for (right, start) in [("main", 17), ("中a\u{301}", 18)] {
        let size = WindowSize {
            num_cols: 21,
            num_lines: 3,
            cell_width: 0,
            cell_height: 0,
        };
        let mut core = Core::new(size, 0xaaaaaa, 0x202020);
        let mut parser = crate::vt::Parser::new();
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
        assert_eq!(core.selection_to_string().as_deref(), Some("cmd"));
        let pane = PaneView {
            process: Default::default(),
            id: 1,
            rect: Rect {
                x: 2,
                y: 1,
                cols: 21,
                rows: 3,
            },
            title: String::new(),
            cwd: None,
            blocks: Vec::new(),
            command_blocks: Vec::new(),
            input_generation: 0,
            shell_prompt: None,
        };
        let mut screen = Screen::default();
        screen.apply(Update::capture(&core, true)).unwrap();
        let mut bytes = Vec::new();
        draw_row(&mut bytes, &screen, &pane, 0, 28, false).unwrap();
        let mut host = Core::new(
            WindowSize {
                num_cols: 28,
                num_lines: 5,
                ..size
            },
            0xaaaaaa,
            0x202020,
        );
        let mut host_parser = crate::vt::Parser::new();
        host_parser.advance(&mut host, &bytes);
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
        screen.apply(Update::capture(&core, true)).unwrap();
        bytes.clear();
        draw_row(&mut bytes, &screen, &pane, 0, 28, false).unwrap();
        host_parser.advance(&mut host, &bytes);
        assert!(host.row(1).cells[19..23].iter().all(|cell| cell.c == ' '));
    }
}

#[test]
fn sparse_redraw_erases_old_text_and_keeps_styled_trailing_spaces() {
    use crate::core::{Cell, Core, Row};
    use crate::screen::Update;
    let size = WindowSize {
        num_cols: 40,
        num_lines: 2,
        cell_width: 0,
        cell_height: 0,
    };
    let core = Core::new(size, 0xcccccc, 0x202020);
    let mut update = Update::capture(&core, true);
    let mut cells = vec![Cell::default(); 40];
    cells[0].c = 'x';
    cells[39].bg = Color::Indexed(1);
    update.rows[0].1 = Row::from_parts(cells, false, 1, Vec::new()).unwrap();
    let mut screen = Screen::default();
    screen.apply(update).unwrap();
    let pane = PaneView {
        process: Default::default(),
        id: 1,
        rect: Rect {
            x: 0,
            y: 0,
            cols: 40,
            rows: 2,
        },
        title: String::new(),
        cwd: None,
        blocks: Vec::new(),
        command_blocks: Vec::new(),
        input_generation: 0,
        shell_prompt: None,
    };
    let mut output = Vec::new();
    draw_row(&mut output, &screen, &pane, 0, 40, false).unwrap();
    let mut host = Core::new(size, 0xcccccc, 0x202020);
    let mut parser = crate::vt::Parser::new();
    parser.advance(&mut host, b"old contents must disappear");
    parser.advance(&mut host, &output);
    assert_eq!(host.row(0).cells[0].c, 'x');
    assert!(host.row(0).cells[1..].iter().all(|cell| cell.c == ' '));
    assert_eq!(
        host.row(0).cells[39].bg,
        Color::Spec(screen.color(Color::Indexed(1)))
    );

    screen.apply(Update::capture(&core, true)).unwrap();
    output.clear();
    draw_row(&mut output, &screen, &pane, 0, 40, false).unwrap();
    assert!(
        output.len() < 80,
        "empty rows should not emit a screenful of spaces"
    );
    parser.advance(&mut host, &output);
    assert!(host.row(0).cells.iter().all(|cell| cell.c == ' '));
    assert!(
        host.row(0)
            .cells
            .iter()
            .all(|cell| cell.bg == Color::Spec(screen.color(Color::Named(NamedColor::Background))))
    );
}

#[test]
fn tab_dots_follow_active_color_with_spaces_inside_both_edges() {
    use crate::core::Core;
    let mut snapshot = Snapshot {
        name: "tabs".into(),
        controller: true,
        focused: 1,
        active_tab: 1,
        tabs: vec![
            crate::workspace::TabView {
                id: 1,
                title: "src".into(),
            },
            crate::workspace::TabView {
                id: 2,
                title: "docs".into(),
            },
        ],
        panes: Vec::new(),
    };
    let size = WindowSize {
        num_cols: 80,
        num_lines: 2,
        cell_width: 0,
        cell_height: 0,
    };
    let mut host = Core::new(size, 0xcccccc, 0x202020);
    let mut parser = crate::vt::Parser::new();
    for active in [1, 2] {
        snapshot.active_tab = active;
        let mut output = Vec::new();
        draw_tabs(&mut output, &snapshot, 80, "", None).unwrap();
        parser.advance(&mut host, &output);
        for (index, x, width) in tab_chrome(&snapshot, 80, "").0 {
            let row = &host.row(0).cells;
            let x = usize::from(x);
            assert_eq!(row[x].c, '◢');
            assert_eq!(row[x + 1].c, ' ');
            assert_eq!(row[x + 2].c, '•');
            assert_eq!(
                row[x + 2].fg,
                Color::Spec(if snapshot.tabs[index].id == active {
                    Rgb {
                        r: 175,
                        g: 196,
                        b: 96,
                    }
                } else {
                    Rgb {
                        r: 64,
                        g: 64,
                        b: 64,
                    }
                })
            );
            assert_eq!(row[x + 3].c, ' ');
            assert_eq!(
                row[x + 4].c,
                snapshot.tabs[index].title.chars().next().unwrap()
            );
            assert_eq!(row[x + usize::from(width) - 3].c, '×');
            assert_eq!(row[x + usize::from(width) - 2].c, ' ');
            assert_eq!(row[x + usize::from(width) - 1].c, '◣');
            for column in [x, x + usize::from(width) - 1] {
                assert!(!row[column].flags.contains(Flags::UNDERLINE));
                assert_eq!(
                    host.row(0)
                        .extra(column)
                        .and_then(|extra| extra.underline_color),
                    None
                );
            }
            let bg = if snapshot.tabs[index].id == active {
                32
            } else {
                28
            };
            for cell in [row[x + 4], row[x + usize::from(width) - 3]] {
                assert!(!cell.flags.contains(Flags::UNDERLINE));
                assert_eq!(
                    cell.bg,
                    Color::Spec(Rgb {
                        r: bg,
                        g: bg,
                        b: bg
                    })
                );
            }
        }
    }
    let tabs = tab_chrome(&snapshot, 80, "").0;
    let (_, first, width) = tabs[0];
    let (_, last, last_width) = *tabs.last().unwrap();
    let plus = last + last_width + 1;
    for (pointer, column) in [
        ((first + 4, 0), first + 4),
        ((first + width - 4, 0), first + width - 3),
        ((plus, 0), plus),
    ] {
        for hovered in [true, false] {
            let mut output = Vec::new();
            draw_tabs(&mut output, &snapshot, 80, "", hovered.then_some(pointer)).unwrap();
            parser.advance(&mut host, &output);
            let cell = host.row(0).cells[usize::from(column)];
            assert_eq!(
                cell.flags.contains(Flags::UNDERLINE),
                hovered && column != first + 4
            );
            let bg = if column == plus { 25 } else { 28 };
            assert_eq!(
                cell.bg,
                Color::Spec(Rgb {
                    r: bg,
                    g: bg,
                    b: bg
                })
            );
            if hovered && column == first + width - 3 {
                assert_eq!(
                    host.row(0)
                        .extra(usize::from(column))
                        .and_then(|extra| extra.underline_color),
                    Some(Color::Spec(Rgb {
                        r: 232,
                        g: 117,
                        b: 117
                    }))
                );
            }
        }
    }
}

#[test]
fn overflowing_tabs_keep_active_tab_visible_and_share_click_geometry() {
    let mut snapshot = Snapshot {
        name: "tabs".into(),
        controller: true,
        focused: 1,
        active_tab: 1,
        tabs: (1..=8)
            .map(|id| crate::workspace::TabView {
                id,
                title: format!("long terminal title {id}"),
            })
            .collect(),
        panes: Vec::new(),
    };
    for active in [1, 8, 4, 1] {
        snapshot.active_tab = active;
        let tabs = tab_chrome(&snapshot, 40, "CPU 10%  MEM 20%  DSK 30%").0;
        assert!(
            tabs.iter()
                .any(|(index, _, _)| snapshot.tabs[*index].id == active)
        );
        for (_, x, width) in tabs {
            assert!(x + width <= 40);
            assert!(width >= 9);
        }
    }
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
fn tui_resize_uses_cells_and_zero_pixel_metrics() {
    let (_master, slave) = pty();
    assert_eq!(
        terminal_size(slave.as_raw_fd()).unwrap(),
        WindowSize {
            num_cols: 80,
            num_lines: 24,
            cell_width: 0,
            cell_height: 0
        }
    );
}

#[test]
fn clipboard_stream_crosses_chunk_boundaries_without_truncation() {
    let plain = vec![b'x'; 3072 * 3 + 2];
    let mut transfer = ClipboardTransfer {
        reader: ClipboardSource::Memory(plain.clone()),
        target: b'c',
        incomplete: true,
        started: false,
        bytes: 0,
    };
    let mut output = Vec::new();
    assert!(transfer.step(&mut output, None).unwrap().is_none());
    let message = loop {
        if let Some(message) = transfer.step(&mut output, None).unwrap() {
            break message;
        }
    };
    assert!(message.contains("9218 bytes"));
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

#[test]
fn application_cursor_translation_preserves_modified_and_ordinary_input() {
    assert_eq!(
        cursor_bytes(b"\x1b[A".to_vec(), Mode::APP_CURSOR),
        b"\x1bOA"
    );
    assert_eq!(cursor_bytes(b"\x1bOF".to_vec(), Mode::NONE), b"\x1b[F");
    assert_eq!(
        cursor_bytes(b"\x1b[1;5A".to_vec(), Mode::APP_CURSOR),
        b"\x1b[1;5A"
    );
    assert_eq!(
        cursor_bytes("你好".as_bytes().to_vec(), Mode::NONE),
        "你好".as_bytes()
    );
}

#[test]
fn mouse_local_coordinates_and_release_match_inner_modes() {
    assert_eq!(
        mouse_bytes(0, 2, 3, false, Mode::SGR_MOUSE).unwrap(),
        b"\x1b[<0;3;4M"
    );
    assert_eq!(
        mouse_bytes(16, 2, 3, true, Mode::SGR_MOUSE).unwrap(),
        b"\x1b[<16;3;4m"
    );
    assert_eq!(
        mouse_bytes(0, 0, 0, false, Mode::NONE).unwrap(),
        b"\x1b[M !!"
    );
    assert!(mouse_bytes(0, 223, 0, false, Mode::NONE).is_none());
}

#[test]
fn labels_obey_cell_width_and_cannot_inject_terminal_sequences() {
    for (text, columns, expected) in [
        ("中a", 4, "中a "),
        ("中中a", 3, "中 "),
        ("a\x1b\n", 4, "a   "),
        ("a\u{2003}", 4, "a\u{2003}  "),
        ("e\u{301}x", 1, "e\u{301}"),
    ] {
        let mut output = Vec::new();
        fit_text(&mut output, text, columns);
        assert_eq!(output, expected.as_bytes());
    }
}
