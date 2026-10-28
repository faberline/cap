// SPEC-MANAGED: tech-design/semantic/cap-src.md#schema
// CODEGEN-BEGIN
//! CLI surface — clap-derive command tree.
//!
//! ```text
//!   cap on|off [claude|codex|agy] [--print] # global agent hooks
//!   cap <cmd> [args...]           # default: wrap cmd
//!   cap run -- <cmd> [args...]    # explicit
//!   cap run "<bash command>"       # string command entrypoint for hooks
//!   cap explain -- <cmd> [args...] # show command replacement decision
//!   cap daemon start|stop|status|run
//!   cap status                    # compatibility daemon + agent hooks
//!   cap ps                        # alias
//!   cap config show|init          # `config init` writes ~/.cap/config.toml
//!   cap ping
//!   cap llm [--topic <topic>] [--format md|json]
//!   cap upgrade [--version <tag>] [--check]
//!   cap issue search|view|create
//! ```

use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::client::Client;
use crate::command_planner::{self, CommandPlan, ExternalPlan};
use crate::config::Config;
use crate::daemon;
use crate::hook_install;
use crate::protocol::{LeaseState, Request, Response};
use crate::resident_shell::{
    ResidentLightShellPlan, ResidentLightShellRun, ResidentLightShellSession,
};
use crate::session_queue::{self, QueueDecision};
use crate::supervisor::SpawnSpec;

const TOOL: cli_std::ToolInfo = cli_std::ToolInfo {
    project: "cap",
    repo: "faberline/cap",
    target: env!("CAP_TARGET"),
    version: env!("CARGO_PKG_VERSION"),
    git_sha: env!("CAP_GIT_SHA"),
    built_at: env!("CAP_BUILT_AT"),
};

const LLM_TOPICS: &[cli_std::llm::Topic] = &[
    cli_std::llm::Topic {
        id: "workflow",
        summary: "agent hook setup and command wrapping mental model",
        body: "\
# cap workflow

cap runs shell tools efficiently. Start with
`cap on` installs global hooks for Claude Code, Codex CLI, and AGY. Claude and
Codex Bash commands are rewritten to `cap run '<original command>'`; the AGY
hook currently supplies the same destructive-command guard for `run_command`.
cap then chooses a native fast path, a session queue barrier, or direct Bash
fallback. Commands do not need a daemon or wait for memory or CPU capacity.

Use `cap status` to inspect agent hooks and the compatibility daemon.
Use `cap explain -- <command>` to inspect a native replacement decision.",
    },
    cli_std::llm::Topic {
        id: "commands",
        summary: "task to command cheat sheet",
        body: "\
# cap commands

| task | command |
|---|---|
| enable agent hooks | `cap on` |
| disable agent hooks | `cap off` |
| preview hook snippets | `cap on --print` |
| wrap one command string | `cap run 'cargo test -p cap'` |
| wrap argv directly | `cap run -- cargo test -p cap` |
| inspect replacement decision | `cap explain -- find . -type f` |
| show compatibility daemon and agent hooks | `cap status` |
| search cap issues | `cap issue search queue` |
| file a diagnostics issue | `cap issue create --title 'cap: ...' ...` |",
    },
    cli_std::llm::Topic {
        id: "boundaries",
        summary: "what cap does not claim",
        body: "\
# cap boundaries

cap optimizes supported shell commands while preserving their behavior.
Unsupported shell syntax falls back to Bash. cap does not set memory or CPU
limits, change process priority, or stop other tools. Only explicit timeout
flags can stop a command. cap is not a full replacement shell.",
    },
];

#[derive(Parser, Debug)]
#[command(
    name = "cap",
    version,
    about = "Efficient shell tools with native command and pipeline execution",
    trailing_var_arg = true
)]
/// @spec tech-design/semantic/cap-src.md#schema
pub struct Cli {
    /// Emit the selected command route as one JSON line on stderr.
    #[arg(long)]
    pub trace_routing: bool,
    #[command(subcommand)]
    pub command: Option<Cmd>,

    /// Default form — anything after `cap` that isn't a known
    /// subcommand is treated as the command to wrap.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub passthrough: Vec<String>,
}

