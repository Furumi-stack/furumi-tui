//! Public podcast cards: their order in the search results, the "already
//! subscribed" mark, the feed preview, subscribing from a card, and an
//! end-to-end run over two loopback discovery nodes with a local feed
//! server (no internet, temp directories only).

use std::sync::Arc;
use std::time::Duration;

use furumi_library::podcasts::PodcastListFilter;
use music_dht::NetworkId;
use music_dht::podcast::{FeedUrl, PodcastId};
use music_dht::podcast_dht::{ClaimedFeedUrl, FederatedSearchHit, SearchHitKind};

use super::discovery::{
    PODCAST_SEARCH_TIMEOUT, PodcastHit, podcast_hits, preview, preview_for, resolve_local_ids,
    search, subscribe_from_hit, test_hit,
};
use super::publication::{Publication, check_share, reconcile, set_shared};
use super::publication_tests::{Node, wait_for};
use super::refresh::{refresh, subscribe};
use super::test_server::Route;
use super::tests::{NOW, fixture, rss};

#[test]
fn podcast_hits_put_exact_titles_first_and_keep_only_podcasts() {
    let other = test_hit("https://feeds.example.test/weekly.xml", "Rust Radio Weekly");
    let exact = test_hit("https://feeds.example.test/radio.xml", "Rust Radio");
    let hits = podcast_hits("rust radio", Vec::new(), vec![other.clone(), exact.clone()]);
    let ids: Vec<&PodcastId> = hits.iter().map(|hit| &hit.hit.podcast_id).collect();
    assert_eq!(ids, [&exact.podcast_id, &other.podcast_id]);
    // Unresolved hits use the card's id; the type marker is "podcast".
    assert!(hits.iter().all(|hit| hit.local_id == hit.hit.podcast_id));
    assert_eq!(hits[0].hit.kind(), SearchHitKind::Podcast);
    assert_eq!(
        FederatedSearchHit::Podcast(exact).kind().as_str(),
        "podcast"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn podcast_search_is_bounded_and_empty_without_the_overlay() {
    // Federation off (no overlay): no hits, nothing to wait for.
    assert!(
        search(None, "rust radio", PODCAST_SEARCH_TIMEOUT)
            .await
            .is_empty()
    );

    // An overlay that knows no overlay peers bootstraps from its music-DHT
    // contacts before it searches, outside its own lookup deadline. With a
    // contact that no longer answers, the search still ends at the bound.
    let temp = tempfile::tempdir().unwrap();
    let network = NetworkId::from_name("furumi-tui-podcast-search-bound-test");
    let a = Node::start(&temp.path().join("a"), network).await;
    let gone = Node::start(&temp.path().join("gone"), network).await;
    a.service
        .connect(gone.service.ticket().await.unwrap())
        .await
        .unwrap();
    wait_for("a music DHT contact", async || {
        (!a.service.known_peers().is_empty()).then_some(())
    })
    .await;
    gone.stop().await;
    assert_eq!(a.dht.known_peers(), 0);
    let bound = Duration::from_millis(500);
    let started = std::time::Instant::now();
    let hits = search(Some(&a.dht), "rust radio", bound).await;
    let elapsed = started.elapsed();
    assert!(hits.is_empty());
    assert!(elapsed < bound + Duration::from_secs(1), "took {elapsed:?}");
    a.stop().await;
}

#[tokio::test]
async fn subscribing_from_a_card_matches_subscribing_by_url() {
    let by_url = fixture();
    let from_card = fixture();
    by_url.server.set(
        "/feed.xml",
        Route::new(
            rss(&[("a", "Pilot"), ("b", "Second")]),
            "application/rss+xml",
        ),
    );
    let url = by_url.server.url("/feed.xml");

    let typed = subscribe(&by_url.services, &url, NOW).await.unwrap();
    let hit = test_hit(&url, "A card title that is only a claim");
    let report = subscribe_from_hit(&from_card.services, &hit, NOW)
        .await
        .unwrap();
    assert_eq!(report.podcast_id, typed.podcast_id);
    assert_eq!(report.podcast_id, hit.podcast_id);
    assert_eq!(report.new_episodes, 2);
    // Metadata comes from the feed, never from the card.
    assert_eq!(report.title, "Test Show");
    // A private subscription: no share decision was written.
    assert_eq!(from_card.log.kinds(), ["podcast_subscription_set"]);
    let store = &from_card.services.store;
    assert_eq!(store.share(&report.podcast_id).unwrap(), None);
    let rows = store.list_podcasts(PodcastListFilter::Subscribed).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].shared);
    assert_eq!(rows[0].meta.title, "Test Show");
    // Subscribing again from the card changes nothing.
    let again = subscribe_from_hit(&from_card.services, &hit, NOW + 1)
        .await
        .unwrap();
    assert!(again.already_subscribed);
    assert_eq!(from_card.log.kinds().len(), 1);
}

