//! Cross-binary wire test. Run with scripts/test_device_interop.py; the other
//! endpoint is compiled from furumusic's real protocol types and player hub.
use super::*;
use tokio::io::AsyncWriteExt;

#[tokio::test]
#[ignore = "run scripts/test_device_interop.py to start both player test binaries"]
async fn localhost_web_peer() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = PathBuf::from(std::env::var("FURUMI_INTEROP_DIR").expect("interop runner"));
        let address = loop {
            if let Ok(address) = std::fs::read_to_string(dir.join("web-address")) {
                break address;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let mut stream = tokio::net::TcpStream::connect(address.trim())
            .await
            .unwrap();
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let sync = DeviceSync {
            _identity_lock: None,
            conn: Arc::new(std::sync::Mutex::new(conn)),
            library: Arc::new(Library::open(&dir.join("interop-library.sqlite3")).unwrap()),
            event_tx: Default::default(),
            playback: Default::default(),
        };
        let id = sync.ensure_identity().unwrap();
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        sync.set_event_tx(tx);
        sync.claim_playback(&id.device_id).unwrap();
        sync.playback_tick(true, true).unwrap();
        let mut state: PlaybackStateWire = serde_json::from_value(serde_json::json!({
            "queue": [{ "id": 7, "title": "Interop track", "duration_seconds": 123.5,
                "release_id": 1, "release_title": "Interop album", "artist_names": ["Interop artist"],
                "content_id": "b3:0000000000000000000000000000000000000000000000000000000000000000" }],
            "queue_pos": 0, "playing": true, "paused": false,
            "position_secs": 42.5, "volume": 73, "shuffle": true, "repeat": "all"
        }))
        .unwrap();
        for phase in 0..3 {
            sync.publish_playback(PlaybackSnapshot {
                device_id: id.device_id.clone(),
                device_name: "TUI interop".into(),
                active: true,
                updated_at_ms: now_ms(),
                state: state.clone(),
                coordination: None,
            });
            let hello = WireMessage::Hello {
                group_id: id.group_id.clone(),
                profile: sync.own_profile("").unwrap(),
                devices: vec![],
                vector: BTreeMap::new(),
                ops: vec![],
                snapshot: SyncSnapshot::default(),
                playback: sync.local_playback_snapshot(),
            };
            let mut bytes = serde_json::to_vec(&hello).unwrap();
            bytes.push(b'\n');
            stream.write_all(&bytes).await.unwrap();
            let response: WireMessage =
                serde_json::from_slice(&read_line(&mut stream).await.unwrap()).unwrap();
            let WireMessage::SyncResponse {
                accepted: true,
                playback: Some(snapshot),
                ops,
                ..
            } = response
            else {
                panic!("expected web response")
            };
            let web_id = snapshot.device_id.clone();
            sync.apply_playback_snapshot(&web_id, snapshot).unwrap();
            assert_eq!(ops.len(), 1);
            let op = &ops[0];
            // Production command adapter including durable fencing/deduplication.
            sync.apply_op(op).unwrap();
            sync.apply_op(op).unwrap();
            let mut commands = Vec::new();
            while let Ok(event) = events.try_recv() {
                if let AppEvent::PlaybackCommand {
                    command,
                    authority,
                    origin,
                } = event
                {
                    assert!(sync.playback_command_is_current(&origin, &authority));
                    commands.push(command);
                }
            }
            assert_eq!(commands.len(), 1, "one event even after duplicate delivery");
            match commands.pop().unwrap() {
                PlaybackCommand::ActiveChanged {
                    active_device_id,
                    state: next,
                    ..
                } => {
                    assert_eq!(
                        active_device_id,
                        if phase == 0 {
                            web_id.clone()
                        } else {
                            id.device_id.clone()
                        }
                    );
                    assert_eq!(next.position_secs, 42.5);
                    assert_eq!(next.queue, state.queue);
                    state = next;
                }
                PlaybackCommand::SetState { state: next, seek } => {
                    assert_eq!(phase, 2);
                    assert!(seek);
                    assert!(next.paused);
                    assert_eq!(next.position_secs, 87.0);
                    state = next;
                }
            }
            assert_eq!(
                sync.playback_tick(true, false).unwrap(),
                Some(if phase == 0 {
                    web_id
                } else {
                    id.device_id.clone()
                })
            );
        }
        stream.write_all(b"ok\n").await.unwrap();
    })
    .await
    .expect("web/TUI exchange timed out");
}
