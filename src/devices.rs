//! Personal-device sync: trusted furumi clients that belong to one listener.
//!
//! The transport is a small JSON-lines protocol over a dedicated frid/iroh
//! byte-stream ALPN. Local state is stored as an append-only operation log plus
//! materialized tables, so offline clients can merge likes, playlists and
//! membership changes deterministically.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use music_dht::{ByteStream, MusicDhtService, NetworkId, PeerTicket, SecretKey, StreamAcceptor};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::app::event::AppEvent;
use crate::library::Library;
use crate::library::models::{ArtistRef, TrackItem};

pub const SYNC_ALPN: &[u8] = b"furumi/sync/1";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: u16 = 1;
const INVITE_TTL_MS: i64 = 10 * 60 * 1000;
const PAIRING_WAIT_MS: i64 = 5 * 60 * 1000;
const PAIRING_RETRY_DELAY: Duration = Duration::from_secs(1);
const RESPONSE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const SYNC_INTERVAL: Duration = Duration::from_secs(2);
const MAX_LINE: usize = 8 * 1024 * 1024;
const MAX_OPS_PER_BATCH: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPairing {
    pub request_id: String,
    pub device_id: String,
    pub name: String,
    pub client_version: String,
    pub requester_group_id: Option<String>,
    pub requester_group_active_devices: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceStatusRow {
    pub device_id: String,
    pub name: String,
    pub client_version: String,
    pub endpoint_id: String,
    pub last_seen_ms: Option<i64>,
    pub revoked: bool,
    pub is_self: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceSyncStatus {
    pub this_device_id: String,
    pub this_device_name: String,
    pub group_id: String,
    pub active_devices: usize,
    pub revoked_devices: usize,
    pub pending_requests: usize,
    pub ops_total: usize,
    pub tombstone_ops: usize,
    pub compactable_tombstones: usize,
    pub outbox_ops: usize,
    pub snapshot_likes: usize,
    pub snapshot_playlists: usize,
    pub snapshot_items: usize,
    pub unresolved_playlist_items: usize,
    pub peer_ack_floor: String,
    pub last_sync: Option<String>,
    pub last_error: Option<String>,
    pub devices: Vec<DeviceStatusRow>,
}

#[derive(Clone)]
pub struct DeviceSync {
    conn: Arc<std::sync::Mutex<Connection>>,
    library: Arc<Library>,
    event_tx: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<AppEvent>>>>,
    playback: Arc<std::sync::Mutex<PlaybackShared>>,
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
    /// Legacy compatibility only. Paths are device-local, so playback sync
    /// must resolve tracks from content/federation identifiers instead.
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

impl PlaybackTrack {
    fn portable_placeholder_id(&self) -> i64 {
        let key = self
            .content_id
            .as_deref()
            .and_then(music_dht::normalize_content_id)
            .map(|content_id| format!("content:{content_id}"))
            .or_else(|| {
                self.fed
                    .as_ref()
                    .map(|fed| fed.content_id.as_str())
                    .and_then(music_dht::normalize_content_id)
                    .map(|content_id| format!("content:{content_id}"))
            })
            .or_else(|| {
                self.fed
                    .as_ref()
                    .map(|fed| format!("fed:{}:{}", fed.owner, fed.item_id))
            })
            .unwrap_or_else(|| {
                format!(
                    "remote:{}:{}:{}:{}",
                    self.id, self.title, self.release_title, self.duration_seconds
                )
            });
        let hash = blake3::hash(key.as_bytes());
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&hash.as_bytes()[..8]);
        let positive = (i64::from_be_bytes(bytes) & i64::MAX).max(1);
        -positive
    }

    pub fn from_track(track: &TrackItem) -> Self {
        Self {
            id: track.id,
            title: track.title.clone(),
            track_number: track.track_number,
            disc_number: track.disc_number,
            duration_seconds: track.duration_seconds,
            artist_names: track
                .artists
                .iter()
                .map(|artist| artist.name.clone())
                .collect(),
            featured_artist_names: track
                .featured_artists
                .iter()
                .map(|artist| artist.name.clone())
                .collect(),
            release_id: track.release_id,
            release_title: track.release_title.clone(),
            release_year: track.release_year,
            file_path: String::new(),
            content_id: track
                .content_id
                .clone()
                .or_else(|| track.fed.as_ref().and_then(|fed| fed.content_id.clone())),
            audio_format: track.audio_format.clone(),
            audio_bitrate: track.audio_bitrate,
            audio_sample_rate: track.audio_sample_rate,
            audio_bit_depth: track.audio_bit_depth,
            file_size_bytes: track.file_size_bytes,
            play_count: track.play_count,
            fed: track.fed.as_ref().and_then(SyncedFedTrack::from_fed),
        }
    }

    pub fn to_track_item(&self) -> TrackItem {
        let refs = |names: &[String]| -> Vec<ArtistRef> {
            names
                .iter()
                .map(|name| ArtistRef {
                    id: -1,
                    name: name.clone(),
                })
                .collect()
        };
        TrackItem {
            id: self.portable_placeholder_id(),
            title: self.title.clone(),
            track_number: self.track_number,
            disc_number: self.disc_number,
            duration_seconds: self.duration_seconds,
            artists: refs(&self.artist_names),
            featured_artists: refs(&self.featured_artist_names),
            release_id: self.release_id,
            release_title: self.release_title.clone(),
            release_year: self.release_year,
            file_path: String::new(),
            content_id: self.content_id.clone(),
            cover_path: None,
            audio_format: self.audio_format.clone(),
            audio_bitrate: self.audio_bitrate,
            audio_sample_rate: self.audio_sample_rate,
            audio_bit_depth: self.audio_bit_depth,
            file_size_bytes: self.file_size_bytes,
            play_count: self.play_count,
            fed: self.fed.as_ref().map(SyncedFedTrack::to_fed_track),
        }
    }
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaybackSnapshot {
    pub device_id: String,
    pub device_name: String,
    pub active: bool,
    pub updated_at_ms: i64,
    pub state: PlaybackStateWire,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlaybackCommand {
    SetState {
        state: PlaybackStateWire,
        #[serde(default)]
        seek: bool,
    },
    ActiveChanged {
        active_device_id: String,
        active_device_name: String,
        state: PlaybackStateWire,
    },
}

#[derive(Debug, Clone, Default)]
struct PlaybackShared {
    local: Option<PlaybackSnapshot>,
    remote: BTreeMap<String, PlaybackSnapshot>,
}

#[derive(Debug, Clone)]
struct LocalIdentity {
    device_id: String,
    group_id: String,
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InviteWire {
    v: u16,
    #[serde(rename = "t")]
    ticket: String,
    #[serde(rename = "d")]
    device_id: String,
    #[serde(rename = "i")]
    invite_id: String,
    #[serde(rename = "s")]
    secret: String,
    #[serde(rename = "e")]
    expires_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceProfileWire {
    device_id: String,
    name: String,
    client_version: String,
    protocol_version: u16,
    endpoint_id: String,
    endpoint_ticket: String,
    #[serde(default)]
    revoked: bool,
    #[serde(default)]
    revoke_cutoff_seq: Option<i64>,
    #[serde(default)]
    updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncOpWire {
    op_id: String,
    origin_device_id: String,
    seq: i64,
    hlc_ms: i64,
    payload: SyncOpPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    },
}

impl SyncOpPayload {
    fn is_tombstone(&self) -> bool {
        matches!(
            self,
            SyncOpPayload::TrackLikeSet { liked: false, .. }
                | SyncOpPayload::PlaylistDeleted { .. }
                | SyncOpPayload::PlaylistTrackRemoved { .. }
                | SyncOpPayload::DeviceRevoked { .. }
        )
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SyncSnapshot {
    #[serde(default)]
    likes: Vec<SnapshotLike>,
    #[serde(default)]
    unlikes: Vec<SnapshotLikeTombstone>,
    #[serde(default)]
    playlists: Vec<SnapshotPlaylist>,
    #[serde(default)]
    deleted_playlists: Vec<SnapshotPlaylistTombstone>,
    #[serde(default)]
    removed_playlist_items: Vec<SnapshotPlaylistItemTombstone>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotLike {
    content_id: String,
    hlc_ms: i64,
    op_id: String,
    #[serde(default)]
    fed: Option<SyncedFedTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotLikeTombstone {
    content_id: String,
    hlc_ms: i64,
    op_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotPlaylist {
    playlist_id: String,
    title: String,
    hlc_ms: i64,
    op_id: String,
    #[serde(default)]
    items: Vec<SnapshotPlaylistItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotPlaylistTombstone {
    playlist_id: String,
    hlc_ms: i64,
    op_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotPlaylistItem {
    content_id: String,
    position: i64,
    hlc_ms: i64,
    op_id: String,
    #[serde(default)]
    fed: Option<SyncedFedTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotPlaylistItemTombstone {
    playlist_id: String,
    content_id: String,
    hlc_ms: i64,
    op_id: String,
}

enum PairAttempt {
    Accepted(String),
    Pending,
    Denied(String),
}

struct PairingStatus {
    status: String,
    use_requester_group: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncedFedTrack {
    item_id: String,
    owner: String,
    title: String,
    #[serde(default)]
    artist_names: Vec<String>,
    #[serde(default)]
    featured_artist_names: Vec<String>,
    year: Option<i32>,
    duration_seconds: Option<i64>,
    content_id: String,
    release_title: Option<String>,
    track_number: Option<i32>,
    disc_number: Option<i32>,
}

impl SyncedFedTrack {
    fn from_fed(fed: &crate::federation::FedTrack) -> Option<Self> {
        let content_id = fed
            .content_id
            .as_deref()
            .and_then(music_dht::normalize_content_id)?;
        Some(Self {
            item_id: fed.item_id.clone(),
            owner: fed.owner.clone(),
            title: fed.title.clone(),
            artist_names: fed.artist_names.clone(),
            featured_artist_names: fed.featured_artist_names.clone(),
            year: fed.year,
            duration_seconds: fed.duration_seconds,
            content_id,
            release_title: fed.release_title.clone(),
            track_number: fed.track_number,
            disc_number: fed.disc_number,
        })
    }

    fn to_fed_track(&self) -> crate::federation::FedTrack {
        crate::federation::FedTrack {
            item_id: self.item_id.clone(),
            owner: self.owner.clone(),
            own: false,
            title: self.title.clone(),
            artist_names: self.artist_names.clone(),
            featured_artist_names: self.featured_artist_names.clone(),
            year: self.year,
            duration_seconds: self.duration_seconds,
            content_id: Some(self.content_id.clone()),
            release_title: self.release_title.clone(),
            track_number: self.track_number,
            disc_number: self.disc_number,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireMessage {
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

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_label() -> String {
    let secs = (now_ms() / 1000).max(0);
    format!(
        "{:02}:{:02}:{:02} UTC",
        secs / 3600 % 24,
        secs / 60 % 60,
        secs % 60
    )
}

fn default_db_path() -> PathBuf {
    crate::config::project_dirs()
        .map(|dirs| dirs.data_dir().join("devices").join("sync.sqlite3"))
        .unwrap_or_else(|| PathBuf::from("devices").join("sync.sqlite3"))
}

impl DeviceSync {
    pub fn new(library: Arc<Library>) -> Result<Arc<Self>> {
        let path = default_db_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn =
            Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        init_schema(&conn)?;
        let sync = Arc::new(Self {
            conn: Arc::new(std::sync::Mutex::new(conn)),
            library,
            event_tx: Arc::new(std::sync::Mutex::new(None)),
            playback: Arc::new(std::sync::Mutex::new(PlaybackShared::default())),
        });
        sync.ensure_identity()?;
        sync.repair_like_order_from_sync_state()?;
        Ok(sync)
    }

    pub fn set_event_tx(&self, tx: tokio::sync::mpsc::UnboundedSender<AppEvent>) {
        *lock(&self.event_tx) = Some(tx);
    }

    pub fn identity_summary(&self) -> Result<(String, String)> {
        let identity = self.ensure_identity()?;
        Ok((identity.device_id, identity.name))
    }

    pub fn publish_playback(&self, mut snapshot: PlaybackSnapshot) {
        if snapshot.updated_at_ms <= 0 {
            snapshot.updated_at_ms = now_ms();
        }
        lock(&self.playback).local = Some(snapshot);
    }

    pub fn record_playback_command(
        &self,
        target_device_id: &str,
        command: PlaybackCommand,
    ) -> Result<()> {
        if target_device_id.trim().is_empty() {
            return Ok(());
        }
        self.record_local_op(SyncOpPayload::PlaybackCommand {
            target_device_id: target_device_id.to_string(),
            command,
        })
    }

    pub fn set_device_name(&self, name: &str, endpoint_ticket: Option<&str>) -> Result<()> {
        let name = if name.trim().is_empty() {
            "furumi".to_string()
        } else {
            name.trim().to_string()
        };
        {
            let conn = lock(&self.conn);
            set_meta(&conn, "device_name", &name)?;
            if let Some(device_id) = get_meta(&conn, "device_id")? {
                conn.execute(
                    "UPDATE sync_devices
                     SET name = ?2, client_version = ?3
                     WHERE device_id = ?1",
                    params![device_id, name, CLIENT_VERSION],
                )?;
            }
        }
        if let Some(ticket) = endpoint_ticket {
            let endpoint_id = ticket_endpoint_id(ticket).unwrap_or_default();
            self.record_local_op(SyncOpPayload::DeviceProfileSet {
                name,
                client_version: CLIENT_VERSION.to_string(),
                endpoint_ticket: ticket.to_string(),
                endpoint_id,
            })?;
        }
        Ok(())
    }

    pub fn status(&self) -> DeviceSyncStatus {
        match self.status_inner() {
            Ok(status) => status,
            Err(err) => DeviceSyncStatus {
                last_error: Some(format!("{err:#}")),
                ..DeviceSyncStatus::default()
            },
        }
    }

    pub async fn create_invite(&self, service: Arc<MusicDhtService>) -> Result<String> {
        let identity = self.ensure_identity()?;
        let ticket = service.ticket().await?.to_string();
        let secret = random_hex(16);
        let invite_id = random_hex(8);
        let expires_at_ms = now_ms() + INVITE_TTL_MS;
        {
            let conn = lock(&self.conn);
            conn.execute(
                "INSERT INTO sync_invites (invite_id, secret_hash, expires_at_ms, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![invite_id, hash_secret(&secret), expires_at_ms, now_ms()],
            )?;
        }
        let payload = InviteWire {
            v: 1,
            ticket,
            device_id: identity.device_id,
            invite_id,
            secret,
            expires_at_ms,
        };
        let bytes = serde_json::to_vec(&payload)?;
        Ok(format!("frid://i/{}", base64url_encode(&bytes)))
    }

    pub async fn connect_invite(
        &self,
        service: Arc<MusicDhtService>,
        invite_link: &str,
        transport_stats: Arc<crate::federation::TransportStats>,
    ) -> Result<String> {
        let invite = parse_invite(invite_link)?;
        anyhow::ensure!(invite.expires_at_ms >= now_ms(), "invite expired");
        let ticket: PeerTicket = invite
            .ticket
            .parse()
            .map_err(|err| anyhow::anyhow!("malformed invite ticket: {err}"))?;
        let deadline = (now_ms() + PAIRING_WAIT_MS).min(invite.expires_at_ms);
        let mut last_error: Option<String>;
        loop {
            match self
                .try_connect_invite(
                    Arc::clone(&service),
                    &invite,
                    ticket.clone(),
                    Arc::clone(&transport_stats),
                )
                .await
            {
                Ok(PairAttempt::Accepted(message)) => return Ok(message),
                Ok(PairAttempt::Pending) => last_error = None,
                Ok(PairAttempt::Denied(message)) => anyhow::bail!(message),
                Err(err) => {
                    tracing::debug!("pairing poll failed: {err:#}");
                    last_error = Some(format!("{err:#}"));
                }
            }
            if now_ms() >= deadline {
                if let Some(error) = last_error {
                    anyhow::bail!("pairing timed out; last error: {error}");
                }
                anyhow::bail!("pairing timed out");
            }
            tokio::time::sleep(PAIRING_RETRY_DELAY).await;
        }
    }

    async fn try_connect_invite(
        &self,
        service: Arc<MusicDhtService>,
        invite: &InviteWire,
        ticket: PeerTicket,
        transport_stats: Arc<crate::federation::TransportStats>,
    ) -> Result<PairAttempt> {
        let peer = service.connect(ticket).await?;
        let own_ticket = service.ticket().await?.to_string();
        let identity = self.ensure_identity()?;
        let group_active_devices = self.active_device_count()?;
        let profile = self.own_profile(&own_ticket)?;
        let devices = self.device_profiles()?;
        let vector = self.vector()?;
        let ops = self.ops_for_peer(&invite.device_id)?;
        let snapshot = self.snapshot()?;
        let playback = self.local_playback_snapshot();
        let mut stream = service.open_stream(peer, SYNC_ALPN).await?;
        crate::federation::record_stream_transport(
            &transport_stats,
            "device-sync",
            "outbound",
            "pair-open",
            &stream,
        );
        write_msg(
            &mut stream,
            &WireMessage::PairRequest {
                invite_id: invite.invite_id.clone(),
                secret: invite.secret.clone(),
                profile,
                group_id: Some(identity.group_id),
                group_active_devices,
                devices,
                vector,
                ops,
                snapshot,
                playback,
            },
        )
        .await?;
        finish_send(&mut stream).await?;
        let response = read_msg(&mut stream)
            .await
            .context("pairing response was not received")?;
        crate::federation::record_stream_transport(
            &transport_stats,
            "device-sync",
            "outbound",
            "pair-done",
            &stream,
        );
        match response {
            WireMessage::PairResponse {
                accepted: true,
                group_id: Some(group_id),
                profile,
                devices,
                vector,
                ops,
                snapshot,
                playback,
                ..
            } => {
                self.set_group_id(&group_id)?;
                if let Some(profile) = profile {
                    self.apply_device_profile(&profile, true)?;
                    if let Some(playback) = playback {
                        self.apply_playback_snapshot(playback)?;
                    }
                }
                self.apply_device_profiles(&devices)?;
                self.apply_snapshot(snapshot)?;
                self.apply_ops(ops)?;
                self.record_local_op(SyncOpPayload::DeviceTrusted {
                    target_device_id: invite.device_id.clone(),
                })?;
                self.note_peer_vector(&invite.device_id, &vector)?;
                self.set_last_sync(Some(format!("paired with {}", short_id(&invite.device_id))))?;
                self.gc_tombstones()?;
                Ok(PairAttempt::Accepted(format!(
                    "connected device {}",
                    short_id(&invite.device_id)
                )))
            }
            WireMessage::PairResponse {
                accepted: false,
                pending: true,
                ..
            } => Ok(PairAttempt::Pending),
            WireMessage::PairResponse {
                accepted: false,
                error,
                ..
            } => Ok(PairAttempt::Denied(
                error.unwrap_or_else(|| "pairing denied".to_string()),
            )),
            _ => anyhow::bail!("unexpected pairing response"),
        }
    }

    pub fn answer_pairing(
        &self,
        request_id: &str,
        accept: bool,
        use_requester_group: bool,
    ) -> Result<()> {
        let pending = {
            let conn = lock(&self.conn);
            conn.query_row(
                "SELECT device_id, name, client_version, endpoint_id,
                        endpoint_ticket, created_at_ms,
                        requester_group_id, requester_group_devices_json
                 FROM sync_pending_pairing
                 WHERE request_id = ?1",
                [request_id],
                |row| {
                    Ok((
                        DeviceProfileWire {
                            device_id: row.get(0)?,
                            name: row.get(1)?,
                            client_version: row.get(2)?,
                            protocol_version: PROTOCOL_VERSION,
                            endpoint_id: row.get(3)?,
                            endpoint_ticket: row.get(4)?,
                            revoked: false,
                            revoke_cutoff_seq: None,
                            updated_at_ms: row.get(5)?,
                        },
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?
        };
        let changed = {
            let conn = lock(&self.conn);
            conn.execute(
                "UPDATE sync_pending_pairing
                 SET status = ?2, answered_at_ms = ?3, use_requester_group = ?4
                 WHERE request_id = ?1 AND status = 'pending'",
                params![
                    request_id,
                    if accept { "accepted" } else { "denied" },
                    now_ms(),
                    i64::from(use_requester_group),
                ],
            )?
        };
        if accept && changed > 0 {
            if let Some((profile, requester_group_id, requester_group_devices_json)) = pending {
                if use_requester_group {
                    if let Some(group_id) = requester_group_id
                        .as_deref()
                        .filter(|id| !id.trim().is_empty())
                    {
                        self.set_group_id(group_id)?;
                    }
                    let requester_devices: Vec<DeviceProfileWire> =
                        serde_json::from_str(&requester_group_devices_json)?;
                    self.apply_device_profiles(&requester_devices)?;
                }
                self.apply_device_profile(&profile, true)?;
                self.record_local_op(SyncOpPayload::DeviceTrusted {
                    target_device_id: profile.device_id,
                })?;
            }
        }
        Ok(())
    }

    pub fn revoke_device(&self, device_id: &str) -> Result<()> {
        let own = self.ensure_identity()?.device_id;
        anyhow::ensure!(device_id != own, "cannot revoke this device from itself");
        let cutoff = {
            let conn = lock(&self.conn);
            conn.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM sync_ops WHERE origin_device_id = ?1",
                [device_id],
                |row| row.get::<_, i64>(0),
            )?
        };
        self.record_local_op(SyncOpPayload::DeviceRevoked {
            target_device_id: device_id.to_string(),
            target_max_seq_seen: cutoff,
        })?;
        self.gc_tombstones()?;
        Ok(())
    }

    pub fn record_content_like(&self, content_id: &str, liked: bool) -> Result<()> {
        let Some(content_id) = music_dht::normalize_content_id(content_id) else {
            return Ok(());
        };
        self.record_local_op(SyncOpPayload::TrackLikeSet {
            content_id,
            liked,
            fed: None,
        })?;
        Ok(())
    }

    pub fn record_fed_like(&self, fed: &crate::federation::FedTrack, liked: bool) -> Result<()> {
        let Some(content_id) = fed
            .content_id
            .as_deref()
            .and_then(music_dht::normalize_content_id)
        else {
            return Ok(());
        };
        self.record_local_op(SyncOpPayload::TrackLikeSet {
            content_id,
            liked,
            fed: liked.then(|| SyncedFedTrack::from_fed(fed)).flatten(),
        })?;
        Ok(())
    }

    pub fn record_playlist_created(&self, playlist_id: i64, title: &str) -> Result<()> {
        let playlist_id = self.library.ensure_playlist_sync_id(playlist_id)?;
        self.record_local_op(SyncOpPayload::PlaylistCreated {
            playlist_id,
            title: title.to_string(),
        })?;
        Ok(())
    }

    pub fn record_playlist_renamed(&self, playlist_id: i64, title: &str) -> Result<()> {
        let playlist_id = self.library.ensure_playlist_sync_id(playlist_id)?;
        self.record_local_op(SyncOpPayload::PlaylistRenamed {
            playlist_id,
            title: title.to_string(),
        })?;
        Ok(())
    }

    pub fn record_playlist_deleted(&self, playlist_id: i64) -> Result<()> {
        let Some(playlist_id) = self.library.playlist_sync_id(playlist_id)? else {
            return Ok(());
        };
        self.record_local_op(SyncOpPayload::PlaylistDeleted { playlist_id })?;
        Ok(())
    }

    pub fn record_playlist_tracks_added(&self, playlist_id: i64, track_ids: &[i64]) -> Result<()> {
        let playlist_sync_id = self.library.ensure_playlist_sync_id(playlist_id)?;
        for (content_id, position) in self
            .library
            .playlist_track_content_positions(playlist_id, track_ids)?
        {
            self.record_local_op(SyncOpPayload::PlaylistTrackAdded {
                playlist_id: playlist_sync_id.clone(),
                content_id,
                position,
                fed: None,
            })?;
        }
        Ok(())
    }

    pub fn record_playlist_fed_tracks_added(
        &self,
        playlist_id: i64,
        tracks: &[crate::federation::FedTrack],
    ) -> Result<()> {
        let playlist_sync_id = self.library.ensure_playlist_sync_id(playlist_id)?;
        for (fallback_position, fed) in tracks.iter().enumerate() {
            let Some(content_id) = fed
                .content_id
                .as_deref()
                .and_then(music_dht::normalize_content_id)
            else {
                continue;
            };
            let position = self
                .library
                .playlist_content_position(playlist_id, &content_id)?
                .unwrap_or(fallback_position as i64);
            self.record_local_op(SyncOpPayload::PlaylistTrackAdded {
                playlist_id: playlist_sync_id.clone(),
                content_id,
                position,
                fed: SyncedFedTrack::from_fed(fed),
            })?;
        }
        Ok(())
    }

    pub fn record_playlist_content_removed(
        &self,
        playlist_id: i64,
        content_ids: &[String],
    ) -> Result<()> {
        let Some(playlist_id) = self.library.playlist_sync_id(playlist_id)? else {
            return Ok(());
        };
        let mut seen = BTreeSet::new();
        for content_id in content_ids {
            let Some(content_id) = music_dht::normalize_content_id(content_id) else {
                continue;
            };
            if !seen.insert(content_id.clone()) {
                continue;
            }
            self.record_local_op(SyncOpPayload::PlaylistTrackRemoved {
                playlist_id: playlist_id.clone(),
                content_id,
            })?;
        }
        Ok(())
    }

    pub async fn sync_once(
        &self,
        service: Arc<MusicDhtService>,
        transport_stats: Arc<crate::federation::TransportStats>,
    ) -> Result<()> {
        let devices = self.active_remote_devices()?;
        for device in devices {
            if device.endpoint_ticket.trim().is_empty() {
                continue;
            }
            if let Err(err) = self
                .sync_device(Arc::clone(&service), &device, Arc::clone(&transport_stats))
                .await
            {
                tracing::debug!(device = %device.device_id, "device sync failed: {err:#}");
                self.set_last_error(Some(format!("{}: {err:#}", short_id(&device.device_id))))?;
            }
        }
        self.gc_tombstones()?;
        Ok(())
    }

    async fn sync_device(
        &self,
        service: Arc<MusicDhtService>,
        device: &StoredDevice,
        transport_stats: Arc<crate::federation::TransportStats>,
    ) -> Result<()> {
        let ticket: PeerTicket = device.endpoint_ticket.parse()?;
        let peer = service.connect(ticket).await?;
        let own_ticket = service.ticket().await?.to_string();
        let identity = self.ensure_identity()?;
        let profile = self.own_profile(&own_ticket)?;
        let devices = self.device_profiles()?;
        let vector = self.vector()?;
        let ops = self.ops_for_peer(&device.device_id)?;
        let snapshot = self.snapshot()?;
        let playback = self.local_playback_snapshot();
        let mut stream = service.open_stream(peer, SYNC_ALPN).await?;
        crate::federation::record_stream_transport(
            &transport_stats,
            "device-sync",
            "outbound",
            "sync-open",
            &stream,
        );
        write_msg(
            &mut stream,
            &WireMessage::Hello {
                group_id: identity.group_id,
                profile,
                devices,
                vector,
                ops,
                snapshot,
                playback,
            },
        )
        .await?;
        finish_send(&mut stream).await?;
        let response = read_msg(&mut stream)
            .await
            .context("device sync response was not received")?;
        crate::federation::record_stream_transport(
            &transport_stats,
            "device-sync",
            "outbound",
            "sync-done",
            &stream,
        );
        match response {
            WireMessage::SyncResponse {
                accepted: true,
                devices,
                vector,
                ops,
                snapshot,
                playback,
                ..
            } => {
                self.apply_device_profiles(&devices)?;
                if let Some(playback) = playback {
                    self.apply_playback_snapshot(playback)?;
                }
                self.apply_snapshot(snapshot)?;
                self.apply_ops(ops)?;
                self.note_peer_vector(&device.device_id, &vector)?;
                self.mark_seen(&device.device_id, Some(peer.to_string()))?;
                self.set_last_sync(Some(format!("synced {}", short_id(&device.device_id))))?;
                self.set_last_error(None)?;
                Ok(())
            }
            WireMessage::SyncResponse {
                accepted: false,
                error,
                ..
            } => anyhow::bail!(error.unwrap_or_else(|| "sync refused".to_string())),
            _ => anyhow::bail!("unexpected sync response"),
        }
    }

    fn ensure_identity(&self) -> Result<LocalIdentity> {
        let conn = lock(&self.conn);
        if let (Some(device_id), Some(group_id), Some(name)) = (
            get_meta(&conn, "device_id")?,
            get_meta(&conn, "group_id")?,
            get_meta(&conn, "device_name")?,
        ) {
            return Ok(LocalIdentity {
                device_id,
                group_id,
                name,
            });
        }

        let key = SecretKey::generate();
        let secret_hex = hex_encode(&key.to_bytes());
        let device_id = format!(
            "dev_{}",
            &blake3::hash(secret_hex.as_bytes()).to_hex()[..24]
        );
        let group_id = format!("grp_{}", &blake3::hash(device_id.as_bytes()).to_hex()[..24]);
        let name = default_device_name(&device_id);
        set_meta(&conn, "device_secret", &secret_hex)?;
        set_meta(&conn, "device_id", &device_id)?;
        set_meta(&conn, "group_id", &group_id)?;
        set_meta(&conn, "device_name", &name)?;
        set_meta(&conn, "local_seq", "0")?;
        set_meta(&conn, "last_hlc_ms", "0")?;
        conn.execute(
            "INSERT OR IGNORE INTO sync_devices
                (device_id, name, client_version, protocol_version, endpoint_id,
                 endpoint_ticket, trusted_at_ms, last_seen_ms)
             VALUES (?1, ?2, ?3, ?4, '', '', ?5, ?5)",
            params![device_id, name, CLIENT_VERSION, PROTOCOL_VERSION, now_ms()],
        )?;
        Ok(LocalIdentity {
            device_id,
            group_id,
            name,
        })
    }

    fn set_group_id(&self, group_id: &str) -> Result<()> {
        let conn = lock(&self.conn);
        set_meta(&conn, "group_id", group_id)
    }

    fn own_profile(&self, endpoint_ticket: &str) -> Result<DeviceProfileWire> {
        let identity = self.ensure_identity()?;
        let endpoint_id = ticket_endpoint_id(endpoint_ticket).unwrap_or_default();
        {
            let conn = lock(&self.conn);
            conn.execute(
                "INSERT INTO sync_devices
                    (device_id, name, client_version, protocol_version, endpoint_id,
                     endpoint_ticket, trusted_at_ms, last_seen_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                 ON CONFLICT(device_id) DO UPDATE SET
                    name = excluded.name,
                    client_version = excluded.client_version,
                    protocol_version = excluded.protocol_version,
                    endpoint_id = excluded.endpoint_id,
                    endpoint_ticket = excluded.endpoint_ticket,
                    last_seen_ms = excluded.last_seen_ms",
                params![
                    identity.device_id,
                    identity.name,
                    CLIENT_VERSION,
                    PROTOCOL_VERSION,
                    endpoint_id,
                    endpoint_ticket,
                    now_ms(),
                ],
            )?;
        }
        Ok(DeviceProfileWire {
            device_id: identity.device_id,
            name: identity.name,
            client_version: CLIENT_VERSION.to_string(),
            protocol_version: PROTOCOL_VERSION,
            endpoint_id,
            endpoint_ticket: endpoint_ticket.to_string(),
            revoked: false,
            revoke_cutoff_seq: None,
            updated_at_ms: now_ms(),
        })
    }

    fn record_local_op(&self, payload: SyncOpPayload) -> Result<()> {
        let identity = self.ensure_identity()?;
        let (op, payload_json, tombstone) = {
            let conn = lock(&self.conn);
            let seq = get_meta(&conn, "local_seq")?
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0)
                + 1;
            let last_hlc = get_meta(&conn, "last_hlc_ms")?
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            let hlc_ms = now_ms().max(last_hlc + 1);
            let op_id = format!("{}:{seq}", identity.device_id);
            let payload_json = serde_json::to_string(&payload)?;
            let tombstone = payload.is_tombstone();
            conn.execute(
                "INSERT OR IGNORE INTO sync_ops
                    (op_id, origin_device_id, seq, kind, payload_json, hlc_ms,
                     received_at_ms, tombstone)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    op_id,
                    identity.device_id,
                    seq,
                    payload_kind(&payload),
                    payload_json,
                    hlc_ms,
                    now_ms(),
                    i64::from(tombstone),
                ],
            )?;
            set_meta(&conn, "local_seq", &seq.to_string())?;
            set_meta(&conn, "last_hlc_ms", &hlc_ms.to_string())?;
            conn.execute(
                "INSERT INTO sync_vectors (device_id, max_seq)
                 VALUES (?1, ?2)
                 ON CONFLICT(device_id) DO UPDATE SET
                    max_seq = MAX(max_seq, excluded.max_seq)",
                params![identity.device_id, seq],
            )?;
            (
                SyncOpWire {
                    op_id,
                    origin_device_id: identity.device_id,
                    seq,
                    hlc_ms,
                    payload,
                },
                payload_json,
                tombstone,
            )
        };
        let _ = self.apply_op(&op)?;
        tracing::debug!(
            op_id = %op.op_id,
            tombstone,
            payload = %payload_json,
            "recorded personal-sync op"
        );
        let _ = self.gc_tombstones();
        Ok(())
    }

    fn apply_ops(&self, ops: Vec<SyncOpWire>) -> Result<()> {
        let mut changed = false;
        for op in ops {
            if !self.should_accept_op(&op)? {
                continue;
            }
            let inserted = {
                let conn = lock(&self.conn);
                conn.execute(
                    "INSERT OR IGNORE INTO sync_ops
                        (op_id, origin_device_id, seq, kind, payload_json, hlc_ms,
                         received_at_ms, tombstone)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        op.op_id,
                        op.origin_device_id,
                        op.seq,
                        payload_kind(&op.payload),
                        serde_json::to_string(&op.payload)?,
                        op.hlc_ms,
                        now_ms(),
                        i64::from(op.payload.is_tombstone()),
                    ],
                )?
            };
            if inserted > 0 {
                changed |= self.apply_op(&op)?;
            }
            let conn = lock(&self.conn);
            conn.execute(
                "INSERT INTO sync_vectors (device_id, max_seq)
                 VALUES (?1, ?2)
                 ON CONFLICT(device_id) DO UPDATE SET
                    max_seq = MAX(max_seq, excluded.max_seq)",
                params![op.origin_device_id, op.seq],
            )?;
        }
        if changed {
            self.notify_library_changed();
        }
        Ok(())
    }

    fn should_accept_op(&self, op: &SyncOpWire) -> Result<bool> {
        let identity = self.ensure_identity()?;
        if op.origin_device_id == identity.device_id {
            return Ok(true);
        }
        let conn = lock(&self.conn);
        let revoked: Option<(Option<i64>,)> = conn
            .query_row(
                "SELECT revoke_cutoff_seq
                 FROM sync_devices
                 WHERE device_id = ?1 AND revoked_at_ms IS NOT NULL",
                [&op.origin_device_id],
                |row| Ok((row.get(0)?,)),
            )
            .optional()?;
        if let Some((cutoff,)) = revoked {
            return Ok(op.seq <= cutoff.unwrap_or(0));
        }
        let known: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM sync_devices
                 WHERE device_id = ?1 AND trusted_at_ms IS NOT NULL",
                [&op.origin_device_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(known.is_some())
    }

    fn apply_op(&self, op: &SyncOpWire) -> Result<bool> {
        let changed = match &op.payload {
            SyncOpPayload::TrackLikeSet {
                content_id,
                liked,
                fed,
            } => self.apply_like_state(content_id, *liked, fed.as_ref(), op.hlc_ms, &op.op_id)?,
            SyncOpPayload::PlaylistCreated { playlist_id, title } => {
                self.apply_playlist_state(playlist_id, title, false, op.hlc_ms, &op.op_id)?
            }
            SyncOpPayload::PlaylistRenamed { playlist_id, title } => {
                self.apply_playlist_state(playlist_id, title, false, op.hlc_ms, &op.op_id)?
            }
            SyncOpPayload::PlaylistDeleted { playlist_id } => {
                self.apply_playlist_state(playlist_id, "", true, op.hlc_ms, &op.op_id)?
            }
            SyncOpPayload::PlaylistTrackAdded {
                playlist_id,
                content_id,
                position,
                fed,
            } => self.apply_playlist_item_state(
                playlist_id,
                content_id,
                true,
                *position,
                fed.as_ref(),
                op.hlc_ms,
                &op.op_id,
            )?,
            SyncOpPayload::PlaylistTrackRemoved {
                playlist_id,
                content_id,
            } => self.apply_playlist_item_state(
                playlist_id,
                content_id,
                false,
                0,
                None,
                op.hlc_ms,
                &op.op_id,
            )?,
            SyncOpPayload::DeviceProfileSet {
                name,
                client_version,
                endpoint_ticket,
                endpoint_id,
            } => {
                let profile = DeviceProfileWire {
                    device_id: op.origin_device_id.clone(),
                    name: name.clone(),
                    client_version: client_version.clone(),
                    protocol_version: PROTOCOL_VERSION,
                    endpoint_id: endpoint_id.clone(),
                    endpoint_ticket: endpoint_ticket.clone(),
                    revoked: false,
                    revoke_cutoff_seq: None,
                    updated_at_ms: op.hlc_ms,
                };
                self.apply_device_profile(&profile, false)?;
                false
            }
            SyncOpPayload::DeviceTrusted { target_device_id } => {
                self.apply_device_trusted(target_device_id, op.hlc_ms)?
            }
            SyncOpPayload::DeviceRevoked {
                target_device_id,
                target_max_seq_seen,
            } => self.apply_device_revoked(
                target_device_id,
                op.hlc_ms,
                &op.origin_device_id,
                *target_max_seq_seen,
            )?,
            SyncOpPayload::PlaybackCommand {
                target_device_id,
                command,
            } => {
                self.apply_playback_command(target_device_id, command, &op.op_id)?;
                false
            }
        };
        Ok(changed)
    }

    fn apply_playback_command(
        &self,
        target_device_id: &str,
        command: &PlaybackCommand,
        op_id: &str,
    ) -> Result<()> {
        let identity = self.ensure_identity()?;
        if target_device_id != identity.device_id {
            return Ok(());
        }
        let inserted = {
            let conn = lock(&self.conn);
            conn.execute(
                "INSERT OR IGNORE INTO sync_playback_applied (op_id, applied_at_ms)
                 VALUES (?1, ?2)",
                params![op_id, now_ms()],
            )?
        };
        if inserted == 0 {
            return Ok(());
        }
        if let Some(tx) = lock(&self.event_tx).as_ref() {
            let _ = tx.send(AppEvent::PlaybackCommand(command.clone()));
        }
        Ok(())
    }

    fn apply_device_trusted(&self, target_device_id: &str, hlc_ms: i64) -> Result<bool> {
        let was_revoked = {
            let conn = lock(&self.conn);
            conn.query_row(
                "SELECT revoked_at_ms IS NOT NULL
                 FROM sync_devices
                 WHERE device_id = ?1",
                [target_device_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
                != 0
        };
        let conn = lock(&self.conn);
        conn.execute(
            "INSERT INTO sync_devices (device_id, trusted_at_ms, last_seen_ms)
             VALUES (?1, ?2, ?2)
             ON CONFLICT(device_id) DO UPDATE SET
                trusted_at_ms = MAX(COALESCE(sync_devices.trusted_at_ms, 0), excluded.trusted_at_ms),
                last_seen_ms = MAX(COALESCE(sync_devices.last_seen_ms, 0), excluded.last_seen_ms),
                revoked_at_ms = CASE
                    WHEN sync_devices.revoked_at_ms IS NOT NULL
                     AND sync_devices.revoked_at_ms <= excluded.trusted_at_ms
                    THEN NULL
                    ELSE sync_devices.revoked_at_ms
                END,
                revoked_by = CASE
                    WHEN sync_devices.revoked_at_ms IS NOT NULL
                     AND sync_devices.revoked_at_ms <= excluded.trusted_at_ms
                    THEN NULL
                    ELSE sync_devices.revoked_by
                END,
                revoke_cutoff_seq = CASE
                    WHEN sync_devices.revoked_at_ms IS NOT NULL
                     AND sync_devices.revoked_at_ms <= excluded.trusted_at_ms
                    THEN NULL
                    ELSE sync_devices.revoke_cutoff_seq
                END",
            params![target_device_id, hlc_ms],
        )?;
        Ok(was_revoked)
    }

    fn apply_device_revoked(
        &self,
        target_device_id: &str,
        hlc_ms: i64,
        revoked_by: &str,
        target_max_seq_seen: i64,
    ) -> Result<bool> {
        let was_active = {
            let conn = lock(&self.conn);
            conn.query_row(
                "SELECT trusted_at_ms IS NOT NULL AND revoked_at_ms IS NULL
                 FROM sync_devices
                 WHERE device_id = ?1",
                [target_device_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
                != 0
        };
        let conn = lock(&self.conn);
        conn.execute(
            "INSERT INTO sync_devices
                (device_id, revoked_at_ms, revoked_by, revoke_cutoff_seq)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(device_id) DO UPDATE SET
                revoked_at_ms = CASE
                    WHEN COALESCE(sync_devices.trusted_at_ms, 0) <= excluded.revoked_at_ms
                     AND COALESCE(sync_devices.revoked_at_ms, 0) <= excluded.revoked_at_ms
                    THEN excluded.revoked_at_ms
                    ELSE sync_devices.revoked_at_ms
                END,
                revoked_by = CASE
                    WHEN COALESCE(sync_devices.trusted_at_ms, 0) <= excluded.revoked_at_ms
                     AND COALESCE(sync_devices.revoked_at_ms, 0) <= excluded.revoked_at_ms
                    THEN excluded.revoked_by
                    ELSE sync_devices.revoked_by
                END,
                revoke_cutoff_seq = CASE
                    WHEN COALESCE(sync_devices.trusted_at_ms, 0) <= excluded.revoked_at_ms
                     AND COALESCE(sync_devices.revoked_at_ms, 0) <= excluded.revoked_at_ms
                    THEN excluded.revoke_cutoff_seq
                    ELSE sync_devices.revoke_cutoff_seq
                END",
            params![target_device_id, hlc_ms, revoked_by, target_max_seq_seen,],
        )?;
        Ok(was_active)
    }

    fn apply_like_state(
        &self,
        content_id: &str,
        liked: bool,
        fed: Option<&SyncedFedTrack>,
        hlc_ms: i64,
        op_id: &str,
    ) -> Result<bool> {
        let Some(content_id) = music_dht::normalize_content_id(content_id) else {
            return Ok(false);
        };
        let current = {
            let conn = lock(&self.conn);
            conn.query_row(
                "SELECT liked, hlc_ms, op_id
                     FROM sync_state_likes
                     WHERE content_id = ?1",
                [&content_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? != 0,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
        };
        let apply = current.as_ref().is_none_or(|(_, current_hlc, current_op)| {
            (hlc_ms, op_id) > (*current_hlc, current_op.as_str())
        });
        if apply {
            let conn = lock(&self.conn);
            conn.execute(
                "INSERT INTO sync_state_likes (content_id, liked, hlc_ms, op_id)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(content_id) DO UPDATE SET
                    liked = excluded.liked,
                    hlc_ms = excluded.hlc_ms,
                    op_id = excluded.op_id",
                params![content_id, i64::from(liked), hlc_ms, op_id],
            )?;
        }
        let effective_liked = if apply {
            liked
        } else {
            current
                .as_ref()
                .map(|(liked, _, _)| *liked)
                .unwrap_or(false)
        };
        let effective_hlc_ms = if apply {
            hlc_ms
        } else {
            current
                .as_ref()
                .map(|(_, current_hlc, _)| *current_hlc)
                .unwrap_or(hlc_ms)
        };
        let mut changed = apply;
        if let Some(track_id) = self.library.track_id_by_content_id(&content_id)? {
            changed |= self
                .library
                .set_synced_like(track_id, effective_liked, effective_hlc_ms)?;
            if effective_liked {
                changed |= self.library.remove_fed_like_by_content_id(&content_id)?;
            }
        } else if effective_liked {
            if let Some(fed) = fed {
                changed |= self
                    .library
                    .upsert_synced_fed_like(&fed.to_fed_track(), effective_hlc_ms)?;
            }
        } else if apply {
            changed |= self.library.remove_fed_like_by_content_id(&content_id)?;
        }
        Ok(changed)
    }

    fn repair_like_order_from_sync_state(&self) -> Result<()> {
        let rows = {
            let conn = lock(&self.conn);
            let mut stmt = conn.prepare(
                "SELECT content_id, hlc_ms
                 FROM sync_state_likes
                 WHERE liked = 1",
            )?;
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (content_id, hlc_ms) in rows {
            if let Some(track_id) = self.library.track_id_by_content_id(&content_id)? {
                let _ = self.library.set_synced_like(track_id, true, hlc_ms)?;
            } else if let Some(fed) = self.library.fed_like_by_content_id(&content_id)? {
                let _ = self.library.upsert_synced_fed_like(&fed, hlc_ms)?;
            }
        }
        Ok(())
    }

    fn apply_playlist_state(
        &self,
        playlist_id: &str,
        title: &str,
        deleted: bool,
        hlc_ms: i64,
        op_id: &str,
    ) -> Result<bool> {
        let apply = {
            let conn = lock(&self.conn);
            let current: Option<(i64, String)> = conn
                .query_row(
                    "SELECT hlc_ms, op_id FROM sync_state_playlists WHERE playlist_id = ?1",
                    [playlist_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            current.as_ref().is_none_or(|(current_hlc, current_op)| {
                (hlc_ms, op_id) > (*current_hlc, current_op.as_str())
            })
        };
        if !apply {
            return Ok(false);
        }
        {
            let conn = lock(&self.conn);
            conn.execute(
                "INSERT INTO sync_state_playlists
                    (playlist_id, title, deleted, hlc_ms, op_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(playlist_id) DO UPDATE SET
                    title = excluded.title,
                    deleted = excluded.deleted,
                    hlc_ms = excluded.hlc_ms,
                    op_id = excluded.op_id",
                params![playlist_id, title, i64::from(deleted), hlc_ms, op_id],
            )?;
        }
        if deleted {
            self.library.delete_playlist_by_sync_id(playlist_id)?;
        } else if !title.trim().is_empty() {
            self.library.upsert_synced_playlist(playlist_id, title)?;
        }
        Ok(true)
    }

    fn apply_playlist_item_state(
        &self,
        playlist_id: &str,
        content_id: &str,
        present: bool,
        position: i64,
        fed: Option<&SyncedFedTrack>,
        hlc_ms: i64,
        op_id: &str,
    ) -> Result<bool> {
        let current = {
            let conn = lock(&self.conn);
            conn.query_row(
                "SELECT present, position, hlc_ms, op_id
                 FROM sync_state_playlist_items
                 WHERE playlist_id = ?1 AND content_id = ?2",
                params![playlist_id, content_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? != 0,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
        };
        let apply = current
            .as_ref()
            .is_none_or(|(_, _, current_hlc, current_op)| {
                (hlc_ms, op_id) > (*current_hlc, current_op.as_str())
            });
        if apply {
            let conn = lock(&self.conn);
            conn.execute(
                "INSERT INTO sync_state_playlist_items
                    (playlist_id, content_id, present, position, hlc_ms, op_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(playlist_id, content_id) DO UPDATE SET
                    present = excluded.present,
                    position = excluded.position,
                    hlc_ms = excluded.hlc_ms,
                    op_id = excluded.op_id",
                params![
                    playlist_id,
                    content_id,
                    i64::from(present),
                    position,
                    hlc_ms,
                    op_id
                ],
            )?;
        }
        let (effective_present, effective_position) = if apply {
            (present, position)
        } else {
            current
                .as_ref()
                .map(|(present, position, _, _)| (*present, *position))
                .unwrap_or((present, position))
        };
        let mut visible_changed = apply;
        if effective_present {
            visible_changed |= self
                .library
                .add_content_id_to_synced_playlist(playlist_id, content_id)?;
            if let Some(fed) = fed {
                visible_changed |= self.library.upsert_fed_playlist_track(
                    playlist_id,
                    &fed.to_fed_track(),
                    effective_position,
                )?;
            } else if let Some(fed) = self.library.fed_like_by_content_id(content_id)? {
                visible_changed |= self.library.upsert_fed_playlist_track(
                    playlist_id,
                    &fed,
                    effective_position,
                )?;
            }
        } else if apply {
            self.library
                .remove_content_id_from_synced_playlist(playlist_id, content_id)?;
        }
        Ok(visible_changed)
    }

    fn apply_snapshot(&self, snapshot: SyncSnapshot) -> Result<()> {
        let mut changed = false;
        for like in snapshot.likes {
            changed |= self.apply_like_state(
                &like.content_id,
                true,
                like.fed.as_ref(),
                like.hlc_ms,
                &like.op_id,
            )?;
        }
        for like in snapshot.unlikes {
            changed |=
                self.apply_like_state(&like.content_id, false, None, like.hlc_ms, &like.op_id)?;
        }
        for playlist in snapshot.playlists {
            changed |= self.apply_playlist_state(
                &playlist.playlist_id,
                &playlist.title,
                false,
                playlist.hlc_ms,
                &playlist.op_id,
            )?;
            for item in playlist.items {
                changed |= self.apply_playlist_item_state(
                    &playlist.playlist_id,
                    &item.content_id,
                    true,
                    item.position,
                    item.fed.as_ref(),
                    item.hlc_ms,
                    &item.op_id,
                )?;
            }
        }
        for playlist in snapshot.deleted_playlists {
            changed |= self.apply_playlist_state(
                &playlist.playlist_id,
                "",
                true,
                playlist.hlc_ms,
                &playlist.op_id,
            )?;
        }
        for item in snapshot.removed_playlist_items {
            changed |= self.apply_playlist_item_state(
                &item.playlist_id,
                &item.content_id,
                false,
                0,
                None,
                item.hlc_ms,
                &item.op_id,
            )?;
        }
        if changed {
            self.notify_library_changed();
        }
        Ok(())
    }

    fn apply_device_profiles(&self, devices: &[DeviceProfileWire]) -> Result<()> {
        for profile in devices {
            self.apply_device_profile(profile, false)?;
        }
        Ok(())
    }

    fn apply_device_profile(&self, profile: &DeviceProfileWire, trusted: bool) -> Result<()> {
        let own = self.ensure_identity()?.device_id;
        if profile.device_id == own {
            return Ok(());
        }
        let trusted_at_ms = if trusted {
            now_ms()
        } else {
            profile.updated_at_ms
        };
        {
            let conn = lock(&self.conn);
            conn.execute(
                "INSERT INTO sync_devices
                    (device_id, name, client_version, protocol_version, endpoint_id,
                     endpoint_ticket, trusted_at_ms, last_seen_ms, revoked_at_ms,
                     revoke_cutoff_seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(device_id) DO UPDATE SET
                    name = excluded.name,
                    client_version = excluded.client_version,
                    protocol_version = excluded.protocol_version,
                    endpoint_id = excluded.endpoint_id,
                    endpoint_ticket = CASE
                        WHEN excluded.endpoint_ticket != '' THEN excluded.endpoint_ticket
                        ELSE sync_devices.endpoint_ticket
                    END,
                    trusted_at_ms = MAX(COALESCE(sync_devices.trusted_at_ms, 0), excluded.trusted_at_ms),
                    last_seen_ms = COALESCE(excluded.last_seen_ms, sync_devices.last_seen_ms),
                    revoked_at_ms = CASE
                        WHEN excluded.revoked_at_ms IS NOT NULL
                         AND COALESCE(sync_devices.trusted_at_ms, 0) <= excluded.revoked_at_ms
                         AND COALESCE(sync_devices.revoked_at_ms, 0) <= excluded.revoked_at_ms
                        THEN excluded.revoked_at_ms
                        ELSE sync_devices.revoked_at_ms
                    END,
                    revoke_cutoff_seq = CASE
                        WHEN excluded.revoked_at_ms IS NOT NULL
                         AND COALESCE(sync_devices.trusted_at_ms, 0) <= excluded.revoked_at_ms
                         AND COALESCE(sync_devices.revoked_at_ms, 0) <= excluded.revoked_at_ms
                        THEN excluded.revoke_cutoff_seq
                        ELSE sync_devices.revoke_cutoff_seq
                    END",
                params![
                    profile.device_id,
                    profile.name,
                    profile.client_version,
                    profile.protocol_version,
                    profile.endpoint_id,
                    profile.endpoint_ticket,
                    trusted_at_ms,
                    profile.updated_at_ms,
                    if profile.revoked { Some(profile.updated_at_ms) } else { None },
                    profile.revoke_cutoff_seq,
                ],
            )?;
        }
        if trusted {
            let _ = self.apply_device_trusted(&profile.device_id, trusted_at_ms)?;
        }
        Ok(())
    }

    fn mark_seen(&self, device_id: &str, endpoint_id: Option<String>) -> Result<()> {
        let conn = lock(&self.conn);
        conn.execute(
            "UPDATE sync_devices
             SET last_seen_ms = ?2,
                 endpoint_id = COALESCE(?3, endpoint_id)
             WHERE device_id = ?1",
            params![device_id, now_ms(), endpoint_id],
        )?;
        Ok(())
    }

    fn vector(&self) -> Result<BTreeMap<String, i64>> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare("SELECT device_id, max_seq FROM sync_vectors")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<BTreeMap<String, i64>>>()?)
    }

    fn note_peer_vector(&self, peer_device_id: &str, vector: &BTreeMap<String, i64>) -> Result<()> {
        let conn = lock(&self.conn);
        for (origin, seq) in vector {
            conn.execute(
                "INSERT INTO sync_peer_acks
                    (peer_device_id, origin_device_id, max_seq, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(peer_device_id, origin_device_id) DO UPDATE SET
                    max_seq = MAX(max_seq, excluded.max_seq),
                    updated_at_ms = excluded.updated_at_ms",
                params![peer_device_id, origin, seq, now_ms()],
            )?;
        }
        Ok(())
    }

    fn ops_for_peer(&self, peer_device_id: &str) -> Result<Vec<SyncOpWire>> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT o.op_id, o.origin_device_id, o.seq, o.hlc_ms, o.payload_json
             FROM sync_ops o
             LEFT JOIN sync_peer_acks a
               ON a.peer_device_id = ?1 AND a.origin_device_id = o.origin_device_id
             WHERE o.seq > COALESCE(a.max_seq, 0)
             ORDER BY o.hlc_ms, o.op_id
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![peer_device_id, MAX_OPS_PER_BATCH as i64], |row| {
            let payload_json: String = row.get(4)?;
            Ok(SyncOpWire {
                op_id: row.get(0)?,
                origin_device_id: row.get(1)?,
                seq: row.get(2)?,
                hlc_ms: row.get(3)?,
                payload: serde_json::from_str(&payload_json).map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        Box::new(err),
                    )
                })?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn snapshot(&self) -> Result<SyncSnapshot> {
        let like_rows = {
            let conn = lock(&self.conn);
            let mut likes_stmt = conn.prepare(
                "SELECT content_id, hlc_ms, op_id
                 FROM sync_state_likes
                 WHERE liked = 1
                 ORDER BY content_id",
            )?;
            likes_stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut likes = Vec::with_capacity(like_rows.len());
        for (content_id, hlc_ms, op_id) in like_rows {
            let fed = self
                .library
                .fed_like_by_content_id(&content_id)?
                .as_ref()
                .and_then(SyncedFedTrack::from_fed);
            likes.push(SnapshotLike {
                content_id,
                hlc_ms,
                op_id,
                fed,
            });
        }
        let unlikes = {
            let conn = lock(&self.conn);
            let mut stmt = conn.prepare(
                "SELECT content_id, hlc_ms, op_id
                 FROM sync_state_likes
                 WHERE liked = 0
                 ORDER BY content_id",
            )?;
            stmt.query_map([], |row| {
                Ok(SnapshotLikeTombstone {
                    content_id: row.get(0)?,
                    hlc_ms: row.get(1)?,
                    op_id: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };

        let conn = lock(&self.conn);
        let mut playlist_stmt = conn.prepare(
            "SELECT playlist_id, title, hlc_ms, op_id
             FROM sync_state_playlists
             WHERE deleted = 0
             ORDER BY title COLLATE NOCASE",
        )?;
        let playlist_rows = playlist_stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut playlists = Vec::new();
        for (playlist_id, title, hlc_ms, op_id) in playlist_rows {
            let mut item_stmt = conn.prepare(
                "SELECT content_id, position, hlc_ms, op_id
                 FROM sync_state_playlist_items
                 WHERE playlist_id = ?1 AND present = 1
                 ORDER BY position, content_id",
            )?;
            let item_rows = item_stmt
                .query_map([&playlist_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut items = Vec::new();
            for (content_id, position, hlc_ms, op_id) in item_rows {
                let fed_track = match self
                    .library
                    .fed_playlist_track_by_content_id(&playlist_id, &content_id)?
                {
                    Some(fed) => Some(fed),
                    None => self.library.fed_like_by_content_id(&content_id)?,
                };
                let fed = fed_track.as_ref().and_then(SyncedFedTrack::from_fed);
                items.push(SnapshotPlaylistItem {
                    content_id,
                    position,
                    hlc_ms,
                    op_id,
                    fed,
                });
            }
            playlists.push(SnapshotPlaylist {
                playlist_id,
                title,
                hlc_ms,
                op_id,
                items,
            });
        }
        let deleted_playlists = {
            let mut stmt = conn.prepare(
                "SELECT playlist_id, hlc_ms, op_id
                 FROM sync_state_playlists
                 WHERE deleted = 1
                 ORDER BY playlist_id",
            )?;
            stmt.query_map([], |row| {
                Ok(SnapshotPlaylistTombstone {
                    playlist_id: row.get(0)?,
                    hlc_ms: row.get(1)?,
                    op_id: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let removed_playlist_items = {
            let mut stmt = conn.prepare(
                "SELECT playlist_id, content_id, hlc_ms, op_id
                 FROM sync_state_playlist_items
                 WHERE present = 0
                 ORDER BY playlist_id, content_id",
            )?;
            stmt.query_map([], |row| {
                Ok(SnapshotPlaylistItemTombstone {
                    playlist_id: row.get(0)?,
                    content_id: row.get(1)?,
                    hlc_ms: row.get(2)?,
                    op_id: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(SyncSnapshot {
            likes,
            unlikes,
            playlists,
            deleted_playlists,
            removed_playlist_items,
        })
    }

    fn device_profiles(&self) -> Result<Vec<DeviceProfileWire>> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT device_id, name, client_version, protocol_version, endpoint_id,
                    endpoint_ticket, revoked_at_ms IS NOT NULL, revoke_cutoff_seq,
                    COALESCE(last_seen_ms, trusted_at_ms, 0)
             FROM sync_devices
             WHERE trusted_at_ms IS NOT NULL
             ORDER BY device_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DeviceProfileWire {
                device_id: row.get(0)?,
                name: row.get(1)?,
                client_version: row.get(2)?,
                protocol_version: row.get(3)?,
                endpoint_id: row.get(4)?,
                endpoint_ticket: row.get(5)?,
                revoked: row.get::<_, i64>(6)? != 0,
                revoke_cutoff_seq: row.get(7)?,
                updated_at_ms: row.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn active_remote_devices(&self) -> Result<Vec<StoredDevice>> {
        let own = self.ensure_identity()?.device_id;
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT device_id, endpoint_ticket
             FROM sync_devices
             WHERE trusted_at_ms IS NOT NULL
               AND revoked_at_ms IS NULL
               AND device_id != ?1
             ORDER BY last_seen_ms DESC",
        )?;
        let rows = stmt.query_map([own], |row| {
            Ok(StoredDevice {
                device_id: row.get(0)?,
                endpoint_ticket: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn active_remote_endpoint_ids(&self) -> Result<Vec<String>> {
        let own = self.ensure_identity()?.device_id;
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT endpoint_id
             FROM sync_devices
             WHERE trusted_at_ms IS NOT NULL
               AND revoked_at_ms IS NULL
               AND device_id != ?1
               AND endpoint_id != ''
             ORDER BY last_seen_ms DESC",
        )?;
        let rows = stmt.query_map([own], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn active_device_count(&self) -> Result<usize> {
        let conn = lock(&self.conn);
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM sync_devices
             WHERE trusted_at_ms IS NOT NULL AND revoked_at_ms IS NULL",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize)
    }

    fn status_inner(&self) -> Result<DeviceSyncStatus> {
        let identity = self.ensure_identity()?;
        let conn = lock(&self.conn);
        let active_devices: usize = conn.query_row(
            "SELECT COUNT(*) FROM sync_devices
             WHERE trusted_at_ms IS NOT NULL AND revoked_at_ms IS NULL",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let revoked_devices: usize = conn.query_row(
            "SELECT COUNT(*) FROM sync_devices WHERE revoked_at_ms IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let pending_requests: usize = conn.query_row(
            "SELECT COUNT(*) FROM sync_pending_pairing WHERE status = 'pending'",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let ops_total: usize = conn.query_row("SELECT COUNT(*) FROM sync_ops", [], |row| {
            row.get::<_, i64>(0)
        })? as usize;
        let tombstone_ops: usize = conn.query_row(
            "SELECT COUNT(*) FROM sync_ops WHERE tombstone = 1",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let compactable_tombstones = self.compactable_tombstone_count(&conn)?;
        let outbox_ops: usize = conn.query_row(
            "SELECT COUNT(*)
             FROM sync_ops o
             WHERE EXISTS (
                SELECT 1 FROM sync_devices d
                LEFT JOIN sync_peer_acks a
                  ON a.peer_device_id = d.device_id
                 AND a.origin_device_id = o.origin_device_id
                WHERE d.trusted_at_ms IS NOT NULL
                  AND d.revoked_at_ms IS NULL
                  AND d.device_id != ?1
                  AND o.seq > COALESCE(a.max_seq, 0)
             )",
            [&identity.device_id],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let snapshot_likes: usize = conn.query_row(
            "SELECT COUNT(*) FROM sync_state_likes WHERE liked = 1",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let snapshot_playlists: usize = conn.query_row(
            "SELECT COUNT(*) FROM sync_state_playlists WHERE deleted = 0",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let snapshot_items: usize = conn.query_row(
            "SELECT COUNT(*) FROM sync_state_playlist_items WHERE present = 1",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let unresolved_playlist_items =
            self.unresolved_playlist_item_count_with_conn(&conn)? as usize;
        let peer_ack_floor = peer_ack_floor_label(&conn)?;
        let last_sync = get_meta(&conn, "last_sync")?;
        let last_error = get_meta(&conn, "last_error")?;
        let mut stmt = conn.prepare(
            "SELECT device_id, name, client_version, endpoint_id, last_seen_ms,
                    revoked_at_ms IS NOT NULL
             FROM sync_devices
             WHERE trusted_at_ms IS NOT NULL
             ORDER BY revoked_at_ms IS NOT NULL, name COLLATE NOCASE, device_id",
        )?;
        let devices = stmt
            .query_map([], |row| {
                let device_id: String = row.get(0)?;
                Ok(DeviceStatusRow {
                    is_self: device_id == identity.device_id,
                    device_id,
                    name: row.get(1)?,
                    client_version: row.get(2)?,
                    endpoint_id: row.get(3)?,
                    last_seen_ms: row.get(4)?,
                    revoked: row.get::<_, i64>(5)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(DeviceSyncStatus {
            this_device_id: identity.device_id,
            this_device_name: identity.name,
            group_id: identity.group_id,
            active_devices,
            revoked_devices,
            pending_requests,
            ops_total,
            tombstone_ops,
            compactable_tombstones,
            outbox_ops,
            snapshot_likes,
            snapshot_playlists,
            snapshot_items,
            unresolved_playlist_items,
            peer_ack_floor,
            last_sync,
            last_error,
            devices,
        })
    }

    fn compactable_tombstone_count(&self, conn: &Connection) -> Result<usize> {
        let rows = compactable_tombstone_ids(conn)?;
        Ok(rows.len())
    }

    fn unresolved_playlist_item_count_with_conn(&self, conn: &Connection) -> Result<i64> {
        let mut stmt = conn.prepare(
            "SELECT playlist_id, content_id
             FROM sync_state_playlist_items
             WHERE present = 1",
        )?;
        let ids = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let mut unresolved = 0;
        for (playlist_id, content_id) in ids {
            if !self
                .library
                .has_playlist_content_reference(&playlist_id, &content_id)?
            {
                unresolved += 1;
            }
        }
        Ok(unresolved)
    }

    fn set_last_sync(&self, message: Option<String>) -> Result<()> {
        let conn = lock(&self.conn);
        match message {
            Some(message) => set_meta(&conn, "last_sync", &format!("{} · {message}", now_label())),
            None => delete_meta(&conn, "last_sync"),
        }
    }

    fn set_last_error(&self, message: Option<String>) -> Result<()> {
        let conn = lock(&self.conn);
        match message {
            Some(message) => set_meta(&conn, "last_error", &message),
            None => delete_meta(&conn, "last_error"),
        }
    }

    fn local_playback_snapshot(&self) -> Option<PlaybackSnapshot> {
        lock(&self.playback).local.clone()
    }

    fn apply_playback_snapshot(&self, snapshot: PlaybackSnapshot) -> Result<()> {
        let identity = self.ensure_identity()?;
        if snapshot.device_id == identity.device_id {
            return Ok(());
        }
        let should_send = {
            let mut playback = lock(&self.playback);
            let changed = playback
                .remote
                .get(&snapshot.device_id)
                .is_none_or(|current| snapshot.updated_at_ms > current.updated_at_ms);
            if changed {
                playback
                    .remote
                    .insert(snapshot.device_id.clone(), snapshot.clone());
            }
            changed
        };
        if should_send && let Some(tx) = lock(&self.event_tx).as_ref() {
            let _ = tx.send(AppEvent::DevicePlayback(snapshot));
        }
        Ok(())
    }

    fn notify_library_changed(&self) {
        if let Some(tx) = lock(&self.event_tx).as_ref() {
            let _ = tx.send(AppEvent::LibraryChanged { message: None });
            let _ = tx.send(AppEvent::DeviceSyncStatus(self.status()));
        }
    }

    fn gc_tombstones(&self) -> Result<()> {
        let conn = lock(&self.conn);
        let compactable = compactable_tombstone_ids(&conn)?;
        for (op_id, origin, seq) in compactable {
            let revoked_target = tombstone_revoke_target(&conn, &op_id)?;
            conn.execute(
                "INSERT INTO sync_compacted (origin_device_id, max_seq)
                 VALUES (?1, ?2)
                 ON CONFLICT(origin_device_id) DO UPDATE SET
                    max_seq = MAX(max_seq, excluded.max_seq)",
                params![origin, seq],
            )?;
            conn.execute("DELETE FROM sync_ops WHERE op_id = ?1", [op_id])?;
            if let Some(device_id) = revoked_target {
                delete_revoked_device_if_fully_compacted(&conn, &device_id)?;
            }
        }
        conn.execute(
            "DELETE FROM sync_state_likes
             WHERE liked = 0
               AND EXISTS (
                    SELECT 1 FROM sync_compacted c
                    WHERE c.origin_device_id = substr(sync_state_likes.op_id, 1, instr(sync_state_likes.op_id, ':') - 1)
               )",
            [],
        )?;
        Ok(())
    }
}

#[derive(Debug)]
struct StoredDevice {
    device_id: String,
    endpoint_ticket: String,
}

pub async fn serve_peers(
    mut acceptor: StreamAcceptor,
    sync: Arc<DeviceSync>,
    service: Arc<MusicDhtService>,
    transport_stats: Arc<crate::federation::TransportStats>,
) {
    while let Some(stream) = acceptor.accept().await {
        let sync = Arc::clone(&sync);
        let service = Arc::clone(&service);
        let transport_stats = Arc::clone(&transport_stats);
        tokio::spawn(async move {
            let peer = stream.peer_id;
            if let Err(err) = serve_one(stream, sync, service, transport_stats).await {
                tracing::warn!(peer = %peer, "personal sync stream failed: {err:#}");
            }
        });
    }
}

pub async fn sync_loop(
    sync: Arc<DeviceSync>,
    service: Arc<MusicDhtService>,
    transport_stats: Arc<crate::federation::TransportStats>,
) {
    let mut interval = tokio::time::interval(SYNC_INTERVAL);
    loop {
        interval.tick().await;
        if let Err(err) = sync
            .sync_once(Arc::clone(&service), Arc::clone(&transport_stats))
            .await
        {
            tracing::debug!("personal sync tick failed: {err:#}");
        }
        if let Some(tx) = lock(&sync.event_tx).as_ref() {
            let _ = tx.send(AppEvent::DeviceSyncStatus(sync.status()));
        }
    }
}

async fn serve_one(
    mut stream: ByteStream,
    sync: Arc<DeviceSync>,
    service: Arc<MusicDhtService>,
    transport_stats: Arc<crate::federation::TransportStats>,
) -> Result<()> {
    crate::federation::record_stream_transport(
        &transport_stats,
        "device-sync",
        "inbound",
        "open",
        &stream,
    );
    match read_msg(&mut stream).await? {
        WireMessage::PairRequest {
            invite_id,
            secret,
            profile,
            group_id,
            group_active_devices,
            devices,
            vector,
            ops,
            snapshot,
            playback,
        } => {
            handle_pair_request(
                stream,
                sync,
                service,
                invite_id,
                secret,
                profile,
                group_id,
                group_active_devices,
                devices,
                vector,
                ops,
                snapshot,
                playback,
            )
            .await
        }
        WireMessage::Hello {
            group_id,
            profile,
            devices,
            vector,
            ops,
            snapshot,
            playback,
        } => {
            handle_hello(
                stream, sync, service, group_id, profile, devices, vector, ops, snapshot, playback,
            )
            .await
        }
        _ => anyhow::bail!("unexpected first message"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_pair_request(
    mut stream: ByteStream,
    sync: Arc<DeviceSync>,
    service: Arc<MusicDhtService>,
    invite_id: String,
    secret: String,
    mut profile: DeviceProfileWire,
    requester_group_id: Option<String>,
    requester_group_active_devices: usize,
    requester_group_devices: Vec<DeviceProfileWire>,
    vector: BTreeMap<String, i64>,
    ops: Vec<SyncOpWire>,
    snapshot: SyncSnapshot,
    playback: Option<PlaybackSnapshot>,
) -> Result<()> {
    profile.endpoint_id = stream.peer_id.to_string();
    let request_id = pair_request_id(&invite_id, &profile.device_id);
    if !valid_pair_request(&sync, &invite_id, &secret, &request_id)? {
        tracing::warn!(
            peer = %stream.peer_id,
            invite_id,
            "ignored pairing request with invalid invite secret"
        );
        write_msg(
            &mut stream,
            &WireMessage::PairResponse {
                accepted: false,
                pending: false,
                error: Some("invalid or expired invite".to_string()),
                group_id: None,
                profile: None,
                devices: Vec::new(),
                vector: BTreeMap::new(),
                ops: Vec::new(),
                snapshot: SyncSnapshot::default(),
                playback: None,
            },
        )
        .await?;
        finish_response(&mut stream).await?;
        return Ok(());
    }
    let identity = sync.ensure_identity()?;
    let requester_group_id = requester_group_id.filter(|id| !id.trim().is_empty());
    let requester_group_active_devices = requester_group_active_devices.max(1);
    let requester_group_conflict = requester_group_id
        .as_deref()
        .is_some_and(|group_id| group_id != identity.group_id)
        && requester_group_active_devices > 1;
    let requester_group_devices_json = serde_json::to_string(&requester_group_devices)?;
    let inserted = {
        let conn = lock(&sync.conn);
        conn.execute(
            "INSERT OR IGNORE INTO sync_pending_pairing
                (request_id, device_id, name, client_version, endpoint_id,
                 endpoint_ticket, invite_id, created_at_ms, status,
                 requester_group_id, requester_group_active_devices,
                 requester_group_devices_json, use_requester_group)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending',
                     ?9, ?10, ?11, 0)",
            params![
                request_id,
                profile.device_id,
                profile.name,
                profile.client_version,
                profile.endpoint_id,
                profile.endpoint_ticket,
                invite_id,
                now_ms(),
                requester_group_id.as_deref(),
                requester_group_active_devices as i64,
                requester_group_devices_json,
            ],
        )?
    };
    if inserted > 0
        && let Some(tx) = lock(&sync.event_tx).as_ref()
    {
        let _ = tx.send(AppEvent::DevicePairingRequest(PendingPairing {
            request_id: request_id.clone(),
            device_id: profile.device_id.clone(),
            name: profile.name.clone(),
            client_version: profile.client_version.clone(),
            requester_group_id: requester_group_conflict
                .then(|| requester_group_id.clone())
                .flatten(),
            requester_group_active_devices: requester_group_conflict
                .then_some(requester_group_active_devices)
                .unwrap_or(0),
        }));
    }
    let pairing = pairing_status(&sync, &request_id)?;
    match pairing.as_ref().map(|status| status.status.as_str()) {
        Some("pending") => {
            write_msg(
                &mut stream,
                &WireMessage::PairResponse {
                    accepted: false,
                    pending: true,
                    error: Some("pairing pending".to_string()),
                    group_id: None,
                    profile: None,
                    devices: Vec::new(),
                    vector: BTreeMap::new(),
                    ops: Vec::new(),
                    snapshot: SyncSnapshot::default(),
                    playback: None,
                },
            )
            .await?;
            finish_response(&mut stream).await?;
            return Ok(());
        }
        Some("accepted") => {}
        _ => {
            write_msg(
                &mut stream,
                &WireMessage::PairResponse {
                    accepted: false,
                    pending: false,
                    error: Some("pairing denied".to_string()),
                    group_id: None,
                    profile: None,
                    devices: Vec::new(),
                    vector: BTreeMap::new(),
                    ops: Vec::new(),
                    snapshot: SyncSnapshot::default(),
                    playback: None,
                },
            )
            .await?;
            finish_response(&mut stream).await?;
            return Ok(());
        }
    }

    let use_requester_group = pairing
        .as_ref()
        .is_some_and(|status| status.use_requester_group);
    let own_ticket = service.ticket().await?.to_string();
    let own_profile = sync.own_profile(&own_ticket)?;
    let mut response_group_id = identity.group_id;
    if use_requester_group
        && let Some(group_id) = requester_group_id
            .as_deref()
            .filter(|group_id| !group_id.trim().is_empty())
    {
        sync.set_group_id(group_id)?;
        response_group_id = group_id.to_string();
        sync.apply_device_profiles(&requester_group_devices)?;
    }
    sync.apply_device_profile(&profile, true)?;
    if let Some(playback) = playback {
        sync.apply_playback_snapshot(playback)?;
    }
    sync.apply_snapshot(snapshot)?;
    sync.apply_ops(ops)?;
    sync.note_peer_vector(&profile.device_id, &vector)?;
    {
        let conn = lock(&sync.conn);
        conn.execute(
            "UPDATE sync_invites SET used_at_ms = ?2 WHERE invite_id = ?1",
            params![invite_id, now_ms()],
        )?;
    }
    sync.set_last_sync(Some(format!("paired {}", short_id(&profile.device_id))))?;
    let devices = sync.device_profiles()?;
    let vector = sync.vector()?;
    let ops = sync.ops_for_peer(&profile.device_id)?;
    let snapshot = sync.snapshot()?;
    let playback = sync.local_playback_snapshot();
    write_msg(
        &mut stream,
        &WireMessage::PairResponse {
            accepted: true,
            pending: false,
            error: None,
            group_id: Some(response_group_id),
            profile: Some(own_profile),
            devices,
            vector,
            ops,
            snapshot,
            playback,
        },
    )
    .await?;
    finish_response(&mut stream).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_hello(
    mut stream: ByteStream,
    sync: Arc<DeviceSync>,
    service: Arc<MusicDhtService>,
    group_id: String,
    mut profile: DeviceProfileWire,
    devices: Vec<DeviceProfileWire>,
    vector: BTreeMap<String, i64>,
    ops: Vec<SyncOpWire>,
    snapshot: SyncSnapshot,
    playback: Option<PlaybackSnapshot>,
) -> Result<()> {
    let identity = sync.ensure_identity()?;
    if group_id != identity.group_id {
        write_msg(
            &mut stream,
            &WireMessage::SyncResponse {
                accepted: false,
                error: Some("sync group mismatch".to_string()),
                devices: Vec::new(),
                vector: BTreeMap::new(),
                ops: Vec::new(),
                snapshot: SyncSnapshot::default(),
                playback: None,
            },
        )
        .await?;
        finish_response(&mut stream).await?;
        return Ok(());
    }
    if !is_active_trusted(&sync, &profile.device_id)? {
        write_msg(
            &mut stream,
            &WireMessage::SyncResponse {
                accepted: false,
                error: Some("device is not trusted".to_string()),
                devices: Vec::new(),
                vector: BTreeMap::new(),
                ops: Vec::new(),
                snapshot: SyncSnapshot::default(),
                playback: None,
            },
        )
        .await?;
        finish_response(&mut stream).await?;
        return Ok(());
    }
    profile.endpoint_id = stream.peer_id.to_string();
    sync.apply_device_profile(&profile, false)?;
    sync.apply_device_profiles(&devices)?;
    if let Some(playback) = playback {
        sync.apply_playback_snapshot(playback)?;
    }
    sync.apply_snapshot(snapshot)?;
    sync.apply_ops(ops)?;
    sync.note_peer_vector(&profile.device_id, &vector)?;
    sync.mark_seen(&profile.device_id, Some(stream.peer_id.to_string()))?;
    sync.set_last_sync(Some(format!("synced {}", short_id(&profile.device_id))))?;

    let own_ticket = service.ticket().await?.to_string();
    let own_profile = sync.own_profile(&own_ticket)?;
    let devices = sync.device_profiles()?;
    let vector = sync.vector()?;
    let ops = sync.ops_for_peer(&profile.device_id)?;
    let snapshot = sync.snapshot()?;
    let playback = sync.local_playback_snapshot();
    write_msg(
        &mut stream,
        &WireMessage::SyncResponse {
            accepted: true,
            error: None,
            devices: {
                let mut profiles = devices;
                profiles.push(own_profile);
                profiles
            },
            vector,
            ops,
            snapshot,
            playback,
        },
    )
    .await?;
    finish_response(&mut stream).await?;
    sync.gc_tombstones()?;
    Ok(())
}

fn pairing_status(sync: &DeviceSync, request_id: &str) -> Result<Option<PairingStatus>> {
    let conn = lock(&sync.conn);
    Ok(conn
        .query_row(
            "SELECT status, use_requester_group
             FROM sync_pending_pairing
             WHERE request_id = ?1",
            [request_id],
            |row| {
                Ok(PairingStatus {
                    status: row.get(0)?,
                    use_requester_group: row.get::<_, i64>(1)? != 0,
                })
            },
        )
        .optional()?)
}

fn pair_request_id(invite_id: &str, device_id: &str) -> String {
    let digest = blake3::hash(format!("{invite_id}:{device_id}").as_bytes());
    format!("pair_{}", &digest.to_hex()[..16])
}

fn valid_pair_request(
    sync: &DeviceSync,
    invite_id: &str,
    secret: &str,
    request_id: &str,
) -> Result<bool> {
    let conn = lock(&sync.conn);
    let expected: Option<(String, i64, Option<i64>)> = conn
        .query_row(
            "SELECT secret_hash, expires_at_ms, used_at_ms
             FROM sync_invites
             WHERE invite_id = ?1",
            [invite_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((secret_hash, expires_at_ms, used_at_ms)) = expected else {
        return Ok(false);
    };
    if expires_at_ms < now_ms() || secret_hash != hash_secret(secret) {
        return Ok(false);
    }
    if used_at_ms.is_none() {
        return Ok(true);
    }
    let accepted: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sync_pending_pairing
             WHERE request_id = ?1 AND status = 'accepted'",
            [request_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(accepted.is_some())
}

fn is_active_trusted(sync: &DeviceSync, device_id: &str) -> Result<bool> {
    let own = sync.ensure_identity()?.device_id;
    if device_id == own {
        return Ok(true);
    }
    let conn = lock(&sync.conn);
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sync_devices
             WHERE device_id = ?1
               AND trusted_at_ms IS NOT NULL
               AND revoked_at_ms IS NULL",
            [device_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS sync_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sync_devices (
    device_id         TEXT PRIMARY KEY,
    name              TEXT NOT NULL DEFAULT '',
    client_version    TEXT NOT NULL DEFAULT '',
    protocol_version  INTEGER NOT NULL DEFAULT 1,
    endpoint_id       TEXT NOT NULL DEFAULT '',
    endpoint_ticket   TEXT NOT NULL DEFAULT '',
    trusted_at_ms     INTEGER,
    last_seen_ms      INTEGER,
    revoked_at_ms     INTEGER,
    revoked_by        TEXT,
    revoke_cutoff_seq INTEGER
);
CREATE TABLE IF NOT EXISTS sync_invites (
    invite_id     TEXT PRIMARY KEY,
    secret_hash   TEXT NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    used_at_ms    INTEGER
);
CREATE TABLE IF NOT EXISTS sync_pending_pairing (
    request_id                     TEXT PRIMARY KEY,
    device_id                      TEXT NOT NULL,
    name                           TEXT NOT NULL,
    client_version                 TEXT NOT NULL,
    endpoint_id                    TEXT NOT NULL,
    endpoint_ticket                TEXT NOT NULL,
    invite_id                      TEXT NOT NULL,
    created_at_ms                  INTEGER NOT NULL,
    answered_at_ms                 INTEGER,
    status                         TEXT NOT NULL,
    requester_group_id             TEXT,
    requester_group_active_devices INTEGER NOT NULL DEFAULT 1,
    requester_group_devices_json   TEXT NOT NULL DEFAULT '[]',
    use_requester_group            INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS sync_ops (
    op_id            TEXT PRIMARY KEY,
    origin_device_id TEXT NOT NULL,
    seq              INTEGER NOT NULL,
    kind             TEXT NOT NULL,
    payload_json     TEXT NOT NULL,
    hlc_ms           INTEGER NOT NULL,
    received_at_ms   INTEGER NOT NULL,
    tombstone        INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_sync_ops_origin_seq ON sync_ops(origin_device_id, seq);
CREATE INDEX IF NOT EXISTS idx_sync_ops_tombstone ON sync_ops(tombstone);
CREATE TABLE IF NOT EXISTS sync_vectors (
    device_id TEXT PRIMARY KEY,
    max_seq   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sync_peer_acks (
    peer_device_id   TEXT NOT NULL,
    origin_device_id TEXT NOT NULL,
    max_seq          INTEGER NOT NULL,
    updated_at_ms    INTEGER NOT NULL,
    PRIMARY KEY (peer_device_id, origin_device_id)
);
CREATE TABLE IF NOT EXISTS sync_compacted (
    origin_device_id TEXT PRIMARY KEY,
    max_seq          INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sync_state_likes (
    content_id TEXT PRIMARY KEY,
    liked      INTEGER NOT NULL,
    hlc_ms     INTEGER NOT NULL,
    op_id      TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sync_state_playlists (
    playlist_id TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    deleted     INTEGER NOT NULL DEFAULT 0,
    hlc_ms      INTEGER NOT NULL,
    op_id       TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sync_state_playlist_items (
    playlist_id TEXT NOT NULL,
    content_id  TEXT NOT NULL,
    present     INTEGER NOT NULL DEFAULT 1,
    position    INTEGER NOT NULL DEFAULT 0,
    hlc_ms      INTEGER NOT NULL,
    op_id       TEXT NOT NULL,
    PRIMARY KEY (playlist_id, content_id)
);
CREATE TABLE IF NOT EXISTS sync_playback_applied (
    op_id         TEXT PRIMARY KEY,
    applied_at_ms INTEGER NOT NULL
);
    "#,
    )?;
    ensure_column(
        conn,
        "sync_pending_pairing",
        "requester_group_id",
        "ALTER TABLE sync_pending_pairing ADD COLUMN requester_group_id TEXT",
    )?;
    ensure_column(
        conn,
        "sync_pending_pairing",
        "requester_group_active_devices",
        "ALTER TABLE sync_pending_pairing
         ADD COLUMN requester_group_active_devices INTEGER NOT NULL DEFAULT 1",
    )?;
    ensure_column(
        conn,
        "sync_pending_pairing",
        "requester_group_devices_json",
        "ALTER TABLE sync_pending_pairing
         ADD COLUMN requester_group_devices_json TEXT NOT NULL DEFAULT '[]'",
    )?;
    ensure_column(
        conn,
        "sync_pending_pairing",
        "use_requester_group",
        "ALTER TABLE sync_pending_pairing
         ADD COLUMN use_requester_group INTEGER NOT NULL DEFAULT 0",
    )?;
    Ok(())
}

fn ensure_column(conn: &Connection, table: &str, column: &str, ddl: &str) -> Result<()> {
    if table_has_column(conn, table, column)? {
        return Ok(());
    }
    conn.execute(ddl, [])?;
    Ok(())
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM sync_meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn delete_meta(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM sync_meta WHERE key = ?1", [key])?;
    Ok(())
}

fn payload_kind(payload: &SyncOpPayload) -> &'static str {
    match payload {
        SyncOpPayload::TrackLikeSet { .. } => "track_like_set",
        SyncOpPayload::PlaylistCreated { .. } => "playlist_created",
        SyncOpPayload::PlaylistRenamed { .. } => "playlist_renamed",
        SyncOpPayload::PlaylistDeleted { .. } => "playlist_deleted",
        SyncOpPayload::PlaylistTrackAdded { .. } => "playlist_track_added",
        SyncOpPayload::PlaylistTrackRemoved { .. } => "playlist_track_removed",
        SyncOpPayload::DeviceProfileSet { .. } => "device_profile_set",
        SyncOpPayload::DeviceTrusted { .. } => "device_trusted",
        SyncOpPayload::DeviceRevoked { .. } => "device_revoked",
        SyncOpPayload::PlaybackCommand { .. } => "playback_command",
    }
}

fn compactable_tombstone_ids(conn: &Connection) -> Result<Vec<(String, String, i64)>> {
    let own_device_id = get_meta(conn, "device_id")?.unwrap_or_default();
    let active_devices = conn
        .prepare(
            "SELECT device_id FROM sync_devices
             WHERE trusted_at_ms IS NOT NULL AND revoked_at_ms IS NULL",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if active_devices.len() <= 1 {
        let mut stmt = conn.prepare(
            "SELECT op_id, origin_device_id, seq
             FROM sync_ops
             WHERE tombstone = 1",
        )?;
        return Ok(stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?);
    }
    let mut stmt = conn.prepare(
        "SELECT op_id, origin_device_id, seq
         FROM sync_ops
         WHERE tombstone = 1
         ORDER BY received_at_ms",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for (op_id, origin, seq) in rows {
        let local_vector: i64 = conn
            .query_row(
                "SELECT max_seq FROM sync_vectors
                 WHERE device_id = ?1",
                [&origin],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let mut all_acked = true;
        for device in &active_devices {
            let seen = if device == &own_device_id {
                local_vector
            } else if device == &origin {
                seq
            } else {
                conn.query_row(
                    "SELECT max_seq FROM sync_peer_acks
                     WHERE peer_device_id = ?1 AND origin_device_id = ?2",
                    params![device, origin],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0)
            };
            if seen < seq {
                all_acked = false;
                break;
            }
        }
        if all_acked {
            out.push((op_id, origin, seq));
        }
    }
    Ok(out)
}

fn tombstone_revoke_target(conn: &Connection, op_id: &str) -> Result<Option<String>> {
    let payload_json: Option<String> = conn
        .query_row(
            "SELECT payload_json FROM sync_ops WHERE op_id = ?1 AND kind = 'device_revoked'",
            [op_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(payload_json) = payload_json else {
        return Ok(None);
    };
    let payload: SyncOpPayload = serde_json::from_str(&payload_json)?;
    Ok(match payload {
        SyncOpPayload::DeviceRevoked {
            target_device_id, ..
        } => Some(target_device_id),
        _ => None,
    })
}

fn has_revoke_op_for_target(conn: &Connection, device_id: &str) -> Result<bool> {
    let mut stmt =
        conn.prepare("SELECT payload_json FROM sync_ops WHERE kind = 'device_revoked'")?;
    let payloads = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for payload_json in payloads {
        let Ok(SyncOpPayload::DeviceRevoked {
            target_device_id, ..
        }) = serde_json::from_str::<SyncOpPayload>(&payload_json)
        else {
            continue;
        };
        if target_device_id == device_id {
            return Ok(true);
        }
    }
    Ok(false)
}

fn delete_revoked_device_if_fully_compacted(conn: &Connection, device_id: &str) -> Result<()> {
    let own_device_id = get_meta(conn, "device_id")?.unwrap_or_default();
    if device_id == own_device_id {
        return Ok(());
    }
    let origin_ops: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sync_ops WHERE origin_device_id = ?1",
        [device_id],
        |row| row.get(0),
    )?;
    if origin_ops > 0 || has_revoke_op_for_target(conn, device_id)? {
        return Ok(());
    }
    conn.execute(
        "DELETE FROM sync_devices
         WHERE device_id = ?1 AND revoked_at_ms IS NOT NULL",
        [device_id],
    )?;
    conn.execute(
        "DELETE FROM sync_peer_acks WHERE peer_device_id = ?1",
        [device_id],
    )?;
    Ok(())
}

fn peer_ack_floor_label(conn: &Connection) -> Result<String> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM sync_peer_acks", [], |row| row.get(0))?;
    if count == 0 {
        return Ok("none".to_string());
    }
    let min: i64 = conn.query_row(
        "SELECT COALESCE(MIN(max_seq), 0) FROM sync_peer_acks",
        [],
        |row| row.get(0),
    )?;
    let max: i64 = conn.query_row(
        "SELECT COALESCE(MAX(max_seq), 0) FROM sync_peer_acks",
        [],
        |row| row.get(0),
    )?;
    Ok(format!("{min}..{max} ({count} acks)"))
}

async fn write_msg(stream: &mut ByteStream, message: &WireMessage) -> Result<()> {
    let mut payload = serde_json::to_vec(message)?;
    payload.push(b'\n');
    stream.send.write_all(&payload).await?;
    Ok(())
}

async fn finish_send(stream: &mut ByteStream) -> Result<()> {
    stream.send.finish()?;
    Ok(())
}

async fn finish_response(stream: &mut ByteStream) -> Result<()> {
    stream.send.finish()?;
    let _ = tokio::time::timeout(RESPONSE_DRAIN_TIMEOUT, stream.send.stopped()).await;
    Ok(())
}

async fn read_msg(stream: &mut ByteStream) -> Result<WireMessage> {
    let line = read_line(&mut stream.recv).await?;
    Ok(serde_json::from_slice(&line)?)
}

async fn read_line<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let read = reader.read(&mut byte).await?;
        if read == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        out.push(byte[0]);
        if out.len() > MAX_LINE {
            anyhow::bail!("protocol line is too large");
        }
    }
    Ok(out)
}

fn parse_invite(value: &str) -> Result<InviteWire> {
    let Some(token) = value.trim().strip_prefix("frid://i/") else {
        anyhow::bail!("usage: :connect frid://i/<invite>");
    };
    let bytes = base64url_decode(token)?;
    let invite: InviteWire = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(invite.v == 1, "unsupported invite version");
    Ok(invite)
}

pub fn invite_network_id(value: &str) -> Result<NetworkId> {
    let invite = parse_invite(value)?;
    let ticket: PeerTicket = invite
        .ticket
        .parse()
        .map_err(|err| anyhow::anyhow!("malformed invite ticket: {err}"))?;
    Ok(ticket.network_id)
}

fn hash_secret(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}

fn random_hex(bytes: usize) -> String {
    let key = SecretKey::generate();
    let mut seed = key.to_bytes().to_vec();
    while seed.len() < bytes {
        seed.extend_from_slice(blake3::hash(&seed).as_bytes());
    }
    hex_encode(&seed[..bytes])
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn ticket_endpoint_id(ticket: &str) -> Option<String> {
    let ticket = PeerTicket::from_str(ticket).ok()?;
    Some(ticket.endpoint_id().to_string())
}

fn default_device_name(device_id: &str) -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "furumi".to_string());
    format!("{host}-{}", short_id(device_id))
}

fn short_id(value: &str) -> String {
    value.chars().take(10).collect()
}

fn base64url_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = bytes.get(i + 1).copied().unwrap_or(0);
        let b2 = bytes.get(i + 2).copied().unwrap_or(0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if i + 1 < bytes.len() {
            out.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        }
        if i + 2 < bytes.len() {
            out.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        }
        i += 3;
    }
    out
}

fn base64url_decode(value: &str) -> Result<Vec<u8>> {
    fn val(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let a = val(bytes[i]).context("invalid base64url invite")?;
        let b = val(*bytes.get(i + 1).context("truncated base64url invite")?)
            .context("invalid base64url invite")?;
        let c = bytes.get(i + 2).and_then(|byte| val(*byte));
        let d = bytes.get(i + 3).and_then(|byte| val(*byte));
        out.push((a << 2) | (b >> 4));
        if let Some(c) = c {
            out.push(((b & 0b0000_1111) << 4) | (c >> 2));
            if let Some(d) = d {
                out.push(((c & 0b0000_0011) << 6) | d);
            }
        }
        i += 4;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
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
}
