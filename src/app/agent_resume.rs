use std::time::Instant;

use bytes::Bytes;
use ratatui::layout::Rect;

use super::App;

struct PendingAgentResumeCandidate {
    pane_id: crate::layout::PaneId,
    terminal_id: crate::terminal::TerminalId,
    cwd: std::path::PathBuf,
    plan: crate::agent_resume::AgentResumePlan,
    rows: u16,
    cols: u16,
}

/// ADR-0002, herdr-ct9: prefer the agent's own reported cwd over the pane's
/// shell cwd for a deferred resume, when it names a directory that still
/// exists - a claude tab whose shell sits in one project while its agent
/// works in another otherwise comes back in the wrong folder, or (if the
/// shell's own folder was moved or retired) in a discard directory neither
/// side ever meant. Falls back to the pane's shell cwd exactly as before
/// this fix whenever no agent cwd was reported or it no longer resolves -
/// the existing "Saved directory is unavailable" handling in
/// `start_pending_agent_resume` is unchanged either way.
pub(crate) fn agent_resume_cwd(terminal: &crate::terminal::TerminalState) -> std::path::PathBuf {
    terminal
        .persisted_agent_session
        .as_ref()
        .and_then(|session| session.cwd.as_deref())
        .map(std::path::Path::new)
        .filter(|cwd| cwd.is_dir())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| terminal.cwd.clone())
}

/// herdr-nrr (codex nrr-r1 P1 + P2): a reported resume command (upstream
/// #4687) runs ONLY in the directory it was reserved for at restore time.
/// `agent --continue` names no session of its own - it resumes whatever is
/// latest in its working directory - so the same argv in another directory
/// is another conversation: one the herdr-4r8 live-holder gate (which checks
/// the saved session id) never looked at, and one restore de-duplication
/// never reserved. When the launch directory differs from the reserved one
/// (the agent cwd vanished between restore and launch, or reappeared), the
/// saved session id's native plan is used instead: it names its session, so
/// 4r8 checks the identity actually resumed and two panes can no longer land
/// on the same conversation. With no usable session id the resume is
/// skipped (`Err` returns the dropped plan for the log line). A native plan
/// (`launch_dir == None`) passes through unchanged.
fn deferred_resume_plan_in(
    terminal: &crate::terminal::TerminalState,
    plan: crate::agent_resume::AgentResumePlan,
    cwd: &std::path::Path,
) -> Result<crate::agent_resume::AgentResumePlan, crate::agent_resume::AgentResumePlan> {
    match plan.launch_dir.as_deref() {
        None => return Ok(plan),
        Some(reserved) if reserved == cwd => return Ok(plan),
        Some(_) => {}
    }
    terminal
        .persisted_agent_session
        .as_ref()
        .filter(|session| session.agent == plan.agent)
        .and_then(|session| {
            crate::agent_resume::plan(&session.source, &session.agent, &session.session_ref)
        })
        .ok_or(plan)
}

impl App {
    pub(crate) fn has_pending_agent_resumes(&self) -> bool {
        self.state
            .terminals
            .values()
            .any(|terminal| terminal.pending_agent_resume_plan.is_some())
    }

    pub(crate) fn sync_pending_agent_resume_deadline(&mut self, now: Instant) {
        if !self.has_pending_agent_resumes() {
            self.pending_agent_resume_deadline = None;
            self.next_agent_resume_at = None;
            return;
        }
        if self.pending_agent_resume_candidates().is_empty() {
            self.pending_agent_resume_deadline = None;
            return;
        }
        if let Some(next) = self.next_agent_resume_at {
            self.pending_agent_resume_deadline = Some(next);
        } else {
            self.pending_agent_resume_deadline
                .get_or_insert(now + super::PENDING_AGENT_RESUME_THEME_WAIT);
        }
    }

    pub(crate) fn pending_agent_resume_due(&self, now: Instant) -> bool {
        self.pending_agent_resume_deadline
            .is_some_and(|deadline| now >= deadline)
    }

    pub(crate) fn start_pending_agent_resumes(
        &mut self,
        now: Instant,
        allow_empty_theme: bool,
    ) -> bool {
        // Geometry/theme events can also enter here; they must not bypass spacing.
        if self.next_agent_resume_at.is_some_and(|next| now < next) {
            return false;
        }
        let pending = self.pending_agent_resume_candidates();
        let mut changed = false;
        for PendingAgentResumeCandidate {
            pane_id,
            terminal_id,
            cwd,
            plan,
            rows,
            cols,
        } in pending
        {
            if self.terminal_runtimes.get(&terminal_id).is_some() {
                continue;
            }
            changed |= self.start_pending_agent_resume(
                pane_id,
                terminal_id,
                cwd,
                plan,
                rows,
                cols,
                allow_empty_theme,
            );
            if changed && !self.startup_per_agent_delay.is_zero() {
                self.next_agent_resume_at = Some(now + self.startup_per_agent_delay);
                self.pending_agent_resume_deadline = self.next_agent_resume_at;
                break;
            }
        }

        if changed {
            self.schedule_session_save();
        }
        if !self.has_pending_agent_resumes() || self.pending_agent_resume_candidates().is_empty() {
            self.pending_agent_resume_deadline = None;
        }
        if !self.has_pending_agent_resumes() {
            self.next_agent_resume_at = None;
        }
        changed
    }

