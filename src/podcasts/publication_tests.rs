//! Publication of shared podcasts: share states built from the store, the
//! local privacy confirmations, and a two-node loopback test of the real
//! discovery overlay (no internet, temp directories only).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use furumi_library::podcasts::{OpStamp, PodcastLocalOp, PodcastStore, StampSource};
use music_dht::device_sync::SyncOpWire;
use music_dht::podcast::{FeedUrl, PodcastId, PodcastMeta};
use music_dht::podcast_dht::{
    FeedUrlPrivacyWarning, PODCAST_DHT_ALPN, PodcastDht, PodcastDhtSyncStats, PodcastSyncFailure,
};
use music_dht::{MusicDhtConfig, MusicDhtService, NetworkId};

use super::publication::{
    Discovery, Publication, PublicationReport, ShareConfirmations, check_share, reconcile,
    set_shared, share_card, share_plan, share_states, summarize, warning_ru,
};

const SHOW_URL: &str = "https://feeds.example.test/rust-radio.xml";
const SECRET_URL: &str = "https://feeds.example.test/members.xml?token=0123456789abcdef";
const PRIVATE_URL: &str = "https://feeds.example.test/diary.xml";
const NEWS_URL: &str = "https://feeds.example.test/crab-news.xml";

/// Stamps of one device's local actions; keeps the operations they emit.
pub(super) struct Device {
    id: &'static str,
    seq: i64,
    ops: Vec<SyncOpWire>,
}

struct Stamps<'a> {
    device: &'a str,
    next: i64,
}

impl StampSource for Stamps<'_> {
    fn device_id(&self) -> &str {
        self.device
    }

    fn next_stamp(&mut self) -> Result<OpStamp> {
        self.next += 1;
        Ok(OpStamp::new(
            format!("{}:{}", self.device, self.next),
            self.device,
            1_700_000_000_000 + self.next,
        ))
    }
}

impl Device {
    pub(super) fn new(id: &'static str) -> Self {
        Self {
            id,
            seq: 0,
            ops: Vec::new(),
        }
    }

    fn act(
        &mut self,
        action: impl FnOnce(&mut dyn StampSource) -> Result<Vec<PodcastLocalOp>>,
    ) -> Vec<SyncOpWire> {
        let mut stamps = Stamps {
            device: self.id,
            next: self.seq,
        };
        let ops = action(&mut stamps).unwrap();
        let mut wires = Vec::new();
        for op in ops {
            self.seq += 1;
            wires.push(op.into_wire(self.seq));
        }
        self.ops.extend(wires.iter().cloned());
        wires
    }

    fn subscribe(&mut self, store: &PodcastStore, url: &str, title: &str) -> PodcastId {
        let meta = PodcastMeta {
            feed_url: FeedUrl::parse(url).unwrap(),
            title: title.to_string(),
            author: Some("Ferris Crab".to_string()),
            description: Some("Weekly news.".to_string()),
            link: None,
            image_url: None,
        };
        self.subscribe_meta(store, &meta)
    }

    fn subscribe_meta(&mut self, store: &PodcastStore, meta: &PodcastMeta) -> PodcastId {
        let mut id = None;
        self.act(|stamps| {
            let outcome = store.subscribe(stamps, meta)?;
            id = Some(outcome.podcast_id);
            Ok(outcome.ops)
        });
        id.unwrap()
    }

    pub(super) fn set_shared(
        &mut self,
        store: &PodcastStore,
        id: &PodcastId,
        shared: bool,
    ) -> Vec<SyncOpWire> {
        self.act(|stamps| store.set_shared(stamps, id, shared))
    }

    fn unsubscribe(&mut self, store: &PodcastStore, id: &PodcastId) {
        self.act(|stamps| store.unsubscribe(stamps, id));
    }
}

/// The user confirms the URLs the dialog shows for a podcast that is
/// already shared (held back here): no new decision is written, the
/// confirmation is bound to the one in force (as the runtime does).
fn confirm_shown(store: &PodcastStore, confirmations: &ShareConfirmations, id: &PodcastId) {
    let check = check_share(store, confirmations, id).unwrap();
    set_shared(store, confirmations, id, true, Some(check.urls), || Ok(())).unwrap();
}

/// Whether this device would publish `id` with its privacy confirmed.
fn confirmed_here(
    store: &PodcastStore,
    confirmations: &ShareConfirmations,
    id: &PodcastId,
) -> bool {
    share_states(store, confirmations)
        .unwrap()
        .into_iter()
        .find(|state| state.subscription.podcast_id == *id)
        .is_some_and(|state| state.is_shared() && state.privacy_confirmed)
}

fn shared_ids(store: &PodcastStore, confirmations: &ShareConfirmations) -> Vec<PodcastId> {
    share_states(store, confirmations)
        .unwrap()
        .into_iter()
        .filter(|state| state.is_shared())
        .map(|state| state.subscription.podcast_id)
        .collect()
}

