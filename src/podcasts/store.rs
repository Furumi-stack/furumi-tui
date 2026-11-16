//! SQLite persistence for podcast subscriptions and episode metadata.
//!
//! Only metadata is stored; audio stays on the publisher's host. Episodes are
//! unique per `(podcast_id, guid)` and their row ids stay stable across
//! refreshes, so they can be referenced from playback state. Both tables use
//! `AUTOINCREMENT`, so ids of removed podcasts and episodes are never handed
//! out again (a stale reference or an in-flight refresh of a removed podcast
//! can never hit a different subscription).

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::feed::{ParsedEpisode, ParsedFeed};
use super::{Episode, Podcast, RefreshSummary};

/// Schema migrations; `PRAGMA user_version` records how many have run.
/// Every statement is idempotent so a partially applied step can be re-run.
const MIGRATIONS: &[&str] = &[
    // v1: subscriptions and episode metadata.
    "
    CREATE TABLE IF NOT EXISTS podcasts (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        feed_url        TEXT NOT NULL UNIQUE,
        title           TEXT NOT NULL,
        author          TEXT,
        description     TEXT,
        link            TEXT,
        image_url       TEXT,
        cover_url       TEXT,
        cover_path      TEXT,
        etag            TEXT,
        last_modified   TEXT,
        added_at        INTEGER NOT NULL,
        last_checked_at INTEGER,
        last_error      TEXT
    );
    CREATE TABLE IF NOT EXISTS podcast_episodes (
        id               INTEGER PRIMARY KEY AUTOINCREMENT,
        podcast_id       INTEGER NOT NULL REFERENCES podcasts(id) ON DELETE CASCADE,
        guid             TEXT NOT NULL,
        title            TEXT NOT NULL,
        description      TEXT,
        published_at     INTEGER,
        duration_seconds REAL,
        audio_url        TEXT NOT NULL,
        mime_type        TEXT,
        byte_len         INTEGER,
        link             TEXT,
        image_url        TEXT,
        first_seen_at    INTEGER NOT NULL,
        UNIQUE (podcast_id, guid)
    );
    CREATE INDEX IF NOT EXISTS podcast_episodes_by_date
        ON podcast_episodes (podcast_id, published_at DESC, id DESC);
    ",
];

const PODCAST_COLUMNS: &str = "
    p.id, p.feed_url, p.title, p.author, p.description, p.link, p.image_url,
    p.cover_path, p.last_checked_at, p.last_error,
    (SELECT COUNT(*) FROM podcast_episodes e WHERE e.podcast_id = p.id),
    (SELECT MAX(e.published_at) FROM podcast_episodes e WHERE e.podcast_id = p.id)";

