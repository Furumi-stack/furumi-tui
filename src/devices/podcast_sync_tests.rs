//! Podcast sync between two (or three) TUI device stores, with the same
//! functions the stream handlers use (`ops_for_peer`, `snapshot_for`,
//! `local_playback_snapshot_for`, `apply_*`). Every message goes through its
//! JSON line, like on the wire. The network round trip itself is covered by
//! `localhost_devices_sync_podcasts` in `tests.rs`.

use super::*;
use furumi_library::podcasts::{
    CheckpointOutcome, PodcastListFilter, ProgressSession, SubscribeOutcome,
};
use music_dht::podcast::{PodcastEpisodeRef, PodcastId, PodcastMeta, ProgressStatus};
use tokio::sync::mpsc::UnboundedReceiver;

pub(super) struct Device {
    pub(super) sync: DeviceSync,
    pub(super) store: Arc<PodcastStore>,
    pub(super) events: UnboundedReceiver<AppEvent>,
    pub(super) id: String,
    _library_dir: tempfile::TempDir,
}

/// A device with an in-memory sync log and its podcast store attached.
pub(super) fn device() -> Device {
    device_with_store(Arc::new(PodcastStore::open_in_memory().unwrap()), true)
}

pub(super) fn device_with_store(store: Arc<PodcastStore>, attach: bool) -> Device {
    let library_dir = tempfile::tempdir().unwrap();
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let sync = DeviceSync {
        _identity_lock: None,
        conn: Arc::new(std::sync::Mutex::new(conn)),
        library: Arc::new(Library::open(&library_dir.path().join("library.sqlite3")).unwrap()),
        event_tx: Default::default(),
        playback: Default::default(),
        podcasts: Default::default(),
    };
    let (tx, events) = tokio::sync::mpsc::unbounded_channel();
    sync.set_event_tx(tx);
    let id = sync.ensure_identity().unwrap().device_id;
    if attach {
        sync.attach_podcast_store(Arc::clone(&store)).unwrap();
    }
    Device {
        sync,
        store,
        events,
        id,
        _library_dir: library_dir,
    }
}

/// Trusts `b` on `a` and the other way round, in `a`'s group, with the
/// versions their profiles report.
pub(super) fn pair(a: &Device, b: &Device) {
    b.sync
        .set_group_id(&a.sync.ensure_identity().unwrap().group_id)
        .unwrap();
    for (left, right) in [(a, b), (b, a)] {
        let profile = right.sync.own_profile("").unwrap();
        left.sync
            .apply_device_profile(
                &profile,
                true,
                VersionReport::SelfReported(Some(profile.protocol_version)),
            )
            .unwrap();
    }
}

/// The `hello` that `sync_device` sends from `from` to `to`, as JSON.
pub(super) fn hello_json(from: &Device, to_id: &str) -> String {
    let version = from.sync.peer_protocol_version(to_id).unwrap();
    let mut message = WireMessage::Hello {
        group_id: from.sync.ensure_identity().unwrap().group_id,
        profile: from.sync.own_profile("").unwrap(),
        devices: from.sync.device_profiles().unwrap(),
        vector: from.sync.vector().unwrap(),
        ops: from.sync.ops_for_peer(to_id, version).unwrap(),
        snapshot: from.sync.snapshot_for(version).unwrap(),
        playback: from.sync.local_playback_snapshot_for(version),
    };
    fit_podcast_snapshot(&mut message);
    serde_json::to_string(&message).unwrap()
}

/// What `handle_hello` does with a `hello` line, minus the transport.
pub(super) fn receive(to: &DeviceSync, json: &str) {
    let WireMessage::Hello {
        profile,
        devices,
        vector,
        ops,
        snapshot,
        playback,
        ..
    } = serde_json::from_str(json).unwrap()
    else {
        panic!("expected a hello");
    };
    to.apply_device_profile(
        &profile,
        false,
        VersionReport::SelfReported(Some(profile.protocol_version)),
    )
    .unwrap();
    to.apply_device_profiles(&devices, Some(&profile.device_id))
        .unwrap();
    if let Some(playback) = playback {
        to.apply_playback_snapshot(&profile.device_id, playback)
            .unwrap();
    }
    to.apply_received(snapshot, ops).unwrap();
    to.note_peer_vector(&profile.device_id, &vector).unwrap();
}

