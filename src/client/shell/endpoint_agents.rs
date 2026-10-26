use super::render::put_text;
use super::*;

pub(super) fn render_collapsed(
    buffer: &mut Buffer,
    area: Rect,
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
    hits: &mut ShellHitMap,
) {
    let rows = agent_rows(endpoints, active_endpoint_id, config);
    for (index, row) in rows.into_iter().take(area.height as usize).enumerate() {
        let rect = Rect::new(area.x, area.y + index as u16, area.width, 1);
        if row.agent.focused {
            buffer.set_style(rect, Style::default().bg(config.palette.active_row_bg));
        }
        let initial = row.machine_label.chars().next().unwrap_or('?').to_string();
        let (icon, clock) = lookup_display_icon(row.agent.display, config, row.stale);
        let written = put_text(
            buffer,
            rect.x,
            rect.y,
            rect.width,
            &format!("{initial}{icon}"),
            Style::default()
                .fg(if row.stale {
                    config.palette.overlay0
                } else {
                    display_state_color(row.agent.display, &config.palette)
                })
                .add_modifier(if row.stale {
                    Modifier::DIM
                } else {
                    display_state_modifier(row.agent.display)
                }),
        );
        config.mark_spinner_drawn_if(
            clock,
            icon_reached_frame(written, super::render::display_width(&initial), icon),
        );
        hits.endpoint_agents
            .push((rect, row.endpoint_id, row.agent.pane_id));
    }
}

/// The host-count sources of the agents header (herdr-8u7): every endpoint that is not disabled
/// names a server; only an online one may contribute its counts.
pub(super) fn host_sources(
    endpoints: &[ClientShellEndpoint],
) -> impl Iterator<Item = super::agent_sidebar::HostSource<'_>> {
    endpoints
        .iter()
        .filter(|endpoint| {
            endpoint.status != crate::client::endpoint::ClientEndpointStatus::Disabled
        })
        .map(|endpoint| super::agent_sidebar::HostSource {
            snapshot: endpoint.snapshot.as_deref(),
            online: endpoint.status == crate::client::endpoint::ClientEndpointStatus::Online,
        })
}

pub(super) fn render_expanded(
    buffer: &mut Buffer,
    area: Rect,
    agent_view_label: Option<&str>,
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
) {
    let rows = agent_rows(endpoints, active_endpoint_id, config);
    // One model for the host counts (herdr-8u7): every non-disabled endpoint names a server; only
    // online ones contribute counts, and a total over fewer servers than known is a lower bound.
    let host = super::agent_sidebar::host_counts(host_sources(endpoints));
    let counts = super::agent_sidebar::header_counts(rows.len(), &host.token_sets, host.expected);
    if !super::agent_sidebar::render_agent_panel_header(
        buffer,
        area,
        agent_view_label,
        &counts,
        config,
        hits,
    ) {
        return;
    }
    super::agent_sidebar::render_agent_list(
        buffer,
        area,
        &rows,
        agent_view_label.map(|_| " no matching agents"),
        config,
        agent_scroll,
        hits,
        |row| row.agent.rows.len(),
        |buffer, rect, row, hits| {
            super::agent_sidebar::render_agent_row(buffer, rect, &row.agent, row.stale, config);
            if row.stale {
                buffer.set_style(
                    rect,
                    Style::default()
                        .fg(config.palette.overlay0)
                        .add_modifier(Modifier::DIM),
                );
            }
            hits.endpoint_agents
                .push((rect, row.endpoint_id.clone(), row.agent.pane_id.clone()));
        },
    );
}

