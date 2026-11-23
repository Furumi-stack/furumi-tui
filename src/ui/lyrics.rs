use crate::app::{
    lyrics::{Editor, Reader},
    state::{AppState, Popup},
};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthChar;

use super::theme;

fn popup_area(screen: Rect) -> Rect {
    let width = screen.width.saturating_sub(4).min(96);
    let height = screen.height.saturating_sub(4).min(30);
    Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    )
}

pub(crate) fn prepare_reader(state: &mut AppState, screen: Rect) {
    if let Some(Popup::Lyrics(reader)) = &mut state.popup {
        let area = popup_area(screen);
        reader.viewport_height = usize::from(area.height.saturating_sub(4));
        let width = usize::from(area.width.saturating_sub(4)).max(1);
        let key = (state.lyrics.document_revision, width);
        if reader.layout_key != Some(key) {
            reader.layout_key = Some(key);
            reader.rows.clear();
            for (index, line) in state.lyrics.document.lines.iter().enumerate() {
                let mut row = String::new();
                let mut columns = 0;
                for ch in line.chars() {
                    let char_width = ch.width().unwrap_or_default();
                    if columns + char_width > width && !row.is_empty() {
                        reader.rows.push((index, std::mem::take(&mut row)));
                        columns = 0;
                    }
                    row.push(ch);
                    columns += char_width;
                }
                reader.rows.push((index, row));
            }
        }
    }
}

fn shell(frame: &mut Frame, state: &AppState, title: String) -> Rect {
    let area = popup_area(frame.area());
    let block = Block::bordered()
        .title(title)
        .border_style(theme::strong_border_for(state))
        .title_style(theme::header_for(state));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    inner
}

fn row(area: Rect, index: u16) -> Rect {
    Rect::new(
        area.x,
        area.y.saturating_add(index),
        area.width,
        u16::from(index < area.height),
    )
}

pub(super) fn draw_reader(frame: &mut Frame, state: &AppState, reader: &Reader) {
    let playback = &state.lyrics;
    let title = if playback.title.is_empty() {
        " Lyrics ".into()
    } else {
        format!(" Lyrics · {} ", playback.title)
    };
    let inner = shell(frame, state, title);
    if inner.height < 3 || inner.width < 4 {
        return;
    }
    let body = Rect::new(inner.x + 1, inner.y + 1, inner.width - 2, inner.height - 2);
    let active = playback.document.line_at(state.player.position_secs);
    let version = playback.versions.get(playback.selected);
    if let Some(version) = version {
        let label = crate::lyrics::clean_text(&version.label).replace('\n', " ");
        let loading = if playback.loading {
            "  ·  Looking for versions…"
        } else {
            ""
        };
        frame.render_widget(
            Paragraph::new(format!(
                "{} / {}  ·  {label}{loading}",
                playback.selected + 1,
                playback.versions.len()
            ))
            .alignment(Alignment::Center)
            .style(theme::dim()),
            row(inner, 0),
        );
    }
    if reader.rows.is_empty() {
        let message = if playback.title.is_empty() {
            "No music track selected".into()
        } else if playback.document.instrumental {
            "Instrumental".into()
        } else if playback.loading {
            format!("{}  Looking for lyrics…", state.spinner())
        } else {
            "Could not find lyrics".into()
        };
        frame.render_widget(
            Paragraph::new(message)
                .alignment(Alignment::Center)
                .style(theme::accent_for(state)),
            row(body, body.height / 2),
        );
    } else {
        let visible = usize::from(body.height);
        let start = reader
            .top
            .unwrap_or_else(|| reader.follow_top(active))
            .min(reader.rows.len().saturating_sub(visible));
        for (offset, (index, text)) in reader.rows.iter().skip(start).take(visible).enumerate() {
            let style = if Some(*index) == active {
                theme::header_for(state)
            } else if playback.document.timed() {
                theme::dim()
            } else {
                ratatui::style::Style::default()
            };
            frame.render_widget(
                Paragraph::new(text.as_str())
                    .alignment(Alignment::Center)
                    .style(style),
                row(body, offset as u16),
            );
        }
    }
    let footer = if playback.document.timed() {
        "↑↓ / PgUp PgDn scroll  ·  f follow  ·  ←→ version  ·  esc close"
    } else {
        "↑↓ / PgUp PgDn scroll  ·  ←→ version  ·  esc close"
    };
    frame.render_widget(
        Paragraph::new(footer)
            .alignment(Alignment::Center)
            .style(theme::dim()),
        row(inner, inner.height - 1),
    );
}

