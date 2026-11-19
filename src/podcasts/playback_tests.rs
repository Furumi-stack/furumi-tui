//! Episode playback preparation and durable progress, against a local HTTP
//! server and a temp-dir store (no internet, no audio device).

use std::sync::PoisonError;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use furumi_library::podcasts::{EpisodeQuery, PodcastStore};
use music_dht::podcast::{
    EpisodeId, PodcastEpisodeRef, PodcastId, ProgressStatus, reaches_completion,
};

use super::playback::{self, CannotResume, StartAt};
use super::progress::{ProgressCommand, ProgressEvent, ProgressWriter};
use super::refresh::{Services, refresh, subscribe};
use super::stream::{CancelToken, StreamEnd};
use super::test_server::{Route, WAV_RATE, wav};
use super::tests::{Fixture, NOW, fixture, rss};

const MINUTE_MS: i64 = 60_000;

/// Subscribes the fixture to a feed with episodes `a` and `b` and returns
/// the podcast and the reference of episode `a`.
async fn subscribed(f: &Fixture) -> (PodcastId, PodcastEpisodeRef) {
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
    let episode = episode_ref(&f.services.store, &report.podcast_id, "Pilot");
    (report.podcast_id, episode)
}

fn episode_ref(store: &PodcastStore, podcast_id: &PodcastId, title: &str) -> PodcastEpisodeRef {
    let item = store
        .episodes(&EpisodeQuery::new(podcast_id.clone()))
        .unwrap()
        .into_iter()
        .find(|item| item.episode.title == title)
        .expect("episode in the catalog");
    store
        .episode_ref(&item.episode.episode_id)
        .unwrap()
        .unwrap()
}

/// A progress writer whose events can be awaited.
struct Writer {
    writer: ProgressWriter,
    events: Receiver<ProgressEvent>,
}

impl Writer {
    fn spawn(services: &Services) -> Writer {
        let (tx, events) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let writer = ProgressWriter::spawn(services.clone(), move |event| {
            let _ = tx
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send(event);
        })
        .unwrap();
        Writer { writer, events }
    }

    fn send(&self, command: ProgressCommand) {
        self.writer.send(command);
    }

    /// The next event (the writer handles commands in order).
    fn next(&self) -> ProgressEvent {
        match self.events.recv_timeout(Duration::from_secs(10)) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => panic!("no progress event"),
            Err(RecvTimeoutError::Disconnected) => panic!("writer stopped"),
        }
    }

    fn saved(&self) -> music_dht::podcast::ProgressRecord {
        match self.next() {
            ProgressEvent::Saved(record) => record,
            other => panic!("expected a save, got {other:?}"),
        }
    }

    fn close(mut self) {
        assert!(self.writer.close(Duration::from_secs(10)));
    }
}

fn begin(
    playback: u64,
    episode: &PodcastEpisodeRef,
    position_ms: i64,
    restart: bool,
) -> ProgressCommand {
    ProgressCommand::Begin {
        playback,
        episode: Box::new(episode.clone()),
        session_id: format!("session-{playback}"),
        position_ms,
        restart,
        now_ms: NOW,
    }
}

fn resume(store: &PodcastStore, id: &EpisodeId) -> (i64, ProgressStatus) {
    let progress = store.progress(id).unwrap().expect("progress");
    (store.resume_position_ms(id).unwrap(), progress.status)
}

