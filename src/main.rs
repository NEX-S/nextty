#[cfg(feature = "tui")]
mod ssh;

#[cfg(any(feature = "gui", feature = "tui"))]
use nextty::config::Config;
use nextty::error::{Error, Result};
use nextty::terminal::Shell;
use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut directory, mut program, mut config_path) = (None, None, None);
    let mut command = None;
    let mut gui = false;
    let mut positions = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            #[cfg(feature = "agent")]
            "--agent-owner" => {
                nextty::agent_owner::run()?;
                return Ok(());
            }
            "--help" | "-h" => {
                help();
                return Ok(());
            }
            "--version" | "-V" => {
                println!("nextty {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--working-directory" => {
                directory =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        Error::msg("--working-directory requires a path")
                    })?))
            }
            "--config" => {
                config_path = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| Error::msg("--config requires a path"))?,
                ))
            }
            "--gui" => gui = true,
            "--tui" => gui = false,
            "--bridge" => {
                #[cfg(feature = "tui")]
                {
                    let version = args
                        .next()
                        .ok_or_else(|| Error::msg("--bridge requires a protocol version"))?
                        .parse::<u8>()
                        .map_err(|_| Error::msg("invalid bridge protocol version"))?;
                    if args.next().is_some() {
                        return Err(Error::msg("unexpected bridge argument"));
                    }
                    nextty::workspace::run_bridge(version)?;
                    return Ok(());
                }
                #[cfg(not(feature = "tui"))]
                return Err(Error::msg("this build has no workspace support"));
            }
            "--server" => {
                #[cfg(feature = "tui")]
                {
                    let path = PathBuf::from(
                        args.next()
                            .ok_or_else(|| Error::msg("--server requires a socket path"))?,
                    );
                    if args.next().is_some() {
                        return Err(Error::msg("unexpected server argument"));
                    }
                    nextty::workspace::run_server(
                        &path,
                        nextty::workspace::ServerOptions::default(),
                    )?;
                    return Ok(());
                }
                #[cfg(not(feature = "tui"))]
                return Err(Error::msg("this build has no workspace support"));
            }
            "-e" => {
                program = Some(Shell::new(
                    args.next()
                        .ok_or_else(|| Error::msg("-e requires a command"))?,
                    args.collect::<Vec<_>>(),
                ));
                break;
            }
            "new" | "attach" | "list" | "terminate" if command.is_none() => command = Some(arg),
            _ if command.is_some() && positions.len() < 2 && !arg.starts_with('-') => {
                positions.push(arg)
            }
            _ => return Err(Error::msg(format!("unknown argument {arg:?}; use --help"))),
        }
    }
    #[cfg(feature = "tui")]
    {
        let (remote, name) = match (command.as_deref(), positions.as_slice()) {
            (_, []) => (None, None),
            (Some("list"), [host]) => (Some(ssh::RemoteTarget::parse(host)?), None),
            (Some("list"), _) => return Err(Error::msg("list takes at most one remote target")),
            (_, [host, name]) => (Some(ssh::RemoteTarget::parse(host)?), Some(name.clone())),
            (_, [host]) if ssh::RemoteTarget::recognizes(host) => {
                (Some(ssh::RemoteTarget::parse(host)?), None)
            }
            (_, [name]) => (None, Some(name.clone())),
            _ => unreachable!(),
        };
        workspace(command, name, remote, gui, directory, program, config_path)
    }
    #[cfg(not(feature = "tui"))]
    {
        if command.is_some() || !positions.is_empty() {
            return Err(Error::msg(
                "workspace commands require the tui Cargo feature",
            ));
        }
        if !gui {
            return Err(Error::msg(
                "this build has no TUI; use --gui for the desktop terminal",
            ));
        }
        #[cfg(feature = "gui")]
        {
            let config = Config::load(config_path)?;
            nextty::Desktop::new(config, program, directory)?.run()
        }
        #[cfg(not(feature = "gui"))]
        {
            let _ = (program, directory, config_path);
            Err(Error::msg("enable gui or tui to run a frontend"))
        }
    }
}

