// SPEC-MANAGED: tech-design/semantic/cap-src.md#schema
// CODEGEN-BEGIN
//! Command lease tracking, run logs, and explicit timeouts.
//!
//! Memory, CPU load, and resident size are measurements only. They never
//! delay, pause, or terminate a command. Legacy pressure methods and fields
//! remain available for callers that use the earlier library API.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::config::Config;
use crate::eventlog::RunRecord;
use crate::protocol::{
    Action, KillClassification, KillEnvelope, LeaseBrief, LeaseId, LeaseSnapshot, LeaseState,
    StatusSnapshot,
};

/// Closure that returns per-process RSS in bytes, or `None` if the
/// PID is unknown / dead. Slice 2 callers pass `|_| None`; Slice 3
/// passes a `sysinfo`-backed lookup. `Send + Sync` is required so
/// `tick()` can be awaited from the tokio sampler task.
/// @spec tech-design/semantic/source/throttle.md#source
pub type RssLookup<'a> = &'a (dyn Fn(i32) -> Option<u64> + Send + Sync);

/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Debug)]
pub struct Lease {
    pub id: LeaseId,
    pub client_pid: i32,
    pub child_pid: Option<i32>,
    /// Bare program name (argv[0], basename only) used to pick a
    /// strategy hint in the kill envelope. The `label` field is for
    /// human display and may carry the full argv.
    pub program: String,
    pub label: String,
    /// Working directory the command was launched in (for the run log).
    pub cwd: String,
    pub registered: Instant,
    /// Wall-clock submission time, for the run log's `started_at`.
    pub registered_wall: chrono::DateTime<chrono::Local>,
    /// When the client reported the child PID (the `Spawned` step). The
    /// gap `spawned_at - registered` is the queue time the run log
    /// reports. `None` until the command actually starts.
    pub spawned_at: Option<Instant>,
    pub state: LeaseState,
    /// Peak leader-process RSS seen across the run (run-log metric,
    /// updated each tick from the sampler's RSS lookup).
    pub peak_rss_bytes: u64,
    /// System free memory captured on the first tick after the child
    /// started — the run log's `free_gb_at_start`.
    pub free_gb_at_start: Option<f64>,
    /// Accumulated time this lease has spent SIGSTOPped, plus the start
    /// of the current pause if it's paused right now. Combined at
    /// release time into the run log's `paused_ms`.
    pub paused_total: Duration,
    pub paused_since: Option<Instant>,
    /// Set when the throttler kills the child. Surfaced to the
    /// client on Release so it can render a structured diagnostic.
    pub kill_envelope: Option<KillEnvelope>,
    /// When the throttler first sent SIGTERM (transition Running/Paused
    /// → Killing). Used by the grace-period escalator on subsequent
    /// ticks: once `elapsed >= kill_grace_secs`, escalate to a
    /// process-group SIGKILL. Uses `tokio::time::Instant` so tests can
    /// drive it via `tokio::time::pause + advance`.
    pub kill_started_at: Option<tokio::time::Instant>,
    /// Absolute wall-clock budget (`cap run --timeout`), or `None` if
    /// disabled. Compared against elapsed time since `spawned_at`,
    /// excluding `paused_total`/`paused_since` — see
    /// `lease_elapsed_excluding_paused`.
    pub timeout: Option<Duration>,
    /// Idle (no-CPU-progress) budget (`cap run --idle-timeout`), or
    /// `None` if disabled. Compared against `idle_ticks` — see
    /// `Throttle::note_cpu_usage`.
    pub idle_timeout: Option<Duration>,
    /// Consecutive sampler ticks (while `Running`) with no observed
    /// CPU activity. Reset to 0 the moment activity is observed;
    /// never incremented while `Paused`, satisfying the same
    /// paused-time exclusion as the absolute-timeout clock. Advanced
    /// by `Throttle::note_cpu_usage`, read by `tick()`.
    pub idle_ticks: u32,
}

#[derive(Debug)]
struct State {
    next_id: LeaseId,
    leases: HashMap<LeaseId, Lease>,
}

/// @spec tech-design/semantic/cap-src.md#schema
pub struct Throttle {
    cfg: Config,
    state: Mutex<State>,
}

/// Result of releasing a lease: the kill diagnostic (if cap killed it)
/// plus the run-log record (if the command ever actually started).
/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Debug)]
pub struct ReleaseOutcome {
    pub kill_envelope: Option<KillEnvelope>,
    pub record: Option<RunRecord>,
}

