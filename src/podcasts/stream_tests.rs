//! Tests of the HTTP Range stream reader against the local test server:
//! sequential reads, seeks inside and outside the window, servers without
//! Range or Content-Length, transient cuts, persistent failures, bounded
//! memory and thread shutdown.

use super::stream::{self, MAX_BUFFERED};
use super::test_server::{Route, TestServer, noise};
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

const MIB: usize = 1024 * 1024;

fn read_to_end(reader: &mut impl Read) -> Vec<u8> {
    let mut out = Vec::new();
    reader.read_to_end(&mut out).unwrap();
    out
}

fn read_n(reader: &mut impl Read, len: usize) -> Vec<u8> {
    let mut out = vec![0; len];
    reader.read_exact(&mut out).unwrap();
    out
}

fn serve(route: Route) -> (TestServer, String) {
    let server = TestServer::start();
    server.set("/audio.mp3", route);
    let url = server.url("/audio.mp3");
    (server, url)
}

#[test]
fn sequential_read_matches_source() {
    let body = noise(3 * MIB + 123, 1);
    let (server, url) = serve(Route::new(body.clone(), "audio/mpeg; charset=binary"));
    let mut opened = stream::open(&url).unwrap();
    assert_eq!(opened.byte_len, Some(body.len() as u64));
    assert_eq!(opened.mime_type.as_deref(), Some("audio/mpeg"));
    assert!(opened.seekable);
    assert_eq!(read_to_end(&mut opened.reader), body);
    let mut buf = [0u8; 16];
    assert_eq!(opened.reader.read(&mut buf).unwrap(), 0, "EOF stays EOF");
    assert_eq!(server.ranges("/audio.mp3"), [Some("bytes=0-".to_string())]);
}

