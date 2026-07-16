//! Importing audio files into the library: directory scanning, tag reading
//! (via lofty) and cover extraction. Importing the same file again updates
//! its metadata instead of duplicating it.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use lofty::file::{AudioFile as _, TaggedFileExt as _};
use lofty::picture::MimeType;
use lofty::tag::{Accessor as _, ItemKey};
use rusqlite::{OptionalExtension as _, params};

use super::{Library, find_or_create_artist};

/// Extensions the playback engine can decode (rodio/symphonia feature set).
const AUDIO_EXTENSIONS: [&str; 8] = ["mp3", "flac", "ogg", "oga", "wav", "m4a", "mp4", "aac"];

/// Everything known about one audio file, ready to be written to the DB.
#[derive(Debug)]
pub struct TrackImport {
    pub file_path: String,
    pub title: String,
    pub artists: Vec<String>,
    pub featured_artists: Vec<String>,
    pub album_artists: Vec<String>,
    pub release_title: String,
    pub year: Option<i32>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub duration_seconds: f64,
    pub audio_format: Option<String>,
    pub audio_bitrate: Option<i32>,
    pub audio_sample_rate: Option<i32>,
    pub audio_bit_depth: Option<i32>,
    pub file_size_bytes: Option<i64>,
    /// Embedded cover art (bytes, file extension), if any.
    pub cover: Option<(Vec<u8>, &'static str)>,
}

#[derive(Debug, Default)]
pub struct ImportOutcome {
    pub added: usize,
    pub updated: usize,
    pub failed: Vec<(PathBuf, String)>,
}

impl ImportOutcome {
    pub fn summary(&self) -> String {
        let mut message = format!("imported {} track(s)", self.added);
        if self.updated > 0 {
            message.push_str(&format!(", updated {}", self.updated));
        }
        if !self.failed.is_empty() {
            message.push_str(&format!(", {} failed", self.failed.len()));
        }
        message
    }
}

/// Import a file or a directory (recursively). `progress(done, total, name)`
/// is called after every file.
pub fn import_path(
    library: &Library,
    path: &Path,
    mut progress: impl FnMut(usize, usize, &str),
) -> Result<ImportOutcome> {
    let path = path
        .canonicalize()
        .with_context(|| format!("{} does not exist", path.display()))?;
    let mut files = Vec::new();
    collect_audio_files(&path, &mut files);
    anyhow::ensure!(
        !files.is_empty(),
        "no audio files found at {} (supported: {})",
        path.display(),
        AUDIO_EXTENSIONS.join(", ")
    );
    files.sort();

    let total = files.len();
    let mut outcome = ImportOutcome::default();
    for (index, file) in files.iter().enumerate() {
        match read_file(file).and_then(|import| upsert_track(library, &import)) {
            Ok((_, created)) => {
                if created {
                    outcome.added += 1;
                } else {
                    outcome.updated += 1;
                }
            }
            Err(err) => {
                tracing::warn!(file = %file.display(), %err, "import failed");
                outcome.failed.push((file.clone(), format!("{err:#}")));
            }
        }
        let name = file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        progress(index + 1, total, &name);
    }
    Ok(outcome)
}

fn collect_audio_files(path: &Path, files: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            collect_audio_files(&entry.path(), files);
        }
        return;
    }
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());
    if extension.is_some_and(|ext| AUDIO_EXTENSIONS.contains(&ext.as_str())) {
        files.push(path.to_path_buf());
    }
}

