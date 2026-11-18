//! Origin-relative Git snapshots, packing pinned objects without temporary refs.
//! Filesystem paths belong to the machine running the supplied Git runner.

use crate::archive::git::{
    GitCommand, GitCommandRunner, discover_linked_worktrees, git_bytes, git_failure, git_text,
    run_git,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SnapshotHead {
    Branch(String),
    Detached(String),
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotStashEntry {
    pub commit: String,
    pub message: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotWorktree {
    pub relative_path: PathBuf,
    pub head: SnapshotHead,
    pub working_state: Option<String>,
    pub working_state_skipped: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub origin: Option<String>,
    pub push_urls: Vec<String>,
    pub head: SnapshotHead,
    pub refs: BTreeMap<String, String>,
    pub stash: Vec<SnapshotStashEntry>,
    pub working_state: Option<String>,
    pub working_state_skipped: bool,
    pub worktrees: Vec<SnapshotWorktree>,
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
    Path(PathBuf),
    Stash { commit: String, message: String },
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
    Shown(Vec<(PathBuf, GitOperationKind)>),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureConsent {
    pub untracked: UntrackedConsent,
    pub operations: OperationConsent,
}
#[derive(Debug)]
pub enum SnapshotRefused {
    Untracked(Vec<UntrackedItem>),
    OperationInProgress(PathBuf, GitOperationKind),
}
impl std::fmt::Display for SnapshotRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OperationInProgress(path, kind) => {
                write!(
                    f,
                    "{kind:?} in progress at {}; abort consent is required",
                    path.display()
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
                        UntrackedItem::Path(path) => write!(f, " {}", path.display())?,
                        UntrackedItem::Stash { commit, message } => {
                            write!(f, " stash {commit} ({message})")?
                        }
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
pub struct UntrackedPath {
    pub path: PathBuf,
    pub bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInventory {
    pub untracked: Vec<UntrackedPath>,
    pub untracked_stashes: Vec<SnapshotStashEntry>,
    pub operation: Option<GitOperationKind>,
    pub raw: bool,
    pub worktrees: Vec<(PathBuf, WorkspaceInventory)>,
}

fn git_path(runner: &dyn GitCommandRunner, checkout: &Path, name: &str) -> Result<PathBuf> {
    let bytes = git_bytes(
        runner,
        checkout,
        ["rev-parse", "--path-format=absolute", "--git-path", name],
        &[],
        "resolve Git path",
    )?;
    // Strip only Git's terminating newline: spaces in paths are significant.
    mj_core::path_input::from_git_bytes(bytes.strip_suffix(b"\n").unwrap_or(&bytes))
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
    let bytes = git_bytes(
        runner,
        checkout,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        &[],
        "resolve common Git directory",
    )?;
    Ok(
        mj_core::path_input::from_git_bytes(bytes.strip_suffix(b"\n").unwrap_or(&bytes))?
            .join("mj/sessions")
            .join(session_id)
            .join("launch"),
    )
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
                !name.starts_with("refs/mj/snapshot/")
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
/// tree and index parents, preserving identity and message for a stable object ID.
fn tracked_stash(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    entry: &SnapshotStashEntry,
) -> Result<SnapshotStashEntry> {
    if !stash_has_untracked(runner, checkout, &entry.commit)? {
        return Ok(entry.clone());
    }
    let metadata = git_bytes(
        runner,
        checkout,
        [
            "show",
            "-s",
            "--format=%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%B",
            &entry.commit,
        ],
        &[],
        "read stash commit metadata",
    )?;
    let mut fields = metadata.splitn(7, |byte| *byte == 0);
    let mut env = Vec::new();
    for name in [
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_AUTHOR_DATE",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_COMMITTER_DATE",
    ] {
        env.push((
            name.into(),
            String::from_utf8(
                fields
                    .next()
                    .context("missing stash identity field")?
                    .to_vec(),
            )?
            .into(),
        ));
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
    let message = raw[message_start..].to_vec();
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
            env,
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
        0 => Ok(SnapshotHead::Branch(
            String::from_utf8(symbolic.stdout)?.trim().to_owned(),
        )),
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
    let state = git_text(runner, checkout, ["stash", "create"])?;
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
                            return Ok(SnapshotHead::Branch(name));
                        }
                        let oid =
                            fs::read_to_string(git_path(runner, checkout, dir)?.join("orig-head"))?;
                        return Ok(SnapshotHead::Detached(oid.trim().to_owned()));
                    }
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
                    Ok(SnapshotHead::Branch(branch))
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
fn head_object(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    head: &SnapshotHead,
) -> Result<String> {
    match head {
        SnapshotHead::Branch(name) => git_text(runner, checkout, ["rev-parse", name]),
        SnapshotHead::Detached(oid) => Ok(oid.clone()),
    }
}
fn check_consent(
    inventory: &WorkspaceInventory,
    consent: &CaptureConsent,
) -> Result<Vec<(PathBuf, GitOperationKind)>> {
    let mut untracked = Vec::new();
    let mut operations = Vec::new();
    let mut collect =
        |path: &Path, inventory: &WorkspaceInventory| {
            untracked.extend(
                inventory
                    .untracked
                    .iter()
                    .map(|entry| UntrackedItem::Path(path.join(&entry.path))),
            );
            untracked.extend(inventory.untracked_stashes.iter().map(|entry| {
                UntrackedItem::Stash {
                    commit: entry.commit.clone(),
                    message: entry.message.clone(),
                }
            }));
            if let Some(kind) = inventory.operation {
                operations.push((path.to_path_buf(), kind));
            }
        };
    collect(Path::new(""), inventory);
    for (path, inventory) in &inventory.worktrees {
        collect(path, inventory);
    }
    untracked.retain(|entry| match &consent.untracked {
        UntrackedConsent::None => true,
        UntrackedConsent::Any => false,
        UntrackedConsent::Shown(items) => !items.contains(entry),
    });
    ensure!(untracked.is_empty(), SnapshotRefused::Untracked(untracked));
    if !inventory.raw {
        for (path, kind) in &operations {
            let covered = match &consent.operations {
                OperationConsent::None => false,
                OperationConsent::Any => true,
                OperationConsent::Shown(shown) => shown.contains(&(path.clone(), *kind)),
            };
            ensure!(
                covered,
                SnapshotRefused::OperationInProgress(path.clone(), *kind)
            );
        }
    }
    Ok(operations)
}
/// Inventory, consent and all aborts precede the pinned cut. Concurrent ref or
/// HEAD changes retry the complete cut; capture never creates temporary refs.
/// Raw callers load their session's launch record with [`LaunchRecord::read`].
pub fn capture_workspace_snapshot(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
    pack_path: &Path,
    consent: &CaptureConsent,
) -> Result<WorkspaceSnapshot> {
    let inventory = workspace_inventory(runner, checkout, ownership)?;
    let operations = check_consent(&inventory, consent)?;
    let nested = discover_linked_worktrees(runner, checkout)?;
    let launch = match ownership {
        CheckoutOwnership::Raw { launch } => launch.as_ref(),
        CheckoutOwnership::Managed => None,
    };
    let nested: Vec<_> = nested
        .into_iter()
        .filter(|tree| {
            !matches!(ownership, CheckoutOwnership::Raw { .. })
                || launch.is_some_and(|l| !l.worktrees.contains(&tree.relative))
        })
        .collect();
    let skipped = prepare_operation(runner, checkout, ownership, inventory.operation)?;
    let mut prepared = Vec::new();
    for tree in nested {
        let kind = operations
            .iter()
            .find(|(path, _)| path == &tree.relative)
            .map(|(_, kind)| *kind);
        let skipped = prepare_operation(runner, &tree.directory, ownership, kind)?;
        prepared.push((tree, skipped));
    }
    let origin = run_git(runner, checkout, ["remote", "get-url", "origin"], &[])?;
    let origin = if origin.status == 0 {
        Some(String::from_utf8(origin.stdout)?.trim().to_owned())
    } else {
        ensure!(
            matches!(ownership, CheckoutOwnership::Raw { .. }),
            "managed snapshot requires an origin remote"
        );
        None
    };
    if origin.is_none() {
        return Ok(WorkspaceSnapshot {
            origin: None,
            push_urls: Vec::new(),
            head: saved_head(runner, checkout, skipped)?,
            refs: BTreeMap::new(),
            stash: Vec::new(),
            working_state: None,
            working_state_skipped: skipped,
            worktrees: Vec::new(),
            remote_refs_stale: false,
            pack: None,
        });
    }
    let push_urls = git_text(
        runner,
        checkout,
        ["remote", "get-url", "--push", "--all", "origin"],
    )?
    .lines()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let fetched = run_git(runner, checkout, ["fetch", "--prune", "origin"], &[]);
    let mut remote_refs_stale = !fetched.is_ok_and(|o| o.status == 0);
    let tags = match run_git(runner, checkout, ["ls-remote", "--tags", "origin"], &[]) {
        Ok(output) if output.status == 0 => String::from_utf8(output.stdout)?
            .lines()
            .filter_map(|line| {
                line.split_once('\t')
                    .map(|(oid, name)| (name.to_owned(), oid.to_owned()))
            })
            .collect::<BTreeMap<_, _>>(),
        _ => {
            remote_refs_stale = true;
            BTreeMap::new()
        }
    };
    // Barrier-free exports may race a ref update. Three cuts give a moving
    // checkout a chance to settle, then report instability to its caller.
    for attempt in 0..3 {
        check_consent(&workspace_inventory(runner, checkout, ownership)?, consent)?;
        let pinned = read_all_refs(runner, checkout)?;
        let head = saved_head(runner, checkout, skipped)?;
        let head_oid = head_object(runner, checkout, &head)?;
        let mut trees = Vec::new();
        for (tree, skipped) in &prepared {
            let head = saved_head(runner, &tree.directory, *skipped)?;
            let oid = head_object(runner, &tree.directory, &head)?;
            trees.push((tree, *skipped, head, oid));
        }
        let original_stash = selected_stash(runner, checkout, ownership)?;
        let working = working_state(runner, checkout, skipped)?;
        let mut worktrees = Vec::new();
        let mut tips = vec![head_oid.clone()];
        tips.extend(working.clone());
        let mut stable = true;
        let mut verify_state = |path: &Path, state: &Option<String>, oid: &str| -> Result<()> {
            if let Some(state) = state {
                stable &= git_text(runner, path, ["rev-parse", &format!("{state}^1")])? == oid;
            }
            Ok(())
        };
        verify_state(checkout, &working, &head_oid)?;
        for (tree, skipped, head, oid) in &trees {
            let state = working_state(runner, &tree.directory, *skipped)?;
            verify_state(&tree.directory, &state, oid)?;
            tips.push(oid.clone());
            tips.extend(state.clone());
            worktrees.push(SnapshotWorktree {
                relative_path: tree.relative.clone(),
                head: head.clone(),
                working_state: state,
                working_state_skipped: *skipped,
            });
        }
        stable &= read_all_refs(runner, checkout)? == pinned;
        stable &= selected_stash(runner, checkout, ownership)? == original_stash;
        stable &= saved_head(runner, checkout, skipped)? == head
            && head_object(runner, checkout, &head)? == head_oid;
        for (tree, skipped, head, oid) in &trees {
            stable &= saved_head(runner, &tree.directory, *skipped)? == *head
                && head_object(runner, &tree.directory, head)? == *oid;
        }
        if !stable {
            if attempt < 2 {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            anyhow::bail!(
                "workspace refs or HEAD kept changing during snapshot capture; retry after the checkout is idle"
            );
        }
        let mut refs: BTreeMap<_, _> = pinned
            .iter()
            .filter(|(name, oid)| {
                !name.starts_with("refs/remotes/origin/")
                    && (!matches!(ownership, CheckoutOwnership::Raw { .. })
                        || matches!(&head, SnapshotHead::Branch(branch) if branch == *name)
                        || launch.is_some_and(|l| l.refs.get(*name) != Some(*oid)))
            })
            .map(|(name, oid)| (name.clone(), oid.clone()))
            .collect();
        for (_, _, head, oid) in &trees {
            if let SnapshotHead::Branch(name) = head {
                refs.insert(name.clone(), oid.clone());
            }
        }
        let stash = original_stash
            .iter()
            .map(|entry| tracked_stash(runner, checkout, entry))
            .collect::<Result<Vec<_>>>()?;
        tips.extend(refs.values().cloned());
        tips.extend(stash.iter().map(|entry| entry.commit.clone()));
        tips.sort();
        tips.dedup();
        let mut revisions = tips.join("\n");
        revisions.push_str("\n--not\n");
        for (name, oid) in &pinned {
            if name.starts_with("refs/remotes/origin/")
                || (name.starts_with("refs/tags/") && tags.get(name) == Some(oid))
            {
                revisions.push_str(oid);
                revisions.push('\n');
            }
        }
        let output = runner.run(
            checkout,
            &GitCommand {
                arguments: vec!["pack-objects".into(), "--revs".into(), "--stdout".into()],
                stdin: revisions.into_bytes(),
                ..Default::default()
            },
        )?;
        ensure!(
            output.status == 0,
            "{}",
            git_failure("pack snapshot objects", &output)
        );
        ensure!(
            output.stdout.len() >= 12 && output.stdout.starts_with(b"PACK"),
            "Git returned an invalid snapshot pack"
        );
        // The pack header's big-endian object count distinguishes the valid empty pack.
        let pack = if output.stdout[8..12] == [0, 0, 0, 0] {
            None
        } else {
            fs::write(pack_path, &output.stdout)
                .with_context(|| format!("write snapshot pack {}", pack_path.display()))?;
            Some(pack_path.to_path_buf())
        };
        return Ok(WorkspaceSnapshot {
            origin,
            push_urls,
            head,
            refs,
            stash,
            working_state: working,
            working_state_skipped: skipped,
            worktrees,
            remote_refs_stale,
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
    fn collect(
        runner: &dyn GitCommandRunner,
        checkout: &Path,
        raw: bool,
        nested: &[PathBuf],
    ) -> Result<WorkspaceInventory> {
        let bytes = git_bytes(
            runner,
            checkout,
            ["ls-files", "--others", "--exclude-standard", "-z"],
            &[],
            "list untracked files",
        )?;
        let mut untracked = Vec::new();
        for path in bytes.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            let path = mj_core::path_input::from_git_bytes(path)?;
            if nested.iter().any(|n| path.starts_with(n)) {
                continue;
            }
            let bytes = fs::symlink_metadata(checkout.join(&path))
                .with_context(|| format!("inspect {}", path.display()))?
                .len();
            untracked.push(UntrackedPath { path, bytes });
        }
        Ok(WorkspaceInventory {
            untracked,
            untracked_stashes: Vec::new(),
            operation: operation(runner, checkout)?,
            raw,
            worktrees: Vec::new(),
        })
    }
    let nested = discover_linked_worktrees(runner, checkout)?;
    let paths: Vec<_> = nested.iter().map(|w| w.relative.clone()).collect();
    let raw = matches!(ownership, CheckoutOwnership::Raw { .. });
    let mut result = collect(runner, checkout, raw, &paths)?;
    for entry in selected_stash(runner, checkout, ownership)? {
        if stash_has_untracked(runner, checkout, &entry.commit)? {
            result.untracked_stashes.push(entry);
        }
    }
    for tree in nested {
        let children: Vec<_> = paths
            .iter()
            .filter_map(|p| p.strip_prefix(&tree.relative).ok())
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .collect();
        result.worktrees.push((
            tree.relative,
            collect(runner, &tree.directory, raw, &children)?,
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
    let oid = head_object(runner, destination, head)?;
    require_object(runner, destination, "HEAD", &oid)?;
    match head {
        SnapshotHead::Branch(name) => {
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
                ["update-ref", "--no-deref", "HEAD", &oid],
            )?;
        }
    }
    git_text(runner, destination, ["reset", "--hard", &oid])?;
    Ok(())
}
pub fn restore_workspace_snapshot(
    runner: &dyn GitCommandRunner,
    snapshot: &WorkspaceSnapshot,
    destination: &Path,
    seed: Option<&Path>,
) -> Result<()> {
    let origin = snapshot
        .origin
        .as_deref()
        .context("cannot restore snapshot without an origin remote; resume on its raw checkout")?;
    let parent = destination.parent().context("destination has no parent")?;
    fs::create_dir_all(parent)?;
    let mut arguments: Vec<OsString> = vec!["clone".into(), "--no-checkout".into()];
    if let Some(seed) = seed {
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    git_text(runner, destination, ["fetch", "--tags", "origin"])?;
    if let Some(pack) = &snapshot.pack {
        let output = runner.run(
            destination,
            &GitCommand {
                arguments: vec!["index-pack".into(), "--stdin".into()],
                stdin: fs::read(pack)
                    .with_context(|| format!("read snapshot pack {}", pack.display()))?,
                ..Default::default()
            },
        )?;
        ensure!(
            output.status == 0,
            "{}",
            git_failure("import snapshot pack", &output)
        );
    }
    for (name, oid) in &snapshot.refs {
        require_object(runner, destination, name, oid)?;
        git_text(runner, destination, ["update-ref", name, oid])?;
    }
    for entry in snapshot.stash.iter().rev() {
        require_object(runner, destination, "stash", &entry.commit)?;
        git_text(
            runner,
            destination,
            ["stash", "store", "-m", &entry.message, &entry.commit],
        )?;
    }
    checkout_head(runner, destination, &snapshot.head)?;
    if let Some(state) = &snapshot.working_state {
        require_object(runner, destination, "working state", state)?;
        git_text(runner, destination, ["stash", "apply", "--index", state])?;
    }
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
        let head_oid = head_object(runner, destination, &tree.head)?;
        require_object(
            runner,
            destination,
            &format!("worktree {} HEAD", tree.relative_path.display()),
            &head_oid,
        )?;
        let rev = match &tree.head {
            SnapshotHead::Branch(name) => name
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
            )
            .unwrap()
        }
        fn restore(&self, snapshot: &WorkspaceSnapshot, seed: Option<&Path>) -> PathBuf {
            let path = self.temp.path().join("restored");
            restore_workspace_snapshot(&SystemGit, snapshot, &path, seed).unwrap();
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
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_none());
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
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_some());
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
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let inventory =
            workspace_inventory(&SystemGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(
            inventory.untracked,
            vec![UntrackedPath {
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
        git(&r.checkout, &["branch", "user-branch"]);
        fs::write(r.checkout.join("file"), "old stash").unwrap();
        git(&r.checkout, &["stash", "push", "-m", "user stash"]);
        let existing = r.checkout.join("user-worktree");
        git(
            &r.checkout,
            &["worktree", "add", "--detach", existing.to_str().unwrap()],
        );
        record_launch(&SystemGit, &r.checkout, "session-1").unwrap();
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
        assert!(snapshot.refs.contains_key("refs/heads/agent-branch"));
        assert_eq!(snapshot.stash.len(), 1);
        assert!(snapshot.stash[0].message.ends_with("agent stash"));
        assert!(snapshot.worktrees.is_empty());
        let restored = r.restore(&snapshot, None);
        assert_eq!(git(&restored, &["branch", "--list", "user-branch"]), "");
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
    #[test]
    fn rewritten_origin_reports_missing_ref_object() {
        let r = Repo::new();
        r.commit("pushed then deleted\n");
        git(&r.checkout, &["push", "origin", "main"]);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_none());
        let base = git(&r.checkout, &["rev-parse", "HEAD~"]);
        git(&r.origin, &["update-ref", "refs/heads/main", &base]);
        git(&r.origin, &["reflog", "expire", "--expire=now", "--all"]);
        git(&r.origin, &["gc", "--prune=now"]);
        let error = restore_workspace_snapshot(
            &SystemGit,
            &snapshot,
            &r.temp.path().join("restored"),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("refs/heads/main"), "{error}");
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
                operations: OperationConsent::Shown(vec![(
                    PathBuf::new(),
                    GitOperationKind::Merge,
                )]),
            },
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::OperationInProgress(
                _,
                GitOperationKind::Rebase
            ))
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
            SnapshotHead::Branch("refs/heads/main".into())
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
            SnapshotHead::Branch("refs/heads/main".into())
        );
        let error = restore_workspace_snapshot(
            &SystemGit,
            &snapshot,
            &r.temp.path().join("restored"),
            None,
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
                untracked: UntrackedConsent::Shown(vec![UntrackedItem::Path("shown".into())]),
                operations: OperationConsent::Any,
            },
        )
        .unwrap_err();
        match error.downcast_ref::<SnapshotRefused>().unwrap() {
            SnapshotRefused::Untracked(paths) => {
                assert_eq!(paths, &vec![UntrackedItem::Path("new".into())])
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
        fs::write(r.checkout.join("file"), "tracked stash\n").unwrap();
        fs::write(r.checkout.join("secret"), "untracked payload").unwrap();
        git(&r.checkout, &["stash", "push", "-u", "-m", "mixed stash"]);
        let original = read_stash(&SystemGit, &r.checkout).unwrap();
        let error = capture_workspace_snapshot(
            &SystemGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items)) if items == &vec![UntrackedItem::Stash { commit: original[0].commit.clone(), message: original[0].message.clone() }])
        );
        let snapshot = capture_workspace_snapshot(
            &SystemGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Shown(vec![UntrackedItem::Stash {
                    commit: original[0].commit.clone(),
                    message: original[0].message.clone(),
                }]),
                operations: OperationConsent::None,
            },
        )
        .unwrap();
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
}
