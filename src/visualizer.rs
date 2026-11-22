//! Script-backed fullscreen and status-bar visualizers.
//!
//! Rust owns the audio tap, script loading, sandboxed execution limits and
//! terminal drawing primitives. Visual math lives in `.rhai` files: every
//! script receives the same input map and returns the same list of draw
//! commands.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use anyhow::{Context as _, Result};
use ratatui::Frame;
use ratatui::buffer::Buffer;

pub mod status;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Clear, Paragraph};
use rhai::{AST, Array, Dynamic, Engine, FLOAT, INT, Map, Scope};
use serde::{Deserialize, Serialize};

use crate::app::state::AppState;

const PRIMARY_SCRIPT_ID: &str = "scope_spectrum";

struct BundledScript {
    id: &'static str,
    file_name: &'static str,
    display_name: &'static str,
    version: u32,
    source: &'static str,
}

const BUNDLED_SCRIPTS: &[BundledScript] = &[
    BundledScript {
        id: PRIMARY_SCRIPT_ID,
        file_name: "scope_spectrum.rhai",
        display_name: "Scope spectrum",
        version: 5,
        source: include_str!("visualizations/scope_spectrum.rhai"),
    },
    BundledScript {
        id: "pulsing_sphere",
        file_name: "pulsing_sphere.rhai",
        display_name: "Pulsing sphere",
        version: 5,
        source: include_str!("visualizations/pulsing_sphere.rhai"),
    },
    BundledScript {
        id: "morphing_cube",
        file_name: "morphing_cube.rhai",
        display_name: "Morphing cube",
        version: 6,
        source: include_str!("visualizations/morphing_cube.rhai"),
    },
    BundledScript {
        id: "vfd_panel",
        file_name: "vfd_panel.rhai",
        display_name: "VFD panel",
        version: 8,
        source: include_str!("visualizations/vfd_panel.rhai"),
    },
];

#[derive(Debug, Clone)]
pub struct VisualizerScript {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualizerConfig {
    #[serde(default = "default_script_id")]
    pub selected: String,
    #[serde(default)]
    pub show_clock: bool,
    /// Display mode the script interprets (`input.mode`), cycled with
    /// `CycleVisualizerMode` while the visualizer is open.
    #[serde(default)]
    pub mode: u32,
}

impl Default for VisualizerConfig {
    fn default() -> Self {
        Self {
            selected: default_script_id(),
            show_clock: false,
            mode: 0,
        }
    }
}

pub struct VisualizerState {
    pub active: bool,
    pub started_at: Option<Instant>,
    pub config: VisualizerConfig,
    pub scripts: Vec<VisualizerScript>,
    pub last_error: Option<String>,
    runtime: Mutex<ScriptRuntime>,
}

impl std::fmt::Debug for VisualizerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VisualizerState")
            .field("active", &self.active)
            .field("started_at", &self.started_at)
            .field("config", &self.config)
            .field("scripts", &self.scripts)
            .field("last_error", &self.last_error)
            .finish_non_exhaustive()
    }
}

impl Default for VisualizerState {
    fn default() -> Self {
        Self {
            active: false,
            started_at: None,
            config: VisualizerConfig::default(),
            scripts: Vec::new(),
            last_error: None,
            runtime: Mutex::new(ScriptRuntime::default()),
        }
    }
}

impl VisualizerState {
    pub fn open(&mut self) {
        self.active = true;
        self.started_at = Some(Instant::now());
        // Every opening is a new session for the script: empty state, frame
        // 0 and dt 0, just as `time` restarts.
        self.runtime
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reset_session();
    }

    pub fn close(&mut self) {
        self.active = false;
        self.started_at = None;
    }

    pub fn load_library(&mut self) -> Result<()> {
        if let Err(err) = status::ensure_script() {
            tracing::warn!(%err, "status visualization initialization failed");
        }
        ensure_bundled_scripts()?;
        self.config = load_config().unwrap_or_else(|err| {
            tracing::warn!(%err, "visualization settings failed to load; using defaults");
            VisualizerConfig::default()
        });
        self.refresh_scripts()?;
        self.ensure_selected_script();
        self.save_config()?;
        Ok(())
    }

    pub fn refresh_scripts(&mut self) -> Result<()> {
        let dir = visualizations_dir()?;
        let mut scripts = Vec::new();
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("rhai") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            scripts.push(VisualizerScript {
                id: id.to_string(),
                name: script_name(&path, id),
                path: path.clone(),
            });
        }
        scripts.sort_by(|left, right| {
            bundled_order(&left.id)
                .unwrap_or(usize::MAX)
                .cmp(&bundled_order(&right.id).unwrap_or(usize::MAX))
                .then_with(|| left.name.cmp(&right.name))
        });
        self.scripts = scripts;
        self.ensure_selected_script();
        Ok(())
    }

    pub fn select_script(&mut self, index: usize) -> Result<()> {
        let Some(script) = self.scripts.get(index) else {
            anyhow::bail!("visualization script not found");
        };
        self.config.selected = script.id.clone();
        self.save_config()
    }

    /// Moves to the next display mode; scripts take it modulo their own
    /// number of modes.
    pub fn cycle_mode(&mut self) -> Result<()> {
        self.config.mode = self.config.mode.wrapping_add(1);
        self.save_config()
    }

    pub fn toggle_clock(&mut self) -> Result<()> {
        self.config.show_clock = !self.config.show_clock;
        self.save_config()
    }

    pub fn selected_script(&self) -> Option<&VisualizerScript> {
        self.scripts
            .iter()
            .find(|script| script.id == self.config.selected)
            .or_else(|| self.scripts.first())
    }

    pub fn selected_script_index(&self) -> Option<usize> {
        self.scripts
            .iter()
            .position(|script| script.id == self.config.selected)
    }

    pub fn selected_script_path(&self) -> Option<PathBuf> {
        self.selected_script().map(|script| script.path.clone())
    }

    pub fn create_script(&mut self) -> Result<PathBuf> {
        ensure_bundled_scripts()?;
        let dir = visualizations_dir()?;
        let mut number = 1;
        let path = loop {
            let candidate = dir.join(format!("custom_{number}.rhai"));
            if !candidate.exists() {
                break candidate;
            }
            number += 1;
        };
        let starter = primary_bundled_script();
        let content = starter.source.replacen(
            "// name: Scope spectrum",
            &format!("// name: Custom {number}"),
            1,
        );
        std::fs::write(&path, content).with_context(|| format!("creating {}", path.display()))?;
        self.refresh_scripts()?;
        let id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or(PRIMARY_SCRIPT_ID)
            .to_string();
        self.config.selected = id;
        self.save_config()?;
        Ok(path)
    }

    pub fn save_config(&self) -> Result<()> {
        let path = visualizer_config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(&self.config)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    fn ensure_selected_script(&mut self) {
        if self
            .scripts
            .iter()
            .any(|script| script.id == self.config.selected)
        {
            return;
        }
        self.config.selected = self
            .scripts
            .first()
            .map(|script| script.id.clone())
            .unwrap_or_else(default_script_id);
    }
}

#[derive(Debug, Clone, Default)]
pub struct AudioFeatures {
    pub elapsed_secs: f64,
    pub position_secs: f64,
    pub progress: f64,
    pub volume: f64,
    pub paused: bool,
    pub energy: f64,
    pub bass: f64,
    pub mid: f64,
    pub treble: f64,
    pub beat: f64,
    pub seed: f64,
    pub scope: Vec<f64>,
}

pub fn draw(frame: &mut Frame, state: &AppState) {
    let area = frame.area();

    let Some(script) = state.visualizer.selected_script() else {
        frame.render_widget(Clear, area);
        draw_message(frame, area, "no visualization scripts found");
        return;
    };
    let input = input_map(area, state);
    let commands = {
        let mut runtime = state
            .visualizer
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime.render(&script.path, input)
    };
    match commands {
        Ok(commands) => render_commands(frame, area, &commands),
        Err(err) => {
            frame.render_widget(Clear, area);
            draw_message(frame, area, &format!("visualizer error: {err}"));
        }
    }
}

fn input_map(area: Rect, state: &AppState) -> Map {
    input_map_with_features(area, state, features_from_state(state, true))
}

