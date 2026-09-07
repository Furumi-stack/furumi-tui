pub mod action;
mod cmdline;
pub mod command;
pub mod event;
pub mod input;
pub(crate) mod popup;
pub mod state;
pub mod update;

use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crokey::KeyCombination;
use crossterm::event::{Event as TermEvent, EventStream, KeyEvent, KeyEventKind};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::config::keymap::{KeyResolution, Keymap};
use crate::library::Library;
use crate::player;
use crate::ui;
use event::AppEvent;
use state::AppState;
use update::{Effect, update};

const TICK_INTERVAL: Duration = Duration::from_millis(250);
const VISUALIZER_TICK_INTERVAL: Duration = Duration::from_millis(50);
const ACTIVE_IDLE_LEASE_MS: i64 = 5 * 60 * 1000;

/// Handles shared by background tasks; AppState stays pure UI data.
pub struct Runtime {
    pub event_tx: mpsc::UnboundedSender<AppEvent>,
    pub library: Arc<Library>,
    pub devices: Arc<crate::devices::DeviceSync>,
    pub jam: Arc<crate::jam::JamManager>,
    pub federation: Arc<crate::federation::Federation>,
    pub similarity: Arc<crate::similarity::Manager>,
    /// When the last Federation-tab status snapshot was requested.
    pub fed_status_at: Option<std::time::Instant>,
    /// Keeps the last successful local-data snapshot visible while a newer
    /// one is calculated and collapses bursts of library-change events.
    pub local_library_stats_refreshing: Arc<std::sync::atomic::AtomicBool>,
    pub local_library_stats_refresh_requested: Arc<std::sync::atomic::AtomicBool>,
    pub library_network_refresh_at: Option<std::time::Instant>,
    pub library_network_refreshing: Arc<std::sync::atomic::AtomicBool>,
    pub library_network_cursors:
        Arc<std::sync::Mutex<std::collections::HashMap<String, Option<String>>>>,
    pub library_network_done: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    pub library_network_mode: crate::config::settings::LibrarySourceMode,
    pub library_network_art_fetching: Arc<std::sync::atomic::AtomicBool>,
    pub library_network_art_attempted: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Coalesces urgent personal-device syncs after remote playback commands.
    pub device_sync_running: Arc<std::sync::atomic::AtomicBool>,
    pub device_sync_requested: Arc<std::sync::atomic::AtomicBool>,
    /// Stable playback keys of federated tracks being resolved right now.
    pub fed_resolving: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Stable playback keys that already started through a streaming reader.
    pub fed_streaming: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Caps concurrent artwork loads so they never starve the disk.
    pub art_semaphore: Arc<tokio::sync::Semaphore>,
    /// The terminal screen was externally disturbed and needs a full repaint.
    pub force_redraw: bool,
    /// The alternate screen is temporarily suspended for copy-friendly text.
    pub plain_text_mode: bool,
    /// Monotonic sequence for live search; stale responses are dropped.
    pub search_seq: Arc<std::sync::atomic::AtomicU64>,
    pub player: player::Controller,
    pub player_start_pending: bool,
    pub media_tx: std::sync::mpsc::Sender<crate::media::MediaUpdate>,
    pub last_media_push: Option<std::time::Instant>,
    pub status_publisher: crate::status::Publisher,
}

#[derive(Debug, Clone, Copy)]
struct StreamingPlaybackRequest {
    volume: u8,
    paused: bool,
    position_secs: f64,
}

fn now_epoch_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn err_string(err: anyhow::Error) -> String {
    format!("{err:#}")
}

fn refresh_local_content_ids(runtime: &Runtime) {
    let library = Arc::clone(&runtime.library);
    let tx = runtime.event_tx.clone();
    tokio::task::spawn_blocking(move || {
        let result = library.local_content_ids().map_err(err_string);
        let _ = tx.send(AppEvent::LocalContentIdsLoaded(result));
    });
}

fn refresh_local_library_stats(runtime: &Runtime) {
    runtime
        .local_library_stats_refresh_requested
        .store(true, std::sync::atomic::Ordering::Release);
    if runtime
        .local_library_stats_refreshing
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        return;
    }
    let library = Arc::clone(&runtime.library);
    let tx = runtime.event_tx.clone();
    let refreshing = Arc::clone(&runtime.local_library_stats_refreshing);
    let requested = Arc::clone(&runtime.local_library_stats_refresh_requested);
    tokio::task::spawn_blocking(move || {
        loop {
            requested.store(false, std::sync::atomic::Ordering::Release);
            let result = library.local_stats().map_err(err_string);
            let _ = tx.send(AppEvent::LocalLibraryStatsLoaded(result));
            if requested.load(std::sync::atomic::Ordering::Acquire) {
                continue;
            }
            refreshing.store(false, std::sync::atomic::Ordering::Release);
            if requested.swap(false, std::sync::atomic::Ordering::AcqRel)
                && !refreshing.swap(true, std::sync::atomic::Ordering::AcqRel)
            {
                continue;
            }
            break;
        }
    });
}

pub(super) fn validate_music_directory(state: &mut AppState, runtime: &Runtime, path: PathBuf) {
    if state.music_dir_changing {
        state.status_message = Some("music directory change is already running".into());
        return;
    }
    state.music_dir_changing = true;
    state.status_message = Some("checking music directory write access…".into());
    let tx = runtime.event_tx.clone();
    tokio::task::spawn_blocking(move || {
        let result = Library::validate_music_directory(&path).map_err(err_string);
        let _ = tx.send(AppEvent::MusicDirectoryValidated(result));
    });
}

fn spawn_artist_federation_enrichment(runtime: &Runtime, id: i64, name: String) {
    let fed = Arc::clone(&runtime.federation);
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let result = fed
            .artist_card(&name)
            .await
            .map_err(|err| format!("{err:#}"));
        let card = result.as_ref().ok().cloned();
        let _ = tx.send(AppEvent::ArtistFederationLoaded {
            id,
            name: name.clone(),
            result,
        });
        let Some(card) = card else { return };
        if let Some(path) = fed.card_image(&card.owners, &name, None).await {
            let _ = tx.send(AppEvent::FedCardArt {
                name: name.clone(),
                release: None,
                path,
            });
        }
        for release in &card.releases {
            let Some(path) = fed
                .card_image(&release.owners, &name, Some(&release.title))
                .await
            else {
                continue;
            };
            let _ = tx.send(AppEvent::FedCardArt {
                name: name.clone(),
                release: Some(release.title.clone()),
                path,
            });
        }
    });
}

fn maybe_enrich_open_artist(state: &mut AppState, runtime: &Runtime) {
    if !state.federation.settings.enabled || state.active_tab != state::Tab::Global {
        return;
    }
    let Some(state::GlobalView::Artist { id, .. }) = state.global.stack.last().copied() else {
        return;
    };
    if state.artist_fed_views.contains_key(&id) {
        return;
    }
    let Some(state::Loadable::Ready(detail)) = state.artist_views.get(&id) else {
        return;
    };
    let name = detail.name.clone();
    state.artist_fed_views.insert(id, state::Loadable::Loading);
    spawn_artist_federation_enrichment(runtime, id, name);
}

fn spawn_content_id_backfill(runtime: &Runtime) {
    let library = Arc::clone(&runtime.library);
    let federation = Arc::clone(&runtime.federation);
    let devices = Arc::clone(&runtime.devices);
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let stats =
            match tokio::task::spawn_blocking(move || library.backfill_missing_content_ids()).await
            {
                Ok(Ok(stats)) => stats,
                Ok(Err(err)) => {
                    tracing::warn!("content id backfill failed: {err:#}");
                    let _ = tx.send(AppEvent::StatusMessage(format!(
                        "content id backfill failed: {err:#}"
                    )));
                    return;
                }
                Err(err) => {
                    tracing::warn!("content id backfill task failed: {err}");
                    let _ = tx.send(AppEvent::StatusMessage(format!(
                        "content id backfill task failed: {err}"
                    )));
                    return;
                }
            };

        if stats.updated() == 0 {
            if stats.failed > 0 {
                tracing::warn!(
                    checked = stats.checked,
                    failed = stats.failed,
                    "content id backfill finished with unreadable tracks"
                );
            }
            return;
        }

        tracing::info!(
            checked = stats.checked,
            normalized = stats.normalized,
            hashed = stats.hashed,
            failed = stats.failed,
            "content id backfill completed"
        );
        let _ = tx.send(AppEvent::LibraryChanged {
            message: Some(format!("content ids: indexed {} track(s)", stats.updated())),
        });

        if federation.status().await.running {
            if let Err(err) = federation.sync_now().await {
                tracing::warn!("federation sync after content id backfill failed: {err:#}");
            }
            let _ = tx.send(AppEvent::FederationStatus(federation.status().await));
            let _ = tx.send(AppEvent::DeviceSyncStatus(devices.status()));
        }
    });
}

pub async fn run(
    mut terminal: DefaultTerminal,
    mut keymap: Keymap,
    startup_warning: Option<String>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    mut event_rx: mpsc::UnboundedReceiver<AppEvent>,
    media_tx: std::sync::mpsc::Sender<crate::media::MediaUpdate>,
) -> Result<()> {
    let db_path = crate::library::default_db_path()?;
    let library = Arc::new(Library::open(&db_path)?);
    tracing::info!(path = %db_path.display(), "library opened");

    let (settings, settings_warning) = crate::config::settings::load();
    let status_message = match (startup_warning, settings_warning) {
        (Some(left), Some(right)) => Some(format!("{left}; {right}")),
        (Some(message), None) | (None, Some(message)) => Some(message),
        (None, None) => None,
    };
    let mut state = AppState {
        status_message,
        ..AppState::default()
    };
    state.player.volume = settings.volume;
    state.global.filters = settings.library;
    state.music_dir = settings.music_dir.clone();
    state.similarity.settings = settings.similarity.clone();
    if let Err(err) = state.visualizer.load_library() {
        state.status_message = Some(format!("visualizations disabled: {err:#}"));
    }

    let devices = crate::devices::DeviceSync::new(Arc::clone(&library))?;
    devices.set_event_tx(event_tx.clone());
    let jam = crate::jam::JamManager::new(event_tx.clone());
    let similarity = crate::similarity::Manager::new(
        Arc::clone(&library),
        event_tx.clone(),
        settings.similarity.clone(),
    );
    state.similarity.status = similarity.status();
    let federation = crate::federation::Federation::new(
        Arc::clone(&library),
        Arc::clone(&devices),
        Arc::clone(&jam),
        Arc::clone(&similarity),
        settings.music_dir.clone(),
    );
    federation.start_supervisor();
    state.music_dir = federation.media_dir();
    state.federation.settings = federation.settings();
    state.federation.devices = Some(devices.status());
    if let Ok((device_id, device_name)) = devices.identity_summary() {
        state.device_playback.self_device_id = device_id.clone();
        state.device_playback.self_device_name = device_name.clone();
        state.device_playback.active_device_id = Some(device_id);
        state.device_playback.active_device_name = Some(device_name);
        state.device_playback.startup_takeover_pending = true;
    }
    let player_events = event_tx.clone();
    let mut runtime = Runtime {
        event_tx,
        library,
        devices,
        jam,
        federation,
        similarity,
        fed_status_at: None,
        local_library_stats_refreshing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        local_library_stats_refresh_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        library_network_refresh_at: None,
        library_network_refreshing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        library_network_cursors: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        library_network_done: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        library_network_mode: state.global.filters.source_mode,
        library_network_art_fetching: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        library_network_art_attempted: Arc::new(std::sync::Mutex::new(
            std::collections::HashSet::new(),
        )),
        device_sync_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        device_sync_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        fed_resolving: std::sync::Mutex::new(std::collections::HashSet::new()),
        fed_streaming: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        art_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        force_redraw: false,
        plain_text_mode: false,
        search_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        player: player::spawn(move |event| {
            let _ = player_events.send(AppEvent::Player(event));
        }),
        player_start_pending: false,
        media_tx,
        last_media_push: None,
        status_publisher: crate::status::Publisher::spawn(),
    };
    spawn_content_id_backfill(&runtime);
    if state.similarity.settings.enabled {
        runtime.similarity.start();
    }

    {
        let fed = Arc::clone(&runtime.federation);
        let devices = Arc::clone(&runtime.devices);
        let tx = runtime.event_tx.clone();
        tokio::spawn(async move {
            fed.start_if_enabled().await;
            let status = fed.status().await;
            let _ = tx.send(AppEvent::FederationStatus(status));
            let _ = tx.send(AppEvent::DeviceSyncStatus(devices.status()));
        });
    }

    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(TICK_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut visual_tick = tokio::time::interval(VISUALIZER_TICK_INTERVAL);
    visual_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        let plain_text = match state.popup.as_ref() {
            Some(state::Popup::PlainText { text }) => Some(text.as_str()),
            _ => None,
        };
        match (runtime.plain_text_mode, plain_text) {
            (false, Some(text)) => {
                enter_plain_text_mode(text)?;
                runtime.plain_text_mode = true;
            }
            (true, None) => {
                leave_plain_text_mode()?;
                runtime.plain_text_mode = false;
                runtime.force_redraw = true;
            }
            _ => {}
        }

        if !runtime.plain_text_mode {
            if runtime.force_redraw {
                terminal.clear()?;
                runtime.force_redraw = false;
            }
            terminal.draw(|frame| ui::draw(frame, &state, &keymap))?;
        }

        tokio::select! {
            maybe_event = input.next() => match maybe_event {
                Some(Ok(event)) => handle_terminal_event(&mut state, &mut keymap, &mut runtime, event),
                Some(Err(err)) => return Err(err.into()),
                None => state.should_quit = true,
            },
            Some(app_event) = event_rx.recv() => handle_app_event(&mut state, &mut runtime, app_event),
            _ = tick.tick() => {
                state.advance_spinner();
                expire_quit_confirmation(&mut state);
                sync_player_shared(&mut state, &runtime);
                runtime
                    .status_publisher
                    .publish(crate::status::PlaybackStatus::from_player(&state.player));
                maybe_prefetch_next(&mut state, &runtime);
                push_media_update(&state, &mut runtime, false);
            }
            _ = visual_tick.tick(), if state.visualizer.active => {
                sync_player_shared(&mut state, &runtime);
            }
        }

        if state.should_quit {
            // A replacement must finish before the runtime/process is torn down.
            if state.updater.busy {
                state.should_quit = false;
                state.status_message =
                    Some("Please wait for the update operation to finish".into());
                continue;
            }
            if runtime.plain_text_mode {
                leave_plain_text_mode()?;
                runtime.plain_text_mode = false;
            }
            state.shutting_down = true;
            terminal.draw(|frame| ui::draw(frame, &state, &keymap))?;
            runtime.federation.shutdown().await;
            return Ok(());
        }
        maintenance(&mut state, &mut runtime);
    }
}

