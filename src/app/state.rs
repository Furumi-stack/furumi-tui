use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::app::input::LineEdit;
use crate::art::ArtImage;
use crate::config::keymap::KeyContext;
use crate::library::models::{
    ArtistCard, ArtistDetail, Availability, PlaylistCard, PlaylistDetail, ReleaseDetail,
    SearchResults, TrackItem,
};

/// Remote data that a view renders: spinner, content, or error.
#[derive(Debug)]
pub enum Loadable<T> {
    Loading,
    Ready(T),
    Failed(String),
}

/// Tile geometry for the Library artist grid (kept here so selection math in
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

/// A drill-down view pushed on top of the Library artist grid. Cursors live
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

/// The Library tab: the whole server library of artists.
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
    pub filters: crate::config::settings::LibraryFilters,
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
            filters: crate::config::settings::LibraryFilters::default(),
            stack: Vec::new(),
            page_limit: None,
            reloading: false,
        }
    }
}

fn release_type_groups<T>(
    items: &[T],
    release_type: impl Fn(&T) -> &str,
    release_year: impl Fn(&T) -> Option<i32>,
    release_title: impl Fn(&T) -> &str,
) -> Vec<(&'static str, Vec<usize>)> {
    const GROUPS: [(&str, &str); 4] = [
        ("album", "Albums"),
        ("single", "Singles"),
        ("ep", "EPs"),
        ("compilation", "Compilations"),
    ];
    let mut groups: Vec<(&'static str, Vec<usize>)> = Vec::new();
    for (kind, label) in GROUPS {
        let mut indices: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| release_type(item).eq_ignore_ascii_case(kind))
            .map(|(i, _)| i)
            .collect();
        sort_release_indices(&mut indices, items, &release_year, &release_title);
        if !indices.is_empty() {
            groups.push((label, indices));
        }
    }
    let known: Vec<usize> = groups.iter().flat_map(|(_, v)| v.iter().copied()).collect();
    let mut other: Vec<usize> = (0..items.len()).filter(|i| !known.contains(i)).collect();
    sort_release_indices(&mut other, items, &release_year, &release_title);
    if !other.is_empty() {
        groups.push(("Other", other));
    }
    groups
}

fn sort_release_indices<T>(
    indices: &mut [usize],
    items: &[T],
    release_year: &impl Fn(&T) -> Option<i32>,
    release_title: &impl Fn(&T) -> &str,
) {
    indices.sort_by(|&left, &right| {
        release_year(&items[right])
            .unwrap_or(i32::MIN)
            .cmp(&release_year(&items[left]).unwrap_or(i32::MIN))
            .then_with(|| release_title(&items[left]).cmp(release_title(&items[right])))
    });
}

pub fn fed_release_groups(
    releases: &[crate::federation::FedRelease],
) -> Vec<(&'static str, Vec<usize>)> {
    release_type_groups(
        releases,
        |release| &release.release_type,
        |release| release.year,
        |release| &release.title,
    )
}

/// Flattened display order of federated releases (concatenated groups).
pub fn fed_release_display_order(releases: &[crate::federation::FedRelease]) -> Vec<usize> {
    fed_release_groups(releases)
        .into_iter()
        .flat_map(|(_, indices)| indices)
        .collect()
}

/// Visual tile-grid rows of the federated releases section.
pub fn fed_release_rows(
    releases: &[crate::federation::FedRelease],
    columns: usize,
) -> Vec<Vec<usize>> {
    grouped_release_rows(fed_release_groups(releases), columns)
}

#[derive(Debug, Clone)]
pub struct ArtistReleaseSlot {
    pub title: String,
    pub release_type: String,
    pub year: Option<i32>,
    pub cover_path: Option<String>,
    pub local_index: Option<usize>,
    pub fed_index: Option<usize>,
    pub local_track_count: usize,
    pub total_track_count: usize,
    pub availability: Availability,
}

pub fn artist_release_groups(releases: &[ArtistReleaseSlot]) -> Vec<(&'static str, Vec<usize>)> {
    release_type_groups(
        releases,
        |release| &release.release_type,
        |release| release.year,
        |release| &release.title,
    )
}

pub fn artist_release_display_order(releases: &[ArtistReleaseSlot]) -> Vec<usize> {
    artist_release_groups(releases)
        .into_iter()
        .flat_map(|(_, indices)| indices)
        .collect()
}

pub fn artist_release_rows(releases: &[ArtistReleaseSlot], columns: usize) -> Vec<Vec<usize>> {
    grouped_release_rows(artist_release_groups(releases), columns)
}

