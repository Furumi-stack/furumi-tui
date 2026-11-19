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
use super::update::{Effect, podcast_discovery, podcasts};
use super::{Runtime, err_string, perform_effect};
use crate::library::models::TrackItem;
use crate::podcasts::discovery;
use crate::podcasts::playback::{self, StartAt};
use crate::podcasts::progress::{
    CHECKPOINT_INTERVAL, ProgressCommand, ProgressEvent, ProgressWriter,
};
use crate::podcasts::publication::{self, Reconciler, ShareConfirmations};
use crate::podcasts::refresh::{self, Services};
use crate::podcasts::stream::{CancelToken, StreamEnd, StreamProbe};

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
    /// Publishes shared podcasts to public discovery and revokes the rest.
    publication: Reconciler,
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
    /// `duration_ms` was measured by the decoder, not only announced by
    /// the feed (`PreparedEpisode::duration_measured`).
    duration_measured: bool,
    /// Last position seen while this playback was current.
    position_ms: i64,
    /// Actual listening time (no pauses, no seeks).
    listened_ms: i64,
    last_tick: Instant,
    last_checkpoint: Instant,
    paused: bool,
    completed: bool,
    /// The server allows seeking in the stream.
    seekable: bool,
    /// How the stream ended, once the episode started.
    probe: Option<StreamProbe>,
}

/// What the end of the current source meant.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Finished {
    /// Not an episode that was playing here.
    NotEpisode,
    /// The episode played to its end (completed).
    Completed,
    /// The stream stopped before the end (connection lost, URL expired):
    /// the last position is saved and the queue must not move on.
    Interrupted(String),
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

/// Starts the publication reconciler. Without a store every own public
/// card is revoked (see `publication::reconcile`).
fn spawn_publication(
    runtime: &Runtime,
    store: Option<Arc<PodcastStore>>,
    confirmations: Arc<ShareConfirmations>,
) -> Reconciler {
    let tx = runtime.event_tx.clone();
    publication::spawn(
        runtime.federation.podcast_dht(),
        store,
        confirmations,
        move |report| {
            let _ = tx.send(AppEvent::PodcastPublication(report));
        },
    )
}