#[tokio::test]
async fn resume_position_survives_restarts_and_late_checkpoints_lose() {
    let f = fixture();
    let (_, episode) = subscribed(&f).await;
    let store = &f.services.store;
    let id = &episode.episode_id;

    // First run: play, save periodically, quit.
    let first = Writer::spawn(&f.services);
    first.send(begin(1, &episode, 0, false));
    let started = first.saved();
    assert_eq!(started.status, ProgressStatus::InProgress);
    first.send(ProgressCommand::Checkpoint {
        playback: 1,
        position_ms: 10 * MINUTE_MS,
    });
    assert_eq!(
        first.saved().generation,
        started.generation,
        "checkpoints keep it"
    );
    assert_eq!(
        resume(store, id),
        (10 * MINUTE_MS, ProgressStatus::InProgress)
    );

    // "Start over" in a later run takes a newer generation…
    let second = Writer::spawn(&f.services);
    second.send(begin(2, &episode, 0, true));
    let restarted = second.saved();
    assert!(restarted.generation > started.generation);
    assert_eq!(resume(store, id), (0, ProgressStatus::InProgress));

    // …so a late checkpoint of the old session loses, whatever its clock.
    first.send(ProgressCommand::Checkpoint {
        playback: 1,
        position_ms: 20 * MINUTE_MS,
    });
    match first.next() {
        ProgressEvent::Superseded { current } => assert_eq!(current.position_ms, 0),
        other => panic!("expected supersede, got {other:?}"),
    }
    assert_eq!(resume(store, id), (0, ProgressStatus::InProgress));
    // The old session no longer writes, not even when it ends.
    first.send(ProgressCommand::End {
        playback: 1,
        position_ms: 21 * MINUTE_MS,
        listened_ms: 0,
        now_ms: NOW + 1,
    });
    assert!(matches!(first.next(), ProgressEvent::Ended { .. }));
    first.close();
    assert_eq!(resume(store, id), (0, ProgressStatus::InProgress));

    // The new session saves normally, including a seek backwards.
    second.send(ProgressCommand::Checkpoint {
        playback: 2,
        position_ms: 5 * MINUTE_MS,
    });
    second.saved();
    second.send(ProgressCommand::Seek {
        playback: 2,
        position_ms: MINUTE_MS,
    });
    let seeked = second.saved();
    assert!(
        seeked.generation > restarted.generation,
        "a seek is explicit"
    );
    assert_eq!(resume(store, id), (MINUTE_MS, ProgressStatus::InProgress));
    second.send(ProgressCommand::End {
        playback: 2,
        position_ms: MINUTE_MS + 2_000,
        listened_ms: 2_000,
        now_ms: NOW + 2,
    });
    second.saved();
    assert!(matches!(second.next(), ProgressEvent::Ended { .. }));
    second.close();
    assert_eq!(
        resume(store, id),
        (MINUTE_MS + 2_000, ProgressStatus::InProgress)
    );
    // The sync log carries every write, in order.
    let kinds = f.log.kinds();
    assert_eq!(kinds[0], "podcast_subscription_set");
    assert!(
        kinds[1..].iter().all(|kind| kind == "podcast_progress_set"),
        "{kinds:?}"
    );
}

#[tokio::test]
async fn completion_counts_once_and_short_sessions_stay_out_of_history() {
    let f = fixture();
    let (_, episode) = subscribed(&f).await;
    let store = &f.services.store;
    let id = &episode.episode_id;
    let duration = episode.duration_ms.expect("the feed has a duration");
    assert_eq!(duration, 45 * MINUTE_MS);
    let near_end = duration - 20_000;
    assert!(reaches_completion(near_end, Some(duration)));

    // A short listen: progress is saved, history is not.
    let writer = Writer::spawn(&f.services);
    writer.send(begin(1, &episode, 0, false));
    writer.saved();
    writer.send(ProgressCommand::End {
        playback: 1,
        position_ms: 5_000,
        listened_ms: 5_000,
        now_ms: NOW + 5_000,
    });
    writer.saved();
    assert!(matches!(writer.next(), ProgressEvent::Ended { .. }));
    assert!(store.listen_history(None, 10).unwrap().is_empty());

    // A full listen completes the episode once, even if the end is
    // reported twice and the session ends afterwards.
    writer.send(begin(2, &episode, 5_000, false));
    writer.saved();
    for _ in 0..2 {
        writer.send(ProgressCommand::Complete {
            playback: 2,
            position_ms: near_end,
            listened_ms: near_end - 5_000,
            now_ms: NOW + duration,
        });
    }
    let completed = writer.saved();
    assert_eq!(completed.status, ProgressStatus::Completed);
    writer.send(ProgressCommand::End {
        playback: 2,
        position_ms: duration,
        listened_ms: duration - 5_000,
        now_ms: NOW + duration + 20_000,
    });
    assert!(matches!(writer.next(), ProgressEvent::Ended { .. }));
    writer.close();
    assert_eq!(
        resume(store, id),
        (0, ProgressStatus::Completed),
        "Play starts over"
    );
    assert_eq!(store.completed_listen_count(id).unwrap(), 1);
    let history = store.listen_history(None, 10).unwrap();
    assert_eq!(history.len(), 1);
    assert!(history[0].session.completed);
    assert_eq!(history[0].session.listened_ms, duration - 5_000);
}

