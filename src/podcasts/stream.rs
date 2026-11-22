//! Seekable playback of a remote episode without downloading it first.
//!
//! Audio is never stored: the reader keeps a bounded in-memory window and
//! the episode stays on the publisher's server (it is not imported, hashed
//! or offered to peers).
//!
//! [`open`] issues one `Range: bytes=0-` request and hands the response to a
//! dedicated downloader thread. The thread fills a bounded in-memory window
//! around the reader position:
//!
//! * up to [`READ_AHEAD`] bytes ahead of the reader (then it pauses),
//! * up to [`KEEP_BEHIND`] bytes behind it (cheap small backward seeks),
//! * plus the first [`HEAD_KEEP`] bytes of the file, kept permanently because
//!   decoders re-read the container header while probing.
//!
//! The window buffer is allocated once at its maximum size and never grows,
//! so memory stays bounded (and no large reallocation happens under the lock
//! the audio callback reads through).
//!
//! A seek inside the window is free. A read outside it restarts the
//! downloader at the new position with a fresh `Range` request; a generation
//! counter makes sure bytes from the superseded response are dropped. Servers
//! that ignore `Range` are still supported: forward seeks read and discard,
//! backward seeks restart from the beginning.
//!
//! Network errors: the downloader never gives up on a transient error by
//! itself. It resumes from its current offset with capped exponential backoff
//! (shortened while the reader is waiting for data), so a short outage is
//! bridged by the buffered window. The decision to fail belongs to the
//! reader: a `read` that is blocked on missing data and sees no download
//! progress for [`STARVE_TIMEOUT`] returns `io::ErrorKind::TimedOut`; the next
//! `read` starts a fresh attempt. Client errors (HTTP 4xx other than 408/429,
//! e.g. an expired signed URL) are reported to the reader immediately.
//!
//! Timeouts: `reqwest::blocking` applies `ClientBuilder::timeout` to waiting
//! for the response head and then *separately to every `Response::read`
//! call* (verified in reqwest 0.12.28: `blocking::Response::read` wraps each
//! read in `wait::timeout(.., self.timeout)`, and the blocking client does not
//! forward that value to the async client's total timeout). So
//! `timeout(STALL_TIMEOUT)` is a per-read stall timeout, not a cap on the whole
//! body, and long episodes are fine. The request-level
//! `RequestBuilder::timeout` (which *is* a total timeout) is never used.
//!
//! Cancellation: a stream belongs to one playback start ([`CancelToken`]).
//! Once that start is superseded (another item, stop, restart), every read,
//! including one blocked waiting for data inside the audio callback, fails
//! within [`CANCEL_POLL`], and the downloader thread stops. So a stalled
//! stream never holds the audio thread past the user's next choice, and a
//! superseded resume (which may be reading and discarding up to its offset)
//! stops downloading.
//!
//! End of playback: [`StreamProbe::end`] tells whether the reader saw the
//! file end the way the server described it or stopped on an error, so an
//! episode whose connection was lost is not taken as played to the end.
//!
//! Decoder note for the player integration: symphonia's MP3 demuxer in
//! accurate seek mode never seeks by byte offset (a backward seek re-parses
//! from the first frame), which would re-download the episode. Build rodio
//! decoders for these streams with `.with_coarse_seek(true)` (and
//! `.with_byte_len(..)`), so a seek maps to a single range request.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::{Client, Response};
use reqwest::header::{CONTENT_RANGE, CONTENT_TYPE, RANGE};
use reqwest::{StatusCode, Url};

use super::fetch::redact;

