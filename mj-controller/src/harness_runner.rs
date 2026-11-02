//! Where Mjolnir runs a coding harness for work that belongs to no session.
//!
//! Sessions run their harness on their target. Signing a profile in, minting
//! a Claude setup token, discovering a profile's models before a session
//! starts, and reading or renewing a subscription login for the quota view
//! also need a harness. Each of those is one fixed procedure (prepare a work
//! directory, write a spec, run `mj-worker`, clean up) whose steps depend on
//! where the harness runs. [`HarnessRunner`] supplies those steps:
//!
//! - [`LocalRunner`] runs the worker on this machine with Mjolnir's pinned
//!   harness installation, the behavior a bare session has.
//! - [`crate::harness_sandbox::HarnessSandbox`] runs it in a short-lived
//!   container from a local Podman or Docker target's agent image, so this
//!   machine needs no harness, Node.js, or harness cache of its own.
//!
//! [`select`] is the one decision of which runner applies, from
//! `[harnesses] run_on`. The procedures ([`harness_cli`], [`HarnessExecutor`],
//! profile discovery, `mj login`) ask a runner for each step and never test
//! which one they hold.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use mj_core::config::{Config, HarnessHostChoice, HarnessKind, HarnessProfile};
use mj_core::worker_launch::HarnessRuntimePolicy;

use crate::targets::{CommandExecutor, CommandSpec};

/// How the caller talks to the worker it launches.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum WorkerIo {
    /// Output is collected and returned; standard input is closed.
    #[default]
    Captured,
    /// The caller speaks a protocol over standard input and output.
    Stream,
    /// The person's terminal is attached for an interactive sign-in whose
    /// browser may return to `localhost` on these ports.
    Terminal { callback_ports: Vec<u16> },
}

/// One run of `mj-worker` on behalf of a procedure.
#[derive(Debug, Clone, Default)]
pub struct WorkerRun {
    pub purpose: String,
    /// The canonical profile home the harness reads and may update.
    pub profile: Option<(HarnessKind, PathBuf)>,
    /// A private work directory on this machine the worker reads and writes.
    /// Paths into it in `arguments` come from [`HarnessRunner::work_view`].
    pub work: Option<PathBuf>,
    /// The worker's arguments, after the program.
    pub arguments: Vec<String>,
    /// Variables the worker process receives. Values may be secrets: no
    /// runner puts them on a command line.
    pub environment: BTreeMap<String, String>,
    pub io: WorkerIo,
}

/// A worker ready to start, and what must be undone after it exits.
pub struct WorkerLaunch {
    pub program: String,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    cleanup: Option<Box<dyn FnOnce() + Send>>,
}

impl WorkerLaunch {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            environment: BTreeMap::new(),
            cwd: None,
            cleanup: None,
        }
    }

    pub fn with_cleanup(mut self, cleanup: impl FnOnce() + Send + 'static) -> Self {
        self.cleanup = Some(Box::new(cleanup));
        self
    }

    /// The launch as a command for a [`CommandExecutor`].
    pub fn spec(&self, purpose: &str) -> CommandSpec {
        let mut spec =
            CommandSpec::new(self.program.clone(), self.args.clone()).purpose(purpose.to_owned());
        spec.env = self.environment.clone();
        spec.cwd = self.cwd.clone();
        spec
    }

    /// The launch as a process to drive over its standard streams.
    pub fn process(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command.args(&self.args).envs(&self.environment);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        command
    }

    /// Undo what the launch set up (a container, a browser relay). Call it on
    /// every exit path once the worker has stopped; it may block briefly.
    pub fn finish(mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

impl Drop for WorkerLaunch {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            tracing::warn!(program = %self.program, "a harness launch was dropped without being finished; cleaning up now");
            cleanup();
        }
    }
}

/// The steps that differ between running a harness here and in a container.
pub trait HarnessRunner: Send + Sync {
    /// Which installation the worker selects: Mjolnir's pinned one here, the
    /// image's in a container.
    fn runtime(&self) -> HarnessRuntimePolicy;

    /// Where private work directories for this runner are created on this
    /// machine.
    fn work_root(&self) -> Result<PathBuf>;

    /// How the worker sees a work directory created under [`Self::work_root`].
    fn work_view(&self, work: &Path) -> PathBuf;

