use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Cyan;
pub const DIM: Color = Color::DarkGray;

pub fn accent() -> Style {
    Style::new().fg(ACCENT)
}

pub fn dim() -> Style {
    Style::new().fg(DIM)
}

pub fn tab_active() -> Style {
    Style::new()
        .fg(Color::Black)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD)
}

pub fn danger_button() -> Style {
    Style::new()
        .fg(Color::White)
        .bg(Color::Rgb(96, 0, 24))
        .add_modifier(Modifier::BOLD)
}

pub fn selection() -> Style {
    Style::new().fg(Color::White).bg(Color::Rgb(24, 68, 72))
}

pub fn header() -> Style {
    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
}
