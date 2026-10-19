//! Worker-owned Git capture for turn review.

use anyhow::{Context, Result};
use mj_checkpoint::archive::{
    GitCommand, GitCommandRunner, GitOutput, ReviewPoint, capture_paths, diff_between_trees,
    merge_trees, numstat_between_trees, pin_review_tree, read_review_point, record_review_point,
    remove_departed_worktree_pins, review_baseline_ref,
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
        // review point records it, so the two cannot disagree.
        let head = resolve_head(git, root)?;
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
        let current = record_review_point(git, root, &tree, head.as_deref(), session)
            .with_context(|| format!("record the capture of {}", root.display()))?;
        let (base, patch, files) = match &baseline {
            Some(baseline) => {
                let base = review_base(git, root, baseline, head.as_deref())
                    .with_context(|| format!("separate merged-in history in {}", root.display()))?;
                let patch = diff_between_trees(git, root, Some(&base.tree), &tree)
                    .with_context(|| format!("diff the captured trees of {}", root.display()))?;
                let numstat = numstat_between_trees(git, root, &base.tree, &tree)
                    .with_context(|| format!("count the changed lines of {}", root.display()))?;
                (Some(base), patch, parse_numstat(&numstat))
            }
            // Neither baseline is usable. Keep the coverage reset honest: an
            // empty-tree diff would report the restored or pre-existing
            // repository as work from this turn.
            None => (None, String::new(), Vec::new()),
        };
        let summary = RawDiffSummary::from_patch(&patch);
        let (baseline_tree, merged_in) =
            base.map_or((None, Vec::new()), |base| (Some(base.tree), base.merged_in));
        deltas.push(RepoDelta {
            root: root.clone(),
            baseline_tree,
            current_tree: current,
            patch: bound_review_section(&patch, REVIEW_CAPTURE_DIFF_LIMIT, "workspace diff"),
            diffstat: summary.diffstat(),
            changed_lines: summary.changed_line_count(),
            files,
            merged_in,
        });
    }
    Ok(deltas)
}

/// What a review diffs from.
struct ReviewBase {
    /// The baseline, with every line of history the turn merged in folded in.
    tree: String,
    /// The merged-in commits whose changes `tree` already holds.
    merged_in: Vec<String>,
}

/// The tree a review compares the capture with, so the diff is the turn's own
/// work and not what it merged in.
///
/// The turn's line is HEAD's first-parent line since the commit the baseline
/// was taken at, which is how Git names a branch's own history. Each merge on
/// that line brought in its other parents' changes; those are folded into the
/// baseline with a three-way merge whose ancestor is the merge base of that
/// merge's first parent and the merged parent. The ancestor is the merge's own
/// and not the baseline commit: upstream may already hold a commit the turn
/// pushed earlier, and measuring from the baseline would fold that commit in as
/// upstream work and hide it. A merged change that conflicts with reviewed
/// content leaves conflict markers in the base, so the diff shows both sides and
/// the turn's resolution. What remains is the turn's own commits, its conflict
/// resolutions, and its uncommitted edits.
///
/// Commits that join the first-parent line by fast-forward or by a rebase onto
/// another line are still reviewed as the turn's: the commit graph cannot tell
/// them from commits the turn pushed, and reviewing too much is the safe way
/// to be wrong.
fn review_base(
    git: &dyn GitCommandRunner,
    repository: &Path,
    baseline: &str,
    head: Option<&str>,
) -> Result<ReviewBase> {
    let unchanged = || ReviewBase {
        tree: baseline.to_owned(),
        merged_in: Vec::new(),
    };
    let Some(head) = head else {
        return Ok(unchanged());
    };
    let point = read_review_point(git, repository, baseline)?;
    let since = match point.head {
        Some(since) if object_exists(git, repository, &format!("{since}^{{commit}}")) => since,
        // The commit outlives a rewrite in the reflog for at least 30 days, so
        // this takes deliberate history deletion. Failing instead would keep
        // the baseline from ever advancing.
        Some(since) => {
            tracing::warn!(
                repository = %repository.display(),
                commit = %since,
                "the review baseline's commit is gone; the review includes any history the turn merged in"
            );
            return Ok(unchanged());
        }
        // A bare tree from an earlier release, or an unborn branch.
        None => return Ok(unchanged()),
    };
    if since == head {
        return Ok(unchanged());
    }
    let merges = first_parent_merges(git, repository, head, &since)?;
    if merges.is_empty() {
        return Ok(unchanged());
    }
    let mut tree = point.tree;
    let mut merged_in = Vec::new();
    for merge in merges {
        for parent in merge.merged {
            let ancestor = merge_base(git, repository, &merge.first_parent, &parent)?;
            tree = merge_trees(git, repository, ancestor.as_deref(), &tree, &parent)?;
            merged_in.push(parent);
        }
    }
    Ok(ReviewBase { tree, merged_in })
}

