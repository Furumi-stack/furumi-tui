pub mod action;
mod cmdline;
pub mod command;
pub mod event;
mod login;
mod popup;
mod sso;
pub mod state;
pub mod update;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crokey::KeyCombination;
use crossterm::event::{Event as TermEvent, EventStream, KeyEvent, KeyEventKind};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::api::auth;
use crate::api::client::{ApiClient, ApiError, http_client};
use crate::config::keymap::{KeyResolution, Keymap};
use crate::player;
use crate::ui;
use event::AppEvent;
use state::{AppState, Screen};
use update::{Effect, update};

const TICK_INTERVAL: Duration = Duration::from_millis(250);
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(500);
// A paused StreamDownload can later issue Range requests with the bearer it
// captured at open time. Reopen after idle so resume gets a freshly refreshed
// token instead of reviving a stale HTTP stream.
const STALE_STREAM_PAUSE_REOPEN_AFTER: Duration = Duration::from_secs(30);

/// Handles shared by background tasks; AppState stays pure UI data.
pub struct Runtime {
    pub event_tx: mpsc::UnboundedSender<AppEvent>,
    pub http: reqwest::Client,
    pub api: Option<Arc<ApiClient>>,
    pub sso: Option<sso::SsoListener>,
    /// Caps concurrent artwork downloads so they never starve API calls.
    pub art_semaphore: Arc<tokio::sync::Semaphore>,
    /// Monotonic sequence for live search; stale responses are dropped.
    pub search_seq: Arc<std::sync::atomic::AtomicU64>,
    pub player: player::Controller,
    pub player_start_pending: bool,
    pub player_paused_since: Option<Instant>,
    pub last_state_push: Option<std::time::Instant>,
    pub media_tx: std::sync::mpsc::Sender<crate::media::MediaUpdate>,
    pub last_media_push: Option<std::time::Instant>,
    pub device_id: String,
    pub last_device_poll: Option<std::time::Instant>,
    pub device_poll_in_flight: bool,
}

