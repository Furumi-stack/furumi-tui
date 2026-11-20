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
    // Center on the estimated wrapped height, but let the paragraph use every
    // row down to the bottom so narrow terminals never cut off the text.
    let wrapped = estimated_rows(&lines, inner.width).min(inner.height);
    let top = inner.height.saturating_sub(wrapped) / 2;
    let body = Rect {
        y: inner.y + top,
        height: inner.height - top,
        ..inner
    };
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        body,
    );
}

/// Rows the lines need when wrapped to `width` (a lower bound: word wrapping
/// may need one more row per line, which the paragraph area leaves room for).
fn estimated_rows(lines: &[Line], width: u16) -> u16 {
    let width = usize::from(width.max(1));
    let rows: usize = lines
        .iter()
        .map(|line| line.width().div_ceil(width).max(1))
        .sum();
    u16::try_from(rows).unwrap_or(u16::MAX)
}