#[tokio::test]
async fn subscribing_from_a_card_never_fetches_a_claimed_url() {
    let f = fixture();
    // The origin URL moved permanently; a publisher also claims a URL of its
    // own choosing.
    f.server.set(
        "/origin.xml",
        Route::redirect(301, &f.server.url("/moved.xml")),
    );
    f.server.set(
        "/moved.xml",
        Route::new(rss(&[("a", "Pilot")]), "application/rss+xml"),
    );
    f.server.set(
        "/claimed.xml",
        Route::new(rss(&[("x", "Imposter")]), "application/rss+xml"),
    );
    let origin = FeedUrl::parse(&f.server.url("/origin.xml")).unwrap();
    let moved = FeedUrl::parse(&f.server.url("/moved.xml")).unwrap();
    let claimed = FeedUrl::parse(&f.server.url("/claimed.xml")).unwrap();
    let mut hit = test_hit(origin.as_str(), "Rust Radio");
    hit.claimed_feed_urls = vec![ClaimedFeedUrl {
        feed_url: claimed.clone(),
        publishers: hit.publishers.clone(),
    }];
    hit.card.feed_url = claimed.clone();
    hit.card.id_feed_url = Some(origin.clone());

    let report = subscribe_from_hit(&f.services, &hit, NOW).await.unwrap();
    assert_eq!(report.podcast_id, PodcastId::from_feed_url(&origin));
    assert!(f.server.requests("/claimed.xml").is_empty());
    let aliases = f.services.store.feed_aliases(&report.podcast_id).unwrap();
    assert!(
        aliases.contains(&moved),
        "the feed's own redirect is an alias"
    );
    assert!(
        !aliases.contains(&claimed),
        "a claim never becomes an alias"
    );

    // A card whose id is not the hash of its origin URL is refused before
    // anything is fetched or stored.
    let other = fixture();
    let mut forged = test_hit(&f.server.url("/moved.xml"), "Rust Radio");
    forged.podcast_id = hit.podcast_id.clone();
    let requests = f.server.requests("/moved.xml").len();
    assert!(
        subscribe_from_hit(&other.services, &forged, NOW)
            .await
            .is_err()
    );
    assert_eq!(f.server.requests("/moved.xml").len(), requests);
    assert!(other.log.kinds().is_empty());
}

#[tokio::test]
async fn hits_resolve_to_the_podcast_a_feed_alias_belongs_to() {
    let f = fixture();
    f.server
        .set("/old.xml", Route::redirect(301, &f.server.url("/new.xml")));
    f.server.set(
        "/new.xml",
        Route::new(rss(&[("a", "Pilot")]), "application/rss+xml"),
    );
    let subscribed = subscribe(&f.services, &f.server.url("/old.xml"), NOW)
        .await
        .unwrap();
    // Somebody else subscribed with the new address and shares that card.
    let mut hits = vec![
        PodcastHit::new(test_hit(&f.server.url("/new.xml"), "Test Show")),
        PodcastHit::new(test_hit(
            "https://feeds.example.test/unknown.xml",
            "Unknown",
        )),
    ];
    assert_ne!(hits[0].hit.podcast_id, subscribed.podcast_id);
    resolve_local_ids(&f.services.store, &mut hits).unwrap();
    assert_eq!(hits[0].local_id, subscribed.podcast_id);
    assert!(hits[0].ids().contains(&&subscribed.podcast_id));
    assert_eq!(hits[1].local_id, hits[1].hit.podcast_id);
}

