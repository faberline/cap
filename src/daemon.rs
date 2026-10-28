// SPEC-MANAGED: tech-design/semantic/cap-src.md#schema
// CODEGEN-BEGIN
//! Compatibility daemon for command leases, logs, and explicit timeouts.
//!
//! Memory and CPU measurements are telemetry only. Commands are admitted
//! immediately. The daemon never pauses commands or reaps other processes.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::config::Config;
use crate::eventlog::EventLog;
use crate::paths;
use crate::protocol::{LeaseId, Request, Response};
use crate::sampler::{CpuSampler, LoadSampler, MemorySampler, RssSampler};
use crate::throttle::{Throttle, TickAction};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Hold an exclusive `flock` on `~/.cap/cap.lock` for the lifetime
/// of the daemon. Drop = release. If another daemon already holds it,
/// `acquire_singleton_lock` returns Ok(None) so the caller can exit
/// cleanly without disturbing the live daemon's socket / pid file.
struct SingletonLock {
    // Held only for its Drop — closing the fd releases the flock.
    _file: std::fs::File,
}

fn acquire_singleton_lock() -> Result<Option<SingletonLock>> {
    use std::os::unix::io::AsRawFd;
    paths::ensure_home()?;
    let path = paths::lock_path()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        // The lockfile is purely an flock handle — we never write to it,
        // so keep any existing content rather than truncating.
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening lockfile {}", path.display()))?;
    // LOCK_NB makes flock return EWOULDBLOCK instead of waiting when
    // another process holds the lock — we want to exit, not queue.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(Some(SingletonLock { _file: file }))
    } else {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(err).with_context(|| format!("flock {}", path.display()))
        }
    }
}

/// @spec tech-design/semantic/cap-src.md#schema
pub async fn run_foreground() -> Result<()> {
    let _singleton = match acquire_singleton_lock()? {
        Some(l) => l,
        None => {
            tracing::info!("another cap daemon is already running; exiting");
            return Ok(());
        }
    };
    let cfg = Config::load()?;
    let eventlog = Arc::new(EventLog::new(cfg.log.enabled));
    let throttle = Throttle::new(cfg, 0.0, 0.0, 0.0)?;
    let socket = paths::socket_path()?;
    paths::ensure_home()?;
    let _ = std::fs::remove_file(&socket);

    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("binding UDS at {}", socket.display()))?;
    tracing::info!(socket = %socket.display(), "cap daemon listening");
    std::fs::write(paths::pid_path()?, std::process::id().to_string())?;

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Spawn the sampler loop.
    let sampler_throttle = throttle.clone();
    let sampler_handle = tokio::spawn(sampler_loop(sampler_throttle));

    loop {
        tokio::select! {
            res = listener.accept() => {
                match res {
                    Ok((stream, _)) => {
                        let t = throttle.clone();
                        let stx = shutdown_tx.clone();
                        let el = eventlog.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_conn(stream, t, stx, el).await {
                                tracing::warn!(error = %e, "connection ended with error");
                            }
                        });
                    }
                    Err(e) => tracing::error!(error = %e, "accept failed"),
                }
            }
            _ = shutdown_rx.recv() => {
                tracing::info!("shutdown requested");
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("ctrl-c received");
                break;
            }
        }
    }

    sampler_handle.abort();
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(paths::pid_path()?);
    Ok(())
}

async fn sampler_loop(throttle: Arc<Throttle>) {
    let interval =
        std::time::Duration::from_millis(throttle.config().protect.sample_interval_ms.max(50));
    let mut sampler = MemorySampler::new();
    let mut rss_sampler = RssSampler::new();
    let mut cpu_sampler = CpuSampler::new();
    loop {
        let pids = throttle.active_child_pids().await;
        if !pids.is_empty() {
            let free = sampler.free_gb();
            let rss_map = rss_sampler.rss_bytes(&pids);
            let rss_lookup: crate::throttle::RssLookup<'_> = &|pid| rss_map.get(&pid).copied();
            let idle_pids = throttle.idle_timeout_child_pids().await;
            if !idle_pids.is_empty() {
                let cpu_map = cpu_sampler.cpu_usage(&idle_pids);
                throttle.note_cpu_usage(&cpu_map).await;
            }
            match throttle.tick(free, 0.0, rss_lookup).await {
                TickAction::TermedVictim { id, classification }
                | TickAction::KilledVictim { id, classification } => {
                    tracing::warn!(lease = id, ?classification, "explicit timeout reached");
                }
                TickAction::EscalatedToKill { id } => {
                    tracing::warn!(lease = id, "timeout grace expired; killed process group");
                }
                _ => {}
            }
        }
        tokio::time::sleep(interval).await;
    }
}

