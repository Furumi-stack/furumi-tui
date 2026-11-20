//! Display meter for visualizations: a third-octave spectrum and stereo levels
//! with analyzer ballistics (fast attack, steady fall, peak hold).
//!
//! The audio thread only writes samples and per-window channel levels into
//! `AnalyzerShared`; the FFT and all smoothing run here, on the reader side,
//! when a snapshot is taken. Ballistics are time-based: rises and falls
//! advance with the clock, not with the number of snapshots.

use std::sync::Arc;
use std::time::Instant;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// Mono samples kept for the spectrum: enough for an ~85 ms window at
/// 192 kHz. The FFT uses the newest `fft_len(rate)` of them.
pub const PCM_SAMPLES: usize = 16_384;

/// Bottom of the display scale: values are normalized from `FLOOR_DB`..0 dBFS
/// to 0.0..1.0, so a full-scale sine reads 1.0.
pub const FLOOR_DB: f32 = -60.0;

/// Nominal ISO third-octave band centres (Hz), 20 Hz .. 20 kHz. These are
/// labels; band edges come from the exact base-2 centres (see `exact_center`)
/// so neighbouring bands meet without gaps or overlaps.
pub const BAND_CENTERS_HZ: [f32; 31] = [
    20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0,
    500.0, 630.0, 800.0, 1_000.0, 1_250.0, 1_600.0, 2_000.0, 2_500.0, 3_150.0, 4_000.0, 5_000.0,
    6_300.0, 8_000.0, 10_000.0, 12_500.0, 16_000.0, 20_000.0,
];

const BANDS: usize = BAND_CENTERS_HZ.len();
/// Index of the 1 kHz band.
const KHZ_BAND: i32 = 17;
const MIN_FFT: usize = 4096;
const BAND_ATTACK_SECS: f32 = 0.025;
const BAND_FALL_DB_PER_SEC: f32 = 36.0;
const PEAK_HOLD_SECS: f32 = 1.2;
const PEAK_FALL_DB_PER_SEC: f32 = 18.0;
const LEVEL_ATTACK_SECS: f32 = 0.05;
const LEVEL_RELEASE_SECS: f32 = 0.3;
/// Longest step applied at once; a stalled reader must not fast-forward a
/// whole second of decay in one frame.
const MAX_STEP_SECS: f32 = 0.25;
/// Audio arrives in device-callback bursts (rodio uses ~46 ms buffers), so a
/// snapshot may see no new samples while playback runs. Only a longer stall
/// (pause, stop) counts as silence.
const STALL_SECS: f32 = 0.15;

/// FFT length for a sample rate: ~85 ms of audio, a power of two.
fn fft_len(sample_rate: u32) -> usize {
    (sample_rate as usize * MIN_FFT / 48_000)
        .next_power_of_two()
        .clamp(MIN_FFT, PCM_SAMPLES)
}

/// Exact base-2 third-octave centre of band `index` (1 kHz at index 17).
fn exact_center(index: usize) -> f32 {
    1_000.0 * 2f32.powf((index as i32 - KHZ_BAND) as f32 / 3.0)
}

/// Linear channel levels of the latest analysis window.
#[derive(Debug, Clone, Copy, Default)]
pub struct StereoReading {
    pub rms_l: f32,
    pub rms_r: f32,
    /// Highest sample peaks since the previous reading.
    pub peak_l: f32,
    pub peak_r: f32,
}

#[derive(Debug, Clone, Default)]
pub struct MeterReading {
    pub spectrum: Vec<f64>,
    pub spectrum_peaks: Vec<f64>,
    pub level_l: f64,
    pub level_r: f64,
    pub peak_l: f64,
    pub peak_r: f64,
}

struct Hold {
    db: f32,
    age: f32,
}

impl Hold {
    fn new() -> Self {
        Self {
            db: FLOOR_DB,
            age: 0.0,
        }
    }

    fn update(&mut self, value_db: f32, dt: f32) {
        if value_db >= self.db {
            self.db = value_db;
            self.age = 0.0;
            return;
        }
        self.age += dt;
        if self.age > PEAK_HOLD_SECS {
            self.db = (self.db - PEAK_FALL_DB_PER_SEC * dt).max(value_db);
        }
    }
}

