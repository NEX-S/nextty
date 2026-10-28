use crate::config::Config;
use crate::ui::AgentPanel;
use crate::workspace::{Action, Client, ClientEndpoint, ClientEvent};
use serde_json::{Value, json};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn bytes(value: &OsStr) -> Value {
    json!(value.as_bytes())
}
fn os(value: Value) -> io::Result<OsString> {
    Ok(OsString::from_vec(serde_json::from_value(value)?))
}

pub(crate) fn spawn(client: &Client, token: &[u8], config: &Config) -> io::Result<()> {
    let endpoint = match client.endpoint() {
        ClientEndpoint::Local(path) => json!({"local": bytes(path.as_os_str())}),
        ClientEndpoint::Remote {
            program,
            args,
            env,
            directory,
            target,
        } => json!({
            "program":bytes(program), "args":args.iter().map(|v| bytes(v)).collect::<Vec<_>>(),
            "env":env.iter().map(|(k,v)| (bytes(k),v.as_ref().map(|v| bytes(v)))).collect::<Vec<_>>(),
            "directory":directory.as_ref().map(|p| bytes(p.as_os_str())), "target":target,
        }),
    };
    let name = &client
        .snapshot()
        .ok_or_else(|| io::Error::other("No workspace"))?
        .name;
    let init = json!({"endpoint":endpoint,"name":name,"token":token,
        "base_url":config.base_url,"model":config.model});
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--agent-owner")
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let stdin = child.stdin.take().unwrap();
    std::thread::spawn(move || child.wait());
    serde_json::to_writer(stdin, &init)?;
    Ok(())
}

pub fn run() -> io::Result<()> {
    let mut init: Value = serde_json::from_reader(io::stdin().lock())?;
    let endpoint = &mut init["endpoint"];
    let mut client = if !endpoint["local"].is_null() {
        Client::connect(&PathBuf::from(os(endpoint["local"].take())?))?
    } else {
        let mut command = Command::new(os(endpoint["program"].take())?);
        let args: Vec<Vec<u8>> = serde_json::from_value(endpoint["args"].take())?;
        command.args(args.into_iter().map(OsString::from_vec));
        let env: Vec<(Vec<u8>, Option<Vec<u8>>)> = serde_json::from_value(endpoint["env"].take())?;
        for (key, value) in env {
            let key = OsString::from_vec(key);
            if let Some(value) = value {
                command.env(key, OsString::from_vec(value));
            } else {
                command.env_remove(key);
            }
        }
        if !endpoint["directory"].is_null() {
            command.current_dir(os(endpoint["directory"].take())?);
        }
        Client::connect_remote(command, serde_json::from_value(endpoint["target"].take())?)?
    };
    let config = Config {
        base_url: serde_json::from_value(init["base_url"].take())?,
        model: serde_json::from_value(init["model"].take())?,
        ..Config::default()
    };
    client.send(Action::AgentAttach {
        name: serde_json::from_value(init["name"].take())?,
        token: serde_json::from_value(init["token"].take())?,
    })?;
    let mut panel = AgentPanel::default();
    panel.set_local();
    let mut pending = Vec::new();
    loop {
        let mut fds = vec![
            libc::pollfd {
                fd: client.fd(),
                events: libc::POLLIN
                    | if client.wants_write() {
                        libc::POLLOUT
                    } else {
                        0
                    },
                revents: 0,
            },
            libc::pollfd {
                fd: client.receiver_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        fds.extend(panel.fds().map(|(fd, events)| libc::pollfd {
            fd,
            events,
            revents: 0,
        }));
        let result = unsafe {
            libc::poll(
                fds.as_mut_ptr(),
                fds.len() as _,
                if client.needs_poll() { 0 } else { -1 },
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        client.flush()?;
        for event in client.poll()? {
            match event {
                ClientEvent::Detached => return Ok(()),
                ClientEvent::Error(error) => return Err(io::Error::other(error)),
                ClientEvent::Updated => {
                    for session in &mut panel.sessions {
                        session.model.poll_monitors(&client)?;
                    }
                }
                _ => (),
            }
        }
        pending.extend(client.take_agent_events());
        if client.snapshot().is_none() {
            continue;
        }
        let mut publish_models = false;
        for (kind, payload) in pending.drain(..) {
            match kind {
                0 => {
                    panel.restore_view_state(serde_json::from_slice(&payload)?)?;
                    panel.set_local();
                }
                2 => {
                    let request: Value = serde_json::from_slice(&payload)?;
                    let id: u64 = serde_json::from_value(request["session"].clone())?;
                    if let Some(session) =
                        panel.sessions.iter_mut().find(|session| session.id == id)
                    {
                        let result = match request["action"].as_str() {
                            Some("send") => session
                                .restore_request(request)
                                .and_then(|()| session.send_local(&client, &config).map(|_| ())),
                            Some("abort") => session.model.abort(),
                            Some("rewind") => session
                                .model
                                .rewind(serde_json::from_value(request["index"].clone())?),
                            _ => Err(io::Error::other("Unknown Agent action")),
                        };
                        if let Err(error) = result {
                            session.model.messages.push(crate::agent::Message {
                                turn: 0,
                                role: "Error".into(),
                                text: error.to_string(),
                                id: None,
                            });
                            session.model.busy = false;
                            session.model.revision += 1;
                        }
                        publish_models = true;
                    }
                }
                _ => (),
            }
        }
        if client.can_agent_input() && (publish_models || fds[2..].iter().any(|fd| fd.revents != 0))
        {
            let models = (!publish_models).then(|| panel.model_snapshot());
            for session in &mut panel.sessions {
                session.poll_local(&mut client)?;
            }
            let updated = panel.model_snapshot();
            if models.as_ref() != Some(&updated) {
                client.send(Action::AgentUpdate {
                    kind: 1,
                    payload: serde_json::to_vec(&updated)?,
                })?;
            }
        }
    }
}
