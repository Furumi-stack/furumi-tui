use std::time::{Duration, Instant};

use super::action::Action;
use crate::library::models::TrackItem;

use super::state::{
    AppState, GlobalView, Loadable, OpenedPlaylist, SearchState, TILE_HEIGHT, TILE_WIDTH, Tab,
    TrackSelectionScope, ViewMode, release_display_order, release_rows,
};

pub const QUIT_CONFIRM_WINDOW: Duration = Duration::from_millis(1500);
pub const QUIT_CONFIRM_HINT: &str = "press quit again to exit";

/// Side effects requested by `update()`; executed by the app loop, which
/// owns the Runtime (audio controller, API client). Keeps update() pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// (Re)start playback of `queue[queue_pos]`.
    PlayCurrent,
    TogglePause,
    StopPlayback,
    /// Seek relative to the current position, in seconds.
    SeekBy(i64),
    SetVolume(u8),
    SetOptions,
    /// Fetch a release and append all its tracks to the queue.
    EnqueueRelease {
        id: i64,
        next: bool,
    },
    ToggleLikes {
        track_ids: Vec<i64>,
    },
    /// Remove tracks from a stored playlist in the library.
    RemoveFromPlaylist {
        playlist_id: i64,
        track_ids: Vec<i64>,
    },
    RemoveQueueIndices {
        indices: Vec<usize>,
        restart_paused: Option<bool>,
        stop: bool,
    },
    /// Persist the Federation-tab settings and start/stop the node.
    FedApplySettings,
    /// Force an immediate library publish into the DHT.
    FedSyncNow,
    /// Fetch this peer's ticket and show it in a popup.
    FedShowTicket,
    /// Download (or resolve) a federated track and play it.
    FedPlay(crate::federation::FedTrack),
}

pub fn update(state: &mut AppState, action: Action) -> Option<Effect> {
    // Any action other than a second Quit press disarms the confirmation.
    let quit_armed = state
        .quit_armed_until
        .take()
        .is_some_and(|deadline| Instant::now() <= deadline);
    state.status_message = None;
    match action {
        // While the help window is open, quit/back just close it.
        Action::Quit | Action::Back if state.help_visible => {
            state.help_visible = false;
            None
        }
        Action::Quit => {
            if quit_armed {
                state.should_quit = true;
            } else {
                state.quit_armed_until = Some(Instant::now() + QUIT_CONFIRM_WINDOW);
                state.status_message = Some(QUIT_CONFIRM_HINT.to_string());
            }
            None
        }
        Action::ToggleHelp => {
            state.help_visible = !state.help_visible;
            None
        }
        Action::NextTab => {
            switch_tab(state, state.active_tab.next());
            None
        }
        Action::PrevTab => {
            switch_tab(state, state.active_tab.prev());
            None
        }
        Action::GoToTab(index) => {
            if let Some(tab) = Tab::from_index(index) {
                // Pressing the current tab's number again resets it to its
                // top level (closes drill-down views).
                if tab == state.active_tab {
                    reset_tab(state, tab);
                } else {
                    switch_tab(state, tab);
                }
            }
            None
        }
        Action::PlayPause => {
            if state.player.current.is_some() {
                state.player.paused = !state.player.paused;
                Some(Effect::TogglePause)
            } else if state.player.queue.is_empty() {
                state.status_message = Some("nothing queued — open a track and press enter".into());
                None
            } else {
                Some(Effect::PlayCurrent)
            }
        }
        Action::NextTrack => queue_step(state, 1),
        Action::PrevTrack => queue_step(state, -1),
        Action::SeekForward { seconds } => state
            .player
            .current
            .is_some()
            .then_some(Effect::SeekBy(seconds as i64)),
        Action::SeekBackward { seconds } => state
            .player
            .current
            .is_some()
            .then_some(Effect::SeekBy(-(seconds as i64))),
        Action::VolumeUp => {
            state.player.volume = (state.player.volume + 5).min(100);
            Some(Effect::SetVolume(state.player.volume))
        }
        Action::VolumeDown => {
            state.player.volume = state.player.volume.saturating_sub(5);
            Some(Effect::SetVolume(state.player.volume))
        }
        Action::ToggleShuffle => {
            state.player.shuffle = !state.player.shuffle;
            // Shuffle physically reorders the unplayed tail, so the Queue
            // tab always shows the real upcoming order; turning it off
            // restores the original ordering.
            if state.player.shuffle {
                shuffle_upcoming(&mut state.player);
            } else {
                restore_queue_order(&mut state.player);
            }
            Some(Effect::SetOptions)
        }
        Action::CycleRepeat => {
            state.player.repeat = state.player.repeat.next();
            Some(Effect::SetOptions)
        }
        Action::MoveUp => {
            move_selection(state, 0, -1);
            None
        }
        Action::MoveDown => {
            move_selection(state, 0, 1);
            None
        }
        Action::MoveLeft => {
            move_selection(state, -1, 0);
            None
        }
        Action::MoveRight => {
            move_selection(state, 1, 0);
            None
        }
        Action::PageUp => {
            move_selection(state, 0, -page_step(state));
            None
        }
        Action::PageDown => {
            move_selection(state, 0, page_step(state));
            None
        }
        Action::SelectFirst => {
            jump_selection(state, true);
            None
        }
        Action::SelectLast => {
            jump_selection(state, false);
            None
        }
        Action::ToggleViewMode => {
            match state.active_tab {
                Tab::Global => state.global.view = state.global.view.toggle(),
                // On the Logs tab the same key cycles the severity filter.
                Tab::Logs => {
                    state.logs.level_index =
                        (state.logs.level_index + 1) % super::state::LOG_LEVELS.len();
                    state.logs.selected_seq = None;
                    state.logs.follow = true;
                }
                _ => {}
            }
            None
        }
        Action::OpenCommandLine => {
            state.cmdline.active = true;
            state.cmdline.input.clear();
            None
        }
        Action::OpenSearch => {
            // The command line opens pre-filled with "/": typing continues
            // the live search, exactly as if `:` then `/` were pressed.
            state.cmdline.active = true;
            state.cmdline.input = "/".to_string();
            state.cmdline.live = true;
            state.search = SearchState::default();
            state.active_tab = Tab::Global;
            if !matches!(state.global.stack.last(), Some(GlobalView::Search { .. })) {
                state.global.stack.push(GlobalView::Search { cursor: 0 });
            }
            None
        }
        Action::Select => select_current(state),
        Action::Back if state.track_selection.is_active() => {
            state.track_selection.clear();
            state.status_message = Some("selection cleared".into());
            None
        }
        Action::Back => {
            go_back(state);
            None
        }
        Action::ToggleLike => {
            let tracks = selected_tracks(state);
            let track_ids: Vec<i64> = if tracks.is_empty() {
                state
                    .player
                    .current
                    .as_ref()
                    .map(|track| vec![track.id])
                    .unwrap_or_default()
            } else {
                tracks.into_iter().map(|track| track.id).collect()
            };
            if track_ids.is_empty() {
                state.status_message = Some("no track selected".into());
                None
            } else {
                let should_like = track_ids.iter().any(|id| !state.likes.contains(id));
                let toggles: Vec<i64> = track_ids
                    .into_iter()
                    .filter(|id| state.likes.contains(id) != should_like)
                    .collect();
                state.status_message = Some(if should_like {
                    format!("liking {} track(s)", toggles.len())
                } else {
                    format!("removing like from {} track(s)", toggles.len())
                });
                Some(Effect::ToggleLikes { track_ids: toggles })
            }
        }
        Action::ToggleTrackSelection => {
            toggle_track_selection(state);
            None
        }
        Action::OpenTrackInfo => {
            let tracks = selected_tracks(state);
            if tracks.is_empty() {
                state.status_message = Some("no track selected".into());
            } else {
                state.popup = Some(super::state::Popup::TrackInfo {
                    tracks,
                    cursor: 0,
                    scroll: 0,
                });
            }
            None
        }
        Action::RemoveFromQueue => remove_selected_from_queue(state),
        Action::QueueAddNext => queue_add(state, true),
        Action::QueueAddLast => queue_add(state, false),
        Action::GoToRelease => {
            let track = selected_track(state).or_else(|| state.player.current.clone());
            match track {
                Some(track) => open_release_for_track(state, &track),
                None => state.status_message = Some("no track selected".into()),
            }
            None
        }
        Action::AddToPlaylist => {
            let track = selected_track(state).or_else(|| state.player.current.clone());
            match track {
                Some(track) => {
                    state.popup = Some(super::state::Popup::AddToPlaylist { track, cursor: 0 });
                }
                None => state.status_message = Some("no track selected".into()),
            }
            None
        }
        Action::NewPlaylist => {
            state.popup = Some(super::state::Popup::NewPlaylist {
                for_track: None,
                input: String::new(),
                busy: false,
            });
            None
        }
        Action::ClearQueue => {
            let had_tracks = !state.player.queue.is_empty();
            state.player.queue.clear();
            state.player.queue_pos = 0;
            state.player.current = None;
            state.player.playing = false;
            state.player.paused = false;
            state.player.prefetched_pos = None;
            state.player.original_order = None;
            state.queue_tab.cursor = 0;
            state.track_selection.clear();
            if had_tracks {
                state.status_message = Some("queue cleared".into());
                Some(Effect::StopPlayback)
            } else {
                None
            }
        }
        Action::EditSelected => {
            open_edit_popup(state);
            None
        }
        Action::DeleteSelected => delete_selected(state),
    }
}

