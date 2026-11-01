//! Where the controller runs a coding harness of its own accord.
//!
//! Sessions run their harness on their target. The controller also needs a
//! harness for work that belongs to no session: signing a profile in, minting
//! a Claude setup token, discovering a profile's models before a session
//! starts, and reading or renewing a subscription login for the quota view.
//! None of that runs on this machine. It runs in a short-lived container from
//! the agent image of a local Podman or Docker target, which already carries
//! the pinned harnesses, so the host needs no harness, Node.js, or harness
//! cache of its own.
//!
//! [`HarnessSandbox::select`] is the one decision of which engine and image
//! that is. Every caller asks it; none chooses an engine itself.
//!
//! Files the harness must read or keep (a profile home, a probe directory)
//! are bind-mounted at fixed container paths, so nothing depends on the host's
//! path syntax. What the container writes into a mount belongs to the person
//! running Mjolnir: rootless Podman maps container root to them, and Docker
//! runs the harness as their own user id unless the daemon is itself rootless.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use mj_core::config::{Config, ContainerTemplate, HarnessKind, ImagePullPolicy, TargetTemplate};

use crate::targets::{CommandExecutor, CommandSpec};

/// Label on every sandbox container, valued with the instance identity, so a
/// person (or a later cleanup) can tell them from session containers.
pub const SANDBOX_LABEL: &str = "org.brokk.mjolnir.harness-sandbox";

/// The container's own home: a persistent host directory, so a harness the
/// image does not carry (Kimi, Grok, OpenCode, DSH's npx package) installs
/// once inside the sandbox rather than at every start.
pub const SANDBOX_HOME: &str = "/mj/home";
/// Where a profile home is mounted. The harness's home variable points here.
pub const SANDBOX_PROFILE_ROOT: &str = "/mj/profile";
/// Where a caller's private working directory is mounted.
pub const SANDBOX_WORK: &str = "/mj/work";
/// Where the controller's own Linux worker is mounted, read-only.
pub const SANDBOX_WORKER: &str = "/mj/bin/mj-worker";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxEngine {
    Podman,
    Docker,
}

impl SandboxEngine {
    pub const fn command(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Docker => "docker",
        }
    }
}

/// Which user the harness runs as inside the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SandboxUser {
    /// Container root, which the engine maps to the person running Mjolnir
    /// (rootless Podman, rootless Docker).
    MappedRoot,
    /// The person's own ids, for a Docker daemon that runs as root.
    Host { uid: u32, gid: u32 },
}

/// The engine and image controller-side harness work runs in.
#[derive(Debug, Clone)]
pub struct HarnessSandbox {
    engine: SandboxEngine,
    target_id: String,
    image: String,
    pull_policy: ImagePullPolicy,
    platform: Option<String>,
    user: SandboxUser,
    home: PathBuf,
}

/// A host directory or file visible inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxMount {
    pub source: PathBuf,
    pub destination: String,
    pub writable: bool,
}

impl SandboxMount {
    pub fn writable(source: impl Into<PathBuf>, destination: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            destination: destination.into(),
            writable: true,
        }
    }

    pub fn read_only(source: impl Into<PathBuf>, destination: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            destination: destination.into(),
            writable: false,
        }
    }
}

/// How the sandbox reaches the network.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SandboxNetwork {
    /// The engine's default network: outbound access only.
    #[default]
    Outbound,
    /// A sign-in whose browser returns to `localhost:<port>` must reach the
    /// harness listening in the container. On Linux the container shares the
    /// host's network, which covers any port; elsewhere the engine runs in a
    /// virtual machine, so the known callback ports are published instead.
    BrowserCallbacks(Vec<u16>),
}

/// One harness command to run in the sandbox.
#[derive(Debug, Clone, Default)]
pub struct SandboxRun {
    pub purpose: String,
    pub mounts: Vec<SandboxMount>,
    pub environment: BTreeMap<String, String>,
    pub command: Vec<String>,
    /// Keep standard input open (`-i`): ACP and app-server talk over it, and
    /// closing it is how they learn their client is gone.
    pub stdin: bool,
    /// Allocate a terminal (`-t`) for an interactive sign-in.
    pub terminal: bool,
    pub network: SandboxNetwork,
    pub working_directory: Option<String>,
}

/// A prepared `podman run` or `docker run`, with the container's name so the
/// caller can remove it on every exit path.
#[derive(Debug, Clone)]
pub struct SandboxCommand {
    pub engine: SandboxEngine,
    pub program: String,
    pub args: Vec<String>,
    /// Values for the container's environment. The arguments name each
    /// variable (`--env NAME`) and the engine client reads its value from its
    /// own environment, so a token never appears in the process list.
    pub environment: BTreeMap<String, String>,
    pub name: String,
}