pub(super) fn deliver(from: &Device, to: &Device) {
    receive(&to.sync, &hello_json(from, &to.id));
}

/// One sync in each direction.
pub(super) fn sync_round(a: &Device, b: &Device) {
    deliver(a, b);
    deliver(b, a);
}

pub(super) fn meta() -> PodcastMeta {
    crate::app::podcast_fixtures::row("Show", 0, 0, 0)
        .summary
        .meta
}

pub(super) fn episode_ref(n: u32) -> PodcastEpisodeRef {
    crate::app::podcast_fixtures::episode(n, &format!("Episode {n}"), None)
        .episode
        .to_ref(&meta())
}

pub(super) fn subscribe(device: &Device) -> SubscribeOutcome {
    let store = Arc::clone(&device.store);
    device
        .sync
        .run_podcast_action(|stamps| {
            let outcome = store.subscribe(stamps, &meta())?;
            let ops = outcome.ops.clone();
            Ok((outcome, ops))
        })
        .unwrap()
}

pub(super) fn start(device: &Device, episode: &PodcastEpisodeRef, at_ms: i64) -> ProgressSession {
    let store = Arc::clone(&device.store);
    let session_id = device.sync.new_listen_id();
    device
        .sync
        .run_podcast_action(|stamps| {
            let write = store.start_session(stamps, episode, &session_id, at_ms, None)?;
            Ok((write.session.clone(), vec![write.op]))
        })
        .unwrap()
}

pub(super) fn checkpoint(
    device: &Device,
    session: &ProgressSession,
    at_ms: i64,
) -> CheckpointOutcome {
    let store = Arc::clone(&device.store);
    device
        .sync
        .run_podcast_action(|stamps| {
            let outcome = store.checkpoint(stamps, session, at_ms)?;
            let ops = match &outcome {
                CheckpointOutcome::Written(write) => vec![write.op.clone()],
                _ => Vec::new(),
            };
            Ok((outcome, ops))
        })
        .unwrap()
}

fn restart(device: &Device, episode: &PodcastEpisodeRef) -> ProgressSession {
    let store = Arc::clone(&device.store);
    let session_id = device.sync.new_listen_id();
    device
        .sync
        .run_podcast_action(|stamps| {
            let write = store.restart(stamps, episode, &session_id)?;
            Ok((write.session.clone(), vec![write.op]))
        })
        .unwrap()
}

fn synced_events(device: &mut Device) -> Vec<(Vec<ProgressRecord>, bool)> {
    let mut events = Vec::new();
    while let Ok(event) = device.events.try_recv() {
        if let AppEvent::PodcastsSynced { progress, changed } = event {
            events.push((progress, changed));
        }
    }
    events
}

fn subscribed_ids(store: &PodcastStore) -> Vec<PodcastId> {
    store
        .list_podcasts(PodcastListFilter::Subscribed)
        .unwrap()
        .into_iter()
        .map(|summary| summary.podcast_id)
        .collect()
}

fn logged_kinds(device: &Device) -> Vec<String> {
    let conn = lock(&device.sync.conn);
    let mut stmt = conn
        .prepare("SELECT kind FROM sync_ops ORDER BY seq")
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap()
}

#[test]
fn a_subscription_on_one_device_appears_on_the_other_without_its_url() {
    let a = device();
    let mut b = device();
    pair(&a, &b);
    let outcome = subscribe(&a);
    assert!(!outcome.already_subscribed);

    deliver(&a, &b);
    assert_eq!(
        subscribed_ids(&b.store),
        std::slice::from_ref(&outcome.podcast_id)
    );
    let podcast = b.store.podcast(&outcome.podcast_id).unwrap().unwrap();
    assert_eq!(podcast.meta.title, "Show");
    assert!(!podcast.shared, "synced subscriptions stay private");
    // B fetches the feed itself, from the synced URL.
    assert_eq!(
        b.store.podcasts_due_for_refresh(now_ms(), 16).unwrap(),
        [(outcome.podcast_id.clone(), meta().feed_url)]
    );
    // The UI hears about it once.
    assert_eq!(synced_events(&mut b), [(Vec::new(), true)]);

    // Delivering the same state again (operations and snapshot) changes
    // nothing and creates no duplicate.
    receive(&b.sync, &hello_json(&a, &b.id));
    deliver(&a, &b);
    assert_eq!(subscribed_ids(&b.store).len(), 1);
    assert!(synced_events(&mut b).is_empty());
}

