//! Worker-owned Git capture for turn review.

use super::attribution::{reflog_anchor, review_base};
use anyhow::{Context, Result};
use mj_checkpoint::archive::{
    GitCommand, GitCommandRunner, GitOutput, ReviewPoint, capture_paths, diff_between_trees,
    numstat_between_trees, pin_review_tree, read_head_reflog, read_review_point,
    record_review_point, remove_departed_worktree_pins, review_baseline_ref,
};
use mj_core::refusal::Refusal;
use mj_core::relay::{FileLineChange, RepoDelta};
use mj_review::delta::RawDiffSummary;
#[cfg(test)]
use mj_review::delta::{captured_trees, has_changes};
use mj_review::{REVIEW_CAPTURE_DIFF_LIMIT, bound_review_section};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Captures every repository in `repositories` against `baselines`, recording
/// each capture as `session`'s.
///
/// Runs Git commands only; nothing here modifies an index, a working tree, or
/// any ref other than the capture pin. A repository whose capture fails is
/// reported as an error rather than silently skipped: a review that quietly
/// omits a repository is worse than one that says it could not read it.
pub fn capture_repository_deltas(
    git: &dyn GitCommandRunner,
    repositories: &[PathBuf],
    baselines: &BTreeMap<PathBuf, String>,
    untracked_at_start: &BTreeMap<PathBuf, Vec<UntrackedEntry>>,
    session: &str,
) -> Result<Vec<RepoDelta>> {
    let mut deltas = Vec::new();
    for root in repositories {
        // A controller baseline takes precedence: it is the point a completed
        // review recorded. The worker pin covers a new session before that
        // first review has a controller baseline, and also a fresh target
        // after a resume. Both are usable only while their tree object is
        // still in this repository, and the pin only while no other session
        // has taken this worktree's pin over. A pin with no recorded owner
        // predates owners and is used as it was before.
        let baseline = match baselines
            .get(root)
            .filter(|tree| tree_exists(git, root, tree))
        {
            Some(baseline) => Some(baseline.clone()),
            None => pinned_baseline(git, root, &review_baseline_ref(git, root)?)
                .filter(|(_, point)| {
                    point
                        .session
                        .as_deref()
                        .is_none_or(|owner| owner == session)
                })
                .map(|(id, _)| id),
        };
        // One HEAD for the whole capture: the tree starts from it and the
        // review point records it, so the two cannot disagree. The reflog is
        // read once, for attributing this capture and anchoring the next.
        let head = resolve_head(git, root)?;
        let reflog = read_head_reflog(git, root).map_err(|error| format!("{error:#}"));
        // Capture tracked changes and untracked paths that are new or changed
        // since the session started. A completed review can put untracked files
        // in its baseline tree, so carry forward those that still exist too;
        // otherwise they would look deleted because this capture starts from
        // HEAD. Untracked files from session start that never entered a
        // baseline remain out of the capture.
        let state = read_workspace_state(git, root)?;
        let empty = Vec::new();
        let start = untracked_at_start.get(root).unwrap_or(&empty);
        let mut changed = state.dirty_tracked.clone();
        changed.extend(changed_untracked(start, &state.untracked));
        if let Some(baseline) = &baseline {
            let baseline_paths = tree_paths(git, root, baseline)?;
            changed.extend(
                state
                    .untracked
                    .iter()
                    .filter(|entry| baseline_paths.contains(&entry.path))
                    .map(|entry| entry.path.clone()),
            );
        }
        // The capture starts from HEAD, not from the baseline tree, so a turn
        // that committed its work is still visible: HEAD has moved, and the
        // diff against the baseline shows the commit. Starting from the
        // baseline would have carried the old content forward for every path
        // that `git status` no longer calls dirty.
        let tree = capture_paths(git, root, head.as_deref(), &changed)
            .with_context(|| format!("capture the changed paths of {}", root.display()))?;
        let base = match &baseline {
            Some(baseline) => Some(
                review_base(
                    git,
                    root,
                    baseline,
                    head.as_deref(),
                    reflog.as_deref().map_err(String::as_str),
                    &tree,
                )
                .with_context(|| format!("separate the turn's commits in {}", root.display()))?,
            ),
            None => None,
        };
        // With a baseline, the attribution reports an unreadable reflog.
        if let (Err(error), None) = (&reflog, &base) {
            tracing::warn!(
                repository = %root.display(),
                error = %error,
                "could not read the HEAD reflog; the next review covers every change"
            );
        }
        let current = record_review_point(
            git,
            root,
            &ReviewPoint {
                tree: tree.clone(),
                head: head.clone(),
                reflog: reflog
                    .as_deref()
                    .ok()
                    .and_then(|reflog| reflog_anchor(reflog, head.as_deref())),
                pending: base
                    .as_ref()
                    .map(|base| base.pending.clone())
                    .unwrap_or_default(),
                session: Some(session.to_owned()),
            },
        )
        .with_context(|| format!("record the capture of {}", root.display()))?;
        let (base, patch, files) = match base {
            Some(base) => {
                let (patch, files) = capture_diff(git, root, &base.tree, &tree)?;
                (Some(base), patch, files)
            }
            // Neither baseline is usable. Keep the coverage reset honest: an
            // empty-tree diff would report the restored or pre-existing
            // repository as work from this turn.
            None => (None, String::new(), Vec::new()),
        };
        let (baseline_tree, merged_in, unattributed) = base
            .map_or((None, Vec::new(), None), |base| {
                (Some(base.tree), base.merged_in, base.unattributed)
            });
        deltas.push(build_delta(
            root,
            baseline_tree,
            current,
            patch,
            files,
            merged_in,
            unattributed,
        ));
    }
    Ok(deltas)
}