pub async fn run(
    mut terminal: DefaultTerminal,
    mut keymap: Keymap,
    startup_warning: Option<String>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    mut event_rx: mpsc::UnboundedReceiver<AppEvent>,
    media_tx: std::sync::mpsc::Sender<crate::media::MediaUpdate>,
) -> Result<()> {
    let device_id = crate::config::load_or_create_device_id();
    let mut state = AppState {
        status_message: startup_warning,
        ..AppState::default()
    };
    state.devices.device_id = device_id.clone();

    let player_events = event_tx.clone();
    let mut runtime = Runtime {
        event_tx,
        http: http_client(),
        api: None,
        sso: None,
        art_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        search_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        player: player::spawn(move |event| {
            let _ = player_events.send(AppEvent::Player(event));
        }),
        player_start_pending: false,
        player_paused_since: None,
        last_state_push: None,
        media_tx,
        last_media_push: None,
        device_id,
        last_device_poll: None,
        device_poll_in_flight: false,
    };

    match auth::load_session() {
        Some(session) => {
            state.user = Some(session.user.clone());
            let api = Arc::new(ApiClient::new(runtime.http.clone(), session));
            runtime.api = Some(Arc::clone(&api));
            spawn_session_check(&runtime, api);
        }
        None => state.screen = Screen::Login,
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
                if state.player.current.is_some()
                    && state.devices.is_playback_device()
                    && !runtime.player_start_pending
                {
                    state.player.position_secs = runtime.player.shared.position().as_secs_f64();
                    state.player.paused = runtime.player.shared.paused();
                } else if state.player.current.is_some()
                    && state.player.playing
                    && !state.player.paused
                {
                    state.player.position_secs += TICK_INTERVAL.as_secs_f64();
                    if let Some(track) = &state.player.current {
                        if track.duration_seconds > 0.0 {
                            state.player.position_secs =
                                state.player.position_secs.min(track.duration_seconds);
                        }
                    }
                }
                maybe_poll_devices(&state, &mut runtime);
                maybe_prefetch_next(&mut state, &runtime);
                maybe_push_state(&state, &mut runtime);
                push_media_update(&state, &mut runtime, false);
            }
        }

        if state.should_quit {
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
    let Some(api) = runtime.api.clone() else {
        return;
    };
    if state.screen != Screen::Main {
        return;
    }

    {
        let global = &mut state.global;
        // Keep at least a full screen plus a margin loaded, and stay ahead
        // of the cursor: a big terminal fills itself on startup without any
        // scrolling, page after page.
        let needed = artist_grid_capacity().max(global.selected + ARTISTS_PREFETCH_MARGIN)
            + ARTISTS_PREFETCH_MARGIN;
        if global.has_more
            && !global.loading
            && global.error.is_none()
            && global.artists.len() < needed
        {
            global.loading = true;
            let page = global.next_page;
            let limit = *global
                .page_limit
                .get_or_insert_with(|| (needed as i64).clamp(48, 200));
            let api = Arc::clone(&api);
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let event = match api.artists(page, limit).await {
                    Ok(page) => AppEvent::ArtistsLoaded(Ok(page)),
                    Err(ApiError::SessionExpired) => AppEvent::SessionExpired,
                    Err(err) => AppEvent::ArtistsLoaded(Err(err.to_string())),
                };
                let _ = tx.send(event);
            });
        }
    }

    // Liked ids load once per session — markers are shown everywhere.
    if !state.likes_loaded {
        state.likes_loaded = true;
        let api = Arc::clone(&api);
        let tx = runtime.event_tx.clone();
        tokio::spawn(async move {
            let event = match api.likes().await {
                Ok(ids) => AppEvent::LikesLoaded(Ok(ids)),
                Err(ApiError::SessionExpired) => AppEvent::SessionExpired,
                Err(err) => AppEvent::LikesLoaded(Err(err.to_string())),
            };
            let _ = tx.send(event);
        });
    }

    // Playlists tab data (also wanted while the add-to-playlist picker is
    // open from any tab).
    let picker_open = matches!(state.popup, Some(state::Popup::AddToPlaylist { .. }));
    if state.active_tab == state::Tab::Playlists || picker_open {
        if state.playlists.list.is_none() {
            state.playlists.list = Some(state::Loadable::Loading);
            let api = Arc::clone(&api);
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let event = match api.playlists().await {
                    Ok(list) => AppEvent::PlaylistsLoaded(Ok(list)),
                    Err(ApiError::SessionExpired) => AppEvent::SessionExpired,
                    Err(err) => AppEvent::PlaylistsLoaded(Err(err.to_string())),
                };
                let _ = tx.send(event);
            });
        }
        if let Some(opened) = state.playlists.opened {
            let id = opened.id;
            if let std::collections::hash_map::Entry::Vacant(entry) = state.playlist_views.entry(id)
            {
                entry.insert(state::Loadable::Loading);
                let api = Arc::clone(&api);
                let tx = runtime.event_tx.clone();
                tokio::spawn(async move {
                    let event = match api.playlist(id).await {
                        Ok(detail) => AppEvent::PlaylistViewLoaded {
                            id,
                            result: Ok(detail),
                        },
                        Err(ApiError::SessionExpired) => AppEvent::SessionExpired,
                        Err(err) => AppEvent::PlaylistViewLoaded {
                            id,
                            result: Err(err.to_string()),
                        },
                    };
                    let _ = tx.send(event);
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
                    let api = Arc::clone(&api);
                    let tx = runtime.event_tx.clone();
                    tokio::spawn(async move {
                        let event = match api.artist(id).await {
                            Ok(detail) => AppEvent::ArtistViewLoaded {
                                id,
                                result: Ok(detail),
                            },
                            Err(ApiError::SessionExpired) => AppEvent::SessionExpired,
                            Err(err) => AppEvent::ArtistViewLoaded {
                                id,
                                result: Err(err.to_string()),
                            },
                        };
                        let _ = tx.send(event);
                    });
                }
            }
            state::GlobalView::Release { id, .. } => {
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    state.release_views.entry(id)
                {
                    entry.insert(state::Loadable::Loading);
                    let api = Arc::clone(&api);
                    let tx = runtime.event_tx.clone();
                    tokio::spawn(async move {
                        let event = match api.release(id).await {
                            Ok(detail) => AppEvent::ReleaseViewLoaded {
                                id,
                                result: Ok(detail),
                            },
                            Err(ApiError::SessionExpired) => AppEvent::SessionExpired,
                            Err(err) => AppEvent::ReleaseViewLoaded {
                                id,
                                result: Err(err.to_string()),
                            },
                        };
                        let _ = tx.send(event);
                    });
                }
            }
            state::GlobalView::Search { .. } => {}
        }
    }

    // Artwork wanted by everything currently loaded, at its display size.
    let mut wanted: Vec<(String, u16, u16)> = Vec::new();
    let tile = (state::ART_CELL_WIDTH, state::ART_CELL_HEIGHT);
    let header = (state::ART_HEADER_WIDTH, state::ART_HEADER_HEIGHT);
    for artist in &state.global.artists {
        if let Some(url) = &artist.image_url {
            wanted.push((url.clone(), tile.0, tile.1));
        }
    }
    for detail in state.artist_views.values() {
        if let state::Loadable::Ready(detail) = detail {
            if let Some(url) = &detail.image_url {
                wanted.push((url.clone(), header.0, header.1));
            }
            for release in &detail.releases {
                if let Some(url) = &release.cover_url {
                    wanted.push((url.clone(), tile.0, tile.1));
                }
            }
        }
    }
    for detail in state.release_views.values() {
        if let state::Loadable::Ready(detail) = detail {
            if let Some(url) = &detail.cover_url {
                wanted.push((url.clone(), header.0, header.1));
            }
        }
    }
    for (url, width, height) in wanted {
        let key = crate::art::cache_key(&url, width, height);
        if state.art.contains_key(&key) {
            continue;
        }
        state.art.insert(key.clone(), state::ArtState::Loading);
        spawn_art_fetch(runtime, Arc::clone(&api), key, url, width, height);
    }
}

fn maybe_poll_devices(state: &AppState, runtime: &mut Runtime) {
    if state.screen != Screen::Main || runtime.device_poll_in_flight {
        return;
    }
    let Some(api) = runtime.api.clone() else {
        return;
    };
    let due = runtime
        .last_device_poll
        .is_none_or(|at| at.elapsed() >= DEVICE_POLL_INTERVAL);
    if !due {
        return;
    }

    runtime.last_device_poll = Some(std::time::Instant::now());
    runtime.device_poll_in_flight = true;
    let device_id = runtime.device_id.clone();
    let playback_state = state
        .devices
        .is_playback_device()
        .then(|| device_playback_state(state))
        .flatten();
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let event = match api.poll_device(&device_id, playback_state).await {
            Ok(response) => AppEvent::DevicesPolled(Ok(response)),
            Err(ApiError::SessionExpired) => AppEvent::SessionExpired,
            Err(err) => AppEvent::DevicesPolled(Err(err.to_string())),
        };
        let _ = tx.send(event);
    });
}