fn enter_plain_text_mode(text: &str) -> Result<()> {
    let text: String = text
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    crossterm::execute!(
        io::stdout(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
        crossterm::style::Print(text)
    )?;
    io::stdout().flush()?;
    Ok(())
}

fn leave_plain_text_mode() -> Result<()> {
    crossterm::execute!(
        io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste
    )?;
    Ok(())
}

fn sync_player_shared(state: &mut AppState, runtime: &Runtime) {
    if state.device_playback.is_control() {
        extrapolate_control_position(state);
    } else if state.player.current.is_some() && !runtime.player_start_pending {
        state.player.position_secs = runtime.player.shared.position().as_secs_f64();
        state.player.paused = runtime.player.shared.paused();
    }
    state.player.audio_analysis = if state.device_playback.is_control() {
        player::AudioAnalysisSnapshot::default()
    } else {
        runtime.player.shared.audio_analysis()
    };
    if state.device_playback.is_audio_owner() {
        publish_playback_snapshot(state, runtime);
    }
}

fn unix_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn playback_repeat_to_wire(mode: state::RepeatMode) -> crate::devices::PlaybackRepeat {
    match mode {
        state::RepeatMode::Off => crate::devices::PlaybackRepeat::Off,
        state::RepeatMode::One => crate::devices::PlaybackRepeat::One,
        state::RepeatMode::All => crate::devices::PlaybackRepeat::All,
    }
}

fn playback_repeat_from_wire(mode: crate::devices::PlaybackRepeat) -> state::RepeatMode {
    match mode {
        crate::devices::PlaybackRepeat::Off => state::RepeatMode::Off,
        crate::devices::PlaybackRepeat::One => state::RepeatMode::One,
        crate::devices::PlaybackRepeat::All => state::RepeatMode::All,
    }
}

fn playback_state_from_ui(state: &AppState) -> crate::devices::PlaybackStateWire {
    crate::devices::PlaybackStateWire {
        queue: state
            .player
            .queue
            .iter()
            .map(crate::devices::PlaybackTrack::from_track)
            .collect(),
        queue_pos: state.player.queue_pos,
        playing: state.player.playing,
        paused: state.player.paused,
        idle_since_ms: (!state.player.playing || state.player.paused)
            .then_some(state.device_playback.local_idle_since_ms)
            .flatten(),
        position_secs: state.player.position_secs,
        volume: state.player.volume,
        shuffle: state.player.shuffle,
        repeat: playback_repeat_to_wire(state.player.repeat),
    }
}

fn playback_track_to_ui(
    wire: &crate::devices::PlaybackTrack,
    library: Option<&Library>,
) -> crate::library::models::TrackItem {
    if let (Some(library), Some(content_id)) = (library, wire.content_id.as_deref())
        && let Ok(Some(track)) = library.track_by_content_id(content_id)
    {
        return track;
    }
    wire.to_track_item()
}

fn track_playback_key(track: &crate::library::models::TrackItem) -> String {
    state::track_key(track)
}

fn apply_playback_state_to_ui(
    state: &mut AppState,
    wire: &crate::devices::PlaybackStateWire,
    library: Option<&Library>,
) {
    let queue: Vec<_> = wire
        .queue
        .iter()
        .map(|track| playback_track_to_ui(track, library))
        .collect();
    let queue_pos = queue
        .iter()
        .take(wire.queue_pos)
        .filter(|track| update::track_allowed_by_source_mode(state, track))
        .count();
    state.player.queue = queue
        .into_iter()
        .filter(|track| update::track_allowed_by_source_mode(state, track))
        .collect();
    state.player.queue_pos = queue_pos.min(state.player.queue.len().saturating_sub(1));
    state.player.play_next_end = None;
    state.player.playing = wire.playing && !state.player.queue.is_empty();
    state.player.paused = wire.paused;
    state.device_playback.local_idle_since_ms = if state.player.playing && !state.player.paused {
        None
    } else {
        wire.idle_since_ms.or_else(|| Some(unix_time_ms()))
    };
    state.player.position_secs = wire.position_secs.max(0.0);
    state.player.volume = wire.volume.min(100);
    state.player.shuffle = wire.shuffle;
    state.player.repeat = playback_repeat_from_wire(wire.repeat);
    state.player.prefetched_pos = None;
    state.player.original_order = None;
    state.player.current = state
        .player
        .playing
        .then(|| state.player.queue.get(state.player.queue_pos).cloned())
        .flatten();
    if !state.player.playing {
        state.player.current = None;
        state.player.paused = false;
    }
    state.queue_tab.cursor = state
        .queue_tab
        .cursor
        .min(state.player.queue.len().saturating_sub(1));
}

fn publish_playback_snapshot(state: &mut AppState, runtime: &Runtime) {
    if !state.device_playback.is_audio_owner() {
        return;
    }
    publish_playback_snapshot_with_active(state, runtime, true);
}

fn publish_inactive_playback_snapshot(state: &mut AppState, runtime: &Runtime) {
    publish_playback_snapshot_with_active(state, runtime, false);
}

fn publish_playback_snapshot_with_active(state: &mut AppState, runtime: &Runtime, active: bool) {
    let Ok((device_id, device_name)) = runtime.devices.identity_summary() else {
        return;
    };
    update_local_idle_since(state);
    state.device_playback.self_device_id = device_id.clone();
    state.device_playback.self_device_name = device_name.clone();
    if active {
        state.device_playback.active_device_id = Some(device_id.clone());
        state.device_playback.active_device_name = Some(device_name.clone());
    }
    let snapshot = crate::devices::PlaybackSnapshot {
        device_id,
        device_name,
        active,
        updated_at_ms: unix_time_ms(),
        state: playback_state_from_ui(state),
    };
    runtime.devices.publish_playback(snapshot);
    if state.device_playback.role == state::DevicePlaybackRole::Jam
        && state.device_playback.jam_host
    {
        runtime
            .jam
            .publish_host_playback(crate::devices::PlaybackSnapshot {
                device_id: state.device_playback.self_device_id.clone(),
                device_name: state.device_playback.self_device_name.clone(),
                active: true,
                updated_at_ms: unix_time_ms(),
                state: playback_state_from_ui(state),
            });
    }
}

fn update_local_idle_since(state: &mut AppState) {
    if !state.player.playing || state.player.paused {
        if state.device_playback.local_idle_since_ms.is_none() {
            state.device_playback.local_idle_since_ms = Some(unix_time_ms());
        }
    } else {
        state.device_playback.local_idle_since_ms = None;
    }
}

fn active_snapshot_idle_since(snapshot: &crate::devices::PlaybackSnapshot) -> Option<i64> {
    if snapshot.state.playing && !snapshot.state.paused {
        None
    } else {
        snapshot
            .state
            .idle_since_ms
            .or(Some(snapshot.updated_at_ms))
    }
}

fn active_idle_lease_expired(snapshot: &crate::devices::PlaybackSnapshot, now: i64) -> bool {
    active_snapshot_idle_since(snapshot)
        .is_some_and(|idle_since| now.saturating_sub(idle_since) >= ACTIVE_IDLE_LEASE_MS)
}

fn local_active_lease_protected(state: &mut AppState, now: i64) -> bool {
    if !state.device_playback.is_audio_owner() || !state.player.playing {
        return false;
    }
    if !state.player.paused {
        return true;
    }
    update_local_idle_since(state);
    state
        .device_playback
        .local_idle_since_ms
        .is_some_and(|idle_since| now.saturating_sub(idle_since) < ACTIVE_IDLE_LEASE_MS)
}

fn extrapolate_control_position(state: &mut AppState) {
    let Some(snapshot) = state.device_playback.last_remote_snapshot.as_ref() else {
        return;
    };
    if !snapshot.state.playing {
        return;
    }
    let elapsed = if snapshot.state.paused {
        0.0
    } else {
        (unix_time_ms().saturating_sub(snapshot.updated_at_ms) as f64 / 1000.0).max(0.0)
    };
    let duration = state
        .player
        .current
        .as_ref()
        .map(|track| track.duration_seconds)
        .unwrap_or(0.0);
    let position = snapshot.state.position_secs + elapsed;
    state.player.position_secs = if duration > 0.0 {
        position.min(duration)
    } else {
        position
    };
}

pub(crate) fn become_control_device(
    state: &mut AppState,
    runtime: &Runtime,
    snapshot: crate::devices::PlaybackSnapshot,
) {
    if state.device_playback.is_audio_owner() {
        runtime.player.stop();
        publish_inactive_playback_snapshot(state, runtime);
    }
    state.device_playback.role = state::DevicePlaybackRole::Control;
    state.device_playback.active_device_id = Some(snapshot.device_id.clone());
    state.device_playback.active_device_name = Some(snapshot.device_name.clone());
    state.device_playback.last_remote_snapshot = Some(snapshot.clone());
    state
        .device_playback
        .remote
        .insert(snapshot.device_id.clone(), snapshot.clone());
    apply_playback_state_to_ui(state, &snapshot.state, Some(runtime.library.as_ref()));
    extrapolate_control_position(state);
    state.status_message = Some(format!("controlling {}", snapshot.device_name));
}

pub(crate) fn become_active_device(state: &mut AppState, runtime: &mut Runtime, start_audio: bool) {
    let was_control = state.device_playback.role == state::DevicePlaybackRole::Control;
    state.device_playback.role = state::DevicePlaybackRole::Active;
    state.device_playback.jam_host = false;
    let Ok((device_id, device_name)) = runtime.devices.identity_summary() else {
        return;
    };
    state.device_playback.self_device_id = device_id.clone();
    state.device_playback.self_device_name = device_name.clone();
    state.device_playback.active_device_id = Some(device_id);
    state.device_playback.active_device_name = Some(device_name);
    state.device_playback.last_remote_snapshot = None;
    state.device_playback.local_idle_since_ms = None;
    if was_control && start_audio && state.player.playing {
        start_current_audio(
            state,
            runtime,
            state.player.position_secs,
            state.player.paused,
        );
    }
    publish_playback_snapshot(state, runtime);
}

pub(crate) fn transfer_active_to_this_device(state: &mut AppState, runtime: &mut Runtime) {
    if state.device_playback.is_audio_owner() {
        publish_playback_snapshot(state, runtime);
        request_urgent_device_sync(runtime);
        return;
    }
    extrapolate_control_position(state);
    let previous_active_id = state.device_playback.active_device_id.clone();
    let should_start = state.player.current.is_some() || !state.player.queue.is_empty();
    if state.player.current.is_none() && !state.player.queue.is_empty() {
        state.player.current = state.player.queue.get(state.player.queue_pos).cloned();
        state.player.position_secs = 0.0;
    }
    if state.player.current.is_some() {
        state.player.playing = true;
        state.player.paused = false;
    }
    become_active_device(state, runtime, false);
    if should_start && state.player.current.is_some() {
        start_current_audio(state, runtime, state.player.position_secs, false);
        push_media_metadata(state, runtime);
        push_media_update(state, runtime, true);
    }
    publish_playback_snapshot(state, runtime);
    record_active_handoff(state, runtime, previous_active_id);
    request_urgent_device_sync(runtime);
}

pub(crate) fn transfer_active_to_remote_device(
    state: &mut AppState,
    runtime: &mut Runtime,
    target_device_id: String,
    target_device_name: String,
) {
    if target_device_id.trim().is_empty()
        || target_device_id == state.device_playback.self_device_id
    {
        transfer_active_to_this_device(state, runtime);
        return;
    }
    extrapolate_control_position(state);
    let previous_active_id = state.device_playback.active_device_id.clone();
    if state.player.current.is_none() && !state.player.queue.is_empty() {
        state.player.current = state.player.queue.get(state.player.queue_pos).cloned();
    }
    let wire = playback_state_from_ui(state);
    let command = crate::devices::PlaybackCommand::ActiveChanged {
        active_device_id: target_device_id.clone(),
        active_device_name: target_device_name.clone(),
        state: wire.clone(),
    };
    record_playback_command_async(
        runtime,
        target_device_id.clone(),
        command.clone(),
        "device handoff",
    );
    if let Some(previous) = previous_active_id
        && previous != target_device_id
        && previous != state.device_playback.self_device_id
    {
        record_playback_command_async(runtime, previous, command.clone(), "device handoff");
    }
    let snapshot = crate::devices::PlaybackSnapshot {
        device_id: target_device_id.clone(),
        device_name: target_device_name.clone(),
        active: true,
        updated_at_ms: unix_time_ms(),
        state: wire,
    };
    become_control_device(state, runtime, snapshot);
    request_urgent_device_sync(runtime);
    state.status_message = Some(format!("active playback moved to {target_device_name}"));
}

fn record_active_handoff(
    state: &mut AppState,
    runtime: &Runtime,
    previous_active_id: Option<String>,
) {
    let Some(target) = previous_active_id else {
        return;
    };
    if target == state.device_playback.self_device_id {
        return;
    }
    let command = crate::devices::PlaybackCommand::ActiveChanged {
        active_device_id: state.device_playback.self_device_id.clone(),
        active_device_name: state.device_playback.self_device_name.clone(),
        state: playback_state_from_ui(state),
    };
    record_playback_command_async(runtime, target, command, "device handoff");
}

fn record_control_playback_state(state: &mut AppState, runtime: &Runtime, seek: bool) {
    if !state.device_playback.is_control() {
        return;
    }
    update_local_idle_since(state);
    let Some(target) = state.device_playback.active_device_id.clone() else {
        return;
    };
    let command = crate::devices::PlaybackCommand::SetState {
        state: playback_state_from_ui(state),
        seek,
    };
    if state.device_playback.role == state::DevicePlaybackRole::Jam {
        if let Err(err) = runtime.jam.submit_command(command) {
            state.status_message = Some(format!("Jam command failed: {err:#}"));
        }
        return;
    }
    record_playback_command_async(runtime, target, command, "device command");
}

fn record_playback_command_async(
    runtime: &Runtime,
    target: String,
    command: crate::devices::PlaybackCommand,
    label: &'static str,
) {
    let devices = Arc::clone(&runtime.devices);
    let sync_devices = Arc::clone(&runtime.devices);
    let federation = Arc::clone(&runtime.federation);
    let tx = runtime.event_tx.clone();
    let sync_tx = runtime.event_tx.clone();
    let running = Arc::clone(&runtime.device_sync_running);
    let requested = Arc::clone(&runtime.device_sync_requested);
    tokio::spawn(async move {
        let target_for_write = target.clone();
        let result = tokio::task::spawn_blocking(move || {
            devices.record_playback_command(&target_for_write, command)
        })
        .await;
        match result {
            Ok(Ok(())) => {
                request_urgent_device_sync_parts(
                    federation,
                    sync_devices,
                    sync_tx,
                    running,
                    requested,
                );
            }
            Ok(Err(err)) => {
                tracing::warn!(%err, target, "recording playback command failed");
                let _ = tx.send(AppEvent::StatusMessage(format!("{label} failed: {err:#}")));
            }
            Err(err) => {
                tracing::warn!(%err, target, "recording playback command task failed");
                let _ = tx.send(AppEvent::StatusMessage(format!("{label} failed: {err}")));
            }
        }
    });
}

fn request_urgent_device_sync(runtime: &Runtime) {
    request_urgent_device_sync_parts(
        Arc::clone(&runtime.federation),
        Arc::clone(&runtime.devices),
        runtime.event_tx.clone(),
        Arc::clone(&runtime.device_sync_running),
        Arc::clone(&runtime.device_sync_requested),
    );
}

fn request_urgent_device_sync_parts(
    federation: Arc<crate::federation::Federation>,
    devices: Arc<crate::devices::DeviceSync>,
    tx: mpsc::UnboundedSender<AppEvent>,
    running: Arc<std::sync::atomic::AtomicBool>,
    requested: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;

    requested.store(true, Ordering::SeqCst);
    if running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        spawn_urgent_device_sync(federation, devices, tx, running, requested);
    }
}

fn spawn_urgent_device_sync(
    federation: Arc<crate::federation::Federation>,
    devices: Arc<crate::devices::DeviceSync>,
    tx: mpsc::UnboundedSender<AppEvent>,
    running: Arc<std::sync::atomic::AtomicBool>,
    requested: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;

    tokio::spawn(async move {
        loop {
            requested.store(false, Ordering::SeqCst);
            match federation.device_sync_now().await {
                Ok(()) => {}
                Err(err) => {
                    tracing::debug!("urgent device sync failed: {err:#}");
                }
            }
            let _ = tx.send(AppEvent::DeviceSyncStatus(devices.status()));
            if !requested.swap(false, Ordering::SeqCst) {
                break;
            }
        }
        running.store(false, Ordering::SeqCst);
        if requested.load(Ordering::SeqCst)
            && running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            spawn_urgent_device_sync(federation, devices, tx, running, requested);
        }
    });
}

const ARTISTS_PREFETCH_MARGIN: usize = 24;

/// How many artist tiles one screen holds right now (grid geometry from the
/// live terminal size), so the initial load always fills the viewport.
fn artist_grid_capacity() -> usize {
    let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
    let columns = usize::from((width.saturating_sub(2) / state::TILE_WIDTH).max(1));
    let rows = usize::from((height.saturating_sub(5) / state::TILE_HEIGHT).max(1));
    columns * rows
}