pub(super) fn draw_editor(frame: &mut Frame, state: &AppState, editor: &Editor) {
    let inner = shell(frame, state, " Track lyrics ".into());
    if inner.height < 4 || inner.width < 4 {
        return;
    }
    let body = Rect::new(inner.x + 1, inner.y, inner.width - 2, inner.height - 1);
    let mut footer = if editor.busy {
        "Working…"
    } else {
        "↑↓ select  ·  a add  ·  enter edit  ·  d delete  ·  esc back"
    };
    if let Some(draft) = &editor.draft {
        if !editor.busy {
            footer =
                "Tab label/text  ·  paste plain text or LRC  ·  Ctrl+S / F2 save  ·  esc cancel";
        }
        let label = if draft.label_focused {
            super::line_edit_spans(&draft.label, usize::from(body.width.saturating_sub(7)))
        } else {
            vec![Span::raw(draft.label.as_str())]
        };
        let mut label_line = vec![Span::styled("Label  ", theme::accent_for(state))];
        label_line.extend(label);
        frame.render_widget(Paragraph::new(Line::from(label_line)), row(body, 0));

        let (cursor_row, cursor_col) = draft.text.line_cursor();
        let height = usize::from(body.height - 1);
        let start = cursor_row.saturating_sub(height.saturating_sub(1));
        for (offset, text) in draft.text.split('\n').skip(start).take(height).enumerate() {
            let line = if !draft.label_focused && start + offset == cursor_row {
                let edit = crate::app::input::LineEdit::with_cursor(text, cursor_col);
                Line::from(super::line_edit_spans(&edit, usize::from(body.width)))
            } else {
                Line::from(text)
            };
            frame.render_widget(Paragraph::new(line), row(body, offset as u16 + 1));
        }
    } else if editor.confirm_delete {
        footer = "Enter / y delete  ·  any other key cancels";
        let label = editor
            .versions
            .get(editor.cursor)
            .map(|version| version.label.as_str())
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(format!("Delete {}?", crate::lyrics::clean_text(label)))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            body,
        );
    } else if editor.versions.is_empty() {
        let message = if editor.busy {
            format!("{}  Loading lyrics…", state.spinner())
        } else {
            "No saved lyrics. Press a to add a version.".into()
        };
        frame.render_widget(
            Paragraph::new(message).alignment(Alignment::Center),
            row(body, body.height / 2),
        );
    } else {
        let start = editor
            .cursor
            .saturating_sub(usize::from(body.height).saturating_sub(1));
        for (offset, version) in editor
            .versions
            .iter()
            .skip(start)
            .take(usize::from(body.height))
            .enumerate()
        {
            let synced = if version.synced_lrc.is_some() {
                "Timed"
            } else if version.instrumental {
                "Instrumental"
            } else {
                "Plain"
            };
            let label = crate::lyrics::clean_text(&version.label).replace('\n', " ");
            let style = if start + offset == editor.cursor {
                theme::header_for(state)
            } else {
                theme::dim()
            };
            frame.render_widget(
                Paragraph::new(format!("{label}  ·  {synced}")).style(style),
                row(body, offset as u16),
            );
        }
    }
    frame.render_widget(
        Paragraph::new(editor.error.as_deref().unwrap_or(footer)).style(theme::accent_for(state)),
        row(inner, inner.height - 1),
    );
}
