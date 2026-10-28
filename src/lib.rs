// SPEC-MANAGED: tech-design/semantic/cap-src.md#exports
// CODEGEN-BEGIN
//! cap — efficient shell tools with native command and pipeline execution.
//!
//! Supported commands use native implementations. Other commands run directly
//! with their original arguments or fall back to Bash.
//!
//! Architecture:
//!
//! ```text
//!   cap <cmd> → command planner → native implementation
//!                              → direct command / Bash fallback
//! ```
//!
//! No memory or CPU limits. No automatic priority changes or process reaping.
//! Explicit timeouts, signal forwarding, and command logs use local supervision.
//! The daemon API remains available for compatibility.

// Client-side primitives live in this crate so the daemon, CLI, and any cap
// library consumers share the same wire protocol, state paths, and supervised
// run helper.

pub use daemon::is_running;

pub mod agent_guard;
pub mod cli;
pub mod client;
pub mod command_planner;
pub mod config;
pub mod daemon;
pub mod eventlog;
pub mod hook;
pub mod hook_install;
pub mod managed_run;
pub mod paths;
pub mod protocol;
pub mod resident_shell;
pub mod sampler;
pub mod session_queue;
pub mod supervisor;
pub mod throttle;
pub mod tool_routing;
// CODEGEN-END
