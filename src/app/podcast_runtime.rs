//! Runtime side of podcasts: opens the podcast store, runs store queries,
//! feed fetches and sync-log writes off the UI thread, schedules background
//! refreshes, starts episode streams and feeds the progress writer, and
//! hands results back as `AppEvent`s. Decisions about state live in
//! `update::podcasts`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use furumi_library::podcasts::{PodcastListFilter, PodcastStore};
use music_dht::podcast::{PodcastEpisodeRef, reaches_completion};

use super::event::AppEvent;
use super::state::{AppState, PodcastRow};
use super::update::{Effect, podcasts};
use super::{Runtime, err_string, perform_effect};
use crate::library::models::TrackItem;
use crate::podcasts::playback::{self, StartAt};
use crate::podcasts::progress::{
    CHECKPOINT_INTERVAL, ProgressCommand, ProgressEvent, ProgressWriter,
};
use crate::podcasts::refresh::{self, Services};

/// How often the scheduler asks the store which feeds are due. Each feed's
/// own due time (success interval or failure back-off) lives in the store.
const SCHEDULER_INTERVAL: Duration = Duration::from_secs(60);
/// Delay before the first scheduler pass, so startup work goes first.
const SCHEDULER_FIRST_DELAY: Duration = Duration::from_secs(10);
/// Feeds fetched at the same time.
const MAX_CONCURRENT_REFRESHES: usize = 4;
/// Feeds started per scheduler pass.
const MAX_DUE_PER_PASS: usize = 16;

/// Podcast services available once the store opened.
pub struct PodcastRuntime {
    pub services: Services,
    refresh_slots: Arc<tokio::sync::Semaphore>,
    /// Writes progress in order on its own thread; `None` if it could not
    /// start (episodes then play without saving progress).
    progress: Option<ProgressWriter>,
}

/// The episode this device is playing (or starting), as the progress
/// tracker sees it.
#[derive(Debug)]
pub struct EpisodePlayback {
    /// Player source generation of this start; identifies it everywhere.
    generation: u64,
    episode: PodcastEpisodeRef,
    /// Queue key of the episode (`ep:…`).
    key: String,
    /// The stream plays; until then nothing is written.
    started: bool,
    duration_ms: Option<i64>,
    /// Last position seen while this playback was current.
    position_ms: i64,
    /// Actual listening time (no pauses, no seeks).
    listened_ms: i64,
    last_tick: Instant,
    last_checkpoint: Instant,
    paused: bool,
    completed: bool,
}

fn now_ms() -> i64 {
    super::unix_time_ms()
}

/// Opens `podcasts.sqlite3` (migrating a prototype database in place) and
/// builds the HTTP client on the blocking pool. A failure only disables the
/// Podcasts tab; the store never modifies a file it refuses.
pub fn spawn_open(runtime: &Runtime) {
    let tx = runtime.event_tx.clone();
    let log: Arc<dyn crate::podcasts::OpLog> = runtime.devices.clone();
    tokio::task::spawn_blocking(move || {
        let result = open(log).map_err(|err| {
            let message = crate::podcasts::describe_open_error(&err);
            tracing::warn!("podcast storage unavailable: {message}");
            message
        });
        let _ = tx.send(AppEvent::PodcastStoreOpened(result));
    });
}

fn open(log: Arc<dyn crate::podcasts::OpLog>) -> anyhow::Result<Services> {
    crate::podcasts::open_services(
        &crate::podcasts::db_path()?,
        crate::podcasts::covers_dir()?,
        log,
    )
}

fn spawn_scheduler(tx: tokio::sync::mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let start = tokio::time::Instant::now() + SCHEDULER_FIRST_DELAY;
        let mut interval = tokio::time::interval_at(start, SCHEDULER_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if tx.send(AppEvent::PodcastRefreshTick).is_err() {
                return;
            }
        }
    });
}

fn services(runtime: &Runtime) -> Option<Services> {
    runtime
        .podcasts
        .as_ref()
        .map(|podcasts| podcasts.services.clone())
}