const EPISODE_COLUMNS: &str = "
    id, podcast_id, guid, title, description, published_at, duration_seconds,
    audio_url, mime_type, byte_len, link, image_url";

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Opens (creating if needed) the podcast database at `path`.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening podcast database {}", path.display()))?;
        Self::init(conn, true)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?, false)
    }

    fn init(conn: Connection, wal: bool) -> Result<Store> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        if wal {
            // Returns the resulting mode as a row, hence query instead of execute.
            let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        }
        migrate(&conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn list_podcasts(&self) -> Result<Vec<Podcast>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {PODCAST_COLUMNS} FROM podcasts p ORDER BY p.title COLLATE NOCASE, p.id"
        ))?;
        let podcasts = stmt
            .query_map([], podcast_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(podcasts)
    }

    pub fn podcast(&self, id: i64) -> Result<Option<Podcast>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                &format!("SELECT {PODCAST_COLUMNS} FROM podcasts p WHERE p.id = ?1"),
                [id],
                podcast_from_row,
            )
            .optional()?)
    }

    pub fn podcast_by_feed_url(&self, feed_url: &str) -> Result<Option<Podcast>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                &format!("SELECT {PODCAST_COLUMNS} FROM podcasts p WHERE p.feed_url = ?1"),
                [feed_url],
                podcast_from_row,
            )
            .optional()?)
    }

    /// Inserts a subscription and all of its episodes in one transaction.
    pub fn add_podcast(&self, feed_url: &str, feed: &ParsedFeed, now: i64) -> Result<i64> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM podcasts WHERE feed_url = ?1)",
            [feed_url],
            |row| row.get(0),
        )?;
        if exists {
            bail!("already subscribed");
        }
        tx.execute(
            "INSERT INTO podcasts
                 (feed_url, title, author, description, link, image_url, added_at, last_checked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![
                feed_url,
                feed.title,
                feed.author,
                feed.description,
                feed.link,
                feed.image_url,
                now
            ],
        )
        .map_err(|err| match err {
            rusqlite::Error::SqliteFailure(code, _)
                if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                anyhow!("already subscribed")
            }
            other => other.into(),
        })?;
        let podcast_id = tx.last_insert_rowid();
        upsert_episodes(&tx, podcast_id, &feed.episodes, now)?;
        tx.commit()?;
        Ok(podcast_id)
    }

    /// Applies a freshly fetched feed: updates channel metadata and upserts
    /// episodes by guid. Episodes no longer in the feed are kept.
    pub fn apply_feed(
        &self,
        podcast_id: i64,
        feed: &ParsedFeed,
        now: i64,
    ) -> Result<RefreshSummary> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE podcasts
                SET title = ?2, author = ?3, description = ?4, link = ?5, image_url = ?6,
                    last_checked_at = ?7, last_error = NULL
              WHERE id = ?1",
            params![
                podcast_id,
                feed.title,
                feed.author,
                feed.description,
                feed.link,
                feed.image_url,
                now
            ],
        )?;
        if updated == 0 {
            bail!("podcast {podcast_id} not found");
        }
        let summary = upsert_episodes(&tx, podcast_id, &feed.episodes, now)?;
        tx.commit()?;
        Ok(summary)
    }

    pub fn record_check(&self, podcast_id: i64, now: i64, error: Option<&str>) -> Result<()> {
        self.conn().execute(
            "UPDATE podcasts SET last_checked_at = ?2, last_error = ?3 WHERE id = ?1",
            params![podcast_id, now, error],
        )?;
        Ok(())
    }

    pub fn http_validators(&self, podcast_id: i64) -> Result<(Option<String>, Option<String>)> {
        Ok(self
            .conn()
            .query_row(
                "SELECT etag, last_modified FROM podcasts WHERE id = ?1",
                [podcast_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, None)))
    }

    pub fn set_http_validators(
        &self,
        podcast_id: i64,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE podcasts SET etag = ?2, last_modified = ?3 WHERE id = ?1",
            params![podcast_id, etag, last_modified],
        )?;
        Ok(())
    }

    /// Records the cached cover file and the image URL it was downloaded from.
    pub fn set_cover(
        &self,
        podcast_id: i64,
        cover_url: Option<&str>,
        cover_path: Option<&str>,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE podcasts SET cover_url = ?2, cover_path = ?3 WHERE id = ?1",
            params![podcast_id, cover_url, cover_path],
        )?;
        Ok(())
    }

    /// `(cover_url, cover_path)`: the image URL the cached cover came from
    /// and the cached file.
    pub fn cover_source(&self, podcast_id: i64) -> Result<(Option<String>, Option<String>)> {
        Ok(self
            .conn()
            .query_row(
                "SELECT cover_url, cover_path FROM podcasts WHERE id = ?1",
                [podcast_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, None)))
    }

    /// Episodes of a podcast, newest first (undated episodes last).
    pub fn episodes(&self, podcast_id: i64) -> Result<Vec<Episode>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {EPISODE_COLUMNS} FROM podcast_episodes
              WHERE podcast_id = ?1
              ORDER BY published_at IS NULL, published_at DESC, id DESC"
        ))?;
        let episodes = stmt
            .query_map([podcast_id], episode_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(episodes)
    }

    #[cfg(test)]
    pub fn episode(&self, episode_id: i64) -> Result<Option<Episode>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {EPISODE_COLUMNS} FROM podcast_episodes WHERE id = ?1"),
                [episode_id],
                episode_from_row,
            )
            .optional()?)
    }

    /// Deletes a subscription and its episodes; returns the cached cover path
    /// so the caller can delete the file.
    pub fn remove_podcast(&self, podcast_id: i64) -> Result<Option<String>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let cover_path: Option<Option<String>> = tx
            .query_row(
                "SELECT cover_path FROM podcasts WHERE id = ?1",
                [podcast_id],
                |row| row.get(0),
            )
            .optional()?;
        // Explicit delete keeps this correct even on a connection where the
        // foreign-key cascade is somehow disabled.
        tx.execute(
            "DELETE FROM podcast_episodes WHERE podcast_id = ?1",
            [podcast_id],
        )?;
        tx.execute("DELETE FROM podcasts WHERE id = ?1", [podcast_id])?;
        tx.commit()?;
        Ok(cover_path.flatten())
    }

    #[cfg(test)]
    pub(super) fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn()
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }
}

fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let target = MIGRATIONS.len() as i64;
    if version > target {
        bail!("podcast database schema v{version} is newer than this Furumi (v{target})");
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let next = index as i64 + 1;
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE; {sql}; PRAGMA user_version = {next}; COMMIT;"
        ))
        .inspect_err(|_| {
            let _ = conn.execute_batch("ROLLBACK");
        })
        .with_context(|| format!("migrating podcast database to v{next}"))?;
    }
    Ok(())
}

