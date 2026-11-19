//! Subscribing and refreshing feeds.
//!
//! Every function records its HTTP outcome in the store's local fetch state
//! (validators, last error, consecutive failures, next check) so the
//! scheduler never needs its own bookkeeping, and an error never clears the
//! stored catalog.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use furumi_library::podcasts::{FeedRefresh, PodcastStore};
use music_dht::podcast::feed::{ParsedFeed, parse_feed_with_base};
use music_dht::podcast::{FeedUrl, PodcastId};

use super::fetch::{FeedBody, Fetcher, HttpStatusError, Validators};
use super::{OpLog, blocking};

/// A successfully checked feed is checked again after this long.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// First retry after a failed check; doubles per consecutive failure.
pub const FAILURE_BACKOFF_BASE: Duration = Duration::from_secs(5 * 60);
/// Longest wait between checks of a failing feed (also caps `Retry-After`).
pub const FAILURE_BACKOFF_MAX: Duration = Duration::from_secs(24 * 60 * 60);

/// Store, HTTP client and sync log shared by podcast background tasks.
#[derive(Clone)]
pub struct Services {
    pub store: Arc<PodcastStore>,
    pub fetcher: Arc<Fetcher>,
    pub log: Arc<dyn OpLog>,
    /// Device-local privacy confirmations for sharing (never synced).
    pub confirmations: Arc<super::publication::ShareConfirmations>,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

/// Result of [`subscribe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscribeReport {
    pub podcast_id: PodcastId,
    pub title: String,
    /// The podcast was already subscribed; nothing was fetched or written.
    pub already_subscribed: bool,
    /// Episodes the first refresh added to the catalog.
    pub new_episodes: usize,
}

/// Result of [`refresh`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshReport {
    /// The server answered 304 Not Modified.
    pub not_modified: bool,
    pub new_episodes: usize,
    pub updated_episodes: usize,
    /// Feed items without identity or with invalid metadata.
    pub skipped_items: usize,
    /// New fetch URL after a permanent redirect or `itunes:new-feed-url`.
    pub moved_to: Option<FeedUrl>,
}

/// When to check a feed again after a success.
pub fn next_check_after_success(now_ms: i64) -> i64 {
    now_ms.saturating_add(duration_ms(REFRESH_INTERVAL))
}

/// When to check a feed again after its `failures`-th consecutive failure:
/// exponential back-off, at least the server's `Retry-After`, at most
/// [`FAILURE_BACKOFF_MAX`].
pub fn next_check_after_failure(now_ms: i64, failures: u32, retry_after: Option<Duration>) -> i64 {
    let exponent = failures.saturating_sub(1).min(16);
    let backoff = FAILURE_BACKOFF_BASE
        .saturating_mul(1 << exponent)
        .max(retry_after.unwrap_or_default())
        .min(FAILURE_BACKOFF_MAX);
    now_ms.saturating_add(duration_ms(backoff))
}

fn duration_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// Subscribes to the feed the user typed (`FeedUrl::from_user_input`).
///
/// Nothing is stored unless the URL serves a parsable feed. A URL that
/// resolves (directly or through a known alias or redirect) to a podcast
/// this device already knows reuses its id, so resubscribing continues the
/// saved progress and history. The subscription is private.
pub async fn subscribe(services: &Services, input: &str, now_ms: i64) -> Result<SubscribeReport> {
    let url = FeedUrl::from_user_input(input).map_err(|err| anyhow!("{err}"))?;
    subscribe_url(services, url, now_ms).await
}

