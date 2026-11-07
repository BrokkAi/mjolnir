//! Worker-owned review snapshots: a detached worktree at the reviewed commit,
//! so reviewers read files that match the diff while the requester keeps
//! working in its own checkout.
//!
//! The checkout lives at `<git-common-dir>/mj-reviews/<instance>/<id>`. The
//! git directory is never part of a working tree, so the checkout cannot show
//! up in `git status` or be committed, and it is shared by every worktree of
//! the repository. Every Mjolnir instance that reviews in a repository (the
//! default daemon, each `--instance`, each isolated `MJ_DATA_DIR`) has its own
//! `<instance>` directory, so listing and removal stay inside the asker's
//! namespace and never touch another instance's live checkout. The review host
//! owns which ids should exist; this module only creates, removes and lists
//! them. It never runs a repository-wide `git worktree prune`: that drops the
//! records of the user's own worktrees on unmounted volumes. Removal deletes
//! only the snapshot's own record.

use anyhow::{Context, Result, bail, ensure};
use mj_checkpoint::archive::{GitCommand, GitCommandRunner, GitOutput};
use std::path::{Path, PathBuf};

/// The directory inside the git directory that holds review snapshots.
const SNAPSHOT_DIRECTORY: &str = "mj-reviews";

#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub path: PathBuf,
    /// Every snapshot id the instance holds in the repository once the
    /// operation finished.
    pub present: Vec<String>,
    /// What the creation left out: submodules it could not populate without
    /// a network fetch.
    pub notes: Vec<String>,
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

/// An id or instance key becomes a directory name, so it may not name
/// anything else.
fn validate_id(kind: &str, id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "invalid review snapshot {kind} {id:?}"
    );
    Ok(())
}

fn common_directory(runner: &dyn GitCommandRunner, repository: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(checked(
        runner,
        repository,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?))
}

/// The instance's directory of snapshots in the repository.
fn snapshot_directory(common: &Path, instance: &str) -> Result<PathBuf> {
    validate_id("instance", instance)?;
    Ok(common.join(SNAPSHOT_DIRECTORY).join(instance))
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

/// Deletes the repository's record of the worktree at `path`, and only that
/// record: the admin directory under `<common>/worktrees` whose `gitdir` file
/// points at the worktree's `.git`. This is the case git's own removal cannot
/// handle, a record whose directory is already gone.
fn forget_record(common: &Path, path: &Path) -> Result<()> {
    let records = common.join("worktrees");
    let entries = match std::fs::read_dir(&records) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("list {}", records.display()));
        }
    };
    let pointer = path.join(".git");
    for entry in entries {
        let record = entry
            .with_context(|| format!("list {}", records.display()))?
            .path();
        let Ok(target) = std::fs::read_to_string(record.join("gitdir")) else {
            continue;
        };
        if Path::new(target.trim()) == pointer {
            std::fs::remove_dir_all(&record)
                .with_context(|| format!("remove worktree record {}", record.display()))?;
        }
    }
    Ok(())
}

/// Creates the snapshot of `commit`, or keeps the one already there when it
/// is at that commit. A restored review asks again, so this is idempotent.
pub fn create(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    instance: &str,
    id: &str,
    commit: &str,
) -> Result<Snapshot> {
    validate_id("id", id)?;
    ensure!(
        !commit.is_empty() && commit.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "review snapshot commit {commit:?} is not a commit id"
    );
    let common = common_directory(runner, repository)?;
    let directory = snapshot_directory(&common, instance)?;
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
    if path.exists() {
        let head = git(runner, &path, &["rev-parse", "HEAD"])?;
        if head.status == 0 && String::from_utf8_lossy(&head.stdout).trim() == commit {
            return Ok(Snapshot {
                path,
                present: present(&directory)?,
                notes: Vec::new(),
            });
        }
        remove_checkout(runner, repository, &common, &path)?;
    }
    // A record whose directory was deleted behind git's back makes `worktree
    // add` refuse the path; drop that one record.
    forget_record(&common, &path)?;
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
    let notes = populate_submodules(runner, repository, &path);
    Ok(Snapshot {
        present: present(&directory)?,
        path,
        notes,
    })
}

/// Removes the snapshot. One that does not exist is already removed.
pub fn remove(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    instance: &str,
    id: &str,
) -> Result<Vec<String>> {
    validate_id("id", id)?;
    let common = common_directory(runner, repository)?;
    let directory = snapshot_directory(&common, instance)?;
    let path = directory.join(id);
    remove_checkout(runner, repository, &common, &path)?;
    present(&directory)
}

