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
//! daemon left behind. Each container names the process that started it, and
//! the sweep never removes one whose process is still running: every process
//! on a host that shares the data directory, such as a test run, has the same
//! instance identity as the daemon.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use crate::quota::QuotaRuntimeConfig;
use anyhow::{Result, bail};
use mj_core::config::QuotaRuntimeChoice;
use mj_core::targets::{
    BoundedProcessExecutor, CommandExecutor, CommandSpec, INSTANCE_LABEL, MANAGED_LABEL,
};

/// Marks a container as a quota check, so a later daemon can find leftovers.
pub(crate) const QUOTA_LABEL: &str = "dev.mj.quota";
/// Names the process that started a quota check: its process id and birth
/// identity, as [`owner_token`] writes them. The sweep removes a container
/// only when that process is gone.
pub(crate) const OWNER_LABEL: &str = "dev.mj.quota-owner";
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
    /// Decide where a check runs, from the configuration and the engines on
    /// `path`.
    ///
    /// A daemon in its own container runs every check beside itself: its
    /// image is the agent-dev image this module would otherwise start, and
    /// the container has no engine to start one with.
    ///
    /// `Auto` uses a container only for an enabled local Podman or Docker
    /// runtime whose engine is installed, and runs that runtime's image,
    /// which the daemon already keeps downloaded. With no such runtime the
    /// check runs on this host, by design. A named engine that is missing is
    /// an error the check reports.
    pub(crate) fn resolve(config: &QuotaRuntimeConfig, path: Option<&OsStr>) -> Result<Self> {
        if config.in_daemon_container {
            return Ok(Self::Host);
        }
        let installed =
            |engine: Engine| mj_core::program_path::find_program_on_path(engine.program(), path);
        let image = |engine: Engine| match engine {
            Engine::Podman => config.podman_image.clone(),
            Engine::Docker => config.docker_image.clone(),
        };
        let (engine, program, image) = match config.choice {
            QuotaRuntimeChoice::Host => return Ok(Self::Host),
            QuotaRuntimeChoice::Auto => {
                let found = [Engine::Podman, Engine::Docker]
                    .into_iter()
                    .find_map(|engine| Some((engine, installed(engine)?, image(engine)?)));
                match found {
                    Some(found) => found,
                    None => return Ok(Self::Host),
                }
            }
            QuotaRuntimeChoice::Podman | QuotaRuntimeChoice::Docker => {
                let engine = if config.choice == QuotaRuntimeChoice::Podman {
                    Engine::Podman
                } else {
                    Engine::Docker
                };
                let Some(program) = installed(engine) else {
                    bail!(
                        "[quota] runtime is {0}, but {0} is not installed; install it or set runtime to auto or host",
                        engine.program()
                    );
                };
                let image = image(engine)
                    .unwrap_or_else(|| mj_core::config::DEFAULT_CONTAINER_IMAGE.to_owned());
                (engine, program, image)
            }
        };
        Ok(Self::Container(ContainerRuntime {
            engine,
            program,
            image,
        }))
    }

    /// Resolve against this process's `PATH`.
    pub(crate) fn current(config: &QuotaRuntimeConfig) -> Result<Self> {
        Self::resolve(config, std::env::var_os("PATH").as_deref())
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
        if let Some(owner) = owner_token() {
            command.extend(["--label".to_owned(), format!("{OWNER_LABEL}={owner}")]);
        }
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
    remove_container_with(
        &BoundedProcessExecutor::new(ENGINE_COMMAND_TIMEOUT),
        command,
        name,
    )
}

