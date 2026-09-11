use super::*;
use crate::config::StatusIndicatorStyle;

const DESKTOP: (u16, u16) = (106, 30);
const MOBILE: (u16, u16) = (44, 30);

fn spinner_state(
    status: AgentStatus,
    style: StatusIndicatorStyle,
    animate_working: bool,
) -> ClientShellState {
    let mut projected = snapshot();
    projected.workspaces[0].agent_status = status;
    projected.tabs[0].agent_status = status;
    projected.agents = vec![ClientShellAgent {
        pane_id: "pane_1".into(),
        workspace_id: "ws_1".into(),
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
        tokens: Vec::new(),
        focused: true,
    }];
    let mut config = Config::default();
    config.ui.status_indicators = style;
    config.ui.animate_working = animate_working;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
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

/// Composes twice, moving the spinner epoch back by exactly one frame period in
/// between, which is the same as composing one period later without sleeping.
fn compose_one_period_apart(state: &mut ClientShellState, size: (u16, u16)) -> (String, String) {
    let first = frame_text(&state.compose(size.0, size.1).expect("first frame"));
    state.spinner_epoch = state
        .spinner_epoch
        .checked_sub(WORKING_SPINNER_FRAME_PERIOD)
        .expect("spinner epoch can move back one period");
    let second = frame_text(&state.compose(size.0, size.1).expect("second frame"));
    (first, second)
}

fn contains_any(text: &str, glyphs: &[&str]) -> bool {
    glyphs.iter().any(|glyph| text.contains(glyph))
}

#[test]
fn status_icon_animates_only_working() {
    for style in [StatusIndicatorStyle::Dots, StatusIndicatorStyle::Symbols] {
        assert_ne!(
            status_icon(AgentStatus::Working, style, Some(0)),
            status_icon(AgentStatus::Working, style, Some(1)),
        );
        for status in [
            AgentStatus::Blocked,
            AgentStatus::Done,
            AgentStatus::Idle,
            AgentStatus::Unknown,
        ] {
            assert_eq!(
                status_icon(status, style, Some(0)),
                status_icon(status, style, Some(1)),
            );
            assert_eq!(
                status_icon(status, style, Some(3)),
                status_icon(status, style, None),
            );
        }
    }
}

#[test]
fn status_icon_frames_match_the_configured_sets_and_wrap() {
    for (frame, glyph) in DOTS_WORKING_FRAMES.iter().enumerate() {
        assert_eq!(
            status_icon(
                AgentStatus::Working,
                StatusIndicatorStyle::Dots,
                Some(frame)
            ),
            *glyph
        );
    }
    for (frame, glyph) in SYMBOLS_WORKING_FRAMES.iter().enumerate() {
        assert_eq!(
            status_icon(
                AgentStatus::Working,
                StatusIndicatorStyle::Symbols,
                Some(frame)
            ),
            *glyph
        );
    }
    assert_eq!(
        status_icon(
            AgentStatus::Working,
            StatusIndicatorStyle::Dots,
            Some(DOTS_WORKING_FRAMES.len())
        ),
        DOTS_WORKING_FRAMES[0]
    );
    assert_eq!(
        status_icon(
            AgentStatus::Working,
            StatusIndicatorStyle::Symbols,
            Some(SYMBOLS_WORKING_FRAMES.len() + 1)
        ),
        SYMBOLS_WORKING_FRAMES[1]
    );
}

#[test]
fn static_working_glyphs_are_unchanged_without_a_frame() {
    assert_eq!(
        status_icon(AgentStatus::Working, StatusIndicatorStyle::Dots, None),
        "\u{25cf}"
    );
    assert_eq!(
        status_icon(AgentStatus::Working, StatusIndicatorStyle::Symbols, None),
        "\u{25d0}"
    );
    assert_eq!(status_dot(AgentStatus::Working), "\u{25cf}");
}

#[test]
fn spinner_frame_advances_once_per_period() {
    assert_eq!(spinner_frame_at(std::time::Duration::ZERO), 0);
    assert_eq!(
        spinner_frame_at(WORKING_SPINNER_FRAME_PERIOD - std::time::Duration::from_millis(1)),
        0
    );
    assert_eq!(spinner_frame_at(WORKING_SPINNER_FRAME_PERIOD), 1);
    assert_eq!(spinner_frame_at(WORKING_SPINNER_FRAME_PERIOD * 7), 7);
}

#[test]
fn working_icon_differs_between_composes_at_different_times() {
    for (style, frames) in [
        (StatusIndicatorStyle::Dots, &DOTS_WORKING_FRAMES[..]),
        (StatusIndicatorStyle::Symbols, &SYMBOLS_WORKING_FRAMES[..]),
    ] {
        for size in [DESKTOP, MOBILE] {
            let mut state = spinner_state(AgentStatus::Working, style, true);
            let (first, second) = compose_one_period_apart(&mut state, size);
            assert!(contains_any(&first, frames), "{style:?} {size:?}: {first}");
            assert!(
                contains_any(&second, frames),
                "{style:?} {size:?}: {second}"
            );
            assert_ne!(first, second, "{style:?} {size:?}");
        }
    }
}

#[test]
fn collapsed_sidebar_animates_the_working_icon() {
    let mut state = spinner_state(AgentStatus::Working, StatusIndicatorStyle::Dots, true);
    state.sidebar_collapsed = true;
    let (first, second) = compose_one_period_apart(&mut state, DESKTOP);
    assert!(contains_any(&first, &DOTS_WORKING_FRAMES), "{first}");
    assert_ne!(first, second);
}

#[test]
fn idle_icon_is_identical_between_composes_at_different_times() {
    for style in [StatusIndicatorStyle::Dots, StatusIndicatorStyle::Symbols] {
        for size in [DESKTOP, MOBILE] {
            let mut state = spinner_state(AgentStatus::Idle, style, true);
            let (first, second) = compose_one_period_apart(&mut state, size);
            assert_eq!(first, second, "{style:?} {size:?}");
            assert!(!contains_any(&first, &DOTS_WORKING_FRAMES));
        }
    }
}

#[test]
fn animate_working_false_keeps_the_static_working_glyph() {
    let mut state = spinner_state(AgentStatus::Working, StatusIndicatorStyle::Dots, false);
    let (first, second) = compose_one_period_apart(&mut state, DESKTOP);
    assert_eq!(first, second);
    assert!(first.contains("\u{25cf}"), "{first}");
    assert!(!contains_any(&first, &DOTS_WORKING_FRAMES), "{first}");
}

#[test]
fn timer_repaints_only_when_a_drawn_working_frame_is_due() {
    let mut state = spinner_state(AgentStatus::Working, StatusIndicatorStyle::Dots, true);
    state.compose(DESKTOP.0, DESKTOP.1).expect("working frame");
    assert!(state.config.spinner_drawn.get());
    let drawn = state.config.spinner_frame as u32;
    let same_frame = state.spinner_epoch + WORKING_SPINNER_FRAME_PERIOD * drawn;
    let next_frame = state.spinner_epoch + WORKING_SPINNER_FRAME_PERIOD * (drawn + 1);
    assert!(!state.tick_working_spinner(same_frame));
    assert!(state.tick_working_spinner(next_frame));

    for (status, animate_working) in [(AgentStatus::Idle, true), (AgentStatus::Working, false)] {
        let mut state = spinner_state(status, StatusIndicatorStyle::Dots, animate_working);
        state.compose(DESKTOP.0, DESKTOP.1).expect("static frame");
        assert!(!state.config.spinner_drawn.get());
        let later = state.spinner_epoch + WORKING_SPINNER_FRAME_PERIOD * 5;
        assert!(
            !state.tick_working_spinner(later),
            "{status:?} {animate_working}"
        );
    }
}
