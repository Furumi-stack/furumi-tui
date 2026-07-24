//! The Settings tab: federation settings, visualization scripts and status.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::theme;
use crate::app::state::{AppState, DevicePresenceSection, FedRow, settings_rows};

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::bordered()
        .title(" Settings ")
        .title_style(theme::header())
        .border_style(theme::dim());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width >= 132 {
        let desired_settings_width = ((inner.width as usize * 44) / 100).clamp(68, 92) as u16;
        let settings_width = desired_settings_width.min(inner.width.saturating_sub(56));
        let [rows_area, _, status_area] = Layout::horizontal([
            Constraint::Length(settings_width),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .areas(inner);

        draw_settings_rows(frame, rows_area, state);
        draw_status(frame, status_area, state);
        return;
    }

    let rows_height =
        (settings_rows(state).len() + 6 + device_presence_sections(state).len()) as u16;
    let [rows_area, _, status_area] = Layout::vertical([
        Constraint::Length(rows_height.min(inner.height)),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);

    draw_settings_rows(frame, rows_area, state);
    draw_status(frame, status_area, state);
}

fn device_presence_sections(state: &AppState) -> Vec<DevicePresenceSection> {
    let Some(status) = &state.federation.devices else {
        return Vec::new();
    };
    let now = crate::app::state::unix_time_ms();
    let mut sections = Vec::new();
    for index in crate::app::state::device_status_order(state) {
        let Some(device) = status.devices.get(index) else {
            continue;
        };
        let section = crate::app::state::device_presence_section(state, device, now);
        if sections.last().copied() != Some(section) {
            sections.push(section);
        }
    }
    sections
}

fn draw_settings_rows(frame: &mut Frame, area: Rect, state: &AppState) {
    let settings = &state.federation.settings;
    let on_off = |on: bool| if on { "on" } else { "off" };
    let mut y = area.y;
    let mut cursor = 0usize;

    draw_section(frame, area, &mut y, "Federation");
    for row in FedRow::ALL {
        let (label, value) = match row {
            FedRow::Toggle => ("Federation", on_off(settings.enabled).to_string()),
            FedRow::NetworkId => (
                "Network ID (shared secret)",
                if settings.network_id.is_empty() {
                    "(not set — press enter)".to_string()
                } else {
                    settings.network_id.clone()
                },
            ),
            FedRow::SaveOnListen => (
                "Save federated tracks to the library on listen",
                on_off(settings.save_on_listen).to_string(),
            ),
            FedRow::SyncNow => ("Publish the library now", "↵".to_string()),
            FedRow::ShowTicket => ("Show my connection ticket", "↵".to_string()),
            FedRow::Connect => ("Connect to a peer by ticket…", "↵".to_string()),
        };
        draw_row(
            frame,
            area,
            &mut y,
            cursor,
            state.settings_cursor,
            label,
            value,
        );
        cursor += 1;
    }

    y = y.saturating_add(1);
    let connected_devices_enabled = state.connected_devices_enabled();
    draw_section(frame, area, &mut y, "Connected Devices");
    let disabled_value = "enable federation first".to_string();
    let devices = state.federation.devices.as_ref();
    draw_row_enabled(
        frame,
        area,
        &mut y,
        cursor,
        state.settings_cursor,
        "This device name",
        if connected_devices_enabled {
            devices
                .map(|status| status.this_device_name.clone())
                .unwrap_or_else(|| "loading…".to_string())
        } else {
            disabled_value.clone()
        },
        connected_devices_enabled,
    );
    cursor += 1;
    draw_row_enabled(
        frame,
        area,
        &mut y,
        cursor,
        state.settings_cursor,
        "Generate device invite",
        if connected_devices_enabled {
            "↵".to_string()
        } else {
            disabled_value.clone()
        },
        connected_devices_enabled,
    );
    cursor += 1;
    draw_row_enabled(
        frame,
        area,
        &mut y,
        cursor,
        state.settings_cursor,
        "Connect device by invite…",
        if connected_devices_enabled {
            "↵".to_string()
        } else {
            disabled_value.clone()
        },
        connected_devices_enabled,
    );
    cursor += 1;
    draw_row_enabled(
        frame,
        area,
        &mut y,
        cursor,
        state.settings_cursor,
        "Sync devices now",
        if connected_devices_enabled {
            "↵".to_string()
        } else {
            disabled_value.clone()
        },
        connected_devices_enabled,
    );
    cursor += 1;
    if let Some(status) = devices {
        let now = crate::app::state::unix_time_ms();
        let mut current_section = None;
        for index in crate::app::state::device_status_order(state) {
            let Some(device) = status.devices.get(index) else {
                continue;
            };
            let section = crate::app::state::device_presence_section(state, device, now);
            if current_section != Some(section) {
                draw_subsection(frame, area, &mut y, section.title());
                current_section = Some(section);
            }
            let name = crate::app::state::device_display_name(device);
            let label = if device.is_self {
                format!("* {name}")
            } else if device.revoked {
                format!("  {name} (revoked)")
            } else {
                format!("  {name}")
            };
            let version = if device.client_version.is_empty() {
                "unknown".to_string()
            } else {
                format!("v{}", device.client_version)
            };
            let can_revoke = connected_devices_enabled && !device.is_self && !device.revoked;
            let presence = match section {
                DevicePresenceSection::Online => "online",
                DevicePresenceSection::Offline => "offline",
                DevicePresenceSection::Revoked => "revoked",
            };
            let value = if can_revoke {
                format!("{version} · {presence} · revoke ↵")
            } else if connected_devices_enabled {
                format!("{version} · {presence}")
            } else {
                disabled_value.clone()
            };
            draw_row_enabled(
                frame,
                area,
                &mut y,
                cursor,
                state.settings_cursor,
                &label,
                value,
                connected_devices_enabled,
            );
            cursor += 1;
        }
    }

    y = y.saturating_add(1);
    draw_section(frame, area, &mut y, "Visualizations");
    draw_row(
        frame,
        area,
        &mut y,
        cursor,
        state.settings_cursor,
        "Show clock",
        if state.visualizer.config.show_clock {
            "[x]".to_string()
        } else {
            "[ ]".to_string()
        },
    );
    cursor += 1;

    for (index, script) in state.visualizer.scripts.iter().enumerate() {
        let selected_script = state
            .visualizer
            .selected_script_index()
            .is_some_and(|selected| selected == index);
        let label = if selected_script {
            format!("* {}", script.name)
        } else {
            format!("  {}", script.name)
        };
        let value = script
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_string();
        draw_row(
            frame,
            area,
            &mut y,
            cursor,
            state.settings_cursor,
            &label,
            value,
        );
        cursor += 1;
    }

    draw_row(
        frame,
        area,
        &mut y,
        cursor,
        state.settings_cursor,
        "+ New visualization script",
        "↵".to_string(),
    );
    cursor += 1;

    if state.visualizer.selected_script().is_some() {
        draw_row(
            frame,
            area,
            &mut y,
            cursor,
            state.settings_cursor,
            "Edit selected visualization",
            "↵".to_string(),
        );
        cursor += 1;
    }

    y = y.saturating_add(1);
    draw_row(
        frame,
        area,
        &mut y,
        cursor,
        state.settings_cursor,
        "Full status details",
        "enter".to_string(),
    );
}

fn draw_section(frame: &mut Frame, area: Rect, y: &mut u16, title: &'static str) {
    if *y >= area.y + area.height {
        return;
    }
    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height: 1,
    };
    frame.render_widget(Paragraph::new(Line::styled(title, theme::header())), rect);
    *y = (*y).saturating_add(1);
}

fn draw_subsection(frame: &mut Frame, area: Rect, y: &mut u16, title: &'static str) {
    if *y >= area.y + area.height {
        return;
    }
    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(title, theme::dim()),
        ])),
        rect,
    );
    *y = (*y).saturating_add(1);
}

