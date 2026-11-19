//! Podcast state fixtures shared by update and rendering tests.

use furumi_library::podcasts::{
    EpisodeListItem, EpisodeQuery, FetchState, PodcastSummary, SeasonSummary,
};
use music_dht::podcast::{
    EpisodeId, EpisodeMeta, EpisodeType, FeedUrl, PodcastId, PodcastMeta, ProgressRecord,
    ProgressStatus,
};

use crate::app::state::{
    AppState, EpisodeList, Loadable, OpenedPodcast, PodcastRow, PodcastStoreStatus, Tab,
};

pub fn podcast_id() -> PodcastId {
    PodcastId::from_feed_url(&FeedUrl::parse("https://example.test/feed.xml").unwrap())
}

pub fn row(title: &str, unplayed: u64, episodes: u64, seasons: u64) -> PodcastRow {
    PodcastRow {
        summary: PodcastSummary {
            podcast_id: podcast_id(),
            meta: PodcastMeta {
                feed_url: FeedUrl::parse("https://example.test/feed.xml").unwrap(),
                title: title.into(),
                author: Some("Host".into()),
                description: Some("A show about tests.".into()),
                link: None,
                image_url: None,
            },
            subscribed: true,
            shared: false,
            episode_count: episodes,
            unplayed_count: unplayed,
            in_progress_count: 0,
            latest_published_at_ms: None,
            season_count: seasons,
        },
        fetch: FetchState::default(),
    }
}

pub fn episode(n: u32, title: &str, status: Option<(ProgressStatus, i64)>) -> EpisodeListItem {
    let podcast = podcast_id();
    let episode_id = EpisodeId::from_guid(&podcast, &format!("ep-{n}")).unwrap();
    EpisodeListItem {
        episode: EpisodeMeta {
            episode_id: episode_id.clone(),
            podcast_id: podcast.clone(),
            guid: Some(format!("ep-{n}")),
            title: title.into(),
            description: Some(format!("Notes for {title}.")),
            season: Some(1),
            episode_number: Some(n),
            episode_type: EpisodeType::Full,
            published_at_ms: Some(1_718_409_600_000),
            duration_ms: Some(2_700_000),
            audio_url: format!("https://example.test/{n}.mp3"),
            mime: Some("audio/mpeg".into()),
            byte_len: None,
            image_url: None,
            link: None,
        },
        progress: status.map(|(status, position_ms)| ProgressRecord {
            episode_id,
            podcast_id: podcast,
            position_ms,
            status,
            generation: 1,
            session_id: "s".into(),
            hlc_ms: 1,
            device_id: "dev".into(),
            op_id: "dev:1".into(),
        }),
        completed_count: 0,
        in_feed: true,
    }
}

pub fn ready_state(rows: Vec<PodcastRow>) -> AppState {
    let mut state = AppState {
        active_tab: Tab::Podcasts,
        ..AppState::default()
    };
    state.podcasts.store = PodcastStoreStatus::Ready;
    state.podcasts.list = Some(Loadable::Ready(rows));
    state
}

pub fn with_episodes(mut state: AppState, items: Vec<EpisodeListItem>) -> AppState {
    let season = furumi_library::podcasts::SeasonFilter::Any;
    state.podcasts.opened = Some(OpenedPodcast {
        podcast_id: podcast_id(),
        season_cursor: None,
        season: Some(season),
        episode_cursor: 0,
    });
    state.podcasts.episodes = Some(EpisodeList {
        seq: 1,
        query: EpisodeQuery::new(podcast_id()),
        items,
        loading: false,
        loading_more: false,
        complete: true,
        error: None,
        focus: None,
    });
    state
}

pub fn seasons() -> Vec<SeasonSummary> {
    vec![
        SeasonSummary {
            season: Some(1),
            episode_count: 3,
            unplayed_count: 1,
            first_published_at_ms: Some(1_609_459_200_000),
            last_published_at_ms: Some(1_640_995_200_000),
        },
        SeasonSummary {
            season: None,
            episode_count: 2,
            unplayed_count: 0,
            first_published_at_ms: None,
            last_published_at_ms: None,
        },
    ]
}
