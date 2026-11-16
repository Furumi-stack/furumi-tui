//! Runtime side of the Podcasts tab: opening the podcast store, running feed
//! fetches and SQLite work off the UI thread, and folding their results back
//! into `AppState`. Only RSS metadata is persisted; episode audio is streamed
//! on demand by `start_stream`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::event::AppEvent;
use super::state::{AppState, Loadable, OpenedPodcast};
use super::update::PODCAST_STALE_SECS;
use super::{Runtime, err_string, now_epoch_seconds};

/// How often the background sweep looks for stale feeds. Each feed is only
/// fetched when it was last checked more than `PODCAST_STALE_SECS` ago.
const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Delay before the first sweep, so startup work is not slowed down.
const AUTO_REFRESH_FIRST_DELAY: Duration = Duration::from_secs(20);
/// Feeds fetched at the same time; protects slow links and podcast hosts.
const MAX_CONCURRENT_REFRESHES: usize = 4;

/// Store and HTTP client shared by podcast background tasks.
pub struct PodcastRuntime {
    pub store: Arc<crate::podcasts::Store>,
    pub fetcher: Arc<crate::podcasts::Fetcher>,
    refresh_slots: Arc<tokio::sync::Semaphore>,
}

/// Open the podcast store beside the library data and the cover cache in
/// the platform cache directory. Failure disables only the Podcasts tab.
pub async fn open() -> Result<PodcastRuntime, String> {
    let dirs = crate::config::project_dirs()
        .ok_or_else(|| "no home directory for podcast data".to_string())?;
    let db_path = dirs.data_dir().join("podcasts.sqlite3");
    let covers_dir = dirs.cache_dir().join("podcasts").join("covers");
    let store = tokio::task::spawn_blocking(move || crate::podcasts::Store::open(&db_path))
        .await
        .map_err(|err| err.to_string())?
        .map_err(err_string)?;
    let fetcher = crate::podcasts::Fetcher::new(covers_dir).map_err(err_string)?;
    Ok(PodcastRuntime {
        store: Arc::new(store),
        fetcher: Arc::new(fetcher),
        refresh_slots: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REFRESHES)),
    })
}

/// Ticks `PodcastAutoRefreshDue`; the handler decides which feeds are stale.
pub fn spawn_auto_refresh_timer(tx: tokio::sync::mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let start = tokio::time::Instant::now() + AUTO_REFRESH_FIRST_DELAY;
        let mut interval = tokio::time::interval_at(start, AUTO_REFRESH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if tx.send(AppEvent::PodcastAutoRefreshDue).is_err() {
                return;
            }
        }
    });
}

fn unavailable(state: &mut AppState) {
    state.status_message = Some("podcasts are unavailable (see the Podcasts tab)".into());
}

pub fn load_list(runtime: &Runtime) {
    let Some(podcasts) = &runtime.podcasts else {
        return;
    };
    let store = Arc::clone(&podcasts.store);
    let tx = runtime.event_tx.clone();
    tokio::task::spawn_blocking(move || {
        let result = store.list_podcasts().map_err(err_string);
        let _ = tx.send(AppEvent::PodcastsLoaded(result));
    });
}

fn load_episodes(runtime: &Runtime, podcast_id: i64) {
    let Some(podcasts) = &runtime.podcasts else {
        return;
    };
    let store = Arc::clone(&podcasts.store);
    let tx = runtime.event_tx.clone();
    tokio::task::spawn_blocking(move || {
        let result = store.episodes(podcast_id).map_err(err_string);
        let _ = tx.send(AppEvent::PodcastEpisodesLoaded { podcast_id, result });
    });
}

/// Effect::PodcastLoadEpisodes: read the stored catalog, and fetch the feed
/// in the background when it has not been checked recently.
pub fn open_episodes(state: &mut AppState, runtime: &Runtime, podcast_id: i64) {
    if runtime.podcasts.is_none() {
        state.podcasts.episodes = Some((
            podcast_id,
            Loadable::Failed("podcast storage is unavailable".into()),
        ));
        return;
    }
    load_episodes(runtime, podcast_id);
    let stale = state
        .podcasts
        .podcast(podcast_id)
        .is_some_and(|podcast| is_stale(podcast.last_checked_at, now_epoch_seconds()));
    if stale && !state.podcasts.refreshing.contains(&podcast_id) {
        state.podcasts.refreshing.insert(podcast_id);
        spawn_refresh(runtime, vec![podcast_id], true);
    }
}

