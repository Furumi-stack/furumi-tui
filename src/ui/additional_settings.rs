//! Secondary settings window; child dialogs are rendered above it.
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use super::theme;
use crate::app::state::{AppState, SettingsRow, additional_settings_rows};

pub fn draw(frame: &mut Frame, state: &AppState) {
    let screen = frame.area();
    let width = screen.width.saturating_sub(2).min(90);
    let height = screen.height.saturating_sub(2).min(26);
    let area = Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    );
    let block = Block::bordered()
        .title(" Additional settings ")
        .title_style(theme::header_for(state))
        .border_style(theme::strong_border_for(state));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let [list_area, status_area, footer] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(inner);

    let rows = additional_settings_rows(state);
    let mut items = Vec::new();
    let mut selected = 0;
    for (index, row) in rows.iter().enumerate() {
        let section = match row {
            SettingsRow::MusicDirectory => Some("Library"),
            SettingsRow::CheckUpdate => Some("Updates"),
            SettingsRow::VisualizationClock => Some("Visualizations"),
            _ => None,
        };
        if let Some(section) = section {
            items.push(ListItem::new(Line::styled(
                section,
                theme::header_for(state),
            )));
        }
        if index
            == state
                .additional_settings_cursor
                .min(rows.len().saturating_sub(1))
        {
            selected = items.len();
        }
        let (label, value, enabled) = match row {
            SettingsRow::MusicDirectory => (
                "Music save directory".into(),
                if state.music_dir_changing {
                    "checking/changing...".into()
                } else {
                    state.music_dir.to_string_lossy().into_owned()
                },
                !state.music_dir_changing,
            ),
            SettingsRow::CheckUpdate => (
                "Check for updates".into(),
                format!("v{}", env!("CARGO_PKG_VERSION")),
                !state.updater.busy && !state.updater.installed,
            ),
            SettingsRow::InstallUpdate => (
                "Install update".into(),
                state
                    .updater
                    .available
                    .as_ref()
                    .map(|u| format!("v{}", u.version))
                    .unwrap_or_else(|| "check for updates first".into()),
                state.updater.available.is_some()
                    && !state.updater.busy
                    && !state.updater.installed,
            ),
            SettingsRow::VisualizationClock => (
                "Show clock".into(),
                if state.visualizer.config.show_clock {
                    "on"
                } else {
                    "off"
                }
                .into(),
                true,
            ),
            SettingsRow::VisualizationScript(index) => {
                let script = &state.visualizer.scripts[*index];
                let mark = if state.visualizer.selected_script_index() == Some(*index) {
                    "* "
                } else {
                    ""
                };
                (
                    format!("{mark}{}", script.name),
                    script
                        .path
                        .file_name()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    true,
                )
            }
            SettingsRow::VisualizationNew => {
                ("+ New visualization script".into(), "enter".into(), true)
            }
            SettingsRow::VisualizationEdit => {
                ("Edit selected visualization".into(), "enter".into(), true)
            }
            _ => continue,
        };
        let item = ListItem::new(format!("{label}: {value}"));
        items.push(if enabled {
            item
        } else {
            item.style(theme::dim())
        });
    }
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("> ")
            .highlight_style(theme::selection_for(state)),
        list_area,
        &mut ListState::default().with_selected(Some(selected)),
    );
    let message = state
        .status_message
        .as_deref()
        .filter(|message| !message.is_empty())
        .unwrap_or(&state.updater.message);
    frame.render_widget(
        Paragraph::new(message)
            .wrap(Wrap { trim: true })
            .style(theme::dim()),
        status_area,
    );
    frame.render_widget(
        Paragraph::new("Up/Down: navigate | Enter: select | Esc: back").style(theme::dim()),
        footer,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_renders_controls_and_scrolls_to_last_row_in_small_terminal() {
        for (width, height) in [(80, 30), (45, 14)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let mut state = AppState::default();
            state.updater.message = "No newer stable release".into();
            terminal.draw(|frame| draw(frame, &state)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("Additional settings"));
            assert!(text.contains("Music save directory"));
            if height >= 30 {
                assert!(text.contains("Check for updates"));
                assert!(text.contains("Install update"));
                assert!(text.contains("Visualizations"));
            }
            state.additional_settings_cursor = additional_settings_rows(&state).len() - 1;
            terminal.draw(|frame| draw(frame, &state)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("New visualization script"));
            assert!(text.contains("No newer stable release"));
        }
    }
}
