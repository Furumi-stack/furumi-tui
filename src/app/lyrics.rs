//! Lyrics effects, playback selection, and the track editor.
use super::Runtime;
use super::event::AppEvent;
use super::input::LineEdit;
use super::state::{AppState, Popup, track_content_id};
use crate::library::models::TrackItem;
use crate::lyrics::{self, Document, SavedLyrics};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use music_dht::lyrics::{LyricsVersion, MAX_LYRICS_TEXT_BYTES};
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug, Default)]
pub(crate) struct Playback {
    pub title: String,
    pub content_id: Option<String>,
    pub versions: Vec<LyricsVersion>,
    pub selected: usize,
    pub document: Document,
    pub document_revision: u64,
    pub loading: bool,
    track: Option<(i64, Option<String>)>,
    serial: u64,
    hidden: HashSet<String>,
    retry_at: Option<Instant>,
    task: Option<tokio::task::AbortHandle>,
}

impl Playback {
    fn select(&mut self, index: usize) {
        self.document_revision = self.document_revision.wrapping_add(1);
        self.selected = index.min(self.versions.len().saturating_sub(1));
        self.document = self
            .versions
            .get(self.selected)
            .map(Document::new)
            .unwrap_or_default();
    }

    fn add(&mut self, versions: Vec<LyricsVersion>) {
        let first = self.versions.is_empty();
        lyrics::merge(&mut self.versions, versions, &self.hidden);
        if first && !self.versions.is_empty() {
            self.select(0);
        }
    }

    fn saved(&mut self, saved: SavedLyrics) {
        let selected = self
            .versions
            .get(self.selected)
            .map(|entry| entry.id.clone());
        self.hidden = saved.hidden;
        let existing = std::mem::replace(&mut self.versions, saved.versions);
        self.add(existing);
        self.select(
            selected
                .and_then(|id| self.versions.iter().position(|v| v.id == id))
                .unwrap_or(0),
        );
    }
}

#[derive(Debug, Default)]
pub(crate) struct Reader {
    pub top: Option<usize>,
    pub viewport_height: usize,
    pub fallback: Option<Box<TrackItem>>,
    pub rows: Vec<(usize, String)>,
    pub layout_key: Option<(u64, usize)>,
}

impl Reader {
    pub fn follow_top(&self, active: Option<usize>) -> usize {
        active
            .map(|active| self.rows.partition_point(|(index, _)| *index < active))
            .unwrap_or_default()
            .saturating_sub(self.viewport_height / 2)
            .min(self.rows.len().saturating_sub(self.viewport_height))
    }
}