#[test]
fn share_states_carry_cards_only_for_shared_podcasts() {
    let store = PodcastStore::open_in_memory().unwrap();
    let confirmations = ShareConfirmations::in_memory();
    let mut device = Device::new("device-a");
    let show = device.subscribe(&store, SHOW_URL, "Rust Radio Weekly");
    let diary = device.subscribe(&store, PRIVATE_URL, "Secret Garden Diaries");

    // A new subscription is private: no state is shared, no card is built.
    let states = share_states(&store, &confirmations).unwrap();
    assert_eq!(states.len(), 2);
    assert!(states.iter().all(|state| !state.is_shared()));
    assert!(states.iter().all(|state| state.card.is_none()));

    device.set_shared(&store, &show, true);
    let states = share_states(&store, &confirmations).unwrap();
    let shown = states
        .iter()
        .find(|state| state.subscription.podcast_id == show)
        .unwrap();
    assert!(shown.is_shared());
    let card = shown.card.clone().unwrap();
    assert_eq!(card.feed_url, FeedUrl::parse(SHOW_URL).unwrap());
    assert_eq!(card.id_feed_url, None, "the feed did not move");
    assert_eq!(card.title, "Rust Radio Weekly");
    assert_eq!(card.episode_count, Some(0));
    assert!(!shown.privacy_confirmed);
    let private = states
        .iter()
        .find(|state| state.subscription.podcast_id == diary)
        .unwrap();
    assert!(!private.is_shared());
    assert!(private.card.is_none());
    assert_eq!(shared_ids(&store, &confirmations), vec![show.clone()]);

    // Share off and unsubscribe both make the podcast private again.
    device.set_shared(&store, &show, false);
    assert!(shared_ids(&store, &confirmations).is_empty());
    device.set_shared(&store, &show, true);
    assert_eq!(shared_ids(&store, &confirmations), vec![show.clone()]);
    device.unsubscribe(&store, &show);
    assert!(shared_ids(&store, &confirmations).is_empty());
    // Subscribing again is private until shared again.
    device.subscribe(&store, SHOW_URL, "Rust Radio Weekly");
    assert!(shared_ids(&store, &confirmations).is_empty());
}

#[test]
fn a_stale_share_decision_from_another_device_does_not_republish() {
    let store = PodcastStore::open_in_memory().unwrap();
    let confirmations = ShareConfirmations::in_memory();
    let mut device = Device::new("device-a");
    let show = device.subscribe(&store, SHOW_URL, "Rust Radio Weekly");
    let shared = device.set_shared(&store, &show, true);

    // Another trusted device turns sharing off; its operation arrives here.
    let other = PodcastStore::open_in_memory().unwrap();
    for op in &device.ops {
        other.apply_remote_op(op).unwrap();
    }
    let mut device_b = Device::new("device-b");
    let revoke = device_b.set_shared(&other, &show, false);
    for op in &revoke {
        store.apply_remote_op(op).unwrap();
    }
    assert!(shared_ids(&store, &confirmations).is_empty());

    // The old "share" decision delivered again (a late device) loses.
    for op in &shared {
        store.apply_remote_op(op).unwrap();
    }
    assert!(shared_ids(&store, &confirmations).is_empty());
}

#[test]
fn privacy_confirmations_are_local_and_bound_to_the_urls() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("podcast-share-confirmations.json");
    let store = PodcastStore::open_in_memory().unwrap();
    let mut device = Device::new("device-a");
    let secret = device.subscribe(&store, SECRET_URL, "Members Only");
    let show = device.subscribe(&store, SHOW_URL, "Rust Radio Weekly");

    let confirmations = ShareConfirmations::open(path.clone());
    let check = check_share(&store, &confirmations, &show).unwrap();
    assert!(check.warnings.is_empty());
    assert!(!check.needs_confirmation());

    let check = check_share(&store, &confirmations, &secret).unwrap();
    assert!(check.needs_confirmation());
    assert!(matches!(
        check.warnings.as_slice(),
        [FeedUrlPrivacyWarning::SecretQueryParameter { name }] if name == "token"
    ));
    assert_eq!(
        warning_ru(&check.warnings[0]),
        "в адресе ленты есть параметр «token», похожий на ключ доступа"
    );
    // The user confirms in the dialog; the decision is written after the
    // (pending) confirmation, which is then bound to it.
    set_shared(
        &store,
        &confirmations,
        &secret,
        true,
        Some(check.urls.clone()),
        || {
            device.set_shared(&store, &secret, true);
            Ok(())
        },
    )
    .unwrap();

    // Persisted on this device, read again after a restart.
    let reopened = ShareConfirmations::open(path.clone());
    let check = check_share(&store, &reopened, &secret).unwrap();
    assert!(check.confirmed);
    assert!(!check.needs_confirmation());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "only the user may read the feed URLs");
    }

    // Other URLs (the feed moved) are not covered.
    let card = share_card(&store, &secret).unwrap().unwrap();
    let moved = music_dht::podcast_dht::PodcastCardSpec {
        feed_url: FeedUrl::parse("https://feeds.example.test/members.xml?token=ffffffffffffffff")
            .unwrap(),
        id_feed_url: Some(card.feed_url.clone()),
        ..card.clone()
    };
    let share = store.share(&secret).unwrap();
    assert!(reopened.is_confirmed(&card, share.as_ref()));
    assert!(!reopened.is_confirmed(&moved, share.as_ref()));

    // The share state uses the confirmation.
    assert!(confirmed_here(&store, &reopened, &secret));

    reopened.forget(&secret).unwrap();
    assert!(!ShareConfirmations::open(path.clone()).is_confirmed(&card, share.as_ref()));

    // A corrupt file asks again instead of failing.
    std::fs::write(&path, b"{not json").unwrap();
    assert!(!ShareConfirmations::open(path).is_confirmed(&card, share.as_ref()));
}