fn input_map_with_features(area: Rect, state: &AppState, features: AudioFeatures) -> Map {
    let mut input = Map::new();
    insert_int(&mut input, "width", i64::from(area.width));
    insert_int(&mut input, "height", i64::from(area.height));
    insert_float(&mut input, "time", features.elapsed_secs);
    insert_float(&mut input, "position", features.position_secs);
    insert_float(&mut input, "progress", features.progress);
    insert_float(&mut input, "volume", features.volume);
    input.insert("paused".into(), features.paused.into());
    insert_float(&mut input, "energy", features.energy);
    insert_float(&mut input, "bass", features.bass);
    insert_float(&mut input, "mid", features.mid);
    insert_float(&mut input, "treble", features.treble);
    insert_float(&mut input, "beat", features.beat);
    insert_float(&mut input, "seed", features.seed);
    insert_int(
        &mut input,
        "analysis_sequence",
        state.player.audio_analysis.sequence as i64,
    );
    input.insert(
        "samples".into(),
        features
            .scope
            .into_iter()
            .map(|sample| Dynamic::from_float(sample as FLOAT))
            .collect::<Array>()
            .into(),
    );
    input.insert(
        "show_clock".into(),
        state.visualizer.config.show_clock.into(),
    );
    let (hour, minute, second, millis) = clock_now();
    input.insert(
        "clock".into(),
        format!("{hour:02}:{minute:02}:{second:02}").into(),
    );
    insert_int(&mut input, "clock_hour", i64::from(hour));
    insert_int(&mut input, "clock_minute", i64::from(minute));
    insert_int(&mut input, "clock_second", i64::from(second));
    insert_int(&mut input, "clock_millis", i64::from(millis));
    insert_int(&mut input, "mode", i64::from(state.visualizer.config.mode));
    let (title, artist) = state
        .player
        .current
        .as_ref()
        .map(|track| (track.title.clone(), track.artist_line()))
        .unwrap_or_else(|| ("".to_string(), "".to_string()));
    input.insert("track_title".into(), title.into());
    input.insert("track_artist".into(), artist.into());
    insert_meter(&mut input, &state.player.audio_analysis);
    insert_track_details(&mut input, state);
    input
}

/// Analyzer meter fields (protocol v3). They report the decoded signal as a
/// level meter would, so unlike `energy`/`bass`/... they are not scaled by
/// the output volume; while paused or stopped they fall on their own.
fn insert_meter(input: &mut Map, analysis: &crate::player::AudioAnalysisSnapshot) {
    let floats = |values: &[f64]| -> Dynamic {
        values
            .iter()
            .map(|value| Dynamic::from_float(*value as FLOAT))
            .collect::<Array>()
            .into()
    };
    let bands = crate::player::BAND_CENTERS_HZ.len();
    let spectrum = if analysis.spectrum.len() == bands {
        analysis.spectrum.clone()
    } else {
        vec![0.0; bands]
    };
    let peaks = if analysis.spectrum_peaks.len() == bands {
        analysis.spectrum_peaks.clone()
    } else {
        vec![0.0; bands]
    };
    input.insert("spectrum".into(), floats(&spectrum));
    input.insert("spectrum_peaks".into(), floats(&peaks));
    input.insert(
        "spectrum_hz".into(),
        floats(
            &crate::player::BAND_CENTERS_HZ
                .iter()
                .map(|hz| f64::from(*hz))
                .collect::<Vec<_>>(),
        ),
    );
    insert_float(
        input,
        "spectrum_floor_db",
        f64::from(crate::player::FLOOR_DB),
    );
    insert_float(input, "level_l", analysis.level_l);
    insert_float(input, "level_r", analysis.level_r);
    insert_float(input, "peak_l", analysis.peak_l);
    insert_float(input, "peak_r", analysis.peak_r);
    insert_float(input, "level_side", analysis.level_side);
    let channel_mode = match analysis.stereo {
        Some(true) => "stereo",
        Some(false) => "mono",
        None => "",
    };
    input.insert("channel_mode".into(), channel_mode.into());
    insert_int(input, "sample_rate", i64::from(analysis.sample_rate));
}

/// Track, queue and transport details for front-panel style displays.
/// Unknown numbers are 0 and unknown strings empty.
fn insert_track_details(input: &mut Map, state: &AppState) {
    let player = &state.player;
    let track = player.current.as_ref();
    let number = |value: Option<i32>| i64::from(value.unwrap_or(0).max(0));
    insert_float(
        input,
        "duration",
        track.map_or(0.0, |track| track.duration_seconds.max(0.0)),
    );
    insert_int(
        input,
        "track_number",
        number(track.and_then(|t| t.track_number)),
    );
    insert_int(
        input,
        "disc_number",
        number(track.and_then(|t| t.disc_number)),
    );
    insert_int(input, "year", number(track.and_then(|t| t.release_year)));
    input.insert(
        "release_title".into(),
        track
            .map_or_else(String::new, |t| t.release_title.clone())
            .into(),
    );
    input.insert(
        "audio_format".into(),
        track
            .and_then(|t| t.audio_format.clone())
            .unwrap_or_default()
            .into(),
    );
    insert_int(
        input,
        "audio_bitrate",
        number(track.and_then(|t| t.audio_bitrate)),
    );
    insert_int(
        input,
        "audio_sample_rate",
        number(track.and_then(|t| t.audio_sample_rate)),
    );
    insert_int(
        input,
        "audio_bit_depth",
        number(track.and_then(|t| t.audio_bit_depth)),
    );
    let source = match track {
        None => "",
        Some(track) if track.is_podcast() => "podcast",
        Some(track) if track.fed.is_some() => "federated",
        Some(_) => "local",
    };
    input.insert("source".into(), source.into());
    input.insert(
        "playing".into(),
        (player.playing && track.is_some() && !player.paused).into(),
    );
    let (index, length) = if track.is_some() && !player.queue.is_empty() {
        (
            i64::try_from(player.queue_pos + 1).unwrap_or(i64::MAX),
            i64::try_from(player.queue.len()).unwrap_or(i64::MAX),
        )
    } else {
        (0, i64::try_from(player.queue.len()).unwrap_or(i64::MAX))
    };
    insert_int(input, "queue_index", index);
    insert_int(input, "queue_length", length);
    input.insert("shuffle".into(), player.shuffle.into());
    let repeat = match player.repeat {
        crate::app::state::RepeatMode::Off => "off",
        crate::app::state::RepeatMode::One => "one",
        crate::app::state::RepeatMode::All => "all",
    };
    input.insert("repeat".into(), repeat.into());
}

