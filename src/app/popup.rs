//! Modal dialog input: the add-to-playlist picker, new-playlist name entry,
//! metadata edit forms and delete confirmations. The popup is taken out of
//! the state, handled as an owned value and put back unless the action
//! closed it.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};

use crate::app::Runtime;
use crate::app::event::AppEvent;
use crate::app::state::{
    self, AppState, DeleteTarget, DevicePresenceSection, EditField, EditTarget, FedInputField,
    FederationStatusPopupState, Loadable, Popup, StatusDetailFocus, addable_playlists,
};
use crate::library::models::{ReleaseEdit, TrackEdit, TrackItem};

#[derive(Clone)]
pub(crate) struct ConnectedDevicePopupRow {
    pub device_id: String,
    pub name: String,
    pub is_self: bool,
    pub online: bool,
    pub section: DevicePresenceSection,
    pub active: bool,
    pub playing: bool,
    pub paused: bool,
    pub queue_len: usize,
}

pub(crate) fn connected_device_rows(state: &AppState) -> Vec<ConnectedDevicePopupRow> {
    let mut rows = Vec::new();
    if let Some(status) = &state.federation.devices {
        let now = state::unix_time_ms();
        for index in state::device_status_order(state) {
            let Some(device) = status.devices.get(index) else {
                continue;
            };
            let snapshot = state.device_playback.remote.get(&device.device_id);
            let is_self =
                device.is_self || device.device_id == state.device_playback.self_device_id;
            let section = state::device_presence_section(state, device, now);
            let online = section == DevicePresenceSection::Online;
            rows.push(ConnectedDevicePopupRow {
                device_id: device.device_id.clone(),
                name: state::device_display_name(device),
                is_self,
                online,
                section,
                active: state::device_status_active(state, &device.device_id),
                playing: if is_self {
                    state.player.playing
                } else {
                    snapshot.is_some_and(|snapshot| snapshot.state.playing)
                },
                paused: if is_self {
                    state.player.paused
                } else {
                    snapshot.is_some_and(|snapshot| snapshot.state.paused)
                },
                queue_len: if is_self {
                    state.player.queue.len()
                } else {
                    snapshot
                        .map(|snapshot| snapshot.state.queue.len())
                        .unwrap_or(0)
                },
            });
        }
    }
    if rows
        .iter()
        .all(|row| row.device_id != state.device_playback.self_device_id)
    {
        rows.insert(
            0,
            ConnectedDevicePopupRow {
                device_id: state.device_playback.self_device_id.clone(),
                name: state.device_playback.self_device_name.clone(),
                is_self: true,
                online: true,
                section: DevicePresenceSection::Online,
                active: state.device_playback.role == crate::app::state::DevicePlaybackRole::Active,
                playing: state.player.playing,
                paused: state.player.paused,
                queue_len: state.player.queue.len(),
            },
        );
    }
    rows
}

