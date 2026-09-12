use super::*;
use crate::config::StatusIndicatorStyle;

const DESKTOP: (u16, u16) = (106, 30);
const MOBILE: (u16, u16) = (44, 30);
/// U+29D7 black hourglass, the static Waiting glyph and the only Stale glyph.
const WAITING: &str = "\u{29d7}";
/// U+29D6 white hourglass, the turned-over Waiting frame.
const WAITING_TURNED: &str = "\u{29d6}";

fn config_with(style: StatusIndicatorStyle) -> Config {
    let mut config = Config::default();
    config.ui.status_indicators = style;
    config
}

fn waiting_agent(
    pane_id: &str,
    workspace_id: &str,
    status: AgentStatus,
    tokens: &[(&str, &str)],
) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: workspace_id.into(),
        tab_id: "tab_1".into(),
        name: Some("claude".into()),
        display_agent: None,
        agent: Some("claude".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: status,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: tokens
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        focused: true,
    }
}

/// The default snapshot with one pane in `status` carrying `tokens`, so the space row
/// and the agent row both speak for that one pane.
fn waiting_state(
    status: AgentStatus,
    tokens: &[(&str, &str)],
    config: &Config,
) -> ClientShellState {
    state_with_agents(
        status,
        vec![waiting_agent("pane_1", "ws_1", status, tokens)],
        config,
    )
}

fn state_with_agents(
    status: AgentStatus,
    agents: Vec<ClientShellAgent>,
    config: &Config,
) -> ClientShellState {
    let mut projected = snapshot();
    projected.workspaces[0].agent_status = status;
    projected.tabs[0].agent_status = status;
    projected.agents = agents;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(config));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state
}

fn frame_text(frame: &FrameData) -> String {
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn cell_at(frame: &FrameData, x: u16, y: u16) -> &crate::protocol::CellData {
    &frame.cells[usize::from(y) * usize::from(frame.width) + usize::from(x)]
}

/// The state icon of the first space row: row 0 of a top-level workspace starts one
/// column into its rect, and `state_icon` is the first token of the default layout.
fn workspace_icon<'a>(
    state: &ClientShellState,
    frame: &'a FrameData,
) -> &'a crate::protocol::CellData {
    let rect = state
        .hits
        .workspaces
        .first()
        .expect("a rendered workspace row")
        .rect;
    cell_at(frame, rect.x.saturating_add(1), rect.y)
}

/// The state icon of the first agent row, which render_agent_row indents by one.
fn agent_icon<'a>(state: &ClientShellState, frame: &'a FrameData) -> &'a crate::protocol::CellData {
    let rect = state.hits.agents.first().expect("a rendered agent row").0;
    cell_at(frame, rect.x.saturating_add(1), rect.y)
}

fn assert_icon(
    cell: &crate::protocol::CellData,
    glyph: &str,
    color: ratatui::style::Color,
    bold: bool,
) {
    assert_eq!(cell.symbol, glyph);
    assert_eq!(cell.fg, crate::protocol::color_to_u32(color));
    assert_eq!(
        (cell.modifier & Modifier::BOLD.bits()) != 0,
        bold,
        "modifier {:#x} bold={bold}",
        cell.modifier
    );
}

fn contains_any(text: &str, glyphs: &[&str]) -> bool {
    glyphs.iter().any(|glyph| text.contains(glyph))
}

/// Composes after moving the spinner epoch back `periods` more Waiting periods, which
/// is the same as composing that much later without sleeping.
fn compose_waiting_periods_later(state: &mut ClientShellState, periods: u32) -> FrameData {
    if periods > 0 {
        state.spinner_epoch = state
            .spinner_epoch
            .checked_sub(WAITING_SPINNER_FRAME_PERIOD * periods)
            .expect("spinner epoch can move back");
    }
    state
        .compose(DESKTOP.0, DESKTOP.1)
        .expect("composed desktop frame")
}

/// The first instant at which the Waiting glyph turns over from the frame the last
/// composition drew.
fn next_waiting_frame_due(state: &ClientShellState) -> std::time::Instant {
    state.spinner_epoch + WAITING_SPINNER_FRAME_PERIOD * (state.config.waiting_frame as u32 + 1)
}

