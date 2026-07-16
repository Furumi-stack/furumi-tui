//! The peer-to-peer audio protocol, wire compatible with furumi-fd.
//!
//! One byte stream per request: the requester sends one JSON line
//! ([`AudioRequest`]) and receives one JSON line ([`AudioResponseHeader`])
//! followed by the raw file bytes from the requested offset.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use music_dht::{ByteStream, EndpointId, ItemId, ItemKind, MusicDhtService, StreamAcceptor};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::library::Library;

/// ALPN of the audio streaming protocol (shared with furumi-fd).
pub const AUDIO_ALPN: &[u8] = b"furumi-fd/audio/1";

/// Maximum size of a JSON protocol line (request or response header).
const MAX_PROTOCOL_LINE: usize = 4096;

#[derive(Debug, Serialize, Deserialize)]
struct AudioRequest {
    /// Hex-encoded [`ItemId`] of the track.
    item_id: String,
    /// Byte offset to start streaming from.
    offset: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct AudioResponseHeader {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    mime_type: String,
    #[serde(default)]
    total_size: u64,
    #[serde(default)]
    offset: u64,
    /// Full track metadata from the owner's database — richer and more
    /// authoritative than whatever tags the file itself carries. Absent
    /// when the peer predates the field (the header is extensible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<TrackMetadata>,
}

/// Track metadata exchanged alongside the audio bytes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrackMetadata {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default)]
    pub featured_artists: Vec<String>,
    #[serde(default)]
    pub album_artists: Vec<String>,
    #[serde(default)]
    pub release_title: String,
    #[serde(default)]
    pub release_type: Option<String>,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub track_number: Option<i32>,
    #[serde(default)]
    pub disc_number: Option<i32>,
}

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn hex_decode_item_id(value: &str) -> Option<ItemId> {
    if value.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(ItemId::from_bytes(bytes))
}

fn guess_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "wav" => "audio/wav",
        "m4a" | "mp4" | "alac" => "audio/mp4",
        "aac" => "audio/aac",
        "aiff" | "aif" => "audio/aiff",
        _ => "application/octet-stream",
    }
}

/// Extension for a downloaded file, from the mime type the peer reported.
fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "audio/mpeg" => "mp3",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/ogg" => "ogg",
        "audio/opus" => "opus",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/aac" => "aac",
        "audio/aiff" => "aiff",
        _ => "bin",
    }
}

/// Reads one `\n`-terminated line, bounded by [`MAX_PROTOCOL_LINE`].
async fn read_line<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            anyhow::bail!("stream ended before the protocol line was complete");
        }
        if byte[0] == b'\n' {
            return Ok(line);
        }
        line.push(byte[0]);
        if line.len() > MAX_PROTOCOL_LINE {
            anyhow::bail!("protocol line exceeds {MAX_PROTOCOL_LINE} bytes");
        }
    }
}