/// `e`: open the metadata edit form for whatever is under the cursor —
/// an artist tile, a release, a track or a playlist.
fn open_edit_popup(state: &mut AppState) {
    use super::state::{EditField, EditTarget, Popup};

    if state.active_tab == Tab::Playlists && state.playlists.opened.is_none() {
        let card = match &state.playlists.list {
            Some(Loadable::Ready(list)) => list.get(state.playlists.selected).cloned(),
            _ => None,
        };
        let Some(card) = card else {
            return;
        };
        if card.kind == "likes" {
            state.status_message = Some("the Likes playlist cannot be edited".into());
            return;
        }
        state.popup = Some(Popup::Edit {
            target: EditTarget::Playlist(card.id),
            title: format!("Edit playlist — {}", card.title),
            fields: vec![EditField::new("Title", card.title.clone())],
            focus: 0,
            error: None,
        });
        return;
    }
    if state.active_tab == Tab::Global && state.global.stack.is_empty() {
        let Some(artist) = state.global.artists.get(state.global.selected).cloned() else {
            return;
        };
        state.popup = Some(artist_edit_popup(artist.id, &artist.name, artist.image_path));
        return;
    }
    if let Some(artist) = selected_search_artist(state) {
        state.popup = Some(artist_edit_popup(artist.id, &artist.name, artist.image_path));
        return;
    }
    if let Some(release) = selected_release_card(state) {
        state.popup = Some(Popup::Edit {
            target: EditTarget::Release(release.id),
            title: format!("Edit release — {}", release.title),
            fields: vec![
                EditField::new("Title", release.title.clone()),
                EditField::new("Type", release.release_type.clone()),
                EditField::new(
                    "Year",
                    release.year.map(|y| y.to_string()).unwrap_or_default(),
                ),
            ],
            focus: 0,
            error: None,
        });
        return;
    }
    if let Some(track) = selected_track(state).or_else(|| state.player.current.clone()) {
        state.popup = Some(track_edit_popup(&track));
        return;
    }
    state.status_message = Some("nothing to edit here".into());
}

fn artist_edit_popup(
    id: i64,
    name: &str,
    image_path: Option<String>,
) -> super::state::Popup {
    use super::state::{EditField, EditTarget, Popup};
    Popup::Edit {
        target: EditTarget::Artist(id),
        title: format!("Edit artist — {name}"),
        fields: vec![
            EditField::new("Name", name),
            EditField::new("Image path", image_path.unwrap_or_default()),
        ],
        focus: 0,
        error: None,
    }
}

fn track_edit_popup(track: &TrackItem) -> super::state::Popup {
    use super::state::{EditField, EditTarget, Popup};
    let join = |artists: &[crate::library::models::ArtistRef]| {
        artists
            .iter()
            .map(|a| a.name.clone())
            .collect::<Vec<_>>()
            .join("; ")
    };
    Popup::Edit {
        target: EditTarget::Track(track.id),
        title: format!("Edit track — {}", track.title),
        fields: vec![
            EditField::new("Title", track.title.clone()),
            EditField::new("Artists", join(&track.artists)),
            EditField::new("Featured", join(&track.featured_artists)),
            EditField::new(
                "Track #",
                track.track_number.map(|n| n.to_string()).unwrap_or_default(),
            ),
            EditField::new(
                "Disc #",
                track.disc_number.map(|n| n.to_string()).unwrap_or_default(),
            ),
        ],
        focus: 0,
        error: None,
    }
}

/// shift-d: delete whatever is under the cursor. Library entities ask for
/// confirmation; playlist/queue rows are removed directly.
fn delete_selected(state: &mut AppState) -> Option<Effect> {
    use super::state::{DeleteTarget, Popup};

    if state.active_tab == Tab::Queue {
        return remove_selected_from_queue(state);
    }
    if state.active_tab == Tab::Playlists {
        match state.playlists.opened {
            None => {
                let card = match &state.playlists.list {
                    Some(Loadable::Ready(list)) => list.get(state.playlists.selected).cloned(),
                    _ => None,
                };
                let card = card?;
                if card.kind == "likes" {
                    state.status_message = Some("the Likes playlist cannot be deleted".into());
                    return None;
                }
                state.popup = Some(Popup::ConfirmDelete {
                    target: DeleteTarget::Playlist(card.id),
                    label: format!("playlist \"{}\"", card.title),
                });
                return None;
            }
            Some(opened) => {
                let tracks = selected_tracks(state);
                if tracks.is_empty() {
                    state.status_message = Some("no track selected".into());
                    return None;
                }
                let track_ids: Vec<i64> = tracks.iter().map(|track| track.id).collect();
                state.track_selection.clear();
                if opened.id == super::state::LIKES_PLAYLIST_ID {
                    let liked: Vec<i64> = track_ids
                        .into_iter()
                        .filter(|id| state.likes.contains(id))
                        .collect();
                    state.status_message = Some(format!("removing {} like(s)", liked.len()));
                    return Some(Effect::ToggleLikes { track_ids: liked });
                }
                state.status_message =
                    Some(format!("removing {} track(s) from playlist", track_ids.len()));
                return Some(Effect::RemoveFromPlaylist {
                    playlist_id: opened.id,
                    track_ids,
                });
            }
        }
    }
    if state.active_tab != Tab::Global {
        return None;
    }
    if state.global.stack.is_empty() {
        let artist = state.global.artists.get(state.global.selected).cloned()?;
        state.popup = Some(Popup::ConfirmDelete {
            target: DeleteTarget::Artist(artist.id),
            label: format!("artist \"{}\" with all their releases and tracks", artist.name),
        });
        return None;
    }
    if let Some(artist) = selected_search_artist(state) {
        state.popup = Some(Popup::ConfirmDelete {
            target: DeleteTarget::Artist(artist.id),
            label: format!("artist \"{}\" with all their releases and tracks", artist.name),
        });
        return None;
    }
    if let Some(release) = selected_release_card(state) {
        state.popup = Some(Popup::ConfirmDelete {
            target: DeleteTarget::Release(release.id),
            label: format!("release \"{}\" with all its tracks", release.title),
        });
        return None;
    }
    if let Some(track) = selected_track(state) {
        state.popup = Some(Popup::ConfirmDelete {
            target: DeleteTarget::Track(track.id),
            label: format!("track \"{}\" (the audio file stays on disk)", track.title),
        });
    } else {
        state.status_message = Some("nothing to delete here".into());
    }
    None
}

/// The artist under the cursor in the search view, if any.
fn selected_search_artist(state: &AppState) -> Option<crate::library::models::ArtistCard> {
    if state.active_tab != Tab::Global {
        return None;
    }
    let Some(GlobalView::Search { cursor }) = state.global.stack.last() else {
        return None;
    };
    state.search.results.as_ref()?.artists.get(*cursor).cloned()
}

/// The release card under the cursor (artist view tiles/rows or search).
fn selected_release_card(state: &AppState) -> Option<crate::library::models::ReleaseCard> {
    if state.active_tab != Tab::Global {
        return None;
    }
    match state.global.stack.last()? {
        GlobalView::Artist { id, cursor } => match state.artist_views.get(id)? {
            Loadable::Ready(detail) => {
                let position = cursor.checked_sub(detail.top_tracks.len())?;
                let order = release_display_order(&detail.releases);
                order
                    .get(position)
                    .map(|&index| detail.releases[index].clone())
            }
            _ => None,
        },
        GlobalView::Search { cursor } => {
            let results = state.search.results.as_ref()?;
            let offset = cursor.checked_sub(results.artists.len())?;
            results.releases.get(offset).cloned()
        }
        GlobalView::Release { .. } => None,
    }
}

fn toggle_track_selection(state: &mut AppState) {
    let Some((scope, cursor, len)) = current_track_list_context(state) else {
        state.status_message = Some("track selection is unavailable here".into());
        return;
    };
    if len == 0 {
        state.status_message = Some("no tracks here".into());
        return;
    }
    if state.track_selection.is_active_for(&scope) {
        state.track_selection.clear();
        state.status_message = Some("selection cleared".into());
    } else {
        state.track_selection.start(scope, cursor);
        visual_selection_status(state);
    }
}

fn visual_selection_status(state: &mut AppState) {
    let Some((scope, _, len)) = current_track_list_context(state) else {
        return;
    };
    if let Some(indices) = state.track_selection.indices(&scope, len) {
        state.status_message = Some(format!("-- VISUAL LINE -- {} track(s)", indices.len()));
    }
}

fn refresh_track_selection_cursor(state: &mut AppState) {
    let Some((scope, cursor, _)) = current_track_list_context(state) else {
        state.track_selection.clear();
        return;
    };
    state.track_selection.set_cursor(scope, cursor);
    if state.track_selection.is_active() {
        visual_selection_status(state);
    }
}