/// FFT plan, window and band bin ranges for one sample rate.
struct Analysis {
    sample_rate: u32,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Sum of squared window weights, for power normalization.
    window_power: f32,
    pcm: Vec<f32>,
    buffer: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    /// Inclusive bin range per band; `None` above Nyquist.
    bins: [Option<(usize, usize)>; BANDS],
}

impl Analysis {
    fn new(sample_rate: u32) -> Self {
        let len = fft_len(sample_rate);
        let fft = FftPlanner::<f32>::new().plan_fft_forward(len);
        let scratch = vec![Complex::default(); fft.get_inplace_scratch_len()];
        let window: Vec<f32> = (0..len)
            .map(|index| {
                let phase = std::f32::consts::TAU * index as f32 / len as f32;
                0.5 - 0.5 * phase.cos()
            })
            .collect();
        let window_power = window.iter().map(|weight| weight * weight).sum();
        Self {
            sample_rate,
            fft,
            window,
            window_power,
            pcm: vec![0.0; len],
            buffer: vec![Complex::default(); len],
            scratch,
            bins: band_bins(sample_rate, len),
        }
    }

    /// Band power in dBFS (a full-scale sine reads 0 dB in its band). The
    /// caller has filled `pcm`.
    fn band_targets(&mut self) -> [f32; BANDS] {
        for ((slot, sample), weight) in self.buffer.iter_mut().zip(&self.pcm).zip(&self.window) {
            *slot = Complex::new(sample * weight, 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.buffer, &mut self.scratch);
        // One-sided power per bin, normalized so the bins of a sine sum to
        // its mean square (A^2 / 2); doubled below so the sine reads 0 dB.
        let scale = 2.0 / (self.buffer.len() as f32 * self.window_power);
        let mut out = [FLOOR_DB; BANDS];
        for (slot, range) in out.iter_mut().zip(self.bins) {
            if let Some((first, last)) = range {
                let power: f32 = self.buffer[first..=last]
                    .iter()
                    .map(|bin| bin.norm_sqr() * scale)
                    .sum();
                *slot = to_db_power(2.0 * power);
            }
        }
        out
    }
}

/// Partitions the FFT bins between the bands: each bin belongs to exactly one
/// band, from the first bin at or above the band's lower edge up to the bin
/// before the next band starts. A band narrower than one bin (deep bass at
/// short windows) reads the bin that contains its centre, which it then shares
/// with its neighbour.
fn band_bins(sample_rate: u32, len: usize) -> [Option<(usize, usize)>; BANDS] {
    let bin_hz = sample_rate as f32 / len as f32;
    let last_bin = len / 2 - 1;
    let nyquist = sample_rate as f32 / 2.0;
    let edge = 2f32.powf(1.0 / 6.0);
    let first_bin = |index: usize| ((exact_center(index) / edge / bin_hz).ceil() as usize).max(1);
    std::array::from_fn(|index| {
        let low = exact_center(index) / edge;
        if low >= nyquist {
            return None;
        }
        let first = first_bin(index);
        let last = if index + 1 < BANDS {
            first_bin(index + 1).saturating_sub(1)
        } else {
            ((exact_center(index) * edge / bin_hz).floor() as usize).max(first)
        }
        .min(last_bin);
        if first <= last {
            Some((first, last))
        } else {
            let center = ((exact_center(index) / bin_hz).round() as usize).clamp(1, last_bin);
            Some((center, center))
        }
    })
}

pub struct Meter {
    analysis: Option<Analysis>,
    bands_db: [f32; BANDS],
    band_peaks: [Hold; BANDS],
    level_db: [f32; 2],
    level_peaks: [Hold; 2],
    last_update: Option<Instant>,
    last_write: usize,
    /// When the PCM counter last moved, and what that audio measured; reused
    /// between device callbacks until `STALL_SECS` pass.
    last_fresh: Option<Instant>,
    held_targets: [f32; BANDS],
    held_rms: [f32; 2],
}

impl std::fmt::Debug for Meter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Meter")
            .field("bands_db", &self.bands_db)
            .field("level_db", &self.level_db)
            .finish_non_exhaustive()
    }
}