fn features_from_state(state: &AppState, scale_output: bool) -> AudioFeatures {
    let elapsed_secs = state
        .visualizer
        .started_at
        .map(|started| started.elapsed().as_secs_f64())
        .unwrap_or_default();
    let position_secs = state.player.position_secs;
    let duration_secs = state
        .player
        .current
        .as_ref()
        .map(|track| track.duration_seconds.max(0.0))
        .unwrap_or_default();
    let progress = if duration_secs > 0.0 {
        (position_secs / duration_secs).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let seed = state
        .player
        .current
        .as_ref()
        .map(track_seed)
        .unwrap_or(0.17);
    let analysis = &state.player.audio_analysis;
    let volume = f64::from(state.player.volume.min(100)) / 100.0;
    let output_scale = if !scale_output {
        1.0
    } else if state.player.volume == 0 {
        0.0
    } else {
        0.35 + volume * 0.65
    };
    let active_scale = if scale_output && state.player.paused {
        0.12
    } else {
        1.0
    };
    let scale = output_scale * active_scale;

    AudioFeatures {
        elapsed_secs,
        position_secs,
        progress,
        volume,
        paused: state.player.paused,
        energy: (analysis.energy * scale).clamp(0.0, 1.0),
        bass: (analysis.bass * scale).clamp(0.0, 1.0),
        mid: (analysis.mid * scale).clamp(0.0, 1.0),
        treble: (analysis.treble * scale).clamp(0.0, 1.0),
        beat: if scale_output && state.player.paused {
            0.0
        } else {
            (analysis.beat * output_scale).clamp(0.0, 1.0)
        },
        seed,
        scope: analysis
            .scope
            .iter()
            .map(|sample| (sample * scale).clamp(-1.0, 1.0))
            .collect(),
    }
}

fn track_seed(track: &crate::library::models::TrackItem) -> f64 {
    let mut hash = 0xcbf29ce484222325u64;
    let artist_line = track.artist_line();
    for byte in track
        .title
        .bytes()
        .chain(track.release_title.bytes())
        .chain(artist_line.bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (hash % 10_000) as f64 / 10_000.0
}

#[derive(Debug)]
struct ScriptRuntime {
    engine: Engine,
    cached_path: Option<PathBuf>,
    cached_modified: Option<SystemTime>,
    ast: Option<AST>,
    /// Value the script returned as `state`, handed back as `input.state` on
    /// the next frame. Reset whenever the script is (re)loaded and whenever
    /// the visualizer opens.
    script_state: Dynamic,
    last_render: Option<Instant>,
    frame: u64,
}

/// Longest `input.dt` a script sees, so a long pause does not make one frame
/// jump a whole animation.
const MAX_FRAME_DT_SECS: f64 = 1.0;

impl Default for ScriptRuntime {
    fn default() -> Self {
        let mut engine = Engine::new();
        engine
            .set_max_operations(2_000_000)
            .set_max_call_levels(32)
            .set_max_variables(512)
            .set_max_functions(128)
            .set_max_modules(0)
            .set_max_expr_depths(64, 64)
            .set_max_string_size(1_000_000)
            .set_max_array_size(120_000)
            .set_max_map_size(1_000_000);
        engine.on_print(|message| tracing::debug!(message, "visualizer script"));
        engine.on_debug(|message, source, position| {
            tracing::debug!(message, ?source, ?position, "visualizer script")
        });
        engine.disable_symbol("import");
        engine.disable_symbol("export");
        engine.register_fn("sin", |value: FLOAT| value.sin());
        engine.register_fn("cos", |value: FLOAT| value.cos());
        engine.register_fn("tan", |value: FLOAT| value.tan());
        engine.register_fn("sqrt", |value: FLOAT| value.sqrt());
        engine.register_fn("abs", |value: FLOAT| value.abs());
        engine.register_fn("pow", |value: FLOAT, power: FLOAT| value.powf(power));
        Self {
            engine,
            cached_path: None,
            cached_modified: None,
            ast: None,
            script_state: Dynamic::UNIT,
            last_render: None,
            frame: 0,
        }
    }
}

impl ScriptRuntime {
    /// Runs the script's `render(input)`. A script returns either an array of
    /// draw commands, or `#{ commands: [...], state: ... }` to keep `state`
    /// for the next frame (`input.state`, an empty map at first).
    fn render(
        &mut self,
        path: &Path,
        mut input: Map,
    ) -> std::result::Result<Vec<DrawCommand>, String> {
        self.load(path)?;
        let Some(ast) = &self.ast else {
            return Err("script did not compile".to_string());
        };
        let now = Instant::now();
        let dt = self.last_render.map_or(0.0, |last| {
            now.saturating_duration_since(last)
                .as_secs_f64()
                .min(MAX_FRAME_DT_SECS)
        });
        self.last_render = Some(now);
        insert_float(&mut input, "dt", dt);
        insert_int(
            &mut input,
            "frame",
            i64::try_from(self.frame).unwrap_or(i64::MAX),
        );
        self.frame = self.frame.wrapping_add(1);
        let state = if self.script_state.is_unit() {
            Dynamic::from_map(Map::new())
        } else {
            self.script_state.clone()
        };
        input.insert("state".into(), state);

        let mut scope = Scope::new();
        let returned = self
            .engine
            .call_fn::<Dynamic>(&mut scope, ast, "render", (input,))
            .map_err(|err| err.to_string())?;
        let output = if returned.is_array() {
            returned.cast::<Array>()
        } else if let Some(mut map) = returned.try_cast::<Map>() {
            let commands = map
                .remove("commands")
                .and_then(|value| value.try_cast::<Array>())
                .ok_or_else(|| "render returned a map without a commands array".to_string())?;
            if let Some(state) = map.remove("state") {
                self.script_state = state;
            }
            commands
        } else {
            return Err("render must return an array or #{ commands, state }".to_string());
        };
        let mut commands = Vec::with_capacity(output.len());
        for (index, value) in output.into_iter().enumerate() {
            commands.push(
                DrawCommand::from_dynamic(value)
                    .map_err(|err| format!("invalid draw command #{index}: {err}"))?,
            );
        }
        Ok(commands)
    }

    fn load(&mut self, path: &Path) -> std::result::Result<(), String> {
        let modified = std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok();
        let cache_hit = self.cached_path.as_deref() == Some(path)
            && self.cached_modified == modified
            && self.ast.is_some();
        if cache_hit {
            return Ok(());
        }

        let source = std::fs::read_to_string(path).map_err(|err| format!("{err}"))?;
        let ast = self
            .engine
            .compile(&source)
            .map_err(|err| err.to_string())?;
        self.cached_path = Some(path.to_path_buf());
        self.cached_modified = modified;
        self.ast = Some(ast);
        self.reset_session();
        Ok(())
    }

    /// Forgets the script's retained state and frame clock.
    fn reset_session(&mut self) {
        self.script_state = Dynamic::UNIT;
        self.frame = 0;
        self.last_render = None;
    }
}

#[derive(Debug, Clone)]
enum DrawCommand {
    Clear {
        bg: Color,
    },
    Cell {
        x: u16,
        y: u16,
        ch: String,
        fg: Color,
        bg: Color,
        mods: Modifier,
    },
    HLine {
        x: u16,
        y: u16,
        w: u16,
        ch: String,
        fg: Color,
        bg: Color,
        mods: Modifier,
    },
    VLine {
        x: u16,
        y: u16,
        h: u16,
        ch: String,
        fg: Color,
        bg: Color,
        mods: Modifier,
    },
    Rect {
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        ch: String,
        fg: Color,
        bg: Color,
        mods: Modifier,
    },
    Trace {
        x: u16,
        ys: Vec<u16>,
        ch: String,
        line_ch: String,
        fg: Color,
        line_fg: Color,
        bg: Color,
        mods: Modifier,
    },
    Text {
        x: u16,
        y: u16,
        text: String,
        fg: Color,
        bg: Color,
        mods: Modifier,
    },
    /// Braille dots: each cell holds 2 x 4 dots, so `points` are in dot
    /// coordinates (x 0..2*width, y 0..4*height). Dots merge with braille
    /// already drawn in the same cell.
    Dots {
        points: Vec<(u16, u16)>,
        fg: Color,
        bg: Color,
        mods: Modifier,
    },
    /// A thin straight line of braille dots between two dot coordinates.
    Line {
        x0: u16,
        y0: u16,
        x1: u16,
        y1: u16,
        fg: Color,
        bg: Color,
        mods: Modifier,
    },
}

impl DrawCommand {
    fn from_dynamic(value: Dynamic) -> std::result::Result<Self, String> {
        let Some(map) = value.try_cast::<Map>() else {
            return Err("expected object map".to_string());
        };
        let op = map_string(&map, "op")
            .or_else(|| map_string(&map, "type"))
            .ok_or_else(|| "missing op".to_string())?;
        let fg = map_color(&map, "fg").unwrap_or(Color::White);
        let bg = map_color(&map, "bg").unwrap_or(Color::Black);
        let mods = map_modifiers(&map);
        match op.as_str() {
            "clear" => Ok(DrawCommand::Clear { bg }),
            "cell" => Ok(DrawCommand::Cell {
                x: map_u16(&map, "x").ok_or_else(|| "cell.x must be an integer".to_string())?,
                y: map_u16(&map, "y").ok_or_else(|| "cell.y must be an integer".to_string())?,
                ch: map_string(&map, "ch").unwrap_or_else(|| " ".to_string()),
                fg,
                bg,
                mods,
            }),
            "hline" => Ok(DrawCommand::HLine {
                x: map_u16(&map, "x").ok_or_else(|| "hline.x must be an integer".to_string())?,
                y: map_u16(&map, "y").ok_or_else(|| "hline.y must be an integer".to_string())?,
                w: map_u16(&map, "w").ok_or_else(|| "hline.w must be an integer".to_string())?,
                ch: map_string(&map, "ch").unwrap_or_else(|| " ".to_string()),
                fg,
                bg,
                mods,
            }),
            "vline" => Ok(DrawCommand::VLine {
                x: map_u16(&map, "x").ok_or_else(|| "vline.x must be an integer".to_string())?,
                y: map_u16(&map, "y").ok_or_else(|| "vline.y must be an integer".to_string())?,
                h: map_u16(&map, "h").ok_or_else(|| "vline.h must be an integer".to_string())?,
                ch: map_string(&map, "ch").unwrap_or_else(|| " ".to_string()),
                fg,
                bg,
                mods,
            }),
            "rect" => Ok(DrawCommand::Rect {
                x: map_u16(&map, "x").ok_or_else(|| "rect.x must be an integer".to_string())?,
                y: map_u16(&map, "y").ok_or_else(|| "rect.y must be an integer".to_string())?,
                w: map_u16(&map, "w").ok_or_else(|| "rect.w must be an integer".to_string())?,
                h: map_u16(&map, "h").ok_or_else(|| "rect.h must be an integer".to_string())?,
                ch: map_string(&map, "ch").unwrap_or_else(|| " ".to_string()),
                fg,
                bg,
                mods,
            }),
            "trace" => Ok(DrawCommand::Trace {
                x: map_u16(&map, "x").unwrap_or(0),
                ys: map_u16_array(&map, "ys")
                    .ok_or_else(|| "trace.ys must be an integer array".to_string())?,
                ch: map_string(&map, "ch").unwrap_or_else(|| "*".to_string()),
                line_ch: map_string(&map, "line_ch").unwrap_or_else(|| "|".to_string()),
                fg,
                line_fg: map_color(&map, "line_fg").unwrap_or(fg),
                bg,
                mods,
            }),
            "text" => Ok(DrawCommand::Text {
                x: map_u16(&map, "x").ok_or_else(|| "text.x must be an integer".to_string())?,
                y: map_u16(&map, "y").ok_or_else(|| "text.y must be an integer".to_string())?,
                text: map_string(&map, "text").unwrap_or_default(),
                fg,
                bg,
                mods,
            }),
            "dots" => {
                let flat = map_u16_array(&map, "points")
                    .ok_or_else(|| "dots.points must be an integer array".to_string())?;
                if flat.len() % 2 != 0 {
                    return Err("dots.points must hold x, y pairs".to_string());
                }
                Ok(DrawCommand::Dots {
                    points: flat
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|[x, y]| (*x, *y))
                        .collect(),
                    fg,
                    bg,
                    mods,
                })
            }
            "line" => Ok(DrawCommand::Line {
                x0: map_u16(&map, "x0").ok_or_else(|| "line.x0 must be an integer".to_string())?,
                y0: map_u16(&map, "y0").ok_or_else(|| "line.y0 must be an integer".to_string())?,
                x1: map_u16(&map, "x1").ok_or_else(|| "line.x1 must be an integer".to_string())?,
                y1: map_u16(&map, "y1").ok_or_else(|| "line.y1 must be an integer".to_string())?,
                fg,
                bg,
                mods,
            }),
            _ => Err(format!("unknown op {op:?}")),
        }
    }
}

fn render_commands(frame: &mut Frame, area: Rect, commands: &[DrawCommand]) {
    render_commands_to_buffer(frame.buffer_mut(), area, commands);
}

fn render_commands_to_buffer(buffer: &mut Buffer, area: Rect, commands: &[DrawCommand]) {
    // Each command sets the cell's whole attribute set, so a plain command
    // drawn over a bold or dim one does not inherit its attributes.
    let styled = |fg: Color, bg: Color, mods: Modifier| {
        Style::new()
            .fg(fg)
            .bg(bg)
            .add_modifier(mods)
            .remove_modifier(Modifier::all().difference(mods))
    };
    for command in commands {
        match command {
            DrawCommand::Clear { bg } => fill_rect(
                buffer,
                area,
                " ",
                Style::new().bg(*bg).remove_modifier(Modifier::all()),
            ),
            DrawCommand::Cell {
                x,
                y,
                ch,
                fg,
                bg,
                mods,
            } => {
                write_cell(buffer, area, *x, *y, ch, styled(*fg, *bg, *mods));
            }
            DrawCommand::HLine {
                x,
                y,
                w,
                ch,
                fg,
                bg,
                mods,
            } => {
                let style = styled(*fg, *bg, *mods);
                for offset in 0..(*w).min(area.width.saturating_sub(*x)) {
                    write_cell(buffer, area, x.saturating_add(offset), *y, ch, style);
                }
            }
            DrawCommand::VLine {
                x,
                y,
                h,
                ch,
                fg,
                bg,
                mods,
            } => {
                let style = styled(*fg, *bg, *mods);
                for offset in 0..(*h).min(area.height.saturating_sub(*y)) {
                    write_cell(buffer, area, *x, y.saturating_add(offset), ch, style);
                }
            }
            DrawCommand::Rect {
                x,
                y,
                w,
                h,
                ch,
                fg,
                bg,
                mods,
            } => {
                let style = styled(*fg, *bg, *mods);
                for row in 0..(*h).min(area.height.saturating_sub(*y)) {
                    for col in 0..(*w).min(area.width.saturating_sub(*x)) {
                        write_cell(
                            buffer,
                            area,
                            x.saturating_add(col),
                            y.saturating_add(row),
                            ch,
                            style,
                        );
                    }
                }
            }
            DrawCommand::Trace {
                x,
                ys,
                ch,
                line_ch,
                fg,
                line_fg,
                bg,
                mods,
            } => {
                let line_style = styled(*line_fg, *bg, *mods);
                let point_style = styled(*fg, *bg, *mods);
                let mut previous_y: Option<u16> = None;
                for (index, y) in ys
                    .iter()
                    .copied()
                    .take(usize::from(area.width.saturating_sub(*x)))
                    .enumerate()
                {
                    let Ok(offset) = u16::try_from(index) else {
                        break;
                    };
                    let x = x.saturating_add(offset);
                    if let Some(previous) = previous_y {
                        let from = previous.min(y);
                        let to = previous.max(y);
                        for y in from..=to.min(area.height.saturating_sub(1)) {
                            write_cell(buffer, area, x, y, line_ch, line_style);
                        }
                    }
                    write_cell(buffer, area, x, y, ch, point_style);
                    previous_y = Some(y);
                }
            }
            DrawCommand::Text {
                x,
                y,
                text,
                fg,
                bg,
                mods,
            } => {
                for (offset, ch) in text
                    .chars()
                    .take(usize::from(area.width.saturating_sub(*x)))
                    .enumerate()
                {
                    let Ok(offset) = u16::try_from(offset) else {
                        break;
                    };
                    write_cell(
                        buffer,
                        area,
                        x.saturating_add(offset),
                        *y,
                        &ch.to_string(),
                        styled(*fg, *bg, *mods),
                    );
                }
            }
            DrawCommand::Dots {
                points,
                fg,
                bg,
                mods,
            } => {
                let style = styled(*fg, *bg, *mods);
                for (dot_x, dot_y) in points {
                    plot_dot(buffer, area, *dot_x, *dot_y, style);
                }
            }
            DrawCommand::Line {
                x0,
                y0,
                x1,
                y1,
                fg,
                bg,
                mods,
            } => {
                let style = styled(*fg, *bg, *mods);
                draw_dot_line(buffer, area, (*x0, *y0), (*x1, *y1), style);
            }
        }
    }
}

/// Braille dot bits by (row, column) inside a cell.
const BRAILLE_BITS: [[u32; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
const BRAILLE_BASE: u32 = 0x2800;

fn plot_dot(buffer: &mut Buffer, area: Rect, dot_x: u16, dot_y: u16, style: Style) {
    let (x, y) = (dot_x / 2, dot_y / 4);
    if x >= area.width || y >= area.height {
        return;
    }
    let bit = BRAILLE_BITS[usize::from(dot_y % 4)][usize::from(dot_x % 2)];
    let Some(cell) = buffer.cell_mut((area.x.saturating_add(x), area.y.saturating_add(y))) else {
        return;
    };
    let existing = cell
        .symbol()
        .chars()
        .next()
        .map(u32::from)
        .filter(|code| (BRAILLE_BASE..=BRAILLE_BASE + 0xff).contains(code))
        .map_or(0, |code| code - BRAILLE_BASE);
    let symbol = char::from_u32(BRAILLE_BASE + (existing | bit)).unwrap_or(' ');
    cell.set_symbol(&symbol.to_string()).set_style(style);
}

/// Bresenham between two dot coordinates. The segment is first clipped to the
/// area's dot grid, so the work is bounded by the area, not by how far away a
/// script puts the endpoints.
fn draw_dot_line(buffer: &mut Buffer, area: Rect, from: (u16, u16), to: (u16, u16), style: Style) {
    let max_x = f64::from(area.width) * 2.0 - 1.0;
    let max_y = f64::from(area.height) * 4.0 - 1.0;
    if max_x < 0.0 || max_y < 0.0 {
        return;
    }
    let Some(((x0, y0), (x1, y1))) = clip_segment(
        (f64::from(from.0), f64::from(from.1)),
        (f64::from(to.0), f64::from(to.1)),
        max_x,
        max_y,
    ) else {
        return;
    };
    let (mut x, mut y) = (x0.round() as i32, y0.round() as i32);
    let (x1, y1) = (x1.round() as i32, y1.round() as i32);
    let dx = (x1 - x).abs();
    let dy = -(y1 - y).abs();
    let step_x = if x < x1 { 1 } else { -1 };
    let step_y = if y < y1 { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        if let (Ok(dot_x), Ok(dot_y)) = (u16::try_from(x), u16::try_from(y)) {
            plot_dot(buffer, area, dot_x, dot_y, style);
        }
        if x == x1 && y == y1 {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dy {
            error += dy;
            x += step_x;
        }
        if doubled <= dx {
            error += dx;
            y += step_y;
        }
    }
}

/// Liang-Barsky clip of a segment to `[0, max_x] x [0, max_y]`.
fn clip_segment(
    from: (f64, f64),
    to: (f64, f64),
    max_x: f64,
    max_y: f64,
) -> Option<((f64, f64), (f64, f64))> {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let (mut enter, mut leave) = (0.0_f64, 1.0_f64);
    for (p, q) in [
        (-dx, from.0),
        (dx, max_x - from.0),
        (-dy, from.1),
        (dy, max_y - from.1),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let t = q / p;
            if p < 0.0 {
                enter = enter.max(t);
            } else {
                leave = leave.min(t);
            }
        }
    }
    if enter > leave {
        return None;
    }
    let at = |t: f64| (from.0 + dx * t, from.1 + dy * t);
    Some((at(enter), at(leave)))
}

fn fill_rect(buffer: &mut Buffer, area: Rect, symbol: &'static str, style: Style) {
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_symbol(symbol).set_style(style);
            }
        }
    }
}

