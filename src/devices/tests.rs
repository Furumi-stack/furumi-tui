use super::*;

/// Exercises the production stream handlers and SQLite adapter, not just the
/// ownership reducer. Each peer has a fresh identity and an isolated library.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn localhost_devices_exchange_state_and_handoff() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let network = NetworkId::from_name(&format!("device-test-{}", random_hex(16)));
        let mut peers = Vec::new();
        let mut servers = Vec::new();
        let mut receivers = Vec::new();
        for dir in &dirs {
            std::fs::create_dir_all(dir.path().join("network")).unwrap();
            let config = music_dht::MusicDhtConfig::builder()
                .data_dir(dir.path().join("network"))
                .network_id(network)
                .stream_protocol(SYNC_ALPN)
                .build()
                .unwrap();
            let (service, events) = MusicDhtService::start(config).await.unwrap();
            let service = Arc::new(service);
            let conn = Connection::open_in_memory().unwrap();
            init_schema(&conn).unwrap();
            let sync = Arc::new(DeviceSync {
                _identity_lock: Some(Arc::new(
                    acquire_identity_lock(&dir.path().join("sync.sqlite3")).unwrap(),
                )),
                conn: Arc::new(std::sync::Mutex::new(conn)),
                library: Arc::new(Library::open(&dir.path().join("library.sqlite3")).unwrap()),
                event_tx: Default::default(),
                playback: Default::default(),
            });
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            sync.set_event_tx(tx);
            let stats = Arc::new(crate::federation::TransportStats::default());
            servers.push(tokio::spawn(serve_peers(
                service.stream_acceptor(SYNC_ALPN).unwrap(),
                sync.clone(),
                service.clone(),
                stats.clone(),
            )));
            receivers.push(rx);
            peers.push((sync, service, stats, events));
        }
        let (a, service_a, stats_a, _) = &peers[0];
        let (b, service_b, stats_b, _) = &peers[1];
        b.ensure_identity().unwrap();
        b.set_group_id(&a.ensure_identity().unwrap().group_id)
            .unwrap();
        let profile_a = a
            .own_profile(&service_a.ticket().await.unwrap().to_string())
            .unwrap();
        let profile_b = b
            .own_profile(&service_b.ticket().await.unwrap().to_string())
            .unwrap();
        a.apply_device_profile(&profile_b, true).unwrap();
        b.apply_device_profile(&profile_a, true).unwrap();
        a.claim_playback(&profile_a.device_id).unwrap();
        a.playback_tick(true, true).unwrap();
        let mut state = empty_playback_state();
        state.playing = true;
        state.position_secs = 42.5;
        a.publish_playback(PlaybackSnapshot {
            device_id: profile_a.device_id.clone(),
            device_name: "TUI".into(),
            active: true,
            updated_at_ms: now_ms(),
            state: state.clone(),
            coordination: None,
        });
        a.sync_device(
            service_a.clone(),
            &a.active_remote_devices().unwrap()[0],
            stats_a.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            b.with_playback_engine(|e| e.owner().map(str::to_owned))
                .unwrap(),
            Some(profile_a.device_id.clone())
        );
        assert_eq!(lock(&b.playback).remote[&profile_a.device_id].state, state);
        assert!(
            b.status()
                .devices
                .iter()
                .any(|d| d.device_id == profile_a.device_id && d.last_seen_ms.is_some())
        );

        a.record_playback_command(
            &profile_b.device_id,
            PlaybackCommand::ActiveChanged {
                active_device_id: profile_b.device_id.clone(),
                active_device_name: "Second player".into(),
                state: state.clone(),
            },
        )
        .unwrap();
        a.sync_device(
            service_a.clone(),
            &a.active_remote_devices().unwrap()[0],
            stats_a.clone(),
        )
        .await
        .unwrap();
        let mut transferred = false;
        while let Ok(event) = receivers[1].try_recv() {
            if let AppEvent::PlaybackCommand {
                command:
                    PlaybackCommand::ActiveChanged {
                        state: received, ..
                    },
                authority,
                origin,
            } = event
            {
                assert_eq!(received, state);
                assert!(b.playback_command_is_current(&origin, &authority));
                transferred = true;
            }
        }
        assert!(transferred, "handoff must reach the player's event loop");
        assert_eq!(
            b.playback_tick(true, true).unwrap(),
            Some(profile_b.device_id.clone())
        );
        b.publish_playback(PlaybackSnapshot {
            device_id: profile_b.device_id.clone(),
            device_name: "Second player".into(),
            active: true,
            updated_at_ms: now_ms(),
            state,
            coordination: None,
        });
        b.sync_device(
            service_b.clone(),
            &b.active_remote_devices().unwrap()[0],
            stats_b.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            a.playback_tick(true, false).unwrap(),
            Some(profile_b.device_id)
        );
        // A paired peer that accepts a connection but never answers must not
        // serialize or stop subsequent polls to the responsive peer.
        let silent_dir = tempfile::tempdir().unwrap();
        let (silent, _events) = MusicDhtService::start(
            music_dht::MusicDhtConfig::builder()
                .data_dir(silent_dir.path())
                .network_id(network)
                .stream_protocol(SYNC_ALPN)
                .build()
                .unwrap(),
        )
        .await
        .unwrap();
        let _silent_acceptor = silent.stream_acceptor(SYNC_ALPN).unwrap();
        let mut silent_profile = profile_a.clone();
        silent_profile.device_id = "silent-peer".into();
        silent_profile.endpoint_id = silent.endpoint_id().to_string();
        silent_profile.endpoint_ticket = silent.ticket().await.unwrap().to_string();
        a.apply_device_profile(&silent_profile, true).unwrap();
        lock(&a.conn)
            .execute(
                "UPDATE sync_devices SET last_seen_ms = ?1 WHERE device_id = 'silent-peer'",
                [now_ms() + 1_000],
            )
            .unwrap();
        assert_eq!(
            a.active_remote_devices().unwrap()[0].device_id,
            "silent-peer"
        );
        a.claim_playback(&profile_a.device_id).unwrap();
        let poller = tokio::spawn(sync_loop(a.clone(), service_a.clone(), stats_a.clone()));
        for position in [99.0, 100.0] {
            a.playback_tick(true, true).unwrap();
            let mut next = empty_playback_state();
            next.playing = true;
            next.position_secs = position;
            a.publish_playback(PlaybackSnapshot {
                device_id: profile_a.device_id.clone(),
                device_name: "TUI".into(),
                active: true,
                updated_at_ms: now_ms(),
                state: next,
                coordination: None,
            });
            tokio::time::timeout(Duration::from_secs(6), async {
                loop {
                    if lock(&b.playback)
                        .remote
                        .get(&profile_a.device_id)
                        .is_some_and(|snapshot| snapshot.state.position_secs == position)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("a stalled peer must not delay live playback polls");
        }
        poller.abort();
        let _ = poller.await;
        silent.shutdown().await.unwrap();
        for server in servers {
            server.abort();
        }
        for (_, service, _, _) in &peers {
            service.shutdown().await.unwrap();
        }
    })
    .await
    .expect("localhost sync must complete within 30 seconds");
}

