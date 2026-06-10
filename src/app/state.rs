use std::collections::HashMap;
use std::sync::Arc;

use crate::api::models::{
    ArtistCard, ArtistDetail, PlaylistCard, PlaylistDetail, ReleaseCard, ReleaseDetail,
    SearchResults, TrackItem, User,
};
use crate::art::ArtImage;
use crate::config::keymap::KeyContext;

/// Remote data that a view renders: spinner, content, or error.
#[derive(Debug)]
pub enum Loadable<T> {
    Loading,
    Ready(T),
    Failed(String),
}

/// Tile geometry for the Global artist grid (kept here so selection math in
/// update() and rendering in ui::global agree). Width × height in cells,
/// including the tile border; the art area inside is 18×8 cells = 18×16 px.
pub const TILE_WIDTH: u16 = 20;
pub const TILE_HEIGHT: u16 = 12;
pub const ART_CELL_WIDTH: u16 = 18;
pub const ART_CELL_HEIGHT: u16 = 8;
/// Header artwork (artist page, release page): 24×12 cells = 24×24 px.
pub const ART_HEADER_WIDTH: u16 = 24;
pub const ART_HEADER_HEIGHT: u16 = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Tiles,
    Table,
}

impl ViewMode {
    pub fn toggle(self) -> ViewMode {
        match self {
            ViewMode::Tiles => ViewMode::Table,
            ViewMode::Table => ViewMode::Tiles,
        }
    }
}

/// Artist image in the shared art cache.
#[derive(Debug, Clone)]
pub enum ArtState {
    Loading,
    Ready(Arc<ArtImage>),
    Failed,
}

/// A drill-down view pushed on top of the Global artist grid. Cursors live
/// in the stack entry so going Back restores the previous position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalView {
    /// Linear cursor over top tracks (0..tracks) then releases in display
    /// order (tracks..tracks+releases).
    Artist { id: i64, cursor: usize },
    Release { id: i64, cursor: usize },
    /// Linear cursor over search results: artists, then releases, then tracks.
    Search { cursor: usize },
}

/// The Global tab: the whole server library of artists.
#[derive(Debug)]
pub struct GlobalTab {
    pub artists: Vec<ArtistCard>,
    pub total: i64,
    pub has_more: bool,
    pub next_page: i64,
    pub loading: bool,
    pub error: Option<String>,
    pub selected: usize,
    pub view: ViewMode,
    pub stack: Vec<GlobalView>,
}

impl Default for GlobalTab {
    fn default() -> Self {
        Self {
            artists: Vec::new(),
            total: 0,
            has_more: true,
            next_page: 1,
            loading: false,
            error: None,
            selected: 0,
            view: ViewMode::default(),
            stack: Vec::new(),
        }
    }
}