pub fn handle_key(state: &mut AppState, runtime: &mut Runtime, key: KeyEvent) {
    let Some(popup) = state.popup.take() else {
        return;
    };
    match popup {
        Popup::AddToPlaylist { target, cursor } => {
            handle_picker(state, runtime, target, cursor, key);
        }
        Popup::NewPlaylist {
            for_target,
            input,
            busy,
        } => handle_name_entry(state, runtime, for_target, input, busy, key),
        Popup::Edit {
            target,
            title,
            fields,
            focus,
            error,
        } => handle_edit(state, runtime, target, title, fields, focus, error, key),
        Popup::ConfirmDelete { target, label } => {
            handle_confirm_delete(state, runtime, target, label, key);
        }
        Popup::LibraryFilters { cursor } => handle_library_filters(state, runtime, cursor, key),
        Popup::TrackInfo {
            tracks,
            cursor,
            scroll,
        } => handle_track_info(state, runtime, tracks, cursor, scroll, key),
        Popup::TrackArtists {
            tracks,
            cursor,
            scroll,
            selected,
        } => handle_track_artists(state, runtime, tracks, cursor, scroll, selected, key),
        Popup::LogDetail(entry) => match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {}
            _ => state.popup = Some(Popup::LogDetail(entry)),
        },
        Popup::FedInput { field, input } => handle_fed_input(state, runtime, field, input, key),
        Popup::FedText { title, text } => match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {}
            _ => state.popup = Some(Popup::FedText { title, text }),
        },
        Popup::FedCopyText { title, text, help } => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {}
            KeyCode::Enter | KeyCode::Char('c') => match copy_to_clipboard(&text) {
                Ok(()) => state.status_message = Some("copied to clipboard".to_string()),
                Err(err) => {
                    state.status_message = Some(format!("copy failed: {err}"));
                    state.popup = Some(Popup::FedCopyText { title, text, help });
                }
            },
            _ => state.popup = Some(Popup::FedCopyText { title, text, help }),
        },
        Popup::FederationStatusDetails {
            focus,
            status_cursor,
            devices_scroll,
            logs_scroll,
        } => handle_federation_status_details(
            state,
            FederationStatusPopupState {
                focus,
                status_cursor,
                devices_scroll,
                logs_scroll,
            },
            key,
        ),
        Popup::FederationStatusText {
            parent,
            title,
            text,
            scroll,
        } => handle_federation_status_child(state, parent, title, text, scroll, key),
        Popup::FederationStatusLog { parent, scroll } => {
            handle_federation_status_log(state, parent, scroll, key);
        }
        Popup::DevicePairing {
            request_id,
            device_id,
            name,
            client_version,
            requester_group_id,
            requester_group_active_devices,
        } => handle_device_pairing(
            state,
            runtime,
            request_id,
            device_id,
            name,
            client_version,
            requester_group_id,
            requester_group_active_devices,
            key,
        ),
        Popup::ConfirmDeviceRevoke { device_id, name } => {
            handle_device_revoke(state, runtime, device_id, name, key);
        }
        Popup::ConfirmDeviceLeave => handle_device_leave(state, runtime, key),
        Popup::ConnectedDevices { cursor } => {
            handle_connected_devices(state, runtime, cursor, key);
        }
    }
}

fn handle_federation_status_details(
    state: &mut AppState,
    mut parent: FederationStatusPopupState,
    key: KeyEvent,
) {
    let action_count = status_detail_action_count(state);
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => return,
        KeyCode::Left | KeyCode::Char('h') => parent.focus = parent.focus.previous(),
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => parent.focus = parent.focus.next(),
        KeyCode::Up | KeyCode::Char('k') => match parent.focus {
            StatusDetailFocus::Status => {
                parent.status_cursor = parent.status_cursor.saturating_sub(1)
            }
            StatusDetailFocus::Devices => {
                parent.devices_scroll = parent.devices_scroll.saturating_sub(1)
            }
            StatusDetailFocus::Logs => {
                if action_count > 0 {
                    parent.focus = StatusDetailFocus::Status;
                    parent.status_cursor = action_count - 1;
                }
            }
        },
        KeyCode::Down | KeyCode::Char('j') => match parent.focus {
            StatusDetailFocus::Status => {
                if parent.status_cursor + 1 < action_count {
                    parent.status_cursor += 1;
                } else {
                    parent.focus = StatusDetailFocus::Logs;
                }
            }
            StatusDetailFocus::Devices => {
                parent.devices_scroll = parent.devices_scroll.saturating_add(1)
            }
            StatusDetailFocus::Logs => {}
        },
        KeyCode::PageUp => match parent.focus {
            StatusDetailFocus::Status => parent.status_cursor = 0,
            StatusDetailFocus::Devices => {
                parent.devices_scroll = parent.devices_scroll.saturating_sub(8)
            }
            StatusDetailFocus::Logs => {}
        },
        KeyCode::PageDown => match parent.focus {
            StatusDetailFocus::Status => {
                parent.status_cursor = action_count.saturating_sub(1);
                parent.focus = StatusDetailFocus::Logs;
            }
            StatusDetailFocus::Devices => {
                parent.devices_scroll = parent.devices_scroll.saturating_add(8)
            }
            StatusDetailFocus::Logs => {}
        },
        KeyCode::Enter if parent.focus == StatusDetailFocus::Status => {
            if let Some((title, text)) = status_detail_action_text(state, parent.status_cursor) {
                state.popup = Some(Popup::FederationStatusText {
                    parent,
                    title,
                    text,
                    scroll: 0,
                });
                return;
            }
        }
        KeyCode::Enter if parent.focus == StatusDetailFocus::Logs => {
            state.popup = Some(Popup::FederationStatusLog { parent, scroll: 0 });
            return;
        }
        _ => {}
    }
    parent.status_cursor = parent.status_cursor.min(action_count.saturating_sub(1));
    state.popup = Some(parent.into());
}

