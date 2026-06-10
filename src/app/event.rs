use std::sync::Arc;

use crate::api::auth::AuthSession;
use crate::api::models::{
    ArtistDetail, ArtistsPage, PlaylistCard, PlaylistDetail, ReleaseDetail, SearchResults,
    TrackItem,
};
use crate::art::ArtImage;

/// Events delivered to the main loop by background tasks (API fetches, the
/// playback engine, device sync). Tasks never touch AppState directly.
#[derive(Debug)]
pub enum AppEvent {
    StatusMessage(String),
    LoginSucceeded(Box<AuthSession>),
    LoginFailed(String),
    /// Loopback listener received the browser SSO callback.
    SsoCallback(Result<String, String>),
    /// Refresh token rejected — stored credentials were deleted.
    SessionExpired,
    /// A page of the Global artists list arrived (or failed).
    ArtistsLoaded(Result<ArtistsPage, String>),
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
    /// Artwork fetched and decoded for the shared art cache.
    ArtLoaded {
        key: String,
        art: Option<Arc<ArtImage>>,
    },
    Player(crate::player::PlayerEvent),
    /// A command from the OS media keys.
    Media(crate::media::MediaCommand),
    /// Gapless prefetch could not open the stream; the normal track-switch
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
}