impl SandboxCommand {
    pub fn spec(&self, purpose: &str) -> CommandSpec {
        let mut spec =
            CommandSpec::new(self.program.clone(), self.args.clone()).purpose(purpose.to_owned());
        spec.env = self.environment.clone();
        spec
    }

    /// The engine client as a process to drive over its standard streams.
    pub fn process(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command.args(&self.args).envs(&self.environment);
        command
    }

    /// Remove the container. `--rm` normally does this when the harness
    /// exits, but a client that was killed does not wait for that, and a
    /// container must never outlive the work that started it.
    pub fn remove(&self, executor: &impl CommandExecutor) {
        let mut args = vec!["rm", "--force"];
        if self.engine == SandboxEngine::Podman {
            args.push("--ignore");
        }
        args.push(&self.name);
        let command = CommandSpec::new(self.engine.command(), args)
            .purpose("remove harness sandbox container");
        match executor.execute(&command) {
            Ok(output) if output.status == 0 => {}
            Ok(output) => {
                let reported = String::from_utf8_lossy(&output.stderr);
                if !reported.contains("No such container")
                    && !reported.contains("no such container")
                {
                    tracing::warn!(container = %self.name, status = output.status, reported = %reported.trim(), "could not remove harness sandbox container");
                }
            }
            Err(error) => {
                tracing::warn!(container = %self.name, error = %format!("{error:#}"), "could not remove harness sandbox container");
            }
        }
    }
}

/// Why no sandbox can run on this machine, with the step that fixes it.
#[derive(Debug)]
pub struct SandboxUnavailable(String);

impl std::fmt::Display for SandboxUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SandboxUnavailable {}

impl SandboxUnavailable {
    #[cfg(test)]
    pub(crate) fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

/// Whether `error` says this machine has no usable container engine.
pub fn is_unavailable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<SandboxUnavailable>())
}

impl HarnessSandbox {
    /// Choose the engine and image. Podman comes first, as it does for new
    /// sessions; a target the person disabled is never used. The image is the
    /// target's own, so the sandbox runs the same harness versions its
    /// sessions run.
    pub fn select(config: &Config, executor: &impl CommandExecutor) -> Result<Self> {
        let path = std::env::var_os("PATH");
        let mut candidates = Vec::new();
        for (id, template) in &config.targets {
            if config.disabled_targets.contains(id) {
                continue;
            }
            let (engine, container) = match template {
                TargetTemplate::LocalPodman { container } => (SandboxEngine::Podman, container),
                TargetTemplate::LocalDocker { container } => (SandboxEngine::Docker, container),
                _ => continue,
            };
            if !crate::targets::program_on_path(engine.command(), path.as_deref()) {
                continue;
            }
            candidates.push((engine, id, container));
        }
        candidates.sort_by_key(|(engine, id, _)| (*engine != SandboxEngine::Podman, (*id).clone()));
        let mut problems = Vec::new();
        for (engine, id, container) in candidates {
            match Self::ready(engine, id, container, executor) {
                Ok(sandbox) => return Ok(sandbox),
                Err(error) => problems.push(format!("{id}: {error:#}")),
            }
        }
        let detail = if problems.is_empty() {
            "no local Podman or Docker target is enabled and installed".to_owned()
        } else {
            problems.join("; ")
        };
        Err(SandboxUnavailable(format!(
            "Mjolnir runs coding agents only in containers, and this machine has no container engine it can use ({detail}). Install Podman or Docker, then run `mj doctor` to check it."
        ))
        .into())
    }

    fn ready(
        engine: SandboxEngine,
        id: &str,
        container: &ContainerTemplate,
        executor: &impl CommandExecutor,
    ) -> Result<Self> {
        let user = match engine {
            SandboxEngine::Podman => {
                crate::targets::verify_local_podman(executor)?;
                SandboxUser::MappedRoot
            }
            SandboxEngine::Docker => {
                crate::targets::verify_local_docker(executor)?;
                docker_user(executor)?
            }
        };
        let home = mj_core::config::data_dir()
            .join("harness-sandbox")
            .join(engine.command());
        create_private_directory(&home)?;
        sweep_abandoned_work(&home.with_file_name("work"));
        let sandbox = Self {
            engine,
            target_id: id.to_owned(),
            image: container.image.clone(),
            pull_policy: container.pull_policy,
            platform: container.platform.clone(),
            user,
            home,
        };
        if let SandboxUser::Host { uid, gid } = sandbox.user {
            sandbox.write_account_files(uid, gid)?;
        }
        Ok(sandbox)
    }