/// Runs after every event: kicks off whatever background work the current
/// state needs — the first artists page, the next page when the selection
/// nears the end, and artwork for loaded artists.
fn maintenance(state: &mut AppState, runtime: &mut Runtime) {
    {
        let global = &mut state.global;
        // Keep at least a full screen plus a margin loaded, and stay ahead
        // of the cursor: a big terminal fills itself on startup without any
        // scrolling, page after page.
        let needed = artist_grid_capacity().max(global.selected + ARTISTS_PREFETCH_MARGIN)
            + ARTISTS_PREFETCH_MARGIN;
        if global.has_more
            && !global.loading
            && !global.reloading
            && global.error.is_none()
            && global.artists.len() < needed
        {
            global.loading = true;
            let page = global.next_page;
            let filters = global.filters;
            let limit = *global
                .page_limit
                .get_or_insert_with(|| (needed as i64).clamp(48, 200));
            let library = Arc::clone(&runtime.library);
            let tx = runtime.event_tx.clone();
            tokio::task::spawn_blocking(move || {
                let result = library.artists(page, limit, filters).map_err(err_string);
                let _ = tx.send(AppEvent::ArtistsLoaded(result));
            });
        }
    }

    maybe_refresh_network_library(state, runtime);
    maybe_fetch_network_artist_images(state, runtime);
    maybe_enrich_open_artist(state, runtime);

    // Liked ids load once per session — markers are shown everywhere.
    if !state.likes_loaded {
        state.likes_loaded = true;
        let library = Arc::clone(&runtime.library);
        let tx = runtime.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = library.liked_content_ids().map_err(err_string);
            let _ = tx.send(AppEvent::LikesLoaded(result));
            let result = library.fed_like_ids().map_err(err_string);
            let _ = tx.send(AppEvent::FedLikesLoaded(result));
        });
    }
    if !state.local_content_ids_loaded {
        state.local_content_ids_loaded = true;
        refresh_local_content_ids(runtime);
    }
    if state.local_library_stats.is_none() {
        state.local_library_stats = Some(state::Loadable::Loading);
        refresh_local_library_stats(runtime);
    }

    // Playlists tab data (also wanted while the add-to-playlist picker is
    // open from any tab).
    let picker_open = matches!(state.popup, Some(state::Popup::AddToPlaylist { .. }));
    if state.active_tab == state::Tab::Playlists || picker_open {
        if state.playlists.list.is_none() {
            state.playlists.list = Some(state::Loadable::Loading);
            let library = Arc::clone(&runtime.library);
            let tx = runtime.event_tx.clone();
            tokio::task::spawn_blocking(move || {
                let result = library.playlists().map_err(err_string);
                let _ = tx.send(AppEvent::PlaylistsLoaded(result));
            });
        }
        if let Some(opened) = state.playlists.opened {
            let id = opened.id;
            if let std::collections::hash_map::Entry::Vacant(entry) = state.playlist_views.entry(id)
            {
                entry.insert(state::Loadable::Loading);
                let library = Arc::clone(&runtime.library);
                let tx = runtime.event_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let result = library.playlist(id).map_err(err_string);
                    let _ = tx.send(AppEvent::PlaylistViewLoaded { id, result });
                });
            }
        }
    }

    // Drill-down views pushed on the stack fetch their data on first sight.
    for view in state.global.stack.clone() {
        match view {
            state::GlobalView::Artist { id, .. } => {
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    state.artist_views.entry(id)
                {
                    entry.insert(state::Loadable::Loading);
                    let library = Arc::clone(&runtime.library);
                    let tx = runtime.event_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let result = library.artist(id).map_err(err_string);
                        let _ = tx.send(AppEvent::ArtistViewLoaded { id, result });
                    });
                }
            }
            state::GlobalView::Release { id, .. } => {
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    state.release_views.entry(id)
                {
                    entry.insert(state::Loadable::Loading);
                    let library = Arc::clone(&runtime.library);
                    let tx = runtime.event_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let result = library.release(id).map_err(err_string);
                        let _ = tx.send(AppEvent::ReleaseViewLoaded { id, result });
                    });
                }
            }
            state::GlobalView::Search { .. }
            | state::GlobalView::FedArtist { .. }
            | state::GlobalView::FedRelease { .. } => {}
        }
    }

    // Refresh the Federation tab status while it is visible.
    if state.active_tab == state::Tab::Federation {
        let due = runtime
            .fed_status_at
            .is_none_or(|at| at.elapsed() > Duration::from_secs(2));
        if due {
            runtime.fed_status_at = Some(std::time::Instant::now());
            fed_spawn_status(runtime);
        }
    }

    // Artwork wanted by everything currently loaded, at its display size.
    let mut wanted: Vec<(String, u16, u16)> = Vec::new();
    let tile = (state::ART_CELL_WIDTH, state::ART_CELL_HEIGHT);
    let header = (state::ART_HEADER_WIDTH, state::ART_HEADER_HEIGHT);
    for artist in &state.global.artists {
        if let Some(path) = &artist.image_path {
            wanted.push((path.clone(), tile.0, tile.1));
        }
    }
    for detail in state.artist_views.values() {
        if let state::Loadable::Ready(detail) = detail {
            if let Some(path) = &detail.image_path {
                wanted.push((path.clone(), header.0, header.1));
            }
            for release in &detail.releases {
                if let Some(path) = &release.cover_path {
                    wanted.push((path.clone(), tile.0, tile.1));
                }
            }
        }
    }
    for detail in state.release_views.values() {
        if let state::Loadable::Ready(detail) = detail
            && let Some(path) = &detail.cover_path
        {
            wanted.push((path.clone(), header.0, header.1));
        }
    }
    for card in state.artist_fed_views.values() {
        if let state::Loadable::Ready(card) = card {
            if let Some(path) = &card.image_path {
                wanted.push((path.clone(), header.0, header.1));
            }
            for release in &card.releases {
                if let Some(path) = &release.cover_path {
                    wanted.push((path.clone(), tile.0, tile.1));
                    wanted.push((path.clone(), header.0, header.1));
                }
            }
        }
    }
    if let Some((_, state::Loadable::Ready(card))) = &state.fed_artist_view {
        if let Some(path) = &card.image_path {
            wanted.push((path.clone(), header.0, header.1));
        }
        for release in &card.releases {
            if let Some(path) = &release.cover_path {
                wanted.push((path.clone(), tile.0, tile.1));
                wanted.push((path.clone(), header.0, header.1));
            }
        }
    }
    for (path, width, height) in wanted {
        let key = crate::art::cache_key(&path, width, height);
        if state.art.contains_key(&key) {
            continue;
        }
        state.art.insert(key.clone(), state::ArtState::Loading);
        spawn_art_fetch(runtime, key, path, width, height);
    }
}

fn maybe_refresh_network_library(state: &AppState, runtime: &mut Runtime) {
    if state.active_tab != state::Tab::Global
        || !state.global.stack.is_empty()
        || !state.global.filters.source_mode.includes_network()
        || !state.federation.settings.enabled
    {
        return;
    }
    let mode = state.global.filters.source_mode;
    if runtime.library_network_mode != mode {
        runtime.library_network_mode = mode;
        runtime.library_network_refresh_at = None;
        if let Ok(mut cursors) = runtime.library_network_cursors.lock() {
            cursors.clear();
        }
        if let Ok(mut done) = runtime.library_network_done.lock() {
            done.clear();
        }
        if let Ok(mut attempted) = runtime.library_network_art_attempted.lock() {
            attempted.clear();
        }
    }
    let near_end = state
        .global
        .artists
        .len()
        .saturating_sub(state.global.selected)
        <= ARTISTS_PREFETCH_MARGIN
        || state.global.artists.len() < artist_grid_capacity();
    let refresh_interval = if near_end {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(60)
    };
    let due = runtime
        .library_network_refresh_at
        .is_none_or(|at| at.elapsed() > refresh_interval);
    if !due {
        return;
    }
    if !near_end {
        if let Ok(mut cursors) = runtime.library_network_cursors.lock() {
            cursors.clear();
        }
        if let Ok(mut done) = runtime.library_network_done.lock() {
            done.clear();
        }
    }
    use std::sync::atomic::Ordering;
    if runtime
        .library_network_refreshing
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    runtime.library_network_refresh_at = Some(std::time::Instant::now());
    let federation = Arc::clone(&runtime.federation);
    let tx = runtime.event_tx.clone();
    let running = Arc::clone(&runtime.library_network_refreshing);
    let cursors = Arc::clone(&runtime.library_network_cursors);
    let done = Arc::clone(&runtime.library_network_done);
    let limit = if near_end { 64 } else { 96 };
    tokio::spawn(async move {
        let result = async {
            let sources = federation.network_library_sources(mode).await?;
            for source in sources {
                let source_id = source.endpoint_id.clone();
                if done
                    .lock()
                    .map(|done| done.contains(&source_id))
                    .unwrap_or(false)
                {
                    continue;
                }
                let cursor = cursors
                    .lock()
                    .ok()
                    .and_then(|cursors| cursors.get(&source_id).cloned())
                    .flatten();
                match tokio::time::timeout(
                    Duration::from_secs(4),
                    federation.cache_artist_slice_from_source(source, cursor.clone(), limit),
                )
                .await
                {
                    Ok(Ok((count, next_cursor))) => {
                        if let Some(next_cursor) = next_cursor {
                            if let Ok(mut cursors) = cursors.lock() {
                                cursors.insert(source_id.clone(), Some(next_cursor));
                            }
                        } else if let Ok(mut done) = done.lock() {
                            done.insert(source_id.clone());
                        }
                        let _ = tx.send(AppEvent::NetworkArtistCacheUpdated { source_id, count });
                    }
                    Ok(Err(err)) => {
                        tracing::debug!(source = %source_id, "network library source failed: {err:#}");
                    }
                    Err(_) => {
                        tracing::debug!(source = %source_id, "network library source timed out");
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(err) = result {
            tracing::debug!("network library refresh skipped: {err:#}");
        }
        running.store(false, Ordering::SeqCst);
    });
}

fn maybe_fetch_network_artist_images(state: &AppState, runtime: &mut Runtime) {
    if state.active_tab != state::Tab::Global
        || !state.global.stack.is_empty()
        || !state.global.filters.source_mode.includes_network()
        || !state.federation.settings.enabled
    {
        return;
    }
    let capacity = artist_grid_capacity().max(24);
    let start = state.global.selected.saturating_sub(capacity / 2);
    let names = state
        .global
        .artists
        .iter()
        .skip(start)
        .take(capacity * 2)
        .filter(|artist| artist.image_path.is_none() && artist.availability.is_remoteish())
        .map(|artist| artist.name.clone())
        .collect::<Vec<_>>();
    if names.is_empty() {
        return;
    }
    use std::sync::atomic::Ordering;
    if runtime
        .library_network_art_fetching
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    let filters = state.global.filters;
    let library = Arc::clone(&runtime.library);
    let federation = Arc::clone(&runtime.federation);
    let tx = runtime.event_tx.clone();
    let attempted = Arc::clone(&runtime.library_network_art_attempted);
    let running = Arc::clone(&runtime.library_network_art_fetching);
    tokio::spawn(async move {
        let query_library = Arc::clone(&library);
        let requests = tokio::task::spawn_blocking(move || {
            query_library.network_artist_image_requests(filters, &names, 8)
        })
        .await
        .map_err(|err| anyhow::anyhow!("network art query failed: {err:#}"))
        .and_then(|result| result);
        let requests = match requests {
            Ok(requests) => requests,
            Err(err) => {
                tracing::debug!("network artist image requests failed: {err:#}");
                running.store(false, Ordering::SeqCst);
                return;
            }
        };

        for request in requests {
            let attempt_key = format!("{}:{}", request.source_id, request.artist_key);
            let should_try = attempted
                .lock()
                .map(|mut attempted| attempted.insert(attempt_key))
                .unwrap_or(false);
            if !should_try {
                continue;
            }
            let Some(path) = federation
                .card_image(
                    std::slice::from_ref(&request.source_id),
                    &request.name,
                    None,
                )
                .await
            else {
                continue;
            };
            let update_library = Arc::clone(&library);
            let source_id = request.source_id.clone();
            let artist_key = request.artist_key.clone();
            let saved = tokio::task::spawn_blocking(move || {
                update_library.set_network_artist_image(&source_id, &artist_key, &path)
            })
            .await
            .map_err(|err| anyhow::anyhow!("network art save failed: {err:#}"))
            .and_then(|result| result);
            match saved {
                Ok(true) => {
                    let _ = tx.send(AppEvent::NetworkArtistCacheUpdated {
                        source_id: request.source_id,
                        count: 1,
                    });
                }
                Ok(false) => {}
                Err(err) => tracing::debug!("network artist image save failed: {err:#}"),
            }
        }
        running.store(false, Ordering::SeqCst);
    });
}

/// Load and decode a local image file for the art cache.
fn spawn_art_fetch(runtime: &Runtime, key: String, path: String, width: u16, height: u16) {
    let tx = runtime.event_tx.clone();
    let semaphore = Arc::clone(&runtime.art_semaphore);
    tokio::spawn(async move {
        let Ok(_permit) = semaphore.acquire_owned().await else {
            return;
        };
        let art = tokio::task::spawn_blocking(move || -> anyhow::Result<crate::art::ArtImage> {
            let bytes = std::fs::read(&path)?;
            crate::art::decode_to_cells(&bytes, width, height)
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result)
        .map_err(|err| tracing::warn!(%err, "artwork load failed"))
        .ok()
        .map(Arc::new);
        let _ = tx.send(AppEvent::ArtLoaded { key, art });
    });
}

/// Execute a side effect requested by update().
fn perform_effect(state: &mut AppState, runtime: &mut Runtime, effect: Effect) {
    if state.device_playback.is_control() && is_controlled_playback_effect(&effect) {
        perform_control_playback_effect(state, runtime, effect);
        return;
    }
    match effect {
        Effect::CheckUpdate => {
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(crate::updater::check)
                    .await
                    .map_err(|err| format!("update worker failed: {err}"))
                    .and_then(|result| result.map_err(|err| format!("{err:#}")));
                let _ = tx.send(AppEvent::UpdateChecked(result));
            });
        }
        Effect::InstallUpdate(update) => {
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let progress_tx = tx.clone();
                let result = tokio::task::spawn_blocking(move || {
                    crate::updater::install(&update, |message| {
                        let _ = progress_tx.send(AppEvent::UpdateProgress(message));
                    })
                })
                .await
                .map_err(|err| format!("update worker failed: {err}"))
                .and_then(|result| result.map_err(|err| format!("{err:#}")));
                let _ = tx.send(AppEvent::UpdateInstalled(result));
            });
        }
        Effect::PlayCurrent => {
            play_current(state, runtime);
            push_media_metadata(state, runtime);
            push_media_update(state, runtime, true);
        }
        Effect::TogglePause => {
            if state.player.paused {
                runtime.player.pause();
            } else {
                runtime.player.resume();
            }
            push_media_update(state, runtime, true);
        }
        Effect::StopPlayback => {
            runtime.player_start_pending = false;
            runtime.player.stop();
            push_media_update(state, runtime, true);
        }
        Effect::SeekBy(delta) => {
            let target = (state.player.position_secs + delta as f64).max(0.0);
            state.player.position_secs = target;
            runtime
                .player
                .seek(std::time::Duration::from_secs_f64(target));
        }
        Effect::SetVolume(volume) => {
            runtime.player.set_volume(player::amplitude(volume));
            save_app_settings(state);
        }
        Effect::SetOptions => {}
        Effect::PlaybackQueueChanged => {}
        Effect::QueueOrderChanged { restart_current } => {
            if restart_current && state.player.playing && state.player.current.is_some() {
                let paused = state.player.paused;
                start_current_audio(state, runtime, state.player.position_secs, paused);
                push_media_metadata(state, runtime);
                push_media_update(state, runtime, true);
            }
        }
        Effect::SourceModeChanged => {
            runtime.library_network_refresh_at = None;
            if let Ok(mut cursors) = runtime.library_network_cursors.lock() {
                cursors.clear();
            }
            if let Ok(mut done) = runtime.library_network_done.lock() {
                done.clear();
            }
            if let Ok(mut attempted) = runtime.library_network_art_attempted.lock() {
                attempted.clear();
            }
            save_app_settings(state);
            reset_artist_pagination(state);
            refresh_artists(state, runtime);
            if let Some(effect) = update::apply_library_filter_change(state) {
                perform_effect(state, runtime, effect);
            }
        }
        Effect::ChangeMusicDirectory {
            path,
            move_existing,
        } => {
            if state.music_dir_changing {
                state.status_message = Some("music directory change is already running".into());
                return;
            }
            state.music_dir_changing = true;
            state.status_message = Some(if move_existing {
                "moving saved music to the new directory…".into()
            } else {
                "changing music save directory…".into()
            });
            let federation = Arc::clone(&runtime.federation);
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let result = federation
                    .change_media_dir(path, move_existing)
                    .await
                    .map_err(err_string);
                let _ = tx.send(AppEvent::MusicDirectoryChanged(result));
            });
        }
        Effect::EnqueueRelease { id, next } => {
            let library = Arc::clone(&runtime.library);
            let tx = runtime.event_tx.clone();
            tokio::task::spawn_blocking(move || match library.release(id) {
                Ok(detail) => {
                    let _ = tx.send(AppEvent::EnqueueTracks {
                        tracks: detail.tracks,
                        next,
                    });
                }
                Err(err) => {
                    tracing::warn!(%err, release = id, "queueing a release failed");
                    let _ = tx.send(AppEvent::StatusMessage(format!("queue failed: {err:#}")));
                }
            });
        }
        Effect::LoadListenHistory => {
            let library = Arc::clone(&runtime.library);
            let devices = Arc::clone(&runtime.devices);
            let tx = runtime.event_tx.clone();
            tokio::task::spawn_blocking(move || {
                let result = library
                    .listen_history(500)
                    .map_err(|err| format!("{err:#}"));
                let _ = tx.send(AppEvent::ListenHistoryLoaded(result));
                let _ = tx.send(AppEvent::DeviceSyncStatus(devices.status()));
            });
        }
        Effect::ToggleLikes {
            track_ids,
            fed_tracks,
        } => {
            let track_ids: Vec<i64> = track_ids.into_iter().filter(|id| *id >= 0).collect();
            if track_ids.is_empty() && fed_tracks.is_empty() {
                return;
            }
            let library = Arc::clone(&runtime.library);
            let devices = Arc::clone(&runtime.devices);
            let tx = runtime.event_tx.clone();
            tokio::task::spawn_blocking(move || {
                for track_id in track_ids {
                    let content_id = match library.track_content_id_by_id(track_id) {
                        Ok(Some(content_id)) => content_id,
                        Ok(None) => {
                            tracing::warn!(track_id, "cannot toggle like without content id");
                            let _ = tx.send(AppEvent::StatusMessage(
                                "like failed: track has no content id".to_string(),
                            ));
                            continue;
                        }
                        Err(err) => {
                            tracing::warn!(%err, track_id, "loading track content id failed");
                            let _ =
                                tx.send(AppEvent::StatusMessage(format!("like failed: {err:#}")));
                            break;
                        }
                    };
                    match library.toggle_like_by_content_id(&content_id) {
                        Ok(liked) => {
                            if let Err(err) = devices.record_content_like(&content_id, liked) {
                                tracing::warn!(%err, track_id, "recording synced like failed");
                            }
                            let _ = tx.send(AppEvent::LikeToggled { content_id, liked });
                        }
                        Err(err) => {
                            tracing::warn!(%err, track_id, "like toggle failed");
                            let _ =
                                tx.send(AppEvent::StatusMessage(format!("like failed: {err:#}")));
                            break;
                        }
                    }
                }
                for fed in fed_tracks {
                    match library.toggle_fed_like(&fed) {
                        Ok(liked) => {
                            if let Err(err) = devices.record_fed_like(&fed, liked) {
                                tracing::warn!(%err, title = %fed.title, "recording synced federated like failed");
                            }
                            let _ = tx.send(AppEvent::FedLikeToggled {
                                item_id: fed.item_id.clone(),
                                content_id: fed.content_id.clone(),
                                liked,
                            });
                        }
                        Err(err) => {
                            tracing::warn!(%err, title = %fed.title, "federated like toggle failed");
                            let _ =
                                tx.send(AppEvent::StatusMessage(format!("like failed: {err:#}")));
                            break;
                        }
                    }
                }
                let _ = tx.send(AppEvent::LikesLoaded(
                    library.liked_content_ids().map_err(err_string),
                ));
                let _ = tx.send(AppEvent::FedLikesLoaded(
                    library.fed_like_ids().map_err(err_string),
                ));
            });
        }
        Effect::RemoveFromPlaylist {
            playlist_id,
            track_ids,
            content_ids,
        } => {
            let library = Arc::clone(&runtime.library);
            let devices = Arc::clone(&runtime.devices);
            let tx = runtime.event_tx.clone();
            tokio::task::spawn_blocking(move || {
                let event = match library
                    .remove_tracks_from_playlist(playlist_id, &track_ids)
                    .and_then(|()| {
                        library.remove_content_ids_from_playlist(playlist_id, &content_ids)
                    }) {
                    Ok(()) => {
                        if let Err(err) =
                            devices.record_playlist_content_removed(playlist_id, &content_ids)
                        {
                            tracing::warn!(%err, playlist_id, "recording synced playlist removal failed");
                        }
                        AppEvent::LibraryChanged {
                            message: Some(format!("removed {} track(s)", track_ids.len())),
                        }
                    }
                    Err(err) => AppEvent::StatusMessage(format!("remove failed: {err:#}")),
                };
                let _ = tx.send(event);
            });
        }
        Effect::SimilarityApplySettings => {
            save_app_settings(state);
            runtime.similarity.apply(state.similarity.settings.clone());
            state.similarity.status = runtime.similarity.status();
        }
        Effect::SimilarityClear => {
            state.status_message = Some("clearing stored embeddings…".to_string());
            runtime.similarity.clear();
        }
        Effect::FedApplySettings => fed_apply_settings(state, runtime),
        Effect::FedSyncNow => {
            state.federation.publishing = true;
            state.status_message = Some("publishing library…".to_string());
            let fed = Arc::clone(&runtime.federation);
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let message = match fed.sync_now().await {
                    Ok(()) => "federation: library published".to_string(),
                    Err(err) => format!("federation sync failed: {err:#}"),
                };
                let _ = tx.send(AppEvent::FederationStatus(fed.status().await));
                let _ = tx.send(AppEvent::FedSyncFinished(message));
            });
        }
        Effect::FedShowTicket => {
            let fed = Arc::clone(&runtime.federation);
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let result = fed.ticket().await.map_err(|err| format!("{err:#}"));
                let _ = tx.send(AppEvent::FedTicket(result));
            });
        }
        Effect::DeviceShowInvite
        | Effect::DeviceConnectInvite(_)
        | Effect::DeviceSyncNow
        | Effect::DeviceSetName(_)
        | Effect::DeviceRevoke(_)
        | Effect::DeviceLeaveGroup
        | Effect::JamCreate
        | Effect::JamJoin(_)
            if !state.connected_devices_enabled() =>
        {
            state.status_message = Some("enable federation before using connected devices".into());
        }
        Effect::JamCreate => {
            let federation = Arc::clone(&runtime.federation);
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let result = federation
                    .create_jam()
                    .await
                    .map_err(|err| format!("{err:#}"));
                let _ = tx.send(AppEvent::JamInvite(result));
            });
        }
        Effect::JamJoin(invite) => {
            let result = runtime
                .federation
                .join_jam(&invite)
                .map(|()| "joined Jam; waiting for host state".to_string())
                .map_err(|err| format!("{err:#}"));
            let _ = runtime.event_tx.send(AppEvent::JamJoined(result));
            let _ = runtime
                .event_tx
                .send(AppEvent::JamStatus(runtime.jam.status()));
        }
        Effect::JamLeave => {
            runtime.jam.leave();
            state.jam = runtime.jam.status();
            state.device_playback.role = state::DevicePlaybackRole::Active;
            state.device_playback.jam_host = false;
            state.status_message = Some("left Jam".into());
        }
        Effect::DeviceShowInvite => {
            let fed = Arc::clone(&runtime.federation);
            let devices = Arc::clone(&runtime.devices);
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let result = fed.device_invite().await.map_err(|err| format!("{err:#}"));
                let _ = tx.send(AppEvent::DeviceInvite(result));
                let _ = tx.send(AppEvent::FederationStatus(fed.status().await));
                let _ = tx.send(AppEvent::DeviceSyncStatus(devices.status()));
            });
        }
        Effect::DeviceConnectInvite(invite) => device_connect(runtime, invite),
        Effect::DeviceSyncNow => {
            state.federation.device_syncing = true;
            state.status_message = Some("syncing devices…".to_string());
            let fed = Arc::clone(&runtime.federation);
            let devices = Arc::clone(&runtime.devices);
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let message = match fed.device_sync_now().await {
                    Ok(()) => "devices: sync complete".to_string(),
                    Err(err) => format!("devices: {err:#}"),
                };
                let _ = tx.send(AppEvent::DeviceSyncStatus(devices.status()));
                let _ = tx.send(AppEvent::DeviceSyncFinished(message));
            });
        }
        Effect::DeviceSetName(name) => {
            let fed = Arc::clone(&runtime.federation);
            let devices = Arc::clone(&runtime.devices);
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let ticket = fed.ticket().await.ok();
                let message = match devices.set_device_name(&name, ticket.as_deref()) {
                    Ok(()) => "device name saved".to_string(),
                    Err(err) => format!("device name: {err:#}"),
                };
                let _ = tx.send(AppEvent::DeviceSyncStatus(devices.status()));
                let _ = tx.send(AppEvent::StatusMessage(message));
            });
        }
        Effect::DeviceRevoke(device_id) => {
            let devices = Arc::clone(&runtime.devices);
            let tx = runtime.event_tx.clone();
            tokio::task::spawn_blocking(move || {
                let message = match devices.revoke_device(&device_id) {
                    Ok(()) => format!("device {} revoked", &device_id[..device_id.len().min(10)]),
                    Err(err) => format!("revoke failed: {err:#}"),
                };
                let _ = tx.send(AppEvent::DeviceSyncStatus(devices.status()));
                let _ = tx.send(AppEvent::StatusMessage(message));
            });
        }
        Effect::DeviceLeaveGroup => {
            state.status_message = Some("leaving device group…".to_string());
            let fed = Arc::clone(&runtime.federation);
            let devices = Arc::clone(&runtime.devices);
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let message = match devices.record_leave_group_revoke() {
                    Ok(op_id) => match fed.device_sync_now().await {
                        Ok(()) => match devices.finish_leave_group_reset() {
                            Ok(group_id) => format!("left device group · new group {group_id}"),
                            Err(err) => format!("leave failed after sync: {err:#}"),
                        },
                        Err(err) => {
                            if let Err(rollback) = devices.cancel_leave_group_revoke(&op_id) {
                                tracing::warn!(
                                    "rolling back failed leave-device-group op failed: {rollback:#}"
                                );
                            }
                            format!("leave failed: revoke was not synced: {err:#}")
                        }
                    },
                    Err(err) => format!("leave failed: {err:#}"),
                };
                let _ = tx.send(AppEvent::DeviceSyncStatus(devices.status()));
                let _ = tx.send(AppEvent::StatusMessage(message));
            });
        }
        Effect::FedOpenArtist(name) => {
            let fed = Arc::clone(&runtime.federation);
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let result = fed
                    .artist_card(&name)
                    .await
                    .map_err(|err| format!("{err:#}"));
                let card = result.as_ref().ok().cloned();
                let _ = tx.send(AppEvent::FedArtistLoaded {
                    name: name.clone(),
                    result,
                });
                // Stream the artwork in after the card is on screen: the
                // artist image first, then every release cover.
                let Some(card) = card else { return };
                if let Some(path) = fed.card_image(&card.owners, &name, None).await {
                    let _ = tx.send(AppEvent::FedCardArt {
                        name: name.clone(),
                        release: None,
                        path,
                    });
                }
                for release in &card.releases {
                    let Some(path) = fed
                        .card_image(&release.owners, &name, Some(&release.title))
                        .await
                    else {
                        continue;
                    };
                    let _ = tx.send(AppEvent::FedCardArt {
                        name: name.clone(),
                        release: Some(release.title.clone()),
                        path,
                    });
                }
            });
        }
        Effect::FedDownload { tracks } => fed_download_spawn(runtime, tracks, None),
        Effect::FedFetchTrackInfo { tracks } => {
            for (placeholder_id, fed_track) in tracks {
                let federation = Arc::clone(&runtime.federation);
                let tx = runtime.event_tx.clone();
                tokio::spawn(async move {
                    let mut preview = crate::federation::pending_track(&fed_track);
                    preview.id = placeholder_id;
                    let item_id = fed_track.item_id.clone();
                    let result = federation
                        .track_info(preview)
                        .await
                        .map_err(|err| format!("{err:#}"));
                    let _ = tx.send(AppEvent::FedTrackInfoLoaded {
                        placeholder_id,
                        item_id,
                        result,
                    });
                });
            }
        }
        Effect::OpenVisualizerEditor { path } => match open_visualizer_editor(&path) {
            Ok(()) => {
                runtime.force_redraw = true;
                match state.visualizer.load_library() {
                    Ok(()) => {
                        clamp_settings_cursor(state);
                        state.status_message =
                            Some(format!("visualization script saved: {}", path.display()));
                    }
                    Err(err) => {
                        state.status_message = Some(format!("visualizations: {err:#}"));
                    }
                }
            }
            Err(err) => {
                runtime.force_redraw = true;
                state.status_message = Some(format!("editor failed: {err:#}"));
                let _ = state.visualizer.load_library();
                clamp_settings_cursor(state);
            }
        },
        Effect::RemoveQueueIndices {
            restart_paused,
            stop,
            ..
        } => {
            if stop {
                runtime.player_start_pending = false;
                runtime.player.stop();
                push_media_update(state, runtime, true);
            } else if let Some(paused) = restart_paused {
                start_current_audio(state, runtime, 0.0, paused);
                push_media_metadata(state, runtime);
                push_media_update(state, runtime, true);
            } else {
                push_media_update(state, runtime, true);
            }
        }
    }
}