/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Print offline agent-facing docs for driving cap.
    Llm(LlmArgs),
    /// Self-update cap from cap@* GitHub releases.
    Upgrade(UpgradeArgs),
    /// Search, view, and file cap issues on the axiom tracker.
    Issue {
        #[command(subcommand)]
        action: IssueCmd,
    },
    /// Deprecated alias for `cap issue create`.
    #[command(name = "report-issue")]
    ReportIssue(ReportIssueArgs),
    /// Run a command under cap (explicit form).
    Run(RunArgs),
    /// Show how cap would execute a command.
    Explain(ExplainArgs),
    /// Daemon lifecycle.
    Daemon {
        #[command(subcommand)]
        action: DaemonCmd,
    },
    /// Print compatibility daemon leases and agent hook status.
    Status,
    /// Alias of `status`.
    Ps,
    /// Show or initialize ~/.cap/config.toml.
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// Liveness probe.
    Ping,
    /// Compatibility command. Return success immediately.
    Wait {
        /// Accepted for compatibility; no capacity wait is performed.
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Enable the PreToolUse hook in your coding agents.
    ///
    /// With no arguments, installs globally into Claude Code, Codex CLI,
    /// and AGY. Cap never installs project-local hooks: every agent receives
    /// the same shell-tool routing. Pass an agent name to narrow it
    /// (`cap on claude`) or `--print` to preview without writing anything.
    On {
        /// Print the snippet(s) to stdout and exit; do not modify files.
        #[arg(long)]
        print: bool,
        /// Limit to specific agents. Default: all of them.
        #[arg(value_enum)]
        agents: Vec<AgentArg>,
    },
    /// Disable cap's PreToolUse hook without affecting unrelated agent hooks.
    Off {
        /// Print the target path(s) without modifying files.
        #[arg(long)]
        print: bool,
        /// Limit to specific agents. Default: all of them.
        #[arg(value_enum)]
        agents: Vec<AgentArg>,
    },
    /// Deprecated alias for `cap on`.
    #[command(hide = true)]
    Init {
        #[arg(long)]
        print: bool,
        #[arg(value_enum)]
        agents: Vec<AgentArg>,
    },
    /// Hook adapters (Claude Code, etc.).
    Hook {
        #[command(subcommand)]
        action: HookCmd,
    },
}

/// @spec tech-design/interfaces/cli/adopt-cli-convention-llm-upgrade-report-issue-via-cclab-cli-std.md
#[derive(Parser, Debug)]
pub struct LlmArgs {
    /// Topic to print: outline (default), workflow, commands, boundaries.
    #[arg(long, default_value = "outline")]
    pub topic: String,
    /// Output format.
    #[arg(long, value_parser = ["md", "json"], default_value = "md")]
    pub format: String,
}

/// @spec tech-design/interfaces/cli/adopt-cli-convention-llm-upgrade-report-issue-via-cclab-cli-std.md
#[derive(Parser, Debug)]
pub struct UpgradeArgs {
    /// Install a specific release tag, e.g. cap@0.3.62 or 0.3.62.
    #[arg(long = "version")]
    pub tag: Option<String>,
    /// Only report whether a newer release exists; do not install.
    #[arg(long)]
    pub check: bool,
}

/// @spec tech-design/interfaces/cli/adopt-cli-convention-llm-upgrade-report-issue-via-cclab-cli-std.md
#[derive(Subcommand, Debug)]
pub enum IssueCmd {
    /// Search cap issues; omit the query to list recent open app:cap issues.
    Search(IssueSearchArgs),
    /// Print a single issue by number.
    View(IssueViewArgs),
    /// File a structured diagnostics-rich issue.
    Create(IssueCreateArgs),
}

/// @spec tech-design/interfaces/cli/adopt-cli-convention-llm-upgrade-report-issue-via-cclab-cli-std.md
#[derive(Args, Debug)]
pub struct IssueSearchArgs {
    /// Search text.
    #[arg(num_args = 0..)]
    pub query: Vec<String>,
    /// Issue state filter.
    #[arg(long, default_value = "open", value_parser = ["open", "closed", "all"])]
    pub state: String,
    /// Max results.
    #[arg(long, default_value_t = 20)]
    pub limit: u32,
}

/// @spec tech-design/interfaces/cli/adopt-cli-convention-llm-upgrade-report-issue-via-cclab-cli-std.md
#[derive(Args, Debug)]
pub struct IssueViewArgs {
    /// Issue number.
    pub number: u64,
}

