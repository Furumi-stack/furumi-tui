pub mod art;
mod federation;
mod global;
mod logs;
mod playlists;
mod popup;
pub mod theme;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Tabs};

use crate::app::input::LineEdit;
use crate::app::state::{AppState, Tab, TrackSelectionScope};
use crate::config::keymap::Keymap;

pub fn draw(frame: &mut Frame, state: &AppState, keymap: &Keymap) {
    if state.visualizer.active {
        crate::visualizer::draw(frame, state);
        return;
    }

    let [tabs_area, main_area, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    draw_tabs(frame, tabs_area, state);
    match state.active_tab {
        Tab::Global => global::draw(frame, main_area, state),
        Tab::Playlists => playlists::draw(frame, main_area, state),
        Tab::Queue => draw_queue(frame, main_area, state),
        Tab::Federation => federation::draw(frame, main_area, state),
        Tab::Logs => logs::draw(frame, main_area, state),
    }
    draw_status(frame, status_area, state);

    if state.help_visible {
        draw_help(frame, keymap);
    }
    popup::draw(frame, state);
}

fn draw_tabs(frame: &mut Frame, area: Rect, state: &AppState) {
    let titles = Tab::ALL
        .iter()
        .map(|tab| format!(" {} {} ", tab.index() + 1, tab.title()));
    let tabs = Tabs::new(titles)
        .select(state.active_tab.index())
        .style(theme::dim())
        .highlight_style(theme::tab_active())
        .divider("");
    frame.render_widget(tabs, area);
}

/// One track row used by every track list: ♥ marker for liked tracks, the
/// title and artists on the left, tech info and duration on the right.
pub(crate) fn track_row(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    track: &crate::library::models::TrackItem,
    index_label: String,
    selected: bool,
    visual_selected: bool,
) {
    let fed_liked = track
        .fed
        .as_ref()
        .is_some_and(|fed| state.fed_track_liked(fed));
    let heart = if state.likes.contains(&track.id) || fed_liked {
        Span::styled("♥ ", theme::accent())
    } else {
        Span::raw("  ")
    };
    let fed_marker = if track.fed.is_some() {
        Span::styled("⇅ ", theme::accent())
    } else {
        Span::raw("")
    };
    let line = Line::from(vec![
        Span::styled(format!("{index_label:>3} "), theme::dim()),
        heart,
        fed_marker,
        Span::raw(track.title.clone()),
        Span::styled(format!("  {}", track.artist_line()), theme::dim()),
    ]);
    frame.render_widget(Paragraph::new(line), area);

    let right = track_meta_suffix(track, area.width >= 60);
    frame.render_widget(
        Paragraph::new(Line::styled(right, theme::dim())).alignment(Alignment::Right),
        area,
    );
    if visual_selected {
        frame.buffer_mut().set_style(area, theme::selection());
    }
    if selected {
        frame.buffer_mut().set_style(area, theme::tab_active());
    }
}

pub(crate) fn track_meta_suffix(
    track: &crate::library::models::TrackItem,
    include_tech: bool,
) -> String {
    let has_tech = track.audio_format.is_some()
        || track.audio_bitrate.is_some()
        || track.file_size_bytes.is_some();
    if !include_tech || !has_tech {
        return track.duration_label();
    }

    let format = track
        .audio_format
        .as_deref()
        .map(|value| value.to_ascii_uppercase())
        .unwrap_or_default();
    let format: String = format.chars().take(4).collect();
    let bitrate = track
        .audio_bitrate
        .map(|value| format!("{value}k"))
        .unwrap_or_default();
    let size = track
        .file_size_bytes
        .map(|bytes| format!("{:.1}MB", bytes as f64 / 1_048_576.0))
        .unwrap_or_default();

    format!(
        "{format:<4} {bitrate:>5} {size:>8} · {:>5}",
        track.duration_label()
    )
}

/// Interactive queue: its own cursor, enter plays the selected track and
/// already-played tracks stay listed, greyed out.
fn draw_queue(frame: &mut Frame, area: Rect, state: &AppState) {
    let player = &state.player;
    let block = Block::bordered()
        .title(format!(
            " Queue — {} tracks · enter: play · d: remove · shift-v: select · shift-c: clear ",
            player.queue.len()
        ))
        .title_style(theme::header())
        .border_style(theme::dim());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if player.queue.is_empty() {
        let middle = Rect {
            y: inner.y + inner.height / 2,
            height: 1,
            ..inner
        };
        frame.render_widget(
            Paragraph::new(Line::styled(
                "queue is empty — open a track and press enter",
                theme::dim(),
            ))
            .alignment(Alignment::Center),
            middle,
        );
        return;
    }

    let cursor = state.queue_tab.cursor.min(player.queue.len() - 1);
    let visible = usize::from(inner.height.max(1));
    let first = cursor
        .saturating_sub(visible / 2)
        .min(player.queue.len().saturating_sub(visible));
    let played_style = Style::new().fg(Color::DarkGray).bg(Color::Rgb(28, 28, 32));
    for (index, track) in player.queue.iter().enumerate().skip(first).take(visible) {
        let row = Rect {
            x: inner.x,
            y: inner.y + (index - first) as u16,
            width: inner.width,
            height: 1,
        };
        let label = if index == player.queue_pos && player.playing {
            "▶".to_string()
        } else {
            (index + 1).to_string()
        };
        let visual_selected = state
            .track_selection
            .contains(&TrackSelectionScope::Queue, index);
        track_row(
            frame,
            row,
            state,
            track,
            label,
            index == cursor,
            visual_selected,
        );
        // Tracks before the playing one are history: greyed out unless the
        // cursor is on them.
        if index < player.queue_pos && index != cursor && !visual_selected {
            frame.buffer_mut().set_style(row, played_style);
        }
    }
}

fn format_secs(secs: f64) -> String {
    let total = secs.max(0.0).round() as i64;
    format!("{}:{:02}", total / 60, total % 60)
}

/// Playback time, progress bar, queue position, volume and mode flags.
/// Wider consoles get a longer bar and full flags; narrow ones drop pieces.
fn player_right_line(player: &crate::app::state::PlayerBar, width: u16) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(track) = &player.current
        && player.playing
    {
        let bar_width: usize = match width {
            0..=59 => 0,
            60..=79 => 8,
            80..=109 => 14,
            _ => 22,
        };
        spans.push(Span::raw(format!("{} ", format_secs(player.position_secs))));
        if bar_width > 0 && track.duration_seconds > 0.0 {
            let ratio = (player.position_secs / track.duration_seconds).clamp(0.0, 1.0);
            let filled = (ratio * bar_width as f64).round() as usize;
            spans.push(Span::styled("━".repeat(filled), theme::accent()));
            spans.push(Span::styled("─".repeat(bar_width - filled), theme::dim()));
            spans.push(Span::raw(" "));
        } else {
            spans.push(Span::styled("/ ", theme::dim()));
        }
        spans.push(Span::raw(track.duration_label()));
        if !player.queue.is_empty() && width >= 70 {
            spans.push(Span::styled(
                format!(" [{}/{}]", player.queue_pos + 1, player.queue.len()),
                theme::dim(),
            ));
        }
    }
    if width >= 80 {
        let volume_cells = usize::from(player.volume / 10);
        spans.extend([
            Span::styled("  vol ", theme::dim()),
            Span::styled("█".repeat(volume_cells), theme::accent()),
            Span::styled("░".repeat(10 - volume_cells), theme::dim()),
            Span::raw(format!(" {:3}%", player.volume)),
            Span::raw("  "),
        ]);
        // Enabled modes light up as filled chips; disabled stay dim text.
        if player.shuffle {
            spans.push(Span::styled(" shuffle ", theme::tab_active()));
        } else {
            spans.push(Span::styled("shuffle off", theme::dim()));
        }
        spans.push(Span::raw("  "));
        if player.repeat == crate::app::state::RepeatMode::Off {
            spans.push(Span::styled("repeat off", theme::dim()));
        } else {
            spans.push(Span::styled(
                format!(" repeat {} ", player.repeat.label()),
                theme::tab_active(),
            ));
        }
    } else {
        spans.push(Span::styled(format!("  {}%", player.volume), theme::dim()));
    }
    // Keep a gap between the flags and the username block to the right.
    spans.push(Span::raw("  "));
    Line::from(spans)
}

