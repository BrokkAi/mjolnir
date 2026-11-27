//! The source side of `mj migrate`.
//!
//! Phase 1 runs while daemon A is live: it copies every file that is not a
//! SQLite database, then streams the databases while a separate process pins
//! their WAL read marks. Phase 2 is the cutover: with A stopped and fenced it
//! sends only the pinned WALs and what changed since Phase 1, then asks B to
//! publish. A failure before B's activation commit restores A.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use mj_core::config::{Classification, MigrateAction, classify_config_entry, classify_data_entry};
use mj_store::database::StoreFenceRecord;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::super::pin::{PinHandle, WalIdentity, wal_path};
use super::super::preflight::HomeCopy;
use super::super::protocol::*;
use super::fsops::{mtime_ns, unix_mode};

type ScanKey = (TransferScope, PathBuf);

const MAIN_DB_FILE: &str = "mj.sqlite3";
const WIKI_DB_FILE: &str = "index.db";

/// What the source's `mj migrate` hands to the transfer engine.
pub struct SenderPlan {
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub sessionwiki_db: PathBuf,
    pub home_copies: Vec<HomeCopy>,
    /// Credential symlinks inside the configuration directory, relative to it,
    /// sent as regular mode-0600 files.
    pub dereference_config_paths: BTreeSet<PathBuf>,
    pub include_logs: bool,
    pub migrate_sessionwiki: bool,
    /// Stream each database unpinned first when B holds none, so the pinned
    /// pass has little left to send.
    pub warm_base_pass: bool,
    /// The command that runs `mj migrate-pin --db <path>`.
    pub pin_command: Box<dyn Fn(&Path) -> tokio::process::Command + Send + Sync>,
}

/// The source's side of the cutover. `mj migrate` implements it with its
/// daemon start lock, controller guard and daemon control; tests implement it
/// directly. All locks stay inside the implementation.
#[allow(async_fn_in_trait, reason = "implemented and awaited in one task")]
pub trait CutoverHooks {
    /// Stop daemon A and take its start lock and controller lock.
    async fn stop_and_lock(&mut self) -> Result<()>;
    /// Under those locks, check that nothing changed that blocks migration.
    /// `pinned_user_version` is the schema revision the pins saw.
    async fn recheck(&mut self, pinned_user_version: i64) -> Result<()>;
    /// Write the fence on A's store and return it.
    async fn fence(&mut self) -> Result<StoreFenceRecord>;
    /// Remove the fence this migration wrote.
    async fn clear_fence(&mut self) -> Result<()>;
    /// Capture the authoritative metadata from the fenced store and build the
    /// publication request.
    async fn build_commit(
        &mut self,
        fence: &StoreFenceRecord,
        destination: &HelloAck,
        home_targets: &[HomeTarget],
    ) -> Result<CommitPublication>;
    /// Failure before B committed: release the locks and restart daemon A.
    /// This is also called when `stop_and_lock` failed partway, so an
    /// implementation must handle a partly stopped daemon and partly taken
    /// locks, and must be safe to call when nothing was done.
    async fn rollback(&mut self) -> Result<()>;
    /// B committed: finish on A (detach shared Codex logins, log failures) and
    /// release the locks, leaving A fenced for good.
    async fn activated(&mut self, summary: &PublicationSummary) -> Result<()>;
}

#[derive(Debug)]
pub struct SenderOutcome {
    pub summary: PublicationSummary,
    pub daemon_started: bool,
}

