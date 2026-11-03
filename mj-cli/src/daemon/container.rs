//! Launching `mj daemon-run` inside a per-instance container.
//!
//! The daemon is the image's own `mj` ([`IMAGE_EXECUTABLE`]), built in the
//! image's builder stage against the image's C library, and only an image
//! whose `org.brokk.mjolnir.revision` label is this client's revision is used,
//! so the daemon in the container is this build. Without such an image the
//! daemon runs on the host and the launch says why and how to get one.
//!
//! The container shares this computer's network and process namespaces, runs
//! as this user's uid, and mounts `$HOME`, the data and configuration
//! directories, `/tmp` and the user's runtime directory at the paths they
//! have here. To every client the daemon is therefore the process it is on
//! the host: the loopback address and token in `daemon.json`, a PID that
//! `kill(2)` and `/proc` reach, and the same store lock.
//!
//! The container only holds processes; `sleep infinity` keeps it up between
//! daemons. Local sessions' workers are started by the daemon and so run in
//! it too, and survive a daemon handoff the way host workers do. A container
//! is never stopped while anything but that placeholder runs in it. When the
//! container's specification changes (image, mounts, engine socket), a new
//! container is created beside the old one, the new daemon reattaches to the
//! old one's workers through their sockets and host PIDs, and the old
//! container is removed once it is idle.

use anyhow::{Context, Result, bail, ensure};
use mj_core::config::{DEFAULT_CONTAINER_IMAGE, DaemonConfig, DaemonRuntime, config_dir, data_dir};
use sha2::Digest;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Label carrying the instance identity, shared by every container this
/// instance's daemons ever ran in.
const INSTANCE_LABEL: &str = "ai.brokk.mjolnir.daemon-instance";
/// Short engine queries: inspect, top, exec of the launch script.
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);
/// Pulling the agent-dev image is gigabytes on a first start.
const PULL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const OUTPUT_LIMIT: usize = 1024 * 1024;
/// The `mj` the image's builder stage installs.
const IMAGE_EXECUTABLE: &str = "/opt/mjolnir/mj";
/// The Git revision the image's `mj` was built from.
const REVISION_LABEL: &str = "org.brokk.mjolnir.revision";
/// Variables that describe the launching shell or the host's file system
/// layout rather than anything the daemon should inherit.
const NOT_FORWARDED: &[&str] = &[
    "PATH",
    "HOSTNAME",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
    "CONTAINER_HOST",
    "MJ_UPGRADE_RESUME_FILE",
    super::DAEMON_CONTAINER_ENV,
    super::DEV_RESTART_STALE_DAEMON_ENV,
];

pub(super) struct ContainerLaunch {
    pub(super) pid: u32,
    pub(super) container: String,
}

/// Where the configuration puts the daemon on this computer, decided once for
/// both the launch and the check that keeps or replaces a running daemon.
pub(super) enum Placement {
    Container(Box<ContainerSpec>),
    /// No image carries this build; the daemon runs on the host.
    Host {
        reason: String,
    },
}

/// Resolve `config` (a container runtime) to the container the daemon runs
/// in, pulling the image when it is not here yet.
pub(super) async fn placement(config: &DaemonConfig) -> Result<Placement> {
    let program = config
        .runtime
        .engine()
        .context("the daemon container launch needs a container runtime")?;
    if mj_core::program_path::find_program_on_path(program, std::env::var_os("PATH").as_deref())
        .is_none()
    {
        return Ok(Placement::Host {
            reason: format!("{program} is not installed on this computer"),
        });
    }
    let image = match resolve_image(program, config).await? {
        Ok(image) => image,
        Err(reason) => return Ok(Placement::Host { reason }),
    };
    Ok(Placement::Container(Box::new(
        ContainerSpec::for_this_host(config.runtime, config, image)?,
    )))
}

/// Start a daemon in the container `spec` describes, creating the container
/// first when it does not exist yet.
pub(super) async fn launch(spec: &ContainerSpec, log_path: &Path) -> Result<ContainerLaunch> {
    let program = spec.program();
    // Housekeeping: a container left over must not stop the daemon starting.
    if let Err(error) = remove_idle_predecessors(program, spec).await {
        tracing::warn!(
            error = format!("{error:#}"),
            "could not remove earlier daemon containers"
        );
    }
    ensure_running(program, spec).await?;
    prepare_log(log_path)?;
    let pid = exec_daemon(program, spec, log_path).await?;
    Ok(ContainerLaunch {
        pid,
        container: spec.name.clone(),
    })
}

