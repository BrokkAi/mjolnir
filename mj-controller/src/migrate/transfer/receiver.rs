//! The destination side of `mj migrate`: stage incoming files, then publish
//! them under B's locks with one SQLite commit as the activation.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use futures::future::BoxFuture;
use mj_core::config::{ConfigLayout, DataLayout};
use mj_core::file_lock::FileLock;
use mj_store::database::{StoreFenceRecord, clear_store_fence_file, read_store_fence_file};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use super::super::protocol::*;
use super::super::rewrite::rewrite_staged_config_and_files;
use super::fsops::*;
use crate::controller::ControllerStoreGuard;

/// Where publication can be made to fail, for crash-recovery tests. A failure
/// returned from the hook leaves the staged and published files exactly as a
/// crash at that point would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationStep {
    /// The rewritten store is complete in the raw staging directory.
    BeforeStoreRename,
    /// Everything else is published; the store is still staged.
    BeforeStorePlacement,
    /// The fenced store is in its final place.
    BeforeUnfence,
}

pub type PublicationHook = Arc<dyn Fn(PublicationStep) -> Result<()> + Send + Sync>;
pub type StartDaemon = Arc<dyn Fn() -> BoxFuture<'static, Result<()>> + Send + Sync>;

pub struct ReceiverConfig {
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub home_dir: PathBuf,
    pub sessionwiki_db: PathBuf,
    /// Start B's daemon while the start lock is still held.
    pub start_daemon: Option<StartDaemon>,
    pub hook: Option<PublicationHook>,
}

const FORBIDDEN_DATA_NAMES: &[&str] =
    &["controller.lock", "daemon-start.lock", ".mj-migrate-stage"];
const MAIN_DB: &str = "mj.sqlite3";
const MAIN_WAL: &str = "mj.sqlite3-wal";
const MAIN_SHM: &str = "mj.sqlite3-shm";
const WIKI_DB: &str = "index.db";
const WIKI_WAL: &str = "index.db-wal";
const WIKI_SHM: &str = "index.db-shm";
const PUBLICATION_FILE: &str = "publication.json";

/// The device and inode of a path this publication created. A path that now
/// names a different inode was replaced by someone else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

impl FileIdentity {
    fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(Self {
                dev: metadata.dev(),
                ino: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct OwnedEntry {
    path: PathBuf,
    #[serde(default)]
    identity: Option<FileIdentity>,
}

/// What this publication has proven it created on the destination. A
/// rolled-back attempt removes exactly these entries, and only while each
/// still is the inode that was created, so a file or directory that was already
/// there, or that replaced ours, is never deleted. An entry is recorded right
/// after its atomic creation, never before.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PublicationRecord {
    expected_fence: StoreFenceRecord,
    /// Entries created under the home directory, relative to it.
    #[serde(default)]
    owned_home_targets: Vec<OwnedEntry>,
    /// Top-level entries created in the data directory.
    #[serde(default)]
    owned_data_entries: Vec<OwnedEntry>,
    /// Top-level entries created in the configuration directory.
    #[serde(default)]
    owned_config_entries: Vec<OwnedEntry>,
    /// The migrated SessionWiki index is installed beside the destination's
    /// index path and renamed into place only after the activation commit, so
    /// nothing can open it, and nothing of the destination's is touched, before.
    #[serde(default)]
    sessionwiki_pending: bool,
    #[serde(default)]
    sessionwiki_pending_identity: Option<FileIdentity>,
    /// Set once the fenced store is in its final place.
    #[serde(default)]
    final_db_placed: bool,
    /// The inode of the store this publication placed. An unfenced
    /// `mj.sqlite3` is "already activated" only if it is this one.
    #[serde(default)]
    final_db_identity: Option<FileIdentity>,
}

#[derive(Clone, Copy)]
enum Owned {
    Home,
    Data,
    Config,
}

impl PublicationRecord {
    fn list(&mut self, which: Owned) -> &mut Vec<OwnedEntry> {
        match which {
            Owned::Home => &mut self.owned_home_targets,
            Owned::Data => &mut self.owned_data_entries,
            Owned::Config => &mut self.owned_config_entries,
        }
    }

    fn owns(&self, which: Owned, name: &Path) -> bool {
        match which {
            Owned::Home => &self.owned_home_targets,
            Owned::Data => &self.owned_data_entries,
            Owned::Config => &self.owned_config_entries,
        }
        .iter()
        .any(|entry| entry.path == name)
    }
}

/// Refuse a path that no longer is what this publication created.
fn verify_owned(dst: &Path, entry: &OwnedEntry, found: &std::fs::Metadata) -> Result<()> {
    if let Some(expected) = entry.identity
        && FileIdentity::of(found).is_some_and(|actual| actual != expected)
    {
        bail!(
            "destination path {} was replaced externally since the interrupted migration; \
             clean it manually or choose --dest-instance",
            dst.display()
        );
    }
    Ok(())
}

/// Install one entry. One this publication already created is finished
/// (merged into if a directory), after checking it is still the one created.
/// Anything else must not exist: it is created atomically and recorded as
/// ours, with its identity, before more is placed.
fn install_entry(
    stage: &Stage,
    record: &mut PublicationRecord,
    which: Owned,
    name: &Path,
    src: &Path,
    dst: &Path,
    mode: PublishMode,
) -> Result<()> {
    if let Some(position) = record
        .list(which)
        .iter()
        .position(|owned| owned.path == name)
    {
        let entry = record.list(which)[position].clone();
        match symlink_metadata_opt(dst)? {
            Some(found) => {
                verify_owned(dst, &entry, &found)?;
                if found.is_dir() {
                    return publish_entry(src, dst, mode);
                }
                // A file or link is complete when it is recorded; only the
                // staged source of a move can be left over.
                if mode == PublishMode::Move {
                    remove_path(src)?;
                }
                return Ok(());
            }
            None => {
                // Removed since; install it afresh under a new identity.
                record.list(which).remove(position);
            }
        }
    }
    publish_entry_no_replace(src, dst, mode, &mut || {
        let identity = symlink_metadata_opt(dst)?
            .as_ref()
            .and_then(FileIdentity::of);
        record.list(which).push(OwnedEntry {
            path: name.to_path_buf(),
            identity,
        });
        stage.write_publication(record)
    })
}

/// The top-level names of `raw` that publication moves or copies on.
fn publishable_names(raw: &Path, skip: impl Fn(&str) -> bool) -> Result<Vec<PathBuf>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(raw)? {
        let name = entry?.file_name();
        if !skip(&name.to_string_lossy()) {
            names.push(PathBuf::from(name));
        }
    }
    Ok(names)
}

fn skipped_data_name(name: &str) -> bool {
    matches!(name, MAIN_DB | MAIN_WAL | MAIN_SHM) || FORBIDDEN_DATA_NAMES.contains(&name)
}

fn skipped_config_name(name: &str) -> bool {
    name == "config.toml"
        || matches!(
            mj_core::config::classify_config_entry(Path::new(name)),
            mj_core::config::Classification::Known(mj_core::config::MigrateAction::Skip)
        )
}

fn single_normal_name(path: &Path) -> Result<()> {
    validate_relative_path(path)?;
    ensure!(
        path.components().count() == 1,
        "recorded entry is not a single name: {}",
        path.display()
    );
    Ok(())
}

/// A source directory whose mode lacks owner access, and so could not be filled.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DirectoryMode {
    scope: TransferScope,
    rel_path: PathBuf,
    mode: u32,
}

#[derive(Clone)]
struct Stage {
    root: PathBuf,
}

