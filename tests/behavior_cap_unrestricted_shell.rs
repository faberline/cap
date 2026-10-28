//! Public shell behavior after retirement of the resource governor.

use std::fs;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn cap(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cap-full"));
    command
        .env("CAP_HOME", home)
        .env("CAP_LOG", "error")
        .env_remove("CAP_SESSION_ID");
    command
}

fn legacy_config(home: &std::path::Path) {
    fs::write(
        home.join("config.toml"),
        "[protect]\nmin_free_gb = 1000000.0\npause_used_percent = 0\n\
         kill_used_percent = 0\npause_load_percent = 1\nreap_enabled = true\n\
         default_timeout_secs = 1\ndefault_idle_timeout_secs = 1\n\
         sample_interval_ms = 50\nkill_grace_secs = 0\n[defaults]\nnice = 19\n",
    )
    .unwrap();
}

fn finish(command: &mut Command) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!("cap did not finish: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn direct_commands_ignore_legacy_limits_and_do_not_contact_old_daemon() {
    let home = tempfile::tempdir().unwrap();
    legacy_config(home.path());
    let listener = UnixListener::bind(home.path().join("cap.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    fs::write(home.path().join("cap.pid"), std::process::id().to_string()).unwrap();

    for args in [
        vec!["run", "--", "sh", "-c", "sleep 1.2; printf 'ok\\n'; exit 7"],
        vec!["run", "sleep 1.2; printf 'ok\\n'; exit 7"],
        vec![
            "run",
            "--timeout",
            "0",
            "--idle-timeout",
            "0",
            "--",
            "sh",
            "-c",
            "sleep 1.2; printf 'ok\\n'; exit 7",
        ],
    ] {
        let output = finish(cap(home.path()).args(args));
        assert_eq!(output.status.code(), Some(7), "{output:?}");
        assert_eq!(output.stdout, b"ok\n");
        assert!(output.stderr.is_empty(), "{output:?}");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    assert!(!home.path().join("cap.lock").exists());
}

#[test]
fn direct_commands_preserve_priority_stdio_cwd_and_exit_code() {
    let home = tempfile::tempdir().unwrap();
    legacy_config(home.path());
    let expected = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    let priority = finish(
        cap(home.path())
            .env("CAP_EXPECT_PRIORITY", expected.to_string())
            .arg("run")
            .arg("--")
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "inherited_priority_probe", "--nocapture"]),
    );
    assert!(priority.status.success(), "{priority:?}");

    let mut child = cap(home.path())
        .args([
            "run",
            "--",
            "sh",
            "-c",
            "read -r line; printf '%s' \"$line\"; pwd; printf problem >&2; exit 9",
        ])
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"input:\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(9));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("input:{}\n", home.path().canonicalize().unwrap().display())
    );
    assert_eq!(output.stderr, b"problem");
    assert!(!home.path().join("cap.pid").exists());
    assert!(!home.path().join("cap.sock").exists());

    let log = fs::read_dir(home.path().join("logs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let records: Vec<serde_json::Value> = fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(records.iter().any(|record| record["exit_code"] == 9));
    assert!(records.iter().all(|record| record["paused_ms"] == 0));
}

#[test]
fn inherited_priority_probe() {
    if let Ok(expected) = std::env::var("CAP_EXPECT_PRIORITY") {
        let actual = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        assert_eq!(actual, expected.parse::<i32>().unwrap());
    }
}

#[test]
fn direct_commands_receive_forwarded_terminal_signals() {
    let home = tempfile::tempdir().unwrap();
    let ready = home.path().join("ready");
    let mut child = cap(home.path())
        .args([
            "run",
            "--",
            "sh",
            "-c",
            "trap 'exit 27' TERM; printf ready > \"$1\"; while :; do sleep 1; done",
            "sh",
        ])
        .arg(&ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("command did not start");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(27));
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("command did not receive SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn explicit_timeouts_work_without_contacting_old_daemon() {
    for flag in ["--timeout", "--idle-timeout"] {
        let home = tempfile::tempdir().unwrap();
        legacy_config(home.path());
        let listener = UnixListener::bind(home.path().join("cap.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let start = Instant::now();
        let output = finish(cap(home.path()).args([
            "run",
            flag,
            "1",
            "--",
            "sh",
            "-c",
            "trap '' TERM; sleep 30",
        ]));
        assert!(!output.status.success(), "{flag}: {output:?}");
        assert!(start.elapsed() < Duration::from_secs(5), "{flag}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("timeout"), "{flag}: {stderr}");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(!home.path().join("cap.pid").exists());
    }
}

#[test]
fn wait_is_immediate_and_does_not_start_daemon() {
    let home = tempfile::tempdir().unwrap();
    legacy_config(home.path());
    let output = finish(cap(home.path()).args(["wait", "--timeout", "30"]));
    assert!(output.status.success(), "{output:?}");
    assert!(!home.path().join("cap.pid").exists());
    assert!(!home.path().join("cap.sock").exists());
}

#[test]
fn run_log_failure_preserves_command_output() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("logs"), "not a directory").unwrap();
    let output = finish(cap(home.path()).env_remove("CAP_LOG").args([
        "run",
        "--",
        "sh",
        "-c",
        "printf out; printf err >&2; exit 4",
    ]));
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(output.stdout, b"out");
    assert_eq!(output.stderr, b"err");
}

#[test]
fn native_execution_and_shell_fallback_preserve_output() {
    let home = tempfile::tempdir().unwrap();
    let native = finish(cap(home.path()).args(["run", "printf 'alpha\\nbeta\\n' | wc -l"]));
    assert!(native.status.success(), "{native:?}");
    assert_eq!(String::from_utf8_lossy(&native.stdout).trim(), "2");
    let fallback = finish(cap(home.path()).args(["run", "value=hello; printf '%s\\n' \"$value\""]));
    assert!(fallback.status.success(), "{fallback:?}");
    assert_eq!(fallback.stdout, b"hello\n");
    assert!(!home.path().join("cap.pid").exists());
}

#[test]
fn compatibility_daemon_accepts_legacy_config_without_resource_limits() {
    use cap::protocol::{AcquireRequest, Request, Response};
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;

    struct Daemon(std::process::Child);
    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let home = tempfile::tempdir().unwrap();
    legacy_config(home.path());
    let mut daemon = Daemon(
        cap(home.path())
            .args(["daemon", "run"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    let stream = loop {
        if let Ok(stream) = UnixStream::connect(home.path().join("cap.sock")) {
            break stream;
        }
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon rejected legacy config"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut stream = BufReader::new(stream);
    let mut request = |request: Request| -> Response {
        let mut data = serde_json::to_vec(&request).unwrap();
        data.push(b'\n');
        stream.get_mut().write_all(&data).unwrap();
        let mut line = String::new();
        stream.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    assert!(matches!(
        request(Request::Acquire(AcquireRequest {
            program: "sh".into(),
            args: vec![],
            cwd: home.path().display().to_string(),
            label: None,
            client_pid: std::process::id() as i32,
            timeout_secs: Some(0),
            idle_timeout_secs: Some(0),
        })),
        Response::Lease { nice: 0, .. }
    ));
    assert!(matches!(
        request(Request::WaitForCapacity {
            timeout_secs: Some(0)
        }),
        Response::CapacityOk
    ));
    let Response::Status(status) = request(Request::Status) else {
        panic!("missing status")
    };
    assert_eq!(status.pause_floor_gb, 0.0);
    assert_eq!(status.kill_floor_gb, 0.0);
    assert_eq!(status.load_pause_floor, 0.0);
    assert!(matches!(request(Request::Shutdown), Response::ShuttingDown));
    assert!(daemon.0.wait().unwrap().success());
}
