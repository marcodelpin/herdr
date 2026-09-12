use std::collections::{HashMap, HashSet, VecDeque};

mod actions;
mod agent_sidebar;
mod aggregate_navigation;
mod composition;
mod config;
mod context_menu;
mod copy_mode;
mod endpoint_agent_state;
mod endpoint_agents;
mod endpoint_navigation;
mod endpoint_notices;
mod endpoint_sidebar;
mod endpoints;
pub(super) use endpoints::*;
mod global_menu;
mod graphics;
mod input;
mod input_source;
mod mobile;
mod mouse;
mod notification_policy;
mod notifications;
mod overlay_input;
mod preferences;
mod render;
mod scroll;
mod settings;
mod state;
mod surface_patch;
mod worktrees;

pub(in crate::client::shell) use render::sidebar;
pub(crate) use state::*;
#[cfg(test)]
pub(super) use surface_patch::apply_composed_surface_patch;
pub(super) use surface_patch::{ClientComposedSurfacePatch, ClientPaneSurfacePatchOutcome};

use crossterm::event::KeyCode;
#[cfg(test)]
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::endpoint::{ClientEndpointId, ClientEndpointStatus, SavedSshEndpoint};
use crate::app::state::Palette;
use crate::config::{
    Config, LiveKeybindConfig, SidebarCollapsedModeConfig, SpacesSidebarConfig,
    TabBarPositionConfig,
};
use crate::protocol::{
    ClientMessage, ClientMousePosition, ClientPaneInputEvent, ClientShellSnapshot, ClientShellTab,
    ClientShellWorkspace, ClientSurfaceSize, FrameData, PaneSurfaceFrame, SemanticNotification,
    SemanticNotificationKind, SemanticNotificationSound,
};
#[cfg(test)]
use crate::raw_input::RawInputEvent;

fn delete_overlay_word(rename: &mut ClientRenameOverlay) {
    if rename.replace_on_type {
        rename.input.clear();
        rename.replace_on_type = false;
        return;
    }
    while rename.input.chars().last().is_some_and(char::is_whitespace) {
        rename.input.pop();
    }
    let Some(word) = rename
        .input
        .chars()
        .last()
        .map(|character| character.is_alphanumeric() || character == '_')
    else {
        return;
    };
    while rename.input.chars().last().is_some_and(|character| {
        !character.is_whitespace() && (character.is_alphanumeric() || character == '_') == word
    }) {
        rename.input.pop();
    }
}

fn target_event_message(target: ClientInputTarget, event: ClientPaneInputEvent) -> ClientMessage {
    match target {
        ClientInputTarget::Pane(pane_id) => ClientMessage::ClientShellPaneInput {
            pane_id,
            events: vec![event],
        },
        ClientInputTarget::Popup(terminal_id) => ClientMessage::ClientShellPopupInput {
            terminal_id,
            events: vec![event],
        },
    }
}

fn push_target_event(
    target: ClientInputTarget,
    event: ClientPaneInputEvent,
    outcome: &mut ClientShellInput,
) {
    match target {
        ClientInputTarget::Pane(pane_id) => {
            if let Some(ClientMessage::ClientShellPaneInput {
                pane_id: pending_pane,
                events,
            }) = outcome.requests.last_mut()
            {
                if *pending_pane == pane_id {
                    events.push(event);
                    return;
                }
            }
            outcome.requests.push(target_event_message(
                ClientInputTarget::Pane(pane_id),
                event,
            ));
        }
        ClientInputTarget::Popup(terminal_id) => {
            if let Some(ClientMessage::ClientShellPopupInput {
                terminal_id: pending_terminal,
                events,
            }) = outcome.requests.last_mut()
            {
                if *pending_terminal == terminal_id {
                    events.push(event);
                    return;
                }
            }
            outcome.requests.push(target_event_message(
                ClientInputTarget::Popup(terminal_id),
                event,
            ));
        }
    }
}

fn contains(rect: Rect, point: (u16, u16)) -> bool {
    rect.width > 0
        && rect.height > 0
        && point.0 >= rect.x
        && point.0 < rect.right()
        && point.1 >= rect.y
        && point.1 < rect.bottom()
}

