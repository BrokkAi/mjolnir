//! Exercise the target-side diff arguments through the standalone worker.

use std::{fs, path::Path};

use mj_checkpoint::archive::{EXPORT_REFUSED_EXIT_CODE, SessionDiff};
use mj_core::targets::{CommandExecutor, CommandSpec, ProcessExecutor};

fn git(root: &Path, args: &[&str]) {
    let mut command = CommandSpec::new("git", args.iter().copied());
    command.cwd = Some(root.to_owned());
    let result = ProcessExecutor.execute(&command).unwrap();
    assert_eq!(
        result.status,
        0,
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn session_diff_worker_accepts_repeated_literal_paths_and_exports_a_patch() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.com"]);
    fs::write(root.join("-dash.txt"), b"old dash\n").unwrap();
    fs::write(root.join("amp & space.txt"), b"old space\n").unwrap();
    fs::write(root.join("latin.txt"), b"caf\xe9\n").unwrap();
    git(
        root,
        &["add", "--", "-dash.txt", "amp & space.txt", "latin.txt"],
    );
    git(root, &["commit", "-qm", "base"]);

    fs::write(root.join("-dash.txt"), b"new dash\n").unwrap();
    fs::write(root.join("amp & space.txt"), b"new space\n").unwrap();
    fs::write(root.join("new.txt"), b"untracked body\n").unwrap();
    let diff = |args: &[&str]| {
        let command = CommandSpec::new(
            env!("CARGO_BIN_EXE_mj-worker"),
            [
                "worker",
                "diff",
                "--repository",
                root.to_str().unwrap(),
                "--base=HEAD",
            ]
            .into_iter()
            .chain(args.iter().copied()),
        );
        ProcessExecutor.execute(&command).unwrap()
    };

    let patch = diff(&["--json", "--path=-dash.txt", "--path=amp & space.txt"]);
    assert_eq!(
        patch.status,
        0,
        "{}",
        String::from_utf8_lossy(&patch.stderr)
    );
    let patch: SessionDiff = serde_json::from_slice(&patch.stdout).unwrap();
    assert!(patch.diff.contains("+new dash\n") && patch.diff.contains("+new space\n"));
    let exported = diff(&["--patch"]);
    assert_eq!(
        exported.status,
        0,
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    assert_eq!(exported.stdout, patch.diff.as_bytes());

    // A patch is the bytes Git wrote; content that is not UTF-8 reaches the
    // caller unchanged, so `git apply` reproduces it.
    fs::write(root.join("latin.txt"), b"caf\xe9 \xff\n").unwrap();
    let exported = diff(&["--patch"]);
    assert_eq!(exported.status, 0);
    assert!(
        exported
            .stdout
            .windows(b"+caf\xe9 \xff\n".len())
            .any(|window| window == b"+caf\xe9 \xff\n")
    );
    fs::write(root.join("latin.txt"), b"caf\xe9\n").unwrap();

    let refused = diff(&["--path=new.txt"]);
    assert_eq!(refused.status, EXPORT_REFUSED_EXIT_CODE);
    let reason = String::from_utf8(refused.stdout).unwrap();
    assert!(reason.contains("new.txt") && reason.contains("mj fetch"));
}