fn set_track_scope_cursor(state: &mut AppState, scope: &TrackSelectionScope, value: usize) {
    match scope {
        TrackSelectionScope::ArtistTop(id) => {
            if matches!(
                state.global.stack.last(),
                Some(GlobalView::Artist { id: current, .. }) if current == id
            ) {
                set_view_cursor(state, value);
            }
        }
        TrackSelectionScope::ArtistFeatured(id) => {
            let Some(GlobalView::Artist { id: current, .. }) = state.global.stack.last() else {
                return;
            };
            if current != id {
                return;
            }
            let Some(Loadable::Ready(detail)) = state.artist_views.get(id) else {
                return;
            };
            let flat = detail.top_tracks.len() + detail.releases.len() + value;
            set_view_cursor(state, flat);
        }
        TrackSelectionScope::Release(id) => {
            if matches!(
                state.global.stack.last(),
                Some(GlobalView::Release { id: current, .. }) if current == id
            ) {
                set_view_cursor(state, value);
            }
        }
        TrackSelectionScope::Playlist(id) => {
            if let Some(opened) = &mut state.playlists.opened
                && opened.id == *id {
                    opened.cursor = value;
                }
        }
        TrackSelectionScope::Queue => {
            state.queue_tab.cursor = value;
        }
    }
}

fn current_track_list_context(state: &AppState) -> Option<(TrackSelectionScope, usize, usize)> {
    match state.active_tab {
        Tab::Global => match state.global.stack.last()? {
            GlobalView::Artist { id, cursor } => match state.artist_views.get(id)? {
                Loadable::Ready(detail) => {
                    let tracks = detail.top_tracks.len();
                    let releases = detail.releases.len();
                    if *cursor < tracks {
                        Some((TrackSelectionScope::ArtistTop(*id), *cursor, tracks))
                    } else {
                        let featured = cursor.checked_sub(tracks + releases)?;
                        (featured < detail.featured_tracks.len()).then_some((
                            TrackSelectionScope::ArtistFeatured(*id),
                            featured,
                            detail.featured_tracks.len(),
                        ))
                    }
                }
                _ => None,
            },
            GlobalView::Release { id, cursor } => match state.release_views.get(id)? {
                Loadable::Ready(detail) => Some((
                    TrackSelectionScope::Release(*id),
                    *cursor,
                    detail.tracks.len(),
                )),
                _ => None,
            },
            _ => None,
        },
        Tab::Playlists => {
            let opened = state.playlists.opened.as_ref()?;
            let len = playlist_tracks(state, opened.id)?.len();
            Some((TrackSelectionScope::Playlist(opened.id), opened.cursor, len))
        }
        Tab::Queue => Some((
            TrackSelectionScope::Queue,
            state.queue_tab.cursor,
            state.player.queue.len(),
        )),
        Tab::Federation | Tab::Logs => None,
    }
}

fn current_track_list(state: &AppState) -> Option<(TrackSelectionScope, usize, &[TrackItem])> {
    match state.active_tab {
        Tab::Global => match state.global.stack.last()? {
            GlobalView::Artist { id, cursor } => match state.artist_views.get(id)? {
                Loadable::Ready(detail) => {
                    let tracks = detail.top_tracks.len();
                    let releases = detail.releases.len();
                    if *cursor < tracks {
                        Some((
                            TrackSelectionScope::ArtistTop(*id),
                            *cursor,
                            &detail.top_tracks,
                        ))
                    } else {
                        let featured = cursor.checked_sub(tracks + releases)?;
                        (featured < detail.featured_tracks.len()).then_some((
                            TrackSelectionScope::ArtistFeatured(*id),
                            featured,
                            &detail.featured_tracks,
                        ))
                    }
                }
                _ => None,
            },
            GlobalView::Release { id, cursor } => match state.release_views.get(id)? {
                Loadable::Ready(detail) => {
                    Some((TrackSelectionScope::Release(*id), *cursor, &detail.tracks))
                }
                _ => None,
            },
            _ => None,
        },
        Tab::Playlists => {
            let opened = state.playlists.opened.as_ref()?;
            Some((
                TrackSelectionScope::Playlist(opened.id),
                opened.cursor,
                playlist_tracks(state, opened.id)?,
            ))
        }
        Tab::Queue => Some((
            TrackSelectionScope::Queue,
            state.queue_tab.cursor,
            &state.player.queue,
        )),
        Tab::Federation | Tab::Logs => None,
    }
}

pub fn selected_tracks(state: &AppState) -> Vec<TrackItem> {
    let Some((scope, cursor, tracks)) = current_track_list(state) else {
        return selected_track(state).into_iter().collect();
    };
    let indices = state
        .track_selection
        .indices(&scope, tracks.len())
        .unwrap_or_else(|| vec![cursor.min(tracks.len().saturating_sub(1))]);
    indices
        .into_iter()
        .filter_map(|index| tracks.get(index).cloned())
        .collect()
}

fn selected_queue_indices(state: &AppState) -> Vec<usize> {
    if state.active_tab != Tab::Queue || state.player.queue.is_empty() {
        return Vec::new();
    }
    state
        .track_selection
        .indices(&TrackSelectionScope::Queue, state.player.queue.len())
        .unwrap_or_else(|| vec![state.queue_tab.cursor.min(state.player.queue.len() - 1)])
}

fn remove_selected_from_queue(state: &mut AppState) -> Option<Effect> {
    let indices = selected_queue_indices(state);
    if indices.is_empty() {
        state.status_message = Some("queue is empty".into());
        return None;
    }
    let outcome = remove_queue_indices(state, &indices);
    state.status_message = Some(format!("removed {} track(s) from queue", indices.len()));
    Some(Effect::RemoveQueueIndices {
        indices,
        restart_paused: outcome.restart_paused,
        stop: outcome.stop,
    })
}

struct QueueRemovalOutcome {
    restart_paused: Option<bool>,
    stop: bool,
}

fn remove_queue_indices(state: &mut AppState, indices: &[usize]) -> QueueRemovalOutcome {
    let len = state.player.queue.len();
    let mut unique: Vec<usize> = indices
        .iter()
        .copied()
        .filter(|index| *index < len)
        .collect();
    unique.sort_unstable();
    unique.dedup();
    if unique.is_empty() {
        return QueueRemovalOutcome {
            restart_paused: None,
            stop: false,
        };
    }

    let old_queue_pos = state.player.queue_pos;
    let current_id = state.player.current.as_ref().map(|track| track.id);
    let current_removed = current_id.is_some_and(|id| {
        unique.iter().any(|index| {
            state
                .player
                .queue
                .get(*index)
                .is_some_and(|track| track.id == id)
        })
    });
    let removed_before_current = unique
        .iter()
        .filter(|index| **index < old_queue_pos)
        .count();
    let was_loaded = state.player.playing;
    let was_paused = state.player.paused;

    for index in unique.iter().rev() {
        state.player.queue.remove(*index);
    }
    state.player.prefetched_pos = None;
    state.track_selection.clear();

    if state.player.queue.is_empty() {
        state.player = super::state::PlayerBar::default();
        state.queue_tab.cursor = 0;
        return QueueRemovalOutcome {
            restart_paused: None,
            stop: true,
        };
    }

    if current_removed {
        let desired = old_queue_pos.saturating_sub(removed_before_current);
        state.player.queue_pos = desired.min(state.player.queue.len() - 1);
        state.player.current = state.player.queue.get(state.player.queue_pos).cloned();
        state.player.position_secs = 0.0;
        state.player.track_started_at = None;
        state.queue_tab.cursor = state.queue_tab.cursor.min(state.player.queue.len() - 1);
        return QueueRemovalOutcome {
            restart_paused: was_loaded.then_some(was_paused),
            stop: false,
        };
    }

    if let Some(id) = current_id {
        if let Some(position) = state.player.queue.iter().position(|track| track.id == id) {
            state.player.queue_pos = position;
        }
    } else {
        state.player.queue_pos = state.player.queue_pos.min(state.player.queue.len() - 1);
    }
    state.player.current = current_id.and_then(|id| {
        state
            .player
            .queue
            .iter()
            .find(|track| track.id == id)
            .cloned()
    });
    state.queue_tab.cursor = state.queue_tab.cursor.min(state.player.queue.len() - 1);
    QueueRemovalOutcome {
        restart_paused: None,
        stop: false,
    }
}

/// The track under the cursor in whatever view is showing tracks.
pub fn selected_track(state: &AppState) -> Option<TrackItem> {
    match state.active_tab {
        Tab::Global => match state.global.stack.last()? {
            GlobalView::Artist { id, cursor } => match state.artist_views.get(id)? {
                Loadable::Ready(detail) => {
                    let tracks = detail.top_tracks.len();
                    if *cursor < tracks {
                        detail.top_tracks.get(*cursor).cloned()
                    } else {
                        cursor
                            .checked_sub(tracks + detail.releases.len())
                            .and_then(|i| detail.featured_tracks.get(i).cloned())
                    }
                }
                _ => None,
            },
            GlobalView::Release { id, cursor } => match state.release_views.get(id)? {
                Loadable::Ready(detail) => detail.tracks.get(*cursor).cloned(),
                _ => None,
            },
            GlobalView::Search { cursor } => {
                let results = state.search.results.as_ref()?;
                let offset = cursor.checked_sub(results.artists.len() + results.releases.len())?;
                results.tracks.get(offset).cloned()
            }
        },
        Tab::Playlists => {
            let opened = state.playlists.opened.as_ref()?;
            playlist_tracks(state, opened.id)?
                .get(opened.cursor)
                .cloned()
        }
        Tab::Queue => state.player.queue.get(state.queue_tab.cursor).cloned(),
        Tab::Federation | Tab::Logs => None,
    }
}

