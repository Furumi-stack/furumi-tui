use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Row, Table};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{art, theme};
use crate::api::models::{ArtistCard, ReleaseCard};
use crate::app::state::{
    ART_CELL_HEIGHT, ART_CELL_WIDTH, ART_HEADER_HEIGHT, ART_HEADER_WIDTH, AppState, ArtState,
    GlobalView, Loadable, TILE_HEIGHT, TILE_WIDTH, ViewMode, release_groups,
};
use crate::art::cache_key;

const TILE_MARQUEE_STEP_MS: u128 = 250;
const TILE_MARQUEE_PAUSE_STEPS: u128 = 4;
const TILE_MARQUEE_GAP: &str = "   ";

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState) {
    match state.global.stack.last() {
        None => draw_grid(frame, area, state),
        Some(GlobalView::Artist { id, cursor }) => draw_artist(frame, area, state, *id, *cursor),
        Some(GlobalView::Release { id, cursor }) => draw_release(frame, area, state, *id, *cursor),
        Some(GlobalView::Search { cursor }) => draw_search(frame, area, state, *cursor),
    }
}

fn error_style() -> Style {
    Style::new().fg(Color::Red)
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
    let middle = Rect {
        y: area.y + area.height / 2,
        height: 1,
        ..area
    };
    frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), middle);
}

fn tile_art<'a>(state: &'a AppState, url: Option<&String>) -> Option<&'a ArtState> {
    state
        .art
        .get(&cache_key(url?, ART_CELL_WIDTH, ART_CELL_HEIGHT))
}

fn header_art<'a>(state: &'a AppState, url: Option<&String>) -> Option<&'a ArtState> {
    state
        .art
        .get(&cache_key(url?, ART_HEADER_WIDTH, ART_HEADER_HEIGHT))
}

fn draw_art(frame: &mut Frame, area: Rect, art_state: Option<&ArtState>) {
    match art_state {
        Some(ArtState::Ready(image)) => {
            frame.render_widget(Paragraph::new(art::to_text(image)), area);
        }
        Some(ArtState::Loading) => centered_line(frame, area, Line::styled("…", theme::dim())),
        _ => centered_line(frame, area, Line::styled("♪", theme::dim())),
    }
}

/// Bordered tile with artwork, a title line and a dim meta line. The
/// selected tile gets a thick accent border and an inverted (filled)
/// caption so it stands out in a large grid; the artwork stays untouched.
fn draw_tile(
    frame: &mut Frame,
    tile: Rect,
    art_state: Option<&ArtState>,
    title: &str,
    meta: &str,
    selected: bool,
) {
    let block = if selected {
        Block::bordered()
            .border_type(ratatui::widgets::BorderType::Thick)
            .border_style(theme::accent())
    } else {
        Block::bordered().border_style(theme::dim())
    };
    let inner = block.inner(tile);
    frame.render_widget(block, tile);

    let art_area = Rect {
        height: ART_CELL_HEIGHT.min(inner.height),
        ..inner
    };
    draw_art(frame, art_area, art_state);

    if inner.height > ART_CELL_HEIGHT {
        let name_area = Rect {
            y: inner.y + ART_CELL_HEIGHT,
            height: 1,
            ..inner
        };
        frame.render_widget(
            Paragraph::new(Line::raw(tile_title(title, name_area.width, selected))),
            name_area,
        );
        if selected {
            frame.buffer_mut().set_style(name_area, theme::tab_active());
        }
    }
    if inner.height > ART_CELL_HEIGHT + 1 {
        let meta_area = Rect {
            y: inner.y + ART_CELL_HEIGHT + 1,
            height: 1,
            ..inner
        };
        frame.render_widget(
            Paragraph::new(Line::styled(meta.to_string(), theme::dim())),
            meta_area,
        );
        if selected {
            frame.buffer_mut().set_style(meta_area, theme::tab_active());
        }
    }
}

fn tile_title(title: &str, width: u16, selected: bool) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    if !selected || UnicodeWidthStr::width(title) <= width {
        return title.to_string();
    }
    marquee_window(title, width)
}

