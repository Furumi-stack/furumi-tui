//! Store opening, subscribing and refreshing against a local HTTP server.
//! No internet, no real user directories: every test uses a temp dir.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Result;
use furumi_library::podcasts::{
    EpisodeQuery, OpStamp, PodcastListFilter, PodcastLocalOp, PodcastStore, StampSource,
};
use music_dht::device_sync::{SyncOpPayload, SyncOpWire};
use music_dht::podcast::{FeedUrl, PodcastId};

use super::OpLog;
use super::fetch::{FETCH_DEADLINE, MAX_COVER_BYTES, MAX_FEED_BYTES, MAX_REDIRECTS, Validators};
use super::refresh::{
    FAILURE_BACKOFF_BASE, FAILURE_BACKOFF_MAX, REFRESH_INTERVAL, Services,
    next_check_after_failure, next_check_after_success, refresh, subscribe,
};
use super::test_server::{Route, TestServer};

pub(super) const NOW: i64 = 1_700_000_000_000;

/// In-memory sync log: hands out stamps and keeps the appended operations.
#[derive(Default)]
pub(super) struct MemoryLog {
    ops: Mutex<Vec<SyncOpWire>>,
}

struct MemoryStamps {
    next_seq: i64,
    reserved: Vec<i64>,
}

impl StampSource for MemoryStamps {
    fn device_id(&self) -> &str {
        "test-device"
    }

    fn next_stamp(&mut self) -> Result<OpStamp> {
        let seq = self.next_seq + self.reserved.len() as i64;
        self.reserved.push(seq);
        Ok(OpStamp::new(
            format!("test-device:{seq}"),
            "test-device",
            NOW + seq,
        ))
    }
}

impl OpLog for MemoryLog {
    fn record(
        &self,
        action: &mut dyn FnMut(&mut dyn StampSource) -> Result<Vec<PodcastLocalOp>>,
    ) -> Result<()> {
        let mut ops = self.ops.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stamps = MemoryStamps {
            next_seq: ops.len() as i64 + 1,
            reserved: Vec::new(),
        };
        for (op, seq) in action(&mut stamps)?.into_iter().zip(stamps.reserved) {
            ops.push(op.into_wire(seq));
        }
        Ok(())
    }
}

impl MemoryLog {
    pub(super) fn kinds(&self) -> Vec<String> {
        self.ops
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|op| {
                serde_json::to_value(&op.payload).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }
}

pub(super) struct Fixture {
    _dir: tempfile::TempDir,
    pub(super) services: Services,
    pub(super) log: Arc<MemoryLog>,
    pub(super) server: TestServer,
}

pub(super) fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryLog::default());
    let services = super::open_services(
        &dir.path().join("podcasts.sqlite3"),
        dir.path().join("covers"),
        log.clone(),
    )
    .unwrap();
    Fixture {
        _dir: dir,
        services,
        log,
        server: TestServer::start(),
    }
}

pub(super) fn rss(items: &[(&str, &str)]) -> Vec<u8> {
    let items: String = items
        .iter()
        .map(|(guid, title)| {
            format!(
                "<item><title>{title}</title><guid>{guid}</guid>\
                 <pubDate>Sat, 15 Jun 2024 10:00:00 GMT</pubDate>\
                 <itunes:duration>45:00</itunes:duration>\
                 <enclosure url=\"/audio/{guid}.mp3\" type=\"audio/mpeg\" length=\"100\"/></item>"
            )
        })
        .collect();
    format!(
        "<?xml version=\"1.0\"?><rss version=\"2.0\" \
         xmlns:itunes=\"http://www.itunes.com/dtds/podcast-1.0.dtd\"><channel>\
         <title>Test Show</title><itunes:author>Host</itunes:author>{items}</channel></rss>"
    )
    .into_bytes()
}

fn episode_count(store: &PodcastStore, id: &PodcastId) -> usize {
    store
        .episodes(&EpisodeQuery::new(id.clone()))
        .unwrap()
        .len()
}

