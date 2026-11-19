//! Origin-relative Git snapshots, packing pinned objects without temporary refs.
//! Filesystem paths belong to the machine running the supplied Git runner.

use crate::archive::git::{
    GitCommand, GitCommandRunner, discover_linked_worktrees, git_bytes, git_failure, git_text,
    run_git, synthetic_git_commit_identity, synthetic_git_identity,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckoutKey {
    pub repository_id: String,
    pub checkout_path: PathBuf,
}
impl Default for CheckoutKey {
    fn default() -> Self {
        Self {
            repository_id: "workspace".into(),
            checkout_path: PathBuf::new(),
        }
    }
}
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SnapshotHead {
    Branch { name: String, commit: String },
    Detached(String),
}
impl SnapshotHead {
    pub fn commit(&self) -> &str {
        match self {
            Self::Branch { commit, .. } | Self::Detached(commit) => commit,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotStashEntry {
    pub commit: String,
    pub message: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotConfigEntry {
    pub name: String,
    pub value: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotSubmodule {
    pub origin_urls: Vec<String>,
    pub push_urls: Vec<String>,
    pub object_format: String,
    pub worktree_config_enabled: bool,
    pub url_rules: Vec<SnapshotConfigEntry>,
    pub worktree_url_rules: Vec<SnapshotConfigEntry>,
    pub path: PathBuf,
    pub commit: String,
    pub name: String,
    pub url: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotGitlink {
    pub path: PathBuf,
    pub commit: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotWorktree {
    pub push_evidence: PushEvidence,
    pub has_uncommitted_state: bool,
    pub url_rules: Vec<SnapshotConfigEntry>,
    pub relative_path: PathBuf,
    pub head: SnapshotHead,
    pub working_state: Option<String>,
    pub working_state_skipped: bool,
    pub initialized_submodules: Vec<SnapshotSubmodule>,
    pub index_gitlinks: Vec<SnapshotGitlink>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BranchTracking {
    pub remote: Option<String>,
    pub merge: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PushEvidence {
    Resolved(Vec<String>),
    Unavailable { reason: String },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub object_format: String,
    pub worktree_config_enabled: bool,
    pub origin_urls: Vec<String>,
    pub push_urls: Vec<String>,
    pub push_evidence: PushEvidence,
    pub url_rules: Vec<SnapshotConfigEntry>,
    pub worktree_url_rules: Vec<SnapshotConfigEntry>,
    pub notices: Vec<String>,
    pub head: SnapshotHead,
    pub refs: BTreeMap<String, String>,
    pub refs_complete: bool,
    pub deleted_refs: Vec<String>,
    pub branch_tracking: BTreeMap<String, BranchTracking>,
    pub stash: Vec<SnapshotStashEntry>,
    pub working_state: Option<String>,
    pub working_state_skipped: bool,
    pub has_uncommitted_state: bool,
    pub worktrees: Vec<SnapshotWorktree>,
    pub initialized_submodules: Vec<SnapshotSubmodule>,
    pub index_gitlinks: Vec<SnapshotGitlink>,
    pub remote_refs_stale: bool,
    /// Non-thin Git pack; `None` when origin already supplies every object.
    pub pack: Option<PathBuf>,
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct FileIdentity(String);
/// Use the same metadata observation for this identity and the displayed size.
/// The controller can use this for .mj/keep entries as well as untracked files.
pub fn file_identity(metadata: &fs::Metadata) -> Result<FileIdentity> {
    let mut identity = blake3::Hasher::new();
    hash_file_metadata(&mut identity, metadata)?;
    Ok(FileIdentity(identity.finalize().to_hex().to_string()))
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum UntrackedItem {
    Path {
        checkout: CheckoutKey,
        path: PathBuf,
        identity: FileIdentity,
    },
    NestedRepository {
        checkout: CheckoutKey,
        path: PathBuf,
        identity: FileIdentity,
    },
    Stash {
        checkout: CheckoutKey,
        commit: String,
        message: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UntrackedConsent {
    None,
    Any,
    Shown(Vec<UntrackedItem>),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OperationConsent {
    None,
    Any,
    Shown(Vec<ShownOperation>),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureConsent {
    pub untracked: UntrackedConsent,
    pub operations: OperationConsent,
}
#[derive(Debug)]
pub enum SnapshotRefused {
    Untracked(Vec<UntrackedItem>),
    OperationInProgress(ShownOperation),
    DirtySubmodule(Scoped<PathBuf>),
    HiddenTrackedEdits(Vec<Scoped<PathBuf>>),
    TrackedKeepFiles {
        keep_dir: PathBuf,
        paths: Vec<Scoped<PathBuf>>,
    },
}
impl std::fmt::Display for SnapshotRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TrackedKeepFiles { keep_dir, paths } => write!(
                f,
                "{} tracked keep files would enter the snapshot; run git rm -r --cached {} before suspending",
                paths.len(),
                keep_dir.display()
            ),
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
            Self::DirtySubmodule(path) => write!(
                f,
                "submodule {}:{}:{} has tracked changes or an unpublished checkout; commit and push inside the submodule, or discard the change; if uninitialized, initialize the submodule or unstage the pointer change",
                path.checkout.repository_id,
                path.checkout.checkout_path.display(),
                path.value.display()
            ),
            Self::OperationInProgress(operation) => {
                write!(
                    f,
                    "{:?} in progress at {}:{}; abort consent is required",
                    operation.kind,
                    operation.checkout.repository_id,
                    operation.checkout.checkout_path.display()
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
                    }
                }
                Ok(())
            }
        }
    }
}
impl std::error::Error for SnapshotRefused {}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum GitOperationKind {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Am,
    Bisect,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitOperation {
    pub checkout: CheckoutKey,
    pub kind: GitOperationKind,
    pub identity: String,
}
pub type ShownOperation = GitOperation;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UntrackedPath {
    pub checkout: CheckoutKey,
    pub path: PathBuf,
    pub bytes: u64,
    pub identity: FileIdentity,
}
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
    pub hidden_tracked_edits: Vec<Scoped<PathBuf>>,
    pub initialized_submodules: Vec<Scoped<SnapshotSubmodule>>,
    pub index_gitlinks: Vec<Scoped<SnapshotGitlink>>,
    pub operation: Option<GitOperation>,
    pub raw: bool,
    pub remote_refs_stale: bool,
    pub worktrees: Vec<(PathBuf, WorkspaceInventory)>,
}

fn git_path(runner: &dyn GitCommandRunner, checkout: &Path, name: &str) -> Result<PathBuf> {
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
pub fn record_launch(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    session_id: &str,
) -> Result<()> {
    let record = LaunchRecord {
        version: 1,
        refs: read_refs(runner, checkout)?,
        stash: read_stash(runner, checkout)?,
        worktrees: discover_linked_worktrees(runner, checkout)?
            .into_iter()
            .map(|w| w.relative)
            .collect(),
    };
    let path = launch_path(runner, checkout, session_id)?;
    let parent = path.parent().context("launch path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut bytes = serde_json::to_vec(&record)?;
    bytes.push(b'\n');
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut file, &bytes)?;
    file.as_file().sync_all()?;
    file.persist(&path)
        .with_context(|| format!("write {}", path.display()))?;
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
            "refs/remotes/origin/",
        ],
    )?;
    let mut refs = BTreeMap::new();
    for line in text.lines() {
        let mut fields = line.split('\0');
        let name = fields.next().context("missing Git ref name")?;
        let oid = fields.next().context("missing Git ref object")?;
        let symbolic = fields.next().context("missing Git symbolic ref field")?;
        if symbolic.is_empty()
            && (local_snapshot_ref(name) || name.starts_with("refs/remotes/origin/"))
        {
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
    let bytes = git_bytes(
        runner,
        checkout,
        ["stash", "list", "--format=%H%x00%gs"],
        &[],
        "read stash list",
    )?;
    std::str::from_utf8(&bytes)?
        .lines()
        .map(|line| {
            let (commit, message) = line.split_once('\0').context("malformed stash entry")?;
            Ok(SnapshotStashEntry {
                commit: commit.to_owned(),
                message: message.to_owned(),
            })
        })
        .collect()
}
fn read_head(runner: &dyn GitCommandRunner, checkout: &Path) -> Result<SnapshotHead> {
    let symbolic = run_git(runner, checkout, ["symbolic-ref", "-q", "HEAD"], &[])?;
    match symbolic.status {
        0 => {
            let name = String::from_utf8(symbolic.stdout)?.trim().to_owned();
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
fn operation(runner: &dyn GitCommandRunner, checkout: &Path) -> Result<Option<GitOperationKind>> {
    for (name, kind) in [
        ("rebase-merge", GitOperationKind::Rebase),
        ("rebase-apply", GitOperationKind::Rebase),
        ("MERGE_HEAD", GitOperationKind::Merge),
        ("CHERRY_PICK_HEAD", GitOperationKind::CherryPick),
        ("REVERT_HEAD", GitOperationKind::Revert),
        ("sequencer", GitOperationKind::CherryPick),
        ("BISECT_LOG", GitOperationKind::Bisect),
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
    let Some(kind) = operation(runner, checkout)? else {
        return Ok(None);
    };
    let paths: &[&str] = match kind {
        GitOperationKind::Merge => &["MERGE_HEAD", "MERGE_MSG", "MERGE_MODE", "ORIG_HEAD"],
        GitOperationKind::Rebase => &[
            "rebase-merge/head-name",
            "rebase-merge/onto",
            "rebase-merge/orig-head",
            "rebase-apply/head-name",
            "rebase-apply/onto",
            "rebase-apply/orig-head",
        ],
        GitOperationKind::CherryPick => &["CHERRY_PICK_HEAD", "sequencer/head", "sequencer/todo"],
        GitOperationKind::Revert => &["REVERT_HEAD", "sequencer/head", "sequencer/todo"],
        GitOperationKind::Am => &[
            "rebase-apply/head-name",
            "rebase-apply/onto",
            "rebase-apply/orig-head",
            "rebase-apply/applying",
            "rebase-apply/next",
        ],
        GitOperationKind::Bisect => &["BISECT_START", "BISECT_LOG"],
    };
    let mut identity = blake3::Hasher::new();
    identity.update(format!("{kind:?}").as_bytes());
    identity.update(&serde_json::to_vec(&read_head(runner, checkout)?)?);
    for name in paths {
        identity.update(name.as_bytes());
        let path = git_path(runner, checkout, name)?;
        match fs::File::open(&path) {
            Ok(mut file) => {
                let metadata = file.metadata()?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                identity.update(&[1]);
                let bytes = if *name == "BISECT_LOG" {
                    bytes.split(|byte| *byte == b'\n').next().unwrap_or(&[])
                } else {
                    &bytes
                };
                identity.update(&(bytes.len() as u64).to_be_bytes());
                identity.update(bytes);
                // Recreating an operation with identical inputs is a new consent
                // decision too. Git rewrites these state files for the new run.
                hash_file_metadata(&mut identity, &metadata)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                identity.update(&[0]);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read operation state {}", path.display()));
            }
        }
    }
    Ok(Some(GitOperation {
        checkout: key.clone(),
        kind,
        identity: identity.finalize().to_hex().to_string(),
    }))
}
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
        let args = match kind {
            GitOperationKind::Merge => ["merge", "--abort"],
            GitOperationKind::Rebase => ["rebase", "--abort"],
            GitOperationKind::CherryPick => ["cherry-pick", "--abort"],
            GitOperationKind::Revert => ["revert", "--abort"],
            GitOperationKind::Am => ["am", "--abort"],
            GitOperationKind::Bisect => ["bisect", "reset"],
        };
        git_text(runner, checkout, args)?;
    }
    Ok(false)
}
fn submodule_positions_differ(inventory: &WorkspaceInventory) -> bool {
    inventory.initialized_submodules.iter().any(|module| {
        !inventory.index_gitlinks.iter().any(|link| {
            link.value.path == module.value.path && link.value.commit == module.value.commit
        })
    })
}
fn staged_gitlink_changes(runner: &dyn GitCommandRunner, checkout: &Path) -> Result<bool> {
    let raw = git_bytes(
        runner,
        checkout,
        [
            "diff",
            "--cached",
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
fn working_state(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    skipped: bool,
) -> Result<Option<String>> {
    if skipped {
        return Ok(None);
    }
    // Editing files needs no user identity; synthetic stash commits use ours.
    let output = runner.run(
        checkout,
        &GitCommand {
            arguments: vec!["stash".into(), "create".into()],
            env: synthetic_git_commit_identity(),
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
fn saved_head(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    skipped: bool,
) -> Result<SnapshotHead> {
    if skipped {
        match operation(runner, checkout)? {
            Some(GitOperationKind::Rebase) => {
                for dir in ["rebase-merge", "rebase-apply"] {
                    let path = git_path(runner, checkout, dir)?.join("head-name");
                    if path.try_exists()? {
                        let name = fs::read_to_string(path)?.trim().to_owned();
                        if name.starts_with("refs/heads/") {
                            let commit = git_text(runner, checkout, ["rev-parse", &name])?;
                            return Ok(SnapshotHead::Branch { name, commit });
                        }
                        let oid =
                            fs::read_to_string(git_path(runner, checkout, dir)?.join("orig-head"))?;
                        return Ok(SnapshotHead::Detached(oid.trim().to_owned()));
                    }
                }
            }
            Some(GitOperationKind::CherryPick | GitOperationKind::Revert) => {
                let path = git_path(runner, checkout, "sequencer/head")?;
                if path.try_exists()? {
                    let commit = fs::read_to_string(path)?.trim().to_owned();
                    return Ok(match read_head(runner, checkout)? {
                        SnapshotHead::Branch { name, .. } => SnapshotHead::Branch { name, commit },
                        SnapshotHead::Detached(_) => SnapshotHead::Detached(commit),
                    });
                }
            }
            Some(GitOperationKind::Bisect) => {
                let start = fs::read_to_string(git_path(runner, checkout, "BISECT_START")?)?;
                let start = start.trim();
                let branch = format!("refs/heads/{start}");
                let result = run_git(
                    runner,
                    checkout,
                    ["show-ref", "--verify", "--quiet", &branch],
                    &[],
                )?;
                return if result.status == 0 {
                    Ok(SnapshotHead::Branch {
                        commit: git_text(runner, checkout, ["rev-parse", &branch])?,
                        name: branch,
                    })
                } else {
                    Ok(SnapshotHead::Detached(git_text(
                        runner,
                        checkout,
                        ["rev-parse", start],
                    )?))
                };
            }
            _ => {}
        }
    }
    read_head(runner, checkout)
}
fn relative_local_remote(remote: &str) -> bool {
    // A colon before the first slash denotes Git's scp or transport syntax.
    // Native drive letters are handled first by Path::is_absolute.
    !Path::new(remote).is_absolute()
        && !remote
            .find(':')
            .is_some_and(|colon| remote.find('/').is_none_or(|slash| colon < slash))
}
/// Strip credentials even from incomplete rewrite prefixes, which URL parsers
/// reject (for example https://user:token@ without a host).
fn redact_snapshot_url(value: &str) -> Result<String> {
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
fn capture_push_evidence(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    has_origin: bool,
    in_container: bool,
) -> Result<PushEvidence> {
    let unavailable = |reason: &str| PushEvidence::Unavailable {
        reason: reason.to_owned(),
    };
    if !has_origin {
        return Ok(unavailable("repository has no named origin"));
    }
    let output = run_git(
        runner,
        checkout,
        ["remote", "get-url", "--push", "--all", "origin"],
        &[],
    )?;
    if output.status != 0 {
        return Ok(unavailable(
            "Git could not resolve origin's push destinations",
        ));
    }
    let Ok(text) = std::str::from_utf8(&output.stdout) else {
        return Ok(unavailable("push destinations could not be safely decoded"));
    };
    let mut destinations = Vec::new();
    for value in text.lines() {
        if value.is_empty() || (value.contains("://") && url::Url::parse(value).is_err()) {
            return Ok(unavailable(
                "a push destination could not be safely redacted",
            ));
        }
        if !value.contains("://")
            && !relative_local_remote(value)
            && !Path::new(value).is_absolute()
        {
            let Some((host, _)) = value.split_once(':') else {
                return Ok(unavailable(
                    "an opaque push destination could not be safely redacted",
                ));
            };
            if host.is_empty() || value.find('@').is_some_and(|at| value[..at].contains(':')) {
                return Ok(unavailable(
                    "an ambiguous push destination could not be safely redacted",
                ));
            }
        }
        let stored = stored_remote(runner, checkout, value)?;
        if in_container
            && (Path::new(&stored).is_absolute()
                || url::Url::parse(&stored).is_ok_and(|url| url.scheme() == "file"))
        {
            return Ok(unavailable("container-local destination"));
        }
        destinations.push(stored);
    }
    if destinations.is_empty() {
        Ok(unavailable("origin has no push destinations"))
    } else {
        Ok(PushEvidence::Resolved(destinations))
    }
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
    let absolute = std::path::absolute(checkout.join(value))?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    let mut stored = normalized
        .to_str()
        .context("Git config path is not UTF-8")?
        .to_owned();
    if value.ends_with('/') && !stored.ends_with('/') {
        stored.push('/');
    }
    Ok(stored)
}
pub(crate) fn stored_remote(
    _runner: &dyn GitCommandRunner,
    checkout: &Path,
    configured: &str,
) -> Result<String> {
    redact_snapshot_url(&absolute_config_url(checkout, configured)?)
}
pub(crate) struct CapturedUrlRules {
    pub rules: Vec<SnapshotConfigEntry>,
    pub notices: Vec<String>,
}
fn url_rule_parts(name: &str) -> Result<(&str, &str)> {
    name.strip_prefix("url.")
        .and_then(|key| key.rsplit_once('.'))
        .context("malformed local URL rule key")
}
fn url_has_userinfo(value: &str) -> bool {
    if let Some(scheme) = value.find("://") {
        let authority = scheme + 3;
        let end = value[authority..]
            .find(['/', '?', '#'])
            .map_or(value.len(), |offset| authority + offset);
        if let Some(at) = value[authority..end].rfind('@') {
            let userinfo = &value[authority..authority + at];
            return value[..scheme].eq_ignore_ascii_case("http")
                || value[..scheme].eq_ignore_ascii_case("https")
                || userinfo.contains(':');
        }
        false
    } else if !Path::new(value).is_absolute()
        && let Some(at) = value.find('@')
        && value[at + 1..].contains(':')
    {
        value[..at].contains(':')
    } else {
        false
    }
}
fn read_config_entries(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    worktree: bool,
    pattern: &str,
) -> Result<Vec<SnapshotConfigEntry>> {
    let output = run_git(
        runner,
        checkout,
        [
            "config",
            if worktree { "--worktree" } else { "--local" },
            "--includes",
            "--null",
            "--get-regexp",
            pattern,
        ],
        &[],
    )?;
    ensure!(
        output.status == 0 || output.status == 1,
        "{}",
        git_failure("read local URL rules", &output)
    );
    let mut rules = Vec::new();
    for entry in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let split = entry
            .iter()
            .position(|byte| *byte == b'\n')
            .context("malformed local URL rule")?;
        let name = std::str::from_utf8(&entry[..split])?.to_owned();
        rules.push(SnapshotConfigEntry {
            name,
            value: std::str::from_utf8(&entry[split + 1..])?.to_owned(),
        });
    }
    Ok(rules)
}
fn absolute_url_rule(checkout: &Path, rule: &SnapshotConfigEntry) -> Result<SnapshotConfigEntry> {
    let (base, kind) = url_rule_parts(&rule.name)?;
    Ok(SnapshotConfigEntry {
        name: format!("url.{}.{kind}", absolute_config_url(checkout, base)?),
        value: absolute_config_url(checkout, &rule.value)?,
    })
}
fn capture_url_rules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    worktree: bool,
) -> Result<CapturedUrlRules> {
    let mut captured = CapturedUrlRules {
        rules: Vec::new(),
        notices: Vec::new(),
    };
    for rule in read_config_entries(runner, checkout, worktree, r"^url\.")? {
        let (base, kind) = url_rule_parts(&rule.name)?;
        if !matches!(kind, "insteadof" | "pushinsteadof") {
            continue;
        }
        if url_has_userinfo(base) || url_has_userinfo(&rule.value) {
            captured.notices.push(format!(
                "omitted credential-bearing URL rule url.{}.{kind}",
                redact_snapshot_url(base)?
            ));
        } else {
            captured.rules.push(absolute_url_rule(checkout, &rule)?);
        }
    }
    Ok(captured)
}
pub(crate) fn local_url_rules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<CapturedUrlRules> {
    capture_url_rules(runner, checkout, false)
}
pub(crate) fn worktree_config_enabled(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<bool> {
    let enabled = run_git(
        runner,
        checkout,
        ["config", "--bool", "--get", "extensions.worktreeConfig"],
        &[],
    )?;
    ensure!(
        enabled.status == 0 || enabled.status == 1,
        "{}",
        git_failure("read worktree config mode", &enabled)
    );
    Ok(enabled.status == 0 && enabled.stdout == b"true\n")
}
pub(crate) fn worktree_url_rules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<CapturedUrlRules> {
    if worktree_config_enabled(runner, checkout)? {
        capture_url_rules(runner, checkout, true)
    } else {
        Ok(CapturedUrlRules {
            rules: Vec::new(),
            notices: Vec::new(),
        })
    }
}
fn restore_url_rules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    rules: &[SnapshotConfigEntry],
) -> Result<()> {
    for rule in rules {
        git_text(
            runner,
            checkout,
            ["config", "--local", "--add", &rule.name, &rule.value],
        )?;
    }
    Ok(())
}
/// Resolve a .gitmodules URL using Git's documented default-remote rule.
/// Local submodule.<name>.url values are already resolved and use snapshot_remote.
pub(crate) fn resolve_submodule_url(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    url: &str,
) -> Result<String> {
    if !url.starts_with("./") && !url.starts_with("../") {
        return Ok(url.to_owned());
    }
    let branch = run_git(
        runner,
        checkout,
        ["symbolic-ref", "--quiet", "--short", "HEAD"],
        &[],
    )?;
    let remote = if branch.status == 0 {
        let branch = String::from_utf8(branch.stdout)?;
        let config = config_values(
            runner,
            checkout,
            &format!("branch.{}.remote", branch.trim()),
        )?;
        config.first().cloned().unwrap_or_else(|| "origin".into())
    } else {
        "origin".into()
    };
    let base = config_values(runner, checkout, &format!("remote.{remote}.url"))?;
    if remote == "." || base.is_empty() {
        return resolve_remote_path(checkout, url);
    }
    relative_submodule_url(&base[0], url, checkout)
}
/// Git treats the superproject URL as a directory when resolving ../sub.git.
fn relative_submodule_url(base: &str, relative: &str, checkout: &Path) -> Result<String> {
    if let Ok(mut base) = url::Url::parse(base)
        && !Path::new(base.as_str()).is_absolute()
    {
        let path = format!("{}/", base.path().trim_end_matches('/'));
        base.set_path(&path);
        return Ok(base.join(relative)?.to_string());
    }
    if !Path::new(base).is_absolute()
        && let Some((host, remote_path)) = base.split_once(':')
    {
        let absolute = remote_path.starts_with('/');
        let mut parts: Vec<_> = remote_path
            .split('/')
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect();
        for part in relative.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    ensure!(
                        parts.pop().is_some(),
                        "relative submodule URL escapes its remote"
                    );
                }
                part => parts.push(part.to_owned()),
            }
        }
        return Ok(format!(
            "{host}:{}{}",
            if absolute { "/" } else { "" },
            parts.join("/")
        ));
    }
    let base = resolve_remote_path(checkout, base)?;
    resolve_remote_path(checkout, &Path::new(&base).join(relative).to_string_lossy())
}

fn append_pack_extension(base: &Path, hash: &str, extension: &str) -> PathBuf {
    let mut path = base.as_os_str().to_owned();
    path.push(format!("-{hash}.{extension}"));
    path.into()
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
    ownership: &CheckoutOwnership,
) -> Result<PinnedWorkspaceCut> {
    let nested = discover_linked_worktrees(runner, checkout)?;
    let refs = read_all_refs(runner, checkout)?;
    let read = |path: &Path| -> Result<SnapshotHead> {
        let raw_operation = matches!(ownership, CheckoutOwnership::Raw { .. })
            && operation(runner, path)?.is_some();
        let mut head = saved_head(runner, path, raw_operation)?;
        if !raw_operation && let SnapshotHead::Branch { name, commit } = &mut head {
            *commit = refs
                .get(name)
                .with_context(|| format!("HEAD branch {name} moved during snapshot capture"))?
                .clone();
        }
        Ok(head)
    };
    let head = read(checkout)?;
    let mut worktrees = Vec::new();
    for tree in nested {
        worktrees.push(PinnedWorktree {
            relative_path: tree.relative,
            head: read(&tree.directory)?,
        });
    }
    Ok(PinnedWorkspaceCut {
        refs,
        head,
        worktrees,
    })
}
fn prepare_capture(
    runner: &dyn GitCommandRunner,
    repository: &RepositoryInput,
    consent: &CaptureConsent,
) -> Result<WorkspaceInventory> {
    let checkout = repository.checkout.as_path();
    let ownership = &repository.ownership;
    let inventory = repository_inventory(runner, repository)?;
    check_repository_consent(repository, &inventory, consent)?;
    prepare_operation(
        runner,
        checkout,
        ownership,
        inventory.operation.as_ref().map(|operation| operation.kind),
    )?;
    for (path, tree) in &inventory.worktrees {
        prepare_operation(
            runner,
            &checkout.join(path),
            ownership,
            tree.operation.as_ref().map(|operation| operation.kind),
        )?;
    }
    if !inventory.raw
        && (inventory.operation.is_some()
            || inventory
                .worktrees
                .iter()
                .any(|(_, tree)| tree.operation.is_some()))
    {
        // Abort may restore different HEAD/index gitlinks. Validate that exact
        // state before pinning it, retaining the pre-abort refusal above.
        let inventory = repository_inventory(runner, repository)?;
        check_repository_consent(repository, &inventory, consent)?;
        return Ok(inventory);
    }
    Ok(inventory)
}
fn check_consent(inventory: &WorkspaceInventory, consent: &CaptureConsent) -> Result<()> {
    let mut untracked = Vec::new();
    let mut operations = Vec::new();
    let mut hidden = Vec::new();
    let mut dirty_submodules = Vec::new();
    let mut tracked_keep = Vec::new();
    let mut collect =
        |inventory: &WorkspaceInventory| {
            untracked.extend(inventory.untracked.iter().map(|entry| UntrackedItem::Path {
                checkout: entry.checkout.clone(),
                path: entry.path.clone(),
                identity: entry.identity.clone(),
            }));
            untracked.extend(inventory.nested_repositories.iter().map(|entry| {
                UntrackedItem::NestedRepository {
                    checkout: entry.checkout.clone(),
                    path: entry.path.clone(),
                    identity: entry.identity.clone(),
                }
            }));
            tracked_keep.extend(inventory.tracked_keep_files.clone());
            untracked.extend(inventory.untracked_stashes.iter().map(|entry| {
                UntrackedItem::Stash {
                    checkout: entry.checkout.clone(),
                    commit: entry.value.commit.clone(),
                    message: entry.value.message.clone(),
                }
            }));
            hidden.extend(inventory.hidden_tracked_edits.clone());
            dirty_submodules.extend(inventory.dirty_submodules.clone());
            if let Some(operation) = &inventory.operation {
                operations.push(operation.clone());
            }
        };
    collect(inventory);
    for (_, inventory) in &inventory.worktrees {
        collect(inventory);
    }
    if !tracked_keep.is_empty() {
        return Err(SnapshotRefused::TrackedKeepFiles {
            keep_dir: inventory
                .keep_dir
                .clone()
                .context("tracked keep inventory has no keep directory")?,
            paths: tracked_keep,
        }
        .into());
    }
    ensure!(
        hidden.is_empty(),
        SnapshotRefused::HiddenTrackedEdits(hidden)
    );
    if let Some(path) = dirty_submodules.into_iter().next() {
        return Err(SnapshotRefused::DirtySubmodule(path).into());
    }
    untracked.retain(|entry| !untracked_covered(&consent.untracked, entry));
    ensure!(untracked.is_empty(), SnapshotRefused::Untracked(untracked));
    if !inventory.raw {
        for operation in operations {
            let covered = match &consent.operations {
                OperationConsent::None => false,
                OperationConsent::Any => true,
                OperationConsent::Shown(shown) => shown.contains(&operation),
            };
            ensure!(covered, SnapshotRefused::OperationInProgress(operation));
        }
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
pub(crate) struct OriginTracking {
    pub objects: Vec<String>,
    pub ambiguous: bool,
}
pub(crate) fn origin_tracking_objects(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<OriginTracking> {
    let specs = config_values(runner, checkout, "remote.origin.fetch")?;
    let negatives = specs
        .iter()
        .filter_map(|spec| spec.strip_prefix('^'))
        .collect::<Vec<_>>();
    let mappings = specs
        .iter()
        .filter(|spec| !spec.starts_with('^'))
        .filter_map(|spec| spec.trim_start_matches('+').split_once(':'))
        .collect::<Vec<_>>();
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
    let maps = |specs: &[String], name: &str| -> bool {
        specs
            .iter()
            .filter(|spec| !spec.starts_with('^'))
            .filter_map(|spec| spec.trim_start_matches('+').split_once(':'))
            .any(|(source, destination)| {
                refspec_suffix(destination, name).is_some_and(|suffix| {
                    let source = source.replacen('*', &suffix, 1);
                    !specs
                        .iter()
                        .filter_map(|spec| spec.strip_prefix('^'))
                        .any(|negative| refspec_suffix(negative, &source).is_some())
                })
            })
    };
    let mut ambiguous = false;
    let listed = git_text(
        runner,
        checkout,
        [
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(symref)",
        ],
    )?;
    let mut objects = Vec::new();
    for line in listed.lines() {
        let mut fields = line.split('\0');
        let name = fields.next().context("missing tracking ref name")?;
        let oid = fields.next().context("missing tracking ref object")?;
        if !fields
            .next()
            .context("missing tracking symref field")?
            .is_empty()
        {
            continue;
        }
        if mappings.iter().any(|(source, destination)| {
            refspec_suffix(destination, name).is_some_and(|suffix| {
                let source = source.replacen('*', &suffix, 1);
                !negatives
                    .iter()
                    .any(|negative| refspec_suffix(negative, &source).is_some())
            })
        }) {
            if other_mappings.iter().any(|other| maps(other, name)) {
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
/// Capture and dry-run share origin queries and objects-only downloads.
fn origin_exclusions(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    remote: &str,
    _refs: &BTreeMap<String, String>,
) -> Result<OriginExclusions> {
    let fallback = origin_tracking_objects(runner, checkout)?;
    let mut result = remote_objects(runner, checkout, remote, &fallback.objects)?;
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
) -> Result<OriginExclusions> {
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
                stale |= !output.is_ok_and(|output| output.status == 0);
            }
            let unavailable = missing_tips(runner, checkout, &tips)?
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>();
            if stale {
                objects.extend_from_slice(fallback);
            } else {
                objects.extend(tips.into_iter().filter(|tip| !unavailable.contains(tip)));
            }
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
fn snapshot_exclusions(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    remote: &str,
    refs: &BTreeMap<String, String>,
    source: &ExclusionSource,
) -> Result<OriginExclusions> {
    match source {
        ExclusionSource::Origin => origin_exclusions(runner, checkout, remote, refs),
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
) -> Result<BTreeMap<String, BranchTracking>> {
    let mut tracking = BTreeMap::new();
    for reference in refs.keys() {
        let Some(name) = reference.strip_prefix("refs/heads/") else {
            continue;
        };
        let remote = config_values(runner, checkout, &format!("branch.{name}.remote"))?
            .into_iter()
            .last();
        let merge = config_values(runner, checkout, &format!("branch.{name}.merge"))?;
        if remote.is_some() || !merge.is_empty() {
            tracking.insert(name.to_owned(), BranchTracking { remote, merge });
        }
    }
    Ok(tracking)
}
fn unset_config(runner: &dyn GitCommandRunner, checkout: &Path, key: &str) -> Result<()> {
    let output = run_git(runner, checkout, ["config", "--unset-all", key], &[])?;
    ensure!(
        matches!(output.status, 0 | 5),
        "{}",
        git_failure("clear cloned tracking configuration", &output)
    );
    Ok(())
}
struct SelectedRefs {
    refs: BTreeMap<String, String>,
    complete: bool,
}
fn selected_refs(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    cut: &PinnedWorkspaceCut,
    ownership: &CheckoutOwnership,
) -> Result<SelectedRefs> {
    let legacy = matches!(ownership, CheckoutOwnership::Managed)
        && !owns_git_directory(runner, checkout, ownership)?;
    let launch = match ownership {
        CheckoutOwnership::Raw { launch } => launch.as_ref(),
        CheckoutOwnership::Managed => None,
    };
    let mut scopes = Vec::new();
    if legacy {
        scopes.push(crate::archive::session_start_scope(runner, checkout)?);
        for tree in &cut.worktrees {
            scopes.push(crate::archive::session_start_scope(
                runner,
                &checkout.join(&tree.relative_path),
            )?);
        }
    }
    let complete = !legacy && launch.is_none();
    let mut refs: BTreeMap<_, _> = cut
        .refs
        .iter()
        .filter(|(name, oid)| {
            if !local_snapshot_ref(name) {
                return false;
            }
            if legacy {
                return scopes
                    .iter()
                    .any(|scope| crate::archive::git::worktree_durable_ref(name, scope));
            }
            launch.is_none_or(|launch| launch.refs.get(*name) != Some(*oid))
        })
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect();
    for head in std::iter::once(&cut.head).chain(cut.worktrees.iter().map(|tree| &tree.head)) {
        if let SnapshotHead::Branch { name, commit } = head {
            refs.insert(name.clone(), commit.clone());
        }
    }
    Ok(SelectedRefs { refs, complete })
}
fn write_snapshot_pack(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    tips: &[String],
    exclusions: &[String],
    base: &Path,
) -> Result<Option<PathBuf>> {
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
    if let Some(parent) = base.parent() {
        fs::create_dir_all(parent)?;
    }
    let output = runner.run(
        checkout,
        &GitCommand {
            arguments: vec![
                "pack-objects".into(),
                "--revs".into(),
                base.as_os_str().to_owned(),
            ],
            stdin: revisions.into_bytes(),
            ..Default::default()
        },
    )?;
    ensure!(
        output.status == 0,
        "{}",
        git_failure("pack snapshot objects", &output)
    );
    let hash = String::from_utf8(output.stdout)?.trim().to_owned();
    ensure!(
        (hash.len() == 40 || hash.len() == 64) && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Git returned an invalid pack hash"
    );
    let path = append_pack_extension(&base, &hash, "pack");
    let mut header = [0_u8; 12];
    fs::File::open(&path)?.read_exact(&mut header)?;
    ensure!(
        header.starts_with(b"PACK"),
        "Git returned an invalid snapshot pack"
    );
    fs::remove_file(append_pack_extension(&base, &hash, "idx"))?;
    if header[8..12] == [0, 0, 0, 0] {
        fs::remove_file(path)?;
        Ok(None)
    } else {
        Ok(Some(path))
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
/// Estimate Git pack bytes without changing refs or aborting. Object-only fetches
/// and synthetic commits may cache objects; refs, FETCH_HEAD and working files stay put.
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
    let inventory = workspace_inventory(runner, checkout, ownership)?;
    let cut = pin_workspace_cut(runner, checkout, ownership)?;
    let exclusions = snapshot_exclusions(runner, checkout, &remote, &cut.refs, source)?;
    let refs = selected_refs(runner, checkout, &cut, ownership)?.refs;
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
    Commits(Vec<String>),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryInput {
    pub in_container: bool,
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
fn check_repository_consent(
    _repository: &RepositoryInput,
    inventory: &WorkspaceInventory,
    consent: &CaptureConsent,
) -> Result<()> {
    check_consent(inventory, consent)
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
    let none = CaptureConsent {
        untracked: UntrackedConsent::None,
        operations: OperationConsent::None,
    };
    let mut inventories = BTreeMap::new();
    for repository in repositories {
        ensure!(
            !inventories.contains_key(&repository.id),
            "duplicate snapshot repository ID {}",
            repository.id
        );
        let inventory = repository_inventory(runner, repository)
            .map_err(|error| session_error(&repository.id, error))?;
        inventories.insert(repository.id.clone(), inventory);
    }
    for repository in repositories {
        check_repository_consent(
            repository,
            &inventories[&repository.id],
            consent.repositories.get(&repository.id).unwrap_or(&none),
        )
        .map_err(|error| session_error(&repository.id, error))?;
    }
    let mut snapshots = BTreeMap::new();
    for repository in repositories {
        let snapshot = capture_prepared_workspace(
            runner,
            repository,
            consent.repositories.get(&repository.id).unwrap_or(&none),
            &repository_key(&repository.id),
        )
        .map_err(|error| session_error(&repository.id, error))?;
        snapshots.insert(repository.id.clone(), snapshot);
    }
    for repository in repositories {
        let inventory = repository_inventory(runner, repository)
            .map_err(|error| session_error(&repository.id, error))?;
        check_repository_consent(
            repository,
            &inventory,
            consent.repositories.get(&repository.id).unwrap_or(&none),
        )
        .map_err(|error| session_error(&repository.id, error))?;
    }
    Ok(SessionSnapshot {
        repositories: snapshots,
    })
}
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
        in_container: false,
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

/// Inventory, consent and all aborts precede the pinned cut. Concurrent ref or
/// HEAD changes retry the complete cut; capture never creates temporary refs.
/// Raw callers load their session's launch record with [`LaunchRecord::read`].
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
fn capture_prepared_workspace(
    runner: &dyn GitCommandRunner,
    repository: &RepositoryInput,
    consent: &CaptureConsent,
    key: &CheckoutKey,
) -> Result<WorkspaceSnapshot> {
    let checkout = repository.checkout.as_path();
    let ownership = &repository.ownership;
    let pack_path = repository.pack_path.as_path();
    let origin_override = repository.origin_override.clone();
    let source = &repository.exclusions;
    let inventory = prepare_capture(runner, repository, consent)?;
    let object_format = git_text(runner, checkout, ["rev-parse", "--show-object-format"])?;
    let worktree_config_enabled = crate::snapshot::worktree_config_enabled(runner, checkout)?;
    let skipped = inventory.raw && inventory.operation.is_some();
    let configured = run_git(runner, checkout, ["remote", "get-url", "origin"], &[])?;
    let has_origin = configured.status == 0;
    let originals = if has_origin {
        config_values(runner, checkout, "remote.origin.url")?
    } else {
        origin_override.into_iter().collect()
    };
    let origin_urls = originals
        .iter()
        .map(|url| stored_remote(runner, checkout, url))
        .collect::<Result<Vec<_>>>()?;
    let captured_rules = local_url_rules(runner, checkout)?;
    let push_evidence =
        capture_push_evidence(runner, checkout, has_origin, repository.in_container)?;
    let url_rules = captured_rules.rules;
    let captured_worktree_rules = worktree_url_rules(runner, checkout)?;
    let worktree_url_rules = captured_worktree_rules.rules;
    let mut notices = captured_rules.notices;
    notices.extend(captured_worktree_rules.notices);
    notices.extend(inventory.notices.iter().cloned());
    for (path, nested) in &inventory.worktrees {
        notices.extend(
            nested
                .notices
                .iter()
                .map(|notice| format!("{}: {notice}", path.display())),
        );
    }
    ensure!(
        !origin_urls.is_empty() || matches!(ownership, CheckoutOwnership::Raw { .. }),
        "managed snapshot requires an origin remote or source override"
    );
    if origin_urls.is_empty() {
        return Ok(WorkspaceSnapshot {
            object_format,
            worktree_config_enabled,
            origin_urls,
            push_urls: Vec::new(),
            push_evidence,
            url_rules,
            worktree_url_rules,

            notices,
            head: saved_head(runner, checkout, skipped)?,
            refs: BTreeMap::new(),
            refs_complete: false,
            deleted_refs: Vec::new(),
            branch_tracking: BTreeMap::new(),
            stash: Vec::new(),
            working_state: None,
            working_state_skipped: skipped,
            has_uncommitted_state: skipped
                || staged_gitlink_changes(runner, checkout)?
                || submodule_positions_differ(&inventory),
            worktrees: Vec::new(),
            initialized_submodules: Vec::new(),
            index_gitlinks: Vec::new(),
            remote_refs_stale: false,
            pack: None,
        });
    }
    let remote = if has_origin {
        "origin"
    } else {
        originals.first().context("missing snapshot origin")?
    };
    let push_urls = if has_origin {
        config_values(runner, checkout, "remote.origin.pushurl")?
            .iter()
            .map(|url| stored_remote(runner, checkout, url))
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let exclusions = snapshot_exclusions(runner, checkout, remote, &BTreeMap::new(), source)?;
    if let Some(notice) = &exclusions.notice {
        notices.push(notice.clone());
    }
    // Barrier-free exports may race a ref update. Three cuts give a moving
    // checkout a chance to settle, then report instability to its caller.
    for attempt in 0..3 {
        let inventory = prepare_capture(runner, repository, consent)?;
        let skipped = inventory.raw && inventory.operation.is_some();
        let cut = pin_workspace_cut(runner, checkout, ownership)?;
        let pinned = &cut.refs;
        let head = &cut.head;
        let original_stash = selected_stash(runner, checkout, ownership)?;
        // This exact list becomes pack tips; a newer stash cannot inherit a
        // previous inventory's consent just because it stays stable afterward.
        check_stash_consent(runner, checkout, &original_stash, &consent.untracked, key)?;
        let working = working_state(runner, checkout, skipped)?;
        let mut worktrees = Vec::new();
        let mut tips = vec![cut.head.commit().to_owned()];
        tips.extend(working.clone());
        let mut stable = true;
        let mut verify_state = |path: &Path, state: &Option<String>, oid: &str| -> Result<()> {
            if let Some(state) = state {
                stable &= git_text(runner, path, ["rev-parse", &format!("{state}^1")])? == oid;
            }
            Ok(())
        };
        verify_state(checkout, &working, cut.head.commit())?;
        for tree in &cut.worktrees {
            let directory = checkout.join(&tree.relative_path);
            let skipped = inventory.raw
                && inventory
                    .worktrees
                    .iter()
                    .find(|(path, _)| path == &tree.relative_path)
                    .is_some_and(|(_, tree)| tree.operation.is_some());
            let state = working_state(runner, &directory, skipped)?;
            verify_state(&directory, &state, tree.head.commit())?;
            tips.push(tree.head.commit().to_owned());
            tips.extend(state.clone());
            let own_rules = crate::snapshot::worktree_url_rules(runner, &directory)?;
            notices.extend(
                own_rules
                    .notices
                    .into_iter()
                    .map(|notice| format!("{}: {notice}", tree.relative_path.display())),
            );
            let has_origin =
                run_git(runner, &directory, ["remote", "get-url", "origin"], &[])?.status == 0;
            let has_uncommitted_state = state.is_some()
                || skipped
                || staged_gitlink_changes(runner, &directory)?
                || inventory
                    .worktrees
                    .iter()
                    .find(|(path, _)| path == &tree.relative_path)
                    .is_some_and(|(_, inventory)| submodule_positions_differ(inventory));
            worktrees.push(SnapshotWorktree {
                push_evidence: capture_push_evidence(
                    runner,
                    &directory,
                    has_origin,
                    repository.in_container,
                )?,
                has_uncommitted_state,
                url_rules: own_rules.rules,

                relative_path: tree.relative_path.clone(),
                head: tree.head.clone(),
                working_state: state,
                working_state_skipped: skipped,
                index_gitlinks: inventory
                    .worktrees
                    .iter()
                    .find(|(path, _)| path == &tree.relative_path)
                    .map(|(_, tree)| {
                        tree.index_gitlinks
                            .iter()
                            .map(|entry| entry.value.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
                initialized_submodules: inventory
                    .worktrees
                    .iter()
                    .find(|(path, _)| path == &tree.relative_path)
                    .map(|(_, tree)| {
                        tree.initialized_submodules
                            .iter()
                            .map(|entry| entry.value.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
            });
        }
        stable &= selected_stash(runner, checkout, ownership)? == original_stash;
        stable &= pin_workspace_cut(runner, checkout, ownership)? == cut;
        if !stable {
            if attempt < 2 {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            anyhow::bail!(
                "workspace refs or HEAD kept changing during snapshot capture; retry after the checkout is idle"
            );
        }
        let selected = selected_refs(runner, checkout, &cut, ownership)?;
        let refs = selected.refs;
        let stash = original_stash
            .iter()
            .map(|entry| tracked_stash(runner, checkout, entry))
            .collect::<Result<Vec<_>>>()?;
        tips.extend(refs.values().cloned());
        tips.extend(stash.iter().map(|entry| entry.commit.clone()));
        tips.sort();
        tips.dedup();
        if let Some(keep) = keep_relative(repository)? {
            let mut tracked = Vec::new();
            let states = working
                .iter()
                .cloned()
                .chain(stash.iter().map(|entry| entry.commit.clone()))
                .collect::<Vec<_>>();
            for state in states
                .iter()
                .flat_map(|state| [state.clone(), format!("{state}^2")])
            {
                tracked.extend(tracked_keep_paths(
                    runner,
                    checkout,
                    key,
                    &keep,
                    Some(&state),
                )?);
            }
            for tree in &worktrees {
                if let Some(state) = &tree.working_state {
                    let nested_key = CheckoutKey {
                        repository_id: key.repository_id.clone(),
                        checkout_path: tree.relative_path.clone(),
                    };
                    tracked.extend(tracked_keep_paths(
                        runner,
                        &checkout.join(&tree.relative_path),
                        &nested_key,
                        &keep,
                        Some(state),
                    )?);
                }
            }
            if !tracked.is_empty() {
                return Err(SnapshotRefused::TrackedKeepFiles {
                    keep_dir: repository.keep_dir.clone().unwrap_or(keep),
                    paths: tracked,
                }
                .into());
            }
        }
        let pack = write_snapshot_pack(runner, checkout, &tips, &exclusions.objects, pack_path)?;
        check_repository_consent(
            repository,
            &repository_inventory(runner, repository)?,
            consent,
        )?;
        return Ok(WorkspaceSnapshot {
            object_format,
            worktree_config_enabled,
            origin_urls,
            push_urls,
            push_evidence,
            url_rules,
            worktree_url_rules,

            notices,
            head: head.clone(),
            refs_complete: selected.complete,
            deleted_refs: match ownership {
                CheckoutOwnership::Raw {
                    launch: Some(launch),
                } => launch
                    .refs
                    .keys()
                    .filter(|name| local_snapshot_ref(name) && !pinned.contains_key(*name))
                    .cloned()
                    .collect(),
                _ => Vec::new(),
            },
            branch_tracking: branch_tracking(runner, checkout, &refs)?,
            refs,
            stash,
            has_uncommitted_state: working.is_some()
                || skipped
                || staged_gitlink_changes(runner, checkout)?
                || submodule_positions_differ(&inventory),
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
            pack,
        });
    }
    unreachable!("every capture attempt returns or retries")
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
        identity.update(file_identity(&metadata)?.0.as_bytes());
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
        FileIdentity(identity.finalize().to_hex().to_string()),
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
fn repository_inventory(
    runner: &dyn GitCommandRunner,
    repository: &RepositoryInput,
) -> Result<WorkspaceInventory> {
    let mut inventory = workspace_inventory_scoped(
        runner,
        &repository.checkout,
        &repository.ownership,
        &repository_key(&repository.id),
    )?;
    if let Some(keep) = keep_relative(repository)? {
        inventory.keep_dir = repository.keep_dir.clone();
        inventory.tracked_keep_files = tracked_keep_paths(
            runner,
            &repository.checkout,
            &inventory.checkout,
            &keep,
            None,
        )?;
        for (path, nested) in &mut inventory.worktrees {
            nested.keep_dir = repository.keep_dir.clone();
            nested.tracked_keep_files = tracked_keep_paths(
                runner,
                &repository.checkout.join(path),
                &nested.checkout,
                &keep,
                None,
            )?;
        }
    }
    Ok(inventory)
}
pub fn workspace_inventory(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
) -> Result<WorkspaceInventory> {
    workspace_inventory_scoped(runner, checkout, ownership, &CheckoutKey::default())
}
pub fn session_inventory(
    runner: &dyn GitCommandRunner,
    repositories: &[RepositoryInput],
) -> Result<BTreeMap<String, WorkspaceInventory>> {
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
    Ok(result)
}
fn workspace_inventory_scoped(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
    key: &CheckoutKey,
) -> Result<WorkspaceInventory> {
    fn collect(
        runner: &dyn GitCommandRunner,
        checkout: &Path,
        raw: bool,
        nested: &[PathBuf],
        key: &CheckoutKey,
    ) -> Result<WorkspaceInventory> {
        let bytes = git_bytes(
            runner,
            checkout,
            ["ls-files", "--others", "--exclude-standard", "-z"],
            &[],
            "list untracked files",
        )?;
        let submodules = crate::checkpoint::inspect_snapshot_submodules(runner, checkout)?;
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
            untracked.push(UntrackedPath {
                checkout: key.clone(),
                identity: file_identity(&metadata)?,
                path,
                bytes: metadata.len(),
            });
        }
        Ok(WorkspaceInventory {
            notices: submodules.notices,
            checkout: key.clone(),
            untracked,
            untracked_stashes: Vec::new(),
            nested_repositories,
            tracked_keep_files: Vec::new(),
            keep_dir: None,
            dirty_submodules: submodules
                .dirty
                .into_iter()
                .map(|path| scoped(key, path))
                .collect(),
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
            hidden_tracked_edits: hidden.into_iter().map(|path| scoped(key, path)).collect(),
            operation: operation_record(runner, checkout, key)?,
            raw,
            remote_refs_stale: submodules.remote_refs_stale,
            worktrees: Vec::new(),
        })
    }
    let nested = discover_linked_worktrees(runner, checkout)?;
    let paths: Vec<_> = nested.iter().map(|tree| tree.relative.clone()).collect();
    let raw = matches!(ownership, CheckoutOwnership::Raw { .. });
    let mut result = collect(runner, checkout, raw, &paths, key)?;
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
            collect(runner, &tree.directory, raw, &children, &nested_key)?,
        ));
    }
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
    git_text(runner, destination, ["reset", "--hard", oid])?;
    Ok(())
}
fn restore_submodules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    submodules: &[SnapshotSubmodule],
    gitlinks: Option<&[SnapshotGitlink]>,
    allow_origin_fetch: bool,
) -> Result<()> {
    let mut submodules: Vec<_> = submodules.iter().collect();
    submodules.sort_by_key(|entry| entry.path.components().count());
    for path in submodules
        .iter()
        .map(|entry| &entry.path)
        .chain(gitlinks.into_iter().flatten().map(|entry| &entry.path))
    {
        ensure!(
            !path.as_os_str().is_empty()
                && path
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_))),
            "unsafe submodule path"
        );
    }
    let owning_checkout = |path: &Path| -> &Path {
        submodules
            .iter()
            .filter(|parent| parent.path != path && path.starts_with(&parent.path))
            .max_by_key(|parent| parent.path.components().count())
            .map(|parent| parent.path.as_path())
            .unwrap_or_else(|| Path::new(""))
    };
    let apply_index = |parent: &Path| -> Result<()> {
        let Some(gitlinks) = gitlinks else {
            return Ok(());
        };
        let owner = checkout.join(parent);
        let desired: Vec<_> = gitlinks
            .iter()
            .filter(|entry| owning_checkout(&entry.path) == parent)
            .collect();
        // This is the complete saved gitlink set, so absence also preserves
        // staged deletions that stash creation would otherwise omit.
        let current = crate::checkpoint::snapshot_index_gitlinks(runner, &owner)?;
        for path in current.into_iter().map(|entry| entry.path) {
            if !desired.iter().any(|entry| {
                entry
                    .path
                    .strip_prefix(parent)
                    .is_ok_and(|relative| relative == path)
            }) {
                let output = runner.run(
                    &owner,
                    &GitCommand {
                        arguments: vec![
                            "update-index".into(),
                            "--force-remove".into(),
                            "--".into(),
                            path.into_os_string(),
                        ],
                        ..Default::default()
                    },
                )?;
                ensure!(
                    output.status == 0,
                    "{}",
                    git_failure("restore deleted index gitlink", &output)
                );
            }
        }
        for submodule in desired {
            let mut entry = OsString::from(format!("160000,{},", submodule.commit));
            entry.push(submodule.path.strip_prefix(parent)?.as_os_str());
            let output = runner.run(
                &owner,
                &GitCommand {
                    arguments: vec![
                        "update-index".into(),
                        "--add".into(),
                        "--cacheinfo".into(),
                        entry,
                    ],
                    ..Default::default()
                },
            )?;
            ensure!(
                output.status == 0,
                "{}",
                git_failure("restore submodule index gitlink", &output)
            );
        }
        Ok(())
    };
    apply_index(Path::new(""))?;
    for submodule in &submodules {
        let parent = owning_checkout(&submodule.path);
        let owner = checkout.join(parent);
        let relative = submodule.path.strip_prefix(parent)?;
        crate::archive::ensure_no_symlink_ancestors(checkout, &submodule.path)?;
        let directory = checkout.join(&submodule.path);
        let admin = if directory.join(".git").try_exists()? {
            crate::archive::git::git_directories(runner, &directory)?.0
        } else {
            git_path(runner, &owner, &format!("modules/{}", submodule.name))?
        };
        let first_origin = submodule.origin_urls.first().with_context(|| {
            format!(
                "submodule {} has no recorded origin",
                submodule.path.display()
            )
        })?;
        let fresh = !admin.try_exists()?;
        if fresh {
            ensure!(
                allow_origin_fetch,
                "submodule {} needs a locally provisioned repository; origin fetching is disabled",
                submodule.path.display()
            );
            initialize_repository(
                runner,
                &directory,
                first_origin,
                &submodule.object_format,
                &submodule.url_rules,
                &submodule.worktree_url_rules,
                submodule.worktree_config_enabled,
            )?;
        } else {
            if !directory.join(".git").try_exists()? {
                let output = runner.run(
                    &owner,
                    &GitCommand {
                        arguments: vec![
                            "init".into(),
                            format!("--object-format={}", submodule.object_format).into(),
                            "--separate-git-dir".into(),
                            admin.as_os_str().to_owned(),
                            "--".into(),
                            directory.as_os_str().to_owned(),
                        ],
                        ..Default::default()
                    },
                )?;
                ensure!(
                    output.status == 0,
                    "{}",
                    git_failure("reattach provisioned submodule", &output)
                );
            }
            restore_url_rules(runner, &directory, &submodule.url_rules)?;
            if submodule.worktree_config_enabled {
                git_text(
                    runner,
                    &directory,
                    ["config", "--local", "extensions.worktreeConfig", "true"],
                )?;
            }
            restore_worktree_rules(runner, &directory, &submodule.worktree_url_rules)?;
            git_text(
                runner,
                &directory,
                ["config", "--replace-all", "remote.origin.url", first_origin],
            )?;
            git_text(
                runner,
                &directory,
                [
                    "config",
                    "--replace-all",
                    "remote.origin.fetch",
                    "+refs/heads/*:refs/remotes/origin/*",
                ],
            )?;
            unset_config(runner, &directory, "remote.origin.pushurl")?;
        }
        for url in submodule.origin_urls.iter().skip(1) {
            git_text(
                runner,
                &directory,
                ["remote", "set-url", "--add", "origin", url],
            )?;
        }
        for url in &submodule.push_urls {
            git_text(
                runner,
                &directory,
                ["config", "--add", "remote.origin.pushurl", url],
            )?;
        }
        if allow_origin_fetch {
            fetch_origin(runner, &directory)?;
        }
        let mut needed = crate::checkpoint::snapshot_gitlink_commits(runner, &owner, relative)?;
        needed.push(submodule.commit.clone());
        needed.sort();
        needed.dedup();
        for oid in needed {
            if run_git(runner, &directory, ["cat-file", "-e", &oid], &[])?.status != 0 {
                ensure!(
                    allow_origin_fetch,
                    "submodule {} is missing {oid}; origin fetching is disabled",
                    submodule.path.display()
                );
                git_text(runner, &directory, ["fetch", "origin", &oid])?;
            }
            require_object(
                runner,
                &directory,
                &format!("submodule {}", submodule.path.display()),
                &oid,
            )?;
        }
        // Give Git a real HEAD before registering/absorbing the module. Its
        // index gitlink can intentionally differ from this checked-out commit.
        git_text(
            runner,
            &directory,
            ["checkout", "--detach", &submodule.commit],
        )?;
        git_text(
            runner,
            &owner,
            [
                "config",
                &format!("submodule.{}.url", submodule.name),
                &submodule.url,
            ],
        )?;
        let output = runner.run(
            &owner,
            &GitCommand {
                arguments: vec![
                    "submodule".into(),
                    "absorbgitdirs".into(),
                    "--".into(),
                    relative.as_os_str().to_owned(),
                ],
                ..Default::default()
            },
        )?;
        ensure!(
            output.status == 0,
            "{}",
            git_failure("register restored submodule repository", &output)
        );
        apply_index(&submodule.path)?;
    }
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RestoreBase {
    Clone {
        origin: String,
        seed: Option<PathBuf>,
    },
    /// The caller has already verified this workspace's history boundary.
    Provisioned { allow_origin_fetch: bool },
}
fn fetch_missing_object(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    oid: &str,
    allowed: bool,
    attempted: &mut std::collections::BTreeSet<String>,
    failures: &mut Vec<String>,
) -> Result<()> {
    if attempted.insert(oid.to_owned()) {
        if !allowed {
            failures.push(format!("origin fetch is disabled for missing object {oid}"));
            return Ok(());
        }
        match run_git(runner, checkout, ["fetch", "origin", oid], &[]) {
            Ok(output) if output.status == 0 => {}
            Ok(output) => {
                failures.push(git_failure("fetch missing snapshot object", &output).to_string())
            }
            Err(error) => failures.push(format!("fetch {oid}: {error:#}")),
        }
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
fn restore_worktree_rules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    rules: &[SnapshotConfigEntry],
) -> Result<()> {
    // `worktree add` copies config.worktree from the primary checkout. Replace
    // those inherited URL rules so each restored checkout keeps its own scope.
    if worktree_config_enabled(runner, checkout)? {
        for entry in read_config_entries(runner, checkout, true, r"^url\.")? {
            let output = run_git(
                runner,
                checkout,
                ["config", "--worktree", "--unset-all", &entry.name],
                &[],
            )?;
            ensure!(
                output.status == 0 || output.status == 5,
                "{}",
                git_failure("clear inherited worktree URL rule", &output)
            );
        }
    }
    if !rules.is_empty() {
        git_text(
            runner,
            checkout,
            ["config", "extensions.worktreeConfig", "true"],
        )?;
        for rule in rules {
            git_text(
                runner,
                checkout,
                ["config", "--worktree", "--add", &rule.name, &rule.value],
            )?;
        }
    }
    Ok(())
}
fn initialize_repository(
    runner: &dyn GitCommandRunner,
    destination: &Path,
    origin: &str,
    object_format: &str,
    url_rules: &[SnapshotConfigEntry],
    worktree_rules: &[SnapshotConfigEntry],
    worktree_config_enabled: bool,
) -> Result<()> {
    let parent = destination.parent().context("destination has no parent")?;
    fs::create_dir_all(parent)?;
    ensure!(
        !destination.try_exists()? || fs::read_dir(destination)?.next().is_none(),
        "snapshot destination {} is not empty",
        destination.display()
    );
    ensure!(
        matches!(object_format, "sha1" | "sha256"),
        "unsupported Git object format {object_format}"
    );
    let output = runner.run(
        parent,
        &GitCommand {
            arguments: vec![
                "init".into(),
                "--initial-branch=main".into(),
                format!("--object-format={object_format}").into(),
                "--".into(),
                destination.as_os_str().to_owned(),
            ],
            ..Default::default()
        },
    )?;
    ensure!(
        output.status == 0,
        "{}",
        git_failure("initialize snapshot restore", &output)
    );
    restore_url_rules(runner, destination, url_rules)?;
    if worktree_config_enabled {
        git_text(
            runner,
            destination,
            ["config", "extensions.worktreeConfig", "true"],
        )?;
    }
    restore_worktree_rules(runner, destination, worktree_rules)?;
    git_text(runner, destination, ["remote", "add", "origin", origin])?;
    Ok(())
}
fn borrow_objects(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    objects: &Path,
) -> Result<PathBuf> {
    let path = git_path(runner, repository, "objects/info/alternates")?;
    fs::create_dir_all(path.parent().context("alternates path has no parent")?)?;
    let quoted = objects
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|byte| format!("\\{byte:03o}"))
        .collect::<String>();
    fs::write(&path, format!("\"{quoted}\"\n"))?;
    Ok(path)
}
fn configure_remote_lists(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    origins: &[String],
    pushes: &[String],
) -> Result<()> {
    for url in origins.iter().skip(1) {
        git_text(
            runner,
            checkout,
            ["remote", "set-url", "--add", "origin", url],
        )?;
    }
    for url in pushes {
        git_text(
            runner,
            checkout,
            ["config", "--add", "remote.origin.pushurl", url],
        )?;
    }
    Ok(())
}
fn fetch_origin(runner: &dyn GitCommandRunner, destination: &Path) -> Result<()> {
    git_text(
        runner,
        destination,
        ["fetch", "origin", "+refs/heads/*:refs/remotes/origin/*"],
    )?;
    git_text(runner, destination, ["fetch", "--tags", "origin"])?;
    let default_head = run_git(
        runner,
        destination,
        ["remote", "set-head", "origin", "--auto"],
        &[],
    )?;
    if default_head.status != 0 {
        tracing::warn!(error = %git_failure("set origin default branch", &default_head), "origin has no usable default branch");
    }
    Ok(())
}
pub fn restore_workspace_snapshot(
    runner: &dyn GitCommandRunner,
    snapshot: &WorkspaceSnapshot,
    destination: &Path,
    base: &RestoreBase,
) -> Result<()> {
    let destination = std::path::absolute(destination)?;
    let destination = destination.as_path();
    let allow_origin_fetch = match base {
        RestoreBase::Clone { origin, seed } => {
            ensure!(
                !snapshot.origin_urls.is_empty(),
                "cannot restore snapshot without an origin remote; resume on its raw checkout"
            );
            initialize_repository(
                runner,
                destination,
                origin,
                &snapshot.object_format,
                &snapshot.url_rules,
                &snapshot.worktree_url_rules,
                snapshot.worktree_config_enabled,
            )?;
            configure_remote_lists(
                runner,
                destination,
                &snapshot.origin_urls,
                &snapshot.push_urls,
            )?;
            let mut alternates = None;
            if let Some(seed) = seed.as_deref()
                && seed.try_exists()?
                && let Ok((_, common)) = crate::archive::git::git_directories(runner, seed)
            {
                let objects = common.join("objects");
                if objects.is_dir() {
                    let path = borrow_objects(runner, destination, &objects)?;
                    alternates = Some(path);
                }
            }
            fetch_origin(runner, destination)?;
            if let Some(alternates) = alternates {
                git_text(runner, destination, ["repack", "-a", "-d"])?;
                fs::remove_file(alternates)?;
            }
            true
        }
        RestoreBase::Provisioned { allow_origin_fetch } => {
            git_text(runner, destination, ["rev-parse", "--is-inside-work-tree"])?;
            restore_url_rules(runner, destination, &snapshot.url_rules)?;
            if snapshot.worktree_config_enabled {
                git_text(
                    runner,
                    destination,
                    ["config", "extensions.worktreeConfig", "true"],
                )?;
            }
            restore_worktree_rules(runner, destination, &snapshot.worktree_url_rules)?;
            if let Some(first) = snapshot.origin_urls.first() {
                git_text(
                    runner,
                    destination,
                    ["config", "--replace-all", "remote.origin.url", first],
                )?;
                for url in snapshot.origin_urls.iter().skip(1) {
                    git_text(
                        runner,
                        destination,
                        ["config", "--add", "remote.origin.url", url],
                    )?;
                }
                unset_config(runner, destination, "remote.origin.pushurl")?;
                for url in &snapshot.push_urls {
                    git_text(
                        runner,
                        destination,
                        ["config", "--add", "remote.origin.pushurl", url],
                    )?;
                }
            }
            *allow_origin_fetch
        }
    };
    if let Some(pack) = &snapshot.pack {
        let format = git_text(runner, destination, ["rev-parse", "--show-object-format"])?;
        let hash_bytes = match format.as_str() {
            "sha1" => 20,
            "sha256" => 32,
            _ => anyhow::bail!("unsupported Git object format {format}"),
        };
        let mut file = fs::File::open(pack)
            .with_context(|| format!("open snapshot pack {}", pack.display()))?;
        file.seek(SeekFrom::End(-hash_bytes))?;
        let mut hash = vec![0; hash_bytes as usize];
        file.read_exact(&mut hash)?;
        let hash = mj_core::hex::lower_hex(&hash);
        let directory = git_path(runner, destination, "objects/pack")?;
        fs::create_dir_all(&directory)?;
        let path = directory.join(format!("pack-{hash}.pack"));
        fs::copy(pack, &path).context("copy snapshot pack into clone")?;
        let output = runner.run(
            destination,
            &GitCommand {
                arguments: vec!["index-pack".into(), path.as_os_str().to_owned()],
                ..Default::default()
            },
        )?;
        ensure!(
            output.status == 0,
            "{}",
            git_failure("import snapshot pack", &output)
        );
    }
    let mut tips: Vec<String> = snapshot.refs.values().cloned().collect();
    tips.push(snapshot.head.commit().to_owned());
    tips.extend(snapshot.working_state.clone());
    tips.extend(snapshot.stash.iter().map(|entry| entry.commit.clone()));
    for tree in &snapshot.worktrees {
        tips.push(tree.head.commit().to_owned());
        tips.extend(tree.working_state.clone());
    }
    tips.sort();
    tips.dedup();
    let mut attempted = std::collections::BTreeSet::new();
    let mut fetch_failures = Vec::new();
    for oid in missing_tips(runner, destination, &tips)? {
        fetch_missing_object(
            runner,
            destination,
            &oid,
            allow_origin_fetch,
            &mut attempted,
            &mut fetch_failures,
        )?;
    }
    let remaining_tips = missing_tips(runner, destination, &tips)?;
    ensure!(
        remaining_tips.is_empty(),
        "snapshot objects {} for HEAD, {} are missing; origin may have rewritten history; {}",
        remaining_tips.join(", "),
        snapshot.refs.keys().cloned().collect::<Vec<_>>().join(", "),
        fetch_failures.join("; ")
    );
    // Origin tags remain available to raw conversions, except when replaced,
    // deleted, or prefix-conflicting with a saved tag.
    let fetched_tags = git_text(
        runner,
        destination,
        [
            "for-each-ref",
            "--format=%(refname)%00%(objectname)",
            "refs/tags",
        ],
    )?;
    for line in fetched_tags.lines() {
        let (name, oid) = line.split_once('\0').context("malformed clone ref")?;
        let conflict = snapshot.refs.iter().any(|(saved, saved_oid)| {
            (saved == name && saved_oid != oid)
                || saved
                    .strip_prefix(name)
                    .is_some_and(|suffix| suffix.starts_with('/'))
                || name
                    .strip_prefix(saved)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        });
        if conflict
            || (snapshot.refs_complete
                && name.starts_with("refs/tags/")
                && !snapshot.refs.contains_key(name))
        {
            git_text(runner, destination, ["update-ref", "-d", name])?;
        }
    }
    for name in &snapshot.deleted_refs {
        ensure!(
            local_snapshot_ref(name),
            "deleted snapshot ref {name} is outside the local-ref allowlist"
        );
        git_text(runner, destination, ["update-ref", "-d", name])?;
    }
    let mut restore_refs = snapshot.refs.clone();
    for head in
        std::iter::once(&snapshot.head).chain(snapshot.worktrees.iter().map(|tree| &tree.head))
    {
        if let SnapshotHead::Branch { name, commit } = head {
            restore_refs.insert(name.clone(), commit.clone());
        }
    }
    for (name, oid) in &restore_refs {
        require_object(runner, destination, name, oid)?;
        git_text(runner, destination, ["update-ref", name, oid])?;
    }
    let graph_command = GitCommand {
        arguments: [
            "rev-list",
            "--objects",
            "--quiet",
            "--missing=print",
            "--stdin",
        ]
        .into_iter()
        .map(OsString::from)
        .collect(),
        stdin: format!("{}\n", tips.join("\n")).into_bytes(),
        ..Default::default()
    };
    // --quiet suppresses ordinary object lines while --missing=print still
    // reports missing commits, trees AND blobs. No object listing is buffered.
    let mut commit_graph_command = graph_command.clone();
    commit_graph_command
        .arguments
        .push("--filter=tree:0".into());
    let first = runner.run(destination, &commit_graph_command)?;
    for line in first.stdout.split(|byte| *byte == b'\n') {
        if let Some(oid) = line.strip_prefix(b"?") {
            fetch_missing_object(
                runner,
                destination,
                std::str::from_utf8(oid)?,
                allow_origin_fetch,
                &mut attempted,
                &mut fetch_failures,
            )?;
        }
    }
    let graph = runner.run(destination, &graph_command)?;
    let missing = graph
        .stdout
        .split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"?"));
    ensure!(
        graph.status == 0 && !missing,
        "snapshot history for {} is incomplete; origin may have rewritten history; missing objects: {}; {} {}",
        std::iter::once("HEAD".to_owned())
            .chain(restore_refs.keys().cloned())
            .collect::<Vec<_>>()
            .join(", "),
        String::from_utf8_lossy(&graph.stdout).trim(),
        String::from_utf8_lossy(&graph.stderr).trim(),
        fetch_failures.join("; ")
    );
    for (name, tracking) in &snapshot.branch_tracking {
        unset_config(runner, destination, &format!("branch.{name}.remote"))?;
        unset_config(runner, destination, &format!("branch.{name}.merge"))?;
        if let Some(remote) = &tracking.remote {
            git_text(
                runner,
                destination,
                ["config", &format!("branch.{name}.remote"), remote],
            )?;
        }
        for merge in &tracking.merge {
            git_text(
                runner,
                destination,
                ["config", "--add", &format!("branch.{name}.merge"), merge],
            )?;
        }
    }
    for entry in snapshot.stash.iter().rev() {
        require_object(runner, destination, "stash", &entry.commit)?;
        let output = runner.run(
            destination,
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
            git_failure("restore stash list", &output)
        );
    }
    checkout_head(runner, destination, &snapshot.head)?;
    if let Some(state) = &snapshot.working_state {
        require_object(runner, destination, "working state", state)?;
        git_text(runner, destination, ["stash", "apply", "--index", state])?;
    }
    restore_submodules(
        runner,
        destination,
        &snapshot.initialized_submodules,
        (!snapshot.working_state_skipped).then_some(snapshot.index_gitlinks.as_slice()),
        allow_origin_fetch,
    )?;
    for tree in &snapshot.worktrees {
        ensure!(
            !tree.relative_path.as_os_str().is_empty()
                && tree
                    .relative_path
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "unsafe nested worktree path"
        );
        crate::archive::ensure_no_symlink_ancestors(destination, &tree.relative_path)?;
        let path = destination.join(&tree.relative_path);
        let mut arguments: Vec<OsString> = vec!["worktree".into(), "add".into(), "--force".into()];
        let head_oid = tree.head.commit();
        require_object(
            runner,
            destination,
            &format!("worktree {} HEAD", tree.relative_path.display()),
            head_oid,
        )?;
        let rev = match &tree.head {
            SnapshotHead::Branch { name, .. } => name
                .strip_prefix("refs/heads/")
                .context("worktree HEAD is not a branch")?,
            SnapshotHead::Detached(oid) => {
                arguments.push("--detach".into());
                oid
            }
        };
        arguments.extend(["--".into(), path.as_os_str().to_owned(), rev.into()]);
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
            git_failure("restore nested worktree", &output)
        );
        checkout_head(runner, &path, &tree.head)?;
        if let Some(state) = &tree.working_state {
            require_object(
                runner,
                destination,
                &format!("worktree {} working state", tree.relative_path.display()),
                state,
            )?;
            git_text(runner, &path, ["stash", "apply", "--index", state])?;
        }
        restore_worktree_rules(runner, &path, &tree.url_rules)?;
        restore_submodules(
            runner,
            &path,
            &tree.initialized_submodules,
            (!tree.working_state_skipped).then_some(tree.index_gitlinks.as_slice()),
            allow_origin_fetch,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::SystemGit;

    struct FileGit;
    impl GitCommandRunner for FileGit {
        fn run(
            &self,
            repository: &Path,
            command: &GitCommand,
        ) -> Result<crate::archive::GitOutput> {
            let mut command = command.clone();
            let mut arguments = vec!["-c".into(), "protocol.file.allow=always".into()];
            arguments.extend(command.arguments);
            command.arguments = arguments;
            SystemGit.run(repository, &command)
        }
    }
    struct HostGit {
        global: PathBuf,
    }
    impl GitCommandRunner for HostGit {
        fn run(
            &self,
            repository: &Path,
            command: &GitCommand,
        ) -> Result<crate::archive::GitOutput> {
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
            SystemGit.run(repository, &command)
        }
    }
    fn host_git(path: &Path, name: &str) -> HostGit {
        let global = path.join(name);
        fs::write(&global, "").unwrap();
        fs::write(global.with_extension("system"), "").unwrap();
        HostGit { global }
    }
    fn add_submodule(root: &Repo, module: &Repo) -> PathBuf {
        git(
            &root.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                module.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&root.checkout, &["commit", "-am", "registered submodule"]);
        git(&root.checkout, &["push", "origin", "main"]);
        root.checkout.join("sub")
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
    fn item_path(root: &Path, path: impl Into<PathBuf>) -> UntrackedItem {
        scoped_item_path(root, CheckoutKey::default(), path)
    }
    fn item_stash(commit: String, message: String) -> UntrackedItem {
        UntrackedItem::Stash {
            checkout: CheckoutKey::default(),
            commit,
            message,
        }
    }
    fn clone_base(snapshot: &WorkspaceSnapshot, seed: Option<&Path>) -> RestoreBase {
        RestoreBase::Clone {
            origin: snapshot.origin_urls.first().cloned().unwrap_or_default(),
            seed: seed.map(Path::to_path_buf),
        }
    }
    struct Repo {
        temp: tempfile::TempDir,
        origin: PathBuf,
        checkout: PathBuf,
    }
    fn git(path: &Path, args: &[&str]) -> String {
        let output = SystemGit
            .run(
                path,
                &GitCommand {
                    arguments: args.iter().map(OsString::from).collect(),
                    env: [
                        "GIT_AUTHOR_NAME",
                        "GIT_AUTHOR_EMAIL",
                        "GIT_COMMITTER_NAME",
                        "GIT_COMMITTER_EMAIL",
                    ]
                    .into_iter()
                    .map(|k| (k.into(), "snapshot@example.test".into()))
                    .collect(),
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
    impl Repo {
        fn new() -> Self {
            Self::new_with_format("sha1")
        }
        fn new_with_format(object_format: &str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let origin = temp.path().join("origin.git");
            let checkout = temp.path().join("checkout");
            git(
                temp.path(),
                &[
                    "init",
                    "--bare",
                    "--initial-branch=main",
                    &format!("--object-format={object_format}"),
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
            // stash create requires an identity even though it doesn't change refs.
            git(&checkout, &["config", "user.name", "Snapshot Test"]);
            git(
                &checkout,
                &["config", "user.email", "snapshot@example.test"],
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
                &self.temp.path().join("snapshot.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap()
        }
        fn restore(&self, snapshot: &WorkspaceSnapshot, seed: Option<&Path>) -> PathBuf {
            let path = self.temp.path().join("restored");
            restore_workspace_snapshot(&FileGit, snapshot, &path, &clone_base(snapshot, seed))
                .unwrap();
            path
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
            let output = run_git(&FileGit, &self.checkout, [kind, "side"], &[]).unwrap();
            assert_ne!(output.status, 0);
        }
    }

    #[test]
    fn pushed_branch_records_refs_without_pack() {
        let r = Repo::new();
        git(
            &r.checkout,
            &["tag", "-a", "origin-tag", "-m", "already published"],
        );
        git(&r.checkout, &["push", "origin", "origin-tag"]);
        git(
            &r.checkout,
            &["branch", "--set-upstream-to=origin/main", "main"],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_none());
        assert_eq!(
            snapshot.branch_tracking["main"],
            BranchTracking {
                remote: Some("origin".into()),
                merge: vec!["refs/heads/main".into()]
            }
        );
        assert!(snapshot.refs.contains_key("refs/tags/origin-tag"));
        assert!(!r.temp.path().join("snapshot.pack").exists());
        assert_eq!(
            snapshot.refs["refs/heads/main"],
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
        let restored = r.restore(&snapshot, Some(&r.checkout));
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            snapshot.refs["refs/heads/main"]
        );
        assert!(!restored.join(".git/objects/info/alternates").exists());
        assert_eq!(git(&restored, &["config", "branch.main.remote"]), "origin");
        assert_eq!(
            git(&restored, &["config", "branch.main.merge"]),
            "refs/heads/main"
        );
    }
    #[test]
    fn unpushed_commit_and_tag_survive() {
        let r = Repo::new();
        r.commit("unpublished\n");
        // Incompressible content exercises both pack stdout and import stdin
        // beyond the 64 KiB pipe buffer boundary.
        let mut state = 0x12345678_u32;
        let large: Vec<u8> = (0..262_144)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        fs::write(r.checkout.join("large"), &large).unwrap();
        git(&r.checkout, &["add", "large"]);
        git(&r.checkout, &["commit", "-m", "large tracked file"]);
        git(&r.checkout, &["tag", "-a", "v1", "-m", "release"]);
        for name in [
            "refs/mj/reviewed/test",
            "refs/mj/overridden/test",
            "refs/mj/push-base/test",
        ] {
            git(&r.checkout, &["update-ref", name, "HEAD"]);
        }
        git(&r.checkout, &["config", "fetch.pruneTags", "true"]);
        git(&r.checkout, &["config", "remote.origin.pruneTags", "true"]);
        git(
            &r.checkout,
            &[
                "config",
                "--add",
                "remote.origin.fetch",
                "+refs/tags/*:refs/tags/*",
            ],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_some());
        assert!(snapshot.refs.contains_key("refs/tags/v1"));
        assert_eq!(git(&r.checkout, &["tag", "--list", "v1"]), "v1");
        assert!(snapshot.refs.contains_key("refs/mj/reviewed/test"));
        assert!(snapshot.refs.contains_key("refs/mj/overridden/test"));
        assert!(snapshot.refs.contains_key("refs/mj/push-base/test"));
        assert!(fs::metadata(snapshot.pack.as_ref().unwrap()).unwrap().len() > 65_536);
        let restored = r.restore(&snapshot, None);
        for (name, oid) in &snapshot.refs {
            assert_eq!(&git(&restored, &["rev-parse", name]), oid);
        }
        assert_eq!(fs::read(restored.join("large")).unwrap(), large);
        assert_eq!(git(&r.checkout, &["for-each-ref", "refs/mj/snapshot"]), "");
        assert_eq!(git(&restored, &["for-each-ref", "refs/mj/restore"]), "");
        assert_eq!(git(&restored, &["status", "--porcelain"]), "");
    }
    #[test]
    fn three_stash_messages_survive() {
        let r = Repo::new();
        for message in ["one", "two", "three"] {
            fs::write(r.checkout.join("file"), message).unwrap();
            git(&r.checkout, &["stash", "push", "-m", message]);
        }
        let before = read_stash(&FileGit, &r.checkout).unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(read_stash(&FileGit, &r.checkout).unwrap(), before);
        let restored = r.restore(&snapshot, None);
        assert_eq!(read_stash(&FileGit, &restored).unwrap(), before);
    }
    #[test]
    fn staged_and_unstaged_edits_to_same_file_survive() {
        let r = Repo::new();
        fs::write(r.checkout.join("file"), "STAGED\nsecond\nthird\n").unwrap();
        git(&r.checkout, &["add", "file"]);
        fs::write(r.checkout.join("file"), "STAGED\nsecond\nUNSTAGED\n").unwrap();
        fs::write(r.checkout.join("ignored"), "ignored").unwrap();
        fs::write(r.checkout.join("untracked\n spaced"), "lost").unwrap();
        fs::write(r.checkout.join(".git/info/exclude"), "ignored\n").unwrap();
        let index = git(&r.checkout, &["diff", "--cached"]);
        let working = git(&r.checkout, &["diff"]);
        git(&r.checkout, &["config", "user.name", ""]);
        git(&r.checkout, &["config", "user.email", ""]);
        git(&r.checkout, &["config", "user.useConfigOnly", "true"]);
        assert_ne!(
            run_git(&FileGit, &r.checkout, ["stash", "create"], &[])
                .unwrap()
                .status,
            0
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let inventory =
            workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(
            inventory.untracked,
            vec![UntrackedPath {
                checkout: CheckoutKey::default(),
                path: "untracked\n spaced".into(),
                bytes: 4,
                identity: file_identity(
                    &fs::symlink_metadata(r.checkout.join("untracked\n spaced")).unwrap()
                )
                .unwrap(),
            }]
        );
        assert_eq!(git(&r.checkout, &["diff", "--cached"]), index);
        assert_eq!(git(&r.checkout, &["diff"]), working);
        let restored = r.restore(&snapshot, None);
        assert_eq!(git(&restored, &["diff", "--cached"]), index);
        assert_eq!(git(&restored, &["diff"]), working);
        assert!(!restored.join("untracked\n spaced").exists());
        assert!(!restored.join("ignored").exists());
    }
    #[test]
    fn nested_detached_worktree_and_dirty_state_survive() {
        let r = Repo::new();
        let nested = r.checkout.join("lanes/nested");
        git(
            &r.checkout,
            &["worktree", "add", "--detach", nested.to_str().unwrap()],
        );
        fs::write(nested.join("file"), "nested commit\n").unwrap();
        git(&nested, &["commit", "-am", "nested"]);
        fs::write(nested.join("file"), "nested working state\n").unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let inventory =
            workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert!(inventory.untracked.is_empty());
        assert_eq!(inventory.worktrees.len(), 1);
        let restored = r.restore(&snapshot, None).join("lanes/nested");
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&nested, &["rev-parse", "HEAD"])
        );
        assert_eq!(git(&restored, &["diff"]), git(&nested, &["diff"]));
    }
    #[test]
    fn conflicted_merge_aborts_managed_and_leaves_raw_untouched() {
        let r = Repo::new();
        r.conflict();
        r.start_conflict("merge");
        let before = fs::read(r.checkout.join("file")).unwrap();
        let estimate =
            estimate_delta(&FileGit, &r.checkout, &CheckoutOwnership::Managed, None).unwrap();
        assert!(estimate.working_state_bytes.is_none());
        assert!(r.checkout.join(".git/MERGE_HEAD").exists());
        let raw = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(raw.working_state_skipped);
        assert!(r.checkout.join(".git/MERGE_HEAD").exists());
        assert_eq!(fs::read(r.checkout.join("file")).unwrap(), before);
        let managed = r.capture(&CheckoutOwnership::Managed);
        assert!(!managed.working_state_skipped);
        assert!(!r.checkout.join(".git/MERGE_HEAD").exists());
        let restored = r.restore(&managed, None);
        assert_eq!(fs::read(restored.join("file")).unwrap(), b"main version\n");
    }
    #[test]
    fn raw_launch_excludes_user_refs_and_stashes_but_keeps_nested_worktrees() {
        let r = Repo::new();
        git(&r.checkout, &["tag", "upstream-unchanged"]);
        git(&r.checkout, &["push", "origin", "upstream-unchanged"]);
        let origin_tag = git(&r.checkout, &["rev-parse", "refs/tags/upstream-unchanged"]);
        git(&r.checkout, &["branch", "user-branch"]);
        fs::write(r.checkout.join("file"), "old stash").unwrap();
        git(&r.checkout, &["stash", "push", "-m", "user stash"]);
        let legacy = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(legacy.refs.contains_key("refs/heads/user-branch"));
        assert_eq!(legacy.stash.len(), 1);
        let existing = r.checkout.join("user-worktree");
        git(
            &r.checkout,
            &["worktree", "add", "--detach", existing.to_str().unwrap()],
        );
        record_launch(&FileGit, &r.checkout, "session-1").unwrap();
        fs::write(existing.join("user-untracked"), "not session work").unwrap();
        fs::write(existing.join("file"), "nested edit\n").unwrap();
        let launch = LaunchRecord::read(&FileGit, &r.checkout, "session-1")
            .unwrap()
            .unwrap();
        r.commit("session commit\n");
        git(&r.checkout, &["branch", "agent-branch"]);
        fs::write(r.checkout.join("file"), "new stash").unwrap();
        git(&r.checkout, &["stash", "push", "-m", "agent stash"]);
        let snapshot = r.capture(&CheckoutOwnership::Raw {
            launch: Some(launch),
        });
        assert!(!snapshot.refs.contains_key("refs/heads/user-branch"));
        assert!(!snapshot.refs.contains_key("refs/tags/upstream-unchanged"));
        assert!(snapshot.refs.contains_key("refs/heads/agent-branch"));
        assert_eq!(snapshot.stash.len(), 1);
        assert!(snapshot.stash[0].message.ends_with("agent stash"));
        assert_eq!(snapshot.worktrees.len(), 1);
        let owned = CheckoutOwnership::Raw {
            launch: LaunchRecord::read(&FileGit, &r.checkout, "session-1").unwrap(),
        };
        let inventory = workspace_inventory(&FileGit, &r.checkout, &owned).unwrap();
        assert_eq!(
            inventory.worktrees[0].1.untracked[0].path,
            Path::new("user-untracked")
        );
        capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &owned,
            &r.temp.path().join("second.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            fs::read_to_string(restored.join("user-worktree/file")).unwrap(),
            "nested edit\n"
        );
        assert!(!restored.join("user-worktree/user-untracked").exists());
        assert_eq!(git(&restored, &["branch", "--list", "user-branch"]), "");
        assert_eq!(
            git(&restored, &["rev-parse", "refs/tags/upstream-unchanged"]),
            origin_tag
        );
    }
    #[test]
    fn unreachable_origin_uses_stale_remote_tracking_refs() {
        let r = Repo::new();
        r.commit("unpushed\n");
        let away = r.temp.path().join("away.git");
        fs::rename(&r.origin, &away).unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.remote_refs_stale);
        fs::rename(&away, &r.origin).unwrap();
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
    }
    // Hard-won: 2b451f9b4: checking only tips missed a rewritten-away parent.
    #[test]
    fn rewritten_origin_reports_missing_parent_object() {
        let r = Repo::new();
        r.commit("pushed then deleted\n");
        git(&r.checkout, &["push", "origin", "main"]);
        r.commit("unpublished child\n");
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_some());
        let base = git(&r.checkout, &["rev-parse", "HEAD~2"]);
        git(&r.origin, &["update-ref", "refs/heads/main", &base]);
        git(&r.origin, &["reflog", "expire", "--expire=now", "--all"]);
        git(&r.origin, &["gc", "--prune=now"]);
        let error = restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &r.temp.path().join("restored"),
            &clone_base(&snapshot, None),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("refs/heads/main"), "{error}");
        assert!(error.contains("history for"), "{error}");
        assert!(
            error.contains("origin may have rewritten history"),
            "{error}"
        );
    }
    #[test]
    fn shown_merge_consent_does_not_cover_rebase() {
        let r = Repo::new();
        r.conflict();
        r.start_conflict("rebase");
        let head = git(&r.checkout, &["rev-parse", "HEAD"]);
        let before = fs::read(r.checkout.join("file")).unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Shown(vec![ShownOperation {
                    checkout: CheckoutKey::default(),
                    kind: GitOperationKind::Merge,
                    identity: "previous merge".into(),
                }]),
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::OperationInProgress(ShownOperation {
                kind: GitOperationKind::Rebase,
                ..
            }))
        ));
        assert_eq!(git(&r.checkout, &["rev-parse", "HEAD"]), head);
        assert_eq!(fs::read(r.checkout.join("file")).unwrap(), before);
        assert_eq!(
            operation(&FileGit, &r.checkout).unwrap(),
            Some(GitOperationKind::Rebase)
        );
    }
    #[test]
    fn raw_rebase_records_original_branch_without_aborting() {
        let r = Repo::new();
        r.conflict();
        let original = git(&r.checkout, &["rev-parse", "HEAD"]);
        r.start_conflict("rebase");
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert_eq!(
            snapshot.head,
            SnapshotHead::Branch {
                name: "refs/heads/main".into(),
                commit: git(&r.checkout, &["rev-parse", "refs/heads/main"])
            }
        );
        assert_eq!(snapshot.refs["refs/heads/main"], original);
        assert!(snapshot.working_state_skipped);
        assert_eq!(
            operation(&FileGit, &r.checkout).unwrap(),
            Some(GitOperationKind::Rebase)
        );
        let restored = r.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", "HEAD"]), original);
    }
    #[test]
    fn detached_primary_unpushed_commit_survives() {
        let r = Repo::new();
        git(&r.checkout, &["checkout", "--detach"]);
        r.commit("detached unpushed\n");
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_some());
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
        assert_eq!(git(&restored, &["status", "--porcelain"]), "");
        assert!(matches!(
            read_head(&FileGit, &restored).unwrap(),
            SnapshotHead::Detached(_)
        ));
    }
    #[test]
    fn raw_without_origin_is_in_place_only() {
        let r = Repo::new();
        git(&r.checkout, &["remote", "remove", "origin"]);
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(snapshot.origin_urls.is_empty());
        assert!(snapshot.pack.is_none());
        assert!(snapshot.working_state.is_none());
        assert_eq!(
            snapshot.head,
            SnapshotHead::Branch {
                name: "refs/heads/main".into(),
                commit: git(&r.checkout, &["rev-parse", "refs/heads/main"])
            }
        );
        let error = restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &r.temp.path().join("restored"),
            &clone_base(&snapshot, None),
        )
        .unwrap_err();
        assert!(error.to_string().contains("without an origin remote"));
    }
    #[test]
    fn shown_untracked_consent_refuses_only_new_paths_before_mutation() {
        let r = Repo::new();
        r.conflict();
        r.start_conflict("merge");
        fs::write(r.checkout.join("shown"), "shown").unwrap();
        fs::write(r.checkout.join("new"), "new").unwrap();
        let before = fs::read(r.checkout.join("file")).unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Shown(vec![item_path(&r.checkout, "shown")]),
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        match error.downcast_ref::<SnapshotRefused>().unwrap() {
            SnapshotRefused::Untracked(paths) => {
                assert_eq!(paths, &vec![item_path(&r.checkout, "new")])
            }
            other => panic!("unexpected refusal {other:?}"),
        }
        assert!(r.checkout.join(".git/MERGE_HEAD").exists());
        assert_eq!(fs::read(r.checkout.join("file")).unwrap(), before);
        assert!(!r.temp.path().join("snapshot.pack").exists());
    }
    #[test]
    fn any_consent_covers_untracked_and_managed_operation() {
        let r = Repo::new();
        r.conflict();
        r.start_conflict("merge");
        fs::write(r.checkout.join("untracked"), "discard").unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(!r.checkout.join(".git/MERGE_HEAD").exists());
        assert!(r.checkout.join("untracked").exists());
        let restored = r.restore(&snapshot, None);
        assert!(!restored.join("untracked").exists());
        assert_eq!(fs::read(restored.join("file")).unwrap(), b"main version\n");
    }
    #[test]
    fn stash_untracked_parent_requires_consent_and_never_restores() {
        let r = Repo::new();
        record_launch(&FileGit, &r.checkout, "raw-stash").unwrap();
        let ownership = CheckoutOwnership::Raw {
            launch: LaunchRecord::read(&FileGit, &r.checkout, "raw-stash").unwrap(),
        };
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
    #[test]
    fn nested_cherry_pick_abort_records_post_abort_shared_branch() {
        let r = Repo::new();
        git(&r.checkout, &["branch", "lane"]);
        let nested = r.checkout.join("lane");
        git(
            &r.checkout,
            &["worktree", "add", nested.to_str().unwrap(), "lane"],
        );
        r.commit("main first\n");
        let first = git(&r.checkout, &["rev-parse", "HEAD"]);
        r.commit("main second\n");
        let second = git(&r.checkout, &["rev-parse", "HEAD"]);
        fs::write(nested.join("file"), "nested change\n").unwrap();
        git(&nested, &["commit", "-am", "lane"]);
        let before = git(&nested, &["rev-parse", "HEAD"]);
        // First pick succeeds and moves the shared lane ref; the second conflicts.
        fs::write(r.checkout.join("extra"), "first pick\n").unwrap();
        git(&r.checkout, &["add", "extra"]);
        git(&r.checkout, &["commit", "-m", "independent"]);
        let independent = git(&r.checkout, &["rev-parse", "HEAD"]);
        let output = run_git(
            &FileGit,
            &nested,
            ["cherry-pick", &independent, &first, &second],
            &[],
        )
        .unwrap();
        assert_ne!(output.status, 0);
        assert_ne!(git(&nested, &["rev-parse", "HEAD"]), before);
        let raw = capture_workspace_snapshot(
            &FileGit,
            &nested,
            &CheckoutOwnership::Raw { launch: None },
            &r.temp.path().join("raw.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        assert_eq!(
            raw.head,
            SnapshotHead::Branch {
                name: "refs/heads/lane".into(),
                commit: before.clone()
            }
        );
        assert_eq!(raw.refs["refs/heads/lane"], before);
        let raw_destination = r.temp.path().join("raw-restored");
        restore_workspace_snapshot(&FileGit, &raw, &raw_destination, &clone_base(&raw, None))
            .unwrap();
        assert_eq!(git(&raw_destination, &["rev-parse", "HEAD"]), before);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(snapshot.refs["refs/heads/lane"], before);
        assert_eq!(git(&nested, &["rev-parse", "HEAD"]), before);
        let restored = r.restore(&snapshot, None).join("lane");
        assert_eq!(git(&restored, &["rev-parse", "HEAD"]), before);
        assert_eq!(git(&restored, &["status", "--porcelain"]), "");
    }

    #[test]
    fn raw_unchanged_unpushed_head_with_working_changes_survives() {
        let r = Repo::new();
        git(&r.checkout, &["switch", "-c", "preexisting"]);
        r.commit("preexisting unpublished\n");
        record_launch(&FileGit, &r.checkout, "raw-1").unwrap();
        let launch = LaunchRecord::read(&FileGit, &r.checkout, "raw-1").unwrap();
        fs::write(r.checkout.join("file"), "session edits\n").unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch });
        assert!(snapshot.refs.contains_key("refs/heads/preexisting"));
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
        assert_eq!(git(&restored, &["diff"]), git(&r.checkout, &["diff"]));
    }
    #[test]
    fn ref_move_during_working_capture_retries_the_pinned_cut() {
        struct MovingGit {
            moved: std::sync::atomic::AtomicBool,
        }
        impl GitCommandRunner for MovingGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                let output = SystemGit.run(repository, command)?;
                if command.arguments == [OsString::from("stash"), OsString::from("create")]
                    && !self.moved.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    git(repository, &["commit", "-am", "concurrent commit"]);
                }
                Ok(output)
            }
        }
        let r = Repo::new();
        fs::write(r.checkout.join("file"), "concurrent edits\n").unwrap();
        let runner = MovingGit {
            moved: std::sync::atomic::AtomicBool::new(false),
        };
        let snapshot = capture_workspace_snapshot(
            &runner,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        assert!(runner.moved.load(std::sync::atomic::Ordering::SeqCst));
        assert!(snapshot.working_state.is_none());
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
        assert_eq!(
            fs::read(restored.join("file")).unwrap(),
            b"concurrent edits\n"
        );
        assert_eq!(git(&restored, &["status", "--porcelain"]), "");
    }
    // Hard-won: 2b451f9b4: relative local origins changed meaning at restore.
    #[test]
    fn relative_origin_and_push_paths_restore_from_another_directory() {
        let r = Repo::new();
        git(
            &r.checkout,
            &["remote", "set-url", "origin", "../origin.git"],
        );
        git(
            &r.checkout,
            &["config", "remote.origin.pushurl", "../origin.git"],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(Path::new(snapshot.origin_urls.first().unwrap()).is_absolute());
        assert!(Path::new(&snapshot.push_urls[0]).is_absolute());
        let inside = tempfile::Builder::new()
            .prefix("snapshot-relative-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let destination = PathBuf::from(inside.path().file_name().unwrap()).join("restored");
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &clone_base(&snapshot, None),
        )
        .unwrap();
        assert_eq!(
            git(&destination, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
        git(
            &r.checkout,
            &["config", "remote.origin.pushurl", "../fork.git"],
        );
        git(
            &r.checkout,
            &[
                "config",
                "url.https://host/org/fork.git.insteadOf",
                "../fork.git",
            ],
        );
        let rewritten = r.capture(&CheckoutOwnership::Managed);
        assert!(Path::new(&rewritten.push_urls[0]).is_absolute());
    }
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
            snapshot.origin_urls.first().map(String::as_str),
            Some("http://127.0.0.1:1/unreachable.git")
        );
        assert_eq!(
            snapshot.push_urls,
            vec!["http://127.0.0.1:1/unreachable.git"]
        );
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("token"));
    }
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
    #[test]
    fn snapshot_refuses_tracked_or_unpublished_submodule_changes() {
        let r = Repo::new();
        let sub = Repo::new();
        let nested_sub = Repo::new();
        git(
            &sub.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                nested_sub.origin.to_str().unwrap(),
                "nested",
            ],
        );
        git(&sub.checkout, &["commit", "-am", "nested submodule"]);
        git(&sub.checkout, &["push", "origin", "main"]);
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        let nested = r.checkout.join("sub");
        let clean = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            clean.initialized_submodules,
            vec![SnapshotSubmodule {
                origin_urls: vec![sub.origin.to_str().unwrap().to_owned()],
                push_urls: Vec::new(),
                object_format: "sha1".into(),
                worktree_config_enabled: false,
                url_rules: Vec::new(),
                worktree_url_rules: Vec::new(),

                path: "sub".into(),
                commit: git(&nested, &["rev-parse", "HEAD"]),
                name: "sub".into(),
                url: sub.origin.to_str().unwrap().to_owned(),
            }]
        );
        let restored = r.temp.path().join("clean-restored");
        restore_workspace_snapshot(&FileGit, &clean, &restored, &clone_base(&clean, None)).unwrap();
        assert_eq!(
            fs::read(restored.join("sub/file")).unwrap(),
            fs::read(nested.join("file")).unwrap()
        );
        assert!(
            !restored.join("sub/nested/.git").exists(),
            "uninitialized nested submodule must stay uninitialized"
        );
        git(&nested, &["update-index", "--assume-unchanged", "file"]);
        fs::write(nested.join("file"), "hidden edit\n").unwrap();
        let hidden = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("hidden.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(hidden.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::HiddenTrackedEdits(paths)) if paths.iter().any(|p| p.value == Path::new("sub/file"))));
        git(&nested, &["update-index", "--no-assume-unchanged", "file"]);
        fs::write(nested.join("file"), "dirty tracked\n").unwrap();
        let inventory =
            workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(
            inventory.dirty_submodules,
            vec![scoped(&CheckoutKey::default(), PathBuf::from("sub"))]
        );
        let consent = CaptureConsent {
            untracked: UntrackedConsent::Any,
            operations: OperationConsent::Any,
        };
        let capture = || {
            capture_workspace_snapshot(
                &FileGit,
                &r.checkout,
                &CheckoutOwnership::Managed,
                &r.temp.path().join("snapshot.pack"),
                &consent,
                None,
            )
        };
        let error = capture().unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::DirtySubmodule(path)) if path.value == Path::new("sub"))
        );
        assert!(
            error
                .to_string()
                .contains("commit and push inside the submodule")
        );
        git(&nested, &["commit", "-am", "unpushed submodule"]);
        git(&r.checkout, &["add", "sub"]);
        assert!(matches!(
            capture().unwrap_err().downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::DirtySubmodule(_))
        ));
        git(&nested, &["push", "origin", "HEAD:main"]);
        assert!(
            capture().is_ok(),
            "published checked-out submodule commit is durable"
        );
        git(&r.checkout, &["reset", "HEAD", "sub"]);
        let advanced = capture().unwrap();
        assert!(advanced.working_state.is_none());
        assert!(advanced.has_uncommitted_state);
        git(&r.checkout, &["add", "sub"]);
        let staged = capture().unwrap();
        assert!(staged.has_uncommitted_state);
        assert!(
            staged.working_state.is_none(),
            "Git ignores gitlink-only changes; explicit metadata preserves them"
        );
        let staged_destination = r.temp.path().join("staged-restored");
        restore_workspace_snapshot(
            &FileGit,
            &staged,
            &staged_destination,
            &clone_base(&staged, None),
        )
        .unwrap();
        let expected = git(&nested, &["rev-parse", "HEAD"]);
        assert_eq!(git(&staged_destination, &["rev-parse", ":sub"]), expected);
        assert_eq!(
            git(&staged_destination.join("sub"), &["rev-parse", "HEAD"]),
            expected
        );
        fs::write(nested.join("untracked"), "untracked submodule work").unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("scratch.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items)) if items == &vec![item_path(&r.checkout, "sub/untracked")])
        );
        capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("shown-scratch.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Shown(vec![item_path(&r.checkout, "sub/untracked")]),
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap();
    }
    // Hard-won: 2b451f9b4: clone-created refs blocked valid saved prefixes.
    #[test]
    fn saved_branch_and_tag_prefixes_replace_clone_local_namespace() {
        let r = Repo::new();
        git(&r.checkout, &["tag", "release"]);
        git(&r.checkout, &["push", "origin", "release"]);
        git(&r.checkout, &["tag", "-d", "release"]);
        git(&r.checkout, &["tag", "release/topic"]);
        git(&r.checkout, &["checkout", "--detach"]);
        git(&r.checkout, &["branch", "-D", "main"]);
        git(&r.checkout, &["switch", "-c", "main/topic"]);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["symbolic-ref", "HEAD"]),
            "refs/heads/main/topic"
        );
        assert_eq!(git(&restored, &["tag", "--list"]), "release/topic");
    }
    #[test]
    fn source_override_restores_managed_checkout_without_origin() {
        let r = Repo::new();
        git(&r.checkout, &["remote", "remove", "origin"]);
        r.commit("local-source session\n");
        let snapshot = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            Some(r.origin.to_str().unwrap().to_owned()),
        )
        .unwrap();
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
    }
    #[test]
    fn deleted_origin_branch_recovers_missing_tip_or_parent_by_sha() {
        for child in [false, true] {
            let r = Repo::new();
            git(
                &r.origin,
                &["config", "uploadpack.allowAnySHA1InWant", "true"],
            );
            let origin = url::Url::from_directory_path(&r.origin)
                .unwrap()
                .to_string();
            git(&r.checkout, &["remote", "set-url", "origin", &origin]);
            git(&r.checkout, &["switch", "-c", "feature"]);
            r.commit("published feature\n");
            git(&r.checkout, &["push", "origin", "feature"]);
            if child {
                r.commit("unpublished child\n");
            }
            let snapshot = r.capture(&CheckoutOwnership::Managed);
            assert_eq!(snapshot.pack.is_some(), child);
            git(&r.origin, &["update-ref", "-d", "refs/heads/feature"]);
            let restored = r.restore(&snapshot, None);
            assert_eq!(
                git(&restored, &["rev-parse", "HEAD"]),
                git(&r.checkout, &["rev-parse", "HEAD"])
            );
            assert_eq!(
                git(&restored, &["rev-list", "HEAD"]),
                git(&r.checkout, &["rev-list", "HEAD"])
            );
        }
    }
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
    #[test]
    fn raw_new_worktree_keeps_unchanged_preexisting_branch_head() {
        let r = Repo::new();
        git(&r.checkout, &["switch", "-c", "preexisting"]);
        r.commit("preexisting unpublished branch\n");
        git(&r.checkout, &["switch", "main"]);
        record_launch(&FileGit, &r.checkout, "raw-tree").unwrap();
        let ownership = CheckoutOwnership::Raw {
            launch: LaunchRecord::read(&FileGit, &r.checkout, "raw-tree").unwrap(),
        };
        let nested = r.checkout.join("new-worktree");
        git(
            &r.checkout,
            &["worktree", "add", nested.to_str().unwrap(), "preexisting"],
        );
        fs::write(nested.join("file"), "new lane edits\n").unwrap();
        let snapshot = r.capture(&ownership);
        assert!(snapshot.refs.contains_key("refs/heads/preexisting"));
        assert!(
            matches!(&snapshot.worktrees[0].head, SnapshotHead::Branch { name, commit } if name == "refs/heads/preexisting" && commit == &git(&nested, &["rev-parse", "HEAD"]))
        );
        let restored = r.restore(&snapshot, None).join("new-worktree");
        assert_eq!(
            git(&restored, &["symbolic-ref", "HEAD"]),
            "refs/heads/preexisting"
        );
        assert_eq!(git(&restored, &["diff"]), git(&nested, &["diff"]));
    }
    // Hard-won: 2b451f9b4: a stash added after inventory escaped discard consent.
    #[test]
    fn new_stash_after_inventory_requires_its_own_consent() {
        struct StashingGit {
            added: std::sync::atomic::AtomicBool,
        }
        impl GitCommandRunner for StashingGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                let output = SystemGit.run(repository, command)?;
                if command
                    .arguments
                    .first()
                    .is_some_and(|arg| arg == "for-each-ref")
                    && !self.added.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    fs::write(repository.join("file"), "new tracked stash\n")?;
                    fs::write(repository.join("new-untracked"), "new untracked stash\n")?;
                    git(repository, &["stash", "push", "-u", "-m", "new stash"]);
                }
                Ok(output)
            }
        }
        let r = Repo::new();
        let runner = StashingGit {
            added: std::sync::atomic::AtomicBool::new(false),
        };
        let error = capture_workspace_snapshot(
            &runner,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items)) if matches!(items.as_slice(), [UntrackedItem::Stash { message, .. }] if message.ends_with("new stash")))
        );
        assert_eq!(read_stash(&FileGit, &r.checkout).unwrap().len(), 1);
    }
    #[test]
    fn missing_blob_is_detected_by_full_graph_check() {
        struct MissingBlobGit {
            blob: String,
            removed: std::sync::atomic::AtomicBool,
        }
        impl GitCommandRunner for MissingBlobGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                if command.arguments.iter().any(|arg| arg == "--missing=print")
                    && !self.removed.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    // This fixture's small push is already unpacked into loose objects.
                    let path = git_path(
                        &FileGit,
                        repository,
                        &format!("objects/{}/{}", &self.blob[..2], &self.blob[2..]),
                    )?;
                    fs::remove_file(path)?;
                }
                SystemGit.run(repository, command)
            }
        }
        let r = Repo::new();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let runner = MissingBlobGit {
            blob: git(&r.checkout, &["rev-parse", "HEAD:file"]),
            removed: std::sync::atomic::AtomicBool::new(false),
        };
        let error = restore_workspace_snapshot(
            &runner,
            &snapshot,
            &r.temp.path().join("restored"),
            &clone_base(&snapshot, None),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(&runner.blob), "{error}");
        assert!(
            error.contains("origin may have rewritten history"),
            "{error}"
        );
    }
    #[test]
    fn hidden_assume_unchanged_edit_refuses_before_any_mutation() {
        let r = Repo::new();
        r.conflict();
        // Use an unrelated tracked path so the hidden flag can coexist with
        // a conflict and prove refusal precedes a consented operation abort.
        fs::write(r.checkout.join("hidden"), "committed\n").unwrap();
        git(&r.checkout, &["add", "hidden"]);
        git(&r.checkout, &["commit", "-m", "hidden fixture"]);
        r.start_conflict("merge");
        git(
            &r.checkout,
            &["update-index", "--assume-unchanged", "hidden"],
        );
        fs::write(r.checkout.join("hidden"), "hidden edits\n").unwrap();
        let inventory =
            workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(
            inventory.hidden_tracked_edits,
            vec![scoped(&CheckoutKey::default(), PathBuf::from("hidden"))]
        );
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::HiddenTrackedEdits(paths)) if paths == &vec![scoped(&CheckoutKey::default(), PathBuf::from("hidden"))])
        );
        assert!(r.checkout.join(".git/MERGE_HEAD").exists());
        assert_eq!(
            fs::read(r.checkout.join("hidden")).unwrap(),
            b"hidden edits\n"
        );
    }
    #[test]
    fn raw_detached_rebase_records_original_detached_commit() {
        let r = Repo::new();
        r.conflict();
        let original = git(&r.checkout, &["rev-parse", "HEAD"]);
        git(&r.checkout, &["checkout", "--detach"]);
        r.start_conflict("rebase");
        let transient = git(&r.checkout, &["rev-parse", "HEAD"]);
        assert_ne!(transient, original);
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert_eq!(snapshot.head, SnapshotHead::Detached(original.clone()));
        assert!(snapshot.working_state_skipped);
        assert_eq!(git(&r.checkout, &["rev-parse", "HEAD"]), transient);
        let restored = r.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", "HEAD"]), original);
        assert!(matches!(
            read_head(&FileGit, &restored).unwrap(),
            SnapshotHead::Detached(_)
        ));
    }
    #[test]
    fn shown_merge_identity_does_not_consent_to_a_replacement_merge() {
        let r = Repo::new();
        r.conflict();
        git(&r.checkout, &["switch", "-c", "alternate", "side"]);
        r.commit("alternate side\n");
        git(&r.checkout, &["switch", "main"]);
        r.start_conflict("merge");
        let shown = workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed)
            .unwrap()
            .operation
            .unwrap();
        git(&r.checkout, &["merge", "--abort"]);
        assert_ne!(
            run_git(&FileGit, &r.checkout, ["merge", "alternate"], &[])
                .unwrap()
                .status,
            0
        );
        let before = fs::read(r.checkout.join("file")).unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Shown(vec![shown.clone()]),
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::OperationInProgress(ShownOperation {
                kind: GitOperationKind::Merge,
                ..
            }))
        ));
        assert!(r.checkout.join(".git/MERGE_HEAD").exists());
        assert_eq!(fs::read(r.checkout.join("file")).unwrap(), before);
        for branch in ["other-main", "main"] {
            git(&r.checkout, &["merge", "--abort"]);
            if branch == "other-main" {
                git(&r.checkout, &["switch", "-c", branch]);
            } else {
                git(&r.checkout, &["switch", branch]);
            }
            r.start_conflict("merge");
            let current = workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed)
                .unwrap()
                .operation
                .unwrap();
            assert_ne!(current.identity, shown.identity);
            let error = capture_workspace_snapshot(
                &FileGit,
                &r.checkout,
                &CheckoutOwnership::Managed,
                &r.temp.path().join("occurrence.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Shown(vec![shown.clone()]),
                },
                None,
            )
            .unwrap_err();
            assert!(matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::OperationInProgress(_))
            ));
        }
    }
    #[test]
    fn staged_uninitialized_gitlink_is_kept_without_initializing_it() {
        let r = Repo::new();
        let sub = Repo::new();
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        git(&r.checkout, &["submodule", "deinit", "-f", "sub"]);
        sub.commit("new published gitlink\n");
        git(&sub.checkout, &["push", "origin", "main"]);
        let commit = git(&sub.checkout, &["rev-parse", "HEAD"]);
        git(
            &r.checkout,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{commit},sub"),
            ],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.has_uncommitted_state);
        assert!(snapshot.initialized_submodules.is_empty());
        assert_eq!(
            snapshot.index_gitlinks,
            vec![SnapshotGitlink {
                path: "sub".into(),
                commit: commit.clone()
            }]
        );
        let restored = r.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", ":sub"]), commit);
        assert!(!restored.join("sub/.git").exists());
    }
    #[test]
    fn session_consent_checks_all_repositories_before_aborting_any() {
        let a = Repo::new();
        let b = Repo::new();
        a.conflict();
        a.start_conflict("merge");
        fs::write(a.checkout.join("notes.txt"), "approved only in a").unwrap();
        fs::write(b.checkout.join("notes.txt"), "not approved in b").unwrap();
        let repositories: Vec<_> = [("a", &a), ("b", &b)]
            .into_iter()
            .map(|(id, repo)| RepositoryInput {
                in_container: false,
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
    }
    #[test]
    fn provisioned_restore_never_fetches_past_its_history_boundary() {
        struct NoNetworkGit;
        impl GitCommandRunner for NoNetworkGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                assert!(
                    !command
                        .arguments
                        .iter()
                        .any(|arg| ["clone", "fetch", "ls-remote"]
                            .iter()
                            .any(|verb| arg == verb)),
                    "provisioned restore contacted origin: {:?}",
                    command.arguments
                );
                SystemGit.run(repository, command)
            }
        }
        let r = Repo::new();
        let initial = git(&r.checkout, &["rev-parse", "HEAD"]);
        r.commit("later published history\n");
        git(&r.checkout, &["push", "origin", "main"]);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_none());
        let provisioned = r.temp.path().join("provisioned");
        git(r.temp.path(), &["init", provisioned.to_str().unwrap()]);
        git(
            &provisioned,
            &["remote", "add", "origin", r.origin.to_str().unwrap()],
        );
        git(&provisioned, &["fetch", "origin", &initial]);
        git(&provisioned, &["checkout", "--detach", &initial]);
        let error = restore_workspace_snapshot(
            &NoNetworkGit,
            &snapshot,
            &provisioned,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("origin may have rewritten history"),
            "{error:#}"
        );
        assert_eq!(git(&provisioned, &["rev-parse", "HEAD"]), initial);
    }
    #[test]
    fn submodule_remote_must_match_effective_restore_url_and_redacts_tokens() {
        let r = Repo::new();
        let sub = Repo::new();
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        let nested = r.checkout.join("sub");
        let fork = r.temp.path().join("fork.git");
        git(
            r.temp.path(),
            &[
                "clone",
                "--bare",
                sub.origin.to_str().unwrap(),
                fork.to_str().unwrap(),
            ],
        );
        git(&nested, &["remote", "add", "fork", fork.to_str().unwrap()]);
        fs::write(nested.join("file"), "only on fork\n").unwrap();
        git(&nested, &["commit", "-am", "fork-only"]);
        git(&nested, &["push", "fork", "HEAD:main"]);
        let consent = CaptureConsent {
            untracked: UntrackedConsent::Any,
            operations: OperationConsent::Any,
        };
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &consent,
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::DirtySubmodule(_))
        ));
        git(
            &r.checkout,
            &["config", "submodule.sub.url", fork.to_str().unwrap()],
        );
        assert!(matches!(
            capture_workspace_snapshot(
                &FileGit,
                &r.checkout,
                &CheckoutOwnership::Managed,
                &r.temp.path().join("still-wrong-origin.pack"),
                &consent,
                None
            )
            .unwrap_err()
            .downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::DirtySubmodule(_))
        ));
        git(
            &nested,
            &["remote", "set-url", "origin", fork.to_str().unwrap()],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let restored = r.temp.path().join("fork-restored");
        restore_workspace_snapshot(&FileGit, &snapshot, &restored, &clone_base(&snapshot, None))
            .unwrap();
        assert_eq!(
            git(&restored.join("sub"), &["rev-parse", "HEAD"]),
            git(&nested, &["rev-parse", "HEAD"])
        );
        let credentialed = "http://submodule-token:submodule-secret@127.0.0.1:1/fork.git";
        git(&r.checkout, &["config", "submodule.sub.url", credentialed]);
        git(&nested, &["remote", "set-url", "fork", credentialed]);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            snapshot.initialized_submodules[0].url,
            "http://127.0.0.1:1/fork.git"
        );
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("submodule-token"));
        assert!(!json.contains("submodule-secret"));
    }

    #[test]
    fn raw_untracked_file_added_during_pack_requires_fresh_consent() {
        struct WritingGit(PathBuf);
        impl GitCommandRunner for WritingGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                let output = SystemGit.run(repository, command)?;
                if command
                    .arguments
                    .first()
                    .is_some_and(|argument| argument == "pack-objects")
                {
                    fs::write(self.0.join("new-during-pack"), "new").unwrap();
                }
                Ok(output)
            }
        }
        let r = Repo::new();
        let error = capture_workspace_snapshot(
            &WritingGit(r.checkout.clone()),
            &r.checkout,
            &CheckoutOwnership::Raw { launch: None },
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items)) if items == &vec![item_path(&r.checkout, "new-during-pack")])
        );
    }
    #[test]
    fn cutoff_exclusions_pack_published_session_work_without_origin_fetch() {
        let r = Repo::new();
        let cutoff = git(&r.checkout, &["rev-parse", "HEAD"]);
        r.commit("published beyond cutoff\n");
        git(&r.checkout, &["push", "origin", "main"]);
        let repository = RepositoryInput {
            in_container: false,
            id: "cutoff".into(),
            checkout: r.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: r.temp.path().join("cutoff.pack"),
            exclusions: ExclusionSource::Commits(vec![cutoff.clone()]),
            keep_dir: None,
        };
        let consent = SessionCaptureConsent {
            repositories: BTreeMap::from([(
                "cutoff".into(),
                CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
            )]),
        };
        let estimate = estimate_repository_delta(&FileGit, &repository).unwrap();
        let session = capture_session_snapshot(&FileGit, &[repository], &consent).unwrap();
        let snapshot = &session.repositories["cutoff"];
        assert!(snapshot.pack.is_some());
        assert_eq!(
            estimate.total_bytes,
            Some(fs::metadata(snapshot.pack.as_ref().unwrap()).unwrap().len())
        );
        let destination = r.temp.path().join("cutoff-base");
        git(r.temp.path(), &["init", destination.to_str().unwrap()]);
        git(
            &destination,
            &["fetch", r.origin.to_str().unwrap(), &cutoff],
        );
        git(&destination, &["checkout", "--detach", &cutoff]);
        restore_workspace_snapshot(
            &FileGit,
            snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert_eq!(
            git(&destination, &["rev-parse", "HEAD"]),
            snapshot.head.commit()
        );
        assert_eq!(
            fs::read(destination.join("file")).unwrap(),
            b"published beyond cutoff\n"
        );
    }
    #[test]
    fn linked_managed_capture_does_not_prune_the_users_common_directory() {
        let r = Repo::new();
        git(&r.checkout, &["branch", "deleted"]);
        git(&r.checkout, &["push", "origin", "deleted"]);
        let tracked = git(&r.checkout, &["rev-parse", "refs/remotes/origin/deleted"]);
        git(&r.origin, &["update-ref", "-d", "refs/heads/deleted"]);
        let legacy = r.temp.path().join("legacy");
        git(
            &r.checkout,
            &["worktree", "add", "-b", "session", legacy.to_str().unwrap()],
        );
        fs::write(r.checkout.join("file"), "user stash\n").unwrap();
        git(
            &r.checkout,
            &["stash", "push", "-m", "unrelated user stash"],
        );
        let scope = crate::archive::session_start_scope(&FileGit, &legacy).unwrap();
        let own_pin = crate::archive::push_review_base_ref(&scope);
        git(&legacy, &["update-ref", &own_pin, "HEAD"]);
        git(
            &legacy,
            &["update-ref", "refs/mj/reviewed/unrelated", "HEAD"],
        );
        let snapshot = capture_workspace_snapshot(
            &FileGit,
            &legacy,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("legacy.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        assert!(!snapshot.refs_complete);
        assert!(snapshot.stash.is_empty());
        assert!(snapshot.refs.contains_key(&own_pin));
        assert!(snapshot.refs.contains_key("refs/heads/session"));
        assert!(!snapshot.refs.contains_key("refs/heads/main"));
        assert!(!snapshot.refs.contains_key("refs/mj/reviewed/unrelated"));
        assert_eq!(
            git(&r.checkout, &["rev-parse", "refs/remotes/origin/deleted"]),
            tracked
        );
    }
    #[test]
    fn capture_and_restore_need_no_user_identity_or_populated_home() {
        struct EmptyHomeGit {
            home: PathBuf,
        }
        impl GitCommandRunner for EmptyHomeGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                let mut command = command.clone();
                command.env.extend([
                    ("HOME".into(), self.home.as_os_str().to_owned()),
                    ("XDG_CONFIG_HOME".into(), self.home.as_os_str().to_owned()),
                    ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
                ]);
                SystemGit.run(repository, &command)
            }
        }
        let r = Repo::new();
        fs::write(r.checkout.join("file"), "saved tracked stash\n").unwrap();
        fs::write(r.checkout.join("scratch"), "drop").unwrap();
        git(&r.checkout, &["stash", "push", "-u", "-m", "without login"]);
        git(&r.checkout, &["config", "--unset", "user.name"]);
        git(&r.checkout, &["config", "--unset", "user.email"]);
        fs::write(r.checkout.join("file"), "working tracked edits\n").unwrap();
        let home = r.temp.path().join("empty-home");
        fs::create_dir(&home).unwrap();
        let runner = EmptyHomeGit { home };
        let snapshot = capture_workspace_snapshot(
            &runner,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("identity.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        let destination = r.temp.path().join("identity-restored");
        restore_workspace_snapshot(
            &runner,
            &snapshot,
            &destination,
            &clone_base(&snapshot, None),
        )
        .unwrap();
        assert_eq!(
            fs::read(destination.join("file")).unwrap(),
            b"working tracked edits\n"
        );
        assert_eq!(
            read_stash(&FileGit, &destination).unwrap()[0].message,
            snapshot.stash[0].message
        );
    }
    #[test]
    fn reachable_submodule_force_push_does_not_trust_stale_tracking_refs() {
        let r = Repo::new();
        let sub = Repo::new();
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        let module = r.checkout.join("sub");
        let original = git(&module, &["rev-parse", "HEAD"]);
        fs::write(module.join("file"), "formerly published\n").unwrap();
        git(&module, &["commit", "-am", "removed"]);
        git(&module, &["push", "origin", "HEAD:main"]);
        git(&r.checkout, &["add", "sub"]);
        git(&sub.origin, &["update-ref", "refs/heads/main", &original]);
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("submodule.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::DirtySubmodule(_))
        ));
    }
    // Hard-won: 2b451f9b4: removing third parents collapsed distinct stash entries.
    #[test]
    fn stash_replacements_keep_duplicate_tracked_states() {
        let r = Repo::new();
        for payload in ["first scratch", "second scratch"] {
            fs::write(r.checkout.join("file"), "same tracked state\n").unwrap();
            fs::write(r.checkout.join("scratch"), payload).unwrap();
            let output = SystemGit
                .run(
                    &r.checkout,
                    &GitCommand {
                        arguments: ["stash", "push", "-u", "-m", "same message"]
                            .into_iter()
                            .map(OsString::from)
                            .collect(),
                        env: [
                            ("GIT_AUTHOR_DATE", "@1000000000 +0000"),
                            ("GIT_COMMITTER_DATE", "@1000000000 +0000"),
                        ]
                        .into_iter()
                        .map(|(key, value)| (key.into(), value.into()))
                        .collect(),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(
                output.status,
                0,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let original = read_stash(&FileGit, &r.checkout).unwrap();
        assert_eq!(original.len(), 2);
        assert_ne!(original[0].commit, original[1].commit);
        let rendered = git(&r.checkout, &["stash", "list", "--format=%gs"]);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_ne!(snapshot.stash[0].commit, snapshot.stash[1].commit);
        let restored = r.restore(&snapshot, None);
        assert_eq!(git(&restored, &["stash", "list", "--format=%gs"]), rendered);
        for (saved, source) in snapshot.stash.iter().zip(original) {
            assert_eq!(
                git(&restored, &["show", "-s", "--format=%s", &saved.commit]),
                git(&r.checkout, &["show", "-s", "--format=%s", &source.commit])
            );
            assert!(
                git(&restored, &["show", "-s", "--format=%B", &saved.commit])
                    .contains(&format!("Mjolnir-Source-Stash: {}", source.commit))
            );
        }
    }
    #[test]
    fn shown_path_identity_refuses_a_replacement_file_at_the_same_name() {
        let r = Repo::new();
        let path = r.checkout.join("notes.txt");
        fs::write(&path, "shown").unwrap();
        let shown = item_path(&r.checkout, "notes.txt");
        // Keep the old inode alive, so even aggressive inode reuse cannot make
        // this fixture's replacement accidentally have the same identity.
        fs::rename(&path, r.temp.path().join("old-notes")).unwrap();
        fs::write(&path, "new!!").unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Raw { launch: None },
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Shown(vec![shown.clone()]),
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items)) if items == &vec![item_path(&r.checkout, "notes.txt")] && items[0] != shown)
        );
        assert!(error.to_string().contains("notes.txt"));
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

    // Hard-won: bae1ae4d3: clone tags resurrected a locally deleted managed tag.
    #[test]
    fn complete_and_raw_deleted_refs_do_not_resurrect_origin_tags() {
        let r = Repo::new();
        git(&r.checkout, &["tag", "deleted"]);
        git(&r.checkout, &["push", "origin", "deleted"]);
        record_launch(&FileGit, &r.checkout, "tag-session").unwrap();
        let launch = LaunchRecord::read(&FileGit, &r.checkout, "tag-session").unwrap();
        git(&r.checkout, &["tag", "-d", "deleted"]);
        let raw = r.capture(&CheckoutOwnership::Raw { launch });
        assert!(!raw.refs_complete);
        assert_eq!(raw.deleted_refs, vec!["refs/tags/deleted"]);
        let restored = r.restore(&raw, None);
        assert_eq!(git(&restored, &["tag", "--list", "deleted"]), "");
        fs::remove_dir_all(&restored).unwrap();
        let managed = r.capture(&CheckoutOwnership::Managed);
        assert!(managed.refs_complete);
        let restored = r.restore(&managed, None);
        assert_eq!(git(&restored, &["tag", "--list", "deleted"]), "");
    }

    // Hard-won: bae1ae4d3: worktree restore rejected a branch shared with the primary.
    #[test]
    fn forced_shared_branch_worktree_restores_independent_edits() {
        let r = Repo::new();
        let nested = r.checkout.join("nested");
        git(
            &r.checkout,
            &[
                "worktree",
                "add",
                "--force",
                nested.to_str().unwrap(),
                "main",
            ],
        );
        fs::write(nested.join("file"), "nested unstaged\n").unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored.join("nested"), &["symbolic-ref", "HEAD"]),
            "refs/heads/main"
        );
        assert_eq!(
            fs::read_to_string(restored.join("nested/file")).unwrap(),
            "nested unstaged\n"
        );
        assert_eq!(
            git(
                &restored,
                &["status", "--porcelain", "--untracked-files=no"]
            ),
            ""
        );
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

    // Hard-won: 37050d816: objects-only fetch must override tag pruning before refs are pinned.
    #[test]
    fn objects_only_fetch_preserves_unpublished_tags_under_prune_configuration() {
        let r = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&r, &module);
        for (source, origin, name) in [
            (&r.checkout, &r.origin, "root"),
            (&directory, &module.origin, "module"),
        ] {
            git(source, &["config", "fetch.prune", "true"]);
            git(source, &["config", "fetch.pruneTags", "true"]);
            git(source, &["config", "fetch.recurseSubmodules", "true"]);
            git(source, &["config", "remote.origin.prune", "true"]);
            git(source, &["config", "remote.origin.pruneTags", "true"]);
            git(source, &["tag", "private"]);
            let publisher = r.temp.path().join(format!("publisher-{name}"));
            git(
                r.temp.path(),
                &[
                    "clone",
                    origin.to_str().unwrap(),
                    publisher.to_str().unwrap(),
                ],
            );
            fs::write(publisher.join("advanced"), "advanced origin\n").unwrap();
            git(&publisher, &["add", "advanced"]);
            git(&publisher, &["commit", "-m", "advance origin"]);
            git(&publisher, &["push", "origin", "main"]);
        }
        let refs = read_all_refs(&FileGit, &r.checkout).unwrap();
        let module_refs = read_all_refs(&FileGit, &directory).unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            snapshot.refs["refs/tags/private"],
            refs["refs/tags/private"]
        );
        assert_eq!(read_all_refs(&FileGit, &r.checkout).unwrap(), refs);
        assert_eq!(read_all_refs(&FileGit, &directory).unwrap(), module_refs);
        assert!(!snapshot.remote_refs_stale);
    }

    #[test]
    fn unregistered_gitlinks_require_loss_consent_before_mutation() {
        let r = Repo::new();
        let nested = r.checkout.join("nested-repo");
        git(
            &r.checkout,
            &[
                "clone",
                r.origin.to_str().unwrap(),
                nested.to_str().unwrap(),
            ],
        );
        fs::write(nested.join("scratch"), "nested data\n").unwrap();
        git(&r.checkout, &["add", "nested-repo"]);
        let before = git(&r.checkout, &["write-tree"]);
        let inventory =
            workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(inventory.nested_repositories.len(), 1);
        assert!(inventory.nested_repositories[0].bytes > 12);
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("unregistered.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items))
            if matches!(&items[0], UntrackedItem::NestedRepository { path, .. } if path == Path::new("nested-repo")))
        );
        assert_eq!(git(&r.checkout, &["write-tree"]), before);
        assert!(nested.join("scratch").exists());
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let restored = r.restore(&snapshot, None);
        assert!(!restored.join("nested-repo/scratch").exists());
    }

    #[test]
    fn force_tracked_keep_files_refuse_even_with_discard_consent() {
        let r = Repo::new();
        fs::create_dir_all(r.checkout.join(".mj/keep")).unwrap();
        fs::write(r.checkout.join(".mj/keep/private"), "keep transport only\n").unwrap();
        git(&r.checkout, &["add", "-f", ".mj/keep/private"]);
        let repository = RepositoryInput {
            in_container: false,
            id: "root".into(),
            checkout: r.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: r.temp.path().join("keep.pack"),
            exclusions: ExclusionSource::Origin,
            keep_dir: Some(r.checkout.join(".mj/keep")),
        };
        let consent = SessionCaptureConsent {
            repositories: BTreeMap::from([(
                "root".into(),
                CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
            )]),
        };
        let error = capture_session_snapshot(&FileGit, std::slice::from_ref(&repository), &consent)
            .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::TrackedKeepFiles { keep_dir, paths })
            if keep_dir == &r.checkout.join(".mj/keep") && paths[0].value == Path::new(".mj/keep/private"))
        );
        assert!(r.checkout.join(".mj/keep/private").exists());
        assert!(!r.temp.path().join("keep.pack").exists());
        git(
            &r.checkout,
            &["stash", "push", "-m", "keep was force tracked"],
        );
        assert_eq!(git(&r.checkout, &["ls-files", ".mj/keep"]), "");
        let error = capture_session_snapshot(&FileGit, &[repository], &consent).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::TrackedKeepFiles { .. })
        ));
    }

    // Hard-won: bae1ae4d3: provisioned offline restore still fetched submodules.
    #[test]
    fn provisioned_submodules_require_local_objects_when_origin_fetch_is_disabled() {
        let r = Repo::new();
        let sub = Repo::new();
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let destination = r.temp.path().join("provisioned");
        git(
            r.temp.path(),
            &[
                "clone",
                r.origin.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        let error = restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("locally provisioned repository"),
            "{error:#}"
        );
        git(
            &destination,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "sub",
            ],
        );
        let unreachable = sub.temp.path().join("offline.git");
        fs::rename(&sub.origin, &unreachable).unwrap();
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert_eq!(
            git(&destination.join("sub"), &["rev-parse", "HEAD"]),
            snapshot.initialized_submodules[0].commit
        );
    }
    #[test]
    fn rebase_abort_rechecks_unpublished_restored_submodule_gitlinks() {
        let r = Repo::new();
        let sub = Repo::new();
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "base submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        git(&r.checkout, &["branch", "side"]);
        r.commit("main conflict\n");
        // A later unpublished gitlink commit has not been replayed when rebase
        // stops on the earlier file conflict.
        let nested = r.checkout.join("sub");
        fs::write(nested.join("file"), "unpublished submodule\n").unwrap();
        git(&nested, &["commit", "-am", "unpublished"]);
        let unpublished = git(&nested, &["rev-parse", "HEAD"]);
        git(&r.checkout, &["commit", "-am", "unpublished gitlink"]);
        let original = git(&r.checkout, &["rev-parse", "HEAD"]);
        git(&r.checkout, &["checkout", "side"]);
        let published = git(&r.checkout, &["rev-parse", ":sub"]);
        git(&nested, &["checkout", "--detach", &published]);
        r.commit("side conflict\n");
        git(&r.checkout, &["checkout", "main"]);
        git(&nested, &["checkout", "--detach", &unpublished]);
        assert_ne!(
            run_git(&FileGit, &r.checkout, ["rebase", "side"], &[])
                .unwrap()
                .status,
            0
        );
        // Match the currently rebasing superproject, as users do while resolving.
        let indexed = git(&r.checkout, &["rev-parse", ":sub"]);
        git(&nested, &["checkout", "--detach", &indexed]);
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("abort.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::DirtySubmodule(path))
            if path.value == Path::new("sub")),
            "{error:#}"
        );
        assert_eq!(git(&r.checkout, &["rev-parse", "HEAD"]), original);
        assert_eq!(git(&r.checkout, &["rev-parse", ":sub"]), unpublished);
        assert!(operation(&FileGit, &r.checkout).unwrap().is_none());
    }
    #[test]
    fn uninitialized_relative_submodule_urls_follow_the_default_remote() {
        let r = Repo::new();
        let sub = Repo::new();
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "base submodule"]);
        git(&r.checkout, &["submodule", "deinit", "-f", "sub"]);
        git(
            &r.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.url",
                "../lib.git",
            ],
        );
        git(&r.checkout, &["add", ".gitmodules"]);
        git(
            &r.checkout,
            &["remote", "set-url", "origin", "https://host/org/app.git"],
        );
        assert_eq!(
            resolve_submodule_url(&FileGit, &r.checkout, "../lib.git").unwrap(),
            "https://host/org/lib.git"
        );
        git(
            &r.checkout,
            &["remote", "add", "upstream", "https://upstream/org/app.git"],
        );
        git(&r.checkout, &["config", "branch.main.remote", "upstream"]);
        assert_eq!(
            resolve_submodule_url(&FileGit, &r.checkout, "../lib.git").unwrap(),
            "https://upstream/org/lib.git"
        );
        // Exercise that resolved URL with real Git transport via insteadOf.
        git(
            &r.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", sub.origin.display()),
                "https://upstream/org/lib.git",
            ],
        );
        sub.commit("published next pin\n");
        git(&sub.checkout, &["push", "origin", "main"]);
        let published = git(&sub.checkout, &["rev-parse", "HEAD"]);
        git(
            &r.checkout,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{published},sub"),
            ],
        );
        let inventory =
            workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert!(inventory.dirty_submodules.is_empty());
        assert!(!r.checkout.join("sub/.git").exists());
        sub.commit("unpublished next pin\n");
        let unpublished = git(&sub.checkout, &["rev-parse", "HEAD"]);
        git(
            &r.checkout,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{unpublished},sub"),
            ],
        );
        let inventory =
            workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(inventory.dirty_submodules[0].value, Path::new("sub"));
    }
    #[test]
    fn initialized_submodule_old_published_pin_fetches_only_new_remote_objects() {
        let r = Repo::new();
        let sub = Repo::new();
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "base submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        let nested = r.checkout.join("sub");
        let old = git(&nested, &["rev-parse", "HEAD"]);
        let refs = read_all_refs(&FileGit, &nested).unwrap();
        let fetch_head = git_path(&FileGit, &nested, "FETCH_HEAD").unwrap();
        fs::write(&fetch_head, "submodule sentinel\n").unwrap();
        sub.commit("newer published tip\n");
        git(&sub.checkout, &["push", "origin", "main"]);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(snapshot.initialized_submodules[0].commit, old);
        assert_eq!(read_all_refs(&FileGit, &nested).unwrap(), refs);
        assert_eq!(
            fs::read_to_string(&fetch_head).unwrap(),
            "submodule sentinel\n"
        );
        let destination = r.temp.path().join("restored");
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &clone_base(&snapshot, None),
        )
        .unwrap();
        assert_eq!(git(&destination.join("sub"), &["rev-parse", "HEAD"]), old);
    }
    #[test]
    fn initialized_relative_local_submodule_url_restores_from_another_directory() {
        let r = Repo::new();
        let sub = Repo::new();
        let remotes = r.temp.path().join("remotes");
        fs::create_dir(&remotes).unwrap();
        let super_origin = remotes.join("super.git");
        let lib_origin = remotes.join("lib.git");
        fs::rename(&r.origin, &super_origin).unwrap();
        fs::rename(&sub.origin, &lib_origin).unwrap();
        git(
            &r.checkout,
            &["remote", "set-url", "origin", "../remotes/super.git"],
        );
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                lib_origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(
            &r.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.url",
                "../lib.git",
            ],
        );
        git(&r.checkout, &["commit", "-am", "relative local submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        git(&r.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(r.checkout.join(".git/modules/sub")).unwrap();
        git(&r.checkout, &["submodule", "init", "sub"]);
        assert_eq!(
            git(&r.checkout, &["config", "submodule.sub.url"]),
            "../remotes/lib.git"
        );
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "sub",
            ],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            Path::new(&snapshot.initialized_submodules[0].url),
            lib_origin
        );
        let destination = r.temp.path().join("another-directory/restored");
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &clone_base(&snapshot, None),
        )
        .unwrap();
        assert_eq!(
            fs::read(destination.join("sub/file")).unwrap(),
            b"first\nsecond\nthird\n"
        );
        assert_eq!(
            git(&destination, &["config", "submodule.sub.url"]),
            lib_origin.to_str().unwrap()
        );
    }

    #[test]
    fn bisect_reset_rechecks_files_that_were_ignored_before_abort() {
        let r = Repo::new();
        let good = git(&r.checkout, &["rev-parse", "HEAD"]);
        fs::write(r.checkout.join(".gitignore"), "scratch\n").unwrap();
        git(&r.checkout, &["add", ".gitignore"]);
        git(&r.checkout, &["commit", "-m", "ignore at midpoint"]);
        git(&r.checkout, &["rm", ".gitignore"]);
        git(
            &r.checkout,
            &["commit", "-m", "do not ignore at original head"],
        );
        let original = git(&r.checkout, &["rev-parse", "HEAD"]);
        git(&r.checkout, &["bisect", "start", &original, &good]);
        fs::write(r.checkout.join("scratch"), "only ignored at midpoint\n").unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("bisect.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items))
            if items == &vec![item_path(&r.checkout, "scratch")]),
            "{error:#}"
        );
        assert_eq!(git(&r.checkout, &["rev-parse", "HEAD"]), original);
        assert!(operation(&FileGit, &r.checkout).unwrap().is_none());
        assert!(r.checkout.join("scratch").exists());
    }

    #[test]
    fn source_push_destinations_survive_explicit_and_fallback_configuration() {
        let r = Repo::new();
        let push = Repo::new();
        let push_url = format!("file://{}", push.origin.display());
        git(
            &r.checkout,
            &["remote", "set-url", "origin", "../origin.git"],
        );
        git(
            &r.checkout,
            &[
                "config",
                &format!("url.{push_url}.pushInsteadOf"),
                "../origin.git",
            ],
        );
        let fallback = r.capture(&CheckoutOwnership::Managed);
        let fallback_destination = r.temp.path().join("fallback");
        restore_workspace_snapshot(
            &FileGit,
            &fallback,
            &fallback_destination,
            &clone_base(&fallback, None),
        )
        .unwrap();
        assert_eq!(
            config_values(&FileGit, &fallback_destination, "remote.origin.url")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            config_values(&FileGit, &fallback_destination, "remote.origin.pushurl")
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            git(
                &fallback_destination,
                &["remote", "get-url", "--push", "--all", "origin"]
            ),
            push_url
        );
        git(
            &r.checkout,
            &["config", "remote.origin.pushurl", "push:repo"],
        );
        git(
            &r.checkout,
            &["config", &format!("url.{push_url}.insteadOf"), "push:repo"],
        );
        // Explicit pushurl disables this conflicting pushInsteadOf rule.
        git(
            &r.checkout,
            &["config", "url.missing:repo.pushInsteadOf", "push:repo"],
        );
        let explicit = r.capture(&CheckoutOwnership::Managed);
        let explicit_destination = r.temp.path().join("explicit");
        restore_workspace_snapshot(
            &FileGit,
            &explicit,
            &explicit_destination,
            &clone_base(&explicit, None),
        )
        .unwrap();
        assert_eq!(
            git(
                &explicit_destination,
                &["remote", "get-url", "--push", "--all", "origin"]
            ),
            push_url
        );
    }
    #[test]
    fn sha256_root_and_submodule_restore_with_an_independent_seed() {
        let r = Repo::new_with_format("sha256");
        let sub = Repo::new_with_format("sha256");
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "sha256 submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        r.commit("unpushed sha256 work\n");
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(snapshot.object_format, "sha256");
        assert_eq!(snapshot.initialized_submodules[0].object_format, "sha256");
        let destination = r.temp.path().join("sha256-restored");
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &clone_base(&snapshot, Some(&r.checkout)),
        )
        .unwrap();
        assert_eq!(
            git(&destination, &["rev-parse", "--show-object-format"]),
            "sha256"
        );
        assert_eq!(
            git(
                &destination.join("sub"),
                &["rev-parse", "--show-object-format"]
            ),
            "sha256"
        );
        assert!(!destination.join(".git/objects/info/alternates").exists());
        assert_eq!(
            git(&destination, &["rev-parse", "HEAD"]),
            snapshot.head.commit()
        );
    }
    #[test]
    fn multiple_urls_push_only_to_the_subset_rewritten_by_git() {
        let r = Repo::new();
        let second = Repo::new();
        let first_url = r.origin.to_str().unwrap().to_owned();
        let second_url = second.origin.to_str().unwrap().to_owned();
        git(
            &r.checkout,
            &["config", "--add", "remote.origin.url", "second:repo"],
        );
        git(
            &r.checkout,
            &[
                "config",
                &format!("url.{second_url}.pushInsteadOf"),
                "second:repo",
            ],
        );
        let source = git(
            &r.checkout,
            &["remote", "get-url", "--push", "--all", "origin"],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let destination = r.restore(&snapshot, None);
        assert_eq!(
            config_values(&FileGit, &destination, "remote.origin.url").unwrap(),
            vec![first_url, "second:repo".into()]
        );
        assert_eq!(
            git(
                &destination,
                &["remote", "get-url", "--push", "--all", "origin"]
            ),
            source
        );
        assert!(source.ends_with(&second_url));
    }
    #[test]
    fn explicit_push_and_fetch_apply_their_local_rewrite_once() {
        let r = Repo::new();
        let push = Repo::new();
        let fetch_input = format!("file://{}", push.origin.display());
        let fetch_effective = format!("file://{}", r.origin.display());
        git(&r.checkout, &["remote", "set-url", "origin", &fetch_input]);
        git(
            &r.checkout,
            &[
                "config",
                &format!("url.{fetch_effective}.insteadOf"),
                &fetch_input,
            ],
        );
        git(
            &r.checkout,
            &["config", "remote.origin.pushurl", "push-alias:repo"],
        );
        git(
            &r.checkout,
            &[
                "config",
                &format!("url.{fetch_input}.insteadOf"),
                "push-alias:repo",
            ],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["remote", "get-url", "origin"]),
            fetch_effective
        );
        assert_eq!(
            git(
                &restored,
                &["remote", "get-url", "--push", "--all", "origin"]
            ),
            fetch_input
        );
    }
    #[test]
    fn local_included_rules_travel_with_relative_bases_and_credential_notices() {
        let r = Repo::new();
        fs::create_dir(r.temp.path().join("mirrors")).unwrap();
        let mirror = r.temp.path().join("mirrors/origin.git");
        fs::rename(&r.origin, &mirror).unwrap();
        let include = r.temp.path().join("source-rules.inc");
        git(
            &r.checkout,
            &["config", "include.path", include.to_str().unwrap()],
        );
        for (name, value) in [
            ("url.../mirrors/.insteadOf", "corp:"),
            ("url.https://user:KEY_TOKEN@host/.insteadOf", "unused-key:"),
            ("url.https://safe/.insteadOf", "https://user:VALUE_TOKEN@"),
            ("url.https://USER_TOKEN@host/.insteadOf", "unused-user:"),
            ("url.ssh://git@host/.insteadOf", "https://host/"),
        ] {
            git(
                r.temp.path(),
                &[
                    "config",
                    "--file",
                    include.to_str().unwrap(),
                    "--add",
                    name,
                    value,
                ],
            );
        }
        git(
            &r.checkout,
            &["remote", "set-url", "origin", "corp:origin.git"],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(
            snapshot
                .url_rules
                .iter()
                .any(|rule| rule.name == "url.ssh://git@host/.insteadof")
        );
        assert!(snapshot.url_rules.iter().any(|rule| {
            rule.name
                .contains(mirror.parent().unwrap().to_str().unwrap())
        }));
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.contains("omitted credential-bearing URL rule"))
        );
        let serialized = serde_json::to_string(&snapshot).unwrap();
        for secret in ["KEY_TOKEN", "VALUE_TOKEN", "USER_TOKEN"] {
            assert!(!serialized.contains(secret));
        }
        let destination = r.temp.path().join("another/depth/restored");
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &clone_base(&snapshot, None),
        )
        .unwrap();
        assert_eq!(
            git(&destination, &["remote", "get-url", "origin"]),
            mirror.to_str().unwrap()
        );
        // Host includes remain host-owned; only repository-local includes are flattened.
        let include = FileGit
            .run(
                &destination,
                &GitCommand {
                    arguments: ["config", "--local", "--get-all", "include.path"]
                        .into_iter()
                        .map(OsString::from)
                        .collect(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(include.status, 1);
        assert!(include.stdout.is_empty());
        git(&destination, &["fetch", "origin"]);
    }

    #[test]
    fn configured_chained_rewrites_apply_once_after_restore() {
        let r = Repo::new();
        let effective = format!("file://{}", r.origin.display());
        let missing = format!("file://{}/missing.git", r.temp.path().display());
        git(&r.checkout, &["remote", "set-url", "origin", "source:repo"]);
        git(
            &r.checkout,
            &[
                "config",
                &format!("url.{effective}.insteadOf"),
                "source:repo",
            ],
        );
        git(
            &r.checkout,
            &["config", &format!("url.{missing}.insteadOf"), &effective],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(snapshot.origin_urls, vec!["source:repo"]);
        let restored = r.restore(&snapshot, None);
        assert_eq!(git(&restored, &["remote", "get-url", "origin"]), effective);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            snapshot.head.commit()
        );
    }

    #[test]
    fn initialized_submodule_restores_its_own_origin_rules_and_pushurl() {
        let r = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&r, &module);
        git(
            &r.checkout,
            &["config", "submodule.sub.url", "parent:unused"],
        );
        git(
            &r.checkout,
            &["config", "url./missing-parent/.insteadOf", "corp:"],
        );
        git(
            &directory,
            &["remote", "set-url", "origin", "module:origin"],
        );
        git(
            &directory,
            &[
                "config",
                &format!("url.{}.insteadOf", module.origin.display()),
                "module:origin",
            ],
        );
        git(
            &directory,
            &["config", "remote.origin.pushurl", "corp:target"],
        );
        let host = host_git(r.temp.path(), "empty-host");
        let snapshot = capture_workspace_snapshot(
            &host,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("module-own.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        let restored = r.temp.path().join("own-scope-restored");
        restore_workspace_snapshot(&host, &snapshot, &restored, &clone_base(&snapshot, None))
            .unwrap();
        assert_eq!(
            git(&restored.join("sub"), &["remote", "get-url", "origin"]),
            module.origin.to_str().unwrap()
        );
        assert_eq!(
            git(
                &restored.join("sub"),
                &["remote", "get-url", "--push", "origin"]
            ),
            "corp:target"
        );
        assert_eq!(
            git(&restored, &["config", "submodule.sub.url"]),
            "parent:unused"
        );
        assert!(git(&restored, &["submodule", "status"]).starts_with(' '));
        assert_eq!(
            git(
                &restored.join("sub"),
                &["symbolic-ref", "refs/remotes/origin/HEAD"]
            ),
            "refs/remotes/origin/main"
        );
    }

    #[test]
    fn scoped_rules_and_push_evidence_stay_with_each_linked_checkout() {
        let r = Repo::new();
        git(
            &r.checkout,
            &["config", "extensions.worktreeConfig", "true"],
        );
        git(
            &r.checkout,
            &[
                "config",
                "--worktree",
                "url.https://root/.insteadOf",
                "scope:",
            ],
        );
        let push_one = Repo::new();
        let push_two = Repo::new();
        for (name, target) in [("one", &push_one.origin), ("two", &push_two.origin)] {
            let path = r.checkout.join(name);
            git(
                &r.checkout,
                &["worktree", "add", "--detach", path.to_str().unwrap()],
            );
            // Git copies the primary config.worktree when creating a worktree.
            git(
                &path,
                &[
                    "config",
                    "--worktree",
                    "--unset-all",
                    "url.https://root/.insteadOf",
                ],
            );
            git(
                &path,
                &[
                    "config",
                    "--worktree",
                    &format!("url.https://{name}/.insteadOf"),
                    "scope:",
                ],
            );
            git(
                &path,
                &[
                    "config",
                    "--worktree",
                    &format!("url.{}.pushInsteadOf", target.display()),
                    r.origin.to_str().unwrap(),
                ],
            );
        }
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let restored = r.restore(&snapshot, None);
        for (name, target) in [("one", &push_one.origin), ("two", &push_two.origin)] {
            assert_eq!(
                git(
                    &restored.join(name),
                    &["ls-remote", "--get-url", "scope:repo"]
                ),
                format!("https://{name}/repo")
            );
            let saved = snapshot
                .worktrees
                .iter()
                .find(|tree| tree.relative_path == Path::new(name))
                .unwrap();
            assert_eq!(
                saved.push_evidence,
                PushEvidence::Resolved(vec![target.to_str().unwrap().to_owned()])
            );
        }
        assert_eq!(
            git(&restored, &["ls-remote", "--get-url", "scope:repo"]),
            "https://root/repo"
        );
        let input = RepositoryInput {
            in_container: true,
            id: "container".into(),
            checkout: r.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: r.temp.path().join("container.pack"),
            exclusions: ExclusionSource::Origin,
            keep_dir: None,
        };
        let captured = capture_session_snapshot(
            &FileGit,
            &[input],
            &SessionCaptureConsent {
                repositories: BTreeMap::from([(
                    "container".into(),
                    CaptureConsent {
                        untracked: UntrackedConsent::Any,
                        operations: OperationConsent::Any,
                    },
                )]),
            },
        )
        .unwrap();
        let saved = &captured.repositories["container"];
        assert!(
            matches!(&saved.push_evidence, PushEvidence::Unavailable { reason } if reason == "container-local destination")
        );
        assert!(saved.worktrees.iter().all(|tree| matches!(&tree.push_evidence, PushEvidence::Unavailable { reason } if reason == "container-local destination")));
    }

    #[test]
    fn relative_fetch_push_and_rule_prefixes_keep_their_targets_elsewhere() {
        let r = Repo::new();
        fs::create_dir(r.temp.path().join("push")).unwrap();
        let push = r.temp.path().join("push/origin.git");
        git(
            r.temp.path(),
            &[
                "clone",
                "--bare",
                r.origin.to_str().unwrap(),
                push.to_str().unwrap(),
            ],
        );
        let old = git(&r.origin, &["rev-parse", "main"]);
        git(
            &r.checkout,
            &["remote", "set-url", "origin", "../origin.git"],
        );
        git(
            &r.checkout,
            &["config", "url.../push/.pushInsteadOf", "../"],
        );
        r.commit("publication target\n");
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let restored = r.temp.path().join("deep/another/restored");
        restore_workspace_snapshot(&FileGit, &snapshot, &restored, &clone_base(&snapshot, None))
            .unwrap();
        git(&restored, &["push", "origin", "main"]);
        assert_eq!(git(&push, &["rev-parse", "main"]), snapshot.head.commit());
        assert_eq!(git(&r.origin, &["rev-parse", "main"]), old);
    }

    #[test]
    fn offline_origin_evidence_follows_refspecs_and_refuses_shared_namespaces() {
        let r = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&r, &module);
        for (path, namespace) in [(&r.checkout, "root"), (&directory, "module")] {
            git(
                path,
                &[
                    "config",
                    "remote.origin.fetch",
                    &format!("+refs/heads/*:refs/cache/{namespace}/*"),
                ],
            );
            git(path, &["fetch", "origin"]);
        }
        let root_offline = r.temp.path().join("root-offline.git");
        let module_offline = module.temp.path().join("module-offline.git");
        fs::rename(&r.origin, &root_offline).unwrap();
        fs::rename(&module.origin, &module_offline).unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.remote_refs_stale);
        assert!(snapshot.pack.is_none());
        git(
            &directory,
            &["remote", "add", "fork", module.origin.to_str().unwrap()],
        );
        git(
            &directory,
            &[
                "config",
                "remote.fork.fetch",
                "+refs/heads/*:refs/cache/module/*",
            ],
        );
        let consent = CaptureConsent {
            untracked: UntrackedConsent::Any,
            operations: OperationConsent::Any,
        };
        let capture = || {
            capture_workspace_snapshot(
                &FileGit,
                &r.checkout,
                &CheckoutOwnership::Managed,
                &r.temp.path().join("ambiguous.pack"),
                &consent,
                None,
            )
        };
        assert!(format!("{:#}", capture().unwrap_err()).contains("ambiguous"));
        git(&directory, &["remote", "remove", "fork"]);
        // Removing fork deletes its mapped namespace, so restore origin's cache.
        fs::rename(&module_offline, &module.origin).unwrap();
        git(&directory, &["fetch", "origin"]);
        fs::rename(&module.origin, &module_offline).unwrap();
        git(
            &r.checkout,
            &["remote", "add", "fork", r.origin.to_str().unwrap()],
        );
        git(
            &r.checkout,
            &[
                "config",
                "remote.fork.fetch",
                "+refs/heads/*:refs/cache/root/*",
            ],
        );
        fs::rename(&module_offline, &module.origin).unwrap();
        let full = capture().unwrap();
        assert!(!full.remote_refs_stale);
        assert!(full.pack.is_some());
        assert!(
            full.notices
                .iter()
                .any(|notice| notice.contains("full history") && notice.contains("ambiguous"))
        );
        fs::rename(&root_offline, &r.origin).unwrap();
        let restored = r.temp.path().join("offline-restored");
        restore_workspace_snapshot(&FileGit, &snapshot, &restored, &clone_base(&snapshot, None))
            .unwrap();
    }

    #[test]
    fn push_evidence_redacts_host_rewrites_and_marks_unsafe_or_missing_context() {
        let r = Repo::new();
        let host = host_git(r.temp.path(), "push-evidence-global");
        let capture = || {
            capture_workspace_snapshot(
                &host,
                &r.checkout,
                &CheckoutOwnership::Managed,
                &r.temp.path().join("push-evidence.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap()
        };
        for (base, expected) in [
            (
                "https://USER_TOKEN:PASSWORD@push.example/repo",
                "https://push.example/repo",
            ),
            (
                "ssh://git:SSH_PASSWORD@push.example/repo",
                "ssh://git@push.example/repo",
            ),
            ("git@push.example:repo", "git@push.example:repo"),
        ] {
            fs::write(&host.global, "").unwrap();
            git(
                r.temp.path(),
                &[
                    "config",
                    "--file",
                    host.global.to_str().unwrap(),
                    &format!("url.{base}.pushInsteadOf"),
                    r.origin.to_str().unwrap(),
                ],
            );
            let snapshot = capture();
            assert_eq!(
                snapshot.push_evidence,
                PushEvidence::Resolved(vec![expected.into()])
            );
            let json = serde_json::to_string(&snapshot).unwrap();
            for token in ["USER_TOKEN", "PASSWORD", "SSH_PASSWORD"] {
                assert!(!json.contains(token));
            }
        }
        fs::write(&host.global, "").unwrap();
        git(
            r.temp.path(),
            &[
                "config",
                "--file",
                host.global.to_str().unwrap(),
                "url.https://user:BAD_TOKEN@.pushInsteadOf",
                r.origin.to_str().unwrap(),
            ],
        );
        assert!(matches!(
            capture().push_evidence,
            PushEvidence::Unavailable { .. }
        ));
        git(&r.checkout, &["remote", "remove", "origin"]);
        let snapshot = capture_workspace_snapshot(
            &host,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("no-named-origin.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            Some(r.origin.to_str().unwrap().to_owned()),
        )
        .unwrap();
        assert!(matches!(
            snapshot.push_evidence,
            PushEvidence::Unavailable { .. }
        ));
    }

    #[test]
    fn staged_submodule_tag_ancestor_and_missing_remote_head_restore() {
        let r = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&r, &module);
        let actual = git(&directory, &["rev-parse", "HEAD"]);
        git(&module.checkout, &["checkout", "--orphan", "tag-history"]);
        git(&module.checkout, &["commit", "-am", "separate history"]);
        let ancestor = git(&module.checkout, &["rev-parse", "HEAD"]);
        module.commit("tagged successor\n");
        git(&module.checkout, &["tag", "published-only"]);
        git(&module.checkout, &["push", "origin", "published-only"]);
        git(&directory, &["fetch", "--tags", "origin"]);
        git(
            &r.checkout,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{ancestor},sub"),
            ],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.has_uncommitted_state);
        git(
            &module.origin,
            &["symbolic-ref", "HEAD", "refs/heads/deleted-default"],
        );
        let restored = r.temp.path().join("tag-ancestor-restored");
        restore_workspace_snapshot(&FileGit, &snapshot, &restored, &clone_base(&snapshot, None))
            .unwrap();
        assert_eq!(git(&restored, &["rev-parse", ":sub"]), ancestor);
        assert_eq!(git(&restored.join("sub"), &["rev-parse", "HEAD"]), actual);
        assert_eq!(
            git(&restored.join("sub"), &["tag", "--list", "published-only"]),
            "published-only"
        );
        assert!(!git(&restored, &["submodule", "status"]).starts_with('-'));
        git(
            &module.origin,
            &["update-ref", "-d", "refs/tags/published-only"],
        );
        let recovered = r.temp.path().join("sha-recovered");
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &recovered,
            &clone_base(&snapshot, None),
        )
        .unwrap();
        git(&recovered.join("sub"), &["cat-file", "-e", &ancestor]);
    }
}
