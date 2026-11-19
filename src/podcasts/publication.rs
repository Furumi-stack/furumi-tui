//! Public podcast discovery: publishing the podcasts the user chose to share.
//!
//! The share decision itself is synced personal state
//! (`PodcastStore::set_shared`, operation `podcast_share_set`); this module
//! turns the merged registers into this device's own publications in the
//! FRID overlay `music_dht::podcast_dht`:
//!
//! * [`share_states`] builds one `PodcastShareState` per subscription, the
//!   integration pattern FRID documents for SQLite clients: the merged
//!   subscription and share registers, a card whose `feed_url` is the URL
//!   this device fetches (catalog) and whose `id_feed_url` is the origin
//!   URL, and the local privacy confirmation. A card is only built for a
//!   podcast that is effectively shared; private subscriptions contribute
//!   their registers only, and `PodcastDht::sync_shares` never publishes
//!   them.
//! * [`reconcile`] hands those states to `PodcastDht::sync_shares`, which is
//!   declarative and idempotent: it publishes what is shared and tombstones
//!   every own publication that is not (share turned off or unsubscribed on
//!   any trusted device). A stale device cannot republish, because its
//!   registers lose to the newer share decision once they converge.
//! * [`spawn`] runs the reconciler off the UI thread: after local share and
//!   unsubscribe actions, after received podcast operations, after feed
//!   refreshes (card metadata), when the overlay (re)starts, and
//!   periodically.
//! * [`ShareConfirmations`] remembers, on this device only, that the user
//!   confirmed publishing feed URLs that look private
//!   (`PodcastCardSpec::privacy_warnings`). FRID leaves this confirmation to
//!   the client and does not sync it: every device asks its own user before
//!   it publishes such a URL under its own identity. A confirmation is bound
//!   to the exact URLs shown, so a feed that moves to another URL asks again.
//!
//! Logging stays free of private data: only counts are logged, never feed
//! URLs, titles or ids of podcasts that are not shared.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use furumi_library::podcasts::{PodcastListFilter, PodcastStore, PodcastSummary};
use music_dht::podcast::{FeedUrl, PodcastId, SubscriptionRecord, effective_share};
use music_dht::podcast_dht::{
    FeedUrlPrivacyWarning, PodcastCardSpec, PodcastDht, PodcastShareState,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, watch};

/// Local file with the privacy confirmations, next to `podcasts.sqlite3`.
pub const CONFIRMATIONS_FILE: &str = "podcast-share-confirmations.json";
/// Format version of [`CONFIRMATIONS_FILE`].
const CONFIRMATIONS_VERSION: u32 = 1;
/// Periodic reconciliation (on top of the triggers). Cards are re-signed by
/// the overlay's own maintenance every 10 minutes; this pass catches share
/// decisions that changed while the overlay was busy or unreachable.
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Triggers within this window are handled by one pass (a "check all"
/// refresh finishes many feeds in a row).
pub const RECONCILE_DEBOUNCE: Duration = Duration::from_secs(1);

/// The feed URLs a privacy confirmation covers: exactly the URLs of the card
/// the user was shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmedUrls {
    pub feed_url: FeedUrl,
    pub id_feed_url: Option<FeedUrl>,
}