async fn write_line<W: AsyncWriteExt + Unpin>(writer: &mut W, value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Requesting side: download a track from its owner
// ---------------------------------------------------------------------------

/// Downloads a whole track from `owner` into `dir/<stem>.<ext>`; returns
/// the file path, the mime type and the track metadata the peer reported.
/// An already complete cached file is reused (the metadata still comes
/// fresh from the header).
pub async fn download_track(
    service: &MusicDhtService,
    owner: EndpointId,
    item_id_hex: &str,
    dir: &Path,
    stem: &str,
) -> Result<(PathBuf, String, Option<TrackMetadata>)> {
    let mut stream = service
        .open_stream(owner, AUDIO_ALPN)
        .await
        .map_err(|err| anyhow::anyhow!("cannot reach the owner peer: {err}"))?;
    write_line(
        &mut stream.send,
        &AudioRequest {
            item_id: item_id_hex.to_string(),
            offset: 0,
        },
    )
    .await?;
    stream.send.finish()?;
    let header: AudioResponseHeader = serde_json::from_slice(&read_line(&mut stream.recv).await?)
        .context("malformed response header")?;
    if !header.ok {
        anyhow::bail!(
            "peer refused the stream: {}",
            header.error.unwrap_or_else(|| "unknown error".to_string())
        );
    }

    let extension = extension_for_mime(&header.mime_type);
    let path = dir.join(format!("{stem}.{extension}"));
    if let Ok(metadata) = tokio::fs::metadata(&path).await
        && metadata.len() == header.total_size
        && header.total_size > 0
    {
        // Already fully downloaded earlier; no need to fetch again.
        return Ok((path, header.mime_type, header.metadata));
    }

    let temp_path = dir.join(format!(".{stem}.{extension}.part"));
    let mut file = tokio::fs::File::create(&temp_path).await?;
    let mut received: u64 = 0;
    let mut chunk = vec![0u8; 64 * 1024];
    // quinn's inherent read returns None when the peer finished the stream.
    while let Some(n) = stream.recv.read(&mut chunk).await? {
        file.write_all(&chunk[..n]).await?;
        received += n as u64;
    }
    file.flush().await?;
    drop(file);
    if header.total_size > 0 && received != header.total_size {
        let _ = tokio::fs::remove_file(&temp_path).await;
        anyhow::bail!(
            "download incomplete: got {received} of {} bytes",
            header.total_size
        );
    }
    tokio::fs::rename(&temp_path, &path).await?;
    Ok((path, header.mime_type, header.metadata))
}

// ---------------------------------------------------------------------------
// Serving side: answer audio requests from other peers
// ---------------------------------------------------------------------------

/// Finds the local track whose derived DHT item id matches `item_id`.
pub fn resolve_local_track_id(
    library: &Library,
    own: EndpointId,
    item_id: ItemId,
) -> Result<Option<i64>> {
    let export = library.federation_export()?;
    for track in export.tracks {
        let derived = ItemId::derive(&own, ItemKind::Track, &format!("track:{}", track.id));
        if derived == item_id {
            return Ok(Some(track.id));
        }
    }
    Ok(None)
}

/// Resolves the item to (file path, full metadata from the database).
fn resolve_for_serving(
    library: &Library,
    own: EndpointId,
    item_id: ItemId,
) -> Result<Option<(String, TrackMetadata)>> {
    let Some(track_id) = resolve_local_track_id(library, own, item_id)? else {
        return Ok(None);
    };
    let Some(track) = library.tracks_by_ids(&[track_id])?.into_iter().next() else {
        return Ok(None);
    };
    // Release type and album artists live on the release row.
    let (release_type, album_artists) = match library.release(track.release_id) {
        Ok(detail) => (
            Some(detail.release_type),
            detail.artists.iter().map(|a| a.name.clone()).collect(),
        ),
        Err(_) => (None, Vec::new()),
    };
    let metadata = TrackMetadata {
        title: track.title.clone(),
        artists: track.artists.iter().map(|a| a.name.clone()).collect(),
        featured_artists: track
            .featured_artists
            .iter()
            .map(|a| a.name.clone())
            .collect(),
        album_artists,
        release_title: track.release_title.clone(),
        release_type,
        year: track.release_year,
        track_number: track.track_number,
        disc_number: track.disc_number,
    };
    Ok(Some((track.file_path, metadata)))
}

/// Runs the accept loop of the audio protocol until the acceptor closes.
/// Every track of the local library is downloadable by every peer of the
/// network — the libraries of all participants are equal.
pub async fn serve_peers(mut acceptor: StreamAcceptor, library: Arc<Library>, own: EndpointId) {
    while let Some(stream) = acceptor.accept().await {
        let library = Arc::clone(&library);
        tokio::spawn(async move {
            let peer = stream.peer_id;
            if let Err(err) = serve_one(stream, library, own).await {
                tracing::warn!(peer = %peer, "audio stream failed: {err:#}");
            }
        });
    }
}

async fn serve_one(mut stream: ByteStream, library: Arc<Library>, own: EndpointId) -> Result<()> {
    let request: AudioRequest = serde_json::from_slice(&read_line(&mut stream.recv).await?)?;
    tracing::info!(
        peer = %stream.peer_id,
        item = %request.item_id,
        offset = request.offset,
        "peer requested audio"
    );

    let resolved = match hex_decode_item_id(&request.item_id) {
        Some(item_id) => {
            let library = Arc::clone(&library);
            match tokio::task::spawn_blocking(move || resolve_for_serving(&library, own, item_id))
                .await
            {
                Ok(Ok(Some(found))) => Ok(found),
                Ok(Ok(None)) => Err("track not found in the library".to_string()),
                Ok(Err(err)) => Err(format!("library lookup failed: {err:#}")),
                Err(err) => Err(format!("lookup task failed: {err}")),
            }
        }
        None => Err("malformed item_id".to_string()),
    };
    let (file_path, metadata) = match resolved {
        Ok(found) => found,
        Err(message) => return refuse(stream, message).await,
    };

    let path = PathBuf::from(&file_path);
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(err) => return refuse(stream, format!("audio file is not readable: {err}")).await,
    };
    let total_size = file.metadata().await?.len();
    let offset = request.offset.min(total_size);
    if offset > 0 {
        file.seek(std::io::SeekFrom::Start(offset)).await?;
    }
    write_line(
        &mut stream.send,
        &AudioResponseHeader {
            ok: true,
            error: None,
            mime_type: guess_mime(&path).to_string(),
            total_size,
            offset,
            metadata: Some(metadata),
        },
    )
    .await?;
    tokio::io::copy(&mut file, &mut stream.send).await?;
    stream.send.finish()?;
    // Wait until the peer read everything (or gave up) before dropping the
    // stream, otherwise the tail of the file is lost.
    let _ = stream.send.stopped().await;
    Ok(())
}

/// Sends a refusal header and waits until the peer read it.
async fn refuse(mut stream: ByteStream, message: String) -> Result<()> {
    write_line(
        &mut stream.send,
        &AudioResponseHeader {
            ok: false,
            error: Some(message.clone()),
            mime_type: String::new(),
            total_size: 0,
            offset: 0,
            metadata: None,
        },
    )
    .await?;
    stream.send.finish()?;
    let _ = stream.send.stopped().await;
    anyhow::bail!("refused audio request: {message}");
}
