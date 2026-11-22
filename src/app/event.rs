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
    UpdateChecked(Result<Option<crate::updater::Update>, String>),
    UpdateProgress(String),
    UpdateInstalled(Result<(), String>),
    StatusMessage(String),
    StatusVisualizationReady(crate::visualizer::status::Ready),
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
    /// Federated tracks that complete an already-open local release view.
    ReleaseFederationLoaded {
        id: i64,
        result: Result<Option<crate::federation::FedRelease>, String>,
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
        result: Result<Vec<crate::similarity::SimilarTrack>, String>,
        query: Option<crate::similarity::QueryVector>,
    },
    /// Federated candidates and diagnostics for a track-seeded search.
    FedSimilaritySearchLoaded {
        seq: u64,
        result: Result<crate::federation::FedSimilaritySearchResults, String>,
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
    /// Boxed to keep `AppEvent` small; the status is large and rare.
    FederationStatus(Box<crate::federation::FedStatus>),
    /// Federated live-search results (artists a card can be opened for,
    /// plus matching tracks).
    FedSearchLoaded {
        seq: u64,
        result: Result<crate::federation::FedSearchResults, String>,
    },
    /// Public podcast cards for the same live search (`seq`), delivered on
    /// their own so the music results never wait for the overlay. Already
    /// resolved against the podcast store.
    FedPodcastsLoaded {
        seq: u64,
        hits: Vec<crate::podcasts::discovery::PodcastHit>,
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
        /// Boxed: `TrackItem` is large since it can carry a podcast episode.
        result: Result<Box<TrackItem>, String>,
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
    PlaybackCommand {
        command: crate::devices::PlaybackCommand,
        authority: music_dht::playback::CommandStamp,
        origin: String,
    },
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
    /// Received podcast operations or snapshot sections changed the podcast
    /// store: `progress` lists changed progress registers, `changed` means
    /// anything else (subscriptions, feeds, sessions).
    PodcastsSynced {
        progress: Vec<music_dht::podcast::ProgressRecord>,
        changed: bool,
    },
    /// The podcast store opened (with the feed client), or why it did not.
    PodcastStoreOpened(Result<crate::podcasts::refresh::Services, String>),
    /// Subscribed podcasts reloaded from the podcast store.
    PodcastsLoaded(Result<Vec<crate::app::state::PodcastRow>, String>),
    PodcastSeasonsLoaded {
        podcast_id: music_dht::podcast::PodcastId,
        result: Result<Vec<furumi_library::podcasts::SeasonSummary>, String>,
    },
    /// One page of episodes; `seq` drops results of older requests.
    PodcastEpisodesLoaded {
        seq: u64,
        offset: usize,
        result: Result<Vec<furumi_library::podcasts::EpisodeListItem>, String>,
    },
    PodcastSubscribed(Result<crate::podcasts::refresh::SubscribeReport, String>),
    /// One feed check finished.
    PodcastRefreshed {
        podcast_id: music_dht::podcast::PodcastId,
        result: Result<crate::podcasts::refresh::RefreshReport, String>,
        /// Asked by the user (status bar), not the background scheduler.
        manual: bool,
    },
    PodcastUnsubscribed {
        podcast_id: music_dht::podcast::PodcastId,
        title: String,
        result: Result<(), String>,
    },
    /// Result of the progress writer (saves, supersedes, mark played).
    PodcastProgress(crate::podcasts::progress::ProgressEvent),
    /// An episode stream is playing; `generation` is the player source
    /// generation of the start that requested it.
    PodcastEpisodeStarted {
        generation: u64,
        episode: Box<music_dht::podcast::PodcastEpisodeRef>,
        start_ms: i64,
        duration_ms: Option<i64>,
        /// The decoder measured `duration_ms` (else the feed announced it).
        duration_measured: bool,
        restart: bool,
        /// The server allows seeking in the stream.
        seekable: bool,
        /// How the stream ended, once it does.
        probe: crate::podcasts::stream::StreamProbe,
    },
    /// Starting an episode failed (network, decoding, cannot resume).
    PodcastEpisodeFailed {
        generation: u64,
        message: String,
    },
    /// Scheduler timer: ask the store which feeds are due.
    PodcastRefreshTick,
    PodcastDownloads(Result<std::collections::HashSet<String>, String>),
    PodcastDownloaded {
        id: String,
        result: Result<(), String>,
    },
    /// Feeds due for a background refresh.
    PodcastsDue(Result<Vec<(music_dht::podcast::PodcastId, music_dht::podcast::FeedUrl)>, String>),
    /// What sharing a podcast would publish (`Effect::PodcastPrepareShare`).
    PodcastSharePrepared {
        podcast_id: music_dht::podcast::PodcastId,
        title: String,
        result: Result<crate::podcasts::publication::ShareCheck, String>,
    },
    /// The share decision was written (`Effect::PodcastSetShared`).
    PodcastShareSet {
        podcast_id: music_dht::podcast::PodcastId,
        title: String,
        shared: bool,
        result: Result<(), String>,
    },
    /// The publication reconciler finished a pass.
    PodcastPublication(crate::podcasts::publication::PublicationReport),
    /// A public card's feed was read for a preview (`Effect::PodcastPreview`).
    PodcastPreviewLoaded {
        podcast_id: music_dht::podcast::PodcastId,
        result: Result<Box<crate::podcasts::discovery::FeedPreview>, String>,
    },
}
