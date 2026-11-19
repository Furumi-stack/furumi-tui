//! Preparing an episode for playback: resolve its current audio URL and
//! resume position from the store, open the HTTP stream, decode it and
//! position it, all off the UI and audio threads.
//!
//! The queue item ([`PodcastEpisodeRef`]) carries the URL that was current
//! when the episode was queued (or received); the store is asked again at
//! play time, so a feed refresh that changed the episode's audio URL is
//! picked up. When the publisher answers a stale URL with a client error,
//! the feed is refreshed once and the episode retried with the address the
//! feed now lists for the same episode id.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use furumi_library::podcasts::PodcastStore;
use music_dht::podcast::PodcastEpisodeRef;
use rodio::Source as _;

use super::blocking;
use super::refresh::{self, Services};
use super::stream::{self, StreamStatusError};
use crate::player::{DecodedSource, decode_source_with};

/// Where playback of an episode starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartAt {
    /// Ordinary Play: the saved resume position (the beginning for a new
    /// or completed episode).
    Resume,
    /// An exact position (handoff, restarting the same item).
    Position { ms: i64 },
    /// "Start over": from the beginning, recorded as an explicit restart.
    Restart,
}

/// An episode ready to hand to the audio thread.
pub struct PreparedEpisode {
    /// The episode as the store knows it now (current URL and duration), or
    /// the queued reference when the store does not know it.
    pub episode: PodcastEpisodeRef,
    pub source: DecodedSource,
    /// Position the source was moved to.
    pub start_ms: i64,
    /// Duration from the decoder, else the one the feed announced.
    pub duration_ms: Option<i64>,
}

impl std::fmt::Debug for PreparedEpisode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedEpisode")
            .field("episode", &self.episode.episode_id)
            .field("start_ms", &self.start_ms)
            .field("duration_ms", &self.duration_ms)
            .finish_non_exhaustive()
    }
}

/// The episode cannot start where it should, so it does not start at all:
/// the saved position is kept and the user is told why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CannotResume {
    pub position_ms: i64,
}

impl std::fmt::Display for CannotResume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the server does not allow seeking in this episode, so it cannot continue at {}; \
             the saved position is kept (use \"start over\" to play it from the beginning)",
            format_position(self.position_ms)
        )
    }
}

impl std::error::Error for CannotResume {}

fn format_position(ms: i64) -> String {
    let secs = ms.max(0) / 1_000;
    let (hours, minutes, seconds) = (secs / 3_600, secs / 60 % 60, secs % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// The episode reference and start position to use, from the store when it
/// knows the episode. Blocking.
pub fn resolve(
    store: Option<&PodcastStore>,
    queued: &PodcastEpisodeRef,
    start: StartAt,
) -> Result<(PodcastEpisodeRef, i64)> {
    let current = match store {
        Some(store) => store.episode_ref(&queued.episode_id)?,
        None => None,
    };
    let mut episode = current.unwrap_or_else(|| queued.clone());
    // Keep the greatest generation either side has seen (handoff).
    episode.progress_generation = episode.progress_generation.max(queued.progress_generation);
    let start_ms = match start {
        StartAt::Resume => match store {
            Some(store) => store.resume_position_ms(&queued.episode_id)?,
            None => 0,
        },
        StartAt::Position { ms } => ms.max(0),
        StartAt::Restart => 0,
    };
    Ok((episode, start_ms))
}

/// Opens, decodes and positions the audio of `episode`. Blocking (network).
pub fn open_at(episode: &PodcastEpisodeRef, start_ms: i64) -> Result<PreparedEpisode> {
    let opened = stream::open(&episode.audio_url)?;
    let seekable = opened.seekable;
    // Coarse seeking maps a time to a byte offset (one Range request)
    // instead of decoding everything before it.
    let coarse_seek = seekable && opened.byte_len.is_some();
    let mime = opened.mime_type.clone().or_else(|| episode.mime.clone());
    let mut source = decode_source_with(
        Box::new(opened.reader),
        opened.byte_len,
        mime.as_deref(),
        seekable,
        coarse_seek,
    )
    .map_err(|err| anyhow!("cannot decode the episode audio: {err}"))?;
    if start_ms > 0 {
        if !seekable {
            return Err(CannotResume {
                position_ms: start_ms,
            }
            .into());
        }
        let target = Duration::from_millis(start_ms as u64);
        if let Err(err) = source.try_seek(target) {
            tracing::warn!(%err, "seeking a podcast stream failed");
            return Err(CannotResume {
                position_ms: start_ms,
            }
            .into());
        }
    }
    let duration_ms = source
        .total_duration()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .filter(|ms| *ms > 0)
        .or(episode.duration_ms);
    Ok(PreparedEpisode {
        episode: episode.clone(),
        source,
        start_ms,
        duration_ms,
    })
}

/// [`resolve`] + [`open_at`], refreshing the feed once when the audio URL
/// answers with a client error (moved or expired file).
pub async fn prepare(
    services: Option<Services>,
    queued: PodcastEpisodeRef,
    start: StartAt,
    now_ms: i64,
) -> Result<PreparedEpisode> {
    let store = services
        .as_ref()
        .map(|services| Arc::clone(&services.store));
    let lookup = queued.clone();
    let (episode, start_ms) = blocking(move || resolve(store.as_deref(), &lookup, start)).await?;
    let attempt = episode.clone();
    let error = match blocking(move || open_at(&attempt, start_ms)).await {
        Ok(prepared) => return Ok(prepared),
        Err(error) => error,
    };
    let stale = error
        .downcast_ref::<StreamStatusError>()
        .is_some_and(|status| status.url_is_stale());
    let Some(services) = services.filter(|_| stale) else {
        return Err(error);
    };
    let store = Arc::clone(&services.store);
    let podcast_id = episode.podcast_id.clone();
    let feed_url = blocking(move || {
        Ok(store
            .podcast(&podcast_id)?
            .map(|summary| summary.meta.feed_url))
    })
    .await?;
    let Some(feed_url) = feed_url else {
        return Err(error);
    };
    tracing::info!(
        episode = %episode.episode_id,
        "episode audio unavailable; checking the feed for a new address"
    );
    if let Err(err) = refresh::refresh(&services, &episode.podcast_id, &feed_url, now_ms).await {
        tracing::warn!("refreshing the feed of an unavailable episode failed: {err:#}");
        return Err(error);
    }
    let store = Arc::clone(&services.store);
    let lookup = queued.clone();
    let (fresh, _) = blocking(move || resolve(Some(&store), &lookup, StartAt::Restart)).await?;
    if fresh.audio_url == episode.audio_url {
        return Err(error).context("the feed lists the same audio address");
    }
    blocking(move || open_at(&fresh, start_ms)).await
}

/// Message for a failed start; the URL never appears (it may carry a
/// private token).
pub fn describe_failure(err: &anyhow::Error) -> String {
    if let Some(cannot) = err.downcast_ref::<CannotResume>() {
        return cannot.to_string();
    }
    format!("{err:#}")
}
