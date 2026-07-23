//! P2P federation for the TUI player.
//!
//! The player stays fully local, but can join a federated network of
//! furumi instances (TUI or furumi-fd): it publishes its library index
//! (names and small metadata, never files) into a shared DHT, searches the
//! other participants' libraries and streams their audio over the same
//! `furumi-fd/audio/1` protocol furumi-fd speaks — the two are wire
//! compatible.
//!
//! Federated tracks are downloaded before playback: into a cache file, or —
//! with "save on listen" enabled — straight into the local library (the
//! file is imported like any local file, so this peer then serves it to
//! the network too).

mod audio;
pub mod catalog;

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use music_dht::{
    EndpointId, ItemKind, ItemSpec, LibraryItem, MusicDhtConfig, MusicDhtService, NetworkId,
    PeerTicket, PublishStats, RendezvousConfig, SyncStats,
};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};

use crate::library::Library;
use crate::library::models::{ArtistRef, TrackItem};

pub use audio::{AUDIO_ALPN, TrackMetadata};
pub use catalog::{CATALOG_ALPN, FedAppearsOn, FedArtistCard, FedCardTrack, FedRelease};

/// How often the published library is re-synchronized with the local index.
const SYNC_INTERVAL: Duration = Duration::from_secs(60);

/// How many times a share-link content lookup is retried before the label
/// fallback kicks in.
const CONTENT_LOOKUP_ATTEMPTS: usize = 3;
/// Pause between share-link content lookup attempts.
const CONTENT_LOOKUP_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Ephemeral (not-in-library) tracks get negative ids so the rest of the
/// app can tell them apart from library rows (history, likes and release
/// navigation skip them).
static NEXT_EPHEMERAL_ID: AtomicI64 = AtomicI64::new(-1);

// ---------------------------------------------------------------------------
// Settings (persisted in <config dir>/federation.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FedSettings {
    pub enabled: bool,
    #[serde(default)]
    pub network_id: String,
    /// Downloaded-for-playback federated tracks are imported into the local
    /// library (replication) instead of a throwaway cache.
    #[serde(default)]
    pub save_on_listen: bool,
}

fn settings_path() -> Option<PathBuf> {
    crate::config::project_dirs().map(|dirs| dirs.config_dir().join("federation.toml"))
}

pub fn load_settings() -> FedSettings {
    let Some(path) = settings_path() else {
        return FedSettings::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|err| {
            tracing::warn!(%err, "federation.toml is malformed; using defaults");
            FedSettings::default()
        }),
        Err(_) => FedSettings::default(),
    }
}

fn save_settings(settings: &FedSettings) -> Result<()> {
    let path = settings_path().context("cannot determine the config directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, toml::to_string_pretty(settings)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Data shapes for the UI
// ---------------------------------------------------------------------------

/// An artist surfaced by federated search — either an artist record, or
/// derived from the artist names of matching tracks/releases (so searching
/// a track title still leads to the artist's card).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FedArtistHit {
    pub name: String,
    /// Distinct peers (other than this instance) holding the artist.
    pub peers: usize,
}

/// Federated search results for the UI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FedSearchResults {
    pub artists: Vec<FedArtistHit>,
    pub tracks: Vec<FedTrack>,
}

/// A track found through federated search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FedTrack {
    /// Hex item id in the DHT (the key audio is requested by).
    pub item_id: String,
    /// Hex endpoint id of the owning peer.
    pub owner: String,
    /// The item is published by this very instance.
    pub own: bool,
    pub title: String,
    pub artist_names: Vec<String>,
    pub featured_artist_names: Vec<String>,
    pub year: Option<i32>,
    pub duration_seconds: Option<i64>,
    /// Stable audio content id (`b3:<64 hex>`) when the owner published it.
    pub content_id: Option<String>,
    /// Release context, known when the track came from an artist card.
    pub release_title: Option<String>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
}

impl FedTrack {
    pub fn artist_line(&self) -> String {
        artist_line(&self.artist_names, &self.featured_artist_names)
    }

    pub fn owner_short(&self) -> String {
        self.owner.chars().take(10).collect()
    }

    pub fn duration_label(&self) -> String {
        match self.duration_seconds {
            Some(total) => format!("{}:{:02}", total / 60, total % 60),
            None => String::new(),
        }
    }
}

/// Live status snapshot rendered on the Federation tab.
#[derive(Debug, Clone, Default)]
pub struct FedStatus {
    pub running: bool,
    pub network: String,
    pub endpoint_id: String,
    pub dht_node_id: String,
    pub connected_peers: Vec<String>,
    pub known_contacts: usize,
    pub stored_dht_records: Option<usize>,
    pub stored_dht_bytes: Option<u64>,
    pub published_items: usize,
    pub last_sync: Option<String>,
    pub last_error: Option<String>,
}

/// Outcome of preparing a federated track for playback.
#[derive(Debug)]
pub struct FedPlayable {
    pub track: TrackItem,
    /// The file was imported into the local library (save-on-listen).
    pub imported: bool,
}

// ---------------------------------------------------------------------------
// The federation manager (lives in Runtime, not AppState)
// ---------------------------------------------------------------------------

struct Running {
    service: Arc<MusicDhtService>,
    network_name: String,
    network_id: NetworkId,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

pub struct Federation {
    library: Arc<Library>,
    devices: Arc<crate::devices::DeviceSync>,
    data_dir: PathBuf,
    cache_dir: PathBuf,
    media_dir: PathBuf,
    metadata_cache: std::sync::Mutex<std::collections::HashMap<String, CachedTrackMetadata>>,
    settings: std::sync::Mutex<FedSettings>,
    running: tokio::sync::Mutex<Option<Running>>,
    last_sync: std::sync::Mutex<Option<String>>,
    last_error: std::sync::Mutex<Option<String>>,
}

#[derive(Debug, Clone)]
struct CachedTrackMetadata {
    fed: FedTrack,
    title: String,
    artists: Vec<String>,
    featured_artists: Vec<String>,
    release_title: Option<String>,
    release_type: Option<String>,
    year: Option<i32>,
    duration_seconds: Option<f64>,
    track_number: Option<i32>,
    disc_number: Option<i32>,
}

impl CachedTrackMetadata {
    fn to_fed_track(&self) -> FedTrack {
        FedTrack {
            item_id: self.fed.item_id.clone(),
            owner: self.fed.owner.clone(),
            own: self.fed.own,
            title: self.title.clone(),
            artist_names: self.artists.clone(),
            featured_artist_names: self.featured_artists.clone(),
            year: self.year,
            duration_seconds: self
                .duration_seconds
                .map(|duration| duration.round() as i64),
            content_id: self.fed.content_id.clone(),
            release_title: self.release_title.clone(),
            track_number: self.track_number,
            disc_number: self.disc_number,
        }
    }
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_label() -> String {
    // Seconds since start of the day are enough for a status line without
    // pulling in a date-time crate.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "{:02}:{:02}:{:02} UTC",
        secs / 3600 % 24,
        secs / 60 % 60,
        secs % 60
    )
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn dht_record_payload_bytes(data_dir: PathBuf, now_ms: u64) -> Result<u64> {
    tokio::task::spawn_blocking(move || -> Result<u64> {
        let path = data_dir.join("state.sqlite3");
        if !path.exists() {
            return Ok(0);
        }
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {}", path.display()))?;
        let bytes: i64 = conn.query_row(
            "SELECT COALESCE(SUM(length(payload)), 0)
             FROM dht_records
             WHERE expires_at_ms > ?1",
            params![now_ms as i64],
            |row| row.get(0),
        )?;
        Ok(bytes.max(0) as u64)
    })
    .await
    .context("DHT size query task failed")?
}

impl Federation {
    pub fn new(library: Arc<Library>, devices: Arc<crate::devices::DeviceSync>) -> Arc<Self> {
        let dirs = crate::config::project_dirs();
        let data_dir = dirs
            .as_ref()
            .map(|d| d.data_dir().join("federation"))
            .unwrap_or_else(|| PathBuf::from("federation"));
        let cache_dir = dirs
            .as_ref()
            .map(|d| d.cache_dir().join("fedcache"))
            .unwrap_or_else(|| PathBuf::from("fedcache"));
        let media_dir = dirs
            .as_ref()
            .map(|d| d.data_dir().join("federation-media"))
            .unwrap_or_else(|| PathBuf::from("federation-media"));
        let initial_error = [&data_dir, &cache_dir, &media_dir]
            .into_iter()
            .find_map(|dir| {
                std::fs::create_dir_all(dir)
                    .err()
                    .map(|err| format!("cannot create {}: {err}", dir.display()))
            });
        Arc::new(Self {
            library,
            devices,
            data_dir,
            cache_dir,
            media_dir,
            metadata_cache: std::sync::Mutex::new(Default::default()),
            settings: std::sync::Mutex::new(load_settings()),
            running: tokio::sync::Mutex::new(None),
            last_sync: std::sync::Mutex::new(None),
            last_error: std::sync::Mutex::new(initial_error),
        })
    }

