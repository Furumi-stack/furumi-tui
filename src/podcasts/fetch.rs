//! HTTP for podcast feeds and cover images.
//!
//! Redirects are followed by hand so the caller learns where the feed
//! really lives: the address reached by the leading permanent (301/308)
//! hops is where the feed moved, and the store records it as the new fetch
//! URL (the old one becomes an alias). Temporary hops after it (a CDN or a
//! tracking prefix) do not move the feed further. Conditional requests
//! (`If-None-Match` / `If-Modified-Since`), response size caps, timeouts and
//! the user agent are fixed here; parsing and storage happen elsewhere.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use music_dht::podcast::{FeedUrl, PodcastId};
use reqwest::header::{
    ACCEPT, ETAG, HeaderMap, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, LOCATION, RETRY_AFTER,
};
use reqwest::{StatusCode, Url};

/// Hard cap on a feed document (large catalogs are a few MiB).
pub const MAX_FEED_BYTES: usize = 32 * 1024 * 1024;
/// Hard cap on a cover image.
pub const MAX_COVER_BYTES: usize = 10 * 1024 * 1024;
/// Redirect hops followed before giving up.
pub const MAX_REDIRECTS: usize = 10;
/// TCP + TLS connection setup.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Whole request, including the body, per hop.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Sent with every podcast request.
pub const USER_AGENT: &str = concat!("furumi/", env!("CARGO_PKG_VERSION"), " (podcast reader)");

const FEED_ACCEPT: &str = "application/rss+xml, application/atom+xml, application/xml;q=0.9, \
     text/xml;q=0.9, */*;q=0.5";

/// HTTP validators of the last successful fetch.
#[derive(Debug, Clone, Copy, Default)]
pub struct Validators<'a> {
    pub etag: Option<&'a str>,
    pub last_modified: Option<&'a str>,
}

/// What a feed request returned.
#[derive(Debug)]
pub enum FeedBody {
    /// `304 Not Modified` to a conditional request.
    NotModified,
    /// A new document with its validators.
    Modified {
        body: Vec<u8>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

/// A feed response and the redirect chain that led to it.
#[derive(Debug)]
pub struct FeedResponse {
    pub body: FeedBody,
    /// URL that finally answered, canonicalized; the base for relative
    /// URLs in the document.
    pub final_url: FeedUrl,
    /// Where the leading permanent redirects (301/308, one or more, before
    /// any temporary one) lead: the feed's new address. `None` when the
    /// first hop was temporary or there was none. Temporary redirects
    /// (tracking prefixes, CDNs) never move a feed.
    pub moved_permanently_to: Option<FeedUrl>,
    /// Redirect hops followed.
    pub redirects: usize,
}

/// A non-success HTTP status, with the server's `Retry-After` (seconds form)
/// so the scheduler can back off as asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpStatusError {
    pub status: u16,
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = StatusCode::from_u16(self.status)
            .ok()
            .and_then(|status| status.canonical_reason())
            .unwrap_or("error");
        write!(f, "HTTP {} {reason}", self.status)
    }
}

impl std::error::Error for HttpStatusError {}

/// Shared HTTP client for feeds and covers.
pub struct Fetcher {
    client: reqwest::Client,
    covers_dir: PathBuf,
}

