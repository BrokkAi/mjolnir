//! Exercise the target-side bundle export through the standalone worker.

use std::{fs, path::Path};

use mj_checkpoint::archive::EXPORT_REFUSED_EXIT_CODE;
use mj_core::targets::{CommandExecutor, CommandOutput, CommandSpec, ProcessExecutor};

fn run(root: &Path, program: &str, args: &[&str]) -> CommandOutput {
    let mut command = CommandSpec::new(program, args.iter().copied());
    command.cwd = Some(root.to_owned());
    ProcessExecutor.execute(&command).unwrap()
}

fn git(root: &Path, args: &[&str]) -> String {
    let result = run(root, "git", args);
    assert_eq!(
        result.status,
        0,
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}

fn bundle(repository: &Path, args: &[&str]) -> CommandOutput {
    let command = CommandSpec::new(
        env!("CARGO_BIN_EXE_mj-worker"),
        [
            "worker",
            "bundle",
            "--repository",
            repository.to_str().unwrap(),
        ]
        .into_iter()
        .chain(args.iter().copied()),
    );
    ProcessExecutor.execute(&command).unwrap()
}

#[test]
fn session_bundle_worker_writes_a_native_bundle_of_the_commits_beyond_its_base() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let origin = root.join("origin.git");
    git(
        root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    let seed = root.join("seed");
    git(
        root,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            seed.to_str().unwrap(),
        ],
    );
    git(&seed, &["config", "user.name", "Test"]);
    git(&seed, &["config", "user.email", "test@example.com"]);
    fs::write(seed.join("tracked.txt"), b"base\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-qm", "base"]);
    git(&seed, &["push", "-q", "-u", "origin", "main"]);
    let base = git(&seed, &["rev-parse", "HEAD"]);

    let work = root.join("work");
    git(
        root,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    git(&work, &["config", "user.name", "Test"]);
    git(&work, &["config", "user.email", "test@example.com"]);
    // Incompressible content past one pipe buffer, so the bundle cannot fit
    // in a 64 KiB stdout pipe and the worker's write has to be drained.
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let large: Vec<u8> = (0..300_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect();
    fs::write(work.join("large.bin"), &large).unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "pushed"]);
    git(&work, &["push", "-q", "origin", "main"]);
    fs::write(work.join("local.txt"), b"local\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "local"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    // An uncommitted edit is not in a bundle.
    fs::write(work.join("tracked.txt"), b"dirty\n").unwrap();

    let output = bundle(&work, &["--base", &base]);
    assert_eq!(
        output.status,
        0,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.len() > 64 * 1024, "{}", output.stdout.len());
    let file = root.join("session.bundle");
    fs::write(&file, &output.stdout).unwrap();
    // The seed holds only the base, so the bundle must carry the pushed
    // commit as well as the local one.
    git(&seed, &["bundle", "verify", file.to_str().unwrap()]);
    git(
        &seed,
        &[
            "fetch",
            "-q",
            file.to_str().unwrap(),
            "HEAD:refs/heads/imported",
        ],
    );
    assert_eq!(git(&seed, &["rev-parse", "imported"]), head);
    assert_eq!(
        git(
            &seed,
            &["rev-list", "--count", "imported", &format!("^{base}")]
        ),
        "2"
    );
    let dirty = fs::read(file).unwrap();
    assert!(!dirty.windows(5).any(|window| window == b"dirty"));

    // No commits beyond the base: a refusal that names the patch export.
    let refused = bundle(&work, &["--base", &head]);
    assert_eq!(refused.status, EXPORT_REFUSED_EXIT_CODE);
    let reason = String::from_utf8(refused.stdout).unwrap();
    assert!(reason.contains("mj export --kind patch"), "{reason}");
}
