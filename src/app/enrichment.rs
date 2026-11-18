//! Scheduling for local cards enriched by replaceable federation data.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::state::{AppState, GlobalView, Loadable, Tab};

const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Card {
    Artist(i64),
    Release(i64),
}

#[derive(Debug, Default)]
pub(super) struct RefreshState {
    visible: Option<Card>,
    /// None means a request is in flight; Some is its completion time.
    requests: HashMap<Card, Option<Instant>>,
}

impl RefreshState {
    pub(super) fn is_loading(&self, card: Card) -> bool {
        matches!(self.requests.get(&card), Some(None))
    }

    fn begin(&mut self, visible: Option<Card>, now: Instant) -> bool {
        let reopened = self.visible != visible;
        self.visible = visible;
        let Some(card) = visible else { return false };
        match self.requests.get(&card) {
            Some(None) => return false,
            Some(Some(completed))
                if !reopened && now.duration_since(*completed) < REFRESH_INTERVAL =>
            {
                return false;
            }
            _ => {}
        }
        self.requests.insert(card, None);
        true
    }

    pub(super) fn complete(&mut self, card: Card, now: Instant) {
        self.requests.insert(card, Some(now));
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Request {
    Artist {
        id: i64,
        name: String,
    },
    Release {
        id: i64,
        title: String,
        artists: Vec<String>,
    },
}

/// Called on navigation and maintenance ticks. A cached success or failure
/// must not suppress discovery for the rest of the session.
pub(super) fn prepare(state: &mut AppState, now: Instant) -> Option<Request> {
    let visible = if state.federation.settings.enabled
        && state.global.filters.source_mode.includes_network()
        && state.active_tab == Tab::Global
    {
        match state.global.stack.last() {
            Some(GlobalView::Artist { id, .. }) => Some(Card::Artist(*id)),
            Some(GlobalView::Release { id, .. }) => Some(Card::Release(*id)),
            _ => None,
        }
    } else {
        None
    };
    let request = match visible {
        Some(Card::Artist(id)) => match state.artist_views.get(&id) {
            Some(Loadable::Ready(detail)) => Some(Request::Artist {
                id,
                name: detail.name.clone(),
            }),
            _ => None,
        },
        Some(Card::Release(id)) => match state.release_views.get(&id) {
            Some(Loadable::Ready(detail)) => {
                let mut artists = Vec::new();
                for artist in &detail.artists {
                    if !artists.iter().any(|name: &String| {
                        music_dht::normalize_name(name) == music_dht::normalize_name(&artist.name)
                    }) {
                        artists.push(artist.name.clone());
                    }
                }
                (!artists.is_empty()).then(|| Request::Release {
                    id,
                    title: detail.title.clone(),
                    artists,
                })
            }
            _ => None,
        },
        None => None,
    };
    if !state
        .card_enrichment
        .begin(visible.filter(|_| request.is_some()), now)
    {
        return None;
    }
    match &request {
        Some(Request::Artist { id, .. }) => mark_loading(&mut state.artist_fed_views, *id),
        Some(Request::Release { id, .. }) => mark_loading(&mut state.release_fed_views, *id),
        None => {}
    }
    request
}

fn mark_loading<T>(views: &mut HashMap<i64, Loadable<T>>, id: i64) {
    if !matches!(views.get(&id), Some(Loadable::Ready(_))) {
        views.insert(id, Loadable::Loading);
    }
}

/// Keep the last usable snapshot if a refresh fails while peers are offline.
pub(super) fn store_result<T>(
    views: &mut HashMap<i64, Loadable<T>>,
    id: i64,
    result: Result<T, String>,
) {
    match result {
        Ok(value) => {
            views.insert(id, Loadable::Ready(value));
        }
        Err(message) if !matches!(views.get(&id), Some(Loadable::Ready(_))) => {
            views.insert(id, Loadable::Failed(message));
        }
        Err(_) => {}
    }
}
