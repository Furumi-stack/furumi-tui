mod api;
mod app;
mod art;
mod config;
mod media;
mod player;
mod ui;

use std::io;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};

fn main() -> Result<()> {
    let mut startup_warning = None;
    if let Err(err) = config::logging::init() {
        startup_warning = Some(format!("logging disabled: {err:#}"));
    }
    let (keymap, keymap_warning) = config::keymap::Keymap::load();
    let startup_warning = keymap_warning.or(startup_warning);

    // The app (tokio + TUI) runs on a worker thread; the main thread stays
    // dedicated to the OS media-key event loop — on macOS the system only
    // delivers media commands while the main thread pumps its CFRunLoop.
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<app::event::AppEvent>();
    let (media_tx, media_rx) = std::sync::mpsc::channel::<media::MediaUpdate>();

    let media_event_tx = event_tx.clone();
    let app_thread = std::thread::Builder::new()
        .name("app".to_string())
        .spawn(move || run_app(keymap, startup_warning, event_tx, event_rx, media_tx))
        .expect("spawning the app thread cannot fail");

    media::run_on_main_thread(media_rx, move |command| {
        let _ = media_event_tx.send(app::event::AppEvent::Media(command));
    });

    app_thread.join().expect("app thread panicked")
}

fn run_app(
    keymap: config::keymap::Keymap,
    startup_warning: Option<String>,
    event_tx: tokio::sync::mpsc::UnboundedSender<app::event::AppEvent>,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<app::event::AppEvent>,
    media_tx: std::sync::mpsc::Sender<media::MediaUpdate>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // ratatui::init() enables raw mode + alternate screen and installs a
    // panic hook that restores the terminal.
    let terminal = ratatui::init();
    let keyboard_enhanced = push_keyboard_enhancements();
    let bracketed_paste = crossterm::execute!(io::stdout(), EnableBracketedPaste).is_ok();
    if bracketed_paste {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
            previous_hook(info);
        }));
    }

    let result = runtime.block_on(app::run(
        terminal,
        keymap,
        startup_warning,
        event_tx,
        event_rx,
        media_tx,
    ));

    if bracketed_paste {
        let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
    }
    if keyboard_enhanced {
        let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    result
}

/// Kitty keyboard protocol, where supported, disambiguates Esc from alt-keys
/// and modifier combos. The flags are popped on exit and on panic — leaving
/// them pushed corrupts the user's shell.
fn push_keyboard_enhancements() -> bool {
    if !matches!(
        crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    ) {
        return false;
    }
    if crossterm::execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_err()
    {
        return false;
    }
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
        previous_hook(info);
    }));
    true
}