    pub fn settings(&self) -> FedSettings {
        lock(&self.settings).clone()
    }

    fn cached_metadata_snapshot(&self) -> Vec<CachedTrackMetadata> {
        lock(&self.metadata_cache).values().cloned().collect()
    }

    fn set_error(&self, message: Option<String>) {
        *lock(&self.last_error) = message;
    }

    /// Persists new settings and starts/stops/restarts the node to match.
    pub async fn apply_settings(self: &Arc<Self>, settings: FedSettings) -> Result<()> {
        anyhow::ensure!(
            !settings.enabled || !settings.network_id.trim().is_empty(),
            "set a network id before enabling federation"
        );
        if let Err(err) = save_settings(&settings) {
            tracing::warn!(%err, "saving federation settings failed");
        }
        *lock(&self.settings) = settings.clone();
        if settings.enabled {
            self.start(settings.network_id.trim().to_string()).await?;
            self.spawn_sync_soon().await;
        } else {
            self.stop().await;
        }
        Ok(())
    }

    pub async fn start_if_enabled(self: &Arc<Self>) {
        let settings = self.settings();
        if settings.enabled && !settings.network_id.trim().is_empty() {
            if let Err(err) = self.start(settings.network_id.trim().to_string()).await {
                tracing::error!("federation autostart failed: {err:#}");
                self.set_error(Some(format!("autostart failed: {err}")));
            } else {
                self.spawn_sync_soon().await;
            }
        }
    }

    /// Starts the DHT node. Idempotent per network name.
    async fn start(self: &Arc<Self>, network_name: String) -> Result<()> {
        self.start_with_network_id(NetworkId::from_name(&network_name), network_name)
            .await
    }

    async fn start_with_network_id(
        self: &Arc<Self>,
        network_id: NetworkId,
        network_name: String,
    ) -> Result<()> {
        let mut guard = self.running.lock().await;
        if let Some(running) = guard.as_ref() {
            if running.network_id == network_id {
                return Ok(());
            }
            stop_running(guard.take()).await;
        }
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("creating {}", self.data_dir.display()))?;

        let config = MusicDhtConfig::builder()
            .data_dir(&self.data_dir)
            .network_id(network_id)
            // Peers of the network find each other knowing only its name.
            .rendezvous(RendezvousConfig::default())
            // Peers stream each other's audio over this protocol.
            .stream_protocol(AUDIO_ALPN)
            // ...and browse each other's per-artist catalogs over this one.
            .stream_protocol(CATALOG_ALPN)
            // Personal-device sync (likes, playlists, trusted devices).
            .stream_protocol(crate::devices::SYNC_ALPN)
            .build()
            .map_err(|err| anyhow::anyhow!("invalid federation config: {err}"))?;
        let (service, mut events) = MusicDhtService::start(config)
            .await
            .map_err(|err| anyhow::anyhow!("failed to start the DHT node: {err}"))?;
        let service = Arc::new(service);
        tracing::info!(
            endpoint_id = %service.endpoint_id(),
            network = %network_name,
            "federation started"
        );