fn device_playback_state(state: &AppState) -> Option<crate::api::models::DevicePlaybackState> {
    let player = &state.player;
    let current = player.current.as_ref()?;
    if player.queue.is_empty() {
        return None;
    }
    Some(crate::api::models::DevicePlaybackState {
        track: serde_json::to_value(current).ok(),
        tracks: player
            .queue
            .iter()
            .filter_map(|track| serde_json::to_value(track).ok())
            .collect(),
        index: player.queue_pos as i32,
        position_seconds: player.position_secs,
        duration_seconds: current.duration_seconds,
        paused: player.paused || !player.playing,
        shuffle: player.shuffle,
        repeat_mode: player.repeat.label().to_string(),
        volume: f64::from(player.volume) / 100.0,
        updated_at_ms: 0,
    })
}

fn spawn_art_fetch(
    runtime: &Runtime,
    api: Arc<ApiClient>,
    key: String,
    url: String,
    width: u16,
    height: u16,
) {
    let tx = runtime.event_tx.clone();
    let semaphore = Arc::clone(&runtime.art_semaphore);
    tokio::spawn(async move {
        let Ok(_permit) = semaphore.acquire_owned().await else {
            return;
        };
        let art = match api.get_bytes(&url).await {
            Ok(bytes) => tokio::task::spawn_blocking(move || {
                crate::art::decode_to_cells(&bytes, width, height)
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
            .map_err(|err| tracing::warn!(%err, url, "artwork decode failed"))
            .ok()
            .map(Arc::new),
            Err(err) => {
                tracing::warn!(%err, url, "artwork fetch failed");
                None
            }
        };
        let _ = tx.send(AppEvent::ArtLoaded { key, art });
    });
}

/// Execute a side effect requested by update().
fn perform_effect(state: &mut AppState, runtime: &mut Runtime, effect: Effect) {
    if perform_remote_effect(state, runtime, &effect) {
        return;
    }
    match effect {
        Effect::PlayCurrent => {
            play_current(state, runtime);
            push_state_now(state, runtime);
            push_media_metadata(state, runtime);
            push_media_update(state, runtime, true);
        }
        Effect::TogglePause => {
            if state.player.paused {
                pause_current_audio(state, runtime);
            } else {
                resume_current_audio(state, runtime);
            }
            push_media_update(state, runtime, true);
        }
        Effect::StopPlayback => {
            runtime.player_start_pending = false;
            runtime.player_paused_since = None;
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
        Effect::SetOptions => {
            push_state_now(state, runtime);
        }
        Effect::EnqueueRelease { id, next } => {
            let Some(api) = runtime.api.clone() else {
                return;
            };
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                match api.release(id).await {
                    Ok(detail) => {
                        let _ = tx.send(AppEvent::EnqueueTracks {
                            tracks: detail.tracks,
                            next,
                        });
                    }
                    Err(err) => {
                        tracing::warn!(%err, release = id, "queueing a release failed");
                        let _ = tx.send(AppEvent::StatusMessage(format!("queue failed: {err}")));
                    }
                }
            });
        }
        Effect::ToggleLikes { track_ids } => {
            if track_ids.is_empty() {
                return;
            }
            let Some(api) = runtime.api.clone() else {
                return;
            };
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                for track_id in track_ids {
                    match api.toggle_like(track_id).await {
                        Ok(liked) => {
                            let _ = tx.send(AppEvent::LikeToggled { track_id, liked });
                        }
                        Err(err) => {
                            tracing::warn!(%err, track_id, "like toggle failed");
                            let _ = tx.send(AppEvent::StatusMessage(format!("like failed: {err}")));
                            break;
                        }
                    }
                }
            });
        }
        Effect::RemoveQueueIndices {
            restart_paused,
            stop,
            ..
        } => {
            if stop {
                runtime.player_start_pending = false;
                runtime.player_paused_since = None;
                runtime.player.stop();
                push_state_now(state, runtime);
                push_media_update(state, runtime, true);
            } else if let Some(paused) = restart_paused {
                start_current_audio(state, runtime, 0.0, paused);
                push_state_now(state, runtime);
                push_media_metadata(state, runtime);
                push_media_update(state, runtime, true);
            } else {
                push_state_now(state, runtime);
                push_media_update(state, runtime, true);
            }
        }
    }
}

