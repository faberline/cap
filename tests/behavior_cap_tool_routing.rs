//! Black-box contract for the first native tool-routing family.

use std::{
    fs::{self, File},
    io::{Seek, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};

struct Installed {
    _root: TempDir,
    cap: PathBuf,
    full: PathBuf,
}

fn installed() -> &'static Installed {
    static BINARIES: OnceLock<Installed> = OnceLock::new();
    BINARIES.get_or_init(|| {
        let root = tempdir().unwrap();
        let cap = root.path().join("cap");
        let fast = root.path().join("cap-fast");
        let full = root.path().join("cap-full");
        fs::copy(env!("CARGO_BIN_EXE_cap-full"), &full).unwrap();
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let strip = if cfg!(target_os = "macos") {
            "-Wl,-dead_strip"
        } else {
            "-Wl,--gc-sections"
        };
        let flags = ["-Oz", "-ffunction-sections", "-fdata-sections", strip];
        compile(&manifest.join("src/cap_fast_frontend.c"), &fast, &flags);
        let mut tiny_flags = flags.to_vec();
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            tiny_flags.extend([
                "-ffreestanding",
                "-fno-builtin",
                "-nostartfiles",
                "-Wl,-e,_start",
            ]);
        }
        compile(&manifest.join("src/cap_frontend.c"), &cap, &tiny_flags);
        if cfg!(target_os = "macos") {
            for (binary, runtime) in [(&cap, true), (&fast, true), (&full, false)] {
                let mut command = Command::new("codesign");
                command.args(["-s", "-", "-f"]);
                if runtime {
                    command.args(["--options", "runtime"]);
                }
                assert!(command.arg(binary).output().unwrap().status.success());
            }
        }
        Installed {
            _root: root,
            cap,
            full,
        }
    })
}