#[test]
fn progress_saved_on_one_device_resumes_on_the_other_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("podcasts.sqlite3");
    let a = device();
    let mut b = device_with_store(Arc::new(PodcastStore::open(&path).unwrap()), true);
    pair(&a, &b);
    subscribe(&a);
    let episode = episode_ref(1);
    let session = start(&a, &episode, 0);
    checkpoint(&a, &session, 600_000);
    checkpoint(&a, &session, 1_800_000);
    // Periodic checkpoints are coalesced: one progress operation remains.
    assert_eq!(
        logged_kinds(&a)
            .iter()
            .filter(|kind| *kind == "podcast_progress_set")
            .count(),
        1
    );

    deliver(&a, &b);
    assert_eq!(
        b.store.resume_position_ms(&episode.episode_id).unwrap(),
        1_800_000
    );
    assert!(!synced_events(&mut b).is_empty());
    // A later checkpoint arrives as an operation only: the UI gets the
    // register to show in place.
    checkpoint(&a, &session, 1_830_000);
    deliver(&a, &b);
    let events = synced_events(&mut b);
    assert!(
        events
            .iter()
            .flat_map(|(progress, _)| progress)
            .any(|record| record.position_ms == 1_830_000),
        "{events:?}"
    );

    // Days later: B starts again with the same files.
    drop(b);
    let store = Arc::new(PodcastStore::open(&path).unwrap());
    let b = device_with_store(Arc::clone(&store), true);
    let queued = TrackItem::podcast_episode(episode.clone());
    let (resolved, start_ms) = crate::podcasts::playback::resolve(
        Some(&b.store),
        queued.podcast_episode_ref().unwrap(),
        crate::podcasts::playback::StartAt::Resume,
    )
    .unwrap();
    assert_eq!(start_ms, 1_830_000);
    assert_eq!(resolved.episode_id, episode.episode_id);
    assert!(resolved.progress_generation >= Some(1));
}

#[test]
fn start_over_on_one_device_beats_a_late_checkpoint_from_the_other() {
    let a = device();
    let b = device();
    pair(&a, &b);
    subscribe(&a);
    let episode = episode_ref(2);
    let session_a = start(&a, &episode, 0);
    checkpoint(&a, &session_a, 2_400_000);
    sync_round(&a, &b);
    assert_eq!(
        b.store.resume_position_ms(&episode.episode_id).unwrap(),
        2_400_000
    );

    // B starts over while A (offline) still plays and saves a checkpoint
    // later in wall-clock time.
    restart(&b, &episode);
    std::thread::sleep(Duration::from_millis(5));
    assert!(matches!(
        checkpoint(&a, &session_a, 2_460_000),
        CheckpointOutcome::Written(_)
    ));
    // Delivery in both orders converges on the start over.
    deliver(&a, &b);
    deliver(&b, &a);
    for device in [&a, &b] {
        let progress = device.store.progress(&episode.episode_id).unwrap().unwrap();
        assert_eq!(progress.position_ms, 0, "{progress:?}");
        assert_eq!(progress.status, ProgressStatus::InProgress);
        assert_eq!(progress.device_id, b.id);
    }
    // A's session stops saving: its next checkpoint is superseded.
    assert!(matches!(
        checkpoint(&a, &session_a, 2_470_000),
        CheckpointOutcome::Superseded { .. }
    ));
}