#[test]
fn a_confirmation_does_not_outlive_a_revocation_on_another_device() {
    let temp = tempfile::tempdir().unwrap();
    let store = PodcastStore::open_in_memory().unwrap();
    let confirmations = ShareConfirmations::open(temp.path().join("confirmations.json"));
    let mut device = Device::new("device-a");
    let secret = device.subscribe(&store, SECRET_URL, "Members Only");
    let other = PodcastStore::open_in_memory().unwrap();
    let mut device_b = Device::new("device-b");
    let sync = |from: &[SyncOpWire], to: &PodcastStore| {
        for op in from {
            to.apply_remote_op(op).unwrap();
        }
    };

    // A: the dialog, confirmed; the confirmation covers A's decision.
    let check = check_share(&store, &confirmations, &secret).unwrap();
    assert!(check.needs_confirmation());
    set_shared(
        &store,
        &confirmations,
        &secret,
        true,
        Some(check.urls),
        || {
            device.set_shared(&store, &secret, true);
            Ok(())
        },
    )
    .unwrap();
    assert!(confirmed_here(&store, &confirmations, &secret));

    // B turns sharing off and on again (confirming on B). A must ask its
    // user again: the confirmation was given for the revoked decision.
    sync(&device.ops, &other);
    let ops = [
        device_b.set_shared(&other, &secret, false),
        device_b.set_shared(&other, &secret, true),
    ]
    .concat();
    sync(&ops, &store);
    assert!(
        !confirmed_here(&store, &confirmations, &secret),
        "held back again"
    );
    let check = check_share(&store, &confirmations, &secret).unwrap();
    assert!(check.needs_confirmation());
    assert!(
        !ShareConfirmations::open(temp.path().join("confirmations.json")).is_confirmed(
            &share_card(&store, &secret).unwrap().unwrap(),
            store.share(&secret).unwrap().as_ref()
        ),
        "the superseded confirmation is dropped from the file"
    );

    // A confirms again (no new decision: B's is in force) ...
    confirm_shown(&store, &confirmations, &secret);
    assert!(confirmed_here(&store, &confirmations, &secret));
    // ... and B only turns sharing off: when it is shared again later
    // (from either device), A asks again.
    let off = device_b.set_shared(&other, &secret, false);
    sync(&off, &store);
    assert!(shared_ids(&store, &confirmations).is_empty());
    let check = check_share(&store, &confirmations, &secret).unwrap();
    assert!(check.needs_confirmation(), "sharing again asks again");

    // Shared again on B and confirmed on A; then B unsubscribes, subscribes
    // again and shares: a new decision, A asks again.
    let on = device_b.set_shared(&other, &secret, true);
    sync(&on, &store);
    confirm_shown(&store, &confirmations, &secret);
    assert!(confirmed_here(&store, &confirmations, &secret));
    device_b.unsubscribe(&other, &secret);
    device_b.subscribe(&other, SECRET_URL, "Members Only");
    let on = device_b.set_shared(&other, &secret, true);
    assert!(!on.is_empty());
    sync(&device_b.ops, &store);
    assert!(!confirmed_here(&store, &confirmations, &secret));

    // A pending confirmation (the decision is not written yet) covers
    // nothing until a newer share decision exists.
    let fresh = device.subscribe(
        &store,
        "https://feeds.example.test/vip.xml?key=0123456789abcdef",
        "VIP",
    );
    let check = check_share(&store, &confirmations, &fresh).unwrap();
    confirmations
        .confirm(&fresh, check.urls, store.share(&fresh).unwrap().as_ref())
        .unwrap();
    assert!(
        !check_share(&store, &confirmations, &fresh)
            .unwrap()
            .confirmed
    );
    device.set_shared(&store, &fresh, true);
    assert!(
        confirmed_here(&store, &confirmations, &fresh),
        "covers the next decision"
    );
}

#[test]
fn a_confirmation_for_a_decision_that_was_not_written_is_dropped() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("confirmations.json");
    let store = PodcastStore::open_in_memory().unwrap();
    let confirmations = ShareConfirmations::open(path.clone());
    let mut device = Device::new("device-a");
    let secret = device.subscribe(&store, SECRET_URL, "Members Only");
    let other = PodcastStore::open_in_memory().unwrap();
    let mut device_b = Device::new("device-b");

    // The user confirms, but the decision cannot be written (for example
    // the podcast was unsubscribed by a sync meanwhile, or the database
    // failed): nothing of the confirmation is left.
    let check = check_share(&store, &confirmations, &secret).unwrap();
    assert!(check.needs_confirmation());
    let result = set_shared(
        &store,
        &confirmations,
        &secret,
        true,
        Some(check.urls.clone()),
        || anyhow::bail!("not subscribed"),
    );
    assert!(result.is_err());
    assert!(shared_ids(&store, &confirmations).is_empty());

    // Days later sharing is turned on on device B: A was never asked for
    // that decision, so it holds the podcast back.
    for op in &device.ops {
        other.apply_remote_op(op).unwrap();
    }
    for op in &device_b.set_shared(&other, &secret, true) {
        store.apply_remote_op(op).unwrap();
    }
    assert!(!confirmed_here(&store, &confirmations, &secret));
    assert!(
        share_plan(&store, &confirmations)
            .unwrap()
            .held
            .contains_key(&secret),
        "held back on A"
    );
    assert!(
        check_share(&store, &confirmations, &secret)
            .unwrap()
            .needs_confirmation()
    );
    assert!(!ShareConfirmations::open(path.clone()).is_confirmed(
        &share_card(&store, &secret).unwrap().unwrap(),
        store.share(&secret).unwrap().as_ref()
    ));

    // A pending confirmation left in the file by a call that never
    // finished (the app was killed before the decision was written) is
    // dropped when the file is read again.
    let fresh = device.subscribe(
        &store,
        "https://feeds.example.test/vip.xml?key=0123456789abcdef",
        "VIP",
    );
    let check = check_share(&store, &confirmations, &fresh).unwrap();
    confirmations
        .confirm(&fresh, check.urls, store.share(&fresh).unwrap().as_ref())
        .unwrap();
    let reopened = ShareConfirmations::open(path);
    device.set_shared(&store, &fresh, true);
    assert!(confirmed_here(&store, &confirmations, &fresh));
    assert!(!confirmed_here(&store, &reopened, &fresh));
}