fn is_controlled_playback_effect(effect: &Effect) -> bool {
    matches!(
        effect,
        Effect::PlayCurrent
            | Effect::TogglePause
            | Effect::StopPlayback
            | Effect::SeekBy(_)
            | Effect::SetVolume(_)
            | Effect::SetOptions
            | Effect::RemoveQueueIndices { .. }
            | Effect::PlaybackQueueChanged
            | Effect::QueueOrderChanged { .. }
    )
}

fn perform_control_playback_effect(state: &mut AppState, runtime: &mut Runtime, effect: Effect) {
    let mut seek = false;
    let local_only_volume = state.device_playback.role == state::DevicePlaybackRole::Jam
        && matches!(effect, Effect::SetVolume(_));
    match effect {
        Effect::PlayCurrent => {
            state.player.current = state.player.queue.get(state.player.queue_pos).cloned();
            state.player.playing = state.player.current.is_some();
            state.player.paused = false;
            state.player.position_secs = 0.0;
            seek = true;
        }
        Effect::TogglePause => {}
        Effect::StopPlayback => {
            state.player.playing = false;
            state.player.current = None;
            state.player.paused = false;
            state.player.position_secs = 0.0;
        }
        Effect::SeekBy(delta) => {
            state.player.position_secs = (state.player.position_secs + delta as f64).max(0.0);
            seek = true;
        }
        Effect::SetVolume(volume) => {
            state.player.volume = volume.min(100);
            save_app_settings(state);
        }
        Effect::SetOptions
        | Effect::RemoveQueueIndices { .. }
        | Effect::PlaybackQueueChanged
        | Effect::QueueOrderChanged { .. }
        | Effect::LoadListenHistory
        | Effect::ChangeMusicDirectory { .. } => {}
        _ => {}
    }
    if local_only_volume {
        return;
    }
    record_control_playback_state(state, runtime, seek);
}

fn clamp_settings_cursor(state: &mut AppState) {
    state.additional_settings_cursor = state.additional_settings_cursor.min(
        state::additional_settings_rows(state)
            .len()
            .saturating_sub(1),
    );
    let last = state::settings_rows(state).len().saturating_sub(1);
    state.settings_cursor = state.settings_cursor.min(last);
}

fn open_visualizer_editor(path: &Path) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(
        io::stdout(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableBracketedPaste
    );

    let status = if cfg!(windows) {
        Command::new("cmd")
            .args(["/C", &format!("{editor} {}", path.display())])
            .status()
    } else {
        Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} {}", shell_quote(path)))
            .status()
    };

    let _ = crossterm::execute!(
        io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste
    );
    let _ = crossterm::terminal::enable_raw_mode();

    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => anyhow::bail!("editor exited with {status}"),
        Err(err) => Err(err.into()),
    }
}

