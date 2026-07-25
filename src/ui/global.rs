use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Row, Table};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{art, availability_marker, availability_prefix, theme};
use crate::app::state::{
    ART_CELL_HEIGHT, ART_CELL_WIDTH, ART_HEADER_HEIGHT, ART_HEADER_WIDTH, AppState, ArtState,
    GlobalView, Loadable, TILE_HEIGHT, TILE_WIDTH, ViewMode, artist_merged_releases,
    artist_release_display_order, artist_release_groups, fed_artist_visible_appearance_indices,
    fed_artist_visible_release_indices, fed_release_display_order, fed_release_groups,
    fed_release_visible_track_indices,
};
use crate::art::cache_key;
use crate::library::models::{ArtistCard, Availability, ReleaseCard, SearchResults};

const TILE_MARQUEE_STEP_MS: u128 = 250;
const TILE_MARQUEE_PAUSE_STEPS: u128 = 4;
const TILE_MARQUEE_GAP: &str = "   ";

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState) {
    match state.global.stack.last() {
        None => draw_grid(frame, area, state),
        Some(GlobalView::Artist { id, cursor }) => draw_artist(frame, area, state, *id, *cursor),
        Some(GlobalView::Release { id, cursor }) => draw_release(frame, area, state, *id, *cursor),
        Some(GlobalView::Search { cursor }) => draw_search(frame, area, state, *cursor),
        Some(GlobalView::FedArtist { cursor }) => draw_fed_artist(frame, area, state, *cursor),
        Some(GlobalView::FedRelease { index, cursor }) => {
            draw_fed_release(frame, area, state, *index, *cursor)
        }
    }
}

fn error_style() -> Style {
    Style::new().fg(Color::Red)
}

fn bordered(frame: &mut Frame, area: Rect, state: &AppState, title: String) -> Rect {
    bordered_line(
        frame,
        area,
        state,
        Line::styled(title, theme::header_for(state)),
    )
}

fn bordered_line(frame: &mut Frame, area: Rect, state: &AppState, title: Line<'static>) -> Rect {
    let block = Block::bordered()
        .title(title)
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
fn draw_tile_with_availability(
    frame: &mut Frame,
    tile: Rect,
    state: &AppState,
    art_state: Option<&ArtState>,
    title: &str,
    meta: &str,
    selected: bool,
    availability: Option<Availability>,
) {
    let block = if selected {
        Block::bordered()
            .border_type(ratatui::widgets::BorderType::Thick)
            .border_style(theme::strong_border_for(state))
    } else {
        Block::bordered().border_style(theme::border_for(state))
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
            frame
                .buffer_mut()
                .set_style(name_area, theme::tab_active_for(state));
        }
    }
    if inner.height > ART_CELL_HEIGHT + 1 {
        let meta_area = Rect {
            y: inner.y + ART_CELL_HEIGHT + 1,
            height: 1,
            ..inner
        };
        draw_tile_meta(frame, meta_area, state, meta, availability, selected);
    }
}

fn draw_tile_meta(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    meta: &str,
    availability: Option<Availability>,
    selected: bool,
) {
    let marker = availability.map(|availability| availability_marker(availability, selected));
    let marker_width = marker
        .map(|(label, _)| UnicodeWidthStr::width(label) as u16)
        .unwrap_or(0)
        .max(u16::from(marker.is_some()) * 2)
        .min(area.width);
    let marker_pad = u16::from(marker_width > 0 && area.width > marker_width);
    let reserved_width = marker_width.saturating_add(marker_pad).min(area.width);
    let text_area = if reserved_width > 0 && area.width > reserved_width {
        Rect {
            width: area.width - reserved_width,
            ..area
        }
    } else {
        area
    };
    frame.render_widget(
        Paragraph::new(Line::styled(meta.to_string(), theme::dim())),
        text_area,
    );
    if selected {
        frame
            .buffer_mut()
            .set_style(area, theme::tab_active_for(state));
    }
    if let Some((label, style)) = marker
        && marker_width > 0
    {
        let marker_area = Rect {
            x: area
                .x
                .saturating_add(area.width.saturating_sub(reserved_width)),
            y: area.y,
            width: marker_width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::styled(label, style)).alignment(Alignment::Right),
            marker_area,
        );
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

fn fed_track_availability_prefix(
    state: &AppState,
    track: &crate::federation::FedTrack,
) -> Span<'static> {
    let availability = if state.fed_track_local(track) {
        Availability::Local
    } else {
        Availability::Remote
    };
    availability_prefix(availability)
}

