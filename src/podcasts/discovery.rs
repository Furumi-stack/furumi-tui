//! Public podcast cards found by federated search: their order next to the
//! music results, whether this device is already subscribed, an episode
//! preview read from the feed itself, and subscribing from a card.
//!
//! A card is a publisher's claim. Only its origin feed URL
//! ([`PodcastSearchHit::feed_url`]) is bound to the podcast id, so it is the
//! only URL this module ever fetches or subscribes with; the claimed moved
//! URLs are shown, never used. Nothing from the card is stored: subscribing
//! reads the feed from the podcast's own host like any other subscription,
//! and the new subscription is private.

use anyhow::{Result, bail};
use furumi_library::podcasts::PodcastStore;
use music_dht::LibraryItem;
use music_dht::podcast::{FeedUrl, PodcastId};
use music_dht::podcast_dht::{
    FederatedSearchHit, PodcastDht, PodcastSearchHit, merge_search_results,
};

use super::fetch::{FeedBody, Fetcher, Validators};
use super::refresh::{self, Services, SubscribeReport};

/// Podcast hits asked from the discovery overlay per search.
pub const PODCAST_SEARCH_LIMIT: usize = 20;
/// Upper bound of one podcast search, the overlay's bootstrap included (the
/// overlay bounds only its lookups). The music lookup deadline.
pub const PODCAST_SEARCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Episodes kept in a preview (newest first).
pub const PREVIEW_EPISODES: usize = 200;

/// One public podcast in the search results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodcastHit {
    pub hit: PodcastSearchHit,
    /// The id this device gives the hit's origin URL
    /// (`PodcastStore::resolve_podcast_id`): the card's id, unless the URL
    /// is a known alias of a podcast this device already has.
    pub local_id: PodcastId,
}

impl PodcastHit {
    /// A hit not yet resolved against the local store.
    pub fn new(hit: PodcastSearchHit) -> Self {
        let local_id = hit.podcast_id.clone();
        Self { hit, local_id }
    }

    /// Ids under which this device may know the podcast.
    pub fn ids(&self) -> [&PodcastId; 2] {
        [&self.local_id, &self.hit.podcast_id]
    }

    pub fn title(&self) -> &str {
        &self.hit.card.title
    }
}

/// The podcast hits of a federated search, in the order of
/// [`merge_search_results`] (exact title matches first). The TUI keeps its
/// own sections for the music results and searches podcasts separately;
/// the order of the podcast entries does not depend on the music items
/// (the merge is a stable exact-title partition), so [`search`] passes none.
pub fn podcast_hits(
    query: &str,
    music: Vec<LibraryItem>,
    podcasts: Vec<PodcastSearchHit>,
) -> Vec<PodcastHit> {
    merge_search_results(query, music, podcasts)
        .into_iter()
        .filter_map(|hit| match hit {
            FederatedSearchHit::Podcast(hit) => Some(PodcastHit::new(hit)),
            FederatedSearchHit::Music(_) => None,
        })
        .collect()
}

/// Searches the discovery overlay for public podcast cards, independently
/// of the music search. Bounded by `timeout` (the overlay bootstraps first
/// when it knows no peers, which its own lookup deadline does not cover).
/// No overlay, an error or a timeout give no hits; the warning names
/// neither the query nor any URL.
pub async fn search(
    dht: Option<&PodcastDht>,
    query: &str,
    timeout: std::time::Duration,
) -> Vec<PodcastHit> {
    let Some(dht) = dht else {
        return Vec::new();
    };
    match tokio::time::timeout(timeout, dht.search(query, PODCAST_SEARCH_LIMIT)).await {
        Ok(Ok(hits)) => podcast_hits(query, Vec::new(), hits),
        Ok(Err(err)) => {
            tracing::warn!("podcast discovery search failed: {err}");
            Vec::new()
        }
        Err(_) => {
            tracing::warn!(
                timeout_s = timeout.as_secs(),
                "podcast discovery search timed out"
            );
            Vec::new()
        }
    }
}

/// Resolves each hit's origin URL against the local store (feed aliases
/// included). Blocking.
pub fn resolve_local_ids(store: &PodcastStore, hits: &mut [PodcastHit]) -> Result<()> {
    for hit in hits {
        hit.local_id = store.resolve_podcast_id(&hit.hit.feed_url)?;
    }
    Ok(())
}

/// Subscribes to the podcast of a public card with its origin URL, never a
/// claimed one. The feed is read from its own host (following its own
/// redirects and `itunes:new-feed-url`), the subscription is private and
/// none of the card's metadata is stored.
pub async fn subscribe_from_hit(
    services: &Services,
    hit: &PodcastSearchHit,
    now_ms: i64,
) -> Result<SubscribeReport> {
    if PodcastId::from_feed_url(&hit.feed_url) != hit.podcast_id {
        bail!("the card's feed URL does not match its podcast id");
    }
    refresh::subscribe_url(services, hit.feed_url.clone(), now_ms).await
}

