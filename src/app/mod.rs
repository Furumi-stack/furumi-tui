pub mod action;
mod cmdline;
pub mod input;
pub mod command;
pub mod event;
mod popup;
pub mod state;
pub mod update;

use std::path::PathBuf;
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

/// Handles shared by background tasks; AppState stays pure UI data.
pub struct Runtime {
    pub event_tx: mpsc::UnboundedSender<AppEvent>,
    pub library: Arc<Library>,
    pub federation: Arc<crate::federation::Federation>,
    /// When the last Federation-tab status snapshot was requested.
    pub fed_status_at: Option<std::time::Instant>,
    /// Caps concurrent artwork loads so they never starve the disk.
    pub art_semaphore: Arc<tokio::sync::Semaphore>,
    /// Monotonic sequence for live search; stale responses are dropped.
    pub search_seq: Arc<std::sync::atomic::AtomicU64>,
    pub player: player::Controller,
    pub player_start_pending: bool,
    pub media_tx: std::sync::mpsc::Sender<crate::media::MediaUpdate>,
    pub last_media_push: Option<std::time::Instant>,
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

    let mut state = AppState {
        status_message: startup_warning,
        ..AppState::default()
    };

    let federation = crate::federation::Federation::new(Arc::clone(&library));
    state.federation.settings = federation.settings();
    let player_events = event_tx.clone();
    let mut runtime = Runtime {
        event_tx,
        library,
        federation,
        fed_status_at: None,
        art_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        search_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        player: player::spawn(move |event| {
            let _ = player_events.send(AppEvent::Player(event));
        }),
        player_start_pending: false,
        media_tx,
        last_media_push: None,
    };

