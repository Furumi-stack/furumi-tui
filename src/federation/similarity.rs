//! Furumi policy and local-index adapter for the shared similarity protocol.
//!
//! `music_dht::similarity` owns the versioned wire contract and framing. This
//! module owns application policy: consent, peer fan-out, local index access,
//! result conversion, deduplication, and ranking limits.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures_util::stream::{self, StreamExt as _};
use music_dht::similarity::{self as wire, SimilarityHit, SimilarityRequest, SimilarityResponse};
use music_dht::similarity_dht::SimilarityDht;
use music_dht::{
    ByteStream, EndpointId, ItemId, ItemKind, MusicDhtService, PeerTicket, StreamAcceptor,
};

use crate::federation::{FedSearchResults, FedTrack, TransportStats};
use crate::similarity::{Manager, QueryVector};

pub use music_dht::similarity::SIMILARITY_ALPN;

const INITIAL_QUERY_PEERS: usize = 16;
const MAX_QUERY_PEERS: usize = 48;
const QUERY_CONCURRENCY: usize = 8;
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const ROUTING_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PER_ARTIST: usize = 3;
const MAX_NEAR_DUPLICATE_SIGNATURE_DISTANCE: u32 = 8;

pub async fn serve_peers(
    mut acceptor: StreamAcceptor,
    similarity: Arc<Manager>,
    own: EndpointId,
    transport: Arc<TransportStats>,
) {
    while let Some(stream) = acceptor.accept().await {
        let similarity = Arc::clone(&similarity);
        let transport = Arc::clone(&transport);
        tokio::spawn(async move {
            let peer = stream.peer_id;
            if let Err(err) = serve_one(stream, similarity, own, transport).await {
                tracing::warn!(peer = %peer, "similarity request failed: {err:#}");
            }
        });
    }
}

async fn serve_one(
    mut stream: ByteStream,
    similarity: Arc<Manager>,
    own: EndpointId,
    transport: Arc<TransportStats>,
) -> Result<()> {
    super::record_stream_transport(&transport, "similarity", "inbound", "open", &stream);
    let request = wire::read_request(&mut stream).await?;
    let response = if !similarity.network_allowed() {
        SimilarityResponse::refused("similarity federation is disabled or has no privacy consent")?
    } else {
        let profile = request.profile_id;
        let vector = request.vector;
        let limit = request.limit;
        let matches = tokio::task::spawn_blocking(move || {
            similarity.search_vector(&profile, &vector, None, None, limit)
        })
        .await
        .context("local similarity task failed")
        .and_then(|result| result);
        match matches {
            Ok(matches) => {
                let hits = matches
                    .into_iter()
                    .filter_map(|found| {
                        let track = found.track;
                        let hit = SimilarityHit {
                            score: found.score,
                            item_id: super::audio::hex_encode(
                                ItemId::derive(
                                    &own,
                                    ItemKind::Track,
                                    &format!("track:{}", track.id),
                                )
                                .as_bytes(),
                            ),
                            title: track.title,
                            artist_names: track
                                .artists
                                .into_iter()
                                .map(|artist| artist.name)
                                .collect(),
                            featured_artist_names: track
                                .featured_artists
                                .into_iter()
                                .map(|artist| artist.name)
                                .collect(),
                            year: track.release_year,
                            duration_seconds: Some(track.duration_seconds.round() as i64),
                            content_id: track.content_id,
                            release_title: Some(track.release_title),
                            track_number: track.track_number,
                            disc_number: track.disc_number,
                            embedding_signature: Some(found.embedding_signature),
                        };
                        match hit.validate() {
                            Ok(()) => Some(hit),
                            Err(err) => {
                                tracing::debug!(%err, "invalid local similarity metadata skipped");
                                None
                            }
                        }
                    })
                    .collect();
                SimilarityResponse::success(hits)?
            }
            Err(err) => {
                SimilarityResponse::refused(format!("similarity query is unavailable: {err:#}"))?
            }
        }
    };
    wire::write_response(&mut stream, &response).await?;
    stream.send.finish()?;
    let _ = stream.send.stopped().await;
    super::record_stream_transport(&transport, "similarity", "inbound", "done", &stream);
    Ok(())
}

