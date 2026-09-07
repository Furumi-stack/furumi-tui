use super::*;
use crate::library::models::{ArtistCard, ArtistDetail, TrackItem};

#[test]
fn additional_settings_preserve_main_navigation_and_child_dialog_parent() {
    use crate::app::state::{FedInputField, Popup, SettingsRow, additional_settings_rows};
    let mut state = AppState {
        active_tab: Tab::Federation,
        ..AppState::default()
    };
    let main_rows = settings_rows(&state);
    assert!(!main_rows.contains(&SettingsRow::MusicDirectory));
    assert!(!main_rows.contains(&SettingsRow::CheckUpdate));
    assert!(!main_rows.contains(&SettingsRow::VisualizationClock));
    state.settings_cursor = main_rows
        .iter()
        .position(|r| *r == SettingsRow::AdditionalSettings)
        .unwrap();
    let main_cursor = state.settings_cursor;
    update(&mut state, Action::Select);
    assert!(state.additional_settings_open);
    update(&mut state, Action::Select);
    assert!(matches!(
        state.popup,
        Some(Popup::FedInput {
            field: FedInputField::MusicDirectory,
            ..
        })
    ));
    // Child dialogs own their input; closing one leaves the parent window intact.
    state.popup = None;
    assert!(state.additional_settings_open);
    update(&mut state, Action::SelectLast);
    assert_eq!(
        state.additional_settings_cursor,
        additional_settings_rows(&state).len() - 1
    );
    update(&mut state, Action::MoveDown);
    assert_eq!(
        state.additional_settings_cursor,
        additional_settings_rows(&state).len() - 1
    );
    update(&mut state, Action::ToggleHelp);
    update(&mut state, Action::Back);
    assert!(!state.help_visible);
    assert!(state.additional_settings_open);
    update(&mut state, Action::Back);
    assert!(!state.additional_settings_open);
    assert!(!state.should_quit);
    assert_eq!(state.settings_cursor, main_cursor);
}

#[test]
fn manual_update_check_is_single_flight_and_disabled_after_install() {
    let mut state = AppState::default();
    state.additional_settings_cursor = crate::app::state::additional_settings_rows(&state)
        .iter()
        .position(|row| *row == crate::app::state::SettingsRow::CheckUpdate)
        .unwrap();
    assert_eq!(
        update_additional_settings(&mut state, Action::Select),
        Some(Effect::CheckUpdate)
    );
    assert!(state.updater.busy);
    assert_eq!(update_additional_settings(&mut state, Action::Select), None);
    state.updater.busy = false;
    state.updater.installed = true;
    assert_eq!(update_additional_settings(&mut state, Action::Select), None);
    state.updater.installed = false;
    state.additional_settings_cursor = crate::app::state::additional_settings_rows(&state)
        .iter()
        .position(|row| *row == crate::app::state::SettingsRow::InstallUpdate)
        .unwrap();
    assert_eq!(update_additional_settings(&mut state, Action::Select), None);
}

fn with_artists(n: usize) -> AppState {
    let mut state = AppState::default();
    state.global.artists = (0..n)
        .map(|i| ArtistCard {
            id: i as i64,
            name: format!("artist {i}"),
            image_path: None,
            release_count: 1,
            track_count: 2,
            availability: crate::library::models::Availability::Local,
        })
        .collect();
    state
}

#[test]
fn listening_history_popup_requests_a_background_load() {
    let mut state = AppState::default();
    assert_eq!(
        update(&mut state, Action::OpenListenHistory),
        Some(Effect::LoadListenHistory)
    );
    assert!(matches!(
        state.popup,
        Some(crate::app::state::Popup::ListenHistory { cursor: 0 })
    ));
    assert!(matches!(
        state.listen_history,
        Some(crate::app::state::Loadable::Loading)
    ));
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
        content_id: Some(format!("b3:{id:064x}")),
        audio_format: None,
        audio_bitrate: None,
        audio_sample_rate: None,
        audio_bit_depth: None,
        file_size_bytes: None,
        play_count: 0,
        fed: None,
    }
}

