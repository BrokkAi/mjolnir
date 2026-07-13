//! Dirty-worktree-aware Git tree snapshots used to attribute changes to one
//! outer user turn or one Eitri invocation without touching the real index.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::process::Command;
use tokio::sync::Mutex;

const RECEIPT_LIMIT: usize = 64 * 1024;
pub const REVIEW_PATCH_LIMIT: usize = 128 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceSnapshot {
    inner: Arc<WorkspaceSnapshotInner>,
}

#[derive(Debug)]
struct WorkspaceSnapshotInner {
    roots: Vec<Mutex<GitTreeSnapshot>>,
    unavailable: Vec<SnapshotNotice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotNotice {
    root: PathBuf,
    message: String,
}

#[derive(Debug)]
struct GitTreeSnapshot {
    repo_root: PathBuf,
    pathspecs: Vec<PathBuf>,
    index_path: PathBuf,
    object_dir: PathBuf,
    alternate_object_dir: PathBuf,
    baseline_tree: String,
    _scratch: tempfile::TempDir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceDelta {
    changed: bool,
    receipt: String,
    review_patch: Option<String>,
}

impl WorkspaceDelta {
    pub(crate) fn changed(&self) -> bool {
        self.changed
    }

    pub(crate) fn receipt(&self) -> &str {
        &self.receipt
    }

    pub(crate) fn review_patch(&self) -> Option<&str> {
        self.review_patch.as_deref()
    }
}

struct RootDelta {
    receipt: String,
    patch: String,
}

impl WorkspaceSnapshot {
    pub(crate) async fn capture(workspace_roots: &[PathBuf]) -> Self {
        let mut repositories: BTreeMap<PathBuf, (PathBuf, BTreeSet<PathBuf>)> = BTreeMap::new();
        let mut unavailable = Vec::new();

        for requested_root in workspace_roots {
            let root = match tokio::fs::canonicalize(requested_root).await {
                Ok(root) => root,
                Err(_) => {
                    unavailable.push(SnapshotNotice {
                        root: requested_root.clone(),
                        message: "workspace root is unavailable".to_string(),
                    });
                    continue;
                }
            };
            let Some((repo_root, common_dir)) = discover_repository(&root).await else {
                unavailable.push(SnapshotNotice {
                    root,
                    message: "not a Git worktree".to_string(),
                });
                continue;
            };
            let pathspec = match root.strip_prefix(&repo_root) {
                Ok(path) if path.as_os_str().is_empty() => PathBuf::from("."),
                Ok(path) => path.to_path_buf(),
                Err(_) => {
                    unavailable.push(SnapshotNotice {
                        root,
                        message: "workspace root is outside its Git worktree".to_string(),
                    });
                    continue;
                }
            };
            repositories
                .entry(repo_root)
                .or_insert_with(|| (common_dir, BTreeSet::new()))
                .1
                .insert(pathspec);
        }

        let mut roots = Vec::new();
        for (repo_root, (common_dir, mut pathspecs)) in repositories {
            if pathspecs.contains(Path::new(".")) {
                pathspecs.clear();
                pathspecs.insert(PathBuf::from("."));
            }
            match GitTreeSnapshot::capture(repo_root.clone(), common_dir, pathspecs).await {
                Ok(snapshot) => roots.push(Mutex::new(snapshot)),
                Err(message) => unavailable.push(SnapshotNotice {
                    root: repo_root,
                    message,
                }),
            }
        }

        if workspace_roots.is_empty() {
            unavailable.push(SnapshotNotice {
                root: PathBuf::from("."),
                message: "no workspace roots were supplied".to_string(),
            });
        }

        Self {
            inner: Arc::new(WorkspaceSnapshotInner { roots, unavailable }),
        }
    }