fn is_stale(last_checked_at: Option<i64>, now: i64) -> bool {
    last_checked_at.is_none_or(|checked| now.saturating_sub(checked) >= PODCAST_STALE_SECS)
}

pub fn subscribe(state: &mut AppState, runtime: &Runtime, url: String) {
    let Some(podcasts) = &runtime.podcasts else {
        return unavailable(state);
    };
    if state.podcasts.subscribing {
        state.status_message = Some("podcasts: a subscription is already being added".into());
        return;
    }
    state.podcasts.subscribing = true;
    state.status_message = Some(format!("podcasts: reading feed {url}…"));
    let store = Arc::clone(&podcasts.store);
    let fetcher = Arc::clone(&podcasts.fetcher);
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let result = crate::podcasts::subscribe(store, &fetcher, &url, now_epoch_seconds())
            .await
            .map_err(err_string);
        let _ = tx.send(AppEvent::PodcastSubscribed(result));
    });
}

/// Effect::PodcastRefresh / the periodic sweep. Callers have already put the
/// ids into `state.podcasts.refreshing`.
pub fn spawn_refresh(runtime: &Runtime, ids: Vec<i64>, quiet: bool) {
    let Some(podcasts) = &runtime.podcasts else {
        return;
    };
    for podcast_id in ids {
        let store = Arc::clone(&podcasts.store);
        let fetcher = Arc::clone(&podcasts.fetcher);
        let slots = Arc::clone(&podcasts.refresh_slots);
        let tx = runtime.event_tx.clone();
        tokio::spawn(async move {
            let Ok(_permit) = slots.acquire_owned().await else {
                return;
            };
            let result = crate::podcasts::refresh(store, &fetcher, podcast_id, now_epoch_seconds())
                .await
                .map_err(err_string);
            let _ = tx.send(AppEvent::PodcastRefreshed {
                podcast_id,
                result,
                quiet,
            });
        });
    }
}

pub fn refresh(state: &mut AppState, runtime: &Runtime, ids: Vec<i64>) {
    if runtime.podcasts.is_none() {
        for id in &ids {
            state.podcasts.refreshing.remove(id);
        }
        return unavailable(state);
    }
    spawn_refresh(runtime, ids, false);
}

pub fn unsubscribe(state: &mut AppState, runtime: &Runtime, podcast_id: i64) {
    let Some(podcasts) = &runtime.podcasts else {
        return unavailable(state);
    };
    let store = Arc::clone(&podcasts.store);
    let tx = runtime.event_tx.clone();
    tokio::task::spawn_blocking(move || {
        let result = store
            .remove_podcast(podcast_id)
            .map(|cover_path| {
                // Covers are replaceable cache; a leftover file is harmless.
                if let Some(path) = cover_path
                    && let Err(err) = std::fs::remove_file(&path)
                    && err.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::debug!(%err, path, "podcast cover cleanup failed");
                }
            })
            .map_err(err_string);
        let _ = tx.send(AppEvent::PodcastUnsubscribed { podcast_id, result });
    });
}

/// The periodic sweep: refresh every subscription not checked recently.
pub fn auto_refresh(state: &mut AppState, runtime: &Runtime) {
    if runtime.podcasts.is_none() {
        return;
    }
    let now = now_epoch_seconds();
    state.podcasts.last_auto_refresh = Some(now);
    let ids: Vec<i64> = state
        .podcasts
        .podcasts()
        .iter()
        .filter(|podcast| is_stale(podcast.last_checked_at, now))
        .map(|podcast| podcast.id)
        .filter(|id| !state.podcasts.refreshing.contains(id))
        .collect();
    if ids.is_empty() {
        return;
    }
    tracing::debug!(count = ids.len(), "podcasts: refreshing stale feeds");
    state.podcasts.refreshing.extend(ids.iter().copied());
    spawn_refresh(runtime, ids, true);
}