fn write_cell(buffer: &mut Buffer, area: Rect, x: u16, y: u16, symbol: &str, style: Style) {
    if x >= area.width || y >= area.height {
        return;
    }
    if let Some(cell) = buffer.cell_mut((area.x.saturating_add(x), area.y.saturating_add(y))) {
        cell.set_symbol(symbol).set_style(style);
    }
}

fn draw_message(frame: &mut Frame, area: Rect, message: &str) {
    frame.render_widget(
        Paragraph::new(message.to_string()).style(Style::new().fg(Color::Red).bg(Color::Black)),
        area,
    );
}

fn map_string(map: &Map, key: &str) -> Option<String> {
    map.get(key)?.clone().try_cast::<String>()
}

fn map_u16(map: &Map, key: &str) -> Option<u16> {
    let value = map.get(key)?.clone().try_cast::<INT>()?;
    u16::try_from(value).ok()
}

fn map_u16_array(map: &Map, key: &str) -> Option<Vec<u16>> {
    map.get(key)?
        .clone()
        .try_cast::<Array>()?
        .into_iter()
        .map(|value| u16::try_from(value.try_cast::<INT>()?).ok())
        .collect()
}

/// Optional text attributes on any command: `bold`, `dim`, `italic`,
/// `underline`, `reversed` (booleans). Terminals render bold as a heavier or
/// brighter glyph and dim as a fainter one.
fn map_modifiers(map: &Map) -> Modifier {
    let flag = |key: &str| {
        map.get(key)
            .and_then(|value| value.clone().try_cast::<bool>())
            .unwrap_or(false)
    };
    let mut mods = Modifier::empty();
    for (key, modifier) in [
        ("bold", Modifier::BOLD),
        ("dim", Modifier::DIM),
        ("italic", Modifier::ITALIC),
        ("underline", Modifier::UNDERLINED),
        ("reversed", Modifier::REVERSED),
    ] {
        if flag(key) {
            mods |= modifier;
        }
    }
    mods
}