impl Stage {
    fn new(data_dir: &Path) -> Self {
        Self {
            root: DataLayout::at(data_dir).migrate_stage(),
        }
    }
    fn raw(&self, scope: TransferScope) -> PathBuf {
        self.root.join("raw").join(match scope {
            TransferScope::Data => "data",
            TransferScope::Config => "config",
            TransferScope::SessionWiki => "sessionwiki",
            TransferScope::Home => "home",
        })
    }
    fn rewritten(&self) -> PathBuf {
        self.root.join("rewritten")
    }
    fn publication(&self) -> PathBuf {
        self.root.join(PUBLICATION_FILE)
    }
    fn stage_id_file(&self) -> PathBuf {
        self.root.join("stage-id")
    }
    fn read_stage_id(&self) -> Result<Option<String>> {
        match std::fs::read_to_string(self.stage_id_file()) {
            Ok(id) => Ok(Some(id.trim().to_owned())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("read migration stage identity"),
        }
    }
    fn ensure_stage_id(&self) -> Result<String> {
        if let Some(id) = self.read_stage_id()?.filter(|id| !id.is_empty()) {
            return Ok(id);
        }
        let id = mj_core::state::new_session_id()?;
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(id.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(self.stage_id_file())
            .context("persist migration stage identity")?;
        sync_directory(&self.root)?;
        Ok(id)
    }
    fn create(&self) -> Result<()> {
        mj_core::config::create_owner_only_directory(&self.rewritten())?;
        for scope in [
            TransferScope::Data,
            TransferScope::Config,
            TransferScope::SessionWiki,
            TransferScope::Home,
        ] {
            mj_core::config::create_owner_only_directory(&self.raw(scope))?;
        }
        self.ensure_stage_id()?;
        Ok(())
    }
    fn dir_modes_file(&self) -> PathBuf {
        self.root.join("dir-modes.json")
    }
    fn read_directory_modes(&self) -> Result<Vec<DirectoryMode>> {
        match std::fs::read(self.dir_modes_file()) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("parse dir-modes.json"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error).context("read dir-modes.json"),
        }
    }
    /// Remember the mode a source directory really has when it is not owner
    /// read-write-search, so publication can restore it after filling the directory.
    fn record_directory_mode(
        &self,
        scope: TransferScope,
        rel_path: &Path,
        mode: u32,
    ) -> Result<()> {
        let mut modes = self.read_directory_modes()?;
        let before = modes.len();
        modes.retain(|entry| !(entry.scope == scope && entry.rel_path == rel_path));
        let restricted = mode & 0o700 != 0o700;
        if !restricted && modes.len() == before {
            return Ok(());
        }
        if restricted {
            modes.push(DirectoryMode {
                scope,
                rel_path: rel_path.to_path_buf(),
                mode,
            });
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&serde_json::to_vec(&modes)?)?;
        file.as_file().sync_all()?;
        file.persist(self.dir_modes_file())
            .context("persist dir-modes.json")?;
        Ok(())
    }
    /// Forget recorded modes at or under a path that was removed or replaced,
    /// so a stale entry can never be applied to whatever is there later.
    fn prune_directory_modes(&self, scope: TransferScope, rel_path: &Path) -> Result<()> {
        let mut modes = self.read_directory_modes()?;
        let before = modes.len();
        modes.retain(|entry| !(entry.scope == scope && entry.rel_path.starts_with(rel_path)));
        if modes.len() == before {
            return Ok(());
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&serde_json::to_vec(&modes)?)?;
        file.as_file().sync_all()?;
        file.persist(self.dir_modes_file())
            .context("persist dir-modes.json")?;
        Ok(())
    }
    fn read_publication(&self) -> Result<Option<PublicationRecord>> {
        match std::fs::read(self.publication()) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).context("parse publication.json")?,
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("read publication.json"),
        }
    }
    /// Durable before any publication step, so a restart can tell its own
    /// earlier work from files that were already on the destination.
    fn write_publication(&self, record: &PublicationRecord) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&serde_json::to_vec_pretty(record)?)?;
        file.as_file().sync_all()?;
        file.persist(self.publication())
            .context("persist publication.json")?;
        sync_directory(&self.root)
    }
}

struct PutState {
    scope: TransferScope,
    rel_path: PathBuf,
    temporary: tempfile::NamedTempFile,
    destination: PathBuf,
    hasher: Sha256,
    mode: u32,
    mtime_ns: i64,
}

struct PatchState {
    file: std::fs::File,
    total_size: u64,
}

struct Receiver {
    config: Arc<ReceiverConfig>,
    stage: Stage,
    /// Set by `Hello`; `None` until then, `Some(Err)` when the destination refuses.
    ready: Option<std::result::Result<(), String>>,
    put: Option<PutState>,
    patch: Option<PatchState>,
    /// Held from the first staging request until publication has removed the
    /// staging directory: one receiver at a time owns a destination's staging.
    stage_lock: Option<FileLock>,
}

type Reply = Vec<(Response, Vec<u8>)>;

/// Serve one migration stream until `Done` or the sender disconnects.
pub async fn run_receiver<R, W>(config: ReceiverConfig, reader: R, writer: W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    mj_core::config::ensure_may_control_store(&config.data_dir, "receive a Mjolnir migration")?;
    let mut reader = tokio::io::BufReader::with_capacity(256 * 1024, reader);
    let mut writer = tokio::io::BufWriter::new(writer);
    let stage = Stage::new(&config.data_dir);
    let mut receiver = Receiver {
        config: Arc::new(config),
        stage,
        ready: None,
        put: None,
        patch: None,
        stage_lock: None,
    };
    loop {
        let Some((request, payload)) = read_frame::<_, Request>(&mut reader).await? else {
            tracing::info!("migration sender disconnected");
            return Ok(());
        };
        let done = matches!(request, Request::Done);
        match receiver.handle(request, payload).await {
            Ok(replies) => {
                for (response, payload) in replies {
                    write_frame(&mut writer, &response, &payload).await?;
                }
                writer.flush().await?;
                if done {
                    return Ok(());
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                // The sender may already be gone; the error is returned either way.
                if write_frame(&mut writer, &Response::Error { message }, &[])
                    .await
                    .is_ok()
                {
                    let _ = writer.flush().await;
                }
                return Err(error);
            }
        }
    }
}

fn ack() -> Reply {
    vec![(Response::Ack, Vec::new())]
}

/// Size of regular files only, without following symlinks.
fn staged_file_bytes(path: &Path) -> Result<u64> {
    let Some(metadata) = symlink_metadata_opt(path)? else {
        return Ok(0);
    };
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mut total = 0_u64;
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", path.display()))?;
        total = total
            .checked_add(staged_file_bytes(&entry.path())?)
            .context("staged file sizes exceed u64")?;
    }
    Ok(total)
}

fn staged_sizes(config: &ReceiverConfig, stage: &Stage) -> Result<(u64, u64, u64)> {
    let Some(root) = symlink_metadata_opt(&stage.root)? else {
        return Ok((0, 0, 0));
    };
    if !root.is_dir() {
        return Ok((0, 0, 0));
    }
    let raw = stage.root.join("raw");
    let raw_is_dir = symlink_metadata_opt(&raw)?.is_some_and(|metadata| metadata.is_dir());
    let size = |scope| {
        if raw_is_dir {
            staged_file_bytes(&stage.raw(scope))
        } else {
            Ok(0)
        }
    };
    let rewritten = staged_file_bytes(&stage.rewritten())?;
    let data = size(TransferScope::Data)?
        .checked_add(size(TransferScope::Home)?)
        .and_then(|total| total.checked_add(rewritten))
        .context("staged data sizes exceed u64")?;
    let config_bytes = size(TransferScope::Config)?;
    let wiki = size(TransferScope::SessionWiki)?
        .checked_add(staged_file_bytes(&sessionwiki_pending_path(
            &config.sessionwiki_db,
        ))?)
        .context("staged SessionWiki sizes exceed u64")?;
    Ok((data, config_bytes, wiki))
}

