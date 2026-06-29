//! Install a registry agent's binary distribution locally.
//!
//! Downloads the archive declared in `BinaryTarget`, extracts it under
//! `~/.cache/mj/agents/<id>/<version>/`, and returns the launch command
//! ready to spawn. Idempotent: once the `.installed` sentinel is present,
//! subsequent calls only resolve paths.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::registry::BinaryTarget;

/// Streaming progress event emitted while downloading.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Started fetching `total_bytes` (may be `None` if the server omits Content-Length).
    Started { total_bytes: Option<u64> },
    /// Received another chunk; `downloaded_bytes` is the cumulative count.
    Downloaded { downloaded_bytes: u64 },
    /// Download finished; extraction begins.
    Extracting,
    /// Install complete.
    Done,
}

/// Where extracted agents live: `$XDG_CACHE_HOME/mj/agents` (or
/// `~/.cache/mj/agents`). One subdirectory per `<id>/<version>`.
pub fn default_install_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("mj")
        .join("agents")
}

/// Install (if needed) the binary for `agent_id` at `version` and return
/// the resolved (program, args) launch command. Skips download+extract
/// when the install sentinel is already present.
///
/// `progress_tx` receives streaming updates so a UI can show a spinner.
/// Pass a no-op channel (`mpsc::unbounded_channel().0`) to ignore them.
pub async fn install_or_resolve(
    agent_id: &str,
    version: &str,
    target: &BinaryTarget,
    install_root: &Path,
    progress_tx: mpsc::UnboundedSender<Progress>,
) -> Result<(PathBuf, Vec<String>)> {
    let dir = install_root.join(agent_id).join(version);
    let sentinel = dir.join(".installed");

    if !sentinel.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create install dir {}", dir.display()))?;
        download_and_extract(&target.archive, &dir, &progress_tx).await?;
        std::fs::write(&sentinel, "ok")
            .with_context(|| format!("write install sentinel {}", sentinel.display()))?;
    }

    let program = resolve_cmd_path(&dir, &target.cmd)?;
    let _ = progress_tx.send(Progress::Done);
    Ok((program, target.args.clone()))
}

/// Resolve the launch command for an already-installed binary agent
/// **without downloading**. Returns `None` when the install sentinel is
/// absent (the agent was never installed) or the executable can no longer
/// be located. Used by the startup validation probe so it can mark
/// uninstalled binary agents "not installed" instead of fetching them.
pub fn resolve_installed(
    agent_id: &str,
    version: &str,
    target: &BinaryTarget,
    install_root: &Path,
) -> Option<(PathBuf, Vec<String>)> {
    let dir = install_root.join(agent_id).join(version);
    if !dir.join(".installed").exists() {
        return None;
    }
    let program = resolve_cmd_path(&dir, &target.cmd).ok()?;
    Some((program, target.args.clone()))
}

/// Resolve `cmd` (e.g. `./codex-acp`, `./bin/agent.exe`) against the
/// install directory. Rejects paths that escape `dir` for safety.
fn resolve_cmd_path(dir: &Path, cmd: &str) -> Result<PathBuf> {
    let stripped = cmd.strip_prefix("./").unwrap_or(cmd);
    let joined = dir.join(stripped);
    // Refuse anything that walks out of the install dir; archive entries
    // are untrusted input.
    let canon_dir = std::fs::canonicalize(dir)
        .with_context(|| format!("canonicalize install dir {}", dir.display()))?;
    let canon_cmd = std::fs::canonicalize(&joined)
        .with_context(|| format!("locate executable {}", joined.display()))?;
    if !canon_cmd.starts_with(&canon_dir) {
        anyhow::bail!(
            "executable {} resolves outside install dir {}",
            canon_cmd.display(),
            canon_dir.display()
        );
    }
    Ok(canon_cmd)
}

pub(crate) async fn download_and_extract(
    url: &str,
    dest: &Path,
    progress_tx: &mpsc::UnboundedSender<Progress>,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build http client")?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("GET {url}: HTTP {status}");
    }

    let total_bytes = resp.content_length();
    let _ = progress_tx.send(Progress::Started { total_bytes });

    let mut bytes: Vec<u8> = Vec::with_capacity(total_bytes.unwrap_or(0) as usize);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("read chunk from {url}"))?;
        bytes.extend_from_slice(&chunk);
        let _ = progress_tx.send(Progress::Downloaded {
            downloaded_bytes: bytes.len() as u64,
        });
    }

    let _ = progress_tx.send(Progress::Extracting);
    let kind = archive_kind(url);
    // Extraction is CPU-bound; run it on a blocking thread so we don't
    // stall the tokio runtime.
    let dest_owned = dest.to_path_buf();
    tokio::task::spawn_blocking(move || extract(&bytes, kind, &dest_owned))
        .await
        .context("join extract task")??;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum ArchiveKind {
    TarGz,
    Zip,
}

fn archive_kind(url: &str) -> ArchiveKind {
    let lower = url.to_lowercase();
    if lower.ends_with(".zip") {
        ArchiveKind::Zip
    } else {
        // .tar.gz / .tgz / unknown all default to tar.gz, matching the
        // dominant pattern in the registry.
        ArchiveKind::TarGz
    }
}

fn extract(bytes: &[u8], kind: ArchiveKind, dest: &Path) -> Result<()> {
    match kind {
        ArchiveKind::TarGz => extract_tar_gz(bytes, dest),
        ArchiveKind::Zip => extract_zip(bytes, dest),
    }
}