#[tokio::test]
async fn marking_played_while_playing_stops_checkpoints_until_the_next_seek() {
    let f = fixture();
    let (_, episode) = subscribed(&f).await;
    let store = &f.services.store;
    let id = &episode.episode_id;
    let writer = Writer::spawn(&f.services);
    writer.send(begin(1, &episode, 0, false));
    writer.saved();
    writer.send(ProgressCommand::SetPlayed {
        episode_id: id.clone(),
        played: true,
    });
    assert_eq!(writer.saved().status, ProgressStatus::Completed);
    assert!(matches!(
        writer.next(),
        ProgressEvent::PlayedSet {
            played: true,
            result: Ok(())
        }
    ));
    writer.send(ProgressCommand::Checkpoint {
        playback: 1,
        position_ms: 90_000,
    });
    assert!(matches!(writer.next(), ProgressEvent::Superseded { .. }));
    assert_eq!(resume(store, id).1, ProgressStatus::Completed);
    // Seeking is the user's explicit choice to keep listening here.
    writer.send(ProgressCommand::Seek {
        playback: 1,
        position_ms: 30_000,
    });
    assert_eq!(writer.saved().status, ProgressStatus::InProgress);
    assert_eq!(resume(store, id), (30_000, ProgressStatus::InProgress));
    writer.close();
}

/// Serves a position-coded WAV for episode `a` and returns its seconds.
fn serve_audio(f: &Fixture, route: impl FnOnce(Route) -> Route, seconds: u32) -> u32 {
    f.server
        .set("/audio/a.mp3", route(Route::new(wav(seconds), "audio/wav")));
    seconds
}

/// Seconds marker of the next decoded sample (see `test_server::wav`).
fn second_marker(source: &mut crate::player::DecodedSource) -> i64 {
    let sample = source.next().expect("a sample");
    (sample * 32_768.0 / 40.0).round() as i64
}

/// The source plays from `second`. A seek may land up to one decoder packet
/// early (1024 frames, 0.128 s at 8 kHz) in the previous second.
fn assert_starts_at(source: &mut crate::player::DecodedSource, second: i64) {
    let marker = second_marker(source);
    assert!(
        marker == second || marker == second - 1,
        "starts in second {marker}, expected {second}"
    );
    // A quarter of a second later it is exactly there.
    let marker = (0..WAV_RATE / 4)
        .map(|_| second_marker(source))
        .last()
        .unwrap();
    assert_eq!(marker, second);
}

async fn save_position(services: &Services, episode: &PodcastEpisodeRef, position_ms: i64) {
    let writer = Writer::spawn(services);
    writer.send(begin(99, episode, position_ms, false));
    writer.saved();
    writer.close();
}

#[tokio::test]
async fn play_resumes_at_the_saved_position_with_a_range_request() {
    let f = fixture();
    let (_, episode) = subscribed(&f).await;
    let seconds = serve_audio(&f, |route| route, 12 * 60);
    save_position(&f.services, &episode, 11 * MINUTE_MS).await;

    let prepared = playback::prepare(
        Some(f.services.clone()),
        episode.clone(),
        StartAt::Resume,
        NOW,
        CancelToken::never(),
    )
    .await
    .unwrap();
    assert_eq!(prepared.start_ms, 11 * MINUTE_MS);
    assert_eq!(prepared.duration_ms, Some(i64::from(seconds) * 1_000));
    let mut source = prepared.source;
    assert_starts_at(&mut source, 11 * 60);
    // The far seek became a ranged request instead of a full download.
    let byte = 11 * 60 * WAV_RATE as usize * 2;
    assert!(
        f.server.ranges("/audio/a.mp3").iter().any(|range| range
            .as_deref()
            .and_then(|range| range.strip_prefix("bytes="))
            .and_then(|range| range.trim_end_matches('-').parse::<usize>().ok())
            .is_some_and(|start| start + 4_096 >= byte && start <= byte + 4_096)),
        "{:?}",
        f.server.ranges("/audio/a.mp3")
    );

    // "Start over" ignores the saved position.
    let restarted = playback::prepare(
        Some(f.services.clone()),
        episode,
        StartAt::Restart,
        NOW,
        CancelToken::never(),
    )
    .await
    .unwrap();
    assert_eq!(restarted.start_ms, 0);
    let mut source = restarted.source;
    assert_eq!(second_marker(&mut source), 0);
}

