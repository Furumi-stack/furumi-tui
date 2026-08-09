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
    /// Local similar-track search completed. It uses the same sequence as
    /// text search so stale pages cannot overwrite a newer request.
    SimilaritySearchLoaded {
        seq: u64,
        result: Result<SearchResults, String>,
        query: Option<crate::similarity::QueryVector>,
    },
    SimilarityStatus(crate::similarity::SimilarityStatus),
    /// `None` is emitted after clearing every stored embedding.
    SimilarityProfileActivated(Option<String>),
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
    /// Liked local content ids for the ♥ markers.
    LikesLoaded(Result<Vec<String>, String>),
    /// Local-library content ids for availability markers.
    LocalContentIdsLoaded(Result<Vec<String>, String>),
    /// Counts and storage footprint of the local library/database.
    LocalLibraryStatsLoaded(Result<crate::library::LocalLibraryStats, String>),
    MusicDirectoryValidated(Result<std::path::PathBuf, String>),
    MusicDirectoryChanged(Result<crate::library::MusicRelocationStats, String>),
    ListenHistoryLoaded(Result<Vec<crate::library::ListenHistoryEntry>, String>),
    /// One content id became available locally while the UI is open.
    LocalContentAvailable {
        content_id: String,
    },
    LikeToggled {
        content_id: String,
        liked: bool,
    },
    /// Liked federated item ids and content ids for the ♥ markers.
    FedLikesLoaded(Result<Vec<String>, String>),
    FedLikeToggled {
        item_id: String,
        content_id: Option<String>,
        liked: bool,
    },
    /// A release fetched for queueing (a / shift-a on a release).
    EnqueueTracks {
        tracks: Vec<TrackItem>,
        next: bool,
    },
    PlaylistCreated {
        result: Result<PlaylistCard, String>,
        /// Add this target to the new playlist right away (Shift-P flow).
        add_target: Option<crate::app::state::PlaylistAddTarget>,
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
    /// Federated enrichment for an already-open local artist view.
    ArtistFederationLoaded {
        id: i64,
        name: String,
        result: Result<crate::federation::FedArtistCard, String>,
    },
    /// A network-library source refreshed its cached top-artist slice.
    NetworkArtistCacheUpdated {
        source_id: String,
        count: usize,
    },
    /// A streamed image for the open card arrived (artist image when
    /// `release` is None, a release cover otherwise).
    FedCardArt {
        name: String,
        release: Option<String>,
        path: String,
    },
    /// A pending federated track finished downloading (or failed); the
    /// queue swaps the placeholder for the resolved track.
    FedTrackResolved {
        placeholder_id: i64,
        resolve_key: String,
        result: Result<Box<crate::federation::FedPlayable>, String>,
    },
    /// Rich metadata for a federated track-info preview arrived without
    /// downloading the audio file.
    FedTrackInfoLoaded {
        placeholder_id: i64,
        item_id: String,
        result: Result<TrackItem, String>,
    },
    /// This peer's connection ticket, requested from the Federation tab.
    FedTicket(Result<String, String>),
    /// Immediate library publish finished.
    FedSyncFinished(String),
    /// Fresh personal-device sync status snapshot for Settings.
    DeviceSyncStatus(crate::devices::DeviceSyncStatus),
    /// Invite link for pairing another device.
    DeviceInvite(Result<String, String>),
    /// Result of `:connect frid://i/...`.
    DeviceConnectResult(Result<String, String>),
    /// Manual trusted-device sync finished.
    DeviceSyncFinished(String),
    /// Incoming pairing request that passed the invite-secret check.
    DevicePairingRequest(crate::devices::PendingPairing),
    /// Trusted device playback state, delivered by personal-device sync.
    DevicePlayback(crate::devices::PlaybackSnapshot),
    /// Playback command addressed to this device.
    PlaybackCommand(crate::devices::PlaybackCommand),
    /// Current lifecycle/status of the federation Jam.
    JamStatus(crate::jam::JamStatus),
    /// Host playback snapshot received by a Jam participant.
    JamPlayback(crate::devices::PlaybackSnapshot),
    /// Participant command accepted by this Jam host.
    JamCommand(crate::devices::PlaybackCommand),
    /// Result of creating/regenerating a Jam capability.
    JamInvite(Result<String, String>),
    /// Result of joining a Jam capability.
    JamJoined(Result<String, String>),
}
