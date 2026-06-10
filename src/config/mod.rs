pub mod keymap;
pub mod logging;

use directories::ProjectDirs;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "furumi")
}

pub fn device_id_path() -> Option<PathBuf> {
    project_dirs().map(|dirs| dirs.config_dir().join("device_id"))
}

pub fn load_or_create_device_id() -> String {
    if let Some(path) = device_id_path() {
        if let Ok(raw) = fs::read_to_string(&path) {
            let id = raw.trim();
            if valid_device_id(id) {
                return id.to_string();
            }
        }

        let id = generate_device_id();
        if let Some(parent) = path.parent() {
            if let Err(err) = fs::create_dir_all(parent) {
                tracing::warn!(path = %parent.display(), %err, "failed to create config directory");
                return id;
            }
        }
        if let Err(err) = fs::write(&path, &id) {
            tracing::warn!(path = %path.display(), %err, "failed to persist device id");
        }
        return id;
    }
    generate_device_id()
}

fn valid_device_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn generate_device_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("tui-{nanos:x}-{:x}", std::process::id())
}
