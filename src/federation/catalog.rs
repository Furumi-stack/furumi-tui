//! The peer catalog protocol: one peer asks another for its library slice
//! of a single artist (releases with full tracklists plus featured
//! appearances), used to assemble a federated artist card.
//!
//! Wire shape on the `furumi-fd/catalog/1` ALPN: the requester sends one
//! JSON line ([`CatalogRequest`]) and finishes; the owner answers with one
//! JSON document ([`CatalogResponse`]) and finishes. All fields default, so
//! the shape is extensible like the audio protocol.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use music_dht::{ByteStream, EndpointId, ItemKind, MusicDhtService, StreamAcceptor};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::library::Library;

/// ALPN of the catalog protocol.
pub const CATALOG_ALPN: &[u8] = b"furumi-fd/catalog/1";

/// Upper bound for one catalog response (thousands of tracks fit easily).
const MAX_CATALOG_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct CatalogRequest {
    /// Artist display name; matched case-insensitively by the owner.
    artist: String,
    /// What is being asked for: `None`/"catalog" — the JSON catalog;
    /// "artist_image" — the artist's image; "release_cover" — the cover of
    /// `release`. Image responses are a JSON header line + raw bytes.
    #[serde(default)]
    want: Option<String>,
    #[serde(default)]
    release: Option<String>,
}

/// Header line preceding raw image bytes (artist image / release cover).
#[derive(Debug, Default, Serialize, Deserialize)]
struct ImageHeader {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    mime_type: String,
    #[serde(default)]
    size: u64,
}

/// Images above this size are skipped rather than transferred.
const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Default, Serialize, Deserialize)]
struct CatalogResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    artist: Option<CatalogArtist>,
}

/// One peer's library slice for an artist.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogArtist {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub releases: Vec<CatalogRelease>,
    /// Tracks where the requested artist is featured instead of being a
    /// release/main artist.
    #[serde(default)]
    pub appears_on: Vec<CatalogAppearance>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogRelease {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub release_type: String,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub tracks: Vec<CatalogTrack>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogAppearance {
    #[serde(default)]
    pub release_title: String,
    #[serde(default)]
    pub release_type: String,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub track: CatalogTrack,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogTrack {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default)]
    pub featured_artists: Vec<String>,
    #[serde(default)]
    pub track_number: Option<i32>,
    #[serde(default)]
    pub disc_number: Option<i32>,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    /// Stable audio content id (`b3:<64 hex>`) when known.
    #[serde(default)]
    pub content_id: Option<String>,
    /// Hex DHT item id — the key the audio is requested by (FedPlay).
    #[serde(default)]
    pub item_id: String,
}

// ---------------------------------------------------------------------------
// Serving side
// ---------------------------------------------------------------------------

/// Runs the catalog accept loop until the acceptor closes.
pub async fn serve_peers(
    mut acceptor: StreamAcceptor,
    library: Arc<Library>,
    own: EndpointId,
    transport_stats: Arc<crate::federation::TransportStats>,
) {
    while let Some(stream) = acceptor.accept().await {
        let library = Arc::clone(&library);
        let transport_stats = Arc::clone(&transport_stats);
        tokio::spawn(async move {
            let peer = stream.peer_id;
            if let Err(err) = serve_one(stream, library, own, transport_stats).await {
                tracing::warn!(peer = %peer, "catalog request failed: {err:#}");
            }
        });
    }
}