fn handle_federation_status_child(
    state: &mut AppState,
    parent: FederationStatusPopupState,
    title: String,
    text: String,
    scroll: usize,
    key: KeyEvent,
) {
    let next_scroll = match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
            state.popup = Some(parent.into());
            return;
        }
        KeyCode::Up | KeyCode::Char('k') => scroll.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => scroll.saturating_add(1),
        KeyCode::PageUp => scroll.saturating_sub(10),
        KeyCode::PageDown => scroll.saturating_add(10),
        _ => scroll,
    };
    state.popup = Some(Popup::FederationStatusText {
        parent,
        title,
        text,
        scroll: next_scroll,
    });
}

fn handle_federation_status_log(
    state: &mut AppState,
    parent: FederationStatusPopupState,
    scroll: usize,
    key: KeyEvent,
) {
    let next_scroll = match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            state.popup = Some(parent.into());
            return;
        }
        KeyCode::Up | KeyCode::Char('k') => scroll.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => scroll.saturating_add(1),
        KeyCode::PageUp => scroll.saturating_sub(10),
        KeyCode::PageDown => scroll.saturating_add(10),
        _ => scroll,
    };
    state.popup = Some(Popup::FederationStatusLog {
        parent,
        scroll: next_scroll,
    });
}

fn status_detail_action_count(state: &AppState) -> usize {
    state
        .federation
        .status
        .as_ref()
        .filter(|status| status.running)
        .map(|status| 1 + usize::from(!status.connected_peers.is_empty()))
        .unwrap_or(0)
}

fn status_detail_action_text(state: &AppState, cursor: usize) -> Option<(String, String)> {
    let status = state
        .federation
        .status
        .as_ref()
        .filter(|status| status.running)?;
    match cursor {
        0 => Some((
            "Endpoint IDs".to_string(),
            format!(
                "Endpoint ID\n{}\n\nDHT node ID\n{}",
                status.endpoint_id, status.dht_node_id
            ),
        )),
        1 if !status.connected_peers.is_empty() => {
            Some(("Peer IDs".to_string(), status.connected_peers.join("\n")))
        }
        _ => None,
    }
}

fn handle_connected_devices(
    state: &mut AppState,
    runtime: &mut Runtime,
    cursor: usize,
    key: KeyEvent,
) {
    let rows = connected_device_rows(state);
    let other_rows: Vec<_> = rows.iter().filter(|row| !row.is_self).cloned().collect();
    let last = other_rows.len();
    let cursor = cursor.min(last);
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {}
        KeyCode::Up | KeyCode::Char('k') => {
            state.popup = Some(Popup::ConnectedDevices {
                cursor: cursor.saturating_sub(1),
            });
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.popup = Some(Popup::ConnectedDevices {
                cursor: (cursor + 1).min(last),
            });
        }
        KeyCode::Enter => {
            if cursor == 0 {
                super::transfer_active_to_this_device(state, runtime);
                state.status_message = Some("active playback moved to this device".into());
            } else if let Some(row) = other_rows.get(cursor.saturating_sub(1).min(last)) {
                if row.active
                    && let Some(snapshot) =
                        state.device_playback.remote.get(&row.device_id).cloned()
                {
                    super::become_control_device(state, runtime, snapshot);
                } else {
                    super::transfer_active_to_remote_device(
                        state,
                        runtime,
                        row.device_id.clone(),
                        row.name.clone(),
                    );
                }
            }
        }
        _ => state.popup = Some(Popup::ConnectedDevices { cursor }),
    }
}

fn handle_library_filters(
    state: &mut AppState,
    runtime: &mut Runtime,
    cursor: usize,
    key: KeyEvent,
) {
    let max_cursor = crate::config::settings::LibrarySourceMode::ALL.len();
    let cursor = cursor.min(max_cursor);
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {}
        KeyCode::Up | KeyCode::Char('k') => {
            state.popup = Some(Popup::LibraryFilters {
                cursor: cursor.saturating_sub(1),
            });
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.popup = Some(Popup::LibraryFilters {
                cursor: (cursor + 1).min(max_cursor),
            });
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            if cursor == 0 {
                state.global.filters.hide_featured_only = !state.global.filters.hide_featured_only;
            } else if let Some(mode) =
                crate::config::settings::LibrarySourceMode::ALL.get(cursor - 1)
            {
                state.global.filters.source_mode = *mode;
            }
            runtime.library_network_refresh_at = None;
            if let Ok(mut cursors) = runtime.library_network_cursors.lock() {
                cursors.clear();
            }
            if let Ok(mut done) = runtime.library_network_done.lock() {
                done.clear();
            }
            if let Ok(mut attempted) = runtime.library_network_art_attempted.lock() {
                attempted.clear();
            }
            super::save_app_settings(state);
            super::reset_artist_pagination(state);
            super::refresh_artists(state, runtime);
            super::update::apply_library_filter_change(state);
        }
        _ => state.popup = Some(Popup::LibraryFilters { cursor }),
    }
}