fn podcast_from_row(row: &Row<'_>) -> rusqlite::Result<Podcast> {
    let episode_count: i64 = row.get(10)?;
    Ok(Podcast {
        id: row.get(0)?,
        feed_url: row.get(1)?,
        title: row.get(2)?,
        author: row.get(3)?,
        description: row.get(4)?,
        link: row.get(5)?,
        image_url: row.get(6)?,
        cover_path: row.get(7)?,
        last_checked_at: row.get(8)?,
        last_error: row.get(9)?,
        episode_count: usize::try_from(episode_count).unwrap_or(0),
        latest_published_at: row.get(11)?,
    })
}

fn episode_from_row(row: &Row<'_>) -> rusqlite::Result<Episode> {
    let byte_len: Option<i64> = row.get(9)?;
    Ok(Episode {
        id: row.get(0)?,
        podcast_id: row.get(1)?,
        guid: row.get(2)?,
        title: row.get(3)?,
        description: row.get(4)?,
        published_at: row.get(5)?,
        duration_seconds: row.get(6)?,
        audio_url: row.get(7)?,
        mime_type: row.get(8)?,
        byte_len: byte_len.and_then(|len| u64::try_from(len).ok()),
        link: row.get(10)?,
        image_url: row.get(11)?,
    })
}

/// The mutable columns of an episode row, for change detection.
#[derive(PartialEq)]
struct EpisodeFields {
    title: String,
    description: Option<String>,
    published_at: Option<i64>,
    duration_seconds: Option<f64>,
    audio_url: String,
    mime_type: Option<String>,
    byte_len: Option<i64>,
    link: Option<String>,
    image_url: Option<String>,
}

impl EpisodeFields {
    fn from_parsed(episode: &ParsedEpisode) -> EpisodeFields {
        EpisodeFields {
            title: episode.title.clone(),
            description: episode.description.clone(),
            published_at: episode.published_at,
            duration_seconds: episode.duration_seconds,
            audio_url: episode.audio_url.clone(),
            mime_type: episode.mime_type.clone(),
            byte_len: episode
                .byte_len
                .map(|len| i64::try_from(len).unwrap_or(i64::MAX)),
            link: episode.link.clone(),
            image_url: episode.image_url.clone(),
        }
    }
}

fn upsert_episodes(
    tx: &rusqlite::Transaction<'_>,
    podcast_id: i64,
    episodes: &[ParsedEpisode],
    now: i64,
) -> Result<RefreshSummary> {
    let mut summary = RefreshSummary::default();
    let mut select = tx.prepare_cached(
        "SELECT id, title, description, published_at, duration_seconds, audio_url,
                mime_type, byte_len, link, image_url
           FROM podcast_episodes WHERE podcast_id = ?1 AND guid = ?2",
    )?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO podcast_episodes
             (podcast_id, guid, title, description, published_at, duration_seconds,
              audio_url, mime_type, byte_len, link, image_url, first_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;
    let mut update = tx.prepare_cached(
        "UPDATE podcast_episodes
            SET title = ?2, description = ?3, published_at = ?4, duration_seconds = ?5,
                audio_url = ?6, mime_type = ?7, byte_len = ?8, link = ?9, image_url = ?10
          WHERE id = ?1",
    )?;
    for episode in episodes {
        let fields = EpisodeFields::from_parsed(episode);
        let existing = select
            .query_row(params![podcast_id, episode.guid], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    EpisodeFields {
                        title: row.get(1)?,
                        description: row.get(2)?,
                        published_at: row.get(3)?,
                        duration_seconds: row.get(4)?,
                        audio_url: row.get(5)?,
                        mime_type: row.get(6)?,
                        byte_len: row.get(7)?,
                        link: row.get(8)?,
                        image_url: row.get(9)?,
                    },
                ))
            })
            .optional()?;
        match existing {
            None => {
                insert.execute(params![
                    podcast_id,
                    episode.guid,
                    fields.title,
                    fields.description,
                    fields.published_at,
                    fields.duration_seconds,
                    fields.audio_url,
                    fields.mime_type,
                    fields.byte_len,
                    fields.link,
                    fields.image_url,
                    now
                ])?;
                summary.new_episodes += 1;
            }
            Some((id, stored)) if stored != fields => {
                update.execute(params![
                    id,
                    fields.title,
                    fields.description,
                    fields.published_at,
                    fields.duration_seconds,
                    fields.audio_url,
                    fields.mime_type,
                    fields.byte_len,
                    fields.link,
                    fields.image_url
                ])?;
                summary.updated_episodes += 1;
            }
            Some(_) => {}
        }
    }
    Ok(summary)
}
