//! herdr-c1o5 (ADR-0008 amendment 2026-10-25, round-cap exit a], round 2 of
//! the redesign): the API layer's verdict on the agent process a
//! `pane.report_agent` names. The process reads go through [`ProcessTable`],
//! so the verdict is a pure function of what was read.

use std::time::Instant;

use crate::detect::Agent;
use crate::platform::ProcessJobRead;
use crate::terminal::NamedAgentVerdict;

/// The process facts the verdict reads.
pub(crate) trait ProcessTable {
    /// One read of the job facts of `pid`, `None` when it cannot be read.
    fn read(&self, pid: u32) -> Option<ProcessJobRead>;
    fn is_zombie(&self, pid: u32) -> bool;
    /// The agent herdr's detector classifier names for `pid`.
    fn agent(&self, pid: u32) -> Option<Agent>;
}

/// Reads the operating system.
pub(crate) struct PlatformProcessTable;

impl ProcessTable for PlatformProcessTable {
    fn read(&self, pid: u32) -> Option<ProcessJobRead> {
        crate::platform::process_job_read(pid)
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

/// A pane shell: its pid and the start token herdr recorded when it spawned
/// or adopted that shell (ADR-0008 round 2, rule 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShellIncarnation {
    pub pid: u32,
    pub start_token: u64,
}

/// The shells of the herdr panes the verdict compares against.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PaneShells<'a> {
    /// The shell of the pane the report is for, `None` without a runtime or
    /// without a recorded start token.
    pub pane: Option<ShellIncarnation>,
    /// The shell pids of every other pane.
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
        || table.read(agent_pid).map(|read| read.start_token) != Some(agent_start)
        || table.is_zombie(agent_pid)
    {
        return false;
    }
    let Some(pane_shell) = shells.pane.map(|shell| shell.pid) else {
        return false;
    };
    let Some(foreground) = table
        .read(pane_shell)
        .and_then(|read| read.foreground_group)
    else {
        return false;
    };
    // Native: the agent runs in this pane and is shown only in its foreground.
    if let Some(group) = live_process_group(table, pane_shell, agent_pid, agent_start) {
        return group == foreground;
    }
    // Carried: shown through a carrier in this pane's foreground, and not the
    // agent of another pane.
    let (Some(carrier_pid), Some(carrier_start)) = (report.carrier_pid, report.carrier_start)
    else {
        return false;
    };
    live_process_group(table, pane_shell, carrier_pid, carrier_start) == Some(foreground)
        && !shells.others.iter().any(|&shell| {
            live_process_group(table, shell, agent_pid, agent_start).is_some()
        })
}