/// One-line text entry on the Federation tab (network id / peer ticket).
fn handle_fed_input(
    state: &mut AppState,
    runtime: &mut Runtime,
    field: FedInputField,
    mut input: crate::app::input::LineEdit,
    key: KeyEvent,
) {
    match key.code {
        KeyCode::Esc => {}
        KeyCode::Enter => {
            let value = input.trim().to_string();
            match field {
                FedInputField::NetworkId => {
                    state.federation.settings.network_id = value;
                    // An empty id turns federation off rather than leaving a
                    // node bound to an unnamed network; a freshly set id
                    // enables it right away.
                    state.federation.settings.enabled =
                        !state.federation.settings.network_id.is_empty();
                    super::fed_apply_settings(state, runtime);
                }
                FedInputField::ConnectTicket => {
                    if value.is_empty() {
                        state.status_message = Some("ticket is empty".into());
                    } else {
                        super::fed_connect(runtime, value);
                    }
                }
                FedInputField::DeviceName => {
                    if value.is_empty() {
                        state.status_message = Some("device name is empty".into());
                    } else {
                        super::perform_effect(
                            state,
                            runtime,
                            crate::app::update::Effect::DeviceSetName(value),
                        );
                    }
                }
                FedInputField::ConnectInvite => {
                    if value.is_empty() {
                        state.status_message = Some("invite is empty".into());
                    } else {
                        super::perform_effect(
                            state,
                            runtime,
                            crate::app::update::Effect::DeviceConnectInvite(value),
                        );
                    }
                }
            }
        }
        _ => {
            input.handle_key(key);
            state.popup = Some(Popup::FedInput { field, input });
        }
    }
}

fn handle_device_pairing(
    state: &mut AppState,
    runtime: &Runtime,
    request_id: String,
    device_id: String,
    name: String,
    client_version: String,
    requester_group_id: Option<String>,
    requester_group_active_devices: usize,
    key: KeyEvent,
) {
    let group_conflict = requester_group_id.is_some() && requester_group_active_devices > 1;
    match key.code {
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
            if let Err(err) = runtime.devices.answer_pairing(&request_id, false, false) {
                state.status_message = Some(format!("pairing: {err:#}"));
            } else {
                state.status_message = Some("device pairing denied".to_string());
            }
            state.federation.devices = Some(runtime.devices.status());
        }
        KeyCode::Char('y') => {
            if let Err(err) = runtime
                .devices
                .answer_pairing(&request_id, true, group_conflict)
            {
                state.status_message = Some(format!("pairing: {err:#}"));
            } else {
                state.status_message = Some(if group_conflict {
                    format!("device \"{name}\" accepted; joining its sync group")
                } else {
                    format!("device \"{name}\" accepted")
                });
            }
            state.federation.devices = Some(runtime.devices.status());
        }
        KeyCode::Char('c') if group_conflict => {
            if let Err(err) = runtime.devices.answer_pairing(&request_id, true, false) {
                state.status_message = Some(format!("pairing: {err:#}"));
            } else {
                state.status_message =
                    Some(format!("device \"{name}\" accepted into this sync group"));
            }
            state.federation.devices = Some(runtime.devices.status());
        }
        _ => {
            state.popup = Some(Popup::DevicePairing {
                request_id,
                device_id,
                name,
                client_version,
                requester_group_id,
                requester_group_active_devices,
            });
        }
    }
}

fn handle_device_revoke(
    state: &mut AppState,
    runtime: &mut Runtime,
    device_id: String,
    name: String,
    key: KeyEvent,
) {
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('n') | KeyCode::Char('q') => {}
        KeyCode::Char('y') => {
            super::perform_effect(
                state,
                runtime,
                crate::app::update::Effect::DeviceRevoke(device_id),
            );
        }
        _ => state.popup = Some(Popup::ConfirmDeviceRevoke { device_id, name }),
    }
}