/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Debug, Clone)]
pub enum TickAction {
    Idle,
    /// Retired resource-control action, retained for API compatibility.
    Resumed(LeaseId),
    /// Retired resource-control action, retained for API compatibility.
    PausedNewest(LeaseId),
    /// An explicit timeout sent SIGTERM to the lease leader.
    /// A later tick sends process-group SIGKILL after the grace period.
    TermedVictim {
        id: LeaseId,
        classification: KillClassification,
    },
    /// The timeout grace period expired and the process group was killed.
    EscalatedToKill {
        id: LeaseId,
    },
    /// An explicit timeout with no grace period terminated the command.
    KilledVictim {
        id: LeaseId,
        classification: KillClassification,
    },
    /// Retired resource-control action, retained for API compatibility.
    KilledAllPaused(Vec<LeaseId>),
}

/// @spec tech-design/semantic/cap-src.md#schema
impl Throttle {
    /// Build a lease tracker. Legacy resource-floor arguments are ignored.
    pub fn new(
        cfg: Config,
        _pause_floor_gb: f64,
        _kill_floor_gb: f64,
        _total_gb: f64,
    ) -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            cfg,
            state: Mutex::new(State {
                next_id: 1,
                leases: HashMap::new(),
            }),
        }))
    }

    /// Legacy status field. Resource limits are disabled.
    pub fn pause_floor_gb(&self) -> f64 {
        0.0
    }

    /// Legacy status field. Resource limits are disabled.
    pub fn kill_floor_gb(&self) -> f64 {
        0.0
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Register a new lease in `Pending` state. Returns its id.
    /// `program` is the bare argv[0] (basename) — used at kill time to
    /// pick a strategy hint for the kill envelope. `label` is for human
    /// display (typically `program + args`).
    pub async fn register(
        &self,
        client_pid: i32,
        program: String,
        label: String,
        cwd: String,
        timeout: Option<Duration>,
        idle_timeout: Option<Duration>,
    ) -> LeaseId {
        let mut st = self.state.lock().await;
        let id = st.next_id;
        st.next_id = st.next_id.wrapping_add(1).max(1);
        st.leases.insert(
            id,
            Lease {
                id,
                client_pid,
                child_pid: None,
                program,
                label,
                cwd,
                registered: Instant::now(),
                registered_wall: chrono::Local::now(),
                spawned_at: None,
                state: LeaseState::Pending,
                peak_rss_bytes: 0,
                free_gb_at_start: None,
                paused_total: Duration::ZERO,
                paused_since: None,
                kill_envelope: None,
                kill_started_at: None,
                timeout,
                idle_timeout,
                idle_ticks: 0,
            },
        );
        id
    }

    /// Snapshot of currently-live child PIDs (any state with a real
    /// `child_pid`). The sampler loop uses this to scope the per-tick
    /// RSS refresh to just our lease set.
    pub async fn active_child_pids(&self) -> Vec<i32> {
        let st = self.state.lock().await;
        st.leases
            .values()
            .filter_map(|l| l.child_pid)
            .filter(|p| *p > 0)
            .collect()
    }

    /// PIDs that need CPU-progress samples for an explicit idle timeout.
    pub async fn idle_timeout_child_pids(&self) -> Vec<i32> {
        let st = self.state.lock().await;
        st.leases
            .values()
            .filter(|lease| lease.idle_timeout.is_some() && lease.state == LeaseState::Running)
            .filter_map(|lease| lease.child_pid)
            .filter(|pid| *pid > 0)
            .collect()
    }

    /// Record the child PID for run logs and explicit timeouts.
    pub async fn attach_pid(&self, lease: LeaseId, child_pid: i32) {
        let mut st = self.state.lock().await;
        if let Some(l) = st.leases.get_mut(&lease) {
            l.child_pid = Some(child_pid);
            if l.spawned_at.is_none() {
                l.spawned_at = Some(Instant::now());
            }
            if l.state == LeaseState::Pending {
                l.state = LeaseState::Running;
            }
        }
    }

    /// Feed a fresh CPU% sample (keyed by `child_pid`) into every
    /// lease that has an idle-timeout configured, advancing or
    /// resetting its `idle_ticks` debounce counter. Must be called
    /// once per sampler-loop iteration, before `tick()`, so `tick()`
    /// observes this iteration's activity when deciding whether the
    /// idle-timeout has fired. Deliberately excluded from `tick()`
    /// itself so `tick()`'s existing 3-argument test surface is
    /// unaffected — this is a separate, additive entry point.
    ///
    /// Only `Running` leases accrue idle ticks: a `Paused` lease is
    /// SIGSTOPped by cap itself and would otherwise misreport as
    /// "hung", so excluding it here satisfies the same paused-time
    /// exclusion the absolute-timeout clock gets from `paused_total`.
    pub async fn note_cpu_usage(&self, cpu_by_pid: &std::collections::HashMap<i32, f32>) {
        let mut st = self.state.lock().await;
        for l in st.leases.values_mut() {
            if l.idle_timeout.is_none() || l.state != LeaseState::Running {
                continue;
            }
            let Some(pid) = l.child_pid else { continue };
            match cpu_by_pid.get(&pid) {
                Some(pct) if *pct > 0.0 => l.idle_ticks = 0,
                _ => l.idle_ticks = l.idle_ticks.saturating_add(1),
            }
        }
    }

    /// Drop a lease entirely. Returns the `kill_envelope` (if the
    /// throttler had killed the child, so the client can surface a
    /// structured diagnostic on stderr) plus a run-log record for the
    /// command, built from the lease's accumulated bookkeeping.
    /// `exit_code` is the child's exit status as the client observed it.
    pub async fn release(&self, lease: LeaseId, exit_code: Option<i32>) -> ReleaseOutcome {
        let mut st = self.state.lock().await;
        let Some(l) = st.leases.remove(&lease) else {
            return ReleaseOutcome {
                kill_envelope: None,
                record: None,
            };
        };
        let record = build_run_record(&l, exit_code);
        ReleaseOutcome {
            kill_envelope: l.kill_envelope,
            record,
        }
    }

    /// Legacy compatibility method. Resource pressure never blocks a command.
    pub fn is_under_pause_pressure(&self, _free_mem_gb: f64, _load_per_core: f64) -> bool {
        false
    }

    /// Legacy compatibility method. Capacity is always available.
    pub async fn wait_for_capacity(&self, _timeout: Option<Duration>) -> bool {
        true
    }

    /// Record measurements and enforce only explicit timeouts.
    pub async fn tick(
        &self,
        free_mem_gb: f64,
        _load_per_core: f64,
        rss_of: RssLookup<'_>,
    ) -> TickAction {
        let grace = Duration::from_secs(self.cfg.protect.kill_grace_secs);
        let sample_interval_ms = self.cfg.protect.sample_interval_ms.max(1);
        let mut st = self.state.lock().await;

        // ── Run-log bookkeeping ────────────────────────────────────
        // Track per-lease peak RSS and the free memory seen just after
        // each command started. Cheap (≤ a handful of live leases) and
        // feeds the JSONL record built at release time.
        for l in st.leases.values_mut() {
            if let Some(pid) = l.child_pid {
                if let Some(rss) = rss_of(pid) {
                    l.peak_rss_bytes = l.peak_rss_bytes.max(rss);
                }
                if l.free_gb_at_start.is_none() && free_mem_gb.is_finite() && free_mem_gb >= 0.0 {
                    l.free_gb_at_start = Some(free_mem_gb);
                }
            }
        }

        // ── Grace-period escalator ─────────────────────────────────
        // Runs FIRST every tick, regardless of headroom, so we don't
        // leave a timed-out command hanging after SIGTERM. Pick the oldest expired
        // Killing lease — multiple-in-one-tick is rare, and surfacing
        // one per tick keeps logs readable.
        let now = tokio::time::Instant::now();
        let expired = st
            .leases
            .values()
            .filter(|l| l.state == LeaseState::Killing)
            .filter(|l| {
                l.kill_started_at
                    .map(|t| now.duration_since(t) >= grace)
                    .unwrap_or(true) // defensive: missing timestamp → escalate
            })
            .min_by_key(|l| l.kill_started_at.unwrap_or(now))
            .map(|l| (l.id, l.child_pid));
        if let Some((id, pid)) = expired {
            if let Some(pid) = pid {
                send_signal(pid, libc::SIGKILL); // process group, this time
            }
            if let Some(l) = st.leases.get_mut(&id) {
                l.state = LeaseState::Killed;
            }
            return TickAction::EscalatedToKill { id };
        }

        // ── Per-lease wall-clock / idle-progress timeouts ─────────
        // Only a configured timeout can terminate a running lease.
        let now_std = Instant::now();
        let timed_out = st
            .leases
            .values()
            .filter(|l| matches!(l.state, LeaseState::Running | LeaseState::Paused))
            .find_map(|l| {
                if let Some(timeout) = l.timeout {
                    let elapsed = lease_elapsed_excluding_paused(l, now_std);
                    if elapsed >= timeout {
                        return Some((
                            l.id,
                            l.child_pid,
                            KillClassification::AbsoluteTimeout,
                            format!(
                                "exceeded --timeout of {}s (ran {}s excluding paused time)",
                                timeout.as_secs(),
                                elapsed.as_secs()
                            ),
                        ));
                    }
                }
                if l.state == LeaseState::Running {
                    if let Some(idle_timeout) = l.idle_timeout {
                        let threshold = idle_ticks_threshold(idle_timeout, sample_interval_ms);
                        if l.idle_ticks >= threshold {
                            return Some((
                                l.id,
                                l.child_pid,
                                KillClassification::IdleTimeout,
                                format!(
                                    "no CPU progress for --idle-timeout of {}s \
                                     ({} consecutive idle ticks)",
                                    idle_timeout.as_secs(),
                                    l.idle_ticks
                                ),
                            ));
                        }
                    }
                }
                None
            });
        if let Some((id, pid, classification, note)) = timed_out {
            let victim_rss_gb = pid.and_then(rss_of).map(bytes_to_gb).unwrap_or(0.0);
            let (victim_program, victim_label) = st
                .leases
                .get(&id)
                .map(|l| (l.program.clone(), l.label.clone()))
                .unwrap_or_default();
            let envelope = build_envelope(
                classification,
                &victim_program,
                &victim_label,
                victim_rss_gb,
                if free_mem_gb.is_finite() && free_mem_gb >= 0.0 {
                    free_mem_gb
                } else {
                    0.0
                },
                0.0,
                0.0,
                Vec::new(),
                Some(note),
            );
            let skip_grace = self.cfg.protect.kill_grace_secs == 0;
            return if skip_grace {
                if let Some(pid) = pid {
                    send_signal(pid, libc::SIGKILL);
                }
                if let Some(l) = st.leases.get_mut(&id) {
                    l.state = LeaseState::Killed;
                    l.kill_envelope = Some(envelope);
                }
                TickAction::KilledVictim { id, classification }
            } else {
                if let Some(pid) = pid {
                    unsafe {
                        let _ = libc::kill(pid, libc::SIGTERM);
                    }
                }
                if let Some(l) = st.leases.get_mut(&id) {
                    l.state = LeaseState::Killing;
                    l.kill_started_at = Some(tokio::time::Instant::now());
                    l.kill_envelope = Some(envelope);
                }
                TickAction::TermedVictim { id, classification }
            };
        }

        TickAction::Idle
    }

    pub async fn snapshot(&self, free_mem_gb: f64, load_per_core: f64) -> StatusSnapshot {
        let st = self.state.lock().await;
        let mut running = 0u32;
        let mut paused = 0u32;
        let mut leases: Vec<LeaseSnapshot> = st
            .leases
            .values()
            .map(|l| {
                match l.state {
                    LeaseState::Running => running += 1,
                    LeaseState::Paused => paused += 1,
                    _ => {}
                }
                LeaseSnapshot {
                    lease: l.id,
                    client_pid: l.client_pid,
                    child_pid: l.child_pid,
                    label: l.label.clone(),
                    state: l.state,
                    age_secs: l.registered.elapsed().as_secs(),
                }
            })
            .collect();
        leases.sort_by_key(|l| l.lease);
        StatusSnapshot {
            free_mem_gb,
            pause_floor_gb: 0.0,
            kill_floor_gb: 0.0,
            load_per_core,
            load_pause_floor: 0.0,
            min_free_gb: 0.0,
            running,
            paused,
            leases,
        }
    }
}

