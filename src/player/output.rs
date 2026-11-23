//! Hardware output is replaceable. Rodio's source queue and playback controls
//! outlive the hardware sink, so unplugging never finishes or reopens a track.
use std::num::{NonZeroU16, NonZeroU32};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    mpsc::{self, RecvTimeoutError},
};
use std::time::{Duration, Instant};

use rodio::{
    DeviceSinkBuilder, Player, Source,
    cpal::{
        self,
        traits::{DeviceTrait, HostTrait},
    },
    queue::SourcesQueueOutput,
    source::UniformSourceIterator,
    stream::MixerDeviceSink,
};

#[derive(Clone, Copy)]
struct AudioFormat {
    channels: NonZeroU16,
    sample_rate: NonZeroU32,
}

impl AudioFormat {
    const FALLBACK: Self = Self {
        channels: NonZeroU16::new(2).unwrap(),
        sample_rate: NonZeroU32::new(48_000).unwrap(),
    };

    fn current_default() -> Self {
        let config = cpal::default_host()
            .default_output_device()
            .and_then(|device| device.default_output_config().ok());
        config
            .and_then(|config| {
                Some(Self {
                    channels: NonZeroU16::new(config.channels())?,
                    sample_rate: NonZeroU32::new(config.sample_rate())?,
                })
            })
            .unwrap_or(Self::FALLBACK)
    }

    fn queue(self, input: SourcesQueueOutput) -> UniformSourceIterator<SourcesQueueOutput> {
        UniformSourceIterator::new(input, self.channels, self.sample_rate)
    }

    fn aligned_samples(self, suggested: usize) -> usize {
        let channels = usize::from(self.channels.get());
        (suggested / channels).max(1) * channels
    }
}
const CHUNK: usize = 64;
const PROBE_INTERVAL: Duration = Duration::from_millis(500);
const STALL_TIME: Duration = Duration::from_secs(2);

struct Audio {
    format: AudioFormat,
    queue: Mutex<UniformSourceIterator<SourcesQueueOutput>>,
    // Rodio services synchronous seek orders when the source is consumed.
    // Without hardware, the monitor can advance a muted buffer to service one.
    seeking: AtomicBool,
    samples: AtomicU64,
    generation: AtomicU64,
}

pub(super) struct Output {
    // Keeping the sender alive keeps the monitor active.
    _monitor: mpsc::Sender<()>,
    audio: Arc<Audio>,
    pub(super) player: Player,
}

impl Output {
    pub fn new() -> Self {
        let (player, queue) = Player::new();
        // Keep the starting device's native format in steady state. A
        // replacement sink resamples only when its format differs.
        let format = AudioFormat::current_default();
        let audio = Arc::new(Audio {
            format,
            queue: Mutex::new(format.queue(queue)),
            seeking: AtomicBool::new(false),
            samples: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        });
        let (monitor, rx) = mpsc::channel();
        let monitor_audio = Arc::clone(&audio);
        std::thread::Builder::new()
            .name("audio-output".into())
            .spawn(move || watch_outputs(rx, monitor_audio))
            .expect("spawning the audio output monitor cannot fail");
        Self {
            _monitor: monitor,
            audio,
            player,
        }
    }

    pub fn reset_player(&mut self) {
        // Rodio waits for the previous callback when appending after stop.
        // Replace the queue instead, so starting another track can never wait
        // for a callback from a disconnected device.
        let (player, queue) = Player::new();
        *self
            .audio
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = self.audio.format.queue(queue);
        self.audio.generation.fetch_add(1, Relaxed);
        self.player = player;
    }

    pub fn seek(&self, position: Duration) -> Result<(), String> {
        self.audio.seeking.store(true, Relaxed);
        let result = self
            .player
            .try_seek(position)
            .map_err(|error| error.to_string());
        self.audio.seeking.store(false, Relaxed);
        result
    }
}

struct DeviceOutput {
    id: Option<cpal::DeviceId>,
    _sink: MixerDeviceSink,
    failed: Arc<AtomicBool>,
    last_samples: u64,
    last_callback: Instant,
}

impl DeviceOutput {
    fn open(
        device: cpal::Device,
        id: Option<cpal::DeviceId>,
        audio: &Arc<Audio>,
    ) -> Result<Self, rodio::stream::DeviceSinkError> {
        let failed = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&failed);
        let mut sink = DeviceSinkBuilder::from_device(device)?
            .with_error_callback(move |_| signal.store(true, Relaxed))
            .open_sink_or_fallback()?;
        sink.log_on_drop(false);
        sink.mixer().add(RoutedSource::new(Arc::clone(audio)));
        Ok(Self {
            id,
            _sink: sink,
            failed,
            last_samples: audio.samples.load(Relaxed),
            last_callback: Instant::now(),
        })
    }
}