/// Maximum bytes buffered ahead of the reader position.
pub const READ_AHEAD: u64 = 8 * 1024 * 1024;
/// Bytes kept behind the reader position for small backward seeks.
pub const KEEP_BEHIND: u64 = 1024 * 1024;
/// Leading bytes of the file kept for the lifetime of the stream.
pub const HEAD_KEEP: usize = 256 * 1024;
/// Size of a single network read.
const CHUNK: usize = 64 * 1024;
/// Largest the window can get: the read-ahead may overshoot by less than one
/// chunk before the downloader pauses.
const WINDOW_CAP: usize = READ_AHEAD as usize + KEEP_BEHIND as usize + CHUNK;
/// A forward gap up to this size is bridged by waiting for the downloader
/// instead of issuing a new range request.
const NEAR_GAP: u64 = 512 * 1024;
/// A restart this close ahead of the current response offset keeps the
/// response and discards bytes instead of reconnecting.
const DISCARD_LIMIT: u64 = 256 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// No progress for this long on one read (or the response head) is an error.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// First retry delay; doubles per consecutive failure.
const RETRY_BASE: Duration = Duration::from_millis(250);
/// Longest delay between attempts while the reader still has buffered data.
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// Longest delay between attempts while the reader is waiting for data.
const STARVED_BACKOFF: Duration = Duration::from_secs(1);
/// A `read` blocked on missing data fails after this long without any
/// download progress. Shorter in tests to keep failure tests fast.
#[cfg(not(test))]
pub const STARVE_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
pub const STARVE_TIMEOUT: Duration = Duration::from_secs(4);
/// How often a blocked read or an idle downloader checks its
/// [`CancelToken`].
pub const CANCEL_POLL: Duration = Duration::from_millis(100);

/// Tells a stream that the playback start it belongs to was superseded.
#[derive(Clone)]
pub struct CancelToken(Option<Arc<dyn Fn() -> bool + Send + Sync>>);

impl CancelToken {
    /// A stream that is only closed by dropping it.
    #[cfg(test)]
    pub fn never() -> CancelToken {
        CancelToken(None)
    }

    /// `cancelled` becomes true once the start is superseded; it is polled.
    pub fn new(cancelled: impl Fn() -> bool + Send + Sync + 'static) -> CancelToken {
        CancelToken(Some(Arc::new(cancelled)))
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.as_ref().is_some_and(|cancelled| cancelled())
    }
}

/// The playback start was superseded while the stream was opened or read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the episode start was superseded")
    }
}

impl std::error::Error for Cancelled {}

fn cancelled_error() -> io::Error {
    io::Error::other(Cancelled)
}

/// How the reader's last read ended, for telling a finished episode from a
/// lost connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnd {
    /// No read failed and the server announced the file size, so the
    /// decoder ended at the end the server described.
    Complete,
    /// No read failed, but the server sent no size: a connection closed
    /// early looks exactly like the end of the file.
    Unknown,
    /// The last read failed (outage longer than [`STARVE_TIMEOUT`], a client
    /// error such as an expired URL, or cancellation).
    Failed,
}

/// Read-only view of a stream's state that outlives handing the reader to
/// the decoder.
#[derive(Clone)]
pub struct StreamProbe(Option<Arc<Shared>>);

impl std::fmt::Debug for StreamProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StreamProbe").field(&self.end()).finish()
    }
}

impl StreamProbe {
    pub fn local() -> Self {
        Self(None)
    }
    pub fn end(&self) -> StreamEnd {
        let Some(shared) = &self.0 else {
            return StreamEnd::Complete;
        };
        let state = shared.lock();
        if state.read_failed {
            StreamEnd::Failed
        } else if state.declared_total {
            StreamEnd::Complete
        } else {
            StreamEnd::Unknown
        }
    }
}

pub struct OpenedStream {
    pub reader: HttpStream,
    pub probe: StreamProbe,
    pub byte_len: Option<u64>,
    pub mime_type: Option<String>,
    pub seekable: bool,
}

/// `Read + Seek` view of a remote file; see the module docs.
pub struct HttpStream {
    pos: u64,
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    cancel: CancelToken,
    /// Signalled when data, EOF, an error or downloader exit is available to
    /// the reader.
    data_ready: Condvar,
    /// Signalled when the reader moved, requested a restart, started waiting
    /// or went away.
    space_ready: Condvar,
}

