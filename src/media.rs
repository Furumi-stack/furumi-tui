//! System media-key integration (play/pause/next/prev from the OS).
//!
//! Backends via souvlaki: MPRIS over D-Bus on Linux, MPNowPlayingInfoCenter /
//! MPRemoteCommandCenter on macOS, SMTC on Windows. On macOS the command
//! callbacks are only delivered while the main thread services its CFRunLoop,
//! so the app runs on a worker thread and `run_on_main_thread` keeps the main
//! thread pumping the run loop and applying metadata updates. On Windows,
//! SMTC needs a window: a hidden one is created here and its message queue
//! is pumped the same way.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};

/// Commands arriving from the OS media keys, translated for the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    TogglePause,
    Play,
    Pause,
    Next,
    Previous,
    Stop,
}

/// Now-playing updates pushed from the app to the OS.
#[derive(Debug)]
pub enum MediaUpdate {
    Metadata {
        title: String,
        artist: String,
        album: String,
        duration_secs: f64,
    },
    Playback {
        playing: bool,
        paused: bool,
        position_secs: f64,
    },
    Stopped,
}

/// Runs until the app drops its `Sender<MediaUpdate>` (i.e. until quit).
/// Must be called on the process main thread (macOS requirement).
pub fn run_on_main_thread(
    updates: Receiver<MediaUpdate>,
    on_command: impl Fn(MediaCommand) + Send + 'static,
) {
    let mut controls = match create_controls() {
        Some(controls) => controls,
        None => {
            // No OS integration: just wait for the app to finish.
            while updates.recv().is_ok() {}
            return;
        }
    };
    if let Err(err) = controls.attach(move |event| {
        let command = match event {
            MediaControlEvent::Toggle => MediaCommand::TogglePause,
            MediaControlEvent::Play => MediaCommand::Play,
            MediaControlEvent::Pause => MediaCommand::Pause,
            MediaControlEvent::Next => MediaCommand::Next,
            MediaControlEvent::Previous => MediaCommand::Previous,
            MediaControlEvent::Stop => MediaCommand::Stop,
            _ => return,
        };
        on_command(command);
    }) {
        tracing::warn!(?err, "attaching media key handler failed");
        while updates.recv().is_ok() {}
        return;
    }
    tracing::info!("media keys attached");

    loop {
        pump_platform_events();
        match updates.recv_timeout(Duration::from_millis(200)) {
            Ok(update) => apply(&mut controls, update),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = controls.detach();
}

fn create_controls() -> Option<MediaControls> {
    // SMTC on Windows attaches to a window; console apps create a hidden one.
    #[cfg(target_os = "windows")]
    let hwnd = match create_hidden_window() {
        Some(hwnd) => Some(hwnd),
        None => {
            tracing::warn!("media keys: hidden window creation failed");
            return None;
        }
    };
    #[cfg(not(target_os = "windows"))]
    let hwnd = None;

    let config = PlatformConfig {
        display_name: "Furumi",
        dbus_name: "cy.hexor.furumi",
        hwnd,
    };
    match MediaControls::new(config) {
        Ok(controls) => Some(controls),
        Err(err) => {
            tracing::warn!(?err, "media controls unavailable");
            None
        }
    }
}

/// An invisible top-level window owning the SMTC session. Created on the
/// main thread, which also pumps its messages in `pump_platform_events`.
#[cfg(target_os = "windows")]
fn create_hidden_window() -> Option<*mut std::ffi::c_void> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, RegisterClassW, WNDCLASSW,
    };
    unsafe {
        let class_name: Vec<u16> = "furumi_media_keys\0".encode_utf16().collect();
        let instance = GetModuleHandleW(core::ptr::null());
        let mut class: WNDCLASSW = core::mem::zeroed();
        class.lpfnWndProc = Some(DefWindowProcW);
        class.hInstance = instance;
        class.lpszClassName = class_name.as_ptr();
        if RegisterClassW(&class) == 0 {
            return None;
        }
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            instance,
            core::ptr::null(),
        );
        if hwnd.is_null() { None } else { Some(hwnd) }
    }
}

fn apply(controls: &mut MediaControls, update: MediaUpdate) {
    let result = match update {
        MediaUpdate::Metadata {
            title,
            artist,
            album,
            duration_secs,
        } => controls.set_metadata(MediaMetadata {
            title: Some(&title),
            artist: Some(&artist),
            album: Some(&album),
            duration: (duration_secs > 0.0).then(|| Duration::from_secs_f64(duration_secs)),
            cover_url: None,
        }),
        MediaUpdate::Playback {
            playing,
            paused,
            position_secs,
        } => {
            let progress = Some(MediaPosition(Duration::from_secs_f64(
                position_secs.max(0.0),
            )));
            let playback = if !playing {
                MediaPlayback::Stopped
            } else if paused {
                MediaPlayback::Paused { progress }
            } else {
                MediaPlayback::Playing { progress }
            };
            controls.set_playback(playback)
        }
        MediaUpdate::Stopped => controls.set_playback(MediaPlayback::Stopped),
    };
    if let Err(err) = result {
        tracing::debug!(?err, "media update failed");
    }
}

/// On macOS, MPRemoteCommandCenter callbacks arrive only while the main
/// thread's CFRunLoop is running; pump it briefly every cycle.
#[cfg(target_os = "macos")]
fn pump_platform_events() {
    use core_foundation::runloop::{CFRunLoop, kCFRunLoopDefaultMode};
    CFRunLoop::run_in_mode(
        unsafe { kCFRunLoopDefaultMode },
        Duration::from_millis(50),
        false,
    );
}

/// On Windows the hidden SMTC window needs its message queue drained on the
/// thread that created it.
#[cfg(target_os = "windows")]
fn pump_platform_events() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };
    unsafe {
        let mut msg: MSG = core::mem::zeroed();
        while PeekMessageW(&mut msg, core::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn pump_platform_events() {}