fn compile(source: &Path, binary: &Path, flags: &[&str]) {
    let output = Command::new(std::env::var("CC").unwrap_or_else(|_| "/usr/bin/cc".into()))
        .args(flags)
        .arg(source)
        .arg("-o")
        .arg(binary)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "compile {}: {}",
        source.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn command(binary: &Path, root: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .current_dir(root)
        .env("CAP_HOME", root.join("cap-home"))
        .env("CAP_LOG", "error")
        .env("LC_ALL", "C")
        .env_remove("CAP_SESSION_ID")
        .env_remove("CAP_PUBLIC_EXE");
    command
}

fn finish(command: &mut Command, input: Option<&[u8]>) -> Output {
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("command timed out: {:?}", child.wait_with_output().unwrap());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn shell_words(words: &[String]) -> String {
    words
        .iter()
        .map(|word| quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    Argv,
    RunArgv,
    RunString,
}

fn invoke(
    binary: &Path,
    root: &Path,
    words: &[String],
    entry: Entry,
    trace: bool,
    input: &[u8],
    path: Option<&str>,
) -> Output {
    let mut cmd = command(binary, root);
    if trace {
        cmd.arg("--trace-routing");
    }
    match entry {
        Entry::Argv => {
            cmd.args(words);
        }
        Entry::RunArgv => {
            cmd.args(["run", "--"]).args(words);
        }
        Entry::RunString => {
            cmd.arg("run").arg(shell_words(words));
        }
    }
    if let Some(path) = path {
        cmd.env("PATH", path);
    }
    finish(&mut cmd, Some(input))
}

fn route(output: &Output) -> (Value, &[u8]) {
    let newline = output
        .stderr
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or_else(|| panic!("missing route JSON: {output:?}"));
    let route: Value = serde_json::from_slice(&output.stderr[..newline])
        .unwrap_or_else(|error| panic!("invalid route JSON ({error}): {output:?}"));
    assert!(
        route["reason"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "{route}"
    );
    for name in ["tool", "backend", "implementation", "reason"] {
        assert!(route.get(name).is_some(), "missing {name}: {route}");
    }
    (route, &output.stderr[newline + 1..])
}

fn explain(binary: &Path, root: &Path, words: &[String], entry: Entry) -> Value {
    let mut cmd = command(binary, root);
    cmd.args(["explain", "--json", "--"]);
    match entry {
        Entry::RunString => {
            cmd.arg(shell_words(words));
        }
        Entry::Argv | Entry::RunArgv => {
            cmd.args(words);
        }
    }
    let output = finish(&mut cmd, Some(b"must not become command output\n"));
    assert!(output.status.success(), "explain failed: {output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("invalid explanation ({error}): {output:?}"))
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn poison_path(root: &Path) -> String {
    let bin = root.join("poison");
    fs::create_dir(&bin).unwrap();
    for name in ["cat", "wc"] {
        script(
            &bin.join(name),
            "printf 'system tool was used\\n' >&2; exit 91",
        );
    }
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn tools(path: Option<&Path>) -> Vec<(String, Vec<String>)> {
    let operand = path.map(|path| path.display().to_string());
    [
        ("cat", vec!["cat".into()]),
        ("wc-lines", vec!["wc".into(), "-l".into()]),
        ("wc-bytes", vec!["wc".into(), "-c".into()]),
    ]
    .into_iter()
    .map(|(tool, mut words)| {
        if let Some(path) = &operand {
            words.push(path.clone());
        }
        (tool.into(), words)
    })
    .collect()
}

fn oracle(words: &[String], input: &[u8]) -> Output {
    let program = if words[0] == "cat" {
        "/bin/cat"
    } else {
        "/usr/bin/wc"
    };
    finish(
        Command::new(program).args(&words[1..]).env("LC_ALL", "C"),
        Some(input),
    )
}

#[test]
fn pilot_native_routes_and_explanations_agree() {
    let installed = installed();
    let root = tempdir().unwrap();
    let file = root.path().join("input with spaces.bin");
    fs::write(&file, b"alpha\nbeta\n").unwrap();
    let path = poison_path(root.path());
    for binary in [&installed.cap, &installed.full] {
        for (tool, words) in tools(Some(&file)) {
            let expected = oracle(&words, b"");
            for entry in [Entry::Argv, Entry::RunArgv, Entry::RunString] {
                let output = invoke(binary, root.path(), &words, entry, true, b"", Some(&path));
                assert_eq!(
                    output.status.code(),
                    expected.status.code(),
                    "{} {entry:?}: {output:?}",
                    binary.display()
                );
                assert_eq!(output.stdout, expected.stdout, "{tool} {entry:?}");
                let (selected, stderr) = route(&output);
                assert_eq!(stderr, expected.stderr, "{tool} {entry:?}");
                assert_eq!(selected["tool"], tool, "{selected}");
                assert_eq!(selected["implementation"], "native", "{selected}");
                assert!(
                    matches!(
                        selected["backend"].as_str(),
                        Some("c-tiny" | "c-fast" | "rust-native")
                    ),
                    "{selected}"
                );
                if binary == &installed.full {
                    assert_eq!(selected["backend"], "rust-native");
                } else if !matches!(entry, Entry::RunArgv) {
                    let expected_backend = if tool == "cat" { "c-tiny" } else { "c-fast" };
                    assert_eq!(selected["backend"], expected_backend, "{selected}");
                }
                if !matches!(entry, Entry::RunArgv) {
                    let explanation = explain(binary, root.path(), &words, entry);
                    for field in ["tool", "backend", "implementation"] {
                        assert_eq!(
                            selected[field], explanation[field],
                            "{field}: {selected} vs {explanation}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn native_pilot_preserves_bytes_without_trace() {
    let installed = installed();
    let root = tempdir().unwrap();
    let file = root.path().join("raw input with spaces");
    let path = poison_path(root.path());
    for data in [
        b"".as_slice(),
        b"one\ntwo",
        b"a\0b\n\xff",
        "line\n中文\n".as_bytes(),
    ] {
        fs::write(&file, data).unwrap();
        for operand in [None, Some(file.as_path())] {
            for (_tool, words) in tools(operand) {
                let input = if operand.is_none() { data } else { b"" };
                let expected = oracle(&words, input);
                for binary in [&installed.cap, &installed.full] {
                    let output = invoke(
                        binary,
                        root.path(),
                        &words,
                        Entry::Argv,
                        false,
                        input,
                        Some(&path),
                    );
                    assert_eq!(
                        output.status.code(),
                        expected.status.code(),
                        "{words:?}: {output:?}"
                    );
                    assert_eq!(output.stdout, expected.stdout, "{words:?}");
                    assert_eq!(output.stderr, expected.stderr, "{words:?}");
                }
            }
        }
    }
}

#[test]
fn late_unsupported_cat_flag_falls_back_once_without_partial_output() {
    let installed = installed();
    for binary in [&installed.cap, &installed.full] {
        for entry in [Entry::Argv, Entry::RunArgv, Entry::RunString] {
            let root = tempdir().unwrap();
            let bin = root.path().join("bin");
            fs::create_dir(&bin).unwrap();
            let file = root.path().join("file with spaces");
            fs::write(&file, b"partial native output must not appear\n").unwrap();
            let calls = root.path().join("calls");
            let args = root.path().join("args");
            script(&bin.join("cat"), &format!("printf 'call\\n' >> {}\nprintf '%s\\000' \"$@\" > {}\nprintf 'fallback:'\nexec /bin/cat", quote(&calls.display().to_string()), quote(&args.display().to_string())));
            let path = format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let words = vec!["cat".into(), file.display().to_string(), "-n".into()];
            let output = invoke(
                binary,
                root.path(),
                &words,
                entry,
                false,
                b"unchanged stdin\n",
                Some(&path),
            );
            assert!(
                output.status.success(),
                "{} {entry:?}: {output:?}",
                binary.display()
            );
            assert_eq!(output.stdout, b"fallback:unchanged stdin\n", "{entry:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            assert_eq!(fs::read(&calls).unwrap(), b"call\n");
            assert_eq!(
                fs::read(&args).unwrap(),
                format!("{}\0-n\0", file.display()).as_bytes()
            );
        }
    }
}

#[test]
fn external_exit_127_is_not_retried() {
    let installed = installed();
    for binary in [&installed.cap, &installed.full] {
        for entry in [Entry::Argv, Entry::RunArgv, Entry::RunString] {
            let root = tempdir().unwrap();
            let program = root.path().join("returns127");
            let calls = root.path().join("calls");
            script(
                &program,
                &format!(
                    "printf 'call\\n' >> {}\nprintf out\nprintf 'err:%s' \"$1\" >&2\nexit 127",
                    quote(&calls.display().to_string())
                ),
            );
            let words = vec![program.display().to_string(), "--trace-routing".into()];
            for trace in [false, true] {
                let output = invoke(binary, root.path(), &words, entry, trace, b"", None);
                assert_eq!(output.status.code(), Some(127), "{entry:?}: {output:?}");
                assert_eq!(output.stdout, b"out");
                let stderr = if trace {
                    let (route, stderr) = route(&output);
                    assert!(route["tool"].is_null(), "{route}");
                    assert!(
                        matches!(route["backend"].as_str(), Some("original" | "bash")),
                        "{route}"
                    );
                    stderr
                } else {
                    output.stderr.as_slice()
                };
                assert_eq!(stderr, b"err:--trace-routing");
                assert_eq!(fs::read(&calls).unwrap(), b"call\n");
                fs::remove_file(&calls).unwrap();
            }
        }
    }
}

#[test]
fn explain_does_not_read_input_or_execute_commands() {
    let installed = installed();
    for binary in [&installed.cap, &installed.full] {
        let root = tempdir().unwrap();
        let input_path = root.path().join("stdin");
        fs::write(&input_path, b"unread input\n").unwrap();
        let mut input = File::open(&input_path).unwrap();
        let output = finish(
            command(binary, root.path())
                .args(["explain", "--json", "--", "cat"])
                .stdin(Stdio::from(input.try_clone().unwrap())),
            None,
        );
        assert!(output.status.success(), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["tool"], "cat");
        assert_eq!(
            input.stream_position().unwrap(),
            0,
            "explain consumed stdin"
        );
        assert!(output.stderr.is_empty(), "{output:?}");
        let marker = root.path().join("must not exist");
        let words = vec![
            "sh".into(),
            "-c".into(),
            format!("printf changed > {}", quote(&marker.display().to_string())),
        ];
        explain(binary, root.path(), &words, Entry::Argv);
        assert!(!marker.exists(), "explain executed the original command");
    }
}

#[test]
fn hook_generated_commands_reach_native_tools() {
    let installed = installed();
    let root = tempdir().unwrap();
    let file = root.path().join("hook input with spaces");
    fs::write(&file, b"a\nb\n").unwrap();
    let path = poison_path(root.path());
    for binary in [&installed.cap, &installed.full] {
        for agent in ["--claude-code", "--codex"] {
            for (tool, words) in tools(Some(&file)) {
                let payload =
                    json!({"tool_name":"Bash", "tool_input":{"command":shell_words(&words)}});
                let output = finish(
                    command(binary, root.path()).args(["hook", "bash", agent]),
                    Some(payload.to_string().as_bytes()),
                );
                assert!(output.status.success(), "{output:?}");
                let hook: Value = serde_json::from_slice(&output.stdout).unwrap();
                let key = if agent == "--codex" {
                    "updatedInput"
                } else {
                    "modifiedInput"
                };
                let rewritten = hook["hookSpecificOutput"][key]["command"].as_str().unwrap();
                let expected = oracle(&words, b"");
                let actual = finish(
                    command(Path::new("/bin/bash"), root.path())
                        .args(["-c", rewritten])
                        .env("PATH", &path),
                    Some(b""),
                );
                assert_eq!(
                    actual.status.code(),
                    expected.status.code(),
                    "{rewritten}: {actual:?}"
                );
                assert_eq!(actual.stdout, expected.stdout);
                assert_eq!(actual.stderr, expected.stderr);

                let prefix = binary.display().to_string();
                assert!(rewritten.starts_with(&prefix), "{rewritten}");
                let traced = format!("{prefix} --trace-routing{}", &rewritten[prefix.len()..]);
                let output = finish(
                    command(Path::new("/bin/bash"), root.path())
                        .args(["-c", &traced])
                        .env("PATH", &path),
                    Some(b""),
                );
                assert_eq!(output.status.code(), expected.status.code(), "{output:?}");
                assert_eq!(output.stdout, expected.stdout);
                let (route, stderr) = route(&output);
                assert_eq!(route["tool"], tool);
                assert_eq!(route["implementation"], "native");
                assert_eq!(stderr, expected.stderr);
            }
        }
    }
}

#[test]
fn unsupported_shell_string_falls_back_once() {
    let installed = installed();
    for binary in [&installed.cap, &installed.full] {
        let root = tempdir().unwrap();
        let marker = root.path().join("calls");
        let text = format!(
            "printf 'call\\n' >> {}; printf '%s' \"$CAP_ROUTE_VALUE\"",
            quote(&marker.display().to_string())
        );
        let output = finish(
            command(binary, root.path())
                .args(["--trace-routing", "run", &text])
                .env("CAP_ROUTE_VALUE", "preserved"),
            Some(b""),
        );
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"preserved");
        let (route, stderr) = route(&output);
        assert_eq!(route["backend"], "bash");
        assert_eq!(route["implementation"], "bash");
        assert!(stderr.is_empty());
        assert_eq!(fs::read(&marker).unwrap(), b"call\n");
    }
}

#[test]
fn stdin_cat_output_failure_preserves_nonzero_exit() {
    fn run_with_readonly_stdout(mut command: Command, output_file: &Path) -> Output {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::from(File::open(output_file).unwrap()))
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"stdin must fail to write\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!(
                    "stdout-failure command timed out: {:?}",
                    child.wait_with_output().unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    let installed = installed();
    let root = tempdir().unwrap();
    let output_file = root.path().join("read-only stdout");
    let unchanged = b"output file must not change\n";
    fs::write(&output_file, unchanged).unwrap();
    let original =
        run_with_readonly_stdout(command(Path::new("/bin/cat"), root.path()), &output_file);
    assert_eq!(original.status.code(), Some(1), "{original:?}");
    let mut mismatches = Vec::new();
    for binary in [&installed.cap, &installed.full] {
        for entry in [Entry::Argv, Entry::RunString] {
            let mut cmd = command(binary, root.path());
            match entry {
                Entry::Argv => {
                    cmd.arg("cat");
                }
                Entry::RunString => {
                    cmd.args(["run", "cat"]);
                }
                Entry::RunArgv => unreachable!(),
            }
            let actual = run_with_readonly_stdout(cmd, &output_file);
            if actual.status.code() != original.status.code() {
                mismatches.push(format!(
                    "{} {entry:?}: expected {:?}, got {:?}; stderr={:?}",
                    binary.display(),
                    original.status.code(),
                    actual.status.code(),
                    String::from_utf8_lossy(&actual.stderr)
                ));
            }
            assert_eq!(fs::read(&output_file).unwrap(), unchanged);
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