fn shell_quote(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Start playing `queue[queue_pos]`: open the local file in a background
/// task and hand the reader to the audio thread.
fn play_current(state: &mut AppState, runtime: &mut Runtime) {
    start_current_audio(state, runtime, 0.0, false);
}

fn start_current_audio(
    state: &mut AppState,
    runtime: &mut Runtime,
    position_secs: f64,
    paused: bool,
) {
    let Some(mut track) = state.player.queue.get(state.player.queue_pos).cloned() else {
        return;
    };
    if track_file_missing(&track) {
        if let Some(local) = local_track_for_playback(runtime, &track) {
            state.player.queue[state.player.queue_pos] = local.clone();
            track = local;
        } else if let Some(fed) = direct_fed_source(&track) {
            let mut pending = crate::federation::pending_track(&fed);
            pending.id = track.id;
            pending.play_count = track.play_count;
            state.player.queue[state.player.queue_pos] = pending.clone();
            track = pending;
        } else if track_content_id(&track).is_some() {
            state.player.current = Some(track.clone());
            state.player.playing = true;
            state.player.paused = paused;
            state.player.position_secs = position_secs.max(0.0);
            state.player.audio_analysis = player::AudioAnalysisSnapshot::default();
            state.player.track_started_at = Some(now_epoch_seconds());
            state.player.listen_id = Some(runtime.devices.new_listen_id());
            state.player.prefetched_pos = None;
            runtime.player_start_pending = true;
            runtime.player.stop();
            state.status_message = Some(format!(
                "federation: locating \"{}\" for this device…",
                track.title
            ));
            spawn_content_id_resolve(
                runtime,
                &track,
                Some(StreamingPlaybackRequest {
                    volume: state.player.volume,
                    paused,
                    position_secs,
                }),
            );
            return;
        } else if let Some(fed) = track.fed.clone() {
            let mut pending = crate::federation::pending_track(&fed);
            pending.id = track.id;
            pending.play_count = track.play_count;
            state.player.queue[state.player.queue_pos] = pending.clone();
            track = pending;
        }
    }
    // The track that was playing until now was cut short by this switch.
    let previous_started_at = state.player.track_started_at;
    let previous_listen_id = state.player.listen_id.take();
    let next_key = track_playback_key(&track);
    let mut same_track = false;
    let same_track_started_at = if let Some(previous) = state.player.current.take() {
        same_track = track_playback_key(&previous) == next_key;
        if state.player.playing && !same_track {
            report_history(
                runtime,
                &previous,
                previous_listen_id.as_deref(),
                state.player.track_started_at,
                (state.player.position_secs * 1_000.0).round() as i64,
                music_dht::device_sync::ListenEndReason::Replaced,
            );
        }
        same_track.then_some(previous_started_at).flatten()
    } else {
        None
    };
    state.player.current = Some(track.clone());
    state.player.playing = true;
    state.player.paused = paused;
    state.player.position_secs = position_secs.max(0.0);
    state.player.audio_analysis = player::AudioAnalysisSnapshot::default();
    state.player.track_started_at = same_track_started_at.or_else(|| Some(now_epoch_seconds()));
    state.player.listen_id = if same_track {
        previous_listen_id
    } else {
        Some(runtime.devices.new_listen_id())
    };
    state.player.prefetched_pos = None;
    state.status_message = Some(format!("▶ {} — {}", track.title, track.artist_line()));

    runtime.player_start_pending = true;
    if track.is_fed_pending() {
        // A federated track that is not on disk yet: silence the previous
        // audio, download it and resume through FedTrackResolved.
        runtime.player.stop();
        state.status_message = Some(format!("federation: fetching \"{}\"…", track.title));
        spawn_fed_resolve(
            runtime,
            &track,
            Some(StreamingPlaybackRequest {
                volume: state.player.volume,
                paused,
                position_secs,
            }),
        );
        return;
    }
    if track_file_missing(&track) {
        runtime.player_start_pending = false;
        runtime.player.stop();
        state.player.playing = false;
        state.player.paused = false;
        state.status_message = Some(format!(
            "playback failed: \"{}\" is not available on this device",
            track.title
        ));
        return;
    }
    let controller = runtime.player.clone();
    let volume = player::amplitude(state.player.volume);
    let tx = runtime.event_tx.clone();
    tokio::task::spawn_blocking(move || match open_track_file(&track.file_path) {
        Ok((reader, byte_len)) => {
            controller.play(reader, byte_len, volume);
            if position_secs > 0.0 {
                controller.seek(std::time::Duration::from_secs_f64(position_secs));
            }
            if paused {
                controller.pause();
            }
        }
        Err(err) => {
            tracing::warn!(
                track_id = track.id,
                title = %track.title,
                file = %track.file_path,
                %err,
                "cannot open track file"
            );
            let _ = tx.send(AppEvent::Player(player::PlayerEvent::Failed(format!(
                "playback failed: {err}"
            ))));
        }
    });
}

fn track_file_missing(track: &crate::library::models::TrackItem) -> bool {
    track.file_path.is_empty() || !Path::new(&track.file_path).is_file()
}

fn local_track_for_playback(
    runtime: &Runtime,
    track: &crate::library::models::TrackItem,
) -> Option<crate::library::models::TrackItem> {
    let content_id = track_content_id(track)?;
    let local = runtime
        .library
        .track_by_content_id(&content_id)
        .ok()
        .flatten()?;
    Path::new(&local.file_path).is_file().then_some(local)
}

fn direct_fed_source(
    track: &crate::library::models::TrackItem,
) -> Option<crate::federation::FedTrack> {
    let fed = track.fed.clone()?;
    let owner_ok = fed.owner.parse::<music_dht::EndpointId>().is_ok();
    let item_ok = fed.item_id.len() == 64 && fed.item_id.chars().all(|c| c.is_ascii_hexdigit());
    (owner_ok && item_ok).then_some(fed)
}

fn track_content_id(track: &crate::library::models::TrackItem) -> Option<String> {
    state::track_content_id(track)
}

fn open_track_file(path: &str) -> std::io::Result<(player::TrackReader, Option<u64>)> {
    let file = std::fs::File::open(path)?;
    let byte_len = file.metadata().ok().map(|meta| meta.len());
    Ok((Box::new(std::io::BufReader::new(file)), byte_len))
}

/// Open the next queue item ~30s before the current track ends and append
/// it in the audio thread, so rodio switches sources without a device gap.
fn maybe_prefetch_next(state: &mut AppState, runtime: &Runtime) {
    const PREFETCH_MARGIN_SECS: f64 = 30.0;
    let player = &state.player;
    if !player.playing || player.paused || player.prefetched_pos.is_some() {
        return;
    }
    let Some(track) = &player.current else {
        return;
    };
    if track.duration_seconds <= 0.0
        || track.duration_seconds - player.position_secs > PREFETCH_MARGIN_SECS
    {
        return;
    }
    let Some(next_pos) = update::peek_next_pos(player) else {
        return;
    };
    let Some(mut next) = player.queue.get(next_pos).cloned() else {
        return;
    };
    if track_file_missing(&next) {
        if let Some(local) = local_track_for_playback(runtime, &next) {
            state.player.queue[next_pos] = local.clone();
            next = local;
        } else if let Some(fed) = direct_fed_source(&next) {
            next = crate::federation::pending_track(&fed);
            spawn_fed_resolve(runtime, &next, None);
            return;
        } else if track_content_id(&next).is_some() {
            // Resolve by content id first: the actual owner/item id may be a
            // synced placeholder from another client.
            spawn_content_id_resolve(runtime, &next, None);
            return;
        } else if next.is_fed_pending() {
            spawn_fed_resolve(runtime, &next, None);
            return;
        } else {
            return;
        }
    }
    state.player.prefetched_pos = Some(next_pos);
    tracing::debug!(title = %next.title, "prefetching next track");
    let controller = runtime.player.clone();
    let tx = runtime.event_tx.clone();
    tokio::task::spawn_blocking(move || match open_track_file(&next.file_path) {
        Ok((reader, byte_len)) => controller.enqueue(reader, byte_len),
        Err(err) => {
            tracing::warn!(%err, "prefetch failed; falling back to a normal switch");
            let _ = tx.send(AppEvent::PrefetchFailed { pos: next_pos });
        }
    });
}

/// Record a finished/aborted listen in the local history; listens shorter
/// than 5s are noise.
fn report_history(
    runtime: &Runtime,
    track: &crate::library::models::TrackItem,
    listen_id: Option<&str>,
    started_at: Option<i64>,
    listened_ms: i64,
    ended_reason: music_dht::device_sync::ListenEndReason,
) {
    let Some(listen_id) = listen_id else {
        return;
    };
    let Some(event) = runtime.devices.listen_event_for_track(
        listen_id.to_string(),
        track,
        started_at.unwrap_or_else(now_epoch_seconds) * 1_000,
        listened_ms,
        ended_reason,
    ) else {
        tracing::warn!(title = %track.title, "history skipped: track has no content id");
        return;
    };
    let devices = Arc::clone(&runtime.devices);
    tokio::task::spawn_blocking(move || {
        if let Err(err) = devices.record_listen(event) {
            tracing::warn!(%err, "history write failed");
        }
    });
}

/// Persist the Federation-tab settings and (re)start or stop the node.
pub(crate) fn fed_apply_settings(state: &mut AppState, runtime: &Runtime) {
    let settings = state.federation.settings.clone();
    let fed = Arc::clone(&runtime.federation);
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        if let Err(err) = fed.apply_settings(settings).await {
            let _ = tx.send(AppEvent::StatusMessage(format!("federation: {err:#}")));
        }
        let _ = tx.send(AppEvent::FederationStatus(fed.status().await));
    });
}

/// Connect to a peer by its pasted ticket (manual peering).
pub(crate) fn fed_connect(runtime: &Runtime, ticket: String) {
    let fed = Arc::clone(&runtime.federation);
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let message = match fed.connect(&ticket).await {
            Ok(peer) => format!("federation: connected to {}…", &peer[..peer.len().min(10)]),
            Err(err) => format!("federation: {err:#}"),
        };
        let _ = tx.send(AppEvent::FederationStatus(fed.status().await));
        let _ = tx.send(AppEvent::StatusMessage(message));
    });
}

/// Pair with another trusted client by an opaque `frid://i/...` invite.
pub(crate) fn device_connect(runtime: &Runtime, invite: String) {
    let fed = Arc::clone(&runtime.federation);
    let devices = Arc::clone(&runtime.devices);
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let _ = tx.send(AppEvent::StatusMessage(
            "waiting for device confirmation...".to_string(),
        ));
        let result = fed
            .device_connect(&invite)
            .await
            .map_err(|err| format!("{err:#}"));
        let _ = tx.send(AppEvent::FederationStatus(fed.status().await));
        let _ = tx.send(AppEvent::DeviceSyncStatus(devices.status()));
        let _ = tx.send(AppEvent::DeviceConnectResult(result));
    });
}

fn download_progress_sender(
    tx: mpsc::UnboundedSender<AppEvent>,
    title: String,
) -> impl FnMut(crate::federation::DownloadProgress) + Send + 'static {
    let mut last_sent: Option<std::time::Instant> = None;
    let started = std::time::Instant::now();
    move |progress| {
        let now = std::time::Instant::now();
        let complete = progress.total > 0 && progress.received >= progress.total;
        let first = progress.received == 0;
        let due = last_sent.is_none_or(|last| now.duration_since(last) >= TICK_INTERVAL);
        if first || complete || due {
            last_sent = Some(now);
            let elapsed_secs = now.duration_since(started).as_secs_f64();
            let bytes_per_sec = if elapsed_secs >= 0.25 && progress.received > 0 {
                Some(progress.received as f64 / elapsed_secs)
            } else {
                None
            };
            let _ = tx.send(AppEvent::StatusMessage(format_download_progress(
                &title,
                progress,
                bytes_per_sec,
            )));
        }
    }
}

fn format_download_progress(
    title: &str,
    progress: crate::federation::DownloadProgress,
    bytes_per_sec: Option<f64>,
) -> String {
    let speed = bytes_per_sec
        .map(|bytes| format!(" · {}", format_transfer_rate(bytes)))
        .unwrap_or_default();
    if progress.total > 0 {
        let percent = (progress.received as f64 / progress.total as f64 * 100.0).clamp(0.0, 100.0);
        format!(
            "federation: downloading \"{title}\" {:.0}% · {}/{}{speed}",
            percent,
            format_bytes(progress.received),
            format_bytes(progress.total)
        )
    } else {
        format!(
            "federation: downloading \"{title}\" · {}{speed}",
            format_bytes(progress.received),
        )
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / MIB)
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn format_transfer_rate(bytes_per_sec: f64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    if bytes_per_sec >= MIB {
        format!("{:.1} MB/s", bytes_per_sec / MIB)
    } else if bytes_per_sec >= KIB {
        format!("{:.0} KB/s", bytes_per_sec / KIB)
    } else {
        format!("{:.0} B/s", bytes_per_sec)
    }
}

/// Downloads one pending federated track (into the cache, or the library
/// when save-on-listen is enabled) and reports back with the placeholder id
/// so the queue can swap the resolved track in.
fn spawn_fed_resolve(
    runtime: &Runtime,
    track: &crate::library::models::TrackItem,
    stream_playback: Option<StreamingPlaybackRequest>,
) {
    let Some(fed_track) = track.fed.clone() else {
        return;
    };
    let resolve_key = track_playback_key(track);
    {
        let mut resolving = runtime
            .fed_resolving
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !resolving.insert(resolve_key.clone()) {
            return;
        }
    }
    let placeholder_id = track.id;
    let title = track.title.clone();
    let fed = Arc::clone(&runtime.federation);
    let tx = runtime.event_tx.clone();
    let controller = runtime.player.clone();
    let streaming = Arc::clone(&runtime.fed_streaming);
    tokio::spawn(async move {
        let progress = download_progress_sender(tx.clone(), title.clone());
        let result = if let Some(playback) =
            stream_playback.filter(|playback| playback.position_secs <= 0.5)
        {
            let stream_tx = tx.clone();
            let stream_title = title.clone();
            let stream_controller = controller.clone();
            let stream_resolve_key = resolve_key.clone();
            let stream_markers = Arc::clone(&streaming);
            let mut started = false;
            fed.prepare_playback_streaming_with_progress(&fed_track, progress, move |stream| {
                if started {
                    return;
                }
                started = true;
                stream_markers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(stream_resolve_key.clone());
                stream_controller.play_stream(
                    Box::new(stream.reader),
                    Some(stream.mime_type),
                    player::amplitude(playback.volume),
                );
                if playback.paused {
                    stream_controller.pause();
                }
                let _ = stream_tx.send(AppEvent::StatusMessage(format!(
                    "federation: streaming \"{stream_title}\" while downloading…"
                )));
            })
            .await
        } else {
            fed.prepare_playback_with_progress(&fed_track, progress)
                .await
        }
        .map(Box::new)
        .map_err(|err| format!("{err:#}"));
        let _ = tx.send(AppEvent::FedTrackResolved {
            placeholder_id,
            resolve_key,
            result,
        });
    });
}

fn spawn_content_id_resolve(
    runtime: &Runtime,
    track: &crate::library::models::TrackItem,
    stream_playback: Option<StreamingPlaybackRequest>,
) {
    let Some(content_id) = track_content_id(track) else {
        return;
    };
    let resolve_key = track_playback_key(track);
    {
        let mut resolving = runtime
            .fed_resolving
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !resolving.insert(resolve_key.clone()) {
            return;
        }
    }
    let placeholder_id = track.id;
    let label = format!("{} {}", track.artist_line(), track.title);
    let title = track.title.clone();
    let fed = Arc::clone(&runtime.federation);
    let tx = runtime.event_tx.clone();
    let controller = runtime.player.clone();
    let streaming = Arc::clone(&runtime.fed_streaming);
    tokio::spawn(async move {
        let result = match fed.track_by_content_id(&content_id, Some(&label)).await {
            Ok(fed_track) => {
                let _ = tx.send(AppEvent::StatusMessage(format!(
                    "federation: found source for \"{title}\", downloading…"
                )));
                let progress = download_progress_sender(tx.clone(), title.clone());
                if let Some(playback) =
                    stream_playback.filter(|playback| playback.position_secs <= 0.5)
                {
                    let stream_tx = tx.clone();
                    let stream_title = title.clone();
                    let stream_controller = controller.clone();
                    let stream_resolve_key = resolve_key.clone();
                    let stream_markers = Arc::clone(&streaming);
                    let mut started = false;
                    fed.prepare_playback_streaming_with_progress(
                        &fed_track,
                        progress,
                        move |stream| {
                            if started {
                                return;
                            }
                            started = true;
                            stream_markers
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .insert(stream_resolve_key.clone());
                            stream_controller.play_stream(
                                Box::new(stream.reader),
                                Some(stream.mime_type),
                                player::amplitude(playback.volume),
                            );
                            if playback.paused {
                                stream_controller.pause();
                            }
                            let _ = stream_tx.send(AppEvent::StatusMessage(format!(
                                "federation: streaming \"{stream_title}\" while downloading…"
                            )));
                        },
                    )
                    .await
                } else {
                    fed.prepare_playback_with_progress(&fed_track, progress)
                        .await
                }
            }
            Err(err) => Err(err),
        }
        .map(Box::new)
        .map_err(|err| format!("{err:#}"));
        let _ = tx.send(AppEvent::FedTrackResolved {
            placeholder_id,
            resolve_key,
            result,
        });
    });
}

/// Downloads federated tracks into the library one by one (with progress in
/// the status bar) and optionally links them to a playlist afterwards.
pub(crate) fn fed_download_spawn(
    runtime: &Runtime,
    tracks: Vec<crate::federation::FedTrack>,
    playlist: Option<(i64, String)>,
) {
    if tracks.is_empty() {
        return;
    }
    let fed = Arc::clone(&runtime.federation);
    let library = Arc::clone(&runtime.library);
    let devices = Arc::clone(&runtime.devices);
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let total = tracks.len();
        let mut imported_ids = Vec::new();
        let mut imported_fed_tracks = Vec::new();
        let mut failed = 0usize;
        for (index, track) in tracks.iter().enumerate() {
            let title = track.title.clone();
            let _ = tx.send(AppEvent::StatusMessage(format!(
                "federation: downloading {}/{total}: {}",
                index + 1,
                track.title
            )));
            let progress = download_progress_sender(tx.clone(), title);
            match fed.download_to_library_with_progress(track, progress).await {
                Ok(imported) => {
                    if let Some(content_id) = track_content_id(&imported) {
                        let _ = tx.send(AppEvent::LocalContentAvailable { content_id });
                    }
                    imported_ids.push(imported.id);
                    imported_fed_tracks.push(track.clone());
                }
                Err(err) => {
                    failed += 1;
                    tracing::warn!(title = %track.title, "federated download failed: {err:#}");
                }
            }
        }
        let mut message = format!("federation: downloaded {} of {total}", imported_ids.len());
        if failed > 0 {
            message.push_str(&format!(" ({failed} failed)"));
        }
        if let Some((playlist_id, playlist_title)) = playlist
            && !imported_ids.is_empty()
        {
            let library = Arc::clone(&library);
            let devices = Arc::clone(&devices);
            let tx_add = tx.clone();
            let title = playlist_title.clone();
            tokio::task::spawn_blocking(move || {
                let result = library
                    .add_tracks_to_playlist(playlist_id, &imported_ids)
                    .map_err(|err| format!("{err:#}"));
                if result.is_ok()
                    && let Err(err) =
                        devices.record_playlist_fed_tracks_added(playlist_id, &imported_fed_tracks)
                {
                    tracing::warn!(%err, playlist_id, "recording synced playlist add failed");
                }
                let _ = tx_add.send(AppEvent::PlaylistTracksAdded {
                    playlist_id,
                    playlist_title: title,
                    result,
                });
            });
        }
        let _ = tx.send(AppEvent::LibraryChanged {
            message: Some(message),
        });
    });
}

/// Request a fresh status snapshot for the Federation tab.
fn fed_spawn_status(runtime: &Runtime) {
    let fed = Arc::clone(&runtime.federation);
    let devices = Arc::clone(&runtime.devices);
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let _ = tx.send(AppEvent::FederationStatus(fed.status().await));
        let _ = tx.send(AppEvent::DeviceSyncStatus(devices.status()));
    });
}

/// Clear a timed-out quit confirmation and its status-bar hint.
fn expire_quit_confirmation(state: &mut AppState) {
    if state
        .quit_armed_until
        .is_some_and(|deadline| std::time::Instant::now() > deadline)
    {
        state.quit_armed_until = None;
        if state.status_message.as_deref() == Some(update::QUIT_CONFIRM_HINT) {
            state.status_message = None;
        }
    }
}