    pub fn engine(&self) -> SandboxEngine {
        self.engine
    }

    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    pub fn image(&self) -> &str {
        &self.image
    }

    /// The Linux architecture of the sandbox's containers, for choosing the
    /// worker binary to mount. Local engines run the host's architecture
    /// unless the target pins a platform.
    pub fn architecture(&self) -> Result<&'static str> {
        let requested = self
            .platform
            .as_deref()
            .and_then(|platform| platform.split('/').nth(1))
            .unwrap_or(std::env::consts::ARCH);
        crate::targets::normalize_architecture(requested)
    }

    /// Build the `run` command for `run`.
    pub fn command(&self, run: &SandboxRun) -> Result<SandboxCommand> {
        let name = sandbox_container_name();
        let mut args = vec!["run".to_owned(), "--rm".to_owned(), "--init".to_owned()];
        match self.engine {
            SandboxEngine::Podman => {
                let policy = self.pull_policy.at_launch(&self.image);
                if policy != ImagePullPolicy::Missing {
                    args.push(format!("--pull={}", policy.podman_value()));
                }
                // Host bind mounts keep the host's SELinux label, which a
                // container process is denied; session containers drop the
                // same layer for the same reason.
                args.extend(["--security-opt".to_owned(), "label=disable".to_owned()]);
            }
            SandboxEngine::Docker => {
                let pull = match self.pull_policy.at_launch(&self.image) {
                    ImagePullPolicy::Always | ImagePullPolicy::Newer => "always",
                    ImagePullPolicy::Never => "never",
                    _ => "missing",
                };
                args.push(format!("--pull={pull}"));
            }
        }
        if let Some(platform) = &self.platform {
            args.push(format!("--platform={platform}"));
        }
        args.extend(["--name".to_owned(), name.clone()]);
        args.extend([
            "--label".to_owned(),
            format!("{SANDBOX_LABEL}={}", mj_core::config::instance_identity()),
        ]);
        if run.stdin {
            args.push("--interactive".to_owned());
        }
        if run.terminal {
            args.push("--tty".to_owned());
        }
        match &run.network {
            SandboxNetwork::Outbound => {}
            SandboxNetwork::BrowserCallbacks(ports) => {
                if cfg!(target_os = "linux") {
                    args.push("--network=host".to_owned());
                } else {
                    for port in ports {
                        args.extend(["--publish".to_owned(), format!("127.0.0.1:{port}:{port}")]);
                    }
                }
            }
        }
        match self.user {
            SandboxUser::MappedRoot => args.extend(["--user".to_owned(), "0:0".to_owned()]),
            SandboxUser::Host { uid, gid } => {
                args.extend(["--user".to_owned(), format!("{uid}:{gid}")]);
                // Node and the harness CLIs look the user up by id; a Docker
                // daemon running as root has no entry for the person's id.
                for file in ["passwd", "group"] {
                    args.extend(volume(
                        &self
                            .home
                            .with_file_name(format!("{}-{file}", self.engine.command())),
                        &format!("/etc/{file}"),
                        false,
                    ));
                }
            }
        }
        args.extend(volume(&self.home, SANDBOX_HOME, true));
        let mut destinations = vec![SANDBOX_HOME.to_owned()];
        for mount in &run.mounts {
            if destinations.contains(&mount.destination) {
                bail!("the harness sandbox mounts {} twice", mount.destination);
            }
            destinations.push(mount.destination.clone());
            args.extend(volume(&mount.source, &mount.destination, mount.writable));
        }
        let mut environment = home_environment();
        environment.extend(run.environment.clone());
        for name in environment.keys() {
            args.extend(["--env".to_owned(), name.clone()]);
        }
        if let Some(directory) = &run.working_directory {
            args.extend(["--workdir".to_owned(), directory.clone()]);
        }
        args.push(self.image.clone());
        args.extend(run.command.iter().cloned());
        Ok(SandboxCommand {
            engine: self.engine,
            program: self.engine.command().to_owned(),
            args,
            environment,
            name,
        })
    }

    /// A run of `kind`'s own CLI with `arguments`, its profile home mounted
    /// where the harness expects it. The worker inside prepares the harness
    /// the way a container session does, so an image without it still works.
    /// `environment` names the home with the mount point
    /// ([`profile_environment`]); it may carry secrets, which travel only in
    /// the engine client's environment.
    pub fn harness_cli(
        &self,
        kind: HarnessKind,
        home: &Path,
        environment: BTreeMap<String, String>,
        arguments: Vec<String>,
        purpose: &str,
    ) -> Result<SandboxRun> {
        let worker = crate::controller::portable_linux_worker(self.architecture()?)?;
        let mut environment = environment;
        environment.extend(home_environment());
        let mut command = vec![
            SANDBOX_WORKER.to_owned(),
            "worker".to_owned(),
            "harness-cli".to_owned(),
            "--harness".to_owned(),
            kind.id().to_owned(),
        ];
        for name in environment.keys() {
            command.extend(["--keep-env".to_owned(), name.clone()]);
        }
        command.push("--".to_owned());
        command.extend(arguments);
        Ok(SandboxRun {
            purpose: purpose.to_owned(),
            mounts: vec![
                SandboxMount::writable(
                    home,
                    profile_mount_point(kind).to_string_lossy().into_owned(),
                ),
                SandboxMount::read_only(worker, SANDBOX_WORKER),
            ],
            environment,
            command,
            working_directory: Some(SANDBOX_HOME.to_owned()),
            ..Default::default()
        })
    }

    fn write_account_files(&self, uid: u32, gid: u32) -> Result<()> {
        let passwd = format!(
            "root:x:0:0:root:/root:/bin/sh\nmj:x:{uid}:{gid}:Mjolnir:{SANDBOX_HOME}:/bin/sh\n"
        );
        let group = format!("root:x:0:\nmj:x:{gid}:\n");
        for (file, contents) in [("passwd", passwd), ("group", group)] {
            let path = self
                .home
                .with_file_name(format!("{}-{file}", self.engine.command()));
            if std::fs::read_to_string(&path).ok().as_deref() != Some(contents.as_str()) {
                mj_core::config::atomic_write(&path, contents.as_bytes())
                    .with_context(|| format!("write {}", path.display()))?;
            }
        }
        Ok(())
    }
}