impl Receiver {
    fn require_ready(&self) -> Result<()> {
        match &self.ready {
            Some(Ok(())) => Ok(()),
            Some(Err(reason)) => bail!("{reason}"),
            None => bail!("the sender must say Hello first"),
        }
    }

    async fn handle(&mut self, request: Request, payload: Vec<u8>) -> Result<Reply> {
        match request {
            Request::Hello(hello) => self.hello(hello).await,
            Request::ProbePaths { paths } => {
                let mut exists_map = BTreeMap::new();
                let publication = self.stage.read_publication()?;
                for path in paths {
                    ensure!(
                        path.is_absolute(),
                        "probe path must be absolute: {}",
                        path.display()
                    );
                    let metadata = symlink_metadata_opt(&path)?;
                    let owned = publication.as_ref().is_some_and(|record| {
                        let identity = metadata.as_ref().and_then(FileIdentity::of);
                        if record.final_db_placed
                            && path == self.config.data_dir.join(MAIN_DB)
                            && identity.is_some()
                            && identity == record.final_db_identity
                        {
                            return true;
                        }
                        [
                            (&self.config.home_dir, &record.owned_home_targets),
                            (&self.config.data_dir, &record.owned_data_entries),
                            (&self.config.config_dir, &record.owned_config_entries),
                        ]
                        .into_iter()
                        .any(|(root, entries)| {
                            entries.iter().any(|entry| {
                                root.join(&entry.path) == path
                                    && identity.is_some()
                                    && identity == entry.identity
                                    && metadata.as_ref().is_some_and(|found| {
                                        verify_owned(&path, entry, found).is_ok()
                                    })
                            })
                        })
                    });
                    exists_map.insert(path.clone(), metadata.is_some() && !owned);
                }
                Ok(vec![(
                    Response::ProbePathsAck { exists: exists_map },
                    Vec::new(),
                )])
            }
            Request::ProbeSsh(request) => {
                let ack = mj_core::runtime::spawn_blocking(move || probe_ssh(&request))
                    .await
                    .context("join SSH probe")?;
                Ok(vec![(Response::ProbeSshAck(ack), Vec::new())])
            }
            Request::StageManifest {
                files,
                hash_files,
                blocks,
            } => {
                self.ensure_staging_locked()?;
                let stage = self.stage.clone();
                mj_core::runtime::spawn_blocking(move || {
                    stage_manifest(&stage, files, hash_files, &blocks)
                })
                .await
                .context("join manifest scan")?
            }
            Request::PutFileStart {
                scope,
                rel_path,
                kind,
            } => {
                self.ensure_staging_locked()?;
                self.put_start(scope, &rel_path, kind)
            }
            Request::PutFileChunk { raw_len } => {
                let state = self
                    .put
                    .as_mut()
                    .context("PutFileChunk without PutFileStart")?;
                let raw = decompress(&payload, raw_len)?;
                state.hasher.update(&raw);
                state.temporary.write_all(&raw)?;
                Ok(Vec::new())
            }
            Request::PutFileEnd { sha256 } => self.put_end(&sha256),
            Request::PatchBlocksBegin { target, total_size } => {
                self.ensure_staging_locked()?;
                ensure!(self.patch.is_none(), "PatchBlocks is already open");
                let (scope, name) = match target {
                    DbTarget::Main => (TransferScope::Data, MAIN_DB),
                    DbTarget::SessionWiki => (TransferScope::SessionWiki, WIKI_DB),
                };
                let path = self.stage.raw(scope).join(name);
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(&path)
                    .with_context(|| format!("open {}", path.display()))?;
                self.patch = Some(PatchState { file, total_size });
                Ok(Vec::new())
            }
            Request::PatchBlock {
                index,
                raw_len,
                sha256,
            } => {
                let state = self
                    .patch
                    .as_mut()
                    .context("PatchBlock without PatchBlocksBegin")?;
                let raw = decompress(&payload, raw_len)?;
                ensure!(
                    sha256_hex(&raw) == sha256,
                    "block {index} failed its SHA-256 check"
                );
                let offset = index
                    .checked_mul(BLOCK_BYTES as u64)
                    .context("block index overflows")?;
                ensure!(
                    offset + raw.len() as u64 <= state.total_size,
                    "block {index} lies beyond the announced size"
                );
                state.file.seek(SeekFrom::Start(offset))?;
                state.file.write_all(&raw)?;
                Ok(Vec::new())
            }
            Request::PatchBlocksEnd => {
                let state = self
                    .patch
                    .take()
                    .context("PatchBlocksEnd without PatchBlocksBegin")?;
                state.file.set_len(state.total_size)?;
                state.file.sync_all()?;
                Ok(ack())
            }
            Request::RemoveFile { scope, rel_path } => {
                self.ensure_staging_locked()?;
                validate_scope_path(scope, &rel_path, false)?;
                if let Some(path) = resolve_in_scope(&self.stage.raw(scope), &rel_path, false)? {
                    remove_path(&path)?;
                }
                self.stage.prune_directory_modes(scope, &rel_path)?;
                Ok(ack())
            }
            Request::CommitPublication(request) => {
                self.ensure_staging_locked()?;
                ensure!(
                    self.put.is_none() && self.patch.is_none(),
                    "a transfer is still open"
                );
                let ack = self.commit(*request).await?;
                Ok(vec![(Response::CommitPublicationAck(ack), Vec::new())])
            }
            Request::Done => Ok(vec![(Response::DoneAck, Vec::new())]),
        }
    }