fn map_color(map: &Map, key: &str) -> Option<Color> {
    let value = map.get(key)?;
    if let Some(rgb) = value.clone().try_cast::<INT>() {
        return Some(rgb_color(rgb));
    }
    let text = value.clone().try_cast::<String>()?;
    parse_color(&text)
}

fn rgb_color(value: INT) -> Color {
    let value = value.clamp(0, 0x00ff_ffff) as u32;
    Color::Rgb(
        ((value >> 16) & 0xff) as u8,
        ((value >> 8) & 0xff) as u8,
        (value & 0xff) as u8,
    )
}

fn parse_color(text: &str) -> Option<Color> {
    if text == "default" {
        return Some(Color::Reset);
    }
    let hex = text.trim().trim_start_matches('#').trim_start_matches("0x");
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(Color::Rgb(
        ((value >> 16) & 0xff) as u8,
        ((value >> 8) & 0xff) as u8,
        (value & 0xff) as u8,
    ))
}

fn insert_int(map: &mut Map, key: &str, value: i64) {
    map.insert(key.into(), Dynamic::from_int(value as INT));
}

fn insert_float(map: &mut Map, key: &str, value: f64) {
    map.insert(key.into(), Dynamic::from_float(value as FLOAT));
}

/// Local wall-clock time as (hour, minute, second, millisecond), read once
/// so all clock inputs of a frame agree.
fn clock_now() -> (u32, u32, u32, u32) {
    use chrono::Timelike as _;
    let now = chrono::Local::now();
    (
        now.hour(),
        now.minute(),
        now.second(),
        now.timestamp_subsec_millis().min(999),
    )
}

fn default_script_id() -> String {
    PRIMARY_SCRIPT_ID.to_string()
}

fn config_dir() -> Result<PathBuf> {
    Ok(crate::config::project_dirs()
        .map(|dirs| dirs.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".")))
}

pub fn visualizations_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("visualizations"))
}

fn visualizer_config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("visualizations.toml"))
}

fn primary_bundled_script() -> &'static BundledScript {
    &BUNDLED_SCRIPTS[0]
}

fn bundled_order(id: &str) -> Option<usize> {
    BUNDLED_SCRIPTS.iter().position(|script| script.id == id)
}