/// The sandbox's own home. A worker in the sandbox rebuilds its environment
/// from a login shell, which takes `HOME` from the account database, so its
/// specs carry these as overrides too.
pub fn home_environment() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("HOME".to_owned(), SANDBOX_HOME.to_owned()),
        (
            "XDG_CACHE_HOME".to_owned(),
            format!("{SANDBOX_HOME}/.cache"),
        ),
    ])
}

/// Runs each harness command it is given in the sandbox, with one profile
/// home mounted where the harness expects it. Code that builds a harness
/// command as if the harness were local (a quota probe, a login renewal)
/// takes this executor, so the command it builds is the whole description
/// and its tests can still run it against a fake directly.
///
/// An environment value naming the home is rewritten to the mount point; no
/// other path is translated.
pub struct SandboxExecutor<E> {
    inner: E,
    kind: HarnessKind,
    home: PathBuf,
}

impl<E: CommandExecutor> SandboxExecutor<E> {
    pub fn new(inner: E, kind: HarnessKind, home: PathBuf) -> Self {
        Self { inner, kind, home }
    }
}

impl<E: CommandExecutor> CommandExecutor for SandboxExecutor<E> {
    fn execute(&self, command: &CommandSpec) -> Result<crate::targets::CommandOutput> {
        let config = Config::load()?;
        let sandbox = HarnessSandbox::select(&config, &self.inner)?;
        let mount = profile_mount_point(self.kind)
            .to_string_lossy()
            .into_owned();
        let home = self.home.to_string_lossy();
        let environment = command
            .env
            .iter()
            .map(|(name, value)| {
                let value = if *value == home {
                    mount.clone()
                } else {
                    value.clone()
                };
                (name.clone(), value)
            })
            .collect();
        // The command names the harness CLI; the worker in the sandbox finds
        // or installs it.
        let run = sandbox.command(&sandbox.harness_cli(
            self.kind,
            &self.home,
            environment,
            command.args.clone(),
            &command.purpose,
        )?)?;
        let mut spec = run.spec(&command.purpose);
        spec.sensitive_output = command.sensitive_output;
        let output = self.inner.execute(&spec);
        // Its own bounded executor: a cancelled command must still be cleaned up.
        run.remove(&crate::targets::CancellableProcessExecutor::with_timeout(
            std::time::Duration::from_secs(30),
        ));
        output
    }

    fn cancellation_requested(&self) -> bool {
        self.inner.cancellation_requested()
    }
}

