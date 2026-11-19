//! Playback engine: a dedicated audio thread owning the rodio output device
//! and player. The app talks to it through `Controller` (commands over a
//! channel, position/pause state over atomics) and receives `PlayerEvent`s
//! through the callback given to `spawn` — this module knows nothing about
//! the UI or app state.

mod analyzer;
mod opus;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use rodio::{Decoder, DeviceSinkBuilder, Player, Source, stream::MixerDeviceSink};

pub use analyzer::AudioAnalysisSnapshot;

pub trait TrackReadSeek: std::io::Read + std::io::Seek + Send + Sync {}

impl<T> TrackReadSeek for T where T: std::io::Read + std::io::Seek + Send + Sync {}

/// Playback sources are either ordinary files or growing streaming cache files.
pub type TrackReader = Box<dyn TrackReadSeek>;

/// Perceptual volume: cubic mapping from percent to linear amplitude, so
/// equal percent steps sound like equal loudness steps and low percentages
/// give fine-grained control.
pub fn amplitude(percent: u8) -> f32 {
    let v = f32::from(percent.min(100)) / 100.0;
    v * v * v
}

#[derive(Debug)]
pub enum PlayerEvent {
    /// A newly requested source was decoded and handed to rodio.
    Started,
    /// A track played to its end. `has_next` is true when a prefetched
    /// source was already queued and is now playing gaplessly.
    TrackFinished {
        has_next: bool,
    },
    Failed(String),
}

enum Command {
    Play {
        reader: TrackReader,
        byte_len: Option<u64>,
        mime_type: Option<String>,
        seekable: bool,
        volume: f32,
    },
    /// Play a source that was already opened, probed and positioned off
    /// this thread (remote podcast streams, whose probing and seeking wait
    /// on the network). Dropped unless `generation` is still current.
    PlaySource {
        source: DecodedSource,
        volume: f32,
        /// Where `source` was positioned before it was handed over.
        start: Duration,
        paused: bool,
        generation: u64,
    },
    /// Append the next track behind the current one without interrupting
    /// playback — rodio switches sources back to back (gapless-ish).
    Enqueue {
        reader: TrackReader,
        byte_len: Option<u64>,
    },
    Pause,
    Resume,
    Stop,
    Seek(Duration),
    SetVolume(f32),
}

/// Lock-free playback state for the UI tick to read.
#[derive(Debug, Default)]
pub struct Shared {
    position_ms: AtomicU64,
    paused: AtomicBool,
    analysis: Arc<analyzer::AnalyzerShared>,
}

impl Shared {
    pub fn position(&self) -> Duration {
        Duration::from_millis(self.position_ms.load(Ordering::Relaxed))
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn audio_analysis(&self) -> AudioAnalysisSnapshot {
        self.analysis.snapshot()
    }
}

#[derive(Clone)]
pub struct Controller {
    tx: Sender<Command>,
    pub shared: Arc<Shared>,
    playback_allowed: Arc<AtomicBool>,
    /// Bumped by every start and stop; a source prepared in the background
    /// plays only if no start or stop happened since it was requested.
    generation: Arc<AtomicU64>,
}

impl Controller {
    /// Revoke the output at the audio thread boundary, including work queued
    /// by background decoders before an ownership transfer.
    pub fn set_playback_allowed(&self, allowed: bool) {
        if self.playback_allowed.swap(allowed, Ordering::AcqRel) && !allowed {
            self.stop();
        }
    }