fn perform_remote_effect(state: &mut AppState, runtime: &Runtime, effect: &Effect) -> bool {
    let Some(target) = state.devices.remote_target_id().map(str::to_string) else {
        return false;
    };
    match effect {
        Effect::PlayCurrent => {
            if let Some(payload) =
                device_playback_state(state).and_then(|state| serde_json::to_value(state).ok())
            {
                send_device_command(runtime, target, "play_from_index", payload);
                state.status_message = Some("sent play command to active device".into());
            }
            true
        }
        Effect::TogglePause => {
            let command = if state.player.paused {
                "pause"
            } else {
                "resume"
            };
            send_device_command(runtime, target, command, serde_json::json!({}));
            true
        }
        Effect::StopPlayback => {
            send_device_command(runtime, target, "queue_clear", serde_json::json!({}));
            true
        }
        Effect::SeekBy(delta) => {
            let target_time = (state.player.position_secs + *delta as f64).max(0.0);
            state.player.position_secs = target_time;
            send_device_command(
                runtime,
                target,
                "seek",
                serde_json::json!({ "time": target_time }),
            );
            true
        }
        Effect::SetVolume(volume) => {
            send_device_command(
                runtime,
                target,
                "set_volume",
                serde_json::json!({ "volume": f64::from(*volume) / 100.0 }),
            );
            true
        }
        Effect::SetOptions => {
            send_device_command(
                runtime,
                target,
                "set_options",
                serde_json::json!({
                    "shuffle": state.player.shuffle,
                    "repeat_mode": state.player.repeat.label(),
                }),
            );
            true
        }
        Effect::RemoveQueueIndices { indices, .. } => {
            let mut indices = indices.clone();
            indices.sort_unstable_by(|a, b| b.cmp(a));
            for index in indices {
                send_device_command(
                    runtime,
                    target.clone(),
                    "queue_remove",
                    serde_json::json!({ "index": index }),
                );
            }
            true
        }
        Effect::EnqueueRelease { .. } | Effect::ToggleLikes { .. } => false,
    }
}

fn send_device_command(
    runtime: &Runtime,
    target_device_id: String,
    command: &'static str,
    payload: serde_json::Value,
) {
    let Some(api) = runtime.api.clone() else {
        return;
    };
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let event = match api
            .send_device_command(Some(&target_device_id), command, &payload)
            .await
        {
            Ok(()) => AppEvent::StatusMessage(format!("sent {command} to active device")),
            Err(ApiError::SessionExpired) => AppEvent::SessionExpired,
            Err(err) => AppEvent::StatusMessage(format!("device command failed: {err}")),
        };
        let _ = tx.send(event);
    });
}

/// Start streaming `queue[queue_pos]`: open the authenticated HTTP stream in
/// a background task and hand the reader to the audio thread.
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
    let Some(api) = runtime.api.clone() else {
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
    state.player.track_started_at =
        same_track_started_at.or_else(|| Some(auth::now_epoch_seconds()));
    state.player.prefetched_pos = None;
    state.status_message = Some(format!("▶ {} — {}", track.title, track.artist_line()));
    if !paused {
        report_now_playing(runtime, track.id);
    }

    runtime.player_start_pending = true;
    if paused {
        runtime.player_paused_since = Some(Instant::now());
    } else {
        runtime.player_paused_since = None;
    }
    let controller = runtime.player.clone();
    let volume = player::amplitude(state.player.volume);
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        match api.open_stream(&track.stream_url).await {
            Ok((reader, byte_len)) => {
                controller.play(reader, byte_len, volume);
                if position_secs > 0.0 {
                    controller.seek(std::time::Duration::from_secs_f64(position_secs));
                }
                if paused {
                    controller.pause();
                }
            }
            Err(ApiError::SessionExpired) => {
                tracing::warn!(
                    track_id = track.id,
                    title = %track.title,
                    "playback stream open reported expired session"
                );
                let _ = tx.send(AppEvent::SessionExpired);
            }
            Err(err) => {
                let message = format!("playback failed: {err}");
                tracing::warn!(
                    track_id = track.id,
                    title = %track.title,
                    stream_url = %track.stream_url,
                    %err,
                    "playback stream open failed"
                );
                let _ = tx.send(AppEvent::Player(player::PlayerEvent::Failed(message)));
            }
        }
    });
}

fn pause_current_audio(state: &mut AppState, runtime: &mut Runtime) {
    state.player.paused = true;
    runtime.player.pause();
    runtime.player_paused_since.get_or_insert_with(Instant::now);
}

fn resume_current_audio(state: &mut AppState, runtime: &mut Runtime) {
    state.player.paused = false;
    let paused_for = runtime
        .player_paused_since
        .take()
        .map(|elapsed| elapsed.elapsed());
    if paused_for.is_some_and(|elapsed| elapsed >= STALE_STREAM_PAUSE_REOPEN_AFTER) {
        let position_secs = state.player.position_secs;
        tracing::info!(
            paused_for_seconds = paused_for.map_or(0.0, |elapsed| elapsed.as_secs_f64()),
            position_secs,
            "reopening playback stream after a long pause"
        );
        start_current_audio(state, runtime, position_secs, false);
    } else {
        runtime.player.resume();
    }
}

/// Start streaming the next queue item ~30s before the current track ends
/// and append it in the audio thread, so rodio switches sources without a
/// device gap.
fn maybe_prefetch_next(state: &mut AppState, runtime: &Runtime) {
    const PREFETCH_MARGIN_SECS: f64 = 30.0;
    if !state.devices.is_playback_device() {
        return;
    }
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
    let Some(api) = runtime.api.clone() else {
        return;
    };
    state.player.prefetched_pos = Some(next_pos);
    tracing::debug!(title = %next.title, "prefetching next track");
    let controller = runtime.player.clone();
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        match api.open_stream(&next.stream_url).await {
            Ok((reader, byte_len)) => controller.enqueue(reader, byte_len),
            Err(ApiError::SessionExpired) => {
                tracing::warn!(
                    track_id = next.id,
                    title = %next.title,
                    "prefetch reported expired session"
                );
                let _ = tx.send(AppEvent::SessionExpired);
            }
            Err(err) => {
                tracing::warn!(%err, "prefetch failed; falling back to a normal switch");
                let _ = tx.send(AppEvent::PrefetchFailed { pos: next_pos });
            }
        }
    });
}