#[tokio::test]
async fn subscribe_then_conditional_refresh_uses_validators() {
    let f = fixture();
    f.server.set(
        "/feed.xml",
        Route::new(
            rss(&[("a", "Pilot"), ("b", "Second")]),
            "application/rss+xml",
        )
        .etag("\"v1\"")
        .last_modified("Sat, 15 Jun 2024 10:00:00 GMT"),
    );
    let report = subscribe(&f.services, &f.server.url("/feed.xml"), NOW)
        .await
        .unwrap();
    assert!(!report.already_subscribed);
    assert_eq!(report.new_episodes, 2);
    assert_eq!(report.title, "Test Show");
    assert_eq!(f.log.kinds(), ["podcast_subscription_set"]);
    let store = &f.services.store;
    let rows = store.list_podcasts(PodcastListFilter::Subscribed).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].shared, "a new subscription is private");
    let state = store.fetch_state(&report.podcast_id).unwrap();
    assert_eq!(state.etag.as_deref(), Some("\"v1\""));
    assert_eq!(state.next_check_at_ms, Some(next_check_after_success(NOW)));
    let first = &f.server.requests("/feed.xml")[0];
    assert!(!first.contains_key("if-none-match"), "{first:?}");
    assert!(
        first
            .get("user-agent")
            .is_some_and(|agent| agent.starts_with("furumi/")),
        "{first:?}"
    );

    // Unchanged: 304, nothing re-parsed, validators sent.
    let url = rows[0].meta.feed_url.clone();
    let checked = refresh(&f.services, &report.podcast_id, &url, NOW + 1)
        .await
        .unwrap();
    assert!(checked.not_modified);
    let second = &f.server.requests("/feed.xml")[1];
    assert_eq!(
        second.get("if-none-match").map(String::as_str),
        Some("\"v1\"")
    );
    assert_eq!(
        second.get("if-modified-since").map(String::as_str),
        Some("Sat, 15 Jun 2024 10:00:00 GMT")
    );

    // Changed: a new episode arrives, the known ones are not duplicated.
    f.server.set(
        "/feed.xml",
        Route::new(
            rss(&[("a", "Pilot (remastered)"), ("b", "Second"), ("c", "Third")]),
            "application/rss+xml",
        )
        .etag("\"v2\""),
    );
    let changed = refresh(&f.services, &report.podcast_id, &url, NOW + 2)
        .await
        .unwrap();
    assert!(!changed.not_modified);
    assert_eq!(changed.new_episodes, 1);
    assert_eq!(changed.updated_episodes, 1);
    assert_eq!(episode_count(store, &report.podcast_id), 3);
    // A refresh never writes personal state: no new operation.
    assert_eq!(f.log.kinds(), ["podcast_subscription_set"]);

    // Subscribing again is recognised without another fetch.
    let requests = f.server.requests("/feed.xml").len();
    let again = subscribe(&f.services, &f.server.url("/feed.xml"), NOW + 3)
        .await
        .unwrap();
    assert!(again.already_subscribed);
    assert_eq!(again.podcast_id, report.podcast_id);
    assert_eq!(f.server.requests("/feed.xml").len(), requests);
}

#[tokio::test]
async fn permanent_redirect_becomes_an_alias_and_the_new_fetch_url() {
    let f = fixture();
    f.server
        .set("/old.xml", Route::redirect(301, &f.server.url("/new.xml")));
    f.server.set(
        "/new.xml",
        Route::new(rss(&[("a", "Pilot")]), "application/rss+xml"),
    );
    let old = FeedUrl::parse(&f.server.url("/old.xml")).unwrap();
    let new = FeedUrl::parse(&f.server.url("/new.xml")).unwrap();
    let report = subscribe(&f.services, old.as_str(), NOW).await.unwrap();
    // The id comes from the URL of the first subscription.
    assert_eq!(report.podcast_id, PodcastId::from_feed_url(&old));
    assert_eq!(
        f.log.kinds(),
        ["podcast_subscription_set", "podcast_feed_aliases_added"]
    );
    let store = &f.services.store;
    assert!(
        store
            .feed_aliases(&report.podcast_id)
            .unwrap()
            .contains(&new)
    );
    let summary = store.podcast(&report.podcast_id).unwrap().unwrap();
    assert_eq!(summary.meta.feed_url, new, "fetched from the new URL now");
    assert_eq!(store.resolve_podcast_id(&new).unwrap(), report.podcast_id);

    // The next refresh goes straight to the new URL; no new alias op.
    let checked = refresh(&f.services, &report.podcast_id, &new, NOW + 1)
        .await
        .unwrap();
    assert!(checked.moved_to.is_none());
    assert_eq!(f.server.requests("/old.xml").len(), 1);
    assert_eq!(f.log.kinds().len(), 2);

    // Subscribing through the new URL finds the same podcast.
    let again = subscribe(&f.services, new.as_str(), NOW + 2).await.unwrap();
    assert!(again.already_subscribed);
    assert_eq!(again.podcast_id, report.podcast_id);
}