fn ensure_bundled_scripts() -> Result<()> {
    let dir = visualizations_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for script in BUNDLED_SCRIPTS {
        let path = dir.join(script.file_name);
        let write_default = match std::fs::read_to_string(&path) {
            Ok(existing) => should_replace_bundled_script(&existing, script),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        if write_default {
            std::fs::write(&path, script.source)
                .with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(())
}

fn should_replace_bundled_script(existing: &str, script: &BundledScript) -> bool {
    let name_marker = format!("// name: {}", script.display_name);
    let version_marker = format!("// bundle-version: {}", script.version);
    existing
        .lines()
        .take(3)
        .any(|line| line.trim() == name_marker)
        && !existing
            .lines()
            .take(8)
            .any(|line| line.trim() == version_marker)
}

fn load_config() -> Result<VisualizerConfig> {
    let path = visualizer_config_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(VisualizerConfig::default()),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

fn script_name(path: &Path, fallback_id: &str) -> String {
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines().take(12) {
            let Some(name) = line.trim().strip_prefix("// name:") else {
                continue;
            };
            let name = name.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    fallback_id
        .split(['_', '-'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_input() -> Map {
        let mut input = Map::new();
        insert_int(&mut input, "width", 80);
        insert_int(&mut input, "height", 24);
        insert_float(&mut input, "time", 1.0);
        insert_float(&mut input, "position", 3.0);
        insert_float(&mut input, "progress", 0.25);
        insert_float(&mut input, "volume", 0.8);
        input.insert("paused".into(), false.into());
        insert_float(&mut input, "energy", 0.7);
        insert_float(&mut input, "bass", 0.8);
        insert_float(&mut input, "mid", 0.5);
        insert_float(&mut input, "treble", 0.3);
        insert_float(&mut input, "beat", 0.4);
        insert_float(&mut input, "seed", 0.17);
        insert_int(&mut input, "analysis_sequence", 1);
        input.insert(
            "samples".into(),
            (0..256)
                .map(|index| {
                    let sample = ((index as f64 / 256.0) * std::f64::consts::TAU).sin();
                    Dynamic::from_float(sample as FLOAT)
                })
                .collect::<Array>()
                .into(),
        );
        input.insert("show_clock".into(), true.into());
        input.insert("clock".into(), "12:34:56".into());
        input.insert("track_title".into(), "track".into());
        input.insert("track_artist".into(), "artist".into());
        insert_meter(&mut input, &crate::player::AudioAnalysisSnapshot::default());
        insert_track_details(&mut input, &AppState::default());
        input
    }

    #[test]
    fn bundled_scripts_render_commands() {
        for script in BUNDLED_SCRIPTS {
            let path = std::env::temp_dir().join(format!(
                "furumi-test-{}-{}",
                std::process::id(),
                script.file_name
            ));
            std::fs::write(&path, script.source).unwrap();

            let commands = ScriptRuntime::default()
                .render(&path, test_input())
                .unwrap();
            let _ = std::fs::remove_file(&path);

            assert!(!commands.is_empty(), "{} returned no commands", script.id);
            assert!(
                commands.len() < 220,
                "{} returned {} commands",
                script.id,
                commands.len()
            );
            assert!(
                commands.iter().any(
                    |command| matches!(command, DrawCommand::Text { text, .. } if text.contains("12:34:56"))
                ),
                "{} did not render the configured clock",
                script.id
            );
            if script.id == PRIMARY_SCRIPT_ID {
                assert!(
                    commands.iter().any(
                        |command| matches!(command, DrawCommand::Text { text, .. } if text.contains("artist - track"))
                    ),
                    "{} did not render track metadata",
                    script.id
                );
            }
        }
    }

    #[test]
    fn stale_default_script_is_migrated() {
        let scope = primary_bundled_script();
        assert!(should_replace_bundled_script(
            "// name: Scope spectrum\nfn rgb(r, g, b) { let rr = clamp(r, 0, 255) as int; }",
            scope,
        ));
        assert!(should_replace_bundled_script(
            "// name: Scope spectrum\n// protocol: furumi-visualizer-v2\nfn rgb(r, g, b) { let rr = to_int(clamp(r, 0, 255)); }",
            scope,
        ));
        assert!(!should_replace_bundled_script(scope.source, scope));
        assert!(!should_replace_bundled_script(
            "// name: Custom 1\nfn rgb(r, g, b) { let rr = clamp(r, 0, 255) as int; }",
            scope,
        ));
    }

    fn cube_file() -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            include_str!("visualizations/morphing_cube.rhai"),
        )
        .unwrap();
        file
    }

    #[test]
    fn cube_update_does_not_upgrade_other_presets() {
        let cube = BUNDLED_SCRIPTS
            .iter()
            .find(|script| script.id == "morphing_cube")
            .unwrap();
        let old_cube = cube
            .source
            .replacen("// bundle-version: 6", "// bundle-version: 5", 1);
        assert!(should_replace_bundled_script(&old_cube, cube));
        for script in BUNDLED_SCRIPTS {
            assert!(!should_replace_bundled_script(script.source, script));
        }
        let custom = old_cube.replacen("// name: Morphing cube", "// name: My cube", 1);
        assert!(!should_replace_bundled_script(&custom, cube));
    }

    #[test]
    fn cube_music_adds_particles_echoes_and_a_larger_pulse() {
        let file = cube_file();
        let mut runtime = ScriptRuntime::default();
        let mut input = test_input();
        for band in ["energy", "bass", "mid", "treble", "beat"] {
            insert_float(&mut input, band, 0.0);
        }
        let quiet = runtime.render(file.path(), input.clone()).unwrap();
        for band in ["energy", "bass", "mid", "treble", "beat"] {
            insert_float(&mut input, band, 0.8);
        }
        let loud = runtime.render(file.path(), input).unwrap();
        let effects = |commands: &[DrawCommand]| {
            commands
                .iter()
                .filter(|command| matches!(command, DrawCommand::Cell { ch, .. } if ch != "o"))
                .count()
        };
        assert!(
            effects(&loud) > effects(&quiet) + 30,
            "music should visibly populate the scene"
        );
        let lines = |commands: &[DrawCommand]| {
            commands
                .iter()
                .filter(|command| {
                    matches!(
                        command,
                        DrawCommand::Trace { .. } | DrawCommand::VLine { .. }
                    )
                })
                .count()
        };
        assert_eq!(
            lines(&loud),
            lines(&quiet) + 30,
            "two echoes and three rings"
        );
        let footprint = |commands: &[DrawCommand]| {
            let vertices = cube_vertices(commands);
            let xs: Vec<_> = vertices.iter().map(|p| p.0).collect();
            let ys: Vec<_> = vertices.iter().map(|p| p.1).collect();
            u32::from(xs.iter().max().unwrap() - xs.iter().min().unwrap())
                * u32::from(ys.iter().max().unwrap() - ys.iter().min().unwrap())
        };
        assert!(
            footprint(&loud) > footprint(&quiet) * 2,
            "pulse should be substantially larger"
        );
    }

    fn cube_vertices(commands: &[DrawCommand]) -> Vec<(u16, u16)> {
        commands
            .iter()
            .filter_map(|command| match command {
                DrawCommand::Cell { x, y, ch, .. } if ch == "o" => Some((*x, *y)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn cube_rotates_and_each_audio_control_deforms_geometry() {
        let file = cube_file();
        let mut runtime = ScriptRuntime::default();
        let mut input = test_input();
        insert_int(&mut input, "width", 160);
        insert_int(&mut input, "height", 60);
        for band in ["energy", "bass", "mid", "treble", "beat"] {
            insert_float(&mut input, band, 0.0);
        }
        let quiet = cube_vertices(&runtime.render(file.path(), input.clone()).unwrap());
        assert_eq!(quiet.len(), 8);
        for band in ["energy", "bass", "mid", "treble", "beat"] {
            let mut loud = input.clone();
            insert_float(&mut loud, band, 1.0);
            let deformed = cube_vertices(&runtime.render(file.path(), loud).unwrap());
            assert_ne!(
                quiet, deformed,
                "{band} should move vertices, not just colors"
            );
        }
        insert_float(&mut input, "time", 3.0);
        let rotated = cube_vertices(&runtime.render(file.path(), input).unwrap());
        assert_ne!(quiet, rotated, "rotation should continue in silence");
    }

    #[test]
    fn cube_stays_bounded_across_sizes_times_and_audio_extremes() {
        let file = cube_file();
        let mut runtime = ScriptRuntime::default();
        // Much tighter than the production two-million-operation ceiling.
        runtime.engine.set_max_operations(200_000);
        for (width, height) in [
            (1, 1),
            (11, 7),
            (12, 8),
            (24, 80),
            (80, 24),
            (160, 50),
            (320, 100),
        ] {
            for level in [0.0, 0.5, 1.0] {
                for tick in 0..12 {
                    let mut input = test_input();
                    insert_int(&mut input, "width", width);
                    insert_int(&mut input, "height", height);
                    insert_float(&mut input, "time", f64::from(tick) * 1.37);
                    for band in ["energy", "bass", "mid", "treble", "beat"] {
                        insert_float(&mut input, band, level);
                    }
                    let commands = runtime.render(file.path(), input).unwrap();
                    assert!(commands.len() <= 220);
                    if width < 12 || height < 8 {
                        assert_eq!(commands.len(), 1);
                        continue;
                    }
                    assert_eq!(cube_vertices(&commands).len(), 8);
                    let mut edges = 0;
                    for command in &commands {
                        match command {
                            DrawCommand::Trace { x, ys, .. } => {
                                edges += 1;
                                assert!(usize::from(*x) + ys.len() <= width as usize);
                                assert!(ys.iter().all(|y| i64::from(*y) < height));
                            }
                            DrawCommand::VLine { x, y, h, .. } => {
                                edges += 1;
                                assert!(i64::from(*x) < width);
                                assert!(i64::from(*y) + i64::from(*h) <= height);
                            }
                            DrawCommand::Cell { x, y, .. } => {
                                assert!(i64::from(*x) < width && i64::from(*y) < height);
                            }
                            _ => {}
                        }
                    }
                    assert!((12..=54).contains(&edges));
                }
            }
        }
    }

    #[test]
    fn cube_edges_handle_reversed_vertical_and_collapsed_projections() {
        let runtime = ScriptRuntime::default();
        let ast = runtime
            .engine
            .compile(include_str!("visualizations/morphing_cube.rhai"))
            .unwrap();
        for (ax, ay, bx, by) in [
            (2_i64, 3_i64, 8_i64, 9_i64),
            (8, 9, 2, 3),
            (2, 9, 8, 3),
            (8, 3, 2, 9),
            (4, 9, 4, 3),
            (4, 3, 4, 3),
        ] {
            let output = runtime
                .engine
                .call_fn::<Dynamic>(
                    &mut Scope::new(),
                    &ast,
                    "edge",
                    (ax, ay, bx, by, 0xffffff_i64, 0_i64),
                )
                .unwrap();
            match DrawCommand::from_dynamic(output).unwrap() {
                DrawCommand::Trace { x, ys, .. } => {
                    assert_eq!(i64::from(x), ax.min(bx));
                    assert_eq!(ys.len(), (ax - bx).unsigned_abs() as usize + 1);
                    let (left_y, right_y) = if ax < bx { (ay, by) } else { (by, ay) };
                    assert_eq!(i64::from(ys[0]), left_y);
                    assert_eq!(i64::from(*ys.last().unwrap()), right_y);
                }
                DrawCommand::VLine { x, y, h, .. } => {
                    assert_eq!(i64::from(x), ax);
                    assert_eq!(i64::from(y), ay.min(by));
                    assert_eq!(u64::from(h), (ay - by).unsigned_abs() + 1);
                }
                other => panic!("unexpected edge: {other:?}"),
            }
        }
    }

    /// Explicit, reproducible CPU measurement; no audio hardware or user config.
    /// TestBackend includes buffer drawing/diff, but not terminal I/O/emulation.
    #[test]
    #[ignore = "manual benchmark: cargo test --release benchmark_visualizers -- --ignored --nocapture"]
    fn benchmark_visualizers() {
        use ratatui::{Terminal, backend::TestBackend};

        for script in BUNDLED_SCRIPTS {
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(file.path(), script.source).unwrap();
            let mut runtime = ScriptRuntime::default();
            for (width, height) in [(80_u16, 24_u16), (160, 50), (240, 80)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut script_times = Vec::new();
                let mut frame_times = Vec::new();
                let mut max_commands = 0;
                for tick in 0..230 {
                    let mut input = test_input();
                    insert_int(&mut input, "width", i64::from(width));
                    insert_int(&mut input, "height", i64::from(height));
                    let time = f64::from(tick) * 0.05;
                    insert_float(&mut input, "time", time);
                    for (i, band) in ["energy", "bass", "mid", "treble", "beat"]
                        .iter()
                        .enumerate()
                    {
                        insert_float(&mut input, band, (time * (i + 1) as f64).sin() * 0.5 + 0.5);
                    }
                    let started = Instant::now();
                    let commands = runtime.render(file.path(), input).unwrap();
                    let script_elapsed = started.elapsed();
                    terminal
                        .draw(|frame| render_commands(frame, frame.area(), &commands))
                        .unwrap();
                    if tick >= 30 {
                        script_times.push(script_elapsed.as_secs_f64() * 1000.0);
                        frame_times.push(started.elapsed().as_secs_f64() * 1000.0);
                        max_commands = max_commands.max(commands.len());
                    }
                }
                script_times.sort_by(f64::total_cmp);
                frame_times.sort_by(f64::total_cmp);
                println!(
                    "{} {width}x{height}: script p95={:.3}ms; frame median={:.3}ms p95={:.3}ms max={:.3}ms; commands<={max_commands}",
                    script.id,
                    script_times[189],
                    frame_times[99],
                    frame_times[189],
                    frame_times[199]
                );
                if script.id == "morphing_cube" && width == 80 {
                    for row in terminal
                        .backend()
                        .buffer()
                        .content
                        .chunks(usize::from(width))
                    {
                        println!(
                            "{}",
                            row.iter().map(|cell| cell.symbol()).collect::<String>()
                        );
                    }
                }
            }
        }
    }

    fn script_file(name: &str, source: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("furumi-test-{}-{name}.rhai", std::process::id()));
        std::fs::write(&path, source).unwrap();
        path
    }

    fn rendered_text(commands: &[DrawCommand]) -> String {
        commands
            .iter()
            .filter_map(|command| match command {
                DrawCommand::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn script_state_round_trips_and_resets_on_reload() {
        let source = r#"
            fn render(input) {
                let n = if "n" in input.state { input.state.n } else { 0 };
                #{
                    commands: [#{ op: "text", x: 0, y: 0, text: "" + n + "/" + input.frame }],
                    state: #{ n: n + 1 }
                }
            }
        "#;
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), source).unwrap();
        let base = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        file.as_file()
            .set_times(std::fs::FileTimes::new().set_modified(base))
            .unwrap();
        let mut runtime = ScriptRuntime::default();
        let mut last = Vec::new();
        for _ in 0..3 {
            last = runtime.render(file.path(), test_input()).unwrap();
        }
        assert_eq!(rendered_text(&last), "2/2");

        // An edited script starts from an empty state again.
        std::fs::write(file.path(), format!("{source}\n// edited")).unwrap();
        file.as_file()
            .set_times(
                std::fs::FileTimes::new().set_modified(base + std::time::Duration::from_secs(10)),
            )
            .unwrap();
        let reloaded = runtime.render(file.path(), test_input()).unwrap();
        assert_eq!(rendered_text(&reloaded), "0/0");

        // So does reopening the visualizer.
        runtime.render(file.path(), test_input()).unwrap();
        runtime.reset_session();
        let reopened = runtime.render(file.path(), test_input()).unwrap();
        assert_eq!(rendered_text(&reopened), "0/0");
    }

    #[test]
    fn map_results_need_a_commands_array() {
        let path = script_file("no-commands", "fn render(input) { #{ state: 1 } }");
        let err = ScriptRuntime::default()
            .render(&path, test_input())
            .unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(err.contains("commands"), "{err}");
    }

    #[test]
    fn dots_and_lines_merge_into_braille_cells() {
        let commands = [
            DrawCommand::from_dynamic(Dynamic::from_map(
                [
                    ("op".into(), "line".into()),
                    ("x0".into(), Dynamic::from_int(0)),
                    ("y0".into(), Dynamic::from_int(0)),
                    ("x1".into(), Dynamic::from_int(3)),
                    ("y1".into(), Dynamic::from_int(0)),
                ]
                .into_iter()
                .collect(),
            ))
            .unwrap(),
            DrawCommand::from_dynamic(Dynamic::from_map(
                [
                    ("op".into(), "dots".into()),
                    (
                        "points".into(),
                        vec![Dynamic::from_int(0), Dynamic::from_int(3)].into(),
                    ),
                    ("fg".into(), Dynamic::from_int(0x33ffee)),
                ]
                .into_iter()
                .collect(),
            ))
            .unwrap(),
        ];
        let area = Rect::new(0, 0, 3, 1);
        let mut buffer = Buffer::empty(area);
        render_commands_to_buffer(&mut buffer, area, &commands);
        // Cell 0 holds dots (0,0), (1,0) and (0,3); cell 1 holds (2,0), (3,0).
        assert_eq!(buffer[(0, 0)].symbol(), "\u{2849}");
        assert_eq!(buffer[(1, 0)].symbol(), "\u{2809}");
        assert_eq!(buffer[(0, 0)].fg, Color::Rgb(0x33, 0xff, 0xee));
        assert_eq!(buffer[(2, 0)].symbol(), " ");
    }

    #[test]
    fn odd_dot_lists_and_off_screen_lines_are_handled() {
        let odd = Dynamic::from_map(
            [
                ("op".into(), "dots".into()),
                ("points".into(), vec![Dynamic::from_int(1)].into()),
            ]
            .into_iter()
            .collect(),
        );
        assert!(DrawCommand::from_dynamic(odd).is_err());

        let area = Rect::new(0, 0, 2, 1);
        let mut buffer = Buffer::empty(area);
        let line = DrawCommand::Line {
            x0: 0,
            y0: 0,
            x1: 400,
            y1: 900,
            fg: Color::White,
            bg: Color::Black,
            mods: Modifier::empty(),
        };
        render_commands_to_buffer(&mut buffer, area, &[line]);
        assert_eq!(buffer[(0, 0)].symbol().chars().count(), 1);
    }

    #[test]
    fn far_line_endpoints_are_clipped_to_the_area() {
        let area = Rect::new(0, 0, 4, 2);
        let line = |x0, y0, x1, y1| DrawCommand::Line {
            x0,
            y0,
            x1,
            y1,
            fg: Color::White,
            bg: Color::Black,
            mods: Modifier::empty(),
        };
        let mut far = Buffer::empty(area);
        let started = Instant::now();
        render_commands_to_buffer(
            &mut far,
            area,
            &[
                line(0, 0, u16::MAX, u16::MAX),
                line(u16::MAX, 0, u16::MAX, u16::MAX),
            ],
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
        // Same dots as the line drawn only to the area's last dot.
        let mut near = Buffer::empty(area);
        render_commands_to_buffer(&mut near, area, &[line(0, 0, 7, 7)]);
        assert_eq!(far, near);
    }

    #[test]
    fn attributes_do_not_leak_into_later_commands() {
        let text = |bold: bool| {
            DrawCommand::from_dynamic(Dynamic::from_map(
                [
                    ("op".into(), "text".into()),
                    ("x".into(), Dynamic::from_int(0)),
                    ("y".into(), Dynamic::from_int(0)),
                    ("text".into(), "A".into()),
                    ("bold".into(), bold.into()),
                    ("reversed".into(), bold.into()),
                ]
                .into_iter()
                .collect(),
            ))
            .unwrap()
        };
        let area = Rect::new(0, 0, 1, 1);
        let mut buffer = Buffer::empty(area);
        render_commands_to_buffer(&mut buffer, area, &[text(true), text(false)]);
        assert!(buffer[(0, 0)].modifier.is_empty());
        render_commands_to_buffer(
            &mut buffer,
            area,
            &[text(true), DrawCommand::Clear { bg: Color::Black }],
        );
        assert!(buffer[(0, 0)].modifier.is_empty());
    }

    #[test]
    fn vfd_panel_renders_in_bounds_for_many_situations() {
        let script = BUNDLED_SCRIPTS
            .iter()
            .find(|script| script.id == "vfd_panel")
            .unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), script.source).unwrap();
        let sizes = [
            (20, 6),
            (40, 12),
            (80, 24),
            (119, 33),
            (120, 34),
            (160, 45),
            (300, 90),
        ];
        let variants: [(&str, i64, &str, i64, f64); 4] = [
            ("", 0, "", 0, 0.0),
            (
                "A very long title that keeps going for a while — on and on",
                3,
                "local",
                128,
                1.0,
            ),
            ("Episode", 1, "podcast", 1500, 0.0),
            ("Remote", 2, "federated", 7, 0.55),
        ];
        for (width, height) in sizes {
            for (title, number, source, queue, volume) in variants {
                let mut runtime = ScriptRuntime::default();
                for frame in 0..3 {
                    let mut input = test_input();
                    insert_int(&mut input, "width", width);
                    insert_int(&mut input, "height", height);
                    input.insert("track_title".into(), title.into());
                    input.insert("source".into(), source.into());
                    insert_int(&mut input, "track_number", number * 41);
                    insert_int(&mut input, "queue_index", number);
                    insert_int(&mut input, "queue_length", queue);
                    insert_float(&mut input, "volume", volume);
                    insert_float(&mut input, "position", 600.0 + f64::from(frame) * 7.0);
                    input.insert("paused".into(), (frame == 1).into());
                    let commands = runtime
                        .render(file.path(), input)
                        .unwrap_or_else(|err| panic!("{width}x{height} {title:?}: {err}"));
                    let area = Rect::new(0, 0, width as u16, height as u16);
                    let mut buffer = Buffer::empty(area);
                    render_commands_to_buffer(&mut buffer, area, &commands);
                    // nothing is laid out past the bottom of the frame
                    for command in &commands {
                        let bottom = match command {
                            DrawCommand::Text { y, .. }
                            | DrawCommand::Cell { y, .. }
                            | DrawCommand::HLine { y, .. } => Some(*y),
                            DrawCommand::VLine { y, h, .. } | DrawCommand::Rect { y, h, .. } => {
                                Some(y + h.saturating_sub(1))
                            }
                            _ => None,
                        };
                        if let Some(bottom) = bottom {
                            assert!(
                                i64::from(bottom) < height,
                                "{width}x{height} {title:?}: drawn at row {bottom}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn vfd_panel_options_standby_and_effects_render() {
        let base = BUNDLED_SCRIPTS
            .iter()
            .find(|script| script.id == "vfd_panel")
            .unwrap()
            .source;
        let variants = [
            base.to_string(),
            base.replacen(
                r#"fn METER_STYLE() { "bars" }"#,
                r#"fn METER_STYLE() { "needles" }"#,
                1,
            ),
            base.replacen(
                r#"fn PALETTE() { "classic" }"#,
                r#"fn PALETTE() { "green" }"#,
                1,
            ),
            base.replacen(
                r#"fn PALETTE() { "classic" }"#,
                r#"fn PALETTE() { "ice" }"#,
                1,
            ),
            base.replacen(
                r#"fn DISPLAY_MODE() { "auto" }"#,
                r#"fn DISPLAY_MODE() { "deck" }"#,
                1,
            ),
            base.replacen(
                r#"fn DISPLAY_MODE() { "auto" }"#,
                r#"fn DISPLAY_MODE() { "minimal" }"#,
                1,
            ),
        ];
        assert_ne!(variants[0], variants[1]);
        assert_ne!(variants[0], variants[2]);
        for source in variants {
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(file.path(), &source).unwrap();
            for (width, height) in [
                (30, 8),
                (80, 24),
                (100, 30),
                (110, 40),
                (160, 45),
                (240, 70),
            ] {
                for title in ["", "Даже не думай — Ёлка", "Track"] {
                    let mut runtime = ScriptRuntime::default();
                    // Past the power-on test, with static and a seek pending.
                    let state: Map = [
                        ("boot".into(), Dynamic::from_float(-1.0)),
                        ("spin".into(), Dynamic::from_float(0.3)),
                        ("over".into(), Dynamic::from_float(0.5)),
                        ("noise".into(), Dynamic::from_float(0.2)),
                        ("search".into(), Dynamic::from_float(0.4)),
                        ("key".into(), "another track".into()),
                        ("source".into(), "local".into()),
                        ("last_pos".into(), Dynamic::from_float(5.0)),
                    ]
                    .into_iter()
                    .collect();
                    runtime.load(file.path()).unwrap();
                    runtime.script_state = Dynamic::from_map(state);
                    let mut input = test_input();
                    insert_int(&mut input, "width", width);
                    insert_int(&mut input, "height", height);
                    input.insert("track_title".into(), title.into());
                    insert_float(&mut input, "volume", 0.2);
                    insert_int(&mut input, "clock_hour", 23);
                    insert_int(&mut input, "clock_second", 1);
                    let commands = runtime
                        .render(file.path(), input)
                        .unwrap_or_else(|err| panic!("{width}x{height} {title:?}: {err}"));
                    let area = Rect::new(0, 0, width as u16, height as u16);
                    let mut buffer = Buffer::empty(area);
                    render_commands_to_buffer(&mut buffer, area, &commands);
                    if title.is_empty() && width >= 80 {
                        let text = rendered_text(&commands);
                        assert!(
                            text.contains("STANDBY"),
                            "{width}x{height}: no standby clock"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn vfd_panel_clock_keeps_clear_of_the_lamps() {
        let script = BUNDLED_SCRIPTS
            .iter()
            .find(|script| script.id == "vfd_panel")
            .unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), script.source).unwrap();
        for (width, height) in [(120, 34), (160, 34), (180, 40)] {
            let mut runtime = ScriptRuntime::default();
            runtime.load(file.path()).unwrap();
            runtime.script_state = Dynamic::from_map(
                [("boot".into(), Dynamic::from_float(-1.0))]
                    .into_iter()
                    .collect(),
            );
            let mut input = test_input();
            insert_int(&mut input, "width", width);
            insert_int(&mut input, "height", height);
            let commands = runtime.render(file.path(), input).unwrap();
            let area = Rect::new(0, 0, width as u16, height as u16);
            let mut buffer = Buffer::empty(area);
            render_commands_to_buffer(&mut buffer, area, &commands);
            let rows: Vec<String> = (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| buffer[(x, y)].symbol().to_string())
                        .collect()
                })
                .collect();
            assert!(rows[0].contains("12:34:56"), "{width}x{height}: clock");
            assert!(
                rows.iter().any(|row| row.contains("HIGH")),
                "{width}x{height}: lamps overdrawn"
            );
        }
    }

    #[test]
    fn text_modifiers_reach_the_buffer() {
        let command = DrawCommand::from_dynamic(Dynamic::from_map(
            [
                ("op".into(), "text".into()),
                ("x".into(), Dynamic::from_int(0)),
                ("y".into(), Dynamic::from_int(0)),
                ("text".into(), "A".into()),
                ("bold".into(), true.into()),
                ("dim".into(), false.into()),
            ]
            .into_iter()
            .collect(),
        ))
        .unwrap();
        let area = Rect::new(0, 0, 1, 1);
        let mut buffer = Buffer::empty(area);
        render_commands_to_buffer(&mut buffer, area, &[command]);
        assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        assert!(!buffer[(0, 0)].modifier.contains(Modifier::DIM));
    }

    #[test]
    fn meter_and_track_details_reach_scripts() {
        let mut state = AppState::default();
        let track = crate::library::models::TrackItem {
            id: 7,
            title: "Even Flow".into(),
            track_number: Some(3),
            disc_number: Some(1),
            duration_seconds: 293.0,
            artists: vec![],
            featured_artists: vec![],
            release_id: 1,
            release_title: "Ten".into(),
            release_year: Some(1991),
            cover_path: None,
            file_path: "/music/even-flow.flac".into(),
            content_id: None,
            audio_format: Some("flac".into()),
            audio_bitrate: Some(900),
            audio_sample_rate: Some(44_100),
            audio_bit_depth: Some(16),
            file_size_bytes: None,
            play_count: 0,
            fed: None,
            podcast: None,
        };
        state.player.queue = vec![track.clone(), track.clone()];
        state.player.queue_pos = 1;
        state.player.current = Some(track);
        state.player.playing = true;
        state.player.audio_analysis.spectrum = vec![0.5; crate::player::BAND_CENTERS_HZ.len()];
        state.player.audio_analysis.level_l = 0.25;
        state.player.audio_analysis.level_side = 0.125;
        state.player.audio_analysis.stereo = Some(true);
        state.player.audio_analysis.sample_rate = 44_100;

        let input = input_map(Rect::new(0, 0, 80, 24), &state);
        let int = |key: &str| input[key].as_int().unwrap();
        let float = |key: &str| input[key].as_float().unwrap();
        let text = |key: &str| input[key].clone().into_string().unwrap();
        assert_eq!(int("queue_index"), 2);
        assert_eq!(int("queue_length"), 2);
        assert_eq!(int("track_number"), 3);
        assert_eq!(int("year"), 1991);
        assert_eq!(int("audio_bitrate"), 900);
        assert_eq!(int("sample_rate"), 44_100);
        assert_eq!(text("source"), "local");
        assert_eq!(text("repeat"), "off");
        assert_eq!(text("audio_format"), "flac");
        assert!(input["playing"].as_bool().unwrap());
        assert_eq!(float("level_l"), 0.25);
        assert_eq!(float("level_side"), 0.125);
        assert_eq!(text("channel_mode"), "stereo");
        assert_eq!(float("spectrum_floor_db"), -60.0);
        let spectrum = input["spectrum"].clone().into_array().unwrap();
        let hz = input["spectrum_hz"].clone().into_array().unwrap();
        assert_eq!(spectrum.len(), hz.len());
        assert_eq!(spectrum[0].as_float().unwrap(), 0.5);

        // Without analysis the arrays keep their length and read silence.
        let empty = input_map(Rect::new(0, 0, 80, 24), &AppState::default());
        let peaks = empty["spectrum_peaks"].clone().into_array().unwrap();
        assert_eq!(peaks.len(), crate::player::BAND_CENTERS_HZ.len());
        assert_eq!(empty["source"].clone().into_string().unwrap(), "");
        assert_eq!(empty["queue_index"].as_int().unwrap(), 0);
    }
}
