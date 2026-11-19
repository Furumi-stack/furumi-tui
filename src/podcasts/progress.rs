//! Durable episode progress: one background thread writes every progress
//! action of this device to the podcast store and the sync log, in order.
//!
//! The rules come from the FRID contract (`music_dht::podcast::state`,
//! `PodcastStore` progress actions); this module only decides *when* to
//! call them:
//!
//! * a playback start is an explicit action (`start_session`, or `restart`
//!   for "start over") that takes a new progress generation;
//! * periodic checkpoints, pause and the final save keep the session's
//!   generation (`checkpoint`), so a late checkpoint of an older session
//!   always loses against a newer explicit action;
//! * a seek is explicit (`seek`) and becomes the session's generation;
//! * reaching the end (`reaches_completion` or the end of the stream) is
//!   `complete_session`;
//! * when the store reports a newer generation (`Superseded`), the session
//!   stops writing checkpoints until the user's next explicit action;
//! * the listening session itself (`ListenSession`) is recorded at pause,
//!   completion and end; the store only keeps sessions that pass the shared
//!   history threshold and only when they grew.
//!
//! "Mark played" / "Mark unplayed" go through the same thread so they are
//! ordered with the checkpoints of a playing episode.
//!
//! Writes never happen on the UI or audio thread, and a failed write is
//! reported and logged without stopping playback.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, anyhow};
use furumi_library::podcasts::{
    CheckpointOutcome, PodcastLocalOp, PodcastStore, ProgressSession, ProgressWrite, StampSource,
};
use music_dht::podcast::{EpisodeId, ListenSession, PodcastEpisodeRef, ProgressRecord};

use super::OpLog;
use super::refresh::Services;

/// How often a playing episode saves its position.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(20);

/// What the UI asks the writer to do. `playback` identifies one start of
/// one episode on this device; commands for a playback that is no longer
/// the active one are ignored (they can only arrive late).
#[derive(Debug, Clone)]
pub enum ProgressCommand {
    /// Playback actually started at `position_ms`.
    Begin {
        playback: u64,
        episode: Box<PodcastEpisodeRef>,
        session_id: String,
        position_ms: i64,
        /// "Start over" instead of an ordinary start or resume.
        restart: bool,
        now_ms: i64,
    },
    /// Periodic save while playing.
    Checkpoint { playback: u64, position_ms: i64 },
    /// Paused: save the position and the listening session so far.
    Pause {
        playback: u64,
        position_ms: i64,
        listened_ms: i64,
        now_ms: i64,
    },
    /// The user moved to `position_ms`.
    Seek { playback: u64, position_ms: i64 },
    /// The position reached the end of the episode.
    Complete {
        playback: u64,
        position_ms: i64,
        listened_ms: i64,
        now_ms: i64,
    },
    /// Playback of this start ended (stop, next item, handoff, exit…).
    End {
        playback: u64,
        position_ms: i64,
        listened_ms: i64,
        now_ms: i64,
    },
    /// "Mark played" / "Mark unplayed" from the episode list.
    SetPlayed { episode_id: EpisodeId, played: bool },
}

/// What the writer reports back.
#[derive(Debug, Clone, PartialEq)]
pub enum ProgressEvent {
    /// The progress register of an episode was written.
    Saved(ProgressRecord),
    /// A newer explicit action holds the register; this playback stopped
    /// saving its position.
    Superseded { current: ProgressRecord },
    /// A playback ended (its session was recorded when long enough).
    Ended { episode_id: EpisodeId },
    /// The result of a `SetPlayed` command.
    PlayedSet {
        played: bool,
        result: Result<(), String>,
    },
    /// A write failed; playback goes on.
    Failed(String),
}

/// Handle of the writer thread.
pub struct ProgressWriter {
    tx: Option<Sender<ProgressCommand>>,
    thread: Option<JoinHandle<()>>,
}