/// @spec tech-design/interfaces/cli/adopt-cli-convention-llm-upgrade-report-issue-via-cclab-cli-std.md
#[derive(Args, Debug)]
pub struct IssueCreateArgs {
    /// Issue title. Defaults to a title derived from the message.
    #[arg(long)]
    pub title: Option<String>,
    /// Print the issue without creating it.
    #[arg(long)]
    pub dry_run: bool,
    /// Free-text problem description.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub message: Vec<String>,
}

/// @spec tech-design/interfaces/cli/adopt-cli-convention-llm-upgrade-report-issue-via-cclab-cli-std.md
#[derive(Parser, Debug)]
pub struct ReportIssueArgs {
    /// Issue title.
    #[arg(long)]
    pub title: Option<String>,
    /// Print the issue without creating it.
    #[arg(long)]
    pub dry_run: bool,
    /// Free-text problem description.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub message: Vec<String>,
}

/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Parser, Debug)]
pub struct RunArgs {
    /// Human-readable label shown in `cap status`.
    #[arg(long)]
    pub label: Option<String>,
    /// Stop an external command after this many seconds. Default: disabled.
    /// Send SIGTERM, then SIGKILL after the configured grace period.
    #[arg(long)]
    pub timeout: Option<u64>,
    /// Stop an external command after this many seconds without CPU progress.
    /// Default: disabled.
    #[arg(long)]
    pub idle_timeout: Option<u64>,
    /// The command to run. Pass argv after `--`, or one Bash command string.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Parser, Debug)]
pub struct ExplainArgs {
    /// Print a structured route without executing the command.
    #[arg(long)]
    pub json: bool,
    /// The command to explain, after `--`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Subcommand, Debug)]
pub enum DaemonCmd {
    /// Spawn a background daemon if none is running.
    Start,
    /// Run the daemon in the foreground (used internally).
    Run,
    /// Ask the daemon to exit cleanly.
    Stop,
    /// Is a daemon currently up?
    Status,
}

/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Print the effective config.
    Show,
    /// Write a default config to ~/.cap/config.toml if missing.
    Init,
}

/// @spec tech-design/semantic/cap-src.md#schema
#[derive(Subcommand, Debug)]
pub enum HookCmd {
    /// PreToolUse adapter for Bash (the runtime hook entrypoint that
    /// `cap on` registers — you normally don't run this by hand).
    /// Reads the hook event from stdin and prints a permission-decision
    /// JSON that rewrites the command to run under cap.
    Bash {
        /// Emit Codex CLI hook JSON (`updatedInput`).
        #[arg(long, conflicts_with = "claude_code")]
        codex: bool,
        /// Emit Claude Code hook JSON (`modifiedInput`).
        #[arg(long, conflicts_with = "codex")]
        claude_code: bool,
    },
    /// PreToolUse adapter for AGY `run_command`. It blocks dangerous
    /// commands; AGY does not yet use cap's command-rewrite adapter.
    Agy,
}

/// @spec tech-design/semantic/cap-src.md#schema
#[derive(ValueEnum, Clone, Debug)]
pub enum AgentArg {
    /// Claude Code (~/.claude/settings.json)
    Claude,
    /// Codex CLI (~/.codex/config.toml)
    Codex,
    /// AGY (~/.gemini/config/hooks.json or its legacy global path)
    Agy,
}

/// @spec tech-design/semantic/cap-src.md#schema
pub async fn run() -> Result<ExitCode> {
    init_tracing();
    let cli = Cli::parse();
    crate::tool_routing::set_trace(cli.trace_routing);

    match cli.command {
        Some(Cmd::Llm(args)) => handle_llm(args),
        Some(Cmd::Upgrade(args)) => handle_upgrade(args).await,
        Some(Cmd::Issue { action }) => handle_issue(action).await,
        Some(Cmd::ReportIssue(args)) => handle_report_issue(args).await,
        Some(Cmd::Daemon { action }) => handle_daemon(action).await,
        Some(Cmd::Status) | Some(Cmd::Ps) => handle_status().await,
        Some(Cmd::Config { action }) => handle_config(action),
        Some(Cmd::Ping) => handle_ping().await,
        Some(Cmd::Wait { timeout }) => handle_wait(timeout).await,
        Some(Cmd::On { print, agents }) => handle_hook_switch(agents, print, HookSwitch::On),
        Some(Cmd::Off { print, agents }) => handle_hook_switch(agents, print, HookSwitch::Off),
        Some(Cmd::Init { print, agents }) => {
            eprintln!("cap init is deprecated; use `cap on`");
            handle_hook_switch(agents, print, HookSwitch::On)
        }
        Some(Cmd::Hook { action }) => handle_hook(action),
        Some(Cmd::Run(args)) => handle_run(args).await,
        Some(Cmd::Explain(args)) => handle_explain(args),
        None => {
            if cli.passthrough.is_empty() {
                use clap::CommandFactory;
                Cli::command().print_help().ok();
                println!();
                Ok(ExitCode::SUCCESS)
            } else {
                handle_run(RunArgs {
                    label: None,
                    timeout: None,
                    idle_timeout: None,
                    command: cli.passthrough,
                })
                .await
            }
        }
    }
}

fn handle_llm(args: LlmArgs) -> Result<ExitCode> {
    let out = cli_std::llm::render(
        TOOL.project,
        TOOL.version,
        LLM_TOPICS,
        &args.topic,
        cli_std::llm::Format::parse(&args.format),
    )?;
    println!("{out}");
    Ok(ExitCode::SUCCESS)
}

async fn handle_upgrade(args: UpgradeArgs) -> Result<ExitCode> {
    cli_std::upgrade::run(
        &TOOL,
        cli_std::upgrade::Options {
            check: args.check,
            tag: args.tag,
            force: false,
            yes: true,
        },
    )
    .await?;
    Ok(ExitCode::SUCCESS)
}

async fn handle_issue(action: IssueCmd) -> Result<ExitCode> {
    match action {
        IssueCmd::Search(args) => {
            cli_std::issue::search(
                &TOOL,
                cli_std::issue::SearchOptions {
                    query: join_words(args.query),
                    state: args.state,
                    limit: args.limit,
                },
            )
            .await?;
        }
        IssueCmd::View(args) => {
            cli_std::issue::view(&TOOL, args.number).await?;
        }
        IssueCmd::Create(args) => {
            cli_std::issue::create(&TOOL, issue_create_options(args)).await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn handle_report_issue(args: ReportIssueArgs) -> Result<ExitCode> {
    cli_std::report_issue::run(&TOOL, report_issue_options(args)).await?;
    Ok(ExitCode::SUCCESS)
}

fn issue_create_options(args: IssueCreateArgs) -> cli_std::issue::CreateOptions {
    let message = join_words(args.message);
    cli_std::issue::CreateOptions {
        title: issue_title(args.title, message.as_deref()),
        message,
        url: None,
        repo: None,
        label: vec!["app:cap".to_string()],
        dry_run: args.dry_run,
        yes: true,
    }
}

fn report_issue_options(args: ReportIssueArgs) -> cli_std::report_issue::Options {
    let message = join_words(args.message);
    cli_std::report_issue::Options {
        title: issue_title(args.title, message.as_deref()),
        message,
        url: None,
        repo: None,
        label: vec!["app:cap".to_string()],
        dry_run: args.dry_run,
        yes: true,
    }
}

fn join_words(words: Vec<String>) -> Option<String> {
    let joined = words.join(" ");
    (!joined.trim().is_empty()).then_some(joined)
}

fn issue_title(explicit: Option<String>, message: Option<&str>) -> String {
    if let Some(title) = explicit.filter(|title| !title.trim().is_empty()) {
        return title;
    }
    let Some(message) = message.map(str::trim).filter(|message| !message.is_empty()) else {
        return "cap: issue report".to_string();
    };
    let head: String = message
        .lines()
        .next()
        .unwrap_or(message)
        .chars()
        .take(72)
        .collect();
    format!("cap: {head}")
}

fn handle_explain(args: ExplainArgs) -> Result<ExitCode> {
    if args.command.is_empty() {
        anyhow::bail!("nothing to explain; usage: cap explain -- <command> [args...]");
    }
    let plan = if args.command.len() == 1 {
        match ResidentLightShellSession::capture().plan_command_string(&args.command[0], None) {
            ResidentLightShellPlan::Native(plan) => CommandPlan::Native(plan),
            ResidentLightShellPlan::BashFallback(plan) => CommandPlan::External(plan),
        }
    } else {
        command_planner::plan(&args.command, None)
    };
    if args.json {
        println!(
            "{}",
            serde_json::to_string(&crate::tool_routing::plan(&plan))?
        );
    } else {
        println!("{}", plan.explain());
    }
    Ok(ExitCode::SUCCESS)
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(match std::env::var("CAP_LOG").as_deref() {
            Ok("trace") => tracing::Level::TRACE,
            Ok("debug") => tracing::Level::DEBUG,
            Ok("warn") => tracing::Level::WARN,
            Ok("error") => tracing::Level::ERROR,
            _ => tracing::Level::INFO,
        })
        .try_init();
}

async fn handle_daemon(action: DaemonCmd) -> Result<ExitCode> {
    match action {
        DaemonCmd::Run => {
            daemon::run_foreground().await?;
            Ok(ExitCode::SUCCESS)
        }
        DaemonCmd::Start => {
            if daemon::is_running()? {
                println!("cap daemon already running");
                return Ok(ExitCode::SUCCESS);
            }
            let pid = daemon::spawn_background()?;
            println!("cap daemon started (pid {pid})");
            Ok(ExitCode::SUCCESS)
        }
        DaemonCmd::Stop => {
            let mut client = Client::connect().await?;
            let resp = client.request(&Request::Shutdown).await?;
            match resp {
                Response::ShuttingDown => {
                    println!("cap daemon: shutting down");
                    Ok(ExitCode::SUCCESS)
                }
                other => {
                    eprintln!("unexpected response: {other:?}");
                    Ok(ExitCode::FAILURE)
                }
            }
        }
        DaemonCmd::Status => {
            let running = daemon::is_running()?;
            println!(
                "cap daemon: {}",
                if running { "running" } else { "stopped" }
            );
            Ok(if running {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

async fn handle_status() -> Result<ExitCode> {
    println!("cap: efficient shell tools; resource limits disabled");
    let mut client = match Client::connect().await {
        Ok(c) => c,
        Err(_) => {
            println!("cap daemon: stopped");
            print_hook_status();
            return Ok(ExitCode::SUCCESS);
        }
    };
    let resp = client.request(&Request::Status).await?;
    let Response::Status(snap) = resp else {
        eprintln!("unexpected response: {resp:?}");
        return Ok(ExitCode::FAILURE);
    };
    println!(
        "compatibility daemon: running {r} leases (direct shell commands run locally)",
        r = snap.running,
    );
    for l in &snap.leases {
        let state = match l.state {
            LeaseState::Pending => "PENDING",
            LeaseState::Running => "RUNNING",
            LeaseState::Paused => "PAUSED ",
            LeaseState::Killing => "KILLING",
            LeaseState::Killed => "KILLED ",
        };
        let pid = l
            .child_pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".into());
        println!(
            "  lease {} {} pid={} age={}s  {}",
            l.lease, state, pid, l.age_secs, l.label
        );
    }
    print_hook_status();
    Ok(ExitCode::SUCCESS)
}

fn handle_config(action: ConfigCmd) -> Result<ExitCode> {
    match action {
        ConfigCmd::Show => {
            let cfg = Config::load()?;
            println!("{}", toml::to_string_pretty(&cfg)?);
            Ok(ExitCode::SUCCESS)
        }
        ConfigCmd::Init => {
            let path = crate::paths::config_path()?;
            if path.exists() {
                println!("config already exists at {}", path.display());
                return Ok(ExitCode::SUCCESS);
            }
            Config::default().save()?;
            println!("wrote default config to {}", path.display());
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn handle_hook(action: HookCmd) -> Result<ExitCode> {
    match action {
        HookCmd::Bash { codex, claude_code } => {
            let agent = match (codex, claude_code) {
                (true, false) => crate::hook::HookAgent::Codex,
                (false, true) => crate::hook::HookAgent::Claude,
                (false, false) => crate::hook::HookAgent::Auto,
                (true, true) => unreachable!("clap conflicts_with prevents both flags"),
            };
            crate::hook::run_bash_hook(agent)?;
            Ok(ExitCode::SUCCESS)
        }
        HookCmd::Agy => {
            crate::hook::run_agy_hook()?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[derive(Clone, Copy)]
enum HookSwitch {
    On,
    Off,
}

/// Toggle the PreToolUse hook for one or more agents.
/// Hooks are always user-global. No agents given = every supported agent.
fn handle_hook_switch(agents: Vec<AgentArg>, print: bool, action: HookSwitch) -> Result<ExitCode> {
    // Default to every supported agent — zero-argument `cap on` is
    // intentionally a complete user-global setup.
    let targets: Vec<hook_install::Agent> = if agents.is_empty() {
        vec![
            hook_install::Agent::Claude,
            hook_install::Agent::Codex,
            hook_install::Agent::Agy,
        ]
    } else {
        agents
            .iter()
            .map(|a| match a {
                AgentArg::Claude => hook_install::Agent::Claude,
                AgentArg::Codex => hook_install::Agent::Codex,
                AgentArg::Agy => hook_install::Agent::Agy,
            })
            .collect()
    };
    for agent in targets {
        match action {
            HookSwitch::On => hook_install::enable(agent, print)?,
            HookSwitch::Off => hook_install::disable(agent, print)?,
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_hook_status() {
    println!("agent hooks:");
    for agent in [
        hook_install::Agent::Claude,
        hook_install::Agent::Codex,
        hook_install::Agent::Agy,
    ] {
        match hook_install::status(agent) {
            Ok(state) => println!("  global {}: {}", agent.label(), state.label()),
            Err(error) => println!("  global {}: unreadable ({error:#})", agent.label()),
        }
    }
}

async fn handle_ping() -> Result<ExitCode> {
    let mut client = Client::connect_or_launch(|| daemon::spawn_background().map(|_| ())).await?;
    let resp = client.request(&Request::Ping).await?;
    println!("{resp:?}");
    Ok(ExitCode::SUCCESS)
}

async fn handle_wait(_timeout: Option<u64>) -> Result<ExitCode> {
    Ok(ExitCode::SUCCESS)
}

/// Forward SIGINT / SIGTERM / SIGHUP from this process to the child's
/// process group, so Ctrl-C in the user's terminal still aborts the
/// wrapped command even though we put it in its own pgid.
async fn handle_run(args: RunArgs) -> Result<ExitCode> {
    if args.command.is_empty() {
        anyhow::bail!(
            "nothing to run; usage: cap run \"<command>\" or cap run -- <command> [args...]"
        );
    }
    if args.command.len() == 1 {
        match session_queue::handle_command_string(&args.command[0])? {
            QueueDecision::ContinueSynchronously => {}
            decision => {
                if let Some(code) = decision.exit_code() {
                    return Ok(code);
                }
            }
        }
        let timeout = args.timeout;
        let idle_timeout = args.idle_timeout;
        let session = ResidentLightShellSession::capture();
        return match session.run_command_string(&args.command[0], args.label)? {
            ResidentLightShellRun::Native(code) => Ok(code),
            ResidentLightShellRun::BashFallback(external) => {
                handle_external_run(external, timeout, idle_timeout).await
            }
        };
    }
    let timeout = args.timeout;
    let idle_timeout = args.idle_timeout;
    let plan = command_planner::plan(&args.command, args.label);
    match plan {
        CommandPlan::Native(native) => command_planner::run_native(&native),
        CommandPlan::External(external) => {
            handle_external_run(external, timeout, idle_timeout).await
        }
    }
}

async fn handle_external_run(
    plan: ExternalPlan,
    timeout_secs: Option<u64>,
    idle_timeout_secs: Option<u64>,
) -> Result<ExitCode> {
    crate::tool_routing::trace(&crate::tool_routing::external(&plan));
    let outcome = crate::managed_run::run_direct(
        SpawnSpec::new(plan.program, plan.args),
        plan.label,
        timeout_secs,
        idle_timeout_secs,
    )
    .await?;
    if let Some(envelope) = &outcome.kill_envelope {
        // `human_message` is pre-formatted multi-line — `eprint!` (not
        // `eprintln!`) so we don't add a trailing blank line.
        eprint!("{}", envelope.human_message);
    }
    Ok(exit_code_from(outcome.status))
}

/// Map a child's `ExitStatus` to a process `ExitCode`. Signal-terminated
/// children (`status.code() == None`, e.g. a cap SIGKILL) map to FAILURE.
fn exit_code_from(status: std::process::ExitStatus) -> ExitCode {
    match status.code() {
        Some(0) => ExitCode::SUCCESS,
        Some(c) if (0..=255).contains(&c) => ExitCode::from(c as u8),
        _ => ExitCode::FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn tool_routing_root_trace_flag_preserves_child_argv() {
        let cli = Cli::try_parse_from([
            "cap",
            "--trace-routing",
            "sh",
            "-c",
            "exit 127",
            "--trace-routing",
        ])
        .expect("the root trace flag must be accepted");
        assert_eq!(cli.passthrough, ["sh", "-c", "exit 127", "--trace-routing"]);
    }

    #[test]
    fn cli_std_convention_help_lists_current_commands_and_compat_alias() {
        let help = Cli::command().render_help().to_string();
        for verb in ["llm", "upgrade", "issue", "report-issue", "on", "off"] {
            assert!(help.contains(verb), "missing {verb} in help:\n{help}");
        }
        assert!(
            !help.contains("\n  init  "),
            "deprecated init must stay hidden:\n{help}"
        );
    }

    #[test]
    fn cli_std_convention_llm_uses_shared_renderer() {
        let out = cli_std::llm::render(
            TOOL.project,
            TOOL.version,
            LLM_TOPICS,
            "outline",
            cli_std::llm::Format::Md,
        )
        .unwrap();
        assert!(out.contains("cap issue search"));
        assert!(out.contains("cap upgrade"));
        assert!(out.contains("workflow"));
    }

    #[test]
    fn cli_std_convention_issue_create_is_project_scoped() {
        let opts = issue_create_options(IssueCreateArgs {
            title: None,
            dry_run: true,
            message: vec!["queue".into(), "barrier".into()],
        });
        assert_eq!(opts.title, "cap: queue barrier");
        assert_eq!(opts.message.as_deref(), Some("queue barrier"));
        assert_eq!(opts.label, vec!["app:cap"]);
        assert!(opts.dry_run);
    }

    #[test]
    fn cli_std_convention_report_issue_alias_uses_same_payload_shape() {
        let opts = report_issue_options(ReportIssueArgs {
            title: Some("custom".into()),
            dry_run: true,
            message: vec!["details".into()],
        });
        assert_eq!(opts.title, "custom");
        assert_eq!(opts.message.as_deref(), Some("details"));
        assert_eq!(opts.label, vec!["app:cap"]);
        assert!(opts.dry_run);
    }

    #[test]
    fn run_args_parse_timeout_and_idle_timeout_flags() {
        let cli = Cli::try_parse_from([
            "cap",
            "run",
            "--timeout",
            "30",
            "--idle-timeout",
            "10",
            "--",
            "cargo",
            "build",
        ])
        .expect("flags parse");
        match cli.command {
            Some(Cmd::Run(args)) => {
                assert_eq!(args.timeout, Some(30));
                assert_eq!(args.idle_timeout, Some(10));
                assert_eq!(args.command, vec!["cargo", "build"]);
            }
            other => panic!("expected Cmd::Run, got {other:?}"),
        }
    }

    #[test]
    fn run_args_timeout_flags_are_optional() {
        let cli = Cli::try_parse_from(["cap", "run", "--", "cargo", "build"]).expect("flags parse");
        match cli.command {
            Some(Cmd::Run(args)) => {
                assert_eq!(args.timeout, None);
                assert_eq!(args.idle_timeout, None);
            }
            other => panic!("expected Cmd::Run, got {other:?}"),
        }
    }

    #[test]
    fn hook_switch_commands_are_global_and_support_all_agents() {
        let on = Cli::try_parse_from(["cap", "on", "agy", "--print"]).expect("on parses");
        match on.command {
            Some(Cmd::On { print, agents }) => {
                assert!(print);
                assert!(matches!(agents.as_slice(), [AgentArg::Agy]));
            }
            other => panic!("expected Cmd::On, got {other:?}"),
        }

        let off = Cli::try_parse_from(["cap", "off", "claude"]).expect("off parses");
        match off.command {
            Some(Cmd::Off { print, agents }) => {
                assert!(!print);
                assert!(matches!(agents.as_slice(), [AgentArg::Claude]));
            }
            other => panic!("expected Cmd::Off, got {other:?}"),
        }

        assert!(Cli::try_parse_from(["cap", "on", "--project"]).is_err());
    }

    #[test]
    fn deprecated_init_still_parses_as_compatibility_command() {
        let cli = Cli::try_parse_from(["cap", "init"]).expect("init parses");
        assert!(matches!(
            cli.command,
            Some(Cmd::Init {
                print: false,
                agents,
            }) if agents.is_empty()
        ));
    }
}
// CODEGEN-END