fn extract_tar_gz(bytes: &[u8], dest: &Path) -> Result<()> {
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    archive
        .unpack(dest)
        .with_context(|| format!("unpack tar.gz to {}", dest.display()))?;
    Ok(())
}

fn extract_zip(bytes: &[u8], dest: &Path) -> Result<()> {
    let reader = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader).context("open zip archive")?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("zip entry {i}"))?;
        let out_path = match entry.enclosed_name() {
            Some(p) => dest.join(p),
            None => continue, // skip entries with absolute / parent-traversal names
        };
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)
                .with_context(|| format!("create dir {}", out_path.display()))?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create dir {}", parent.display()))?;
            }
            let mut out = std::fs::File::create(&out_path)
                .with_context(|| format!("create file {}", out_path.display()))?;
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry
                .read_to_end(&mut buf)
                .with_context(|| format!("read zip entry {}", out_path.display()))?;
            std::io::Write::write_all(&mut out, &buf)
                .with_context(|| format!("write {}", out_path.display()))?;
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fake_target(archive: &str, cmd: &str) -> BinaryTarget {
        BinaryTarget {
            archive: archive.to_string(),
            cmd: cmd.to_string(),
            args: vec!["--flag".to_string()],
        }
    }

    /// Build a small tar.gz in memory containing one executable file.
    fn make_tar_gz(file_name: &str, content: &[u8]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;

        let mut header = tar::Header::new_gnu();
        header.set_path(file_name).expect("path");
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();

        let mut tar_bytes: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            builder.append(&header, content).expect("append");
            builder.finish().expect("finish");
        }

        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar_bytes).expect("gz write");
        gz.finish().expect("gz finish")
    }

    #[test]
    fn archive_kind_dispatch() {
        assert!(matches!(archive_kind("foo.zip"), ArchiveKind::Zip));
        assert!(matches!(
            archive_kind("https://example.com/x.tar.gz"),
            ArchiveKind::TarGz
        ));
        assert!(matches!(archive_kind("path/x.tgz"), ArchiveKind::TarGz));
        assert!(matches!(archive_kind("no-ext"), ArchiveKind::TarGz));
    }

    #[test]
    fn extract_tar_gz_creates_expected_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = make_tar_gz("agent-bin", b"#!/bin/sh\necho ok\n");
        extract(&archive, ArchiveKind::TarGz, dir.path()).expect("extract");
        let path = dir.path().join("agent-bin");
        assert!(path.exists(), "expected {} to exist", path.display());
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.contains("echo ok"));
    }

    #[test]
    fn resolve_cmd_path_strips_dot_slash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("agent-bin");
        std::fs::write(&bin, b"x").expect("write");
        let resolved = resolve_cmd_path(dir.path(), "./agent-bin").expect("resolve");
        assert_eq!(
            std::fs::canonicalize(&resolved).expect("canon"),
            std::fs::canonicalize(&bin).expect("canon bin")
        );
    }

    #[test]
    fn resolve_cmd_path_rejects_parent_traversal() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("sub")).expect("mkdir");
        // Try to escape via `..`.
        let result = resolve_cmd_path(&dir.path().join("sub"), "../whatever");
        assert!(result.is_err(), "expected rejection, got {result:?}");
    }

    #[test]
    fn resolve_installed_returns_none_without_sentinel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let install_root = dir.path();
        // Create the agent dir + binary but NO `.installed` sentinel.
        let agent_dir = install_root.join("test-agent").join("1.0.0");
        std::fs::create_dir_all(&agent_dir).expect("mkdir");
        std::fs::write(agent_dir.join("bin"), b"#!/bin/sh\n").expect("write bin");

        let target = fake_target("https://not-used.example.com/x.tar.gz", "./bin");
        assert!(resolve_installed("test-agent", "1.0.0", &target, install_root).is_none());
    }

    #[test]
    fn resolve_installed_resolves_when_sentinel_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let install_root = dir.path();
        let agent_dir = install_root.join("test-agent").join("1.0.0");
        std::fs::create_dir_all(&agent_dir).expect("mkdir");
        let bin = agent_dir.join("bin");
        std::fs::write(&bin, b"#!/bin/sh\n").expect("write bin");
        std::fs::write(agent_dir.join(".installed"), "ok").expect("sentinel");

        let target = fake_target("https://not-used.example.com/x.tar.gz", "./bin");
        let (program, args) =
            resolve_installed("test-agent", "1.0.0", &target, install_root).expect("resolve");
        assert_eq!(
            std::fs::canonicalize(&program).expect("canon"),
            std::fs::canonicalize(&bin).expect("canon bin")
        );
        assert_eq!(args, vec!["--flag"]);
    }

    #[tokio::test]
    async fn install_skips_when_sentinel_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let install_root = dir.path();
        let agent_dir = install_root.join("test-agent").join("1.0.0");
        std::fs::create_dir_all(&agent_dir).expect("mkdir");
        let bin = agent_dir.join("bin");
        std::fs::write(&bin, b"#!/bin/sh\n").expect("write bin");
        std::fs::write(agent_dir.join(".installed"), "ok").expect("sentinel");

        let target = fake_target("https://not-used.example.com/x.tar.gz", "./bin");
        let (tx, _rx) = mpsc::unbounded_channel::<Progress>();
        let (program, args) = install_or_resolve("test-agent", "1.0.0", &target, install_root, tx)
            .await
            .expect("resolve");
        assert_eq!(
            std::fs::canonicalize(&program).expect("canon"),
            std::fs::canonicalize(&bin).expect("canon bin")
        );
        assert_eq!(args, vec!["--flag"]);
    }
}