/// Persist playback state server-side: on track changes (called directly)
/// and every ~10s while something is playing (called from the tick).
fn maybe_push_state(state: &AppState, runtime: &mut Runtime) {
    const PUSH_INTERVAL: Duration = Duration::from_secs(10);
    if !state.player.playing || !state.devices.is_playback_device() {
        return;
    }
    let due = runtime
        .last_state_push
        .is_none_or(|at| at.elapsed() >= PUSH_INTERVAL);
    if due {
        push_state_now(state, runtime);
    }
}

fn push_state_now(state: &AppState, runtime: &mut Runtime) {
    let Some(api) = runtime.api.clone() else {
        return;
    };
    runtime.last_state_push = Some(std::time::Instant::now());
    let player = &state.player;
    let body = crate::api::client::PlaybackStateBody {
        current_track_id: player.current.as_ref().map(|t| t.id),
        position_ms: (player.position_secs * 1000.0) as i32,
        queue: player.queue.iter().map(|t| t.id).collect(),
        queue_position: player.queue_pos as i32,
        shuffle: player.shuffle,
        repeat_mode: player.repeat.label().to_string(),
        volume: f64::from(player.volume) / 100.0,
    };
    tokio::spawn(async move {
        if let Err(err) = api.push_state(&body).await {
            tracing::warn!(%err, "state push failed");
        }
    });
}

/// Announce the just-started track as "now playing" on last.fm. Quiet on
/// failure — last.fm may simply not be connected for this account.
fn report_now_playing(runtime: &Runtime, track_id: i64) {
    let Some(api) = runtime.api.clone() else {
        return;
    };
    tokio::spawn(async move {
        if let Err(err) = api.lastfm_now_playing(track_id).await {
            tracing::debug!(%err, track_id, "lastfm now-playing failed");
        }
    });
}

/// Fire-and-forget history report; listens shorter than 5s are noise.
fn report_history(
    runtime: &Runtime,
    track_id: i64,
    started_at: Option<i64>,
    listened: i32,
    completed: bool,
) {
    if listened < 5 {
        return;
    }
    let Some(api) = runtime.api.clone() else {
        return;
    };
    tokio::spawn(async move {
        if let Err(err) = api
            .report_history(track_id, started_at, listened, completed)
            .await
        {
            tracing::warn!(%err, "history report failed");
        }
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

/// Validate the stored session in the background: a dead refresh token sends
/// the user back to the login screen instead of failing on first use.
fn spawn_session_check(runtime: &Runtime, api: Arc<ApiClient>) {
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        match api.me().await {
            Ok(me) => {
                let _ = tx.send(AppEvent::StatusMessage(format!("signed in as {}", me.name)));
            }
            Err(ApiError::SessionExpired) => {
                let _ = tx.send(AppEvent::SessionExpired);
            }
            Err(err) => {
                tracing::warn!(%err, "session check failed");
                let _ = tx.send(AppEvent::StatusMessage(format!(
                    "server unreachable: {err}"
                )));
            }
        }
    });
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
            match state.screen {
                Screen::Login => login::handle_key(state, runtime, key),
                Screen::Main if state.popup.is_some() => popup::handle_key(state, runtime, key),
                Screen::Main if state.cmdline.active => cmdline::handle_key(state, runtime, key),
                Screen::Main => handle_main_key(state, keymap, runtime, key),
            }
        }
        TermEvent::Paste(pasted) => match state.screen {
            Screen::Login => login::handle_paste(state, &pasted),
            Screen::Main if state.popup.is_some() => popup::handle_paste(state, &pasted),
            Screen::Main if state.cmdline.active => cmdline::handle_paste(state, runtime, &pasted),
            Screen::Main => {}
        },
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
            // Logout needs the Runtime, which pure update() never touches.
            if action == action::Action::Logout {
                perform_logout(state, runtime);
            } else if let Some(effect) = update(state, action) {
                perform_effect(state, runtime, effect);
            }
        }
        KeyResolution::Pending(keys) => state.pending_keys = Some(keys),
        KeyResolution::Unmatched => state.pending_keys = None,
    }
}

/// Sign out: revoke the session server-side (best effort, in the background),
/// delete stored credentials, return to the login screen with the server
/// URL kept for convenience.
fn perform_logout(state: &mut AppState, runtime: &mut Runtime) {
    let server_url = runtime.api.as_ref().map(|api| api.base_url().to_string());
    if let Some(api) = runtime.api.take() {
        tokio::spawn(async move {
            match api.logout().await {
                Ok(revoked) => tracing::info!(revoked, "logged out"),
                Err(err) => tracing::warn!(%err, "server-side logout failed"),
            }
        });
    }
    auth::delete_session();
    runtime.last_device_poll = None;
    runtime.device_poll_in_flight = false;
    runtime.player.stop();
    state.player = state::PlayerBar::default();
    state.user = None;
    state.login = state::LoginForm::default();
    if let Some(url) = server_url {
        state.login.server_url = url;
    }
    reset_library_state(state);
    state.screen = Screen::Login;
    state.status_message = None;
}