fn remove_checkout(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    common: &Path,
    path: &Path,
) -> Result<()> {
    if path.exists() {
        let removed = git(
            runner,
            repository,
            &["worktree", "remove", "--force", &path.to_string_lossy()],
        )?;
        if removed.status != 0 && path.exists() {
            // The directory is not a registered worktree any more (its record
            // was never written or already dropped), so git cannot remove it.
            // Only a review snapshot directory is ever passed here.
            std::fs::remove_dir_all(path)
                .with_context(|| format!("remove review snapshot {}", path.display()))?;
            if path.exists() {
                bail!("review snapshot {} was not removed", path.display());
            }
        }
    }
    forget_record(common, path)
}

/// Fills the submodules the requester has initialized. `git worktree add`
/// leaves every submodule directory empty, so a reviewer reading it would see
/// nothing there. Each is cloned from the requester's own copy of that
/// submodule: no network access, and only when that copy already holds the
/// commit the reviewed commit pins. Anything else is skipped and reported in
/// the returned notes; a missing submodule never fails the snapshot.
fn populate_submodules(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    snapshot: &Path,
) -> Vec<String> {
    let mut notes = Vec::new();
    // Initialization is recorded in the repository's shared configuration, so
    // the snapshot sees what the requester initialized.
    let listed = match git(
        runner,
        snapshot,
        &["config", "--get-regexp", r"^submodule\..*\.url$"],
    ) {
        Ok(listed) => listed,
        Err(error) => {
            notes.push(format!("submodules were not checked: {error:#}"));
            return notes;
        }
    };
    // Exit status 1 means no submodule is initialized.
    if listed.status > 1 {
        notes.push(format!(
            "submodules were not checked: {}",
            String::from_utf8_lossy(&listed.stderr).trim()
        ));
        return notes;
    }
    let urls = String::from_utf8_lossy(&listed.stdout).into_owned();
    for line in urls.lines() {
        let Some(name) = line
            .split_whitespace()
            .next()
            .and_then(|key| key.strip_prefix("submodule."))
            .and_then(|key| key.strip_suffix(".url"))
        else {
            continue;
        };
        if let Err(error) = populate_submodule(runner, repository, snapshot, name) {
            notes.push(format!(
                "submodule {name} is empty in the snapshot: {error:#}"
            ));
        }
    }
    notes
}

