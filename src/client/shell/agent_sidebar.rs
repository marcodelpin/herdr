use std::collections::HashMap;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
    widgets::{Paragraph, Widget},
};

use super::*;

pub(super) struct AgentRow {
    pub(super) pane_id: String,
    pub(super) display: DisplayState,
    pub(super) focused: bool,
    pub(super) rows: Vec<Vec<crate::ui::ResolvedToken>>,
}

pub(super) fn ordered_agent_pane_ids(
    snapshot: &ClientShellSnapshot,
    sort: crate::config::AgentPanelSortConfig,
) -> Vec<String> {
    if snapshot.agent_view_label.is_some() {
        return snapshot
            .agent_order
            .iter()
            .filter(|pane_id| {
                snapshot
                    .agents
                    .iter()
                    .any(|agent| agent.pane_id == pane_id.as_str())
            })
            .cloned()
            .collect();
    }
    let mut agents = snapshot.agents.iter().collect::<Vec<_>>();
    if sort == crate::config::AgentPanelSortConfig::Priority {
        agents.sort_by_key(|agent| {
            (
                std::cmp::Reverse(status_priority(agent.agent_status)),
                std::cmp::Reverse(agent.state_change_seq),
            )
        });
    }
    agents
        .into_iter()
        .map(|agent| agent.pane_id.clone())
        .collect()
}

pub(super) fn render_agent_panel(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
) {
    let rows = agent_rows(snapshot, config, None);
    let counts = header_counts(rows.len(), &host_count_tokens([snapshot]), 0);
    if !render_agent_panel_header(
        buffer,
        area,
        snapshot.agent_view_label.as_deref(),
        &counts,
        config,
        hits,
    ) {
        return;
    }

    render_agent_list(
        buffer,
        area,
        &rows,
        snapshot
            .agent_view_label
            .as_ref()
            .map(|_| " no matching agents"),
        config,
        agent_scroll,
        hits,
        |row| row.rows.len(),
        |buffer, rect, row, hits| {
            hits.agents.push((rect, row.pane_id.clone()));
            render_agent_row(buffer, rect, row, false, config);
        },
    );
}

/// The host-wide agent counts herdr-ccwait publishes on every workspace (herdr-47w): subagents,
/// detached workers, workflows, Claude sessions without a pane.
const HOST_COUNT_TOKENS: [(&str, &str); 4] = [
    ("A", "agents_a"),
    ("W", "agents_w"),
    ("F", "agents_f"),
    ("S", "agents_s"),
];

/// One token set per server (per host): the first workspace that carries any host count. Every
/// workspace carries the same values, so reading one per host avoids counting a host N times.
/// Two endpoints connected to the same server process (local plus ssh-to-self, two aliases)
/// share its boot_id and contribute once; an empty boot_id is never merged with another.
pub(super) fn host_count_tokens<'a>(
    snapshots: impl IntoIterator<Item = &'a ClientShellSnapshot>,
) -> Vec<&'a [(String, String)]> {
    let mut seen_boots = std::collections::HashSet::new();
    snapshots
        .into_iter()
        .filter(|snapshot| {
            snapshot.boot_id.is_empty() || seen_boots.insert(snapshot.boot_id.as_str())
        })
        .filter_map(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .map(|workspace| workspace.tokens.as_slice())
                .find(|tokens| {
                    tokens
                        .iter()
                        .any(|(key, _)| HOST_COUNT_TOKENS.iter().any(|(_, k)| key == k))
                })
        })
        .collect()
}

/// The suffix of the agents header: "P:<panes>" and, per host count some host reported,
/// " <letter>:<sum over hosts>". A count no host reported (an older herdr-ccwait) is left out
/// rather than shown as 0, which would state something nobody measured. A sum that misses a
/// host - one that did not report that count (missing or non-numeric), or `unreachable_hosts`
/// endpoints that are not online - is marked with a trailing '?': it is a lower bound.
pub(super) fn header_counts(
    panes: usize,
    token_sets: &[&[(String, String)]],
    unreachable_hosts: usize,
) -> String {
    let mut out = format!("P:{panes}");
    for (letter, key) in HOST_COUNT_TOKENS {
        let values = token_sets
            .iter()
            .filter_map(|tokens| tokens.iter().find(|(k, _)| k == key))
            .filter_map(|(_, value)| value.parse::<u64>().ok())
            .collect::<Vec<_>>();
        if !values.is_empty() {
            let partial = unreachable_hosts > 0 || values.len() < token_sets.len();
            let mark = if partial { "?" } else { "" };
            out.push_str(&format!(" {letter}:{}{mark}", values.iter().sum::<u64>()));
        }
    }
    out
}

