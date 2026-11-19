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
//!   appends the operations they return.
//!
//! Only RSS metadata is stored. Audio stays on the publisher's server.

pub mod fetch;
pub mod refresh;

#[cfg(test)]
mod test_server;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use furumi_library::podcasts::{
    PODCAST_DB_FILE, PodcastLocalOp, PodcastStore, PodcastStoreError, StampSource,
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
    let store = PodcastStore::open(db_path)?;
    tracing::info!(path = %db_path.display(), "podcast store opened");
    match store.legacy_quarantine() {
        Ok(entries) if !entries.is_empty() => tracing::warn!(
            count = entries.len(),
            "some prototype podcast rows could not be imported; they are kept in the legacy_* tables"
        ),
        Ok(_) => {}
        Err(err) => tracing::debug!("reading the podcast import quarantine failed: {err:#}"),
    }
    Ok(Services {
        store: Arc::new(store),
        fetcher: Arc::new(Fetcher::new(covers_dir)?),
        log,
    })
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
