// SPEC-MANAGED: tech-design/semantic/cap-src.md#schema
// CODEGEN-BEGIN
//! IPC protocol between cap clients and the cap daemon.
//!
//! Wire format: newline-delimited JSON over a Unix domain socket.
//!
//! Kept in the cap library so every client speaks the daemon protocol without
//! re-declaring the wire types.

use serde::{Deserialize, Serialize};

/// @spec tech-design/semantic/cap-src.md#schema
pub type LeaseId = u64;

/// Client → Daemon.
/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// Register a compatibility daemon lease without admission limits.
    Acquire(AcquireRequest),
    /// Client has spawned the child; register its PID for timeout handling.
    Spawned { lease: LeaseId, child_pid: i32 },
    /// Child has exited.
    Release {
        lease: LeaseId,
        exit_code: Option<i32>,
    },
    /// `cap status` — snapshot of compatibility daemon leases.
    Status,
    /// `cap ps` — alias of status.
    Ps,
    /// `cap daemon stop` — graceful shutdown.
    Shutdown,
    /// `cap ping` — health probe.
    Ping,
    /// Compatibility request. Always returns CapacityOk immediately.
    WaitForCapacity { timeout_secs: Option<u64> },
}

/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcquireRequest {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub label: Option<String>,
    pub client_pid: i32,
    /// Absolute wall-clock budget in seconds, measured from `Spawned`
    /// and excluding time spent `Paused`. `None` defers to the
    /// daemon's `default_timeout_secs`; `Some(0)` explicitly disables
    /// the trigger for this lease regardless of that default.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Kill the lease if it makes no CPU progress for this many
    /// seconds (excluding time spent `Paused`). Same `None`/`Some(0)`
    /// semantics as `timeout_secs`.
    #[serde(default)]
    pub idle_timeout_secs: Option<u64>,
}

/// Daemon → Client.
/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Lease assigned. Client may now spawn the child.
    Lease { lease: LeaseId, nice: i32 },
    /// Spawned acknowledged.
    SpawnedAck,
    /// Release acknowledged. A timeout can supply a structured diagnostic.
    Released {
        lease: LeaseId,
        kill_envelope: Option<KillEnvelope>,
    },
    /// Snapshot reply.
    Status(StatusSnapshot),
    /// Daemon will exit imminently.
    ShuttingDown,
    /// Health probe reply.
    Pong { version: String },
    /// Compatibility response. Resource capacity does not restrict commands.
    CapacityOk,
    /// `WaitForCapacity` timed out (client deadline or server hard cap)
    /// before headroom recovered. The CLI surfaces this as exit 124
    /// (matching GNU `timeout`).
    CapacityTimeout,
    /// Error path.
    Error { message: String },
}

/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub free_mem_gb: f64,
    /// Retired memory threshold. Always zero in the current daemon.
    pub pause_floor_gb: f64,
    /// Retired memory threshold. Always zero in the current daemon.
    pub kill_floor_gb: f64,
    /// Loadavg(1m) divided by core count. Telemetry only.
    pub load_per_core: f64,
    /// Retired load threshold. Always zero in the current daemon.
    pub load_pause_floor: f64,
    /// Legacy field kept for older `cap status` clients. Mirrors
    /// `pause_floor_gb`; new code should read `pause_floor_gb`.
    pub min_free_gb: f64,
    pub running: u32,
    pub paused: u32,
    pub leases: Vec<LeaseSnapshot>,
}

/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    /// Registered, waiting for client to send Spawned.
    Pending,
    /// Child is running.
    Running,
    /// Retired pause state, retained for older daemon responses.
    Paused,
    /// Throttler has sent SIGTERM and is waiting `kill_grace_secs`
    /// before escalating to SIGKILL. Excluded from victim re-selection.
    Killing,
    /// Child was SIGKILLed by the throttler.
    Killed,
}

