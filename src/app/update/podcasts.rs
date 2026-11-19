//! Podcasts tab state transitions.
//!
//! Pure functions over `AppState`: they request store, HTTP and sync work
//! through [`Effect`]s (executed by `app::podcast_runtime`) and fold the
//! results back in when the runtime reports them.

use furumi_library::podcasts::{
    EpisodeListItem, EpisodeQuery, EpisodeSort, PlayedFilter, SeasonFilter, SeasonSummary,
};
use music_dht::podcast::{EpisodeId, FeedUrl, PodcastId, ProgressRecord, ProgressStatus};

use super::Effect;
use crate::app::action::Action;
use crate::app::input::LineEdit;
use crate::app::state::track_key;
use crate::app::state::{
    AppState, EpisodeList, FedInputField, Loadable, OpenedPodcast, PODCAST_ACTION_ROWS,
    PODCAST_EPISODE_PAGE, PodcastListRow, PodcastRow, PodcastStoreStatus, Popup, SeasonRow, Tab,
};
use crate::library::models::TrackItem;
use crate::podcasts::refresh::{RefreshReport, SubscribeReport};

/// The next page starts loading when the cursor is this close to the end.
const LOAD_MORE_MARGIN: usize = 20;

/// Podcasts-tab handling of `action`; `None` lets the global handler run.
pub(super) fn handle(state: &mut AppState, action: &Action) -> Option<Option<Effect>> {
    let effect = match action {
        Action::MoveUp => move_by(state, -1),
        Action::MoveDown => move_by(state, 1),
        Action::PageUp => move_by(state, -super::page_step(state)),
        Action::PageDown => move_by(state, super::page_step(state)),
        Action::SelectFirst => move_by(state, isize::MIN),
        Action::SelectLast => move_by(state, isize::MAX),
        Action::Select => select(state),
        Action::Back if state.podcasts.opened.is_some() => {
            back(state);
            None
        }
        Action::OpenSearch | Action::SearchEpisodes => {
            open_search(state);
            None
        }
        Action::DeleteSelected | Action::UnsubscribePodcast => {
            confirm_unsubscribe(state);
            None
        }
        Action::EditSelected => {
            state.status_message = Some("podcast details come from the RSS feed".into());
            None
        }
        Action::AddPodcast => {
            open_add(state);
            None
        }
        Action::RefreshPodcast => refresh_current(state),
        Action::RefreshAllPodcasts => refresh_all(state),
        Action::CycleEpisodeSort => cycle_sort(state),
        Action::CycleEpisodeFilter => cycle_played_filter(state),
        Action::ToggleEpisodePlayed => toggle_played(state),
        Action::RestartEpisode => restart_selected(state),
        _ => return None,
    };
    Some(effect)
}

