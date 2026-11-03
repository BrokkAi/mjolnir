//! Launching `mj daemon-run` inside a per-instance container.
//!
//! The container shares this computer's network and process namespaces and
//! mounts `$HOME`, the data and configuration directories, `/tmp`, the user's
//! runtime directory and the `mj` executable at the paths they have here. To
//! every client the daemon is therefore the process it is on the host: the
//! loopback address and token in `daemon.json`, a PID that `kill(2)` and
//! `/proc` reach, and the same store lock. Nothing past the launch knows a
//! container is involved.
//!
//! The container only holds processes; `sleep infinity` keeps it up between
//! daemons. Local sessions' workers are started by the daemon and so run in
//! it too, and survive a daemon handoff the way host workers do. A container
//! is never stopped while anything but that placeholder runs in it. When the
//! container's specification changes (image, mounts, engine socket, an
//! executable outside `$HOME`), a new container is created beside the old
//! one, the new daemon reattaches to the old one's workers through their
//! sockets and host PIDs, and the old container is removed once it is idle.

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
/// Directories an image owns; mounting the host's over them would replace the
/// image's own programs, so an executable there is mounted file by file.
const SYSTEM_BIN_DIRS: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
];
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

/// Start a daemon in this instance's container, creating the container first
/// when it does not exist yet.
pub(super) async fn launch(config: &DaemonConfig, log_path: &Path) -> Result<ContainerLaunch> {
    let engine = config.runtime;
    let program = engine
        .engine()
        .context("the daemon container launch needs a container runtime")?;
    ensure!(
        mj_core::program_path::find_program_on_path(program, std::env::var_os("PATH").as_deref())
            .is_some(),
        "`[daemon] runtime = \"{program}\"` runs the daemon with {program}, which is not on PATH"
    );
    let spec = ContainerSpec::for_this_host(engine, config)?;
    ensure_image(program, &spec.image).await?;
    // Housekeeping: a container left over must not stop the daemon starting.
    if let Err(error) = remove_idle_predecessors(program, &spec).await {
        tracing::warn!(
            error = format!("{error:#}"),
            "could not remove earlier daemon containers"
        );
    }
    ensure_running(program, &spec).await?;
    prepare_log(log_path)?;
    let pid = exec_daemon(program, &spec, log_path).await?;
    Ok(ContainerLaunch {
        pid,
        container: spec.name,
    })
}

/// The name of the container `config` runs the daemon in on this computer.
pub(super) fn container_name(config: &DaemonConfig) -> Result<String> {
    Ok(ContainerSpec::for_this_host(config.runtime, config)?.name)
}

/// Everything that decides what the daemon container looks like. Its digest
/// names the container, so a change creates a new one instead of altering a
/// container that may still hold workers.
struct ContainerSpec {
    instance: String,
    name: String,
    image: String,
    engine: DaemonRuntime,
    /// `(host, container)` bind mounts.
    mounts: Vec<(PathBuf, PathBuf)>,
    /// `KEY=VALUE` pairs fixed at creation.
    environment: Vec<(String, String)>,
    /// Further `run` arguments, such as the user and groups.
    run_args: Vec<String>,
    home: PathBuf,
    /// The `mj` executable's path inside the container.
    executable: PathBuf,
    /// Host `PATH` entries inside mounted directories, appended to the
    /// image's `PATH`.
    host_path: Vec<PathBuf>,
}

impl ContainerSpec {
    fn for_this_host(engine: DaemonRuntime, config: &DaemonConfig) -> Result<Self> {
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

        let (executable, executable_identity) = mount_executable(&mut mounts)?;

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

        let image = config
            .image
            .clone()
            .unwrap_or_else(|| DEFAULT_CONTAINER_IMAGE.to_owned());
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
            executable,
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
        digest.update([5]);
        digest.update(executable_identity);
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

/// Make the running `mj` executable reachable inside the container at the
/// path it has on the host, and return that path with an identity that
/// changes when an executable mounted file by file is replaced.
fn mount_executable(mounts: &mut Vec<(PathBuf, PathBuf)>) -> Result<(PathBuf, Vec<u8>)> {
    let link = std::fs::read_link("/proc/self/exe").context("find the running mj executable")?;
    let executable = PathBuf::from(
        link.to_str()
            .and_then(|text| text.strip_suffix(" (deleted)"))
            .map_or_else(|| link.clone().into_os_string(), Into::into),
    );
    ensure!(
        same_file(&executable, Path::new("/proc/self/exe")),
        "the mj executable at {} was replaced after this command started; run it again to start the daemon from the new build",
        executable.display()
    );
    if mounts
        .iter()
        .any(|(_, inside)| executable.starts_with(inside))
    {
        return Ok((executable, Vec::new()));
    }
    let directory = executable
        .parent()
        .context("the mj executable has no parent directory")?
        .to_path_buf();
    if !SYSTEM_BIN_DIRS
        .iter()
        .any(|system| directory == Path::new(system))
    {
        mounts.push((directory.clone(), directory));
        return Ok((executable, Vec::new()));
    }
    // A file mount pins the file it was given, so the container must change
    // when an upgrade replaces one: the identities go into the spec.
    let mut identity = Vec::new();
    for entry in
        std::fs::read_dir(&directory).with_context(|| format!("list {}", directory.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "mj" || name.starts_with("mj-worker") || entry.path() == executable {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(entry.path())?;
            identity.extend(metadata.dev().to_le_bytes());
            identity.extend(metadata.ino().to_le_bytes());
            mounts.push((entry.path(), entry.path()));
        }
    }
    Ok((executable, identity))
}

fn same_file(left: &Path, right: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(left), std::fs::metadata(right)) {
        (Ok(left), Ok(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
        _ => false,
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

async fn ensure_image(program: &str, image: &str) -> Result<()> {
    let present = engine(program, ["image", "inspect", image], QUERY_TIMEOUT).await?;
    if present.status.success() {
        return Ok(());
    }
    super::startup_notice(format!(
        "Pulling the daemon container image {image}; the first start waits for it."
    ));
    let pulled = engine(program, ["pull", image], PULL_TIMEOUT).await?;
    if !pulled.status.success() {
        return Err(failure(program, &format!("pull {image}"), &pulled));
    }
    Ok(())
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
    args.push(spec.executable.clone().into());
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