/// Executes a podcast [`Effect`]; any other effect is handed back.
pub fn perform(state: &mut AppState, runtime: &mut Runtime, effect: Effect) -> Option<Effect> {
    let Some(services) = services(runtime) else {
        if is_podcast_effect(&effect) {
            if let Effect::PodcastRefresh { targets, .. } = &effect {
                for (id, _) in targets {
                    state.podcasts.refreshing.remove(id);
                }
            }
            if matches!(effect, Effect::PodcastSubscribe(_)) {
                state.podcasts.subscribing = false;
            }
            state.status_message = Some("podcasts are unavailable (see the Podcasts tab)".into());
            return None;
        }
        return Some(effect);
    };
    let tx = runtime.event_tx.clone();
    match effect {
        Effect::PodcastsReload => {
            tokio::task::spawn_blocking(move || {
                let result = load_rows(&services.store).map_err(err_string);
                let _ = tx.send(AppEvent::PodcastsLoaded(result));
            });
        }
        Effect::PodcastLoadSeasons(podcast_id) => {
            tokio::task::spawn_blocking(move || {
                let result = services.store.seasons(&podcast_id).map_err(err_string);
                let _ = tx.send(AppEvent::PodcastSeasonsLoaded { podcast_id, result });
            });
        }
        Effect::PodcastLoadEpisodes { seq, query } => {
            tokio::task::spawn_blocking(move || {
                let offset = query.offset;
                let result = services.store.episodes(&query).map_err(err_string);
                let _ = tx.send(AppEvent::PodcastEpisodesLoaded {
                    seq,
                    offset,
                    result,
                });
            });
        }
        Effect::PodcastSubscribe(input) => {
            tokio::spawn(async move {
                let result = refresh::subscribe(&services, &input, now_ms())
                    .await
                    .map_err(err_string);
                let _ = tx.send(AppEvent::PodcastSubscribed(result));
            });
        }
        Effect::PodcastRefresh { targets, manual } => {
            let slots = runtime
                .podcasts
                .as_ref()
                .map(|podcasts| Arc::clone(&podcasts.refresh_slots))?;
            for (podcast_id, url) in targets {
                let services = services.clone();
                let slots = Arc::clone(&slots);
                let tx = tx.clone();
                tokio::spawn(async move {
                    let Ok(_permit) = slots.acquire_owned().await else {
                        return;
                    };
                    let result = refresh::refresh(&services, &podcast_id, &url, now_ms())
                        .await
                        .map_err(err_string);
                    let _ = tx.send(AppEvent::PodcastRefreshed {
                        podcast_id,
                        result,
                        manual,
                    });
                });
            }
        }
        Effect::PodcastUnsubscribe { podcast_id, title } => {
            tokio::task::spawn_blocking(move || {
                let store = Arc::clone(&services.store);
                let id = podcast_id.clone();
                let result = services
                    .log
                    .record(&mut |stamps| store.unsubscribe(stamps, &id))
                    .map_err(err_string);
                let _ = tx.send(AppEvent::PodcastUnsubscribed {
                    podcast_id,
                    title,
                    result,
                });
            });
        }
        Effect::PodcastSetPlayed { episode_id, played } => {
            // Through the progress writer, so it is ordered with the
            // checkpoints of an episode that is playing.
            match runtime
                .podcasts
                .as_ref()
                .and_then(|podcasts| podcasts.progress.as_ref())
            {
                Some(progress) => progress.send(ProgressCommand::SetPlayed { episode_id, played }),
                None => {
                    state.status_message =
                        Some("podcasts: progress cannot be saved (see the log)".into());
                }
            }
        }
        other => return Some(other),
    }
    None
}

fn is_podcast_effect(effect: &Effect) -> bool {
    matches!(
        effect,
        Effect::PodcastsReload
            | Effect::PodcastLoadSeasons(_)
            | Effect::PodcastLoadEpisodes { .. }
            | Effect::PodcastSubscribe(_)
            | Effect::PodcastRefresh { .. }
            | Effect::PodcastUnsubscribe { .. }
            | Effect::PodcastSetPlayed { .. }
    )
}

/// Subscribed podcasts with their local fetch state.
pub(crate) fn load_rows(store: &PodcastStore) -> anyhow::Result<Vec<PodcastRow>> {
    store
        .list_podcasts(PodcastListFilter::Subscribed)?
        .into_iter()
        .map(|summary| {
            let fetch = store.fetch_state(&summary.podcast_id)?;
            Ok(PodcastRow { summary, fetch })
        })
        .collect()
}

