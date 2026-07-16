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

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use music_dht::{
    EndpointId, ItemKind, ItemSpec, MusicDhtConfig, MusicDhtService, NetworkId, PeerTicket,
    RendezvousConfig,
};
use serde::{Deserialize, Serialize};

use crate::library::Library;
use crate::library::models::{ArtistRef, TrackItem};

pub use audio::{AUDIO_ALPN, TrackMetadata};

/// How often the published library is re-synchronized with the local index.
const SYNC_INTERVAL: Duration = Duration::from_secs(60);

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
    pub year: Option<i32>,
    pub duration_seconds: Option<i64>,
}

impl FedTrack {
    pub fn artist_line(&self) -> String {
        self.artist_names.join(", ")
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
    pub connected_peers: Vec<String>,
    pub known_contacts: usize,
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
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

pub struct Federation {
    library: Arc<Library>,
    data_dir: PathBuf,
    cache_dir: PathBuf,
    media_dir: PathBuf,
    settings: std::sync::Mutex<FedSettings>,
    running: tokio::sync::Mutex<Option<Running>>,
    last_sync: std::sync::Mutex<Option<String>>,
    last_error: std::sync::Mutex<Option<String>>,
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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

impl Federation {
    pub fn new(library: Arc<Library>) -> Arc<Self> {
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
        Arc::new(Self {
            library,
            data_dir,
            cache_dir,
            media_dir,
            settings: std::sync::Mutex::new(load_settings()),
            running: tokio::sync::Mutex::new(None),
            last_sync: std::sync::Mutex::new(None),
            last_error: std::sync::Mutex::new(None),
        })
    }

    pub fn settings(&self) -> FedSettings {
        lock(&self.settings).clone()
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
        let mut guard = self.running.lock().await;
        if let Some(running) = guard.as_ref() {
            if running.network_name == network_name {
                return Ok(());
            }
            stop_running(guard.take()).await;
        }

        let config = MusicDhtConfig::builder()
            .data_dir(&self.data_dir)
            .network_id(NetworkId::from_name(&network_name))
            // Peers of the network find each other knowing only its name.
            .rendezvous(RendezvousConfig::default())
            // Peers stream each other's audio over this protocol.
            .stream_protocol(AUDIO_ALPN)
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
                sync_self.sync_once(&sync_service).await;
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

        *guard = Some(Running {
            service,
            network_name,
            tasks: vec![event_task, sync_task, audio_task],
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
            tokio::spawn(async move { fed.sync_once(&service).await });
        }
    }

    pub async fn sync_now(self: &Arc<Self>) -> Result<()> {
        let service = self.service().await?;
        self.sync_once(&service).await;
        Ok(())
    }

    async fn sync_once(&self, service: &MusicDhtService) {
        let library = Arc::clone(&self.library);
        let specs = tokio::task::spawn_blocking(move || collect_specs(&library)).await;
        let specs = match specs {
            Ok(Ok(specs)) => specs,
            Ok(Err(err)) => {
                tracing::warn!("federation sync: library read failed: {err:#}");
                self.set_error(Some(format!("library read failed: {err}")));
                return;
            }
            Err(err) => {
                tracing::warn!("federation sync task failed: {err}");
                return;
            }
        };
        match service.sync_library(specs).await {
            Ok(stats) => {
                *lock(&self.last_sync) = Some(format!(
                    "{} (+{} ~{} −{}, unchanged {})",
                    now_label(),
                    stats.added,
                    stats.updated,
                    stats.removed,
                    stats.unchanged
                ));
                self.set_error(None);
            }
            Err(err) => {
                tracing::warn!("federation sync failed: {err}");
                self.set_error(Some(format!("sync failed: {err}")));
            }
        }
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
            status.connected_peers = service
                .connected_peers()
                .iter()
                .map(|p| p.to_string())
                .collect();
            status.known_contacts = service.known_peers().len();
            status.published_items = service
                .list_local_items()
                .await
                .map(|items| items.len())
                .unwrap_or(0);
        }
        status
    }

    /// Searches the federated network for tracks matching `query`.
    pub async fn search(&self, query: &str) -> Result<Vec<FedTrack>> {
        let service = self.service().await?;
        let outcome = service
            .search_network(query)
            .await
            .map_err(|err| anyhow::anyhow!("federated search failed: {err}"))?;
        let own = service.endpoint_id();
        Ok(outcome
            .network_results
            .iter()
            .filter(|item| item.kind == ItemKind::Track)
            .map(|item| FedTrack {
                item_id: audio::hex_encode(item.id.as_bytes()),
                owner: item.owner.to_string(),
                own: item.owner == own,
                title: item.name.clone(),
                artist_names: item.artist_names.clone(),
                year: item.year,
                duration_seconds: item.duration_seconds.map(|d| d.round() as i64),
            })
            .collect())
    }

    pub async fn ticket(&self) -> Result<String> {
        let service = self.service().await?;
        let ticket = service
            .ticket()
            .await
            .map_err(|err| anyhow::anyhow!("cannot create a ticket: {err}"))?;
        Ok(ticket.to_string())
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

    /// Prepares a federated track for playback: local tracks resolve
    /// straight to the library; remote tracks are downloaded — into the
    /// library when save-on-listen is enabled, into the cache otherwise.
    pub async fn prepare_playback(self: &Arc<Self>, fed: &FedTrack) -> Result<FedPlayable> {
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
        let save = self.settings().save_on_listen;
        let dir = if save { &self.media_dir } else { &self.cache_dir };
        tokio::fs::create_dir_all(dir).await?;

        let (path, mime, metadata) =
            audio::download_track(&service, owner, &fed.item_id, dir, &download_stem(fed)).await?;
        tracing::info!(path = %path.display(), %mime, "federated track downloaded");

        if save {
            let library = Arc::clone(&self.library);
            let import_path = path.clone();
            let import_metadata = metadata.clone();
            let imported = tokio::task::spawn_blocking(move || -> Result<Option<TrackItem>> {
                let mut import = crate::library::import::read_file(&import_path)?;
                // The owner's database is more authoritative than whatever
                // tags the file happens to carry (often none at all).
                if let Some(meta) = &import_metadata {
                    apply_remote_metadata(&mut import, meta);
                }
                let (track_id, _) = crate::library::import::upsert_track(&library, &import)?;
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
                    tracing::warn!("importing the downloaded track failed: {err:#}; playing from the file");
                }
            }
        }

        Ok(FedPlayable {
            track: ephemeral_track(fed, metadata.as_ref(), &path),
            imported: false,
        })
    }
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
            year: None,
            release_type: None,
            duration_seconds: None,
        });
    }
    for release in export.releases {
        specs.push(ItemSpec {
            local_key: format!("release:{}", release.id),
            kind: ItemKind::Release,
            name: release.title,
            artist_names: release.artist_names,
            year: release.year,
            release_type: Some(release.release_type),
            duration_seconds: None,
        });
    }
    for track in export.tracks {
        specs.push(ItemSpec {
            local_key: format!("track:{}", track.id),
            kind: ItemKind::Track,
            name: track.title,
            artist_names: track.artist_names,
            year: track.year,
            release_type: None,
            duration_seconds: (track.duration_seconds > 0.0).then_some(track.duration_seconds),
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
    let release_title = metadata
        .map(|m| m.release_title.trim())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .unwrap_or_else(|| format!("federation · {}", fed.owner_short()));
    TrackItem {
        id,
        title,
        track_number: metadata.and_then(|m| m.track_number),
        disc_number: metadata.and_then(|m| m.disc_number),
        duration_seconds: fed.duration_seconds.unwrap_or(0) as f64,
        artists,
        featured_artists: metadata.map(|m| refs(&m.featured_artists)).unwrap_or_default(),
        release_id: -1,
        release_title,
        release_year: metadata.and_then(|m| m.year).or(fed.year),
        file_path: path.to_string_lossy().into_owned(),
        cover_path: None,
        audio_format: path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_string()),
        audio_bitrate: None,
        audio_sample_rate: None,
        audio_bit_depth: None,
        file_size_bytes: file_size,
        play_count: 0,
    }
}
