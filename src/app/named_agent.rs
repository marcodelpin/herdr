//! herdr-c1o5 (ADR-0008 amendment 2026-10-25, round-cap exit a]): the API
//! layer's verdict on the agent process a `pane.report_agent` names. The
//! process reads go through [`ProcessTable`], so the verdict is a pure
//! function of what was read.

use std::time::Instant;

use crate::detect::Agent;
use crate::terminal::NamedAgentVerdict;

/// The process facts the verdict reads.
pub(crate) trait ProcessTable {
    fn start_token(&self, pid: u32) -> Option<u64>;
    fn is_zombie(&self, pid: u32) -> bool;
    /// The agent herdr's detector classifier names for `pid`.
    fn agent(&self, pid: u32) -> Option<Agent>;
    /// The foreground process group of the terminal of pane shell `shell_pid`.
    fn foreground_process_group(&self, shell_pid: u32) -> Option<u32>;
    /// The process group of `pid` while that process, started at
    /// `start_token`, is alive in the terminal session of `shell_pid`.
    fn live_process_group(&self, shell_pid: u32, pid: u32, start_token: u64) -> Option<u32>;
}

/// Reads the operating system.
pub(crate) struct PlatformProcessTable;

impl ProcessTable for PlatformProcessTable {
    fn start_token(&self, pid: u32) -> Option<u64> {
        crate::platform::process_start_token(pid)
    }

    fn is_zombie(&self, pid: u32) -> bool {
        crate::platform::process_is_zombie(pid)
    }

    fn agent(&self, pid: u32) -> Option<Agent> {
        let process = crate::platform::process_snapshot(pid)?;
        crate::detect::identify_agent_in_job(&crate::platform::ForegroundJob {
            process_group_id: pid,
            processes: vec![process],
        })
        .map(|(agent, _)| agent)
    }

    fn foreground_process_group(&self, shell_pid: u32) -> Option<u32> {
        crate::detect::foreground_process_group_id(shell_pid)
    }

    fn live_process_group(&self, shell_pid: u32, pid: u32, start_token: u64) -> Option<u32> {
        crate::platform::live_pane_process_group(shell_pid, pid, start_token)
    }
}

/// What a report says about its agent process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NamedAgentReport {
    /// The agent the report claims, `None` for a label herdr cannot classify.
    pub claimed: Option<Agent>,
    pub agent_pid: Option<u32>,
    pub agent_start: Option<u64>,
    pub carrier_pid: Option<u32>,
    pub carrier_start: Option<u64>,
}

/// The shells of the herdr panes the verdict compares against.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PaneShells<'a> {
    /// The shell of the pane the report is for, `None` without a runtime.
    pub pane: Option<u32>,
    /// The shells of every other pane.
    pub others: &'a [u32],
}

/// RED baseline (fork 18f6698b, round 2): the named process counts when its
/// pid exists and, when `agent_start` is sent, has that start token.
pub(crate) fn judge_named_agent(
    table: &impl ProcessTable,
    report: NamedAgentReport,
    shells: PaneShells<'_>,
    clock: impl FnOnce() -> Instant,
) -> NamedAgentVerdict {
    let _ = shells;
    let Some(agent_pid) = report.agent_pid else {
        return NamedAgentVerdict::Unnamed;
    };
    let observed_at = clock();
    if table
        .start_token(agent_pid)
        .is_some_and(|token| report.agent_start.is_none_or(|start| start == token))
    {
        NamedAgentVerdict::Witnessed { observed_at }
    } else {
        NamedAgentVerdict::Refuted
    }
}