/// Why a particular kill happened — surfaced to the agent so it can
/// decide whether to wait-and-retry, change strategy, or wait for
/// external pressure to clear. Computed at the tick site where lease
/// count and victim RSS are both known.
/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KillClassification {
    /// Multiple cap-managed leases were running and the largest one
    /// was evicted to make room for the others. Retry after `cap wait`.
    Competition,
    /// A single lease's RSS by itself exceeded the system's headroom
    /// budget. Retrying the same command will hit the same wall —
    /// agent must change strategy (lower `--jobs`, split the task).
    Oversize,
    /// Only one (or zero) cap lease was running, but free memory was
    /// still below the kill floor. Non-cap processes (LSPs, browsers,
    /// IDEs) are eating the budget. Agent should `cap wait` and let
    /// the system recover; retrying immediately will fail again.
    External,
    /// The lease exceeded its `--timeout` wall-clock budget (excluding
    /// `Paused` time). Not a memory-pressure kill — retrying as-is
    /// will hit the same wall unless the budget is raised or the work
    /// is split.
    AbsoluteTimeout,
    /// The lease made no CPU progress for `--idle-timeout` seconds
    /// (excluding `Paused` time). Likely a hang (blocked I/O,
    /// deadlock), not resource competition.
    IdleTimeout,
}

/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseSnapshot {
    pub lease: LeaseId,
    pub client_pid: i32,
    pub child_pid: Option<i32>,
    pub label: String,
    pub state: LeaseState,
    pub age_secs: u64,
}

/// One-line summary of another lease that was active at the moment a
/// kill happened. Useful for the agent to understand whether it was
/// competing with its own siblings or with non-cap work.
/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseBrief {
    pub lease: LeaseId,
    pub label: String,
    pub state: LeaseState,
    pub rss_gb: f64,
}

/// Actionable suggestion attached to a kill, derived from the
/// classification. Phrased as a hint — agents may choose to apply it,
/// fall back to their own strategy, or escalate to a human.
/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Competition with other cap leases. `cap wait` for capacity,
    /// then retry the same command — the winner finishes and frees RAM.
    WaitAndRetry {
        suggested_secs: u64,
        next_step: String,
    },
    /// Single lease alone was over the headroom budget. Retrying as-is
    /// will hit the same wall — agent must change strategy (lower
    /// parallelism, split the workload, etc).
    ChangeStrategy { hint: String, next_step: String },
    /// Non-cap processes are eating the budget. Agent should pause,
    /// optionally inspect what's hogging RAM, then `cap wait`.
    InspectAndWait {
        suggested_secs: u64,
        next_step: String,
    },
    /// `AbsoluteTimeout` kill. Retrying as-is will hit the same wall —
    /// agent should raise `--timeout` or split the work into smaller
    /// steps.
    RaiseTimeoutOrSplit { hint: String, next_step: String },
    /// `IdleTimeout` kill. Not a resource-pressure kill — agent should
    /// investigate why the command stopped making progress before
    /// retrying.
    InvestigateHang { hint: String, next_step: String },
}

/// Structured kill report — superseded the old `killed_reason: String`.
/// Surfaced to clients on `Response::Released` and rendered as a
/// multi-line stderr message via `human_message`.
/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillEnvelope {
    pub classification: KillClassification,
    pub action: Action,
    pub victim_label: String,
    pub victim_rss_gb: f64,
    pub free_gb: f64,
    pub kill_floor_gb: f64,
    pub total_gb: f64,
    pub other_leases: Vec<LeaseBrief>,
    /// Pre-formatted multi-line message for `eprintln!` on the client.
    /// Agents that want structure should read the typed fields instead.
    pub human_message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_request_timeout_fields_round_trip() {
        let req = AcquireRequest {
            program: "cargo".into(),
            args: vec!["build".into()],
            cwd: "/tmp".into(),
            label: None,
            client_pid: 1,
            timeout_secs: Some(30),
            idle_timeout_secs: Some(0),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: AcquireRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.timeout_secs, Some(30));
        assert_eq!(parsed.idle_timeout_secs, Some(0));
    }

    #[test]
    fn acquire_request_omitted_timeout_fields_default_to_none() {
        // Old clients that never learned about these fields must still
        // deserialize cleanly.
        let json = r#"{"program":"ls","args":[],"cwd":"/tmp","label":null,"client_pid":1}"#;
        let parsed: AcquireRequest = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.timeout_secs, None);
        assert_eq!(parsed.idle_timeout_secs, None);
    }
}
// CODEGEN-END