/// Tracks backing an opened playlist, if loaded.
pub fn playlist_tracks(state: &AppState, id: i64) -> Option<&Vec<TrackItem>> {
    match state.playlist_views.get(&id)? {
        Loadable::Ready(detail) => Some(&detail.tracks),
        _ => None,
    }
}

/// A *release* under the cursor (artist-view tile/row or a search release).
fn selected_release_id(state: &AppState) -> Option<i64> {
    if state.active_tab != Tab::Global {
        return None;
    }
    match state.global.stack.last()? {
        GlobalView::Artist { id, cursor } => match state.artist_views.get(id)? {
            Loadable::Ready(detail) => {
                let position = cursor.checked_sub(detail.top_tracks.len())?;
                let order = release_display_order(&detail.releases);
                order.get(position).map(|&i| detail.releases[i].id)
            }
            _ => None,
        },
        GlobalView::Search { cursor } => {
            let results = state.search.results.as_ref()?;
            let offset = cursor.checked_sub(results.artists.len())?;
            results.releases.get(offset).map(|r| r.id)
        }
        GlobalView::Release { .. } => None,
    }
}

/// a / shift-a: queue the selection — a single track directly, a release via
/// an async fetch effect.
fn queue_add(state: &mut AppState, next: bool) -> Option<Effect> {
    let tracks = selected_tracks(state);
    if !tracks.is_empty() {
        let count = tracks.len();
        let title = tracks[0].title.clone();
        enqueue_tracks(state, tracks, next);
        state.status_message = Some(if count == 1 && next {
            format!("queued next: {title}")
        } else if count == 1 {
            format!("queued: {title}")
        } else if next {
            format!("queued next: {count} tracks")
        } else {
            format!("queued: {count} tracks")
        });
        return None;
    }
    if let Some(id) = selected_release_id(state) {
        return Some(Effect::EnqueueRelease { id, next });
    }
    state.status_message = Some("nothing to queue here".into());
    None
}

/// Shift-J: open the release the track belongs to, with the cursor on that
/// track. If the release view is still loading, the focus is applied when
/// it arrives (`pending_release_focus`).
fn open_release_for_track(state: &mut AppState, track: &TrackItem) {
    let release_id = track.release_id;
    let cursor = match state.release_views.get(&release_id) {
        Some(Loadable::Ready(detail)) => detail
            .tracks
            .iter()
            .position(|t| t.id == track.id)
            .unwrap_or(0),
        _ => {
            state.pending_release_focus = Some((release_id, track.id));
            0
        }
    };
    let origin = state.active_tab;
    state.active_tab = Tab::Global;
    match state.global.stack.last_mut() {
        Some(GlobalView::Release {
            id,
            cursor: current,
        }) if *id == release_id => {
            *current = cursor;
        }
        _ => state.global.stack.push(GlobalView::Release {
            id: release_id,
            cursor,
        }),
    }
    // Jumps from another tab return there on Esc; jumps within Global
    // unwind the navigation stack as usual.
    if origin != Tab::Global {
        state.jump_origin = Some((origin, state.global.stack.len() - 1));
    }
}

/// Insert tracks after the playing one (`next`) or at the end. Keeps the
/// gapless prefetch index pointing at the same track if items shift.
pub fn enqueue_tracks(state: &mut AppState, tracks: Vec<TrackItem>, next: bool) {
    let player = &mut state.player;
    if tracks.is_empty() {
        return;
    }
    let insert_at = if next && !player.queue.is_empty() {
        (player.queue_pos + 1).min(player.queue.len())
    } else if next {
        0
    } else {
        player.queue.len()
    };
    let count = tracks.len();
    for (offset, track) in tracks.into_iter().enumerate() {
        player.queue.insert(insert_at + offset, track);
    }
    if let Some(prefetched) = &mut player.prefetched_pos
        && insert_at <= *prefetched {
            *prefetched += count;
        }
    if insert_at <= player.queue_pos && player.current.is_some() {
        player.queue_pos += count;
    }
}

/// Manual queue navigation (n / p); the tail is pre-shuffled when shuffle
/// is on, so stepping is always sequential.
fn queue_step(state: &mut AppState, direction: isize) -> Option<Effect> {
    let player = &mut state.player;
    if player.queue.is_empty() {
        state.status_message = Some("queue is empty".into());
        return None;
    }
    let len = player.queue.len();
    let next = player.queue_pos as isize + direction;
    if next < 0 {
        player.queue_pos = 0;
    } else if next >= len as isize {
        if player.repeat == super::state::RepeatMode::All {
            player.queue_pos = 0;
        } else {
            state.status_message = Some("end of queue".into());
            return None;
        }
    } else {
        player.queue_pos = next as usize;
    }
    Some(Effect::PlayCurrent)
}

/// What plays after the current track, without mutating anything — used to
/// pick the gapless prefetch target. Mirrors `advance_after_finish`.
/// Shuffle needs no special case: the queue tail is already shuffled.
pub fn peek_next_pos(player: &super::state::PlayerBar) -> Option<usize> {
    if player.queue.is_empty() {
        return None;
    }
    match player.repeat {
        super::state::RepeatMode::One => Some(player.queue_pos),
        repeat => {
            if player.queue_pos + 1 < player.queue.len() {
                Some(player.queue_pos + 1)
            } else if repeat == super::state::RepeatMode::All {
                Some(0)
            } else {
                None
            }
        }
    }
}

/// The current track finished: play the next queue position (the tail is
/// pre-shuffled when shuffle is on), or stop at the end.
pub fn advance_after_finish(state: &mut AppState) -> Option<Effect> {
    let player = &mut state.player;
    if player.queue.is_empty() {
        player.playing = false;
        player.current = None;
        return None;
    }
    match player.repeat {
        super::state::RepeatMode::One => Some(Effect::PlayCurrent),
        repeat => {
            if player.queue_pos + 1 < player.queue.len() {
                player.queue_pos += 1;
                Some(Effect::PlayCurrent)
            } else if repeat == super::state::RepeatMode::All {
                player.queue_pos = 0;
                Some(Effect::PlayCurrent)
            } else {
                player.playing = false;
                player.paused = false;
                Some(Effect::StopPlayback)
            }
        }
    }
}

/// First index of the not-yet-played queue tail: everything after the
/// current track, or from the current position when nothing is loaded.
fn upcoming_start(player: &super::state::PlayerBar) -> usize {
    if player.current.is_some() {
        (player.queue_pos + 1).min(player.queue.len())
    } else {
        player.queue_pos.min(player.queue.len())
    }
}

/// Remember the original order and Fisher-Yates the unplayed tail.
pub fn shuffle_upcoming(player: &mut super::state::PlayerBar) {
    if player.queue.is_empty() {
        return;
    }
    if player.original_order.is_none() {
        player.original_order = Some(player.queue.iter().map(|t| t.id).collect());
    }
    shuffle_range(player, upcoming_start(player));
}

/// Put the unplayed tail back into pre-shuffle order. Tracks queued while
/// shuffled (absent from the snapshot) keep their relative order at the end.
pub fn restore_queue_order(player: &mut super::state::PlayerBar) {
    let Some(order) = player.original_order.take() else {
        return;
    };
    let start = upcoming_start(player);
    if start >= player.queue.len() {
        return;
    }
    let tail = player.queue.split_off(start);
    let mut used = vec![false; order.len()];
    let mut keyed: Vec<(usize, usize, crate::library::models::TrackItem)> = tail
        .into_iter()
        .enumerate()
        .map(|(position, track)| {
            let key = order
                .iter()
                .enumerate()
                .position(|(slot, id)| !used[slot] && *id == track.id)
                .inspect(|&slot| used[slot] = true)
                .unwrap_or(usize::MAX);
            (key, position, track)
        })
        .collect();
    keyed.sort_by_key(|(key, position, _)| (*key, *position));
    player
        .queue
        .extend(keyed.into_iter().map(|(_, _, track)| track));
    player.prefetched_pos = None;
}

/// Columns of the Global tile grid. Derived from the terminal width the same
/// way ui::global does (full width minus the surrounding block's borders),
/// so selection math and rendering agree.
pub fn grid_columns() -> usize {
    let width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
    usize::from((width.saturating_sub(2) / TILE_WIDTH).max(1))
}

/// Lines of content visible in the main area (terminal height minus the tab
/// bar, status bar and the view's borders).
fn viewport_lines() -> isize {
    let height = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
    height.saturating_sub(5).max(1) as isize
}

/// One PageUp/PageDown step in MoveUp/MoveDown units for the current view:
/// move_selection() multiplies vertical steps by the column count in tile
/// zones, so tile views page by visible tile rows, line views by visible
/// lines.
fn page_step(state: &AppState) -> isize {
    let lines = viewport_lines();
    if state.active_tab != Tab::Global {
        return lines;
    }
    let tile_rows = (lines / TILE_HEIGHT as isize).max(1);
    match state.global.stack.last() {
        None => match state.global.view {
            ViewMode::Tiles => tile_rows,
            ViewMode::Table => lines,
        },
        Some(GlobalView::Artist { id, cursor }) => {
            let in_release_tiles = match state.artist_views.get(id) {
                Some(Loadable::Ready(detail)) => {
                    *cursor >= detail.top_tracks.len()
                        && *cursor < detail.top_tracks.len() + detail.releases.len()
                }
                _ => false,
            };
            if in_release_tiles && state.global.view == ViewMode::Tiles {
                tile_rows
            } else {
                lines
            }
        }
        Some(GlobalView::Release { .. }) | Some(GlobalView::Search { .. }) => lines,
    }
}

