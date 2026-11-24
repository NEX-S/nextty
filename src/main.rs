mod ssh;

use nextty::config::Config;
use nextty::terminal::Shell;
use nextty::workspace::{self, Action, Client, CreateOptions};
use std::io::{self, Result};
use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut directory, mut program, mut config_path) = (None, None, None);
    let (mut command, mut gui, mut positional) = (None, false, Vec::new());
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| io::Error::other(format!("{arg} requires an argument")))
        };
        match arg.as_str() {
            "--agent-owner" => return nextty::agent::owner::run(),
            "--help" | "-h" => {
                help();
                return Ok(());
            }
            "--version" | "-V" => {
                println!("nextty {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--working-directory" => directory = Some(PathBuf::from(value()?)),
            "--config" => config_path = Some(PathBuf::from(value()?)),
            "--gui" => gui = true,
            "--tui" => gui = false,
            "--bridge" => return nextty::bridge::run_bridge(&value()?),
            "--server" => return nextty::workspace::run_server(&PathBuf::from(value()?)),
            "-e" => {
                program = Some(Shell::new(value()?, args.collect()));
                break;
            }
            "new" | "attach" | "list" | "terminate" if command.is_none() => command = Some(arg),
            _ if command.is_some() && positional.len() < 2 && !arg.starts_with('-') => {
                positional.push(arg)
            }
            _ => {
                return Err(io::Error::other(format!(
                    "unknown argument {arg:?}; use --help"
                )));
            }
        }
    }

    let command = command.as_deref().unwrap_or("new");
    let (remote, name) = match (command, positional.as_slice()) {
        (_, []) => (None, None),
        ("list", [host]) => (Some(host), None),
        ("list", _) => return Err(io::Error::other("list takes at most one remote target")),
        (_, [host, name]) => (Some(host), Some(name.clone())),
        (_, [host]) if ssh::RemoteTarget::recognizes(host) => (Some(host), None),
        (_, [name]) => (None, Some(name.clone())),
        _ => unreachable!(),
    };
    let remote = remote
        .map(|host| ssh::RemoteTarget::parse(host))
        .transpose()?;
    if matches!(command, "attach" | "terminate") && name.is_none() {
        return Err(io::Error::other(format!(
            "{command} requires a workspace name; use nextty list"
        )));
    }
    if matches!(command, "new" | "attach") && gui && !cfg!(feature = "gui") {
        return Err(io::Error::other(
            "this build has no gui frontend; enable its Cargo feature",
        ));
    }
    let socket = remote.is_none().then(workspace::socket_path).transpose()?;
    let connect = || {
        match (&remote, &socket) {
            (Some(target), _) => Client::connect_remote(target.command(command), target.label()),
            (None, socket) => Client::connect(socket.as_ref().unwrap()),
        }
        .map_err(|error| io::Error::new(error.kind(), format!("connect workspace server: {error}")))
    };
    if matches!(command, "list" | "terminate") {
        let list = command == "list";
        // Control commands do not need a default frontend configuration.
        if config_path.is_some() {
            Config::load(config_path)?;
        }
        let mut client = match connect() {
            Err(error)
                if list
                    && remote.is_none()
                    && matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) =>
            {
                println!("No workspaces.");
                return Ok(());
            }
            client => client?,
        };
        client.send(match name {
            Some(name) => Action::TerminateName(name),
            None => Action::List,
        })?;
        return control_reply(&mut client, list);
    }
    let config = Config::load(config_path)?;
    let action = match name {
        Some(name) if command == "attach" => Action::Attach { name },
        name => Action::Create {
            name: name.unwrap_or_default(),
            options: CreateOptions {
                foreground: config.foreground,
                background: config.background,
                // A remote server resolves relative or missing directories against its own HOME.
                directory: if remote.is_some() {
                    directory
                } else {
                    Some(std::path::absolute(
                        directory.unwrap_or_else(|| ".".into()),
                    )?)
                },
                program,
                history_capacity: config.history_limit_bytes,
                ..CreateOptions::default()
            },
        },
    };
    if command == "new"
        && let Some(socket) = &socket
    {
        workspace::ensure_server(&std::env::current_exe()?, socket).map_err(|error| {
            io::Error::new(error.kind(), format!("start workspace server: {error}"))
        })?;
    }
    let mut client = connect()?;
    client.send(action)?;
    #[cfg(feature = "gui")]
    if gui {
        return nextty::WorkspaceView::new(client, config)?.run();
    }
    nextty::tui::run(client, config)
}