/// Asks the publication reconciler for a pass (share decisions, feed
/// metadata or subscriptions changed).
fn wake_publication(runtime: &Runtime) {
    if let Some(podcasts) = &runtime.podcasts {
        podcasts.publication.wake();
    }
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
            if matches!(
                effect,
                Effect::PodcastSubscribe(_) | Effect::PodcastSubscribeFromCard(_)
            ) {
                state.podcasts.subscribing = false;
            }
            if let Effect::PodcastPreview { podcast_id, .. } = &effect {
                podcast_discovery::preview_loaded(
                    state,
                    podcast_id.clone(),
                    Err("podcasts are unavailable".into()),
                );
            }
            if let Effect::PodcastPrepareShare { podcast_id, .. }
            | Effect::PodcastSetShared { podcast_id, .. } = &effect
            {
                state.podcasts.share_busy.remove(podcast_id);
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
        Effect::PodcastSubscribeFromCard(hit) => {
            tokio::spawn(async move {
                let result = discovery::subscribe_from_hit(&services, &hit, now_ms())
                    .await
                    .map_err(err_string);
                let _ = tx.send(AppEvent::PodcastSubscribed(result));
            });
        }
        Effect::PodcastPreview {
            podcast_id,
            feed_url,
        } => {
            tokio::spawn(async move {
                let result = discovery::preview(&services.fetcher, &feed_url)
                    .await
                    .map(Box::new)
                    .map_err(err_string);
                let _ = tx.send(AppEvent::PodcastPreviewLoaded { podcast_id, result });
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
                // Unsubscribing also revoked the share decision: sharing
                // after subscribing again asks again.
                if result.is_ok()
                    && let Err(err) = services.confirmations.forget(&podcast_id)
                {
                    tracing::warn!("podcast share confirmation not removed: {err:#}");
                }
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
        Effect::PodcastPrepareShare { podcast_id, title } => {
            tokio::task::spawn_blocking(move || {
                let result =
                    publication::check_share(&services.store, &services.confirmations, &podcast_id)
                        .map_err(err_string);
                let _ = tx.send(AppEvent::PodcastSharePrepared {
                    podcast_id,
                    title,
                    result,
                });
            });
        }
        Effect::PodcastSetShared {
            podcast_id,
            title,
            shared,
            confirm,
        } => {
            tokio::task::spawn_blocking(move || {
                let result =
                    set_shared(&services, &podcast_id, shared, confirm).map_err(err_string);
                let _ = tx.send(AppEvent::PodcastShareSet {
                    podcast_id,
                    title,
                    shared,
                    result,
                });
            });
        }
        Effect::PodcastPublicationSync => wake_publication(runtime),
        other => return Some(other),
    }
    None
}

/// Writes a share decision. The local confirmation is recorded before the
/// synced decision, so the reconciler pass the decision triggers already
/// sees it; turning sharing off forgets it. Blocking.
fn set_shared(
    services: &Services,
    podcast_id: &music_dht::podcast::PodcastId,
    shared: bool,
    confirm: Option<publication::ConfirmedUrls>,
) -> anyhow::Result<()> {
    if let Some(urls) = confirm {
        services.confirmations.confirm(podcast_id, urls)?;
    }
    let store = Arc::clone(&services.store);
    services
        .log
        .record(&mut |stamps| store.set_shared(stamps, podcast_id, shared))?;
    if !shared {
        services.confirmations.forget(podcast_id)?;
    }
    Ok(())
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
            | Effect::PodcastPrepareShare { .. }
            | Effect::PodcastSetShared { .. }
            | Effect::PodcastPublicationSync
            | Effect::PodcastSubscribeFromCard(_)
            | Effect::PodcastPreview { .. }
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
            if result.is_err() {
                // This device can no longer tell what is shared: revoke its
                // own public cards (fail-safe for privacy).
                let _ = spawn_publication(runtime, None, Arc::new(ShareConfirmations::in_memory()));
            }
            let result = result.map(|services| {
                let tx = runtime.event_tx.clone();
                let progress = ProgressWriter::spawn(services.clone(), move |event| {
                    let _ = tx.send(AppEvent::PodcastProgress(event));
                })
                .inspect_err(|err| tracing::warn!("{err:#}"))
                .ok();
                // Device sync applies received podcast operations and
                // snapshot sections to the store from now on (and the ones
                // that arrived before it opened).
                let devices = Arc::clone(&runtime.devices);
                let store = Arc::clone(&services.store);
                tokio::task::spawn_blocking(move || {
                    if let Err(err) = devices.attach_podcast_store(store) {
                        tracing::warn!("podcast sync: {err:#}");
                    }
                });
                let publication = spawn_publication(
                    runtime,
                    Some(Arc::clone(&services.store)),
                    Arc::clone(&services.confirmations),
                );
                runtime.podcasts = Some(PodcastRuntime {
                    services,
                    refresh_slots: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REFRESHES)),
                    progress,
                    publication,
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
        } => {
            // The card of a shared podcast follows its feed metadata.
            if result.is_ok() {
                wake_publication(runtime);
            }
            podcasts::refreshed(state, podcast_id, result, manual)
        }
        AppEvent::PodcastUnsubscribed {
            podcast_id,
            title,
            result,
        } => {
            // Unsubscribing revokes this device's card.
            wake_publication(runtime);
            podcasts::unsubscribed(state, podcast_id, title, result)
                .into_iter()
                .collect()
        }
        AppEvent::PodcastSharePrepared {
            podcast_id,
            title,
            result,
        } => podcasts::share_prepared(state, podcast_id, title, result)
            .into_iter()
            .collect(),
        AppEvent::PodcastShareSet {
            podcast_id,
            title,
            shared,
            result,
        } => {
            wake_publication(runtime);
            podcasts::share_set(state, podcast_id, title, shared, result)
                .into_iter()
                .collect()
        }
        AppEvent::PodcastPreviewLoaded { podcast_id, result } => {
            podcast_discovery::preview_loaded(state, podcast_id, result);
            Vec::new()
        }
        AppEvent::PodcastPublication(report) => {
            podcasts::publication_reported(state, report);
            Vec::new()
        }
        AppEvent::PodcastProgress(event) => progress_event(state, runtime, event),
        AppEvent::PodcastsSynced { progress, changed } => {
            if changed {
                // Share or subscription decisions of another device: publish
                // or revoke this device's own cards accordingly.
                wake_publication(runtime);
                // A subscription that came from another device has no
                // episodes here until its feed is fetched: check now.
                let _ = runtime.event_tx.send(AppEvent::PodcastRefreshTick);
            }
            podcasts::synced(state, progress, changed)
        }
        AppEvent::PodcastEpisodeStarted {
            generation,
            episode,
            start_ms,
            duration_ms,
            duration_measured,
            restart,
            seekable,
            probe,
        } => {
            episode_started(
                state,
                runtime,
                generation,
                *episode,
                Started {
                    start_ms,
                    duration_ms,
                    duration_measured,
                    restart,
                    seekable,
                    probe,
                },
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
    // Pause and resume pressed while the stream connects update this.
    runtime.player.expect_source(paused);
    runtime.podcast_playback = Some(EpisodePlayback {
        generation,
        episode: queued.clone(),
        key: super::state::track_key(track),
        started: false,
        duration_ms: queued.duration_ms,
        duration_measured: false,
        position_ms: 0,
        listened_ms: 0,
        last_tick: Instant::now(),
        last_checkpoint: Instant::now(),
        paused,
        completed: false,
        seekable: false,
        probe: None,
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
    // Another start or stop cancels the connection, a resume still reading
    // up to its offset, and later reads of this stream.
    let cancel = CancelToken::new(controller.superseded_check(generation));
    tokio::spawn(async move {
        let prepared = playback::prepare(services, queued, start, now_ms(), cancel).await;
        // A newer start or stop happened meanwhile: neither play nor report.
        if controller.source_generation() != generation {
            return;
        }
        match prepared {
            Ok(prepared) => {
                let start = Duration::from_millis(prepared.start_ms.max(0) as u64);
                controller.play_source(prepared.source, volume, start, generation);
                let _ = tx.send(AppEvent::PodcastEpisodeStarted {
                    generation,
                    episode: Box::new(prepared.episode),
                    start_ms: prepared.start_ms,
                    duration_ms: prepared.duration_ms,
                    duration_measured: prepared.duration_measured,
                    restart,
                    seekable: prepared.seekable,
                    probe: prepared.probe,
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

/// What [`AppEvent::PodcastEpisodeStarted`] reports about the stream.
struct Started {
    start_ms: i64,
    duration_ms: Option<i64>,
    duration_measured: bool,
    restart: bool,
    seekable: bool,
    probe: StreamProbe,
}

fn episode_started(
    state: &mut AppState,
    runtime: &mut Runtime,
    generation: u64,
    episode: PodcastEpisodeRef,
    started: Started,
) {
    let Started {
        start_ms,
        duration_ms,
        duration_measured,
        restart,
        seekable,
        probe,
    } = started;
    let Some(playback) = runtime
        .podcast_playback
        .as_mut()
        .filter(|playback| playback.generation == generation)
    else {
        return;
    };
    playback.started = true;
    playback.seekable = seekable;
    playback.probe = Some(probe);
    playback.position_ms = start_ms;
    playback.duration_ms = duration_ms;
    playback.duration_measured = duration_measured;
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
    stop_on_episode(state);
    state.status_message = Some(format!("podcast: {message}"));
}

/// Leaves the queue on the current episode, stopped: Play starts it again
/// (from its saved position).
fn stop_on_episode(state: &mut AppState) {
    state.player.playing = false;
    state.player.paused = false;
    state.player.current = None;
    state.player.listen_id = None;
    state.player.prefetched_pos = None;
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

impl EpisodePlayback {
    /// Why this episode cannot be seeked now, if it cannot.
    fn seek_refusal(&self) -> Option<&'static str> {
        if !self.started {
            Some("podcast: the episode is still connecting; seek once it plays")
        } else if !self.seekable {
            Some("podcast: the publisher's server does not allow seeking in this episode")
        } else {
            None
        }
    }

    /// The stream ended by itself (`end`). Completes the episode only if it
    /// was played to the end (`playback::played_to_the_end`); otherwise the
    /// last seen position stays what is saved. Returns the commands to send
    /// and whether it was played to the end.
    fn finish(&mut self, end: StreamEnd, now_ms: i64) -> (Vec<ProgressCommand>, bool) {
        if !playback::played_to_the_end(
            end,
            self.position_ms,
            self.duration_ms,
            self.duration_measured,
        ) {
            return (Vec::new(), false);
        }
        let position_ms = self
            .duration_ms
            .unwrap_or(self.position_ms)
            .max(self.position_ms);
        self.position_ms = position_ms;
        let mut commands = Vec::new();
        if !self.completed {
            self.completed = true;
            commands.push(ProgressCommand::Complete {
                playback: self.generation,
                position_ms,
                listened_ms: self.listened_ms,
                now_ms,
            });
        }
        (commands, true)
    }

    /// Folds one player observation in and returns what to save: counts
    /// actual listening time (not pauses, seeks or stalls), saves on pause,
    /// every [`CHECKPOINT_INTERVAL`] while playing, and completes the
    /// episode once the position reaches the shared completion criterion.
    fn observe(
        &mut self,
        position_ms: i64,
        paused: bool,
        now: Instant,
        now_ms: i64,
    ) -> Vec<ProgressCommand> {
        let mut commands = Vec::new();
        let elapsed_ms = now.saturating_duration_since(self.last_tick).as_millis() as i64;
        if !paused && !self.paused {
            let advanced = position_ms - self.position_ms;
            if advanced >= 0 && advanced <= elapsed_ms + 1_000 {
                self.listened_ms += advanced;
            }
        }
        self.position_ms = position_ms;
        self.last_tick = now;
        if paused != self.paused {
            self.paused = paused;
            if paused {
                commands.push(ProgressCommand::Pause {
                    playback: self.generation,
                    position_ms,
                    listened_ms: self.listened_ms,
                    now_ms,
                });
            }
            self.last_checkpoint = now;
        }
        if paused {
            return commands;
        }
        if !self.completed && reaches_completion(position_ms, self.duration_ms) {
            self.completed = true;
            commands.push(ProgressCommand::Complete {
                playback: self.generation,
                position_ms,
                listened_ms: self.listened_ms,
                now_ms,
            });
        } else if now.saturating_duration_since(self.last_checkpoint) >= CHECKPOINT_INTERVAL {
            self.last_checkpoint = now;
            commands.push(ProgressCommand::Checkpoint {
                playback: self.generation,
                position_ms,
            });
        }
        commands
    }
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
    let commands = runtime
        .podcast_playback
        .as_mut()
        .expect("checked above")
        .observe(position_ms, paused, Instant::now(), now_ms());
    for command in commands {
        send_progress(runtime, command);
    }
}

/// Why the current episode cannot be seeked now, if it cannot: it is still
/// connecting, or its server does not allow seeking. Callers skip the seek
/// and show the reason.
pub(crate) fn seek_refusal(state: &AppState, runtime: &Runtime) -> Option<String> {
    let playback = runtime.podcast_playback.as_ref()?;
    if !still_current(state, playback) {
        return None;
    }
    playback.seek_refusal().map(str::to_string)
}

/// The audio thread handled a seek of the source started under
/// `generation`. Only a seek that moved the episode is saved, as an
/// explicit action at the position actually reached.
pub(crate) fn seek_done(
    state: &mut AppState,
    runtime: &mut Runtime,
    generation: u64,
    position: Duration,
    result: Result<(), String>,
) {
    if runtime
        .podcast_playback
        .as_ref()
        .is_none_or(|playback| playback.generation != generation)
    {
        return;
    }
    match result {
        Ok(()) => seeked(state, runtime, position.as_secs_f64()),
        Err(err) => {
            state.status_message = Some(format!("podcast: cannot seek in this episode ({err})"));
        }
    }
}

/// The playing episode moved to `target_secs` by the user: an explicit action.
fn seeked(state: &AppState, runtime: &mut Runtime, target_secs: f64) {
    let Some(playback) = runtime.podcast_playback.as_mut() else {
        return;
    };
    if !playback.started || !still_current(state, playback) {
        return;
    }
    let mut position_ms = (target_secs.max(0.0) * 1_000.0).round() as i64;
    // rodio stops a seek at the end of a source of known length.
    if let Some(duration_ms) = playback.duration_ms {
        position_ms = position_ms.min(duration_ms);
    }
    playback.position_ms = position_ms;
    playback.last_tick = Instant::now();
    playback.last_checkpoint = Instant::now();
    // The next tick completes it again if the seek went into the last
    // seconds; a seek back makes it "in progress" (the writer agrees).
    playback.completed = false;
    let command = ProgressCommand::Seek {
        playback: playback.generation,
        position_ms,
    };
    send_progress(runtime, command);
}

/// The source of the current episode ended by itself. Completes the
/// episode when its stream reached the end; a stream that broke off (lost
/// connection, expired URL) keeps the last position, and the caller must
/// stop on the episode instead of moving on.
pub(crate) fn finished(state: &mut AppState, runtime: &mut Runtime) -> Finished {
    let Some(playback) = runtime.podcast_playback.as_mut() else {
        return Finished::NotEpisode;
    };
    if !playback.started {
        return Finished::NotEpisode;
    }
    let end = playback
        .probe
        .as_ref()
        .map_or(StreamEnd::Complete, StreamProbe::end);
    let (commands, reached) = playback.finish(end, now_ms());
    let position_ms = playback.position_ms;
    let seekable = playback.seekable;
    for command in commands {
        send_progress(runtime, command);
    }
    end_playback(runtime);
    if reached {
        return Finished::Completed;
    }
    tracing::warn!(?end, position_ms, "podcast stream stopped before the end");
    runtime.player_start_pending = false;
    stop_on_episode(state);
    Finished::Interrupted(interrupted_message(position_ms, seekable))
}

/// Status for an episode whose stream stopped before its end. Without
/// seeking (no file size from the server) Play cannot continue there, so
/// the way out is to start over or mark it played.
fn interrupted_message(position_ms: i64, seekable: bool) -> String {
    let at = format_clock(position_ms);
    if seekable {
        format!(
            "podcast: the audio stopped at {at} (the publisher's server stopped sending it); \
             the position is saved, press play to continue"
        )
    } else {
        format!(
            "podcast: the audio stopped at {at} (the publisher's server stopped sending it); \
             the position is saved, but this server does not allow continuing there: start it \
             over (b) or mark it played (m) in Podcasts"
        )
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked(duration_ms: Option<i64>) -> (EpisodePlayback, Instant) {
        let track = crate::app::podcast_fixtures::episode_track(1);
        let episode = track.podcast_episode_ref().unwrap().clone();
        let start = Instant::now();
        let playback = EpisodePlayback {
            generation: 7,
            key: super::super::state::track_key(&track),
            episode,
            started: true,
            duration_ms,
            duration_measured: duration_ms.is_some(),
            position_ms: 0,
            listened_ms: 0,
            last_tick: start,
            last_checkpoint: start,
            paused: false,
            completed: false,
            seekable: true,
            probe: None,
        };
        (playback, start)
    }

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn listening_time_skips_jumps_and_pauses_and_checkpoints_every_interval() {
        let (mut playback, start) = tracked(Some(3_600_000));
        let mut commands = Vec::new();
        // Ten seconds of ordinary playback, observed every 250 ms.
        for step in 1..=40u64 {
            commands.extend(playback.observe(step as i64 * 250, false, at(start, step * 250), 0));
        }
        assert!(commands.is_empty(), "{commands:?}");
        assert_eq!(playback.listened_ms, 10_000);
        // A forward jump (a seek) is not listening time.
        playback.observe(600_000, false, at(start, 10_250), 0);
        assert_eq!(playback.listened_ms, 10_000);
        // Pausing saves once; time while paused is not counted.
        let paused = playback.observe(600_000, true, at(start, 10_500), 1);
        assert!(matches!(
            paused.as_slice(),
            [ProgressCommand::Pause {
                playback: 7,
                position_ms: 600_000,
                listened_ms: 10_000,
                now_ms: 1,
            }]
        ));
        assert!(
            playback
                .observe(600_000, true, at(start, 60_000), 2)
                .is_empty()
        );
        assert!(
            playback
                .observe(600_000, false, at(start, 60_250), 3)
                .is_empty()
        );
        // The next periodic save comes one interval after the resume.
        let interval = CHECKPOINT_INTERVAL.as_millis() as u64;
        let mut saved = Vec::new();
        let mut position = 600_000;
        for step in 1..=(interval / 250) {
            position += 250;
            saved.extend(playback.observe(position, false, at(start, 60_250 + step * 250), 4));
        }
        assert!(matches!(
            saved.as_slice(),
            [ProgressCommand::Checkpoint { playback: 7, position_ms }] if *position_ms == position
        ));
        assert_eq!(playback.listened_ms, 10_000 + interval as i64);
    }

    #[test]
    fn a_stream_that_broke_off_keeps_the_episode_in_progress() {
        let (mut playback, start) = tracked(Some(3_600_000));
        playback.observe(600_000, false, at(start, 250), 0);
        // Connection lost far from the end: nothing is completed; the
        // caller ends the session at the last seen position.
        let (commands, reached) = playback.finish(StreamEnd::Failed, 1);
        assert!(commands.is_empty() && !reached);
        assert_eq!(playback.position_ms, 600_000);
        assert!(!playback.completed);
        // Same for a stream without a size that closed early.
        let (commands, reached) = playback.finish(StreamEnd::Unknown, 1);
        assert!(commands.is_empty() && !reached);
        // A failure within the completion window, or a real end, completes.
        playback.observe(3_590_000, false, at(start, 500), 0);
        playback.completed = false;
        let (commands, reached) = playback.finish(StreamEnd::Failed, 2);
        assert!(reached);
        assert!(matches!(
            commands.as_slice(),
            [ProgressCommand::Complete {
                position_ms: 3_600_000,
                ..
            }]
        ));
        let (mut ended, start) = tracked(Some(3_600_000));
        ended.observe(1_000, false, at(start, 250), 0);
        let (commands, reached) = ended.finish(StreamEnd::Complete, 3);
        assert!(reached);
        assert_eq!(commands.len(), 1);
        // Without a size or a duration, the end of the stream is all there is.
        let (mut unknown, start) = tracked(None);
        unknown.observe(1_000, false, at(start, 250), 0);
        assert!(unknown.finish(StreamEnd::Unknown, 4).1);
    }

    /// A stream without a size whose decoder could not measure it (CBR MP3
    /// without a Xing header, say) only has the feed's `itunes:duration`,
    /// which may be longer than the audio by more than the completion
    /// window. Played to its real end, it completes and the queue moves on.
    #[test]
    fn a_stream_shorter_than_its_feed_duration_completes_at_its_end() {
        // The feed says 60 min; the audio ends at 55 min (more than the
        // 30 s completion window earlier).
        let (mut feed_only, start) = tracked(Some(3_600_000));
        feed_only.duration_measured = false;
        feed_only.observe(3_300_000, false, at(start, 250), 0);
        let (commands, reached) = feed_only.finish(StreamEnd::Unknown, 1);
        assert!(reached, "the clean close is the end");
        assert!(matches!(
            commands.as_slice(),
            [ProgressCommand::Complete {
                position_ms: 3_600_000,
                ..
            }]
        ));
        // An early close (a fifth of the announced length) is still not.
        let (mut early, start) = tracked(Some(3_600_000));
        early.duration_measured = false;
        early.observe(600_000, false, at(start, 250), 0);
        assert!(!early.finish(StreamEnd::Unknown, 2).1);
        // A duration the decoder measured keeps the strict window.
        let (mut measured, start) = tracked(Some(3_600_000));
        measured.observe(3_300_000, false, at(start, 250), 0);
        assert!(!measured.finish(StreamEnd::Unknown, 3).1);
        // A failed read never counts outside the window.
        let (mut failed, start) = tracked(Some(3_600_000));
        failed.duration_measured = false;
        failed.observe(3_300_000, false, at(start, 250), 0);
        assert!(!failed.finish(StreamEnd::Failed, 4).1);
    }

    #[test]
    fn an_interrupted_stream_without_seeking_says_how_to_move_on() {
        let message = interrupted_message(65_000, true);
        assert!(
            message.contains("1:05") && message.contains("press play"),
            "{message}"
        );
        let message = interrupted_message(65_000, false);
        assert!(!message.contains("press play"), "{message}");
        assert!(message.contains("start it over (b)"), "{message}");
        assert!(message.contains("mark it played (m)"), "{message}");
    }

    #[test]
    fn seeks_are_refused_while_connecting_and_without_range_support() {
        let (mut playback, _) = tracked(Some(3_600_000));
        assert_eq!(playback.seek_refusal(), None);
        playback.seekable = false;
        assert!(
            playback
                .seek_refusal()
                .is_some_and(|reason| reason.contains("does not allow seeking"))
        );
        playback.started = false;
        assert!(
            playback
                .seek_refusal()
                .is_some_and(|reason| reason.contains("still connecting"))
        );
    }

    #[test]
    fn the_end_of_an_episode_completes_it_once() {
        let (mut playback, start) = tracked(Some(2_700_000));
        assert!(
            playback
                .observe(2_669_000, false, at(start, 250), 0)
                .is_empty()
        );
        let completed = playback.observe(2_670_500, false, at(start, 500), 0);
        assert!(matches!(
            completed.as_slice(),
            [ProgressCommand::Complete {
                position_ms: 2_670_500,
                ..
            }]
        ));
        assert!(
            playback
                .observe(2_671_000, false, at(start, 750), 0)
                .is_empty()
        );
        // Without a known duration only the end of the stream completes.
        let (mut unknown, start) = tracked(None);
        assert!(
            unknown
                .observe(10_000_000, false, at(start, 250), 0)
                .is_empty()
        );
    }
}
