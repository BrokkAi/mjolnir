//! Shared fixtures for the CLI unit tests.

/// Re-runs one test in a child process with its own `MJ_DATA_DIR` and
/// `MJ_CONFIG_DIR`.
///
/// `MJ_DATA_DIR` and `MJ_CONFIG_DIR` are process-global, so a test that needs
/// its own store cannot share the test binary with the rest of the suite. The
/// parent spawns the child, waits for it and asserts that it passed; the child
/// sees `child_var` set and runs the body.
///
/// Returns `true` in the parent, which should return immediately, and `false`
/// in the child, which should run the test body. `test_path` is the full test
/// path as `--exact` matches it.
pub(crate) fn rerun_in_isolated_child(child_var: &str, test_path: &str) -> bool {
    if std::env::var_os(child_var).is_some() {
        return false;
    }
    let directory = tempfile::tempdir().expect("temporary store for the isolated child");
    let mut command = std::process::Command::new(
        std::env::current_exe().expect("locate the running test binary"),
    );
    command
        .args(["--exact", test_path, "--nocapture"])
        .env(child_var, "1")
        .env("MJ_DATA_DIR", directory.path())
        .env("MJ_CONFIG_DIR", directory.path());
    let output =
        mj_core::subprocess::run_with_input(&mut command, &[]).expect("run the isolated child");
    assert!(
        output.status.success(),
        "isolated run of {test_path} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}
