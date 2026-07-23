//! Script-backed fullscreen visualizers.
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
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Clear, Paragraph};
use rhai::{AST, Array, Dynamic, Engine, FLOAT, INT, Map, Scope};
use serde::{Deserialize, Serialize};

use crate::app::state::AppState;

const PRIMARY_SCRIPT_ID: &str = "scope_spectrum";
const BUNDLE_VERSION: u32 = 5;

struct BundledScript {
    id: &'static str,
    file_name: &'static str,
    display_name: &'static str,
    source: &'static str,
}

const BUNDLED_SCRIPTS: &[BundledScript] = &[
    BundledScript {
        id: PRIMARY_SCRIPT_ID,
        file_name: "scope_spectrum.rhai",
        display_name: "Scope spectrum",
        source: include_str!("visualizations/scope_spectrum.rhai"),
    },
    BundledScript {
        id: "pulsing_sphere",
        file_name: "pulsing_sphere.rhai",
        display_name: "Pulsing sphere",
        source: include_str!("visualizations/pulsing_sphere.rhai"),
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
}

impl Default for VisualizerConfig {
    fn default() -> Self {
        Self {
            selected: default_script_id(),
            show_clock: false,
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
    }

    pub fn close(&mut self) {
        self.active = false;
        self.started_at = None;
    }

    pub fn load_library(&mut self) -> Result<()> {
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
    let features = features_from_state(state);
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
    input.insert("clock".into(), clock_label().into());
    let (title, artist) = state
        .player
        .current
        .as_ref()
        .map(|track| (track.title.clone(), track.artist_line()))
        .unwrap_or_else(|| ("".to_string(), "".to_string()));
    input.insert("track_title".into(), title.into());
    input.insert("track_artist".into(), artist.into());
    input
}

fn features_from_state(state: &AppState) -> AudioFeatures {
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
    let output_scale = if state.player.volume == 0 {
        0.0
    } else {
        0.35 + volume * 0.65
    };
    let active_scale = if state.player.paused { 0.12 } else { 1.0 };
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
        beat: if state.player.paused {
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
}

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
        }
    }
}

impl ScriptRuntime {
    fn render(&mut self, path: &Path, input: Map) -> std::result::Result<Vec<DrawCommand>, String> {
        self.load(path)?;
        let Some(ast) = &self.ast else {
            return Err("script did not compile".to_string());
        };
        let mut scope = Scope::new();
        let output = self
            .engine
            .call_fn::<Array>(&mut scope, ast, "render", (input,))
            .map_err(|err| err.to_string())?;
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
        Ok(())
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
    },
    HLine {
        x: u16,
        y: u16,
        w: u16,
        ch: String,
        fg: Color,
        bg: Color,
    },
    VLine {
        x: u16,
        y: u16,
        h: u16,
        ch: String,
        fg: Color,
        bg: Color,
    },
    Rect {
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        ch: String,
        fg: Color,
        bg: Color,
    },
    Trace {
        x: u16,
        ys: Vec<u16>,
        ch: String,
        line_ch: String,
        fg: Color,
        line_fg: Color,
        bg: Color,
    },
    Text {
        x: u16,
        y: u16,
        text: String,
        fg: Color,
        bg: Color,
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
        match op.as_str() {
            "clear" => Ok(DrawCommand::Clear { bg }),
            "cell" => Ok(DrawCommand::Cell {
                x: map_u16(&map, "x").ok_or_else(|| "cell.x must be an integer".to_string())?,
                y: map_u16(&map, "y").ok_or_else(|| "cell.y must be an integer".to_string())?,
                ch: map_string(&map, "ch").unwrap_or_else(|| " ".to_string()),
                fg,
                bg,
            }),
            "hline" => Ok(DrawCommand::HLine {
                x: map_u16(&map, "x").ok_or_else(|| "hline.x must be an integer".to_string())?,
                y: map_u16(&map, "y").ok_or_else(|| "hline.y must be an integer".to_string())?,
                w: map_u16(&map, "w").ok_or_else(|| "hline.w must be an integer".to_string())?,
                ch: map_string(&map, "ch").unwrap_or_else(|| " ".to_string()),
                fg,
                bg,
            }),
            "vline" => Ok(DrawCommand::VLine {
                x: map_u16(&map, "x").ok_or_else(|| "vline.x must be an integer".to_string())?,
                y: map_u16(&map, "y").ok_or_else(|| "vline.y must be an integer".to_string())?,
                h: map_u16(&map, "h").ok_or_else(|| "vline.h must be an integer".to_string())?,
                ch: map_string(&map, "ch").unwrap_or_else(|| " ".to_string()),
                fg,
                bg,
            }),
            "rect" => Ok(DrawCommand::Rect {
                x: map_u16(&map, "x").ok_or_else(|| "rect.x must be an integer".to_string())?,
                y: map_u16(&map, "y").ok_or_else(|| "rect.y must be an integer".to_string())?,
                w: map_u16(&map, "w").ok_or_else(|| "rect.w must be an integer".to_string())?,
                h: map_u16(&map, "h").ok_or_else(|| "rect.h must be an integer".to_string())?,
                ch: map_string(&map, "ch").unwrap_or_else(|| " ".to_string()),
                fg,
                bg,
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
            }),
            "text" => Ok(DrawCommand::Text {
                x: map_u16(&map, "x").ok_or_else(|| "text.x must be an integer".to_string())?,
                y: map_u16(&map, "y").ok_or_else(|| "text.y must be an integer".to_string())?,
                text: map_string(&map, "text").unwrap_or_default(),
                fg,
                bg,
            }),
            _ => Err(format!("unknown op {op:?}")),
        }
    }
}

fn render_commands(frame: &mut Frame, area: Rect, commands: &[DrawCommand]) {
    for command in commands {
        match command {
            DrawCommand::Clear { bg } => fill_rect(frame, area, " ", Style::new().bg(*bg)),
            DrawCommand::Cell { x, y, ch, fg, bg } => {
                write_cell(frame, area, *x, *y, ch, Style::new().fg(*fg).bg(*bg));
            }
            DrawCommand::HLine {
                x,
                y,
                w,
                ch,
                fg,
                bg,
            } => {
                let style = Style::new().fg(*fg).bg(*bg);
                for offset in 0..*w {
                    write_cell(frame, area, x.saturating_add(offset), *y, ch, style);
                }
            }
            DrawCommand::VLine {
                x,
                y,
                h,
                ch,
                fg,
                bg,
            } => {
                let style = Style::new().fg(*fg).bg(*bg);
                for offset in 0..*h {
                    write_cell(frame, area, *x, y.saturating_add(offset), ch, style);
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
            } => {
                let style = Style::new().fg(*fg).bg(*bg);
                for row in 0..*h {
                    for col in 0..*w {
                        write_cell(
                            frame,
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
            } => {
                let line_style = Style::new().fg(*line_fg).bg(*bg);
                let point_style = Style::new().fg(*fg).bg(*bg);
                let mut previous_y: Option<u16> = None;
                for (index, y) in ys.iter().copied().enumerate() {
                    let Ok(offset) = u16::try_from(index) else {
                        break;
                    };
                    let x = x.saturating_add(offset);
                    if let Some(previous) = previous_y {
                        let from = previous.min(y);
                        let to = previous.max(y);
                        for y in from..=to {
                            write_cell(frame, area, x, y, line_ch, line_style);
                        }
                    }
                    write_cell(frame, area, x, y, ch, point_style);
                    previous_y = Some(y);
                }
            }
            DrawCommand::Text { x, y, text, fg, bg } => {
                for (offset, ch) in text.chars().enumerate() {
                    let Ok(offset) = u16::try_from(offset) else {
                        break;
                    };
                    write_cell(
                        frame,
                        area,
                        x.saturating_add(offset),
                        *y,
                        &ch.to_string(),
                        Style::new().fg(*fg).bg(*bg),
                    );
                }
            }
        }
    }
}

fn fill_rect(frame: &mut Frame, area: Rect, symbol: &'static str, style: Style) {
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                cell.set_symbol(symbol).set_style(style);
            }
        }
    }
}

fn write_cell(frame: &mut Frame, area: Rect, x: u16, y: u16, symbol: &str, style: Style) {
    if x >= area.width || y >= area.height {
        return;
    }
    if let Some(cell) = frame
        .buffer_mut()
        .cell_mut((area.x.saturating_add(x), area.y.saturating_add(y)))
    {
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

fn clock_label() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600 % 24,
        secs / 60 % 60,
        secs % 60
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
    let version_marker = format!("// bundle-version: {BUNDLE_VERSION}");
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
            (0..128)
                .map(|index| {
                    let sample = ((index as f64 / 128.0) * std::f64::consts::TAU).sin();
                    Dynamic::from_float(sample as FLOAT)
                })
                .collect::<Array>()
                .into(),
        );
        input.insert("show_clock".into(), true.into());
        input.insert("clock".into(), "12:34:56".into());
        input.insert("track_title".into(), "track".into());
        input.insert("track_artist".into(), "artist".into());
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
}