fn draw_row(
    frame: &mut Frame,
    area: Rect,
    y: &mut u16,
    row_index: usize,
    cursor: usize,
    label: &str,
    value: String,
) {
    draw_row_enabled(frame, area, y, row_index, cursor, label, value, true);
}

fn draw_row_enabled(
    frame: &mut Frame,
    area: Rect,
    y: &mut u16,
    row_index: usize,
    cursor: usize,
    label: &str,
    value: String,
    enabled: bool,
) {
    if *y >= area.y + area.height {
        return;
    }
    let selected = row_index == cursor;
    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height: 1,
    };
    let marker = if selected { "▶ " } else { "  " };
    let label_width = settings_label_width(area.width);
    let line = Line::from(vec![
        Span::styled(
            marker,
            if enabled {
                theme::accent()
            } else {
                theme::dim()
            },
        ),
        Span::styled(
            format!("{label:<label_width$}"),
            if !enabled {
                theme::dim()
            } else if selected {
                theme::accent()
            } else {
                ratatui::style::Style::default()
            },
        ),
        Span::styled(value, theme::dim()),
    ]);
    frame.render_widget(Paragraph::new(line), rect);
    *y = (*y).saturating_add(1);
}

fn settings_label_width(width: u16) -> usize {
    let width = width as usize;
    if width >= 88 {
        48
    } else {
        width.saturating_sub(28).clamp(24, 48)
    }
}

