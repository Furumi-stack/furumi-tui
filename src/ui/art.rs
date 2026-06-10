use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};

use crate::art::ArtImage;

/// Render half-block art as ratatui text: one `▀` per cell, foreground =
/// top pixel, background = bottom pixel.
pub fn to_text(art: &ArtImage) -> Text<'static> {
    let mut lines = Vec::with_capacity(usize::from(art.height_cells));
    for y in 0..art.height_cells {
        let mut spans = Vec::with_capacity(usize::from(art.width_cells));
        for x in 0..art.width_cells {
            let (top, bottom) = art.cell(x, y);
            spans.push(Span::styled(
                "▀",
                Style::new()
                    .fg(Color::Rgb(top[0], top[1], top[2]))
                    .bg(Color::Rgb(bottom[0], bottom[1], bottom[2])),
            ));
        }
        lines.push(Line::from(spans));
    }
    Text::from(lines)
}
