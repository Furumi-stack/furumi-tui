//! The local music library: a SQLite database holding artists, releases,
//! tracks (with paths to local audio files), playlists, likes and play
//! history. This module is the only place that talks SQL; it returns the
//! same data shapes the furumusic API used to serve, so the views did not
//! have to change their model of the world.
//!
//! All methods take `&self` and lock the connection internally, so an
//! `Arc<Library>` can be shared across blocking tasks.

pub mod import;
pub mod models;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OptionalExtension as _, params};

use models::{
    ArtistCard, ArtistDetail, ArtistRef, ArtistsPage, Availability, PlaylistCard, PlaylistDetail,
    ReleaseCard, ReleaseDetail, ReleaseEdit, SearchResults, TrackEdit, TrackItem,
};

/// The virtual "Liked tracks" playlist id, kept from the server API.
pub const LIKES_PLAYLIST_ID: i64 = -1;
const NETWORK_ARTIST_CACHE_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS artists (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE COLLATE NOCASE,
    image_path  TEXT
);
CREATE TABLE IF NOT EXISTS releases (
    id            INTEGER PRIMARY KEY,
    title         TEXT NOT NULL,
    release_type  TEXT NOT NULL DEFAULT 'album',
    year          INTEGER,
    cover_path    TEXT
);
CREATE TABLE IF NOT EXISTS release_artists (
    release_id  INTEGER NOT NULL REFERENCES releases(id) ON DELETE CASCADE,
    artist_id   INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (release_id, artist_id)
);
CREATE TABLE IF NOT EXISTS tracks (
    id                 INTEGER PRIMARY KEY,
    title              TEXT NOT NULL,
    track_number       INTEGER,
    disc_number        INTEGER,
    duration_seconds   REAL NOT NULL DEFAULT 0,
    release_id         INTEGER NOT NULL REFERENCES releases(id) ON DELETE CASCADE,
    file_path          TEXT NOT NULL UNIQUE,
    content_id         TEXT,
    audio_format       TEXT,
    audio_bitrate      INTEGER,
    audio_sample_rate  INTEGER,
    audio_bit_depth    INTEGER,
    file_size_bytes    INTEGER,
    created_at         TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS track_artists (
    track_id   INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    artist_id  INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
    role       TEXT NOT NULL DEFAULT 'main',
    position   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, artist_id, role)
);
CREATE TABLE IF NOT EXISTS playlists (
    id           INTEGER PRIMARY KEY,
    sync_id      TEXT UNIQUE,
    title        TEXT NOT NULL,
    description  TEXT,
    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS playlist_tracks (
    playlist_id  INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    track_id     INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    position     INTEGER NOT NULL,
    PRIMARY KEY (playlist_id, track_id)
);
CREATE TABLE IF NOT EXISTS likes (
    track_id  INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    liked_at  TEXT NOT NULL DEFAULT (datetime('now')),
    liked_hlc_ms INTEGER
);
CREATE TABLE IF NOT EXISTS fed_likes (
    item_id          TEXT PRIMARY KEY,
    owner            TEXT NOT NULL,
    title            TEXT NOT NULL,
    artist_names     TEXT NOT NULL DEFAULT '',
    featured_artist_names TEXT NOT NULL DEFAULT '',
    year             INTEGER,
    duration_seconds REAL,
    content_id       TEXT,
    release_title    TEXT,
    track_number     INTEGER,
    disc_number      INTEGER,
    liked_at         TEXT NOT NULL DEFAULT (datetime('now')),
    liked_hlc_ms     INTEGER
);
CREATE TABLE IF NOT EXISTS fed_playlist_tracks (
    playlist_sync_id TEXT NOT NULL,
    item_id          TEXT NOT NULL,
    owner            TEXT NOT NULL,
    title            TEXT NOT NULL,
    artist_names     TEXT NOT NULL DEFAULT '',
    featured_artist_names TEXT NOT NULL DEFAULT '',
    year             INTEGER,
    duration_seconds REAL,
    content_id       TEXT NOT NULL,
    release_title    TEXT,
    track_number     INTEGER,
    disc_number      INTEGER,
    position         INTEGER NOT NULL DEFAULT 0,
    added_at         TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (playlist_sync_id, content_id)
);
CREATE TABLE IF NOT EXISTS history (
    id                INTEGER PRIMARY KEY,
    track_id          INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    started_at        INTEGER,
    listened_seconds  INTEGER NOT NULL DEFAULT 0,
    completed         INTEGER NOT NULL DEFAULT 0,
    played_at         TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_tracks_release ON tracks(release_id);
CREATE INDEX IF NOT EXISTS idx_track_artists_artist ON track_artists(artist_id);
CREATE INDEX IF NOT EXISTS idx_release_artists_artist ON release_artists(artist_id);
CREATE INDEX IF NOT EXISTS idx_history_track ON history(track_id);
CREATE INDEX IF NOT EXISTS idx_playlist_tracks_playlist ON playlist_tracks(playlist_id);
CREATE INDEX IF NOT EXISTS idx_fed_playlist_tracks_playlist
    ON fed_playlist_tracks(playlist_sync_id, position);
CREATE TABLE IF NOT EXISTS network_artist_cache (
    source_id      TEXT NOT NULL,
    source_kind    TEXT NOT NULL,
    artist_key     TEXT NOT NULL,
    name           TEXT NOT NULL,
    image_path     TEXT,
    remote_image_hint TEXT,
    release_count  INTEGER NOT NULL DEFAULT 0,
    track_count    INTEGER NOT NULL DEFAULT 0,
    seen_at_ms     INTEGER NOT NULL,
    PRIMARY KEY (source_id, artist_key)
);
CREATE INDEX IF NOT EXISTS idx_network_artist_cache_kind
    ON network_artist_cache(source_kind, seen_at_ms);
CREATE INDEX IF NOT EXISTS idx_network_artist_cache_artist
    ON network_artist_cache(artist_key);
";

/// The SELECT column list every TrackItem row is built from; artist lists
/// are attached in a second pass.
const TRACK_COLUMNS: &str = "
    t.id, t.title, t.track_number, t.disc_number, t.duration_seconds,
    t.release_id, r.title, r.year, r.cover_path,
    t.file_path, t.audio_format, t.audio_bitrate, t.audio_sample_rate,
    t.audio_bit_depth, t.file_size_bytes,
    t.content_id,
    (SELECT COUNT(*) FROM history h WHERE h.track_id = t.id AND h.completed = 1)
";

/// Plain rows handed to the federation for publishing (see
/// `federation_export`).
#[derive(Debug)]
pub struct FederationExport {
    pub artists: Vec<(i64, String)>,
    pub releases: Vec<ExportRelease>,
    pub tracks: Vec<ExportTrack>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentIdBackfillStats {
    pub checked: usize,
    pub normalized: usize,
    pub hashed: usize,
    pub failed: usize,
}

impl ContentIdBackfillStats {
    pub fn updated(&self) -> usize {
        self.normalized + self.hashed
    }
}

#[derive(Debug)]
pub struct ExportRelease {
    pub id: i64,
    pub title: String,
    pub year: Option<i32>,
    pub release_type: String,
    pub artist_names: Vec<String>,
}

#[derive(Debug)]
pub struct ExportTrack {
    pub id: i64,
    pub title: String,
    pub year: Option<i32>,
    pub duration_seconds: f64,
    pub artist_names: Vec<String>,
    pub featured_artist_names: Vec<String>,
    pub release_title: String,
    pub release_type: String,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub content_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NetworkArtistPreview {
    pub artist_key: String,
    pub name: String,
    pub image_path: Option<String>,
    pub release_count: i64,
    pub track_count: i64,
}

#[derive(Debug, Clone)]
pub struct NetworkArtistImageRequest {
    pub source_id: String,
    pub artist_key: String,
    pub name: String,
}

pub struct Library {
    conn: Mutex<Connection>,
    db_path: PathBuf,
    /// Directory where extracted embedded covers are stored.
    covers_dir: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalLibraryStats {
    pub artist_count: i64,
    pub release_count: i64,
    pub track_count: i64,
    pub audio_bytes: u64,
    pub tracks_without_size: i64,
    pub cover_bytes: u64,
    pub database_bytes: u64,
}

impl LocalLibraryStats {
    pub fn total_bytes(&self) -> u64 {
        self.audio_bytes
            .saturating_add(self.cover_bytes)
            .saturating_add(self.database_bytes)
    }
}

/// Default database location: `<data dir>/furumi/library.db`.
pub fn default_db_path() -> Result<PathBuf> {
    let dirs = crate::config::project_dirs().context("cannot determine the data directory")?;
    Ok(dirs.data_dir().join("library.db"))
}

impl Library {
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("opening database {}", db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        register_norm_function(&conn)?;
        conn.execute_batch(SCHEMA).context("applying schema")?;
        ensure_schema_migrations(&conn)?;
        let covers_dir = db_path
            .parent()
            .map(|dir| dir.join("covers"))
            .unwrap_or_else(|| PathBuf::from("covers"));
        Ok(Self {
            conn: Mutex::new(conn),
            db_path: db_path.to_path_buf(),
            covers_dir,
        })
    }

    pub fn covers_dir(&self) -> &Path {
        &self.covers_dir
    }

    pub fn local_stats(&self) -> Result<LocalLibraryStats> {
        let (artist_count, release_count, track_count, audio_bytes, tracks_without_size) = {
            let conn = self.lock();
            conn.query_row(
                "SELECT
                    (SELECT COUNT(*) FROM artists),
                    (SELECT COUNT(*) FROM releases),
                    (SELECT COUNT(*) FROM tracks),
                    (SELECT COALESCE(SUM(CASE
                        WHEN file_size_bytes IS NOT NULL AND file_size_bytes >= 0
                        THEN file_size_bytes ELSE 0 END), 0) FROM tracks),
                    (SELECT COUNT(*) FROM tracks WHERE file_size_bytes IS NULL)
                ",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )?
        };
        Ok(LocalLibraryStats {
            artist_count,
            release_count,
            track_count,
            audio_bytes: audio_bytes.max(0) as u64,
            tracks_without_size,
            cover_bytes: directory_size(&self.covers_dir),
            database_bytes: sqlite_database_size(&self.db_path),
        })
    }

    /// Make `content_id` a local-library invariant.
    ///
    /// Old databases can have NULL/invalid ids because the column was added
    /// after import already existed. This scans rows cheaply, hashes only the
    /// tracks that actually need an id, and never holds the SQLite lock while
    /// reading audio files from disk.
    pub fn backfill_missing_content_ids(&self) -> Result<ContentIdBackfillStats> {
        let rows = {
            let conn = self.lock();
            let mut statement =
                conn.prepare("SELECT id, content_id, file_path FROM tracks ORDER BY id")?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut stats = ContentIdBackfillStats {
            checked: rows.len(),
            ..ContentIdBackfillStats::default()
        };
        let mut updates = Vec::new();
        for (track_id, raw_content_id, file_path) in rows {
            if let Some(normalized) = raw_content_id
                .as_deref()
                .and_then(music_dht::normalize_content_id)
            {
                if raw_content_id.as_deref() != Some(normalized.as_str()) {
                    updates.push((track_id, normalized));
                    stats.normalized += 1;
                }
            } else if let Some(content_id) = audio_content_id(&file_path) {
                updates.push((track_id, content_id));
                stats.hashed += 1;
            } else {
                stats.failed += 1;
                tracing::warn!(
                    track_id,
                    path = %file_path,
                    "content id backfill skipped an unreadable track"
                );
            }

            if updates.len() >= 64 {
                self.write_content_id_updates(&mut updates)?;
            }
        }
        self.write_content_id_updates(&mut updates)?;
        Ok(stats)
    }

    fn write_content_id_updates(&self, updates: &mut Vec<(i64, String)>) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        for (track_id, content_id) in updates.drain(..) {
            tx.execute(
                "UPDATE tracks SET content_id = ?2 WHERE id = ?1",
                params![track_id, content_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| {
            // A panic mid-query leaves the connection usable; keep going.
            poisoned.into_inner()
        })
    }

    // -----------------------------------------------------------------
    // Reads (same shapes the API used to return)
    // -----------------------------------------------------------------

    pub fn artists(
        &self,
        page: i64,
        limit: i64,
        filters: crate::config::settings::LibraryFilters,
    ) -> Result<ArtistsPage> {
        if !filters.source_mode.includes_network() {
            return self.local_artists(page, limit, filters.hide_featured_only);
        }
        self.merged_artists(page, limit, filters)
    }

    fn local_artists(
        &self,
        page: i64,
        limit: i64,
        hide_featured_only: bool,
    ) -> Result<ArtistsPage> {
        let conn = self.lock();
        let hide_featured_only = i64::from(hide_featured_only);
        let total: i64 = conn.query_row(
            "SELECT COUNT(*)
             FROM artists a
             WHERE ?1 = 0
                OR EXISTS (
                    SELECT 1
                    FROM release_artists ra
                    WHERE ra.artist_id = a.id
                )",
            [hide_featured_only],
            |row| row.get(0),
        )?;
        let offset = (page.max(1) - 1) * limit;
        let mut statement = conn.prepare(
            "SELECT a.id, a.name, a.image_path,
                (SELECT COUNT(DISTINCT ra.release_id)
                 FROM release_artists ra
                 WHERE ra.artist_id = a.id) AS release_count,
                (SELECT COUNT(DISTINCT ta.track_id)
                 FROM track_artists ta
                 WHERE ta.artist_id = a.id) AS track_count
             FROM artists a
             WHERE ?1 = 0
                OR EXISTS (
                    SELECT 1
                    FROM release_artists ra
                    WHERE ra.artist_id = a.id
                )
             ORDER BY release_count DESC, track_count DESC, a.name COLLATE NOCASE
             LIMIT ?2 OFFSET ?3",
        )?;
        let items = statement
            .query_map(params![hide_featured_only, limit, offset], |row| {
                Ok(ArtistCard {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    image_path: row.get(2)?,
                    release_count: row.get(3)?,
                    track_count: row.get(4)?,
                    availability: Availability::Local,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = offset + (items.len() as i64) < total;
        Ok(ArtistsPage {
            has_more,
            items,
            total,
            page: page.max(1),
        })
    }

    fn local_artist_cards(&self, hide_featured_only: bool) -> Result<Vec<ArtistCard>> {
        let conn = self.lock();
        let hide_featured_only = i64::from(hide_featured_only);
        let mut statement = conn.prepare(
            "SELECT a.id, a.name, a.image_path,
                (SELECT COUNT(DISTINCT ra.release_id)
                 FROM release_artists ra
                 WHERE ra.artist_id = a.id) AS release_count,
                (SELECT COUNT(DISTINCT ta.track_id)
                 FROM track_artists ta
                 WHERE ta.artist_id = a.id) AS track_count
             FROM artists a
             WHERE ?1 = 0
                OR EXISTS (
                    SELECT 1
                    FROM release_artists ra
                    WHERE ra.artist_id = a.id
                )",
        )?;
        statement
            .query_map(params![hide_featured_only], |row| {
                Ok(ArtistCard {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    image_path: row.get(2)?,
                    release_count: row.get(3)?,
                    track_count: row.get(4)?,
                    availability: Availability::Local,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    fn merged_artists(
        &self,
        page: i64,
        limit: i64,
        filters: crate::config::settings::LibraryFilters,
    ) -> Result<ArtistsPage> {
        let mut by_key: HashMap<String, ArtistCard> = HashMap::new();
        for artist in self.local_artist_cards(filters.hide_featured_only)? {
            let key = music_dht::normalize_name(&artist.name);
            if !key.is_empty() {
                by_key.insert(key, artist);
            }
        }

        let conn = self.lock();
        let cutoff = now_ms_i64().saturating_sub(NETWORK_ARTIST_CACHE_TTL_MS);
        let source_predicate = if filters.source_mode.includes_global_peers() {
            "seen_at_ms >= ?1"
        } else {
            "seen_at_ms >= ?1 AND source_kind = 'personal'"
        };
        let release_predicate = if filters.hide_featured_only {
            " AND release_count > 0"
        } else {
            ""
        };
        let sql = format!(
            "SELECT artist_key,
                    COALESCE(NULLIF(MIN(name), ''), artist_key) AS name,
                    MAX(image_path) AS image_path,
                    MAX(release_count) AS release_count,
                    MAX(track_count) AS track_count
             FROM network_artist_cache
             WHERE {source_predicate}{release_predicate}
             GROUP BY artist_key"
        );
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map([cutoff], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (artist_key, name, image_path, release_count, track_count) = row?;
            if artist_key.trim().is_empty() || track_count <= 0 {
                continue;
            }
            match by_key.get_mut(&artist_key) {
                Some(local) => {
                    if release_count > local.release_count || track_count > local.track_count {
                        local.availability = Availability::Mixed;
                    }
                    local.release_count = local.release_count.max(release_count);
                    local.track_count = local.track_count.max(track_count);
                    if local.image_path.is_none() {
                        local.image_path = image_path;
                    }
                }
                None => {
                    by_key.insert(
                        artist_key.clone(),
                        ArtistCard {
                            id: remote_artist_id(&artist_key),
                            name,
                            image_path,
                            release_count,
                            track_count,
                            availability: Availability::Remote,
                        },
                    );
                }
            }
        }

        let mut items: Vec<ArtistCard> = by_key.into_values().collect();
        items.sort_by(|left, right| {
            right
                .release_count
                .cmp(&left.release_count)
                .then_with(|| right.track_count.cmp(&left.track_count))
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        let total = items.len() as i64;
        let offset = (page.max(1) - 1) * limit;
        let page_items = items
            .into_iter()
            .skip(offset.max(0) as usize)
            .take(limit.max(0) as usize)
            .collect::<Vec<_>>();
        let has_more = offset + (page_items.len() as i64) < total;
        Ok(ArtistsPage {
            items: page_items,
            total,
            page: page.max(1),
            has_more,
        })
    }

    pub fn artist_preview_slice(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<NetworkArtistPreview>, Option<String>)> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT a.name, a.image_path,
                (SELECT COUNT(DISTINCT ra.release_id)
                 FROM release_artists ra
                 WHERE ra.artist_id = a.id) AS release_count,
                (SELECT COUNT(DISTINCT ta.track_id)
                 FROM track_artists ta
                 WHERE ta.artist_id = a.id) AS track_count
             FROM artists a
             WHERE EXISTS (
                 SELECT 1 FROM track_artists ta WHERE ta.artist_id = a.id
             )
             ORDER BY release_count DESC, track_count DESC, a.name COLLATE NOCASE
             LIMIT ?1 OFFSET ?2",
        )?;
        let items = statement
            .query_map(params![limit as i64 + 1, offset as i64], |row| {
                let name: String = row.get(0)?;
                Ok(NetworkArtistPreview {
                    artist_key: music_dht::normalize_name(&name),
                    name,
                    image_path: row.get(1)?,
                    release_count: row.get(2)?,
                    track_count: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = items.len() > limit;
        let mut items: Vec<_> = items
            .into_iter()
            .take(limit)
            .filter(|artist| !artist.artist_key.is_empty() && artist.track_count > 0)
            .collect();
        let next = has_more.then(|| (offset + items.len()).to_string());
        Ok((std::mem::take(&mut items), next))
    }

    pub fn replace_network_artist_cache(
        &self,
        source_id: &str,
        source_kind: &str,
        artists: &[NetworkArtistPreview],
        _replace_source: bool,
    ) -> Result<usize> {
        let source_id = source_id.trim();
        let source_kind = source_kind.trim();
        if source_id.is_empty() || source_kind.is_empty() {
            return Ok(0);
        }
        let now = now_ms_i64();
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut inserted = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO network_artist_cache
                    (source_id, source_kind, artist_key, name, image_path, remote_image_hint,
                     release_count, track_count, seen_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(source_id, artist_key) DO UPDATE SET
                    source_kind = excluded.source_kind,
                    name = excluded.name,
                    image_path = COALESCE(network_artist_cache.image_path, excluded.image_path),
                    remote_image_hint = excluded.remote_image_hint,
                    release_count = excluded.release_count,
                    track_count = excluded.track_count,
                    seen_at_ms = excluded.seen_at_ms",
            )?;
            for artist in artists {
                let artist_key = if artist.artist_key.trim().is_empty() {
                    music_dht::normalize_name(&artist.name)
                } else {
                    artist.artist_key.clone()
                };
                if artist_key.is_empty() || artist.track_count <= 0 {
                    continue;
                }
                stmt.execute(params![
                    source_id,
                    source_kind,
                    artist_key,
                    artist.name.trim(),
                    Option::<&str>::None,
                    artist.image_path.as_deref(),
                    artist.release_count.max(0),
                    artist.track_count.max(0),
                    now,
                ])?;
                inserted += 1;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    pub fn network_artist_image_requests(
        &self,
        filters: crate::config::settings::LibraryFilters,
        artist_names: &[String],
        limit: usize,
    ) -> Result<Vec<NetworkArtistImageRequest>> {
        if !filters.source_mode.includes_network() || artist_names.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let cutoff = now_ms_i64().saturating_sub(NETWORK_ARTIST_CACHE_TTL_MS);
        let source_predicate = if filters.source_mode.includes_global_peers() {
            "seen_at_ms >= ?2"
        } else {
            "seen_at_ms >= ?2 AND source_kind = 'personal'"
        };
        let sql = format!(
            "SELECT source_id, artist_key, name
             FROM network_artist_cache
             WHERE artist_key = ?1
               AND {source_predicate}
               AND image_path IS NULL
               AND remote_image_hint IS NOT NULL
               AND remote_image_hint <> ''
             ORDER BY CASE source_kind WHEN 'personal' THEN 0 ELSE 1 END,
                      seen_at_ms DESC
             LIMIT 1"
        );
        let mut statement = conn.prepare(&sql)?;
        let mut seen_keys = HashSet::new();
        let mut requests = Vec::new();
        for name in artist_names {
            let artist_key = music_dht::normalize_name(name);
            if artist_key.is_empty() || !seen_keys.insert(artist_key.clone()) {
                continue;
            }
            let row = statement
                .query_row(params![artist_key, cutoff], |row| {
                    Ok(NetworkArtistImageRequest {
                        source_id: row.get(0)?,
                        artist_key: row.get(1)?,
                        name: row.get(2)?,
                    })
                })
                .optional()?;
            if let Some(request) = row {
                requests.push(request);
                if requests.len() >= limit {
                    break;
                }
            }
        }
        Ok(requests)
    }

    pub fn set_network_artist_image(
        &self,
        source_id: &str,
        artist_key: &str,
        image_path: &str,
    ) -> Result<bool> {
        let changed = self.lock().execute(
            "UPDATE network_artist_cache
             SET image_path = ?3
             WHERE source_id = ?1 AND artist_key = ?2",
            params![source_id, artist_key, image_path],
        )?;
        Ok(changed > 0)
    }

    pub fn artist(&self, id: i64) -> Result<ArtistDetail> {
        let conn = self.lock();
        let (name, image_path): (String, Option<String>) = conn
            .query_row(
                "SELECT name, image_path FROM artists WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .context("artist not found")?;
        let total_track_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM track_artists WHERE artist_id = ?1 AND role = 'main'",
            [id],
            |row| row.get(0),
        )?;
        let total_play_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM history h
             WHERE h.completed = 1 AND h.track_id IN
                (SELECT track_id FROM track_artists WHERE artist_id = ?1)",
            [id],
            |row| row.get(0),
        )?;
        let top_tracks = query_tracks(
            &conn,
            &format!(
                "SELECT {TRACK_COLUMNS} FROM tracks t
                 JOIN releases r ON r.id = t.release_id
                 JOIN track_artists ta ON ta.track_id = t.id
                 WHERE ta.artist_id = ?1 AND ta.role = 'main'
                 ORDER BY 17 DESC, t.title COLLATE NOCASE
                 LIMIT 10"
            ),
            params![id],
        )?;
        let featured_tracks = query_tracks(
            &conn,
            &format!(
                "SELECT {TRACK_COLUMNS} FROM tracks t
                 JOIN releases r ON r.id = t.release_id
                 JOIN track_artists ta ON ta.track_id = t.id
                 WHERE ta.artist_id = ?1 AND ta.role = 'featured'
                 ORDER BY t.title COLLATE NOCASE"
            ),
            params![id],
        )?;
        let mut statement = conn.prepare(
            "SELECT r.id, r.title, r.release_type, r.year, r.cover_path,
                (SELECT COUNT(*) FROM tracks t WHERE t.release_id = r.id)
             FROM releases r
             JOIN release_artists ra ON ra.release_id = r.id
             WHERE ra.artist_id = ?1
             ORDER BY r.year IS NULL, r.year DESC, r.title COLLATE NOCASE",
        )?;
        let releases = statement
            .query_map([id], release_card_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ArtistDetail {
            id,
            name,
            image_path,
            total_track_count,
            total_play_count,
            top_tracks,
            releases,
            featured_tracks,
        })
    }

    pub fn release(&self, id: i64) -> Result<ReleaseDetail> {
        let conn = self.lock();
        let (title, release_type, year, cover_path) = conn
            .query_row(
                "SELECT title, release_type, year, cover_path FROM releases WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i32>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .context("release not found")?;
        let mut statement = conn.prepare(
            "SELECT a.id, a.name FROM artists a
             JOIN release_artists ra ON ra.artist_id = a.id
             WHERE ra.release_id = ?1 ORDER BY ra.position",
        )?;
        let artists = statement
            .query_map([id], |row| {
                Ok(ArtistRef {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let tracks = query_tracks(
            &conn,
            &format!(
                "SELECT {TRACK_COLUMNS} FROM tracks t
                 JOIN releases r ON r.id = t.release_id
                 WHERE t.release_id = ?1
                 ORDER BY t.disc_number IS NULL, t.disc_number,
                          t.track_number IS NULL, t.track_number,
                          t.title COLLATE NOCASE"
            ),
            params![id],
        )?;
        Ok(ReleaseDetail {
            id,
            title,
            release_type,
            year,
            cover_path,
            artists,
            tracks,
        })
    }

    pub fn search(&self, query: &str, limit: i64) -> Result<SearchResults> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(SearchResults::default());
        }
        // instr over norm() folds case for every script, unlike LIKE/NOCASE
        // which only handle ASCII.
        let pattern = music_dht::normalize_name(query);
        if pattern.is_empty() {
            // Punctuation-only queries (e.g. "%") normalize to nothing.
            return Ok(SearchResults::default());
        }
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT a.id, a.name, a.image_path,
                (SELECT COUNT(DISTINCT ra.release_id)
                 FROM release_artists ra
                 WHERE ra.artist_id = a.id),
                (SELECT COUNT(DISTINCT ta.track_id)
                 FROM track_artists ta
                 WHERE ta.artist_id = a.id)
             FROM artists a WHERE instr(norm(a.name), ?1) > 0
             ORDER BY a.name COLLATE NOCASE LIMIT ?2",
        )?;
        let artists = statement
            .query_map(params![pattern, limit], |row| {
                Ok(ArtistCard {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    image_path: row.get(2)?,
                    release_count: row.get(3)?,
                    track_count: row.get(4)?,
                    availability: Availability::Local,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut statement = conn.prepare(
            "SELECT r.id, r.title, r.release_type, r.year, r.cover_path,
                (SELECT COUNT(*) FROM tracks t WHERE t.release_id = r.id)
             FROM releases r WHERE instr(norm(r.title), ?1) > 0
             ORDER BY r.title COLLATE NOCASE LIMIT ?2",
        )?;
        let releases = statement
            .query_map(params![pattern, limit], release_card_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let tracks = query_tracks(
            &conn,
            &format!(
                "SELECT {TRACK_COLUMNS} FROM tracks t
                 JOIN releases r ON r.id = t.release_id
                 WHERE instr(norm(t.title), ?1) > 0
                 ORDER BY t.title COLLATE NOCASE LIMIT ?2"
            ),
            params![pattern, limit],
        )?;
        let mut results = SearchResults {
            artists,
            releases,
            tracks,
        };
        rank_search_results(&mut results, &pattern);
        Ok(results)
    }

    /// Everything the federation publishes into the DHT: plain rows, so the
    /// federation module needs no SQL of its own.
    pub fn federation_export(&self) -> Result<FederationExport> {
        let conn = self.lock();
        let mut statement = conn.prepare("SELECT id, name FROM artists")?;
        let artists = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<(i64, String)>>>()?;

        let mut statement = conn.prepare(
            "SELECT ra.release_id, a.name FROM release_artists ra
             JOIN artists a ON a.id = ra.artist_id
             ORDER BY ra.release_id, ra.position",
        )?;
        let mut release_artists: std::collections::HashMap<i64, Vec<String>> = Default::default();
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })? {
            let (id, name) = row?;
            release_artists.entry(id).or_default().push(name);
        }
        let mut statement = conn.prepare("SELECT id, title, year, release_type FROM releases")?;
        let releases = statement
            .query_map([], |row| {
                Ok(ExportRelease {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    year: row.get(2)?,
                    release_type: row.get(3)?,
                    artist_names: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|mut release| {
                release.artist_names = release_artists.remove(&release.id).unwrap_or_default();
                release
            })
            .collect();

        let mut statement = conn.prepare(
            "SELECT ta.track_id, a.name, ta.role FROM track_artists ta
             JOIN artists a ON a.id = ta.artist_id
             WHERE ta.role IN ('main', 'featured')
             ORDER BY ta.track_id,
                      CASE ta.role WHEN 'main' THEN 0 ELSE 1 END,
                      ta.position",
        )?;
        let mut track_artists: std::collections::HashMap<i64, Vec<String>> = Default::default();
        let mut featured_artists: std::collections::HashMap<i64, Vec<String>> = Default::default();
        for row in statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })? {
            let (id, name, role) = row?;
            if role == "featured" {
                featured_artists.entry(id).or_default().push(name);
            } else {
                track_artists.entry(id).or_default().push(name);
            }
        }
        let mut statement = conn.prepare(
            "SELECT t.id, t.title, r.year, t.duration_seconds,
                    r.title, r.release_type, t.track_number, t.disc_number,
                    t.content_id, t.file_path
             FROM tracks t JOIN releases r ON r.id = t.release_id",
        )?;
        let track_rows = statement
            .query_map([], |row| {
                Ok((
                    ExportTrack {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        year: row.get(2)?,
                        duration_seconds: row.get(3)?,
                        artist_names: Vec::new(),
                        featured_artist_names: Vec::new(),
                        release_title: row.get(4)?,
                        release_type: row.get(5)?,
                        track_number: row.get(6)?,
                        disc_number: row.get(7)?,
                        content_id: None,
                    },
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut tracks = Vec::with_capacity(track_rows.len());
        for (mut track, raw_content_id, file_path) in track_rows {
            let mut content_id = raw_content_id
                .as_deref()
                .and_then(music_dht::normalize_content_id);
            if content_id.is_none() {
                content_id = audio_content_id(&file_path);
            }
            if content_id != raw_content_id {
                conn.execute(
                    "UPDATE tracks SET content_id = ?2 WHERE id = ?1",
                    params![track.id, content_id.as_deref()],
                )?;
            }
            track.artist_names = track_artists.remove(&track.id).unwrap_or_default();
            track.featured_artist_names = featured_artists.remove(&track.id).unwrap_or_default();
            track.content_id = content_id;
            tracks.push(track);
        }

        Ok(FederationExport {
            artists,
            releases,
            tracks,
        })
    }

    pub fn tracks_by_ids(&self, ids: &[i64]) -> Result<Vec<TrackItem>> {
        let conn = self.lock();
        let mut tracks = Vec::with_capacity(ids.len());
        for &id in ids {
            let mut found = query_tracks(
                &conn,
                &format!(
                    "SELECT {TRACK_COLUMNS} FROM tracks t
                     JOIN releases r ON r.id = t.release_id
                     WHERE t.id = ?1"
                ),
                params![id],
            )?;
            tracks.append(&mut found);
        }
        Ok(tracks)
    }

    pub fn track_by_content_id(&self, content_id: &str) -> Result<Option<TrackItem>> {
        let Some(content_id) = music_dht::normalize_content_id(content_id) else {
            return Ok(None);
        };
        let conn = self.lock();
        let mut tracks = query_tracks(
            &conn,
            &format!(
                "SELECT {TRACK_COLUMNS} FROM tracks t
                 JOIN releases r ON r.id = t.release_id
                 WHERE t.content_id = ?1
                 LIMIT 1"
            ),
            params![content_id],
        )?;
        Ok(tracks.pop())
    }

    // -----------------------------------------------------------------
    // Playlists & likes
    // -----------------------------------------------------------------

    pub fn playlists(&self) -> Result<Vec<PlaylistCard>> {
        let conn = self.lock();
        let liked: i64 = conn.query_row(
            "SELECT (SELECT COUNT(*) FROM likes) + (SELECT COUNT(*) FROM fed_likes)",
            [],
            |row| row.get(0),
        )?;
        let mut list = vec![PlaylistCard {
            id: LIKES_PLAYLIST_ID,
            title: "Liked tracks".to_string(),
            track_count: liked,
            kind: "likes".to_string(),
        }];
        let mut statement = conn.prepare(
            "SELECT p.id, p.title,
                (SELECT COUNT(*) FROM playlist_tracks pt WHERE pt.playlist_id = p.id)
                +
                (SELECT COUNT(*) FROM fed_playlist_tracks f
                 WHERE f.playlist_sync_id = p.sync_id
                   AND NOT EXISTS (
                    SELECT 1 FROM playlist_tracks pt
                    JOIN tracks t ON t.id = pt.track_id
                    WHERE pt.playlist_id = p.id
                      AND t.content_id = f.content_id
                   ))
             FROM playlists p ORDER BY p.title COLLATE NOCASE",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(PlaylistCard {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    track_count: row.get(2)?,
                    kind: "normal".to_string(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        list.extend(rows);
        Ok(list)
    }

    pub fn playlist(&self, id: i64) -> Result<PlaylistDetail> {
        let conn = self.lock();
        if id == LIKES_PLAYLIST_ID {
            let local_tracks = query_tracks(
                &conn,
                &format!(
                    "SELECT {TRACK_COLUMNS} FROM tracks t
                     JOIN releases r ON r.id = t.release_id
                     JOIN likes k ON k.track_id = t.id"
                ),
                params![],
            )?;
            let mut liked_at_by_track = HashMap::new();
            let mut liked_stmt = conn.prepare(
                "SELECT track_id,
                        COALESCE(liked_hlc_ms,
                                 CAST(strftime('%s', liked_at) AS INTEGER) * 1000,
                                 0)
                 FROM likes",
            )?;
            let liked_rows = liked_stmt
                .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
            for row in liked_rows {
                let (track_id, liked_at) = row?;
                liked_at_by_track.insert(track_id, liked_at);
            }
            drop(liked_stmt);

            let mut entries: Vec<(i64, String, TrackItem)> = local_tracks
                .into_iter()
                .map(|track| {
                    let liked_at = liked_at_by_track
                        .get(&track.id)
                        .copied()
                        .unwrap_or_default();
                    (liked_at, track.title.clone(), track)
                })
                .collect();

            let mut fed_stmt = conn.prepare(
                "SELECT COALESCE(liked_hlc_ms,
                                 CAST(strftime('%s', liked_at) AS INTEGER) * 1000,
                                 0),
                        item_id, owner, title, artist_names, featured_artist_names,
                        year, duration_seconds, content_id, release_title, track_number, disc_number
                 FROM fed_likes",
            )?;
            let fed_rows = fed_stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, fed_track_from_offset_row(row, 1)?))
            })?;
            for row in fed_rows {
                let (liked_at, fed) = row?;
                entries.push((
                    liked_at,
                    fed.title.clone(),
                    crate::federation::pending_track(&fed),
                ));
            }
            entries.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
            return Ok(PlaylistDetail {
                id,
                title: "Liked tracks".to_string(),
                description: None,
                tracks: entries.into_iter().map(|(_, _, track)| track).collect(),
            });
        }
        let (title, description, sync_id): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT title, description, sync_id FROM playlists WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .context("playlist not found")?;
        let local_tracks = query_tracks(
            &conn,
            &format!(
                "SELECT {TRACK_COLUMNS} FROM tracks t
                 JOIN releases r ON r.id = t.release_id
                 JOIN playlist_tracks pt ON pt.track_id = t.id
                 WHERE pt.playlist_id = ?1
                 ORDER BY pt.position"
            ),
            params![id],
        )?;
        let mut positions = HashMap::new();
        let mut position_stmt =
            conn.prepare("SELECT track_id, position FROM playlist_tracks WHERE playlist_id = ?1")?;
        let position_rows = position_stmt.query_map([id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in position_rows {
            let (track_id, position) = row?;
            positions.insert(track_id, position);
        }
        drop(position_stmt);
        let mut entries: Vec<(i64, TrackItem)> = local_tracks
            .into_iter()
            .enumerate()
            .map(|(index, track)| {
                let position = positions.get(&track.id).copied().unwrap_or(index as i64);
                (position, track)
            })
            .collect();
        if let Some(sync_id) = sync_id.as_deref() {
            for (position, fed) in fed_playlist_tracks(&conn, id, sync_id)? {
                entries.push((position, crate::federation::pending_track(&fed)));
            }
        }
        entries.sort_by(|(left_pos, left_track), (right_pos, right_track)| {
            left_pos
                .cmp(right_pos)
                .then_with(|| left_track.title.cmp(&right_track.title))
        });
        Ok(PlaylistDetail {
            id,
            title,
            description,
            tracks: entries.into_iter().map(|(_, track)| track).collect(),
        })
    }

    pub fn create_playlist(&self, title: &str) -> Result<PlaylistCard> {
        let conn = self.lock();
        conn.execute("INSERT INTO playlists (title) VALUES (?1)", [title])?;
        let id = conn.last_insert_rowid();
        let sync_id = make_playlist_sync_id(id, title);
        conn.execute(
            "UPDATE playlists SET sync_id = ?2 WHERE id = ?1",
            params![id, sync_id],
        )?;
        Ok(PlaylistCard {
            id,
            title: title.to_string(),
            track_count: 0,
            kind: "normal".to_string(),
        })
    }

    pub fn update_playlist(&self, id: i64, title: &str, description: Option<&str>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE playlists SET title = ?2, description = ?3 WHERE id = ?1",
            params![id, title, description],
        )?;
        Ok(())
    }

    pub fn delete_playlist(&self, id: i64) -> Result<()> {
        let conn = self.lock();
        let sync_id: Option<String> = conn
            .query_row("SELECT sync_id FROM playlists WHERE id = ?1", [id], |row| {
                row.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten();
        if let Some(sync_id) = sync_id {
            conn.execute(
                "DELETE FROM fed_playlist_tracks WHERE playlist_sync_id = ?1",
                [sync_id],
            )?;
        }
        conn.execute("DELETE FROM playlists WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn delete_playlist_by_sync_id(&self, sync_id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM fed_playlist_tracks WHERE playlist_sync_id = ?1",
            [sync_id],
        )?;
        conn.execute("DELETE FROM playlists WHERE sync_id = ?1", [sync_id])?;
        Ok(())
    }

    pub fn playlist_sync_id(&self, id: i64) -> Result<Option<String>> {
        let conn = self.lock();
        Ok(conn
            .query_row("SELECT sync_id FROM playlists WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn ensure_playlist_sync_id(&self, id: i64) -> Result<String> {
        let conn = self.lock();
        let existing: Option<String> = conn
            .query_row("SELECT sync_id FROM playlists WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?
            .flatten();
        if let Some(sync_id) = existing {
            return Ok(sync_id);
        }
        let title: String =
            conn.query_row("SELECT title FROM playlists WHERE id = ?1", [id], |row| {
                row.get(0)
            })?;
        let sync_id = make_playlist_sync_id(id, &title);
        conn.execute(
            "UPDATE playlists SET sync_id = ?2 WHERE id = ?1",
            params![id, sync_id],
        )?;
        Ok(sync_id)
    }

    pub fn upsert_synced_playlist(&self, sync_id: &str, title: &str) -> Result<i64> {
        let conn = self.lock();
        if let Some(id) = conn
            .query_row(
                "SELECT id FROM playlists WHERE sync_id = ?1",
                [sync_id],
                |row| row.get(0),
            )
            .optional()?
        {
            conn.execute(
                "UPDATE playlists SET title = ?2 WHERE id = ?1",
                params![id, title],
            )?;
            return Ok(id);
        }
        conn.execute(
            "INSERT INTO playlists (sync_id, title) VALUES (?1, ?2)",
            params![sync_id, title],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn add_tracks_to_playlist(&self, playlist_id: i64, track_ids: &[i64]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut next: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM playlist_tracks WHERE playlist_id = ?1",
            [playlist_id],
            |row| row.get(0),
        )?;
        for &track_id in track_ids {
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO playlist_tracks (playlist_id, track_id, position)
                 VALUES (?1, ?2, ?3)",
                params![playlist_id, track_id, next],
            )?;
            if inserted > 0 {
                next += 1;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn add_fed_tracks_to_playlist(
        &self,
        playlist_id: i64,
        tracks: &[crate::federation::FedTrack],
    ) -> Result<()> {
        if tracks.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let playlist_sync_id: Option<String> = tx
            .query_row(
                "SELECT sync_id FROM playlists WHERE id = ?1",
                [playlist_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let playlist_sync_id = match playlist_sync_id {
            Some(sync_id) => sync_id,
            None => {
                let title: String = tx.query_row(
                    "SELECT title FROM playlists WHERE id = ?1",
                    [playlist_id],
                    |row| row.get(0),
                )?;
                let sync_id = make_playlist_sync_id(playlist_id, &title);
                tx.execute(
                    "UPDATE playlists SET sync_id = ?2 WHERE id = ?1",
                    params![playlist_id, sync_id],
                )?;
                sync_id
            }
        };
        let local_max: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position), -1) FROM playlist_tracks WHERE playlist_id = ?1",
            [playlist_id],
            |row| row.get(0),
        )?;
        let fed_max: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position), -1) FROM fed_playlist_tracks WHERE playlist_sync_id = ?1",
            [&playlist_sync_id],
            |row| row.get(0),
        )?;
        let mut next = local_max.max(fed_max) + 1;
        for fed in tracks {
            let Some(content_id) = fed
                .content_id
                .as_deref()
                .and_then(music_dht::normalize_content_id)
            else {
                continue;
            };
            let existing_position: Option<i64> = tx
                .query_row(
                    "SELECT position FROM fed_playlist_tracks
                     WHERE playlist_sync_id = ?1 AND content_id = ?2",
                    params![playlist_sync_id, content_id],
                    |row| row.get(0),
                )
                .optional()?;
            let position = match existing_position {
                Some(position) => position,
                None => {
                    let position = next;
                    next += 1;
                    position
                }
            };
            tx.execute(
                "INSERT INTO fed_playlist_tracks
                    (playlist_sync_id, item_id, owner, title, artist_names,
                     featured_artist_names, year, duration_seconds, content_id,
                     release_title, track_number, disc_number, position)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(playlist_sync_id, content_id) DO UPDATE SET
                    item_id = excluded.item_id,
                    owner = excluded.owner,
                    title = excluded.title,
                    artist_names = excluded.artist_names,
                    featured_artist_names = excluded.featured_artist_names,
                    year = excluded.year,
                    duration_seconds = excluded.duration_seconds,
                    release_title = excluded.release_title,
                    track_number = excluded.track_number,
                    disc_number = excluded.disc_number,
                    position = excluded.position",
                params![
                    playlist_sync_id,
                    fed.item_id,
                    fed.owner,
                    fed.title,
                    fed.artist_names.join("; "),
                    fed.featured_artist_names.join("; "),
                    fed.year,
                    fed.duration_seconds.map(|d| d as f64),
                    content_id,
                    fed.release_title,
                    fed.track_number,
                    fed.disc_number,
                    position,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_tracks_from_playlist(&self, playlist_id: i64, track_ids: &[i64]) -> Result<()> {
        let conn = self.lock();
        for &track_id in track_ids {
            conn.execute(
                "DELETE FROM playlist_tracks WHERE playlist_id = ?1 AND track_id = ?2",
                params![playlist_id, track_id],
            )?;
        }
        Ok(())
    }

    pub fn remove_content_ids_from_playlist(
        &self,
        playlist_id: i64,
        content_ids: &[String],
    ) -> Result<()> {
        let conn = self.lock();
        let playlist_sync_id: Option<String> = conn
            .query_row(
                "SELECT sync_id FROM playlists WHERE id = ?1",
                [playlist_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        for content_id in content_ids {
            let Some(content_id) = music_dht::normalize_content_id(content_id) else {
                continue;
            };
            conn.execute(
                "DELETE FROM playlist_tracks
                 WHERE playlist_id = ?1
                   AND track_id IN (
                    SELECT id FROM tracks WHERE content_id = ?2
                   )",
                params![playlist_id, content_id],
            )?;
            if let Some(sync_id) = playlist_sync_id.as_deref() {
                conn.execute(
                    "DELETE FROM fed_playlist_tracks
                     WHERE playlist_sync_id = ?1 AND content_id = ?2",
                    params![sync_id, content_id],
                )?;
            }
        }
        Ok(())
    }

    pub fn track_content_id_by_id(&self, track_id: i64) -> Result<Option<String>> {
        let conn = self.lock();
        let content_id: Option<String> = conn
            .query_row(
                "SELECT content_id FROM tracks WHERE id = ?1",
                [track_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .and_then(|value| music_dht::normalize_content_id(&value));
        Ok(content_id)
    }

    pub fn playlist_track_content_positions(
        &self,
        playlist_id: i64,
        track_ids: &[i64],
    ) -> Result<Vec<(String, i64)>> {
        let conn = self.lock();
        let mut out = Vec::new();
        for &track_id in track_ids {
            let row: Option<(Option<String>, i64)> = conn
                .query_row(
                    "SELECT t.content_id, pt.position
                     FROM tracks t
                     JOIN playlist_tracks pt ON pt.track_id = t.id
                     WHERE t.id = ?1 AND pt.playlist_id = ?2",
                    params![track_id, playlist_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((content_id, position)) = row else {
                continue;
            };
            if let Some(content_id) = content_id
                .as_deref()
                .and_then(music_dht::normalize_content_id)
            {
                out.push((content_id, position));
            }
        }
        Ok(out)
    }

    pub fn playlist_content_position(
        &self,
        playlist_id: i64,
        content_id: &str,
    ) -> Result<Option<i64>> {
        let Some(content_id) = music_dht::normalize_content_id(content_id) else {
            return Ok(None);
        };
        let conn = self.lock();
        let local_position = conn
            .query_row(
                "SELECT pt.position
                 FROM playlist_tracks pt
                 JOIN tracks t ON t.id = pt.track_id
                 WHERE pt.playlist_id = ?1 AND t.content_id = ?2
                 LIMIT 1",
                params![playlist_id, content_id],
                |row| row.get(0),
            )
            .optional()?;
        if local_position.is_some() {
            return Ok(local_position);
        }
        let sync_id: Option<String> = conn
            .query_row(
                "SELECT sync_id FROM playlists WHERE id = ?1",
                [playlist_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let Some(sync_id) = sync_id else {
            return Ok(None);
        };
        Ok(conn
            .query_row(
                "SELECT position
                 FROM fed_playlist_tracks
                 WHERE playlist_sync_id = ?1 AND content_id = ?2
                 LIMIT 1",
                params![sync_id, content_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn track_id_by_content_id(&self, content_id: &str) -> Result<Option<i64>> {
        let Some(content_id) = music_dht::normalize_content_id(content_id) else {
            return Ok(None);
        };
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT id FROM tracks WHERE content_id = ?1 LIMIT 1",
                [content_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn liked_content_ids(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT DISTINCT t.content_id
             FROM likes k
             JOIN tracks t ON t.id = k.track_id
             WHERE t.content_id IS NOT NULL",
        )?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|content_id| music_dht::normalize_content_id(&content_id))
            .collect())
    }

    pub fn local_content_ids(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT DISTINCT content_id
             FROM tracks
             WHERE content_id IS NOT NULL",
        )?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|content_id| music_dht::normalize_content_id(&content_id))
            .collect())
    }

    /// Returns the new liked state for this content id.
    pub fn toggle_like_by_content_id(&self, content_id: &str) -> Result<bool> {
        let Some(content_id) = music_dht::normalize_content_id(content_id) else {
            anyhow::bail!("invalid content id");
        };
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let removed_local = tx.execute(
            "DELETE FROM likes
             WHERE track_id IN (
                SELECT id FROM tracks WHERE content_id = ?1
             )",
            [&content_id],
        )?;
        let removed_fed =
            tx.execute("DELETE FROM fed_likes WHERE content_id = ?1", [&content_id])?;
        if removed_local + removed_fed > 0 {
            tx.commit()?;
            return Ok(false);
        }
        let track_id: i64 = tx
            .query_row(
                "SELECT id FROM tracks WHERE content_id = ?1 ORDER BY id LIMIT 1",
                [&content_id],
                |row| row.get(0),
            )
            .optional()?
            .with_context(|| format!("no local track with content id {content_id}"))?;
        tx.execute("INSERT INTO likes (track_id) VALUES (?1)", [track_id])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn set_synced_like(&self, track_id: i64, liked: bool, liked_hlc_ms: i64) -> Result<bool> {
        let conn = self.lock();
        let changed = if liked {
            conn.execute(
                "INSERT INTO likes (track_id, liked_at, liked_hlc_ms)
                 VALUES (?1, datetime(?2 / 1000, 'unixepoch'), ?2)
                 ON CONFLICT(track_id) DO UPDATE SET
                    liked_at = excluded.liked_at,
                    liked_hlc_ms = excluded.liked_hlc_ms",
                params![track_id, liked_hlc_ms],
            )?
        } else {
            conn.execute("DELETE FROM likes WHERE track_id = ?1", [track_id])?
        };
        Ok(changed > 0)
    }

    pub fn upsert_fed_playlist_track(
        &self,
        playlist_sync_id: &str,
        fed: &crate::federation::FedTrack,
        position: i64,
    ) -> Result<bool> {
        let Some(content_id) = fed
            .content_id
            .as_deref()
            .and_then(music_dht::normalize_content_id)
        else {
            return Ok(false);
        };
        let conn = self.lock();
        Ok(conn.execute(
            "INSERT INTO fed_playlist_tracks
                (playlist_sync_id, item_id, owner, title, artist_names,
                 featured_artist_names, year, duration_seconds, content_id,
                 release_title, track_number, disc_number, position)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(playlist_sync_id, content_id) DO UPDATE SET
                item_id = excluded.item_id,
                owner = excluded.owner,
                title = excluded.title,
                artist_names = excluded.artist_names,
                featured_artist_names = excluded.featured_artist_names,
                year = excluded.year,
                duration_seconds = excluded.duration_seconds,
                release_title = excluded.release_title,
                track_number = excluded.track_number,
                disc_number = excluded.disc_number,
                position = excluded.position",
            params![
                playlist_sync_id,
                fed.item_id,
                fed.owner,
                fed.title,
                fed.artist_names.join("; "),
                fed.featured_artist_names.join("; "),
                fed.year,
                fed.duration_seconds.map(|d| d as f64),
                content_id,
                fed.release_title,
                fed.track_number,
                fed.disc_number,
                position,
            ],
        )? > 0)
    }

    pub fn fed_playlist_track_by_content_id(
        &self,
        playlist_sync_id: &str,
        content_id: &str,
    ) -> Result<Option<crate::federation::FedTrack>> {
        let Some(content_id) = music_dht::normalize_content_id(content_id) else {
            return Ok(None);
        };
        let conn = self.lock();
        conn.query_row(
            "SELECT item_id, owner, title, artist_names, featured_artist_names,
                    year, duration_seconds, content_id, release_title, track_number, disc_number
             FROM fed_playlist_tracks
             WHERE playlist_sync_id = ?1 AND content_id = ?2
             LIMIT 1",
            params![playlist_sync_id, content_id],
            fed_track_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn has_playlist_content_reference(
        &self,
        playlist_sync_id: &str,
        content_id: &str,
    ) -> Result<bool> {
        if self.track_id_by_content_id(content_id)?.is_some() {
            return Ok(true);
        }
        Ok(self
            .fed_playlist_track_by_content_id(playlist_sync_id, content_id)?
            .is_some())
    }

    pub fn add_content_id_to_synced_playlist(
        &self,
        playlist_sync_id: &str,
        content_id: &str,
    ) -> Result<bool> {
        let Some(track_id) = self.track_id_by_content_id(content_id)? else {
            return Ok(false);
        };
        let conn = self.lock();
        let playlist_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM playlists WHERE sync_id = ?1",
                [playlist_sync_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let Some(playlist_id) = playlist_id else {
            return Ok(false);
        };
        let next: i64 = conn.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM playlist_tracks WHERE playlist_id = ?1",
            [playlist_id],
            |row| row.get(0),
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO playlist_tracks (playlist_id, track_id, position)
             VALUES (?1, ?2, ?3)",
            params![playlist_id, track_id, next],
        )?;
        Ok(true)
    }

    pub fn remove_content_id_from_synced_playlist(
        &self,
        playlist_sync_id: &str,
        content_id: &str,
    ) -> Result<()> {
        let Some(content_id) = music_dht::normalize_content_id(content_id) else {
            return Ok(());
        };
        let conn = self.lock();
        let playlist_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM playlists WHERE sync_id = ?1",
                [playlist_sync_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if let Some(playlist_id) = playlist_id {
            let track_id: Option<i64> = conn
                .query_row(
                    "SELECT id FROM tracks WHERE content_id = ?1 LIMIT 1",
                    [&content_id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(track_id) = track_id {
                conn.execute(
                    "DELETE FROM playlist_tracks WHERE playlist_id = ?1 AND track_id = ?2",
                    params![playlist_id, track_id],
                )?;
            }
        }
        conn.execute(
            "DELETE FROM fed_playlist_tracks
             WHERE playlist_sync_id = ?1 AND content_id = ?2",
            params![playlist_sync_id, content_id],
        )?;
        Ok(())
    }

    /// Item ids and content ids of every liked federated track (for the ♥ markers).
    pub fn fed_like_ids(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut statement = conn.prepare("SELECT item_id, content_id FROM fed_likes")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut keys = Vec::new();
        for (item_id, content_id) in rows {
            keys.push(item_id);
            if let Some(content_id) = content_id
                .as_deref()
                .and_then(music_dht::normalize_content_id)
            {
                keys.push(content_id);
            }
        }
        Ok(keys)
    }

    /// Toggles a like on a federated track; returns the resulting state.
    pub fn toggle_fed_like(&self, fed: &crate::federation::FedTrack) -> Result<bool> {
        let content_id = fed
            .content_id
            .as_deref()
            .and_then(music_dht::normalize_content_id);
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let removed = match content_id.as_deref() {
            Some(content_id) => {
                let removed_fed = tx.execute(
                    "DELETE FROM fed_likes WHERE item_id = ?1 OR content_id = ?2",
                    params![fed.item_id, content_id],
                )?;
                let removed_local = tx.execute(
                    "DELETE FROM likes
                     WHERE track_id IN (
                        SELECT id FROM tracks WHERE content_id = ?1
                     )",
                    [content_id],
                )?;
                removed_fed + removed_local
            }
            None => tx.execute("DELETE FROM fed_likes WHERE item_id = ?1", [&fed.item_id])?,
        };
        if removed > 0 {
            tx.commit()?;
            return Ok(false);
        }
        if let Some(content_id) = content_id.as_deref() {
            tx.execute(
                "DELETE FROM fed_likes WHERE content_id = ?1 AND item_id != ?2",
                params![content_id, fed.item_id],
            )?;
        }
        tx.execute(
            "INSERT INTO fed_likes (item_id, owner, title, artist_names,
                featured_artist_names, year, duration_seconds, content_id,
                release_title, track_number, disc_number)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                fed.item_id,
                fed.owner,
                fed.title,
                fed.artist_names.join("; "),
                fed.featured_artist_names.join("; "),
                fed.year,
                fed.duration_seconds.map(|d| d as f64),
                content_id,
                fed.release_title,
                fed.track_number,
                fed.disc_number,
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn fed_like_by_content_id(
        &self,
        content_id: &str,
    ) -> Result<Option<crate::federation::FedTrack>> {
        let Some(content_id) = music_dht::normalize_content_id(content_id) else {
            return Ok(None);
        };
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT item_id, owner, title, artist_names, featured_artist_names,
                    year, duration_seconds, content_id, release_title, track_number, disc_number
             FROM fed_likes
             WHERE content_id = ?1
             ORDER BY COALESCE(liked_hlc_ms,
                               CAST(strftime('%s', liked_at) AS INTEGER) * 1000,
                               0) DESC
             LIMIT 1",
        )?;
        let track = statement
            .query_row([content_id], |row| {
                let artists: String = row.get(3)?;
                Ok(crate::federation::FedTrack {
                    item_id: row.get(0)?,
                    owner: row.get(1)?,
                    own: false,
                    title: row.get(2)?,
                    artist_names: artists
                        .split("; ")
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                        .collect(),
                    featured_artist_names: row
                        .get::<_, String>(4)?
                        .split("; ")
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                        .collect(),
                    year: row.get(5)?,
                    duration_seconds: row.get::<_, Option<f64>>(6)?.map(|d| d.round() as i64),
                    content_id: row.get(7)?,
                    release_title: row.get(8)?,
                    track_number: row.get(9)?,
                    disc_number: row.get(10)?,
                })
            })
            .optional()?;
        Ok(track)
    }

    pub fn upsert_synced_fed_like(
        &self,
        fed: &crate::federation::FedTrack,
        liked_hlc_ms: i64,
    ) -> Result<bool> {
        let Some(content_id) = fed
            .content_id
            .as_deref()
            .and_then(music_dht::normalize_content_id)
        else {
            return Ok(false);
        };
        let conn = self.lock();
        let duplicate_rows = conn.execute(
            "DELETE FROM fed_likes WHERE content_id = ?1 AND item_id != ?2",
            params![content_id, fed.item_id],
        )?;
        let existing: Option<(
            String,
            String,
            String,
            String,
            Option<i32>,
            Option<i64>,
            Option<String>,
            Option<i32>,
            Option<i32>,
            Option<i64>,
        )> = conn
            .query_row(
                "SELECT owner, title, artist_names, featured_artist_names,
                        year, duration_seconds, release_title, track_number, disc_number,
                        liked_hlc_ms
                 FROM fed_likes
                 WHERE item_id = ?1",
                [&fed.item_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get::<_, Option<f64>>(5)?.map(|d| d.round() as i64),
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                    ))
                },
            )
            .optional()?;
        let incoming = (
            fed.owner.clone(),
            fed.title.clone(),
            fed.artist_names.join("; "),
            fed.featured_artist_names.join("; "),
            fed.year,
            fed.duration_seconds,
            fed.release_title.clone(),
            fed.track_number,
            fed.disc_number,
            Some(liked_hlc_ms),
        );
        if duplicate_rows == 0 && existing.as_ref() == Some(&incoming) {
            return Ok(false);
        }
        conn.execute(
            "INSERT INTO fed_likes (item_id, owner, title, artist_names,
                featured_artist_names, year, duration_seconds, content_id,
                release_title, track_number, disc_number, liked_at, liked_hlc_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                     datetime(?12 / 1000, 'unixepoch'), ?12)
             ON CONFLICT(item_id) DO UPDATE SET
                owner = excluded.owner,
                title = excluded.title,
                artist_names = excluded.artist_names,
                featured_artist_names = excluded.featured_artist_names,
                year = excluded.year,
                duration_seconds = excluded.duration_seconds,
                content_id = excluded.content_id,
                release_title = excluded.release_title,
                track_number = excluded.track_number,
                disc_number = excluded.disc_number,
                liked_at = excluded.liked_at,
                liked_hlc_ms = excluded.liked_hlc_ms",
            params![
                fed.item_id,
                fed.owner,
                fed.title,
                fed.artist_names.join("; "),
                fed.featured_artist_names.join("; "),
                fed.year,
                fed.duration_seconds.map(|d| d as f64),
                content_id,
                fed.release_title,
                fed.track_number,
                fed.disc_number,
                liked_hlc_ms,
            ],
        )?;
        Ok(true)
    }

    pub fn remove_fed_like_by_content_id(&self, content_id: &str) -> Result<bool> {
        let Some(content_id) = music_dht::normalize_content_id(content_id) else {
            return Ok(false);
        };
        let conn = self.lock();
        Ok(conn.execute("DELETE FROM fed_likes WHERE content_id = ?1", [content_id])? > 0)
    }

    /// Moves a federated like onto a freshly imported local track. Returns
    /// whether a transfer happened.
    pub fn transfer_fed_like(&self, item_id: &str, track_id: i64) -> Result<bool> {
        let conn = self.lock();
        let liked: Option<(String, Option<i64>)> = conn
            .query_row(
                "SELECT liked_at, liked_hlc_ms FROM fed_likes WHERE item_id = ?1",
                [item_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let removed = conn.execute("DELETE FROM fed_likes WHERE item_id = ?1", [item_id])?;
        if removed == 0 {
            return Ok(false);
        }
        if let Some((liked_at, liked_hlc_ms)) = liked {
            conn.execute(
                "INSERT INTO likes (track_id, liked_at, liked_hlc_ms)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(track_id) DO UPDATE SET
                    liked_at = excluded.liked_at,
                    liked_hlc_ms = excluded.liked_hlc_ms",
                params![track_id, liked_at, liked_hlc_ms],
            )?;
        } else {
            conn.execute(
                "INSERT OR IGNORE INTO likes (track_id) VALUES (?1)",
                [track_id],
            )?;
        }
        Ok(true)
    }

    pub fn add_history(
        &self,
        track_id: i64,
        started_at: Option<i64>,
        listened_seconds: i32,
        completed: bool,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO history (track_id, started_at, listened_seconds, completed)
             VALUES (?1, ?2, ?3, ?4)",
            params![track_id, started_at, listened_seconds, completed],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Editing & deleting
    // -----------------------------------------------------------------

    pub fn update_track(&self, id: i64, edit: &TrackEdit) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE tracks SET title = ?2, track_number = ?3, disc_number = ?4 WHERE id = ?1",
            params![id, edit.title, edit.track_number, edit.disc_number],
        )?;
        // The cover is a release attribute; editing it from a track updates
        // the release cover (what every view shows for this track).
        tx.execute(
            "UPDATE releases SET cover_path = ?2
             WHERE id = (SELECT release_id FROM tracks WHERE id = ?1)",
            params![id, edit.cover_path],
        )?;
        tx.execute("DELETE FROM track_artists WHERE track_id = ?1", [id])?;
        link_track_artists(&tx, id, &edit.artists, "main")?;
        link_track_artists(&tx, id, &edit.featured_artists, "featured")?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_release(&self, id: i64, edit: &ReleaseEdit) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE releases SET title = ?2, release_type = ?3, year = ?4 WHERE id = ?1",
            params![id, edit.title, edit.release_type, edit.year],
        )?;
        // An empty artist list means "keep the current artists".
        if !edit.artists.is_empty() {
            tx.execute("DELETE FROM release_artists WHERE release_id = ?1", [id])?;
            for (position, name) in edit.artists.iter().enumerate() {
                let artist_id = find_or_create_artist(&tx, name)?;
                tx.execute(
                    "INSERT OR IGNORE INTO release_artists (release_id, artist_id, position)
                     VALUES (?1, ?2, ?3)",
                    params![id, artist_id, position as i64],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn update_artist(&self, id: i64, name: &str, image_path: Option<&str>) -> Result<()> {
        let conn = self.lock();
        let taken: Option<i64> = conn
            .query_row(
                "SELECT id FROM artists WHERE name = ?1 COLLATE NOCASE AND id != ?2",
                params![name, id],
                |row| row.get(0),
            )
            .optional()?;
        if taken.is_some() {
            anyhow::bail!("an artist named \"{name}\" already exists");
        }
        conn.execute(
            "UPDATE artists SET name = ?2, image_path = ?3 WHERE id = ?1",
            params![id, name, image_path],
        )?;
        Ok(())
    }

    /// Artist id by display name (case-insensitive), for catalog requests.
    pub fn artist_id_by_name(&self, name: &str) -> Result<Option<i64>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT id FROM artists WHERE norm(name) = norm(?1) LIMIT 1",
                [name],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Cover path of an artist's release, matched by names (for the peer
    /// catalog image protocol).
    pub fn release_cover_by_names(&self, artist: &str, release: &str) -> Result<Option<String>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT r.cover_path FROM releases r
                 JOIN release_artists ra ON ra.release_id = r.id
                 JOIN artists a ON a.id = ra.artist_id
                 WHERE norm(a.name) = norm(?1) AND norm(r.title) = norm(?2)
                 LIMIT 1",
                params![artist, release],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Image of one artist, for the federation metadata exchange.
    pub fn artist_image(&self, artist_id: i64) -> Result<Option<String>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT image_path FROM artists WHERE id = ?1",
                [artist_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    /// `true` when the artist exists and has no image yet.
    pub fn artist_image_missing(&self, name: &str) -> Result<bool> {
        let conn = self.lock();
        let missing: Option<bool> = conn
            .query_row(
                "SELECT image_path IS NULL FROM artists WHERE norm(name) = norm(?1)",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        Ok(missing.unwrap_or(false))
    }

    /// Sets an artist's image (matched by name) unless one is already set.
    /// Returns whether the image was applied.
    pub fn set_artist_image_if_missing(&self, name: &str, image_path: &str) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE artists SET image_path = ?2
             WHERE norm(name) = norm(?1) AND image_path IS NULL",
            params![name, image_path],
        )?;
        Ok(changed > 0)
    }

    pub fn delete_track(&self, id: i64) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM tracks WHERE id = ?1", [id])?;
        cleanup_empty_releases(&tx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_release(&self, id: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM releases WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Delete an artist together with everything that only existed because
    /// of them: tracks left without a main artist and releases left without
    /// tracks. Collaborations with other artists survive.
    pub fn delete_artist(&self, id: i64) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM artists WHERE id = ?1", [id])?;
        tx.execute(
            "DELETE FROM tracks WHERE id NOT IN
                (SELECT track_id FROM track_artists WHERE role = 'main')",
            [],
        )?;
        cleanup_empty_releases(&tx)?;
        tx.commit()?;
        Ok(())
    }
}

fn cleanup_empty_releases(tx: &rusqlite::Transaction) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM releases WHERE id NOT IN (SELECT release_id FROM tracks)",
        [],
    )?;
    Ok(())
}

fn link_track_artists(
    tx: &rusqlite::Transaction,
    track_id: i64,
    names: &[String],
    role: &str,
) -> Result<()> {
    for (position, name) in names.iter().enumerate() {
        let artist_id = find_or_create_artist(tx, name)?;
        tx.execute(
            "INSERT OR IGNORE INTO track_artists (track_id, artist_id, role, position)
             VALUES (?1, ?2, ?3, ?4)",
            params![track_id, artist_id, role, position as i64],
        )?;
    }
    Ok(())
}

pub(crate) fn find_or_create_artist(conn: &Connection, name: &str) -> Result<i64> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "artist name is empty");
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM artists WHERE norm(name) = norm(?1) LIMIT 1",
            [name],
            |row| row.get(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    conn.execute("INSERT INTO artists (name) VALUES (?1)", [name])?;
    Ok(conn.last_insert_rowid())
}

fn release_card_from_row(row: &rusqlite::Row) -> rusqlite::Result<ReleaseCard> {
    Ok(ReleaseCard {
        id: row.get(0)?,
        title: row.get(1)?,
        release_type: row.get(2)?,
        year: row.get(3)?,
        cover_path: row.get(4)?,
        track_count: row.get(5)?,
        availability: Availability::Local,
    })
}

fn rank_search_results(results: &mut SearchResults, normalized_query: &str) {
    results
        .artists
        .sort_by_key(|artist| exact_match_rank(&artist.name, normalized_query));
    results
        .releases
        .sort_by_key(|release| exact_match_rank(&release.title, normalized_query));
    results
        .tracks
        .sort_by_key(|track| track_match_rank(track, normalized_query));
}

fn exact_match_rank(value: &str, normalized_query: &str) -> u8 {
    if music_dht::normalize_name(value) == normalized_query {
        0
    } else {
        1
    }
}

fn track_match_rank(track: &TrackItem, normalized_query: &str) -> u8 {
    if music_dht::normalize_name(&track.title) == normalized_query {
        return 0;
    }
    if music_dht::normalize_name(&track.release_title) == normalized_query {
        return 1;
    }
    if track
        .artists
        .iter()
        .chain(track.featured_artists.iter())
        .any(|artist| music_dht::normalize_name(&artist.name) == normalized_query)
    {
        return 2;
    }
    3
}

/// Run a track query built on TRACK_COLUMNS and attach artist lists.
fn query_tracks(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::types::ToSql],
) -> Result<Vec<TrackItem>> {
    let mut statement = conn.prepare(sql)?;
    let mut tracks = statement
        .query_map(params, |row| {
            Ok(TrackItem {
                id: row.get(0)?,
                title: row.get(1)?,
                track_number: row.get(2)?,
                disc_number: row.get(3)?,
                duration_seconds: row.get(4)?,
                release_id: row.get(5)?,
                release_title: row.get(6)?,
                release_year: row.get(7)?,
                cover_path: row.get(8)?,
                file_path: row.get(9)?,
                audio_format: row.get(10)?,
                audio_bitrate: row.get(11)?,
                audio_sample_rate: row.get(12)?,
                audio_bit_depth: row.get(13)?,
                file_size_bytes: row.get(14)?,
                content_id: row
                    .get::<_, Option<String>>(15)?
                    .as_deref()
                    .and_then(music_dht::normalize_content_id),
                play_count: row.get(16)?,
                artists: Vec::new(),
                featured_artists: Vec::new(),
                fed: None,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut artist_statement = conn.prepare(
        "SELECT a.id, a.name, ta.role FROM artists a
         JOIN track_artists ta ON ta.artist_id = a.id
         WHERE ta.track_id = ?1
         ORDER BY ta.position",
    )?;
    for track in &mut tracks {
        let rows = artist_statement.query_map([track.id], |row| {
            Ok((
                ArtistRef {
                    id: row.get(0)?,
                    name: row.get(1)?,
                },
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (artist, role) = row?;
            if role == "featured" {
                track.featured_artists.push(artist);
            } else {
                track.artists.push(artist);
            }
        }
    }
    Ok(tracks)
}

fn fed_playlist_tracks(
    conn: &Connection,
    playlist_id: i64,
    playlist_sync_id: &str,
) -> Result<Vec<(i64, crate::federation::FedTrack)>> {
    let mut statement = conn.prepare(
        "SELECT position, item_id, owner, title, artist_names, featured_artist_names,
                year, duration_seconds, content_id, release_title, track_number, disc_number
         FROM fed_playlist_tracks f
         WHERE f.playlist_sync_id = ?1
           AND NOT EXISTS (
            SELECT 1 FROM playlist_tracks pt
            JOIN tracks t ON t.id = pt.track_id
            WHERE pt.playlist_id = ?2
              AND t.content_id = f.content_id
           )
         ORDER BY position, title COLLATE NOCASE",
    )?;
    let rows = statement.query_map(params![playlist_sync_id, playlist_id], |row| {
        Ok((row.get::<_, i64>(0)?, fed_track_from_offset_row(row, 1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn fed_track_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<crate::federation::FedTrack> {
    fed_track_from_offset_row(row, 0)
}

fn fed_track_from_offset_row(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<crate::federation::FedTrack> {
    Ok(crate::federation::FedTrack {
        item_id: row.get(offset)?,
        owner: row.get(offset + 1)?,
        own: false,
        title: row.get(offset + 2)?,
        artist_names: split_joined_names(row.get(offset + 3)?),
        featured_artist_names: split_joined_names(row.get(offset + 4)?),
        year: row.get(offset + 5)?,
        duration_seconds: row
            .get::<_, Option<f64>>(offset + 6)?
            .map(|d| d.round() as i64),
        content_id: row.get(offset + 7)?,
        release_title: row.get(offset + 8)?,
        track_number: row.get(offset + 9)?,
        disc_number: row.get(offset + 10)?,
    })
}

fn split_joined_names(names: String) -> Vec<String> {
    names
        .split("; ")
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// Escape LIKE wildcards in user input; queries use `ESCAPE '\'`.
/// Registers `norm(text)` — Unicode-aware case folding and normalization
/// (NFKC, lowercase, punctuation stripped). SQLite's own LIKE/NOCASE only
/// fold ASCII, so Cyrillic and other non-Latin names were unsearchable
/// without it. Shares the exact algorithm with the federation DHT.
fn register_norm_function(conn: &Connection) -> Result<()> {
    use rusqlite::functions::FunctionFlags;
    conn.create_scalar_function(
        "norm",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let value: String = ctx.get(0)?;
            Ok(music_dht::normalize_name(&value))
        },
    )?;
    Ok(())
}

pub(crate) fn audio_content_id(path: &str) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher).ok()?;
    Some(format!("b3:{}", hasher.finalize().to_hex()))
}

fn directory_size(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::metadata(path) else {
        return 0;
    };
    if metadata.is_file() {
        return metadata.len();
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| directory_size(&entry.path()))
        .sum()
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn sqlite_database_size(path: &Path) -> u64 {
    file_size(path)
        .saturating_add(file_size(&path_with_suffix(path, "-wal")))
        .saturating_add(file_size(&path_with_suffix(path, "-shm")))
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn now_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn remote_artist_id(artist_key: &str) -> i64 {
    let hash = blake3::hash(artist_key.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&hash.as_bytes()[..8]);
    -(i64::from_be_bytes(bytes) & i64::MAX).max(1)
}

fn ensure_schema_migrations(conn: &Connection) -> Result<()> {
    let fed_like_columns = table_columns(conn, "fed_likes")?;
    if !fed_like_columns
        .iter()
        .any(|column| column == "featured_artist_names")
    {
        conn.execute(
            "ALTER TABLE fed_likes
             ADD COLUMN featured_artist_names TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    if !fed_like_columns.iter().any(|column| column == "content_id") {
        conn.execute("ALTER TABLE fed_likes ADD COLUMN content_id TEXT", [])?;
    }
    if !fed_like_columns
        .iter()
        .any(|column| column == "liked_hlc_ms")
    {
        conn.execute("ALTER TABLE fed_likes ADD COLUMN liked_hlc_ms INTEGER", [])?;
    }
    let like_columns = table_columns(conn, "likes")?;
    if !like_columns.iter().any(|column| column == "liked_hlc_ms") {
        conn.execute("ALTER TABLE likes ADD COLUMN liked_hlc_ms INTEGER", [])?;
    }
    let track_columns = table_columns(conn, "tracks")?;
    if !track_columns.iter().any(|column| column == "content_id") {
        conn.execute("ALTER TABLE tracks ADD COLUMN content_id TEXT", [])?;
    }
    let playlist_columns = table_columns(conn, "playlists")?;
    if !playlist_columns.iter().any(|column| column == "sync_id") {
        conn.execute("ALTER TABLE playlists ADD COLUMN sync_id TEXT", [])?;
        conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_playlists_sync_id
             ON playlists(sync_id)",
            [],
        )?;
    }
    let network_artist_columns = table_columns(conn, "network_artist_cache")?;
    if !network_artist_columns
        .iter()
        .any(|column| column == "remote_image_hint")
    {
        conn.execute(
            "ALTER TABLE network_artist_cache ADD COLUMN remote_image_hint TEXT",
            [],
        )?;
    }
    let mut rows = conn.prepare("SELECT id, title FROM playlists WHERE sync_id IS NULL")?;
    let missing = rows
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(rows);
    for (id, title) in missing {
        conn.execute(
            "UPDATE playlists SET sync_id = ?2 WHERE id = ?1",
            params![id, make_playlist_sync_id(id, &title)],
        )?;
    }
    Ok(())
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    Ok(statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn make_playlist_sync_id(id: i64, title: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("playlist:{id}:{title}:{now}:{}", std::process::id());
    format!("pl_{}", &blake3::hash(seed.as_bytes()).to_hex()[..24])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_library() -> Library {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        register_norm_function(&conn).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Library {
            conn: Mutex::new(conn),
            db_path: std::env::temp_dir().join("furumi-test-library.db"),
            covers_dir: std::env::temp_dir().join("furumi-test-covers-unused"),
        }
    }

    fn add_track(lib: &Library, title: &str, artist: &str, album: &str) -> i64 {
        add_track_with_featured(lib, title, artist, &[], album)
    }

    fn add_track_with_featured(
        lib: &Library,
        title: &str,
        artist: &str,
        featured: &[&str],
        album: &str,
    ) -> i64 {
        let import = import::TrackImport {
            release_type: None,
            file_path: format!("/music/{artist}/{album}/{title}.mp3"),
            title: title.to_string(),
            artists: vec![artist.to_string()],
            featured_artists: featured.iter().map(|name| (*name).to_string()).collect(),
            album_artists: vec![artist.to_string()],
            release_title: album.to_string(),
            year: Some(2020),
            track_number: None,
            disc_number: None,
            duration_seconds: 60.0,
            audio_format: Some("mp3".into()),
            audio_bitrate: Some(320),
            audio_sample_rate: Some(44100),
            audio_bit_depth: None,
            file_size_bytes: Some(1),
            cover: None,
        };
        let id = import::upsert_track(lib, &import).unwrap().0;
        let content_id = format!("b3:{}", blake3::hash(import.file_path.as_bytes()).to_hex());
        lib.lock()
            .execute(
                "UPDATE tracks SET content_id = ?2 WHERE id = ?1",
                params![id, content_id],
            )
            .unwrap();
        id
    }

    fn artist_filters(hide_featured_only: bool) -> crate::config::settings::LibraryFilters {
        crate::config::settings::LibraryFilters {
            hide_featured_only,
            ..Default::default()
        }
    }

    #[test]
    fn local_stats_counts_library_rows_and_audio_bytes() {
        let lib = test_library();
        add_track(&lib, "One", "Artist", "First");
        add_track(&lib, "Two", "Artist", "Second");

        let stats = lib.local_stats().unwrap();
        assert_eq!(stats.artist_count, 1);
        assert_eq!(stats.release_count, 2);
        assert_eq!(stats.track_count, 2);
        assert_eq!(stats.audio_bytes, 2);
        assert_eq!(stats.tracks_without_size, 0);
    }

    #[test]
    fn artists_page_prioritizes_releases_then_tracks() {
        let lib = test_library();
        add_track(&lib, "Solo", "Zed", "Zed Album");
        add_track_with_featured(&lib, "Guest One", "A Host", &["Guest"], "A Host Album");
        add_track_with_featured(&lib, "Guest Two", "B Host", &["Guest"], "B Host Album");

        let page = lib.artists(1, 10, artist_filters(false)).unwrap();
        let zed_pos = page
            .items
            .iter()
            .position(|artist| artist.name == "Zed")
            .unwrap();
        let guest_pos = page
            .items
            .iter()
            .position(|artist| artist.name == "Guest")
            .unwrap();
        let guest = &page.items[guest_pos];

        assert_eq!(guest.release_count, 0);
        assert_eq!(guest.track_count, 2);
        assert!(zed_pos < guest_pos);

        let filtered = lib.artists(1, 10, artist_filters(true)).unwrap();
        assert!(filtered.items.iter().all(|artist| artist.release_count > 0));
        assert!(!filtered.items.iter().any(|artist| artist.name == "Guest"));
    }

    #[test]
    fn network_artist_image_hint_becomes_local_image_after_fetch() {
        let lib = test_library();
        let artist_key = music_dht::normalize_name("Remote Artist");
        lib.replace_network_artist_cache(
            "peer-a",
            "personal",
            &[NetworkArtistPreview {
                artist_key: artist_key.clone(),
                name: "Remote Artist".into(),
                image_path: Some("peer-local/image.jpg".into()),
                release_count: 1,
                track_count: 3,
            }],
            true,
        )
        .unwrap();

        let filters = crate::config::settings::LibraryFilters {
            source_mode: crate::config::settings::LibrarySourceMode::My,
            ..Default::default()
        };
        let page = lib.artists(1, 10, filters).unwrap();
        assert_eq!(page.items[0].image_path, None);

        let requests = lib
            .network_artist_image_requests(filters, &["Remote Artist".into()], 8)
            .unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].source_id, "peer-a");
        assert_eq!(requests[0].artist_key, artist_key);

        lib.set_network_artist_image("peer-a", &artist_key, "/tmp/remote-artist.jpg")
            .unwrap();
        let page = lib.artists(1, 10, filters).unwrap();
        assert_eq!(
            page.items[0].image_path.as_deref(),
            Some("/tmp/remote-artist.jpg")
        );
    }

    #[test]
    fn import_creates_artist_release_track() {
        let lib = test_library();
        let track_id = add_track(&lib, "Song", "Artist", "Album");
        let page = lib.artists(1, 10, artist_filters(false)).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].name, "Artist");
        assert_eq!(page.items[0].track_count, 1);

        let detail = lib.artist(page.items[0].id).unwrap();
        assert_eq!(detail.releases.len(), 1);
        assert_eq!(detail.top_tracks.len(), 1);

        let release = lib.release(detail.releases[0].id).unwrap();
        assert_eq!(release.tracks.len(), 1);
        assert_eq!(release.tracks[0].id, track_id);
        assert_eq!(release.tracks[0].artists[0].name, "Artist");
    }

    #[test]
    fn reimport_updates_instead_of_duplicating() {
        let lib = test_library();
        let first = add_track(&lib, "Song", "Artist", "Album");
        let second = add_track(&lib, "Song", "Artist", "Album");
        assert_eq!(first, second);
        let page = lib.artists(1, 10, artist_filters(false)).unwrap();
        assert_eq!(page.items[0].track_count, 1);
    }

    #[test]
    fn content_id_backfill_hashes_missing_track_ids() {
        let lib = test_library();
        let path = std::env::temp_dir().join(format!(
            "furumi-content-id-test-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"portable content id").unwrap();
        let file_path = path.to_string_lossy().into_owned();
        let import = import::TrackImport {
            release_type: None,
            file_path: file_path.clone(),
            title: "Portable".to_string(),
            artists: vec!["Artist".to_string()],
            featured_artists: Vec::new(),
            album_artists: vec!["Artist".to_string()],
            release_title: "Album".to_string(),
            year: Some(2026),
            track_number: None,
            disc_number: None,
            duration_seconds: 60.0,
            audio_format: Some("bin".into()),
            audio_bitrate: None,
            audio_sample_rate: None,
            audio_bit_depth: None,
            file_size_bytes: Some(19),
            cover: None,
        };
        let track_id = import::upsert_track(&lib, &import).unwrap().0;
        let expected = audio_content_id(&file_path).unwrap();
        {
            let conn = lib.lock();
            conn.execute(
                "UPDATE tracks SET content_id = NULL WHERE id = ?1",
                [track_id],
            )
            .unwrap();
        }

        let stats = lib.backfill_missing_content_ids().unwrap();
        assert_eq!(stats.hashed, 1);
        assert_eq!(stats.updated(), 1);
        assert_eq!(
            lib.track_content_id_by_id(track_id).unwrap().as_deref(),
            Some(expected.as_str())
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn search_finds_all_kinds() {
        let lib = test_library();
        add_track(&lib, "Neon Lights", "Neon Artist", "Neon Album");
        let results = lib.search("neon", 10).unwrap();
        assert_eq!(results.artists.len(), 1);
        assert_eq!(results.releases.len(), 1);
        assert_eq!(results.tracks.len(), 1);
        // LIKE wildcards in the query must not match everything.
        assert_eq!(lib.search("%", 10).unwrap().len(), 0);
    }

    #[test]
    fn search_ranks_exact_names_first() {
        let lib = test_library();
        add_track(&lib, "A Needle", "A Needle Artist", "A Needle Album");
        add_track(&lib, "Needle", "Needle", "Needle");

        let results = lib.search("needle", 10).unwrap();
        assert_eq!(results.artists[0].name, "Needle");
        assert_eq!(results.releases[0].title, "Needle");
        assert_eq!(results.tracks[0].title, "Needle");
    }

    #[test]
    fn search_folds_case_beyond_ascii() {
        let lib = test_library();
        add_track(&lib, "Nothing Else Matters", "Металлика", "Чёрный альбом");
        // SQLite's LIKE/NOCASE only fold ASCII; norm() folds every script.
        assert_eq!(lib.search("металлика", 10).unwrap().artists.len(), 1);
        assert_eq!(lib.search("МЕТАЛЛИКА", 10).unwrap().artists.len(), 1);
        assert_eq!(lib.search("чёрный", 10).unwrap().releases.len(), 1);
        assert_eq!(lib.search("matters", 10).unwrap().tracks.len(), 1);
    }

    #[test]
    fn playlists_and_likes_round_trip() {
        let lib = test_library();
        let track_id = add_track(&lib, "Song", "Artist", "Album");
        let playlist = lib.create_playlist("Mix").unwrap();
        lib.add_tracks_to_playlist(playlist.id, &[track_id])
            .unwrap();
        assert_eq!(lib.playlist(playlist.id).unwrap().tracks.len(), 1);

        let content_id = lib.track_content_id_by_id(track_id).unwrap().unwrap();
        assert!(lib.toggle_like_by_content_id(&content_id).unwrap());
        assert_eq!(lib.liked_content_ids().unwrap(), vec![content_id.clone()]);
        assert_eq!(lib.playlist(LIKES_PLAYLIST_ID).unwrap().tracks.len(), 1);
        assert!(!lib.toggle_like_by_content_id(&content_id).unwrap());

        lib.remove_tracks_from_playlist(playlist.id, &[track_id])
            .unwrap();
        assert_eq!(lib.playlist(playlist.id).unwrap().tracks.len(), 0);
        lib.delete_playlist(playlist.id).unwrap();
        // Only the virtual Likes playlist remains.
        assert_eq!(lib.playlists().unwrap().len(), 1);
    }

    #[test]
    fn likes_playlist_orders_local_and_federated_by_liked_at() {
        let lib = test_library();
        let old_id = add_track(&lib, "Old Local", "Artist", "Album");
        let new_id = add_track(&lib, "New Local", "Artist", "Album");
        let old_content_id = lib.track_content_id_by_id(old_id).unwrap().unwrap();
        let new_content_id = lib.track_content_id_by_id(new_id).unwrap().unwrap();
        let content_id = format!("b3:{}", "c".repeat(64));
        let fed = crate::federation::FedTrack {
            item_id: "fed_item_order".to_string(),
            owner: "fed_owner_order".to_string(),
            own: false,
            title: "Middle Fed".to_string(),
            artist_names: vec!["Remote Artist".to_string()],
            featured_artist_names: Vec::new(),
            year: Some(2026),
            duration_seconds: Some(123),
            content_id: Some(content_id),
            release_title: Some("Remote Release".to_string()),
            track_number: Some(1),
            disc_number: Some(1),
        };

        assert!(lib.toggle_like_by_content_id(&old_content_id).unwrap());
        assert!(lib.toggle_like_by_content_id(&new_content_id).unwrap());
        assert!(lib.toggle_fed_like(&fed).unwrap());
        {
            let conn = lib.lock();
            conn.execute(
                "UPDATE likes SET liked_at = ?2 WHERE track_id = ?1",
                params![old_id, "2026-01-01 00:00:00"],
            )
            .unwrap();
            conn.execute(
                "UPDATE likes SET liked_at = ?2 WHERE track_id = ?1",
                params![new_id, "2026-01-02 00:00:00"],
            )
            .unwrap();
            conn.execute(
                "UPDATE fed_likes SET liked_at = ?2 WHERE item_id = ?1",
                params![fed.item_id, "2026-01-03 00:00:00"],
            )
            .unwrap();
        }

        let titles: Vec<String> = lib
            .playlist(LIKES_PLAYLIST_ID)
            .unwrap()
            .tracks
            .into_iter()
            .map(|track| track.title)
            .collect();
        assert_eq!(titles, vec!["Middle Fed", "New Local", "Old Local"]);

        assert!(!lib.toggle_like_by_content_id(&old_content_id).unwrap());
        assert!(lib.toggle_like_by_content_id(&old_content_id).unwrap());
        {
            let conn = lib.lock();
            conn.execute(
                "UPDATE likes SET liked_at = ?2 WHERE track_id = ?1",
                params![old_id, "2026-01-04 00:00:00"],
            )
            .unwrap();
        }
        let titles: Vec<String> = lib
            .playlist(LIKES_PLAYLIST_ID)
            .unwrap()
            .tracks
            .into_iter()
            .map(|track| track.title)
            .collect();
        assert_eq!(titles, vec!["Old Local", "Middle Fed", "New Local"]);
    }

    #[test]
    fn synced_playlist_can_show_federated_pending_tracks() {
        let lib = test_library();
        let playlist = lib.create_playlist("Remote Mix").unwrap();
        let sync_id = lib.ensure_playlist_sync_id(playlist.id).unwrap();
        let content_id = format!("b3:{}", "a".repeat(64));
        let fed = crate::federation::FedTrack {
            item_id: "fed_item_1".to_string(),
            owner: "fed_owner_1".to_string(),
            own: false,
            title: "Remote Song".to_string(),
            artist_names: vec!["Remote Artist".to_string()],
            featured_artist_names: vec!["Remote Guest".to_string()],
            year: Some(2026),
            duration_seconds: Some(123),
            content_id: Some(content_id.clone()),
            release_title: Some("Remote Release".to_string()),
            track_number: Some(2),
            disc_number: Some(1),
        };

        assert!(lib.upsert_fed_playlist_track(&sync_id, &fed, 4).unwrap());
        assert!(
            lib.has_playlist_content_reference(&sync_id, &content_id)
                .unwrap()
        );

        let detail = lib.playlist(playlist.id).unwrap();
        assert_eq!(detail.tracks.len(), 1);
        let track = &detail.tracks[0];
        assert!(track.is_fed_pending());
        assert_eq!(track.title, "Remote Song");
        assert_eq!(track.artist_line(), "Remote Artist feat. Remote Guest");
        assert_eq!(track.release_title, "Remote Release");
        assert_eq!(track.content_id.as_deref(), Some(content_id.as_str()));

        let card = lib
            .playlists()
            .unwrap()
            .into_iter()
            .find(|card| card.id == playlist.id)
            .unwrap();
        assert_eq!(card.track_count, 1);

        lib.remove_content_ids_from_playlist(playlist.id, std::slice::from_ref(&content_id))
            .unwrap();
        assert_eq!(lib.playlist(playlist.id).unwrap().tracks.len(), 0);
        assert!(
            lib.fed_playlist_track_by_content_id(&sync_id, &content_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn add_federated_pending_track_to_playlist_records_position() {
        let lib = test_library();
        let local_id = add_track(&lib, "Local Song", "Artist", "Album");
        let playlist = lib.create_playlist("Remote Mix").unwrap();
        let content_id = format!("b3:{}", "b".repeat(64));
        let fed = crate::federation::FedTrack {
            item_id: "fed_item_2".to_string(),
            owner: "fed_owner_2".to_string(),
            own: false,
            title: "Remote Song".to_string(),
            artist_names: vec!["Remote Artist".to_string()],
            featured_artist_names: Vec::new(),
            year: Some(2026),
            duration_seconds: Some(123),
            content_id: Some(content_id.clone()),
            release_title: Some("Remote Release".to_string()),
            track_number: Some(2),
            disc_number: Some(1),
        };

        lib.add_tracks_to_playlist(playlist.id, &[local_id])
            .unwrap();
        lib.add_fed_tracks_to_playlist(playlist.id, std::slice::from_ref(&fed))
            .unwrap();

        let position = lib
            .playlist_content_position(playlist.id, &content_id)
            .unwrap();
        assert_eq!(position, Some(1));
        let detail = lib.playlist(playlist.id).unwrap();
        assert_eq!(
            detail
                .tracks
                .into_iter()
                .map(|track| track.title)
                .collect::<Vec<_>>(),
            vec!["Local Song", "Remote Song"]
        );
    }

    #[test]
    fn track_edit_relinks_artists() {
        let lib = test_library();
        let track_id = add_track(&lib, "Song", "Artist", "Album");
        lib.update_track(
            track_id,
            &TrackEdit {
                title: "Renamed".into(),
                artists: vec!["Other".into()],
                featured_artists: vec!["Guest".into()],
                track_number: Some(2),
                disc_number: None,
                cover_path: None,
            },
        )
        .unwrap();
        let track = lib.tracks_by_ids(&[track_id]).unwrap().remove(0);
        assert_eq!(track.title, "Renamed");
        assert_eq!(track.artists[0].name, "Other");
        assert_eq!(track.featured_artists[0].name, "Guest");
        assert_eq!(track.track_number, Some(2));
    }

    #[test]
    fn deleting_artist_cleans_up_own_content() {
        let lib = test_library();
        add_track(&lib, "Song", "Solo", "Solo Album");
        let page = lib.artists(1, 10, artist_filters(false)).unwrap();
        lib.delete_artist(page.items[0].id).unwrap();
        assert_eq!(lib.artists(1, 10, artist_filters(false)).unwrap().total, 0);
        assert_eq!(lib.search("Song", 10).unwrap().len(), 0);
    }

    #[test]
    fn delete_track_drops_empty_release() {
        let lib = test_library();
        let track_id = add_track(&lib, "Only", "Artist", "Album");
        lib.delete_track(track_id).unwrap();
        let detail = lib
            .artist(lib.artists(1, 10, artist_filters(false)).unwrap().items[0].id)
            .unwrap();
        assert!(detail.releases.is_empty());
    }

    #[test]
    fn history_counts_completed_plays() {
        let lib = test_library();
        let track_id = add_track(&lib, "Song", "Artist", "Album");
        lib.add_history(track_id, None, 60, true).unwrap();
        lib.add_history(track_id, None, 10, false).unwrap();
        let track = lib.tracks_by_ids(&[track_id]).unwrap().remove(0);
        assert_eq!(track.play_count, 1);
    }
}