struct State {
    /// Bumped on every restart; downloader output for older generations is
    /// discarded.
    generation: u64,
    /// Start offset requested for the current generation.
    target: u64,
    /// File offset of `window[0]`.
    window_start: u64,
    window: VecDeque<u8>,
    /// Bytes `0..head.len()` of the file.
    head: Vec<u8>,
    /// Last position the reader consumed from the window.
    reader_pos: u64,
    total: Option<u64>,
    /// `total` comes from the server (`Content-Length` / `Content-Range`),
    /// not from a connection that closed.
    declared_total: bool,
    /// The reader's last read returned an error (cleared by data).
    read_failed: bool,
    ranges: bool,
    /// The current generation reached the end of the body.
    done: bool,
    /// The current generation was abandoned (fatal error, or the reader gave
    /// up waiting); the downloader idles until the reader restarts it.
    failed: bool,
    /// Fatal error to report to the reader once.
    error: Option<(io::ErrorKind, String)>,
    /// Most recent transient failure, for the reader's timeout message.
    last_error: Option<String>,
    /// Bytes received from the network for the current generation
    /// (including discarded ones); the reader's stall detection watches it.
    progress: u64,
    /// The reader is blocked waiting for data.
    reader_waiting: bool,
    /// The reader went away.
    closed: bool,
    /// The downloader thread exited.
    exited: bool,
    peak_buffered: usize,
    peak_capacity: usize,
}

impl State {
    fn window_end(&self) -> u64 {
        self.window_start + self.window.len() as u64
    }

    fn buffered(&self) -> usize {
        self.window.len() + self.head.len()
    }

    fn restart_at(&mut self, pos: u64) {
        self.generation = self.generation.wrapping_add(1);
        self.target = pos;
        self.window_start = pos;
        // Keeps the allocation: the window never needs more than WINDOW_CAP.
        self.window.clear();
        self.done = false;
        self.failed = false;
        self.error = None;
        self.last_error = None;
        self.progress = 0;
    }

    fn trim_behind(&mut self) {
        let keep_from = self.reader_pos.saturating_sub(KEEP_BEHIND);
        if keep_from > self.window_start {
            let drop = (keep_from - self.window_start).min(self.window.len() as u64) as usize;
            self.window.drain(..drop);
            self.window_start += drop as u64;
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// [`open_with`] without cancellation.
#[cfg(test)]
pub fn open(url: &str) -> Result<OpenedStream> {
    open_with(url, CancelToken::never())
}

/// Opens a remote audio file for streaming playback. Blocking: call it from
/// `spawn_blocking` or a plain thread, never directly on a Tokio worker.
///
/// Only http(s) URLs are accepted. Errors never contain the URL's query,
/// which may hold a private token. Fails with [`Cancelled`] when `cancel`
/// fires before the stream is ready.
pub fn open_with(url: &str, cancel: CancelToken) -> Result<OpenedStream> {
    let parsed = Url::parse(url).context("the episode audio URL is invalid")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("unsupported audio URL scheme {:?}", parsed.scheme());
    }
    let shown = redact(&parsed);
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        // Per-read stall timeout, see the module docs.
        .timeout(STALL_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                attempt.error("too many redirects")
            } else if matches!(attempt.url().scheme(), "http" | "https") {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .user_agent(concat!(
            "furumi/",
            env!("CARGO_PKG_VERSION"),
            " (podcast reader)"
        ))
        .build()
        .context("building the streaming HTTP client")?;
    let response = client
        .get(parsed)
        .header(RANGE, "bytes=0-")
        .send()
        .map_err(|err| anyhow!(err.without_url()))
        .with_context(|| format!("requesting {shown}"))?;
    if cancel.is_cancelled() {
        return Err(Cancelled.into());
    }
    let opened = classify(&response, 0).with_context(|| format!("requesting {shown}"))?;
    let mime_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());

    let window = VecDeque::with_capacity(WINDOW_CAP);
    let peak_capacity = window.capacity();
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            generation: 0,
            target: 0,
            window_start: 0,
            window,
            head: Vec::with_capacity(HEAD_KEEP),
            reader_pos: 0,
            total: opened.total,
            declared_total: opened.total.is_some(),
            read_failed: false,
            ranges: opened.ranges,
            done: false,
            failed: false,
            error: None,
            last_error: None,
            progress: 0,
            reader_waiting: false,
            closed: false,
            exited: false,
            peak_buffered: 0,
            peak_capacity,
        }),
        cancel,
        data_ready: Condvar::new(),
        space_ready: Condvar::new(),
    });
    // Requests continue at the final URL of the redirect chain.
    let downloader = Downloader {
        shared: Arc::clone(&shared),
        client,
        url: response.url().clone(),
    };
    std::thread::Builder::new()
        .name("podcast-stream".to_string())
        .spawn(move || downloader.run(response))
        .context("spawning the podcast stream thread")?;
    Ok(OpenedStream {
        probe: StreamProbe(Some(Arc::clone(&shared))),
        reader: HttpStream { pos: 0, shared },
        byte_len: opened.total,
        mime_type,
        seekable: opened.total.is_some(),
    })
}