async fn serve_one(
    mut stream: ByteStream,
    library: Arc<Library>,
    own: EndpointId,
    transport_stats: Arc<crate::federation::TransportStats>,
) -> Result<()> {
    crate::federation::record_stream_transport(
        &transport_stats,
        "catalog",
        "inbound",
        "open",
        &stream,
    );
    let request: CatalogRequest =
        serde_json::from_slice(&super::audio::read_line(&mut stream.recv).await?)?;
    tracing::info!(
        peer = %stream.peer_id,
        artist = %request.artist,
        want = request.want.as_deref().unwrap_or("catalog"),
        "peer requested a catalog"
    );

    match request.want.as_deref() {
        None | Some("catalog") => {
            let response =
                tokio::task::spawn_blocking(move || build_catalog(&library, own, &request.artist))
                    .await?
                    .unwrap_or_else(|err| CatalogResponse {
                        ok: false,
                        error: Some(format!("catalog lookup failed: {err:#}")),
                        artist: None,
                    });
            let payload = serde_json::to_vec(&response)?;
            stream.send.write_all(&payload).await?;
        }
        Some(want @ ("artist_image" | "release_cover")) => {
            let want_cover = want == "release_cover";
            let release = request.release.clone().unwrap_or_default();
            let artist = request.artist.clone();
            let path = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
                if want_cover {
                    library.release_cover_by_names(&artist, &release)
                } else {
                    let Some(artist_id) = library.artist_id_by_name(&artist)? else {
                        return Ok(None);
                    };
                    library.artist_image(artist_id)
                }
            })
            .await??;
            serve_image(&mut stream, path.as_deref()).await?;
        }
        Some(other) => {
            let response = CatalogResponse {
                ok: false,
                error: Some(format!("unknown request kind '{other}'")),
                artist: None,
            };
            stream
                .send
                .write_all(&serde_json::to_vec(&response)?)
                .await?;
        }
    }
    stream.send.finish()?;
    let _ = stream.send.stopped().await;
    crate::federation::record_stream_transport(
        &transport_stats,
        "catalog",
        "inbound",
        "done",
        &stream,
    );
    Ok(())
}

/// Streams one image file: header line, then the raw bytes.
async fn serve_image(stream: &mut ByteStream, path: Option<&str>) -> Result<()> {
    let loaded = match path {
        Some(path) => match tokio::fs::read(path).await {
            Ok(bytes) if !bytes.is_empty() && bytes.len() as u64 <= MAX_IMAGE_BYTES => {
                Some((bytes, image_mime_by_path(path)))
            }
            _ => None,
        },
        None => None,
    };
    let header = match &loaded {
        Some((bytes, mime)) => ImageHeader {
            ok: true,
            error: None,
            mime_type: (*mime).to_string(),
            size: bytes.len() as u64,
        },
        None => ImageHeader {
            ok: false,
            error: Some("no image".to_string()),
            ..ImageHeader::default()
        },
    };
    let mut line = serde_json::to_vec(&header)?;
    line.push(b'\n');
    stream.send.write_all(&line).await?;
    if let Some((bytes, _)) = &loaded {
        stream.send.write_all(bytes).await?;
    }
    Ok(())
}

fn image_mime_by_path(path: &str) -> &'static str {
    match std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        _ => "image/jpeg",
    }
}

/// Builds this instance's library slice for `artist`.
fn build_catalog(library: &Library, own: EndpointId, artist: &str) -> Result<CatalogResponse> {
    let Some(artist) = build_catalog_artist(library, own, artist)? else {
        return Ok(CatalogResponse {
            ok: false,
            error: Some("artist not found in the library".to_string()),
            artist: None,
        });
    };
    Ok(CatalogResponse {
        ok: true,
        error: None,
        artist: Some(artist),
    })
}

/// Builds the successful payload for this instance's library slice.
pub(crate) fn build_catalog_artist(
    library: &Library,
    own: EndpointId,
    artist: &str,
) -> Result<Option<CatalogArtist>> {
    let Some(artist_id) = library.artist_id_by_name(artist)? else {
        return Ok(None);
    };
    let detail = library.artist(artist_id)?;
    let item_id_of = |track_id: i64| -> String {
        super::audio::hex_encode(
            music_dht::ItemId::derive(&own, ItemKind::Track, &format!("track:{track_id}"))
                .as_bytes(),
        )
    };
    let mut releases = Vec::new();
    for card in &detail.releases {
        let release = library.release(card.id)?;
        releases.push(CatalogRelease {
            title: release.title,
            release_type: release.release_type,
            year: release.year,
            tracks: release
                .tracks
                .iter()
                .map(|track| catalog_track(track, item_id_of(track.id)))
                .collect(),
        });
    }
    let mut appears_on = Vec::new();
    for track in &detail.featured_tracks {
        let release = library.release(track.release_id)?;
        appears_on.push(CatalogAppearance {
            release_title: track.release_title.clone(),
            release_type: release.release_type,
            year: track.release_year,
            track: catalog_track(track, item_id_of(track.id)),
        });
    }
    Ok(Some(CatalogArtist {
        name: detail.name,
        releases,
        appears_on,
    }))
}

