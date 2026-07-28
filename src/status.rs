//! Cheap now-playing export for status bars and other polling clients.
//!
//! The UI only sends snapshots through a bounded channel. A dedicated thread
//! performs the filesystem writes so a slow filesystem cannot stall drawing
//! or input handling.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::app::state::PlayerBar;

const STALE_AFTER_MS: u64 = 5_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlaybackStatus {
    pub playing: bool,
    pub paused: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub position_secs: f64,
    pub duration_secs: f64,
    pub volume: u8,
    pub updated_at_ms: u64,
}

impl PlaybackStatus {
    pub fn from_player(player: &PlayerBar) -> Option<Self> {
        let track = player.current.as_ref()?;
        player.playing.then(|| Self {
            playing: true,
            paused: player.paused,
            title: track.title.clone(),
            artist: track.artist_line(),
            album: track.release_title.clone(),
            position_secs: player.position_secs.max(0.0),
            duration_secs: track.duration_seconds.max(0.0),
            volume: player.volume,
            updated_at_ms: now_ms(),
        })
    }

    pub fn one_line(&self) -> String {
        let icon = if self.paused { "⏸" } else { "▶" };
        let track = if self.artist.is_empty() {
            self.title.clone()
        } else {
            format!("{} — {}", self.artist, self.title)
        };
        format!(
            "{icon} {track} {}/{}",
            duration(self.position_secs),
            duration(self.duration_secs)
        )
    }
}

pub struct Publisher {
    tx: Option<SyncSender<Option<PlaybackStatus>>>,
}

impl Publisher {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("playback-status".to_string())
            .spawn(move || {
                while let Ok(snapshot) = rx.recv() {
                    if let Some(snapshot) = snapshot {
                        if let Err(err) = write(&snapshot) {
                            tracing::debug!(?err, "writing playback status failed");
                        }
                    } else {
                        remove();
                    }
                }
                remove();
            })
            .ok();
        Self { tx: Some(tx) }
    }

    pub fn publish(&self, snapshot: Option<PlaybackStatus>) {
        let Some(tx) = &self.tx else { return };
        match tx.try_send(snapshot) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.tx.take();
    }
}

pub fn print(json: bool) -> anyhow::Result<()> {
    let Some(status) = read()? else {
        return Ok(());
    };
    if json {
        println!("{}", serde_json::to_string(&status)?);
    } else {
        println!("{}", status.one_line());
    }
    Ok(())
}

fn read() -> anyhow::Result<Option<PlaybackStatus>> {
    let Some(path) = path() else {
        return Ok(None);
    };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let status: PlaybackStatus = serde_json::from_slice(&bytes)?;
    if now_ms().saturating_sub(status.updated_at_ms) > STALE_AFTER_MS {
        return Ok(None);
    }
    Ok(Some(status))
}

fn write(status: &PlaybackStatus) -> anyhow::Result<()> {
    let Some(path) = path() else {
        return Ok(());
    };
    let Some(dir) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(dir)?;
    let temporary = dir.join("playback-status.tmp");
    fs::write(&temporary, serde_json::to_vec(status)?)?;
    #[cfg(windows)]
    if path.exists() {
        let _ = fs::remove_file(&path);
    }
    fs::rename(temporary, path)?;
    Ok(())
}

fn remove() {
    if let Some(path) = path() {
        let _ = fs::remove_file(path);
    }
}

fn path() -> Option<PathBuf> {
    crate::config::project_dirs().map(|dirs| dirs.cache_dir().join("playback-status.json"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_contains_state_metadata_and_progress() {
        let status = PlaybackStatus {
            playing: true,
            paused: true,
            title: "Track".into(),
            artist: "Artist".into(),
            album: "Release".into(),
            position_secs: 83.0,
            duration_secs: 245.0,
            volume: 80,
            updated_at_ms: 0,
        };
        assert_eq!(status.one_line(), "⏸ Artist — Track 1:23/4:05");
    }
}
