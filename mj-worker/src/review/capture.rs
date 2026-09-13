//! Worker-owned Git capture for turn review.

use anyhow::{Context, Result};
use mj_checkpoint::archive::{
    GitCommandRunner, REVIEW_BASELINE_REF, capture_worktree_tree, diff_between_trees,
    pin_review_tree,
};
use mj_core::relay::RepoDelta;
use mj_core::review::delta::RawDiffSummary;
#[cfg(test)]
use mj_core::review::delta::{captured_trees, has_changes};
use mj_core::review::{LANE_DIFF_LIMIT, bound_review_section};
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
) -> Result<Vec<RepoDelta>> {
    let mut deltas = Vec::new();
    for root in repositories {
        let current = capture_worktree_tree(git, root)
            .with_context(|| format!("capture the working tree of {}", root.display()))?;
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
) -> Result<()> {
    for root in repositories {
        if pinned_review_baseline(git, root).is_some() {
            continue;
        }
        let current = capture_worktree_tree(git, root)
            .with_context(|| format!("capture the startup worktree of {}", root.display()))?;
        pin_review_tree(git, root, REVIEW_BASELINE_REF, &current)
            .with_context(|| format!("pin the startup review baseline of {}", root.display()))?;
    }
    Ok(())
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
    use mj_checkpoint::archive::SystemGit;

    fn git(repository: &Path, arguments: &[&str]) {
        let output = std::process::Command::new("git")
            .args(arguments)
            .current_dir(repository)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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
        let first = capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new()).unwrap();
        assert!(
            !has_changes(&first),
            "a repository with no baseline starts coverage rather than reviewing its whole history"
        );
        let baselines = captured_trees(&first);

        std::fs::write(temp.path().join("tracked.rs"), "fn main() { retry(); }\n").unwrap();
        let second = capture_repository_deltas(&SystemGit, &roots, &baselines).unwrap();
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

        let deltas = capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new()).unwrap();
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

        let deltas = capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new()).unwrap();
        assert!(has_changes(&deltas));
        assert!(deltas[0].patch.contains("+fn main() { committed(); }"));
    }

    #[test]
    fn persisted_review_baseline_takes_precedence_over_the_startup_pin() {
        let temp = repository();
        let roots = vec![temp.path().to_path_buf()];
        initialize_review_baselines(&SystemGit, &roots).unwrap();

        std::fs::write(temp.path().join("tracked.rs"), "fn main() { first(); }\n").unwrap();
        let first = capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new()).unwrap();
        let persisted = captured_trees(&first);

        std::fs::write(temp.path().join("tracked.rs"), "fn main() { second(); }\n").unwrap();
        let second = capture_repository_deltas(&SystemGit, &roots, &persisted).unwrap();
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
        initialize_review_baselines(&SystemGit, &roots).unwrap();

        std::fs::write(temp.path().join("tracked.rs"), "base\nbefore\nafter\n").unwrap();
        std::fs::write(temp.path().join("agent.rs"), "new work\n").unwrap();

        let deltas = capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new()).unwrap();
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

        let deltas = capture_repository_deltas(&SystemGit, &roots, &BTreeMap::new()).unwrap();
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
        let deltas = capture_repository_deltas(&SystemGit, &roots, &stale).unwrap();
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
}