/// A feed that moved (301) to a host that answers through a temporary
/// redirect (302, a CDN or tracker) moves to the permanent target only; the
/// following checks start there and are conditional again.
#[tokio::test]
async fn a_permanent_move_behind_a_temporary_redirect_still_moves_the_feed() {
    let f = fixture();
    f.server.set(
        "/old.xml",
        Route::redirect(301, &f.server.url("/moved.xml")),
    );
    f.server
        .set("/moved.xml", Route::redirect(302, "/cdn/feed.xml"));
    f.server.set(
        "/cdn/feed.xml",
        Route::new(rss(&[("a", "Pilot")]), "application/rss+xml").etag("\"v1\""),
    );
    let old = FeedUrl::parse(&f.server.url("/old.xml")).unwrap();
    let moved = FeedUrl::parse(&f.server.url("/moved.xml")).unwrap();
    let report = subscribe(&f.services, old.as_str(), NOW).await.unwrap();
    let store = &f.services.store;
    let summary = store.podcast(&report.podcast_id).unwrap().unwrap();
    assert_eq!(
        summary.meta.feed_url, moved,
        "the permanent target, not the CDN"
    );
    assert!(
        store
            .feed_aliases(&report.podcast_id)
            .unwrap()
            .contains(&moved)
    );
    // Relative enclosures resolve against the document's own address.
    let episodes = store
        .episodes(&EpisodeQuery::new(report.podcast_id.clone()))
        .unwrap();
    assert_eq!(episodes[0].episode.audio_url, f.server.url("/audio/a.mp3"));

    // The next check starts at the new address and keeps its validators
    // through the temporary hop.
    let checked = refresh(&f.services, &report.podcast_id, &moved, NOW + 1)
        .await
        .unwrap();
    assert!(checked.not_modified);
    assert_eq!(f.server.requests("/old.xml").len(), 1);
    let cdn = f.server.requests("/cdn/feed.xml");
    assert_eq!(cdn.len(), 2);
    assert_eq!(
        cdn[1].get("if-none-match").map(String::as_str),
        Some("\"v1\"")
    );

    // A temporary hop first never moves the feed, whatever follows it.
    f.server.set(
        "/tracked.xml",
        Route::redirect(302, &f.server.url("/old.xml")),
    );
    let tracked = FeedUrl::parse(&f.server.url("/tracked.xml")).unwrap();
    let other = subscribe(&f.services, tracked.as_str(), NOW + 2)
        .await
        .unwrap();
    let summary = store.podcast(&other.podcast_id).unwrap().unwrap();
    assert_eq!(summary.meta.feed_url, tracked);
}

#[tokio::test]
async fn temporary_redirect_keeps_the_feed_url() {
    let f = fixture();
    f.server
        .set("/tracked.xml", Route::redirect(302, "/real.xml"));
    f.server.set(
        "/real.xml",
        Route::new(rss(&[("a", "Pilot")]), "application/rss+xml"),
    );
    let tracked = FeedUrl::parse(&f.server.url("/tracked.xml")).unwrap();
    let report = subscribe(&f.services, tracked.as_str(), NOW).await.unwrap();
    assert_eq!(f.log.kinds(), ["podcast_subscription_set"]);
    let summary = f
        .services
        .store
        .podcast(&report.podcast_id)
        .unwrap()
        .unwrap();
    assert_eq!(summary.meta.feed_url, tracked);
    // Episode URLs resolve against where the document was served from.
    let episodes = f
        .services
        .store
        .episodes(&EpisodeQuery::new(report.podcast_id.clone()))
        .unwrap();
    assert_eq!(episodes[0].episode.audio_url, f.server.url("/audio/a.mp3"));
}