fn pane_surface_topology_signature(surface: &PaneSurfaceFrame) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn write(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(PRIME);
        }
        *hash ^= 0xff;
        *hash = hash.wrapping_mul(PRIME);
    }

    let mut pane_ids = surface
        .panes
        .iter()
        .map(|pane| pane.pane_id.as_bytes())
        .collect::<Vec<_>>();
    pane_ids.sort_unstable();
    let mut hash = OFFSET;
    for pane_id in pane_ids {
        write(&mut hash, pane_id);
    }
    let mut splits = surface.splits.iter().collect::<Vec<_>>();
    splits.sort_by(|left, right| left.path.cmp(&right.path));
    for split in splits {
        write(
            &mut hash,
            &[match split.direction {
                crate::protocol::PaneSurfaceSplitDirection::Horizontal => 0,
                crate::protocol::PaneSurfaceSplitDirection::Vertical => 1,
            }],
        );
        write(
            &mut hash,
            &split
                .path
                .iter()
                .map(|right| u8::from(*right))
                .collect::<Vec<_>>(),
        );
    }
    hash
}

/// Working spinner for the dots style: the braille block (U+2800..U+28FF) that
/// `terminal::title` already treats as an agent activity glyph.
const DOTS_WORKING_FRAMES: [&str; 10] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280f}",
];
/// Working spinner for the symbols style: the quarter-circle rotation (U+25D0,
/// U+25D3, U+25D1, U+25D2) whose first frame is the static symbols Working glyph.
const SYMBOLS_WORKING_FRAMES: [&str; 4] = ["\u{25d0}", "\u{25d3}", "\u{25d1}", "\u{25d2}"];
const WORKING_SPINNER_FRAME_PERIOD: std::time::Duration = std::time::Duration::from_millis(200);

/// Waiting spinner, shared by both indicator styles: the hourglass turning over
/// (U+29D7 black hourglass, U+29D6 white hourglass). Frame 0 is the static Waiting
/// glyph, and the only glyph the never-animated Stale state shows.
const WAITING_FRAMES: [&str; 2] = ["\u{29d7}", "\u{29d6}"];
/// Waiting turns over five times slower than the Working spinner: the pane is not
/// producing output, it is holding, so the glyph reads as a pulse, not as activity.
const WAITING_SPINNER_FRAME_PERIOD: std::time::Duration = std::time::Duration::from_millis(1000);

/// The `herdr-ccwait` pane metadata tokens (bd herdr-qlk.1) that report background
/// work still pending behind an idle Claude Code prompt. A cleared token arrives as
/// an empty value, which counts as absent.
const WAIT_TOKEN: &str = "wait";
const STALE_TOKEN: &str = "stale";

fn spinner_frame_at(elapsed: std::time::Duration) -> usize {
    (elapsed.as_millis() / WORKING_SPINNER_FRAME_PERIOD.as_millis()) as usize
}

fn waiting_frame_at(elapsed: std::time::Duration) -> usize {
    (elapsed.as_millis() / WAITING_SPINNER_FRAME_PERIOD.as_millis()) as usize
}

/// What one row shows: an agent status, or the WAITING / STALE state the
/// `herdr-ccwait` tokens report while herdr itself only sees an idle prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DisplayState {
    Status(crate::api::schema::AgentStatus),
    Waiting,
    Stale,
}

/// The clock an animated glyph advances on. Both run off the one composition epoch,
/// so no row needs a timer of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SpinnerClock {
    Working,
    Waiting,
}

/// The display state of a row that speaks for `tokens`: one pane's own tokens, or
/// the tokens of every agent a rollup row aggregates.
///
/// Only Idle and Done are overridden, so a Working, Blocked or Unknown pane keeps
/// its own glyph even while a stale `wait` value is still cached for it. `stale`
/// beats `wait` wherever both appear.
fn display_state_of<'a>(
    status: crate::api::schema::AgentStatus,
    tokens: impl IntoIterator<Item = (&'a str, &'a str)>,
    waiting_indicator: bool,
) -> DisplayState {
    if !waiting_indicator || !display_state_overridable(status) {
        return DisplayState::Status(status);
    }
    let mut waiting = false;
    for (key, value) in tokens {
        if value.is_empty() {
            continue;
        }
        if key == STALE_TOKEN {
            return DisplayState::Stale;
        }
        if key == WAIT_TOKEN {
            waiting = true;
        }
    }
    if waiting {
        DisplayState::Waiting
    } else {
        DisplayState::Status(status)
    }
}

/// Which statuses the WAITING / STALE tokens may override: a pane herdr already
/// reads as busy keeps its own glyph. A rollup row asks the same question of the
/// status it rolled up, so both go through here.
fn display_state_overridable(status: crate::api::schema::AgentStatus) -> bool {
    use crate::api::schema::AgentStatus;
    matches!(status, AgentStatus::Idle | AgentStatus::Done)
}

/// `display_state_of` for the reported tokens of one pane or workspace.
fn display_state(
    status: crate::api::schema::AgentStatus,
    tokens: &[(String, String)],
    waiting_indicator: bool,
) -> DisplayState {
    display_state_of(status, token_pairs(tokens), waiting_indicator)
}