#[cfg(feature = "tui")]
fn workspace(
    command: Option<String>,
    name: Option<String>,
    remote: Option<ssh::RemoteTarget>,
    gui: bool,
    directory: Option<PathBuf>,
    program: Option<Shell>,
    config_path: Option<PathBuf>,
) -> Result<()> {
    use nextty::workspace::{self, Action, Client, CreateOptions};
    let command = command.as_deref().unwrap_or("new");
    if command != "new" && (directory.is_some() || program.is_some()) {
        return Err(Error::msg(
            "--working-directory and -e apply to new workspaces",
        ));
    }
    if matches!(command, "attach" | "terminate") && name.is_none() {
        return Err(Error::msg(format!(
            "{command} requires a workspace name; use nextty list"
        )));
    }
    if matches!(command, "new" | "attach") && gui && !cfg!(feature = "gui") {
        return Err(Error::msg(
            "this build has no gui frontend; enable its Cargo feature",
        ));
    }
    let socket = if remote.is_none() {
        Some(workspace::socket_path()?)
    } else {
        None
    };
    let connect = || match &remote {
        Some(target) => Client::connect_remote(target.command(), target.label()),
        None => Client::connect(socket.as_ref().unwrap()),
    };
    if matches!(command, "list" | "terminate") {
        // Control commands do not need a default frontend configuration.
        if config_path.is_some() {
            Config::load(config_path)?;
        }
        let action = if command == "list" {
            Action::List
        } else {
            Action::TerminateName(name.unwrap())
        };
        let mut client = match connect() {
            Ok(client) => client,
            Err(error)
                if command == "list"
                    && remote.is_none()
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
            {
                println!("No workspaces.");
                return Ok(());
            }
            Err(error) => return Err(Error::context("connect workspace server", error)),
        };
        client.send(action)?;
        return control_reply(&mut client, command == "list");
    }
    let config = Config::load(config_path)?;
    let size = CreateOptions::default().size;
    let action = if command == "attach" {
        Action::Attach {
            name: name.unwrap(),
            size,
        }
    } else {
        Action::Create {
            name: name.unwrap_or_default(),
            options: CreateOptions {
                size,
                foreground: config.foreground,
                background: config.background,
                directory: if remote.is_some() {
                    directory
                } else {
                    Some({
                        match directory {
                            Some(path) if path.is_absolute() => path,
                            Some(path) => std::env::current_dir()?.join(path),
                            None => std::env::current_dir()?,
                        }
                    })
                },
                program,
                history_capacity: config.history_limit_bytes,
            },
        }
    };
    if command == "new" && remote.is_none() {
        workspace::ensure_server(&std::env::current_exe()?, socket.as_ref().unwrap())
            .map_err(|error| Error::context("start workspace server", error))?;
    }
    let mut client =
        connect().map_err(|error| Error::context("connect workspace server", error))?;
    client.send(action)?;
    #[cfg(feature = "gui")]
    if gui {
        return nextty::WorkspaceView::new(client, config)?.run();
    }
    nextty::tui::run(client, config)?;
    Ok(())
}

