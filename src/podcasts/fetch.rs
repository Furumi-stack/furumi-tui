//! Async HTTP for podcast feeds and cover images.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::header::{ACCEPT, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use reqwest::{StatusCode, Url};

use super::feed::{ParsedFeed, parse_feed_with_base};

/// Hard cap on a feed document (large catalogs are a few MiB).
pub const MAX_FEED_BYTES: usize = 32 * 1024 * 1024;
/// Hard cap on a cover image.
pub const MAX_COVER_BYTES: usize = 10 * 1024 * 1024;

const FEED_ACCEPT: &str =
    "application/rss+xml, application/atom+xml, application/xml;q=0.9, text/xml;q=0.9, */*;q=0.5";

pub enum FeedFetch {
    NotModified,
    Fetched {
        feed: ParsedFeed,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

pub struct Fetcher {
    client: reqwest::Client,
    covers_dir: PathBuf,
}

impl Fetcher {
    pub fn new(covers_dir: PathBuf) -> Result<Fetcher> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(10))
            .user_agent(concat!("furumi/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building the podcast HTTP client")?;
        Ok(Fetcher { client, covers_dir })
    }

    /// Conditional GET of a feed. Parsing runs on the blocking pool.
    pub async fn fetch_feed(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<FeedFetch> {
        let mut request = self.client.get(url).header(ACCEPT, FEED_ACCEPT);
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = last_modified {
            request = request.header(IF_MODIFIED_SINCE, last_modified);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("fetching feed {url}"))?;
        let status = response.status();
        if status == StatusCode::NOT_MODIFIED {
            return Ok(FeedFetch::NotModified);
        }
        if !status.is_success() {
            bail!("feed request failed: HTTP {status}");
        }
        let header = |name| {
            response
                .headers()
                .get(name)
                .and_then(|value: &reqwest::header::HeaderValue| value.to_str().ok())
                .map(str::to_string)
        };
        let etag = header(ETAG);
        let last_modified = header(LAST_MODIFIED);
        // Relative enclosure URLs resolve against where the feed really is
        // (after redirects).
        let base = response.url().to_string();
        let body = read_capped(response, MAX_FEED_BYTES, "feed").await?;
        let feed = tokio::task::spawn_blocking(move || parse_feed_with_base(&body, Some(&base)))
            .await
            .map_err(|err| anyhow!("feed parser task failed: {err}"))??;
        Ok(FeedFetch::Fetched {
            feed,
            etag,
            last_modified,
        })
    }

    /// Downloads a cover image into the covers directory and returns its path.
    pub async fn fetch_cover(&self, podcast_id: i64, image_url: &str) -> Result<PathBuf> {
        let response = self
            .client
            .get(image_url)
            .send()
            .await
            .with_context(|| format!("fetching cover {image_url}"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("cover request failed: HTTP {status}");
        }
        let body = read_capped(response, MAX_COVER_BYTES, "cover image").await?;
        let format = image::guess_format(&body).context("cover is not a supported image")?;
        let extension = format.extensions_str().first().copied().unwrap_or("img");
        let hash = blake3::hash(image_url.as_bytes()).to_hex();
        let name = format!("{podcast_id}-{}.{extension}", &hash.as_str()[..16]);
        let dir = self.covers_dir.clone();
        tokio::task::spawn_blocking(move || write_atomically(&dir, &name, &body))
            .await
            .map_err(|err| anyhow!("cover writer task failed: {err}"))?
    }
}

async fn read_capped(mut response: reqwest::Response, cap: usize, what: &str) -> Result<Vec<u8>> {
    if let Some(len) = response.content_length()
        && len > cap as u64
    {
        bail!("{what} is too large ({len} bytes, limit {cap})");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("reading {what}"))?
    {
        if body.len() + chunk.len() > cap {
            bail!("{what} is too large (over {cap} bytes)");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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

/// Whether `text` starts with `<scheme>://`. Only a prefix counts: a URL
/// embedded in the query (`a.test/rss?src=https://b.test`) is not a scheme.
fn has_scheme(text: &str) -> bool {
    let Some((scheme, rest)) = text.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        && rest.starts_with("//")
}

/// Normalizes user input into a feed URL: trims, maps `feed://`,
/// `podcast://`, `pcast://` and `itpc://` to `https://`, defaults to
/// `https://` without a scheme and accepts only http(s) URLs with a host.
pub fn normalize_feed_url(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("feed URL is empty");
    }
    if trimmed.chars().any(char::is_whitespace) {
        bail!("feed URL must not contain whitespace");
    }
    let lower = trimmed.to_ascii_lowercase();
    let mut candidate = trimmed.to_string();
    for scheme in ["feed:", "podcast:", "pcast:", "itpc:"] {
        if lower.starts_with(scheme) {
            let rest = &trimmed[scheme.len()..];
            let rest_lower = rest.to_ascii_lowercase();
            candidate = if rest_lower.starts_with("http://") || rest_lower.starts_with("https://") {
                rest.to_string()
            } else {
                format!("https://{}", rest.trim_start_matches('/'))
            };
            break;
        }
    }
    if !has_scheme(&candidate) {
        candidate = format!("https://{candidate}");
    }
    let url = Url::parse(&candidate).with_context(|| format!("invalid feed URL {trimmed:?}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("unsupported feed URL scheme {:?}", url.scheme());
    }
    if url.host_str().is_none_or(str::is_empty) {
        bail!("feed URL has no host");
    }
    Ok(url.to_string())
}