fn send_signal(pid: i32, sig: libc::c_int) {
    unsafe {
        // Negative pid = "send to process group PGID=pid". The
        // supervisor makes each child its own group leader via
        // setpgid(0, 0), so this hits the whole subtree (cargo test
        // → rustc → ...). EPERM / ESRCH are OK to ignore — child may
        // have already exited or be in mid-reap.
        let _ = libc::kill(-pid, sig);
    }
}

fn bytes_to_gb(b: u64) -> f64 {
    b as f64 / 1024.0 / 1024.0 / 1024.0
}

/// Wall-clock time since `spawned_at`, minus time spent `Paused`
/// (both fully-elapsed pauses in `paused_total` and any pause still
/// in flight). Returns `Duration::ZERO` if the lease never spawned.
fn lease_elapsed_excluding_paused(l: &Lease, now: Instant) -> Duration {
    let Some(spawned_at) = l.spawned_at else {
        return Duration::ZERO;
    };
    let total = now.saturating_duration_since(spawned_at);
    let mut paused = l.paused_total;
    if let Some(since) = l.paused_since {
        paused += now.saturating_duration_since(since);
    }
    total.saturating_sub(paused)
}

/// Convert an idle-timeout duration into a consecutive-tick count,
/// debounced the same way the pause/kill floors debounce via
/// `trigger_samples` — but keyed to the sampler's actual cadence
/// instead of a fixed sample count, since `--idle-timeout` is
/// specified in seconds.
fn idle_ticks_threshold(idle_timeout: Duration, sample_interval_ms: u64) -> u32 {
    let ticks = idle_timeout.as_millis() / sample_interval_ms.max(1) as u128;
    ticks.max(1) as u32
}