#[tokio::test]
async fn a_server_without_range_support_still_resumes_at_the_right_place() {
    let f = fixture();
    let (_, episode) = subscribed(&f).await;
    serve_audio(&f, Route::no_ranges, 30);
    save_position(&f.services, &episode, 20_000).await;
    let prepared = playback::prepare(
        Some(f.services.clone()),
        episode,
        StartAt::Resume,
        NOW,
        CancelToken::never(),
    )
    .await
    .unwrap();
    assert_eq!(prepared.start_ms, 20_000);
    let mut source = prepared.source;
    assert_starts_at(&mut source, 20);
    assert!(
        f.server
            .ranges("/audio/a.mp3")
            .iter()
            .skip(1)
            .all(Option::is_none),
        "no ranges are sent to a server that ignores them"
    );
}

#[tokio::test]
async fn an_unseekable_stream_does_not_pretend_to_resume() {
    let f = fixture();
    let (_, episode) = subscribed(&f).await;
    serve_audio(&f, |route| route.no_ranges().no_length(), 30);
    save_position(&f.services, &episode, 20_000).await;
    let err = playback::prepare(
        Some(f.services.clone()),
        episode.clone(),
        StartAt::Resume,
        NOW,
        CancelToken::never(),
    )
    .await
    .expect_err("cannot resume");
    assert_eq!(
        err.downcast_ref::<CannotResume>(),
        Some(&CannotResume {
            position_ms: 20_000
        })
    );
    assert!(playback::describe_failure(&err).contains("0:20"));
    // The saved position is untouched, and "start over" still plays.
    let store = &f.services.store;
    assert_eq!(
        store.resume_position_ms(&episode.episode_id).unwrap(),
        20_000
    );
    let prepared = playback::prepare(
        Some(f.services.clone()),
        episode,
        StartAt::Restart,
        NOW,
        CancelToken::never(),
    )
    .await
    .unwrap();
    assert_eq!(prepared.start_ms, 0);
}

#[tokio::test]
async fn the_audio_url_comes_from_the_feed_at_play_time() {
    let f = fixture();
    let (podcast_id, queued) = subscribed(&f).await;
    // The publisher moved the file and the feed already says so.
    f.server.set(
        "/feed.xml",
        Route::new(
            rss(&[("a", "Pilot"), ("b", "Second")]).pipe_replace("/audio/a.mp3", "/audio/a-v2.mp3"),
            "application/rss+xml",
        ),
    );
    let url = f
        .services
        .store
        .podcast(&podcast_id)
        .unwrap()
        .unwrap()
        .meta
        .feed_url;
    refresh(&f.services, &podcast_id, &url, NOW + 1)
        .await
        .unwrap();
    f.server
        .set("/audio/a-v2.mp3", Route::new(wav(5), "audio/wav"));
    // The queue item still has the old URL; the store's is used.
    assert!(queued.audio_url.ends_with("/audio/a.mp3"));
    let prepared = playback::prepare(
        Some(f.services.clone()),
        queued.clone(),
        StartAt::Resume,
        NOW,
        CancelToken::never(),
    )
    .await
    .unwrap();
    assert!(prepared.episode.audio_url.ends_with("/audio/a-v2.mp3"));
    assert!(f.server.requests("/audio/a.mp3").is_empty());
    assert_eq!(
        prepared.episode.episode_id, queued.episode_id,
        "same episode"
    );
}

