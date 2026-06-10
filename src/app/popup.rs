//! Modal dialog input: the add-to-playlist picker and new-playlist name
//! entry. The popup is taken out of the state, handled as an owned value
//! and put back unless the action closed it.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::api::models::TrackItem;
use crate::app::Runtime;
use crate::app::event::AppEvent;
use crate::app::state::{AppState, Popup, addable_playlists};

pub fn handle_key(state: &mut AppState, runtime: &Runtime, key: KeyEvent) {
    let Some(popup) = state.popup.take() else {
        return;
    };
    match popup {
        Popup::AddToPlaylist { track, cursor } => {
            handle_picker(state, runtime, track, cursor, key);
        }
        Popup::NewPlaylist {
            for_track,
            input,
            busy,
        } => handle_name_entry(state, runtime, for_track, input, busy, key),
        Popup::Devices { cursor } => handle_devices(state, runtime, cursor, key),
        Popup::LogDetail(entry) => match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {}
            _ => state.popup = Some(Popup::LogDetail(entry)),
        },
    }
}

/// Pasted text goes into the name field when it is open.
pub fn handle_paste(state: &mut AppState, pasted: &str) {
    if let Some(Popup::NewPlaylist { input, busy, .. }) = &mut state.popup {
        if !*busy {
            input.extend(pasted.chars().filter(|c| !c.is_control()));
        }
    }
}

fn handle_devices(state: &mut AppState, runtime: &Runtime, cursor: usize, key: KeyEvent) {
    let len = state.devices.devices.len();
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {}
        KeyCode::Up | KeyCode::Char('k') => {
            state.popup = Some(Popup::Devices {
                cursor: cursor.saturating_sub(1),
            });
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.popup = Some(Popup::Devices {
                cursor: if len == 0 {
                    0
                } else {
                    (cursor + 1).min(len - 1)
                },
            });
        }
        KeyCode::Enter => {
            if let Some(device) = state.devices.devices.get(cursor.min(len.saturating_sub(1))) {
                let target = device.id.clone();
                state.devices.switching_to = Some(target.clone());
                state.popup = Some(Popup::Devices { cursor });
                spawn_select_device(runtime, target);
            } else {
                state.popup = Some(Popup::Devices { cursor: 0 });
            }
        }
        _ => {
            state.popup = Some(Popup::Devices {
                cursor: cursor.min(len.saturating_sub(1)),
            })
        }
    }
}

fn handle_picker(
    state: &mut AppState,
    runtime: &Runtime,
    track: TrackItem,
    cursor: usize,
    key: KeyEvent,
) {
    let options = addable_playlists(state);
    match key.code {
        KeyCode::Esc => {}
        KeyCode::Up | KeyCode::Char('k') => {
            state.popup = Some(Popup::AddToPlaylist {
                track,
                cursor: cursor.saturating_sub(1),
            });
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.popup = Some(Popup::AddToPlaylist {
                track,
                cursor: (cursor + 1).min(options.len()),
            });
        }
        KeyCode::Enter => {
            if cursor == 0 {
                state.popup = Some(Popup::NewPlaylist {
                    for_track: Some(track),
                    input: String::new(),
                    busy: false,
                });
            } else if let Some((id, title)) = options.get(cursor - 1).cloned() {
                spawn_add_track(runtime, id, title, track);
            }
        }
        _ => state.popup = Some(Popup::AddToPlaylist { track, cursor }),
    }
}

fn handle_name_entry(
    state: &mut AppState,
    runtime: &Runtime,
    for_track: Option<TrackItem>,
    mut input: String,
    busy: bool,
    key: KeyEvent,
) {
    if busy {
        state.popup = Some(Popup::NewPlaylist {
            for_track,
            input,
            busy,
        });
        return;
    }
    match key.code {
        KeyCode::Esc => {
            // Reached from the picker → step back to it; otherwise close.
            if let Some(track) = for_track {
                state.popup = Some(Popup::AddToPlaylist { track, cursor: 0 });
            }
        }
        KeyCode::Enter => {
            let title = input.trim().to_string();
            if title.is_empty() {
                state.status_message = Some("playlist name is empty".into());
                state.popup = Some(Popup::NewPlaylist {
                    for_track,
                    input,
                    busy: false,
                });
                return;
            }
            spawn_create_playlist(runtime, title, for_track.clone());
            state.popup = Some(Popup::NewPlaylist {
                for_track,
                input,
                busy: true,
            });
        }
        KeyCode::Backspace => {
            input.pop();
            state.popup = Some(Popup::NewPlaylist {
                for_track,
                input,
                busy: false,
            });
        }
        KeyCode::Char(c) if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
            input.push(c);
            state.popup = Some(Popup::NewPlaylist {
                for_track,
                input,
                busy: false,
            });
        }
        _ => {
            state.popup = Some(Popup::NewPlaylist {
                for_track,
                input,
                busy: false,
            });
        }
    }
}

fn spawn_select_device(runtime: &Runtime, target_device_id: String) {
    let Some(api) = runtime.api.clone() else {
        return;
    };
    let current_device_id = runtime.device_id.clone();
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let event = match api
            .select_device(&target_device_id, &current_device_id)
            .await
        {
            Ok(response) => AppEvent::DeviceActivated(Ok(response)),
            Err(crate::api::client::ApiError::SessionExpired) => AppEvent::SessionExpired,
            Err(err) => AppEvent::DeviceActivated(Err(err.to_string())),
        };
        let _ = tx.send(event);
    });
}

fn spawn_add_track(runtime: &Runtime, playlist_id: i64, playlist_title: String, track: TrackItem) {
    let Some(api) = runtime.api.clone() else {
        return;
    };
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let result = api
            .add_tracks_to_playlist(playlist_id, &[track.id])
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(AppEvent::PlaylistTracksAdded {
            playlist_id,
            playlist_title,
            result,
        });
    });
}

fn spawn_create_playlist(runtime: &Runtime, title: String, add_track: Option<TrackItem>) {
    let Some(api) = runtime.api.clone() else {
        return;
    };
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let result = api.create_playlist(&title).await.map_err(|e| e.to_string());
        let _ = tx.send(AppEvent::PlaylistCreated { result, add_track });
    });
}