    /// Take the staging lock, create the staging directories, and check the
    /// destination again under the lock. Called by every request that stages
    /// or publishes, so a second receiver for the same destination fails here.
    fn ensure_staging_locked(&mut self) -> Result<()> {
        self.require_ready()?;
        if self.stage_lock.is_some() {
            return Ok(());
        }
        let lock_path = self.stage.root.join("receiver.lock");
        let mut attempts = 0;
        let lock = loop {
            attempts += 1;
            mj_core::config::create_owner_only_directory(&self.config.data_dir)?;
            mj_core::config::create_owner_only_directory(&self.stage.root)?;
            let file = open_lock_file(&lock_path)?;
            match file.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    bail!("another migration to this destination is already in progress")
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(error).with_context(|| format!("lock {}", lock_path.display()));
                }
            }
            // A finishing receiver removes the staging directory, lock file
            // included; a lock on a file that is no longer there excludes nobody.
            if lock_file_is_current(&file, &lock_path)? {
                break FileLock::new(file);
            }
            ensure!(
                attempts < 2,
                "the staging directory kept disappearing while it was locked"
            );
        };
        self.stage.create()?;
        // A receiver killed mid-PutFile leaves a temporary behind. Holding the
        // lock, nothing of this session has started, so every one is orphaned.
        for scope in [
            TransferScope::Data,
            TransferScope::Config,
            TransferScope::SessionWiki,
            TransferScope::Home,
        ] {
            remove_orphaned_put_files(&self.stage.raw(scope))?;
        }
        match destination_status(&self.config, &self.stage)? {
            DestinationStatus::Empty | DestinationStatus::ResumablePublication { .. } => {}
            _ => bail!(
                "the destination changed while it was being staged; it now holds a store or a \
                 container daemon"
            ),
        }
        self.stage_lock = Some(lock);
        Ok(())
    }

    async fn hello(&mut self, hello: Hello) -> Result<Reply> {
        let config = self.config.clone();
        let stage = self.stage.clone();
        let (status, wiki, staged, stage_id) =
            mj_core::runtime::spawn_blocking(move || -> Result<_> {
                let status = destination_status(&config, &stage)?;
                let wiki = if exists(&config.sessionwiki_db)? {
                    SessionWikiStatus::Existing
                } else {
                    SessionWikiStatus::Absent
                };
                let staged = staged_sizes(&config, &stage)?;
                Ok((status, wiki, staged, stage.read_stage_id()?))
            })
            .await
            .context("join destination inspection")??;
        let mut refusal = match &status {
            DestinationStatus::Empty | DestinationStatus::ResumablePublication { .. } => None,
            DestinationStatus::ExistingStore { .. } => Some(
                "the destination already has an mj.sqlite3 or config.toml; clean it manually or \
                 migrate into another instance with --dest-instance"
                    .to_owned(),
            ),
            DestinationStatus::ContainerDaemon => Some(
                "the destination runs a container-hosted daemon, which migration does not support"
                    .to_owned(),
            ),
        };
        if refusal.is_none()
            && (hello.protocol_version != PROTOCOL_VERSION
                || hello.schema_version != mj_store::database::STORE_SCHEMA_VERSION
                || hello.os_family != "unix")
        {
            refusal = Some(format!(
                "the source (protocol {}, schema {}, {}) does not match this destination \
                 (protocol {PROTOCOL_VERSION}, schema {}, unix); run the same Mjolnir version on both",
                hello.protocol_version,
                hello.schema_version,
                hello.os_family,
                mj_store::database::STORE_SCHEMA_VERSION
            ));
        }
        // Nothing is created here: Hello, probes and dry runs leave no staging.
        let config = &self.config;
        let ack = HelloAck {
            stage_id,
            mj_version: env!("CARGO_PKG_VERSION").to_owned(),
            schema_version: mj_store::database::STORE_SCHEMA_VERSION,
            protocol_version: PROTOCOL_VERSION,
            os_family: "unix".to_owned(),
            home_dir: config.home_dir.clone(),
            data_dir: config.data_dir.clone(),
            config_dir: config.config_dir.clone(),
            sessionwiki_db: config.sessionwiki_db.clone(),
            destination_status: status,
            sessionwiki_status: wiki,
            available_bytes_data: available_bytes(&config.data_dir),
            available_bytes_config: available_bytes(&config.config_dir),
            available_bytes_sessionwiki: available_bytes(&config.sessionwiki_db),
            staged_bytes_data: staged.0,
            staged_bytes_config: staged.1,
            staged_bytes_sessionwiki: staged.2,
        };
        self.ready = Some(match refusal {
            None => Ok(()),
            Some(reason) => Err(reason),
        });
        Ok(vec![(Response::HelloAck(Box::new(ack)), Vec::new())])
    }

    fn put_start(
        &mut self,
        scope: TransferScope,
        rel_path: &Path,
        kind: FileKind,
    ) -> Result<Reply> {
        ensure!(self.put.is_none(), "the previous PutFile did not end");
        validate_scope_path(scope, rel_path, true)?;
        if scope == TransferScope::Data {
            ensure!(
                rel_path != Path::new(MAIN_DB) && rel_path != Path::new(MAIN_SHM),
                "{} is transferred with PatchBlocks, not PutFile",
                rel_path.display()
            );
        }
        let destination = resolve_in_scope(&self.stage.raw(scope), rel_path, true)?
            .context("cannot create the destination path")?;
        match kind {
            FileKind::Directory { mode } => {
                match symlink_metadata_opt(&destination)? {
                    Some(existing) if existing.is_dir() => {}
                    Some(_) => {
                        remove_path(&destination)?;
                        std::fs::create_dir(&destination)?;
                    }
                    None => std::fs::create_dir(&destination)?,
                }
                // Staged directories stay owner-writable so their children can
                // be placed; the source's mode is applied when publishing.
                set_unix_mode(&destination, mode | 0o700)?;
                self.stage.record_directory_mode(scope, rel_path, mode)?;
                Ok(ack())
            }
            FileKind::Symlink { target } => {
                remove_path(&destination)?;
                self.stage.prune_directory_modes(scope, rel_path)?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(&target, &destination)
                    .with_context(|| format!("create symlink {}", destination.display()))?;
                #[cfg(not(unix))]
                bail!(
                    "cannot create symlink {} on this platform",
                    target.display()
                );
                #[cfg(unix)]
                Ok(ack())
            }
            FileKind::Regular { mode, mtime_ns, .. } => {
                let directory = destination.parent().context("file has no parent")?;
                let temporary = tempfile::Builder::new()
                    .prefix(".mj-put-")
                    .tempfile_in(directory)?;
                self.put = Some(PutState {
                    scope,
                    rel_path: rel_path.to_path_buf(),
                    temporary,
                    destination,
                    hasher: Sha256::new(),
                    mode,
                    mtime_ns,
                });
                Ok(Vec::new())
            }
        }
    }

    fn put_end(&mut self, sha256: &str) -> Result<Reply> {
        let state = self.put.take().context("PutFileEnd without PutFileStart")?;
        ensure!(
            hex(&state.hasher.finalize()) == sha256,
            "{} failed its SHA-256 check",
            state.destination.display()
        );
        let file = state.temporary.as_file();
        set_unix_mode(state.temporary.path(), state.mode)?;
        if state.mtime_ns > 0 {
            let modified =
                std::time::UNIX_EPOCH + std::time::Duration::from_nanos(state.mtime_ns as u64);
            file.set_modified(modified)?;
        }
        file.sync_all()?;
        if symlink_metadata_opt(&state.destination)?.is_some_and(|existing| existing.is_dir()) {
            remove_path(&state.destination)?;
        }
        state
            .temporary
            .persist(&state.destination)
            .with_context(|| format!("place {}", state.destination.display()))?;
        self.stage
            .prune_directory_modes(state.scope, &state.rel_path)?;
        Ok(ack())
    }
}

/// Which relative paths each scope accepts. Database base files and the
/// destination's locks are never written through the file transfer.
fn validate_scope_path(scope: TransferScope, rel_path: &Path, writing: bool) -> Result<()> {
    validate_relative_path(rel_path)?;
    let first = rel_path
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .unwrap_or_default();
    match scope {
        TransferScope::Data => ensure!(
            !FORBIDDEN_DATA_NAMES.contains(&first),
            "{first} cannot be transferred into the data directory"
        ),
        TransferScope::SessionWiki => ensure!(
            matches!(rel_path.to_str(), Some(WIKI_DB | WIKI_WAL))
                || (!writing && rel_path == Path::new(WIKI_SHM)),
            "only the SessionWiki index and its WAL are transferred: {}",
            rel_path.display()
        ),
        TransferScope::Config | TransferScope::Home => {}
    }
    Ok(())
}

fn destination_status(config: &ReceiverConfig, stage: &Stage) -> Result<DestinationStatus> {
    let data = DataLayout::at(&config.data_dir);
    if exists(&data.daemon_container())? {
        return Ok(DestinationStatus::ContainerDaemon);
    }
    let has_db = exists(&data.database())?;
    let layout = ConfigLayout::at(&config.config_dir);
    let has_config = exists(&layout.config_toml())? || exists(&layout.secrets_toml())?;
    if let Some(record) = stage.read_publication()? {
        return Ok(DestinationStatus::ResumablePublication {
            expected_fence: record.expected_fence,
        });
    }
    if has_db || has_config {
        return Ok(DestinationStatus::ExistingStore { has_db, has_config });
    }
    Ok(DestinationStatus::Empty)
}