    /// The current source generation: capture it when starting to prepare a
    /// source in the background and pass it to [`Controller::play_source`].
    pub fn source_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn supersede(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Play a source decoded (and possibly positioned at `start`) outside
    /// the audio thread, built with [`decode_source_with`]. The audio thread
    /// drops it when another start or stop happened after `generation` was
    /// read, so a slow stream never interrupts what the user chose since.
    pub fn play_source(
        &self,
        source: DecodedSource,
        volume: f32,
        start: Duration,
        paused: bool,
        generation: u64,
    ) {
        let _ = self.tx.send(Command::PlaySource {
            source,
            volume,
            start,
            paused,
            generation,
        });
    }

    pub fn play(&self, reader: TrackReader, byte_len: Option<u64>, volume: f32) {
        self.supersede();
        let _ = self.tx.send(Command::Play {
            reader,
            byte_len,
            mime_type: None,
            seekable: true,
            volume,
        });
    }

    pub fn play_stream(&self, reader: TrackReader, mime_type: Option<String>, volume: f32) {
        self.supersede();
        let _ = self.tx.send(Command::Play {
            reader,
            byte_len: None,
            mime_type,
            seekable: false,
            volume,
        });
    }

    pub fn enqueue(&self, reader: TrackReader, byte_len: Option<u64>) {
        let _ = self.tx.send(Command::Enqueue { reader, byte_len });
    }

    pub fn pause(&self) {
        let _ = self.tx.send(Command::Pause);
    }

    pub fn resume(&self) {
        let _ = self.tx.send(Command::Resume);
    }

    pub fn stop(&self) {
        self.supersede();
        let _ = self.tx.send(Command::Stop);
    }

    pub fn seek(&self, position: Duration) {
        let _ = self.tx.send(Command::Seek(position));
    }

    pub fn set_volume(&self, volume: f32) {
        let _ = self.tx.send(Command::SetVolume(volume));
    }
}

pub fn spawn(on_event: impl Fn(PlayerEvent) + Send + 'static) -> Controller {
    let (tx, rx) = std::sync::mpsc::channel();
    let shared = Arc::new(Shared::default());
    let thread_shared = Arc::clone(&shared);
    let playback_allowed = Arc::new(AtomicBool::new(true));
    let thread_allowed = Arc::clone(&playback_allowed);
    let generation = Arc::new(AtomicU64::new(0));
    let thread_generation = Arc::clone(&generation);
    std::thread::Builder::new()
        .name("audio".to_string())
        .spawn(move || {
            run(
                rx,
                thread_shared,
                thread_allowed,
                thread_generation,
                on_event,
            )
        })
        .expect("spawning the audio thread cannot fail");
    Controller {
        tx,
        shared,
        playback_allowed,
        generation,
    }
}

struct Output {
    /// Keeps the audio device open; dropping it stops the mixer.
    _device: MixerDeviceSink,
    player: Player,
}

/// Audio-thread state besides the output device.
struct Playing {
    track_loaded: bool,
    /// Position of the current source when it was appended: sources
    /// positioned before playback (resumed podcast episodes) start there,
    /// while rodio counts from zero until the next seek.
    base: Duration,
}

fn run(
    rx: Receiver<Command>,
    shared: Arc<Shared>,
    playback_allowed: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    on_event: impl Fn(PlayerEvent),
) {
    let mut output: Option<Output> = None;
    let mut playing = Playing {
        track_loaded: false,
        base: Duration::ZERO,
    };
    let mut last_len = 0usize;

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(command) => {
                if !playback_allowed.load(Ordering::Acquire)
                    && matches!(
                        command,
                        Command::Play { .. }
                            | Command::PlaySource { .. }
                            | Command::Enqueue { .. }
                            | Command::Resume
                    )
                {
                    continue;
                }
                if let Command::PlaySource {
                    generation: requested,
                    ..
                } = &command
                    && *requested != generation.load(Ordering::SeqCst)
                {
                    // Superseded while it was being prepared.
                    continue;
                }
                // Commands change the source queue legitimately; resync the
                // length so the next tick doesn't read it as a track ending.
                handle(command, &shared, &mut output, &mut playing, &on_event);
                last_len = output.as_ref().map_or(0, |out| out.player.len());
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(out) = &output {
                    let position = out.player.get_pos() + playing.base;
                    shared
                        .position_ms
                        .store(position.as_millis() as u64, Ordering::Relaxed);
                    shared
                        .paused
                        .store(out.player.is_paused(), Ordering::Relaxed);
                    let len = out.player.len();
                    if playing.track_loaded && len < last_len {
                        // The next (gapless) source starts at zero.
                        playing.base = Duration::ZERO;
                        if len == 0 {
                            playing.track_loaded = false;
                        }
                        for _ in len..last_len {
                            on_event(PlayerEvent::TrackFinished { has_next: len > 0 });
                        }
                    }
                    last_len = len;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn handle(
    command: Command,
    shared: &Arc<Shared>,
    output: &mut Option<Output>,
    playing: &mut Playing,
    on_event: &impl Fn(PlayerEvent),
) {
    match command {
        Command::Play {
            reader,
            byte_len,
            mime_type,
            seekable,
            volume,
        } => {
            let Some(out) = ensure_output(output, on_event) else {
                return;
            };
            match decode_source(reader, byte_len, mime_type.as_deref(), seekable) {
                Ok(source) => start_source(out, source, volume, false, shared, on_event),
                Err(err) => {
                    on_event(PlayerEvent::Failed(format!("cannot decode track: {err}")));
                    return;
                }
            }
            playing.track_loaded = true;
            playing.base = Duration::ZERO;
        }
        Command::PlaySource {
            source,
            volume,
            start,
            paused,
            generation: _,
        } => {
            let Some(out) = ensure_output(output, on_event) else {
                return;
            };
            start_source(out, source, volume, paused, shared, on_event);
            playing.track_loaded = true;
            playing.base = start;
            shared
                .position_ms
                .store(start.as_millis() as u64, Ordering::Relaxed);
        }
        Command::Enqueue { reader, byte_len } => {
            let Some(out) = output.as_ref() else {
                return;
            };
            match decode_source(reader, byte_len, None, true) {
                Ok(source) => out.player.append(analyzer::AnalyzedSource::new(
                    source,
                    Arc::clone(&shared.analysis),
                )),
                Err(err) => {
                    on_event(PlayerEvent::Failed(format!(
                        "cannot decode next track: {err}"
                    )));
                }
            }
        }
        Command::Pause => {
            if let Some(out) = output {
                out.player.pause();
            }
        }
        Command::Resume => {
            if let Some(out) = output {
                out.player.play();
            }
        }
        Command::Stop => {
            if let Some(out) = output.take() {
                out.player.stop();
            }
            shared.analysis.clear();
            playing.track_loaded = false;
            playing.base = Duration::ZERO;
        }
        Command::Seek(position) => {
            if let Some(out) = output {
                match out.player.try_seek(position) {
                    // rodio now reports absolute positions.
                    Ok(()) => playing.base = Duration::ZERO,
                    Err(err) => tracing::warn!(%err, "seek failed"),
                }
            }
            shared.analysis.clear();
        }
        Command::SetVolume(volume) => {
            if let Some(out) = output {
                out.player.set_volume(volume);
            }
        }
    }
}

/// The device is opened lazily on first playback so the app works on
/// machines with no audio output until you actually press play.
fn ensure_output<'a>(
    output: &'a mut Option<Output>,
    on_event: &impl Fn(PlayerEvent),
) -> Option<&'a Output> {
    if output.is_none() {
        match DeviceSinkBuilder::open_default_sink() {
            Ok(device) => {
                let player = Player::connect_new(device.mixer());
                *output = Some(Output {
                    _device: device,
                    player,
                });
            }
            Err(err) => {
                on_event(PlayerEvent::Failed(format!("no audio device: {err}")));
                return None;
            }
        }
    }
    output.as_ref()
}

/// Replace whatever is playing with `source`.
fn start_source(
    out: &Output,
    source: DecodedSource,
    volume: f32,
    paused: bool,
    shared: &Shared,
    on_event: &impl Fn(PlayerEvent),
) {
    shared.analysis.clear();
    out.player.stop();
    out.player.set_volume(volume);
    out.player.append(analyzer::AnalyzedSource::new(
        source,
        Arc::clone(&shared.analysis),
    ));
    if paused {
        out.player.pause();
    } else {
        out.player.play();
    }
    on_event(PlayerEvent::Started);
}

pub(crate) type DecodedSource = Box<dyn Source + Send>;

pub(crate) fn decode_source(
    reader: TrackReader,
    byte_len: Option<u64>,
    mime_type: Option<&str>,
    seekable: bool,
) -> Result<DecodedSource, String> {
    decode_source_with(reader, byte_len, mime_type, seekable, false)
}

/// `decode_source` with optional coarse (byte-estimate) seeking, which
/// remote streams need: an accurate MP3 seek decodes every frame before the
/// target, i.e. downloads it.
pub(crate) fn decode_source_with(
    mut reader: TrackReader,
    byte_len: Option<u64>,
    mime_type: Option<&str>,
    seekable: bool,
    coarse_seek: bool,
) -> Result<DecodedSource, String> {
    let mime_is_opus = mime_type.is_some_and(|mime| {
        let mime = mime.to_ascii_lowercase();
        mime == "audio/opus" || mime.contains("codecs=opus") || mime.contains("codecs=\"opus\"")
    });
    let ogg_is_opus = opus::is_ogg_opus(&mut reader)
        .map_err(|error| format!("cannot inspect audio stream: {error}"))?;
    if mime_is_opus || ogg_is_opus {
        return opus::OggOpusSource::new(reader, byte_len, seekable)
            .map(|source| Box::new(source) as DecodedSource);
    }

    let mut builder = Decoder::builder()
        .with_data(reader)
        .with_seekable(seekable)
        .with_coarse_seek(coarse_seek)
        .with_gapless(true);
    if let Some(len) = byte_len {
        builder = builder.with_byte_len(len);
    }
    if let Some(mime_type) = mime_type {
        builder = builder.with_mime_type(mime_type);
    }
    builder
        .build()
        .map(|decoder| Box::new(decoder) as DecodedSource)
        .map_err(|error| error.to_string())
}