struct Classified {
    /// Offset of the first body byte.
    start: u64,
    total: Option<u64>,
    ranges: bool,
}

/// Interprets a response to `Range: bytes=<requested>-` (or a plain GET).
fn classify(response: &Response, requested: u64) -> Result<Classified> {
    let status = response.status();
    if status == StatusCode::PARTIAL_CONTENT {
        let range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range);
        let Some((start, total)) = range else {
            bail!("HTTP 206 without a usable Content-Range");
        };
        if start != requested {
            bail!("server returned range starting at {start}, expected {requested}");
        }
        return Ok(Classified {
            start,
            total,
            ranges: true,
        });
    }
    if status.is_success() {
        return Ok(Classified {
            start: 0,
            total: response.content_length(),
            ranges: false,
        });
    }
    Err(StreamStatusError(status.as_u16()).into())
}

/// The audio server answered with an error status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamStatusError(pub u16);

impl StreamStatusError {
    /// The URL itself is wrong or expired (4xx other than 408/429): the
    /// feed may know a newer address for the episode.
    pub fn url_is_stale(self) -> bool {
        (400..500).contains(&self.0) && self.0 != 408 && self.0 != 429
    }
}

impl std::fmt::Display for StreamStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match StatusCode::from_u16(self.0) {
            Ok(status) => write!(f, "HTTP {status}"),
            Err(_) => write!(f, "HTTP {}", self.0),
        }
    }
}

impl std::error::Error for StreamStatusError {}

/// Parses `bytes <start>-<end>/<total|*>` into `(start, total)`.
fn parse_content_range(value: &str) -> Option<(u64, Option<u64>)> {
    let value = value.trim();
    let rest = value
        .get(..6)
        .filter(|unit| unit.eq_ignore_ascii_case("bytes "))
        .map(|_| &value[6..])?;
    let (range, total) = rest.split_once('/')?;
    let (start, _end) = range.trim().split_once('-')?;
    let start = start.trim().parse().ok()?;
    let total = match total.trim() {
        "*" => None,
        total => Some(total.parse().ok()?),
    };
    Some((start, total))
}

/// Why a (re)connection attempt failed.
enum Failure {
    /// Worth retrying (network error, 5xx, 408/429, odd range answer).
    Transient(String),
    /// Retrying will not help (4xx such as 403/404/410).
    Fatal(String),
}

struct Downloader {
    shared: Arc<Shared>,
    client: Client,
    url: Url,
}

/// Marks the downloader as exited however `run` returns (including a panic).
struct ExitFlag(Arc<Shared>);

impl Drop for ExitFlag {
    fn drop(&mut self) {
        // Set under the lock so a reader that checked `exited` and is about
        // to wait cannot miss the notification.
        let mut state = self.0.lock();
        state.exited = true;
        drop(state);
        self.0.data_ready.notify_all();
    }
}