#[cfg(feature = "tui")]
fn control_reply(client: &mut nextty::workspace::Client, list: bool) -> Result<()> {
    use nextty::workspace::ClientEvent;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        for event in client.poll()? {
            match event {
                ClientEvent::List(workspaces) => {
                    if workspaces.is_empty() {
                        println!("No workspaces.");
                    }
                    for workspace in workspaces {
                        println!(
                            "{}\t{} tabs\t{} panes\t{} attached",
                            workspace.name, workspace.tabs, workspace.panes, workspace.attached
                        );
                    }
                    return Ok(());
                }
                ClientEvent::Detached if !list => return Ok(()),
                ClientEvent::Error(error) => return Err(Error::msg(error)),
                _ => (),
            }
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(Error::msg("workspace server did not reply"));
        }
        let mut fd = libc::pollfd {
            fd: client.fd(),
            events: libc::POLLIN
                | if client.wants_write() {
                    libc::POLLOUT
                } else {
                    0
                },
            revents: 0,
        };
        if unsafe {
            libc::poll(
                &mut fd,
                1,
                remaining.as_millis().min(i32::MAX as u128) as i32,
            )
        } < 0
        {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
        if fd.revents & libc::POLLOUT != 0 {
            client.flush()?;
        }
    }
}
fn help() {
    println!("nextty — terminal\n\nUsage: nextty [OPTIONS] [-e COMMAND [ARG...]]");
    #[cfg(feature = "tui")]
    println!(
        "       nextty new [HOST] [NAME] [OPTIONS]\n       nextty attach [HOST] NAME [OPTIONS]\n       nextty list [HOST]\n       nextty terminate [HOST] NAME\n\nDefaults: use the current terminal; --tui selects it explicitly.\nHOST: [user@]host[:port] or ssh://[user@]host[:port]\nUse [IPv6]:port; SSH aliases work as HOST with NAME or ssh://alias."
    );
    #[cfg(feature = "gui")]
    println!("--gui opens a desktop window (Vulkan 1.3).");
    println!(
        "Options: --config PATH, --working-directory PATH, --help, --version\nConfiguration: $XDG_CONFIG_HOME/nextty/config.toml (default ~/.config/nextty/config.toml)"
    );
    #[cfg(feature = "gui")]
    println!(
        "\nDesktop input:\nCtrl+Shift+C/V    Copy/paste\nCtrl+double-click Select a URL, path or quoted object\nShift+PageUp/Down Scroll history\nShift+Home/End    Top/bottom of history\nShift+mouse      Select when an application captures the mouse\nMiddle click     Paste primary selection"
    );
    #[cfg(feature = "tui")]
    println!(
        "\nWorkspace input:\nAlt+v/s          Split left/right or top/bottom; drag the divider to resize\nAlt+h/j/k/l      Focus a pane\nAlt+,/.          Previous/next tab\nAlt+Enter        New tab\nAlt+q            Detach; workspace keeps running\nAlt+o            Cycle input focus\nAlt+?            Context help\nAlt+/            Find pane output; Enter/ShiftEnter next/previous, Esc restores view\nAlt+p/n          Previous/next prompt\nAlt+c            Copy command output; an existing selection takes precedence\n\nNew unnamed sessions use the lowest available number: 0, 1, 2, ...\nAttach takes control; the previous frontend becomes read-only.\nRemote sessions use system ssh and require nextty on PATH at both ends.\nRemote cwd defaults to remote HOME; relative paths are resolved there."
    );
    #[cfg(feature = "agent")]
    println!(
        "\nAlt+a            Toggle Agent chat\nDouble-click     Attach object context to chat\nAgent editor: Enter sends, Ctrl+J inserts a newline, Ctrl+A selects all.\nEsc selects an earlier user message; repeat for older, Enter confirms rewind.\nCtrl+C copies a selection or clears the draft; in Agent output it copies a selection or stops the reply."
    );
    #[cfg(feature = "monitor")]
    println!(
        "Alt+m or resource counters open the resizable monitor; Tab/arrows navigate, Enter activates. SSH counters describe the local machine."
    );
    #[cfg(feature = "files")]
    println!(
        "Alt+f            Files: hjkl navigate, / search, Enter inserts a quoted path (local sessions)"
    );
    println!(
        "\nCompiled features: gui={}, tui={}, monitor={}, files={}, agent={}",
        cfg!(feature = "gui"),
        cfg!(feature = "tui"),
        cfg!(feature = "monitor"),
        cfg!(feature = "files"),
        cfg!(feature = "agent")
    );
}