    /// How the harness sees `kind`'s canonical profile home.
    fn profile_view(&self, kind: HarnessKind, home: &Path) -> PathBuf;

    /// The variables that name the profile home, as the harness there reads
    /// it.
    fn profile_environment(&self, kind: HarnessKind, home: &Path) -> BTreeMap<String, String>;

    /// Variables every worker of this runner needs on top of the account's
    /// login environment.
    fn base_environment(&self) -> BTreeMap<String, String>;

    /// Whether `mj login` may run against a profile whose login database
    /// belongs to the person's own installation. `Err` says why not.
    fn may_sign_in_foreign_store(&self, profile: &HarnessProfile) -> Result<()>;

    /// Prepare `run` to start.
    fn launch_worker(&self, run: WorkerRun) -> Result<WorkerLaunch>;

    /// Whether the harness this runner starts reads this machine's macOS
    /// Keychain. Where it does not, a Keychain login is handed to it as a
    /// private copy.
    fn reads_host_keychain(&self) -> bool;

    /// A short phrase for messages: "on this machine", "in a docker container
    /// from <image>".
    fn describe(&self) -> String;
}

/// The runner `config` selects. A machine that runs no worker of its own
/// (Windows) cannot run a harness here, so it uses a container even when the
/// configuration says `host`, and says so once.
pub fn select(config: &Config, executor: &impl CommandExecutor) -> Result<Arc<dyn HarnessRunner>> {
    match config.harnesses.run_on {
        HarnessHostChoice::Host if mj_core::targets::HOST_RUNS_WORKERS => Ok(Arc::new(LocalRunner)),
        HarnessHostChoice::Host => {
            static NOTED: std::sync::Once = std::sync::Once::new();
            NOTED.call_once(|| {
                tracing::info!(
                    "this machine runs no Mjolnir worker, so harness work runs in a container"
                );
            });
            Ok(Arc::new(crate::harness_sandbox::HarnessSandbox::select(
                config, executor,
            )?))
        }
        HarnessHostChoice::Container => Ok(Arc::new(
            crate::harness_sandbox::HarnessSandbox::select(config, executor)?,
        )),
    }
}

/// Run `kind`'s own CLI with `arguments` through `runner`, as a procedure
/// that drives a harness command (a quota probe, a login renewal) does.
/// `environment` may name the canonical home; those values are rewritten to
/// where the runner shows it. Variables in `drop_environment` never reach the
/// harness, even from the account's login shell.
// Each argument is an independent input: where, which harness and home, what
// it receives and runs, and how the caller talks to it.
#[allow(clippy::too_many_arguments)]
pub fn harness_cli(
    runner: &dyn HarnessRunner,
    kind: HarnessKind,
    home: &Path,
    environment: BTreeMap<String, String>,
    drop_environment: &[&str],
    arguments: Vec<String>,
    io: WorkerIo,
    purpose: &str,
) -> Result<WorkerLaunch> {
    let host_home = home.to_string_lossy();
    let view = runner
        .profile_view(kind, home)
        .to_string_lossy()
        .into_owned();
    let mut variables = runner.base_environment();
    variables.extend(environment.into_iter().map(|(name, value)| {
        let value = if value == host_home {
            view.clone()
        } else {
            value
        };
        (name, value)
    }));
    variables.extend(runner.profile_environment(kind, home));
    for name in drop_environment {
        variables.remove(*name);
    }
    let mut command = vec![
        "worker".to_owned(),
        "harness-cli".to_owned(),
        "--harness".to_owned(),
        kind.id().to_owned(),
        "--runtime".to_owned(),
        match runner.runtime() {
            HarnessRuntimePolicy::Managed => "managed",
            HarnessRuntimePolicy::Ambient => "ambient",
        }
        .to_owned(),
    ];
    for name in variables.keys() {
        command.extend(["--keep-env".to_owned(), name.clone()]);
    }
    for name in drop_environment {
        command.extend(["--drop-env".to_owned(), (*name).to_owned()]);
    }
    command.push("--".to_owned());
    command.extend(arguments);
    runner.launch_worker(WorkerRun {
        purpose: purpose.to_owned(),
        profile: Some((kind, home.to_path_buf())),
        work: None,
        arguments: command,
        environment: variables,
        io,
    })
}

