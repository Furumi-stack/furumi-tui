//! Ogg/Opus decoding for rodio.
//!
//! Rodio 0.22 can demux Ogg and decode Vorbis, but its Symphonia version does
//! not include an Opus decoder.  This source keeps Symphonia's mature Ogg
//! demuxing and feeds the packets into the pure-Rust `rusty-opus` decoder.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

use rodio::source::SeekError;
use rodio::{ChannelCount, SampleRate, Source};
use rusty_opus::OpusDecoder;
use symphonia::core::codecs::{CODEC_TYPE_OPUS, CodecParameters};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use super::TrackReader;

const OPUS_SAMPLE_RATE: u32 = 48_000;
const OPUS_SEEK_PREROLL_FRAMES: u64 = OPUS_SAMPLE_RATE as u64 * 80 / 1_000;
const SNIFF_BYTES: usize = 512;

/// Inspect the Ogg identification page without changing the reader position.
pub(super) fn is_ogg_opus(reader: &mut TrackReader) -> io::Result<bool> {
    let position = reader.stream_position()?;
    let mut header = [0; SNIFF_BYTES];
    let read_result = reader.read(&mut header);
    let rewind_result = reader.seek(SeekFrom::Start(position));

    let read = read_result?;
    rewind_result?;
    Ok(header[..read].starts_with(b"OggS")
        && header[..read]
            .windows(b"OpusHead".len())
            .any(|window| window == b"OpusHead"))
}

struct ReaderMediaSource {
    reader: TrackReader,
    byte_len: Option<u64>,
    seekable: bool,
}

impl Read for ReaderMediaSource {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buffer)
    }
}

impl Seek for ReaderMediaSource {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.reader.seek(position)
    }
}

impl MediaSource for ReaderMediaSource {
    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn byte_len(&self) -> Option<u64> {
        self.byte_len
    }
}

pub(super) struct OggOpusSource {
    format: Box<dyn FormatReader>,
    decoder: OpusDecoder,
    track_id: u32,
    channels: u16,
    pre_skip: u64,
    playable_frames: Option<u64>,
    output_position_frames: u64,
    discard_frames: u64,
    output_gain: f32,
    buffer: Vec<f32>,
    scratch: Vec<f32>,
    buffer_position: usize,
    seekable: bool,
    done: bool,
}

