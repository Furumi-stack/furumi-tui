mod app;
mod art;
mod config;
mod devices;
mod federation;
mod library;
mod media;
mod player;
mod share;
mod status;
mod streaming;
mod ui;
mod visualizer;

use std::io;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};

const HELP: &str = "\
Furumi — federated terminal music player

Usage:
  furumi [OPTION]

Options:
  -h, --help       Show this help
  -V, --version    Show version
      --status     Print a one-line now-playing status
      --status-json
                   Print now-playing status as JSON

tmux:
  set -g status-right '#(furumi --status)'
";

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{HELP}");
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("furumi {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args
        .iter()
        .any(|arg| arg == "--status" || arg == "--status-json")
    {
        return status::print(args.iter().any(|arg| arg == "--status-json"));
    }

    let mut startup_warning = None;
    if let Err(err) = config::logging::init() {
        startup_warning = Some(format!("logging disabled: {err:#}"));
    }
    capture_stderr();
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

/// C libraries (ALSA on some distros, in particular) print warnings straight
/// to stderr, which corrupts the TUI. Replace stderr with a pipe and forward
/// every line into tracing — it lands in the Logs tab and the log file
/// instead of the screen.
#[cfg(unix)]
fn capture_stderr() {
    use std::io::BufRead as _;
    use std::os::fd::FromRawFd as _;

    let mut fds = [0i32; 2];
    // SAFETY: plain pipe/dup2 syscalls on freshly created fds.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return;
        }
        let [read_fd, write_fd] = fds;
        if libc::dup2(write_fd, libc::STDERR_FILENO) == -1 {
            libc::close(read_fd);
            libc::close(write_fd);
            return;
        }
        libc::close(write_fd);
        let reader = std::fs::File::from_raw_fd(read_fd);
        std::thread::Builder::new()
            .name("stderr".to_string())
            .spawn(move || {
                for line in std::io::BufReader::new(reader).lines() {
                    let Ok(line) = line else { break };
                    if !line.trim().is_empty() {
                        tracing::warn!(target: "stderr", "{line}");
                    }
                }
            })
            .ok();
    }
}

#[cfg(windows)]
fn capture_stderr() {
    use std::io::BufRead as _;
    use std::os::windows::io::FromRawHandle as _;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};
    use windows_sys::Win32::System::Pipes::CreatePipe;

    unsafe {
        let mut read = core::ptr::null_mut();
        let mut write = core::ptr::null_mut();
        if CreatePipe(&mut read, &mut write, core::ptr::null(), 0) == 0 {
            return;
        }
        if SetStdHandle(STD_ERROR_HANDLE, write) == 0 {
            CloseHandle(read);
            CloseHandle(write);
            return;
        }
        let reader = std::fs::File::from_raw_handle(read);
        std::thread::Builder::new()
            .name("stderr".to_string())
            .spawn(move || {
                for line in std::io::BufReader::new(reader).lines() {
                    let Ok(line) = line else { break };
                    if !line.trim().is_empty() {
                        tracing::warn!(target: "stderr", "{line}");
                    }
                }
            })
            .ok();
    }
}

#[cfg(not(any(unix, windows)))]
fn capture_stderr() {}

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