fn catalog_track(track: &crate::library::models::TrackItem, item_id: String) -> CatalogTrack {
    CatalogTrack {
        title: track.title.clone(),
        artists: track
            .artists
            .iter()
            .map(|artist| artist.name.clone())
            .collect(),
        featured_artists: track
            .featured_artists
            .iter()
            .map(|artist| artist.name.clone())
            .collect(),
        track_number: track.track_number,
        disc_number: track.disc_number,
        duration_seconds: (track.duration_seconds > 0.0).then_some(track.duration_seconds),
        content_id: track.content_id.clone(),
        item_id,
    }
}

// ---------------------------------------------------------------------------
// Requesting side
// ---------------------------------------------------------------------------

/// Fetches one peer's catalog slice for `artist`.
pub async fn fetch_catalog(
    service: &MusicDhtService,
    owner: EndpointId,
    artist: &str,
    transport_stats: &Arc<crate::federation::TransportStats>,
) -> Result<CatalogArtist> {
    let mut stream = service
        .open_stream(owner, CATALOG_ALPN)
        .await
        .map_err(|err| anyhow::anyhow!("cannot reach the peer: {err}"))?;
    crate::federation::record_stream_transport(
        transport_stats,
        "catalog",
        "outbound",
        "open",
        &stream,
    );
    let mut line = serde_json::to_vec(&CatalogRequest {
        artist: artist.to_string(),
        want: None,
        release: None,
    })?;
    line.push(b'\n');
    stream.send.write_all(&line).await?;
    stream.send.finish()?;

    let mut payload = Vec::new();
    // The whole response is one JSON document, bounded by the byte cap.
    tokio::io::AsyncReadExt::take(StreamReader(&mut stream), MAX_CATALOG_BYTES + 1)
        .read_to_end(&mut payload)
        .await?;
    anyhow::ensure!(
        payload.len() as u64 <= MAX_CATALOG_BYTES,
        "catalog response exceeds {MAX_CATALOG_BYTES} bytes"
    );
    let response: CatalogResponse =
        serde_json::from_slice(&payload).context("malformed catalog response")?;
    crate::federation::record_stream_transport(
        transport_stats,
        "catalog",
        "outbound",
        "done",
        &stream,
    );
    if !response.ok {
        anyhow::bail!(
            "peer refused the catalog: {}",
            response
                .error
                .unwrap_or_else(|| "unknown error".to_string())
        );
    }
    response.artist.context("empty catalog response")
}

/// Fetches an image (artist image or a release cover) from a peer over the
/// catalog protocol. `release: None` asks for the artist image. Returns the
/// raw bytes and a file extension, or None when the peer has no image.
pub async fn fetch_image(
    service: &MusicDhtService,
    owner: EndpointId,
    artist: &str,
    release: Option<&str>,
    transport_stats: &Arc<crate::federation::TransportStats>,
) -> Result<Option<(Vec<u8>, &'static str)>> {
    let mut stream = service
        .open_stream(owner, CATALOG_ALPN)
        .await
        .map_err(|err| anyhow::anyhow!("cannot reach the peer: {err}"))?;
    crate::federation::record_stream_transport(
        transport_stats,
        "catalog",
        "outbound",
        "open",
        &stream,
    );
    let mut line = serde_json::to_vec(&CatalogRequest {
        artist: artist.to_string(),
        want: Some(if release.is_some() {
            "release_cover".to_string()
        } else {
            "artist_image".to_string()
        }),
        release: release.map(str::to_string),
    })?;
    line.push(b'\n');
    stream.send.write_all(&line).await?;
    stream.send.finish()?;

    let header: ImageHeader =
        serde_json::from_slice(&super::audio::read_line(&mut stream.recv).await?)
            .context("malformed image header")?;
    if !header.ok || header.size == 0 {
        return Ok(None);
    }
    anyhow::ensure!(
        header.size <= MAX_IMAGE_BYTES,
        "image of {} bytes exceeds the {MAX_IMAGE_BYTES} byte limit",
        header.size
    );
    let mut bytes = vec![0u8; header.size as usize];
    stream
        .recv
        .read_exact(&mut bytes)
        .await
        .context("stream ended inside the image")?;
    crate::federation::record_stream_transport(
        transport_stats,
        "catalog",
        "outbound",
        "done",
        &stream,
    );
    let extension = match header.mime_type.as_str() {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        _ => "jpg",
    };
    Ok(Some((bytes, extension)))
}

