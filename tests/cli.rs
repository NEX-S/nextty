use nextty::terminal::Shell;
use nextty::workspace::{Action, Client, CreateOptions, Response};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

#[test]
fn remote_argument_errors_do_not_launch_ssh_or_load_configuration() {
    for (args, expected) in [
        (vec!["attach", "user@host"], "requires a workspace name"),
        (vec!["list", "user@host", "0"], "at most one remote target"),
        (
            vec!["new", "user@host:invalid", "0", "--tui"],
            "remote target",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_nextty"))
            .args(&args)
            .env("PATH", "")
            .env("XDG_CONFIG_HOME", "relative-invalid-config")
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn control_commands_skip_default_config_but_validate_explicit_config() {
    // The runtime path must fit sockaddr_un even in a deeply nested checkout.
    let directory = std::env::temp_dir().join(format!("nextty-cli-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let config = directory.join("config");
    fs::create_dir_all(config.join("nextty")).unwrap();
    let invalid = config.join("nextty/config.toml");
    fs::write(&invalid, "unknown = true\n").unwrap();
    let runtime = directory.join("runtime");
    fs::DirBuilder::new().mode(0o700).create(&runtime).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_nextty"))
            .args(args)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap()
    };

    let list = run(&["list"]);
    let terminate = run(&["terminate"]);
    let explicit = run(&["list", "--config", invalid.to_str().unwrap()]);
    fs::remove_dir_all(directory).unwrap();

    assert!(list.status.success(), "{:?}", list);
    assert_eq!(list.stdout, b"No workspaces.\n");
    assert!(!terminate.status.success());
    assert!(String::from_utf8_lossy(&terminate.stderr).contains("requires a workspace name"));
    assert!(!explicit.status.success());
    assert!(String::from_utf8_lossy(&explicit.stderr).contains("unknown configuration field"));
}

#[test]
fn bridge_starts_the_server_for_new_only_and_resolves_directories_in_its_home() {
    fn until(client: &mut Client, mut done: impl FnMut(&[Response]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(&client.poll().unwrap()) {
            assert!(Instant::now() < deadline, "bridge did not answer");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let root = std::env::temp_dir().join(format!("nextty-bridge-{}", std::process::id()));
    let (home, runtime, launch) = (root.join("home"), root.join("runtime"), root.join("launch"));
    fs::create_dir_all(home.join("project")).unwrap();
    fs::create_dir(&launch).unwrap();
    fs::DirBuilder::new().mode(0o700).create(&runtime).unwrap();
    let bridge = |new: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nextty"));
        command
            .args(["--bridge", if new { "new" } else { "attach" }])
            .current_dir(&launch)
            .env("HOME", &home)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CACHE_HOME", root.join("cache"));
        Client::connect_remote(command, "test".into()).unwrap()
    };

    // `nextty list HOST` through an ssh that just runs the bridge here.
    fs::create_dir(root.join("bin")).unwrap();
    fs::write(
        root.join("bin/ssh"),
        "#!/bin/sh\nfor last; do :; done\nexec \"$NEXTTY\" ${last#nextty }\n",
    )
    .unwrap();
    fs::set_permissions(root.join("bin/ssh"), fs::Permissions::from_mode(0o755)).unwrap();
    let list_over_ssh = || {
        let output = Command::new(env!("CARGO_BIN_EXE_nextty"))
            .args(["list", "user@host"])
            .env("NEXTTY", env!("CARGO_BIN_EXE_nextty"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            )
            .env("HOME", &home)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CACHE_HOME", root.join("cache"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    assert_eq!(list_over_ssh(), "No workspaces.\n");
    assert!(
        fs::read_dir(runtime.join("nextty"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".sock")),
        "list started a server"
    );

    let mut frontend = bridge(true);
    frontend
        .send(Action::Create {
            name: "bridged".into(),
            options: CreateOptions {
                directory: Some("project".into()),
                program: Some(Shell::new(
                    "/bin/sh".into(),
                    vec!["-c".into(), "pwd > ../pwd.out; exec sleep 30".into()],
                )),
                ..CreateOptions::default()
            },
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    // The shell creates the file before it writes the directory.
    while !fs::read_to_string(home.join("pwd.out")).is_ok_and(|text| !text.is_empty())
        && Instant::now() < deadline
    {
        frontend.poll().unwrap();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        fs::read_to_string(home.join("pwd.out")).unwrap().trim(),
        home.join("project").to_str().unwrap()
    );
    let server = fs::read_dir("/proc")
        .unwrap()
        .filter_map(Result::ok)
        .find(|process| {
            fs::read_to_string(process.path().join("cmdline"))
                .is_ok_and(|command| command.contains(runtime.to_str().unwrap()))
        })
        .expect("no server process")
        .path();
    assert_eq!(fs::read_link(server.join("cwd")).unwrap(), Path::new("/"));

    assert!(list_over_ssh().starts_with("bridged\t1 tabs\t1 panes"));
    let mut control = bridge(false);
    control.send(Action::List).unwrap();
    until(&mut control, |events| {
        events.iter().any(|event| {
            matches!(event, Response::List(list) if list.len() == 1 && list[0].name == "bridged")
        })
    });
    control
        .send(Action::TerminateName("bridged".into()))
        .unwrap();
    until(&mut control, |events| {
        events
            .iter()
            .any(|event| matches!(event, Response::Detached))
    });
    drop((control, frontend));
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.exists() {
        assert!(
            Instant::now() < deadline,
            "server outlived its last workspace"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    fs::remove_dir_all(root).unwrap();
}