fn empty_playback_state() -> PlaybackStateWire {
    PlaybackStateWire {
        queue: vec![],
        queue_pos: 0,
        playing: false,
        paused: false,
        idle_since_ms: None,
        position_secs: 0.0,
        volume: 80,
        shuffle: false,
        repeat: PlaybackRepeat::Off,
    }
}

#[test]
fn coordination_ignores_legacy_commands_and_wrong_snapshot_sender() {
    let sync = test_sync();
    let identity = sync.ensure_identity().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    sync.set_event_tx(tx);
    let command = PlaybackCommand::SetState {
        state: empty_playback_state(),
        seek: false,
    };
    sync.apply_playback_command(&identity.device_id, &command, None, "remote", "legacy")
        .unwrap();
    assert!(rx.try_recv().is_err());
    let mut remote = Engine::new(
        "remote".into(),
        PlaybackConfig::default(),
        Default::default(),
        0,
    );
    remote.transfer("remote", 0);
    remote.set_output(true, true);
    remote.heartbeat(1);
    let snapshot = PlaybackSnapshot {
        device_id: "remote".into(),
        device_name: "Remote".into(),
        active: true,
        updated_at_ms: now_ms(),
        state: empty_playback_state(),
        coordination: Some(remote.announcement()),
    };
    sync.apply_playback_snapshot("different-sender", snapshot.clone())
        .unwrap();
    assert!(
        sync.with_playback_engine(|engine| engine.owner().is_none())
            .unwrap()
    );
    sync.apply_playback_snapshot("remote", snapshot).unwrap();
    assert_eq!(
        sync.playback_tick(true, false).unwrap().as_deref(),
        Some("remote")
    );
    sync.publish_playback(PlaybackSnapshot {
        device_id: identity.device_id,
        device_name: identity.name,
        active: false,
        updated_at_ms: now_ms(),
        state: empty_playback_state(),
        coordination: None,
    });
    let gossip = sync
        .local_playback_snapshot()
        .unwrap()
        .coordination
        .unwrap();
    assert_eq!(gossip.claim.unwrap().owner, "remote");
}