fn move_selection(state: &mut AppState, dx: isize, dy: isize) {
    if state.active_tab == Tab::Federation {
        if dy != 0 {
            let last = super::state::FedRow::ALL.len() as isize - 1;
            state.federation.cursor =
                (state.federation.cursor as isize + dy).clamp(0, last) as usize;
        }
        return;
    }
    if state.active_tab == Tab::Logs {
        // The cursor anchors to an entry's seq, so freshly appended log
        // lines (including ones caused by this very keypress) don't shift
        // the selection.
        if dy != 0 {
            let level = super::state::LOG_LEVELS[state.logs.level_index];
            if let Some(buffer) = crate::config::logging::buffer() {
                let current = if state.logs.follow {
                    None
                } else {
                    state.logs.selected_seq
                };
                if let Some((seq, is_newest)) = buffer.move_selection(level, current, dy) {
                    state.logs.selected_seq = Some(seq);
                    state.logs.follow = is_newest && dy > 0;
                }
            }
        }
        return;
    }
    if state.track_selection.is_active()
        && dx == 0
        && let Some((scope, cursor, len)) = current_track_list_context(state)
    {
        if len == 0 {
            return;
        }
        let next = (cursor as isize + dy).clamp(0, len as isize - 1) as usize;
        set_track_scope_cursor(state, &scope, next);
        refresh_track_selection_cursor(state);
        return;
    }
    if state.active_tab == Tab::Playlists {
        let len = playlists_view_len(state);
        if len == 0 {
            return;
        }
        let last = len as isize - 1;
        match &mut state.playlists.opened {
            Some(opened) => {
                opened.cursor = (opened.cursor as isize + dy).clamp(0, last) as usize;
                refresh_track_selection_cursor(state);
            }
            None => {
                state.playlists.selected =
                    (state.playlists.selected as isize + dy).clamp(0, last) as usize;
                state.track_selection.clear();
            }
        }
        return;
    }
    if state.active_tab == Tab::Queue {
        let len = state.player.queue.len();
        if len == 0 {
            return;
        }
        state.queue_tab.cursor =
            (state.queue_tab.cursor as isize + dy).clamp(0, len as isize - 1) as usize;
        refresh_track_selection_cursor(state);
        return;
    }
    if state.active_tab != Tab::Global {
        return not_yet(state, "Navigation in this view");
    }
    match state.global.stack.last().copied() {
        None => {
            let global = &mut state.global;
            if global.artists.is_empty() {
                return;
            }
            let step = match global.view {
                ViewMode::Tiles => dx + dy * grid_columns() as isize,
                ViewMode::Table => dy,
            };
            let last = global.artists.len() as isize - 1;
            global.selected = (global.selected as isize + step).clamp(0, last) as usize;
            state.track_selection.clear();
        }
        Some(GlobalView::Artist { id, cursor }) => {
            let Some(Loadable::Ready(detail)) = state.artist_views.get(&id) else {
                return;
            };
            let tracks = detail.top_tracks.len();
            let releases = detail.releases.len();
            let featured = detail.featured_tracks.len();
            let total = tracks + releases + featured;
            if total == 0 {
                return;
            }
            let in_release_tiles = state.global.view == ViewMode::Tiles
                && cursor >= tracks
                && cursor < tracks + releases;
            let next = if !in_release_tiles {
                // List zones (top tracks, featured tracks; releases in
                // table mode): plain vertical steps cross zone boundaries
                // in flat order.
                (cursor as isize + dy).clamp(0, total as isize - 1) as usize
            } else {
                // Release tiles: move by visual rows (groups break rows),
                // keeping the column, so Up lands on the tile above.
                let rows = release_rows(&detail.releases, grid_columns());
                let position = cursor - tracks;
                let (row, column) = rows
                    .iter()
                    .enumerate()
                    .find_map(|(r, items)| {
                        items.iter().position(|p| *p == position).map(|c| (r, c))
                    })
                    .unwrap_or((0, 0));
                if dx != 0 {
                    let last = releases as isize - 1;
                    tracks + (position as isize + dx).clamp(0, last) as usize
                } else {
                    let target = row as isize + dy;
                    if target < 0 {
                        if tracks > 0 { tracks - 1 } else { cursor }
                    } else if target as usize >= rows.len() {
                        // Below the last release row: the featured section.
                        if featured > 0 {
                            tracks + releases
                        } else {
                            cursor
                        }
                    } else {
                        let items = &rows[target as usize];
                        tracks + items[column.min(items.len() - 1)]
                    }
                }
            };
            set_view_cursor(state, next);
            state.track_selection.clear();
        }
        Some(GlobalView::Release { id, cursor }) => {
            let Some(Loadable::Ready(detail)) = state.release_views.get(&id) else {
                return;
            };
            let total = detail.tracks.len() as isize;
            if total == 0 {
                return;
            }
            let next = (cursor as isize + dy).clamp(0, total - 1);
            set_view_cursor(state, next as usize);
            refresh_track_selection_cursor(state);
        }
        Some(GlobalView::Search { cursor }) => {
            let total = state.search.results.as_ref().map_or(0, |r| r.len()) as isize;
            if total == 0 {
                return;
            }
            let next = (cursor as isize + dy).clamp(0, total - 1);
            set_view_cursor(state, next as usize);
            state.track_selection.clear();
        }
    }
}

fn set_view_cursor(state: &mut AppState, value: usize) {
    if let Some(view) = state.global.stack.last_mut() {
        match view {
            GlobalView::Artist { cursor, .. }
            | GlobalView::Release { cursor, .. }
            | GlobalView::Search { cursor } => *cursor = value,
        }
    }
}

/// Items in the playlists tab's current view (list or opened playlist).
fn playlists_view_len(state: &AppState) -> usize {
    match &state.playlists.opened {
        Some(opened) => playlist_tracks(state, opened.id).map_or(0, Vec::len),
        None => match &state.playlists.list {
            Some(Loadable::Ready(list)) => list.len(),
            _ => 0,
        },
    }
}

fn current_view_len(state: &AppState) -> usize {
    if state.active_tab == Tab::Playlists {
        return playlists_view_len(state);
    }
    if state.active_tab == Tab::Queue {
        return state.player.queue.len();
    }
    if state.active_tab == Tab::Federation {
        return super::state::FedRow::ALL.len();
    }
    match state.global.stack.last() {
        None => state.global.artists.len(),
        Some(GlobalView::Artist { id, .. }) => match state.artist_views.get(id) {
            Some(Loadable::Ready(d)) => {
                d.top_tracks.len() + d.releases.len() + d.featured_tracks.len()
            }
            _ => 0,
        },
        Some(GlobalView::Release { id, .. }) => match state.release_views.get(id) {
            Some(Loadable::Ready(d)) => d.tracks.len(),
            _ => 0,
        },
        Some(GlobalView::Search { .. }) => {
            state.search.results.as_ref().map_or(0, |r| r.len()) + state.search.fed_tracks.len()
        }
    }
}

fn jump_selection(state: &mut AppState, first: bool) {
    if state.track_selection.is_active()
        && let Some((scope, _, len)) = current_track_list_context(state)
    {
        if len > 0 {
            let target = if first { 0 } else { len - 1 };
            set_track_scope_cursor(state, &scope, target);
            refresh_track_selection_cursor(state);
        }
        return;
    }
    if state.active_tab == Tab::Queue {
        let len = state.player.queue.len();
        if len > 0 {
            state.queue_tab.cursor = if first { 0 } else { len - 1 };
            refresh_track_selection_cursor(state);
        }
        return;
    }
    if state.active_tab == Tab::Federation {
        let last = super::state::FedRow::ALL.len() - 1;
        state.federation.cursor = if first { 0 } else { last };
        return;
    }
    if state.active_tab == Tab::Logs {
        if first {
            let level = super::state::LOG_LEVELS[state.logs.level_index];
            if let Some(buffer) = crate::config::logging::buffer()
                && let Some((seq, _)) = buffer.move_selection(level, None, isize::MIN) {
                    state.logs.selected_seq = Some(seq);
                    state.logs.follow = false;
                }
        } else {
            state.logs.follow = true;
            state.logs.selected_seq = None;
        }
        return;
    }
    if state.active_tab != Tab::Global && state.active_tab != Tab::Playlists {
        return not_yet(state, "Navigation in this view");
    }
    let len = current_view_len(state);
    if len == 0 {
        return;
    }
    let target = if first { 0 } else { len - 1 };
    if state.active_tab == Tab::Playlists {
        match &mut state.playlists.opened {
            Some(opened) => opened.cursor = target,
            None => state.playlists.selected = target,
        }
        refresh_track_selection_cursor(state);
    } else if state.global.stack.is_empty() {
        state.global.selected = target;
        state.track_selection.clear();
    } else {
        set_view_cursor(state, target);
        refresh_track_selection_cursor(state);
    }
}