#[derive(Debug)]
pub struct PushCapture {
    pub delta: RepoDelta,
    pub base: PushBase,
    pub head_commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushBase {
    ReviewPoint {
        commit: String,
    },
    MergeBase {
        default_branch: String,
        commit: String,
    },
    SessionStart {
        commit: String,
    },
}

impl PushBase {
    pub fn commit(&self) -> &str {
        match self {
            Self::ReviewPoint { commit }
            | Self::MergeBase { commit, .. }
            | Self::SessionStart { commit } => commit,
        }
    }
}

#[derive(Debug)]
pub enum PushCaptureRefusal {
    DirtyTracked { files: Vec<PathBuf> },
    NoHead,
    Git(anyhow::Error),
}

impl From<anyhow::Error> for PushCaptureRefusal {
    fn from(error: anyhow::Error) -> Self {
        Self::Git(error)
    }
}

impl std::fmt::Display for PushCaptureRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DirtyTracked { files } => {
                formatter.write_str("commit or stash these tracked changes first:")?;
                for file in files {
                    write!(formatter, "\n  {}", file.display())?;
                }
                Ok(())
            }
            Self::NoHead => formatter.write_str("the repository has no HEAD commit"),
            Self::Git(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for PushCaptureRefusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Git(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

/// Captures committed work only, using the last reviewed commit within the
/// branch's current merge-base boundary.
pub fn capture_pushable_commits(
    runner: &dyn GitCommandRunner,
    root: &Path,
    session_start_head: &str,
) -> std::result::Result<PushCapture, PushCaptureRefusal> {
    let status = checked_git(
        runner,
        root,
        &["status", "--porcelain", "--untracked-files=no", "-z"],
    )?;
    let mut entries = status.split(|byte| *byte == 0);
    let mut files = Vec::new();
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        files.push(PathBuf::from(
            String::from_utf8_lossy(&entry[3..]).into_owned(),
        ));
        if entry[..2].iter().any(|byte| matches!(byte, b'R' | b'C')) {
            entries.next();
        }
    }
    if !files.is_empty() {
        return Err(PushCaptureRefusal::DirtyTracked { files });
    }
    let head = resolve_head(runner, root)?.ok_or(PushCaptureRefusal::NoHead)?;
    let mut base = PushBase::SessionStart {
        commit: session_start_head.to_owned(),
    };
    if let Some(default_branch) = default_branch(runner, root)? {
        let mb = checked_git_text(runner, root, &["merge-base", &head, &default_branch])?;
        if mb != head {
            base = PushBase::MergeBase {
                default_branch,
                commit: mb,
            };
        }
    }
    let refs = checked_git_text(
        runner,
        root,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(objectname)",
            "refs/mj/reviewed/",
        ],
    )?;
    for commit in refs.lines() {
        if is_ancestor(runner, root, commit, &head)?
            && is_ancestor(runner, root, base.commit(), commit)?
        {
            base = PushBase::ReviewPoint {
                commit: commit.to_owned(),
            };
            break;
        }
    }
    let baseline = checked_git_text(
        runner,
        root,
        &[
            "rev-parse",
            "--verify",
            &format!("{}^{{tree}}", base.commit()),
        ],
    )?;
    let current = checked_git_text(
        runner,
        root,
        &["rev-parse", "--verify", &format!("{head}^{{tree}}")],
    )?;
    let (patch, files) = capture_diff(runner, root, &baseline, &current)?;
    let mut delta = build_delta(
        root,
        Some(baseline),
        current,
        patch,
        files,
        Vec::new(),
        None,
    );
    delta.base_commit = Some(base.commit().to_owned());
    delta.head_commit = Some(head.clone());
    delta.base_kind = Some(match &base {
        PushBase::ReviewPoint { .. } => "review point".into(),
        PushBase::MergeBase { default_branch, .. } => format!("merge base with {default_branch}"),
        PushBase::SessionStart { .. } => "session start".into(),
    });
    Ok(PushCapture {
        delta,
        base,
        head_commit: head,
    })
}

fn default_branch(runner: &dyn GitCommandRunner, root: &Path) -> Result<Option<String>> {
    let remote = run(
        runner,
        root,
        &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"],
    )?;
    match remote.status {
        0 => return Ok(Some(String::from_utf8(remote.stdout)?.trim().to_owned())),
        1 => {}
        _ => anyhow::bail!(
            "read origin's default branch: {}",
            String::from_utf8_lossy(&remote.stderr)
        ),
    }
    for branch in ["refs/heads/main", "refs/heads/master"] {
        if resolve_commit(runner, root, branch)?.is_some() {
            return Ok(Some(branch.to_owned()));
        }
    }
    Ok(None)
}

fn is_ancestor(
    runner: &dyn GitCommandRunner,
    root: &Path,
    older: &str,
    newer: &str,
) -> Result<bool> {
    let output = run(runner, root, &["merge-base", "--is-ancestor", older, newer])?;
    match output.status {
        0 => Ok(true),
        1 => Ok(false),
        _ => anyhow::bail!(
            "check review ancestry: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

pub(super) fn checked_git(
    runner: &dyn GitCommandRunner,
    root: &Path,
    args: &[&str],
) -> Result<Vec<u8>> {
    let output = run(runner, root, args)?;
    anyhow::ensure!(
        output.status == 0,
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn checked_git_text(runner: &dyn GitCommandRunner, root: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(checked_git(runner, root, args)?)?
        .trim()
        .to_owned())
}

fn capture_diff(
    runner: &dyn GitCommandRunner,
    root: &Path,
    baseline: &str,
    current: &str,
) -> Result<(String, Vec<FileLineChange>)> {
    let patch = diff_between_trees(runner, root, Some(baseline), current)
        .with_context(|| format!("diff the captured trees of {}", root.display()))?;
    let numstat = numstat_between_trees(runner, root, baseline, current)
        .with_context(|| format!("count the changed lines of {}", root.display()))?;
    Ok((patch, parse_numstat(&numstat)))
}

fn build_delta(
    root: &Path,
    baseline_tree: Option<String>,
    current_tree: String,
    patch: String,
    files: Vec<FileLineChange>,
    merged_in: Vec<String>,
    unattributed: Option<String>,
) -> RepoDelta {
    let summary = RawDiffSummary::from_patch(&patch);
    RepoDelta {
        base_commit: None,
        head_commit: None,
        base_kind: None,
        root: root.to_path_buf(),
        baseline_tree,
        current_tree,
        patch: bound_review_section(&patch, REVIEW_CAPTURE_DIFF_LIMIT, "workspace diff"),
        diffstat: summary.diffstat(),
        changed_lines: summary.changed_line_count(),
        files,
        merged_in,
        unattributed,
    }
}

/// The commit HEAD names, or `None` on an unborn branch.
fn resolve_head(git: &dyn GitCommandRunner, repository: &Path) -> Result<Option<String>> {
    resolve_commit(git, repository, "HEAD")
}

fn resolve_commit(
    git: &dyn GitCommandRunner,
    repository: &Path,
    reference: &str,
) -> Result<Option<String>> {
    let output = run(
        git,
        repository,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{reference}^{{commit}}"),
        ],
    )?;
    match output.status {
        0 => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )),
        1 => Ok(None),
        status => anyhow::bail!(
            "resolving {reference} of {} failed with status {status}: {}",
            repository.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// Runs one Git command in `repository` with no input.
pub(super) fn run(
    git: &dyn GitCommandRunner,
    repository: &Path,
    arguments: &[&str],
) -> Result<GitOutput> {
    git.run(
        repository,
        &GitCommand {
            arguments: arguments.iter().map(Into::into).collect(),
            stdin: Vec::new(),
            env: Vec::new(),
        },
    )
}

/// Paths already represented in a review baseline tree.
fn tree_paths(
    git: &dyn GitCommandRunner,
    repository: &Path,
    tree: &str,
) -> Result<BTreeSet<PathBuf>> {
    let output = run(
        git,
        repository,
        &["ls-tree", "-r", "--name-only", "-z", tree],
    )
    .with_context(|| format!("list paths in review baseline {tree}"))?;
    anyhow::ensure!(
        output.status == 0,
        "listing paths in review baseline {tree} failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(String::from_utf8_lossy(path).into_owned()))
        .collect())
}

/// Reads `git diff --numstat -z -M` output: `added\tdeleted\tpath\0` per file,
/// or `added\tdeleted\t\0old path\0new path\0` for a rename or copy, with
/// `-` for both counts on a binary file.
fn parse_numstat(output: &[u8]) -> Vec<FileLineChange> {
    let mut fields = output.split(|byte| *byte == 0);
    let mut files = Vec::new();
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(record);
        let mut parts = text.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let binary = added == "-" && deleted == "-";
        let (path, old_path) = if path.is_empty() {
            let old_path = fields
                .next()
                .map(|old| String::from_utf8_lossy(old).into_owned());
            let new_path = fields
                .next()
                .map(|new| String::from_utf8_lossy(new).into_owned())
                .unwrap_or_default();
            (new_path, old_path)
        } else {
            (path.to_owned(), None)
        };
        files.push(FileLineChange {
            path,
            old_path,
            insertions: added.parse().unwrap_or(0),
            deletions: deleted.parse().unwrap_or(0),
            binary,
        });
    }
    files
}

fn session_start_ref(runner: &dyn GitCommandRunner, root: &Path, session: &str) -> Result<String> {
    use sha2::{Digest, Sha256};
    let baseline = review_baseline_ref(runner, root)?;
    Ok(format!(
        "refs/mj/session-start/{}/{}",
        mj_core::hex::lower_hex(Sha256::digest(session.as_bytes())),
        mj_core::hex::lower_hex(Sha256::digest(baseline.as_bytes())),
    ))
}

/// Reads the immutable start commit, if this checkout has recorded one.
/// Legacy checkouts and moves that omit private refs return `None`.
pub fn session_start_head(
    runner: &dyn GitCommandRunner,
    root: &Path,
    session: &str,
) -> Result<Option<String>> {
    resolve_commit(runner, root, &session_start_ref(runner, root, session)?)
}

/// Preserves a recorded start, or starts coverage at the current HEAD when
/// an older session or a restored checkout has no start ref.
pub fn ensure_session_start_head(
    runner: &dyn GitCommandRunner,
    root: &Path,
    session: &str,
) -> Result<Option<String>> {
    if let Some(head) = session_start_head(runner, root, session)? {
        return Ok(Some(head));
    }
    let Some(head) = resolve_head(runner, root)? else {
        return Ok(None);
    };
    let reference = session_start_ref(runner, root, session)?;
    // An empty old value creates the ref only if it does not exist.
    if let Err(error) = checked_git(runner, root, &["update-ref", &reference, &head, ""]) {
        // A concurrent initializer may have published the start first.
        return session_start_head(runner, root, session)?
            .map(Some)
            .ok_or(error);
    }
    Ok(Some(head))
}

/// Pins the current worktree as `session`'s baseline for a fresh worker
/// workspace.
///
/// This is deliberately separate from [`capture_repository_deltas`]: startup
/// must establish the point before the primary harness can edit anything, but
/// a daemon restart must not replace a baseline that may have unreviewed work
/// after it. The caller decides whether this is a fresh startup; this function
/// is idempotent and preserves a valid pin `session` already holds. A pin
/// another session left in this worktree is replaced: its baseline is that
/// session's reviewed state, and adopting it would review everything since
/// then as this session's first turn.
pub fn initialize_review_baselines(
    git: &dyn GitCommandRunner,
    repositories: &[PathBuf],
    session: &str,
) -> Result<BTreeMap<PathBuf, Vec<UntrackedEntry>>> {
    let mut untracked_at_start = BTreeMap::new();
    for root in repositories {
        // The untracked list is recorded even when a baseline tree is already
        // pinned: a restarted worker still has to know which untracked files
        // predate it, or it would report all of them as this turn's work.
        let state = read_workspace_state(git, root).map_err(|error| {
            error.context(Refusal::precondition(format!(
                "turn review could not read the state of {} in {}s; the working tree is too \
                 large or too slow to review. Start this session without review by clearing \
                 [review] profile in config.toml.",
                root.display(),
                WORKSPACE_STATE_TIMEOUT.as_secs()
            )))
        })?;
        refuse_unreviewable_workspace(root, &state)?;
        untracked_at_start.insert(root.clone(), state.untracked);
        // Housekeeping for the whole repository; a failure leaves stale refs
        // behind but does not touch this session's pin.
        if let Err(error) = remove_departed_worktree_pins(git, root) {
            tracing::warn!(
                repository = %root.display(),
                error = format!("{error:#}"),
                "could not remove review pins of removed worktrees"
            );
        }
        ensure_session_start_head(git, root, session)?;
        let reference = review_baseline_ref(git, root)?;
        if pinned_baseline(git, root, &reference)
            .is_some_and(|(_, point)| point.session.as_deref() == Some(session))
        {
            continue;
        }
        // The baseline is HEAD plus what the checkout was already dirty with.
        // Untracked files are deliberately not in it; see `changed_untracked`
        // for how a review still tells a new file from a pre-existing one.
        let head = resolve_head(git, root)?;
        let reflog = read_head_reflog(git, root).unwrap_or_else(|error| {
            tracing::warn!(
                repository = %root.display(),
                error = format!("{error:#}"),
                "could not read the HEAD reflog; the first review covers every change"
            );
            Vec::new()
        });
        let tree = capture_paths(git, root, head.as_deref(), &state.dirty_tracked)
            .with_context(|| format!("capture the startup baseline of {}", root.display()))?;
        let point = record_review_point(
            git,
            root,
            &ReviewPoint {
                tree,
                reflog: reflog_anchor(&reflog, head.as_deref()),
                head,
                pending: Vec::new(),
                session: Some(session.to_owned()),
            },
        )
        .with_context(|| format!("record the startup baseline of {}", root.display()))?;
        pin_review_tree(git, root, &reference, &point)
            .with_context(|| format!("pin the startup review baseline of {}", root.display()))?;
    }
    Ok(untracked_at_start)
}

/// Refuses a workspace a turn review cannot honestly cover.
///
/// There is no partial-coverage mode: a review that silently leaves untracked
/// files out is one the user cannot trust. The sentence names the count, the
/// limit, and both ways out, and travels to the caller as a 409.
fn refuse_unreviewable_workspace(repository: &Path, state: &WorkspaceState) -> Result<()> {
    if state.untracked.len() <= UNTRACKED_REVIEW_LIMIT {
        return Ok(());
    }
    let count = state.untracked.len();
    Err(anyhow::Error::new(Refusal::precondition(format!(
        "turn review cannot cover {}: it has {count} untracked files and the limit is \
         {UNTRACKED_REVIEW_LIMIT}. Commit or ignore them, or start this session without \
         review by clearing [review] profile in config.toml.",
        repository.display()
    ))))
}

/// The review point a worktree's baseline pin holds, with its id, if it still
/// resolves to a tree in this repository. A pin from a release that did not
/// record its session reads with no owner.
fn pinned_baseline(
    git: &dyn GitCommandRunner,
    repository: &Path,
    reference: &str,
) -> Option<(String, ReviewPoint)> {
    let output = run(
        git,
        repository,
        &["rev-parse", "--verify", "--quiet", reference],
    )
    .ok()?;
    if output.status != 0 {
        return None;
    }
    let id = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    if id.is_empty() || !tree_exists(git, repository, &id) {
        return None;
    }
    let point = read_review_point(git, repository, &id).ok()?;
    Some((id, point))
}

/// Whether this repository still holds the tree a baseline names.
fn tree_exists(git: &dyn GitCommandRunner, repository: &Path, baseline: &str) -> bool {
    object_exists(git, repository, &format!("{baseline}^{{tree}}"))
}

pub(super) fn object_exists(git: &dyn GitCommandRunner, repository: &Path, object: &str) -> bool {
    run(git, repository, &["cat-file", "-e", object]).is_ok_and(|output| output.status == 0)
}

/// Reads which untracked paths each repository holds right now, as the point a
/// later review measures new files against.
pub fn read_untracked_at_start(
    git: &dyn GitCommandRunner,
    repositories: &[PathBuf],
) -> Result<BTreeMap<PathBuf, Vec<UntrackedEntry>>> {
    repositories
        .iter()
        .map(|root| Ok((root.clone(), read_workspace_state(git, root)?.untracked)))
        .collect()
}

/// Pins each named review point as its worktree's review baseline.
pub fn advance_baselines(
    git: &dyn GitCommandRunner,
    trees: &BTreeMap<PathBuf, String>,
) -> Result<()> {
    for (root, tree) in trees {
        let reference = review_baseline_ref(git, root)?;
        pin_review_tree(git, root, &reference, tree)
            .with_context(|| format!("pin the review baseline of {}", root.display()))?;
    }
    Ok(())
}

/// The workspace repositories a review covers: the session's working directory
/// and its additional roots, each resolved to the Git repository that contains
/// it. A directory that is not in a repository is skipped, and two roots inside
/// one repository collapse to a single entry.
pub fn discover_repositories(git: &dyn GitCommandRunner, roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut discovered: Vec<PathBuf> = Vec::new();
    for root in roots {
        let Some(toplevel) = repository_root(git, root) else {
            continue;
        };
        if !discovered.contains(&toplevel) {
            discovered.push(toplevel);
        }
    }
    discovered
}

fn repository_root(git: &dyn GitCommandRunner, directory: &Path) -> Option<PathBuf> {
    if !directory.is_dir() {
        return None;
    }
    let output = run(git, directory, &["rev-parse", "--show-toplevel"]).ok()?;
    if output.status != 0 {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use crate::test_support::git;
    use mj_checkpoint::archive::{REVIEW_BASELINE_REF, SystemGit};

    const SESSION: &str = "session-a";

    /// The review point this worktree's pin holds.
    fn pinned(repository: &Path) -> Option<String> {
        let reference = review_baseline_ref(&SystemGit, repository).unwrap();
        pinned_baseline(&SystemGit, repository, &reference).map(|(id, _)| id)
    }

    fn repository() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q", "-b", "main"]);
        git(temp.path(), &["config", "user.name", "Hel Test"]);
        git(temp.path(), &["config", "user.email", "hel@example.test"]);
        std::fs::write(temp.path().join("tracked.rs"), "fn main() {}\n").unwrap();
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-qm", "base"]);
        temp
    }

    #[test]
    fn a_capture_reports_what_changed_since_the_baseline() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        let first = capture_repository_deltas(
            &SystemGit,
            &roots,
            &BTreeMap::new(),
            &BTreeMap::new(),
            SESSION,
        )
        .unwrap();
        assert!(
            !has_changes(&first),
            "a repository with no baseline starts coverage rather than reviewing its whole history"
        );
        let baselines = captured_trees(&first);

        std::fs::write(temp.path().join("tracked.rs"), "fn main() { retry(); }\n").unwrap();
        let second =
            capture_repository_deltas(&SystemGit, &roots, &baselines, &BTreeMap::new(), SESSION)
                .unwrap();
        assert!(has_changes(&second));
        assert!(second[0].patch.contains("+fn main() { retry(); }"));
        assert_eq!(
            second[0].baseline_tree.as_deref(),
            baselines.values().next().map(String::as_str)
        );
        assert_eq!(second[0].changed_lines, 2);
        assert_eq!(
            second[0].files,
            vec![FileLineChange {
                path: "tracked.rs".into(),
                insertions: 1,
                deletions: 1,
                ..Default::default()
            }],
            "the capture counts each changed file's lines"
        );
    }

    /// Every changed file is counted, whatever kind of change it is, so a
    /// prompt whose diff was cut short still names it.
    #[test]
    fn a_capture_counts_new_renamed_and_binary_files() {
        assert!(parse_numstat(b"").is_empty());
        let temp = repository();
        std::fs::write(
            temp.path().join("moved.rs"),
            "fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("old name.rs"), "fn named() {}\n").unwrap();
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-qm", "more"]);
        let roots = vec![temp.path().to_path_buf()];
        let baselines = captured_trees(
            &capture_repository_deltas(
                &SystemGit,
                &roots,
                &BTreeMap::new(),
                &BTreeMap::new(),
                SESSION,
            )
            .unwrap(),
        );
        git(temp.path(), &["mv", "moved.rs", "renamed.rs"]);
        git(temp.path(), &["mv", "old name.rs", "new name.rs"]);
        std::fs::write(temp.path().join("logo.bin"), [0u8, 1, 2, 0, 255]).unwrap();
        std::fs::write(temp.path().join("new.rs"), "fn new() {}\nfn other() {}\n").unwrap();
        let deltas =
            capture_repository_deltas(&SystemGit, &roots, &baselines, &BTreeMap::new(), SESSION)
                .unwrap();
        let mut files = deltas[0].files.clone();
        files.sort_by(|left, right| left.path.cmp(&right.path));
        assert_eq!(
            files,
            vec![
                FileLineChange {
                    path: "logo.bin".into(),
                    binary: true,
                    ..Default::default()
                },
                FileLineChange {
                    path: "new name.rs".into(),
                    old_path: Some("old name.rs".into()),
                    insertions: 0,
                    deletions: 0,
                    ..Default::default()
                },
                FileLineChange {
                    path: "new.rs".into(),
                    insertions: 2,
                    ..Default::default()
                },
                FileLineChange {
                    path: "renamed.rs".into(),
                    old_path: Some("moved.rs".into()),
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn startup_baseline_makes_the_first_edits_reviewable() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();
        let startup = pinned(temp.path()).unwrap();

        std::fs::write(temp.path().join("tracked.rs"), "fn main() { first(); }\n").unwrap();
        std::fs::write(temp.path().join("new.rs"), "fn new() {}\n").unwrap();

        let deltas = capture_repository_deltas(
            &SystemGit,
            &roots,
            &BTreeMap::new(),
            &BTreeMap::new(),
            SESSION,
        )
        .unwrap();
        assert!(has_changes(&deltas));
        assert_eq!(deltas[0].baseline_tree.as_deref(), Some(startup.as_str()));
        assert!(deltas[0].patch.contains("+fn main() { first(); }"));
        assert!(deltas[0].patch.contains("new.rs"));
    }

    #[test]
    fn startup_baseline_reviews_edits_committed_during_the_turn() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();

        std::fs::write(
            temp.path().join("tracked.rs"),
            "fn main() { committed(); }\n",
        )
        .unwrap();
        git(temp.path(), &["add", "tracked.rs"]);
        git(temp.path(), &["commit", "-qm", "turn change"]);

        let deltas = capture_repository_deltas(
            &SystemGit,
            &roots,
            &BTreeMap::new(),
            &BTreeMap::new(),
            SESSION,
        )
        .unwrap();
        assert!(has_changes(&deltas));
        assert!(deltas[0].patch.contains("+fn main() { committed(); }"));
    }

    /// A turn that merges another line of history is reviewed for its own
    /// commits, its conflict resolutions and its edits, and not for what the
    /// merges brought in. The merged lines are written in another worktree of
    /// the repository, as another session would write them. One builds on a
    /// commit the turn made earlier, which must stay in the review; the other
    /// conflicts with content an earlier review already covered. The shared
    /// file is over 64KB, so its conflicting versions cross pipe buffers
    /// through Git.
    // Hard-won: #1285: a merge made during the turn put the merged-in commits in the turn's review.
    #[test]
    fn a_merge_during_the_turn_reviews_only_the_turns_work() {
        let temp = repository();
        let root = temp.path();
        let outside = tempfile::tempdir().unwrap();
        let other = outside.path().join("other");
        let edit = |checkout: &Path, edits: &[(usize, &str)]| {
            let shared = checkout.join("shared.txt");
            let text = std::fs::read_to_string(&shared).unwrap_or_default();
            let mut lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
            lines.resize_with(8000, String::new);
            for (number, line) in lines.iter_mut().enumerate() {
                if line.is_empty() {
                    *line = format!("line {}", number + 1);
                }
            }
            for (number, replacement) in edits {
                lines[number - 1] = (*replacement).to_owned();
            }
            std::fs::write(&shared, lines.join("\n") + "\n").unwrap();
        };
        let commit = |checkout: &Path, message: &str| {
            git(checkout, &["add", "-A"]);
            git(checkout, &["commit", "-qm", message]);
        };
        // Merges `branch`, which conflicts in one place, and settles that
        // conflict with `resolution`.
        let merge = |branch: &str, resolution: &str| {
            let merged = std::process::Command::new("git")
                .args(["merge", "-q", "--no-edit", branch])
                .current_dir(root)
                .output()
                .unwrap();
            assert!(!merged.status.success(), "merging {branch} conflicts");
            let shared = root.join("shared.txt");
            let text = std::fs::read_to_string(&shared).unwrap();
            let mut resolved = Vec::new();
            let mut in_conflict = false;
            for line in text.lines() {
                if line.starts_with("<<<<<<<") {
                    in_conflict = true;
                    resolved.push(resolution);
                } else if line.starts_with(">>>>>>>") {
                    in_conflict = false;
                } else if !in_conflict {
                    resolved.push(line);
                }
            }
            std::fs::write(&shared, resolved.join("\n") + "\n").unwrap();
            git(root, &["add", "-A"]);
            git(root, &["commit", "-q", "--no-edit"]);
        };
        let roots = vec![root.to_path_buf()];

        edit(root, &[]);
        commit(root, "shared");
        git(root, &["branch", "old-upstream"]);
        edit(root, &[(30, "line 30 reviewed")]);
        commit(root, "reviewed before the session");
        let other_path = other.to_str().unwrap();
        git(root, &["worktree", "add", "-q", "--detach", other_path]);
        let untracked_at_start = initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();

        edit(root, &[(10, "line 10 turn")]);
        commit(root, "turn, later pushed");
        git(&other, &["checkout", "-q", "-b", "upstream", "main"]);
        std::fs::write(other.join("upstream.rs"), "fn upstream() {}\n").unwrap();
        edit(
            &other,
            &[(20, "line 20 upstream"), (6000, "line 6000 upstream")],
        );
        commit(&other, "upstream, built on the turn's commit");
        git(&other, &["checkout", "-q", "old-upstream"]);
        edit(
            &other,
            &[(30, "line 30 upstream"), (7000, "line 7000 old upstream")],
        );
        commit(&other, "upstream from before the session");
        edit(root, &[(20, "line 20 turn")]);
        commit(root, "turn");
        merge("upstream", "line 20 resolved");
        merge("old-upstream", "line 30 resolved");
        std::fs::write(root.join("tracked.rs"), "fn main() { edited(); }\n").unwrap();

        let deltas = capture_repository_deltas(
            &SystemGit,
            &roots,
            &BTreeMap::new(),
            &untracked_at_start,
            SESSION,
        )
        .unwrap();

        let patch = &deltas[0].patch;
        for wanted in [
            "+line 10 turn",
            "-line 20 upstream",
            "+line 20 resolved",
            "-<<<<<<< reviewed",
            "-line 30 upstream",
            "+line 30 resolved",
            "+fn main() { edited(); }",
        ] {
            assert!(patch.contains(wanted), "{wanted:?} is missing: {patch}");
        }
        for unwanted in ["upstream.rs", "line 6000", "line 7000"] {
            assert!(
                !patch.contains(unwanted),
                "{unwanted:?} was merged in: {patch}"
            );
        }
        assert_eq!(
            deltas[0]
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["shared.txt", "tracked.rs"]
        );
        assert_eq!(
            deltas[0].merged_in,
            vec![
                git(root, &["rev-parse", "upstream"]),
                git(root, &["rev-parse", "old-upstream"])
            ]
        );

        // The capture records the HEAD it was taken at, so the next review
        // starts from it and finds nothing new.
        let baselines = captured_trees(&deltas);
        advance_baselines(&SystemGit, &baselines).unwrap();
        let next_start = read_untracked_at_start(&SystemGit, &roots).unwrap();
        let next = capture_repository_deltas(&SystemGit, &roots, &baselines, &next_start, SESSION)
            .unwrap();
        assert!(!has_changes(&next), "{}", next[0].patch);
    }

    /// Another clone pushes its own work to `main` and squash-merges the
    /// turn's pushed branch. The turn fast-forwards `main`, starts a branch
    /// from it, and commits there, squashing its own commits with a soft reset
    /// and amending the result. The review holds the turn's work, including
    /// the commit that reached HEAD only inside the other clone's squash, and
    /// nothing the other clone wrote; the commits the turn rewrote leave no
    /// conflict markers. A second review after the turn merges `main` again
    /// holds only its new commit.
    // Hard-won: 57fd7fe8e: a fast-forward or a branch from upstream put other people's commits in the turn's review.
    #[test]
    fn a_pulled_or_squashed_upstream_is_not_the_turns_work() {
        let temp = repository();
        let root = temp.path();
        let outside = tempfile::tempdir().unwrap();
        let remote = outside.path().join("remote.git");
        let remote_path = remote.to_str().unwrap();
        let teammate = outside.path().join("teammate");
        git(
            outside.path(),
            &["init", "-q", "--bare", "-b", "main", remote_path],
        );
        git(root, &["remote", "add", "origin", remote_path]);
        git(root, &["push", "-q", "origin", "main"]);
        git(
            outside.path(),
            &["clone", "-q", remote_path, teammate.to_str().unwrap()],
        );
        git(&teammate, &["config", "user.name", "Teammate"]);
        git(
            &teammate,
            &["config", "user.email", "teammate@example.test"],
        );
        let write = |checkout: &Path, name: &str, text: &str| {
            std::fs::write(checkout.join(name), text).unwrap();
            git(checkout, &["add", "-A"]);
        };
        let pull = |checkout: &Path, arguments: &[&str]| {
            let mut command = vec!["-c", "pull.rebase=false", "pull", "-q"];
            command.extend(arguments);
            git(checkout, &command);
        };
        let roots = vec![root.to_path_buf()];
        let untracked_at_start = initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();

        git(root, &["checkout", "-qb", "feature"]);
        write(root, "feature.rs", "fn feature() {}\n");
        git(root, &["commit", "-qm", "feature, squash-merged upstream"]);
        git(root, &["push", "-q", "origin", "feature"]);
        write(&teammate, "teammate.rs", "fn teammate() {}\n");
        git(&teammate, &["commit", "-qm", "teammate"]);
        git(&teammate, &["fetch", "-q", "origin"]);
        git(&teammate, &["merge", "-q", "--squash", "origin/feature"]);
        git(&teammate, &["commit", "-qm", "Squash feature (#1)"]);
        git(&teammate, &["push", "-q", "origin", "main"]);
        git(root, &["checkout", "-q", "main"]);
        pull(root, &["--ff-only", "origin", "main"]);
        git(root, &["checkout", "-qb", "next"]);
        write(root, "tracked.rs", "fn main() { draft(); }\n");
        git(root, &["commit", "-qm", "next"]);
        write(root, "tracked.rs", "fn main() { fixed(); }\n");
        git(root, &["commit", "-qm", "next, fixed"]);
        git(root, &["reset", "-q", "--soft", "HEAD~2"]);
        git(root, &["commit", "-qm", "next"]);
        write(root, "tracked.rs", "fn main() { done(); }\n");
        git(root, &["commit", "-q", "--amend", "--no-edit"]);

        let first = capture_repository_deltas(
            &SystemGit,
            &roots,
            &BTreeMap::new(),
            &untracked_at_start,
            SESSION,
        )
        .unwrap();
        let patch = &first[0].patch;
        for wanted in ["+fn feature() {}", "+fn main() { done(); }"] {
            assert!(patch.contains(wanted), "{wanted:?} is missing: {patch}");
        }
        for unwanted in ["teammate.rs", "draft", "fixed", "<<<<<<<"] {
            assert!(!patch.contains(unwanted), "{unwanted:?} is in: {patch}");
        }
        assert_eq!(first[0].merged_in, vec![git(root, &["rev-parse", "main"])]);
        assert_eq!(first[0].unattributed, None);

        let baselines = captured_trees(&first);
        advance_baselines(&SystemGit, &baselines).unwrap();
        let next_start = read_untracked_at_start(&SystemGit, &roots).unwrap();
        write(&teammate, "later.rs", "fn later() {}\n");
        git(&teammate, &["commit", "-qm", "teammate, later"]);
        git(&teammate, &["push", "-q", "origin", "main"]);
        pull(root, &["--no-edit", "origin", "main"]);
        write(root, "more.rs", "fn more() {}\n");
        git(root, &["commit", "-qm", "more"]);

        let second =
            capture_repository_deltas(&SystemGit, &roots, &baselines, &next_start, SESSION)
                .unwrap();
        let patch = &second[0].patch;
        assert!(patch.contains("+fn more() {}"), "{patch}");
        for unwanted in ["later.rs", "tracked.rs", "feature.rs"] {
            assert!(!patch.contains(unwanted), "{unwanted:?} is in: {patch}");
        }
    }

    /// A review that covered commits the turn then replays onto upstream with
    /// `git rebase --rebase-merges`, among them a merge whose conflict it had
    /// resolved, and then amends shows only what the amend changed: not the
    /// replayed commits, not the resolution it already reviewed, and not the
    /// upstream commit they now sit on.
    // Hard-won: 424f97670: amending or rebasing a reviewed commit put the whole commit back in the review.
    // Hard-won: f93bececc: a rebase reset step forced a whole-tree review, and a replayed merge put its reviewed resolution back.
    #[test]
    fn a_rewrite_of_reviewed_commits_reviews_only_what_it_changed() {
        let temp = repository();
        let root = temp.path();
        let outside = tempfile::tempdir().unwrap();
        let other = outside.path().join("other");
        git(
            root,
            &["worktree", "add", "-q", "--detach", other.to_str().unwrap()],
        );
        // Runs a Git command that stops on a conflict in tracked.rs and
        // settles it with `resolution`.
        let settle = |arguments: &[&str], resolution: &str| {
            let output = std::process::Command::new("git")
                .args(arguments)
                .current_dir(root)
                .env("GIT_EDITOR", "true")
                .output()
                .unwrap();
            assert!(!output.status.success(), "{arguments:?} conflicts");
            std::fs::write(root.join("tracked.rs"), resolution).unwrap();
            git(root, &["add", "tracked.rs"]);
        };
        let roots = vec![root.to_path_buf()];
        let untracked_at_start = initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();
        git(root, &["checkout", "-qb", "side"]);
        std::fs::write(root.join("tracked.rs"), "fn main() { side(); }\n").unwrap();
        git(root, &["commit", "-qam", "side"]);
        git(root, &["checkout", "-qb", "feature", "main"]);
        std::fs::write(root.join("feature.rs"), "fn one() {}\n").unwrap();
        std::fs::write(root.join("tracked.rs"), "fn main() { feature(); }\n").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "one"]);
        settle(
            &["merge", "-q", "--no-edit", "side"],
            "fn main() { both(); }\n",
        );
        git(root, &["commit", "-q", "--no-edit"]);
        std::fs::write(root.join("feature.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        git(root, &["commit", "-qam", "two"]);
        let first = capture_repository_deltas(
            &SystemGit,
            &roots,
            &BTreeMap::new(),
            &untracked_at_start,
            SESSION,
        )
        .unwrap();
        for wanted in ["+fn main() { both(); }", "+fn two() {}"] {
            assert!(first[0].patch.contains(wanted), "{}", first[0].patch);
        }

        let baselines = captured_trees(&first);
        advance_baselines(&SystemGit, &baselines).unwrap();
        let next_start = read_untracked_at_start(&SystemGit, &roots).unwrap();
        git(&other, &["checkout", "-q", "main"]);
        std::fs::write(other.join("upstream.rs"), "fn upstream() {}\n").unwrap();
        git(&other, &["add", "upstream.rs"]);
        git(&other, &["commit", "-qm", "upstream"]);
        settle(
            &["rebase", "-q", "--rebase-merges", "main"],
            "fn main() { both(); }\n",
        );
        let continued = std::process::Command::new("git")
            .args(["rebase", "--continue"])
            .current_dir(root)
            .env("GIT_EDITOR", "true")
            .output()
            .unwrap();
        assert!(
            continued.status.success(),
            "{}",
            String::from_utf8_lossy(&continued.stderr)
        );
        std::fs::write(
            root.join("feature.rs"),
            "fn one() {}\nfn two() { fixed(); }\n",
        )
        .unwrap();
        git(root, &["commit", "-qa", "--amend", "--no-edit"]);
        let second =
            capture_repository_deltas(&SystemGit, &roots, &baselines, &next_start, SESSION)
                .unwrap();
        assert_eq!(second[0].unattributed, None);
        let patch = &second[0].patch;
        for wanted in ["-fn two() {}", "+fn two() { fixed(); }"] {
            assert!(patch.contains(wanted), "{wanted:?} is missing: {patch}");
        }
        for unwanted in ["+fn one() {}", "upstream.rs", "both", "<<<<<<<"] {
            assert!(!patch.contains(unwanted), "{unwanted:?} is in: {patch}");
        }
    }

    /// Work the turn commits on a branch it then leaves is not in that
    /// review's capture. It is still the turn's when it returns later, here by
    /// a merge, so a later review holds it rather than counting it as merged-in
    /// history, even when a review in between could not attribute. Work that
    /// returns during such a review is covered by its whole-tree diff and is
    /// not reviewed again.
    // Hard-won: 57fd7fe8e: merging the turn's own branch folded its commits into the base as merged-in history.
    // Hard-won: 424f97670: a review that could not attribute forgot the commits still pending.
    // Hard-won: f93bececc: a review that could not attribute kept pending a commit its whole-tree diff had covered.
    #[test]
    fn work_left_on_another_branch_is_reviewed_when_it_returns() {
        let temp = repository();
        let root = temp.path();
        let roots = vec![root.to_path_buf()];
        let untracked_at_start = initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();
        let leave = |branch: &str| {
            git(root, &["checkout", "-qb", branch, "main"]);
            let file = format!("{branch}.rs");
            std::fs::write(root.join(&file), format!("fn {branch}() {{}}\n")).unwrap();
            git(root, &["add", &file]);
            git(root, &["commit", "-qm", branch]);
            git(root, &["checkout", "-q", "main"]);
        };
        let review = |baselines: &BTreeMap<PathBuf, String>| {
            let start = read_untracked_at_start(&SystemGit, &roots).unwrap();
            let deltas =
                capture_repository_deltas(&SystemGit, &roots, baselines, &start, SESSION).unwrap();
            advance_baselines(&SystemGit, &captured_trees(&deltas)).unwrap();
            deltas
        };
        leave("early");
        leave("late");
        std::fs::write(root.join("tracked.rs"), "fn main() { edited(); }\n").unwrap();

        let first = capture_repository_deltas(
            &SystemGit,
            &roots,
            &BTreeMap::new(),
            &untracked_at_start,
            SESSION,
        )
        .unwrap();
        advance_baselines(&SystemGit, &captured_trees(&first)).unwrap();
        assert!(first[0].patch.contains("+fn main() { edited(); }"));
        for unwanted in ["early.rs", "late.rs"] {
            assert!(!first[0].patch.contains(unwanted), "{}", first[0].patch);
        }

        std::fs::remove_file(root.join(".git/logs/HEAD")).unwrap();
        git(root, &["commit", "-qam", "edited"]);
        git(root, &["merge", "-q", "--no-ff", "--no-edit", "early"]);
        let second = review(&captured_trees(&first));
        assert!(second[0].unattributed.is_some(), "{:?}", second[0]);
        assert!(
            second[0].patch.contains("+fn early() {}"),
            "{}",
            second[0].patch
        );

        std::fs::write(root.join("more.rs"), "fn more() {}\n").unwrap();
        git(root, &["add", "more.rs"]);
        git(root, &["commit", "-qm", "more"]);
        let third = review(&captured_trees(&second));
        assert_eq!(third[0].unattributed, None);
        assert!(
            third[0].patch.contains("+fn more() {}"),
            "{}",
            third[0].patch
        );
        assert!(!third[0].patch.contains("early.rs"), "{}", third[0].patch);

        git(root, &["merge", "-q", "--no-ff", "--no-edit", "late"]);
        let fourth = review(&captured_trees(&third));
        assert!(
            fourth[0].patch.contains("+fn late() {}"),
            "{}",
            fourth[0].patch
        );
        for unwanted in ["tracked.rs", "early.rs", "more.rs"] {
            assert!(!fourth[0].patch.contains(unwanted), "{}", fourth[0].patch);
        }
        assert!(fourth[0].merged_in.is_empty(), "{:?}", fourth[0].merged_in);
    }

    /// The turn's own work stays pending while the capture lacks it, even
    /// when a later mainline change conflicts with it, or the turn merged it
    /// and reverted the merge, whether a review could attribute that or not.
    /// All of it comes back inside another worktree's commits and is reviewed
    /// as the turn's.
    // Hard-won: b4a2db6e6: a conflict with later lines, or a merge the turn reverted, dropped the work from pending.
    #[test]
    fn pending_work_the_capture_lacks_is_reviewed_when_it_returns() {
        let temp = repository();
        let root = temp.path();
        let outside = tempfile::tempdir().unwrap();
        let other = outside.path().join("other");
        let roots = vec![root.to_path_buf()];
        let review = |baselines: &BTreeMap<PathBuf, String>| {
            let start = read_untracked_at_start(&SystemGit, &roots).unwrap();
            let deltas =
                capture_repository_deltas(&SystemGit, &roots, baselines, &start, SESSION).unwrap();
            advance_baselines(&SystemGit, &captured_trees(&deltas)).unwrap();
            deltas
        };
        initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();
        git(root, &["checkout", "-qb", "lost"]);
        std::fs::write(root.join("tracked.rs"), "fn main() { lost(); }\n").unwrap();
        git(root, &["commit", "-qam", "lost"]);
        for branch in ["undone", "again"] {
            git(root, &["checkout", "-qb", branch, "main"]);
            let file = format!("{branch}.rs");
            std::fs::write(root.join(&file), format!("fn {branch}() {{}}\n")).unwrap();
            git(root, &["add", &file]);
            git(root, &["commit", "-qm", branch]);
            git(root, &["checkout", "-q", "main"]);
        }
        std::fs::write(root.join("tracked.rs"), "fn main() { kept(); }\n").unwrap();
        git(root, &["commit", "-qam", "kept"]);
        let first = review(&BTreeMap::new());
        assert!(
            first[0].patch.contains("+fn main() { kept(); }"),
            "{}",
            first[0].patch
        );

        std::fs::remove_file(root.join(".git/logs/HEAD")).unwrap();
        git(root, &["merge", "-q", "--no-ff", "--no-edit", "undone"]);
        git(root, &["revert", "-m", "1", "--no-edit", "HEAD"]);
        let second = review(&captured_trees(&first));
        assert!(second[0].unattributed.is_some(), "{:?}", second[0]);

        git(root, &["merge", "-q", "--no-ff", "--no-edit", "again"]);
        git(root, &["revert", "-m", "1", "--no-edit", "HEAD"]);
        let third = review(&captured_trees(&second));
        assert_eq!(third[0].unattributed, None);

        git(
            root,
            &["worktree", "add", "-q", "--detach", other.to_str().unwrap()],
        );
        git(&other, &["cherry-pick", "undone", "again"]);
        let squash = std::process::Command::new("git")
            .args(["merge", "-q", "--squash", "lost"])
            .current_dir(&other)
            .output()
            .unwrap();
        assert!(!squash.status.success(), "the squash conflicts with kept()");
        std::fs::write(other.join("tracked.rs"), "fn main() { lost(); }\n").unwrap();
        git(&other, &["add", "tracked.rs"]);
        git(&other, &["commit", "-qm", "Squash lost"]);
        let returned = git(&other, &["rev-parse", "HEAD"]);
        git(root, &["merge", "-q", "--ff-only", &returned]);
        let fourth = review(&captured_trees(&third));
        assert_eq!(fourth[0].unattributed, None);
        for wanted in [
            "+fn undone() {}",
            "+fn again() {}",
            "+fn main() { lost(); }",
        ] {
            assert!(
                fourth[0].patch.contains(wanted),
                "{wanted:?} is missing: {}",
                fourth[0].patch
            );
        }
    }

    /// When the HEAD reflog no longer explains how HEAD moved, the review
    /// covers every change since the baseline and names why, rather than
    /// guessing which commits are the turn's.
    #[test]
    fn a_head_move_the_reflog_cannot_explain_is_reviewed_whole_and_says_why() {
        let temp = repository();
        let root = temp.path();
        let roots = vec![root.to_path_buf()];
        let untracked_at_start = initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();
        std::fs::remove_file(root.join(".git/logs/HEAD")).unwrap();
        std::fs::write(root.join("tracked.rs"), "fn main() { committed(); }\n").unwrap();
        git(root, &["commit", "-qam", "after the reflog was lost"]);

        let deltas = capture_repository_deltas(
            &SystemGit,
            &roots,
            &BTreeMap::new(),
            &untracked_at_start,
            SESSION,
        )
        .unwrap();
        assert!(deltas[0].patch.contains("+fn main() { committed(); }"));
        let reason = deltas[0].unattributed.as_deref().unwrap_or_default();
        assert!(reason.contains("reflog"), "{reason:?}");
    }

    /// Earlier workers recorded a baseline as a bare tree id, in the
    /// controller's records and in the pinned ref alike. After an upgrade it
    /// is still reviewed, as a plain tree diff.
    #[test]
    fn a_bare_tree_baseline_from_an_earlier_worker_is_still_reviewed() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        let tree = git(temp.path(), &["rev-parse", "HEAD^{tree}"]);
        git(temp.path(), &["update-ref", REVIEW_BASELINE_REF, &tree]);
        std::fs::write(
            temp.path().join("tracked.rs"),
            "fn main() { upgraded(); }\n",
        )
        .unwrap();

        let recorded = BTreeMap::from([(temp.path().to_path_buf(), tree.clone())]);
        for baselines in [BTreeMap::new(), recorded] {
            let deltas = capture_repository_deltas(
                &SystemGit,
                &roots,
                &baselines,
                &BTreeMap::new(),
                SESSION,
            )
            .unwrap();
            assert_eq!(deltas[0].baseline_tree.as_deref(), Some(tree.as_str()));
            assert!(deltas[0].patch.contains("+fn main() { upgraded(); }"));
        }
    }

    /// Every linked worktree shares its repository's refs, and a managed
    /// worktree is how a session usually checks a repository out. Each
    /// worktree session keeps a pin of its own: it neither starts from another
    /// session's baseline nor moves one, and the next session to start in the
    /// repository deletes the pin of a worktree that was removed.
    // Hard-won: #1307: a new worktree session took another session's review baseline as its own.
    #[test]
    fn each_worktree_session_keeps_its_own_baseline() {
        let temp = repository();
        let main = temp.path().to_path_buf();
        let outside = tempfile::tempdir().unwrap();
        let linked = outside.path().join("feature");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                linked.to_str().unwrap(),
            ],
        );
        let capture =
            |root: &Path, session: &str, start: &BTreeMap<PathBuf, Vec<UntrackedEntry>>| {
                capture_repository_deltas(
                    &SystemGit,
                    &[root.to_path_buf()],
                    &BTreeMap::new(),
                    start,
                    session,
                )
                .unwrap()
            };

        initialize_review_baselines(&SystemGit, std::slice::from_ref(&main), "session-a").unwrap();
        std::fs::write(main.join("tracked.rs"), "fn main() { a(); }\n").unwrap();
        let a = capture(&main, "session-a", &BTreeMap::new());
        advance_baselines(&SystemGit, &captured_trees(&a)).unwrap();

        let b_start =
            initialize_review_baselines(&SystemGit, std::slice::from_ref(&linked), "session-b")
                .unwrap();
        std::fs::write(linked.join("b.rs"), "fn b() {}\n").unwrap();
        let b = capture(&linked, "session-b", &b_start);
        assert!(b[0].patch.contains("b.rs"), "{}", b[0].patch);
        assert!(
            !b[0].patch.contains("a();"),
            "the worktree session started from another session's baseline: {}",
            b[0].patch
        );
        advance_baselines(&SystemGit, &captured_trees(&b)).unwrap();

        let a_again = capture(&main, "session-a", &BTreeMap::new());
        assert!(
            a_again[0].baseline_tree.is_some(),
            "this session lost its pin"
        );
        assert!(
            !has_changes(&a_again),
            "another session's review moved this one's baseline: {}",
            a_again[0].patch
        );

        assert!(
            git(&main, &["for-each-ref", "refs/hel/worktrees/feature"]).contains("review-baseline")
        );
        git(
            &main,
            &["worktree", "remove", "--force", linked.to_str().unwrap()],
        );
        initialize_review_baselines(&SystemGit, std::slice::from_ref(&main), "session-c").unwrap();
        assert_eq!(git(&main, &["for-each-ref", "refs/hel/worktrees"]), "");
    }

    /// A checkout an earlier session worked in still holds that session's
    /// pin. A new session starts from its own baseline; adopting the old pin
    /// would review everything changed since that session's last review as the
    /// new session's first turn.
    // Hard-won: #1307: a new session in a reused checkout took the previous session's baseline.
    #[test]
    fn a_new_session_in_a_used_checkout_starts_from_its_own_baseline() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        initialize_review_baselines(&SystemGit, &roots, "earlier").unwrap();
        std::fs::write(temp.path().join("tracked.rs"), "fn main() { by_hand(); }\n").unwrap();
        git(temp.path(), &["commit", "-qam", "between sessions"]);

        let start = initialize_review_baselines(&SystemGit, &roots, "later").unwrap();
        std::fs::write(temp.path().join("later.rs"), "fn later() {}\n").unwrap();
        let deltas =
            capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new(), &start, "later")
                .unwrap();

        assert!(deltas[0].patch.contains("later.rs"), "{}", deltas[0].patch);
        assert!(
            !deltas[0].patch.contains("by_hand"),
            "the new session started from the earlier session's baseline: {}",
            deltas[0].patch
        );
        // The earlier session's pin is no longer anyone's fallback.
        let earlier_capture =
            capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new(), &start, "earlier")
                .unwrap();
        assert_eq!(earlier_capture[0].baseline_tree, None);
    }