fn handle_terminal_event(
    state: &mut AppState,
    keymap: &mut Keymap,
    runtime: &mut Runtime,
    event: TermEvent,
) {
    match event {
        TermEvent::Key(key) => {
            // Kitty-enhanced terminals and Windows also deliver Release
            // events; acting on them would double-fire every binding.
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                return;
            }
            if state.popup.is_some() {
                popup::handle_key(state, runtime, key);
            } else if state.cmdline.active {
                cmdline::handle_key(state, runtime, key);
            } else {
                handle_main_key(state, keymap, runtime, key);
            }
        }
        TermEvent::Paste(pasted) => {
            if state.popup.is_some() {
                popup::handle_paste(state, &pasted);
            } else if state.cmdline.active {
                cmdline::handle_paste(state, runtime, &pasted);
            }
        }
        _ => {}
    }
}

fn handle_main_key(
    state: &mut AppState,
    keymap: &mut Keymap,
    runtime: &mut Runtime,
    key: KeyEvent,
) {
    let combo = KeyCombination::from(key);
    match keymap.resolve(combo, state.active_tab.key_context()) {
        KeyResolution::Action(action) => {
            state.pending_keys = None;
            // trace, not debug: on the Logs tab every keypress would
            // otherwise append a line and pollute what's being read.
            tracing::trace!(?action, "key resolved");
            if let Some(effect) = update(state, action) {
                perform_effect(state, runtime, effect);
            }
        }
        KeyResolution::Pending(keys) => state.pending_keys = Some(keys),
        KeyResolution::Unmatched => state.pending_keys = None,
    }
}

/// `:import <path>` — run a library import in the background, reporting
/// progress into the status bar.
pub(super) fn spawn_import(state: &mut AppState, runtime: &Runtime, path: &str) {
    let expanded = expand_tilde(path);
    state.status_message = Some(format!("importing {}…", expanded.display()));
    let library = Arc::clone(&runtime.library);
    let tx = runtime.event_tx.clone();
    tokio::task::spawn_blocking(move || {
        let progress_tx = tx.clone();
        let mut last_refresh = std::time::Instant::now();
        let result =
            crate::library::import::import_path(&library, &expanded, |done, total, name| {
                let _ = progress_tx.send(AppEvent::ImportProgress {
                    done,
                    total,
                    current: name.to_string(),
                });
                // Long imports show up in the views as they go, not only at
                // the end: refresh about once a second.
                if done < total && last_refresh.elapsed() >= Duration::from_secs(1) {
                    last_refresh = std::time::Instant::now();
                    let _ = progress_tx.send(AppEvent::LibraryChanged { message: None });
                }
            });
        let event = match result {
            Ok(outcome) => {
                for (file, error) in &outcome.failed {
                    tracing::warn!(file = %file.display(), error, "file was not imported");
                }
                AppEvent::LibraryChanged {
                    message: Some(outcome.summary()),
                }
            }
            Err(err) => AppEvent::StatusMessage(format!("import failed: {err:#}")),
        };
        let _ = tx.send(event);
    });
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// The library changed (import, edit, delete): reload everything that is
/// currently on screen or cached, replacing data in place so nothing
/// flashes "loading" while the user keeps browsing. Runs repeatedly during
/// long imports, so every step must be cheap and non-disruptive.
fn on_library_changed(state: &mut AppState, runtime: &mut Runtime) {
    refresh_artists(state, runtime);

    // Refresh every cached drill-down view in place; the handlers replace
    // the entries when the fresh data arrives.
    for id in state.artist_views.keys().copied().collect::<Vec<_>>() {
        let library = Arc::clone(&runtime.library);
        let tx = runtime.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = library.artist(id).map_err(err_string);
            let _ = tx.send(AppEvent::ArtistViewLoaded { id, result });
        });
    }
    for id in state.release_views.keys().copied().collect::<Vec<_>>() {
        let library = Arc::clone(&runtime.library);
        let tx = runtime.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = library.release(id).map_err(err_string);
            let _ = tx.send(AppEvent::ReleaseViewLoaded { id, result });
        });
    }
    for id in state.playlist_views.keys().copied().collect::<Vec<_>>() {
        let library = Arc::clone(&runtime.library);
        let tx = runtime.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = library.playlist(id).map_err(err_string);
            let _ = tx.send(AppEvent::PlaylistViewLoaded { id, result });
        });
    }
    if state.playlists.list.is_some() {
        let library = Arc::clone(&runtime.library);
        let tx = runtime.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = library.playlists().map_err(err_string);
            let _ = tx.send(AppEvent::PlaylistsLoaded(result));
        });
    }
    // Likes reload on the next maintenance pass; the old set stays visible
    // until then.
    state.likes_loaded = false;
    state.local_content_ids_loaded = false;
    // Refresh in place: status cards keep the last successful snapshot
    // instead of flashing `loading` for every background library event.
    if state.local_library_stats.is_none() {
        state.local_library_stats = Some(state::Loadable::Loading);
    }
    refresh_local_library_stats(runtime);

    // Fresh copies of whatever sits in the queue. Federated placeholders
    // and ephemeral tracks (negative ids) are not library rows and keep
    // their in-memory copies.
    let ids: Vec<i64> = state
        .player
        .queue
        .iter()
        .map(|track| track.id)
        .filter(|id| *id >= 0)
        .collect();
    if !ids.is_empty() {
        let library = Arc::clone(&runtime.library);
        let tx = runtime.event_tx.clone();
        tokio::task::spawn_blocking(move || match library.tracks_by_ids(&ids) {
            Ok(tracks) => {
                let _ = tx.send(AppEvent::QueueTracksRefreshed { tracks });
            }
            Err(err) => tracing::warn!(%err, "queue refresh failed"),
        });
    }

    // A live search view shows stale local rows now; refresh only the local
    // half so device-sync/library events do not wipe federated results.
    if state
        .global
        .stack
        .iter()
        .any(|view| matches!(view, state::GlobalView::Search { .. }))
        && !state.search.query.is_empty()
    {
        cmdline::refresh_local_search(state, runtime);
    }
}

/// Reload the artist grid atomically: fetch everything that is loaded now
/// as one page and swap it in when it arrives, so the grid never shows an
/// empty "loading" state in between. Pagination then continues from page 2.
fn refresh_artists(state: &mut AppState, runtime: &Runtime) {
    let global = &mut state.global;
    let needed = artist_grid_capacity() + ARTISTS_PREFETCH_MARGIN;
    let limit = (global.artists.len().max(needed) as i64).clamp(48, 1000);
    let filters = global.filters;
    global.reloading = true;
    let library = Arc::clone(&runtime.library);
    let tx = runtime.event_tx.clone();
    tokio::task::spawn_blocking(move || {
        let event = match library.artists(1, limit, filters) {
            Ok(page) => AppEvent::ArtistsReloaded { page, limit },
            Err(err) => AppEvent::ArtistsLoaded(Err(err_string(err))),
        };
        let _ = tx.send(event);
    });
}

fn save_app_settings(state: &AppState) {
    let settings = crate::config::settings::AppSettings {
        volume: state.player.volume,
        library: state.global.filters,
        music_dir: state.music_dir.clone(),
        similarity: state.similarity.settings.clone(),
    };
    if let Err(err) = crate::config::settings::save(&settings) {
        tracing::warn!(%err, "saving app settings failed");
    }
}

fn reset_artist_pagination(state: &mut AppState) {
    let global = &mut state.global;
    global.artists.clear();
    global.total = 0;
    global.has_more = true;
    global.next_page = 1;
    global.loading = false;
    global.error = None;
    global.selected = 0;
    global.page_limit = None;
    global.reloading = false;
}

/// Swap queue entries for their fresh library copies; tracks that were
/// deleted leave the queue.
fn apply_queue_refresh(
    state: &mut AppState,
    runtime: &mut Runtime,
    tracks: Vec<crate::library::models::TrackItem>,
) {
    let by_id: std::collections::HashMap<i64, _> =
        tracks.into_iter().map(|track| (track.id, track)).collect();
    let by_key: std::collections::HashMap<String, _> = by_id
        .values()
        .cloned()
        .map(|track| (track_playback_key(&track), track))
        .collect();
    let current_id = state.player.current.as_ref().map(|track| track.id);
    let current_key = state.player.current.as_ref().map(track_playback_key);
    for track in &mut state.player.queue {
        if let Some(fresh) = by_id.get(&track.id) {
            *track = fresh.clone();
        } else if let Some(fresh) = by_key.get(&track_playback_key(track)) {
            *track = fresh.clone();
        }
    }
    let had_missing = state
        .player
        .queue
        .iter()
        .any(|track| track.id >= 0 && !by_id.contains_key(&track.id));
    if had_missing {
        state
            .player
            .queue
            .retain(|track| track.id < 0 || by_id.contains_key(&track.id));
        state.player.prefetched_pos = None;
    }
    if state.player.queue.is_empty() {
        if state.player.playing {
            runtime.player_start_pending = false;
            runtime.player.stop();
        }
        state.player = state::PlayerBar::default();
        state.queue_tab.cursor = 0;
        return;
    }
    state.queue_tab.cursor = state.queue_tab.cursor.min(state.player.queue.len() - 1);
    match (current_id, current_key) {
        (Some(id), _) if id < 0 => {
            // The current source can be an ephemeral federated stream while
            // the queue is refreshed after save-on-listen. It is not a
            // deleted library row, so keep playback untouched.
            state.player.queue_pos = state.player.queue_pos.min(state.player.queue.len() - 1);
        }
        (Some(id), Some(key)) if by_id.contains_key(&id) || by_key.contains_key(&key) => {
            if let Some(position) = state
                .player
                .queue
                .iter()
                .position(|track| track_playback_key(track) == key)
            {
                state.player.queue_pos = position;
            }
            state.player.current = by_id
                .get(&id)
                .cloned()
                .or_else(|| by_key.get(&key).cloned());
            push_media_metadata(state, runtime);
        }
        (Some(_), _) => {
            // The playing track was deleted from the library.
            runtime.player_start_pending = false;
            runtime.player.stop();
            state.player.queue_pos = state.player.queue_pos.min(state.player.queue.len() - 1);
            state.player.current = None;
            state.player.playing = false;
            state.player.paused = false;
            push_media_update(state, runtime, true);
        }
        (None, _) => {
            state.player.queue_pos = state.player.queue_pos.min(state.player.queue.len() - 1);
        }
    }
}

fn handle_device_playback_snapshot(
    state: &mut AppState,
    runtime: &mut Runtime,
    snapshot: crate::devices::PlaybackSnapshot,
) {
    // Personal-device reconciliation must never change Jam ownership. Jam
    // has its own authority and lifecycle even when the same TUI also belongs
    // to a trusted-device group.
    if state.device_playback.role == state::DevicePlaybackRole::Jam {
        return;
    }
    if snapshot.device_id == state.device_playback.self_device_id {
        return;
    }
    state
        .device_playback
        .remote
        .insert(snapshot.device_id.clone(), snapshot.clone());
    let now = unix_time_ms();
    state.device_playback.online_devices = state
        .device_playback
        .remote
        .values()
        .filter(|snapshot| {
            now.saturating_sub(snapshot.updated_at_ms) <= state::DEVICE_ONLINE_TTL_MS
        })
        .count()
        + 1;

    if !snapshot.active {
        return;
    }
    // Starting a player is an explicit claim of the active role. Import the
    // current queue/position from the previously active peer, then announce a
    // normal handoff so that the old owner becomes a control device. This is
    // intentionally one-shot: subsequent snapshots use the regular lease and
    // explicit-transfer rules.
    if state.device_playback.startup_takeover_pending {
        state.device_playback.startup_takeover_pending = false;
        become_control_device(state, runtime, snapshot);
        transfer_active_to_this_device(state, runtime);
        state.status_message = Some("playback moved to this newly started player".to_string());
        return;
    }
    if local_active_lease_protected(state, now) {
        tracing::debug!(
            remote = %snapshot.device_id,
            "ignored remote active snapshot while local active playback is protected"
        );
        return;
    }
    let lease_expired = active_idle_lease_expired(&snapshot, now);
    let already_controls_this_device = state.device_playback.is_personal_control()
        && state.device_playback.active_device_id.as_deref() == Some(snapshot.device_id.as_str());
    if !lease_expired || already_controls_this_device {
        let was_active = state.device_playback.is_audio_owner();
        let was_paused = state.player.playing && state.player.paused;
        become_control_device(state, runtime, snapshot.clone());
        if was_active && was_paused {
            state.popup = Some(state::Popup::FedText {
                title: "Active device moved".to_string(),
                text: format!("Playback is now controlled by {}.", snapshot.device_name),
            });
        }
        return;
    }

    if state.device_playback.is_personal_control() {
        return;
    }
    become_active_device(state, runtime, false);
    state.status_message = Some(format!(
        "active playback moved here; {} was idle for 5m",
        snapshot.device_name
    ));
}

fn handle_playback_command(
    state: &mut AppState,
    runtime: &mut Runtime,
    command: crate::devices::PlaybackCommand,
) {
    match command {
        crate::devices::PlaybackCommand::SetState { state: wire, seek } => {
            let old_current_key = state.player.current.as_ref().map(track_playback_key);
            let old_playing = state.player.playing;
            let old_paused = state.player.paused;
            become_active_device(state, runtime, false);
            apply_playback_state_to_ui(state, &wire, Some(runtime.library.as_ref()));
            runtime
                .player
                .set_volume(player::amplitude(state.player.volume));
            if !state.player.playing {
                runtime.player_start_pending = false;
                runtime.player.stop();
                push_media_update(state, runtime, true);
                publish_playback_snapshot(state, runtime);
                return;
            }
            let current_key = state.player.current.as_ref().map(track_playback_key);
            if !old_playing || old_current_key != current_key {
                start_current_audio(
                    state,
                    runtime,
                    state.player.position_secs,
                    state.player.paused,
                );
                push_media_metadata(state, runtime);
            } else {
                if seek {
                    runtime.player.seek(std::time::Duration::from_secs_f64(
                        state.player.position_secs,
                    ));
                }
                if state.player.paused && !old_paused {
                    runtime.player.pause();
                } else if !state.player.paused && old_paused {
                    runtime.player.resume();
                }
            }
            push_media_update(state, runtime, true);
            publish_playback_snapshot(state, runtime);
        }
        crate::devices::PlaybackCommand::ActiveChanged {
            active_device_id,
            active_device_name,
            state: wire,
        } => {
            if active_device_id == state.device_playback.self_device_id {
                return;
            }
            let was_active = state.device_playback.is_audio_owner();
            let snapshot = crate::devices::PlaybackSnapshot {
                device_id: active_device_id,
                device_name: active_device_name,
                active: true,
                updated_at_ms: unix_time_ms(),
                state: wire,
            };
            become_control_device(state, runtime, snapshot);
            runtime.player_start_pending = false;
            push_media_metadata(state, runtime);
            push_media_update(state, runtime, true);
            if was_active {
                state.status_message = Some("active playback moved to another device".into());
            }
        }
    }
}

