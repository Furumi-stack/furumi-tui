//! Podcast runtime of the TUI: feed HTTP, refresh scheduling and the glue
//! between the shared podcast store and the device-sync log.
//!
//! The data model, identity rules, merge rules and the SQLite store are not
//! defined here: they come from FRID (`music_dht::podcast` and
//! `furumi_library::podcasts::PodcastStore`, file `podcasts.sqlite3`). This
//! module only does what the contract leaves to clients:
//!
//! * [`fetch`] downloads feeds (conditional GET, redirects reported so
//!   permanent moves become feed aliases, size and time limits) and covers;
//! * [`refresh`] subscribes and refreshes feeds: it parses them with the
//!   shared `music_dht::podcast::feed` parser, ingests them with
//!   `PodcastStore::apply_feed` and keeps the local fetch state (validators,
//!   errors, back-off);
//! * [`OpLog`] hands store actions the stamps of the device-sync log and
//!   appends the operations they return;
//! * [`stream`] plays an episode straight from the publisher through a
//!   bounded HTTP Range reader, and [`playback`] resolves the current audio
//!   URL and resume position, opens and positions it;
//! * [`progress`] writes resume positions, completions and listening
//!   sessions in order on its own thread;
//! * [`publication`] publishes the podcasts the user chose to share to the
//!   public discovery overlay (`music_dht::podcast_dht`) and revokes them;
//! * [`discovery`] handles public cards found by search: their order, the
//!   "already subscribed" mark, an episode preview read from the feed and
//!   subscribing from a card.
//!
//! Audio is streamed unless the user explicitly downloads an episode.

pub mod discovery;
pub mod fetch;
pub mod offline;
pub mod playback;
pub mod progress;
pub mod publication;
pub mod refresh;
pub mod stream;

#[cfg(test)]
mod discovery_tests;
#[cfg(test)]
mod playback_tests;
#[cfg(test)]
mod publication_tests;
#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod test_server;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use furumi_library::podcasts::{
    LEGACY_PROTOTYPE_SCHEMA_VERSION, PODCAST_DB_FILE, PodcastListFilter, PodcastLocalOp,
    PodcastStore, PodcastStoreError, StampSource,
};

pub use fetch::Fetcher;
use refresh::Services;

/// Appends podcast operations to the device-sync log.
///
/// [`OpLog::record`] gives `action` a [`StampSource`] that reserves stamps
/// in the log. The action runs a `PodcastStore` local action and returns the
/// operations it emitted; they are appended with exactly those stamps.
pub trait OpLog: Send + Sync {
    fn record(
        &self,
        action: &mut dyn FnMut(&mut dyn StampSource) -> Result<Vec<PodcastLocalOp>>,
    ) -> Result<()>;
}

/// The podcast store file. It is the file the prototype podcast build
/// used, so an existing prototype database is migrated in place.
pub fn db_path() -> Result<PathBuf> {
    let dirs = crate::config::project_dirs()
        .ok_or_else(|| anyhow!("no home directory for podcast data"))?;
    Ok(dirs.data_dir().join(PODCAST_DB_FILE))
}

/// Replaceable cache of downloaded podcast covers.
pub fn covers_dir() -> Result<PathBuf> {
    let dirs = crate::config::project_dirs()
        .ok_or_else(|| anyhow!("no cache directory for podcast covers"))?;
    Ok(dirs.cache_dir().join("podcasts").join("covers"))
}

/// Opens the podcast store at `db_path` and builds the feed client.
///
/// `PodcastStore::open` migrates a prototype database in place and refuses,
/// without writing anything, a database from a newer Furumi or a file that
/// is not a podcast database. Blocking: call it on the blocking pool.
pub fn open_services(db_path: &Path, covers_dir: PathBuf, log: Arc<dyn OpLog>) -> Result<Services> {
    // The import commits inside `PodcastStore::open`; dropping its
    // validators happens afterwards. A marker written before the import and
    // removed only once every podcast was handled makes that step resumable
    // after a crash or a failed write in between.
    let marker = validators_marker(db_path);
    let mut pending = marker.is_file();
    if is_prototype_database(db_path) {
        if let Err(err) = std::fs::write(&marker, b"") {
            tracing::warn!("cannot record the pending prototype feed checks: {err}");
        }
        pending = true;
    }
    let store = PodcastStore::open(db_path)?;
    tracing::info!(path = %db_path.display(), "podcast store opened");
    if pending && forget_prototype_validators(&store) {
        match std::fs::remove_file(&marker) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!("cannot remove {}: {err}", marker.display()),
        }
    }
    match store.legacy_quarantine() {
        Ok(entries) if !entries.is_empty() => tracing::warn!(
            count = entries.len(),
            "some prototype podcast rows could not be imported; they are kept in the legacy_* tables"
        ),
        Ok(_) => {}
        Err(err) => tracing::debug!("reading the podcast import quarantine failed: {err:#}"),
    }
    let confirmations =
        publication::ShareConfirmations::open(publication::confirmations_path(db_path));
    Ok(Services {
        downloads_dir: db_path.with_file_name("podcast-downloads"),
        store: Arc::new(store),
        fetcher: Arc::new(Fetcher::new(covers_dir)?),
        log,
        confirmations: Arc::new(confirmations),
    })
}

