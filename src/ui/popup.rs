use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use super::theme;
use crate::api::models::{ArtistRef, TrackItem};
use crate::app::state::{AppState, Loadable, Popup, addable_playlists};

pub fn draw(frame: &mut Frame, state: &AppState) {
    match state.popup.as_ref() {
        Some(Popup::AddToPlaylist { track, cursor }) => {
            draw_picker(frame, state, &track.title, *cursor)
        }
        Some(Popup::NewPlaylist { input, busy, .. }) => draw_name_entry(frame, input, *busy),
        Some(Popup::Devices { cursor }) => draw_devices(frame, state, *cursor),
        Some(Popup::TrackInfo {
            tracks,
            cursor,
            scroll,
        }) => draw_track_info(frame, tracks, *cursor, *scroll),
        Some(Popup::LogDetail(entry)) => draw_log_detail(frame, entry),
        None => {}
    }
}

fn draw_devices(frame: &mut Frame, state: &AppState, cursor: usize) {
    let rows = state.devices.devices.len().max(1);
    let height = (rows as u16 + 4)
        .min(frame.area().height.saturating_sub(2))
        .max(7);
    let area = centered(frame.area(), 64, height);
    let block = Block::bordered()
        .title(" Connected devices ")
        .title_style(theme::header())
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let [list_area, _, footer] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    if state.devices.devices.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled("waiting for device poll…", theme::dim()))
                .alignment(Alignment::Center),
            list_area,
        );
    } else {
        let visible = usize::from(list_area.height.max(1));
        let cursor = cursor.min(state.devices.devices.len() - 1);
        let first = cursor
            .saturating_sub(visible / 2)
            .min(state.devices.devices.len().saturating_sub(visible));
        for (index, device) in state
            .devices
            .devices
            .iter()
            .enumerate()
            .skip(first)
            .take(visible)
        {
            let row = Rect {
                x: list_area.x,
                y: list_area.y + (index - first) as u16,
                width: list_area.width,
                height: 1,
            };
            let marker = if device.is_active {
                Span::styled("▶ ", theme::accent())
            } else {
                Span::styled("  ", theme::dim())
            };
            let current = if device.is_current {
                " · this TUI"
            } else {
                ""
            };
            let switching = if state.devices.switching_to.as_deref() == Some(device.id.as_str()) {
                " · switching"
            } else {
                ""
            };
            let line = Line::from(vec![
                marker,
                Span::raw(device.name.clone()),
                Span::styled(
                    format!(" · {}{current}{switching}", device.kind),
                    theme::dim(),
                ),
            ]);
            frame.render_widget(Paragraph::new(line), row);
            if index == cursor {
                frame.buffer_mut().set_style(row, theme::tab_active());
            }
        }
    }

    let hint = if let Some(error) = &state.devices.poll_error {
        Line::styled(format!("sync error: {error}"), theme::dim())
    } else {
        Line::styled("enter make active · esc close", theme::dim())
    };
    frame.render_widget(Paragraph::new(hint).alignment(Alignment::Center), footer);
}

fn draw_log_detail(frame: &mut Frame, entry: &crate::config::logging::LogEntry) {
    let width = 90.min(frame.area().width.saturating_sub(4)).max(40);
    let height = 18.min(frame.area().height.saturating_sub(2)).max(7);
    let area = centered(frame.area(), width, height);

    let block = Block::bordered()
        .title(format!(" Log entry — {} {} ", entry.time, entry.level))
        .title_style(theme::header())
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let [target_area, body, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(Line::styled(entry.target.clone(), theme::dim())),
        target_area,
    );
    frame.render_widget(
        Paragraph::new(entry.message.clone()).wrap(ratatui::widgets::Wrap { trim: false }),
        body,
    );
    frame.render_widget(
        Paragraph::new(Line::styled("esc close", theme::dim())).alignment(Alignment::Center),
        footer,
    );
}

fn draw_track_info(frame: &mut Frame, tracks: &[TrackItem], cursor: usize, scroll: usize) {
    let Some(track) = tracks.get(cursor.min(tracks.len().saturating_sub(1))) else {
        return;
    };
    let width = 92.min(frame.area().width.saturating_sub(4)).max(48);
    let height = 24.min(frame.area().height.saturating_sub(2)).max(10);
    let area = centered(frame.area(), width, height);
    let title = if tracks.len() > 1 {
        format!(" Track info — {}/{} ", cursor + 1, tracks.len())
    } else {
        " Track info ".to_string()
    };

    let block = Block::bordered()
        .title(title)
        .title_style(theme::header())
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let lines = track_info_lines(track);
    let max_scroll = lines.len().saturating_sub(usize::from(body.height));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll.min(max_scroll) as u16, 0)),
        body,
    );

    let hint = if tracks.len() > 1 {
        "j/k scroll · h/left previous · l/right next · esc close"
    } else {
        "j/k scroll · esc close"
    };
    frame.render_widget(
        Paragraph::new(Line::styled(hint, theme::dim())).alignment(Alignment::Center),
        footer,
    );
}

