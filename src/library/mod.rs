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

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OptionalExtension as _, params};

use models::{
    ArtistCard, ArtistDetail, ArtistRef, ArtistsPage, PlaylistCard, PlaylistDetail, ReleaseCard,
    ReleaseDetail, ReleaseEdit, SearchResults, TrackEdit, TrackItem,
};

/// The virtual "Liked tracks" playlist id, kept from the server API.
pub const LIKES_PLAYLIST_ID: i64 = -1;

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
    liked_at  TEXT NOT NULL DEFAULT (datetime('now'))
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
    liked_at         TEXT NOT NULL DEFAULT (datetime('now'))
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

pub struct Library {
    conn: Mutex<Connection>,
    /// Directory where extracted embedded covers are stored.
    covers_dir: PathBuf,
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
            covers_dir,
        })
    }

    pub fn covers_dir(&self) -> &Path {
        &self.covers_dir
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

    pub fn artists(&self, page: i64, limit: i64, hide_featured_only: bool) -> Result<ArtistsPage> {
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
            let mut tracks = query_tracks(
                &conn,
                &format!(
                    "SELECT {TRACK_COLUMNS} FROM tracks t
                     JOIN releases r ON r.id = t.release_id
                     JOIN likes k ON k.track_id = t.id
                     ORDER BY k.liked_at DESC"
                ),
                params![],
            )?;
            drop(conn);
            // Liked federated tracks follow the local ones as queueable
            // placeholders (downloaded on playback like everywhere else).
            for fed in self.fed_likes()? {
                tracks.push(crate::federation::pending_track(&fed));
            }
            return Ok(PlaylistDetail {
                id,
                title: "Liked tracks".to_string(),
                description: None,
                tracks,
            });
        }
        let (title, description): (String, Option<String>) = conn
            .query_row(
                "SELECT title, description FROM playlists WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .context("playlist not found")?;
        let tracks = query_tracks(
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
        Ok(PlaylistDetail {
            id,
            title,
            description,
            tracks,
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
        conn.execute("DELETE FROM playlists WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn delete_playlist_by_sync_id(&self, sync_id: &str) -> Result<()> {
        let conn = self.lock();
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

    pub fn track_content_ids(&self, track_ids: &[i64]) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut out = Vec::new();
        for &track_id in track_ids {
            let content_id: Option<String> = conn
                .query_row(
                    "SELECT content_id FROM tracks WHERE id = ?1",
                    [track_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten()
                .and_then(|value| music_dht::normalize_content_id(&value));
            if let Some(content_id) = content_id {
                out.push(content_id);
            }
        }
        Ok(out)
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

    pub fn set_like(&self, track_id: i64, liked: bool) -> Result<()> {
        let conn = self.lock();
        if liked {
            conn.execute(
                "INSERT OR IGNORE INTO likes (track_id) VALUES (?1)",
                [track_id],
            )?;
        } else {
            conn.execute("DELETE FROM likes WHERE track_id = ?1", [track_id])?;
        }
        Ok(())
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
        let Some(track_id) = self.track_id_by_content_id(content_id)? else {
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
        let Some(playlist_id) = playlist_id else {
            return Ok(());
        };
        conn.execute(
            "DELETE FROM playlist_tracks WHERE playlist_id = ?1 AND track_id = ?2",
            params![playlist_id, track_id],
        )?;
        Ok(())
    }

    /// Liked federated tracks, newest first, as playable references.
    pub fn fed_likes(&self) -> Result<Vec<crate::federation::FedTrack>> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT item_id, owner, title, artist_names, featured_artist_names,
                    year, duration_seconds, content_id, release_title, track_number, disc_number
             FROM fed_likes ORDER BY liked_at DESC",
        )?;
        let rows = statement
            .query_map([], |row| {
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
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Item ids of every liked federated track (for the ♥ markers).
    pub fn fed_like_ids(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut statement = conn.prepare("SELECT item_id FROM fed_likes")?;
        let rows = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    /// Toggles a like on a federated track; returns the resulting state.
    pub fn toggle_fed_like(&self, fed: &crate::federation::FedTrack) -> Result<bool> {
        let conn = self.lock();
        let removed = conn.execute("DELETE FROM fed_likes WHERE item_id = ?1", [&fed.item_id])?;
        if removed > 0 {
            return Ok(false);
        }
        conn.execute(
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
                fed.content_id,
                fed.release_title,
                fed.track_number,
                fed.disc_number,
            ],
        )?;
        Ok(true)
    }

    /// Moves a federated like onto a freshly imported local track. Returns
    /// whether a transfer happened.
    pub fn transfer_fed_like(&self, item_id: &str, track_id: i64) -> Result<bool> {
        let conn = self.lock();
        let removed = conn.execute("DELETE FROM fed_likes WHERE item_id = ?1", [item_id])?;
        if removed == 0 {
            return Ok(false);
        }
        conn.execute(
            "INSERT OR IGNORE INTO likes (track_id) VALUES (?1)",
            [track_id],
        )?;
        Ok(true)
    }

    pub fn likes(&self) -> Result<Vec<i64>> {
        let conn = self.lock();
        let mut statement = conn.prepare("SELECT track_id FROM likes")?;
        let ids = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(ids)
    }

    /// Returns the new liked state.
    pub fn toggle_like(&self, track_id: i64) -> Result<bool> {
        let conn = self.lock();
        let removed = conn.execute("DELETE FROM likes WHERE track_id = ?1", [track_id])?;
        if removed > 0 {
            return Ok(false);
        }
        conn.execute("INSERT INTO likes (track_id) VALUES (?1)", [track_id])?;
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
            covers_dir: std::env::temp_dir(),
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
        import::upsert_track(lib, &import).unwrap().0
    }

    #[test]
    fn artists_page_prioritizes_releases_then_tracks() {
        let lib = test_library();
        add_track(&lib, "Solo", "Zed", "Zed Album");
        add_track_with_featured(&lib, "Guest One", "A Host", &["Guest"], "A Host Album");
        add_track_with_featured(&lib, "Guest Two", "B Host", &["Guest"], "B Host Album");

        let page = lib.artists(1, 10, false).unwrap();
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

        let filtered = lib.artists(1, 10, true).unwrap();
        assert!(filtered.items.iter().all(|artist| artist.release_count > 0));
        assert!(!filtered.items.iter().any(|artist| artist.name == "Guest"));
    }

    #[test]
    fn import_creates_artist_release_track() {
        let lib = test_library();
        let track_id = add_track(&lib, "Song", "Artist", "Album");
        let page = lib.artists(1, 10, false).unwrap();
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
        let page = lib.artists(1, 10, false).unwrap();
        assert_eq!(page.items[0].track_count, 1);
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

        assert!(lib.toggle_like(track_id).unwrap());
        assert_eq!(lib.likes().unwrap(), vec![track_id]);
        assert_eq!(lib.playlist(LIKES_PLAYLIST_ID).unwrap().tracks.len(), 1);
        assert!(!lib.toggle_like(track_id).unwrap());

        lib.remove_tracks_from_playlist(playlist.id, &[track_id])
            .unwrap();
        assert_eq!(lib.playlist(playlist.id).unwrap().tracks.len(), 0);
        lib.delete_playlist(playlist.id).unwrap();
        // Only the virtual Likes playlist remains.
        assert_eq!(lib.playlists().unwrap().len(), 1);
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
        let page = lib.artists(1, 10, false).unwrap();
        lib.delete_artist(page.items[0].id).unwrap();
        assert_eq!(lib.artists(1, 10, false).unwrap().total, 0);
        assert_eq!(lib.search("Song", 10).unwrap().len(), 0);
    }

    #[test]
    fn delete_track_drops_empty_release() {
        let lib = test_library();
        let track_id = add_track(&lib, "Only", "Artist", "Album");
        lib.delete_track(track_id).unwrap();
        let detail = lib
            .artist(lib.artists(1, 10, false).unwrap().items[0].id)
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