/// This client's Git revision, which the daemon image's label must carry.
fn this_revision() -> Option<&'static str> {
    mj_core::worker_build::BUILD_ID
        .rsplit_once('+')
        .map(|(_, revision)| revision)
}

/// An image present here, by id, with the revision its `mj` was built from.
async fn inspect_image(program: &str, image: &str) -> Result<Option<(String, Option<String>)>> {
    let format = format!("--format={{{{.Id}}}}|{{{{index .Config.Labels \"{REVISION_LABEL}\"}}}}");
    let output = engine(program, ["image", "inspect", &format, image], QUERY_TIMEOUT).await?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (id, revision) = text
        .trim()
        .split_once('|')
        .with_context(|| format!("{program} reported {text:?} for image {image}"))?;
    // Docker prints a missing label as `<no value>`, Podman as nothing.
    let revision = Some(revision.trim())
        .filter(|value| !value.is_empty() && *value != "<no value>")
        .map(str::to_owned);
    let id = id.trim().to_owned();
    Ok(Some((id, revision)))
}

/// The id of an image whose `mj` is this build: the configured image, or
/// this build's default image, or, with no image configured, any image here
/// labelled with this revision. `Err` explains why there is none.
async fn resolve_image(
    program: &str,
    config: &DaemonConfig,
) -> Result<std::result::Result<String, String>> {
    let Some(wanted) = this_revision() else {
        return Ok(Err(
            "this mj build does not know its Git revision, so no image can be matched to it"
                .to_owned(),
        ));
    };
    let candidate = config
        .image
        .clone()
        .unwrap_or_else(|| DEFAULT_CONTAINER_IMAGE.to_owned());
    let mut found = inspect_image(program, &candidate).await?;
    if found
        .as_ref()
        .is_some_and(|(_, revision)| revision.as_deref() == Some(wanted))
    {
        return Ok(Ok(found.expect("checked").0));
    }
    if config.image.is_none() {
        let listed = engine(
            program,
            [
                "images",
                "--no-trunc",
                "--format={{.ID}}",
                &format!("--filter=label={REVISION_LABEL}={wanted}"),
            ],
            QUERY_TIMEOUT,
        )
        .await?;
        if listed.status.success()
            && let Some(id) = String::from_utf8_lossy(&listed.stdout)
                .lines()
                .map(str::trim)
                .find(|id| !id.is_empty())
        {
            return Ok(Ok(id.to_owned()));
        }
    }
    let mut pull_error = None;
    if found.is_none() {
        super::startup_notice(format!(
            "Pulling the daemon container image {candidate}; the first start waits for it."
        ));
        let pulled = engine(program, ["pull", &candidate], PULL_TIMEOUT).await?;
        if pulled.status.success() {
            found = inspect_image(program, &candidate).await?;
        } else {
            pull_error = Some(failure(program, &format!("pull {candidate}"), &pulled));
        }
    }
    Ok(match found {
        Some((id, revision)) if revision.as_deref() == Some(wanted) => Ok(id),
        Some((_, revision)) => Err(format!(
            "image {candidate} carries Mjolnir revision {}, not this build's {wanted}; \
             `scripts/build-agent-dev-image.sh {program}` builds one for this build from its \
             checkout",
            revision.as_deref().unwrap_or("(none)")
        )),
        None => Err(format!(
            "{:#}",
            pull_error.unwrap_or_else(|| anyhow::anyhow!("image {candidate} is not available"))
        )),
    })
}

/// Everything that decides what the daemon container looks like. Its digest
/// names the container, so a change creates a new one instead of altering a
/// container that may still hold workers.
pub(super) struct ContainerSpec {
    instance: String,
    pub(super) name: String,
    /// The image's id, so a retagged image gets a container of its own.
    image: String,
    engine: DaemonRuntime,
    /// `(host, container)` bind mounts.
    mounts: Vec<(PathBuf, PathBuf)>,
    /// `KEY=VALUE` pairs fixed at creation.
    environment: Vec<(String, String)>,
    /// Further `run` arguments, such as the user and groups.
    run_args: Vec<String>,
    home: PathBuf,
    /// Host `PATH` entries inside mounted directories, appended to the
    /// image's `PATH`.
    host_path: Vec<PathBuf>,
}