/// Build the JSONL run-log record for a finished lease, or `None` if the
/// command never actually started (no `spawned_at` — e.g. the client
/// died during Acquire backpressure). Durations are computed from the
/// lease bookkeeping at release time.
fn build_run_record(l: &Lease, exit_code: Option<i32>) -> Option<RunRecord> {
    let spawned_at = l.spawned_at?;
    let now = Instant::now();
    let queue = spawned_at.saturating_duration_since(l.registered);
    let duration = now.saturating_duration_since(spawned_at);
    let mut paused = l.paused_total;
    if let Some(since) = l.paused_since {
        // Still paused at release (e.g. killed while SIGSTOPped) — count
        // the in-flight pause too.
        paused += now.saturating_duration_since(since);
    }
    let killed = l.kill_envelope.is_some();
    Some(RunRecord {
        ts: chrono::Local::now().to_rfc3339(),
        started_at: l.registered_wall.to_rfc3339(),
        lease: l.id,
        command: l.label.clone(),
        program: l.program.clone(),
        cwd: l.cwd.clone(),
        client_pid: l.client_pid,
        child_pid: l.child_pid,
        queue_ms: queue.as_millis() as u64,
        duration_ms: duration.as_millis() as u64,
        paused_ms: paused.as_millis() as u64,
        peak_rss_gb: bytes_to_gb(l.peak_rss_bytes),
        free_gb_at_start: l.free_gb_at_start,
        exit_code,
        outcome: if killed { "killed" } else { "completed" },
        kill_classification: l.kill_envelope.as_ref().map(|e| e.classification),
    })
}

