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
        podcast: None,
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
fn tabs_follow_the_documented_order() {
    use crate::config::keymap::KeyContext;

    assert_eq!(
        Tab::ALL,
        [
            Tab::Global,
            Tab::Playlists,
            Tab::Queue,
            Tab::Podcasts,
            Tab::Federation,
            Tab::Logs,
        ]
    );
    let titles: Vec<_> = Tab::ALL.iter().map(|tab| tab.title()).collect();
    assert_eq!(
        titles,
        [
            "Library",
            "Playlists",
            "Queue",
            "Podcasts",
            "Settings",
            "Logs"
        ]
    );
    for (index, tab) in Tab::ALL.iter().enumerate() {
        assert_eq!(tab.index(), index);
        assert_eq!(Tab::from_index(index), Some(*tab));
    }
    assert_eq!(Tab::from_index(Tab::ALL.len()), None);
    assert_eq!(Tab::Podcasts.key_context(), KeyContext::Podcasts);
    assert_eq!(Tab::Federation.key_context(), KeyContext::Federation);
}

#[test]
fn next_and_prev_tab_visit_every_tab_in_order() {
    let mut state = AppState::default();
    for expected in Tab::ALL.iter().cycle().skip(1).take(Tab::ALL.len()) {
        update(&mut state, Action::NextTab);
        assert_eq!(state.active_tab, *expected);
    }
    assert_eq!(state.active_tab, Tab::Global);
    for expected in Tab::ALL.iter().rev() {
        update(&mut state, Action::PrevTab);
        assert_eq!(state.active_tab, *expected);
    }
    assert_eq!(state.active_tab, Tab::Global);
}

#[test]
fn go_to_tab_uses_the_new_numbering() {
    let mut state = AppState::default();
    for (index, tab) in [
        (3, Tab::Podcasts),
        (4, Tab::Federation),
        (5, Tab::Logs),
        (2, Tab::Queue),
    ] {
        update(&mut state, Action::GoToTab(index));
        assert_eq!(state.active_tab, tab);
    }
    // Out-of-range indices are ignored.
    update(&mut state, Action::GoToTab(6));
    assert_eq!(state.active_tab, Tab::Queue);
    // Re-selecting the placeholder tab is a harmless reset.
    update(&mut state, Action::GoToTab(3));
    update(&mut state, Action::GoToTab(3));
    assert_eq!(state.active_tab, Tab::Podcasts);
}

