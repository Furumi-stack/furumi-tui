use std::collections::VecDeque;
use std::fs;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context as _, Result};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

/// Ring-buffer capacity for the in-app Logs tab. Bounded so a player left
/// running for days cannot grow memory; ~10k entries ≈ a few MB worst case.
pub const LOG_CAPACITY: usize = 10_000;

#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Monotonic id; the Logs-tab cursor anchors to it, so appends never
    /// move the selection.
    pub seq: u64,
    pub level: tracing::Level,
    /// HH:MM:SS, UTC (same clock as the log file).
    pub time: String,
    pub target: String,
    pub message: String,
}

/// What the Logs tab renders: a window of entries around the cursor.
pub struct LogView {
    /// Oldest-first window of entries.
    pub entries: Vec<LogEntry>,
    /// Row index of the cursor within `entries`.
    pub cursor_row: Option<usize>,
    /// Total entries matching the level filter.
    pub matched: usize,
    /// How far the cursor is from the newest matching entry.
    pub from_end: usize,
}

#[derive(Default)]
pub struct LogBuffer {
    entries: Mutex<VecDeque<LogEntry>>,
    next_seq: std::sync::atomic::AtomicU64,
}

impl LogBuffer {
    fn push(&self, mut entry: LogEntry) {
        entry.seq = self
            .next_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() == LOG_CAPACITY {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// Move the cursor `delta` steps among entries matching the filter
    /// (negative = older). `current = None` starts from the newest. Returns
    /// the new anchor and whether it is the newest matching entry.
    pub fn move_selection(
        &self,
        max_level: tracing::Level,
        current: Option<u64>,
        delta: isize,
    ) -> Option<(u64, bool)> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let seqs: Vec<u64> = entries
            .iter()
            .filter(|e| e.level <= max_level)
            .map(|e| e.seq)
            .collect();
        if seqs.is_empty() {
            return None;
        }
        let index = current
            .and_then(|seq| seqs.iter().position(|s| *s == seq))
            .unwrap_or(seqs.len() - 1);
        let new = (index as i128 + delta as i128).clamp(0, seqs.len() as i128 - 1) as usize;
        Some((seqs[new], new == seqs.len() - 1))
    }

    /// The anchored entry (or the newest matching one when `None`).
    pub fn entry_at(&self, max_level: tracing::Level, selected: Option<u64>) -> Option<LogEntry> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match selected {
            Some(seq) => entries
                .iter()
                .find(|e| e.seq == seq && e.level <= max_level)
                .cloned(),
            None => entries.iter().rev().find(|e| e.level <= max_level).cloned(),
        }
    }

    /// Window of up to `visible` entries with the cursor kept centered.
    /// Only the window is cloned; the scan is cheap level comparisons.
    pub fn view(
        &self,
        max_level: tracing::Level,
        selected: Option<u64>,
        visible: usize,
    ) -> LogView {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let matched: Vec<&LogEntry> = entries.iter().filter(|e| e.level <= max_level).collect();
        let total = matched.len();
        if total == 0 || visible == 0 {
            return LogView {
                entries: Vec::new(),
                cursor_row: None,
                matched: total,
                from_end: 0,
            };
        }
        let cursor_index = selected
            .and_then(|seq| matched.iter().position(|e| e.seq == seq))
            .unwrap_or(total - 1);
        let start = cursor_index
            .saturating_sub(visible / 2)
            .min(total.saturating_sub(visible));
        let end = (start + visible).min(total);
        LogView {
            entries: matched[start..end].iter().map(|e| (*e).clone()).collect(),
            cursor_row: Some(cursor_index - start),
            matched: total,
            from_end: total - 1 - cursor_index,
        }
    }
}

static BUFFER: OnceLock<Arc<LogBuffer>> = OnceLock::new();

pub fn buffer() -> Option<Arc<LogBuffer>> {
    BUFFER.get().cloned()
}

struct MemoryLayer {
    buffer: Arc<LogBuffer>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for MemoryLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut message = String::new();
        event.record(&mut MessageVisitor { out: &mut message });
        let metadata = event.metadata();
        self.buffer.push(LogEntry {
            seq: 0, // assigned in push()
            level: *metadata.level(),
            time: hms_now(),
            target: metadata.target().to_string(),
            message,
        });
    }
}

struct MessageVisitor<'a> {
    out: &'a mut String,
}

impl tracing::field::Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        if field.name() == "message" {
            let _ = write!(self.out, "{value:?}");
        } else {
            let _ = write!(self.out, " {}={:?}", field.name(), value);
        }
    }
}

fn hms_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "{:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    )
}

/// Two sinks: the log file (filtered by RUST_LOG, default info) and the
/// in-app ring buffer for the Logs tab (our crate down to TRACE, noisy
/// dependencies capped at INFO). The buffer works even when the file can't
/// be opened — the error is returned for the status bar, logging still runs.
pub fn init() -> Result<()> {
    let buffer = Arc::new(LogBuffer::default());
    let _ = BUFFER.set(Arc::clone(&buffer));

    fn memory_layer<S>(buffer: Arc<LogBuffer>) -> impl tracing_subscriber::Layer<S>
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        MemoryLayer { buffer }.with_filter(
            tracing_subscriber::filter::Targets::new()
                .with_default(LevelFilter::INFO)
                .with_target(env!("CARGO_CRATE_NAME"), LevelFilter::TRACE),
        )
    }

    match open_log_file() {
        Ok(file) => {
            let file = Arc::new(file);
            let filter =
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
            let file_layer = tracing_subscriber::fmt::layer()
                .with_writer(move || Arc::clone(&file))
                .with_ansi(false)
                .with_filter(filter);
            tracing_subscriber::registry()
                .with(file_layer)
                .with(memory_layer(buffer))
                .init();
            tracing::info!(version = env!("CARGO_PKG_VERSION"), "furumi starting");
            Ok(())
        }
        Err(err) => {
            tracing_subscriber::registry()
                .with(memory_layer(buffer))
                .init();
            tracing::warn!(%err, "log file unavailable, in-app logs only");
            Err(err)
        }
    }
}

fn open_log_file() -> Result<fs::File> {
    let dirs = crate::config::project_dirs().context("cannot determine home directory")?;
    let dir = dirs.cache_dir();
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("furumi-cli.log");
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))
}