fn handle_device_leave(state: &mut AppState, runtime: &mut Runtime, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('n') | KeyCode::Char('q') => {}
        KeyCode::Char('y') => {
            super::perform_effect(state, runtime, crate::app::update::Effect::DeviceLeaveGroup);
        }
        _ => state.popup = Some(Popup::ConfirmDeviceLeave),
    }
}

/// Pasted text goes into the focused text field when one is open.
pub fn handle_paste(state: &mut AppState, pasted: &str) {
    let cleaned: String = pasted.chars().filter(|c| !c.is_control()).collect();
    match &mut state.popup {
        Some(Popup::NewPlaylist { input, busy, .. }) if !*busy => input.insert_str(&cleaned),
        Some(Popup::FedInput { input, .. }) => input.insert_str(&cleaned),
        Some(Popup::Edit { fields, focus, .. }) => {
            if let Some(field) = fields.get_mut(*focus) {
                field.value.insert_str(&cleaned);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Edit form
// ---------------------------------------------------------------------------

#[allow(
    clippy::too_many_arguments,
    reason = "owned popup state passed back in"
)]
fn handle_edit(
    state: &mut AppState,
    runtime: &Runtime,
    target: EditTarget,
    title: String,
    mut fields: Vec<EditField>,
    mut focus: usize,
    error: Option<String>,
    key: KeyEvent,
) {
    match key.code {
        KeyCode::Esc => return,
        KeyCode::Enter => {
            match save_edit(runtime, target, &fields) {
                Ok(message) => {
                    state.status_message = Some(message);
                    return;
                }
                Err(message) => {
                    state.popup = Some(Popup::Edit {
                        target,
                        title,
                        fields,
                        focus,
                        error: Some(message),
                    });
                    return;
                }
            };
        }
        KeyCode::Tab | KeyCode::Down => focus = (focus + 1) % fields.len().max(1),
        KeyCode::BackTab | KeyCode::Up => {
            let len = fields.len().max(1);
            focus = (focus + len - 1) % len;
        }
        _ => {
            if let Some(field) = fields.get_mut(focus) {
                field.value.handle_key(key);
            }
        }
    }
    state.popup = Some(Popup::Edit {
        target,
        title,
        fields,
        focus,
        error,
    });
}

/// Validate the form and write it to the library. Returns the status
/// message on success, the error text to show in the form otherwise.
fn save_edit(
    runtime: &Runtime,
    target: EditTarget,
    fields: &[EditField],
) -> Result<String, String> {
    let value = |label: &str| {
        fields
            .iter()
            .find(|field| field.label == label)
            .map(|field| field.value.trim().to_string())
            .unwrap_or_default()
    };
    let number = |label: &str| -> Result<Option<i32>, String> {
        let raw = value(label);
        if raw.is_empty() {
            return Ok(None);
        }
        raw.parse::<i32>()
            .map(Some)
            .map_err(|_| format!("{label} must be a number"))
    };
    let names = |label: &str| -> Vec<String> {
        value(label)
            .split(';')
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect()
    };

    let library = Arc::clone(&runtime.library);
    let result = match target {
        EditTarget::Track(id) => {
            let title = value("Title");
            if title.is_empty() {
                return Err("title is empty".to_string());
            }
            let artists = names("Artists");
            if artists.is_empty() {
                return Err("at least one artist is required".to_string());
            }
            let cover_path = value("Cover path");
            let edit = TrackEdit {
                title,
                artists,
                featured_artists: names("Featured"),
                track_number: number("Track #")?,
                disc_number: number("Disc #")?,
                cover_path: (!cover_path.is_empty()).then_some(cover_path),
            };
            library.update_track(id, &edit)
        }
        EditTarget::Release(id) => {
            let title = value("Title");
            if title.is_empty() {
                return Err("title is empty".to_string());
            }
            let release_type = value("Type").to_lowercase();
            let release_type = if release_type.is_empty() {
                "album".to_string()
            } else {
                release_type
            };
            let edit = ReleaseEdit {
                title,
                release_type,
                year: number("Year")?,
                artists: Vec::new(),
            };
            library.update_release(id, &edit)
        }
        EditTarget::Artist(id) => {
            let name = value("Name");
            if name.is_empty() {
                return Err("name is empty".to_string());
            }
            let image = value("Image path");
            let image = (!image.is_empty()).then_some(image);
            library.update_artist(id, &name, image.as_deref())
        }
        EditTarget::Playlist(id) => {
            let title = value("Title");
            if title.is_empty() {
                return Err("title is empty".to_string());
            }
            let result = library.update_playlist(id, &title, None);
            if result.is_ok()
                && let Err(err) = runtime.devices.record_playlist_renamed(id, &title)
            {
                tracing::warn!(%err, playlist = id, "recording synced playlist rename failed");
            }
            result
        }
    };
    match result {
        Ok(()) => {
            let _ = runtime.event_tx.send(AppEvent::LibraryChanged {
                message: Some("saved".to_string()),
            });
            Ok("saved".to_string())
        }
        Err(err) => Err(format!("{err:#}")),
    }
}

// ---------------------------------------------------------------------------
// Delete confirmation
// ---------------------------------------------------------------------------

fn handle_confirm_delete(
    state: &mut AppState,
    runtime: &Runtime,
    target: DeleteTarget,
    label: String,
    key: KeyEvent,
) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {}
        KeyCode::Enter | KeyCode::Char('y') => {
            let library = Arc::clone(&runtime.library);
            let result = match target {
                DeleteTarget::Track(id) => library.delete_track(id),
                DeleteTarget::Release(id) => library.delete_release(id),
                DeleteTarget::Artist(id) => library.delete_artist(id),
                DeleteTarget::Playlist(id) => match runtime.devices.record_playlist_deleted(id) {
                    Ok(()) => library.delete_playlist(id),
                    Err(err) => {
                        tracing::warn!(%err, playlist = id, "recording synced playlist deletion failed");
                        library.delete_playlist(id)
                    }
                },
            };
            match result {
                Ok(()) => {
                    let _ = runtime.event_tx.send(AppEvent::LibraryChanged {
                        message: Some(format!("deleted {label}")),
                    });
                }
                Err(err) => state.status_message = Some(format!("delete failed: {err:#}")),
            }
        }
        _ => state.popup = Some(Popup::ConfirmDelete { target, label }),
    }
}