fn fed_card_track_availability_prefix(
    state: &AppState,
    track: &crate::federation::FedCardTrack,
) -> Span<'static> {
    let availability = if state.fed_card_track_local(track) {
        Availability::Local
    } else {
        Availability::Remote
    };
    availability_prefix(availability)
}

fn fed_release_availability(
    state: &AppState,
    release: &crate::federation::FedRelease,
) -> Availability {
    if release.tracks.is_empty() {
        return Availability::Remote;
    }
    let local = release
        .tracks
        .iter()
        .filter(|track| state.fed_card_track_local(track))
        .count();
    match local {
        0 => Availability::Remote,
        count if count == release.tracks.len() => Availability::Local,
        _ => Availability::Mixed,
    }
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
fn draw_row(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    line: Line,
    right: Option<String>,
    selected: bool,
) {
    frame.render_widget(Paragraph::new(line), area);
    if let Some(right) = right {
        frame.render_widget(
            Paragraph::new(Line::styled(right, theme::dim())).alignment(Alignment::Right),
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
        format!(
            " Library — {} artists · {} ",
            global.total,
            global.filters.source_mode.label()
        )
    } else {
        format!(" Library · {} ", global.filters.source_mode.label())
    };
    let mut title_spans = vec![Span::styled(title, theme::tab_active_for(state))];
    if global.filters.is_active() {
        title_spans.push(Span::raw(" "));
        title_spans.push(Span::styled(" FILTERED ", theme::tab_active_for(state)));
    }
    let inner = bordered_line(frame, area, state, Line::from(title_spans));

    if global.artists.is_empty() {
        let message = if let Some(error) = &global.error {
            Line::styled(error.clone(), error_style())
        } else if global.loading {
            super::loading_line(state, "loading artists…")
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
    format!("{} rel {} trk", artist.release_count, artist.track_count)
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
        draw_tile_with_availability(
            frame,
            tile,
            state,
            tile_art(state, artist.image_path.as_ref()),
            &artist.name,
            &artist_tile_meta(artist),
            index == global.selected,
            Some(artist.availability),
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
                theme::tab_active_for(state)
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
    .header(Row::new(vec!["Artist", "Releases", "Tracks"]).style(theme::header_for(state)));
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
    let inner = bordered(frame, area, state, format!(" Library ▸ {name} "));

    let detail = match loadable {
        Some(Loadable::Ready(detail)) => detail,
        Some(Loadable::Failed(error)) => {
            return centered_line(frame, inner, Line::styled(error.clone(), error_style()));
        }
        _ => return centered_line(frame, inner, super::loading_line(state, "loading…")),
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
        header_art(
            state,
            detail.image_path.as_ref().or_else(|| {
                crate::app::state::artist_fed_card(state, id)
                    .and_then(|card| card.image_path.as_ref())
            }),
        ),
    );
    let merged_releases = artist_merged_releases(state, id, detail);
    let mut about = format!("{} releases", merged_releases.len());
    if !detail.featured_tracks.is_empty() {
        about.push_str(&format!(" · appears on {}", detail.featured_tracks.len()));
    }
    let federation_line = match state.artist_fed_views.get(&id) {
        Some(Loadable::Loading) => Some(super::loading_line(state, "searching peers…")),
        Some(Loadable::Ready(card)) => {
            let tracks: usize = card
                .releases
                .iter()
                .map(|release| release.tracks.len())
                .sum();
            Some(Line::styled(
                format!(
                    "federation: {} releases · {} tracks · {} peers",
                    card.releases.len(),
                    tracks,
                    card.peers
                ),
                theme::dim(),
            ))
        }
        Some(Loadable::Failed(_)) => Some(Line::styled("federation: unavailable", theme::dim())),
        None if state.federation.settings.enabled => {
            Some(Line::styled("federation: pending", theme::dim()))
        }
        None => None,
    };
    let mut info = vec![
        Line::default(),
        Line::styled(detail.name.clone(), theme::header_for(state)),
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
    if let Some(line) = federation_line {
        info.push(line);
    }
    frame.render_widget(Paragraph::new(info), info_area);

    // Scrollable content: top tracks, releases grouped by type, then the
    // tracks this artist is featured on.
    let tracks = detail.top_tracks.len();
    let releases_len = merged_releases.len();
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
        items.push(PlanItem::Header("Liked tracks".to_string()));
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
    for (label, group) in artist_release_groups(&merged_releases) {
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

    let display_order = artist_release_display_order(&merged_releases);
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
                    let release = &merged_releases[display_order[*position]];
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
                    draw_tile_with_availability(
                        frame,
                        tile,
                        state,
                        tile_art(state, release.cover_path.as_ref()),
                        &release.title,
                        &artist_release_tile_meta(release),
                        cursor == tracks + position,
                        Some(release.availability),
                    );
                }
            }
            PlanItem::TableRow(position) => {
                let release = &merged_releases[display_order[*position]];
                let year = release.year.map(|y| y.to_string()).unwrap_or_default();
                draw_row(
                    frame,
                    rect,
                    state,
                    Line::from(vec![
                        Span::raw(release.title.clone()),
                        Span::styled(format!("  {year}"), theme::dim()),
                    ]),
                    Some(artist_release_count_meta(release)),
                    cursor == tracks + position,
                );
            }
            _ => unreachable!("headers and gaps are rendered by render_plan"),
        },
    );
}

fn artist_release_tile_meta(release: &crate::app::state::ArtistReleaseSlot) -> String {
    match release.year {
        Some(year) => format!("{year} · {}", artist_release_count_meta(release)),
        None => artist_release_count_meta(release),
    }
}

fn artist_release_count_meta(release: &crate::app::state::ArtistReleaseSlot) -> String {
    match release.availability {
        Availability::Mixed => format!(
            "{}/{} local",
            release.local_track_count.min(release.total_track_count),
            release.total_track_count
        ),
        Availability::Remote => format!("{} network", release.total_track_count),
        Availability::Local => format!("{} trk", release.total_track_count),
    }
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
    state: &AppState,
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
                Paragraph::new(Line::styled(label.clone(), theme::header_for(state))),
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
    let inner = bordered(frame, area, state, format!(" Library ▸ {title} "));

    let detail = match loadable {
        Some(Loadable::Ready(detail)) => detail,
        Some(Loadable::Failed(error)) => {
            return centered_line(frame, inner, Line::styled(error.clone(), error_style()));
        }
        _ => return centered_line(frame, inner, super::loading_line(state, "loading…")),
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
        header_art(state, detail.cover_path.as_ref()),
    );

    let artists: Vec<&str> = detail.artists.iter().map(|a| a.name.as_str()).collect();
    let year = detail.year.map(|y| format!(" · {y}")).unwrap_or_default();
    let info = vec![
        Line::default(),
        Line::styled(detail.title.clone(), theme::header_for(state)),
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
    let inner = bordered(frame, area, state, title);

    let empty_results = SearchResults::default();
    let results = match &search.results {
        Some(results) => results,
        None if !state.search.fed_tracks.is_empty()
            || !state.search.fed_artists.is_empty()
            || state.search.fed_loading =>
        {
            &empty_results
        }
        None => {
            let hint = if search.query.is_empty() {
                "type to search artists, releases and tracks"
            } else {
                "searching…"
            };
            let line = if search.query.is_empty() {
                Line::styled(hint, theme::dim())
            } else {
                super::loading_line(state, hint)
            };
            return centered_line(frame, inner, line);
        }
    };
    if results.len() == 0
        && state.search.fed_tracks.is_empty()
        && state.search.fed_artists.is_empty()
        && !state.search.fed_loading
    {
        return centered_line(frame, inner, Line::styled("nothing found", theme::dim()));
    }

    // All rows are one line tall: (line, right column, cursor index).
    let mut rows: Vec<(Line, Option<String>, Option<usize>)> = Vec::new();
    let mut index = 0;
    if !results.artists.is_empty() {
        rows.push((
            Line::styled("Artists", theme::header_for(state)),
            None,
            None,
        ));
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
        rows.push((
            Line::styled("Releases", theme::header_for(state)),
            None,
            None,
        ));
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
        rows.push((Line::styled("Tracks", theme::header_for(state)), None, None));
        for track in &results.tracks {
            let heart = if state.track_liked(track) {
                Span::styled("♥ ", theme::accent_for(state))
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
    // Federated section: artists whose card can be assembled, then tracks
    // (both marked with the owning peers).
    if !state.search.fed_tracks.is_empty()
        || !state.search.fed_artists.is_empty()
        || state.search.fed_loading
    {
        if !rows.is_empty() {
            rows.push((Line::default(), None, None));
        }
        let header = if state.search.fed_loading {
            "Federation · searching…"
        } else {
            "Federation"
        };
        rows.push((Line::styled(header, theme::header_for(state)), None, None));
        for hit in &state.search.fed_artists {
            rows.push((
                Line::from(vec![
                    availability_prefix(Availability::Remote),
                    Span::raw(hit.name.clone()),
                    Span::styled("  artist · open the card", theme::dim()),
                ]),
                Some(format!(
                    "{} peer{}",
                    hit.peers,
                    if hit.peers == 1 { "" } else { "s" }
                )),
                Some(index),
            ));
            index += 1;
        }
        for fed in &state.search.fed_tracks {
            let heart = if state.fed_track_liked(fed) {
                Span::styled("♥ ", theme::accent_for(state))
            } else {
                Span::raw("  ")
            };
            let origin = if fed.own {
                "your library".to_string()
            } else {
                format!("peer {}…", fed.owner_short())
            };
            let mut meta = fed.duration_label();
            if let Some(year) = fed.year {
                if !meta.is_empty() {
                    meta.push_str(" · ");
                }
                meta.push_str(&year.to_string());
            }
            rows.push((
                Line::from(vec![
                    heart,
                    fed_track_availability_prefix(state, fed),
                    Span::raw(fed.title.clone()),
                    Span::styled(
                        format!("  {} · {}", fed.artist_line(), origin),
                        theme::dim(),
                    ),
                ]),
                Some(meta),
                Some(index),
            ));
            index += 1;
        }
    }

    // Absolute cursor indices covered by an active Shift-V range in the
    // federated tracks section.
    let fed_scope = crate::app::state::TrackSelectionScope::FedSearch;
    let mut fed_selected: std::collections::HashSet<usize> = Default::default();
    if state.track_selection.is_active_for(&fed_scope) {
        let base = results.len() + state.search.fed_artists.len();
        if let Some(indices) = state
            .track_selection
            .indices(&fed_scope, state.search.fed_tracks.len())
        {
            fed_selected.extend(indices.into_iter().map(|i| base + i));
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
        if let Some(row_index) = row_cursor
            && fed_selected.contains(&row_index)
            && row_index != cursor
        {
            frame
                .buffer_mut()
                .set_style(rect, theme::selection_for(state));
        }
        draw_row(frame, rect, state, line, right, row_cursor == Some(cursor));
    }
}

// ---------------------------------------------------------------------------
// Federated artist card (assembled from peer catalogs)
// ---------------------------------------------------------------------------

fn draw_fed_artist(frame: &mut Frame, area: Rect, state: &AppState, cursor: usize) {
    let Some((name, data)) = &state.fed_artist_view else {
        return centered_line(frame, area, Line::styled("no card is open", theme::dim()));
    };
    let inner = bordered(frame, area, state, format!(" Federation ▸ {name} "));
    let card = match data {
        Loadable::Loading => {
            return centered_line(
                frame,
                inner,
                super::loading_line(state, "assembling the card from peers…"),
            );
        }
        Loadable::Failed(message) => {
            return centered_line(frame, inner, Line::styled(message.clone(), error_style()));
        }
        Loadable::Ready(card) => card,
    };

    // Header: artist image (streamed from a peer) left, stats right.
    let header_height = (ART_HEADER_HEIGHT + 1).min(inner.height);
    let [header_area, content_area] =
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
        header_art(state, card.image_path.as_ref()),
    );
    let release_indices = fed_artist_visible_release_indices(state, card);
    let appearance_indices = fed_artist_visible_appearance_indices(state, card);
    let visible_releases: Vec<_> = release_indices
        .iter()
        .map(|&index| card.releases[index].clone())
        .collect();
    let release_tracks_total: usize = visible_releases
        .iter()
        .map(|release| fed_release_visible_track_indices(state, release).len())
        .sum();
    let appears_on_len = appearance_indices.len();
    let mut stats = format!(
        "{} releases · {} tracks",
        visible_releases.len(),
        release_tracks_total
    );
    if appears_on_len > 0 {
        stats.push_str(&format!(" · appears on {appears_on_len}"));
    }
    if state.global.filters.source_mode.includes_network() {
        stats.push_str(&format!(" · from {} peers", card.peers));
    } else {
        stats.push_str(" · local only");
    }
    let info = vec![
        Line::default(),
        Line::styled(name.clone(), theme::header_for(state)),
        Line::default(),
        Line::styled(stats, theme::dim()),
        Line::styled("enter: open release / play track · esc: back", theme::dim()),
    ];
    frame.render_widget(Paragraph::new(info), info_area);

    if visible_releases.is_empty() && appearance_indices.is_empty() {
        let message = if state.global.filters.source_mode.includes_network() {
            "the peers returned no releases or appearances"
        } else {
            "artist is not available locally"
        };
        return centered_line(frame, content_area, Line::styled(message, theme::dim()));
    }

    // Release tiles grouped by type, then featured appearances as tracks.
    let columns = usize::from((content_area.width / TILE_WIDTH).max(1));
    let releases_len = visible_releases.len();
    let mut items = Vec::new();
    let mut cursor_item = None;
    let mut position = 0;
    for (label, group) in fed_release_groups(&visible_releases) {
        items.push(PlanItem::Header(format!("{label} ({})", group.len())));
        for chunk in group.chunks(columns) {
            let row: Vec<usize> = (position..position + chunk.len()).collect();
            if row.contains(&cursor) {
                cursor_item = Some(items.len());
            }
            items.push(PlanItem::TileRow(row));
            position += chunk.len();
        }
        items.push(PlanItem::Gap);
    }
    if appears_on_len > 0 {
        items.push(PlanItem::Header(format!("Appears on ({appears_on_len})")));
        for index in 0..appears_on_len {
            let flat = releases_len + index;
            if cursor == flat {
                cursor_item = Some(items.len());
            }
            items.push(PlanItem::Track { cursor_index: flat });
        }
        items.push(PlanItem::Gap);
    }

    let display_order = fed_release_display_order(&visible_releases);
    let appears_scope = crate::app::state::TrackSelectionScope::FedAppearsOn;
    render_plan(
        frame,
        content_area,
        state,
        &items,
        cursor_item,
        &mut |frame, rect, item| match item {
            PlanItem::TileRow(row) => {
                for (column, position) in row.iter().enumerate() {
                    let release = &visible_releases[display_order[*position]];
                    let tile = Rect {
                        x: rect.x + column as u16 * TILE_WIDTH,
                        y: rect.y,
                        width: TILE_WIDTH
                            .min(rect.width.saturating_sub(column as u16 * TILE_WIDTH)),
                        height: rect.height,
                    };
                    if tile.width < 3 || tile.height < 3 {
                        break;
                    }
                    let mut meta = release.release_type.clone();
                    if let Some(year) = release.year {
                        meta = format!("{meta} · {year}");
                    }
                    draw_tile_with_availability(
                        frame,
                        tile,
                        state,
                        tile_art(state, release.cover_path.as_ref()),
                        &release.title,
                        &meta,
                        cursor == *position,
                        Some(fed_release_availability(state, release)),
                    );
                }
            }
            PlanItem::Track { cursor_index } => {
                let index = cursor_index - releases_len;
                let Some(&appearance_index) = appearance_indices.get(index) else {
                    return;
                };
                let Some(appearance) = card.appears_on.get(appearance_index) else {
                    return;
                };
                draw_fed_appearance_row(
                    frame,
                    rect,
                    state,
                    appearance,
                    index + 1,
                    cursor == *cursor_index,
                    state.track_selection.contains(&appears_scope, index),
                );
            }
            _ => unreachable!("headers and gaps are rendered by render_plan"),
        },
    );
}

fn draw_fed_appearance_row(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    appearance: &crate::federation::FedAppearsOn,
    number: usize,
    selected: bool,
    visual_selected: bool,
) {
    let track = &appearance.track;
    let liked = state.fed_card_track_liked(track);
    let heart = if liked {
        Span::styled("♥ ", theme::accent_for(state))
    } else {
        Span::raw("  ")
    };
    let mut context = fed_card_track_artist_line(track);
    if !appearance.release_title.is_empty() {
        if !context.is_empty() {
            context.push_str(" · ");
        }
        context.push_str(&appearance.release_title);
    }
    let line = Line::from(vec![
        Span::styled(format!("{number:>3} "), theme::dim()),
        heart,
        fed_card_track_availability_prefix(state, track),
        Span::raw(track.title.clone()),
        Span::styled(format!("  {context}"), theme::dim()),
    ]);
    let mut meta = track
        .duration_seconds
        .map(|duration| {
            let total = duration.round() as i64;
            format!("{}:{:02}", total / 60, total % 60)
        })
        .unwrap_or_default();
    if let Some(year) = appearance.year {
        if !meta.is_empty() {
            meta.push_str(" · ");
        }
        meta.push_str(&year.to_string());
    }
    if track.sources.len() > 1 {
        if !meta.is_empty() {
            meta.push_str(" · ");
        }
        meta.push_str(&format!("{} peers", track.sources.len()));
    }
    if visual_selected && !selected {
        frame
            .buffer_mut()
            .set_style(area, theme::selection_for(state));
    }
    draw_row(
        frame,
        area,
        state,
        line,
        (!meta.is_empty()).then_some(meta),
        selected,
    );
}

fn fed_card_track_artist_line(track: &crate::federation::FedCardTrack) -> String {
    let mut main = Vec::new();
    for artist in &track.artists {
        push_display_artist(&mut main, artist);
    }
    let mut featured_names = Vec::new();
    for artist in &track.featured_artists {
        if !main
            .iter()
            .any(|name| music_dht::normalize_name(name) == music_dht::normalize_name(artist))
        {
            push_display_artist(&mut featured_names, artist);
        }
    }
    let artists = main.join(", ");
    let featured = featured_names.join(", ");
    match (artists.is_empty(), featured.is_empty()) {
        (false, false) => format!("{artists} feat. {featured}"),
        (false, true) => artists,
        (true, false) => format!("feat. {featured}"),
        (true, true) => String::new(),
    }
}

fn push_display_artist(names: &mut Vec<String>, artist: &str) {
    if !names
        .iter()
        .any(|name| music_dht::normalize_name(name) == music_dht::normalize_name(artist))
    {
        names.push(artist.to_string());
    }
}

fn draw_fed_release(frame: &mut Frame, area: Rect, state: &AppState, index: usize, cursor: usize) {
    let Some((name, Loadable::Ready(card))) = &state.fed_artist_view else {
        return centered_line(frame, area, Line::styled("no card is open", theme::dim()));
    };
    let Some(release) = card.releases.get(index) else {
        return centered_line(frame, area, Line::styled("release is gone", theme::dim()));
    };
    let inner = bordered(
        frame,
        area,
        state,
        format!(" Federation ▸ {name} ▸ {} ", release.title),
    );

    // Header: cover left; title, meta and the download button right.
    let header_height = (ART_HEADER_HEIGHT + 1).min(inner.height);
    let [header_area, content_area] =
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
        header_art(state, release.cover_path.as_ref()),
    );
    let mut meta = release.release_type.clone();
    if let Some(year) = release.year {
        meta.push_str(&format!(" · {year}"));
    }
    let visible_track_indices = fed_release_visible_track_indices(state, release);
    let track_count = visible_track_indices.len();
    if state.global.filters.source_mode.includes_network() {
        meta.push_str(&format!(
            " · {} tracks · from {} peers",
            track_count,
            release.owners.len().max(1)
        ));
    } else {
        meta.push_str(&format!(" · {track_count} tracks · local only"));
    }
    let button_style = if cursor == 0 {
        theme::tab_active_for(state)
    } else {
        theme::accent_for(state)
    };
    let action_line = if state.global.filters.source_mode.includes_network() {
        Line::styled(
            format!(" ⤓ Download the whole release ({track_count}) "),
            button_style,
        )
    } else {
        Line::styled(" Local filter is active ", button_style)
    };
    let info = vec![
        Line::default(),
        Line::styled(release.title.clone(), theme::header_for(state)),
        Line::styled(meta, theme::dim()),
        Line::default(),
        action_line,
        Line::styled(
            if state.global.filters.source_mode.includes_network() {
                "shift+v: select · y: download · p: add to playlist"
            } else {
                "shift+v: select · enter: play · p: add to playlist"
            },
            theme::dim(),
        ),
    ];
    frame.render_widget(Paragraph::new(info), info_area);

    // Tracklist: rows 1..=n of the cursor space.
    if visible_track_indices.is_empty() {
        let message = if state.global.filters.source_mode.includes_network() {
            "the peers returned no tracks"
        } else {
            "release is not available locally"
        };
        return centered_line(frame, content_area, Line::styled(message, theme::dim()));
    }
    let scope = crate::app::state::TrackSelectionScope::FedRelease(index);
    let visible = usize::from(content_area.height.max(1));
    let cursor_track = cursor.saturating_sub(1);
    let first = cursor_track
        .saturating_sub(visible / 2)
        .min(visible_track_indices.len().saturating_sub(visible));
    for (offset, (position, track_index)) in visible_track_indices
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .enumerate()
    {
        let Some(track) = release.tracks.get(*track_index) else {
            continue;
        };
        let rect = Rect {
            x: content_area.x,
            y: content_area.y + offset as u16,
            width: content_area.width,
            height: 1,
        };
        let number = track
            .track_number
            .map(|n| format!("{n:>2}. "))
            .unwrap_or_else(|| "    ".to_string());
        let duration = track
            .duration_seconds
            .map(|d| {
                let total = d.round() as i64;
                format!("{}:{:02}", total / 60, total % 60)
            })
            .unwrap_or_default();
        let right = if track.sources.len() > 1 {
            format!("{duration} · {} peers", track.sources.len())
        } else {
            duration
        };
        let in_selection = state.track_selection.contains(&scope, position);
        let liked = state.fed_card_track_liked(track);
        let heart = if liked {
            Span::styled("♥ ", theme::accent_for(state))
        } else {
            Span::raw("  ")
        };
        let line = Line::from(vec![
            heart,
            fed_card_track_availability_prefix(state, track),
            Span::raw(format!("{number}{}", track.title)),
        ]);
        if in_selection && cursor != position + 1 {
            frame
                .buffer_mut()
                .set_style(rect, theme::selection_for(state));
        }
        draw_row(
            frame,
            rect,
            state,
            line,
            Some(right),
            cursor == position + 1,
        );
    }
}