    #[test]
    fn persisted_review_baseline_takes_precedence_over_the_startup_pin() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();

        std::fs::write(temp.path().join("tracked.rs"), "fn main() { first(); }\n").unwrap();
        let first = capture_repository_deltas(
            &SystemGit,
            &roots,
            &BTreeMap::new(),
            &BTreeMap::new(),
            SESSION,
        )
        .unwrap();
        let persisted = captured_trees(&first);

        std::fs::write(temp.path().join("tracked.rs"), "fn main() { second(); }\n").unwrap();
        let second =
            capture_repository_deltas(&SystemGit, &roots, &persisted, &BTreeMap::new(), SESSION)
                .unwrap();
        assert!(has_changes(&second));
        assert_eq!(second[0].baseline_tree, persisted.values().next().cloned());
        assert!(second[0].patch.contains("+fn main() { second(); }"));
        assert!(!second[0].patch.contains("+fn main() { first(); }"));
    }

    #[test]
    fn startup_baseline_excludes_dirty_files_present_before_the_turn() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        std::fs::write(temp.path().join("tracked.rs"), "base\nbefore\n").unwrap();
        std::fs::write(temp.path().join("preexisting.rs"), "already here\n").unwrap();
        let untracked_at_start = initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();

        std::fs::write(temp.path().join("tracked.rs"), "base\nbefore\nafter\n").unwrap();
        std::fs::write(temp.path().join("agent.rs"), "new work\n").unwrap();

        let deltas = capture_repository_deltas(
            &SystemGit,
            &roots,
            &BTreeMap::new(),
            &untracked_at_start,
            SESSION,
        )
        .unwrap();
        assert!(has_changes(&deltas));
        assert!(deltas[0].patch.contains("+after"));
        assert!(!deltas[0].patch.contains("+before"));
        assert!(deltas[0].patch.contains("agent.rs"));
        assert!(!deltas[0].patch.contains("preexisting.rs"));
    }

    #[test]
    fn restarting_preserves_the_startup_baseline_and_pending_changes() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();
        let startup = pinned(temp.path()).unwrap();

        std::fs::write(temp.path().join("tracked.rs"), "fn main() { pending(); }\n").unwrap();
        initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();
        assert_eq!(pinned(temp.path()).as_deref(), Some(startup.as_str()));

        let deltas = capture_repository_deltas(
            &SystemGit,
            &roots,
            &BTreeMap::new(),
            &BTreeMap::new(),
            SESSION,
        )
        .unwrap();
        assert!(has_changes(&deltas));
        assert!(deltas[0].patch.contains("+fn main() { pending(); }"));
    }

    #[test]
    fn a_baseline_this_repository_no_longer_holds_restarts_coverage() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        // A tree id from another repository -- what a resume onto a fresh
        // target leaves behind. Reviewing the whole tree instead would bury
        // the turn's own change in it.
        let stale = BTreeMap::from([(
            temp.path().to_path_buf(),
            "0123456789abcdef0123456789abcdef01234567".to_string(),
        )]);
        let deltas =
            capture_repository_deltas(&SystemGit, &roots, &stale, &BTreeMap::new(), SESSION)
                .unwrap();
        assert!(!has_changes(&deltas));
        assert_eq!(deltas[0].baseline_tree, None);
        assert!(!deltas[0].current_tree.is_empty());
    }

    #[test]
    fn discovery_finds_the_repository_a_workspace_root_sits_in() {
        let temp = repository();
        let nested = temp.path().join("src");
        std::fs::create_dir_all(&nested).unwrap();
        let roots = vec![
            nested,
            temp.path().to_path_buf(),
            PathBuf::from("/nonexistent"),
        ];
        let discovered = discover_repositories(&SystemGit, &roots);
        assert_eq!(
            discovered.len(),
            1,
            "two roots inside one repository collapse, and a missing one is skipped: {discovered:?}"
        );
    }

    /// Counts the loose objects in a repository, which is how much this
    /// repository grew.
    fn loose_objects(repository: &Path) -> usize {
        let objects = repository.join(".git/objects");
        let Ok(entries) = std::fs::read_dir(&objects) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_name().to_str().is_some_and(|name| {
                    name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit())
                })
            })
            .filter_map(|entry| std::fs::read_dir(entry.path()).ok())
            .map(|entries| entries.filter_map(Result::ok).count())
            .sum()
    }

    /// A workspace with more untracked files than the list can hold is refused
    /// rather than reviewed without coverage, and the sentence names the count,
    /// the limit, and both ways out.
    #[test]
    fn a_workspace_review_cannot_cover_is_refused_with_both_remedies() {
        let temp = repository();
        let roots = [temp.path().to_path_buf()];
        // Asserting on the real 50,000-file limit would mean creating 50,001
        // files, so the refusal is driven through the same code with a
        // workspace state that reports more than the limit.
        let state = WorkspaceState {
            dirty_tracked: Vec::new(),
            untracked: (0..=UNTRACKED_REVIEW_LIMIT)
                .map(|index| UntrackedEntry {
                    path: PathBuf::from(format!("file-{index}")),
                    size: 0,
                    modified_ms: 0,
                })
                .collect(),
        };

        let error = refuse_unreviewable_workspace(&roots[0], &state).unwrap_err();

        let refusal = Refusal::of(&error).expect("an oversized workspace is a refusal");
        let message = refusal.message();
        assert!(
            message.contains(&(UNTRACKED_REVIEW_LIMIT + 1).to_string()),
            "{message}"
        );
        assert!(
            message.contains(&UNTRACKED_REVIEW_LIMIT.to_string()),
            "{message}"
        );
        assert!(message.contains("[review] profile"), "{message}");
        assert!(message.contains("Commit or ignore"), "{message}");
    }

    /// The cost of a capture must be the session's own changes, not the size
    /// of the working tree. This is #1065: a workspace with hundreds of
    /// thousands of untracked files made every session start read, hash and
    /// store every one of them before the worker could be reached.
    // Hard-won: b3457c71: capturing a session baseline staged and hashed hundreds of thousands of unrelated files.
    #[test]
    fn a_startup_baseline_does_not_read_untracked_files_it_was_not_asked_about() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        for index in 0..200 {
            std::fs::write(
                temp.path().join(format!("untracked-{index}.bin")),
                format!("content {index}"),
            )
            .unwrap();
        }
        let before = loose_objects(temp.path());

        initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();

        // At most a tree and the review point naming it. Two hundred
        // untracked files cost nothing, because nothing opened them.
        let written = loose_objects(temp.path()) - before;
        assert!(
            written <= 2,
            "a clean checkout's baseline wrote {written} objects; it must not stage untracked files"
        );
    }

    /// A review captures what the turn changed and nothing else, so the same
    /// untouched untracked files stay unread at review time too.
    // Hard-won: b3457c71: review capture cost grew with all untracked workspace files instead of turn changes.
    #[test]
    fn a_review_capture_costs_the_turns_changes_and_not_the_tree() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        for index in 0..200 {
            std::fs::write(
                temp.path().join(format!("untracked-{index}.bin")),
                format!("content {index}"),
            )
            .unwrap();
        }
        let untracked_at_start = initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();
        let before = loose_objects(temp.path());

        std::fs::write(temp.path().join("tracked.rs"), "fn main() { changed(); }\n").unwrap();
        let deltas = capture_repository_deltas(
            &SystemGit,
            &roots,
            &BTreeMap::new(),
            &untracked_at_start,
            SESSION,
        )
        .unwrap();

        assert!(has_changes(&deltas));
        assert!(deltas[0].patch.contains("+fn main() { changed(); }"));
        assert!(
            !deltas[0].patch.contains("untracked-7.bin"),
            "an untouched untracked file is not this turn's work: {}",
            deltas[0].patch
        );
        // One blob for the changed file, one tree and the review point; the
        // 200 untracked files are neither read nor stored.
        let written = loose_objects(temp.path()) - before;
        assert!(
            written <= 3,
            "the review capture wrote {written} objects for a one-file change"
        );
    }

    /// An untracked file the turn created is this turn's work and must be in
    /// the review, which is what the recorded start list makes possible.
    #[test]
    fn a_review_reports_an_untracked_file_the_turn_created() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        std::fs::write(temp.path().join("already.txt"), "was here\n").unwrap();
        let untracked_at_start = initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();

        std::fs::write(temp.path().join("made.txt"), "the agent made this\n").unwrap();
        let deltas = capture_repository_deltas(
            &SystemGit,
            &roots,
            &BTreeMap::new(),
            &untracked_at_start,
            SESSION,
        )
        .unwrap();

        assert!(deltas[0].patch.contains("made.txt"), "{}", deltas[0].patch);
        assert!(
            !deltas[0].patch.contains("already.txt"),
            "{}",
            deltas[0].patch
        );
    }

    #[test]
    fn a_review_carries_unchanged_untracked_files_from_its_baseline() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        let first_start = initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();

        std::fs::write(temp.path().join("a.py"), "first version\n").unwrap();
        std::fs::write(temp.path().join("b.py"), "unchanged\n").unwrap();
        let first =
            capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new(), &first_start, SESSION)
                .unwrap();
        assert!(first[0].patch.contains("a.py"), "{}", first[0].patch);
        assert!(first[0].patch.contains("b.py"), "{}", first[0].patch);
        let baselines = captured_trees(&first);
        advance_baselines(&SystemGit, &baselines).unwrap();
        let second_start = read_untracked_at_start(&SystemGit, &roots).unwrap();

        std::fs::write(temp.path().join("a.py"), "a revised version\n").unwrap();
        let second =
            capture_repository_deltas(&SystemGit, &roots, &baselines, &second_start, SESSION)
                .unwrap();

        assert!(second[0].patch.contains("a revised version"));
        assert!(!second[0].patch.contains("b.py"), "{}", second[0].patch);
        assert_eq!(
            second[0]
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["a.py"]
        );
    }

    #[test]
    fn a_review_reports_an_untracked_file_removed_since_its_baseline() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        let first_start = initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();

        std::fs::write(temp.path().join("a.py"), "unchanged\n").unwrap();
        std::fs::write(temp.path().join("b.py"), "to be removed\n").unwrap();
        let first =
            capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new(), &first_start, SESSION)
                .unwrap();
        let baselines = captured_trees(&first);
        advance_baselines(&SystemGit, &baselines).unwrap();
        let second_start = read_untracked_at_start(&SystemGit, &roots).unwrap();

        std::fs::remove_file(temp.path().join("b.py")).unwrap();
        let second =
            capture_repository_deltas(&SystemGit, &roots, &baselines, &second_start, SESSION)
                .unwrap();

        assert!(second[0].patch.contains("b.py"), "{}", second[0].patch);
        assert!(!second[0].patch.contains("a.py"), "{}", second[0].patch);
        assert_eq!(
            second[0]
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["b.py"]
        );
    }
}