        // Drain DHT events into the log; the channel is bounded.
        let event_task = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                tracing::debug!("federation event: {event:?}");
            }
        });
        // Keep the published library in sync with the local index.
        let sync_self = Arc::clone(self);
        let sync_service = Arc::clone(&service);
        let sync_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(SYNC_INTERVAL);
            loop {
                interval.tick().await;
                let _ = sync_self.sync_once(&sync_service).await;
            }
        });
        // Serve audio requests from other peers of the network.
        let audio_acceptor = service
            .stream_acceptor(AUDIO_ALPN)
            .map_err(|err| anyhow::anyhow!("failed to take the audio acceptor: {err}"))?;
        let audio_task = tokio::spawn(audio::serve_peers(
            audio_acceptor,
            Arc::clone(&self.library),
            service.endpoint_id(),
        ));
        // Serve per-artist catalog requests (the federated artist card).
        let catalog_acceptor = service
            .stream_acceptor(CATALOG_ALPN)
            .map_err(|err| anyhow::anyhow!("failed to take the catalog acceptor: {err}"))?;
        let catalog_task = tokio::spawn(catalog::serve_peers(
            catalog_acceptor,
            Arc::clone(&self.library),
            service.endpoint_id(),
        ));
        let sync_acceptor = service
            .stream_acceptor(crate::devices::SYNC_ALPN)
            .map_err(|err| anyhow::anyhow!("failed to take the device-sync acceptor: {err}"))?;
        let device_sync_task = tokio::spawn(crate::devices::serve_peers(
            sync_acceptor,
            Arc::clone(&self.devices),
            Arc::clone(&service),
        ));
        let device_sync = Arc::clone(&self.devices);
        let device_service = Arc::clone(&service);
        let device_tick_task = tokio::spawn(async move {
            crate::devices::sync_loop(device_sync, device_service).await;
        });

        *guard = Some(Running {
            service,
            network_name,
            network_id,
            tasks: vec![
                event_task,
                sync_task,
                audio_task,
                catalog_task,
                device_sync_task,
                device_tick_task,
            ],
        });
        self.set_error(None);
        Ok(())
    }

    async fn stop(&self) {
        let mut guard = self.running.lock().await;
        stop_running(guard.take()).await;
    }

    pub async fn shutdown(&self) {
        self.stop().await;
    }

    async fn service(&self) -> Result<Arc<MusicDhtService>> {
        self.running
            .lock()
            .await
            .as_ref()
            .map(|running| Arc::clone(&running.service))
            .context("federation is not running")
    }

    /// Publishes the library immediately (used right after start/settings).
    async fn spawn_sync_soon(self: &Arc<Self>) {
        if let Ok(service) = self.service().await {
            let fed = Arc::clone(self);
            tokio::spawn(async move {
                let _ = fed.sync_once(&service).await;
            });
        }
    }

    pub async fn sync_now(self: &Arc<Self>) -> Result<()> {
        let service = self.service().await?;
        let sync_stats = self.sync_once(&service).await?;
        let publish_stats = match service.republish().await {
            Ok(stats) => stats,
            Err(err) => {
                tracing::warn!("federation republish failed: {err}");
                self.set_error(Some(format!("republish failed: {err}")));
                anyhow::bail!("republish failed: {err}");
            }
        };
        self.record_publish_success(sync_stats, publish_stats);
        Ok(())
    }

    async fn sync_once(&self, service: &MusicDhtService) -> Result<SyncStats> {
        let library = Arc::clone(&self.library);
        let specs = tokio::task::spawn_blocking(move || collect_specs(&library)).await;
        let specs = match specs {
            Ok(Ok(specs)) => specs,
            Ok(Err(err)) => {
                tracing::warn!("federation sync: library read failed: {err:#}");
                self.set_error(Some(format!("library read failed: {err}")));
                anyhow::bail!("library read failed: {err}");
            }
            Err(err) => {
                tracing::warn!("federation sync task failed: {err}");
                anyhow::bail!("sync task failed: {err}");
            }
        };
        match service.sync_library(specs).await {
            Ok(stats) => {
                self.record_sync_success(stats);
                if stats.failed > 0 {
                    self.set_error(Some(format!(
                        "{} item(s) failed to publish in the last sync",
                        stats.failed
                    )));
                } else {
                    self.set_error(None);
                }
                Ok(stats)
            }
            Err(err) => {
                tracing::warn!("federation sync failed: {err}");
                self.set_error(Some(format!("sync failed: {err}")));
                Err(anyhow::anyhow!("sync failed: {err}"))
            }
        }
    }

    fn record_sync_success(&self, stats: SyncStats) {
        *lock(&self.last_sync) = Some(format!(
            "{} (+{} ~{} −{}, unchanged {}, failed {})",
            now_label(),
            stats.added,
            stats.updated,
            stats.removed,
            stats.unchanged,
            stats.failed
        ));
    }

    fn record_publish_success(&self, sync_stats: SyncStats, publish_stats: PublishStats) {
        *lock(&self.last_sync) = Some(format!(
            "{} (+{} ~{} −{}, unchanged {}, failed {}; republished {} records, {} keys, remote nodes {})",
            now_label(),
            sync_stats.added,
            sync_stats.updated,
            sync_stats.removed,
            sync_stats.unchanged,
            sync_stats.failed,
            publish_stats.records,
            publish_stats.keys,
            publish_stats.remote_nodes,
        ));
        self.set_error(None);
    }

    pub async fn status(&self) -> FedStatus {
        let settings = self.settings();
        let guard = self.running.lock().await;
        let mut status = FedStatus {
            network: settings.network_id,
            last_sync: lock(&self.last_sync).clone(),
            last_error: lock(&self.last_error).clone(),
            ..FedStatus::default()
        };
        if let Some(running) = guard.as_ref() {
            let service = &running.service;
            status.running = true;
            status.network = running.network_name.clone();
            status.endpoint_id = service.endpoint_id().to_string();
            status.dht_node_id = service.node_id().to_string();
            status.connected_peers = service
                .connected_peers()
                .iter()
                .map(|p| p.to_string())
                .collect();
            status.known_contacts = service.known_peers().len();
            status.stored_dht_records = service.dht_record_count().await.ok();
            status.stored_dht_bytes =
                dht_record_payload_bytes(self.data_dir.clone(), unix_time_ms())
                    .await
                    .ok();
            status.published_items = service
                .list_local_items()
                .await
                .map(|items| items.len())
                .unwrap_or(0);
        }
        status
    }

    /// Searches the federated network: matching tracks plus the artists a
    /// card can be assembled for (from artist records and from the artist
    /// names of matching tracks/releases).
    pub async fn search(&self, query: &str) -> Result<FedSearchResults> {
        let service = self.service().await?;
        let normalized = music_dht::normalize_name(query);
        let outcome = service
            .search_network(query)
            .await
            .map_err(|err| anyhow::anyhow!("federated search failed: {err}"))?;
        let own = service.endpoint_id();
        let mut tracks: Vec<FedTrack> = outcome
            .network_results
            .iter()
            .filter(|item| item.kind == ItemKind::Track)
            .map(|item| FedTrack {
                item_id: audio::hex_encode(item.id.as_bytes()),
                owner: item.owner.to_string(),
                own: item.owner == own,
                title: item.name.clone(),
                artist_names: item.artist_names.clone(),
                featured_artist_names: item.featured_artist_names.clone(),
                year: item.year,
                duration_seconds: item.duration_seconds.map(|d| d.round() as i64),
                content_id: item.content_id.clone(),
                release_title: item.release_title.clone(),
                track_number: item.track_number,
                disc_number: item.disc_number,
            })
            .collect();
        let mut seen_tracks: std::collections::HashSet<(String, String)> = tracks
            .iter()
            .map(|track| (track.owner.clone(), track.item_id.clone()))
            .collect();

        // Artists: normalized name -> (display name, distinct non-own peers).
        let mut artists: std::collections::HashMap<
            String,
            (String, std::collections::HashSet<String>),
        > = Default::default();
        for item in &outcome.network_results {
            if item.owner == own {
                continue;
            }
            let mut note = |name: &str| {
                let key = music_dht::normalize_name(name);
                if key.is_empty() {
                    return;
                }
                let entry = artists
                    .entry(key)
                    .or_insert_with(|| (name.to_string(), Default::default()));
                entry.1.insert(item.owner.to_string());
            };
            if item.kind == ItemKind::Artist {
                note(&item.name);
            }
            for artist in &item.artist_names {
                note(artist);
            }
            for artist in &item.featured_artist_names {
                note(artist);
            }
        }
        for cached in self.cached_metadata_snapshot() {
            if !cached_matches_query(&cached, &normalized) {
                continue;
            }
            let fed = cached.to_fed_track();
            if seen_tracks.insert((fed.owner.clone(), fed.item_id.clone())) {
                tracks.push(fed);
            }
            if !cached.fed.own {
                for artist in &cached.featured_artists {
                    let key = music_dht::normalize_name(artist);
                    if key.is_empty() {
                        continue;
                    }
                    let entry = artists
                        .entry(key)
                        .or_insert_with(|| (artist.clone(), Default::default()));
                    entry.1.insert(cached.fed.owner.clone());
                }
            }
        }
        let mut artists: Vec<FedArtistHit> = artists
            .into_values()
            .map(|(name, owners)| FedArtistHit {
                name,
                peers: owners.len(),
            })
            .collect();
        artists.sort_by(|a, b| b.peers.cmp(&a.peers).then_with(|| a.name.cmp(&b.name)));
        rank_fed_search_results(&mut artists, &mut tracks, &normalized);

        Ok(FedSearchResults { artists, tracks })
    }

    /// Resolves a share-link content id to one playable federated track.
    ///
    /// Resolution order: the in-session metadata cache, the DHT content key
    /// (retried — the DHT is eventually consistent, so a single lookup can
    /// transiently come up short), then a name search by the link label:
    /// records under the name keys carry content ids too, and failing an
    /// exact match, a track whose artists and title all match the label is
    /// the same song from another owner.
    pub async fn track_by_content_id(
        &self,
        content_id: &str,
        label: Option<&str>,
    ) -> Result<FedTrack> {
        let service = self.service().await?;
        let own = service.endpoint_id();

        for cached in self.cached_metadata_snapshot() {
            if cached.fed.content_id.as_deref() == Some(content_id) {
                return Ok(cached.to_fed_track());
            }
        }

        let mut queried_nodes = 0usize;
        for attempt in 0..CONTENT_LOOKUP_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(CONTENT_LOOKUP_RETRY_DELAY).await;
            }
            let outcome = service
                .search_content_id(content_id)
                .await
                .map_err(|err| anyhow::anyhow!("federated content lookup failed: {err}"))?;
            queried_nodes = queried_nodes.max(outcome.queried_nodes);
            if let Some(item) = outcome
                .local_results
                .into_iter()
                .chain(outcome.network_results)
                .find(|item| item.kind == ItemKind::Track)
            {
                return Ok(fed_track_from_item(item, own));
            }
        }

        if let Some(label) = label {
            let normalized = music_dht::normalize_name(label);
            if !normalized.is_empty() {
                let outcome = service
                    .search_network(label)
                    .await
                    .map_err(|err| anyhow::anyhow!("federated search failed: {err}"))?;
                queried_nodes = queried_nodes.max(outcome.queried_nodes);
                let candidates: Vec<music_dht::LibraryItem> = outcome
                    .local_results
                    .into_iter()
                    .chain(outcome.network_results)
                    .filter(|item| item.kind == ItemKind::Track)
                    .collect();
                if let Some(item) = candidates
                    .iter()
                    .find(|item| item.content_id.as_deref() == Some(content_id))
                {
                    return Ok(fed_track_from_item(item.clone(), own));
                }
                // "feat" is an artifact of the label format ("A feat. B-Title"),
                // not a token of any track record.
                let tokens: Vec<String> = music_dht::tokenize(&normalized)
                    .into_iter()
                    .filter(|token| token != "feat")
                    .collect();
                if !tokens.is_empty()
                    && let Some(item) = candidates.into_iter().find(|item| {
                        let item_tokens = item.search_tokens();
                        tokens.iter().all(|token| item_tokens.contains(token))
                    })
                {
                    return Ok(fed_track_from_item(item, own));
                }
            }
        }

        if queried_nodes == 0 {
            anyhow::bail!("no federation peers reachable yet — check the Federation tab and retry");
        }
        anyhow::bail!("no peers currently publish this shared track")
    }

    /// Assembles the federated artist card: finds the peers holding the
    /// artist through the DHT, asks each for its catalog slice directly and
    /// merges the answers (missing/slow peers are skipped). Role-aware DHT
    /// track records are also folded in, so featured appearances can be shown
    /// even before a peer's catalog response arrives.
    pub async fn artist_card(&self, name: &str) -> Result<FedArtistCard> {
        let service = self.service().await?;
        let own = service.endpoint_id();
        let own_hex = own.to_string();
        let normalized = music_dht::normalize_name(name);
        let outcome = service
            .search_network(name)
            .await
            .map_err(|err| anyhow::anyhow!("federated search failed: {err}"))?;
        let cached_metadata = self.cached_metadata_snapshot();
        let has_cached_artist = cached_metadata
            .iter()
            .any(|cached| cached_has_artist(cached, &normalized));
        let owners: std::collections::HashSet<EndpointId> = outcome
            .network_results
            .iter()
            .filter(|item| {
                (item.kind == ItemKind::Artist && item.normalized_name == normalized)
                    || item
                        .artist_names
                        .iter()
                        .any(|artist| music_dht::normalize_name(artist) == normalized)
                    || item
                        .featured_artist_names
                        .iter()
                        .any(|artist| music_dht::normalize_name(artist) == normalized)
            })
            .map(|item| item.owner)
            .filter(|owner| *owner != own)
            .collect();

        let local_library = Arc::clone(&self.library);
        let local_name = name.to_string();
        let mut catalogs = match tokio::task::spawn_blocking(move || {
            catalog::build_catalog_artist(&local_library, own, &local_name)
        })
        .await
        {
            Ok(Ok(Some(catalog))) => vec![(own_hex.clone(), catalog)],
            Ok(Ok(None)) => Vec::new(),
            Ok(Err(err)) => {
                tracing::warn!("local catalog lookup failed: {err:#}");
                Vec::new()
            }
            Err(err) => {
                tracing::warn!("local catalog task failed: {err}");
                Vec::new()
            }
        };
        anyhow::ensure!(
            !owners.is_empty() || !catalogs.is_empty() || has_cached_artist,
            "no peers hold artist \"{name}\""
        );

        let mut requests = Vec::new();
        for owner in owners {
            let service = Arc::clone(&service);
            let name = name.to_string();
            requests.push(tokio::spawn(async move {
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    catalog::fetch_catalog(&service, owner, &name),
                )
                .await;
                match result {
                    Ok(Ok(catalog)) => Some((owner.to_string(), catalog)),
                    Ok(Err(err)) => {
                        tracing::warn!(peer = %owner, "catalog fetch failed: {err:#}");
                        None
                    }
                    Err(_) => {
                        tracing::warn!(peer = %owner, "catalog fetch timed out");
                        None
                    }
                }
            }));
        }
        for request in requests {
            if let Ok(Some(catalog)) = request.await {
                catalogs.push(catalog);
            }
        }
        let mut card = catalog::merge_catalogs(name, catalogs);
        add_dht_appearance_hits(&mut card, &outcome.network_results, &normalized, name);
        add_cached_appearance_hits(&mut card, &cached_metadata, &normalized, name);
        anyhow::ensure!(
            !card.releases.is_empty() || !card.appears_on.is_empty(),
            "none of the peers returned releases or appearances"
        );
        card.own_owner = Some(own_hex);
        Ok(card)
    }

    pub async fn ticket(&self) -> Result<String> {
        let service = self.service().await?;
        let ticket = service
            .ticket()
            .await
            .map_err(|err| anyhow::anyhow!("cannot create a ticket: {err}"))?;
        Ok(ticket.to_string())
    }

    pub async fn device_invite(self: &Arc<Self>) -> Result<String> {
        if self.running.lock().await.is_none() {
            let status = self.devices.status();
            let network_name = format!("furumi-device-sync:{}", status.group_id);
            self.start_with_network_id(NetworkId::from_name(&network_name), "device-sync".into())
                .await?;
        }
        let service = self.service().await?;
        self.devices.create_invite(service).await
    }

    pub async fn device_connect(self: &Arc<Self>, invite: &str) -> Result<String> {
        let network_id = crate::devices::invite_network_id(invite)?;
        let needs_start = self
            .running
            .lock()
            .await
            .as_ref()
            .is_none_or(|running| running.network_id != network_id);
        if needs_start {
            self.start_with_network_id(network_id, "device-invite".to_string())
                .await?;
        }
        let service = self.service().await?;
        self.devices.connect_invite(service, invite).await
    }

    pub async fn device_sync_now(self: &Arc<Self>) -> Result<()> {
        let service = self.service().await?;
        self.devices.sync_once(service).await
    }

    pub async fn connect(&self, ticket: &str) -> Result<String> {
        let service = self.service().await?;
        let ticket: PeerTicket = ticket
            .trim()
            .parse()
            .map_err(|err| anyhow::anyhow!("malformed ticket: {err}"))?;
        let peer = service
            .connect(ticket)
            .await
            .map_err(|err| anyhow::anyhow!("connect failed: {err}"))?;
        Ok(peer.to_string())
    }

    /// Directory for streamed (never library-imported) card artwork.
    fn art_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("art")
    }

    /// Returns a cached-or-streamed image for the card: the artist image
    /// (`release: None`) or a release cover. Peers are tried in order until
    /// one answers with an image; the result lands in the art cache and its
    /// local path is returned.
    pub async fn card_image(
        &self,
        owners: &[String],
        artist: &str,
        release: Option<&str>,
    ) -> Option<String> {
        let dir = self.art_cache_dir();
        let stem = match release {
            Some(release) => format!(
                "cover-{}-{}",
                sanitize_file_stem(artist),
                sanitize_file_stem(release)
            ),
            None => format!("artist-{}", sanitize_file_stem(artist)),
        };
        // Reuse a previously streamed copy of any known image type.
        for extension in ["jpg", "png", "webp", "gif", "bmp"] {
            let path = dir.join(format!("{stem}.{extension}"));
            if path.is_file() {
                return Some(path.to_string_lossy().into_owned());
            }
        }
        let service = self.service().await.ok()?;
        tokio::fs::create_dir_all(&dir).await.ok()?;
        for owner in owners {
            let Ok(owner) = EndpointId::from_str(owner) else {
                continue;
            };
            let fetched = tokio::time::timeout(
                Duration::from_secs(5),
                catalog::fetch_image(&service, owner, artist, release),
            )
            .await;
            match fetched {
                Ok(Ok(Some((bytes, extension)))) => {
                    let path = dir.join(format!("{stem}.{extension}"));
                    if tokio::fs::write(&path, &bytes).await.is_ok() {
                        return Some(path.to_string_lossy().into_owned());
                    }
                }
                Ok(Ok(None)) => continue,
                Ok(Err(err)) => tracing::debug!(peer = %owner, "image fetch failed: {err:#}"),
                Err(_) => tracing::debug!(peer = %owner, "image fetch timed out"),
            }
        }
        None
    }

    /// Downloads a federated track straight into the local library
    /// (regardless of the save-on-listen setting) and returns the imported
    /// track. Own/already-local tracks resolve without downloading.
    pub async fn download_to_library(self: &Arc<Self>, fed: &FedTrack) -> Result<TrackItem> {
        let playable = self.fetch_playable(fed, true).await?;
        Ok(playable.track)
    }

    /// Prepares a federated track for playback: local tracks resolve
    /// straight to the library; remote tracks are downloaded — into the
    /// library when save-on-listen is enabled, into the cache otherwise.
    pub async fn prepare_playback(self: &Arc<Self>, fed: &FedTrack) -> Result<FedPlayable> {
        let save = self.settings().save_on_listen;
        self.fetch_playable(fed, save).await
    }

    /// Fetches rich metadata for a federated track without downloading the
    /// audio bytes, for the track-info popup.
    pub async fn track_info(&self, track: TrackItem) -> Result<TrackItem> {
        let Some(fed) = track.fed.clone() else {
            return Ok(track);
        };
        let service = self.service().await?;
        let item_id =
            audio::hex_decode_item_id(&fed.item_id).context("malformed item id in the result")?;

        if fed.own {
            let library = Arc::clone(&self.library);
            let own_id = service.endpoint_id();
            return tokio::task::spawn_blocking(move || -> Result<TrackItem> {
                let Some(track_id) = audio::resolve_local_track_id(&library, own_id, item_id)?
                else {
                    anyhow::bail!("this track is no longer in the local library");
                };
                library
                    .tracks_by_ids(&[track_id])?
                    .into_iter()
                    .next()
                    .context("this track is no longer in the local library")
            })
            .await?;
        }

        let owner = EndpointId::from_str(&fed.owner)
            .map_err(|_| anyhow::anyhow!("malformed owner id '{}'", fed.owner))?;
        let fetched = self
            .fetch_metadata_with_fallback(&service, owner, &fed)
            .await?;
        let enriched = metadata_preview_track(&track, &fed, &fetched);
        if let Some(cached) = cached_track_metadata(&enriched, &fed, &fetched) {
            lock(&self.metadata_cache).insert(cached_cache_key(&fed), cached);
        }
        Ok(enriched)
    }

    async fn fetch_playable(self: &Arc<Self>, fed: &FedTrack, save: bool) -> Result<FedPlayable> {
        let service = self.service().await?;
        let item_id =
            audio::hex_decode_item_id(&fed.item_id).context("malformed item id in the result")?;

        if fed.own {
            let library = Arc::clone(&self.library);
            let own_id = service.endpoint_id();
            let track = tokio::task::spawn_blocking(move || -> Result<Option<TrackItem>> {
                let Some(track_id) = audio::resolve_local_track_id(&library, own_id, item_id)?
                else {
                    return Ok(None);
                };
                Ok(library.tracks_by_ids(&[track_id])?.into_iter().next())
            })
            .await??
            .context("this track is no longer in the local library")?;
            return Ok(FedPlayable {
                track,
                imported: false,
            });
        }

        let owner = EndpointId::from_str(&fed.owner)
            .map_err(|_| anyhow::anyhow!("malformed owner id '{}'", fed.owner))?;
        let dir = if save {
            &self.media_dir
        } else {
            &self.cache_dir
        };
        tokio::fs::create_dir_all(dir).await?;

        let downloaded = self
            .download_track_with_fallback(&service, owner, fed, dir)
            .await?;
        tracing::info!(
            path = %downloaded.path.display(),
            mime = %downloaded.mime_type,
            cover = downloaded.cover.is_some(),
            "federated track downloaded"
        );

        if save {
            let library = Arc::clone(&self.library);
            let import_path = downloaded.path.clone();
            let import_metadata = downloaded.metadata.clone();
            let import_cover = downloaded.cover.clone();
            let artist_image = downloaded.artist_image.clone();
            let fed_item_id = fed.item_id.clone();
            let imported = tokio::task::spawn_blocking(move || -> Result<Option<TrackItem>> {
                let mut import = crate::library::import::read_file(&import_path)?;
                // The owner's database is more authoritative than whatever
                // tags the file happens to carry (often none at all).
                if let Some(meta) = &import_metadata {
                    apply_remote_metadata(&mut import, meta);
                }
                // Same for the cover: the peer's library cover wins over an
                // embedded picture; embedded art stays as the fallback.
                if import_cover.is_some() {
                    import.cover = import_cover;
                }
                let (track_id, _) = crate::library::import::upsert_track(&library, &import)?;
                // A like that referenced the federated track moves onto the
                // freshly imported local row.
                if let Err(err) = library.transfer_fed_like(&fed_item_id, track_id) {
                    tracing::warn!(%err, "federated like transfer failed");
                }
                // The owner's artist image fills the gap for a freshly
                // created (or still image-less) main artist.
                if let (Some((bytes, extension)), Some(artist_name)) =
                    (&artist_image, import.artists.first())
                    && let Err(err) = save_artist_image(&library, artist_name, bytes, extension)
                {
                    tracing::warn!(%err, "saving the artist image failed");
                }
                Ok(library.tracks_by_ids(&[track_id])?.into_iter().next())
            })
            .await?;
            match imported {
                Ok(Some(track)) => {
                    return Ok(FedPlayable {
                        track,
                        imported: true,
                    });
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        "importing the downloaded track failed: {err:#}; playing from the file"
                    );
                }
            }
        }

        // Ephemeral playback: put the cover next to the cached audio so the
        // views can show it.
        let cover_path = match &downloaded.cover {
            Some((bytes, extension)) => {
                let path = downloaded.path.with_extension(format!("cover.{extension}"));
                match tokio::fs::write(&path, bytes).await {
                    Ok(()) => Some(path.to_string_lossy().into_owned()),
                    Err(err) => {
                        tracing::warn!(%err, "saving the cover failed");
                        None
                    }
                }
            }
            None => None,
        };
        let mut track = ephemeral_track(fed, downloaded.metadata.as_ref(), &downloaded.path);
        track.cover_path = cover_path;
        Ok(FedPlayable {
            track,
            imported: false,
        })
    }

    async fn download_track_with_fallback(
        &self,
        service: &MusicDhtService,
        owner: EndpointId,
        fed: &FedTrack,
        dir: &Path,
    ) -> Result<audio::Downloaded> {
        let stem = download_stem(fed);
        match audio::download_track(service, owner, &fed.item_id, dir, &stem).await {
            Ok(downloaded) => return Ok(downloaded),
            Err(primary_err) => {
                let Some(content_id) = fed.content_id.as_deref() else {
                    return Err(primary_err);
                };
                tracing::warn!(
                    owner = %fed.owner,
                    item_id = %fed.item_id,
                    content_id,
                    "primary federated source failed; searching content-id fallbacks: {primary_err:#}"
                );
                let outcome = match service.search_content_id(content_id).await {
                    Ok(outcome) => outcome,
                    Err(err) => {
                        tracing::warn!(content_id, "content-id fallback lookup failed: {err}");
                        return Err(primary_err);
                    }
                };
                for item in outcome.network_results {
                    if item.kind != ItemKind::Track {
                        continue;
                    }
                    let candidate_item_id = audio::hex_encode(item.id.as_bytes());
                    if item.owner == owner && candidate_item_id == fed.item_id {
                        continue;
                    }
                    let candidate_owner = item.owner;
                    match audio::download_track(
                        service,
                        candidate_owner,
                        &candidate_item_id,
                        dir,
                        &stem,
                    )
                    .await
                    {
                        Ok(downloaded) => {
                            tracing::info!(
                                owner = %candidate_owner,
                                item_id = %candidate_item_id,
                                content_id,
                                "federated track downloaded from content-id fallback"
                            );
                            return Ok(downloaded);
                        }
                        Err(err) => {
                            tracing::debug!(
                                owner = %candidate_owner,
                                item_id = %candidate_item_id,
                                content_id,
                                "content-id fallback source failed: {err:#}"
                            );
                        }
                    }
                }
                Err(primary_err)
            }
        }
    }

    async fn fetch_metadata_with_fallback(
        &self,
        service: &MusicDhtService,
        owner: EndpointId,
        fed: &FedTrack,
    ) -> Result<audio::FetchedMetadata> {
        match audio::fetch_metadata(service, owner, &fed.item_id).await {
            Ok(metadata) => return Ok(metadata),
            Err(primary_err) => {
                let Some(content_id) = fed.content_id.as_deref() else {
                    return Err(primary_err);
                };
                tracing::warn!(
                    owner = %fed.owner,
                    item_id = %fed.item_id,
                    content_id,
                    "primary metadata source failed; searching content-id fallbacks: {primary_err:#}"
                );
                let outcome = match service.search_content_id(content_id).await {
                    Ok(outcome) => outcome,
                    Err(err) => {
                        tracing::warn!(content_id, "metadata fallback lookup failed: {err}");
                        return Err(primary_err);
                    }
                };
                for item in outcome.network_results {
                    if item.kind != ItemKind::Track {
                        continue;
                    }
                    let candidate_item_id = audio::hex_encode(item.id.as_bytes());
                    if item.owner == owner && candidate_item_id == fed.item_id {
                        continue;
                    }
                    match audio::fetch_metadata(service, item.owner, &candidate_item_id).await {
                        Ok(metadata) => {
                            tracing::info!(
                                owner = %item.owner,
                                item_id = %candidate_item_id,
                                content_id,
                                "federated metadata fetched from content-id fallback"
                            );
                            return Ok(metadata);
                        }
                        Err(err) => {
                            tracing::debug!(
                                owner = %item.owner,
                                item_id = %candidate_item_id,
                                content_id,
                                "metadata fallback source failed: {err:#}"
                            );
                        }
                    }
                }
                Err(primary_err)
            }
        }
    }
}

