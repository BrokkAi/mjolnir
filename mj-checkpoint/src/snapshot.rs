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
pub struct SnapshotSubmodule {
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
pub struct WorkspaceSnapshot {
    pub origin: Option<String>,
    pub push_urls: Vec<String>,
    pub head: SnapshotHead,
    pub refs: BTreeMap<String, String>,
    pub branch_tracking: BTreeMap<String, BranchTracking>,
    pub stash: Vec<SnapshotStashEntry>,
    pub working_state: Option<String>,
    pub working_state_skipped: bool,
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
pub enum UntrackedItem {
    Path {
        checkout: CheckoutKey,
        path: PathBuf,
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
}
impl std::fmt::Display for SnapshotRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
                "submodule {}:{}:{} has tracked changes or an unpublished checkout; commit and push inside the submodule, or discard the change",
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
                        UntrackedItem::Path { checkout, path } => write!(
                            f,
                            " {}:{}:{}",
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
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInventory {
    pub checkout: CheckoutKey,
    pub untracked: Vec<UntrackedPath>,
    pub untracked_stashes: Vec<Scoped<SnapshotStashEntry>>,
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
fn read_all_refs(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<BTreeMap<String, String>> {
    let text = git_text(
        runner,
        checkout,
        [
            "for-each-ref",
            "--format=%(refname)%00%(objectname)",
            "refs/heads",
            "refs/tags",
            "refs/notes",
            "refs/mj/",
            "refs/remotes/origin/",
        ],
    )?;
    text.lines()
        .map(|line| {
            let (name, oid) = line.split_once('\0').context("malformed Git ref")?;
            Ok((name.to_owned(), oid.to_owned()))
        })
        .filter(|entry: &Result<(String, String)>| {
            entry.as_ref().map_or(true, |(name, _)| {
                name != "refs/stash"
                    && !name.starts_with("refs/mj/snapshot/")
                    && !name.starts_with("refs/mj/restore/")
                    && !mj_core::archive::is_review_baseline_pin(name)
            })
        })
        .collect()
}
fn read_refs(runner: &dyn GitCommandRunner, checkout: &Path) -> Result<BTreeMap<String, String>> {
    Ok(read_all_refs(runner, checkout)?
        .into_iter()
        .filter(|(name, _)| !name.starts_with("refs/remotes/origin/"))
        .collect())
}
fn selected_stash(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
) -> Result<Vec<SnapshotStashEntry>> {
    let mut stash = read_stash(runner, checkout)?;
    if let CheckoutOwnership::Raw {
        launch: Some(launch),
    } = ownership
    {
        stash.retain(|entry| !launch.stash.iter().any(|old| old.commit == entry.commit));
    }
    Ok(stash)
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
fn hash_operation_occurrence(identity: &mut blake3::Hasher, metadata: &fs::Metadata) -> Result<()> {
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
                hash_operation_occurrence(&mut identity, &metadata)?;
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
fn owns_worktree(ownership: &CheckoutOwnership, relative: &Path) -> bool {
    match ownership {
        CheckoutOwnership::Managed => true,
        CheckoutOwnership::Raw { launch } => launch
            .as_ref()
            .is_some_and(|record| !record.worktrees.iter().any(|path| path == relative)),
    }
}
/// Git resolves a relative filesystem remote from the checkout. Store that
/// absolute path so restoring from another directory cannot change its meaning.
pub(crate) fn snapshot_remote(checkout: &Path, remote: &str) -> Result<String> {
    resolve_remote_path(
        checkout,
        &crate::archive::redact_origin_credentials(remote)?,
    )
}
pub(crate) fn resolve_remote_path(checkout: &Path, remote: &str) -> Result<String> {
    let path = Path::new(remote);
    // A colon before the first slash denotes Git's scp or transport syntax.
    // Path::is_absolute handles native drive letters before this text boundary.
    let transport = remote
        .find(':')
        .is_some_and(|colon| remote.find('/').is_none_or(|slash| colon < slash));
    if path.is_absolute() || transport {
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
    for tree in nested
        .into_iter()
        .filter(|tree| owns_worktree(ownership, &tree.relative))
    {
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
    checkout: &Path,
    ownership: &CheckoutOwnership,
    consent: &CaptureConsent,
    key: &CheckoutKey,
) -> Result<WorkspaceInventory> {
    let inventory = workspace_inventory_scoped(runner, checkout, ownership, key)?;
    check_consent(&inventory, consent)?;
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
    Ok(inventory)
}
fn check_consent(inventory: &WorkspaceInventory, consent: &CaptureConsent) -> Result<()> {
    let mut untracked = Vec::new();
    let mut operations = Vec::new();
    let mut hidden = Vec::new();
    let mut dirty_submodules = Vec::new();
    let mut collect =
        |inventory: &WorkspaceInventory| {
            untracked.extend(inventory.untracked.iter().map(|entry| UntrackedItem::Path {
                checkout: entry.checkout.clone(),
                path: entry.path.clone(),
            }));
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
}
/// Capture and dry-run share this read-only interpretation of origin's state.
fn origin_exclusions(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    remote: &str,
    refs: &BTreeMap<String, String>,
) -> Result<OriginExclusions> {
    let fallback = refs
        .iter()
        .filter(|(name, _)| name.starts_with("refs/remotes/origin/"))
        .map(|(_, oid)| oid.clone())
        .collect::<Vec<_>>();
    remote_objects(runner, checkout, remote, &fallback)
}
pub(crate) fn remote_objects(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    remote: &str,
    fallback: &[String],
) -> Result<OriginExclusions> {
    let advertised = run_git(runner, checkout, ["ls-remote", remote], &[]);
    let mut objects = Vec::new();
    let stale = match advertised {
        Ok(output) if output.status == 0 => {
            for line in std::str::from_utf8(&output.stdout)?.lines() {
                let (oid, _) = line
                    .split_once('\t')
                    .context("malformed origin advertisement")?;
                if run_git(runner, checkout, ["cat-file", "-e", oid], &[])?.status == 0 {
                    objects.push(oid.to_owned());
                }
            }
            false
        }
        _ => {
            objects.extend_from_slice(fallback);
            true
        }
    };
    objects.sort();
    objects.dedup();
    Ok(OriginExclusions { objects, stale })
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
            })
        }
    }
}
fn config_values(runner: &dyn GitCommandRunner, checkout: &Path, key: &str) -> Result<Vec<String>> {
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
fn selected_refs(
    cut: &PinnedWorkspaceCut,
    ownership: &CheckoutOwnership,
) -> BTreeMap<String, String> {
    let launch = match ownership {
        CheckoutOwnership::Raw { launch } => launch.as_ref(),
        CheckoutOwnership::Managed => None,
    };
    let mut refs: BTreeMap<_, _> = cut.refs.iter().filter(|(name, oid)| !name.starts_with("refs/remotes/origin/") && (!matches!(ownership, CheckoutOwnership::Raw { .. }) || matches!(&cut.head, SnapshotHead::Branch { name: branch, .. } if branch == *name) || launch.is_none_or(|launch| launch.refs.get(*name) != Some(*oid)))).map(|(name, oid)| (name.clone(), oid.clone())).collect();
    for head in std::iter::once(&cut.head).chain(cut.worktrees.iter().map(|tree| &tree.head)) {
        if let SnapshotHead::Branch { name, commit } = head {
            refs.insert(name.clone(), commit.clone());
        }
    }
    refs
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
/// Estimate Git pack bytes without changing refs, aborting or fetching. Synthetic
/// stash/commit objects may be written; the working tree and stash list stay put.
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
    let refs = selected_refs(&cut, ownership);
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
    pub id: String,
    pub checkout: PathBuf,
    pub ownership: CheckoutOwnership,
    pub origin_override: Option<String>,
    pub pack_path: PathBuf,
    #[serde(default)]
    pub exclusions: ExclusionSource,
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
/// Inventory every repository and decide consent for the complete session before
/// the first abort or other mutation. Paths inside consent remain repository-relative.
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
        let inventory = workspace_inventory_scoped(
            runner,
            &repository.checkout,
            &repository.ownership,
            &repository_key(&repository.id),
        )
        .map_err(|error| session_error(&repository.id, error))?;
        inventories.insert(repository.id.clone(), inventory);
    }
    for repository in repositories {
        check_consent(
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
        let inventory = workspace_inventory_scoped(
            runner,
            &repository.checkout,
            &repository.ownership,
            &repository_key(&repository.id),
        )
        .map_err(|error| session_error(&repository.id, error))?;
        check_consent(
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
        id: id.clone(),
        checkout: checkout.to_path_buf(),
        ownership: ownership.clone(),
        origin_override,
        pack_path: pack_path.to_path_buf(),
        exclusions: ExclusionSource::Origin,
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
    let inventory = prepare_capture(runner, checkout, ownership, consent, key)?;
    let skipped = inventory.raw && inventory.operation.is_some();
    let configured = run_git(runner, checkout, ["remote", "get-url", "origin"], &[])?;
    let has_origin = configured.status == 0;
    let original_origin = if has_origin {
        Some(String::from_utf8(configured.stdout)?.trim().to_owned())
    } else {
        origin_override
    };
    let origin = original_origin
        .as_deref()
        .map(|remote| snapshot_remote(checkout, remote))
        .transpose()?;
    ensure!(
        origin.is_some() || matches!(ownership, CheckoutOwnership::Raw { .. }),
        "managed snapshot requires an origin remote or source override"
    );
    if origin.is_none() {
        return Ok(WorkspaceSnapshot {
            origin: None,
            push_urls: Vec::new(),
            head: saved_head(runner, checkout, skipped)?,
            refs: BTreeMap::new(),
            branch_tracking: BTreeMap::new(),
            stash: Vec::new(),
            working_state: None,
            working_state_skipped: skipped,
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
        original_origin
            .as_deref()
            .context("missing snapshot origin")?
    };
    let push_urls = if has_origin {
        git_text(
            runner,
            checkout,
            ["remote", "get-url", "--push", "--all", "origin"],
        )?
        .lines()
        .map(|remote| snapshot_remote(checkout, remote))
        .collect::<Result<Vec<_>>>()?
    } else {
        origin.iter().cloned().collect()
    };
    let remote_refs_stale = if !owns_git_directory(runner, checkout, ownership)?
        || matches!(source, ExclusionSource::Commits(_))
    {
        false
    } else {
        // Only our origin comparison refs may be refreshed or pruned. User
        // pruneTags settings and tag fetch refspecs must not delete private tags.
        !run_git(
            runner,
            checkout,
            [
                "-c",
                "fetch.pruneTags=false",
                "-c",
                "remote.origin.pruneTags=false",
                "fetch",
                "--prune",
                "--no-tags",
                remote,
                "+refs/heads/*:refs/remotes/origin/*",
            ],
            &[],
        )
        .is_ok_and(|o| o.status == 0)
    };
    // Barrier-free exports may race a ref update. Three cuts give a moving
    // checkout a chance to settle, then report instability to its caller.
    for attempt in 0..3 {
        let inventory = prepare_capture(runner, checkout, ownership, consent, key)?;
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
            worktrees.push(SnapshotWorktree {
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
        let refs = selected_refs(&cut, ownership);
        let stash = original_stash
            .iter()
            .map(|entry| tracked_stash(runner, checkout, entry))
            .collect::<Result<Vec<_>>>()?;
        tips.extend(refs.values().cloned());
        tips.extend(stash.iter().map(|entry| entry.commit.clone()));
        tips.sort();
        tips.dedup();
        let exclusions = snapshot_exclusions(runner, checkout, remote, pinned, source)?;
        let pack = write_snapshot_pack(runner, checkout, &tips, &exclusions.objects, pack_path)?;
        check_consent(
            &workspace_inventory_scoped(runner, checkout, ownership, key)?,
            consent,
        )?;
        return Ok(WorkspaceSnapshot {
            origin,
            push_urls,
            head: head.clone(),
            branch_tracking: branch_tracking(runner, checkout, &refs)?,
            refs,
            stash,
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
            remote_refs_stale: remote_refs_stale
                || exclusions.stale
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
            workspace_inventory_scoped(
                runner,
                &repository.checkout,
                &repository.ownership,
                &repository_key(&repository.id),
            )?,
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
        for path in paths {
            if nested.iter().any(|nested| path.starts_with(nested)) {
                continue;
            }
            let bytes = fs::symlink_metadata(checkout.join(&path))
                .with_context(|| format!("inspect {}", path.display()))?
                .len();
            untracked.push(UntrackedPath {
                checkout: key.clone(),
                path,
                bytes,
            });
        }
        Ok(WorkspaceInventory {
            checkout: key.clone(),
            untracked,
            untracked_stashes: Vec::new(),
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
            hidden_tracked_edits: crate::archive::git::hidden_tracked_paths(runner, checkout)?
                .into_iter()
                .map(|path| scoped(key, path))
                .collect(),
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
    for tree in nested
        .into_iter()
        .filter(|tree| owns_worktree(ownership, &tree.relative))
    {
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
                    "update".into(),
                    "--init".into(),
                    "--".into(),
                    relative.as_os_str().to_owned(),
                ],
                ..Default::default()
            },
        )?;
        ensure!(
            output.status == 0,
            "{}",
            git_failure("restore initialized submodule", &output)
        );
        let directory = checkout.join(&submodule.path);
        if run_git(
            runner,
            &directory,
            ["cat-file", "-e", &submodule.commit],
            &[],
        )?
        .status
            != 0
        {
            git_text(runner, &directory, ["fetch", "origin", &submodule.commit])?;
        }
        require_object(
            runner,
            &directory,
            &format!("submodule {} HEAD", submodule.path.display()),
            &submodule.commit,
        )?;
        git_text(
            runner,
            &directory,
            ["checkout", "--detach", &submodule.commit],
        )?;
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
                snapshot.origin.is_some(),
                "cannot restore snapshot without an origin remote; resume on its raw checkout"
            );
            let parent = destination.parent().context("destination has no parent")?;
            fs::create_dir_all(parent)?;
            let seed = seed.as_deref().map(std::path::absolute).transpose()?;
            let origin = snapshot_remote(&std::env::current_dir()?, origin)?;
            let mut arguments: Vec<OsString> = vec!["clone".into(), "--no-checkout".into()];
            if let Some(seed) = &seed {
                arguments.extend(["--reference-if-able".into(), seed.as_os_str().to_owned()]);
            }
            arguments.extend([
                "--".into(),
                origin.into(),
                destination.as_os_str().to_owned(),
            ]);
            let output = runner.run(
                parent,
                &GitCommand {
                    arguments,
                    ..Default::default()
                },
            )?;
            ensure!(
                output.status == 0,
                "{}",
                git_failure("clone snapshot origin", &output)
            );
            if seed.is_some() {
                git_text(runner, destination, ["repack", "-a", "-d"])?;
                let alternates = git_path(runner, destination, "objects/info/alternates")?;
                match fs::remove_file(alternates) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            git_text(runner, destination, ["fetch", "--tags", "origin"])?;
            true
        }
        RestoreBase::Provisioned { allow_origin_fetch } => {
            git_text(runner, destination, ["rev-parse", "--is-inside-work-tree"])?;
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
    // Local heads are snapshot-owned. Origin tags remain available to raw
    // conversions, except where a saved tag replaces or prefix-conflicts with one.
    let cloned_refs = git_text(
        runner,
        destination,
        [
            "for-each-ref",
            "--format=%(refname)%00%(objectname)",
            "refs/heads",
            "refs/tags",
            "refs/notes",
        ],
    )?;
    for line in cloned_refs.lines() {
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
        if name.starts_with("refs/heads/") || conflict {
            git_text(runner, destination, ["update-ref", "-d", name])?;
            if let Some(branch) = name.strip_prefix("refs/heads/") {
                unset_config(runner, destination, &format!("branch.{branch}.remote"))?;
                unset_config(runner, destination, &format!("branch.{branch}.merge"))?;
            }
        }
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
        let mut arguments: Vec<OsString> = vec!["worktree".into(), "add".into()];
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
        restore_submodules(
            runner,
            &path,
            &tree.initialized_submodules,
            (!tree.working_state_skipped).then_some(tree.index_gitlinks.as_slice()),
        )?;
    }
    for url in &snapshot.push_urls {
        git_text(
            runner,
            destination,
            ["config", "--add", "remote.origin.pushurl", url],
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
            command
                .env
                .push(("GIT_ALLOW_PROTOCOL".into(), "file".into()));
            SystemGit.run(repository, &command)
        }
    }
    fn item_path(path: impl Into<PathBuf>) -> UntrackedItem {
        UntrackedItem::Path {
            checkout: CheckoutKey::default(),
            path: path.into(),
        }
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
            origin: snapshot.origin.clone().unwrap_or_default(),
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
            let temp = tempfile::tempdir().unwrap();
            let origin = temp.path().join("origin.git");
            let checkout = temp.path().join("checkout");
            git(
                temp.path(),
                &[
                    "init",
                    "--bare",
                    "--initial-branch=main",
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
                &SystemGit,
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
            restore_workspace_snapshot(&SystemGit, snapshot, &path, &clone_base(snapshot, seed))
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
            let output = run_git(&SystemGit, &self.checkout, [kind, "side"], &[]).unwrap();
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
        let before = read_stash(&SystemGit, &r.checkout).unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(read_stash(&SystemGit, &r.checkout).unwrap(), before);
        let restored = r.restore(&snapshot, None);
        assert_eq!(read_stash(&SystemGit, &restored).unwrap(), before);
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
            run_git(&SystemGit, &r.checkout, ["stash", "create"], &[])
                .unwrap()
                .status,
            0
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let inventory =
            workspace_inventory(&SystemGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(
            inventory.untracked,
            vec![UntrackedPath {
                checkout: CheckoutKey::default(),
                path: "untracked\n spaced".into(),
                bytes: 4
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
            workspace_inventory(&SystemGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
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
            estimate_delta(&SystemGit, &r.checkout, &CheckoutOwnership::Managed, None).unwrap();
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
    fn raw_launch_excludes_preexisting_user_refs_stashes_and_worktrees() {
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
        record_launch(&SystemGit, &r.checkout, "session-1").unwrap();
        fs::write(existing.join("user-untracked"), "not session work").unwrap();
        let launch = LaunchRecord::read(&SystemGit, &r.checkout, "session-1")
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
        assert!(snapshot.worktrees.is_empty());
        let owned = CheckoutOwnership::Raw {
            launch: LaunchRecord::read(&SystemGit, &r.checkout, "session-1").unwrap(),
        };
        assert!(
            workspace_inventory(&SystemGit, &r.checkout, &owned)
                .unwrap()
                .worktrees
                .is_empty()
        );
        capture_workspace_snapshot(
            &SystemGit,
            &r.checkout,
            &owned,
            &r.temp.path().join("second.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap();
        let restored = r.restore(&snapshot, None);
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
            &SystemGit,
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
            &SystemGit,
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
            operation(&SystemGit, &r.checkout).unwrap(),
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
            operation(&SystemGit, &r.checkout).unwrap(),
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
            read_head(&SystemGit, &restored).unwrap(),
            SnapshotHead::Detached(_)
        ));
    }
    #[test]
    fn raw_without_origin_is_in_place_only() {
        let r = Repo::new();
        git(&r.checkout, &["remote", "remove", "origin"]);
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(snapshot.origin.is_none());
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
            &SystemGit,
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
            &SystemGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Shown(vec![item_path("shown")]),
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        match error.downcast_ref::<SnapshotRefused>().unwrap() {
            SnapshotRefused::Untracked(paths) => {
                assert_eq!(paths, &vec![item_path("new")])
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
        record_launch(&SystemGit, &r.checkout, "raw-stash").unwrap();
        let ownership = CheckoutOwnership::Raw {
            launch: LaunchRecord::read(&SystemGit, &r.checkout, "raw-stash").unwrap(),
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
        let original = read_stash(&SystemGit, &r.checkout).unwrap();
        let estimate = estimate_delta(&SystemGit, &r.checkout, &ownership, None).unwrap();
        assert!(
            estimate.total_bytes.unwrap() < 65_536,
            "scratch must not inflate the real transfer pack"
        );
        let error = capture_workspace_snapshot(
            &SystemGit,
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
            &SystemGit,
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
        assert_eq!(read_stash(&SystemGit, &r.checkout).unwrap(), original);
        let restored = r.restore(&snapshot, None);
        assert!(!stash_has_untracked(&SystemGit, &restored, &snapshot.stash[0].commit).unwrap());
        let missing = run_git(
            &SystemGit,
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
            &SystemGit,
            &nested,
            ["cherry-pick", &independent, &first, &second],
            &[],
        )
        .unwrap();
        assert_ne!(output.status, 0);
        assert_ne!(git(&nested, &["rev-parse", "HEAD"]), before);
        let raw = capture_workspace_snapshot(
            &SystemGit,
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
        restore_workspace_snapshot(&SystemGit, &raw, &raw_destination, &clone_base(&raw, None))
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
        record_launch(&SystemGit, &r.checkout, "raw-1").unwrap();
        let launch = LaunchRecord::read(&SystemGit, &r.checkout, "raw-1").unwrap();
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
        assert!(Path::new(snapshot.origin.as_ref().unwrap()).is_absolute());
        assert!(Path::new(&snapshot.push_urls[0]).is_absolute());
        let inside = tempfile::Builder::new()
            .prefix("snapshot-relative-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let destination = PathBuf::from(inside.path().file_name().unwrap()).join("restored");
        restore_workspace_snapshot(
            &SystemGit,
            &snapshot,
            &destination,
            &clone_base(&snapshot, None),
        )
        .unwrap();
        assert_eq!(
            git(&destination, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
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
            snapshot.origin.as_deref(),
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
        fs::write(nested.join("file"), "dirty tracked\n").unwrap();
        let inventory =
            workspace_inventory(&SystemGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
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
                &SystemGit,
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
        git(&r.checkout, &["add", "sub"]);
        let staged = capture().unwrap();
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
            &SystemGit,
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
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items)) if items == &vec![item_path("sub/untracked")])
        );
        capture_workspace_snapshot(
            &SystemGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("shown-scratch.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Shown(vec![item_path("sub/untracked")]),
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
            &SystemGit,
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
        record_launch(&SystemGit, &r.checkout, "raw-tree").unwrap();
        let ownership = CheckoutOwnership::Raw {
            launch: LaunchRecord::read(&SystemGit, &r.checkout, "raw-tree").unwrap(),
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
        assert_eq!(read_stash(&SystemGit, &r.checkout).unwrap().len(), 1);
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
                        &SystemGit,
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
            workspace_inventory(&SystemGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(
            inventory.hidden_tracked_edits,
            vec![scoped(&CheckoutKey::default(), PathBuf::from("hidden"))]
        );
        let error = capture_workspace_snapshot(
            &SystemGit,
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
            read_head(&SystemGit, &restored).unwrap(),
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
        let shown = workspace_inventory(&SystemGit, &r.checkout, &CheckoutOwnership::Managed)
            .unwrap()
            .operation
            .unwrap();
        git(&r.checkout, &["merge", "--abort"]);
        assert_ne!(
            run_git(&SystemGit, &r.checkout, ["merge", "alternate"], &[])
                .unwrap()
                .status,
            0
        );
        let before = fs::read(r.checkout.join("file")).unwrap();
        let error = capture_workspace_snapshot(
            &SystemGit,
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
            let current = workspace_inventory(&SystemGit, &r.checkout, &CheckoutOwnership::Managed)
                .unwrap()
                .operation
                .unwrap();
            assert_ne!(current.identity, shown.identity);
            let error = capture_workspace_snapshot(
                &SystemGit,
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
                id: id.into(),
                checkout: repo.checkout.clone(),
                ownership: CheckoutOwnership::Managed,
                origin_override: None,
                pack_path: repo.temp.path().join("snapshot.pack"),
                exclusions: ExclusionSource::Origin,
            })
            .collect();
        let consent = SessionCaptureConsent {
            repositories: BTreeMap::from([
                (
                    "a".into(),
                    CaptureConsent {
                        untracked: UntrackedConsent::Shown(vec![UntrackedItem::Path {
                            checkout: repository_key("a"),
                            path: "notes.txt".into(),
                        }]),
                        operations: OperationConsent::Any,
                    },
                ),
                (
                    "b".into(),
                    CaptureConsent {
                        untracked: UntrackedConsent::Shown(vec![UntrackedItem::Path {
                            checkout: repository_key("a"),
                            path: "notes.txt".into(),
                        }]),
                        operations: OperationConsent::Any,
                    },
                ),
            ]),
        };
        let error = capture_session_snapshot(&SystemGit, &repositories, &consent).unwrap_err();
        let refusal = error.downcast_ref::<SnapshotRefused>().unwrap();
        assert!(
            matches!(refusal, SnapshotRefused::Untracked(items) if items == &vec![UntrackedItem::Path { checkout: repository_key("b"), path: "notes.txt".into() }])
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
            &SystemGit,
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
        struct WritingGit;
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
                    fs::write(repository.join("new-during-pack"), "new").unwrap();
                }
                Ok(output)
            }
        }
        let r = Repo::new();
        let error = capture_workspace_snapshot(
            &WritingGit,
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
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items)) if items == &vec![item_path("new-during-pack")])
        );
    }
    #[test]
    fn cutoff_exclusions_pack_published_session_work_without_origin_fetch() {
        let r = Repo::new();
        let cutoff = git(&r.checkout, &["rev-parse", "HEAD"]);
        r.commit("published beyond cutoff\n");
        git(&r.checkout, &["push", "origin", "main"]);
        let repository = RepositoryInput {
            id: "cutoff".into(),
            checkout: r.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: r.temp.path().join("cutoff.pack"),
            exclusions: ExclusionSource::Commits(vec![cutoff.clone()]),
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
        let estimate = estimate_repository_delta(&SystemGit, &repository).unwrap();
        let session = capture_session_snapshot(&SystemGit, &[repository], &consent).unwrap();
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
            &SystemGit,
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
        capture_workspace_snapshot(
            &SystemGit,
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
            read_stash(&SystemGit, &destination).unwrap()[0].message,
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
            &SystemGit,
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
        let original = read_stash(&SystemGit, &r.checkout).unwrap();
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
}