/// Read tags and audio properties from one file.
pub fn read_file(path: &Path) -> Result<TrackImport> {
    let tagged = lofty::read_from_path(path).context("cannot read tags")?;
    let properties = tagged.properties();
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());

    let fallback_title = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Unknown".to_string());
    let (mut title, artist_raw, album, year, track_number, disc_number, album_artist_raw, cover) =
        match tag {
            Some(tag) => (
                tag.title()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .unwrap_or(fallback_title),
                tag.artist().map(|value| value.into_owned()),
                tag.album()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                tag.year().and_then(|value| i32::try_from(value).ok()),
                tag.track().and_then(|value| i32::try_from(value).ok()),
                tag.disk().and_then(|value| i32::try_from(value).ok()),
                tag.get_string(&ItemKey::AlbumArtist)
                    .map(|value| value.to_string()),
                tag.pictures().first().map(|picture| {
                    let extension = match picture.mime_type() {
                        Some(MimeType::Png) => "png",
                        Some(MimeType::Gif) => "gif",
                        Some(MimeType::Bmp) => "bmp",
                        _ => "jpg",
                    };
                    (picture.data().to_vec(), extension)
                }),
            ),
            None => (fallback_title, None, None, None, None, None, None, None),
        };

    let (mut artists, mut featured) = split_artist_tag(artist_raw.as_deref().unwrap_or(""));
    // "Song (feat. X)" in the title moves X into the featured list.
    if let Some((clean_title, feat)) = extract_title_feat(&title) {
        title = clean_title;
        for name in feat {
            if !featured.iter().any(|f| f.eq_ignore_ascii_case(&name)) {
                featured.push(name);
            }
        }
    }
    if artists.is_empty() {
        artists.push("Unknown Artist".to_string());
    }
    let album_artists = match album_artist_raw.as_deref().map(split_artist_tag) {
        Some((main, _)) if !main.is_empty() => main,
        _ => artists.clone(),
    };

    let metadata = std::fs::metadata(path).ok();
    Ok(TrackImport {
        file_path: path.to_string_lossy().into_owned(),
        title,
        artists,
        featured_artists: featured,
        album_artists,
        release_title: album.unwrap_or_else(|| "Unknown Album".to_string()),
        year,
        track_number,
        disc_number,
        duration_seconds: properties.duration().as_secs_f64(),
        audio_format: path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase()),
        audio_bitrate: properties
            .audio_bitrate()
            .and_then(|value| i32::try_from(value).ok()),
        audio_sample_rate: properties
            .sample_rate()
            .and_then(|value| i32::try_from(value).ok()),
        audio_bit_depth: properties.bit_depth().map(i32::from),
        file_size_bytes: metadata.map(|meta| meta.len() as i64),
        cover,
    })
}

/// Insert or update one track (matching by file path). Returns the track id
/// and whether a new row was created.
pub fn upsert_track(library: &Library, import: &TrackImport) -> Result<(i64, bool)> {
    let mut conn = library.lock();
    let tx = conn.transaction()?;

    // Release, keyed by (title, first album artist).
    let album_artist_id = find_or_create_artist(
        &tx,
        import
            .album_artists
            .first()
            .map(String::as_str)
            .unwrap_or("Unknown Artist"),
    )?;
    let release_id: Option<i64> = tx
        .query_row(
            "SELECT r.id FROM releases r
             JOIN release_artists ra ON ra.release_id = r.id
             WHERE r.title = ?1 COLLATE NOCASE AND ra.artist_id = ?2",
            params![import.release_title, album_artist_id],
            |row| row.get(0),
        )
        .optional()?;
    let release_id = match release_id {
        Some(id) => {
            // Fill in the year if this file is the first one to know it.
            if import.year.is_some() {
                tx.execute(
                    "UPDATE releases SET year = COALESCE(year, ?2) WHERE id = ?1",
                    params![id, import.year],
                )?;
            }
            id
        }
        None => {
            tx.execute(
                "INSERT INTO releases (title, release_type, year) VALUES (?1, 'album', ?2)",
                params![import.release_title, import.year],
            )?;
            let id = tx.last_insert_rowid();
            for (position, name) in import.album_artists.iter().enumerate() {
                let artist_id = find_or_create_artist(&tx, name)?;
                tx.execute(
                    "INSERT OR IGNORE INTO release_artists (release_id, artist_id, position)
                     VALUES (?1, ?2, ?3)",
                    params![id, artist_id, position as i64],
                )?;
            }
            id
        }
    };

    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM tracks WHERE file_path = ?1",
            [&import.file_path],
            |row| row.get(0),
        )
        .optional()?;
    let (track_id, created) = match existing {
        Some(id) => {
            tx.execute(
                "UPDATE tracks SET title = ?2, track_number = ?3, disc_number = ?4,
                    duration_seconds = ?5, release_id = ?6, audio_format = ?7,
                    audio_bitrate = ?8, audio_sample_rate = ?9, audio_bit_depth = ?10,
                    file_size_bytes = ?11
                 WHERE id = ?1",
                params![
                    id,
                    import.title,
                    import.track_number,
                    import.disc_number,
                    import.duration_seconds,
                    release_id,
                    import.audio_format,
                    import.audio_bitrate,
                    import.audio_sample_rate,
                    import.audio_bit_depth,
                    import.file_size_bytes,
                ],
            )?;
            tx.execute("DELETE FROM track_artists WHERE track_id = ?1", [id])?;
            (id, false)
        }
        None => {
            tx.execute(
                "INSERT INTO tracks (title, track_number, disc_number, duration_seconds,
                    release_id, file_path, audio_format, audio_bitrate, audio_sample_rate,
                    audio_bit_depth, file_size_bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    import.title,
                    import.track_number,
                    import.disc_number,
                    import.duration_seconds,
                    release_id,
                    import.file_path,
                    import.audio_format,
                    import.audio_bitrate,
                    import.audio_sample_rate,
                    import.audio_bit_depth,
                    import.file_size_bytes,
                ],
            )?;
            (tx.last_insert_rowid(), true)
        }
    };
    for (position, name) in import.artists.iter().enumerate() {
        let artist_id = find_or_create_artist(&tx, name)?;
        tx.execute(
            "INSERT OR IGNORE INTO track_artists (track_id, artist_id, role, position)
             VALUES (?1, ?2, 'main', ?3)",
            params![track_id, artist_id, position as i64],
        )?;
    }
    for (position, name) in import.featured_artists.iter().enumerate() {
        let artist_id = find_or_create_artist(&tx, name)?;
        tx.execute(
            "INSERT OR IGNORE INTO track_artists (track_id, artist_id, role, position)
             VALUES (?1, ?2, 'featured', ?3)",
            params![track_id, artist_id, position as i64],
        )?;
    }

    // Cover: a release keeps the first cover found — an image file next to
    // the audio, or the embedded picture saved into the covers directory.
    let has_cover: bool = tx
        .query_row(
            "SELECT cover_path IS NOT NULL FROM releases WHERE id = ?1",
            [release_id],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !has_cover
        && let Some(cover_path) = resolve_cover(library, release_id, import)
    {
        tx.execute(
            "UPDATE releases SET cover_path = ?2 WHERE id = ?1",
            params![release_id, cover_path],
        )?;
    }

    tx.commit()?;
    Ok((track_id, created))
}