/// Fits the space-separated count fields into `max` cells without cutting a number: trailing
/// fields that do not fit are dropped and replaced by " +". Only when even the first field
/// cannot fit is it cut, since a partial "P:" still beats an empty rule.
pub(super) fn fit_counts(counts: &str, max: usize) -> String {
    let fields = counts
        .split(' ')
        .filter(|f| !f.is_empty())
        .collect::<Vec<_>>();
    let joined = fields.join(" ");
    if joined.chars().count() <= max {
        return joined;
    }
    for keep in (1..fields.len()).rev() {
        let candidate = format!("{} +", fields[..keep].join(" "));
        if candidate.chars().count() <= max {
            return candidate;
        }
    }
    joined.chars().take(max).collect()
}

/// The rule row above the agents label: a box-drawing rule carrying " <counts> " after its first
/// cell, filled to `width` cells, or a plain rule when there are no counts. Truncated to `width`,
/// so a narrow sidebar clips the tail.
pub(super) fn header_rule_text(counts: &str, width: usize) -> String {
    let dash = '\u{2500}';
    let mut text = if counts.is_empty() {
        String::new()
    } else {
        format!("{dash} {counts} ")
    };
    let used = text.chars().count();
    if used < width {
        text.extend(std::iter::repeat_n(dash, width - used));
    }
    text.chars().take(width).collect()
}