/// The display state of one agent row, from that pane's own reported tokens.
fn agent_display_state(
    agent: &crate::protocol::ClientShellAgent,
    config: &ClientShellConfig,
) -> DisplayState {
    display_state(agent.agent_status, &agent.tokens, config.waiting_indicator)
}

fn token_pairs(tokens: &[(String, String)]) -> impl Iterator<Item = (&str, &str)> {
    tokens
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
}

/// `spinner_frame` animates only the Working glyph; `None` keeps the static glyph
/// and every other status is static regardless.
fn status_icon(
    status: crate::api::schema::AgentStatus,
    style: crate::config::StatusIndicatorStyle,
    spinner_frame: Option<usize>,
) -> &'static str {
    use crate::api::schema::AgentStatus;
    use crate::config::StatusIndicatorStyle;
    if let (AgentStatus::Working, Some(frame)) = (status, spinner_frame) {
        return match style {
            StatusIndicatorStyle::Dots => DOTS_WORKING_FRAMES[frame % DOTS_WORKING_FRAMES.len()],
            StatusIndicatorStyle::Symbols => {
                SYMBOLS_WORKING_FRAMES[frame % SYMBOLS_WORKING_FRAMES.len()]
            }
        };
    }
    match (style, status) {
        (
            StatusIndicatorStyle::Dots,
            AgentStatus::Working | AgentStatus::Blocked | AgentStatus::Done,
        ) => "\u{25cf}",
        (StatusIndicatorStyle::Dots, AgentStatus::Idle) => "\u{25cb}",
        (StatusIndicatorStyle::Dots, AgentStatus::Unknown) => "\u{00b7}",
        (StatusIndicatorStyle::Symbols, AgentStatus::Blocked) => "\u{00d7}",
        (StatusIndicatorStyle::Symbols, AgentStatus::Working) => "\u{25d0}",
        (StatusIndicatorStyle::Symbols, AgentStatus::Done) => "\u{2713}",
        (StatusIndicatorStyle::Symbols, AgentStatus::Idle) => "\u{25cb}",
        (StatusIndicatorStyle::Symbols, AgentStatus::Unknown) => "\u{00b7}",
    }
}

/// The glyph for a display state. `spinner_frame` animates the Working and Waiting
/// glyphs; `None` keeps their static frame, and Stale is static regardless.
fn display_state_icon(
    display: DisplayState,
    style: crate::config::StatusIndicatorStyle,
    spinner_frame: Option<usize>,
) -> &'static str {
    match display {
        DisplayState::Status(status) => status_icon(status, style, spinner_frame),
        DisplayState::Waiting => WAITING_FRAMES[spinner_frame.unwrap_or(0) % WAITING_FRAMES.len()],
        DisplayState::Stale => WAITING_FRAMES[0],
    }
}

/// A row's configured state icon and the clock it animates on, without recording
/// anything: the caller marks the composition only once the glyph is known to be on
/// screen. A row of a disconnected (`stale`) endpoint keeps the static glyph, since
/// its cached state is not live activity.
fn lookup_display_icon(
    display: DisplayState,
    config: &ClientShellConfig,
    stale: bool,
) -> (&'static str, Option<SpinnerClock>) {
    let clock = if stale {
        None
    } else {
        config.spinner_clock_for(display)
    };
    (
        display_state_icon(
            display,
            config.status_indicators,
            clock.map(|clock| config.spinner_frame_for(clock)),
        ),
        clock,
    )
}

/// Whether a `put_text` that wrote `written` columns reached the state icon, which
/// sits `lead` columns into the text that call was given. The mark goes through
/// `mark_spinner_drawn_if` with this, so a row too narrow for the glyph animates
/// nothing.
fn icon_reached_frame(written: u16, lead: u16, icon: &str) -> bool {
    written >= lead.saturating_add(render::display_width(icon))
}

/// The glyph and the clock for the `state_icon` token of one rendered token row,
/// recording nothing. `resolved_token_spans` draws the icon only for that token, so a
/// row without it gets the static glyph and no clock at all. The caller marks the
/// composition through `token_row_icon_reached_frame` once the row is laid out.
fn token_row_state_icon(
    row: &[crate::ui::ResolvedToken],
    display: DisplayState,
    config: &ClientShellConfig,
    stale: bool,
) -> (&'static str, Option<SpinnerClock>) {
    if row
        .iter()
        .any(|token| matches!(token.kind, crate::ui::ResolvedTokenKind::StateIcon))
    {
        lookup_display_icon(display, config, stale)
    } else {
        (
            display_state_icon(display, config.status_indicators, None),
            None,
        )
    }
}

