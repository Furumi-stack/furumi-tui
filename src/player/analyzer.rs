use std::sync::atomic::{AtomicI16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rodio::source::SeekError;
use rodio::{ChannelCount, Sample, SampleRate, Source};

use super::spectrum::{Meter, PCM_SAMPLES, StereoReading};

const LEVEL_SCALE: f32 = 1_000_000.0;
const SCOPE_SAMPLES: usize = 256;
const SCOPE_SAMPLE_SCALE: f32 = i16::MAX as f32;
const TARGET_ANALYSIS_HZ: f32 = 30.0;
const TARGET_SCOPE_HZ: f32 = 240.0;

#[derive(Debug, Clone, Default)]
pub struct AudioAnalysisSnapshot {
    pub sequence: u64,
    pub energy: f64,
    pub bass: f64,
    pub mid: f64,
    pub treble: f64,
    pub beat: f64,
    pub scope: Vec<f64>,
    /// Third-octave band levels (see `spectrum::BAND_CENTERS_HZ`), 0.0..1.0
    /// over `spectrum::FLOOR_DB`..0 dBFS, with analyzer ballistics.
    pub spectrum: Vec<f64>,
    /// Per-band peak-hold markers on the same scale as `spectrum`.
    pub spectrum_peaks: Vec<f64>,
    /// Left/right RMS levels and held sample peaks on the same dB scale.
    /// Mono sources report the same value for both channels.
    pub level_l: f64,
    pub level_r: f64,
    pub peak_l: f64,
    pub peak_r: f64,
    /// Sample rate of the analyzed stream in Hz (0 when nothing has played
    /// since the player last stopped).
    pub sample_rate: u32,
}

#[derive(Debug)]
pub struct AnalyzerShared {
    sequence: AtomicU64,
    energy: AtomicU32,
    bass: AtomicU32,
    mid: AtomicU32,
    treble: AtomicU32,
    beat: AtomicU32,
    scope_write: AtomicUsize,
    scope: Box<[AtomicI16]>,
    /// Full-rate mono history for the spectrum; written by the audio thread.
    pcm_write: AtomicUsize,
    pcm: Box<[AtomicI16]>,
    sample_rate: AtomicU32,
    /// Per-window channel levels (linear, `LEVEL_SCALE`). Peaks accumulate
    /// with `fetch_max` until the meter consumes them.
    rms_l: AtomicU32,
    rms_r: AtomicU32,
    peak_l: AtomicU32,
    peak_r: AtomicU32,
    /// Ballistics and FFT state. Only snapshot readers (the UI side) lock it;
    /// the audio thread never does.
    meter: Mutex<Meter>,
}

impl Default for AnalyzerShared {
    fn default() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            energy: AtomicU32::new(0),
            bass: AtomicU32::new(0),
            mid: AtomicU32::new(0),
            treble: AtomicU32::new(0),
            beat: AtomicU32::new(0),
            scope_write: AtomicUsize::new(0),
            scope: atomic_ring(SCOPE_SAMPLES),
            pcm_write: AtomicUsize::new(0),
            pcm: atomic_ring(PCM_SAMPLES),
            sample_rate: AtomicU32::new(0),
            rms_l: AtomicU32::new(0),
            rms_r: AtomicU32::new(0),
            peak_l: AtomicU32::new(0),
            peak_r: AtomicU32::new(0),
            meter: Mutex::new(Meter::default()),
        }
    }
}

