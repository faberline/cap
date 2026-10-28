// SPEC-MANAGED: tech-design/semantic/cap-src.md#schema
// CODEGEN-BEGIN
//! Command supervision with signal forwarding, logging, and explicit timeouts.
//!
//! The CLI uses local supervision. Library clients can also use the
//! Acquire → Spawned → wait → Release protocol with a compatibility daemon.
//!
//! Normal shell commands run locally without a daemon or resource sampling.
//! The daemon client helper remains available for compatibility.

use anyhow::{bail, Result};

use crate::client::Client;
use crate::protocol::{AcquireRequest, KillEnvelope, Request, Response};
use crate::supervisor::{self, SpawnOpts, SpawnSpec};

/// Result of a supervised run.
/// @spec tech-design/semantic/cap-src.md#schema
pub struct ManagedOutcome {
    /// The child's final exit status.
    pub status: std::process::ExitStatus,
    /// Present if cap stopped the child for an explicit timeout.
    /// Read `classification` / `action` to decide whether to retry.
    pub kill_envelope: Option<KillEnvelope>,
}

/// Run locally. Old daemons and retired resource settings cannot affect this
/// command. Only positive timeout flags enable the periodic supervision loop.
pub async fn run_direct(
    spec: SpawnSpec,
    label: Option<String>,
    timeout_secs: Option<u64>,
    idle_timeout_secs: Option<u64>,
) -> Result<ManagedOutcome> {
    use std::time::Duration;

    use crate::config::Config;
    use crate::eventlog::EventLog;
    use crate::sampler::CpuSampler;
    use crate::throttle::Throttle;

    let cfg = Config::load().unwrap_or_else(|error| {
        eprintln!("cap: cannot read config ({error:#}); using defaults");
        Config::default()
    });
    let interval = Duration::from_millis(cfg.protect.sample_interval_ms.max(50));
    let eventlog = EventLog::new(cfg.log.enabled);
    let tracker = Throttle::new(cfg, 0.0, 0.0, 0.0)?;
    let duration = |secs: Option<u64>| secs.filter(|s| *s > 0).map(Duration::from_secs);
    let timeout = duration(timeout_secs);
    let idle_timeout = duration(idle_timeout_secs);
    let label = label.unwrap_or_else(|| {
        std::iter::once(spec.program.as_str())
            .chain(spec.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    });
    let cwd = spec.cwd.clone().or_else(|| std::env::current_dir().ok());
    let lease = tracker
        .register(
            std::process::id() as i32,
            crate::hook::effective_program(&spec.program, &spec.args),
            label,
            cwd.map(|path| path.display().to_string())
                .unwrap_or_default(),
            timeout,
            idle_timeout,
        )
        .await;
    let running = supervisor::spawn(&spec, SpawnOpts { nice: 0 })?;
    let pid = running.pid;
    let forwarder = spawn_signal_forwarder(pid);
    tracker.attach_pid(lease, pid).await;
    let status = if timeout.is_some() || idle_timeout.is_some() {
        // CPU sampling serves only an explicitly requested idle timeout.
        // Memory use and host load never affect command execution.
        let mut cpu = idle_timeout.map(|_| CpuSampler::new());
        if let Some(cpu) = &mut cpu {
            cpu.cpu_usage(&[pid]);
        }
        let mut timer = tokio::time::interval(interval);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        timer.tick().await;
        let waiting = running.wait();
        tokio::pin!(waiting);
        loop {
            tokio::select! {
                status = &mut waiting => break status,
                _ = timer.tick() => {
                    if let Some(cpu) = &mut cpu {
                        tracker.note_cpu_usage(&cpu.cpu_usage(&[pid])).await;
                    }
                    tracker.tick(f64::NAN, 0.0, &|_| None).await;
                }
            }
        }
    } else {
        running.wait().await
    };
    forwarder.abort();
    let status = status?;
    let outcome = tracker.release(lease, status.code()).await;
    if let Some(record) = &outcome.record {
        eventlog.append(record);
    }
    Ok(ManagedOutcome {
        status,
        kill_envelope: outcome.kill_envelope,
    })
}

/// Run `spec` with a compatibility lease on an already-connected `client`.
///
/// `label` is the human/agent-facing name recorded in the lease (e.g. the
/// original command line, or another tool's workload id). The full lease dance:
///
/// 1. `Acquire` — register intent and receive a lease id.
/// 2. spawn the child in its own process group.
/// 3. `Spawned` — tell the daemon the PID so it can target it.
/// 4. wait (the daemon can enforce the requested timeouts).
/// 5. `Release` — report exit; receive a kill envelope iff we were killed.
/// @spec tech-design/semantic/cap-src.md#schema
pub async fn managed_run(
    client: &mut Client,
    spec: SpawnSpec,
    label: Option<String>,
    timeout_secs: Option<u64>,
    idle_timeout_secs: Option<u64>,
) -> Result<ManagedOutcome> {
    let cwd = spec
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    // 1. Register intent.
    let acquire = Request::Acquire(AcquireRequest {
        program: spec.program.clone(),
        args: spec.args.clone(),
        cwd,
        label,
        client_pid: std::process::id() as i32,
        timeout_secs,
        idle_timeout_secs,
    });
    let (lease, nice) = match client.request(&acquire).await? {
        Response::Lease { lease, nice } => (lease, nice),
        Response::Error { message } => bail!("cap daemon: {message}"),
        other => bail!("unexpected daemon response to Acquire: {other:?}"),
    };

    // 2. Spawn the child in its own process group.
    let running = supervisor::spawn(&spec, SpawnOpts { nice })?;
    let child_pid = running.pid;

    // 2a. Forward terminal-driven signals to the child's group. The supervisor
    //     remapped the process group so the throttler can target the subtree;
    //     the side effect is the terminal no longer delivers Ctrl-C — restore
    //     that by hand.
    let forwarder = spawn_signal_forwarder(child_pid);

    // 3. Tell the daemon the PID.
    match client
        .request(&Request::Spawned { lease, child_pid })
        .await?
    {
        Response::SpawnedAck => {}
        Response::Error { message } => eprintln!("cap: warn: daemon rejected Spawned: {message}"),
        other => eprintln!("cap: warn: unexpected response to Spawned: {other:?}"),
    }

    // 4. Wait for the child; the daemon can enforce requested timeouts.
    let status = running.wait().await?;
    forwarder.abort();

    // 5. Release and collect the kill envelope (if any).
    let kill_envelope = match client
        .request(&Request::Release {
            lease,
            exit_code: status.code(),
        })
        .await
    {
        Ok(Response::Released { kill_envelope, .. }) => kill_envelope,
        _ => None,
    };

    Ok(ManagedOutcome {
        status,
        kill_envelope,
    })
}
// CODEGEN-END

/// Forward SIGINT/SIGTERM/SIGHUP to the child's process group so an
/// interactive Ctrl-C still reaches it despite the process-group remap.
fn spawn_signal_forwarder(child_pid: i32) -> tokio::task::JoinHandle<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let signals = (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    );
    tokio::spawn(async move {
        let (mut sigint, mut sigterm, mut sighup) = match signals {
            (Ok(a), Ok(b), Ok(c)) => (a, b, c),
            _ => return,
        };
        loop {
            let sig = tokio::select! {
                _ = sigint.recv()  => libc::SIGINT,
                _ = sigterm.recv() => libc::SIGTERM,
                _ = sighup.recv()  => libc::SIGHUP,
            };
            unsafe {
                libc::kill(-child_pid, sig);
            }
        }
    })
}