/// Writes a received artist image into the covers directory and attaches it
/// to the artist unless one is already set.
fn save_artist_image(
    library: &Library,
    artist_name: &str,
    bytes: &[u8],
    extension: &str,
) -> Result<()> {
    let covers_dir = library.covers_dir();
    std::fs::create_dir_all(covers_dir)?;
    let path = covers_dir.join(format!(
        "artist-{}.{extension}",
        sanitize_file_stem(artist_name)
    ));
    // Write only if the artist actually lacks an image, to avoid litter.
    if library.artist_image_missing(artist_name)? {
        std::fs::write(&path, bytes)?;
        library.set_artist_image_if_missing(artist_name, &path.to_string_lossy())?;
    }
    Ok(())
}

/// Overlays the peer-supplied metadata onto tag-derived import data. Every
/// non-empty peer field wins; file tags only fill the gaps.
fn apply_remote_metadata(import: &mut crate::library::import::TrackImport, meta: &TrackMetadata) {
    let title = meta.title.trim();
    if !title.is_empty() {
        import.title = title.to_string();
    }
    if !meta.artists.is_empty() {
        import.artists = meta.artists.clone();
    }
    if !meta.featured_artists.is_empty() {
        import.featured_artists = meta.featured_artists.clone();
    }
    if !meta.album_artists.is_empty() {
        import.album_artists = meta.album_artists.clone();
    } else if !meta.artists.is_empty() {
        import.album_artists = meta.artists.clone();
    }
    let release_title = meta.release_title.trim();
    if !release_title.is_empty() {
        import.release_title = release_title.to_string();
    }
    if meta.release_type.is_some() {
        import.release_type = meta.release_type.clone();
    }
    if meta.year.is_some() {
        import.year = meta.year;
    }
    if meta.track_number.is_some() {
        import.track_number = meta.track_number;
    }
    if meta.disc_number.is_some() {
        import.disc_number = meta.disc_number;
    }
    if let Some(duration) = meta.duration_seconds {
        import.duration_seconds = duration;
    }
    if meta.audio_format.is_some() {
        import.audio_format = meta.audio_format.clone();
    }
    if meta.audio_bitrate.is_some() {
        import.audio_bitrate = meta.audio_bitrate;
    }
    if meta.audio_sample_rate.is_some() {
        import.audio_sample_rate = meta.audio_sample_rate;
    }
    if meta.audio_bit_depth.is_some() {
        import.audio_bit_depth = meta.audio_bit_depth;
    }
}

