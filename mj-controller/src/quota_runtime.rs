//! Where a quota check runs the harness program it starts.
//!
//! Codex, Grok and Claude Code quota checks start the harness itself. With
//! `[quota] runtime` at its default, the daemon runs that program inside the
//! release's agent-dev image, which carries every harness at its pinned
//! version, so a host that runs its sessions in containers needs no harness of
//! its own. The profile's home is mounted at the same path, so the check reads
//! the person's login and a refreshed token lands where the host expects it.
//!
//! The named container, not the engine client, owns the check: killing
//! `podman run` leaves its container running. [`QuotaContainer`] removes it on
//! every exit path, and [`sweep_quota_containers`] removes any a stopped
//! daemon left behind.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use mj_core::config::QuotaRuntimeChoice;
use mj_core::targets::{
    BoundedProcessExecutor, CommandExecutor, CommandSpec, INSTANCE_LABEL, MANAGED_LABEL,
};

/// Marks a container as a quota check, so a later daemon can find leftovers.
pub(crate) const QUOTA_LABEL: &str = "dev.mj.quota";
/// Bounds the engine's own bookkeeping commands: image lookup and removal.
const ENGINE_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// The container's home and working directory. A tmpfs, so a check runs the
/// same as any user id and leaves nothing behind.
const CONTAINER_SCRATCH: &str = "/tmp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Engine {
    Podman,
    Docker,
}

impl Engine {
    pub(crate) fn program(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Docker => "docker",
        }
    }
}

/// The decision for one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QuotaRuntime {
    Host,
    Container(ContainerRuntime),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerRuntime {
    pub(crate) engine: Engine,
    /// The engine executable found on `PATH` when the check was decided.
    pub(crate) program: std::path::PathBuf,
    pub(crate) image: String,
}

impl QuotaRuntime {
    /// Decide where a check runs, from the configured choice and the engines
    /// on `path`. `Auto` without an engine is the host by design; an engine
    /// that was named but is missing is an error the check reports.
    pub(crate) fn resolve(choice: QuotaRuntimeChoice, path: Option<&OsStr>) -> Result<Self> {
        let installed = |engine: Engine| {
            mj_core::program_path::find_program_on_path(engine.program(), path)
                .map(|program| (engine, program))
        };
        let (engine, program) = match choice {
            QuotaRuntimeChoice::Host => return Ok(Self::Host),
            QuotaRuntimeChoice::Auto => {
                match [Engine::Podman, Engine::Docker]
                    .into_iter()
                    .find_map(installed)
                {
                    Some(found) => found,
                    None => return Ok(Self::Host),
                }
            }
            QuotaRuntimeChoice::Podman | QuotaRuntimeChoice::Docker => {
                let engine = if choice == QuotaRuntimeChoice::Podman {
                    Engine::Podman
                } else {
                    Engine::Docker
                };
                match installed(engine) {
                    Some(found) => found,
                    None => bail!(
                        "[quota] runtime is {0}, but {0} is not installed; install it or set runtime to auto or host",
                        engine.program()
                    ),
                }
            }
        };
        Ok(Self::Container(ContainerRuntime {
            engine,
            program,
            image: mj_core::config::DEFAULT_CONTAINER_IMAGE.to_owned(),
        }))
    }

    /// Resolve against this process's `PATH`.
    pub(crate) fn current(choice: QuotaRuntimeChoice) -> Result<Self> {
        Self::resolve(choice, std::env::var_os("PATH").as_deref())
    }
}

/// One harness program to run in the image.
pub(crate) struct ContainerLaunch {
    /// The engine program; the caller runs it with `args` and `env`.
    pub(crate) program: std::path::PathBuf,
    pub(crate) args: Vec<String>,
    /// The values behind every `--env NAME` in `args`. They reach the engine
    /// through its own environment, never its argument list, which every
    /// local user can read.
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) container: QuotaContainer,
}

