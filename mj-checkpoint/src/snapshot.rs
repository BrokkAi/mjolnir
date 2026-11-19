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
mod relative_path_serde {
    use super::*;
    use serde::{Deserializer, Serializer};
    pub fn serialize<S: Serializer>(
        path: &Path,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer
            .serialize_str(&crate::archive::slash_path(path).map_err(serde::ser::Error::custom)?)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<PathBuf, D::Error> {
        let text = String::deserialize(deserializer)?;
        let path = PathBuf::from(&text);
        let normalized = crate::archive::slash_path(&path).map_err(serde::de::Error::custom)?;
        if normalized != text {
            return Err(serde::de::Error::custom(
                "snapshot path is not slash-separated",
            ));
        }
        Ok(path)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotSubmodule {
    pub origin_urls: Vec<String>,
    pub push_urls: Vec<String>,
    pub object_format: String,
    pub worktree_config_enabled: bool,
    pub url_rules: Vec<SnapshotConfigEntry>,
    pub worktree_url_rules: Vec<SnapshotConfigEntry>,
    #[serde(with = "relative_path_serde")]
    pub path: PathBuf,
    pub commit: String,
    pub name: String,
    pub url: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotGitlink {
    #[serde(with = "relative_path_serde")]
    pub path: PathBuf,
    pub commit: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotWorktree {
    pub push_evidence: PushEvidence,
    pub has_uncommitted_state: bool,
    pub url_rules: Vec<SnapshotConfigEntry>,
    #[serde(with = "relative_path_serde")]
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
    UnpublishedSubmoduleWork(Vec<Scoped<String>>),
    HiddenTrackedEdits(Vec<Scoped<PathBuf>>),
    IntentToAdd(Vec<Scoped<PathBuf>>),
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
                "{} tracked keep files would enter the snapshot; run git rm -r --cached {} and commit any removal from HEAD before suspending",
                paths.len(),
                keep_dir.display()
            ),
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
            Self::UnpublishedSubmoduleWork(items) => write!(
                f,
                "{} would be lost; push the work, or delete the ref or drop the stash, then suspend again",
                items
                    .iter()
                    .map(|item| format!(
                        "{}:{}:{}",
                        item.checkout.repository_id,
                        item.checkout.checkout_path.display(),
                        item.value
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::DirtySubmodule(path) => write!(
                f,
                "submodule {}:{}:{} has tracked changes or an unpublished checkout; commit and push inside the submodule, or discard the change; if uninitialized, run `git submodule update --init {}` or unstage the pointer change, then suspend again",
                path.checkout.repository_id,
                path.checkout.checkout_path.display(),
                path.value.display(),
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
    pub submodule_local_work: Vec<Scoped<String>>,
    pub hidden_tracked_edits: Vec<Scoped<PathBuf>>,
    pub intent_to_add: Vec<Scoped<PathBuf>>,
    pub initialized_submodules: Vec<Scoped<SnapshotSubmodule>>,
    pub index_gitlinks: Vec<Scoped<SnapshotGitlink>>,
    pub operation: Option<GitOperation>,
    pub raw: bool,
    pub remote_refs_stale: bool,
    pub worktrees: Vec<(PathBuf, WorkspaceInventory)>,
}

/// Disable maintenance at the capture boundary: lazy promisor fetches started
/// by rev-list/pack-objects inherit these -c settings through Git's child config.
struct CaptureGit<'a> {
    inner: &'a dyn GitCommandRunner,
}
impl GitCommandRunner for CaptureGit<'_> {
    fn run(&self, repository: &Path, command: &GitCommand) -> Result<crate::archive::GitOutput> {
        let mut command = command.clone();
        let mut arguments = vec![
            "-c".into(),
            "gc.auto=0".into(),
            "-c".into(),
            "maintenance.auto=false".into(),
        ];
        arguments.extend(command.arguments);
        command.arguments = arguments;
        // Inspection must not refresh or lock a user's real index.
        command.env.push(("GIT_OPTIONAL_LOCKS".into(), "0".into()));
        self.inner.run(repository, &command)
    }
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
    let mut entries = std::str::from_utf8(&bytes)?
        .lines()
        .map(|line| {
            let (commit, message) = line.split_once('\0').context("malformed stash entry")?;
            Ok(SnapshotStashEntry {
                commit: commit.to_owned(),
                message: message.to_owned(),
            })
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
        if entries.first().is_none_or(|entry| entry.commit != commit) {
            entries.insert(
                0,
                SnapshotStashEntry {
                    commit: commit.to_owned(),
                    message: git_text(runner, checkout, ["show", "-s", "--format=%s", commit])?,
                },
            );
        }
    }
    Ok(entries)
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
    let mut invisible = std::collections::BTreeSet::new();
    for path in git_bytes(
        runner,
        checkout,
        [
            "diff",
            "--cached",
            "--name-only",
            "--no-renames",
            "--no-ext-diff",
            "--ignore-submodules=all",
            "--ita-invisible-in-index",
            "-z",
        ],
        &[],
        "inspect intent-to-add index entries",
    )?
    .split(|byte| *byte == 0)
    .filter(|path| !path.is_empty())
    {
        invisible.insert(path.to_vec());
    }
    git_bytes(
        runner,
        checkout,
        [
            "diff",
            "--cached",
            "--name-only",
            "--no-renames",
            "--no-ext-diff",
            "--ignore-submodules=all",
            "--ita-visible-in-index",
            "-z",
        ],
        &[],
        "inspect intent-to-add index entries",
    )?
    .split(|byte| *byte == 0)
    .filter(|path| !path.is_empty() && !invisible.contains(*path))
    .map(mj_core::path_input::from_git_bytes)
    .collect()
}

fn working_state(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    skipped: bool,
) -> Result<Option<String>> {
    if skipped {
        return Ok(None);
    }
    // stash create refreshes and writes its index even with optional locks off.
    // Give it a private copy beside the real index (also preserving split-index
    // lookup), so estimation and raw capture never write or lock the live index.
    let index = git_path(runner, checkout, "index")?;
    let temporary =
        tempfile::NamedTempFile::new_in(index.parent().context("index has no parent")?)?;
    if index.try_exists()? {
        fs::copy(&index, temporary.path())?;
    } else {
        fs::remove_file(temporary.path())?;
    }
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
        let (_, address) = transport_url(value);
        if address.is_empty() || (address.contains("://") && url::Url::parse(address).is_err()) {
            return Ok(unavailable(
                "a push destination could not be safely redacted",
            ));
        }
        if !address.contains("://")
            && !relative_local_remote(address)
            && !Path::new(address).is_absolute()
        {
            let Some((host, _)) = address.split_once(':') else {
                return Ok(unavailable(
                    "an opaque push destination could not be safely redacted",
                ));
            };
            if host.is_empty()
                || address
                    .find('@')
                    .is_some_and(|at| address[..at].contains(':'))
            {
                return Ok(unavailable(
                    "an ambiguous push destination could not be safely redacted",
                ));
            }
        }
        let stored = stored_remote(runner, checkout, value)?;
        let (_, stored_address) = transport_url(&stored);
        if in_container
            && (Path::new(stored_address).is_absolute()
                || url::Url::parse(stored_address).is_ok_and(|url| url.scheme() == "file"))
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
    // Preserve URL text for prefix rewrites; filesystem canonicalization can
    // move a symlinked destination outside its configured pushInsteadOf prefix.
    let mut stored = std::path::absolute(checkout.join(value))?
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
    let (_, value) = transport_url(value);
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
pub(crate) fn worktree_remote_notice(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<Option<String>> {
    if worktree_config_enabled(runner, checkout)?
        && !read_config_entries(runner, checkout, true, r"^remote\.origin\.")?.is_empty()
    {
        Ok(Some("worktree-specific remote settings are not restored; every restored worktree uses the primary checkout's origin".to_owned()))
    } else {
        Ok(None)
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
/// Resolve .gitmodules URLs against the only remote restore recreates: origin.
pub(crate) fn resolve_submodule_url(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    url: &str,
) -> Result<String> {
    if !url.starts_with("./") && !url.starts_with("../") {
        return absolute_config_url(checkout, url);
    }
    let base = config_values(runner, checkout, "remote.origin.url")?;
    match base.first() {
        Some(base) => relative_submodule_url(base, url, checkout),
        None => resolve_remote_path(checkout, url),
    }
}
/// Git treats the superproject URL as a directory when resolving ../sub.git.
fn relative_submodule_url(base: &str, relative: &str, checkout: &Path) -> Result<String> {
    if let Ok(mut base) = url::Url::parse(base)
        && !base.cannot_be_a_base()
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
    // Nested worktrees travel even if they existed at launch, by explicit policy.
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
    let mut intent = Vec::new();
    let mut dirty_submodules = Vec::new();
    let mut local_work = Vec::new();
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
            if !(inventory.raw && inventory.operation.is_some()) {
                hidden.extend(inventory.hidden_tracked_edits.clone());
                intent.extend(inventory.intent_to_add.clone());
            }
            dirty_submodules.extend(inventory.dirty_submodules.clone());
            local_work.extend(inventory.submodule_local_work.clone());
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
    ensure!(intent.is_empty(), SnapshotRefused::IntentToAdd(intent));
    ensure!(
        hidden.is_empty(),
        SnapshotRefused::HiddenTrackedEdits(hidden)
    );
    if !inventory.raw && !local_work.is_empty() {
        return Err(SnapshotRefused::UnpublishedSubmoduleWork(local_work).into());
    }
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
                destination.starts_with("refs/remotes/")
                    && (source.starts_with("refs/heads/") || source.starts_with("refs/tags/"))
                    && !negatives
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
    let complete = !legacy && launch.is_none();
    let mut refs: BTreeMap<_, _> = cut
        .refs
        .iter()
        .filter(|(name, oid)| {
            if !local_snapshot_ref(name) {
                return false;
            }
            if legacy {
                return false;
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
    // A snapshot has one pack artifact; ignore repository-configured splitting.
    let output = runner.run(
        checkout,
        &GitCommand {
            arguments: vec![
                "-c".into(),
                "pack.packSizeLimit=0".into(),
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
    let capture = CaptureGit { inner: runner };
    let runner: &dyn GitCommandRunner = &capture;
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
    let inventory =
        workspace_inventory_scoped(runner, checkout, ownership, &CheckoutKey::default())?;
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
    let capture = CaptureGit { inner: runner };
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
        let in_place = matches!(repository.ownership, CheckoutOwnership::Raw { .. })
            && repository.origin_override.is_none()
            && run_git(
                runner,
                &repository.checkout,
                ["remote", "get-url", "origin"],
                &[],
            )?
            .status
                != 0;
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
    for repository in repositories {
        if let CaptureMode::Transfer(inventory) = &inventories[&repository.id] {
            check_repository_consent(
                repository,
                inventory,
                consent.repositories.get(&repository.id).unwrap_or(&none),
            )
            .map_err(|error| session_error(&repository.id, error))?;
        }
    }
    let mut snapshots = BTreeMap::new();
    for repository in repositories {
        if matches!(inventories[&repository.id], CaptureMode::InPlaceOnly) {
            snapshots.insert(
                repository.id.clone(),
                capture_in_place_metadata(runner, repository)?,
            );
            continue;
        }
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
        if matches!(inventories[&repository.id], CaptureMode::InPlaceOnly) {
            continue;
        }
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
fn capture_in_place_metadata(
    runner: &dyn GitCommandRunner,
    repository: &RepositoryInput,
) -> Result<WorkspaceSnapshot> {
    let checkout = &repository.checkout;
    let skipped = operation(runner, checkout)?.is_some();
    let mut notices = vec!["raw checkout has no origin; this snapshot resumes only in place and keeps files, index and operations unchanged".to_owned()];
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
    let hidden = crate::archive::git::hidden_tracked_paths(runner, checkout)?;
    Ok(WorkspaceSnapshot {
        object_format: git_text(runner, checkout, ["rev-parse", "--show-object-format"])?,
        worktree_config_enabled: worktree_config_enabled(runner, checkout)?,
        origin_urls: Vec::new(),
        push_urls: Vec::new(),
        push_evidence: PushEvidence::Unavailable {
            reason: "raw snapshot resumes only in place without origin".to_owned(),
        },
        url_rules: Vec::new(),
        worktree_url_rules: Vec::new(),
        notices,
        head: saved_head(runner, checkout, skipped)?,
        refs: BTreeMap::new(),
        refs_complete: false,
        deleted_refs: Vec::new(),
        branch_tracking: BTreeMap::new(),
        stash: Vec::new(),
        working_state: None,
        working_state_skipped: skipped,
        has_uncommitted_state: skipped || dirty || !hidden.is_empty(),
        worktrees: Vec::new(),
        initialized_submodules: Vec::new(),
        index_gitlinks: Vec::new(),
        remote_refs_stale: false,
        pack: None,
    })
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
    if matches!(ownership, CheckoutOwnership::Managed)
        && !owns_git_directory(runner, checkout, ownership)?
    {
        notices.push("legacy linked-worktree snapshot omits durable review refs; resume starts with a fresh review baseline".to_owned());
    }
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
        return capture_in_place_metadata(runner, repository);
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
        stable &= same_checkout_cut(&inventory, &repository_inventory(runner, repository)?);
        if !stable {
            if attempt < 2 {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            anyhow::bail!(
                "workspace refs, HEAD or submodules kept changing during snapshot capture; retry after the checkout is idle"
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
        let mut tracking = branch_tracking(runner, checkout, &refs)?;
        let dropped = tracking
            .iter()
            .filter(|(_, tracking)| tracking.remote.as_deref() != Some("origin"))
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        if !dropped.is_empty() {
            notices.push(format!(
                "upstreams for branches {} were omitted because restore recreates only origin",
                dropped.join(", ")
            ));
        }
        tracking.retain(|_, tracking| tracking.remote.as_deref() == Some("origin"));
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
            branch_tracking: tracking,
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
            nested.keep_dir = repository.keep_dir.clone();
            nested.tracked_keep_files = tracked_keep_paths(
                runner,
                &repository.checkout.join(path.as_path()),
                &nested.checkout,
                &keep,
                None,
            )?;
            nested.tracked_keep_files.extend(tracked_keep_paths(
                runner,
                &repository.checkout.join(path.as_path()),
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
    Ok(inventory)
}
pub fn workspace_inventory(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
) -> Result<WorkspaceInventory> {
    let capture = CaptureGit { inner: runner };
    let runner: &dyn GitCommandRunner = &capture;
    workspace_inventory_scoped(runner, checkout, ownership, &CheckoutKey::default())
}
pub fn session_inventory(
    runner: &dyn GitCommandRunner,
    repositories: &[RepositoryInput],
) -> Result<BTreeMap<String, WorkspaceInventory>> {
    let capture = CaptureGit { inner: runner };
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
        let mut submodules = crate::checkpoint::inspect_snapshot_submodules(runner, checkout)?;
        if let Some(notice) = worktree_remote_notice(runner, checkout)? {
            submodules.notices.push(notice);
        }
        if raw && !submodules.local_work.is_empty() {
            submodules.notices.push(format!("{}; submodule local branches and stashes do not travel with this snapshot; the raw checkout is kept", submodules.local_work.join(", ")));
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
            submodule_local_work: submodules
                .local_work
                .into_iter()
                .map(|work| scoped(key, work))
                .collect(),
            intent_to_add: intent,
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
fn same_checkout_cut(left: &WorkspaceInventory, right: &WorkspaceInventory) -> bool {
    left.operation == right.operation
        && left.index_gitlinks == right.index_gitlinks
        && left.initialized_submodules == right.initialized_submodules
        && left.submodule_local_work == right.submodule_local_work
        && left.intent_to_add == right.intent_to_add
        && left.worktrees.len() == right.worktrees.len()
        && left.worktrees.iter().zip(&right.worktrees).all(
            |((left_path, left), (right_path, right))| {
                left_path == right_path && same_checkout_cut(left, right)
            },
        )
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
        match fs::symlink_metadata(checkout.join(path)) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "refusing symlink at submodule restore path {}",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect submodule restore path"),
        }
    }
    for submodule in &submodules {
        crate::archive::slash_path(Path::new(&submodule.name))
            .with_context(|| format!("unsafe submodule admin name {:?}", submodule.name))?;
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
        let mut submodule = (*submodule).clone();
        let relative = submodule.path.strip_prefix(parent)?;
        if gitlinks.is_none() {
            // Raw operations skip every uncommitted overlay, including a
            // submodule checkout position differing from the saved HEAD.
            let Some(position) = crate::checkpoint::snapshot_index_gitlinks(runner, &owner)?
                .into_iter()
                .find(|entry| entry.path == relative)
            else {
                continue;
            };
            submodule.commit = position.commit;
        }
        crate::archive::ensure_no_symlink_ancestors(checkout, &submodule.path)?;
        let directory = checkout.join(&submodule.path);
        let admin = if directory.join(".git").try_exists()? {
            crate::archive::git::git_directories(runner, &directory)?.0
        } else {
            let modules = git_path(runner, &owner, "modules")?;
            let relative = Path::new("modules").join(&submodule.name);
            crate::archive::ensure_no_symlink_ancestors(
                modules.parent().context("module store has no parent")?,
                &relative,
            )?;
            let admin = modules.join(&submodule.name);
            match fs::symlink_metadata(&admin) {
                Ok(metadata) => ensure!(
                    !metadata.file_type().is_symlink(),
                    "submodule admin path is a symlink"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("inspect submodule admin path"),
            }
            admin
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
        // absorbgitdirs resolves its path through the index, even when the
        // saved index intentionally deletes this still-initialized checkout.
        if !crate::checkpoint::snapshot_index_gitlinks(runner, &owner)?
            .iter()
            .any(|entry| entry.path == relative)
        {
            let mut entry = OsString::from(format!("160000,{},", submodule.commit));
            entry.push(relative.as_os_str());
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
                git_failure("register deleted submodule gitlink", &output)
            );
        }
        let output = runner.run(
            &owner,
            &GitCommand {
                arguments: vec![
                    "--literal-pathspecs".into(),
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
        apply_index(parent)?;
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
        // index-pack makes installed packs read-only. A provisioned retry can
        // reuse this content-addressed file; index-pack still verifies it below.
        if !path.try_exists()? {
            fs::copy(pack, &path).context("copy snapshot pack into clone")?;
        }
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
            "refs/heads",
            "refs/notes",
            "refs/mj",
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
                && local_snapshot_ref(name)
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
    let git_version =
        mj_core::local_git::parse_git_version(&git_text(runner, destination, ["--version"])?)
            .unwrap_or(mj_core::local_git::MINIMUM_GIT_VERSION);
    // Before Git 2.43, missing interior commits fail visibly; only saved tips get SHA recovery.
    if git_version >= (2, 43, 0) {
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
    }
    let graph = runner.run(destination, &graph_command)?;
    let missing = graph
        .stdout
        .split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"?"));
    ensure!(
        graph.status == 0 && !missing,
        "snapshot history for {} is incomplete; origin may have rewritten history; missing objects: {}; {} {}{}",
        std::iter::once("HEAD".to_owned())
            .chain(restore_refs.keys().cloned())
            .collect::<Vec<_>>()
            .join(", "),
        String::from_utf8_lossy(&graph.stdout).trim(),
        String::from_utf8_lossy(&graph.stderr).trim(),
        fetch_failures.join("; "),
        if git_version < (2, 43, 0) {
            "; install Git 2.43 or newer and resume again"
        } else {
            ""
        }
    );
    for name in restore_refs
        .keys()
        .filter_map(|name| name.strip_prefix("refs/heads/"))
    {
        unset_config(runner, destination, &format!("branch.{name}.remote"))?;
        unset_config(runner, destination, &format!("branch.{name}.merge"))?;
    }
    for (name, tracking) in &snapshot.branch_tracking {
        if tracking.remote.as_deref() != Some("origin") {
            continue;
        }
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
    // Provisioned restore may retry after a later step needs local provisioning.
    // Rebuild the saved list rather than appending another copy on each attempt.
    git_text(runner, destination, ["update-ref", "-d", "refs/stash"])?;
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

    fn git_arguments(command: &GitCommand) -> &[OsString] {
        let mut arguments = command.arguments.as_slice();
        while arguments.len() >= 2 && arguments[0] == "-c" {
            arguments = &arguments[2..];
        }
        arguments
    }
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

    // Hard-won: ffd652c27: invalid .gitmodules blocked inventory before raw/managed operation policy.
    #[test]
    fn conflicted_gitmodules_obeys_raw_skip_and_managed_abort_policy() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        git(
            &root.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.branch",
                "base",
            ],
        );
        git(&root.checkout, &["commit", "-am", "base module setting"]);
        git(&root.checkout, &["switch", "-c", "side"]);
        git(
            &root.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.branch",
                "side",
            ],
        );
        git(&root.checkout, &["commit", "-am", "side module setting"]);
        git(&root.checkout, &["checkout", "main"]);
        git(
            &root.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.branch",
                "main",
            ],
        );
        git(&root.checkout, &["commit", "-am", "main module setting"]);
        assert_ne!(
            run_git(&FileGit, &root.checkout, ["merge", "side"], &[])
                .unwrap()
                .status,
            0
        );
        let original = fs::read(root.checkout.join(".gitmodules")).unwrap();
        let raw = root.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(raw.working_state_skipped);
        assert_eq!(
            fs::read(root.checkout.join(".gitmodules")).unwrap(),
            original
        );
        assert!(root.checkout.join(".git/MERGE_HEAD").exists());
        let restored = root.restore(&raw, None);
        assert_eq!(
            git(
                &restored,
                &["config", "--file", ".gitmodules", "submodule.sub.branch"]
            ),
            "main"
        );
        let refused = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("refused.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                refused.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::OperationInProgress(_))
            ),
            "{refused:#}"
        );
        root.capture(&CheckoutOwnership::Managed);
        assert!(!root.checkout.join(".git/MERGE_HEAD").exists());
        assert_eq!(
            git(
                &root.checkout,
                &["config", "--file", ".gitmodules", "submodule.sub.branch"]
            ),
            "main"
        );
    }

    // Hard-won: ffd652c27: scoped origin push URLs silently collapsed to the shared primary remote.
    #[test]
    fn scoped_remote_settings_emit_primary_remote_restore_notice() {
        let root = Repo::new();
        let module = Repo::new();
        let primary_push = Repo::new();
        let scoped_push = Repo::new();
        let module_dir = add_submodule(&root, &module);
        for directory in [&root.checkout, &module_dir] {
            git(directory, &["config", "extensions.worktreeConfig", "true"]);
        }
        git(
            &root.checkout,
            &[
                "config",
                "--worktree",
                "remote.origin.pushurl",
                primary_push.origin.to_str().unwrap(),
            ],
        );
        git(
            &module_dir,
            &[
                "config",
                "--worktree",
                "remote.origin.pushurl",
                scoped_push.origin.to_str().unwrap(),
            ],
        );
        let linked = root.checkout.join("lane");
        git(
            &root.checkout,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        );
        git(
            &linked,
            &[
                "config",
                "--worktree",
                "--replace-all",
                "remote.origin.pushurl",
                scoped_push.origin.to_str().unwrap(),
            ],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.notices.iter().any(|notice| notice.contains("lane")
            && notice.contains("worktree-specific remote settings are not restored")));
        assert!(snapshot.notices.iter().any(|notice| notice.contains("sub:")
            && notice.contains("worktree-specific remote settings are not restored")));
        assert_eq!(
            snapshot.worktrees[0].push_evidence,
            PushEvidence::Resolved(vec![scoped_push.origin.to_str().unwrap().to_owned()])
        );
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(
                &restored.join("lane"),
                &["remote", "get-url", "--push", "origin"]
            ),
            primary_push.origin.to_str().unwrap()
        );
        assert_eq!(
            git(
                &restored.join("sub"),
                &["remote", "get-url", "--push", "origin"]
            ),
            scoped_push.origin.to_str().unwrap()
        );
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
    // Hard-won: 42f6271ee: stash create writes and locks the real index even with optional locks disabled.
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
        let index_path = git_path(&FileGit, &r.checkout, "index").unwrap();
        let index_before = fs::read(&index_path).unwrap();
        let lock = index_path.with_extension("lock");
        fs::write(&lock, "another Git process owns the index\n").unwrap();
        estimate_delta(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Raw { launch: None },
            None,
        )
        .unwrap();
        assert_eq!(fs::read(&index_path).unwrap(), index_before);
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert_eq!(fs::read(&index_path).unwrap(), index_before);
        assert_eq!(
            fs::read_to_string(&lock).unwrap(),
            "another Git process owns the index\n"
        );
        fs::remove_file(lock).unwrap();
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
    // Hard-won: 42f6271ee: newline worktree paths disappeared from line-delimited discovery.
    #[test]
    fn nested_detached_worktree_and_dirty_state_survive() {
        let r = Repo::new();
        let nested = r.checkout.join("lanes/nested\nline");
        git(
            &r.checkout,
            &["worktree", "add", "--detach", nested.to_str().unwrap()],
        );
        fs::write(nested.join("file"), "nested commit\n").unwrap();
        git(&nested, &["commit", "-am", "nested"]);
        fs::write(nested.join("file"), "nested working state\n").unwrap();
        let legacy = workspace_inventory(
            &crate::test_support::Git231,
            &r.checkout,
            &CheckoutOwnership::Managed,
        )
        .unwrap_err();
        assert!(
            legacy.to_string().contains("newline worktree paths"),
            "{legacy:#}"
        );
        let encoded = serde_json::to_value(r.capture(&CheckoutOwnership::Managed)).unwrap();
        assert_eq!(
            encoded["worktrees"][0]["relative_path"],
            "lanes/nested\nline"
        );
        let snapshot: WorkspaceSnapshot = serde_json::from_value(encoded).unwrap();
        let inventory =
            workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert!(inventory.untracked.is_empty());
        assert_eq!(inventory.worktrees.len(), 1);
        let restored = r.restore(&snapshot, None).join("lanes/nested\nline");
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&nested, &["rev-parse", "HEAD"])
        );
        assert_eq!(git(&restored, &["diff"]), git(&nested, &["diff"]));
    }
    // Hard-won: 42f6271ee: raw operation capture rejected index state it deliberately skips.
    #[test]
    fn conflicted_merge_aborts_managed_and_leaves_raw_untouched() {
        let r = Repo::new();
        fs::write(r.checkout.join("base"), "hidden marker\n").unwrap();
        git(&r.checkout, &["add", "base"]);
        git(&r.checkout, &["commit", "-m", "hidden marker"]);
        r.conflict();
        r.start_conflict("merge");
        let before = fs::read(r.checkout.join("file")).unwrap();
        let estimate =
            estimate_delta(&FileGit, &r.checkout, &CheckoutOwnership::Managed, None).unwrap();
        assert!(estimate.working_state_bytes.is_none());
        assert!(r.checkout.join(".git/MERGE_HEAD").exists());
        fs::write(r.checkout.join("draft"), "intent during merge\n").unwrap();
        git(&r.checkout, &["add", "-N", "draft"]);
        git(&r.checkout, &["update-index", "--assume-unchanged", "base"]);
        let index_path = git_path(&FileGit, &r.checkout, "index").unwrap();
        let index_before = fs::read(&index_path).unwrap();
        let raw = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert_eq!(fs::read(&index_path).unwrap(), index_before);
        assert!(raw.working_state_skipped);
        git(
            &r.checkout,
            &["update-index", "--no-assume-unchanged", "base"],
        );
        git(&r.checkout, &["reset", "--", "draft"]);
        fs::remove_file(r.checkout.join("draft")).unwrap();
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
    // Hard-won: 7469d2e8b: a collapsed untracked repository used only its directory's metadata for consent.
    #[test]
    fn untracked_repository_child_edit_invalidates_shown_discard_consent() {
        let root = Repo::new();
        let embedded = root.checkout.join("embedded");
        git(
            root.temp.path(),
            &[
                "clone",
                root.origin.to_str().unwrap(),
                embedded.to_str().unwrap(),
            ],
        );
        let directory_identity = file_identity(&fs::symlink_metadata(&embedded).unwrap()).unwrap();
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
            .collect::<Vec<_>>();
        fs::write(embedded.join("file"), "edited existing embedded file\n").unwrap();
        assert_eq!(
            file_identity(&fs::symlink_metadata(&embedded).unwrap()).unwrap(),
            directory_identity
        );
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("refused.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Shown(shown.clone()),
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items)) if !items.is_empty() && items != &shown),
            "{error:#}"
        );
        assert_eq!(
            fs::read_to_string(embedded.join("file")).unwrap(),
            "edited existing embedded file\n"
        );
    }

    // Hard-won: 7469d2e8b: linked HEAD keep entries escaped the current-state refusal after staged deletion.
    #[test]
    fn linked_worktree_head_under_keep_is_refused_after_staged_deletion() {
        let root = Repo::new();
        let keep = root.checkout.join(".mj/keep");
        fs::create_dir_all(&keep).unwrap();
        let linked = keep.join("lane");
        git(
            &root.checkout,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        );
        git(&linked, &["rm", "file"]);
        let input = RepositoryInput {
            in_container: false,
            id: "root".into(),
            checkout: root.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: root.temp.path().join("refused.pack"),
            exclusions: ExclusionSource::Origin,
            keep_dir: Some(keep),
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
        let error = capture_session_snapshot(&FileGit, &[input], &consent).unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::TrackedKeepFiles { paths, .. })
            if paths.iter().any(|entry| entry.checkout.checkout_path == Path::new(".mj/keep/lane") && entry.value == Path::new("file"))),
            "{error:#}"
        );
    }

    // Hard-won: d7c636759: explicit HTTP remote helpers leaked their underlying URL credentials.
    #[test]
    fn explicit_http_helper_urls_redact_metadata_and_omit_credential_rules() {
        let root = Repo::new();
        let origin = "https::https://ORIGIN_TOKEN@host/origin.git";
        let push = "https::https://PUSH_USER:PUSH_PASSWORD@host/publish.git";
        git(&root.checkout, &["remote", "set-url", "origin", origin]);
        git(
            &root.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", root.origin.display()),
                origin,
            ],
        );
        git(
            &root.checkout,
            &[
                "config",
                "url.https::https://RULE_TOKEN@host/.insteadOf",
                "unused:",
            ],
        );
        git(&root.checkout, &["config", "remote.origin.pushurl", push]);
        assert_eq!(
            git(&root.checkout, &["remote", "get-url", "origin"]),
            root.origin.to_str().unwrap()
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let json = serde_json::to_string(&snapshot).unwrap();
        for secret in ["ORIGIN_TOKEN", "PUSH_USER", "PUSH_PASSWORD", "RULE_TOKEN"] {
            assert!(!json.contains(secret), "metadata leaked {secret}");
        }
        assert_eq!(snapshot.origin_urls, vec!["https::https://host/origin.git"]);
        assert_eq!(snapshot.push_urls, vec!["https::https://host/publish.git"]);
        assert_eq!(
            snapshot.push_evidence,
            PushEvidence::Resolved(vec!["https::https://host/publish.git".to_owned()])
        );
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.contains("credential-bearing"))
        );
    }

    // Hard-won: d7c636759: metadata-only raw suspension incorrectly applied transfer/discard refusals.
    #[test]
    fn raw_without_origin_keeps_hidden_edits_dirty_modules_and_intent_to_add() {
        struct NoNetworkGit;
        impl GitCommandRunner for NoNetworkGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                assert!(!command.arguments.iter().any(|argument| {
                    ["fetch", "clone", "ls-remote"]
                        .iter()
                        .any(|verb| argument == verb)
                }));
                FileGit.run(repository, command)
            }
        }
        let root = Repo::new();
        let module = Repo::new();
        let module_dir = add_submodule(&root, &module);
        fs::write(root.checkout.join("hidden"), "original hidden file\n").unwrap();
        git(&root.checkout, &["add", "hidden"]);
        git(&root.checkout, &["commit", "-m", "hidden fixture"]);
        root.conflict();
        root.start_conflict("merge");
        git(&root.checkout, &["remote", "remove", "origin"]);
        git(
            &root.checkout,
            &["update-index", "--assume-unchanged", "hidden"],
        );
        fs::write(root.checkout.join("hidden"), "hidden edit stays\n").unwrap();
        fs::write(module_dir.join("file"), "module edit stays\n").unwrap();
        fs::write(root.checkout.join("draft"), "intent stays\n").unwrap();
        git(&root.checkout, &["add", "-N", "draft"]);
        fs::write(root.checkout.join("untracked"), "untracked stays\n").unwrap();
        let index = fs::read(root.checkout.join(".git/index")).unwrap();
        let module_index = fs::read(git_path(&FileGit, &module_dir, "index").unwrap()).unwrap();
        let merge = fs::read(root.checkout.join(".git/MERGE_HEAD")).unwrap();
        let snapshot = capture_workspace_snapshot(
            &NoNetworkGit,
            &root.checkout,
            &CheckoutOwnership::Raw { launch: None },
            &root.temp.path().join("metadata.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap();
        assert!(
            snapshot.origin_urls.is_empty()
                && snapshot.pack.is_none()
                && snapshot.working_state.is_none()
        );
        assert!(snapshot.working_state_skipped && snapshot.has_uncommitted_state);
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.contains("resumes only in place"))
        );
        assert_eq!(fs::read(root.checkout.join(".git/index")).unwrap(), index);
        assert_eq!(
            fs::read(git_path(&FileGit, &module_dir, "index").unwrap()).unwrap(),
            module_index
        );
        assert_eq!(
            fs::read(root.checkout.join(".git/MERGE_HEAD")).unwrap(),
            merge
        );
        for (path, expected) in [
            (root.checkout.join("hidden"), "hidden edit stays\n"),
            (module_dir.join("file"), "module edit stays\n"),
            (root.checkout.join("draft"), "intent stays\n"),
            (root.checkout.join("untracked"), "untracked stays\n"),
        ] {
            assert_eq!(fs::read_to_string(path).unwrap(), expected);
        }
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

    // Hard-won: d7c636759: complete provisioned restore resurrected absent branches/notes/durable refs.
    #[test]
    fn complete_snapshot_removes_absent_provisioned_local_namespaces() {
        let root = Repo::new();
        git(&root.checkout, &["checkout", "--detach"]);
        git(&root.checkout, &["branch", "-D", "main"]);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let destination = root.temp.path().join("provisioned");
        git(
            root.temp.path(),
            &[
                "clone",
                root.origin.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        git(
            &destination,
            &["notes", "add", "-m", "destination-only note"],
        );
        git(
            &destination,
            &["update-ref", "refs/mj/reviewed/destination", "HEAD"],
        );
        git(
            &destination,
            &["update-ref", "refs/mj/restore/inflight", "HEAD"],
        );
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        let refs = read_all_refs(&FileGit, &destination).unwrap();
        for reference in [
            "refs/heads/main",
            "refs/notes/commits",
            "refs/mj/reviewed/destination",
        ] {
            assert!(!refs.contains_key(reference));
        }
        assert_eq!(
            git(&destination, &["rev-parse", "refs/mj/restore/inflight"]),
            snapshot.head.commit()
        );
        assert_eq!(
            git(&destination, &["rev-parse", "HEAD"]),
            snapshot.head.commit()
        );
    }

    // Hard-won: 7469d2e8b: no-origin raw snapshots marked ordinary tracked edits as clean.
    #[test]
    fn raw_without_origin_is_in_place_only() {
        let r = Repo::new();
        git(&r.checkout, &["remote", "remove", "origin"]);
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(!snapshot.has_uncommitted_state);
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
        let clean = fs::read(r.checkout.join("file")).unwrap();
        fs::write(r.checkout.join("file"), "unstaged tracked edit\n").unwrap();
        let dirty = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(dirty.has_uncommitted_state);
        assert!(dirty.working_state.is_none());
        git(&r.checkout, &["add", "file"]);
        let index = git(&r.checkout, &["write-tree"]);
        fs::write(r.checkout.join("file"), &clean).unwrap();
        // Comparing only the working file to HEAD would miss this staged edit.
        let staged = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(staged.has_uncommitted_state);
        assert_eq!(git(&r.checkout, &["write-tree"]), index);
        assert_eq!(fs::read(r.checkout.join("file")).unwrap(), clean);
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
                if git_arguments(command)
                    == [OsString::from("stash"), OsString::from("create")].as_slice()
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
        let provisioned = r.temp.path().join("provisioned");
        git(
            r.temp.path(),
            &[
                "clone",
                r.origin.to_str().unwrap(),
                provisioned.to_str().unwrap(),
            ],
        );
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &provisioned,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert_eq!(
            git(&provisioned, &["symbolic-ref", "HEAD"]),
            "refs/heads/main/topic"
        );
        assert_eq!(git(&provisioned, &["tag", "--list"]), "release/topic");
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
                if git_arguments(command)
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
        git(&r.checkout, &["config", "diff.ignoreSubmodules", "all"]);
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
                    .iter()
                    .any(|argument| argument == "pack-objects")
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
        let module = Repo::new();
        add_submodule(&r, &module);
        let primary_module = r.checkout.join("sub");
        git(&primary_module, &["checkout", "--detach"]);
        fs::write(
            primary_module.join("file"),
            "unrelated primary module work\n",
        )
        .unwrap();
        git(
            &primary_module,
            &["commit", "-am", "unrelated primary module work"],
        );
        git(&r.checkout, &["rm", "-f", "sub"]);
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
        assert!(!snapshot.refs.contains_key(&own_pin));
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.contains("fresh review baseline"))
        );
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
    // Hard-won: a2932eae2: initialized staged deletions failed absorption; nested published positions were refused.
    #[test]
    fn nested_submodule_positions_and_initialized_staged_deletions_restore() {
        let root = Repo::new();
        let module = Repo::new();
        let leaf = Repo::new();
        add_submodule(&module, &leaf);
        let parent = add_submodule(&root, &module);
        git(
            &root.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "--recursive",
            ],
        );
        let nested = parent.join("sub");
        let pin = git(&parent, &["rev-parse", ":sub"]);
        leaf.commit("published nested checkout\n");
        git(&leaf.checkout, &["push", "origin", "main"]);
        let published = git(&leaf.checkout, &["rev-parse", "HEAD"]);
        git(&nested, &["fetch", "origin"]);
        git(&nested, &["checkout", "--detach", &published]);
        let encoded = serde_json::to_value(root.capture(&CheckoutOwnership::Managed)).unwrap();
        assert!(
            encoded["initialized_submodules"]
                .as_array()
                .unwrap()
                .iter()
                .any(|module| module["path"] == "sub/sub")
        );
        let snapshot: WorkspaceSnapshot = serde_json::from_value(encoded).unwrap();
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(&restored.join("sub/sub"), &["rev-parse", "HEAD"]),
            published
        );
        assert_eq!(git(&restored.join("sub"), &["rev-parse", ":sub"]), pin);
        fs::remove_dir_all(restored).unwrap();
        git(&root.checkout, &["rm", "--cached", "-f", "sub"]);
        git(&root.checkout, &["config", "diff.ignoreSubmodules", "all"]);
        let before = fs::read(parent.join("file")).unwrap();
        fs::write(parent.join("file"), "uncommitted removed-module file\n").unwrap();
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
        fs::write(parent.join("file"), before).unwrap();

        let deleted = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&deleted, None);
        assert!(git(&restored, &["ls-files", "--stage", "--", "sub"]).is_empty());
        assert_eq!(
            git(&restored.join("sub/sub"), &["rev-parse", "HEAD"]),
            published
        );
    }

    // Hard-won: a2932eae2: submodule index changes could escape the ref/HEAD cut check.
    #[test]
    fn submodule_gitlink_update_during_stash_retries_the_snapshot_cut() {
        struct SwitchingGit {
            root: PathBuf,
            commit: String,
            changed: std::sync::atomic::AtomicBool,
        }
        impl GitCommandRunner for SwitchingGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                if repository == self.root
                    && git_arguments(command).starts_with(&["stash".into(), "create".into()])
                    && !self.changed.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    git(
                        repository,
                        &[
                            "update-index",
                            "--cacheinfo",
                            &format!("160000,{},sub", self.commit),
                        ],
                    );
                }
                FileGit.run(repository, command)
            }
        }
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        module.commit("published changed gitlink\n");
        git(&module.checkout, &["push", "origin", "main"]);
        let commit = git(&module.checkout, &["rev-parse", "HEAD"]);
        let runner = SwitchingGit {
            root: root.checkout.clone(),
            commit: commit.clone(),
            changed: false.into(),
        };
        let snapshot = capture_workspace_snapshot(
            &runner,
            &root.checkout,
            &CheckoutOwnership::Raw { launch: None },
            &root.temp.path().join("race.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        assert_eq!(snapshot.index_gitlinks[0].commit, commit);
        let restored = root.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", ":sub"]), commit);
    }

    // Hard-won: a2932eae2: exact-tip evidence for uninitialized modules must come from .gitmodules, never cached forks.
    #[test]
    fn uninitialized_submodule_ignores_cached_forks_and_gives_an_init_step() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        module.commit("staged published ancestor\n");
        let staged = git(&module.checkout, &["rev-parse", "HEAD"]);
        module.commit("advanced tip\n");
        git(&module.checkout, &["push", "origin", "main"]);
        let tip = git(&module.checkout, &["rev-parse", "HEAD"]);
        git(
            &root.checkout,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{staged},sub"),
            ],
        );
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        let fork = root.temp.path().join("fork.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                fork.to_str().unwrap(),
            ],
        );
        git(&fork, &["update-ref", "refs/heads/main", &staged]);
        let admin = root.checkout.join(".git/modules/sub");
        git(
            &admin,
            &["remote", "set-url", "origin", fork.to_str().unwrap()],
        );
        for cached in [true, false] {
            if !cached {
                fs::remove_dir_all(&admin).unwrap();
            }
            let error = capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("no-proof.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("git submodule update --init sub"),
                "{error:#}"
            );
            assert!(
                format!("{error:#}").contains("unstage the pointer change"),
                "{error:#}"
            );
        }
        git(
            &root.checkout,
            &["update-index", "--cacheinfo", &format!("160000,{tip},sub")],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.initialized_submodules.is_empty());
        let restored = root.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", ":sub"]), tip);
        assert!(!restored.join("sub/.git").exists());
    }

    // Hard-won: a2932eae2: gitlink pathspec metacharacters selected sibling histories.
    #[test]
    fn submodule_paths_with_globs_use_only_their_own_history() {
        let root = Repo::new();
        for path in ["libs[1]", "libs1"] {
            let module = Repo::new();
            module.commit(path);
            git(&module.checkout, &["push", "origin", "main"]);
            git(
                &root.checkout,
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    module.origin.to_str().unwrap(),
                    path,
                ],
            );
            // Keep each origin alive after its fixture goes out of scope.
            let origin = root.temp.path().join(format!("{path}.git"));
            git(
                root.temp.path(),
                &[
                    "clone",
                    "--bare",
                    module.origin.to_str().unwrap(),
                    origin.to_str().unwrap(),
                ],
            );
            git(
                &root.checkout.join(path),
                &["remote", "set-url", "origin", origin.to_str().unwrap()],
            );
            git(
                &root.checkout,
                &[
                    "config",
                    &format!("submodule.{path}.url"),
                    origin.to_str().unwrap(),
                ],
            );
        }
        git(
            &root.checkout,
            &["commit", "-am", "registered literal modules"],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&snapshot, None);
        for path in ["libs[1]", "libs1"] {
            assert_eq!(
                git(&restored.join(path), &["rev-parse", "HEAD"]),
                git(&root.checkout.join(path), &["rev-parse", "HEAD"])
            );
        }
    }

    // Hard-won: a2932eae2: raw-operation conversion restored live submodule overlays.
    #[test]
    fn raw_merge_skips_uncommitted_submodule_checkout_positions() {
        let root = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&root, &module);
        let saved = git(&directory, &["rev-parse", "HEAD"]);
        root.conflict();
        module.commit("different published checkout\n");
        git(&module.checkout, &["push", "origin", "main"]);
        let live = git(&module.checkout, &["rev-parse", "HEAD"]);
        git(&directory, &["fetch", "origin"]);
        git(&directory, &["checkout", "--detach", &live]);
        root.start_conflict("merge");
        let snapshot = root.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(snapshot.working_state_skipped);
        let restored = root.restore(&snapshot, None);
        assert_eq!(git(&restored.join("sub"), &["rev-parse", "HEAD"]), saved);
        assert!(git(&restored, &["status", "--porcelain"]).is_empty());
    }

    // Hard-won: a2932eae2: lexical parent removal changed remotes that traversed symlinks.
    #[cfg(unix)]
    #[test]
    fn relative_remote_symlink_parent_keeps_fetch_and_push_destination() {
        let root = Repo::new();
        let target = Repo::new();
        std::os::unix::fs::symlink(&target.checkout, root.checkout.join("link")).unwrap();
        git(
            &root.checkout,
            &["remote", "set-url", "origin", "link/../origin.git"],
        );
        root.commit("work for symlink destination\n");
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            resolve_remote_path(&root.checkout, &snapshot.origin_urls[0]).unwrap(),
            target.origin.to_str().unwrap()
        );
        let restored = root.restore(&snapshot, None);
        git(&restored, &["push", "origin", "main"]);
        assert_eq!(
            git(&target.origin, &["rev-parse", "main"]),
            snapshot.head.commit()
        );
        assert_ne!(
            git(&root.origin, &["rev-parse", "main"]),
            snapshot.head.commit()
        );
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

    // Hard-won: 3a4aef552: the leaf submodule symlink could redirect restore into another checkout.
    #[cfg(unix)]
    #[test]
    fn submodule_restore_rejects_a_leaf_symlink_without_touching_its_target() {
        let root = Repo::new();
        let module = Repo::new();
        let outside = Repo::new();
        add_submodule(&root, &module);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let destination = root.temp.path().join("destination");
        fs::create_dir(&destination).unwrap();
        git(&destination, &["init"]);
        std::os::unix::fs::symlink(&outside.checkout, destination.join("sub")).unwrap();
        let refs = read_all_refs(&FileGit, &outside.checkout).unwrap();
        let config = fs::read(outside.checkout.join(".git/config")).unwrap();
        let index = fs::read(outside.checkout.join(".git/index")).unwrap();
        let error = restore_submodules(
            &FileGit,
            &destination,
            &snapshot.initialized_submodules,
            Some(&snapshot.index_gitlinks),
            true,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");
        assert_eq!(read_all_refs(&FileGit, &outside.checkout).unwrap(), refs);
        assert_eq!(
            fs::read(outside.checkout.join(".git/config")).unwrap(),
            config
        );
        assert_eq!(
            fs::read(outside.checkout.join(".git/index")).unwrap(),
            index
        );
    }

    // Hard-won: efc122a75: published module-linked HEADs hid tracked edits inside teardown.
    #[test]
    fn module_linked_checkout_edits_refuse_only_inside_the_managed_teardown() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        let source = root.checkout.join("sub");
        let outside = root.temp.path().join("outside-module-lane");
        git(
            &source,
            &[
                "worktree",
                "add",
                "--detach",
                outside.to_str().unwrap(),
                "origin/main",
            ],
        );
        fs::write(outside.join("file"), "unrelated external edits\n").unwrap();
        root.capture(&CheckoutOwnership::Managed);
        let inside = source.join("local-lane");
        git(
            &source,
            &[
                "worktree",
                "add",
                "--detach",
                inside.to_str().unwrap(),
                "origin/main",
            ],
        );
        let exclude = git_path(&FileGit, &source, "info/exclude").unwrap();
        fs::write(exclude, "local-lane/\n").unwrap();
        fs::write(inside.join("file"), "staged linked edits\n").unwrap();
        git(&inside, &["add", "file"]);
        fs::write(inside.join("file"), "unstaged linked edits\n").unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("guard.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("linked-worktree")
                && error.to_string().contains("tracked edits"),
            "{error:#}"
        );
        assert_eq!(
            fs::read_to_string(inside.join("file")).unwrap(),
            "unstaged linked edits\n"
        );
    }

    // Hard-won: efc122a75: module names could escape the administrative module store.
    #[test]
    fn submodule_admin_name_cannot_attach_an_outside_repository() {
        let root = Repo::new();
        let module = Repo::new();
        let outside = Repo::new();
        add_submodule(&root, &module);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let destination = root.temp.path().join("destination");
        fs::create_dir(&destination).unwrap();
        git(&destination, &["init"]);
        let config = fs::read(outside.checkout.join(".git/config")).unwrap();
        let head = git(&outside.checkout, &["rev-parse", "HEAD"]);
        for name in [
            outside.checkout.join(".git").to_str().unwrap(),
            "../../outside",
        ] {
            let mut modules = snapshot.initialized_submodules.clone();
            modules[0].name = name.to_owned();
            let error = restore_submodules(
                &FileGit,
                &destination,
                &modules,
                Some(&snapshot.index_gitlinks),
                true,
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("unsafe submodule admin name"),
                "{error:#}"
            );
        }
        assert_eq!(
            fs::read(outside.checkout.join(".git/config")).unwrap(),
            config
        );
        assert_eq!(git(&outside.checkout, &["rev-parse", "HEAD"]), head);
        assert!(!destination.join("sub").exists());
    }

    // Hard-won: 3a4aef552: deinit hid unpublished nested refs/stashes from the managed loss guard.
    #[test]
    fn deinitialized_submodule_protects_cached_nested_refs_and_stashes() {
        let root = Repo::new();
        let parent = Repo::new();
        let leaf = Repo::new();
        add_submodule(&parent, &leaf);
        add_submodule(&root, &parent);
        git(
            &root.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "--recursive",
            ],
        );
        let nested = root.checkout.join("sub/sub");
        let pin = git(&nested, &["rev-parse", "HEAD"]);
        git(&nested, &["switch", "-c", "hidden"]);
        fs::write(nested.join("file"), "hidden unpublished commit\n").unwrap();
        git(&nested, &["commit", "-am", "hidden"]);
        let detached = git(&nested, &["rev-parse", "HEAD"]);
        git(&nested, &["checkout", "--detach", &pin]);
        fs::write(nested.join("file"), "hidden stash\n").unwrap();
        git(&nested, &["stash", "push", "-m", "hidden stash"]);
        let stash = git(&nested, &["rev-parse", "refs/stash"]);
        git(&nested, &["reflog", "expire", "--expire=all", "refs/stash"]);
        assert_eq!(git(&nested, &["rev-parse", "refs/stash"]), stash);
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        let capture = |ownership| {
            capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                ownership,
                &root.temp.path().join("deinit.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
        };
        let error = capture(&CheckoutOwnership::Managed).unwrap_err();
        for name in ["sub/sub", "refs/heads/hidden", "refs/stash"] {
            assert!(error.to_string().contains(name), "{error:#}");
        }
        let snapshot = capture(&CheckoutOwnership::Raw { launch: None }).unwrap();
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.contains("refs/heads/hidden") && notice.contains("refs/stash"))
        );
        assert!(snapshot.initialized_submodules.is_empty());
        let admin = root.checkout.join(".git/modules/sub/modules/sub");
        for arguments in [
            vec!["update-ref", "--no-deref", "HEAD", detached.as_str()],
            vec!["update-ref", "-d", "refs/heads/hidden"],
            vec!["update-ref", "-d", "refs/stash"],
        ] {
            let mut command = vec![
                "--git-dir",
                admin.to_str().unwrap(),
                "--work-tree",
                admin.to_str().unwrap(),
            ];
            command.extend(arguments);
            git(&root.checkout, &command);
        }
        let error = capture(&CheckoutOwnership::Managed).unwrap_err();
        assert!(error.to_string().contains("sub/sub HEAD"), "{error:#}");
        // git rm also deletes the live .gitmodules registration, but not its cache.
        git(&root.checkout, &["rm", "sub"]);
        let error = capture(&CheckoutOwnership::Managed).unwrap_err();
        assert!(error.to_string().contains("sub/sub HEAD"), "{error:#}");
        let raw = capture(&CheckoutOwnership::Raw { launch: None }).unwrap();
        assert!(
            raw.notices
                .iter()
                .any(|notice| notice.contains("sub/sub HEAD"))
        );
        git(
            &root.checkout,
            &[
                "--git-dir",
                admin.to_str().unwrap(),
                "--work-tree",
                admin.to_str().unwrap(),
                "update-ref",
                "--no-deref",
                "HEAD",
                &pin,
            ],
        );
        let linked = root.checkout.join("lane");
        git(
            &root.checkout,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        );
        git(
            &linked,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "sub",
            ],
        );
        let linked_module = linked.join("sub");
        git(&linked_module, &["switch", "-c", "private-linked"]);
        fs::write(linked_module.join("file"), "linked cache work\n").unwrap();
        git(&linked_module, &["commit", "-am", "private linked work"]);
        let own_linked = root.temp.path().join("module-linked-tip");
        git(
            &linked_module,
            &[
                "worktree",
                "add",
                "--detach",
                own_linked.to_str().unwrap(),
                "origin/main",
            ],
        );
        fs::write(own_linked.join("file"), "only module-linked HEAD\n").unwrap();
        git(&own_linked, &["commit", "-am", "only module-linked HEAD"]);
        let only_head = git(&own_linked, &["rev-parse", "HEAD"]);
        git(&linked, &["rm", "-f", "sub"]);
        let error = capture(&CheckoutOwnership::Managed).unwrap_err();
        assert!(
            error.to_string().contains("lane")
                && error.to_string().contains("refs/heads/private-linked"),
            "{error:#}"
        );
        let linked_admin = git_path(&FileGit, &linked, "modules/sub").unwrap();
        let published = git(&parent.checkout, &["rev-parse", "HEAD"]);
        for arguments in [
            vec!["update-ref", "--no-deref", "HEAD", published.as_str()],
            vec!["update-ref", "-d", "refs/heads/private-linked"],
        ] {
            let mut command = vec![
                "--git-dir",
                linked_admin.to_str().unwrap(),
                "--work-tree",
                linked_admin.to_str().unwrap(),
            ];
            command.extend(arguments);
            git(&root.checkout, &command);
        }
        let error = capture(&CheckoutOwnership::Managed).unwrap_err();
        assert!(
            error.to_string().contains("linked-worktree") && error.to_string().contains(&only_head),
            "{error:#}"
        );
        git(&own_linked, &["checkout", "--detach", "origin/main"]);
        let missing = root.checkout.join("gone-lane");
        git(
            &root.checkout,
            &["worktree", "add", "--detach", missing.to_str().unwrap()],
        );
        git(
            &missing,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "sub",
            ],
        );
        let missing_module = missing.join("sub");
        git(&missing_module, &["checkout", "--detach"]);
        fs::write(missing_module.join("file"), "prunable module HEAD only\n").unwrap();
        git(
            &missing_module,
            &["commit", "-am", "prunable module HEAD only"],
        );
        let missing_head = git(&missing_module, &["rev-parse", "HEAD"]);
        git(&missing, &["rm", "-f", "sub"]);
        fs::remove_dir_all(&missing).unwrap();
        let error = capture(&CheckoutOwnership::Managed).unwrap_err();
        assert!(
            error.to_string().contains("gone-lane") && error.to_string().contains(&missing_head),
            "{error:#}"
        );
    }

    // Hard-won: 3a4aef552: a parent-local rewrite could authorize an uninitialized mirror-only pin.
    #[test]
    fn uninitialized_submodule_probe_ignores_parent_local_mirror_rewrites() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        let mirror = root.temp.path().join("mirror.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        module.commit("published only on mirror\n");
        git(
            &module.checkout,
            &["push", mirror.to_str().unwrap(), "HEAD:main"],
        );
        let pin = git(&module.checkout, &["rev-parse", "HEAD"]);
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        git(
            &root.checkout,
            &["update-index", "--cacheinfo", &format!("160000,{pin},sub")],
        );
        git(
            &root.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", mirror.display()),
                module.origin.to_str().unwrap(),
            ],
        );
        assert!(
            git(
                &root.checkout,
                &["ls-remote", module.origin.to_str().unwrap()]
            )
            .contains(&pin)
        );
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("mirror.pack"),
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
        assert!(
            error
                .to_string()
                .contains("git submodule update --init sub")
        );
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
            Some(SnapshotRefused::UnpublishedSubmoduleWork(_))
        ));
        for text in [
            "submodule sub",
            "refs/heads/private",
            "stash@{0}",
            "push",
            "drop",
        ] {
            assert!(refused.to_string().contains(text), "{refused:#}");
        }
        let raw = capture(&CheckoutOwnership::Raw { launch: None }).unwrap();
        assert!(
            raw.notices
                .iter()
                .any(|notice| notice.contains("refs/heads/private")
                    && notice.contains("stash@{0}")
                    && notice.contains("raw checkout is kept"))
        );
        git(&directory, &["push", "origin", "private"]);
        assert!(
            capture(&CheckoutOwnership::Managed)
                .unwrap_err()
                .to_string()
                .contains("stash@{0}")
        );
        git(&directory, &["stash", "drop"]);
        capture(&CheckoutOwnership::Managed).unwrap();
    }

    // Hard-won: 8251715a1: operation state could change after inventory without moving HEAD.
    #[test]
    fn raw_merge_started_during_capture_retries_with_working_state_skipped() {
        struct MergingGit {
            root: PathBuf,
            started: std::sync::atomic::AtomicBool,
        }
        impl GitCommandRunner for MergingGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                if repository == self.root
                    && git_arguments(command).starts_with(&["stash".into(), "create".into()])
                    && !self.started.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    git(repository, &["merge", "--no-commit", "--no-ff", "side"]);
                }
                FileGit.run(repository, command)
            }
        }
        let root = Repo::new();
        git(&root.checkout, &["switch", "-c", "side"]);
        fs::write(root.checkout.join("side"), "merge result\n").unwrap();
        git(&root.checkout, &["add", "side"]);
        git(&root.checkout, &["commit", "-m", "side"]);
        git(&root.checkout, &["checkout", "main"]);
        let runner = MergingGit {
            root: root.checkout.clone(),
            started: false.into(),
        };
        let snapshot = capture_workspace_snapshot(
            &runner,
            &root.checkout,
            &CheckoutOwnership::Raw { launch: None },
            &root.temp.path().join("merge.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        assert!(snapshot.working_state_skipped);
        assert!(snapshot.working_state.is_none());
        assert!(root.checkout.join(".git/MERGE_HEAD").exists());
        let restored = root.restore(&snapshot, None);
        assert!(!restored.join("side").exists());
        assert!(git(&restored, &["status", "--porcelain"]).is_empty());
    }

    // Hard-won: 8251715a1: canonicalizing a symlinked URL broke pushInsteadOf prefixes.
    #[cfg(unix)]
    #[test]
    fn symlinked_fetch_remote_preserves_textual_push_rewrites() {
        let root = Repo::new();
        fs::create_dir(root.temp.path().join("mirrors")).unwrap();
        fs::create_dir(root.temp.path().join("publish")).unwrap();
        std::os::unix::fs::symlink(&root.origin, root.temp.path().join("mirrors/repo.git"))
            .unwrap();
        let publication = root.temp.path().join("publish/repo.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                root.origin.to_str().unwrap(),
                publication.to_str().unwrap(),
            ],
        );
        let original = git(&root.origin, &["rev-parse", "main"]);
        git(
            &root.checkout,
            &["remote", "set-url", "origin", "../mirrors/repo.git"],
        );
        git(
            &root.checkout,
            &["config", "url.../publish/.pushInsteadOf", "../mirrors/"],
        );
        root.commit("published through rewrite\n");
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&snapshot, None);
        git(&restored, &["push", "origin", "main"]);
        assert_eq!(
            git(&publication, &["rev-parse", "main"]),
            snapshot.head.commit()
        );
        assert_eq!(git(&root.origin, &["rev-parse", "main"]), original);
    }

    // Hard-won: 8251715a1: URL parsing treated an SCP host as a non-hierarchical scheme.
    #[test]
    fn scp_default_remote_resolves_uninitialized_relative_module_urls() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        module.commit("new staged module tip\n");
        git(&module.checkout, &["push", "origin", "main"]);
        let tip = git(&module.checkout, &["rev-parse", "HEAD"]);
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        git(
            &root.checkout,
            &["update-index", "--cacheinfo", &format!("160000,{tip},sub")],
        );
        git(
            &root.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.url",
                "../lib.git",
            ],
        );
        git(&root.checkout, &["add", ".gitmodules"]);
        git(
            &root.checkout,
            &["remote", "set-url", "origin", "git-host:team/app.git"],
        );
        git(
            &root.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", root.origin.display()),
                "git-host:team/app.git",
            ],
        );
        git(
            &root.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", module.origin.display()),
                "git-host:team/lib.git",
            ],
        );
        // A same-named directory relative to the checkout is a different repo.
        let decoy = root.temp.path().join("lib.git");
        git(
            root.temp.path(),
            &["init", "--bare", decoy.to_str().unwrap()],
        );
        let host = host_git(root.temp.path(), "scp-host-config");
        for (base, prefix) in [
            (&root.origin, "git-host:team/app.git"),
            (&module.origin, "git-host:team/lib.git"),
        ] {
            git(
                root.temp.path(),
                &[
                    "config",
                    "--file",
                    host.global.to_str().unwrap(),
                    &format!("url.{}.insteadOf", base.display()),
                    prefix,
                ],
            );
        }
        let snapshot = capture_workspace_snapshot(
            &host,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("scp.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        let restored = root.temp.path().join("restored");
        restore_workspace_snapshot(&host, &snapshot, &restored, &clone_base(&snapshot, None))
            .unwrap();
        assert_eq!(git(&restored, &["rev-parse", ":sub"]), tip);
        git(&restored, &["submodule", "init", "sub"]);
        assert_eq!(
            git(&restored, &["config", "submodule.sub.url"]),
            "git-host:team/lib.git"
        );
        assert!(!restored.join("sub/.git").exists());
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
        let restored = r.temp.path().join("provisioned");
        fs::create_dir(&restored).unwrap();
        git(&restored, &["init"]);
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &restored,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
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
        let error = capture_session_snapshot(&FileGit, std::slice::from_ref(&repository), &consent)
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::TrackedKeepFiles { .. })
        ));
        git(&r.checkout, &["stash", "pop"]);
        git(&r.checkout, &["commit", "-m", "keep file was committed"]);
        let committed = git(&r.checkout, &["rev-parse", "HEAD"]);
        git(&r.checkout, &["rm", ".mj/keep/private"]);
        assert!(
            capture_session_snapshot(&FileGit, std::slice::from_ref(&repository), &consent)
                .is_err()
        );
        git(&r.checkout, &["commit", "-m", "remove keep file"]);
        // Historical tracked state remains faithful; only current HEAD/index
        // force-add mistakes are refused, not commits that already removed it.
        let session = capture_session_snapshot(&FileGit, &[repository], &consent).unwrap();
        let restored = r.restore(&session.repositories["root"], None);
        assert_eq!(
            git(
                &restored,
                &["show", &format!("{committed}:.mj/keep/private")]
            ),
            "keep transport only"
        );
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
        for message in ["first saved stash", "second saved stash"] {
            fs::write(r.checkout.join("file"), message).unwrap();
            git(&r.checkout, &["stash", "push", "-m", message]);
        }
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
        assert_eq!(read_stash(&FileGit, &destination).unwrap(), snapshot.stash);
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
        // Isolate the post-abort gitlink guard from inactive-branch loss refusal.
        git(&nested, &["branch", "-D", "main"]);
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
    fn uninitialized_relative_submodule_urls_follow_origin_and_drop_other_upstreams() {
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
            "https://host/org/lib.git"
        );
        // Exercise that resolved URL with real Git transport via insteadOf.
        git(
            &r.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", sub.origin.display()),
                "https://host/org/lib.git",
            ],
        );
        git(
            &r.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", r.origin.display()),
                "https://host/org/app.git",
            ],
        );
        sub.commit("published next pin\n");
        git(&sub.checkout, &["push", "origin", "main"]);
        let published = git(&sub.checkout, &["rev-parse", "HEAD"]);
        git(
            &r.checkout,
            &["remote", "set-url", "origin", r.origin.to_str().unwrap()],
        );
        git(
            r.temp.path(),
            &[
                "clone",
                "--bare",
                sub.origin.to_str().unwrap(),
                r.temp.path().join("lib.git").to_str().unwrap(),
            ],
        );
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
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.contains("upstreams") && notice.contains("main"))
        );
        let restored = r.restore(&snapshot, None);
        assert!(
            config_values(&FileGit, &restored, "branch.main.remote")
                .unwrap()
                .is_empty()
        );
        assert!(
            config_values(&FileGit, &restored, "branch.main.merge")
                .unwrap()
                .is_empty()
        );

        let provisioned = r.temp.path().join("provisioned-upstream");
        git(
            r.temp.path(),
            &[
                "clone",
                r.origin.to_str().unwrap(),
                provisioned.to_str().unwrap(),
            ],
        );
        git(&provisioned, &["config", "branch.main.remote", "upstream"]);
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &provisioned,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert!(
            config_values(&FileGit, &provisioned, "branch.main.remote")
                .unwrap()
                .is_empty()
        );
        assert!(
            config_values(&FileGit, &provisioned, "branch.main.merge")
                .unwrap()
                .is_empty()
        );
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
        let index_path = git_path(&FileGit, &nested, "index").unwrap();
        let before_index = fs::read(&index_path).unwrap();
        let unchanged = fs::read(nested.join("file")).unwrap();
        fs::write(nested.join("file"), unchanged).unwrap();
        estimate_delta(&FileGit, &r.checkout, &CheckoutOwnership::Managed, None).unwrap();
        assert_eq!(fs::read(&index_path).unwrap(), before_index);
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
            PathBuf::from(
                resolve_remote_path(&r.checkout, &snapshot.initialized_submodules[0].url).unwrap()
            ),
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
            snapshot.initialized_submodules[0].url
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
            rule.value == "corp:" && Path::new(url_rule_parts(&rule.name).unwrap().0).is_absolute()
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
            resolve_remote_path(
                &destination,
                &git(&destination, &["remote", "get-url", "origin"])
            )
            .unwrap(),
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
                    &format!("+refs/heads/*:refs/remotes/custom-{namespace}/*"),
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
                "+refs/heads/*:refs/remotes/custom-module/*",
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
                "+refs/heads/*:refs/remotes/custom-root/*",
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
        // A fetched PR tip is not available to ordinary restore fetches, even
        // when origin's configured destination resembles its tracking namespace.
        let pull = Repo::new();
        pull.commit("base published only under pull ref\n");
        let base = git(&pull.checkout, &["rev-parse", "HEAD"]);
        git(&pull.checkout, &["push", "origin", "HEAD:refs/pull/1/head"]);
        git(
            &pull.checkout,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/pull/*/head:refs/remotes/origin/pr/*",
            ],
        );
        git(&pull.checkout, &["fetch", "origin"]);
        pull.commit("child of pull-only base\n");
        fs::rename(&pull.origin, pull.temp.path().join("offline.git")).unwrap();
        let snapshot = pull.capture(&CheckoutOwnership::Managed);
        assert!(!snapshot.remote_refs_stale);
        assert!(snapshot.pack.is_some());
        let restored = pull.temp.path().join("provisioned");
        fs::create_dir(&restored).unwrap();
        git(&restored, &["init"]);
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &restored,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        git(&restored, &["cat-file", "-e", &base]);
        git(&restored, &["fsck", "--no-reflogs"]);
        // Fetch mappings into local branches do not certify their later commits.
        let local = Repo::new();
        git(
            &local.checkout,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/main:refs/heads/integration",
            ],
        );
        git(&local.checkout, &["fetch", "origin"]);
        git(&local.checkout, &["checkout", "integration"]);
        local.commit("local integration work\n");
        let unpublished = git(&local.checkout, &["rev-parse", "HEAD"]);
        fs::rename(&local.origin, local.temp.path().join("offline.git")).unwrap();
        let snapshot = local.capture(&CheckoutOwnership::Managed);
        assert!(!snapshot.remote_refs_stale);
        assert!(snapshot.pack.is_some());
        let restored = local.temp.path().join("provisioned");
        fs::create_dir(&restored).unwrap();
        git(&restored, &["init"]);
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &restored,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert_eq!(git(&restored, &["rev-parse", "HEAD"]), unpublished);
        git(&restored, &["fsck", "--no-reflogs"]);
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