fn metadata_preview_track(
    base: &TrackItem,
    fed: &FedTrack,
    fetched: &audio::FetchedMetadata,
) -> TrackItem {
    let refs = |names: &[String]| -> Vec<ArtistRef> {
        names
            .iter()
            .map(|name| ArtistRef {
                id: -1,
                name: name.clone(),
            })
            .collect()
    };
    let metadata = fetched.metadata.as_ref();
    let title = metadata
        .map(|meta| meta.title.trim())
        .filter(|title| !title.is_empty())
        .unwrap_or(&base.title)
        .to_string();
    let artists = match metadata {
        Some(meta) if !meta.artists.is_empty() => refs(&meta.artists),
        _ => base.artists.clone(),
    };
    let featured_artists = match metadata {
        Some(meta) if !meta.featured_artists.is_empty() => refs(&meta.featured_artists),
        _ => base.featured_artists.clone(),
    };
    let release_title = metadata
        .map(|meta| meta.release_title.trim())
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| base.release_title.clone());
    TrackItem {
        id: base.id,
        title,
        track_number: metadata
            .and_then(|meta| meta.track_number)
            .or(base.track_number),
        disc_number: metadata
            .and_then(|meta| meta.disc_number)
            .or(base.disc_number),
        duration_seconds: metadata
            .and_then(|meta| meta.duration_seconds)
            .or_else(|| fed.duration_seconds.map(|duration| duration as f64))
            .unwrap_or(base.duration_seconds),
        artists,
        featured_artists,
        release_id: base.release_id,
        release_title,
        release_year: metadata
            .and_then(|meta| meta.year)
            .or(base.release_year)
            .or(fed.year),
        file_path: String::new(),
        content_id: fed.content_id.clone().or_else(|| base.content_id.clone()),
        cover_path: base.cover_path.clone(),
        audio_format: metadata
            .and_then(|meta| meta.audio_format.clone())
            .or_else(|| audio::format_for_mime(&fetched.mime_type))
            .or_else(|| base.audio_format.clone()),
        audio_bitrate: metadata
            .and_then(|meta| meta.audio_bitrate)
            .or(base.audio_bitrate),
        audio_sample_rate: metadata
            .and_then(|meta| meta.audio_sample_rate)
            .or(base.audio_sample_rate),
        audio_bit_depth: metadata
            .and_then(|meta| meta.audio_bit_depth)
            .or(base.audio_bit_depth),
        file_size_bytes: (fetched.total_size > 0)
            .then_some(fetched.total_size as i64)
            .or(base.file_size_bytes),
        play_count: base.play_count,
        fed: Some(fed.clone()),
    }
}