/// AsyncRead adapter over the receive half of a byte stream.
struct StreamReader<'a>(&'a mut ByteStream);

impl tokio::io::AsyncRead for StreamReader<'_> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0.recv).poll_read(cx, buf)
    }
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

/// The assembled, deduplicated federated artist card.
#[derive(Debug, Clone, Default)]
pub struct FedArtistCard {
    #[allow(dead_code, reason = "the open card is keyed by name in AppState")]
    pub name: String,
    /// This node's endpoint id, used to keep own tracks local when an open
    /// card includes the local catalog alongside remote peers.
    pub own_owner: Option<String>,
    /// Peers whose catalogs contributed to the card.
    pub peers: usize,
    /// Every contributing peer (hex ids) — where images are fetched from.
    pub owners: Vec<String>,
    /// Local cache path of the artist image, streamed from a peer.
    pub image_path: Option<String>,
    pub releases: Vec<FedRelease>,
    pub appears_on: Vec<FedAppearsOn>,
}

#[derive(Debug, Clone, Default)]
pub struct FedRelease {
    pub title: String,
    pub release_type: String,
    pub year: Option<i32>,
    /// Peers holding this release (hex ids).
    pub owners: Vec<String>,
    /// Local cache path of the cover, streamed from a peer.
    pub cover_path: Option<String>,
    pub tracks: Vec<FedCardTrack>,
}

#[derive(Debug, Clone, Default)]
pub struct FedAppearsOn {
    pub release_title: String,
    pub release_type: String,
    pub year: Option<i32>,
    pub track: FedCardTrack,
}

#[derive(Debug, Clone, Default)]
pub struct FedCardTrack {
    pub title: String,
    pub artists: Vec<String>,
    pub featured_artists: Vec<String>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub duration_seconds: Option<f64>,
    pub content_id: Option<String>,
    /// Every peer that can serve this track: (owner hex, item id hex).
    /// Duplicates collapse into one row; all sources stay playable.
    pub sources: Vec<(String, String)>,
}