/// A merge commit on the turn's line: the parent it was made on, and the
/// parents it merged in.
struct FirstParentMerge {
    first_parent: String,
    merged: Vec<String>,
}

/// The merges on HEAD's first-parent line that `since` does not contain,
/// oldest first.
fn first_parent_merges(
    git: &dyn GitCommandRunner,
    repository: &Path,
    head: &str,
    since: &str,
) -> Result<Vec<FirstParentMerge>> {
    let exclude = format!("^{since}");
    let output = run(
        git,
        repository,
        &[
            "rev-list",
            "--first-parent",
            "--merges",
            "--parents",
            "--reverse",
            head,
            &exclude,
        ],
    )?;
    anyhow::ensure!(
        output.status == 0,
        "listing the merges since {since} failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut commits = line.split_whitespace().skip(1).map(str::to_owned);
            Some(FirstParentMerge {
                first_parent: commits.next()?,
                merged: commits.collect(),
            })
        })
        .collect())
}

/// The best common ancestor of two commits, or `None` for unrelated histories.
fn merge_base(
    git: &dyn GitCommandRunner,
    repository: &Path,
    left: &str,
    right: &str,
) -> Result<Option<String>> {
    let output = run(git, repository, &["merge-base", left, right])?;
    match output.status {
        0 => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )),
        1 => Ok(None),
        status => anyhow::bail!(
            "finding the merge base of {left} and {right} failed with status {status}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// The commit HEAD names, or `None` on an unborn branch.
fn resolve_head(git: &dyn GitCommandRunner, repository: &Path) -> Result<Option<String>> {
    let output = run(
        git,
        repository,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
    )?;
    match output.status {
        0 => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )),
        1 => Ok(None),
        status => anyhow::bail!(
            "resolving HEAD of {} failed with status {status}: {}",
            repository.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// Runs one Git command in `repository` with no input.
fn run(git: &dyn GitCommandRunner, repository: &Path, arguments: &[&str]) -> Result<GitOutput> {
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
        let tree = capture_paths(git, root, head.as_deref(), &state.dirty_tracked)
            .with_context(|| format!("capture the startup baseline of {}", root.display()))?;
        let point = record_review_point(git, root, &tree, head.as_deref(), session)
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

fn object_exists(git: &dyn GitCommandRunner, repository: &Path, object: &str) -> bool {
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
    /// merges brought in. One merged line builds on a commit the turn made
    /// earlier, which must stay in the review; the other conflicts with
    /// content an earlier review already covered. The shared file is over
    /// 64KB, so its conflicting versions cross pipe buffers through Git.
    // Hard-won: #1285: a merge made during the turn put the merged-in commits in the turn's review.
    #[test]
    fn a_merge_during_the_turn_reviews_only_the_turns_work() {
        let temp = repository();
        let root = temp.path();
        let shared = root.join("shared.txt");
        let edit = |edits: &[(usize, &str)]| {
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
        let commit = |message: &str| {
            git(root, &["add", "-A"]);
            git(root, &["commit", "-qm", message]);
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

        edit(&[]);
        commit("shared");
        git(root, &["branch", "old-upstream"]);
        edit(&[(30, "line 30 reviewed")]);
        commit("reviewed before the session");
        let untracked_at_start = initialize_review_baselines(&SystemGit, &roots, SESSION).unwrap();

        edit(&[(10, "line 10 turn")]);
        commit("turn, later pushed");
        git(root, &["checkout", "-qb", "upstream"]);
        std::fs::write(root.join("upstream.rs"), "fn upstream() {}\n").unwrap();
        edit(&[(20, "line 20 upstream"), (6000, "line 6000 upstream")]);
        commit("upstream, built on the turn's commit");
        git(root, &["checkout", "-q", "old-upstream"]);
        edit(&[(30, "line 30 upstream"), (7000, "line 7000 old upstream")]);
        commit("upstream from before the session");
        git(root, &["checkout", "-q", "main"]);
        edit(&[(20, "line 20 turn")]);
        commit("turn");
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
