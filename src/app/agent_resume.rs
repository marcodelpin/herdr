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
            return;
        }
        if self.pending_agent_resume_candidates().is_empty() {
            self.pending_agent_resume_deadline = None;
            return;
        }
        self.pending_agent_resume_deadline
            .get_or_insert(now + super::PENDING_AGENT_RESUME_THEME_WAIT);
    }

    pub(crate) fn pending_agent_resume_due(&self, now: Instant) -> bool {
        self.pending_agent_resume_deadline
            .is_some_and(|deadline| now >= deadline)
    }

    pub(crate) fn start_pending_agent_resumes(&mut self, allow_empty_theme: bool) -> bool {
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
        }

        if changed {
            self.schedule_session_save();
        }
        if !self.has_pending_agent_resumes() || self.pending_agent_resume_candidates().is_empty() {
            self.pending_agent_resume_deadline = None;
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
                        cwd: terminal.cwd.clone(),
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
                        terminal.cwd.clone(),
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
            };
            terminal.persisted_agent_session = Some(session.clone());
            terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
                agent: "codex".into(),
                argv: long_running_test_argv(),
                dedupe_key: "resume-test".into(),
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
        });

        assert!(!app.start_pending_agent_resumes(false));
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

        assert!(app.start_pending_agent_resumes(false));
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

        assert!(app.start_pending_agent_resumes(false));
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

        assert!(app.start_pending_agent_resumes(false));
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
        });

        app.sync_pending_agent_resume_deadline(std::time::Instant::now());
        assert!(!app.start_pending_agent_resumes(false));
        assert!(app.start_pending_agent_resumes(true));
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
            });
        }
        app.pending_agent_resume_deadline =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1));

        assert!(app.start_pending_agent_resumes(false));
        assert!(app.terminal_runtimes.get(&active_terminal).is_some());
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
        });

        assert!(app.start_pending_agent_resumes(false));
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
        });

        assert!(app.start_pending_agent_resumes(false));
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
        });

        app.sync_pending_agent_resume_deadline(std::time::Instant::now());
        assert!(app.pending_agent_resume_deadline.is_some());
        assert!(app.start_pending_agent_resumes(false));
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
        });

        assert!(app.start_pending_agent_resumes(false));
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