fn handle_app_event(state: &mut AppState, runtime: &mut Runtime, event: AppEvent) {
    match event {
        AppEvent::UpdateChecked(result) => {
            state.updater.busy = false;
            state.updater.message = match result {
                Ok(Some(update)) => {
                    let message = format!("v{} available", update.version);
                    state.updater.available = Some(update);
                    message
                }
                Ok(None) => "No newer stable release".into(),
                Err(error) => format!("Check failed: {error}"),
            };
            state.status_message = Some(state.updater.message.clone());
        }
        AppEvent::UpdateProgress(message) => state.updater.message = message,
        AppEvent::UpdateInstalled(result) => {
            state.updater.busy = false;
            state.updater.message = match result {
                Ok(()) => {
                    state.updater.installed = true;
                    let version = state
                        .updater
                        .available
                        .as_ref()
                        .map(|u| u.version.as_str())
                        .unwrap_or("new version");
                    format!("Installed {version}; restart furumi")
                }
                Err(error) => format!("Update failed: {error}"),
            };
            state.status_message = Some(state.updater.message.clone());
        }
        AppEvent::StatusMessage(message) => state.status_message = Some(message),
        AppEvent::ListenHistoryLoaded(result) => {
            state.listen_history = Some(match result {
                Ok(entries) => state::Loadable::Ready(entries),
                Err(err) => state::Loadable::Failed(err),
            });
        }
        AppEvent::MusicDirectoryValidated(result) => {
            state.music_dir_changing = false;
            match result {
                Ok(path) => {
                    let current = std::fs::canonicalize(&state.music_dir)
                        .unwrap_or_else(|_| state.music_dir.clone());
                    if path == current {
                        state.status_message =
                            Some("this is already the music save directory".into());
                    } else {
                        state.popup = Some(state::Popup::ConfirmMusicDirectory { path });
                        state.status_message = None;
                    }
                }
                Err(message) => {
                    state.status_message = Some(format!(
                        "music directory is not writable; nothing changed: {message}"
                    ));
                }
            }
        }
        AppEvent::MusicDirectoryChanged(result) => {
            state.music_dir_changing = false;
            match result {
                Ok(stats) => {
                    state.music_dir = runtime.federation.media_dir();
                    save_app_settings(state);
                    state.status_message = Some(format!(
                        "music directory changed · moved {} track(s), {} image(s)",
                        stats.tracks, stats.images
                    ));
                    let _ = runtime.event_tx.send(AppEvent::LibraryChanged {
                        message: state.status_message.clone(),
                    });
                }
                Err(message) => {
                    state.status_message = Some(format!(
                        "music directory change failed; old library kept: {message}"
                    ));
                }
            }
        }
        AppEvent::FederationStatus(status) => {
            state.federation.status = Some(status);
        }
        AppEvent::DeviceSyncStatus(status) => {
            state.federation.devices = Some(status);
            let now = unix_time_ms();
            state.device_playback.online_devices = state
                .federation
                .devices
                .as_ref()
                .map(|status| {
                    status
                        .devices
                        .iter()
                        .filter(|device| {
                            state::device_presence_section(state, device, now)
                                == state::DevicePresenceSection::Online
                        })
                        .count()
                })
                .unwrap_or(1)
                .max(1);
            let active_revoked = state.device_playback.is_personal_control()
                && state
                    .device_playback
                    .active_device_id
                    .as_ref()
                    .is_some_and(|active| {
                        state
                            .federation
                            .devices
                            .as_ref()
                            .and_then(|status| {
                                status
                                    .devices
                                    .iter()
                                    .find(|device| device.device_id == *active)
                            })
                            .is_some_and(|device| device.revoked)
                    });
            let active_missing = state.device_playback.is_personal_control()
                && state
                    .device_playback
                    .active_device_id
                    .as_ref()
                    .is_some_and(|active| {
                        active != &state.device_playback.self_device_id
                            && !state.federation.devices.as_ref().is_some_and(|status| {
                                status
                                    .devices
                                    .iter()
                                    .any(|device| device.device_id == *active)
                            })
                    });
            if active_revoked || active_missing {
                runtime.player.stop();
                state.player.playing = false;
                state.player.current = None;
                state.player.paused = false;
                state.player.position_secs = 0.0;
                become_active_device(state, runtime, false);
                state.status_message = Some("active playback moved to this device".into());
            }
            clamp_settings_cursor(state);
        }
        AppEvent::FedSyncFinished(message) => {
            state.federation.publishing = false;
            state.status_message = Some(message);
        }
        AppEvent::DeviceSyncFinished(message) => {
            state.federation.device_syncing = false;
            state.status_message = Some(message);
        }
        AppEvent::DeviceInvite(result) => match result {
            Ok(invite) => {
                state.popup = Some(state::Popup::FedCopyText {
                    title: "Device invite".to_string(),
                    text: invite,
                    help: "Use this invite on another client within 10 minutes to pair it with this device group.".to_string(),
                    cursor: 0,
                });
                state.status_message = Some("device invite generated".to_string());
            }
            Err(message) => state.status_message = Some(format!("device invite: {message}")),
        },
        AppEvent::DeviceConnectResult(result) => match result {
            Ok(message) => state.status_message = Some(message),
            Err(message) => state.status_message = Some(format!("connect failed: {message}")),
        },
        AppEvent::DevicePairingRequest(request) => {
            state.popup = Some(state::Popup::DevicePairing {
                request_id: request.request_id,
                device_id: request.device_id,
                name: request.name,
                client_version: request.client_version,
                requester_group_id: request.requester_group_id,
                requester_group_active_devices: request.requester_group_active_devices,
            });
            state.federation.devices = Some(runtime.devices.status());
        }
        AppEvent::DevicePlayback(snapshot) => {
            handle_device_playback_snapshot(state, runtime, snapshot);
        }
        AppEvent::PlaybackCommand(_)
            if state.device_playback.role == state::DevicePlaybackRole::Jam =>
        {
            tracing::debug!("ignored personal-device playback command while Jam is active");
        }
        AppEvent::PlaybackCommand(command) => {
            handle_playback_command(state, runtime, command);
        }
        AppEvent::JamStatus(status) => {
            state.jam = status.clone();
            match status.role {
                crate::jam::JamRole::Host => {
                    state.device_playback.role = state::DevicePlaybackRole::Jam;
                    state.device_playback.jam_host = true;
                    publish_playback_snapshot(state, runtime);
                }
                crate::jam::JamRole::Participant => {
                    if state.device_playback.is_audio_owner() {
                        runtime.player.stop();
                        runtime.player_start_pending = false;
                    }
                    state.device_playback.role = state::DevicePlaybackRole::Jam;
                    state.device_playback.jam_host = false;
                }
                crate::jam::JamRole::None => {
                    if state.device_playback.role == state::DevicePlaybackRole::Jam {
                        state.device_playback.role = state::DevicePlaybackRole::Active;
                        state.device_playback.jam_host = false;
                    }
                }
            }
        }
        AppEvent::JamPlayback(snapshot) => {
            let local_volume = state.player.volume;
            become_control_device(state, runtime, snapshot);
            state.device_playback.role = state::DevicePlaybackRole::Jam;
            state.device_playback.jam_host = false;
            state.player.volume = local_volume;
            state.status_message = Some(format!(
                "Jam · controlling {}",
                state.device_playback.active_label()
            ));
        }
        AppEvent::JamCommand(command) => {
            let command = match command {
                crate::devices::PlaybackCommand::SetState {
                    state: mut wire,
                    seek,
                } => {
                    wire.volume = state.player.volume;
                    crate::devices::PlaybackCommand::SetState { state: wire, seek }
                }
                crate::devices::PlaybackCommand::ActiveChanged { .. } => {
                    state.status_message =
                        Some("Jam cannot transfer audio away from its host".into());
                    return;
                }
            };
            handle_playback_command(state, runtime, command);
            state.device_playback.role = state::DevicePlaybackRole::Jam;
            state.device_playback.jam_host = true;
            publish_playback_snapshot(state, runtime);
        }
        AppEvent::JamInvite(result) => match result {
            Ok(invite) => {
                if !state.device_playback.is_audio_owner() {
                    transfer_active_to_this_device(state, runtime);
                }
                state.jam = runtime.jam.status();
                state.device_playback.role = state::DevicePlaybackRole::Jam;
                state.device_playback.jam_host = true;
                publish_playback_snapshot(state, runtime);
                state.popup = Some(state::Popup::FedCopyText {
                    title: "Jam invite".to_string(),
                    text: invite,
                    help: "Copied capability lets federation peers control this host player until restart or regeneration.".to_string(),
                    cursor: 0,
                });
                state.status_message = Some("Jam started".into());
            }
            Err(error) => state.status_message = Some(format!("Jam: {error}")),
        },
        AppEvent::JamJoined(result) => match result {
            Ok(message) => state.status_message = Some(message),
            Err(error) => state.status_message = Some(format!("Jam: {error}")),
        },
        AppEvent::FedSearchLoaded { seq, result } => {
            if runtime.search_seq.load(std::sync::atomic::Ordering::SeqCst) != seq {
                return;
            }
            state.search.fed_loading = false;
            match result {
                Ok(results) => {
                    state.search.fed_artists = results.artists;
                    state.search.fed_tracks = results.tracks;
                }
                Err(message) => tracing::warn!(%message, "federated search failed"),
            }
        }
        AppEvent::FedSimilaritySearchLoaded { seq, result } => {
            if runtime.search_seq.load(std::sync::atomic::Ordering::SeqCst) != seq {
                return;
            }
            state.search.fed_loading = false;
            let selected_key = state.global.stack.last().and_then(|view| match view {
                state::GlobalView::Search { cursor } => state.search.similarity_key(*cursor),
                _ => None,
            });
            match result {
                Ok(results) => {
                    let remote = results.tracks.into_iter().map(|hit| {
                        state::SimilaritySearchHit::Federated {
                            track: hit.track,
                            score: hit.score,
                            embedding_signature: hit.embedding_signature,
                        }
                    });
                    state.search.similarity_tracks.extend(remote);
                    rank_similarity_search_tracks(
                        &mut state.search.similarity_tracks,
                        state.similarity.settings.max_tracks_per_artist,
                    );
                    state.search.similarity_stats = Some(results.stats);
                    state.search.similarity_error = None;
                }
                Err(message) => {
                    tracing::warn!(%message, "federated similarity search failed");
                    state.search.similarity_error = Some(message);
                }
            }
            restore_similarity_cursor(state, selected_key.as_deref());
        }
        AppEvent::FedTrackResolved {
            placeholder_id,
            resolve_key,
            result,
        } => {
            runtime
                .fed_resolving
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&resolve_key);
            let stream_started = runtime
                .fed_streaming
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&resolve_key);
            if state.device_playback.is_control() {
                runtime.player_start_pending = false;
                tracing::debug!(
                    placeholder_id,
                    resolve_key,
                    "ignored local federated track resolution while controlling remote playback"
                );
                if let Err(message) = result {
                    state.status_message = Some(format!("federation: {message}"));
                }
                return;
            }
            let placeholder_key = state
                .player
                .queue
                .iter()
                .find(|track| track.id == placeholder_id)
                .or_else(|| {
                    state
                        .player
                        .current
                        .as_ref()
                        .filter(|track| track.id == placeholder_id)
                })
                .map(track_playback_key)
                .or(Some(resolve_key.clone()));
            let queue_pos_waiting =
                state
                    .player
                    .queue
                    .get(state.player.queue_pos)
                    .is_some_and(|track| {
                        track.id == placeholder_id
                            || placeholder_key
                                .as_deref()
                                .is_some_and(|key| track_playback_key(track) == key)
                    });
            let current_waiting = state.player.current.as_ref().is_some_and(|current| {
                current.id == placeholder_id
                    || placeholder_key
                        .as_deref()
                        .is_some_and(|key| track_playback_key(current) == key)
            });
            let waiting = queue_pos_waiting || current_waiting;
            let resume_position_secs = if waiting {
                state.player.position_secs
            } else {
                0.0
            };
            let already_streaming = waiting && stream_started && state.player.playing;
            match result {
                Ok(playable) => {
                    if playable.imported {
                        if let Some(content_id) = state::track_content_id(&playable.track) {
                            state.local_content_ids.insert(content_id.clone());
                            let _ = runtime
                                .event_tx
                                .send(AppEvent::LocalContentAvailable { content_id });
                        }
                        // Save-on-listen imported the file; refresh the
                        // library views through the standard change path.
                        let _ = runtime.event_tx.send(AppEvent::LibraryChanged {
                            message: Some(format!(
                                "saved \"{}\" to the library",
                                playable.track.title
                            )),
                        });
                    }
                    let resolved = playable.track.clone();
                    let resolved_key = track_playback_key(&resolved);
                    // Swap the placeholder for the real track everywhere it
                    // sits in the queue.
                    for slot in &mut state.player.queue {
                        if slot.id == placeholder_id
                            || placeholder_key
                                .as_deref()
                                .is_some_and(|key| track_playback_key(slot) == key)
                        {
                            *slot = resolved.clone();
                        }
                    }
                    if waiting {
                        // Playback was either parked on this track or already
                        // running from the streaming reader.
                        let paused = state.player.paused;
                        if state
                            .player
                            .queue
                            .get(state.player.queue_pos)
                            .is_none_or(|track| track_playback_key(track) != resolved_key)
                            && let Some(pos) = state
                                .player
                                .queue
                                .iter()
                                .position(|track| track_playback_key(track) == resolved_key)
                        {
                            state.player.queue_pos = pos;
                        }
                        if already_streaming {
                            // Do not swap the currently playing item under
                            // the audio pipeline. The streaming reader will
                            // play through the completed cache file naturally;
                            // the resolved queue entry is for future starts.
                            push_media_update(state, runtime, true);
                        } else {
                            state.player.current =
                                state.player.queue.get(state.player.queue_pos).cloned();
                            start_current_audio(state, runtime, resume_position_secs, paused);
                            push_media_metadata(state, runtime);
                            push_media_update(state, runtime, true);
                        }
                    }
                }
                Err(message) => {
                    state.status_message = Some(format!("federation: {message}"));
                    if waiting {
                        // Skip the failed track instead of stalling the queue.
                        if state.player.queue_pos + 1 < state.player.queue.len() {
                            state.player.queue_pos += 1;
                            update::normalize_play_next_block(&mut state.player);
                            start_current_audio(state, runtime, 0.0, state.player.paused);
                            push_media_metadata(state, runtime);
                            push_media_update(state, runtime, true);
                        } else {
                            runtime.player_start_pending = false;
                            state.player.playing = false;
                            state.player.current = None;
                            runtime.player.stop();
                        }
                    }
                }
            }
        }
        AppEvent::FedTrackInfoLoaded {
            placeholder_id,
            item_id,
            result,
        } => match result {
            Ok(enriched) => {
                if let Some(state::Popup::TrackInfo { tracks, .. }) = &mut state.popup
                    && let Some(slot) = tracks.iter_mut().find(|track| {
                        track.id == placeholder_id
                            && track.fed.as_ref().is_some_and(|fed| fed.item_id == item_id)
                    })
                {
                    *slot = enriched;
                }
            }
            Err(message) => {
                state.status_message = Some(format!("federation metadata: {message}"));
            }
        },
        AppEvent::FedArtistLoaded { name, result } => {
            if let Some((current, data)) = &mut state.fed_artist_view
                && *current == name
            {
                *data = match result {
                    Ok(card) => state::Loadable::Ready(card),
                    Err(message) => state::Loadable::Failed(message),
                };
            }
        }
        AppEvent::ArtistFederationLoaded { id, name, result } => {
            let cursor_anchor = state::artist_cursor_anchor(state, id);
            let entry = match result {
                Ok(card) => state::Loadable::Ready(card),
                Err(message) => {
                    tracing::debug!(artist = id, %name, %message, "artist federation enrichment failed");
                    state::Loadable::Failed(message)
                }
            };
            state.artist_fed_views.insert(id, entry);
            state::restore_artist_cursor_anchor(state, id, cursor_anchor);
        }
        AppEvent::NetworkArtistCacheUpdated { source_id, count } => {
            tracing::debug!(source = %source_id, count, "network artist cache updated");
            if state.active_tab == state::Tab::Global && state.global.stack.is_empty() {
                refresh_artists(state, runtime);
            }
        }
        AppEvent::FedCardArt {
            name,
            release,
            path,
        } => {
            if let Some((current, state::Loadable::Ready(card))) = &mut state.fed_artist_view
                && *current == name
            {
                match &release {
                    None => card.image_path = Some(path.clone()),
                    Some(title) => {
                        if let Some(slot) = card.releases.iter_mut().find(|r| r.title == *title) {
                            slot.cover_path = Some(path.clone());
                        }
                    }
                }
            }
            for data in state.artist_fed_views.values_mut() {
                let state::Loadable::Ready(card) = data else {
                    continue;
                };
                if music_dht::normalize_name(&card.name) != music_dht::normalize_name(&name) {
                    continue;
                }
                match &release {
                    None => card.image_path = Some(path.clone()),
                    Some(title) => {
                        if let Some(slot) = card.releases.iter_mut().find(|r| r.title == *title) {
                            slot.cover_path = Some(path.clone());
                        }
                    }
                }
            }
        }
        AppEvent::FedTicket(result) => match result {
            Ok(ticket) => {
                state.popup = Some(state::Popup::FedCopyText {
                    title: "Connection ticket".to_string(),
                    text: ticket,
                    help: "Copy this ticket and paste it into Connect to a peer on another client."
                        .to_string(),
                    cursor: 0,
                });
            }
            Err(message) => state.status_message = Some(message),
        },
        AppEvent::ArtistsLoaded(Ok(page)) => {
            let global = &mut state.global;
            if global.reloading {
                // A stale page of the pre-change pagination; the pending
                // ArtistsReloaded swap supersedes it.
                return;
            }
            global.loading = false;
            global.total = page.total;
            global.has_more = page.has_more;
            global.next_page = page.page + 1;
            global.artists.extend(page.items);
            if !global.has_more && !global.artists.is_empty() {
                global.selected = global.selected.min(global.artists.len() - 1);
            }
        }
        AppEvent::ArtistsLoaded(Err(message)) => {
            tracing::warn!(%message, "artists page load failed");
            state.global.reloading = false;
            state.global.loading = false;
            state.global.error = Some(message.clone());
            state.status_message = Some(message);
        }
        AppEvent::ArtistsReloaded { page, limit } => {
            let global = &mut state.global;
            let selected_key = global
                .artists
                .get(global.selected)
                .map(|artist| music_dht::normalize_name(&artist.name));
            global.reloading = false;
            global.loading = false;
            global.error = None;
            global.total = page.total;
            global.has_more = page.has_more;
            global.next_page = 2;
            global.page_limit = Some(limit);
            global.artists = page.items;
            if !global.artists.is_empty() {
                global.selected = selected_key
                    .as_deref()
                    .and_then(|key| {
                        global
                            .artists
                            .iter()
                            .position(|artist| music_dht::normalize_name(&artist.name) == key)
                    })
                    .unwrap_or_else(|| global.selected.min(global.artists.len() - 1));
            } else {
                global.selected = 0;
            }
        }
        AppEvent::ArtistViewLoaded { id, result } => {
            let entry = match result {
                Ok(detail) => state::Loadable::Ready(detail),
                Err(message) => {
                    tracing::warn!(artist = id, %message, "artist view load failed");
                    state::Loadable::Failed(message)
                }
            };
            state.artist_views.insert(id, entry);
            maybe_enrich_open_artist(state, runtime);
        }
        AppEvent::ReleaseViewLoaded { id, result } => {
            let entry = match result {
                Ok(detail) => state::Loadable::Ready(detail),
                Err(message) => {
                    tracing::warn!(release = id, %message, "release view load failed");
                    state::Loadable::Failed(message)
                }
            };
            state.release_views.insert(id, entry);
            // A Shift-J jump was waiting for this release: focus its track.
            if let Some((release_id, track_id)) = state.pending_release_focus
                && release_id == id
            {
                state.pending_release_focus = None;
                if let Some(state::Loadable::Ready(detail)) = state.release_views.get(&id) {
                    let position = detail
                        .tracks
                        .iter()
                        .position(|t| t.id == track_id)
                        .unwrap_or(0);
                    if let Some(state::GlobalView::Release { id: top, cursor }) =
                        state.global.stack.last_mut()
                        && *top == release_id
                    {
                        *cursor = position;
                    }
                }
            }
        }
        AppEvent::SearchLoaded { seq, result } => {
            if seq != runtime.search_seq.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            state.search.loading = false;
            match result {
                Ok(results) => state.search.results = Some(results),
                Err(message) => state.status_message = Some(message),
            }
        }
        AppEvent::SimilaritySearchLoaded { seq, result, query } => {
            if seq != runtime.search_seq.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            state.search.loading = false;
            match result {
                Ok(results) => {
                    state.search.similarity_tracks = results
                        .into_iter()
                        .map(|hit| state::SimilaritySearchHit::Local {
                            track: hit.track,
                            score: hit.score,
                            embedding_signature: hit.embedding_signature,
                        })
                        .collect();
                    rank_similarity_search_tracks(
                        &mut state.search.similarity_tracks,
                        state.similarity.settings.max_tracks_per_artist,
                    );
                }
                Err(message) => {
                    state.status_message = Some(format!("similarity search failed: {message}"));
                    return;
                }
            }
            if let Some(query) = query
                && state.federation.settings.enabled
                && runtime.similarity.network_allowed()
            {
                state.search.fed_loading = true;
                let federation = Arc::clone(&runtime.federation);
                let tx = runtime.event_tx.clone();
                tokio::spawn(async move {
                    let result = federation
                        .search_similar(query, 50)
                        .await
                        .map_err(|err| format!("{err:#}"));
                    let _ = tx.send(AppEvent::FedSimilaritySearchLoaded { seq, result });
                });
            }
        }
        AppEvent::SimilarityStatus(status) => state.similarity.status = status,
        AppEvent::SimilarityProfileActivated(profile_id) => {
            state.similarity.settings.active_profile = profile_id;
            state.similarity.status = runtime.similarity.status();
            save_app_settings(state);
        }
        AppEvent::ArtLoaded { key, art } => {
            let entry = match art {
                Some(image) => state::ArtState::Ready(image),
                None => state::ArtState::Failed,
            };
            state.art.insert(key, entry);
        }
        AppEvent::Player(event) if state.device_playback.is_control() => {
            runtime.player_start_pending = false;
            tracing::debug!(
                ?event,
                "ignored local player event while controlling remote playback"
            );
        }
        AppEvent::Player(player::PlayerEvent::Started) => {
            runtime.player_start_pending = false;
        }
        AppEvent::Player(player::PlayerEvent::TrackFinished { has_next }) => {
            // The finished track gets a full-duration, completed entry.
            if let Some(finished) = state.player.current.clone() {
                report_history(
                    runtime,
                    &finished,
                    state.player.listen_id.as_deref(),
                    state.player.track_started_at,
                    (finished.duration_seconds * 1_000.0).round() as i64,
                    music_dht::device_sync::ListenEndReason::Finished,
                );
            }
            if has_next {
                // A prefetched source is already playing; just realign state.
                let next_pos = state
                    .player
                    .prefetched_pos
                    .take()
                    .unwrap_or(state.player.queue_pos + 1);
                state.player.queue_pos = next_pos.min(state.player.queue.len().saturating_sub(1));
                update::normalize_play_next_block(&mut state.player);
                state.player.current = state.player.queue.get(state.player.queue_pos).cloned();
                state.player.position_secs = 0.0;
                state.player.track_started_at = Some(now_epoch_seconds());
                state.player.listen_id = Some(runtime.devices.new_listen_id());
                push_media_metadata(state, runtime);
                push_media_update(state, runtime, true);
            } else {
                state.player.current = None;
                state.player.listen_id = None;
                state.player.prefetched_pos = None;
                if let Some(effect) = update::advance_after_finish(state) {
                    perform_effect(state, runtime, effect);
                }
            }
        }
        AppEvent::Player(player::PlayerEvent::Failed(message)) => {
            runtime.player_start_pending = false;
            tracing::error!(%message, "playback failed");
            state.player.playing = false;
            state.player.paused = false;
            state.status_message = Some(message);
        }
        AppEvent::PrefetchFailed { pos } => {
            if state.device_playback.is_control() {
                return;
            }
            if state.player.prefetched_pos == Some(pos) {
                state.player.prefetched_pos = None;
            }
        }
        AppEvent::PlaylistsLoaded(result) => {
            state.playlists.list = Some(match result {
                Ok(list) => {
                    state.playlists.selected =
                        state.playlists.selected.min(list.len().saturating_sub(1));
                    state::Loadable::Ready(list)
                }
                Err(message) => {
                    tracing::warn!(%message, "playlists load failed");
                    state::Loadable::Failed(message)
                }
            });
        }
        AppEvent::PlaylistViewLoaded { id, result } => {
            let entry = match result {
                Ok(detail) => state::Loadable::Ready(detail),
                Err(message) => state::Loadable::Failed(message),
            };
            state.playlist_views.insert(id, entry);
        }
        AppEvent::LikesLoaded(result) => match result {
            Ok(ids) => {
                state.likes = ids.into_iter().collect();
            }
            Err(message) => tracing::warn!(%message, "likes load failed"),
        },
        AppEvent::LocalContentIdsLoaded(result) => match result {
            Ok(ids) => {
                state.local_content_ids = ids.into_iter().collect();
            }
            Err(message) => {
                state.local_content_ids_loaded = false;
                tracing::warn!(%message, "local content id load failed");
            }
        },
        AppEvent::LocalLibraryStatsLoaded(result) => match result {
            Ok(stats) => {
                state.local_library_stats = Some(state::Loadable::Ready(stats));
            }
            Err(message) => {
                tracing::warn!(%message, "local library stats load failed");
                if !matches!(state.local_library_stats, Some(state::Loadable::Ready(_))) {
                    state.local_library_stats = Some(state::Loadable::Failed(message));
                }
            }
        },
        AppEvent::LocalContentAvailable { content_id } => {
            if let Some(content_id) = music_dht::normalize_content_id(&content_id) {
                state.local_content_ids.insert(content_id);
            }
        }
        AppEvent::FedLikesLoaded(result) => match result {
            Ok(ids) => state.fed_likes = ids.into_iter().collect(),
            Err(message) => tracing::warn!(%message, "federated likes load failed"),
        },
        AppEvent::FedLikeToggled {
            item_id,
            content_id,
            liked,
        } => {
            if liked {
                state.fed_likes.insert(item_id);
                if let Some(content_id) =
                    content_id.and_then(|id| music_dht::normalize_content_id(&id))
                {
                    state.fed_likes.insert(content_id);
                }
            } else {
                state.fed_likes.remove(&item_id);
                if let Some(content_id) =
                    content_id.and_then(|id| music_dht::normalize_content_id(&id))
                {
                    state.fed_likes.remove(&content_id);
                    state.likes.remove(&content_id);
                }
            }
            // The virtual Likes playlist is stale now; refetch on next open.
            state.playlist_views.remove(&state::LIKES_PLAYLIST_ID);
            state.playlists.list = None;
            state.status_message = Some(if liked {
                "♥ liked (federation)".to_string()
            } else {
                "like removed".to_string()
            });
        }
        AppEvent::LikeToggled { content_id, liked } => {
            if liked {
                state.fed_likes.remove(&content_id);
                state.likes.insert(content_id);
            } else {
                state.likes.remove(&content_id);
                state.fed_likes.remove(&content_id);
            }
            // The virtual Likes playlist is stale now; refetch on next open.
            state.playlist_views.remove(&state::LIKES_PLAYLIST_ID);
            state.playlists.list = None;
            state.status_message = Some(if liked {
                "♥ liked".to_string()
            } else {
                "like removed".to_string()
            });
        }
        AppEvent::EnqueueTracks { tracks, next } => {
            let previous_len = state.player.queue.len();
            let restart_current = update::enqueue_tracks(state, tracks, next);
            let count = state.player.queue.len().saturating_sub(previous_len);
            perform_effect(
                state,
                runtime,
                Effect::QueueOrderChanged { restart_current },
            );
            state.status_message = Some(if count == 0 {
                "no tracks available in the current source mode".to_string()
            } else if next {
                format!("{count} tracks queued next")
            } else {
                format!("{count} tracks queued")
            });
        }
        AppEvent::PlaylistCreated { result, add_target } => match result {
            Ok(playlist) => {
                tracing::info!(title = %playlist.title, "playlist created");
                state.status_message = Some(format!("playlist \"{}\" created", playlist.title));
                state.popup = None;
                // The list is stale; refetch when next needed.
                state.playlists.list = None;
                if let Some(target) = add_target {
                    popup::spawn_add_target(runtime, playlist.id, playlist.title.clone(), target);
                }
            }
            Err(message) => {
                tracing::warn!(%message, "playlist creation failed");
                state.status_message = Some(format!("create failed: {message}"));
                if let Some(state::Popup::NewPlaylist { busy, .. }) = &mut state.popup {
                    *busy = false;
                }
            }
        },
        AppEvent::PlaylistTracksAdded {
            playlist_id,
            playlist_title,
            result,
        } => {
            state.popup = None;
            match result {
                Ok(()) => {
                    state.status_message = Some(format!("added to \"{playlist_title}\""));
                    // Counts and contents changed; refetch lazily.
                    state.playlist_views.remove(&playlist_id);
                    state.playlists.list = None;
                }
                Err(message) => {
                    tracing::warn!(%message, playlist_id, "adding to playlist failed");
                    state.status_message = Some(format!("add failed: {message}"));
                }
            }
        }
        AppEvent::LibraryChanged { message } => {
            on_library_changed(state, runtime);
            if state.similarity.settings.enabled {
                runtime.similarity.start();
            }
            if let Some(message) = message {
                state.status_message = Some(message);
            }
        }
        AppEvent::ImportProgress {
            done,
            total,
            current,
        } => {
            state.status_message = Some(format!("importing {done}/{total}: {current}"));
        }
        AppEvent::QueueTracksRefreshed { tracks } => {
            if state.device_playback.is_control() {
                return;
            }
            apply_queue_refresh(state, runtime, tracks);
        }
        AppEvent::Media(command) => {
            use crate::media::MediaCommand;
            tracing::debug!(?command, "media key");
            let action = match command {
                MediaCommand::TogglePause => action::Action::PlayPause,
                MediaCommand::Play if state.player.paused || state.player.current.is_none() => {
                    action::Action::PlayPause
                }
                MediaCommand::Play => return,
                MediaCommand::Pause if state.player.current.is_some() && !state.player.paused => {
                    action::Action::PlayPause
                }
                MediaCommand::Pause => return,
                MediaCommand::Next => action::Action::NextTrack,
                MediaCommand::Previous => action::Action::PrevTrack,
                MediaCommand::Stop => {
                    runtime.player_start_pending = false;
                    state.player.playing = false;
                    state.player.paused = false;
                    state.player.current = None;
                    if state.device_playback.is_control() {
                        record_control_playback_state(state, runtime, false);
                    } else {
                        runtime.player.stop();
                        push_media_update(state, runtime, true);
                    }
                    return;
                }
            };
            if let Some(effect) = update(state, action) {
                perform_effect(state, runtime, effect);
            }
        }
    }
}