    pub(crate) async fn delta(&self) -> WorkspaceDelta {
        let mut receipt_sections = Vec::new();
        let mut patch_sections = Vec::new();
        let mut review_notices = Vec::new();

        for root in &self.inner.roots {
            let mut root = root.lock().await;
            match root.delta().await {
                Ok(Some(delta)) => {
                    receipt_sections.push(format!(
                        "Repository: {}\n{}",
                        root.repo_root.display(),
                        delta.receipt.trim_end()
                    ));
                    patch_sections.push(format!(
                        "Repository: {}\n{}",
                        root.repo_root.display(),
                        delta.patch.trim_end()
                    ));
                }
                Ok(None) => {}
                Err(message) => {
                    let notice = format!(
                        "Repository: {}\n  delta unavailable: {message}",
                        root.repo_root.display()
                    );
                    receipt_sections.push(notice.clone());
                    review_notices.push(notice);
                }
            }
        }

        if !self.inner.unavailable.is_empty() {
            let mut section = String::from("Unavailable workspace roots:");
            for notice in &self.inner.unavailable {
                section.push_str(&format!(
                    "\n  - {}: {}",
                    notice.root.display(),
                    notice.message
                ));
            }
            receipt_sections.push(section.clone());
            review_notices.push(section);
        }

        let changed = !patch_sections.is_empty();
        let receipt = if receipt_sections.is_empty() {
            "No workspace changes.".to_string()
        } else {
            bound_text(receipt_sections.join("\n\n"), RECEIPT_LIMIT)
        };
        if changed && !review_notices.is_empty() {
            patch_sections.push(review_notices.join("\n\n"));
        }
        let review_patch =
            changed.then(|| bound_text(patch_sections.join("\n\n"), REVIEW_PATCH_LIMIT));
        WorkspaceDelta {
            changed,
            receipt,
            review_patch,
        }
    }
}

impl GitTreeSnapshot {
    async fn capture(
        repo_root: PathBuf,
        common_dir: PathBuf,
        pathspecs: BTreeSet<PathBuf>,
    ) -> Result<Self, String> {
        let scratch = tempfile::Builder::new()
            .prefix("mj-workspace-snapshot-")
            .tempdir()
            .map_err(|_| "could not create temporary snapshot storage".to_string())?;
        let index_path = scratch.path().join("index");
        let object_dir = scratch.path().join("objects");
        std::fs::create_dir_all(object_dir.join("info"))
            .and_then(|_| std::fs::create_dir_all(object_dir.join("pack")))
            .map_err(|_| "could not initialize temporary Git object storage".to_string())?;
        let alternate_object_dir = common_dir.join("objects");
        let pathspecs = pathspecs.into_iter().collect::<Vec<_>>();

        let mut snapshot = Self {
            repo_root,
            pathspecs,
            index_path,
            object_dir,
            alternate_object_dir,
            baseline_tree: String::new(),
            _scratch: scratch,
        };

        let head_tree = run_plain_git(
            &snapshot.repo_root,
            &["rev-parse", "--verify", "HEAD^{tree}"],
        )
        .await
        .ok()
        .and_then(|output| output.lines().next().map(str::trim).map(str::to_string))
        .filter(|tree| !tree.is_empty());
        match head_tree {
            Some(tree) => {
                snapshot
                    .run_scratch_git(["read-tree"], [tree.as_str()])
                    .await?
            }
            None => {
                snapshot
                    .run_scratch_git(["read-tree", "--empty"], std::iter::empty::<&str>())
                    .await?
            }
        }
        snapshot.refresh_index().await?;
        snapshot.baseline_tree = snapshot.write_tree().await?;
        Ok(snapshot)
    }

    async fn delta(&mut self) -> Result<Option<RootDelta>, String> {
        self.refresh_index().await?;
        let after_tree = self.write_tree().await?;
        if after_tree == self.baseline_tree {
            return Ok(None);
        }
        let receipt = self.diff(&after_tree, &["--stat", "--summary"]).await?;
        let patch = self.diff(&after_tree, &[]).await?;
        Ok(Some(RootDelta { receipt, patch }))
    }

    async fn refresh_index(&self) -> Result<(), String> {
        let pathspecs = self
            .pathspecs
            .iter()
            .map(|path| path.as_os_str())
            .collect::<Vec<_>>();
        self.run_scratch_git(["add", "-A", "--"], pathspecs).await
    }

