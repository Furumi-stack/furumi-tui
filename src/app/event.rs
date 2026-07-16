use std::sync::Arc;

use crate::art::ArtImage;
use crate::library::models::{
    ArtistDetail, ArtistsPage, PlaylistCard, PlaylistDetail, ReleaseDetail, SearchResults,
    TrackItem,
};

/// Events delivered to the main loop by background tasks (library queries,
/// the playback engine, imports). Tasks never touch AppState directly.
#[derive(Debug)]
pub enum AppEvent {
    StatusMessage(String),
    /// A page of the artists list arrived (or failed).
    ArtistsLoaded(Result<ArtistsPage, String>),
    /// A full reload after a library change: replaces the loaded artist
    /// list wholesale, so the grid never flashes empty.
    ArtistsReloaded {
        page: ArtistsPage,
        limit: i64,
    },
    ArtistViewLoaded {
        id: i64,
        result: Result<ArtistDetail, String>,
    },
    ReleaseViewLoaded {
        id: i64,
        result: Result<ReleaseDetail, String>,
    },
    /// Live search results; `seq` drops responses that are already stale.
    SearchLoaded {
        seq: u64,
        result: Result<SearchResults, String>,
    },
    /// Artwork loaded and decoded for the shared art cache.
    ArtLoaded {
        key: String,
        art: Option<Arc<ArtImage>>,
    },
    Player(crate::player::PlayerEvent),
    /// A command from the OS media keys.
    Media(crate::media::MediaCommand),
    /// Gapless prefetch could not open the file; the normal track-switch
    /// path takes over when the current track ends.
    PrefetchFailed {
        pos: usize,
    },
    PlaylistsLoaded(Result<Vec<PlaylistCard>, String>),
    PlaylistViewLoaded {
        id: i64,
        result: Result<PlaylistDetail, String>,
    },
    /// Liked track ids for the ♥ markers.
    LikesLoaded(Result<Vec<i64>, String>),
    LikeToggled {
        track_id: i64,
        liked: bool,
    },
    /// A release fetched for queueing (a / shift-a on a release).
    EnqueueTracks {
        tracks: Vec<TrackItem>,
        next: bool,
    },
    PlaylistCreated {
        result: Result<PlaylistCard, String>,
        /// Add this track to the new playlist right away (Shift-P flow).
        add_track: Option<TrackItem>,
    },
    PlaylistTracksAdded {
        playlist_id: i64,
        playlist_title: String,
        result: Result<(), String>,
    },
    /// The library was mutated (import, edit, delete): cached views must be
    /// dropped and reloaded lazily.
    LibraryChanged {
        message: Option<String>,
    },
    /// Progress of a running import, shown in the status bar.
    ImportProgress {
        done: usize,
        total: usize,
        current: String,
    },
    /// Fresh copies of the queued tracks after a library change. Tracks
    /// missing from the result were deleted and leave the queue.
    QueueTracksRefreshed {
        tracks: Vec<TrackItem>,
    },
    /// A status snapshot for the Federation tab.
    FederationStatus(crate::federation::FedStatus),
    /// Federated live-search results (artists a card can be opened for,
    /// plus matching tracks).
    FedSearchLoaded {
        seq: u64,
        result: Result<crate::federation::FedSearchResults, String>,
    },
    /// A federated artist card finished assembling.
    FedArtistLoaded {
        name: String,
        result: Result<crate::federation::FedArtistCard, String>,
    },
    /// A federated track finished downloading and is ready to play.
    FedPlayReady {
        result: Result<crate::federation::FedPlayable, String>,
    },
    /// This peer's connection ticket, requested from the Federation tab.
    FedTicket(Result<String, String>),
}