    fn pending_agent_resume_candidates(&self) -> Vec<PendingAgentResumeCandidate> {
        let terminal_area = self.state.view.terminal_area;
        if terminal_area.width == 0 || terminal_area.height == 0 {
            return Vec::new();
        };

        let mut pending = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            for (tab_idx, tab) in ws.tabs.iter().enumerate() {
                for info in
                    self.pending_agent_resume_pane_infos(ws_idx, tab_idx, tab, terminal_area)
                {
                    let Some(pane) = tab.panes.get(&info.id) else {
                        continue;
                    };
                    if self
                        .terminal_runtimes
                        .get(&pane.attached_terminal_id)
                        .is_some()
                    {
                        continue;
                    }
                    let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id)
                    else {
                        continue;
                    };
                    let Some(plan) = terminal.pending_agent_resume_plan.clone() else {
                        continue;
                    };
                    pending.push(PendingAgentResumeCandidate {
                        pane_id: info.id,
                        terminal_id: pane.attached_terminal_id.clone(),
                        cwd: agent_resume_cwd(terminal),
                        plan,
                        rows: info.inner_rect.height,
                        cols: info.inner_rect.width,
                    });
                }
            }
        }
        pending
    }

    fn pending_agent_resume_pane_infos(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        tab: &crate::workspace::Tab,
        terminal_area: Rect,
    ) -> Vec<crate::layout::PaneInfo> {
        let mut pane_infos = derived_pending_agent_resume_pane_infos(
            tab,
            terminal_area,
            self.state.pane_borders,
            self.state.pane_gaps,
            self.state.pane_outer_borders,
        );

        if self.state.active == Some(ws_idx)
            && self
                .state
                .workspaces
                .get(ws_idx)
                .is_some_and(|ws| tab_idx == ws.active_tab_index())
        {
            for visible_info in &self.state.view.pane_infos {
                if let Some(info) = pane_infos
                    .iter_mut()
                    .find(|info| info.id == visible_info.id)
                {
                    *info = visible_info.clone();
                } else {
                    pane_infos.push(visible_info.clone());
                }
            }
        }

        pane_infos
    }

    pub(crate) fn start_pending_agent_resume_for_terminal(
        &mut self,
        terminal_id: &crate::terminal::TerminalId,
        rows: u16,
        cols: u16,
        allow_empty_theme: bool,
    ) -> bool {
        if self.terminal_runtimes.get(terminal_id).is_some() {
            return false;
        }
        let Some((pane_id, cwd, plan)) = self.state.workspaces.iter().find_map(|ws| {
            ws.tabs.iter().find_map(|tab| {
                tab.layout.pane_ids().into_iter().find_map(|pane_id| {
                    let pane = tab.panes.get(&pane_id)?;
                    if &pane.attached_terminal_id != terminal_id {
                        return None;
                    }
                    let terminal = self.state.terminals.get(terminal_id)?;
                    Some((
                        pane_id,
                        agent_resume_cwd(terminal),
                        terminal.pending_agent_resume_plan.clone()?,
                    ))
                })
            })
        }) else {
            return false;
        };

        let changed = self.start_pending_agent_resume(
            pane_id,
            terminal_id.clone(),
            cwd,
            plan,
            rows,
            cols,
            allow_empty_theme,
        );
        if changed {
            self.schedule_session_save();
        }
        if !self.has_pending_agent_resumes() {
            self.pending_agent_resume_deadline = None;
        }
        changed
    }

    fn start_pending_agent_resume(
        &mut self,
        pane_id: crate::layout::PaneId,
        terminal_id: crate::terminal::TerminalId,
        cwd: std::path::PathBuf,
        plan: crate::agent_resume::AgentResumePlan,
        rows: u16,
        cols: u16,
        allow_empty_theme: bool,
    ) -> bool {
        let host_terminal_theme = self.state.host_terminal_theme;
        if host_terminal_theme.is_empty() && !allow_empty_theme {
            return false;
        }

        // herdr-nrr: a reported resume command runs only in the directory
        // its dedupe key reserved. See `deferred_resume_plan_in`.
        let reserved_dir = plan.launch_dir.clone();
        let resolved = match self.state.terminals.get(&terminal_id) {
            Some(terminal) => deferred_resume_plan_in(terminal, plan, &cwd),
            None => Ok(plan),
        };
        let plan = match resolved {
            Ok(plan) => {
                if plan.launch_dir.is_none() && reserved_dir.is_some() {
                    tracing::info!(
                        pane = pane_id.raw(),
                        terminal = %terminal_id,
                        agent = %plan.agent,
                        reserved_dir = ?reserved_dir,
                        cwd = %cwd.display(),
                        "reported resume command's directory changed; resuming the saved session id instead"
                    );
                }
                plan
            }
            Err(plan) => {
                tracing::warn!(
                    pane = pane_id.raw(),
                    terminal = %terminal_id,
                    agent = %plan.agent,
                    reserved_dir = ?reserved_dir,
                    cwd = %cwd.display(),
                    "skipping deferred agent resume: the reported resume command's directory changed and no saved session id names the session"
                );
                if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                    terminal.pending_agent_resume_plan = None;
                    terminal.restore_error = Some("The saved resume command belongs to a directory that is no longer available. Restore the directory and restart this session.".into());
                    terminal.revision = terminal.revision.saturating_add(1);
                }
                return true;
            }
        };

        let launcher = self.state.agent_launchers.get(&plan.agent);
        if launcher_has_control_character(launcher) {
            tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                agent = %plan.agent,
                "agent launcher contains a control character; using the stock resume command"
            );
        }
        let Some(resume_command) = resume_command(&plan.argv, launcher) else {
            tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                agent = %plan.agent,
                "failed to start deferred agent resume with empty argv"
            );
            return false;
        };
        let Some(launch_env) = self
            .find_pane(pane_id)
            .and_then(|(ws_idx, _)| self.pane_launch_env(ws_idx, pane_id, Vec::new()))
        else {
            return false;
        };

        if !cwd.is_dir() {
            if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                terminal.pending_agent_resume_plan = None;
                terminal.restore_error = Some("Saved directory is unavailable. Restore the directory and restart this session.".into());
                terminal.revision = terminal.revision.saturating_add(1);
            }
            return true;
        }

        // ADR-0002, herdr-4r8: refuse to resume onto a claude session id
        // that some other live process already has open (measured: two
        // separate interactive `claude` processes ended up attached to the
        // same session through exactly this path). Scoped to
        // ("herdr:claude", "claude") - the one agent this ADR's registry
        // scan and its liveness marker were built and verified against; a
        // registry miss, a non-claude plan, or any I/O failure inside the
        // scan all fail open to today's unconditional resume.
        if plan.agent == "claude" {
            let target_id = self
                .state
                .terminals
                .get(&terminal_id)
                .and_then(|terminal| terminal.persisted_agent_session.as_ref())
                .filter(|session| {
                    session.source == "herdr:claude"
                        && session.agent == "claude"
                        && session.session_ref.kind == crate::agent_resume::AgentSessionRefKind::Id
                })
                .map(|session| session.session_ref.value.clone());
            if let Some(target_id) = target_id {
                if let Some(holder) = crate::agent_session_registry::find_live_holder(&target_id) {
                    if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                        terminal.pending_agent_resume_plan = None;
                        terminal.restore_error = Some(format!(
                            "This conversation is already open in another running Claude session (pid {}{}). Close it and restart this session.",
                            holder.pid,
                            holder
                                .cwd
                                .map(|cwd| format!(", {cwd}"))
                                .unwrap_or_default(),
                        ));
                        terminal.revision = terminal.revision.saturating_add(1);
                    }
                    return true;
                }
            }
        }

        let runtime = match crate::terminal::TerminalRuntime::spawn(
            pane_id,
            rows,
            cols,
            cwd,
            self.state.pane_scrollback_limit_bytes,
            host_terminal_theme,
            self.state.host_terminal_appearance,
            crate::pane::PaneShellConfig::new(&self.state.default_shell, self.state.shell_mode),
            &launch_env,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        ) {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::warn!(
                    pane = pane_id.raw(),
                    terminal = %terminal_id,
                    agent = %plan.agent,
                    err = %err,
                    "failed to start shell for deferred agent resume"
                );
                if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                    terminal.pending_agent_resume_plan = None;
                    terminal.restore_error = Some(format!("Could not start the saved shell: {err}. Fix the shell configuration and restart this session."));
                    terminal.revision = terminal.revision.saturating_add(1);
                }
                return true;
            }
        };

        let mut input = resume_command;
        input.push('\r');
        if let Err(err) = runtime.try_send_bytes(Bytes::from(input)) {
            tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                agent = %plan.agent,
                err = %err,
                "failed to send deferred agent resume command to shell"
            );
            runtime.shutdown();
            return false;
        }

        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.pending_agent_resume_plan = None;
            terminal.respawn_shell_on_exit = false;
        }
        true
    }
}

fn derived_pending_agent_resume_pane_infos(
    tab: &crate::workspace::Tab,
    terminal_area: Rect,
    pane_borders: crate::config::PaneBordersConfig,
    pane_gaps: bool,
    pane_outer_borders: bool,
) -> Vec<crate::layout::PaneInfo> {
    crate::ui::apply_pane_chrome(
        tab.layout.panes(terminal_area),
        pane_borders,
        pane_gaps,
        pane_outer_borders,
    )
    .into_iter()
    .map(|mut info| {
        let pane_inner = crate::ui::pane_inner_rect(info.rect, info.borders);
        info.inner_rect = stable_terminal_inner_rect(pane_inner);
        info
    })
    .collect()
}