/// Whether ANY state icon of a laid-out token row survives the clip: `columns` are the
/// columns `resolved_token_spans` placed the glyphs at and `max_width` is the width the
/// row's `Paragraph` renders into. Fixed tokens are never dropped from the span list, so
/// a glyph can be laid out past the budget and never reach the frame - and a layout that
/// names `state_icon` twice draws one glyph that survives and one that does not, so the
/// row animates as long as a single one of them is on screen.
fn token_row_icon_reached_frame(columns: &[usize], icon: &str, max_width: u16) -> bool {
    columns.iter().any(|column| {
        icon_reached_frame(max_width, u16::try_from(*column).unwrap_or(u16::MAX), icon)
    })
}

fn status_dot(status: crate::api::schema::AgentStatus) -> &'static str {
    status_icon(status, crate::config::StatusIndicatorStyle::Dots, None)
}

fn status_priority(status: crate::api::schema::AgentStatus) -> u8 {
    use crate::api::schema::AgentStatus;
    match status {
        AgentStatus::Blocked => 4,
        AgentStatus::Done => 3,
        AgentStatus::Working => 2,
        AgentStatus::Idle => 1,
        AgentStatus::Unknown => 0,
    }
}

fn status_text(status: crate::api::schema::AgentStatus) -> &'static str {
    use crate::api::schema::AgentStatus;
    match status {
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Idle => "idle",
        AgentStatus::Unknown => "unknown",
    }
}

fn status_color(
    status: crate::api::schema::AgentStatus,
    palette: &Palette,
) -> ratatui::style::Color {
    use crate::api::schema::AgentStatus;
    match status {
        AgentStatus::Working => palette.yellow,
        AgentStatus::Blocked => palette.red,
        AgentStatus::Done => palette.teal,
        AgentStatus::Idle => palette.green,
        AgentStatus::Unknown => palette.overlay0,
    }
}

/// The colour of a display state: Waiting borrows the informational blue, Stale the
/// blocked red, every status keeps `status_color`.
fn display_state_color(display: DisplayState, palette: &Palette) -> ratatui::style::Color {
    match display {
        DisplayState::Status(status) => status_color(status, palette),
        DisplayState::Waiting => palette.blue,
        DisplayState::Stale => palette.red,
    }
}

/// Stale is the one display state that carries weight of its own.
fn display_state_modifier(display: DisplayState) -> Modifier {
    match display {
        DisplayState::Stale => Modifier::BOLD,
        DisplayState::Status(_) | DisplayState::Waiting => Modifier::empty(),
    }
}

/// The dimming a row applies to a state text it colours by the display state,
/// dropped for Stale so the bold red the operator must notice keeps its weight.
fn display_state_dim(display: DisplayState) -> Modifier {
    match display {
        DisplayState::Stale => Modifier::empty(),
        DisplayState::Status(_) | DisplayState::Waiting => Modifier::DIM,
    }
}

/// One style for a state icon, or for a state text coloured by the same state.
fn display_state_style(display: DisplayState, palette: &Palette) -> Style {
    Style::default()
        .fg(display_state_color(display, palette))
        .add_modifier(display_state_modifier(display))
}

fn panel_contrast_fg(palette: &Palette) -> ratatui::style::Color {
    match palette.panel_bg {
        ratatui::style::Color::Reset => palette.surface_dim,
        color => color,
    }
}

fn blit_pane_surface(target: &mut FrameData, source: &FrameData, area: Rect) {
    let copy_width = source.width.min(area.width);
    let copy_height = source.height.min(area.height);
    let hyperlink_base = target.hyperlinks.len() as u32;
    target.hyperlinks.extend(source.hyperlinks.iter().cloned());

    for row in 0..copy_height {
        for col in 0..copy_width {
            let source_index = row as usize * source.width as usize + col as usize;
            let target_x = area.x + col;
            let target_y = area.y + row;
            let target_index = target_y as usize * target.width as usize + target_x as usize;
            let (Some(source_cell), Some(target_cell)) = (
                source.cells.get(source_index),
                target.cells.get_mut(target_index),
            ) else {
                continue;
            };
            *target_cell = source_cell.clone();
            target_cell.hyperlink = source_cell.hyperlink.and_then(|index| {
                ((index as usize) < source.hyperlinks.len()).then_some(hyperlink_base + index)
            });
        }
    }

    target.cursor = source.cursor.as_ref().and_then(|cursor| {
        (cursor.x < copy_width && cursor.y < copy_height).then(|| crate::protocol::CursorState {
            x: area.x + cursor.x,
            y: area.y + cursor.y,
            visible: cursor.visible,
            shape: cursor.shape,
        })
    });
    target.graphics.clear();
}

#[cfg(test)]
mod tests;