#[tokio::test]
async fn malformed_feed_and_server_errors_back_off_and_keep_the_catalog() {
    let f = fixture();
    f.server.set(
        "/feed.xml",
        Route::new(
            rss(&[("a", "Pilot"), ("b", "Second")]),
            "application/rss+xml",
        ),
    );
    let report = subscribe(&f.services, &f.server.url("/feed.xml"), NOW)
        .await
        .unwrap();
    let id = report.podcast_id;
    let url = FeedUrl::parse(&f.server.url("/feed.xml")).unwrap();

    f.server.set(
        "/feed.xml",
        Route::new("<html><body>Moved to our app!</body></html>", "text/html"),
    );
    let err = refresh(&f.services, &id, &url, NOW + 1).await.unwrap_err();
    assert!(format!("{err:#}").contains("unusable feed"), "{err:#}");
    let store = &f.services.store;
    let state = store.fetch_state(&id).unwrap();
    assert_eq!(state.consecutive_failures, 1);
    assert!(
        state
            .last_error
            .as_deref()
            .unwrap()
            .contains("unusable feed")
    );
    assert_eq!(
        state.next_check_at_ms,
        Some(next_check_after_failure(NOW + 1, 1, None))
    );
    assert_eq!(episode_count(store, &id), 2, "an error keeps the catalog");

    f.server.set(
        "/feed.xml",
        Route::new("busy", "text/plain")
            .status(503)
            .retry_after("7200"),
    );
    let err = refresh(&f.services, &id, &url, NOW + 2).await.unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 503"), "{err:#}");
    let state = store.fetch_state(&id).unwrap();
    assert_eq!(state.consecutive_failures, 2);
    assert_eq!(state.next_check_at_ms, Some(NOW + 2 + 7_200_000));
    assert_eq!(episode_count(store, &id), 2);
    // A failing feed is not due before its back-off expires.
    assert!(
        store
            .podcasts_due_for_refresh(NOW + 3, 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .podcasts_due_for_refresh(NOW + 2 + 7_200_000, 10)
            .unwrap()
            .len(),
        1
    );

    // Recovery clears the error.
    f.server.set(
        "/feed.xml",
        Route::new(
            rss(&[("a", "Pilot"), ("b", "Second")]),
            "application/rss+xml",
        ),
    );
    refresh(&f.services, &id, &url, NOW + 3).await.unwrap();
    let state = store.fetch_state(&id).unwrap();
    assert_eq!(state.consecutive_failures, 0);
    assert!(state.last_error.is_none());
}

#[tokio::test]
async fn subscribing_to_a_non_feed_stores_nothing() {
    let f = fixture();
    f.server
        .set("/page", Route::new("<html>hello</html>", "text/html"));
    let err = subscribe(&f.services, &f.server.url("/page"), NOW)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("unusable feed"), "{err:#}");
    let err = subscribe(&f.services, &f.server.url("/missing"), NOW)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 404"), "{err:#}");
    let err = subscribe(&f.services, "ftp://example.test/feed", NOW)
        .await
        .unwrap_err();
    assert!(!format!("{err:#}").is_empty());
    assert!(
        f.services
            .store
            .list_podcasts(PodcastListFilter::All)
            .unwrap()
            .is_empty()
    );
    assert!(f.log.kinds().is_empty());
}

#[tokio::test]
async fn unsubscribe_then_resubscribe_keeps_the_podcast_id() {
    let f = fixture();
    f.server.set(
        "/feed.xml",
        Route::new(rss(&[("a", "Pilot")]), "application/rss+xml"),
    );
    let first = subscribe(&f.services, &f.server.url("/feed.xml"), NOW)
        .await
        .unwrap();
    let store = Arc::clone(&f.services.store);
    let id = first.podcast_id.clone();
    f.log
        .record(&mut |stamps| store.unsubscribe(stamps, &id))
        .unwrap();
    assert!(
        store
            .list_podcasts(PodcastListFilter::Subscribed)
            .unwrap()
            .is_empty()
    );
    let again = subscribe(&f.services, &f.server.url("/feed.xml"), NOW + 1)
        .await
        .unwrap();
    assert!(!again.already_subscribed);
    assert_eq!(again.podcast_id, first.podcast_id);
    assert_eq!(
        f.log.kinds(),
        [
            "podcast_subscription_set",
            "podcast_subscription_set",
            "podcast_share_set",
            "podcast_subscription_set"
        ]
    );
    let ops = f.log.ops.lock().unwrap();
    assert!(matches!(
        ops.last().map(|op| &op.payload),
        Some(SyncOpPayload::PodcastSubscriptionSet {
            subscribed: true,
            ..
        })
    ));
}

#[test]
fn back_off_doubles_and_honours_retry_after() {
    let base = FAILURE_BACKOFF_BASE.as_millis() as i64;
    assert_eq!(next_check_after_failure(0, 1, None), base);
    assert_eq!(next_check_after_failure(0, 2, None), 2 * base);
    assert_eq!(next_check_after_failure(0, 4, None), 8 * base);
    let max = FAILURE_BACKOFF_MAX.as_millis() as i64;
    assert_eq!(next_check_after_failure(0, 40, None), max);
    assert_eq!(
        next_check_after_failure(0, 1, Some(Duration::from_secs(3_600))),
        3_600_000
    );
    assert_eq!(
        next_check_after_failure(0, 1, Some(Duration::from_secs(10 * 86_400))),
        max
    );
    assert_eq!(
        next_check_after_success(0),
        REFRESH_INTERVAL.as_millis() as i64
    );
}

/// The prototype podcast build's schema (user_version 1), verbatim.
const PROTOTYPE_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS podcasts (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        feed_url        TEXT NOT NULL UNIQUE,
        title           TEXT NOT NULL,
        author          TEXT,
        description     TEXT,
        link            TEXT,
        image_url       TEXT,
        cover_url       TEXT,
        cover_path      TEXT,
        etag            TEXT,
        last_modified   TEXT,
        added_at        INTEGER NOT NULL,
        last_checked_at INTEGER,
        last_error      TEXT
    );
    CREATE TABLE IF NOT EXISTS podcast_episodes (
        id               INTEGER PRIMARY KEY AUTOINCREMENT,
        podcast_id       INTEGER NOT NULL REFERENCES podcasts(id) ON DELETE CASCADE,
        guid             TEXT NOT NULL,
        title            TEXT NOT NULL,
        description      TEXT,
        published_at     INTEGER,
        duration_seconds REAL,
        audio_url        TEXT NOT NULL,
        mime_type        TEXT,
        byte_len         INTEGER,
        link             TEXT,
        image_url        TEXT,
        first_seen_at    INTEGER NOT NULL,
        UNIQUE (podcast_id, guid)
    );
    CREATE INDEX IF NOT EXISTS podcast_episodes_by_date
        ON podcast_episodes (podcast_id, published_at DESC, id DESC);
";

#[test]
fn prototype_database_is_migrated_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(furumi_library::podcasts::PODCAST_DB_FILE);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(PROTOTYPE_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO podcasts (feed_url, title, etag, added_at, last_checked_at)
             VALUES ('https://example.test/feed.xml', 'Old Show', '\"e1\"', 1700000000, 1700000100)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO podcast_episodes
                (podcast_id, guid, title, published_at, duration_seconds, audio_url, first_seen_at)
             VALUES (1, 'ep-1', 'Old Pilot', 1700000000, 1800.0,
                     'https://example.test/1.mp3', 1700000000)",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
    }
    let services = super::open_services(
        &path,
        dir.path().join("covers"),
        Arc::new(MemoryLog::default()),
    )
    .unwrap();
    let store = &services.store;
    assert_eq!(
        store.schema_version().unwrap(),
        furumi_library::podcasts::PODCAST_SCHEMA_VERSION
    );
    let rows = store.list_podcasts(PodcastListFilter::Subscribed).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].meta.title, "Old Show");
    assert_eq!(rows[0].episode_count, 1);
    let expected =
        PodcastId::from_feed_url(&FeedUrl::parse("https://example.test/feed.xml").unwrap());
    assert_eq!(rows[0].podcast_id, expected);
    // The prototype's validators are dropped: the first check parses the
    // feed (`imported_podcasts_get_their_seasons_on_the_first_check`).
    let fetch = store.fetch_state(&expected).unwrap();
    assert_eq!(fetch.etag, None);
    assert_eq!(fetch.last_success_at_ms, Some(1_700_000_100_000));
    assert_eq!(fetch.next_check_at_ms, None, "due at once");
    drop(services);

    // The original rows are kept, renamed.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let legacy: i64 = conn
        .query_row("SELECT COUNT(*) FROM legacy_podcast_episodes", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(legacy, 1);
}