/// One untracked path as it stood when the session started.
///
/// Size and modification time, not content: recording the content of every
/// untracked file is exactly the cost this design exists to avoid. They are
/// enough to tell, at review time, a file the turn created from one that was
/// already there, and one the turn changed from one it left alone.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UntrackedEntry {
    pub path: PathBuf,
    pub size: u64,
    pub modified_ms: i64,
}

/// What `git status` says about one repository, split into the two sets a
/// capture needs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WorkspaceState {
    /// Tracked paths that differ from HEAD, staged or not, including deletions.
    pub dirty_tracked: Vec<PathBuf>,
    /// Untracked, non-ignored paths.
    pub untracked: Vec<UntrackedEntry>,
}

/// Reads the two sets from one `git status`.
///
/// This is a stat-walk: one `stat` per tracked file and a `readdir` per
/// directory. It reads no file contents, which is what separates it from a
/// capture and is why it is affordable on a very large working tree.
pub fn read_workspace_state(
    git: &dyn GitCommandRunner,
    repository: &Path,
) -> Result<WorkspaceState> {
    let output = run(
        git,
        repository,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--no-renames",
            "-z",
        ],
    )
    .with_context(|| format!("read the status of {}", repository.display()))?;
    anyhow::ensure!(
        output.status == 0,
        "reading the status of {} failed with status {}: {}",
        repository.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let mut state = WorkspaceState::default();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        // `XY <path>`: two status letters, a space, then the path.
        let Some(path) = record.get(3..) else {
            continue;
        };
        let path = PathBuf::from(String::from_utf8_lossy(path).into_owned());
        if record[0] == b'?' && record[1] == b'?' {
            let metadata = repository.join(&path).metadata().ok();
            state.untracked.push(UntrackedEntry {
                path,
                size: metadata.as_ref().map_or(0, std::fs::Metadata::len),
                modified_ms: metadata
                    .as_ref()
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|modified| {
                        modified
                            .duration_since(std::time::UNIX_EPOCH)
                            .ok()
                            .and_then(|since| i64::try_from(since.as_millis()).ok())
                    })
                    .unwrap_or_default(),
            });
        } else if record[0] != b'!' {
            state.dirty_tracked.push(path);
        }
    }
    Ok(state)
}