impl ClientShellState {
    pub(super) fn reveal_endpoint_agent(
        &mut self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
        body_height: u16,
    ) {
        if body_height == 0 {
            return;
        }
        let rows = agent_rows(&self.endpoints, &self.active_endpoint_id, &self.config);
        let Some(target) = rows
            .iter()
            .position(|row| &row.endpoint_id == endpoint_id && row.agent.pane_id == pane_id)
        else {
            return;
        };
        let heights = rows
            .iter()
            .map(|row| row.agent.rows.len().max(1).min(u16::MAX as usize) as u16)
            .collect::<Vec<_>>();
        let mut gaps = vec![self.config.agents.row_gap; rows.len()];
        if let Some(last) = gaps.last_mut() {
            *last = 0;
        }
        self.agent_scroll = super::scroll::list_scroll_start_to_reveal(
            &heights,
            &gaps,
            body_height,
            self.agent_scroll,
            target,
        );
    }
}

struct EndpointAgentRow {
    endpoint_id: ClientEndpointId,
    machine_label: String,
    stale: bool,
    agent: super::agent_sidebar::AgentRow,
}

fn agent_rows(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
) -> Vec<EndpointAgentRow> {
    let mut rendered_rows = endpoints
        .iter()
        .filter_map(|endpoint| {
            endpoint.snapshot.as_deref().map(|snapshot| {
                snapshot
                    .agents
                    .iter()
                    .filter_map(|agent| {
                        super::agent_sidebar::agent_row(
                            snapshot,
                            &agent.pane_id,
                            config,
                            Some(&endpoint.label),
                        )
                    })
                    .map(|agent| ((endpoint.endpoint_id.clone(), agent.pane_id.clone()), agent))
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect::<HashMap<_, _>>();

    super::aggregate_navigation::aggregate_agent_rows(
        endpoints,
        active_endpoint_id,
        config.agent_panel_sort,
    )
    .into_iter()
    .filter_map(|row| {
        let key = (row.endpoint.endpoint_id.clone(), row.agent.pane_id.clone());
        let mut agent = rendered_rows.remove(&key)?;
        agent.focused &= row.endpoint.endpoint_id == active_endpoint_id;
        Some(EndpointAgentRow {
            endpoint_id: row.endpoint.endpoint_id.clone(),
            machine_label: row.endpoint.label.to_owned(),
            stale: row.endpoint.stale(),
            agent,
        })
    })
    .collect()
}

#[cfg(test)]
mod host_sources_tests {
    use super::host_sources;
    use crate::client::endpoint::ClientEndpointStatus;

    fn endpoint(
        status: ClientEndpointStatus,
        boot: Option<(&str, &str)>,
    ) -> super::ClientShellEndpoint {
        let mut endpoint = super::super::endpoints::local_endpoint();
        endpoint.status = status;
        endpoint.snapshot = boot.map(|(boot_id, workers)| {
            let mut snapshot = crate::client::shell::tests::snapshot();
            snapshot.boot_id = boot_id.into();
            snapshot.workspaces[0].tokens = vec![("agents_w".to_owned(), workers.to_owned())];
            Box::new(snapshot)
        });
        endpoint
    }

    fn header(endpoints: &[super::ClientShellEndpoint]) -> String {
        let host = super::super::agent_sidebar::host_counts(host_sources(endpoints));
        super::super::agent_sidebar::header_counts(0, &host.token_sets, host.expected)
    }

    // herdr-8u7 through the real endpoint mapping: disabled endpoints name no server, an offline
    // alias of an online server neither contributes its stale counts nor marks the total, and an
    // unreachable endpoint with no snapshot marks it.
    #[test]
    fn host_sources_maps_endpoint_status_into_the_count_model() {
        let online = endpoint(ClientEndpointStatus::Online, Some(("boot-1", "2")));
        let alias = endpoint(ClientEndpointStatus::Reconnecting, Some(("boot-1", "9")));
        let disabled = endpoint(ClientEndpointStatus::Disabled, Some(("boot-3", "5")));
        assert_eq!(header(&[alias, online, disabled]), "P:0 W:2");

        let online = endpoint(ClientEndpointStatus::Online, Some(("boot-1", "2")));
        let connecting = endpoint(ClientEndpointStatus::Connecting, None);
        assert_eq!(header(&[online, connecting]), "P:0 W:2?");
    }
}
