//! Worker-owned review snapshots: a detached worktree at the reviewed commit,
//! so reviewers read files that match the diff while the requester keeps
//! working in its own checkout.
//!
//! The checkout lives at `<git-common-dir>/mj-reviews/<id>`. The git directory
//! is never part of a working tree, so the checkout cannot show up in
//! `git status` or be committed, and it is shared by every worktree of the
//! repository. The review host owns which ids should exist; this module only
//! creates, removes and lists them.

use anyhow::{Context, Result, bail, ensure};
use mj_checkpoint::archive::{GitCommand, GitCommandRunner, GitOutput};
use std::path::{Path, PathBuf};

/// The directory inside the git directory that holds review snapshots.
const SNAPSHOT_DIRECTORY: &str = "mj-reviews";

#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub path: PathBuf,
    /// Every snapshot id the repository holds once the operation finished.
    pub present: Vec<String>,
}

fn git(runner: &dyn GitCommandRunner, directory: &Path, args: &[&str]) -> Result<GitOutput> {
    runner.run(
        directory,
        &GitCommand {
            arguments: args.iter().map(Into::into).collect(),
            ..GitCommand::default()
        },
    )
}

fn checked(runner: &dyn GitCommandRunner, directory: &Path, args: &[&str]) -> Result<String> {
    let output = git(runner, directory, args)?;
    ensure!(
        output.status == 0,
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// An id becomes a directory name, so it may not name anything else.
fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "invalid review snapshot id {id:?}"
    );
    Ok(())
}

fn snapshot_directory(runner: &dyn GitCommandRunner, repository: &Path) -> Result<PathBuf> {
    let common = checked(
        runner,
        repository,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok(PathBuf::from(common).join(SNAPSHOT_DIRECTORY))
}

fn present(directory: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("list {}", directory.display()));
        }
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("list {}", directory.display()))?;
        if let Ok(name) = entry.file_name().into_string() {
            ids.push(name);
        }
    }
    ids.sort();
    Ok(ids)
}

/// Creates the snapshot of `commit`, or keeps the one already there when it
/// is at that commit. A restored review asks again, so this is idempotent.
pub fn create(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    id: &str,
    commit: &str,
) -> Result<Snapshot> {
    validate_id(id)?;
    ensure!(
        !commit.is_empty() && commit.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "review snapshot commit {commit:?} is not a commit id"
    );
    let directory = snapshot_directory(runner, repository)?;
    let path = directory.join(id);
    let commit = checked(
        runner,
        repository,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{commit}^{{commit}}"),
        ],
    )?;
    // Drop records of snapshots whose directories are gone, or `worktree add`
    // refuses the path as a missing but registered worktree.
    checked(runner, repository, &["worktree", "prune"])?;
    if path.exists() {
        let head = git(runner, &path, &["rev-parse", "HEAD"])?;
        if head.status == 0 && String::from_utf8_lossy(&head.stdout).trim() == commit {
            return Ok(Snapshot {
                path,
                present: present(&directory)?,
            });
        }
        remove_checkout(runner, repository, &path)?;
    }
    checked(
        runner,
        repository,
        &[
            "worktree",
            "add",
            "--detach",
            "--quiet",
            &path.to_string_lossy(),
            &commit,
        ],
    )?;
    Ok(Snapshot {
        present: present(&directory)?,
        path,
    })
}

/// Removes the snapshot. One that does not exist is already removed.
pub fn remove(runner: &dyn GitCommandRunner, repository: &Path, id: &str) -> Result<Vec<String>> {
    validate_id(id)?;
    let directory = snapshot_directory(runner, repository)?;
    let path = directory.join(id);
    if path.exists() {
        remove_checkout(runner, repository, &path)?;
    }
    checked(runner, repository, &["worktree", "prune"])?;
    present(&directory)
}

fn remove_checkout(runner: &dyn GitCommandRunner, repository: &Path, path: &Path) -> Result<()> {
    let removed = git(
        runner,
        repository,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    )?;
    if removed.status != 0 && path.exists() {
        // The directory is not a registered worktree any more (its record was
        // pruned or never written), so git cannot remove it. Only the review
        // snapshot directory is ever passed here.
        std::fs::remove_dir_all(path)
            .with_context(|| format!("remove review snapshot {}", path.display()))?;
        if path.exists() {
            bail!("review snapshot {} was not removed", path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::git;
    use mj_checkpoint::archive::SystemGit;

    #[test]
    fn a_snapshot_is_kept_replaced_and_removed_across_the_worktrees_of_one_repository() {
        let temp = tempfile::tempdir().unwrap();
        // Git reports resolved paths; macOS temporary directories sit behind a symlink.
        let root = temp.path().canonicalize().unwrap().join("root");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Review Test"]);
        git(&root, &["config", "user.email", "review@example.invalid"]);
        let mut commits = Vec::new();
        for content in ["one\n", "two\n"] {
            std::fs::write(root.join("f"), content).unwrap();
            git(&root, &["add", "f"]);
            git(&root, &["commit", "-qm", content]);
            commits.push(git(&root, &["rev-parse", "HEAD"]).trim().to_owned());
        }
        let linked = root.parent().unwrap().join("linked");
        git(
            &root,
            &["worktree", "add", "-qb", "side", linked.to_str().unwrap()],
        );

        // Asking from a linked worktree lands in the shared git directory.
        let made = create(&SystemGit, &linked, "group-a", &commits[1]).unwrap();
        assert_eq!(made.path, root.join(".git/mj-reviews/group-a"));
        assert_eq!(made.present, ["group-a"]);
        assert_eq!(
            std::fs::read_to_string(made.path.join("f")).unwrap(),
            "two\n"
        );

        // A restored review asks again: the checkout is kept as it is.
        std::fs::write(made.path.join("scratch"), "reviewer notes").unwrap();
        let again = create(&SystemGit, &root, "group-a", &commits[1]).unwrap();
        assert_eq!(again.path, made.path);
        assert!(made.path.join("scratch").exists());

        // The same id at another commit is replaced, not left stale.
        let replaced = create(&SystemGit, &root, "group-a", &commits[0]).unwrap();
        assert_eq!(
            std::fs::read_to_string(replaced.path.join("f")).unwrap(),
            "one\n"
        );
        assert!(!replaced.path.join("scratch").exists());

        // A directory deleted behind git's back does not block the next create.
        std::fs::remove_dir_all(&replaced.path).unwrap();
        create(&SystemGit, &root, "group-a", &commits[1]).unwrap();

        create(&SystemGit, &root, "group-b", &commits[1]).unwrap();
        assert_eq!(remove(&SystemGit, &linked, "group-a").unwrap(), ["group-b"]);
        assert_eq!(remove(&SystemGit, &root, "group-a").unwrap(), ["group-b"]);
        // Only the review's own worktrees are touched.
        let listed = git(&root, &["worktree", "list", "--porcelain"]);
        assert!(listed.contains(linked.to_str().unwrap()), "{listed}");
        assert!(listed.contains("mj-reviews/group-b"), "{listed}");
        assert!(!listed.contains("group-a"), "{listed}");
    }

    #[test]
    fn an_id_or_commit_that_could_name_something_else_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init", "-q", "-b", "main"]);
        for id in ["", "../escape", "a/b", ".hidden", "with space"] {
            assert!(remove(&SystemGit, root, id).is_err(), "{id:?}");
            assert!(create(&SystemGit, root, id, "abc123").is_err(), "{id:?}");
        }
        assert!(create(&SystemGit, root, "group", "--output=x").is_err());
    }
}
