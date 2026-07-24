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

    let rows_height =
        (settings_rows(state).len() + 5 + device_presence_sections(state).len()) as u16;
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
    }
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
    let label_width = 48usize;
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

fn status_line(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<22}"), theme::dim()),
        Span::raw(value),
    ])
}

fn bytes_label(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{bytes} B ({:.1} MiB)", bytes as f64 / 1024.0 / 1024.0)
    } else if bytes >= 1024 {
        format!("{bytes} B ({:.1} KiB)", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(12).collect::<String>() + "…"
}

fn draw_status(frame: &mut Frame, area: Rect, state: &AppState) {
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
            lines.push(status_line("Node", "running".to_string()));
            lines.push(status_line("Network", status.network.clone()));
            lines.push(status_line("Endpoint ID", status.endpoint_id.clone()));
            lines.push(status_line("DHT node ID", status.dht_node_id.clone()));
            let peers = if status.connected_peers.is_empty() {
                "none yet".to_string()
            } else {
                let names: Vec<String> =
                    status.connected_peers.iter().map(|p| short_id(p)).collect();
                format!("{} — {}", status.connected_peers.len(), names.join(", "))
            };
            lines.push(status_line("Connected peers", peers));
            lines.push(status_line(
                "Known contacts",
                status.known_contacts.to_string(),
            ));
            lines.push(status_line(
                "Stored DHT records",
                status
                    .stored_dht_records
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "unavailable".to_string()),
            ));
            lines.push(status_line(
                "Stored DHT bytes",
                status
                    .stored_dht_bytes
                    .map(bytes_label)
                    .unwrap_or_else(|| "unavailable".to_string()),
            ));
            lines.push(status_line(
                "Published items",
                status.published_items.to_string(),
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
        }
    }
    lines.push(Line::default());
    lines.push(Line::styled("Connected Devices", theme::header()));
    match &state.federation.devices {
        None => lines.push(Line::styled("loading…", theme::dim())),
        Some(status) => {
            lines.push(status_line("This device", status.this_device_id.clone()));
            lines.push(status_line("Sync group", status.group_id.clone()));
            lines.push(status_line(
                "Active devices",
                status.active_devices.to_string(),
            ));
            lines.push(status_line(
                "Revoked devices",
                status.revoked_devices.to_string(),
            ));
            lines.push(status_line(
                "Pending requests",
                status.pending_requests.to_string(),
            ));
            lines.push(status_line("Ops in log", status.ops_total.to_string()));
            lines.push(status_line(
                "Tombstones",
                format!(
                    "{} ({} compactable)",
                    status.tombstone_ops, status.compactable_tombstones
                ),
            ));
            lines.push(status_line("Outbox ops", status.outbox_ops.to_string()));
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
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}
