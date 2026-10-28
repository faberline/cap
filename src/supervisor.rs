// SPEC-MANAGED: tech-design/semantic/cap-src.md#schema
// CODEGEN-BEGIN
//! Spawn a child command and report its PID before waiting.
//!
//! Children keep the caller's process priority. Each child leads its own
//! process group so terminal signals and explicit timeouts reach its subtree.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use tokio::process::{Child, Command};

/// @spec tech-design/semantic/cap-src.md#schema
pub struct SpawnOpts {
    /// Retired option. The child always keeps the caller's priority.
    pub nice: i32,
}

/// Everything needed to launch the child. Generalized from "(program, args)"
/// so callers that wrap the command or need a cwd / extra env can express it.
/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Debug, Clone, Default)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Working directory for the child (default: inherit caller's cwd).
    pub cwd: Option<PathBuf>,
    /// Extra environment variables layered onto the inherited environment.
    pub env: BTreeMap<String, String>,
}

/// @spec tech-design/semantic/cap-src.md#schema
impl SpawnSpec {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        SpawnSpec {
            program: program.into(),
            args,
            cwd: None,
            env: BTreeMap::new(),
        }
    }
}

/// A spawned-but-not-yet-waited child. Callers must read `pid` first
/// (to register it with the daemon), then call `wait()`.
/// @spec tech-design/semantic/cap-src.md#schema
pub struct RunningChild {
    pub pid: i32,
    child: Child,
}

/// @spec tech-design/semantic/cap-src.md#schema
impl RunningChild {
    pub async fn wait(mut self) -> Result<std::process::ExitStatus> {
        self.child.wait().await.context("waiting for child to exit")
    }
}

/// Spawn from a [`SpawnSpec`], inheriting stdio, in its own process group.
/// @spec tech-design/semantic/cap-src.md#schema
pub fn spawn(spec: &SpawnSpec, _opts: SpawnOpts) -> Result<RunningChild> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(dir) = &spec.cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    unsafe {
        cmd.pre_exec(move || {
            // Keep signal forwarding and explicit timeouts within this subtree.
            if libc::setpgid(0, 0) != 0 {
                let _ = std::io::Error::last_os_error();
            }
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", spec.program))?;
    let pid = child.id().ok_or_else(|| anyhow!("spawn produced no PID"))? as i32;
    Ok(RunningChild { pid, child })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn legacy_nice_option_does_not_change_child_priority() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("priority");
        let parent_priority = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        let mut spec = SpawnSpec::new(
            std::env::current_exe().unwrap().display().to_string(),
            vec![
                "--exact".into(),
                "supervisor::tests::child_priority_probe".into(),
            ],
        );
        spec.env.insert(
            "CAP_TEST_PRIORITY_FILE".into(),
            output.display().to_string(),
        );
        let status = spawn(&spec, SpawnOpts { nice: 19 })
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert!(status.success());
        let priority: i32 = std::fs::read_to_string(output)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(priority, parent_priority);
    }

    #[test]
    fn child_priority_probe() {
        if let Ok(output) = std::env::var("CAP_TEST_PRIORITY_FILE") {
            let priority = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
            std::fs::write(output, priority.to_string()).unwrap();
        }
    }
}
// CODEGEN-END
