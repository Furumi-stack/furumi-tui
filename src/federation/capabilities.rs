//! Informational publication and observation of protocol versions.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result};
pub use music_dht::capabilities::CAPABILITIES_ALPN;
use music_dht::capabilities::{
    CAPABILITIES_PROTOCOL_VERSION, CapabilityManifest, CapabilityMessage, read_message,
    write_message,
};
use music_dht::podcast_dht::PodcastDht;
use music_dht::{ByteStream, EndpointId, MusicDhtService, StreamAcceptor};

const PROBE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProtocolVersions {
    pub local: BTreeMap<String, u16>,
    pub observed: BTreeMap<String, u16>,
    pub observed_peers: usize,
    pub newer: Vec<NewerProtocol>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewerProtocol {
    pub id: String,
    pub local: u16,
    pub observed: u16,
}

impl ProtocolVersions {
    /// `podcast_dht`: whether this node runs the podcast-discovery overlay
    /// (it is advertised only then).
    pub fn snapshot(observed: &ObservedVersions, podcast_dht: bool) -> Self {
        let local = local_manifest(podcast_dht).protocols;
        let observed_versions = lock(&observed.versions).clone();
        let newer = observed_versions
            .iter()
            .filter_map(|(id, remote)| {
                let local_version = local.get(id)?;
                (*remote > *local_version).then(|| NewerProtocol {
                    id: id.clone(),
                    local: *local_version,
                    observed: *remote,
                })
            })
            .collect();
        Self {
            local,
            observed: observed_versions,
            observed_peers: lock(&observed.peers).len(),
            newer,
        }
    }
}

#[derive(Default)]
pub struct ObservedVersions {
    versions: Mutex<BTreeMap<String, u16>>,
    peers: Mutex<BTreeMap<String, String>>,
}

/// This node's manifest. The podcast-discovery overlay is opt-in in FRID:
/// it is advertised only when this node serves it (`podcast_dht`), so
/// peers never dial an overlay that failed to open here.
fn local_manifest(podcast_dht: bool) -> CapabilityManifest {
    let manifest = CapabilityManifest::frid("furumi", env!("CARGO_PKG_VERSION"))
        .with_protocol("audio", super::audio::AUDIO_PROTOCOL_VERSION)
        .with_protocol(
            music_dht::lyrics::LYRICS_ID,
            music_dht::lyrics::LYRICS_PROTOCOL_VERSION,
        );
    if podcast_dht {
        manifest.with_podcast_dht()
    } else {
        manifest
    }
}

pub async fn serve(mut acceptor: StreamAcceptor, podcast_dht: bool) {
    while let Some(stream) = acceptor.accept().await {
        tokio::spawn(async move {
            if let Err(error) = serve_one(stream, podcast_dht).await {
                tracing::debug!("capability stream failed: {error:#}");
            }
        });
    }
}

async fn serve_one(mut stream: ByteStream, podcast_dht: bool) -> Result<()> {
    let response = match read_message(&mut stream).await? {
        CapabilityMessage::Get {
            version: CAPABILITIES_PROTOCOL_VERSION,
        } => CapabilityMessage::Manifest {
            manifest: local_manifest(podcast_dht),
        },
        CapabilityMessage::Get { version } => CapabilityMessage::Error {
            message: format!("unsupported capability protocol {version}"),
        },
        _ => CapabilityMessage::Error {
            message: "expected capability request".to_string(),
        },
    };
    write_message(&mut stream, &response).await?;
    stream.send.finish()?;
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.send.stopped()).await;
    Ok(())
}

/// Polls the manifests of known peers. With the podcast-discovery overlay
/// running, every manifest is handed to `PodcastDht::note_peer_capabilities`,
/// which stops dialing peers that do not serve the overlay.
pub async fn probe_loop(
    service: Arc<MusicDhtService>,
    observed: Arc<ObservedVersions>,
    podcast_dht: Option<Arc<PodcastDht>>,
) {
    let mut interval = tokio::time::interval(PROBE_INTERVAL);
    loop {
        interval.tick().await;
        let peers = service
            .connected_peers()
            .into_iter()
            .chain(
                service
                    .known_peers()
                    .into_iter()
                    .map(|contact| contact.peer_id),
            )
            .collect::<std::collections::BTreeSet<_>>();
        for peer in peers {
            if let Err(error) = probe_peer(&service, peer, &observed, podcast_dht.as_deref()).await
            {
                tracing::trace!(%peer, "peer capability probe unavailable: {error:#}");
            }
        }
    }
}

async fn probe_peer(
    service: &MusicDhtService,
    peer: EndpointId,
    observed: &ObservedVersions,
    podcast_dht: Option<&PodcastDht>,
) -> Result<()> {
    let mut stream = service.open_stream(peer, CAPABILITIES_ALPN).await?;
    write_message(
        &mut stream,
        &CapabilityMessage::Get {
            version: CAPABILITIES_PROTOCOL_VERSION,
        },
    )
    .await?;
    stream.send.finish()?;
    let response = tokio::time::timeout(Duration::from_secs(5), read_message(&mut stream))
        .await
        .context("capability request timed out")??;
    let CapabilityMessage::Manifest { manifest } = response else {
        anyhow::bail!("peer did not return a capability manifest");
    };
    manifest.validate()?;
    if let Some(podcast_dht) = podcast_dht
        && let Err(error) = podcast_dht.note_peer_capabilities(peer, &manifest).await
    {
        tracing::debug!(%peer, "podcast DHT capability note failed: {error:#}");
    }
    {
        let mut versions = lock(&observed.versions);
        for (id, version) in manifest.protocols {
            versions
                .entry(id)
                .and_modify(|current| *current = (*current).max(version))
                .or_insert(version);
        }
    }
    lock(&observed.peers).insert(peer.to_string(), manifest.application_version);
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_manifest_lists_every_player_protocol() {
        let manifest = local_manifest(true);
        for id in [
            "federation_net",
            "ticket",
            "rendezvous",
            "music_dht",
            "catalog",
            "audio",
            "similarity",
            "similarity_dht",
            "device_sync",
            "jam",
            "podcast_dht",
        ] {
            assert!(manifest.protocols.contains_key(id), "missing {id}");
        }
        manifest.validate().unwrap();
        assert!(manifest.supports_podcast_dht());
    }

    #[test]
    fn podcast_discovery_is_advertised_only_when_it_runs() {
        let manifest = local_manifest(false);
        assert!(!manifest.supports_podcast_dht());
        assert!(!manifest.protocols.contains_key("podcast_dht"));
        manifest.validate().unwrap();
        let observed = ObservedVersions::default();
        assert!(
            !ProtocolVersions::snapshot(&observed, false)
                .local
                .contains_key("podcast_dht")
        );
        assert!(
            ProtocolVersions::snapshot(&observed, true)
                .local
                .contains_key("podcast_dht")
        );
    }

    #[test]
    fn snapshot_reports_only_strictly_newer_versions() {
        let observed = ObservedVersions::default();
        lock(&observed.versions).insert("music_dht".to_string(), 99);
        lock(&observed.versions).insert("jam".to_string(), crate::jam::PROTOCOL_VERSION);
        let snapshot = ProtocolVersions::snapshot(&observed, true);
        assert_eq!(snapshot.newer.len(), 1);
        assert_eq!(snapshot.newer[0].id, "music_dht");
    }
}