#[test]
fn checkpoint_is_not_reused_after_changing_trusted_group() {
    let sync = test_sync();
    sync.claim_playback("old-group-owner").unwrap();
    sync.set_group_id("new-test-group").unwrap();
    assert!(
        sync.with_playback_engine(|engine| engine.owner().is_none())
            .unwrap()
    );
}

#[test]
fn queued_command_loses_its_fence_when_another_owner_wins() {
    let sync = test_sync();
    let identity = sync.ensure_identity().unwrap();
    sync.claim_playback(&identity.device_id).unwrap();
    let stamp = sync
        .with_playback_engine(|engine| engine.stamp().unwrap())
        .unwrap();
    sync.apply_playback_command(
        &identity.device_id,
        &PlaybackCommand::SetState {
            state: empty_playback_state(),
            seek: false,
        },
        Some(&stamp),
        &identity.device_id,
        "queued",
    )
    .unwrap();
    assert!(sync.playback_command_is_current(&identity.device_id, &stamp));
    sync.claim_playback("new-owner").unwrap();
    assert!(!sync.playback_command_is_current(&identity.device_id, &stamp));
}

static NEXT_TEST_DB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn test_sync() -> DeviceSync {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let unique = NEXT_TEST_DB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let library_path = std::env::temp_dir().join(format!(
        "furumi-devices-test-{}-{}-{}.sqlite3",
        std::process::id(),
        now_ms(),
        unique
    ));
    let sync = DeviceSync {
        _identity_lock: None,
        conn: Arc::new(std::sync::Mutex::new(conn)),
        library: Arc::new(Library::open(&library_path).unwrap()),
        event_tx: Arc::new(std::sync::Mutex::new(None)),
        playback: Arc::new(std::sync::Mutex::new(PlaybackShared::default())),
    };
    sync.ensure_identity().unwrap();
    sync
}

#[test]
fn device_identity_has_one_coordinator_across_installation_paths() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sync.sqlite3");
    let first = acquire_identity_lock(&path).unwrap();
    assert!(acquire_identity_lock(&path).is_err());
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "devices::tests::identity_lock_child_process",
            "--nocapture",
        ])
        .env("FURUMI_IDENTITY_LOCK_TEST_PATH", &path)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    drop(first);
    // Reopening a leftover lock file after shutdown must succeed.
    assert!(acquire_identity_lock(&path).is_ok());
}

#[test]
fn identity_lock_child_process() {
    if let Some(path) = std::env::var_os("FURUMI_IDENTITY_LOCK_TEST_PATH") {
        assert!(acquire_identity_lock(std::path::Path::new(&path)).is_err());
    }
}

