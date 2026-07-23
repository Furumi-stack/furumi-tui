use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use super::theme;
use crate::app::state::{AppState, EditField, Loadable, Popup, addable_playlists};
use crate::library::models::{ArtistRef, TrackItem};

pub fn draw(frame: &mut Frame, state: &AppState) {
    match state.popup.as_ref() {
        Some(Popup::AddToPlaylist { target, cursor }) => {
            draw_picker(frame, state, &target.label(), *cursor)
        }
        Some(Popup::NewPlaylist { input, busy, .. }) => draw_name_entry(frame, input, *busy),
        Some(Popup::Edit {
            title,
            fields,
            focus,
            error,
            ..
        }) => draw_edit(frame, title, fields, *focus, error.as_deref()),
        Some(Popup::ConfirmDelete { label, .. }) => draw_confirm_delete(frame, label),
        Some(Popup::LibraryFilters { cursor }) => draw_library_filters(frame, state, *cursor),
        Some(Popup::TrackInfo {
            tracks,
            cursor,
            scroll,
        }) => draw_track_info(frame, tracks, *cursor, *scroll),
        Some(Popup::TrackArtists {
            tracks,
            cursor,
            selected,
            ..
        }) => draw_track_artists(frame, tracks, *cursor, *selected),
        Some(Popup::LogDetail(entry)) => draw_log_detail(frame, entry),
        Some(Popup::FedInput { field, input }) => draw_fed_input(frame, field.title(), input),
        Some(Popup::FedText { title, text }) => draw_fed_text(frame, title, text),
        Some(Popup::DevicePairing {
            device_id,
            name,
            client_version,
            requester_group_id,
            requester_group_active_devices,
            ..
        }) => draw_device_pairing(
            frame,
            device_id,
            name,
            client_version,
            requester_group_id.as_deref(),
            *requester_group_active_devices,
        ),
        Some(Popup::ConfirmDeviceRevoke { device_id, name }) => {
            draw_device_revoke(frame, device_id, name)
        }
        None => {}
    }
}

fn draw_library_filters(frame: &mut Frame, state: &AppState, cursor: usize) {
    let area = centered(frame.area(), 46, 6);
    let block = Block::bordered()
        .title(" Library filters ")
        .title_style(theme::header())
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let [list_area, _, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let checked = if state.global.filters.hide_featured_only {
        "[x]"
    } else {
        "[ ]"
    };
    let row = Rect {
        height: 1,
        ..list_area
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{checked} "), theme::accent()),
            Span::raw("Hide featured only"),
        ])),
        row,
    );
    if cursor == 0 {
        frame.buffer_mut().set_style(row, theme::tab_active());
    }
    frame.render_widget(
        Paragraph::new(Line::styled("space/enter toggle · esc close", theme::dim()))
            .alignment(Alignment::Center),
        footer,
    );
}

/// One-line text entry on the Federation tab (network id / peer ticket).
fn draw_fed_input(frame: &mut Frame, title: &str, input: &crate::app::input::LineEdit) {
    let area = centered(frame.area(), 64, 5);
    let block = Block::bordered()
        .title(format!(" {title} "))
        .title_style(theme::header())
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let [entry_area, hint_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    let spans = super::line_edit_spans(input, usize::from(entry_area.width.saturating_sub(1)));
    frame.render_widget(Paragraph::new(Line::from(spans)), entry_area);
    frame.render_widget(
        Paragraph::new(Line::styled("enter: apply · esc: cancel", theme::dim()))
            .alignment(Alignment::Center),
        hint_area,
    );
}

/// Read-only wrapped text (this peer's federation ticket).
fn draw_fed_text(frame: &mut Frame, title: &str, text: &str) {
    let width = frame.area().width.saturating_sub(8).clamp(24, 90);
    let text_width = usize::from(width.saturating_sub(2));
    let lines_needed = (text.chars().count() / text_width.max(1) + 3) as u16;
    let area = centered(
        frame.area(),
        width,
        lines_needed.clamp(5, frame.area().height),
    );
    let block = Block::bordered()
        .title(format!(" {title} "))
        .title_style(theme::header())
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(text.to_string()).wrap(Wrap { trim: false }),
        inner,
    );
}