// ---------------------------------------------------------------------------
// Track info
// ---------------------------------------------------------------------------

fn handle_track_info(
    state: &mut AppState,
    runtime: &mut Runtime,
    tracks: Vec<TrackItem>,
    cursor: usize,
    scroll: usize,
    key: KeyEvent,
) {
    let len = tracks.len();
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {}
        KeyCode::Char('a') => {
            let artists = tracks
                .get(cursor.min(len.saturating_sub(1)))
                .map(crate::app::update::track_artist_refs)
                .unwrap_or_default();
            match artists.len() {
                0 => {
                    state.status_message = Some("this track has no artists".into());
                    state.popup = Some(Popup::TrackInfo {
                        tracks,
                        cursor,
                        scroll,
                    });
                }
                // A single artist opens directly; the popup closes.
                1 => open_artist(state, runtime, &artists[0]),
                _ => {
                    state.popup = Some(Popup::TrackArtists {
                        tracks,
                        cursor,
                        scroll,
                        selected: 0,
                    });
                }
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.popup = Some(Popup::TrackInfo {
                tracks,
                cursor,
                scroll: scroll.saturating_sub(1),
            });
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.popup = Some(Popup::TrackInfo {
                tracks,
                cursor,
                scroll: scroll + 1,
            });
        }
        KeyCode::Left | KeyCode::Char('h') => {
            state.popup = Some(Popup::TrackInfo {
                tracks,
                cursor: cursor.saturating_sub(1),
                scroll: 0,
            });
        }
        KeyCode::Right | KeyCode::Char('l') => {
            state.popup = Some(Popup::TrackInfo {
                tracks,
                cursor: if len == 0 {
                    0
                } else {
                    (cursor + 1).min(len - 1)
                },
                scroll: 0,
            });
        }
        KeyCode::Char('c') => {
            if let Some(track) = tracks.get(cursor.min(len.saturating_sub(1))) {
                match crate::share::track_share_link(track) {
                    Some(link) => match copy_to_clipboard(&link) {
                        Ok(()) => state.status_message = Some("frid link copied".into()),
                        Err(err) => state.status_message = Some(format!("copy failed: {err}")),
                    },
                    None => state.status_message = Some("no content id for this track yet".into()),
                }
            }
            state.popup = Some(Popup::TrackInfo {
                tracks,
                cursor: cursor.min(len.saturating_sub(1)),
                scroll,
            });
        }
        _ => {
            state.popup = Some(Popup::TrackInfo {
                tracks,
                cursor: cursor.min(len.saturating_sub(1)),
                scroll,
            });
        }
    }
}