impl ContainerSpec {
    fn program(&self) -> &'static str {
        self.engine
            .engine()
            .expect("a container spec has an engine")
    }

    fn for_this_host(engine: DaemonRuntime, config: &DaemonConfig, image: String) -> Result<Self> {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set, so the daemon container has no home directory to mount")?;
        let data = data_dir();
        let configuration = config_dir();
        std::fs::create_dir_all(&data)
            .with_context(|| format!("create data directory {}", data.display()))?;
        std::fs::create_dir_all(&configuration).with_context(|| {
            format!("create configuration directory {}", configuration.display())
        })?;
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);

        let mut shared: Vec<PathBuf> = vec![home.clone(), data.clone(), configuration];
        shared.push(PathBuf::from("/tmp"));
        shared.extend(runtime_dir.clone());
        for mount in &config.mounts {
            ensure!(
                mount.is_absolute(),
                "`[daemon] mounts` entry {} must be an absolute path",
                mount.display()
            );
            shared.push(mount.clone());
        }
        let mut mounts: Vec<(PathBuf, PathBuf)> = Vec::new();
        for path in shared {
            if path.exists() && !mounts.iter().any(|(host, _)| *host == path) {
                mounts.push((path.clone(), path));
            }
        }

        let mut environment = vec![("HOME".to_owned(), home.display().to_string())];
        let mut run_args = Vec::new();
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        match engine {
            DaemonRuntime::Podman => {
                // The engine's own socket lets the daemon reach the host's
                // Podman for container sessions.
                if let Some(socket) = podman_socket(runtime_dir.as_deref()) {
                    environment.push((
                        "CONTAINER_HOST".to_owned(),
                        format!("unix://{}", socket.display()),
                    ));
                }
                if uid != 0 {
                    // The daemon and its workers run as this user's own uid on
                    // the host, so this user's signals and `/proc` reach them.
                    run_args.push("--userns=keep-id".to_owned());
                }
            }
            DaemonRuntime::Host => bail!("the host daemon runtime has no container"),
            DaemonRuntime::Docker => {
                run_args.push(format!("--user={uid}:{gid}"));
                // The image has no account for this uid; ssh and git need one.
                for file in ["/etc/passwd", "/etc/group"] {
                    mounts.push((PathBuf::from(file), PathBuf::from(file)));
                }
                let socket = PathBuf::from("/var/run/docker.sock");
                if let Ok(metadata) = std::fs::metadata(&socket) {
                    use std::os::unix::fs::MetadataExt;
                    run_args.push(format!("--group-add={}", metadata.gid()));
                    mounts.push((socket.clone(), socket));
                }
            }
        }

        let mounted = |entry: &Path| mounts.iter().any(|(_, inside)| entry.starts_with(inside));
        let host_path = std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .filter(|entry| entry.is_absolute() && mounted(entry))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let instance = short_digest(data.as_os_str().as_encoded_bytes());
        let mut spec = Self {
            name: String::new(),
            instance,
            image,
            engine,
            mounts,
            environment,
            run_args,
            home,
            host_path,
        };
        let mut digest = sha2::Sha256::new();
        digest.update(spec.engine.engine().unwrap_or("host"));
        digest.update([0]);
        digest.update(&spec.image);
        for (host, inside) in &spec.mounts {
            digest.update([0]);
            digest.update(host.as_os_str().as_encoded_bytes());
            digest.update([1]);
            digest.update(inside.as_os_str().as_encoded_bytes());
        }
        for (key, value) in &spec.environment {
            digest.update([2]);
            digest.update(key);
            digest.update([3]);
            digest.update(value);
        }
        for arg in &spec.run_args {
            digest.update([4]);
            digest.update(arg);
        }
        let digest = mj_core::hex::lower_hex(digest.finalize());
        spec.name = format!("mj-daemon-{}-{}", spec.instance, &digest[..12]);
        Ok(spec)
    }

    fn run_command(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![
            "run".into(),
            "--detach".into(),
            format!("--name={}", self.name).into(),
            format!("--label={INSTANCE_LABEL}={}", self.instance).into(),
            "--network=host".into(),
            "--pid=host".into(),
            // Home directories carry their own SELinux labels; relabeling them
            // for one container would break the host's use of them.
            "--security-opt=label=disable".into(),
            format!("--workdir={}", self.home.display()).into(),
            "--entrypoint=sleep".into(),
        ];
        args.extend(self.run_args.iter().map(OsString::from));
        for (key, value) in &self.environment {
            args.push(format!("--env={key}={value}").into());
        }
        for (host, inside) in &self.mounts {
            let mut volume = OsString::from("--volume=");
            volume.push(host);
            volume.push(":");
            volume.push(inside);
            args.push(volume);
        }
        args.push(self.image.clone().into());
        args.push("infinity".into());
        args
    }
}