/// Releases of an artist in display order: grouped by type (albums, EPs,
/// singles, compilations, then anything else), keeping server order within a
/// group. Returns (group label, indices into the original slice). Cursor
/// positions use this flattened order, so update() and ui must both go
/// through here.
pub fn release_groups(releases: &[ReleaseCard]) -> Vec<(&'static str, Vec<usize>)> {
    const GROUPS: [(&str, &str); 4] = [
        ("album", "Albums"),
        ("ep", "EPs"),
        ("single", "Singles"),
        ("compilation", "Compilations"),
    ];
    let mut groups: Vec<(&'static str, Vec<usize>)> = Vec::new();
    for (kind, label) in GROUPS {
        let indices: Vec<usize> = releases
            .iter()
            .enumerate()
            .filter(|(_, r)| r.release_type.eq_ignore_ascii_case(kind))
            .map(|(i, _)| i)
            .collect();
        if !indices.is_empty() {
            groups.push((label, indices));
        }
    }
    let known: Vec<usize> = groups.iter().flat_map(|(_, v)| v.iter().copied()).collect();
    let other: Vec<usize> = (0..releases.len()).filter(|i| !known.contains(i)).collect();
    if !other.is_empty() {
        groups.push(("Other", other));
    }
    groups
}

/// Flattened display order of releases (concatenated groups).
pub fn release_display_order(releases: &[ReleaseCard]) -> Vec<usize> {
    release_groups(releases)
        .into_iter()
        .flat_map(|(_, indices)| indices)
        .collect()
}

/// Visual tile-grid rows of the releases section: each group starts its own
/// rows, chunked by the column count. Values are display-order positions.
/// Vertical cursor movement must follow these rows to match the rendering.
pub fn release_rows(releases: &[ReleaseCard], columns: usize) -> Vec<Vec<usize>> {
    let columns = columns.max(1);
    let mut rows = Vec::new();
    let mut position = 0;
    for (_, group) in release_groups(releases) {
        for chunk in group.chunks(columns) {
            rows.push((position..position + chunk.len()).collect());
            position += chunk.len();
        }
    }
    rows
}

/// The virtual server-side Likes playlist id (`kind == "likes"`).
pub const LIKES_PLAYLIST_ID: i64 = -1;

#[derive(Debug, Clone, Copy)]
pub struct OpenedPlaylist {
    pub id: i64,
    pub cursor: usize,
}

/// The Playlists tab. The server list includes the virtual "Likes"
/// playlist (id = -1), rendered with a ♥ marker.
#[derive(Debug, Default)]
pub struct PlaylistsTab {
    pub list: Option<Loadable<Vec<PlaylistCard>>>,
    pub selected: usize,
    pub opened: Option<OpenedPlaylist>,
}

/// Severity steps for the Logs tab filter, cycled with the view-toggle key.
pub const LOG_LEVELS: [tracing::Level; 5] = [
    tracing::Level::ERROR,
    tracing::Level::WARN,
    tracing::Level::INFO,
    tracing::Level::DEBUG,
    tracing::Level::TRACE,
];

/// The Logs tab: a live view over the in-memory ring buffer.
#[derive(Debug)]
pub struct LogsTab {
    /// Index into LOG_LEVELS; entries more verbose than this are hidden.
    pub level_index: usize,
    /// Stick to the newest entries as they arrive.
    pub follow: bool,
    /// When not following: how many (filtered) entries back from the end.
    pub scroll_from_end: usize,
}

impl Default for LogsTab {
    fn default() -> Self {
        Self {
            level_index: 2,
            follow: true,
            scroll_from_end: 0,
        }
    }
}

/// Command line (`:`), vim-style. Lives on the Main screen status bar.
#[derive(Debug, Default)]
pub struct Cmdline {
    pub active: bool,
    pub input: String,
    /// A live command (search) applied effects during this session; Esc
    /// undoes them, Enter keeps them.
    pub live: bool,
}

/// Live search state driven by the `:/query` command.
#[derive(Debug, Default)]
pub struct SearchState {
    pub query: String,
    pub loading: bool,
    pub results: Option<SearchResults>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    #[default]
    Main,
    Login,
}

/// SSO is the primary sign-in path, so it sits right under the server URL;
/// the password fields below are the rare fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoginField {
    #[default]
    ServerUrl,
    SsoButton,
    Username,
    Password,
    SignInButton,
}

impl LoginField {
    const ORDER: [LoginField; 5] = [
        LoginField::ServerUrl,
        LoginField::SsoButton,
        LoginField::Username,
        LoginField::Password,
        LoginField::SignInButton,
    ];

    pub fn next(self) -> LoginField {
        let i = Self::ORDER.iter().position(|f| *f == self).unwrap();
        Self::ORDER[(i + 1) % Self::ORDER.len()]
    }