impl ContainerRuntime {
    /// Fail unless the engine already holds the image. A check never pulls:
    /// the daemon's image refresher does, and a multi-gigabyte download would
    /// outlast any quota request.
    pub(crate) fn ensure_image(&self, executor: &impl CommandExecutor) -> Result<()> {
        let command = CommandSpec::new(
            mj_core::target_path::text(&self.program),
            [
                "image",
                "inspect",
                "--format",
                "{{.Id}}",
                self.image.as_str(),
            ],
        )
        .purpose("look up the agent-dev image for a quota check");
        let output = executor.execute(&command)?;
        if output.status != 0 {
            bail!(
                "the agent-dev image {} is not available to {} yet ({}); Mjolnir downloads it in the background",
                self.image,
                self.engine.program(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// [`Self::ensure_image`] off the async runtime, bounded.
    pub(crate) async fn ensure_image_async(&self) -> Result<()> {
        let runtime = self.clone();
        tokio::task::spawn_blocking(move || {
            runtime.ensure_image(&BoundedProcessExecutor::new(ENGINE_COMMAND_TIMEOUT))
        })
        .await
        .map_err(|error| anyhow::anyhow!("agent-dev image lookup task failed: {error}"))?
    }

    /// The engine invocation that runs `program args` in the image with
    /// `home` mounted read-write at its own path and `env` passed by name.
    /// `PATH` and `HOME` stay the image's own: a host value names host
    /// directories.
    pub(crate) fn launch(
        &self,
        home: &Path,
        program: &str,
        args: &[&str],
        env: &BTreeMap<String, String>,
    ) -> ContainerLaunch {
        let container = QuotaContainer::new(self.engine, self.program.clone());
        let home = mj_core::target_path::text(home);
        let mut command = vec![
            "run".to_owned(),
            "--rm".to_owned(),
            "--interactive".to_owned(),
            "--init".to_owned(),
            "--pull=never".to_owned(),
            "--name".to_owned(),
            container.name.clone(),
            "--label".to_owned(),
            format!("{MANAGED_LABEL}=true"),
            "--label".to_owned(),
            format!("{INSTANCE_LABEL}={}", mj_core::config::instance_identity()),
            "--label".to_owned(),
            format!("{QUOTA_LABEL}=true"),
        ];
        command.extend(self.user_args());
        command.extend([
            "--tmpfs".to_owned(),
            CONTAINER_SCRATCH.to_owned(),
            "--workdir".to_owned(),
            CONTAINER_SCRATCH.to_owned(),
            "--env".to_owned(),
            format!("HOME={CONTAINER_SCRATCH}"),
            "--volume".to_owned(),
            format!("{home}:{home}"),
        ]);
        let env = env
            .iter()
            .filter(|(name, _)| !matches!(name.as_str(), "PATH" | "HOME"))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>();
        for name in env.keys() {
            command.extend(["--env".to_owned(), name.clone()]);
        }
        command.push(self.image.clone());
        command.push(program.to_owned());
        command.extend(args.iter().map(|arg| (*arg).to_owned()));
        ContainerLaunch {
            program: self.program.clone(),
            args: command,
            env,
            container,
        }
    }

    /// Files the check writes into the mounted home must stay the host
    /// user's. Rootless Podman maps container root to that user, as its
    /// sessions rely on; Docker maps nothing, so the check runs as the
    /// host's own ids.
    fn user_args(&self) -> Vec<String> {
        match self.engine {
            Engine::Podman => vec![
                "--user".to_owned(),
                "0:0".to_owned(),
                "--security-opt".to_owned(),
                "label=disable".to_owned(),
            ],
            #[cfg(unix)]
            Engine::Docker => {
                // SAFETY: getuid and getgid only read this process's ids.
                let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
                vec!["--user".to_owned(), format!("{uid}:{gid}")]
            }
            #[cfg(not(unix))]
            Engine::Docker => Vec::new(),
        }
    }
}

/// A quota check's container, removed when this is dropped or removed.
pub(crate) struct QuotaContainer {
    engine: Engine,
    program: std::path::PathBuf,
    name: String,
    removed: bool,
}

impl QuotaContainer {
    fn new(engine: Engine, program: std::path::PathBuf) -> Self {
        let mut random = [0_u8; 8];
        if let Err(error) = getrandom::fill(&mut random) {
            // The name only needs to be unique among this host's quota checks;
            // the time and process id keep it so without the random part.
            tracing::warn!(%error, "could not read randomness for a quota container name");
            random =
                (std::process::id() as u64 ^ mj_core::clock::epoch_millis() as u64).to_le_bytes();
        }
        Self {
            engine,
            program,
            name: format!("mj-quota-{}", mj_core::hex::lower_hex(random)),
            removed: false,
        }
    }

    fn remove_command(&self) -> CommandSpec {
        // Docker has no `--ignore`; a container `--rm` already removed is the
        // ordinary case, which `remove_reports_failure` filters out.
        let mut args = vec!["rm", "--force"];
        if self.engine == Engine::Podman {
            args.push("--ignore");
        }
        args.push(self.name.as_str());
        CommandSpec::new(mj_core::target_path::text(&self.program), args)
            .purpose("remove a quota check container")
    }

    /// Remove the container now, off the async runtime.
    pub(crate) async fn remove(mut self) {
        self.removed = true;
        let command = self.remove_command();
        let name = self.name.clone();
        match tokio::task::spawn_blocking(move || remove_container(&command, &name)).await {
            Ok(()) => {}
            Err(error) => {
                tracing::warn!(%error, container = %self.name, "quota container removal task failed")
            }
        }
    }
}

impl Drop for QuotaContainer {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        // An abandoned check (a dropped future, a panic) still owes its
        // container. The removal runs on its own thread so a drop on the
        // async runtime never blocks it.
        let command = self.remove_command();
        let name = self.name.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("mj-quota-container-removal".to_owned())
            .spawn(move || remove_container(&command, &name))
        {
            tracing::warn!(%error, container = %self.name, "could not start quota container removal");
        }
    }
}

fn remove_container(command: &CommandSpec, name: &str) {
    match BoundedProcessExecutor::new(ENGINE_COMMAND_TIMEOUT).execute(command) {
        Ok(output) if output.status == 0 || !remove_reports_failure(&output.stderr) => {}
        Ok(output) => tracing::warn!(
            container = name,
            status = output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "could not remove a quota check container"
        ),
        Err(error) => {
            tracing::warn!(container = name, %error, "could not remove a quota check container")
        }
    }
}

/// Whether a failed `rm` means anything other than "already gone".
fn remove_reports_failure(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr).to_lowercase();
    !stderr.contains("no such container")
}

/// Remove every quota container this instance left behind, such as one whose
/// daemon stopped mid-check. Runs on the engines installed on this host.
pub(crate) fn sweep_quota_containers(executor: &impl CommandExecutor, path: Option<&OsStr>) {
    let instance = mj_core::config::instance_identity();
    for engine in [Engine::Podman, Engine::Docker] {
        let Some(program) = mj_core::program_path::find_program_on_path(engine.program(), path)
        else {
            continue;
        };
        let program = mj_core::target_path::text(&program);
        let list = CommandSpec::new(
            program.as_str(),
            [
                "ps".to_owned(),
                "--all".to_owned(),
                "--quiet".to_owned(),
                "--filter".to_owned(),
                format!("label={QUOTA_LABEL}=true"),
                "--filter".to_owned(),
                format!("label={INSTANCE_LABEL}={instance}"),
            ],
        )
        .purpose("list leftover quota check containers");
        let ids = match executor.execute(&list) {
            Ok(output) if output.status == 0 => String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            Ok(output) => {
                tracing::warn!(
                    engine = engine.program(),
                    stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                    "could not list leftover quota check containers"
                );
                continue;
            }
            Err(error) => {
                tracing::warn!(engine = engine.program(), %error, "could not list leftover quota check containers");
                continue;
            }
        };
        for id in ids {
            let command = CommandSpec::new(program.as_str(), ["rm", "--force", id.as_str()])
                .purpose("remove a leftover quota check container");
            remove_container(&command, &id);
        }
    }
}

/// One profile's check in a container: the image to run and the profile home
/// to mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerCheck {
    pub(crate) runtime: ContainerRuntime,
    pub(crate) home: std::path::PathBuf,
}

impl ContainerCheck {
    pub(crate) fn launch(
        &self,
        program: &str,
        args: &[&str],
        env: &BTreeMap<String, String>,
    ) -> ContainerLaunch {
        self.runtime.launch(&self.home, program, args, env)
    }