fn status_line(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<18}"), theme::dim()),
        Span::raw(value),
    ])
}

fn short_bytes_label(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / 1024.0 / 1024.0)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn rtt_label(ms: Option<u64>) -> String {
    ms.map(|ms| format!("{ms} ms"))
        .unwrap_or_else(|| "rtt n/a".to_string())
}

fn short_id(id: &str) -> String {
    id.chars().take(12).collect::<String>() + "…"
}

fn push_transport_status(lines: &mut Vec<Line<'static>>, status: &crate::federation::FedStatus) {
    lines.push(Line::default());
    lines.push(Line::styled("Iroh Transport", theme::header()));
    let transport = &status.transport;
    if transport.total_samples == 0 {
        lines.push(status_line("Streams", "no samples yet".to_string()));
        return;
    }
    let runtime_total = transport
        .runtime_tx_bytes
        .saturating_add(transport.runtime_rx_bytes);
    lines.push(status_line(
        "Runtime traffic",
        format!(
            "{} · tx {} · rx {} · active {}",
            short_bytes_label(runtime_total),
            short_bytes_label(transport.runtime_tx_bytes),
            short_bytes_label(transport.runtime_rx_bytes),
            transport.active_streams
        ),
    ));
    if transport.runtime_lost_packets > 0 || transport.runtime_lost_bytes > 0 {
        lines.push(status_line(
            "Runtime loss",
            format!(
                "{} pkts · {}",
                transport.runtime_lost_packets,
                short_bytes_label(transport.runtime_lost_bytes)
            ),
        ));
    }
    lines.push(status_line(
        "Samples",
        format!(
            "{} total · direct {} · relay {} · custom {} · unknown {}",
            transport.total_samples,
            transport.direct_samples,
            transport.relay_samples,
            transport.custom_samples,
            transport.unknown_samples
        ),
    ));
    lines.push(status_line(
        "Protocols",
        format!(
            "audio {} · catalog {} · sync {}",
            transport.audio_samples, transport.catalog_samples, transport.sync_samples
        ),
    ));
    if let Some(sample) = transport.last.first() {
        lines.push(status_line(
            "Last stream",
            format!(
                "{} {} {} · {} · {}",
                sample.protocol,
                sample.direction,
                sample.phase,
                sample.selected_path,
                rtt_label(sample.selected_rtt_ms)
            ),
        ));
        lines.push(status_line(
            "Last peer",
            format!(
                "{} · paths d/r/c/open {}/{}/{}/{}",
                sample.peer_id,
                sample.direct_paths,
                sample.relay_paths,
                sample.custom_paths,
                sample.open_paths
            ),
        ));
        lines.push(status_line(
            "Last bytes",
            format!(
                "sel {}/{} · total {}/{} · lost {} / {}",
                short_bytes_label(sample.selected_tx_bytes),
                short_bytes_label(sample.selected_rx_bytes),
                short_bytes_label(sample.total_tx_bytes),
                short_bytes_label(sample.total_rx_bytes),
                sample.lost_packets,
                short_bytes_label(sample.lost_bytes)
            ),
        ));
    }
    for sample in &transport.last {
        lines.push(Line::from(vec![
            Span::styled(format!("{:<14}", sample.at), theme::dim()),
            Span::raw(format!(
                "{} {} {} · {} · tx {} rx {}",
                sample.protocol,
                sample.direction,
                sample.phase,
                sample.selected_path,
                short_bytes_label(sample.total_tx_bytes),
                short_bytes_label(sample.total_rx_bytes)
            )),
        ]));
    }
}