#[test]
fn cards_leave_out_artwork_and_links_that_look_private() {
    let store = PodcastStore::open_in_memory().unwrap();
    let mut device = Device::new("device-a");
    let meta = |feed: &str, image: &str, link: &str| PodcastMeta {
        feed_url: FeedUrl::parse(feed).unwrap(),
        title: "Art Show".to_string(),
        author: None,
        description: None,
        image_url: Some(image.to_string()),
        link: Some(link.to_string()),
    };
    // A public feed: plain URLs stay, a signed one is left out.
    let show = device.subscribe_meta(
        &store,
        &meta(
            SHOW_URL,
            "https://cdn.example.test/art.jpg?token=0123456789abcdef",
            "https://rust-radio.example.test/",
        ),
    );
    let card = share_card(&store, &show).unwrap().unwrap();
    assert_eq!(
        card.image_url, None,
        "the signed artwork URL is not published"
    );
    assert_eq!(
        card.link.as_deref(),
        Some("https://rust-radio.example.test/")
    );
    assert!(card.privacy_warnings().is_empty());

    // Public hosts name artwork and pages by UUIDs and content hashes in
    // the path: kept (a key in the query is not).
    for public in [
        "https://megaphone.imgix.net/podcasts/1f2e3d4c-5b6a-11ee-8c99-0242ac120002/image/art.jpg",
        "https://image.simplecastcdn.com/images/4c3b2a19-8d7e-4f6a-b5c4-d3e2f1a0b9c8/3a2b1c0d-9e8f-4a7b-6c5d-4e3f2a1b0c9d/3000x3000/art.jpg",
        "https://is1-ssl.mzstatic.com/image/thumb/Podcasts126/v4/0a/1b/2c/0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5d/mza_1234567890123456789.jpg/600x600bb.jpg",
        "https://d3t3ozftmdmh3i.cloudfront.net/production/podcast_uploaded/1234567/1234567-1600000000000-0123456789abc.jpg",
    ] {
        assert!(
            FeedUrl::parse(public)
                .ok()
                .and_then(|url| music_dht::podcast_dht::feed_url_privacy_warning(&url))
                .is_some(),
            "the feed URL rules flag {public}"
        );
        let show = device.subscribe_meta(
            &store,
            &PodcastMeta {
                feed_url: FeedUrl::parse(&format!(
                    "https://feeds.example.test/{}.xml",
                    public.len()
                ))
                .unwrap(),
                ..meta(SHOW_URL, public, public)
            },
        );
        let card = share_card(&store, &show).unwrap().unwrap();
        assert_eq!(card.image_url.as_deref(), Some(public));
        assert_eq!(card.link.as_deref(), Some(public));
    }
    for signed in [
        "https://cdn.example.test/art.jpg?sig=Zm9vYmFyYmF6cXV4cXV1eDEyMzQ1Njc4OQ",
        "https://www.patreon.com/file?h=1234&i=5678",
        "ftp://cdn.example.test/art.jpg",
    ] {
        let show = device.subscribe_meta(
            &store,
            &PodcastMeta {
                feed_url: FeedUrl::parse(&format!(
                    "https://feeds.example.test/signed-{}.xml",
                    signed.len()
                ))
                .unwrap(),
                ..meta(SHOW_URL, signed, signed)
            },
        );
        let card = share_card(&store, &show).unwrap().unwrap();
        assert_eq!((card.image_url, card.link), (None, None), "{signed}");
    }

    // A member feed (its URL needs the dialog): artwork and page are left
    // out whatever they look like, the dialog shows the feed URLs only.
    let secret = device.subscribe_meta(
        &store,
        &meta(
            SECRET_URL,
            "https://cdn.example.test/members-art.jpg",
            "https://members.example.test/show",
        ),
    );
    let card = share_card(&store, &secret).unwrap().unwrap();
    assert!(!card.privacy_warnings().is_empty());
    assert_eq!((card.image_url, card.link), (None, None));
}

#[test]
fn revocations_that_fail_are_reported_without_ids() {
    let show = PodcastId::from_feed_url(&FeedUrl::parse(SHOW_URL).unwrap());
    let diary = PodcastId::from_feed_url(&FeedUrl::parse(PRIVATE_URL).unwrap());
    let secret = PodcastId::from_feed_url(&FeedUrl::parse(SECRET_URL).unwrap());

    // The tombstone of the private diary could not be pushed: its card is
    // still live and the user is told, without the podcast's id or URL.
    let mut report = PublicationReport::default();
    let stats = PodcastDhtSyncStats {
        failed: vec![PodcastSyncFailure {
            podcast_id: diary.clone(),
            error: "no replica reached".into(),
        }],
        ..PodcastDhtSyncStats::default()
    };
    let active = [show.clone(), diary.clone()].into_iter().collect();
    summarize(&mut report, vec![show.clone()], stats, &active);
    assert_eq!(report.podcasts.get(&show), Some(&Publication::Published));
    assert_eq!(
        report.podcasts.get(&diary),
        None,
        "private: not listed as shared"
    );
    assert_eq!(report.unrevoked, [diary.clone()].into_iter().collect());
    let error = report.error.clone().unwrap();
    assert_eq!(error, "1 public card could not be revoked yet; retrying");
    assert!(!error.contains(diary.as_str()));

    // A private podcast whose card is still active (not revoked yet) is
    // unrevoked too, without an error; a failed shared one is Failed.
    let mut report = PublicationReport::default();
    let stats = PodcastDhtSyncStats {
        failed: vec![PodcastSyncFailure {
            podcast_id: show.clone(),
            error: "signing failed".into(),
        }],
        ..PodcastDhtSyncStats::default()
    };
    let active = [secret.clone()].into_iter().collect();
    summarize(&mut report, vec![show.clone()], stats, &active);
    assert_eq!(
        report.podcasts.get(&show),
        Some(&Publication::Failed("signing failed".into()))
    );
    assert_eq!(report.unrevoked, [secret.clone()].into_iter().collect());
    assert_eq!(report.error, None);

    // Turning sharing off in the UI counts a published card as not revoked
    // until the next pass; a held-back one was never published here.
    let mut report = PublicationReport::default();
    report.podcasts.insert(show.clone(), Publication::Published);
    report
        .podcasts
        .insert(secret.clone(), Publication::HeldBack(Vec::new()));
    report.discovery = Discovery::Running { peers: 1 };
    report.note_revocation(&show, true);
    report.note_revocation(&secret, true);
    assert!(report.podcasts.is_empty());
    assert_eq!(report.unrevoked, [show.clone()].into_iter().collect());

    // Without a pass of the running overlay nothing tells whether a card
    // of an earlier session is live: a podcast that was shared counts.
    for discovery in [
        Discovery::Off("federation is off".into()),
        Discovery::Starting,
    ] {
        let mut report = PublicationReport {
            discovery,
            ..PublicationReport::default()
        };
        report.note_revocation(&show, true);
        report.note_revocation(&diary, false);
        assert_eq!(report.unrevoked, [show.clone()].into_iter().collect());
    }
}

