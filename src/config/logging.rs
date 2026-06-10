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
    pub level: tracing::Level,
    /// HH:MM:SS, UTC (same clock as the log file).
    pub time: String,
    pub target: String,
    pub message: String,
}

#[derive(Default)]
pub struct LogBuffer {
    entries: Mutex<VecDeque<LogEntry>>,
}

impl LogBuffer {
    fn push(&self, entry: LogEntry) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() == LOG_CAPACITY {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Window for the UI, newest-last: entries at most `max_level` verbose,
    /// skipping `skip` newest matches and returning up to `take`. Also
    /// returns the total number of matching entries (for scroll clamping).
    /// Only the visible window is cloned, so rendering stays O(buffer scan)
    /// with cheap comparisons even at full capacity.
    pub fn window(
        &self,
        max_level: tracing::Level,
        skip: usize,
        take: usize,
    ) -> (Vec<LogEntry>, usize) {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let mut matched = 0usize;
        let mut out = Vec::with_capacity(take);
        for entry in entries.iter().rev() {
            if entry.level > max_level {
                continue;
            }
            matched += 1;
            if matched > skip && out.len() < take {
                out.push(entry.clone());
            }
        }
        out.reverse();
        (out, matched)
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
    let memory_layer = MemoryLayer { buffer }.with_filter(
        tracing_subscriber::filter::Targets::new()
            .with_default(LevelFilter::INFO)
            .with_target("furumi_cli", LevelFilter::TRACE),
    );

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
                .with(memory_layer)
                .init();
            tracing::info!(version = env!("CARGO_PKG_VERSION"), "furumi-cli starting");
            Ok(())
        }
        Err(err) => {
            tracing_subscriber::registry().with(memory_layer).init();
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
