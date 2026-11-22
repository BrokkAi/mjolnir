//! A clone stopped in the middle of a Git operation goes through a real
//! checkpoint archive and a resume-style restore, and both copies must then
//! behave the same way under Git's own next step.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::archive::tests::{clone_repository, initialize_repository, input};
use crate::archive::{
    GitCollectionSpec, GitHistoryMode, RepositorySnapshot, SystemGit, collect_git_snapshot,
    collect_git_snapshot_with_progress, write_archive_atomic,
};
use crate::test_support::{git, git_line};

/// Runs Git with a fixed clock and an editor that accepts every message, so
/// the same step taken in two copies of a repository writes the same commits.
fn step(repository: &Path, arguments: &[&str]) -> (i32, String) {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .env("GIT_AUTHOR_DATE", "@1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "@1700000000 +0000")
        .env("GIT_EDITOR", "true")
        .output()
        .unwrap();
    (
        output.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn step_ok(repository: &Path, arguments: &[&str]) {
    let (status, output) = step(repository, arguments);
    assert_eq!(
        status,
        0,
        "git {arguments:?} in {}: {output}",
        repository.display()
    );
}

/// Starts an operation that has to stop for the person to resolve conflicts.
fn step_stops(repository: &Path, arguments: &[&str]) {
    let (status, output) = step(repository, arguments);
    assert_ne!(status, 0, "git {arguments:?} did not stop: {output}");
}

const A_BASE: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";

/// Each history below writes these files: two separate hunks of `a.txt`, all
/// of `b.txt`, `m.txt` that one side deletes, and the symbolic link `link`.
fn main_changes() -> Vec<(&'static str, Option<&'static str>)> {
    vec![
        ("a.txt", Some("1\nmain 2\n3\n4\n5\n6\n7\nmain 8\n9\n")),
        ("b.txt", Some("main b\n")),
        ("m.txt", Some("main m\n")),
        ("link", Some("b.txt")),
    ]
}

fn side_changes() -> Vec<(&'static str, Option<&'static str>)> {
    vec![
        ("a.txt", Some("1\nside 2\n3\n4\n5\n6\n7\nside 8\n9\n")),
        ("b.txt", Some("side b\n")),
        ("m.txt", None),
        ("link", Some("c.txt")),
    ]
}

struct Lab {
    directory: tempfile::TempDir,
    origin: PathBuf,
    source: PathBuf,
    base: String,
}

fn lab() -> Lab {
    let directory = tempfile::tempdir().unwrap();
    let origin = directory.path().join("origin");
    initialize_repository(&origin);
    change(
        &origin,
        &[
            ("a.txt", Some(A_BASE)),
            ("b.txt", Some("b\n")),
            ("c.txt", Some("c\n")),
            ("d.txt", Some("d\n")),
            ("e.txt", Some("e\n")),
            ("m.txt", Some("m\n")),
            ("link", Some("a.txt")),
        ],
    );
    let base = commit(&origin, "base");
    let source = clone_repository(directory.path(), &origin, "source");
    Lab {
        directory,
        origin,
        source,
        base,
    }
}

/// Writes each path, deleting it for `None`. `link` is a symbolic link to the
/// named target where the platform has them.
fn change(repository: &Path, changes: &[(&str, Option<&str>)]) {
    for (path, contents) in changes {
        let destination = repository.join(path);
        if *path == "link" {
            #[cfg(unix)]
            {
                let _ = fs::remove_file(&destination);
                if let Some(target) = contents {
                    std::os::unix::fs::symlink(target, &destination).unwrap();
                }
            }
            continue;
        }
        match contents {
            Some(contents) => fs::write(&destination, contents).unwrap(),
            None => fs::remove_file(&destination).unwrap(),
        }
    }
}

fn commit(repository: &Path, message: &str) -> String {
    step_ok(repository, &["add", "-A"]);
    step_ok(repository, &["commit", "-qm", message]);
    git_line(repository, &["rev-parse", "HEAD"])
}

fn commit_changes(repository: &Path, message: &str, changes: &[(&str, Option<&str>)]) -> String {
    change(repository, changes);
    commit(repository, message)
}

/// `main` and `side` both change every file, so any operation between them
/// stops on conflicts; `side` has a second commit that applies cleanly.
struct Diverged {
    main: String,
    side_first: String,
    side_tip: String,
}

fn diverge(lab: &Lab) -> Diverged {
    let main = commit_changes(&lab.source, "main work", &main_changes());
    step_ok(&lab.source, &["switch", "-q", "-c", "side", &lab.base]);
    let side_first = commit_changes(&lab.source, "side work", &side_changes());
    let side_tip = commit_changes(&lab.source, "side extra", &[("s.txt", Some("side s\n"))]);
    Diverged {
        main,
        side_first,
        side_tip,
    }
}

/// Dirties a tracked file the operation does not touch, for `--autostash`.
fn dirty_before_starting(repository: &Path) {
    fs::write(repository.join("e.txt"), "autostashed e\n").unwrap();
}

/// Resolves `b.txt` completely and one of the two conflicts in `a.txt`, takes
/// `m.txt` out of the working tree without resolving it, and adds unrelated
/// staged, unstaged and untracked work.
fn partly_resolve(repository: &Path) {
    fs::write(repository.join("b.txt"), "resolved b\n").unwrap();
    step_ok(repository, &["add", "b.txt"]);
    let conflicted = fs::read_to_string(repository.join("a.txt")).unwrap();
    let start = conflicted.find("<<<<<<<").expect("a.txt has a conflict");
    let end = start + conflicted[start..].find(">>>>>>>").unwrap();
    let end = end + conflicted[end..].find('\n').unwrap() + 1;
    let resolved = format!("{}resolved 2\n{}", &conflicted[..start], &conflicted[end..]);
    assert!(
        resolved.contains("<<<<<<<"),
        "a.txt keeps its second conflict"
    );
    fs::write(repository.join("a.txt"), resolved).unwrap();
    let _ = fs::remove_file(repository.join("m.txt"));
    unrelated_work(repository);
    let unmerged = git(repository, &["ls-files", "--unmerged"]);
    let unmerged = String::from_utf8_lossy(&unmerged);
    assert!(
        unmerged.contains("\ta.txt") && unmerged.contains("\tm.txt"),
        "{unmerged}"
    );
}

fn unrelated_work(repository: &Path) {
    fs::write(repository.join("c.txt"), "staged c\n").unwrap();
    step_ok(repository, &["add", "c.txt"]);
    fs::write(repository.join("d.txt"), "unstaged d\n").unwrap();
    fs::create_dir_all(repository.join("notes")).unwrap();
    fs::write(repository.join("notes/untracked.txt"), "untracked\n").unwrap();
}

#[derive(Debug, PartialEq, Eq)]
enum WorktreeEntry {
    Missing,
    File { bytes: Vec<u8>, executable: bool },
    Symlink(PathBuf),
}

/// Everything about a checkout that the operation depends on or that a person
/// can see of it.
#[derive(Debug, PartialEq, Eq)]
struct Observed {
    head: String,
    status: String,
    index: String,
    refs: String,
    worktree: BTreeMap<String, WorktreeEntry>,
    git_directory: BTreeMap<String, Vec<u8>>,
}

fn observe(repository: &Path) -> Observed {
    let text =
        |arguments: &[&str]| String::from_utf8_lossy(&git(repository, arguments)).into_owned();
    let listed = git(
        repository,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    );
    let mut worktree = BTreeMap::new();
    for path in listed
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = String::from_utf8(path.to_vec()).unwrap();
        let file = repository.join(&path);
        let entry = match fs::symlink_metadata(&file) {
            Err(_) => WorktreeEntry::Missing,
            Ok(metadata) if metadata.file_type().is_symlink() => {
                WorktreeEntry::Symlink(fs::read_link(&file).unwrap())
            }
            Ok(metadata) => WorktreeEntry::File {
                bytes: fs::read(&file).unwrap(),
                executable: is_executable(&metadata),
            },
        };
        worktree.insert(path, entry);
    }
    let git_dir = PathBuf::from(git_line(repository, &["rev-parse", "--absolute-git-dir"]));
    let mut git_directory = BTreeMap::new();
    collect_git_directory(&git_dir, &git_dir, &mut git_directory);
    Observed {
        head: text(&["rev-parse", "--symbolic-full-name", "HEAD"]) + &text(&["rev-parse", "HEAD"]),
        status: text(&["status", "--porcelain=v2", "-z", "--branch"]),
        index: text(&["ls-files", "--stage", "-z"]),
        refs: text(&["for-each-ref", "--format=%(refname) %(objectname)"]),
        worktree,
        git_directory,
    }
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

/// Every file in the Git directory except the object store, refs, logs, the
/// index (compared through `ls-files`), and files a clone or a commit writes
/// for itself. Whatever else is there is operation state, so a state file the
/// capture missed shows up here as a difference.
fn collect_git_directory(root: &Path, directory: &Path, found: &mut BTreeMap<String, Vec<u8>>) {
    const NOT_OPERATION_STATE: [&str; 13] = [
        "objects",
        "refs",
        "logs",
        "hooks",
        "info",
        "branches",
        "index",
        "config",
        "description",
        "packed-refs",
        "HEAD",
        "FETCH_HEAD",
        "COMMIT_EDITMSG",
    ];
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if directory == root && NOT_OPERATION_STATE.contains(&relative.as_str()) {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            found.insert(format!("{relative}/"), Vec::new());
            collect_git_directory(root, &path, found);
        } else {
            found.insert(relative, fs::read(&path).unwrap());
        }
    }
}

fn capture(source: &Path, base: &str) -> RepositorySnapshot {
    let snapshot = collect_git_snapshot(
        &crate::test_support::Git231,
        source,
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: "repo".into(),
            history: GitHistoryMode::CloneFrom(base.to_owned()),
            origin_override: None,
        },
    )
    .unwrap();
    assert_eq!(
        git_line(source, &["for-each-ref", "refs/mj"]),
        "",
        "capture left temporary refs behind"
    );
    snapshot
}

/// Restores the archive the way resume does: into a fresh clone of origin,
/// from the verified archive file, returning the checkout path and notices.
fn restore(lab: &Lab, archive: &Path, name: &str) -> (PathBuf, Vec<String>) {
    let workspace = lab.directory.path().join(name);
    fs::create_dir_all(&workspace).unwrap();
    let destination = clone_repository(&workspace, &lab.origin, "repo");
    let notices = crate::checkpoint::restore_single_repository_into_checkout(
        archive,
        &destination,
        &crate::test_support::Git231,
    )
    .unwrap();
    (destination, notices)
}

/// Checkpoints the source through a real legacy archive, restores it, and
/// checks that the in-progress Git operation state is dropped with a notice
/// while conflicted working files survive as ordinary unstaged edits.
fn round_trip_dropped_operation(
    lab: &Lab,
    expected_kind: &str,
    expected_conflicted_paths: &[&str],
) -> (PathBuf, Vec<String>) {
    let snapshot = capture(&lab.source, &lab.base);
    let archive = lab.directory.path().join("checkpoint.hel.zip");
    let mut archive_input = input();
    archive_input.bundle.primary_repository = "repo".into();
    archive_input.repositories = vec![snapshot];
    write_archive_atomic(&archive, &archive_input).unwrap();

    let (restored, notices) = restore(lab, &archive, "restored");
    let before = observe(&lab.source);
    let after = observe(&restored);

    // Working-tree files (including conflicted files, missing_worktree_paths,
    // staged/unstaged edits, and untracked files) survive as ordinary files.
    // A path that was both unmerged and missing from the working tree may no
    // longer appear in `ls-files` once unmerged index stages are dropped.
    let mut expected_worktree = before.worktree;
    expected_worktree.retain(|path, entry| {
        !(*entry == WorktreeEntry::Missing && !after.worktree.contains_key(path))
    });
    assert_eq!(after.worktree, expected_worktree);
    // No active Git operation state or unmerged index stages remain.
    assert!(
        after.git_directory.is_empty(),
        "expected no active Git operation state, found: {:?}",
        after.git_directory.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        git_line(&restored, &["ls-files", "--unmerged"]),
        "",
        "expected no unmerged index entries after dropping legacy operation state"
    );
    assert_eq!(
        notices.len(),
        1,
        "expected one resume notice, got: {notices:?}"
    );
    assert!(
        notices[0].contains(expected_kind),
        "expected notice to name '{expected_kind}': {}",
        notices[0]
    );
    for path in expected_conflicted_paths {
        assert!(
            notices[0].contains(path),
            "expected notice to name preserved conflicted path '{path}': {}",
            notices[0]
        );
    }
    (restored, notices)
}

// Hard-won: #1319: a clone mid-merge could not be checkpointed, so suspend failed forever
// Hard-won: 5a3457a30: Git 2.25 echoed --path-format=absolute, so every clone checkpoint failed to locate its operation state.
#[test]
fn a_clone_stopped_in_a_conflicted_merge_drops_operation_and_preserves_conflicted_files() {
    let lab = lab();
    let diverged = diverge(&lab);
    step_ok(&lab.source, &["switch", "-q", "main"]);
    fs::create_dir(lab.source.join(".git/rr-cache")).unwrap();
    dirty_before_starting(&lab.source);
    step_stops(&lab.source, &["merge", "--autostash", "side"]);
    partly_resolve(&lab.source);

    let (restored, _) = round_trip_dropped_operation(&lab, "merge", &["a.txt", "link", "m.txt"]);
    assert_eq!(git_line(&restored, &["rev-parse", "HEAD"]), diverged.main);
}

#[test]
fn a_clone_stopped_in_a_cherry_pick_sequence_drops_operation_and_preserves_conflicted_files() {
    let lab = lab();
    let diverged = diverge(&lab);
    step_ok(&lab.source, &["switch", "-q", "main"]);
    step_ok(&lab.source, &["branch", "-q", "-D", "side"]);
    step_stops(
        &lab.source,
        &["cherry-pick", &diverged.side_first, &diverged.side_tip],
    );
    partly_resolve(&lab.source);

    let (restored, _) =
        round_trip_dropped_operation(&lab, "cherry-pick", &["a.txt", "link", "m.txt"]);
    assert_eq!(git_line(&restored, &["rev-parse", "HEAD"]), diverged.main);
}

#[test]
fn a_clone_stopped_in_a_conflicted_revert_drops_operation_and_preserves_conflicted_files() {
    let lab = lab();
    let reverted = commit_changes(&lab.source, "main work", &main_changes());
    let later = commit_changes(&lab.source, "later work", &side_changes());
    step_stops(&lab.source, &["revert", &reverted]);
    partly_resolve(&lab.source);

    let (restored, _) = round_trip_dropped_operation(&lab, "revert", &["a.txt", "link", "m.txt"]);
    assert_eq!(git_line(&restored, &["rev-parse", "HEAD"]), later);
}

#[test]
fn a_clone_stopped_in_a_rebase_drops_operation_and_preserves_conflicted_files() {
    for backend in ["--merge", "--apply"] {
        let lab = lab();
        let _diverged = diverge(&lab);
        dirty_before_starting(&lab.source);
        step_stops(&lab.source, &["rebase", backend, "--autostash", "main"]);
        partly_resolve(&lab.source);

        let (_restored, _) =
            round_trip_dropped_operation(&lab, "rebase", &["a.txt", "link", "m.txt"]);
    }
}

#[test]
fn a_clone_in_the_middle_of_a_bisect_drops_operation_with_notice() {
    let lab = lab();
    for number in 1..=6 {
        commit_changes(
            &lab.source,
            &format!("history {number}"),
            &[("h.txt", Some(&format!("{number}\n")))],
        );
    }
    step_ok(&lab.source, &["bisect", "start"]);
    step_ok(&lab.source, &["bisect", "bad"]);
    step_ok(&lab.source, &["bisect", "good", &lab.base]);
    step_ok(&lab.source, &["bisect", "bad"]);
    unrelated_work(&lab.source);

    let (restored, _) = round_trip_dropped_operation(&lab, "bisect", &[]);
    assert_eq!(git_line(&restored, &["for-each-ref", "refs/bisect"]), "");
}

#[test]
fn a_capture_that_fails_mid_operation_leaves_no_temporary_refs() {
    let lab = lab();
    diverge(&lab);
    step_ok(&lab.source, &["switch", "-q", "main"]);
    step_stops(&lab.source, &["merge", "side"]);
    partly_resolve(&lab.source);

    let error = collect_git_snapshot_with_progress(
        &SystemGit,
        &lab.source,
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: "repo".into(),
            history: GitHistoryMode::CloneFrom(lab.base.clone()),
            origin_override: None,
        },
        crate::archive::UntrackedFiles::Include,
        &|_| Err(anyhow::anyhow!("stopped by the caller")),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("stopped by the caller"),
        "{error:#}"
    );
    assert_eq!(git_line(&lab.source, &["for-each-ref", "refs/mj"]), "");
}
