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
    /// Byte offset to start streaming from (audio only; the cover, when
    /// requested, is always sent whole).
    offset: u64,
    /// Ask the owner to send the cover art between the header and the
    /// audio bytes. Default false keeps the wire layout compatible with
    /// older peers in both directions.
    #[serde(default)]
    want_cover: bool,
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
    /// Size of the cover-art segment sent between this header and the
    /// audio bytes; 0 = no cover (not requested, not available).
    #[serde(default)]
    cover_size: u64,
    #[serde(default)]
    cover_mime: String,
    /// Size of the main artist's image segment, sent after the cover and
    /// before the audio; 0 = none. Governed by the same `want_cover` flag.
    #[serde(default)]
    artist_image_size: u64,
    #[serde(default)]
    artist_image_mime: String,
}

/// Covers above this size are skipped rather than transferred.
const MAX_COVER_BYTES: u64 = 16 * 1024 * 1024;

fn image_mime(path: &Path) -> &'static str {
    match path
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

/// File extension for a received cover, from its mime type.
pub fn image_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        _ => "jpg",
    }
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
pub(super) async fn read_line<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
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

/// Outcome of [`download_track`].
pub struct Downloaded {
    pub path: PathBuf,
    pub mime_type: String,
    pub metadata: Option<TrackMetadata>,
    /// Cover art (bytes, file extension) sent by the owner, if any.
    pub cover: Option<(Vec<u8>, &'static str)>,
    /// The main artist's image (bytes, file extension), if any.
    pub artist_image: Option<(Vec<u8>, &'static str)>,
}

/// Downloads a whole track (with metadata and cover art) from `owner` into
/// `dir/<stem>.<ext>`. An already complete cached audio file is reused;
/// the metadata and cover still come fresh from the header.
pub async fn download_track(
    service: &MusicDhtService,
    owner: EndpointId,
    item_id_hex: &str,
    dir: &Path,
    stem: &str,
) -> Result<Downloaded> {
    let mut stream = service
        .open_stream(owner, AUDIO_ALPN)
        .await
        .map_err(|err| anyhow::anyhow!("cannot reach the owner peer: {err}"))?;
    write_line(
        &mut stream.send,
        &AudioRequest {
            item_id: item_id_hex.to_string(),
            offset: 0,
            want_cover: true,
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

    // The image segments precede the audio bytes and are read regardless of
    // the cache state — they sit first in the stream.
    let mut read_image = async |size: u64, mime: &str, what: &str| -> Result<Option<(Vec<u8>, &'static str)>> {
        if size == 0 {
            return Ok(None);
        }
        anyhow::ensure!(
            size <= MAX_COVER_BYTES,
            "{what} of {size} bytes exceeds the {MAX_COVER_BYTES} byte limit"
        );
        let mut bytes = vec![0u8; size as usize];
        stream
            .recv
            .read_exact(&mut bytes)
            .await
            .with_context(|| format!("stream ended inside the {what} segment"))?;
        Ok(Some((bytes, image_extension(mime))))
    };
    let cover = read_image(header.cover_size, &header.cover_mime, "cover").await?;
    let artist_image = read_image(
        header.artist_image_size,
        &header.artist_image_mime,
        "artist image",
    )
    .await?;

    let extension = extension_for_mime(&header.mime_type);
    let path = dir.join(format!("{stem}.{extension}"));
    if let Ok(metadata) = tokio::fs::metadata(&path).await
        && metadata.len() == header.total_size
        && header.total_size > 0
    {
        // Audio already fully downloaded earlier; no need to fetch again.
        return Ok(Downloaded {
            path,
            mime_type: header.mime_type,
            metadata: header.metadata,
            cover,
            artist_image,
        });
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
    Ok(Downloaded {
        path,
        mime_type: header.mime_type,
        metadata: header.metadata,
        cover,
        artist_image,
    })
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

/// What the serving side needs to answer one audio request.
struct Served {
    file_path: String,
    metadata: TrackMetadata,
    cover_path: Option<String>,
    artist_image_path: Option<String>,
}

/// Resolves the item to the audio file, metadata and cover.
fn resolve_for_serving(
    library: &Library,
    own: EndpointId,
    item_id: ItemId,
) -> Result<Option<Served>> {
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
    let artist_image_path = track
        .artists
        .first()
        .and_then(|artist| library.artist_image(artist.id).ok().flatten());
    Ok(Some(Served {
        cover_path: track.cover_path.clone(),
        artist_image_path,
        file_path: track.file_path,
        metadata,
    }))
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
    let served = match resolved {
        Ok(found) => found,
        Err(message) => return refuse(stream, message).await,
    };

    let path = PathBuf::from(&served.file_path);
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(err) => return refuse(stream, format!("audio file is not readable: {err}")).await,
    };
    let total_size = file.metadata().await?.len();
    let offset = request.offset.min(total_size);
    if offset > 0 {
        file.seek(std::io::SeekFrom::Start(offset)).await?;
    }

    // Images ride between the header and the audio, when asked for.
    let (cover, artist_image) = if request.want_cover {
        (
            load_cover(served.cover_path.as_deref()).await,
            load_cover(served.artist_image_path.as_deref()).await,
        )
    } else {
        (None, None)
    };
    write_line(
        &mut stream.send,
        &AudioResponseHeader {
            ok: true,
            error: None,
            mime_type: guess_mime(&path).to_string(),
            total_size,
            offset,
            metadata: Some(served.metadata),
            cover_size: cover.as_ref().map_or(0, |(bytes, _)| bytes.len() as u64),
            cover_mime: cover
                .as_ref()
                .map(|(_, mime)| mime.to_string())
                .unwrap_or_default(),
            artist_image_size: artist_image
                .as_ref()
                .map_or(0, |(bytes, _)| bytes.len() as u64),
            artist_image_mime: artist_image
                .as_ref()
                .map(|(_, mime)| mime.to_string())
                .unwrap_or_default(),
        },
    )
    .await?;
    if let Some((bytes, _)) = &cover {
        stream.send.write_all(bytes).await?;
    }
    if let Some((bytes, _)) = &artist_image {
        stream.send.write_all(bytes).await?;
    }
    tokio::io::copy(&mut file, &mut stream.send).await?;
    stream.send.finish()?;
    // Wait until the peer read everything (or gave up) before dropping the
    // stream, otherwise the tail of the file is lost.
    let _ = stream.send.stopped().await;
    Ok(())
}

/// Reads a cover image from disk, skipping unreadable or oversized files.
async fn load_cover(cover_path: Option<&str>) -> Option<(Vec<u8>, &'static str)> {
    let path = PathBuf::from(cover_path?);
    let size = tokio::fs::metadata(&path).await.ok()?.len();
    if size == 0 || size > MAX_COVER_BYTES {
        return None;
    }
    let bytes = tokio::fs::read(&path).await.ok()?;
    Some((bytes, image_mime(&path)))
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
            cover_size: 0,
            cover_mime: String::new(),
            artist_image_size: 0,
            artist_image_mime: String::new(),
        },
    )
    .await?;
    stream.send.finish()?;
    let _ = stream.send.stopped().await;
    anyhow::bail!("refused audio request: {message}");
}
