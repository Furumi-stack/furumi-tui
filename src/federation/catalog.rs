//! The peer catalog protocol: one peer asks another for its library slice
//! of a single artist (releases with full tracklists), used to assemble a
//! federated artist card.
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
}

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
pub struct CatalogTrack {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub track_number: Option<i32>,
    #[serde(default)]
    pub disc_number: Option<i32>,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    /// Hex DHT item id — the key the audio is requested by (FedPlay).
    #[serde(default)]
    pub item_id: String,
}

// ---------------------------------------------------------------------------
// Serving side
// ---------------------------------------------------------------------------

/// Runs the catalog accept loop until the acceptor closes.
pub async fn serve_peers(mut acceptor: StreamAcceptor, library: Arc<Library>, own: EndpointId) {
    while let Some(stream) = acceptor.accept().await {
        let library = Arc::clone(&library);
        tokio::spawn(async move {
            let peer = stream.peer_id;
            if let Err(err) = serve_one(stream, library, own).await {
                tracing::warn!(peer = %peer, "catalog request failed: {err:#}");
            }
        });
    }
}

async fn serve_one(mut stream: ByteStream, library: Arc<Library>, own: EndpointId) -> Result<()> {
    let request: CatalogRequest =
        serde_json::from_slice(&super::audio::read_line(&mut stream.recv).await?)?;
    tracing::info!(peer = %stream.peer_id, artist = %request.artist, "peer requested a catalog");

    let response = tokio::task::spawn_blocking(move || build_catalog(&library, own, &request.artist))
        .await?
        .unwrap_or_else(|err| CatalogResponse {
            ok: false,
            error: Some(format!("catalog lookup failed: {err:#}")),
            artist: None,
        });
    let payload = serde_json::to_vec(&response)?;
    stream.send.write_all(&payload).await?;
    stream.send.finish()?;
    let _ = stream.send.stopped().await;
    Ok(())
}

/// Builds this instance's library slice for `artist`.
fn build_catalog(library: &Library, own: EndpointId, artist: &str) -> Result<CatalogResponse> {
    let Some(artist_id) = library.artist_id_by_name(artist)? else {
        return Ok(CatalogResponse {
            ok: false,
            error: Some("artist not found in the library".to_string()),
            artist: None,
        });
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
                .map(|track| CatalogTrack {
                    title: track.title.clone(),
                    track_number: track.track_number,
                    disc_number: track.disc_number,
                    duration_seconds: (track.duration_seconds > 0.0)
                        .then_some(track.duration_seconds),
                    item_id: item_id_of(track.id),
                })
                .collect(),
        });
    }
    Ok(CatalogResponse {
        ok: true,
        error: None,
        artist: Some(CatalogArtist {
            name: detail.name,
            releases,
        }),
    })
}

// ---------------------------------------------------------------------------
// Requesting side
// ---------------------------------------------------------------------------

/// Fetches one peer's catalog slice for `artist`.
pub async fn fetch_catalog(
    service: &MusicDhtService,
    owner: EndpointId,
    artist: &str,
) -> Result<CatalogArtist> {
    let mut stream = service
        .open_stream(owner, CATALOG_ALPN)
        .await
        .map_err(|err| anyhow::anyhow!("cannot reach the peer: {err}"))?;
    let mut line = serde_json::to_vec(&CatalogRequest {
        artist: artist.to_string(),
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
    /// Peers whose catalogs contributed to the card.
    pub peers: usize,
    pub releases: Vec<FedRelease>,
}

#[derive(Debug, Clone, Default)]
pub struct FedRelease {
    pub title: String,
    pub release_type: String,
    pub year: Option<i32>,
    pub tracks: Vec<FedCardTrack>,
}

#[derive(Debug, Clone, Default)]
pub struct FedCardTrack {
    pub title: String,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub duration_seconds: Option<f64>,
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

    for (owner_hex, catalog) in catalogs {
        for release in catalog.releases {
            let release_key = music_dht::normalize_name(&release.title);
            let slot = *release_index.entry(release_key).or_insert_with(|| {
                releases.push(FedRelease {
                    title: release.title.clone(),
                    release_type: release.release_type.clone(),
                    year: None,
                    tracks: Vec::new(),
                });
                releases.len() - 1
            });
            let merged = &mut releases[slot];
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
                        if t.track_number.is_none() {
                            t.track_number = track.track_number;
                        }
                        if t.duration_seconds.is_none() {
                            t.duration_seconds = track.duration_seconds;
                        }
                        t.sources.push((owner_hex.clone(), track.item_id));
                    }
                    None => merged.tracks.push(FedCardTrack {
                        title: track.title,
                        track_number: track.track_number,
                        disc_number: track.disc_number,
                        duration_seconds: track.duration_seconds,
                        sources: vec![(owner_hex.clone(), track.item_id)],
                    }),
                }
            }
        }
    }

    for release in &mut releases {
        release
            .tracks
            .sort_by_key(|t| (t.disc_number.unwrap_or(1), t.track_number.unwrap_or(i32::MAX)));
    }
    releases.sort_by(|a, b| {
        a.year
            .unwrap_or(i32::MAX)
            .cmp(&b.year.unwrap_or(i32::MAX))
            .then_with(|| a.title.cmp(&b.title))
    });

    FedArtistCard {
        name: name.to_string(),
        peers,
        releases,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, number: i32, item: &str) -> CatalogTrack {
        CatalogTrack {
            title: title.into(),
            track_number: Some(number),
            disc_number: None,
            duration_seconds: Some(100.0),
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
}
