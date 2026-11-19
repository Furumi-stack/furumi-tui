//! Public podcast cards opened from the search results.
//!
//! The card shows what its publishers claim. "Подписаться" subscribes with
//! the card's origin feed URL only (`Effect::PodcastSubscribeFromCard`); the
//! subscription is private and its metadata comes from the feed. When this
//! device is already subscribed (under the card's id or a feed alias), the
//! action reads «Уже подписан» and Enter opens the podcast instead.

use crossterm::event::KeyCode;
use music_dht::podcast::PodcastId;

use super::Effect;
use super::podcasts::{display_title, reveal_podcast, store_ready};
use crate::app::state::{AppState, Loadable, Popup};
use crate::podcasts::discovery::{FeedPreview, PodcastHit};

/// Lines a page key scrolls the card by.
const PAGE: u16 = 10;

/// Enter on a podcast hit of the search results.
pub fn open_card(state: &mut AppState, index: usize) {
    let Some(hit) = state.search.fed_podcasts.get(index) else {
        return;
    };
    state.popup = Some(Popup::PodcastCard {
        hit: Box::new(hit.clone()),
        preview: None,
        scroll: 0,
    });
}

/// A key in [`Popup::PodcastCard`] (the popup was taken out of the state):
/// Enter/s subscribes (or opens the subscribed podcast), `e` reads the
/// episodes from the feed, arrows and page keys scroll, Esc/q closes.
pub fn card_key(state: &mut AppState, popup: Popup, key: KeyCode) -> Option<Effect> {
    let Popup::PodcastCard {
        hit,
        mut preview,
        mut scroll,
    } = popup
    else {
        state.popup = Some(popup);
        return None;
    };
    let mut effect = None;
    match key {
        KeyCode::Esc | KeyCode::Char('q') => return None,
        KeyCode::Enter | KeyCode::Char('s') => {
            if let Some(id) = subscribed_id(state, &hit) {
                // «Уже подписан»: show the subscription instead.
                return reveal_podcast(state, id, hit.title());
            }
            effect = subscribe(state, &hit);
        }
        KeyCode::Char('e') => {
            if !matches!(preview, Some(Loadable::Loading | Loadable::Ready(_))) {
                preview = Some(Loadable::Loading);
                effect = Some(Effect::PodcastPreview {
                    podcast_id: hit.hit.podcast_id.clone(),
                    feed_url: hit.hit.feed_url.clone(),
                });
            }
        }
        KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => scroll = scroll.saturating_add(1),
        KeyCode::PageUp => scroll = scroll.saturating_sub(PAGE),
        KeyCode::PageDown => scroll = scroll.saturating_add(PAGE),
        KeyCode::Home | KeyCode::Char('g') => scroll = 0,
        _ => {}
    }
    state.popup = Some(Popup::PodcastCard {
        hit,
        preview,
        scroll,
    });
    effect
}

/// The id under which this device is subscribed to the card's podcast.
fn subscribed_id(state: &AppState, hit: &PodcastHit) -> Option<PodcastId> {
    hit.ids()
        .into_iter()
        .find(|id| state.podcasts.podcast(id).is_some())
        .cloned()
}

fn subscribe(state: &mut AppState, hit: &PodcastHit) -> Option<Effect> {
    if !store_ready(state) {
        return None;
    }
    if state.podcasts.subscribing {
        state.status_message = Some("podcasts: a subscription is already being added".into());
        return None;
    }
    state.podcasts.subscribing = true;
    state.status_message = Some(format!(
        "podcasts: reading the feed of {}…",
        display_title(hit.title())
    ));
    Some(Effect::PodcastSubscribeFromCard(Box::new(hit.hit.clone())))
}

/// The preview of an open card arrived (ignored when another card is open).
pub fn preview_loaded(
    state: &mut AppState,
    podcast_id: PodcastId,
    result: Result<Box<FeedPreview>, String>,
) {
    if let Some(Popup::PodcastCard { hit, preview, .. }) = &mut state.popup
        && hit.hit.podcast_id == podcast_id
    {
        *preview = Some(match result {
            Ok(feed) => Loadable::Ready(*feed),
            Err(err) => Loadable::Failed(err),
        });
    }
}