#[derive(Debug)]
pub(crate) struct Editor {
    pub parent: Box<Popup>,
    pub track_id: i64,
    pub query: u64,
    pub content_id: Option<String>,
    pub versions: Vec<LyricsVersion>,
    pub cursor: usize,
    pub draft: Option<Draft>,
    pub busy: bool,
    pub confirm_delete: bool,
    pub error: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Draft {
    pub replaced: Option<String>,
    pub label: LineEdit,
    pub text: LineEdit,
    pub label_focused: bool,
}

#[derive(Debug)]
pub(crate) enum Event {
    Local {
        serial: u64,
        content_id: Option<String>,
        saved: SavedLyrics,
    },
    Found {
        serial: u64,
        versions: Vec<LyricsVersion>,
    },
    Done {
        serial: u64,
    },
    Editor {
        query: u64,
        result: Result<(String, SavedLyrics), String>,
    },
    Edited {
        query: u64,
        result: Result<(String, SavedLyrics), String>,
    },
}

fn send(tx: &tokio::sync::mpsc::UnboundedSender<AppEvent>, event: Event) {
    let _ = tx.send(AppEvent::Lyrics(event));
}

pub(super) fn refresh(state: &mut AppState, runtime: &Runtime) {
    let track = state
        .player
        .current
        .as_ref()
        .or(match &state.popup {
            Some(Popup::Lyrics(reader)) => reader.fallback.as_deref(),
            _ => None,
        })
        .filter(|track| !track.is_podcast());
    let key = track.map(|track| (track.id, track_content_id(track)));
    let playback = &mut state.lyrics;
    // The pending, streaming and imported copies can change row id during one song.
    let same = playback.track == key
        || matches!((&playback.track, &key),
        (Some((_, Some(old))), Some((_, Some(new)))) if old == new);
    if same {
        if playback.loading || playback.retry_at.is_none_or(|at| at > Instant::now()) {
            return;
        }
    } else {
        if let Some(task) = playback.task.take() {
            task.abort();
        }
        *playback = Playback {
            track: key,
            serial: playback.serial.wrapping_add(1),
            title: track.map(|track| track.title.clone()).unwrap_or_default(),
            ..Playback::default()
        };
    }
    let Some(track) = track else {
        if !same && let Some(Popup::Lyrics(reader)) = state.popup.as_mut() {
            reader.top = None;
            reader.layout_key = None;
        }
        return;
    };
    playback.loading = true;
    playback.retry_at = None;
    let serial = playback.serial;
    let id = track.id;
    let content_id = playback
        .content_id
        .clone()
        .or_else(|| track_content_id(track));
    let owner = track.fed.as_ref().map(|fed| fed.owner.clone());
    if !same && let Some(Popup::Lyrics(reader)) = state.popup.as_mut() {
        reader.top = None;
        reader.layout_key = None;
    }
    let library = Arc::clone(&runtime.library);
    let federation = Arc::clone(&runtime.federation);
    let tx = runtime.event_tx.clone();
    let task = tokio::spawn(async move {
        let local = tokio::task::spawn_blocking({
            let library = Arc::clone(&library);
            move || -> anyhow::Result<_> {
                let content_id = match content_id {
                    Some(content_id) => Some(content_id),
                    None if id > 0 => library.track_content_id_by_id(id)?,
                    None => None,
                };
                let saved = content_id
                    .as_deref()
                    .map(|id| library.lyrics(id))
                    .transpose();
                Ok((content_id, saved))
            }
        })
        .await;
        if let Ok(Ok((content_id, saved))) = local {
            let saved = saved
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "could not read local lyrics");
                    None
                })
                .unwrap_or_default();
            let hidden = saved.hidden.clone();
            send(
                &tx,
                Event::Local {
                    serial,
                    content_id: content_id.clone(),
                    saved,
                },
            );
            if let Some(content_id) = content_id {
                let mut found = Vec::new();
                let _ = federation
                    .find_lyrics(&content_id, owner.as_deref(), |versions| {
                        let previous = found.len();
                        lyrics::merge(&mut found, versions, &hidden);
                        if found.len() > previous {
                            let versions = found[previous..].to_vec();
                            if federation.settings().save_on_listen {
                                store_found(
                                    Arc::clone(&library),
                                    content_id.clone(),
                                    versions.clone(),
                                );
                            }
                            send(&tx, Event::Found { serial, versions });
                        }
                    })
                    .await;
            }
        } else {
            tracing::warn!("lyrics library lookup failed");
        }
        send(&tx, Event::Done { serial });
    });
    playback.task = Some(task.abort_handle());
}

fn store_found(
    library: Arc<crate::library::Library>,
    content_id: String,
    versions: Vec<LyricsVersion>,
) {
    if !versions.is_empty() {
        tokio::task::spawn_blocking(move || {
            if let Err(error) = library.merge_lyrics(&content_id, versions) {
                tracing::warn!(%error, %content_id, "could not save peer lyrics");
            }
        });
    }
}