/// Whether `path` is a database of the prototype podcast build, which
/// `PodcastStore::open` is about to import. Read-only; any doubt is "no".
fn is_prototype_database(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    rusqlite::Connection::open_with_flags(path, flags)
        .and_then(|conn| conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0)))
        .is_ok_and(|version| version == LEGACY_PROTOTYPE_SCHEMA_VERSION)
}

/// Marker next to the podcast store: the validators imported from a
/// prototype database still have to be dropped
/// ([`forget_prototype_validators`]).
fn validators_marker(db_path: &Path) -> PathBuf {
    let mut name = db_path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".full-check-pending");
    db_path.with_file_name(name)
}

/// Drops the HTTP validators imported from a prototype database. Returns
/// whether every podcast was handled (otherwise it runs again at the next
/// start).
///
/// The prototype stored no seasons, episode numbers or episode types, and
/// its GUID-less episodes stay provisional until a feed is parsed again. With
/// the imported `ETag` / `Last-Modified` the first check would be answered
/// "304 Not Modified" and the feed never parsed while it does not change, so
/// a finished series would never show its seasons. Without them the first
/// check downloads and applies the feed. The fetch state is local (never
/// synced).
///
/// Only the public fetch-state API exists for this: the validators are
/// cleared by recording a success at the last check, keeping the next check
/// time, and an imported error is recorded again as many times as it was
/// counted, so the back-off stays as imported. The one side effect is that a
/// podcast imported with an error gets a last success time (its last check),
/// which is not shown while the error is. Running this again only costs one
/// unconditional feed download per podcast. Contract gap: FRID's prototype
/// import should not carry the validators over.
fn forget_prototype_validators(store: &PodcastStore) -> bool {
    let result = (|| -> Result<usize> {
        let mut cleared = 0;
        for podcast in store.list_podcasts(PodcastListFilter::All)? {
            let id = &podcast.podcast_id;
            let state = store.fetch_state(id)?;
            if state.etag.is_none() && state.last_modified.is_none() {
                continue;
            }
            let checked = state
                .last_success_at_ms
                .or(state.checked_at_ms)
                .unwrap_or(0);
            store.record_fetch_success(id, checked, None, None, state.next_check_at_ms)?;
            if let Some(error) = &state.last_error {
                for _ in 0..state.consecutive_failures.max(1) {
                    store.record_fetch_failure(
                        id,
                        state.checked_at_ms.unwrap_or(checked),
                        error,
                        state.next_check_at_ms,
                    )?;
                }
            }
            cleared += 1;
        }
        Ok(cleared)
    })();
    match result {
        Ok(cleared) => {
            if cleared > 0 {
                tracing::info!(
                    cleared,
                    "imported podcasts get a full first feed check (prototype validators dropped)"
                );
            }
            true
        }
        Err(err) => {
            tracing::warn!(
                "dropping imported podcast validators failed (retried at the next start): {err:#}"
            );
            false
        }
    }
}

/// A readable reason why the store did not open; the typed store errors
/// already say that the file was left unchanged.
pub fn describe_open_error(err: &anyhow::Error) -> String {
    match err.downcast_ref::<PodcastStoreError>() {
        Some(store_error) => store_error.to_string(),
        None => format!("{err:#}"),
    }
}

/// Runs blocking store or filesystem work off the Tokio workers.
pub(crate) async fn blocking<T, F>(work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| anyhow!("background task failed: {err}"))?
}
