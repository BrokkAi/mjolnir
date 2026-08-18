//! Bounded download and archive extraction used by runtime bootstrap helpers.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;

const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;

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
    let mut bytes = download_buffer(total_bytes, url, MAX_ARCHIVE_BYTES)?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("read chunk from {url}"))?;
        append_download_chunk(&mut bytes, &chunk, url, MAX_ARCHIVE_BYTES)?;
    }
    let kind = ArchiveKind::from_url(url);
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || extract(&bytes, kind, &dest))
        .await
        .context("join archive extraction")??;
    Ok(())
}

fn download_buffer(total_bytes: Option<u64>, url: &str, max_bytes: usize) -> Result<Vec<u8>> {
    if let Some(total_bytes) = total_bytes {
        anyhow::ensure!(
            total_bytes <= max_bytes as u64,
            "archive from {url} is too large ({total_bytes} bytes; max {max_bytes})"
        );
    }
    Ok(Vec::with_capacity(total_bytes.unwrap_or(0) as usize))
}

fn append_download_chunk(
    bytes: &mut Vec<u8>,
    chunk: &[u8],
    url: &str,
    max_bytes: usize,
) -> Result<()> {
    anyhow::ensure!(
        bytes.len().saturating_add(chunk.len()) <= max_bytes,
        "archive from {url} exceeds maximum size of {max_bytes} bytes"
    );
    bytes.extend_from_slice(chunk);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    TarGz,
    Zip,
}

impl ArchiveKind {
    fn from_url(url: &str) -> Self {
        let path = url.split(['?', '#']).next().unwrap_or(url);
        if path.to_ascii_lowercase().ends_with(".zip") {
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

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::thread;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use zip::write::SimpleFileOptions;

    use super::*;

    fn make_tar_gz(file_name: &str, content: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.set_path(file_name).expect("tar path");
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();

        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            builder.append(&header, content).expect("append tar file");
            builder.finish().expect("finish tar");
        }

        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        gzip.write_all(&tar_bytes).expect("write gzip");
        gzip.finish().expect("finish gzip")
    }

    fn make_zip(traversal_name: Option<&str>) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        writer
            .add_directory("nested/", SimpleFileOptions::default())
            .expect("add directory");
        writer
            .start_file(
                "nested/tool",
                SimpleFileOptions::default().unix_permissions(0o755),
            )
            .expect("start nested file");
        writer
            .write_all(b"binary bytes")
            .expect("write nested file");
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

    #[test]
    fn archive_kind_uses_case_insensitive_url_path_extension() {
        assert_eq!(
            ArchiveKind::from_url("https://example.test/release.ZIP?download=1#asset"),
            ArchiveKind::Zip
        );
        assert_eq!(
            ArchiveKind::from_url("https://example.test/release.tar.gz?download=1"),
            ArchiveKind::TarGz
        );
        assert_eq!(
            ArchiveKind::from_url("https://example.test/no-extension"),
            ArchiveKind::TarGz
        );
    }

    #[test]
    fn zip_extracts_nested_files_preserves_mode_and_rejects_traversal() {
        let destination = tempfile::tempdir().expect("destination");
        let traversal_name = format!(
            "{}-escape",
            destination
                .path()
                .file_name()
                .expect("destination name")
                .to_string_lossy()
        );
        let outside = destination
            .path()
            .parent()
            .expect("destination parent")
            .join(&traversal_name);

        extract(
            &make_zip(Some(&traversal_name)),
            ArchiveKind::Zip,
            destination.path(),
        )
        .expect("extract zip");

        let output = destination.path().join("nested/tool");
        assert_eq!(
            std::fs::read(&output).expect("read output"),
            b"binary bytes"
        );
        assert!(!outside.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(output)
                    .expect("output metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn tar_gz_extracts_nested_file() {
        let destination = tempfile::tempdir().expect("destination");
        let archive = make_tar_gz("nested/tool", b"tar bytes");

        extract(&archive, ArchiveKind::TarGz, destination.path()).expect("extract tar.gz");

        assert_eq!(
            std::fs::read(destination.path().join("nested/tool")).expect("read output"),
            b"tar bytes"
        );
    }

    #[test]
    fn malformed_archives_report_the_extraction_boundary() {
        let destination = tempfile::tempdir().expect("destination");
        assert!(
            extract(b"not a zip", ArchiveKind::Zip, destination.path())
                .expect_err("invalid zip")
                .to_string()
                .contains("open zip archive")
        );
        assert!(
            extract(b"not a tarball", ArchiveKind::TarGz, destination.path())
                .expect_err("invalid tar.gz")
                .to_string()
                .contains("unpack tar.gz")
        );
    }

    #[tokio::test]
    async fn download_and_extract_handles_zip_urls_with_queries() {
        let destination = tempfile::tempdir().expect("destination");
        let (server_url, server) = serve_once("200 OK", make_zip(None));

        let result = download_and_extract(
            &format!("{server_url}/release.ZIP?download=1"),
            destination.path(),
        )
        .await;
        server.join().expect("HTTP server");
        result.expect("download and extract");

        assert_eq!(
            std::fs::read(destination.path().join("nested/tool")).expect("read output"),
            b"binary bytes"
        );
    }

    #[tokio::test]
    async fn download_and_extract_reports_http_status() {
        let destination = tempfile::tempdir().expect("destination");
        let (server_url, server) = serve_once("404 Not Found", b"missing".to_vec());

        let error = download_and_extract(&format!("{server_url}/release.zip"), destination.path())
            .await
            .expect_err("HTTP status error");
        server.join().expect("HTTP server");

        assert!(error.to_string().contains("HTTP 404 Not Found"));
        assert!(!destination.path().join("nested/tool").exists());
    }

    #[test]
    fn download_size_limit_rejects_declared_and_streamed_overflow() {
        let declared = download_buffer(Some(5), "https://example.test/archive.zip", 4)
            .expect_err("declared size limit");
        assert!(declared.to_string().contains("5 bytes; max 4"));

        let mut bytes = download_buffer(Some(3), "https://example.test/archive.zip", 4)
            .expect("within declared limit");
        append_download_chunk(&mut bytes, b"123", "https://example.test/archive.zip", 4)
            .expect("first chunk");
        let streamed =
            append_download_chunk(&mut bytes, b"45", "https://example.test/archive.zip", 4)
                .expect_err("streamed size limit");
        assert!(streamed.to_string().contains("maximum size of 4 bytes"));
        assert_eq!(bytes, b"123");
    }
}