impl Fetcher {
    pub fn new(covers_dir: PathBuf) -> Result<Fetcher> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(USER_AGENT)
            .build()
            .context("building the podcast HTTP client")?;
        Ok(Fetcher { client, covers_dir })
    }

    /// GET of a feed. `validators` make it conditional; they are dropped
    /// after a permanent redirect because they belong to the old URL.
    pub async fn fetch_feed(
        &self,
        url: &FeedUrl,
        validators: Validators<'_>,
    ) -> Result<FeedResponse> {
        let Followed {
            response,
            url: final_url,
            moved_permanently_to,
            redirects,
            conditional,
        } = self
            .get_following(url.as_str(), Some(FEED_ACCEPT), validators)
            .await?;
        let final_url = FeedUrl::parse(final_url.as_str()).map_err(|err| {
            anyhow!(
                "the feed was redirected to an unsupported URL {}: {err}",
                redact(&final_url)
            )
        })?;
        // An address that is not a valid feed URL is not worth moving to.
        let moved_permanently_to =
            moved_permanently_to.and_then(|url| FeedUrl::parse(url.as_str()).ok());
        let status = response.status();
        if status == StatusCode::NOT_MODIFIED {
            if !conditional {
                bail!("the server answered 304 Not Modified to an unconditional request");
            }
            return Ok(FeedResponse {
                body: FeedBody::NotModified,
                final_url,
                moved_permanently_to,
                redirects,
            });
        }
        if !status.is_success() {
            return Err(status_error(status, response.headers()).into());
        }
        let etag = header_string(response.headers(), ETAG);
        let last_modified = header_string(response.headers(), LAST_MODIFIED);
        let body = read_capped(response, MAX_FEED_BYTES, "feed").await?;
        Ok(FeedResponse {
            body: FeedBody::Modified {
                body,
                etag,
                last_modified,
            },
            final_url,
            moved_permanently_to,
            redirects,
        })
    }

    /// Downloads a cover image into the covers directory; returns its path.
    pub async fn fetch_cover(&self, podcast_id: &PodcastId, image_url: &str) -> Result<PathBuf> {
        let response = self
            .get_following(image_url, None, Validators::default())
            .await?
            .response;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(status, response.headers()).into());
        }
        let body = read_capped(response, MAX_COVER_BYTES, "cover image").await?;
        let format = image::guess_format(&body).context("the cover is not a supported image")?;
        let extension = format.extensions_str().first().copied().unwrap_or("img");
        let name = cover_file_name(podcast_id, image_url, extension);
        let dir = self.covers_dir.clone();
        tokio::task::spawn_blocking(move || write_atomically(&dir, &name, &body))
            .await
            .map_err(|err| anyhow!("cover writer task failed: {err}"))?
    }

    /// GET following up to [`MAX_REDIRECTS`] http(s) redirects by hand.
    async fn get_following(
        &self,
        url: &str,
        accept: Option<&str>,
        validators: Validators<'_>,
    ) -> Result<Followed> {
        let mut current = Url::parse(url).with_context(|| format!("invalid URL {url:?}"))?;
        check_scheme(&current)?;
        // Still in the leading run of permanent hops.
        let mut permanent_run = true;
        let mut moved_permanently_to = None;
        let mut send_validators = true;
        let mut hops = 0;
        loop {
            let mut request = self.client.get(current.clone());
            if let Some(accept) = accept {
                request = request.header(ACCEPT, accept);
            }
            if send_validators {
                if let Some(etag) = validators.etag {
                    request = request.header(IF_NONE_MATCH, etag);
                }
                if let Some(last_modified) = validators.last_modified {
                    request = request.header(IF_MODIFIED_SINCE, last_modified);
                }
            }
            let response = request
                .send()
                .await
                // reqwest errors print the full URL, query tokens included.
                .map_err(reqwest::Error::without_url)
                .with_context(|| format!("requesting {}", redact(&current)))?;
            let status = response.status();
            if !status.is_redirection() || status == StatusCode::NOT_MODIFIED {
                let conditional = send_validators
                    && (validators.etag.is_some() || validators.last_modified.is_some());
                return Ok(Followed {
                    response,
                    url: current,
                    moved_permanently_to,
                    redirects: hops,
                    conditional,
                });
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| anyhow!("HTTP {status} redirect without a Location header"))?;
            let next = current
                .join(location.trim())
                .with_context(|| format!("invalid redirect location {location:?}"))?;
            check_scheme(&next)?;
            hops += 1;
            if hops > MAX_REDIRECTS {
                bail!("too many redirects (more than {MAX_REDIRECTS})");
            }
            let permanent = matches!(
                status,
                StatusCode::MOVED_PERMANENTLY | StatusCode::PERMANENT_REDIRECT
            );
            permanent_run &= permanent;
            if permanent_run {
                moved_permanently_to = Some(next.clone());
            }
            if permanent {
                send_validators = false;
            }
            current = next;
        }
    }
}

struct Followed {
    response: reqwest::Response,
    /// URL that answered.
    url: Url,
    /// Target of the leading permanent hops.
    moved_permanently_to: Option<Url>,
    redirects: usize,
    /// The final request carried validators.
    conditional: bool,
}

fn check_scheme(url: &Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!("unsupported URL scheme {:?}", url.scheme());
    }
    if url.host_str().is_none_or(str::is_empty) {
        bail!("URL has no host");
    }
    Ok(())
}

/// The URL without query and fragment, for messages and logs: feed URLs
/// may carry private tokens in their query.
pub fn redact(url: &Url) -> String {
    let mut shown = url.clone();
    if shown.query().is_some() {
        shown.set_query(Some("…"));
    }
    shown.set_fragment(None);
    shown.to_string()
}

fn header_string(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn status_error(status: StatusCode, headers: &HeaderMap) -> HttpStatusError {
    let retry_after = header_string(headers, RETRY_AFTER)
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs);
    HttpStatusError {
        status: status.as_u16(),
        retry_after,
    }
}

async fn read_capped(mut response: reqwest::Response, cap: usize, what: &str) -> Result<Vec<u8>> {
    if let Some(len) = response.content_length()
        && len > cap as u64
    {
        bail!("the {what} is too large ({len} bytes, limit {cap})");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)
        .with_context(|| format!("reading the {what}"))?
    {
        if body.len() + chunk.len() > cap {
            bail!("the {what} is too large (over {cap} bytes)");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Stable per podcast and image URL, so a changed image gets a new file and
/// the previous one can be removed.
fn cover_file_name(podcast_id: &PodcastId, image_url: &str, extension: &str) -> String {
    let podcast = blake3::hash(podcast_id.as_str().as_bytes()).to_hex();
    let image = blake3::hash(image_url.as_bytes()).to_hex();
    format!("{}-{}.{extension}", &podcast[..16], &image[..16])
}

fn write_atomically(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let target = dir.join(name);
    let temp = dir.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = std::fs::write(&temp, bytes)
        .and_then(|()| std::fs::rename(&temp, &target))
        .with_context(|| format!("writing {}", target.display()));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result?;
    Ok(target)
}
