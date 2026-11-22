//! One-row surface: blocking preparation runs off the UI thread; views copy
//! only the prepared buffer. Uses the fullscreen engine and drawing commands.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::Paragraph,
};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

use super::{
    ScriptRuntime, features_from_state, input_map_with_features, render_commands_to_buffer,
};
use crate::app::{event::AppEvent, state::AppState};

const SOURCE: &str = include_str!("../visualizations/status/braille.rhai");
const LEGACY_SOURCE: &str = include_str!("fixtures/status_braille_v1.rhai");

pub fn script_path() -> anyhow::Result<PathBuf> {
    Ok(super::visualizations_dir()?
        .join("status")
        .join("braille.rhai"))
}

pub(super) fn ensure_script() -> anyhow::Result<()> {
    install_script_at(&script_path()?)
}

fn install_script_at(path: &Path) -> anyhow::Result<()> {
    ensure_script_at(path)?;
    // Upgrade only a byte-for-byte stock script, never a user's edited one.
    if std::fs::metadata(path)?.len() == LEGACY_SOURCE.len() as u64
        && std::fs::read(path)? == LEGACY_SOURCE.as_bytes()
    {
        let mut replacement =
            tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
        replacement.write_all(SOURCE.as_bytes())?;
        replacement.as_file().sync_all()?;
        if std::fs::read(path)? == LEGACY_SOURCE.as_bytes() {
            replacement.persist(path)?;
        }
    }
    Ok(())
}

fn ensure_script_at(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => file.write_all(SOURCE.as_bytes())?,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

pub fn visible(state: &AppState) -> bool {
    state.player.playing && state.player.current.is_some() && state.device_playback.is_audio_owner()
}

pub fn playing(state: &AppState) -> bool {
    visible(state) && !state.player.paused
}

fn source_input(width: u16, state: &AppState, elapsed: f64) -> rhai::Map {
    // AnalyzedSource taps decoded audio before rodio's output volume. Keep
    // those source levels intact; input.volume remains available as metadata.
    let mut features = features_from_state(state, false);
    features.elapsed_secs = elapsed;
    input_map_with_features(Rect::new(0, 0, width, 1), state, features)
}

#[derive(Debug, Default)]
pub struct State {
    target: Option<(u16, String)>,
    generation: u64,
    frame: Option<Buffer>,
    error: Option<String>,
    paused: bool,
}

impl State {
    /// Invalidates in-flight results. A resize keeps the last frame until a
    /// replacement arrives, including when playback is paused.
    pub fn set_target(&mut self, target: Option<(u16, String)>) {
        if self.target != target {
            let same_track = self
                .target
                .as_ref()
                .zip(target.as_ref())
                .is_some_and(|(old, new)| old.1 == new.1);
            self.target = target;
            self.generation = self.generation.wrapping_add(1);
            if !same_track {
                self.frame = None;
            }
            self.error = None;
        }
    }

    pub fn set_paused(&mut self, paused: bool) {
        if self.paused != paused {
            self.paused = paused;
            self.generation = self.generation.wrapping_add(1);
        }
    }

    pub fn apply(&mut self, ready: Ready) {
        if ready.generation != self.generation || self.target.is_none() || self.paused {
            return;
        }
        match ready.result {
            Ok(frame) => {
                self.frame = Some(frame);
                self.error = None;
            }
            Err(error) => {
                if self.error.as_ref() != Some(&error) {
                    tracing::warn!(%error, "status visualization failed");
                }
                self.frame = None;
                self.error = Some(error);
            }
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        if let Some(buffer) = &self.frame {
            if buffer.area.width == 0 {
                return;
            }
            for x in 0..area.width {
                let source_x =
                    usize::from(x) * usize::from(buffer.area.width) / usize::from(area.width);
                if let Some(target) = frame.buffer_mut().cell_mut((area.x + x, area.y)) {
                    *target = buffer.content[source_x].clone();
                }
            }
        } else if let Some(error) = &self.error {
            frame.render_widget(
                Paragraph::new(format!("viz: {error}")).style(Style::new().fg(Color::Red)),
                area,
            );
        }
    }
}

#[derive(Debug)]
pub struct Ready {
    generation: u64,
    result: Result<Buffer, String>,
}

/// One in-flight job at most: slow custom scripts drop ticks, never queue
/// frames or block input. AST caches are independent of fullscreen selection.
pub struct Renderer {
    runtime: Arc<Mutex<ScriptRuntime>>,
    started: Instant,
    pub busy: bool,
}

impl Default for Renderer {
    fn default() -> Self {
        let mut runtime = ScriptRuntime::default();
        runtime
            .engine
            .set_max_operations(200_000)
            .set_max_array_size(8192)
            .set_max_map_size(4096)
            .set_max_string_size(32_768);
        Self {
            runtime: Arc::new(Mutex::new(runtime)),
            started: Instant::now(),
            busy: false,
        }
    }
}

impl Renderer {
    pub fn request(&mut self, state: &AppState, tx: mpsc::UnboundedSender<AppEvent>) {
        let Some((width, _)) = &state.status_visualizer.target else {
            return;
        };
        if self.busy || !playing(state) || state.visualizer.active {
            return;
        }
        self.busy = true;
        let width = *width;
        let generation = state.status_visualizer.generation;
        let input = source_input(width, state, self.started.elapsed().as_secs_f64());
        let runtime = Arc::clone(&self.runtime);
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                let path = script_path().map_err(|e| e.to_string())?;
                let mut runtime = runtime
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if runtime.cached_path.is_none() {
                    // Retry initialization after a transient startup error.
                    install_script_at(&path).map_err(|e| e.to_string())?;
                }
                prepare(&mut runtime, &path, input, width)
            })
            .await
            .unwrap_or_else(|err| Err(err.to_string()));
            let _ = tx.send(AppEvent::StatusVisualizationReady(Ready {
                generation,
                result,
            }));
        });
    }
}