/// Merges per-peer catalogs into one card: releases are keyed by normalized
/// title, tracks within a release by normalized title + track number; a
/// track present on several peers keeps every source.
pub fn merge_catalogs(name: &str, catalogs: Vec<(String, CatalogArtist)>) -> FedArtistCard {
    let peers = catalogs.len();
    let mut releases: Vec<FedRelease> = Vec::new();
    let mut release_index: HashMap<String, usize> = HashMap::new();
    let mut appears_on: Vec<FedAppearsOn> = Vec::new();
    let mut appearance_index: HashMap<String, usize> = HashMap::new();

    let mut card_owners: Vec<String> = Vec::new();
    for (owner_hex, catalog) in catalogs {
        if !card_owners.contains(&owner_hex) {
            card_owners.push(owner_hex.clone());
        }
        for release in catalog.releases {
            let release_key = music_dht::normalize_name(&release.title);
            let slot = *release_index.entry(release_key).or_insert_with(|| {
                releases.push(FedRelease {
                    title: release.title.clone(),
                    release_type: release.release_type.clone(),
                    year: None,
                    owners: Vec::new(),
                    cover_path: None,
                    tracks: Vec::new(),
                });
                releases.len() - 1
            });
            let merged = &mut releases[slot];
            if !merged.owners.contains(&owner_hex) {
                merged.owners.push(owner_hex.clone());
            }
            if merged.year.is_none() {
                merged.year = release.year;
            }
            if merged.release_type.is_empty() {
                merged.release_type = release.release_type.clone();
            }
            for track in release.tracks {
                if track.item_id.is_empty() {
                    continue;
                }
                let existing = merged.tracks.iter_mut().find(|t| {
                    music_dht::normalize_name(&t.title) == music_dht::normalize_name(&track.title)
                        && (t.track_number == track.track_number
                            || t.track_number.is_none()
                            || track.track_number.is_none())
                });
                match existing {
                    Some(t) => {
                        merge_card_track(t, &owner_hex, track);
                    }
                    None => merged.tracks.push(card_track(owner_hex.clone(), track)),
                }
            }
        }
        for appearance in catalog.appears_on {
            if appearance.track.item_id.is_empty() {
                continue;
            }
            let key = format!(
                "{}:{}:{:?}",
                music_dht::normalize_name(&appearance.release_title),
                music_dht::normalize_name(&appearance.track.title),
                appearance.track.track_number
            );
            let slot = *appearance_index.entry(key).or_insert_with(|| {
                appears_on.push(FedAppearsOn {
                    release_title: appearance.release_title.clone(),
                    release_type: appearance.release_type.clone(),
                    year: appearance.year,
                    track: FedCardTrack::default(),
                });
                appears_on.len() - 1
            });
            let merged = &mut appears_on[slot];
            if merged.release_type.is_empty() {
                merged.release_type = appearance.release_type.clone();
            }
            if merged.year.is_none() {
                merged.year = appearance.year;
            }
            if merged.track.title.is_empty() {
                merged.track = card_track(owner_hex.clone(), appearance.track);
            } else {
                merge_card_track(&mut merged.track, &owner_hex, appearance.track);
            }
        }
    }

    for release in &mut releases {
        release.tracks.sort_by_key(|t| {
            (
                t.disc_number.unwrap_or(1),
                t.track_number.unwrap_or(i32::MAX),
            )
        });
    }
    appears_on.sort_by(|a, b| {
        b.year
            .unwrap_or(i32::MIN)
            .cmp(&a.year.unwrap_or(i32::MIN))
            .then_with(|| a.release_title.cmp(&b.release_title))
            .then_with(|| {
                a.track
                    .track_number
                    .unwrap_or(i32::MAX)
                    .cmp(&b.track.track_number.unwrap_or(i32::MAX))
            })
            .then_with(|| a.track.title.cmp(&b.track.title))
    });
    releases.sort_by(|a, b| {
        b.year
            .unwrap_or(i32::MIN)
            .cmp(&a.year.unwrap_or(i32::MIN))
            .then_with(|| a.title.cmp(&b.title))
    });

    FedArtistCard {
        name: name.to_string(),
        own_owner: None,
        peers,
        owners: card_owners,
        image_path: None,
        releases,
        appears_on,
    }
}

fn card_track(owner_hex: String, track: CatalogTrack) -> FedCardTrack {
    FedCardTrack {
        title: track.title,
        artists: track.artists,
        featured_artists: track.featured_artists,
        track_number: track.track_number,
        disc_number: track.disc_number,
        duration_seconds: track.duration_seconds,
        content_id: track.content_id,
        sources: vec![(owner_hex, track.item_id)],
    }
}

