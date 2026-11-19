//! Runtime side of the Podcasts tab: opens the podcast store, runs store
//! queries, feed fetches and sync-log writes off the UI thread, schedules
//! background refreshes, and hands results back as `AppEvent`s. Decisions
//! about state live in `update::podcasts`.

use std::sync::Arc;
use std::time::Duration;

use furumi_library::podcasts::{PodcastListFilter, PodcastStore};

use super::event::AppEvent;
use super::state::{AppState, PodcastRow};
use super::update::{Effect, podcasts};
use super::{Runtime, err_string, perform_effect};
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
            tokio::task::spawn_blocking(move || {
                let result = set_played(&services, &episode_id, played).map_err(err_string);
                let _ = tx.send(AppEvent::PodcastPlayedSet { played, result });
            });
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

/// "Mark played" / "Mark unplayed": explicit progress actions that bump the
/// generation and are appended to the sync log.
fn set_played(
    services: &Services,
    episode_id: &music_dht::podcast::EpisodeId,
    played: bool,
) -> anyhow::Result<()> {
    let episode = services
        .store
        .episode_ref(episode_id)?
        .ok_or_else(|| anyhow::anyhow!("the episode is no longer in the catalog"))?;
    let store = &services.store;
    services.log.record(&mut |stamps| {
        let write = if played {
            store.mark_played(stamps, &episode)?
        } else {
            store.mark_unplayed(stamps, &episode)?
        };
        Ok(vec![write.op])
    })
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
                runtime.podcasts = Some(PodcastRuntime {
                    services,
                    refresh_slots: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REFRESHES)),
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
        AppEvent::PodcastPlayedSet { played, result } => {
            podcasts::played_set(state, played, result)
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