fn stable_terminal_inner_rect(pane_inner: Rect) -> Rect {
    if pane_inner.width <= 4 {
        return pane_inner;
    }

    Rect::new(
        pane_inner.x,
        pane_inner.y,
        pane_inner.width.saturating_sub(1),
        pane_inner.height,
    )
}

/// Serializes `argv` the way upstream serializes the stock resume command:
/// each element quoted only if it is not already a "plain word" (see
/// `shell_quote`). Every stock resume argv is nothing but plain words (a
/// name, a flag, a UUID), so the typed text this produces is identical
/// across bash, zsh, Git Bash, PowerShell and cmd (ADR-0001).
fn shell_command_from_argv(argv: &[String]) -> Option<String> {
    let mut parts = argv.iter();
    let first = shell_quote(parts.next()?);
    let mut command = first;
    for part in parts {
        command.push(' ');
        command.push_str(&shell_quote(part));
    }
    Some(command)
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'_' | b'-' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b'='
            )
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// True when `launcher` is configured and not blank but its text contains a
/// control character anywhere, including at either end (CR, LF, tab, or any
/// other C0/C1 control character), so a config value can never type more than
/// one command line.
/// Kept separate from `resume_command` below purely so the caller can log a
/// warning naming the pane and agent; `resume_command` itself treats this
/// case identically to no launcher at all.
fn launcher_has_control_character(launcher: Option<&String>) -> bool {
    launcher.is_some_and(|launcher| {
        !launcher.trim().is_empty() && launcher.chars().any(char::is_control)
    })
}