fn draw_status(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    if area.height < 18 || area.width < 36 {
        frame.render_widget(
            Paragraph::new(compact_status_lines(state))
                .wrap(ratatui::widgets::Wrap { trim: false }),
            area,
        );
        return;
    }

    let [node_area, _, transport_area, _, devices_area, _] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(1),
        Constraint::Length(5),
        Constraint::Length(1),
        Constraint::Length(5),
        Constraint::Min(0),
    ])
    .areas(area);

    draw_summary_card(frame, node_area, " Status ", node_summary_lines(state));
    draw_summary_card(
        frame,
        transport_area,
        " Iroh Transport ",
        transport_summary_lines(state),
    );
    draw_summary_card(
        frame,
        devices_area,
        " Connected Devices ",
        device_summary_lines(state),
    );
}

fn compact_status_lines(state: &AppState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::styled("Status", theme::header()));
    lines.extend(node_summary_lines(state).into_iter().take(2));
    lines.push(Line::default());
    lines.push(Line::styled("Iroh Transport", theme::header()));
    lines.extend(transport_summary_lines(state).into_iter().take(2));
    lines.push(Line::default());
    lines.push(Line::styled("Connected Devices", theme::header()));
    lines.extend(device_summary_lines(state).into_iter().take(2));
    lines
}

fn draw_summary_card(
    frame: &mut Frame,
    area: Rect,
    title: &'static str,
    lines: Vec<Line<'static>>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let block = Block::bordered()
        .title(title)
        .title_style(theme::header())
        .border_style(theme::dim());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn summary_line(label: &'static str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), theme::dim()),
        Span::raw(value),
    ])
}

fn node_summary_lines(state: &AppState) -> Vec<Line<'static>> {
    match &state.federation.status {
        None => vec![
            summary_line("Node", "loading".to_string()),
            summary_line("Network", "unknown".to_string()),
            summary_line("Peers", "waiting for status".to_string()),
        ],
        Some(status) if !status.running => {
            let network = if state.federation.settings.network_id.trim().is_empty() {
                "network id not set".to_string()
            } else {
                state.federation.settings.network_id.clone()
            };
            vec![
                summary_line("Node", "stopped".to_string()),
                summary_line("Network", network),
                summary_line(
                    "Problem",
                    status
                        .last_error
                        .as_deref()
                        .map(first_line)
                        .unwrap_or_else(|| "disabled".to_string()),
                ),
            ]
        }
        Some(status) => vec![
            summary_line("Node", format!("running on {}", status.network)),
            summary_line(
                "Peers",
                format!(
                    "{} connected / {} contacts",
                    status.connected_peers.len(),
                    status.known_contacts
                ),
            ),
            summary_line(
                "Library",
                format!(
                    "{} published / {} DHT records",
                    status.published_items,
                    status
                        .stored_dht_records
                        .map(|count| count.to_string())
                        .unwrap_or_else(|| "n/a".to_string())
                ),
            ),
        ],
    }
}

fn transport_summary_lines(state: &AppState) -> Vec<Line<'static>> {
    let Some(status) = &state.federation.status else {
        return vec![
            summary_line("Traffic", "loading".to_string()),
            summary_line("Streams", "loading".to_string()),
            summary_line("Path", "loading".to_string()),
        ];
    };
    let transport = &status.transport;
    if transport.total_samples == 0 {
        return vec![
            summary_line("Traffic", "no samples yet".to_string()),
            summary_line("Streams", format!("{} active", transport.active_streams)),
            summary_line("Path", "waiting for a stream".to_string()),
        ];
    }
    let runtime_total = transport
        .runtime_tx_bytes
        .saturating_add(transport.runtime_rx_bytes);
    let last_path = transport
        .last
        .first()
        .map(|sample| {
            format!(
                "{} / {} / {}",
                sample.protocol,
                sample.selected_path,
                rtt_label(sample.selected_rtt_ms)
            )
        })
        .unwrap_or_else(|| "no recent stream".to_string());
    vec![
        summary_line(
            "Traffic",
            format!(
                "{} (tx {} / rx {})",
                short_bytes_label(runtime_total),
                short_bytes_label(transport.runtime_tx_bytes),
                short_bytes_label(transport.runtime_rx_bytes)
            ),
        ),
        summary_line(
            "Streams",
            format!(
                "{} active / {} samples",
                transport.active_streams, transport.total_samples
            ),
        ),
        summary_line(
            "Paths",
            format!(
                "direct {} / relay {} / custom {}",
                transport.direct_samples, transport.relay_samples, transport.custom_samples
            ),
        ),
        summary_line("Last", last_path),
    ]
}

