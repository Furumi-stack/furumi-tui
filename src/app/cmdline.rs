use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::api::client::ApiError;
use crate::app::Runtime;
use crate::app::command::{self, Command, Parsed};
use crate::app::event::AppEvent;
use crate::app::state::{AppState, GlobalView, SearchState, Tab};

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(180);
const SEARCH_LIMIT: i64 = 12;

/// Keys go here instead of the keymap while the command line is open.
pub fn handle_key(state: &mut AppState, runtime: &Runtime, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => cancel(state),
        KeyCode::Enter => commit(state),
        KeyCode::Backspace => {
            if state.cmdline.input.pop().is_none() {
                // Backspace on an empty line closes it, like vim.
                cancel(state);
                return;
            }
            after_change(state, runtime);
        }
        KeyCode::Char(c) if is_typing(key) => {
            state.cmdline.input.push(c);
            after_change(state, runtime);
        }
        _ => {}
    }
}

pub fn handle_paste(state: &mut AppState, runtime: &Runtime, pasted: &str) {
    let cleaned: String = pasted.chars().filter(|c| !c.is_control()).collect();
    state.cmdline.input.push_str(&cleaned);
    after_change(state, runtime);
}

fn is_typing(key: KeyEvent) -> bool {
    key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
}

/// Re-evaluate the input after every edit; live commands (search) take
/// effect immediately, while typing.
fn after_change(state: &mut AppState, runtime: &Runtime) {
    match command::parse(&state.cmdline.input) {
        Parsed::Command(command) if command::is_live(&command) => {
            apply_live(state, runtime, command);
        }
        _ => retract_live(state),
    }
}

fn apply_live(state: &mut AppState, runtime: &Runtime, command: Command) {
    match command {
        Command::Search(query) => {
            state.active_tab = Tab::Global;
            if !matches!(state.global.stack.last(), Some(GlobalView::Search { .. })) {
                state.global.stack.push(GlobalView::Search { cursor: 0 });
            }
            state.cmdline.live = true;
            if state.search.query != query {
                state.search.query = query;
                set_view_cursor_zero(state);
                schedule_search(state, runtime);
            }
        }
    }
}

fn set_view_cursor_zero(state: &mut AppState) {
    if let Some(GlobalView::Search { cursor }) = state.global.stack.last_mut() {
        *cursor = 0;
    }
}

/// Debounced, race-free search: every edit bumps the global sequence; the
/// spawned task only queries if it is still the latest after the debounce,
/// and the receiver drops responses that arrive out of date.
fn schedule_search(state: &mut AppState, runtime: &Runtime) {
    let seq = runtime.search_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let query = state.search.query.clone();
    if query.is_empty() {
        state.search.loading = false;
        state.search.results = None;
        return;
    }
    let Some(api) = runtime.api.clone() else {
        return;
    };
    state.search.loading = true;
    let tx = runtime.event_tx.clone();
    let latest = Arc::clone(&runtime.search_seq);
    tokio::spawn(async move {
        tokio::time::sleep(SEARCH_DEBOUNCE).await;
        if latest.load(Ordering::SeqCst) != seq {
            return;
        }
        let event = match api.search(&query, SEARCH_LIMIT).await {
            Ok(results) => AppEvent::SearchLoaded {
                seq,
                result: Ok(results),
            },
            Err(ApiError::SessionExpired) => AppEvent::SessionExpired,
            Err(err) => AppEvent::SearchLoaded {
                seq,
                result: Err(err.to_string()),
            },
        };
        let _ = tx.send(event);
    });
}

/// Enter: close the line. Live commands already took effect (their view
/// stays open); one-shot commands would execute here.
fn commit(state: &mut AppState) {
    let parsed = command::parse(&state.cmdline.input);
    close(state);
    match parsed {
        Parsed::Empty | Parsed::Command(_) => {}
        Parsed::Unknown(name) => {
            state.status_message = Some(format!("unknown command: {name}"));
        }
    }
}

/// Esc: close the line and undo any live effect it had.
fn cancel(state: &mut AppState) {
    retract_live(state);
    close(state);
}

fn close(state: &mut AppState) {
    state.cmdline.active = false;
    state.cmdline.input.clear();
    state.cmdline.live = false;
}

/// Pop the live search view if this command-line session opened it.
fn retract_live(state: &mut AppState) {
    if !state.cmdline.live {
        return;
    }
    state.cmdline.live = false;
    if matches!(state.global.stack.last(), Some(GlobalView::Search { .. })) {
        state.global.stack.pop();
        state.search = SearchState::default();
    }
}