#[test]
fn display_state_follows_the_tokens_only_for_idle_and_done() {
    let wait = [("wait", "\u{29d7} mon 1 8m")];
    let stale = [("stale", "STALE: monitor expired")];
    let both = [
        ("wait", "\u{29d7} sh 2"),
        ("stale", "STALE 45m: shell ended, no wake"),
    ];

    for status in [AgentStatus::Idle, AgentStatus::Done] {
        assert_eq!(
            display_state(status, &pairs(&wait), true),
            DisplayState::Waiting
        );
        assert_eq!(
            display_state(status, &pairs(&stale), true),
            DisplayState::Stale
        );
        assert_eq!(
            display_state(status, &pairs(&both), true),
            DisplayState::Stale
        );
        assert_eq!(
            display_state(status, &pairs(&[("wait", "")]), true),
            DisplayState::Status(status)
        );
        assert_eq!(
            display_state(status, &pairs(&[("stale", "")]), true),
            DisplayState::Status(status)
        );
        assert_eq!(
            display_state(status, &[], true),
            DisplayState::Status(status)
        );
        assert_eq!(
            display_state(status, &pairs(&wait), false),
            DisplayState::Status(status)
        );
    }

    for status in [
        AgentStatus::Working,
        AgentStatus::Blocked,
        AgentStatus::Unknown,
    ] {
        assert_eq!(
            display_state(status, &pairs(&wait), true),
            DisplayState::Status(status)
        );
        assert_eq!(
            display_state(status, &pairs(&stale), true),
            DisplayState::Status(status)
        );
    }
}