fn cached_track_metadata(
    track: &TrackItem,
    fed: &FedTrack,
    fetched: &audio::FetchedMetadata,
) -> Option<CachedTrackMetadata> {
    let artist_names = |items: &[ArtistRef]| {
        items
            .iter()
            .map(|artist| artist.name.clone())
            .collect::<Vec<_>>()
    };
    Some(CachedTrackMetadata {
        fed: fed.clone(),
        title: track.title.clone(),
        artists: artist_names(&track.artists),
        featured_artists: artist_names(&track.featured_artists),
        release_title: (!track.release_title.trim().is_empty())
            .then(|| track.release_title.clone()),
        release_type: fetched
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.release_type.clone()),
        year: track.release_year.or(fed.year),
        duration_seconds: (track.duration_seconds > 0.0).then_some(track.duration_seconds),
        track_number: track.track_number.or(fed.track_number),
        disc_number: track.disc_number.or(fed.disc_number),
    })
}

fn cached_cache_key(fed: &FedTrack) -> String {
    format!("{}:{}", fed.owner, fed.item_id)
}

fn cached_matches_query(cached: &CachedTrackMetadata, normalized_query: &str) -> bool {
    if normalized_query.is_empty() {
        return false;
    }
    if music_dht::normalize_name(&cached.title) == normalized_query {
        return true;
    }
    if cached
        .release_title
        .as_deref()
        .is_some_and(|title| music_dht::normalize_name(title) == normalized_query)
    {
        return true;
    }
    let query_tokens = music_dht::tokenize(normalized_query);
    if query_tokens.is_empty() {
        return false;
    }
    let item_tokens = cached_search_tokens(cached);
    query_tokens
        .iter()
        .all(|token| item_tokens.iter().any(|candidate| candidate == token))
}

fn cached_search_tokens(cached: &CachedTrackMetadata) -> Vec<String> {
    let mut tokens = music_dht::tokenize(&music_dht::normalize_name(&cached.title));
    for value in cached
        .artists
        .iter()
        .chain(cached.featured_artists.iter())
        .chain(cached.release_title.iter())
    {
        tokens.extend(music_dht::tokenize(&music_dht::normalize_name(value)));
    }
    tokens
}

