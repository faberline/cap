---
id: cap-external-contracts
summary: External contract gates for cap README capabilities.
fill_sections: [e2e-test]
capability_refs:
  - id: standard-agent-cli-operations
    role: primary
    gap: shared-standard-cli-commands
    claim: shared-standard-cli-commands
    coverage: full
    rationale: "The EC gate verifies cap's standard llm, upgrade, issue, and report-issue compatibility surface."
  - id: agent-hook-installation
    role: primary
    gap: claude-and-codex-hook-installation
    claim: claude-and-codex-hook-installation
    coverage: full
    rationale: "The EC gate verifies hook installation and hook payload rewrite behavior."
  - id: command-lease-throttling
    role: primary
    gap: lease-admission-and-process-supervision
    claim: lease-admission-and-process-supervision
    coverage: full
    rationale: "The EC gate verifies direct shell execution, explicit timeouts, and removal of resource limits."
  - id: daemon-lifecycle-and-status
    role: primary
    gap: daemon-process-lifecycle
    claim: daemon-process-lifecycle
    coverage: full
    rationale: "The EC gate verifies daemon lifecycle and CLI status surfaces."
  - id: config-logging-and-reap-policy
    role: primary
    gap: configuration-defaults-and-compatibility
    claim: configuration-defaults-and-compatibility
    coverage: full
    rationale: "The EC gate verifies legacy config compatibility, event logging, and disabled process reaping."
---

# External Contracts: cap

## Current execution policy

Cap provides efficient shell tools without memory or CPU resource limits.
Direct commands bypass the daemon and keep the caller's priority.
Only explicit per-invocation timeout flags limit direct external commands.
`cap wait` returns success immediately. Cap never reaps other processes.
Legacy EC IDs remain stable for compatibility with existing gate references.
The IDs do not retain the retired resource-governor behavior.
These contracts replace pressure and reap requirements in older source snapshots.

## cap-unrestricted-shell
<!-- type: e2e-test lang: yaml -->

```yaml
e2e_tests:
  - id: cap-unrestricted-shell
    capability_id: command-lease-throttling
    claim_id: direct-shell-execution-without-resource-limits
    contract_id: unrestricted-shell
    category: behavior
    command: "cargo test -p cap --test behavior_cap_unrestricted_shell"
    assertions:
      - "normal commands run without starting or contacting the daemon"
      - "legacy memory, CPU, priority and reap settings cannot restrict commands"
      - "external commands preserve output, exit status and caller priority"
      - "explicit external-command timeouts remain available"
```

## Standard Agent CLI Operations EC
<!-- type: e2e-test lang: yaml -->

```yaml
e2e_tests:
  - id: cap-standard-agent-cli-operations
    capability_id: standard-agent-cli-operations
    claim_id: shared-standard-cli-commands
    contract_id: standard-agent-cli-operations
    category: behavior
    command: "cargo test -p cap --lib cli_std_convention -- --nocapture && cargo test -p cap installed_frontend_exposes_standard_agent_commands -- --nocapture && cargo build -p cap --features release"
    assertions:
      - "cap help lists llm, upgrade, issue, and report-issue compatibility commands"
      - "installed cap frontend delegates standard commands through the cap-full sibling"
      - "installed cap frontend preserves the caller environment for cap-full passthrough commands"
      - "cap llm renders cap-specific offline docs through cli-std"
      - "cap issue create and report-issue dry-run payloads carry app:cap diagnostics"
      - "release-feature builds enable cli-std online paths"
```

## Agent Hook Installation EC
<!-- type: e2e-test lang: yaml -->

```yaml
e2e_tests:
  - id: cap-agent-hook-installation
    capability_id: agent-hook-installation
    claim_id: claude-and-codex-hook-installation
    contract_id: agent-hook-installation
    category: behavior
    command: "cargo test -p cap hook_install -- --nocapture && cargo test -p cap hook -- --nocapture"
    assertions:
      - "cap on/off global hook lifecycle is idempotent for Claude Code, Codex CLI, and AGY"
      - "agent hooks deny destructive commands outside the current Git workspace"
      - "unrelated user hooks are preserved"
      - "hook payload adapters rewrite Bash commands without making cap a hard dependency"
```

## Hook Payload Rewrite Adapters EC
<!-- type: e2e-test lang: yaml -->

