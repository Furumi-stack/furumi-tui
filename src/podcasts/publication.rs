//! Public podcast discovery: publishing the podcasts the user chose to share.
//!
//! The share decision itself is synced personal state
//! (`PodcastStore::set_shared`, operation `podcast_share_set`); this module
//! turns the merged registers into this device's own publications in the
//! FRID overlay `music_dht::podcast_dht`:
//!
//! * [`share_plan`] builds one `PodcastShareState` per subscription, the
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
//!   to the exact URLs shown, so a feed that moves to another URL asks again,
//!   and to the share decision it was given for (the share register's
//!   operation), so a decision taken anywhere later — sharing turned off,
//!   unsubscribing, sharing again — asks again too. A podcast whose card
//!   looks private and is not confirmed for the current decision is held
//!   back by [`share_plan`] itself, and a card this device published for an
//!   earlier decision is revoked (`sync_shares` alone would keep publishing
//!   feed URLs it already publishes).
//!
//! Logging stays free of private data: only counts are logged, never feed
//! URLs, titles or ids of podcasts that are not shared.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use furumi_library::podcasts::{PodcastListFilter, PodcastStore, PodcastSummary};
use music_dht::podcast::{FeedUrl, PodcastId, ShareRecord, SubscriptionRecord, effective_share};
use music_dht::podcast_dht::{
    FeedUrlPrivacyWarning, PodcastCardSpec, PodcastDht, PodcastDhtSyncStats, PodcastPrivacyHold,
    PodcastShareState, feed_url_privacy_warning,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, watch};

/// Local file with the privacy confirmations, next to `podcasts.sqlite3`.
pub const CONFIRMATIONS_FILE: &str = "podcast-share-confirmations.json";
/// Format version of [`CONFIRMATIONS_FILE`]. Version 1 (URLs only, never
/// released) is read as empty: the user is asked again.
const CONFIRMATIONS_VERSION: u32 = 2;
/// Periodic reconciliation (on top of the triggers). Cards are re-signed by
/// the overlay's own maintenance every 10 minutes; this pass catches share
/// decisions that changed while the overlay was busy or unreachable.
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Triggers within this window are handled by one pass (a "check all"
/// refresh finishes many feeds in a row).
pub const RECONCILE_DEBOUNCE: Duration = Duration::from_secs(1);
/// How often the overlay's peer count is checked between passes, so the
/// Podcasts tab does not show a stale "no discovery peers yet".
pub const PEER_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

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

/// The share decision a confirmation was given for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Decision {
    /// Confirmed before this device recorded its share decision: covers the
    /// first share decision newer than the share register seen then
    /// (`after`, its order key; `None`: no register yet).
    Pending { after: Option<(u64, i64, String)> },
    /// Covers exactly the share decision written by operation `op_id`.
    Bound { op_id: String },
}

/// One confirmation: the URLs shown and the decision it was given for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Confirmation {
    urls: ConfirmedUrls,
    decision: Decision,
}

impl Confirmation {
    /// Whether the confirmation covers the merged share register `share`.
    fn covers(&self, share: Option<&ShareRecord>) -> bool {
        let Some(share) = share.filter(|share| share.shared) else {
            return false;
        };
        match &self.decision {
            Decision::Bound { op_id } => *op_id == share.op_id,
            Decision::Pending { after } => is_after(after.as_ref(), share),
        }
    }

    /// A newer share decision than the confirmed one exists: sharing was
    /// turned off, or turned on again after a revocation, here or on
    /// another trusted device. Unsubscribing never changes the share
    /// register, but sharing after subscribing again writes a new decision.
    fn superseded(&self, share: Option<&ShareRecord>) -> bool {
        match (&self.decision, share) {
            (Decision::Bound { op_id }, Some(share)) => *op_id != share.op_id,
            (Decision::Pending { after }, Some(share)) => {
                !share.shared && is_after(after.as_ref(), share)
            }
            (_, None) => false,
        }
    }
}

