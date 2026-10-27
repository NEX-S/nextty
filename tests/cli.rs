#![cfg(feature = "tui")]

use std::{
    fs,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, MetadataExt},
            net::UnixListener,
        },
    },
    process::{Command, Stdio},
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
        (vec!["new", "ssh://host/path", "--tui"], "remote target"),
        (vec!["--bridge", "0"], "bridge"),
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
fn versioned_server_coexists_with_legacy_lock_and_rejects_duplicate() {
    let directory = std::env::temp_dir().join(format!("nextty-startup-{}", std::process::id()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .unwrap();
    let legacy_lock = fs::File::create(directory.join("server.lock")).unwrap();
    assert_eq!(
        unsafe { libc::flock(legacy_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let legacy_socket = directory.join("server.sock");
    let legacy_listener = UnixListener::bind(&legacy_socket).unwrap();
    let socket = directory.join("server-next.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_nextty"))
        .arg("--server")
        .arg(&socket)
        .env("XDG_CACHE_HOME", directory.join("cache"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let ready = loop {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            break true;
        }
        if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let duplicate = ready.then(|| {
        let mut process = Command::new(env!("CARGO_BIN_EXE_nextty"))
            .arg("--server")
            .arg(&socket)
            .env("XDG_CACHE_HOME", directory.join("cache"))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let exited = loop {
            if process.try_wait().unwrap().is_some() {
                break true;
            }
            if Instant::now() >= deadline {
                process.kill().unwrap();
                break false;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        (exited, process.wait_with_output().unwrap())
    });
    let current_socket_preserved = std::os::unix::net::UnixStream::connect(&socket).is_ok();
    let legacy_socket_preserved = std::os::unix::net::UnixStream::connect(&legacy_socket).is_ok();
    let legacy_lock_preserved = fs::metadata(directory.join("server.lock")).unwrap().ino()
        == legacy_lock.metadata().unwrap().ino();
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    drop(legacy_listener);
    drop(legacy_lock);
    fs::remove_dir_all(directory).unwrap();

    assert!(ready, "server did not start: {:?}", output);
    let (exited, duplicate) = duplicate.unwrap();
    assert!(exited, "duplicate server did not reject the lock");
    assert!(!duplicate.status.success());
    assert!(current_socket_preserved);
    assert!(legacy_socket_preserved);
    assert!(legacy_lock_preserved);
}