/// Enter on the Playlists tab: open a playlist from the list, or play the
/// selected track with the playlist as the queue.
fn select_playlist(state: &mut AppState) -> Option<Effect> {
    match state.playlists.opened {
        Some(opened) => {
            let tracks = playlist_tracks(state, opened.id)?.clone();
            if tracks.is_empty() {
                return None;
            }
            state.player.queue = tracks;
            state.player.queue_pos = opened.cursor.min(state.player.queue.len() - 1);
            on_new_queue(state);
            Some(Effect::PlayCurrent)
        }
        None => {
            let id = match &state.playlists.list {
                Some(Loadable::Ready(list)) => list.get(state.playlists.selected)?.id,
                _ => return None,
            };
            state.playlists.opened = Some(OpenedPlaylist { id, cursor: 0 });
            None
        }
    }
}

/// Enter: drill down (grid → artist → release) or play the selected track
/// with its surrounding list as the queue.
fn select_current(state: &mut AppState) -> Option<Effect> {
    if state.active_tab == Tab::Playlists {
        return select_playlist(state);
    }
    // Logs: open the full, wrapped entry under the cursor.
    if state.active_tab == Tab::Logs {
        let level = super::state::LOG_LEVELS[state.logs.level_index];
        if let Some(buffer) = crate::config::logging::buffer() {
            let selected = if state.logs.follow {
                None
            } else {
                state.logs.selected_seq
            };
            if let Some(entry) = buffer.entry_at(level, selected) {
                state.popup = Some(super::state::Popup::LogDetail(entry));
            }
        }
        return None;
    }
    if state.active_tab == Tab::Federation {
        return federation_select(state);
    }
    // Queue: jump playback to the track under the cursor. Earlier tracks
    // stay in the queue as "played"; picking one of them just moves the
    // playing position back.
    if state.active_tab == Tab::Queue {
        if state.player.queue.is_empty() {
            return None;
        }
        state.player.queue_pos = state.queue_tab.cursor.min(state.player.queue.len() - 1);
        return Some(Effect::PlayCurrent);
    }
    if state.active_tab != Tab::Global {
        not_yet(state, "Navigation in this view");
        return None;
    }
    enum Outcome {
        Push(GlobalView),
        Play {
            tracks: Vec<crate::library::models::TrackItem>,
            start: usize,
        },
        PlayFed(crate::federation::FedTrack),
        Nothing,
    }
    let outcome = match state.global.stack.last().copied() {
        None => match state.global.artists.get(state.global.selected) {
            Some(artist) => Outcome::Push(GlobalView::Artist {
                id: artist.id,
                cursor: 0,
            }),
            None => Outcome::Nothing,
        },
        Some(GlobalView::Artist { id, cursor }) => match state.artist_views.get(&id) {
            Some(Loadable::Ready(detail)) => {
                let tracks = detail.top_tracks.len();
                let releases = detail.releases.len();
                if cursor < tracks {
                    Outcome::Play {
                        tracks: detail.top_tracks.clone(),
                        start: cursor,
                    }
                } else if cursor < tracks + releases {
                    let order = release_display_order(&detail.releases);
                    match order.get(cursor - tracks) {
                        Some(&original) => Outcome::Push(GlobalView::Release {
                            id: detail.releases[original].id,
                            cursor: 0,
                        }),
                        None => Outcome::Nothing,
                    }
                } else if detail
                    .featured_tracks
                    .get(cursor - tracks - releases)
                    .is_some()
                {
                    Outcome::Play {
                        tracks: detail.featured_tracks.clone(),
                        start: cursor - tracks - releases,
                    }
                } else {
                    Outcome::Nothing
                }
            }
            _ => Outcome::Nothing,
        },
        Some(GlobalView::Release { id, cursor }) => match state.release_views.get(&id) {
            Some(Loadable::Ready(detail)) if !detail.tracks.is_empty() => Outcome::Play {
                tracks: detail.tracks.clone(),
                start: cursor.min(detail.tracks.len() - 1),
            },
            _ => Outcome::Nothing,
        },
        Some(GlobalView::Search { cursor }) => match &state.search.results {
            Some(results) => {
                let artists = results.artists.len();
                let releases = results.releases.len();
                if cursor < artists {
                    Outcome::Push(GlobalView::Artist {
                        id: results.artists[cursor].id,
                        cursor: 0,
                    })
                } else if cursor < artists + releases {
                    Outcome::Push(GlobalView::Release {
                        id: results.releases[cursor - artists].id,
                        cursor: 0,
                    })
                } else if results.tracks.get(cursor - artists - releases).is_some() {
                    Outcome::Play {
                        tracks: results.tracks.clone(),
                        start: cursor - artists - releases,
                    }
                } else {
                    let fed_index = cursor - artists - releases - results.tracks.len();
                    match state.search.fed_tracks.get(fed_index) {
                        Some(fed) => Outcome::PlayFed(fed.clone()),
                        None => Outcome::Nothing,
                    }
                }
            }
            None => match state.search.fed_tracks.get(cursor) {
                Some(fed) => Outcome::PlayFed(fed.clone()),
                None => Outcome::Nothing,
            },
        },
    };
    match outcome {
        Outcome::Push(view) => {
            state.global.stack.push(view);
            None
        }
        Outcome::Play { tracks, start } => {
            state.player.queue = tracks;
            state.player.queue_pos = start;
            on_new_queue(state);
            Some(Effect::PlayCurrent)
        }
        Outcome::PlayFed(fed) => {
            state.status_message = Some(format!("federation: fetching \"{}\"…", fed.title));
            Some(Effect::FedPlay(fed))
        }
        Outcome::Nothing => None,
    }
}

/// Enter on the Federation tab: toggle switches, open text inputs, run
/// one-shot operations. The heavy lifting happens in perform_effect().
fn federation_select(state: &mut AppState) -> Option<Effect> {
    use super::state::{FedInputField, FedRow, Popup};
    match FedRow::ALL.get(state.federation.cursor)? {
        FedRow::Toggle => {
            let settings = &mut state.federation.settings;
            if !settings.enabled && settings.network_id.trim().is_empty() {
                state.popup = Some(Popup::FedInput {
                    field: FedInputField::NetworkId,
                    input: String::new(),
                });
                return None;
            }
            settings.enabled = !settings.enabled;
            Some(Effect::FedApplySettings)
        }
        FedRow::NetworkId => {
            state.popup = Some(Popup::FedInput {
                field: FedInputField::NetworkId,
                input: state.federation.settings.network_id.clone(),
            });
            None
        }
        FedRow::SaveOnListen => {
            state.federation.settings.save_on_listen = !state.federation.settings.save_on_listen;
            Some(Effect::FedApplySettings)
        }
        FedRow::SyncNow => Some(Effect::FedSyncNow),
        FedRow::ShowTicket => Some(Effect::FedShowTicket),
        FedRow::Connect => {
            state.popup = Some(Popup::FedInput {
                field: FedInputField::ConnectTicket,
                input: String::new(),
            });
            None
        }
    }
}

/// A freshly created play context: drop the stale pre-shuffle snapshot and,
/// if shuffle is on, shuffle everything after the chosen track right away.
pub(super) fn on_new_queue(state: &mut AppState) {
    let player = &mut state.player;
    player.original_order = None;
    if player.shuffle && !player.queue.is_empty() {
        player.original_order = Some(player.queue.iter().map(|t| t.id).collect());
        shuffle_range(player, (player.queue_pos + 1).min(player.queue.len()));
    }
}

fn shuffle_range(player: &mut super::state::PlayerBar, start: usize) {
    let tail = &mut player.queue[start..];
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        | 1;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for i in (1..tail.len()).rev() {
        tail.swap(i, next() as usize % (i + 1));
    }
    player.prefetched_pos = None;
}

/// Esc/Backspace: pop the navigation stack; leaving a search view resets the
/// search so the next `:/` starts clean.
fn go_back(state: &mut AppState) {
    state.track_selection.clear();
    match state.active_tab {
        Tab::Playlists => {
            state.playlists.opened = None;
        }
        Tab::Global => {
            // Esc on a view opened by Shift-J from another tab goes back to
            // that tab, not down the Global stack.
            if let Some((origin, depth)) = state.jump_origin
                && state.global.stack.len() == depth + 1 {
                    state.global.stack.pop();
                    state.jump_origin = None;
                    state.active_tab = origin;
                    return;
                }
            if let Some(popped) = state.global.stack.pop()
                && matches!(popped, GlobalView::Search { .. }) {
                    state.search = SearchState::default();
                }
        }
        _ => {}
    }
}

fn switch_tab(state: &mut AppState, tab: Tab) {
    state.active_tab = tab;
    state.help_visible = false;
    state.track_selection.clear();
    // Manually leaving a view cancels any pending Shift-J return path.
    state.jump_origin = None;
}

fn reset_tab(state: &mut AppState, tab: Tab) {
    state.track_selection.clear();
    match tab {
        Tab::Global => {
            if state
                .global
                .stack
                .iter()
                .any(|v| matches!(v, GlobalView::Search { .. }))
            {
                state.search = SearchState::default();
            }
            state.global.stack.clear();
        }
        Tab::Playlists => state.playlists.opened = None,
        Tab::Federation => state.federation.cursor = 0,
        Tab::Logs => {
            state.logs.follow = true;
            state.logs.selected_seq = None;
        }
        Tab::Queue => {}
    }
}