/// Subscribes to `url` exactly as given (see [`subscribe`]).
///
/// Subscribing from a public card passes the card's origin URL here
/// (`podcasts::discovery::subscribe_from_hit`): the id then matches the
/// card's `podcast_id` unless this device already knows the URL as an alias
/// of another podcast, and everything stored comes from the feed itself.
pub async fn subscribe_url(
    services: &Services,
    url: FeedUrl,
    now_ms: i64,
) -> Result<SubscribeReport> {
    if let Some(report) = already_subscribed(services, &url).await? {
        return Ok(report);
    }

    let response = services
        .fetcher
        .fetch_feed(&url, Validators::default())
        .await?;
    let FeedBody::Modified {
        body,
        etag,
        last_modified,
    } = response.body
    else {
        bail!("the server answered 304 Not Modified to an unconditional request");
    };
    let final_url = response.final_url;
    let feed = parse(body, &final_url).await?;
    // The feed's own address: where its permanent redirects lead.
    let moved_permanently = response.moved_permanently_to.is_some();
    let landed = response.moved_permanently_to.unwrap_or(final_url);
    // An older address of a feed this device follows under the newer one it
    // announces (`itunes:new-feed-url`, no redirect): the same podcast, not
    // a second subscription. Nothing is written.
    if let Some(announced) = feed
        .new_feed_url
        .as_deref()
        .and_then(|announced| FeedUrl::parse(announced).ok())
        .filter(|announced| *announced != url && *announced != landed)
        && let Some(report) = already_subscribed(services, &announced).await?
    {
        return Ok(report);
    }

    let store = Arc::clone(&services.store);
    let log = Arc::clone(&services.log);
    let image_url = feed.image_url.clone();
    let report = blocking(move || {
        // Identity comes from the URL of the first subscription. Reuse a
        // podcast this device already knows under the typed or the final
        // URL; otherwise the typed URL defines the id and a permanent
        // redirect becomes an alias below.
        let identity_url = if knows(&store, &url)? || !knows(&store, &landed)? {
            url.clone()
        } else {
            landed.clone()
        };
        let meta = feed.to_podcast_meta(identity_url);
        let mut subscribed = None;
        log.record(&mut |stamps| {
            let outcome = store.subscribe(stamps, &meta)?;
            let ops = outcome.ops.clone();
            subscribed = Some(outcome);
            Ok(ops)
        })?;
        let outcome = subscribed.ok_or_else(|| anyhow!("subscription was not recorded"))?;
        let podcast_id = outcome.podcast_id;
        let mut refreshed = None;
        log.record(&mut |stamps| {
            let outcome = store.apply_feed(
                stamps,
                &FeedRefresh {
                    podcast_id: &podcast_id,
                    feed: &feed,
                    fetched_from: &landed,
                    moved_permanently,
                },
            )?;
            let ops = outcome.ops.clone();
            refreshed = Some(outcome);
            Ok(ops)
        })?;
        let refreshed = refreshed.ok_or_else(|| anyhow!("feed refresh was not recorded"))?;
        store.record_fetch_success(
            &podcast_id,
            now_ms,
            etag.as_deref(),
            last_modified.as_deref(),
            Some(next_check_after_success(now_ms)),
        )?;
        Ok(SubscribeReport {
            podcast_id,
            title: meta.title,
            already_subscribed: outcome.already_subscribed,
            new_episodes: refreshed.new_episodes,
        })
    })
    .await?;
    if let Some(image_url) = image_url {
        update_cover(services, &report.podcast_id, &image_url).await;
    }
    Ok(report)
}

/// The subscription `url` belongs to on this device (directly or as a
/// known feed alias), as an "already subscribed" report.
async fn already_subscribed(services: &Services, url: &FeedUrl) -> Result<Option<SubscribeReport>> {
    let store = Arc::clone(&services.store);
    let lookup = url.clone();
    blocking(move || {
        let Some(podcast_id) = subscribed_podcast(&store, &lookup)? else {
            return Ok(None);
        };
        let title = store
            .podcast(&podcast_id)?
            .map(|summary| summary.meta.title);
        Ok(Some(SubscribeReport {
            podcast_id,
            title: title.unwrap_or_default(),
            already_subscribed: true,
            new_episodes: 0,
        }))
    })
    .await
}

/// The podcast `url` resolves to on this device (feed aliases included),
/// when it is subscribed. Blocking.
pub(crate) fn subscribed_podcast(store: &PodcastStore, url: &FeedUrl) -> Result<Option<PodcastId>> {
    let id = store.resolve_podcast_id(url)?;
    let subscribed = store
        .subscription(&id)?
        .is_some_and(|record| record.subscribed);
    Ok(subscribed.then_some(id))
}

fn knows(store: &PodcastStore, url: &FeedUrl) -> Result<bool> {
    let id = store.resolve_podcast_id(url)?;
    Ok(store.podcast(&id)?.is_some() || store.subscription(&id)?.is_some())
}

/// Checks one feed (conditional GET from `url`, the store's fetch URL) and
/// ingests a changed document. Failures are recorded in the fetch state
/// with back-off and returned; the stored catalog is kept.
pub async fn refresh(
    services: &Services,
    podcast_id: &PodcastId,
    url: &FeedUrl,
    now_ms: i64,
) -> Result<RefreshReport> {
    let store = Arc::clone(&services.store);
    let id = podcast_id.clone();
    let fetch_state = blocking(move || store.fetch_state(&id)).await?;
    let result = fetch_and_apply(services, podcast_id, url, &fetch_state, now_ms).await;
    if let Err(err) = &result {
        let message = format!("{err:#}");
        let retry_after = err
            .downcast_ref::<HttpStatusError>()
            .and_then(|status| status.retry_after);
        let next = next_check_after_failure(
            now_ms,
            fetch_state.consecutive_failures.saturating_add(1),
            retry_after,
        );
        let store = Arc::clone(&services.store);
        let id = podcast_id.clone();
        if let Err(record_err) = blocking(move || {
            store
                .record_fetch_failure(&id, now_ms, &message, Some(next))
                .map(|_| ())
        })
        .await
        {
            tracing::warn!("recording a podcast refresh error failed: {record_err:#}");
        }
    }
    result
}