/// Audio can become permanent after the lyrics request already completed.
pub(super) fn on_import(state: &AppState, runtime: &Runtime, track: &TrackItem) {
    let Some(content_id) = track_content_id(track) else {
        return;
    };
    if state.lyrics.content_id.as_deref() == Some(content_id.as_str()) {
        store_found(
            Arc::clone(&runtime.library),
            content_id,
            state.lyrics.versions.clone(),
        );
    } else if runtime.federation.settings().save_on_listen {
        // The queue could have advanced while the audio was still downloading.
        let federation = Arc::clone(&runtime.federation);
        let library = Arc::clone(&runtime.library);
        tokio::spawn(async move {
            let mut versions = Vec::new();
            let _ = federation
                .find_lyrics(&content_id, None, |incoming| {
                    lyrics::merge(&mut versions, incoming, &HashSet::new());
                })
                .await;
            store_found(library, content_id, versions);
        });
    }
}

pub(super) fn handle_event(state: &mut AppState, event: Event) {
    let playback = &mut state.lyrics;
    match event {
        Event::Local {
            serial,
            content_id,
            saved,
        } if playback.serial == serial => {
            playback.content_id = content_id;
            playback.saved(saved);
        }
        Event::Found { serial, versions } if playback.serial == serial => playback.add(versions),
        Event::Done { serial } if playback.serial == serial => {
            playback.task = None;
            playback.loading = false;
            playback.retry_at = Some(Instant::now() + Duration::from_secs(60));
        }
        Event::Editor { query, result } | Event::Edited { query, result } => {
            if let Ok((content_id, saved)) = &result
                && playback.content_id.as_deref() == Some(content_id)
            {
                playback.saved(SavedLyrics {
                    versions: saved.versions.clone(),
                    hidden: saved.hidden.clone(),
                });
            }
            if let Some(Popup::LyricsEditor(editor)) = &mut state.popup
                && editor.query == query
            {
                editor.busy = false;
                match result {
                    Ok((content_id, saved)) => {
                        editor.content_id = Some(content_id);
                        editor.versions = saved.versions;
                        editor.cursor = editor.cursor.min(editor.versions.len().saturating_sub(1));
                        editor.draft = None;
                        editor.error = None;
                    }
                    Err(error) => editor.error = Some(error),
                }
            }
        }
        _ => {}
    }
}

pub(super) fn reader_key(state: &mut AppState, mut reader: Reader, key: KeyEvent) {
    let playback = &mut state.lyrics;
    let last = reader.rows.len().saturating_sub(reader.viewport_height);
    let top = reader.top.unwrap_or_else(|| {
        reader.follow_top(playback.document.line_at(state.player.position_secs))
    });
    let page = reader.viewport_height.saturating_sub(1).max(1);
    match key.code {
        KeyCode::Esc | KeyCode::Char('l' | 'q') => return,
        KeyCode::Up | KeyCode::Char('k') => reader.top = Some(top.saturating_sub(1)),
        KeyCode::Down | KeyCode::Char('j') => reader.top = Some((top + 1).min(last)),
        KeyCode::PageUp => reader.top = Some(top.saturating_sub(page)),
        KeyCode::PageDown => reader.top = Some((top + page).min(last)),
        KeyCode::Home => reader.top = Some(0),
        KeyCode::End => reader.top = Some(last),
        KeyCode::Char('f') => reader.top = None,
        KeyCode::Right | KeyCode::Tab if !playback.versions.is_empty() => {
            playback.select((playback.selected + 1) % playback.versions.len());
            reader.top = None;
        }
        KeyCode::Left | KeyCode::BackTab if !playback.versions.is_empty() => {
            playback.select(
                (playback.selected + playback.versions.len() - 1) % playback.versions.len(),
            );
            reader.top = None;
        }
        _ => {}
    }
    state.popup = Some(Popup::Lyrics(reader));
}