#[test]
fn foreign_or_newer_databases_are_refused_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let log: Arc<dyn OpLog> = Arc::new(MemoryLog::default());

    let foreign = dir.path().join("foreign.sqlite3");
    {
        let conn = rusqlite::Connection::open(&foreign).unwrap();
        conn.execute_batch("CREATE TABLE tracks (id INTEGER PRIMARY KEY, title TEXT);")
            .unwrap();
    }
    let newer = dir.path().join("newer.sqlite3");
    {
        let conn = rusqlite::Connection::open(&newer).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }
    let garbage = dir.path().join("garbage.sqlite3");
    std::fs::write(
        &garbage,
        b"this is not a database at all, just some text bytes",
    )
    .unwrap();

    for (path, expected) in [
        (&foreign, "not a Furumi podcast database"),
        (&newer, "newer than this Furumi supports"),
        (&garbage, ""),
    ] {
        let before = std::fs::read(path).unwrap();
        let Err(err) = super::open_services(path, dir.path().join("covers"), Arc::clone(&log))
        else {
            panic!("{} opened", path.display());
        };
        let message = super::describe_open_error(&err);
        assert!(message.contains(expected), "{message}");
        assert_eq!(std::fs::read(path).unwrap(), before, "{}", path.display());
    }
}

/// A prototype database knew no seasons or episode numbers. Its stored
/// `ETag` would make the first check "304 Not Modified" forever for a feed
/// that no longer changes, so the seasons would never appear.
#[tokio::test]
async fn imported_podcasts_get_their_seasons_on_the_first_check() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestServer::start();
    let feed = "<?xml version=\"1.0\"?><rss version=\"2.0\" \
         xmlns:itunes=\"http://www.itunes.com/dtds/podcast-1.0.dtd\"><channel>\
         <title>Old Show</title><item><title>Old Pilot</title><guid>ep-1</guid>\
         <pubDate>Sat, 15 Jun 2024 10:00:00 GMT</pubDate>\
         <itunes:season>1</itunes:season><itunes:episode>1</itunes:episode>\
         <enclosure url=\"/audio/1.mp3\" type=\"audio/mpeg\" length=\"100\"/></item>\
         </channel></rss>";
    // The server answers the prototype's validator with 304.
    server.set(
        "/feed.xml",
        Route::new(feed, "application/rss+xml").etag("\"e1\""),
    );
    let feed_url = server.url("/feed.xml");
    let path = dir.path().join(furumi_library::podcasts::PODCAST_DB_FILE);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(PROTOTYPE_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO podcasts (feed_url, title, etag, last_modified, added_at, last_checked_at)
             VALUES (?1, 'Old Show', '\"e1\"', 'Sat, 15 Jun 2024 10:00:00 GMT',
                     1700000000, 1700000100)",
            [&feed_url],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO podcast_episodes
                (podcast_id, guid, title, published_at, duration_seconds, audio_url, first_seen_at)
             VALUES (1, 'ep-1', 'Old Pilot', 1718445600, 1800.0, ?1, 1700000000)",
            [server.url("/audio/1.mp3")],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
    }
    let services = super::open_services(
        &path,
        dir.path().join("covers"),
        Arc::new(MemoryLog::default()),
    )
    .unwrap();
    let store = Arc::clone(&services.store);
    let id = PodcastId::from_feed_url(&FeedUrl::parse(&feed_url).unwrap());
    assert!(
        store
            .seasons(&id)
            .unwrap()
            .iter()
            .all(|s| s.season.is_none()),
        "the prototype had no seasons"
    );
    let due = store.podcasts_due_for_refresh(NOW, 16).unwrap();
    assert_eq!(due.len(), 1);
    let report = refresh(&services, &id, &due[0].1, NOW).await.unwrap();
    assert!(!report.not_modified);
    let requests = server.requests("/feed.xml");
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].contains_key("if-none-match"));
    assert!(!requests[0].contains_key("if-modified-since"));
    let seasons = store.seasons(&id).unwrap();
    assert_eq!(
        seasons.iter().map(|s| s.season).collect::<Vec<_>>(),
        [Some(1)]
    );
    // From then on the checks are conditional again.
    let report = refresh(&services, &id, &due[0].1, NOW + 1).await.unwrap();
    assert!(report.not_modified);
    drop(services);

    // A store that was already migrated keeps its validators on reopen.
    let services = super::open_services(
        &path,
        dir.path().join("covers"),
        Arc::new(MemoryLog::default()),
    )
    .unwrap();
    assert_eq!(
        services.store.fetch_state(&id).unwrap().etag.as_deref(),
        Some("\"e1\"")
    );
}