fn pending_fed_track(id: i64) -> TrackItem {
    crate::federation::pending_track(&crate::federation::FedTrack {
        item_id: format!("fed-{id}"),
        owner: "peer".into(),
        own: false,
        title: format!("remote-{id}"),
        artist_names: vec!["remote artist".into()],
        featured_artist_names: vec![],
        year: None,
        duration_seconds: Some(1),
        content_id: Some(format!("b3:{id:064x}")),
        release_title: Some("remote release".into()),
        track_number: None,
        disc_number: None,
    })
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
fn library_filters_popup_opens_on_library_screens() {
    let mut state = AppState::default();
    update(&mut state, Action::OpenLibraryFilters);
    assert!(matches!(
        state.popup,
        Some(crate::app::state::Popup::LibraryFilters { .. })
    ));

    state.popup = None;
    state.global.stack.push(GlobalView::Search { cursor: 0 });
    update(&mut state, Action::OpenLibraryFilters);
    assert!(matches!(
        state.popup,
        Some(crate::app::state::Popup::LibraryFilters { .. })
    ));

    state.popup = None;
    state.active_tab = Tab::Queue;
    update(&mut state, Action::OpenLibraryFilters);
    assert!(state.popup.is_none());
}

#[test]
fn source_mode_cycles_on_library_playlists_and_queue_tabs() {
    use crate::config::settings::LibrarySourceMode;

    let mut state = AppState::default();
    assert_eq!(
        update(&mut state, Action::CycleSourceMode),
        Some(Effect::SourceModeChanged)
    );
    assert_eq!(state.global.filters.source_mode, LibrarySourceMode::Local);

    state.active_tab = Tab::Playlists;
    assert_eq!(
        update(&mut state, Action::CycleSourceMode),
        Some(Effect::SourceModeChanged)
    );
    assert_eq!(state.global.filters.source_mode, LibrarySourceMode::My);

    state.active_tab = Tab::Queue;
    assert_eq!(
        update(&mut state, Action::CycleSourceMode),
        Some(Effect::SourceModeChanged)
    );
    assert_eq!(state.global.filters.source_mode, LibrarySourceMode::Global);

    state.active_tab = Tab::Federation;
    assert_eq!(update(&mut state, Action::CycleSourceMode), None);
    assert_eq!(state.global.filters.source_mode, LibrarySourceMode::Global);
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
        availability: crate::library::models::Availability::Local,
    };
    let columns = grid_columns();
    // The terminal size can be visible to tests. Build enough albums to
    // force a short second album row for whichever width this run has.
    let detail = ArtistDetail {
        id: 1,
        name: "a".into(),
        image_path: None,
        total_track_count: 0,
        total_play_count: 0,
        top_tracks: vec![],
        featured_tracks: vec![],
        releases: (0..=columns)
            .map(|index| release(10 + index as i64, "album"))
            .chain((0..2).map(|index| release(100 + index, "compilation")))
            .collect(),
    };
    let mut state = AppState::default();
    state.artist_views.insert(1, Loadable::Ready(detail));
    state.global.stack.push(GlobalView::Artist {
        id: 1,
        cursor: columns + 1,
    });

    // Up from the first compilation lands on the album row directly
    // above, not one flat grid-width jump back.
    update(&mut state, Action::MoveUp);
    assert_eq!(
        state.global.stack.last(),
        Some(&GlobalView::Artist {
            id: 1,
            cursor: columns
        })
    );
    // And back down returns to the compilation row, same column.
    update(&mut state, Action::MoveDown);
    assert_eq!(
        state.global.stack.last(),
        Some(&GlobalView::Artist {
            id: 1,
            cursor: columns + 1
        })
    );
    // Up from the second compilation clamps to the single tile above.
    state.global.stack.pop();
    state.global.stack.push(GlobalView::Artist {
        id: 1,
        cursor: columns + 2,
    });
    update(&mut state, Action::MoveUp);
    assert_eq!(
        state.global.stack.last(),
        Some(&GlobalView::Artist {
            id: 1,
            cursor: columns
        })
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
    use crate::app::state::RepeatMode;
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
        file_path: format!("/api/player/stream/{id}"),
        content_id: None,
        audio_format: None,
        audio_bitrate: None,
        audio_sample_rate: None,
        audio_bit_depth: None,
        file_size_bytes: None,
        play_count: 0,
        fed: None,
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
        content_id: None,
        audio_format: None,
        audio_bitrate: None,
        audio_sample_rate: None,
        audio_bit_depth: None,
        file_size_bytes: None,
        play_count: 0,
        fed: None,
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
fn local_mode_hides_pending_federation_tracks_from_playlists_and_playback() {
    let mut state = AppState {
        active_tab: Tab::Playlists,
        ..AppState::default()
    };
    state.global.filters.source_mode = crate::config::settings::LibrarySourceMode::Local;
    state.playlists.opened = Some(OpenedPlaylist { id: 7, cursor: 1 });
    state.playlist_views.insert(
        7,
        Loadable::Ready(crate::library::models::PlaylistDetail {
            id: 7,
            title: "mixed".into(),
            description: None,
            tracks: vec![test_track(1), pending_fed_track(2), test_track(3)],
        }),
    );

    assert_eq!(
        playlist_tracks(&state, 7)
            .unwrap()
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert_eq!(
        update(&mut state, Action::Select),
        Some(Effect::PlayCurrent)
    );
    assert_eq!(
        state
            .player
            .queue
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert_eq!(state.player.queue_pos, 1);
}

#[test]
fn network_modes_show_pending_federation_playlist_tracks() {
    let mut state = AppState::default();
    state.global.filters.source_mode = crate::config::settings::LibrarySourceMode::My;
    state.playlist_views.insert(
        7,
        Loadable::Ready(crate::library::models::PlaylistDetail {
            id: 7,
            title: "mixed".into(),
            description: None,
            tracks: vec![test_track(1), pending_fed_track(2)],
        }),
    );

    assert_eq!(playlist_tracks(&state, 7).unwrap().len(), 2);
}

#[test]
fn local_mode_rejects_async_federation_queue_additions() {
    let mut state = AppState::default();
    state.global.filters.source_mode = crate::config::settings::LibrarySourceMode::Local;

    enqueue_tracks(
        &mut state,
        vec![test_track(1), pending_fed_track(2), test_track(3)],
        false,
    );

    assert_eq!(
        state
            .player
            .queue
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
}

#[test]
fn switching_to_local_mode_removes_pending_federation_queue_tracks() {
    let mut state = AppState::default();
    state.global.filters.source_mode = crate::config::settings::LibrarySourceMode::My;
    state.player.queue = vec![test_track(1), pending_fed_track(2), test_track(3)];
    state.player.queue_pos = 1;
    state.player.current = Some(state.player.queue[1].clone());
    state.player.playing = true;
    state.global.filters.source_mode = crate::config::settings::LibrarySourceMode::Local;

    let effect = apply_library_filter_change(&mut state);

    assert!(matches!(
        effect,
        Some(Effect::RemoveQueueIndices {
            indices,
            restart_paused: Some(false),
            stop: false,
        }) if indices == vec![1]
    ));
    assert_eq!(
        state
            .player
            .queue
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert_eq!(state.player.queue_pos, 1);
    assert_eq!(state.player.current.as_ref().map(|track| track.id), Some(3));
}

#[test]
fn current_track_info_uses_now_playing_track() {
    let mut state = AppState {
        active_tab: Tab::Queue,
        ..AppState::default()
    };
    let mut complete = test_track(2);
    complete.audio_format = Some("flac".into());
    complete.audio_bitrate = Some(921);
    state.player.queue = vec![test_track(1), complete];
    state.player.queue_pos = 1;
    state.queue_tab.cursor = 0;
    let mut lightweight = test_track(2);
    lightweight.file_path.clear();
    state.player.current = Some(lightweight);

    assert_eq!(update(&mut state, Action::OpenCurrentTrackInfo), None);
    match &state.popup {
        Some(crate::app::state::Popup::TrackInfo { tracks, .. }) => {
            assert_eq!(
                tracks.iter().map(|track| track.id).collect::<Vec<_>>(),
                vec![2]
            );
            assert_eq!(tracks[0].file_path, "/s/2");
            assert_eq!(tracks[0].audio_format.as_deref(), Some("flac"));
            assert_eq!(tracks[0].audio_bitrate, Some(921));
        }
        other => panic!("expected track info popup, got {other:?}"),
    }
}

#[test]
fn visualizer_requires_current_track() {
    let mut state = AppState::default();

    assert_eq!(update(&mut state, Action::ToggleVisualizer), None);

    assert!(!state.visualizer.active);
    assert_eq!(
        state.status_message.as_deref(),
        Some("nothing playing — start a track first")
    );
}

#[test]
fn visualizer_toggles_and_back_closes_it() {
    let mut state = AppState::default();
    state.player.current = Some(test_track(7));
    state.help_visible = true;

    assert_eq!(update(&mut state, Action::ToggleVisualizer), None);
    assert!(state.visualizer.active);
    assert!(state.visualizer.started_at.is_some());
    assert!(!state.help_visible);

    assert_eq!(update(&mut state, Action::Back), None);
    assert!(!state.visualizer.active);
    assert!(state.visualizer.started_at.is_none());
}

#[test]
fn add_to_playlist_from_release_carries_selected_track() {
    use crate::app::state::{PlaylistAddTarget, Popup};
    use crate::library::models::ReleaseDetail;

    let mut state = AppState::default();
    state
        .global
        .stack
        .push(GlobalView::Release { id: 1, cursor: 1 });
    state.release_views.insert(
        1,
        Loadable::Ready(ReleaseDetail {
            id: 1,
            title: "r".into(),
            release_type: "album".into(),
            year: None,
            cover_path: None,
            artists: vec![],
            tracks: vec![test_track(1), test_track(2)],
        }),
    );

    assert_eq!(update(&mut state, Action::AddToPlaylist), None);
    match &state.popup {
        Some(Popup::AddToPlaylist {
            target: PlaylistAddTarget::Local(tracks),
            ..
        }) => {
            assert_eq!(
                tracks.iter().map(|track| track.id).collect::<Vec<_>>(),
                vec![2]
            );
        }
        other => panic!("expected add-to-playlist popup with selected track, got {other:?}"),
    }
}

#[test]
fn add_to_playlist_from_non_track_view_uses_current_track() {
    use crate::app::state::{PlaylistAddTarget, Popup};

    let mut state = AppState {
        active_tab: Tab::Logs,
        ..AppState::default()
    };
    state.player.current = Some(test_track(7));

    assert_eq!(update(&mut state, Action::AddToPlaylist), None);
    match &state.popup {
        Some(Popup::AddToPlaylist {
            target: PlaylistAddTarget::Local(tracks),
            ..
        }) => {
            assert_eq!(
                tracks.iter().map(|track| track.id).collect::<Vec<_>>(),
                vec![7]
            );
        }
        other => panic!("expected add-to-playlist popup with current track, got {other:?}"),
    }
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
    assert_eq!(
        update(&mut state, Action::QueueAddLast),
        Some(Effect::QueueOrderChanged {
            restart_current: false,
        }),
    );
    let queued: Vec<i64> = state.player.queue.iter().map(|track| track.id).collect();
    assert_eq!(queued, vec![1, 2]);
    assert!(!state.track_selection.is_active());
}

#[test]
fn sequential_queue_next_additions_keep_their_order_as_one_block() {
    let mut state = AppState::default();
    state.player.queue = (10..=13).map(test_track).collect();
    state.player.queue_pos = 0;
    state.player.current = Some(test_track(10));

    assert!(!enqueue_tracks(&mut state, vec![test_track(1)], true));
    assert!(!enqueue_tracks(&mut state, vec![test_track(2)], true));
    assert!(!enqueue_tracks(&mut state, vec![test_track(3)], true));

    assert_eq!(
        state
            .player
            .queue
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>(),
        vec![10, 1, 2, 3, 11, 12, 13]
    );
    assert_eq!(state.player.play_next_end, Some(4));
}

#[test]
fn queue_selection_moves_as_a_group_and_preserves_current_track() {
    let mut state = AppState {
        active_tab: Tab::Queue,
        ..AppState::default()
    };
    state.player.queue = (1..=5).map(test_track).collect();
    state.player.queue_pos = 0;
    state.player.current = Some(test_track(1));
    state.queue_tab.cursor = 2;
    state.track_selection.start(TrackSelectionScope::Queue, 2);
    state
        .track_selection
        .set_cursor(TrackSelectionScope::Queue, 3);

    assert_eq!(
        update(&mut state, Action::MoveQueueUp),
        Some(Effect::QueueOrderChanged {
            restart_current: false,
        })
    );
    assert_eq!(
        state
            .player
            .queue
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>(),
        vec![1, 3, 4, 2, 5]
    );
    assert_eq!(state.player.queue_pos, 0);
    assert_eq!(state.queue_tab.cursor, 1);
    assert_eq!(
        state
            .track_selection
            .indices(&TrackSelectionScope::Queue, 5),
        Some(vec![1, 2])
    );
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
    state.likes.insert(format!("b3:{:064x}", 1));

    update(&mut state, Action::ToggleTrackSelection);
    update(&mut state, Action::SelectLast);
    assert_eq!(
        update(&mut state, Action::ToggleLike),
        Some(Effect::ToggleLikes {
            track_ids: vec![2, 3],
            fed_tracks: vec![],
        })
    );

    state.likes = [1, 2, 3]
        .into_iter()
        .map(|id| format!("b3:{id:064x}"))
        .collect();
    assert_eq!(
        update(&mut state, Action::ToggleLike),
        Some(Effect::ToggleLikes {
            track_ids: vec![1, 2, 3],
            fed_tracks: vec![],
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
        content_id: None,
        audio_format: None,
        audio_bitrate: None,
        audio_sample_rate: None,
        audio_bit_depth: None,
        file_size_bytes: None,
        play_count: 0,
        fed: None,
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
        content_id: None,
        audio_format: None,
        audio_bitrate: None,
        audio_sample_rate: None,
        audio_bit_depth: None,
        file_size_bytes: None,
        play_count: 0,
        fed: None,
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
