use super::*;

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
        conn: Arc::new(std::sync::Mutex::new(conn)),
        library: Arc::new(Library::open(&library_path).unwrap()),
        event_tx: Arc::new(std::sync::Mutex::new(None)),
        playback: Arc::new(std::sync::Mutex::new(PlaybackShared::default())),
    };
    sync.ensure_identity().unwrap();
    sync
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

    sync.apply_playback_command("dev_other", &command, "op_other")
        .unwrap();
    assert!(rx.try_recv().is_err());

    sync.apply_playback_command(&identity.device_id, &command, "op_1")
        .unwrap();
    assert!(matches!(
        rx.try_recv().unwrap(),
        crate::app::event::AppEvent::PlaybackCommand(_)
    ));

    sync.apply_playback_command(&identity.device_id, &command, "op_1")
        .unwrap();
    assert!(rx.try_recv().is_err());
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