/// One episode of a preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewEpisode {
    pub title: String,
    pub season: Option<u32>,
    pub episode_number: Option<u32>,
    pub published_at_ms: Option<i64>,
    pub duration_ms: Option<i64>,
}

/// What a podcast's own feed says, read without subscribing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedPreview {
    /// Channel title in the feed (may differ from the card's claim).
    pub title: String,
    pub episode_count: usize,
    /// Season number (`None`: episodes without one) and episode count,
    /// seasons in ascending order, unnumbered last.
    pub seasons: Vec<(Option<u32>, usize)>,
    /// Newest first, at most [`PREVIEW_EPISODES`].
    pub episodes: Vec<PreviewEpisode>,
    /// The feed answered from, or announced, another address.
    pub moved_to: Option<FeedUrl>,
    /// This device is subscribed to the podcast under `moved_to` (it
    /// followed the feed's newer address, the card names the older one):
    /// the card shows «Уже подписан». Set by [`preview_for`].
    pub subscribed_as: Option<PodcastId>,
}

/// Reads the feed at `url` (a card's origin URL) for a preview. Nothing is
/// stored and no fetch state is recorded.
pub async fn preview(fetcher: &Fetcher, url: &FeedUrl) -> Result<FeedPreview> {
    let response = fetcher.fetch_feed(url, Validators::default()).await?;
    let FeedBody::Modified { body, .. } = response.body else {
        bail!("the server answered 304 Not Modified to an unconditional request");
    };
    let feed = refresh::parse(body, &response.final_url).await?;
    let announced = feed
        .new_feed_url
        .as_deref()
        .and_then(|url| FeedUrl::parse(url).ok());
    let moved_to = response
        .moved_permanently_to
        .or(announced)
        .filter(|moved| moved != url);

    let mut seasons: std::collections::BTreeMap<(bool, u32), usize> = Default::default();
    for episode in &feed.episodes {
        let key = match episode.season {
            Some(season) => (false, season),
            None => (true, 0),
        };
        *seasons.entry(key).or_default() += 1;
    }
    let mut episodes: Vec<PreviewEpisode> = feed
        .episodes
        .iter()
        .map(|episode| PreviewEpisode {
            title: episode.title.clone(),
            season: episode.season,
            episode_number: episode.episode_number,
            published_at_ms: episode.published_at_ms,
            duration_ms: episode.duration_ms,
        })
        .collect();
    episodes.sort_by_key(|episode| std::cmp::Reverse(episode.published_at_ms));
    episodes.truncate(PREVIEW_EPISODES);
    Ok(FeedPreview {
        title: feed.title.clone(),
        episode_count: feed.episodes.len(),
        seasons: seasons
            .into_iter()
            .map(|((unnumbered, season), count)| ((!unnumbered).then_some(season), count))
            .collect(),
        episodes,
        moved_to,
        subscribed_as: None,
    })
}

/// [`preview`], plus whether this device already follows the podcast under
/// the newer address the feed moved to (`subscribed_as`). The store is only
/// read.
pub async fn preview_for(services: &Services, url: &FeedUrl) -> Result<FeedPreview> {
    let mut preview = preview(&services.fetcher, url).await?;
    if let Some(moved) = preview.moved_to.clone() {
        let store = std::sync::Arc::clone(&services.store);
        preview.subscribed_as =
            tokio::task::spawn_blocking(move || refresh::subscribed_podcast(&store, &moved))
                .await??;
    }
    Ok(preview)
}

/// A search hit for `feed_url` as one publisher would announce it (tests of
/// the search UI and of subscribing from a card).
#[cfg(test)]
pub(crate) fn test_hit(feed_url: &str, title: &str) -> PodcastSearchHit {
    let feed_url = FeedUrl::parse(feed_url).unwrap();
    let podcast_id = PodcastId::from_feed_url(&feed_url);
    let publisher = music_dht::SecretKey::from_bytes(&[7; 32]).public();
    PodcastSearchHit {
        podcast_id: podcast_id.clone(),
        feed_url: feed_url.clone(),
        card: music_dht::podcast_dht::PodcastCard {
            version: 1,
            network_id: music_dht::NetworkId::from_name("furumi-tui-discovery-test"),
            publisher,
            podcast_id,
            feed_url,
            id_feed_url: None,
            title: title.to_string(),
            author: Some("Ferris Crab".to_string()),
            description: Some("Weekly news about Rust.".to_string()),
            image_url: None,
            link: None,
            language: Some("en".to_string()),
            categories: vec!["Technology".to_string()],
            episode_count: Some(12),
            latest_episode_at_ms: Some(1_718_445_600_000),
            revision: 1,
            issued_at_ms: 1,
            expires_at_ms: 2,
        },
        publishers: vec![publisher],
        claimed_feed_urls: Vec::new(),
    }
}