#[tokio::test]
async fn a_stale_audio_url_refreshes_the_feed_once_and_retries() {
    let f = fixture();
    let (_, queued) = subscribed(&f).await;
    // The old file is gone (404) and only the feed knows the new one.
    f.server.set(
        "/feed.xml",
        Route::new(
            rss(&[("a", "Pilot"), ("b", "Second")]).pipe_replace("/audio/a.mp3", "/audio/a-v3.mp3"),
            "application/rss+xml",
        ),
    );
    f.server
        .set("/audio/a-v3.mp3", Route::new(wav(5), "audio/wav"));
    let prepared = playback::prepare(
        Some(f.services.clone()),
        queued.clone(),
        StartAt::Resume,
        NOW,
        CancelToken::never(),
    )
    .await
    .unwrap();
    assert!(prepared.episode.audio_url.ends_with("/audio/a-v3.mp3"));
    assert_eq!(f.server.requests("/audio/a.mp3").len(), 1);
    assert_eq!(
        f.server.requests("/feed.xml").len(),
        2,
        "one extra feed check"
    );

    // Without a newer address the original error is reported, once.
    f.server.set(
        "/audio/a-v3.mp3",
        Route::new("gone", "text/plain").status(410),
    );
    let err = playback::prepare(
        Some(f.services.clone()),
        queued,
        StartAt::Resume,
        NOW,
        CancelToken::never(),
    )
    .await
    .expect_err("still unavailable");
    assert!(format!("{err:#}").contains("410"), "{err:#}");
    assert_eq!(f.server.requests("/feed.xml").len(), 3);
}

#[tokio::test]
async fn without_a_store_the_queued_reference_plays_from_the_start() {
    let f = fixture();
    let (_, episode) = subscribed(&f).await;
    serve_audio(&f, |route| route, 5);
    save_position(&f.services, &episode, 3_000).await;
    let prepared = playback::prepare(None, episode, StartAt::Resume, NOW, CancelToken::never())
        .await
        .unwrap();
    assert_eq!(prepared.start_ms, 0);
}

trait PipeReplace {
    fn pipe_replace(self, from: &str, to: &str) -> Vec<u8>;
}

impl PipeReplace for Vec<u8> {
    fn pipe_replace(self, from: &str, to: &str) -> Vec<u8> {
        String::from_utf8(self)
            .unwrap()
            .replace(from, to)
            .into_bytes()
    }
}

/// Decodes `source` to its end, as the audio thread would; returns the
/// position reached (mono samples of `test_server::wav`).
fn play_out(source: &mut crate::player::DecodedSource) -> i64 {
    let mut samples = 0i64;
    while source.next().is_some() {
        samples += 1;
    }
    samples * 1_000 / i64::from(WAV_RATE)
}

/// Byte offset of second `second` in `test_server::wav`.
fn wav_offset(second: u32) -> usize {
    44 + (second * WAV_RATE * 2) as usize
}

#[tokio::test]
async fn a_lost_connection_does_not_complete_the_episode_and_keeps_its_position() {
    let f = fixture();
    let (_, episode) = subscribed(&f).await;
    // Two minutes of audio; the server stops sending after 30 s for good.
    serve_audio(&f, |route| route.cut(wav_offset(30), usize::MAX), 120);
    let prepared = playback::prepare(
        Some(f.services.clone()),
        episode.clone(),
        StartAt::Resume,
        NOW,
        CancelToken::never(),
    )
    .await
    .unwrap();
    let writer = Writer::spawn(&f.services);
    writer.send(begin(1, &episode, 0, false));
    writer.saved();

    let mut source = prepared.source;
    // The decoder ends once the stalled read gives up (STARVE_TIMEOUT).
    let position_ms = play_out(&mut source);
    assert!(
        (29_000..=30_500).contains(&position_ms),
        "stopped at {position_ms} ms"
    );
    assert_eq!(prepared.probe.end(), StreamEnd::Failed);
    assert_eq!(prepared.duration_ms, Some(120_000));
    assert!(!playback::played_to_the_end(
        prepared.probe.end(),
        position_ms,
        prepared.duration_ms
    ));

    // What the runtime then saves: the last position, not a completion.
    writer.send(ProgressCommand::End {
        playback: 1,
        position_ms,
        listened_ms: position_ms,
        now_ms: NOW + 1,
    });
    while !matches!(writer.next(), ProgressEvent::Ended { .. }) {}
    writer.close();
    let store = &f.services.store;
    assert_eq!(
        resume(store, &episode.episode_id),
        (position_ms, ProgressStatus::InProgress)
    );
    assert_eq!(
        store.completed_listen_count(&episode.episode_id).unwrap(),
        0
    );
}

