//! Podcast RSS/Atom subscriptions.
//!
//! Furumi stores only subscription and episode *metadata* (SQLite, see
//! [`store`]); audio stays on the publisher's host and is streamed on demand
//! through [`stream`] when an episode is played. Adding a subscription never
//! downloads episodes.
//!
//! * [`feed`] parses RSS 2.0 / RSS 1.0 / Atom documents.
//! * [`fetch`] downloads feeds (conditional GET) and cover images.
//! * [`store`] persists podcasts and episodes, deduplicated by GUID.
//! * [`stream`] plays a remote episode through a bounded read-ahead buffer.
//! * [`subscribe`] / [`refresh`] orchestrate the above without blocking
//!   Tokio workers.

pub mod feed;
pub mod fetch;
pub mod store;
pub mod stream;

#[cfg(test)]
mod test_server;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};

use crate::library::models::{ArtistRef, TrackItem};

pub use fetch::{FeedFetch, Fetcher, normalize_feed_url};
pub use store::Store;
// Convenience re-exports used by the module's tests.
#[cfg(test)]
pub use feed::{ParsedEpisode, ParsedFeed, parse_feed, parse_feed_with_base};
#[cfg(test)]
pub use stream::HttpStream;

/// Track ids of podcast episodes are `-(EPISODE_TRACK_ID_BASE + episode.id)`
/// for episode ids in `1..EPISODE_TRACK_ID_BASE`, i.e. the narrow band
/// `-(2^41 - 1)..=-(2^40 + 1)`, far away from library rows (positive) and
/// other synthetic ids.
pub const EPISODE_TRACK_ID_BASE: i64 = 1 << 40;

#[derive(Debug, Clone, PartialEq)]
pub struct Podcast {
    pub id: i64,
    pub feed_url: String,
    pub title: String,
    pub author: Option<String>,
    /// Plain text (HTML stripped, entities decoded, whitespace collapsed),
    /// at most 4000 characters.
    pub description: Option<String>,
    pub link: Option<String>,
    pub image_url: Option<String>,
    /// Absolute path of the locally cached cover image, if downloaded.
    pub cover_path: Option<String>,
    pub episode_count: usize,
    /// Unix seconds, max over episodes.
    pub latest_published_at: Option<i64>,
    /// Unix seconds of the last refresh attempt.
    pub last_checked_at: Option<i64>,
    /// Error of the last refresh attempt; `None` if it succeeded.
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    /// SQLite rowid, stable across refreshes.
    pub id: i64,
    pub podcast_id: i64,
    pub guid: String,
    pub title: String,
    /// Plain text, at most 4000 characters.
    pub description: Option<String>,
    /// Unix seconds.
    pub published_at: Option<i64>,
    pub duration_seconds: Option<f64>,
    pub audio_url: String,
    pub mime_type: Option<String>,
    pub byte_len: Option<u64>,
    pub link: Option<String>,
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefreshSummary {
    pub new_episodes: usize,
    pub updated_episodes: usize,
    pub not_modified: bool,
}

/// Runs blocking store/filesystem work off the Tokio workers.
async fn blocking<T, F>(work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| anyhow!("background task failed: {err}"))?
}

/// Subscribes to a feed: normalizes the URL, rejects duplicates, fetches and
/// parses the feed (nothing is stored if it is not a feed), stores the
/// podcast with all episode metadata, then downloads the cover (best effort).
pub async fn subscribe(
    store: Arc<Store>,
    fetcher: &Fetcher,
    input_url: &str,
    now: i64,
) -> Result<i64> {
    let feed_url = normalize_feed_url(input_url)?;
    let existing = {
        let store = Arc::clone(&store);
        let feed_url = feed_url.clone();
        blocking(move || store.podcast_by_feed_url(&feed_url)).await?
    };
    if let Some(podcast) = existing {
        bail!("already subscribed to {}", podcast.title);
    }
    let (feed, etag, last_modified) = match fetcher.fetch_feed(&feed_url, None, None).await? {
        FeedFetch::Fetched {
            feed,
            etag,
            last_modified,
        } => (feed, etag, last_modified),
        FeedFetch::NotModified => {
            bail!("server answered 304 Not Modified to an unconditional request")
        }
    };
    let image_url = feed.image_url.clone();
    let podcast_id = {
        let store = Arc::clone(&store);
        blocking(move || {
            let id = store.add_podcast(&feed_url, &feed, now)?;
            store.set_http_validators(id, etag.as_deref(), last_modified.as_deref())?;
            Ok(id)
        })
        .await?
    };
    if let Some(image_url) = image_url {
        update_cover(&store, fetcher, podcast_id, &image_url).await;
    }
    Ok(podcast_id)
}