impl Default for Meter {
    fn default() -> Self {
        Self {
            analysis: None,
            bands_db: [FLOOR_DB; BANDS],
            band_peaks: std::array::from_fn(|_| Hold::new()),
            level_db: [FLOOR_DB; 2],
            level_peaks: std::array::from_fn(|_| Hold::new()),
            last_update: None,
            last_write: 0,
            last_fresh: None,
            held_targets: [FLOOR_DB; BANDS],
            held_rms: [0.0; 2],
        }
    }
}

impl Meter {
    /// Advances the meter to `now`. `write` is the analyzer's PCM write
    /// counter: while it keeps moving the newest audio is measured; once it
    /// has not moved for `STALL_SECS` (paused, stopped) the meter is fed
    /// silence and falls instead of freezing. `fill_pcm` copies the newest
    /// `out.len()` mono samples, oldest first.
    pub fn update(
        &mut self,
        now: Instant,
        write: usize,
        sample_rate: u32,
        stereo: StereoReading,
        fill_pcm: impl FnOnce(&mut [f32]),
    ) -> MeterReading {
        let dt = self
            .last_update
            .map(|last| now.saturating_duration_since(last).as_secs_f32())
            .unwrap_or(MAX_STEP_SECS)
            .min(MAX_STEP_SECS);
        self.last_update = Some(now);
        let fresh = write != self.last_write;
        self.last_write = write;

        let (targets, rms, peaks) = if fresh && sample_rate > 0 {
            if self
                .analysis
                .as_ref()
                .is_none_or(|analysis| analysis.sample_rate != sample_rate)
            {
                self.analysis = Some(Analysis::new(sample_rate));
            }
            let analysis = self.analysis.as_mut().expect("analysis was just set");
            fill_pcm(&mut analysis.pcm);
            self.held_targets = analysis.band_targets();
            self.held_rms = [stereo.rms_l, stereo.rms_r];
            self.last_fresh = Some(now);
            (
                self.held_targets,
                self.held_rms,
                [stereo.peak_l, stereo.peak_r],
            )
        } else if self
            .last_fresh
            .is_some_and(|last| now.saturating_duration_since(last).as_secs_f32() <= STALL_SECS)
        {
            (self.held_targets, self.held_rms, [0.0; 2])
        } else {
            ([FLOOR_DB; BANDS], [0.0; 2], [0.0; 2])
        };

        for ((current, target), peak) in self
            .bands_db
            .iter_mut()
            .zip(targets)
            .zip(self.band_peaks.iter_mut())
        {
            *current = follow_band(*current, target, dt);
            peak.update(*current, dt);
        }
        for channel in 0..2 {
            self.level_db[channel] = follow_level(self.level_db[channel], to_db(rms[channel]), dt);
            self.level_peaks[channel].update(to_db(peaks[channel]), dt);
        }

        MeterReading {
            spectrum: self.bands_db.iter().map(|db| normalize(*db)).collect(),
            spectrum_peaks: self
                .band_peaks
                .iter()
                .map(|hold| normalize(hold.db))
                .collect(),
            level_l: normalize(self.level_db[0]),
            level_r: normalize(self.level_db[1]),
            peak_l: normalize(self.level_peaks[0].db),
            peak_r: normalize(self.level_peaks[1].db),
        }
    }
}

fn follow_band(current: f32, target: f32, dt: f32) -> f32 {
    if target > current {
        current + (target - current) * (1.0 - (-dt / BAND_ATTACK_SECS).exp())
    } else {
        (current - BAND_FALL_DB_PER_SEC * dt).max(target)
    }
}

fn follow_level(current: f32, target: f32, dt: f32) -> f32 {
    let tau = if target > current {
        LEVEL_ATTACK_SECS
    } else {
        LEVEL_RELEASE_SECS
    };
    current + (target - current) * (1.0 - (-dt / tau).exp())
}