/// Folds a podcast `AppEvent` into the state; any other event is handed
/// back.
pub fn handle_event(
    state: &mut AppState,
    runtime: &mut Runtime,
    event: AppEvent,
) -> Option<AppEvent> {
    let effects: Vec<Effect> = match event {
        AppEvent::PodcastStoreOpened(result) => {
            let result = result.map(|services| {
                let tx = runtime.event_tx.clone();
                let progress = ProgressWriter::spawn(services.clone(), move |event| {
                    let _ = tx.send(AppEvent::PodcastProgress(event));
                })
                .inspect_err(|err| tracing::warn!("{err:#}"))
                .ok();
                runtime.podcasts = Some(PodcastRuntime {
                    services,
                    refresh_slots: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REFRESHES)),
                    progress,
                });
                spawn_scheduler(runtime.event_tx.clone());
            });
            podcasts::store_opened(state, result).into_iter().collect()
        }
        AppEvent::PodcastsLoaded(result) => {
            podcasts::list_loaded(state, result);
            Vec::new()
        }
        AppEvent::PodcastSeasonsLoaded { podcast_id, result } => {
            podcasts::seasons_loaded(state, podcast_id, result);
            Vec::new()
        }
        AppEvent::PodcastEpisodesLoaded {
            seq,
            offset,
            result,
        } => {
            podcasts::episodes_loaded(state, seq, offset, result);
            Vec::new()
        }
        AppEvent::PodcastSubscribed(result) => {
            podcasts::subscribed(state, result).into_iter().collect()
        }
        AppEvent::PodcastRefreshed {
            podcast_id,
            result,
            manual,
        } => podcasts::refreshed(state, podcast_id, result, manual),
        AppEvent::PodcastUnsubscribed {
            podcast_id,
            title,
            result,
        } => podcasts::unsubscribed(state, podcast_id, title, result)
            .into_iter()
            .collect(),
        AppEvent::PodcastProgress(event) => progress_event(state, runtime, event),
        AppEvent::PodcastEpisodeStarted {
            generation,
            episode,
            start_ms,
            duration_ms,
            restart,
        } => {
            episode_started(
                state,
                runtime,
                generation,
                *episode,
                start_ms,
                duration_ms,
                restart,
            );
            Vec::new()
        }
        AppEvent::PodcastEpisodeFailed {
            generation,
            message,
        } => {
            episode_failed(state, runtime, generation, message);
            Vec::new()
        }
        AppEvent::PodcastRefreshTick => {
            if let Some(services) = services(runtime) {
                let tx = runtime.event_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let result = services
                        .store
                        .podcasts_due_for_refresh(now_ms(), MAX_DUE_PER_PASS)
                        .map_err(err_string);
                    let _ = tx.send(AppEvent::PodcastsDue(result));
                });
            }
            Vec::new()
        }
        AppEvent::PodcastsDue(result) => match result {
            Ok(due) => podcasts::refresh_due(state, due).into_iter().collect(),
            Err(err) => {
                tracing::warn!(%err, "podcast scheduler query failed");
                Vec::new()
            }
        },
        other => return Some(other),
    };
    for effect in effects {
        perform_effect(state, runtime, effect);
    }
    None
}

/// Cover of the podcast under the cursor (or opened), for the art cache.
pub fn wanted_cover(state: &AppState) -> Option<String> {
    if state.active_tab != super::state::Tab::Podcasts {
        return None;
    }
    state.podcasts.current_podcast()?.fetch.cover_path.clone()
}

// ---------------------------------------------------------------------------
// Episode playback and progress

fn progress_writer(runtime: &Runtime) -> Option<&ProgressWriter> {
    runtime
        .podcasts
        .as_ref()
        .and_then(|podcasts| podcasts.progress.as_ref())
}

fn send_progress(runtime: &Runtime, command: ProgressCommand) {
    if let Some(progress) = progress_writer(runtime) {
        progress.send(command);
    }
}

/// Starts streaming `track` (a podcast episode) for the current queue item.
/// The caller already updated the player state; this silences the previous
/// audio, ends the previous episode's session and prepares the stream in
/// the background. The stream plays only if no other start or stop
/// happened meanwhile.
pub(crate) fn start_episode(
    state: &mut AppState,
    runtime: &mut Runtime,
    track: &TrackItem,
    start: StartAt,
    paused: bool,
) {
    let Some(queued) = track.podcast_episode_ref().cloned() else {
        return;
    };
    end_playback(runtime);
    runtime.player.stop();
    let generation = runtime.player.source_generation();
    runtime.podcast_playback = Some(EpisodePlayback {
        generation,
        episode: queued.clone(),
        key: super::state::track_key(track),
        started: false,
        duration_ms: queued.duration_ms,
        position_ms: 0,
        listened_ms: 0,
        last_tick: Instant::now(),
        last_checkpoint: Instant::now(),
        paused,
        completed: false,
    });
    if runtime.podcasts.is_none() {
        tracing::warn!("podcast store unavailable: this episode plays without saving progress");
    }
    state.status_message = Some(format!("podcast: connecting to \"{}\"…", track.title));
    let services = services(runtime);
    let controller = runtime.player.clone();
    let tx = runtime.event_tx.clone();
    let volume = crate::player::amplitude(state.player.volume);
    let restart = start == StartAt::Restart;
    tokio::spawn(async move {
        let prepared = playback::prepare(services, queued, start, now_ms()).await;
        // A newer start or stop happened meanwhile: neither play nor report.
        if controller.source_generation() != generation {
            return;
        }
        match prepared {
            Ok(prepared) => {
                let start = Duration::from_millis(prepared.start_ms.max(0) as u64);
                controller.play_source(prepared.source, volume, start, paused, generation);
                let _ = tx.send(AppEvent::PodcastEpisodeStarted {
                    generation,
                    episode: Box::new(prepared.episode),
                    start_ms: prepared.start_ms,
                    duration_ms: prepared.duration_ms,
                    restart,
                });
            }
            Err(err) => {
                let message = playback::describe_failure(&err);
                tracing::warn!("podcast episode cannot start: {message}");
                let _ = tx.send(AppEvent::PodcastEpisodeFailed {
                    generation,
                    message,
                });
            }
        }
    });
}