pub(super) fn open_editor(state: &mut AppState, runtime: &Runtime, parent: Popup, track_id: i64) {
    static NEXT_QUERY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let query = NEXT_QUERY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    state.popup = Some(Popup::LyricsEditor(Box::new(Editor {
        parent: Box::new(parent),
        track_id,
        query,
        content_id: None,
        versions: Vec::new(),
        cursor: 0,
        draft: None,
        busy: true,
        confirm_delete: false,
        error: None,
    })));
    let tx = runtime.event_tx.clone();
    let library = Arc::clone(&runtime.library);
    tokio::task::spawn_blocking(move || {
        let result = (|| -> anyhow::Result<_> {
            let content_id = match library.track_content_id_by_id(track_id)? {
                Some(id) => Some(id),
                None => library
                    .tracks_by_ids(&[track_id])?
                    .first()
                    .and_then(|track| crate::library::audio_content_id(&track.file_path)),
            }
            .ok_or_else(|| anyhow::anyhow!("Could not identify the local audio file"))?;
            let saved = library.lyrics(&content_id)?;
            Ok((content_id, saved))
        })()
        .map_err(|error| format!("{error:#}"));
        send(&tx, Event::Editor { query, result });
    });
}

pub(super) fn editor_key(
    state: &mut AppState,
    runtime: &Runtime,
    mut editor: Box<Editor>,
    key: KeyEvent,
) {
    if !editor.busy {
        if let Some(draft) = &mut editor.draft {
            match key.code {
                KeyCode::Esc => {
                    editor.draft = None;
                    editor.error = None;
                }
                KeyCode::F(2) => save_draft(runtime, &mut editor),
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    save_draft(runtime, &mut editor)
                }
                KeyCode::Tab | KeyCode::BackTab => draft.label_focused = !draft.label_focused,
                _ if draft.label_focused => {
                    if draft.label.len() < 512 || !matches!(key.code, KeyCode::Char(_)) {
                        draft.label.handle_key(key);
                    }
                }
                KeyCode::Enter if draft.text.len() < MAX_LYRICS_TEXT_BYTES => {
                    draft.text.insert('\n')
                }
                KeyCode::Up => draft.text.move_line(-1),
                KeyCode::Down => draft.text.move_line(1),
                KeyCode::Home => draft.text.line_edge(false),
                KeyCode::End => draft.text.line_edge(true),
                _ => {
                    if draft.text.len() < MAX_LYRICS_TEXT_BYTES
                        || !matches!(key.code, KeyCode::Char(_))
                    {
                        draft.text.handle_key(key);
                    }
                }
            }
        } else if editor.confirm_delete {
            editor.confirm_delete = false;
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Enter) {
                let id = editor
                    .versions
                    .get(editor.cursor)
                    .map(|entry| entry.id.clone());
                submit(runtime, &mut editor, id, None);
            }
        } else {
            match key.code {
                KeyCode::Esc => {
                    state.popup = Some(*editor.parent);
                    return;
                }
                KeyCode::Up => editor.cursor = editor.cursor.saturating_sub(1),
                KeyCode::Down => {
                    editor.cursor = (editor.cursor + 1).min(editor.versions.len().saturating_sub(1))
                }
                KeyCode::Char('a') if editor.content_id.is_some() => {
                    editor.error = None;
                    editor.draft = Some(Draft {
                        replaced: None,
                        label: LineEdit::default(),
                        text: LineEdit::default(),
                        label_focused: false,
                    });
                }
                KeyCode::Enter if editor.content_id.is_some() => {
                    if let Some(entry) = editor.versions.get(editor.cursor) {
                        editor.error = None;
                        editor.draft = Some(Draft {
                            replaced: Some(entry.id.clone()),
                            label: LineEdit::new(
                                lyrics::clean_text(&entry.label).replace('\n', " "),
                            ),
                            text: LineEdit::new(lyrics::clean_text(
                                entry
                                    .synced_lrc
                                    .as_deref()
                                    .or(entry.plain_text.as_deref())
                                    .unwrap_or_default(),
                            )),
                            label_focused: false,
                        });
                    }
                }
                KeyCode::Delete | KeyCode::Char('d') if !editor.versions.is_empty() => {
                    editor.confirm_delete = true
                }
                _ => {}
            }
        }
    }
    state.popup = Some(Popup::LyricsEditor(editor));
}