fn to_db(amplitude: f32) -> f32 {
    if amplitude <= 0.0 || !amplitude.is_finite() {
        FLOOR_DB
    } else {
        (20.0 * amplitude.log10()).max(FLOOR_DB)
    }
}

fn to_db_power(power: f32) -> f32 {
    if power <= 0.0 || !power.is_finite() {
        FLOOR_DB
    } else {
        (10.0 * power.log10()).max(FLOOR_DB)
    }
}

fn normalize(db: f32) -> f64 {
    f64::from(((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn sine(frequency: f32, amplitude: f32, sample_rate: u32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|index| {
                let time = index as f32 / sample_rate as f32;
                amplitude * (std::f32::consts::TAU * frequency * time).sin()
            })
            .collect()
    }

    fn band(center: f32) -> usize {
        BAND_CENTERS_HZ
            .iter()
            .position(|value| *value == center)
            .unwrap()
    }

    /// Feeds a steady tone for `steps` snapshots 50 ms apart.
    fn settle(meter: &mut Meter, frequency: f32, sample_rate: u32, steps: u64) -> MeterReading {
        let samples = sine(frequency, 0.5, sample_rate, PCM_SAMPLES);
        let start = Instant::now();
        let mut reading = MeterReading::default();
        for step in 1..=steps {
            reading = meter.update(
                start + Duration::from_millis(50 * step),
                step as usize,
                sample_rate,
                StereoReading::default(),
                |out| out.copy_from_slice(&samples[..out.len()]),
            );
        }
        reading
    }

    fn band_powers_db(frequency: f32, sample_rate: u32) -> [f32; BANDS] {
        let mut analysis = Analysis::new(sample_rate);
        let len = analysis.pcm.len();
        analysis
            .pcm
            .copy_from_slice(&sine(frequency, 1.0, sample_rate, len));
        analysis.band_targets()
    }

    #[test]
    fn a_sine_lights_its_own_third_octave_band() {
        let reading = settle(&mut Meter::default(), 1_000.0, 48_000, 10);
        let at_1k = reading.spectrum[band(1_000.0)];
        // -6 dBFS on a 60 dB scale reads 0.9.
        assert!((at_1k - 0.9).abs() < 0.03, "1 kHz band read {at_1k}");
        assert!(reading.spectrum[band(250.0)] < 0.2);
        assert!(reading.spectrum[band(4_000.0)] < 0.2);
        assert!(reading.spectrum_peaks[band(1_000.0)] >= at_1k);
    }

    #[test]
    fn bands_cover_the_axis_without_gaps_or_double_counting() {
        for sample_rate in [44_100, 48_000, 96_000] {
            // Up to just below the top band's upper edge (~22.4 kHz) or
            // Nyquist; 1/24-octave steps also land right on band edges.
            let top = (sample_rate as f32 / 2.0 * 0.85).min(exact_center(BANDS - 1) * 1.1);
            let mut frequency = 80.0_f32;
            while frequency < top {
                let powers = band_powers_db(frequency, sample_rate);
                let loudest = powers.iter().copied().fold(FLOOR_DB, f32::max);
                let total: f32 = powers
                    .iter()
                    .filter(|db| **db > FLOOR_DB)
                    .map(|db| 10f32.powf(db / 10.0))
                    .sum();
                assert!(
                    (-3.5..=0.5).contains(&loudest),
                    "{frequency} Hz at {sample_rate}: loudest band {loudest} dB"
                );
                let total_db = 10.0 * total.log10();
                assert!(
                    total_db.abs() < 0.5,
                    "{frequency} Hz at {sample_rate}: bands sum to {total_db} dB"
                );
                frequency *= 2f32.powf(1.0 / 24.0);
            }
        }
    }

    #[test]
    fn deep_bass_reads_at_every_sample_rate() {
        for sample_rate in [44_100, 48_000, 96_000, 192_000] {
            let powers = band_powers_db(31.5, sample_rate);
            assert!(
                powers[band(31.5)] > -3.0,
                "31.5 Hz at {sample_rate}: {} dB",
                powers[band(31.5)]
            );
            assert!(powers[band(1_000.0)] < -40.0);
        }
        // Bands above Nyquist stay at the floor.
        assert_eq!(band_powers_db(1_000.0, 32_000)[band(20_000.0)], FLOOR_DB);
    }

    #[test]
    fn callback_gaps_keep_the_meter_steady_but_a_stall_falls() {
        let mut meter = Meter::default();
        let samples = sine(1_000.0, 0.5, 48_000, PCM_SAMPLES);
        let tone = StereoReading {
            rms_l: 0.35,
            rms_r: 0.35,
            peak_l: 0.5,
            peak_r: 0.5,
        };
        let start = Instant::now();
        let mut write = 0;
        let mut steady = MeterReading::default();
        for step in 1..=40u64 {
            // Every third snapshot lands between two device callbacks.
            if step % 3 != 0 {
                write += 1;
            }
            let reading = meter.update(
                start + Duration::from_millis(50 * step),
                write,
                48_000,
                tone,
                |out| out.copy_from_slice(&samples[..out.len()]),
            );
            if step > 20 {
                assert!(
                    (reading.level_l - steady.level_l).abs() < 0.02 || step == 21,
                    "level jumped at step {step}"
                );
                assert!(
                    (reading.spectrum[band(1_000.0)] - 0.9).abs() < 0.03,
                    "band jumped at step {step}"
                );
            }
            steady = reading;
        }

        // The counter stops for longer than a callback gap: paused.
        let index = band(1_000.0);
        let paused_at = start + Duration::from_millis(2_000);
        let first = meter.update(paused_at, write, 48_000, tone, |_| {});
        let later = meter.update(
            paused_at + Duration::from_millis(500),
            write,
            48_000,
            tone,
            |_| {},
        );
        assert!(later.spectrum[index] < first.spectrum[index]);
        assert!(later.level_l < first.level_l);
        // The peak marker holds for 1.2 s before it falls too.
        assert!((later.spectrum_peaks[index] - 0.9).abs() < 0.03);
        let mut reading = later;
        for step in 1..=20u64 {
            reading = meter.update(
                paused_at + Duration::from_millis(500 + 250 * step),
                write,
                48_000,
                tone,
                |_| {},
            );
        }
        assert_eq!(reading.spectrum[index], 0.0);
        assert!(reading.spectrum_peaks[index] < 0.5);
    }

    #[test]
    fn channel_levels_follow_rms_and_hold_peaks() {
        let mut meter = Meter::default();
        let start = Instant::now();
        let loud_left = StereoReading {
            rms_l: 0.5,
            rms_r: 0.05,
            peak_l: 0.9,
            peak_r: 0.1,
        };
        let mut reading = MeterReading::default();
        for step in 1..=20 {
            reading = meter.update(
                start + Duration::from_millis(50 * step),
                step as usize,
                48_000,
                loud_left,
                |out| out.fill(0.0),
            );
        }
        assert!(reading.level_l > reading.level_r);
        assert!((reading.level_l - normalize(to_db(0.5))).abs() < 0.02);
        assert!((reading.peak_l - normalize(to_db(0.9))).abs() < 0.01);
        assert!(reading.peak_l >= reading.level_l);
    }

    #[test]
    fn silence_and_unknown_rate_read_the_floor() {
        let mut meter = Meter::default();
        let reading = meter.update(Instant::now(), 1, 0, StereoReading::default(), |_| {});
        assert!(reading.spectrum.iter().all(|value| *value == 0.0));
        assert_eq!(reading.level_l, 0.0);
        assert_eq!(reading.spectrum.len(), BAND_CENTERS_HZ.len());
    }

    #[test]
    fn fft_length_tracks_the_sample_rate() {
        assert_eq!(fft_len(44_100), 4096);
        assert_eq!(fft_len(48_000), 4096);
        assert_eq!(fft_len(96_000), 8192);
        assert_eq!(fft_len(192_000), 16_384);
        assert_eq!(fft_len(0), 4096);
    }
}