async fn fetch_and_apply(
    services: &Services,
    podcast_id: &PodcastId,
    url: &FeedUrl,
    fetch_state: &furumi_library::podcasts::FetchState,
    now_ms: i64,
) -> Result<RefreshReport> {
    let response = services
        .fetcher
        .fetch_feed(
            url,
            Validators {
                etag: fetch_state.etag.as_deref(),
                last_modified: fetch_state.last_modified.as_deref(),
            },
        )
        .await?;
    tracing::debug!(
        podcast = %podcast_id,
        redirects = response.redirects,
        moved_permanently = response.moved_permanently_to.is_some(),
        not_modified = matches!(response.body, FeedBody::NotModified),
        "podcast feed checked"
    );
    let next = Some(next_check_after_success(now_ms));
    let (body, etag, last_modified) = match response.body {
        FeedBody::NotModified => {
            let store = Arc::clone(&services.store);
            let id = podcast_id.clone();
            blocking(move || store.record_fetch_not_modified(&id, now_ms, next)).await?;
            return Ok(RefreshReport {
                not_modified: true,
                ..RefreshReport::default()
            });
        }
        FeedBody::Modified {
            body,
            etag,
            last_modified,
        } => (body, etag, last_modified),
    };
    let final_url = response.final_url;
    let feed = parse(body, &final_url).await?;
    let moved_permanently = response.moved_permanently_to.is_some();
    let landed = response.moved_permanently_to.unwrap_or(final_url);
    let image_url = feed.image_url.clone();
    let store = Arc::clone(&services.store);
    let log = Arc::clone(&services.log);
    let id = podcast_id.clone();
    let previous_url = url.clone();
    let report = blocking(move || {
        let mut refreshed = None;
        log.record(&mut |stamps| {
            let outcome = store.apply_feed(
                stamps,
                // `fetched_from` is the address the permanent redirects led
                // to; the document itself may come from a temporary hop
                // behind it (parsed against that URL above).
                &FeedRefresh {
                    podcast_id: &id,
                    feed: &feed,
                    fetched_from: &landed,
                    moved_permanently,
                },
            )?;
            let ops = outcome.ops.clone();
            refreshed = Some(outcome);
            Ok(ops)
        })?;
        let outcome = refreshed.ok_or_else(|| anyhow!("feed refresh was not recorded"))?;
        store.record_fetch_success(&id, now_ms, etag.as_deref(), last_modified.as_deref(), next)?;
        Ok(RefreshReport {
            not_modified: false,
            new_episodes: outcome.new_episodes,
            updated_episodes: outcome.updated_episodes,
            skipped_items: outcome.skipped_items,
            moved_to: outcome
                .feed_url
                .filter(|feed_url| *feed_url != previous_url),
        })
    })
    .await?;
    if let Some(image_url) = image_url {
        update_cover(services, podcast_id, &image_url).await;
    }
    Ok(report)
}

/// Parses a feed document on the blocking pool; relative URLs resolve
/// against where the document was served from.
pub(crate) async fn parse(body: Vec<u8>, base: &FeedUrl) -> Result<ParsedFeed> {
    let base = base.as_str().to_string();
    blocking(move || {
        parse_feed_with_base(&body, Some(&base)).map_err(|err| anyhow!("unusable feed: {err}"))
    })
    .await
}

/// Downloads the cover when the feed image changed or the cached file is
/// gone, records it and deletes the replaced file. Best effort: failures are
/// only logged, covers are a replaceable cache.
async fn update_cover(services: &Services, podcast_id: &PodcastId, image_url: &str) {
    let store = Arc::clone(&services.store);
    let id = podcast_id.clone();
    let current = blocking(move || {
        let state = store.fetch_state(&id)?;
        Ok((state.cover_url, state.cover_path))
    })
    .await;
    let (old_url, old_path) = match current {
        Ok(current) => current,
        Err(err) => {
            tracing::warn!("reading podcast cover state failed: {err:#}");
            return;
        }
    };
    let cached = old_path
        .as_deref()
        .is_some_and(|path| Path::new(path).is_file());
    if old_url.as_deref() == Some(image_url) && cached {
        return;
    }
    let path = match services.fetcher.fetch_cover(podcast_id, image_url).await {
        Ok(path) => path,
        Err(err) => {
            tracing::debug!(podcast = %podcast_id, "podcast cover download failed: {err:#}");
            return;
        }
    };
    let store = Arc::clone(&services.store);
    let id = podcast_id.clone();
    let image_url = image_url.to_string();
    let result = blocking(move || {
        let new_path = std::path::absolute(&path)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        store
            .set_cover_cache(&id, Some(&image_url), Some(&new_path))
            .context("recording the cover")?;
        if let Some(old_path) = old_path.filter(|old| *old != new_path) {
            let _ = std::fs::remove_file(old_path);
        }
        Ok(())
    })
    .await;
    if let Err(err) = result {
        tracing::warn!("recording a podcast cover failed: {err:#}");
    }
}