fn draw_device_pairing(
    frame: &mut Frame,
    device_id: &str,
    name: &str,
    client_version: &str,
    requester_group_id: Option<&str>,
    requester_group_active_devices: usize,
) {
    let group_conflict = requester_group_id.is_some() && requester_group_active_devices > 1;
    let area = centered(
        frame.area(),
        if group_conflict { 76 } else { 64 },
        if group_conflict { 12 } else { 8 },
    );
    let block = Block::bordered()
        .title(" Pair device ")
        .title_style(theme::header())
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Name     ", theme::dim()),
            Span::raw(name.to_string()),
        ]),
        Line::from(vec![
            Span::styled("Version  ", theme::dim()),
            Span::raw(client_version.to_string()),
        ]),
        Line::from(vec![
            Span::styled("Device   ", theme::dim()),
            Span::raw(device_id.chars().take(24).collect::<String>()),
        ]),
    ];
    if let Some(group_id) = requester_group_id.filter(|_| group_conflict) {
        lines.extend([
            Line::from(vec![
                Span::styled("Group    ", theme::dim()),
                Span::raw(format!(
                    "{} · {requester_group_active_devices} active devices",
                    group_id.chars().take(24).collect::<String>()
                )),
            ]),
            Line::default(),
            Line::styled(
                "Recommended joins that group and keeps its peers syncing.",
                theme::dim(),
            ),
            Line::styled(
                "Cancel keeps this group; that device will switch groups and its peers may stop syncing.",
                theme::dim(),
            ),
            Line::default(),
            Line::from(vec![
                Span::styled("  Recommended  ", theme::tab_active()),
                Span::raw("  "),
                Span::styled("  Cancel  ", theme::danger_button()),
            ])
            .alignment(Alignment::Center),
            Line::styled("y recommended · c cancel · n/esc deny", theme::dim())
                .alignment(Alignment::Center),
        ]);
    } else {
        lines.extend([
            Line::default(),
            Line::styled("y accept · n/esc deny", theme::dim()),
        ]);
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_device_revoke(frame: &mut Frame, device_id: &str, name: &str) {
    let area = centered(frame.area(), 64, 8);
    let block = Block::bordered()
        .title(" Revoke device ")
        .title_style(theme::header())
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let [details, _, buttons, hint, _] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);
    let lines = vec![
        Line::from(vec![
            Span::styled("Device   ", theme::dim()),
            Span::raw(name.to_string()),
        ]),
        Line::from(vec![
            Span::styled("ID       ", theme::dim()),
            Span::raw(device_id.chars().take(24).collect::<String>()),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), details);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  Yes  ", theme::danger_button()),
            Span::raw("  "),
            Span::styled("  Cancel  ", theme::tab_active()),
        ]))
        .alignment(Alignment::Center),
        buttons,
    );
    frame.render_widget(
        Paragraph::new(Line::styled("y revoke · enter/esc cancel", theme::dim()))
            .alignment(Alignment::Center),
        hint,
    );
}

/// Metadata edit form: one bordered input per field, the focused field gets
/// the accent border and a cursor block.
fn draw_edit(
    frame: &mut Frame,
    title: &str,
    fields: &[EditField],
    focus: usize,
    error: Option<&str>,
) {
    let height = (fields.len() as u16 * 3 + 4).min(frame.area().height.saturating_sub(2));
    let area = centered(frame.area(), 60, height);
    let block = Block::bordered()
        .title(format!(" {title} "))
        .title_style(theme::header())
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let mut constraints: Vec<Constraint> = fields.iter().map(|_| Constraint::Length(3)).collect();
    constraints.push(Constraint::Min(0));
    constraints.push(Constraint::Length(1));
    let areas = Layout::vertical(constraints).split(inner);

    for (index, field) in fields.iter().enumerate() {
        let focused = index == focus;
        let field_block = Block::bordered()
            .title(field.label)
            .border_style(if focused {
                theme::accent()
            } else {
                theme::dim()
            });
        let field_inner = field_block.inner(areas[index]);
        frame.render_widget(field_block, areas[index]);
        let width = usize::from(field_inner.width);
        if focused {
            let spans = super::line_edit_spans(&field.value, width);
            frame.render_widget(Paragraph::new(Line::from(spans)), field_inner);
        } else {
            let shown: String = field
                .value
                .chars()
                .skip(field.value.chars().count().saturating_sub(width))
                .collect();
            frame.render_widget(Paragraph::new(shown), field_inner);
        }
    }

    let footer = areas[areas.len() - 1];
    let hint = match error {
        Some(error) => Line::styled(error.to_string(), theme::accent()),
        None => Line::styled("tab/↑↓ field · enter save · esc cancel", theme::dim()),
    };
    frame.render_widget(Paragraph::new(hint).alignment(Alignment::Center), footer);
}

fn draw_confirm_delete(frame: &mut Frame, label: &str) {
    let width = 64.min(frame.area().width.saturating_sub(4)).max(30);
    let area = centered(frame.area(), width, 7);
    let block = Block::bordered()
        .title(" Delete? ")
        .title_style(theme::header())
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(format!("Delete {label}?"))
            .wrap(Wrap { trim: false })
            .alignment(Alignment::Center),
        body,
    );
    frame.render_widget(
        Paragraph::new(Line::styled("enter/y delete · esc/n cancel", theme::dim()))
            .alignment(Alignment::Center),
        footer,
    );
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

    let can_share = crate::share::track_can_share(track);
    let hint = if tracks.len() > 1 && can_share {
        "j/k scroll · h/left previous · l/right next · a artist · c copy frid link · esc close"
    } else if tracks.len() > 1 {
        "j/k scroll · h/left previous · l/right next · a artist · esc close"
    } else if can_share {
        "j/k scroll · a artist · c copy frid link · esc close"
    } else {
        "j/k scroll · a artist · esc close"
    };
    frame.render_widget(
        Paragraph::new(Line::styled(hint, theme::dim())).alignment(Alignment::Center),
        footer,
    );
}

