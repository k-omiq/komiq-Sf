//! Fetch the pinned connector, checking the release bytes before unpacking or running them.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

// Change this version and these hashes together with deploy/selfhost/Dockerfile.
// That Dockerfile's pin must likewise track this table.
pub const VERSION: &str = "2026.9.3";
pub const BASE: &str = "https://github.com/cloudflare/cloudflared/releases/download/2026.9.3";
const MAX_BYTES: usize = 150 * 1024 * 1024;
pub const UNSUPPORTED: &str = "There is no pinned cloudflared build for this system. Install cloudflared and set CLOUDFLARED_BIN to its path.";

pub struct Release<'a> {
    pub asset: &'a str,
    pub sha256: &'a str,
}

pub fn release(os: &str, arch: &str) -> Option<Release<'static>> {
    let (asset, sha256) = match (os, arch) {
        ("linux", "x86_64") => (
            "cloudflared-linux-amd64",
            "77e26d8d900e0b8469f416239d14b5f296525fdf79fee6f511ef55609e3fbac2",
        ),
        ("linux", "aarch64") => (
            "cloudflared-linux-arm64",
            "aaeb2d7d0da3614634c7e03ab13487a1522c2e79165ed2929cfe23d5e95b326d",
        ),
        ("macos", "x86_64") => (
            "cloudflared-darwin-amd64.tgz",
            "d1155d0837487f261183b15c1eab6c4ebcad9dc49b94675f1524c3564cea3977",
        ),
        ("macos", "aarch64") => (
            "cloudflared-darwin-arm64.tgz",
            "587c2cfb1c230fe36c7fa7727da78be459dae028cabe8c001291999350f07095",
        ),
        ("windows", "x86_64") => (
            "cloudflared-windows-amd64.exe",
            "f096265ec2fcbe9bb6e2d64268db167ced3fcbb83d894bdb9e2fcdb26f2ea7e2",
        ),
        _ => return None,
    };
    Some(Release { asset, sha256 })
}

pub fn current_release() -> Option<Release<'static>> {
    release(std::env::consts::OS, std::env::consts::ARCH)
}

/// `pin` is passed in so tests can serve their own release bytes and digest.
pub async fn fetch(data_dir: &Path, base: &str, pin: &Release<'_>) -> Result<PathBuf, String> {
    let http = reqwest::Client::builder()
        // Up to 55 MB: leave a slow home connection the time to finish.
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| format!("Could not download cloudflared: {e}"))?;
    let mut response = http
        .get(format!("{}/{}", base.trim_end_matches('/'), pin.asset))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("Could not download cloudflared: {}", e.without_url()))?;
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BYTES as u64)
    {
        return Err("The cloudflared download is too large.".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("Could not download cloudflared: {}", e.without_url()))?
    {
        if chunk.len() > MAX_BYTES - bytes.len() {
            return Err("The cloudflared download is too large.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    install(data_dir, pin, &bytes)
}

fn install(data_dir: &Path, pin: &Release<'_>, bytes: &[u8]) -> Result<PathBuf, String> {
    if hex::encode(Sha256::digest(bytes)) != pin.sha256 {
        return Err("The cloudflared download failed its checksum check. Retry setup.".into());
    }
    let binary = if pin.asset.ends_with(".tgz") {
        unpack(bytes)?
    } else {
        bytes.to_vec()
    };
    if binary.is_empty() {
        return Err("The cloudflared download is empty. Retry setup.".into());
    }
    let dir = data_dir.join("bin");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Could not save cloudflared: {e}"))?;
    let target = dir.join(if pin.asset.ends_with(".exe") {
        "cloudflared.exe"
    } else {
        "cloudflared"
    });
    let temp = dir.join(format!(".cloudflared-{}.tmp", uuid::Uuid::new_v4()));
    let saved = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&binary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, &target)
    })();
    if let Err(e) = saved {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("Could not save cloudflared: {e}"));
    }
    Ok(target)
}

