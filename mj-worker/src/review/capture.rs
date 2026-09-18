//! Worker-owned Git capture for turn review.

use anyhow::{Context, Result};
use mj_checkpoint::archive::{
    CaptureBase, GitCommandRunner, REVIEW_BASELINE_REF, capture_paths, diff_between_trees,
    pin_review_tree,
};
use mj_core::relay::RepoDelta;
use mj_review::delta::RawDiffSummary;
#[cfg(test)]
use mj_review::delta::{captured_trees, has_changes};
use mj_review::{LANE_DIFF_LIMIT, bound_review_section};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Captures every repository in `repositories` against `baselines`.
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
) -> Result<Vec<RepoDelta>> {
    let mut deltas = Vec::new();
    for root in repositories {
        // A controller baseline takes precedence: it is the point a completed
        // review recorded. The worker pin covers a new session before that
        // first review has a controller baseline, and also a fresh target
        // after a resume. Both are usable only while their tree object is
        // still in this repository.
        let baseline = baselines
            .get(root)
            .filter(|tree| tree_exists(git, root, tree))
            .cloned()
            .or_else(|| pinned_review_baseline(git, root));
        // Capture only what this turn could have changed. The tracked changes
        // come straight from `git status`; an untracked path counts when it is
        // new since the session started, or when its size or modification time
        // moved. Everything else is carried over from the baseline tree
        // without being read.
        let state = read_workspace_state(git, root)?;
        let empty = Vec::new();
        let start = untracked_at_start.get(root).unwrap_or(&empty);
        let mut changed = state.dirty_tracked.clone();
        changed.extend(changed_untracked(start, &state.untracked));
        // The capture starts from HEAD, not from the baseline tree, so a turn
        // that committed its work is still visible: HEAD has moved, and the
        // diff against the baseline shows the commit. Starting from the
        // baseline would have carried the old content forward for every path
        // that `git status` no longer calls dirty.
        let current = capture_paths(git, root, CaptureBase::Head, &changed)
            .with_context(|| format!("capture the changed paths of {}", root.display()))?;
        let patch = match &baseline {
            Some(baseline) => diff_between_trees(git, root, Some(baseline), &current)
                .with_context(|| format!("diff the captured trees of {}", root.display()))?,
            // Neither baseline is usable. Keep the coverage reset honest: an
            // empty-tree diff would report the restored or pre-existing
            // repository as work from this turn.
            None => String::new(),
        };
        let summary = RawDiffSummary::from_patch(&patch);
        deltas.push(RepoDelta {
            root: root.clone(),
            baseline_tree: baseline,
            current_tree: current,
            patch: bound_review_section(&patch, LANE_DIFF_LIMIT, "workspace diff"),
            diffstat: summary.diffstat(),
            changed_lines: summary.changed_line_count(),
        });
    }
    Ok(deltas)
}

/// Pins the current worktree as the baseline for a fresh worker workspace.
///
/// This is deliberately separate from [`capture_repository_deltas`]: startup
/// must establish the point before the primary harness can edit anything, but
/// a daemon restart must not replace a baseline that may have unreviewed work
/// after it. The caller decides whether this is a fresh startup; this function
/// is idempotent and preserves every valid existing pin.
pub fn initialize_review_baselines(
    git: &dyn GitCommandRunner,
    repositories: &[PathBuf],
) -> Result<BTreeMap<PathBuf, Vec<UntrackedEntry>>> {
    let mut untracked_at_start = BTreeMap::new();
    for root in repositories {
        // The untracked list is recorded even when a baseline tree is already
        // pinned: a restarted worker still has to know which untracked files
        // predate it, or it would report all of them as this turn's work.
        let state = read_workspace_state(git, root)?;
        untracked_at_start.insert(root.clone(), state.untracked);
        if pinned_review_baseline(git, root).is_some() {
            continue;
        }
        // The baseline is HEAD plus what the checkout was already dirty with.
        // Untracked files are deliberately not in it; see `changed_untracked`
        // for how a review still tells a new file from a pre-existing one.
        let current = capture_paths(git, root, CaptureBase::Head, &state.dirty_tracked)
            .with_context(|| format!("capture the startup baseline of {}", root.display()))?;
        pin_review_tree(git, root, REVIEW_BASELINE_REF, &current)
            .with_context(|| format!("pin the startup review baseline of {}", root.display()))?;
    }
    Ok(untracked_at_start)
}