/// Refreshes one subscription with a conditional request. Failures are
/// recorded on the podcast (`last_error`) and returned.
pub async fn refresh(
    store: Arc<Store>,
    fetcher: &Fetcher,
    podcast_id: i64,
    now: i64,
) -> Result<RefreshSummary> {
    let (podcast, (etag, last_modified)) = {
        let store = Arc::clone(&store);
        blocking(move || {
            let podcast = store
                .podcast(podcast_id)?
                .ok_or_else(|| anyhow!("podcast {podcast_id} not found"))?;
            Ok((podcast, store.http_validators(podcast_id)?))
        })
        .await?
    };
    let fetched = fetcher
        .fetch_feed(&podcast.feed_url, etag.as_deref(), last_modified.as_deref())
        .await;
    let (summary, image_url) = match fetched {
        Err(err) => {
            let message = format!("{err:#}");
            let store = Arc::clone(&store);
            if let Err(record_err) =
                blocking(move || store.record_check(podcast_id, now, Some(&message))).await
            {
                tracing::warn!("recording podcast refresh error failed: {record_err:#}");
            }
            return Err(err);
        }
        Ok(FeedFetch::NotModified) => {
            let store = Arc::clone(&store);
            blocking(move || store.record_check(podcast_id, now, None)).await?;
            let summary = RefreshSummary {
                not_modified: true,
                ..RefreshSummary::default()
            };
            (summary, podcast.image_url)
        }
        Ok(FeedFetch::Fetched {
            feed,
            etag,
            last_modified,
        }) => {
            let image_url = feed.image_url.clone();
            let store = Arc::clone(&store);
            let applied = blocking(move || {
                let summary = store.apply_feed(podcast_id, &feed, now);
                match &summary {
                    Ok(_) => store.set_http_validators(
                        podcast_id,
                        etag.as_deref(),
                        last_modified.as_deref(),
                    )?,
                    Err(err) => {
                        store.record_check(podcast_id, now, Some(&format!("{err:#}")))?;
                    }
                }
                summary
            })
            .await?;
            (applied, image_url)
        }
    };
    if let Some(image_url) = image_url {
        refresh_cover(&store, fetcher, podcast_id, &image_url).await;
    }
    Ok(summary)
}

/// Re-downloads the cover when the feed image changed or the cached file is
/// gone. Best effort: failures are only logged.
async fn refresh_cover(store: &Arc<Store>, fetcher: &Fetcher, podcast_id: i64, image_url: &str) {
    let source = {
        let store = Arc::clone(store);
        blocking(move || {
            let (url, path) = store.cover_source(podcast_id)?;
            let missing = path
                .as_deref()
                .is_none_or(|path| !Path::new(path).is_file());
            Ok((url, missing))
        })
        .await
    };
    match source {
        Ok((url, missing)) if url.as_deref() == Some(image_url) && !missing => {}
        Ok(_) => update_cover(store, fetcher, podcast_id, image_url).await,
        Err(err) => tracing::warn!("reading podcast cover state failed: {err:#}"),
    }
}