/// The import commits inside `PodcastStore::open` and the validators are
/// dropped afterwards. A start that stopped in between (killed, or a failed
/// write) finishes the job at the next start, and the imported back-off is
/// kept.
#[test]
fn dropping_prototype_validators_resumes_after_an_interrupted_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(furumi_library::podcasts::PODCAST_DB_FILE);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(PROTOTYPE_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO podcasts (feed_url, title, etag, added_at, last_checked_at)
             VALUES ('https://example.test/ok.xml', 'Fine Show', '\"e1\"', 1700000000, 1700000100)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO podcasts
                (feed_url, title, last_modified, added_at, last_checked_at, last_error)
             VALUES ('https://example.test/broken.xml', 'Broken Show',
                     'Sat, 15 Jun 2024 10:00:00 GMT', 1700000000, 1700000200, 'HTTP 500')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
    }
    let ok = PodcastId::from_feed_url(&FeedUrl::parse("https://example.test/ok.xml").unwrap());
    let broken =
        PodcastId::from_feed_url(&FeedUrl::parse("https://example.test/broken.xml").unwrap());

    // The first start: the marker is written, then the import commits, and
    // the process stops before the validators are dropped.
    let marker = super::validators_marker(&path);
    assert!(super::is_prototype_database(&path));
    std::fs::write(&marker, b"").unwrap();
    drop(PodcastStore::open(&path).unwrap());
    assert!(!super::is_prototype_database(&path), "migrated");

    // The next start finishes the job.
    let services = super::open_services(
        &path,
        dir.path().join("covers"),
        Arc::new(MemoryLog::default()),
    )
    .unwrap();
    let store = &services.store;
    let fetch = store.fetch_state(&ok).unwrap();
    assert_eq!(fetch.etag, None);
    assert_eq!(fetch.last_error, None);
    let fetch = store.fetch_state(&broken).unwrap();
    assert_eq!(fetch.last_modified, None);
    assert_eq!(fetch.last_error.as_deref(), Some("HTTP 500"));
    assert_eq!(
        fetch.consecutive_failures, 1,
        "the imported back-off is kept"
    );
    assert_eq!(fetch.checked_at_ms, Some(1_700_000_200_000));
    assert!(!marker.exists(), "done once");

    // Validators of later checks are kept on every start after that.
    store
        .record_fetch_success(&ok, NOW, Some("\"e2\""), None, None)
        .unwrap();
    drop(services);
    let services = super::open_services(
        &path,
        dir.path().join("covers"),
        Arc::new(MemoryLog::default()),
    )
    .unwrap();
    assert_eq!(
        services.store.fetch_state(&ok).unwrap().etag.as_deref(),
        Some("\"e2\"")
    );
}