/// Drop everything fetched from the previous account/server.
fn reset_library_state(state: &mut AppState) {
    state.global = state::GlobalTab::default();
    state.artist_views.clear();
    state.release_views.clear();
    state.playlists = state::PlaylistsTab::default();
    state.playlist_views.clear();
    state.queue_tab = state::QueueTab::default();
    state.pending_release_focus = None;
    state.jump_origin = None;
    state.popup = None;
    let device_id = state.devices.device_id.clone();
    state.devices = state::DevicesState {
        device_id,
        ..state::DevicesState::default()
    };
    state.likes.clear();
    state.likes_loaded = false;
    state.search = state::SearchState::default();
    state.cmdline = state::Cmdline::default();
    state.art.clear();
}

fn apply_devices_response(
    state: &mut AppState,
    runtime: &mut Runtime,
    response: crate::api::models::DevicePollResponse,
    from_activation: bool,
) {
    let was_playback_device = state.devices.is_playback_device();
    state.devices.device_id = response.device_id;
    state.devices.active_device_id = response.active_device_id;
    state.devices.devices = response.devices;
    state.devices.poll_error = None;
    if from_activation {
        state.devices.switching_to = None;
        if matches!(state.popup, Some(state::Popup::Devices { .. })) {
            state.popup = None;
        }
    }

    let is_playback_device = state.devices.is_playback_device();
    if was_playback_device && !is_playback_device {
        runtime.player_start_pending = false;
        runtime.player_paused_since = None;
        runtime.player.stop();
    }

    if !is_playback_device {
        if let Some(playback_state) = &response.playback_state {
            apply_device_playback_state(state, runtime, playback_state, false);
        } else {
            runtime.player_start_pending = false;
            runtime.player_paused_since = None;
            runtime.player.stop();
        }
    } else if from_activation {
        if let Some(playback_state) = &response.playback_state {
            apply_device_playback_state(state, runtime, playback_state, true);
        }
    }

    for command in response.commands {
        execute_device_command(state, runtime, command);
    }
}

fn apply_device_playback_state(
    state: &mut AppState,
    runtime: &mut Runtime,
    playback_state: &crate::api::models::DevicePlaybackState,
    start_audio: bool,
) {
    let mut tracks = tracks_from_values(&playback_state.tracks);
    let track = playback_state
        .track
        .as_ref()
        .and_then(track_from_value)
        .or_else(|| {
            usize::try_from(playback_state.index)
                .ok()
                .and_then(|index| tracks.get(index).cloned())
        });
    if tracks.is_empty() {
        if let Some(track) = track.clone() {
            tracks.push(track);
        }
    }
    let mut index = usize::try_from(playback_state.index).unwrap_or(0);
    if let Some(track) = &track {
        index = tracks
            .iter()
            .position(|item| item.id == track.id)
            .unwrap_or(index);
    }
    if !tracks.is_empty() {
        index = index.min(tracks.len() - 1);
    } else {
        index = 0;
    }

    state.player.queue = tracks;
    state.player.queue_pos = index;
    state.player.current = track.or_else(|| state.player.queue.get(index).cloned());
    state.player.playing = state.player.current.is_some();
    state.player.paused = playback_state.paused;
    state.player.position_secs = playback_state.position_seconds.max(0.0);
    state.player.prefetched_pos = None;
    state.player.original_order = None;
    state.player.shuffle = playback_state.shuffle;
    state.player.repeat = repeat_from_label(&playback_state.repeat_mode);
    state.player.volume = volume_percent(playback_state.volume);
    state.queue_tab.cursor = state
        .queue_tab
        .cursor
        .min(state.player.queue.len().saturating_sub(1));

    if start_audio && state.player.current.is_some() {
        start_current_audio(
            state,
            runtime,
            playback_state.position_seconds,
            playback_state.paused,
        );
        push_media_metadata(state, runtime);
        push_media_update(state, runtime, true);
    } else {
        runtime.player_start_pending = false;
        runtime.player_paused_since = None;
        runtime.player.stop();
    }
}

fn track_from_value(value: &serde_json::Value) -> Option<crate::api::models::TrackItem> {
    serde_json::from_value(value.clone())
        .map_err(|err| tracing::warn!(%err, "invalid track in device payload"))
        .ok()
}

fn tracks_from_values(values: &[serde_json::Value]) -> Vec<crate::api::models::TrackItem> {
    values.iter().filter_map(track_from_value).collect()
}

fn repeat_from_label(label: &str) -> state::RepeatMode {
    match label {
        "one" => state::RepeatMode::One,
        "all" => state::RepeatMode::All,
        _ => state::RepeatMode::Off,
    }
}

fn volume_percent(volume: f64) -> u8 {
    (volume.clamp(0.0, 1.0) * 100.0).round() as u8
}

fn payload_playback_state(payload: &serde_json::Value) -> crate::api::models::DevicePlaybackState {
    serde_json::from_value(payload.clone()).unwrap_or_default()
}

fn payload_tracks(payload: &serde_json::Value) -> Vec<crate::api::models::TrackItem> {
    if let Some(values) = payload.get("tracks").and_then(serde_json::Value::as_array) {
        let tracks = tracks_from_values(values);
        if !tracks.is_empty() {
            return tracks;
        }
    }
    payload
        .get("track")
        .and_then(track_from_value)
        .into_iter()
        .collect()
}

fn payload_index(payload: &serde_json::Value, key: &str) -> Option<usize> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| usize::try_from(value).ok())
}

fn payload_f64(payload: &serde_json::Value, key: &str) -> Option<f64> {
    payload.get(key).and_then(serde_json::Value::as_f64)
}