/// Returns the tree held by the worker's durable baseline ref, if it still
/// resolves to a tree in this repository.
fn pinned_review_baseline(git: &dyn GitCommandRunner, repository: &Path) -> Option<String> {
    let output = git
        .run(
            repository,
            &mj_checkpoint::archive::GitCommand {
                arguments: vec![
                    "rev-parse".into(),
                    "--verify".into(),
                    format!("{REVIEW_BASELINE_REF}^{{tree}}").into(),
                ],
                stdin: Vec::new(),
                env: Vec::new(),
            },
        )
        .ok()?;
    if output.status != 0 {
        return None;
    }
    let tree = String::from_utf8(output.stdout).ok()?;
    let tree = tree.trim();
    (!tree.is_empty()).then(|| tree.to_owned())
}

/// Whether this repository still holds the tree a baseline names.
fn tree_exists(git: &dyn GitCommandRunner, repository: &Path, tree: &str) -> bool {
    git.run(
        repository,
        &mj_checkpoint::archive::GitCommand {
            arguments: vec![
                "cat-file".into(),
                "-e".into(),
                format!("{tree}^{{tree}}").into(),
            ],
            stdin: Vec::new(),
            env: Vec::new(),
        },
    )
    .is_ok_and(|output| output.status == 0)
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

/// Pins each named tree as that repository's review baseline.
pub fn advance_baselines(
    git: &dyn GitCommandRunner,
    trees: &BTreeMap<PathBuf, String>,
) -> Result<()> {
    for (root, tree) in trees {
        pin_review_tree(git, root, REVIEW_BASELINE_REF, tree)
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
    let output = git
        .run(
            directory,
            &mj_checkpoint::archive::GitCommand {
                arguments: vec!["rev-parse".into(), "--show-toplevel".into()],
                stdin: Vec::new(),
                env: Vec::new(),
            },
        )
        .ok()?;
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
    use mj_checkpoint::archive::SystemGit;

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
        let first =
            capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new(), &BTreeMap::new())
                .unwrap();
        assert!(
            !has_changes(&first),
            "a repository with no baseline starts coverage rather than reviewing its whole history"
        );
        let baselines = captured_trees(&first);

        std::fs::write(temp.path().join("tracked.rs"), "fn main() { retry(); }\n").unwrap();
        let second =
            capture_repository_deltas(&SystemGit, &roots, &baselines, &BTreeMap::new()).unwrap();
        assert!(has_changes(&second));
        assert!(second[0].patch.contains("+fn main() { retry(); }"));
        assert_eq!(
            second[0].baseline_tree.as_deref(),
            baselines.values().next().map(String::as_str)
        );
        assert_eq!(second[0].changed_lines, 2);
    }

    #[test]
    fn startup_baseline_makes_the_first_edits_reviewable() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        initialize_review_baselines(&SystemGit, &roots).unwrap();
        let startup = pinned_review_baseline(&SystemGit, temp.path()).unwrap();

        std::fs::write(temp.path().join("tracked.rs"), "fn main() { first(); }\n").unwrap();
        std::fs::write(temp.path().join("new.rs"), "fn new() {}\n").unwrap();

        let deltas =
            capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new(), &BTreeMap::new())
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
        initialize_review_baselines(&SystemGit, &roots).unwrap();

        std::fs::write(
            temp.path().join("tracked.rs"),
            "fn main() { committed(); }\n",
        )
        .unwrap();
        git(temp.path(), &["add", "tracked.rs"]);
        git(temp.path(), &["commit", "-qm", "turn change"]);

        let deltas =
            capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new(), &BTreeMap::new())
                .unwrap();
        assert!(has_changes(&deltas));
        assert!(deltas[0].patch.contains("+fn main() { committed(); }"));
    }

    #[test]
    fn persisted_review_baseline_takes_precedence_over_the_startup_pin() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        initialize_review_baselines(&SystemGit, &roots).unwrap();

        std::fs::write(temp.path().join("tracked.rs"), "fn main() { first(); }\n").unwrap();
        let first =
            capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new(), &BTreeMap::new())
                .unwrap();
        let persisted = captured_trees(&first);

        std::fs::write(temp.path().join("tracked.rs"), "fn main() { second(); }\n").unwrap();
        let second =
            capture_repository_deltas(&SystemGit, &roots, &persisted, &BTreeMap::new()).unwrap();
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
        let untracked_at_start = initialize_review_baselines(&SystemGit, &roots).unwrap();

        std::fs::write(temp.path().join("tracked.rs"), "base\nbefore\nafter\n").unwrap();
        std::fs::write(temp.path().join("agent.rs"), "new work\n").unwrap();

        let deltas =
            capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new(), &untracked_at_start)
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
        initialize_review_baselines(&SystemGit, &roots).unwrap();
        let startup = pinned_review_baseline(&SystemGit, temp.path()).unwrap();

        std::fs::write(temp.path().join("tracked.rs"), "fn main() { pending(); }\n").unwrap();
        initialize_review_baselines(&SystemGit, &roots).unwrap();
        assert_eq!(
            pinned_review_baseline(&SystemGit, temp.path()).as_deref(),
            Some(startup.as_str())
        );

        let deltas =
            capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new(), &BTreeMap::new())
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
            capture_repository_deltas(&SystemGit, &roots, &stale, &BTreeMap::new()).unwrap();
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

    /// The cost of a capture must be the session's own changes, not the size
    /// of the working tree. This is #1065: a workspace with hundreds of
    /// thousands of untracked files made every session start read, hash and
    /// store every one of them before the worker could be reached.
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

        initialize_review_baselines(&SystemGit, &roots).unwrap();

        // One tree object for the baseline. Two hundred untracked files cost
        // nothing, because nothing opened them.
        let written = loose_objects(temp.path()) - before;
        assert!(
            written <= 2,
            "a clean checkout's baseline wrote {written} objects; it must not stage untracked files"
        );
    }

    /// A review captures what the turn changed and nothing else, so the same
    /// untouched untracked files stay unread at review time too.
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
        let untracked_at_start = initialize_review_baselines(&SystemGit, &roots).unwrap();
        let before = loose_objects(temp.path());

        std::fs::write(temp.path().join("tracked.rs"), "fn main() { changed(); }\n").unwrap();
        let deltas =
            capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new(), &untracked_at_start)
                .unwrap();

        assert!(has_changes(&deltas));
        assert!(deltas[0].patch.contains("+fn main() { changed(); }"));
        assert!(
            !deltas[0].patch.contains("untracked-7.bin"),
            "an untouched untracked file is not this turn's work: {}",
            deltas[0].patch
        );
        // One blob for the changed file plus one tree; the 200 untracked files
        // are neither read nor stored.
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
        let untracked_at_start = initialize_review_baselines(&SystemGit, &roots).unwrap();

        std::fs::write(temp.path().join("made.txt"), "the agent made this\n").unwrap();
        let deltas =
            capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new(), &untracked_at_start)
                .unwrap();

        assert!(deltas[0].patch.contains("made.txt"), "{}", deltas[0].patch);
        assert!(
            !deltas[0].patch.contains("already.txt"),
            "{}",
            deltas[0].patch
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
    let output = git
        .run(
            repository,
            &mj_checkpoint::archive::GitCommand {
                arguments: vec![
                    "status".into(),
                    "--porcelain=v1".into(),
                    "--untracked-files=all".into(),
                    "--no-renames".into(),
                    "-z".into(),
                ],
                stdin: Vec::new(),
                env: Vec::new(),
            },
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
