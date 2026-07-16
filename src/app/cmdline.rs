use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::Runtime;
use crate::app::command::{self, Command, Parsed};
use crate::app::event::AppEvent;
use crate::app::state::{AppState, GlobalView, SearchState, Tab};

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(180);
const SEARCH_LIMIT: i64 = 12;

/// Keys go here instead of the keymap while the command line is open.
pub fn handle_key(state: &mut AppState, runtime: &mut Runtime, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => cancel(state),
        KeyCode::Enter => commit(state, runtime),
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
        // One-shot commands have no live effect.
        Command::Quit
        | Command::Import(_)
        | Command::Volume(_)
        | Command::Seek(_)
        | Command::SeekTo(_)
        | Command::Shuffle
        | Command::Repeat(_)
        | Command::ClearQueue
        | Command::Next
        | Command::Prev
        | Command::PlayPause
        | Command::Help
        | Command::Logs(_) => {}
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
pub(super) fn schedule_search(state: &mut AppState, runtime: &Runtime) {
    let seq = runtime.search_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let query = state.search.query.clone();
    if query.is_empty() {
        state.search.loading = false;
        state.search.results = None;
        state.search.fed_tracks.clear();
        state.search.fed_loading = false;
        return;
    }
    let library = Arc::clone(&runtime.library);
    state.search.loading = true;
    let tx = runtime.event_tx.clone();
    let latest = Arc::clone(&runtime.search_seq);
    tokio::spawn(async move {
        tokio::time::sleep(SEARCH_DEBOUNCE).await;
        if latest.load(Ordering::SeqCst) != seq {
            return;
        }
        let result = tokio::task::spawn_blocking(move || library.search(&query, SEARCH_LIMIT))
            .await
            .map_err(|err| err.to_string())
            .and_then(|result| result.map_err(|err| format!("{err:#}")));
        let _ = tx.send(AppEvent::SearchLoaded { seq, result });
    });

    // The same query also runs against the federated network (when the
    // node is up); its results render as a separate, marked section.
    state.search.fed_tracks.clear();
    state.search.fed_loading = false;
    if runtime.federation.settings().enabled {
        state.search.fed_loading = true;
        let fed = Arc::clone(&runtime.federation);
        let query = state.search.query.clone();
        let tx = runtime.event_tx.clone();
        let latest = Arc::clone(&runtime.search_seq);
        tokio::spawn(async move {
            tokio::time::sleep(SEARCH_DEBOUNCE).await;
            if latest.load(Ordering::SeqCst) != seq {
                return;
            }
            let result = fed.search(&query).await.map_err(|err| format!("{err:#}"));
            let _ = tx.send(AppEvent::FedSearchLoaded { seq, result });
        });
    }
}

/// Enter: close the line. Live commands already took effect (their view
/// stays open); one-shot commands execute here.
fn commit(state: &mut AppState, runtime: &mut Runtime) {
    let parsed = command::parse(&state.cmdline.input);
    close(state);
    match parsed {
        Parsed::Empty => {}
        Parsed::Command(command) if command::is_live(&command) => {}
        Parsed::Command(command) => execute(state, runtime, command),
        Parsed::Invalid(usage) => state.status_message = Some(usage),
        Parsed::Unknown(name) => {
            state.status_message = Some(format!("unknown command: {name}"));
        }
    }
}

/// One-shot command execution. Most commands reuse the same Action/Effect
/// path as keybindings, so behavior stays identical.
fn execute(state: &mut AppState, runtime: &mut Runtime, command: Command) {
    use crate::app::action::Action;
    use crate::app::state::{LOG_LEVELS, RepeatMode, Tab};
    use crate::app::update::Effect;

    let run_action = |state: &mut AppState, runtime: &mut Runtime, action: Action| {
        if let Some(effect) = crate::app::update::update(state, action) {
            super::perform_effect(state, runtime, effect);
        }
    };
    match command {
        Command::Search(_) => {}
        Command::Quit => state.should_quit = true,
        Command::Import(path) => super::spawn_import(state, runtime, &path),
        Command::Volume(value) => {
            state.player.volume = value;
            super::perform_effect(state, runtime, Effect::SetVolume(value));
            state.status_message = Some(format!("volume {value}%"));
        }
        Command::Seek(delta) => {
            if state.player.current.is_some() {
                super::perform_effect(state, runtime, Effect::SeekBy(delta));
            }
        }
        Command::SeekTo(seconds) => {
            if state.player.current.is_some() {
                let delta = seconds as f64 - state.player.position_secs;
                super::perform_effect(state, runtime, Effect::SeekBy(delta.round() as i64));
            }
        }
        Command::Shuffle => run_action(state, runtime, Action::ToggleShuffle),
        Command::Repeat(None) => run_action(state, runtime, Action::CycleRepeat),
        Command::Repeat(Some(mode)) => {
            state.player.repeat = match mode {
                command::RepeatArg::Off => RepeatMode::Off,
                command::RepeatArg::One => RepeatMode::One,
                command::RepeatArg::All => RepeatMode::All,
            };
            super::perform_effect(state, runtime, Effect::SetOptions);
            state.status_message = Some(format!("repeat {}", state.player.repeat.label()));
        }
        Command::ClearQueue => run_action(state, runtime, Action::ClearQueue),
        Command::Next => run_action(state, runtime, Action::NextTrack),
        Command::Prev => run_action(state, runtime, Action::PrevTrack),
        Command::PlayPause => run_action(state, runtime, Action::PlayPause),
        Command::Help => state.help_visible = true,
        Command::Logs(level) => {
            if let Some(index) = level {
                state.logs.level_index = index.min(LOG_LEVELS.len() - 1);
                state.logs.follow = true;
                state.logs.selected_seq = None;
            }
            state.active_tab = Tab::Logs;
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