#[allow(clippy::too_many_arguments)]
fn build_envelope(
    classification: KillClassification,
    _victim_program: &str,
    victim_label: &str,
    victim_rss_gb: f64,
    free_gb: f64,
    kill_floor_gb: f64,
    total_gb: f64,
    other_leases: Vec<LeaseBrief>,
    extra_note: Option<String>,
) -> KillEnvelope {
    let action = match classification {
        KillClassification::AbsoluteTimeout => Action::RaiseTimeoutOrSplit {
            hint: "raise --timeout, or split the command into smaller steps".to_string(),
            next_step: format!(
                "# exceeded its --timeout budget — raise it or break up the work: {victim_label}"
            ),
        },
        KillClassification::IdleTimeout => Action::InvestigateHang {
            hint: "no CPU progress observed — likely stuck (blocked I/O, deadlock)".to_string(),
            next_step: format!(
                "# investigate why `{victim_label}` stopped making progress before retrying"
            ),
        },
        KillClassification::Competition
        | KillClassification::Oversize
        | KillClassification::External => unreachable!("resource-pressure kills were removed"),
    };

    let mut human = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(
        human,
        "cap: killed `{victim_label}` — {label}",
        label = classification_label(classification),
    );
    if !other_leases.is_empty() {
        let _ = writeln!(human, "  siblings at kill time:");
        for b in &other_leases {
            let _ = writeln!(
                human,
                "    [{state:?}] {label}  RSS {rss:.2} GB",
                state = b.state,
                label = b.label,
                rss = b.rss_gb,
            );
        }
    }
    let _ = writeln!(human, "  next: {}", action_next_step(&action));
    if let Some(note) = &extra_note {
        let _ = writeln!(human, "  note: {note}");
    }

    KillEnvelope {
        classification,
        action,
        victim_label: victim_label.to_string(),
        victim_rss_gb,
        free_gb,
        kill_floor_gb,
        total_gb,
        other_leases,
        human_message: human,
    }
}