    pub fn prev(self) -> LoginField {
        let i = Self::ORDER.iter().position(|f| *f == self).unwrap();
        Self::ORDER[(i + Self::ORDER.len() - 1) % Self::ORDER.len()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoginMode {
    /// Server / username / password fields plus the SSO button.
    #[default]
    Form,
    /// Browser SSO started; waiting for the pasted callback link or code.
    SsoPending,
}

#[derive(Debug)]
pub struct LoginForm {
    pub server_url: String,
    pub username: String,
    pub password: String,
    pub sso_paste: String,
    pub sso_url: String,
    pub sso_port: Option<u16>,
    pub focus: LoginField,
    pub mode: LoginMode,
    pub busy: bool,
    pub error: Option<String>,
}

impl Default for LoginForm {
    fn default() -> Self {
        Self {
            server_url: "https://music.hexor.cy".to_string(),
            username: String::new(),
            password: String::new(),
            sso_paste: String::new(),
            sso_url: String::new(),
            sso_port: None,
            focus: LoginField::default(),
            mode: LoginMode::default(),
            busy: false,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Global,
    Playlists,
    Queue,
    Devices,
    Logs,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Global,
        Tab::Playlists,
        Tab::Queue,
        Tab::Devices,
        Tab::Logs,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Global => "Global",
            Tab::Playlists => "Playlists",
            Tab::Queue => "Queue",
            Tab::Devices => "Devices",
            Tab::Logs => "Logs",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap()
    }

    pub fn from_index(index: usize) -> Option<Tab> {
        Self::ALL.get(index).copied()
    }

    pub fn next(self) -> Tab {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Tab {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    pub fn key_context(self) -> KeyContext {
        match self {
            Tab::Global => KeyContext::Library,
            Tab::Playlists => KeyContext::Playlists,
            Tab::Queue => KeyContext::Queue,
            Tab::Devices => KeyContext::Devices,
            Tab::Logs => KeyContext::Logs,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepeatMode {
    #[default]
    Off,
    One,
    All,
}

impl RepeatMode {
    pub fn label(self) -> &'static str {
        match self {
            RepeatMode::Off => "off",
            RepeatMode::One => "one",
            RepeatMode::All => "all",
        }
    }

    pub fn next(self) -> RepeatMode {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }
}

/// Playback state mirrored for the UI: the queue, the loaded track and the
/// position polled from the audio thread on every tick.
#[derive(Debug)]
pub struct PlayerBar {
    pub queue: Vec<TrackItem>,
    pub queue_pos: usize,
    pub current: Option<TrackItem>,
    /// A track is loaded (playing or paused); false = stopped.
    pub playing: bool,
    pub paused: bool,
    pub position_secs: f64,
    /// Epoch seconds when the current track started (for history reports).
    pub track_started_at: Option<i64>,
    /// Queue index already enqueued in the audio thread for gapless play.
    pub prefetched_pos: Option<usize>,
    pub volume: u8,
    pub shuffle: bool,
    pub repeat: RepeatMode,
}

impl Default for PlayerBar {
    fn default() -> Self {
        Self {
            queue: Vec::new(),
            queue_pos: 0,
            current: None,
            playing: false,
            paused: false,
            position_secs: 0.0,
            track_started_at: None,
            prefetched_pos: None,
            volume: 80,
            shuffle: false,
            repeat: RepeatMode::Off,
        }
    }
}

/// Single source of truth for the UI. Mutated only by `update()` and the
/// event handlers in the main loop; views render from `&AppState`.
#[derive(Debug, Default)]
pub struct AppState {
    pub screen: Screen,
    pub active_tab: Tab,
    pub should_quit: bool,
    /// Double-press quit confirmation: set by the first Quit press, expires
    /// after a short window (any other action also cancels it).
    pub quit_armed_until: Option<std::time::Instant>,
    pub help_visible: bool,
    pub pending_keys: Option<String>,
    pub status_message: Option<String>,
    pub player: PlayerBar,
    pub login: LoginForm,
    pub user: Option<User>,
    pub global: GlobalTab,
    pub artist_views: HashMap<i64, Loadable<ArtistDetail>>,
    pub release_views: HashMap<i64, Loadable<ReleaseDetail>>,
    pub playlists: PlaylistsTab,
    pub playlist_views: HashMap<i64, Loadable<PlaylistDetail>>,
    /// Liked track ids, for the ♥ markers everywhere tracks are shown.
    pub likes: std::collections::HashSet<i64>,
    pub likes_loaded: bool,
    pub logs: LogsTab,
    pub cmdline: Cmdline,
    pub search: SearchState,
    /// Shared image cache keyed by `art::cache_key(url, w, h)`; reused by
    /// every view that shows artwork.
    pub art: HashMap<String, ArtState>,
}
