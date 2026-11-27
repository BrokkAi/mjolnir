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

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PublicationRecord {
    expected_fence: StoreFenceRecord,
    home_targets: Vec<PathBuf>,
    /// Set once the SessionWiki index has been placed, so a rolled-back
    /// attempt can remove exactly what it placed.
    #[serde(default)]
    sessionwiki_published: bool,
    /// Top-level names this publication places in the data directory.
    #[serde(default)]
    published_data_entries: Vec<PathBuf>,
    /// Top-level names this publication places in the configuration directory.
    #[serde(default)]
    published_config_entries: Vec<PathBuf>,
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
    name == "config.toml" || name.ends_with(".lock")
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
                for path in paths {
                    ensure!(
                        path.is_absolute(),
                        "probe path must be absolute: {}",
                        path.display()
                    );
                    exists_map.insert(path.clone(), exists(&path)?);
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
                self.require_ready()?;
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
                self.require_ready()?;
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
                self.require_ready()?;
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
                self.require_ready()?;
                validate_scope_path(scope, &rel_path, false)?;
                if let Some(path) = resolve_in_scope(&self.stage.raw(scope), &rel_path, false)? {
                    remove_path(&path)?;
                }
                Ok(ack())
            }
            Request::CommitPublication(request) => {
                self.require_ready()?;
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

    async fn hello(&mut self, hello: Hello) -> Result<Reply> {
        let config = self.config.clone();
        let stage = self.stage.clone();
        let (status, wiki) = mj_core::runtime::spawn_blocking(move || -> Result<_> {
            let status = destination_status(&config, &stage)?;
            let wiki = if exists(&config.sessionwiki_db)? {
                SessionWikiStatus::Existing
            } else {
                SessionWikiStatus::Absent
            };
            Ok((status, wiki))
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
        if refusal.is_none() {
            self.stage.create()?;
        }
        let config = &self.config;
        let ack = HelloAck {
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
                set_unix_mode(&destination, mode)?;
                Ok(ack())
            }
            FileKind::Symlink { target } => {
                remove_path(&destination)?;
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
        ) {
            Ok(user) => source_user = Some(user),
            Err(error) => failures.push(format!("{error:#}")),
        }
    }
    for host in &request.third_hosts {
        if let Err(error) = super::super::preflight::probe_third_host_directly(&executor, host) {
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
                entries: Vec::new(),
                blocks: None,
                last: false,
            },
            Vec::new(),
        ));
    }
    if let Some((page, _)) = pages.last_mut() {
        page.last = true;
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
    async fn commit(&self, request: CommitPublication) -> Result<CommitPublicationAck> {
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
    let mut record = stage.read_publication()?;
    if let Some(old) = &record
        && !old.expected_fence.matches_snapshot(&spec.expected_fence)
    {
        // An earlier attempt was rolled back on the source, which then ran on
        // and fenced again, so its fence is not this one. What it left here
        // is provably ours (recorded, and fenced with its own fence) and
        // stale; clear it so this attempt does not strand the user.
        discard_unfinished_publication(config, stage, old)?;
        record = None;
    }
    let resuming = record.is_some();
    let mut summary = PublicationSummary {
        home_targets: request.home_targets.len(),
        sessionwiki: request.migrate_sessionwiki,
        ..PublicationSummary::default()
    };
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
                // The earlier run reached its commit; only its reply was lost.
                remove_path(&stage.root)?;
                summary
                    .warnings
                    .push("the destination was already activated".to_owned());
                return Ok(Published {
                    start_lock,
                    summary,
                });
            }
        }
    }
    if !resuming {
        ensure!(
            !exists(&layout.config_toml())? && !exists(&layout.secrets_toml())?,
            "the destination already has a config.toml; clean it manually or choose --dest-instance"
        );
        for target in &request.home_targets {
            let path = config.home_dir.join(&target.rel_path);
            ensure!(
                !exists(&path)?,
                "destination path already exists: {}",
                path.display()
            );
        }
        if request.migrate_sessionwiki && !request.overwrite_sessionwiki {
            ensure!(
                !exists(&config.sessionwiki_db)?,
                "the destination already has a SessionWiki index; pass --overwrite-sessionwiki to replace it"
            );
        }
    }

    // 3. The controller lock, again without creating an instance identity.
    let controller = ControllerStoreGuard::acquire_migration_lock_at(&config.data_dir)?;

    // 4. Durable record, before anything is placed, of what is ours: a
    // rolled-back attempt removes exactly these, so files deleted on the source
    // in between cannot linger here. A resumed attempt keeps what its earlier
    // run recorded, because those entries have already left the staging area.
    let raw_data = stage.raw(TransferScope::Data);
    let raw_config = stage.raw(TransferScope::Config);
    {
        let mut record = record.unwrap_or_else(|| PublicationRecord {
            expected_fence: spec.expected_fence.clone(),
            home_targets: Vec::new(),
            sessionwiki_published: false,
            published_data_entries: Vec::new(),
            published_config_entries: Vec::new(),
        });
        let before = serde_json::to_vec(&record)?;
        for target in &request.home_targets {
            if !record.home_targets.contains(&target.rel_path) {
                record.home_targets.push(target.rel_path.clone());
            }
        }
        for (recorded, found) in [
            (
                &mut record.published_data_entries,
                publishable_names(&raw_data, skipped_data_name)?,
            ),
            (
                &mut record.published_config_entries,
                publishable_names(&raw_config, skipped_config_name)?,
            ),
        ] {
            for name in found {
                if !recorded.contains(&name) {
                    recorded.push(name);
                }
            }
        }
        // Set before the index is placed: a crash in between must still be
        // able to clean it up.
        record.sessionwiki_published |= request.migrate_sessionwiki;
        if serde_json::to_vec(&record)? != before || !resuming {
            stage.write_publication(&record)?;
        }
    }

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
    let mut published_data = 0;
    for target in &request.home_targets {
        let src = stage.raw(TransferScope::Home).join(&target.rel_path);
        let dst = config.home_dir.join(&target.rel_path);
        ensure!(
            resuming || exists(&src)?,
            "staged home entry is missing: {}",
            target.rel_path.display()
        );
        publish_entry(&src, &dst, PublishMode::Move)?;
        if target.private {
            set_unix_mode(&dst, 0o600)?;
        }
    }
    std::fs::create_dir_all(&config.config_dir)?;
    for entry in std::fs::read_dir(&raw_config)? {
        let entry = entry?;
        let name = entry.file_name();
        if skipped_config_name(&name.to_string_lossy()) {
            continue;
        }
        publish_entry(
            &entry.path(),
            &config.config_dir.join(&name),
            PublishMode::Copy,
        )?;
    }
    if request.migrate_sessionwiki {
        publish_sessionwiki(config, stage, request, resuming)?;
    }
    for entry in std::fs::read_dir(&raw_data)? {
        let entry = entry?;
        let name = entry.file_name();
        if skipped_data_name(&name.to_string_lossy()) {
            continue;
        }
        publish_entry(
            &entry.path(),
            &config.data_dir.join(&name),
            PublishMode::Move,
        )?;
        published_data += 1;
    }
    summary.data_entries = published_data;
    publish_entry(&rewritten_config, &layout.config_toml(), PublishMode::Copy)?;
    hook(PublicationStep::BeforeStorePlacement)?;
    if !exists(&final_db)? {
        // A stale WAL beside a database that is not ours would be replayed into it.
        remove_path(&data.database_wal())?;
        remove_path(&data.database_shm())?;
        publish_entry(&rewritten_store, &final_db, PublishMode::Move)?;
        sync_directory(&config.data_dir)?;
    }
    hook(PublicationStep::BeforeUnfence)?;

    // 8. The single commit point.
    clear_store_fence_file(&final_db, &spec.expected_fence)
        .context("clear the destination store fence")?;

    // 9. Cleanup is best effort: the destination is already activated.
    if let Err(error) = remove_path(&stage.root) {
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

/// Remove what a rolled-back publication placed outside the staging directory:
/// its still-fenced store, its configuration and its copied home entries.
fn discard_unfinished_publication(
    config: &ReceiverConfig,
    stage: &Stage,
    old: &PublicationRecord,
) -> Result<()> {
    let data = DataLayout::at(&config.data_dir);
    let layout = ConfigLayout::at(&config.config_dir);
    let final_db = data.database();
    if exists(&final_db)? {
        match read_store_fence_file(&final_db)? {
            Some(fence) if fence.matches_snapshot(&old.expected_fence) => {}
            Some(_) => bail!("the destination store is fenced by another migration"),
            None => bail!("the destination store is already active"),
        }
        remove_path(&final_db)?;
    }
    remove_path(&data.database_wal())?;
    remove_path(&data.database_shm())?;
    remove_path(&layout.config_toml())?;
    remove_path(&layout.secrets_toml())?;
    for rel_path in &old.home_targets {
        validate_relative_path(rel_path)?;
        remove_path(&config.home_dir.join(rel_path))?;
    }
    for name in &old.published_data_entries {
        single_normal_name(name)?;
        ensure!(
            !skipped_data_name(&name.to_string_lossy()),
            "recorded data entry is reserved: {}",
            name.display()
        );
        remove_path(&config.data_dir.join(name))?;
    }
    for name in &old.published_config_entries {
        single_normal_name(name)?;
        remove_path(&config.config_dir.join(name))?;
    }
    if old.sessionwiki_published {
        for suffix in ["", "-wal", "-shm"] {
            let mut name = config.sessionwiki_db.as_os_str().to_owned();
            name.push(suffix);
            remove_path(Path::new(&name))?;
        }
    }
    remove_path(&stage.rewritten())?;
    std::fs::create_dir_all(stage.rewritten())?;
    remove_path(&stage.publication())
}

fn remove_empty_file(path: &Path) -> Result<()> {
    if symlink_metadata_opt(path)?.is_some_and(|metadata| metadata.len() == 0) {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Fold the staged index's WAL into it so the index moves as one file, then
/// place it. The staged `-shm` is never carried.
fn publish_sessionwiki(
    config: &ReceiverConfig,
    stage: &Stage,
    request: &CommitPublication,
    resuming: bool,
) -> Result<()> {
    let raw = stage.raw(TransferScope::SessionWiki);
    let staged = raw.join(WIKI_DB);
    if !exists(&staged)? {
        ensure!(
            resuming && exists(&config.sessionwiki_db)?,
            "the staged SessionWiki index is missing"
        );
        return Ok(());
    }
    remove_path(&raw.join(WIKI_SHM))?;
    if exists(&raw.join(WIKI_WAL))? {
        let connection = rusqlite::Connection::open_with_flags(
            &staged,
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
    if request.overwrite_sessionwiki || resuming {
        for suffix in ["", "-wal", "-shm"] {
            let mut name = config.sessionwiki_db.as_os_str().to_owned();
            name.push(suffix);
            remove_path(Path::new(&name))?;
        }
    }
    if let Some(parent) = config.sessionwiki_db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    publish_entry(&staged, &config.sessionwiki_db, PublishMode::Move)
}