fn classification_label(c: KillClassification) -> &'static str {
    match c {
        KillClassification::Competition => "memory competition (other cap leases were running)",
        KillClassification::Oversize => "single-process oversize (RSS exceeded headroom budget)",
        KillClassification::External => {
            "external pressure (non-cap processes are eating the budget)"
        }
        KillClassification::AbsoluteTimeout => {
            "absolute timeout (exceeded --timeout wall-clock budget)"
        }
        KillClassification::IdleTimeout => "idle timeout (no CPU progress for --idle-timeout)",
    }
}

fn action_next_step(a: &Action) -> &str {
    match a {
        Action::WaitAndRetry { next_step, .. }
        | Action::ChangeStrategy { next_step, .. }
        | Action::InspectAndWait { next_step, .. }
        | Action::RaiseTimeoutOrSplit { next_step, .. }
        | Action::InvestigateHang { next_step, .. } => next_step.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(min_free: f64, trigger: u32) -> Config {
        let mut cfg = Config::default();
        cfg.protect.min_free_gb = min_free;
        cfg.protect.trigger_samples = trigger;
        cfg.protect.pause_load_percent = 80;
        cfg.protect.kill_grace_secs = 0;
        cfg
    }

    fn throttle_with(min_free: f64, trigger: u32) -> Arc<Throttle> {
        Throttle::new(cfg_with(min_free, trigger), min_free, 1.0, 16.0).unwrap()
    }

    const NO_RSS: RssLookup<'_> = &|_| None;

    #[tokio::test]
    async fn resource_pressure_never_pauses_or_kills_commands() {
        let mut cfg = cfg_with(1000.0, 1);
        cfg.protect.reap_enabled = true;
        let tracker = Throttle::new(cfg, 1000.0, 2000.0, 0.0).unwrap();
        for label in ["first", "second"] {
            let id = tracker
                .register(1, label.into(), label.into(), String::new(), None, None)
                .await;
            tracker.attach_pid(id, i32::MAX).await;
        }
        let huge_rss: RssLookup<'_> = &|_| Some(u64::MAX);
        for _ in 0..20 {
            assert!(matches!(
                tracker.tick(0.0, 10000.0, huge_rss).await,
                TickAction::Idle
            ));
        }
        let snap = tracker.snapshot(0.0, 10000.0).await;
        assert_eq!(snap.running, 2);
        assert_eq!(snap.paused, 0);
        assert!(snap
            .leases
            .iter()
            .all(|lease| lease.state == LeaseState::Running));
        assert_eq!(snap.pause_floor_gb, 0.0);
        assert_eq!(snap.kill_floor_gb, 0.0);
        assert_eq!(snap.load_pause_floor, 0.0);
        assert!(!tracker.is_under_pause_pressure(0.0, 10000.0));
        assert!(tracker.wait_for_capacity(Some(Duration::ZERO)).await);
        assert!(tracker.wait_for_capacity(None).await);
    }

    #[tokio::test]
    async fn missing_memory_measurement_stays_unknown_in_run_log() {
        let tracker = throttle_with(0.0, 1);
        let id = tracker
            .register(1, "x".into(), "x".into(), String::new(), None, None)
            .await;
        tracker.attach_pid(id, i32::MAX).await;
        assert!(matches!(
            tracker.tick(f64::NAN, 0.0, NO_RSS).await,
            TickAction::Idle
        ));
        assert_eq!(
            tracker
                .release(id, Some(0))
                .await
                .record
                .unwrap()
                .free_gb_at_start,
            None
        );
    }

    #[test]
    fn invalid_legacy_resource_floors_do_not_block_startup() {
        let tracker = Throttle::new(Config::default(), f64::NAN, f64::INFINITY, -1.0).unwrap();
        assert_eq!(tracker.pause_floor_gb(), 0.0);
        assert_eq!(tracker.kill_floor_gb(), 0.0);
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_timeout_sigterm_then_grace_escalation() {
        let mut cfg = cfg_with(2.0, 1);
        cfg.protect.kill_grace_secs = 3;
        let tracker = Throttle::new(cfg, 0.0, 0.0, 0.0).unwrap();
        let id = tracker
            .register(
                1,
                "hang".into(),
                "hang".into(),
                String::new(),
                Some(Duration::ZERO),
                None,
            )
            .await;
        tracker.attach_pid(id, i32::MAX).await;
        assert!(
            matches!(tracker.tick(0.0, 10000.0, NO_RSS).await, TickAction::TermedVictim { id: got, classification: KillClassification::AbsoluteTimeout } if got == id)
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(matches!(
            tracker.tick(0.0, 10000.0, NO_RSS).await,
            TickAction::Idle
        ));
        tokio::time::advance(Duration::from_secs(3)).await;
        assert!(
            matches!(tracker.tick(100.0, 0.0, NO_RSS).await, TickAction::EscalatedToKill { id: got } if got == id)
        );
        let result = tracker.release(id, None).await;
        assert_eq!(
            result.kill_envelope.unwrap().classification,
            KillClassification::AbsoluteTimeout
        );
        assert_eq!(result.record.unwrap().outcome, "killed");
    }

    #[tokio::test]
    async fn release_record_captures_queue_duration_and_peak_rss() {
        let t = throttle_with(2.0, 1);
        let id = t
            .register(
                7,
                "cargo".into(),
                "cargo test -p cap".into(),
                "/tmp/proj".into(),
                None,
                None,
            )
            .await;
        // Queue time: the command waits before the client reports its PID.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        t.attach_pid(id, 4242).await;
        // A healthy-headroom tick records peak RSS + free-at-start.
        let rss: &(dyn Fn(i32) -> Option<u64> + Send + Sync) = &|pid| {
            if pid == 4242 {
                Some(3 * 1024 * 1024 * 1024)
            } else {
                None
            }
        };
        let _ = t.tick(8.0, 0.0, rss).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let rec = t
            .release(id, Some(0))
            .await
            .record
            .expect("a spawned command yields a run record");
        assert_eq!(rec.command, "cargo test -p cap");
        assert_eq!(rec.cwd, "/tmp/proj");
        assert_eq!(rec.exit_code, Some(0));
        assert_eq!(rec.outcome, "completed");
        assert!(rec.kill_classification.is_none());
        assert!(
            rec.queue_ms >= 15,
            "queue_ms reflects pre-spawn wait, got {}",
            rec.queue_ms
        );
        assert!(
            rec.duration_ms >= 15,
            "duration_ms reflects run time, got {}",
            rec.duration_ms
        );
        assert!(
            (rec.peak_rss_gb - 3.0).abs() < 0.05,
            "peak_rss_gb ~3.0, got {}",
            rec.peak_rss_gb
        );
        assert_eq!(rec.free_gb_at_start, Some(8.0));
    }

    #[tokio::test]
    async fn release_record_is_none_when_never_spawned() {
        // A lease that registered but never reached Spawned (client died
        // during Acquire backpressure) didn't actually run → not logged.
        let t = throttle_with(2.0, 1);
        let id = t
            .register(7, "ls".into(), "ls".into(), String::new(), None, None)
            .await;
        let out = t.release(id, None).await;
        assert!(
            out.record.is_none(),
            "never-spawned command must not be logged"
        );
    }

    #[tokio::test]
    async fn absolute_timeout_fires_regardless_of_pressure() {
        // Ample free memory (8.0 GB, well above pause/kill floors) —
        // proves the timeout check is NOT gated on pressure.
        let t = throttle_with(2.0, 1);
        let id = t
            .register(
                1,
                "hang".into(),
                "hang".into(),
                String::new(),
                Some(Duration::from_millis(10)),
                None,
            )
            .await;
        t.attach_pid(id, i32::MAX).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        match t.tick(8.0, 0.0, NO_RSS).await {
            TickAction::KilledVictim {
                id: killed,
                classification,
            } => {
                assert_eq!(killed, id);
                assert_eq!(classification, KillClassification::AbsoluteTimeout);
            }
            other => panic!("expected KilledVictim(AbsoluteTimeout), got {other:?}"),
        }
        let env = t
            .release(id, None)
            .await
            .kill_envelope
            .expect("timeout kill must surface an envelope");
        assert!(matches!(env.action, Action::RaiseTimeoutOrSplit { .. }));
    }

    #[tokio::test]
    async fn absolute_timeout_excludes_paused_duration() {
        let t = throttle_with(2.0, 1);
        let id = t
            .register(
                1,
                "hang".into(),
                "hang".into(),
                String::new(),
                Some(Duration::from_millis(20)),
                None,
            )
            .await;
        t.attach_pid(id, i32::MAX).await;
        // Immediately mark Paused and backdate paused_since so the
        // whole sleep below counts as paused time, not run time.
        {
            let mut st = t.state.lock().await;
            let l = st.leases.get_mut(&id).unwrap();
            l.state = LeaseState::Paused;
            l.paused_since = Some(Instant::now());
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
        // Pressure is clear (free=8.0), so a Paused lease may also
        // legitimately Resume this same tick — either outcome proves
        // the timeout branch did NOT treat it as timed out.
        assert!(
            matches!(
                t.tick(8.0, 0.0, NO_RSS).await,
                TickAction::Idle | TickAction::Resumed(_)
            ),
            "a fully-paused lease must not accrue timeout time"
        );
    }

    #[tokio::test]
    async fn idle_timeout_disabled_by_default_never_fires() {
        let t = throttle_with(2.0, 1);
        let id = t
            .register(1, "x".into(), "x".into(), String::new(), None, None)
            .await;
        t.attach_pid(id, i32::MAX).await;
        let empty = HashMap::new();
        for _ in 0..10 {
            t.note_cpu_usage(&empty).await;
        }
        assert!(matches!(t.tick(8.0, 0.0, NO_RSS).await, TickAction::Idle));
    }

    #[tokio::test]
    async fn idle_timeout_fires_after_debounced_no_progress_ticks() {
        // sample_interval_ms defaults to 500; idle_timeout=1500ms →
        // threshold = 3 debounced ticks with no observed CPU%.
        let t = throttle_with(2.0, 1);
        let id = t
            .register(
                1,
                "hang".into(),
                "hang".into(),
                String::new(),
                None,
                Some(Duration::from_millis(1500)),
            )
            .await;
        t.attach_pid(id, 424_242).await;
        let empty = HashMap::new();
        t.note_cpu_usage(&empty).await;
        assert!(matches!(t.tick(8.0, 0.0, NO_RSS).await, TickAction::Idle));
        t.note_cpu_usage(&empty).await;
        assert!(matches!(t.tick(8.0, 0.0, NO_RSS).await, TickAction::Idle));
        t.note_cpu_usage(&empty).await;
        match t.tick(8.0, 0.0, NO_RSS).await {
            TickAction::KilledVictim {
                id: killed,
                classification,
            } => {
                assert_eq!(killed, id);
                assert_eq!(classification, KillClassification::IdleTimeout);
            }
            other => panic!("expected KilledVictim(IdleTimeout), got {other:?}"),
        }
        let env = t
            .release(id, None)
            .await
            .kill_envelope
            .expect("idle-timeout kill must surface an envelope");
        assert!(matches!(env.action, Action::InvestigateHang { .. }));
    }

    #[tokio::test]
    async fn idle_timeout_resets_on_observed_cpu_progress() {
        let t = throttle_with(2.0, 1);
        let id = t
            .register(
                1,
                "hang".into(),
                "hang".into(),
                String::new(),
                None,
                Some(Duration::from_millis(1500)),
            )
            .await;
        t.attach_pid(id, 424_243).await;
        let empty = HashMap::new();
        t.note_cpu_usage(&empty).await;
        t.note_cpu_usage(&empty).await;
        // Progress observed on the 3rd sample — resets the debounce
        // counter, so the tick right after must NOT fire.
        let mut busy = HashMap::new();
        busy.insert(424_243, 12.5f32);
        t.note_cpu_usage(&busy).await;
        assert!(matches!(t.tick(8.0, 0.0, NO_RSS).await, TickAction::Idle));
    }

    #[tokio::test]
    async fn idle_timeout_never_accrues_while_paused() {
        let t = throttle_with(2.0, 1);
        let id = t
            .register(
                1,
                "hang".into(),
                "hang".into(),
                String::new(),
                None,
                Some(Duration::from_millis(1)), // threshold clamps to 1 tick
            )
            .await;
        t.attach_pid(id, 424_244).await;
        {
            let mut st = t.state.lock().await;
            st.leases.get_mut(&id).unwrap().state = LeaseState::Paused;
        }
        let empty = HashMap::new();
        for _ in 0..5 {
            t.note_cpu_usage(&empty).await;
        }
        let snap = t.snapshot(8.0, 0.0).await;
        let _ = snap;
        // A Paused lease is excluded from idle-timeout tick()'s check
        // entirely (see the `l.state == LeaseState::Running` guard), so
        // no kill can fire. Pressure is clear (free=8.0), so the lease
        // may also legitimately Resume this same tick — either outcome
        // proves the idle-timeout branch did not fire.
        assert!(matches!(
            t.tick(8.0, 0.0, NO_RSS).await,
            TickAction::Idle | TickAction::Resumed(_)
        ));
    }
}
// CODEGEN-END