fn marquee_window(title: &str, width: usize) -> String {
    let stream = format!("{title}{TILE_MARQUEE_GAP}");
    let cells: Vec<(char, usize)> = stream
        .chars()
        .map(|ch| (ch, UnicodeWidthChar::width(ch).unwrap_or(0)))
        .collect();
    let total_width: usize = cells.iter().map(|(_, width)| *width).sum();
    if total_width == 0 {
        return title.to_string();
    }

    let phase = marquee_phase(total_width);
    let mut skipped = 0usize;
    let mut index = 0usize;
    while index < cells.len() {
        let next = skipped + cells[index].1;
        if next > phase {
            break;
        }
        skipped = next;
        index += 1;
    }

    let mut out = String::new();
    let mut used = 0usize;
    let mut steps = 0usize;
    while used < width && steps < cells.len() + width {
        let (ch, ch_width) = cells[(index + steps) % cells.len()];
        steps += 1;
        if ch_width == 0 {
            out.push(ch);
            continue;
        }
        if used + ch_width > width {
            continue;
        }
        out.push(ch);
        used += ch_width;
    }
    out
}

fn marquee_phase(total_width: usize) -> usize {
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() / TILE_MARQUEE_STEP_MS)
        .unwrap_or(0);
    let cycle = total_width as u128 + TILE_MARQUEE_PAUSE_STEPS;
    let phase = tick % cycle;
    if phase < TILE_MARQUEE_PAUSE_STEPS {
        0
    } else {
        (phase - TILE_MARQUEE_PAUSE_STEPS) as usize
    }
}

/// One selectable row: left content, optional right-aligned suffix, full-row
/// highlight when selected.
fn draw_row(frame: &mut Frame, area: Rect, line: Line, right: Option<String>, selected: bool) {
    frame.render_widget(Paragraph::new(line), area);
    if let Some(right) = right {
        frame.render_widget(
            Paragraph::new(Line::styled(right, theme::dim())).alignment(Alignment::Right),
            area,
        );
    }
    if selected {
        frame.buffer_mut().set_style(area, theme::tab_active());
    }
}

// ---------------------------------------------------------------------------
// Scrollable content plan: a vertical list of items with known heights; the
// viewport is scrolled so the cursor's item stays centered.
// ---------------------------------------------------------------------------

enum PlanItem {
    /// Section header line.
    Header(String),
    Gap,
    /// Selectable track row; the payload is the cursor index it represents.
    Track {
        cursor_index: usize,
    },
    /// One row of release tiles (display-order positions).
    TileRow(Vec<usize>),
    /// One release as a table row (display-order position).
    TableRow(usize),
}

impl PlanItem {
    fn height(&self) -> u16 {
        match self {
            PlanItem::Header(_)
            | PlanItem::Gap
            | PlanItem::Track { .. }
            | PlanItem::TableRow(_) => 1,
            PlanItem::TileRow(_) => TILE_HEIGHT,
        }
    }
}

fn scroll_offset(items: &[PlanItem], cursor_item: Option<usize>, viewport: u16) -> u16 {
    let total: u16 = items.iter().map(PlanItem::height).sum();
    if total <= viewport {
        return 0;
    }
    let Some(cursor_item) = cursor_item else {
        return 0;
    };
    let top: u16 = items[..cursor_item].iter().map(PlanItem::height).sum();
    let center = top + items[cursor_item].height() / 2;
    center
        .saturating_sub(viewport / 2)
        .min(total.saturating_sub(viewport))
}

// ---------------------------------------------------------------------------
// Artist grid (stack root)
// ---------------------------------------------------------------------------

fn draw_grid(frame: &mut Frame, area: Rect, state: &AppState) {
    let global = &state.global;
    let title = if global.total > 0 {
        format!(" Global — {} artists ", global.total)
    } else {
        " Global ".to_string()
    };
    let inner = bordered(frame, area, title);

    if global.artists.is_empty() {
        let message = if let Some(error) = &global.error {
            Line::styled(error.clone(), error_style())
        } else if global.loading {
            Line::styled("loading artists…", theme::dim())
        } else {
            Line::styled("no artists in the library", theme::dim())
        };
        centered_line(frame, inner, message);
        return;
    }

    match global.view {
        ViewMode::Tiles => draw_grid_tiles(frame, inner, state),
        ViewMode::Table => draw_grid_table(frame, inner, state),
    }
}