    async fn write_tree(&self) -> Result<String, String> {
        let output = self
            .run_scratch_git_output(["write-tree"], std::iter::empty::<&str>())
            .await?;
        let tree = output.trim();
        if tree.is_empty() {
            Err("Git returned an empty tree identifier".to_string())
        } else {
            Ok(tree.to_string())
        }
    }

    async fn diff(&self, after_tree: &str, display_args: &[&str]) -> Result<String, String> {
        let mut args = vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
        ];
        args.extend_from_slice(display_args);
        args.push(&self.baseline_tree);
        args.push(after_tree);
        args.push("--");
        let pathspecs = self
            .pathspecs
            .iter()
            .map(|path| path.as_os_str())
            .collect::<Vec<_>>();
        self.run_scratch_git_output(args, pathspecs).await
    }

    async fn run_scratch_git<I, S, J, T>(&self, args: I, trailing: J) -> Result<(), String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
        J: IntoIterator<Item = T>,
        T: AsRef<std::ffi::OsStr>,
    {
        self.run_scratch_git_output(args, trailing)
            .await
            .map(|_| ())
    }

    async fn run_scratch_git_output<I, S, J, T>(
        &self,
        args: I,
        trailing: J,
    ) -> Result<String, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
        J: IntoIterator<Item = T>,
        T: AsRef<std::ffi::OsStr>,
    {
        let output = Command::new("git")
            .current_dir(&self.repo_root)
            .env("GIT_INDEX_FILE", &self.index_path)
            .env("GIT_OBJECT_DIRECTORY", &self.object_dir)
            .env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                &self.alternate_object_dir,
            )
            .args(args)
            .args(trailing)
            .output()
            .await
            .map_err(|_| "could not launch Git snapshot command".to_string())?;
        if !output.status.success() {
            return Err(git_failure(&output));
        }
        String::from_utf8(output.stdout)
            .map_err(|_| "Git snapshot output was not UTF-8".to_string())
    }
}

async fn discover_repository(workspace_root: &Path) -> Option<(PathBuf, PathBuf)> {
    let repo_root = run_plain_git(workspace_root, &["rev-parse", "--show-toplevel"])
        .await
        .ok()?;
    let common_dir = run_plain_git(
        workspace_root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await
    .ok()?;
    let repo_root = tokio::fs::canonicalize(repo_root.trim()).await.ok()?;
    let common_dir = tokio::fs::canonicalize(common_dir.trim()).await.ok()?;
    Some((repo_root, common_dir))
}

async fn run_plain_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .args(args)
        .output()
        .await
        .map_err(|_| "could not launch Git".to_string())?;
    if !output.status.success() {
        return Err(git_failure(&output));
    }
    String::from_utf8(output.stdout).map_err(|_| "Git output was not UTF-8".to_string())
}

fn git_failure(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("Git command failed");
    let detail = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "Git snapshot command failed: {}",
        truncate_chars(&detail, 240)
    )
}

