pub mod keymap;
pub mod logging;
pub mod settings;

use directories::ProjectDirs;

pub fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "furumi")
}
