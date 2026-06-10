pub mod action;
mod cmdline;
pub mod command;
pub mod event;
mod login;
mod sso;
pub mod state;
pub mod update;

use std::sync::Arc;
use std::time::Duration;

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
    pub last_state_push: Option<std::time::Instant>,
    pub media_tx: std::sync::mpsc::Sender<crate::media::MediaUpdate>,
    pub last_media_push: Option<std::time::Instant>,
}

pub async fn run(
    mut terminal: DefaultTerminal,
    mut keymap: Keymap,
    startup_warning: Option<String>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    mut event_rx: mpsc::UnboundedReceiver<AppEvent>,
    media_tx: std::sync::mpsc::Sender<crate::media::MediaUpdate>,
) -> Result<()> {
    let mut state = AppState {
        status_message: startup_warning,
        ..AppState::default()
    };

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
        last_state_push: None,
        media_tx,
        last_media_push: None,
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
                if state.player.current.is_some() {
                    state.player.position_secs = runtime.player.shared.position().as_secs_f64();
                    state.player.paused = runtime.player.shared.paused();
                }
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

const ARTISTS_PAGE_SIZE: i64 = 48;
const ARTISTS_PREFETCH_MARGIN: usize = 24;

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
        let initial = global.artists.is_empty();
        let near_end =
            !initial && global.selected + ARTISTS_PREFETCH_MARGIN >= global.artists.len();
        if global.has_more && !global.loading && global.error.is_none() && (initial || near_end) {
            global.loading = true;
            let page = global.next_page;
            let api = Arc::clone(&api);
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                let event = match api.artists(page, ARTISTS_PAGE_SIZE).await {
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

    // Playlists tab data.
    if state.active_tab == state::Tab::Playlists {
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
            if let std::collections::hash_map::Entry::Vacant(entry) =
                state.playlist_views.entry(id)
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
    match effect {
        Effect::PlayCurrent => {
            play_current(state, runtime);
            push_state_now(state, runtime);
            push_media_metadata(state, runtime);
            push_media_update(state, runtime, true);
        }
        Effect::TogglePause => {
            runtime.player.toggle_pause();
            push_media_update(state, runtime, true);
        }
        Effect::StopPlayback => {
            runtime.player.stop();
            push_media_update(state, runtime, true);
        }
        Effect::SeekBy(delta) => {
            let target = (state.player.position_secs + delta as f64).max(0.0);
            state.player.position_secs = target;
            runtime.player.seek(std::time::Duration::from_secs_f64(target));
        }
        Effect::SetVolume(volume) => runtime.player.set_volume(player::amplitude(volume)),
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
        Effect::ToggleLike { track_id } => {
            let Some(api) = runtime.api.clone() else {
                return;
            };
            let tx = runtime.event_tx.clone();
            tokio::spawn(async move {
                match api.toggle_like(track_id).await {
                    Ok(liked) => {
                        let _ = tx.send(AppEvent::LikeToggled { track_id, liked });
                    }
                    Err(err) => {
                        tracing::warn!(%err, track_id, "like toggle failed");
                        let _ = tx.send(AppEvent::StatusMessage(format!("like failed: {err}")));
                    }
                }
            });
        }
    }
}

/// Start streaming `queue[queue_pos]`: open the authenticated HTTP stream in
/// a background task and hand the reader to the audio thread.
fn play_current(state: &mut AppState, runtime: &Runtime) {
    let Some(track) = state.player.queue.get(state.player.queue_pos).cloned() else {
        return;
    };
    let Some(api) = runtime.api.clone() else {
        return;
    };
    // The track that was playing until now was cut short by this switch.
    if let Some(previous) = state.player.current.take() {
        if state.player.playing {
            report_history(
                runtime,
                previous.id,
                state.player.track_started_at,
                state.player.position_secs.round() as i32,
            );
        }
    }
    state.player.current = Some(track.clone());
    state.player.playing = true;
    state.player.paused = false;
    state.player.position_secs = 0.0;
    state.player.track_started_at = Some(auth::now_epoch_seconds());
    state.player.prefetched_pos = None;
    state.status_message = Some(format!("▶ {} — {}", track.title, track.artist_line()));

    let controller = runtime.player.clone();
    let volume = player::amplitude(state.player.volume);
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        match api.open_stream(&track.stream_url).await {
            Ok((reader, byte_len)) => controller.play(reader, byte_len, volume),
            Err(ApiError::SessionExpired) => {
                let _ = tx.send(AppEvent::SessionExpired);
            }
            Err(err) => {
                let _ = tx.send(AppEvent::StatusMessage(format!("playback failed: {err}")));
            }
        }
    });
}

/// Start streaming the next queue item ~30s before the current track ends
/// and append it in the audio thread, so rodio switches sources without a
/// device gap.
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
    if !state.player.playing {
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

/// Fire-and-forget history report; listens shorter than 5s are noise.
fn report_history(runtime: &Runtime, track_id: i64, started_at: Option<i64>, listened: i32) {
    if listened < 5 {
        return;
    }
    let Some(api) = runtime.api.clone() else {
        return;
    };
    tokio::spawn(async move {
        if let Err(err) = api.report_history(track_id, started_at, listened).await {
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
                let _ = tx.send(AppEvent::StatusMessage(format!("server unreachable: {err}")));
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
                Screen::Main if state.cmdline.active => cmdline::handle_key(state, runtime, key),
                Screen::Main => handle_main_key(state, keymap, runtime, key),
            }
        }
        TermEvent::Paste(pasted) => match state.screen {
            Screen::Login => login::handle_paste(state, &pasted),
            Screen::Main if state.cmdline.active => cmdline::handle_paste(state, runtime, &pasted),
            Screen::Main => {}
        },
        _ => {}
    }
}

fn handle_main_key(state: &mut AppState, keymap: &mut Keymap, runtime: &mut Runtime, key: KeyEvent) {
    let combo = KeyCombination::from(key);
    match keymap.resolve(combo, state.active_tab.key_context()) {
        KeyResolution::Action(action) => {
            state.pending_keys = None;
            tracing::debug!(?action, "key resolved");
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
    state.likes.clear();
    state.likes_loaded = false;
    state.search = state::SearchState::default();
    state.cmdline = state::Cmdline::default();
    state.art.clear();
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
        AppEvent::Player(player::PlayerEvent::TrackFinished { has_next }) => {
            // The finished track gets a full-duration history entry.
            if let Some(finished) = state.player.current.clone() {
                report_history(
                    runtime,
                    finished.id,
                    state.player.track_started_at,
                    finished.duration_seconds.round() as i32,
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
        AppEvent::EnqueueTracks { tracks, next } => {
            let count = tracks.len();
            update::enqueue_tracks(state, tracks, next);
            state.status_message = Some(if next {
                format!("{count} tracks queued next")
            } else {
                format!("{count} tracks queued")
            });
        }
        AppEvent::Media(command) => {
            use crate::media::MediaCommand;
            tracing::debug!(?command, "media key");
            let action = match command {
                MediaCommand::TogglePause | MediaCommand::Play | MediaCommand::Pause => {
                    action::Action::PlayPause
                }
                MediaCommand::Next => action::Action::NextTrack,
                MediaCommand::Previous => action::Action::PrevTrack,
                MediaCommand::Stop => {
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