#[test]
fn unsubscribing_keeps_progress_and_resubscribing_continues() {
    let a = device();
    let b = device();
    pair(&a, &b);
    let podcast_id = subscribe(&a).podcast_id;
    let episode = episode_ref(3);
    let session = start(&a, &episode, 0);
    checkpoint(&a, &session, 900_000);
    sync_round(&a, &b);

    let store = Arc::clone(&a.store);
    a.sync
        .run_podcast_action(|stamps| {
            let ops = store.unsubscribe(stamps, &podcast_id)?;
            Ok(((), ops))
        })
        .unwrap();
    sync_round(&a, &b);
    for device in [&a, &b] {
        assert!(subscribed_ids(&device.store).is_empty());
        assert!(
            !device
                .store
                .subscription(&podcast_id)
                .unwrap()
                .unwrap()
                .subscribed
        );
        assert_eq!(
            device
                .store
                .resume_position_ms(&episode.episode_id)
                .unwrap(),
            900_000
        );
    }

    // Subscribing again on B reuses the podcast and its progress, and the
    // later decision wins on A as well.
    let again = subscribe(&b);
    assert_eq!(again.podcast_id, podcast_id);
    sync_round(&b, &a);
    for device in [&a, &b] {
        assert_eq!(
            subscribed_ids(&device.store),
            std::slice::from_ref(&podcast_id)
        );
        assert_eq!(
            device
                .store
                .resume_position_ms(&episode.episode_id)
                .unwrap(),
            900_000
        );
    }
}

