//! Data shapes the views render. They mirror what the furumusic API used to
//! return, but every field is now filled from the local SQLite library.

#[derive(Debug, Clone)]
pub struct ArtistCard {
    pub id: i64,
    pub name: String,
    /// Path to a local image file, if one is set for the artist.
    pub image_path: Option<String>,
    pub release_count: i64,
    pub track_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistRef {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct TrackItem {
    pub id: i64,
    pub title: String,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub duration_seconds: f64,
    pub artists: Vec<ArtistRef>,
    pub featured_artists: Vec<ArtistRef>,
    pub release_id: i64,
    pub release_title: String,
    pub release_year: Option<i32>,
    /// Absolute path to the local audio file.
    pub file_path: String,
    /// Stable audio content id (`b3:<64 hex>`) when known.
    pub content_id: Option<String>,
    /// Path to a local cover image (the release cover).
    pub cover_path: Option<String>,
    pub audio_format: Option<String>,
    pub audio_bitrate: Option<i32>,
    pub audio_sample_rate: Option<i32>,
    pub audio_bit_depth: Option<i32>,
    pub file_size_bytes: Option<i64>,
    /// Completed local plays, from the history table.
    pub play_count: i64,
    /// Set for federated tracks that are not in the local library (yet):
    /// carries everything needed to download them from the owning peer.
    /// With an empty `file_path` the player resolves the track on demand.
    pub fed: Option<crate::federation::FedTrack>,
}

impl TrackItem {
    /// A federated track that still needs downloading before playback.
    pub fn is_fed_pending(&self) -> bool {
        self.fed.is_some() && self.file_path.is_empty()
    }

    pub fn artist_line(&self) -> String {
        let artists = self
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let featured = self
            .featured_artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        match (artists.is_empty(), featured.is_empty()) {
            (false, false) => format!("{artists} feat. {featured}"),
            (false, true) => artists,
            (true, false) => format!("feat. {featured}"),
            (true, true) => String::new(),
        }
    }

    pub fn duration_label(&self) -> String {
        let total = self.duration_seconds.round() as i64;
        format!("{}:{:02}", total / 60, total % 60)
    }

    /// Full tech line for the status bar, including the sample rate.
    pub fn tech_label_full(&self) -> String {
        let mut parts = Vec::new();
        if let Some(format) = &self.audio_format {
            parts.push(format.to_uppercase());
        }
        if let Some(bitrate) = self.audio_bitrate {
            parts.push(format!("{bitrate}kbps"));
        }
        if let Some(rate) = self.audio_sample_rate {
            parts.push(format!("{:.1}kHz", f64::from(rate) / 1000.0));
        }
        if let Some(bytes) = self.file_size_bytes {
            parts.push(format!("{:.1}MB", bytes as f64 / 1_048_576.0));
        }
        parts.join(" · ")
    }
}

#[derive(Debug, Clone)]
pub struct ReleaseCard {
    pub id: i64,
    pub title: String,
    pub release_type: String,
    pub year: Option<i32>,
    pub cover_path: Option<String>,
    pub track_count: i64,
}

#[derive(Debug)]
pub struct ArtistDetail {
    #[allow(dead_code, reason = "cache key is held by the caller")]
    pub id: i64,
    pub name: String,
    pub image_path: Option<String>,
    pub total_track_count: i64,
    pub total_play_count: i64,
    pub top_tracks: Vec<TrackItem>,
    pub releases: Vec<ReleaseCard>,
    /// Tracks where this artist is featured (the only content for artists
    /// without own releases).
    pub featured_tracks: Vec<TrackItem>,
}

#[derive(Debug)]
pub struct ReleaseDetail {
    #[allow(dead_code, reason = "cache key is held by the caller")]
    pub id: i64,
    pub title: String,
    pub release_type: String,
    pub year: Option<i32>,
    pub cover_path: Option<String>,
    pub artists: Vec<ArtistRef>,
    pub tracks: Vec<TrackItem>,
}

#[derive(Debug, Clone)]
pub struct PlaylistCard {
    pub id: i64,
    pub title: String,
    pub track_count: i64,
    /// "normal" for user playlists, "likes" for the virtual Likes playlist.
    pub kind: String,
}

#[derive(Debug)]
pub struct PlaylistDetail {
    #[allow(dead_code, reason = "cache key is held by the caller")]
    pub id: i64,
    pub title: String,
    #[allow(dead_code, reason = "shown in a detail header later")]
    pub description: Option<String>,
    pub tracks: Vec<TrackItem>,
}

#[derive(Debug, Default)]
pub struct SearchResults {
    pub artists: Vec<ArtistCard>,
    pub releases: Vec<ReleaseCard>,
    pub tracks: Vec<TrackItem>,
}

impl SearchResults {
    pub fn len(&self) -> usize {
        self.artists.len() + self.releases.len() + self.tracks.len()
    }
}

#[derive(Debug)]
pub struct ArtistsPage {
    pub items: Vec<ArtistCard>,
    pub total: i64,
    pub page: i64,
    pub has_more: bool,
}

/// Edited values submitted from the track edit form. `None` numbers clear
/// the column.
#[derive(Debug, Clone)]
pub struct TrackEdit {
    pub title: String,
    pub artists: Vec<String>,
    pub featured_artists: Vec<String>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    /// Cover image path; the cover lives on the track's release (the same
    /// image every view shows for the track). None clears it.
    pub cover_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReleaseEdit {
    pub title: String,
    pub release_type: String,
    pub year: Option<i32>,
    pub artists: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artist(name: &str) -> ArtistRef {
        ArtistRef {
            id: 1,
            name: name.to_string(),
        }
    }

    #[test]
    fn artist_line_formats_featured_artists() {
        let track = TrackItem {
            id: 1,
            title: "Track".into(),
            track_number: None,
            disc_number: None,
            duration_seconds: 1.0,
            artists: vec![artist("Main")],
            featured_artists: vec![artist("Guest"), artist("Other")],
            release_id: 1,
            release_title: "Release".into(),
            release_year: None,
            file_path: "/tmp/track.mp3".into(),
            content_id: None,
            cover_path: None,
            audio_format: None,
            audio_bitrate: None,
            audio_sample_rate: None,
            audio_bit_depth: None,
            file_size_bytes: None,
            play_count: 0,
            fed: None,
        };

        assert_eq!(track.artist_line(), "Main feat. Guest, Other");
    }
}
