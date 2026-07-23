//! The Settings tab: federation settings, visualization scripts and status.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::theme;
use crate::app::state::{AppState, FedRow};

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::bordered()
        .title(" Settings ")
        .title_style(theme::header())
        .border_style(theme::dim());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows_height = (FedRow::ALL.len() + state.visualizer.scripts.len() + 6) as u16;
    let [rows_area, _, status_area] = Layout::vertical([
        Constraint::Length(rows_height.min(inner.height)),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);

    draw_settings_rows(frame, rows_area, state);
    draw_status(frame, status_area, state);
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

fn draw_row(
    frame: &mut Frame,
    area: Rect,
    y: &mut u16,
    row_index: usize,
    cursor: usize,
    label: &str,
    value: String,
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
        Span::styled(marker, theme::accent()),
        Span::styled(
            format!("{label:<label_width$}"),
            if selected {
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
    frame.render_widget(Paragraph::new(lines), area);
}
