//! The first tool-support contract. Probing never reads command input.

use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::command_planner::{CommandPlan, ExternalPlan, NativeCommand, NativePlan, WcCountMode};

static TRACE: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolId {
    Cat,
    WcLines,
    WcBytes,
}

/// Syntax-only support probe. Filesystem eligibility remains with the planner.
/// Every operand is checked before an executor can consume input or output.
pub fn probe(program: &str, args: &[String]) -> Option<ToolId> {
    let base = program.rsplit('/').next()?;
    let (tool, operands) = match base {
        "cat" => (ToolId::Cat, args),
        "wc" => match args.split_first()? {
            (flag, operands) if flag == "-l" => (ToolId::WcLines, operands),
            (flag, operands) if flag == "-c" => (ToolId::WcBytes, operands),
            _ => return None,
        },
        _ => return None,
    };
    operands
        .iter()
        .all(|arg| !arg.starts_with('-'))
        .then_some(tool)
}

#[derive(Debug, Clone, Serialize)]
pub struct Route {
    pub tool: Option<ToolId>,
    pub backend: &'static str,
    pub implementation: &'static str,
    pub reason: &'static str,
}

pub fn native(plan: &NativePlan) -> Route {
    let tool = match &plan.command {
        NativeCommand::Cat(_) => Some(ToolId::Cat),
        NativeCommand::WcLines(wc) => match wc.mode {
            WcCountMode::Lines => Some(ToolId::WcLines),
            WcCountMode::Bytes => Some(ToolId::WcBytes),
            WcCountMode::Words => None,
        },
        _ => None,
    };
    Route {
        tool,
        backend: "rust-native",
        implementation: "native",
        reason: "supported",
    }
}

pub fn external(plan: &ExternalPlan) -> Route {
    let shell = plan.program.rsplit('/').next() == Some("bash")
        && plan.args.first().is_some_and(|arg| arg == "-c");
    Route {
        tool: None,
        backend: if shell { "bash" } else { "original" },
        implementation: if shell { "bash" } else { "original" },
        reason: if shell {
            "shell_syntax"
        } else {
            "unsupported_shape"
        },
    }
}

pub fn plan(plan: &CommandPlan) -> Route {
    match plan {
        CommandPlan::Native(plan) => native(plan),
        CommandPlan::External(plan) => external(plan),
    }
}

pub fn set_trace(enabled: bool) {
    TRACE.store(enabled, Ordering::Relaxed);
}

pub fn trace(route: &Route) {
    if TRACE.load(Ordering::Relaxed) {
        // Fixed fields make serialization infallible and keep diagnostics off stdout.
        if let Ok(json) = serde_json::to_string(route) {
            eprintln!("{json}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_checks_all_operands_without_opening_them() {
        assert_eq!(probe("cat", &[]), Some(ToolId::Cat));
        assert_eq!(probe("cat", &["not-created".into()]), Some(ToolId::Cat));
        assert_eq!(probe("cat", &["file".into(), "-n".into()]), None);
        assert_eq!(probe("wc", &["-l".into()]), Some(ToolId::WcLines));
        assert_eq!(
            probe("wc", &["-c".into(), "file".into()]),
            Some(ToolId::WcBytes)
        );
        assert_eq!(
            probe("wc", &["-l".into(), "file".into(), "-c".into()]),
            None
        );
    }
}