impl Downloader {
    fn run(self, first: Response) {
        let _exit = ExitFlag(Arc::clone(&self.shared));
        let mut generation = 0u64;
        let mut response = Some(first);
        // File offset of the next byte `response` yields, or where the next
        // request should start when there is no response.
        let mut offset = 0u64;
        // Consecutive failures of the current generation (drives backoff).
        let mut failures = 0u32;
        let mut buf = vec![0u8; CHUNK];
        loop {
            if !self.wait_for_work(&mut generation, &mut response, &mut offset, &mut failures) {
                return;
            }
            if response.is_none() {
                match self.reopen(offset) {
                    Ok(Some((next, start))) => {
                        response = Some(next);
                        offset = start;
                    }
                    Ok(None) => {
                        // Nothing left at this offset (HTTP 416).
                        self.mark_done(generation, offset, false);
                        continue;
                    }
                    Err(Failure::Fatal(message)) => {
                        failures = 0;
                        self.fail(generation, message);
                        continue;
                    }
                    Err(Failure::Transient(message)) => {
                        if !self.retry(generation, &mut failures, &message) {
                            return;
                        }
                        continue;
                    }
                }
            }
            let Some(body) = response.as_mut() else {
                continue;
            };
            let read = body.read(&mut buf);
            if self.shared.cancel.is_cancelled() {
                return;
            }
            match read {
                Ok(0) => {
                    response = None;
                    let total = self.shared.lock().total;
                    if total.is_some_and(|total| offset < total) {
                        // Connection closed early: resume from `offset`.
                        let message = format!("connection closed at byte {offset}");
                        if !self.retry(generation, &mut failures, &message) {
                            return;
                        }
                        continue;
                    }
                    self.mark_done(generation, offset, true);
                }
                Ok(read) => {
                    failures = 0;
                    let start = offset;
                    offset += read as u64;
                    self.publish(generation, start, &buf[..read]);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => {
                    response = None;
                    if !self.retry(generation, &mut failures, &err.to_string()) {
                        return;
                    }
                }
            }
        }
    }

    /// Blocks until there is something to download, adopting restarts
    /// requested by the reader. Returns `false` when the reader is gone.
    fn wait_for_work(
        &self,
        generation: &mut u64,
        response: &mut Option<Response>,
        offset: &mut u64,
        failures: &mut u32,
    ) -> bool {
        let mut state = self.shared.lock();
        loop {
            if state.closed || self.shared.cancel.is_cancelled() {
                return false;
            }
            if state.generation != *generation {
                *generation = state.generation;
                // A new generation gets a fresh backoff.
                *failures = 0;
                // Keep the open response when the new target is a little
                // ahead of it (or anywhere ahead if ranges are unsupported):
                // discarding is cheaper than reconnecting.
                let keep = response.is_some()
                    && state.target >= *offset
                    && (!state.ranges || state.target - *offset <= DISCARD_LIMIT);
                if !keep {
                    *response = None;
                    *offset = state.target;
                }
                continue;
            }
            let paused = state.done
                || state.failed
                || state.window_end() >= state.reader_pos.saturating_add(READ_AHEAD);
            if !paused {
                return true;
            }
            state = self
                .shared
                .space_ready
                .wait_timeout(state, CANCEL_POLL)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Requests the file from `offset` (from 0 when ranges are unsupported).
    /// `Ok(None)` means the server has nothing at that offset.
    fn reopen(&self, offset: u64) -> std::result::Result<Option<(Response, u64)>, Failure> {
        let ranges = self.shared.lock().ranges;
        let mut request = self.client.get(self.url.clone());
        if ranges {
            request = request.header(RANGE, format!("bytes={offset}-"));
        }
        let response = request
            .send()
            .map_err(|err| Failure::Transient(format!("{:#}", anyhow!(err.without_url()))))?;
        let status = response.status();
        if ranges && status == StatusCode::RANGE_NOT_SATISFIABLE {
            return Ok(None);
        }
        if status.is_client_error()
            && status != StatusCode::REQUEST_TIMEOUT
            && status != StatusCode::TOO_MANY_REQUESTS
        {
            return Err(Failure::Fatal(format!("HTTP {status}")));
        }
        let classified = classify(&response, if ranges { offset } else { 0 })
            .map_err(|err| Failure::Transient(format!("{err:#}")))?;
        let mut state = self.shared.lock();
        if !classified.ranges && state.ranges {
            // The server stopped honouring Range: read from 0 and discard.
            state.ranges = false;
        }
        if state.total.is_none() && classified.total.is_some() {
            state.total = classified.total;
            state.declared_total = true;
        }
        Ok(Some((response, classified.start)))
    }

    fn mark_done(&self, generation: u64, offset: u64, learn_total: bool) {
        let mut state = self.shared.lock();
        if state.generation != generation {
            return;
        }
        state.done = true;
        if learn_total && state.total.is_none() {
            state.total = Some(offset.max(state.window_end()));
        }
        self.shared.data_ready.notify_all();
    }

    /// Abandons the current generation with an error for the reader.
    fn fail(&self, generation: u64, message: String) {
        let mut state = self.shared.lock();
        if state.generation != generation {
            return;
        }
        tracing::warn!("podcast stream failed: {message}");
        state.failed = true;
        state.error = Some((io::ErrorKind::Other, message));
        self.shared.data_ready.notify_all();
    }

    /// Records a transient failure and backs off before the next attempt.
    /// Returns `false` when the thread should exit.
    fn retry(&self, generation: u64, failures: &mut u32, message: &str) -> bool {
        let mut state = self.shared.lock();
        if state.closed {
            return false;
        }
        if state.generation != generation {
            // The reader moved on; `wait_for_work` adopts the new target.
            return true;
        }
        *failures = failures.saturating_add(1);
        state.last_error = Some(message.to_string());
        tracing::debug!("podcast stream attempt failed ({failures} in a row): {message}");
        let delay = RETRY_BASE
            .saturating_mul(1 << (*failures - 1).min(6))
            .min(MAX_BACKOFF);
        let started = Instant::now();
        // Wakes early when the reader goes away, seeks elsewhere or starts
        // waiting for data (which shortens the delay).
        loop {
            if state.closed || self.shared.cancel.is_cancelled() {
                return false;
            }
            if state.generation != generation {
                return true;
            }
            let limit = if state.reader_waiting {
                delay.min(STARVED_BACKOFF)
            } else {
                delay
            };
            let elapsed = started.elapsed();
            if elapsed >= limit {
                return true;
            }
            state = self
                .shared
                .space_ready
                .wait_timeout(state, (limit - elapsed).min(CANCEL_POLL))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn publish(&self, generation: u64, start: u64, bytes: &[u8]) {
        let mut state = self.shared.lock();
        if state.generation != generation {
            return;
        }
        state.progress = state.progress.wrapping_add(bytes.len() as u64);
        state.last_error = None;
        let end = start + bytes.len() as u64;
        // Permanent copy of the file head for format probing.
        let head_len = state.head.len() as u64;
        if start <= head_len && end > head_len && head_len < HEAD_KEEP as u64 {
            let from = (head_len - start) as usize;
            let to = (HEAD_KEEP as u64).min(end) - start;
            state.head.extend_from_slice(&bytes[from..to as usize]);
        }
        // Only bytes at or after the window end are new; earlier ones are
        // being skipped (Range-less seek or resume) or are already buffered.
        let want = state.window_end();
        if end > want {
            if start > want {
                // A gap should not happen; resynchronise rather than corrupt.
                state.window.clear();
                state.window_start = start;
                state.window.extend(bytes);
            } else {
                state.window.extend(&bytes[(want - start) as usize..]);
            }
            state.trim_behind();
        }
        state.peak_buffered = state.peak_buffered.max(state.buffered());
        state.peak_capacity = state.peak_capacity.max(state.window.capacity());
        self.shared.data_ready.notify_all();
    }
}

fn downloader_stopped() -> io::Error {
    io::Error::other("podcast stream downloader stopped")
}

impl Read for HttpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let shared = &*self.shared;
        let state = shared.lock();
        let (mut state, result) = read_locked(shared, &mut self.pos, state, buf);
        state.reader_waiting = false;
        match &result {
            Ok(0) => {}
            Ok(_) => state.read_failed = false,
            Err(_) => state.read_failed = true,
        }
        result
    }
}

/// The body of [`HttpStream::read`]; hands the guard back so the caller can
/// clear `reader_waiting` on every exit path.
fn read_locked<'a>(
    shared: &Shared,
    pos: &mut u64,
    mut state: MutexGuard<'a, State>,
    buf: &mut [u8],
) -> (MutexGuard<'a, State>, io::Result<usize>) {
    // When this read started waiting, and the download progress then.
    let mut starved: Option<(Instant, u64)> = None;
    loop {
        if shared.cancel.is_cancelled() {
            return (state, Err(cancelled_error()));
        }
        if state.total.is_some_and(|total| *pos >= total) {
            return (state, Ok(0));
        }
        if *pos < state.head.len() as u64 {
            let from = *pos as usize;
            let len = buf.len().min(state.head.len() - from);
            buf[..len].copy_from_slice(&state.head[from..from + len]);
            *pos += len as u64;
            return (state, Ok(len));
        }
        let window_end = state.window_end();
        if *pos >= state.window_start && *pos < window_end {
            let from = (*pos - state.window_start) as usize;
            let len = buf.len().min(state.window.len() - from);
            let (front, back) = state.window.as_slices();
            copy_from_ring(front, back, from, &mut buf[..len]);
            *pos += len as u64;
            state.reader_pos = *pos;
            // Free what fell out of the keep-behind range even when the
            // downloader is idle (paused or done).
            state.trim_behind();
            shared.space_ready.notify_all();
            return (state, Ok(len));
        }
        // Not buffered. Wait for the current generation if it is heading
        // here, otherwise restart the downloader at the reader position.
        let reachable = *pos >= state.window_start
            && (!state.ranges || *pos <= window_end.saturating_add(NEAR_GAP));
        if reachable && !state.failed {
            if state.done {
                return (state, Ok(0));
            }
            if state.exited {
                return (state, Err(downloader_stopped()));
            }
            if state.reader_pos != *pos {
                state.reader_pos = *pos;
                state.trim_behind();
                shared.space_ready.notify_all();
            }
            let now = Instant::now();
            let since = match starved {
                Some((since, seen)) if seen == state.progress => since,
                _ => {
                    starved = Some((now, state.progress));
                    now
                }
            };
            let waited = now.saturating_duration_since(since);
            if waited >= STARVE_TIMEOUT {
                // Give up on this generation; the next read retries.
                state.failed = true;
                state.error = None;
                let cause = state
                    .last_error
                    .take()
                    .unwrap_or_else(|| "no data received".to_string());
                let error = io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "podcast stream stalled for {}s: {cause}",
                        STARVE_TIMEOUT.as_secs()
                    ),
                );
                return (state, Err(error));
            }
            if !state.reader_waiting {
                // Lets a downloader in backoff retry sooner.
                state.reader_waiting = true;
                shared.space_ready.notify_all();
            }
            state = shared
                .data_ready
                .wait_timeout(state, (STARVE_TIMEOUT - waited).min(CANCEL_POLL))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            continue;
        }
        if let Some((kind, message)) = state.error.take() {
            // Report once; the next read retries from scratch.
            return (state, Err(io::Error::new(kind, message)));
        }
        if state.exited {
            return (state, Err(downloader_stopped()));
        }
        state.restart_at(*pos);
        state.reader_pos = *pos;
        starved = None;
        shared.space_ready.notify_all();
    }
}