fn artist_tile_meta(artist: &ArtistCard) -> String {
    format!("{} rel · {} trk", artist.release_count, artist.track_count)
}

fn draw_grid_tiles(frame: &mut Frame, inner: Rect, state: &AppState) {
    let global = &state.global;
    let columns = usize::from((inner.width / TILE_WIDTH).max(1));
    let visible_rows = usize::from((inner.height / TILE_HEIGHT).max(1));

    let selected_row = global.selected / columns;
    let first_row = (selected_row / visible_rows) * visible_rows;
    let first_index = first_row * columns;
    let last_index = (first_index + visible_rows * columns).min(global.artists.len());

    for (offset, artist) in global.artists[first_index..last_index].iter().enumerate() {
        let index = first_index + offset;
        let tile = Rect {
            x: inner.x + (offset % columns) as u16 * TILE_WIDTH,
            y: inner.y + (offset / columns) as u16 * TILE_HEIGHT,
            width: TILE_WIDTH,
            height: TILE_HEIGHT,
        };
        draw_tile(
            frame,
            tile,
            tile_art(state, artist.image_url.as_ref()),
            &artist.name,
            &artist_tile_meta(artist),
            index == global.selected,
        );
    }
}

fn draw_grid_table(frame: &mut Frame, inner: Rect, state: &AppState) {
    let global = &state.global;
    let visible_rows = usize::from(inner.height.saturating_sub(1).max(1));
    let first = (global.selected / visible_rows) * visible_rows;
    let last = (first + visible_rows).min(global.artists.len());

    let rows = global.artists[first..last]
        .iter()
        .enumerate()
        .map(|(offset, artist)| {
            let index = first + offset;
            let style = if index == global.selected {
                theme::tab_active()
            } else {
                Style::new()
            };
            Row::new(vec![
                artist.name.clone(),
                artist.release_count.to_string(),
                artist.track_count.to_string(),
            ])
            .style(style)
        });
    let table = Table::new(
        rows,
        [
            Constraint::Min(24),
            Constraint::Length(9),
            Constraint::Length(7),
        ],
    )
    .header(Row::new(vec!["Artist", "Releases", "Tracks"]).style(theme::header()));
    frame.render_widget(table, inner);
}

// ---------------------------------------------------------------------------
// Artist view
// ---------------------------------------------------------------------------