#[tokio::test]
async fn a_card_for_an_older_address_finds_the_subscription_under_the_newer_one() {
    use furumi_library::podcasts::PodcastListFilter;
    // The user follows the feed under its newer address; a long-standing
    // publisher's card names the older one (the card's origin).
    let f = fixture();
    let feed = |announce: Option<&str>| {
        let announce = announce
            .map(|url| format!("<itunes:new-feed-url>{url}</itunes:new-feed-url>"))
            .unwrap_or_default();
        format!(
            "<?xml version=\"1.0\"?><rss version=\"2.0\" \
             xmlns:itunes=\"http://www.itunes.com/dtds/podcast-1.0.dtd\"><channel>\
             <title>Test Show</title>{announce}{}</channel></rss>",
            item("a", "Pilot", 1, "Sat, 01 Jun 2024 10:00:00 GMT"),
        )
    };
    let subscribed_rows = || {
        f.services
            .store
            .list_podcasts(PodcastListFilter::Subscribed)
            .unwrap()
            .len()
    };

    // 1. The old address redirects permanently to the new one.
    f.server
        .set("/old.xml", Route::redirect(301, &f.server.url("/new.xml")));
    f.server
        .set("/new.xml", Route::new(feed(None), "application/rss+xml"));
    let followed = subscribe(&f.services, &f.server.url("/new.xml"), NOW)
        .await
        .unwrap();
    let card = test_hit(&f.server.url("/old.xml"), "Test Show");
    let url = card.feed_url.clone();
    let preview = preview_for(&f.services, &url).await.unwrap();
    assert_eq!(preview.subscribed_as.as_ref(), Some(&followed.podcast_id));
    let report = subscribe_from_hit(&f.services, &card, NOW).await.unwrap();
    assert!(report.already_subscribed);
    assert_eq!(report.podcast_id, followed.podcast_id);
    assert_eq!(subscribed_rows(), 1);

    // 2. The old address only announces the new one (itunes:new-feed-url,
    //    no redirect): no second subscription for the same podcast.
    let new_url = f.server.url("/new2.xml");
    f.server
        .set("/new2.xml", Route::new(feed(None), "application/rss+xml"));
    f.server.set(
        "/old2.xml",
        Route::new(feed(Some(&new_url)), "application/rss+xml"),
    );
    let followed = subscribe(&f.services, &new_url, NOW).await.unwrap();
    let kinds = f.log.kinds().len();
    let card = test_hit(&f.server.url("/old2.xml"), "Test Show");
    let url = card.feed_url.clone();
    let preview = preview_for(&f.services, &url).await.unwrap();
    assert_eq!(preview.moved_to, Some(FeedUrl::parse(&new_url).unwrap()));
    assert_eq!(preview.subscribed_as.as_ref(), Some(&followed.podcast_id));
    let report = subscribe_from_hit(&f.services, &card, NOW).await.unwrap();
    assert!(report.already_subscribed);
    assert_eq!(report.podcast_id, followed.podcast_id);
    assert_eq!(subscribed_rows(), 2, "one row per podcast");
    assert_eq!(f.log.kinds().len(), kinds, "nothing was written");

    // A card whose feed moved to an address this device does not follow
    // is not marked.
    f.server.set(
        "/elsewhere.xml",
        Route::new(
            feed(Some("https://feeds.example.test/unknown.xml")),
            "application/rss+xml",
        ),
    );
    let url = FeedUrl::parse(&f.server.url("/elsewhere.xml")).unwrap();
    let preview = preview_for(&f.services, &url).await.unwrap();
    assert!(preview.moved_to.is_some());
    assert_eq!(preview.subscribed_as, None);
}

