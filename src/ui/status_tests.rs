use super::*;
use ratatui::{Terminal, backend::TestBackend};

fn playing_state() -> AppState {
    let mut state = AppState::default();
    state.player.current = Some(crate::library::models::TrackItem {
        id: 1,
        title: "Песня 界".into(),
        track_number: None,
        disc_number: None,
        duration_seconds: 240.0,
        artists: vec![],
        featured_artists: vec![],
        release_id: 1,
        release_title: "Release".into(),
        release_year: None,
        cover_path: None,
        file_path: "/unused/test.flac".into(),
        content_id: None,
        audio_format: None,
        audio_bitrate: None,
        audio_sample_rate: None,
        audio_bit_depth: None,
        file_size_bytes: None,
        play_count: 0,
        fed: None,
        podcast: None,
    });
    state.player.playing = true;
    state.player.position_secs = 12.0;
    state
}

#[test]
fn footer_centers_version_and_aligns_visualization_under_volume() {
    let mut state = playing_state();
    for width in [160, 200] {
        for shuffle in [false, true] {
            state.player.shuffle = shuffle;
            let mut terminal = Terminal::new(TestBackend::new(width, 2)).unwrap();
            terminal
                .draw(|frame| draw_status(frame, frame.area(), &state))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let version = format!("v{}", env!("CARGO_PKG_VERSION"));
            // Count display cells, not UTF-8 bytes: the title is non-ASCII.
            let volume_x = (0..width)
                .find(|x| {
                    buffer[(*x, 0)].symbol() == "v"
                        && *x + 2 < width
                        && buffer[(*x + 1, 0)].symbol() == "o"
                        && buffer[(*x + 2, 0)].symbol() == "l"
                })
                .unwrap();
            let areas = footer_areas(Rect::new(0, 0, width, 2), &state);
            assert_eq!(areas[2].x, volume_x);
            assert_eq!(areas[2].y, 1);
            assert_eq!(areas[2].right(), width);
            let version_x = (width - version.len() as u16) / 2;
            for (offset, ch) in version.chars().enumerate() {
                assert_eq!(
                    buffer[(version_x + offset as u16, 1)].symbol(),
                    ch.to_string()
                );
            }
            assert_eq!(
                status_visualizer_area(Rect::new(0, 0, width, 24), &state).x,
                volume_x
            );
        }
    }
}

#[test]
fn footer_regions_do_not_overlap_on_narrow_terminals_or_command_input() {
    let mut state = playing_state();
    state.status_message = Some("long status message ".repeat(20));
    for width in [1, 5, 8, 20, 40, 80, 120, 200] {
        let areas = footer_areas(Rect::new(0, 0, width, 2), &state);
        assert!(areas[0].right() <= areas[1].x);
        assert!(areas[1].right() <= areas[2].x);
        assert!(areas[2].right() <= width);
        for command in [false, true] {
            state.cmdline.active = command;
            let mut terminal = Terminal::new(TestBackend::new(width, 2)).unwrap();
            terminal
                .draw(|frame| draw_status(frame, frame.area(), &state))
                .unwrap();
            // Neither messages nor the command prompt may paint the graph.
            for x in areas[2].x..areas[2].right() {
                assert_eq!(terminal.backend().buffer()[(x, 1)].symbol(), " ");
            }
        }
    }
}

#[test]
fn status_animation_requires_local_unpaused_playback() {
    let mut state = playing_state();
    assert!(crate::visualizer::status::playing(&state));
    state.player.paused = true;
    assert!(!crate::visualizer::status::playing(&state));
    assert!(crate::visualizer::status::visible(&state));
    state.player.paused = false;
    state.player.playing = false;
    assert!(!crate::visualizer::status::playing(&state));
    assert!(!crate::visualizer::status::visible(&state));
    state.player.playing = true;
    state.device_playback.role = crate::app::state::DevicePlaybackRole::Control;
    assert!(!crate::visualizer::status::playing(&state));
}

#[test]
fn federation_mode_badge_stays_visible_at_the_right_edge() {
    use crate::config::settings::LibrarySourceMode;
    use ratatui::style::Color;
    let mut state = playing_state();
    for (mode, background) in [
        (LibrarySourceMode::Local, Color::Yellow),
        (LibrarySourceMode::My, Color::Cyan),
        (LibrarySourceMode::Global, Color::Green),
    ] {
        state.global.filters.source_mode = mode;
        for width in [20, 40, 80, 160, 200] {
            for playing in [false, true] {
                state.player.playing = playing;
                state.cmdline.active = !playing;
                let mut terminal = Terminal::new(TestBackend::new(width, 2)).unwrap();
                terminal
                    .draw(|frame| draw_status(frame, frame.area(), &state))
                    .unwrap();
                let label = format!(" federation: {} ", mode.label());
                let start = width - label.len() as u16;
                for (offset, ch) in label.chars().enumerate() {
                    let cell = &terminal.backend().buffer()[(start + offset as u16, 0)];
                    assert_eq!(cell.symbol(), ch.to_string());
                    assert_eq!(cell.bg, background);
                    assert_eq!(cell.fg, Color::Black);
                }
            }
        }
    }
}