fn draw_artist(frame: &mut Frame, area: Rect, state: &AppState, id: i64, cursor: usize) {
    let loadable = state.artist_views.get(&id);
    let name = match loadable {
        Some(Loadable::Ready(detail)) => detail.name.clone(),
        _ => "Artist".to_string(),
    };
    let inner = bordered(frame, area, format!(" Global ▸ {name} "));

    let detail = match loadable {
        Some(Loadable::Ready(detail)) => detail,
        Some(Loadable::Failed(error)) => {
            return centered_line(frame, inner, Line::styled(error.clone(), error_style()));
        }
        _ => return centered_line(frame, inner, Line::styled("loading…", theme::dim())),
    };

    let header_height = (ART_HEADER_HEIGHT + 1).min(inner.height);
    let [header_area, content_area] =
        Layout::vertical([Constraint::Length(header_height), Constraint::Min(0)]).areas(inner);

    // Header: artwork left, metadata right.
    let [art_area, _, info_area] = Layout::horizontal([
        Constraint::Length(ART_HEADER_WIDTH.min(header_area.width)),
        Constraint::Length(2),
        Constraint::Min(0),
    ])
    .areas(header_area);
    draw_art(
        frame,
        Rect {
            height: ART_HEADER_HEIGHT.min(art_area.height),
            ..art_area
        },
        header_art(state, detail.image_url.as_ref()),
    );
    let mut about = format!("{} releases", detail.releases.len());
    if !detail.featured_tracks.is_empty() {
        about.push_str(&format!(" · appears on {}", detail.featured_tracks.len()));
    }
    let info = vec![
        Line::default(),
        Line::styled(detail.name.clone(), theme::header()),
        Line::default(),
        Line::styled(
            format!(
                "{} tracks · {} plays",
                detail.total_track_count, detail.total_play_count
            ),
            theme::dim(),
        ),
        Line::styled(about, theme::dim()),
    ];
    frame.render_widget(Paragraph::new(info), info_area);

    // Scrollable content: top tracks, releases grouped by type, then the
    // tracks this artist is featured on.
    let tracks = detail.top_tracks.len();
    let releases_len = detail.releases.len();
    let featured_len = detail.featured_tracks.len();
    let mut items = Vec::new();
    let mut cursor_item = None;
    if tracks + releases_len + featured_len == 0 {
        return centered_line(
            frame,
            content_area,
            Line::styled("nothing here yet", theme::dim()),
        );
    }
    if tracks > 0 {
        items.push(PlanItem::Header("Top tracks".to_string()));
        for index in 0..tracks {
            if cursor == index {
                cursor_item = Some(items.len());
            }
            items.push(PlanItem::Track {
                cursor_index: index,
            });
        }
        items.push(PlanItem::Gap);
    }
    let columns = usize::from((content_area.width / TILE_WIDTH).max(1));
    let mut position = 0;
    for (label, group) in release_groups(&detail.releases) {
        items.push(PlanItem::Header(format!("{label} ({})", group.len())));
        match state.global.view {
            ViewMode::Tiles => {
                for chunk in group.chunks(columns) {
                    let row: Vec<usize> = (position..position + chunk.len()).collect();
                    if row.contains(&(cursor.wrapping_sub(tracks))) {
                        cursor_item = Some(items.len());
                    }
                    items.push(PlanItem::TileRow(row));
                    position += chunk.len();
                }
            }
            ViewMode::Table => {
                for _ in &group {
                    if cursor == tracks + position {
                        cursor_item = Some(items.len());
                    }
                    items.push(PlanItem::TableRow(position));
                    position += 1;
                }
            }
        }
        items.push(PlanItem::Gap);
    }
    if featured_len > 0 {
        items.push(PlanItem::Header(format!("Appears on ({featured_len})")));
        for index in 0..featured_len {
            let flat = tracks + releases_len + index;
            if cursor == flat {
                cursor_item = Some(items.len());
            }
            items.push(PlanItem::Track { cursor_index: flat });
        }
        items.push(PlanItem::Gap);
    }

    let display_order = crate::app::state::release_display_order(&detail.releases);
    render_plan(
        frame,
        content_area,
        state,
        &items,
        cursor_item,
        &mut |frame, rect, item| match item {
            PlanItem::Track { cursor_index } => {
                let (track, number, visual_selected) = if *cursor_index < tracks {
                    (
                        &detail.top_tracks[*cursor_index],
                        cursor_index + 1,
                        state.track_selection.contains(
                            &crate::app::state::TrackSelectionScope::ArtistTop(id),
                            *cursor_index,
                        ),
                    )
                } else {
                    let offset = cursor_index - tracks - releases_len;
                    (
                        &detail.featured_tracks[offset],
                        offset + 1,
                        state.track_selection.contains(
                            &crate::app::state::TrackSelectionScope::ArtistFeatured(id),
                            offset,
                        ),
                    )
                };
                super::track_row(
                    frame,
                    rect,
                    state,
                    track,
                    number.to_string(),
                    cursor == *cursor_index,
                    visual_selected,
                );
            }
            PlanItem::TileRow(row) => {
                for (column, position) in row.iter().enumerate() {
                    let release = &detail.releases[display_order[*position]];
                    let tile = Rect {
                        x: rect.x + column as u16 * TILE_WIDTH,
                        y: rect.y,
                        width: TILE_WIDTH
                            .min(rect.width.saturating_sub(column as u16 * TILE_WIDTH)),
                        height: rect.height,
                    };
                    if tile.width < 3 {
                        break;
                    }
                    draw_tile(
                        frame,
                        tile,
                        tile_art(state, release.cover_url.as_ref()),
                        &release.title,
                        &release_tile_meta(release),
                        cursor == tracks + position,
                    );
                }
            }
            PlanItem::TableRow(position) => {
                let release = &detail.releases[display_order[*position]];
                let year = release.year.map(|y| y.to_string()).unwrap_or_default();
                draw_row(
                    frame,
                    rect,
                    Line::from(vec![
                        Span::raw(release.title.clone()),
                        Span::styled(format!("  {year}"), theme::dim()),
                    ]),
                    Some(format!("{} trk", release.track_count)),
                    cursor == tracks + position,
                );
            }
            _ => unreachable!("headers and gaps are rendered by render_plan"),
        },
    );
}

