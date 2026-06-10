//! System media-key integration (play/pause/next/prev from the OS).
//!
//! Backends via souvlaki: MPRIS over D-Bus on Linux, MPNowPlayingInfoCenter /
//! MPRemoteCommandCenter on macOS, SMTC on Windows. On macOS the command
//! callbacks are only delivered while the main thread services its CFRunLoop,
//! so the app runs on a worker thread and `run_on_main_thread` keeps the main
//! thread pumping the run loop and applying metadata updates.
//!
//! Windows note: SMTC needs a window handle; creating a hidden window is not
//! wired up yet, so media keys are skipped there with a log line.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig};

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
    let config = PlatformConfig {
        display_name: "Furumi",
        dbus_name: "cy.hexor.furumi_cli",
        hwnd: None,
    };
    if cfg!(windows) {
        tracing::info!("media keys: hidden-window SMTC setup not implemented yet, skipping");
        return None;
    }
    match MediaControls::new(config) {
        Ok(controls) => Some(controls),
        Err(err) => {
            tracing::warn!(?err, "media controls unavailable");
            None
        }
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
            duration: (duration_secs > 0.0)
                .then(|| Duration::from_secs_f64(duration_secs)),
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

#[cfg(not(target_os = "macos"))]
fn pump_platform_events() {}
