//! What a turn changed: capturing it in the worker, and describing it to the
//! reviewing agents.
//!
//! The review target is a pair of Git tree ids per repository -- the baseline
//! recorded when the last review completed, and a capture taken the moment the
//! turn finished -- plus the unified diff between them. Tree ids are content
//! ids, so a baseline stays valid across a daemon restart, a harness swap, and
//! a cross-harness resume.

use std::collections::BTreeMap;
use std::path::PathBuf;

use mj_core::relay::RepoDelta;

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

/// Every changed file with its added and removed line counts, one section per
/// repository headed by that repository's totals. Git counts the whole change,
/// so the table lists every file even where the patch a prompt shows was cut
/// short.
#[must_use]
pub fn changed_files_table(deltas: &[RepoDelta]) -> String {
    let sections = deltas
        .iter()
        .filter(|delta| !delta.patch.trim().is_empty())
        .map(|delta| {
            let mut lines = vec![format!(
                "Repository: {} -- {}",
                delta.root.display(),
                delta.diffstat
            )];
            if delta.files.is_empty() {
                lines.push("  (this worker did not report per-file counts)".to_string());
            }
            for file in &delta.files {
                let counts = if file.binary {
                    format!("{:>15}", "binary")
                } else {
                    format!(
                        "{:>7} {:>7}",
                        format!("+{}", file.insertions),
                        format!("-{}", file.deletions)
                    )
                };
                let path = match &file.old_path {
                    Some(old_path) => format!("{old_path} -> {}", file.path),
                    None => file.path.clone(),
                };
                lines.push(format!("  {counts}  {path}"));
            }
            lines.join("\n")
        })
        .collect::<Vec<_>>();
    if sections.is_empty() {
        "No files changed.".to_string()
    } else {
        sections.join("\n\n")
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
            files: Vec::new(),
        }
    }

    #[test]
    fn golden_review_change_packet() {
        use std::fmt::Write as _;

        let mut output = String::new();
        let changed = vec![delta("/w/app", PATCH), delta("/w/lib", "")];
        writeln!(
            output,
            "=== one changed repository (review change packet) ==="
        )
        .unwrap();
        writeln!(output, "diffstat: {}", combined_diffstat(&changed)).unwrap();
        writeln!(output, "changed lines: {}", changed_line_count(&changed)).unwrap();
        writeln!(output, "workspace diff:\n{}", workspace_diff(&changed)).unwrap();

        let mut app = delta("/w/app", PATCH);
        app.files = vec![
            mj_core::relay::FileLineChange {
                path: "src/lib.rs".into(),
                insertions: 12,
                deletions: 3,
                ..Default::default()
            },
            mj_core::relay::FileLineChange {
                path: "src/new.rs".into(),
                old_path: Some("src/old.rs".into()),
                insertions: 1,
                deletions: 1,
                ..Default::default()
            },
            mj_core::relay::FileLineChange {
                path: "logo.png".into(),
                binary: true,
                ..Default::default()
            },
        ];
        let file_deltas = vec![app, delta("/w/lib", PATCH), delta("/w/quiet", "")];
        writeln!(
            output,
            "\n=== changed files by repository (review change packet) ==="
        )
        .unwrap();
        output.push_str(&changed_files_table(&file_deltas));
        output.push('\n');

        let empty = [delta("/w/quiet", "")];
        writeln!(output, "\n=== no changes (review change packet) ===").unwrap();
        writeln!(output, "diffstat: {}", combined_diffstat(&empty)).unwrap();
        writeln!(output, "changed lines: {}", changed_line_count(&empty)).unwrap();
        let empty_workspace_diff = workspace_diff(&empty);
        writeln!(
            output,
            "workspace diff: {}",
            if empty_workspace_diff.is_empty() {
                "<empty>"
            } else {
                &empty_workspace_diff
            }
        )
        .unwrap();
        writeln!(output, "changed files: {}", changed_files_table(&empty)).unwrap();

        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "review-change-packet", &output);
    }
}
