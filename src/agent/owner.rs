use crate::agent::Agent;
use crate::config::Config;
use crate::workspace::{Action, AgentEvent, AgentRequest, Client, ClientEndpoint, Response};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct OwnerInit {
    endpoint: ClientEndpoint,
    name: String,
    base_url: String,
    model: String,
}

pub(crate) fn spawn(client: &Client, config: &Config) -> io::Result<()> {
    let init = OwnerInit {
        endpoint: client.endpoint().clone(),
        name: client.snapshot().unwrap().name.clone(),
        base_url: config.base_url.clone(),
        model: config.model.clone(),
    };
    // Keep using the running binary even if an upgrade has unlinked it.
    let mut command = Command::new("/proc/self/exe");
    command.arg("--agent-owner").stdin(Stdio::piped());
    let log = crate::workspace::socket_path()?.with_extension("agent.log");
    let mut child = crate::workspace::spawn_detached(&mut command, log)?;
    let mut stdin = child.stdin.take().unwrap();
    crate::sys::task(move || {
        let _ = child.wait();
    })?;
    stdin.write_all(&postcard::to_allocvec(&init).map_err(io::Error::other)?)
}

pub fn run() -> io::Result<()> {
    let mut bytes = Vec::new();
    io::stdin().read_to_end(&mut bytes)?;
    let OwnerInit {
        endpoint,
        name,
        base_url,
        model,
    } = postcard::from_bytes(&bytes).map_err(io::Error::other)?;
    let mut client = match endpoint {
        ClientEndpoint::Local(path) => Client::connect(&path)?,
        ClientEndpoint::Remote {
            program,
            args,
            target,
        } => {
            let mut command = Command::new(OsString::from_vec(program));
            command.args(args.into_iter().map(OsString::from_vec));
            Client::connect_remote(command, target)?
        }
    };
    let config = Config {
        base_url,
        model,
        ..Config::default()
    };
    client.send(Action::AgentAttach { name })?;
    client.flush()?;
    let mut agents = Vec::<Agent>::new();
    let mut publish_at = Instant::now();
    let mut fds = Vec::new();
    loop {
        fds.clear();
        fds.push(client.pollfd());
        fds.extend(agents.iter().map(|agent| libc::pollfd {
            fd: agent.fd(),
            events: libc::POLLIN,
            revents: 0,
        }));
        let timeout = if client.needs_poll() {
            0
        } else {
            let publish = agents.iter().any(|agent| agent.dirty);
            agents
                .iter()
                .filter_map(Agent::deadline)
                .chain(publish.then_some(publish_at))
                .min()
                .map_or(-1, |deadline| {
                    crate::sys::timeout_millis(deadline.saturating_duration_since(Instant::now()))
                })
        };
        crate::sys::poll(&mut fds, timeout)?;
        for fd in fds[1..].iter().filter(|fd| fd.revents != 0) {
            crate::sys::consume_notification(fd.fd)?;
        }
        if fds[0].revents != 0 || client.needs_poll() {
            for mut event in client.poll()? {
                for agent in &mut agents {
                    agent.handle_event(&mut event, &mut client);
                }
                match event {
                    Response::Detached => return Ok(()),
                    Response::Error(error) => return Err(io::Error::other(error)),
                    _ => (),
                }
            }
        }
        if client.snapshot().is_none() {
            continue;
        }
        while let Some(event) = client.next_agent_event() {
            match event {
                // Only complete snapshots arrive: the daemon keeps draft edits between frontends.
                AgentEvent::Draft(drafts) => {
                    let listed = |id| drafts.sessions.iter().any(|draft| draft.id == id);
                    for mut agent in agents.extract_if(.., |agent| !listed(agent.id)) {
                        agent.close(&mut client)?;
                        client.send(Action::AgentRemove(agent.id))?;
                    }
                    for draft in &drafts.sessions {
                        if !agents.iter().any(|agent| agent.id == draft.id) {
                            agents.push(Agent {
                                id: draft.id,
                                ..Agent::default()
                            });
                        }
                    }
                }
                AgentEvent::Request(request) => {
                    let id = match &request {
                        AgentRequest::Send(send) => send.session,
                        AgentRequest::Abort { session } | AgentRequest::Rewind { session, .. } => {
                            *session
                        }
                    };
                    let Some(agent) = agents.iter_mut().find(|agent| agent.id == id) else {
                        continue;
                    };
                    if let AgentRequest::Send(send) = request {
                        agent.pending_send = Some(send);
                        client.refresh_screen()?;
                    } else {
                        agent.request(request, &config, &mut client);
                    }
                }
                _ => (),
            }
        }
        for agent in &mut agents {
            if !client.screen_stale()
                && let Some(send) = agent.pending_send.take()
            {
                agent.request(AgentRequest::Send(send), &config, &mut client);
            }
            agent.poll(&mut client)?;
        }
        if agents.iter().any(Agent::needs_screen) {
            client.request_screen()?;
        }
        if agents.iter().any(|agent| agent.dirty)
            && (Instant::now() >= publish_at || agents.iter().all(|agent| !agent.busy))
        {
            for agent in agents.iter_mut().filter(|agent| agent.dirty) {
                client.send(Action::AgentUpdate(agent.patch()))?;
            }
            publish_at = Instant::now() + Duration::from_millis(16);
        }
        client.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::panel::AgentView;
    use crate::context::{ContextItem, ContextSnapshot};
    use crate::workspace::{AgentChange, AgentMessage, AgentRecovery, AgentSend};
    use std::sync::Arc;

    #[test]
    fn streaming_patches_only_append_new_bytes_and_reset_after_trim() {
        let mut agent = Agent::default();
        let message = |role: &str, text: String| AgentMessage {
            turn: 1,
            role: role.into(),
            id: None,
            text,
        };
        agent.messages = vec![
            message("You", "question".into()),
            message("Agent", "x".repeat(128 * 1024)),
        ];
        let mut mirror = AgentView::default();
        mirror.restore_patch(&agent.patch()).unwrap();
        let before = agent.messages[1].text.len();
        agent.messages[1].text.push_str("中文🙂");
        let patch = agent.patch();
        assert_eq!((patch.start, patch.prefix_bytes as usize), (1, before));
        assert_eq!(patch.messages.len(), 1);
        assert_eq!(patch.messages[0].text, "中文🙂");
        assert!(patch.changes.is_none());
        assert!(!mirror.restore_patch(&patch).unwrap());
        assert_eq!(mirror.messages, agent.messages);
        agent.messages[1].role = "shell · done".into();
        agent.revision += 1;
        assert!(mirror.restore_patch(&agent.patch()).unwrap());
        agent.messages.remove(0);
        agent.revision += 1;
        let patch = agent.patch();
        assert_eq!((patch.start, patch.prefix_bytes), (0, 0));
        mirror.restore_patch(&patch).unwrap();
        assert_eq!(mirror.messages, agent.messages);
    }

    #[test]
    fn edit_records_are_published_when_they_change_and_survive_transcript_resets() {
        let mut agent = Agent::default();
        let mut mirror = AgentView::default();
        mirror.restore_patch(&agent.patch()).unwrap();
        Arc::make_mut(&mut agent.changes).push(AgentChange {
            path: "project/a.rs".into(),
            added: 2,
            removed: 1,
            ..AgentChange::default()
        });
        agent.revision += 1;
        assert!(mirror.restore_patch(&agent.patch()).unwrap());
        assert_eq!(mirror.changes, agent.changes);
        agent.revision += 1;
        assert!(agent.patch().changes.is_none());
        agent.changes_omitted = 1;
        assert!(mirror.restore_patch(&agent.patch()).unwrap());
        assert_eq!((mirror.changes.len(), mirror.changes_omitted), (1, 1));
    }

    #[test]
    fn rejected_and_superseded_sends_return_draft_and_attachments() {
        let path =
            std::env::temp_dir().join(format!("nextty-owner-send-{}.sock", std::process::id()));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut client = Client::connect(&path).unwrap();
        let mut peer = crate::wire::Peer::new(listener.accept().unwrap().0).unwrap();
        std::fs::remove_file(path).unwrap();
        let basket = vec![ContextItem {
            label: "screen".into(),
            source: "pane 1".into(),
            kind: "Screen".into(),
            snapshot: ContextSnapshot {
                text: "registers".into(),
                incomplete: false,
            },
        }];
        let send = |input: &str| AgentSend {
            session: 0,
            pane: 1,
            input: input.into(),
            basket: basket.clone(),
        };
        let mut recovered = |client: &mut Client, input: &str| {
            client.flush().unwrap();
            let expected = AgentRecovery {
                session: 0,
                rewind: false,
                input: input.into(),
                basket: basket.clone(),
            };
            match peer.next_action().unwrap() {
                Some(Action::AgentRecover(recovery)) => assert_eq!(recovery, expected),
                other => panic!("expected a recovery, got {other:?}"),
            }
        };
        let mut agent = Agent::default();
        let request = AgentRequest::Send(send("inspect this"));
        agent.request(request, &Config::default(), &mut client);
        recovered(&mut client, "inspect this");
        assert_eq!(agent.messages[0].role, "Error");
        assert!(!agent.busy && agent.sent_context.is_empty());
        agent.pending_send = Some(send("queued draft"));
        let abort = AgentRequest::Abort { session: 0 };
        agent.request(abort, &Config::default(), &mut client);
        recovered(&mut client, "queued draft");
        assert!(agent.pending_send.is_none());
    }
}
