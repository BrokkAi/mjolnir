//! Disk-backed downloads. Retrying a GET after daemon replacement is safe;
//! only a complete download can be published or written to stdout.
use super::*;
use mj_checkpoint::archive::{FetchedKind, FetchedPath};
use tokio::io::AsyncWriteExt as _;

impl ApiClient {
    pub(crate) async fn fetch(&self, session: &str, path: &str, list: bool) -> Result<FetchedPath> {
        let mut replacement = None;
        loop {
            let client: &Self = replacement.as_ref().unwrap_or(self);
            let error = match client.fetch_once(session, path, list).await {
                Ok(fetched) => return Ok(fetched),
                Err(error) => error,
            };
            let transport_failed = error.chain().any(|cause| {
                cause.downcast_ref::<reqwest::Error>().is_some_and(|error| {
                    error.is_connect() || error.is_body() || error.is_request() || error.is_decode()
                })
            });
            // GET is repeatable even if the old daemon is still answering its
            // control socket while its HTTP connection shuts down. Checking
            // identity here would race that handoff and lose a safe retry.
            if !is_daemon_handoff(&error) && !transport_failed {
                return Err(error);
            }
            (self.wait_notice)(
                "The daemon connection was interrupted during fetch; reconnecting and retrying the download.",
            );
            replacement = Some(Self::connect_after_handoff().await?);
        }
    }

    async fn fetch_once(&self, session: &str, path: &str, list: bool) -> Result<FetchedPath> {
        let mut request = self
            .http
            .get(self.url(&format!("/sessions/{session}/files")))
            .query(&[("path", path)]);
        if list {
            request = request.query(&[("list", "true")]);
        }
        let mut response = self.send(request).await?;
        let kind = match response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .map(reqwest::header::HeaderValue::as_bytes)
        {
            Some(b"application/octet-stream") => FetchedKind::File,
            Some(b"application/zip") => FetchedKind::Directory,
            Some(b"application/json") if list => FetchedKind::Listing,
            other => bail!(
                "the daemon answered a fetch with content type {:?}",
                other.map(String::from_utf8_lossy)
            ),
        };
        let path = response
            .headers()
            .get(mj_controller::server::api::FETCHED_PATH_HEADER)
            .and_then(|value| mj_controller::server::api::decode_fetched_path(value.as_bytes()))
            .context("the daemon did not say where the path resolved; it predates `mj fetch`")?;
        let expected = response.content_length();
        let file = tokio::task::spawn_blocking(tempfile::tempfile).await??;
        let mut file = tokio::fs::File::from_std(file);
        let mut received = 0u64;
        while let Some(chunk) = response.chunk().await.context("read the fetched path")? {
            file.write_all(&chunk)
                .await
                .context("spool fetched payload")?;
            received += chunk.len() as u64;
        }
        file.flush().await.context("flush fetched payload")?;
        if let Some(expected) = expected {
            anyhow::ensure!(
                received == expected,
                "incomplete fetch download: expected {expected} bytes, received {received}"
            );
        }
        let file = file.into_std().await;
        tokio::task::spawn_blocking(move || FetchedPath::from_file(kind, path, file)).await?
    }
}