/// The artist picker over the track info: j/k choose, Enter jumps to the
/// artist page, Esc returns to the info view.
fn handle_track_artists(
    state: &mut AppState,
    runtime: &mut Runtime,
    tracks: Vec<TrackItem>,
    cursor: usize,
    scroll: usize,
    selected: usize,
    key: KeyEvent,
) {
    let artists = tracks
        .get(cursor.min(tracks.len().saturating_sub(1)))
        .map(crate::app::update::track_artist_refs)
        .unwrap_or_default();
    if artists.is_empty() {
        state.popup = Some(Popup::TrackInfo {
            tracks,
            cursor,
            scroll,
        });
        return;
    }
    let last = artists.len() - 1;
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('a') => {
            state.popup = Some(Popup::TrackInfo {
                tracks,
                cursor,
                scroll,
            });
        }
        KeyCode::Enter => open_artist(state, runtime, &artists[selected.min(last)]),
        KeyCode::Up | KeyCode::Char('k') => {
            state.popup = Some(Popup::TrackArtists {
                tracks,
                cursor,
                scroll,
                selected: selected.saturating_sub(1),
            });
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.popup = Some(Popup::TrackArtists {
                tracks,
                cursor,
                scroll,
                selected: (selected + 1).min(last),
            });
        }
        _ => {
            state.popup = Some(Popup::TrackArtists {
                tracks,
                cursor,
                scroll,
                selected: selected.min(last),
            });
        }
    }
}

/// Closes the popup and jumps to the artist's page (local or federated).
fn open_artist(
    state: &mut AppState,
    runtime: &mut Runtime,
    artist: &crate::library::models::ArtistRef,
) {
    state.track_selection.clear();
    if let Some(effect) = crate::app::update::open_artist_ref(state, artist) {
        super::perform_effect(state, runtime, effect);
    }
}

fn copy_to_clipboard(text: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        return run_clipboard_command("pbcopy", &[], text);
    }
    #[cfg(target_os = "windows")]
    {
        return run_clipboard_command("cmd", &["/C", "clip"], text);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for (program, args) in [
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
            ("xsel", &["--clipboard", "--input"][..]),
        ] {
            if run_clipboard_command(program, args, text).is_ok() {
                return Ok(());
            }
        }
        Err("clipboard command not found (tried wl-copy, xclip, xsel)".into())
    }
}

fn run_clipboard_command(program: &str, args: &[&str], text: &str) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("{program}: {err}"))?;
    let Some(stdin) = child.stdin.as_mut() else {
        return Err(format!("{program}: stdin unavailable"));
    };
    stdin
        .write_all(text.as_bytes())
        .map_err(|err| format!("{program}: {err}"))?;
    drop(child.stdin.take());
    let status = child.wait().map_err(|err| format!("{program}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program}: exited with {status}"))
    }
}

// ---------------------------------------------------------------------------
// Add-to-playlist picker & new playlist
// ---------------------------------------------------------------------------

fn handle_picker(
    state: &mut AppState,
    runtime: &Runtime,
    target: crate::app::state::PlaylistAddTarget,
    cursor: usize,
    key: KeyEvent,
) {
    let waiting = matches!(&state.playlists.list, None | Some(Loadable::Loading));
    let options = addable_playlists(state);
    let new_index = options.len();
    match key.code {
        KeyCode::Esc => {}
        KeyCode::Up | KeyCode::Char('k') => {
            state.popup = Some(Popup::AddToPlaylist {
                target,
                cursor: cursor.saturating_sub(1),
            });
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.popup = Some(Popup::AddToPlaylist {
                target,
                cursor: (cursor + 1).min(new_index),
            });
        }
        KeyCode::Enter => {
            if waiting {
                state.status_message = Some("loading playlists…".into());
                state.popup = Some(Popup::AddToPlaylist { target, cursor });
            } else if cursor < options.len() {
                if let Some((id, title)) = options.get(cursor).cloned() {
                    spawn_add_target(runtime, id, title, target);
                }
            } else {
                state.popup = Some(Popup::NewPlaylist {
                    for_target: Some(target),
                    input: crate::app::input::LineEdit::default(),
                    busy: false,
                });
            }
        }
        _ => state.popup = Some(Popup::AddToPlaylist { target, cursor }),
    }
}