fn feed_url(f: &Fixture, path: &str) -> FeedUrl {
    FeedUrl::parse(&f.server.url(path)).unwrap()
}

#[tokio::test]
async fn feeds_over_the_size_limit_are_refused_with_or_without_a_length() {
    let f = fixture();
    f.server.set(
        "/feed.xml",
        Route::new(rss(&[("a", "Pilot")]), "application/rss+xml"),
    );
    let url = feed_url(&f, "/feed.xml");
    let id = subscribe(&f.services, url.as_str(), NOW)
        .await
        .unwrap()
        .podcast_id;
    let mut huge = rss(&[("a", "Pilot"), ("b", "Second")]);
    huge.resize(MAX_FEED_BYTES + 1, b' ');
    let store = &f.services.store;
    for (attempt, route) in [
        Route::new(huge.clone(), "application/rss+xml"),
        Route::new(huge, "application/rss+xml").no_length(),
    ]
    .into_iter()
    .enumerate()
    {
        let failures = attempt as u32 + 1;
        f.server.set("/feed.xml", route);
        let now = NOW + i64::from(failures);
        let err = refresh(&f.services, &id, &url, now).await.unwrap_err();
        assert!(format!("{err:#}").contains("too large"), "{err:#}");
        let state = store.fetch_state(&id).unwrap();
        assert_eq!(state.consecutive_failures, failures);
        assert!(state.last_error.unwrap().contains("too large"));
        assert_eq!(
            state.next_check_at_ms,
            Some(next_check_after_failure(now, failures, None))
        );
        assert_eq!(episode_count(store, &id), 1, "the catalog is kept");
    }
}

