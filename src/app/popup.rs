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
    AppState, DeleteTarget, EditField, EditTarget, FedInputField, Loadable, Popup,
    addable_playlists,
};
use crate::library::models::{ReleaseEdit, TrackEdit, TrackItem};

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
    }
}

/// One-line text entry on the Federation tab (network id / peer ticket).
fn handle_fed_input(
    state: &mut AppState,
    runtime: &Runtime,
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
            }
        }
        _ => {
            input.handle_key(key);
            state.popup = Some(Popup::FedInput { field, input });
        }
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
            library.update_playlist(id, &title, None)
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
                DeleteTarget::Playlist(id) => library.delete_playlist(id),
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
            let tx = runtime.event_tx.clone();
            let ids: Vec<i64> = tracks.iter().map(|t| t.id).filter(|id| *id >= 0).collect();
            tokio::task::spawn_blocking(move || {
                let result = library
                    .add_tracks_to_playlist(playlist_id, &ids)
                    .map_err(|err| format!("{err:#}"));
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
    let tx = runtime.event_tx.clone();
    tokio::task::spawn_blocking(move || {
        let result = library
            .create_playlist(&title)
            .map_err(|err| format!("{err:#}"));
        let _ = tx.send(AppEvent::PlaylistCreated { result, add_target });
    });
}