fn track_info_lines(track: &TrackItem) -> Vec<Line<'static>> {
    vec![
        field("ID", track.id.to_string()),
        field("Title", track.title.clone()),
        field("Artists", artist_refs(&track.artists)),
        field("Featured artists", artist_refs(&track.featured_artists)),
        field("Release", release_label(track)),
        field("Release ID", track.release_id.to_string()),
        field("Disc", opt_display(track.disc_number)),
        field("Track number", opt_display(track.track_number)),
        field(
            "Duration",
            format!(
                "{} ({:.2}s)",
                track.duration_label(),
                track.duration_seconds
            ),
        ),
        field("Uploader", empty_dash(&track.uploader_name)),
        field("Audio format", opt_string(track.audio_format.clone())),
        field(
            "Bitrate",
            opt_map(track.audio_bitrate, |v| format!("{v} kbps")),
        ),
        field(
            "Sample rate",
            opt_map(track.audio_sample_rate, |v| {
                format!("{:.1} kHz", f64::from(v) / 1000.0)
            }),
        ),
        field(
            "Bit depth",
            opt_map(track.audio_bit_depth, |v| format!("{v} bit")),
        ),
        field("File size", file_size(track.file_size_bytes)),
        field("Last.fm listeners", opt_display(track.lastfm_listeners)),
        field("Last.fm plays", opt_display(track.lastfm_playcount)),
        field(
            "Last.fm rating",
            opt_map(track.lastfm_rating, |v| format!("{v:.3}")),
        ),
        field(
            "Last.fm updated",
            opt_string(track.lastfm_updated_at.clone()),
        ),
        field("Stream URL", empty_dash(&track.stream_url)),
        field("Cover URL", opt_string(track.cover_url.clone())),
    ]
}

fn field(label: &'static str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<18}"), theme::dim()),
        Span::raw(value),
    ])
}

fn artist_refs(items: &[ArtistRef]) -> String {
    if items.is_empty() {
        return "—".to_string();
    }
    items
        .iter()
        .map(|artist| format!("{} ({})", artist.name, artist.id))
        .collect::<Vec<_>>()
        .join(", ")
}

fn release_label(track: &TrackItem) -> String {
    let mut label = empty_dash(&track.release_title);
    if let Some(year) = track.release_year {
        label.push_str(&format!(" ({year})"));
    }
    label
}

fn empty_dash(value: &str) -> String {
    if value.trim().is_empty() {
        "—".to_string()
    } else {
        value.to_string()
    }
}

fn opt_string(value: Option<String>) -> String {
    value
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "—".to_string())
}

fn opt_display<T: std::fmt::Display>(value: Option<T>) -> String {
    opt_map(value, |value| value.to_string())
}

fn opt_map<T>(value: Option<T>, format: impl FnOnce(T) -> String) -> String {
    value.map(format).unwrap_or_else(|| "—".to_string())
}

fn file_size(value: Option<i64>) -> String {
    value
        .map(|bytes| format!("{:.1} MB ({} bytes)", bytes as f64 / 1_048_576.0, bytes))
        .unwrap_or_else(|| "—".to_string())
}

fn draw_picker(frame: &mut Frame, state: &AppState, track_title: &str, cursor: usize) {
    let options = addable_playlists(state);
    let loading = !matches!(&state.playlists.list, Some(Loadable::Ready(_)));
    let rows = options.len() + 1;
    let height = (rows as u16 + 4)
        .min(frame.area().height.saturating_sub(2))
        .max(6);
    let area = centered(frame.area(), 44, height);

    let block = Block::bordered()
        .title(" Add to playlist ")
        .title_style(theme::header())
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let [list_area, _, footer] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let mut lines: Vec<Line> = vec![Line::styled("+ New playlist…", theme::accent())];
    if loading {
        lines.push(Line::styled("loading playlists…", theme::dim()));
    } else if options.is_empty() {
        lines.push(Line::styled("no playlists yet", theme::dim()));
    } else {
        for (_, title) in &options {
            lines.push(Line::raw(title.clone()));
        }
    }
    let visible = usize::from(list_area.height.max(1));
    let first = cursor
        .saturating_sub(visible / 2)
        .min(lines.len().saturating_sub(visible));
    for (index, line) in lines.into_iter().enumerate().skip(first).take(visible) {
        let row = Rect {
            x: list_area.x,
            y: list_area.y + (index - first) as u16,
            width: list_area.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(line), row);
        if index == cursor {
            frame.buffer_mut().set_style(row, theme::tab_active());
        }
    }

    frame.render_widget(
        Paragraph::new(Line::styled(
            format!("♪ {track_title} · enter add · esc close"),
            theme::dim(),
        ))
        .alignment(Alignment::Center),
        footer,
    );
}

fn draw_name_entry(frame: &mut Frame, input: &str, busy: bool) {
    let area = centered(frame.area(), 44, 7);
    let block = Block::bordered()
        .title(" New playlist ")
        .title_style(theme::header())
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let [field, _, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let name_block = Block::bordered()
        .title("Name")
        .border_style(theme::accent());
    let name_inner = name_block.inner(field);
    frame.render_widget(name_block, field);
    let width = usize::from(name_inner.width.saturating_sub(1));
    let mut shown: String = input
        .chars()
        .skip(input.chars().count().saturating_sub(width))
        .collect();
    shown.push('█');
    frame.render_widget(Paragraph::new(shown), name_inner);

    let hint = if busy {
        Line::styled("creating…", theme::accent())
    } else {
        Line::styled("enter create · esc back", theme::dim())
    };
    frame.render_widget(Paragraph::new(hint).alignment(Alignment::Center), footer);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [rect] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [rect] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(rect);
    rect
}