/// `share` is newer than the register order key `after` (`None`: there was
/// no register).
fn is_after(after: Option<&(u64, i64, String)>, share: &ShareRecord) -> bool {
    after.is_none_or(|(generation, hlc, op)| share.order_key() > (*generation, *hlc, op.as_str()))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConfirmationsFile {
    version: u32,
    confirmed: BTreeMap<PodcastId, Confirmation>,
}

/// Device-local record of "publish this feed URL although it looks
/// private" (`PodcastShareState::privacy_confirmed`). Not synced.
///
/// A confirmation covers the URLs shown and one share decision: it is
/// recorded before the decision is written (pending), bound to the share
/// register's operation once written ([`ShareConfirmations::bind`]), and
/// dropped when a newer decision supersedes it — including decisions taken
/// on other trusted devices ([`ShareConfirmations::prune`]).
#[derive(Debug)]
pub struct ShareConfirmations {
    /// `None`: in memory only (tests, or the podcast store is unavailable).
    path: Option<PathBuf>,
    confirmed: Mutex<BTreeMap<PodcastId, Confirmation>>,
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
        // A pending confirmation only lives between `confirm` and `bind`
        // within one `set_shared` call; one found at start belongs to a
        // call that never finished (the decision may not have been written)
        // and must not cover a later decision taken elsewhere.
        let mut confirmed = confirmed;
        confirmed
            .retain(|_, confirmation| !matches!(confirmation.decision, Decision::Pending { .. }));
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

    /// Whether the user confirmed publishing exactly these feed URLs for
    /// the current share decision `share` (the merged share register).
    pub fn is_confirmed(&self, card: &PodcastCardSpec, share: Option<&ShareRecord>) -> bool {
        self.lock()
            .get(&card.podcast_id)
            .is_some_and(|confirmation| {
                confirmation.urls == ConfirmedUrls::of(card) && confirmation.covers(share)
            })
    }

    /// Records the confirmation of `urls` for the next share decision after
    /// `share` (the merged share register now, before this device writes
    /// its decision), replacing an older one. Blocking (writes the file).
    pub fn confirm(
        &self,
        podcast_id: &PodcastId,
        urls: ConfirmedUrls,
        share: Option<&ShareRecord>,
    ) -> Result<()> {
        let after = share.map(|share| {
            let (generation, hlc, op) = share.order_key();
            (generation, hlc, op.to_string())
        });
        self.update(podcast_id, |entry| {
            *entry = Some(Confirmation {
                urls,
                decision: Decision::Pending { after },
            });
        })
    }

    /// Binds the confirmation of a podcast to the share decision `share`
    /// this device just wrote (or found already written, when it confirms
    /// a podcast shared elsewhere). No-op without a confirmation or when
    /// `share` does not share. Blocking.
    pub fn bind(&self, podcast_id: &PodcastId, share: &ShareRecord) -> Result<()> {
        if !share.shared {
            return Ok(());
        }
        self.update(podcast_id, |entry| {
            if let Some(confirmation) = entry {
                confirmation.decision = Decision::Bound {
                    op_id: share.op_id.clone(),
                };
            }
        })
    }

    /// Drops a confirmation that a newer share decision superseded (taken
    /// here or on another trusted device), and binds a pending one to the
    /// decision it now covers. Blocking.
    pub fn prune(&self, podcast_id: &PodcastId, share: Option<&ShareRecord>) -> Result<()> {
        self.update(podcast_id, |entry| {
            let Some(confirmation) = entry else {
                return;
            };
            if confirmation.superseded(share) {
                *entry = None;
            } else if let (Decision::Pending { .. }, Some(share)) = (&confirmation.decision, share)
                && confirmation.covers(Some(share))
            {
                confirmation.decision = Decision::Bound {
                    op_id: share.op_id.clone(),
                };
            }
        })
    }

    /// Drops the confirmation of a podcast (sharing turned off or
    /// unsubscribed here): sharing it again asks again. Blocking.
    pub fn forget(&self, podcast_id: &PodcastId) -> Result<()> {
        self.update(podcast_id, |entry| *entry = None)
    }

    /// The confirmation of a podcast as it is now, to put back with
    /// [`ShareConfirmations::restore`].
    fn entry(&self, podcast_id: &PodcastId) -> Option<Confirmation> {
        self.lock().get(podcast_id).cloned()
    }

    /// Puts back the confirmation `previous` of a podcast (the decision it
    /// was given for could not be written). Takes effect in memory even
    /// when the file cannot be saved: a pending confirmation left in the
    /// file is dropped when it is read again ([`ShareConfirmations::open`]).
    /// Blocking.
    fn restore(&self, podcast_id: &PodcastId, previous: Option<Confirmation>) -> Result<()> {
        let mut confirmed = self.lock();
        if confirmed.get(podcast_id) == previous.as_ref() {
            return Ok(());
        }
        match previous {
            Some(confirmation) => confirmed.insert(podcast_id.clone(), confirmation),
            None => confirmed.remove(podcast_id),
        };
        self.save(&confirmed)
    }

    /// Applies `change` to the entry of `podcast_id` and saves the file when
    /// the entry changed.
    fn update(
        &self,
        podcast_id: &PodcastId,
        change: impl FnOnce(&mut Option<Confirmation>),
    ) -> Result<()> {
        let mut confirmed = self.lock();
        let current = confirmed.get(podcast_id).cloned();
        let mut entry = current.clone();
        change(&mut entry);
        if entry == current {
            return Ok(());
        }
        let mut next = confirmed.clone();
        match entry {
            Some(confirmation) => next.insert(podcast_id.clone(), confirmation),
            None => next.remove(podcast_id),
        };
        self.save(&next)?;
        *confirmed = next;
        Ok(())
    }

    fn save(&self, confirmed: &BTreeMap<PodcastId, Confirmation>) -> Result<()> {
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

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<PodcastId, Confirmation>> {
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
///
/// The artwork and web page URLs are left out when they look private
/// themselves (`feed_url_privacy_warning`) or when the feed URLs do: member
/// feeds serve signed artwork and item links, and the share dialog shows
/// the feed URLs only.
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
    let mut card = PodcastCardSpec {
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
    };
    let private_feed = !card.privacy_warnings().is_empty();
    for url in [&mut card.image_url, &mut card.link] {
        if private_feed || url.as_deref().is_some_and(looks_private) {
            *url = None;
        }
    }
    let card = card.bounded();
    card.validate()?;
    Ok(card)
}

/// An artwork or web page URL that must not be published: it carries what
/// looks like an access key (the feed URL rules), or it is not a plain
/// http(s) URL those rules can check.
fn looks_private(url: &str) -> bool {
    FeedUrl::parse(url).map_or(true, |url| feed_url_privacy_warning(&url).is_some())
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
    let share = store.share(podcast_id)?;
    Ok(ShareCheck {
        urls: ConfirmedUrls::of(&card),
        warnings: card.privacy_warnings(),
        confirmed: confirmations.is_confirmed(&card, share.as_ref()),
    })
}

/// Writes a share decision with `record` (the synced `podcast_share_set`
/// operation). The confirmation of `confirm` is recorded first (pending: it
/// covers the next share decision), so a reconciler pass the decision
/// triggers already sees it, and is then bound to the decision in force —
/// the one just written, or, for a podcast shared elsewhere and held back
/// here, the one already there (`set_shared` writes nothing then). Turning
/// sharing off forgets it. Blocking.
pub fn set_shared(
    store: &PodcastStore,
    confirmations: &ShareConfirmations,
    podcast_id: &PodcastId,
    shared: bool,
    confirm: Option<ConfirmedUrls>,
    record: impl FnOnce() -> Result<()>,
) -> Result<()> {
    // What to put back if the decision cannot be written: a pending
    // confirmation must not outlive this call, or it would cover the next
    // share decision taken on any device, which this user never saw.
    let mut previous = None;
    if let Some(urls) = confirm {
        let share = store.share(podcast_id)?;
        previous = Some(confirmations.entry(podcast_id));
        confirmations.confirm(podcast_id, urls, share.as_ref())?;
    }
    if let Err(err) = record() {
        if let Some(previous) = previous
            && let Err(restore) = confirmations.restore(podcast_id, previous)
        {
            tracing::warn!("podcast share confirmation not restored: {restore:#}");
        }
        return Err(err);
    }
    if !shared {
        return confirmations.forget(podcast_id);
    }
    match store.share(podcast_id)? {
        Some(share) => confirmations.bind(podcast_id, &share),
        None => Ok(()),
    }
}

/// What this device hands to `PodcastDht::sync_shares`: the share states,
/// and the shared podcasts it holds back itself.
#[derive(Debug, Default)]
pub struct SharePlan {
    /// One state per subscription, except the held-back podcasts.
    pub states: Vec<PodcastShareState>,
    /// Shared podcasts whose card has privacy warnings this device's user
    /// has not confirmed for the current share decision. They are left out
    /// of [`SharePlan::states`], so `sync_shares` revokes a card this device
    /// published for them earlier.
    pub held: BTreeMap<PodcastId, Vec<FeedUrlPrivacyWarning>>,
}

/// One `PodcastShareState` per podcast with a subscription register, the
/// input of `PodcastDht::sync_shares`, and the podcasts held back. Blocking.
///
/// Only podcasts for which `effective_share` holds get a card; for every
/// other podcast `card` is `None` and `privacy_confirmed` false, so no
/// private metadata is prepared for publication at all. Confirmations that
/// a newer share decision superseded (on any device) are dropped here.
///
/// A shared podcast whose card looks private and is not confirmed for the
/// current decision is held back here rather than by `sync_shares`: FRID
/// keeps publishing a card whose feed URLs this device already publishes
/// even without the confirmation, so a device that missed an intermediate
/// "share off" (closed, offline, or within the debounce) would go on
/// publishing a private-looking URL nobody confirmed for the new decision.
/// Leaving the podcast out makes `sync_shares` revoke that card instead.
pub fn share_plan(store: &PodcastStore, confirmations: &ShareConfirmations) -> Result<SharePlan> {
    let mut plan = SharePlan::default();
    for summary in store.list_podcasts(PodcastListFilter::All)? {
        let podcast_id = &summary.podcast_id;
        let Some(subscription) = store.subscription(podcast_id)? else {
            continue;
        };
        let share = store.share(podcast_id)?;
        if let Err(err) = confirmations.prune(podcast_id, share.as_ref()) {
            // Not dropping it only keeps a confirmation that no longer
            // covers the current decision (`is_confirmed` checks that).
            tracing::warn!("podcast share confirmation not updated: {err:#}");
        }
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
            .is_some_and(|card| confirmations.is_confirmed(card, share.as_ref()));
        if shared && !privacy_confirmed {
            let warnings = match &card {
                Some(card) => card.privacy_warnings(),
                None => PodcastCardSpec::from_subscription(&subscription)
                    .map(|card| card.privacy_warnings())
                    .unwrap_or_default(),
            };
            if !warnings.is_empty() {
                plan.held.insert(podcast_id.clone(), warnings);
                continue;
            }
        }
        plan.states.push(PodcastShareState {
            subscription,
            share,
            card,
            privacy_confirmed,
        });
    }
    Ok(plan)
}

/// The share states of [`share_plan`] (held-back podcasts left out).
/// Blocking.
#[cfg(test)]
pub(super) fn share_states(
    store: &PodcastStore,
    confirmations: &ShareConfirmations,
) -> Result<Vec<PodcastShareState>> {
    share_plan(store, confirmations).map(|plan| plan.states)
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
    /// Podcasts that are no longer shared but whose card from this device is
    /// still live: the revocation is pending, failed (retried every pass),
    /// or waits for the overlay (federation off). In memory only; never
    /// logged.
    pub unrevoked: BTreeSet<PodcastId>,
    /// The pass as a whole failed (store or overlay error), or cards could
    /// not be revoked.
    pub error: Option<String>,
}

impl PublicationReport {
    /// Sharing of `podcast_id` was turned off (or it was unsubscribed) on
    /// this device: until the next pass says otherwise, a card it may have
    /// published counts as not revoked yet.
    pub fn note_revocation(&mut self, podcast_id: &PodcastId) {
        if let Some(publication) = self.podcasts.remove(podcast_id)
            && !matches!(publication, Publication::HeldBack(_))
        {
            self.unrevoked.insert(podcast_id.clone());
        }
    }
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
    let SharePlan { states, held } = match store {
        Some(store) => {
            match tokio::task::spawn_blocking(move || share_plan(&store, &confirmations)).await {
                Ok(Ok(plan)) => plan,
                Ok(Err(err)) => {
                    revoke_all = Some(format!("cannot read share decisions ({err:#})"));
                    SharePlan::default()
                }
                Err(err) => {
                    revoke_all = Some(format!("cannot read share decisions ({err})"));
                    SharePlan::default()
                }
            }
        }
        None => {
            revoke_all = Some("the podcast database is unavailable".into());
            SharePlan::default()
        }
    };
    let shared: Vec<PodcastId> = states
        .iter()
        .filter(|state| state.is_shared())
        .map(|state| state.subscription.podcast_id.clone())
        .chain(held.keys().cloned())
        .collect();
    let stats = match dht.sync_shares(states).await {
        Ok(mut stats) => {
            stats
                .held_back
                .extend(
                    held.into_iter()
                        .map(|(podcast_id, warnings)| PodcastPrivacyHold {
                            podcast_id,
                            warnings,
                        }),
                );
            stats
        }
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
    summarize(&mut report, shared, stats, &active);
    report
}

/// Folds one `sync_shares` result and the set of this device's active
/// publications into `report`: the state of each shared podcast, the cards
/// of private podcasts that are still live (`unrevoked`), and an error when
/// revocations failed (counts only, no ids).
pub(super) fn summarize(
    report: &mut PublicationReport,
    shared: Vec<PodcastId>,
    stats: PodcastDhtSyncStats,
    active: &BTreeSet<PodcastId>,
) {
    let shared_set: BTreeSet<&PodcastId> = shared.iter().collect();
    let revoke_failed: BTreeSet<PodcastId> = stats
        .failed
        .iter()
        .filter(|failure| !shared_set.contains(&failure.podcast_id))
        .map(|failure| failure.podcast_id.clone())
        .collect();
    // Cards of podcasts that are private now but still live from here.
    report.unrevoked = active
        .iter()
        .filter(|podcast_id| !shared_set.contains(podcast_id))
        .cloned()
        .chain(revoke_failed.iter().cloned())
        .collect();
    if !revoke_failed.is_empty() && report.error.is_none() {
        let count = revoke_failed.len();
        tracing::warn!(count, "podcast publication: public cards not revoked yet");
        report.error = Some(format!(
            "{count} public card{} could not be revoked yet; retrying",
            if count == 1 { "" } else { "s" }
        ));
    }
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
}

/// Wakes the running reconciler.
#[derive(Debug, Clone)]
pub struct Reconciler {
    /// Debounced passes (feed refreshes, received operations).
    wake: Arc<Notify>,
    /// Immediate passes (revocations).
    urgent: Arc<Notify>,
    /// Number of revocations requested so far.
    revocations: Arc<AtomicU64>,
    /// Revocations covered by a finished pass (or by a pass that found the
    /// overlay off, when nothing can be sent).
    covered: watch::Receiver<u64>,
}

impl Reconciler {
    /// Requests a reconciliation soon (after [`RECONCILE_DEBOUNCE`]). A
    /// request during a pass is kept and runs another pass afterwards.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Requests a reconciliation now, without the debounce: sharing was
    /// turned off or a podcast unsubscribed, and its card must be revoked
    /// before the user may quit. See [`Reconciler::flush`].
    pub fn revoke_now(&self) {
        self.revocations.fetch_add(1, Ordering::SeqCst);
        self.urgent.notify_one();
    }

    /// Waits, at most `timeout`, until a pass started after every
    /// [`Reconciler::revoke_now`] so far finished (the tombstones were
    /// signed and pushed, or the overlay is off and nothing can be sent).
    /// Returns at once when nothing is pending. `false` on timeout or when
    /// the reconciler stopped.
    pub async fn flush(&self, timeout: Duration) -> bool {
        let target = self.revocations.load(Ordering::SeqCst);
        let mut covered = self.covered.clone();
        if *covered.borrow() >= target {
            return true;
        }
        // `revoke_now` already asked for the pass.
        matches!(
            tokio::time::timeout(timeout, covered.wait_for(|covered| *covered >= target)).await,
            Ok(Ok(_))
        )
    }
}

/// Runs the reconciler until the overlay watch closes. `dht` follows the
/// federation node; while it is `None` nothing is published from this
/// device and `report` says so (keeping the cards known not to be revoked
/// yet: they are revoked once the overlay runs again). Between passes the
/// overlay's peer count is refreshed every [`PEER_REFRESH_INTERVAL`].
pub fn spawn(
    mut dht: watch::Receiver<Option<Arc<PodcastDht>>>,
    store: Option<Arc<PodcastStore>>,
    confirmations: Arc<ShareConfirmations>,
    report: impl Fn(PublicationReport) + Send + 'static,
) -> Reconciler {
    let wake = Arc::new(Notify::new());
    let urgent = Arc::new(Notify::new());
    let revocations = Arc::new(AtomicU64::new(0));
    let (covered_tx, covered) = watch::channel(0);
    let reconciler = Reconciler {
        wake: Arc::clone(&wake),
        urgent: Arc::clone(&urgent),
        revocations: Arc::clone(&revocations),
        covered,
    };
    tokio::spawn(async move {
        let mut last = PublicationReport::default();
        let mut peers = tokio::time::interval(PEER_REFRESH_INTERVAL);
        peers.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        peers.reset();
        loop {
            let requested = revocations.load(Ordering::SeqCst);
            let current = dht.borrow_and_update().clone();
            last = match current {
                Some(dht) => reconcile(&dht, store.clone(), Arc::clone(&confirmations)).await,
                None => PublicationReport {
                    discovery: Discovery::Off(
                        "federation is off or podcast discovery is unavailable".into(),
                    ),
                    unrevoked: std::mem::take(&mut last.unrevoked),
                    ..PublicationReport::default()
                },
            };
            report(last.clone());
            covered_tx.send_replace(requested);
            let periodic = tokio::time::sleep(RECONCILE_INTERVAL);
            tokio::pin!(periodic);
            loop {
                tokio::select! {
                    _ = urgent.notified() => break,
                    _ = wake.notified() => {
                        tokio::time::sleep(RECONCILE_DEBOUNCE).await;
                        // Wakes during the debounce are covered by this
                        // pass: drop the permit they left.
                        let _ = futures_util::FutureExt::now_or_never(wake.notified());
                        break;
                    }
                    changed = dht.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        break;
                    }
                    _ = &mut periodic => break,
                    _ = peers.tick() => {
                        // Keep the peer count live (the first bootstrap
                        // usually finishes after the first pass).
                        let Discovery::Running { peers: shown } = &mut last.discovery else {
                            continue;
                        };
                        let Some(known) = dht.borrow().as_ref().map(|dht| dht.known_peers()) else {
                            continue;
                        };
                        if *shown != known {
                            *shown = known;
                            report(last.clone());
                        }
                    }
                }
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