fn device_revoked(sync: &DeviceSync, device_id: &str) -> bool {
    let conn = lock(&sync.conn);
    conn.query_row(
        "SELECT revoked_at_ms IS NOT NULL
             FROM sync_devices
             WHERE device_id = ?1",
        [device_id],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .unwrap()
    .unwrap_or(0)
        != 0
}

fn device_known(sync: &DeviceSync, device_id: &str) -> bool {
    let conn = lock(&sync.conn);
    conn.query_row(
        "SELECT 1 FROM sync_devices WHERE device_id = ?1",
        [device_id],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .unwrap()
    .is_some()
}

fn test_fed_track(content_id: &str) -> crate::federation::FedTrack {
    crate::federation::FedTrack {
        item_id: "fed_item_1".to_string(),
        owner: "fed_owner_1".to_string(),
        own: false,
        title: "Remote Song".to_string(),
        artist_names: vec!["Remote Artist".to_string()],
        featured_artist_names: Vec::new(),
        year: Some(2026),
        duration_seconds: Some(123),
        content_id: Some(content_id.to_string()),
        release_title: Some("Remote Release".to_string()),
        track_number: Some(1),
        disc_number: Some(1),
    }
}

#[test]
fn base64url_round_trip_without_padding() {
    for input in [b"".as_slice(), b"a", b"ab", b"abc", b"abcdef"] {
        let encoded = base64url_encode(input);
        assert!(!encoded.contains('='));
        assert_eq!(base64url_decode(&encoded).unwrap(), input);
    }
}

#[test]
fn tombstone_detection() {
    assert!(
        SyncOpPayload::TrackLikeSet {
            content_id: "b3:0".into(),
            liked: false,
            fed: None,
        }
        .is_tombstone()
    );
    assert!(
        !SyncOpPayload::TrackLikeSet {
            content_id: "b3:0".into(),
            liked: true,
            fed: None,
        }
        .is_tombstone()
    );
}

#[test]
fn playback_tracks_do_not_sync_device_local_paths() {
    let source = TrackItem {
        id: 7,
        title: "Local Song".to_string(),
        track_number: Some(1),
        disc_number: Some(1),
        duration_seconds: 180.0,
        artists: vec![ArtistRef {
            id: 1,
            name: "Local Artist".to_string(),
        }],
        featured_artists: Vec::new(),
        release_id: 2,
        release_title: "Local Release".to_string(),
        release_year: Some(2026),
        file_path: r"C:\Users\me\Music\song.mp3".to_string(),
        content_id: Some(format!("b3:{}", "a".repeat(64))),
        cover_path: None,
        audio_format: Some("mp3".to_string()),
        audio_bitrate: Some(320),
        audio_sample_rate: Some(44_100),
        audio_bit_depth: None,
        file_size_bytes: Some(123_456),
        play_count: 3,
        fed: None,
    };

    let wire = PlaybackTrack::from_track(&source);
    assert!(wire.file_path.is_empty());

    let mut legacy_wire = wire.clone();
    legacy_wire.file_path = "/Users/me/Music/song.mp3".to_string();
    let restored = legacy_wire.to_track_item();
    assert!(restored.id < 0);
    assert_ne!(restored.id, source.id);
    assert!(restored.file_path.is_empty());
    assert_eq!(restored.content_id, source.content_id);
}

#[test]
fn compacted_device_revoke_removes_device_row() {
    let sync = test_sync();
    let device_id = "dev_old";

    sync.apply_device_trusted(device_id, 10).unwrap();
    assert!(device_known(&sync, device_id));

    sync.revoke_device(device_id).unwrap();
    assert!(!device_known(&sync, device_id));
}

#[test]
fn leave_group_self_revokes_then_resets_to_new_group() {
    let sync = test_sync();
    let identity = sync.ensure_identity().unwrap();
    let old_group = identity.group_id.clone();
    sync.apply_device_trusted("dev_peer", 10).unwrap();

    let op_id = sync.record_leave_group_revoke().unwrap();
    assert!(device_revoked(&sync, &identity.device_id));
    {
        let conn = lock(&sync.conn);
        let payload_json: String = conn
            .query_row(
                "SELECT payload_json FROM sync_ops WHERE op_id = ?1",
                [&op_id],
                |row| row.get(0),
            )
            .unwrap();
        let payload: SyncOpPayload = serde_json::from_str(&payload_json).unwrap();
        match payload {
            SyncOpPayload::DeviceRevoked {
                target_device_id,
                target_max_seq_seen,
            } => {
                assert_eq!(target_device_id, identity.device_id);
                assert_eq!(target_max_seq_seen, 1);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    let new_group = sync.finish_leave_group_reset().unwrap();
    assert_ne!(old_group, new_group);
    let status = sync.status();
    assert_eq!(status.group_id, new_group);
    assert_eq!(status.active_devices, 1);
    assert_eq!(status.devices.len(), 1);
    assert!(status.devices[0].is_self);
    assert!(!status.devices[0].revoked);
    assert_eq!(status.ops_total, 0);
    assert_eq!(status.outbox_ops, 0);
    assert!(!device_known(&sync, "dev_peer"));
}

#[test]
fn playback_command_is_targeted_and_deduplicated() {
    let sync = test_sync();
    let identity = sync.ensure_identity().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    sync.set_event_tx(tx);
    let command = PlaybackCommand::SetState {
        state: PlaybackStateWire {
            queue: Vec::new(),
            queue_pos: 0,
            playing: false,
            paused: false,
            idle_since_ms: None,
            position_secs: 0.0,
            volume: 42,
            shuffle: false,
            repeat: PlaybackRepeat::Off,
        },
        seek: false,
    };

    sync.claim_playback(&identity.device_id).unwrap();
    let stamp = sync
        .with_playback_engine(|engine| engine.stamp().unwrap())
        .unwrap();
    sync.apply_playback_command(
        "dev_other",
        &command,
        Some(&stamp),
        &identity.device_id,
        "op_other",
    )
    .unwrap();
    assert!(rx.try_recv().is_err());

    sync.apply_playback_command(
        &identity.device_id,
        &command,
        Some(&stamp),
        &identity.device_id,
        "op_1",
    )
    .unwrap();
    assert!(matches!(
        rx.try_recv().unwrap(),
        crate::app::event::AppEvent::PlaybackCommand { .. }
    ));

    sync.apply_playback_command(
        &identity.device_id,
        &command,
        Some(&stamp),
        &identity.device_id,
        "op_1",
    )
    .unwrap();
    assert!(rx.try_recv().is_err());
}

#[test]
fn playback_commands_are_caught_up_only_by_their_target_while_fresh() {
    let sync = test_sync();
    let command = PlaybackCommand::SetState {
        state: PlaybackStateWire {
            queue: Vec::new(),
            queue_pos: 0,
            playing: false,
            paused: false,
            idle_since_ms: None,
            position_secs: 0.0,
            volume: 42,
            shuffle: false,
            repeat: PlaybackRepeat::Off,
        },
        seek: false,
    };
    sync.claim_playback("dev_target").unwrap();
    sync.record_playback_command("dev_target", command.clone())
        .unwrap();
    sync.record_playback_command("dev_other", command).unwrap();

    let target_ops = sync.ops_for_peer("dev_target").unwrap();
    assert_eq!(
        target_ops
            .iter()
            .filter(|op| matches!(op.payload, SyncOpPayload::PlaybackCommand { .. }))
            .count(),
        1
    );
    assert!(
        sync.ops_for_peer("dev_unknown")
            .unwrap()
            .iter()
            .all(|op| !matches!(op.payload, SyncOpPayload::PlaybackCommand { .. }))
    );

    lock(&sync.conn)
        .execute(
            "UPDATE sync_ops
             SET hlc_ms = ?1
             WHERE kind = 'playback_command'",
            [now_ms().saturating_sub(PLAYBACK_COMMAND_TTL_MS + 1)],
        )
        .unwrap();
    assert!(
        sync.ops_for_peer("dev_target")
            .unwrap()
            .iter()
            .all(|op| !matches!(op.payload, SyncOpPayload::PlaybackCommand { .. }))
    );
}

#[test]
fn newer_device_trust_reactivates_revoked_device() {
    let sync = test_sync();
    let device_id = "dev_readd";

    sync.apply_device_trusted(device_id, 10).unwrap();
    assert!(!device_revoked(&sync, device_id));

    sync.apply_device_revoked(device_id, 20, "dev_owner", 0)
        .unwrap();
    assert!(device_revoked(&sync, device_id));

    sync.apply_device_trusted(device_id, 30).unwrap();
    assert!(!device_revoked(&sync, device_id));

    sync.apply_device_revoked(device_id, 25, "dev_owner", 0)
        .unwrap();
    assert!(!device_revoked(&sync, device_id));

    sync.apply_device_profile(
        &DeviceProfileWire {
            device_id: device_id.to_string(),
            name: "readded".to_string(),
            client_version: CLIENT_VERSION.to_string(),
            protocol_version: PROTOCOL_VERSION,
            endpoint_id: String::new(),
            endpoint_ticket: String::new(),
            revoked: true,
            revoke_cutoff_seq: Some(0),
            updated_at_ms: 20,
        },
        false,
    )
    .unwrap();
    assert!(!device_revoked(&sync, device_id));
}

#[test]
fn tombstone_gc_waits_for_every_active_remote_ack() {
    let sync = test_sync();
    let origin = sync.ensure_identity().unwrap().device_id;
    sync.apply_device_trusted("dev_a", 1).unwrap();
    sync.apply_device_trusted("dev_b", 1).unwrap();

    sync.record_local_op(SyncOpPayload::PlaylistDeleted {
        playlist_id: "pl_deleted".to_string(),
    })
    .unwrap();
    {
        let conn = lock(&sync.conn);
        let tombstones: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_ops WHERE tombstone = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tombstones, 1);
    }

    let ack = BTreeMap::from([(origin, 1)]);
    sync.note_peer_vector("dev_a", &ack).unwrap();
    sync.gc_tombstones().unwrap();
    {
        let conn = lock(&sync.conn);
        let tombstones: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_ops WHERE tombstone = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tombstones, 1);
    }

    sync.note_peer_vector("dev_b", &ack).unwrap();
    sync.gc_tombstones().unwrap();
    let conn = lock(&sync.conn);
    let tombstones: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sync_ops WHERE tombstone = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tombstones, 0);
}

#[test]
fn snapshot_carries_deleted_playlists_to_repair_stale_peers() {
    let source = test_sync();
    let source_playlist = source.library.create_playlist("Gone").unwrap();
    let playlist_sync_id = source
        .library
        .ensure_playlist_sync_id(source_playlist.id)
        .unwrap();
    source
        .apply_playlist_state(&playlist_sync_id, "Gone", false, 10, "dev_remote:1")
        .unwrap();
    source
        .apply_playlist_state(&playlist_sync_id, "", true, 20, "dev_remote:2")
        .unwrap();
    let snapshot = source.snapshot().unwrap();
    assert!(
        snapshot
            .deleted_playlists
            .iter()
            .any(|playlist| playlist.playlist_id == playlist_sync_id)
    );

    let peer = test_sync();
    let peer_playlist = peer
        .library
        .upsert_synced_playlist(&playlist_sync_id, "Gone")
        .unwrap();
    assert!(peer.library.playlist(peer_playlist).is_ok());

    peer.apply_snapshot(snapshot).unwrap();
    assert!(
        !peer
            .library
            .playlists()
            .unwrap()
            .iter()
            .any(|playlist| playlist.title == "Gone")
    );
}

#[test]
fn synced_fed_like_metadata_repairs_existing_like_state() {
    let sync = test_sync();
    let content_id = format!("b3:{}", "a".repeat(64));
    let fed = test_fed_track(&content_id);
    let synced = SyncedFedTrack::from_fed(&fed).unwrap();

    assert!(
        sync.apply_like_state(&content_id, true, None, 10, "dev_remote:1")
            .unwrap()
    );
    assert!(sync.library.fed_like_ids().unwrap().is_empty());

    assert!(
        sync.apply_like_state(&content_id, true, Some(&synced), 10, "dev_remote:1")
            .unwrap()
    );
    let keys = sync.library.fed_like_ids().unwrap();
    assert!(keys.contains(&fed.item_id));
    assert!(keys.contains(&content_id));

    assert!(
        sync.apply_like_state(&content_id, false, None, 11, "dev_remote:2")
            .unwrap()
    );
    assert!(sync.library.fed_like_ids().unwrap().is_empty());
}

#[test]
fn synced_fed_likes_are_ordered_by_hlc_not_receive_time() {
    let sync = test_sync();
    let old_content_id = format!("b3:{}", "c".repeat(64));
    let new_content_id = format!("b3:{}", "d".repeat(64));
    let mut old_fed = test_fed_track(&old_content_id);
    old_fed.item_id = "fed_old".to_string();
    old_fed.title = "Old Fed".to_string();
    let mut new_fed = test_fed_track(&new_content_id);
    new_fed.item_id = "fed_new".to_string();
    new_fed.title = "New Fed".to_string();

    let new_synced = SyncedFedTrack::from_fed(&new_fed).unwrap();
    let old_synced = SyncedFedTrack::from_fed(&old_fed).unwrap();
    sync.apply_like_state(&new_content_id, true, Some(&new_synced), 20, "dev_remote:2")
        .unwrap();
    sync.apply_like_state(&old_content_id, true, Some(&old_synced), 10, "dev_remote:1")
        .unwrap();

    let titles: Vec<String> = sync
        .library
        .playlist(crate::library::LIKES_PLAYLIST_ID)
        .unwrap()
        .tracks
        .into_iter()
        .map(|track| track.title)
        .collect();
    assert_eq!(titles, vec!["New Fed", "Old Fed"]);
}

#[test]
fn synced_playlist_item_metadata_creates_pending_fed_track() {
    let sync = test_sync();
    let playlist = sync.library.create_playlist("Remote Mix").unwrap();
    let playlist_sync_id = sync.library.ensure_playlist_sync_id(playlist.id).unwrap();
    let content_id = format!("b3:{}", "b".repeat(64));
    let fed = test_fed_track(&content_id);
    let synced = SyncedFedTrack::from_fed(&fed).unwrap();

    assert!(
        sync.apply_playlist_item_state(
            &playlist_sync_id,
            &content_id,
            true,
            3,
            Some(&synced),
            10,
            "dev_remote:2",
        )
        .unwrap()
    );

    let detail = sync.library.playlist(playlist.id).unwrap();
    assert_eq!(detail.tracks.len(), 1);
    assert!(detail.tracks[0].is_fed_pending());
    assert_eq!(detail.tracks[0].title, fed.title);

    let conn = lock(&sync.conn);
    assert_eq!(
        sync.unresolved_playlist_item_count_with_conn(&conn)
            .unwrap(),
        0
    );
    drop(conn);

    assert!(
        sync.apply_playlist_item_state(
            &playlist_sync_id,
            &content_id,
            false,
            0,
            None,
            11,
            "dev_remote:3",
        )
        .unwrap()
    );
    assert_eq!(sync.library.playlist(playlist.id).unwrap().tracks.len(), 0);
}

#[test]
fn stale_synced_playlist_item_metadata_repairs_pending_fed_track() {
    let sync = test_sync();
    let playlist = sync.library.create_playlist("Remote Mix").unwrap();
    let playlist_sync_id = sync.library.ensure_playlist_sync_id(playlist.id).unwrap();
    let content_id = format!("b3:{}", "e".repeat(64));
    let fed = test_fed_track(&content_id);
    let synced = SyncedFedTrack::from_fed(&fed).unwrap();

    assert!(
        sync.apply_playlist_item_state(
            &playlist_sync_id,
            &content_id,
            true,
            7,
            None,
            10,
            "dev_remote:2",
        )
        .unwrap()
    );
    assert_eq!(sync.library.playlist(playlist.id).unwrap().tracks.len(), 0);

    assert!(
        sync.apply_playlist_item_state(
            &playlist_sync_id,
            &content_id,
            true,
            7,
            Some(&synced),
            10,
            "dev_remote:2",
        )
        .unwrap()
    );
    let detail = sync.library.playlist(playlist.id).unwrap();
    assert_eq!(detail.tracks.len(), 1);
    assert!(detail.tracks[0].is_fed_pending());
    assert_eq!(detail.tracks[0].title, fed.title);
}
