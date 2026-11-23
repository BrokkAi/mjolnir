use super::*;
use futures::TryStreamExt;
use mj_client::daemon::ImportedBundle;
use mj_core::config::{Config, data_dir, sessions_dir};
use mj_core::state::{MaterializedSession, SessionRecord};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::io::StreamReader;

pub(super) struct PreparedImport {
    session: Box<SessionRecord>,
    bundle: Option<ImportedBundle>,
    materialized: MaterializedSession,
    staged: NamedTempFile,
}

pub(super) async fn prepare(body: Body) -> Result<PreparedImport, ApiFailure> {
    let mut reader = StreamReader::new(body.into_data_stream().map_err(std::io::Error::other));
    let header_len =
        reader.read_u32().await.map_err(|error| {
            ApiFailure::bad_request(format!("read import header length: {error}"))
        })? as usize;
    if header_len > MAX_FRAME_BYTES {
        return Err(ApiFailure::bad_request(
            "import header exceeds the daemon frame limit",
        ));
    }
    let mut header = vec![0; header_len];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|error| ApiFailure::bad_request(format!("read import header: {error}")))?;
    let metadata: ImportSessionMetadata = serde_json::from_slice(&header)
        .map_err(|error| ApiFailure::bad_request(format!("decode import header: {error}")))?;
    let ImportSessionMetadata { session, bundle } = metadata;
    if session.id.is_empty()
        || !session
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(ApiFailure::bad_request("invalid imported session id"));
    }

    let (uploaded, descriptor) = mj_core::runtime::spawn_blocking(|| -> AnyResult<_> {
        let root = data_dir().join("uploads");
        std::fs::create_dir_all(&root)?;
        let uploaded = NamedTempFile::new_in(root)?;
        let descriptor = uploaded.reopen().context("open import upload")?;
        Ok((uploaded, descriptor))
    })
    .await
    .context("import upload file task panicked")??;
    let mut file = tokio::fs::File::from_std(descriptor);
    tokio::io::copy(&mut reader, &mut file)
        .await
        .context("stream import upload")?;
    file.flush().await.context("flush import upload")?;
    drop(file);

    let stopped = Arc::new(AtomicBool::new(false));
    let _cancel = CancelImportPreparation(stopped.clone());
    Ok(mj_core::runtime::spawn_blocking(move || -> AnyResult<_> {
        let verified = mj_checkpoint::archive::verify_archive_streaming_cancellable(
            uploaded.path(),
            stopped.clone(),
        )?;
        let checkpoint = session
            .checkpoint
            .as_ref()
            .context("imported session has no checkpoint")?;
        anyhow::ensure!(
            verified.archive_sha256 == checkpoint.sha256,
            "import archive digest mismatch"
        );
        let materialized = mj_transcript::projection::materialized_session_from_canonical(
            session.id.clone(),
            &verified.canonical_session,
        )?;
        let root = sessions_dir();
        std::fs::create_dir_all(&root)?;
        let mut staged = NamedTempFile::new_in(root)?;
        let mut input = uploaded.reopen()?;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            anyhow::ensure!(
                !stopped.load(Ordering::Acquire),
                "import preparation cancelled"
            );
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            staged.write_all(&buffer[..count])?;
        }
        staged
            .as_file()
            .sync_all()
            .context("sync imported archive")?;
        Ok(PreparedImport {
            session,
            bundle,
            materialized,
            staged,
        })
    })
    .await
    .context("import preparation task panicked")??)
}

impl PreparedImport {
    pub(super) fn register(self) -> AnyResult<()> {
        let Self {
            mut session,
            bundle,
            materialized,
            staged,
        } = self;
        let destination = sessions_dir().join(format!("{}.hel.zip", session.id));
        session
            .checkpoint
            .as_mut()
            .context("imported session has no checkpoint")?
            .archive_path = destination.clone();
        let installed = staged
            .persist_noclobber(&destination)
            .map_err(|error| error.error)?;
        let registered = (|| -> AnyResult<()> {
            if let Some(ImportedBundle { bundle_id, bundle }) = bundle {
                Config::update(|config| {
                    if let Some(existing) = config.bundles.get(&bundle_id) {
                        anyhow::ensure!(
                            existing == &bundle,
                            "bundle {bundle_id:?} changed during import; retry the import"
                        );
                    } else {
                        config.bundles.insert(bundle_id, bundle);
                    }
                    Ok(())
                })?;
            }
            mj_store::database::save_imported_session(&session, &materialized)
        })();
        drop(installed);
        if registered.is_err() {
            std::fs::remove_file(&destination).context("remove failed import archive")?;
        }
        registered
    }
}

/// The preparation task owns its files until verification and staging stop.
struct CancelImportPreparation(Arc<AtomicBool>);

impl Drop for CancelImportPreparation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