/// The macOS release is a gzipped tar that holds the `cloudflared` program. Walk the
/// entries and return that file's bytes; other entries (tar's own extended headers,
/// macOS metadata files) are skipped. Nothing is written by archive path, and the
/// archive passed its checksum before it got here.
fn unpack(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let fail = || "The cloudflared archive could not be read. Retry setup.".to_string();
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut tar)
        .map_err(|_| fail())?;
    if tar.len() > MAX_BYTES {
        return Err(fail());
    }
    let mut at = 0;
    while let Some(header) = tar.get(at..at + 512) {
        // Two empty blocks end an archive.
        if header.iter().all(|b| *b == 0) {
            break;
        }
        let name = std::str::from_utf8(&header[..100])
            .map_err(|_| fail())?
            .trim_end_matches('\0');
        let size = std::str::from_utf8(&header[124..136])
            .map_err(|_| fail())?
            .trim_matches(['\0', ' ']);
        let size = usize::from_str_radix(size, 8).map_err(|_| fail())?;
        let data = at + 512;
        let end = data.checked_add(size).ok_or_else(fail)?;
        let regular_file = matches!(header[156], 0 | b'0');
        if regular_file && name.rsplit('/').next() == Some("cloudflared") {
            return tar.get(data..end).map(<[u8]>::to_vec).ok_or_else(fail);
        }
        at = data + size.div_ceil(512) * 512;
    }
    Err(fail())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};

    async fn download(
        bytes: Vec<u8>,
        asset: &str,
        good: bool,
    ) -> (Result<PathBuf, String>, PathBuf) {
        let digest = hex::encode(Sha256::digest(&bytes));
        let router = Router::new().route(
            "/{asset}",
            get(move || {
                let bytes = bytes.clone();
                async move { bytes }
            }),
        );
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", tcp.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(tcp, router).await.unwrap();
        });
        let dir = std::env::temp_dir().join(format!("komika-fetch-{}", uuid::Uuid::new_v4()));
        let result = fetch(
            &dir,
            &base,
            &Release {
                asset,
                sha256: if good { &digest } else { "bad" },
            },
        )
        .await;
        server.abort();
        (result, dir)
    }

    #[test]
    fn verified_install_is_atomic_and_mismatch_leaves_the_previous_binary() {
        let dir = std::env::temp_dir().join(format!("komika-install-{}", uuid::Uuid::new_v4()));
        let bytes = b"binary";
        let digest = hex::encode(Sha256::digest(bytes));
        let pin = Release {
            asset: "plain",
            sha256: &digest,
        };
        assert!(install(&dir, &pin, b"tampered")
            .unwrap_err()
            .contains("checksum"));
        assert!(!dir.exists());
        let target = install(&dir, &pin, bytes).unwrap();
        assert!(install(&dir, &pin, b"tampered").is_err());
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(dir.join("bin")).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn checksum_mismatch_keeps_no_file() {
        let (result, dir) = download(b"binary".to_vec(), "plain", false).await;
        assert!(result.unwrap_err().contains("checksum"));
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn good_plain_binary_is_saved() {
        let (result, dir) = download(b"binary".to_vec(), "plain", true).await;
        let path = result.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn tgz() -> Vec<u8> {
        let mut tar = vec![0; 2048];
        tar[..11].copy_from_slice(b"cloudflared");
        tar[124..136].copy_from_slice(b"00000000006\0");
        tar[156] = b'0';
        tar[512..518].copy_from_slice(b"binary");
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&tar).unwrap();
        gzip.finish().unwrap()
    }

    fn gz(tar: &[u8]) -> Vec<u8> {
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(tar).unwrap();
        gzip.finish().unwrap()
    }

    /// One tar entry: a header block and the data padded to a block.
    fn entry(name: &str, kind: u8, data: &[u8]) -> Vec<u8> {
        let mut block = vec![0; 512 + data.len().div_ceil(512) * 512];
        block[..name.len()].copy_from_slice(name.as_bytes());
        block[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        block[156] = kind;
        block[512..512 + data.len()].copy_from_slice(data);
        block
    }

    #[test]
    fn archive_reader_finds_the_program_and_rejects_bad_archives() {
        assert_eq!(unpack(&tgz()).unwrap(), b"binary");
        assert!(unpack(b"not gzip").is_err());
        // No program inside.
        let other = [entry("../anything", b'0', b"binary"), vec![0; 1024]].concat();
        assert!(unpack(&gz(&other)).is_err());
        // A directory with the program's name is not the program.
        let dir = [entry("cloudflared", b'5', b""), vec![0; 1024]].concat();
        assert!(unpack(&gz(&dir)).is_err());
        // The data is cut short.
        let mut cut = entry("cloudflared", b'0', &[7; 600]);
        cut.truncate(512 + 100);
        assert!(unpack(&gz(&cut)).is_err());
    }

    #[test]
    fn archive_reader_skips_what_tar_and_macos_add() {
        // An extended header, a macOS metadata file and a folder prefix, as tar on a
        // Mac may write them, before the program itself.
        let tar = [
            entry(
                "./PaxHeader/cloudflared",
                b'x',
                b"30 mtime=1700000000.000000000\n",
            ),
            entry("./._cloudflared", b'0', &[1; 700]),
            entry("./cloudflared", b'0', &[9; 1300]),
            vec![0; 1024],
        ]
        .concat();
        assert_eq!(unpack(&gz(&tar)).unwrap(), vec![9; 1300]);
    }

    #[tokio::test]
    async fn good_tgz_is_verified_then_unpacked() {
        let (result, dir) = download(tgz(), "mac.tgz", true).await;
        assert_eq!(std::fs::read(result.unwrap()).unwrap(), b"binary");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pinned_platforms_and_unsupported_platforms() {
        for (os, arch) in [
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "x86_64"),
            ("macos", "aarch64"),
            ("windows", "x86_64"),
        ] {
            assert_eq!(release(os, arch).unwrap().sha256.len(), 64);
        }
        assert!(release("windows", "aarch64").is_none());
        assert!(release("freebsd", "x86_64").is_none());
    }

    /// The real pinned release for this machine, from GitHub. Run it by hand when the
    /// pin changes: `cargo test real_pinned_release -- --ignored --nocapture`. It
    /// proves the address, the digest and the archive layout; it never runs the program.
    #[tokio::test]
    #[ignore = "downloads the pinned cloudflared release from GitHub"]
    async fn real_pinned_release_downloads_and_verifies() {
        let pin = current_release().expect("a pinned build for this system");
        let dir = std::env::temp_dir().join(format!("komika-real-{}", uuid::Uuid::new_v4()));
        let saved = fetch(&dir, BASE, &pin).await.unwrap();
        let binary = std::fs::read(&saved).unwrap();
        println!(
            "{}: {} bytes, starts with {}",
            pin.asset,
            binary.len(),
            hex::encode(&binary[..4])
        );
        assert!(binary.len() > 1024 * 1024);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&saved).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