/// Fold a podcast `AppEvent` into the state; any other event is handed back.
pub fn handle_event(state: &mut AppState, runtime: &Runtime, event: AppEvent) -> Option<AppEvent> {
    match event {
        AppEvent::PodcastsLoaded(result) => {
            let selected_id = selected_podcast_id(state);
            match result {
                Ok(list) => state.podcasts.list = Some(Loadable::Ready(list)),
                Err(err) => {
                    tracing::warn!(%err, "podcast list load failed");
                    if matches!(state.podcasts.list, Some(Loadable::Ready(_))) {
                        // A failed reload keeps the visible subscriptions.
                        state.status_message = Some(format!("podcasts: {err}"));
                    } else {
                        state.podcasts.list = Some(Loadable::Failed(err));
                    }
                }
            }
            restore_list_cursor(state, selected_id);
            // A podcast that disappeared (unsubscribed elsewhere) closes.
            if let Some(opened) = state.podcasts.opened
                && matches!(state.podcasts.list, Some(Loadable::Ready(_)))
                && state.podcasts.podcast(opened.id).is_none()
            {
                state.podcasts.opened = None;
                state.podcasts.episodes = None;
                state.podcasts.filter.clear();
            }
        }
        AppEvent::PodcastEpisodesLoaded { podcast_id, result } => {
            // Only the catalog slot this load was issued for may take it: a
            // slow read for a podcast the user already left must not replace
            // the one now open (each open re-keys the slot first).
            if !episodes_result_is_current(state, podcast_id) {
                return None;
            }
            // Keep the cursor on the same episode when new ones arrive above.
            let selected_episode = state
                .podcasts
                .selected_episode()
                .filter(|episode| episode.podcast_id == podcast_id)
                .map(|episode| episode.id);
            match result {
                Ok(episodes) => {
                    state.podcasts.episodes = Some((podcast_id, Loadable::Ready(episodes)));
                }
                Err(err) => {
                    tracing::warn!(%err, podcast_id, "podcast episodes load failed");
                    // A failed reload keeps an already visible catalog.
                    let keep = matches!(
                        &state.podcasts.episodes,
                        Some((id, Loadable::Ready(_))) if *id == podcast_id
                    );
                    if keep {
                        state.status_message = Some(format!("podcasts: {err}"));
                    } else {
                        state.podcasts.episodes = Some((podcast_id, Loadable::Failed(err)));
                    }
                }
            }
            if let (Some(episode_id), Some(opened)) = (selected_episode, state.podcasts.opened)
                && opened.id == podcast_id
                && let Some(episodes) = state.podcasts.opened_episodes()
            {
                let visible = state.podcasts.visible_episode_indices();
                if let Some(cursor) = visible
                    .iter()
                    .position(|&index| episodes[index].id == episode_id)
                {
                    state.podcasts.opened = Some(OpenedPodcast {
                        id: podcast_id,
                        cursor,
                    });
                }
            }
        }
        AppEvent::PodcastSubscribed(result) => {
            state.podcasts.subscribing = false;
            match result {
                Ok(podcast_id) => {
                    state.status_message = Some("podcasts: subscribed".into());
                    // Select the new row once the reloaded list arrives.
                    state.podcasts.opened = None;
                    state.podcasts.filter.clear();
                    state.podcasts.pending_select = Some(podcast_id);
                    load_list(runtime);
                }
                Err(err) => {
                    state.status_message = Some(format!("podcasts: cannot subscribe: {err}"));
                }
            }
        }
        AppEvent::PodcastRefreshed {
            podcast_id,
            result,
            quiet,
        } => {
            state.podcasts.refreshing.remove(&podcast_id);
            let title = state
                .podcasts
                .podcast(podcast_id)
                .map(|podcast| podcast.title.clone())
                .unwrap_or_else(|| "podcast".to_string());
            match result {
                Ok(summary) => {
                    if summary.new_episodes > 0 {
                        state.status_message = Some(format!(
                            "podcasts: {} new episode(s) in {title}",
                            summary.new_episodes
                        ));
                    } else if !quiet && state.podcasts.refreshing.is_empty() {
                        state.status_message = Some("podcasts: no new episodes".into());
                    }
                    if state
                        .podcasts
                        .opened
                        .is_some_and(|opened| opened.id == podcast_id)
                        && (summary.new_episodes > 0 || summary.updated_episodes > 0)
                    {
                        load_episodes(runtime, podcast_id);
                    }
                }
                Err(err) => {
                    tracing::warn!(%err, podcast_id, "podcast refresh failed");
                    if !quiet {
                        state.status_message =
                            Some(format!("podcasts: {title}: refresh failed: {err}"));
                    }
                }
            }
            // Counts, check time and errors live on the list rows.
            load_list(runtime);
        }
        AppEvent::PodcastUnsubscribed { podcast_id, result } => match result {
            Ok(()) => {
                if state
                    .podcasts
                    .opened
                    .is_some_and(|opened| opened.id == podcast_id)
                {
                    state.podcasts.opened = None;
                    state.podcasts.filter.clear();
                }
                if matches!(&state.podcasts.episodes, Some((id, _)) if *id == podcast_id) {
                    state.podcasts.episodes = None;
                }
                state.status_message = Some("podcasts: unsubscribed".into());
                load_list(runtime);
            }
            Err(err) => {
                state.status_message = Some(format!("podcasts: unsubscribe failed: {err}"));
            }
        },
        AppEvent::PodcastAutoRefreshDue => auto_refresh(state, runtime),
        other => return Some(other),
    }
    None
}