fn draw_status(frame: &mut Frame, area: Rect, state: &AppState) {
    let [player_row, message_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    let player = &state.player;
    // Layout: track title left, time/progress/flags on the right. The
    // right block is built first and gets a fixed width; the title
    // truncates into whatever is left.
    let center = player_right_line(player, area.width);
    let center_width = (center.width() as u16).min(area.width);
    let [title_area, right_area] =
        Layout::horizontal([Constraint::Min(8), Constraint::Length(center_width)])
            .areas(player_row);

    let mut spans = Vec::new();
    match &player.current {
        Some(track) if player.playing => {
            if player.paused {
                spans.push(Span::styled("⏸ ", theme::dim()));
            } else {
                spans.push(Span::styled("▶ ", theme::accent()));
            }
            if state.likes.contains(&track.id) {
                spans.push(Span::styled("♥ ", theme::accent()));
            }
            spans.push(Span::raw(track.title.clone()));
            spans.push(Span::styled(
                format!(" — {}", track.artist_line()),
                theme::dim(),
            ));
        }
        _ => {
            spans.push(Span::styled("■ stopped", theme::dim()));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), title_area);
    frame.render_widget(Paragraph::new(center), right_area);

    if state.cmdline.active {
        // Vim-style command line takes over the message row.
        let mut spans = vec![Span::styled(":", theme::header())];
        spans.extend(line_edit_spans(
            &state.cmdline.input,
            usize::from(message_row.width.saturating_sub(2)),
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), message_row);
        draw_version(frame, message_row);
        return;
    }

    let message = match &state.status_message {
        Some(message) => Line::styled(message.clone(), theme::accent()),
        None => match &state.player.current {
            // Idle line doubles as the current track's tech data display.
            Some(track) if state.player.playing && !track.tech_label_full().is_empty() => {
                Line::styled(track.tech_label_full(), theme::dim())
            }
            _ => Line::styled("press ? for keybindings", theme::dim()),
        },
    };
    frame.render_widget(Paragraph::new(message), message_row);
    draw_version(frame, message_row);

    if let Some(pending) = &state.pending_keys {
        let pending = Paragraph::new(Line::styled(format!("{pending} …"), theme::header()))
            .alignment(Alignment::Right);
        frame.render_widget(pending, message_row);
    }
}

fn draw_version(frame: &mut Frame, area: Rect) {
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    frame.render_widget(
        Paragraph::new(Line::styled(version, theme::dim())).alignment(Alignment::Right),
        area,
    );
}

/// Help window: bindings merged per action (j / down on one row), grouped
/// into titled sections and laid out in two balanced columns.
fn draw_help(frame: &mut Frame, keymap: &Keymap) {
    use crate::app::action::{Action, Category};
    use crate::config::keymap::KeyContext;

    struct MergedRow {
        keys: Vec<String>,
        action: Action,
        context: KeyContext,
    }
    let mut merged: Vec<MergedRow> = Vec::new();
    for (keys, action, context) in keymap.help_entries() {
        match merged
            .iter_mut()
            .find(|row| row.action == action && row.context == context)
        {
            Some(row) => row.keys.push(keys),
            None => merged.push(MergedRow {
                keys: vec![keys],
                action,
                context,
            }),
        }
    }

    // One block of lines per category: section header + its rows.
    let mut blocks: Vec<Vec<Line>> = Vec::new();
    for category in Category::ALL {
        let rows: Vec<&MergedRow> = merged
            .iter()
            .filter(|row| row.action.category() == category)
            .collect();
        if rows.is_empty() {
            continue;
        }
        let mut lines = vec![Line::styled(category.title(), theme::header())];
        for row in rows {
            let keys = row.keys.join(" / ");
            let context = if row.context == KeyContext::Global {
                String::new()
            } else {
                format!(" [{}]", row.context.label())
            };
            let command = row.action.command_hint().unwrap_or("");
            lines.push(Line::from(vec![
                Span::styled(format!("{keys:<13}"), theme::accent()),
                Span::raw(format!(
                    "{:<24}",
                    format!("{}{context}", row.action.describe())
                )),
                Span::styled(command.to_string(), theme::accent()),
            ]));
        }
        lines.push(Line::default());
        blocks.push(lines);
    }

    // Balance the blocks across two columns.
    let total: usize = blocks.iter().map(Vec::len).sum();
    let mut left: Vec<Line> = Vec::new();
    let mut right: Vec<Line> = Vec::new();
    for block in blocks {
        if left.len() < total.div_ceil(2) {
            left.extend(block);
        } else {
            right.extend(block);
        }
    }

    let column_height = left.len().max(right.len()) as u16;
    let width = 110.min(frame.area().width.saturating_sub(2));
    let height = (column_height + 4).min(frame.area().height.saturating_sub(2));
    let area = centered_rect(frame.area(), width, height);

    let block = Block::bordered()
        .title(" Keybindings & commands ")
        .title_style(theme::header())
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let [columns_area, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let [left_area, _, right_area] = Layout::horizontal([
        Constraint::Percentage(50),
        Constraint::Length(2),
        Constraint::Percentage(50),
    ])
    .areas(columns_area);
    frame.render_widget(Paragraph::new(left), left_area);
    frame.render_widget(Paragraph::new(right), right_area);
    frame.render_widget(
        Paragraph::new(Line::styled(
            ": opens the command line · full forms: :seek +30|1:30 · :volume 0-100 · :repeat off|one|all · :logs [level]",
            theme::dim(),
        ))
        .alignment(Alignment::Center),
        footer,
    );
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let [rect] = Layout::horizontal([Constraint::Length(width)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [rect] = Layout::vertical([Constraint::Length(height)])
        .flex(ratatui::layout::Flex::Center)
        .areas(rect);
    rect
}

/// Renders a [`LineEdit`] as spans with a visible cursor, windowed so the
/// cursor always stays on screen when the value is wider than `width`.
pub(crate) fn line_edit_spans(edit: &LineEdit, width: usize) -> Vec<Span<'static>> {
    let width = width.max(2);
    let chars: Vec<char> = edit.as_str().chars().collect();
    let cursor = edit.cursor().min(chars.len());
    // Window start: keep the cursor within the visible slice (one cell is
    // reserved for the cursor block itself when it sits at the end).
    let start = (cursor + 1).saturating_sub(width);
    let end = (start + width.saturating_sub(1)).min(chars.len());
    let before: String = chars[start..cursor].iter().collect();
    let (under, after): (String, String) = if cursor < chars.len() {
        (
            chars[cursor].to_string(),
            chars[cursor + 1..end.max(cursor + 1)].iter().collect(),
        )
    } else {
        ("█".to_string(), String::new())
    };
    let cursor_style = if cursor < chars.len() {
        ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::REVERSED)
    } else {
        theme::accent()
    };
    vec![
        Span::raw(before),
        Span::styled(under, cursor_style),
        Span::raw(after),
    ]
}