#[tokio::test]
async fn preview_reads_seasons_and_episodes_without_storing_anything() {
    let f = fixture();
    let new_url = "https://feeds.example.test/new-home.xml";
    let feed = format!(
        "<?xml version=\"1.0\"?><rss version=\"2.0\" \
         xmlns:itunes=\"http://www.itunes.com/dtds/podcast-1.0.dtd\"><channel>\
         <title>Feed Title</title><itunes:new-feed-url>{new_url}</itunes:new-feed-url>\
         {}{}{}{}</channel></rss>",
        item("a", "One", 1, "Sat, 01 Jun 2024 10:00:00 GMT"),
        item("b", "Two", 1, "Sat, 08 Jun 2024 10:00:00 GMT"),
        item("c", "Three", 2, "Sat, 15 Jun 2024 10:00:00 GMT"),
        "<item><title>Bonus</title><guid>d</guid>\
         <pubDate>Sat, 22 Jun 2024 10:00:00 GMT</pubDate>\
         <enclosure url=\"/d.mp3\" type=\"audio/mpeg\"/></item>",
    );
    f.server
        .set("/feed.xml", Route::new(feed, "application/rss+xml"));
    let url = FeedUrl::parse(&f.server.url("/feed.xml")).unwrap();
    let preview = preview(&f.services.fetcher, &url).await.unwrap();
    assert_eq!(preview.title, "Feed Title");
    assert_eq!(preview.episode_count, 4);
    assert_eq!(preview.seasons, [(Some(1), 2), (Some(2), 1), (None, 1)]);
    let titles: Vec<&str> = preview.episodes.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(titles, ["Bonus", "Three", "Two", "One"]);
    assert_eq!(preview.episodes[1].season, Some(2));
    assert_eq!(preview.episodes[1].duration_ms, Some(45 * 60 * 1000));
    assert_eq!(
        preview.moved_to,
        Some(FeedUrl::parse(new_url).unwrap()),
        "the feed announced a new address"
    );
    // Nothing was stored: no podcast, no fetch state, no operation.
    let store = &f.services.store;
    assert!(
        store
            .list_podcasts(PodcastListFilter::All)
            .unwrap()
            .is_empty()
    );
    assert!(f.log.kinds().is_empty());
}

fn item(guid: &str, title: &str, season: u32, date: &str) -> String {
    format!(
        "<item><title>{title}</title><guid>{guid}</guid><pubDate>{date}</pubDate>\
         <itunes:season>{season}</itunes:season><itunes:duration>45:00</itunes:duration>\
         <enclosure url=\"/{guid}.mp3\" type=\"audio/mpeg\"/></item>"
    )
}

// ---------------------------------------------------------------------------
// Two loopback nodes and a local feed server

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stranger_finds_a_shared_card_subscribes_and_refreshes_from_the_feed() {
    tokio::time::timeout(Duration::from_secs(240), stranger_subscribes())
        .await
        .unwrap();
}

