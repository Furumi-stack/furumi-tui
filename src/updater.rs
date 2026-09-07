//! Manual portable-binary updates. All network/disk work runs off the UI thread.
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use object::Object;
use reqwest::blocking::Client;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const REPOSITORY: &str = "https://api.github.com/repos/house-of-vanity/furumi_tui";
const MAX_ARCHIVE: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub version: String,
    asset: Asset,
    checksums: Asset,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Debug, Default)]
pub struct State {
    pub busy: bool,
    pub installed: bool,
    pub available: Option<Update>,
    pub message: String,
}

fn client() -> Result<Client> {
    // Reuse the ring backend already used by federation; respect an existing provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(Client::builder()
        .user_agent(concat!("furumi/", env!("CARGO_PKG_VERSION")))
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .build()?)
}

fn read_limited(mut reader: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.by_ref().take(limit + 1).read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= limit, "download exceeds size limit");
    Ok(data)
}

fn select_release(release: Release, current: &str, os: &str, arch: &str) -> Result<Option<Update>> {
    let version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let next = Version::parse(version).context("invalid release version")?;
    if release.draft
        || release.prerelease
        || !next.pre.is_empty()
        || next <= Version::parse(current)?
    {
        return Ok(None);
    }
    let platform = match os {
        "linux" => "linux",
        "macos" => "macos",
        "windows" => "windows",
        _ => bail!("self-update is unsupported on {os}"),
    };
    let extension = if os == "windows" { "zip" } else { "tar.gz" };
    let name = format!("furumi-{platform}-{arch}-{version}.{extension}");
    let find = |name: &str| -> Result<Asset> {
        let matches: Vec<_> = release
            .assets
            .iter()
            .filter(|asset| asset.name == name)
            .collect();
        ensure!(matches.len() == 1, "release has no unique {name} asset");
        Ok(matches[0].clone())
    };
    Ok(Some(Update {
        version: version.to_owned(),
        asset: find(&name)?,
        checksums: find("SHA256SUMS")?,
    }))
}

pub fn check() -> Result<Option<Update>> {
    let response = client()?
        .get(format!("{REPOSITORY}/releases/latest"))
        .timeout(Duration::from_secs(20))
        .send()?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let release = serde_json::from_slice(&read_limited(
        response.error_for_status()?,
        2 * 1024 * 1024,
    )?)?;
    select_release(
        release,
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

fn checksum(text: &str, name: &str) -> Result<String> {
    let mut found = None;
    for line in text.lines() {
        let Some((hash, filename)) = line.split_once(' ') else {
            continue;
        };
        if filename.trim_start().trim_start_matches('*') != name {
            continue;
        }
        ensure!(found.is_none(), "duplicate checksum for {name}");
        ensure!(
            hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid SHA-256 for {name}"
        );
        found = Some(hash.to_ascii_lowercase());
    }
    found.context("release does not contain a checksum for the selected archive")
}

fn extract(archive: &Path, name: &str, output: &mut File) -> Result<()> {
    let binary = if cfg!(windows) {
        "furumi.exe"
    } else {
        "furumi"
    };
    let root = name
        .strip_suffix(".tar.gz")
        .or_else(|| name.strip_suffix(".zip"))
        .context("unsupported archive")?;
    // Existing release archives omit the version in their inner directory.
    let root = root.rsplit_once('-').context("invalid archive name")?.0;
    let expected = format!("{root}/{binary}");
    let mut count = 0;
    if name.ends_with(".zip") {
        let mut archive = zip::ZipArchive::new(File::open(archive)?)?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            if entry.name() != expected {
                continue;
            }
            ensure!(
                entry.is_file() && !entry.is_symlink(),
                "binary is not a regular file"
            );
            count += 1;
            ensure!(count == 1, "duplicate binary in archive");
            output.write_all(&read_limited(&mut entry, MAX_ARCHIVE)?)?;
        }
    } else {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive)?));
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry.path()?.as_ref() != Path::new(&expected) {
                continue;
            }
            ensure!(
                entry.header().entry_type().is_file(),
                "binary is not a regular file"
            );
            count += 1;
            ensure!(count == 1, "duplicate binary in archive");
            output.write_all(&read_limited(&mut entry, MAX_ARCHIVE)?)?;
        }
    }
    ensure!(count == 1, "archive does not contain {expected}");
    output.sync_all()?;
    Ok(())
}

