use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibrarySourceMode {
    Local,
    My,
    #[default]
    Global,
}

impl LibrarySourceMode {
    pub fn label(self) -> &'static str {
        match self {
            LibrarySourceMode::Local => "Local",
            LibrarySourceMode::My => "My",
            LibrarySourceMode::Global => "Global",
        }
    }

    pub fn includes_network(self) -> bool {
        !matches!(self, LibrarySourceMode::Local)
    }

    pub fn includes_global_peers(self) -> bool {
        matches!(self, LibrarySourceMode::Global)
    }

    pub fn next(self) -> Self {
        match self {
            LibrarySourceMode::Local => LibrarySourceMode::My,
            LibrarySourceMode::My => LibrarySourceMode::Global,
            LibrarySourceMode::Global => LibrarySourceMode::Local,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryFilters {
    #[serde(default)]
    pub hide_featured_only: bool,
    #[serde(default)]
    pub source_mode: LibrarySourceMode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimilaritySettings {
    /// Local embedding/search master switch. Network participation follows
    /// federation and additionally requires the explicit privacy consent.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_similarity_model")]
    pub model: String,
    #[serde(default = "default_similarity_profile")]
    pub profile: String,
    #[serde(default = "default_similarity_workers")]
    pub workers: usize,
    /// Requester-side cosine score floor. This is search policy, not part of
    /// the embedding profile, so changing it never invalidates vectors.
    #[serde(default = "default_similarity_minimum_score")]
    pub minimum_score: f32,
    /// Requester-side diversity cap applied independently to local and
    /// federated candidates.
    #[serde(default = "default_similarity_max_tracks_per_artist")]
    pub max_tracks_per_artist: usize,
    #[serde(default)]
    pub federation_consent: bool,
    /// Exact fingerprint of the last fully usable profile. Keeping this
    /// separate from the selected target lets an old index serve searches
    /// while a newly selected model/profile is being calculated.
    #[serde(default)]
    pub active_profile: Option<String>,
}

impl Default for SimilaritySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            model: default_similarity_model(),
            profile: default_similarity_profile(),
            workers: default_similarity_workers(),
            minimum_score: default_similarity_minimum_score(),
            max_tracks_per_artist: default_similarity_max_tracks_per_artist(),
            federation_consent: false,
            active_profile: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "default_volume")]
    pub volume: u8,
    #[serde(default)]
    pub library: LibraryFilters,
    /// Root used for music materialized from federation peers.
    #[serde(default = "default_music_dir")]
    pub music_dir: PathBuf,
    #[serde(default)]
    pub similarity: SimilaritySettings,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            volume: default_volume(),
            library: LibraryFilters::default(),
            music_dir: default_music_dir(),
            similarity: SimilaritySettings::default(),
        }
    }
}

impl AppSettings {
    pub fn normalized(mut self) -> Self {
        self.volume = self.volume.min(100);
        if self.music_dir.as_os_str().is_empty() {
            self.music_dir = default_music_dir();
        }
        if self.similarity.model.trim().is_empty() {
            self.similarity.model = default_similarity_model();
        }
        if self.similarity.profile.trim().is_empty() {
            self.similarity.profile = default_similarity_profile();
        }
        self.similarity.workers = self.similarity.workers.clamp(1, 16);
        if !self.similarity.minimum_score.is_finite() {
            self.similarity.minimum_score = default_similarity_minimum_score();
        }
        self.similarity.minimum_score = self.similarity.minimum_score.clamp(0.0, 1.0);
        self.similarity.max_tracks_per_artist = self.similarity.max_tracks_per_artist.clamp(1, 50);
        self
    }
}

fn default_volume() -> u8 {
    80
}

fn default_similarity_model() -> String {
    "discogs-effnet-bsdynamic-1".to_string()
}

fn default_similarity_profile() -> String {
    "furumi-full-track-v1".to_string()
}

fn default_similarity_workers() -> usize {
    std::thread::available_parallelism()
        .map(|count| (count.get() / 2).clamp(1, 4))
        .unwrap_or(1)
}

fn default_similarity_minimum_score() -> f32 {
    0.70
}

fn default_similarity_max_tracks_per_artist() -> usize {
    5
}

/// The historical permanent-download location, kept as the default for
/// backward compatibility with existing installations.
pub fn default_music_dir() -> PathBuf {
    crate::config::project_dirs()
        .map(|dirs| dirs.data_dir().join("federation-media"))
        .unwrap_or_else(|| PathBuf::from("federation-media"))
}

pub fn load() -> (AppSettings, Option<String>) {
    let Some(path) = settings_path() else {
        return (AppSettings::default(), None);
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<AppSettings>(&text) {
            Ok(settings) => (settings.normalized(), None),
            Err(err) => (
                AppSettings::default(),
                Some(format!("settings.toml is malformed: {err}")),
            ),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (AppSettings::default(), None),
        Err(err) => (
            AppSettings::default(),
            Some(format!("settings.toml could not be read: {err}")),
        ),
    }
}

pub fn save(settings: &AppSettings) -> Result<()> {
    let path = settings_path().context("cannot determine the config directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &path,
        toml::to_string_pretty(&settings.clone().normalized())?,
    )
    .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn settings_path() -> Option<std::path::PathBuf> {
    crate::config::project_dirs().map(|dirs| dirs.config_dir().join("settings.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_and_normalize() {
        let settings: AppSettings = toml::from_str(
            r#"
volume = 150

[library]
hide_featured_only = true
"#,
        )
        .unwrap();
        let settings = settings.normalized();

        assert_eq!(settings.volume, 100);
        assert!(settings.library.hide_featured_only);
        assert_eq!(settings.library.source_mode, LibrarySourceMode::Global);
        assert_eq!(settings.music_dir, default_music_dir());
        assert_eq!(settings.similarity.minimum_score, 0.70);
        assert_eq!(settings.similarity.max_tracks_per_artist, 5);
    }

    #[test]
    fn similarity_search_policy_is_normalized_without_changing_the_profile() {
        let mut settings = AppSettings::default();
        let profile = settings.similarity.profile.clone();
        settings.similarity.minimum_score = f32::NAN;
        settings.similarity.max_tracks_per_artist = 0;

        let settings = settings.normalized();

        assert_eq!(settings.similarity.minimum_score, 0.70);
        assert_eq!(settings.similarity.max_tracks_per_artist, 1);
        assert_eq!(settings.similarity.profile, profile);
    }
}