/// The group of `pid` while it is alive with `start_token` in the session of
/// `shell_pid`.
fn live_process_group(
    table: &impl ProcessTable,
    shell_pid: u32,
    pid: u32,
    start_token: u64,
) -> Option<u32> {
    let read = table.read(pid)?;
    let shell_session = table.read(shell_pid)?.session;
    (!read.exited && read.start_token == start_token && read.session == shell_session)
        .then_some(read.process_group)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    #[derive(Clone, Copy)]
    struct FakeProcess {
        start: u64,
        zombie: bool,
        agent: Option<Agent>,
        session: u32,
        group: u32,
        foreground: Option<u32>,
    }

    /// A process table read one stat line per pid. A pid in `readable_reads`
    /// reads for that many reads and fails from then on.
    #[derive(Default)]
    struct FakeTable {
        processes: HashMap<u32, FakeProcess>,
        readable_reads: HashMap<u32, usize>,
        reads: RefCell<HashMap<u32, usize>>,
        first_read: Cell<Option<Instant>>,
    }

    impl FakeTable {
        fn process(&self, pid: u32) -> Option<FakeProcess> {
            if self.first_read.get().is_none() {
                self.first_read.set(Some(Instant::now()));
            }
            let mut reads = self.reads.borrow_mut();
            let count = reads.entry(pid).or_insert(0);
            *count += 1;
            if self
                .readable_reads
                .get(&pid)
                .is_some_and(|&readable| *count > readable)
            {
                return None;
            }
            self.processes.get(&pid).copied()
        }

        fn set(&mut self, pid: u32, edit: impl FnOnce(&mut FakeProcess)) {
            edit(self.processes.get_mut(&pid).expect("a fixture process"));
        }
    }

    impl ProcessTable for FakeTable {
        fn read(&self, pid: u32) -> Option<ProcessJobRead> {
            self.process(pid).map(|process| ProcessJobRead {
                start_token: process.start,
                exited: process.zombie,
                process_group: process.group,
                session: process.session,
                foreground_group: process.foreground,
            })
        }

        fn is_zombie(&self, pid: u32) -> bool {
            self.process(pid).is_some_and(|process| process.zombie)
        }

        fn agent(&self, pid: u32) -> Option<Agent> {
            self.process(pid).and_then(|process| process.agent)
        }
    }

    const PANE: u32 = 100;
    const OTHER_PANE: u32 = 200;
    const CLIENT: u32 = 110;
    const CLAUDE: u32 = 300;
    const TMUX_SESSION: u32 = 900;
    const STRANGER_CLIENT: u32 = 400;
    const PANE_SHELL: ShellIncarnation = ShellIncarnation {
        pid: PANE,
        start_token: 10,
    };

    fn process(start: u64, session: u32, group: u32) -> FakeProcess {
        FakeProcess {
            start,
            zombie: false,
            agent: None,
            session,
            group,
            foreground: None,
        }
    }

    /// Pane PANE shows the tmux client CLIENT in front; CLAUDE runs in the
    /// tmux server's session, outside every pane.
    fn fixture() -> FakeTable {
        let mut table = FakeTable::default();
        let mut pane = process(10, PANE, PANE);
        pane.foreground = Some(CLIENT);
        table.processes.insert(PANE, pane);
        let mut other = process(20, OTHER_PANE, OTHER_PANE);
        other.foreground = Some(OTHER_PANE);
        table.processes.insert(OTHER_PANE, other);
        table.processes.insert(CLIENT, process(11, PANE, CLIENT));
        let mut claude = process(30, TMUX_SESSION, CLAUDE);
        claude.agent = Some(Agent::Claude);
        table.processes.insert(CLAUDE, claude);
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
            pane: Some(PANE_SHELL),
            others: &[OTHER_PANE],
        };
        judge_named_agent(table, report, shells, Instant::now)
    }

    const CARRIER: Option<(u32, u64)> = Some((CLIENT, 11));
    const NAMED_CLAUDE: Option<(u32, u64)> = Some((CLAUDE, 30));

    #[test]
    fn a_carried_claude_shown_by_the_pane_foreground_is_witnessed() {
        let verdict = judge(&fixture(), report(NAMED_CLAUDE, CARRIER));
        assert!(verdict.witnessed_at().is_some(), "{verdict:?}");
    }

    #[test]
    fn a_claude_of_another_pane_is_refuted() {
        let mut table = fixture();
        table.set(CLAUDE, |claude| claude.session = OTHER_PANE);
        let verdict = judge(&table, report(NAMED_CLAUDE, CARRIER));
        assert_eq!(verdict, NamedAgentVerdict::Refuted);
    }

    // codex c1o5-r3 P2-1: the claude belongs to pane Q. Q's shell cannot be
    // read; then the claude itself cannot be read from each of its reads on,
    // as when it disappears during the membership checks.
    #[test]
    fn a_claude_of_another_pane_is_refuted_when_reads_fail() {
        let mut table = fixture();
        table.set(CLAUDE, |claude| claude.session = OTHER_PANE);
        table.readable_reads.insert(OTHER_PANE, 0);
        let verdict = judge(&table, report(NAMED_CLAUDE, CARRIER));
        assert_eq!(verdict, NamedAgentVerdict::Refuted, "Q's shell unreadable");
        for readable in 0..8 {
            let mut table = fixture();
            table.set(CLAUDE, |claude| claude.session = OTHER_PANE);
            table.readable_reads.insert(CLAUDE, readable);
            let verdict = judge(&table, report(NAMED_CLAUDE, CARRIER));
            assert_eq!(
                verdict,
                NamedAgentVerdict::Refuted,
                "the claude unreadable from its read {}",
                readable + 1
            );
        }
    }

    // codex c1o5-r3 P2-4: P's shell exited and, while PaneDied is queued, its
    // pid belongs to the shell of an unrelated terminal whose foreground
    // carrier the report names; or the shell exited and is not yet reaped; or
    // the shell cannot be read at its first or its second read.
    #[test]
    fn a_reused_pane_shell_pid_is_refuted() {
        let mut reused = fixture();
        reused.set(PANE, |shell| {
            shell.start = 99;
            shell.foreground = Some(STRANGER_CLIENT);
        });
        reused
            .processes
            .insert(STRANGER_CLIENT, process(41, PANE, STRANGER_CLIENT));
        let verdict = judge(&reused, report(NAMED_CLAUDE, Some((STRANGER_CLIENT, 41))));
        assert_eq!(verdict, NamedAgentVerdict::Refuted, "pid reused");

        let mut exited = fixture();
        exited.set(PANE, |shell| shell.zombie = true);
        let verdict = judge(&exited, report(NAMED_CLAUDE, CARRIER));
        assert_eq!(verdict, NamedAgentVerdict::Refuted, "shell exited");

        for readable in 0..2 {
            let mut table = fixture();
            table.readable_reads.insert(PANE, readable);
            let verdict = judge(&table, report(NAMED_CLAUDE, CARRIER));
            assert_eq!(
                verdict,
                NamedAgentVerdict::Refuted,
                "the shell unreadable from its read {}",
                readable + 1
            );
        }
    }

    #[test]
    fn a_background_job_of_the_pane_is_refuted() {
        let mut table = fixture();
        table.set(CLAUDE, |claude| claude.session = PANE);
        for carrier in [None, CARRIER] {
            let verdict = judge(&table, report(NAMED_CLAUDE, carrier));
            assert_eq!(verdict, NamedAgentVerdict::Refuted, "{carrier:?}");
        }
    }

    #[test]
    fn a_native_claude_in_the_pane_foreground_is_witnessed() {
        let mut table = fixture();
        table.set(CLAUDE, |claude| claude.session = PANE);
        table.set(PANE, |shell| shell.foreground = Some(CLAUDE));
        let verdict = judge(&table, report(NAMED_CLAUDE, None));
        assert!(verdict.witnessed_at().is_some(), "{verdict:?}");
    }

    #[test]
    fn a_carrier_outside_the_pane_foreground_is_refuted() {
        let mut backgrounded = fixture();
        backgrounded.set(PANE, |shell| shell.foreground = Some(120));
        let verdict = judge(&backgrounded, report(NAMED_CLAUDE, CARRIER));
        assert_eq!(verdict, NamedAgentVerdict::Refuted, "carrier not in front");
        for carrier in [
            None,
            Some((CLIENT, 12)),
            Some((CLAUDE, 30)),
            Some((OTHER_PANE, 1)),
        ] {
            let verdict = judge(&fixture(), report(NAMED_CLAUDE, carrier));
            assert_eq!(verdict, NamedAgentVerdict::Refuted, "{carrier:?}");
        }
    }

    #[test]
    fn a_live_process_that_is_not_the_claimed_agent_is_refuted() {
        let mut sleep = fixture();
        sleep.set(CLAUDE, |claude| claude.agent = None);
        let mut zombie = fixture();
        zombie.set(CLAUDE, |claude| claude.zombie = true);
        let mut custom_label = report(NAMED_CLAUDE, CARRIER);
        custom_label.claimed = None;
        for (table, report, case) in [
            (&sleep, report(NAMED_CLAUDE, CARRIER), "not a claude"),
            (&zombie, report(NAMED_CLAUDE, CARRIER), "zombie"),
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
        let verdict =
            judge_named_agent(&fixture(), report(NAMED_CLAUDE, CARRIER), no_shell, Instant::now);
        assert_eq!(verdict, NamedAgentVerdict::Refuted, "pane without a shell");
    }

    #[test]
    fn a_report_without_a_start_token_names_no_process() {
        let table = fixture();
        let mut pid_only = report(NAMED_CLAUDE, CARRIER);
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
        let verdict = judge(&table, report(NAMED_CLAUDE, CARRIER));
        let first_read = table.first_read.get().expect("the verdict read the table");
        assert!(
            verdict
                .witnessed_at()
                .is_some_and(|observed_at| observed_at <= first_read),
            "{verdict:?}"
        );
    }
}