fn execute_device_command(
    state: &mut AppState,
    runtime: &mut Runtime,
    command: crate::api::models::DeviceCommandDto,
) {
    let payload = command.payload;
    tracing::debug!(command = %command.command, id = ?command.id, "device command");
    match command.command.as_str() {
        "transfer_state" | "play_track" | "play_from_index" => {
            let playback_state = payload_playback_state(&payload);
            let start_audio = state.devices.is_playback_device();
            apply_device_playback_state(state, runtime, &playback_state, start_audio);
        }
        "pause" => {
            pause_current_audio(state, runtime);
            push_media_update(state, runtime, true);
        }
        "resume" | "play" => {
            resume_current_audio(state, runtime);
            push_media_update(state, runtime, true);
        }
        "seek" => {
            if let Some(time) =
                payload_f64(&payload, "time").or_else(|| payload_f64(&payload, "position_seconds"))
            {
                state.player.position_secs = time.max(0.0);
                runtime.player.seek(std::time::Duration::from_secs_f64(
                    state.player.position_secs,
                ));
            }
        }
        "next" => {
            apply_options_payload(state, &payload);
            if let Some(effect) = update::update(state, action::Action::NextTrack) {
                perform_effect(state, runtime, effect);
            }
        }
        "prev" | "previous" => {
            if let Some(effect) = update::update(state, action::Action::PrevTrack) {
                perform_effect(state, runtime, effect);
            }
        }
        "set_volume" | "volume" => {
            if let Some(volume) = payload_f64(&payload, "volume") {
                state.player.volume = volume_percent(volume);
                runtime
                    .player
                    .set_volume(player::amplitude(state.player.volume));
            }
        }
        "set_options" => apply_options_payload(state, &payload),
        "queue_add_end" => {
            update::enqueue_tracks(state, payload_tracks(&payload), false);
        }
        "queue_add_next" => {
            update::enqueue_tracks(state, payload_tracks(&payload), true);
        }
        "queue_remove" => {
            if let Some(index) = payload_index(&payload, "index") {
                remove_queue_index(state, runtime, index);
            }
        }
        "queue_move" => {
            if let (Some(from), Some(to)) = (
                payload_index(&payload, "from_index"),
                payload_index(&payload, "to_index"),
            ) {
                move_queue_index(state, from, to);
            }
        }
        "queue_clear" => {
            state.player = state::PlayerBar::default();
            state.queue_tab.cursor = 0;
            runtime.player.stop();
            push_media_update(state, runtime, true);
        }
        _ => {}
    }
}

fn apply_options_payload(state: &mut AppState, payload: &serde_json::Value) {
    if let Some(shuffle) = payload.get("shuffle").and_then(serde_json::Value::as_bool) {
        if shuffle != state.player.shuffle {
            state.player.shuffle = shuffle;
            if shuffle {
                update::shuffle_upcoming(&mut state.player);
            } else {
                update::restore_queue_order(&mut state.player);
            }
        }
    }
    if let Some(repeat) = payload
        .get("repeat_mode")
        .and_then(serde_json::Value::as_str)
    {
        state.player.repeat = repeat_from_label(repeat);
    }
}

fn remove_queue_index(state: &mut AppState, runtime: &mut Runtime, index: usize) {
    if index >= state.player.queue.len() {
        return;
    }
    let current_id = state.player.current.as_ref().map(|track| track.id);
    let removed_current = state
        .player
        .queue
        .get(index)
        .is_some_and(|track| Some(track.id) == current_id);
    let was_loaded = state.player.playing;
    let was_paused = state.player.paused;
    state.player.queue.remove(index);
    state.player.prefetched_pos = None;
    state.track_selection.clear();
    if state.player.queue.is_empty() {
        state.player = state::PlayerBar::default();
        runtime.player_start_pending = false;
        runtime.player_paused_since = None;
        runtime.player.stop();
        push_state_now(state, runtime);
        push_media_update(state, runtime, true);
        return;
    }
    state.player.queue_pos = current_id
        .and_then(|id| state.player.queue.iter().position(|track| track.id == id))
        .unwrap_or_else(|| state.player.queue_pos.min(state.player.queue.len() - 1));
    state.player.current = state.player.queue.get(state.player.queue_pos).cloned();
    state.queue_tab.cursor = state.queue_tab.cursor.min(state.player.queue.len() - 1);
    if removed_current && was_loaded {
        start_current_audio(state, runtime, 0.0, was_paused);
        push_media_metadata(state, runtime);
        push_media_update(state, runtime, true);
    }
    push_state_now(state, runtime);
}

fn move_queue_index(state: &mut AppState, from: usize, to: usize) {
    if from >= state.player.queue.len() || to >= state.player.queue.len() || from == to {
        return;
    }
    let current_id = state.player.current.as_ref().map(|track| track.id);
    let track = state.player.queue.remove(from);
    state.player.queue.insert(to, track);
    if let Some(id) = current_id {
        if let Some(position) = state.player.queue.iter().position(|track| track.id == id) {
            state.player.queue_pos = position;
        }
    }
    state.queue_tab.cursor = to;
    state.player.prefetched_pos = None;
}