fn release_tile_meta(release: &ReleaseCard) -> String {
    match release.year {
        Some(year) => format!("{year} · {} trk", release.track_count),
        None => format!("{} trk", release.track_count),
    }
}

/// Render plan items into `area`, scrolled so the cursor item is visible.
/// Headers and gaps are drawn here; everything else is delegated.
fn render_plan(
    frame: &mut Frame,
    area: Rect,
    _state: &AppState,
    items: &[PlanItem],
    cursor_item: Option<usize>,
    draw_item: &mut dyn FnMut(&mut Frame, Rect, &PlanItem),
) {
    if area.height == 0 {
        return;
    }
    let offset = scroll_offset(items, cursor_item, area.height);
    let mut top: u16 = 0;
    for item in items {
        let height = item.height();
        let item_top = top;
        top += height;
        if item_top < offset {
            continue;
        }
        let rel_y = item_top - offset;
        if rel_y >= area.height {
            break;
        }
        let rect = Rect {
            x: area.x,
            y: area.y + rel_y,
            width: area.width,
            height: height.min(area.height - rel_y),
        };
        match item {
            PlanItem::Header(label) => frame.render_widget(
                Paragraph::new(Line::styled(label.clone(), theme::header())),
                rect,
            ),
            PlanItem::Gap => {}
            other => draw_item(frame, rect, other),
        }
    }
}

// ---------------------------------------------------------------------------
// Release view
// ---------------------------------------------------------------------------

fn draw_release(frame: &mut Frame, area: Rect, state: &AppState, id: i64, cursor: usize) {
    let loadable = state.release_views.get(&id);
    let title = match loadable {
        Some(Loadable::Ready(detail)) => detail.title.clone(),
        _ => "Release".to_string(),
    };
    let inner = bordered(frame, area, format!(" Global ▸ {title} "));

    let detail = match loadable {
        Some(Loadable::Ready(detail)) => detail,
        Some(Loadable::Failed(error)) => {
            return centered_line(frame, inner, Line::styled(error.clone(), error_style()));
        }
        _ => return centered_line(frame, inner, Line::styled("loading…", theme::dim())),
    };

    let header_height = (ART_HEADER_HEIGHT + 1).min(inner.height);
    let [header_area, tracks_area] =
        Layout::vertical([Constraint::Length(header_height), Constraint::Min(0)]).areas(inner);

    let [art_area, _, info_area] = Layout::horizontal([
        Constraint::Length(ART_HEADER_WIDTH.min(header_area.width)),
        Constraint::Length(2),
        Constraint::Min(0),
    ])
    .areas(header_area);
    draw_art(
        frame,
        Rect {
            height: ART_HEADER_HEIGHT.min(art_area.height),
            ..art_area
        },
        header_art(state, detail.cover_url.as_ref()),
    );

    let artists: Vec<&str> = detail.artists.iter().map(|a| a.name.as_str()).collect();
    let year = detail.year.map(|y| format!(" · {y}")).unwrap_or_default();
    let uploaders: Vec<&str> = detail.uploaders.iter().map(|u| u.name.as_str()).collect();
    let mut info = vec![
        Line::default(),
        Line::styled(detail.title.clone(), theme::header()),
        Line::raw(artists.join(", ")),
        Line::default(),
        Line::styled(
            format!(
                "{}{year} · {} tracks",
                detail.release_type,
                detail.tracks.len()
            ),
            theme::dim(),
        ),
    ];
    if !uploaders.is_empty() {
        info.push(Line::styled(
            format!("uploaded by {}", uploaders.join(", ")),
            theme::dim(),
        ));
    }
    frame.render_widget(Paragraph::new(info), info_area);

    // Track list with centered scrolling.
    let visible = usize::from(tracks_area.height.max(1));
    let total = detail.tracks.len();
    let first = cursor
        .saturating_sub(visible / 2)
        .min(total.saturating_sub(visible));
    for (offset, track) in detail.tracks.iter().enumerate().skip(first).take(visible) {
        let rect = Rect {
            x: tracks_area.x,
            y: tracks_area.y + (offset - first) as u16,
            width: tracks_area.width,
            height: 1,
        };
        let number = track
            .track_number
            .map(|n| n.to_string())
            .unwrap_or_else(|| (offset + 1).to_string());
        super::track_row(
            frame,
            rect,
            state,
            track,
            number,
            cursor == offset,
            state
                .track_selection
                .contains(&crate::app::state::TrackSelectionScope::Release(id), offset),
        );
    }
}