/// Downloads `image_url` as the podcast cover, records it and deletes a
/// superseded cover file. Best effort.
async fn update_cover(store: &Arc<Store>, fetcher: &Fetcher, podcast_id: i64, image_url: &str) {
    let path = match fetcher.fetch_cover(podcast_id, image_url).await {
        Ok(path) => path,
        Err(err) => {
            tracing::warn!("downloading podcast cover {image_url} failed: {err:#}");
            return;
        }
    };
    let store = Arc::clone(store);
    let image_url = image_url.to_string();
    let result = blocking(move || {
        let new_path = path_string(&path);
        if store.podcast(podcast_id)?.is_none() {
            // Unsubscribed while the cover was downloading.
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        let (_, old_path) = store.cover_source(podcast_id)?;
        store.set_cover(podcast_id, Some(&image_url), Some(&new_path))?;
        if let Some(old_path) = old_path.filter(|old| *old != new_path) {
            let _ = std::fs::remove_file(old_path);
        }
        Ok(())
    })
    .await;
    if let Err(err) = result {
        tracing::warn!("recording podcast cover failed: {err:#}");
    }
}

fn path_string(path: &Path) -> String {
    let absolute: PathBuf = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    absolute.to_string_lossy().into_owned()
}

/// Whether a track path is a remote (http/https) URL rather than a file.
pub fn is_remote_url(value: &str) -> bool {
    let value = value.as_bytes();
    let has_prefix = |prefix: &[u8]| {
        value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix)
    };
    has_prefix(b"http://") || has_prefix(b"https://")
}

/// The player-facing track for an episode. `file_path` is the remote audio
/// URL; see [`is_remote_url`] and [`stream::open`].
pub fn episode_track(podcast: &Podcast, episode: &Episode) -> TrackItem {
    TrackItem {
        id: -(EPISODE_TRACK_ID_BASE + episode.id),
        title: episode.title.clone(),
        track_number: None,
        disc_number: None,
        duration_seconds: episode.duration_seconds.unwrap_or(0.0),
        artists: vec![ArtistRef {
            id: -1,
            name: podcast.title.clone(),
        }],
        featured_artists: Vec::new(),
        release_id: -1,
        release_title: podcast.title.clone(),
        release_year: episode.published_at.map(feed::year_from_unix),
        file_path: episode.audio_url.clone(),
        content_id: None,
        cover_path: podcast.cover_path.clone(),
        audio_format: episode.mime_type.as_deref().and_then(audio_format),
        audio_bitrate: None,
        audio_sample_rate: None,
        audio_bit_depth: None,
        file_size_bytes: episode
            .byte_len
            .map(|len| i64::try_from(len).unwrap_or(i64::MAX)),
        play_count: 0,
        fed: None,
    }
}

/// Inverse of the id mapping in [`episode_track`]. Only the band reserved
/// for episodes maps back, so hash-derived negative ids (for example device
/// sync placeholders spread over the whole negative range) are almost never
/// mistaken for episodes; callers should still confirm with a store lookup.
pub fn episode_id_from_track_id(track_id: i64) -> Option<i64> {
    let episode_id = track_id.checked_neg()?.checked_sub(EPISODE_TRACK_ID_BASE)?;
    (episode_id > 0 && episode_id < EPISODE_TRACK_ID_BASE).then_some(episode_id)
}

/// Display format from a MIME type: `audio/mpeg` -> `MP3`, otherwise the
/// upper-cased subtype without an `x-` prefix.
fn audio_format(mime: &str) -> Option<String> {
    let (kind, subtype) = mime.split(';').next()?.trim().split_once('/')?;
    let subtype = subtype.strip_prefix("x-").unwrap_or(subtype);
    if subtype.is_empty() {
        return None;
    }
    let audio = kind.eq_ignore_ascii_case("audio");
    Some(match subtype.to_ascii_lowercase().as_str() {
        "mpeg" | "mp3" | "mpeg3" | "mpg" if audio => "MP3".to_string(),
        "mp4" | "m4a" if audio => "M4A".to_string(),
        "vorbis" => "OGG".to_string(),
        _ => subtype.to_ascii_uppercase(),
    })
}
