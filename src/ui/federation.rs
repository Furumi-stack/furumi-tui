//! The Federation tab: settings rows on top, a live status block below.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::theme;
use crate::app::state::{AppState, FedRow};

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::bordered()
        .title(" Federation ")
        .title_style(theme::header())
        .border_style(theme::dim());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows_height = FedRow::ALL.len() as u16;
    let [rows_area, _, status_area] = Layout::vertical([
        Constraint::Length(rows_height),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);

    let settings = &state.federation.settings;
    let on_off = |on: bool| if on { "on" } else { "off" };
    for (index, row) in FedRow::ALL.iter().enumerate() {
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
        let selected = index == state.federation.cursor;
        let rect = Rect {
            x: rows_area.x,
            y: rows_area.y + index as u16,
            width: rows_area.width,
            height: 1,
        };
        if rect.y >= rows_area.y + rows_area.height {
            break;
        }
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
    }

    draw_status(frame, status_area, state);
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
