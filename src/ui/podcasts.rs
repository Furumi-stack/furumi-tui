//! Podcasts tab: subscriptions with their check status, a podcast's
//! seasons, and its episodes with saved position, played state, duration
//! and date. Rendering only reads `AppState`.

use furumi_library::podcasts::{EpisodeListItem, SeasonSummary};
use music_dht::podcast::ProgressStatus;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use super::{loading_line, theme};
use crate::app::state::{
    ART_HEADER_HEIGHT, ART_HEADER_WIDTH, AppState, ArtState, Loadable, PodcastListRow, PodcastRow,
    PodcastStoreStatus, SeasonRow,
};
use crate::app::update::podcasts::{played_label, sort_label};

/// Width below which the side panel with the cover is dropped.
const SIDE_PANEL_MIN_TOTAL_WIDTH: u16 = 80;
const SIDE_PANEL_WIDTH: u16 = ART_HEADER_WIDTH + 2;

pub(crate) const EMPTY_HINT: &str =
    "No subscriptions yet. Choose \"Add podcast\" and paste an RSS feed URL.";
pub(crate) const STORE_FAILED_NOTE: &str = "The podcast database was left unchanged. \
     Music, playlists and sync keep working.";

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState) {
    let tab = &state.podcasts;
    match &tab.store {
        PodcastStoreStatus::Opening => {
            let inner = bordered(frame, area, state, " Podcasts ".into());
            return centered_line(
                frame,
                inner,
                loading_line(state, "opening the podcast database…"),
            );
        }
        PodcastStoreStatus::Failed(err) => {
            let inner = bordered(frame, area, state, " Podcasts ".into());
            let lines = vec![
                Line::styled("Podcasts are unavailable", theme::header_for(state)),
                Line::default(),
                Line::styled(err.clone(), error_style()),
                Line::default(),
                Line::styled(STORE_FAILED_NOTE, theme::dim()),
            ];
            frame.render_widget(
                Paragraph::new(lines)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true }),
                inner,
            );
            return;
        }
        PodcastStoreStatus::Ready => {}
    }
    match &tab.opened {
        None => draw_list(frame, area, state),
        Some(opened) if opened.season.is_none() => draw_seasons(frame, area, state),
        Some(_) => draw_episodes(frame, area, state),
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

/// First row so that `cursor` stays visible, roughly centered.
fn first_visible(cursor: usize, len: usize, visible: usize) -> usize {
    cursor
        .saturating_sub(visible / 2)
        .min(len.saturating_sub(visible))
}

fn row_area(area: Rect, offset: usize) -> Rect {
    Rect {
        y: area.y + offset as u16,
        height: 1,
        ..area
    }
}

/// A row with a left part and a right-aligned dim part.
fn draw_row(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    left: Line<'static>,
    right: Option<String>,
    selected: bool,
) {
    frame.render_widget(Paragraph::new(left), area);
    if let Some(right) = right {
        frame.render_widget(
            Paragraph::new(Line::styled(format!("{right} "), theme::dim()))
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

// ---------------------------------------------------------------------------
// Subscriptions

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
    let first = first_visible(tab.selected, rows.len(), visible);
    for (index, row) in rows.iter().enumerate().skip(first).take(visible) {
        let (left, right) = list_row(state, *row, list_area.width);
        draw_row(
            frame,
            row_area(list_area, index - first),
            state,
            left,
            right,
            index == tab.selected,
        );
    }
    let used = rows.len().saturating_sub(first).min(visible) as u16;
    let below = Rect {
        y: list_area.y + used + 1,
        height: list_area.height.saturating_sub(used + 1),
        ..list_area
    };
    if below.height == 0 {
        return;
    }
    let hint = if tab.podcasts().is_empty() {
        Line::styled(EMPTY_HINT, theme::dim())
    } else if side_area.is_none()
        && let Some(row) = tab.current_podcast()
    {
        // Narrow terminal: the selected podcast's status below the list.
        fetch_status_line(state, row)
    } else {
        Line::default()
    };
    frame.render_widget(Paragraph::new(hint).wrap(Wrap { trim: true }), below);
}

fn list_row(state: &AppState, row: PodcastListRow, width: u16) -> (Line<'static>, Option<String>) {
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
            let label = if tab.refreshing.is_empty() {
                "↻ Check all podcasts for new episodes".to_string()
            } else {
                format!(
                    "{} Checking {} feed(s) for new episodes…",
                    state.spinner(),
                    tab.refreshing.len()
                )
            };
            (Line::styled(label, theme::accent_for(state)), None)
        }
        PodcastListRow::Podcast(index) => {
            let Some(podcast) = tab.podcasts().get(index) else {
                return (Line::raw(""), None);
            };
            let marker = if tab.refreshing.contains(podcast.id()) {
                Span::styled(format!("{} ", state.spinner()), theme::accent_for(state))
            } else if podcast.fetch.last_error.is_some() {
                Span::styled("! ", error_style())
            } else {
                Span::raw("  ")
            };
            let summary = &podcast.summary;
            let meta = if summary.unplayed_count > 0 {
                format!(
                    "{} unplayed · {} ep",
                    summary.unplayed_count, summary.episode_count
                )
            } else {
                format!("{} ep", summary.episode_count)
            };
            let title_width = usize::from(width).saturating_sub(2 + meta.width() + 2);
            let left = Line::from(vec![
                marker,
                Span::raw(fit_width(podcast.title(), title_width)),
            ]);
            (left, Some(meta))
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

fn draw_side_panel(frame: &mut Frame, area: Rect, state: &AppState, podcast: Option<&PodcastRow>) {
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
    let summary = &podcast.summary;
    let mut lines = vec![Line::styled(
        podcast.title().to_string(),
        Style::new().add_modifier(Modifier::BOLD),
    )];
    if let Some(author) = &summary.meta.author {
        lines.push(Line::styled(author.clone(), theme::dim()));
    }
    let mut counts = format!("{} episodes", summary.episode_count);
    if summary.season_count > 0 {
        counts.push_str(&format!(" · {} seasons", summary.season_count));
    }
    lines.push(Line::styled(counts, theme::dim()));
    lines.push(Line::styled("private", theme::dim()));
    lines.push(fetch_status_line(state, podcast));
    if let Some(description) = &summary.meta.description {
        lines.push(Line::raw(""));
        lines.push(Line::raw(description.clone()));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), text_area);
}

/// Refresh status of one podcast: checking, the last error, or when it was
/// last checked.
fn fetch_status_line(state: &AppState, podcast: &PodcastRow) -> Line<'static> {
    if state.podcasts.refreshing.contains(podcast.id()) {
        return loading_line(state, "checking for new episodes…");
    }
    let fetch = &podcast.fetch;
    if let Some(error) = &fetch.last_error {
        let mut text = format!("last check failed: {error}");
        if fetch.consecutive_failures > 1 {
            text.push_str(&format!(" ({} times)", fetch.consecutive_failures));
        }
        if let Some(next) = fetch.next_check_at_ms {
            text.push_str(&format!("; next try {}", relative_time(next)));
        }
        return Line::styled(text, error_style());
    }
    match fetch.last_success_at_ms {
        Some(at) => Line::styled(format!("checked {}", relative_time(at)), theme::dim()),
        None => Line::styled("not checked yet", theme::dim()),
    }
}

fn draw_cover(frame: &mut Frame, area: Rect, state: &AppState, podcast: &PodcastRow) {
    let art = podcast.fetch.cover_path.as_ref().and_then(|path| {
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

// ---------------------------------------------------------------------------
// Seasons

fn draw_seasons(frame: &mut Frame, area: Rect, state: &AppState) {
    let tab = &state.podcasts;
    let podcast = tab.current_podcast();
    let title = podcast.map_or("…".to_string(), |row| row.title().to_string());
    let inner = bordered(frame, area, state, format!(" Podcasts ▸ {title} "));
    let seasons = match &tab.seasons {
        Some((_, Loadable::Failed(error))) => {
            return centered_line(frame, inner, Line::styled(error.clone(), error_style()));
        }
        _ => match tab.opened_seasons() {
            Some(seasons) => seasons,
            None => return centered_line(frame, inner, loading_line(state, "loading seasons…")),
        },
    };
    let (main, side) = split_side_panel(inner);
    if let Some(side) = side {
        draw_side_panel(frame, side, state, podcast);
    }
    let cursor = tab
        .opened
        .as_ref()
        .and_then(|opened| opened.season_cursor)
        .unwrap_or(0);
    let rows = tab.season_rows();
    let visible = usize::from(main.height.max(1));
    let first = first_visible(cursor, rows.len(), visible);
    for (index, row) in rows.iter().enumerate().skip(first).take(visible) {
        let (label, meta) = match row {
            SeasonRow::All => {
                let (episodes, unplayed) = seasons.iter().fold((0, 0), |(e, u), season| {
                    (e + season.episode_count, u + season.unplayed_count)
                });
                ("All episodes".to_string(), counts(episodes, unplayed))
            }
            SeasonRow::Group(index) => {
                let Some(season) = seasons.get(*index) else {
                    continue;
                };
                (season_title(season), season_meta(season))
            }
        };
        let width = usize::from(main.width).saturating_sub(meta.width() + 4);
        draw_row(
            frame,
            row_area(main, index - first),
            state,
            Line::raw(format!("  {}", fit_width(&label, width))),
            Some(meta),
            index == cursor,
        );
    }
}

fn season_title(season: &SeasonSummary) -> String {
    match season.season {
        Some(number) => format!("Season {number}"),
        None => "Without a season".to_string(),
    }
}

fn counts(episodes: u64, unplayed: u64) -> String {
    if unplayed > 0 {
        format!("{unplayed} unplayed · {episodes} ep")
    } else {
        format!("{episodes} ep")
    }
}

fn season_meta(season: &SeasonSummary) -> String {
    let mut meta = counts(season.episode_count, season.unplayed_count);
    let years = (
        season.first_published_at_ms.map(year_of),
        season.last_published_at_ms.map(year_of),
    );
    match years {
        (Some(first), Some(last)) if first != last => meta.push_str(&format!(" · {first}–{last}")),
        (Some(year), _) | (None, Some(year)) => meta.push_str(&format!(" · {year}")),
        (None, None) => {}
    }
    meta
}

// ---------------------------------------------------------------------------
// Episodes

fn draw_episodes(frame: &mut Frame, area: Rect, state: &AppState) {
    let tab = &state.podcasts;
    let Some(opened) = &tab.opened else {
        return;
    };
    let podcast = tab.current_podcast();
    let mut title = podcast.map_or("…".to_string(), |row| row.title().to_string());
    if opened.season_cursor.is_some() {
        let season = match opened.season {
            Some(furumi_library::podcasts::SeasonFilter::Season(number)) => {
                format!("Season {number}")
            }
            Some(furumi_library::podcasts::SeasonFilter::NoSeason) => {
                "Without a season".to_string()
            }
            _ => "All episodes".to_string(),
        };
        title = format!("{title} ▸ {season}");
    }
    let inner = bordered(frame, area, state, format!(" Podcasts ▸ {title} "));
    let Some(list) = tab.opened_episodes() else {
        return centered_line(frame, inner, loading_line(state, "loading episodes…"));
    };

    let (main, side) = split_side_panel(inner);
    if let Some(side) = side {
        draw_side_panel(frame, side, state, podcast);
    }
    let notes_height = if main.height >= 18 { 6 } else { 0 };
    let [summary_area, list_area, notes_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(notes_height),
    ])
    .areas(main);

    let mut summary = vec![Span::styled(
        episode_summary(state, list.items.len(), list.complete),
        theme::dim(),
    )];
    if list.loading || list.loading_more {
        summary.push(Span::raw("  "));
        summary.extend(loading_line(state, "loading…").spans);
    } else if podcast.is_some_and(|row| tab.refreshing.contains(row.id())) {
        summary.push(Span::raw("  "));
        summary.extend(loading_line(state, "checking for new episodes…").spans);
    }
    frame.render_widget(Paragraph::new(Line::from(summary)), summary_area);

    if let Some(error) = &list.error {
        return centered_line(frame, list_area, Line::styled(error.clone(), error_style()));
    }
    if list.items.is_empty() {
        if list.loading {
            return;
        }
        let message = if tab.search.trim().is_empty()
            && tab.played == furumi_library::podcasts::PlayedFilter::Any
        {
            "this feed has no playable episodes yet"
        } else {
            "no episode matches the search or filter"
        };
        return centered_line(frame, list_area, Line::styled(message, theme::dim()));
    }

    let cursor = opened.episode_cursor.min(list.items.len() - 1);
    let visible = usize::from(list_area.height.max(1));
    let first = first_visible(cursor, list.items.len(), visible);
    for (index, item) in list.items.iter().enumerate().skip(first).take(visible) {
        draw_episode_row(
            frame,
            row_area(list_area, index - first),
            state,
            item,
            index == cursor,
        );
    }
    if notes_height > 0
        && let Some(item) = list.items.get(cursor)
    {
        draw_notes(frame, notes_area, state, item);
    }
}

fn episode_summary(state: &AppState, loaded: usize, complete: bool) -> String {
    let tab = &state.podcasts;
    let count = if complete {
        format!("{loaded} episodes")
    } else {
        format!("{loaded}+ episodes")
    };
    let mut parts = vec![
        count,
        played_label(tab.played).to_string(),
        sort_label(tab.sort).to_string(),
    ];
    let search = tab.search.trim();
    if !search.is_empty() {
        parts.push(format!("search: \"{search}\""));
    }
    parts.join(" · ")
}

/// Marker and right-hand text for an episode's listening state.
pub(crate) fn episode_state(item: &EpisodeListItem) -> (&'static str, String) {
    let duration = item.episode.duration_ms.filter(|ms| *ms > 0);
    match item.status() {
        ProgressStatus::Completed => (
            "✓",
            match duration {
                Some(ms) => format!("played · {}", format_duration_ms(ms)),
                None => "played".to_string(),
            },
        ),
        ProgressStatus::InProgress => {
            let position = format_duration_ms(item.resume_position_ms());
            (
                "◐",
                match duration {
                    Some(ms) => format!("{position} / {}", format_duration_ms(ms)),
                    None => format!("at {position}"),
                },
            )
        }
        ProgressStatus::NotStarted => ("●", duration.map(format_duration_ms).unwrap_or_default()),
    }
}

fn draw_episode_row(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    item: &EpisodeListItem,
    selected: bool,
) {
    let episode = &item.episode;
    let date = episode
        .published_at_ms
        .map(|ms| format_date(ms.div_euclid(1000)))
        .unwrap_or_else(|| " ".repeat(10));
    let (marker, right) = episode_state(item);
    let marker_style = match item.status() {
        ProgressStatus::NotStarted => theme::accent_for(state),
        _ => theme::dim(),
    };
    let number = match (episode.season, episode.episode_number) {
        (Some(season), Some(number)) => format!("S{season}E{number} "),
        (None, Some(number)) => format!("#{number} "),
        _ => String::new(),
    };
    let mut title = episode.title.clone();
    if !item.in_feed {
        title.push_str(" (no longer in the feed)");
    }
    let title_width =
        usize::from(area.width).saturating_sub(2 + 10 + 2 + number.width() + right.width() + 2);
    let title_style = match item.status() {
        ProgressStatus::Completed => theme::dim(),
        _ => Style::default(),
    };
    let line = Line::from(vec![
        Span::styled(format!("{marker} "), marker_style),
        Span::styled(date, theme::dim()),
        Span::raw("  "),
        Span::styled(number, theme::dim()),
        Span::styled(fit_width(&title, title_width), title_style),
    ]);
    draw_row(
        frame,
        area,
        state,
        line,
        (!right.is_empty()).then_some(right),
        selected,
    );
}

fn draw_notes(frame: &mut Frame, area: Rect, state: &AppState, item: &EpisodeListItem) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(theme::border_for(state))
        .title(Span::styled(" Episode notes ", theme::dim()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut text = item
        .episode
        .description
        .clone()
        .unwrap_or_else(|| "No description in the feed.".to_string());
    if item.completed_count > 1 {
        text = format!("Listened {} times. {text}", item.completed_count);
    }
    frame.render_widget(
        Paragraph::new(Line::raw(text)).wrap(Wrap { trim: true }),
        inner,
    );
}

// ---------------------------------------------------------------------------
// Formatting

/// Truncate to a display width, adding an ellipsis when cut.
pub(crate) fn fit_width(text: &str, width: usize) -> String {
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

pub(crate) fn format_duration_ms(ms: i64) -> String {
    let total = (ms.max(0) + 500) / 1000;
    let (hours, minutes, secs) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

/// `YYYY-MM-DD` (UTC) for unix seconds: Howard Hinnant's days-to-civil.
pub(crate) fn format_date(unix_seconds: i64) -> String {
    let (year, month, day) = civil(unix_seconds);
    format!("{year:04}-{month:02}-{day:02}")
}

fn year_of(unix_ms: i64) -> i64 {
    civil(unix_ms.div_euclid(1000)).0
}

fn civil(unix_seconds: i64) -> (i64, i64, i64) {
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
    (year, month, day)
}

/// "5 min ago", "in 2 h", or a date for anything further away.
fn relative_time(at_ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    relative_to(at_ms, now)
}

pub(crate) fn relative_to(at_ms: i64, now_ms: i64) -> String {
    let delta = (now_ms - at_ms) / 1000;
    let (amount, future) = (delta.unsigned_abs(), delta < 0);
    let text = match amount {
        0..60 => {
            return if future {
                "soon".into()
            } else {
                "just now".into()
            };
        }
        60..3_600 => format!("{} min", amount / 60),
        3_600..86_400 => format!("{} h", amount / 3_600),
        _ => return format_date(at_ms.div_euclid(1000)),
    };
    if future {
        format!("in {text}")
    } else {
        format!("{text} ago")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dates_durations_and_relative_times() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(951_782_400), "2000-02-29");
        assert_eq!(format_date(-86_400), "1969-12-31");
        assert_eq!(format_duration_ms(59_600), "1:00");
        assert_eq!(format_duration_ms(754_000), "12:34");
        assert_eq!(format_duration_ms(3_723_000), "1:02:03");
        let now = 1_700_000_000_000;
        assert_eq!(relative_to(now - 30_000, now), "just now");
        assert_eq!(relative_to(now - 5 * 60_000, now), "5 min ago");
        assert_eq!(relative_to(now + 2 * 3_600_000, now), "in 2 h");
        assert_eq!(relative_to(now - 3 * 86_400_000, now), "2023-11-11");
    }

    #[test]
    fn fits_display_width_with_ellipsis() {
        assert_eq!(fit_width("Выпуск 650", 20), "Выпуск 650");
        assert_eq!(fit_width("Выпуск 650", 5), "Выпу…");
        assert_eq!(fit_width("abc", 0), "");
    }
}