impl OggOpusSource {
    pub(super) fn new(
        reader: TrackReader,
        byte_len: Option<u64>,
        seekable: bool,
    ) -> Result<Self, String> {
        let source = ReaderMediaSource {
            reader,
            byte_len,
            seekable,
        };
        let stream = MediaSourceStream::new(Box::new(source), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("ogg");
        hint.mime_type("audio/ogg");
        let format_options = FormatOptions {
            enable_gapless: false,
            ..Default::default()
        };
        let probed = symphonia::default::get_probe()
            .format(&hint, stream, &format_options, &MetadataOptions::default())
            .map_err(|error| format!("cannot read Ogg container: {error}"))?;
        let format = probed.format;

        let (track_id, channels, pre_skip, playable_frames, output_gain) = {
            let track = format
                .default_track()
                .ok_or_else(|| "Ogg container has no audio track".to_string())?;
            if track.codec_params.codec != CODEC_TYPE_OPUS {
                return Err("Ogg track is not Opus".to_string());
            }
            let channels = track
                .codec_params
                .channels
                .map(|channels| channels.count() as u16)
                .ok_or_else(|| "Opus track has no channel layout".to_string())?;
            if !(1..=2).contains(&channels) {
                return Err(format!(
                    "Opus track has {channels} channels; only mono and stereo are supported"
                ));
            }
            let extra_data = track.codec_params.extra_data.as_deref();
            let pre_skip = opus_pre_skip(extra_data);
            let playable_frames = opus_playable_frames(&track.codec_params, pre_skip);
            let output_gain = opus_output_gain(extra_data);
            (track.id, channels, pre_skip, playable_frames, output_gain)
        };
        let decoder = OpusDecoder::new(OPUS_SAMPLE_RATE as i32, usize::from(channels))
            .map_err(|error| format!("cannot initialize Opus decoder: {error}"))?;

        let mut source = Self {
            format,
            decoder,
            track_id,
            channels,
            pre_skip,
            playable_frames,
            output_position_frames: 0,
            discard_frames: pre_skip,
            output_gain,
            buffer: Vec::new(),
            scratch: Vec::new(),
            buffer_position: 0,
            seekable,
            done: false,
        };
        source.fill_buffer();
        if source.buffer.is_empty() {
            return Err("Opus track contains no decodable audio".to_string());
        }
        Ok(source)
    }

    fn fill_buffer(&mut self) {
        self.buffer.clear();
        self.buffer_position = 0;

        while self.buffer.is_empty() && !self.done {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(error))
                    if error.kind() == io::ErrorKind::UnexpectedEof =>
                {
                    self.done = true;
                    break;
                }
                Err(error) => {
                    tracing::warn!(%error, "Ogg/Opus demux failed");
                    self.done = true;
                    break;
                }
            };
            if packet.track_id() != self.track_id {
                continue;
            }

            let Ok(decoded_frames) = usize::try_from(packet.dur) else {
                tracing::warn!("Ogg/Opus packet duration is too large");
                self.done = true;
                break;
            };
            if decoded_frames == 0 {
                continue;
            }

            let sample_count = match decoded_frames.checked_mul(usize::from(self.channels)) {
                Some(sample_count) => sample_count,
                None => {
                    tracing::warn!("Ogg/Opus packet sample count overflow");
                    self.done = true;
                    break;
                }
            };
            self.scratch.resize(sample_count, 0.0);
            let frames = match self
                .decoder
                .decode(&packet.data, decoded_frames, &mut self.scratch)
            {
                Ok(frames) => frames,
                Err(error) => {
                    tracing::warn!(%error, "Opus packet decode failed");
                    continue;
                }
            };

            let discarded = self.discard_frames.min(frames as u64) as usize;
            self.discard_frames -= discarded as u64;
            let start_frame = discarded;
            let remaining = self
                .playable_frames
                .map(|total| total.saturating_sub(self.output_position_frames))
                .unwrap_or(u64::MAX);
            let end_frame = frames
                .min(start_frame.saturating_add(usize::try_from(remaining).unwrap_or(usize::MAX)));
            if start_frame >= end_frame {
                if self
                    .playable_frames
                    .is_some_and(|total| self.output_position_frames >= total)
                {
                    self.done = true;
                }
                continue;
            }

            let channels = usize::from(self.channels);
            let start = start_frame * channels;
            let end = end_frame.saturating_mul(channels).min(self.scratch.len());
            self.output_position_frames = self
                .output_position_frames
                .saturating_add((end_frame - start_frame) as u64);
            self.buffer.extend(
                self.scratch[start..end]
                    .iter()
                    .map(|sample| (sample * self.output_gain).clamp(-1.0, 1.0)),
            );
            if self
                .playable_frames
                .is_some_and(|total| self.output_position_frames >= total)
            {
                self.done = true;
            }
        }
    }

    fn reset_decoder(&mut self) -> Result<(), SeekError> {
        self.decoder = OpusDecoder::new(OPUS_SAMPLE_RATE as i32, usize::from(self.channels))
            .map_err(|error| seek_error(format!("cannot reset Opus decoder: {error}")))?;
        Ok(())
    }
}

impl Iterator for OggOpusSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.buffer_position < self.buffer.len() {
                let sample = self.buffer[self.buffer_position];
                self.buffer_position += 1;
                return Some(sample);
            }
            if self.done {
                return None;
            }
            self.fill_buffer();
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.buffer.len().saturating_sub(self.buffer_position), None)
    }
}