fn probe_ssh(request: &ProbeSshRequest) -> ProbeSshAck {
    let executor = mj_core::targets::ProcessExecutor;
    let mut failures = Vec::new();
    let mut source_user = None;
    if let Some(source) = &request.source {
        match super::super::preflight::probe_source_directly(
            &executor,
            &source.ssh,
            &source.data_root,
            &source.expected_identity,
            &source.engines,
            request.read_only,
        ) {
            Ok(user) => source_user = Some(user),
            Err(error) => failures.push(format!("{error:#}")),
        }
    }
    for host in &request.third_hosts {
        if let Err(error) =
            super::super::preflight::probe_third_host_directly(&executor, host, request.read_only)
        {
            failures.push(format!("{error:#}"));
        }
    }
    ProbeSshAck {
        source_user,
        failures,
    }
}

fn staged_entries(stage: &Stage, hash_files: bool) -> Result<Vec<StagedEntry>> {
    fn visit(
        scope: TransferScope,
        root: &Path,
        directory: &Path,
        hash_files: bool,
        out: &mut Vec<StagedEntry>,
    ) -> Result<()> {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            let rel_path = path.strip_prefix(root)?.to_path_buf();
            // A temporary from an interrupted transfer is not staged content.
            if entry.file_name().to_string_lossy().starts_with(".mj-put-") {
                continue;
            }
            let (kind, sha256) = if metadata.file_type().is_symlink() {
                (
                    FileKind::Symlink {
                        target: std::fs::read_link(&path)?,
                    },
                    None,
                )
            } else if metadata.is_dir() {
                (
                    FileKind::Directory {
                        mode: unix_mode(&metadata, 0o755),
                    },
                    None,
                )
            } else {
                let sha256 = hash_files.then(|| hash_file(&path)).transpose()?;
                (
                    FileKind::Regular {
                        size: metadata.len(),
                        mode: unix_mode(&metadata, 0o644),
                        mtime_ns: mtime_ns(&metadata),
                    },
                    sha256,
                )
            };
            let is_dir = metadata.is_dir();
            out.push(StagedEntry {
                scope,
                rel_path,
                kind,
                sha256,
            });
            if is_dir {
                visit(scope, root, &path, hash_files, out)?;
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    for scope in [
        TransferScope::Data,
        TransferScope::Config,
        TransferScope::SessionWiki,
        TransferScope::Home,
    ] {
        let root = stage.raw(scope);
        visit(scope, &root, &root, hash_files, &mut out)?;
    }
    Ok(out)
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; BLOCK_BYTES];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(hex(&hasher.finalize()));
        }
        hasher.update(&buffer[..count]);
    }
}

fn block_digests(path: &Path) -> Result<Option<(u64, Vec<u8>)>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let size = file.metadata()?.len();
    let mut digests = Vec::new();
    let mut buffer = vec![0_u8; BLOCK_BYTES];
    loop {
        let mut filled = 0;
        while filled < buffer.len() {
            let count = file.read(&mut buffer[filled..])?;
            if count == 0 {
                break;
            }
            filled += count;
        }
        if filled == 0 {
            break;
        }
        digests.extend_from_slice(&Sha256::digest(&buffer[..filled]));
        if filled < buffer.len() {
            break;
        }
    }
    Ok(Some((size, digests)))
}

fn stage_manifest(
    stage: &Stage,
    files: bool,
    hash_files: bool,
    blocks: &[DbTarget],
) -> Result<Reply> {
    let mut pages: Vec<(ManifestPage, Vec<u8>)> = Vec::new();
    if files {
        for chunk in staged_entries(stage, hash_files)?.chunks(MANIFEST_PAGE_ITEMS) {
            pages.push((
                ManifestPage {
                    stage_id: None,
                    entries: chunk.to_vec(),
                    blocks: None,
                    last: false,
                },
                Vec::new(),
            ));
        }
    }
    for target in blocks {
        let path = match target {
            DbTarget::Main => stage.raw(TransferScope::Data).join(MAIN_DB),
            DbTarget::SessionWiki => stage.raw(TransferScope::SessionWiki).join(WIKI_DB),
        };
        let Some((total_size, digests)) = block_digests(&path)? else {
            continue;
        };
        if digests.is_empty() {
            pages.push((
                ManifestPage {
                    stage_id: None,
                    entries: Vec::new(),
                    blocks: Some(BlockPage {
                        target: *target,
                        total_size,
                        first_block: 0,
                    }),
                    last: false,
                },
                Vec::new(),
            ));
        }
        for (page, chunk) in digests.chunks(32 * MANIFEST_PAGE_ITEMS).enumerate() {
            pages.push((
                ManifestPage {
                    stage_id: None,
                    entries: Vec::new(),
                    blocks: Some(BlockPage {
                        target: *target,
                        total_size,
                        first_block: (page * MANIFEST_PAGE_ITEMS) as u64,
                    }),
                    last: false,
                },
                chunk.to_vec(),
            ));
        }
    }
    if pages.is_empty() {
        pages.push((
            ManifestPage {
                stage_id: None,
                entries: Vec::new(),
                blocks: None,
                last: false,
            },
            Vec::new(),
        ));
    }
    if let Some((page, _)) = pages.last_mut() {
        page.last = true;
        page.stage_id = stage.read_stage_id()?;
    }
    Ok(pages
        .into_iter()
        .map(|(page, payload)| (Response::StageManifestAck(page), payload))
        .collect())
}

struct Published {
    start_lock: FileLock,
    summary: PublicationSummary,
}

fn lock_file_is_current(file: &std::fs::File, path: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match std::fs::symlink_metadata(path) {
            Ok(on_disk) => Ok(on_disk.ino() == file.metadata()?.ino()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (file, path);
        Ok(true)
    }
}

fn open_lock_file(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("open lock {}", path.display()))
}

impl Receiver {
    /// Publish and activate. `Err` is a failure of the stream or the task that
    /// leaves the outcome unknown; a definite failure before the activation
    /// commit is `FailedBeforeCommit`.
    async fn commit(&mut self, request: CommitPublication) -> Result<CommitPublicationAck> {
        let config = self.config.clone();
        let stage = self.stage.clone();
        let start_daemon = request.start_daemon;
        let outcome = mj_core::runtime::spawn_blocking(move || publish(&config, &stage, &request))
            .await
            .context("join publication")?;
        let published = match outcome {
            Ok(published) => published,
            Err(error) => {
                return Ok(CommitPublicationAck::FailedBeforeCommit {
                    error: format!("{error:#}"),
                });
            }
        };
        // The staging directory is gone; release its lock with the commit.
        self.stage_lock = None;
        let Published {
            start_lock,
            mut summary,
        } = published;
        // The store is unfenced. The start lock is still held, so a client on
        // this host cannot start a second daemon before ours.
        let mut daemon_started = false;
        if start_daemon {
            match &self.config.start_daemon {
                Some(start) => match start().await {
                    Ok(()) => daemon_started = true,
                    Err(error) => summary
                        .warnings
                        .push(format!("the destination daemon did not start: {error:#}")),
                },
                None => summary
                    .warnings
                    .push("no daemon launcher is configured on the destination".to_owned()),
            }
        }
        if let Err(error) = start_lock.release() {
            summary
                .warnings
                .push(format!("could not release the daemon start lock: {error}"));
        }
        Ok(CommitPublicationAck::Activated {
            summary,
            daemon_started,
        })
    }
}

