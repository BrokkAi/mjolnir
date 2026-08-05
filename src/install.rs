//! Installs binary ACP server distributions owned by Mjolnir.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::registry::BinaryTarget;

#[derive(Debug, Clone)]
pub enum Progress {
    Started { total_bytes: Option<u64> },
    Downloaded { downloaded_bytes: u64 },
    Extracting,
    Done,
}

pub fn default_install_root() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from(".local/share"))
        .join("mj")
        .join("agents")
}

pub async fn install_or_resolve(
    agent_id: &str,
    version: &str,
    target: &BinaryTarget,
    progress_tx: mpsc::UnboundedSender<Progress>,
) -> Result<(PathBuf, Vec<String>)> {
    install_or_resolve_at(
        &default_install_root(),
        agent_id,
        version,
        target,
        progress_tx,
    )
    .await
}

async fn install_or_resolve_at(
    install_root: &Path,
    agent_id: &str,
    version: &str,
    target: &BinaryTarget,
    progress_tx: mpsc::UnboundedSender<Progress>,
) -> Result<(PathBuf, Vec<String>)> {
    anyhow::ensure!(
        safe_path_component(agent_id),
        "invalid ACP registry agent id"
    );
    anyhow::ensure!(safe_path_component(version), "invalid ACP registry version");
    let directory = install_root.join(agent_id).join(version);
    let sentinel = directory.join(".installed");
    if !sentinel.exists() {
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("create install directory {}", directory.display()))?;
        download_and_extract(target, &directory, &progress_tx).await?;
        std::fs::write(&sentinel, "ok").with_context(|| format!("write {}", sentinel.display()))?;
    }
    let command = resolve_command(&directory, &target.cmd)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&command)?.permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        std::fs::set_permissions(&command, permissions)?;
    }
    let _ = progress_tx.send(Progress::Done);
    Ok((command, target.args.clone()))
}

fn safe_path_component(value: &str) -> bool {
    !value.is_empty()
        && Path::new(value).components().count() == 1
        && matches!(
            Path::new(value).components().next(),
            Some(std::path::Component::Normal(_))
        )
}

fn resolve_command(directory: &Path, command: &str) -> Result<PathBuf> {
    let candidate = directory.join(command.strip_prefix("./").unwrap_or(command));
    let root = std::fs::canonicalize(directory)
        .with_context(|| format!("canonicalize {}", directory.display()))?;
    let command = std::fs::canonicalize(&candidate)
        .with_context(|| format!("locate installed command {}", candidate.display()))?;
    anyhow::ensure!(
        command.starts_with(root),
        "installed command resolves outside its installation directory"
    );
    Ok(command)
}

async fn download_and_extract(
    target: &BinaryTarget,
    destination: &Path,
    progress_tx: &mpsc::UnboundedSender<Progress>,
) -> Result<()> {
    let url = &target.archive;
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build ACP installer client")?
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;
    let total_bytes = response.content_length();
    let _ = progress_tx.send(Progress::Started { total_bytes });
    let mut bytes = Vec::with_capacity(total_bytes.unwrap_or(0) as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.context("read ACP server archive")?);
        let _ = progress_tx.send(Progress::Downloaded {
            downloaded_bytes: bytes.len() as u64,
        });
    }
    if !target.sha256.is_empty() {
        let actual = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        anyhow::ensure!(
            actual.eq_ignore_ascii_case(&target.sha256),
            "ACP server archive checksum mismatch: expected {}, got {actual}",
            target.sha256
        );
    }
    let _ = progress_tx.send(Progress::Extracting);
    let destination = destination.to_path_buf();
    let archive = target.archive.clone();
    let command = target.cmd.clone();
    tokio::task::spawn_blocking(move || extract(&bytes, &archive, &command, &destination))
        .await
        .context("join ACP archive extraction")??;
    Ok(())
}

