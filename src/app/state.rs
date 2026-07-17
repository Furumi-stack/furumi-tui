use std::collections::HashMap;
use std::sync::Arc;

use crate::app::input::LineEdit;
use crate::art::ArtImage;
use crate::config::keymap::KeyContext;
use crate::library::models::{
    ArtistCard, ArtistDetail, PlaylistCard, PlaylistDetail, ReleaseCard, ReleaseDetail,
    SearchResults, TrackItem,
};

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
    Artist {
        id: i64,
        cursor: usize,
    },
    Release {
        id: i64,
        cursor: usize,
    },
    /// Linear cursor over search results: artists, then releases, then tracks.
    Search {
        cursor: usize,
    },
    /// A federated artist card (data lives in `AppState::fed_artist_view`).
    FedArtist {
        cursor: usize,
    },
    /// One release of the open federated card: row 0 is the
    /// download-release button, rows 1..=n are its tracks.
    FedRelease {
        index: usize,
        cursor: usize,
    },
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
    /// Page size, fixed at the first request — the offset is
    /// `(page-1) * limit`, so it must not change between pages.
    pub page_limit: Option<i64>,
    /// A full atomic reload is in flight (after a library change); incoming
    /// pages of the old pagination are dropped until it lands.
    pub reloading: bool,
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
            page_limit: None,
            reloading: false,
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

/// The virtual Likes playlist id (`kind == "likes"`).
pub use crate::library::LIKES_PLAYLIST_ID;

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

/// The Queue tab's own cursor — independent from the playing position, so
/// the user can browse and pick tracks while something else plays.
#[derive(Debug, Default)]
pub struct QueueTab {
    pub cursor: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackSelectionScope {
    ArtistTop(i64),
    ArtistFeatured(i64),
    Release(i64),
    Playlist(i64),
    Queue,
    /// The federated section of the search results (its tracks).
    FedSearch,
    /// The tracklist of the open federated release view.
    FedRelease(usize),
}

/// Vim-like Shift-V selection for line-oriented track lists. The selected
/// range is always contiguous: anchor is where visual mode started, cursor is
/// extended by normal navigation.
#[derive(Debug, Clone, Default)]
pub struct TrackSelection {
    pub scope: Option<TrackSelectionScope>,
    pub anchor: usize,
    pub cursor: usize,
}

impl TrackSelection {
    pub fn is_active(&self) -> bool {
        self.scope.is_some()
    }

    pub fn is_active_for(&self, scope: &TrackSelectionScope) -> bool {
        self.scope.as_ref() == Some(scope)
    }

    pub fn start(&mut self, scope: TrackSelectionScope, cursor: usize) {
        self.scope = Some(scope);
        self.anchor = cursor;
        self.cursor = cursor;
    }

    pub fn clear(&mut self) {
        self.scope = None;
        self.anchor = 0;
        self.cursor = 0;
    }

    pub fn set_cursor(&mut self, scope: TrackSelectionScope, cursor: usize) {
        if self.scope.as_ref() == Some(&scope) {
            self.cursor = cursor;
        }
    }

    pub fn contains(&self, scope: &TrackSelectionScope, index: usize) -> bool {
        if self.scope.as_ref() != Some(scope) {
            return false;
        }
        let start = self.anchor.min(self.cursor);
        let end = self.anchor.max(self.cursor);
        (start..=end).contains(&index)
    }

    pub fn indices(&self, scope: &TrackSelectionScope, len: usize) -> Option<Vec<usize>> {
        if len == 0 || self.scope.as_ref() != Some(scope) {
            return None;
        }
        let start = self.anchor.min(self.cursor).min(len - 1);
        let end = self.anchor.max(self.cursor).min(len - 1);
        Some((start..=end).collect())
    }
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
    /// Cursor anchored to a specific entry's seq; appends never move it.
    /// None = newest (follow mode).
    pub selected_seq: Option<u64>,
}

impl Default for LogsTab {
    fn default() -> Self {
        Self {
            level_index: 2,
            follow: true,
            selected_seq: None,
        }
    }
}

/// What an open edit form writes to when saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTarget {
    Track(i64),
    Release(i64),
    Artist(i64),
    Playlist(i64),
}

/// What a confirmed delete removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteTarget {
    Track(i64),
    Release(i64),
    Artist(i64),
    Playlist(i64),
}

/// One text field of an edit form.
#[derive(Debug, Clone)]
pub struct EditField {
    pub label: &'static str,
    pub value: LineEdit,
}

impl EditField {
    pub fn new(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            label,
            value: LineEdit::new(value),
        }
    }
}

/// What an add-to-playlist flow adds: local library tracks directly, or
/// federated tracks that are downloaded into the library first.
#[derive(Debug, Clone)]
pub enum PlaylistAddTarget {
    Local(Vec<TrackItem>),
    Fed(Vec<crate::federation::FedTrack>),
}

impl PlaylistAddTarget {
    /// Short description for popup titles.
    pub fn label(&self) -> String {
        match self {
            PlaylistAddTarget::Local(tracks) if tracks.len() == 1 => tracks[0].title.clone(),
            PlaylistAddTarget::Fed(tracks) if tracks.len() == 1 => {
                format!("{} (federation)", tracks[0].title)
            }
            PlaylistAddTarget::Local(tracks) => format!("{} tracks", tracks.len()),
            PlaylistAddTarget::Fed(tracks) => format!("{} tracks (federation)", tracks.len()),
        }
    }
}