/// Runs each harness command it is given through the selected runner, with
/// one profile home. Code that builds a harness command as if the harness
/// were installed here (a Claude quota probe or login renewal) takes this
/// executor, so the command stays the whole description and its tests can
/// still run it against a fake directly.
pub struct HarnessExecutor<E> {
    inner: E,
    runner: Arc<dyn HarnessRunner>,
    kind: HarnessKind,
    home: PathBuf,
}

impl<E: CommandExecutor> HarnessExecutor<E> {
    pub fn new(inner: E, runner: Arc<dyn HarnessRunner>, kind: HarnessKind, home: PathBuf) -> Self {
        Self {
            inner,
            runner,
            kind,
            home,
        }
    }
}

impl<E: CommandExecutor> CommandExecutor for HarnessExecutor<E> {
    fn execute(&self, command: &CommandSpec) -> Result<crate::targets::CommandOutput> {
        let launch = harness_cli(
            self.runner.as_ref(),
            self.kind,
            &self.home,
            command.env.clone(),
            &[],
            command.args.clone(),
            WorkerIo::Captured,
            &command.purpose,
        )?;
        let mut spec = launch.spec(&command.purpose);
        spec.sensitive_output = command.sensitive_output;
        let output = self.inner.execute(&spec);
        launch.finish();
        output
    }

    fn cancellation_requested(&self) -> bool {
        self.inner.cancellation_requested()
    }
}

/// Runs the worker on this machine with Mjolnir's pinned harness
/// installation, exactly as a bare session does.
pub struct LocalRunner;

/// Variables a sign-in needs from the person's desktop session. The worker
/// rebuilds its environment from a login shell, which does not carry them.
const DESKTOP_ENVIRONMENT: [&str; 6] = [
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "BROWSER",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_RUNTIME_DIR",
];

impl HarnessRunner for LocalRunner {
    fn runtime(&self) -> HarnessRuntimePolicy {
        HarnessRuntimePolicy::Managed
    }

    fn work_root(&self) -> Result<PathBuf> {
        Ok(std::env::temp_dir())
    }

    fn work_view(&self, work: &Path) -> PathBuf {
        work.to_path_buf()
    }

    fn profile_view(&self, _kind: HarnessKind, home: &Path) -> PathBuf {
        home.to_path_buf()
    }

    fn profile_environment(&self, kind: HarnessKind, home: &Path) -> BTreeMap<String, String> {
        let mut environment = BTreeMap::new();
        kind.configure_profile_home_environment(
            home,
            mj_core::config::HarnessHost::current(),
            &mut environment,
        );
        environment
    }

    fn base_environment(&self) -> BTreeMap<String, String> {
        DESKTOP_ENVIRONMENT
            .iter()
            .filter_map(|name| Some(((*name).to_owned(), std::env::var(name).ok()?)))
            .collect()
    }

    fn may_sign_in_foreign_store(&self, _profile: &HarnessProfile) -> Result<()> {
        Ok(())
    }

    fn launch_worker(&self, run: WorkerRun) -> Result<WorkerLaunch> {
        let worker = crate::controller::native_worker_binary()?;
        let mut launch = WorkerLaunch::new(worker.to_string_lossy(), run.arguments);
        launch.environment = run.environment;
        launch.cwd = run.work;
        Ok(launch)
    }

    fn reads_host_keychain(&self) -> bool {
        true
    }

    fn describe(&self) -> String {
        "on this machine".to_owned()
    }
}

/// Create a private work directory for one procedure under `runner`'s root.
pub fn work_directory(runner: &dyn HarnessRunner, prefix: &str) -> Result<tempfile::TempDir> {
    let root = runner.work_root()?;
    std::fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    let work = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&root)
        .with_context(|| format!("create a private work directory in {}", root.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(work.path(), std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict {}", work.path().display()))?;
    }
    Ok(work)
}

/// Whether `profile` keeps its logins in a database the person's own
/// installation owns (their own OpenCode or Muse home, or MiMo's stock data).
pub fn uses_foreign_login_store(profile: &HarnessProfile) -> bool {
    let own_nested = profile.kind.nested_home() && profile.kind.is_own_nested_home(&profile.home);
    let own_data = !profile
        .kind
        .data_source(&profile.home)
        .starts_with(&profile.home);
    own_nested || own_data
}