fn pairs(tokens: &[(&str, &str)]) -> Vec<(String, String)> {
    tokens
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn an_idle_pane_with_a_wait_token_shows_a_blue_hourglass() {
    for style in [StatusIndicatorStyle::Dots, StatusIndicatorStyle::Symbols] {
        let config = config_with(style);
        let mut state = waiting_state(AgentStatus::Idle, &[("wait", "\u{29d7} mon 1 8m")], &config);
        let frame = state.compose(DESKTOP.0, DESKTOP.1).expect("idle frame");
        let blue = state.config.palette.blue;
        assert_icon(agent_icon(&state, &frame), WAITING, blue, false);
        assert_icon(workspace_icon(&state, &frame), WAITING, blue, false);
        assert!(
            !frame_text(&frame).contains(status_icon(AgentStatus::Idle, style, None)),
            "{style:?}: the idle circle is still on screen"
        );
    }
}

#[test]
fn a_done_pane_with_a_wait_token_shows_a_blue_hourglass() {
    for style in [StatusIndicatorStyle::Dots, StatusIndicatorStyle::Symbols] {
        let config = config_with(style);
        let mut state = waiting_state(
            AgentStatus::Done,
            &[("wait", "\u{29d7} sh 2 wk 1")],
            &config,
        );
        let frame = state.compose(DESKTOP.0, DESKTOP.1).expect("done frame");
        let blue = state.config.palette.blue;
        assert_icon(agent_icon(&state, &frame), WAITING, blue, false);
        assert_icon(workspace_icon(&state, &frame), WAITING, blue, false);
    }
}

#[test]
fn a_stale_token_shows_a_bold_red_hourglass_that_never_animates() {
    for style in [StatusIndicatorStyle::Dots, StatusIndicatorStyle::Symbols] {
        let config = config_with(style);
        let mut state = waiting_state(
            AgentStatus::Idle,
            &[("stale", "STALE 45m: shell ended, no wake")],
            &config,
        );
        let first = compose_waiting_periods_later(&mut state, 0);
        let red = state.config.palette.red;
        assert_icon(agent_icon(&state, &first), WAITING, red, true);
        assert_icon(workspace_icon(&state, &first), WAITING, red, true);
        assert!(
            !state.config.waiting_drawn.get(),
            "{style:?}: a stale row scheduled Waiting repaints"
        );
        assert!(!state.tick_working_spinner(next_waiting_frame_due(&state)));

        let later = compose_waiting_periods_later(&mut state, 1);
        assert_eq!(frame_text(&first), frame_text(&later), "{style:?}");
    }
}

#[test]
fn working_and_blocked_panes_keep_their_own_glyph_with_a_wait_token() {
    let config = config_with(StatusIndicatorStyle::Dots);
    let mut state = waiting_state(
        AgentStatus::Working,
        &[
            ("wait", "\u{29d7} mon 1"),
            ("stale", "STALE: monitor expired"),
        ],
        &config,
    );
    let frame = state.compose(DESKTOP.0, DESKTOP.1).expect("working frame");
    let text = frame_text(&frame);
    assert!(!text.contains(WAITING), "{text}");
    assert!(!text.contains(WAITING_TURNED), "{text}");
    assert!(contains_any(&text, &DOTS_WORKING_FRAMES), "{text}");
    assert_eq!(
        agent_icon(&state, &frame).fg,
        crate::protocol::color_to_u32(state.config.palette.yellow)
    );

    let mut state = waiting_state(AgentStatus::Blocked, &[("wait", "\u{29d7} sh 1")], &config);
    let frame = state.compose(DESKTOP.0, DESKTOP.1).expect("blocked frame");
    assert!(!frame_text(&frame).contains(WAITING));
    assert_icon(
        agent_icon(&state, &frame),
        status_icon(AgentStatus::Blocked, StatusIndicatorStyle::Dots, None),
        state.config.palette.red,
        false,
    );
}

#[test]
fn an_empty_wait_token_value_counts_as_cleared() {
    let config = config_with(StatusIndicatorStyle::Symbols);
    let mut state = waiting_state(AgentStatus::Idle, &[("stale", ""), ("wait", "")], &config);
    let frame = state.compose(DESKTOP.0, DESKTOP.1).expect("cleared frame");
    assert!(!frame_text(&frame).contains(WAITING));
    assert_icon(
        agent_icon(&state, &frame),
        status_icon(AgentStatus::Idle, StatusIndicatorStyle::Symbols, None),
        state.config.palette.green,
        false,
    );
}

#[test]
fn waiting_indicator_false_keeps_the_idle_icon() {
    let mut config = config_with(StatusIndicatorStyle::Symbols);
    config.ui.waiting_indicator = false;
    let mut state = waiting_state(
        AgentStatus::Idle,
        &[("wait", "\u{29d7} mon 1 8m"), ("stale", "STALE: pending")],
        &config,
    );
    let frame = state
        .compose(DESKTOP.0, DESKTOP.1)
        .expect("opted-out frame");
    assert!(!frame_text(&frame).contains(WAITING));
    assert_icon(
        agent_icon(&state, &frame),
        status_icon(AgentStatus::Idle, StatusIndicatorStyle::Symbols, None),
        state.config.palette.green,
        false,
    );
    assert!(!state.tick_working_spinner(next_waiting_frame_due(&state)));
}

#[test]
fn the_waiting_hourglass_turns_over_once_a_second() {
    for style in [StatusIndicatorStyle::Dots, StatusIndicatorStyle::Symbols] {
        let config = config_with(style);
        let mut state = waiting_state(AgentStatus::Idle, &[("wait", "\u{29d7} mon 1")], &config);
        for (periods, glyph) in [(0, WAITING), (1, WAITING_TURNED), (1, WAITING)] {
            let frame = compose_waiting_periods_later(&mut state, periods);
            assert_eq!(agent_icon(&state, &frame).symbol, glyph, "{style:?}");
            assert!(
                state.config.waiting_drawn.get(),
                "{style:?}: a drawn hourglass did not schedule its repaint"
            );
            assert!(
                state.tick_working_spinner(next_waiting_frame_due(&state)),
                "{style:?}"
            );
        }
        let same_frame =
            state.spinner_epoch + WAITING_SPINNER_FRAME_PERIOD * state.config.waiting_frame as u32;
        assert!(
            !state.tick_working_spinner(same_frame),
            "{style:?}: the timer repaints inside the same Waiting frame"
        );
    }

    let mut config = config_with(StatusIndicatorStyle::Dots);
    config.ui.animate_working = false;
    let mut state = waiting_state(AgentStatus::Idle, &[("wait", "\u{29d7} mon 1")], &config);
    let first = compose_waiting_periods_later(&mut state, 0);
    let later = compose_waiting_periods_later(&mut state, 1);
    assert_eq!(agent_icon(&state, &first).symbol, WAITING);
    assert_eq!(agent_icon(&state, &later).symbol, WAITING);
    assert!(!state.config.waiting_drawn.get());
    assert!(!state.tick_working_spinner(next_waiting_frame_due(&state)));
}

#[test]
fn workspace_rows_roll_the_waiting_state_up_from_their_agents() {
    let config = config_with(StatusIndicatorStyle::Symbols);
    let quiet = waiting_agent("pane_1", "ws_1", AgentStatus::Idle, &[]);

    let mut state = state_with_agents(
        AgentStatus::Idle,
        vec![
            quiet.clone(),
            waiting_agent(
                "pane_2",
                "ws_1",
                AgentStatus::Idle,
                &[("wait", "\u{29d7} mon 1")],
            ),
        ],
        &config,
    );
    let frame = state.compose(DESKTOP.0, DESKTOP.1).expect("waiting rollup");
    assert_icon(
        workspace_icon(&state, &frame),
        WAITING,
        state.config.palette.blue,
        false,
    );

    let mut state = state_with_agents(
        AgentStatus::Idle,
        vec![
            quiet.clone(),
            waiting_agent(
                "pane_2",
                "ws_1",
                AgentStatus::Idle,
                &[("wait", "\u{29d7} mon 1")],
            ),
            waiting_agent(
                "pane_3",
                "ws_1",
                AgentStatus::Idle,
                &[("stale", "STALE: monitor expired")],
            ),
        ],
        &config,
    );
    let frame = state.compose(DESKTOP.0, DESKTOP.1).expect("stale rollup");
    assert_icon(
        workspace_icon(&state, &frame),
        WAITING,
        state.config.palette.red,
        true,
    );

    let mut state = state_with_agents(
        AgentStatus::Working,
        vec![
            waiting_agent(
                "pane_2",
                "ws_1",
                AgentStatus::Idle,
                &[("wait", "\u{29d7} sh 1")],
            ),
            waiting_agent("pane_1", "ws_1", AgentStatus::Working, &[]),
        ],
        &config,
    );
    let frame = state.compose(DESKTOP.0, DESKTOP.1).expect("working rollup");
    assert_eq!(
        workspace_icon(&state, &frame).fg,
        crate::protocol::color_to_u32(state.config.palette.yellow)
    );
    assert_ne!(workspace_icon(&state, &frame).symbol, WAITING);
}

#[test]
fn the_mobile_header_and_switcher_show_the_waiting_hourglass() {
    let config = config_with(StatusIndicatorStyle::Dots);
    let mut state = waiting_state(AgentStatus::Idle, &[("wait", "\u{29d7} mon 1 8m")], &config);
    let header = state.compose(MOBILE.0, MOBILE.1).expect("mobile header");
    assert_icon(
        cell_at(&header, 1, 0),
        WAITING,
        state.config.palette.blue,
        false,
    );
    assert!(state.config.waiting_drawn.get());
    assert!(state.tick_working_spinner(next_waiting_frame_due(&state)));

    state.mode = ClientShellMode::Navigate;
    let switcher = state.compose(MOBILE.0, MOBILE.1).expect("mobile switcher");
    assert!(
        frame_text(&switcher).contains(WAITING),
        "{}",
        frame_text(&switcher)
    );
    assert!(
        state.config.waiting_drawn.get(),
        "a visible switcher item did not schedule its repaint"
    );
    assert!(state.tick_working_spinner(next_waiting_frame_due(&state)));
}

/// Local endpoint idle and agentless, plus a saved machine whose cached snapshot
/// holds one idle pane reporting `wait` in workspace `remote-work`.
fn remote_waiting_state(connected: bool) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let profile = SavedSshEndpoint {
        id: crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef")
            .expect("profile id"),
        label: "Build".into(),
        target: "build".into(),
        session: "agents".into(),
        enabled: true,
    };
    let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let mut remote = snapshot();
    remote.boot_id = "remote-boot".into();
    remote.workspaces[0].label = "remote-work".into();
    remote.workspaces[0].custom_label = true;
    remote.agents = vec![waiting_agent(
        "pane_1",
        "ws_1",
        AgentStatus::Idle,
        &[("wait", "\u{29d7} mon 1 8m")],
    )];
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote));
    if !connected {
        state.mark_endpoint_disconnected(&endpoint_id);
    }
    state
}

#[test]
fn an_unavailable_endpoints_waiting_row_keeps_the_static_hourglass() {
    let mut state = remote_waiting_state(false);
    let first = compose_waiting_periods_later(&mut state, 0);
    assert!(
        frame_text(&first).contains(WAITING),
        "{}",
        frame_text(&first)
    );
    assert!(!frame_text(&first).contains(WAITING_TURNED));
    assert!(
        !state.config.waiting_drawn.get(),
        "a disconnected machine's cached waiting row scheduled repaints"
    );
    assert!(!state.tick_working_spinner(next_waiting_frame_due(&state)));
    let later = compose_waiting_periods_later(&mut state, 1);
    assert_eq!(frame_text(&first), frame_text(&later));

    let mut state = remote_waiting_state(true);
    let live = compose_waiting_periods_later(&mut state, 0);
    assert!(frame_text(&live).contains(WAITING), "{}", frame_text(&live));
    assert!(state.config.waiting_drawn.get());
    assert!(state.tick_working_spinner(next_waiting_frame_due(&state)));
    let turned = compose_waiting_periods_later(&mut state, 1);
    assert!(
        frame_text(&turned).contains(WAITING_TURNED),
        "{}",
        frame_text(&turned)
    );
}
