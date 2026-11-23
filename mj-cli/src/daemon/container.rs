//! Launching `mj daemon-run` in a durable, container-native container.
//!
//! The container is the daemon's machine. It has its own filesystem, its own
//! network namespace, and one named volume mounted at [`VOLUME_MOUNT`]. The
//! host supplies the engine command, published loopback ports, and credentials. No host directory, network namespace, PID namespace or
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
    CONTAINER_OWNER_SCHEMA, ContainerDaemonRecord, DaemonMetadata, DaemonPlacement,
    PROTOCOL_VERSION, write_container_owner,
};
use mj_core::config::{DaemonConfig, DaemonRuntime, RELEASE_CONTAINER_IMAGE, data_dir};
use sha2::Digest;
use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
const CONTAINER_API_PORT: u16 = 8788;
#[derive(Debug, Clone)]
pub(super) struct ContainerLaunch {
    pub(super) runtime: DaemonRuntime,
    pub(super) container_id: String,
    pub(super) container_name: String,
    pub(super) metadata: DaemonMetadata,
    pub(super) api_address: std::net::SocketAddr,
    pub(super) api_token: String,
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
    if mj_harness_install::program_path::find_program_on_path(
        program,
        std::env::var_os("PATH").as_deref(),
    )
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
pub(super) async fn launch(
    spec: &ContainerSpec,
    prior_unpublished: Option<&ContainerDaemonRecord>,
) -> Result<ContainerLaunch> {
    let program = spec.program();
    remove_idle_predecessors(program, spec).await?;
    ensure_running(program, spec).await?;
    let port = published_port(program, &spec.name, CONTAINER_PORT).await?;
    let api_port = published_port(program, &spec.name, CONTAINER_API_PORT).await?;
    let container_id = resolve_container_id(spec.runtime(), &spec.name)
        .await?
        .with_context(|| format!("daemon container {} disappeared before launch", spec.name))?;
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let api_address = std::net::SocketAddr::from(([127, 0, 0, 1], api_port));
    let prior = prior_unpublished.filter(|prior| {
        prior.runtime == spec.runtime()
            && prior.container_id == container_id
            && prior.address == address
    });
    let (token, api_token) = if let Some(prior) = prior {
        // Both execs may reach the store lock. Either winner must accept the published tokens.
        let token = prior
            .api_token
            .as_ref()
            .filter(|token| prior.api_address == Some(api_address) && !token.is_empty());
        let api_token = match token {
            Some(token) => token.clone(),
            None => {
                super::remove_container_api_endpoint(prior)?;
                super::random_token()?
            }
        };
        (prior.token.clone(), api_token)
    } else {
        if let Some(prior) = prior_unpublished {
            super::clear_container_owner(prior)?;
        }
        (super::random_token()?, super::random_token()?)
    };
    let metadata = daemon_metadata(spec, address, token);
    write_container_owner(&ContainerDaemonRecord {
        schema: CONTAINER_OWNER_SCHEMA,
        protocol_version: Some(metadata.protocol_version),
        runtime: spec.runtime(),
        container_id: container_id.clone(),
        container_name: spec.name.clone(),
        address: metadata.address,
        token: metadata.token.clone(),
        started_at: metadata.started_at.clone(),
        api_address: Some(api_address),
        api_token: Some(api_token.clone()),
    })?;
    exec_daemon(program, spec, &container_id, &metadata.token, &api_token).await?;
    Ok(ContainerLaunch {
        runtime: spec.runtime(),
        container_id,
        container_name: spec.name.clone(),
        metadata,
        api_address,
        api_token,
    })
}

fn daemon_metadata(
    spec: &ContainerSpec,
    address: std::net::SocketAddr,
    token: String,
) -> DaemonMetadata {
    DaemonMetadata {
        protocol_version: PROTOCOL_VERSION,
        // The daemon's PID is a container PID. Clients ask `Identity`; this
        // field is the frozen status shape and is never a host PID here.
        pid: 0,
        address,
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

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ContainerDaemonStatus {
    Running,
    Exited { container_running: bool },
    Unknown(String),
}

pub(super) async fn resolve_container_id(
    runtime: DaemonRuntime,
    name: &str,
) -> Result<Option<String>> {
    let program = runtime
        .engine()
        .context("the daemon container has no engine")?;
    resolve_id_by_name(program, name).await
}

async fn resolve_id_by_name(program: &str, name: &str) -> Result<Option<String>> {
    // Container names may contain literal dots in addition to letters,
    // digits, hyphens, and underscores. Docker matches names with a leading
    // slash while Podman omits it, so accept an optional leading slash and
    // verify the exact name from the formatted output.
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')),
        "invalid daemon container name: {name:?}"
    );
    let pattern = name.replace('.', "[.]");
    let listed = list_containers(
        program,
        &format!("name=^/?{pattern}$"),
        "{{.ID}}\t{{.Names}}",
    )
    .await?;
    parse_container_id_for_name(program, &listed, name)
}

fn parse_container_id_for_name(program: &str, stdout: &str, name: &str) -> Result<Option<String>> {
    let mut ids = stdout.lines().filter_map(|line| {
        let (id, names) = line.split_once('\t')?;
        let id = id.trim();
        let matched = names
            .split(',')
            .any(|candidate| candidate.trim().trim_start_matches('/') == name);
        (!id.is_empty() && matched).then_some(id)
    });
    let id = ids.next().map(str::to_owned);
    ensure!(
        ids.next().is_none(),
        "{program} listed multiple containers named {name}"
    );
    Ok(id)
}

async fn list_container_ids(program: &str, filter: &str) -> Result<String> {
    list_containers(program, filter, "{{.ID}}").await
}

async fn list_containers(program: &str, filter: &str, format: &str) -> Result<String> {
    let output = engine(
        program,
        [
            "ps",
            "--all",
            "--no-trunc",
            "--filter",
            filter,
            "--format",
            format,
        ],
        QUERY_TIMEOUT,
    )
    .await
    .with_context(|| format!("list containers matching {filter}"))?;
    if !output.status.success() {
        return Err(failure(
            program,
            &format!("list containers matching {filter}"),
            &output,
        ));
    }
    String::from_utf8(output.stdout).context("container listing was not valid UTF-8")
}

async fn container_is_running(program: &str, id: &str) -> Result<bool> {
    let output = engine(
        program,
        ["inspect", "--format={{.State.Running}}", id],
        QUERY_TIMEOUT,
    )
    .await
    .with_context(|| format!("inspect container {id}"))?;
    if !output.status.success() {
        return Err(failure(
            program,
            &format!("inspect container {id}"),
            &output,
        ));
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        state => {
            bail!("{program} reported an unexpected running state for container {id}: {state:?}")
        }
    }
}

async fn container_top(program: &str, id: &str, field: &str) -> Result<String> {
    // Docker hands its arguments to ps(1); Podman takes field names.
    let mut args = vec!["top", id];
    if program == "docker" {
        args.push("-o");
    }
    args.push(field);
    let output = engine(program, args, QUERY_TIMEOUT)
        .await
        .with_context(|| format!("list processes in container {id}"))?;
    if !output.status.success() {
        return Err(failure(
            program,
            &format!("list processes in container {id}"),
            &output,
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(super) async fn probe_container_daemon(
    runtime: DaemonRuntime,
    container_id: &str,
) -> ContainerDaemonStatus {
    let probe = async {
        let program = runtime
            .engine()
            .context("the daemon container has no engine")?;
        let ids = list_container_ids(program, &format!("id={container_id}")).await?;
        if !ids.lines().any(|id| id.trim() == container_id)
            || !container_is_running(program, container_id).await?
        {
            return Ok(ContainerDaemonStatus::Exited {
                container_running: false,
            });
        }
        let top = container_top(program, container_id, "args").await?;
        Ok::<_, anyhow::Error>(if top_output_has_daemon_run(&top) {
            ContainerDaemonStatus::Running
        } else {
            ContainerDaemonStatus::Exited {
                container_running: true,
            }
        })
    };
    match probe.await {
        Ok(status) => status,
        Err(error) => ContainerDaemonStatus::Unknown(format!("{error:#}")),
    }
}

pub(super) fn top_output_has_daemon_run(stdout: &str) -> bool {
    stdout.lines().skip(1).any(|line| {
        let mut argv = line.split_whitespace();
        let Some(program) = argv.next().and_then(|arg| Path::new(arg).file_name()) else {
            return false;
        };
        match argv.next() {
            Some("daemon-run" | "daemon-wait-exit") => program == "mj",
            Some("-c") if program == "sh" => {
                // The transient launch shell execs the image binary in place.
                let command = match argv.next() {
                    Some("exec") => argv.next(),
                    command => command,
                };
                command == Some(IMAGE_EXECUTABLE)
                    && matches!(argv.next(), Some("daemon-run" | "daemon-wait-exit"))
            }
            _ => false,
        }
    })
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
pub(crate) async fn exec_in_container(args: &[String], interactive: bool) -> Result<bool> {
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
    if interactive && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
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
    let status =
        mj_core::runtime::spawn_blocking(move || mj_core::subprocess::run_inherited(&mut command))
            .await
            .context("container command task panicked")??;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(true)
}

/// This client's Git revision, which the daemon image's label must carry.
fn this_revision() -> Option<&'static str> {
    mj_core::worker_build::build_id()
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
        .unwrap_or_else(|| RELEASE_CONTAINER_IMAGE.to_owned());
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
        spec.name = spec.name_with_api_port(CONTAINER_API_PORT);
        Ok(spec)
    }

    fn name_with_api_port(&self, api_port: u16) -> String {
        let mut digest = sha2::Sha256::new();
        digest.update(self.engine.engine().unwrap_or("host"));
        digest.update([0]);
        digest.update(&self.image);
        digest.update([0]);
        digest.update(&self.volume);
        for mount in &self.mounts {
            digest.update([1]);
            digest.update(mount.as_os_str().as_encoded_bytes());
        }
        digest.update(api_port.to_be_bytes());
        let digest = mj_core::hex::lower_hex(digest.finalize());
        format!("mj-daemon-{}-{}", self.instance, &digest[..12])
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
            format!("--publish=127.0.0.1::{CONTAINER_API_PORT}").into(),
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
async fn published_port(program: &str, name: &str, container_port: u16) -> Result<u16> {
    let output = engine(
        program,
        ["port", name, &format!("{container_port}/tcp")],
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
        let Some(id) = resolve_id_by_name(program, name).await? else {
            continue;
        };
        if processes_in(program, &id)
            .await?
            .is_some_and(|count| count > 1)
        {
            bail!(
                "an earlier daemon container {name} still runs workers; finish or destroy those sessions before changing the daemon container"
            );
        }
        let removed = engine(program, ["rm", "--force", &id], QUERY_TIMEOUT).await?;
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
async fn processes_in(program: &str, id: &str) -> Result<Option<usize>> {
    if !container_is_running(program, id).await? {
        return Ok(None);
    }
    let top = container_top(program, id, "pid").await?;
    Ok(Some(
        top.lines()
            .skip(1)
            .filter(|line| !line.trim().is_empty())
            .count(),
    ))
}

async fn ensure_running(program: &str, spec: &ContainerSpec) -> Result<()> {
    if let Some(id) = resolve_id_by_name(program, &spec.name).await? {
        if container_is_running(program, &id).await? {
            return Ok(());
        }
        let started = engine(program, ["start", &id], QUERY_TIMEOUT).await?;
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
async fn exec_daemon(
    program: &str,
    spec: &ContainerSpec,
    id: &str,
    token: &str,
    api_token: &str,
) -> Result<()> {
    let script = format!(
        "exec {IMAGE_EXECUTABLE} daemon-wait-exit --exit-dir {VOLUME_MOUNT}/data/daemon-exits -- {IMAGE_EXECUTABLE} daemon-run >>\"$MJ_DAEMON_LOG\" 2>&1"
    );
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
        format!("--env=MJ_DAEMON_API_LISTEN=0.0.0.0:{CONTAINER_API_PORT}").into(),
        "--env=MJ_DAEMON_API_TOKEN".into(),
        format!("--env=MJ_DAEMON_CONTAINER={}", spec.name).into(),
        format!("--env=MJ_DAEMON_CONTAINER_RUNTIME={program}").into(),
        "--env=MJ_DAEMON_LOG=/var/lib/mjolnir/daemon.log".into(),
        id.into(),
        "sh".into(),
        "-c".into(),
        script.into(),
    ];
    let output = engine_with_env(
        program,
        args,
        &[
            ("MJ_DAEMON_TOKEN", token),
            ("MJ_DAEMON_API_TOKEN", api_token),
        ],
        QUERY_TIMEOUT,
    )
    .await?;
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

    #[test]
    fn docker_and_podman_top_distinguish_daemons_from_workers() {
        let docker = "COMMAND\nsleep infinity\n/opt/mjolnir/mj daemon-run\n";
        let podman = "COMMAND\nsleep infinity\nmj daemon-run\n";
        for output in [
            docker,
            podman,
            "COMMAND\n/usr/local/bin/mj daemon-run\n",
            "COMMAND\nmj daemon-wait-exit -- /opt/mjolnir/mj daemon-run\n",
            "COMMAND\nsh -c exec /opt/mjolnir/mj daemon-run >>\"$MJ_DAEMON_LOG\" 2>&1\n",
            "COMMAND\n/bin/sh -c exec /opt/mjolnir/mj daemon-wait-exit -- mj daemon-run\n",
        ] {
            assert!(top_output_has_daemon_run(output), "{output}");
        }
        let workers = "COMMAND\nsleep infinity\n/usr/local/bin/hel worker\n                       /usr/bin/codex app-server\n/bin/sh -c echo daemon-run\n                       grep daemon-run\n";
        assert!(!top_output_has_daemon_run(workers));
    }

    #[test]
    fn container_name_listing_matches_both_docker_and_podman_formats() {
        let podman = "id-other\tmj-daemon-2\nid-match\tmj-daemon\n";
        assert_eq!(
            parse_container_id_for_name("podman", podman, "mj-daemon").unwrap(),
            Some("id-match".into())
        );
        let docker = "id-match\t/mj-daemon\n";
        assert_eq!(
            parse_container_id_for_name("docker", docker, "mj-daemon").unwrap(),
            Some("id-match".into())
        );
        let absent = "id-other\tmj-daemon-2\n";
        assert_eq!(
            parse_container_id_for_name("podman", absent, "mj-daemon").unwrap(),
            None
        );
    }

    #[tokio::test]
    #[ignore = "requires live podman socket"]
    async fn podman_probe_distinguishes_stopped_container_and_live_placeholder() {
        let name = format!(
            "mj-test-daemon-probe-{}",
            super::super::random_token().unwrap()
        );
        let runtime = DaemonRuntime::Podman;
        let image = "docker.io/library/alpine:3";
        let run = engine(
            "podman",
            ["run", "--detach", "--name", &name, image, "sleep", "3600"],
            PULL_TIMEOUT,
        )
        .await
        .unwrap();
        assert!(
            run.status.success(),
            "{}",
            failure("podman", "run test container", &run)
        );
        let id = String::from_utf8(run.stdout).unwrap().trim().to_owned();

        let resolved = resolve_container_id(runtime, &name).await;
        let was_running = container_is_running("podman", &id).await;
        let live = probe_container_daemon(runtime, &id).await;
        let stopped = stop(runtime, &id).await;
        let is_running = container_is_running("podman", &id).await;
        let after_stop = probe_container_daemon(runtime, &id).await;
        let cleanup = engine("podman", ["rm", "--force", &id], QUERY_TIMEOUT)
            .await
            .unwrap();
        assert!(
            cleanup.status.success(),
            "{}",
            failure("podman", "remove test container", &cleanup)
        );

        assert_eq!(resolved.unwrap().as_deref(), Some(id.as_str()));
        assert!(
            was_running.unwrap(),
            "the placeholder must still be running"
        );
        assert_eq!(
            live,
            ContainerDaemonStatus::Exited {
                container_running: true
            }
        );
        stopped.unwrap();
        assert!(!is_running.unwrap());
        assert_eq!(
            after_stop,
            ContainerDaemonStatus::Exited {
                container_running: false
            }
        );
    }

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
        let spec = spec(vec![PathBuf::from("/srv/project")]);
        assert_ne!(
            spec.name_with_api_port(CONTAINER_API_PORT),
            spec.name_with_api_port(8789)
        );
        let args = spec
            .run_command()
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.iter().any(|arg| arg == "--publish=127.0.0.1::8787"));
        assert!(args.iter().any(|arg| arg == "--publish=127.0.0.1::8788"));
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
        let metadata = daemon_metadata(
            &spec(Vec::new()),
            ([127, 0, 0, 1], 43123).into(),
            "token".into(),
        );
        assert_eq!(metadata.pid, 0);
        assert_eq!(metadata.address.port(), 43123);
        assert!(matches!(
            metadata.placement,
            DaemonPlacement::Container { .. }
        ));
    }
}
