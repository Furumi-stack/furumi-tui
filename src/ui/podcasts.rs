//! Podcasts tab: subscriptions with their cover, and the episode catalog of
//! one podcast with title search, date sort and the selected episode's notes.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use super::{loading_line, theme};
use crate::app::state::{
    ART_HEADER_HEIGHT, ART_HEADER_WIDTH, AppState, ArtState, Loadable, PodcastListRow,
};
use crate::podcasts::{Episode, Podcast};

/// Width below which the side panel with the cover is dropped.
const SIDE_PANEL_MIN_TOTAL_WIDTH: u16 = 72;
const SIDE_PANEL_WIDTH: u16 = ART_HEADER_WIDTH + 2;

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState) {
    match state.podcasts.opened {
        Some(opened) => draw_episodes(frame, area, state, opened.id, opened.cursor),
        None => draw_list(frame, area, state),
    }
}

fn bordered(frame: &mut Frame, area: Rect, state: &AppState, title: String) -> Rect {
    let block = Block::bordered()
        .title(title)
        .title_style(theme::header_for(state))
        .border_style(theme::border_for(state));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

fn centered_line(frame: &mut Frame, area: Rect, line: Line) {
    if area.height == 0 {
        return;
    }
    let middle = Rect {
        y: area.y + area.height / 2,
        height: 1,
        ..area
    };
    frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), middle);
}

fn error_style() -> Style {
    Style::new().fg(Color::Red)
}

fn draw_list(frame: &mut Frame, area: Rect, state: &AppState) {
    let tab = &state.podcasts;
    let inner = bordered(frame, area, state, " Podcasts ".to_string());
    match &tab.list {
        Some(Loadable::Failed(error)) => {
            return centered_line(frame, inner, Line::styled(error.clone(), error_style()));
        }
        None | Some(Loadable::Loading) => {
            return centered_line(frame, inner, loading_line(state, "loading podcasts…"));
        }
        Some(Loadable::Ready(_)) => {}
    }

    let (list_area, side_area) = split_side_panel(inner);
    if let Some(side) = side_area {
        draw_side_panel(frame, side, state, tab.current_podcast());
    }

    let rows = tab.list_rows();
    let visible = usize::from(list_area.height.max(1));
    let first = tab
        .selected
        .saturating_sub(visible / 2)
        .min(rows.len().saturating_sub(visible));
    for (index, row) in rows.iter().enumerate().skip(first).take(visible) {
        let area = Rect {
            x: list_area.x,
            y: list_area.y + (index - first) as u16,
            width: list_area.width,
            height: 1,
        };
        let (left, right) = list_row_lines(state, *row, list_area.width);
        frame.render_widget(Paragraph::new(left), area);
        if let Some(right) = right {
            frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
        }
        if index == tab.selected {
            frame
                .buffer_mut()
                .set_style(area, theme::tab_active_for(state));
        }
    }
    if tab.podcasts().is_empty() {
        let hint_y = list_area.y + rows.len() as u16 + 1;
        if hint_y < list_area.y + list_area.height {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    "No subscriptions yet. Choose \"Add podcast\" and paste an RSS feed URL.",
                    theme::dim(),
                ))
                .wrap(Wrap { trim: true }),
                Rect {
                    y: hint_y,
                    height: list_area.y + list_area.height - hint_y,
                    ..list_area
                },
            );
        }
    }
}

fn list_row_lines(
    state: &AppState,
    row: PodcastListRow,
    width: u16,
) -> (Line<'static>, Option<Line<'static>>) {
    let tab = &state.podcasts;
    match row {
        PodcastListRow::Add => {
            let label = if tab.subscribing {
                format!("{} Adding podcast…", state.spinner())
            } else {
                "+ Add podcast (RSS feed URL)…".to_string()
            };
            (Line::styled(label, theme::accent_for(state)), None)
        }
        PodcastListRow::RefreshAll => {
            let busy = !tab.refreshing.is_empty();
            let label = if busy {
                format!(
                    "{} Checking {} feed(s) for new episodes…",
                    state.spinner(),
                    tab.refreshing.len()
                )
            } else {
                "↻ Check all podcasts for new episodes".to_string()
            };
            (Line::styled(label, theme::accent_for(state)), None)
        }
        PodcastListRow::Podcast(index) => {
            let Some(podcast) = tab.podcasts().get(index) else {
                return (Line::raw(""), None);
            };
            let marker = if tab.refreshing.contains(&podcast.id) {
                Span::styled(format!("{} ", state.spinner()), theme::accent_for(state))
            } else if podcast.last_error.is_some() {
                Span::styled("! ", error_style())
            } else {
                Span::raw("  ")
            };
            let mut meta = format!("{} ep", podcast.episode_count);
            if let Some(latest) = podcast.latest_published_at {
                meta.push_str(&format!(" · {}", format_date(latest)));
            }
            meta.push(' ');
            // Marker (2), the title, one gap column, then the meta.
            let title_width =
                usize::from(width).saturating_sub(2 + UnicodeWidthStr::width(meta.as_str()) + 1);
            let left = Line::from(vec![
                marker,
                Span::raw(fit_width(&podcast.title, title_width)),
            ]);
            (left, Some(Line::styled(meta, theme::dim())))
        }
    }
}