fn handle_app_event(state: &mut AppState, runtime: &mut Runtime, event: AppEvent) {
    match event {
        AppEvent::StatusMessage(message) => state.status_message = Some(message),
        AppEvent::LoginSucceeded(session) => {
            if let Some(listener) = runtime.sso.take() {
                listener.abort();
            }
            state.status_message = Some(format!("signed in as {}", session.user.name));
            state.user = Some(session.user.clone());
            runtime.api = Some(Arc::new(ApiClient::new(runtime.http.clone(), *session)));
            runtime.last_device_poll = None;
            runtime.device_poll_in_flight = false;
            state.login = state::LoginForm::default();
            state.screen = Screen::Main;
        }
        AppEvent::LoginFailed(message) => {
            state.login.busy = false;
            state.login.error = Some(message);
        }
        AppEvent::SsoCallback(result) => {
            runtime.sso = None;
            if state.screen != Screen::Login
                || state.login.mode != state::LoginMode::SsoPending
                || state.login.busy
            {
                return;
            }
            match result {
                Ok(code) => login::spawn_sso_exchange(&mut state.login, runtime, code),
                Err(message) => state.login.error = Some(message),
            }
        }
        AppEvent::SessionExpired => {
            runtime.player_start_pending = false;
            runtime.player_paused_since = None;
            runtime.device_poll_in_flight = false;
            state.user = None;
            state.login = state::LoginForm::default();
            if let Some(api) = runtime.api.take() {
                state.login.server_url = api.base_url().to_string();
            }
            state.login.error = Some("session expired — sign in again".to_string());
            runtime.player.stop();
            state.player = state::PlayerBar::default();
            reset_library_state(state);
            state.screen = Screen::Login;
        }
        AppEvent::ArtistsLoaded(Ok(page)) => {
            let global = &mut state.global;
            global.loading = false;
            global.total = page.total;
            global.has_more = page.has_more;
            global.next_page = page.page + 1;
            global.artists.extend(page.items);
        }
        AppEvent::ArtistsLoaded(Err(message)) => {
            tracing::warn!(%message, "artists page load failed");
            state.global.loading = false;
            state.global.error = Some(message.clone());
            state.status_message = Some(message);
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
            if let Some((release_id, track_id)) = state.pending_release_focus {
                if release_id == id {
                    state.pending_release_focus = None;
                    if let Some(state::Loadable::Ready(detail)) = state.release_views.get(&id) {
                        let position = detail
                            .tracks
                            .iter()
                            .position(|t| t.id == track_id)
                            .unwrap_or(0);
                        if let Some(state::GlobalView::Release { id: top, cursor }) =
                            state.global.stack.last_mut()
                        {
                            if *top == release_id {
                                *cursor = position;
                            }
                        }
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
                state.player.track_started_at = Some(auth::now_epoch_seconds());
                if let Some(track) = &state.player.current {
                    report_now_playing(runtime, track.id);
                }
                push_media_metadata(state, runtime);
                push_media_update(state, runtime, true);
            } else {
                state.player.current = None;
                state.player.prefetched_pos = None;
                if let Some(effect) = update::advance_after_finish(state) {
                    perform_effect(state, runtime, effect);
                }
            }
            push_state_now(state, runtime);
        }
        AppEvent::Player(player::PlayerEvent::Failed(message)) => {
            runtime.player_start_pending = false;
            runtime.player_paused_since = None;
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
                Ok(list) => state::Loadable::Ready(list),
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
            state.status_message = Some(if liked {
                "♥ liked".to_string()
            } else {
                "like removed".to_string()
            });
        }
        AppEvent::DevicesPolled(result) => {
            runtime.device_poll_in_flight = false;
            match result {
                Ok(response) => apply_devices_response(state, runtime, response, false),
                Err(message) => {
                    tracing::warn!(%message, "device poll failed");
                    state.devices.poll_error = Some(message);
                }
            }
        }
        AppEvent::DeviceActivated(result) => match result {
            Ok(response) => apply_devices_response(state, runtime, response, true),
            Err(message) => {
                tracing::warn!(%message, "device activation failed");
                state.devices.switching_to = None;
                state.devices.poll_error = Some(message.clone());
                state.status_message = Some(format!("device switch failed: {message}"));
            }
        },
        AppEvent::EnqueueTracks { tracks, next } => {
            let count = tracks.len();
            if let Some(target) = state.devices.remote_target_id().map(str::to_string) {
                let payload = serde_json::json!({ "tracks": tracks });
                send_device_command(
                    runtime,
                    target,
                    if next {
                        "queue_add_next"
                    } else {
                        "queue_add_end"
                    },
                    payload,
                );
                state.status_message = Some(if next {
                    format!("{count} tracks queued next on active device")
                } else {
                    format!("{count} tracks queued on active device")
                });
                return;
            }
            update::enqueue_tracks(state, tracks, next);
            state.status_message = Some(if next {
                format!("{count} tracks queued next")
            } else {
                format!("{count} tracks queued")
            });
        }
        AppEvent::PlaylistCreated { result, add_track } => match result {
            Ok(playlist) => {
                tracing::info!(title = %playlist.title, "playlist created");
                state.status_message = Some(format!("playlist \"{}\" created", playlist.title));
                state.popup = None;
                // The list is stale; refetch when next needed.
                state.playlists.list = None;
                if let Some(track) = add_track {
                    let Some(api) = runtime.api.clone() else {
                        return;
                    };
                    let tx = runtime.event_tx.clone();
                    let (id, title) = (playlist.id, playlist.title.clone());
                    tokio::spawn(async move {
                        let result = api
                            .add_tracks_to_playlist(id, &[track.id])
                            .await
                            .map_err(|e| e.to_string());
                        let _ = tx.send(AppEvent::PlaylistTracksAdded {
                            playlist_id: id,
                            playlist_title: title,
                            result,
                        });
                    });
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
                    runtime.player_paused_since = None;
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