fn populate_submodule(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    snapshot: &Path,
    name: &str,
) -> Result<()> {
    let path = checked(
        runner,
        snapshot,
        &[
            "config",
            "-f",
            ".gitmodules",
            "--get",
            &format!("submodule.{name}.path"),
        ],
    )
    .context("it has no path in .gitmodules at the reviewed commit")?;
    let listing = checked(runner, snapshot, &["ls-tree", "HEAD", "--", &path])?;
    let pinned = listing
        .strip_prefix("160000 commit ")
        .and_then(|rest| rest.split_whitespace().next())
        .context("the reviewed commit does not pin it")?
        .to_owned();
    let modules = checked(
        runner,
        repository,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            &format!("modules/{name}"),
        ],
    )?;
    let source = [PathBuf::from(modules), repository.join(&path).join(".git")]
        .into_iter()
        .find(|candidate| candidate.is_dir())
        .context("the requester's checkout has no local copy of it")?;
    let present = git(
        runner,
        &source,
        &["cat-file", "-e", &format!("{pinned}^{{commit}}")],
    )?;
    ensure!(
        present.status == 0,
        "the requester's copy lacks commit {pinned}, and fetching it needs the network"
    );
    // The url override sends the clone to the requester's copy, which makes
    // it a local transfer. Git blocks file-transport submodule clones unless
    // allowed, and this one is the requester's own repository.
    checked(
        runner,
        snapshot,
        &[
            "-c",
            "protocol.file.allow=always",
            "-c",
            &format!("submodule.{name}.url={}", source.display()),
            "submodule",
            "update",
            "--init",
            "--no-fetch",
            "--",
            &path,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::git;
    use mj_checkpoint::archive::SystemGit;

    const INSTANCE: &str = "default0123456789";

    fn repository_with_two_commits(root: &Path) -> Vec<String> {
        std::fs::create_dir(root).unwrap();
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.name", "Review Test"]);
        git(root, &["config", "user.email", "review@example.invalid"]);
        let mut commits = Vec::new();
        for content in ["one\n", "two\n"] {
            std::fs::write(root.join("f"), content).unwrap();
            git(root, &["add", "f"]);
            git(root, &["commit", "-qm", content]);
            commits.push(git(root, &["rev-parse", "HEAD"]).trim().to_owned());
        }
        commits
    }

    #[test]
    fn a_snapshot_is_kept_replaced_and_removed_across_the_worktrees_of_one_repository() {
        let temp = tempfile::tempdir().unwrap();
        // Git reports resolved paths; macOS temporary directories sit behind a symlink.
        let root = temp.path().canonicalize().unwrap().join("root");
        let commits = repository_with_two_commits(&root);
        let linked = root.parent().unwrap().join("linked");
        git(
            &root,
            &["worktree", "add", "-qb", "side", linked.to_str().unwrap()],
        );

        // Asking from a linked worktree lands in the shared git directory.
        let made = create(&SystemGit, &linked, INSTANCE, "group-a", &commits[1]).unwrap();
        assert_eq!(
            made.path,
            root.join(".git/mj-reviews").join(INSTANCE).join("group-a")
        );
        assert_eq!(made.present, ["group-a"]);
        assert_eq!(
            std::fs::read_to_string(made.path.join("f")).unwrap(),
            "two\n"
        );

        // A restored review asks again: the checkout is kept as it is.
        std::fs::write(made.path.join("scratch"), "reviewer notes").unwrap();
        let again = create(&SystemGit, &root, INSTANCE, "group-a", &commits[1]).unwrap();
        assert_eq!(again.path, made.path);
        assert!(made.path.join("scratch").exists());

        // The same id at another commit is replaced, not left stale.
        let replaced = create(&SystemGit, &root, INSTANCE, "group-a", &commits[0]).unwrap();
        assert_eq!(
            std::fs::read_to_string(replaced.path.join("f")).unwrap(),
            "one\n"
        );
        assert!(!replaced.path.join("scratch").exists());

        // A directory deleted behind git's back does not block the next create.
        std::fs::remove_dir_all(&replaced.path).unwrap();
        create(&SystemGit, &root, INSTANCE, "group-a", &commits[1]).unwrap();

        create(&SystemGit, &root, INSTANCE, "group-b", &commits[1]).unwrap();
        assert_eq!(
            remove(&SystemGit, &linked, INSTANCE, "group-a").unwrap(),
            ["group-b"]
        );
        assert_eq!(
            remove(&SystemGit, &root, INSTANCE, "group-a").unwrap(),
            ["group-b"]
        );
        // Only the review's own worktrees are touched.
        let listed = git(&root, &["worktree", "list", "--porcelain"]);
        assert!(listed.contains(linked.to_str().unwrap()), "{listed}");
        assert!(listed.contains("mj-reviews"), "{listed}");
        assert!(listed.contains("group-b"), "{listed}");
        assert!(!listed.contains("group-a"), "{listed}");
    }

    #[test]
    fn instances_sharing_a_repository_never_list_or_remove_each_others_checkouts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("root");
        let commits = repository_with_two_commits(&root);

        let mine = create(
            &SystemGit,
            &root,
            "default0123456789",
            "group-a",
            &commits[1],
        )
        .unwrap();
        let theirs = create(&SystemGit, &root, "other-test", "group-a", &commits[1]).unwrap();
        create(&SystemGit, &root, "other-test", "group-live", &commits[1]).unwrap();
        assert_ne!(mine.path, theirs.path);
        // Each instance sees its own ids only, even for the same id.
        assert_eq!(mine.present, ["group-a"]);
        assert_eq!(theirs.present, ["group-a"]);

        // The first instance removes everything it holds; the other's live
        // checkouts and its records stay.
        assert!(
            remove(&SystemGit, &root, "default0123456789", "group-a")
                .unwrap()
                .is_empty()
        );
        assert!(!mine.path.exists());
        assert!(theirs.path.join("f").exists());
        assert_eq!(
            git(&root, &["rev-parse", "HEAD"]).trim(),
            commits[1],
            "the repository itself is untouched"
        );
        let listed = git(&root, &["worktree", "list", "--porcelain"]);
        assert!(listed.contains("other-test/group-a"), "{listed}");
        assert!(listed.contains("other-test/group-live"), "{listed}");
        assert!(!listed.contains("default0123456789"), "{listed}");
    }

    #[test]
    fn removal_drops_only_the_snapshots_own_record_not_a_users_prunable_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("root");
        let commits = repository_with_two_commits(&root);
        // A worktree on a volume that is not mounted right now: its directory
        // is missing, which `git worktree prune` would treat as removable.
        let away = temp.path().canonicalize().unwrap().join("unmounted");
        git(
            &root,
            &["worktree", "add", "--detach", "-q", away.to_str().unwrap()],
        );
        let hidden = temp.path().join("unmounted-hidden");
        std::fs::rename(&away, &hidden).unwrap();

        create(&SystemGit, &root, INSTANCE, "group-a", &commits[1]).unwrap();
        create(&SystemGit, &root, INSTANCE, "group-gone", &commits[1]).unwrap();
        // One snapshot loses its directory; the other is removed normally.
        std::fs::remove_dir_all(
            root.join(".git/mj-reviews")
                .join(INSTANCE)
                .join("group-gone"),
        )
        .unwrap();
        remove(&SystemGit, &root, INSTANCE, "group-a").unwrap();
        remove(&SystemGit, &root, INSTANCE, "group-gone").unwrap();
        // Creating over a stale record of its own also leaves the user's alone.
        create(&SystemGit, &root, INSTANCE, "group-b", &commits[1]).unwrap();

        std::fs::rename(&hidden, &away).unwrap();
        let listed = git(&root, &["worktree", "list", "--porcelain"]);
        assert!(listed.contains(away.to_str().unwrap()), "{listed}");
        assert!(!listed.contains("group-a"), "{listed}");
        assert!(!listed.contains("group-gone"), "{listed}");
        assert_eq!(
            std::fs::read_dir(root.join(".git/worktrees"))
                .unwrap()
                .count(),
            2,
            "the user's record and group-b's"
        );
    }

    #[test]
    fn initialized_submodules_are_populated_offline_and_an_unavailable_one_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let library = base.join("library");
        std::fs::create_dir(&library).unwrap();
        git(&library, &["init", "-q", "-b", "main"]);
        git(&library, &["config", "user.name", "Review Test"]);
        git(
            &library,
            &["config", "user.email", "review@example.invalid"],
        );
        std::fs::write(library.join("lib.txt"), "library\n").unwrap();
        git(&library, &["add", "lib.txt"]);
        git(&library, &["commit", "-qm", "library"]);

        let root = base.join("root");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Review Test"]);
        git(&root, &["config", "user.email", "review@example.invalid"]);
        git(
            &root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                library.to_str().unwrap(),
                "vendor/library",
            ],
        );
        git(&root, &["commit", "-qm", "add the submodule"]);
        let reviewed = git(&root, &["rev-parse", "HEAD"]).trim().to_owned();
        // The origin goes away: populating has to work from the requester's copy.
        std::fs::remove_dir_all(&library).unwrap();

        let made = create(&SystemGit, &root, INSTANCE, "group-a", &reviewed).unwrap();
        assert_eq!(made.notes, Vec::<String>::new());
        assert_eq!(
            std::fs::read_to_string(made.path.join("vendor/library/lib.txt")).unwrap(),
            "library\n"
        );
        assert_eq!(
            remove(&SystemGit, &root, INSTANCE, "group-a").unwrap(),
            Vec::<String>::new()
        );
        assert!(!made.path.exists());

        // The reviewed commit pins a commit the requester's copy does not
        // hold: the snapshot is still made, and the note says what is missing.
        let inner = root.join("vendor/library");
        std::fs::write(inner.join("lib.txt"), "newer\n").unwrap();
        git(&inner, &["commit", "-qam", "newer"]);
        git(&root, &["commit", "-qam", "pin the newer commit"]);
        let pinned = git(&root, &["rev-parse", "HEAD"]).trim().to_owned();
        let new_pin = git(&inner, &["rev-parse", "HEAD"]).trim().to_owned();
        git(&inner, &["reset", "-q", "--hard", "HEAD~1"]);
        git(&inner, &["reflog", "expire", "--expire=now", "--all"]);
        git(&inner, &["gc", "-q", "--prune=now"]);
        let made = create(&SystemGit, &root, INSTANCE, "group-b", &pinned).unwrap();
        assert!(made.path.join("f").exists() || made.path.join(".gitmodules").exists());
        assert!(!made.path.join("vendor/library/lib.txt").exists());
        assert_eq!(made.notes.len(), 1, "{:?}", made.notes);
        assert!(
            made.notes[0].contains("vendor/library") || made.notes[0].contains("library"),
            "{:?}",
            made.notes
        );
        assert!(made.notes[0].contains(&new_pin), "{:?}", made.notes);
        assert!(made.notes[0].contains("network"), "{:?}", made.notes);
    }

    #[test]
    fn an_id_commit_or_instance_that_could_name_something_else_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init", "-q", "-b", "main"]);
        for id in ["", "../escape", "a/b", ".hidden", "with space"] {
            assert!(remove(&SystemGit, root, INSTANCE, id).is_err(), "{id:?}");
            assert!(
                create(&SystemGit, root, INSTANCE, id, "abc123").is_err(),
                "{id:?}"
            );
            assert!(remove(&SystemGit, root, id, "group").is_err(), "{id:?}");
        }
        assert!(create(&SystemGit, root, INSTANCE, "group", "--output=x").is_err());
    }
}