fn draw_track_artists(frame: &mut Frame, tracks: &[TrackItem], cursor: usize, selected: usize) {
    let Some(track) = tracks.get(cursor.min(tracks.len().saturating_sub(1))) else {
        return;
    };
    let artists = crate::app::update::track_artist_refs(track);
    // Main artists form the prefix of the deduplicated list; everything
    // after them came from the featured credits.
    let featured_from = artists
        .iter()
        .take_while(|kept| {
            track
                .artists
                .iter()
                .any(|main| main.id == kept.id && main.name == kept.name)
        })
        .count();
    let height = (artists.len() as u16 + 4)
        .min(frame.area().height.saturating_sub(2))
        .max(6);
    let area = centered(frame.area(), 44, height);

    let block = Block::bordered()
        .title(" Open artist ")
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

    let visible = usize::from(list_area.height.max(1));
    let first = selected
        .saturating_sub(visible / 2)
        .min(artists.len().saturating_sub(visible));
    for (index, artist) in artists.iter().enumerate().skip(first).take(visible) {
        let row = Rect {
            x: list_area.x,
            y: list_area.y + (index - first) as u16,
            width: list_area.width,
            height: 1,
        };
        let line = if index >= featured_from {
            Line::from(vec![
                Span::raw(artist.name.clone()),
                Span::styled("  feat.", theme::dim()),
            ])
        } else {
            Line::raw(artist.name.clone())
        };
        frame.render_widget(Paragraph::new(line), row);
        if index == selected {
            frame.buffer_mut().set_style(row, theme::tab_active());
        }
    }

    frame.render_widget(
        Paragraph::new(Line::styled(
            "enter open card · esc back to info",
            theme::dim(),
        ))
        .alignment(Alignment::Center),
        footer,
    );
}

fn track_info_lines(track: &TrackItem) -> Vec<Line<'static>> {
    let mut lines = vec![
        field("ID", row_id(track.id)),
        field("Title", track.title.clone()),
        field("Artists", artist_refs(&track.artists)),
        field("Featured artists", artist_refs(&track.featured_artists)),
        field("Release", release_label(track)),
        field("Release ID", row_id(track.release_id)),
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
        field("Plays", track.play_count.to_string()),
        field(
            "Content ID",
            crate::share::track_content_id(track).unwrap_or_else(|| {
                if !track.file_path.trim().is_empty() {
                    "press c to compute".to_string()
                } else {
                    "—".to_string()
                }
            }),
        ),
        field("File path", empty_dash(&track.file_path)),
        field("Cover path", opt_string(track.cover_path.clone())),
    ];
    if let Some(link) = crate::share::cached_track_share_link(track) {
        lines.push(field("Share link", link));
    }
    lines
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
        .map(|artist| {
            if artist.id >= 0 {
                format!("{} ({})", artist.name, artist.id)
            } else {
                artist.name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn row_id(id: i64) -> String {
    if id >= 0 {
        id.to_string()
    } else {
        "—".to_string()
    }
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
    let loading = matches!(&state.playlists.list, None | Some(Loadable::Loading));
    let rows = if loading {
        1
    } else if options.is_empty() {
        2
    } else {
        options.len() + 1
    };
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

    let mut lines: Vec<Line> = Vec::new();
    if loading {
        lines.push(Line::styled("loading playlists…", theme::dim()));
    } else if matches!(&state.playlists.list, Some(Loadable::Failed(_))) {
        lines.push(Line::styled("playlist list unavailable", theme::dim()));
        lines.push(Line::styled("+ New playlist…", theme::accent()));
    } else if options.is_empty() {
        lines.push(Line::styled("no playlists yet", theme::dim()));
        lines.push(Line::styled("+ New playlist…", theme::accent()));
    } else {
        for (_, title) in &options {
            lines.push(Line::raw(title.clone()));
        }
        lines.push(Line::styled("+ New playlist…", theme::accent()));
    }
    let selected_line = if loading {
        0
    } else if options.is_empty() {
        lines.len().saturating_sub(1)
    } else {
        cursor.min(options.len())
    };
    let visible = usize::from(list_area.height.max(1));
    let first = selected_line
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
        if index == selected_line {
            frame.buffer_mut().set_style(row, theme::tab_active());
        }
    }

    let footer_text = if loading {
        format!("♪ {track_title} · loading playlists · esc close")
    } else {
        format!("♪ {track_title} · enter add/create · esc close")
    };
    frame.render_widget(
        Paragraph::new(Line::styled(footer_text, theme::dim())).alignment(Alignment::Center),
        footer,
    );
}

fn draw_name_entry(frame: &mut Frame, input: &crate::app::input::LineEdit, busy: bool) {
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
    let spans = super::line_edit_spans(input, usize::from(name_inner.width));
    frame.render_widget(Paragraph::new(Line::from(spans)), name_inner);

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