fn device_summary_lines(state: &AppState) -> Vec<Line<'static>> {
    if !state.connected_devices_enabled() {
        return vec![
            summary_line("Sync", "disabled".to_string()),
            summary_line("This", state.device_playback.self_device_name.clone()),
            summary_line("Devices", "enable federation first".to_string()),
        ];
    }
    let Some(status) = &state.federation.devices else {
        return vec![
            summary_line("Sync", "loading".to_string()),
            summary_line("This", state.device_playback.self_device_name.clone()),
            summary_line("Devices", "waiting for device status".to_string()),
        ];
    };
    let (online, offline, revoked) = device_presence_counts(state);
    let this_name = if status.this_device_name.trim().is_empty() {
        short_id(&status.this_device_id)
    } else {
        format!(
            "{} / {}",
            status.this_device_name,
            short_id(&status.this_device_id)
        )
    };
    vec![
        summary_line("This", this_name),
        summary_line(
            "Devices",
            format!(
                "{} active / {} online / {} pending",
                status.active_devices, online, status.pending_requests
            ),
        ),
        summary_line(
            "Sync",
            format!(
                "{} outbox / last {}",
                status.outbox_ops,
                status
                    .last_sync
                    .clone()
                    .unwrap_or_else(|| "not yet".to_string())
            ),
        ),
        summary_line(
            "Other",
            format!("{} offline / {} revoked", offline, revoked),
        ),
    ]
}

fn device_presence_counts(state: &AppState) -> (usize, usize, usize) {
    let Some(status) = &state.federation.devices else {
        return (0, 0, 0);
    };
    let now = crate::app::state::unix_time_ms();
    let mut online = 0usize;
    let mut offline = 0usize;
    let mut revoked = 0usize;
    for device in &status.devices {
        match crate::app::state::device_presence_section(state, device, now) {
            DevicePresenceSection::Online => online += 1,
            DevicePresenceSection::Offline => offline += 1,
            DevicePresenceSection::Revoked => revoked += 1,
        }
    }
    (online, offline, revoked)
}

fn first_line(value: &str) -> String {
    value.lines().next().unwrap_or(value).to_string()
}

