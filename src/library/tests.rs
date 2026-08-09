use super::*;

fn test_library() -> Library {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    register_norm_function(&conn).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    Library {
        conn: Mutex::new(conn),
        db_path: std::env::temp_dir().join("furumi-test-library.db"),
        covers_dir: std::env::temp_dir().join("furumi-test-covers-unused"),
    }
}

fn add_track(lib: &Library, title: &str, artist: &str, album: &str) -> i64 {
    add_track_with_featured(lib, title, artist, &[], album)
}

fn add_track_with_featured(
    lib: &Library,
    title: &str,
    artist: &str,
    featured: &[&str],
    album: &str,
) -> i64 {
    let import = import::TrackImport {
        release_type: None,
        file_path: format!("/music/{artist}/{album}/{title}.mp3"),
        title: title.to_string(),
        artists: vec![artist.to_string()],
        featured_artists: featured.iter().map(|name| (*name).to_string()).collect(),
        album_artists: vec![artist.to_string()],
        release_title: album.to_string(),
        year: Some(2020),
        track_number: None,
        disc_number: None,
        duration_seconds: 60.0,
        audio_format: Some("mp3".into()),
        audio_bitrate: Some(320),
        audio_sample_rate: Some(44100),
        audio_bit_depth: None,
        file_size_bytes: Some(1),
        cover: None,
    };
    let id = import::upsert_track(lib, &import).unwrap().0;
    let content_id = format!("b3:{}", blake3::hash(import.file_path.as_bytes()).to_hex());
    lib.lock()
        .execute(
            "UPDATE tracks SET content_id = ?2 WHERE id = ?1",
            params![id, content_id],
        )
        .unwrap();
    id
}

fn artist_filters(hide_featured_only: bool) -> crate::config::settings::LibraryFilters {
    crate::config::settings::LibraryFilters {
        hide_featured_only,
        ..Default::default()
    }
}

fn unique_test_dir(label: &str) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("furumi-{label}-{}-{unique}", std::process::id()))
}

