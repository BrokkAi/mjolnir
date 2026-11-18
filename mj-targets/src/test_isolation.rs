//! Running one test alone in a child of the test binary, for tests that need
//! process-global state (the data directory, the database writer, the working
//! directory) to themselves.

use std::path::Path;
use std::process::Command;

/// The `--exact` name of a test in this binary, given its `module_path!()`.
///
/// `module_path!()` carries the crate name, which libtest's filter does not.
/// mj-daemon's tests use this too, so any crate name is stripped.
pub fn test_name(module_path: &str, test: &str) -> String {
    let module = module_path
        .split_once("::")
        .map_or(module_path, |(_, module)| module);
    format!("{module}::{test}")
}

/// Re-run one test alone, in a child of this test binary.
///
/// The data directory, the installed database writer, the tracing subscriber
/// and the process working directory are all process-global, so a test that
/// needs its own has to be the only test in its process. The child runs the
/// named test with `--exact`, and [`IsolatedTest::run`] fails the parent with
/// the child's own output when it does not pass.
///
/// Build the name with [`test_name`], which strips the crate prefix that
/// `module_path!()` carries and libtest's filter does not accept.
pub struct IsolatedTest {
    name: String,
    command: Command,
}

impl IsolatedTest {
    /// A child runs one whole flow on one test thread. An in-place Move, for
    /// example, polls the checkpoint and provisioning chain inline, whose
    /// debug-build frames are tens of kilobytes each; at the 8 MiB default
    /// that sits within a few pages of the limit, so an unrelated change can
    /// overflow a child that passed before. Give the child room instead of
    /// leaving every isolated test to the edge of its stack.
    const CHILD_STACK_BYTES: &'static str = "33554432";

    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        let mut command = Command::new(std::env::current_exe().expect("this test binary"));
        command
            .args(["--exact", &name, "--nocapture"])
            .env("RUST_MIN_STACK", Self::CHILD_STACK_BYTES)
            // The instance name selects the default viewer port and the
            // store layout, so a child owns it: an inherited one changes
            // output the child's goldens pin. A test that needs an instance
            // sets it with `env`.
            .env_remove("MJ_INSTANCE");
        // Control commands run a staged current worker, so a child that
        // exercises them needs a worker source. A test names its own with
        // `env`, which replaces this one.
        #[cfg(unix)]
        command.env(
            "MJ_WORKER_BINARY",
            mj_core::test_hooks::fake_worker_dispatcher(),
        );
        Self { name, command }
    }

    /// Set a variable for the child. The marker a test uses to tell the child
    /// apart from the parent goes here too.
    pub fn env(
        mut self,
        key: impl AsRef<std::ffi::OsStr>,
        value: impl AsRef<std::ffi::OsStr>,
    ) -> Self {
        self.command.env(key, value);
        self
    }

    /// Remove a variable from the child, such as the default worker source.
    pub fn env_remove(mut self, key: impl AsRef<std::ffi::OsStr>) -> Self {
        self.command.env_remove(key);
        self
    }

    /// Include an ignored integration case in the isolated child process.
    #[cfg(unix)]
    pub fn include_ignored(mut self) -> Self {
        self.command.arg("--ignored");
        self
    }

    /// Give the child its own configuration and data directories under `root`.
    pub fn isolated_store(self, root: &Path) -> Self {
        self.env("MJ_DATA_DIR", root.join("data"))
            .env("MJ_CONFIG_DIR", root.join("config"))
    }

    /// Run the child and return its output without judging it.
    pub fn output(mut self) -> std::process::Output {
        self.command
            .output()
            .unwrap_or_else(|error| panic!("run isolated {}: {error}", self.name))
    }

    /// Run the child and fail this test with its output if it did not pass.
    pub fn run(self) -> std::process::Output {
        let name = self.name.clone();
        let output = self.output();
        assert!(
            output.status.success(),
            "isolated {name} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}