/// Where a profile home is mounted for `kind`: Muse and OpenCode read
/// `$XDG_CONFIG_HOME/<id>`, so their home keeps that leaf name.
pub fn profile_mount_point(kind: HarnessKind) -> PathBuf {
    kind.home_from_environment(Path::new(SANDBOX_PROFILE_ROOT))
}

/// The harness's home variables for a profile home mounted at
/// [`profile_mount_point`], as a Linux harness reads them.
pub fn profile_environment(kind: HarnessKind) -> BTreeMap<String, String> {
    let mut environment = BTreeMap::new();
    kind.configure_home_environment(&profile_mount_point(kind), &mut environment);
    environment
}

fn volume(source: &Path, destination: &str, writable: bool) -> [String; 2] {
    let mode = if writable { "rw" } else { "ro" };
    [
        "--volume".to_owned(),
        format!("{}:{destination}:{mode}", source.display()),
    ]
}

fn sandbox_container_name() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "mj-harness-{}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_millis(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// How long a work directory may outlive its sign-in or probe. A sign-in
/// waits for the person for at most ten minutes and a probe for five.
const ABANDONED_WORK_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Remove work directories whose owner stopped without cleaning up, such as
/// a probe interrupted by `mj daemon stop`. They hold staged credentials.
fn sweep_abandoned_work(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let abandoned = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > ABANDONED_WORK_AGE);
        if abandoned && let Err(error) = std::fs::remove_dir_all(entry.path()) {
            tracing::warn!(path = %entry.path().display(), %error, "could not remove abandoned harness sandbox work");
        }
    }
}

fn create_private_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("create harness sandbox home {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict harness sandbox home {}", path.display()))?;
    }
    Ok(())
}

/// A Docker daemon running as root writes bind-mounted files as whichever
/// user the container runs as, so the harness runs as the person's own ids.
/// A rootless daemon maps container root to them already. Docker Desktop's
/// file sharing gives files to the person either way; their own ids work
/// there too.
fn docker_user(executor: &impl CommandExecutor) -> Result<SandboxUser> {
    static ROOTLESS: OnceLock<bool> = OnceLock::new();
    let rootless = match ROOTLESS.get() {
        Some(rootless) => *rootless,
        None => {
            let command =
                CommandSpec::new("docker", ["info", "--format", "{{json .SecurityOptions}}"])
                    .purpose("check whether Docker is rootless");
            let output = executor.execute(&command)?;
            if output.status != 0 {
                bail!(
                    "`docker info` exited with status {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            let rootless = String::from_utf8_lossy(&output.stdout).contains("name=rootless");
            *ROOTLESS.get_or_init(|| rootless)
        }
    };
    if rootless {
        return Ok(SandboxUser::MappedRoot);
    }
    #[cfg(unix)]
    {
        // SAFETY: getuid and getgid cannot fail and touch no memory.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Ok(SandboxUser::Host { uid, gid })
    }
    #[cfg(not(unix))]
    {
        Ok(SandboxUser::MappedRoot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(engine: SandboxEngine, user: SandboxUser) -> HarnessSandbox {
        HarnessSandbox {
            engine,
            target_id: engine.command().to_owned(),
            image: "ghcr.io/brokkai/mjolnir/agent-dev:test".to_owned(),
            pull_policy: ImagePullPolicy::Missing,
            platform: None,
            user,
            home: PathBuf::from("/data/harness-sandbox").join(engine.command()),
        }
    }

    /// A sandbox writes into the person's profile home. Under a Docker daemon
    /// that runs as root, a container left at its image user would leave
    /// those files to another user; it must run as the person's own ids, with
    /// an account entry Node can look up.
    #[test]
    fn docker_under_a_root_daemon_writes_profile_files_as_the_person() {
        let command = sandbox(
            SandboxEngine::Docker,
            SandboxUser::Host { uid: 1234, gid: 99 },
        )
        .command(&SandboxRun {
            mounts: vec![SandboxMount::writable(
                "/home/me/.codex",
                profile_mount_point(HarnessKind::Codex)
                    .display()
                    .to_string(),
            )],
            command: vec!["codex".into(), "login".into()],
            ..Default::default()
        })
        .unwrap();
        let args = command.args.join(" ");
        assert!(args.contains("--user 1234:99"), "{args}");
        assert!(
            args.contains("--volume /data/harness-sandbox/docker-passwd:/etc/passwd:ro"),
            "{args}"
        );
        assert!(
            args.contains("--volume /home/me/.codex:/mj/profile:rw"),
            "{args}"
        );
        assert!(args.ends_with("agent-dev:test codex login"), "{args}");
    }
}