fn merge_card_track(target: &mut FedCardTrack, owner_hex: &str, track: CatalogTrack) {
    if target.artists.is_empty() {
        target.artists = track.artists;
    }
    if target.featured_artists.is_empty() {
        target.featured_artists = track.featured_artists;
    }
    if target.track_number.is_none() {
        target.track_number = track.track_number;
    }
    if target.disc_number.is_none() {
        target.disc_number = track.disc_number;
    }
    if target.duration_seconds.is_none() {
        target.duration_seconds = track.duration_seconds;
    }
    if target.content_id.is_none() {
        target.content_id = track.content_id;
    }
    let source = (owner_hex.to_string(), track.item_id);
    if !target.sources.contains(&source) {
        target.sources.push(source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, number: i32, item: &str) -> CatalogTrack {
        CatalogTrack {
            title: title.into(),
            artists: vec!["Metallica".into()],
            featured_artists: Vec::new(),
            track_number: Some(number),
            disc_number: None,
            duration_seconds: Some(100.0),
            content_id: None,
            item_id: item.into(),
        }
    }

    #[test]
    fn merges_and_dedupes_across_peers() {
        let catalog = |item_prefix: &str| CatalogArtist {
            name: "Metallica".into(),
            releases: vec![CatalogRelease {
                title: "Black Album".into(),
                release_type: "album".into(),
                year: Some(1991),
                tracks: vec![
                    track("Enter Sandman", 1, &format!("{item_prefix}1")),
                    track("Sad But True", 2, &format!("{item_prefix}2")),
                ],
            }],
            appears_on: Vec::new(),
        };
        let card = merge_catalogs(
            "Metallica",
            vec![
                ("peer-a".to_string(), catalog("a")),
                ("peer-b".to_string(), catalog("b")),
            ],
        );
        assert_eq!(card.peers, 2);
        assert_eq!(card.releases.len(), 1);
        let release = &card.releases[0];
        assert_eq!(release.year, Some(1991));
        assert_eq!(release.tracks.len(), 2);
        // Both peers stay as sources of the deduplicated track.
        assert_eq!(release.tracks[0].sources.len(), 2);
    }

    #[test]
    fn merges_featured_appearances_across_peers() {
        let mut featured = track("Guest Verse", 3, "a1");
        featured.artists = vec!["Host".into()];
        featured.featured_artists = vec!["Guest".into()];
        let mut same_featured = featured.clone();
        same_featured.item_id = "b1".into();

        let card = merge_catalogs(
            "Guest",
            vec![
                (
                    "peer-a".to_string(),
                    CatalogArtist {
                        name: "Guest".into(),
                        releases: Vec::new(),
                        appears_on: vec![CatalogAppearance {
                            release_title: "Host Album".into(),
                            release_type: "album".into(),
                            year: Some(2024),
                            track: featured,
                        }],
                    },
                ),
                (
                    "peer-b".to_string(),
                    CatalogArtist {
                        name: "Guest".into(),
                        releases: Vec::new(),
                        appears_on: vec![CatalogAppearance {
                            release_title: "Host Album".into(),
                            release_type: "album".into(),
                            year: Some(2024),
                            track: same_featured,
                        }],
                    },
                ),
            ],
        );

        assert!(card.releases.is_empty());
        assert_eq!(card.appears_on.len(), 1);
        assert_eq!(card.appears_on[0].track.sources.len(), 2);
        assert_eq!(card.appears_on[0].track.artists, vec!["Host"]);
        assert_eq!(card.appears_on[0].track.featured_artists, vec!["Guest"]);
    }

    #[test]
    fn merge_catalogs_sorts_releases_newest_first() {
        let release = |title: &str, year: Option<i32>, item: &str| CatalogRelease {
            title: title.into(),
            release_type: "album".into(),
            year,
            tracks: vec![track("Song", 1, item)],
        };
        let card = merge_catalogs(
            "Metallica",
            vec![(
                "peer-a".to_string(),
                CatalogArtist {
                    name: "Metallica".into(),
                    releases: vec![
                        release("Old Album", Some(1991), "a1"),
                        release("New Album", Some(2024), "a2"),
                        release("Undated Album", None, "a3"),
                    ],
                    appears_on: Vec::new(),
                },
            )],
        );

        let titles: Vec<&str> = card
            .releases
            .iter()
            .map(|release| release.title.as_str())
            .collect();
        assert_eq!(titles, vec!["New Album", "Old Album", "Undated Album"]);
    }
}