async fn stranger_subscribes() {
    let temp = tempfile::tempdir().unwrap();
    let network = NetworkId::from_name("furumi-tui-podcast-discovery-test");
    // A shares a podcast. B is another user's TUI outside A's
    // trusted-device group: no device sync between them, only the overlay.
    let a = Node::start(&temp.path().join("a"), network).await;
    let b = Node::start(&temp.path().join("b"), network).await;
    b.service
        .connect(a.service.ticket().await.unwrap())
        .await
        .unwrap();
    wait_for("music DHT contacts", async || {
        (!a.service.known_peers().is_empty() && !b.service.known_peers().is_empty()).then_some(())
    })
    .await;
    for node in [&a, &b] {
        node.dht.bootstrap().await.unwrap();
    }
    wait_for("overlay contacts", async || {
        (a.dht.known_peers() >= 1 && b.dht.known_peers() >= 1).then_some(())
    })
    .await;

    // The podcast's own host (a local HTTP server).
    let user_a = fixture();
    let user_b = fixture();
    let feed = &user_a.server;
    feed.set(
        "/show.xml",
        Route::new(
            rss(&[("a", "Pilot"), ("b", "Second")]),
            "application/rss+xml",
        ),
    );
    let feed_url = FeedUrl::parse(&feed.url("/show.xml")).unwrap();
    let private_url = feed.url("/diary.xml");
    feed.set(
        "/diary.xml",
        Route::new(rss(&[("p", "Private entry")]), "application/rss+xml"),
    );

    // A subscribes to two podcasts and shares one. The local address looks
    // private, so A confirms exactly these URLs first (the dialog).
    let shared = subscribe(&user_a.services, feed_url.as_str(), NOW)
        .await
        .unwrap();
    let private = subscribe(&user_a.services, &private_url, NOW)
        .await
        .unwrap();
    let store_a = Arc::clone(&user_a.services.store);
    let confirmations = Arc::clone(&user_a.services.confirmations);
    let check = check_share(&store_a, &confirmations, &shared.podcast_id).unwrap();
    set_shared(
        &store_a,
        &confirmations,
        &shared.podcast_id,
        true,
        check.needs_confirmation().then_some(check.urls),
        || {
            user_a
                .services
                .log
                .record(&mut |stamps| store_a.set_shared(stamps, &shared.podcast_id, true))
        },
    )
    .unwrap();
    let report = reconcile(
        &a.dht,
        Some(Arc::clone(&store_a)),
        Arc::clone(&confirmations),
    )
    .await;
    assert_eq!(report.error, None);
    assert_eq!(
        report.podcasts.get(&shared.podcast_id),
        Some(&Publication::Published)
    );

    // B searches like the search view does: overlay search, merged order,
    // resolved against B's own store.
    let mut hits = wait_for("the shared card", async || {
        let found = search(Some(&b.dht), "test show", PODCAST_SEARCH_TIMEOUT).await;
        (!found.is_empty()).then_some(found)
    })
    .await;
    assert_eq!(hits.len(), 1, "the private subscription is not found");
    resolve_local_ids(&user_b.services.store, &mut hits).unwrap();
    let hit = hits.remove(0);
    assert_eq!(hit.hit.kind(), SearchHitKind::Podcast);
    assert_eq!(hit.hit.podcast_id, shared.podcast_id);
    assert_eq!(hit.hit.feed_url, feed_url);
    assert_eq!(hit.hit.publishers, vec![a.service.endpoint_id()]);
    assert_eq!(hit.local_id, shared.podcast_id);
    assert!(
        b.dht
            .lookup_podcast(&private.podcast_id)
            .await
            .unwrap()
            .is_none()
    );

    // "Подписаться": B reads the feed from its host, not from A.
    let before = feed.requests("/show.xml").len();
    let subscribed = subscribe_from_hit(&user_b.services, &hit.hit, NOW)
        .await
        .unwrap();
    assert_eq!(subscribed.podcast_id, shared.podcast_id, "same podcast id");
    assert_eq!(subscribed.new_episodes, 2);
    assert_eq!(feed.requests("/show.xml").len(), before + 1);
    let store_b = &user_b.services.store;
    assert_eq!(store_b.share(&subscribed.podcast_id).unwrap(), None);
    assert_eq!(user_b.log.kinds(), ["podcast_subscription_set"]);
    assert!(
        b.dht.published().await.unwrap().is_empty(),
        "B publishes nothing"
    );

    // B refreshes on its own from the feed.
    feed.set(
        "/show.xml",
        Route::new(
            rss(&[("a", "Pilot"), ("b", "Second"), ("c", "Third")]),
            "application/rss+xml",
        ),
    );
    let refreshed = refresh(&user_b.services, &subscribed.podcast_id, &feed_url, NOW + 1)
        .await
        .unwrap();
    assert_eq!(refreshed.new_episodes, 1);

    // A stops sharing: the card disappears for B, B's subscription stays.
    user_a
        .services
        .log
        .record(&mut |stamps| store_a.set_shared(stamps, &shared.podcast_id, false))
        .unwrap();
    let report = reconcile(&a.dht, Some(Arc::clone(&store_a)), confirmations).await;
    assert_eq!(report.podcasts.get(&shared.podcast_id), None);
    assert!(
        b.dht
            .lookup_podcast(&shared.podcast_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        search(Some(&b.dht), "test show", PODCAST_SEARCH_TIMEOUT)
            .await
            .is_empty()
    );
    let rows = store_b
        .list_podcasts(PodcastListFilter::Subscribed)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].podcast_id, shared.podcast_id);
    assert!(!rows[0].shared);
    let refreshed = refresh(&user_b.services, &subscribed.podcast_id, &feed_url, NOW + 2)
        .await
        .unwrap();
    assert_eq!(refreshed.new_episodes, 0);

    a.stop().await;
    b.stop().await;
}
