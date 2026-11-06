//! Launching `mj daemon-run` in a durable, container-native container.
//!
//! The container is the daemon's machine. It has its own filesystem, its own
//! network namespace, and one named volume mounted at [`VOLUME_MOUNT`]. The
//! host supplies only three things: the engine command, a published loopback
//! port, and the token. No host directory, network namespace, PID namespace or
//! uid is shared with the container. A host directory the user explicitly
//! wants inside a session belongs in `[daemon] mounts`.
//!
//! The container holds a `sleep infinity` placeholder so it can be reused
//! across daemon processes. A daemon replacement starts a new daemon inside
//! the same container; workers started by the daemon are processes in that
//! container, and the data volume outlives both. The host client publishes
//! `daemon.json` itself, because the container cannot write the host data
//! directory.

use anyhow::{Context, Result, bail, ensure};
use mj_client::daemon::{
    DaemonAction, DaemonClient, DaemonMetadata, DaemonPlacement, DaemonReply, PROTOCOL_VERSION,
    metadata_path,
};
use mj_core::config::{DEFAULT_CONTAINER_IMAGE, DaemonConfig, DaemonRuntime, data_dir};
use sha2::Digest;
use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Label carrying the instance identity, shared by every container this
/// instance's daemons ever ran in.
const INSTANCE_LABEL: &str = "ai.brokk.mjolnir.daemon-instance";
/// Short engine queries: inspect, top, port, exec of the launch script.
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);
/// Pulling the agent-dev image is gigabytes on a first start.
const PULL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const OUTPUT_LIMIT: usize = 1024 * 1024;
/// The `mj` the image's builder stage installs.
const IMAGE_EXECUTABLE: &str = "/opt/mjolnir/mj";
/// The Git revision the image's `mj` was built from.
const REVISION_LABEL: &str = "org.brokk.mjolnir.revision";
/// The one volume every daemon process of this instance uses.
const VOLUME_MOUNT: &str = "/var/lib/mjolnir";
/// The daemon listens on this port inside its own network namespace. The host
/// publishes it on a random loopback port, so containers never collide.
const CONTAINER_PORT: u16 = 8787;
/// How often a launch checks whether the new daemon answers.
const START_POLL: Duration = Duration::from_millis(200);
/// How long a container daemon gets to answer its first Ping.
const START_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) struct ContainerLaunch {
    pub(super) container: String,
    pub(super) port: u16,
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
/// and its volume when they do not exist yet.
pub(super) async fn launch(spec: &ContainerSpec) -> Result<ContainerLaunch> {
    let program = spec.program();
    remove_idle_predecessors(program, spec).await?;
    ensure_running(program, spec).await?;
    let port = published_port(program, &spec.name).await?;
    let token = super::random_token()?;
    let metadata = daemon_metadata(spec, port, token);
    exec_daemon(program, spec, &metadata.token).await?;
    wait_until_answering(program, spec, &metadata).await?;
    super::write_launcher_metadata(&metadata)?;
    Ok(ContainerLaunch {
        container: spec.name.clone(),
        port,
    })
}

fn daemon_metadata(spec: &ContainerSpec, port: u16, token: String) -> DaemonMetadata {
    DaemonMetadata {
        protocol_version: PROTOCOL_VERSION,
        // The daemon's PID is a container PID. Clients ask `Identity`; this
        // field is the frozen status shape and is never a host PID here.
        pid: 0,
        address: std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        token,
        started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        build_version: mj_client::build_identity::this_build().published(),
        instance_id: String::new(),
        placement: DaemonPlacement::Container {
            name: spec.name.clone(),
            runtime: Some(spec.engine),
        },
    }
}

async fn wait_until_answering(
    program: &str,
    spec: &ContainerSpec,
    metadata: &DaemonMetadata,
) -> Result<()> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(mut client) = DaemonClient::connect(metadata.clone()).await
            && let Ok(DaemonReply::Pong) = client.request(DaemonAction::Ping).await
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let log = log_tail(program, &spec.name).await;
            let error = if log.is_empty() {
                anyhow::anyhow!(
                    "daemon container {} did not answer on loopback port {} within {}s",
                    spec.name,
                    metadata.address.port(),
                    START_TIMEOUT.as_secs()
                )
            } else {
                anyhow::anyhow!(
                    "daemon container {} did not answer on loopback port {} within {}s; log:\n{log}",
                    spec.name,
                    metadata.address.port(),
                    START_TIMEOUT.as_secs()
                )
            };
            let _ = remove_metadata();
            return Err(error);
        }
        tokio::time::sleep(START_POLL).await;
    }
}