/// Modal dialog over the main screen.
#[derive(Debug)]
pub enum Popup {
    /// Pick one of the playlists (row 0 = "create new"); the target is
    /// added on Enter (federated tracks are downloaded first).
    AddToPlaylist {
        target: PlaylistAddTarget,
        cursor: usize,
    },
    /// Name input for a new playlist; when `for_target` is set, it is
    /// added to the playlist right after creation.
    NewPlaylist {
        for_target: Option<PlaylistAddTarget>,
        input: LineEdit,
        busy: bool,
    },
    /// Metadata edit form for a track, release, artist or playlist.
    Edit {
        target: EditTarget,
        title: String,
        fields: Vec<EditField>,
        focus: usize,
        error: Option<String>,
    },
    /// Delete confirmation; Enter/y deletes, Esc/n cancels.
    ConfirmDelete { target: DeleteTarget, label: String },
    /// Track metadata viewer; left/right switch between selected tracks.
    TrackInfo {
        tracks: Vec<TrackItem>,
        cursor: usize,
        scroll: usize,
    },
    /// Full, wrapped view of one log entry (Enter on the Logs tab).
    LogDetail(crate::config::logging::LogEntry),
    /// One-line text entry on the Federation tab (network id, peer ticket).
    FedInput {
        field: FedInputField,
        input: LineEdit,
    },
    /// Wrapped read-only text (this peer's connection ticket).
    FedText { title: String, text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FedInputField {
    NetworkId,
    ConnectTicket,
}

impl FedInputField {
    pub fn title(self) -> &'static str {
        match self {
            FedInputField::NetworkId => "Network ID",
            FedInputField::ConnectTicket => "Connect to peer (paste ticket)",
        }
    }
}

/// Rows of the Federation tab, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FedRow {
    Toggle,
    NetworkId,
    SaveOnListen,
    SyncNow,
    ShowTicket,
    Connect,
}

impl FedRow {
    pub const ALL: [FedRow; 6] = [
        FedRow::Toggle,
        FedRow::NetworkId,
        FedRow::SaveOnListen,
        FedRow::SyncNow,
        FedRow::ShowTicket,
        FedRow::Connect,
    ];
}

/// The Federation tab: settings mirror + the latest status snapshot.
#[derive(Debug, Default)]
pub struct FederationTab {
    pub cursor: usize,
    pub settings: crate::federation::FedSettings,
    pub status: Option<crate::federation::FedStatus>,
}

/// Playlists eligible as add-targets (the virtual Likes playlist is managed
/// through likes, not direct adds).
pub fn addable_playlists(state: &AppState) -> Vec<(i64, String)> {
    match &state.playlists.list {
        Some(Loadable::Ready(list)) => list
            .iter()
            .filter(|p| p.kind != "likes")
            .map(|p| (p.id, p.title.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// Command line (`:`), vim-style. Lives on the Main screen status bar.
#[derive(Debug, Default)]
pub struct Cmdline {
    pub active: bool,
    pub input: LineEdit,
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
    /// Tracks found on the federated network (empty while federation is
    /// off); rendered as a separate, marked section.
    pub fed_tracks: Vec<crate::federation::FedTrack>,
    /// Artists a federated card can be opened for — from artist records and
    /// from the artist names of matching tracks.
    pub fed_artists: Vec<crate::federation::FedArtistHit>,
    pub fed_loading: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Global,
    Playlists,
    Queue,
    Federation,
    Logs,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Global,
        Tab::Playlists,
        Tab::Queue,
        Tab::Federation,
        Tab::Logs,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Global => "Global",
            Tab::Playlists => "Playlists",
            Tab::Queue => "Queue",
            Tab::Federation => "Federation",
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
            Tab::Federation => KeyContext::Federation,
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
    /// Track ids in pre-shuffle order; restores the queue when shuffle is
    /// turned off.
    pub original_order: Option<Vec<i64>>,
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
            original_order: None,
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
    pub active_tab: Tab,
    pub should_quit: bool,
    /// Double-press quit confirmation: set by the first Quit press, expires
    /// after a short window (any other action also cancels it).
    pub quit_armed_until: Option<std::time::Instant>,
    pub help_visible: bool,
    pub pending_keys: Option<String>,
    pub status_message: Option<String>,
    pub player: PlayerBar,
    pub global: GlobalTab,
    pub artist_views: HashMap<i64, Loadable<ArtistDetail>>,
    pub release_views: HashMap<i64, Loadable<ReleaseDetail>>,
    pub playlists: PlaylistsTab,
    pub playlist_views: HashMap<i64, Loadable<PlaylistDetail>>,
    /// Liked track ids, for the ♥ markers everywhere tracks are shown.
    pub likes: std::collections::HashSet<i64>,
    /// Liked federated tracks (DHT item ids) — likes that reference peers'
    /// tracks without downloading them.
    pub fed_likes: std::collections::HashSet<String>,
    pub likes_loaded: bool,
    pub logs: LogsTab,
    pub queue_tab: QueueTab,
    pub federation: FederationTab,
    /// The one federated artist card being viewed (name + loading state);
    /// opening another card replaces it.
    pub fed_artist_view: Option<(String, Loadable<crate::federation::FedArtistCard>)>,
    /// The "search this artist in the federation" button of the open local
    /// artist view has the focus (reached by pressing Up from the first
    /// row, like the download button on a federated release).
    pub artist_fed_button: bool,
    pub track_selection: TrackSelection,
    /// Shift-J jump in flight: focus this (release, track) once the release
    /// view finishes loading.
    pub pending_release_focus: Option<(i64, i64)>,
    /// Where a Shift-J jump came from (tab, stack depth of the pushed
    /// view): Esc from that view returns to the origin tab instead of
    /// unwinding the Global stack.
    pub jump_origin: Option<(Tab, usize)>,
    pub popup: Option<Popup>,
    pub cmdline: Cmdline,
    pub search: SearchState,
    /// Shared image cache keyed by `art::cache_key(url, w, h)`; reused by
    /// every view that shows artwork.
    pub art: HashMap<String, ArtState>,
}