pub(super) fn status_detail_lines(state: &AppState) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = vec![Line::styled("Status", theme::header())];
    match &state.federation.status {
        None => lines.push(Line::styled("loading…", theme::dim())),
        Some(status) if !status.running => {
            lines.push(status_line("Node", "stopped".to_string()));
            if let Some(error) = &status.last_error {
                lines.push(status_line("Error", error.clone()));
            }
            lines.push(Line::default());
            lines.push(Line::styled(
                "Enable federation and set a network id — every instance using the",
                theme::dim(),
            ));
            lines.push(Line::styled(
                "same id (furumi TUI or furumi-fd) finds the others automatically.",
                theme::dim(),
            ));
        }
        Some(status) => {
            lines.push(status_line("Node", format!("running · {}", status.network)));
            lines.push(status_line(
                "Endpoint",
                format!("{} · dht {}", status.endpoint_id, status.dht_node_id),
            ));
            let peers = if status.connected_peers.is_empty() {
                format!("none · contacts {}", status.known_contacts)
            } else {
                let names: Vec<String> = status.connected_peers.iter().map(String::clone).collect();
                let more = status.connected_peers.len().saturating_sub(names.len());
                let more = if more > 0 {
                    format!(" +{more}")
                } else {
                    String::new()
                };
                format!(
                    "{} connected{} · contacts {} · {}",
                    status.connected_peers.len(),
                    more,
                    status.known_contacts,
                    names.join(", ")
                )
            };
            lines.push(status_line("Peers", peers));
            lines.push(status_line(
                "DHT",
                format!(
                    "{} records · {} · {} published",
                    status
                        .stored_dht_records
                        .map(|count| count.to_string())
                        .unwrap_or_else(|| "unavailable".to_string()),
                    status
                        .stored_dht_bytes
                        .map(short_bytes_label)
                        .unwrap_or_else(|| "unavailable".to_string()),
                    status.published_items
                ),
            ));
            lines.push(status_line(
                "Last sync",
                status
                    .last_sync
                    .clone()
                    .unwrap_or_else(|| "not yet".to_string()),
            ));
            if let Some(error) = &status.last_error {
                lines.push(status_line("Error", error.clone()));
            }
            push_transport_status(&mut lines, status);
        }
    }
    lines.push(Line::default());
    lines.push(Line::styled("Connected Devices", theme::header()));
    match &state.federation.devices {
        None => lines.push(Line::styled("loading…", theme::dim())),
        Some(status) => {
            lines.push(status_line(
                "This device",
                format!("{} · {}", status.this_device_name, status.this_device_id),
            ));
            lines.push(status_line("Sync group", status.group_id.clone()));
            lines.push(status_line(
                "Devices",
                format!(
                    "{} active · {} revoked · {} pending",
                    status.active_devices, status.revoked_devices, status.pending_requests
                ),
            ));
            lines.push(status_line(
                "Sync log",
                format!(
                    "{} ops · {} outbox · {} tombstones ({} gc)",
                    status.ops_total,
                    status.outbox_ops,
                    status.tombstone_ops,
                    status.compactable_tombstones
                ),
            ));
            lines.push(status_line(
                "Snapshot",
                format!(
                    "{} likes, {} playlists, {} items",
                    status.snapshot_likes, status.snapshot_playlists, status.snapshot_items
                ),
            ));
            lines.push(status_line(
                "Unresolved items",
                status.unresolved_playlist_items.to_string(),
            ));
            lines.push(status_line("Peer ack floor", status.peer_ack_floor.clone()));
            if let Some(last_sync) = &status.last_sync {
                lines.push(status_line("Last device sync", last_sync.clone()));
            }
            if let Some(last_error) = &status.last_error {
                lines.push(status_line("Device error", last_error.clone()));
            }
            lines.push(Line::default());
            lines.push(Line::styled("Device List", theme::header()));
            if status.devices.is_empty() {
                lines.push(status_line("Devices", "none recorded".to_string()));
            } else {
                let ordered = crate::app::state::device_status_order(state);
                let now = crate::app::state::unix_time_ms();
                for index in &ordered {
                    if let Some(device) = status.devices.get(*index) {
                        push_device_detail(&mut lines, state, device, now);
                    }
                }
                for (index, device) in status.devices.iter().enumerate() {
                    if !ordered.contains(&index) {
                        push_device_detail(&mut lines, state, device, now);
                    }
                }
            }
        }
    }
    lines
}

fn push_device_detail(
    lines: &mut Vec<Line<'static>>,
    state: &AppState,
    device: &crate::devices::DeviceStatusRow,
    now_ms: i64,
) {
    let mut name = crate::app::state::device_display_name(device);
    if device.is_self {
        name.push_str(" (this device)");
    }
    if device.revoked {
        name.push_str(" (revoked)");
    }
    let presence = match crate::app::state::device_presence_section(state, device, now_ms) {
        DevicePresenceSection::Online => "online",
        DevicePresenceSection::Offline => "offline",
        DevicePresenceSection::Revoked => "revoked",
    };
    lines.push(status_line("Device", name));
    lines.push(status_line("Device ID", device.device_id.clone()));
    lines.push(status_line(
        "Endpoint ID",
        if device.endpoint_id.trim().is_empty() {
            "unavailable".to_string()
        } else {
            device.endpoint_id.clone()
        },
    ));
    lines.push(status_line(
        "Version",
        if device.client_version.trim().is_empty() {
            "unknown".to_string()
        } else {
            device.client_version.clone()
        },
    ));
    lines.push(status_line(
        "Presence",
        match device.last_seen_ms {
            Some(seen) => format!("{presence} · last seen {seen} ms"),
            None => format!("{presence} · last seen unavailable"),
        },
    ));
}