/// The untracked paths a turn added or changed, judged against how the
/// repository looked when the session started.
///
/// A file that is in both lists with the same size and modification time was
/// not touched, so it stays out of the capture and out of the review.
fn changed_untracked(start: &[UntrackedEntry], now: &[UntrackedEntry]) -> Vec<PathBuf> {
    let before: std::collections::HashMap<&Path, &UntrackedEntry> = start
        .iter()
        .map(|entry| (entry.path.as_path(), entry))
        .collect();
    now.iter()
        .filter(|entry| {
            before
                .get(entry.path.as_path())
                .is_none_or(|was| was.size != entry.size || was.modified_ms != entry.modified_ms)
        })
        .map(|entry| entry.path.clone())
        .collect()
}

/// How many untracked paths a repository may hold and still be reviewable.
///
/// The list of untracked paths is what lets a review tell a file the turn
/// created from one that was already there. Past this many, recording it is no
/// longer a cheap stat-walk and the workspace is not one anybody reviews a turn
/// in; the session is refused instead of quietly reviewing without coverage.
pub const UNTRACKED_REVIEW_LIMIT: usize = 50_000;

/// How long reading one repository's status may take before the session is
/// refused. This is a backstop against a pathological filesystem, not the
/// design: the read is a stat-walk and takes a couple of seconds even on a
/// half-million-file tree.
pub const WORKSPACE_STATE_TIMEOUT: Duration = Duration::from_secs(120);