#[test]
fn help_names_tabs_for_go_to_tab() {
    let labels: Vec<_> = (0..Tab::ALL.len())
        .map(|index| Action::GoToTab(index).describe())
        .collect();
    assert_eq!(
        labels,
        [
            "Go to Library",
            "Go to Playlists",
            "Go to Queue",
            "Go to Podcasts",
            "Go to Settings",
            "Go to Logs",
        ]
    );
    assert_eq!(Action::GoToTab(6).describe(), "Go to tab 7");
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
        podcast: None,
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
        podcast: None,
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
        podcast: None,
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
        podcast: None,
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

fn enrichment_state(release: bool) -> AppState {
    use crate::library::models::{ArtistRef, ReleaseDetail};
    let mut state = AppState::default();
    state.federation.settings.enabled = true;
    state.global.filters.source_mode = crate::config::settings::LibrarySourceMode::Global;
    state.artist_views.insert(
        1,
        Loadable::Ready(ArtistDetail {
            id: 1,
            name: "Artist".into(),
            image_path: None,
            total_track_count: 1,
            total_play_count: 0,
            top_tracks: vec![test_track(1)],
            featured_tracks: vec![],
            releases: vec![],
        }),
    );
    state.release_views.insert(
        1,
        Loadable::Ready(ReleaseDetail {
            id: 1,
            title: "Release".into(),
            release_type: "album".into(),
            year: None,
            cover_path: None,
            artists: vec![ArtistRef {
                id: 1,
                name: "Artist".into(),
            }],
            tracks: vec![test_track(1)],
        }),
    );
    state.global.stack.push(if release {
        GlobalView::Release { id: 1, cursor: 0 }
    } else {
        GlobalView::Artist { id: 1, cursor: 0 }
    });
    state
}

#[test]
fn card_enrichment_retries_startup_failure_without_reopening() {
    use crate::app::enrichment::{self, Card, Request};
    use std::time::{Duration, Instant};
    for release in [false, true] {
        let mut state = enrichment_state(release);
        let now = Instant::now();
        let request = enrichment::prepare(&mut state, now).unwrap();
        let card = match request {
            Request::Artist { id: 1, ref name } if name == "Artist" => Card::Artist(1),
            Request::Release {
                id: 1,
                ref title,
                ref artists,
            } if title == "Release" && artists == &["Artist"] => Card::Release(1),
            _ => panic!("wrong enrichment request: {request:?}"),
        };
        // Slow requests are never duplicated, even after the refresh interval.
        assert!(enrichment::prepare(&mut state, now + Duration::from_secs(60)).is_none());
        let completed = now + Duration::from_secs(60);
        state.card_enrichment.complete(card, completed);
        if release {
            enrichment::store_result(
                &mut state.release_fed_views,
                1,
                Err("federation is not running".into()),
            );
        } else {
            enrichment::store_result(
                &mut state.artist_fed_views,
                1,
                Err("federation is not running".into()),
            );
        }
        assert!(enrichment::prepare(&mut state, completed + Duration::from_secs(1)).is_none());
        assert_eq!(
            enrichment::prepare(&mut state, completed + Duration::from_secs(30)),
            Some(request)
        );
        assert!(matches!(
            state.artist_views.get(&1),
            Some(Loadable::Ready(_))
        ));
        assert!(matches!(
            state.release_views.get(&1),
            Some(Loadable::Ready(_))
        ));
    }
}

#[test]
fn card_enrichment_reopens_cached_cards_and_keeps_success_during_failed_refresh() {
    use crate::app::enrichment::{self, Card};
    use std::time::{Duration, Instant};
    for release in [false, true] {
        let mut state = enrichment_state(release);
        let now = Instant::now();
        let request = enrichment::prepare(&mut state, now).unwrap();
        state.card_enrichment.complete(
            if release {
                Card::Release(1)
            } else {
                Card::Artist(1)
            },
            now,
        );
        if release {
            enrichment::store_result(
                &mut state.release_fed_views,
                1,
                Ok(crate::federation::FedRelease {
                    title: "Release".into(),
                    ..Default::default()
                }),
            );
        } else {
            enrichment::store_result(
                &mut state.artist_fed_views,
                1,
                Ok(crate::federation::FedArtistCard {
                    name: "Artist".into(),
                    ..Default::default()
                }),
            );
        }
        // Successful but local-only/empty snapshots must also be refreshed.
        assert!(enrichment::prepare(&mut state, now).is_none());
        let view = state.global.stack.pop().unwrap();
        assert!(enrichment::prepare(&mut state, now).is_none());
        state.global.stack.push(view);
        assert_eq!(
            enrichment::prepare(&mut state, now + Duration::from_secs(1)),
            Some(request)
        );
        if release {
            assert!(matches!(
                state.release_fed_views.get(&1),
                Some(Loadable::Ready(_))
            ));
            enrichment::store_result(&mut state.release_fed_views, 1, Err("offline".into()));
            assert!(
                matches!(state.release_fed_views.get(&1), Some(Loadable::Ready(card)) if card.title == "Release")
            );
        } else {
            assert!(matches!(
                state.artist_fed_views.get(&1),
                Some(Loadable::Ready(_))
            ));
            enrichment::store_result(&mut state.artist_fed_views, 1, Err("offline".into()));
            assert!(
                matches!(state.artist_fed_views.get(&1), Some(Loadable::Ready(card)) if card.name == "Artist")
            );
        }
    }
}

#[test]
fn card_enrichment_respects_network_mode_and_deduplicates_navigation() {
    use crate::app::enrichment;
    use crate::config::settings::LibrarySourceMode;
    use std::time::Instant;
    for release in [false, true] {
        let mut state = enrichment_state(release);
        let now = Instant::now();
        state.federation.settings.enabled = false;
        assert!(enrichment::prepare(&mut state, now).is_none());
        state.federation.settings.enabled = true;
        state.global.filters.source_mode = LibrarySourceMode::Local;
        assert!(enrichment::prepare(&mut state, now).is_none());
        state.global.filters.source_mode = LibrarySourceMode::Global;
        assert!(enrichment::prepare(&mut state, now).is_some());
        state.active_tab = Tab::Queue;
        assert!(enrichment::prepare(&mut state, now).is_none());
        state.active_tab = Tab::Global;
        assert!(enrichment::prepare(&mut state, now).is_none());
    }
}

#[test]
fn card_enrichment_refreshes_local_only_success_while_still_open() {
    use crate::app::enrichment::{self, Card};
    use std::time::{Duration, Instant};
    for release in [false, true] {
        let mut state = enrichment_state(release);
        let now = Instant::now();
        let request = enrichment::prepare(&mut state, now).unwrap();
        state.card_enrichment.complete(
            if release {
                Card::Release(1)
            } else {
                Card::Artist(1)
            },
            now,
        );
        if release {
            enrichment::store_result(&mut state.release_fed_views, 1, Ok(Default::default()));
        } else {
            enrichment::store_result(&mut state.artist_fed_views, 1, Ok(Default::default()));
        }
        assert!(enrichment::prepare(&mut state, now + Duration::from_secs(29)).is_none());
        assert_eq!(
            enrichment::prepare(&mut state, now + Duration::from_secs(30)),
            Some(request)
        );
    }
}

fn render_enrichment_card(state: &AppState) -> String {
    use ratatui::{Terminal, backend::TestBackend};
    let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
    terminal
        .draw(|frame| crate::ui::global::draw(frame, frame.area(), state))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[test]
fn local_card_explains_source_filter_instead_of_waiting_for_a_request() {
    use crate::app::enrichment;
    let mut state = enrichment_state(false);
    state.global.filters.source_mode = crate::config::settings::LibrarySourceMode::Local;
    assert!(enrichment::prepare(&mut state, std::time::Instant::now()).is_none());
    let text = render_enrichment_card(&state);
    assert!(
        !text.contains("federation: pending"),
        "Local mode must not promise a request that is disabled"
    );
    assert!(
        text.contains("Local"),
        "the card must explain why network results are hidden"
    );
}

#[test]
fn card_refresh_animates_while_previous_results_stay_visible() {
    use crate::app::enrichment::{self, Card};
    use std::time::{Duration, Instant};
    for release in [false, true] {
        let mut state = enrichment_state(release);
        let now = Instant::now();
        assert!(enrichment::prepare(&mut state, now).is_some());
        let card = if release {
            Card::Release(1)
        } else {
            Card::Artist(1)
        };
        state.card_enrichment.complete(card, now);
        if release {
            enrichment::store_result(&mut state.release_fed_views, 1, Ok(Default::default()));
        } else {
            enrichment::store_result(&mut state.artist_fed_views, 1, Ok(Default::default()));
        }
        assert!(enrichment::prepare(&mut state, now + Duration::from_secs(30)).is_some());
        let first = render_enrichment_card(&state);
        assert!(first.contains(&format!("{} searching peers", state.spinner())));
        state.advance_spinner();
        let next = render_enrichment_card(&state);
        assert!(next.contains(&format!("{} searching peers", state.spinner())));
        assert_ne!(first, next);
        state
            .card_enrichment
            .complete(card, now + Duration::from_secs(31));
        assert!(!render_enrichment_card(&state).contains("searching peers"));
    }
}

// ---------------------------------------------------------------------------
// Podcasts tab

mod podcast_tab {
    use furumi_library::podcasts::{EpisodeSort, PlayedFilter, SeasonFilter};
    use music_dht::podcast::{FeedUrl, PodcastId, ProgressStatus};

    use super::*;
    use crate::app::podcast_fixtures::{self as fx, podcast_id};
    use crate::app::state::{
        FedInputField, PODCAST_ACTION_ROWS, PODCAST_EPISODE_PAGE, PodcastStoreStatus, Popup,
    };
    use crate::app::update::podcasts;
    use crate::podcasts::refresh::{RefreshReport, SubscribeReport};

    fn ready(rows: Vec<crate::app::state::PodcastRow>) -> AppState {
        let mut state = fx::ready_state(rows);
        state.podcasts.selected = PODCAST_ACTION_ROWS;
        state
    }

    fn other_id() -> PodcastId {
        PodcastId::from_feed_url(&FeedUrl::parse("https://other.test/feed.xml").unwrap())
    }

    fn episode_request(effect: Option<Effect>) -> (u64, furumi_library::podcasts::EpisodeQuery) {
        match effect {
            Some(Effect::PodcastLoadEpisodes { seq, query }) => (seq, query),
            other => panic!("expected an episode load, got {other:?}"),
        }
    }

    fn page(count: u32) -> Vec<furumi_library::podcasts::EpisodeListItem> {
        (1..=count)
            .map(|n| fx::episode(n, &format!("Episode {n}"), None))
            .collect()
    }

    #[test]
    fn podcast_actions_wait_for_the_store() {
        let mut state = AppState {
            active_tab: Tab::Podcasts,
            ..AppState::default()
        };
        assert_eq!(update(&mut state, Action::AddPodcast), None);
        assert!(state.popup.is_none());
        assert!(
            state
                .status_message
                .as_deref()
                .is_some_and(|m| m.contains("still opening"))
        );
        state.podcasts.store = PodcastStoreStatus::Failed("newer schema".into());
        assert_eq!(update(&mut state, Action::RefreshAllPodcasts), None);
        assert!(
            state
                .status_message
                .as_deref()
                .is_some_and(|m| m.contains("unavailable: newer schema"))
        );
        // Opening the store loads the list.
        assert_eq!(
            podcasts::store_opened(&mut state, Ok(())),
            Some(Effect::PodcastsReload)
        );
        assert_eq!(state.podcasts.store, PodcastStoreStatus::Ready);
    }

    #[test]
    fn podcast_actions_outside_the_tab_only_explain() {
        let mut state = fx::ready_state(vec![fx::row("Show", 0, 1, 0)]);
        state.active_tab = Tab::Global;
        assert_eq!(update(&mut state, Action::RefreshAllPodcasts), None);
        assert!(state.podcasts.refreshing.is_empty());
        assert!(
            state
                .status_message
                .as_deref()
                .is_some_and(|m| m.contains("Podcasts tab (4)"))
        );
    }

    #[test]
    fn podcast_opens_seasons_then_a_season_then_goes_back() {
        let mut state = ready(vec![fx::row("Show", 1, 5, 1)]);
        assert_eq!(
            update(&mut state, Action::Select),
            Some(Effect::PodcastLoadSeasons(podcast_id()))
        );
        assert!(!state.podcasts.showing_episodes());
        podcasts::seasons_loaded(&mut state, podcast_id(), Ok(fx::seasons()));
        // Rows: All, Season 1, Without a season.
        assert_eq!(state.podcasts.season_rows().len(), 3);
        update(&mut state, Action::MoveDown);
        let (seq, query) = episode_request(update(&mut state, Action::Select));
        assert_eq!(query.season, SeasonFilter::Season(1));
        assert_eq!(query.limit, Some(PODCAST_EPISODE_PAGE));
        assert_eq!(query.offset, 0);
        podcasts::episodes_loaded(&mut state, seq, 0, Ok(page(3)));
        update(&mut state, Action::MoveDown);
        assert_eq!(
            state
                .podcasts
                .selected_episode()
                .map(|item| item.episode.title.as_str()),
            Some("Episode 2")
        );

        // Esc: episodes -> seasons (cursor kept) -> list (on the podcast).
        update(&mut state, Action::Back);
        let opened = state.podcasts.opened.as_ref().unwrap();
        assert_eq!((opened.season, opened.season_cursor), (None, Some(1)));
        update(&mut state, Action::Back);
        assert!(state.podcasts.opened.is_none());
        assert_eq!(state.podcasts.selected, PODCAST_ACTION_ROWS);
        // A late seasons result for the closed podcast is ignored.
        podcasts::seasons_loaded(&mut state, podcast_id(), Ok(Vec::new()));
        assert!(state.podcasts.opened_seasons().is_none());
    }

    #[test]
    fn podcast_without_seasons_opens_its_episodes_and_tab_key_resets() {
        let mut state = ready(vec![fx::row("Show", 1, 2, 0)]);
        let (_, query) = episode_request(update(&mut state, Action::Select));
        assert_eq!(query.season, SeasonFilter::Any);
        assert!(state.podcasts.showing_episodes());
        // Pressing the tab's number again returns to the list.
        update(&mut state, Action::GoToTab(Tab::Podcasts.index()));
        assert!(state.podcasts.opened.is_none());
    }

    #[test]
    fn live_search_drops_stale_pages_and_keeps_the_rows_meanwhile() {
        let mut state = ready(vec![fx::row("Show", 1, 3, 0)]);
        let (first, _) = episode_request(update(&mut state, Action::Select));
        podcasts::episodes_loaded(&mut state, first, 0, Ok(page(3)));

        update(&mut state, Action::SearchEpisodes);
        assert!(matches!(
            state.popup,
            Some(Popup::FedInput {
                field: FedInputField::PodcastSearch,
                ..
            })
        ));
        let (second, query) = episode_request(podcasts::set_search(&mut state, "pilot".into()));
        assert_eq!(query.search.as_deref(), Some("pilot"));
        assert!(second > first);
        // The old rows stay visible while the search runs.
        let list = state.podcasts.opened_episodes().unwrap();
        assert!(list.loading);
        assert_eq!(list.items.len(), 3);
        // Typing on: the previous request's result arrives late and is dropped.
        let (third, _) = episode_request(podcasts::set_search(&mut state, "pilot 2".into()));
        podcasts::episodes_loaded(&mut state, second, 0, Ok(page(2)));
        assert_eq!(state.podcasts.opened_episodes().unwrap().items.len(), 3);
        podcasts::episodes_loaded(&mut state, third, 0, Ok(page(1)));
        let list = state.podcasts.opened_episodes().unwrap();
        assert_eq!(list.items.len(), 1);
        assert!(!list.loading && list.complete);
        // An unchanged search requests nothing.
        assert_eq!(podcasts::set_search(&mut state, "pilot 2".into()), None);
        // Search, filter and sort are per opened podcast view.
        update(&mut state, Action::Back);
        assert!(state.podcasts.search.is_empty());
    }

    #[test]
    fn sort_and_played_filter_reload_from_the_top() {
        let mut state = ready(vec![fx::row("Show", 1, 3, 0)]);
        let (seq, _) = episode_request(update(&mut state, Action::Select));
        podcasts::episodes_loaded(&mut state, seq, 0, Ok(page(3)));
        update(&mut state, Action::SelectLast);
        let (_, query) = episode_request(update(&mut state, Action::CycleEpisodeSort));
        assert_eq!(query.sort, EpisodeSort::OldestFirst);
        assert_eq!(state.podcasts.opened.as_ref().unwrap().episode_cursor, 0);
        let (_, query) = episode_request(update(&mut state, Action::CycleEpisodeFilter));
        assert_eq!(query.played, PlayedFilter::Unplayed);
        assert_eq!(query.sort, EpisodeSort::OldestFirst);
    }

    #[test]
    fn long_episode_lists_load_page_by_page() {
        let mut state = ready(vec![fx::row("Show", 600, 600, 0)]);
        let (seq, _) = episode_request(update(&mut state, Action::Select));
        podcasts::episodes_loaded(&mut state, seq, 0, Ok(page(PODCAST_EPISODE_PAGE as u32)));
        assert!(!state.podcasts.opened_episodes().unwrap().complete);
        let (more_seq, query) = episode_request(update(&mut state, Action::SelectLast));
        assert_eq!(more_seq, seq);
        assert_eq!(query.offset, PODCAST_EPISODE_PAGE);
        // No duplicate request while the page is in flight.
        assert_eq!(update(&mut state, Action::MoveUp), None);
        podcasts::episodes_loaded(&mut state, seq, PODCAST_EPISODE_PAGE, Ok(page(100)));
        let list = state.podcasts.opened_episodes().unwrap();
        assert_eq!(list.items.len(), PODCAST_EPISODE_PAGE + 100);
        assert!(list.complete);
        assert_eq!(update(&mut state, Action::SelectLast), None);
    }

    #[test]
    fn manual_and_scheduled_refreshes_are_deduplicated() {
        let mut state = ready(vec![fx::row("Show", 0, 1, 0)]);
        let url = FeedUrl::parse("https://example.test/feed.xml").unwrap();
        assert_eq!(
            update(&mut state, Action::RefreshPodcast),
            Some(Effect::PodcastRefresh {
                targets: vec![(podcast_id(), url.clone())],
                manual: true,
            })
        );
        assert!(state.podcasts.refreshing.contains(&podcast_id()));
        assert_eq!(update(&mut state, Action::RefreshPodcast), None);
        assert_eq!(update(&mut state, Action::RefreshAllPodcasts), None);
        // The scheduler skips feeds already in flight.
        let due = vec![(podcast_id(), url.clone()), (other_id(), url.clone())];
        assert_eq!(
            podcasts::refresh_due(&mut state, due),
            Some(Effect::PodcastRefresh {
                targets: vec![(other_id(), url)],
                manual: false,
            })
        );
        let effects = podcasts::refreshed(
            &mut state,
            podcast_id(),
            Ok(RefreshReport {
                new_episodes: 2,
                ..RefreshReport::default()
            }),
            true,
        );
        assert_eq!(effects, vec![Effect::PodcastsReload]);
        assert!(!state.podcasts.refreshing.contains(&podcast_id()));
        assert!(
            state
                .status_message
                .as_deref()
                .is_some_and(|m| m.contains("2 new episode(s) in \"Show\""))
        );
        // A background failure stays out of the status bar.
        state.status_message = None;
        podcasts::refreshed(&mut state, other_id(), Err("HTTP 500".into()), false);
        assert!(state.status_message.is_none());
    }

    #[test]
    fn refreshing_the_opened_podcast_reloads_it_on_the_same_episode() {
        let mut state = ready(vec![fx::row("Show", 0, 3, 0)]);
        let (seq, _) = episode_request(update(&mut state, Action::Select));
        podcasts::episodes_loaded(&mut state, seq, 0, Ok(page(3)));
        update(&mut state, Action::MoveDown);
        let effects = podcasts::refreshed(
            &mut state,
            podcast_id(),
            Ok(RefreshReport {
                new_episodes: 1,
                ..RefreshReport::default()
            }),
            false,
        );
        assert_eq!(effects.len(), 2);
        let (seq, _) = episode_request(effects.into_iter().nth(1));
        // A new episode arrived on top: the cursor follows "Episode 2".
        let mut rows = vec![fx::episode(9, "Newest", None)];
        rows.extend(page(3));
        podcasts::episodes_loaded(&mut state, seq, 0, Ok(rows));
        assert_eq!(
            state
                .podcasts
                .selected_episode()
                .map(|item| item.episode.title.as_str()),
            Some("Episode 2")
        );
    }

    #[test]
    fn adding_a_feed_validates_the_url_then_selects_the_new_podcast() {
        let mut state = ready(Vec::new());
        state.podcasts.selected = 0;
        update(&mut state, Action::Select);
        assert!(matches!(
            state.popup,
            Some(Popup::FedInput {
                field: FedInputField::PodcastFeedUrl,
                ..
            })
        ));
        state.popup = None;
        assert_eq!(podcasts::submit_feed_url(&mut state, "  "), None);
        assert_eq!(
            podcasts::submit_feed_url(&mut state, "ftp://example.test/feed"),
            None
        );
        assert!(!state.podcasts.subscribing);
        assert_eq!(
            podcasts::submit_feed_url(&mut state, "example.test/feed.xml"),
            Some(Effect::PodcastSubscribe("example.test/feed.xml".into()))
        );
        assert!(state.podcasts.subscribing);
        // A second add while one is running is refused.
        update(&mut state, Action::AddPodcast);
        assert!(state.popup.is_none());

        let effect = podcasts::subscribed(
            &mut state,
            Ok(SubscribeReport {
                podcast_id: podcast_id(),
                title: "Show".into(),
                already_subscribed: false,
                new_episodes: 4,
            }),
        );
        assert_eq!(effect, Some(Effect::PodcastsReload));
        assert!(!state.podcasts.subscribing);
        assert!(
            state
                .status_message
                .as_deref()
                .is_some_and(|m| m.contains("subscribed to \"Show\" (4 episode(s); private)"))
        );
        let mut other = fx::row("Another", 0, 1, 0);
        other.summary.podcast_id = other_id();
        podcasts::list_loaded(&mut state, Ok(vec![other, fx::row("Show", 4, 4, 0)]));
        assert_eq!(state.podcasts.selected, PODCAST_ACTION_ROWS + 1);
        assert_eq!(
            state.podcasts.current_podcast().map(|row| row.title()),
            Some("Show")
        );
    }

    #[test]
    fn unsubscribing_asks_first_and_keeps_progress() {
        let mut state = ready(vec![fx::row("Show", 0, 1, 0)]);
        let (seq, _) = episode_request(update(&mut state, Action::Select));
        podcasts::episodes_loaded(&mut state, seq, 0, Ok(page(1)));
        update(&mut state, Action::UnsubscribePodcast);
        assert!(matches!(
            &state.popup,
            Some(Popup::ConfirmPodcastUnsubscribe { podcast_id: id, title })
                if *id == podcast_id() && title == "Show"
        ));
        state.popup = None;
        let effect = podcasts::unsubscribed(&mut state, podcast_id(), "Show".into(), Ok(()));
        assert_eq!(effect, Some(Effect::PodcastsReload));
        assert!(state.podcasts.opened.is_none());
        assert!(
            state
                .status_message
                .as_deref()
                .is_some_and(|m| m.contains("progress and history are kept"))
        );
        // Unsubscribed elsewhere: the reloaded list closes an open view.
        let mut state = ready(vec![fx::row("Show", 0, 1, 0)]);
        update(&mut state, Action::Select);
        podcasts::list_loaded(&mut state, Ok(Vec::new()));
        assert!(state.podcasts.opened.is_none());
    }

    #[test]
    fn marking_played_toggles_by_status() {
        let mut state = ready(vec![fx::row("Show", 1, 2, 0)]);
        let (seq, _) = episode_request(update(&mut state, Action::Select));
        let items = vec![
            fx::episode(1, "Done", Some((ProgressStatus::Completed, 0))),
            fx::episode(2, "Half", Some((ProgressStatus::InProgress, 60_000))),
        ];
        let done = items[0].episode.episode_id.clone();
        let half = items[1].episode.episode_id.clone();
        podcasts::episodes_loaded(&mut state, seq, 0, Ok(items));
        assert_eq!(
            update(&mut state, Action::ToggleEpisodePlayed),
            Some(Effect::PodcastSetPlayed {
                episode_id: done,
                played: false,
            })
        );
        update(&mut state, Action::MoveDown);
        assert_eq!(
            update(&mut state, Action::ToggleEpisodePlayed),
            Some(Effect::PodcastSetPlayed {
                episode_id: half,
                played: true,
            })
        );
        let effects = podcasts::played_set(&mut state, true, Ok(()));
        assert_eq!(effects[0], Effect::PodcastsReload);
        assert!(matches!(
            effects.last(),
            Some(Effect::PodcastLoadEpisodes { .. })
        ));
    }
}

/// Podcast episodes in the shared queue: identity by episode id (never the
/// shared sentinel id) and music-only actions that must skip them.
mod podcast_queue {
    use super::*;
    use crate::app::podcast_fixtures::{self as fx, episode_track};
    use crate::app::state::{PODCAST_ACTION_ROWS, Popup, RepeatMode, track_content_id};
    use crate::app::update::PODCAST_NOT_MUSIC;
    use music_dht::podcast::ProgressStatus;

    fn keys(queue: &[TrackItem]) -> Vec<String> {
        queue.iter().map(track_key).collect()
    }

    #[test]
    fn episodes_are_keyed_by_episode_id_not_by_the_sentinel_id() {
        let (one, two) = (episode_track(1), episode_track(2));
        assert_eq!(one.id, TrackItem::NOT_A_LIBRARY_ROW);
        assert_eq!(one.id, two.id);
        assert!(track_key(&one).starts_with("ep:ep1"), "{}", track_key(&one));
        assert_ne!(track_key(&one), track_key(&two));
        assert_eq!(track_key(&one), track_key(&episode_track(1)));
        // Music keeps its content-first key; a library row without a
        // content id is keyed by its row, never as `local:` / an episode.
        assert!(track_key(&test_track(5)).starts_with("content:"));
        let mut bare = test_track(5);
        bare.content_id = None;
        assert_eq!(track_key(&bare), "lib:5");
    }

    #[test]
    fn removing_an_episode_keeps_the_other_episode_current() {
        let mut state = AppState::default();
        state.player.queue = vec![episode_track(1), test_track(7), episode_track(2)];
        state.player.queue_pos = 2;
        state.player.current = Some(episode_track(2));
        state.player.playing = true;
        state.active_tab = Tab::Queue;
        state.queue_tab.cursor = 0;
        let effect = update(&mut state, Action::RemoveFromQueue);
        assert_eq!(
            effect,
            Some(Effect::RemoveQueueIndices {
                indices: vec![0],
                restart_paused: None,
                stop: false,
            })
        );
        assert_eq!(state.player.queue_pos, 1);
        assert_eq!(
            state.player.current.as_ref().map(track_key),
            Some(track_key(&episode_track(2)))
        );
    }

    #[test]
    fn removing_the_playing_episode_moves_to_the_next_item_not_another_episode() {
        let mut state = AppState::default();
        state.player.queue = vec![episode_track(1), episode_track(2), test_track(7)];
        state.player.queue_pos = 0;
        state.player.current = Some(episode_track(1));
        state.player.playing = true;
        state.active_tab = Tab::Queue;
        state.queue_tab.cursor = 0;
        let effect = update(&mut state, Action::RemoveFromQueue);
        assert!(matches!(
            effect,
            Some(Effect::RemoveQueueIndices {
                restart_paused: Some(false),
                ..
            })
        ));
        assert_eq!(
            state.player.current.as_ref().map(track_key),
            Some(track_key(&episode_track(2)))
        );
    }

    #[test]
    fn shuffle_and_repeat_keep_a_mixed_queue_intact() {
        let mut state = AppState::default();
        let queue = vec![
            test_track(1),
            episode_track(1),
            episode_track(2),
            test_track(2),
            episode_track(3),
            test_track(3),
        ];
        let original = keys(&queue);
        state.player.queue = queue;
        state.player.current = Some(test_track(1));
        state.player.playing = true;
        for _ in 0..8 {
            update(&mut state, Action::ToggleShuffle);
            let mut shuffled = keys(&state.player.queue);
            shuffled.sort();
            let mut expected = original.clone();
            expected.sort();
            assert_eq!(shuffled, expected, "no item lost or duplicated");
            update(&mut state, Action::ToggleShuffle);
            assert_eq!(keys(&state.player.queue), original);
        }
        // Next/previous step through episodes like any other item, and
        // repeat-all wraps around.
        state.player.repeat = RepeatMode::All;
        state.player.queue_pos = 4;
        state.player.current = Some(episode_track(3));
        assert_eq!(
            update(&mut state, Action::NextTrack),
            Some(Effect::PlayCurrent)
        );
        assert_eq!(state.player.queue_pos, 5);
        assert_eq!(
            update(&mut state, Action::NextTrack),
            Some(Effect::PlayCurrent)
        );
        assert_eq!(state.player.queue_pos, 0);
        assert_eq!(
            update(&mut state, Action::PrevTrack),
            Some(Effect::PlayCurrent)
        );
        assert_eq!(state.player.queue_pos, 0);
    }

    #[test]
    fn current_track_info_finds_the_playing_episode_among_episodes() {
        let mut state = AppState::default();
        state.player.queue = vec![episode_track(1), episode_track(2)];
        // The queue position points at another episode (same sentinel id).
        state.player.queue_pos = 0;
        state.player.current = Some(episode_track(2));
        state.player.playing = true;
        update(&mut state, Action::OpenCurrentTrackInfo);
        let Some(Popup::TrackInfo { tracks, .. }) = &state.popup else {
            panic!("track info expected");
        };
        assert_eq!(track_key(&tracks[0]), track_key(&episode_track(2)));
    }

    #[test]
    fn music_only_actions_skip_episodes() {
        let mut state = fx::ready_state(vec![fx::row("Show", 1, 2, 0)]);
        state.active_tab = Tab::Queue;
        state.player.queue = vec![episode_track(1)];
        state.player.current = Some(episode_track(1));
        state.player.playing = true;

        assert_eq!(update(&mut state, Action::ToggleLike), None);
        assert_eq!(state.status_message.as_deref(), Some(PODCAST_NOT_MUSIC));
        assert_eq!(update(&mut state, Action::AddToPlaylist), None);
        assert!(state.popup.is_none());
        assert_eq!(state.status_message.as_deref(), Some(PODCAST_NOT_MUSIC));
        update(&mut state, Action::NewPlaylist);
        assert!(matches!(
            state.popup.take(),
            Some(Popup::NewPlaylist {
                for_target: None,
                ..
            })
        ));
        update(&mut state, Action::EditSelected);
        assert!(state.popup.is_none(), "episode metadata is not editable");
        assert_eq!(update(&mut state, Action::DownloadSelected), None);

        // No content id, no share link, no music history, private on the
        // v3 wire.
        let episode = episode_track(1);
        assert_eq!(track_content_id(&episode), None);
        assert_eq!(crate::share::track_content_id(&episode), None);
        assert_eq!(crate::share::track_share_link(&episode), None);
        let wire = <crate::devices::PlaybackTrack as crate::devices::PlaybackTrackExt>::from_track(
            &episode,
        );
        assert_eq!(wire.title, music_dht::device_sync::PRIVATE_ITEM_TITLE);
        assert!(wire.artist_names.is_empty() && wire.release_title.is_empty());
        assert_eq!((wire.id, wire.content_id, wire.fed), (0, None, None));

        // Shift-J opens the podcast instead of a music release.
        let effect = update(&mut state, Action::GoToRelease);
        assert_eq!(state.active_tab, Tab::Podcasts);
        assert_eq!(state.podcasts.selected, PODCAST_ACTION_ROWS);
        assert!(state.podcasts.opened.is_some());
        assert!(matches!(effect, Some(Effect::PodcastLoadEpisodes { .. })));
    }

    fn episodes_tab(statuses: &[Option<(music_dht::podcast::ProgressStatus, i64)>]) -> AppState {
        let items = statuses
            .iter()
            .enumerate()
            .map(|(index, status)| {
                let n = index as u32 + 1;
                fx::episode(n, &format!("Episode {n}"), *status)
            })
            .collect();
        fx::with_episodes(fx::ready_state(vec![fx::row("Show", 2, 2, 0)]), items)
    }

    #[test]
    fn enter_plays_the_episode_right_after_the_current_item() {
        let mut state = episodes_tab(&[None, None]);
        state.player.queue = vec![test_track(1), test_track(2)];
        state.player.queue_pos = 0;
        state.player.current = Some(test_track(1));
        state.player.playing = true;
        state.podcasts.opened.as_mut().unwrap().episode_cursor = 1;

        assert_eq!(
            update(&mut state, Action::Select),
            Some(Effect::PlayCurrent)
        );
        assert_eq!(
            keys(&state.player.queue),
            keys(&[test_track(1), episode_track(2), test_track(2)])
        );
        assert_eq!(state.player.queue_pos, 1);
        let queued = state.player.queue[1].podcast_episode_ref().unwrap();
        assert_eq!(queued.podcast_title, "Show");
        assert_eq!(queued.audio_url, "https://example.test/2.mp3");

        // Enter again on the playing episode does not queue it twice.
        state.player.current = Some(state.player.queue[1].clone());
        assert_eq!(update(&mut state, Action::Select), None);
        assert_eq!(state.player.queue.len(), 3);
        state.player.paused = true;
        assert_eq!(
            update(&mut state, Action::Select),
            Some(Effect::TogglePause)
        );
        assert!(!state.player.paused);
        // "Start over" restarts it in place.
        assert_eq!(
            update(&mut state, Action::RestartEpisode),
            Some(Effect::PlayCurrentFromStart)
        );
        assert_eq!(state.player.queue.len(), 3);
        assert_eq!(state.player.queue_pos, 1);
    }

    #[test]
    fn start_over_plays_a_selected_episode_from_the_beginning() {
        let mut state = episodes_tab(&[Some((
            music_dht::podcast::ProgressStatus::InProgress,
            600_000,
        ))]);
        assert_eq!(
            update(&mut state, Action::RestartEpisode),
            Some(Effect::PlayCurrentFromStart)
        );
        assert_eq!(keys(&state.player.queue), keys(&[episode_track(1)]));
        assert_eq!(state.player.queue_pos, 0);
        // The queue item carries the generation the list showed (handoff
        // and start-over order after it).
        let queued = state.player.queue[0].podcast_episode_ref().unwrap();
        assert_eq!(queued.progress_generation, Some(1));
        // On another tab, `b` (if bound there) restarts the playing episode.
        state.active_tab = Tab::Queue;
        state.player.current = Some(state.player.queue[0].clone());
        state.player.playing = true;
        assert_eq!(
            update(&mut state, Action::RestartEpisode),
            Some(Effect::PlayCurrentFromStart)
        );
        state.player.current = Some(test_track(3));
        assert_eq!(update(&mut state, Action::RestartEpisode), None);
    }

    #[test]
    fn episodes_can_be_queued_but_not_while_another_device_plays() {
        let mut state = episodes_tab(&[None, None]);
        assert_eq!(
            update(&mut state, Action::QueueAddLast),
            Some(Effect::QueueOrderChanged {
                restart_current: false
            })
        );
        state.podcasts.opened.as_mut().unwrap().episode_cursor = 1;
        update(&mut state, Action::QueueAddLast);
        assert_eq!(
            keys(&state.player.queue),
            keys(&[episode_track(1), episode_track(2)])
        );

        state.device_playback.role = crate::app::state::DevicePlaybackRole::Control;
        assert_eq!(update(&mut state, Action::QueueAddNext), None);
        assert_eq!(update(&mut state, Action::Select), None);
        assert_eq!(state.player.queue.len(), 2);
        assert!(
            state
                .status_message
                .as_deref()
                .unwrap()
                .contains("another device")
        );
    }

    #[test]
    fn saved_progress_updates_the_episode_list_in_place() {
        let mut state = episodes_tab(&[None, None]);
        let mut record = fx::episode(
            2,
            "Episode 2",
            Some((music_dht::podcast::ProgressStatus::InProgress, 90_000)),
        )
        .progress
        .unwrap();
        record.generation = 4;
        assert!(crate::app::update::podcasts::progress_saved(&mut state, record).is_empty());
        let items = &state.podcasts.episodes.as_ref().unwrap().items;
        assert!(items[0].progress.is_none());
        assert_eq!(items[1].resume_position_ms(), 90_000);
        assert_eq!(items[1].progress.as_ref().unwrap().generation, 4);
    }

    #[test]
    fn saved_progress_raises_the_generation_a_handoff_carries() {
        let mut state = AppState::default();
        state.player.queue = vec![episode_track(1), episode_track(2), test_track(3)];
        state.player.current = Some(episode_track(2));
        let mut record = fx::episode(2, "Episode 2", Some((ProgressStatus::InProgress, 1)))
            .progress
            .unwrap();
        record.generation = 7;
        crate::app::update::podcasts::progress_saved(&mut state, record.clone());
        let generation = |track: &TrackItem| track.podcast.as_ref().unwrap().progress_generation;
        assert_eq!(generation(&state.player.queue[0]), None);
        assert_eq!(generation(&state.player.queue[1]), Some(7));
        assert_eq!(generation(state.player.current.as_ref().unwrap()), Some(7));
        // Never lowered by an older register.
        record.generation = 3;
        crate::app::update::podcasts::progress_saved(&mut state, record);
        assert_eq!(generation(&state.player.queue[1]), Some(7));
    }

    #[test]
    fn podcast_changes_from_other_devices_reload_what_is_shown() {
        let mut state = episodes_tab(&[None, None]);
        let record = fx::episode(1, "Episode 1", Some((ProgressStatus::InProgress, 5_000)))
            .progress
            .unwrap();
        // A progress register is shown in place; the list counts reload.
        let effects = crate::app::update::podcasts::synced(&mut state, vec![record], false);
        assert_eq!(effects, [Effect::PodcastsReload]);
        let items = &state.podcasts.episodes.as_ref().unwrap().items;
        assert_eq!(items[0].resume_position_ms(), 5_000);
        // Anything else reloads the opened podcast too.
        let effects = crate::app::update::podcasts::synced(&mut state, Vec::new(), true);
        assert_eq!(effects[0], Effect::PodcastsReload);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::PodcastLoadEpisodes { .. })),
            "{effects:?}"
        );
        // Before the store is ready there is nothing to reload.
        let mut opening = AppState::default();
        assert!(crate::app::update::podcasts::synced(&mut opening, Vec::new(), true).is_empty());
    }
}
