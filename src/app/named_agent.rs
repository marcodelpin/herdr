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

/// The verdict on the agent process a report names (ADR-0008 amendment
/// 2026-10-25, round-cap exit a]). `clock` is read once, before the first
/// process read: the witness.
pub(crate) fn judge_named_agent(
    table: &impl ProcessTable,
    report: NamedAgentReport,
    shells: PaneShells<'_>,
    clock: impl FnOnce() -> Instant,
) -> NamedAgentVerdict {
    let (Some(agent_pid), Some(agent_start)) = (report.agent_pid, report.agent_start) else {
        return NamedAgentVerdict::Unnamed;
    };
    let observed_at = clock();
    if named_agent_is_shown(table, report, agent_pid, agent_start, shells) {
        NamedAgentVerdict::Witnessed { observed_at }
    } else {
        NamedAgentVerdict::Refuted
    }
}

fn named_agent_is_shown(
    table: &impl ProcessTable,
    report: NamedAgentReport,
    agent_pid: u32,
    agent_start: u64,
    shells: PaneShells<'_>,
) -> bool {
    let Some(claimed) = report.claimed else {
        return false;
    };
    // Classify first, then confirm the identity: a pid reused between the
    // two reads fails the start token.
    if table.agent(agent_pid) != Some(claimed)
        || table.start_token(agent_pid) != Some(agent_start)
        || table.is_zombie(agent_pid)
    {
        return false;
    }
    let Some(pane_shell) = shells.pane else {
        return false;
    };
    let Some(foreground) = table.foreground_process_group(pane_shell) else {
        return false;
    };
    // Native: the agent runs in this pane and is shown only in its foreground.
    if let Some(group) = table.live_process_group(pane_shell, agent_pid, agent_start) {
        return group == foreground;
    }
    // Carried: shown through a carrier in this pane's foreground, and not the
    // agent of another pane.
    let (Some(carrier_pid), Some(carrier_start)) = (report.carrier_pid, report.carrier_start)
    else {
        return false;
    };
    table.live_process_group(pane_shell, carrier_pid, carrier_start) == Some(foreground)
        && !shells.others.iter().any(|&shell| {
            table
                .live_process_group(shell, agent_pid, agent_start)
                .is_some()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::HashMap;

    #[derive(Clone, Copy)]
    struct FakeProcess {
        start: u64,
        zombie: bool,
        agent: Option<Agent>,
        session_shell: Option<u32>,
        group: u32,
    }

    #[derive(Default)]
    struct FakeTable {
        processes: HashMap<u32, FakeProcess>,
        foreground: HashMap<u32, u32>,
        first_read: Cell<Option<Instant>>,
    }

    impl FakeTable {
        fn read(&self) {
            if self.first_read.get().is_none() {
                self.first_read.set(Some(Instant::now()));
            }
        }
    }

    impl ProcessTable for FakeTable {
        fn start_token(&self, pid: u32) -> Option<u64> {
            self.read();
            self.processes.get(&pid).map(|process| process.start)
        }

        fn is_zombie(&self, pid: u32) -> bool {
            self.read();
            self.processes
                .get(&pid)
                .is_some_and(|process| process.zombie)
        }

        fn agent(&self, pid: u32) -> Option<Agent> {
            self.read();
            self.processes.get(&pid).and_then(|process| process.agent)
        }

        fn foreground_process_group(&self, shell_pid: u32) -> Option<u32> {
            self.read();
            self.foreground.get(&shell_pid).copied()
        }

        fn live_process_group(&self, shell_pid: u32, pid: u32, start_token: u64) -> Option<u32> {
            self.read();
            let process = self.processes.get(&pid)?;
            (!process.zombie
                && process.start == start_token
                && process.session_shell == Some(shell_pid))
            .then_some(process.group)
        }
    }

    const PANE: u32 = 100;
    const OTHER_PANE: u32 = 200;
    const CLIENT: u32 = 110;
    const CLAUDE: u32 = 300;

    /// Pane PANE shows the tmux client CLIENT in front; CLAUDE runs outside
    /// every pane.
    fn fixture() -> FakeTable {
        let mut table = FakeTable::default();
        table.processes.insert(
            CLIENT,
            FakeProcess {
                start: 11,
                zombie: false,
                agent: None,
                session_shell: Some(PANE),
                group: CLIENT,
            },
        );
        table.processes.insert(
            CLAUDE,
            FakeProcess {
                start: 30,
                zombie: false,
                agent: Some(Agent::Claude),
                session_shell: None,
                group: CLAUDE,
            },
        );
        table.foreground.insert(PANE, CLIENT);
        table.foreground.insert(OTHER_PANE, OTHER_PANE);
        table
    }

    fn report(agent: Option<(u32, u64)>, carrier: Option<(u32, u64)>) -> NamedAgentReport {
        NamedAgentReport {
            claimed: Some(Agent::Claude),
            agent_pid: agent.map(|(pid, _)| pid),
            agent_start: agent.map(|(_, start)| start),
            carrier_pid: carrier.map(|(pid, _)| pid),
            carrier_start: carrier.map(|(_, start)| start),
        }
    }

    fn judge(table: &FakeTable, report: NamedAgentReport) -> NamedAgentVerdict {
        let shells = PaneShells {
            pane: Some(PANE),
            others: &[OTHER_PANE],
        };
        judge_named_agent(table, report, shells, Instant::now)
    }

    const CARRIER: Option<(u32, u64)> = Some((CLIENT, 11));

    #[test]
    fn a_carried_claude_shown_by_the_pane_foreground_is_witnessed() {
        let verdict = judge(&fixture(), report(Some((CLAUDE, 30)), CARRIER));
        assert!(verdict.witnessed_at().is_some(), "{verdict:?}");
    }

    #[test]
    fn a_claude_of_another_pane_is_refuted() {
        let mut table = fixture();
        table.processes.get_mut(&CLAUDE).unwrap().session_shell = Some(OTHER_PANE);
        let verdict = judge(&table, report(Some((CLAUDE, 30)), CARRIER));
        assert_eq!(verdict, NamedAgentVerdict::Refuted);
    }

    #[test]
    fn a_background_job_of_the_pane_is_refuted() {
        let mut table = fixture();
        table.processes.get_mut(&CLAUDE).unwrap().session_shell = Some(PANE);
        for carrier in [None, CARRIER] {
            let verdict = judge(&table, report(Some((CLAUDE, 30)), carrier));
            assert_eq!(verdict, NamedAgentVerdict::Refuted, "{carrier:?}");
        }
    }

    #[test]
    fn a_native_claude_in_the_pane_foreground_is_witnessed() {
        let mut table = fixture();
        table.processes.get_mut(&CLAUDE).unwrap().session_shell = Some(PANE);
        table.foreground.insert(PANE, CLAUDE);
        let verdict = judge(&table, report(Some((CLAUDE, 30)), None));
        assert!(verdict.witnessed_at().is_some(), "{verdict:?}");
    }

    #[test]
    fn a_carrier_outside_the_pane_foreground_is_refuted() {
        let mut backgrounded = fixture();
        backgrounded.foreground.insert(PANE, 120);
        let verdict = judge(&backgrounded, report(Some((CLAUDE, 30)), CARRIER));
        assert_eq!(verdict, NamedAgentVerdict::Refuted, "carrier not in front");
        for carrier in [
            None,
            Some((CLIENT, 12)),
            Some((CLAUDE, 30)),
            Some((OTHER_PANE, 1)),
        ] {
            let verdict = judge(&fixture(), report(Some((CLAUDE, 30)), carrier));
            assert_eq!(verdict, NamedAgentVerdict::Refuted, "{carrier:?}");
        }
    }

    #[test]
    fn a_live_process_that_is_not_the_claimed_agent_is_refuted() {
        let mut sleep = fixture();
        sleep.processes.get_mut(&CLAUDE).unwrap().agent = None;
        let mut zombie = fixture();
        zombie.processes.get_mut(&CLAUDE).unwrap().zombie = true;
        let mut custom_label = report(Some((CLAUDE, 30)), CARRIER);
        custom_label.claimed = None;
        for (table, report, case) in [
            (&sleep, report(Some((CLAUDE, 30)), CARRIER), "not a claude"),
            (&zombie, report(Some((CLAUDE, 30)), CARRIER), "zombie"),
            (
                &fixture(),
                report(Some((CLAUDE, 31)), CARRIER),
                "start token",
            ),
            (&fixture(), custom_label, "unclassified label"),
        ] {
            assert_eq!(judge(table, report), NamedAgentVerdict::Refuted, "{case}");
        }
        let no_shell = PaneShells {
            pane: None,
            others: &[],
        };
        let verdict = judge_named_agent(
            &fixture(),
            report(Some((CLAUDE, 30)), CARRIER),
            no_shell,
            Instant::now,
        );
        assert_eq!(verdict, NamedAgentVerdict::Refuted, "pane without a shell");
    }

    #[test]
    fn a_report_without_a_start_token_names_no_process() {
        let table = fixture();
        let mut pid_only = report(Some((CLAUDE, 30)), CARRIER);
        pid_only.agent_start = None;
        assert_eq!(judge(&table, pid_only), NamedAgentVerdict::Unnamed);
        assert_eq!(
            judge(&table, report(None, CARRIER)),
            NamedAgentVerdict::Unnamed
        );
        assert!(table.first_read.get().is_none(), "no process was read");
    }

    #[test]
    fn the_witness_is_taken_before_the_first_read() {
        let table = fixture();
        let verdict = judge(&table, report(Some((CLAUDE, 30)), CARRIER));
        let first_read = table.first_read.get().expect("the verdict read the table");
        assert!(
            verdict
                .witnessed_at()
                .is_some_and(|observed_at| observed_at <= first_read),
            "{verdict:?}"
        );
    }
}