#[tokio::test]
async fn redirect_chains_are_bounded_and_lead_only_to_http() {
    let f = fixture();
    let fetch = |path: &'static str| {
        let url = feed_url(&f, path);
        let fetcher = std::sync::Arc::clone(&f.services.fetcher);
        async move { fetcher.fetch_feed(&url, Validators::default()).await }
    };
    for n in 0..=MAX_REDIRECTS {
        f.server.set(
            &format!("/r{n}.xml"),
            Route::redirect(302, &format!("/r{}.xml", n + 1)),
        );
    }
    f.server.set(
        &format!("/r{}.xml", MAX_REDIRECTS + 1),
        Route::new(rss(&[("a", "Pilot")]), "application/rss+xml"),
    );
    // MAX_REDIRECTS hops are followed, one more is refused.
    let followed = fetch("/r1.xml").await.unwrap();
    assert_eq!(followed.redirects, MAX_REDIRECTS);
    let err = fetch("/r0.xml").await.unwrap_err();
    assert!(format!("{err:#}").contains("too many redirects"), "{err:#}");
    // A loop ends the same way.
    f.server.set("/a.xml", Route::redirect(301, "/b.xml"));
    f.server.set("/b.xml", Route::redirect(301, "/a.xml"));
    let err = fetch("/a.xml").await.unwrap_err();
    assert!(format!("{err:#}").contains("too many redirects"), "{err:#}");
    assert_eq!(f.server.requests("/a.xml").len(), MAX_REDIRECTS / 2 + 1);
    // Only http(s) is followed.
    f.server.set(
        "/ftp.xml",
        Route::redirect(302, "ftp://example.test/feed.xml"),
    );
    let err = fetch("/ftp.xml").await.unwrap_err();
    assert!(
        format!("{err:#}").contains("unsupported URL scheme"),
        "{err:#}"
    );
    // Subscribing through any of them stores nothing.
    for path in ["/r0.xml", "/a.xml", "/ftp.xml"] {
        assert!(
            subscribe(&f.services, &f.server.url(path), NOW)
                .await
                .is_err()
        );
    }
    assert!(
        f.services
            .store
            .list_podcasts(PodcastListFilter::All)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_hanging_server_fails_the_check_at_the_deadline() {
    let f = fixture();
    f.server.set(
        "/feed.xml",
        Route::new(rss(&[("a", "Pilot")]), "application/rss+xml"),
    );
    let url = feed_url(&f, "/feed.xml");
    let id = subscribe(&f.services, url.as_str(), NOW)
        .await
        .unwrap()
        .podcast_id;
    f.server.set(
        "/feed.xml",
        Route::new(rss(&[("a", "Pilot")]), "application/rss+xml")
            .stall(FETCH_DEADLINE + Duration::from_secs(5)),
    );
    let started = std::time::Instant::now();
    let err = refresh(&f.services, &id, &url, NOW + 1).await.unwrap_err();
    assert!(format!("{err:#}").contains("took longer than"), "{err:#}");
    assert!(started.elapsed() < FETCH_DEADLINE + Duration::from_secs(2));
    let state = f.services.store.fetch_state(&id).unwrap();
    assert_eq!(state.consecutive_failures, 1);
    assert_eq!(episode_count(&f.services.store, &id), 1);
}

#[tokio::test]
async fn covers_must_be_images_within_the_limit_and_are_written_whole() {
    let f = fixture();
    let png = {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(2, 2)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    };
    f.server
        .set("/cover.png", Route::new(png.clone(), "image/png"));
    f.server.set(
        "/page.png",
        Route::new("<html>not an image</html>", "image/png"),
    );
    f.server.set(
        "/huge.png",
        Route::new(vec![0u8; MAX_COVER_BYTES + 1], "image/png"),
    );
    let id = PodcastId::from_feed_url(&feed_url(&f, "/feed.xml"));
    let fetcher = &f.services.fetcher;
    let path = fetcher
        .fetch_cover(&id, &f.server.url("/cover.png"))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), png);
    assert_eq!(path.extension().unwrap(), "png");
    let err = fetcher
        .fetch_cover(&id, &f.server.url("/page.png"))
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("not a supported image"),
        "{err:#}"
    );
    let err = fetcher
        .fetch_cover(&id, &f.server.url("/huge.png"))
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("too large"), "{err:#}");
    // Only the one finished file: nothing partial, no temporary files.
    let files: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files, [path]);
}