impl Source for OggOpusSource {
    fn current_span_len(&self) -> Option<usize> {
        Some(self.buffer.len())
    }

    fn channels(&self) -> ChannelCount {
        ChannelCount::new(self.channels).expect("Opus channel count was validated")
    }

    fn sample_rate(&self) -> SampleRate {
        SampleRate::new(OPUS_SAMPLE_RATE).expect("Opus sample rate is non-zero")
    }

    fn total_duration(&self) -> Option<Duration> {
        self.playable_frames
            .map(|frames| Duration::from_secs_f64(frames as f64 / f64::from(OPUS_SAMPLE_RATE)))
    }

    fn try_seek(&mut self, position: Duration) -> Result<(), SeekError> {
        if !self.seekable {
            return Err(SeekError::NotSupported {
                underlying_source: std::any::type_name::<Self>(),
            });
        }

        let requested = duration_frames(position);
        let target = self
            .playable_frames
            .map_or(requested, |total| requested.min(total));
        let raw_target = target.saturating_add(self.pre_skip);
        let preroll = raw_target.saturating_sub(OPUS_SEEK_PREROLL_FRAMES);
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::TimeStamp {
                    ts: preroll,
                    track_id: self.track_id,
                },
            )
            .map_err(|error| seek_error(format!("Ogg seek failed: {error}")))?;
        self.reset_decoder()?;
        self.buffer.clear();
        self.buffer_position = 0;
        self.output_position_frames = target;
        self.discard_frames = raw_target.saturating_sub(seeked.actual_ts);
        self.done = false;
        self.fill_buffer();
        Ok(())
    }
}

fn duration_frames(duration: Duration) -> u64 {
    let frames = duration.as_secs_f64() * f64::from(OPUS_SAMPLE_RATE);
    frames.round().clamp(0.0, u64::MAX as f64) as u64
}

fn opus_pre_skip(extra_data: Option<&[u8]>) -> u64 {
    extra_data
        .filter(|header| header.len() >= 12)
        .map(|header| u64::from(u16::from_le_bytes([header[10], header[11]])))
        .unwrap_or(0)
}

fn opus_playable_frames(params: &CodecParameters, pre_skip: u64) -> Option<u64> {
    let encoded_frames = params.n_frames?;
    let padding = u64::from(params.padding.unwrap_or(0));

    // Symphonia normally leaves the OpusHead pre-skip in `delay`. For a very
    // short stream whose first audio page is also its last page, it instead
    // reports the page's trailing padding there. Preserve that information so
    // both one-page and ordinary Ogg/Opus streams end on the correct sample.
    let one_page_padding = params
        .delay
        .map(u64::from)
        .filter(|delay| *delay != pre_skip)
        .unwrap_or(0);

    Some(
        encoded_frames
            .saturating_sub(pre_skip)
            .saturating_sub(padding)
            .saturating_sub(one_page_padding),
    )
}

fn opus_output_gain(extra_data: Option<&[u8]>) -> f32 {
    let Some(header) = extra_data.filter(|header| header.len() >= 18) else {
        return 1.0;
    };
    let gain_q8 = i16::from_le_bytes([header[16], header[17]]);
    10.0_f32.powf(f32::from(gain_q8) / (20.0 * 256.0))
}

