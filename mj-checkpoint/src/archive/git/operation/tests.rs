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

/// Settles every path `partly_resolve` left unmerged.
const FINISH_RESOLVING: &[&[&str]] = &[&["add", "a.txt", "link"], &["rm", "-q", "m.txt"]];

fn finish_resolving(repository: &Path) {
    fs::write(
        repository.join("a.txt"),
        "1\nresolved 2\n3\n4\n5\n6\n7\nresolved 8\n9\n",
    )
    .unwrap();
    for arguments in FINISH_RESOLVING {
        let arguments = arguments
            .iter()
            .copied()
            .filter(|argument| cfg!(unix) || *argument != "link")
            .collect::<Vec<_>>();
        step_ok(repository, &arguments);
    }
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

/// An exact copy of a checkout, Git directory included.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target);
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path()).unwrap(), &target).unwrap();
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn capture(source: &Path, base: &str) -> RepositorySnapshot {
    let snapshot = collect_git_snapshot(
        &SystemGit,
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
/// from the verified archive file.
fn restore(lab: &Lab, archive: &Path, name: &str) -> PathBuf {
    let workspace = lab.directory.path().join(name);
    fs::create_dir_all(&workspace).unwrap();
    let destination = clone_repository(&workspace, &lab.origin, "repo");
    crate::checkpoint::restore_repositories(archive, &workspace, &SystemGit).unwrap();
    destination
}

struct RoundTrip {
    finished_source: PathBuf,
    finished_restored: PathBuf,
    abandoned_copy: PathBuf,
    abandoned_restored: PathBuf,
}

/// Checkpoints the source through a real archive, restores it twice, and
/// checks that each restored copy is the source: first as it stands, then
/// after Git's own step that finishes the operation (taken in the source and
/// one copy) and after the step that abandons it (taken in an exact copy of
/// the source and the other).
fn round_trip(lab: &Lab, finish: &dyn Fn(&Path), abandon: &dyn Fn(&Path)) -> RoundTrip {
    let abandoned_copy = lab.directory.path().join("abandoned-copy");
    copy_tree(&lab.source, &abandoned_copy);
    // The copy's index records the source's file times; Git's own abort
    // steps do not refresh it before they compare.
    step(&abandoned_copy, &["update-index", "-q", "--refresh"]);
    let snapshot = capture(&lab.source, &lab.base);
    let archive = lab.directory.path().join("checkpoint.hel.zip");
    let mut archive_input = input();
    archive_input.bundle.primary_repository = "repo".into();
    archive_input.repositories = vec![snapshot];
    write_archive_atomic(&archive, &archive_input).unwrap();

    let finished_restored = restore(lab, &archive, "finished");
    let abandoned_restored = restore(lab, &archive, "abandoned");
    let before = observe(&lab.source);
    assert_eq!(observe(&finished_restored), before);
    assert_eq!(observe(&abandoned_restored), before);

    finish(&lab.source);
    finish(&finished_restored);
    assert_eq!(observe(&finished_restored), observe(&lab.source));
    abandon(&abandoned_copy);
    abandon(&abandoned_restored);
    assert_eq!(observe(&abandoned_restored), observe(&abandoned_copy));
    RoundTrip {
        finished_source: lab.source.clone(),
        finished_restored,
        abandoned_copy,
        abandoned_restored,
    }
}

fn parents(repository: &Path) -> Vec<String> {
    git_line(repository, &["rev-list", "--parents", "-n", "1", "HEAD"])
        .split(' ')
        .skip(1)
        .map(str::to_owned)
        .collect()
}

// Hard-won: #1319: a clone mid-merge could not be checkpointed, so suspend failed forever
#[test]
fn a_clone_stopped_in_a_conflicted_merge_resumes_mid_merge() {
    let lab = lab();
    let diverged = diverge(&lab);
    step_ok(&lab.source, &["switch", "-q", "main"]);
    // An rr-cache directory turns rerere on, so the merge records its
    // conflicts there and in MERGE_RR.
    fs::create_dir(lab.source.join(".git/rr-cache")).unwrap();
    dirty_before_starting(&lab.source);
    step_stops(&lab.source, &["merge", "--autostash", "side"]);
    partly_resolve(&lab.source);

    let checked = round_trip(
        &lab,
        &|repository| {
            finish_resolving(repository);
            step_ok(repository, &["commit", "--no-edit"]);
        },
        &|repository| step_ok(repository, &["merge", "--abort"]),
    );
    assert_eq!(
        parents(&checked.finished_restored),
        [diverged.main.clone(), diverged.side_tip]
    );
    assert_eq!(
        git_line(&checked.abandoned_restored, &["rev-parse", "HEAD"]),
        diverged.main
    );
    assert_eq!(
        fs::read_to_string(checked.abandoned_restored.join("e.txt")).unwrap(),
        "autostashed e\n"
    );
}

#[test]
fn a_clone_stopped_in_a_cherry_pick_sequence_resumes_it() {
    let lab = lab();
    let diverged = diverge(&lab);
    step_ok(&lab.source, &["switch", "-q", "main"]);
    // The picked commits are then named only by the operation: the second
    // one only by the sequencer's todo list, with an abbreviated id.
    step_ok(&lab.source, &["branch", "-q", "-D", "side"]);
    step_stops(
        &lab.source,
        &["cherry-pick", &diverged.side_first, &diverged.side_tip],
    );
    partly_resolve(&lab.source);

    let checked = round_trip(
        &lab,
        &|repository| {
            finish_resolving(repository);
            step_ok(repository, &["cherry-pick", "--continue"]);
        },
        &|repository| step_ok(repository, &["cherry-pick", "--abort"]),
    );
    assert_eq!(
        fs::read_to_string(checked.finished_restored.join("s.txt")).unwrap(),
        "side s\n"
    );
    assert_eq!(
        git_line(&checked.finished_restored, &["rev-parse", "HEAD~2"]),
        diverged.main
    );
    assert_eq!(
        git_line(&checked.abandoned_restored, &["rev-parse", "HEAD"]),
        diverged.main
    );
}

#[test]
fn a_clone_stopped_in_a_conflicted_revert_resumes_it() {
    let lab = lab();
    let reverted = commit_changes(&lab.source, "main work", &main_changes());
    let later = commit_changes(&lab.source, "later work", &side_changes());
    step_stops(&lab.source, &["revert", &reverted]);
    partly_resolve(&lab.source);

    let checked = round_trip(
        &lab,
        &|repository| {
            finish_resolving(repository);
            step_ok(repository, &["revert", "--continue"]);
        },
        &|repository| step_ok(repository, &["revert", "--abort"]),
    );
    assert_eq!(
        parents(&checked.finished_restored),
        std::slice::from_ref(&later)
    );
    assert_eq!(
        git_line(&checked.abandoned_restored, &["rev-parse", "HEAD"]),
        later
    );
}

#[test]
fn a_clone_stopped_in_a_rebase_resumes_it_with_either_backend() {
    for backend in ["--merge", "--apply"] {
        let lab = lab();
        let diverged = diverge(&lab);
        dirty_before_starting(&lab.source);
        step_stops(&lab.source, &["rebase", backend, "--autostash", "main"]);
        partly_resolve(&lab.source);

        let checked = round_trip(
            &lab,
            &|repository| {
                finish_resolving(repository);
                // A rebase continues only once nothing is left unstaged.
                step_ok(repository, &["add", "d.txt"]);
                step_ok(repository, &["rebase", "--continue"]);
            },
            &|repository| step_ok(repository, &["rebase", "--abort"]),
        );
        for repository in [&checked.finished_source, &checked.finished_restored] {
            assert_eq!(
                git_line(repository, &["symbolic-ref", "HEAD"]),
                "refs/heads/side",
                "{backend}"
            );
            assert_eq!(
                git_line(repository, &["rev-parse", "HEAD~2"]),
                diverged.main,
                "{backend}"
            );
        }
        for repository in [&checked.abandoned_copy, &checked.abandoned_restored] {
            assert_eq!(
                git_line(repository, &["rev-parse", "HEAD"]),
                diverged.side_tip,
                "{backend}"
            );
        }
    }
}

#[test]
fn a_clone_in_the_middle_of_a_bisect_resumes_it() {
    let lab = lab();
    for number in 1..=6 {
        commit_changes(
            &lab.source,
            &format!("history {number}"),
            &[("h.txt", Some(&format!("{number}\n")))],
        );
    }
    let tip = git_line(&lab.source, &["rev-parse", "HEAD"]);
    step_ok(&lab.source, &["bisect", "start"]);
    step_ok(&lab.source, &["bisect", "bad"]);
    step_ok(&lab.source, &["bisect", "good", &lab.base]);
    step_ok(&lab.source, &["bisect", "bad"]);
    unrelated_work(&lab.source);
    let log = git(&lab.source, &["bisect", "log"]);

    let checked = round_trip(
        &lab,
        &|repository| {
            assert_eq!(git(repository, &["bisect", "log"]), log);
            step_ok(repository, &["bisect", "good"]);
        },
        &|repository| step_ok(repository, &["bisect", "reset"]),
    );
    assert_ne!(
        git_line(&checked.finished_restored, &["rev-parse", "HEAD"]),
        tip
    );
    assert_eq!(
        git_line(&checked.abandoned_restored, &["rev-parse", "HEAD"]),
        tip
    );
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
        true,
        &|_| Err(anyhow::anyhow!("stopped by the caller")),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("stopped by the caller"),
        "{error:#}"
    );
    assert_eq!(git_line(&lab.source, &["for-each-ref", "refs/mj"]), "");
}
