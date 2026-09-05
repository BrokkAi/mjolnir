//! What a turn changed: capturing it in the worker, and describing it to the
//! reviewing agents.
//!
//! The review target is a pair of Git tree ids per repository -- the baseline
//! recorded when the last review completed, and a capture taken the moment the
//! turn finished -- plus the unified diff between them. Tree ids are content
//! ids, so a baseline stays valid across a daemon restart, a harness swap, and
//! a cross-harness resume.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::hel_archive::{
    GitCommandRunner, REVIEW_BASELINE_REF, capture_worktree_tree, diff_between_trees,
    pin_review_tree,
};
use crate::hel_worker::RepoDelta;

use super::{LANE_DIFF_LIMIT, bound_review_section};

/// Line and file totals parsed straight from a unified diff.
///
/// Ported from mjolnir's `RawDiffSummary` (`mj-agents/src/discrete_review.rs`),
/// where it summarized a patch when Bifrost analysis was disabled. Hel always
/// runs Bifrost, so this survives only as the worker's own diffstat: it is
/// computed from the untruncated patch, which keeps a bounded patch from making
/// a change look smaller than it is.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RawDiffSummary {
    pub files: usize,
    pub insertions: usize,
    pub deletions: usize,
}

impl RawDiffSummary {
    #[must_use]
    pub fn from_patch(patch: &str) -> Self {
        let mut summary = Self::default();
        let mut in_hunk = false;
        for line in patch.lines() {
            if line.starts_with("diff --git ") {
                summary.files = summary.files.saturating_add(1);
                in_hunk = false;
            } else if line.starts_with("@@") {
                in_hunk = true;
            } else if in_hunk && line.starts_with('+') {
                summary.insertions = summary.insertions.saturating_add(1);
            } else if in_hunk && line.starts_with('-') {
                summary.deletions = summary.deletions.saturating_add(1);
            }
        }
        if summary.files == 0 && summary.changed_line_count() > 0 {
            summary.files = 1;
        }
        summary
    }

    #[must_use]
    pub fn changed_line_count(&self) -> usize {
        self.insertions.saturating_add(self.deletions)
    }

    #[must_use]
    pub fn diffstat(&self) -> String {
        let mut summary = format!(
            "{} {} changed",
            self.files,
            if self.files == 1 { "file" } else { "files" }
        );
        if self.insertions > 0 {
            summary.push_str(&format!(
                ", {} {}(+)",
                self.insertions,
                if self.insertions == 1 {
                    "insertion"
                } else {
                    "insertions"
                }
            ));
        }
        if self.deletions > 0 {
            summary.push_str(&format!(
                ", {} {}(-)",
                self.deletions,
                if self.deletions == 1 {
                    "deletion"
                } else {
                    "deletions"
                }
            ));
        }
        summary
    }
}

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
            &crate::hel_archive::GitCommand {
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
        &crate::hel_archive::GitCommand {
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

/// Whether any repository has something to review.
#[must_use]
pub fn has_changes(deltas: &[RepoDelta]) -> bool {
    deltas.iter().any(|delta| !delta.patch.trim().is_empty())
}

/// The `<workspace_diff>` body every reviewing role sees: one section per
/// repository, each headed by its root so a lane can tell which Bifrost server
/// answers for a path.
#[must_use]
pub fn workspace_diff(deltas: &[RepoDelta]) -> String {
    deltas
        .iter()
        .filter(|delta| !delta.patch.trim().is_empty())
        .map(|delta| format!("Repository: {}\n{}", delta.root.display(), delta.patch))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Combined diffstat across repositories, for the prompts that show totals
/// rather than the patch itself.
#[must_use]
pub fn combined_diffstat(deltas: &[RepoDelta]) -> String {
    let lines = deltas
        .iter()
        .filter(|delta| !delta.patch.trim().is_empty())
        .map(|delta| format!("{}: {}", delta.root.display(), delta.diffstat))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        "No files changed.".to_string()
    } else {
        lines.join("\n")
    }
}

/// Total changed lines across every repository in the delta.
#[must_use]
pub fn changed_line_count(deltas: &[RepoDelta]) -> usize {
    deltas.iter().fold(0usize, |total, delta| {
        total.saturating_add(delta.changed_lines)
    })
}

/// The trees a completed review should record as its new baselines.
#[must_use]
pub fn captured_trees(deltas: &[RepoDelta]) -> BTreeMap<PathBuf, String> {
    deltas
        .iter()
        .map(|delta| (delta.root.clone(), delta.current_tree.clone()))
        .collect()
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
            &crate::hel_archive::GitCommand {
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
mod tests {
    use super::*;

    const PATCH: &str = "diff --git a/one.rs b/one.rs\n\
        --- a/one.rs\n\
        +++ b/one.rs\n\
        @@ -1,2 +1,3 @@\n\
        +added\n\
        +added again\n\
        -removed\n\
         context\n\
        diff --git a/two.rs b/two.rs\n\
        --- a/two.rs\n\
        +++ b/two.rs\n\
        @@ -1 +1 @@\n\
        +only\n";

    #[test]
    fn a_raw_diff_summary_counts_files_and_changed_lines() {
        let summary = RawDiffSummary::from_patch(PATCH);
        assert_eq!(summary.files, 2);
        assert_eq!(summary.insertions, 3);
        assert_eq!(summary.deletions, 1);
        assert_eq!(summary.changed_line_count(), 4);
        assert_eq!(
            summary.diffstat(),
            "2 files changed, 3 insertions(+), 1 deletion(-)"
        );
    }

    #[test]
    fn a_raw_diff_summary_of_nothing_reports_nothing() {
        let summary = RawDiffSummary::from_patch("");
        assert_eq!(summary, RawDiffSummary::default());
        assert_eq!(summary.diffstat(), "0 files changed");
    }

    #[test]
    fn headers_outside_a_hunk_are_not_counted_as_changed_lines() {
        let summary = RawDiffSummary::from_patch(
            "diff --git a/one.rs b/one.rs\n--- a/one.rs\n+++ b/one.rs\n",
        );
        assert_eq!(summary.changed_line_count(), 0);
        assert_eq!(summary.files, 1);
    }

    fn delta(root: &str, patch: &str) -> RepoDelta {
        let summary = RawDiffSummary::from_patch(patch);
        RepoDelta {
            root: PathBuf::from(root),
            baseline_tree: None,
            current_tree: "tree".into(),
            patch: patch.to_string(),
            diffstat: summary.diffstat(),
            changed_lines: summary.changed_line_count(),
        }
    }

    #[test]
    fn a_workspace_diff_names_the_repository_each_section_belongs_to() {
        let deltas = vec![delta("/w/app", PATCH), delta("/w/lib", "")];
        let rendered = workspace_diff(&deltas);
        assert!(rendered.starts_with("Repository: /w/app\n"));
        assert!(
            !rendered.contains("/w/lib"),
            "a repository with no changes contributes no section"
        );
        assert!(has_changes(&deltas));
        assert!(!has_changes(&[delta("/w/lib", "")]));
    }

    #[test]
    fn a_combined_diffstat_totals_only_the_repositories_that_changed() {
        let deltas = vec![delta("/w/app", PATCH), delta("/w/lib", "")];
        assert_eq!(
            combined_diffstat(&deltas),
            "/w/app: 2 files changed, 3 insertions(+), 1 deletion(-)"
        );
        assert_eq!(changed_line_count(&deltas), 4);
        assert_eq!(
            combined_diffstat(&[delta("/w/lib", "")]),
            "No files changed."
        );
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use crate::hel_archive::SystemGit;

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
