//! Playback engine: a dedicated audio thread owning the rodio output device
//! and player. The app talks to it through `Controller` (commands over a
//! channel, position/pause state over atomics) and receives `PlayerEvent`s
//! through the callback given to `spawn` — this module knows nothing about
//! the UI or app state.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use rodio::{Decoder, DeviceSinkBuilder, Player, stream::MixerDeviceSink};
use stream_download::StreamDownload;
use stream_download::storage::temp::TempStorageProvider;

pub type TrackReader = StreamDownload<TempStorageProvider>;

/// Perceptual volume: cubic mapping from percent to linear amplitude, so
/// equal percent steps sound like equal loudness steps and low percentages
/// give fine-grained control.
pub fn amplitude(percent: u8) -> f32 {
    let v = f32::from(percent.min(100)) / 100.0;
    v * v * v
}

#[derive(Debug)]
pub enum PlayerEvent {
    /// A track played to its end. `has_next` is true when a prefetched
    /// source was already queued and is now playing gaplessly.
    TrackFinished {
        has_next: bool,
    },
    Failed(String),
}

enum Command {
    Play {
        reader: Box<TrackReader>,
        byte_len: Option<u64>,
        volume: f32,
    },
    /// Append the next track behind the current one without interrupting
    /// playback — rodio switches sources back to back (gapless-ish).
    Enqueue {
        reader: Box<TrackReader>,
        byte_len: Option<u64>,
    },
    TogglePause,
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
}

impl Shared {
    pub fn position(&self) -> Duration {
        Duration::from_millis(self.position_ms.load(Ordering::Relaxed))
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
pub struct Controller {
    tx: Sender<Command>,
    pub shared: Arc<Shared>,
}

impl Controller {
    pub fn play(&self, reader: TrackReader, byte_len: Option<u64>, volume: f32) {
        let _ = self.tx.send(Command::Play {
            reader: Box::new(reader),
            byte_len,
            volume,
        });
    }

    pub fn enqueue(&self, reader: TrackReader, byte_len: Option<u64>) {
        let _ = self.tx.send(Command::Enqueue {
            reader: Box::new(reader),
            byte_len,
        });
    }

    pub fn toggle_pause(&self) {
        let _ = self.tx.send(Command::TogglePause);
    }

    pub fn pause(&self) {
        let _ = self.tx.send(Command::Pause);
    }

    pub fn resume(&self) {
        let _ = self.tx.send(Command::Resume);
    }

    pub fn stop(&self) {
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
    std::thread::Builder::new()
        .name("audio".to_string())
        .spawn(move || run(rx, thread_shared, on_event))
        .expect("spawning the audio thread cannot fail");
    Controller { tx, shared }
}

struct Output {
    /// Keeps the audio device open; dropping it stops the mixer.
    _device: MixerDeviceSink,
    player: Player,
}

fn run(rx: Receiver<Command>, shared: Arc<Shared>, on_event: impl Fn(PlayerEvent)) {
    let mut output: Option<Output> = None;
    let mut track_loaded = false;
    let mut last_len = 0usize;

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(command) => {
                // Commands change the source queue legitimately; resync the
                // length so the next tick doesn't read it as a track ending.
                handle(command, &mut output, &mut track_loaded, &on_event);
                last_len = output.as_ref().map_or(0, |out| out.player.len());
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(out) = &output {
                    shared
                        .position_ms
                        .store(out.player.get_pos().as_millis() as u64, Ordering::Relaxed);
                    shared
                        .paused
                        .store(out.player.is_paused(), Ordering::Relaxed);
                    let len = out.player.len();
                    if track_loaded && len < last_len {
                        if len == 0 {
                            track_loaded = false;
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
    output: &mut Option<Output>,
    track_loaded: &mut bool,
    on_event: &impl Fn(PlayerEvent),
) {
    match command {
        Command::Play {
            reader,
            byte_len,
            volume,
        } => {
            // The device is opened lazily on first playback so the app works
            // on machines with no audio output until you actually press play.
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
                        return;
                    }
                }
            }
            let out = output.as_ref().expect("output opened above");

            let mut builder = Decoder::builder()
                .with_data(reader)
                .with_seekable(true)
                .with_gapless(true);
            if let Some(len) = byte_len {
                builder = builder.with_byte_len(len);
            }
            match builder.build() {
                Ok(decoder) => {
                    out.player.stop();
                    out.player.set_volume(volume);
                    out.player.append(decoder);
                    out.player.play();
                    *track_loaded = true;
                }
                Err(err) => {
                    on_event(PlayerEvent::Failed(format!("cannot decode track: {err}")));
                }
            }
        }
        Command::Enqueue { reader, byte_len } => {
            let Some(out) = output.as_ref() else {
                return;
            };
            let mut builder = Decoder::builder()
                .with_data(reader)
                .with_seekable(true)
                .with_gapless(true);
            if let Some(len) = byte_len {
                builder = builder.with_byte_len(len);
            }
            match builder.build() {
                Ok(decoder) => out.player.append(decoder),
                Err(err) => {
                    on_event(PlayerEvent::Failed(format!(
                        "cannot decode next track: {err}"
                    )));
                }
            }
        }
        Command::TogglePause => {
            if let Some(out) = output {
                if out.player.is_paused() {
                    out.player.play();
                } else {
                    out.player.pause();
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
            if let Some(out) = output {
                out.player.stop();
            }
            *track_loaded = false;
        }
        Command::Seek(position) => {
            if let Some(out) = output {
                if let Err(err) = out.player.try_seek(position) {
                    tracing::warn!(%err, "seek failed");
                }
            }
        }
        Command::SetVolume(volume) => {
            if let Some(out) = output {
                out.player.set_volume(volume);
            }
        }
    }
}
