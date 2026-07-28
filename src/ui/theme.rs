use ratatui::style::{Color, Modifier, Style};

use crate::app::state::{AppState, DevicePlaybackRole};

pub const ACCENT: Color = Color::Cyan;
pub const CONTROL_ACCENT: Color = Color::Yellow;
pub const JAM_ACCENT: Color = Color::Magenta;
pub const DIM: Color = Color::DarkGray;

pub fn accent() -> Style {
    Style::new().fg(ACCENT)
}

pub fn accent_for(state: &AppState) -> Style {
    Style::new().fg(accent_color_for(state))
}

pub fn dim() -> Style {
    Style::new().fg(DIM)
}

pub fn tab_active_for(state: &AppState) -> Style {
    Style::new()
        .fg(Color::Black)
        .bg(accent_color_for(state))
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

pub fn selection_for(state: &AppState) -> Style {
    match state.device_playback.role {
        DevicePlaybackRole::Jam => Style::new().fg(Color::White).bg(Color::Rgb(80, 24, 96)),
        DevicePlaybackRole::Control => Style::new().fg(Color::White).bg(Color::Rgb(92, 72, 0)),
        DevicePlaybackRole::Active => selection(),
    }
}

pub fn header_for(state: &AppState) -> Style {
    accent_for(state).add_modifier(Modifier::BOLD)
}

pub fn border_for(state: &AppState) -> Style {
    match state.device_playback.role {
        DevicePlaybackRole::Jam => Style::new().fg(JAM_ACCENT),
        DevicePlaybackRole::Control => Style::new().fg(CONTROL_ACCENT),
        DevicePlaybackRole::Active => dim(),
    }
}

pub fn strong_border_for(state: &AppState) -> Style {
    accent_for(state)
}

pub fn role_pill(role: DevicePlaybackRole) -> Style {
    let bg = match role {
        DevicePlaybackRole::Active => Color::Green,
        DevicePlaybackRole::Control => CONTROL_ACCENT,
        DevicePlaybackRole::Jam => JAM_ACCENT,
    };
    Style::new()
        .fg(Color::Black)
        .bg(bg)
        .add_modifier(Modifier::BOLD)
}

fn accent_color_for(state: &AppState) -> Color {
    match state.device_playback.role {
        DevicePlaybackRole::Jam => JAM_ACCENT,
        DevicePlaybackRole::Control => CONTROL_ACCENT,
        DevicePlaybackRole::Active => ACCENT,
    }
}
