//! Origin-relative Git snapshots, packing pinned objects without temporary refs.
//! Filesystem paths belong to the machine running the supplied Git runner.

const RAW_NOTICE: &str = "raw checkout is kept; teardown audits are skipped; untracked files and unsupported Git state do not travel with the snapshot";

use crate::archive::git::{
    GitCommand, GitCommandRunner, discover_linked_worktrees, git_bytes, git_failure, git_text,
    run_git, synthetic_git_commit_identity, synthetic_git_identity,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

pub use mj_core::suspend_consent::{
    CaptureConsent, CheckoutKey, FileIdentity, GitOperation, GitOperationKind, OperationConsent,
    ShownOperation, UntrackedConsent, UntrackedItem, UntrackedPath,
};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Scoped<T> {
    pub checkout: CheckoutKey,
    pub value: T,
}
fn scoped<T>(checkout: &CheckoutKey, value: T) -> Scoped<T> {
    Scoped {
        checkout: checkout.clone(),
        value,
    }
}
pub use mj_core::archive::{
    BranchTracking, SnapshotGitlink, SnapshotHead, SnapshotStashEntry, SnapshotSubmodule,
    SnapshotWorktree, WorkspaceSnapshotMetadata,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub metadata: WorkspaceSnapshotMetadata,
    /// Non-thin Git pack, if origin does not supply every object.
    pub pack: Option<PathBuf>,
}

impl std::ops::Deref for WorkspaceSnapshot {
    type Target = WorkspaceSnapshotMetadata;

    fn deref(&self) -> &Self::Target {
        &self.metadata
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CheckoutOwnership {
    Managed,
    Raw { launch: Option<LaunchRecord> },
}
/// Version 1 is a UTF-8 JSON object followed by a newline. Paths use serde's
/// platform PathBuf representation. Stashes are newest first; refs are full names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchRecord {
    pub version: u32,
    pub refs: BTreeMap<String, String>,
    pub stash: Vec<SnapshotStashEntry>,
    pub worktrees: Vec<PathBuf>,
}
/// Use the same metadata observation for this identity and the displayed size.
/// The controller can use this for .mj/keep entries as well as untracked files.
pub fn file_identity(metadata: &fs::Metadata) -> Result<FileIdentity> {
    let mut identity = blake3::Hasher::new();
    hash_file_metadata(&mut identity, metadata)?;
    Ok(FileIdentity::new(identity.finalize().to_hex().to_string()))
}

/// Scan `.mj/keep/` (or a session's keep directory) for regular files and symlinks,
/// returning each entry with its byte size and file identity plus the total size.
pub fn scan_keep_dir(
    checkout: &CheckoutKey,
    keep_dir: &Path,
) -> Result<(Vec<mj_core::suspend_consent::KeepDirEntry>, u64)> {
    if !keep_dir.exists() {
        return Ok((Vec::new(), 0));
    }
    let mut entries = Vec::new();
    let mut total_bytes = 0u64;
    let mut stack = vec![keep_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut children: Vec<PathBuf> = fs::read_dir(&dir)
            .with_context(|| format!("read keep directory {}", dir.display()))?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .map(|entry| entry.path())
            .collect();
        children.sort();
        for path in children {
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("stat keep entry {}", path.display()))?;
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(keep_dir)
                .unwrap_or(&path)
                .to_path_buf();
            let bytes = metadata.len();
            total_bytes = total_bytes.saturating_add(bytes);
            entries.push(mj_core::suspend_consent::KeepDirEntry {
                checkout: checkout.clone(),
                path: relative,
                bytes,
                identity: file_identity(&metadata)?,
            });
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((entries, total_bytes))
}

/// Verify that every file currently inside `.mj/keep/` is covered by `consent`.
/// Called immediately before capture under the barrier so scratch files created
/// after preflight are never silently discarded.
pub fn check_keep_dir_consent(
    checkout: &CheckoutKey,
    keep_dir: &Path,
    consent: &UntrackedConsent,
) -> Result<()> {
    let (entries, _) = scan_keep_dir(checkout, keep_dir)?;
    if entries.is_empty() {
        return Ok(());
    }
    let uncovered: Vec<UntrackedItem> = entries
        .into_iter()
        .map(|entry| UntrackedItem::KeepDirFile {
            checkout: entry.checkout,
            path: entry.path,
            identity: entry.identity,
        })
        .filter(|item| !consent.covers(item))
        .collect();
    if uncovered.is_empty() {
        Ok(())
    } else {
        Err(SnapshotRefused::Untracked(uncovered).into())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnrepresentedWorktree {
    pub locked: bool,
    pub can_move_inside: bool,
    pub git_dir: PathBuf,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UninitializedOrigin {
    pub owner: PathBuf,
    pub config_key: String,
    pub route: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginDependency {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_url_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_provider: Option<PathBuf>,
    pub route: PathBuf,
    pub canonical_path: PathBuf,
    pub target_is_removed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryBlocker {
    #[serde(flatten)]
    pub kind: SnapshotRefused,
    pub paths: Vec<Scoped<PathBuf>>,
    pub remedy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum SnapshotRefused {
    Untracked(Vec<UntrackedItem>),
    OperationInProgress(ShownOperation),
    DirtySubmodule(Scoped<PathBuf>),
    MissingManagedOrigin(CheckoutKey),
    OriginRemovedByTeardown(Vec<Scoped<OriginDependency>>),
    UnrepresentedGitState {
        directories: Vec<Scoped<PathBuf>>,
        worktrees: Vec<Scoped<UnrepresentedWorktree>>,
    },
    HiddenTrackedEdits(Vec<Scoped<PathBuf>>),
    IntentToAdd(Vec<Scoped<PathBuf>>),
    UntrackedSubmoduleRegistration(Vec<Scoped<PathBuf>>),
    TrackedKeepFiles {
        keep_dir: PathBuf,
        paths: Vec<Scoped<PathBuf>>,
    },
}
impl std::fmt::Display for SnapshotRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingManagedOrigin(checkout) => write!(f,
                "managed checkout {}:{} has no origin remote or source override; restore its origin with `git remote add origin <url>`",
                checkout.repository_id, checkout.checkout_path.display()),
            Self::OriginRemovedByTeardown(origins) => {
                for (index, origin) in origins.iter().enumerate() {
                    if index != 0 { write!(f, "; ")?; }
                    if let Some(provider) = &origin.value.object_provider {
                        let path = mj_core::targets::posix_quote(&origin.value.canonical_path.to_string_lossy());
                        write!(f, "local origin {} relies on provider {} inside the managed teardown; make the origin an independent repository outside that set; to dissociate borrowed objects run `git -C {path} repack -a -d`, then remove its objects/info/alternates, and suspend again", origin.value.route.display(), provider.display())?;
                        continue;
                    }
                    let directory = if origin.checkout.checkout_path.as_os_str().is_empty() { String::new() }
                        else { format!(" -C {}", mj_core::targets::posix_quote(&origin.checkout.checkout_path.to_string_lossy())) };
                    write!(f, "origin route {} for {}:{} depends on the managed teardown; ", origin.value.route.display(), origin.checkout.repository_id, origin.checkout.checkout_path.display())?;
                    let setting = match &origin.value.module_url_key {
                        Some(key) => format!("config -f .gitmodules {}", mj_core::targets::posix_quote(key)),
                        None => "remote set-url origin".to_owned(),
                    };
                    if origin.value.target_is_removed {
                        write!(f, "move the mirror {} outside the clone and run `git{directory} {setting} <new path>`", origin.value.canonical_path.display())?;
                    } else {
                        write!(f, "run `git{directory} {setting} {}`", mj_core::targets::posix_quote(&origin.value.canonical_path.to_string_lossy()))?;
                    }
                    write!(f, ", then suspend again")?;
                }
                Ok(())
            }
            Self::TrackedKeepFiles { keep_dir, paths } => write!(
                f,
                "{} tracked keep files would enter the snapshot; run git rm -r --cached {} and commit any removal from HEAD before suspending",
                paths.len(),
                keep_dir.display()
            ),
            Self::UntrackedSubmoduleRegistration(paths) => write!(f, "{}", paths.iter().map(|entry| {
                let directory = if entry.checkout.checkout_path.as_os_str().is_empty() { String::new() } else { format!(" -C {}", mj_core::targets::posix_quote(&entry.checkout.checkout_path.to_string_lossy())) };
                format!("submodule registration {}:{}:{} is untracked and would not survive discard; run `git{directory} add {}` to preserve it before suspending", entry.checkout.repository_id, entry.checkout.checkout_path.display(), entry.value.display(), mj_core::targets::posix_quote(&entry.value.to_string_lossy()))
            }).collect::<Vec<_>>().join("; ")),
            Self::IntentToAdd(paths) => write!(f, "{}", paths.iter().map(|entry| {
                let directory = if entry.checkout.checkout_path.as_os_str().is_empty() { String::new() }
                    else { format!(" -C {}", mj_core::targets::posix_quote(&entry.checkout.checkout_path.to_string_lossy())) };
                let path = mj_core::targets::posix_quote(&entry.value.to_string_lossy());
                format!("intent-to-add {}:{}:{} cannot be captured; run `git{directory} add {path}` to stage its contents, or `git{directory} reset -- {path}` to leave it untracked",
                    entry.checkout.repository_id, entry.checkout.checkout_path.display(), entry.value.display())
            }).collect::<Vec<_>>().join("; ")),
            Self::HiddenTrackedEdits(paths) => write!(
                f,
                "{} have skip-worktree or assume-unchanged flags; clear those flags before suspending or moving so all tracked edits can be verified",
                paths
                    .iter()
                    .map(|path| format!(
                        "{}:{}:{}",
                        path.checkout.repository_id,
                        path.checkout.checkout_path.display(),
                        path.value.display()
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::UnrepresentedGitState { directories, worktrees } => {
                let mut items = directories.iter().map(|item| format!("unsupported Git state at {}; finish or remove it through Git before suspending", item.value.display())).collect::<Vec<_>>();
                items.extend(worktrees.iter().map(|item| {
                    let repo = mj_core::targets::posix_quote(&item.value.git_dir.to_string_lossy());
                    let path = mj_core::targets::posix_quote(&item.value.path.to_string_lossy());
                    let alternative = if item.value.can_move_inside && !item.value.locked { " or move it inside the clone" } else { "" };
                    {
                    let locked = if item.value.locked { format!("; this worktree is locked: once it is available and its contents are safe, deliberately run `git --git-dir {repo} --work-tree {repo} worktree unlock {path}` first") } else { String::new() };
                    format!("unrepresented worktree {}{locked}; push or copy out anything wanted, then run `git --git-dir {repo} --work-tree {repo} worktree remove --force {path}`{alternative} before suspending", item.value.path.display())
                }
                }));
                write!(f, "{}", items.join("; "))
            }
            Self::DirtySubmodule(path) => write!(f,
                "submodule {}:{}:{} has tracked changes or an origin-relative delta; commit and push the work (including wanted refs and stashes)",
                path.checkout.repository_id, path.checkout.checkout_path.display(), path.value.display()),
            Self::OperationInProgress(operation) => {
                write!(
                    f,
                    "{:?} in progress at {}:{}; finish it or run `git {command} {argument}` there before suspending",
                    operation.kind,
                    operation.checkout.repository_id,
                    operation.checkout.checkout_path.display(),
                    command = abort_operation_arguments(operation.kind)[0],
                    argument = abort_operation_arguments(operation.kind)[1],
                )
            }
            Self::Untracked(items) => {
                write!(
                    f,
                    "{} untracked items require discard consent:",
                    items.len()
                )?;
                for item in items {
                    match item {
                        UntrackedItem::Path { checkout, path, .. } => write!(
                            f,
                            " {}:{}:{}",
                            checkout.repository_id,
                            checkout.checkout_path.display(),
                            path.display()
                        )?,
                        UntrackedItem::NestedRepository { checkout, path, .. } => write!(
                            f,
                            " {}:{} nested repository {}",
                            checkout.repository_id,
                            checkout.checkout_path.display(),
                            path.display()
                        )?,
                        UntrackedItem::Stash {
                            checkout,
                            commit,
                            message,
                        } => write!(
                            f,
                            " {}:{} stash {commit} ({message})",
                            checkout.repository_id,
                            checkout.checkout_path.display()
                        )?,
                        UntrackedItem::KeepDirFile { checkout, path, .. } => write!(
                            f,
                            " {}:{} .mj/keep/{}",
                            checkout.repository_id,
                            checkout.checkout_path.display(),
                            path.display()
                        )?,
                    }
                }
                write!(f, ". Commit wanted files, move them outside the clone, or remove them before suspending. Preserve wanted stash contents outside the clone before dropping those stashes.")
            }
        }
    }
}
impl std::error::Error for SnapshotRefused {}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInventory {
    pub notices: Vec<String>,
    pub checkout: CheckoutKey,
    pub untracked: Vec<UntrackedPath>,
    pub untracked_stashes: Vec<Scoped<SnapshotStashEntry>>,
    pub nested_repositories: Vec<UntrackedPath>,
    pub tracked_keep_files: Vec<Scoped<PathBuf>>,
    pub keep_dir: Option<PathBuf>,
    pub dirty_submodules: Vec<Scoped<PathBuf>>,
    pub untracked_submodule_registration: Vec<Scoped<PathBuf>>,
    pub unrepresented_worktrees: Vec<Scoped<UnrepresentedWorktree>>,
    pub presence: Vec<Scoped<PathBuf>>,
    pub origins_removed_by_teardown: Vec<Scoped<OriginDependency>>,
    #[serde(skip)]
    pub(crate) uninitialized_origins: Vec<UninitializedOrigin>,
    pub blockers: Vec<InventoryBlocker>,
    pub in_place_only: bool,
    pub origin_missing: bool,
    pub hidden_tracked_edits: Vec<Scoped<PathBuf>>,
    pub intent_to_add: Vec<Scoped<PathBuf>>,
    pub initialized_submodules: Vec<Scoped<SnapshotSubmodule>>,
    pub index_gitlinks: Vec<Scoped<SnapshotGitlink>>,
    pub operation: Option<GitOperation>,
    pub raw: bool,
    pub remote_refs_stale: bool,
    pub worktrees: Vec<(PathBuf, WorkspaceInventory)>,
}

/// Disable source maintenance, including lazy promisor fetch children.
struct CaptureGit<'a> {
    inner: &'a dyn GitCommandRunner,
}
impl<'a> CaptureGit<'a> {
    fn new(inner: &'a dyn GitCommandRunner) -> Self {
        Self { inner }
    }
}
impl GitCommandRunner for CaptureGit<'_> {
    fn run(&self, repository: &Path, command: &GitCommand) -> Result<crate::archive::GitOutput> {
        let command = self.prepare_command(repository, command)?;
        self.inner.run(repository, &command)
    }

    fn run_to_file(
        &self,
        repository: &Path,
        command: &GitCommand,
        output: fs::File,
    ) -> Result<crate::archive::GitOutput> {
        let command = self.prepare_command(repository, command)?;
        self.inner.run_to_file(repository, &command, output)
    }
}
impl CaptureGit<'_> {
    fn prepare_command(&self, repository: &Path, command: &GitCommand) -> Result<GitCommand> {
        let mut command = command.clone();
        let mut arguments = vec![
            "-c".into(),
            "gc.auto=0".into(),
            "-c".into(),
            "maintenance.auto=false".into(),
        ];
        arguments.extend(command.arguments);
        command.arguments = arguments;
        honor_repository_ssh(self.inner, repository, &mut command)?;
        // Snapshot packs must hydrate promised objects they carry. The -c
        // maintenance settings above also reach Git's lazy-fetch children.
        command.env.push(("GIT_NO_LAZY_FETCH".into(), "0".into()));
        // Inspection must not refresh or lock a user's real index.
        command.env.push(("GIT_OPTIONAL_LOCKS".into(), "0".into()));
        Ok(command)
    }
}
// Preserve a user-selected identity or proxy before the runner supplies its
// default SSH command. The explicit process environment still wins.
fn honor_repository_ssh(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    command: &mut GitCommand,
) -> Result<()> {
    if std::env::var_os("GIT_SSH_COMMAND").is_none()
        && !command
            .env
            .iter()
            .any(|(name, _)| name == "GIT_SSH_COMMAND")
    {
        let configured = runner.run(
            repository,
            &GitCommand {
                arguments: ["config", "--get", "core.sshCommand"]
                    .map(OsString::from)
                    .to_vec(),
                env: command.env.clone(),
                ..Default::default()
            },
        )?;
        ensure!(
            configured.status == 0 || configured.status == 1,
            "{}",
            git_failure("read source SSH command", &configured)
        );
        if configured.status == 0 {
            let value = configured
                .stdout
                .strip_suffix(b"\n")
                .unwrap_or(&configured.stdout);
            command.env.push((
                "GIT_SSH_COMMAND".into(),
                mj_core::path_input::from_git_bytes(value)?.into_os_string(),
            ));
        }
    }
    Ok(())
}
pub(crate) fn git_path(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    name: &str,
) -> Result<PathBuf> {
    let bytes = git_bytes(
        runner,
        checkout,
        ["rev-parse", "--git-path", name],
        &[],
        "resolve Git path",
    )?;
    // Strip only Git's terminating newline: spaces in paths are significant.
    crate::archive::git::absolute_git_path(checkout, &bytes)
}
fn launch_path(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    session_id: &str,
) -> Result<PathBuf> {
    ensure!(
        !session_id.is_empty()
            && session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "invalid session id"
    );
    let (_, common) = crate::archive::git::git_directories(runner, checkout)?;
    Ok(common.join("mj/sessions").join(session_id).join("launch"))
}
impl LaunchRecord {
    pub fn read(
        runner: &dyn GitCommandRunner,
        checkout: &Path,
        session_id: &str,
    ) -> Result<Option<Self>> {
        let path = launch_path(runner, checkout, session_id)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let record: Self = serde_json::from_slice(&bytes).context("decode launch record")?;
        ensure!(
            record.version == 1,
            "unsupported launch record version {}",
            record.version
        );
        Ok(Some(record))
    }
}
fn build_launch_record(runner: &dyn GitCommandRunner, top_level: &Path) -> Result<LaunchRecord> {
    Ok(LaunchRecord {
        version: 1,
        refs: read_refs(runner, top_level)?,
        stash: read_stash(runner, top_level)?,
        worktrees: discover_linked_worktrees(runner, top_level)?
            .into_iter()
            .map(|w| w.relative)
            .collect(),
    })
}

fn write_launch_record_temp(
    parent: &Path,
    record: &LaunchRecord,
) -> Result<tempfile::NamedTempFile> {
    fs::create_dir_all(parent)?;
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut file, &bytes)?;
    file.as_file().sync_all()?;
    Ok(file)
}

pub fn record_launch(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    session_id: &str,
) -> Result<()> {
    let top_level = crate::archive::git::locate_checkout(runner, checkout)?.top_level;
    let record = build_launch_record(runner, &top_level)?;
    let path = launch_path(runner, &top_level, session_id)?;
    let parent = path.parent().context("launch path has no parent")?;
    let file = write_launch_record_temp(parent, &record)?;
    file.persist(&path)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn record_launch_if_absent(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    session_id: &str,
) -> Result<()> {
    let top_level = crate::archive::git::locate_checkout(runner, checkout)?.top_level;
    let path = launch_path(runner, &top_level, session_id)?;
    if path.exists() {
        return Ok(());
    }
    let record = build_launch_record(runner, &top_level)?;
    let parent = path.parent().context("launch path has no parent")?;
    let file = write_launch_record_temp(parent, &record)?;
    match file.persist_noclobber(&path) {
        Ok(_) => Ok(()),
        Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err.error).with_context(|| format!("write {}", path.display())),
    }
}

pub fn remove_launch_record(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    session_id: &str,
) -> Result<()> {
    let path = launch_path(runner, checkout, session_id)?;
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("remove {}", path.display())),
    }
    if let Some(parent) = path.parent() {
        match fs::remove_dir(parent) {
            Ok(()) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(_) => {}
        }
    }
    Ok(())
}
fn local_snapshot_ref(name: &str) -> bool {
    name.starts_with("refs/heads/")
        || name.starts_with("refs/tags/")
        || name.starts_with("refs/notes/")
        || (name.starts_with("refs/mj/")
            && !name.starts_with("refs/mj/snapshot/")
            && !name.starts_with("refs/mj/restore/")
            && !mj_core::archive::is_review_baseline_pin(name))
}
fn read_all_refs(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<BTreeMap<String, String>> {
    let text = git_text(
        runner,
        checkout,
        [
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(symref)",
            "refs/heads",
            "refs/tags",
            "refs/notes",
            "refs/mj/",
            "refs/remotes/",
        ],
    )?;
    let mut refs = BTreeMap::new();
    for line in text.lines() {
        let mut fields = line.split('\0');
        let name = fields.next().context("missing Git ref name")?;
        let oid = fields.next().context("missing Git ref object")?;
        let symbolic = fields.next().context("missing Git symbolic ref field")?;
        // Keep local symbolic aliases as ordinary refs at their resolved OID.
        if local_snapshot_ref(name) || (symbolic.is_empty() && name.starts_with("refs/remotes/")) {
            refs.insert(name.to_owned(), oid.to_owned());
        }
    }
    Ok(refs)
}
fn read_refs(runner: &dyn GitCommandRunner, checkout: &Path) -> Result<BTreeMap<String, String>> {
    Ok(read_all_refs(runner, checkout)?
        .into_iter()
        .filter(|(name, _)| local_snapshot_ref(name))
        .collect())
}
fn selected_stash(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
) -> Result<Vec<SnapshotStashEntry>> {
    if matches!(ownership, CheckoutOwnership::Managed)
        && !owns_git_directory(runner, checkout, ownership)?
    {
        return Ok(Vec::new());
    }
    let stash = read_stash(runner, checkout)?;
    let CheckoutOwnership::Raw {
        launch: Some(launch),
    } = ownership
    else {
        return Ok(stash);
    };
    let mut old = BTreeMap::<(String, String), usize>::new();
    for entry in &launch.stash {
        *old.entry((entry.commit.clone(), entry.message.clone()))
            .or_default() += 1;
    }
    // Oldest-first matching leaves newer repeated entries in the snapshot.
    let mut added = Vec::new();
    for entry in stash.into_iter().rev() {
        let count = old
            .entry((entry.commit.clone(), entry.message.clone()))
            .or_default();
        if *count > 0 {
            *count -= 1;
        } else {
            added.push(entry);
        }
    }
    added.reverse();
    Ok(added)
}
fn stash_has_untracked(runner: &dyn GitCommandRunner, checkout: &Path, oid: &str) -> Result<bool> {
    let parents = git_text(runner, checkout, ["rev-list", "--parents", "-n", "1", oid])?;
    Ok(parents.split_whitespace().count() > 3)
}
/// A stash's third parent contains untracked files. Rebuild only its tracked
/// tree and index parents, using the shared internal identity and original message.
fn tracked_stash(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    entry: &SnapshotStashEntry,
) -> Result<SnapshotStashEntry> {
    if !stash_has_untracked(runner, checkout, &entry.commit)? {
        return Ok(entry.clone());
    }
    let raw = git_bytes(
        runner,
        checkout,
        ["cat-file", "commit", &entry.commit],
        &[],
        "read stash message",
    )?;
    let message_start = raw
        .windows(2)
        .position(|bytes| bytes == b"\n\n")
        .context("stash commit has no message separator")?
        + 2;
    let mut message = raw[message_start..].to_vec();
    if !message.ends_with(b"\n") {
        message.push(b'\n');
    }
    // Distinct -u stashes can have identical tracked parents and messages.
    // Keep their identities distinct without retaining the discarded parent.
    message.extend_from_slice(format!("\nMjolnir-Source-Stash: {}\n", entry.commit).as_bytes());
    let output = runner.run(
        checkout,
        &GitCommand {
            arguments: [
                "commit-tree".to_owned(),
                format!("{}^{{tree}}", entry.commit),
                "-p".to_owned(),
                format!("{}^1", entry.commit),
                "-p".to_owned(),
                format!("{}^2", entry.commit),
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
            stdin: message,
            env: synthetic_git_commit_identity(),
        },
    )?;
    ensure!(
        output.status == 0,
        "{}",
        git_failure("strip stash untracked parent", &output)
    );
    Ok(SnapshotStashEntry {
        commit: String::from_utf8(output.stdout)?.trim().to_owned(),
        message: entry.message.clone(),
    })
}
fn read_stash(runner: &dyn GitCommandRunner, checkout: &Path) -> Result<Vec<SnapshotStashEntry>> {
    Ok(named_stash(runner, checkout)?
        .into_iter()
        .map(|(_, entry)| entry)
        .collect())
}
fn named_stash(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<Vec<(String, SnapshotStashEntry)>> {
    let bytes = git_bytes(
        runner,
        checkout,
        ["stash", "list", "--format=%gd%x00%H%x00%gs"],
        &[],
        "read stash list",
    )?;
    let mut entries = std::str::from_utf8(&bytes)?
        .lines()
        .map(|line| {
            let (name, entry) = line.split_once('\0').context("missing stash selector")?;
            let (commit, message) = entry.split_once('\0').context("malformed stash entry")?;
            Ok((
                name.to_owned(),
                SnapshotStashEntry {
                    commit: commit.to_owned(),
                    message: message.to_owned(),
                },
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let current = run_git(
        runner,
        checkout,
        ["rev-parse", "--verify", "--quiet", "refs/stash"],
        &[],
    )?;
    ensure!(
        matches!(current.status, 0 | 1),
        "{}",
        git_failure("read current stash ref", &current)
    );
    if current.status == 0 {
        let commit = std::str::from_utf8(&current.stdout)?.trim();
        if entries
            .first()
            .is_none_or(|(_, entry)| entry.commit != commit)
        {
            entries.insert(
                0,
                (
                    "refs/stash".to_owned(),
                    SnapshotStashEntry {
                        commit: commit.to_owned(),
                        message: git_text(runner, checkout, ["show", "-s", "--format=%s", commit])?,
                    },
                ),
            );
        }
    }
    Ok(entries)
}
fn read_head(runner: &dyn GitCommandRunner, checkout: &Path) -> Result<SnapshotHead> {
    let symbolic = run_git(runner, checkout, ["symbolic-ref", "-q", "HEAD"], &[])?;
    match symbolic.status {
        0 => {
            let name = String::from_utf8(symbolic.stdout)?;
            let name = name.strip_suffix('\n').unwrap_or(&name).to_owned();
            let commit = git_text(runner, checkout, ["rev-parse", "HEAD"])?;
            Ok(SnapshotHead::Branch { name, commit })
        }
        1 => Ok(SnapshotHead::Detached(git_text(
            runner,
            checkout,
            ["rev-parse", "HEAD"],
        )?)),
        _ => Err(git_failure("read symbolic HEAD", &symbolic)),
    }
}
pub(crate) fn operation(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<Option<GitOperationKind>> {
    for (name, kind) in [
        ("rebase-merge", GitOperationKind::Rebase),
        ("rebase-apply", GitOperationKind::Rebase),
        ("MERGE_HEAD", GitOperationKind::Merge),
        ("CHERRY_PICK_HEAD", GitOperationKind::CherryPick),
        ("REVERT_HEAD", GitOperationKind::Revert),
        ("sequencer", GitOperationKind::CherryPick),
        ("BISECT_LOG", GitOperationKind::Bisect),
        ("BISECT_START", GitOperationKind::Bisect),
    ] {
        let path = git_path(runner, checkout, name)?;
        if path.try_exists()? {
            if name == "rebase-apply" && path.join("applying").try_exists()? {
                return Ok(Some(GitOperationKind::Am));
            }
            if name == "sequencer" {
                let todo = fs::read_to_string(path.join("todo"))?;
                return Ok(Some(if todo.lines().any(|l| l.starts_with("revert ")) {
                    GitOperationKind::Revert
                } else {
                    kind
                }));
            }
            return Ok(Some(kind));
        }
    }
    Ok(None)
}
fn hash_file_metadata(identity: &mut blake3::Hasher, metadata: &fs::Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        identity.update(&metadata.dev().to_be_bytes());
        identity.update(&metadata.ino().to_be_bytes());
    }
    #[cfg(windows)]
    {
        // Stable Rust has no Windows file-index accessor. Git's lockfile rename
        // gives each occurrence a new creation timestamp with 100 ns precision.
        identity.update(format!("{:?}", metadata.created()?).as_bytes());
    }
    // Shown discard consent includes the displayed length, even when a writer
    // preserves the inode and restores its previous modification timestamp.
    identity.update(&metadata.len().to_be_bytes());
    let modified = metadata.modified()?;
    match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => {
            identity.update(&[0]);
            identity.update(&duration.as_nanos().to_be_bytes());
        }
        Err(error) => {
            identity.update(&[1]);
            identity.update(&error.duration().as_nanos().to_be_bytes());
        }
    }
    Ok(())
}
fn operation_record(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    key: &CheckoutKey,
) -> Result<Option<GitOperation>> {
    Ok(operation(runner, checkout)?.map(|kind| GitOperation {
        checkout: key.clone(),
        kind,
    }))
}
fn abort_operation_arguments(kind: GitOperationKind) -> [&'static str; 2] {
    match kind {
        GitOperationKind::Merge => ["merge", "--abort"],
        GitOperationKind::Rebase => ["rebase", "--abort"],
        GitOperationKind::CherryPick => ["cherry-pick", "--abort"],
        GitOperationKind::Revert => ["revert", "--abort"],
        GitOperationKind::Am => ["am", "--abort"],
        GitOperationKind::Bisect => ["bisect", "reset"],
    }
}
/// Managed callers must stop every writer and hold exclusive checkout ownership
/// from inventory/consent through capture completion. External Git writers are
/// outside this contract; raw checkouts are never aborted.
fn prepare_operation(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
    operation: Option<GitOperationKind>,
) -> Result<bool> {
    if let Some(kind) = operation {
        if matches!(ownership, CheckoutOwnership::Raw { .. }) {
            return Ok(true);
        }
        let args = abort_operation_arguments(kind);
        git_text(runner, checkout, args)?;
    }
    Ok(false)
}
fn submodule_uncommitted_state(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    inventory: &WorkspaceInventory,
) -> Result<bool> {
    if !inventory.dirty_submodules.is_empty() {
        return Ok(true);
    }
    for module in &inventory.initialized_submodules {
        let directory = checkout.join(&module.value.path);
        let pin = if let Some(link) = inventory
            .index_gitlinks
            .iter()
            .find(|link| link.value.path == module.value.path)
        {
            Some(link.value.commit.clone())
        } else {
            let parent = directory.parent().context("module has no parent")?;
            let leaf = Path::new(directory.file_name().context("module has no name")?);
            crate::checkpoint::snapshot_index_gitlinks(runner, parent)?
                .into_iter()
                .find(|link| link.path == leaf)
                .map(|link| link.commit)
        };
        if pin.as_deref() != Some(module.value.commit.as_str())
            || staged_gitlink_changes(runner, &directory)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn staged_gitlink_changes(runner: &dyn GitCommandRunner, checkout: &Path) -> Result<bool> {
    let raw = git_bytes(
        runner,
        checkout,
        [
            "diff",
            "--cached",
            "--ignore-submodules=none",
            "--raw",
            "--no-renames",
            "--no-ext-diff",
            "-z",
        ],
        &[],
        "inspect staged gitlinks",
    )?;
    let fields = raw
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let (pairs, remainder) = fields.as_chunks::<2>();
    ensure!(remainder.is_empty(), "malformed staged Git diff");
    Ok(pairs.iter().any(|pair| {
        pair[0].starts_with(b":160000 ")
            || pair[0].split(|byte| *byte == b' ').nth(1) == Some(b"160000".as_slice())
    }))
}
pub(crate) fn intent_to_add_paths(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<Vec<PathBuf>> {
    let changes = |visibility| -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        let raw = git_bytes(
            runner,
            checkout,
            [
                "diff",
                "--cached",
                "--raw",
                "--no-abbrev",
                "--no-renames",
                "--no-ext-diff",
                "--ignore-submodules=all",
                visibility,
                "-z",
            ],
            &[],
            "inspect intent-to-add index entries",
        )?;
        let fields = raw
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        let (pairs, remainder) = fields.as_chunks::<2>();
        ensure!(remainder.is_empty(), "malformed intent-to-add index diff");
        Ok(pairs
            .iter()
            .map(|pair| (pair[1].to_vec(), pair[0].to_vec()))
            .collect())
    };
    let invisible = changes("--ita-invisible-in-index")?;
    let visible = changes("--ita-visible-in-index")?;
    // Compare complete entry identities in both directions. For a HEAD path,
    // ITA can be a modification when visible and a deletion when invisible;
    // an empty HEAD blob may disappear from the visible diff altogether.
    invisible
        .keys()
        .chain(visible.keys())
        .filter(|path| invisible.get(*path) != visible.get(*path))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|path| mj_core::path_input::from_git_bytes(path))
        .collect()
}

/// A private index beside the real one preserves split-index lookup while
/// letting snapshot inspection remove gitlinks or create stashes safely.
pub(crate) fn private_index(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<tempfile::NamedTempFile> {
    let index = git_path(runner, checkout, "index")?;
    let temporary =
        tempfile::NamedTempFile::new_in(index.parent().context("index has no parent")?)?;
    if index.try_exists()? {
        fs::copy(&index, temporary.path())?;
    } else {
        fs::remove_file(temporary.path())?;
    }
    Ok(temporary)
}
fn working_state(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    skipped: bool,
) -> Result<Option<String>> {
    if skipped {
        return Ok(None);
    }
    // stash create writes its index even with optional locks disabled.
    let temporary = private_index(runner, checkout)?;
    let mut env = synthetic_git_commit_identity();
    env.push((
        "GIT_INDEX_FILE".into(),
        temporary.path().as_os_str().to_owned(),
    ));
    // Editing files needs no user identity; synthetic stash commits use ours.
    let output = runner.run(
        checkout,
        &GitCommand {
            arguments: vec!["stash".into(), "create".into()],
            env,
            ..Default::default()
        },
    )?;
    ensure!(
        output.status == 0,
        "{}",
        git_failure("capture tracked working state", &output)
    );
    let state = String::from_utf8(output.stdout)?.trim().to_owned();
    Ok((!state.is_empty()).then_some(state))
}

fn relative_local_remote(remote: &str) -> bool {
    // A colon before the first slash denotes Git's scp or transport syntax.
    // Native drive letters are handled first by Path::is_absolute.
    !Path::new(remote).is_absolute()
        && !remote
            .find(':')
            .is_some_and(|colon| remote.find('/').is_none_or(|slash| colon < slash))
}
/// Git's explicit helper syntax wraps an underlying URL (https::https://...).
fn transport_url(value: &str) -> (&str, &str) {
    let mut address = value;
    while let Some((helper, inner)) = address.split_once("::") {
        if helper.is_empty()
            || !helper
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"+.-_".contains(&byte))
        {
            break;
        }
        address = inner;
    }
    (&value[..value.len() - address.len()], address)
}
/// Strip credentials even from incomplete rewrite prefixes, which URL parsers
/// reject (for example https://user:token@ without a host).
fn redact_snapshot_url(value: &str) -> Result<String> {
    let (transport, address) = transport_url(value);
    if !transport.is_empty() {
        return Ok(format!("{transport}{}", redact_snapshot_url(address)?));
    }
    if let Ok(mut url) = url::Url::parse(value) {
        if matches!(url.scheme(), "http" | "https") {
            if url.username().is_empty() && url.password().is_none() {
                return Ok(value.to_owned());
            }
            url.set_username("")
                .map_err(|()| anyhow::anyhow!("cannot redact Git URL username"))?;
            url.set_password(None)
                .map_err(|()| anyhow::anyhow!("cannot redact Git URL password"))?;
            return Ok(url.to_string());
        }
        if url.password().is_some() {
            url.set_password(None)
                .map_err(|()| anyhow::anyhow!("cannot redact Git URL password"))?;
            return Ok(url.to_string());
        }
    }
    if let Some(scheme) = value.find("://") {
        let authority = scheme + 3;
        let end = value[authority..]
            .find(['/', '?', '#'])
            .map_or(value.len(), |offset| authority + offset);
        if let Some(at) = value[authority..end].rfind('@') {
            let userinfo = &value[authority..authority + at];
            if value[..scheme].eq_ignore_ascii_case("http")
                || value[..scheme].eq_ignore_ascii_case("https")
            {
                return Ok(format!(
                    "{}{}",
                    &value[..authority],
                    &value[authority + at + 1..]
                ));
            }
            if let Some((username, _)) = userinfo.split_once(':') {
                return Ok(format!(
                    "{}{username}@{}",
                    &value[..authority],
                    &value[authority + at + 1..]
                ));
            }
        }
    } else if !Path::new(value).is_absolute()
        && let Some(at) = value.find('@')
        && value[at + 1..].contains(':')
        && let Some((username, _)) = value[..at].split_once(':')
    {
        return Ok(format!("{username}@{}", &value[at + 1..]));
    }
    Ok(value.to_owned())
}
pub(crate) fn resolve_remote_path(checkout: &Path, remote: &str) -> Result<String> {
    let path = Path::new(remote);
    if !path.is_absolute() && !relative_local_remote(remote) {
        return Ok(remote.to_owned());
    }
    let absolute = std::path::absolute(checkout.join(path))?;
    let absolute = match fs::canonicalize(&absolute) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => absolute,
        Err(error) => return Err(error).context("resolve Git remote path"),
    };
    Ok(absolute
        .to_str()
        .context("Git remote path is not UTF-8")?
        .to_owned())
}
fn absolute_config_url(checkout: &Path, value: &str) -> Result<String> {
    if value.is_empty() || !relative_local_remote(value) {
        return Ok(value.to_owned());
    }
    // Preserve URL text for prefix rewrites; filesystem canonicalization can
    // move a symlinked destination outside its configured pushInsteadOf prefix.
    let anchor = if checkout.is_absolute() {
        checkout.to_path_buf()
    } else {
        std::path::absolute(checkout)?
    };
    let mut stored = anchor
        .join(value)
        .to_str()
        .context("Git config path is not UTF-8")?
        .to_owned();
    if value.ends_with('/') && !stored.ends_with('/') {
        stored.push('/');
    }
    Ok(stored)
}
/// Resolve .gitmodules URLs against the only remote restore recreates: origin.
pub(crate) fn resolve_submodule_url(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    url: &str,
    origin_override: Option<&str>,
) -> Result<String> {
    if !url.starts_with("./") && !url.starts_with("../") {
        return absolute_config_url(checkout, url);
    }
    let base = config_values(runner, checkout, "remote.origin.url")?;
    match origin_override.or_else(|| base.last().map(String::as_str)) {
        Some(base) => relative_submodule_url(base, url, checkout),
        None => resolve_remote_path(checkout, url),
    }
}
/// Git treats the superproject URL as a directory when resolving ../sub.git.
fn relative_submodule_url(base: &str, relative: &str, checkout: &Path) -> Result<String> {
    let is_url = base.contains("://");
    let scp_colon = (!is_url && !Path::new(base).is_absolute())
        .then(|| base.find(':'))
        .flatten();
    let mut base = if is_url || scp_colon.is_some() {
        base.to_owned()
    } else {
        absolute_config_url(checkout, base)?
    };
    #[cfg(windows)]
    if !is_url && scp_colon.is_none() {
        base = base.replace('\\', "/");
    }
    let minimum = if let Some((scheme, address)) = base.split_once("://") {
        scheme.len() + 3 + address.find('/').unwrap_or(address.len())
    } else {
        scp_colon.map_or(0, |colon| colon + 1)
    };
    base.truncate(base.trim_end_matches('/').len());
    let mut suffix = relative;
    // Git consumes only the leading ./ and ../ components. Preserve the
    // remaining suffix literally: an internal x/.. may cross a symlink.
    loop {
        if let Some(rest) = suffix.strip_prefix("./") {
            suffix = rest;
        } else if let Some(rest) = suffix.strip_prefix("../") {
            let split = base
                .rfind('/')
                .filter(|split| *split >= minimum)
                .or_else(|| {
                    scp_colon
                        .filter(|_| base.len() > minimum)
                        .map(|colon| colon + 1)
                });
            let split = split.context("relative submodule URL escapes its remote")?;
            base.truncate(split);
            suffix = rest;
        } else {
            break;
        }
    }
    let separator = if scp_colon.is_some() && base.ends_with(':') {
        ""
    } else {
        "/"
    };
    Ok(format!("{base}{separator}{suffix}"))
}

fn untracked_covered(consent: &UntrackedConsent, item: &UntrackedItem) -> bool {
    match consent {
        UntrackedConsent::None => false,
        UntrackedConsent::Any => true,
        UntrackedConsent::Shown(items) => items.contains(item),
    }
}
fn check_stash_consent(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    stash: &[SnapshotStashEntry],
    consent: &UntrackedConsent,
    key: &CheckoutKey,
) -> Result<()> {
    let mut missing = Vec::new();
    for entry in stash {
        if stash_has_untracked(runner, checkout, &entry.commit)? {
            let item = UntrackedItem::Stash {
                checkout: key.clone(),
                commit: entry.commit.clone(),
                message: entry.message.clone(),
            };
            if !untracked_covered(consent, &item) {
                missing.push(item);
            }
        }
    }
    ensure!(missing.is_empty(), SnapshotRefused::Untracked(missing));
    Ok(())
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedWorktree {
    pub relative_path: PathBuf,
    pub head: SnapshotHead,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedWorkspaceCut {
    pub refs: BTreeMap<String, String>,
    pub head: SnapshotHead,
    pub worktrees: Vec<PinnedWorktree>,
}
/// Read refs and HEADs without fetching, aborting operations, creating stashes
/// or changing anything. Branch commit IDs come from the one ref enumeration.
pub fn pin_workspace_cut(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    _ownership: &CheckoutOwnership,
) -> Result<PinnedWorkspaceCut> {
    let refs = read_all_refs(runner, checkout)?;
    let read = |path: &Path| -> Result<SnapshotHead> {
        let mut head = read_head(runner, path)?;
        if let SnapshotHead::Branch { name, commit } = &mut head {
            if let Some(value) = refs.get(name) {
                *commit = value.clone();
            } else {
                head = SnapshotHead::Detached(commit.clone());
            }
        }
        Ok(head)
    };
    let head = read(checkout)?;
    let worktrees = discover_linked_worktrees(runner, checkout)?
        .into_iter()
        .map(|tree| {
            Ok(PinnedWorktree {
                head: read(&tree.directory)?,
                relative_path: tree.relative,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(PinnedWorkspaceCut {
        refs,
        head,
        worktrees,
    })
}
struct ConsentInputs {
    untracked: Vec<UntrackedItem>,
    operations: Vec<ShownOperation>,
}
fn check_unconditional_blockers(inventory: &WorkspaceInventory) -> Result<ConsentInputs> {
    if let Some(blocker) = inventory.blockers.first() {
        return Err(blocker.kind.clone().into());
    }
    let mut untracked = Vec::new();
    let mut operations = Vec::new();
    for tree in std::iter::once(inventory).chain(inventory.worktrees.iter().map(|(_, tree)| tree)) {
        untracked.extend(tree.untracked.iter().map(|entry| UntrackedItem::Path {
            checkout: entry.checkout.clone(),
            path: entry.path.clone(),
            identity: entry.identity.clone(),
        }));
        untracked.extend(tree.nested_repositories.iter().map(|entry| {
            UntrackedItem::NestedRepository {
                checkout: entry.checkout.clone(),
                path: entry.path.clone(),
                identity: entry.identity.clone(),
            }
        }));
        untracked.extend(
            tree.untracked_stashes
                .iter()
                .map(|entry| UntrackedItem::Stash {
                    checkout: entry.checkout.clone(),
                    commit: entry.value.commit.clone(),
                    message: entry.value.message.clone(),
                }),
        );
        operations.extend(tree.operation.iter().cloned());
    }
    Ok(ConsentInputs {
        untracked,
        operations,
    })
}
fn inventory_blockers(inventory: &WorkspaceInventory) -> Result<Vec<InventoryBlocker>> {
    if inventory.in_place_only {
        return Ok(Vec::new());
    }
    let mut reasons = Vec::new();
    if !inventory.raw && inventory.origin_missing {
        reasons.push(SnapshotRefused::MissingManagedOrigin(
            inventory.checkout.clone(),
        ));
    }
    if !inventory.raw && !inventory.origins_removed_by_teardown.is_empty() {
        reasons.push(SnapshotRefused::OriginRemovedByTeardown(
            inventory.origins_removed_by_teardown.clone(),
        ));
    }
    let mut directories = Vec::new();
    let mut worktrees = Vec::new();
    for tree in std::iter::once(inventory).chain(inventory.worktrees.iter().map(|(_, tree)| tree)) {
        if !tree.tracked_keep_files.is_empty() {
            reasons.push(SnapshotRefused::TrackedKeepFiles {
                keep_dir: tree
                    .keep_dir
                    .clone()
                    .context("tracked keep inventory has no keep directory")?,
                paths: tree.tracked_keep_files.clone(),
            });
        }
        if !(tree.raw && tree.operation.is_some()) {
            if !tree.untracked_submodule_registration.is_empty() {
                reasons.push(SnapshotRefused::UntrackedSubmoduleRegistration(
                    tree.untracked_submodule_registration.clone(),
                ));
            }
            if !tree.intent_to_add.is_empty() {
                reasons.push(SnapshotRefused::IntentToAdd(tree.intent_to_add.clone()));
            }
            if !tree.hidden_tracked_edits.is_empty() {
                reasons.push(SnapshotRefused::HiddenTrackedEdits(
                    tree.hidden_tracked_edits.clone(),
                ));
            }
        }
        if !tree.raw {
            directories.extend(tree.presence.clone());
            worktrees.extend(tree.unrepresented_worktrees.clone());
        }
        if !tree.raw {
            reasons.extend(
                tree.dirty_submodules
                    .iter()
                    .cloned()
                    .map(SnapshotRefused::DirtySubmodule),
            );
        }
    }
    if !directories.is_empty() || !worktrees.is_empty() {
        reasons.push(SnapshotRefused::UnrepresentedGitState {
            directories,
            worktrees,
        });
    }
    Ok(reasons
        .into_iter()
        .map(|kind| {
            let paths = match &kind {
                SnapshotRefused::TrackedKeepFiles { paths, .. }
                | SnapshotRefused::IntentToAdd(paths)
                | SnapshotRefused::HiddenTrackedEdits(paths)
                | SnapshotRefused::UntrackedSubmoduleRegistration(paths) => paths.clone(),
                SnapshotRefused::OriginRemovedByTeardown(origins) => origins
                    .iter()
                    .map(|origin| scoped(&origin.checkout, origin.value.route.clone()))
                    .collect(),
                SnapshotRefused::DirtySubmodule(path) => vec![path.clone()],
                SnapshotRefused::MissingManagedOrigin(checkout) => {
                    vec![scoped(checkout, PathBuf::new())]
                }
                SnapshotRefused::UnrepresentedGitState {
                    directories,
                    worktrees,
                } => directories
                    .iter()
                    .cloned()
                    .chain(
                        worktrees
                            .iter()
                            .map(|item| scoped(&item.checkout, item.value.path.clone())),
                    )
                    .collect(),
                SnapshotRefused::Untracked(_) | SnapshotRefused::OperationInProgress(_) => {
                    Vec::new()
                }
            };
            InventoryBlocker {
                remedy: kind.to_string(),
                paths,
                kind,
            }
        })
        .collect())
}
fn check_consent_inputs(raw: bool, inputs: ConsentInputs, consent: &CaptureConsent) -> Result<()> {
    let ConsentInputs {
        mut untracked,
        operations,
    } = inputs;
    if raw {
        return Ok(());
    }
    untracked.retain(|entry| !untracked_covered(&consent.untracked, entry));
    ensure!(untracked.is_empty(), SnapshotRefused::Untracked(untracked));
    for operation in operations {
        let covered = match &consent.operations {
            OperationConsent::None => false,
            OperationConsent::Any => true,
            OperationConsent::Shown(shown) => shown.contains(&operation),
        };
        ensure!(covered, SnapshotRefused::OperationInProgress(operation));
    }
    Ok(())
}
pub(crate) struct OriginExclusions {
    pub objects: Vec<String>,
    pub stale: bool,
    pub notice: Option<String>,
}
fn refspec_suffix(pattern: &str, reference: &str) -> Option<String> {
    if let Some((prefix, suffix)) = pattern.split_once('*') {
        reference
            .strip_prefix(prefix)?
            .strip_suffix(suffix)
            .map(str::to_owned)
    } else {
        (pattern == reference).then(String::new)
    }
}
/// All effective source refs that could have populated a destination. Origin
/// shares the same provenance rule as other remotes; one eligible mapping is
/// not proof when another mapping can put pull/review-only history there.
fn mapped_fetch_sources(specs: &[String], destination: &str) -> std::collections::BTreeSet<String> {
    specs
        .iter()
        .filter(|spec| !spec.starts_with('^'))
        .filter_map(|spec| spec.trim_start_matches('+').split_once(':'))
        .filter_map(|(source, pattern)| {
            let suffix = refspec_suffix(pattern, destination)?;
            let source = source.replacen('*', &suffix, 1);
            (!specs
                .iter()
                .filter_map(|spec| spec.strip_prefix('^'))
                .any(|negative| refspec_suffix(negative, &source).is_some()))
            .then_some(source)
        })
        .collect()
}
pub(crate) struct OriginTracking {
    pub objects: Vec<String>,
    pub ambiguous: bool,
}
pub(crate) fn origin_tracking_objects(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    refs: &BTreeMap<String, String>,
) -> Result<OriginTracking> {
    let specs = config_values(runner, checkout, "remote.origin.fetch")?;
    let mut other_mappings = Vec::new();
    for remote in git_text(runner, checkout, ["remote"])?
        .lines()
        .filter(|remote| *remote != "origin")
    {
        other_mappings.push(config_values(
            runner,
            checkout,
            &format!("remote.{remote}.fetch"),
        )?);
    }
    let mut ambiguous = false;
    let mut objects = Vec::new();
    for (name, oid) in refs {
        let sources = mapped_fetch_sources(&specs, name);
        let eligible =
            |source: &str| source.starts_with("refs/heads/") || source.starts_with("refs/tags/");
        if name.starts_with("refs/remotes/") && sources.iter().any(|source| eligible(source)) {
            if sources.iter().any(|source| !eligible(source))
                || other_mappings
                    .iter()
                    .any(|other| !mapped_fetch_sources(other, name).is_empty())
            {
                ambiguous = true;
            } else {
                objects.push(oid.to_owned());
            }
        }
    }
    objects.sort();
    objects.dedup();
    Ok(OriginTracking { objects, ambiguous })
}
/// Ref names that must not be excluded when `remote` shares `checkout`'s
/// common Git directory (for example an originless legacy linked worktree
/// falling back to `source_repository` via `origin_override`). Otherwise
/// `ls-remote` would advertise the session's own mutable branch back to
/// capture and drop the session's commits from the pack.
fn shared_source_skip_refs(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    remote: &str,
    selected_refs: &BTreeMap<String, String>,
) -> Result<(bool, BTreeSet<String>)> {
    if remote.is_empty() {
        return Ok((false, BTreeSet::new()));
    }
    let effective = run_git(runner, checkout, ["ls-remote", "--get-url", remote], &[])?;
    if effective.status != 0 {
        return Ok((false, BTreeSet::new()));
    }
    let text = std::str::from_utf8(
        effective
            .stdout
            .strip_suffix(b"\n")
            .unwrap_or(&effective.stdout),
    )?;
    let Some(local_remote) = local_origin_route(checkout, text)? else {
        return Ok((false, BTreeSet::new()));
    };
    if !local_remote.exists() {
        return Ok((false, BTreeSet::new()));
    }
    let checkout_common_out = run_git(runner, checkout, ["rev-parse", "--git-common-dir"], &[])?;
    let remote_common_out =
        run_git(runner, &local_remote, ["rev-parse", "--git-common-dir"], &[])?;
    if checkout_common_out.status != 0 || remote_common_out.status != 0 {
        return Ok((false, BTreeSet::new()));
    }
    let checkout_common =
        crate::archive::git::absolute_git_path(checkout, &checkout_common_out.stdout)?;
    let remote_common =
        crate::archive::git::absolute_git_path(&local_remote, &remote_common_out.stdout)?;
    let checkout_canonical = fs::canonicalize(&checkout_common).unwrap_or(checkout_common);
    let remote_canonical = fs::canonicalize(&remote_common).unwrap_or(remote_common);
    if checkout_canonical == remote_canonical {
        Ok((true, selected_refs.keys().cloned().collect()))
    } else {
        Ok((false, BTreeSet::new()))
    }
}

/// Capture and dry-run share origin queries and objects-only downloads.
fn origin_exclusions(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    remote: &str,
    refs: &BTreeMap<String, String>,
    skip_shared_source_refs: &(bool, BTreeSet<String>),
) -> Result<OriginExclusions> {
    let fallback = origin_tracking_objects(runner, checkout, refs)?;
    let mut result = remote_objects(
        runner,
        checkout,
        remote,
        &fallback.objects,
        skip_shared_source_refs,
    )?;
    if result.stale && result.objects.is_empty() {
        result.notice = Some(if fallback.ambiguous {
            "snapshot carries full history because origin was unreachable and its offline refs were ambiguous"
        } else {
            "snapshot carries full history because origin was unreachable and no offline refs were available"
        }.to_owned());
        result.stale = false;
    }
    Ok(result)
}
pub(crate) fn remote_objects(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    remote: &str,
    fallback: &[String],
    skip_shared_source_refs: &(bool, BTreeSet<String>),
) -> Result<OriginExclusions> {
    let (shares_common_dir, skip_refs) = skip_shared_source_refs;
    let advertised = run_git(runner, checkout, ["ls-remote", remote], &[]);
    let mut objects = Vec::new();
    let mut stale = false;
    match advertised {
        Ok(output) if output.status == 0 => {
            let mut tips = Vec::new();
            for line in std::str::from_utf8(&output.stdout)?.lines() {
                let (oid, name) = line
                    .split_once('\t')
                    .context("malformed origin advertisement")?;
                if *shares_common_dir
                    && (skip_refs.contains(name) || name.starts_with("refs/heads/mj/"))
                {
                    continue;
                }
                if name.starts_with("refs/heads/") || name.starts_with("refs/tags/") {
                    tips.push(oid.to_owned());
                }
            }
            tips.sort();
            tips.dedup();
            let missing = missing_tips(runner, checkout, &tips)?;
            if !missing.is_empty() {
                let output = runner.run(
                    checkout,
                    &GitCommand {
                        arguments: [
                            "fetch",
                            "--no-write-fetch-head",
                            "--no-tags",
                            "--no-prune",
                            "--no-prune-tags",
                            "--refmap=",
                            "--no-recurse-submodules",
                            "--stdin",
                            remote,
                        ]
                        .into_iter()
                        .map(OsString::from)
                        .collect(),
                        stdin: format!("{}\n", missing.join("\n")).into_bytes(),
                        ..Default::default()
                    },
                );
                match output {
                    Ok(output) if output.status != 0 => tracing::warn!(
                        status = output.status,
                        "could not download advertised origin tips; packing locally missing history"
                    ),
                    Err(error) => tracing::warn!(%error,
                        "could not download advertised origin tips; packing locally missing history"),
                    _ => {}
                }
            }
            let unavailable = missing_tips(runner, checkout, &tips)?
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>();
            // A successful advertisement is authoritative even if downloading
            // a missing tip fails. Stale refs may cover force-pushed-away work.
            objects.extend(tips.into_iter().filter(|tip| !unavailable.contains(tip)));
        }
        _ => {
            // Deliberate policy even after an offline origin URL change: callers
            // warn on stale refs, and restore verifies all missing dependencies.
            objects.extend_from_slice(fallback);
            stale = true;
        }
    }
    objects.sort();
    objects.dedup();
    Ok(OriginExclusions {
        objects,
        stale,
        notice: None,
    })
}
pub(crate) fn submodule_selection_has_delta(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    mut tips: Vec<String>,
) -> Result<(bool, bool)> {
    let refs = read_all_refs(runner, checkout)?;
    tips.extend(
        refs.iter()
            .filter(|(name, _)| local_snapshot_ref(name))
            .map(|(_, oid)| oid.clone()),
    );
    tips.extend(
        read_stash(runner, checkout)?
            .into_iter()
            .map(|entry| entry.commit),
    );
    let no_skip = (false, BTreeSet::new());
    let exclusions = origin_exclusions(runner, checkout, "origin", &refs, &no_skip)?;
    tips.sort();
    tips.dedup();
    if !missing_tips(runner, checkout, &tips)?.is_empty() {
        return Ok((true, exclusions.stale));
    }
    let revisions = format!(
        "{}\n--not\n{}\n",
        tips.join("\n"),
        exclusions.objects.join("\n")
    );
    let objects = git_bytes(
        runner,
        checkout,
        ["rev-list", "--objects", "--no-object-names", "--stdin"],
        revisions.as_bytes(),
        "inspect submodule delta",
    )?;
    Ok((!objects.is_empty(), exclusions.stale))
}
fn snapshot_exclusions(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    remote: &str,
    refs: &BTreeMap<String, String>,
    selected_refs: &BTreeMap<String, String>,
    source: &ExclusionSource,
) -> Result<OriginExclusions> {
    match source {
        ExclusionSource::Origin => {
            let skip_shared = shared_source_skip_refs(runner, checkout, remote, selected_refs)?;
            origin_exclusions(runner, checkout, remote, refs, &skip_shared)
        }
        ExclusionSource::Remote(_) => {
            let skip_shared = shared_source_skip_refs(runner, checkout, remote, selected_refs)?;
            let mut result = remote_objects(runner, checkout, remote, &[], &skip_shared)?;
            if result.stale && result.objects.is_empty() {
                result.notice = Some(
                    "snapshot carries full history because the selected remote was unreachable"
                        .to_owned(),
                );
                result.stale = false;
            }
            Ok(result)
        }
        ExclusionSource::Commits(commits) => {
            let mut objects = Vec::new();
            for commit in commits {
                objects.push(
                    git_text(
                        runner,
                        checkout,
                        [
                            "rev-parse",
                            "--verify",
                            "--end-of-options",
                            &format!("{commit}^{{commit}}"),
                        ],
                    )
                    .with_context(|| format!("cutoff exclusion commit {commit} is unavailable"))?,
                );
            }
            objects.sort();
            objects.dedup();
            Ok(OriginExclusions {
                objects,
                stale: false,
                notice: None,
            })
        }
    }
}
pub(crate) fn config_values(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    key: &str,
) -> Result<Vec<String>> {
    let output = run_git(
        runner,
        checkout,
        ["config", "--null", "--get-all", key],
        &[],
    )?;
    if output.status == 1 {
        return Ok(Vec::new());
    }
    ensure!(
        output.status == 0,
        "{}",
        git_failure("read branch tracking", &output)
    );
    let bytes = output.stdout.strip_suffix(b"\0").unwrap_or(&output.stdout);
    bytes
        .split(|byte| *byte == 0)
        .map(|value| String::from_utf8(value.to_vec()).map_err(Into::into))
        .collect()
}

fn branch_tracking(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    refs: &BTreeMap<String, String>,
    cut: &PinnedWorkspaceCut,
) -> Result<BTreeMap<String, BranchTracking>> {
    let mut tracking = BTreeMap::new();
    for reference in refs.keys() {
        let Some(name) = reference.strip_prefix("refs/heads/") else {
            continue;
        };
        let is_head = |head: &SnapshotHead| matches!(head, SnapshotHead::Branch { name, .. } if name == reference);
        let path = if is_head(&cut.head) {
            Path::new("")
        } else {
            cut.worktrees
                .iter()
                .find(|tree| is_head(&tree.head))
                .map(|tree| tree.relative_path.as_path())
                .unwrap_or_else(|| Path::new(""))
        };
        let owner = checkout.join(path);
        let remote = config_values(runner, &owner, &format!("branch.{name}.remote"))?
            .into_iter()
            .last();
        let merge = config_values(runner, &owner, &format!("branch.{name}.merge"))?;
        if remote.as_deref() == Some("origin") {
            tracking.insert(name.to_owned(), BranchTracking { remote, merge });
        }
    }
    Ok(tracking)
}
// Choose once per cut: primary first, then nested checkout discovery order.

fn selected_refs(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    cut: &PinnedWorkspaceCut,
    ownership: &CheckoutOwnership,
) -> Result<BTreeMap<String, String>> {
    let legacy = matches!(ownership, CheckoutOwnership::Managed)
        && !owns_git_directory(runner, checkout, ownership)?;
    let heads = std::iter::once(&cut.head).chain(cut.worktrees.iter().map(|tree| &tree.head));
    let branches = heads
        .filter_map(|head| match head {
            SnapshotHead::Branch { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    Ok(cut
        .refs
        .iter()
        .filter(|(name, oid)| {
            local_snapshot_ref(name)
                && (branches.contains(name.as_str())
                    || match ownership {
                        CheckoutOwnership::Managed => !legacy,
                        CheckoutOwnership::Raw {
                            launch: Some(launch),
                        } => launch.refs.get(*name) != Some(*oid),
                        CheckoutOwnership::Raw { launch: None } => true,
                    })
        })
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect())
}
fn write_snapshot_pack(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    tips: &[String],
    exclusions: &[String],
    base: &Path,
) -> Result<Option<tempfile::TempPath>> {
    let mut tips = tips.to_vec();
    tips.sort();
    tips.dedup();
    let mut exclusions = exclusions.to_vec();
    exclusions.sort();
    exclusions.dedup();
    let mut revisions = tips.join("\n");
    revisions.push_str("\n--not\n");
    for oid in &exclusions {
        revisions.push_str(oid);
        revisions.push('\n');
    }
    let base = std::path::absolute(base)?;
    let parent = base.parent().context("pack has no parent")?;
    fs::create_dir_all(parent)?;
    // Every in-flight capture owns a distinct temp file in the destination
    // directory, removed on drop if packing fails or produces an empty pack.
    // Streaming via `--stdout` avoids Git creating a temp pack in the
    // repository's object store and renaming it across filesystems (#1635).
    let mut published = tempfile::Builder::new()
        .prefix(base.file_name().context("pack base has no filename")?)
        .suffix(".pack")
        .tempfile_in(parent)?;
    // A snapshot has one pack artifact; ignore repository-configured splitting.
    let output = runner.run_to_file(
        checkout,
        &GitCommand {
            arguments: vec![
                "-c".into(),
                "pack.packSizeLimit=0".into(),
                "pack-objects".into(),
                "--revs".into(),
                "--stdout".into(),
                "--quiet".into(),
            ],
            stdin: revisions.into_bytes(),
            ..Default::default()
        },
        published.as_file().try_clone()?,
    )?;
    ensure!(
        output.status == 0,
        "{}",
        git_failure("pack snapshot objects", &output)
    );
    ensure!(
        published.as_file().metadata()?.len() >= 32,
        "Git returned an invalid snapshot pack"
    );
    let mut header = [0_u8; 12];
    published.as_file_mut().seek(SeekFrom::Start(0))?;
    published.as_file_mut().read_exact(&mut header)?;
    ensure!(
        header.starts_with(b"PACK"),
        "Git returned an invalid snapshot pack"
    );
    if header[8..12] == [0, 0, 0, 0] {
        Ok(None)
    } else {
        Ok(Some(published.into_temp_path()))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaEstimate {
    pub committed_bytes: u64,
    /// Additional working-state pack bytes; unknown during an operation.
    pub working_state_bytes: Option<u64>,
    /// Combined pack bytes; unknown when tracked working state cannot be read.
    pub total_bytes: Option<u64>,
    pub remote_refs_stale: bool,
}
/// Dry run changes no ref, index, working file or stash list, and may add
/// unreferenced objects that garbage collection removes. It never aborts operations.
pub fn estimate_delta(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
    origin_override: Option<String>,
) -> Result<DeltaEstimate> {
    estimate_delta_with_source(
        runner,
        checkout,
        ownership,
        origin_override,
        &ExclusionSource::Origin,
    )
}
pub fn estimate_repository_delta(
    runner: &dyn GitCommandRunner,
    repository: &RepositoryInput,
) -> Result<DeltaEstimate> {
    estimate_delta_with_source(
        runner,
        &repository.checkout,
        &repository.ownership,
        repository.origin_override.clone(),
        &repository.exclusions,
    )
}
fn estimate_delta_with_source(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
    origin_override: Option<String>,
    source: &ExclusionSource,
) -> Result<DeltaEstimate> {
    let capture = CaptureGit::new(runner);
    let runner: &dyn GitCommandRunner = &capture;
    let history_cutoff = matches!(source, ExclusionSource::Commits(_));
    let (remote, inventory_origin) = match source {
        ExclusionSource::Commits(_) => (String::new(), None),
        ExclusionSource::Remote(url) => (
            absolute_config_url(checkout, url)?,
            Some(url.clone()),
        ),
        ExclusionSource::Origin => {
            let configured = run_git(runner, checkout, ["remote", "get-url", "origin"], &[])?;
            let remote = if configured.status == 0 {
                Some("origin".to_owned())
            } else {
                origin_override
            };
            let Some(remote) = remote else {
                ensure!(
                    matches!(ownership, CheckoutOwnership::Raw { .. }),
                    "managed snapshot requires an origin remote or source override"
                );
                return Ok(DeltaEstimate {
                    committed_bytes: 0,
                    working_state_bytes: None,
                    total_bytes: None,
                    remote_refs_stale: false,
                });
            };
            let resolved_remote = if configured.status == 0 {
                remote.clone()
            } else {
                absolute_config_url(checkout, &remote)?
            };
            (resolved_remote, Some(remote))
        }
    };
    let inventory = workspace_inventory_scoped(
        runner,
        checkout,
        ownership,
        &CheckoutKey::default(),
        inventory_origin.as_deref(),
        history_cutoff,
    )?;
    let cut = pin_workspace_cut(runner, checkout, ownership)?;
    let refs = selected_refs(runner, checkout, &cut, ownership)?;
    let exclusions = snapshot_exclusions(runner, checkout, &remote, &cut.refs, &refs, source)?;
    let mut committed: Vec<_> = refs.values().cloned().collect();
    committed.push(cut.head.commit().to_owned());
    committed.extend(
        cut.worktrees
            .iter()
            .map(|tree| tree.head.commit().to_owned()),
    );
    for entry in selected_stash(runner, checkout, ownership)? {
        committed.push(tracked_stash(runner, checkout, &entry)?.commit);
    }
    committed.sort();
    committed.dedup();
    let temp = tempfile::tempdir().context("create Git delta estimate directory")?;
    let bytes = |tips: &[String], exclusions: &[String], name: &str| -> Result<u64> {
        write_snapshot_pack(runner, checkout, tips, exclusions, &temp.path().join(name))?
            .map(|path| {
                fs::metadata(path)
                    .map(|metadata| metadata.len())
                    .map_err(Into::into)
            })
            .transpose()
            .map(|bytes| bytes.unwrap_or(0))
    };
    let committed_bytes = bytes(&committed, &exclusions.objects, "committed")?;
    if inventory.operation.is_some()
        || inventory
            .worktrees
            .iter()
            .any(|(_, inventory)| inventory.operation.is_some())
    {
        return Ok(DeltaEstimate {
            committed_bytes,
            working_state_bytes: None,
            total_bytes: None,
            remote_refs_stale: exclusions.stale
                || inventory.remote_refs_stale
                || inventory
                    .worktrees
                    .iter()
                    .any(|(_, tree)| tree.remote_refs_stale),
        });
    }
    let mut working = Vec::new();
    working.extend(working_state(runner, checkout, false)?);
    for tree in &cut.worktrees {
        working.extend(working_state(
            runner,
            &checkout.join(&tree.relative_path),
            false,
        )?);
    }
    let mut working_exclusions = exclusions.objects.clone();
    working_exclusions.extend(committed.clone());
    let working_state_bytes = if working.is_empty() {
        0
    } else {
        bytes(&working, &working_exclusions, "working")?
    };
    let mut combined = committed;
    combined.extend(working);
    let total_bytes = bytes(&combined, &exclusions.objects, "combined")?;
    Ok(DeltaEstimate {
        committed_bytes,
        working_state_bytes: Some(working_state_bytes),
        total_bytes: Some(total_bytes),
        remote_refs_stale: exclusions.stale
            || inventory.remote_refs_stale
            || inventory
                .worktrees
                .iter()
                .any(|(_, tree)| tree.remote_refs_stale),
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum ExclusionSource {
    #[default]
    Origin,
    /// Exclude objects reachable from this explicit remote URL even when
    /// `remote.origin.url` points elsewhere.
    Remote(String),
    Commits(Vec<String>),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryInput {
    pub id: String,
    pub checkout: PathBuf,
    pub ownership: CheckoutOwnership,
    pub origin_override: Option<String>,
    pub pack_path: PathBuf,
    #[serde(default)]
    pub exclusions: ExclusionSource,
    pub keep_dir: Option<PathBuf>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionCaptureConsent {
    /// Consent is scoped by repository ID, then by the path/item inside it.
    pub repositories: BTreeMap<String, CaptureConsent>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub repositories: BTreeMap<String, WorkspaceSnapshot>,
}
fn session_error(id: &str, error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<SnapshotRefused>().is_some() {
        error
    } else {
        error.context(format!("snapshot repository {id}"))
    }
}
fn repository_key(id: &str) -> CheckoutKey {
    CheckoutKey {
        repository_id: id.to_owned(),
        checkout_path: PathBuf::new(),
    }
}
fn in_place_only_mode(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
    origin_override: Option<&str>,
) -> Result<bool> {
    Ok(matches!(ownership, CheckoutOwnership::Raw { .. })
        && origin_override.is_none()
        && run_git(runner, checkout, ["remote", "get-url", "origin"], &[])?.status != 0)
}
fn in_place_only_inventory(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    key: &CheckoutKey,
) -> Result<WorkspaceInventory> {
    let notices = vec![RAW_NOTICE.to_owned()];
    Ok(WorkspaceInventory {
        notices,
        checkout: key.clone(),
        in_place_only: true,
        origin_missing: true,
        operation: operation_record(runner, checkout, key)?,
        raw: true,
        hidden_tracked_edits: crate::archive::git::hidden_tracked_paths(runner, checkout)?
            .into_iter()
            .map(|path| scoped(key, path))
            .collect(),
        untracked: Vec::new(),
        untracked_stashes: Vec::new(),
        nested_repositories: Vec::new(),
        tracked_keep_files: Vec::new(),
        keep_dir: None,
        dirty_submodules: Vec::new(),
        untracked_submodule_registration: Vec::new(),
        unrepresented_worktrees: Vec::new(),
        presence: Vec::new(),
        origins_removed_by_teardown: Vec::new(),
        uninitialized_origins: Vec::new(),
        blockers: Vec::new(),
        intent_to_add: Vec::new(),
        initialized_submodules: Vec::new(),
        index_gitlinks: Vec::new(),
        remote_refs_stale: false,
        worktrees: Vec::new(),
    })
}

/// Inventory every repository and decide consent for the complete session before
/// the first abort or other mutation. Paths inside consent remain repository-relative.
/// The caller must have stopped all writers to managed checkouts and hold their
/// exclusive ownership until capture returns. Raw checkouts are never aborted.
pub fn capture_session_snapshot(
    runner: &dyn GitCommandRunner,
    repositories: &[RepositoryInput],
    consent: &SessionCaptureConsent,
) -> Result<SessionSnapshot> {
    let capture = CaptureGit::new(runner);
    let runner: &dyn GitCommandRunner = &capture;
    let none = CaptureConsent {
        untracked: UntrackedConsent::None,
        operations: OperationConsent::None,
    };
    enum CaptureMode {
        InPlaceOnly,
        Transfer(Box<WorkspaceInventory>),
    }
    let mut inventories = BTreeMap::new();
    for repository in repositories {
        ensure!(
            !inventories.contains_key(&repository.id),
            "duplicate snapshot repository ID {}",
            repository.id
        );
        let history_cutoff = matches!(repository.exclusions, ExclusionSource::Commits(_));
        let in_place = !history_cutoff
            && in_place_only_mode(
                runner,
                &repository.checkout,
                &repository.ownership,
                repository.origin_override.as_deref(),
            )?;
        let mode = if in_place {
            CaptureMode::InPlaceOnly
        } else {
            CaptureMode::Transfer(Box::new(
                repository_inventory(runner, repository)
                    .map_err(|error| session_error(&repository.id, error))?,
            ))
        };
        inventories.insert(repository.id.clone(), mode);
    }
    let deletion_roots = session_teardown_roots(runner, repositories)?;
    for repository in repositories {
        if let CaptureMode::Transfer(inventory) = inventories
            .get_mut(&repository.id)
            .context("missing capture inventory")?
        {
            if !matches!(repository.exclusions, ExclusionSource::Commits(_)) {
                inventory_origin_dependencies(
                    runner,
                    &repository.checkout,
                    inventory,
                    repository.origin_override.as_deref(),
                    &deletion_roots,
                )?;
            }
            inventory.blockers = inventory_blockers(inventory)?;
        }
    }
    let validate = |inventories: &BTreeMap<String, CaptureMode>| -> Result<()> {
        let mut pending = Vec::new();
        for repository in repositories {
            if let CaptureMode::Transfer(inventory) = &inventories[&repository.id] {
                let inputs = check_unconditional_blockers(inventory)
                    .map_err(|error| session_error(&repository.id, error))?;
                pending.push((repository, inventory.raw, inputs));
            }
        }
        for (repository, raw, inputs) in pending {
            let repo_consent = consent.repositories.get(&repository.id).unwrap_or(&none);
            check_consent_inputs(raw, inputs, repo_consent)
                .map_err(|error| session_error(&repository.id, error))?;
            if !raw
                && let Some(keep_dir) = &repository.keep_dir
            {
                check_keep_dir_consent(
                    &repository_key(&repository.id),
                    keep_dir,
                    &repo_consent.untracked,
                )
                .map_err(|error| session_error(&repository.id, error))?;
            }
        }
        Ok(())
    };
    validate(&inventories)?;
    let mut aborted = false;
    for repository in repositories {
        let CaptureMode::Transfer(inventory) = &inventories[&repository.id] else {
            continue;
        };
        if inventory.raw {
            continue;
        }
        for (relative, tree) in std::iter::once((Path::new(""), inventory.as_ref())).chain(
            inventory
                .worktrees
                .iter()
                .map(|(path, tree)| (path.as_path(), tree)),
        ) {
            if let Some(operation) = &tree.operation {
                prepare_operation(
                    runner,
                    &repository.checkout.join(relative),
                    &repository.ownership,
                    Some(operation.kind),
                )?;
                aborted = true;
            }
        }
    }
    if aborted {
        // One session owner aborts first, then inventories the final state
        // against every teardown root. No abort prediction or stability retry.
        for repository in repositories {
            if let CaptureMode::Transfer(previous) = inventories
                .get_mut(&repository.id)
                .context("missing capture inventory")?
            {
                let mut current = repository_inventory(runner, repository)
                    .map_err(|error| session_error(&repository.id, error))?;
                if !matches!(repository.exclusions, ExclusionSource::Commits(_)) {
                    inventory_origin_dependencies(
                        runner,
                        &repository.checkout,
                        &mut current,
                        repository.origin_override.as_deref(),
                        &deletion_roots,
                    )?;
                }
                current.blockers = inventory_blockers(&current)?;
                **previous = current;
            }
        }
        validate(&inventories)?;
    }
    let mut snapshots = BTreeMap::new();
    let mut pending_packs = Vec::new();
    for repository in repositories {
        let inventory = match inventories
            .remove(&repository.id)
            .context("missing prepared inventory")?
        {
            CaptureMode::InPlaceOnly => {
                snapshots.insert(
                    repository.id.clone(),
                    capture_in_place_metadata(runner, repository)?,
                );
                continue;
            }
            CaptureMode::Transfer(inventory) => *inventory,
        };
        let snapshot = capture_prepared_workspace(
            runner,
            repository,
            consent.repositories.get(&repository.id).unwrap_or(&none),
            &repository_key(&repository.id),
            &mut pending_packs,
            inventory,
        )
        .map_err(|error| session_error(&repository.id, error))?;
        snapshots.insert(repository.id.clone(), snapshot);
    }
    for pack in &mut pending_packs {
        pack.disable_cleanup(true);
    }
    Ok(SessionSnapshot {
        repositories: snapshots,
    })
}
/// Writers in managed clones must be stopped by the caller. Raw capture uses
/// one ref enumeration and never rewrites refs, index, working files or stashes.
/// Raw callers load their session's launch record with [`LaunchRecord::read`].
/// A one-repository session, retaining direct typed workspace refusals.
pub fn capture_workspace_snapshot(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
    pack_path: &Path,
    consent: &CaptureConsent,
    origin_override: Option<String>,
) -> Result<WorkspaceSnapshot> {
    let id = "workspace".to_owned();
    let repository = RepositoryInput {
        id: id.clone(),
        checkout: checkout.to_path_buf(),
        ownership: ownership.clone(),
        origin_override,
        pack_path: pack_path.to_path_buf(),
        exclusions: ExclusionSource::Origin,
        keep_dir: None,
    };
    let consent = SessionCaptureConsent {
        repositories: BTreeMap::from([(id.clone(), consent.clone())]),
    };
    let mut session = capture_session_snapshot(runner, &[repository], &consent)?;
    session
        .repositories
        .remove(&id)
        .context("single-repository snapshot was not captured")
}

fn owns_git_directory(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
) -> Result<bool> {
    if matches!(ownership, CheckoutOwnership::Raw { .. }) {
        return Ok(false);
    }
    let (_, common) = crate::archive::git::git_directories(runner, checkout)?;
    Ok(fs::canonicalize(common)?.starts_with(fs::canonicalize(checkout)?))
}
fn capture_in_place_metadata(
    runner: &dyn GitCommandRunner,
    repository: &RepositoryInput,
) -> Result<WorkspaceSnapshot> {
    let checkout = &repository.checkout;
    let inventory = in_place_only_inventory(runner, checkout, &repository_key(&repository.id))?;
    let skipped = inventory.operation.is_some();
    let mut notices = inventory.notices;
    let status = run_git(
        runner,
        checkout,
        [
            "status",
            "--porcelain",
            "--untracked-files=no",
            "--ignore-submodules=none",
        ],
        &[],
    );
    let dirty = match status {
        Ok(output) if output.status == 0 => !output.stdout.is_empty(),
        _ => {
            tracing::warn!(checkout = %checkout.display(), "could not inspect raw in-place tracked state; checkout is kept");
            notices
                .push("tracked state could not be inspected; the raw checkout is kept".to_owned());
            true
        }
    };
    let hidden = inventory.hidden_tracked_edits;
    Ok(WorkspaceSnapshot {
        metadata: WorkspaceSnapshotMetadata {
            object_format: git_text(runner, checkout, ["rev-parse", "--show-object-format"])?,
            origin_url: None,
            notices,
            head: read_head(runner, checkout)?,
            refs: BTreeMap::new(),
            branch_tracking: BTreeMap::new(),
            stash: Vec::new(),
            working_state: None,
            working_state_skipped: skipped,
            has_uncommitted_state: skipped || dirty || !hidden.is_empty(),
            worktrees: Vec::new(),
            initialized_submodules: Vec::new(),
            index_gitlinks: Vec::new(),
            remote_refs_stale: false,
            prerequisites: Vec::new(),
            history_cutoff: false,
        },
        pack: None,
    })
}

fn capture_prepared_workspace(
    runner: &dyn GitCommandRunner,
    repository: &RepositoryInput,
    consent: &CaptureConsent,
    key: &CheckoutKey,
    pending_packs: &mut Vec<tempfile::TempPath>,
    inventory: WorkspaceInventory,
) -> Result<WorkspaceSnapshot> {
    let checkout = &repository.checkout;
    let ownership = &repository.ownership;
    let history_cutoff = matches!(repository.exclusions, ExclusionSource::Commits(_));
    let (origin_url, remote) = match &repository.exclusions {
        ExclusionSource::Commits(_) => (None, String::new()),
        ExclusionSource::Remote(url) => {
            let remote = absolute_config_url(checkout, url)?;
            (Some(redact_snapshot_url(url)?), remote)
        }
        ExclusionSource::Origin => {
            let configured = config_values(runner, checkout, "remote.origin.url")?
                .into_iter()
                .next();
            let origin_url = configured
                .clone()
                .or_else(|| repository.origin_override.clone());
            let Some(origin_url) = origin_url else {
                return capture_in_place_metadata(runner, repository);
            };
            let remote = if configured.is_some() {
                "origin".to_owned()
            } else {
                absolute_config_url(checkout, &origin_url)?
            };
            (Some(redact_snapshot_url(&origin_url)?), remote)
        }
    };
    let cut = pin_workspace_cut(runner, checkout, ownership)?;
    let refs = selected_refs(runner, checkout, &cut, ownership)?;
    let exclusions = snapshot_exclusions(
        runner,
        checkout,
        &remote,
        &cut.refs,
        &refs,
        &repository.exclusions,
    )?;
    let mut notices = inventory.notices.clone();
    notices.extend(exclusions.notice.clone());
    let original_stash = selected_stash(runner, checkout, ownership)?;
    if !inventory.raw {
        check_stash_consent(runner, checkout, &original_stash, &consent.untracked, key)?;
    }
    let stash = original_stash
        .iter()
        .map(|entry| tracked_stash(runner, checkout, entry))
        .collect::<Result<Vec<_>>>()?;
    let skipped = inventory.raw && inventory.operation.is_some();
    let working = working_state(runner, checkout, skipped)?;
    let head = head_for_working_state(runner, checkout, cut.head.clone(), &working)?;
    let mut tips = vec![head.commit().to_owned()];
    tips.extend(refs.values().cloned());
    tips.extend(stash.iter().map(|entry| entry.commit.clone()));
    tips.extend(working.clone());
    let mut worktrees = Vec::new();
    for tree in &cut.worktrees {
        let directory = checkout.join(&tree.relative_path);
        let own = &inventory
            .worktrees
            .iter()
            .find(|(path, _)| path == &tree.relative_path)
            .context("missing checkout inventory")?
            .1;
        let skipped = inventory.raw && own.operation.is_some();
        let state = working_state(runner, &directory, skipped)?;
        let head = head_for_working_state(runner, &directory, tree.head.clone(), &state)?;
        tips.push(head.commit().to_owned());
        tips.extend(state.clone());
        worktrees.push(SnapshotWorktree {
            has_uncommitted_state: state.is_some()
                || skipped
                || staged_gitlink_changes(runner, &directory)?
                || submodule_uncommitted_state(runner, &directory, own)?,
            relative_path: tree.relative_path.clone(),
            head,
            working_state: state,
            working_state_skipped: skipped,
            initialized_submodules: own
                .initialized_submodules
                .iter()
                .map(|entry| entry.value.clone())
                .collect(),
            index_gitlinks: own
                .index_gitlinks
                .iter()
                .map(|entry| entry.value.clone())
                .collect(),
        });
    }
    let prerequisites = pack_prerequisites(runner, checkout, &tips, &exclusions.objects)?;
    // Pack against the recorded prerequisites, not every exclusion. An
    // exclusion that is not an ancestor of the saved state can share blobs
    // with it; restore never checks for such a tip, so those objects must
    // travel in the pack.
    let owned = write_snapshot_pack(
        runner,
        checkout,
        &tips,
        &prerequisites,
        &repository.pack_path,
    )?;
    let pack = owned.as_ref().map(|path| path.to_path_buf());
    pending_packs.extend(owned);
    Ok(WorkspaceSnapshot {
        metadata: WorkspaceSnapshotMetadata {
            object_format: git_text(runner, checkout, ["rev-parse", "--show-object-format"])?,
            origin_url,
            notices,
            head,
            refs: refs.clone(),
            branch_tracking: if history_cutoff {
                BTreeMap::new()
            } else {
                branch_tracking(runner, checkout, &refs, &cut)?
            },
            stash,
            has_uncommitted_state: working.is_some()
                || skipped
                || staged_gitlink_changes(runner, checkout)?
                || submodule_uncommitted_state(runner, checkout, &inventory)?,
            working_state: working,
            working_state_skipped: skipped,
            worktrees,
            initialized_submodules: inventory
                .initialized_submodules
                .into_iter()
                .map(|entry| entry.value)
                .collect(),
            index_gitlinks: inventory
                .index_gitlinks
                .into_iter()
                .map(|entry| entry.value)
                .collect(),
            remote_refs_stale: exclusions.stale
                || inventory.remote_refs_stale
                || inventory
                    .worktrees
                    .iter()
                    .any(|(_, tree)| tree.remote_refs_stale),
            prerequisites,
            history_cutoff,
        },
        pack,
    })
}
fn head_for_working_state(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    head: SnapshotHead,
    state: &Option<String>,
) -> Result<SnapshotHead> {
    if let Some(state) = state {
        let parent = git_text(runner, checkout, ["rev-parse", &format!("{state}^1")])?;
        if parent != head.commit() {
            return Ok(SnapshotHead::Detached(parent));
        }
    }
    Ok(head)
}
fn pack_prerequisites(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    tips: &[String],
    exclusions: &[String],
) -> Result<Vec<String>> {
    let mut omitted = tips
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut input = tips.join("\n");
    input.push_str("\n--not\n");
    input.push_str(&exclusions.join("\n"));
    input.push('\n');
    let output = git_bytes(
        runner,
        checkout,
        [
            "rev-list",
            "--objects",
            "--no-object-names",
            "--boundary",
            "--stdin",
        ],
        input.as_bytes(),
        "record pack prerequisites",
    )?;
    let mut boundary = std::collections::BTreeSet::new();
    for line in std::str::from_utf8(&output)?.lines() {
        if let Some(oid) = line.strip_prefix('-') {
            boundary.insert(oid.to_owned());
        } else {
            omitted.remove(line);
        }
    }
    boundary.extend(omitted);
    Ok(boundary.into_iter().collect())
}

fn nested_repository_inventory(root: &Path) -> Result<(u64, FileIdentity)> {
    let mut pending = vec![(root.to_path_buf(), PathBuf::new())];
    let mut identity = blake3::Hasher::new();
    let mut bytes = 0_u64;
    while let Some((path, relative)) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect nested repository {}", path.display()))?;
        let encoded = relative.as_os_str().as_encoded_bytes();
        identity.update(&(encoded.len() as u64).to_be_bytes());
        identity.update(encoded);
        identity.update(file_identity(&metadata)?.as_str().as_bytes());
        if metadata.is_dir() {
            let mut children = fs::read_dir(&path)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()?;
            children.sort();
            for child in children.into_iter().rev() {
                let name = child.file_name().context("nested path has no name")?;
                pending.push((child.clone(), relative.join(name)));
            }
        } else {
            bytes = bytes
                .checked_add(metadata.len())
                .context("nested repository byte size overflow")?;
        }
    }
    Ok((
        bytes,
        FileIdentity::new(identity.finalize().to_hex().to_string()),
    ))
}
fn keep_relative(repository: &RepositoryInput) -> Result<Option<PathBuf>> {
    let Some(keep) = &repository.keep_dir else {
        return Ok(None);
    };
    let root = std::path::absolute(&repository.checkout)?;
    let keep = std::path::absolute(root.join(keep))?;
    let relative = keep
        .strip_prefix(&root)
        .context("keep directory is outside its repository")?
        .to_path_buf();
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
        "keep directory must be a nonempty path inside its repository"
    );
    Ok(Some(relative))
}
fn tracked_keep_paths(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    key: &CheckoutKey,
    keep: &Path,
    state: Option<&str>,
) -> Result<Vec<Scoped<PathBuf>>> {
    let bytes = if let Some(state) = state {
        git_bytes(
            runner,
            checkout,
            [
                "ls-tree",
                "-r",
                "-z",
                "--name-only",
                &format!("{state}^{{tree}}"),
            ],
            &[],
            "inspect keep files in working state",
        )?
    } else {
        git_bytes(
            runner,
            checkout,
            ["ls-files", "-z"],
            &[],
            "inspect tracked keep files",
        )?
    };
    let mut paths = Vec::new();
    for path in bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = mj_core::path_input::from_git_bytes(path)?;
        if key.checkout_path.join(&path).starts_with(keep) {
            paths.push(scoped(key, path));
        }
    }
    paths.sort_by(|left, right| left.value.cmp(&right.value));
    paths.dedup();
    Ok(paths)
}

fn strip_keep_paths_from_untracked(inventory: &mut WorkspaceInventory, keep: &Path) {
    let local_keep = match keep.strip_prefix(&inventory.checkout.checkout_path) {
        Ok(path) if !path.as_os_str().is_empty() => path,
        _ if inventory.checkout.checkout_path.starts_with(keep) => Path::new("."),
        _ => return,
    };
    inventory.untracked.retain(|entry| {
        if local_keep == Path::new(".") {
            false
        } else {
            !entry.path.starts_with(local_keep)
        }
    });
}

fn repository_inventory(
    runner: &dyn GitCommandRunner,
    repository: &RepositoryInput,
) -> Result<WorkspaceInventory> {
    let history_cutoff = matches!(repository.exclusions, ExclusionSource::Commits(_));
    let mut inventory = workspace_inventory_scoped(
        runner,
        &repository.checkout,
        &repository.ownership,
        &repository_key(&repository.id),
        if history_cutoff {
            None
        } else {
            repository.origin_override.as_deref()
        },
        history_cutoff,
    )?;
    if inventory.in_place_only {
        return Ok(inventory);
    }
    if !inventory.raw
        && let Some(keep) = keep_relative(repository)?
    {
        inventory.keep_dir = repository.keep_dir.clone();
        // `.mj/keep/<session-id>/` files are owned exclusively by
        // `scan_keep_dir` / `keep_dir_entries` / `check_keep_dir_consent` so
        // preflight and dialogs never double-count them in both `untracked`
        // and `keep_dir_entries`.
        strip_keep_paths_from_untracked(&mut inventory, &keep);
        inventory.tracked_keep_files = tracked_keep_paths(
            runner,
            &repository.checkout,
            &inventory.checkout,
            &keep,
            None,
        )?;
        inventory.tracked_keep_files.extend(tracked_keep_paths(
            runner,
            &repository.checkout,
            &inventory.checkout,
            &keep,
            Some("HEAD"),
        )?);
        inventory
            .tracked_keep_files
            .sort_by(|left, right| left.value.cmp(&right.value));
        inventory.tracked_keep_files.dedup();
        for (path, nested) in &mut inventory.worktrees {
            let directory = repository.checkout.join(path.as_path());
            nested.keep_dir = repository.keep_dir.clone();
            strip_keep_paths_from_untracked(nested, &keep);
            nested.tracked_keep_files =
                tracked_keep_paths(runner, &directory, &nested.checkout, &keep, None)?;
            nested.tracked_keep_files.extend(tracked_keep_paths(
                runner,
                &directory,
                &nested.checkout,
                &keep,
                Some("HEAD"),
            )?);
            nested
                .tracked_keep_files
                .sort_by(|left, right| left.value.cmp(&right.value));
            nested.tracked_keep_files.dedup();
        }
    }
    inventory.blockers = inventory_blockers(&inventory)?;
    Ok(inventory)
}
pub fn workspace_inventory(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
) -> Result<WorkspaceInventory> {
    let capture = CaptureGit::new(runner);
    let runner: &dyn GitCommandRunner = &capture;
    workspace_inventory_scoped(
        runner,
        checkout,
        ownership,
        &CheckoutKey::default(),
        None,
        false,
    )
}
pub fn session_inventory(
    runner: &dyn GitCommandRunner,
    repositories: &[RepositoryInput],
) -> Result<BTreeMap<String, WorkspaceInventory>> {
    let capture = CaptureGit::new(runner);
    let runner: &dyn GitCommandRunner = &capture;
    let mut result = BTreeMap::new();
    for repository in repositories {
        ensure!(
            !result.contains_key(&repository.id),
            "duplicate snapshot repository ID {}",
            repository.id
        );
        result.insert(
            repository.id.clone(),
            repository_inventory(runner, repository)?,
        );
    }
    let deletion_roots = session_teardown_roots(runner, repositories)?;
    for repository in repositories {
        let inventory = result
            .get_mut(&repository.id)
            .context("missing session inventory")?;
        if !matches!(repository.exclusions, ExclusionSource::Commits(_)) {
            inventory_origin_dependencies(
                runner,
                &repository.checkout,
                inventory,
                repository.origin_override.as_deref(),
                &deletion_roots,
            )?;
        }
        inventory.blockers = inventory_blockers(inventory)?;
    }
    Ok(result)
}

/// Read-only suspend preflight. Final capture independently evaluates the same
/// policy against the quiesced checkout.
pub fn require_managed_snapshot_without_consent(
    runner: &dyn GitCommandRunner,
    repository: &RepositoryInput,
) -> Result<()> {
    let inventories = session_inventory(runner, std::slice::from_ref(repository))?;
    let inventory = inventories
        .get(&repository.id)
        .context("snapshot inventory is missing")?;
    let inputs = check_unconditional_blockers(inventory)?;
    check_consent_inputs(
        false,
        inputs,
        &CaptureConsent {
            untracked: UntrackedConsent::None,
            operations: OperationConsent::None,
        },
    )
}

fn teardown_roots(runner: &dyn GitCommandRunner, checkout: &Path) -> Result<Vec<PathBuf>> {
    Ok(vec![
        fs::canonicalize(checkout)?,
        fs::canonicalize(crate::archive::git::git_directories(runner, checkout)?.0)?,
    ])
}
fn session_teardown_roots(
    runner: &dyn GitCommandRunner,
    repositories: &[RepositoryInput],
) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    for repository in repositories {
        if matches!(repository.ownership, CheckoutOwnership::Managed) {
            roots.extend(teardown_roots(runner, &repository.checkout)?);
        }
    }
    Ok(roots)
}
fn local_file_origin_route(address: &str) -> Result<PathBuf> {
    // Git decodes the entire file URL path literally, including dot segments,
    // question marks and backslashes on Unix. Url::parse normalizes those.
    #[cfg(windows)]
    let address = address.replace('\\', "/");
    let after_scheme = address
        .split_once("://")
        .context("invalid local file origin")?
        .1;
    let (authority, path) = after_scheme
        .split_once('/')
        .context("local file origin has no path")?;
    let bytes = percent_encoding::percent_decode_str(&format!("/{path}")).collect::<Vec<_>>();
    ensure!(
        !bytes.contains(&0),
        "file origin contains an invalid NUL path byte"
    );
    #[cfg(windows)]
    {
        let path = std::str::from_utf8(&bytes).context("local file origin path is not UTF-8")?;
        let drive = |value: &str| {
            value.len() >= 2
                && value.as_bytes()[0].is_ascii_alphabetic()
                && value.as_bytes()[1] == b':'
        };
        let native = if drive(authority) {
            format!("{authority}{path}")
        } else if path.starts_with('/') && drive(&path[1..]) {
            path[1..].to_owned()
        } else if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
            format!("//{authority}{path}")
        } else {
            path.to_owned()
        };
        Ok(PathBuf::from(native))
    }
    #[cfg(not(windows))]
    {
        let _ = authority;
        mj_core::path_input::from_git_bytes(&bytes)
    }
}

fn origin_route_is_removed(
    route: &Path,
    roots: &[PathBuf],
    inspected: &mut std::collections::BTreeSet<PathBuf>,
) -> Result<bool> {
    let route = std::path::absolute(route)?;
    if !inspected.insert(route.clone()) {
        return Ok(false);
    }
    let mut prefix = PathBuf::new();
    for component in route.components() {
        prefix.push(component.as_os_str());
        match fs::canonicalize(&prefix) {
            Ok(path) if roots.iter().any(|root| path.starts_with(root)) => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("resolve origin route component"),
        }
        match fs::symlink_metadata(&prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = fs::read_link(&prefix)?;
                let target = prefix
                    .parent()
                    .context("origin symlink has no parent")?
                    .join(target);
                if origin_route_is_removed(&target, roots, inspected)? {
                    return Ok(true);
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect origin route component"),
        }
    }
    Ok(false)
}

pub(crate) fn local_origin_route(repository: &Path, text: &str) -> Result<Option<PathBuf>> {
    let (transport, address) = transport_url(text);
    if !transport.is_empty()
        && transport
            .split("::")
            .filter(|part| !part.is_empty())
            .any(|part| part != "file")
    {
        return Ok(None);
    }
    if address
        .split_once("://")
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("file"))
    {
        Ok(Some(local_file_origin_route(address)?))
    } else if Path::new(address).is_absolute() || relative_local_remote(address) {
        Ok(Some(repository.join(address)))
    } else {
        Ok(None)
    }
}

fn teardown_path_dependency(absolute: &Path, roots: &[PathBuf]) -> Result<Option<(PathBuf, bool)>> {
    let mut cursor = absolute;
    let mut missing = Vec::new();
    let mut canonical = loop {
        match fs::canonicalize(cursor) {
            Ok(path) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    cursor
                        .components()
                        .next_back()
                        .context("local origin has no component")?
                        .as_os_str()
                        .to_owned(),
                );
                cursor = cursor
                    .parent()
                    .context("local origin has no existing ancestor")?;
            }
            Err(error) => return Err(error).context("resolve local origin dependency"),
        }
    };
    for component in missing.into_iter().rev() {
        if component == ".." {
            canonical.pop();
        } else if component != "." {
            canonical.push(component);
        }
    }
    let target_is_removed = roots.iter().any(|root| canonical.starts_with(root));
    if target_is_removed || origin_route_is_removed(absolute, roots, &mut Default::default())? {
        canonical
            .to_str()
            .context("origin dependency must be UTF-8; rename it before suspending")?;
        Ok(Some((canonical, target_is_removed)))
    } else {
        Ok(None)
    }
}

/// Native Git expands recursive object providers. Retain literal alternates
/// routes too: canonical provider names alone hide symlinks through teardown.
enum LocalOriginProviders {
    Unavailable(String),
    Ready {
        providers: Vec<PathBuf>,
        shallow: Option<PathBuf>,
    },
}

fn local_origin_object_providers(
    runner: &dyn GitCommandRunner,
    context: &Path,
    origin: &Path,
) -> Result<LocalOriginProviders> {
    let marker = if origin.is_dir() && origin.join(".git").try_exists()? {
        origin.join(".git")
    } else {
        origin.to_path_buf()
    };
    let output = runner.run(
        context,
        &GitCommand {
            arguments: vec![
                "rev-parse".into(),
                "--resolve-git-dir".into(),
                marker.as_os_str().to_owned(),
            ],
            ..Default::default()
        },
    )?;
    if output.status != 0 {
        return Ok(LocalOriginProviders::Unavailable(format!(
            "{:#}",
            git_failure("resolve local origin Git directory", &output)
        )));
    }
    let private = crate::archive::git::absolute_git_path(context, &output.stdout)?;
    let query = |args: &[&str]| -> Result<Vec<u8>> {
        let mut arguments = vec!["--git-dir".into(), private.as_os_str().to_owned()];
        arguments.extend(args.iter().map(OsString::from));
        let output = runner.run(
            context,
            &GitCommand {
                arguments,
                ..Default::default()
            },
        )?;
        ensure!(
            output.status == 0,
            "{}",
            git_failure("inspect local origin object providers", &output)
        );
        Ok(output.stdout)
    };
    let shallow = if query(&["rev-parse", "--is-shallow-repository"])? == b"true\n" {
        Some(crate::archive::git::absolute_git_path(
            context,
            &query(&["rev-parse", "--git-path", "shallow"])?,
        )?)
    } else {
        None
    };
    let common = crate::archive::git::absolute_git_path(
        context,
        &query(&["rev-parse", "--git-common-dir"])?,
    )?;
    let objects = crate::archive::git::absolute_git_path(
        context,
        &query(&["rev-parse", "--git-path", "objects"])?,
    )?;
    let listing = query(&["count-objects", "-v"])?;
    let mut object_directories = std::collections::BTreeSet::from([objects.clone()]);
    for line in listing.split(|byte| *byte == b'\n') {
        if let Some(path) = line.strip_prefix(b"alternate: ") {
            object_directories.insert(context.join(crate::archive::git::unquote_git_path(path)?));
        }
    }
    let mut providers =
        std::collections::BTreeSet::from([marker, private.clone(), common, objects]);
    if private.join("commondir").try_exists()? {
        providers.insert(private.join("commondir"));
    }
    for directory in object_directories {
        providers.insert(directory.clone());
        let alternates = directory.join("info/alternates");
        let entries = match fs::read(&alternates) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("read local origin alternates routes"),
        };
        providers.insert(alternates.clone());
        for path in entries
            .split(|byte| *byte == b'\n')
            .filter(|path| !path.is_empty())
        {
            providers.insert(directory.join(
                crate::archive::git::unquote_git_path(path).with_context(|| {
                    format!("invalid alternate route in {}", alternates.display())
                })?,
            ));
        }
    }
    Ok(LocalOriginProviders::Ready {
        providers: providers.into_iter().collect(),
        shallow,
    })
}

fn inventory_origin_dependencies(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    inventory: &mut WorkspaceInventory,
    origin_override: Option<&str>,
    roots: &[PathBuf],
) -> Result<()> {
    inventory.origins_removed_by_teardown.clear();
    if inventory.raw {
        return Ok(());
    }
    let mut repositories = vec![(
        checkout.to_path_buf(),
        inventory.checkout.clone(),
        origin_override,
    )];
    for (path, tree) in std::iter::once((Path::new(""), &*inventory)).chain(
        inventory
            .worktrees
            .iter()
            .map(|(path, tree)| (path.as_path(), tree)),
    ) {
        for module in &tree.initialized_submodules {
            let key = CheckoutKey {
                repository_id: tree.checkout.repository_id.clone(),
                checkout_path: tree.checkout.checkout_path.join(&module.value.path),
            };
            repositories.push((checkout.join(path).join(&module.value.path), key, None));
        }
    }
    let mut candidates = Vec::new();
    for (_, tree) in std::iter::once((Path::new(""), &*inventory)).chain(
        inventory
            .worktrees
            .iter()
            .map(|(path, tree)| (path.as_path(), tree)),
    ) {
        for origin in &tree.uninitialized_origins {
            candidates.push((
                CheckoutKey {
                    repository_id: tree.checkout.repository_id.clone(),
                    checkout_path: tree.checkout.checkout_path.join(&origin.owner),
                },
                Some(origin.config_key.clone()),
                origin.route.clone(),
            ));
        }
    }
    for (repository, key, override_url) in repositories {
        let mut output = run_git(runner, &repository, ["remote", "get-url", "origin"], &[])?;
        if output.status != 0 {
            let Some(override_url) = override_url else {
                continue;
            };
            let literal_override = absolute_config_url(&repository, override_url)?;
            output = run_git(
                runner,
                &repository,
                ["ls-remote", "--get-url", &literal_override],
                &[],
            )?;
        }
        ensure!(
            output.status == 0,
            "{}",
            git_failure("resolve effective origin", &output)
        );
        let text =
            std::str::from_utf8(output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout))?;
        if let Some(local) = local_origin_route(&repository, text)? {
            candidates.push((key, None, local));
        }
    }
    let mut provider_cache = BTreeMap::new();
    for (key, module_url_key, local) in candidates {
        let absolute = std::path::absolute(&local)?;
        if let Some((canonical, target_is_removed)) = teardown_path_dependency(&absolute, roots)? {
            inventory.origins_removed_by_teardown.push(scoped(
                &key,
                OriginDependency {
                    module_url_key,
                    object_provider: None,
                    route: absolute,
                    canonical_path: canonical,
                    target_is_removed,
                },
            ));
            continue;
        }
        if !absolute.try_exists()? {
            continue;
        }
        let providers = match provider_cache.entry(absolute.clone()) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let providers = match local_origin_object_providers(runner, checkout, &absolute).with_context(|| format!("cannot inspect local origin object storage at {}; repair its objects/info/alternates, or make it independent with `git -C {} repack -a -d` and remove its alternates file, then retry", absolute.display(), mj_core::targets::posix_quote(&absolute.to_string_lossy())))? {
                    LocalOriginProviders::Ready { providers, shallow } => {
                        // A local shallow server advertises tips without promising
                        // their parents. Never exclude using our fuller source graph.
                        // Hosted origins retain the agreed complete-origin assumption.
                        if let Some(boundary) = shallow { inventory.presence.push(scoped(&key, boundary)); }
                        providers
                    },
                    LocalOriginProviders::Unavailable(error) => {
                        // Only an absent/unresolvable repository uses the
                        // existing offline policy. Errors after resolution
                        // must never masquerade as an empty provider set.
                        inventory.notices.push(format!("could not inspect local origin object storage at {}: {error}", absolute.display()));
                        Vec::new()
                    }
                };
                entry.insert(providers)
            }
        };
        for provider in providers {
            if teardown_path_dependency(provider, roots)?.is_some() {
                inventory.origins_removed_by_teardown.push(scoped(
                    &key,
                    OriginDependency {
                        module_url_key: module_url_key.clone(),
                        object_provider: Some(provider.clone()),
                        route: absolute.clone(),
                        canonical_path: fs::canonicalize(&absolute)?,
                        target_is_removed: false,
                    },
                ));
            }
        }
    }
    Ok(())
}

fn workspace_inventory_scoped(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
    key: &CheckoutKey,
    origin_override: Option<&str>,
    history_cutoff: bool,
) -> Result<WorkspaceInventory> {
    if !history_cutoff && in_place_only_mode(runner, checkout, ownership, origin_override)? {
        return in_place_only_inventory(runner, checkout, key);
    }
    fn collect(
        runner: &dyn GitCommandRunner,
        checkout: &Path,
        raw: bool,
        nested: &[PathBuf],
        key: &CheckoutKey,
        origin_override: Option<&str>,
        history_cutoff: bool,
    ) -> Result<WorkspaceInventory> {
        let bytes = git_bytes(
            runner,
            checkout,
            ["ls-files", "--others", "--exclude-standard", "-z"],
            &[],
            "list untracked files",
        )?;
        let submodules = crate::checkpoint::inspect_snapshot_submodules(
            runner,
            checkout,
            origin_override,
            !raw && !history_cutoff,
        )?;
        for path in submodules
            .index_gitlinks
            .iter()
            .map(|link| &link.path)
            .chain(submodules.initialized.iter().map(|module| &module.path))
        {
            crate::archive::slash_path(path).with_context(|| {
                format!(
                    "snapshot paths must be UTF-8; rename {} before suspending",
                    path.display()
                )
            })?;
        }

        let mut paths = bytes
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(mj_core::path_input::from_git_bytes)
            .collect::<Result<Vec<_>>>()?;
        paths.extend(submodules.untracked);
        paths.sort();
        paths.dedup();
        let mut untracked = Vec::new();
        let mut nested_repositories = Vec::new();
        for path in &submodules.unregistered {
            if nested.iter().any(|nested| path.starts_with(nested)) {
                continue;
            }
            let directory = checkout.join(path);
            if directory.try_exists()? {
                let (bytes, identity) = nested_repository_inventory(&directory)?;
                nested_repositories.push(UntrackedPath {
                    checkout: key.clone(),
                    path: path.clone(),
                    bytes,
                    identity,
                });
            }
        }
        let mut intent = intent_to_add_paths(runner, checkout)?
            .into_iter()
            .map(|path| scoped(key, path))
            .collect::<Vec<_>>();
        intent.extend(
            submodules
                .intent_to_add
                .into_iter()
                .map(|(module, path)| Scoped {
                    checkout: CheckoutKey {
                        repository_id: key.repository_id.clone(),
                        checkout_path: key.checkout_path.join(module),
                    },
                    value: path,
                }),
        );
        let mut hidden = crate::archive::git::hidden_tracked_paths(runner, checkout)?;
        hidden.extend(submodules.hidden_tracked_edits);
        for path in paths {
            if nested.iter().any(|nested| path.starts_with(nested))
                || nested_repositories
                    .iter()
                    .any(|repository| path.starts_with(&repository.path))
            {
                continue;
            }
            let metadata = fs::symlink_metadata(checkout.join(&path))
                .with_context(|| format!("inspect {}", path.display()))?;
            let (bytes, identity) = if metadata.is_dir() {
                // Git collapses an embedded untracked repository to one path.
                // Consent must cover its files, not just the directory inode.
                nested_repository_inventory(&checkout.join(&path))?
            } else {
                (metadata.len(), file_identity(&metadata)?)
            };
            untracked.push(UntrackedPath {
                checkout: key.clone(),
                identity,
                path,
                bytes,
            });
        }
        Ok(WorkspaceInventory {
            notices: Vec::new(),
            checkout: key.clone(),
            untracked,
            untracked_stashes: Vec::new(),
            nested_repositories,
            tracked_keep_files: Vec::new(),
            keep_dir: None,
            untracked_submodule_registration: submodules
                .untracked_registration
                .into_iter()
                .map(|path| scoped(key, path))
                .collect(),
            dirty_submodules: {
                let mut dirty: Vec<_> = submodules
                    .dirty
                    .into_iter()
                    .map(|path| scoped(key, path))
                    .collect();
                if history_cutoff && !raw {
                    for module in &submodules.initialized {
                        let entry = scoped(key, module.path.clone());
                        if !dirty.contains(&entry) {
                            dirty.push(entry);
                        }
                    }
                }
                dirty
            },
            initialized_submodules: submodules
                .initialized
                .into_iter()
                .map(|value| scoped(key, value))
                .collect(),
            index_gitlinks: submodules
                .index_gitlinks
                .into_iter()
                .map(|value| scoped(key, value))
                .collect(),
            unrepresented_worktrees: Vec::new(),
            presence: Vec::new(),
            origins_removed_by_teardown: Vec::new(),
            uninitialized_origins: submodules.uninitialized_origins,
            blockers: Vec::new(),
            in_place_only: false,
            origin_missing: !history_cutoff
                && origin_override.is_none()
                && run_git(runner, checkout, ["remote", "get-url", "origin"], &[])?.status != 0,
            intent_to_add: intent,
            hidden_tracked_edits: hidden.into_iter().map(|path| scoped(key, path)).collect(),
            operation: operation_record(runner, checkout, key)?,
            raw,
            remote_refs_stale: submodules.remote_refs_stale,
            worktrees: Vec::new(),
        })
    }
    // Restore shares the primary checkout's anchored origin with its linked
    // worktrees, even when their source config contains a relative URL.
    let restore_origin = if history_cutoff {
        None
    } else {
        let primary_origin = config_values(runner, checkout, "remote.origin.url")?;
        primary_origin
            .last()
            .map(String::as_str)
            .or(origin_override)
            .map(|origin| absolute_config_url(checkout, origin))
            .transpose()?
    };
    let origin_override = restore_origin.as_deref();
    let nested = discover_linked_worktrees(runner, checkout)?;
    let paths: Vec<_> = nested.iter().map(|tree| tree.relative.clone()).collect();
    for path in &paths {
        crate::archive::slash_path(path).with_context(|| {
            format!(
                "snapshot paths must be UTF-8; rename {} before suspending",
                path.display()
            )
        })?;
    }
    let raw = matches!(ownership, CheckoutOwnership::Raw { .. });
    let mut result = collect(
        runner,
        checkout,
        raw,
        &paths,
        key,
        origin_override,
        history_cutoff,
    )?;
    for entry in selected_stash(runner, checkout, ownership)? {
        if stash_has_untracked(runner, checkout, &entry.commit)? {
            result.untracked_stashes.push(scoped(key, entry));
        }
    }
    for tree in nested {
        let children: Vec<_> = paths
            .iter()
            .filter_map(|path| path.strip_prefix(&tree.relative).ok())
            .filter(|path| !path.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .collect();
        let nested_key = CheckoutKey {
            repository_id: key.repository_id.clone(),
            checkout_path: key.checkout_path.join(&tree.relative),
        };
        result.worktrees.push((
            tree.relative,
            collect(
                runner,
                &tree.directory,
                raw,
                &children,
                &nested_key,
                origin_override,
                history_cutoff,
            )?,
        ));
    }
    let mut roots = vec![checkout.to_path_buf()];
    let mut modules = result
        .initialized_submodules
        .iter()
        .map(|module| checkout.join(&module.value.path))
        .collect::<Vec<_>>();
    for (path, tree) in &result.worktrees {
        let root = checkout.join(path);
        roots.push(root.clone());
        modules.extend(
            tree.initialized_submodules
                .iter()
                .map(|module| root.join(&module.value.path)),
        );
    }
    if !raw && !history_cutoff {
        inventory_origin_dependencies(
            runner,
            checkout,
            &mut result,
            origin_override,
            &teardown_roots(runner, checkout)?,
        )?;
    }
    if raw {
        result.notices.push(RAW_NOTICE.to_owned());
    } else {
        let structure =
            crate::checkpoint::inspect_snapshot_git_structure(runner, checkout, &roots, &modules)?;
        result.presence.extend(
            structure
                .directories
                .into_iter()
                .map(|path| scoped(key, path)),
        );
        result.unrepresented_worktrees = structure
            .worktrees
            .into_iter()
            .map(|value| scoped(key, value))
            .collect();
    }
    result.blockers = inventory_blockers(&result)?;
    Ok(result)
}

fn require_object(
    runner: &dyn GitCommandRunner,
    destination: &Path,
    name: &str,
    oid: &str,
) -> Result<()> {
    let output = run_git(
        runner,
        destination,
        ["cat-file", "-e", &format!("{oid}^{{object}}")],
        &[],
    )?;
    ensure!(
        output.status == 0,
        "snapshot object {oid} for {name} is missing; origin may have rewritten history"
    );
    Ok(())
}

fn checkout_head(
    runner: &dyn GitCommandRunner,
    destination: &Path,
    head: &SnapshotHead,
) -> Result<()> {
    let oid = head.commit();
    require_object(runner, destination, "HEAD", oid)?;
    match head {
        SnapshotHead::Branch { name, .. } => {
            ensure!(
                name.starts_with("refs/heads/"),
                "saved HEAD is not a branch"
            );
            git_text(runner, destination, ["symbolic-ref", "HEAD", name])?;
        }
        SnapshotHead::Detached(_) => {
            git_text(
                runner,
                destination,
                ["update-ref", "--no-deref", "HEAD", oid],
            )?;
        }
    }
    git_text(
        runner,
        destination,
        ["reset", "--hard", "--no-recurse-submodules", oid],
    )?;
    Ok(())
}
/// Git state outside the snapshot grammar is a presence-only teardown audit.
pub(crate) fn git_presence_paths(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if !git_text(runner, checkout, ["replace", "--list"])?.is_empty() {
        paths.push(git_path(runner, checkout, "refs/replace")?);
    }
    for name in ["info/grafts", "shallow"] {
        let path = git_path(runner, checkout, name)?;
        if path.try_exists()? {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn restore_submodules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    modules: &[SnapshotSubmodule],
    links: Option<&[SnapshotGitlink]>,
    notices: &mut Vec<String>,
) -> Result<()> {
    if let Some(links) = links {
        // This is the superproject's own index, never flattened child indexes.
        let desired = links;
        let current = crate::checkpoint::snapshot_index_gitlinks(runner, checkout)?;
        let mut input = Vec::new();
        for link in &current {
            if !desired.iter().any(|saved| saved.path == link.path) {
                input.extend(
                    format!(
                        "0 {}\t{}\0",
                        link.commit,
                        crate::archive::slash_path(&link.path)?
                    )
                    .as_bytes(),
                );
            }
        }
        for link in desired {
            if !current.iter().any(|present| present == link) {
                input.extend(
                    format!(
                        "160000 {}\t{}\0",
                        link.commit,
                        crate::archive::slash_path(&link.path)?
                    )
                    .as_bytes(),
                );
            }
        }
        if !input.is_empty() {
            git_bytes(
                runner,
                checkout,
                ["update-index", "-z", "--index-info"],
                &input,
                "restore saved gitlinks",
            )?;
        }
    }
    let mut modules = modules.iter().collect::<Vec<_>>();
    modules.sort_by_key(|module| module.path.components().count());
    for module in &modules {
        crate::archive::validate_worktree_relative_path(&module.path)?;
        let parent = modules
            .iter()
            .filter(|parent| module.path != parent.path && module.path.starts_with(&parent.path))
            .max_by_key(|parent| parent.path.components().count())
            .map(|parent| parent.path.as_path())
            .unwrap_or_else(|| Path::new(""));
        let relative = module.path.strip_prefix(parent)?;
        let parent = checkout.join(parent);
        let mut command = GitCommand {
            arguments: vec![
                "--literal-pathspecs".into(),
                "submodule".into(),
                "update".into(),
                "--init".into(),
                "--".into(),
                relative.as_os_str().to_owned(),
            ],
            ..Default::default()
        };
        honor_repository_ssh(runner, &parent, &mut command)?;
        let output = runner.run(&parent, &command);
        match output {
            Ok(output) if output.status == 0 => {
                let directory = checkout.join(&module.path);
                let restored =
                    fetch_prerequisites(runner, &directory, std::slice::from_ref(&module.commit))
                        .and_then(|()| {
                            git_text(
                                runner,
                                &directory,
                                ["switch", "--detach", "--", &module.commit],
                            )
                            .map(drop)
                        });
                if let Err(error) = restored {
                    notices.push(format!(
                        "submodule {} was not restored at {}: {error:#}",
                        module.path.display(),
                        module.commit
                    ));
                }
            }
            Ok(output) => notices.push(format!(
                "submodule {} was not initialized: {}",
                module.path.display(),
                git_failure("git submodule update --init", &output)
            )),
            Err(error) => notices.push(format!(
                "submodule {} was not initialized: {error:#}",
                module.path.display()
            )),
        }
    }
    Ok(())
}
fn fetch_prerequisites(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    prerequisites: &[String],
) -> Result<()> {
    let missing = missing_tips(runner, checkout, prerequisites)?;
    let mut failure = String::new();
    if !missing.is_empty() {
        let mut command = GitCommand {
            arguments: [
                "fetch",
                "--no-write-fetch-head",
                "--no-tags",
                "--no-prune",
                "--no-prune-tags",
                "--refmap=",
                "--no-recurse-submodules",
                "--stdin",
                "origin",
            ]
            .map(Into::into)
            .to_vec(),
            stdin: format!("{}\n", missing.join("\n")).into_bytes(),
            ..Default::default()
        };
        honor_repository_ssh(runner, checkout, &mut command)?;
        let output = runner.run(checkout, &command)?;
        if output.status != 0 {
            failure = git_failure("fetch snapshot prerequisites", &output).to_string();
        }
    }
    let missing = missing_tips(runner, checkout, prerequisites)?;
    ensure!(
        missing.is_empty(),
        "snapshot prerequisites are missing: {}; origin may have rewritten history; {failure}",
        missing.join(", ")
    );
    Ok(())
}
/// Apply once to the fresh clone create_managed_clone made for this resume.
/// The provisioner owns initialization, baseline history and publication. On
/// failure it discards this staging clone; restore never reconciles old state.
pub fn restore_workspace_snapshot(
    runner: &dyn GitCommandRunner,
    snapshot: &WorkspaceSnapshot,
    destination: &Path,
) -> Result<Vec<String>> {
    if snapshot.history_cutoff {
        ensure!(
            snapshot.origin_url.is_none(),
            "history-cutoff snapshot must not record an origin"
        );
    } else {
        ensure!(
            snapshot.origin_url.is_some(),
            "snapshot is in-place-only; resume its raw checkout without restoring Git state"
        );
        let origin = snapshot
            .origin_url
            .as_ref()
            .context("snapshot has no origin")?;
        // Capture saved origin with credentials redacted; the seed keeps its URL
        // as configured. Compare both redacted, so an unchanged origin keeps the
        // seed's credentialed URL and its push settings.
        let seeded = run_git(
            runner,
            destination,
            ["config", "--get", "remote.origin.url"],
            &[],
        )?;
        let origin_changed = seeded.status != 0
            || redact_snapshot_url(String::from_utf8_lossy(&seeded.stdout).trim_end())?
                != origin.as_str();
        if origin_changed {
            // The seed's push URL and tracking refs describe the seed's remote.
            // Keep neither under the session's origin: a push must not reach the
            // old destination, and upstreams must not resolve to its commits.
            let unset = run_git(
                runner,
                destination,
                ["config", "--local", "--unset-all", "remote.origin.pushurl"],
                &[],
            )?;
            // Exit 5: there was no push URL to remove.
            ensure!(
                matches!(unset.status, 0 | 5),
                "remove the seed's origin push URL: {}",
                String::from_utf8_lossy(&unset.stderr).trim()
            );
            // Git's own listing goes back to Git byte for byte: a ref name may end
            // in characters that text trimming would remove.
            let stale = git_bytes(
                runner,
                destination,
                [
                    "for-each-ref",
                    "--format=delete %(refname)",
                    "refs/remotes/origin/",
                ],
                &[],
                "list the seed's origin tracking refs",
            )?;
            if !stale.is_empty() {
                git_bytes(
                    runner,
                    destination,
                    ["update-ref", "--no-deref", "--stdin"],
                    &stale,
                    "remove the seed's origin tracking refs",
                )?;
            }
            git_text(
                runner,
                destination,
                [
                    "config",
                    "--local",
                    "--replace-all",
                    "remote.origin.url",
                    origin,
                ],
            )?;
        }
    }
    // Tracking refs for the restored origin arrive with the next fetch;
    // until then, upstreams show as gone rather than as the seed's commits.
    let mut notices = Vec::new();
    if let Some(pack) = &snapshot.pack {
        let directory = git_path(runner, destination, "objects/pack")?;
        fs::create_dir_all(&directory)?;
        let mut source = fs::File::open(pack)?;
        let hash_bytes = if snapshot.object_format == "sha256" {
            32
        } else {
            20
        };
        source.seek(SeekFrom::End(-hash_bytes))?;
        let mut trailer = vec![0; hash_bytes as usize];
        source.read_exact(&mut trailer)?;
        let hash = trailer
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let installed = directory.join(format!("pack-{hash}.pack"));
        fs::copy(pack, &installed)?;
        let output = runner.run(
            destination,
            &GitCommand {
                arguments: vec!["index-pack".into(), installed.into_os_string()],
                ..Default::default()
            },
        )?;
        ensure!(
            output.status == 0,
            "{}",
            git_failure("import snapshot pack", &output)
        );
    }
    if snapshot.history_cutoff {
        let missing = missing_tips(runner, destination, &snapshot.prerequisites)?;
        ensure!(
            missing.is_empty(),
            "history-cutoff snapshot prerequisites are missing: {}; refusing to fetch later history",
            missing.join(", ")
        );
    } else {
        fetch_prerequisites(runner, destination, &snapshot.prerequisites)?;
    }
    for (name, oid) in &snapshot.refs {
        ensure!(
            local_snapshot_ref(name),
            "snapshot ref {name} is outside its local namespaces"
        );
        git_text(runner, destination, ["update-ref", "--no-deref", name, oid])?;
    }
    if !snapshot.history_cutoff {
        for (name, tracking) in &snapshot.branch_tracking {
            git_text(
                runner,
                destination,
                [
                    "config",
                    "--local",
                    "--replace-all",
                    &format!("branch.{name}.remote"),
                    "origin",
                ],
            )?;
            let merge_key = format!("branch.{name}.merge");
            // Replace the provisioner's default upstream; --add would duplicate it.
            // Exit 5 is Git config's normal result when the key does not exist.
            let cleared = run_git(
                runner,
                destination,
                ["config", "--local", "--unset-all", &merge_key],
                &[],
            )?;
            ensure!(
                matches!(cleared.status, 0 | 5),
                "{}",
                git_failure("set saved origin upstream", &cleared)
            );
            for merge in &tracking.merge {
                git_text(
                    runner,
                    destination,
                    ["config", "--local", "--add", &merge_key, merge],
                )?;
            }
        }
    }
    for entry in snapshot.stash.iter().rev() {
        store_snapshot_stash(runner, destination, entry)?;
    }
    checkout_head(runner, destination, &snapshot.head)?;
    if let Some(state) = &snapshot.working_state {
        git_text(runner, destination, ["stash", "apply", "--index", state])?;
    }
    if snapshot.history_cutoff {
        ensure!(
            snapshot.initialized_submodules.is_empty(),
            "history-cutoff snapshot cannot restore initialized submodules without fetching later history"
        );
        restore_submodules(
            runner,
            destination,
            &[],
            (!snapshot.working_state_skipped).then_some(snapshot.index_gitlinks.as_slice()),
            &mut notices,
        )?;
    } else {
        restore_submodules(
            runner,
            destination,
            &snapshot.initialized_submodules,
            (!snapshot.working_state_skipped).then_some(snapshot.index_gitlinks.as_slice()),
            &mut notices,
        )?;
    }
    for tree in &snapshot.worktrees {
        crate::archive::validate_worktree_relative_path(&tree.relative_path)?;
        let path = destination.join(&tree.relative_path);
        let output = runner.run(
            destination,
            &GitCommand {
                arguments: vec![
                    "worktree".into(),
                    "add".into(),
                    "--detach".into(),
                    "--".into(),
                    path.as_os_str().to_owned(),
                    tree.head.commit().into(),
                ],
                ..Default::default()
            },
        )?;
        ensure!(
            output.status == 0,
            "{}",
            git_failure("create saved nested worktree", &output)
        );
        checkout_head(runner, &path, &tree.head)?;
        if let Some(state) = &tree.working_state {
            git_text(runner, &path, ["stash", "apply", "--index", state])?;
        }
        if snapshot.history_cutoff {
            ensure!(
                tree.initialized_submodules.is_empty(),
                "history-cutoff snapshot cannot restore initialized submodules without fetching later history"
            );
            restore_submodules(
                runner,
                &path,
                &[],
                (!tree.working_state_skipped).then_some(tree.index_gitlinks.as_slice()),
                &mut notices,
            )?;
        } else {
            restore_submodules(
                runner,
                &path,
                &tree.initialized_submodules,
                (!tree.working_state_skipped).then_some(tree.index_gitlinks.as_slice()),
                &mut notices,
            )?;
        }
    }
    Ok(notices)
}
/// Called by the provisioner after the staging directory is renamed into place.
pub fn repair_workspace_snapshot(
    runner: &dyn GitCommandRunner,
    snapshot: &WorkspaceSnapshot,
    destination: &Path,
) -> Result<()> {
    if snapshot.worktrees.is_empty() {
        return Ok(());
    }
    let mut arguments = vec!["worktree".into(), "repair".into()];
    for tree in &snapshot.worktrees {
        crate::archive::validate_worktree_relative_path(&tree.relative_path)?;
        arguments.push(destination.join(&tree.relative_path).into_os_string());
    }
    let output = runner.run(
        destination,
        &GitCommand {
            arguments,
            ..Default::default()
        },
    )?;
    ensure!(
        output.status == 0,
        "{}",
        git_failure("repair published nested worktrees", &output)
    );
    Ok(())
}
fn store_snapshot_stash(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    entry: &SnapshotStashEntry,
) -> Result<()> {
    // stash store elides an adjacent identical OID. A temporary parent transition
    // and removal of its reflog row preserve repeated entries and their messages.
    let current = run_git(
        runner,
        checkout,
        ["rev-parse", "--verify", "--quiet", "refs/stash"],
        &[],
    )?;
    let repeated =
        current.status == 0 && std::str::from_utf8(&current.stdout)?.trim() == entry.commit;
    if repeated {
        let parent = git_text(
            runner,
            checkout,
            ["rev-parse", &format!("{}^1", entry.commit)],
        )?;
        git_text(runner, checkout, ["update-ref", "refs/stash", &parent])?;
    }
    let output = runner.run(
        checkout,
        &GitCommand {
            arguments: vec![
                "stash".into(),
                "store".into(),
                "-m".into(),
                entry.message.clone().into(),
                entry.commit.clone().into(),
            ],
            env: synthetic_git_identity(),
            ..Default::default()
        },
    )?;
    ensure!(
        output.status == 0,
        "{}",
        git_failure("restore saved stash", &output)
    );
    if repeated {
        git_text(
            runner,
            checkout,
            ["reflog", "delete", "--rewrite", "refs/stash@{1}"],
        )?;
    }
    Ok(())
}

fn missing_tips(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    tips: &[String],
) -> Result<Vec<String>> {
    if tips.is_empty() {
        return Ok(Vec::new());
    }
    let output = runner.run(
        checkout,
        &GitCommand {
            arguments: vec![
                "cat-file".into(),
                "--batch-check=%(objectname) %(objecttype)".into(),
            ],
            stdin: format!("{}\n", tips.join("\n")).into_bytes(),
            ..Default::default()
        },
    )?;
    ensure!(
        output.status == 0,
        "{}",
        git_failure("check saved snapshot tips", &output)
    );
    std::str::from_utf8(&output.stdout)?
        .lines()
        .filter_map(|line| line.strip_suffix(" missing"))
        .map(|oid| Ok(oid.to_owned()))
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::SystemGit;

    struct FileGit;
    fn prepare_file_git(command: &GitCommand) -> GitCommand {
        let mut command = command.clone();
        command
            .arguments
            .splice(0..0, ["-c".into(), "protocol.file.allow=always".into()]);
        command.env.extend([
            ("GIT_AUTHOR_NAME".into(), "Snapshot Test".into()),
            ("GIT_AUTHOR_EMAIL".into(), "snapshot@example.test".into()),
            ("GIT_COMMITTER_NAME".into(), "Snapshot Test".into()),
            ("GIT_COMMITTER_EMAIL".into(), "snapshot@example.test".into()),
        ]);
        command
    }
    impl GitCommandRunner for FileGit {
        fn run(&self, path: &Path, command: &GitCommand) -> Result<crate::archive::GitOutput> {
            SystemGit.run(path, &prepare_file_git(command))
        }
        fn run_to_file(
            &self,
            path: &Path,
            command: &GitCommand,
            output: fs::File,
        ) -> Result<crate::archive::GitOutput> {
            SystemGit.run_to_file(path, &prepare_file_git(command), output)
        }
    }
    fn git(path: &Path, args: &[&str]) -> String {
        let output = FileGit
            .run(
                path,
                &GitCommand {
                    arguments: args.iter().map(OsString::from).collect(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            output.status,
            0,
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end_matches('\n')
            .to_owned()
    }
    struct Repo {
        temp: tempfile::TempDir,
        origin: PathBuf,
        checkout: PathBuf,
    }
    impl Repo {
        fn new() -> Self {
            Self::new_with_format("sha1")
        }
        fn new_with_format(format: &str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let origin = temp.path().join("origin.git");
            let checkout = temp.path().join("source");
            git(
                temp.path(),
                &[
                    "init",
                    "--bare",
                    "--initial-branch=main",
                    &format!("--object-format={format}"),
                    origin.to_str().unwrap(),
                ],
            );
            git(
                temp.path(),
                &[
                    "clone",
                    origin.to_str().unwrap(),
                    checkout.to_str().unwrap(),
                ],
            );
            fs::write(checkout.join("file"), "first\nsecond\nthird\n").unwrap();
            git(&checkout, &["add", "file"]);
            git(&checkout, &["commit", "-m", "base"]);
            git(&checkout, &["push", "origin", "main"]);
            Self {
                temp,
                origin,
                checkout,
            }
        }
        fn capture(&self, ownership: &CheckoutOwnership) -> WorkspaceSnapshot {
            capture_workspace_snapshot(
                &FileGit,
                &self.checkout,
                ownership,
                &self.temp.path().join("snapshot"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap()
        }
        fn provision(&self, name: &str) -> PathBuf {
            let path = self.temp.path().join(name);
            git(
                self.temp.path(),
                &[
                    "clone",
                    "--no-checkout",
                    self.origin.to_str().unwrap(),
                    path.to_str().unwrap(),
                ],
            );
            path
        }
        fn restore(&self, snapshot: &WorkspaceSnapshot, _seed: Option<&Path>) -> PathBuf {
            self.restore_with_baseline(snapshot).0
        }
        fn restore_with_baseline(
            &self,
            snapshot: &WorkspaceSnapshot,
        ) -> (PathBuf, BTreeMap<String, String>) {
            let staging = self.provision("staging");
            let baseline = state(&staging, &[]).refs;
            let notices = restore_workspace_snapshot(&FileGit, snapshot, &staging).unwrap();
            assert!(notices.is_empty(), "{notices:?}");
            let published = self.temp.path().join("restored");
            fs::rename(staging, &published).unwrap();
            repair_workspace_snapshot(&FileGit, snapshot, &published).unwrap();
            (published, baseline)
        }
        fn commit(&self, text: &str) {
            fs::write(self.checkout.join("file"), text).unwrap();
            git(&self.checkout, &["commit", "-am", "change"]);
        }
        fn conflict(&self) {
            git(&self.checkout, &["branch", "side"]);
            self.commit("main version\n");
            git(&self.checkout, &["checkout", "side"]);
            self.commit("side version\n");
            git(&self.checkout, &["checkout", "main"]);
        }
        fn start_conflict(&self, kind: &str) {
            assert_ne!(
                run_git(&FileGit, &self.checkout, [kind, "side"], &[])
                    .unwrap()
                    .status,
                0
            );
        }
    }
    fn add_submodule(root: &Repo, module: &Repo) -> PathBuf {
        git(
            &root.checkout,
            &["submodule", "add", module.origin.to_str().unwrap(), "sub"],
        );
        git(&root.checkout, &["commit", "-am", "registered submodule"]);
        git(&root.checkout, &["push", "origin", "main"]);
        root.checkout.join("sub")
    }

    #[derive(Debug, PartialEq, Eq)]
    struct CheckoutState {
        head: SnapshotHead,
        index: String,
        tracked: String,
    }
    #[derive(Debug, PartialEq, Eq)]
    struct State {
        refs: BTreeMap<String, String>,
        stash: Vec<(String, String, String)>,
        checkouts: BTreeMap<PathBuf, CheckoutState>,
    }
    fn tracked_files(path: &Path) -> String {
        // Compare canonical tracked file data through a private index. Module
        // commit positions and missing/uninitialized directory placeholders
        // follow native update/checkout and are declared topology losses.
        let links = crate::checkpoint::snapshot_index_gitlinks(&FileGit, path).unwrap();
        let index = private_index(&FileGit, path).unwrap();
        let env = vec![("GIT_INDEX_FILE".into(), index.path().as_os_str().to_owned())];
        let run = |arguments: Vec<OsString>, stdin: Vec<u8>| {
            let output = FileGit
                .run(
                    path,
                    &GitCommand {
                        arguments,
                        env: env.clone(),
                        stdin,
                    },
                )
                .unwrap();
            assert_eq!(
                output.status,
                0,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        run(vec!["add".into(), "-u".into()], Vec::new());
        let mut normalized = Vec::new();
        for link in links {
            let topology = match fs::symlink_metadata(path.join(&link.path)) {
                Ok(metadata) => metadata.is_dir(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(error) => panic!("{error}"),
            };
            if topology {
                normalized.extend(
                    format!(
                        "160000 {}\t{}\0",
                        link.commit,
                        crate::archive::slash_path(&link.path).unwrap()
                    )
                    .as_bytes(),
                );
            }
        }
        if !normalized.is_empty() {
            run(
                vec!["update-index".into(), "-z".into(), "--index-info".into()],
                normalized,
            );
        }
        String::from_utf8(run(vec!["write-tree".into()], Vec::new()))
            .unwrap()
            .trim_end_matches('\n')
            .to_owned()
    }
    fn state(path: &Path, checkout_paths: &[PathBuf]) -> State {
        let refs = git(path, &["for-each-ref", "--format=%(refname) %(objectname)"])
            .lines()
            .map(|line| {
                let (name, oid) = line.split_once(' ').unwrap();
                (name.to_owned(), oid.to_owned())
            })
            .filter(|(name, _)| name != "refs/stash")
            .collect();
        let stash = read_stash(&FileGit, path)
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    git(path, &["rev-parse", &format!("{}^{{tree}}", entry.commit)]),
                    git(
                        path,
                        &["rev-parse", &format!("{}^2^{{tree}}", entry.commit)],
                    ),
                    entry.message.clone(),
                )
            })
            .collect();
        let checkouts = checkout_paths
            .iter()
            .map(|relative| {
                let directory = path.join(relative);
                (
                    relative.clone(),
                    CheckoutState {
                        head: read_head(&FileGit, &directory).unwrap(),
                        index: git(&directory, &["write-tree"]),
                        tracked: tracked_files(&directory),
                    },
                )
            })
            .collect();
        State {
            refs,
            stash,
            checkouts,
        }
    }
    #[derive(Debug, PartialEq, Eq)]
    struct RawState {
        refs: Vec<u8>,
        indexes: Vec<Option<Vec<u8>>>,
        stash: Vec<SnapshotStashEntry>,
        fetch_head: Option<Vec<u8>>,
    }
    fn raw_state(root: &Path, paths: &[PathBuf]) -> RawState {
        RawState {
            refs: run_git(
                &FileGit,
                root,
                [
                    "for-each-ref",
                    "--format=%(refname) %(objectname) %(symref)",
                ],
                &[],
            )
            .unwrap()
            .stdout,
            indexes: paths
                .iter()
                .map(|p| fs::read(git_path(&FileGit, &root.join(p), "index").unwrap()).ok())
                .collect(),
            stash: read_stash(&FileGit, root).unwrap(),
            fetch_head: fs::read(git_path(&FileGit, root, "FETCH_HEAD").unwrap()).ok(),
        }
    }
    struct Generator(u64);
    impl Generator {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }
    fn generated_commands(root: &Path, paths: &[PathBuf], seed: u64) {
        let mut generator = Generator(seed);
        for step in 0..18 {
            let directory = root.join(&paths[(generator.next() as usize) % paths.len()]);
            match generator.next() % 11 {
                0 | 1 => {
                    fs::write(
                        directory.join("file"),
                        format!("seed {seed} step {step}\nsecond\nthird\n"),
                    )
                    .unwrap();
                    git(&directory, &["add", "file"]);
                    if step % 2 == 0 {
                        fs::write(
                            directory.join("file"),
                            format!("seed {seed} step {step}\nsecond\nunstaged\n"),
                        )
                        .unwrap();
                    }
                }
                2 => {
                    git(&directory, &["add", "-u"]);
                    git(
                        &directory,
                        &[
                            "commit",
                            "--allow-empty",
                            "-m",
                            &format!("generated {step}"),
                        ],
                    );
                }
                3 => {
                    let message = format!("seed {seed} stash {step}");
                    if step % 3 == 0 {
                        git(&directory, &["stash", "push", "-u", "-m", &message]);
                    } else {
                        git(&directory, &["stash", "push", "-m", &message]);
                    }
                }
                4 => {
                    git(
                        &directory,
                        &[
                            "tag",
                            "-a",
                            &format!("tag-{step}"),
                            "-m",
                            &format!("seed {seed}"),
                        ],
                    );
                }
                5 => {
                    git(&directory, &["branch", &format!("branch-{step}")]);
                }
                6 => {
                    git(
                        &directory,
                        &["notes", "add", "-f", "-m", &format!("note {seed} {step}")],
                    );
                }
                7 => {
                    fs::write(
                        directory.join(format!("untracked-{step}")),
                        "declared loss\n",
                    )
                    .unwrap();
                }
                8 => {
                    git(
                        &directory,
                        &["push", "--force", "origin", "HEAD:refs/heads/published"],
                    );
                }
                9 => {
                    let name = format!("added-{step}");
                    fs::write(directory.join(&name), format!("added {seed} {step}\n")).unwrap();
                    git(&directory, &["add", &name]);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(
                            directory.join(&name),
                            fs::Permissions::from_mode(0o755),
                        )
                        .unwrap();
                    }
                }
                _ => {
                    git(&directory, &["rm", "-f", "--ignore-unmatch", "file"]);
                }
            }
        }
        git(
            root,
            &[
                "remote",
                "add",
                "discarded",
                "https://example.invalid/extra.git",
            ],
        );
        git(
            root,
            &[
                "config",
                "url.https://example.invalid/.insteadOf",
                "never-matches:",
            ],
        );
        git(
            root,
            &[
                "config",
                "remote.origin.pushurl",
                "https://example.invalid/publication.git",
            ],
        );
        if seed.is_multiple_of(11) {
            git(root, &["config", "--unset-all", "branch.main.merge"]);
        }
    }
    // Allowed differences are deliberately closed: untracked/ignored files and
    // stash third parents never travel; raw launch-baseline refs/stashes do not
    // travel; active raw operations omit tracked edits; symbolic ref aliases are
    // materialized at their resolved OID. Initialized modules retain checkout
    // gitlinks; native update selects the parent's pin. Module local refs,
    // branch attachment, configuration other than origin/upstreams, extra
    // remotes and rewrite rules are declared losses. Submodule update failure
    // is a notice and an uninitialized path. Gitlink working positions and
    // missing/uninitialized directory placeholders follow Git; regular-file
    // data and the exact index remain compared. The provisioner
    // contributes origin tracking refs, origin tags, and its initial branch only
    // when that branch is absent from the projected source. No other extras pass.
    fn assert_projected_state(
        mut source: State,
        restored: State,
        launch: Option<&LaunchRecord>,
        baseline: &BTreeMap<String, String>,
    ) {
        source
            .refs
            .retain(|name, _| !name.starts_with("refs/remotes/"));
        if let Some(launch) = launch {
            source.refs.retain(|name, oid| {
                let head = source.checkouts.values().any(|tree| matches!(&tree.head, SnapshotHead::Branch { name: branch, .. } if branch == name));
                head || launch.refs.get(name) != Some(oid)
            });
            source
                .stash
                .retain(|entry| !launch.stash.iter().any(|old| old.message == entry.2));
        }
        let mut actual = restored.refs;
        for (name, oid) in baseline {
            if !source.refs.contains_key(name)
                && (name.starts_with("refs/remotes/origin/")
                    || name.starts_with("refs/tags/")
                    || name == "refs/heads/main")
            {
                assert_eq!(
                    actual.remove(name).as_ref(),
                    Some(oid),
                    "provisioner baseline {name}"
                );
            }
        }
        assert_eq!(actual, source.refs, "saved refs and all unexpected extras");
        assert_eq!(
            restored.stash, source.stash,
            "stash trees, index trees and messages"
        );
        assert_eq!(
            restored.checkouts, source.checkouts,
            "HEAD, index and tracked worktree"
        );
    }
    #[test]
    fn seeded_git_round_trip_after_source_deletion_and_raw_immutability() {
        let started = std::time::Instant::now();
        let seeds = 1..=32;
        for seed in seeds {
            eprintln!("snapshot property seed={seed}");
            let r = Repo::new_with_format(if seed % 8 == 0 { "sha256" } else { "sha1" });
            git(&r.checkout, &["fetch", "origin"]);
            git(&r.checkout, &["branch", "user-before-launch"]);
            fs::write(r.checkout.join("file"), "baseline stash\n").unwrap();
            git(&r.checkout, &["stash", "push", "-m", "baseline stash"]);
            record_launch(&FileGit, &r.checkout, "property").unwrap();
            let launch = LaunchRecord::read(&FileGit, &r.checkout, "property")
                .unwrap()
                .unwrap();
            let raw = seed % 2 == 0;
            let ownership = if raw {
                CheckoutOwnership::Raw {
                    launch: Some(launch.clone()),
                }
            } else {
                CheckoutOwnership::Managed
            };
            let mut paths = vec![PathBuf::new()];
            if seed % 3 != 0 {
                let relative = PathBuf::from("lanes/nested lane");
                git(
                    &r.checkout,
                    &[
                        "worktree",
                        "add",
                        "--detach",
                        r.checkout.join(&relative).to_str().unwrap(),
                    ],
                );
                paths.push(relative);
            }
            if seed % 5 == 0 && paths.len() > 1 {
                let relative = paths[1].join("inner");
                git(
                    &r.checkout,
                    &[
                        "worktree",
                        "add",
                        "--detach",
                        r.checkout.join(&relative).to_str().unwrap(),
                    ],
                );
                paths.push(relative);
            }
            generated_commands(&r.checkout, &paths, seed);
            let override_origin = if seed % 7 == 0 {
                git(&r.checkout, &["remote", "remove", "origin"]);
                Some(r.origin.to_str().unwrap().to_owned())
            } else {
                None
            };
            let expected = state(&r.checkout, &paths);
            let origin_upstream =
                config_values(&FileGit, &r.checkout, "branch.main.merge").unwrap();
            let invariant = raw_state(&r.checkout, &paths);
            let snapshot = if raw || override_origin.is_some() {
                capture_workspace_snapshot(
                    &FileGit,
                    &r.checkout,
                    &ownership,
                    &r.temp.path().join("raw.pack"),
                    &CaptureConsent {
                        untracked: if raw {
                            UntrackedConsent::None
                        } else {
                            UntrackedConsent::Any
                        },
                        operations: OperationConsent::None,
                    },
                    override_origin.clone(),
                )
                .unwrap()
            } else {
                r.capture(&ownership)
            };
            if raw {
                assert_eq!(
                    raw_state(&r.checkout, &paths),
                    invariant,
                    "seed {seed}: capture mutated raw state"
                );
            }
            let encoded = serde_json::to_vec(&snapshot).unwrap();
            let snapshot = serde_json::from_slice(&encoded).unwrap();
            fs::remove_dir_all(&r.checkout).unwrap();
            let staging = r.provision("staging");
            if override_origin.is_some() {
                git(&staging, &["remote", "remove", "origin"]);
            }
            let baseline = state(&staging, &[]).refs;
            restore_workspace_snapshot(&FileGit, &snapshot, &staging).unwrap();
            let restored = r.temp.path().join("published");
            fs::rename(staging, &restored).unwrap();
            repair_workspace_snapshot(&FileGit, &snapshot, &restored).unwrap();
            assert_eq!(git(&restored, &["remote"]), "origin");
            assert_eq!(
                config_values(&FileGit, &restored, "branch.main.merge").unwrap(),
                origin_upstream,
                "seed {seed}: origin upstream must replace the provisioner's value"
            );
            assert!(
                config_values(&FileGit, &restored, "remote.origin.pushurl")
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                run_git(
                    &FileGit,
                    &restored,
                    ["config", "--local", "--get-regexp", r"^url\."],
                    &[]
                )
                .unwrap()
                .status,
                1
            );
            assert_projected_state(
                expected,
                state(&restored, &paths),
                raw.then_some(&launch),
                &baseline,
            );
            for relative in &paths {
                for step in 0..18 {
                    assert!(
                        !restored
                            .join(relative)
                            .join(format!("untracked-{step}"))
                            .exists(),
                        "untracked file survived seed {seed}, step {step}"
                    );
                }
            }
        }
        eprintln!("32 seeds passed in {:?}", started.elapsed());
    }
    #[test]
    fn seeded_fully_pushed_sessions_have_empty_packs() {
        for seed in 33..=40 {
            let r = Repo::new();
            generated_commands(&r.checkout, &[PathBuf::new()], seed);
            git(
                &r.checkout,
                &["config", "--unset-all", "remote.origin.pushurl"],
            );
            git(&r.checkout, &["add", "-u"]);
            git(
                &r.checkout,
                &["commit", "--allow-empty", "-m", "publish tracked state"],
            );
            for (index, entry) in read_stash(&FileGit, &r.checkout)
                .unwrap()
                .iter()
                .enumerate()
            {
                git(
                    &r.checkout,
                    &[
                        "tag",
                        &format!("published-stash-{index}"),
                        &tracked_stash(&FileGit, &r.checkout, entry).unwrap().commit,
                    ],
                );
            }
            git(&r.checkout, &["push", "origin", "--all"]);
            if run_git(
                &FileGit,
                &r.checkout,
                ["show-ref", "--verify", "refs/notes/commits"],
                &[],
            )
            .unwrap()
            .status
                == 0
            {
                git(
                    &r.checkout,
                    &["tag", "published-notes", "refs/notes/commits"],
                );
            }
            git(&r.checkout, &["push", "origin", "--tags"]);
            let notes = run_git(
                &FileGit,
                &r.checkout,
                ["show-ref", "--verify", "refs/notes/commits"],
                &[],
            )
            .unwrap();
            if notes.status == 0 {
                git(&r.checkout, &["push", "origin", "refs/notes/commits"]);
            }
            let expected = state(&r.checkout, &[PathBuf::new()]);
            let snapshot = r.capture(&CheckoutOwnership::Managed);
            assert!(snapshot.pack.is_none(), "fully pushed seed {seed}");
            fs::remove_dir_all(&r.checkout).unwrap();
            let (restored, baseline) = r.restore_with_baseline(&snapshot);
            assert_projected_state(
                expected,
                state(&restored, &[PathBuf::new()]),
                None,
                &baseline,
            );
        }
    }
    struct HostGit {
        global: PathBuf,
    }
    impl HostGit {
        fn prepare_command(&self, command: &GitCommand) -> GitCommand {
            let mut command = command.clone();
            command.env.extend([
                (
                    "GIT_CONFIG_GLOBAL".into(),
                    self.global.as_os_str().to_owned(),
                ),
                (
                    "GIT_CONFIG_SYSTEM".into(),
                    self.global.with_extension("system").into_os_string(),
                ),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ]);
            command
        }
    }
    impl GitCommandRunner for HostGit {
        fn run(
            &self,
            repository: &Path,
            command: &GitCommand,
        ) -> Result<crate::archive::GitOutput> {
            SystemGit.run(repository, &self.prepare_command(command))
        }
        fn run_to_file(
            &self,
            repository: &Path,
            command: &GitCommand,
            output: fs::File,
        ) -> Result<crate::archive::GitOutput> {
            SystemGit.run_to_file(repository, &self.prepare_command(command), output)
        }
    }
    fn host_git(path: &Path, name: &str) -> HostGit {
        let global = path.join(name);
        fs::write(&global, "").unwrap();
        fs::write(global.with_extension("system"), "").unwrap();
        HostGit { global }
    }
    fn scoped_item_path(
        root: &Path,
        checkout: CheckoutKey,
        path: impl Into<PathBuf>,
    ) -> UntrackedItem {
        let path = path.into();
        let identity = file_identity(&fs::symlink_metadata(root.join(&path)).unwrap()).unwrap();
        UntrackedItem::Path {
            checkout,
            path,
            identity,
        }
    }
    fn item_stash(commit: String, message: String) -> UntrackedItem {
        UntrackedItem::Stash {
            checkout: CheckoutKey::default(),
            commit,
            message,
        }
    }
    // Hard-won: ffd652c27: reflog expiration hid the still-applicable root stash ref.
    #[test]
    fn expired_root_stash_ref_survives_without_reflog_entries() {
        let root = Repo::new();
        fs::write(root.checkout.join("file"), "stash retained by ref\n").unwrap();
        git(&root.checkout, &["stash", "push", "-m", "retained stash"]);
        let stash = git(&root.checkout, &["rev-parse", "refs/stash"]);
        git(
            &root.checkout,
            &["reflog", "expire", "--expire=all", "refs/stash"],
        );
        assert!(git(&root.checkout, &["stash", "list"]).is_empty());
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert_eq!(snapshot.stash[0].commit, stash);
        let restored = root.restore(&snapshot, None);
        git(&restored, &["stash", "apply"]);
        assert_eq!(
            fs::read_to_string(restored.join("file")).unwrap(),
            "stash retained by ref\n"
        );
    }

    // Hard-won: 227e874ea: retained source/configuration regression.
    #[test]
    fn unreachable_origin_uses_stale_remote_tracking_refs() {
        let r = Repo::new();
        r.commit("unpushed\n");
        let away = r.temp.path().join("away.git");
        fs::rename(&r.origin, &away).unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.remote_refs_stale);
        fs::create_dir(&r.origin).unwrap();
        let unresolvable = r.capture(&CheckoutOwnership::Managed);
        assert!(unresolvable.remote_refs_stale);
        assert!(
            unresolvable
                .notices
                .iter()
                .any(|notice| notice.contains("could not inspect local origin object storage"))
        );
        fs::remove_dir(&r.origin).unwrap();
        fs::rename(&away, &r.origin).unwrap();
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
    }
    // Hard-won: d7c636759: intent-to-add could fail capture after an already-approved managed abort.
    #[test]
    fn intent_to_add_refuses_before_aborting_a_managed_merge() {
        let root = Repo::new();
        root.conflict();
        root.start_conflict("merge");
        fs::write(root.checkout.join("draft"), "intent payload\n").unwrap();
        git(&root.checkout, &["add", "-N", "draft"]);
        let index = fs::read(root.checkout.join(".git/index")).unwrap();
        let merge = fs::read(root.checkout.join(".git/MERGE_HEAD")).unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("refused.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::IntentToAdd(_))
            ),
            "{error:#}"
        );
        for step in ["draft", "git add", "git reset --"] {
            assert!(error.to_string().contains(step), "{error:#}");
        }
        assert_eq!(fs::read(root.checkout.join(".git/index")).unwrap(), index);
        assert_eq!(
            fs::read(root.checkout.join(".git/MERGE_HEAD")).unwrap(),
            merge
        );
        assert_eq!(
            fs::read_to_string(root.checkout.join("draft")).unwrap(),
            "intent payload\n"
        );
    }

    // Hard-won: 227e874ea: retained source/configuration regression.
    #[test]
    fn stash_untracked_parent_requires_consent_and_never_restores() {
        let r = Repo::new();
        let ownership = CheckoutOwnership::Managed;
        fs::write(r.checkout.join("file"), "tracked stash\n").unwrap();
        let mut state = 0xabcdef01_u32;
        let scratch: Vec<_> = (0..262_144)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        fs::write(r.checkout.join("secret"), scratch).unwrap();
        git(&r.checkout, &["stash", "push", "-u", "-m", "mixed stash"]);
        let original = read_stash(&FileGit, &r.checkout).unwrap();
        let estimate = estimate_delta(&FileGit, &r.checkout, &ownership, None).unwrap();
        assert!(
            estimate.total_bytes.unwrap() < 65_536,
            "scratch must not inflate the real transfer pack"
        );
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &ownership,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items)) if items == &vec![item_stash(original[0].commit.clone(), original[0].message.clone())])
        );
        let snapshot = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &ownership,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Shown(vec![item_stash(
                    original[0].commit.clone(),
                    original[0].message.clone(),
                )]),
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap();
        assert_eq!(
            estimate.total_bytes,
            Some(fs::metadata(snapshot.pack.as_ref().unwrap()).unwrap().len())
        );
        assert_ne!(snapshot.stash[0].commit, original[0].commit);
        assert_eq!(read_stash(&FileGit, &r.checkout).unwrap(), original);
        let restored = r.restore(&snapshot, None);
        assert!(!stash_has_untracked(&FileGit, &restored, &snapshot.stash[0].commit).unwrap());
        let missing = run_git(
            &FileGit,
            &restored,
            ["cat-file", "-e", &original[0].commit],
            &[],
        )
        .unwrap();
        assert_ne!(
            missing.status, 0,
            "original untracked stash must not travel"
        );
        git(&restored, &["stash", "apply"]);
        assert_eq!(fs::read(restored.join("file")).unwrap(), b"tracked stash\n");
        assert!(!restored.join("secret").exists());
    }
    // Hard-won: 227e874ea: retained source/configuration regression.
    #[test]
    fn embedded_http_credentials_never_enter_snapshot() {
        let r = Repo::new();
        let credentialed = "http://token:secret@127.0.0.1:1/unreachable.git";
        git(&r.checkout, &["remote", "set-url", "origin", credentialed]);
        git(
            &r.checkout,
            &["config", "remote.origin.pushurl", credentialed],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            snapshot.origin_url.as_deref(),
            Some("http://127.0.0.1:1/unreachable.git")
        );
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("token"));
    }
    // Hard-won: 227e874ea: retained source/configuration regression.
    #[test]
    fn raw_capture_never_prunes_user_remote_tracking_refs() {
        let r = Repo::new();
        git(&r.checkout, &["branch", "deleted"]);
        git(&r.checkout, &["push", "origin", "deleted"]);
        let tracked = git(&r.checkout, &["rev-parse", "refs/remotes/origin/deleted"]);
        git(&r.origin, &["update-ref", "-d", "refs/heads/deleted"]);
        r.capture(&CheckoutOwnership::Raw { launch: None });
        assert_eq!(
            git(&r.checkout, &["rev-parse", "refs/remotes/origin/deleted"]),
            tracked
        );
    }
    // Hard-won: 227e874ea: retained source/configuration regression.
    #[test]
    fn raw_reachable_origin_does_not_exclude_force_pushed_tracking_tip() {
        let r = Repo::new();
        r.commit("old published branch\n");
        git(&r.checkout, &["push", "origin", "main"]);
        let base = git(&r.checkout, &["rev-parse", "HEAD~"]);
        git(&r.origin, &["update-ref", "refs/heads/main", &base]);
        git(&r.origin, &["gc", "--prune=now"]);
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(snapshot.pack.is_some());
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
    }
    // Hard-won: 227e874ea: retained source/configuration regression.
    #[test]
    fn session_consent_checks_all_repositories_before_aborting_any() {
        let a = Repo::new();
        let b = Repo::new();
        a.conflict();
        a.start_conflict("merge");
        fs::write(a.checkout.join("notes.txt"), "approved only in a").unwrap();
        fs::write(b.checkout.join("notes.txt"), "not approved in b").unwrap();
        let mut repositories: Vec<_> = [("a", &a), ("b", &b)]
            .into_iter()
            .map(|(id, repo)| RepositoryInput {
                id: id.into(),
                checkout: repo.checkout.clone(),
                ownership: CheckoutOwnership::Managed,
                origin_override: None,
                pack_path: repo.temp.path().join("snapshot.pack"),
                exclusions: ExclusionSource::Origin,
                keep_dir: None,
            })
            .collect();
        let consent = SessionCaptureConsent {
            repositories: BTreeMap::from([
                (
                    "a".into(),
                    CaptureConsent {
                        untracked: UntrackedConsent::Shown(vec![scoped_item_path(
                            &a.checkout,
                            repository_key("a"),
                            "notes.txt",
                        )]),
                        operations: OperationConsent::Any,
                    },
                ),
                (
                    "b".into(),
                    CaptureConsent {
                        untracked: UntrackedConsent::Shown(vec![scoped_item_path(
                            &a.checkout,
                            repository_key("a"),
                            "notes.txt",
                        )]),
                        operations: OperationConsent::Any,
                    },
                ),
            ]),
        };
        let error = capture_session_snapshot(&FileGit, &repositories, &consent).unwrap_err();
        let refusal = error.downcast_ref::<SnapshotRefused>().unwrap();
        assert!(
            matches!(refusal, SnapshotRefused::Untracked(items) if items == &vec![scoped_item_path(&b.checkout, repository_key("b"), "notes.txt")])
        );
        assert!(a.checkout.join(".git/MERGE_HEAD").exists());
        let c = Repo::new();
        c.commit("shallow blocker\n");
        git(&c.checkout, &["push", "origin", "main"]);
        git(&c.checkout, &["fetch", "--depth=1", "origin"]);
        repositories.push(RepositoryInput {
            id: "c".into(),
            checkout: c.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: c.temp.path().join("blocked.pack"),
            exclusions: ExclusionSource::Origin,
            keep_dir: None,
        });
        let inventory = session_inventory(&FileGit, &repositories).unwrap();
        assert!(
            inventory["c"].blockers.iter().any(|blocker| matches!(
                blocker.kind,
                SnapshotRefused::UnrepresentedGitState { .. }
            ))
        );
        let error =
            capture_session_snapshot(&FileGit, &repositories, &SessionCaptureConsent::default())
                .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::UnrepresentedGitState { directories, .. }) if directories.iter().any(|row| row.checkout.repository_id == "c" && row.value.ends_with("shallow"))),
            "{error:#}"
        );
        assert!(a.checkout.join(".git/MERGE_HEAD").exists());
    }
    // Hard-won: 3a4aef552: Git's configured pack splitting produced multiple snapshot artifacts.
    #[test]
    fn snapshot_and_estimate_override_configured_pack_splitting() {
        let root = Repo::new();
        git(&root.checkout, &["config", "pack.packSizeLimit", "1m"]);
        let mut state = 0x34ab_9812_76ff_1229_u64;
        for name in ["a", "b", "c", "d"] {
            let bytes = (0..600_000)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect::<Vec<_>>();
            fs::write(root.checkout.join(name), bytes).unwrap();
        }
        git(&root.checkout, &["add", "a", "b", "c", "d"]);
        git(&root.checkout, &["commit", "-m", "incompressible delta"]);
        let split = git(
            &root.checkout,
            &[
                "pack-objects",
                "--all",
                root.temp.path().join("split").to_str().unwrap(),
            ],
        );
        assert!(
            split.lines().count() > 1,
            "fixture did not exercise splitting"
        );
        let estimate =
            estimate_delta(&FileGit, &root.checkout, &CheckoutOwnership::Managed, None).unwrap();
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            estimate.total_bytes,
            Some(fs::metadata(snapshot.pack.as_ref().unwrap()).unwrap().len())
        );
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD^{tree}"]),
            git(&root.checkout, &["rev-parse", "HEAD^{tree}"])
        );
        git(&restored, &["fsck", "--full"]);
    }

    // Hard-won: #1635: snapshot packing failed with EXDEV when repository and capture directory were on different filesystems.
    #[cfg(target_os = "linux")]
    #[test]
    fn snapshot_and_estimate_pack_across_different_filesystems() {
        use std::os::unix::fs::MetadataExt;

        let system_temp = std::env::temp_dir();
        let system_dev = fs::metadata(&system_temp).unwrap().dev();
        let Some(other) = ["/dev/shm", "/var/tmp", "/tmp"]
            .into_iter()
            .find_map(|candidate| {
                let metadata = fs::metadata(candidate).ok()?;
                if !metadata.is_dir() || metadata.dev() == system_dev {
                    return None;
                }
                tempfile::Builder::new()
                    .prefix("mj-snapshot-exdev-")
                    .tempdir_in(candidate)
                    .ok()
            })
        else {
            eprintln!(
                "skipping cross-filesystem snapshot test: no writable mount distinct from {}",
                system_temp.display()
            );
            return;
        };
        assert_ne!(fs::metadata(other.path()).unwrap().dev(), system_dev);

        let origin = other.path().join("origin.git");
        let checkout = other.path().join("checkout");
        git(
            other.path(),
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                origin.to_str().unwrap(),
            ],
        );
        git(
            other.path(),
            &[
                "clone",
                origin.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ],
        );
        fs::write(checkout.join("file"), "origin\n").unwrap();
        git(&checkout, &["add", "file"]);
        git(&checkout, &["commit", "-m", "base"]);
        git(&checkout, &["push", "origin", "main"]);
        let capture_dir = tempfile::tempdir_in(&system_temp).unwrap();
        assert_ne!(
            fs::metadata(other.path()).unwrap().dev(),
            fs::metadata(capture_dir.path()).unwrap().dev()
        );
        let pack_base = capture_dir.path().join("snapshot");
        let clean = capture_workspace_snapshot(
            &FileGit,
            &checkout,
            &CheckoutOwnership::Managed,
            &pack_base,
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        assert!(clean.pack.is_none());
        assert!(!fs::read_dir(capture_dir.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "pack")
        }));
        assert_eq!(
            estimate_delta(&FileGit, &checkout, &CheckoutOwnership::Managed, None)
                .unwrap()
                .total_bytes,
            Some(0)
        );

        fs::write(checkout.join("committed"), "committed change\n").unwrap();
        git(&checkout, &["add", "committed"]);
        git(&checkout, &["commit", "-m", "committed change"]);
        fs::write(checkout.join("file"), "working change\n").unwrap();
        let estimate =
            estimate_delta(&FileGit, &checkout, &CheckoutOwnership::Managed, None).unwrap();
        let snapshot = capture_workspace_snapshot(
            &FileGit,
            &checkout,
            &CheckoutOwnership::Managed,
            &pack_base,
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        let pack = snapshot.pack.as_ref().unwrap();
        let pack_len = fs::metadata(pack).unwrap().len();
        assert_eq!(estimate.total_bytes, Some(pack_len));

        let root = Repo {
            temp: other,
            origin,
            checkout,
        };
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD^{tree}"]),
            git(&root.checkout, &["rev-parse", "HEAD^{tree}"])
        );
        assert_eq!(
            fs::read(restored.join("file")).unwrap(),
            b"working change\n"
        );
        git(&restored, &["fsck", "--full"]);
    }

    // Hard-won: 227e874ea: retained source/configuration regression.
    #[cfg(unix)]
    #[test]
    fn integration_snapshot_fetch_honors_repository_ssh_command() {
        let root = Repo::new();
        let staging = root.provision("staging");
        root.commit("published after the seed was cloned\n");
        git(&root.checkout, &["push", "origin", "main"]);
        let ssh = root.temp.path().join("ssh.sh");
        fs::write(
            &ssh,
            format!("exec git-upload-pack '{}'\n", root.origin.display()),
        )
        .unwrap();
        for checkout in [&root.checkout, &staging] {
            git(
                checkout,
                &[
                    "config",
                    "core.sshCommand",
                    &format!("sh '{}'", ssh.display()),
                ],
            );
        }
        git(
            &root.checkout,
            &["remote", "set-url", "origin", "ssh://snapshot.invalid/repo"],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert!(!snapshot.remote_refs_stale);
        assert!(snapshot.pack.is_none());
        let notices = restore_workspace_snapshot(&FileGit, &snapshot, &staging).unwrap();
        assert!(notices.is_empty(), "{notices:?}");
        assert_eq!(
            git(&staging, &["rev-parse", "HEAD"]),
            git(&root.checkout, &["rev-parse", "HEAD"])
        );
    }

    // Hard-won: 227e874ea: retained source/configuration regression.
    #[cfg(unix)]
    #[test]
    fn integration_review_legacy_git_preserves_quoted_nested_worktree_paths() {
        let root = Repo::new();
        let relative = Path::new("lanes/tab\tquote\"slash\\café");
        let nested = root.checkout.join(relative);
        git(
            &root.checkout,
            &["worktree", "add", "--detach", nested.to_str().unwrap()],
        );
        fs::write(nested.join("file"), "unpublished nested commit\n").unwrap();
        git(&nested, &["commit", "-am", "nested commit"]);
        fs::write(nested.join("file"), "nested working edits\n").unwrap();
        let snapshot = capture_workspace_snapshot(
            &crate::test_support::Git231,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("quoted.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        assert_eq!(snapshot.worktrees.len(), 1);
        assert_eq!(snapshot.worktrees[0].relative_path, relative);
        let restored = root.restore(&snapshot, None).join(relative);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&nested, &["rev-parse", "HEAD"])
        );
        assert_eq!(git(&restored, &["diff"]), git(&nested, &["diff"]));
    }
    // Hard-won: 1d7cc60a7: consecutive identical stash OIDs did not create consecutive reflog entries.
    #[test]
    fn restore_edge_adjacent_stash_entries_preserve_every_oid_and_message() {
        let root = Repo::new();
        fs::write(root.checkout.join("file"), "stash A\n").unwrap();
        let a = git(&root.checkout, &["stash", "create"]);
        fs::write(root.checkout.join("file"), "stash B\n").unwrap();
        let b = git(&root.checkout, &["stash", "create"]);
        git(&root.checkout, &["stash", "store", "-m", "oldest A", &a]);
        for message in ["middle A", "newest A"] {
            git(&root.checkout, &["stash", "store", "-m", "temporary B", &b]);
            git(&root.checkout, &["stash", "store", "-m", message, &a]);
            git(&root.checkout, &["stash", "drop", "stash@{1}"]);
        }
        git(&root.checkout, &["reset", "--hard", "HEAD"]);
        let before = read_stash(&FileGit, &root.checkout).unwrap();
        assert_eq!(before.len(), 3);
        assert!(before.iter().all(|entry| entry.commit == a));
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&snapshot, None);
        assert_eq!(read_stash(&FileGit, &restored).unwrap(), before);
    }

    // Hard-won: 1548f4aa4: Unicode whitespace was stripped from symbolic HEAD ref names.
    #[test]
    fn review_eighth_symbolic_head_preserves_unicode_whitespace() {
        let root = Repo::new();
        git(&root.checkout, &["branch", "topic"]);
        let name = "topic\u{2003}";
        git(&root.checkout, &["checkout", "-b", name]);
        root.commit("distinct branch with Unicode whitespace\n");
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            snapshot.head,
            SnapshotHead::Branch {
                name: format!("refs/heads/{name}"),
                commit: git(&root.checkout, &["rev-parse", "HEAD"])
            }
        );
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["symbolic-ref", "HEAD"]),
            format!("refs/heads/{name}")
        );
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            snapshot.head.commit()
        );
        assert_ne!(
            git(&restored, &["rev-parse", "topic"]),
            snapshot.head.commit()
        );
    }

    // Hard-won: 58c5ca099: name-only ITA subtraction missed tracked nonempty and empty HEAD paths.
    #[test]
    fn review_tenth_existing_head_paths_with_intent_to_add_refuse_before_abort() {
        for empty in [false, true] {
            let root = Repo::new();
            if empty {
                fs::write(root.checkout.join("file"), b"").unwrap();
                git(&root.checkout, &["commit", "-am", "empty HEAD path"]);
            }
            git(&root.checkout, &["checkout", "-b", "side"]);
            fs::write(root.checkout.join("other"), b"side\n").unwrap();
            git(&root.checkout, &["add", "other"]);
            git(&root.checkout, &["commit", "-m", "side path"]);
            git(&root.checkout, &["checkout", "main"]);
            fs::write(root.checkout.join("other"), b"main\n").unwrap();
            git(&root.checkout, &["add", "other"]);
            git(&root.checkout, &["commit", "-m", "main path"]);
            root.start_conflict("merge");
            git(&root.checkout, &["rm", "--cached", "file"]);
            git(&root.checkout, &["add", "-N", "file"]);
            let index = fs::read(root.checkout.join(".git/index")).unwrap();
            let inventory =
                workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
            assert!(
                inventory
                    .intent_to_add
                    .iter()
                    .any(|entry| entry.value == Path::new("file"))
            );
            let error = capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("refused.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<SnapshotRefused>(),
                    Some(SnapshotRefused::IntentToAdd(_))
                ),
                "{error:#}"
            );
            assert_eq!(fs::read(root.checkout.join(".git/index")).unwrap(), index);
            assert!(root.checkout.join(".git/MERGE_HEAD").exists());
        }
    }

    // Hard-won: 58c5ca099: primary-scope reads dropped upstreams defined by the branch's linked worktree.
    #[test]
    fn review_tenth_nested_branch_upstreams_come_from_the_owning_worktree() {
        let root = Repo::new();
        git(
            &root.checkout,
            &["config", "extensions.worktreeConfig", "true"],
        );
        let nested = root.checkout.join("lanes/upstream");
        git(
            &root.checkout,
            &["worktree", "add", "-b", "lane", nested.to_str().unwrap()],
        );
        git(
            &nested,
            &["config", "--worktree", "branch.lane.remote", "origin"],
        );
        git(
            &nested,
            &[
                "config",
                "--worktree",
                "branch.lane.merge",
                "refs/heads/main",
            ],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            snapshot.branch_tracking["lane"],
            BranchTracking {
                remote: Some("origin".into()),
                merge: vec!["refs/heads/main".into()]
            }
        );
        let restored = root.restore(&snapshot, None);
        let restored_lane = restored.join("lanes/upstream");
        assert_eq!(
            git(
                &restored_lane,
                &["rev-parse", "--abbrev-ref", "@{upstream}"]
            ),
            "origin/main"
        );
        git(&restored_lane, &["pull", "--ff-only"]);
        assert_eq!(
            git(&restored_lane, &["symbolic-ref", "HEAD"]),
            "refs/heads/lane"
        );
    }
    // Hard-won: ecc543144: an origin pull mapping could contaminate its own ordinary tracking namespace.
    #[test]
    fn review_eleventh_offline_origin_mixed_sources_keep_pull_only_history_in_pack() {
        let root = Repo::new();
        root.commit("pull-only unpublished branch history\n");
        let tip = git(&root.checkout, &["rev-parse", "HEAD"]);
        git(&root.checkout, &["push", "origin", "HEAD:refs/pull/1/head"]);
        git(
            &root.checkout,
            &[
                "config",
                "--add",
                "remote.origin.fetch",
                "+refs/pull/1/head:refs/remotes/origin/pull-cache",
            ],
        );
        git(&root.checkout, &["fetch", "origin"]);
        let evidence = origin_tracking_objects(
            &FileGit,
            &root.checkout,
            &read_all_refs(&FileGit, &root.checkout).unwrap(),
        )
        .unwrap();
        assert!(evidence.ambiguous);
        assert!(!evidence.objects.contains(&tip));
        let away = root.temp.path().join("offline.git");
        fs::rename(&root.origin, &away).unwrap();
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_some());
        assert!(snapshot.remote_refs_stale);
        fs::rename(&away, &root.origin).unwrap();
        git(&root.origin, &["update-ref", "-d", "refs/pull/1/head"]);
        git(&root.origin, &["reflog", "expire", "--expire=now", "--all"]);
        git(&root.origin, &["gc", "--prune=now"]);
        assert_ne!(
            run_git(&FileGit, &root.origin, ["cat-file", "-e", &tip], &[])
                .unwrap()
                .status,
            0
        );
        fs::remove_dir_all(&root.checkout).unwrap();
        let restored = root.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", "HEAD"]), tip);
        assert_eq!(
            fs::read(restored.join("file")).unwrap(),
            b"pull-only unpublished branch history\n"
        );
    }

    // Hard-won: e0cc04156: preserving inode and mtime let changed file sizes reuse discard consent.
    #[test]
    fn review_twelfth_shown_file_size_changes_refuse_even_with_preserved_mtime() {
        for nested in [false, true] {
            let root = Repo::new();
            let path = if nested {
                let embedded = root.checkout.join("embedded");
                git(
                    root.temp.path(),
                    &[
                        "clone",
                        root.origin.to_str().unwrap(),
                        embedded.to_str().unwrap(),
                    ],
                );
                embedded.join("file")
            } else {
                root.checkout.join("untracked")
            };
            if !nested {
                fs::write(&path, b"before").unwrap();
            }
            let inventory =
                workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
            let shown = inventory
                .untracked
                .iter()
                .map(|entry| UntrackedItem::Path {
                    checkout: entry.checkout.clone(),
                    path: entry.path.clone(),
                    identity: entry.identity.clone(),
                })
                .collect();
            let before = fs::metadata(&path).unwrap();
            let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_len(before.len() + 123).unwrap();
            file.set_times(fs::FileTimes::new().set_modified(before.modified().unwrap()))
                .unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().modified().unwrap(),
                before.modified().unwrap()
            );
            let error = capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("refused.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Shown(shown),
                    operations: OperationConsent::None,
                },
                None,
            )
            .unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<SnapshotRefused>(),
                    Some(SnapshotRefused::Untracked(_))
                ),
                "{error:#}"
            );
        }
    }

    #[test]
    fn seeded_published_submodule_positions_round_trip_after_source_deletion() {
        for seed in 41..=48 {
            eprintln!("snapshot module property seed={seed}");
            let r = Repo::new();
            let module = Repo::new();
            let mut directory = add_submodule(&r, &module);
            let module_path = if seed == 44 { "sub[0]" } else { "sub" };
            if seed == 44 {
                git(&r.checkout, &["mv", "sub", module_path]);
                git(
                    &r.checkout,
                    &["submodule", "add", module.origin.to_str().unwrap(), "sub0"],
                );
                git(
                    &r.checkout,
                    &[
                        "commit",
                        "-am",
                        "literal module path and uninitialized sibling",
                    ],
                );
                git(&r.checkout, &["push", "origin", "main"]);
                git(&r.checkout, &["submodule", "deinit", "-f", "sub0"]);
                fs::remove_dir_all(r.checkout.join(".git/modules/sub0")).unwrap();
                directory = r.checkout.join(module_path);
            }
            let old = git(&directory, &["rev-parse", "HEAD"]);
            module.commit(&format!("published module {seed}\n"));
            git(&module.checkout, &["push", "origin", "main"]);
            git(&directory, &["fetch", "origin"]);
            let new = git(&module.checkout, &["rev-parse", "HEAD"]);
            git(
                &directory,
                &[
                    "checkout",
                    "--detach",
                    if seed % 2 == 0 { &old } else { &new },
                ],
            );
            if seed % 3 == 0 {
                git(
                    &r.checkout,
                    &[
                        "update-index",
                        "--cacheinfo",
                        &format!("160000,{new},{module_path}"),
                    ],
                );
            }
            generated_commands(&r.checkout, &[PathBuf::new()], seed);
            let paths = vec![PathBuf::new(), PathBuf::from(module_path)];
            let snapshot = r.capture(&CheckoutOwnership::Managed);
            // Native update selects the parent's saved gitlink, not the
            // module's previous branch attachment or checkout position.
            let saved_pin = git(&r.checkout, &["rev-parse", &format!(":{module_path}")]);
            git(&directory, &["checkout", "--detach", &saved_pin]);
            let expected = state(&r.checkout, &paths);
            fs::remove_dir_all(&r.checkout).unwrap();
            let (restored, baseline) = r.restore_with_baseline(&snapshot);
            assert_projected_state(expected, state(&restored, &paths), None, &baseline);
            assert!(
                !git(
                    &restored,
                    &[
                        "--literal-pathspecs",
                        "submodule",
                        "status",
                        "--",
                        module_path
                    ]
                )
                .starts_with('-')
            );
            if seed == 44 {
                assert!(
                    !restored.join("sub0/.git").exists(),
                    "literal path initialized an unrelated module"
                );
            }
        }
    }
    #[test]
    fn raw_operation_saves_literal_head_and_refs_without_mutating_the_checkout() {
        for kind in ["merge", "rebase", "cherry-pick"] {
            let r = Repo::new();
            r.conflict();
            r.start_conflict(kind);
            let paths = vec![PathBuf::new()];
            let before = raw_state(&r.checkout, &paths);
            let head = read_head(&FileGit, &r.checkout).unwrap();
            let refs = read_refs(&FileGit, &r.checkout).unwrap();
            let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
            assert_eq!(raw_state(&r.checkout, &paths), before);
            assert_eq!(snapshot.head, head);
            assert!(snapshot.working_state_skipped);
            assert!(snapshot.working_state.is_none());
            assert_eq!(
                snapshot
                    .notices
                    .iter()
                    .filter(|notice| notice.as_str() == RAW_NOTICE)
                    .count(),
                1
            );
            fs::remove_dir_all(&r.checkout).unwrap();
            let restored = r.restore(&snapshot, None);
            assert_eq!(read_head(&FileGit, &restored).unwrap(), head);
            for (name, oid) in snapshot.metadata.refs {
                assert_eq!(refs[&name], oid);
            }
            assert!(git(&restored, &["status", "--porcelain"]).is_empty());
        }
        let r = Repo::new();
        r.conflict();
        r.start_conflict("merge");
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(operation(&FileGit, &r.checkout).unwrap().is_none());
        let expected = state(&r.checkout, &[PathBuf::new()]);
        fs::remove_dir_all(&r.checkout).unwrap();
        let (restored, baseline) = r.restore_with_baseline(&snapshot);
        assert_projected_state(
            expected,
            state(&restored, &[PathBuf::new()]),
            None,
            &baseline,
        );
    }
    #[test]
    fn boundary_prerequisites_recover_by_sha_and_name_rewritten_history() {
        for discard in [false, true] {
            let r = Repo::new();
            r.commit("published boundary\n");
            git(&r.checkout, &["push", "origin", "main"]);
            let boundary = git(&r.checkout, &["rev-parse", "HEAD"]);
            r.commit("local child\n");
            let snapshot = r.capture(&CheckoutOwnership::Managed);
            assert!(snapshot.prerequisites.contains(&boundary));
            let base = git(&r.checkout, &["rev-parse", "HEAD~2"]);
            git(&r.origin, &["update-ref", "refs/heads/main", &base]);
            if discard {
                git(&r.origin, &["reflog", "expire", "--expire=now", "--all"]);
                git(&r.origin, &["gc", "--prune=now"]);
            }
            fs::remove_dir_all(&r.checkout).unwrap();
            let staging = r.provision("staging");
            let result =
                restore_workspace_snapshot(&crate::test_support::Git231, &snapshot, &staging);
            if discard {
                let error = format!("{:#}", result.unwrap_err());
                assert!(
                    error.contains(&boundary)
                        && error.contains("origin may have rewritten history"),
                    "{error}"
                );
            } else {
                result.unwrap();
                assert_eq!(
                    git(&staging, &["rev-parse", "HEAD"]),
                    snapshot.head.commit()
                );
            }
        }
    }
    #[test]
    fn legacy_git_repairs_nested_worktrees_after_the_staging_rename() {
        let r = Repo::new();
        let relative = Path::new("lanes/legacy");
        git(
            &r.checkout,
            &[
                "worktree",
                "add",
                "--detach",
                r.checkout.join(relative).to_str().unwrap(),
            ],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        fs::remove_dir_all(&r.checkout).unwrap();
        let staging = r.provision("staging");
        restore_workspace_snapshot(&crate::test_support::Git231, &snapshot, &staging).unwrap();
        let final_path = r.temp.path().join("final");
        fs::rename(staging, &final_path).unwrap();
        repair_workspace_snapshot(&crate::test_support::Git231, &snapshot, &final_path).unwrap();
        assert_eq!(
            fs::canonicalize(git(
                &final_path.join(relative),
                &["rev-parse", "--show-toplevel"]
            ))
            .unwrap(),
            fs::canonicalize(final_path.join(relative)).unwrap()
        );
    }

    // Hard-won: 227e874ea: retained Git fidelity regression.
    #[test]
    fn raw_origin_refresh_fetches_objects_without_changing_refs_or_fetch_head() {
        let r = Repo::new();
        record_launch(&FileGit, &r.checkout, "raw-refresh").unwrap();
        let launch = LaunchRecord::read(&FileGit, &r.checkout, "raw-refresh").unwrap();
        let before = read_all_refs(&FileGit, &r.checkout).unwrap();
        let fetch_head = r.checkout.join(".git/FETCH_HEAD");
        fs::write(&fetch_head, "unchanged sentinel\n").unwrap();
        let publisher = r.temp.path().join("publisher");
        git(
            r.temp.path(),
            &[
                "clone",
                r.origin.to_str().unwrap(),
                publisher.to_str().unwrap(),
            ],
        );
        fs::write(publisher.join("published"), "published after raw launch\n").unwrap();
        git(&publisher, &["add", "published"]);
        git(&publisher, &["commit", "-m", "published"]);
        git(&publisher, &["push", "origin", "main"]);
        let published = git(&publisher, &["rev-parse", "HEAD"]);
        r.commit("raw session\n");
        let expected_refs = read_all_refs(&FileGit, &r.checkout).unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch });
        assert_eq!(read_all_refs(&FileGit, &r.checkout).unwrap(), expected_refs);
        assert_eq!(
            fs::read_to_string(&fetch_head).unwrap(),
            "unchanged sentinel\n"
        );
        assert!(
            snapshot
                .refs
                .keys()
                .all(|name| !name.starts_with("refs/remotes/"))
        );
        assert!(!snapshot.remote_refs_stale);
        git(&r.checkout, &["cat-file", "-e", &published]);
        assert_eq!(
            before["refs/remotes/origin/main"],
            expected_refs["refs/remotes/origin/main"]
        );
        let restored = r.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", "origin/main"]), published);
    }

    // Hard-won: a2932eae2: failed tip downloads reused force-pushed-away tracking refs.
    #[test]
    fn advertised_but_unservable_tips_never_use_stale_tracking_evidence() {
        let r = Repo::new();
        let unavailable = "1234567890123456789012345678901234567890";
        // ls-remote advertises a loose ref without reading its object; upload-pack
        // cannot serve it. This exercises a successful query and a real fetch failure.
        fs::write(r.origin.join("refs/heads/main"), format!("{unavailable}\n")).unwrap();
        assert!(git(&r.checkout, &["ls-remote", "origin"]).contains(unavailable));
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(!snapshot.remote_refs_stale);
        assert!(snapshot.pack.is_some());
        // Restore receives a fresh provisioned clone, after origin is usable.
        git(
            &r.origin,
            &["update-ref", "refs/heads/main", snapshot.head.commit()],
        );
        let restored = r.temp.path().join("fresh");
        restore_workspace_snapshot(&FileGit, &snapshot, &r.provision("fresh")).unwrap();
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            snapshot.head.commit()
        );
        git(&restored, &["fsck", "--no-reflogs"]);

        let root = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&root, &module);
        fs::write(
            module.origin.join("refs/heads/main"),
            format!("{unavailable}\n"),
        )
        .unwrap();
        assert!(git(&directory, &["ls-remote", "origin"]).contains(unavailable));
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("refused.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::DirtySubmodule(_))
            ),
            "{error:#}"
        );
    }

    // Hard-won: bae1ae4d3: custom origin refs excluded trees absent from a normal clone.
    #[test]
    fn custom_origin_refs_do_not_exclude_shared_trees() {
        let r = Repo::new();
        r.commit("only in custom origin ref\n");
        git(&r.checkout, &["push", "origin", "HEAD:refs/pull/1/head"]);
        git(
            &r.checkout,
            &["commit", "--amend", "-m", "distinct commit sharing tree"],
        );
        let file_origin = format!("file://{}", r.origin.display());
        git(&r.checkout, &["remote", "set-url", "origin", &file_origin]);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            fs::read_to_string(restored.join("file")).unwrap(),
            "only in custom origin ref\n"
        );
        git(&restored, &["fsck", "--no-reflogs"]);
    }

    // Hard-won: bae1ae4d3: raw stash filtering discarded a newly stored old object.
    #[test]
    fn raw_new_stash_message_keeps_a_reused_launch_object() {
        let r = Repo::new();
        fs::write(r.checkout.join("file"), "old stash\n").unwrap();
        git(&r.checkout, &["stash", "push", "-m", "old"]);
        let old = git(&r.checkout, &["rev-parse", "refs/stash"]);
        record_launch(&FileGit, &r.checkout, "stash-session").unwrap();
        fs::write(r.checkout.join("file"), "new stash\n").unwrap();
        git(&r.checkout, &["stash", "push", "-m", "new"]);
        git(
            &r.checkout,
            &["stash", "store", "-m", "reused object", &old],
        );
        let snapshot = r.capture(&CheckoutOwnership::Raw {
            launch: LaunchRecord::read(&FileGit, &r.checkout, "stash-session").unwrap(),
        });
        assert_eq!(snapshot.stash.len(), 2);
        assert_eq!(snapshot.stash[0].message, "reused object");
        let restored = r.restore(&snapshot, None);
        assert_eq!(read_stash(&FileGit, &restored).unwrap(), snapshot.stash);
    }

    // Hard-won: d00d140bd: even fully published leftover state is not represented by a snapshot.
    #[test]
    fn structural_git_state_refuses_published_leftovers_and_clean_module_worktrees_and_warns_raw() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        git(
            &root.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                module.origin.to_str().unwrap(),
                "retired",
            ],
        );
        git(
            &root.checkout,
            &["commit", "-am", "published retired module"],
        );
        git(&root.checkout, &["push", "origin", "main"]);
        git(&root.checkout, &["submodule", "deinit", "-f", "retired"]);
        let leftover = root.checkout.join(".git/modules/retired");
        let source = root.checkout.join("sub");
        let linked = source.join("local-lane");
        git(
            &source,
            &[
                "worktree",
                "add",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        );
        fs::write(
            git_path(&FileGit, &source, "info/exclude").unwrap(),
            "local-lane/\n",
        )
        .unwrap();
        git(&source, &["worktree", "lock", linked.to_str().unwrap()]);
        let outside = root.temp.path().join("outside-root-with-module");
        git(
            &root.checkout,
            &["worktree", "add", "--detach", outside.to_str().unwrap()],
        );
        git(
            &outside,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "sub",
            ],
        );
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("structure.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::UnrepresentedGitState { .. })
            ),
            "{error:#}"
        );
        for path in [&leftover, &linked, &outside] {
            assert!(
                error.to_string().contains(path.to_str().unwrap()),
                "{error:#}"
            );
        }
        assert!(error.to_string().contains("worktree remove --force"));
        assert!(
            error.to_string().contains("locked")
                && error.to_string().contains("worktree unlock")
                && error.to_string().contains("once it is available")
        );
        assert!(!error.to_string().contains("move it inside"));
        let raw = root.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(raw.notices.iter().any(|notice| notice == RAW_NOTICE));
        assert!(linked.join(".git").exists());
        assert!(leftover.exists());
    }

    // Hard-won: 8251715a1: inactive submodule branches and stashes escaped snapshot loss checks.
    #[test]
    fn inactive_submodule_work_refuses_managed_capture_and_warns_raw() {
        let root = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&root, &module);
        let published = git(&directory, &["rev-parse", "HEAD"]);
        git(&directory, &["switch", "-c", "private"]);
        fs::write(directory.join("file"), "inactive unpublished work\n").unwrap();
        git(&directory, &["commit", "-am", "private work"]);
        git(&directory, &["checkout", "--detach", &published]);
        fs::write(directory.join("file"), "stashed work\n").unwrap();
        git(&directory, &["stash", "push", "-m", "private stash"]);
        let capture = |ownership| {
            capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                ownership,
                &root.temp.path().join("work.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
        };
        let refused = capture(&CheckoutOwnership::Managed).unwrap_err();
        assert!(matches!(
            refused.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::DirtySubmodule(_))
        ));
        assert!(
            refused.to_string().contains("submodule workspace::sub"),
            "{refused:#}"
        );
        let raw = capture(&CheckoutOwnership::Raw { launch: None }).unwrap();
        assert!(raw.notices.iter().any(|notice| notice == RAW_NOTICE));
        git(&directory, &["push", "origin", "private"]);
        assert!(capture(&CheckoutOwnership::Managed).is_err());
        git(&directory, &["stash", "drop"]);
        capture(&CheckoutOwnership::Managed).unwrap();
    }

    // Hard-won: 58c5ca099: an external shared origin lost its only objects during source teardown.
    #[test]
    fn review_tenth_shared_origin_and_recursive_alternates_must_be_independent() {
        let mut root = Repo::new();
        let moved = root.temp.path().join(if cfg!(unix) {
            "checkout\towned"
        } else {
            "checkout-owned"
        });
        fs::rename(&root.checkout, &moved).unwrap();
        root.checkout = moved;
        root.commit("only source owns this commit\n");
        let first = root.temp.path().join("shared-first.git");
        let last = root.temp.path().join("shared-recursive.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--shared",
                "--bare",
                root.checkout.to_str().unwrap(),
                first.to_str().unwrap(),
            ],
        );
        git(
            root.temp.path(),
            &[
                "clone",
                "--shared",
                "--bare",
                first.to_str().unwrap(),
                last.to_str().unwrap(),
            ],
        );
        git(
            &root.checkout,
            &["remote", "set-url", "origin", last.to_str().unwrap()],
        );
        let inventory =
            workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
        assert!(inventory.origins_removed_by_teardown.iter().any(|origin| {
            origin
                .value
                .object_provider
                .as_ref()
                .is_some_and(|provider| provider.starts_with(&root.checkout))
        }));
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("refused.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::OriginRemovedByTeardown(_))
            ),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("repack -a -d")
                && error.to_string().contains("objects/info/alternates")
                && error.to_string().contains(last.to_str().unwrap()),
            "{error:#}"
        );
        assert!(
            workspace_inventory(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Raw { launch: None }
            )
            .unwrap()
            .origins_removed_by_teardown
            .is_empty()
        );
        let alternates = first.join("objects/info/alternates");
        let original = fs::read(&alternates).unwrap();
        let mut malformed = original.clone();
        malformed.extend_from_slice(b"\"unterminated\n");
        fs::write(&alternates, malformed).unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("invalid-provider.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("cannot inspect local origin object storage")
                && message.contains(last.to_str().unwrap())
                && message.contains("repair its objects/info/alternates"),
            "{message}"
        );
        fs::write(alternates, original).unwrap();
        git(&last, &["repack", "-a", "-d"]);
        fs::remove_file(last.join("objects/info/alternates")).unwrap();
        #[cfg(unix)]
        {
            // Native provider names are canonical. The literal route can
            // still cross a deleted symlink even when its target survives.
            let route = root.checkout.join("borrowed-objects");
            std::os::unix::fs::symlink(root.origin.join("objects"), &route).unwrap();
            fs::write(
                last.join("objects/info/alternates"),
                format!("{}\n", route.display()),
            )
            .unwrap();
            let inventory =
                workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
            assert!(
                inventory
                    .origins_removed_by_teardown
                    .iter()
                    .any(|origin| origin.value.object_provider.as_ref() == Some(&route))
            );
            fs::remove_file(last.join("objects/info/alternates")).unwrap();
            fs::remove_file(route).unwrap();
        }
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_none());
        let saved = snapshot.head.commit().to_owned();
        fs::remove_dir_all(&root.checkout).unwrap();
        let restored = root.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", "HEAD"]), saved);
        assert_eq!(
            fs::read(restored.join("file")).unwrap(),
            b"only source owns this commit\n"
        );
    }

    #[test]
    fn seeded_submodule_failures_are_notices_and_configuration_is_a_declared_loss() {
        for seed in 49..=56 {
            eprintln!("snapshot notice property seed={seed}");
            let r = Repo::new();
            let module = Repo::new();
            add_submodule(&r, &module);
            generated_commands(&r.checkout, &[PathBuf::new()], seed);
            if seed % 2 == 0 {
                // A root-owned gitlink can lie below an old initialized module.
                // Its path prefix must never move it to that module's index.
                let pin = git(&module.checkout, &["rev-parse", "HEAD"]);
                git(&r.checkout, &["update-index", "--force-remove", "sub"]);
                git(
                    &r.checkout,
                    &[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("160000,{pin},sub/child"),
                    ],
                );
            }
            let expected = state(&r.checkout, &[PathBuf::new()]);
            let snapshot = r.capture(&CheckoutOwnership::Managed);
            fs::remove_dir_all(&r.checkout).unwrap();
            fs::rename(&module.origin, module.temp.path().join("offline.git")).unwrap();
            let staging = r.provision("staging");
            let baseline = state(&staging, &[]).refs;
            let notices = restore_workspace_snapshot(&FileGit, &snapshot, &staging).unwrap();
            assert!(
                notices
                    .iter()
                    .any(|notice| notice.contains("submodule sub")
                        && notice.contains("not initialized")),
                "seed {seed}: {notices:?}"
            );
            assert_eq!(git(&staging, &["remote"]), "origin");
            assert!(
                config_values(&FileGit, &staging, "remote.origin.pushurl")
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                run_git(
                    &FileGit,
                    &staging,
                    ["config", "--local", "--get-regexp", r"^url\."],
                    &[]
                )
                .unwrap()
                .status,
                1
            );
            assert_projected_state(
                expected,
                state(&staging, &[PathBuf::new()]),
                None,
                &baseline,
            );
        }
    }

    // Hard-won: 227e874ea: managed origins must survive the complete teardown.
    #[test]
    fn review_fourteenth_local_shallow_origins_refuse_before_managed_abort() {
        for operation in [false, true] {
            let root = Repo::new();
            root.commit("history below the shallow origin tip\n");
            git(&root.checkout, &["push", "origin", "main"]);
            let uri = url::Url::from_directory_path(&root.origin)
                .unwrap()
                .to_string();
            let shallow = root.temp.path().join("shallow.git");
            git(
                root.temp.path(),
                &[
                    "clone",
                    "--bare",
                    "--depth",
                    "1",
                    &uri,
                    shallow.to_str().unwrap(),
                ],
            );
            git(
                &root.checkout,
                &["remote", "set-url", "origin", shallow.to_str().unwrap()],
            );
            assert_eq!(
                git(&root.checkout, &["rev-parse", "--is-shallow-repository"]),
                "false"
            );
            if operation {
                root.conflict();
                root.start_conflict("merge");
            }
            let index = fs::read(root.checkout.join(".git/index")).unwrap();
            let file = fs::read(root.checkout.join("file")).unwrap();
            let error = capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("refused.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap_err();
            assert!(
                matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::UnrepresentedGitState { directories, .. }) if directories.iter().any(|row| row.value.ends_with("shallow"))),
                "{error:#}"
            );
            assert_eq!(fs::read(root.checkout.join(".git/index")).unwrap(), index);
            assert_eq!(fs::read(root.checkout.join("file")).unwrap(), file);
            assert_eq!(root.checkout.join(".git/MERGE_HEAD").exists(), operation);
        }
        let root = Repo::new();
        let module = Repo::new();
        module.commit("module ancestry\n");
        git(&module.checkout, &["push", "origin", "main"]);
        let directory = add_submodule(&root, &module);
        let shallow = module.temp.path().join("shallow.git");
        let uri = url::Url::from_directory_path(&module.origin)
            .unwrap()
            .to_string();
        git(
            module.temp.path(),
            &[
                "clone",
                "--bare",
                "--depth",
                "1",
                &uri,
                shallow.to_str().unwrap(),
            ],
        );
        git(
            &directory,
            &["remote", "set-url", "origin", shallow.to_str().unwrap()],
        );
        let inventory =
            workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
        assert!(
            inventory
                .presence
                .iter()
                .any(|row| row.value.starts_with(&shallow)),
            "{inventory:?}"
        );
    }

    // Hard-won: 227e874ea: managed origins must survive the complete teardown.
    #[test]
    fn review_seventh_uninitialized_module_mirrors_must_survive_teardown() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(root.checkout.join(".git/modules")).unwrap();
        let mirror = root.checkout.join("mirror.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        fs::write(root.checkout.join(".git/info/exclude"), "mirror.git\n").unwrap();
        let host = host_git(root.temp.path(), "module-routes");
        git(
            root.temp.path(),
            &[
                "config",
                "--file",
                host.global.to_str().unwrap(),
                &format!("url.{}.insteadOf", mirror.display()),
                "module-mirror:",
            ],
        );
        git(
            &root.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", module.origin.display()),
                "module-mirror:",
            ],
        );
        git(
            &root.checkout,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.sub.url",
                "module-mirror:",
            ],
        );
        for changed_pin in [false, true] {
            if changed_pin {
                module.commit("new pin\n");
                git(
                    &module.checkout,
                    &["push", mirror.to_str().unwrap(), "main"],
                );
                git(
                    &root.checkout,
                    &[
                        "update-index",
                        "--cacheinfo",
                        &format!(
                            "160000,{},sub",
                            git(&module.checkout, &["rev-parse", "HEAD"])
                        ),
                    ],
                );
            }
            let inventory =
                workspace_inventory(&host, &root.checkout, &CheckoutOwnership::Managed).unwrap();
            assert_eq!(inventory.origins_removed_by_teardown.len(), 1);
            let dependency = &inventory.origins_removed_by_teardown[0];
            assert_eq!(
                dependency.value.canonical_path,
                fs::canonicalize(&mirror).unwrap()
            );
            assert_eq!(
                dependency.value.module_url_key.as_deref(),
                Some("submodule.sub.url")
            );
            let error = capture_workspace_snapshot(
                &host,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("refused.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<SnapshotRefused>(),
                    Some(SnapshotRefused::OriginRemovedByTeardown(_))
                ),
                "{error:#}"
            );
            assert!(error.to_string().contains("config -f .gitmodules"));
            assert!(
                workspace_inventory(
                    &host,
                    &root.checkout,
                    &CheckoutOwnership::Raw { launch: None }
                )
                .unwrap()
                .origins_removed_by_teardown
                .is_empty()
            );
        }
        // Updating .gitmodules to an independent origin removes the dependency.
        git(
            &root.checkout,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.sub.url",
                mirror.to_str().unwrap(),
            ],
        );
        git(
            &root.checkout,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.sub.url",
                module.origin.to_str().unwrap(),
            ],
        );
        assert!(
            workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed)
                .unwrap()
                .origins_removed_by_teardown
                .is_empty()
        );
    }

    // Hard-won: 227e874ea: managed origins must survive the complete teardown.
    #[test]
    fn review_tenth_module_origins_cannot_borrow_from_initialized_or_cached_admins() {
        for initialized in [true, false] {
            let root = Repo::new();
            let module = Repo::new();
            let directory = add_submodule(&root, &module);
            let mirror = root.temp.path().join("module-shared.git");
            git(
                root.temp.path(),
                &[
                    "clone",
                    "--shared",
                    "--bare",
                    directory.to_str().unwrap(),
                    mirror.to_str().unwrap(),
                ],
            );
            git(
                &directory,
                &["remote", "set-url", "origin", mirror.to_str().unwrap()],
            );
            if !initialized {
                git(
                    &root.checkout,
                    &[
                        "config",
                        "-f",
                        ".gitmodules",
                        "submodule.sub.url",
                        mirror.to_str().unwrap(),
                    ],
                );
                git(&root.checkout, &["add", ".gitmodules"]);
                git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
            }
            let inventory =
                workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
            let expected_modules = fs::canonicalize(root.checkout.join(".git/modules")).unwrap();
            assert!(inventory.origins_removed_by_teardown.iter().any(|origin| {
                origin
                    .value
                    .object_provider
                    .as_ref()
                    .is_some_and(|provider| {
                        fs::canonicalize(provider)
                            .unwrap_or_else(|_| provider.clone())
                            .starts_with(&expected_modules)
                    })
            }));
            let error = capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("refused.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<SnapshotRefused>(),
                    Some(SnapshotRefused::OriginRemovedByTeardown(_))
                ),
                "{error:#}"
            );
            git(&mirror, &["repack", "-a", "-d"]);
            fs::remove_file(mirror.join("objects/info/alternates")).unwrap();
            if !initialized {
                fs::remove_dir_all(root.checkout.join(".git/modules")).unwrap();
            }
            root.capture(&CheckoutOwnership::Managed);
        }
    }

    // Hard-won: 227e874ea: managed origins must survive the complete teardown.
    #[test]
    fn review_edge_managed_origins_must_survive_all_selected_teardown_roots() {
        let root = Repo::new();
        let mirror = root.checkout.join("hidden.git");
        fs::write(root.checkout.join(".git/info/exclude"), "hidden.git\n").unwrap();
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                root.origin.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        root.conflict();
        root.start_conflict("merge");
        for origin in [
            mirror.to_str().unwrap().to_owned(),
            url::Url::from_directory_path(&mirror).unwrap().to_string(),
        ] {
            git(&root.checkout, &["remote", "set-url", "origin", &origin]);
            let inventory =
                workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
            assert!(matches!(
                inventory.blockers[0].kind,
                SnapshotRefused::OriginRemovedByTeardown(_)
            ));
            let error = capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("refused"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<SnapshotRefused>(),
                    Some(SnapshotRefused::OriginRemovedByTeardown(_))
                ),
                "{error:#}"
            );
            assert!(
                error
                    .to_string()
                    .contains("git remote set-url origin <new path>")
            );
            assert!(root.checkout.join(".git/MERGE_HEAD").exists());
        }
        #[cfg(unix)]
        {
            let alias = root.temp.path().join("outside-alias.git");
            std::os::unix::fs::symlink(&mirror, &alias).unwrap();
            git(
                &root.checkout,
                &["remote", "set-url", "origin", alias.to_str().unwrap()],
            );
            assert!(matches!(
                workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed)
                    .unwrap()
                    .blockers[0]
                    .kind,
                SnapshotRefused::OriginRemovedByTeardown(_)
            ));
        }
        git(
            &root.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", mirror.display()),
                "internal:",
            ],
        );
        git(
            &root.checkout,
            &["remote", "set-url", "origin", "internal:"],
        );
        assert!(matches!(
            workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed)
                .unwrap()
                .blockers[0]
                .kind,
            SnapshotRefused::OriginRemovedByTeardown(_)
        ));
        assert!(
            workspace_inventory(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Raw { launch: None }
            )
            .unwrap()
            .origins_removed_by_teardown
            .is_empty()
        );

        let parent = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&parent, &module);
        let module_mirror = parent.checkout.join("module-mirror.git");
        fs::write(
            parent.checkout.join(".git/info/exclude"),
            "module-mirror.git\n",
        )
        .unwrap();
        git(
            parent.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                module_mirror.to_str().unwrap(),
            ],
        );
        git(
            &parent.checkout,
            &[
                "config",
                "submodule.sub.url",
                module_mirror.to_str().unwrap(),
            ],
        );
        git(
            &directory,
            &[
                "remote",
                "set-url",
                "origin",
                module_mirror.to_str().unwrap(),
            ],
        );
        let inventory =
            workspace_inventory(&FileGit, &parent.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(
            inventory.origins_removed_by_teardown[0]
                .checkout
                .checkout_path,
            Path::new("sub")
        );

        let dependent = Repo::new();
        git(
            &dependent.checkout,
            &[
                "remote",
                "set-url",
                "origin",
                module_mirror.to_str().unwrap(),
            ],
        );
        let inputs =
            [("dependent", &dependent), ("parent", &parent)].map(|(id, root)| RepositoryInput {
                id: id.into(),
                checkout: root.checkout.clone(),
                ownership: CheckoutOwnership::Managed,
                origin_override: None,
                pack_path: root.temp.path().join("cross-root"),
                exclusions: ExclusionSource::Origin,
                keep_dir: None,
            });
        let inventory = session_inventory(&FileGit, &inputs).unwrap();
        assert!(matches!(
            inventory["dependent"].blockers[0].kind,
            SnapshotRefused::OriginRemovedByTeardown(_)
        ));
        let error = capture_session_snapshot(&FileGit, &inputs, &SessionCaptureConsent::default())
            .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::OriginRemovedByTeardown(_))
            ),
            "{error:#}"
        );
    }

    // Hard-won: 227e874ea: managed origins must survive the complete teardown.
    #[test]
    fn review_ninth_post_abort_routes_use_the_complete_session_teardown_set() {
        let first = Repo::new();
        let second = Repo::new();
        let module = Repo::new();
        add_submodule(&first, &module);
        git(&first.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(first.checkout.join(".git/modules")).unwrap();
        let mirror = second.checkout.join("mirror.git");
        git(
            second.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        fs::write(second.checkout.join(".git/info/exclude"), "mirror.git\n").unwrap();
        git(
            &first.checkout,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.sub.url",
                mirror.to_str().unwrap(),
            ],
        );
        git(&first.checkout, &["commit", "-am", "unsafe module route"]);
        git(&first.checkout, &["branch", "unsafe"]);
        git(
            &first.checkout,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.sub.url",
                module.origin.to_str().unwrap(),
            ],
        );
        git(&first.checkout, &["commit", "-am", "safe module route"]);
        first.commit("safe middle\n");
        first.commit("bad tip\n");
        git(&first.checkout, &["checkout", "unsafe"]);
        git(&first.checkout, &["bisect", "start", "main", "unsafe"]);
        assert_eq!(
            git(
                &first.checkout,
                &["config", "-f", ".gitmodules", "submodule.sub.url"]
            ),
            module.origin.to_str().unwrap()
        );
        let inputs = [("first", &first), ("second", &second)].map(|(id, root)| RepositoryInput {
            id: id.into(),
            checkout: root.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: root.temp.path().join("pending.pack"),
            exclusions: ExclusionSource::Origin,
            keep_dir: None,
        });
        let consent = SessionCaptureConsent {
            repositories: inputs
                .iter()
                .map(|input| {
                    (
                        input.id.clone(),
                        CaptureConsent {
                            untracked: UntrackedConsent::Any,
                            operations: OperationConsent::Any,
                        },
                    )
                })
                .collect(),
        };
        let error = capture_session_snapshot(&FileGit, &inputs, &consent).unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::OriginRemovedByTeardown(_))
            ),
            "{error:#}"
        );
        assert!(
            error.to_string().contains(mirror.to_str().unwrap()),
            "{error:#}"
        );
        assert!(
            !git_path(&FileGit, &first.checkout, "BISECT_START")
                .unwrap()
                .exists()
        );
        for root in [&first, &second] {
            assert!(!fs::read_dir(root.temp.path()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "pack")
            }));
        }
    }

    // Hard-won: 3f919bd96: capture inherited the archival runner's lazy-promisor-fetch prohibition.
    #[test]
    fn integration_review_partial_clone_hydrates_removed_origin_history_for_pack() {
        let mut root = Repo::new();
        let original_source = root.checkout.clone();
        let historical = "promised historical content\n".repeat(4000);
        fs::write(root.checkout.join("historical"), &historical).unwrap();
        git(&root.checkout, &["add", "historical"]);
        git(
            &root.checkout,
            &["commit", "-m", "history retained only locally"],
        );
        git(&root.checkout, &["push", "origin", "main"]);
        let blob = git(&root.checkout, &["rev-parse", "HEAD:historical"]);
        git(&root.origin, &["config", "uploadpack.allowFilter", "true"]);
        git(
            &root.origin,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        let partial = root.temp.path().join("partial");
        let url = url::Url::from_directory_path(&root.origin)
            .unwrap()
            .to_string();
        git(
            root.temp.path(),
            &[
                "clone",
                "--filter=blob:none",
                "--no-checkout",
                &url,
                partial.to_str().unwrap(),
            ],
        );
        git(&partial, &["branch", "retained", "HEAD"]);
        git(&root.checkout, &["checkout", "--orphan", "replacement"]);
        git(&root.checkout, &["rm", "-rf", "."]);
        fs::write(root.checkout.join("current"), "new origin root\n").unwrap();
        git(&root.checkout, &["add", "current"]);
        git(&root.checkout, &["commit", "-m", "replace origin history"]);
        git(&root.checkout, &["push", "--force", "origin", "HEAD:main"]);
        git(&partial, &["fetch", "origin"]);
        git_text(
            &CaptureGit::new(&FileGit),
            &partial,
            ["checkout", "-B", "main", "origin/main"],
        )
        .unwrap();
        let absent = run_git(&FileGit, &partial, ["cat-file", "-e", &blob], &[]).unwrap();
        assert_ne!(
            absent.status, 0,
            "fixture unexpectedly hydrated the historical blob"
        );
        root.checkout = partial;
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_some());
        fs::remove_dir_all(&root.checkout).unwrap();
        fs::remove_dir_all(original_source).unwrap();
        git(&root.origin, &["reflog", "expire", "--expire=now", "--all"]);
        git(&root.origin, &["gc", "--prune=now"]);
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["show", "retained:historical"]),
            historical.trim_end_matches('\n')
        );
        git(&restored, &["fsck", "--full"]);
    }

    #[test]
    fn raw_capture_without_launch_record_keeps_all_local_refs_and_stashes() {
        let r = Repo::new();
        git(&r.checkout, &["branch", "extra-branch"]);
        git(&r.checkout, &["tag", "v-local"]);
        git(&r.checkout, &["notes", "add", "-m", "local note"]);
        let head = git(&r.checkout, &["rev-parse", "HEAD"]);
        git(&r.checkout, &["update-ref", "refs/mj/durable-keep", &head]);
        r.commit("base tracked\n");
        fs::write(r.checkout.join("a.txt"), "stashed change\n").unwrap();
        git(&r.checkout, &["add", "a.txt"]);
        git(&r.checkout, &["stash", "push", "-m", "raw stash"]);

        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(snapshot.metadata.refs.contains_key("refs/heads/main"));
        assert!(snapshot.metadata.refs.contains_key("refs/heads/extra-branch"));
        assert!(snapshot.metadata.refs.contains_key("refs/tags/v-local"));
        assert!(snapshot.metadata.refs.contains_key("refs/notes/commits"));
        assert!(snapshot.metadata.refs.contains_key("refs/mj/durable-keep"));
        assert!(!snapshot.metadata.refs.contains_key("refs/stash"));
        assert_eq!(snapshot.metadata.stash.len(), 1);
    }

    #[test]
    fn record_launch_if_absent_from_subdirectory_is_idempotent_and_removable() {
        let r = Repo::new();
        let sub = r.checkout.join("nested/sub");
        fs::create_dir_all(&sub).unwrap();
        record_launch_if_absent(&SystemGit, &sub, "session-a").unwrap();
        record_launch_if_absent(&SystemGit, &sub, "session-b").unwrap();

        git(&r.checkout, &["branch", "post-launch"]);
        record_launch_if_absent(&SystemGit, &sub, "session-a").unwrap();

        let a = LaunchRecord::read(&SystemGit, &r.checkout, "session-a")
            .unwrap()
            .expect("session-a launch record");
        assert!(!a.refs.contains_key("refs/heads/post-launch"));

        remove_launch_record(&SystemGit, &sub, "session-a").unwrap();
        assert!(
            LaunchRecord::read(&SystemGit, &r.checkout, "session-a")
                .unwrap()
                .is_none()
        );
        assert!(
            LaunchRecord::read(&SystemGit, &r.checkout, "session-b")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn history_cutoff_capture_and_restore_without_origin_or_later_history() {
        let r = Repo::new();
        let cutoff = git(&r.checkout, &["rev-parse", "HEAD"]);
        // Add a later commit to origin that must never leak into the cutoff session.
        r.commit("later upstream commit\n");
        let later = git(&r.checkout, &["rev-parse", "HEAD"]);
        git(&r.checkout, &["push", "origin", "main"]);

        // Provision a cutoff checkout with no origin and only history up to `cutoff`.
        let cutoff_checkout = r.temp.path().join("cutoff-checkout");
        fs::create_dir_all(&cutoff_checkout).unwrap();
        git(&cutoff_checkout, &["init", "-b", "main"]);
        git(&cutoff_checkout, &["config", "user.name", "Test"]);
        git(
            &cutoff_checkout,
            &["config", "user.email", "test@example.com"],
        );
        git(
            &cutoff_checkout,
            &["fetch", r.origin.to_str().unwrap(), &cutoff],
        );
        git(&cutoff_checkout, &["checkout", "-B", "main", "FETCH_HEAD"]);

        // Agent work beyond cutoff: commit, branch, stash, staged+unstaged edits,
        // and even a manually added `origin` remote pointing at the full repo.
        let origin_url = url::Url::from_directory_path(&r.origin)
            .unwrap()
            .to_string();
        git(&cutoff_checkout, &["remote", "add", "origin", &origin_url]);
        git(&cutoff_checkout, &["checkout", "-b", "work"]);
        fs::write(cutoff_checkout.join("a.txt"), "cutoff work commit\n").unwrap();
        git(&cutoff_checkout, &["add", "a.txt"]);
        git(&cutoff_checkout, &["commit", "-m", "cutoff work"]);
        let work_head = git(&cutoff_checkout, &["rev-parse", "HEAD"]);
        fs::write(cutoff_checkout.join("a.txt"), "stashed change\n").unwrap();
        git(&cutoff_checkout, &["stash", "push", "-m", "cutoff stash"]);
        fs::write(cutoff_checkout.join("a.txt"), "staged change\n").unwrap();
        git(&cutoff_checkout, &["add", "a.txt"]);
        fs::write(cutoff_checkout.join("a.txt"), "unstaged change\n").unwrap();

        let pack_path = r.temp.path().join("cutoff.pack");
        let input = RepositoryInput {
            id: "repo".to_owned(),
            checkout: cutoff_checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path,
            exclusions: ExclusionSource::Commits(vec![cutoff.clone()]),
            keep_dir: None,
        };
        let snapshot = capture_session_snapshot(
            &FileGit,
            &[input],
            &SessionCaptureConsent::default(),
        )
        .unwrap()
        .repositories
        .remove("repo")
        .unwrap();

        assert!(snapshot.metadata.history_cutoff);
        assert!(snapshot.metadata.origin_url.is_none());
        assert!(snapshot.metadata.branch_tracking.is_empty());
        assert!(snapshot.pack.is_some());
        assert_eq!(snapshot.metadata.stash.len(), 1);
        assert!(snapshot.metadata.working_state.is_some());

        // Restore onto a fresh cutoff clone that has no `origin` remote.
        let fresh = r.temp.path().join("cutoff-fresh");
        fs::create_dir_all(&fresh).unwrap();
        git(&fresh, &["init", "-b", "main"]);
        git(&fresh, &["config", "user.name", "Test"]);
        git(&fresh, &["config", "user.email", "test@example.com"]);
        git(&fresh, &["fetch", r.origin.to_str().unwrap(), &cutoff]);
        git(&fresh, &["checkout", "-B", "main", "FETCH_HEAD"]);

        let notices = restore_workspace_snapshot(&FileGit, &snapshot, &fresh).unwrap();
        assert!(notices.is_empty());
        assert_eq!(git(&fresh, &["rev-parse", "HEAD"]), work_head);
        assert_eq!(git(&fresh, &["branch", "--show-current"]), "work");
        assert_eq!(
            fs::read_to_string(fresh.join("a.txt")).unwrap(),
            "unstaged change\n"
        );
        assert_eq!(git(&fresh, &["show", ":a.txt"]), "staged change");
        assert_eq!(git(&fresh, &["stash", "list"]).lines().count(), 1);
        // No origin remote was created and later upstream history was never fetched.
        let origin_check = run_git(&FileGit, &fresh, ["remote", "get-url", "origin"], &[]).unwrap();
        assert_ne!(origin_check.status, 0);
        let later_check = run_git(&FileGit, &fresh, ["cat-file", "-e", &later], &[]).unwrap();
        assert_ne!(later_check.status, 0);
    }

    #[test]
    fn history_cutoff_restore_fails_visibly_when_prerequisite_missing_without_fetching() {
        let r = Repo::new();
        let cutoff = git(&r.checkout, &["rev-parse", "HEAD"]);
        git(&r.checkout, &["remote", "remove", "origin"]);
        fs::write(r.checkout.join("a.txt"), "after cutoff\n").unwrap();
        git(&r.checkout, &["add", "a.txt"]);
        git(&r.checkout, &["commit", "-m", "after cutoff"]);

        let pack_path = r.temp.path().join("cutoff-missing-prereq.pack");
        let input = RepositoryInput {
            id: "repo".to_owned(),
            checkout: r.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path,
            exclusions: ExclusionSource::Commits(vec![cutoff.clone()]),
            keep_dir: None,
        };
        let snapshot = capture_session_snapshot(
            &FileGit,
            &[input],
            &SessionCaptureConsent::default(),
        )
        .unwrap()
        .repositories
        .remove("repo")
        .unwrap();
        assert!(snapshot.metadata.prerequisites.contains(&cutoff));

        // Destination is an unrelated empty repo that DOES have `origin` pointing to `r.origin`
        // (which contains `cutoff`). Restore must still refuse to fetch!
        let dest = r.temp.path().join("dest-missing-prereq");
        fs::create_dir_all(&dest).unwrap();
        git(&dest, &["init", "-b", "main"]);
        let origin_url = url::Url::from_directory_path(&r.origin)
            .unwrap()
            .to_string();
        git(&dest, &["remote", "add", "origin", &origin_url]);

        let err = restore_workspace_snapshot(&FileGit, &snapshot, &dest)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&cutoff) && err.contains("refusing to fetch later history"),
            "unexpected error: {err}"
        );
        let check = run_git(&FileGit, &dest, ["cat-file", "-e", &cutoff], &[]).unwrap();
        assert_ne!(check.status, 0, "restore must not fetch missing prerequisites");
    }

    #[test]
    fn history_cutoff_refuses_initialized_submodules_before_teardown() {
        let r = Repo::new();
        let sub_origin = r.temp.path().join("sub.git");
        let sub_src = r.temp.path().join("sub-src");
        git(r.temp.path(), &["init", "--bare", "-b", "main", sub_origin.to_str().unwrap()]);
        git(r.temp.path(), &["clone", sub_origin.to_str().unwrap(), sub_src.to_str().unwrap()]);
        fs::write(sub_src.join("s.txt"), "sub\n").unwrap();
        git(&sub_src, &["add", "s.txt"]);
        git(&sub_src, &["commit", "-m", "sub base"]);
        git(&sub_src, &["push", "origin", "main"]);

        let cutoff = git(&r.checkout, &["rev-parse", "HEAD"]);
        git(&r.checkout, &["remote", "remove", "origin"]);
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub_origin.to_str().unwrap(),
                "vendor/sub",
            ],
        );
        git(&r.checkout, &["commit", "-m", "add submodule"]);
        // Create a local unpushed commit inside the initialized submodule and stage it.
        let sub_path = r.checkout.join("vendor/sub");
        fs::write(sub_path.join("s.txt"), "local sub commit\n").unwrap();
        git(&sub_path, &["add", "s.txt"]);
        git(&sub_path, &["commit", "-m", "local submodule commit"]);
        git(&r.checkout, &["add", "vendor/sub"]);

        let input = RepositoryInput {
            id: "repo".to_owned(),
            checkout: r.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: r.temp.path().join("cutoff-sub.pack"),
            exclusions: ExclusionSource::Commits(vec![cutoff]),
            keep_dir: None,
        };
        let err = capture_session_snapshot(&FileGit, &[input], &SessionCaptureConsent::default())
            .expect_err("history-cutoff capture must refuse initialized submodules");
        assert!(
            matches!(
                err.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::DirtySubmodule(_))
            ),
            "expected DirtySubmodule refusal, got: {err:#}"
        );
    }

    #[test]
    fn consent_staleness_refuses_new_untracked_or_keep_dir_file_after_clean_preflight() {
        let r = Repo::new();
        let keep_dir = r.checkout.join(".mj/keep");
        let input = RepositoryInput {
            id: "workspace".to_owned(),
            checkout: r.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: r.temp.path().join("stale.pack"),
            exclusions: ExclusionSource::Origin,
            keep_dir: Some(keep_dir.clone()),
        };
        // 1. Preflight sees a clean workspace (no untracked files, no keep-dir files).
        let inv = session_inventory(&FileGit, std::slice::from_ref(&input)).unwrap();
        assert!(inv["workspace"].untracked.is_empty());
        assert!(scan_keep_dir(&inv["workspace"].checkout, &keep_dir).unwrap().0.is_empty());

        // 2. A file appears before the capture cut. Capture with UntrackedConsent::None refuses.
        fs::write(r.checkout.join("late-scratch.txt"), "created after preflight\n").unwrap();
        let err = capture_session_snapshot(&FileGit, std::slice::from_ref(&input), &SessionCaptureConsent::default())
            .expect_err("capture must refuse untracked file created after clean preflight");
        assert!(
            matches!(err.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(_))),
            "expected Untracked refusal, got: {err:#}"
        );

        // 3. Even if the user approved `late-scratch.txt` via Shown, modifying it or adding a
        //    keep-dir file after preflight refuses because the file identity / keep-dir changed.
        let inv2 = session_inventory(&FileGit, std::slice::from_ref(&input)).unwrap();
        let shown_item = UntrackedItem::Path {
            checkout: inv2["workspace"].checkout.clone(),
            path: inv2["workspace"].untracked[0].path.clone(),
            identity: inv2["workspace"].untracked[0].identity.clone(),
        };
        fs::write(r.checkout.join("late-scratch.txt"), "different content and size!\n").unwrap();
        let stale_consent = SessionCaptureConsent {
            repositories: [(
                "workspace".to_owned(),
                CaptureConsent {
                    untracked: UntrackedConsent::Shown(vec![shown_item]),
                    operations: OperationConsent::None,
                },
            )]
            .into_iter()
            .collect(),
        };
        let err = capture_session_snapshot(&FileGit, std::slice::from_ref(&input), &stale_consent)
            .expect_err("capture must refuse when approved file identity changed");
        assert!(
            matches!(err.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(_))),
            "expected Untracked refusal for changed identity, got: {err:#}"
        );

        // 4. A file created in `.mj/keep/` after preflight is owned solely by
        //    keep_dir scanning (never duplicated into `inventory.untracked`),
        //    and is refused directly by `capture_session_snapshot` as well as
        //    `check_keep_dir_consent`.
        fs::remove_file(r.checkout.join("late-scratch.txt")).unwrap();
        fs::create_dir_all(&keep_dir).unwrap();
        fs::write(keep_dir.join("note.txt"), "keep note\n").unwrap();
        let inv_with_keep = session_inventory(&FileGit, std::slice::from_ref(&input)).unwrap();
        assert!(
            inv_with_keep["workspace"].untracked.is_empty(),
            "keep_dir files must not be duplicated into inventory.untracked: {:?}",
            inv_with_keep["workspace"].untracked
        );
        let err = capture_session_snapshot(
            &FileGit,
            std::slice::from_ref(&input),
            &SessionCaptureConsent::default(),
        )
        .expect_err("capture_session_snapshot must refuse unapproved keep_dir file");
        assert!(
            matches!(err.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(_))),
            "expected Untracked refusal from capture_session_snapshot, got: {err:#}"
        );
        let err = check_keep_dir_consent(
            &inv2["workspace"].checkout,
            &keep_dir,
            &UntrackedConsent::None,
        )
        .expect_err("keep-dir check must refuse unapproved keep file");
        assert!(
            matches!(err.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(_))),
            "expected Untracked refusal for keep-dir file, got: {err:#}"
        );
    }
}