pub(super) fn render_agent_panel_header(
    buffer: &mut Buffer,
    area: Rect,
    agent_view_label: Option<&str>,
    counts: &str,
    config: &ClientShellConfig,
    hits: &mut ShellHitMap,
) -> bool {
    if area.height == 0 {
        return false;
    }
    // "<rule> " before the counts and one cell after them keep at least one rule cell visible.
    let shown = fit_counts(counts, (area.width as usize).saturating_sub(3));
    put_text(
        buffer,
        area.x,
        area.y,
        area.width,
        &header_rule_text(&shown, area.width as usize),
        Style::default().fg(config.palette.surface_dim),
    );
    // The counts sit on the rule row, not beside "agents": at the default sidebar width (26) the
    // right-aligned sort toggle on the label row would overwrite them (herdr-47w review r1).
    if !shown.is_empty() {
        put_text(
            buffer,
            area.x.saturating_add(2),
            area.y,
            area.width.saturating_sub(2),
            &shown,
            Style::default().fg(config.palette.overlay0),
        );
    }
    if area.height < 2 {
        return false;
    }
    put_text(
        buffer,
        area.x,
        area.y + 1,
        area.width,
        " agents",
        Style::default()
            .fg(config.palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    let sort_label = agent_view_label.unwrap_or(match config.agent_panel_sort {
        crate::config::AgentPanelSortConfig::Spaces => "grouped",
        crate::config::AgentPanelSortConfig::Priority => "priority",
    });
    let sort_width = display_width(sort_label).min(area.width as usize) as u16;
    let sort_rect = Rect::new(
        area.right().saturating_sub(sort_width),
        area.y + 1,
        sort_width,
        1,
    );
    hits.agent_sort_toggle = if config.mouse_capture && agent_view_label.is_none() {
        sort_rect
    } else {
        Rect::default()
    };
    put_text(
        buffer,
        sort_rect.x,
        sort_rect.y,
        sort_rect.width,
        sort_label,
        Style::default()
            .fg(if agent_view_label.is_some() {
                config.palette.accent
            } else {
                config.palette.overlay0
            })
            .add_modifier(Modifier::BOLD),
    );
    true
}

pub(super) fn render_agent_list<T>(
    buffer: &mut Buffer,
    area: Rect,
    rows: &[T],
    empty_message: Option<&str>,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
    row_lines: impl Fn(&T) -> usize,
    mut render_row: impl FnMut(&mut Buffer, Rect, &T, &mut ShellHitMap),
) {
    let body = Rect::new(
        area.x,
        area.y.saturating_add(3),
        area.width,
        area.height.saturating_sub(3),
    );
    hits.agent_body = body;
    if body.is_empty() || rows.is_empty() {
        *agent_scroll = 0;
        if let Some(message) = empty_message.filter(|_| !body.is_empty()) {
            put_text(
                buffer,
                body.x,
                body.y,
                body.width,
                message,
                Style::default()
                    .fg(config.palette.overlay0)
                    .add_modifier(Modifier::DIM),
            );
        }
        return;
    }

    let row_heights = rows
        .iter()
        .map(|row| row_lines(row).max(1).min(u16::MAX as usize) as u16)
        .collect::<Vec<_>>();
    let gaps = rows
        .iter()
        .enumerate()
        .map(|(index, _)| {
            if index + 1 < rows.len() {
                config.agents.row_gap
            } else {
                0
            }
        })
        .collect::<Vec<_>>();
    let metrics =
        super::scroll::list_scroll_metrics(&row_heights, &gaps, body.height, *agent_scroll);
    hits.agent_max_scroll = metrics.max_offset_from_bottom;
    hits.agent_scroll_metrics = Some(metrics);
    *agent_scroll = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let show_scrollbar = metrics.max_offset_from_bottom > 0 && body.width > 1;
    let content_width = body.width.saturating_sub(u16::from(show_scrollbar));
    let mut y = body.y;
    for (index, row) in rows.iter().enumerate().skip(*agent_scroll) {
        let height = row_heights[index].min(body.height);
        if y.saturating_add(height) > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, content_width, height);
        render_row(buffer, rect, row, hits);
        y = y
            .saturating_add(height)
            .saturating_add(if index + 1 < rows.len() {
                config.agents.row_gap
            } else {
                0
            });
    }

    if show_scrollbar {
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
        hits.agent_scrollbar = track;
        super::scroll::render_list_scrollbar(buffer, track, metrics, &config.palette);
    }
}

pub(super) fn agent_rows(
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    machine: Option<&str>,
) -> Vec<AgentRow> {
    ordered_agent_pane_ids(snapshot, config.agent_panel_sort)
        .into_iter()
        .filter_map(|pane_id| agent_row(snapshot, &pane_id, config, machine))
        .collect()
}

pub(super) fn agent_row(
    snapshot: &ClientShellSnapshot,
    pane_id: &str,
    config: &ClientShellConfig,
    machine: Option<&str>,
) -> Option<AgentRow> {
    let agent = snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)?;
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == agent.workspace_id)?;
    let tab = snapshot.tabs.iter().find(|tab| tab.tab_id == agent.tab_id);
    let pane = snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_id == agent.pane_id);
    let tab_count = snapshot
        .tabs
        .iter()
        .filter(|candidate| candidate.workspace_id == agent.workspace_id)
        .count();
    let tab_label = tab
        .filter(|tab| tab_count > 1 || tab.custom_label)
        .map(|tab| tab.label.as_str());
    let agent_label = agent
        .display_agent
        .as_deref()
        .or(agent.name.as_deref())
        .or(agent.agent.as_deref())
        .or(agent.title.as_deref());
    let labels = agent
        .state_labels
        .iter()
        .cloned()
        .collect::<HashMap<_, _>>();
    let tokens = agent.tokens.iter().cloned().collect::<HashMap<_, _>>();
    let state_text = labels
        .get(status_text(agent.agent_status))
        .map(String::as_str)
        .unwrap_or_else(|| sidebar_status_text(agent.agent_status));
    let canonical_agent = agent
        .agent
        .as_deref()
        .and_then(crate::detect::parse_agent_label);
    let rows = crate::ui::sidebar_agent_rows(
        &config.agents,
        crate::ui::AgentTokenContext {
            machine,
            workspace: &workspace.label,
            tab: tab_label,
            pane: agent
                .title
                .as_deref()
                .or_else(|| pane.and_then(|pane| pane.label.as_deref())),
            agent_label,
            terminal_title: agent.terminal_title.as_deref(),
            terminal_title_stripped: agent.terminal_title_stripped.as_deref(),
            canonical_agent,
            tokens: &tokens,
        },
        state_text,
    );
    Some(AgentRow {
        pane_id: agent.pane_id.clone(),
        display: agent_display_state(agent, config),
        focused: agent.focused,
        rows,
    })
}