fn copy_from_ring(front: &[u8], back: &[u8], from: usize, out: &mut [u8]) {
    let mut written = 0;
    if from < front.len() {
        let len = out.len().min(front.len() - from);
        out[..len].copy_from_slice(&front[from..from + len]);
        written = len;
    }
    if written < out.len() {
        let back_from = from + written - front.len();
        let len = out.len() - written;
        out[written..].copy_from_slice(&back[back_from..back_from + len]);
    }
}

impl Seek for HttpStream {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let target = match position {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::Current(delta) => i128::from(self.pos) + i128::from(delta),
            SeekFrom::End(delta) => {
                let Some(total) = self.shared.lock().total else {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "stream length is unknown",
                    ));
                };
                i128::from(total) + i128::from(delta)
            }
        };
        let target = u64::try_from(target).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a negative or overflowing position",
            )
        })?;
        // Lazy: the downloader is only redirected when a read needs data.
        self.pos = target;
        Ok(target)
    }
}

impl Drop for HttpStream {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.closed = true;
        state.window = VecDeque::new();
        state.head = Vec::new();
        drop(state);
        self.shared.space_ready.notify_all();
        self.shared.data_ready.notify_all();
    }
}

#[cfg(test)]
impl HttpStream {
    /// Bytes currently held in memory (window + head).
    pub(super) fn buffered_bytes(&self) -> usize {
        self.shared.lock().buffered()
    }

    /// Highest number of bytes ever held in memory at once.
    pub(super) fn peak_buffered_bytes(&self) -> usize {
        self.shared.lock().peak_buffered
    }

    /// Largest allocation (window + head capacity) ever held.
    pub(super) fn peak_allocated_bytes(&self) -> usize {
        let state = self.shared.lock();
        state.peak_capacity + state.head.capacity()
    }

    /// Flag the downloader thread sets when it exits.
    pub(super) fn exit_probe(&self) -> impl Fn() -> bool + Send + 'static {
        let shared = Arc::clone(&self.shared);
        move || shared.lock().exited
    }
}

/// Upper bound of [`HttpStream`] memory use, for tests.
#[cfg(test)]
pub(super) const MAX_BUFFERED: usize = WINDOW_CAP + HEAD_KEEP;