#[test]
fn managed_music_relocation_builds_artist_release_tree_and_updates_paths() {
    let root = unique_test_dir("music-relocation");
    let old = root.join("old");
    let new = root.join("new");
    let covers = root.join("covers");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::create_dir_all(&covers).unwrap();
    let audio = old.join("legacy.flac");
    let cover = covers.join("release.jpg");
    let artist_image = covers.join("artist.png");
    std::fs::write(&audio, b"audio").unwrap();
    std::fs::write(&cover, b"cover").unwrap();
    std::fs::write(&artist_image, b"artist").unwrap();

    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    register_norm_function(&conn).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    let library = Library {
        conn: Mutex::new(conn),
        db_path: root.join("library.db"),
        covers_dir: covers,
    };
    let track_id = import::upsert_track(
        &library,
        &import::TrackImport {
            file_path: audio.to_string_lossy().into_owned(),
            title: "Song".into(),
            artists: vec!["Artist".into()],
            featured_artists: vec![],
            album_artists: vec!["Artist".into()],
            release_title: "Release".into(),
            release_type: Some("album".into()),
            year: Some(2026),
            track_number: Some(1),
            disc_number: Some(1),
            duration_seconds: 1.0,
            audio_format: Some("flac".into()),
            audio_bitrate: None,
            audio_sample_rate: None,
            audio_bit_depth: None,
            file_size_bytes: Some(5),
            cover: None,
        },
    )
    .unwrap()
    .0;
    let (release_id, artist_id): (i64, i64) = library
        .lock()
        .query_row(
            "SELECT t.release_id, ta.artist_id
             FROM tracks t JOIN track_artists ta ON ta.track_id = t.id
             WHERE t.id = ?1 AND ta.role = 'main'",
            [track_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    library
        .lock()
        .execute(
            "UPDATE releases SET cover_path = ?2 WHERE id = ?1",
            params![release_id, cover.to_string_lossy()],
        )
        .unwrap();
    library
        .lock()
        .execute(
            "UPDATE artists SET image_path = ?2 WHERE id = ?1",
            params![artist_id, artist_image.to_string_lossy()],
        )
        .unwrap();

    let stats = library.relocate_managed_music(&old, &new).unwrap();
    assert_eq!(stats.tracks, 1);
    assert_eq!(stats.images, 2);
    let track = library.tracks_by_ids(&[track_id]).unwrap().remove(0);
    assert_eq!(
        track.file_path,
        new.join("Artist/Release/legacy.flac").to_string_lossy()
    );
    assert_eq!(
        track.cover_path.as_deref(),
        Some(
            new.join("Artist/Release/cover.jpg")
                .to_string_lossy()
                .as_ref()
        )
    );
    let image: String = library
        .lock()
        .query_row(
            "SELECT image_path FROM artists WHERE id = ?1",
            [artist_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(image, new.join("Artist/artist.png").to_string_lossy());
    assert!(!audio.exists());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn music_directory_validation_rejects_a_file_without_touching_it() {
    let root = unique_test_dir("music-validation");
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("not-a-directory");
    std::fs::write(&file, b"keep").unwrap();

    assert!(Library::validate_music_directory(&file).is_err());
    assert_eq!(std::fs::read(&file).unwrap(), b"keep");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn managed_music_names_are_portable_to_windows() {
    assert_eq!(storage_name("Artist/Name", "fallback"), "Artist_Name");
    assert_eq!(storage_name("CON", "fallback"), "_CON");
    assert_eq!(storage_name("lpt9.live", "fallback"), "_lpt9.live");
    assert_eq!(storage_name("...", "fallback"), "fallback");
}

#[test]
fn local_stats_counts_library_rows_and_audio_bytes() {
    let lib = test_library();
    add_track(&lib, "One", "Artist", "First");
    add_track(&lib, "Two", "Artist", "Second");

    let stats = lib.local_stats().unwrap();
    assert_eq!(stats.artist_count, 1);
    assert_eq!(stats.release_count, 2);
    assert_eq!(stats.track_count, 2);
    assert_eq!(stats.audio_bytes, 2);
    assert_eq!(stats.tracks_without_size, 0);
}

#[test]
fn artists_page_prioritizes_releases_then_tracks() {
    let lib = test_library();
    add_track(&lib, "Solo", "Zed", "Zed Album");
    add_track_with_featured(&lib, "Guest One", "A Host", &["Guest"], "A Host Album");
    add_track_with_featured(&lib, "Guest Two", "B Host", &["Guest"], "B Host Album");

    let page = lib.artists(1, 10, artist_filters(false)).unwrap();
    let zed_pos = page
        .items
        .iter()
        .position(|artist| artist.name == "Zed")
        .unwrap();
    let guest_pos = page
        .items
        .iter()
        .position(|artist| artist.name == "Guest")
        .unwrap();
    let guest = &page.items[guest_pos];

    assert_eq!(guest.release_count, 0);
    assert_eq!(guest.track_count, 2);
    assert!(zed_pos < guest_pos);

    let filtered = lib.artists(1, 10, artist_filters(true)).unwrap();
    assert!(filtered.items.iter().all(|artist| artist.release_count > 0));
    assert!(!filtered.items.iter().any(|artist| artist.name == "Guest"));
}

#[test]
fn network_artist_image_hint_becomes_local_image_after_fetch() {
    let lib = test_library();
    let artist_key = music_dht::normalize_name("Remote Artist");
    lib.replace_network_artist_cache(
        "peer-a",
        "personal",
        &[NetworkArtistPreview {
            artist_key: artist_key.clone(),
            name: "Remote Artist".into(),
            image_path: Some("peer-local/image.jpg".into()),
            release_count: 1,
            track_count: 3,
        }],
        true,
    )
    .unwrap();

    let filters = crate::config::settings::LibraryFilters {
        source_mode: crate::config::settings::LibrarySourceMode::My,
        ..Default::default()
    };
    let page = lib.artists(1, 10, filters).unwrap();
    assert_eq!(page.items[0].image_path, None);

    let requests = lib
        .network_artist_image_requests(filters, &["Remote Artist".into()], 8)
        .unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].source_id, "peer-a");
    assert_eq!(requests[0].artist_key, artist_key);

    lib.set_network_artist_image("peer-a", &artist_key, "/tmp/remote-artist.jpg")
        .unwrap();
    let page = lib.artists(1, 10, filters).unwrap();
    assert_eq!(
        page.items[0].image_path.as_deref(),
        Some("/tmp/remote-artist.jpg")
    );
}

#[test]
fn import_creates_artist_release_track() {
    let lib = test_library();
    let track_id = add_track(&lib, "Song", "Artist", "Album");
    let page = lib.artists(1, 10, artist_filters(false)).unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].name, "Artist");
    assert_eq!(page.items[0].track_count, 1);

    let detail = lib.artist(page.items[0].id).unwrap();
    assert_eq!(detail.releases.len(), 1);
    assert_eq!(detail.top_tracks.len(), 1);

    let release = lib.release(detail.releases[0].id).unwrap();
    assert_eq!(release.tracks.len(), 1);
    assert_eq!(release.tracks[0].id, track_id);
    assert_eq!(release.tracks[0].artists[0].name, "Artist");
}

#[test]
fn reimport_updates_instead_of_duplicating() {
    let lib = test_library();
    let first = add_track(&lib, "Song", "Artist", "Album");
    let second = add_track(&lib, "Song", "Artist", "Album");
    assert_eq!(first, second);
    let page = lib.artists(1, 10, artist_filters(false)).unwrap();
    assert_eq!(page.items[0].track_count, 1);
}

#[test]
fn content_id_backfill_hashes_missing_track_ids() {
    let lib = test_library();
    let path = std::env::temp_dir().join(format!(
        "furumi-content-id-test-{}-{}.bin",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, b"portable content id").unwrap();
    let file_path = path.to_string_lossy().into_owned();
    let import = import::TrackImport {
        release_type: None,
        file_path: file_path.clone(),
        title: "Portable".to_string(),
        artists: vec!["Artist".to_string()],
        featured_artists: Vec::new(),
        album_artists: vec!["Artist".to_string()],
        release_title: "Album".to_string(),
        year: Some(2026),
        track_number: None,
        disc_number: None,
        duration_seconds: 60.0,
        audio_format: Some("bin".into()),
        audio_bitrate: None,
        audio_sample_rate: None,
        audio_bit_depth: None,
        file_size_bytes: Some(19),
        cover: None,
    };
    let track_id = import::upsert_track(&lib, &import).unwrap().0;
    let expected = audio_content_id(&file_path).unwrap();
    {
        let conn = lib.lock();
        conn.execute(
            "UPDATE tracks SET content_id = NULL WHERE id = ?1",
            [track_id],
        )
        .unwrap();
    }

    let stats = lib.backfill_missing_content_ids().unwrap();
    assert_eq!(stats.hashed, 1);
    assert_eq!(stats.updated(), 1);
    assert_eq!(
        lib.track_content_id_by_id(track_id).unwrap().as_deref(),
        Some(expected.as_str())
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn search_finds_all_kinds() {
    let lib = test_library();
    add_track(&lib, "Neon Lights", "Neon Artist", "Neon Album");
    let results = lib.search("neon", 10).unwrap();
    assert_eq!(results.artists.len(), 1);
    assert_eq!(results.releases.len(), 1);
    assert_eq!(results.tracks.len(), 1);
    // LIKE wildcards in the query must not match everything.
    assert_eq!(lib.search("%", 10).unwrap().len(), 0);
}

#[test]
fn search_ranks_exact_names_first() {
    let lib = test_library();
    add_track(&lib, "A Needle", "A Needle Artist", "A Needle Album");
    add_track(&lib, "Needle", "Needle", "Needle");

    let results = lib.search("needle", 10).unwrap();
    assert_eq!(results.artists[0].name, "Needle");
    assert_eq!(results.releases[0].title, "Needle");
    assert_eq!(results.tracks[0].title, "Needle");
}

#[test]
fn search_folds_case_beyond_ascii() {
    let lib = test_library();
    add_track(&lib, "Nothing Else Matters", "Металлика", "Чёрный альбом");
    // SQLite's LIKE/NOCASE only fold ASCII; norm() folds every script.
    assert_eq!(lib.search("металлика", 10).unwrap().artists.len(), 1);
    assert_eq!(lib.search("МЕТАЛЛИКА", 10).unwrap().artists.len(), 1);
    assert_eq!(lib.search("чёрный", 10).unwrap().releases.len(), 1);
    assert_eq!(lib.search("matters", 10).unwrap().tracks.len(), 1);
}

#[test]
fn playlists_and_likes_round_trip() {
    let lib = test_library();
    let track_id = add_track(&lib, "Song", "Artist", "Album");
    let playlist = lib.create_playlist("Mix").unwrap();
    lib.add_tracks_to_playlist(playlist.id, &[track_id])
        .unwrap();
    assert_eq!(lib.playlist(playlist.id).unwrap().tracks.len(), 1);

    let content_id = lib.track_content_id_by_id(track_id).unwrap().unwrap();
    assert!(lib.toggle_like_by_content_id(&content_id).unwrap());
    assert_eq!(lib.liked_content_ids().unwrap(), vec![content_id.clone()]);
    assert_eq!(lib.playlist(LIKES_PLAYLIST_ID).unwrap().tracks.len(), 1);
    assert!(!lib.toggle_like_by_content_id(&content_id).unwrap());

    lib.remove_tracks_from_playlist(playlist.id, &[track_id])
        .unwrap();
    assert_eq!(lib.playlist(playlist.id).unwrap().tracks.len(), 0);
    lib.delete_playlist(playlist.id).unwrap();
    // Only the virtual Likes playlist remains.
    assert_eq!(lib.playlists().unwrap().len(), 1);
}

#[test]
fn likes_playlist_orders_local_and_federated_by_liked_at() {
    let lib = test_library();
    let old_id = add_track(&lib, "Old Local", "Artist", "Album");
    let new_id = add_track(&lib, "New Local", "Artist", "Album");
    let old_content_id = lib.track_content_id_by_id(old_id).unwrap().unwrap();
    let new_content_id = lib.track_content_id_by_id(new_id).unwrap().unwrap();
    let content_id = format!("b3:{}", "c".repeat(64));
    let fed = crate::federation::FedTrack {
        item_id: "fed_item_order".to_string(),
        owner: "fed_owner_order".to_string(),
        own: false,
        title: "Middle Fed".to_string(),
        artist_names: vec!["Remote Artist".to_string()],
        featured_artist_names: Vec::new(),
        year: Some(2026),
        duration_seconds: Some(123),
        content_id: Some(content_id),
        release_title: Some("Remote Release".to_string()),
        track_number: Some(1),
        disc_number: Some(1),
    };

    assert!(lib.toggle_like_by_content_id(&old_content_id).unwrap());
    assert!(lib.toggle_like_by_content_id(&new_content_id).unwrap());
    assert!(lib.toggle_fed_like(&fed).unwrap());
    {
        let conn = lib.lock();
        conn.execute(
            "UPDATE likes SET liked_at = ?2 WHERE track_id = ?1",
            params![old_id, "2026-01-01 00:00:00"],
        )
        .unwrap();
        conn.execute(
            "UPDATE likes SET liked_at = ?2 WHERE track_id = ?1",
            params![new_id, "2026-01-02 00:00:00"],
        )
        .unwrap();
        conn.execute(
            "UPDATE fed_likes SET liked_at = ?2 WHERE item_id = ?1",
            params![fed.item_id, "2026-01-03 00:00:00"],
        )
        .unwrap();
    }

    let titles: Vec<String> = lib
        .playlist(LIKES_PLAYLIST_ID)
        .unwrap()
        .tracks
        .into_iter()
        .map(|track| track.title)
        .collect();
    assert_eq!(titles, vec!["Middle Fed", "New Local", "Old Local"]);

    assert!(!lib.toggle_like_by_content_id(&old_content_id).unwrap());
    assert!(lib.toggle_like_by_content_id(&old_content_id).unwrap());
    {
        let conn = lib.lock();
        conn.execute(
            "UPDATE likes SET liked_at = ?2 WHERE track_id = ?1",
            params![old_id, "2026-01-04 00:00:00"],
        )
        .unwrap();
    }
    let titles: Vec<String> = lib
        .playlist(LIKES_PLAYLIST_ID)
        .unwrap()
        .tracks
        .into_iter()
        .map(|track| track.title)
        .collect();
    assert_eq!(titles, vec!["Old Local", "Middle Fed", "New Local"]);
}

#[test]
fn synced_playlist_can_show_federated_pending_tracks() {
    let lib = test_library();
    let playlist = lib.create_playlist("Remote Mix").unwrap();
    let sync_id = lib.ensure_playlist_sync_id(playlist.id).unwrap();
    let content_id = format!("b3:{}", "a".repeat(64));
    let fed = crate::federation::FedTrack {
        item_id: "fed_item_1".to_string(),
        owner: "fed_owner_1".to_string(),
        own: false,
        title: "Remote Song".to_string(),
        artist_names: vec!["Remote Artist".to_string()],
        featured_artist_names: vec!["Remote Guest".to_string()],
        year: Some(2026),
        duration_seconds: Some(123),
        content_id: Some(content_id.clone()),
        release_title: Some("Remote Release".to_string()),
        track_number: Some(2),
        disc_number: Some(1),
    };

    assert!(lib.upsert_fed_playlist_track(&sync_id, &fed, 4).unwrap());
    assert!(
        lib.has_playlist_content_reference(&sync_id, &content_id)
            .unwrap()
    );

    let detail = lib.playlist(playlist.id).unwrap();
    assert_eq!(detail.tracks.len(), 1);
    let track = &detail.tracks[0];
    assert!(track.is_fed_pending());
    assert_eq!(track.title, "Remote Song");
    assert_eq!(track.artist_line(), "Remote Artist feat. Remote Guest");
    assert_eq!(track.release_title, "Remote Release");
    assert_eq!(track.content_id.as_deref(), Some(content_id.as_str()));

    let card = lib
        .playlists()
        .unwrap()
        .into_iter()
        .find(|card| card.id == playlist.id)
        .unwrap();
    assert_eq!(card.track_count, 1);

    lib.remove_content_ids_from_playlist(playlist.id, std::slice::from_ref(&content_id))
        .unwrap();
    assert_eq!(lib.playlist(playlist.id).unwrap().tracks.len(), 0);
    assert!(
        lib.fed_playlist_track_by_content_id(&sync_id, &content_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn add_federated_pending_track_to_playlist_records_position() {
    let lib = test_library();
    let local_id = add_track(&lib, "Local Song", "Artist", "Album");
    let playlist = lib.create_playlist("Remote Mix").unwrap();
    let content_id = format!("b3:{}", "b".repeat(64));
    let fed = crate::federation::FedTrack {
        item_id: "fed_item_2".to_string(),
        owner: "fed_owner_2".to_string(),
        own: false,
        title: "Remote Song".to_string(),
        artist_names: vec!["Remote Artist".to_string()],
        featured_artist_names: Vec::new(),
        year: Some(2026),
        duration_seconds: Some(123),
        content_id: Some(content_id.clone()),
        release_title: Some("Remote Release".to_string()),
        track_number: Some(2),
        disc_number: Some(1),
    };

    lib.add_tracks_to_playlist(playlist.id, &[local_id])
        .unwrap();
    lib.add_fed_tracks_to_playlist(playlist.id, std::slice::from_ref(&fed))
        .unwrap();

    let position = lib
        .playlist_content_position(playlist.id, &content_id)
        .unwrap();
    assert_eq!(position, Some(1));
    let detail = lib.playlist(playlist.id).unwrap();
    assert_eq!(
        detail
            .tracks
            .into_iter()
            .map(|track| track.title)
            .collect::<Vec<_>>(),
        vec!["Local Song", "Remote Song"]
    );
}

#[test]
fn track_edit_relinks_artists() {
    let lib = test_library();
    let track_id = add_track(&lib, "Song", "Artist", "Album");
    lib.update_track(
        track_id,
        &TrackEdit {
            title: "Renamed".into(),
            artists: vec!["Other".into()],
            featured_artists: vec!["Guest".into()],
            track_number: Some(2),
            disc_number: None,
            cover_path: None,
        },
    )
    .unwrap();
    let track = lib.tracks_by_ids(&[track_id]).unwrap().remove(0);
    assert_eq!(track.title, "Renamed");
    assert_eq!(track.artists[0].name, "Other");
    assert_eq!(track.featured_artists[0].name, "Guest");
    assert_eq!(track.track_number, Some(2));
}

#[test]
fn deleting_artist_cleans_up_own_content() {
    let lib = test_library();
    add_track(&lib, "Song", "Solo", "Solo Album");
    let page = lib.artists(1, 10, artist_filters(false)).unwrap();
    lib.delete_artist(page.items[0].id).unwrap();
    assert_eq!(lib.artists(1, 10, artist_filters(false)).unwrap().total, 0);
    assert_eq!(lib.search("Song", 10).unwrap().len(), 0);
}

#[test]
fn delete_track_drops_empty_release() {
    let lib = test_library();
    let track_id = add_track(&lib, "Only", "Artist", "Album");
    lib.delete_track(track_id).unwrap();
    let detail = lib
        .artist(lib.artists(1, 10, artist_filters(false)).unwrap().items[0].id)
        .unwrap();
    assert!(detail.releases.is_empty());
}

#[test]
fn history_counts_completed_plays() {
    let lib = test_library();
    let track_id = add_track(&lib, "Song", "Artist", "Album");
    let content_id = lib
        .tracks_by_ids(&[track_id])
        .unwrap()
        .remove(0)
        .content_id
        .unwrap();
    let event = music_dht::device_sync::ListenEvent {
        listen_id: "listen-1".to_string(),
        content_id,
        started_at_ms: 1_700_000_000_000,
        listened_ms: 60_000,
        track_duration_ms: Some(60_000),
        ended_reason: music_dht::device_sync::ListenEndReason::Finished,
        track: music_dht::device_sync::ListenTrackMetadata {
            title: "Song".to_string(),
            artist_names: vec!["Artist".to_string()],
            featured_artist_names: Vec::new(),
            release_title: Some("Album".to_string()),
        },
    };
    assert!(lib.apply_listen_event(&event, "device-a").unwrap());
    assert!(!lib.apply_listen_event(&event, "device-a").unwrap());
    let track = lib.tracks_by_ids(&[track_id]).unwrap().remove(0);
    assert_eq!(track.play_count, 1);
    let history = lib.listen_history(20).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].listen_id, "listen-1");
    assert_eq!(history[0].title, "Song");
    assert_eq!(history[0].artist, "Artist");
    assert_eq!(history[0].origin_device_id, "device-a");
}

#[test]
fn listen_history_hides_unqualified_events_and_keeps_remote_metadata() {
    let lib = test_library();
    let event = music_dht::device_sync::ListenEvent {
        listen_id: "remote-listen".to_string(),
        content_id: format!("b3:{}", "a".repeat(64)),
        started_at_ms: 1_700_000_000_000,
        listened_ms: 10_000,
        track_duration_ms: Some(120_000),
        ended_reason: music_dht::device_sync::ListenEndReason::Skipped,
        track: music_dht::device_sync::ListenTrackMetadata {
            title: "Remote song".to_string(),
            artist_names: vec!["Remote artist".to_string()],
            featured_artist_names: vec!["Guest".to_string()],
            release_title: None,
        },
    };
    assert!(lib.apply_listen_event(&event, "remote-device").unwrap());
    assert!(lib.listen_history(20).unwrap().is_empty());
}

#[test]
fn similarity_embeddings_round_trip_and_keep_profiles_separate() {
    let lib = test_library();
    let track_id = add_track(&lib, "Song", "Artist", "Album");
    for profile in ["profile-a", "profile-b"] {
        lib.ensure_similarity_profile(profile, "model", "1", "sha", "prep", 3)
            .unwrap();
    }
    let track = lib
        .pending_similarity_tracks("profile-a")
        .unwrap()
        .into_iter()
        .find(|track| track.id == track_id)
        .unwrap();
    lib.store_similarity_embedding(&track, "profile-a", &[0.1, 0.2, 0.3])
        .unwrap();
    lib.store_similarity_embedding(&track, "profile-b", &[0.3, 0.2, 0.1])
        .unwrap();

    assert_eq!(
        lib.similarity_embedding(track_id, "profile-a").unwrap(),
        Some(vec![0.1, 0.2, 0.3])
    );
    assert_eq!(
        lib.similarity_embedding(track_id, "profile-b").unwrap(),
        Some(vec![0.3, 0.2, 0.1])
    );
    let stats = lib.similarity_storage_stats("profile-a").unwrap();
    assert_eq!(stats.total_tracks, 1);
    assert_eq!(stats.embedded_tracks, 1);
    assert_eq!(stats.stored_vectors, 2);
    assert_eq!(stats.stored_bytes, 24);
}

#[test]
fn changed_content_id_invalidates_only_the_stale_embedding() {
    let lib = test_library();
    let track_id = add_track(&lib, "Song", "Artist", "Album");
    lib.ensure_similarity_profile("profile", "model", "1", "sha", "prep", 2)
        .unwrap();
    let track = lib.pending_similarity_tracks("profile").unwrap().remove(0);
    lib.store_similarity_embedding(&track, "profile", &[0.6, 0.8])
        .unwrap();
    lib.lock()
        .execute(
            "UPDATE tracks SET content_id = ?2 WHERE id = ?1",
            params![track_id, format!("b3:{}", "f".repeat(64))],
        )
        .unwrap();

    assert_eq!(lib.pending_similarity_tracks("profile").unwrap().len(), 1);
    assert_eq!(lib.similarity_embedding(track_id, "profile").unwrap(), None);
    assert!(lib.load_similarity_index("profile").unwrap().is_empty());
}

#[test]
fn clearing_embeddings_preserves_the_library() {
    let lib = test_library();
    let track_id = add_track(&lib, "Song", "Artist", "Album");
    lib.ensure_similarity_profile("profile", "model", "1", "sha", "prep", 2)
        .unwrap();
    let track = lib.pending_similarity_tracks("profile").unwrap().remove(0);
    lib.store_similarity_embedding(&track, "profile", &[0.6, 0.8])
        .unwrap();

    lib.clear_similarity_embeddings().unwrap();

    assert_eq!(lib.tracks_by_ids(&[track_id]).unwrap().len(), 1);
    assert!(
        lib.similarity_embedding(track_id, "profile")
            .unwrap()
            .is_none()
    );
}
