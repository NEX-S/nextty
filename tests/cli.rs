use nextty::terminal::Shell;
use nextty::workspace::{Action, Client, CreateOptions};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

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
    let root = std::env::temp_dir().join(format!("nextty-bridge-{}", std::process::id()));
    let (home, runtime, launch) = (root.join("home"), root.join("runtime"), root.join("launch"));
    fs::create_dir_all(home.join("project")).unwrap();
    fs::create_dir(&launch).unwrap();
    fs::DirBuilder::new().mode(0o700).create(&runtime).unwrap();
    let environment = |command: &mut Command| {
        command
            .current_dir(&launch)
            .env("HOME", &home)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CACHE_HOME", root.join("cache"));
    };
    // Control commands for HOST go through an ssh that runs the remote command here.
    fs::create_dir(root.join("bin")).unwrap();
    fs::write(
        root.join("bin/ssh"),
        "#!/bin/sh\nfor last; do :; done\nexec \"$NEXTTY\" ${last#nextty }\n",
    )
    .unwrap();
    fs::set_permissions(root.join("bin/ssh"), fs::Permissions::from_mode(0o755)).unwrap();
    let over_ssh = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nextty"));
        environment(command.args(args));
        let path = format!("{}:/usr/bin:/bin", root.join("bin").display());
        let command = command.env("NEXTTY", env!("CARGO_BIN_EXE_nextty"));
        let output = command.env("PATH", path).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    assert_eq!(over_ssh(&["list", "user@host"]), "No workspaces.\n");
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

    let mut bridge = Command::new(env!("CARGO_BIN_EXE_nextty"));
    environment(bridge.args(["--bridge", "new"]));
    let mut frontend = Client::connect_remote(bridge, "test".into()).unwrap();
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

    assert!(over_ssh(&["list", "user@host"]).starts_with("bridged\t1 tabs\t1 panes"));
    over_ssh(&["terminate", "user@host", "bridged"]);
    drop(frontend);
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