fn seek_error(message: String) -> SeekError {
    SeekError::Other(Arc::new(io::Error::other(message)))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use base64::Engine as _;

    use super::*;

    // 30 ms mono Ogg/Opus sine, encoded by FFmpeg/libopus. Keeping a real
    // interoperable stream here catches container and codec regressions.
    const OGG_OPUS: &str = "T2dnUwACAAAAAAAAAABkiBtVAAAAAFZb1toBE09wdXNIZWFkAQE4AYC7AAAAAABPZ2dTAAAAAAAAAAAAAGSIG1UBAAAAq+b2jQE+T3B1c1RhZ3MNAAAATGF2ZjYyLjEyLjEwMgEAAAAdAAAAZW5jb2Rlcj1MYXZjNjIuMjguMTAyIGxpYm9wdXNPZ2dTAATYBgAAAAAAAGSIG1UCAAAAEkNnrgT/NP8x+HJJRycQ5MhbeCeLhvfY79vodePsNuYR8hAn2iaItTjKzvL0UULTvai7BFd4FJ2BZdtZQn4K2vCS9rjadnLmp+u4QMCAJiL3MXbVJBVHjtDhATQq5rg5ZlscpBXWk/NqnQE/QaT/nhMi/VqnLKwRCFcpFnH/NqPx7RhjpvzicAu/blC/sRygALOf6blR6HYkOb7BLn/Vn3ijqdTVwCzwtcpWU2hbCXUOWnTEuXUOWaOmQKy8zmqYgzM7aOueo/2siHPpomLeiHPp4NH0cpD63qkSv4/XxX8HWdBtg9IvLpQ42ch0PrqI7BR4ZhvJoDmg45q/177KfOUxlfoG/8GpJWKTCpaN1u64it+1vkpE7sfaLIuSVNbz6BDOIREzM5pPkdIyBzqThkbRkGJr8KDGTyQyJfi0JXhIR5fGM2RptXd5mTRUZHmCz9dCMyKay/nzCir7oSDyEbWNlfaV7qzrxPXTNJRpJ/6GgfI3Ht2Y9jKPV8WkGzGO/Pyz+hznJTgJdGOpxRmzAWXmcwcSdI0TjPVLIs8SIBtipLhr0R+yDe2ar7ZQR3hxQzoIn5ydLO7mk8QZvr+4gAAAAAAAAAAAAAAAAAAAAAAAAE+e11+HmY5uncnz1m153WYm1urhAnMTa3qa3G/MIezSH+urKDn4eZAnEgz8PMnPgAOL22PlLS7647fYrYbJfnfYvLgwLpXSlHeh7VHf+GQd99vAzM/086lHsksCvwXkd12GFX2BTQbtW/3wPMmi4zL6VsKJUH/Q5ZQ00cvMtFDgSw5CLIg8nHONgH2/XVeZC+VwngKBeuzHHK4=";

    fn fixture() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(OGG_OPUS)
            .unwrap()
    }

    #[test]
    fn detects_and_decodes_ogg_opus() {
        let bytes = fixture();
        let len = bytes.len() as u64;
        let mut reader: TrackReader = Box::new(Cursor::new(bytes));
        assert!(is_ogg_opus(&mut reader).unwrap());

        let source = OggOpusSource::new(reader, Some(len), true).unwrap();
        assert_eq!(source.channels().get(), 1);
        assert_eq!(source.sample_rate().get(), OPUS_SAMPLE_RATE);
        let samples: Vec<_> = source.collect();
        assert_eq!(samples.len(), 1_440);
        assert!(samples.iter().any(|sample| sample.abs() > 0.001));
    }

    #[test]
    fn seeks_within_ogg_opus() {
        let bytes = fixture();
        let len = bytes.len() as u64;
        let reader: TrackReader = Box::new(Cursor::new(bytes));
        let mut source = OggOpusSource::new(reader, Some(len), true).unwrap();

        assert_eq!(source.by_ref().count(), 1_440);
        source.try_seek(Duration::from_millis(15)).unwrap();
        let samples: Vec<_> = source.collect();
        assert_eq!(samples.len(), 720);
        assert!(samples.iter().any(|sample| sample.abs() > 0.001));
    }

    #[test]
    fn ogg_vorbis_header_is_not_misclassified_as_opus() {
        let mut bytes =
            b"OggS\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x01\x1e\x01vorbis".to_vec();
        bytes.resize(SNIFF_BYTES, 0);
        let mut reader: TrackReader = Box::new(Cursor::new(bytes));
        assert!(!is_ogg_opus(&mut reader).unwrap());
    }
}
