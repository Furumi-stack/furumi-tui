//! Golden wire-compatibility tests: the shared `music_dht::device_sync`
//! types this client now uses must serialize v3 content byte-for-byte like
//! the private copies of furumi-tui 0.3.x (kept verbatim in `legacy`
//! below), so 0.3.x clients and furumi-web 0.12.x keep interoperating.

use super::*;

/// Private wire types of furumi-tui 0.3.x (`src/devices.rs` before sync
/// v4), copied verbatim apart from visibility, `PartialEq` and doc comments.
#[allow(dead_code)]
pub(super) mod legacy {
    use music_dht::device_sync::ListenEvent;
    use music_dht::playback::CommandStamp;
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct InviteWire {
        pub v: u16,
        #[serde(rename = "t")]
        pub ticket: String,
        #[serde(rename = "d")]
        pub device_id: String,
        #[serde(rename = "i")]
        pub invite_id: String,
        #[serde(rename = "s")]
        pub secret: String,
        #[serde(rename = "e")]
        pub expires_at_ms: i64,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct DeviceProfileWire {
        pub device_id: String,
        pub name: String,
        pub client_version: String,
        pub protocol_version: u16,
        pub endpoint_id: String,
        pub endpoint_ticket: String,
        #[serde(default)]
        pub revoked: bool,
        #[serde(default)]
        pub revoke_cutoff_seq: Option<i64>,
        #[serde(default)]
        pub updated_at_ms: i64,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SyncedFedTrack {
        pub item_id: String,
        pub owner: String,
        pub title: String,
        #[serde(default)]
        pub artist_names: Vec<String>,
        #[serde(default)]
        pub featured_artist_names: Vec<String>,
        pub year: Option<i32>,
        pub duration_seconds: Option<i64>,
        pub content_id: String,
        pub release_title: Option<String>,
        pub track_number: Option<i32>,
        pub disc_number: Option<i32>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct PlaybackTrack {
        pub id: i64,
        pub title: String,
        pub track_number: Option<i32>,
        pub disc_number: Option<i32>,
        pub duration_seconds: f64,
        #[serde(default)]
        pub artist_names: Vec<String>,
        #[serde(default)]
        pub featured_artist_names: Vec<String>,
        pub release_id: i64,
        pub release_title: String,
        pub release_year: Option<i32>,
        #[serde(default)]
        pub file_path: String,
        pub content_id: Option<String>,
        pub audio_format: Option<String>,
        pub audio_bitrate: Option<i32>,
        pub audio_sample_rate: Option<i32>,
        pub audio_bit_depth: Option<i32>,
        pub file_size_bytes: Option<i64>,
        #[serde(default)]
        pub play_count: i64,
        #[serde(default)]
        pub fed: Option<SyncedFedTrack>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
    #[serde(rename_all = "snake_case")]
    pub enum PlaybackRepeat {
        #[default]
        Off,
        One,
        All,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct PlaybackStateWire {
        #[serde(default)]
        pub queue: Vec<PlaybackTrack>,
        #[serde(default)]
        pub queue_pos: usize,
        pub playing: bool,
        pub paused: bool,
        #[serde(default)]
        pub idle_since_ms: Option<i64>,
        pub position_secs: f64,
        #[serde(default)]
        pub volume: u8,
        pub shuffle: bool,
        pub repeat: PlaybackRepeat,
    }

    pub type PlaybackSnapshot = music_dht::playback::Snapshot<PlaybackStateWire>;
    pub type PlaybackCommand = music_dht::playback::Command<PlaybackStateWire>;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct SyncOpWire {
        pub op_id: String,
        pub origin_device_id: String,
        pub seq: i64,
        pub hlc_ms: i64,
        pub payload: SyncOpPayload,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum SyncOpPayload {
        TrackLikeSet {
            content_id: String,
            liked: bool,
            #[serde(default)]
            fed: Option<SyncedFedTrack>,
        },
        PlaylistCreated {
            playlist_id: String,
            title: String,
        },
        PlaylistRenamed {
            playlist_id: String,
            title: String,
        },
        PlaylistDeleted {
            playlist_id: String,
        },
        PlaylistTrackAdded {
            playlist_id: String,
            content_id: String,
            position: i64,
            #[serde(default)]
            fed: Option<SyncedFedTrack>,
        },
        PlaylistTrackRemoved {
            playlist_id: String,
            content_id: String,
        },
        DeviceProfileSet {
            name: String,
            client_version: String,
            endpoint_ticket: String,
            endpoint_id: String,
        },
        DeviceTrusted {
            target_device_id: String,
        },
        DeviceRevoked {
            target_device_id: String,
            target_max_seq_seen: i64,
        },
        PlaybackCommand {
            target_device_id: String,
            command: PlaybackCommand,
            #[serde(default)]
            authority: Option<CommandStamp>,
        },
        ListenRecorded {
            event: ListenEvent,
        },
    }

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct SyncSnapshot {
        #[serde(default)]
        pub likes: Vec<SnapshotLike>,
        #[serde(default)]
        pub unlikes: Vec<SnapshotLikeTombstone>,
        #[serde(default)]
        pub playlists: Vec<SnapshotPlaylist>,
        #[serde(default)]
        pub deleted_playlists: Vec<SnapshotPlaylistTombstone>,
        #[serde(default)]
        pub removed_playlist_items: Vec<SnapshotPlaylistItemTombstone>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct SnapshotLike {
        pub content_id: String,
        pub hlc_ms: i64,
        pub op_id: String,
        #[serde(default)]
        pub fed: Option<SyncedFedTrack>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct SnapshotLikeTombstone {
        pub content_id: String,
        pub hlc_ms: i64,
        pub op_id: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct SnapshotPlaylist {
        pub playlist_id: String,
        pub title: String,
        pub hlc_ms: i64,
        pub op_id: String,
        #[serde(default)]
        pub items: Vec<SnapshotPlaylistItem>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct SnapshotPlaylistTombstone {
        pub playlist_id: String,
        pub hlc_ms: i64,
        pub op_id: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct SnapshotPlaylistItem {
        pub content_id: String,
        pub position: i64,
        pub hlc_ms: i64,
        pub op_id: String,
        #[serde(default)]
        pub fed: Option<SyncedFedTrack>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct SnapshotPlaylistItemTombstone {
        pub playlist_id: String,
        pub content_id: String,
        pub hlc_ms: i64,
        pub op_id: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum WireMessage {
        PairRequest {
            invite_id: String,
            secret: String,
            profile: DeviceProfileWire,
            #[serde(default)]
            group_id: Option<String>,
            #[serde(default)]
            group_active_devices: usize,
            #[serde(default)]
            devices: Vec<DeviceProfileWire>,
            vector: BTreeMap<String, i64>,
            ops: Vec<SyncOpWire>,
            snapshot: SyncSnapshot,
            #[serde(default)]
            playback: Option<PlaybackSnapshot>,
        },
        PairResponse {
            accepted: bool,
            #[serde(default)]
            pending: bool,
            #[serde(default)]
            error: Option<String>,
            #[serde(default)]
            group_id: Option<String>,
            #[serde(default)]
            profile: Option<DeviceProfileWire>,
            #[serde(default)]
            devices: Vec<DeviceProfileWire>,
            #[serde(default)]
            vector: BTreeMap<String, i64>,
            #[serde(default)]
            ops: Vec<SyncOpWire>,
            #[serde(default)]
            snapshot: SyncSnapshot,
            #[serde(default)]
            playback: Option<PlaybackSnapshot>,
        },
        Hello {
            group_id: String,
            profile: DeviceProfileWire,
            devices: Vec<DeviceProfileWire>,
            vector: BTreeMap<String, i64>,
            ops: Vec<SyncOpWire>,
            snapshot: SyncSnapshot,
            #[serde(default)]
            playback: Option<PlaybackSnapshot>,
        },
        SyncResponse {
            accepted: bool,
            #[serde(default)]
            error: Option<String>,
            #[serde(default)]
            devices: Vec<DeviceProfileWire>,
            #[serde(default)]
            vector: BTreeMap<String, i64>,
            #[serde(default)]
            ops: Vec<SyncOpWire>,
            #[serde(default)]
            snapshot: SyncSnapshot,
            #[serde(default)]
            playback: Option<PlaybackSnapshot>,
        },
    }
}

const CONTENT_ID: &str = "b3:1111111111111111111111111111111111111111111111111111111111111111";

/// A v3 `hello` exercising every v3 operation kind, every snapshot section
/// and a coordinated playback snapshot (input order is irrelevant).
fn v3_hello_input() -> String {
    let fed = serde_json::json!({
        "item_id": "item", "owner": "owner", "title": "Song", "artist_names": ["A"],
        "featured_artist_names": [], "year": 2001, "duration_seconds": 180,
        "content_id": CONTENT_ID, "release_title": "Album", "track_number": 1, "disc_number": 1,
    });
    let track = serde_json::json!({
        "id": 7, "title": "Song", "track_number": 1, "disc_number": null,
        "duration_seconds": 180.5, "artist_names": ["A"], "featured_artist_names": ["B"],
        "release_id": 3, "release_title": "Album", "release_year": 2001, "file_path": "",
        "content_id": CONTENT_ID, "audio_format": "flac", "audio_bitrate": 900,
        "audio_sample_rate": 44100, "audio_bit_depth": 16, "file_size_bytes": 12345,
        "play_count": 2, "fed": fed,
    });
    let state = serde_json::json!({
        "queue": [track, {"id": 0, "title": "Remote", "track_number": null, "disc_number": null,
            "duration_seconds": 0.0, "release_id": 0, "release_title": "", "release_year": null,
            "content_id": null, "audio_format": null, "audio_bitrate": null,
            "audio_sample_rate": null, "audio_bit_depth": null, "file_size_bytes": null}],
        "queue_pos": 1, "playing": true, "paused": false, "idle_since_ms": null,
        "position_secs": 42.5, "volume": 73, "shuffle": true, "repeat": "all",
    });
    let claim = serde_json::json!({"counter": 3, "issuer": "tui_a", "owner": "tui_a"});
    let stamp = serde_json::json!({"version": 1, "claim": claim, "sequence": 9});
    let op = |seq: i64, payload: serde_json::Value| {
        serde_json::json!({"op_id": format!("tui_a:{seq}"), "origin_device_id": "tui_a",
            "seq": seq, "hlc_ms": 1_700_000_000_000i64 + seq, "payload": payload})
    };
    let profile = |id: &str| {
        serde_json::json!({"device_id": id, "name": "TUI", "client_version": "0.3.4",
            "protocol_version": 3, "endpoint_id": "e", "endpoint_ticket": "t",
            "revoked": false, "revoke_cutoff_seq": null, "updated_at_ms": 5})
    };
    serde_json::json!({
        "type": "hello",
        "group_id": "grp_1",
        "profile": profile("tui_a"),
        "devices": [profile("tui_a"), profile("web_b")],
        "vector": {"tui_a": 12, "web_b": 4},
        "ops": [
            op(1, serde_json::json!({"kind": "track_like_set", "content_id": CONTENT_ID, "liked": true, "fed": fed})),
            op(2, serde_json::json!({"kind": "playlist_created", "playlist_id": "pl", "title": "Mix"})),
            op(3, serde_json::json!({"kind": "playlist_renamed", "playlist_id": "pl", "title": "Mix 2"})),
            op(4, serde_json::json!({"kind": "playlist_track_added", "playlist_id": "pl", "content_id": CONTENT_ID, "position": 0, "fed": null})),
            op(5, serde_json::json!({"kind": "playlist_track_removed", "playlist_id": "pl", "content_id": CONTENT_ID})),
            op(6, serde_json::json!({"kind": "playlist_deleted", "playlist_id": "pl"})),
            op(7, serde_json::json!({"kind": "device_profile_set", "name": "TUI", "client_version": "0.3.4", "endpoint_ticket": "t", "endpoint_id": "e"})),
            op(8, serde_json::json!({"kind": "device_trusted", "target_device_id": "web_b"})),
            op(9, serde_json::json!({"kind": "device_revoked", "target_device_id": "old", "target_max_seq_seen": 3})),
            op(10, serde_json::json!({"kind": "playback_command", "target_device_id": "web_b",
                "command": {"kind": "set_state", "state": state, "seek": true}, "authority": stamp})),
            op(11, serde_json::json!({"kind": "playback_command", "target_device_id": "web_b",
                "command": {"kind": "active_changed", "active_device_id": "web_b", "active_device_name": "WEB", "state": state}})),
            op(12, serde_json::json!({"kind": "listen_recorded", "event": {"listen_id": "l1",
                "content_id": CONTENT_ID, "started_at_ms": 1, "listened_ms": 60000, "track_duration_ms": 180000,
                "ended_reason": "finished", "track": {"title": "Song", "artist_names": ["A"], "featured_artist_names": [], "release_title": null}}})),
        ],
        "snapshot": {
            "likes": [{"content_id": CONTENT_ID, "hlc_ms": 1, "op_id": "tui_a:1", "fed": fed}],
            "unlikes": [{"content_id": CONTENT_ID, "hlc_ms": 2, "op_id": "tui_a:2"}],
            "playlists": [{"playlist_id": "pl", "title": "Mix", "hlc_ms": 3, "op_id": "tui_a:3",
                "items": [{"content_id": CONTENT_ID, "position": 0, "hlc_ms": 4, "op_id": "tui_a:4", "fed": null}]}],
            "deleted_playlists": [{"playlist_id": "gone", "hlc_ms": 5, "op_id": "tui_a:5"}],
            "removed_playlist_items": [{"playlist_id": "pl", "content_id": CONTENT_ID, "hlc_ms": 6, "op_id": "tui_a:6"}],
        },
        "playback": {"device_id": "tui_a", "device_name": "TUI", "active": true, "updated_at_ms": 9,
            "state": state, "coordination": {"version": 1, "claim": claim, "heartbeat": 4, "available": true, "playing": true}},
    })
    .to_string()
}

fn v3_other_inputs() -> Vec<String> {
    vec![
        r#"{"type":"sync_response","accepted":false,"error":"device is not trusted"}"#.into(),
        r#"{"type":"pair_response","accepted":false,"pending":true,"error":"pairing pending"}"#
            .into(),
        serde_json::json!({"type": "pair_request", "invite_id": "i", "secret": "s",
            "profile": {"device_id": "tui_a", "name": "TUI", "client_version": "0.3.4",
                "protocol_version": 3, "endpoint_id": "e", "endpoint_ticket": "t"},
            "vector": {}, "ops": [], "snapshot": {}})
        .to_string(),
    ]
}

/// Parses `input` with the 0.3.x types and the shared types and checks
/// that both serialize to the same bytes, and that each side parses the
/// other's output.
fn assert_identical(input: &str) -> String {
    let old: legacy::WireMessage = serde_json::from_str(input).unwrap();
    let new: WireMessage = serde_json::from_str(input).unwrap();
    let old_json = serde_json::to_string(&old).unwrap();
    let new_json = serde_json::to_string(&new).unwrap();
    assert_eq!(old_json, new_json);
    assert_eq!(
        serde_json::from_str::<legacy::WireMessage>(&new_json).unwrap(),
        old
    );
    assert_eq!(serde_json::from_str::<WireMessage>(&old_json).unwrap(), new);
    new_json
}

#[test]
fn v3_messages_serialize_like_furumi_tui_0_3() {
    let hello = assert_identical(&v3_hello_input());
    let golden = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/devices/fixtures/v3_hello.golden.json"
    );
    if std::env::var_os("FURUMI_BLESS_GOLDEN").is_some() {
        // Only for a deliberate change of the v3 shape.
        std::fs::write(golden, format!("{hello}\n")).unwrap();
    }
    assert_eq!(hello, std::fs::read_to_string(golden).unwrap().trim_end());
    for input in v3_other_inputs() {
        assert_identical(&input);
    }
}

#[test]
fn music_queue_items_keep_their_v3_shape() {
    let track = TrackItem {
        id: 7,
        title: "Song".into(),
        track_number: Some(1),
        disc_number: None,
        duration_seconds: 180.5,
        artists: vec![ArtistRef {
            id: 1,
            name: "A".into(),
        }],
        featured_artists: Vec::new(),
        release_id: 3,
        release_title: "Album".into(),
        release_year: Some(2001),
        file_path: "/Users/me/Music/song.flac".into(),
        content_id: Some(CONTENT_ID.into()),
        cover_path: None,
        audio_format: Some("flac".into()),
        audio_bitrate: Some(900),
        audio_sample_rate: Some(44_100),
        audio_bit_depth: Some(16),
        file_size_bytes: Some(12_345),
        play_count: 2,
        fed: None,
        podcast: None,
    };
    let wire = PlaybackTrack::from_track(&track);
    let json = serde_json::to_string(&wire).unwrap();
    assert!(
        !json.contains("source"),
        "music items carry no source: {json}"
    );
    let old: legacy::PlaybackTrack = serde_json::from_str(&json).unwrap();
    assert_eq!(serde_json::to_string(&old).unwrap(), json);
}

#[test]
fn invites_from_older_clients_stay_readable() {
    let old = legacy::InviteWire {
        v: 1,
        ticket: "ticket".into(),
        device_id: "tui_a".into(),
        invite_id: "inv".into(),
        secret: "s3cr3t".into(),
        expires_at_ms: 10,
    };
    let old_json = serde_json::to_string(&old).unwrap();
    assert_eq!(
        old_json,
        r#"{"v":1,"t":"ticket","d":"tui_a","i":"inv","s":"s3cr3t","e":10}"#
    );
    let shared: InviteWire = serde_json::from_str(&old_json).unwrap();
    assert_eq!(
        shared.inviter_protocol_version(),
        music_dht::device_sync::UNREPORTED_PROTOCOL_VERSION
    );
    assert_eq!(serde_json::to_string(&shared).unwrap(), old_json);
}

#[test]
fn v3_peers_cannot_parse_podcast_operations() {
    // Why podcast operations must be gated: an unknown `kind` breaks the
    // whole message for a 0.3.x peer, not one operation.
    let mut hello: serde_json::Value = serde_json::from_str(&v3_hello_input()).unwrap();
    hello["ops"].as_array_mut().unwrap().push(serde_json::json!({
        "op_id": "tui_a:13", "origin_device_id": "tui_a", "seq": 13, "hlc_ms": 13,
        "payload": {"kind": "podcast_share_set",
            "podcast_id": music_dht::podcast::PodcastId::from_feed_url(
                &music_dht::podcast::FeedUrl::parse("https://podcasts.example/feed.xml").unwrap()),
            "shared": false, "generation": 1},
    }));
    assert!(serde_json::from_value::<legacy::WireMessage>(hello.clone()).is_err());
    assert!(serde_json::from_value::<WireMessage>(hello).is_ok());
}

#[test]
fn v3_peers_would_drop_the_source_of_podcast_items() {
    // Why podcast playback must be gated: older types ignore `source` and
    // forward the episode as a broken music item.
    let episode = crate::app::podcast_fixtures::episode_track(1)
        .podcast
        .unwrap();
    let wire = PlaybackTrack::podcast_episode(episode);
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains(r#""source":{"kind":"podcast_episode""#));
    let old: legacy::PlaybackTrack = serde_json::from_str(&json).unwrap();
    let forwarded: PlaybackTrack =
        serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
    assert_eq!(forwarded.source, None);
    assert_eq!(forwarded.id, 0);
}

#[test]
fn version_reports_stay_readable_by_older_clients() {
    let link = encode_invite(&InviteWire {
        v: 1,
        ticket: "ticket".into(),
        device_id: "tui_a".into(),
        invite_id: "inv".into(),
        secret: "s3cr3t".into(),
        expires_at_ms: 10,
        protocol_version: Some(PROTOCOL_VERSION),
    })
    .unwrap();
    let invite = parse_invite(&link).unwrap();
    assert_eq!(invite.inviter_protocol_version(), PROTOCOL_VERSION);
    let json = serde_json::to_string(&invite).unwrap();
    assert!(
        json.ends_with(&format!(r#","p":{PROTOCOL_VERSION}}}"#)),
        "{json}"
    );
    // `p` is an unknown field for 0.3.x and is ignored there.
    assert_eq!(
        serde_json::from_str::<legacy::InviteWire>(&json)
            .unwrap()
            .invite_id,
        "inv"
    );

    let announcement = SyncOpPayload::DeviceProfileSet {
        name: "TUI".into(),
        client_version: CLIENT_VERSION.into(),
        endpoint_ticket: "t".into(),
        endpoint_id: "e".into(),
        protocol_version: Some(PROTOCOL_VERSION),
    };
    let json = serde_json::to_string(&announcement).unwrap();
    assert!(json.ends_with(&format!(r#","protocol_version":{PROTOCOL_VERSION}}}"#)));
    let old: legacy::SyncOpPayload = serde_json::from_str(&json).unwrap();
    assert!(matches!(
        old,
        legacy::SyncOpPayload::DeviceProfileSet { .. }
    ));
}