// ---------------------------------------------------------------------------
// Search view (driven by the `:/query` command)
// ---------------------------------------------------------------------------

fn draw_search(frame: &mut Frame, area: Rect, state: &AppState, cursor: usize) {
    let search = &state.search;
    let mut title = format!(" Search: {} ", search.query);
    if search.loading {
        title.push_str("· searching… ");
    }
    let inner = bordered(frame, area, title);

    let Some(results) = &search.results else {
        let hint = if search.query.is_empty() {
            "type to search artists, releases and tracks"
        } else {
            "searching…"
        };
        return centered_line(frame, inner, Line::styled(hint, theme::dim()));
    };
    if results.len() == 0 {
        return centered_line(frame, inner, Line::styled("nothing found", theme::dim()));
    }

    // All rows are one line tall: (line, right column, cursor index).
    let mut rows: Vec<(Line, Option<String>, Option<usize>)> = Vec::new();
    let mut index = 0;
    if !results.artists.is_empty() {
        rows.push((Line::styled("Artists", theme::header()), None, None));
        for artist in &results.artists {
            rows.push((
                Line::raw(artist.name.clone()),
                Some(artist_tile_meta(artist)),
                Some(index),
            ));
            index += 1;
        }
        rows.push((Line::default(), None, None));
    }
    if !results.releases.is_empty() {
        rows.push((Line::styled("Releases", theme::header()), None, None));
        for release in &results.releases {
            rows.push((
                Line::from(vec![
                    Span::raw(release.title.clone()),
                    Span::styled(format!("  {}", release.release_type), theme::dim()),
                ]),
                Some(release_tile_meta(release)),
                Some(index),
            ));
            index += 1;
        }
        rows.push((Line::default(), None, None));
    }
    if !results.tracks.is_empty() {
        rows.push((Line::styled("Tracks", theme::header()), None, None));
        for track in &results.tracks {
            let heart = if state.likes.contains(&track.id) {
                Span::styled("♥ ", theme::accent())
            } else {
                Span::raw("  ")
            };
            rows.push((
                Line::from(vec![
                    heart,
                    Span::raw(track.title.clone()),
                    Span::styled(
                        format!("  {} · {}", track.artist_line(), track.release_title),
                        theme::dim(),
                    ),
                ]),
                Some(super::track_meta_suffix(track, true)),
                Some(index),
            ));
            index += 1;
        }
    }

    let cursor_row = rows
        .iter()
        .position(|(_, _, c)| *c == Some(cursor))
        .unwrap_or(0);
    let visible = usize::from(inner.height.max(1));
    let first = cursor_row
        .saturating_sub(visible / 2)
        .min(rows.len().saturating_sub(visible));
    for (offset, (line, right, row_cursor)) in
        rows.into_iter().enumerate().skip(first).take(visible)
    {
        let rect = Rect {
            x: inner.x,
            y: inner.y + (offset - first) as u16,
            width: inner.width,
            height: 1,
        };
        draw_row(frame, rect, line, right, row_cursor == Some(cursor));
    }
}