/// A Git runner that gives every command a deadline.
///
/// The startup baseline runs before the worker can be reached, so a Git command
/// that never returns is a session that never starts and never says why. With a
/// deadline it becomes a refusal the user can act on.
pub struct BoundedGit {
    timeout: Duration,
    executor: mj_core::targets::CancellableProcessExecutor,
}

impl BoundedGit {
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            executor: mj_core::targets::CancellableProcessExecutor::new(std::sync::Arc::new(
                std::sync::atomic::AtomicBool::new(false),
            )),
        }
    }

    #[must_use]
    pub fn cancellation_guard(&self) -> mj_core::targets::ProcessCancellationGuard {
        self.executor.cancel_on_drop()
    }
}

impl GitCommandRunner for BoundedGit {
    fn run(&self, repository: &Path, command: &GitCommand) -> Result<GitOutput> {
        let mut spec = mj_core::targets::CommandSpec::new(
            "git",
            command
                .arguments
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned()),
        )
        .purpose("read the workspace state for a review baseline");
        spec.cwd = Some(repository.to_path_buf());
        for (name, value) in mj_checkpoint::archive::NON_INTERACTIVE_GIT_ENV {
            spec.env.insert(name.to_owned(), value.to_owned());
        }
        for (name, value) in &command.env {
            spec.env.insert(
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            );
        }
        // The executor's pipe handler feeds input while it drains output, and
        // gives up on it at the deadline like any other wait.
        if !command.stdin.is_empty() {
            spec = spec.with_sensitive_stdin(command.stdin.clone());
        }
        let executor = self.executor.clone().with_deadline(self.timeout);
        let output =
            mj_core::targets::CommandExecutor::execute(&executor, &spec).map_err(|error| {
                if executor.is_cancelled() && !self.executor.is_cancelled() {
                    anyhow::Error::new(mj_core::targets::CommandTimedOut {
                        program: spec.program.clone(),
                        purpose: spec.purpose.clone(),
                        timeout: self.timeout,
                    })
                } else {
                    error
                }
            })?;
        Ok(GitOutput {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[cfg(test)]
pub(super) mod push_tests {
    use super::*;
    use crate::test_support::git;
    use mj_checkpoint::archive::{SystemGit, record_reviewed_tree};

    pub(in crate::review) fn repository() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let origin = temp.path().join("origin.git");
        let root = temp.path().join("checkout");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "--bare", "-q", "-b", "main"]);
        git(
            temp.path(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                root.to_str().unwrap(),
            ],
        );
        git(&root, &["config", "user.name", "Review Test"]);
        git(&root, &["config", "user.email", "review@example.test"]);
        commit(&root, "base");
        git(&root, &["push", "-q", "origin", "main"]);
        git(&root, &["remote", "set-head", "origin", "main"]);
        temp
    }