fn rank_similarity_search_tracks(
    tracks: &mut Vec<state::SimilaritySearchHit>,
    max_tracks_per_artist: usize,
) {
    const RESULT_LIMIT: usize = 49;
    const MAX_NEAR_DUPLICATE_SIGNATURE_DISTANCE: u32 = 8;

    tracks.sort_by(|left, right| right.score().total_cmp(&left.score()));
    let candidates = std::mem::take(tracks);
    let mut content = std::collections::HashSet::new();
    let mut signatures = Vec::new();
    let mut artist_counts: std::collections::HashMap<String, usize> = Default::default();
    for hit in candidates {
        if !content.insert(hit.content_key()) {
            continue;
        }
        if hit.embedding_signature().is_some_and(|candidate| {
            signatures.iter().any(|existing| {
                music_dht::similarity::signature_distance(&candidate, existing)
                    <= MAX_NEAR_DUPLICATE_SIGNATURE_DISTANCE
            })
        }) {
            continue;
        }
        let artist = hit.primary_artist_key();
        let count = artist_counts.entry(artist.clone()).or_default();
        if !artist.is_empty() && *count >= max_tracks_per_artist.clamp(1, RESULT_LIMIT) {
            continue;
        }
        *count += 1;
        if let Some(signature) = hit.embedding_signature() {
            signatures.push(signature);
        }
        tracks.push(hit);
        if tracks.len() >= RESULT_LIMIT {
            break;
        }
    }
}

fn restore_similarity_cursor(state: &mut AppState, selected_key: Option<&str>) {
    let selected_index = selected_key.and_then(|key| state.search.similarity_index_for_key(key));
    let len = state.search.similarity_len();
    if let Some(state::GlobalView::Search { cursor }) = state.global.stack.last_mut() {
        *cursor = selected_index.unwrap_or(*cursor).min(len.saturating_sub(1));
    }
}

#[cfg(test)]
mod similarity_search_tests {
    use super::*;
    use crate::library::models::{ArtistRef, TrackItem};

    fn local_hit(id: i64, artist: &str, score: f32, signature: u8) -> state::SimilaritySearchHit {
        state::SimilaritySearchHit::Local {
            track: TrackItem {
                id,
                title: format!("local {id}"),
                track_number: None,
                disc_number: None,
                duration_seconds: 1.0,
                artists: vec![ArtistRef {
                    id,
                    name: artist.to_string(),
                }],
                featured_artists: Vec::new(),
                release_id: id,
                release_title: "release".to_string(),
                release_year: None,
                file_path: format!("/music/{id}"),
                content_id: Some(format!("local-{id}")),
                cover_path: None,
                audio_format: None,
                audio_bitrate: None,
                audio_sample_rate: None,
                audio_bit_depth: None,
                file_size_bytes: None,
                play_count: 0,
                fed: None,
            },
            score,
            embedding_signature: [signature; music_dht::similarity::SIMILARITY_SIGNATURE_BYTES],
        }
    }

    fn remote_hit(artist: &str, score: f32, signature: u8) -> state::SimilaritySearchHit {
        state::SimilaritySearchHit::Federated {
            track: crate::federation::FedTrack {
                item_id: format!("remote-{signature}"),
                owner: "peer".to_string(),
                own: false,
                title: format!("remote {signature}"),
                artist_names: vec![artist.to_string()],
                featured_artist_names: Vec::new(),
                year: None,
                duration_seconds: Some(1),
                content_id: Some(format!("remote-{signature}")),
                release_title: None,
                track_number: None,
                disc_number: None,
            },
            score,
            embedding_signature: Some(
                [signature; music_dht::similarity::SIMILARITY_SIGNATURE_BYTES],
            ),
        }
    }

    #[test]
    fn similarity_results_rank_local_and_remote_together_with_one_artist_cap() {
        let mut tracks = vec![
            local_hit(1, "same artist", 0.70, 1),
            remote_hit("other artist", 0.90, 2),
            remote_hit("same artist", 0.80, 3),
            local_hit(2, "same artist", 0.60, 4),
        ];

        rank_similarity_search_tracks(&mut tracks, 1);

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].score(), 0.90);
        assert_eq!(tracks[1].score(), 0.80);
        assert!(matches!(
            tracks[0],
            state::SimilaritySearchHit::Federated { .. }
        ));
    }
}

/// Mirror the playback state to the OS now-playing surface. `force` skips
/// the position throttle (track switches, pauses).
fn push_media_update(state: &AppState, runtime: &mut Runtime, force: bool) {
    use crate::media::MediaUpdate;
    const POSITION_INTERVAL: Duration = Duration::from_secs(2);
    if !force
        && runtime
            .last_media_push
            .is_some_and(|at| at.elapsed() < POSITION_INTERVAL)
    {
        return;
    }
    runtime.last_media_push = Some(std::time::Instant::now());
    let player = &state.player;
    if !player.playing {
        let _ = runtime.media_tx.send(MediaUpdate::Stopped);
        return;
    }
    let _ = runtime.media_tx.send(MediaUpdate::Playback {
        playing: player.playing,
        paused: player.paused,
        position_secs: player.position_secs,
    });
}

fn push_media_metadata(state: &AppState, runtime: &Runtime) {
    use crate::media::MediaUpdate;
    if let Some(track) = &state.player.current {
        let _ = runtime.media_tx.send(MediaUpdate::Metadata {
            title: track.title.clone(),
            artist: track.artist_line(),
            album: track.release_title.clone(),
            duration_secs: track.duration_seconds,
        });
    }
}