pub(super) fn render_agent_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &AgentRow,
    stale: bool,
    config: &ClientShellConfig,
) {
    let palette = &config.palette;
    let row_style = if row.focused {
        Style::default().bg(palette.active_row_bg)
    } else {
        Style::default()
    };
    let name_style = if row.focused {
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(palette.subtext0)
            .add_modifier(Modifier::BOLD)
    };
    let status_style = display_state_style(row.display, palette);
    let secondary = Style::default().fg(palette.overlay0);
    let icon_style = display_state_style(row.display, palette);
    let rows = if row.rows.is_empty() {
        vec![vec![crate::ui::ResolvedToken {
            kind: crate::ui::ResolvedTokenKind::StateIcon,
            style: Default::default(),
        }]]
    } else {
        row.rows.clone()
    };
    for (index, tokens) in rows.iter().take(rect.height as usize).enumerate() {
        let indent = if index == 0 { 1 } else { 3 };
        let (icon, clock) = token_row_state_icon(tokens, row.display, config, stale);
        let row_width = rect.width.saturating_sub(indent as u16);
        let line = crate::ui::resolved_token_spans(
            tokens,
            (icon, icon_style),
            status_style,
            name_style,
            secondary,
            secondary,
            palette,
            row_width as usize,
        );
        config.mark_spinner_drawn_if(
            clock,
            token_row_icon_reached_frame(line.state_icon_column, icon, row_width),
        );
        let mut spans = vec![ratatui::text::Span::raw(" ".repeat(indent))];
        spans.extend(line.spans);
        Paragraph::new(Line::from(spans)).style(row_style).render(
            Rect::new(rect.x, rect.y + index as u16, rect.width, 1),
            buffer,
        );
    }
}

fn put_text(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    for (offset, character) in text.chars().take(width as usize).enumerate() {
        if let Some(cell) = buffer.cell_mut((x + offset as u16, y)) {
            cell.set_char(character).set_style(style);
        }
    }
}

fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

fn sidebar_status_text(status: crate::api::schema::AgentStatus) -> &'static str {
    use crate::api::schema::AgentStatus;
    match status {
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Working => "working",
        AgentStatus::Idle | AgentStatus::Unknown => "idle",
    }
}

#[cfg(test)]
mod header_counts_tests {
    use super::{
        fit_counts, header_counts, header_rule_text, host_count_tokens, render_agent_panel_header,
    };

    fn tokens(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    // herdr-47w: P from the agent rows, A/W/F/S summed over the hosts that reported them.
    #[test]
    fn agents_header_counts_sum_hosts_and_keep_zeros() {
        let dcc = tokens(&[
            ("agents_a", "0"),
            ("agents_w", "7"),
            ("agents_f", "1"),
            ("agents_s", "0"),
        ]);
        let local = tokens(&[
            ("agents_a", "2"),
            ("agents_w", "1"),
            ("agents_f", "0"),
            ("agents_s", "3"),
        ]);
        assert_eq!(header_counts(12, &[&dcc], 0), "P:12 A:0 W:7 F:1 S:0");
        assert_eq!(
            header_counts(12, &[&dcc, &local], 0),
            "P:12 A:2 W:8 F:1 S:3"
        );
    }

    // an older herdr-ccwait publishes nothing: P alone, never an invented 0
    #[test]
    fn agents_header_counts_without_host_tokens_show_only_panes() {
        assert_eq!(header_counts(5, &[], 0), "P:5");
        let partial = tokens(&[("agents_w", "4"), ("agents_s", "not-a-number")]);
        assert_eq!(header_counts(5, &[&partial], 0), "P:5 W:4");
    }

    fn row(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| {
                buffer
                    .cell((x, y))
                    .map(|c| c.symbol().to_owned())
                    .unwrap_or_default()
            })
            .collect()
    }