    pub(in crate::review) fn commit(root: &Path, text: &str) -> String {
        std::fs::write(root.join("tracked.txt"), format!("{text}\n")).unwrap();
        git(root, &["add", "tracked.txt"]);
        git(root, &["commit", "-qm", text]);
        git(root, &["rev-parse", "HEAD"])
    }

    fn capture(root: &Path, start: &str) -> PushCapture {
        capture_pushable_commits(&SystemGit, root, start).unwrap()
    }

    #[test]
    fn push_capture_refuses_dirty_tracked_paths_and_ignores_untracked() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let head = git(&root, &["rev-parse", "HEAD"]);
        std::fs::write(root.join("untracked"), "ignored").unwrap();
        assert!(capture(&root, &head).delta.patch.is_empty());
        git(&root, &["mv", "tracked.txt", "renamed with spaces.txt"]);
        assert!(matches!(capture_pushable_commits(&SystemGit, &root, &head),
            Err(PushCaptureRefusal::DirtyTracked { files }) if files == [PathBuf::from("renamed with spaces.txt")]));
    }

    #[test]
    fn push_capture_uses_origin_default_merge_base_and_shared_diff_summary() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let base = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["checkout", "-qb", "feature"]);
        let head = commit(&root, "feature");
        let capture = capture(&root, &head);
        assert_eq!(
            capture.base,
            PushBase::MergeBase {
                default_branch: "refs/remotes/origin/main".into(),
                commit: base.clone()
            }
        );
        assert_eq!(capture.head_commit, head);
        assert_eq!(
            capture.delta.baseline_tree,
            Some(git(&root, &["rev-parse", &format!("{base}^{{tree}}")]))
        );
        assert!(capture.delta.patch.contains("+feature"));
        assert_eq!(capture.delta.changed_lines, 2);
        assert_eq!(capture.delta.files[0].path, "tracked.txt");
    }

    #[test]
    fn push_capture_uses_reviewed_descendant_and_handles_head_as_base() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["checkout", "-qb", "feature"]);
        let reviewed = commit(&root, "reviewed");
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &tree, &reviewed).unwrap();
        assert_eq!(
            git(&root, &["rev-parse", &format!("refs/mj/reviewed/{tree}")]),
            reviewed
        );
        let empty = capture(&root, &start);
        assert_eq!(
            empty.base,
            PushBase::ReviewPoint {
                commit: reviewed.clone()
            }
        );
        assert!(empty.delta.patch.is_empty());
        assert!(empty.delta.files.is_empty());
        assert_eq!(empty.delta.changed_lines, 0);
        commit(&root, "later");
        assert_eq!(capture(&root, &start).base.commit(), reviewed);
    }

    #[test]
    fn push_capture_orders_reviewed_ancestors_by_committer_date() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["checkout", "-qb", "feature"]);
        commit(&root, "reviewed with later clock");
        let amended = SystemGit
            .run(
                &root,
                &GitCommand {
                    arguments: ["commit", "--amend", "--no-edit", "-q"]
                        .map(Into::into)
                        .to_vec(),
                    stdin: Vec::new(),
                    env: vec![("GIT_COMMITTER_DATE".into(), "2030-01-01T00:00:00Z".into())],
                },
            )
            .unwrap();
        assert_eq!(amended.status, 0);
        let newer_date = git(&root, &["rev-parse", "HEAD"]);
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &tree, &newer_date).unwrap();
        let later = commit(&root, "reviewed with earlier clock");
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &tree, &later).unwrap();
        assert_eq!(capture(&root, &start).base.commit(), newer_date);
        git(&root, &["checkout", "-qb", "sibling", &start]);
        let sibling = commit(&root, "not an ancestor");
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &tree, &sibling).unwrap();
        git(&root, &["checkout", "-q", "feature"]);
        assert_eq!(capture(&root, &start).base.commit(), newer_date);
    }

    #[test]
    fn push_capture_ignores_reviewed_history_before_current_merge_base() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let old = git(&root, &["rev-parse", "HEAD"]);
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &tree, &old).unwrap();
        git(&root, &["checkout", "-qb", "feature"]);
        commit(&root, "feature before rebase");
        git(&root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("default-only"), "advance default\n").unwrap();
        git(&root, &["add", "default-only"]);
        git(&root, &["commit", "-qm", "advance default"]);
        let mb = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["push", "-q", "origin", "main"]);
        git(&root, &["checkout", "-q", "feature"]);
        git(&root, &["rebase", "main"]);
        assert_eq!(capture(&root, &old).base.commit(), mb);
    }

    #[test]
    fn push_capture_on_default_branch_uses_session_start() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        commit(&root, "later");
        git(&root, &["push", "-q", "origin", "main"]);
        assert_eq!(
            capture(&root, &start).base,
            PushBase::SessionStart { commit: start }
        );
    }

    #[test]
    fn push_capture_without_default_branch_uses_session_start() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["remote", "remove", "origin"]);
        git(&root, &["branch", "-m", "topic"]);
        commit(&root, "later");
        assert_eq!(
            capture(&root, &start).base,
            PushBase::SessionStart { commit: start }
        );
    }

    #[test]
    fn push_capture_without_origin_head_uses_local_main() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        git(
            &root,
            &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
        );
        git(&root, &["checkout", "-qb", "feature"]);
        let head = commit(&root, "later");
        assert_eq!(
            capture(&root, &head).base,
            PushBase::MergeBase {
                default_branch: "refs/heads/main".into(),
                commit: start
            }
        );
    }

    #[test]
    fn session_start_pin_survives_baseline_advancement_restart_and_missing_baseline() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let roots = std::slice::from_ref(&root);
        let start = git(&root, &["rev-parse", "HEAD"]);
        assert_eq!(
            session_start_head(&SystemGit, &root, "session").unwrap(),
            None
        );
        initialize_review_baselines(&SystemGit, roots, "session").unwrap();
        commit(&root, "later");
        assert_eq!(
            ensure_session_start_head(&SystemGit, &root, "session").unwrap(),
            Some(start.clone())
        );
        let deltas = capture_repository_deltas(
            &SystemGit,
            roots,
            &BTreeMap::new(),
            &BTreeMap::new(),
            "session",
        )
        .unwrap();
        advance_baselines(&SystemGit, &mj_review::delta::captured_trees(&deltas)).unwrap();
        initialize_review_baselines(&SystemGit, roots, "session").unwrap();
        assert_eq!(
            session_start_head(&SystemGit, &root, "session").unwrap(),
            Some(start.clone())
        );
        git(
            &root,
            &[
                "update-ref",
                "-d",
                &review_baseline_ref(&SystemGit, &root).unwrap(),
            ],
        );
        initialize_review_baselines(&SystemGit, roots, "session").unwrap();
        assert_eq!(
            session_start_head(&SystemGit, &root, "session").unwrap(),
            Some(start)
        );
        let moved = temp.path().join("moved");
        git(
            temp.path(),
            &[
                "clone",
                "-q",
                root.to_str().unwrap(),
                moved.to_str().unwrap(),
            ],
        );
        assert_eq!(
            session_start_head(&SystemGit, &moved, "session").unwrap(),
            None
        );
        let moved_head = git(&moved, &["rev-parse", "HEAD"]);
        assert_eq!(
            ensure_session_start_head(&SystemGit, &moved, "session").unwrap(),
            Some(moved_head.clone())
        );
        initialize_review_baselines(&SystemGit, std::slice::from_ref(&moved), "session").unwrap();
        assert_eq!(
            ensure_session_start_head(&SystemGit, &moved, "session").unwrap(),
            Some(moved_head)
        );
        let linked = temp.path().join("linked");
        git(
            &root,
            &["worktree", "add", "-qb", "linked", linked.to_str().unwrap()],
        );
        let linked_head = git(&linked, &["rev-parse", "HEAD"]);
        initialize_review_baselines(&SystemGit, std::slice::from_ref(&linked), "session").unwrap();
        assert_eq!(
            session_start_head(&SystemGit, &linked, "session").unwrap(),
            Some(linked_head)
        );
        assert_ne!(
            session_start_head(&SystemGit, &root, "session").unwrap(),
            session_start_head(&SystemGit, &linked, "session").unwrap()
        );
        // An old advancing baseline cannot reveal its original start commit.
        git(
            &root,
            &[
                "update-ref",
                "-d",
                &session_start_ref(&SystemGit, &root, "session").unwrap(),
            ],
        );
        let restored_head = git(&root, &["rev-parse", "HEAD"]);
        initialize_review_baselines(&SystemGit, roots, "session").unwrap();
        assert_eq!(
            ensure_session_start_head(&SystemGit, &root, "session").unwrap(),
            Some(restored_head)
        );
    }
    #[test]
    fn ensuring_session_start_waits_for_an_unborn_head_to_be_committed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.name", "Review Test"]);
        git(root, &["config", "user.email", "review@example.test"]);
        assert_eq!(
            ensure_session_start_head(&SystemGit, root, "session").unwrap(),
            None
        );
        let head = commit(root, "first commit");
        assert_eq!(
            ensure_session_start_head(&SystemGit, root, "session").unwrap(),
            Some(head.clone())
        );
        commit(root, "later commit");
        assert_eq!(
            ensure_session_start_head(&SystemGit, root, "session").unwrap(),
            Some(head)
        );
    }
}