/// Main area plus an optional right-hand panel for the cover and details.
fn split_side_panel(area: Rect) -> (Rect, Option<Rect>) {
    if area.width < SIDE_PANEL_MIN_TOTAL_WIDTH {
        return (area, None);
    }
    let [main, side] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(SIDE_PANEL_WIDTH)]).areas(area);
    (main, Some(side))
}

fn draw_side_panel(frame: &mut Frame, area: Rect, state: &AppState, podcast: Option<&Podcast>) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(theme::border_for(state));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(podcast) = podcast else {
        return;
    };
    let inner = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(1),
        ..inner
    };
    let art_height = ART_HEADER_HEIGHT.min(inner.height);
    draw_cover(
        frame,
        Rect {
            height: art_height,
            width: ART_HEADER_WIDTH.min(inner.width),
            ..inner
        },
        state,
        podcast,
    );
    let text_area = Rect {
        y: inner.y + art_height + 1,
        height: inner.height.saturating_sub(art_height + 1),
        ..inner
    };
    if text_area.height == 0 {
        return;
    }
    let mut lines = vec![Line::styled(
        podcast.title.clone(),
        Style::new().add_modifier(Modifier::BOLD),
    )];
    if let Some(author) = &podcast.author {
        lines.push(Line::styled(author.clone(), theme::dim()));
    }
    lines.push(Line::styled(
        format!("{} episodes", podcast.episode_count),
        theme::dim(),
    ));
    lines.extend(check_status_line(state, podcast));
    if let Some(description) = &podcast.description {
        lines.push(Line::raw(""));
        lines.push(Line::raw(description.clone()));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), text_area);
}

fn check_status_line(state: &AppState, podcast: &Podcast) -> Option<Line<'static>> {
    if state.podcasts.refreshing.contains(&podcast.id) {
        return Some(loading_line(state, "checking for new episodes…"));
    }
    if let Some(error) = &podcast.last_error {
        return Some(Line::styled(
            format!("last check failed: {error}"),
            error_style(),
        ));
    }
    podcast
        .last_checked_at
        .map(|at| Line::styled(format!("checked {}", format_date(at)), theme::dim()))
}

fn draw_cover(frame: &mut Frame, area: Rect, state: &AppState, podcast: &Podcast) {
    let art = podcast.cover_path.as_ref().and_then(|path| {
        state.art.get(&crate::art::cache_key(
            path,
            ART_HEADER_WIDTH,
            ART_HEADER_HEIGHT,
        ))
    });
    match art {
        Some(ArtState::Ready(image)) => {
            frame.render_widget(Paragraph::new(super::art::to_text(image)), area);
        }
        Some(ArtState::Loading) => centered_line(frame, area, Line::styled("…", theme::dim())),
        _ => centered_line(frame, area, Line::styled("🎙", theme::dim())),
    }
}