impl ProgressWriter {
    pub fn spawn(
        services: Services,
        on_event: impl Fn(ProgressEvent) + Send + 'static,
    ) -> Result<ProgressWriter> {
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("podcast-progress".to_string())
            .spawn(move || run(rx, &services.store, services.log.as_ref(), on_event))
            .map_err(|err| anyhow!("starting the podcast progress writer: {err}"))?;
        Ok(ProgressWriter {
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    pub fn send(&self, command: ProgressCommand) {
        if let Some(tx) = &self.tx
            && tx.send(command).is_err()
        {
            tracing::warn!("podcast progress writer stopped; a progress update was lost");
        }
    }

    /// Stops accepting commands and waits up to `timeout` for the queued
    /// ones to be written. Blocking; used at exit.
    pub fn close(&mut self, timeout: Duration) -> bool {
        self.tx = None;
        let Some(thread) = self.thread.take() else {
            return true;
        };
        let deadline = std::time::Instant::now() + timeout;
        while !thread.is_finished() {
            if std::time::Instant::now() >= deadline {
                tracing::warn!("podcast progress writer did not finish before exit");
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = thread.join();
        true
    }
}

/// The playback whose session the writer is maintaining.
struct Active {
    playback: u64,
    /// `None` when starting the session failed: nothing is written for it.
    session: Option<ProgressSession>,
    episode: PodcastEpisodeRef,
    session_id: String,
    started_at_ms: i64,
    start_position_ms: i64,
    /// A newer generation holds the register: no more checkpoints.
    superseded: bool,
    /// The register says this session completed the episode (a later seek
    /// back makes it "in progress" again).
    completed: bool,
    /// The session reached the end at least once; it counts as a completed
    /// listen whatever happens afterwards.
    listen_completed: bool,
}

fn run(
    rx: Receiver<ProgressCommand>,
    store: &Arc<PodcastStore>,
    log: &dyn OpLog,
    on_event: impl Fn(ProgressEvent),
) {
    let mut writer = Writer {
        store,
        log,
        active: None,
        on_event: &on_event,
    };
    while let Ok(command) = rx.recv() {
        writer.handle(command);
    }
}

struct Writer<'a, F: Fn(ProgressEvent)> {
    store: &'a PodcastStore,
    log: &'a dyn OpLog,
    active: Option<Active>,
    on_event: &'a F,
}

/// Runs a store action through the sync log and returns its value.
fn record<T>(
    log: &dyn OpLog,
    mut action: impl FnMut(&mut dyn StampSource) -> Result<(T, Vec<PodcastLocalOp>)>,
) -> Result<T> {
    let mut value = None;
    log.record(&mut |stamps| {
        let (result, ops) = action(stamps)?;
        value = Some(result);
        Ok(ops)
    })?;
    value.ok_or_else(|| anyhow!("the podcast action did not run"))
}

impl<F: Fn(ProgressEvent)> Writer<'_, F> {
    fn emit(&self, event: ProgressEvent) {
        (self.on_event)(event);
    }

    fn fail(&self, what: &str, err: anyhow::Error) {
        let message = format!("podcast progress: {what} failed: {err:#}");
        tracing::warn!("{message}");
        self.emit(ProgressEvent::Failed(message));
    }

    fn active_mut(&mut self, playback: u64) -> Option<&mut Active> {
        self.active
            .as_mut()
            .filter(|active| active.playback == playback)
    }

    fn handle(&mut self, command: ProgressCommand) {
        match command {
            ProgressCommand::Begin {
                playback,
                episode,
                session_id,
                position_ms,
                restart,
                now_ms,
            } => self.begin(playback, *episode, session_id, position_ms, restart, now_ms),
            ProgressCommand::Checkpoint {
                playback,
                position_ms,
            } => self.checkpoint(playback, position_ms),
            ProgressCommand::Pause {
                playback,
                position_ms,
                listened_ms,
                now_ms,
            } => {
                self.checkpoint(playback, position_ms);
                self.record_session(playback, position_ms, listened_ms, now_ms);
            }
            ProgressCommand::Seek {
                playback,
                position_ms,
            } => self.seek(playback, position_ms),
            ProgressCommand::Complete {
                playback,
                position_ms,
                listened_ms,
                now_ms,
            } => {
                self.complete(playback, position_ms);
                self.record_session(playback, position_ms, listened_ms, now_ms);
            }
            ProgressCommand::End {
                playback,
                position_ms,
                listened_ms,
                now_ms,
            } => {
                self.checkpoint(playback, position_ms);
                self.record_session(playback, position_ms, listened_ms, now_ms);
                if let Some(active) = self.active.take_if(|active| active.playback == playback) {
                    self.emit(ProgressEvent::Ended {
                        episode_id: active.episode.episode_id,
                    });
                }
            }
            ProgressCommand::SetPlayed { episode_id, played } => {
                let result = self.set_played(&episode_id, played);
                if let Err(err) = &result {
                    tracing::warn!("podcast progress: marking played failed: {err:#}");
                }
                self.emit(ProgressEvent::PlayedSet {
                    played,
                    result: result.map_err(|err| format!("{err:#}")),
                });
            }
        }
    }

    fn begin(
        &mut self,
        playback: u64,
        episode: PodcastEpisodeRef,
        session_id: String,
        position_ms: i64,
        restart: bool,
        now_ms: i64,
    ) {
        let store = self.store;
        let carried = episode.progress_generation;
        let result = record(self.log, |stamps| {
            let write = if restart {
                store.restart(stamps, &episode, &session_id)?
            } else {
                store.start_session(stamps, &episode, &session_id, position_ms, carried)?
            };
            let op = write.op.clone();
            Ok((write, vec![op]))
        });
        let session = match result {
            Ok(write) => {
                let session = write.session.clone();
                self.emit(ProgressEvent::Saved(write.record));
                Some(session)
            }
            Err(err) => {
                self.fail("starting a listening session", err);
                None
            }
        };
        self.active = Some(Active {
            playback,
            session,
            episode,
            session_id,
            started_at_ms: now_ms,
            start_position_ms: position_ms,
            superseded: false,
            completed: false,
            listen_completed: false,
        });
    }

    fn checkpoint(&mut self, playback: u64, position_ms: i64) {
        let store = self.store;
        let log = self.log;
        let Some(active) = self.active_mut(playback) else {
            return;
        };
        if active.superseded {
            return;
        }
        let Some(session) = active.session.clone() else {
            return;
        };
        let result = record(log, |stamps| {
            let outcome = store.checkpoint(stamps, &session, position_ms)?;
            let ops = match &outcome {
                CheckpointOutcome::Written(write) => vec![write.op.clone()],
                _ => Vec::new(),
            };
            Ok((outcome, ops))
        });
        match result {
            Ok(CheckpointOutcome::Written(write)) => self.emit(ProgressEvent::Saved(write.record)),
            Ok(CheckpointOutcome::Superseded { current }) => {
                if let Some(active) = self.active_mut(playback) {
                    active.superseded = true;
                }
                tracing::info!(
                    episode = %current.episode_id,
                    "a newer progress action took over; this playback stops saving"
                );
                self.emit(ProgressEvent::Superseded { current });
            }
            Ok(CheckpointOutcome::AlreadyCompleted { .. }) => {}
            Err(err) => self.fail("saving the position", err),
        }
    }

    /// Explicit actions of a running session (seek, completion): they take
    /// a new generation and resume saving after a supersede.
    fn explicit(
        &mut self,
        playback: u64,
        what: &str,
        action: impl Fn(&PodcastStore, &mut dyn StampSource, &ProgressSession) -> Result<ProgressWrite>,
    ) -> bool {
        let store = self.store;
        let log = self.log;
        let Some(active) = self.active_mut(playback) else {
            return false;
        };
        let Some(session) = active.session.clone() else {
            return false;
        };
        let result = record(log, |stamps| {
            let write = action(store, stamps, &session)?;
            let op = write.op.clone();
            Ok((write, vec![op]))
        });
        match result {
            Ok(write) => {
                if let Some(active) = self.active_mut(playback) {
                    active.session = Some(write.session.clone());
                    active.superseded = false;
                }
                self.emit(ProgressEvent::Saved(write.record));
                true
            }
            Err(err) => {
                self.fail(what, err);
                false
            }
        }
    }

    fn seek(&mut self, playback: u64, position_ms: i64) {
        if self.explicit(playback, "saving a seek", |store, stamps, session| {
            store.seek(stamps, session, position_ms)
        }) && let Some(active) = self.active_mut(playback)
        {
            active.completed = false;
        }
    }

    fn complete(&mut self, playback: u64, position_ms: i64) {
        if self
            .active_mut(playback)
            .is_none_or(|active| active.completed)
        {
            return;
        }
        if self.explicit(
            playback,
            "marking the episode finished",
            |store, stamps, session| store.complete_session(stamps, session, position_ms),
        ) && let Some(active) = self.active_mut(playback)
        {
            active.completed = true;
            active.listen_completed = true;
        }
    }

    /// Records (or grows) the listening session of `playback`.
    fn record_session(&mut self, playback: u64, position_ms: i64, listened_ms: i64, now_ms: i64) {
        let store = self.store;
        let log = self.log;
        let Some(active) = self.active_mut(playback) else {
            return;
        };
        let mut session = ListenSession {
            session_id: active.session_id.clone(),
            episode_id: active.episode.episode_id.clone(),
            podcast_id: active.episode.podcast_id.clone(),
            device_id: String::new(),
            started_at_ms: active.started_at_ms,
            ended_at_ms: now_ms.max(active.started_at_ms),
            start_position_ms: active.start_position_ms.max(0),
            end_position_ms: position_ms.max(0),
            listened_ms: listened_ms.max(0),
            completed: active.listen_completed,
        };
        let episode = active.episode.clone();
        let result = record(log, |stamps| {
            session.device_id = stamps.device_id().to_string();
            let op = store.record_listen_session(stamps, &session, Some(&episode))?;
            Ok(((), op.into_iter().collect()))
        });
        if let Err(err) = result {
            self.fail("recording the listening session", err);
        }
    }

    fn set_played(&mut self, episode_id: &EpisodeId, played: bool) -> Result<()> {
        let episode = self
            .store
            .episode_ref(episode_id)?
            .ok_or_else(|| anyhow!("the episode is no longer in the catalog"))?;
        let store = self.store;
        let write = record(self.log, |stamps| {
            let write = if played {
                store.mark_played(stamps, &episode)?
            } else {
                store.mark_unplayed(stamps, &episode)?
            };
            let op = write.op.clone();
            Ok((write, vec![op]))
        })?;
        self.emit(ProgressEvent::Saved(write.record));
        Ok(())
    }
}