fn atomic_ring(len: usize) -> Box<[AtomicI16]> {
    (0..len)
        .map(|_| AtomicI16::new(0))
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

impl AnalyzerShared {
    pub fn clear(&self) {
        self.energy.store(0, Ordering::Relaxed);
        self.bass.store(0, Ordering::Relaxed);
        self.mid.store(0, Ordering::Relaxed);
        self.treble.store(0, Ordering::Relaxed);
        self.beat.store(0, Ordering::Relaxed);
        self.scope_write.store(0, Ordering::Relaxed);
        for sample in self.scope.iter() {
            sample.store(0, Ordering::Relaxed);
        }
        self.pcm_write.store(0, Ordering::Relaxed);
        for sample in self.pcm.iter() {
            sample.store(0, Ordering::Relaxed);
        }
        for level in [&self.rms_l, &self.rms_r, &self.peak_l, &self.peak_r] {
            level.store(0, Ordering::Relaxed);
        }
        self.sequence.store(0, Ordering::Relaxed);
    }

    /// Like [`Self::clear`], and also forgets the sample rate: nothing is
    /// loaded any more.
    pub fn reset(&self) {
        self.clear();
        self.sample_rate.store(0, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> AudioAnalysisSnapshot {
        self.snapshot_at(Instant::now())
    }

    /// Like [`Self::snapshot`] with an explicit clock, so meter ballistics are
    /// deterministic in tests. Ballistics are time-based: how often callers
    /// read does not change how fast bars rise or fall.
    pub fn snapshot_at(&self, now: Instant) -> AudioAnalysisSnapshot {
        let write = self.scope_write.load(Ordering::Relaxed);
        let len = self.scope.len().max(1);
        let scope = (0..self.scope.len())
            .map(|offset| {
                let index = (write + offset) % len;
                f64::from(self.scope[index].load(Ordering::Relaxed)) / f64::from(i16::MAX)
            })
            .collect();

        let sample_rate = self.sample_rate.load(Ordering::Relaxed);
        let pcm_write = self.pcm_write.load(Ordering::Relaxed);
        let stereo = StereoReading {
            rms_l: load_norm(&self.rms_l) as f32,
            rms_r: load_norm(&self.rms_r) as f32,
            peak_l: f64::from(self.peak_l.swap(0, Ordering::Relaxed)) as f32 / LEVEL_SCALE,
            peak_r: f64::from(self.peak_r.swap(0, Ordering::Relaxed)) as f32 / LEVEL_SCALE,
        };
        let reading = {
            let mut meter = self
                .meter
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            meter.update(now, pcm_write, sample_rate, stereo, |out| {
                self.copy_pcm(pcm_write, out)
            })
        };

        AudioAnalysisSnapshot {
            sequence: self.sequence.load(Ordering::Relaxed),
            energy: load_norm(&self.energy),
            bass: load_norm(&self.bass),
            mid: load_norm(&self.mid),
            treble: load_norm(&self.treble),
            beat: load_norm(&self.beat),
            scope,
            spectrum: reading.spectrum,
            spectrum_peaks: reading.spectrum_peaks,
            level_l: reading.level_l,
            level_r: reading.level_r,
            peak_l: reading.peak_l,
            peak_r: reading.peak_r,
            sample_rate,
        }
    }

    /// Copies the newest `out.len()` mono samples, oldest first. Slots not
    /// written since the last clear read as silence.
    fn copy_pcm(&self, write: usize, out: &mut [f32]) {
        let len = self.pcm.len();
        let count = out.len().min(len);
        let start = write.wrapping_sub(count);
        for (offset, slot) in out.iter_mut().enumerate() {
            *slot = if offset < count && write >= count - offset {
                let index = start.wrapping_add(offset) % len;
                f32::from(self.pcm[index].load(Ordering::Relaxed)) / SCOPE_SAMPLE_SCALE
            } else {
                0.0
            };
        }
    }

    fn store_levels(&self, energy: f32, bass: f32, mid: f32, treble: f32, beat: f32) {
        store_norm(&self.energy, energy);
        store_norm(&self.bass, bass);
        store_norm(&self.mid, mid);
        store_norm(&self.treble, treble);
        store_norm(&self.beat, beat);
        self.sequence.fetch_add(1, Ordering::Relaxed);
    }

    fn push_pcm(&self, sample: f32) {
        let index = self.pcm_write.fetch_add(1, Ordering::Relaxed) % self.pcm.len();
        let value = (sample.clamp(-1.0, 1.0) * SCOPE_SAMPLE_SCALE).round() as i16;
        self.pcm[index].store(value, Ordering::Relaxed);
    }

    fn store_channels(&self, sample_rate: f32, rms: [f32; 2], peak: [f32; 2]) {
        self.sample_rate
            .store(sample_rate.round() as u32, Ordering::Relaxed);
        store_norm(&self.rms_l, rms[0]);
        store_norm(&self.rms_r, rms[1]);
        self.peak_l.fetch_max(norm_bits(peak[0]), Ordering::Relaxed);
        self.peak_r.fetch_max(norm_bits(peak[1]), Ordering::Relaxed);
    }

    fn push_scope(&self, sample: f32) {
        let index = self.scope_write.fetch_add(1, Ordering::Relaxed) % self.scope.len();
        let value = (sample.clamp(-1.0, 1.0) * SCOPE_SAMPLE_SCALE).round() as i16;
        self.scope[index].store(value, Ordering::Relaxed);
    }
}

pub struct AnalyzedSource<S> {
    input: S,
    shared: Arc<AnalyzerShared>,
    state: AnalyzerState,
}

impl<S> AnalyzedSource<S>
where
    S: Source,
{
    pub fn new(input: S, shared: Arc<AnalyzerShared>) -> Self {
        let channels = input.channels();
        let sample_rate = input.sample_rate();
        Self {
            input,
            shared,
            state: AnalyzerState::new(channels, sample_rate),
        }
    }
}

impl<S> Iterator for AnalyzedSource<S>
where
    S: Source,
{
    type Item = Sample;

    fn next(&mut self) -> Option<Self::Item> {
        let sample = self.input.next()?;
        self.state
            .ensure_format(self.input.channels(), self.input.sample_rate());
        self.state.accept_sample(sample as f32, &self.shared);
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.input.size_hint()
    }
}

impl<S> Source for AnalyzedSource<S>
where
    S: Source,
{
    fn current_span_len(&self) -> Option<usize> {
        self.input.current_span_len()
    }

    fn channels(&self) -> ChannelCount {
        self.input.channels()
    }

    fn sample_rate(&self) -> SampleRate {
        self.input.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.input.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        let result = self.input.try_seek(pos);
        if result.is_ok() {
            self.shared.clear();
            self.state.reset_filters();
        }
        result
    }
}

#[derive(Debug)]
struct AnalyzerState {
    channels: usize,
    sample_rate: f32,
    bass_alpha: f32,
    mid_alpha: f32,
    channel_index: usize,
    frame_sum: f32,
    /// Left/right of the current frame (mono feeds both).
    frame_lr: [f32; 2],
    channel_sq: [f32; 2],
    channel_peak: [f32; 2],
    frame_count: usize,
    window_frames: usize,
    scope_counter: usize,
    scope_stride: usize,
    low_bass: f32,
    low_mid: f32,
    full_sq: f32,
    bass_sq: f32,
    mid_sq: f32,
    treble_sq: f32,
    slow_energy: f32,
    smooth_energy: f32,
    smooth_bass: f32,
    smooth_mid: f32,
    smooth_treble: f32,
    smooth_beat: f32,
}

impl AnalyzerState {
    fn new(channels: ChannelCount, sample_rate: SampleRate) -> Self {
        let mut state = Self {
            channels: channels.get() as usize,
            sample_rate: sample_rate.get() as f32,
            bass_alpha: 0.0,
            mid_alpha: 0.0,
            channel_index: 0,
            frame_sum: 0.0,
            frame_lr: [0.0; 2],
            channel_sq: [0.0; 2],
            channel_peak: [0.0; 2],
            frame_count: 0,
            window_frames: 0,
            scope_counter: 0,
            scope_stride: 0,
            low_bass: 0.0,
            low_mid: 0.0,
            full_sq: 0.0,
            bass_sq: 0.0,
            mid_sq: 0.0,
            treble_sq: 0.0,
            slow_energy: 0.0,
            smooth_energy: 0.0,
            smooth_bass: 0.0,
            smooth_mid: 0.0,
            smooth_treble: 0.0,
            smooth_beat: 0.0,
        };
        state.configure();
        state
    }

    fn ensure_format(&mut self, channels: ChannelCount, sample_rate: SampleRate) {
        let channels = channels.get() as usize;
        let sample_rate = sample_rate.get() as f32;
        if self.channels != channels || (self.sample_rate - sample_rate).abs() >= 1.0 {
            self.channels = channels;
            self.sample_rate = sample_rate;
            self.configure();
            self.reset_filters();
        }
    }

    fn configure(&mut self) {
        self.channels = self.channels.max(1);
        self.sample_rate = self.sample_rate.max(1.0);
        self.bass_alpha = lowpass_alpha(180.0, self.sample_rate);
        self.mid_alpha = lowpass_alpha(2_400.0, self.sample_rate);
        self.window_frames = (self.sample_rate / TARGET_ANALYSIS_HZ).round().max(256.0) as usize;
        self.scope_stride = (self.sample_rate / TARGET_SCOPE_HZ).round().max(1.0) as usize;
    }

    fn reset_filters(&mut self) {
        self.channel_index = 0;
        self.frame_sum = 0.0;
        self.frame_lr = [0.0; 2];
        self.channel_sq = [0.0; 2];
        self.channel_peak = [0.0; 2];
        self.frame_count = 0;
        self.scope_counter = 0;
        self.low_bass = 0.0;
        self.low_mid = 0.0;
        self.full_sq = 0.0;
        self.bass_sq = 0.0;
        self.mid_sq = 0.0;
        self.treble_sq = 0.0;
        self.slow_energy = 0.0;
        self.smooth_energy = 0.0;
        self.smooth_bass = 0.0;
        self.smooth_mid = 0.0;
        self.smooth_treble = 0.0;
        self.smooth_beat = 0.0;
    }

    fn accept_sample(&mut self, sample: f32, shared: &AnalyzerShared) {
        let sample = if sample.is_finite() {
            sample.clamp(-1.5, 1.5)
        } else {
            0.0
        };
        self.frame_sum += sample;
        // First channel is left, second right; extra channels only feed mono.
        if self.channel_index < 2 {
            self.frame_lr[self.channel_index] = sample;
        }
        self.channel_index += 1;
        if self.channel_index < self.channels {
            return;
        }
        if self.channels == 1 {
            self.frame_lr[1] = self.frame_lr[0];
        }
        for (channel, value) in self.frame_lr.iter().enumerate() {
            self.channel_sq[channel] += value * value;
            self.channel_peak[channel] = self.channel_peak[channel].max(value.abs());
        }

        let mono = self.frame_sum / self.channels as f32;
        self.channel_index = 0;
        self.frame_sum = 0.0;
        self.accept_frame(mono, shared);
    }

    fn accept_frame(&mut self, sample: f32, shared: &AnalyzerShared) {
        self.low_bass += self.bass_alpha * (sample - self.low_bass);
        self.low_mid += self.mid_alpha * (sample - self.low_mid);

        let bass = self.low_bass;
        let mid = self.low_mid - self.low_bass;
        let treble = sample - self.low_mid;

        self.full_sq += sample * sample;
        self.bass_sq += bass * bass;
        self.mid_sq += mid * mid;
        self.treble_sq += treble * treble;
        self.frame_count += 1;
        shared.push_pcm(sample);

        self.scope_counter += 1;
        if self.scope_counter >= self.scope_stride {
            self.scope_counter = 0;
            shared.push_scope(sample);
        }

        if self.frame_count >= self.window_frames {
            self.publish_window(shared);
        }
    }

    fn publish_window(&mut self, shared: &AnalyzerShared) {
        let frames = self.frame_count.max(1) as f32;
        let energy = compress_rms((self.full_sq / frames).sqrt(), 7.5);
        let bass = compress_rms((self.bass_sq / frames).sqrt(), 11.0);
        let mid = compress_rms((self.mid_sq / frames).sqrt(), 15.0);
        let treble = compress_rms((self.treble_sq / frames).sqrt(), 22.0);

        if self.slow_energy == 0.0 {
            self.slow_energy = energy;
        } else {
            self.slow_energy = self.slow_energy * 0.94 + energy * 0.06;
        }
        let beat_raw = ((energy - self.slow_energy * 1.18) * 5.5).clamp(0.0, 1.0);

        self.smooth_energy = smooth_level(self.smooth_energy, energy, 0.35, 0.82);
        self.smooth_bass = smooth_level(self.smooth_bass, bass, 0.30, 0.80);
        self.smooth_mid = smooth_level(self.smooth_mid, mid, 0.35, 0.82);
        self.smooth_treble = smooth_level(self.smooth_treble, treble, 0.28, 0.76);
        self.smooth_beat = smooth_level(self.smooth_beat, beat_raw, 0.18, 0.70);

        shared.store_levels(
            self.smooth_energy,
            self.smooth_bass,
            self.smooth_mid,
            self.smooth_treble,
            self.smooth_beat,
        );

        shared.store_channels(
            self.sample_rate,
            [
                (self.channel_sq[0] / frames).sqrt(),
                (self.channel_sq[1] / frames).sqrt(),
            ],
            self.channel_peak,
        );

        self.full_sq = 0.0;
        self.bass_sq = 0.0;
        self.mid_sq = 0.0;
        self.treble_sq = 0.0;
        self.channel_sq = [0.0; 2];
        self.channel_peak = [0.0; 2];
        self.frame_count = 0;
    }
}

fn lowpass_alpha(cutoff_hz: f32, sample_rate: f32) -> f32 {
    1.0 - (-std::f32::consts::TAU * cutoff_hz / sample_rate.max(1.0)).exp()
}

fn compress_rms(rms: f32, scale: f32) -> f32 {
    (1.0 - (-rms.max(0.0) * scale).exp()).clamp(0.0, 1.0)
}

fn smooth_level(previous: f32, next: f32, attack: f32, release: f32) -> f32 {
    let keep = if next > previous { attack } else { release };
    previous * keep + next * (1.0 - keep)
}

fn store_norm(target: &AtomicU32, value: f32) {
    target.store(norm_bits(value), Ordering::Relaxed);
}

fn norm_bits(value: f32) -> u32 {
    (value.clamp(0.0, 1.0) * LEVEL_SCALE).round() as u32
}

fn load_norm(source: &AtomicU32) -> f64 {
    f64::from(source.load(Ordering::Relaxed)) / f64::from(LEVEL_SCALE)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestSource {
        samples: Vec<Sample>,
        cursor: usize,
        channels: ChannelCount,
        sample_rate: SampleRate,
    }

    impl TestSource {
        fn sine(frames: usize, channels: u16, sample_rate: u32, frequency: f32) -> Self {
            let channels_count = channels.max(1);
            let mut samples = Vec::with_capacity(frames * usize::from(channels_count));
            for frame in 0..frames {
                let time = frame as f32 / sample_rate as f32;
                let sample = (std::f32::consts::TAU * frequency * time).sin() * 0.5;
                for _ in 0..channels_count {
                    samples.push(sample as Sample);
                }
            }
            Self {
                samples,
                cursor: 0,
                channels: ChannelCount::new(channels_count).unwrap(),
                sample_rate: SampleRate::new(sample_rate).unwrap(),
            }
        }
    }

    impl Iterator for TestSource {
        type Item = Sample;

        fn next(&mut self) -> Option<Self::Item> {
            let sample = self.samples.get(self.cursor).copied()?;
            self.cursor += 1;
            Some(sample)
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            let remaining = self.samples.len().saturating_sub(self.cursor);
            (remaining, Some(remaining))
        }
    }

    impl Source for TestSource {
        fn current_span_len(&self) -> Option<usize> {
            Some(self.samples.len().saturating_sub(self.cursor))
        }

        fn channels(&self) -> ChannelCount {
            self.channels
        }

        fn sample_rate(&self) -> SampleRate {
            self.sample_rate
        }

        fn total_duration(&self) -> Option<Duration> {
            let frames = self.samples.len() / usize::from(self.channels.get());
            Some(Duration::from_secs_f64(
                frames as f64 / f64::from(self.sample_rate.get()),
            ))
        }
    }

    #[test]
    fn analyzed_source_publishes_levels_and_scope() {
        let shared = Arc::new(AnalyzerShared::default());
        let source = TestSource::sine(4_096, 2, 48_000, 110.0);
        let analyzed = AnalyzedSource::new(source, Arc::clone(&shared));

        for _ in analyzed {}

        let snapshot = shared.snapshot();
        assert!(snapshot.sequence > 0);
        assert!(snapshot.energy > 0.0);
        assert!(snapshot.bass > 0.0);
        assert!(snapshot.scope.iter().any(|sample| sample.abs() > 0.001));
    }

    #[test]
    fn analyzed_source_feeds_the_spectrum_and_channel_meters() {
        let shared = Arc::new(AnalyzerShared::default());
        let source = TestSource::sine(48_000, 2, 48_000, 1_000.0);
        let analyzed = AnalyzedSource::new(source, Arc::clone(&shared));

        for _ in analyzed {}

        let snapshot = shared.snapshot();
        assert_eq!(snapshot.sample_rate, 48_000);
        let bands = crate::player::BAND_CENTERS_HZ;
        let at_1k = bands.iter().position(|hz| *hz == 1_000.0).unwrap();
        let loudest = snapshot
            .spectrum
            .iter()
            .enumerate()
            .max_by(|left, right| left.1.total_cmp(right.1))
            .map(|(index, _)| index);
        assert_eq!(loudest, Some(at_1k));
        assert!(snapshot.level_l > 0.5 && (snapshot.level_l - snapshot.level_r).abs() < 1e-6);
        assert!(snapshot.peak_l >= snapshot.level_l);
    }

    #[test]
    fn pcm_history_reads_newest_samples_and_silence_before_the_first() {
        let shared = AnalyzerShared::default();
        for value in [0.25_f32, 0.5, -0.5] {
            shared.push_pcm(value);
        }
        let mut out = [1.0_f32; 5];
        shared.copy_pcm(shared.pcm_write.load(Ordering::Relaxed), &mut out);
        let rounded: Vec<f32> = out
            .iter()
            .map(|value| (value * 4.0).round() / 4.0)
            .collect();
        assert_eq!(rounded, [0.0, 0.0, 0.25, 0.5, -0.5]);

        shared.clear();
        shared.copy_pcm(shared.pcm_write.load(Ordering::Relaxed), &mut out);
        assert!(out.iter().all(|value| *value == 0.0));
    }
}