fn draw_episodes(frame: &mut Frame, area: Rect, state: &AppState, id: i64, cursor: usize) {
    let tab = &state.podcasts;
    let podcast = tab.podcast(id);
    let title = podcast.map_or("…".to_string(), |podcast| podcast.title.clone());
    let mut header = format!(" Podcasts ▸ {title} · {} ", tab.sort.label());
    if !tab.filter.is_empty() {
        header.push_str(&format!("· search: \"{}\" ", tab.filter));
    }
    let inner = bordered(frame, area, state, header);

    let episodes = match &tab.episodes {
        Some((loaded, Loadable::Ready(episodes))) if *loaded == id => episodes,
        Some((loaded, Loadable::Failed(error))) if *loaded == id => {
            return centered_line(frame, inner, Line::styled(error.clone(), error_style()));
        }
        _ => return centered_line(frame, inner, loading_line(state, "loading episodes…")),
    };
    let visible_indices = tab.visible_episode_indices();

    let (main, side) = split_side_panel(inner);
    if let Some(side) = side {
        draw_side_panel(frame, side, state, podcast);
    }
    // Episode notes below the list when there is room for them.
    let notes_height = if main.height >= 16 { 6 } else { 0 };
    let [list_area, notes_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(notes_height)]).areas(main);
    let [summary_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(list_area);

    let summary = if tab.filter.is_empty() {
        format!("{} episodes", episodes.len())
    } else {
        format!(
            "{} of {} episodes match \"{}\"",
            visible_indices.len(),
            episodes.len(),
            tab.filter
        )
    };
    let mut summary_spans = vec![Span::styled(summary, theme::dim())];
    if tab.refreshing.contains(&id) {
        summary_spans.push(Span::raw("  "));
        summary_spans.extend(loading_line(state, "checking for new episodes…").spans);
    }
    frame.render_widget(Paragraph::new(Line::from(summary_spans)), summary_area);

    if visible_indices.is_empty() {
        let message = if episodes.is_empty() {
            "this feed has no playable episodes"
        } else {
            "no episode title matches the search"
        };
        return centered_line(frame, list_area, Line::styled(message, theme::dim()));
    }

    let cursor = cursor.min(visible_indices.len() - 1);
    let playing_id = state
        .player
        .current
        .as_ref()
        .and_then(|track| crate::podcasts::episode_id_from_track_id(track.id));
    let visible = usize::from(list_area.height.max(1));
    let first = cursor
        .saturating_sub(visible / 2)
        .min(visible_indices.len().saturating_sub(visible));
    for (row_index, &episode_index) in visible_indices.iter().enumerate().skip(first).take(visible)
    {
        let episode = &episodes[episode_index];
        let area = Rect {
            x: list_area.x,
            y: list_area.y + (row_index - first) as u16,
            width: list_area.width,
            height: 1,
        };
        draw_episode_row(
            frame,
            area,
            state,
            episode,
            playing_id == Some(episode.id),
            row_index == cursor,
        );
    }

    if notes_height > 0
        && let Some(episode) = visible_indices
            .get(cursor)
            .and_then(|&index| episodes.get(index))
    {
        draw_notes(frame, notes_area, state, episode);
    }
}

fn draw_episode_row(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    episode: &Episode,
    playing: bool,
    selected: bool,
) {
    let date = episode
        .published_at
        .map(format_date)
        .unwrap_or_else(|| "          ".to_string());
    let marker = if playing {
        Span::styled("▶ ", theme::accent_for(state))
    } else {
        Span::raw("  ")
    };
    let duration = episode
        .duration_seconds
        .map(format_duration)
        .unwrap_or_default();
    let title_width = usize::from(area.width)
        .saturating_sub(2 + 10 + 2 + UnicodeWidthStr::width(duration.as_str()) + 2);
    let line = Line::from(vec![
        marker,
        Span::styled(date, theme::dim()),
        Span::raw("  "),
        Span::raw(fit_width(&episode.title, title_width)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
    if !duration.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(format!("{duration} "), theme::dim()))
                .alignment(Alignment::Right),
            area,
        );
    }
    if selected {
        frame
            .buffer_mut()
            .set_style(area, theme::tab_active_for(state));
    }
}

fn draw_notes(frame: &mut Frame, area: Rect, state: &AppState, episode: &Episode) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(theme::border_for(state))
        .title(Span::styled(" Episode notes ", theme::dim()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text = episode
        .description
        .clone()
        .unwrap_or_else(|| "No description in the feed.".to_string());
    frame.render_widget(
        Paragraph::new(Line::raw(text)).wrap(Wrap { trim: true }),
        inner,
    );
}

/// Truncate to a display width, adding an ellipsis when cut.
fn fit_width(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

fn format_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (hours, minutes, secs) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

/// `YYYY-MM-DD` (UTC) for unix seconds, without a date library: Howard
/// Hinnant's days-to-civil algorithm.
fn format_date(unix_seconds: i64) -> String {
    let days = unix_seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dates_in_utc() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(951_782_400), "2000-02-29");
        assert_eq!(format_date(1_718_409_600), "2024-06-15");
        assert_eq!(format_date(-86_400), "1969-12-31");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(59.6), "1:00");
        assert_eq!(format_duration(754.0), "12:34");
        assert_eq!(format_duration(3_723.0), "1:02:03");
    }

    #[test]
    fn fits_display_width_with_ellipsis() {
        assert_eq!(fit_width("Выпуск 650", 20), "Выпуск 650");
        assert_eq!(fit_width("Выпуск 650", 5), "Выпу…");
        assert_eq!(fit_width("abc", 0), "");
    }
}