/// The host file clients read. The container cannot write it, so the launcher
/// publishes it once the daemon answers.
pub(super) async fn is_running(runtime: DaemonRuntime, name: &str) -> Result<bool> {
    let program = runtime
        .engine()
        .context("the daemon container has no engine")?;
    let output = engine(
        program,
        ["inspect", "--format={{.State.Running}}", name],
        QUERY_TIMEOUT,
    )
    .await?;
    if !output.status.success() {
        return Ok(false);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "true")
}

/// Stop the daemon's container when it does not answer the protocol stop.
/// This is the forced path; it stops the workers in the container too.
pub(super) async fn stop(runtime: DaemonRuntime, name: &str) -> Result<()> {
    let program = runtime
        .engine()
        .context("the daemon container has no engine")?;
    let output = engine(program, ["stop", name], QUERY_TIMEOUT).await?;
    if !output.status.success() {
        return Err(failure(program, &format!("stop {name}"), &output));
    }
    Ok(())
}

/// The last part of the daemon log inside the container, for a launch failure.
pub(super) async fn log_tail(program: &str, name: &str) -> String {
    let output = engine(
        program,
        [
            "exec",
            name,
            "tail",
            "-c",
            "65536",
            &format!("{VOLUME_MOUNT}/daemon.log"),
        ],
        QUERY_TIMEOUT,
    )
    .await;
    if let Ok(output) = &output
        && output.status.success()
    {
        return String::from_utf8_lossy(&output.stdout).into_owned();
    }
    let logs = engine(program, ["logs", "--tail", "60", name], QUERY_TIMEOUT).await;
    match logs {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

/// Run `mj <args>` inside the configured daemon container, with the same
/// roots the daemon uses. Returns false when this is a host placement, so the
/// caller runs the command locally. The nested command carries the
/// container's name in [`mj_core::placement::DAEMON_CONTAINER_ENV`], like the
/// daemon: it runs in the daemon container, so it uses the image's harnesses
/// and cannot try to launch another container.
pub(crate) async fn exec_in_container(args: &[String]) -> Result<bool> {
    if mj_core::placement::daemon_container().is_some() {
        return Ok(false);
    }
    let config = super::load_daemon_config().await.unwrap_or_default();
    if config.runtime.engine().is_none() {
        return Ok(false);
    }
    let mut client = super::connect_or_start().await?;
    let identity = client.identity().await?;
    let DaemonPlacement::Container {
        name,
        runtime: Some(runtime),
    } = identity.placement
    else {
        return Ok(false);
    };
    let program = runtime
        .engine()
        .context("the daemon container has no engine")?;
    let mut command = std::process::Command::new(program);
    command.args(["exec"]);
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        command.args(["--interactive", "--tty"]);
    } else {
        command.arg("--interactive");
    }
    command
        .env("HOME", format!("{VOLUME_MOUNT}/home"))
        .env("MJ_DATA_DIR", format!("{VOLUME_MOUNT}/data"))
        .env("MJ_CONFIG_DIR", format!("{VOLUME_MOUNT}/config"))
        .env(mj_core::placement::DAEMON_CONTAINER_ENV, &name)
        .arg(&name)
        .arg(IMAGE_EXECUTABLE)
        .args(args);
    let status = mj_core::subprocess::run_inherited(&mut command)
        .with_context(|| format!("run mj {args:?} inside the daemon container"))?;
    ensure!(
        status.success(),
        "mj {args:?} inside the daemon container failed ({status})"
    );
    Ok(true)
}

/// Remove the host metadata a stopped container daemon leaves behind.
pub(super) fn remove_metadata() -> Result<()> {
    match std::fs::remove_file(metadata_path()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("remove container daemon metadata"),
    }
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
/// names the container, so a changed spec gets a new container instead of
/// altering one that may still hold workers.
pub(super) struct ContainerSpec {
    instance: String,
    pub(super) name: String,
    /// The image's id, so a retagged image gets a container of its own.
    image: String,
    engine: DaemonRuntime,
    volume: String,
    /// Explicit host directories the user asked to expose inside the
    /// container, at the same absolute path. Nothing is mounted by default.
    mounts: Vec<PathBuf>,
}

impl ContainerSpec {
    fn program(&self) -> &'static str {
        self.engine
            .engine()
            .expect("a container spec has an engine")
    }

    pub(super) fn runtime(&self) -> DaemonRuntime {
        self.engine
    }

    fn for_this_host(engine: DaemonRuntime, config: &DaemonConfig, image: String) -> Result<Self> {
        let data = data_dir();
        let instance = short_digest(data.as_os_str().as_encoded_bytes());
        let volume = format!("mj-{instance}-data");
        let mut mounts = Vec::new();
        for mount in &config.mounts {
            ensure!(
                mount.is_absolute(),
                "`[daemon] mounts` entry {} must be an absolute path",
                mount.display()
            );
            if mount.exists() {
                mounts.push(mount.clone());
            } else {
                bail!(
                    "`[daemon] mounts` entry {} does not exist on this computer",
                    mount.display()
                );
            }
        }
        let mut spec = Self {
            name: String::new(),
            instance,
            image,
            engine,
            volume,
            mounts,
        };
        let mut digest = sha2::Sha256::new();
        digest.update(spec.engine.engine().unwrap_or("host"));
        digest.update([0]);
        digest.update(&spec.image);
        digest.update([0]);
        digest.update(&spec.volume);
        for mount in &spec.mounts {
            digest.update([1]);
            digest.update(mount.as_os_str().as_encoded_bytes());
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
            // The host picks a free loopback port; the container always
            // listens on its own fixed port.
            format!("--publish=127.0.0.1::{CONTAINER_PORT}").into(),
            format!("--volume={}:{VOLUME_MOUNT}", self.volume).into(),
            format!("--workdir={VOLUME_MOUNT}").into(),
            "--entrypoint=sleep".into(),
        ];
        for mount in &self.mounts {
            let mut volume = OsString::from("--volume=");
            volume.push(mount);
            volume.push(":");
            volume.push(mount);
            args.push(volume);
        }
        args.push(self.image.clone().into());
        args.push("infinity".into());
        args
    }
}

/// The container engine's published loopback port for the daemon's fixed
/// inside port.
async fn published_port(program: &str, name: &str) -> Result<u16> {
    let output = engine(
        program,
        ["port", name, &format!("{CONTAINER_PORT}/tcp")],
        QUERY_TIMEOUT,
    )
    .await?;
    if !output.status.success() {
        return Err(failure(
            program,
            &format!("read the published port of {name}"),
            &output,
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // Docker and Podman both answer `127.0.0.1:<port>`; an IPv6 host is
    // bracketed, so the last colon still separates the port.
    let port = text
        .lines()
        .find_map(|line| line.trim().rsplit(':').next()?.parse::<u16>().ok())
        .with_context(|| format!("{program} reported no loopback port for {name}: {text:?}"))?;
    Ok(port)
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

async fn engine_with_env(
    program: &str,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    environment: &[(&str, &str)],
    timeout: Duration,
) -> Result<std::process::Output> {
    let mut command = tokio::process::Command::new(program);
    command.args(args);
    for (name, value) in environment {
        command.env(name, value);
    }
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
/// but their placeholder runs in them. A container that still holds workers
/// is left alone and blocks a second container from opening the same volume.
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
            bail!(
                "an earlier daemon container {name} still runs workers; finish or destroy those sessions before changing the daemon container"
            );
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
    let volume = engine(program, ["volume", "create", &spec.volume], QUERY_TIMEOUT).await?;
    if !volume.status.success() {
        return Err(failure(
            program,
            &format!("create data volume {}", spec.volume),
            &volume,
        ));
    }
    let created = engine(program, spec.run_command(), QUERY_TIMEOUT).await?;
    if !created.status.success() {
        return Err(failure(program, &format!("create {}", spec.name), &created));
    }
    tracing::info!(container = %spec.name, image = %spec.image, "created the daemon container");
    Ok(())
}

/// Start a daemon process in the container's own filesystem. The token and
/// the data roots are environment, never arguments, and the daemon's output
/// goes to the volume's log.
async fn exec_daemon(program: &str, spec: &ContainerSpec, token: &str) -> Result<()> {
    let script = format!("exec {IMAGE_EXECUTABLE} daemon-run >>\"$MJ_DAEMON_LOG\" 2>&1");
    let args: Vec<OsString> = vec![
        "exec".into(),
        "--detach".into(),
        format!("--workdir={VOLUME_MOUNT}").into(),
        "--env=HOME=/var/lib/mjolnir/home".into(),
        "--env=MJ_DATA_DIR=/var/lib/mjolnir/data".into(),
        "--env=MJ_CONFIG_DIR=/var/lib/mjolnir/config".into(),
        format!("--env=MJ_DAEMON_LISTEN=0.0.0.0:{CONTAINER_PORT}").into(),
        // The token is copied from this process's environment; `--env=NAME`
        // keeps its value out of the engine's argument list.
        "--env=MJ_DAEMON_TOKEN".into(),
        format!("--env=MJ_DAEMON_CONTAINER={}", spec.name).into(),
        format!("--env=MJ_DAEMON_CONTAINER_RUNTIME={program}").into(),
        "--env=MJ_DAEMON_LOG=/var/lib/mjolnir/daemon.log".into(),
        spec.name.clone().into(),
        "sh".into(),
        "-c".into(),
        script.into(),
    ];
    let output =
        engine_with_env(program, args, &[("MJ_DAEMON_TOKEN", token)], QUERY_TIMEOUT).await?;
    if !output.status.success() {
        return Err(failure(
            program,
            &format!("start mj daemon-run in {}", spec.name),
            &output,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(mounts: Vec<PathBuf>) -> ContainerSpec {
        ContainerSpec {
            instance: "instance".into(),
            name: "mj-daemon-instance-abc".into(),
            image: "image-id".into(),
            engine: DaemonRuntime::Docker,
            volume: "mj-instance-data".into(),
            mounts,
        }
    }

    /// The container is its own machine: a private network and PID
    /// namespace, one data volume, and no implicit host home or path.
    #[test]
    fn a_daemon_container_shares_nothing_but_its_volume() {
        let args = spec(vec![PathBuf::from("/srv/project")])
            .run_command()
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.iter().any(|arg| arg == "--publish=127.0.0.1::8787"));
        assert!(
            args.iter()
                .any(|arg| arg == "--volume=mj-instance-data:/var/lib/mjolnir")
        );
        assert!(
            args.iter()
                .any(|arg| arg == "--volume=/srv/project:/srv/project")
        );
        assert!(!args.iter().any(|arg| arg.contains("--network=host")));
        assert!(!args.iter().any(|arg| arg.contains("--pid=host")));
        assert!(!args.iter().any(|arg| arg.contains("--userns")));
        assert_eq!(
            args.iter()
                .filter(|arg| arg.starts_with("--volume="))
                .count(),
            2,
            "only the data volume and an explicit mount may be present"
        );
    }

    #[test]
    fn a_container_daemon_publishes_no_host_pid() {
        let metadata = daemon_metadata(&spec(Vec::new()), 43123, "token".into());
        assert_eq!(metadata.pid, 0);
        assert_eq!(metadata.address.port(), 43123);
        assert!(matches!(
            metadata.placement,
            DaemonPlacement::Container { .. }
        ));
    }
}