/// Composes the text to type into a restored pane's shell for `argv` (a
/// resume plan's argv), honoring an optional per-agent launcher prefix from
/// `[session.agent_launchers]` (see `AppState::agent_launchers`).
///
/// With no launcher, or one that is empty, all whitespace, or containing a
/// control character, this is exactly `shell_command_from_argv(argv)`: the
/// stock resume command, byte-identical to what upstream types with no
/// launcher configured (ADR-0001).
///
/// Otherwise, `launcher` is typed VERBATIM as a prefix - never trimmed,
/// quoted or escaped, because it is already exactly what the operator would
/// type into that pane's own shell - followed by a space and `argv[1..]`
/// serialized exactly as upstream serializes the stock resume arguments, or
/// nothing if `argv` has no arguments beyond `argv[0]`.
fn resume_command(argv: &[String], launcher: Option<&String>) -> Option<String> {
    let prefix = launcher
        .map(String::as_str)
        .filter(|launcher| !launcher.trim().is_empty() && !launcher.chars().any(char::is_control));
    let Some(prefix) = prefix else {
        return shell_command_from_argv(argv);
    };
    match argv.get(1..).and_then(shell_command_from_argv) {
        Some(rest) => Some(format!("{prefix} {rest}")),
        None => Some(prefix.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    #[cfg(windows)]
    use std::process::Command;

    #[cfg(unix)]
    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    #[tokio::test]
    async fn pending_agent_resume_spacing_survives_events_and_failed_restores() {
        for delay_ms in [100, 250, 0] {
            let config: crate::config::Config = toml::from_str(&format!(
                "[session]\nstartup_per_agent_delay_ms = {delay_ms}"
            ))
            .unwrap();
            let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(
                &config,
                crate::app::AppPolicy::TEST,
                None,
                api_rx,
                crate::api::EventHub::default(),
            );
            app.state.workspaces = (0..4)
                .map(|_| crate::workspace::Workspace::test_new("restore"))
                .collect();
            app.state.active = Some(0);
            app.state.view.terminal_area = Rect::new(0, 0, 100, 30);
            app.state.ensure_test_terminals();
            let missing = std::env::current_dir()
                .unwrap()
                .join("__missing_resume_cwd__");
            assert!(!missing.exists());
            for terminal in app.state.terminals.values_mut() {
                terminal.cwd = missing.clone();
                terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
                    agent: "codex".into(),
                    argv: vec!["codex".into()],
                    dedupe_key: terminal.id.to_string(),
                    launch_dir: None,
                });
            }
            let now = Instant::now();
            app.sync_pending_agent_resume_deadline(now);
            assert!(!app.start_pending_agent_resumes(now, false));
            assert!(app.start_pending_agent_resumes(now, true));
            if delay_ms != 0 {
                let next = now + std::time::Duration::from_millis(delay_ms);
                assert_eq!(
                    app.state
                        .terminals
                        .values()
                        .filter(|t| t.restore_error.is_some())
                        .count(),
                    1
                );
                // Geometry changes clear the wakeup, but must preserve the launch gap.
                app.pending_agent_resume_deadline = None;
                app.sync_pending_agent_resume_deadline(now);
                assert_eq!(app.pending_agent_resume_deadline, Some(next));
                assert!(!app
                    .start_pending_agent_resumes(next - std::time::Duration::from_millis(1), true));
                // A late wakeup must not release every overdue agent in a burst.
                for processed in 2..=4 {
                    let late = now + std::time::Duration::from_secs(processed * 10);
                    assert!(app.start_pending_agent_resumes(late, true));
                    assert_eq!(
                        app.state
                            .terminals
                            .values()
                            .filter(|t| t.restore_error.is_some())
                            .count(),
                        processed as usize
                    );
                }
            }
            assert!(!app.has_pending_agent_resumes());
            assert!(app.pending_agent_resume_deadline.is_none());
            assert!(app.next_agent_resume_at.is_none());
            assert_eq!(
                app.state
                    .terminals
                    .values()
                    .filter(|t| t.restore_error.is_some())
                    .count(),
                4
            );
        }
    }

    #[cfg(unix)]
    fn long_running_test_argv() -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()]
    }

    #[cfg(unix)]
    fn marker_resume_test_argv() -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '%s' 'restored agent: shell quoted | marker'; sleep 5".into(),
        ]
    }

    // herdr-ct9 (ADR-0002): `agent_resume_cwd` is a pure function of the
    // terminal's reported agent cwd and its pane cwd - no App/runtime
    // fixture needed to exercise it.

    #[test]
    fn agent_restore_ct9_resume_uses_agent_cwd_when_it_exists() {
        let pane_cwd = std::env::current_dir().unwrap();
        let agent_cwd = pane_cwd.join("__herdr_ct9_agent_cwd_exists__");
        std::fs::create_dir_all(&agent_cwd).unwrap();

        let mut terminal =
            crate::terminal::TerminalState::new(crate::terminal::TerminalId::alloc(), pane_cwd);
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("ct9-test-session").unwrap(),
            cwd: Some(agent_cwd.display().to_string()),
        });

        assert_eq!(agent_resume_cwd(&terminal), agent_cwd);

        let _ = std::fs::remove_dir_all(&agent_cwd);
    }

    #[test]
    fn agent_restore_ct9_resume_falls_back_to_pane_cwd_when_agent_cwd_missing_or_not_dir() {
        let pane_cwd = std::env::current_dir().unwrap();

        let mut terminal = crate::terminal::TerminalState::new(
            crate::terminal::TerminalId::alloc(),
            pane_cwd.clone(),
        );
        assert_eq!(
            agent_resume_cwd(&terminal),
            pane_cwd,
            "no agent cwd was ever reported"
        );

        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("ct9-test-session-2").unwrap(),
            cwd: Some(
                pane_cwd
                    .join("__herdr_ct9_agent_cwd_does_not_exist__")
                    .display()
                    .to_string(),
            ),
        });
        assert_eq!(
            agent_resume_cwd(&terminal),
            pane_cwd,
            "a reported agent cwd that no longer resolves must fall back to the pane cwd"
        );
    }

    // herdr-4r8 (ADR-0002): a deferred claude resume must not spawn onto a
    // session id another live process already has open. These tests write
    // a real Claude Code session-registry fixture under an overridden
    // CLAUDE_CONFIG_DIR and use this test process's own pid as the "live"
    // holder - it is guaranteed alive and its start marker is stable for
    // the test's whole lifetime, on every platform this module's crate
    // tests run on (ubuntu-latest, windows-latest).

    struct ClaudeRegistryFixture {
        dir: std::path::PathBuf,
        _lock: crate::integration::IntegrationEnvLock,
    }

    impl ClaudeRegistryFixture {
        fn new(label: &str) -> Self {
            let lock = crate::integration::integration_env_lock();
            let dir = std::env::temp_dir().join(format!(
                "herdr-agent-restore-4r8-test-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(dir.join("sessions")).unwrap();
            std::env::set_var(crate::integration::CLAUDE_CONFIG_DIR_ENV_VAR, &dir);
            Self { dir, _lock: lock }
        }

        /// Registers this test PROCESS's own pid as the live holder of
        /// `session_id` - real, alive, and its start marker is a genuine
        /// reading from this platform's own primitive.
        fn register_self_as_live_holder(&self, session_id: &str) {
            let pid = std::process::id();
            let marker = crate::platform::process_start_marker(pid);
            let json = format!(
                r#"{{"sessionId":"{session_id}","procStart":{marker},"procStartFt":{marker}}}"#,
                marker = marker.unwrap_or(0)
            );
            std::fs::write(self.dir.join("sessions").join(format!("{pid}.json")), json).unwrap();
        }
    }

    impl Drop for ClaudeRegistryFixture {
        fn drop(&mut self) {
            std::env::remove_var(crate::integration::CLAUDE_CONFIG_DIR_ENV_VAR);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_restore_4r8_deferred_resume_onto_live_elsewhere_session_sets_restore_error_and_keeps_session(
    ) {
        let fixture = ClaudeRegistryFixture::new("live-elsewhere");
        fixture.register_self_as_live_holder("claude-live-session");

        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("claude-live-elsewhere");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).unwrap().clone();
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        let session = crate::agent_resume::PersistedAgentSession {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("claude-live-session").unwrap(),
            cwd: None,
        };
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.persisted_agent_session = Some(session.clone());
        terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "claude".into(),
            argv: long_running_test_argv(),
            dedupe_key: "claude-live-session".into(),
            launch_dir: None,
        });

        app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true);

        assert!(
            app.terminal_runtimes.get(&terminal_id).is_none(),
            "must not spawn a second writer onto a session another live process holds"
        );
        let terminal = &app.state.terminals[&terminal_id];
        assert!(terminal.pending_agent_resume_plan.is_none());
        assert_eq!(
            terminal.persisted_agent_session.as_ref(),
            Some(&session),
            "the session reference must survive so a later restart or manual retry can resume it"
        );
        let pid = std::process::id().to_string();
        assert!(
            terminal
                .restore_error
                .as_deref()
                .is_some_and(|err| err.contains(&pid)),
            "restore_error should name the pid already holding the session, got {:?}",
            terminal.restore_error
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_restore_4r8_deferred_resume_onto_not_live_session_spawns_normally() {
        // pid 0 is never a real process on any platform this crate targets
        // (`process_exists` special-cases it on Linux; Windows cannot open
        // a handle to it either) - a portable, deterministic "dead pid".
        let fixture = ClaudeRegistryFixture::new("not-live");
        std::fs::write(
            fixture.dir.join("sessions").join("0.json"),
            r#"{"sessionId":"claude-stale-session","procStart":1}"#,
        )
        .unwrap();

        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("claude-not-live");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).unwrap().clone();
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("claude-stale-session").unwrap(),
            cwd: None,
        });
        terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "claude".into(),
            argv: long_running_test_argv(),
            dedupe_key: "claude-stale-session".into(),
            launch_dir: None,
        });

        app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true);

        assert!(
            app.terminal_runtimes.get(&terminal_id).is_some(),
            "a registry entry for a dead pid must not block the resume"
        );
        let terminal = &app.state.terminals[&terminal_id];
        assert!(terminal.pending_agent_resume_plan.is_none());
        assert!(terminal.restore_error.is_none());

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    // Mutant killer for "the 4r8 scoping is load-bearing, not decorative":
    // the gate reads `plan.agent == "claude"` (cheap outer check) AND filters
    // the target id on `session.source == "herdr:claude" && session.agent ==
    // "claude"` (the actual scoping - `plan` and `terminal.persisted_agent_session`
    // are two different pieces of state and are not guaranteed to agree). A
    // claude PLAN whose terminal happens to carry a stale non-claude
    // persisted session must not have that session's id looked up in the
    // (claude-only) registry just because the plan says "claude" - if the
    // source/agent filter were widened to accept any session kind==Id, this
    // mismatched session would incorrectly get blocked by a live fixture
    // that has nothing to do with it.
    #[cfg(unix)]
    #[tokio::test]
    async fn agent_restore_4r8_scoping_is_limited_to_claude_sessions() {
        let fixture = ClaudeRegistryFixture::new("scoping");
        fixture.register_self_as_live_holder("mismatched-session-that-collides");

        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("mismatched-session");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).unwrap().clone();
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
            source: "herdr:mastracode".into(),
            agent: "mastracode".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id(
                "mismatched-session-that-collides",
            )
            .unwrap(),
            cwd: None,
        });
        terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "claude".into(),
            argv: long_running_test_argv(),
            dedupe_key: "mismatched-session-that-collides".into(),
            launch_dir: None,
        });

        app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true);

        assert!(
            app.terminal_runtimes.get(&terminal_id).is_some(),
            "a persisted session that is not herdr:claude/claude must never be looked up in the claude live-elsewhere registry"
        );
        let terminal = &app.state.terminals[&terminal_id];
        assert!(terminal.restore_error.is_none());

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    // herdr-nrr (ADR-0002 ported onto upstream #4687): #4687 lets a
    // `pane.report_agent_session` carry a `resume_argv`, recorded when the
    // report's session is current. A `herdr:claude` report refused by the
    // 3ir foreground gate changes no state - so it must not record its
    // resume_argv nor advance the sequence watermark, also when it names the
    // session that is ALREADY current (the case `session_report_applied`
    // alone lets through). The test process's own pid is a real process
    // outside the pane's pty session, so the gate sees `Some(false)`; the
    // control report without `agent_pid` (fails open) proves the same path
    // records the command when the gate does not refuse it.
    #[cfg(unix)]
    #[tokio::test]
    async fn agent_restore_3ir_refused_report_does_not_record_its_resume_argv() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("claude-3ir-resume-argv");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).unwrap().clone();
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "claude".into(),
            argv: long_running_test_argv(),
            dedupe_key: "claude-3ir-resume-argv".into(),
            launch_dir: None,
        });
        app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true);
        assert!(
            app.terminal_runtimes.get(&terminal_id).is_some(),
            "the pane needs a real shell for the foreground gate to measure"
        );
        app.handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
            pane_id,
            agent: crate::detect::Agent::Claude,
            observed_at: std::time::Instant::now(),
        });
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        fn report(
            app: &mut App,
            public_pane_id: &str,
            seq: u64,
            agent_pid: Option<u32>,
            resume: bool,
        ) {
            let response = app.handle_api_request(crate::api::schema::Request {
                id: format!("report-{seq}"),
                method: crate::api::schema::Method::PaneReportAgentSession(
                    crate::api::schema::PaneReportAgentSessionParams {
                        pane_id: public_pane_id.to_string(),
                        source: "herdr:claude".into(),
                        agent: "claude".into(),
                        seq: Some(seq),
                        agent_session_id: Some("claude-3ir-session".into()),
                        agent_session_path: None,
                        session_start_source: Some("resume".into()),
                        resume_argv: resume.then(|| {
                            vec![
                                "claude".into(),
                                "--resume".into(),
                                format!("claude-3ir-session-{seq}"),
                            ]
                        }),
                        agent_pid,
                        agent_session_cwd: None,
                    },
                ),
            });
            assert!(!response.contains("\"error\""), "{response}");
        }

        report(&mut app, &public_pane_id, 1, None, false);
        report(&mut app, &public_pane_id, 2, Some(std::process::id()), true);
        {
            let terminal = &app.state.terminals[&terminal_id];
            assert!(terminal.session_ref_is_current(
                &crate::agent_resume::AgentSessionRef::id("claude-3ir-session").unwrap()
            ));
            assert!(
                terminal.reported_resume().is_none(),
                "a report refused by the foreground gate must not record its resume_argv, got {:?}",
                terminal.reported_resume()
            );
            assert!(
                terminal.hook_report_is_newer("herdr:claude", Some(2)),
                "a refused report must not advance the sequence watermark"
            );
        }

        report(&mut app, &public_pane_id, 3, None, true);
        assert_eq!(
            app.state.terminals[&terminal_id]
                .reported_resume()
                .map(|resume| resume.argv.clone()),
            Some(vec![
                "claude".to_string(),
                "--resume".to_string(),
                "claude-3ir-session-3".to_string()
            ]),
            "control: the same report without agent_pid fails open and records the command"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    // herdr-nrr (codex nrr-r1): a reported resume command runs only in the
    // directory its dedupe key reserved at restore time. These tests route
    // the typed command through an `agent_launchers` recorder script that
    // appends "<physical cwd>|<args>" to a log, so they observe exactly which
    // command ran in which directory - no real agent binary needed.
    #[cfg(unix)]
    struct NrrLaunchRecorder {
        root: std::path::PathBuf,
        log: std::path::PathBuf,
        launcher: String,
    }

    #[cfg(unix)]
    impl NrrLaunchRecorder {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "herdr-nrr-launch-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&root).unwrap();
            let root = root.canonicalize().unwrap();
            let log = root.join("launches.log");
            let script = root.join("record.sh");
            std::fs::write(
                &script,
                format!(
                    "printf '%s|%s\\n' \"$(pwd -P)\" \"$*\" >> '{}'\n",
                    log.display()
                ),
            )
            .unwrap();
            let launcher = format!("/bin/sh {}", script.display());
            Self {
                root,
                log,
                launcher,
            }
        }

        fn dir(&self, name: &str) -> std::path::PathBuf {
            let dir = self.root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        async fn wait_for_launches(&self, count: usize) -> Vec<String> {
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let lines: Vec<String> = std::fs::read_to_string(&self.log)
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect();
                if lines.len() >= count || Instant::now() >= deadline {
                    return lines;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }

    #[cfg(unix)]
    impl Drop for NrrLaunchRecorder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(unix)]
    fn nrr_reported_continue_plan(
        reserved_dir: &std::path::Path,
    ) -> crate::agent_resume::AgentResumePlan {
        crate::agent_resume::ReportedAgentResume {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            argv: vec!["claude".into(), "--continue".into()],
        }
        .plan(reserved_dir)
    }

    #[cfg(unix)]
    fn nrr_claude_session(
        id: &str,
        agent_cwd: &std::path::Path,
    ) -> crate::agent_resume::PersistedAgentSession {
        crate::agent_resume::PersistedAgentSession {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id(id).unwrap(),
            cwd: Some(agent_cwd.display().to_string()),
        }
    }

    // codex nrr-r1 P1: session A is persisted with the reported command
    // `claude --continue`, reserved for A's agent dir X. X disappears before
    // the deferred launch, so the launch dir falls back to the pane dir P,
    // whose latest conversation is B - held by a live process. 4r8 checks A
    // (not held), so typing `--continue` in P would attach a second writer to
    // B. The launch must resume A by id instead, and never type `--continue`
    // outside X.
    #[cfg(unix)]
    #[tokio::test]
    async fn agent_restore_nrr_reported_continue_does_not_resume_a_live_session_after_agent_cwd_vanished(
    ) {
        let fixture = ClaudeRegistryFixture::new("nrr-p1");
        fixture.register_self_as_live_holder("nrr-session-b");
        let recorder = NrrLaunchRecorder::new("p1");
        let pane_dir = recorder.dir("pane-p");
        let agent_dir = recorder.dir("agent-x");

        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("nrr-p1");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).unwrap().clone();
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state
            .agent_launchers
            .insert("claude".into(), recorder.launcher.clone());
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.cwd = pane_dir.clone();
        terminal.persisted_agent_session = Some(nrr_claude_session("nrr-session-a", &agent_dir));
        terminal.pending_agent_resume_plan = Some(nrr_reported_continue_plan(&agent_dir));

        std::fs::remove_dir_all(&agent_dir).unwrap();
        assert_eq!(
            agent_resume_cwd(&app.state.terminals[&terminal_id]),
            pane_dir
        );

        app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true);
        assert!(
            app.terminal_runtimes.get(&terminal_id).is_some(),
            "session A is held by nobody, so its resume must still start"
        );
        let launches = recorder.wait_for_launches(1).await;
        assert_eq!(
            launches,
            vec![format!("{}|--resume nrr-session-a", pane_dir.display())],
            "the pane must resume the saved session A by id, never `--continue` onto P's live conversation B"
        );
        assert!(app.state.terminals[&terminal_id].restore_error.is_none());

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    // codex nrr-r1 P2: two panes share pane dir P and report `claude
    // --continue` from distinct agent dirs X and Y, so restore reserves two
    // different dedupe keys and keeps both plans. X and Y disappear before
    // the deferred launch; both would fall back to P and resume P's same
    // latest conversation twice. At most one `--continue` may run in P.
    #[cfg(unix)]
    #[tokio::test]
    async fn agent_restore_nrr_reported_continue_reservations_hold_when_agent_cwds_vanish_before_launch(
    ) {
        let _fixture = ClaudeRegistryFixture::new("nrr-p2");
        let recorder = NrrLaunchRecorder::new("p2");
        let pane_dir = recorder.dir("pane-p");
        let agent_dirs = [recorder.dir("agent-x"), recorder.dir("agent-y")];

        let mut app = test_app();
        app.state.workspaces = (0..2)
            .map(|_| crate::workspace::Workspace::test_new("nrr-p2"))
            .collect();
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state
            .agent_launchers
            .insert("claude".into(), recorder.launcher.clone());
        let terminal_ids: Vec<_> = app
            .state
            .workspaces
            .iter()
            .map(|ws| ws.terminal_id(ws.tabs[0].root_pane).unwrap().clone())
            .collect();
        let mut dedupe_keys = std::collections::HashSet::new();
        for ((terminal_id, agent_dir), session_id) in terminal_ids
            .iter()
            .zip(&agent_dirs)
            .zip(["nrr-session-x", "nrr-session-y"])
        {
            let plan = nrr_reported_continue_plan(agent_dir);
            assert!(
                dedupe_keys.insert(plan.dedupe_key.clone()),
                "restore reserves a distinct key per agent dir, so both plans survive de-duplication"
            );
            let terminal = app.state.terminals.get_mut(terminal_id).unwrap();
            terminal.cwd = pane_dir.clone();
            terminal.persisted_agent_session = Some(nrr_claude_session(session_id, agent_dir));
            terminal.pending_agent_resume_plan = Some(plan);
        }

        for agent_dir in &agent_dirs {
            std::fs::remove_dir_all(agent_dir).unwrap();
        }
        for terminal_id in &terminal_ids {
            app.start_pending_agent_resume_for_terminal(terminal_id, 24, 80, true);
            assert!(app.terminal_runtimes.get(terminal_id).is_some());
        }

        let mut launches = recorder.wait_for_launches(2).await;
        launches.sort();
        let continues_in_pane_dir = launches
            .iter()
            .filter(|line| *line == &format!("{}|--continue", pane_dir.display()))
            .count();
        assert!(
            continues_in_pane_dir <= 1,
            "two panes must not both resume P's latest conversation, launches: {launches:?}"
        );
        assert_eq!(
            launches,
            vec![
                format!("{}|--resume nrr-session-x", pane_dir.display()),
                format!("{}|--resume nrr-session-y", pane_dir.display()),
            ],
            "each pane resumes its own saved session by id"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_deferred_restore_keeps_session_reference_without_retrying_elsewhere() {
        for missing_shell in [false, true] {
            let mut app = test_app();
            let workspace = crate::workspace::Workspace::test_new("unavailable");
            let pane_id = workspace.tabs[0].root_pane;
            let terminal_id = workspace.terminal_id(pane_id).unwrap().clone();
            app.state.workspaces = vec![workspace];
            app.state.active = Some(0);
            app.state.ensure_test_terminals();
            if missing_shell {
                app.state.default_shell = "__herdr_missing_resume_shell__".into();
            }
            let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
            if !missing_shell {
                terminal.cwd = std::env::current_dir()
                    .unwrap()
                    .join("__herdr_missing_resume_cwd__");
                assert!(!terminal.cwd.exists());
            }
            let session = crate::agent_resume::PersistedAgentSession {
                source: "herdr:codex".into(),
                agent: "codex".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id("resume-test").unwrap(),
                cwd: None,
            };
            terminal.persisted_agent_session = Some(session.clone());
            terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
                agent: "codex".into(),
                argv: long_running_test_argv(),
                dedupe_key: "resume-test".into(),
                launch_dir: None,
            });
            app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true);
            assert!(app.terminal_runtimes.get(&terminal_id).is_none());
            let terminal = &app.state.terminals[&terminal_id];
            assert!(terminal.pending_agent_resume_plan.is_none());
            assert_eq!(terminal.persisted_agent_session.as_ref(), Some(&session));
            assert!(terminal.restore_error.is_some());
            assert!(!app.has_pending_agent_resumes());
            assert!(!app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_agent_resume_waits_for_host_theme_before_launch() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        let pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.view.pane_infos = pane_infos;
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist");
        terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: marker_resume_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
            launch_dir: None,
        });

        assert!(!app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&terminal_id).is_none());

        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());
        let terminal = app
            .state
            .terminals
            .get(&terminal_id)
            .expect("terminal should survive launch");
        assert!(terminal.pending_agent_resume_plan.is_none());
        assert!(!terminal.respawn_shell_on_exit);

        let runtime = app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("pending resume should leave a shell runtime");
        let marker = "restored agent: shell quoted | marker";
        for _ in 0..20 {
            if runtime
                .snapshot_history()
                .is_some_and(|text| text.contains(marker))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            runtime
                .snapshot_history()
                .expect("runtime should expose terminal history")
                .contains(marker),
            "deferred restore should inject the resume argv into the restored shell"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    // Runs a real, short-lived /bin/sh -c command that prints a*b (computed by
    // the spawned shell's own arithmetic expansion, not this test) wrapped in
    // a distinctive marker, then sleeps. Terminal local echo reflects the
    // RAW TYPED bytes regardless of whether the command that follows ever
    // runs, so a literal marker string embedded in argv would reach the pane
    // history just from being typed - a plan whose argv[0] cannot execute
    // would still make an earlier version of this test pass. Checking for
    // the PRODUCT instead of the typed factors means the check string never
    // appears in the bytes sent to the shell; it can only appear in the pane
    // history if a shell actually evaluated "$(( a * b ))", which only
    // happens when the command that was typed actually ran.
    #[cfg(unix)]
    fn launcher_target_check_argv(a: u64, b: u64, tag: &str) -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("printf 'HERDR-CHECK-{tag}-%s-END' \"$(( {a} * {b} ))\"; sleep 5"),
        ]
    }

    #[cfg(unix)]
    fn launcher_target_check_marker(a: u64, b: u64, tag: &str) -> String {
        format!("HERDR-CHECK-{tag}-{}-END", a * b)
    }

    #[cfg(unix)]
    async fn assert_marker_reaches_pane(
        runtime: &crate::terminal::TerminalRuntime,
        marker: &str,
        failure_context: &str,
    ) {
        for _ in 0..20 {
            if runtime
                .snapshot_history()
                .is_some_and(|text| text.contains(marker))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            runtime
                .snapshot_history()
                .expect("runtime should expose terminal history")
                .contains(marker),
            "{failure_context}"
        );
    }

    // App-level seam: start_pending_agent_resume() has no method that returns
    // the composed command on its own, so this drives the same public entry
    // point pending_agent_resume_waits_for_host_theme_before_launch uses and
    // reads back what actually ran in the restored pane's shell, via its pty
    // history. argv[0] in the plan is a name that cannot run; only the
    // configured launcher PREFIX resolves to something runnable ("/bin/sh",
    // typed verbatim ahead of argv[1..] exactly as the operator would type
    // it), so the marker reaching the pane proves the launcher override was
    // applied to the command typed into the shell, not merely computed and
    // discarded.
    #[cfg(unix)]
    #[tokio::test]
    async fn agent_launcher_overrides_the_typed_resume_command() {
        let mut app = test_app();
        app.state
            .agent_launchers
            .insert("claude".into(), "/bin/sh".into());
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        let pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.view.pane_infos = pane_infos;

        let (a, b, tag) = (733u64, 617u64, "override");
        let mut launcher_argv = launcher_target_check_argv(a, b, tag);
        launcher_argv[0] = "herdr-test-unresolved-agent-binary".into();
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist");
        terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "claude".into(),
            argv: launcher_argv,
            dedupe_key: "herdr:claude\0claude\0Id\0launcher-test-session".into(),
            launch_dir: None,
        });
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        let runtime = app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("pending resume should launch");
        assert_marker_reaches_pane(
            runtime,
            &launcher_target_check_marker(a, b, tag),
            "a configured agent launcher should replace argv[0] with a runnable command",
        )
        .await;

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    // Mirror of agent_launcher_overrides_the_typed_resume_command: with no
    // agent_launchers entry for "claude", the plan's own argv[0] ("/bin/sh")
    // must run unmodified.
    #[cfg(unix)]
    #[tokio::test]
    async fn agent_launcher_absent_runs_the_stock_resume_argv() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        let pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.view.pane_infos = pane_infos;

        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist");
        let (a, b, tag) = (811u64, 601u64, "stock");
        terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "claude".into(),
            argv: launcher_target_check_argv(a, b, tag),
            dedupe_key: "herdr:claude\0claude\0Id\0stock-test-session".into(),
            launch_dir: None,
        });
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        let runtime = app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("pending resume should launch");
        assert_marker_reaches_pane(
            runtime,
            &launcher_target_check_marker(a, b, tag),
            "an unconfigured agent should run its stock resume argv unchanged",
        )
        .await;

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_agent_resume_can_launch_after_theme_wait_expires() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
            launch_dir: None,
        });

        app.sync_pending_agent_resume_deadline(std::time::Instant::now());
        assert!(!app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.start_pending_agent_resumes(Instant::now(), true));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_launches_hidden_panes_with_current_terminal_area() {
        let mut app = test_app();
        let active_workspace = crate::workspace::Workspace::test_new("active");
        let active_pane = active_workspace.tabs[0].root_pane;
        let active_terminal = active_workspace.terminal_id(active_pane).cloned().unwrap();
        let hidden_workspace = crate::workspace::Workspace::test_new("hidden");
        let hidden_pane = hidden_workspace.tabs[0].root_pane;
        let hidden_terminal = hidden_workspace.terminal_id(hidden_pane).cloned().unwrap();
        app.state.view.pane_infos = active_workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![active_workspace, hidden_workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        for terminal_id in [&active_terminal, &hidden_terminal] {
            app.state
                .terminals
                .get_mut(terminal_id)
                .expect("test terminal should exist")
                .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
                agent: "codex".into(),
                argv: long_running_test_argv(),
                dedupe_key: format!("herdr:codex\0codex\0Id\0{terminal_id}"),
                launch_dir: None,
            });
        }
        app.pending_agent_resume_deadline =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1));

        let now = Instant::now();
        assert!(app.start_pending_agent_resumes(now, false));
        assert!(app.terminal_runtimes.get(&active_terminal).is_some());
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_none());
        assert!(!app.start_pending_agent_resumes(now, true));
        assert!(
            app.start_pending_agent_resumes(now + std::time::Duration::from_millis(100), false,)
        );
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_some());
        assert!(
            app.pending_agent_resume_deadline.is_none(),
            "launched pending resumes should clear the wakeup deadline"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_launches_inactive_tab_panes_with_current_terminal_area() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("tabs");
        let active_pane = workspace.tabs[0].root_pane;
        let inactive_tab = workspace.test_add_tab(Some("agents"));
        let inactive_pane = workspace.tabs[inactive_tab].root_pane;
        let inactive_terminal = workspace.tabs[inactive_tab]
            .terminal_id(inactive_pane)
            .cloned()
            .unwrap();
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        assert!(app
            .state
            .workspaces
            .first()
            .and_then(|ws| ws.tabs[0].terminal_id(active_pane))
            .is_some());
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&inactive_terminal)
            .expect("inactive tab terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0inactive-tab-session".into(),
            launch_dir: None,
        });

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&inactive_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&inactive_terminal)
                .expect("inactive tab terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "inactive tab restored panes should not wait for tab focus"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_launches_zoom_hidden_active_tab_panes() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("zoomed");
        let hidden_pane = workspace.tabs[0].root_pane;
        let visible_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        workspace.tabs[0].zoomed = true;
        let hidden_terminal = workspace.terminal_id(hidden_pane).cloned().unwrap();
        app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: visible_pane,
            rect: ratatui::layout::Rect::new(0, 0, 100, 30),
            inner_rect: ratatui::layout::Rect::new(1, 1, 98, 28),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::ALL,
            is_focused: true,
        }];
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&hidden_terminal)
            .expect("hidden zoom pane terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0zoom-hidden-session".into(),
            launch_dir: None,
        });

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&hidden_terminal)
                .expect("hidden zoom pane terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "zoom-hidden restored panes should not wait for pane focus"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_uses_current_terminal_area_for_background_panes() {
        let mut app = test_app();
        let previous_workspace = crate::workspace::Workspace::test_new("previous");
        let previous_pane = previous_workspace.tabs[0].root_pane;
        let previous_terminal = previous_workspace
            .terminal_id(previous_pane)
            .cloned()
            .unwrap();
        let current_workspace = crate::workspace::Workspace::test_new("current");
        app.state.view.pane_infos = previous_workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 80, 24);
        app.state.workspaces = vec![previous_workspace, current_workspace];
        app.state.active = Some(1);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&previous_terminal)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
            launch_dir: None,
        });

        app.sync_pending_agent_resume_deadline(std::time::Instant::now());
        assert!(app.pending_agent_resume_deadline.is_some());
        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&previous_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&previous_terminal)
                .expect("previous terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "background restored panes should not wait for focus once terminal area is known"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_agent_resume_launches_with_inner_rect_size() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("split");
        let pane_id = workspace.test_split(ratatui::layout::Direction::Horizontal);
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, 100, 30),
            inner_rect: ratatui::layout::Rect::new(1, 1, 98, 28),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::ALL,
            is_focused: true,
        }];
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
            launch_dir: None,
        });

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert_eq!(
            app.terminal_runtimes
                .get(&terminal_id)
                .expect("pending resume should launch")
                .current_size(),
            (28, 98)
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    // Restored byte-for-byte from upstream ddb19e68: the stock resume
    // command has no launcher involved at all, so its quoting is exactly
    // this and nothing else.
    #[test]
    fn shell_command_from_argv_quotes_resume_arguments() {
        let argv = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "session with ' quote".to_string(),
        ];

        assert_eq!(
            shell_command_from_argv(&argv).as_deref(),
            Some("claude --resume 'session with '\\'' quote'")
        );
        assert_eq!(shell_command_from_argv(&[]), None);
    }

    fn resume_command_test_argv() -> Vec<String> {
        vec!["claude".into(), "--resume".into(), "sid".into()]
    }

    #[test]
    fn agent_launcher_none_uses_stock_command() {
        assert_eq!(
            resume_command(&resume_command_test_argv(), None),
            shell_command_from_argv(&resume_command_test_argv())
        );
    }

    #[test]
    fn agent_launcher_empty_uses_stock_command() {
        let launcher = String::new();
        assert_eq!(
            resume_command(&resume_command_test_argv(), Some(&launcher)),
            shell_command_from_argv(&resume_command_test_argv())
        );
    }

    #[test]
    fn agent_launcher_whitespace_only_uses_stock_command() {
        let launcher = "   ".to_string();
        assert_eq!(
            resume_command(&resume_command_test_argv(), Some(&launcher)),
            shell_command_from_argv(&resume_command_test_argv())
        );
    }

    #[test]
    fn agent_launcher_prefix_types_stock_arguments_after_it() {
        let launcher = "cas".to_string();
        assert_eq!(
            resume_command(&resume_command_test_argv(), Some(&launcher)).as_deref(),
            Some("cas --resume sid")
        );
    }

    #[test]
    fn agent_launcher_prefix_is_never_quoted() {
        // The operator's own example (ADR-0001): a PowerShell call-operator
        // prefix naming a path with a space. If this were quoted or escaped
        // like an argument, PowerShell would read it as a string literal
        // instead of running it.
        let launcher = "& 'C:\\Agent Tools\\cas.ps1'".to_string();
        assert_eq!(
            resume_command(&resume_command_test_argv(), Some(&launcher)).as_deref(),
            Some("& 'C:\\Agent Tools\\cas.ps1' --resume sid")
        );
    }

    #[test]
    fn agent_launcher_prefix_is_typed_verbatim_with_its_spaces() {
        // ADR-0001: the prefix is typed exactly as configured. Surrounding
        // spaces are harmless to a shell, and trimming would also strip
        // characters that are part of a name (a trailing no-break space).
        let launcher = "  cas\u{a0} ".to_string();
        assert_eq!(
            resume_command(&resume_command_test_argv(), Some(&launcher)).as_deref(),
            Some("  cas\u{a0}  --resume sid")
        );
    }

    #[test]
    fn agent_launcher_control_character_falls_back_to_stock_command() {
        // Inside the prefix and at either end: a control character at the
        // boundary must not be trimmed away and accepted (codex r4). U+0085
        // (NEL) is a C1 control that str::trim would also have removed.
        for control in ['\n', '\r', '\t', '\u{85}'] {
            for launcher in [
                format!("cas{control}--resume-elsewhere"),
                format!("cas{control}"),
                format!("{control}cas"),
            ] {
                assert!(launcher_has_control_character(Some(&launcher)));
                assert_eq!(
                    resume_command(&resume_command_test_argv(), Some(&launcher)),
                    shell_command_from_argv(&resume_command_test_argv()),
                    "a launcher {launcher:?} must fall back to the stock command"
                );
            }
        }
    }

    #[test]
    fn agent_launcher_stock_arguments_keep_upstream_quoting() {
        let argv = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "session with ' quote".to_string(),
        ];
        let launcher = "cas".to_string();
        assert_eq!(
            resume_command(&argv, Some(&launcher)).as_deref(),
            Some("cas --resume 'session with '\\'' quote'")
        );
    }

    #[test]
    fn agent_launcher_prefix_with_no_extra_arguments() {
        let argv = vec!["claude".to_string()];
        let launcher = "cas".to_string();
        assert_eq!(
            resume_command(&argv, Some(&launcher)).as_deref(),
            Some("cas")
        );
    }

    // Pure seam test (no pane, no shell process): resume_command() must
    // apply the configured launcher prefix, not serialize the plan's stock
    // argv directly. The assert_ne! is the one that actually distinguishes
    // the two - it reddens under the same "agent launcher override
    // disabled" mutant the CI mutant job applies to resume_command(), which
    // is exactly the regression this seam guards.
    #[test]
    fn agent_launcher_resume_command_matches_launcher_composition() {
        let argv = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "session with ' quote".to_string(),
        ];
        let launcher = "cas".to_string();

        assert_eq!(
            resume_command(&argv, Some(&launcher)).as_deref(),
            Some("cas --resume 'session with '\\'' quote'")
        );
        assert_ne!(
            resume_command(&argv, Some(&launcher)),
            resume_command(&argv, None),
            "a configured launcher must change the composed resume command"
        );
        assert_eq!(resume_command(&[], None), None);
    }

    // Windows execution tests use plain-word resume arguments throughout
    // (the pure tests above already pin the exotic-quoting cases): ADR-0001
    // holds because a stock resume argv is always plain words, and mixing
    // that concern into an integration test that also spawns a real shell
    // would test PowerShell/cmd quoting of POSIX-escaped text, which is not
    // what this fix does or needs to do.

    // r3's whole defect: routing the composed command through
    // crate::platform::interactive_shell_command wrapped a non-script
    // command in Start-Process, which resolves neither a PowerShell function
    // nor an alias defined in the pane's own profile. resume_command types
    // the launcher prefix verbatim, so a prefix that is itself the name of a
    // function the pane shell already knows about resolves exactly as it
    // would if the operator typed it by hand.
    #[cfg(windows)]
    #[test]
    fn agent_launcher_prefix_resolves_as_a_powershell_function() {
        let argv = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "session-abc".to_string(),
        ];
        let launcher = "cas".to_string();
        let composed =
            resume_command(&argv, Some(&launcher)).expect("non-empty argv always composes");
        assert_eq!(composed, "cas --resume session-abc");

        let script = format!(
            "function cas {{ Write-Output \"CAS-CALLED:$($args -join ' ')\" }}; {composed}"
        );
        let output = Command::new("powershell.exe")
            .args(["-NoLogo", "-NoProfile", "-Command", &script])
            .output()
            .expect("powershell.exe should run");
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("CAS-CALLED:--resume session-abc"),
            "the composed command should invoke the pane-shell function named by the launcher, got: {stdout}"
        );
    }

    // The operator's own example (ADR-0001): a launcher prefix naming a path
    // with a space, typed verbatim into each pane shell's own syntax - the
    // call operator for PowerShell, a quoted path for cmd.exe. resume_command
    // never touches this text, so each shell parses it with its own rules,
    // exactly as if the operator had typed it themselves.
    #[cfg(windows)]
    #[test]
    fn agent_launcher_prefix_with_a_space_runs_via_the_pane_shells_own_syntax() {
        let base = std::env::temp_dir().join(format!(
            "herdr agent launcher {}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let helper = base.join("cas launcher.cmd");
        std::fs::write(
            &helper,
            "@echo off\r\n>\"%HERDR_ARGV_CAPTURE%\" (\r\necho(%~1\r\necho(%~2\r\n)\r\n",
        )
        .unwrap();
        let helper_path = helper.to_str().unwrap().to_string();

        let argv = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "session-xyz".to_string(),
        ];

        let cases = [
            ("powershell.exe", format!("& '{helper_path}'")),
            ("cmd.exe", format!("\"{helper_path}\"")),
        ];

        for (shell, launcher) in cases {
            let composed =
                resume_command(&argv, Some(&launcher)).expect("non-empty argv always composes");
            let capture = base.join(format!("{shell}.txt"));
            let status = if shell == "cmd.exe" {
                // cmd.exe's own /C quote-preservation rule only fires for
                // EXACTLY two quote characters in the command tail (see
                // `cmd /?`); `composed` already carries exactly the two that
                // wrap the launcher path, so it must reach cmd.exe verbatim.
                // Command::args() would add a THIRD layer of quoting around
                // the whole string (it contains a space), pushing the count
                // past two and sending cmd.exe into its "old behaviour"
                // quote-stripping instead - raw_arg() is the escape hatch.
                Command::new("cmd.exe")
                    .arg("/d")
                    .arg("/c")
                    .raw_arg(&composed)
                    .env("HERDR_ARGV_CAPTURE", &capture)
                    .status()
            } else {
                Command::new("powershell.exe")
                    .args(["-NoLogo", "-NoProfile", "-Command", &composed])
                    .env("HERDR_ARGV_CAPTURE", &capture)
                    .env("PSExecutionPolicyPreference", "Bypass")
                    .status()
            }
            .expect("shell should run");
            assert!(
                status.success(),
                "{shell} failed to run the composed resume command"
            );
            assert_eq!(
                std::fs::read_to_string(&capture)
                    .unwrap()
                    .replace("\r\n", "\n"),
                "--resume\nsession-xyz\n",
                "{shell} did not hand the launcher its exact resume arguments"
            );
        }

        let _ = std::fs::remove_dir_all(base);
    }
}