// ---------------------------------------------------------------------------
// Two loopback nodes

pub(super) struct Node {
    pub(super) service: Arc<MusicDhtService>,
    pub(super) dht: Arc<PodcastDht>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Node {
    pub(super) async fn start(directory: &std::path::Path, network: NetworkId) -> Self {
        std::fs::create_dir_all(directory).unwrap();
        let config = MusicDhtConfig::builder()
            .data_dir(directory)
            .network_id(network)
            .request_timeout(Duration::from_secs(2))
            .lookup_timeout(Duration::from_secs(5))
            .transport_timeout(Duration::from_secs(5))
            .dial_timeout(Duration::from_secs(2))
            .schema_independent_stream_protocol(PODCAST_DHT_ALPN)
            .build()
            .unwrap();
        let (service, mut events) = MusicDhtService::start(config).await.unwrap();
        let service = Arc::new(service);
        let events = tokio::spawn(async move { while events.recv().await.is_some() {} });
        // Same wiring as `federation::open_podcast_dht`.
        let acceptor = service.stream_acceptor(PODCAST_DHT_ALPN).unwrap();
        let dht = PodcastDht::open(
            Arc::clone(&service),
            directory.join(crate::federation::PODCAST_DHT_CACHE_FILE),
        )
        .await
        .unwrap();
        let serve = tokio::spawn(Arc::clone(&dht).serve(acceptor));
        Self {
            service,
            dht,
            tasks: vec![events, serve],
        }
    }

    pub(super) async fn stop(self) {
        for task in &self.tasks {
            task.abort();
        }
        self.service.shutdown().await.unwrap();
    }
}

pub(super) async fn wait_for<T>(what: &str, mut check: impl AsyncFnMut() -> Option<T>) -> T {
    let started = std::time::Instant::now();
    loop {
        if let Some(value) = check().await {
            return value;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn found(node: &Node, id: &PodcastId) -> bool {
    node.dht.lookup_podcast(id).await.unwrap().is_some()
}

fn publication(report: &PublicationReport, id: &PodcastId) -> Option<Publication> {
    report.podcasts.get(id).cloned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_podcast_is_found_by_a_stranger_and_revoked_by_share_off() {
    tokio::time::timeout(Duration::from_secs(240), two_nodes())
        .await
        .unwrap();
}

async fn two_nodes() {
    let temp = tempfile::tempdir().unwrap();
    let network = NetworkId::from_name("furumi-tui-podcast-publication-test");
    // A: this TUI with its podcast store. C: a stranger outside A's
    // trusted-device group (no device sync at all).
    let a = Node::start(&temp.path().join("a"), network).await;
    let c = Node::start(&temp.path().join("c"), network).await;
    c.service
        .connect(a.service.ticket().await.unwrap())
        .await
        .unwrap();
    wait_for("music DHT contacts", async || {
        (!a.service.known_peers().is_empty() && !c.service.known_peers().is_empty()).then_some(())
    })
    .await;
    for node in [&a, &c] {
        node.dht.bootstrap().await.unwrap();
    }
    wait_for("overlay contacts", async || {
        (a.dht.known_peers() >= 1 && c.dht.known_peers() >= 1).then_some(())
    })
    .await;

    let store = Arc::new(PodcastStore::open_in_memory().unwrap());
    let confirmations = Arc::new(ShareConfirmations::open(
        temp.path().join("podcast-share-confirmations.json"),
    ));
    let mut device = Device::new("device-a");
    let show = device.subscribe(&store, SHOW_URL, "Rust Radio Weekly");
    let diary = device.subscribe(&store, PRIVATE_URL, "Secret Garden Diaries");
    let secret = device.subscribe(&store, SECRET_URL, "Members Only Show");

    // Nothing is shared yet: nothing is published.
    let report = reconcile(&a.dht, Some(Arc::clone(&store)), Arc::clone(&confirmations)).await;
    assert_eq!(report.error, None);
    assert!(report.podcasts.is_empty());
    assert!(a.dht.published().await.unwrap().is_empty());

    // A shares two podcasts; the one with a private-looking URL is held
    // back until confirmed on this device.
    device.set_shared(&store, &show, true);
    device.set_shared(&store, &secret, true);
    let report = reconcile(&a.dht, Some(Arc::clone(&store)), Arc::clone(&confirmations)).await;
    assert_eq!(report.error, None);
    assert!(matches!(report.discovery, Discovery::Running { peers } if peers >= 1));
    assert_eq!(publication(&report, &show), Some(Publication::Published));
    assert!(matches!(
        publication(&report, &secret),
        Some(Publication::HeldBack(warnings)) if !warnings.is_empty()
    ));
    assert_eq!(publication(&report, &diary), None);

    // The stranger finds the shared card by text and by id ...
    let hits = wait_for("the shared card", async || {
        let hits = c.dht.search("rust radio", 10).await.unwrap();
        (!hits.is_empty()).then_some(hits)
    })
    .await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].podcast_id, show);
    assert_eq!(hits[0].feed_url, FeedUrl::parse(SHOW_URL).unwrap());
    assert_eq!(hits[0].publishers, vec![a.service.endpoint_id()]);
    assert!(found(&c, &show).await);
    // ... but neither the private subscription nor the held-back one.
    assert!(!found(&c, &diary).await);
    assert!(c.dht.search("secret garden", 10).await.unwrap().is_empty());
    assert!(!found(&c, &secret).await);
    assert!(c.dht.search("members only", 10).await.unwrap().is_empty());

    // Confirming exactly the URLs shown publishes the held-back podcast.
    confirm_shown(&store, &confirmations, &secret);
    let report = reconcile(&a.dht, Some(Arc::clone(&store)), Arc::clone(&confirmations)).await;
    assert_eq!(publication(&report, &secret), Some(Publication::Published));
    wait_for("the confirmed card", async || {
        found(&c, &secret).await.then_some(())
    })
    .await;

    // Another trusted device B turns sharing off and on again while A does
    // not reconcile in between (closed, offline, or within the debounce).
    // The confirmation was given for the revoked decision: A holds the
    // podcast back and revokes its card, although it already publishes the
    // same feed URL (FRID alone would keep publishing it).
    let other = PodcastStore::open_in_memory().unwrap();
    for op in &device.ops {
        other.apply_remote_op(op).unwrap();
    }
    let mut device_b = Device::new("device-b");
    let off_on = [
        device_b.set_shared(&other, &secret, false),
        device_b.set_shared(&other, &secret, true),
    ]
    .concat();
    for op in &off_on {
        store.apply_remote_op(op).unwrap();
    }
    let report = reconcile(&a.dht, Some(Arc::clone(&store)), Arc::clone(&confirmations)).await;
    assert!(
        matches!(
            publication(&report, &secret),
            Some(Publication::HeldBack(warnings)) if !warnings.is_empty()
        ),
        "{report:?}"
    );
    assert!(
        check_share(&store, &confirmations, &secret)
            .unwrap()
            .needs_confirmation()
    );
    assert!(
        a.dht
            .published()
            .await
            .unwrap()
            .iter()
            .any(|publication| publication.podcast_id == secret && !publication.active),
        "the card confirmed for the earlier decision is revoked"
    );
    assert!(!found(&c, &secret).await);
    // A second pass changes nothing; confirming again publishes it.
    let report = reconcile(&a.dht, Some(Arc::clone(&store)), Arc::clone(&confirmations)).await;
    assert!(matches!(
        publication(&report, &secret),
        Some(Publication::HeldBack(_))
    ));
    assert!(!found(&c, &secret).await);
    confirm_shown(&store, &confirmations, &secret);
    let report = reconcile(&a.dht, Some(Arc::clone(&store)), Arc::clone(&confirmations)).await;
    assert_eq!(publication(&report, &secret), Some(Publication::Published));
    wait_for("the card confirmed again", async || {
        found(&c, &secret).await.then_some(())
    })
    .await;

    // Sharing off: the tombstone wins over the card at the stranger.
    let shared_op = device.ops.clone();
    device.set_shared(&store, &show, false);
    let report = reconcile(&a.dht, Some(Arc::clone(&store)), Arc::clone(&confirmations)).await;
    assert_eq!(publication(&report, &show), None);
    assert!(!found(&c, &show).await, "the tombstone hides the card");
    assert!(c.dht.search("rust radio", 10).await.unwrap().is_empty());
    let own = a.dht.published().await.unwrap();
    assert!(
        own.iter()
            .any(|publication| publication.podcast_id == show && !publication.active)
    );

    // A late copy of the old "share" operation does not republish it.
    for op in &shared_op {
        store.apply_remote_op(op).unwrap();
    }
    let report = reconcile(&a.dht, Some(Arc::clone(&store)), Arc::clone(&confirmations)).await;
    assert_eq!(publication(&report, &show), None);
    assert!(!found(&c, &show).await);

    // Unsubscribing revokes too.
    device.unsubscribe(&store, &secret);
    let report = reconcile(&a.dht, Some(Arc::clone(&store)), Arc::clone(&confirmations)).await;
    assert!(report.podcasts.is_empty());
    assert!(!found(&c, &secret).await);

    // Without a podcast store (it failed to open) every own card is revoked.
    device.subscribe(&store, SECRET_URL, "Members Only Show");
    device.set_shared(&store, &secret, true);
    confirm_shown(&store, &confirmations, &secret);
    let report = reconcile(&a.dht, Some(Arc::clone(&store)), Arc::clone(&confirmations)).await;
    assert_eq!(publication(&report, &secret), Some(Publication::Published));
    wait_for("the republished card", async || {
        found(&c, &secret).await.then_some(())
    })
    .await;
    let report = reconcile(&a.dht, None, Arc::clone(&confirmations)).await;
    assert!(report.error.is_some());
    assert!(report.podcasts.is_empty());
    // Nothing left to revoke: no error to show again.
    let report = reconcile(&a.dht, None, Arc::clone(&confirmations)).await;
    assert_eq!(report.error, None);
    assert!(!found(&c, &secret).await);
    assert!(
        a.dht
            .published()
            .await
            .unwrap()
            .iter()
            .all(|publication| !publication.active)
    );

    a.stop().await;
    c.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_reconciler_follows_the_overlay_and_revokes_without_the_debounce() {
    tokio::time::timeout(Duration::from_secs(180), reconciler_lifecycle())
        .await
        .unwrap();
}

/// `publication::spawn` as the runtime drives it: the federation watch,
/// debounced wakes, immediate revocations with the quit-time flush, the live
/// peer count, and the end of the task with the watch.
async fn reconciler_lifecycle() {
    use super::publication::{PEER_REFRESH_INTERVAL, RECONCILE_DEBOUNCE, spawn};

    let temp = tempfile::tempdir().unwrap();
    let network = NetworkId::from_name("furumi-tui-podcast-reconciler-test");
    let store = Arc::new(PodcastStore::open_in_memory().unwrap());
    let confirmations = Arc::new(ShareConfirmations::in_memory());
    let mut device = Device::new("device-a");
    let show = device.subscribe(&store, SHOW_URL, "Rust Radio Weekly");
    device.set_shared(&store, &show, true);

    let (overlay, watched) = tokio::sync::watch::channel(None);
    let (_registers, current) = tokio::sync::watch::channel(true);
    let (tx, mut reports) = tokio::sync::mpsc::unbounded_channel();
    let reconciler = spawn(
        watched,
        current,
        Some(Arc::clone(&store)),
        confirmations,
        move |report| {
            let _ = tx.send(report);
        },
    );
    let mut next = async || reports.recv().await.expect("a report");

    // Federation off: nothing is published, nothing to wait for on quit.
    let report = next().await;
    assert!(matches!(report.discovery, Discovery::Off(_)));
    assert!(report.podcasts.is_empty());
    assert!(reconciler.flush(Duration::from_millis(10)).await);

    // Sharing off while the federation is off (a card of an earlier
    // session may be live). Quitting while the decision is still being
    // written waits for the write and the pass, then says the card may not
    // be revoked; the Off reports keep it as not revoked.
    let ticket = reconciler.begin_revocation();
    assert!(
        !reconciler.flush(Duration::from_millis(50)).await,
        "the write is still running"
    );
    let flush = tokio::spawn({
        let reconciler = reconciler.clone();
        async move { reconciler.flush(Duration::from_secs(5)).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!flush.is_finished(), "quitting waits for the write");
    device.set_shared(&store, &show, false);
    ticket.done(Some(&show));
    assert!(!flush.await.unwrap(), "the card may not be revoked yet");
    let report = next().await;
    assert!(matches!(report.discovery, Discovery::Off(_)));
    assert_eq!(report.unrevoked, [show.clone()].into_iter().collect());
    reconciler.wake();
    let report = next().await;
    assert_eq!(
        report.unrevoked,
        [show.clone()].into_iter().collect(),
        "kept while the federation is off"
    );
    // A write that failed (the ticket is dropped) is not waited for.
    drop(reconciler.begin_revocation());
    // Shared again: no longer "private · not revoked".
    device.set_shared(&store, &show, true);
    reconciler.wake();
    let report = next().await;
    assert!(report.unrevoked.is_empty());
    assert!(reconciler.flush(Duration::from_millis(10)).await);

    // The overlay starts (two nodes, the overlay not bootstrapped yet): a
    // pass publishes right away.
    let a = Node::start(&temp.path().join("a"), network).await;
    let c = Node::start(&temp.path().join("c"), network).await;
    c.service
        .connect(a.service.ticket().await.unwrap())
        .await
        .unwrap();
    wait_for("music DHT contacts", async || {
        (!a.service.known_peers().is_empty()).then_some(())
    })
    .await;
    overlay.send_replace(Some(Arc::clone(&a.dht)));
    let report = next().await;
    assert!(matches!(report.discovery, Discovery::Running { .. }));
    assert_eq!(publication(&report, &show), Some(Publication::Published));

    // The peer count follows the overlay without another pass.
    a.dht.bootstrap().await.unwrap();
    c.dht.bootstrap().await.unwrap();
    let mut report = report;
    let started = std::time::Instant::now();
    while !matches!(report.discovery, Discovery::Running { peers } if peers >= 1) {
        assert!(
            started.elapsed() < PEER_REFRESH_INTERVAL * 3,
            "the peer count is refreshed"
        );
        report = next().await;
    }
    assert_eq!(publication(&report, &show), Some(Publication::Published));

    // Wakes within the debounce are handled by one pass.
    reconciler.wake();
    reconciler.wake();
    reconciler.wake();
    let report = next().await;
    assert_eq!(publication(&report, &show), Some(Publication::Published));
    assert!(
        tokio::time::timeout(RECONCILE_DEBOUNCE * 2, reports.recv())
            .await
            .is_err(),
        "one pass for several wakes"
    );

    // Sharing off: the revocation does not wait for the debounce, and the
    // quit-time flush returns once the tombstone was sent.
    device.set_shared(&store, &show, false);
    reconciler.revoke_now(Some(&show));
    assert!(
        reconciler.flush(RECONCILE_DEBOUNCE * 9 / 10).await,
        "revoked before the debounce would have ended"
    );
    let report = reports.recv().await.unwrap();
    assert!(report.podcasts.is_empty());
    assert!(report.unrevoked.is_empty());
    assert!(
        a.dht
            .published()
            .await
            .unwrap()
            .iter()
            .all(|publication| !publication.active)
    );
    assert!(!found(&c, &show).await);

    // The overlay stops: off again; the task ends with the watch.
    overlay.send_replace(None);
    let mut report = reports.recv().await.unwrap();
    while !matches!(report.discovery, Discovery::Off(_)) {
        // A peer count refresh may still come in between.
        assert!(report.podcasts.is_empty());
        report = reports.recv().await.unwrap();
    }
    drop(overlay);
    assert!(reports.recv().await.is_none(), "the reconciler stopped");

    a.stop().await;
    c.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_device_publishes_only_after_syncing_with_its_group() {
    tokio::time::timeout(Duration::from_secs(240), restart_before_device_sync())
        .await
        .unwrap();
}

/// Interop scenario 7c: device B was closed while another device of its
/// group turned sharing of a podcast off. Restarted with that stale "share"
/// decision, B does not send the card again while it waits for device sync
/// (`DeviceSync::podcast_registers_current`), still revokes what its own
/// registers no longer share, and publishes once its registers are current.
async fn restart_before_device_sync() {
    use super::publication::spawn;

    let temp = tempfile::tempdir().unwrap();
    let network = NetworkId::from_name("furumi-tui-podcast-restart-test");
    let store = Arc::new(PodcastStore::open_in_memory().unwrap());
    let confirmations = Arc::new(ShareConfirmations::in_memory());
    let mut device = Device::new("device-b");
    let show = device.subscribe(&store, SHOW_URL, "Rust Radio Weekly");
    let news = device.subscribe(&store, NEWS_URL, "Crab News");
    let diary = device.subscribe(&store, PRIVATE_URL, "Ferris Diary");
    for id in [&show, &news, &diary] {
        device.set_shared(&store, id, true);
    }

    // The earlier session, alone: the three cards are published (and kept
    // only in B's own cache).
    let b = Node::start(&temp.path().join("b"), network).await;
    let report = reconcile(&b.dht, Some(Arc::clone(&store)), Arc::clone(&confirmations)).await;
    for id in [&show, &news, &diary] {
        assert_eq!(publication(&report, id), Some(Publication::Published));
    }
    b.stop().await;

    // While B is closed, device A turns sharing of `news` off.
    let other = PodcastStore::open_in_memory().unwrap();
    for op in &device.ops {
        other.apply_remote_op(op).unwrap();
    }
    let share_off = Device::new("device-a").set_shared(&other, &news, false);

    // B starts again from the same directory, next to a stranger C.
    let b = Node::start(&temp.path().join("b"), network).await;
    let c = Node::start(&temp.path().join("c"), network).await;
    c.service
        .connect(b.service.ticket().await.unwrap())
        .await
        .unwrap();
    wait_for("overlay contacts", async || {
        b.dht.bootstrap().await.ok()?;
        c.dht.bootstrap().await.ok()?;
        (b.dht.known_peers() >= 1 && c.dht.known_peers() >= 1).then_some(())
    })
    .await;

    let (_overlay, watched) = tokio::sync::watch::channel(Some(Arc::clone(&b.dht)));
    let (registers, current) = tokio::sync::watch::channel(false);
    let (tx, mut reports) = tokio::sync::mpsc::unbounded_channel();
    let reconciler = spawn(
        watched,
        current,
        Some(Arc::clone(&store)),
        Arc::clone(&confirmations),
        move |report| {
            let _ = tx.send(report);
        },
    );
    let mut next = async |what: &str, check: &dyn Fn(&PublicationReport) -> bool| loop {
        let report = tokio::time::timeout(Duration::from_secs(30), reports.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .expect("a report");
        if check(&report) {
            return report;
        }
    };

    // Before device sync: nothing is published or sent, not even by the
    // overlay's own republishing.
    let report = next("the first pass", &|report| {
        matches!(report.discovery, Discovery::Running { .. })
    })
    .await;
    for id in [&show, &news, &diary] {
        assert!(!found(&c, id).await, "a card was sent before device sync");
    }
    assert_eq!(b.dht.republish().await.unwrap().records, 0);
    for id in [&show, &news, &diary] {
        assert!(
            !found(&c, id).await,
            "a card was republished before device sync"
        );
        assert_eq!(
            publication(&report, id),
            Some(Publication::WaitingForDevices)
        );
    }
    assert!(report.unrevoked.is_empty());

    // Sharing turned off here is revoked at once all the same.
    device.set_shared(&store, &diary, false);
    reconciler.revoke_now(Some(&diary));
    assert!(reconciler.flush(Duration::from_secs(20)).await);
    let report = next("the revocation", &|report| {
        !report.podcasts.contains_key(&diary)
    })
    .await;
    assert!(report.unrevoked.is_empty());
    assert_eq!(
        publication(&report, &news),
        Some(Publication::WaitingForDevices)
    );
    assert!(
        b.dht
            .published()
            .await
            .unwrap()
            .iter()
            .any(|publication| publication.podcast_id == diary && !publication.active)
    );

    // Device sync brings A's share-off, then the registers are current:
    // `news` is revoked, `show` published.
    for op in &share_off {
        store.apply_remote_op(op).unwrap();
    }
    registers.send_replace(true);
    let report = next("the full pass", &|report| {
        report.podcasts.get(&show) == Some(&Publication::Published)
    })
    .await;
    assert_eq!(publication(&report, &news), None);
    assert!(report.unrevoked.is_empty());
    wait_for("the shared card", async || {
        found(&c, &show).await.then_some(())
    })
    .await;
    assert!(!found(&c, &news).await);
    assert!(!found(&c, &diary).await);

    b.stop().await;
    c.stop().await;
}