fn cached_has_artist(cached: &CachedTrackMetadata, normalized_artist: &str) -> bool {
    cached
        .featured_artists
        .iter()
        .any(|artist| music_dht::normalize_name(artist) == normalized_artist)
}

fn rank_fed_search_results(
    artists: &mut [FedArtistHit],
    tracks: &mut [FedTrack],
    normalized_query: &str,
) {
    artists.sort_by(|a, b| {
        exact_match_rank(&a.name, normalized_query)
            .cmp(&exact_match_rank(&b.name, normalized_query))
            .then_with(|| b.peers.cmp(&a.peers))
            .then_with(|| a.name.cmp(&b.name))
    });
    tracks.sort_by_key(|track| fed_track_match_rank(track, normalized_query));
}

fn exact_match_rank(value: &str, normalized_query: &str) -> u8 {
    if music_dht::normalize_name(value) == normalized_query {
        0
    } else {
        1
    }
}

fn fed_track_match_rank(track: &FedTrack, normalized_query: &str) -> u8 {
    if music_dht::normalize_name(&track.title) == normalized_query {
        return 0;
    }
    if track
        .release_title
        .as_deref()
        .is_some_and(|title| music_dht::normalize_name(title) == normalized_query)
    {
        return 1;
    }
    if track
        .artist_names
        .iter()
        .chain(track.featured_artist_names.iter())
        .any(|artist| music_dht::normalize_name(artist) == normalized_query)
    {
        return 2;
    }
    3
}

fn add_dht_appearance_hits(
    card: &mut FedArtistCard,
    hits: &[LibraryItem],
    normalized_artist: &str,
    display_artist: &str,
) {
    let mut changed = false;
    for item in hits {
        let Some(appearance) = dht_appearance_hit(item, normalized_artist, display_artist) else {
            continue;
        };
        if add_appearance_to_card(card, appearance) {
            changed = true;
        }
    }
    if changed {
        card.peers = card.owners.len();
        sort_fed_appearances(&mut card.appears_on);
    }
}

fn add_cached_appearance_hits(
    card: &mut FedArtistCard,
    cached: &[CachedTrackMetadata],
    normalized_artist: &str,
    display_artist: &str,
) {
    let mut changed = false;
    for track in cached {
        let Some(appearance) = cached_appearance_hit(track, normalized_artist, display_artist)
        else {
            continue;
        };
        if add_appearance_to_card(card, appearance) {
            changed = true;
        }
    }
    if changed {
        card.peers = card.owners.len();
        sort_fed_appearances(&mut card.appears_on);
    }
}

fn dht_appearance_hit(
    item: &LibraryItem,
    normalized_artist: &str,
    display_artist: &str,
) -> Option<FedAppearsOn> {
    if item.kind != ItemKind::Track {
        return None;
    }
    let appears_as_featured = item
        .featured_artist_names
        .iter()
        .any(|artist| music_dht::normalize_name(artist) == normalized_artist);
    if !appears_as_featured {
        return None;
    }
    let mut artists = Vec::new();
    for artist in &item.artist_names {
        push_artist_once(&mut artists, artist);
    }
    let mut featured_artists = Vec::new();
    for artist in &item.featured_artist_names {
        push_artist_once(&mut featured_artists, artist);
    }
    if featured_artists.is_empty() {
        push_artist_once(&mut featured_artists, display_artist);
    }
    Some(FedAppearsOn {
        release_title: item.release_title.clone().unwrap_or_default(),
        release_type: item.release_type.clone().unwrap_or_default(),
        year: item.year,
        track: FedCardTrack {
            title: item.name.clone(),
            artists,
            featured_artists,
            track_number: item.track_number,
            disc_number: item.disc_number,
            duration_seconds: item.duration_seconds,
            content_id: item.content_id.clone(),
            sources: vec![(
                item.owner.to_string(),
                audio::hex_encode(item.id.as_bytes()),
            )],
        },
    })
}

/// Converts a raw DHT record into the UI-facing federated track shape.
fn fed_track_from_item(item: music_dht::LibraryItem, own: EndpointId) -> FedTrack {
    FedTrack {
        item_id: audio::hex_encode(item.id.as_bytes()),
        owner: item.owner.to_string(),
        own: item.owner == own,
        title: item.name,
        artist_names: item.artist_names,
        featured_artist_names: item.featured_artist_names,
        year: item.year,
        duration_seconds: item.duration_seconds.map(|d| d.round() as i64),
        content_id: item.content_id,
        release_title: item.release_title,
        track_number: item.track_number,
        disc_number: item.disc_number,
    }
}

fn cached_appearance_hit(
    cached: &CachedTrackMetadata,
    normalized_artist: &str,
    display_artist: &str,
) -> Option<FedAppearsOn> {
    if !cached_has_artist(cached, normalized_artist) {
        return None;
    }
    let mut featured_artists = cached.featured_artists.clone();
    if featured_artists.is_empty() {
        push_artist_once(&mut featured_artists, display_artist);
    }
    Some(FedAppearsOn {
        release_title: cached.release_title.clone().unwrap_or_default(),
        release_type: cached.release_type.clone().unwrap_or_default(),
        year: cached.year,
        track: FedCardTrack {
            title: cached.title.clone(),
            artists: cached.artists.clone(),
            featured_artists,
            track_number: cached.track_number,
            disc_number: cached.disc_number,
            duration_seconds: cached.duration_seconds,
            content_id: cached.fed.content_id.clone(),
            sources: vec![(cached.fed.owner.clone(), cached.fed.item_id.clone())],
        },
    })
}

fn add_appearance_to_card(card: &mut FedArtistCard, appearance: FedAppearsOn) -> bool {
    let Some((owner, item_id)) = appearance.track.sources.first().cloned() else {
        return false;
    };
    if fed_releases_have_source(card, &owner, &item_id) {
        return false;
    }
    if let Some(existing) = card.appears_on.iter_mut().find(|existing| {
        existing
            .track
            .sources
            .iter()
            .any(|(source_owner, source_id)| source_owner == &owner && source_id == &item_id)
    }) {
        merge_dht_appearance(existing, appearance);
        return true;
    }
    if !card.owners.contains(&owner) {
        card.owners.push(owner);
    }
    if let Some(slot) = card
        .appears_on
        .iter_mut()
        .find(|existing| same_appearance(existing, &appearance))
    {
        merge_dht_appearance(slot, appearance);
    } else {
        card.appears_on.push(appearance);
    }
    true
}

fn same_appearance(left: &FedAppearsOn, right: &FedAppearsOn) -> bool {
    let left_release = music_dht::normalize_name(&left.release_title);
    let right_release = music_dht::normalize_name(&right.release_title);
    (left_release == right_release || left_release.is_empty() || right_release.is_empty())
        && music_dht::normalize_name(&left.track.title)
            == music_dht::normalize_name(&right.track.title)
        && (left.track.track_number == right.track.track_number
            || left.track.track_number.is_none()
            || right.track.track_number.is_none())
        && (left.year == right.year || left.year.is_none() || right.year.is_none())
}

fn fed_releases_have_source(card: &FedArtistCard, owner: &str, item_id: &str) -> bool {
    card.releases
        .iter()
        .flat_map(|release| release.tracks.iter())
        .any(|track| {
            track
                .sources
                .iter()
                .any(|(source_owner, source_id)| source_owner == owner && source_id == item_id)
        })
}

fn merge_dht_appearance(target: &mut FedAppearsOn, appearance: FedAppearsOn) {
    if target.release_title.is_empty() {
        target.release_title = appearance.release_title;
    }
    if target.release_type.is_empty() {
        target.release_type = appearance.release_type;
    }
    if target.year.is_none() {
        target.year = appearance.year;
    }
    if target.track.duration_seconds.is_none() {
        target.track.duration_seconds = appearance.track.duration_seconds;
    }
    if target.track.content_id.is_none() {
        target.track.content_id = appearance.track.content_id;
    }
    for artist in appearance.track.artists {
        push_artist_once(&mut target.track.artists, &artist);
    }
    for artist in appearance.track.featured_artists {
        push_artist_once(&mut target.track.featured_artists, &artist);
    }
    for source in appearance.track.sources {
        if !target.track.sources.contains(&source) {
            target.track.sources.push(source);
        }
    }
}