/// Whether podcast actions may run; explains why not in the status bar.
fn store_ready(state: &mut AppState) -> bool {
    match &state.podcasts.store {
        PodcastStoreStatus::Ready => true,
        PodcastStoreStatus::Opening => {
            state.status_message = Some("podcasts: the podcast database is still opening".into());
            false
        }
        PodcastStoreStatus::Failed(err) => {
            state.status_message = Some(format!("podcasts are unavailable: {err}"));
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Navigation

fn move_by(state: &mut AppState, delta: isize) -> Option<Effect> {
    let tab = &mut state.podcasts;
    let len = match &tab.opened {
        None => tab.list_rows().len(),
        Some(opened) if opened.season.is_none() => tab.season_rows().len(),
        Some(_) => tab.opened_episodes().map_or(0, |list| list.items.len()),
    };
    if len == 0 {
        return None;
    }
    let target = |cursor: usize| {
        (cursor as isize)
            .saturating_add(delta)
            .clamp(0, len as isize - 1) as usize
    };
    match &mut tab.opened {
        None => tab.selected = target(tab.selected),
        Some(opened) => match (opened.season, &mut opened.season_cursor) {
            (None, Some(cursor)) => *cursor = target(*cursor),
            (None, None) => {}
            (Some(_), _) => opened.episode_cursor = target(opened.episode_cursor),
        },
    }
    load_more_if_needed(state)
}

/// Requests the next episode page when the cursor nears the loaded end.
fn load_more_if_needed(state: &mut AppState) -> Option<Effect> {
    let cursor = state.podcasts.opened.as_ref()?.episode_cursor;
    state.podcasts.opened_episodes()?;
    let list = state.podcasts.episodes.as_mut()?;
    if list.complete
        || list.loading
        || list.loading_more
        || cursor + LOAD_MORE_MARGIN < list.items.len()
    {
        return None;
    }
    list.loading_more = true;
    let mut query = list.query.clone();
    query.offset = list.items.len();
    Some(Effect::PodcastLoadEpisodes {
        seq: list.seq,
        query,
    })
}

fn select(state: &mut AppState) -> Option<Effect> {
    let Some(opened) = state.podcasts.opened.clone() else {
        return match state
            .podcasts
            .list_rows()
            .get(state.podcasts.selected)
            .copied()?
        {
            PodcastListRow::Add => {
                open_add(state);
                None
            }
            PodcastListRow::RefreshAll => refresh_all(state),
            PodcastListRow::Podcast(index) => open_podcast(state, index),
        };
    };
    if opened.season.is_none() {
        let cursor = opened.season_cursor.unwrap_or(0);
        let season = match state.podcasts.season_rows().get(cursor).copied()? {
            SeasonRow::All => SeasonFilter::Any,
            SeasonRow::Group(index) => match state.podcasts.opened_seasons()?.get(index)?.season {
                Some(number) => SeasonFilter::Season(number),
                None => SeasonFilter::NoSeason,
            },
        };
        if let Some(opened) = &mut state.podcasts.opened {
            opened.season = Some(season);
            opened.episode_cursor = 0;
        }
        state.podcasts.search.clear();
        return Some(load_episodes(state, None));
    }
    let track = state.podcasts.selected_episode_track()?;
    play_episode_now(state, track, false)
}

// ---------------------------------------------------------------------------
// Playback

/// Episodes play on this device only for now: a device that controls
/// another one would hand them over as private placeholders (sync protocol
/// v3 cannot carry them). Says so and returns `false` in that case.
pub fn episodes_play_here(state: &mut AppState) -> bool {
    if state.device_playback.is_control() {
        state.status_message = Some(
            "podcasts: playback is on another device; episodes play only on this device \
             (take playback over here first)"
                .into(),
        );
        return false;
    }
    true
}

/// Enter on an episode: play it now, right after the current item, keeping
/// the rest of the queue. Ordinary Play resumes from the saved position;
/// `restart` is the explicit "start over".
pub(super) fn play_episode_now(
    state: &mut AppState,
    track: TrackItem,
    restart: bool,
) -> Option<Effect> {
    if !episodes_play_here(state) {
        return None;
    }
    let key = track_key(&track);
    let player = &mut state.player;
    if let Some(index) = player
        .current
        .as_ref()
        .filter(|current| track_key(current) == key)
        .and_then(|_| {
            if player
                .queue
                .get(player.queue_pos)
                .is_some_and(|queued| track_key(queued) == key)
            {
                Some(player.queue_pos)
            } else {
                player
                    .queue
                    .iter()
                    .position(|queued| track_key(queued) == key)
            }
        })
    {
        // Already the current item.
        player.queue_pos = index;
        if restart {
            return Some(Effect::PlayCurrentFromStart);
        }
        if player.playing && player.paused {
            player.paused = false;
            return Some(Effect::TogglePause);
        }
        if player.playing {
            state.status_message = Some("podcasts: this episode is playing".into());
            return None;
        }
        return Some(Effect::PlayCurrent);
    }
    let len = player.queue.len();
    let at = if player.current.is_some() {
        (player.queue_pos + 1).min(len)
    } else {
        player.queue_pos.min(len)
    };
    player.queue.insert(at, track);
    if let Some(end) = player.play_next_end
        && end >= at
    {
        player.play_next_end = Some(end + 1);
    }
    player.prefetched_pos = None;
    player.queue_pos = at;
    super::normalize_play_next_block(player);
    Some(if restart {
        Effect::PlayCurrentFromStart
    } else {
        Effect::PlayCurrent
    })
}

/// `b` on the episode list: play the selected episode from the beginning.
fn restart_selected(state: &mut AppState) -> Option<Effect> {
    match state.podcasts.selected_episode_track() {
        Some(track) => play_episode_now(state, track, true),
        None => restart_current(state),
    }
}

/// "Start over" for the episode that is playing.
pub fn restart_current(state: &mut AppState) -> Option<Effect> {
    let Some(track) = state.player.current.clone().filter(TrackItem::is_podcast) else {
        state.status_message = Some("podcasts: select an episode or play one first".into());
        return None;
    };
    play_episode_now(state, track, true)
}

/// The progress writer saved (or found) a progress register: show it in
/// the episode list without reloading it, and carry its generation in the
/// queue items of that episode (a handoff passes it on).
pub fn progress_saved(state: &mut AppState, record: ProgressRecord) -> Vec<Effect> {
    if let Some(list) = state.podcasts.episodes.as_mut() {
        for item in &mut list.items {
            if item.episode.episode_id == record.episode_id {
                item.progress = Some(record.clone());
            }
        }
    }
    let player = &mut state.player;
    for track in player.queue.iter_mut().chain(player.current.as_mut()) {
        if let Some(episode) = track.podcast.as_mut()
            && episode.episode_id == record.episode_id
        {
            episode.progress_generation = Some(
                episode
                    .progress_generation
                    .unwrap_or(0)
                    .max(record.generation),
            );
        }
    }
    Vec::new()
}

/// Podcast state changed by other devices (received operations or
/// snapshot sections): progress registers update the list in place, the
/// rest reloads what is on screen.
pub fn synced(state: &mut AppState, progress: Vec<ProgressRecord>, changed: bool) -> Vec<Effect> {
    let mut effects = Vec::new();
    for record in progress {
        effects.extend(progress_saved(state, record));
    }
    if state.podcasts.store != PodcastStoreStatus::Ready {
        return effects;
    }
    // Counts in the list (unplayed, in progress) follow both.
    effects.push(Effect::PodcastsReload);
    if changed {
        if let Some(opened) = &state.podcasts.opened
            && opened.season_cursor.is_some()
        {
            effects.push(Effect::PodcastLoadSeasons(opened.podcast_id.clone()));
        }
        effects.extend(reload_episodes(state));
    }
    effects
}

fn open_podcast(state: &mut AppState, index: usize) -> Option<Effect> {
    let row = state.podcasts.podcasts().get(index)?;
    let podcast_id = row.id().clone();
    let has_seasons = row.summary.season_count > 0;
    state.podcasts.opened = Some(OpenedPodcast {
        podcast_id: podcast_id.clone(),
        season_cursor: has_seasons.then_some(0),
        season: (!has_seasons).then_some(SeasonFilter::Any),
        episode_cursor: 0,
    });
    state.podcasts.search.clear();
    if has_seasons {
        state.podcasts.seasons = Some((podcast_id.clone(), Loadable::Loading));
        Some(Effect::PodcastLoadSeasons(podcast_id))
    } else {
        Some(load_episodes(state, None))
    }
}

/// Shift-J (or the info popup's artist jump) on a podcast episode: opens its
/// podcast on the Podcasts tab instead of a music release or artist.
pub fn reveal_episode_podcast(state: &mut AppState, track: &TrackItem) -> Option<Effect> {
    let episode = track.podcast_episode_ref()?;
    let podcast_id = episode.podcast_id.clone();
    let title = episode.podcast_title.clone();
    state.active_tab = Tab::Podcasts;
    state.help_visible = false;
    state.track_selection.clear();
    state.jump_origin = None;
    if !store_ready(state) {
        return None;
    }
    if state
        .podcasts
        .opened
        .as_ref()
        .is_some_and(|opened| opened.podcast_id == podcast_id)
    {
        return None;
    }
    close(state);
    match state
        .podcasts
        .podcasts()
        .iter()
        .position(|row| *row.id() == podcast_id)
    {
        Some(index) => {
            state.podcasts.selected = index + PODCAST_ACTION_ROWS;
            open_podcast(state, index)
        }
        None => {
            state.status_message = Some(format!(
                "podcasts: {} is not subscribed on this device",
                display_title(&title)
            ));
            None
        }
    }
}

/// Esc: episodes -> seasons (when the podcast has them) -> podcast list.
fn back(state: &mut AppState) {
    let tab = &mut state.podcasts;
    match &mut tab.opened {
        Some(opened) if opened.season.is_some() && opened.season_cursor.is_some() => {
            opened.season = None;
            tab.search.clear();
        }
        Some(_) => close(state),
        None => {}
    }
}

/// Closes the opened podcast and puts the list cursor on it.
pub(super) fn close(state: &mut AppState) {
    let tab = &mut state.podcasts;
    if let Some(opened) = tab.opened.take()
        && let Some(index) = tab
            .podcasts()
            .iter()
            .position(|row| *row.id() == opened.podcast_id)
    {
        tab.selected = index + PODCAST_ACTION_ROWS;
    }
    tab.search.clear();
}

// ---------------------------------------------------------------------------
// Episode queries

/// Starts loading the first page of the opened group with the current
/// search, filter and sort. `focus` keeps the cursor on that episode when
/// it is still listed (reloads after a refresh); otherwise the cursor goes
/// to the top.
fn load_episodes(state: &mut AppState, focus: Option<EpisodeId>) -> Effect {
    let tab = &mut state.podcasts;
    let opened = tab
        .opened
        .as_mut()
        .expect("load_episodes needs an opened podcast");
    let season = opened.season.unwrap_or_default();
    let search = tab.search.trim();
    let query = EpisodeQuery {
        podcast_id: opened.podcast_id.clone(),
        season,
        played: tab.played,
        search: (!search.is_empty()).then(|| search.to_string()),
        sort: tab.sort,
        limit: Some(PODCAST_EPISODE_PAGE),
        offset: 0,
    };
    if focus.is_none() {
        opened.episode_cursor = 0;
    }
    tab.episode_seq += 1;
    let seq = tab.episode_seq;
    // Keep showing the previous rows of the same group while the new query
    // runs, so typing a search does not flash an empty list.
    let items = match tab.episodes.take() {
        Some(list) if list.query.podcast_id == query.podcast_id && list.query.season == season => {
            list.items
        }
        _ => Vec::new(),
    };
    tab.episodes = Some(EpisodeList {
        seq,
        query: query.clone(),
        items,
        loading: true,
        loading_more: false,
        complete: false,
        error: None,
        focus,
    });
    Effect::PodcastLoadEpisodes { seq, query }
}

/// Reloads the visible episodes, keeping the cursor on the same episode.
fn reload_episodes(state: &mut AppState) -> Option<Effect> {
    if !state.podcasts.showing_episodes() {
        return None;
    }
    let focus = state
        .podcasts
        .selected_episode()
        .map(|item| item.episode.episode_id.clone());
    Some(load_episodes(state, focus))
}

/// Live search: called for every edit of the search field.
pub fn set_search(state: &mut AppState, search: String) -> Option<Effect> {
    if state.podcasts.search == search {
        return None;
    }
    state.podcasts.search = search;
    state
        .podcasts
        .showing_episodes()
        .then(|| load_episodes(state, None))
}

fn open_search(state: &mut AppState) {
    if !state.podcasts.showing_episodes() {
        state.status_message = Some("podcasts: open a podcast to search its episodes".into());
        return;
    }
    state.popup = Some(Popup::FedInput {
        field: FedInputField::PodcastSearch,
        input: LineEdit::new(state.podcasts.search.clone()),
    });
}

fn cycle_sort(state: &mut AppState) -> Option<Effect> {
    let next = match state.podcasts.sort {
        EpisodeSort::NewestFirst => EpisodeSort::OldestFirst,
        EpisodeSort::OldestFirst => EpisodeSort::EpisodeNumber,
        EpisodeSort::EpisodeNumber => EpisodeSort::EpisodeNumberDesc,
        EpisodeSort::EpisodeNumberDesc => EpisodeSort::NewestFirst,
    };
    state.podcasts.sort = next;
    state.status_message = Some(format!("episodes: {}", sort_label(next)));
    state
        .podcasts
        .showing_episodes()
        .then(|| load_episodes(state, None))
}

fn cycle_played_filter(state: &mut AppState) -> Option<Effect> {
    let next = match state.podcasts.played {
        PlayedFilter::Any => PlayedFilter::Unplayed,
        PlayedFilter::Unplayed => PlayedFilter::InProgress,
        PlayedFilter::InProgress => PlayedFilter::Played,
        PlayedFilter::Played | PlayedFilter::NotStarted => PlayedFilter::Any,
    };
    state.podcasts.played = next;
    state.status_message = Some(format!("episodes: {}", played_label(next)));
    state
        .podcasts
        .showing_episodes()
        .then(|| load_episodes(state, None))
}

pub fn sort_label(sort: EpisodeSort) -> &'static str {
    match sort {
        EpisodeSort::NewestFirst => "newest first",
        EpisodeSort::OldestFirst => "oldest first",
        EpisodeSort::EpisodeNumber => "by episode number",
        EpisodeSort::EpisodeNumberDesc => "by episode number, descending",
    }
}

pub fn played_label(filter: PlayedFilter) -> &'static str {
    match filter {
        PlayedFilter::Any => "all episodes",
        PlayedFilter::Unplayed => "unplayed",
        PlayedFilter::NotStarted => "not started",
        PlayedFilter::InProgress => "in progress",
        PlayedFilter::Played => "played",
    }
}

// ---------------------------------------------------------------------------
// Subscriptions

fn open_add(state: &mut AppState) {
    if !store_ready(state) {
        return;
    }
    if state.podcasts.subscribing {
        state.status_message = Some("podcasts: a subscription is already being added".into());
        return;
    }
    state.popup = Some(Popup::FedInput {
        field: FedInputField::PodcastFeedUrl,
        input: LineEdit::default(),
    });
}

/// Enter in the "Add podcast" field.
pub fn submit_feed_url(state: &mut AppState, input: &str) -> Option<Effect> {
    let input = input.trim();
    if input.is_empty() {
        state.status_message = Some("podcasts: the feed URL is empty".into());
        return None;
    }
    if !store_ready(state) {
        return None;
    }
    if let Err(err) = FeedUrl::from_user_input(input) {
        state.status_message = Some(format!("podcasts: {err}"));
        return None;
    }
    state.podcasts.subscribing = true;
    state.status_message = Some("podcasts: reading the feed…".into());
    Some(Effect::PodcastSubscribe(input.to_string()))
}

fn confirm_unsubscribe(state: &mut AppState) {
    if !store_ready(state) {
        return;
    }
    let Some(row) = state.podcasts.current_podcast() else {
        state.status_message = Some("podcasts: select a podcast to unsubscribe".into());
        return;
    };
    state.popup = Some(Popup::ConfirmPodcastUnsubscribe {
        podcast_id: row.id().clone(),
        title: row.title().to_string(),
    });
}

fn refresh_targets(state: &mut AppState, rows: Vec<&PodcastRow>) -> Vec<(PodcastId, FeedUrl)> {
    rows.into_iter()
        .filter(|row| !state.podcasts.refreshing.contains(row.id()))
        .map(|row| (row.id().clone(), row.summary.meta.feed_url.clone()))
        .collect()
}

fn start_refresh(
    state: &mut AppState,
    targets: Vec<(PodcastId, FeedUrl)>,
    manual: bool,
) -> Option<Effect> {
    if targets.is_empty() {
        return None;
    }
    state
        .podcasts
        .refreshing
        .extend(targets.iter().map(|(id, _)| id.clone()));
    Some(Effect::PodcastRefresh { targets, manual })
}

/// Shift-R: the opened podcast or the one under the cursor.
fn refresh_current(state: &mut AppState) -> Option<Effect> {
    if !store_ready(state) {
        return None;
    }
    let Some(row) = state.podcasts.current_podcast().cloned() else {
        return refresh_all(state);
    };
    if state.podcasts.refreshing.contains(row.id()) {
        state.status_message = Some(format!("podcasts: already checking {}", row.title()));
        return None;
    }
    let targets = refresh_targets(state, vec![&row]);
    state.status_message = Some(format!("podcasts: checking {}…", row.title()));
    start_refresh(state, targets, true)
}

/// Ctrl-R / "Check all": every subscription.
fn refresh_all(state: &mut AppState) -> Option<Effect> {
    if !store_ready(state) {
        return None;
    }
    let rows: Vec<PodcastRow> = state.podcasts.podcasts().to_vec();
    if rows.is_empty() {
        state.status_message = Some("podcasts: no subscriptions yet".into());
        return None;
    }
    let targets = refresh_targets(state, rows.iter().collect());
    if targets.is_empty() {
        state.status_message = Some("podcasts: already checking every feed".into());
        return None;
    }
    state.status_message = Some(format!(
        "podcasts: checking {} feed(s) for new episodes…",
        targets.len()
    ));
    start_refresh(state, targets, true)
}

/// Scheduler: feeds the store reports as due (back-off already applied).
pub fn refresh_due(state: &mut AppState, due: Vec<(PodcastId, FeedUrl)>) -> Option<Effect> {
    let targets: Vec<_> = due
        .into_iter()
        .filter(|(id, _)| !state.podcasts.refreshing.contains(id))
        .collect();
    start_refresh(state, targets, false)
}

fn toggle_played(state: &mut AppState) -> Option<Effect> {
    if !store_ready(state) {
        return None;
    }
    let Some(item) = state.podcasts.selected_episode() else {
        state.status_message = Some("podcasts: select an episode".into());
        return None;
    };
    let played = item.status() != ProgressStatus::Completed;
    Some(Effect::PodcastSetPlayed {
        episode_id: item.episode.episode_id.clone(),
        played,
    })
}

// ---------------------------------------------------------------------------
// Results from the runtime

/// The store opened (or failed to).
pub fn store_opened(state: &mut AppState, result: Result<(), String>) -> Option<Effect> {
    match result {
        Ok(()) => {
            state.podcasts.store = PodcastStoreStatus::Ready;
            state.podcasts.list = Some(Loadable::Loading);
            Some(Effect::PodcastsReload)
        }
        Err(err) => {
            state.podcasts.store = PodcastStoreStatus::Failed(err);
            None
        }
    }
}

pub fn list_loaded(state: &mut AppState, result: Result<Vec<PodcastRow>, String>) {
    let tab = &mut state.podcasts;
    let previous = match (&tab.opened, tab.list_rows().get(tab.selected)) {
        (None, Some(PodcastListRow::Podcast(index))) => {
            tab.podcasts().get(*index).map(|row| row.id().clone())
        }
        _ => None,
    };
    match result {
        Ok(rows) => tab.list = Some(Loadable::Ready(rows)),
        Err(err) => {
            tracing::warn!(%err, "podcast list load failed");
            if matches!(tab.list, Some(Loadable::Ready(_))) {
                state.status_message = Some(format!("podcasts: {err}"));
                return;
            }
            tab.list = Some(Loadable::Failed(err));
            return;
        }
    }
    if let Some(id) = tab.pending_select.take().or(previous)
        && let Some(index) = tab.podcasts().iter().position(|row| *row.id() == id)
    {
        tab.selected = index + PODCAST_ACTION_ROWS;
    }
    tab.selected = tab.selected.min(tab.list_rows().len().saturating_sub(1));
    // A podcast unsubscribed elsewhere closes.
    if let Some(opened) = &tab.opened
        && tab.podcast(&opened.podcast_id).is_none()
    {
        close(state);
    }
}

pub fn seasons_loaded(
    state: &mut AppState,
    podcast_id: PodcastId,
    result: Result<Vec<SeasonSummary>, String>,
) {
    let tab = &mut state.podcasts;
    if tab.opened.as_ref().map(|opened| &opened.podcast_id) != Some(&podcast_id) {
        return;
    }
    let loaded = match result {
        Ok(seasons) => Loadable::Ready(seasons),
        Err(err) => {
            if matches!(&tab.seasons, Some((id, Loadable::Ready(_))) if *id == podcast_id) {
                state.status_message = Some(format!("podcasts: {err}"));
                return;
            }
            Loadable::Failed(err)
        }
    };
    tab.seasons = Some((podcast_id, loaded));
    let rows = tab.season_rows().len();
    if let Some(OpenedPodcast {
        season_cursor: Some(cursor),
        ..
    }) = &mut tab.opened
    {
        *cursor = (*cursor).min(rows.saturating_sub(1));
    }
}

/// A page of episodes arrived. Stale sequences and pages that no longer
/// line up with the loaded rows are dropped.
pub fn episodes_loaded(
    state: &mut AppState,
    seq: u64,
    offset: usize,
    result: Result<Vec<EpisodeListItem>, String>,
) {
    let tab = &mut state.podcasts;
    let Some(list) = tab.episodes.as_mut().filter(|list| list.seq == seq) else {
        return;
    };
    match result {
        Ok(page) => {
            let complete = page.len() < PODCAST_EPISODE_PAGE;
            if offset == 0 {
                list.items = page;
                list.loading = false;
            } else if offset == list.items.len() && list.loading_more {
                list.items.extend(page);
                list.loading_more = false;
            } else {
                return;
            }
            list.complete = complete;
            list.error = None;
        }
        Err(err) => {
            tracing::warn!(%err, "podcast episodes load failed");
            list.loading = false;
            list.loading_more = false;
            list.error = Some(err);
            return;
        }
    }
    let focus = list.focus.take();
    let len = list.items.len();
    let focused = focus.and_then(|id| {
        list.items
            .iter()
            .position(|item| item.episode.episode_id == id)
    });
    if let Some(opened) = &mut tab.opened {
        if let Some(index) = focused {
            opened.episode_cursor = index;
        }
        opened.episode_cursor = opened.episode_cursor.min(len.saturating_sub(1));
    }
}

pub fn subscribed(state: &mut AppState, result: Result<SubscribeReport, String>) -> Option<Effect> {
    state.podcasts.subscribing = false;
    match result {
        Ok(report) => {
            let title = display_title(&report.title);
            state.status_message = Some(if report.already_subscribed {
                format!("podcasts: already subscribed to {title}")
            } else {
                format!(
                    "podcasts: subscribed to {title} ({} episode(s); private)",
                    report.new_episodes
                )
            });
            if state.podcasts.opened.is_some() {
                close(state);
            }
            state.podcasts.pending_select = Some(report.podcast_id);
            Some(Effect::PodcastsReload)
        }
        Err(err) => {
            state.status_message = Some(format!("podcasts: cannot subscribe: {err}"));
            None
        }
    }
}

fn display_title(title: &str) -> String {
    if title.trim().is_empty() {
        "the podcast".to_string()
    } else {
        format!("\"{title}\"")
    }
}

pub fn refreshed(
    state: &mut AppState,
    podcast_id: PodcastId,
    result: Result<RefreshReport, String>,
    manual: bool,
) -> Vec<Effect> {
    state.podcasts.refreshing.remove(&podcast_id);
    let title = state
        .podcasts
        .podcast(&podcast_id)
        .map(|row| display_title(row.title()))
        .unwrap_or_else(|| "the podcast".to_string());
    let mut effects = vec![Effect::PodcastsReload];
    match result {
        Ok(report) => {
            if report.new_episodes > 0 {
                state.status_message = Some(format!(
                    "podcasts: {} new episode(s) in {title}",
                    report.new_episodes
                ));
            } else if manual && state.podcasts.refreshing.is_empty() {
                state.status_message = Some("podcasts: no new episodes".into());
            }
            let changed = report.new_episodes > 0 || report.updated_episodes > 0;
            let opened_here = state
                .podcasts
                .opened
                .as_ref()
                .is_some_and(|opened| opened.podcast_id == podcast_id);
            if changed && opened_here {
                if state
                    .podcasts
                    .opened
                    .as_ref()
                    .is_some_and(|opened| opened.season_cursor.is_some())
                {
                    effects.push(Effect::PodcastLoadSeasons(podcast_id.clone()));
                }
                effects.extend(reload_episodes(state));
            }
        }
        Err(err) => {
            tracing::warn!(podcast = %podcast_id, %err, "podcast refresh failed");
            if manual {
                state.status_message = Some(format!("podcasts: {title}: {err}"));
            }
        }
    }
    effects
}

pub fn unsubscribed(
    state: &mut AppState,
    podcast_id: PodcastId,
    title: String,
    result: Result<(), String>,
) -> Option<Effect> {
    match result {
        Ok(()) => {
            state.status_message = Some(format!(
                "podcasts: unsubscribed from {}; progress and history are kept",
                display_title(&title)
            ));
            if state
                .podcasts
                .opened
                .as_ref()
                .is_some_and(|opened| opened.podcast_id == podcast_id)
            {
                close(state);
            }
            Some(Effect::PodcastsReload)
        }
        Err(err) => {
            state.status_message = Some(format!("podcasts: unsubscribe failed: {err}"));
            None
        }
    }
}

pub fn played_set(state: &mut AppState, played: bool, result: Result<(), String>) -> Vec<Effect> {
    match result {
        Ok(()) => {
            state.status_message = Some(
                if played {
                    "podcasts: marked as played"
                } else {
                    "podcasts: marked as unplayed"
                }
                .into(),
            );
            let mut effects = vec![Effect::PodcastsReload];
            if let Some(opened) = &state.podcasts.opened
                && opened.season_cursor.is_some()
            {
                effects.push(Effect::PodcastLoadSeasons(opened.podcast_id.clone()));
            }
            effects.extend(reload_episodes(state));
            effects
        }
        Err(err) => {
            state.status_message = Some(format!("podcasts: {err}"));
            Vec::new()
        }
    }
}