fn validate_binary(current: &[u8], candidate: &[u8]) -> Result<()> {
    let current = object::File::parse(current).context("cannot inspect installed binary")?;
    let candidate = object::File::parse(candidate).context("invalid downloaded binary")?;
    ensure!(
        candidate.kind() == object::ObjectKind::Executable
            || candidate.kind() == object::ObjectKind::Dynamic,
        "download is not an executable"
    );
    ensure!(
        candidate.format() == current.format()
            && candidate.architecture() == current.architecture()
            && candidate.is_64() == current.is_64()
            && candidate.is_little_endian() == current.is_little_endian(),
        "downloaded binary has incompatible platform or architecture"
    );
    Ok(())
}

pub fn install(update: &Update, mut progress: impl FnMut(String)) -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let parent = exe.parent().context("executable has no parent directory")?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(parent.join(".furumi-update.lock"))
        .context("cannot write to installation directory")?;
    fs2::FileExt::try_lock_exclusive(&lock).context("another furumi instance is updating")?;
    // Keep staging on the destination filesystem and never overwrite a running image.
    let staging = tempfile::Builder::new()
        .prefix(".furumi-update-")
        .tempdir_in(parent)
        .context("cannot create update staging directory")?;
    let client = client()?;
    let sums = read_limited(
        client
            .get(&update.checksums.browser_download_url)
            .send()?
            .error_for_status()?,
        1024 * 1024,
    )?;
    let expected = checksum(std::str::from_utf8(&sums)?, &update.asset.name)?;
    let mut response = client
        .get(&update.asset.browser_download_url)
        .send()?
        .error_for_status()?;
    let total = response.content_length();
    ensure!(
        total.is_none_or(|size| size <= MAX_ARCHIVE),
        "archive exceeds size limit"
    );
    let archive = staging.path().join("download");
    let mut file = File::create(&archive)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut downloaded = 0u64;
    let mut reported = u64::MAX;
    loop {
        let count = response.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        downloaded += count as u64;
        ensure!(downloaded <= MAX_ARCHIVE, "archive exceeds size limit");
        file.write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
        let mb = downloaded / (1024 * 1024);
        if mb != reported {
            progress(format!("Downloading: {mb} MiB"));
            reported = mb;
        }
    }
    file.sync_all()?;
    drop(file);
    ensure!(
        format!("{:x}", hash.finalize()) == expected,
        "SHA-256 mismatch; update was not installed"
    );
    progress("Verifying binary...".into());
    let binary = staging.path().join(if cfg!(windows) {
        "furumi.exe"
    } else {
        "furumi"
    });
    let mut output = File::create(&binary)?;
    extract(&archive, &update.asset.name, &mut output)?;
    drop(output);
    validate_binary(&std::fs::read(&exe)?, &std::fs::read(&binary)?)?;
    std::fs::set_permissions(&binary, std::fs::metadata(&exe)?.permissions())?;
    progress("Installing...".into());
    self_replace::self_replace(&binary).context("could not replace executable")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn https_client_can_be_constructed() {
        client().unwrap();
    }

    #[test]
    fn extracts_only_expected_binary_from_release_archives() {
        let temp = tempfile::tempdir().unwrap();
        let binary = if cfg!(windows) {
            "furumi.exe"
        } else {
            "furumi"
        };
        let expected = format!("furumi-windows-x86_64/{binary}");
        let payload = b"test executable";
        let zip_path = temp.path().join("test.zip");
        let mut zip = zip::ZipWriter::new(File::create(&zip_path).unwrap());
        zip.start_file("README.md", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"readme").unwrap();
        zip.start_file(&expected, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(payload).unwrap();
        zip.finish().unwrap();
        let output = temp.path().join("output");
        extract(
            &zip_path,
            "furumi-windows-x86_64-0.1.6.zip",
            &mut File::create(&output).unwrap(),
        )
        .unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), payload);
        assert!(
            extract(
                &zip_path,
                "furumi-linux-x86_64-0.1.6.zip",
                &mut File::create(&output).unwrap()
            )
            .is_err()
        );

        let tar_path = temp.path().join("test.tar.gz");
        let gzip = flate2::write::GzEncoder::new(
            File::create(&tar_path).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gzip);
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, &expected, payload.as_slice())
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        extract(
            &tar_path,
            "furumi-windows-x86_64-0.1.6.tar.gz",
            &mut File::create(&output).unwrap(),
        )
        .unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), payload);
    }

    #[test]
    fn binary_validation_rejects_wrong_architecture() {
        let current = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let mut other = current.clone();
        // Change only the machine field, preserving an otherwise valid executable.
        if current.starts_with(b"MZ") {
            let pe = u32::from_le_bytes(current[0x3c..0x40].try_into().unwrap()) as usize;
            let machine: u16 = if current[pe + 4..pe + 6] == [0x64, 0x86] {
                0xaa64
            } else {
                0x8664
            };
            other[pe + 4..pe + 6].copy_from_slice(&machine.to_le_bytes());
        } else if current.starts_with(b"\x7fELF") {
            let machine: u16 = if current[18..20] == [62, 0] { 183 } else { 62 };
            other[18..20].copy_from_slice(&machine.to_le_bytes());
        } else if current.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) {
            let cpu: u32 = if current[4] == 7 {
                0x0100000c
            } else {
                0x01000007
            };
            other[4..8].copy_from_slice(&cpu.to_le_bytes());
        } else {
            panic!("unsupported test executable format");
        }
        assert!(validate_binary(&current, &other).is_err());
    }
    #[test]
    fn checksums_require_exact_unique_valid_entry() {
        let hash = "ab".repeat(32);
        assert_eq!(
            checksum(&format!("{hash}  app.zip\n"), "app.zip").unwrap(),
            hash
        );
        assert!(checksum(&format!("{hash}  other.zip"), "app.zip").is_err());
        assert!(checksum("broken  app.zip", "app.zip").is_err());
        assert!(checksum(&format!("{hash}  app.zip\n{hash}  app.zip"), "app.zip").is_err());
    }
    #[test]
    fn release_selection_respects_semver_and_assets() {
        let release = |version: &str| Release {
            tag_name: version.into(),
            draft: false,
            prerelease: false,
            assets: vec![
                Asset {
                    name: "furumi-linux-x86_64-0.1.10.tar.gz".into(),
                    browser_download_url: String::new(),
                },
                Asset {
                    name: "SHA256SUMS".into(),
                    browser_download_url: String::new(),
                },
            ],
        };
        assert!(
            select_release(release("v0.1.10"), "0.1.9", "linux", "x86_64")
                .unwrap()
                .is_some()
        );
        assert!(
            select_release(release("v0.1.8"), "0.1.9", "linux", "x86_64")
                .unwrap()
                .is_none()
        );
        assert!(
            select_release(release("v0.2.0-beta.1"), "0.1.9", "linux", "x86_64")
                .unwrap()
                .is_none()
        );
        assert!(select_release(release("v0.1.10"), "0.1.9", "linux", "aarch64").is_err());
    }
    #[test]
    fn binary_validation_rejects_corrupt_download() {
        let current = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        validate_binary(&current, &current).unwrap();
        assert!(validate_binary(&current, b"not a binary").is_err());
    }
}