fn extract(bytes: &[u8], archive: &str, command: &str, destination: &Path) -> Result<()> {
    let archive = archive
        .split_once('?')
        .map_or(archive, |(path, _)| path)
        .to_ascii_lowercase();
    if archive.ends_with(".tar.gz") || archive.ends_with(".tgz") {
        return tar::Archive::new(flate2::read::GzDecoder::new(bytes))
            .unpack(destination)
            .with_context(|| format!("extract archive into {}", destination.display()));
    }
    if archive.ends_with(".tar.bz2") || archive.ends_with(".tbz2") {
        return tar::Archive::new(bzip2::read::BzDecoder::new(bytes))
            .unpack(destination)
            .with_context(|| format!("extract archive into {}", destination.display()));
    }
    if !archive.ends_with(".zip") {
        let relative = Path::new(command.strip_prefix("./").unwrap_or(command));
        anyhow::ensure!(
            relative.components().next().is_some()
                && relative
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_))),
            "raw binary command path escapes its installation directory"
        );
        let path = destination.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
        return Ok(());
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("open zip archive")?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).context("read zip entry")?;
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        let path = destination.join(name);
        if entry.is_dir() {
            std::fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = std::fs::File::create(&path)?;
        let mut contents = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut contents)?;
        std::io::Write::write_all(&mut output, &contents)?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::thread;

    use bzip2::write::BzEncoder;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use zip::write::SimpleFileOptions;

    use super::*;

    fn binary_target(
        archive: impl Into<String>,
        sha256: impl Into<String>,
        cmd: impl Into<String>,
    ) -> BinaryTarget {
        BinaryTarget {
            archive: archive.into(),
            sha256: sha256.into(),
            cmd: cmd.into(),
            args: vec!["--stdio".to_string()],
            env: HashMap::new(),
        }
    }

    fn make_tar(file_name: &str, content: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.set_path(file_name).expect("tar path");
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();

        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            builder.append(&header, content).expect("append tar file");
            builder.finish().expect("finish tar");
        }
        bytes
    }

    fn make_tar_gz(file_name: &str, content: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&make_tar(file_name, content))
            .expect("write gzip");
        encoder.finish().expect("finish gzip")
    }

    fn make_tar_bz2(file_name: &str, content: &[u8]) -> Vec<u8> {
        let mut encoder = BzEncoder::new(Vec::new(), bzip2::Compression::default());
        encoder
            .write_all(&make_tar(file_name, content))
            .expect("write bzip2");
        encoder.finish().expect("finish bzip2")
    }

    fn make_zip(traversal_name: Option<&str>) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        writer
            .add_directory("nested/", SimpleFileOptions::default())
            .expect("add directory");
        writer
            .start_file(
                "nested/agent",
                SimpleFileOptions::default().unix_permissions(0o755),
            )
            .expect("start file");
        writer.write_all(b"zip binary").expect("write file");
        if let Some(traversal_name) = traversal_name {
            writer
                .start_file(format!("../{traversal_name}"), SimpleFileOptions::default())
                .expect("start traversal file");
            writer
                .write_all(b"must not escape")
                .expect("write traversal file");
        }
        writer.finish().expect("finish zip").into_inner()
    }

    fn serve_once(status: &str, body: Vec<u8>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP server");
        let address = listener.local_addr().expect("HTTP server address");
        let status = status.to_string();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).expect("read request");
            let headers = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).expect("write headers");
            stream.write_all(&body).expect("write body");
        });
        (format!("http://{address}"), server)
    }

    fn sha256(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn drain_progress(mut receiver: mpsc::UnboundedReceiver<Progress>) -> Vec<Progress> {
        let mut progress = Vec::new();
        while let Ok(update) = receiver.try_recv() {
            progress.push(update);
        }
        progress
    }

    #[test]
    fn default_root_uses_the_mj_agents_suffix() {
        assert!(default_install_root().ends_with(Path::new("mj").join("agents")));
    }

    #[cfg(unix)]
    #[test]
    fn installed_command_cannot_escape_root() {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::NamedTempFile::new().expect("outside");
        let command = root.path().join("escape");
        std::os::unix::fs::symlink(outside.path(), &command).expect("symlink");
        assert!(resolve_command(root.path(), "escape").is_err());
    }

    #[test]
    fn command_resolution_reports_missing_install_artifacts() {
        let root = tempfile::tempdir().expect("root");
        let error =
            resolve_command(root.path(), "missing-agent").expect_err("missing command should fail");
        assert!(error.to_string().contains("locate installed command"));
    }

    #[test]
    fn raw_binary_is_written_to_the_declared_command() {
        let root = tempfile::tempdir().expect("root");
        extract(
            b"binary",
            "https://example.com/agent",
            "./bin/agent",
            root.path(),
        )
        .expect("extract raw binary");
        assert_eq!(
            std::fs::read(root.path().join("bin/agent")).expect("read binary"),
            b"binary"
        );

        assert!(
            extract(
                b"binary",
                "https://example.com/agent",
                "../agent",
                root.path(),
            )
            .expect_err("parent traversal")
            .to_string()
            .contains("escapes its installation directory")
        );
        assert!(
            extract(
                b"binary",
                "https://example.com/agent",
                "/absolute/agent",
                root.path(),
            )
            .expect_err("absolute command")
            .to_string()
            .contains("escapes its installation directory")
        );
    }

    #[test]
    fn tar_compressions_extract_nested_commands() {
        for (archive, url, expected) in [
            (
                make_tar_gz("nested/agent", b"gzip binary"),
                "https://example.com/agent.TGZ?download=1",
                b"gzip binary".as_slice(),
            ),
            (
                make_tar_bz2("nested/agent", b"bzip binary"),
                "https://example.com/agent.tbz2",
                b"bzip binary".as_slice(),
            ),
        ] {
            let root = tempfile::tempdir().expect("root");
            extract(&archive, url, "nested/agent", root.path()).expect("extract tar archive");
            assert_eq!(
                std::fs::read(root.path().join("nested/agent")).expect("read command"),
                expected
            );
        }
    }

    #[test]
    fn zip_extracts_nested_files_and_ignores_escaping_entries() {
        let root = tempfile::tempdir().expect("root");
        let traversal_name = format!(
            "{}-escape",
            root.path()
                .file_name()
                .expect("root name")
                .to_string_lossy()
        );
        let outside = root
            .path()
            .parent()
            .expect("root parent")
            .join(&traversal_name);

        extract(
            &make_zip(Some(&traversal_name)),
            "https://example.com/agent.ZIP?download=1",
            "nested/agent",
            root.path(),
        )
        .expect("extract zip");

        let command = root.path().join("nested/agent");
        assert_eq!(
            std::fs::read(&command).expect("read command"),
            b"zip binary"
        );
        assert!(!outside.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(command)
                    .expect("command metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn malformed_zip_reports_the_archive_boundary() {
        let root = tempfile::tempdir().expect("root");
        let error = extract(
            b"not a zip",
            "https://example.com/agent.zip",
            "agent",
            root.path(),
        )
        .expect_err("malformed zip");
        assert!(error.to_string().contains("open zip archive"));
    }

    #[tokio::test]
    async fn install_downloads_validates_reports_progress_and_reuses_the_sentinel() {
        let root = tempfile::tempdir().expect("root");
        let bytes = b"downloaded binary".to_vec();
        let (server_url, server) = serve_once("200 OK", bytes.clone());
        let target = binary_target(
            format!("{server_url}/agent"),
            sha256(&bytes).to_ascii_uppercase(),
            "./bin/agent",
        );
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();

        let (command, args) =
            install_or_resolve_at(root.path(), "example-agent", "1.2.3", &target, progress_tx)
                .await
                .expect("install command");
        server.join().expect("HTTP server");

        assert_eq!(
            command,
            std::fs::canonicalize(root.path().join("example-agent/1.2.3/bin/agent"))
                .expect("canonical command")
        );
        assert_eq!(args, vec!["--stdio"]);
        assert_eq!(
            std::fs::read(&command).expect("read installed command"),
            bytes
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("example-agent/1.2.3/.installed"))
                .expect("read sentinel"),
            "ok"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(
                std::fs::metadata(&command)
                    .expect("command metadata")
                    .permissions()
                    .mode()
                    & 0o111,
                0
            );
        }

        let progress = drain_progress(progress_rx);
        assert!(matches!(
            progress.first(),
            Some(Progress::Started {
                total_bytes: Some(total)
            }) if *total == bytes.len() as u64
        ));
        assert!(matches!(progress.last(), Some(Progress::Done)));
        assert!(progress.len() >= 4);
        assert!(matches!(
            progress.get(progress.len() - 2),
            Some(Progress::Extracting)
        ));
        let downloads = &progress[1..progress.len() - 2];
        assert!(!downloads.is_empty());
        assert!(
            downloads
                .iter()
                .all(|update| matches!(update, Progress::Downloaded { .. }))
        );
        assert!(matches!(
            downloads.last(),
            Some(Progress::Downloaded { downloaded_bytes })
                if *downloaded_bytes == bytes.len() as u64
        ));

        let cached_target = binary_target("not a valid URL", "wrong checksum", "./bin/agent");
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        let (cached_command, cached_args) = install_or_resolve_at(
            root.path(),
            "example-agent",
            "1.2.3",
            &cached_target,
            progress_tx,
        )
        .await
        .expect("reuse cached command");
        assert_eq!(cached_command, command);
        assert_eq!(cached_args, vec!["--stdio"]);
        assert!(matches!(
            drain_progress(progress_rx).as_slice(),
            [Progress::Done]
        ));
    }

    #[tokio::test]
    async fn invalid_install_coordinates_fail_before_creating_directories() {
        let root = tempfile::tempdir().expect("root");
        let target = binary_target("not a valid URL", "", "agent");

        for (agent_id, version, expected) in [
            ("../agent", "1.0.0", "agent id"),
            ("agent", "../version", "version"),
        ] {
            let (progress_tx, _progress_rx) = mpsc::unbounded_channel();
            let error = install_or_resolve_at(root.path(), agent_id, version, &target, progress_tx)
                .await
                .expect_err("invalid install coordinates");
            assert!(error.to_string().contains(expected));
        }
        assert!(
            std::fs::read_dir(root.path())
                .expect("read root")
                .next()
                .is_none()
        );
    }

    #[tokio::test]
    async fn download_rejects_http_errors_and_checksum_mismatches_before_extraction() {
        let root = tempfile::tempdir().expect("root");
        let (server_url, server) = serve_once("404 Not Found", b"missing".to_vec());
        let (progress_tx, _progress_rx) = mpsc::unbounded_channel();
        let error = download_and_extract(
            &binary_target(format!("{server_url}/agent.zip"), "", "agent"),
            root.path(),
            &progress_tx,
        )
        .await
        .expect_err("HTTP status error");
        server.join().expect("HTTP server");
        assert!(format!("{error:#}").contains("404 Not Found"));

        let bytes = b"tampered binary".to_vec();
        let (server_url, server) = serve_once("200 OK", bytes.clone());
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        let error = download_and_extract(
            &binary_target(format!("{server_url}/agent"), "00".repeat(32), "agent"),
            root.path(),
            &progress_tx,
        )
        .await
        .expect_err("checksum mismatch");
        server.join().expect("HTTP server");
        assert!(error.to_string().contains("checksum mismatch"));
        assert!(!root.path().join("agent").exists());
        let progress = drain_progress(progress_rx);
        assert!(matches!(
            progress.first(),
            Some(Progress::Started {
                total_bytes: Some(total)
            }) if *total == bytes.len() as u64
        ));
        assert!(progress.len() >= 2);
        assert!(
            progress[1..]
                .iter()
                .all(|update| matches!(update, Progress::Downloaded { .. }))
        );
        assert!(matches!(
            progress.last(),
            Some(Progress::Downloaded { downloaded_bytes })
                if *downloaded_bytes == bytes.len() as u64
        ));
    }

    #[test]
    fn install_coordinates_reject_path_traversal() {
        assert!(safe_path_component("agent-name"));
        assert!(safe_path_component("1.2.3"));
        assert!(!safe_path_component("../agent"));
        assert!(!safe_path_component("nested/agent"));
        assert!(!safe_path_component("."));
        assert!(!safe_path_component("/agent"));
        assert!(!safe_path_component(""));
    }
}