fn control_reply(client: &mut Client, list: bool) -> Result<()> {
    use nextty::workspace::Response;
    loop {
        let events = match client.poll() {
            // A bridge that finds no server ends quietly, as the local connect does.
            Err(error)
                if list
                    && matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::BrokenPipe
                            | io::ErrorKind::ConnectionReset
                    ) =>
            {
                println!("No workspaces.");
                return Ok(());
            }
            events => events?,
        };
        for event in events {
            match event {
                Response::List(workspaces) => {
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
                Response::Detached if !list => return Ok(()),
                Response::Error(error) => return Err(io::Error::other(error)),
                _ => (),
            }
        }
        // The next poll also sends what is still queued.
        nextty::sys::poll(&mut [client.pollfd()], -1)?;
    }
}
fn help() {
    println!("nextty — terminal\n\nUsage: nextty [OPTIONS] [-e COMMAND [ARG...]]");

    println!(
        "       nextty new [HOST] [NAME] [OPTIONS]\n       nextty attach [HOST] NAME [OPTIONS]\n       nextty list [HOST]\n       nextty terminate [HOST] NAME\n\nDefaults to the current terminal.\nHOST: [user@]host[:port] or ssh://[user@]host[:port]\nUse [IPv6]:port; SSH aliases work as HOST with NAME or ssh://alias."
    );
    #[cfg(feature = "gui")]
    println!("--gui opens a desktop window (Vulkan 1.3).");
    println!(
        "Options: --config PATH, --working-directory PATH, --help, --version\nConfiguration: $XDG_CONFIG_HOME/nextty/config.toml (default ~/.config/nextty/config.toml)"
    );
    #[cfg(feature = "gui")]
    println!(
        "\nDesktop input:\nCtrl+Shift+C/V    Copy/paste\nShift+Home/End    Top/bottom of history\nMiddle click     Paste primary selection\nRight click      Paste when there is nothing to copy"
    );

    println!(
        "\nWorkspace input:\nAlt+v/s          Split left/right or top/bottom; drag the divider to resize\nAlt+h/j/k/l      Focus a pane\nAlt+,/.          Previous/next tab\nAlt+Enter        New tab\nAlt+q            Detach; workspace keeps running\nAlt+/            Find pane output; Enter/ShiftEnter next/previous, Esc restores view\nAlt+p/n          Previous/next prompt\nAlt+c            Copy command output; an existing selection takes precedence\n\nMouse: drag selects, double-click opens the highlighted object with xdg-open or selects a word,\ntriple-click selects a line, right click copies the highlight or command output.\nAlt+mouse selects or scrolls locally when an application captures the mouse.\n\nNew unnamed sessions use the lowest available number: 0, 1, 2, ...\nAttach takes control; the previous frontend becomes read-only.\nRemote sessions use system ssh and require nextty on PATH at both ends.\nRemote cwd defaults to remote HOME; relative paths are resolved there."
    );

    println!(
        "\nAlt+a            Toggle Agent chat\nAgent editor: Enter sends, Shift+Enter inserts a newline, Ctrl+A selects all.\nEsc twice enters history; j/k selects, Enter rewinds, Esc leaves history.\nCtrl+C clears input; when empty, stops the Agent. Ctrl+h/j/k/l navigates input and prompt history."
    );

    println!(
        "Alt+m or resource counters open the resizable monitor; j/k selects controls, l enters, h returns. SSH counters describe the local machine."
    );

    println!(
        "Alt+f            Files: left tree, l expands/previews, Enter inserts path (local sessions)"
    );
    println!("\nCompiled features: gui={}", cfg!(feature = "gui"));
}
