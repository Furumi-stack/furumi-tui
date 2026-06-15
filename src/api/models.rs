use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub role: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokensResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in_seconds: i64,
}

#[derive(Debug, Deserialize)]
pub struct LoginResponse {
    pub user: User,
    pub tokens: TokensResponse,
}

#[derive(Debug, Deserialize)]
#[allow(
    dead_code,
    reason = "rendered by the profile view in a later milestone"
)]
pub struct MeStats {
    pub liked_tracks: i64,
    pub playlists: i64,
    pub plays: i64,
    pub listened_minutes: i64,
}

#[derive(Debug, Deserialize)]
#[allow(
    dead_code,
    reason = "rendered by the profile view in a later milestone"
)]
pub struct MeResponse {
    pub id: i64,
    pub name: String,
    pub role: String,
    pub stats: MeStats,
}

#[derive(Debug, Deserialize)]
pub struct ApiErrorBody {
    pub error: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArtistCard {
    #[allow(dead_code, reason = "opens the artist view in the next milestone")]
    pub id: i64,
    pub name: String,
    /// Relative path like `/api/player/cover/{file_id}/medium`.
    pub image_url: Option<String>,
    pub release_count: i64,
    pub track_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistRef {
    #[allow(dead_code, reason = "navigation to artists from track rows later")]
    pub id: i64,
    pub name: String,
}

/// Serialize keeps every field the backend sent us, so device-sync payloads
/// (play_from_index, queue_add) carry full track objects like the web does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackItem {
    #[allow(dead_code, reason = "playback engine consumes this in milestone 3")]
    pub id: i64,
    pub title: String,
    /// Absent in the artist-appearance variant of track payloads.
    #[serde(default)]
    pub track_number: Option<i32>,
    #[serde(default)]
    pub disc_number: Option<i32>,
    pub duration_seconds: f64,
    #[serde(default)]
    pub artists: Vec<ArtistRef>,
    #[serde(default)]
    pub featured_artists: Vec<ArtistRef>,
    #[allow(dead_code, reason = "jump-to-release navigation later")]
    #[serde(default)]
    pub release_id: i64,
    #[serde(default)]
    pub release_title: String,
    #[allow(dead_code, reason = "shown in queue/now-playing later")]
    pub release_year: Option<i32>,
    /// Server-relative path to `/api/player/stream/{id}`.
    #[serde(default)]
    pub stream_url: String,
    #[allow(dead_code, reason = "now-playing artwork in milestone 3")]
    pub cover_url: Option<String>,
    #[serde(default)]
    pub uploader_name: String,
    pub audio_format: Option<String>,
    pub audio_bitrate: Option<i32>,
    pub audio_sample_rate: Option<i32>,
    pub audio_bit_depth: Option<i32>,
    pub file_size_bytes: Option<i64>,
    pub lastfm_listeners: Option<i64>,
    #[allow(dead_code, reason = "popularity column later")]
    pub lastfm_playcount: Option<i64>,
    pub lastfm_rating: Option<f64>,
    pub lastfm_updated_at: Option<String>,
}

impl TrackItem {
    pub fn artist_line(&self) -> String {
        let mut names: Vec<&str> = self.artists.iter().map(|a| a.name.as_str()).collect();
        if !self.featured_artists.is_empty() {
            names.push("feat.");
            names.extend(self.featured_artists.iter().map(|a| a.name.as_str()));
        }
        names.join(", ")
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

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseCard {
    pub id: i64,
    pub title: String,
    pub release_type: String,
    pub year: Option<i32>,
    pub cover_url: Option<String>,
    pub track_count: i64,
}

#[derive(Debug, Deserialize)]
pub struct ArtistDetail {
    #[allow(dead_code, reason = "cache key is held by the caller")]
    pub id: i64,
    pub name: String,
    pub image_url: Option<String>,
    pub total_track_count: i64,
    pub total_play_count: i64,
    pub top_tracks: Vec<TrackItem>,
    pub releases: Vec<ReleaseCard>,
    /// Tracks where this artist is featured (the only content for artists
    /// without own releases).
    #[serde(default)]
    pub featured_tracks: Vec<TrackItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UploaderSummary {
    pub name: String,
    #[allow(dead_code, reason = "per-uploader stats for a later detail popup")]
    pub track_count: i64,
}

#[derive(Debug, Deserialize)]
pub struct ReleaseDetail {
    #[allow(dead_code, reason = "cache key is held by the caller")]
    pub id: i64,
    pub title: String,
    pub release_type: String,
    pub year: Option<i32>,
    pub cover_url: Option<String>,
    pub artists: Vec<ArtistRef>,
    pub tracks: Vec<TrackItem>,
    pub uploaders: Vec<UploaderSummary>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlaylistCard {
    pub id: i64,
    pub title: String,
    pub track_count: i64,
    pub is_own: bool,
    pub owner_name: Option<String>,
    pub is_public: bool,
    #[allow(dead_code, reason = "save/unsave playlists later")]
    pub is_saved: bool,
    #[allow(dead_code, reason = "playlist kinds get distinct icons later")]
    pub kind: String,
}

#[derive(Debug, Deserialize)]
pub struct PlaylistDetail {
    #[allow(dead_code, reason = "cache key is held by the caller")]
    pub id: i64,
    pub title: String,
    #[allow(dead_code, reason = "shown in a detail header later")]
    pub description: Option<String>,
    pub tracks: Vec<TrackItem>,
}

#[derive(Debug, Deserialize)]
pub struct LikesResponse {
    pub track_ids: Vec<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceDto {
    pub id: String,
    pub name: String,
    pub kind: String,
    #[allow(dead_code, reason = "server-side flag; we compare ids directly")]
    pub is_current: bool,
    pub is_active: bool,
    #[allow(dead_code, reason = "freshness display later")]
    pub last_seen_ms: i64,
}

#[derive(Debug, Deserialize)]
pub struct DeviceCommandDto {
    #[allow(dead_code, reason = "commands are applied in poll order")]
    pub id: Option<String>,
    pub command: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Mirrors the backend's PlayerDevicePlaybackStateDto; tracks stay raw JSON
/// so unknown fields survive the round trip between clients.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DevicePlaybackState {
    #[serde(default)]
    pub track: Option<serde_json::Value>,
    #[serde(default)]
    pub tracks: Vec<serde_json::Value>,
    #[serde(default)]
    pub index: i32,
    #[serde(default)]
    pub position_seconds: f64,
    #[serde(default)]
    pub duration_seconds: f64,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub shuffle: bool,
    #[serde(default = "default_repeat_mode")]
    pub repeat_mode: String,
    #[serde(default = "default_volume")]
    pub volume: f64,
    #[serde(default)]
    pub updated_at_ms: i64,
}

fn default_repeat_mode() -> String {
    "off".to_string()
}

fn default_volume() -> f64 {
    1.0
}

#[derive(Debug, Deserialize)]
pub struct DevicePollResponse {
    #[allow(dead_code, reason = "echo of our own id")]
    pub device_id: String,
    pub active_device_id: Option<String>,
    #[serde(default)]
    pub devices: Vec<DeviceDto>,
    #[serde(default)]
    pub commands: Vec<DeviceCommandDto>,
    #[serde(default)]
    #[allow(dead_code, reason = "Jam control is out of scope for the TUI v1")]
    pub current_jam_id: Option<String>,
    pub playback_state: Option<DevicePlaybackState>,
}

#[derive(Debug, Default, Deserialize)]
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

#[derive(Debug, Deserialize)]
pub struct ArtistsPage {
    pub items: Vec<ArtistCard>,
    pub total: i64,
    pub page: i64,
    #[allow(dead_code, reason = "part of the server pagination envelope")]
    pub per_page: i64,
    pub has_more: bool,
}