#[test]
fn operations_received_before_the_store_opens_are_applied_when_it_does() {
    let a = device();
    let mut b = device_with_store(Arc::new(PodcastStore::open_in_memory().unwrap()), false);
    pair(&a, &b);
    let podcast_id = subscribe(&a).podcast_id;
    let episode = episode_ref(4);
    let session = start(&a, &episode, 0);
    checkpoint(&a, &session, 120_000);
    deliver(&a, &b);
    assert!(subscribed_ids(&b.store).is_empty());
    // The operations are in B's log (and acknowledged) already: nothing
    // would deliver them again.
    assert!(
        logged_kinds(&b)
            .iter()
            .any(|kind| kind == "podcast_subscription_set")
    );

    b.sync.attach_podcast_store(Arc::clone(&b.store)).unwrap();
    assert_eq!(subscribed_ids(&b.store), [podcast_id]);
    assert_eq!(
        b.store.resume_position_ms(&episode.episode_id).unwrap(),
        120_000
    );
    assert!(!synced_events(&mut b).is_empty());
    let pending: i64 = lock(&b.sync.conn)
        .query_row("SELECT COUNT(*) FROM sync_podcast_pending", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(pending, 0);
}

#[test]
fn podcast_operations_are_relayed_to_devices_that_were_not_online() {
    let a = device();
    let b = device();
    let c = device();
    pair(&a, &b);
    pair(&b, &c);
    let podcast_id = subscribe(&a).podcast_id;
    deliver(&a, &b);
    // C never talks to A.
    deliver(&b, &c);
    assert_eq!(subscribed_ids(&c.store), [podcast_id]);
}

/// A peer that still runs protocol v3 (furumi-tui 0.3.x, furumi-web
/// 0.12.x), simulated with its exact JSON types.
#[test]
fn a_v3_peer_gets_no_podcast_state_and_catches_up_from_the_snapshot_after_upgrading() {
    use super::wire_compat_tests::legacy;

    let mut a = device();
    let old = device();
    a.sync.set_group_id("grp_test").unwrap();
    let old_profile = legacy::DeviceProfileWire {
        device_id: old.id.clone(),
        name: "old".into(),
        client_version: "0.3.5".into(),
        protocol_version: 3,
        endpoint_id: String::new(),
        endpoint_ticket: String::new(),
        revoked: false,
        revoke_cutoff_seq: None,
        updated_at_ms: 1,
    };
    // Its hello, as a 0.3.x client writes it, with a profile announcement
    // that has no version field.
    let old_hello = legacy::WireMessage::Hello {
        group_id: "grp_test".into(),
        profile: old_profile.clone(),
        devices: vec![old_profile.clone()],
        vector: BTreeMap::from([(old.id.clone(), 1)]),
        ops: vec![legacy::SyncOpWire {
            op_id: format!("{}:1", old.id),
            origin_device_id: old.id.clone(),
            seq: 1,
            hlc_ms: 1,
            payload: legacy::SyncOpPayload::DeviceProfileSet {
                name: "old".into(),
                client_version: "0.3.5".into(),
                endpoint_ticket: String::new(),
                endpoint_id: String::new(),
            },
        }],
        snapshot: legacy::SyncSnapshot::default(),
        playback: None,
    };
    a.sync.apply_device_trusted(&old.id, 1).unwrap();
    receive(&a.sync, &serde_json::to_string(&old_hello).unwrap());
    assert_eq!(a.sync.peer_protocol_version(&old.id).unwrap(), 3);

    let podcast_id = subscribe(&a).podcast_id;
    let episode = episode_ref(5);
    let session = start(&a, &episode, 0);
    checkpoint(&a, &session, 300_000);
    a.sync
        .record_content_like(&format!("b3:{}", "c".repeat(64)), true)
        .unwrap();
    a.sync.publish_playback(PlaybackSnapshot {
        device_id: a.id.clone(),
        device_name: "A".into(),
        active: true,
        updated_at_ms: now_ms(),
        state: PlaybackStateWire {
            queue: vec![PlaybackTrack::podcast_episode(episode.clone())],
            queue_pos: 0,
            playing: true,
            paused: false,
            idle_since_ms: None,
            position_secs: 300.0,
            volume: 80,
            shuffle: false,
            repeat: PlaybackRepeat::Off,
            playback_rate: None,
        },
        coordination: None,
    });

    // What A sends the v3 peer parses with the v3 types and has no podcast
    // operation, podcast section or podcast-bearing playback state.
    let json = hello_json(&a, &old.id);
    let parsed: legacy::WireMessage = serde_json::from_str(&json).unwrap();
    let legacy::WireMessage::Hello {
        ops,
        snapshot,
        playback,
        vector,
        ..
    } = parsed
    else {
        panic!("expected a hello");
    };
    assert!(!json.contains("podcast"), "{json}");
    assert_eq!(ops.len(), 1, "only the like: {ops:?}");
    assert_eq!(snapshot.likes.len(), 1);
    assert!(playback.is_none());
    // The old peer acknowledges everything it was sent.
    a.sync.note_peer_vector(&old.id, &vector).unwrap();

    // After upgrading, its hello reports v4: the skipped operations are not
    // sent again, but the snapshot carries the podcast state.
    receive(&a.sync, &hello_json(&old, &a.id));
    assert_eq!(
        a.sync.peer_protocol_version(&old.id).unwrap(),
        PROTOCOL_VERSION
    );
    let json = hello_json(&a, &old.id);
    let WireMessage::Hello {
        ops,
        snapshot,
        playback,
        ..
    } = serde_json::from_str(&json).unwrap()
    else {
        panic!("expected a hello");
    };
    assert!(ops.iter().all(|op| !op.payload.is_podcast()), "{ops:?}");
    assert_eq!(snapshot.podcast_subscriptions.len(), 1);
    assert_eq!(snapshot.podcast_progress.len(), 1);
    assert!(playback.is_some());
    receive(&old.sync, &json);
    assert_eq!(subscribed_ids(&old.store), [podcast_id]);
    assert_eq!(
        old.store.resume_position_ms(&episode.episode_id).unwrap(),
        300_000
    );
    let _ = synced_events(&mut a);
}

#[test]
fn podcast_snapshot_sections_shrink_to_fit_the_line_limit() {
    let a = device();
    let b = device();
    pair(&a, &b);
    subscribe(&a);
    let mut message = WireMessage::Hello {
        group_id: String::new(),
        profile: a.sync.own_profile("").unwrap(),
        devices: Vec::new(),
        vector: BTreeMap::new(),
        ops: Vec::new(),
        snapshot: a.sync.snapshot_for(PROTOCOL_VERSION).unwrap(),
        playback: None,
    };
    assert_eq!(
        message_snapshot(&mut message).podcast_subscriptions.len(),
        1
    );
    // A music part that leaves no room for podcast state.
    message_snapshot(&mut message).likes = (0..70_000)
        .map(|n| SnapshotLike {
            content_id: format!("b3:{n:064x}"),
            hlc_ms: n,
            op_id: format!("x:{n}"),
            fed: None,
        })
        .collect();
    fit_podcast_snapshot(&mut message);
    assert!(!message_snapshot(&mut message).has_podcast_state());
    // The music part is never cut for podcast state.
    assert_eq!(message_snapshot(&mut message).likes.len(), 70_000);
}
