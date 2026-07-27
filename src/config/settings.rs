use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibrarySourceMode {
    #[default]
    Local,
    My,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "default_volume")]
    pub volume: u8,
    #[serde(default)]
    pub library: LibraryFilters,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            volume: default_volume(),
            library: LibraryFilters::default(),
        }
    }
}

impl AppSettings {
    pub fn normalized(mut self) -> Self {
        self.volume = self.volume.min(100);
        self
    }
}

fn default_volume() -> u8 {
    80
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
        assert_eq!(settings.library.source_mode, LibrarySourceMode::Local);
    }
}