fn not_yet(state: &mut AppState, what: &str) {
    state.status_message = Some(format!("{what}: coming in a later milestone"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::models::{ArtistCard, ArtistDetail, TrackItem};

    fn with_artists(n: usize) -> AppState {
        let mut state = AppState::default();
        state.global.artists = (0..n)
            .map(|i| ArtistCard {
                id: i as i64,
                name: format!("artist {i}"),
                image_path: None,
                release_count: 1,
                track_count: 2,
            })
            .collect();
        state
    }

    fn test_track(id: i64) -> TrackItem {
        TrackItem {
            id,
            title: format!("t{id}"),
            track_number: None,
            disc_number: None,
            duration_seconds: 1.0,
            artists: vec![],
            featured_artists: vec![],
            release_id: 1,
            release_title: "r".into(),
            release_year: None,
            cover_path: None,
            file_path: format!("/s/{id}"),
            audio_format: None,
            audio_bitrate: None,
            audio_sample_rate: None,
            audio_bit_depth: None,
            file_size_bytes: None,
            play_count: 0,
        }
    }

    #[test]
    fn quit_needs_double_press() {
        let mut state = AppState::default();
        update(&mut state, Action::Quit);
        assert!(!state.should_quit);
        assert_eq!(state.status_message.as_deref(), Some(QUIT_CONFIRM_HINT));
        update(&mut state, Action::Quit);
        assert!(state.should_quit);
    }

    #[test]
    fn other_action_disarms_quit() {
        let mut state = AppState::default();
        update(&mut state, Action::Quit);
        update(&mut state, Action::NextTab);
        update(&mut state, Action::Quit);
        assert!(!state.should_quit);
    }

    #[test]
    fn expired_quit_confirmation_rearms() {
        let mut state = AppState::default();
        update(&mut state, Action::Quit);
        state.quit_armed_until = Some(Instant::now() - Duration::from_secs(1));
        update(&mut state, Action::Quit);
        assert!(!state.should_quit);
        assert_eq!(state.status_message.as_deref(), Some(QUIT_CONFIRM_HINT));
    }

    #[test]
    fn tab_cycling_wraps() {
        let mut state = AppState::default();
        update(&mut state, Action::PrevTab);
        assert_eq!(state.active_tab, Tab::Logs);
        update(&mut state, Action::NextTab);
        assert_eq!(state.active_tab, Tab::Global);
    }

    #[test]
    fn volume_clamps() {
        let mut state = AppState::default();
        for _ in 0..30 {
            update(&mut state, Action::VolumeUp);
        }
        assert_eq!(state.player.volume, 100);
        for _ in 0..30 {
            update(&mut state, Action::VolumeDown);
        }
        assert_eq!(state.player.volume, 0);
    }

    #[test]
    fn back_closes_help_first() {
        let mut state = AppState::default();
        update(&mut state, Action::ToggleHelp);
        assert!(state.help_visible);
        update(&mut state, Action::Back);
        assert!(!state.help_visible);
    }

    #[test]
    fn grid_movement_clamps_and_wraps_rows() {
        let mut state = with_artists(10);
        let cols = grid_columns();
        update(&mut state, Action::MoveDown);
        assert_eq!(state.global.selected, cols.min(9));
        update(&mut state, Action::MoveUp);
        assert_eq!(state.global.selected, 0);
        update(&mut state, Action::MoveLeft);
        assert_eq!(state.global.selected, 0);
        update(&mut state, Action::MoveRight);
        assert_eq!(state.global.selected, 1);
    }

    #[test]
    fn table_mode_moves_one_row() {
        let mut state = with_artists(10);
        state.global.view = ViewMode::Table;
        update(&mut state, Action::MoveDown);
        assert_eq!(state.global.selected, 1);
        // Left/right are meaningless in the table.
        update(&mut state, Action::MoveRight);
        assert_eq!(state.global.selected, 1);
    }

    #[test]
    fn page_down_moves_a_page_and_clamps() {
        let mut state = with_artists(200);
        let cols = grid_columns() as isize;
        let expected = (page_step(&state) * cols).min(199) as usize;
        update(&mut state, Action::PageDown);
        assert_eq!(state.global.selected, expected);
        update(&mut state, Action::PageUp);
        assert_eq!(state.global.selected, 0);

        state.global.view = ViewMode::Table;
        update(&mut state, Action::PageDown);
        assert_eq!(state.global.selected, page_step(&state).min(199) as usize);
    }

    #[test]
    fn jump_first_last() {
        let mut state = with_artists(10);
        update(&mut state, Action::SelectLast);
        assert_eq!(state.global.selected, 9);
        update(&mut state, Action::SelectFirst);
        assert_eq!(state.global.selected, 0);
    }

    #[test]
    fn artist_tiles_move_by_visual_rows_across_groups() {
        use crate::library::models::{ArtistDetail, ReleaseCard};

        let release = |id: i64, kind: &str| ReleaseCard {
            id,
            title: format!("r{id}"),
            release_type: kind.to_string(),
            year: None,
            cover_path: None,
            track_count: 1,
        };
        // columns = 3 in tests (no tty → 80 wide): albums rows [0,1,2],[3],
        // compilations row [4,5].
        let detail = ArtistDetail {
            id: 1,
            name: "a".into(),
            image_path: None,
            total_track_count: 0,
            total_play_count: 0,
            top_tracks: vec![],
            featured_tracks: vec![],
            releases: vec![
                release(10, "album"),
                release(11, "album"),
                release(12, "album"),
                release(13, "album"),
                release(14, "compilation"),
                release(15, "compilation"),
            ],
        };
        let mut state = AppState::default();
        state.artist_views.insert(1, Loadable::Ready(detail));
        state
            .global
            .stack
            .push(GlobalView::Artist { id: 1, cursor: 4 });

        // Up from the first compilation lands on the album row directly
        // above (position 3), not three flat items back.
        update(&mut state, Action::MoveUp);
        assert_eq!(
            state.global.stack.last(),
            Some(&GlobalView::Artist { id: 1, cursor: 3 })
        );
        // And back down returns to the compilation row, same column.
        update(&mut state, Action::MoveDown);
        assert_eq!(
            state.global.stack.last(),
            Some(&GlobalView::Artist { id: 1, cursor: 4 })
        );
        // Up from the second compilation clamps to the single tile above.
        state.global.stack.pop();
        state
            .global
            .stack
            .push(GlobalView::Artist { id: 1, cursor: 5 });
        update(&mut state, Action::MoveUp);
        assert_eq!(
            state.global.stack.last(),
            Some(&GlobalView::Artist { id: 1, cursor: 3 })
        );
    }

    #[test]
    fn select_opens_artist_and_back_returns() {
        let mut state = with_artists(3);
        state.global.selected = 2;
        update(&mut state, Action::Select);
        assert_eq!(
            state.global.stack.last(),
            Some(&GlobalView::Artist { id: 2, cursor: 0 })
        );
        update(&mut state, Action::Back);
        assert!(state.global.stack.is_empty());
        assert_eq!(state.global.selected, 2);
    }

    #[test]
    fn back_from_search_resets_search_state() {
        let mut state = AppState::default();
        state.global.stack.push(GlobalView::Search { cursor: 0 });
        state.search.query = "abc".to_string();
        update(&mut state, Action::Back);
        assert!(state.global.stack.is_empty());
        assert!(state.search.query.is_empty());
    }

    #[test]
    fn queue_advances_and_respects_repeat() {
        use crate::library::models::TrackItem;
        use crate::app::state::RepeatMode;

        let track = |id: i64| TrackItem {
            id,
            title: format!("t{id}"),
            track_number: None,
            disc_number: None,
            duration_seconds: 1.0,
            artists: vec![],
            featured_artists: vec![],
            release_id: 1,
            release_title: "r".into(),
            release_year: None,
            cover_path: None,
            file_path: format!("/api/player/stream/{id}"),
            audio_format: None,
            audio_bitrate: None,
            audio_sample_rate: None,
            audio_bit_depth: None,
            file_size_bytes: None,
            play_count: 0,
        };
        let mut state = AppState::default();
        state.player.queue = vec![track(1), track(2)];
        state.player.playing = true;

        // Track 1 finishes → play track 2.
        assert_eq!(advance_after_finish(&mut state), Some(Effect::PlayCurrent));
        assert_eq!(state.player.queue_pos, 1);
        // Last track, repeat off → stop.
        assert_eq!(advance_after_finish(&mut state), Some(Effect::StopPlayback));
        assert!(!state.player.playing);
        // Repeat all wraps to the start.
        state.player.playing = true;
        state.player.repeat = RepeatMode::All;
        assert_eq!(advance_after_finish(&mut state), Some(Effect::PlayCurrent));
        assert_eq!(state.player.queue_pos, 0);
        // Repeat one replays the same position.
        state.player.repeat = RepeatMode::One;
        assert_eq!(advance_after_finish(&mut state), Some(Effect::PlayCurrent));
        assert_eq!(state.player.queue_pos, 0);
    }

    #[test]
    fn same_tab_number_resets_to_root() {
        let mut state = with_artists(3);
        update(&mut state, Action::Select);
        assert!(!state.global.stack.is_empty());
        update(&mut state, Action::GoToTab(0));
        assert!(state.global.stack.is_empty());

        state.playlists.opened = Some(OpenedPlaylist {
            id: crate::app::state::LIKES_PLAYLIST_ID,
            cursor: 0,
        });
        update(&mut state, Action::GoToTab(1));
        assert_eq!(state.active_tab, Tab::Playlists);
        assert!(state.playlists.opened.is_some());
        update(&mut state, Action::GoToTab(1));
        assert!(state.playlists.opened.is_none());
    }

    #[test]
    fn queue_tab_select_and_clear() {
        use crate::library::models::TrackItem;
        let track = |id: i64| TrackItem {
            id,
            title: format!("t{id}"),
            track_number: None,
            disc_number: None,
            duration_seconds: 1.0,
            artists: vec![],
            featured_artists: vec![],
            release_id: 1,
            release_title: "r".into(),
            release_year: None,
            cover_path: None,
            file_path: format!("/s/{id}"),
            audio_format: None,
            audio_bitrate: None,
            audio_sample_rate: None,
            audio_bit_depth: None,
            file_size_bytes: None,
            play_count: 0,
        };
        let mut state = AppState {
            active_tab: Tab::Queue,
            ..AppState::default()
        };
        state.player.queue = vec![track(1), track(2), track(3)];
        state.player.queue_pos = 2;

        // Cursor moves independently; enter rewinds playback to that track
        // without dropping anything from the queue.
        update(&mut state, Action::MoveUp);
        update(&mut state, Action::MoveUp);
        assert_eq!(state.queue_tab.cursor, 0);
        assert_eq!(
            update(&mut state, Action::Select),
            Some(Effect::PlayCurrent)
        );
        assert_eq!(state.player.queue_pos, 0);
        assert_eq!(state.player.queue.len(), 3);

        assert_eq!(
            update(&mut state, Action::ClearQueue),
            Some(Effect::StopPlayback)
        );
        assert!(state.player.queue.is_empty());
        assert!(!state.player.playing);
    }

    #[test]
    fn visual_selection_removes_queue_range() {
        let mut state = AppState {
            active_tab: Tab::Queue,
            ..AppState::default()
        };
        state.player.queue = (1..=4).map(test_track).collect();
        state.queue_tab.cursor = 1;

        assert_eq!(update(&mut state, Action::ToggleTrackSelection), None);
        update(&mut state, Action::MoveDown);

        let selected: Vec<i64> = selected_tracks(&state)
            .into_iter()
            .map(|track| track.id)
            .collect();
        assert_eq!(selected, vec![2, 3]);
        assert_eq!(
            update(&mut state, Action::RemoveFromQueue),
            Some(Effect::RemoveQueueIndices {
                indices: vec![1, 2],
                restart_paused: None,
                stop: false,
            })
        );
        let remaining: Vec<i64> = state.player.queue.iter().map(|track| track.id).collect();
        assert_eq!(remaining, vec![1, 4]);
        assert!(!state.track_selection.is_active());
    }

    #[test]
    fn artist_top_track_selection_queues_all_selected_tracks() {
        let mut state = AppState::default();
        state
            .global
            .stack
            .push(GlobalView::Artist { id: 9, cursor: 0 });
        state.artist_views.insert(
            9,
            Loadable::Ready(ArtistDetail {
                id: 9,
                name: "artist".into(),
                image_path: None,
                total_track_count: 3,
                total_play_count: 0,
                top_tracks: (1..=3).map(test_track).collect(),
                releases: vec![],
                featured_tracks: vec![],
            }),
        );

        update(&mut state, Action::ToggleTrackSelection);
        update(&mut state, Action::MoveDown);
        assert_eq!(update(&mut state, Action::QueueAddLast), None,);
        let queued: Vec<i64> = state.player.queue.iter().map(|track| track.id).collect();
        assert_eq!(queued, vec![1, 2]);
    }

    #[test]
    fn removing_current_queue_track_requests_paused_restart() {
        let mut state = AppState {
            active_tab: Tab::Queue,
            ..AppState::default()
        };
        state.player.queue = (1..=3).map(test_track).collect();
        state.player.queue_pos = 1;
        state.queue_tab.cursor = 1;
        state.player.current = Some(test_track(2));
        state.player.playing = true;
        state.player.paused = true;

        assert_eq!(
            update(&mut state, Action::RemoveFromQueue),
            Some(Effect::RemoveQueueIndices {
                indices: vec![1],
                restart_paused: Some(true),
                stop: false,
            })
        );
        let remaining: Vec<i64> = state.player.queue.iter().map(|track| track.id).collect();
        assert_eq!(remaining, vec![1, 3]);
        assert_eq!(state.player.queue_pos, 1);
        assert_eq!(state.player.current.as_ref().map(|track| track.id), Some(3));
    }

    #[test]
    fn bulk_like_targets_only_tracks_that_need_toggle() {
        let mut state = AppState {
            active_tab: Tab::Queue,
            ..AppState::default()
        };
        state.player.queue = (1..=3).map(test_track).collect();
        state.likes.insert(1);

        update(&mut state, Action::ToggleTrackSelection);
        update(&mut state, Action::SelectLast);
        assert_eq!(
            update(&mut state, Action::ToggleLike),
            Some(Effect::ToggleLikes {
                track_ids: vec![2, 3],
            })
        );

        state.likes = [1, 2, 3].into_iter().collect();
        assert_eq!(
            update(&mut state, Action::ToggleLike),
            Some(Effect::ToggleLikes {
                track_ids: vec![1, 2, 3],
            })
        );
    }

    #[test]
    fn shuffle_reorders_tail_and_restores() {
        use crate::library::models::TrackItem;
        let track = |id: i64| TrackItem {
            id,
            title: format!("t{id}"),
            track_number: None,
            disc_number: None,
            duration_seconds: 1.0,
            artists: vec![],
            featured_artists: vec![],
            release_id: 1,
            release_title: "r".into(),
            release_year: None,
            cover_path: None,
            file_path: format!("/s/{id}"),
            audio_format: None,
            audio_bitrate: None,
            audio_sample_rate: None,
            audio_bit_depth: None,
            file_size_bytes: None,
            play_count: 0,
        };
        let mut state = AppState::default();
        state.player.queue = (1..=8).map(track).collect();
        state.player.queue_pos = 2;
        state.player.current = Some(track(3));

        update(&mut state, Action::ToggleShuffle);
        assert!(state.player.shuffle);
        // Played part and the current track stay in place.
        let ids: Vec<i64> = state.player.queue.iter().map(|t| t.id).collect();
        assert_eq!(&ids[..3], &[1, 2, 3]);
        // The tail is a permutation of the original tail.
        let mut tail = ids[3..].to_vec();
        tail.sort_unstable();
        assert_eq!(tail, vec![4, 5, 6, 7, 8]);

        update(&mut state, Action::ToggleShuffle);
        assert!(!state.player.shuffle);
        let restored: Vec<i64> = state.player.queue.iter().map(|t| t.id).collect();
        assert_eq!(restored, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(state.player.original_order.is_none());
    }

    #[test]
    fn shift_j_opens_release_from_queue() {
        use crate::library::models::{ReleaseDetail, TrackItem};
        let track = |id: i64, release_id: i64| TrackItem {
            id,
            title: format!("t{id}"),
            track_number: None,
            disc_number: None,
            duration_seconds: 1.0,
            artists: vec![],
            featured_artists: vec![],
            release_id,
            release_title: "r".into(),
            release_year: None,
            cover_path: None,
            file_path: format!("/s/{id}"),
            audio_format: None,
            audio_bitrate: None,
            audio_sample_rate: None,
            audio_bit_depth: None,
            file_size_bytes: None,
            play_count: 0,
        };
        let mut state = AppState {
            active_tab: Tab::Queue,
            ..AppState::default()
        };
        state.player.queue = vec![track(1, 7), track(2, 7)];
        state.queue_tab.cursor = 1;

        // Release not loaded yet → jump queued as pending focus.
        update(&mut state, Action::GoToRelease);
        assert_eq!(state.active_tab, Tab::Global);
        assert_eq!(
            state.global.stack.last(),
            Some(&GlobalView::Release { id: 7, cursor: 0 })
        );
        assert_eq!(state.pending_release_focus, Some((7, 2)));

        // Esc returns to the origin tab, not to the Global grid.
        update(&mut state, Action::Back);
        assert_eq!(state.active_tab, Tab::Queue);
        assert!(state.global.stack.is_empty());
        assert!(state.jump_origin.is_none());

        // With the release cached, the cursor lands on the track directly.
        state.global.stack.clear();
        state.pending_release_focus = None;
        state.release_views.insert(
            7,
            Loadable::Ready(ReleaseDetail {
                id: 7,
                title: "r".into(),
                release_type: "album".into(),
                year: None,
                cover_path: None,
                artists: vec![],
                tracks: vec![track(1, 7), track(2, 7)],
            }),
        );
        state.active_tab = Tab::Queue;
        update(&mut state, Action::GoToRelease);
        assert_eq!(
            state.global.stack.last(),
            Some(&GlobalView::Release { id: 7, cursor: 1 })
        );
        assert!(state.pending_release_focus.is_none());
    }

    #[test]
    fn view_toggle() {
        let mut state = AppState::default();
        update(&mut state, Action::ToggleViewMode);
        assert_eq!(state.global.view, ViewMode::Table);
        update(&mut state, Action::ToggleViewMode);
        assert_eq!(state.global.view, ViewMode::Tiles);
    }
}
