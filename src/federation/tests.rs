use super::*;

fn test_owner() -> EndpointId {
    music_dht::SecretKey::from_bytes(&[7; 32]).public()
}

fn dht_track(main: &[&str], featured: &[&str]) -> LibraryItem {
    let owner = test_owner();
    LibraryItem {
        id: music_dht::ItemId::derive(&owner, ItemKind::Track, "track:1"),
        owner,
        kind: ItemKind::Track,
        name: "Guest Verse".into(),
        normalized_name: music_dht::normalize_name("Guest Verse"),
        artist_names: main.iter().map(|name| name.to_string()).collect(),
        featured_artist_names: featured.iter().map(|name| name.to_string()).collect(),
        year: Some(2024),
        release_type: Some("album".into()),
        release_title: Some("Host Album".into()),
        track_number: Some(2),
        disc_number: Some(1),
        duration_seconds: Some(180.0),
        content_id: Some(
            "b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        ),
        revision: 1,
        deleted: false,
        updated_at_ms: 0,
    }
}

#[test]
fn dht_appearance_requires_explicit_featured_artist() {
    let normalized = music_dht::normalize_name("Guest");
    assert!(dht_appearance_hit(&dht_track(&["Guest"], &[]), &normalized, "Guest").is_none());

    let hit = dht_appearance_hit(&dht_track(&["Host"], &["Guest"]), &normalized, "Guest").unwrap();
    assert_eq!(hit.release_title, "Host Album");
    assert_eq!(hit.release_type, "album");
    assert_eq!(hit.year, Some(2024));
    assert_eq!(hit.track.artists, vec!["Host"]);
    assert_eq!(hit.track.featured_artists, vec!["Guest"]);
    assert_eq!(hit.track.track_number, Some(2));
    assert_eq!(hit.track.disc_number, Some(1));
}

#[test]
fn federation_search_ranks_exact_names_first() {
    let normalized = music_dht::normalize_name("ежемесячные");
    let mut artists = vec![
        FedArtistHit {
            name: "Booker".into(),
            peers: 3,
        },
        FedArtistHit {
            name: "Ежемесячные".into(),
            peers: 1,
        },
    ];
    let mut tracks = vec![
        FedTrack {
            item_id: "a".into(),
            owner: "peer-a".into(),
            own: false,
            title: "Гость".into(),
            artist_names: vec!["Other".into()],
            featured_artist_names: vec!["Ежемесячные".into()],
            year: None,
            duration_seconds: None,
            content_id: None,
            release_title: None,
            track_number: None,
            disc_number: None,
        },
        FedTrack {
            item_id: "b".into(),
            owner: "peer-b".into(),
            own: false,
            title: "Ежемесячные".into(),
            artist_names: vec!["Other".into()],
            featured_artist_names: Vec::new(),
            year: None,
            duration_seconds: None,
            content_id: None,
            release_title: None,
            track_number: None,
            disc_number: None,
        },
    ];

    rank_fed_search_results(&mut artists, &mut tracks, &normalized);

    assert_eq!(artists[0].name, "Ежемесячные");
    assert_eq!(tracks[0].title, "Ежемесячные");
}