    // herdr-47w review r1: at the default sidebar width (26) every count must stay visible;
    // beside "agents" the right-aligned sort toggle overwrote W, F and S.
    #[test]
    fn agents_header_counts_visible_at_default_sidebar_width() {
        // 26-cell sidebar minus the border cell (ui/sidebar.rs expanded_sidebar_sections)
        let width = 25u16;
        let area = ratatui::layout::Rect::new(0, 0, width, 2);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        let config = crate::client::shell::state::ClientShellConfig::from_config(
            &crate::config::Config::default(),
        );
        let mut hits = crate::client::shell::state::ShellHitMap::default();
        let counts = "P:12 A:20 W:80 F:10 S:30";
        assert!(render_agent_panel_header(
            &mut buffer,
            area,
            None,
            counts,
            &config,
            &mut hits
        ));
        let rule = row(&buffer, 0, width);
        let expected = fit_counts(counts, width as usize - 3);
        assert!(
            rule.contains(&expected),
            "rule row {rule:?} lost {expected:?}"
        );
        assert!(
            !rule.contains("S:3") || rule.contains("S:30"),
            "cut number in {rule:?}"
        );
        let label = row(&buffer, 1, width);
        assert!(label.starts_with(" agents"), "label row {label:?}");
        assert!(
            !label.contains("P:"),
            "counts must not share the sort-toggle row: {label:?}"
        );
    }

    // codex r2 finding 1: never cut inside a number; drop whole fields and say so with '+'
    #[test]
    fn fit_counts_drops_whole_fields() {
        let counts = "P:12 A:20 W:80 F:10 S:30";
        assert_eq!(fit_counts(counts, 22), "P:12 A:20 W:80 F:10 +");
        assert_eq!(fit_counts(counts, 24), counts);
        assert_eq!(fit_counts(counts, 8), "P:12 +");
        assert_eq!(fit_counts(counts, 3), "P:1");
        for max in 0..30 {
            let fitted = fit_counts(counts, max);
            assert!(fitted.chars().count() <= max, "{max}: {fitted:?}");
            for field in fitted.split(' ').filter(|f| *f != "+") {
                assert!(
                    counts.split(' ').any(|c| c == field) || fitted.chars().count() == max,
                    "{max}: cut field {field:?} in {fitted:?}"
                );
            }
        }
    }

    // codex r2 findings 2 + 4: the real selector - any host key counts, one server boot once
    #[test]
    fn host_count_tokens_selects_any_key_and_dedupes_boot() {
        let mut only_w = crate::client::shell::tests::snapshot();
        only_w.workspaces[0].tokens = vec![("agents_w".to_owned(), "4".to_owned())];
        let sets = host_count_tokens([&only_w]);
        assert_eq!(header_counts(5, &sets, 0), "P:5 W:4");

        let mut other = crate::client::shell::tests::snapshot();
        other.boot_id = "boot-2".into();
        other.workspaces[0].tokens = vec![
            ("agents_w".to_owned(), "3".to_owned()),
            ("agents_s".to_owned(), "1".to_owned()),
        ];
        let same_boot = only_w.clone();
        let sets = host_count_tokens([&only_w, &same_boot, &other]);
        assert_eq!(sets.len(), 2, "one contribution per boot_id");
        // W summed over both hosts; S only from one of two hosts, so it is a lower bound
        assert_eq!(header_counts(5, &sets, 0), "P:5 W:7 S:1?");
    }

    // codex r2 finding 3: an endpoint that is not online makes every total a lower bound
    #[test]
    fn header_counts_marks_totals_when_a_host_is_unreachable() {
        let t = tokens(&[("agents_a", "1"), ("agents_w", "2")]);
        assert_eq!(header_counts(3, &[&t], 1), "P:3 A:1? W:2?");
    }

    #[test]
    fn header_rule_text_fills_and_truncates() {
        let dash = '\u{2500}';
        assert_eq!(header_rule_text("", 4), dash.to_string().repeat(4));
        let t = header_rule_text("P:1", 10);
        assert_eq!(t.chars().count(), 10);
        assert!(t.starts_with(&format!("{dash} P:1 ")));
        assert_eq!(header_rule_text("P:12 A:2", 5).chars().count(), 5);
    }
}