impl ConfirmedUrls {
    pub fn of(card: &PodcastCardSpec) -> Self {
        Self {
            feed_url: card.feed_url.clone(),
            id_feed_url: card.id_feed_url.clone(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConfirmationsFile {
    version: u32,
    confirmed: BTreeMap<PodcastId, ConfirmedUrls>,
}

/// Device-local record of "publish this feed URL although it looks
/// private" (`PodcastShareState::privacy_confirmed`). Not synced.
#[derive(Debug)]
pub struct ShareConfirmations {
    /// `None`: in memory only (tests, or the podcast store is unavailable).
    path: Option<PathBuf>,
    confirmed: Mutex<BTreeMap<PodcastId, ConfirmedUrls>>,
}

impl ShareConfirmations {
    /// Loads the confirmations at `path`. A missing file is empty; an
    /// unreadable or unknown file is treated as empty too (the user is asked
    /// again, which is the safe direction) and replaced on the next change.
    pub fn open(path: PathBuf) -> Self {
        let confirmed = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<ConfirmationsFile>(&bytes) {
                Ok(file) if file.version == CONFIRMATIONS_VERSION => file.confirmed,
                Ok(file) => {
                    tracing::warn!(
                        version = file.version,
                        "podcast share confirmations have an unknown format; asking again"
                    );
                    BTreeMap::new()
                }
                Err(err) => {
                    tracing::warn!(
                        "podcast share confirmations are unreadable; asking again: {err}"
                    );
                    BTreeMap::new()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(err) => {
                tracing::warn!("cannot read the podcast share confirmations; asking again: {err}");
                BTreeMap::new()
            }
        };
        Self {
            path: Some(path),
            confirmed: Mutex::new(confirmed),
        }
    }

    pub fn in_memory() -> Self {
        Self {
            path: None,
            confirmed: Mutex::new(BTreeMap::new()),
        }
    }

    /// Whether the user confirmed publishing exactly these feed URLs.
    pub fn is_confirmed(&self, card: &PodcastCardSpec) -> bool {
        self.lock()
            .get(&card.podcast_id)
            .is_some_and(|urls| *urls == ConfirmedUrls::of(card))
    }

    /// Records the confirmation of `urls` (replacing an older one).
    /// Blocking (writes the file).
    pub fn confirm(&self, podcast_id: &PodcastId, urls: ConfirmedUrls) -> Result<()> {
        let mut confirmed = self.lock();
        if confirmed.get(podcast_id) == Some(&urls) {
            return Ok(());
        }
        let mut next = confirmed.clone();
        next.insert(podcast_id.clone(), urls);
        self.save(&next)?;
        *confirmed = next;
        Ok(())
    }

    /// Drops the confirmation of a podcast (sharing turned off or
    /// unsubscribed here): sharing it again asks again. Blocking.
    pub fn forget(&self, podcast_id: &PodcastId) -> Result<()> {
        let mut confirmed = self.lock();
        if !confirmed.contains_key(podcast_id) {
            return Ok(());
        }
        let mut next = confirmed.clone();
        next.remove(podcast_id);
        self.save(&next)?;
        *confirmed = next;
        Ok(())
    }

    fn save(&self, confirmed: &BTreeMap<PodcastId, ConfirmedUrls>) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let file = ConfirmationsFile {
            version: CONFIRMATIONS_VERSION,
            confirmed: confirmed.clone(),
        };
        write_private(path, &serde_json::to_vec_pretty(&file)?)
            .with_context(|| format!("saving {}", path.display()))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<PodcastId, ConfirmedUrls>> {
        self.confirmed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Atomically replaces `path`; the file holds feed URLs, so it is readable
/// by the user only (like the podcast database).
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
    std::io::Write::write_all(&mut temp, bytes)?;
    temp.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    temp.persist(path)?;
    Ok(())
}

/// The confirmations file of the podcast store at `db_path`.
pub fn confirmations_path(db_path: &Path) -> PathBuf {
    db_path.with_file_name(CONFIRMATIONS_FILE)
}

// ---------------------------------------------------------------------------
// Cards and share states

/// The card this device publishes for a subscription: channel metadata and
/// counters from the local catalog (refreshed from the feed), `feed_url` the
/// URL this device fetches, `id_feed_url` the origin URL the podcast id was
/// derived from. Bounded to the card limits.
fn catalog_card(
    summary: &PodcastSummary,
    subscription: &SubscriptionRecord,
) -> Result<PodcastCardSpec> {
    // Finds the origin URL in the register's alias set (FRID keeps it).
    let derived = PodcastCardSpec::from_subscription(subscription)?;
    let origin = derived
        .id_feed_url
        .clone()
        .unwrap_or_else(|| derived.feed_url.clone());
    let meta = &summary.meta;
    let card = PodcastCardSpec {
        podcast_id: subscription.podcast_id.clone(),
        feed_url: meta.feed_url.clone(),
        id_feed_url: Some(origin),
        title: meta.title.clone(),
        author: meta.author.clone(),
        description: meta.description.clone(),
        image_url: meta.image_url.clone(),
        link: meta.link.clone(),
        language: None,
        categories: Vec::new(),
        episode_count: Some(u32::try_from(summary.episode_count).unwrap_or(u32::MAX)),
        latest_episode_at_ms: summary.latest_published_at_ms,
    }
    .bounded();
    card.validate()?;
    Ok(card)
}

/// The card [`share_states`] would publish for `podcast_id` (whether or not
/// it is shared yet), for the share confirmation. `None` when there is no
/// subscription.
pub fn share_card(store: &PodcastStore, podcast_id: &PodcastId) -> Result<Option<PodcastCardSpec>> {
    let (Some(summary), Some(subscription)) =
        (store.podcast(podcast_id)?, store.subscription(podcast_id)?)
    else {
        return Ok(None);
    };
    catalog_card(&summary, &subscription).map(Some)
}

/// What enabling "share" would publish and whether it needs the user's
/// confirmation first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareCheck {
    pub urls: ConfirmedUrls,
    /// `PodcastCardSpec::privacy_warnings` of the card.
    pub warnings: Vec<FeedUrlPrivacyWarning>,
    /// The user already confirmed exactly these URLs on this device.
    pub confirmed: bool,
}

impl ShareCheck {
    /// Sharing must be confirmed in a dialog first.
    pub fn needs_confirmation(&self) -> bool {
        !self.warnings.is_empty() && !self.confirmed
    }
}

/// Builds the [`ShareCheck`] of a subscribed podcast. Blocking.
pub fn check_share(
    store: &PodcastStore,
    confirmations: &ShareConfirmations,
    podcast_id: &PodcastId,
) -> Result<ShareCheck> {
    let card = share_card(store, podcast_id)?
        .with_context(|| "the podcast is not subscribed".to_string())?;
    Ok(ShareCheck {
        urls: ConfirmedUrls::of(&card),
        warnings: card.privacy_warnings(),
        confirmed: confirmations.is_confirmed(&card),
    })
}

/// One `PodcastShareState` per podcast with a subscription register, the
/// input of `PodcastDht::sync_shares`. Blocking.
///
/// Only podcasts for which `effective_share` holds get a card; for every
/// other podcast `card` is `None` and `privacy_confirmed` false, so no
/// private metadata is prepared for publication at all.
pub fn share_states(
    store: &PodcastStore,
    confirmations: &ShareConfirmations,
) -> Result<Vec<PodcastShareState>> {
    let mut states = Vec::new();
    for summary in store.list_podcasts(PodcastListFilter::All)? {
        let podcast_id = &summary.podcast_id;
        let Some(subscription) = store.subscription(podcast_id)? else {
            continue;
        };
        let share = store.share(podcast_id)?;
        let shared = effective_share(Some(&subscription), share.as_ref());
        // A card that cannot be built from the catalog falls back to the
        // one `sync_shares` derives from the register (and fails there,
        // reported, if that is impossible too).
        let card = if shared {
            catalog_card(&summary, &subscription)
                .inspect_err(|err| tracing::debug!("podcast card from the catalog: {err:#}"))
                .ok()
        } else {
            None
        };
        let privacy_confirmed = card
            .as_ref()
            .is_some_and(|card| confirmations.is_confirmed(card));
        states.push(PodcastShareState {
            subscription,
            share,
            card,
            privacy_confirmed,
        });
    }
    Ok(states)
}

// ---------------------------------------------------------------------------
// Reconciliation

/// Public discovery as this device runs it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Discovery {
    /// No reconciliation finished yet.
    #[default]
    Starting,
    /// Nothing is published from this device; the reason.
    Off(String),
    /// The overlay runs; `peers` overlay contacts are known.
    Running { peers: usize },
}

/// Publication of one shared podcast from this device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Publication {
    /// This device's card is published (signed, stored by the replicas it
    /// reached and republished every 10 minutes).
    Published,
    /// Shared, but not yet published here (for example the overlay has not
    /// accepted it yet).
    Pending,
    /// Shared, but held back on this device until its user confirms the
    /// feed URLs that look private.
    HeldBack(Vec<FeedUrlPrivacyWarning>),
    /// The card could not be built, signed or published.
    Failed(String),
}

/// Result of one reconciliation, for the Podcasts tab.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublicationReport {
    pub discovery: Discovery,
    /// Shared podcasts only.
    pub podcasts: BTreeMap<PodcastId, Publication>,
    /// The pass as a whole failed (store or overlay error).
    pub error: Option<String>,
}

/// Reconciles this device's publications with the share registers of
/// `store` once. Without a store (it could not be opened) every own
/// publication is revoked: this device can no longer tell what the user
/// still shares, and revoking is the privacy-safe direction. The same holds
/// when the registers cannot be read.
pub async fn reconcile(
    dht: &PodcastDht,
    store: Option<Arc<PodcastStore>>,
    confirmations: Arc<ShareConfirmations>,
) -> PublicationReport {
    let mut report = PublicationReport {
        discovery: Discovery::Running {
            peers: dht.known_peers(),
        },
        ..PublicationReport::default()
    };
    // Why every own card is revoked, when the registers are unknown.
    let mut revoke_all: Option<String> = None;
    let states = match store {
        Some(store) => {
            match tokio::task::spawn_blocking(move || share_states(&store, &confirmations)).await {
                Ok(Ok(states)) => states,
                Ok(Err(err)) => {
                    revoke_all = Some(format!("cannot read share decisions ({err:#})"));
                    Vec::new()
                }
                Err(err) => {
                    revoke_all = Some(format!("cannot read share decisions ({err})"));
                    Vec::new()
                }
            }
        }
        None => {
            revoke_all = Some("the podcast database is unavailable".into());
            Vec::new()
        }
    };
    let shared: Vec<PodcastId> = states
        .iter()
        .filter(|state| state.is_shared())
        .map(|state| state.subscription.podcast_id.clone())
        .collect();
    let stats = match dht.sync_shares(states).await {
        Ok(stats) => stats,
        Err(err) => {
            tracing::warn!("podcast publication failed: {err}");
            report.error = Some(format!("publication failed: {err}"));
            for podcast_id in shared {
                report
                    .podcasts
                    .insert(podcast_id, Publication::Failed(err.to_string()));
            }
            return report;
        }
    };
    if stats.published + stats.unpublished + stats.failed.len() > 0 {
        tracing::info!(
            published = stats.published,
            unchanged = stats.unchanged,
            unpublished = stats.unpublished,
            held_back = stats.held_back.len(),
            failed = stats.failed.len(),
            remote_nodes = stats.push.remote_nodes,
            "podcast publications reconciled"
        );
    }
    for failure in &stats.failed {
        // Failures may concern revoked (now private) podcasts: no ids.
        tracing::warn!(error = %failure.error, "podcast publication step failed");
    }
    if let Some(reason) = revoke_all {
        // Worth telling the user only when something was published.
        if stats.unpublished > 0 || !stats.failed.is_empty() {
            tracing::warn!(
                unpublished = stats.unpublished,
                "podcast publication: {reason}; own public cards revoked"
            );
            report.error = Some(format!("{reason}; own public cards revoked"));
        } else {
            tracing::debug!("podcast publication: {reason}; nothing is published");
        }
    }
    let active: BTreeSet<PodcastId> = match dht.published().await {
        Ok(published) => published
            .into_iter()
            .filter(|publication| publication.active)
            .map(|publication| publication.podcast_id)
            .collect(),
        Err(err) => {
            report.error = Some(format!("cannot read own publications: {err}"));
            BTreeSet::new()
        }
    };
    let held: BTreeMap<PodcastId, Vec<FeedUrlPrivacyWarning>> = stats
        .held_back
        .into_iter()
        .map(|hold| (hold.podcast_id, hold.warnings))
        .collect();
    let failed: BTreeMap<PodcastId, String> = stats
        .failed
        .into_iter()
        .map(|failure| (failure.podcast_id, failure.error))
        .collect();
    for podcast_id in shared {
        let publication = if let Some(warnings) = held.get(&podcast_id) {
            Publication::HeldBack(warnings.clone())
        } else if let Some(error) = failed.get(&podcast_id) {
            Publication::Failed(error.clone())
        } else if active.contains(&podcast_id) {
            Publication::Published
        } else {
            Publication::Pending
        };
        report.podcasts.insert(podcast_id, publication);
    }
    report
}

/// Wakes the running reconciler.
#[derive(Debug, Clone)]
pub struct Reconciler {
    wake: Arc<Notify>,
}

impl Reconciler {
    /// Requests a reconciliation soon (after [`RECONCILE_DEBOUNCE`]). A
    /// request during a pass is kept and runs another pass afterwards.
    pub fn wake(&self) {
        self.wake.notify_one();
    }
}

/// Runs the reconciler until the overlay watch closes. `dht` follows the
/// federation node; while it is `None` nothing is published from this
/// device and `report` says so.
pub fn spawn(
    mut dht: watch::Receiver<Option<Arc<PodcastDht>>>,
    store: Option<Arc<PodcastStore>>,
    confirmations: Arc<ShareConfirmations>,
    report: impl Fn(PublicationReport) + Send + 'static,
) -> Reconciler {
    let wake = Arc::new(Notify::new());
    let reconciler = Reconciler {
        wake: Arc::clone(&wake),
    };
    tokio::spawn(async move {
        loop {
            let current = dht.borrow_and_update().clone();
            match current {
                Some(dht) => {
                    let pass = reconcile(&dht, store.clone(), Arc::clone(&confirmations)).await;
                    report(pass);
                }
                None => report(PublicationReport {
                    discovery: Discovery::Off(
                        "federation is off or podcast discovery is unavailable".into(),
                    ),
                    ..PublicationReport::default()
                }),
            }
            tokio::select! {
                _ = wake.notified() => tokio::time::sleep(RECONCILE_DEBOUNCE).await,
                changed = dht.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                _ = tokio::time::sleep(RECONCILE_INTERVAL) => {}
            }
        }
    });
    reconciler
}

/// Russian text of a privacy warning, for the share confirmation dialog.
pub fn warning_ru(warning: &FeedUrlPrivacyWarning) -> String {
    match warning {
        FeedUrlPrivacyWarning::SecretQueryParameter { name } => {
            format!("в адресе ленты есть параметр «{name}», похожий на ключ доступа")
        }
        FeedUrlPrivacyWarning::PrivateFeedHost { host } => {
            format!("{host} обычно выдаёт личные ленты для платных подписчиков")
        }
        FeedUrlPrivacyWarning::HighEntropyQueryValue { name } => {
            format!("значение параметра «{name}» похоже на секретный токен")
        }
        FeedUrlPrivacyWarning::HighEntropyPathSegment { index } => {
            format!("часть пути адреса №{} похожа на секретный токен", index + 1)
        }
        other => other.to_string(),
    }
}