/// The user's Podman API socket, started through systemd when it is not
/// listening yet. `None` leaves the daemon without container sessions on this
/// computer, which its own runtime checks then report.
fn podman_socket(runtime_dir: Option<&Path>) -> Option<PathBuf> {
    let socket = runtime_dir?.join("podman").join("podman.sock");
    if !socket.exists() {
        let mut command = std::process::Command::new("systemctl");
        command.args(["--user", "start", "podman.socket"]);
        match mj_core::subprocess::run_inherited(&mut command) {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::warn!(%status, "could not start the user's Podman socket"),
            Err(error) => tracing::warn!(
                error = format!("{error:#}"),
                "could not start the user's Podman socket"
            ),
        }
    }
    socket.exists().then_some(socket)
}

fn short_digest(bytes: &[u8]) -> String {
    let digest = mj_core::hex::lower_hex(sha2::Sha256::digest(bytes));
    digest[..12].to_owned()
}

async fn engine(
    program: &str,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    timeout: Duration,
) -> Result<std::process::Output> {
    let mut command = tokio::process::Command::new(program);
    command.args(args);
    mj_core::subprocess::run_bounded(&mut command, OUTPUT_LIMIT, timeout)
        .await
        .with_context(|| format!("run {program}"))
}

fn failure(program: &str, action: &str, output: &std::process::Output) -> anyhow::Error {
    anyhow::anyhow!(
        "{program} could not {action} ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

/// Remove this instance's containers from earlier specifications once nothing
/// but their placeholder runs in them. One that still holds workers stays.
async fn remove_idle_predecessors(program: &str, spec: &ContainerSpec) -> Result<()> {
    let listed = engine(
        program,
        [
            "ps",
            "--all",
            "--format={{.Names}}",
            &format!("--filter=label={INSTANCE_LABEL}={}", spec.instance),
        ],
        QUERY_TIMEOUT,
    )
    .await?;
    if !listed.status.success() {
        return Err(failure(program, "list daemon containers", &listed));
    }
    for name in String::from_utf8_lossy(&listed.stdout).lines() {
        let name = name.trim();
        if name.is_empty() || name == spec.name {
            continue;
        }
        if processes_in(program, name)
            .await?
            .is_some_and(|count| count > 1)
        {
            tracing::info!(
                container = name,
                "an earlier daemon container still runs workers"
            );
            continue;
        }
        let removed = engine(program, ["rm", "--force", name], QUERY_TIMEOUT).await?;
        if removed.status.success() {
            tracing::info!(container = name, "removed an idle earlier daemon container");
        } else {
            tracing::warn!(
                container = name,
                error = %String::from_utf8_lossy(&removed.stderr).trim(),
                "could not remove an idle earlier daemon container"
            );
        }
    }
    Ok(())
}

/// How many processes run in a container, or `None` when it is not running.
async fn processes_in(program: &str, name: &str) -> Result<Option<usize>> {
    let running = engine(
        program,
        ["inspect", "--format={{.State.Running}}", name],
        QUERY_TIMEOUT,
    )
    .await?;
    if !running.status.success() || String::from_utf8_lossy(&running.stdout).trim() != "true" {
        return Ok(None);
    }
    // Docker hands its arguments to ps(1); Podman takes field names.
    let fields: &[&str] = if program == "docker" {
        &["-o", "pid"]
    } else {
        &["pid"]
    };
    let mut args = vec!["top", name];
    args.extend(fields);
    let top = engine(program, args, QUERY_TIMEOUT).await?;
    if !top.status.success() {
        return Err(failure(program, &format!("list processes in {name}"), &top));
    }
    // A header line, then one PID per line.
    Ok(Some(
        String::from_utf8_lossy(&top.stdout)
            .lines()
            .skip(1)
            .filter(|line| !line.trim().is_empty())
            .count(),
    ))
}

async fn ensure_running(program: &str, spec: &ContainerSpec) -> Result<()> {
    let state = engine(
        program,
        ["inspect", "--format={{.State.Running}}", &spec.name],
        QUERY_TIMEOUT,
    )
    .await?;
    if state.status.success() {
        if String::from_utf8_lossy(&state.stdout).trim() == "true" {
            return Ok(());
        }
        let started = engine(program, ["start", &spec.name], QUERY_TIMEOUT).await?;
        if !started.status.success() {
            return Err(failure(program, &format!("start {}", spec.name), &started));
        }
        tracing::info!(container = %spec.name, "restarted the daemon container");
        return Ok(());
    }
    let created = engine(program, spec.run_command(), QUERY_TIMEOUT).await?;
    if !created.status.success() {
        return Err(failure(program, &format!("create {}", spec.name), &created));
    }
    tracing::info!(container = %spec.name, image = %spec.image, "created the daemon container");
    Ok(())
}

/// The daemon log is opened by the shell inside the container; create it here
/// first so it keeps the owner-only mode a host launch gives it.
fn prepare_log(log_path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_path)
        .with_context(|| format!("open daemon log {}", log_path.display()))?;
    Ok(())
}

/// The shell run by `exec`: it detaches the daemon into a session of its own,
/// exactly as `spawn_detached` does on the host, and prints its PID. The PID
/// namespace is the host's, so that PID is the one clients see.
const LAUNCH_SCRIPT: &str = r#"
if [ -n "${MJ_DAEMON_HOST_PATH:-}" ]; then PATH="$PATH:$MJ_DAEMON_HOST_PATH"; fi
export PATH
unset MJ_DAEMON_HOST_PATH
setsid bash -c 'exec -a mj "$0" daemon-run' "$0" </dev/null >>"$1" 2>&1 &
printf '%s\n' "$!"
"#;

async fn exec_daemon(program: &str, spec: &ContainerSpec, log_path: &Path) -> Result<u32> {
    let mut args: Vec<OsString> = vec!["exec".into()];
    // A bare `--env NAME` copies the value from the engine command's own
    // environment, which is this client's, so no value appears in an argument
    // list another user could read.
    for (name, _) in std::env::vars_os() {
        let Some(name) = name.to_str() else { continue };
        if NOT_FORWARDED.contains(&name) || spec.environment.iter().any(|(key, _)| key == name) {
            continue;
        }
        args.push(format!("--env={name}").into());
    }
    if !spec.host_path.is_empty() {
        let joined = std::env::join_paths(&spec.host_path).context("join host PATH entries")?;
        let mut entry = OsString::from("--env=MJ_DAEMON_HOST_PATH=");
        entry.push(joined);
        args.push(entry);
    }
    args.push(format!("--env={}={}", super::DAEMON_CONTAINER_ENV, spec.name).into());
    args.push(format!("--workdir={}", spec.home.display()).into());
    args.push(spec.name.clone().into());
    args.extend(["bash".into(), "-c".into(), LAUNCH_SCRIPT.into()]);
    args.push(IMAGE_EXECUTABLE.into());
    args.push(log_path.into());
    let output = engine(program, args, QUERY_TIMEOUT).await?;
    if !output.status.success() {
        return Err(failure(
            program,
            &format!("start mj daemon-run in {}", spec.name),
            &output,
        ));
    }
    let printed = String::from_utf8_lossy(&output.stdout);
    printed
        .trim()
        .parse::<u32>()
        .with_context(|| format!("read the daemon PID from {program} exec output {printed:?}"))
}