#[test]
fn seeks_forward_backward_and_from_end() {
    let body = noise(12 * MIB, 2);
    let (server, url) = serve(Route::new(body.clone(), "audio/mpeg"));
    let mut reader = stream::open(&url).unwrap().reader;
    assert_eq!(read_n(&mut reader, 1000), body[..1000]);

    // Far forward: outside the read-ahead window -> new range request.
    let far = 11 * MIB + 7;
    assert_eq!(
        reader.seek(SeekFrom::Start(far as u64)).unwrap(),
        far as u64
    );
    assert_eq!(read_n(&mut reader, 70_000), body[far..far + 70_000]);
    assert!(
        server
            .ranges("/audio.mp3")
            .contains(&Some(format!("bytes={far}-")))
    );

    // Small backward seek within the kept window: no new request.
    let requests = server.ranges("/audio.mp3").len();
    reader.seek(SeekFrom::Current(-50_000)).unwrap();
    let pos = far + 20_000;
    assert_eq!(read_n(&mut reader, 1000), body[pos..pos + 1000]);
    assert_eq!(server.ranges("/audio.mp3").len(), requests);

    // Back into the permanently kept head: no new request.
    reader.seek(SeekFrom::Start(10)).unwrap();
    assert_eq!(read_n(&mut reader, 100), body[10..110]);
    assert_eq!(server.ranges("/audio.mp3").len(), requests);

    // Backward outside the window and the head.
    let back = 2 * MIB + 3;
    reader.seek(SeekFrom::Start(back as u64)).unwrap();
    assert_eq!(read_n(&mut reader, 300_000), body[back..back + 300_000]);

    // From the end.
    let end = reader.seek(SeekFrom::End(-100)).unwrap();
    assert_eq!(end, body.len() as u64 - 100);
    assert_eq!(read_to_end(&mut reader), body[body.len() - 100..]);

    // Past the end.
    let beyond = reader.seek(SeekFrom::End(50)).unwrap();
    assert_eq!(beyond, body.len() as u64 + 50);
    let mut buf = [0u8; 8];
    assert_eq!(reader.read(&mut buf).unwrap(), 0);
    assert_eq!(
        reader
            .seek(SeekFrom::Current(-(beyond as i64) - 1))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
}

#[test]
fn server_without_range_support() {
    let body = noise(3 * MIB, 3);
    let (server, url) = serve(Route::new(body.clone(), "audio/mpeg").no_ranges());
    let opened = stream::open(&url).unwrap();
    assert!(opened.seekable, "length is known");
    let mut reader = opened.reader;
    assert_eq!(read_n(&mut reader, 400_000), body[..400_000]);
    // Forward seek: read and discard on the same response.
    reader.seek(SeekFrom::Start(2 * MIB as u64)).unwrap();
    assert_eq!(read_n(&mut reader, 1000), body[2 * MIB..2 * MIB + 1000]);
    // Backward seek outside window and head: restart from 0.
    reader.seek(SeekFrom::Start(300_000)).unwrap();
    let _ = read_n(&mut reader, 1);
    reader.seek(SeekFrom::Start(300_000)).unwrap();
    assert_eq!(read_to_end(&mut reader), body[300_000..]);
    let requests = server.ranges("/audio.mp3");
    assert!(requests.len() >= 2, "{requests:?}");
    assert!(requests[1..].iter().all(Option::is_none), "{requests:?}");
    reader.seek(SeekFrom::End(-10)).unwrap();
    assert_eq!(read_to_end(&mut reader), body[body.len() - 10..]);
}

#[test]
fn unknown_length_is_not_seekable_from_end() {
    let body = noise(MIB + 5, 4);
    let (_server, url) = serve(
        Route::new(body.clone(), "audio/mpeg")
            .no_ranges()
            .no_length(),
    );
    let opened = stream::open(&url).unwrap();
    assert!(!opened.seekable);
    assert_eq!(opened.byte_len, None);
    let mut reader = opened.reader;
    assert_eq!(
        reader.seek(SeekFrom::End(0)).unwrap_err().kind(),
        ErrorKind::Unsupported
    );
    assert_eq!(read_to_end(&mut reader), body);
}

#[test]
fn transient_disconnects_are_retried() {
    let body = noise(2 * MIB, 5);
    let (server, url) = serve(Route::new(body.clone(), "audio/mpeg").cut(700_000, 2));
    let mut reader = stream::open(&url).unwrap().reader;
    assert_eq!(read_to_end(&mut reader), body);
    let requests = server.ranges("/audio.mp3");
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert_eq!(requests[1], Some("bytes=700000-".to_string()));
}

#[test]
fn persistent_failure_surfaces_an_error() {
    let body = noise(2 * MIB, 6);
    let (_server, url) = serve(Route::new(body, "audio/mpeg").cut(100_000, 100));
    let mut reader = stream::open(&url).unwrap().reader;
    let mut out = Vec::new();
    let started = Instant::now();
    let err = reader.read_to_end(&mut out).unwrap_err();
    assert_eq!(out.len(), 100_000);
    assert_eq!(err.kind(), ErrorKind::TimedOut, "{err}");
    assert!(err.to_string().contains("stalled"), "{err}");
    // Bounded by the starvation deadline, not by a retry count.
    assert!(started.elapsed() >= stream::STARVE_TIMEOUT);
    assert!(started.elapsed() < stream::STARVE_TIMEOUT * 3);
}

/// An outage that ends while buffered audio is still playing must not
/// end playback: the downloader keeps retrying instead of giving up
/// after a fixed number of attempts.
#[test]
fn outage_shorter_than_the_buffer_is_bridged() {
    let body = noise(3 * MIB, 10);
    let (server, url) = serve(Route::new(body.clone(), "audio/mpeg").cut(MIB, 4));
    let mut reader = stream::open(&url).unwrap().reader;
    assert_eq!(read_n(&mut reader, 1000), body[..1000]);
    // "Play" buffered audio while the server fails four times and then
    // recovers.
    std::thread::sleep(Duration::from_millis(4500));
    assert_eq!(read_to_end(&mut reader), body[1000..]);
    let requests = server.ranges("/audio.mp3");
    assert_eq!(requests.len(), 5, "{requests:?}");
    assert!(
        requests[1..]
            .iter()
            .all(|range| range.as_deref() == Some("bytes=1048576-")),
        "{requests:?}"
    );
}

#[test]
fn client_errors_are_reported_without_waiting() {
    let body = noise(2 * MIB, 11);
    let (server, url) = serve(Route::new(body, "audio/mpeg").cut(100_000, 1));
    let mut reader = stream::open(&url).unwrap().reader;
    // The signed URL "expires" after the first response.
    server.set("/audio.mp3", Route::new("gone", "text/plain").status(404));
    let started = Instant::now();
    let mut out = Vec::new();
    let err = reader.read_to_end(&mut out).unwrap_err();
    assert_eq!(out.len(), 100_000);
    assert!(err.to_string().contains("404"), "{err}");
    assert!(
        started.elapsed() < stream::STARVE_TIMEOUT,
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn large_body_keeps_memory_bounded() {
    let body = noise(20 * MIB, 7);
    let (_server, url) = serve(Route::new(body.clone(), "audio/mpeg"));
    let mut reader = stream::open(&url).unwrap().reader;
    let mut buf = vec![0u8; 32 * 1024];
    let mut pos = 0usize;
    let mut hasher_ok = true;
    loop {
        let read = reader.read(&mut buf).unwrap();
        if read == 0 {
            break;
        }
        hasher_ok &= buf[..read] == body[pos..pos + read];
        pos += read;
        if pos % (2 * MIB) < 32 * 1024 && pos + stream::READ_AHEAD as usize <= body.len() {
            // Let the downloader run ahead until it pauses at its cap.
            let deadline = Instant::now() + Duration::from_secs(5);
            while reader.buffered_bytes() < stream::READ_AHEAD as usize && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert!(reader.buffered_bytes() <= MAX_BUFFERED);
    }
    assert_eq!(pos, body.len());
    assert!(hasher_ok, "data mismatch");
    let peak = reader.peak_buffered_bytes();
    assert!(peak <= MAX_BUFFERED, "peak {peak} > {MAX_BUFFERED}");
    assert!(
        peak > 4 * MIB,
        "read-ahead should have buffered, peak {peak}"
    );
    // Far seeks restart the window without growing its allocation.
    for target in [MIB + 5, 15 * MIB, 3 * MIB] {
        reader.seek(SeekFrom::Start(target as u64)).unwrap();
        assert_eq!(read_n(&mut reader, 1000), body[target..target + 1000]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while reader.buffered_bytes() < stream::READ_AHEAD as usize && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    // The allocation, not just the length, is bounded.
    let allocated = reader.peak_allocated_bytes();
    assert!(
        allocated <= MAX_BUFFERED,
        "allocated {allocated} > {MAX_BUFFERED}"
    );
}

#[test]
fn dropping_the_reader_stops_the_thread() {
    let body = noise(20 * MIB, 8);
    let (_server, url) = serve(Route::new(body.clone(), "audio/mpeg"));
    let mut reader = stream::open(&url).unwrap().reader;
    assert_eq!(read_n(&mut reader, 100_000), body[..100_000]);
    let exited = reader.exit_probe();
    let deadline = Instant::now() + Duration::from_secs(5);
    // Let it fill the read-ahead window (it then pauses).
    while reader.buffered_bytes() < stream::READ_AHEAD as usize && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!exited());
    drop(reader);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !exited() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(exited(), "downloader thread did not stop");

    // Also while actively downloading (not paused).
    let mut reader = stream::open(&url).unwrap().reader;
    let exited = reader.exit_probe();
    let _ = read_n(&mut reader, 10);
    drop(reader);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !exited() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(exited(), "downloader thread did not stop");
}

/// The documented calling convention: `open` on Tokio's blocking pool,
/// reads on another plain thread (like the audio thread).
#[test]
fn open_from_spawn_blocking() {
    let body = noise(MIB, 9);
    let (server, url) = serve(Route::new(body.clone(), "audio/mpeg"));
    server.set("/missing.mp3", Route::new("x", "text/plain").status(404));
    let missing = server.url("/missing.mp3");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (opened, failed) = runtime.block_on(async move {
        let opened = tokio::task::spawn_blocking(move || stream::open(&url))
            .await
            .unwrap();
        let failed = tokio::task::spawn_blocking(move || stream::open(&missing).is_err())
            .await
            .unwrap();
        (opened, failed)
    });
    assert!(failed);
    let mut reader = opened.unwrap().reader;
    let read = std::thread::spawn(move || read_to_end(&mut reader))
        .join()
        .unwrap();
    assert_eq!(read, body);
    drop(runtime);
}

#[test]
fn open_fails_for_http_errors() {
    let (_server, url) = serve(Route::new("gone", "text/plain").status(404));
    let err = stream::open(&url).err().expect("404 must fail");
    assert!(format!("{err:#}").contains("404"), "{err:#}");
}

#[test]
fn errors_do_not_show_the_url_query() {
    let server = TestServer::start();
    // The test server matches paths with their query, so this is a 404.
    let err = stream::open(&server.url("/private.mp3?token=s3cr3t"))
        .err()
        .expect("404 must fail");
    let message = format!("{err:#}");
    assert!(message.contains("404"), "{message}");
    assert!(!message.contains("s3cr3t"), "{message}");
    let err = stream::open("file:///etc/passwd").err().expect("not http");
    assert!(format!("{err:#}").contains("scheme"), "{err:#}");
}

#[test]
fn http_stream_is_send_and_sync() {
    fn check<T: Send + Sync + std::io::Read + std::io::Seek>() {}
    check::<stream::HttpStream>();
}

/// A read blocked on a stalled server (inside the audio callback, in the
/// player) returns as soon as its playback is superseded, instead of after
/// `STARVE_TIMEOUT`, and the downloader stops; the stream then reports a
/// failed end.
#[test]
fn a_blocked_read_fails_as_soon_as_its_playback_is_superseded() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let body = noise(4 * MIB, 10);
    let (_server, url) = serve(Route::new(body.clone(), "audio/mpeg").cut(MIB, usize::MAX));
    let superseded = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&superseded);
    let opened = stream::open_with(
        &url,
        stream::CancelToken::new(move || flag.load(Ordering::SeqCst)),
    )
    .unwrap();
    let probe = opened.probe.clone();
    let mut reader = opened.reader;
    let exited = reader.exit_probe();
    assert_eq!(read_n(&mut reader, MIB), body[..MIB]);
    assert_eq!(probe.end(), stream::StreamEnd::Complete);

    let reading = std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        let started = Instant::now();
        let result = reader.read(&mut buf);
        (result, started.elapsed(), reader)
    });
    std::thread::sleep(Duration::from_millis(300));
    superseded.store(true, Ordering::SeqCst);
    let (result, waited, mut reader) = reading.join().unwrap();
    let err = result.expect_err("superseded");
    assert!(
        err.get_ref()
            .is_some_and(|inner| inner.is::<stream::Cancelled>()),
        "{err}"
    );
    assert!(
        waited < stream::STARVE_TIMEOUT / 2,
        "blocked for {waited:?}"
    );
    assert_eq!(probe.end(), stream::StreamEnd::Failed);
    // Later reads fail at once, even from buffered data.
    reader.seek(SeekFrom::Start(0)).unwrap();
    assert!(reader.read(&mut [0u8; 16]).is_err());
    let deadline = Instant::now() + Duration::from_secs(2);
    while !exited() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(exited(), "downloader thread did not stop");
}