```yaml
e2e_tests:
  - id: cap-hook-payload-rewrite-adapters
    capability_id: agent-hook-installation
    claim_id: hook-payload-rewrite-adapters
    contract_id: hook-payload-rewrite-adapters
    category: behavior
    command: "cargo test -p cap hook -- --nocapture"
    assertions:
      - "hook payload adapters rewrite Bash commands without making cap a hard dependency"
      - "recursive cap invocations and empty commands remain fail-open"
```

## Shell Execution EC
<!-- type: e2e-test lang: yaml -->

```yaml
e2e_tests:
  - id: cap-command-lease-throttling
    capability_id: command-lease-throttling
    claim_id: lease-admission-and-process-supervision
    contract_id: command-lease-throttling
    category: behavior
    command: "cargo test -p cap throttle -- --nocapture && cargo test -p cap sampler -- --nocapture"
    assertions:
      - "memory and CPU pressure never pause or kill cap commands"
      - "explicit timeout outcomes preserve structured diagnostics"
      - "sampling supplies telemetry and explicit idle-timeout checks only"
```

## Telemetry Sampling EC
<!-- type: e2e-test lang: yaml -->

```yaml
e2e_tests:
  - id: cap-memory-and-cpu-pressure-sampling
    capability_id: command-lease-throttling
    claim_id: memory-and-cpu-pressure-sampling
    contract_id: memory-and-cpu-pressure-sampling
    category: behavior
    command: "cargo test -p cap sampler -- --nocapture"
    assertions:
      - "sampling supplies telemetry and explicit idle-timeout checks only"
      - "command admission never waits for memory or CPU capacity"
```

## Daemon Lifecycle And Status EC
<!-- type: e2e-test lang: yaml -->

```yaml
e2e_tests:
  - id: cap-daemon-lifecycle-and-status
    capability_id: daemon-lifecycle-and-status
    claim_id: daemon-process-lifecycle
    contract_id: daemon-lifecycle-and-status
    category: behavior
    command: "cargo test -p cap daemon -- --nocapture && cargo test -p cap cli -- --nocapture"
    assertions:
      - "daemon lifecycle and liveness surfaces remain callable"
      - "status shows optional daemon leases and cap wait returns success immediately"
      - "command leases stay isolated by process group"
```

## CLI Status And Wait Surfaces EC
<!-- type: e2e-test lang: yaml -->

```yaml
e2e_tests:
  - id: cap-cli-status-and-wait-surfaces
    capability_id: daemon-lifecycle-and-status
    claim_id: cli-status-and-wait-surfaces
    contract_id: cli-status-and-wait-surfaces
    category: behavior
    command: "cargo test -p cap cli -- --nocapture"
    assertions:
      - "status shows optional daemon leases and cap wait returns success immediately"
      - "command leases stay isolated by process group"
```

## Config And Logging EC
<!-- type: e2e-test lang: yaml -->

```yaml
e2e_tests:
  - id: cap-config-logging-and-reap-policy
    capability_id: config-logging-and-reap-policy
    claim_id: configuration-defaults-and-compatibility
    contract_id: config-logging-and-reap-policy
    category: behavior
    command: "cargo test -p cap config -- --nocapture && cargo test -p cap eventlog -- --nocapture && cargo test -p cap --test behavior_cap_unrestricted_shell"
    assertions:
      - "legacy resource-control keys parse without enforcing limits"
      - "JSONL event log persistence roundtrips run records"
      - "cap never reaps other processes for memory pressure"
```

## Run Log Persistence EC
<!-- type: e2e-test lang: yaml -->

```yaml
e2e_tests:
  - id: cap-run-log-persistence
    capability_id: config-logging-and-reap-policy
    claim_id: run-log-persistence
    contract_id: run-log-persistence
    category: behavior
    command: "cargo test -p cap eventlog -- --nocapture"
    assertions:
      - "JSONL event log persistence roundtrips run records"
      - "run evidence remains durable enough for agent diagnosis"
```

## Retired Reap Policy EC
<!-- type: e2e-test lang: yaml -->

```yaml
e2e_tests:
  - id: cap-reap-allowlist-policy
    capability_id: config-logging-and-reap-policy
    claim_id: reap-allowlist-policy
    contract_id: reap-allowlist-policy
    category: behavior
    command: "cargo test -p cap --test behavior_cap_unrestricted_shell"
    assertions:
      - "cap never reaps other processes for memory pressure"
      - "legacy reap settings cannot enable process reaping"
```