/// Every step before the unfence commit runs while B's store is absent or
/// fenced, under B's start lock and controller lock, so no client can start a
/// daemon on a partial store. The last step is the commit.
fn publish(
    config: &ReceiverConfig,
    stage: &Stage,
    request: &CommitPublication,
) -> Result<Published> {
    let hook = |step| config.hook.as_ref().map_or(Ok(()), |hook| hook(step));
    let spec = &request.rewrite_spec;
    ensure!(
        spec.new_data_root == config.data_dir
            && spec.new_config_root == config.config_dir
            && spec.new_home == config.home_dir
            && request.config_rewrite_spec.new_config_root == config.config_dir
            && request.config_rewrite_spec.new_home == config.home_dir,
        "the rewrite specification does not describe this destination"
    );
    for target in &request.home_targets {
        validate_relative_path(&target.rel_path)?;
    }
    let data = DataLayout::at(&config.data_dir);
    let layout = ConfigLayout::at(&config.config_dir);
    let final_db = data.database();

    // 1. The start lock, on a stable inode inside the data directory.
    mj_core::config::create_owner_only_directory(&config.data_dir)?;
    let start_lock =
        FileLock::acquire_cancellable(open_lock_file(&data.daemon_start_lock())?, &|| false)
            .context("lock the destination daemon start lock")?;

    // 2. Re-verify the destination under the lock.
    let mut loaded = stage.read_publication()?;
    if let Some(old) = &loaded
        && !old.expected_fence.matches_snapshot(&spec.expected_fence)
    {
        // An earlier attempt was rolled back on the source, which then ran on
        // and fenced again, so its fence is not this one. What it created is
        // recorded, so it is provably ours and stale; remove exactly that.
        discard_unfinished_publication(config, stage, old)?;
        loaded = None;
    }
    let resuming = loaded.is_some();
    let mut summary = PublicationSummary {
        home_targets: request.home_targets.len(),
        sessionwiki: request.migrate_sessionwiki,
        ..PublicationSummary::default()
    };
    // Parents before children, and children a copied parent already contains.
    let mut targets = request.home_targets.clone();
    targets.sort_by_key(|target| target.rel_path.components().count());
    let covered = |target: &HomeTarget| {
        targets.iter().any(|parent| {
            !parent.private
                && parent.rel_path != target.rel_path
                && target.rel_path.starts_with(&parent.rel_path)
        })
    };
    let raw_data = stage.raw(TransferScope::Data);
    let raw_config = stage.raw(TransferScope::Config);
    if exists(&final_db)? {
        ensure!(
            resuming,
            "the destination already has an mj.sqlite3; clean it manually or choose --dest-instance"
        );
        match read_store_fence_file(&final_db)? {
            Some(fence) => ensure!(
                fence.matches_snapshot(&spec.expected_fence),
                "the destination store carries a different fence than this migration"
            ),
            None => {
                // Unfenced: either the earlier run reached its commit and only
                // its reply was lost, or a client made a store of its own.
                // Only the inode this publication placed counts as ours.
                let current = symlink_metadata_opt(&final_db)?
                    .as_ref()
                    .and_then(FileIdentity::of);
                let placed = loaded.as_ref().is_some_and(|record| {
                    record.final_db_placed && record.final_db_identity == current
                });
                ensure!(
                    placed,
                    "the destination already has an unrelated active mj.sqlite3; clean it \
                     manually or choose --dest-instance"
                );
                // The store is active: from here nothing may return `Err`, which
                // would tell the source to unfence and restart. Whatever cannot be
                // finished is a warning, and staging is kept for a retry.
                let finish = || -> Result<()> {
                    // A crash right after the commit may have left the migrated
                    // SessionWiki index beside the old one.
                    if let Some(record) =
                        loaded.as_ref().filter(|record| record.sessionwiki_pending)
                    {
                        let pending = sessionwiki_pending_path(&config.sessionwiki_db);
                        if let Some(found) = symlink_metadata_opt(&pending)? {
                            verify_owned(
                                &pending,
                                &OwnedEntry {
                                    path: pending.clone(),
                                    identity: record.sessionwiki_pending_identity,
                                },
                                &found,
                            )?;
                            let in_use = if request.overwrite_sessionwiki
                                && exists(&config.sessionwiki_db)?
                            {
                                Some(lock_existing_sessionwiki(&config.sessionwiki_db)?)
                            } else {
                                None
                            };
                            install_pending_sessionwiki(config, &pending, in_use.is_some())?;
                        }
                    }
                    Ok(())
                };
                match finish() {
                    Ok(()) => {
                        if let Err(error) = remove_path(&stage.root) {
                            summary
                                .warnings
                                .push(format!("could not remove the staging directory: {error:#}"));
                        }
                        summary
                            .warnings
                            .push("the destination was already activated".to_owned());
                    }
                    Err(error) => summary.warnings.push(format!(
                        "the destination was already activated; the migrated SessionWiki index \
                         could not be put in place and staging is kept for a retry: {error:#}"
                    )),
                }
                return Ok(Published {
                    start_lock,
                    summary,
                });
            }
        }
    }
    if !resuming {
        // Nothing this migration will create may already exist: it creates
        // entries atomically and never merges into or replaces user data.
        ensure!(
            !exists(&layout.config_toml())? && !exists(&layout.secrets_toml())?,
            "the destination already has a config.toml; clean it manually or choose --dest-instance"
        );
        for target in targets.iter().filter(|target| !covered(target)) {
            let path = config.home_dir.join(&target.rel_path);
            ensure!(
                !exists(&path)?,
                "destination path already exists: {}",
                path.display()
            );
        }
        for name in publishable_names(&raw_data, skipped_data_name)? {
            let path = config.data_dir.join(&name);
            ensure!(
                !exists(&path)?,
                "destination path already exists: {}; remove it or choose --dest-instance",
                path.display()
            );
        }
        for name in publishable_names(&raw_config, skipped_config_name)? {
            let path = config.config_dir.join(&name);
            ensure!(
                !exists(&path)?,
                "destination path already exists: {}; remove it or choose --dest-instance",
                path.display()
            );
        }
        if request.migrate_sessionwiki && !request.overwrite_sessionwiki {
            ensure_no_sessionwiki(&config.sessionwiki_db)?;
        }
    }

    // 3. The controller lock, again without creating an instance identity.
    let controller = ControllerStoreGuard::acquire_migration_lock_at(&config.data_dir)?;

    // 4. Durable record that an attempt is under way. It owns nothing yet;
    // entries are added as each one is created.
    let mut record = match loaded {
        Some(record) => record,
        None => {
            let record = PublicationRecord {
                expected_fence: spec.expected_fence.clone(),
                owned_home_targets: Vec::new(),
                owned_data_entries: Vec::new(),
                owned_config_entries: Vec::new(),
                sessionwiki_pending: false,
                sessionwiki_pending_identity: None,
                final_db_placed: false,
                final_db_identity: None,
            };
            stage.write_publication(&record)?;
            record
        }
    };

    // 5. The configuration, from the immutable raw copy, so it can repeat.
    let rewritten_config = stage.rewritten().join("config.toml");
    let mapping = rewrite_staged_config_and_files(
        &raw_config.join("config.toml"),
        &rewritten_config,
        &request.config_rewrite_spec,
    )
    .context("rewrite the staged configuration")?;

    // 6. The store, in one SQLite transaction, while it is still fenced.
    let staged_store = raw_data.join(MAIN_DB);
    let rewritten_store = stage.rewritten().join(MAIN_DB);
    if !exists(&final_db)? && !exists(&rewritten_store)? {
        ensure!(
            exists(&staged_store)?,
            "the staged store is missing; the source must send it again"
        );
        remove_path(&raw_data.join(MAIN_SHM))?;
        let mut store_spec = spec.clone();
        store_spec.source_templates = mapping;
        store_spec.staged_sessions_dir = raw_data.join("sessions");
        summary.store = mj_store::database::apply_store_migration(&staged_store, &store_spec)
            .context("rewrite the staged store")?;
        remove_empty_file(&raw_data.join(MAIN_WAL))?;
        remove_path(&raw_data.join(MAIN_SHM))?;
        hook(PublicationStep::BeforeStoreRename)?;
        std::fs::rename(&staged_store, &rewritten_store)
            .context("move the rewritten store aside")?;
    }

    // 7. Dependencies first. The final store is not in place yet.
    for target in &targets {
        let src = stage.raw(TransferScope::Home).join(&target.rel_path);
        let dst = config.home_dir.join(&target.rel_path);
        if covered(target) {
            // Its parent directory already carried it over.
            ensure!(
                exists(&dst)?,
                "copied home entry is missing: {}",
                dst.display()
            );
        } else {
            ensure!(
                exists(&src)? || record.owns(Owned::Home, &target.rel_path),
                "staged home entry is missing: {}",
                target.rel_path.display()
            );
            install_entry(
                stage,
                &mut record,
                Owned::Home,
                &target.rel_path,
                &src,
                &dst,
                PublishMode::Move,
            )?;
        }
        if target.private {
            set_unix_mode(&dst, 0o600)?;
        }
    }
    std::fs::create_dir_all(&config.config_dir)?;
    for name in publishable_names(&raw_config, skipped_config_name)? {
        install_entry(
            stage,
            &mut record,
            Owned::Config,
            &name,
            &raw_config.join(&name),
            &config.config_dir.join(&name),
            PublishMode::Copy,
        )?;
    }
    let mut published_data = 0;
    for name in publishable_names(&raw_data, skipped_data_name)? {
        install_entry(
            stage,
            &mut record,
            Owned::Data,
            &name,
            &raw_data.join(&name),
            &config.data_dir.join(&name),
            PublishMode::Move,
        )?;
        published_data += 1;
    }
    summary.data_entries = published_data;
    // An overwritten index stays untouched, and exclusively locked, until the
    // activation commit below.
    let pending_wiki = if request.migrate_sessionwiki {
        publish_sessionwiki(config, stage, request, &mut record)?
    } else {
        None
    };

    install_entry(
        stage,
        &mut record,
        Owned::Config,
        Path::new("config.toml"),
        &rewritten_config,
        &layout.config_toml(),
        PublishMode::Copy,
    )?;
    // Directories whose real mode is restrictive were kept writable while
    // they were filled; restore them now, deepest first.
    let mut modes = stage.read_directory_modes()?;
    modes.sort_by_key(|entry| std::cmp::Reverse(entry.rel_path.components().count()));
    for entry in modes {
        let (base, ours) = match entry.scope {
            TransferScope::Home => (
                &config.home_dir,
                record
                    .owned_home_targets
                    .iter()
                    .any(|owned| entry.rel_path.starts_with(&owned.path)),
            ),
            TransferScope::Data => (
                &config.data_dir,
                first_component_is_owned(&record.owned_data_entries, &entry.rel_path),
            ),
            TransferScope::Config => (
                &config.config_dir,
                first_component_is_owned(&record.owned_config_entries, &entry.rel_path),
            ),
            TransferScope::SessionWiki => continue,
        };
        // Only directories this publication created, and only by walking real
        // directories: a link that replaced one must never be followed.
        if ours && is_real_directory_path(base, &entry.rel_path)? {
            set_unix_mode(&base.join(&entry.rel_path), entry.mode)?;
        }
    }
    hook(PublicationStep::BeforeStorePlacement)?;
    if !exists(&final_db)? {
        // A stale WAL beside a database that is not ours would be replayed into it.
        remove_path(&data.database_wal())?;
        remove_path(&data.database_shm())?;
        publish_entry_no_replace(&rewritten_store, &final_db, PublishMode::Move, &mut || {
            record.final_db_identity = symlink_metadata_opt(&final_db)?
                .as_ref()
                .and_then(FileIdentity::of);
            record.final_db_placed = true;
            stage.write_publication(&record)
        })?;
        sync_directory(&config.data_dir)?;
    }
    hook(PublicationStep::BeforeUnfence)?;

    // 8. The single commit point.
    clear_store_fence_file(&final_db, &spec.expected_fence)
        .context("clear the destination store fence")?;

    // The SessionWiki index is put in place only now. The store is active, so
    // a failure is reported, and staging is kept so a retry can finish it.
    let mut keep_stage = false;
    if let Some(pending) = pending_wiki {
        if let Err(error) =
            install_pending_sessionwiki(config, &pending.pending_path, pending._in_use.is_some())
        {
            summary.warnings.push(format!(
                "the migrated SessionWiki index is waiting at {}: {error:#}",
                pending.pending_path.display()
            ));
            keep_stage = true;
        }
        drop(pending);
    }

    // 9. Cleanup is best effort: the destination is already activated.
    if !keep_stage && let Err(error) = remove_path(&stage.root) {
        summary
            .warnings
            .push(format!("could not remove the staging directory: {error:#}"));
    }
    drop(controller);
    Ok(Published {
        start_lock,
        summary,
    })
}

