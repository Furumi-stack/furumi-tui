use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Wrap};

use super::theme;
use crate::app::state::AppState;

/// Placeholder notice shown until podcast support lands: the tab already
/// holds its final position so the numeric shortcuts stay stable.
pub(crate) const UNAVAILABLE: &str = "Podcasts are not available in this build yet.";

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::bordered()
        .title(" Podcasts ")
        .title_style(theme::header_for(state))
        .border_style(theme::border_for(state));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let lines = vec![
        Line::styled(UNAVAILABLE, theme::header_for(state)),
        Line::default(),
        Line::styled(
            "Subscriptions, episodes and listening progress will appear here.",
            theme::dim(),
        ),
    ];
    let height = (lines.len() as u16).min(inner.height);
    let body = Rect {
        y: inner.y + inner.height.saturating_sub(height) / 2,
        height,
        ..inner
    };
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        body,
    );
}