async fn handle_conn(
    stream: UnixStream,
    throttle: Arc<Throttle>,
    shutdown_tx: tokio::sync::mpsc::Sender<()>,
    eventlog: Arc<EventLog>,
) -> Result<()> {
    let (rx, mut tx) = stream.into_split();
    let mut reader = BufReader::new(rx);
    let mut line = String::new();

    // Lease (if any) owned by this connection. Released on EOF.
    let mut held_lease: Option<LeaseId> = None;

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let req: Request = match serde_json::from_str(line.trim()) {
            Ok(r) => r,
            Err(e) => {
                write_resp(
                    &mut tx,
                    &Response::Error {
                        message: format!("bad request: {e}"),
                    },
                )
                .await?;
                continue;
            }
        };

        match req {
            Request::Ping => {
                write_resp(
                    &mut tx,
                    &Response::Pong {
                        version: VERSION.into(),
                    },
                )
                .await?;
            }
            Request::Acquire(a) => {
                let label = a.label.clone().unwrap_or_else(|| {
                    if a.args.is_empty() {
                        a.program.clone()
                    } else {
                        format!("{} {}", a.program, a.args.join(" "))
                    }
                });
                // Strategy hints in the kill envelope (and the run log's
                // `program`) are keyed by the bare program basename. For
                // hook-wrapped `bash -c '<script>'` invocations we look
                // through the shell to the real tool (`cargo`, `pytest`),
                // so hints fire and the log groups by the actual command.
                let program = crate::hook::effective_program(&a.program, &a.args);
                // `None` defers to the config default; `Some(0)` (or a
                // `0` default) means disabled — only a positive number
                // of seconds becomes an active `Duration` budget.
                let protect = &throttle.config().protect;
                let timeout_secs = a.timeout_secs.unwrap_or(protect.default_timeout_secs);
                let idle_timeout_secs = a
                    .idle_timeout_secs
                    .unwrap_or(protect.default_idle_timeout_secs);
                let timeout =
                    (timeout_secs > 0).then(|| std::time::Duration::from_secs(timeout_secs));
                let idle_timeout = (idle_timeout_secs > 0)
                    .then(|| std::time::Duration::from_secs(idle_timeout_secs));
                let lease = throttle
                    .register(a.client_pid, program, label, a.cwd, timeout, idle_timeout)
                    .await;
                held_lease = Some(lease);
                let nice = 0;
                write_resp(&mut tx, &Response::Lease { lease, nice }).await?;
            }
            Request::Spawned { lease, child_pid } => {
                throttle.attach_pid(lease, child_pid).await;
                write_resp(&mut tx, &Response::SpawnedAck).await?;
            }
            Request::Release { lease, exit_code } => {
                let outcome = throttle.release(lease, exit_code).await;
                if let Some(rec) = &outcome.record {
                    eventlog.append(rec);
                }
                if held_lease == Some(lease) {
                    held_lease = None;
                }
                write_resp(
                    &mut tx,
                    &Response::Released {
                        lease,
                        kill_envelope: outcome.kill_envelope,
                    },
                )
                .await?;
            }
            Request::Status | Request::Ps => {
                let mut sampler = MemorySampler::new();
                let free = sampler.free_gb();
                let load_sampler = LoadSampler::new();
                let load = load_sampler.load_per_core();
                let snap = throttle.snapshot(free, load).await;
                write_resp(&mut tx, &Response::Status(snap)).await?;
            }
            Request::WaitForCapacity { .. } => {
                write_resp(&mut tx, &Response::CapacityOk).await?;
            }
            Request::Shutdown => {
                write_resp(&mut tx, &Response::ShuttingDown).await?;
                let _ = shutdown_tx.send(()).await;
                break;
            }
        }
    }

    // Drop a disconnected client's lease and record its unknown exit status.
    if let Some(lease) = held_lease.take() {
        let outcome = throttle.release(lease, None).await;
        if let Some(rec) = &outcome.record {
            eventlog.append(rec);
        }
    }
    Ok(())
}

async fn write_resp<W: AsyncWriteExt + Unpin>(w: &mut W, r: &Response) -> Result<()> {
    let mut s = serde_json::to_string(r)?;
    s.push('\n');
    w.write_all(s.as_bytes()).await?;
    Ok(())
}

/// Is a daemon currently running for this user?
/// @spec tech-design/semantic/cap-src.md#schema
pub fn is_running() -> Result<bool> {
    let pid_path = paths::pid_path()?;
    if !pid_path.exists() {
        return Ok(false);
    }
    let pid_text = std::fs::read_to_string(&pid_path)?;
    let pid: i32 = pid_text.trim().parse().unwrap_or(0);
    if pid <= 0 {
        return Ok(false);
    }
    let alive = unsafe { libc::kill(pid, 0) == 0 };
    Ok(alive)
}

/// @spec tech-design/semantic/cap-src.md#schema
pub fn spawn_background() -> Result<i32> {
    use std::process::{Command, Stdio};
    paths::ensure_home()?;
    let exe = std::env::current_exe().context("locating cap binary")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::log_path()?)?;
    let log_err = log.try_clone()?;
    let child = Command::new(exe)
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .context("spawning cap daemon")?;
    Ok(child.id() as i32)
}
// CODEGEN-END