fn episode_started(
    state: &mut AppState,
    runtime: &mut Runtime,
    generation: u64,
    episode: PodcastEpisodeRef,
    start_ms: i64,
    duration_ms: Option<i64>,
    restart: bool,
) {
    let Some(playback) = runtime
        .podcast_playback
        .as_mut()
        .filter(|playback| playback.generation == generation)
    else {
        return;
    };
    playback.started = true;
    playback.position_ms = start_ms;
    playback.duration_ms = duration_ms;
    playback.last_tick = Instant::now();
    playback.last_checkpoint = Instant::now();
    playback.episode = episode.clone();
    state.player.position_secs = start_ms as f64 / 1_000.0;
    if let Some(current) = state.player.current.as_mut()
        && current.is_podcast()
        && current.duration_seconds <= 0.0
        && let Some(ms) = duration_ms
    {
        current.duration_seconds = ms as f64 / 1_000.0;
    }
    state.status_message = Some(if start_ms > 0 {
        format!(
            "▶ {} — continuing at {}",
            episode.episode_title,
            format_clock(start_ms)
        )
    } else {
        format!("▶ {} — {}", episode.episode_title, episode.podcast_title)
    });
    let session_id = runtime.devices.new_listen_id();
    send_progress(
        runtime,
        ProgressCommand::Begin {
            playback: generation,
            episode: Box::new(episode),
            session_id,
            position_ms: start_ms,
            restart,
            now_ms: now_ms(),
        },
    );
}

fn episode_failed(state: &mut AppState, runtime: &mut Runtime, generation: u64, message: String) {
    if runtime
        .podcast_playback
        .as_ref()
        .is_none_or(|playback| playback.generation != generation)
    {
        return;
    }
    runtime.podcast_playback = None;
    runtime.player_start_pending = false;
    state.player.playing = false;
    state.player.paused = false;
    state.status_message = Some(format!("podcast: {message}"));
}