fn remove_container_with(executor: &impl CommandExecutor, command: &CommandSpec, name: &str) {
    match executor.execute(command) {
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

/// This process as a container owner: `<pid>:<birth identity>`. The birth
/// identity keeps a reused process id from passing for this process. `None`
/// when the host cannot report it; a container without an owner is never
/// swept.
fn owner_token() -> Option<&'static str> {
    static OWNER: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    OWNER
        .get_or_init(|| {
            let pid = std::process::id();
            match mj_core::subprocess::process_birth_identity(pid) {
                Ok(birth) => Some(format!("{pid}:{birth}")),
                Err(error) => {
                    tracing::warn!(%error, "quota containers will not name their owner");
                    None
                }
            }
        })
        .as_deref()
}

/// Whether the process an owner label names is still running.
fn owner_is_running(pid: u32, birth: &str) -> bool {
    match mj_core::subprocess::process_birth_identity(pid) {
        Ok(current) => current == birth,
        // The identity could not be read. Only a process that does not exist
        // is certainly gone.
        Err(_) => mj_client::daemon::process_is_alive(pid),
    }
}

/// Whether the sweep may remove a container with this owner label.
///
/// Only a label that names a process which is no longer running qualifies. A
/// container with no label or an unreadable one is kept: it was started by a
/// build that did not record its owner, so a live daemon may be using it, and
/// a quota container can live as long as its daemon.
fn owner_is_gone(label: &str, running: impl Fn(u32, &str) -> bool) -> bool {
    let Some((pid, birth)) = label.trim().split_once(':') else {
        return false;
    };
    let Ok(pid) = pid.parse::<u32>() else {
        return false;
    };
    !running(pid, birth)
}

/// Remove every quota container this instance left behind, such as one whose
/// daemon stopped mid-check. Runs on the engines installed on this host.
///
/// Containers whose owning process is still running stay. The instance
/// identity comes from the data directory, so a second daemon, a CLI command
/// or a test run on the same host sweeps with the live daemon's identity.
pub(crate) fn sweep_quota_containers(executor: &impl CommandExecutor, path: Option<&OsStr>) {
    sweep_quota_containers_with(executor, path, &owner_is_running)
}

fn sweep_quota_containers_with(
    executor: &impl CommandExecutor,
    path: Option<&OsStr>,
    running: &dyn Fn(u32, &str) -> bool,
) {
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
            let inspect = CommandSpec::new(
                program.as_str(),
                [
                    "container",
                    "inspect",
                    "--format",
                    &format!("{{{{index .Config.Labels \"{OWNER_LABEL}\"}}}}"),
                    id.as_str(),
                ],
            )
            .purpose("read a quota check container's owner");
            let label = match executor.execute(&inspect) {
                Ok(output) if output.status == 0 => {
                    String::from_utf8_lossy(&output.stdout).trim().to_owned()
                }
                // Gone since the listing, or not readable: leave it.
                _ => continue,
            };
            if !owner_is_gone(&label, running) {
                continue;
            }
            let command = CommandSpec::new(program.as_str(), ["rm", "--force", id.as_str()])
                .purpose("remove a leftover quota check container");
            remove_container_with(executor, &command, &id);
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
}

/// Add the image quota checks run in to the daemon's background downloads,
/// unless a configured runtime already downloads it from the same engine, as
/// it does whenever `auto` chose that runtime.
pub(crate) fn add_quota_image(
    plan: &mut Vec<mj_core::targets::ImageRefresh>,
    config: &QuotaRuntimeConfig,
    path: Option<&OsStr>,
) {
    let Ok(QuotaRuntime::Container(runtime)) = QuotaRuntime::resolve(config, path) else {
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A `PATH` holding stand-in executables named after `engines`.
    fn path_with(engines: &[&str]) -> (tempfile::TempDir, std::ffi::OsString) {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        for engine in engines {
            let program = directory.path().join(engine);
            std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = directory.path().as_os_str().to_owned();
        (directory, path)
    }

    /// Answers the sweep's engine commands from `containers` (id, owner label)
    /// and records every `rm`.
    struct FakeEngine {
        containers: Vec<(&'static str, &'static str)>,
        removed: std::sync::Mutex<Vec<String>>,
    }

    impl FakeEngine {
        fn new(containers: Vec<(&'static str, &'static str)>) -> Self {
            Self {
                containers,
                removed: Default::default(),
            }
        }

        fn removed(&self) -> Vec<String> {
            self.removed.lock().unwrap().clone()
        }
    }

    impl CommandExecutor for FakeEngine {
        fn execute(&self, command: &CommandSpec) -> Result<mj_core::targets::CommandOutput> {
            let args = &command.args;
            let stdout = match args[0].as_str() {
                "ps" => self
                    .containers
                    .iter()
                    .map(|(id, _)| format!("{id}\n"))
                    .collect::<String>(),
                "container" => {
                    let id = args.last().unwrap();
                    self.containers
                        .iter()
                        .find(|(candidate, _)| candidate == id)
                        .map(|(_, label)| format!("{label}\n"))
                        .unwrap_or_default()
                }
                "rm" => {
                    self.removed
                        .lock()
                        .unwrap()
                        .push(args.last().unwrap().clone());
                    String::new()
                }
                other => panic!("unexpected engine command {other}"),
            };
            Ok(mj_core::targets::CommandOutput {
                status: 0,
                stdout: stdout.into_bytes(),
                stderr: Vec::new(),
            })
        }
    }

    fn sweep_removed(
        containers: Vec<(&'static str, &'static str)>,
        running: &dyn Fn(u32, &str) -> bool,
    ) -> Vec<String> {
        let (_directory, path) = path_with(&["podman"]);
        let engine = FakeEngine::new(containers);
        sweep_quota_containers_with(&engine, Some(&path), running);
        engine.removed()
    }

    #[test]
    fn sweep_keeps_containers_whose_owner_is_running() {
        let removed = sweep_removed(vec![("live", "4242:boot-a:77")], &|pid, birth| {
            pid == 4242 && birth == "boot-a:77"
        });
        assert!(removed.is_empty(), "{removed:?}");
    }

    #[test]
    fn sweep_removes_containers_whose_owner_is_gone() {
        // Dead: no such process. Reused: the pid runs, but it was born later.
        let removed = sweep_removed(
            vec![("dead", "4242:boot-a:77"), ("reused", "4243:boot-a:78")],
            &|pid, birth| pid == 4243 && birth == "boot-a:99",
        );
        assert_eq!(removed, ["dead", "reused"]);
    }

    #[test]
    fn sweep_removes_only_the_dead_owners_among_several() {
        let removed = sweep_removed(
            vec![
                ("live", "10:b:1"),
                ("dead", "11:b:2"),
                ("also-live", "10:b:1"),
            ],
            &|pid, _| pid == 10,
        );
        assert_eq!(removed, ["dead"]);
    }

    /// A container started before owners were recorded may belong to a live
    /// daemon, and a daemon's containers can outlive any age bound, so it
    /// stays. An unreadable label is treated the same way.
    #[test]
    fn sweep_keeps_containers_without_a_readable_owner() {
        let removed = sweep_removed(
            vec![
                ("legacy", ""),
                ("no-pid", "not-a-pid:boot:1"),
                ("no-birth", "4242"),
            ],
            &|_, _| false,
        );
        assert!(removed.is_empty(), "{removed:?}");
    }

    #[test]
    fn owner_token_names_this_process_as_running() {
        let Some(token) = owner_token() else {
            return;
        };
        assert!(owner_is_gone(token, |_, _| false));
        assert!(!owner_is_gone(token, owner_is_running));
    }

    #[test]
    fn a_process_that_exited_is_not_running() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let birth = mj_core::subprocess::process_birth_identity(pid).ok();
        child.wait().unwrap();
        assert!(!owner_is_running(
            pid,
            birth.as_deref().unwrap_or("unknown")
        ));
    }

    /// Where `auto` runs a check for a config file with `targets` and these
    /// engines installed, as the daemon loads it: the loader adds the
    /// standard `podman` and `docker` runtimes the file does not name.
    fn auto_decision(targets: &str, engines: &[&str]) -> Option<(Engine, String)> {
        decision(targets, engines, false)
    }

    fn decision(
        targets: &str,
        engines: &[&str],
        in_daemon_container: bool,
    ) -> Option<(Engine, String)> {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("config.toml");
        std::fs::write(
            &file,
            format!("version = {}\n{targets}", mj_core::config::CONFIG_VERSION),
        )
        .unwrap();
        let config = mj_core::config::Config::load_from(&file)
            .unwrap()
            .with_local_targets();
        if !targets.contains("[quota]") {
            assert_eq!(config.quota.runtime, QuotaRuntimeChoice::Auto);
        }
        let (_bin, path) = path_with(engines);
        let mut runtime = QuotaRuntimeConfig::of(&config);
        runtime.in_daemon_container = in_daemon_container;
        match QuotaRuntime::resolve(&runtime, Some(&path)).unwrap() {
            QuotaRuntime::Host => None,
            QuotaRuntime::Container(runtime) => {
                assert_eq!(
                    runtime.program.file_name().unwrap(),
                    runtime.engine.program()
                );
                Some((runtime.engine, runtime.image))
            }
        }
    }

    #[test]
    fn auto_runs_quota_checks_in_an_enabled_local_runtime_and_otherwise_on_the_host() {
        let standard = mj_core::config::DEFAULT_CONTAINER_IMAGE.to_owned();
        // The standard candidates exist in every loaded config, so an engine
        // that is not installed is what keeps the check on this machine.
        assert_eq!(auto_decision("", &[]), None);
        assert_eq!(
            auto_decision("", &["podman", "docker"]),
            Some((Engine::Podman, standard.clone()))
        );
        assert_eq!(
            auto_decision("", &["docker"]),
            Some((Engine::Docker, standard.clone()))
        );
        // A runtime switched off does not count, even with its engine there.
        let podman_off = "[targets.podman]\nkind = \"podman\"\nenabled = false\n";
        assert_eq!(
            auto_decision(podman_off, &["podman", "docker"]),
            Some((Engine::Docker, standard))
        );
        let both_off = "[targets.podman]\nkind = \"podman\"\nenabled = false\n\
             [targets.docker]\nkind = \"docker\"\nenabled = false\n";
        assert_eq!(auto_decision(both_off, &["podman", "docker"]), None);
        // A runtime the person configured wins over the standard candidate,
        // and the check runs its image.
        let mine = "[targets.zz-mine]\nkind = \"podman\"\nimage = \"example.invalid/mine:1\"\n";
        assert_eq!(
            auto_decision(mine, &["podman"]),
            Some((Engine::Podman, "example.invalid/mine:1".to_owned()))
        );
        // The daemon container's image ships both engine clients and its
        // config has the standard runtimes, but it has no engine to reach:
        // its checks run beside the daemon on the image's harnesses, even
        // when a named engine is chosen.
        assert_eq!(decision("", &["podman", "docker"], true), None);
        assert_eq!(
            decision("[quota]\nruntime = \"podman\"\n", &["podman"], true),
            None
        );
    }
}
