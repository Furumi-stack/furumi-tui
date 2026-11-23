//! Optional lyrics streams use audio content identities, independent of the catalog schema.
use super::Federation;
use crate::library::Library;
use anyhow::{Context, Result};
use futures_util::{StreamExt, stream};
use music_dht::lyrics::{self, LyricsRequest, LyricsResponse, LyricsVersion};
use music_dht::{ByteStream, EndpointId, MusicDhtService, StreamAcceptor};
use std::{collections::HashSet, sync::Arc, time::Duration};

const PEER_TIMEOUT: Duration = Duration::from_secs(6);

impl Federation {
    pub(crate) async fn find_lyrics(
        &self,
        content_id: &str,
        preferred: Option<&str>,
        mut on_versions: impl FnMut(Vec<LyricsVersion>),
    ) -> Result<()> {
        let request = LyricsRequest::new(content_id)?;
        let service = self.service().await?;
        let own = service.endpoint_id();

        // Start with the audio source while content discovery looks for other copies.
        let preferred = preferred
            .and_then(|id| id.parse::<EndpointId>().ok())
            .filter(|id| *id != own);
        let direct = async {
            if let Some(peer) = preferred {
                fetch(&service, peer, &request).await.ok()
            } else {
                None
            }
        };
        let discovery = tokio::time::timeout(PEER_TIMEOUT, service.search_content_id(content_id));
        tokio::pin!(direct, discovery);

        let found = tokio::select! {
            result = &mut direct => {
                if let Some(versions) = result {
                    on_versions(versions);
                }
                discovery.await
            }
            result = &mut discovery => {
                if let Some(versions) = direct.await {
                    on_versions(versions);
                }
                result
            }
        };

        let mut seen = HashSet::from([own]);
        seen.extend(preferred);
        let owners = found
            .ok()
            .and_then(Result::ok)
            .into_iter()
            .flat_map(|outcome| {
                outcome
                    .local_results
                    .into_iter()
                    .chain(outcome.network_results)
            })
            .filter(|item| {
                item.kind == music_dht::ItemKind::Track
                    && item.content_id.as_deref() == Some(content_id)
            })
            .map(|item| item.owner);

        // Connected peers can also have newly stored copies that aren't indexed yet.
        let candidates = owners
            .chain(service.connected_peers())
            .filter(|peer| seen.insert(*peer))
            .take(16);
        let mut responses = stream::iter(candidates)
            .map(|peer| fetch(&service, peer, &request))
            .buffer_unordered(6);
        while let Some(result) = responses.next().await {
            match result {
                Ok(versions) => on_versions(versions),
                Err(error) => {
                    tracing::trace!(error = %format!("{error:#}"), "peer lyrics unavailable")
                }
            }
        }
        Ok(())
    }
}

async fn fetch(
    service: &MusicDhtService,
    peer: EndpointId,
    request: &LyricsRequest,
) -> Result<Vec<LyricsVersion>> {
    tokio::time::timeout(PEER_TIMEOUT, async {
        let mut retried = false;
        loop {
            let mut stream = service
                .open_stream(peer, lyrics::LYRICS_ALPN)
                .await
                .context("opening peer lyrics stream")?;
            lyrics::write_request(&mut stream, request).await?;
            stream.send.finish()?;
            match lyrics::read_response_from(&mut stream.recv).await {
                Ok(response) => {
                    anyhow::ensure!(
                        response.content_id == request.content_id,
                        "lyrics response content identity mismatch"
                    );
                    return Ok(response.versions);
                }
                Err(music_dht::MusicDhtError::Network(_)) if !retried => retried = true,
                Err(error) => return Err(error).context("reading peer lyrics response"),
            }
        }
    })
    .await?
}

pub(super) async fn serve(mut acceptor: StreamAcceptor, library: Arc<Library>) {
    let limit = Arc::new(tokio::sync::Semaphore::new(16));
    while let Some(stream) = acceptor.accept().await {
        let Ok(permit) = Arc::clone(&limit).try_acquire_owned() else {
            continue;
        };
        let library = Arc::clone(&library);
        tokio::spawn(async move {
            let _permit = permit;
            match tokio::time::timeout(Duration::from_secs(12), serve_one(stream, library)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::debug!(%error, "could not serve lyrics"),
                Err(_) => tracing::debug!("lyrics stream timed out"),
            }
        });
    }
}