/// Remove what a rolled-back publication created outside the staging
/// directory: its still-fenced store and the entries it recorded as created.
/// Anything it did not record was on the destination before it and stays.
fn discard_unfinished_publication(
    config: &ReceiverConfig,
    stage: &Stage,
    old: &PublicationRecord,
) -> Result<()> {
    let data = DataLayout::at(&config.data_dir);
    let final_db = data.database();
    if exists(&final_db)? {
        match read_store_fence_file(&final_db)? {
            Some(fence) if fence.matches_snapshot(&old.expected_fence) => {}
            Some(_) => bail!("the destination store is fenced by another migration"),
            None => bail!("the destination store is already active"),
        }
        remove_path(&final_db)?;
        remove_path(&data.database_wal())?;
        remove_path(&data.database_shm())?;
    }
    // Check every recorded entry before removing any, so a path someone else
    // replaced stops the cleanup with the rest still in place.
    let mut doomed = Vec::new();
    for (entries, base) in [
        (&old.owned_home_targets, &config.home_dir),
        (&old.owned_data_entries, &config.data_dir),
        (&old.owned_config_entries, &config.config_dir),
    ] {
        for entry in entries {
            if base == &config.home_dir {
                validate_relative_path(&entry.path)?;
            } else {
                single_normal_name(&entry.path)?;
            }
            if base == &config.data_dir {
                ensure!(
                    !skipped_data_name(&entry.path.to_string_lossy()),
                    "recorded data entry is reserved: {}",
                    entry.path.display()
                );
            }
            let path = base.join(&entry.path);
            if let Some(found) = symlink_metadata_opt(&path)? {
                verify_owned(&path, entry, &found)?;
                doomed.push(path);
            }
        }
    }
    if old.sessionwiki_pending {
        let pending = sessionwiki_pending_path(&config.sessionwiki_db);
        if let Some(found) = symlink_metadata_opt(&pending)? {
            verify_owned(
                &pending,
                &OwnedEntry {
                    path: pending.clone(),
                    identity: old.sessionwiki_pending_identity,
                },
                &found,
            )?;
            doomed.push(pending);
        }
    }
    for path in doomed {
        remove_path(&path)?;
    }
    remove_path(&stage.rewritten())?;
    std::fs::create_dir_all(stage.rewritten())?;
    remove_path(&stage.publication())
}