fn push_artist_once(names: &mut Vec<String>, name: &str) {
    if names
        .iter()
        .any(|existing| music_dht::normalize_name(existing) == music_dht::normalize_name(name))
    {
        return;
    }
    names.push(name.to_string());
}

fn artist_line(artists: &[String], featured_artists: &[String]) -> String {
    let mut main = Vec::new();
    for artist in artists {
        push_artist_once(&mut main, artist);
    }
    let mut featured = Vec::new();
    for artist in featured_artists {
        if !main
            .iter()
            .any(|name| music_dht::normalize_name(name) == music_dht::normalize_name(artist))
        {
            push_artist_once(&mut featured, artist);
        }
    }
    match (main.is_empty(), featured.is_empty()) {
        (false, false) => format!("{} feat. {}", main.join(", "), featured.join(", ")),
        (false, true) => main.join(", "),
        (true, false) => format!("feat. {}", featured.join(", ")),
        (true, true) => String::new(),
    }
}

fn sort_fed_appearances(appearances: &mut [FedAppearsOn]) {
    appearances.sort_by(|a, b| {
        b.year
            .unwrap_or(i32::MIN)
            .cmp(&a.year.unwrap_or(i32::MIN))
            .then_with(|| a.release_title.cmp(&b.release_title))
            .then_with(|| {
                a.track
                    .track_number
                    .unwrap_or(i32::MAX)
                    .cmp(&b.track.track_number.unwrap_or(i32::MAX))
            })
            .then_with(|| a.track.title.cmp(&b.track.title))
    });
}

async fn stop_running(running: Option<Running>) {
    let Some(running) = running else { return };
    for task in &running.tasks {
        task.abort();
    }
    if let Err(err) = running.service.shutdown().await {
        tracing::warn!("federation node shutdown reported an error: {err}");
    }
    tracing::info!("federation stopped");
}

/// Reads the local library and converts it into DHT item specs. Only names
/// and small metadata are shared — never file paths or the files themselves.
fn collect_specs(library: &Library) -> Result<Vec<ItemSpec>> {
    let export = library.federation_export()?;
    let mut specs = Vec::new();
    for (id, name) in export.artists {
        specs.push(ItemSpec {
            local_key: format!("artist:{id}"),
            kind: ItemKind::Artist,
            name,
            artist_names: Vec::new(),
            featured_artist_names: Vec::new(),
            year: None,
            release_type: None,
            release_title: None,
            track_number: None,
            disc_number: None,
            duration_seconds: None,
            content_id: None,
        });
    }
    for release in export.releases {
        specs.push(ItemSpec {
            local_key: format!("release:{}", release.id),
            kind: ItemKind::Release,
            name: release.title,
            artist_names: release.artist_names,
            featured_artist_names: Vec::new(),
            year: release.year,
            release_type: Some(release.release_type),
            release_title: None,
            track_number: None,
            disc_number: None,
            duration_seconds: None,
            content_id: None,
        });
    }
    for track in export.tracks {
        specs.push(ItemSpec {
            local_key: format!("track:{}", track.id),
            kind: ItemKind::Track,
            name: track.title,
            artist_names: track.artist_names,
            featured_artist_names: track.featured_artist_names,
            year: track.year,
            release_type: Some(track.release_type),
            release_title: Some(track.release_title),
            track_number: track.track_number,
            disc_number: track.disc_number,
            duration_seconds: (track.duration_seconds > 0.0).then_some(track.duration_seconds),
            content_id: track.content_id,
        });
    }
    Ok(specs)
}

fn sanitize_file_stem(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    let mut stem: String = trimmed.chars().take(120).collect();
    if stem.is_empty() {
        stem.push_str("track");
    }
    stem
}

fn download_stem(fed: &FedTrack) -> String {
    let artists = fed.artist_line();
    if artists.is_empty() {
        sanitize_file_stem(&fed.title)
    } else {
        sanitize_file_stem(&format!("{artists} - {}", fed.title))
    }
}

/// A queueable placeholder for a federated track that has not been
/// downloaded yet: it behaves like a regular track everywhere (queue, info,
/// selection) and resolves to a local file when playback reaches it.
pub fn pending_track(fed: &FedTrack) -> TrackItem {
    let id = NEXT_EPHEMERAL_ID.fetch_sub(1, Ordering::Relaxed);
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
        id,
        title: fed.title.clone(),
        track_number: fed.track_number,
        disc_number: fed.disc_number,
        duration_seconds: fed.duration_seconds.unwrap_or(0) as f64,
        artists: refs(&fed.artist_names),
        featured_artists: refs(&fed.featured_artist_names),
        release_id: -1,
        release_title: fed
            .release_title
            .clone()
            .unwrap_or_else(|| format!("federation · {}", fed.owner_short())),
        release_year: fed.year,
        file_path: String::new(),
        content_id: fed.content_id.clone(),
        cover_path: None,
        audio_format: None,
        audio_bitrate: None,
        audio_sample_rate: None,
        audio_bit_depth: None,
        file_size_bytes: None,
        play_count: 0,
        fed: Some(fed.clone()),
    }
}

/// A playable TrackItem for a downloaded-but-not-imported federated track.
fn ephemeral_track(
    fed: &FedTrack,
    metadata: Option<&TrackMetadata>,
    path: &std::path::Path,
) -> TrackItem {
    let id = NEXT_EPHEMERAL_ID.fetch_sub(1, Ordering::Relaxed);
    let file_size = std::fs::metadata(path).map(|m| m.len() as i64).ok();
    let refs = |names: &[String]| -> Vec<ArtistRef> {
        names
            .iter()
            .map(|name| ArtistRef {
                id: -1,
                name: name.clone(),
            })
            .collect()
    };
    let title = metadata
        .map(|m| m.title.trim())
        .filter(|t| !t.is_empty())
        .unwrap_or(&fed.title)
        .to_string();
    let artists = match metadata {
        Some(meta) if !meta.artists.is_empty() => refs(&meta.artists),
        _ => refs(&fed.artist_names),
    };
    let featured_artists = match metadata {
        Some(meta) => refs(&meta.featured_artists),
        None => refs(&fed.featured_artist_names),
    };
    let release_title = metadata
        .map(|m| m.release_title.trim())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .or_else(|| fed.release_title.clone())
        .unwrap_or_else(|| format!("federation · {}", fed.owner_short()));
    TrackItem {
        id,
        title,
        track_number: metadata.and_then(|m| m.track_number),
        disc_number: metadata.and_then(|m| m.disc_number),
        duration_seconds: metadata
            .and_then(|m| m.duration_seconds)
            .or_else(|| fed.duration_seconds.map(|duration| duration as f64))
            .unwrap_or(0.0),
        artists,
        featured_artists,
        release_id: -1,
        release_title,
        release_year: metadata.and_then(|m| m.year).or(fed.year),
        file_path: path.to_string_lossy().into_owned(),
        content_id: fed
            .content_id
            .clone()
            .or_else(|| crate::library::audio_content_id(&path.to_string_lossy())),
        cover_path: None,
        audio_format: metadata.and_then(|m| m.audio_format.clone()).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        }),
        audio_bitrate: metadata.and_then(|m| m.audio_bitrate),
        audio_sample_rate: metadata.and_then(|m| m.audio_sample_rate),
        audio_bit_depth: metadata.and_then(|m| m.audio_bit_depth),
        file_size_bytes: file_size,
        play_count: 0,
        // Keep the federation reference: the cached track can still be
        // liked, re-downloaded and marked as federated in the lists.
        fed: Some(fed.clone()),
    }
}

#[cfg(test)]
mod tests {
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

        let hit =
            dht_appearance_hit(&dht_track(&["Host"], &["Guest"]), &normalized, "Guest").unwrap();
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
}