struct Link<R, W> {
    reader: tokio::io::BufReader<R>,
    writer: tokio::io::BufWriter<W>,
    pending: usize,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Link<R, W> {
    async fn send(&mut self, request: &Request, payload: &[u8]) -> Result<()> {
        match write_frame(&mut self.writer, request, payload).await {
            Ok(()) => Ok(()),
            Err(error) => {
                // A destination that refused a request says why before it closes.
                let _ = self.writer.flush().await;
                if let Ok(Some((Response::Error { message }, _))) =
                    read_frame::<_, Response>(&mut self.reader).await
                {
                    bail!("destination: {message}");
                }
                Err(error)
            }
        }
    }

    async fn recv(&mut self) -> Result<(Response, Vec<u8>)> {
        self.writer.flush().await?;
        match read_frame::<_, Response>(&mut self.reader).await? {
            Some((Response::Error { message }, _)) => bail!("destination: {message}"),
            Some(reply) => Ok(reply),
            None => bail!("the destination closed the connection"),
        }
    }

    /// Read every outstanding acknowledgement.
    async fn drain(&mut self) -> Result<()> {
        while self.pending > 0 {
            let (reply, _) = self.recv().await?;
            ensure!(
                matches!(reply, Response::Ack),
                "expected an Ack, got {reply:?}"
            );
            self.pending -= 1;
        }
        Ok(())
    }

    /// One data operation was sent whole; its Ack is collected later, so a
    /// round trip does not separate every small file.
    async fn operation_sent(&mut self) -> Result<()> {
        self.writer.flush().await?;
        self.pending += 1;
        if self.pending >= ACK_WINDOW {
            self.drain().await?;
        }
        Ok(())
    }

    async fn request(&mut self, request: &Request) -> Result<(Response, Vec<u8>)> {
        self.drain().await?;
        self.send(request, &[]).await?;
        self.recv().await
    }
}

#[derive(Debug, Clone)]
enum Origin {
    Nothing,
    File(PathBuf),
    /// A consistent `rusqlite` backup of a live SQLite file.
    Snapshot(PathBuf),
}

#[derive(Debug, Clone)]
struct SourceEntry {
    kind: FileKind,
    origin: Origin,
}

struct Pin {
    target: DbTarget,
    handle: PinHandle,
    db_path: PathBuf,
    initial_wal: WalIdentity,
}

/// The state Phase 1 leaves for Phase 2.
pub struct Phase1 {
    destination: HelloAck,
    home_targets: Vec<HomeTarget>,
    sent: BTreeMap<ScanKey, FileKind>,
    pins: Vec<Pin>,
}

pub struct Sender<R, W> {
    link: Link<R, W>,
    plan: SenderPlan,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Sender<R, W> {
    pub fn new(plan: SenderPlan, reader: R, writer: W) -> Self {
        Self {
            link: Link {
                reader: tokio::io::BufReader::with_capacity(256 * 1024, reader),
                writer: tokio::io::BufWriter::with_capacity(256 * 1024, writer),
                pending: 0,
            },
            plan,
        }
    }

    pub async fn hello(&mut self) -> Result<HelloAck> {
        let (reply, _) = self
            .link
            .request(&Request::Hello(Hello {
                mj_version: env!("CARGO_PKG_VERSION").to_owned(),
                schema_version: mj_store::database::STORE_SCHEMA_VERSION,
                protocol_version: PROTOCOL_VERSION,
                os_family: "unix".to_owned(),
            }))
            .await?;
        let Response::HelloAck(ack) = reply else {
            bail!("expected HelloAck, got {reply:?}");
        };
        match &ack.destination_status {
            DestinationStatus::Empty | DestinationStatus::ResumablePublication { .. } => {}
            DestinationStatus::ExistingStore { .. } => bail!(
                "the destination already has an mj.sqlite3 or config.toml; clean it manually or \
                 migrate into another instance with --dest-instance"
            ),
            DestinationStatus::ContainerDaemon => {
                bail!(
                    "the destination runs a container-hosted daemon, which migration does not support"
                )
            }
        }
        ensure!(
            ack.os_family == "unix" && ack.protocol_version == PROTOCOL_VERSION,
            "the destination speaks another migration protocol (protocol {}, {})",
            ack.protocol_version,
            ack.os_family
        );
        ensure!(
            ack.schema_version == mj_store::database::STORE_SCHEMA_VERSION,
            "the destination's store schema is {} but this host's is {}; run the same Mjolnir version on both",
            ack.schema_version,
            mj_store::database::STORE_SCHEMA_VERSION
        );
        Ok(*ack)
    }

    /// Phase 1, with daemon A running.
    pub async fn phase1(&mut self) -> Result<Phase1> {
        let destination = self.hello().await?;
        let home_targets = self.home_targets(&destination.home_dir)?;
        let mut targets = vec![DbTarget::Main];
        if self.plan.migrate_sessionwiki {
            targets.push(DbTarget::SessionWiki);
        }

        // Everything that is not a database first, so no pin is held while the
        // bulk of sessions/ and tool-details/ moves.
        let (manifest, _) = self.manifest(true, &[]).await?;
        let mut sent: BTreeMap<ScanKey, FileKind> = manifest
            .into_iter()
            .map(|entry| ((entry.scope, entry.rel_path), entry.kind))
            .collect();
        self.sync_files(&mut sent, &home_targets).await?;
        // A WAL left by an earlier run belongs to that run's pin generation.
        for name in database_side_files(&self.plan) {
            self.remove_staged(name.0, &name.1).await?;
            sent.remove(&name);
        }

        if self.plan.warm_base_pass {
            let (_, blocks) = self.manifest(false, &targets).await?;
            for target in &targets {
                if !blocks.contains_key(target) {
                    self.patch_pass(*target, None).await?;
                }
            }
        }

        let mut pins = Vec::new();
        for target in &targets {
            let db_path = self.db_path(*target);
            let mut command = (self.plan.pin_command)(&db_path);
            let handle = PinHandle::spawn(&mut command)
                .await
                .with_context(|| format!("pin {}", db_path.display()))?;
            let initial_wal = WalIdentity::inspect(&db_path)?;
            pins.push(Pin {
                target: *target,
                handle,
                db_path,
                initial_wal,
            });
        }
        // Under the pins: always hash the live source against what B holds now.
        let (_, blocks) = self.manifest(false, &targets).await?;
        for (target, pin) in targets.iter().zip(&pins) {
            self.patch_pass(*target, blocks.get(target)).await?;
            pin.handle.ensure_alive()?;
        }
        self.link.drain().await?;
        Ok(Phase1 {
            destination,
            home_targets,
            sent,
            pins,
        })
    }

    /// Phase 2, the cutover. The fence on A is cleared and A restarted on any
    /// failure before B's activation commit. When the outcome of the commit is
    /// unknown, A stays fenced.
    pub async fn phase2<H: CutoverHooks>(
        &mut self,
        mut phase1: Phase1,
        hooks: &mut H,
    ) -> Result<SenderOutcome> {
        let mut state = CutoverState::default();
        let commit = match self.cutover(&mut phase1, hooks, &mut state).await {
            Ok(commit) => commit,
            Err(error) => {
                // Drop the pins (and kill their helpers) before A restarts.
                phase1.pins.clear();
                return Err(restore_source(hooks, &state, error).await);
            }
        };
        let reply = self
            .link
            .request(&Request::CommitPublication(Box::new(commit)))
            .await;
        let ack = match reply {
            Ok((Response::CommitPublicationAck(ack), _)) => ack,
            Ok((other, _)) => {
                return Err(uncertain(anyhow!("unexpected reply {other:?}")));
            }
            Err(error) => return Err(uncertain(error)),
        };
        match ack {
            CommitPublicationAck::Activated {
                summary,
                daemon_started,
            } => {
                if let Err(error) = hooks.activated(&summary).await {
                    tracing::warn!(error = %format!("{error:#}"), "source cleanup after activation failed");
                }
                if let Err(error) = self.link.request(&Request::Done).await {
                    tracing::debug!(error = %format!("{error:#}"), "destination closed without DoneAck");
                }
                Ok(SenderOutcome {
                    summary,
                    daemon_started,
                })
            }
            CommitPublicationAck::FailedBeforeCommit { error } => {
                phase1.pins.clear();
                Err(restore_source(
                    hooks,
                    &state,
                    anyhow!("the destination could not activate the migration: {error}"),
                )
                .await)
            }
        }
    }

    async fn cutover<H: CutoverHooks>(
        &mut self,
        phase1: &mut Phase1,
        hooks: &mut H,
        state: &mut CutoverState,
    ) -> Result<CommitPublication> {
        state.locked = true;
        hooks.stop_and_lock().await?;
        let user_version = phase1
            .pins
            .first()
            .map(|pin| pin.handle.user_version())
            .context("no database is pinned")?;
        hooks.recheck(user_version).await?;
        let fence = hooks.fence().await?;
        state.fenced = true;
        for pin in &phase1.pins {
            pin.handle.ensure_alive()?;
            let current = WalIdentity::inspect(&pin.db_path)?;
            ensure!(
                pin.initial_wal.is_valid_continuation(&current),
                "the write-ahead log of {} changed identity during the migration; the pinned copy \
                 cannot be completed",
                pin.db_path.display()
            );
        }
        let commit = hooks
            .build_commit(&fence, &phase1.destination, &phase1.home_targets)
            .await?;
        // The WALs, then whatever else changed. Neither `-shm` is ever sent.
        for pin in &phase1.pins {
            self.send_wal(pin.target, &pin.db_path).await?;
        }
        let mut sent = std::mem::take(&mut phase1.sent);
        self.sync_files(&mut sent, &phase1.home_targets).await?;
        phase1.sent = sent;
        self.link.drain().await?;
        // Sending the WALs and files took time; the pins must have held throughout.
        for pin in &phase1.pins {
            pin.handle.ensure_alive()?;
            let current = WalIdentity::inspect(&pin.db_path)?;
            ensure!(
                pin.initial_wal.is_valid_continuation(&current),
                "the write-ahead log of {} changed identity while it was copied",
                pin.db_path.display()
            );
        }
        for pin in phase1.pins.drain(..) {
            pin.handle.release().await?;
        }
        Ok(commit)
    }

    fn db_path(&self, target: DbTarget) -> PathBuf {
        match target {
            DbTarget::Main => mj_core::config::DataLayout::at(&self.plan.data_dir).database(),
            DbTarget::SessionWiki => self.plan.sessionwiki_db.clone(),
        }
    }

    fn home_targets(&self, destination_home: &Path) -> Result<Vec<HomeTarget>> {
        self.plan
            .home_copies
            .iter()
            .map(|copy| {
                let rel_path = copy
                    .destination
                    .strip_prefix(destination_home)
                    .with_context(|| {
                        format!(
                            "{} is not under the destination home {}",
                            copy.destination.display(),
                            destination_home.display()
                        )
                    })?
                    .to_path_buf();
                validate_relative_path(&rel_path)?;
                Ok(HomeTarget {
                    rel_path,
                    private: copy.private,
                })
            })
            .collect()
    }

    async fn manifest(
        &mut self,
        files: bool,
        blocks: &[DbTarget],
    ) -> Result<(Vec<StagedEntry>, BTreeMap<DbTarget, Vec<[u8; 32]>>)> {
        self.link.drain().await?;
        self.link
            .send(
                &Request::StageManifest {
                    files,
                    hash_files: false,
                    blocks: blocks.to_vec(),
                },
                &[],
            )
            .await?;
        let mut entries = Vec::new();
        let mut digests: BTreeMap<DbTarget, Vec<[u8; 32]>> = BTreeMap::new();
        loop {
            let (reply, payload) = self.link.recv().await?;
            let Response::StageManifestAck(page) = reply else {
                bail!("expected a manifest page, got {reply:?}");
            };
            entries.extend(page.entries);
            if let Some(block_page) = page.blocks {
                let target = digests.entry(block_page.target).or_default();
                ensure!(
                    target.len() as u64 == block_page.first_block && payload.len() % 32 == 0,
                    "manifest digest pages arrived out of order"
                );
                target.extend(payload.as_chunks::<32>().0.iter().copied());
            }
            if page.last {
                return Ok((entries, digests));
            }
        }
    }

    async fn remove_staged(&mut self, scope: TransferScope, rel_path: &Path) -> Result<()> {
        self.link
            .send(
                &Request::RemoveFile {
                    scope,
                    rel_path: rel_path.to_owned(),
                },
                &[],
            )
            .await?;
        self.link.operation_sent().await
    }

    /// Compare the live database with `staged` block digests and send the
    /// blocks that differ. With no digests every block is sent.
    async fn patch_pass(&mut self, target: DbTarget, staged: Option<&Vec<[u8; 32]>>) -> Result<()> {
        let path = self.db_path(target);
        let mut file = tokio::fs::File::open(&path)
            .await
            .with_context(|| format!("open {}", path.display()))?;
        let total_size = file.metadata().await?.len();
        self.link
            .send(&Request::PatchBlocksBegin { target, total_size }, &[])
            .await?;
        let mut buffer = vec![0_u8; BLOCK_BYTES];
        let mut offset = 0_u64;
        let mut index = 0_u64;
        while offset < total_size {
            let want = BLOCK_BYTES.min((total_size - offset) as usize);
            let filled = read_full(&mut file, &mut buffer[..want]).await?;
            if filled == 0 {
                break;
            }
            let block = &buffer[..filled];
            let digest = Sha256::digest(block);
            let unchanged = staged
                .and_then(|staged| staged.get(index as usize))
                .is_some_and(|known| known[..] == digest[..]);
            if !unchanged {
                let payload = compress(block)?;
                self.link
                    .send(
                        &Request::PatchBlock {
                            index,
                            raw_len: filled as u32,
                            sha256: hex(&digest),
                        },
                        &payload,
                    )
                    .await?;
            }
            offset += filled as u64;
            index += 1;
        }
        self.link.send(&Request::PatchBlocksEnd, &[]).await?;
        self.link.operation_sent().await
    }

    /// The complete WAL of a pinned database, or its removal when A has none.
    async fn send_wal(&mut self, target: DbTarget, db_path: &Path) -> Result<()> {
        let (scope, name) = match target {
            DbTarget::Main => (TransferScope::Data, "mj.sqlite3-wal"),
            DbTarget::SessionWiki => (TransferScope::SessionWiki, "index.db-wal"),
        };
        let wal = wal_path(db_path);
        match tokio::fs::symlink_metadata(&wal).await {
            Ok(metadata) => {
                let kind = FileKind::Regular {
                    size: metadata.len(),
                    mode: 0o600,
                    mtime_ns: mtime_ns(&metadata),
                };
                ensure!(
                    self.put_regular(scope, Path::new(name), kind, &wal).await?
                        != PutOutcome::Vanished,
                    "the write-ahead log disappeared while it was copied"
                );
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.remove_staged(scope, Path::new(name)).await
            }
            Err(error) => Err(error).with_context(|| format!("inspect {}", wal.display())),
        }
    }

    /// Bring B's staged non-database files in line with the source now.
    async fn sync_files(
        &mut self,
        sent: &mut BTreeMap<ScanKey, FileKind>,
        home_targets: &[HomeTarget],
    ) -> Result<()> {
        let source = scan_source(&self.plan, home_targets).await?;
        let protected = protected_names(&self.plan);
        for (key, entry) in &source {
            // A live SQLite file's newest pages can sit in its WAL without
            // changing the file's size or time, so a snapshot is never skipped.
            let snapshot = matches!(entry.origin, Origin::Snapshot(_));
            if !snapshot && sent.get(key) == Some(&entry.kind) {
                continue;
            }
            match (&entry.kind, &entry.origin) {
                (FileKind::Regular { .. }, Origin::File(path)) => {
                    match self
                        .put_regular(key.0, &key.1, entry.kind.clone(), path)
                        .await?
                    {
                        PutOutcome::SentExact => {}
                        // Still changing while read: B holds a valid copy of some
                        // moment, but it is not recorded as current, so the next
                        // scan sends the file again.
                        PutOutcome::SentChangedSize => continue,
                        PutOutcome::Vanished => continue,
                    }
                }
                (FileKind::Regular { .. }, Origin::Snapshot(path)) => {
                    let path = path.clone();
                    let snapshot = mj_core::runtime::spawn_blocking(move || snapshot_sqlite(&path))
                        .await
                        .context("join SQLite snapshot")??;
                    // The announced stamp covers the database and its WAL, so
                    // the snapshot's own size is never equal to it.
                    let outcome = self
                        .put_regular(key.0, &key.1, entry.kind.clone(), snapshot.path())
                        .await?;
                    drop(snapshot);
                    if outcome == PutOutcome::Vanished {
                        continue;
                    }
                }
                _ => {
                    self.link
                        .send(
                            &Request::PutFileStart {
                                scope: key.0,
                                rel_path: key.1.clone(),
                                kind: entry.kind.clone(),
                            },
                            &[],
                        )
                        .await?;
                    self.link.operation_sent().await?;
                }
            }
            sent.insert(key.clone(), entry.kind.clone());
        }
        // B creates the parent directories of what it receives (for example
        // `.secrets/` above a copied credential); they are not stale.
        let parents: BTreeSet<ScanKey> = source
            .keys()
            .flat_map(|(scope, path)| {
                path.ancestors()
                    .skip(1)
                    .filter(|ancestor| !ancestor.as_os_str().is_empty())
                    .map(|ancestor| (*scope, ancestor.to_path_buf()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let stale: Vec<ScanKey> = sent
            .keys()
            .filter(|key| {
                !source.contains_key(*key) && !protected.contains(*key) && !parents.contains(*key)
            })
            .cloned()
            .collect();
        for key in stale {
            self.remove_staged(key.0, &key.1).await?;
            sent.remove(&key);
        }
        self.link.drain().await
    }

    /// Stream one regular file, finishing validly with the SHA-256 of the
    /// bytes actually read even when the file changed size under the read.
    async fn put_regular(
        &mut self,
        scope: TransferScope,
        rel_path: &Path,
        kind: FileKind,
        path: &Path,
    ) -> Result<PutOutcome> {
        let announced = match &kind {
            FileKind::Regular { size, .. } => Some(*size),
            _ => None,
        };
        let mut file = match tokio::fs::File::open(path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PutOutcome::Vanished);
            }
            Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
        };
        self.link
            .send(
                &Request::PutFileStart {
                    scope,
                    rel_path: rel_path.to_owned(),
                    kind,
                },
                &[],
            )
            .await?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; BLOCK_BYTES];
        let mut bytes_sent = 0_u64;
        loop {
            let filled = read_full(&mut file, &mut buffer).await?;
            if filled == 0 {
                break;
            }
            bytes_sent += filled as u64;
            hasher.update(&buffer[..filled]);
            let payload = compress(&buffer[..filled])?;
            self.link
                .send(
                    &Request::PutFileChunk {
                        raw_len: filled as u32,
                    },
                    &payload,
                )
                .await?;
        }
        self.link
            .send(
                &Request::PutFileEnd {
                    sha256: hex(&hasher.finalize()),
                },
                &[],
            )
            .await?;
        self.link.operation_sent().await?;
        Ok(if announced.is_none_or(|size| size == bytes_sent) {
            PutOutcome::SentExact
        } else {
            PutOutcome::SentChangedSize
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PutOutcome {
    /// Sent, and as many bytes as the scan announced.
    SentExact,
    /// Sent, but the file's size differed from the scan: it is still changing.
    SentChangedSize,
    /// Gone before it was opened; nothing was sent.
    Vanished,
}

#[derive(Default)]
struct CutoverState {
    locked: bool,
    fenced: bool,
}

fn uncertain(error: anyhow::Error) -> anyhow::Error {
    error.context(
        "the destination may or may not have activated the migration; this host stays fenced. \
         Run `mj daemon status` on the destination before taking any manual action",
    )
}

/// Undo the cutover on A after a failure that happened before B committed.
async fn restore_source<H: CutoverHooks>(
    hooks: &mut H,
    state: &CutoverState,
    error: anyhow::Error,
) -> anyhow::Error {
    let mut notes = Vec::new();
    if state.fenced
        && let Err(clear) = hooks.clear_fence().await
    {
        notes.push(format!("could not clear the fence: {clear:#}"));
    }
    if state.locked
        && let Err(restart) = hooks.rollback().await
    {
        notes.push(format!("could not restart the daemon: {restart:#}"));
    }
    if notes.is_empty() {
        error.context("the migration was rolled back and the source daemon restarted")
    } else {
        error.context(format!(
            "the migration failed and the source could not be fully restored: {}",
            notes.join("; ")
        ))
    }
}

async fn read_full<F: AsyncRead + Unpin>(file: &mut F, buffer: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let count = file.read(&mut buffer[filled..]).await?;
        if count == 0 {
            break;
        }
        filled += count;
    }
    Ok(filled)
}

/// Staged names the file sync must leave to the database transfer.
fn protected_names(plan: &SenderPlan) -> BTreeSet<ScanKey> {
    let mut names = BTreeSet::from([
        (TransferScope::Data, PathBuf::from(MAIN_DB_FILE)),
        (TransferScope::Data, PathBuf::from("mj.sqlite3-wal")),
    ]);
    if plan.migrate_sessionwiki {
        names.insert((TransferScope::SessionWiki, PathBuf::from(WIKI_DB_FILE)));
        names.insert((TransferScope::SessionWiki, PathBuf::from("index.db-wal")));
    }
    names
}

/// Staged WAL and shared-memory files of the databases, removed before pinning.
fn database_side_files(plan: &SenderPlan) -> Vec<ScanKey> {
    let mut names = vec![
        (TransferScope::Data, PathBuf::from("mj.sqlite3-wal")),
        (TransferScope::Data, PathBuf::from("mj.sqlite3-shm")),
    ];
    if plan.migrate_sessionwiki {
        names.push((TransferScope::SessionWiki, PathBuf::from("index.db-wal")));
        names.push((TransferScope::SessionWiki, PathBuf::from("index.db-shm")));
    }
    names
}

const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

fn is_sqlite_file(path: &Path) -> bool {
    use std::io::Read;
    let mut magic = [0_u8; 16];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && &magic == SQLITE_MAGIC
}

/// A consistent copy of a SQLite database that another process may be
/// writing. The `-wal` and `-shm` files are not copied; the snapshot is whole.
fn snapshot_sqlite(path: &Path) -> Result<tempfile::NamedTempFile> {
    let source = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {} for a snapshot", path.display()))?;
    source.busy_timeout(std::time::Duration::from_secs(5))?;
    let snapshot = tempfile::NamedTempFile::new()?;
    {
        let mut destination = rusqlite::Connection::open(snapshot.path())?;
        let backup = rusqlite::backup::Backup::new(&source, &mut destination)?;
        backup
            .run_to_completion(256, std::time::Duration::from_millis(50), None)
            .with_context(|| format!("snapshot {}", path.display()))?;
    }
    Ok(snapshot)
}

async fn scan_source(
    plan: &SenderPlan,
    home_targets: &[HomeTarget],
) -> Result<BTreeMap<ScanKey, SourceEntry>> {
    ensure!(
        plan.home_copies.len() == home_targets.len(),
        "home copies and targets differ in number"
    );
    let data_dir = plan.data_dir.clone();
    let config_dir = plan.config_dir.clone();
    let sessionwiki_db = plan.sessionwiki_db.clone();
    let include_logs = plan.include_logs;
    let dereference = plan.dereference_config_paths.clone();
    let homes: Vec<(PathBuf, PathBuf, bool)> = plan
        .home_copies
        .iter()
        .zip(home_targets)
        .map(|(copy, target)| (copy.source.clone(), target.rel_path.clone(), target.private))
        .collect();
    mj_core::runtime::spawn_blocking(move || {
        let mut out = BTreeMap::new();
        // The shared index is transferred as a database, never as a file.
        let mut excluded = BTreeSet::new();
        for suffix in ["", "-wal", "-shm"] {
            let mut name = sessionwiki_db.as_os_str().to_owned();
            name.push(suffix);
            excluded.insert(PathBuf::from(name));
        }
        scan_layout(
            &data_dir,
            TransferScope::Data,
            |relative| {
                let name = relative.to_str().unwrap_or_default();
                if matches!(name, "mj.sqlite3" | "mj.sqlite3-wal" | "mj.sqlite3-shm") {
                    return false;
                }
                match classify_data_entry(relative) {
                    Classification::Known(MigrateAction::Copy) | Classification::Unknown => true,
                    Classification::Known(MigrateAction::CopyOptional) => include_logs,
                    _ => false,
                }
            },
            &excluded,
            &BTreeSet::new(),
            &mut out,
        )?;
        scan_layout(
            &config_dir,
            TransferScope::Config,
            |relative| match classify_config_entry(relative) {
                Classification::Known(MigrateAction::Copy) | Classification::Unknown => true,
                Classification::Known(MigrateAction::CopyOptional) => include_logs,
                _ => false,
            },
            &BTreeSet::new(),
            &dereference,
            &mut out,
        )?;
        for (source, rel_path, private) in &homes {
            scan_home(source, rel_path, *private, &mut out)?;
        }
        Ok(out)
    })
    .await
    .context("join source scan")?
}

/// One copied home entry: a dereferenced credential file, or a tree whose
/// symlinks stay symlinks and whose `opencode.db` is snapshotted, not copied.
fn scan_home(
    source: &Path,
    rel_path: &Path,
    private: bool,
    out: &mut BTreeMap<ScanKey, SourceEntry>,
) -> Result<()> {
    if private {
        let metadata = std::fs::metadata(source)
            .with_context(|| format!("credential {} does not resolve", source.display()))?;
        ensure!(
            metadata.is_file(),
            "credential {} is not a regular file",
            source.display()
        );
        out.insert(
            (TransferScope::Home, rel_path.to_path_buf()),
            SourceEntry {
                kind: regular_kind(&metadata, Some(0o600)),
                origin: Origin::File(source.to_path_buf()),
            },
        );
        return Ok(());
    }
    fn visit(
        absolute: &Path,
        relative: &Path,
        out: &mut BTreeMap<ScanKey, SourceEntry>,
    ) -> Result<()> {
        let metadata = match std::fs::symlink_metadata(absolute) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", absolute.display()));
            }
        };
        let key = (TransferScope::Home, relative.to_path_buf());
        if metadata.file_type().is_symlink() {
            out.insert(
                key,
                SourceEntry {
                    kind: FileKind::Symlink {
                        target: std::fs::read_link(absolute)?,
                    },
                    origin: Origin::Nothing,
                },
            );
        } else if metadata.is_dir() {
            out.insert(
                key,
                SourceEntry {
                    kind: FileKind::Directory {
                        mode: unix_mode(&metadata, 0o755),
                    },
                    origin: Origin::Nothing,
                },
            );
            for child in std::fs::read_dir(absolute)? {
                let name = match child {
                    Ok(child) => child.file_name(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                visit(&absolute.join(&name), &relative.join(&name), out)?;
            }
        } else if metadata.is_file() {
            let name = absolute.file_name().and_then(|name| name.to_str());
            let sibling_database = |suffix: &str| {
                absolute
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_suffix(suffix))
                    .map(|stem| absolute.with_file_name(stem))
            };
            if name == Some("opencode.db") && is_sqlite_file(absolute) {
                // The live database's newest pages may still be in its WAL.
                let wal = absolute.with_file_name("opencode.db-wal");
                let wal_metadata = std::fs::metadata(&wal).ok();
                let kind = FileKind::Regular {
                    size: metadata.len() + wal_metadata.as_ref().map_or(0, std::fs::Metadata::len),
                    mode: unix_mode(&metadata, 0o644),
                    mtime_ns: mtime_ns(&metadata).max(wal_metadata.as_ref().map_or(0, mtime_ns)),
                };
                out.insert(
                    key,
                    SourceEntry {
                        kind,
                        origin: Origin::Snapshot(absolute.to_path_buf()),
                    },
                );
            } else if [("-wal"), ("-shm")].iter().any(|suffix| {
                sibling_database(suffix).is_some_and(|database| {
                    database
                        .file_name()
                        .is_some_and(|name| name == "opencode.db")
                        && is_sqlite_file(&database)
                })
            }) {
                // Covered by the snapshot.
            } else {
                out.insert(
                    key,
                    SourceEntry {
                        kind: regular_kind(&metadata, None),
                        origin: Origin::File(absolute.to_path_buf()),
                    },
                );
            }
        }
        Ok(())
    }
    visit(source, rel_path, out)
}

fn regular_kind(metadata: &std::fs::Metadata, mode: Option<u32>) -> FileKind {
    FileKind::Regular {
        size: metadata.len(),
        mode: mode.unwrap_or_else(|| unix_mode(metadata, 0o644)),
        mtime_ns: mtime_ns(metadata),
    }
}

/// Walk `root` and record what `include` accepts, by path relative to `root`.
/// Directories are always recorded so empty ones and their modes survive.
fn scan_layout(
    root: &Path,
    scope: TransferScope,
    include: impl Fn(&Path) -> bool,
    excluded_absolute: &BTreeSet<PathBuf>,
    dereference: &BTreeSet<PathBuf>,
    out: &mut BTreeMap<ScanKey, SourceEntry>,
) -> Result<()> {
    fn visit(
        root: &Path,
        relative: &Path,
        scope: TransferScope,
        include: &dyn Fn(&Path) -> bool,
        excluded_absolute: &BTreeSet<PathBuf>,
        dereference: &BTreeSet<PathBuf>,
        out: &mut BTreeMap<ScanKey, SourceEntry>,
    ) -> Result<()> {
        let absolute = root.join(relative);
        if excluded_absolute.contains(&absolute) || !include(relative) {
            return Ok(());
        }
        let metadata = match std::fs::symlink_metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", absolute.display()));
            }
        };
        let key = (scope, relative.to_path_buf());
        if metadata.file_type().is_symlink() {
            if dereference.contains(relative) {
                match std::fs::metadata(&absolute) {
                    Ok(target) if target.is_file() => {
                        out.insert(
                            key,
                            SourceEntry {
                                kind: regular_kind(&target, Some(0o600)),
                                origin: Origin::File(absolute),
                            },
                        );
                        return Ok(());
                    }
                    Ok(_) => bail!("credential {} is not a regular file", absolute.display()),
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("credential link {} does not resolve", absolute.display())
                        });
                    }
                }
            }
            out.insert(
                key,
                SourceEntry {
                    kind: FileKind::Symlink {
                        target: std::fs::read_link(&absolute)?,
                    },
                    origin: Origin::Nothing,
                },
            );
        } else if metadata.is_dir() {
            out.insert(
                key,
                SourceEntry {
                    kind: FileKind::Directory {
                        mode: unix_mode(&metadata, 0o755),
                    },
                    origin: Origin::Nothing,
                },
            );
            let mut children = Vec::new();
            for child in std::fs::read_dir(&absolute)? {
                match child {
                    Ok(child) => children.push(child.file_name()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            for child in children {
                visit(
                    root,
                    &relative.join(child),
                    scope,
                    include,
                    excluded_absolute,
                    dereference,
                    out,
                )?;
            }
        } else if metadata.is_file() {
            out.insert(
                key,
                SourceEntry {
                    kind: regular_kind(&metadata, None),
                    origin: Origin::File(absolute),
                },
            );
        }
        Ok(())
    }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("read {}", root.display())),
    };
    for entry in entries {
        let entry = entry?;
        visit(
            root,
            Path::new(&entry.file_name()),
            scope,
            &include,
            excluded_absolute,
            dereference,
            out,
        )?;
    }
    Ok(())
}

/// Run both phases in order.
pub async fn run_migration_sender<R, W, H>(
    plan: SenderPlan,
    reader: R,
    writer: W,
    hooks: &mut H,
) -> Result<SenderOutcome>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    H: CutoverHooks,
{
    let mut sender = Sender::new(plan, reader, writer);
    let phase1 = sender.phase1().await?;
    sender.phase2(phase1, hooks).await
}