fn format_clock(ms: i64) -> String {
    let secs = ms.max(0) / 1_000;
    let (hours, minutes, seconds) = (secs / 3_600, secs / 60 % 60, secs % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Ends the session of the episode that was playing, saving its last seen
/// position (never the state after a stop, which may already be reset).
pub(crate) fn end_playback(runtime: &mut Runtime) {
    let Some(playback) = runtime.podcast_playback.take() else {
        return;
    };
    if !playback.started {
        return;
    }
    send_progress(
        runtime,
        ProgressCommand::End {
            playback: playback.generation,
            position_ms: playback.position_ms,
            listened_ms: playback.listened_ms,
            now_ms: now_ms(),
        },
    );
}

/// Whether `state` still plays the tracked episode on this device.
fn still_current(state: &AppState, playback: &EpisodePlayback) -> bool {
    state.device_playback.is_audio_owner()
        && state.player.playing
        && state
            .player
            .current
            .as_ref()
            .is_some_and(|track| super::state::track_key(track) == playback.key)
}

/// Player tick (every 250 ms): follows the position, counts listening time,
/// saves on pause, periodically while playing, and completes the episode
/// near its end.
pub(crate) fn tick(state: &AppState, runtime: &mut Runtime) {
    let Some(playback) = runtime.podcast_playback.as_ref() else {
        return;
    };
    if !still_current(state, playback) {
        // Stopped, replaced or handed off by a path without its own hook:
        // the last seen position is what gets saved.
        end_playback(runtime);
        return;
    }
    if !playback.started || runtime.player_start_pending {
        return;
    }
    let position_ms = (state.player.position_secs * 1_000.0).round() as i64;
    let paused = state.player.paused;
    let now = Instant::now();
    let mut commands = Vec::new();
    let playback = runtime.podcast_playback.as_mut().expect("checked above");
    let elapsed_ms = now.duration_since(playback.last_tick).as_millis() as i64;
    if !paused && !playback.paused {
        let advanced = position_ms - playback.position_ms;
        // Pauses, seeks and stalls do not count as listening.
        if advanced >= 0 && advanced <= elapsed_ms + 1_000 {
            playback.listened_ms += advanced;
        }
    }
    playback.position_ms = position_ms;
    playback.last_tick = now;
    let generation = playback.generation;
    if paused != playback.paused {
        playback.paused = paused;
        if paused {
            commands.push(ProgressCommand::Pause {
                playback: generation,
                position_ms,
                listened_ms: playback.listened_ms,
                now_ms: now_ms(),
            });
        }
        playback.last_checkpoint = now;
    }
    if !paused && !playback.completed && reaches_completion(position_ms, playback.duration_ms) {
        playback.completed = true;
        commands.push(ProgressCommand::Complete {
            playback: generation,
            position_ms,
            listened_ms: playback.listened_ms,
            now_ms: now_ms(),
        });
    } else if !paused && now.duration_since(playback.last_checkpoint) >= CHECKPOINT_INTERVAL {
        playback.last_checkpoint = now;
        commands.push(ProgressCommand::Checkpoint {
            playback: generation,
            position_ms,
        });
    }
    for command in commands {
        send_progress(runtime, command);
    }
}

/// The user seeked the playing episode to `target_secs`: an explicit action.
pub(crate) fn seeked(state: &AppState, runtime: &mut Runtime, target_secs: f64) {
    let Some(playback) = runtime.podcast_playback.as_mut() else {
        return;
    };
    if !playback.started || !still_current(state, playback) {
        return;
    }
    let position_ms = (target_secs.max(0.0) * 1_000.0).round() as i64;
    playback.position_ms = position_ms;
    playback.last_tick = Instant::now();
    playback.last_checkpoint = Instant::now();
    playback.completed = reaches_completion(position_ms, playback.duration_ms);
    let command = ProgressCommand::Seek {
        playback: playback.generation,
        position_ms,
    };
    send_progress(runtime, command);
}

/// The stream of the current episode ended by itself: the end of the stream
/// always completes the episode.
pub(crate) fn finished(runtime: &mut Runtime) {
    let Some(playback) = runtime.podcast_playback.as_mut() else {
        return;
    };
    if !playback.started {
        return;
    }
    let position_ms = playback
        .duration_ms
        .unwrap_or(playback.position_ms)
        .max(playback.position_ms);
    playback.position_ms = position_ms;
    if !playback.completed {
        playback.completed = true;
        let command = ProgressCommand::Complete {
            playback: playback.generation,
            position_ms,
            listened_ms: playback.listened_ms,
            now_ms: now_ms(),
        };
        send_progress(runtime, command);
    }
    end_playback(runtime);
}

/// The audio thread failed while an episode played or started.
pub(crate) fn player_failed(runtime: &mut Runtime) {
    end_playback(runtime);
}

fn progress_event(
    state: &mut AppState,
    runtime: &mut Runtime,
    event: ProgressEvent,
) -> Vec<Effect> {
    match event {
        ProgressEvent::Saved(record) => podcasts::progress_saved(state, record),
        ProgressEvent::Superseded { current } => {
            if runtime
                .podcast_playback
                .as_ref()
                .is_some_and(|playback| playback.episode.episode_id == current.episode_id)
            {
                state.status_message = Some(
                    "podcast: this episode's progress changed (marked, or played on another \
                     device); its position is saved again after a seek or start over"
                        .into(),
                );
            }
            podcasts::progress_saved(state, current)
        }
        ProgressEvent::Ended { .. } => vec![Effect::PodcastsReload],
        ProgressEvent::PlayedSet { played, result } => podcasts::played_set(state, played, result),
        ProgressEvent::Failed(message) => {
            state.status_message = Some(message);
            Vec::new()
        }
    }
}

/// Before exit: end the playing episode's session and give the progress
/// writer a moment to save it.
pub(crate) async fn shutdown(runtime: &mut Runtime) {
    end_playback(runtime);
    let Some(mut progress) = runtime
        .podcasts
        .as_mut()
        .and_then(|podcasts| podcasts.progress.take())
    else {
        return;
    };
    let _ = tokio::task::spawn_blocking(move || progress.close(Duration::from_secs(3))).await;
}