fn save_draft(runtime: &Runtime, editor: &mut Editor) {
    if let Some(draft) = editor.draft.as_ref() {
        match lyrics::manual_version(&draft.label, &draft.text) {
            Ok(version) => submit(runtime, editor, draft.replaced.clone(), Some(version)),
            Err(error) => editor.error = Some(format!("{error:#}")),
        }
    }
}

fn submit(
    runtime: &Runtime,
    editor: &mut Editor,
    replaced: Option<String>,
    version: Option<LyricsVersion>,
) {
    let Some(content_id) = editor.content_id.clone() else {
        return;
    };
    let library = Arc::clone(&runtime.library);
    let track_id = editor.track_id;
    let tx = runtime.event_tx.clone();
    let query = editor.query;
    editor.busy = true;
    editor.error = None;
    tokio::task::spawn_blocking(move || {
        let result = (|| -> anyhow::Result<_> {
            let track = library
                .tracks_by_ids(&[track_id])?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("The local track has been removed"))?;
            let current_id = track_content_id(&track)
                .or_else(|| crate::library::audio_content_id(&track.file_path));
            anyhow::ensure!(
                current_id.as_deref() == Some(&content_id),
                "The local audio has changed"
            );
            let saved = library.edit_lyrics(track_id, &content_id, replaced.as_deref(), version)?;
            Ok((content_id, saved))
        })()
        .map_err(|error| format!("{error:#}"));
        send(&tx, Event::Edited { query, result });
    });
}

pub(super) fn paste(editor: &mut Editor, pasted: &str) {
    if editor.busy {
        return;
    }
    let Some(draft) = &mut editor.draft else {
        return;
    };
    let cleaned = lyrics::clean_text(pasted);
    let (input, text, limit): (_, _, usize) = if draft.label_focused {
        (&mut draft.label, cleaned.replace('\n', " "), 512)
    } else {
        (&mut draft.text, cleaned, MAX_LYRICS_TEXT_BYTES)
    };
    if text.len() <= limit.saturating_sub(input.len()) {
        input.insert_str(&text);
        editor.error = None;
    } else {
        editor.error = Some("Pasted text exceeds the field size limit".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_peer_results_cannot_replace_the_current_track_or_selected_version() {
        let mut state = AppState::default();
        state.lyrics.serial = 2;
        let local = lyrics::manual_version("Local", "Current song").unwrap();
        state.lyrics.saved(SavedLyrics {
            versions: vec![local.clone()],
            ..Default::default()
        });
        handle_event(
            &mut state,
            Event::Found {
                serial: 1,
                versions: vec![lyrics::manual_version("Old", "Previous song").unwrap()],
            },
        );
        assert_eq!(state.lyrics.versions, [local]);
        handle_event(
            &mut state,
            Event::Found {
                serial: 2,
                versions: vec![lyrics::manual_version("Peer", "[00:01]Timed song").unwrap()],
            },
        );
        assert_eq!(state.lyrics.versions.len(), 2);
        assert_eq!(state.lyrics.document.lines, ["Current song"]);
        reader_key(
            &mut state,
            Reader::default(),
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        );
        assert_eq!(state.lyrics.document.line_at(1.0), Some(0));
        assert_eq!(state.lyrics.document.lines, ["Timed song"]);
    }

    #[test]
    fn reader_scrolls_plain_text_one_row_per_keypress() {
        let mut state = AppState::default();
        let reader = Reader {
            viewport_height: 4,
            rows: (0..8)
                .map(|index| (index, format!("Line {index}")))
                .collect(),
            ..Default::default()
        };
        reader_key(
            &mut state,
            reader,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        );
        assert!(matches!(
            state.popup,
            Some(Popup::Lyrics(Reader { top: Some(1), .. }))
        ));
    }
}