fn watch_outputs(rx: mpsc::Receiver<()>, audio: Arc<Audio>) {
    let host = cpal::default_host();
    let mut active: Option<DeviceOutput> = None;
    let mut unavailable = false;
    loop {
        let device = host.default_output_device();
        let id = device.as_ref().and_then(|device| device.id().ok());
        let now = Instant::now();
        if let Some(output) = &mut active {
            let samples = audio.samples.load(Relaxed);
            if samples != output.last_samples {
                output.last_samples = samples;
                output.last_callback = now;
            }
            if output.failed.load(Relaxed)
                || now.duration_since(output.last_callback) >= STALL_TIME
                || device.is_none()
                || matches!((&output.id, &id), (Some(old), Some(new)) if old != new)
            {
                // Drop the old callback first: only one sink may draw samples.
                active = None;
            }
        }
        if active.is_none() {
            if let Some(device) = device {
                match DeviceOutput::open(device, id, &audio) {
                    Ok(output) => {
                        if unavailable {
                            tracing::info!("audio output restored");
                        }
                        active = Some(output);
                    }
                    Err(error) if !unavailable => {
                        tracing::debug!(%error, "could not open audio output")
                    }
                    Err(_) => {}
                }
            }
            if active.is_none() && !unavailable {
                tracing::warn!("waiting for a usable default audio output");
            }
            unavailable = active.is_none();
        }

        if active.is_none() && audio.seeking.load(Relaxed) {
            let mut queue = audio
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let channels = usize::from(audio.format.channels.get());
            for _ in 0..(1024 / channels).max(1) {
                if !audio.seeking.load(Relaxed) {
                    break;
                }
                for _ in 0..channels {
                    queue.next();
                }
            }
        }
        match rx.recv_timeout(PROBE_INTERVAL) {
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) | Ok(()) => {}
        }
    }
}

struct RoutedSource {
    audio: Arc<Audio>,
    chunk: Vec<f32>,
    index: usize,
    generation: u64,
}

impl RoutedSource {
    fn new(audio: Arc<Audio>) -> Self {
        let size = audio.format.aligned_samples(CHUNK);
        Self {
            audio,
            chunk: vec![0.0; size],
            index: size,
            generation: 0,
        }
    }
}

impl Iterator for RoutedSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let generation = self.audio.generation.load(Relaxed);
        if self.index == self.chunk.len() || self.generation != generation {
            let mut queue = self
                .audio
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for sample in &mut self.chunk {
                *sample = queue.next().unwrap_or(0.0);
            }
            self.audio
                .samples
                .fetch_add(self.chunk.len() as u64, Relaxed);
            self.index = 0;
            self.generation = generation;
        }
        let sample = self.chunk[self.index];
        self.index += 1;
        Some(sample)
    }
}

impl Source for RoutedSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> NonZeroU16 {
        self.audio.format.channels
    }
    fn sample_rate(&self) -> NonZeroU32 {
        self.audio.format.sample_rate
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_the_sink_preserves_the_rodio_queue_and_position() {
        let (player, queue) = Player::new();
        let format = AudioFormat::FALLBACK;
        let audio = Arc::new(Audio {
            format,
            queue: Mutex::new(format.queue(queue)),
            seeking: AtomicBool::new(false),
            samples: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        });
        player.append(rodio::buffer::SamplesBuffer::new(
            format.channels,
            format.sample_rate,
            vec![0.5; 96_000],
        ));
        player.append(rodio::buffer::SamplesBuffer::new(
            format.channels,
            format.sample_rate,
            vec![0.8; 96_000],
        ));
        let mut first = RoutedSource::new(Arc::clone(&audio));
        first.by_ref().take(9_600).for_each(|_| {});
        let previous = player.get_pos();
        drop(first);
        assert_eq!(player.len(), 2);
        player.pause();
        let mut reconnected = RoutedSource::new(audio);
        reconnected.by_ref().take(9_600).for_each(|_| {});
        assert!(player.is_paused());
        assert!(player.get_pos() - previous < Duration::from_millis(10));
        player.play();
        reconnected.take(9_600).for_each(|_| {});
        assert!(player.get_pos() > previous + Duration::from_millis(50));
        assert_eq!(player.len(), 2);
    }
}
