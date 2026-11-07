mod ssh;

use nextty::config::Config;
use nextty::error::{Error, Result};
use nextty::terminal::Shell;
use nextty::workspace::{self, Action, Client, CreateOptions};
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
    let mut gui = !cfg!(feature = "tui") && cfg!(feature = "gui");
    let mut positions = [None, None];
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--agent-owner" => {
                nextty::agent::owner::run()?;
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
            "--server" => {
                let path = PathBuf::from(
                    args.next()
                        .ok_or_else(|| Error::msg("--server requires a socket path"))?,
                );
                if args.next().is_some() {
                    return Err(Error::msg("unexpected server argument"));
                }
                nextty::workspace::run_server(&path)?;
                return Ok(());
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
            _ if command.is_some() && positions[1].is_none() && !arg.starts_with('-') => {
                let index = usize::from(positions[0].is_some());
                positions[index] = Some(arg)
            }
            _ => return Err(Error::msg(format!("unknown argument {arg:?}; use --help"))),
        }
    }

    let [first, second] = positions;
    let (remote, name) = match (command.as_deref(), first, second) {
        (_, None, None) => (None, None),
        (Some("list"), Some(host), None) => (Some(ssh::RemoteTarget::parse(&host)?), None),
        (Some("list"), _, _) => return Err(Error::msg("list takes at most one remote target")),
        (_, Some(host), Some(name)) => (Some(ssh::RemoteTarget::parse(&host)?), Some(name)),
        (_, Some(host), None) if ssh::RemoteTarget::recognizes(&host) => {
            (Some(ssh::RemoteTarget::parse(&host)?), None)
        }
        (_, Some(name), None) => (None, Some(name)),
        _ => unreachable!(),
    };
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
    if matches!(command, "new" | "attach") && !gui && !cfg!(feature = "tui") {
        return Err(Error::msg(
            "this build has no tui frontend; enable its Cargo feature",
        ));
    }
    let socket = remote.is_none().then(workspace::socket_path).transpose()?;
    let connect = || match &remote {
        Some(target) => Client::connect_remote(target.command(), target.label()),
        None => Client::connect(socket.as_ref().unwrap()),
    };
    if matches!(command, "list" | "terminate") {
        let list = command == "list";
        // Control commands do not need a default frontend configuration.
        if config_path.is_some() {
            Config::load(config_path)?;
        }
        let action = if list {
            Action::List
        } else {
            Action::TerminateName(name.unwrap())
        };
        let mut client = match connect() {
            Ok(client) => client,
            Err(error)
                if list
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
        return control_reply(&mut client, list);
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
                    Some(match directory {
                        Some(path) if path.is_absolute() => path,
                        Some(path) => std::env::current_dir()?.join(path),
                        None => std::env::current_dir()?,
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
    #[cfg(feature = "tui")]
    nextty::tui::run(client, config)?;
    Ok(())
}

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
        let mut fd = client.pollfd();
        nextty::poll::poll(
            std::slice::from_mut(&mut fd),
            nextty::poll::timeout_millis(remaining),
        )?;
        if fd.revents & libc::POLLOUT != 0 {
            client.flush()?;
        }
    }
}
fn help() {
    println!("nextty — terminal\n\nUsage: nextty [OPTIONS] [-e COMMAND [ARG...]]");

    println!(
        "       nextty new [HOST] [NAME] [OPTIONS]\n       nextty attach [HOST] NAME [OPTIONS]\n       nextty list [HOST]\n       nextty terminate [HOST] NAME\n\nDefaults: use the current terminal when tui is compiled in, otherwise GUI. --tui selects the current terminal explicitly.\nHOST: [user@]host[:port] or ssh://[user@]host[:port]\nUse [IPv6]:port; SSH aliases work as HOST with NAME or ssh://alias."
    );
    #[cfg(feature = "gui")]
    println!("--gui opens a desktop window (Vulkan 1.3).");
    println!(
        "Options: --config PATH, --working-directory PATH, --help, --version\nConfiguration: $XDG_CONFIG_HOME/nextty/config.toml (default ~/.config/nextty/config.toml)"
    );
    #[cfg(feature = "gui")]
    println!(
        "\nDesktop input:\nCtrl+Shift+C/V    Copy/paste\nShift+Home/End    Top/bottom of history\nAlt+mouse        Select/scroll when an application captures the mouse\nMiddle click     Paste primary selection\nRight click      Copy highlighted object; otherwise paste"
    );

    println!(
        "\nWorkspace input:\nAlt+v/s          Split left/right or top/bottom; drag the divider to resize\nAlt+h/j/k/l      Focus a pane\nAlt+,/.          Previous/next tab\nAlt+Enter        New tab\nAlt+q            Detach; workspace keeps running\nAlt+/            Find pane output; Enter/ShiftEnter next/previous, Esc restores view\nAlt+p/n          Previous/next prompt\nAlt+c            Copy command output; an existing selection takes precedence\n\nNew unnamed sessions use the lowest available number: 0, 1, 2, ...\nAttach takes control; the previous frontend becomes read-only.\nRemote sessions use system ssh and require nextty on PATH at both ends.\nRemote cwd defaults to remote HOME; relative paths are resolved there."
    );

    println!(
        "\nAlt+a            Toggle Agent chat\nDouble-click     Open highlighted object with xdg-open\nAgent editor: Enter sends, Shift+Enter inserts a newline, Ctrl+A selects all.\nEsc twice enters history; j/k selects, Enter rewinds, Esc leaves history.\nCtrl+C clears input; when empty, stops the Agent. Ctrl+h/j/k/l navigates input and prompt history."
    );

    println!(
        "Alt+m or resource counters open the resizable monitor; j/k selects controls, l enters, h returns. SSH counters describe the local machine."
    );

    println!(
        "Alt+f            Files: left tree, l expands/previews, Enter inserts path (local sessions)"
    );
    println!(
        "\nCompiled features: gui={}, tui={}",
        cfg!(feature = "gui"),
        cfg!(feature = "tui")
    );
}
