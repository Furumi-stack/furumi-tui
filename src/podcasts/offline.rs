//! Explicit device-local downloads, outside the podcast sync store.
use anyhow::{Result, bail};
use music_dht::podcast::PodcastEpisodeRef;
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
pub fn key(id: &str) -> String {
    blake3::hash(id.as_bytes()).to_hex().to_string()
}
pub fn path(dir: &Path, ep: &PodcastEpisodeRef) -> PathBuf {
    dir.join(key(ep.episode_id.as_str()))
}
pub fn available(dir: &Path) -> Result<std::collections::HashSet<String>> {
    if !dir.exists() {
        return Ok(Default::default());
    }
    Ok(fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let n = e.file_name().to_str()?.to_owned();
            (n.len() == 64 && n.bytes().all(|b| b.is_ascii_hexdigit())).then_some(n)
        })
        .collect())
}
pub fn cleanup(
    dir: &Path,
    days: u32,
    now: SystemTime,
) -> Result<std::collections::HashSet<String>> {
    for name in available(dir)? {
        let p = dir.join(name);
        if now
            .duration_since(fs::metadata(&p)?.modified()?)
            .unwrap_or_default()
            >= Duration::from_secs(u64::from(days.max(1)) * 86400)
        {
            let _ = fs::remove_file(p);
        }
    }
    available(dir)
}
pub fn open(dir: &Path, ep: &PodcastEpisodeRef) -> Result<Option<File>> {
    match File::open(path(dir, ep)) {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
#[cfg(test)]
pub fn download(dir: &Path, ep: &PodcastEpisodeRef) -> Result<()> {
    download_with_cancel(dir, ep, super::stream::CancelToken::never())
}
pub fn download_with_cancel(
    dir: &Path,
    ep: &PodcastEpisodeRef,
    cancel: super::stream::CancelToken,
) -> Result<()> {
    let target = path(dir, ep);
    let dir = target.parent().unwrap();
    fs::create_dir_all(dir)?;
    if target.is_file() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let mut stream = super::stream::open_with(
        &ep.audio_url,
        super::stream::CancelToken::new(move || {
            cancel.is_cancelled() || started.elapsed() > Duration::from_secs(7200)
        }),
    )?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    let mut buf = [0; 65536];
    let mut total = 0u64;
    loop {
        let n = stream.reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > 4 * 1024 * 1024 * 1024 {
            bail!("episode exceeds 4 GiB");
        }
        temp.write_all(&buf[..n])?;
    }
    if total == 0
        || stream.probe.end() == super::stream::StreamEnd::Failed
        || stream.byte_len.is_some_and(|len| len != total)
    {
        bail!("incomplete episode download");
    }
    temp.as_file().sync_all()?;
    temp.persist(target)
        .map_err(|_| anyhow::anyhow!("cannot save episode"))?;
    Ok(())
}