fn episodes_result_is_current(state: &AppState, podcast_id: i64) -> bool {
    matches!(&state.podcasts.episodes, Some((id, _)) if *id == podcast_id)
}

fn selected_podcast_id(state: &AppState) -> Option<i64> {
    if state.podcasts.opened.is_some() {
        return None;
    }
    state.podcasts.current_podcast().map(|podcast| podcast.id)
}

/// After a list reload, keep the cursor on the same podcast (or on a newly
/// subscribed one) even when rows above it were added or removed.
fn restore_list_cursor(state: &mut AppState, previous: Option<i64>) {
    let target = state.podcasts.pending_select.take().or(previous);
    let rows = state.podcasts.list_rows().len();
    if let Some(id) = target
        && let Some(index) = state
            .podcasts
            .podcasts()
            .iter()
            .position(|podcast| podcast.id == id)
    {
        state.podcasts.selected = index + super::state::PODCAST_ACTION_ROWS;
    }
    state.podcasts.selected = state.podcasts.selected.min(rows.saturating_sub(1));
}

/// Start playing a remote episode URL. Opening the HTTP stream may take a
/// few seconds, so it runs on the blocking pool; a newer playback request
/// started meanwhile wins (`playback_seq`).
pub fn start_stream(
    runtime: &Runtime,
    track: crate::library::models::TrackItem,
    volume: u8,
    position_secs: f64,
    paused: bool,
) {
    let seq = runtime.playback_seq.load(Ordering::SeqCst);
    let current_seq = Arc::clone(&runtime.playback_seq);
    let controller = runtime.player.clone();
    let tx = runtime.event_tx.clone();
    let volume = crate::player::amplitude(volume);
    // Silence the previous track while the stream connects. This must not
    // bump `playback_seq`: the request captured above is the current one.
    runtime.player.stop();
    tokio::task::spawn_blocking(move || {
        // Connect and probe the container here, not on the audio thread:
        // both wait on the network, and a stalled host must not block
        // pause/stop of whatever plays next.
        let decoded = crate::podcasts::stream::open(&track.file_path).and_then(|opened| {
            let coarse_seek = opened.seekable && opened.byte_len.is_some();
            crate::player::decode_source_with(
                Box::new(opened.reader),
                opened.byte_len,
                opened.mime_type.as_deref(),
                opened.seekable,
                coarse_seek,
            )
            .map_err(|err| anyhow::anyhow!("cannot decode the episode audio: {err}"))
        });
        // A newer start or stop happened meanwhile: neither play nor report.
        if current_seq.load(Ordering::SeqCst) != seq {
            tracing::debug!(title = %track.title, "podcast stream superseded before start");
            return;
        }
        match decoded {
            Ok(source) => {
                controller.play_source(source, volume);
                if position_secs > 0.0 {
                    controller.seek(Duration::from_secs_f64(position_secs));
                }
                if paused {
                    controller.pause();
                }
            }
            Err(err) => {
                tracing::warn!(title = %track.title, url = %track.file_path, err = %format!("{err:#}"), "podcast stream failed");
                let _ = tx.send(AppEvent::Player(crate::player::PlayerEvent::Failed(
                    format!("cannot stream \"{}\": {err:#}", track.title),
                )));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_episode_loads_for_another_podcast_are_ignored() {
        let mut state = AppState::default();
        state.podcasts.opened = Some(OpenedPodcast { id: 7, cursor: 0 });
        state.podcasts.episodes = Some((7, Loadable::Loading));
        assert!(episodes_result_is_current(&state, 7));
        assert!(!episodes_result_is_current(&state, 3));
        state.podcasts.episodes = None;
        assert!(!episodes_result_is_current(&state, 7));
    }

    #[test]
    fn staleness_uses_last_check_time() {
        let now = 1_000_000;
        assert!(is_stale(None, now));
        assert!(is_stale(Some(now - PODCAST_STALE_SECS), now));
        assert!(!is_stale(Some(now - PODCAST_STALE_SECS + 1), now));
        // A clock that moved backwards does not trigger a refresh storm.
        assert!(!is_stale(Some(now + 60), now));
    }
}