fn bound_text(text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    const MARKER: &str = "\n…[workspace delta truncated]…\n";
    let available = limit.saturating_sub(MARKER.len());
    let head_len = available.saturating_mul(3) / 4;
    let tail_len = available.saturating_sub(head_len);
    let head_end = text.floor_char_boundary(head_len);
    let tail_start = text.ceil_char_boundary(text.len().saturating_sub(tail_len));
    format!("{}{}{}", &text[..head_end], MARKER, &text[tail_start..])
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        text.chars().take(limit).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf8 git output")
    }

    fn init_repo(root: &Path) {
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "mjolnir@example.test"]);
        git(root, &["config", "user.name", "Mjolnir Tests"]);
    }

    fn commit_all(root: &Path) {
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "baseline"]);
    }

    fn object_files(root: &Path) -> BTreeSet<PathBuf> {
        fn visit(root: &Path, dir: &Path, output: &mut BTreeSet<PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read object directory") {
                let entry = entry.expect("object entry");
                let path = entry.path();
                if path.is_dir() {
                    visit(root, &path, output);
                } else {
                    output.insert(
                        path.strip_prefix(root)
                            .expect("relative object")
                            .to_path_buf(),
                    );
                }
            }
        }

        let objects = root.join(".git").join("objects");
        let mut output = BTreeSet::new();
        visit(&objects, &objects, &mut output);
        output
    }

    #[tokio::test]
    async fn snapshot_attributes_only_interval_changes_without_touching_git_metadata() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        init_repo(root);
        std::fs::write(root.join("dirty.txt"), "committed dirty\n").expect("dirty seed");
        std::fs::write(root.join("staged.txt"), "committed staged\n").expect("staged seed");
        std::fs::write(root.join("delete.txt"), "delete me\n").expect("delete seed");
        std::fs::write(root.join("rename-old.txt"), "rename me\n").expect("rename seed");
        std::fs::write(root.join("mode.sh"), "#!/bin/sh\nexit 0\n").expect("mode seed");
        std::fs::write(root.join("baseline-dirty-only.txt"), "committed\n")
            .expect("baseline dirty seed");
        std::fs::write(root.join("baseline-staged-only.txt"), "committed\n")
            .expect("baseline staged seed");
        std::fs::write(root.join("baseline-deleted-only.txt"), "committed\n")
            .expect("baseline deleted seed");
        commit_all(root);

        std::fs::write(root.join("dirty.txt"), "dirty before Eitri\n").expect("predirty");
        std::fs::write(root.join("staged.txt"), "staged before Eitri\n").expect("prestage");
        git(root, &["add", "staged.txt"]);
        std::fs::write(root.join("untracked.txt"), "untracked before Eitri\n")
            .expect("pre-untracked");
        std::fs::write(
            root.join("baseline-dirty-only.txt"),
            "dirty before capture\n",
        )
        .expect("baseline dirty");
        std::fs::write(
            root.join("baseline-staged-only.txt"),
            "staged before capture\n",
        )
        .expect("baseline staged");
        git(root, &["add", "baseline-staged-only.txt"]);
        std::fs::remove_file(root.join("baseline-deleted-only.txt")).expect("baseline deletion");
        std::fs::write(
            root.join("baseline-untracked-only.txt"),
            "untracked before capture\n",
        )
        .expect("baseline untracked");

        let git_dir = root.join(".git");
        let index_before = std::fs::read(git_dir.join("index")).expect("read real index");
        let refs_before = git(root, &["show-ref"]);
        let objects_before = object_files(root);
        let branch_before = git(root, &["symbolic-ref", "HEAD"]);
        let status_before = git(root, &["status", "--porcelain=v1", "--untracked-files=all"]);
        let snapshot = WorkspaceSnapshot::capture(&[root.to_path_buf()]).await;
        assert_eq!(
            git(root, &["status", "--porcelain=v1", "--untracked-files=all"]),
            status_before
        );
        assert_eq!(git(root, &["symbolic-ref", "HEAD"]), branch_before);

        std::fs::write(
            root.join("dirty.txt"),
            "dirty before Eitri\nchanged during invocation\n",
        )
        .expect("change dirty");
        std::fs::write(
            root.join("staged.txt"),
            "staged before Eitri\nchanged during invocation\n",
        )
        .expect("change staged");
        std::fs::write(
            root.join("untracked.txt"),
            "untracked before Eitri\nchanged during invocation\n",
        )
        .expect("change untracked");
        std::fs::write(root.join("created.txt"), "created during invocation\n").expect("create");
        std::fs::remove_file(root.join("delete.txt")).expect("delete");
        std::fs::rename(root.join("rename-old.txt"), root.join("rename-new.txt")).expect("rename");
        std::fs::write(root.join("binary.bin"), [0_u8, 1, 2, 0]).expect("binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(root.join("mode.sh"))
                .expect("mode metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(root.join("mode.sh"), permissions).expect("chmod");
        }

        let delta = snapshot.delta().await;
        assert!(delta.changed());
        let receipt = delta.receipt();
        for path in [
            "dirty.txt",
            "staged.txt",
            "untracked.txt",
            "created.txt",
            "delete.txt",
            "binary.bin",
        ] {
            assert!(receipt.contains(path), "receipt omitted {path}: {receipt}");
        }
        assert!(receipt.contains("rename-old.txt") || receipt.contains("rename-new.txt"));
        #[cfg(unix)]
        assert!(receipt.contains("mode change 100644 => 100755 mode.sh"));
        for path in [
            "baseline-dirty-only.txt",
            "baseline-staged-only.txt",
            "baseline-deleted-only.txt",
            "baseline-untracked-only.txt",
        ] {
            assert!(
                !receipt.contains(path),
                "pre-capture-only change leaked into receipt: {path}: {receipt}"
            );
        }

        let patch = delta.review_patch().expect("review patch");
        assert!(patch.contains("+changed during invocation"));
        assert!(!patch.contains("-committed dirty"));
        assert!(!patch.contains("-committed staged"));

        assert_eq!(
            std::fs::read(git_dir.join("index")).expect("read real index after"),
            index_before
        );
        assert_eq!(git(root, &["show-ref"]), refs_before);
        assert_eq!(git(root, &["symbolic-ref", "HEAD"]), branch_before);
        assert_eq!(object_files(root), objects_before);
    }

    #[tokio::test]
    async fn overlapping_outer_and_invocation_snapshots_have_independent_baselines() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        init_repo(root);
        git(root, &["commit", "--allow-empty", "-qm", "baseline"]);

        let outer = WorkspaceSnapshot::capture(&[root.to_path_buf()]).await;
        std::fs::write(root.join("thor.txt"), "changed by Thor\n").expect("Thor edit");
        let invocation = WorkspaceSnapshot::capture(&[root.to_path_buf()]).await;
        std::fs::write(root.join("eitri.txt"), "changed by Eitri\n").expect("Eitri edit");

        let invocation_delta = invocation.delta().await;
        assert!(invocation_delta.receipt().contains("eitri.txt"));
        assert!(!invocation_delta.receipt().contains("thor.txt"));

        let outer_delta = outer.delta().await;
        assert!(outer_delta.receipt().contains("thor.txt"));
        assert!(outer_delta.receipt().contains("eitri.txt"));

        let second_invocation = WorkspaceSnapshot::capture(&[root.to_path_buf()]).await;
        std::fs::write(root.join("followup.txt"), "second Eitri call\n").expect("followup");
        let second_delta = second_invocation.delta().await;
        assert!(second_delta.receipt().contains("followup.txt"));
        assert!(!second_delta.receipt().contains("thor.txt"));
        assert!(!second_delta.receipt().contains("eitri.txt"));
    }

    #[tokio::test]
    async fn unborn_repository_and_non_git_root_fail_open() {
        let unborn = tempfile::tempdir().expect("unborn");
        init_repo(unborn.path());
        let snapshot = WorkspaceSnapshot::capture(&[unborn.path().to_path_buf()]).await;
        std::fs::write(unborn.path().join("first.txt"), "first\n").expect("first file");
        let delta = snapshot.delta().await;
        assert!(delta.changed());
        assert!(delta.receipt().contains("first.txt"));

        let non_git = tempfile::tempdir().expect("non-git");
        let snapshot = WorkspaceSnapshot::capture(&[non_git.path().to_path_buf()]).await;
        let delta = snapshot.delta().await;
        assert!(!delta.changed());
        assert!(delta.receipt().contains("not a Git worktree"));
    }

    #[test]
    fn bounded_delta_preserves_head_and_tail_with_marker() {
        let text = format!("HEAD{}TAIL", "x".repeat(256));
        let bounded = bound_text(text, 80);
        assert!(bounded.starts_with("HEAD"));
        assert!(bounded.ends_with("TAIL"));
        assert!(bounded.contains("workspace delta truncated"));
        assert!(bounded.len() <= 80);
    }
}
