//! Verified immutable publication shared by model and native-runtime preparation.
use super::{PreparationFailure, StudentFailureKind};
use anyhow::{Context, Result, ensure};
use futures::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Deserialize)]
pub(super) struct Manifest {
    pub repository: String,
    pub revision: String,
    pub files: Vec<Artifact>,
}
#[derive(Clone, Deserialize)]
pub(super) struct Artifact {
    pub name: String,
    pub sha256: String,
    pub bytes: u64,
}
impl Manifest {
    pub fn selected(&self, coreml: bool) -> Self {
        let mut selected = self.clone();
        selected.files.retain(|f| {
            coreml || !(f.name.starts_with("model-coreml-") || f.name.starts_with("initializers-"))
        });
        if coreml {
            selected.revision.push_str("-coreml");
        }
        selected
    }
    pub fn pinned() -> &'static Self {
        static MANIFEST: std::sync::OnceLock<Manifest> = std::sync::OnceLock::new();
        MANIFEST.get_or_init(|| {
            serde_json::from_str(include_str!("manifest.json")).expect("checked-in model manifest")
        })
    }
}
pub(super) fn verify_file(path: &Path, bytes: u64, sha256: &str) -> Result<()> {
    use std::io::Read;
    let mut input = std::fs::File::open(path)?;
    ensure!(
        input.metadata()?.len() == bytes,
        "{}: wrong size",
        path.display()
    );
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    ensure!(
        mj_core::hex::lower_hex(hasher.finalize()) == sha256,
        "{}: wrong SHA-256",
        path.display()
    );
    Ok(())
}
pub(super) fn verified(path: &Path, files: &[Artifact]) -> Result<()> {
    for file in files {
        verify_file(&path.join(&file.name), file.bytes, &file.sha256)?;
    }
    Ok(())
}
/// Cleanup also runs off the event loop, including cancellation between awaits.
pub(super) struct Staging(Option<tempfile::TempDir>, tokio::runtime::Handle);
impl Staging {
    pub async fn new(root: PathBuf) -> Result<Self> {
        tokio::fs::create_dir_all(&root).await?;
        let directory = tokio::task::spawn_blocking(move || {
            tempfile::Builder::new()
                .prefix(".student-")
                .tempdir_in(root)
        })
        .await??;
        Ok(Self(Some(directory), tokio::runtime::Handle::current()))
    }
    pub fn path(&self) -> &Path {
        self.0.as_ref().expect("staging directory consumed").path()
    }
    /// Must run on a blocking task. One OS lock serializes the publication
    /// decision across daemon replacement, including a cancelled old preparer.
    pub fn publish(mut self, destination: PathBuf, files: &[Artifact]) -> Result<PathBuf> {
        verified(self.path(), files)
            .map_err(|e| PreparationFailure::error(StudentFailureKind::Verification, e, None))?;
        let root = destination
            .parent()
            .context("cache destination has no parent")?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join(".publication.lock"))?;
        lock.lock()?;
        let _lock = mj_core::file_lock::FileLock::new(lock);
        if verified(&destination, files).is_ok() {
            return Ok(destination);
        }
        if destination.exists() {
            std::fs::remove_dir_all(&destination)?;
        }
        std::fs::rename(self.path(), &destination)?;
        drop(self.0.take().expect("staging directory consumed").keep());
        Ok(destination)
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        if let Some(directory) = self.0.take() {
            drop(self.1.spawn_blocking(move || {
                if let Err(error) = directory.close() {
                    tracing::warn!(%error, "student staging cleanup failed");
                }
            }));
        }
    }
}
pub(super) async fn download(
    url: &str,
    path: &Path,
    client: &reqwest::Client,
    cancellation: &CancellationToken,
) -> Result<()> {
    let response = tokio::select! {
        _ = cancellation.cancelled() => anyhow::bail!("student preparation cancelled"),
        response = client.get(url).send() => response?,
    };
    let retry_after = crate::quota::retry_after_header(response.headers());
    let response = response
        .error_for_status()
        .map_err(|e| PreparationFailure::error(StudentFailureKind::Download, e, retry_after))?;
    let mut stream = response.bytes_stream();
    let mut output = tokio::fs::File::create(path).await?;
    loop {
        let chunk = tokio::select! {
            _ = cancellation.cancelled() => anyhow::bail!("student preparation cancelled"),
            chunk = stream.next() => chunk,
        };
        let Some(chunk) = chunk else {
            break;
        };
        output.write_all(&chunk?).await?;
    }
    output.sync_all().await?;
    Ok(())
}
fn root_for_reuse(destination: &Path, name: &str) -> PathBuf {
    let revision = destination.file_name().expect("revision").to_string_lossy();
    if let Some(base) = revision.strip_suffix("-coreml") {
        destination.with_file_name(base).join(name)
    } else {
        destination.join(name)
    }
}
/// Only the owner calls preparation; pair classification observes Loading.
pub(super) async fn prepare(
    root: PathBuf,
    manifest: Manifest,
    base: &str,
    client: &reqwest::Client,
    cancellation: &CancellationToken,
) -> Result<PathBuf> {
    let destination = root.join(&manifest.revision);
    let check = destination.clone();
    let expected = manifest.files.clone();
    if tokio::task::spawn_blocking(move || verified(&check, &expected))
        .await?
        .is_ok()
    {
        return Ok(destination);
    }
    let staging = Staging::new(root).await?;
    futures::future::try_join_all(manifest.files.iter().map(|artifact| {
        let url = format!(
            "{base}/{}/resolve/{}/{}",
            manifest.repository,
            manifest.revision.trim_end_matches("-coreml"),
            artifact.name
        );
        let path = staging.path().join(&artifact.name);
        let existing = root_for_reuse(&destination, &artifact.name);
        async move {
            let reuse = artifact.clone();
            let to = path.clone();
            let copied = tokio::task::spawn_blocking(move || -> Result<()> {
                verify_file(&existing, reuse.bytes, &reuse.sha256)?;
                std::fs::copy(existing, to)?;
                Ok(())
            })
            .await?;
            if copied.is_ok() {
                Ok(())
            } else {
                download(&url, &path, client, cancellation).await
            }
        }
    }))
    .await?;
    tokio::task::spawn_blocking(move || staging.publish(destination, &manifest.files)).await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, extract::State, response::Response, routing::get};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    struct Source {
        bytes: Vec<u8>,
        interrupt: AtomicBool,
        corrupt: AtomicBool,
        requests: AtomicUsize,
    }
    struct Server {
        url: String,
        stop: CancellationToken,
        task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    }
    impl Server {
        async fn start(source: Arc<Source>) -> Self {
            async fn serve(State(source): State<Arc<Source>>) -> Response {
                source.requests.fetch_add(1, Ordering::SeqCst);
                let length = source.bytes.len();
                let body = if source.interrupt.swap(false, Ordering::SeqCst) {
                    // Cross the64KiB boundary before the peer aborts the body.
                    Body::from_stream(futures::stream::iter([
                        Ok(source.bytes[..100_000].to_vec()),
                        Err(std::io::Error::other("recorded interrupted transfer")),
                    ]))
                } else {
                    let mut bytes = source.bytes.clone();
                    if source.corrupt.load(Ordering::SeqCst) {
                        bytes[0] ^= 1;
                    }
                    Body::from(bytes)
                };
                Response::builder()
                    .header("content-length", length)
                    .body(body)
                    .unwrap()
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let stop = CancellationToken::new();
            let shutdown = stop.clone();
            let task = tokio::spawn(async move {
                let result = axum::serve(
                    listener,
                    Router::new()
                        .route("/{*path}", get(serve))
                        .with_state(source),
                )
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await;
                if let Err(error) = &result {
                    tracing::error!(%error,"student fixture HTTP server failed");
                }
                result
            });
            Self {
                url,
                stop,
                task: Some(task),
            }
        }
        async fn close(mut self) {
            self.stop.cancel();
            self.task.take().unwrap().await.unwrap().unwrap();
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.cancel();
        }
    }
    fn source() -> Arc<Source> {
        Arc::new(Source {
            bytes: (0..300_000).map(|i| (i % 251) as u8).collect(),
            interrupt: AtomicBool::new(false),
            corrupt: AtomicBool::new(false),
            requests: AtomicUsize::new(0),
        })
    }
    fn manifest(source: &Source) -> Manifest {
        Manifest {
            repository: "fixture".into(),
            revision: "immutable".into(),
            files: vec![Artifact {
                name: "model".into(),
                bytes: source.bytes.len() as u64,
                sha256: mj_core::hex::lower_hex(Sha256::digest(&source.bytes)),
            }],
        }
    }
    #[tokio::test]
    async fn interrupted_download_never_publishes_and_restart_recovers() {
        let source = source();
        source.interrupt.store(true, Ordering::SeqCst);
        let server = Server::start(source.clone()).await;
        let root = tempfile::tempdir().unwrap();
        let client = reqwest::Client::new();
        let cancel = CancellationToken::new();
        assert!(
            prepare(
                root.path().into(),
                manifest(&source),
                &server.url,
                &client,
                &cancel
            )
            .await
            .is_err()
        );
        assert!(!root.path().join("immutable").exists());
        let path = prepare(
            root.path().into(),
            manifest(&source),
            &server.url,
            &client,
            &cancel,
        )
        .await
        .unwrap();
        verified(&path, &manifest(&source).files).unwrap();
        assert_eq!(source.requests.load(Ordering::SeqCst), 2);
        server.close().await;
    }
    #[tokio::test]
    async fn bad_digest_is_a_verification_failure_and_not_a_cache_entry() {
        let source = source();
        source.corrupt.store(true, Ordering::SeqCst);
        let server = Server::start(source.clone()).await;
        let root = tempfile::tempdir().unwrap();
        let client = reqwest::Client::new();
        let error = prepare(
            root.path().into(),
            manifest(&source),
            &server.url,
            &client,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<PreparationFailure>().unwrap().kind,
            StudentFailureKind::Verification
        );
        assert!(!root.path().join("immutable").exists());
        source.corrupt.store(false, Ordering::SeqCst);
        let path = prepare(
            root.path().into(),
            manifest(&source),
            &server.url,
            &client,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        verified(&path, &manifest(&source).files).unwrap();
        server.close().await;
    }
    #[tokio::test]
    async fn publication_serializes_old_and_new_preparers_across_handoff() {
        let source = source();
        let root = tempfile::tempdir().unwrap();
        let files = manifest(&source).files;
        let destination = root.path().join("immutable");
        // This bypasses the owner deliberately to simulate two daemon epochs.
        let first = Staging::new(root.path().into()).await.unwrap();
        let second = Staging::new(root.path().into()).await.unwrap();
        tokio::fs::write(first.path().join("model"), &source.bytes)
            .await
            .unwrap();
        tokio::fs::write(second.path().join("model"), &source.bytes)
            .await
            .unwrap();
        let a = destination.clone();
        let f = files.clone();
        let left = tokio::task::spawn_blocking(move || first.publish(a, &f));
        let b = destination.clone();
        let f = files.clone();
        let right = tokio::task::spawn_blocking(move || second.publish(b, &f));
        left.await.unwrap().unwrap();
        right.await.unwrap().unwrap();
        verified(&destination, &files).unwrap();
    }
}