/// Find a cover image for the release: a cover/folder/front image in the
/// audio file's directory, or the embedded picture written to disk.
fn resolve_cover(library: &Library, release_id: i64, import: &TrackImport) -> Option<String> {
    let directory = Path::new(&import.file_path).parent()?;
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.flatten() {
            let path = entry.path();
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(|stem| stem.to_ascii_lowercase());
            let extension = path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase());
            let is_image = matches!(
                extension.as_deref(),
                Some("jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif")
            );
            if is_image
                && matches!(stem.as_deref(), Some("cover" | "folder" | "front" | "album"))
            {
                return Some(path.to_string_lossy().into_owned());
            }
        }
    }
    let (data, extension) = import.cover.as_ref()?;
    let covers_dir = library.covers_dir();
    if let Err(err) = std::fs::create_dir_all(covers_dir) {
        tracing::warn!(%err, "cannot create covers directory");
        return None;
    }
    let path = covers_dir.join(format!("release_{release_id}.{extension}"));
    match std::fs::write(&path, data) {
        Ok(()) => Some(path.to_string_lossy().into_owned()),
        Err(err) => {
            tracing::warn!(%err, path = %path.display(), "cannot save embedded cover");
            None
        }
    }
}

/// Split an artist tag into (main artists, featured artists).
/// Separators: ";" and "/" between main artists; "feat."/"ft."/"featuring"
/// starts the featured list.
pub fn split_artist_tag(raw: &str) -> (Vec<String>, Vec<String>) {
    let raw = raw.trim();
    if raw.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let (main_part, feat_part) = match find_feat_marker(raw) {
        Some((at, marker_len)) => {
            let main = raw[..at].trim_end_matches(['(', '[', ' ', ',', '-']);
            let feat = raw[at + marker_len..].trim_end_matches([')', ']']);
            (main, feat)
        }
        None => (raw, ""),
    };
    (split_names(main_part), split_names(feat_part))
}