    /// A `tokio` command for `launch`, with stdio piped for a JSON-RPC
    /// exchange and the engine client killed when the command is dropped.
    /// The container itself goes with [`ContainerLaunch::container`].
    pub(crate) fn piped_command(launch: &ContainerLaunch) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&launch.program);
        command
            .args(&launch.args)
            .envs(&launch.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

/// Add the image quota checks run in to the daemon's background downloads,
/// unless a configured target already downloads it from the same engine.
pub(crate) fn add_quota_image(
    plan: &mut Vec<mj_core::targets::ImageRefresh>,
    choice: QuotaRuntimeChoice,
    path: Option<&OsStr>,
) {
    let Ok(QuotaRuntime::Container(runtime)) = QuotaRuntime::resolve(choice, path) else {
        return;
    };
    let host = match runtime.engine {
        Engine::Podman => mj_core::targets::ImageHost::LocalPodman,
        Engine::Docker => mj_core::targets::ImageHost::LocalDocker,
    };
    if plan
        .iter()
        .any(|entry| entry.host == host && entry.image == runtime.image && entry.platform.is_none())
    {
        return;
    }
    if let Some(refresh) = mj_core::targets::image_refresh(
        host,
        &runtime.image,
        None,
        mj_core::config::ImagePullPolicy::Auto,
    ) {
        plan.push(refresh);
    }
}