fn grouped_release_rows(
    groups: Vec<(&'static str, Vec<usize>)>,
    columns: usize,
) -> Vec<Vec<usize>> {
    let columns = columns.max(1);
    let mut rows = Vec::new();
    let mut position = 0;
    for (_, group) in groups {
        for chunk in group.chunks(columns) {
            rows.push((position..position + chunk.len()).collect());
            position += chunk.len();
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(title: &str, release_type: &str, year: Option<i32>) -> ArtistReleaseSlot {
        ArtistReleaseSlot {
            title: title.to_string(),
            release_type: release_type.to_string(),
            year,
            cover_path: None,
            local_index: None,
            fed_index: None,
            local_track_count: 1,
            total_track_count: 1,
            availability: crate::library::models::Availability::Local,
        }
    }

    fn fed_release(
        title: &str,
        release_type: &str,
        year: Option<i32>,
    ) -> crate::federation::FedRelease {
        crate::federation::FedRelease {
            title: title.to_string(),
            release_type: release_type.to_string(),
            year,
            ..Default::default()
        }
    }

    #[test]
    fn release_display_order_is_newest_first_within_each_type() {
        let releases = vec![
            release("Old Album", "album", Some(1991)),
            release("New Single", "single", Some(2024)),
            release("New Album", "album", Some(2020)),
            release("Undated Album", "album", None),
            release("Old Single", "single", Some(1999)),
        ];

        assert_eq!(artist_release_display_order(&releases), vec![2, 0, 3, 1, 4]);
    }

    #[test]
    fn fed_release_display_order_is_newest_first_within_each_type() {
        let releases = vec![
            fed_release("Old Album", "album", Some(1991)),
            fed_release("New Single", "single", Some(2024)),
            fed_release("New Album", "album", Some(2020)),
            fed_release("Undated Album", "album", None),
            fed_release("Old Single", "single", Some(1999)),
        ];

        assert_eq!(fed_release_display_order(&releases), vec![2, 0, 3, 1, 4]);
    }

    #[test]
    fn artist_merged_releases_marks_partially_local_federated_release() {
        let local_id = "b3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let remote_id = "b3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let detail = ArtistDetail {
            id: 7,
            name: "Artist".to_string(),
            image_path: None,
            total_track_count: 1,
            total_play_count: 0,
            top_tracks: Vec::new(),
            releases: vec![crate::library::models::ReleaseCard {
                id: 11,
                title: "Album".to_string(),
                release_type: "album".to_string(),
                year: Some(2024),
                cover_path: None,
                track_count: 1,
                availability: Availability::Local,
            }],
            featured_tracks: Vec::new(),
        };
        let mut state = AppState::default();
        state.global.filters.source_mode = crate::config::settings::LibrarySourceMode::My;
        state.local_content_ids.insert(local_id.to_string());
        state.artist_fed_views.insert(
            detail.id,
            Loadable::Ready(crate::federation::FedArtistCard {
                name: detail.name.clone(),
                own_owner: None,
                peers: 1,
                owners: vec!["peer".to_string()],
                image_path: None,
                releases: vec![crate::federation::FedRelease {
                    title: "Album".to_string(),
                    release_type: "album".to_string(),
                    year: Some(2024),
                    tracks: vec![
                        crate::federation::FedCardTrack {
                            title: "Local".to_string(),
                            content_id: Some(local_id.to_string()),
                            ..Default::default()
                        },
                        crate::federation::FedCardTrack {
                            title: "Remote".to_string(),
                            content_id: Some(remote_id.to_string()),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                appears_on: Vec::new(),
            }),
        );

        let merged = artist_merged_releases(&state, detail.id, &detail);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].local_track_count, 1);
        assert_eq!(merged[0].total_track_count, 2);
        assert_eq!(merged[0].availability, Availability::Mixed);
        assert_eq!(merged[0].local_index, Some(0));
        assert_eq!(merged[0].fed_index, Some(0));
    }

    #[test]
    fn artist_merged_releases_ignores_federation_in_local_filter() {
        let detail = ArtistDetail {
            id: 7,
            name: "Artist".to_string(),
            image_path: None,
            total_track_count: 1,
            total_play_count: 0,
            top_tracks: Vec::new(),
            releases: vec![crate::library::models::ReleaseCard {
                id: 11,
                title: "Album".to_string(),
                release_type: "album".to_string(),
                year: Some(2024),
                cover_path: None,
                track_count: 1,
                availability: Availability::Local,
            }],
            featured_tracks: Vec::new(),
        };
        let mut state = AppState::default();
        state.artist_fed_views.insert(
            detail.id,
            Loadable::Ready(crate::federation::FedArtistCard {
                name: detail.name.clone(),
                peers: 1,
                releases: vec![crate::federation::FedRelease {
                    title: "Album".to_string(),
                    release_type: "album".to_string(),
                    tracks: vec![crate::federation::FedCardTrack {
                        title: "Remote".to_string(),
                        content_id: Some(
                            "b3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                                .to_string(),
                        ),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }),
        );

        let merged = artist_merged_releases(&state, detail.id, &detail);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].total_track_count, 1);
        assert_eq!(merged[0].availability, Availability::Local);
        assert_eq!(merged[0].fed_index, None);
    }

    #[test]
    fn artist_cursor_anchor_restores_release_after_federation_enrichment() {
        let detail = ArtistDetail {
            id: 7,
            name: "Artist".to_string(),
            image_path: None,
            total_track_count: 1,
            total_play_count: 0,
            top_tracks: Vec::new(),
            releases: vec![crate::library::models::ReleaseCard {
                id: 11,
                title: "Old Local".to_string(),
                release_type: "album".to_string(),
                year: Some(2001),
                cover_path: None,
                track_count: 1,
                availability: Availability::Local,
            }],
            featured_tracks: Vec::new(),
        };
        let mut state = AppState::default();
        state.global.filters.source_mode = crate::config::settings::LibrarySourceMode::My;
        let artist_id = detail.id;
        let artist_name = detail.name.clone();
        state
            .artist_views
            .insert(artist_id, Loadable::Ready(detail));
        state.global.stack.push(GlobalView::Artist {
            id: artist_id,
            cursor: 0,
        });

        let anchor = artist_cursor_anchor(&state, artist_id);
        state.artist_fed_views.insert(
            artist_id,
            Loadable::Ready(crate::federation::FedArtistCard {
                name: artist_name,
                peers: 1,
                releases: vec![crate::federation::FedRelease {
                    title: "New Remote".to_string(),
                    release_type: "album".to_string(),
                    year: Some(2024),
                    owners: vec!["peer".to_string()],
                    ..Default::default()
                }],
                ..Default::default()
            }),
        );
        restore_artist_cursor_anchor(&mut state, artist_id, anchor);

        assert!(matches!(
            state.global.stack.last(),
            Some(GlobalView::Artist { cursor: 1, .. })
        ));
    }
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
    /// The appears-on track list of the open federated artist card.
    FedAppearsOn,
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

/// What an add-to-playlist flow adds: local library tracks directly (including
/// already-materialized federation placeholders), or federated search/card
/// tracks that are downloaded into the library first.
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
    /// Pick one of the playlists (last row = "create new"); the target is
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
    /// Library-home filters. Cursor is kept for the next filters added here.
    LibraryFilters { cursor: usize },
    /// Track metadata viewer; left/right switch between selected tracks.
    TrackInfo {
        tracks: Vec<TrackItem>,
        cursor: usize,
        scroll: usize,
    },
    /// Artist picker over the info popup ('a' with several artists):
    /// Enter jumps to the chosen artist's page, Esc returns to the info.
    TrackArtists {
        tracks: Vec<TrackItem>,
        cursor: usize,
        scroll: usize,
        selected: usize,
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
    /// Wrapped text that can be copied to the system clipboard.
    FedCopyText {
        title: String,
        text: String,
        help: String,
    },
    /// Incoming trusted-device pairing request.
    DevicePairing {
        request_id: String,
        device_id: String,
        name: String,
        client_version: String,
        requester_group_id: Option<String>,
        requester_group_active_devices: usize,
    },
    /// Confirmation before revoking a trusted device.
    ConfirmDeviceRevoke { device_id: String, name: String },
    /// Connected playback devices and their current role/status.
    ConnectedDevices { cursor: usize },
    /// Full federation, transport and device status details.
    FederationStatusDetails {
        focus: StatusDetailFocus,
        status_cursor: usize,
        devices_scroll: usize,
        logs_scroll: usize,
    },
    /// Full text opened from the full federation status dashboard.
    FederationStatusText {
        parent: FederationStatusPopupState,
        title: String,
        text: String,
        scroll: usize,
    },
    /// Full connection log opened from the full federation status dashboard.
    FederationStatusLog {
        parent: FederationStatusPopupState,
        scroll: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FederationStatusPopupState {
    pub focus: StatusDetailFocus,
    pub status_cursor: usize,
    pub devices_scroll: usize,
    pub logs_scroll: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusDetailFocus {
    Status,
    Devices,
    Logs,
}

impl From<FederationStatusPopupState> for Popup {
    fn from(value: FederationStatusPopupState) -> Self {
        Popup::FederationStatusDetails {
            focus: value.focus,
            status_cursor: value.status_cursor,
            devices_scroll: value.devices_scroll,
            logs_scroll: value.logs_scroll,
        }
    }
}

impl StatusDetailFocus {
    pub fn next(self) -> Self {
        match self {
            StatusDetailFocus::Status => StatusDetailFocus::Devices,
            StatusDetailFocus::Devices => StatusDetailFocus::Logs,
            StatusDetailFocus::Logs => StatusDetailFocus::Status,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            StatusDetailFocus::Status => StatusDetailFocus::Logs,
            StatusDetailFocus::Devices => StatusDetailFocus::Status,
            StatusDetailFocus::Logs => StatusDetailFocus::Devices,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FedInputField {
    NetworkId,
    ConnectTicket,
    DeviceName,
    ConnectInvite,
}

impl FedInputField {
    pub fn title(self) -> &'static str {
        match self {
            FedInputField::NetworkId => "Network ID",
            FedInputField::ConnectTicket => "Connect to peer (paste ticket)",
            FedInputField::DeviceName => "Device name",
            FedInputField::ConnectInvite => "Connect device (paste frid://i invite)",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            FedInputField::NetworkId => {
                "A unique network id. It must match exactly on every client that should see and connect to the same peers."
            }
            FedInputField::ConnectTicket => {
                "Paste a connection ticket generated by another client to connect to that peer directly."
            }
            FedInputField::DeviceName => {
                "A friendly nickname for this device, used only to make connected-device management easier."
            }
            FedInputField::ConnectInvite => {
                "Paste a frid:// invite generated by another client to add this device to its sync group."
            }
        }
    }
}

/// Rows of the federation block inside Settings, in display order.
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

/// Rows of the Settings tab, in display order. Federation rows are fixed;
/// visualization script rows are derived from the scripts currently found in
/// the config directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    Federation(FedRow),
    StatusDetails,
    DeviceName,
    DeviceInvite,
    DeviceConnect,
    DeviceSyncNow,
    Device(usize),
    VisualizationClock,
    VisualizationScript(usize),
    VisualizationNew,
    VisualizationEdit,
}

pub const DEVICE_ONLINE_TTL_MS: i64 = 45_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevicePresenceSection {
    Online,
    Offline,
    Revoked,
}

impl DevicePresenceSection {
    pub fn title(self) -> &'static str {
        match self {
            DevicePresenceSection::Online => "Online devices",
            DevicePresenceSection::Offline => "Offline devices",
            DevicePresenceSection::Revoked => "Revoked devices",
        }
    }

    fn sort_rank(self) -> u8 {
        match self {
            DevicePresenceSection::Online => 0,
            DevicePresenceSection::Offline => 1,
            DevicePresenceSection::Revoked => 2,
        }
    }
}

pub fn unix_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn device_display_name(device: &crate::devices::DeviceStatusRow) -> String {
    if device.name.trim().is_empty() {
        device.device_id.chars().take(10).collect()
    } else {
        device.name.clone()
    }
}

pub fn device_status_active(state: &AppState, device_id: &str) -> bool {
    state
        .device_playback
        .active_device_id
        .as_deref()
        .is_some_and(|active| active == device_id)
        || state
            .device_playback
            .remote
            .get(device_id)
            .is_some_and(|snapshot| snapshot.active)
}

pub fn device_status_online(
    state: &AppState,
    device: &crate::devices::DeviceStatusRow,
    now_ms: i64,
) -> bool {
    device.is_self
        || device.device_id == state.device_playback.self_device_id
        || device
            .last_seen_ms
            .is_some_and(|seen| now_ms.saturating_sub(seen) <= DEVICE_ONLINE_TTL_MS)
        || state
            .device_playback
            .remote
            .get(&device.device_id)
            .is_some_and(|snapshot| {
                now_ms.saturating_sub(snapshot.updated_at_ms) <= DEVICE_ONLINE_TTL_MS
            })
}

pub fn device_presence_section(
    state: &AppState,
    device: &crate::devices::DeviceStatusRow,
    now_ms: i64,
) -> DevicePresenceSection {
    if device.revoked {
        DevicePresenceSection::Revoked
    } else if device_status_online(state, device, now_ms) {
        DevicePresenceSection::Online
    } else {
        DevicePresenceSection::Offline
    }
}

pub fn device_status_order(state: &AppState) -> Vec<usize> {
    let Some(status) = &state.federation.devices else {
        return Vec::new();
    };
    let now = unix_time_ms();
    let mut indices: Vec<usize> = status
        .devices
        .iter()
        .enumerate()
        .filter_map(|(index, device)| (!device.revoked).then_some(index))
        .collect();
    indices.sort_by(|left, right| {
        let left_device = &status.devices[*left];
        let right_device = &status.devices[*right];
        let left_section = device_presence_section(state, left_device, now).sort_rank();
        let right_section = device_presence_section(state, right_device, now).sort_rank();
        (
            left_section,
            !device_status_active(state, &left_device.device_id),
            !left_device.is_self,
            device_display_name(left_device).to_ascii_lowercase(),
            left_device.device_id.as_str(),
        )
            .cmp(&(
                right_section,
                !device_status_active(state, &right_device.device_id),
                !right_device.is_self,
                device_display_name(right_device).to_ascii_lowercase(),
                right_device.device_id.as_str(),
            ))
    });
    indices
}

pub fn settings_rows(state: &AppState) -> Vec<SettingsRow> {
    let mut rows = Vec::new();
    rows.extend(FedRow::ALL.into_iter().map(SettingsRow::Federation));
    rows.push(SettingsRow::DeviceName);
    rows.push(SettingsRow::DeviceInvite);
    rows.push(SettingsRow::DeviceConnect);
    rows.push(SettingsRow::DeviceSyncNow);
    rows.extend(
        device_status_order(state)
            .into_iter()
            .map(SettingsRow::Device),
    );
    rows.push(SettingsRow::VisualizationClock);
    rows.extend(
        state
            .visualizer
            .scripts
            .iter()
            .enumerate()
            .map(|(index, _)| SettingsRow::VisualizationScript(index)),
    );
    rows.push(SettingsRow::VisualizationNew);
    if !state.visualizer.scripts.is_empty() {
        rows.push(SettingsRow::VisualizationEdit);
    }
    rows.push(SettingsRow::StatusDetails);
    rows
}

/// Federation settings mirror + the latest status snapshot for Settings.
#[derive(Debug, Default)]
pub struct FederationTab {
    pub settings: crate::federation::FedSettings,
    pub status: Option<crate::federation::FedStatus>,
    pub devices: Option<crate::devices::DeviceSyncStatus>,
    pub publishing: bool,
    pub device_syncing: bool,
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
            Tab::Global => "Library",
            Tab::Playlists => "Playlists",
            Tab::Queue => "Queue",
            Tab::Federation => "Settings",
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
    pub audio_analysis: crate::player::AudioAnalysisSnapshot,
    /// Epoch seconds when the current track started (for history reports).
    pub track_started_at: Option<i64>,
    /// Queue index already enqueued in the audio thread for gapless play.
    pub prefetched_pos: Option<usize>,
    pub volume: u8,
    pub shuffle: bool,
    /// Stable track keys in pre-shuffle order; restores the queue when
    /// shuffle is turned off.
    pub original_order: Option<Vec<String>>,
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
            audio_analysis: crate::player::AudioAnalysisSnapshot::default(),
            track_started_at: None,
            prefetched_pos: None,
            original_order: None,
            volume: 80,
            shuffle: false,
            repeat: RepeatMode::Off,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DevicePlaybackRole {
    #[default]
    Active,
    Control,
}

impl DevicePlaybackRole {
    pub fn label(self) -> &'static str {
        match self {
            DevicePlaybackRole::Active => "active",
            DevicePlaybackRole::Control => "control",
        }
    }
}

#[derive(Debug, Default)]
pub struct DevicePlaybackState {
    pub role: DevicePlaybackRole,
    pub self_device_id: String,
    pub self_device_name: String,
    pub active_device_id: Option<String>,
    pub active_device_name: Option<String>,
    pub online_devices: usize,
    pub local_idle_since_ms: Option<i64>,
    pub remote: BTreeMap<String, crate::devices::PlaybackSnapshot>,
    pub last_remote_snapshot: Option<crate::devices::PlaybackSnapshot>,
}

impl DevicePlaybackState {
    pub fn is_control(&self) -> bool {
        self.role == DevicePlaybackRole::Control
    }

    pub fn active_label(&self) -> String {
        self.active_device_name
            .clone()
            .or_else(|| self.active_device_id.clone())
            .unwrap_or_else(|| "this device".to_string())
    }
}

/// Single source of truth for the UI. Mutated only by `update()` and the
/// event handlers in the main loop; views render from `&AppState`.
#[derive(Debug, Default)]
pub struct AppState {
    pub active_tab: Tab,
    pub should_quit: bool,
    pub shutting_down: bool,
    /// Double-press quit confirmation: set by the first Quit press, expires
    /// after a short window (any other action also cancels it).
    pub quit_armed_until: Option<std::time::Instant>,
    pub help_visible: bool,
    pub pending_keys: Option<String>,
    pub status_message: Option<String>,
    pub spinner_frame: usize,
    pub settings_cursor: usize,
    pub player: PlayerBar,
    pub device_playback: DevicePlaybackState,
    pub visualizer: crate::visualizer::VisualizerState,
    pub global: GlobalTab,
    pub artist_views: HashMap<i64, Loadable<ArtistDetail>>,
    /// Federated card data that enriches a local artist page in-place.
    pub artist_fed_views: HashMap<i64, Loadable<crate::federation::FedArtistCard>>,
    pub release_views: HashMap<i64, Loadable<ReleaseDetail>>,
    pub playlists: PlaylistsTab,
    pub playlist_views: HashMap<i64, Loadable<PlaylistDetail>>,
    /// Liked local-library content ids, for the ♥ markers everywhere tracks are shown.
    pub likes: HashSet<String>,
    /// Liked federated tracks (DHT item ids and content ids) — likes that
    /// reference peers' tracks without downloading them.
    pub fed_likes: HashSet<String>,
    /// Content ids that currently have a local playable file.
    pub local_content_ids: HashSet<String>,
    pub likes_loaded: bool,
    pub local_content_ids_loaded: bool,
    pub logs: LogsTab,
    pub queue_tab: QueueTab,
    pub federation: FederationTab,
    /// The one federated artist card being viewed (name + loading state);
    /// opening another card replaces it.
    pub fed_artist_view: Option<(String, Loadable<crate::federation::FedArtistCard>)>,
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

impl AppState {
    pub fn advance_spinner(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
    }

    pub fn spinner(&self) -> &'static str {
        const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        FRAMES[self.spinner_frame % FRAMES.len()]
    }

    pub fn connected_devices_enabled(&self) -> bool {
        self.federation.settings.enabled && !self.federation.settings.network_id.trim().is_empty()
    }

    pub fn fed_track_liked(&self, fed: &crate::federation::FedTrack) -> bool {
        self.fed_likes.contains(&fed.item_id)
            || fed
                .content_id
                .as_deref()
                .and_then(music_dht::normalize_content_id)
                .is_some_and(|content_id| {
                    self.likes.contains(&content_id) || self.fed_likes.contains(&content_id)
                })
    }

    pub fn fed_card_track_liked(&self, track: &crate::federation::FedCardTrack) -> bool {
        track
            .content_id
            .as_deref()
            .and_then(music_dht::normalize_content_id)
            .is_some_and(|content_id| {
                self.likes.contains(&content_id) || self.fed_likes.contains(&content_id)
            })
            || track
                .sources
                .iter()
                .any(|(_, item_id)| self.fed_likes.contains(item_id))
    }

    pub fn content_id_local(&self, content_id: &str) -> bool {
        music_dht::normalize_content_id(content_id)
            .is_some_and(|content_id| self.local_content_ids.contains(&content_id))
    }

    pub fn fed_track_local(&self, track: &crate::federation::FedTrack) -> bool {
        track
            .content_id
            .as_deref()
            .is_some_and(|content_id| self.content_id_local(content_id))
    }

    pub fn fed_card_track_local(&self, track: &crate::federation::FedCardTrack) -> bool {
        track
            .content_id
            .as_deref()
            .is_some_and(|content_id| self.content_id_local(content_id))
    }

    pub fn track_content_local(&self, track: &TrackItem) -> bool {
        track_content_id(track)
            .is_some_and(|content_id| self.local_content_ids.contains(&content_id))
    }

    pub fn track_liked(&self, track: &TrackItem) -> bool {
        if let Some(content_id) = track_content_id(track) {
            return self.likes.contains(&content_id) || self.fed_likes.contains(&content_id);
        }
        track
            .fed
            .as_ref()
            .is_some_and(|fed| self.fed_track_liked(fed))
    }
}

pub fn artist_fed_card(state: &AppState, id: i64) -> Option<&crate::federation::FedArtistCard> {
    match state.artist_fed_views.get(&id) {
        Some(Loadable::Ready(card)) => Some(card),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtistCursorAnchor {
    TopTrack(i64),
    Release {
        local_id: Option<i64>,
        title_key: String,
    },
    FeaturedTrack(i64),
}

pub fn artist_cursor_anchor(state: &AppState, id: i64) -> Option<ArtistCursorAnchor> {
    let Some(GlobalView::Artist {
        id: current_id,
        cursor,
    }) = state.global.stack.last()
    else {
        return None;
    };
    if *current_id != id {
        return None;
    }
    let Some(Loadable::Ready(detail)) = state.artist_views.get(&id) else {
        return None;
    };
    if *cursor < detail.top_tracks.len() {
        return detail
            .top_tracks
            .get(*cursor)
            .map(|track| ArtistCursorAnchor::TopTrack(track.id));
    }

    let releases = artist_merged_releases(state, id, detail);
    let release_order = artist_release_display_order(&releases);
    let release_position = cursor.checked_sub(detail.top_tracks.len())?;
    if let Some(&slot_index) = release_order.get(release_position) {
        let slot = &releases[slot_index];
        let local_id = slot
            .local_index
            .and_then(|index| detail.releases.get(index))
            .map(|release| release.id);
        return Some(ArtistCursorAnchor::Release {
            local_id,
            title_key: release_merge_key(&slot.title),
        });
    }

    let featured_position = cursor.checked_sub(detail.top_tracks.len() + release_order.len())?;
    detail
        .featured_tracks
        .get(featured_position)
        .map(|track| ArtistCursorAnchor::FeaturedTrack(track.id))
}

pub fn restore_artist_cursor_anchor(
    state: &mut AppState,
    id: i64,
    anchor: Option<ArtistCursorAnchor>,
) {
    let Some(anchor) = anchor else {
        return;
    };
    let Some(Loadable::Ready(detail)) = state.artist_views.get(&id) else {
        return;
    };
    let releases = artist_merged_releases(state, id, detail);
    let release_order = artist_release_display_order(&releases);
    let next_cursor = match anchor {
        ArtistCursorAnchor::TopTrack(track_id) => detail
            .top_tracks
            .iter()
            .position(|track| track.id == track_id),
        ArtistCursorAnchor::Release {
            local_id,
            title_key,
        } => release_order
            .iter()
            .position(|&slot_index| {
                let slot = &releases[slot_index];
                if let Some(wanted_id) = local_id {
                    let local_matches = slot
                        .local_index
                        .and_then(|index| detail.releases.get(index))
                        .is_some_and(|release| release.id == wanted_id);
                    if local_matches {
                        return true;
                    }
                }
                release_merge_key(&slot.title) == title_key
            })
            .map(|position| detail.top_tracks.len() + position),
        ArtistCursorAnchor::FeaturedTrack(track_id) => detail
            .featured_tracks
            .iter()
            .position(|track| track.id == track_id)
            .map(|position| detail.top_tracks.len() + release_order.len() + position),
    };
    let Some(next_cursor) = next_cursor else {
        return;
    };
    if let Some(GlobalView::Artist {
        id: current_id,
        cursor,
    }) = state.global.stack.last_mut()
        && *current_id == id
    {
        *cursor = next_cursor;
    }
}

pub fn artist_merged_releases(
    state: &AppState,
    id: i64,
    detail: &ArtistDetail,
) -> Vec<ArtistReleaseSlot> {
    let mut slots: Vec<ArtistReleaseSlot> = detail
        .releases
        .iter()
        .enumerate()
        .map(|(index, release)| ArtistReleaseSlot {
            title: release.title.clone(),
            release_type: release.release_type.clone(),
            year: release.year,
            cover_path: release.cover_path.clone(),
            local_index: Some(index),
            fed_index: None,
            local_track_count: release.track_count.max(0) as usize,
            total_track_count: release.track_count.max(0) as usize,
            availability: release.availability,
        })
        .collect();

    if !state.global.filters.source_mode.includes_network() {
        return slots;
    }

    let mut by_key: HashMap<String, usize> = slots
        .iter()
        .enumerate()
        .filter_map(|(slot_index, release)| {
            let key = release_merge_key(&release.title);
            (!key.is_empty()).then_some((key, slot_index))
        })
        .collect();

    let Some(card) = artist_fed_card(state, id) else {
        return slots;
    };
    for (fed_index, release) in card.releases.iter().enumerate() {
        let key = release_merge_key(&release.title);
        let existing = by_key.get(&key).copied();
        let local_from_fed = release
            .tracks
            .iter()
            .filter(|track| state.fed_card_track_local(track))
            .count();
        let fed_total = release.tracks.len();
        let local_count = existing
            .and_then(|slot_index| slots.get(slot_index))
            .map(|slot| slot.local_track_count)
            .unwrap_or(0)
            .max(local_from_fed);
        let availability =
            fed_release_slot_availability(local_count, fed_total, existing.is_some());
        match existing {
            Some(slot_index) => {
                let slot = &mut slots[slot_index];
                slot.fed_index = Some(fed_index);
                slot.release_type = prefer_text(&slot.release_type, &release.release_type);
                slot.year = slot.year.or(release.year);
                if slot.cover_path.is_none() {
                    slot.cover_path = release.cover_path.clone();
                }
                slot.total_track_count = slot.total_track_count.max(fed_total);
                slot.local_track_count = slot.local_track_count.max(local_from_fed);
                slot.availability = availability;
            }
            None => {
                by_key.insert(key, slots.len());
                slots.push(ArtistReleaseSlot {
                    title: release.title.clone(),
                    release_type: release.release_type.clone(),
                    year: release.year,
                    cover_path: release.cover_path.clone(),
                    local_index: None,
                    fed_index: Some(fed_index),
                    local_track_count: local_from_fed,
                    total_track_count: fed_total,
                    availability,
                });
            }
        }
    }
    slots
}

pub fn fed_card_track_visible(state: &AppState, track: &crate::federation::FedCardTrack) -> bool {
    state.global.filters.source_mode.includes_network() || state.fed_card_track_local(track)
}

pub fn fed_release_visible_track_indices(
    state: &AppState,
    release: &crate::federation::FedRelease,
) -> Vec<usize> {
    release
        .tracks
        .iter()
        .enumerate()
        .filter_map(|(index, track)| fed_card_track_visible(state, track).then_some(index))
        .collect()
}

pub fn fed_release_visible(state: &AppState, release: &crate::federation::FedRelease) -> bool {
    state.global.filters.source_mode.includes_network()
        || release
            .tracks
            .iter()
            .any(|track| state.fed_card_track_local(track))
}

pub fn fed_appearance_visible(
    state: &AppState,
    appearance: &crate::federation::FedAppearsOn,
) -> bool {
    fed_card_track_visible(state, &appearance.track)
}

pub fn fed_artist_visible_release_indices(
    state: &AppState,
    card: &crate::federation::FedArtistCard,
) -> Vec<usize> {
    card.releases
        .iter()
        .enumerate()
        .filter_map(|(index, release)| fed_release_visible(state, release).then_some(index))
        .collect()
}

pub fn fed_artist_visible_appearance_indices(
    state: &AppState,
    card: &crate::federation::FedArtistCard,
) -> Vec<usize> {
    card.appears_on
        .iter()
        .enumerate()
        .filter_map(|(index, appearance)| {
            fed_appearance_visible(state, appearance).then_some(index)
        })
        .collect()
}

fn release_merge_key(title: &str) -> String {
    music_dht::normalize_name(title)
}

fn prefer_text(left: &str, right: &str) -> String {
    if left.trim().is_empty() {
        right.to_string()
    } else {
        left.to_string()
    }
}

fn fed_release_slot_availability(
    local: usize,
    total: usize,
    has_local_release: bool,
) -> Availability {
    if total == 0 {
        return if has_local_release {
            Availability::Local
        } else {
            Availability::Remote
        };
    }
    if local == 0 {
        Availability::Remote
    } else if local >= total {
        Availability::Local
    } else {
        Availability::Mixed
    }
}

pub fn track_content_id(track: &TrackItem) -> Option<String> {
    track
        .content_id
        .as_deref()
        .and_then(music_dht::normalize_content_id)
        .or_else(|| {
            track
                .fed
                .as_ref()
                .and_then(|fed| fed.content_id.as_deref())
                .and_then(music_dht::normalize_content_id)
        })
}

pub fn track_key(track: &TrackItem) -> String {
    if let Some(content_id) = track_content_id(track) {
        return format!("content:{content_id}");
    }
    if let Some(fed) = &track.fed {
        return format!("fed:{}:{}", fed.owner, fed.item_id);
    }
    format!("local:{}", track.id)
}
