pub mod art;
mod global;
mod login;
mod logs;
mod playlists;
pub mod theme;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Row, Table, Tabs};

use crate::app::state::{AppState, Screen, Tab};
use crate::config::keymap::Keymap;

pub fn draw(frame: &mut Frame, state: &AppState, keymap: &Keymap) {
    if state.screen == Screen::Login {
        login::draw(frame, &state.login);
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
        Tab::Devices => draw_main(frame, main_area, state),
        Tab::Logs => logs::draw(frame, main_area, state),
    }
    draw_status(frame, status_area, state);

    if state.help_visible {
        draw_help(frame, keymap);
    }
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

fn draw_main(frame: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::bordered()
        .title(format!(" {} ", state.active_tab.title()))
        .title_style(theme::header())
        .border_style(theme::dim());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (summary, milestone) = match state.active_tab {
        Tab::Devices => ("Connected devices and playback transfer", "milestone 5"),
        _ => ("", ""),
    };
    let lines = vec![
        Line::default(),
        Line::styled(summary, theme::accent()),
        Line::styled(format!("coming in {milestone}"), theme::dim()),
        Line::default(),
        Line::styled("Tab / Shift-Tab or 1-5 to switch tabs", theme::dim()),
        Line::styled("?  keybindings    q  quit", theme::dim()),
    ];
    let paragraph = Paragraph::new(lines).alignment(Alignment::Center);
    frame.render_widget(paragraph, centered_vertically(inner, 6));
}

/// One track row used by every track list: ♥ marker for liked tracks, the
/// title and artists on the left, tech info and duration on the right.
pub(crate) fn track_row(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    track: &crate::api::models::TrackItem,
    index_label: String,
    selected: bool,
) {
    let heart = if state.likes.contains(&track.id) {
        Span::styled("♥ ", theme::accent())
    } else {
        Span::raw("  ")
    };
    let line = Line::from(vec![
        Span::styled(format!("{index_label:>3} "), theme::dim()),
        heart,
        Span::raw(track.title.clone()),
        Span::styled(format!("  {}", track.artist_line()), theme::dim()),
    ]);
    frame.render_widget(Paragraph::new(line), area);

    let tech = track.tech_label_short();
    let right = if tech.is_empty() || area.width < 60 {
        track.duration_label()
    } else {
        format!("{tech} · {}", track.duration_label())
    };
    frame.render_widget(
        Paragraph::new(Line::styled(right, theme::dim())).alignment(Alignment::Right),
        area,
    );
    if selected {
        frame.buffer_mut().set_style(area, theme::tab_active());
    }
}

/// Read-only queue listing; the playing track is highlighted.
fn draw_queue(frame: &mut Frame, area: Rect, state: &AppState) {
    let player = &state.player;
    let block = Block::bordered()
        .title(format!(" Queue — {} tracks ", player.queue.len()))
        .title_style(theme::header())
        .border_style(theme::dim());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if player.queue.is_empty() {
        let middle = Rect { y: inner.y + inner.height / 2, height: 1, ..inner };
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

    let visible = usize::from(inner.height.max(1));
    let first = player
        .queue_pos
        .saturating_sub(visible / 2)
        .min(player.queue.len().saturating_sub(visible));
    for (index, track) in player.queue.iter().enumerate().skip(first).take(visible) {
        let row = Rect {
            x: inner.x,
            y: inner.y + (index - first) as u16,
            width: inner.width,
            height: 1,
        };
        track_row(
            frame,
            row,
            state,
            track,
            (index + 1).to_string(),
            index == player.queue_pos,
        );
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
    if let Some(track) = &player.current {
        if player.playing {
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
    }
    if width >= 80 {
        let volume_cells = usize::from(player.volume / 10);
        spans.extend([
            Span::styled("  vol ", theme::dim()),
            Span::styled("█".repeat(volume_cells), theme::accent()),
            Span::styled("░".repeat(10 - volume_cells), theme::dim()),
            Span::raw(format!(" {:3}%", player.volume)),
            Span::styled("  shuffle ", theme::dim()),
            Span::raw(if player.shuffle { "on" } else { "off" }.to_string()),
            Span::styled("  repeat ", theme::dim()),
            Span::raw(player.repeat.label().to_string()),
        ]);
    } else {
        spans.push(Span::styled(
            format!("  {}%", player.volume),
            theme::dim(),
        ));
    }
    Line::from(spans)
}

fn draw_status(frame: &mut Frame, area: Rect, state: &AppState) {
    let [player_row, message_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    let player = &state.player;
    // Layout: track title left, time/progress/flags centered, user right.
    // The center block is built first and gets a fixed width; the title
    // truncates into whatever is left.
    let center = player_right_line(player, area.width);
    let center_width = (center.width() as u16).min(area.width);
    let user_line = state.user.as_ref().map(|user| {
        Line::from(vec![
            Span::styled("◉ ", theme::accent()),
            Span::raw(user.name.clone()),
        ])
    });
    let user_width = user_line.as_ref().map_or(0, |l| l.width() as u16);
    let [title_area, right_area, user_area] = Layout::horizontal([
        Constraint::Min(8),
        Constraint::Length(center_width),
        Constraint::Length(user_width),
    ])
    .areas(player_row);
    if let Some(user_line) = user_line {
        frame.render_widget(
            Paragraph::new(user_line).alignment(Alignment::Right),
            user_area,
        );
    }

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
        let line = Line::from(vec![
            Span::styled(":", theme::header()),
            Span::raw(state.cmdline.input.clone()),
            Span::styled("█", theme::accent()),
        ]);
        frame.render_widget(Paragraph::new(line), message_row);
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

    if let Some(pending) = &state.pending_keys {
        let pending = Paragraph::new(Line::styled(format!("{pending} …"), theme::header()))
            .alignment(Alignment::Right);
        frame.render_widget(pending, message_row);
    }
}

fn draw_help(frame: &mut Frame, keymap: &Keymap) {
    let entries = keymap.help_entries();
    let height = (entries.len() as u16 + 4).min(frame.area().height.saturating_sub(2));
    let width = 56.min(frame.area().width.saturating_sub(2));
    let area = centered_rect(frame.area(), width, height);

    let rows = entries.into_iter().map(|(keys, description, context)| {
        Row::new(vec![
            Span::styled(keys, theme::accent()),
            Span::raw(description),
            Span::styled(context.label(), theme::dim()),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Min(20),
            Constraint::Length(9),
        ],
    )
    .header(Row::new(vec!["keys", "action", "context"]).style(theme::header()))
    .block(
        Block::bordered()
            .title(" Keybindings ")
            .title_style(theme::header()),
    );

    frame.render_widget(Clear, area);
    frame.render_widget(table, area);
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

fn centered_vertically(area: Rect, content_height: u16) -> Rect {
    let [rect] = Layout::vertical([Constraint::Length(content_height)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    rect
}