fn handle_name_entry(
    state: &mut AppState,
    runtime: &Runtime,
    for_target: Option<crate::app::state::PlaylistAddTarget>,
    mut input: crate::app::input::LineEdit,
    busy: bool,
    key: KeyEvent,
) {
    if busy {
        state.popup = Some(Popup::NewPlaylist {
            for_target,
            input,
            busy,
        });
        return;
    }
    match key.code {
        KeyCode::Esc => {
            // Reached from the picker → step back to it; otherwise close.
            if let Some(target) = for_target {
                let cursor = addable_playlists(state).len();
                state.popup = Some(Popup::AddToPlaylist { target, cursor });
            }
        }
        KeyCode::Enter => {
            let title = input.trim().to_string();
            if title.is_empty() {
                state.status_message = Some("playlist name is empty".into());
                state.popup = Some(Popup::NewPlaylist {
                    for_target,
                    input,
                    busy: false,
                });
                return;
            }
            spawn_create_playlist(runtime, title, for_target.clone());
            state.popup = Some(Popup::NewPlaylist {
                for_target,
                input,
                busy: true,
            });
        }
        _ => {
            input.handle_key(key);
            state.popup = Some(Popup::NewPlaylist {
                for_target,
                input,
                busy: false,
            });
        }
    }
}

/// Adds a target to a playlist: local tracks directly; federated ones are
/// downloaded into the library first, then linked.
pub(crate) fn spawn_add_target(
    runtime: &Runtime,
    playlist_id: i64,
    playlist_title: String,
    target: crate::app::state::PlaylistAddTarget,
) {
    match target {
        crate::app::state::PlaylistAddTarget::Local(tracks) => {
            let library = Arc::clone(&runtime.library);
            let devices = Arc::clone(&runtime.devices);
            let tx = runtime.event_tx.clone();
            let ids: Vec<i64> = tracks.iter().map(|t| t.id).filter(|id| *id >= 0).collect();
            let fed_tracks: Vec<crate::federation::FedTrack> = tracks
                .iter()
                .filter(|track| track.is_fed_pending())
                .filter_map(|track| track.fed.clone())
                .collect();
            tokio::task::spawn_blocking(move || {
                let result = library
                    .add_tracks_to_playlist(playlist_id, &ids)
                    .and_then(|()| library.add_fed_tracks_to_playlist(playlist_id, &fed_tracks))
                    .map_err(|err| format!("{err:#}"));
                if result.is_ok()
                    && let Err(err) = devices.record_playlist_tracks_added(playlist_id, &ids)
                {
                    tracing::warn!(%err, playlist_id, "recording synced playlist add failed");
                }
                if result.is_ok()
                    && let Err(err) =
                        devices.record_playlist_fed_tracks_added(playlist_id, &fed_tracks)
                {
                    tracing::warn!(%err, playlist_id, "recording synced federated playlist add failed");
                }
                let _ = tx.send(AppEvent::PlaylistTracksAdded {
                    playlist_id,
                    playlist_title,
                    result,
                });
            });
        }
        crate::app::state::PlaylistAddTarget::Fed(tracks) => {
            super::fed_download_spawn(runtime, tracks, Some((playlist_id, playlist_title)));
        }
    }
}

fn spawn_create_playlist(
    runtime: &Runtime,
    title: String,
    add_target: Option<crate::app::state::PlaylistAddTarget>,
) {
    let library = Arc::clone(&runtime.library);
    let devices = Arc::clone(&runtime.devices);
    let tx = runtime.event_tx.clone();
    tokio::task::spawn_blocking(move || {
        let result = library
            .create_playlist(&title)
            .map_err(|err| format!("{err:#}"));
        if let Ok(playlist) = &result
            && let Err(err) = devices.record_playlist_created(playlist.id, &playlist.title)
        {
            tracing::warn!(%err, playlist = playlist.id, "recording synced playlist creation failed");
        }
        let _ = tx.send(AppEvent::PlaylistCreated { result, add_target });
    });
}
