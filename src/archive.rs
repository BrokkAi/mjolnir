//! Bounded download and archive extraction used by runtime bootstrap helpers.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
pub async fn download_and_extract(url: &str, dest: &Path) -> Result<()> {
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build archive HTTP client")?
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = response.status();
    anyhow::ensure!(status.is_success(), "GET {url}: HTTP {status}");
    let total_bytes = response.content_length();
    let mut bytes = Vec::with_capacity(total_bytes.unwrap_or(0) as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("read chunk from {url}"))?;
        bytes.extend_from_slice(&chunk);
    }
    let kind = ArchiveKind::from_url(url);
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || extract(&bytes, kind, &dest))
        .await
        .context("join archive extraction")??;
    Ok(())
}

#[derive(Clone, Copy)]
enum ArchiveKind {
    TarGz,
    Zip,
}

impl ArchiveKind {
    fn from_url(url: &str) -> Self {
        if url.to_ascii_lowercase().ends_with(".zip") {
            Self::Zip
        } else {
            Self::TarGz
        }
    }
}

fn extract(bytes: &[u8], kind: ArchiveKind, dest: &Path) -> Result<()> {
    match kind {
        ArchiveKind::TarGz => tar::Archive::new(flate2::read::GzDecoder::new(bytes))
            .unpack(dest)
            .with_context(|| format!("unpack tar.gz to {}", dest.display())),
        ArchiveKind::Zip => extract_zip(bytes, dest),
    }
}

fn extract_zip(bytes: &[u8], dest: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("open zip archive")?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("zip entry {index}"))?;
        let Some(relative) = entry.enclosed_name() else {
            continue;
        };
        let output = PathBuf::from(dest).join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes)?;
        std::fs::write(&output, bytes)?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&output, std::fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}