fn remove_orphaned_put_files(directory: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("read {}", directory.display())),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            remove_orphaned_put_files(&path)?;
        } else if entry.file_name().to_string_lossy().starts_with(".mj-put-") {
            remove_path(&path)?;
        }
    }
    Ok(())
}

fn first_component_is_owned(owned: &[OwnedEntry], rel_path: &Path) -> bool {
    rel_path.components().next().is_some_and(|first| {
        owned
            .iter()
            .any(|entry| entry.path.as_os_str() == first.as_os_str())
    })
}

/// Whether every component of `rel_path` under `base`, the last included, is a
/// real directory. A symlink anywhere on the way makes it `false`.
fn is_real_directory_path(base: &Path, rel_path: &Path) -> Result<bool> {
    let mut current = base.to_path_buf();
    for component in rel_path.components() {
        current.push(component);
        if !symlink_metadata_opt(&current)?.is_some_and(|metadata| metadata.is_dir()) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn remove_empty_file(path: &Path) -> Result<()> {
    if symlink_metadata_opt(path)?.is_some_and(|metadata| metadata.len() == 0) {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// The migrated index waiting beside the destination's index path, and the
/// exclusive hold on an existing destination index that keeps anyone from
/// opening it until the rename.
struct PendingSessionWiki {
    pending_path: PathBuf,
    _in_use: Option<rusqlite::Connection>,
}

/// Where the migrated index waits until the activation commit.
fn sessionwiki_pending_path(db: &Path) -> PathBuf {
    let mut name = db.file_name().unwrap_or_default().to_owned();
    name.push(".mj-migrate-pending");
    db.with_file_name(name)
}

fn sessionwiki_sidecar_paths(db: &Path) -> [PathBuf; 2] {
    ["-wal", "-shm"].map(|suffix| {
        let mut name = db.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    })
}

/// A fresh destination must have no index and no journal files of one: a
/// stale `-wal` would be replayed into the index placed next to it.
fn ensure_no_sessionwiki(db: &Path) -> Result<()> {
    for path in std::iter::once(db.to_path_buf()).chain(sessionwiki_sidecar_paths(db)) {
        ensure!(
            !exists(&path)?,
            "the destination already has SessionWiki files ({}); remove them or pass \
             --overwrite-sessionwiki to replace the index",
            path.display()
        );
    }
    Ok(())
}

/// Fold the staged index's WAL into it so the index moves as one file. The
/// staged `-shm` is never carried.
fn fold_staged_sessionwiki(raw: &Path, staged: &Path) -> Result<()> {
    remove_path(&raw.join(WIKI_SHM))?;
    if exists(&raw.join(WIKI_WAL))? {
        let connection = rusqlite::Connection::open_with_flags(
            staged,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .context("open the staged SessionWiki index")?;
        let (busy, _, _): (i64, i64, i64) = connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .context("fold the staged SessionWiki WAL")?;
        ensure!(busy == 0, "the staged SessionWiki index is busy");
        drop(connection);
        remove_path(&raw.join(WIKI_WAL))?;
        remove_path(&raw.join(WIKI_SHM))?;
    }
    Ok(())
}

/// Stage the migrated SessionWiki index beside its destination path, to be
/// renamed into place after the activation commit. Until then the destination
/// path is untouched: an index the user asked to overwrite stays as it is,
/// exclusively locked, and a fresh destination stays empty, so nothing can
/// open the migrated index and write to it only to lose those writes when a
/// failed attempt is rolled back.
fn publish_sessionwiki(
    config: &ReceiverConfig,
    stage: &Stage,
    request: &CommitPublication,
    record: &mut PublicationRecord,
) -> Result<Option<PendingSessionWiki>> {
    let raw = stage.raw(TransferScope::SessionWiki);
    let staged = raw.join(WIKI_DB);
    let db = &config.sessionwiki_db;
    let parent = db
        .parent()
        .context("the SessionWiki index has no directory")?;
    let in_use = if request.overwrite_sessionwiki {
        if exists(db)? {
            Some(lock_existing_sessionwiki(db)?)
        } else {
            None
        }
    } else {
        ensure_no_sessionwiki(db)?;
        None
    };
    let pending = sessionwiki_pending_path(db);
    let mut installed = false;
    if record.sessionwiki_pending {
        if let Some(found) = symlink_metadata_opt(&pending)? {
            verify_owned(
                &pending,
                &OwnedEntry {
                    path: pending.clone(),
                    identity: record.sessionwiki_pending_identity,
                },
                &found,
            )?;
            installed = true;
        } else {
            record.sessionwiki_pending = false;
        }
    }
    if !installed {
        ensure!(exists(&staged)?, "the staged SessionWiki index is missing");
        fold_staged_sessionwiki(&raw, &staged)?;
        std::fs::create_dir_all(parent)?;
        publish_entry_no_replace(&staged, &pending, PublishMode::Move, &mut || {
            record.sessionwiki_pending = true;
            record.sessionwiki_pending_identity = symlink_metadata_opt(&pending)?
                .as_ref()
                .and_then(FileIdentity::of);
            stage.write_publication(record)
        })?;
        sync_directory(parent)?;
    }
    Ok(Some(PendingSessionWiki {
        pending_path: pending,
        _in_use: in_use,
    }))
}

/// Put the migrated index at its destination path. `replace_existing` is true
/// only when the caller holds the destination's existing index exclusively:
/// its leftover journal files are then dropped and the rename goes over it.
/// Otherwise nothing may be there, and the install is atomic and refuses an
/// index that appeared meanwhile, however the user asked to overwrite.
fn install_pending_sessionwiki(
    config: &ReceiverConfig,
    pending: &Path,
    replace_existing: bool,
) -> Result<()> {
    let db = &config.sessionwiki_db;
    if replace_existing {
        for path in sessionwiki_sidecar_paths(db) {
            remove_path(&path)?;
        }
        std::fs::rename(pending, db).with_context(|| format!("replace {}", db.display()))?;
    } else {
        for path in sessionwiki_sidecar_paths(db) {
            ensure!(
                !exists(&path)?,
                "stale SessionWiki file at the destination: {}",
                path.display()
            );
        }
        publish_entry_no_replace(pending, db, PublishMode::Move, &mut || Ok(()))?;
    }
    if let Some(parent) = db.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

/// Take the destination's SessionWiki index exclusively, or fail because
/// another process has it open. Switching a WAL database to rollback-journal
/// mode needs the lock that every other WAL connection prevents, so success
/// means no other connection exists.
fn lock_existing_sessionwiki(path: &Path) -> Result<rusqlite::Connection> {
    let in_use = |error: &dyn std::fmt::Display| {
        anyhow::anyhow!(
            "destination SessionWiki index is currently in use by another process; stop \
             standalone sessionwiki or retry ({error})"
        )
    };
    let connection = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| in_use(&error))?;
    connection
        .busy_timeout(std::time::Duration::from_millis(200))
        .map_err(|error| in_use(&error))?;
    connection
        .execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE;")
        .map_err(|error| in_use(&error))?;
    let mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .map_err(|error| in_use(&error))?;
    if mode.eq_ignore_ascii_case("wal") {
        connection
            .execute_batch("ROLLBACK;")
            .map_err(|error| in_use(&error))?;
        let next: String = connection
            .query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))
            .map_err(|error| in_use(&error))?;
        ensure!(
            !next.eq_ignore_ascii_case("wal"),
            "destination SessionWiki index is currently in use by another process; stop \
             standalone sessionwiki or retry"
        );
        connection
            .execute_batch("BEGIN EXCLUSIVE;")
            .map_err(|error| in_use(&error))?;
    }
    Ok(connection)
}