fn prepare(
    runtime: &mut ScriptRuntime,
    path: &Path,
    input: rhai::Map,
    width: u16,
) -> Result<Buffer, String> {
    if std::fs::metadata(path).map_err(|e| e.to_string())?.len() > 65_536 {
        return Err("status script exceeds 64 KiB".into());
    }
    let commands = runtime.render(path, input)?;
    if commands.len() > 4096 {
        return Err("too many status drawing commands".into());
    }
    let area = Rect::new(0, 0, width, 1);
    let mut buffer = Buffer::empty(area);
    render_commands_to_buffer(&mut buffer, area, &commands);
    // A user-supplied cell string must never spill into neighboring UI areas.
    for cell in &mut buffer.content {
        if cell.symbol().width() != 1 || cell.symbol().chars().any(char::is_control) {
            cell.set_symbol(" ");
        }
    }
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::super::insert_float;
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use rhai::{Array, Dynamic};

    fn input(width: u16, level: f64) -> rhai::Map {
        let mut input = source_input(width, &AppState::default(), 0.0);
        for band in ["energy", "bass", "mid", "treble", "beat"] {
            insert_float(&mut input, band, level);
        }
        input.insert(
            "samples".into(),
            (0..256)
                .map(|index| {
                    Dynamic::from_float(level * (index as f64 * std::f64::consts::TAU / 64.0).sin())
                })
                .collect::<Array>()
                .into(),
        );
        input
    }

    #[test]
    fn histogram_uses_braille_and_real_levels_in_one_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status/braille.rhai");
        ensure_script_at(&path).unwrap();
        let renderer = Renderer::default();
        let mut runtime = renderer.runtime.lock().unwrap();
        for width in [0, 1, 8, 60, 200] {
            let quiet = prepare(&mut runtime, &path, input(width, 0.0), width).unwrap();
            let loud = prepare(&mut runtime, &path, input(width, 1.0), width).unwrap();
            assert_eq!(loud.area, Rect::new(0, 0, width, 1));
            assert!(quiet.content.iter().all(|cell| cell.symbol() == "⠀"));
            assert!(loud.content.iter().all(|cell| {
                cell.symbol()
                    .chars()
                    .all(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch))
                    && cell.bg == Color::Reset
            }));
            if width > 0 {
                assert_ne!(loud, quiet);
            }
            let mut paused = input(width, 1.0);
            paused.insert("paused".into(), true.into());
            assert_eq!(prepare(&mut runtime, &path, paused, width).unwrap(), loud);
        }
    }

    #[test]
    fn existing_user_script_survives_and_hot_reload_recovers_from_errors() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path();
        let mut runtime = ScriptRuntime::default();
        for (revision, symbol) in [(1, "A"), (2, "B"), (3, "bad"), (4, "C")] {
            let source = if symbol == "bad" {
                "fn render(".into()
            } else {
                format!(
                    "fn render(input) {{ [#{{ op: \"text\", x: 0, y: 0, text: \"{symbol}\" }}] }}"
                )
            };
            std::fs::write(path, &source).unwrap();
            file.as_file()
                .set_times(std::fs::FileTimes::new().set_modified(
                    std::time::UNIX_EPOCH
                        + std::time::Duration::from_secs(1_700_000_000 + revision),
                ))
                .unwrap();
            install_script_at(path).unwrap();
            assert_eq!(std::fs::read_to_string(path).unwrap(), source);
            let frame = prepare(&mut runtime, path, input(8, 0.0), 8);
            if symbol == "bad" {
                assert!(frame.is_err());
            } else {
                assert_eq!(frame.unwrap().content[0].symbol(), symbol);
            }
        }
    }

    #[test]
    fn status_commands_are_clipped_and_cannot_spill_into_the_ui() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r##"fn render(input) { [
            #{ op: "rect", x: 0, y: 0, w: 65535, h: 65535, ch: "#" },
            #{ op: "text", x: 0, y: 1, text: "outside" },
            #{ op: "trace", x: 6, ys: [65535, 0, 65535], ch: "*" },
            #{ op: "cell", x: 0, y: 0, ch: "界" },
            #{ op: "cell", x: 1, y: 0, ch: "\n" },
            #{ op: "cell", x: 2, y: 0, ch: "too long" }
        ] }"##,
        )
        .unwrap();
        let buffer = prepare(&mut ScriptRuntime::default(), file.path(), input(8, 0.0), 8).unwrap();
        assert!(buffer.content[..3].iter().all(|cell| cell.symbol() == " "));
        let state = State {
            frame: Some(buffer),
            ..State::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();
        terminal
            .draw(|frame| {
                for cell in &mut frame.buffer_mut().content {
                    cell.set_symbol("x");
                }
                state.draw(frame, Rect::new(11, 3, 8, 1));
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..4 {
            for x in 0..20 {
                if y != 3 || !(11..19).contains(&x) {
                    assert_eq!(buffer[(x, y)].symbol(), "x");
                }
            }
        }
    }

    #[test]
    fn stale_frames_are_discarded_after_resize_track_change_or_stop() {
        let mut state = State::default();
        state.set_target(Some((8, "track-a".into())));
        let generation = state.generation;
        state.apply(Ready {
            generation,
            result: Ok(Buffer::empty(Rect::new(0, 0, 8, 1))),
        });
        assert!(state.frame.is_some());
        for target in [
            Some((12, "track-a".into())),
            Some((12, "track-b".into())),
            None,
        ] {
            state.set_target(target);
            let previous_frame = state.frame.clone();
            state.apply(Ready {
                generation,
                result: Ok(Buffer::empty(Rect::new(0, 0, 8, 1))),
            });
            assert_eq!(state.frame, previous_frame);
        }
        assert!(state.frame.is_none());
    }

    #[test]
    fn pause_freezes_the_last_frame_and_rejects_in_flight_work() {
        let mut state = State::default();
        state.set_target(Some((8, "track".into())));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        for cell in &mut buffer.content {
            cell.set_symbol("⣤");
        }
        let generation = state.generation;
        state.apply(Ready {
            generation,
            result: Ok(buffer.clone()),
        });
        state.set_paused(true);
        state.apply(Ready {
            generation,
            result: Err("late error".into()),
        });
        state.apply(Ready {
            generation: state.generation,
            result: Ok(Buffer::empty(buffer.area)),
        });
        assert_eq!(state.frame.as_ref(), Some(&buffer));
        // A paused resize adapts the frozen row instead of erasing it.
        state.set_target(Some((12, "track".into())));
        let mut terminal = Terminal::new(TestBackend::new(12, 1)).unwrap();
        terminal
            .draw(|frame| state.draw(frame, frame.area()))
            .unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| cell.symbol() == "⣤")
        );
        let paused_generation = state.generation;
        state.set_paused(false);
        state.apply(Ready {
            generation: paused_generation,
            result: Err("stale pause".into()),
        });
        assert_eq!(state.frame.as_ref(), Some(&buffer));
        state.apply(Ready {
            generation: state.generation,
            result: Ok(Buffer::empty(Rect::new(0, 0, 12, 1))),
        });
        assert_ne!(state.frame.as_ref(), Some(&buffer));
        state.set_target(None);
        assert!(state.frame.is_none());
    }

    #[test]
    fn source_audio_input_is_independent_of_output_volume_and_pause() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), SOURCE).unwrap();
        let mut state = AppState::default();
        state.player.audio_analysis = crate::player::AudioAnalysisSnapshot {
            energy: 0.8,
            bass: 0.9,
            mid: 0.6,
            treble: 0.3,
            beat: 0.5,
            scope: (0..256).map(|i| (i as f64 * 0.13).sin() * 0.7).collect(),
            ..Default::default()
        };
        let mut runtime = ScriptRuntime::default();
        let expected =
            prepare(&mut runtime, file.path(), source_input(64, &state, 1.0), 64).unwrap();
        for volume in [0, 1, 25, 80, 100] {
            for paused in [false, true] {
                state.player.volume = volume;
                state.player.paused = paused;
                let map = source_input(64, &state, 1.0);
                for (key, level) in [
                    ("energy", 0.8),
                    ("bass", 0.9),
                    ("mid", 0.6),
                    ("treble", 0.3),
                    ("beat", 0.5),
                ] {
                    assert_eq!(map[key].as_float().unwrap(), level);
                }
                assert_eq!(map["volume"].as_float().unwrap(), f64::from(volume) / 100.0);
                assert_eq!(
                    prepare(&mut runtime, file.path(), map, 64).unwrap(),
                    expected
                );
            }
        }
    }

    #[test]
    fn loud_recordings_use_multiple_heights_instead_of_a_solid_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("braille.rhai");
        let mut runtime = ScriptRuntime::default();
        std::fs::write(&path, LEGACY_SOURCE).unwrap();
        let old = prepare(&mut runtime, &path, input(64, 1.0), 64).unwrap();
        assert!(old.content.iter().all(|cell| cell.symbol() == "⣿"));
        install_script_at(&path).unwrap();
        // A fresh runtime also models a restart installing the new default.
        let mut runtime = ScriptRuntime::default();
        let new = prepare(&mut runtime, &path, input(64, 1.0), 64).unwrap();
        let symbols: std::collections::HashSet<_> =
            new.content.iter().map(|cell| cell.symbol()).collect();
        assert!(symbols.len() >= 4, "{symbols:?}");
        assert!(
            new.content
                .iter()
                .filter(|cell| cell.symbol() == "⣿")
                .count()
                < 32
        );
        assert!(new.content.iter().any(|cell| cell.symbol() == "⠀"));
        // Compressed band/beat values alone must not create a filled baseline.
        let mut silence = input(64, 1.0);
        silence.insert(
            "samples".into(),
            (0..256)
                .map(|_| Dynamic::from_float(0.001))
                .collect::<Array>()
                .into(),
        );
        let silent = prepare(&mut runtime, &path, silence, 64).unwrap();
        assert!(silent.content.iter().all(|cell| cell.symbol() == "⠀"));
    }

    #[test]
    fn only_the_untouched_previous_default_is_upgraded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("braille.rhai");
        std::fs::write(&path, LEGACY_SOURCE).unwrap();
        install_script_at(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SOURCE);
        install_script_at(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SOURCE);
        for stock in [LEGACY_SOURCE, SOURCE] {
            let custom = format!("{stock}\n// user customization\n");
            std::fs::write(&path, &custom).unwrap();
            install_script_at(&path).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), custom);
        }
    }

    #[test]
    fn status_script_infinite_loop_hits_the_operation_limit() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "fn render(input) { loop {} [] }").unwrap();
        let renderer = Renderer::default();
        let mut runtime = renderer.runtime.lock().unwrap();
        let err = prepare(&mut runtime, file.path(), input(8, 0.0), 8).unwrap_err();
        assert!(err.contains("operations"), "{err}");
    }

    #[test]
    #[ignore = "manual benchmark: cargo test --release benchmark_status_visualizer -- --ignored --nocapture"]
    fn benchmark_status_visualizer() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), SOURCE).unwrap();
        let renderer = Renderer::default();
        let mut runtime = renderer.runtime.lock().unwrap();
        for width in [32, 64, 128] {
            let mut times = Vec::new();
            for tick in 0..230 {
                let level = (f64::from(tick) * 0.13).sin() * 0.5 + 0.5;
                let input = input(width, level);
                let start = Instant::now();
                let frame = prepare(&mut runtime, file.path(), input, width).unwrap();
                std::hint::black_box(frame);
                if tick >= 30 {
                    times.push(start.elapsed().as_secs_f64() * 1000.0);
                }
            }
            times.sort_by(f64::total_cmp);
            println!(
                "status {width}x1: median={:.3}ms p95={:.3}ms max={:.3}ms",
                times[99], times[189], times[199]
            );
        }
    }
}