    {
        let fed = Arc::clone(&runtime.federation);
        let tx = runtime.event_tx.clone();
        tokio::spawn(async move {
            fed.start_if_enabled().await;
            let status = fed.status().await;
            let _ = tx.send(AppEvent::FederationStatus(status));
        });
    }

    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(TICK_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        terminal.draw(|frame| ui::draw(frame, &state, &keymap))?;

        tokio::select! {
            maybe_event = input.next() => match maybe_event {
                Some(Ok(event)) => handle_terminal_event(&mut state, &mut keymap, &mut runtime, event),
                Some(Err(err)) => return Err(err.into()),
                None => state.should_quit = true,
            },
            Some(app_event) = event_rx.recv() => handle_app_event(&mut state, &mut runtime, app_event),
            _ = tick.tick() => {
                expire_quit_confirmation(&mut state);
                if state.player.current.is_some() && !runtime.player_start_pending {
                    state.player.position_secs = runtime.player.shared.position().as_secs_f64();
                    state.player.paused = runtime.player.shared.paused();
                }
                maybe_prefetch_next(&mut state, &runtime);
                push_media_update(&state, &mut runtime, false);
            }
        }

        if state.should_quit {
            runtime.federation.shutdown().await;
            return Ok(());
        }
        maintenance(&mut state, &mut runtime);
    }
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
            let limit = *global
                .page_limit
                .get_or_insert_with(|| (needed as i64).clamp(48, 200));
            let library = Arc::clone(&runtime.library);
            let tx = runtime.event_tx.clone();
            tokio::task::spawn_blocking(move || {
                let result = library.artists(page, limit).map_err(err_string);
                let _ = tx.send(AppEvent::ArtistsLoaded(result));
            });
        }
    }

    // Liked ids load once per session — markers are shown everywhere.
    if !state.likes_loaded {
        state.likes_loaded = true;
        let library = Arc::clone(&runtime.library);
        let tx = runtime.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = library.likes().map_err(err_string);
            let _ = tx.send(AppEvent::LikesLoaded(result));
        });
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
            && let Some(path) = &detail.cover_path {
                wanted.push((path.clone(), header.0, header.1));
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
    match effect {
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
        Effect::SetVolume(volume) => runtime.player.set_volume(player::amplitude(volume)),
        Effect::SetOptions => {}
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
        Effect::ToggleLikes { track_ids } => {
            // Ephemeral federated tracks (negative ids) are not in the DB.
            let track_ids: Vec<i64> = track_ids.into_iter().filter(|id| *id >= 0).collect();
            if track_ids.is_empty() {
                return;
            }
            let library = Arc::clone(&runtime.library);
            let tx = runtime.event_tx.clone();
            tokio::task::spawn_blocking(move || {
                for track_id in track_ids {
                    match library.toggle_like(track_id) {
                        Ok(liked) => {
                            let _ = tx.send(AppEvent::LikeToggled { track_id, liked });
                        }
                        Err(err) => {
                            tracing::warn!(%err, track_id, "like toggle failed");
                            let _ =
                                tx.send(AppEvent::StatusMessage(format!("like failed: {err:#}")));
                            break;
                        }
                    }
                }
            });
        }
        Effect::RemoveFromPlaylist {
            playlist_id,
            track_ids,
        } => {
            let library = Arc::clone(&runtime.library);
            let tx = runtime.event_tx.clone();
            tokio::task::spawn_blocking(move || {
                let event = match library.remove_tracks_from_playlist(playlist_id, &track_ids) {
                    Ok(()) => AppEvent::LibraryChanged {
                        message: Some(format!("removed {} track(s)", track_ids.len())),
                    },
                    Err(err) => AppEvent::StatusMessage(format!("remove failed: {err:#}")),
                };
                let _ = tx.send(event);
            });
        }
        Effect::FedApplySettings => fed_apply_settings(state, runtime),
        Effect::FedSyncNow => {
            let fed = Arc::clone(&runtime.federation);
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let message = match fed.sync_now().await {
                    Ok(()) => "federation: library published".to_string(),
                    Err(err) => format!("federation sync failed: {err:#}"),
                };
                let _ = tx.send(AppEvent::FederationStatus(fed.status().await));
                let _ = tx.send(AppEvent::StatusMessage(message));
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
        Effect::FedPlay(fed_track) => {
            let fed = Arc::clone(&runtime.federation);
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let result = fed
                    .prepare_playback(&fed_track)
                    .await
                    .map_err(|err| format!("{err:#}"));
                let _ = tx.send(AppEvent::FedPlayReady { result });
            });
        }
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
    let Some(track) = state.player.queue.get(state.player.queue_pos).cloned() else {
        return;
    };
    // The track that was playing until now was cut short by this switch.
    let previous_started_at = state.player.track_started_at;
    let same_track_started_at = if let Some(previous) = state.player.current.take() {
        let same_track = previous.id == track.id;
        if state.player.playing && previous.id != track.id {
            report_history(
                runtime,
                previous.id,
                state.player.track_started_at,
                state.player.position_secs.round() as i32,
                false,
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
    state.player.track_started_at = same_track_started_at.or_else(|| Some(now_epoch_seconds()));
    state.player.prefetched_pos = None;
    state.status_message = Some(format!("▶ {} — {}", track.title, track.artist_line()));

    runtime.player_start_pending = true;
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

fn open_track_file(path: &str) -> std::io::Result<(player::TrackReader, Option<u64>)> {
    let file = std::fs::File::open(path)?;
    let byte_len = file.metadata().ok().map(|meta| meta.len());
    Ok((std::io::BufReader::new(file), byte_len))
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
    let Some(next) = player.queue.get(next_pos).cloned() else {
        return;
    };
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
    track_id: i64,
    started_at: Option<i64>,
    listened: i32,
    completed: bool,
) {
    // Ephemeral federated tracks are not library rows; no history for them.
    if listened < 5 || track_id < 0 {
        return;
    }
    let library = Arc::clone(&runtime.library);
    tokio::task::spawn_blocking(move || {
        if let Err(err) = library.add_history(track_id, started_at, listened, completed) {
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
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let total = tracks.len();
        let mut imported_ids = Vec::new();
        let mut failed = 0usize;
        for (index, track) in tracks.iter().enumerate() {
            let _ = tx.send(AppEvent::StatusMessage(format!(
                "federation: downloading {}/{total}: {}",
                index + 1,
                track.title
            )));
            match fed.download_to_library(track).await {
                Ok(imported) => imported_ids.push(imported.id),
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
            let tx_add = tx.clone();
            let title = playlist_title.clone();
            tokio::task::spawn_blocking(move || {
                let result = library
                    .add_tracks_to_playlist(playlist_id, &imported_ids)
                    .map_err(|err| format!("{err:#}"));
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
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let _ = tx.send(AppEvent::FederationStatus(fed.status().await));
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
        && let Some(home) = std::env::home_dir() {
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

    // Fresh copies of whatever sits in the queue.
    let ids: Vec<i64> = state.player.queue.iter().map(|track| track.id).collect();
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

    // A live search view shows stale rows now; run the query again.
    if state
        .global
        .stack
        .iter()
        .any(|view| matches!(view, state::GlobalView::Search { .. }))
        && !state.search.query.is_empty()
    {
        cmdline::schedule_search(state, runtime);
    }
}

/// Reload the artist grid atomically: fetch everything that is loaded now
/// as one page and swap it in when it arrives, so the grid never shows an
/// empty "loading" state in between. Pagination then continues from page 2.
fn refresh_artists(state: &mut AppState, runtime: &Runtime) {
    let global = &mut state.global;
    let needed = artist_grid_capacity() + ARTISTS_PREFETCH_MARGIN;
    let limit = (global.artists.len().max(needed) as i64).clamp(48, 1000);
    global.reloading = true;
    let library = Arc::clone(&runtime.library);
    let tx = runtime.event_tx.clone();
    tokio::task::spawn_blocking(move || {
        let event = match library.artists(1, limit) {
            Ok(page) => AppEvent::ArtistsReloaded { page, limit },
            Err(err) => AppEvent::ArtistsLoaded(Err(err_string(err))),
        };
        let _ = tx.send(event);
    });
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
    let current_id = state.player.current.as_ref().map(|track| track.id);
    for track in &mut state.player.queue {
        if let Some(fresh) = by_id.get(&track.id) {
            *track = fresh.clone();
        }
    }
    let had_missing = state
        .player
        .queue
        .iter()
        .any(|track| !by_id.contains_key(&track.id));
    if had_missing {
        state.player.queue.retain(|track| by_id.contains_key(&track.id));
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
    match current_id {
        Some(id) if by_id.contains_key(&id) => {
            if let Some(position) = state.player.queue.iter().position(|track| track.id == id) {
                state.player.queue_pos = position;
            }
            state.player.current = by_id.get(&id).cloned();
            push_media_metadata(state, runtime);
        }
        Some(_) => {
            // The playing track was deleted from the library.
            runtime.player_start_pending = false;
            runtime.player.stop();
            state.player.queue_pos = state.player.queue_pos.min(state.player.queue.len() - 1);
            state.player.current = None;
            state.player.playing = false;
            state.player.paused = false;
            push_media_update(state, runtime, true);
        }
        None => {
            state.player.queue_pos = state.player.queue_pos.min(state.player.queue.len() - 1);
        }
    }
}

fn handle_app_event(state: &mut AppState, runtime: &mut Runtime, event: AppEvent) {
    match event {
        AppEvent::StatusMessage(message) => state.status_message = Some(message),
        AppEvent::FederationStatus(status) => {
            state.federation.status = Some(status);
        }
        AppEvent::FedSearchLoaded { seq, result } => {
            if runtime
                .search_seq
                .load(std::sync::atomic::Ordering::SeqCst)
                != seq
            {
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
        AppEvent::FedPlayReady { result } => match result {
            Ok(playable) => {
                if playable.imported {
                    // Save-on-listen imported the file; refresh the library
                    // views through the standard change path.
                    let _ = runtime.event_tx.send(AppEvent::LibraryChanged {
                        message: Some(format!("saved \"{}\" to the library", playable.track.title)),
                    });
                }
                state.player.queue = vec![playable.track];
                state.player.queue_pos = 0;
                update::on_new_queue(state);
                perform_effect(state, runtime, Effect::PlayCurrent);
            }
            Err(message) => state.status_message = Some(format!("federation: {message}")),
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
        AppEvent::FedCardArt {
            name,
            release,
            path,
        } => {
            if let Some((current, state::Loadable::Ready(card))) = &mut state.fed_artist_view
                && *current == name
            {
                match release {
                    None => card.image_path = Some(path),
                    Some(title) => {
                        if let Some(slot) = card.releases.iter_mut().find(|r| r.title == title) {
                            slot.cover_path = Some(path);
                        }
                    }
                }
            }
        }
        AppEvent::FedTicket(result) => match result {
            Ok(ticket) => {
                state.popup = Some(state::Popup::FedText {
                    title: "Federation ticket (share with a peer)".to_string(),
                    text: ticket,
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
            global.reloading = false;
            global.loading = false;
            global.error = None;
            global.total = page.total;
            global.has_more = page.has_more;
            global.next_page = 2;
            global.page_limit = Some(limit);
            global.artists = page.items;
            if !global.artists.is_empty() {
                global.selected = global.selected.min(global.artists.len() - 1);
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
                && release_id == id {
                    state.pending_release_focus = None;
                    if let Some(state::Loadable::Ready(detail)) = state.release_views.get(&id) {
                        let position = detail
                            .tracks
                            .iter()
                            .position(|t| t.id == track_id)
                            .unwrap_or(0);
                        if let Some(state::GlobalView::Release { id: top, cursor }) =
                            state.global.stack.last_mut()
                            && *top == release_id {
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
        AppEvent::ArtLoaded { key, art } => {
            let entry = match art {
                Some(image) => state::ArtState::Ready(image),
                None => state::ArtState::Failed,
            };
            state.art.insert(key, entry);
        }
        AppEvent::Player(player::PlayerEvent::Started) => {
            runtime.player_start_pending = false;
        }
        AppEvent::Player(player::PlayerEvent::TrackFinished { has_next }) => {
            // The finished track gets a full-duration, completed entry.
            if let Some(finished) = state.player.current.clone() {
                report_history(
                    runtime,
                    finished.id,
                    state.player.track_started_at,
                    finished.duration_seconds.round() as i32,
                    true,
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
                state.player.current = state.player.queue.get(state.player.queue_pos).cloned();
                state.player.position_secs = 0.0;
                state.player.track_started_at = Some(now_epoch_seconds());
                push_media_metadata(state, runtime);
                push_media_update(state, runtime, true);
            } else {
                state.player.current = None;
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
        AppEvent::LikeToggled { track_id, liked } => {
            if liked {
                state.likes.insert(track_id);
            } else {
                state.likes.remove(&track_id);
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
            let count = tracks.len();
            update::enqueue_tracks(state, tracks, next);
            state.status_message = Some(if next {
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
                    runtime.player.stop();
                    push_media_update(state, runtime, true);
                    return;
                }
            };
            if let Some(effect) = update(state, action) {
                perform_effect(state, runtime, effect);
            }
        }
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
