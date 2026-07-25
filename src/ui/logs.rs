use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::theme;
use crate::app::state::{AppState, LOG_LEVELS};
use crate::config::logging;

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState) {
    let logs = &state.logs;
    let level = LOG_LEVELS[logs.level_index];
    let mode = if logs.follow { "follow" } else { "scroll" };
    let block = Block::bordered()
        .title(format!(" Logs — {level}+ · {mode} "))
        .title_style(theme::header_for(state))
        .border_style(theme::border_for(state));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(buffer) = logging::buffer() else {
        return centered(frame, inner, "in-app log buffer unavailable");
    };

    let visible = usize::from(inner.height.max(1));
    let selected = if logs.follow { None } else { logs.selected_seq };
    let view = buffer.view(level, selected, visible);
    if view.entries.is_empty() {
        return centered(frame, inner, "no log entries at this level yet");
    }

    for (row_index, entry) in view.entries.iter().enumerate() {
        let row = Rect {
            x: inner.x,
            y: inner.y + row_index as u16,
            width: inner.width,
            height: 1,
        };
        let line = Line::from(vec![
            Span::styled(format!("{} ", entry.time), theme::dim()),
            level_span(entry.level, state),
            Span::styled(format!(" {}: ", short_target(&entry.target)), theme::dim()),
            Span::raw(entry.message.clone()),
        ]);
        frame.render_widget(Paragraph::new(line), row);
        if view.cursor_row == Some(row_index) {
            frame
                .buffer_mut()
                .set_style(row, theme::tab_active_for(state));
        }
    }

    // Footer hint with position info while scrolled back.
    if !logs.follow && inner.height > 1 {
        let footer = Rect {
            y: inner.y + inner.height - 1,
            height: 1,
            ..inner
        };
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(
                    " ↑{} of {} · enter: details · shift-g: follow · v: level ",
                    view.from_end, view.matched
                ),
                theme::tab_active_for(state),
            ))
            .alignment(Alignment::Right),
            footer,
        );
    }
}

fn centered(frame: &mut Frame, area: Rect, text: &str) {
    if area.height == 0 {
        return;
    }
    let middle = Rect {
        y: area.y + area.height / 2,
        height: 1,
        ..area
    };
    frame.render_widget(
        Paragraph::new(Line::styled(text.to_string(), theme::dim())).alignment(Alignment::Center),
        middle,
    );
}

fn level_span(level: tracing::Level, state: &AppState) -> Span<'static> {
    match level {
        tracing::Level::ERROR => Span::styled(
            "ERROR",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        tracing::Level::WARN => Span::styled("WARN ", Style::new().fg(Color::Yellow)),
        tracing::Level::INFO => Span::styled("INFO ", theme::accent_for(state)),
        tracing::Level::DEBUG => Span::styled("DEBUG", theme::dim()),
        tracing::Level::TRACE => Span::styled("TRACE", theme::dim()),
    }
}

/// `furumi_tui::app::update` → `app::update` — the crate prefix is noise.
fn short_target(target: &str) -> &str {
    target.split_once("::").map_or(target, |(_, rest)| rest)
}