pub async fn search(
    service: Arc<MusicDhtService>,
    routing: Arc<SimilarityDht>,
    query: QueryVector,
    limit: usize,
    transport: Arc<TransportStats>,
) -> Result<FedSearchResults> {
    let own = service.endpoint_id();
    let routed = match tokio::time::timeout(
        ROUTING_TIMEOUT,
        routing.find_peers(&query.profile_id, &query.vector, MAX_QUERY_PEERS),
    )
    .await
    {
        Ok(Ok(peers)) => peers,
        Err(_) => {
            tracing::debug!("similarity DHT lookup timed out; using known peers");
            Vec::new()
        }
        Ok(Err(error)) => {
            tracing::debug!(%error, "similarity DHT lookup unavailable; using known peers");
            Vec::new()
        }
    };
    let mut seen = HashSet::new();
    let mut peers: Vec<QueryPeer> = routed
        .into_iter()
        .filter_map(|ticket| {
            let owner = ticket.endpoint_id();
            (owner != own && seen.insert(owner)).then_some(QueryPeer {
                owner,
                ticket: Some(ticket),
            })
        })
        .collect();
    for peer in service
        .connected_peers()
        .into_iter()
        .chain(service.known_peers().into_iter().map(|peer| peer.peer_id))
    {
        if peer != own && seen.insert(peer) {
            peers.push(QueryPeer {
                owner: peer,
                ticket: None,
            });
        }
        if peers.len() >= MAX_QUERY_PEERS {
            break;
        }
    }
    let query_signature = wire::embedding_signature(&query.vector)?;
    let request = Arc::new(SimilarityRequest::new(
        query.profile_id,
        query.vector,
        limit.clamp(1, wire::MAX_SIMILARITY_RESULTS),
    )?);

    let mut hits = Vec::new();
    let initial = peers.len().min(INITIAL_QUERY_PEERS);
    let responses = query_peers(
        Arc::clone(&service),
        &peers[..initial],
        Arc::clone(&request),
        Arc::clone(&transport),
    )
    .await;
    let mut successful = 0usize;
    for response in responses {
        match response {
            Ok(peer_hits) => {
                successful += 1;
                hits.extend(peer_hits);
            }
            Err(err) => tracing::debug!(%err, "similarity peer query skipped"),
        }
    }
    if initial < peers.len() && (hits.len() < limit || successful < initial.min(4)) {
        for response in query_peers(
            Arc::clone(&service),
            &peers[initial..],
            Arc::clone(&request),
            Arc::clone(&transport),
        )
        .await
        {
            match response {
                Ok(peer_hits) => hits.extend(peer_hits),
                Err(err) => tracing::debug!(%err, "fallback similarity peer query skipped"),
            }
        }
    }
    hits.sort_by(|left, right| right.1.total_cmp(&left.1));
    let mut dedup = HashSet::new();
    let mut embedding_signatures = vec![query_signature];
    let mut artist_counts: HashMap<String, usize> = HashMap::new();
    let mut tracks = Vec::new();
    for (track, _, embedding_signature) in hits {
        if query
            .source_content_id
            .as_deref()
            .is_some_and(|source| track.content_id.as_deref() == Some(source))
        {
            continue;
        }
        let key = track
            .content_id
            .clone()
            .unwrap_or_else(|| format!("{}:{}", track.owner, track.item_id));
        if !dedup.insert(key) {
            continue;
        }
        if embedding_signature.is_some_and(|candidate| {
            embedding_signatures.iter().any(|existing| {
                wire::signature_distance(&candidate, existing)
                    <= MAX_NEAR_DUPLICATE_SIGNATURE_DISTANCE
            })
        }) {
            continue;
        }
        let artist = track
            .artist_names
            .first()
            .map(|name| music_dht::normalize_name(name))
            .unwrap_or_default();
        let count = artist_counts.entry(artist.clone()).or_default();
        if !artist.is_empty() && *count >= MAX_PER_ARTIST {
            continue;
        }
        *count += 1;
        if let Some(signature) = embedding_signature {
            embedding_signatures.push(signature);
        }
        tracks.push(track);
        if tracks.len() >= limit.min(wire::MAX_SIMILARITY_RESULTS) {
            break;
        }
    }
    Ok(FedSearchResults {
        artists: Vec::new(),
        tracks,
    })
}

type PeerHits = Vec<(
    FedTrack,
    f32,
    Option<[u8; wire::SIMILARITY_SIGNATURE_BYTES]>,
)>;

#[derive(Clone)]
struct QueryPeer {
    owner: EndpointId,
    ticket: Option<PeerTicket>,
}

async fn query_peers(
    service: Arc<MusicDhtService>,
    peers: &[QueryPeer],
    request: Arc<SimilarityRequest>,
    transport: Arc<TransportStats>,
) -> Vec<Result<PeerHits>> {
    stream::iter(peers.iter().cloned().map(|peer| {
        let service = Arc::clone(&service);
        let request = Arc::clone(&request);
        let transport = Arc::clone(&transport);
        async move {
            tokio::time::timeout(
                QUERY_TIMEOUT,
                query_peer(service, peer, &request, transport),
            )
            .await
            .map_err(|_| anyhow::anyhow!("similarity peer timed out"))?
        }
    }))
    .buffer_unordered(QUERY_CONCURRENCY)
    .collect()
    .await
}

async fn query_peer(
    service: Arc<MusicDhtService>,
    peer: QueryPeer,
    request: &SimilarityRequest,
    transport: Arc<TransportStats>,
) -> Result<PeerHits> {
    let owner = peer.owner;
    let mut stream = match peer.ticket {
        Some(ticket) => service.open_stream_to(&ticket, SIMILARITY_ALPN).await,
        None => service.open_stream(owner, SIMILARITY_ALPN).await,
    }
    .map_err(|err| anyhow::anyhow!("cannot reach similarity peer: {err}"))?;
    super::record_stream_transport(&transport, "similarity", "outbound", "open", &stream);
    let response = wire::exchange(&mut stream, request).await?;
    super::record_stream_transport(&transport, "similarity", "outbound", "done", &stream);
    anyhow::ensure!(
        response.ok,
        "peer refused similarity query: {}",
        response.error.unwrap_or_default()
    );
    Ok(response
        .hits
        .into_iter()
        .map(|hit| {
            let score = hit.score;
            let embedding_signature = hit.embedding_signature;
            (
                FedTrack {
                    item_id: hit.item_id,
                    owner: owner.to_string(),
                    own: false,
                    title: hit.title,
                    artist_names: hit.artist_names,
                    featured_artist_names: hit.featured_artist_names,
                    year: hit.year,
                    duration_seconds: hit.duration_seconds,
                    content_id: hit.content_id,
                    release_title: hit.release_title,
                    track_number: hit.track_number,
                    disc_number: hit.disc_number,
                },
                score,
                embedding_signature,
            )
        })
        .collect())
}
