use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::{theme, track_row};
use crate::app::state::{AppState, Loadable};
use crate::app::update::playlist_tracks;

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState) {
    match state.playlists.opened {
        Some(opened) => draw_opened(frame, area, state, opened.id, opened.cursor),
        None => draw_list(frame, area, state),
    }
}

fn bordered(frame: &mut Frame, area: Rect, title: String) -> Rect {
    let block = Block::bordered()
        .title(title)
        .title_style(theme::header())
        .border_style(theme::dim());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

fn centered_line(frame: &mut Frame, area: Rect, line: Line) {
    if area.height == 0 {
        return;
    }
    let middle = Rect { y: area.y + area.height / 2, height: 1, ..area };
    frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), middle);
}

fn draw_list(frame: &mut Frame, area: Rect, state: &AppState) {
    let inner = bordered(frame, area, " Playlists ".to_string());
    let selected = state.playlists.selected;

    let list = match &state.playlists.list {
        Some(Loadable::Ready(list)) => list,
        Some(Loadable::Failed(error)) => {
            return centered_line(
                frame,
                inner,
                Line::styled(error.clone(), Style::new().fg(Color::Red)),
            );
        }
        _ => {
            return centered_line(frame, inner, Line::styled("loading playlists…", theme::dim()));
        }
    };
    if list.is_empty() {
        return centered_line(frame, inner, Line::styled("no playlists yet", theme::dim()));
    }

    let visible = usize::from(inner.height.max(1));
    let first = selected
        .saturating_sub(visible / 2)
        .min(list.len().saturating_sub(visible));
    for (index, playlist) in list.iter().enumerate().skip(first).take(visible) {
        let row = Rect {
            x: inner.x,
            y: inner.y + (index - first) as u16,
            width: inner.width,
            height: 1,
        };
        let marker = if playlist.kind == "likes" {
            Span::styled("♥ ", theme::accent())
        } else {
            Span::raw("  ")
        };
        let mut flags = Vec::new();
        if !playlist.is_own {
            if let Some(owner) = &playlist.owner_name {
                flags.push(format!("by {owner}"));
            }
        }
        if playlist.is_public {
            flags.push("public".to_string());
        }
        let suffix = if flags.is_empty() {
            String::new()
        } else {
            format!("  {}", flags.join(" · "))
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                marker,
                Span::raw(playlist.title.clone()),
                Span::styled(suffix, theme::dim()),
            ])),
            row,
        );
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("{} trk", playlist.track_count),
                theme::dim(),
            ))
            .alignment(Alignment::Right),
            row,
        );
        if index == selected {
            frame.buffer_mut().set_style(row, theme::tab_active());
        }
    }
}

fn draw_opened(frame: &mut Frame, area: Rect, state: &AppState, id: i64, cursor: usize) {
    let loadable = state.playlist_views.get(&id);
    let title = match loadable {
        Some(Loadable::Ready(detail)) => format!(" Playlists ▸ {} ", detail.title),
        _ => " Playlists ▸ … ".to_string(),
    };
    let inner = bordered(frame, area, title);

    if let Some(Loadable::Failed(error)) = loadable {
        return centered_line(
            frame,
            inner,
            Line::styled(error.clone(), Style::new().fg(Color::Red)),
        );
    }
    let Some(tracks) = playlist_tracks(state, id) else {
        return centered_line(frame, inner, Line::styled("loading…", theme::dim()));
    };
    if tracks.is_empty() {
        return centered_line(frame, inner, Line::styled("no tracks here yet", theme::dim()));
    }

    let visible = usize::from(inner.height.max(1));
    let first = cursor
        .saturating_sub(visible / 2)
        .min(tracks.len().saturating_sub(visible));
    for (index, track) in tracks.iter().enumerate().skip(first).take(visible) {
        let row = Rect {
            x: inner.x,
            y: inner.y + (index - first) as u16,
            width: inner.width,
            height: 1,
        };
        track_row(frame, row, state, track, (index + 1).to_string(), index == cursor);
    }
}