#[tokio::test]
async fn a_stream_of_unknown_length_that_closes_early_is_not_the_end() {
    let f = fixture();
    let (_, episode) = subscribed(&f).await;
    serve_audio(
        &f,
        |route| route.no_length().cut(wav_offset(30), usize::MAX),
        120,
    );
    let prepared = playback::prepare(
        Some(f.services.clone()),
        episode.clone(),
        StartAt::Restart,
        NOW,
        CancelToken::never(),
    )
    .await
    .unwrap();
    let mut source = prepared.source;
    let position_ms = play_out(&mut source);
    assert!((29_000..=30_500).contains(&position_ms), "{position_ms} ms");
    // A closed connection looks like the end of the file here...
    assert_eq!(prepared.probe.end(), StreamEnd::Unknown);
    // ...but the duration says otherwise.
    assert!(!playback::played_to_the_end(
        StreamEnd::Unknown,
        position_ms,
        Some(120_000)
    ));
    assert!(playback::played_to_the_end(
        StreamEnd::Unknown,
        119_000,
        Some(120_000)
    ));

    // A whole file with a size is played to its end.
    serve_audio(&f, |route| route, 3);
    let prepared = playback::prepare(
        Some(f.services.clone()),
        episode,
        StartAt::Restart,
        NOW,
        CancelToken::never(),
    )
    .await
    .unwrap();
    let mut source = prepared.source;
    assert_eq!(play_out(&mut source), 3_000);
    assert_eq!(prepared.probe.end(), StreamEnd::Complete);
    assert!(playback::played_to_the_end(
        StreamEnd::Complete,
        3_000,
        prepared.duration_ms
    ));
    // A failure in the last seconds still counts as the end.
    assert!(playback::played_to_the_end(
        StreamEnd::Failed,
        119_500,
        Some(120_000)
    ));
}

#[test]
fn a_superseded_resume_stops_reading_up_to_its_offset() {
    let f = fixture();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (_, episode) = rt.block_on(subscribed(&f));
    // No Range support: resuming at 60 s means reading and discarding
    // everything before it (while positioning, or on the first reads after
    // it, depending on the format), and this server stalls after 10 s.
    serve_audio(
        &f,
        |route| route.no_ranges().cut(wav_offset(10), usize::MAX),
        120,
    );
    let superseded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&superseded);
    let cancel = CancelToken::new(move || flag.load(std::sync::atomic::Ordering::SeqCst));
    let started = std::time::Instant::now();
    let starting = std::thread::spawn(move || {
        let prepared = playback::open_at(&episode, 60_000, &cancel)?;
        let mut source = prepared.source;
        Ok::<_, anyhow::Error>((source.next(), prepared.probe))
    });
    std::thread::sleep(Duration::from_millis(300));
    superseded.store(true, std::sync::atomic::Ordering::SeqCst);
    match starting.join().unwrap() {
        Err(err) => assert!(
            err.downcast_ref::<super::stream::Cancelled>().is_some(),
            "{err:#}"
        ),
        Ok((sample, probe)) => {
            assert_eq!(sample, None, "nothing plays after the cancel");
            assert_eq!(probe.end(), StreamEnd::Failed);
        }
    }
    // Well before a stalled read would give up by itself.
    assert!(
        started.elapsed() < super::stream::STARVE_TIMEOUT / 2,
        "{:?}",
        started.elapsed()
    );
}