/// The earliest "feat."/"ft."/"featuring" marker that stands as its own
/// word — preceded by a separator and followed by a space — so artist names
/// like "Daft Punk" are not split on the "ft" inside them.
fn find_feat_marker(raw: &str) -> Option<(usize, usize)> {
    let lowered = raw.to_lowercase();
    let mut best: Option<(usize, usize)> = None;
    for marker in ["featuring", "feat.", "feat", "ft.", "ft"] {
        for (at, _) in lowered.match_indices(marker) {
            let before_ok = raw[..at]
                .chars()
                .next_back()
                .is_some_and(|c| matches!(c, ' ' | '(' | '[' | ',' | '-'));
            let after_ok = raw[at + marker.len()..].starts_with(' ');
            if before_ok && after_ok && best.is_none_or(|(current, _)| at < current) {
                best = Some((at, marker.len()));
            }
        }
    }
    best
}

fn split_names(raw: &str) -> Vec<String> {
    raw.split([';', '/'])
        .flat_map(|part| part.split(" & "))
        .map(|name| name.trim().trim_matches(',').trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Extract "(feat. X)" / "[ft. Y]" from a track title.
fn extract_title_feat(title: &str) -> Option<(String, Vec<String>)> {
    let lowered = title.to_lowercase();
    for marker in ["(feat.", "(feat ", "(ft.", "[feat.", "[ft."] {
        if let Some(start) = lowered.find(marker) {
            let closer = if marker.starts_with('(') { ')' } else { ']' };
            let rest = &title[start + marker.len()..];
            let end = rest.find(closer)?;
            let names = split_names(&rest[..end]);
            if names.is_empty() {
                return None;
            }
            let mut clean = title[..start].trim_end().to_string();
            clean.push_str(rest[end + 1..].trim_end());
            return Some((clean.trim().to_string(), names));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid WAV file: 0.1s of silence at 8kHz mono 16-bit.
    fn write_test_wav(path: &Path) {
        let samples: u32 = 800;
        let data_len = samples * 2;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
        bytes.extend_from_slice(&8000u32.to_le_bytes()); // sample rate
        bytes.extend_from_slice(&16000u32.to_le_bytes()); // byte rate
        bytes.extend_from_slice(&2u16.to_le_bytes()); // block align
        bytes.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.resize(bytes.len() + data_len as usize, 0);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn imports_a_real_audio_file_end_to_end() {
        let dir = std::env::temp_dir().join(format!("furumi-import-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("My Song.wav");
        write_test_wav(&wav);

        let db = dir.join("library.db");
        let library = Library::open(&db).unwrap();
        let outcome = import_path(&library, &dir, |_, _, _| {}).unwrap();
        assert_eq!(outcome.added, 1);
        assert!(outcome.failed.is_empty());

        // Untagged files fall back to the file name and placeholder names.
        let results = library.search("My Song", 10).unwrap();
        assert_eq!(results.tracks.len(), 1);
        let track = &results.tracks[0];
        assert_eq!(track.title, "My Song");
        assert_eq!(track.artists[0].name, "Unknown Artist");
        assert_eq!(track.release_title, "Unknown Album");
        assert!(track.duration_seconds > 0.05);
        assert_eq!(track.audio_sample_rate, Some(8000));
        assert!(std::fs::File::open(&track.file_path).is_ok());

        // Re-importing the same directory only updates.
        let outcome = import_path(&library, &dir, |_, _, _| {}).unwrap();
        assert_eq!((outcome.added, outcome.updated), (0, 1));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn splits_plain_artist() {
        let (main, feat) = split_artist_tag("Daft Punk");
        assert_eq!(main, vec!["Daft Punk"]);
        assert!(feat.is_empty());
    }

    #[test]
    fn splits_multiple_and_featured() {
        let (main, feat) = split_artist_tag("A; B feat. C & D");
        assert_eq!(main, vec!["A", "B"]);
        assert_eq!(feat, vec!["C", "D"]);
    }

    #[test]
    fn keeps_commas_inside_names() {
        let (main, _) = split_artist_tag("Tyler, The Creator");
        assert_eq!(main, vec!["Tyler, The Creator"]);
    }

    #[test]
    fn extracts_feat_from_title() {
        let (title, names) = extract_title_feat("Song (feat. X & Y)").unwrap();
        assert_eq!(title, "Song");
        assert_eq!(names, vec!["X", "Y"]);
        assert!(extract_title_feat("Plain Song").is_none());
    }
}