async fn serve_one(mut stream: ByteStream, library: Arc<Library>) -> Result<()> {
    let request = lyrics::read_request_from(&mut stream.recv).await?;
    let content_id = request.content_id.clone();
    let versions = tokio::task::spawn_blocking(move || -> Result<_> {
        let local = library.track_by_content_id(&content_id)?;
        if local.is_some_and(|track| std::path::Path::new(&track.file_path).is_file()) {
            Ok(library.lyrics(&content_id)?.versions)
        } else {
            Ok(Vec::new())
        }
    })
    .await??;
    // JSON can be larger than the stored UTF-8 text because of escapes.
    let mut response = LyricsResponse {
        version: lyrics::LYRICS_PROTOCOL_VERSION,
        content_id: request.content_id,
        versions,
    };
    // Leave space for the bounded envelope and the comma before each entry.
    let mut remaining = lyrics::MAX_LYRICS_RESPONSE_BYTES - 256;
    response.versions.retain(|version| {
        let Ok(encoded) = serde_json::to_vec(version) else {
            return false;
        };
        let Some(rest) = remaining.checked_sub(encoded.len() + 1) else {
            return false;
        };
        remaining = rest;
        true
    });
    lyrics::write_response(&mut stream, &response).await?;
    stream.send.finish()?;
    // ByteStream also owns its connection. Keep it alive through FIN delivery.
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.send.stopped()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::import::{TrackImport, upsert_track};
    use music_dht::{MusicDhtConfig, NetworkId};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn localhost_lyrics_stream_serves_only_saved_content_and_all_versions() {
        let temp = tempfile::tempdir().unwrap();
        let network = NetworkId::from_name(&format!("lyrics-test-{}", temp.path().display()));
        let start = |name| {
            let dir = temp.path().join(name);
            async move {
                std::fs::create_dir_all(&dir).unwrap();
                let (service, mut events) = MusicDhtService::start(
                    MusicDhtConfig::builder()
                        .data_dir(dir)
                        .network_id(network)
                        .schema_independent_stream_protocol(lyrics::LYRICS_ALPN)
                        .build()
                        .unwrap(),
                )
                .await
                .unwrap();
                tokio::spawn(async move { while events.recv().await.is_some() {} });
                service
            }
        };
        let (server, client) = tokio::join!(start("server"), start("client"));
        let library = Arc::new(Library::open(&temp.path().join("library.db")).unwrap());
        let path = temp.path().join("track.flac");
        std::fs::write(&path, b"immutable music bytes").unwrap();
        let track_id = upsert_track(
            &library,
            &TrackImport {
                file_path: path.to_string_lossy().into_owned(),
                title: "Track".into(),
                artists: vec!["Artist".into()],
                album_artists: vec!["Artist".into()],
                featured_artists: vec![],
                release_title: "Release".into(),
                release_type: None,
                year: None,
                track_number: None,
                disc_number: None,
                duration_seconds: 5.0,
                audio_format: None,
                audio_bitrate: None,
                audio_sample_rate: None,
                audio_bit_depth: None,
                file_size_bytes: None,
                cover: None,
            },
        )
        .unwrap()
        .0;
        let id = library.track_content_id_by_id(track_id).unwrap().unwrap();
        library
            .edit_lyrics(
                track_id,
                &id,
                None,
                Some(crate::lyrics::manual_version("Plain", "Words").unwrap()),
            )
            .unwrap();
        library
            .edit_lyrics(
                track_id,
                &id,
                None,
                Some(crate::lyrics::manual_version("Timed", "[00:01]Words").unwrap()),
            )
            .unwrap();
        let task = tokio::spawn(serve(
            server.stream_acceptor(lyrics::LYRICS_ALPN).unwrap(),
            library,
        ));
        client
            .connect(server.ticket().await.unwrap())
            .await
            .unwrap();
        let versions = fetch(
            &client,
            server.endpoint_id(),
            &LyricsRequest::new(&id).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].synced_lrc.as_deref(), Some("[00:01]Words"));
        assert_eq!(versions[1].plain_text.as_deref(), Some("Words"));
        std::fs::remove_file(path).unwrap();
        assert!(
            fetch(
                &client,
                server.endpoint_id(),
                &LyricsRequest::new(id).unwrap()
            )
            .await
            .unwrap()
            .is_empty()
        );
        task.abort();
        server.shutdown().await.unwrap();
        client.shutdown().await.unwrap();
    }
}
