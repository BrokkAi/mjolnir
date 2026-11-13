//! Worker-owned Git capture for push review.

use anyhow::{Context, Result};
use mj_checkpoint::archive::{
    GitCommand, GitCommandRunner, GitOutput, diff_between_trees, numstat_between_trees,
    session_start_scope,
};
use mj_core::relay::{FileLineChange, RepoDelta};
use mj_review::delta::RawDiffSummary;
use mj_review::{REVIEW_CAPTURE_DIFF_LIMIT, bound_review_section};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    MergedReviewPoint {
        commit: String,
        merge_base: String,
        default_branch: String,
    },
    SessionStart {
        commit: String,
    },
    /// HEAD shares no history with the session start or a default branch,
    /// as on an orphan branch: every commit it holds is reviewed.
    Root,
}

impl PushBase {
    pub fn commit(&self) -> Option<&str> {
        match self {
            Self::ReviewPoint { commit }
            | Self::MergeBase { commit, .. }
            | Self::MergedReviewPoint { commit, .. }
            | Self::SessionStart { commit } => Some(commit),
            Self::Root => None,
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
    let mut base = if merge_base(runner, root, session_start_head, &head)?.is_some() {
        PushBase::SessionStart {
            commit: session_start_head.to_owned(),
        }
    } else {
        PushBase::Root
    };
    if let Some(default_branch) = default_branch(runner, root)?
        && let Some(mb) = merge_base(runner, root, &head, &default_branch)?
        && mb != head
    {
        base = PushBase::MergeBase {
            default_branch,
            commit: mb,
        };
    }
    // Git filters the reviewed refs to HEAD's history within the boundary,
    // then keeps the one that every other candidate leads to.
    let mut filters = vec![format!("--merged={head}")];
    if let Some(boundary) = base.commit() {
        filters.push(format!("--contains={boundary}"));
    }
    let eligible = reviewed_refs(runner, root, &filters)?;
    if let Some(commit) = select_reviewed_ancestor(runner, root, &eligible)? {
        base = PushBase::ReviewPoint { commit };
    }
    // Reviewed work merged with newer default-branch history: diff from Git's
    // merge of the two, so the reviewer sees only resolutions and later edits.
    let mut merged = None;
    let mut push_review_warning = None;
    if let PushBase::MergeBase {
        default_branch,
        commit: mb,
    } = &base
    {
        let eligible = reviewed_refs(
            runner,
            root,
            &[format!("--merged={head}"), format!("--no-merged={mb}")],
        )?;
        if let Some(reviewed) = select_reviewed_ancestor(runner, root, &eligible)? {
            let reviewed = reviewed.as_str();
            let version = checked_git_text(runner, root, &["version"])?;
            let found = mj_core::local_git::parse_git_version(&version);
            let floor = mj_core::local_git::MINIMUM_GIT_VERSION;
            if found.is_some_and(|found| found >= floor) {
                merged = Some(merged_review_tree(runner, root, reviewed, mb, &head)?);
            } else {
                push_review_warning = Some(format!(
                    "Pushed without review: Git {} in {} is older than {}.{}.{} or its version could not be read, so Mjolnir could not separate this merge's new changes from reviewed work. Upgrade Git to review merged work.",
                    version
                        .trim()
                        .strip_prefix("git version ")
                        .unwrap_or(version.trim()),
                    root.display(),
                    floor.0,
                    floor.1,
                    floor.2
                ));
            }
            base = PushBase::MergedReviewPoint {
                commit: reviewed.to_owned(),
                merge_base: mb.clone(),
                default_branch: default_branch.clone(),
            };
        }
    }
    let merged_in = match &base {
        PushBase::MergedReviewPoint { merge_base, .. } => vec![merge_base.clone()],
        _ => Vec::new(),
    };
    let current = checked_git_text(
        runner,
        root,
        &["rev-parse", "--verify", &format!("{head}^{{tree}}")],
    )?;
    let baseline = match (merged, base.commit()) {
        (Some(tree), _) => tree,
        (None, _) if push_review_warning.is_some() => current.clone(),
        (None, Some(commit)) => checked_git_text(
            runner,
            root,
            &["rev-parse", "--verify", &format!("{commit}^{{tree}}")],
        )?,
        (None, None) => mj_checkpoint::archive::empty_tree_id(runner, root)?,
    };
    let (patch, files) = capture_diff(runner, root, &baseline, &current)?;
    let mut delta = build_delta(root, Some(baseline), current, patch, files, merged_in, None);
    delta.base_commit = base.commit().map(str::to_owned);
    delta.push_review_warning = push_review_warning;
    delta.head_commit = Some(head.clone());
    delta.base_kind = Some(match &base {
        PushBase::ReviewPoint { .. } => "review point".into(),
        PushBase::MergeBase { default_branch, .. } => format!("merge base with {default_branch}"),
        PushBase::MergedReviewPoint { default_branch, .. } => {
            format!("review point merged with {default_branch}")
        }
        PushBase::SessionStart { .. } => "session start".into(),
        PushBase::Root => "no shared history".into(),
    });
    Ok(PushCapture {
        delta,
        base,
        head_commit: head,
    })
}

/// The tree Git makes by merging the reviewed point with default-branch work,
/// as the base for reviewing `head`.
///
/// A text conflict leaves markers in that tree, so the diff to `head` shows its
/// resolution. A binary, modify/delete or submodule conflict leaves one side's
/// version instead; when `head` resolved it to that same version, the diff
/// would show nothing. Such paths are removed from the base, so the reviewer
/// sees each in full.
fn merged_review_tree(
    runner: &dyn GitCommandRunner,
    root: &Path,
    reviewed: &str,
    merge_base_commit: &str,
    head: &str,
) -> Result<String> {
    let output = run(
        runner,
        root,
        &[
            "merge-tree",
            "--write-tree",
            "--allow-unrelated-histories",
            "--no-messages",
            "--name-only",
            "-z",
            reviewed,
            merge_base_commit,
        ],
    )?;
    // Status 1 means conflicts; the tree is still written.
    if !matches!(output.status, 0 | 1) {
        anyhow::bail!(
            "git merge-tree failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // Paths are raw bytes: a file name need not be UTF-8.
    let mut fields = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let tree = std::str::from_utf8(fields.next().context("git merge-tree printed no tree id")?)?
        .to_owned();
    let conflicted = fields
        .map(mj_core::path_input::from_git_bytes)
        .collect::<Result<BTreeSet<_>>>()?;
    if conflicted.is_empty() {
        return Ok(tree);
    }
    let merged = tree_entries(runner, root, &tree, &conflicted)?;
    let resolved = tree_entries(runner, root, head, &conflicted)?;
    let hidden: Vec<PathBuf> = conflicted
        .into_iter()
        .filter(|path| merged.get(path) == resolved.get(path))
        .collect();
    if hidden.is_empty() {
        return Ok(tree);
    }
    mj_checkpoint::archive::tree_without_paths(runner, root, &tree, &hidden)
}

/// The mode and object of each of `paths` that `tree_ish` holds.
fn tree_entries(
    runner: &dyn GitCommandRunner,
    root: &Path,
    tree_ish: &str,
    paths: &BTreeSet<PathBuf>,
) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut arguments: Vec<std::ffi::OsString> =
        ["--literal-pathspecs", "ls-tree", "-r", "-z", tree_ish, "--"]
            .map(Into::into)
            .to_vec();
    arguments.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
    let output = runner.run(
        root,
        &GitCommand {
            arguments,
            stdin: Vec::new(),
            env: Vec::new(),
        },
    )?;
    anyhow::ensure!(
        output.status == 0,
        "git ls-tree failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let mut entries = BTreeMap::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if let Some(tab) = record.iter().position(|byte| *byte == b'\t') {
            entries.insert(
                mj_core::path_input::from_git_bytes(&record[tab + 1..])?,
                record[..tab].to_vec(),
            );
        }
    }
    Ok(entries)
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
    for branch in [
        "refs/remotes/origin/main",
        "refs/remotes/origin/master",
        "refs/heads/main",
        "refs/heads/master",
    ] {
        if resolve_commit(runner, root, branch)?.is_some() {
            return Ok(Some(branch.to_owned()));
        }
    }
    Ok(None)
}

/// The best common ancestor of two commits, or `None` when they share no history.
fn merge_base(
    runner: &dyn GitCommandRunner,
    root: &Path,
    one: &str,
    other: &str,
) -> Result<Option<String>> {
    let output = run(runner, root, &["merge-base", one, other])?;
    match output.status {
        0 => Ok(Some(String::from_utf8(output.stdout)?.trim().to_owned())),
        1 => Ok(None),
        _ => anyhow::bail!(
            "find a merge base: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

pub(crate) fn checked_git(
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

/// Makes `revision` the base of the next push review of `root`. The commit's
/// tree must already be recorded as reviewed or overridden, which is what lets
/// the agent move the base only to work a review has seen. Returns the commit
/// and tree.
pub fn set_review_base(
    runner: &dyn GitCommandRunner,
    root: &Path,
    revision: &str,
) -> Result<(String, String)> {
    anyhow::ensure!(
        !revision.is_empty() && !revision.starts_with('-'),
        "{revision:?} is not a commit; pass a commit hash from a review verdict"
    );
    let resolve = |suffix: &str| -> Result<String> {
        checked_git_text(
            runner,
            root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{revision}{suffix}"),
            ],
        )
        .map_err(|_| {
            anyhow::anyhow!(
                "{revision} is not a commit in {}; pass a commit hash from a review verdict",
                root.display()
            )
        })
    };
    let commit = resolve("^{commit}")?;
    let tree = resolve("^{tree}")?;
    let recorded = checked_git_text(
        runner,
        root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/mj/reviewed/{tree}"),
            &format!("refs/mj/overridden/{tree}"),
        ],
    )?;
    anyhow::ensure!(
        !recorded.is_empty(),
        "commit {commit} was never reviewed or overridden in {}; call request_review first, then call set_review_baseline once the verdict is clean or you have overridden it",
        root.display()
    );
    record_review_base(runner, root, &tree, &commit)?;
    Ok((commit, tree))
}

/// Records a clean review or an accepted explicit baseline in one transaction.
pub fn record_review_base(
    runner: &dyn GitCommandRunner,
    root: &Path,
    tree: &str,
    commit: &str,
) -> Result<()> {
    let selected = worktree_review_base_ref(runner, root)?;
    let output = runner.run(
        root,
        &GitCommand {
            arguments: ["update-ref", "--stdin"].map(Into::into).to_vec(),
            stdin: format!("update refs/mj/reviewed/{tree} {commit}\nupdate {selected} {commit}\n")
                .into_bytes(),
            env: Vec::new(),
        },
    )?;
    anyhow::ensure!(
        output.status == 0,
        "set the review base: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn worktree_review_base_ref(runner: &dyn GitCommandRunner, root: &Path) -> Result<String> {
    Ok(mj_checkpoint::archive::push_review_base_ref(
        &session_start_scope(runner, root)?,
    ))
}

struct ReviewedRefs {
    commits: Vec<String>,
    selected: Option<String>,
}

/// Reviewed commits under `refs/mj/reviewed/` that match the given
/// `for-each-ref` filters, including this worktree's selected base.
fn reviewed_refs(
    runner: &dyn GitCommandRunner,
    root: &Path,
    filters: &[String],
) -> Result<ReviewedRefs> {
    let selected = worktree_review_base_ref(runner, root)?;
    let mut args = vec!["for-each-ref", "--format=%(objectname) %(refname)"];
    args.extend(filters.iter().map(String::as_str));
    args.push("refs/mj/reviewed/");
    args.push(&selected);
    let mut refs = ReviewedRefs {
        commits: Vec::new(),
        selected: None,
    };
    for line in checked_git_text(runner, root, &args)?.lines() {
        let (commit, name) = line
            .split_once(' ')
            .context("read the reviewed ref's name")?;
        refs.commits.push(commit.to_owned());
        if name == selected {
            refs.selected = Some(commit.to_owned());
        }
    }
    Ok(refs)
}

/// Advance when every candidate leads to one reviewed commit. When reviewed
/// histories are incomparable, retain the worktree's eligible selected base.
fn select_reviewed_ancestor(
    runner: &dyn GitCommandRunner,
    root: &Path,
    candidates: &ReviewedRefs,
) -> Result<Option<String>> {
    if candidates.commits.is_empty() {
        return Ok(None);
    }
    let mut args = vec!["merge-base", "--independent"];
    args.extend(candidates.commits.iter().map(String::as_str));
    let tips = checked_git_text(runner, root, &args)?;
    let mut tips = tips.lines();
    if let (Some(tip), None) = (tips.next(), tips.next()) {
        return Ok(Some(tip.to_owned()));
    }
    Ok(candidates.selected.clone())
}

pub fn later_review_base(
    runner: &dyn GitCommandRunner,
    root: &Path,
    revision: &str,
) -> Result<Option<(String, String)>> {
    anyhow::ensure!(
        !revision.is_empty() && !revision.starts_with('-'),
        "invalid review commit"
    );
    let commit = checked_git_text(
        runner,
        root,
        &["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
    )?;
    let head = checked_git_text(runner, root, &["rev-parse", "HEAD"])?;
    let later = reviewed_refs(
        runner,
        root,
        &[format!("--contains={commit}"), format!("--merged={head}")],
    )?;
    let Some(candidate) =
        select_reviewed_ancestor(runner, root, &later)?.filter(|candidate| *candidate != commit)
    else {
        return Ok(None);
    };
    let tree = checked_git_text(
        runner,
        root,
        &["rev-parse", &format!("{candidate}^{{tree}}")],
    )?;
    Ok(Some((candidate, tree)))
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
        push_review_warning: None,
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
    let baseline = session_start_scope(runner, root)?;
    let prefix = format!(
        "refs/mj/session-start/{}",
        mj_core::hex::lower_hex(Sha256::digest(session.as_bytes()))
    );
    let reference = format!(
        "{prefix}/{}",
        mj_core::hex::lower_hex(Sha256::digest(baseline.as_bytes()))
    );
    let legacy_baseline = baseline.replacen("refs/mj/", "refs/hel/", 1);
    let legacy = format!(
        "{prefix}/{}",
        mj_core::hex::lower_hex(Sha256::digest(legacy_baseline.as_bytes()))
    );
    mj_checkpoint::archive::migrate_review_ref(runner, root, &legacy, &reference)?;
    Ok(reference)
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

/// Where push coverage starts in a linked worktree the session did not start
/// in: the fork point of its HEAD with the session's start in `origin`, a
/// workspace checkout of the same repository. Pinning the worktree's own start
/// at its current HEAD would leave commits made before its first review with
/// nothing to review when no default branch bounds them. Without a shared
/// history the session start is returned as is, and the capture reviews
/// every commit HEAD holds.
pub fn linked_worktree_start(
    runner: &dyn GitCommandRunner,
    worktree: &Path,
    origin: &Path,
    session: &str,
) -> Result<Option<String>> {
    let (Some(head), Some(start)) = (
        resolve_head(runner, worktree)?,
        ensure_session_start_head(runner, origin, session)?,
    ) else {
        return ensure_session_start_head(runner, worktree, session);
    };
    Ok(Some(
        merge_base(runner, worktree, &head, &start)?.unwrap_or(start),
    ))
}

/// Where a review's coverage starts. A recorded session start wins. Without
/// one, the merge base with the default branch bounds the session's work when
/// HEAD has moved past it, and then the newest reviewed commit in HEAD's
/// history does. Otherwise nothing tells the session's commits from
/// older history: the current HEAD is recorded as the start so later commits
/// can be reviewed, and this review is refused instead of reporting the
/// session's earlier commits as reviewed.
pub fn review_start_head(
    runner: &dyn GitCommandRunner,
    root: &Path,
    session: &str,
) -> Result<Option<String>> {
    if let Some(start) = session_start_head(runner, root, session)? {
        return Ok(Some(start));
    }
    let Some(head) = resolve_head(runner, root)? else {
        return Ok(None);
    };
    if let Some(branch) = default_branch(runner, root)?
        && let Some(base) = merge_base(runner, root, &head, &branch)?
        && base != head
    {
        return Ok(Some(base));
    }
    // An earlier review of HEAD's history bounds the work that follows it.
    let reviewed = reviewed_refs(runner, root, &[format!("--merged={head}")])?;
    if let Some(reviewed) = select_reviewed_ancestor(runner, root, &reviewed)? {
        return Ok(Some(reviewed));
    }
    ensure_session_start_head(runner, root, session)?;
    anyhow::bail!(
        "No starting point was recorded for this session's work in {}, so its commits cannot be told apart from earlier history. Mjolnir now records the current commit as the start; a later review covers the commits made after it.",
        root.display()
    )
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

pub(crate) fn repository_root(git: &dyn GitCommandRunner, directory: &Path) -> Option<PathBuf> {
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

pub const WORKSPACE_STATE_TIMEOUT: Duration = Duration::from_secs(120);

/// A Git runner that gives every command a deadline.
///
/// Push-hook preparation uses a deadline so a stuck Git command reports a failure.
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
        .purpose("prepare push review");
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

    struct VersionGit(&'static str);

    impl GitCommandRunner for VersionGit {
        fn run(&self, root: &Path, command: &GitCommand) -> Result<GitOutput> {
            if command.arguments.len() == 1 && command.arguments[0] == "version" {
                return Ok(GitOutput {
                    status: 0,
                    stdout: format!("git version {}\n", self.0).into_bytes(),
                    stderr: Vec::new(),
                });
            }
            SystemGit.run(root, command)
        }
    }

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
        assert_eq!(
            capture(&root, &start).base.commit(),
            Some(reviewed.as_str())
        );
    }

    // An overridden tree is pushable but is not a review point, so the next
    // review still covers the whole change.
    #[test]
    fn push_capture_ignores_overridden_commits_as_a_base() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["checkout", "-qb", "feature"]);
        let overridden = commit(&root, "overridden");
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        mj_checkpoint::archive::record_overridden_tree(&SystemGit, &root, &tree, &overridden)
            .unwrap();
        commit(&root, "fix");
        let capture = capture(&root, &start);
        assert_eq!(
            capture.base.commit(),
            Some(start.as_str()),
            "the base stays at the merge base, not the overridden commit"
        );
        assert!(
            capture.delta.patch.contains("+overridden") || capture.delta.patch.contains("+fix")
        );
        assert!(git(&root, &["for-each-ref", "refs/mj/reviewed/"]).is_empty());
    }

    #[test]
    fn set_review_base_accepts_reviewed_and_overridden_trees_and_refuses_others() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["checkout", "-qb", "feature"]);
        let reviewed = commit(&root, "reviewed");
        let reviewed_tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &reviewed_tree, &reviewed).unwrap();
        commit(&root, "overridden");
        // Review points are ordered by committer date, which has one-second
        // resolution; give the overridden commit a clearly later one.
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
        let overridden = git(&root, &["rev-parse", "HEAD"]);
        let overridden_tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        mj_checkpoint::archive::record_overridden_tree(
            &SystemGit,
            &root,
            &overridden_tree,
            &overridden,
        )
        .unwrap();
        let unreviewed = commit(&root, "unreviewed");

        let error = set_review_base(&SystemGit, &root, &unreviewed).unwrap_err();
        assert!(
            error.to_string().contains("call request_review first"),
            "{error}"
        );
        let error = set_review_base(&SystemGit, &root, "no-such-revision").unwrap_err();
        assert!(error.to_string().contains("is not a commit"), "{error}");
        let error = set_review_base(&SystemGit, &root, "--output=x").unwrap_err();
        assert!(error.to_string().contains("is not a commit"), "{error}");
        assert!(
            git(&root, &["for-each-ref", "refs/mj/reviewed/"])
                .lines()
                .count()
                == 1
        );

        // A rejected explicit-ref write must not promote an overridden tree.
        git(&root, &["update-ref", "refs/mj/push-base", &reviewed]);
        let error = set_review_base(&SystemGit, &root, &overridden).unwrap_err();
        assert!(error.to_string().contains("set the review base"), "{error}");
        assert_eq!(
            git(
                &root,
                &[
                    "for-each-ref",
                    "--format=%(objectname)",
                    "refs/mj/reviewed/"
                ]
            ),
            reviewed
        );
        git(&root, &["update-ref", "-d", "refs/mj/push-base"]);

        // Any revision works: HEAD~1 is the overridden commit. It becomes a
        // review point and the next capture starts there.
        assert_eq!(
            set_review_base(&SystemGit, &root, "HEAD~1").unwrap(),
            (overridden.clone(), overridden_tree.clone())
        );
        assert_eq!(
            git(
                &root,
                &["rev-parse", &format!("refs/mj/reviewed/{overridden_tree}")]
            ),
            overridden
        );
        assert_eq!(
            capture(&root, &start).base.commit(),
            Some(overridden.as_str())
        );
        // An abbreviated hash names a reviewed commit as well.
        assert_eq!(
            set_review_base(&SystemGit, &root, &reviewed[..10])
                .unwrap()
                .0,
            reviewed
        );
    }

    #[test]
    fn push_capture_orders_reviewed_ancestors_by_ancestry() {
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
        assert_eq!(capture(&root, &start).base.commit(), Some(later.as_str()));
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
        let later = git(&root, &["rev-parse", "HEAD"]);
        record_reviewed_tree(&SystemGit, &root, &tree, &later).unwrap();
        assert_eq!(capture(&root, &start).base.commit(), Some(later.as_str()));
        git(&root, &["checkout", "-qb", "sibling", &start]);
        let sibling = commit(&root, "not an ancestor");
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &tree, &sibling).unwrap();
        git(&root, &["checkout", "-q", "feature"]);
        assert_eq!(capture(&root, &start).base.commit(), Some(later.as_str()));
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
        assert_eq!(capture(&root, &old).base.commit(), Some(mb.as_str()));
    }

    #[test]
    fn push_capture_uses_reviewed_merge_tree_for_renames() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["checkout", "-qb", "feature"]);
        std::fs::write(root.join("tracked.txt"), "reviewed edit\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-qm", "reviewed edit"]);
        let reviewed = git(&root, &["rev-parse", "HEAD"]);
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &tree, &reviewed).unwrap();

        git(&root, &["checkout", "-q", "main"]);
        git(&root, &["mv", "tracked.txt", "renamed.txt"]);
        git(&root, &["commit", "-qm", "rename upstream file"]);
        let mb = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["push", "-q", "origin", "main"]);
        git(&root, &["checkout", "-q", "feature"]);
        git(&root, &["merge", "-q", "main"]);

        let captured = capture(&root, &start);
        assert_eq!(captured.base.commit(), Some(reviewed.as_str()));
        assert_eq!(captured.delta.merged_in, vec![mb]);
        assert!(captured.delta.patch.is_empty(), "{}", captured.delta.patch);
        assert!(captured.delta.files.is_empty());
    }

    #[test]
    fn push_capture_old_git_records_merged_work_without_review() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["checkout", "-qb", "feature"]);
        let reviewed = commit(&root, "reviewed");
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &tree, &reviewed).unwrap();
        git(&root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("upstream-only"), "upstream\n").unwrap();
        git(&root, &["add", "upstream-only"]);
        git(&root, &["commit", "-qm", "upstream change"]);
        git(&root, &["push", "-q", "origin", "main"]);
        git(&root, &["checkout", "-q", "feature"]);
        git(&root, &["merge", "-q", "main"]);

        let captured = capture_pushable_commits(&VersionGit("2.34.1"), &root, &start).unwrap();
        assert_eq!(
            captured.delta.baseline_tree.as_deref(),
            Some(captured.delta.current_tree.as_str())
        );
        assert!(captured.delta.patch.is_empty());
        assert!(
            captured
                .delta
                .push_review_warning
                .as_deref()
                .unwrap()
                .contains("Git 2.34.1")
        );
        let new_git = capture_pushable_commits(&VersionGit("2.39.5"), &root, &start).unwrap();
        let expected = git(
            &root,
            &[
                "merge-tree",
                "--write-tree",
                "--allow-unrelated-histories",
                &reviewed,
                &git(&root, &["rev-parse", "main"]),
            ],
        );
        assert_eq!(
            new_git.delta.baseline_tree.as_deref(),
            Some(expected.as_str())
        );
        assert!(new_git.delta.push_review_warning.is_none());
    }

    #[test]
    fn push_capture_merges_unrelated_reviewed_and_default_histories() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["checkout", "-qb", "feature"]);
        let reviewed = commit(&root, "reviewed");
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &tree, &reviewed).unwrap();
        git(&root, &["checkout", "--orphan", "new-main"]);
        git(&root, &["rm", "-qrf", "."]);
        std::fs::write(root.join("upstream-only"), "upstream\n").unwrap();
        git(&root, &["add", "upstream-only"]);
        git(&root, &["commit", "-qm", "new upstream root"]);
        git(&root, &["branch", "-f", "main"]);
        git(&root, &["push", "-qf", "origin", "main"]);
        git(&root, &["checkout", "-q", "feature"]);
        git(
            &root,
            &["merge", "-q", "--allow-unrelated-histories", "main"],
        );
        let captured = capture(&root, &start);
        assert!(matches!(captured.base, PushBase::MergedReviewPoint { .. }));
        assert!(captured.delta.patch.is_empty());
        assert!(captured.delta.push_review_warning.is_none());
    }

    #[test]
    fn push_capture_uses_merged_review_baseline_and_keeps_later_edits() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["checkout", "-qb", "feature"]);
        let reviewed = commit(&root, "reviewed");
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &tree, &reviewed).unwrap();
        set_review_base(&SystemGit, &root, &reviewed).unwrap();

        git(&root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("upstream-only"), "upstream\n").unwrap();
        git(&root, &["add", "upstream-only"]);
        git(&root, &["commit", "-qm", "upstream change"]);
        let mb = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["push", "-q", "origin", "main"]);
        git(&root, &["checkout", "-q", "feature"]);
        git(&root, &["merge", "-q", "main"]);

        let merged = capture(&root, &start);
        assert_eq!(merged.base.commit(), Some(reviewed.as_str()));
        assert_eq!(merged.delta.merged_in, vec![mb.clone()]);
        assert!(merged.delta.patch.is_empty());
        assert!(merged.delta.files.is_empty());

        commit(&root, "post-merge edit");
        let later = capture(&root, &start);
        assert_eq!(later.delta.merged_in, vec![mb]);
        assert!(later.delta.patch.contains("+post-merge edit"));
    }

    #[test]
    fn push_capture_marks_conflicts_in_merged_review_baseline() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["checkout", "-qb", "feature"]);
        std::fs::write(root.join("tracked.txt"), "branch resolution\n").unwrap();
        std::fs::write(root.join("branch-only"), "reviewed elsewhere\n").unwrap();
        git(&root, &["add", "tracked.txt", "branch-only"]);
        git(&root, &["commit", "-qm", "reviewed branch change"]);
        let reviewed = git(&root, &["rev-parse", "HEAD"]);
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &tree, &reviewed).unwrap();

        git(&root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("tracked.txt"), "upstream resolution\n").unwrap();
        std::fs::write(root.join("upstream-only"), "upstream\n").unwrap();
        git(&root, &["add", "tracked.txt", "upstream-only"]);
        git(&root, &["commit", "-qm", "upstream conflict change"]);
        let mb = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["push", "-q", "origin", "main"]);
        git(&root, &["checkout", "-q", "feature"]);
        assert!(
            SystemGit
                .run(
                    &root,
                    &GitCommand {
                        arguments: ["merge", "main"].map(Into::into).to_vec(),
                        stdin: Vec::new(),
                        env: Vec::new(),
                    }
                )
                .unwrap()
                .status
                != 0
        );
        std::fs::write(root.join("tracked.txt"), "hand resolved\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-qm", "resolve merge"]);

        let captured = capture(&root, &start);
        assert_eq!(captured.base.commit(), Some(reviewed.as_str()));
        assert_eq!(captured.delta.merged_in, vec![mb]);
        assert!(captured.delta.patch.contains("hand resolved"));
        // Only the hand resolution is new: the base holds both sides inside
        // conflict markers, and neither side's other files.
        let files: Vec<_> = captured.delta.files.iter().map(|file| &file.path).collect();
        assert_eq!(files, ["tracked.txt"]);
        assert!(
            captured.delta.patch.contains("-<<<<<<<"),
            "{}",
            captured.delta.patch
        );
    }

    #[test]
    fn push_capture_shows_non_text_conflicts_resolved_to_the_merge_tree_side() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        std::fs::write(root.join("image.bin"), b"\0base").unwrap();
        std::fs::write(root.join("kept.txt"), "base\n").unwrap();
        git(&root, &["add", "image.bin", "kept.txt"]);
        git(&root, &["commit", "-qm", "shared files"]);
        git(&root, &["push", "-q", "origin", "main"]);
        git(&root, &["checkout", "-qb", "feature"]);
        std::fs::write(root.join("image.bin"), b"\0branch").unwrap();
        std::fs::write(root.join("kept.txt"), "branch edit\n").unwrap();
        git(&root, &["add", "image.bin", "kept.txt"]);
        git(&root, &["commit", "-qm", "reviewed branch change"]);
        let reviewed = git(&root, &["rev-parse", "HEAD"]);
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &tree, &reviewed).unwrap();

        git(&root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("image.bin"), b"\0upstream").unwrap();
        std::fs::write(root.join("upstream-only"), "upstream\n").unwrap();
        git(&root, &["add", "image.bin", "upstream-only"]);
        git(&root, &["rm", "-q", "kept.txt"]);
        git(&root, &["commit", "-qm", "upstream conflicting change"]);
        git(&root, &["push", "-q", "origin", "main"]);
        git(&root, &["checkout", "-q", "feature"]);
        assert_ne!(
            SystemGit
                .run(
                    &root,
                    &GitCommand {
                        arguments: ["merge", "main"].map(Into::into).to_vec(),
                        stdin: Vec::new(),
                        env: Vec::new(),
                    }
                )
                .unwrap()
                .status,
            0
        );
        // Keep the branch's side of both conflicts: the version merge-tree
        // itself writes for a binary and a modify/delete conflict.
        git(&root, &["checkout", "--ours", "--", "image.bin"]);
        git(&root, &["add", "image.bin", "kept.txt"]);
        git(&root, &["commit", "-qm", "resolve merge"]);

        let captured = capture(&root, &start);
        assert_eq!(captured.base.commit(), Some(reviewed.as_str()));
        let files: Vec<_> = captured.delta.files.iter().map(|file| &file.path).collect();
        assert_eq!(files, ["image.bin", "kept.txt"], "{}", captured.delta.patch);
    }

    // Hard-won: 9be41400f: macOS rejects the non-UTF-8 filesystem fixture.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn push_capture_shows_conflicted_paths_that_are_not_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::OsStr::from_bytes(b"image-\xff.bin");
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        std::fs::write(root.join(name), b"\0base").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "shared file"]);
        git(&root, &["push", "-q", "origin", "main"]);
        git(&root, &["checkout", "-qb", "feature"]);
        std::fs::write(root.join(name), b"\0branch").unwrap();
        git(&root, &["commit", "-qam", "reviewed branch change"]);
        let reviewed = git(&root, &["rev-parse", "HEAD"]);
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &tree, &reviewed).unwrap();
        git(&root, &["checkout", "-q", "main"]);
        std::fs::write(root.join(name), b"\0upstream").unwrap();
        git(&root, &["commit", "-qam", "upstream conflicting change"]);
        git(&root, &["push", "-q", "origin", "main"]);
        git(&root, &["checkout", "-q", "feature"]);
        assert_ne!(
            SystemGit
                .run(
                    &root,
                    &GitCommand {
                        arguments: ["merge", "main"].map(Into::into).to_vec(),
                        stdin: Vec::new(),
                        env: Vec::new(),
                    }
                )
                .unwrap()
                .status,
            0
        );
        std::fs::write(root.join(name), b"\0branch").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "resolve merge"]);

        let captured = capture(&root, &start);
        assert_eq!(captured.base.commit(), Some(reviewed.as_str()));
        assert_eq!(captured.delta.files.len(), 1, "{}", captured.delta.patch);
    }

    #[test]
    fn push_capture_shows_submodule_changes_that_diff_settings_ignore() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["config", "diff.ignoreSubmodules", "all"]);
        git(&root, &["checkout", "-qb", "feature"]);
        git(
            &root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{start},vendored"),
            ],
        );
        git(&root, &["commit", "-qm", "point at a submodule commit"]);

        let captured = capture(&root, &start);
        let files: Vec<_> = captured.delta.files.iter().map(|file| &file.path).collect();
        assert_eq!(files, ["vendored"], "{}", captured.delta.patch);
        assert!(captured.delta.patch.contains("Subproject commit"));
    }

    // Hard-won: f92186373: a first manual review with no recorded start
    // pinned the current HEAD and reported the session's commits as reviewed.
    #[test]
    fn a_review_without_a_recorded_start_never_reports_earlier_commits_as_reviewed() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let main = git(&root, &["rev-parse", "HEAD"]);
        commit(&root, "on main");
        git(&root, &["push", "-q", "origin", "main"]);
        let error = review_start_head(&SystemGit, &root, "unpinned").unwrap_err();
        assert!(
            error.to_string().contains("No starting point was recorded"),
            "{error:#}"
        );
        let head = git(&root, &["rev-parse", "HEAD"]);
        assert_eq!(
            session_start_head(&SystemGit, &root, "unpinned").unwrap(),
            Some(head),
            "later reviews start at the commit recorded now"
        );

        let reviewed = git(&root, &["rev-parse", "HEAD"]);
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &tree, &reviewed).unwrap();
        commit(&root, "after a review");
        git(&root, &["push", "-q", "origin", "main"]);
        assert_eq!(
            review_start_head(&SystemGit, &root, "reviewed").unwrap(),
            Some(reviewed),
            "an earlier reviewed commit bounds the work after it"
        );
        assert_eq!(
            session_start_head(&SystemGit, &root, "reviewed").unwrap(),
            None
        );

        git(&root, &["checkout", "-qb", "feature", &main]);
        commit(&root, "on a branch");
        assert_eq!(
            review_start_head(&SystemGit, &root, "branch").unwrap(),
            Some(main),
            "a branch is bounded by its merge base with the default branch"
        );
        assert_eq!(
            session_start_head(&SystemGit, &root, "branch").unwrap(),
            None
        );
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
    fn push_capture_without_origin_head_prefers_remote_main_from_local_master() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let start = git(&root, &["rev-parse", "HEAD"]);
        git(
            &root,
            &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
        );
        git(&root, &["branch", "master", &start]);
        git(&root, &["checkout", "-q", "master"]);
        let reviewed = commit(&root, "reviewed on master");
        let reviewed_tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_reviewed_tree(&SystemGit, &root, &reviewed_tree, &reviewed).unwrap();
        git(&root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("upstream-only"), "upstream\n").unwrap();
        git(&root, &["add", "upstream-only"]);
        git(&root, &["commit", "-qm", "advance upstream"]);
        let mb = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["push", "-q", "origin", "main"]);
        git(&root, &["checkout", "-q", "master"]);
        git(&root, &["merge", "-q", "origin/main"]);

        let captured = capture(&root, &start);
        assert_eq!(
            captured.base,
            PushBase::MergedReviewPoint {
                commit: reviewed,
                merge_base: mb.clone(),
                default_branch: "refs/remotes/origin/main".into()
            }
        );
        assert_eq!(captured.delta.merged_in, vec![mb]);
        assert!(captured.delta.patch.is_empty());
    }

    #[test]
    fn session_start_pin_survives_restart_and_worktree_moves() {
        let temp = repository();
        let root = temp.path().join("checkout");

        let start = git(&root, &["rev-parse", "HEAD"]);
        assert_eq!(
            session_start_head(&SystemGit, &root, "session").unwrap(),
            None
        );
        ensure_session_start_head(&SystemGit, &root, "session").unwrap();
        commit(&root, "later");
        assert_eq!(
            ensure_session_start_head(&SystemGit, &root, "session").unwrap(),
            Some(start.clone())
        );
        assert_eq!(
            session_start_head(&SystemGit, &root, "session").unwrap(),
            Some(start.clone())
        );
        ensure_session_start_head(&SystemGit, &root, "session").unwrap();
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
        ensure_session_start_head(&SystemGit, &moved, "session").unwrap();
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
        ensure_session_start_head(&SystemGit, &linked, "session").unwrap();
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
        ensure_session_start_head(&SystemGit, &root, "session").unwrap();
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
