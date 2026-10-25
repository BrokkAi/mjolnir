//! Container replacement has no storage-destruction capability. In particular,
//! none of these commands use provisioning traps, close plans, or `rm -v`.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RetainedContainer {
    pub locator: TargetLocator,
    pub source_id: String,
    pub name: String,
    pub backup_name: String,
    pub current_image: String,
    current_image_id: String,
    pub target_image: String,
    pub prepared_image: Option<String>,
    pub image_pin_reserved: bool,
    selected_image_id: Option<String>,
    control_state: Option<ControlState>,
    pub control_state_restored: bool,
    pub operation_id: String,
    mounts: Vec<RetainedMount>,
    user: String,
    environment: Vec<String>,
    working_directory: String,
    memory: i64,
    nano_cpus: i64,
    pids_limit: i64,
    network_mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ControlState {
    root: String,
    staging_directory: String,
    excludes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RetainedMount {
    kind: String,
    source: String,
    destination: String,
    writable: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Inspection {
    #[serde(alias = "ID")]
    id: String,
    name: String,
    image: String,
    #[serde(default)]
    image_name: String,
    config: InspectConfig,
    host_config: InspectHost,
    mounts: Vec<InspectMount>,
    state: InspectState,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectConfig {
    image: String,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    user: String,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    working_dir: String,
    cmd: Option<Vec<String>>,
    entrypoint: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct InspectHost {
    auto_remove: bool,
    memory: i64,
    nano_cpus: i64,
    pids_limit: Option<i64>,
    network_mode: String,
    privileged: bool,
    readonly_rootfs: bool,
    devices: Option<serde_json::Value>,
    port_bindings: Option<serde_json::Value>,
    userns_mode: String,
    security_opt: Option<Vec<String>>,
    cap_add: Option<Vec<String>>,
    cap_drop: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectMount {
    #[serde(rename = "Type")]
    kind: String,
    #[serde(default)]
    name: String,
    source: String,
    destination: String,
    #[serde(rename = "RW")]
    writable: bool,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    propagation: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectState {
    running: bool,
}

// The engine env-file parser has a 64 KiB line buffer. A total environment
// may exceed that, but an individual assignment must fit before acceptance.
fn validate_retained_environment(assignment: &str) -> Result<()> {
    let (name, value) = assignment
        .split_once('=')
        .context("container environment is not an assignment")?;
    validate_inherited_environment_name(name)?;
    ensure!(
        !value.contains(['\n', '\r']) && assignment.len() < 64 * 1024 - 1,
        "this container has a multiline or oversized environment entry that the engine env-file format cannot preserve; the existing container is unchanged"
    );
    Ok(())
}

fn host_and_name(locator: &TargetLocator) -> Result<(ImageHost, &str)> {
    ensure!(
        !is_borrowed(locator),
        "a borrowed container must be upgraded through its owning session"
    );
    Ok(match locator {
        TargetLocator::LocalPodman { container_id, .. } => (ImageHost::LocalPodman, container_id),
        TargetLocator::LocalDocker { container_id, .. } => (ImageHost::LocalDocker, container_id),
        TargetLocator::SshPodman {
            ssh, container_id, ..
        } => (ImageHost::SshPodman(ssh.clone()), container_id),
        TargetLocator::SshDocker {
            ssh, container_id, ..
        } => (ImageHost::SshDocker(ssh.clone()), container_id),
        _ => bail!(
            "storage-preserving container upgrades support owned Podman and Docker containers; this target is unsupported"
        ),
    })
}

fn inspect(
    host: &ImageHost,
    name: &str,
    executor: &impl CommandExecutor,
) -> Result<Option<Inspection>> {
    // `container exists` is Podman-specific. Both engines use inspect's exit 1
    // for absence; any other failure remains an error rather than a fallback.
    let output = executor.execute(&host.command(
        vec![
            host.engine().into(),
            "container".into(),
            "inspect".into(),
            name.into(),
        ],
        "inspect retained session container".into(),
    ))?;
    let missing = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    if output.status != 0
        && (missing.contains("no such container")
            || missing.contains("no such object")
            || missing.contains("no container with name or id"))
    {
        return Ok(None);
    }
    ensure!(
        output.status == 0,
        "inspect retained container {name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut records: Vec<Inspection> =
        serde_json::from_slice(&output.stdout).context("decode container inspection")?;
    ensure!(
        records.len() == 1,
        "container inspection did not identify exactly one container"
    );
    Ok(records.pop())
}

fn owned(record: &Inspection, session_id: &str) -> Result<()> {
    ensure!(
        record
            .config
            .labels
            .get("dev.mj.managed")
            .map(String::as_str)
            == Some("true")
            && record
                .config
                .labels
                .get("dev.mj.session")
                .map(String::as_str)
                == Some(session_id),
        "refusing to replace a foreign container"
    );
    Ok(())
}

pub(crate) fn inspect_retained_container(
    locator: &TargetLocator,
    session_id: &str,
    target_image: &str,
    operation_id: &str,
    workspace: &Path,
    worker_root: &Path,
    executor: &impl CommandExecutor,
) -> Result<RetainedContainer> {
    let (host, name) = host_and_name(locator)?;
    let record = inspect(&host, name, executor)?.context(
        "the existing container is missing; Restart and upgrade cannot reconstruct its filesystem",
    )?;
    owned(&record, session_id)?;
    let entrypoint = record.config.entrypoint.as_deref().unwrap_or(&[]);
    let command = record.config.cmd.as_deref().unwrap_or(&[]);
    ensure!(
        (entrypoint.is_empty()
            && (command == ["sh", "-c", container::KEEPALIVE_SCRIPT]
                || command == ["sleep", "infinity"]))
            || (entrypoint == ["sh"] && command == ["-c", container::KEEPALIVE_SCRIPT]),
        "container upgrade cannot safely restart this custom container entrypoint; retain the current environment"
    );
    let nonempty = |value: &Option<serde_json::Value>| {
        value.as_ref().is_some_and(|value| match value {
            serde_json::Value::Null => false,
            serde_json::Value::Array(v) => !v.is_empty(),
            serde_json::Value::Object(v) => !v.is_empty(),
            _ => true,
        })
    };
    ensure!(
        !record.host_config.privileged
            && !record.host_config.readonly_rootfs
            && !nonempty(&record.host_config.devices)
            && !nonempty(&record.host_config.port_bindings)
            && record.host_config.userns_mode.is_empty()
            && record
                .host_config
                .cap_add
                .as_ref()
                .is_none_or(|capabilities| {
                    capabilities.iter().all(|capability| {
                        container::container_capabilities(host.engine())
                            .contains(&capability.strip_prefix("CAP_").unwrap_or(capability))
                    })
                })
            && record
                .host_config
                .cap_drop
                .as_ref()
                .is_none_or(Vec::is_empty)
            && record
                .host_config
                .security_opt
                .as_ref()
                .is_none_or(|options| {
                    options.iter().all(|option| {
                        container::container_security_opts(host.engine()).contains(&option.as_str())
                    })
                })
            && !record.host_config.network_mode.starts_with("container:"),
        "storage-preserving upgrades do not support this container's device, privilege, port, or namespace overrides; the existing container is unchanged"
    );
    for assignment in &record.config.env {
        validate_retained_environment(assignment)?;
    }
    ensure!(
        !record.host_config.auto_remove,
        "cannot preserve storage of an auto-remove container"
    );
    ensure!(
        record.name.trim_start_matches('/') == name,
        "container locator must name its managed container"
    );
    let mut mounts = record
        .mounts
        .into_iter()
        .map(|mount| {
            ensure!(
                matches!(mount.kind.as_str(), "volume" | "bind")
                    && !mount.options.iter().any(|o| matches!(
                        o.as_str(),
                        "O" | "z" | "Z" | "noexec"
                    ) || o.contains("overlay")
                        || o.contains("idmap"))
                    && matches!(mount.propagation.as_str(), "" | "private" | "rprivate"),
                "cannot preserve transient or unsupported mount at {} ({})",
                mount.destination,
                mount.kind
            );
            let source = if mount.kind == "volume" {
                mount.name
            } else {
                mount.source
            };
            ensure!(
                !source.is_empty()
                    && !source.contains([',', '\n'])
                    && !mount.destination.contains([',', '\n']),
                "mount at {} cannot be represented safely for replacement",
                mount.destination
            );
            Ok(RetainedMount {
                kind: mount.kind,
                source,
                destination: mount.destination,
                writable: mount.writable,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    mounts.sort_by(|left, right| left.destination.cmp(&right.destination));
    let retained_path = |path: &Path| {
        mounts
            .iter()
            .any(|m| m.writable && path.starts_with(Path::new(&m.destination)))
    };
    ensure!(
        retained_path(workspace),
        "cannot preserve workspace: {} lives in the container writable layer; use independent workspace storage before upgrading",
        workspace.display()
    );
    let control_root = Path::new("/var/lib/hel");
    let control_state = if retained_path(worker_root)
        && retained_path(mj_core::target_path::join(control_root, Path::new("profiles")).as_path())
    {
        None
    } else {
        ensure!(
            worker_root.starts_with(control_root),
            "cannot transfer this worker's unsupported control-state layout"
        );
        let base = if retained_path(Path::new("/tmp")) {
            Path::new("/tmp")
        } else {
            workspace
        };
        Some(ControlState {
            root: control_root.to_string_lossy().into_owned(),
            staging_directory: mj_core::target_path::text(&mj_core::target_path::join(
                base,
                Path::new(&format!(".mj-container-upgrade-{operation_id}")),
            )),
            excludes: mounts
                .iter()
                .filter_map(|mount| {
                    Path::new(&mount.destination)
                        .strip_prefix(control_root)
                        .ok()
                })
                .filter(|path| !path.as_os_str().is_empty())
                .map(|path| format!("./{}", mj_core::target_path::text(path)))
                .collect(),
        })
    };
    if control_state.is_some() {
        let probe = executor.execute(
            &locator_command(
                locator,
                vec!["sh".into(), "-c".into(), "tar --help 2>&1".into()],
            )
            .purpose("verify control-state transfer support"),
        )?;
        let help = String::from_utf8_lossy(&probe.stdout);
        ensure!(
            probe.status == 0
                && [
                    "--one-file-system",
                    "--unlink-first",
                    "--strip-components",
                    "--no-recursion",
                    "--occurrence"
                ]
                .iter()
                .all(|option| help.contains(option)),
            "this container lacks the tar support needed to preserve worker state; the source is unchanged"
        );
    }
    let cached = executor.execute(&host.command(
        vec![
            host.engine().into(),
            "image".into(),
            "inspect".into(),
            "--format".into(),
            "{{.Id}}".into(),
            target_image.into(),
        ],
        "inspect selected upgrade image".into(),
    ))?;
    let prepared_image = if cached.status == 0 {
        Some(String::from_utf8(cached.stdout)?.trim().to_owned())
    } else {
        let error = String::from_utf8_lossy(&cached.stderr).to_lowercase();
        ensure!(
            error.contains("no such image")
                || error.contains("image not known")
                || error.contains("image not found"),
            "cannot inspect selected image: {error}"
        );
        None
    };
    Ok(RetainedContainer {
        locator: locator.clone(),
        source_id: record.id,
        name: name.into(),
        backup_name: format!("{name}-upgrade-{operation_id}"),
        current_image_id: record.image,
        current_image: if let Some(selected) = record.config.labels.get("dev.mj.container-image") {
            selected.clone()
        } else if record.image_name.is_empty() {
            record.config.image
        } else {
            record.image_name
        },
        target_image: target_image.into(),
        control_state,
        control_state_restored: false,
        selected_image_id: prepared_image.clone(),
        prepared_image,
        image_pin_reserved: false,
        operation_id: operation_id.into(),
        mounts,
        user: record.config.user,
        environment: record.config.env,
        working_directory: record.config.working_dir,
        memory: record.host_config.memory,
        nano_cpus: record.host_config.nano_cpus,
        pids_limit: record.host_config.pids_limit.unwrap_or(0),
        network_mode: record.host_config.network_mode,
    })
}

impl RetainedContainer {
    pub(crate) fn preview(&self, session_id: &str) -> mj_core::state::ContainerUpgradePreview {
        mj_core::state::ContainerUpgradePreview {
            warning: mj_core::state::CONTAINER_UPGRADE_WARNING.into(),
            session_id: session_id.into(),
            source_container_id: self.source_id.clone(),
            current_image: self.current_image.clone(),
            target_image: self.target_image.clone(),
            current_image_id: self.current_image_id.clone(),
            target_image_id: self.selected_image_id.clone(),
            available: self.prepared_image.as_ref().map_or(
                self.current_image != self.target_image,
                |target| {
                    target.trim_start_matches("sha256:")
                        != self.current_image_id.trim_start_matches("sha256:")
                },
            ),
            retained_mounts: self.mounts.iter().map(|m| m.destination.clone()).collect(),
        }
    }

    /// Only the stopped worker's private control tree is copied. Every
    /// independent mount beneath it is excluded, including build caches.
    pub(crate) fn capture_control_state(&self, executor: &impl CommandExecutor) -> Result<()> {
        let Some(state) = &self.control_state else {
            return Ok(());
        };
        let next = format!(
            "{}/state.tar.{}",
            state.staging_directory,
            crate::session_manager::new_command_id("capture")?
        );
        let mut args = vec![
            "sh".into(),
            "-c".into(),
            "set -eu; mkdir -p -- \"$1\"; chmod 700 -- \"$1\"; shift; exec \"$@\"".into(),
            "mj-container-control-state".into(),
            state.staging_directory.clone(),
            "tar".into(),
            "--one-file-system".into(),
        ];
        for path in &state.excludes {
            args.extend(["--exclude".into(), path.clone()]);
        }
        args.extend([
            "-C".into(),
            state.root.clone(),
            "-cf".into(),
            next.clone(),
            ".".into(),
        ]);
        let output = executor.execute(
            &locator_command(&self.locator, args)
                .purpose("capture stopped worker journal and harness state"),
        )?;
        ensure!(
            output.status == 0,
            "cannot transfer worker control state; source and storage retained: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = executor.execute(
            &locator_command(
                &self.locator,
                vec![
                    "mv".into(),
                    "--".into(),
                    next,
                    format!("{}/state.tar", state.staging_directory),
                ],
            )
            .purpose("seal private control-state transfer"),
        )?;
        ensure!(
            output.status == 0,
            "cannot seal worker control-state transfer; source retained"
        );
        Ok(())
    }

    pub(crate) fn restore_control_state(&self, executor: &impl CommandExecutor) -> Result<()> {
        let Some(state) = &self.control_state else {
            return Ok(());
        };
        // A disconnected engine client may leave its extractor alive. The
        // lock and durable completion marker make every later extractor wait
        // and then skip, even after the new worker starts accepted work.
        // Capture prefixes every member with './'. Strip that component so
        // --unlink-first never tries to unlink the root '.' directory, then
        // restore just that directory's metadata without extracting files.
        let script = "set -eu; state=$1; root=$2; exec flock -x \"$state/restore.lock\" sh -c 'set -eu; [ ! -f \"$1/restore-complete\" ] || exit 0; mkdir -p -- \"$2\"; tar --unlink-first --strip-components=1 -C \"$2\" -xf \"$1/state.tar\"; tar --no-recursion --occurrence=1 -C \"$2\" -xf \"$1/state.tar\" .; touch \"$1/restore-complete\"' mj-control-restore \"$state\" \"$root\"";
        let output = executor.execute(
            &locator_command(
                &self.locator,
                vec![
                    "sh".into(),
                    "-c".into(),
                    script.into(),
                    "mj-container-control-state".into(),
                    state.staging_directory.clone(),
                    state.root.clone(),
                ],
            )
            .purpose("transfer preserved worker journal and harness state"),
        )?;
        ensure!(
            output.status == 0,
            "cannot restore private worker state; source and all mounts retained: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    pub(crate) fn source_is_current(&self, executor: &impl CommandExecutor) -> Result<bool> {
        let (host, _) = host_and_name(&self.locator)?;
        Ok(inspect(&host, &self.name, executor)?.is_some_and(|record| record.id == self.source_id))
    }

    fn image_pin(&self) -> String {
        format!(
            "localhost/mj-container-upgrade/{}:{}",
            self.name, self.operation_id
        )
    }

    pub(crate) fn release_image_pin(&self, executor: &impl CommandExecutor) -> Result<()> {
        if !self.image_pin_reserved {
            return Ok(());
        }
        let (host, _) = host_and_name(&self.locator)?;
        let pin = self.image_pin();
        let inspected = executor.execute(&host.command(
            vec![
                host.engine().into(),
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Id}}".into(),
                pin.clone(),
            ],
            "inspect private upgrade image pin".into(),
        ))?;
        if inspected.status != 0 {
            let message = String::from_utf8_lossy(&inspected.stderr).to_ascii_lowercase();
            ensure!(
                message.contains("no such image")
                    || message.contains("image not known")
                    || message.contains("image not found"),
                "cannot inspect private upgrade image pin: {message}"
            );
            return Ok(());
        }
        let image = String::from_utf8(inspected.stdout)?.trim().to_owned();
        if let Some(expected) = &self.prepared_image {
            ensure!(
                image.trim_start_matches("sha256:") == expected.trim_start_matches("sha256:"),
                "private upgrade image pin changed ownership"
            );
        }
        let args = if host.engine() == "podman" {
            vec!["untag".into(), image, pin]
        } else {
            vec![
                "image".into(),
                "rm".into(),
                "--force".into(),
                "--no-prune".into(),
                pin,
            ]
        };
        checked(&host, args, executor)?;
        Ok(())
    }

    pub(crate) fn prepare(&mut self, executor: &impl CommandExecutor) -> Result<()> {
        let (host, _) = host_and_name(&self.locator)?;
        if self.prepared_image.is_none() {
            let lock = crate::image_pull_gate::image_pull_mutex(&host, &self.target_image);
            let _pull = crate::image_pull_gate::hold_image_pull(
                &lock,
                || executor.cancellation_requested(),
                || {},
            )?;
            checked(
                &host,
                vec!["pull".into(), self.target_image.clone()],
                executor,
            )?;
            let output = checked(
                &host,
                vec![
                    "image".into(),
                    "inspect".into(),
                    "--format".into(),
                    "{{.Id}}".into(),
                    self.target_image.clone(),
                ],
                executor,
            )?;
            let id = String::from_utf8(output.stdout)?.trim().to_owned();
            ensure!(
                !id.is_empty(),
                "image preparation returned no immutable image identity"
            );
            self.prepared_image = Some(id);
        }
        checked(
            &host,
            vec![
                "image".into(),
                "tag".into(),
                self.prepared_image.as_ref().unwrap().clone(),
                self.image_pin(),
            ],
            executor,
        )?;
        if self.control_state.is_some() {
            checked(
                &host,
                vec![
                    "run".into(),
                    "--rm".into(),
                    "--entrypoint".into(),
                    "sh".into(),
                    "--user".into(),
                    if self.user.is_empty() { "0".into() } else { self.user.clone() },
                    self.prepared_image.as_ref().unwrap().clone(),
                    "-c".into(),
                    "command -v tar >/dev/null && command -v flock >/dev/null && mkdir -p /var/lib/hel && test -w /var/lib/hel".into(),
                ],
                executor,
            )?;
        }
        Ok(())
    }

    /// Idempotent after the durable replacement boundary. The stopped source
    /// remains a rollback resource until the new worker is ready.
    pub(crate) fn replace(&self, session_id: &str, executor: &impl CommandExecutor) -> Result<()> {
        let (host, _) = host_and_name(&self.locator)?;
        let image = self
            .prepared_image
            .as_ref()
            .context("container image has not been prepared")?;
        if let Some(current) = inspect(&host, &self.name, executor)? {
            owned(&current, session_id)?;
            if current.config.labels.get("dev.mj.container-upgrade") == Some(&self.operation_id) {
                if !current.state.running {
                    checked(&host, vec!["start".into(), self.name.clone()], executor)?;
                }
                return Ok(());
            }
            ensure!(
                current.id == self.source_id,
                "container identity changed since upgrade confirmation; refusing replacement"
            );
            if current.state.running {
                checked(&host, vec!["kill".into(), self.source_id.clone()], executor)?;
            }
            checked(
                &host,
                vec![
                    "rename".into(),
                    self.source_id.clone(),
                    self.backup_name.clone(),
                ],
                executor,
            )?;
        }
        ensure!(
            !self.control_state_restored,
            "replacement container is missing after its worker state was restored; retaining the source and refusing to replay older work"
        );
        let backup = inspect(&host, &self.backup_name, executor)?
            .context("retained source container disappeared; refusing to create another worker")?;
        owned(&backup, session_id)?;
        ensure!(
            backup.id == self.source_id && !backup.state.running,
            "retained source identity or stopped state changed"
        );
        mj_core::test_hooks::reach_test_hook(
            "container_upgrade_source_retained_before_replacement",
        )?;
        for mount in &self.mounts {
            if mount.kind == "volume" {
                checked(
                    &host,
                    vec!["volume".into(), "inspect".into(), mount.source.clone()],
                    executor,
                )?;
            }
        }
        let mut args = vec![
            "run".into(),
            "--detach".into(),
            "--name".into(),
            self.name.clone(),
            "--pull=never".into(),
            "--init".into(),
            "--stop-signal=KILL".into(),
        ];
        args.extend(
            container::container_capabilities(host.engine())
                .iter()
                .map(|capability| format!("--cap-add={capability}")),
        );
        for option in container::container_security_opts(host.engine()) {
            args.extend(["--security-opt".to_owned(), (*option).to_owned()]);
        }
        args.extend(managed_resource_identity_args(
            ManagedResourceKind::Container,
            session_id,
        ));
        args.extend([
            "--label".into(),
            format!("dev.mj.container-upgrade={}", self.operation_id),
            "--label".into(),
            format!("dev.mj.container-image={}", self.target_image),
        ]);
        if !self.user.is_empty() {
            args.extend(["--user".into(), self.user.clone()]);
        }
        if !self.working_directory.is_empty() {
            args.extend(["--workdir".into(), self.working_directory.clone()]);
        }
        // The CLI reads environment files on its own host, without changing
        // HOME or putting values in argv. SSH hosts read their forwarded stdin.
        let mut input = Vec::new();
        for assignment in &self.environment {
            validate_retained_environment(assignment)?;
            input.extend_from_slice(assignment.as_bytes());
            input.push(b'\n');
        }
        let environment_file = match &host {
            ImageHost::SshPodman(_) | ImageHost::SshDocker(_) => None,
            _ => {
                use std::io::Write as _;
                let mut file = tempfile::NamedTempFile::new()
                    .context("create private container environment file")?;
                file.write_all(&input)
                    .context("write private container environment")?;
                Some(file)
            }
        };
        args.extend([
            "--env-file".into(),
            environment_file.as_ref().map_or_else(
                || "/dev/stdin".into(),
                |file| file.path().to_string_lossy().into_owned(),
            ),
        ]);
        if !self.network_mode.is_empty() {
            args.extend(["--network".into(), self.network_mode.clone()]);
        }
        if self.memory > 0 {
            args.extend(["--memory".into(), self.memory.to_string()]);
        }
        if self.nano_cpus > 0 {
            args.extend(["--cpus".into(), (self.nano_cpus as f64 / 1e9).to_string()]);
        }
        args.extend(["--pids-limit".into(), self.pids_limit.to_string()]);
        for mount in &self.mounts {
            args.extend([
                "--mount".into(),
                format!(
                    "type={},source={},target={}{}",
                    mount.kind,
                    mount.source,
                    mount.destination,
                    if mount.writable { "" } else { ",readonly" }
                ),
            ]);
        }
        args.extend([
            "--entrypoint".into(),
            "sh".into(),
            image.clone(),
            "-c".into(),
            container::KEEPALIVE_SCRIPT.into(),
        ]);
        args.insert(0, host.engine().into());
        let mut command = host.command(args, "start replacement on retained storage".into());
        if environment_file.is_none() {
            command = command.with_sensitive_stdin(input);
        }
        let result = executor.execute(&command)?;
        drop(environment_file);
        ensure!(
            result.status == 0,
            "replacement start failed; source and all storage retained: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(())
    }

    pub(crate) fn retire_backup_for_destroy(
        &self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        let (host, _) = host_and_name(&self.locator)?;
        if let Some(backup) = inspect(&host, &self.backup_name, executor)? {
            owned(&backup, session_id)?;
            ensure!(
                backup.id == self.source_id && !backup.state.running,
                "refusing to retire a changed or running backup"
            );
            checked(&host, vec!["rm".into(), self.source_id.clone()], executor)?;
        }
        Ok(())
    }

    pub(crate) fn finish(&self, session_id: &str, executor: &impl CommandExecutor) -> Result<()> {
        let (host, _) = host_and_name(&self.locator)?;
        let replacement =
            inspect(&host, &self.name, executor)?.context("replacement container disappeared")?;
        owned(&replacement, session_id)?;
        ensure!(
            replacement.config.labels.get("dev.mj.container-upgrade") == Some(&self.operation_id),
            "replacement ownership changed"
        );
        if let Some(backup) = inspect(&host, &self.backup_name, executor)? {
            owned(&backup, session_id)?;
            ensure!(
                backup.id == self.source_id && !backup.state.running,
                "refusing to retire a changed or running source"
            );
            checked(&host, vec!["rm".into(), self.source_id.clone()], executor)?;
        }
        if let Some(state) = &self.control_state {
            let output = executor.execute(&locator_command(&self.locator, vec!["sh".into(), "-c".into(), "set -eu; [ -f \"$1/restore-complete\" ] || exit 0; rm -f -- \"$1/state.tar\" \"$1\"/state.tar.capture-*".into(), "mj-control-transfer-cleanup".into(), state.staging_directory.clone()]).purpose("retire completed private control-state transfer"))?;
            ensure!(
                output.status == 0,
                "cannot retire completed private control-state transfer"
            );
        }
        Ok(())
    }
}

fn checked(
    host: &ImageHost,
    mut args: Vec<String>,
    executor: &impl CommandExecutor,
) -> Result<CommandOutput> {
    args.insert(0, host.engine().into());
    let output = executor
        .execute(&host.command(args, "upgrade container while retaining storage".into()))?;
    ensure!(
        output.status == 0,
        "container upgrade failed; storage retained: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

#[cfg(test)]
pub(crate) fn container_upgrade_test_plan(session_id: &str) -> RetainedContainer {
    let name = resource_name(session_id).unwrap();
    RetainedContainer {
        locator: TargetLocator::LocalPodman {
            container_id: name.clone(),
            workspace_storage: PodmanWorkspaceLocator::Volume {
                name: format!("{name}-workspace"),
            },
            borrowed_from: None,
        },
        source_id: "source-container".into(),
        name: name.clone(),
        backup_name: format!("{name}-upgrade-upgrade-test"),
        current_image: "agent-dev:2.51.0".into(),
        current_image_id: "sha256:source".into(),
        target_image: "agent-dev:2.52.0".into(),
        prepared_image: Some("sha256:target".into()),
        image_pin_reserved: false,
        selected_image_id: Some("sha256:target".into()),
        control_state: Some(ControlState {
            root: "/var/lib/hel".into(),
            staging_directory: "/tmp/.mj-container-upgrade-test".into(),
            excludes: Vec::new(),
        }),
        control_state_restored: false,
        operation_id: "upgrade-test".into(),
        mounts: vec![RetainedMount {
            kind: "volume".into(),
            source: format!("{name}-workspace"),
            destination: "/workspace".into(),
            writable: true,
        }],
        user: "0:0".into(),
        environment: Vec::new(),
        working_directory: "/workspace".into(),
        memory: 0,
        nano_cpus: 0,
        pids_limit: 16384,
        network_mode: "bridge".into(),
    }
}

/// Prove absence before creating, and ownership before reattaching. Engine
/// failure never turns surviving workspace data into a checkpoint restore.
fn retain_legacy_volume(
    host: &ImageHost,
    name: &str,
    session: &str,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    let query = || {
        executor.execute(&host.command(
            vec![
                host.engine().into(),
                "volume".into(),
                "inspect".into(),
                name.into(),
            ],
            "inspect retained legacy volume".into(),
        ))
    };
    let mut output = query()?;
    let existed = output.status == 0;
    if !existed {
        let message = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        ensure!(
            message.contains("no such volume") || message.contains("no volume with name"),
            "cannot establish legacy volume absence: {message}; checkpoint and storage retained"
        );
        checked(
            host,
            vec![
                "volume".into(),
                "create".into(),
                "--label=dev.mj.managed=true".into(),
                format!("--label=dev.mj.session={session}"),
                name.into(),
            ],
            executor,
        )?;
        output = query()?;
    }
    ensure!(
        output.status == 0,
        "cannot verify retained legacy volume ownership"
    );
    let records: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    let record = records.first().context("volume inspection is empty")?;
    ensure!(
        record["Name"].as_str() == Some(name)
            && record["Labels"]["dev.mj.managed"].as_str() == Some("true")
            && record["Labels"]["dev.mj.session"].as_str() == Some(session),
        "legacy volume ownership changed; refusing checkpoint restore over retained storage"
    );
    Ok(existed)
}

/// Recovery only for a shipped recreation that already removed its source.
/// Existing workspace storage wins over the archive, even if it contains
/// files the archive never covered. No failure cleanup removes any storage.
pub(crate) fn recover_removed_container(
    locator: &TargetLocator,
    template: &ContainerTemplate,
    session_id: &str,
    mounts: &[AdditionalMount],
    workspace: &str,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    let (host, name) = host_and_name(locator)?;
    ensure!(
        inspect(&host, name, executor)?.is_none(),
        "legacy recovery target already exists"
    );
    let storage = match locator {
        TargetLocator::LocalPodman {
            workspace_storage, ..
        }
        | TargetLocator::SshPodman {
            workspace_storage, ..
        } => Some(workspace_storage),
        _ => None,
    };
    let restore_repositories = match storage {
        Some(PodmanWorkspaceLocator::Volume { name }) => {
            !retain_legacy_volume(&host, name, session_id, executor)?
        }
        Some(PodmanWorkspaceLocator::HostPath { path, .. }) => {
            let result = executor.execute(&host.command(
                vec![
                    "sh".into(),
                    "-c".into(),
                    "test -d \"$1\"".into(),
                    "mj-retained-workspace".into(),
                    path.clone(),
                ],
                "find surviving helper workspace".into(),
            ))?;
            ensure!(
                result.status == 0,
                "legacy helper workspace is absent or inaccessible; checkpoint retained; restore the helper resource before retrying"
            );
            false
        }
        _ => {
            let workspace = Path::new(workspace);
            if let Some(mount) = mounts
                .iter()
                .filter(|mount| workspace.starts_with(&mount.destination))
                .max_by_key(|mount| mount.destination.components().count())
            {
                ensure!(
                    mount.access == MountAccess::Rw,
                    "legacy workspace uses an unsupported read-only or transient attachment; checkpoint retained"
                );
                let source = mj_core::target_path::join(
                    &mount.source,
                    workspace.strip_prefix(&mount.destination)?,
                );
                let result = executor.execute(&host.command(
                    vec![
                        "test".into(),
                        "-d".into(),
                        source.to_string_lossy().into_owned(),
                    ],
                    "verify surviving legacy workspace attachment".into(),
                ))?;
                ensure!(
                    result.status == 0,
                    "retained legacy workspace attachment is inaccessible; refusing checkpoint restore over its storage"
                );
                false
            } else {
                true
            }
        }
    };
    let temporary = container::container_temporary_volume_name(name);
    retain_legacy_volume(&host, &temporary, session_id, executor)?;
    // Captured historical mounts are retained; provisioning's destructive
    // error traps and repository clone plan are deliberately absent.
    let args = container::container_run_args(
        host.engine(),
        template,
        name,
        session_id,
        mounts,
        storage,
        workspace,
        false,
    )?;
    checked(&host, args, executor)?;
    Ok(restore_repositories)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    // Runs the Linux container's tar/flock protocol on a real filesystem.
    #[cfg(target_os = "linux")]
    #[test]
    fn control_state_archive_stores_hard_link_bytes_and_restore_never_writes_through_links() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("control");
        let stage = directory.path().join("stage");
        std::fs::create_dir(&root).unwrap();
        let old_store = directory.path().join("old-store");
        let new_store = directory.path().join("new-store");
        let old = vec![b'o'; 256 * 1024];
        let new = vec![b'n'; 256 * 1024];
        std::fs::write(&old_store, &old).unwrap();
        std::fs::write(&new_store, &new).unwrap();
        std::fs::hard_link(&old_store, root.join("hel")).unwrap();
        std::fs::hard_link(&old_store, root.join("second-hel")).unwrap();
        let mut plan = container_upgrade_test_plan("55555555555555555555555555555555");
        plan.locator = TargetLocator::LocalBare {
            worker_root: root.to_string_lossy().into_owned(),
        };
        plan.control_state = Some(ControlState {
            root: root.to_string_lossy().into_owned(),
            staging_directory: stage.to_string_lossy().into_owned(),
            excludes: Vec::new(),
        });
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        plan.capture_control_state(&ProcessExecutor).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_file(root.join("hel")).unwrap();
        std::fs::hard_link(&new_store, root.join("hel")).unwrap();
        plan.restore_control_state(&ProcessExecutor).unwrap();
        assert_eq!(std::fs::read(root.join("hel")).unwrap(), old);
        assert_eq!(std::fs::read(&new_store).unwrap(), new);
        assert_eq!(std::fs::read(&old_store).unwrap(), old);
        assert_eq!(root.metadata().unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(
            root.join("hel").metadata().unwrap().ino(),
            root.join("second-hel").metadata().unwrap().ino()
        );
    }

    struct Resources {
        containers: Vec<String>,
        volumes: Vec<String>,
    }
    impl Drop for Resources {
        fn drop(&mut self) {
            for (resource, names) in [("container", &self.containers), ("volume", &self.volumes)] {
                for name in names {
                    let command = CommandSpec::new("podman", [resource, "rm", "--force", name]);
                    if let Err(error) = ProcessExecutor.execute_cleanup(&command) {
                        tracing::warn!(%error, %name, "isolated container upgrade test cleanup failed");
                    }
                }
            }
        }
    }

    struct FailReplacementRun;
    impl CommandExecutor for FailReplacementRun {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            if command.purpose == "start replacement on retained storage" {
                bail!("injected engine failure after retaining the stopped source");
            }
            ProcessExecutor.execute(command)
        }
    }

    struct RetainedInspectionExecutor {
        inspection: String,
    }

    impl CommandExecutor for RetainedInspectionExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            match command.purpose.as_str() {
                "inspect retained session container" => Ok(CommandOutput {
                    status: 0,
                    stdout: self.inspection.as_bytes().to_vec(),
                    stderr: Vec::new(),
                }),
                "inspect selected upgrade image" => Ok(CommandOutput {
                    status: 0,
                    stdout: b"sha256:prepared\n".to_vec(),
                    stderr: Vec::new(),
                }),
                purpose => bail!("unexpected fixture command {purpose}"),
            }
        }
    }

    fn retained_inspection(security_opt: &[&str]) -> String {
        serde_json::json!([{
            "Id": "source-id",
            "Name": "/container-name",
            "Image": "sha256:source-image",
            "Config": {
                "Image": "example/image:old",
                "Labels": {
                    "dev.mj.managed": "true",
                    "dev.mj.session": "session-fixture",
                },
                "User": "",
                "Env": [],
                "WorkingDir": "",
                "Cmd": ["sh", "-c", container::KEEPALIVE_SCRIPT],
                "Entrypoint": null,
            },
            "HostConfig": {
                "Memory": 0,
                "NanoCpus": 0,
                "PidsLimit": null,
                "NetworkMode": "default",
                "Privileged": false,
                "ReadonlyRootfs": false,
                "Devices": null,
                "PortBindings": null,
                "UsernsMode": "",
                "SecurityOpt": security_opt,
                "CapAdd": ["PERFMON"],
                "CapDrop": [],
            },
            "Mounts": [{
                "Type": "bind",
                "Source": "/tmp",
                "Destination": "/",
                "RW": true,
                "Propagation": "rprivate",
                "Options": [],
            }],
            "State": { "Running": true },
        }])
        .to_string()
    }

    fn inspect_fixture(
        locator: &TargetLocator,
        security_opt: &[&str],
    ) -> Result<RetainedContainer> {
        inspect_retained_container(
            locator,
            "session-fixture",
            "example/image:new",
            "operation-fixture",
            Path::new("/workspace/session-fixture"),
            Path::new("/var/lib/hel/workers/session-fixture"),
            &RetainedInspectionExecutor {
                inspection: retained_inspection(security_opt),
            },
        )
    }

    #[test]
    fn retained_podman_container_may_carry_the_managed_security_option() {
        let locator = TargetLocator::LocalPodman {
            container_id: "container-name".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        };
        let retained = inspect_fixture(&locator, &["label=disable"]).unwrap();
        assert_eq!(retained.name, "container-name");
    }

    #[test]
    fn retained_container_rejects_a_security_option_the_replacement_would_drop() {
        let podman = TargetLocator::LocalPodman {
            container_id: "container-name".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        };
        let error = inspect_fixture(&podman, &["seccomp=unconfined"]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("storage-preserving upgrades do not support"),
            "{error:#}"
        );

        let docker = TargetLocator::LocalDocker {
            container_id: "container-name".into(),
            borrowed_from: None,
        };
        let error = inspect_fixture(&docker, &["label=disable"]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("storage-preserving upgrades do not support"),
            "{error:#}"
        );
    }

    // Hard-won: #1326: automatic recreation deleted uncheckpointed workspace and cache storage.
    #[test]
    #[ignore = "requires rootless Podman, cached 2.51.0/2.52.0 images, and MJ_INSTANCE=container-upgrade-regression"]
    fn replacement_recovery_reattaches_the_same_storage_without_a_checkpoint() {
        assert_eq!(
            mj_core::config::instance_name().as_deref(),
            Some("container-upgrade-regression")
        );
        let session = mj_core::state::new_session_id().unwrap();
        let name = resource_name(&session).unwrap();
        let cache = format!("{name}-test-cache");
        let workspace = format!("/workspace/{session}");
        let large_environment = (0..20)
            .map(|index| format!("MJ_UPGRADE_ENV_{index}={}", "e".repeat(4096)))
            .collect::<Vec<_>>();
        let mut template = ContainerTemplate {
            image: "ghcr.io/brokkai/mjolnir/agent-dev:2.51.0".into(),
            pull_policy: ImagePullPolicy::Never,
            extra_run_args: vec![
                "--mount".into(),
                format!("type=volume,source={cache},target=/cache"),
            ],
            workspace_storage: PodmanWorkspaceStorage::PodmanVolume,
            build_cache: None,
        };
        let storage = podman_workspace_locator_named(&template, &name).unwrap();
        let PodmanWorkspaceLocator::Volume { name: volume } = &storage else {
            unreachable!()
        };
        // Drop the containers before this retained host directory.
        let directory = tempfile::tempdir().unwrap();
        let _resources = Resources {
            containers: vec![name.clone(), format!("{name}-upgrade-test-operation")],
            volumes: vec![
                cache.clone(),
                volume.clone(),
                container::container_temporary_volume_name(&name),
            ],
        };
        let host = ImageHost::LocalPodman;
        checked(
            &host,
            vec!["volume".into(), "create".into(), cache.clone()],
            &ProcessExecutor,
        )
        .unwrap();
        for assignment in &large_environment {
            template
                .extra_run_args
                .extend(["--env".into(), assignment.clone()]);
        }
        let locator = TargetLocator::LocalPodman {
            container_id: name.clone(),
            workspace_storage: storage,
            borrowed_from: None,
        };
        let root = worker_root(&locator, &session).unwrap();
        // A source can carry a value too long for env-file transport. Refuse
        // it before stopping anything, rather than discovering the limit on
        // the far side of replacement.
        let mut unsupported = template.clone();
        unsupported.extra_run_args.truncate(2);
        unsupported.extra_run_args.extend([
            "--env".into(),
            format!("MJ_UPGRADE_TOO_LONG={}", "e".repeat(70 * 1024)),
        ]);
        let started = ProcessExecutor
            .execute(
                &container::podman_container_run(
                    &unsupported,
                    &name,
                    &session,
                    &[],
                    None,
                    &workspace,
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            started.status,
            0,
            "{}",
            String::from_utf8_lossy(&started.stderr)
        );
        let source = inspect(&host, &name, &ProcessExecutor).unwrap().unwrap();
        assert!(
            source
                .host_config
                .cap_add
                .as_ref()
                .unwrap()
                .iter()
                .any(|capability| capability.trim_start_matches("CAP_") == "PERFMON")
        );
        let error = inspect_retained_container(
            &locator,
            &session,
            "ghcr.io/brokkai/mjolnir/agent-dev:2.52.0",
            "unsupported",
            Path::new(&workspace),
            Path::new(&root),
            &ProcessExecutor,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("oversized environment"),
            "{error:#}"
        );
        let unchanged = inspect(&host, &name, &ProcessExecutor).unwrap().unwrap();
        assert_eq!(unchanged.id, source.id);
        assert!(unchanged.state.running);
        checked(
            &host,
            vec!["rm".into(), "--force".into(), name.clone()],
            &ProcessExecutor,
        )
        .unwrap();
        let start =
            container::podman_container_run(&template, &name, &session, &[], None, &workspace)
                .unwrap();
        let started = ProcessExecutor.execute(&start).unwrap();
        assert_eq!(
            started.status,
            0,
            "{}",
            String::from_utf8_lossy(&started.stderr)
        );
        // Exercise the SSH-side env-file transport through the real engine,
        // with more than a pipe buffer so input/output ordering matters.
        let output = ProcessExecutor
            .execute(
                &host
                    .command(
                        vec![
                            "podman".into(),
                            "run".into(),
                            "--rm".into(),
                            "--pull=never".into(),
                            "--env-file".into(),
                            "/dev/stdin".into(),
                            "--entrypoint".into(),
                            "sh".into(),
                            template.image.clone(),
                            "-c".into(),
                            "test \"${#MJ_UPGRADE_ENV_0}\" -eq 4096 && test \"${#MJ_UPGRADE_ENV_19}\" -eq 4096".into(),
                        ],
                        "verify engine stdin environment transport".into(),
                    )
                    .with_sensitive_stdin(
                        format!("{}\n", large_environment.join("\n")).into_bytes(),
                    ),
            )
            .unwrap();
        assert_eq!(
            output.status,
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let script = format!(
            "set -eu; mkdir -p '{workspace}/uncheckpointed-cache' '{root}'; printf workspace >'{workspace}/uncheckpointed-cache/marker'; printf cache >/cache/marker; printf journal >'{root}/journal-marker'; printf manual >/manual-layer-marker"
        );
        checked(
            &host,
            vec![
                "exec".into(),
                name.clone(),
                "sh".into(),
                "-c".into(),
                script,
            ],
            &ProcessExecutor,
        )
        .unwrap();
        let mut plan = inspect_retained_container(
            &locator,
            &session,
            "ghcr.io/brokkai/mjolnir/agent-dev:2.52.0",
            "test-operation",
            Path::new(&workspace),
            Path::new(&root),
            &ProcessExecutor,
        )
        .unwrap();
        // Use the cached immutable target, keeping this isolated test offline.
        let image = checked(
            &host,
            vec![
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Id}}".into(),
                plan.target_image.clone(),
            ],
            &ProcessExecutor,
        )
        .unwrap();
        plan.prepared_image = Some(String::from_utf8(image.stdout).unwrap().trim().into());
        let retained = plan.mounts.clone();
        plan.capture_control_state(&ProcessExecutor).unwrap();
        assert!(plan.replace(&session, &FailReplacementRun).is_err());
        let backup = inspect(&host, &plan.backup_name, &ProcessExecutor)
            .unwrap()
            .unwrap();
        assert_eq!(backup.id, plan.source_id);
        assert!(!backup.state.running);
        // Simulate a daemon replacement after the engine failed. Recovery gets
        // only the serialized plan; no checkpoint exists anywhere in this test.
        let recovered: RetainedContainer =
            serde_json::from_slice(&serde_json::to_vec(&plan).unwrap()).unwrap();
        recovered.replace(&session, &ProcessExecutor).unwrap();
        recovered.restore_control_state(&ProcessExecutor).unwrap();
        recovered.replace(&session, &ProcessExecutor).unwrap();
        let upgraded = inspect(&host, &name, &ProcessExecutor).unwrap().unwrap();
        assert_eq!(upgraded.host_config.cap_add, source.host_config.cap_add);
        let replacement = inspect_retained_container(
            &locator,
            &session,
            &plan.target_image,
            "observation",
            Path::new(&workspace),
            Path::new(&root),
            &ProcessExecutor,
        )
        .unwrap();
        assert_ne!(replacement.source_id, plan.source_id);
        assert_eq!(replacement.mounts, retained);
        let script = format!(
            "set -eu; test \"$(cat '{workspace}/uncheckpointed-cache/marker')\" = workspace; test \"$(cat /cache/marker)\" = cache; test \"$(cat '{root}/journal-marker')\" = journal; test ! -e /manual-layer-marker; test \"${{#MJ_UPGRADE_ENV_0}}\" -eq 4096; test \"${{#MJ_UPGRADE_ENV_19}}\" -eq 4096"
        );
        checked(
            &host,
            vec!["exec".into(), name, "sh".into(), "-c".into(), script],
            &ProcessExecutor,
        )
        .unwrap();
        recovered.finish(&session, &ProcessExecutor).unwrap();
        recovered.finish(&session, &ProcessExecutor).unwrap();
        assert!(
            inspect(&host, &plan.backup_name, &ProcessExecutor)
                .unwrap()
                .is_none()
        );
        // A layout we cannot preserve must fail before any container mutation.
        assert!(
            inspect_retained_container(
                &locator,
                &session,
                &plan.target_image,
                "unsupported",
                Path::new("/layer-only-workspace"),
                Path::new(&root),
                &ProcessExecutor
            )
            .is_err()
        );
        assert_eq!(
            inspect(&host, &plan.name, &ProcessExecutor)
                .unwrap()
                .unwrap()
                .id,
            replacement.source_id
        );

        // Docker-style workspaces can be ordinary recorded bind attachments.
        // Historical recovery must recognize their data instead of replaying
        // repository checkpoint contents over them, even without a Podman
        // workspace-volume locator.
        checked(
            &host,
            vec!["rm".into(), "--force".into(), plan.name.clone()],
            &ProcessExecutor,
        )
        .unwrap();
        std::fs::write(
            directory.path().join("uncheckpointed"),
            b"retained attachment",
        )
        .unwrap();
        let legacy = TargetLocator::LocalPodman {
            container_id: plan.name.clone(),
            workspace_storage: PodmanWorkspaceLocator::ContainerLayer,
            borrowed_from: None,
        };
        let mounts = [AdditionalMount {
            source: directory.path().into(),
            destination: workspace.clone().into(),
            access: MountAccess::Rw,
        }];
        assert!(
            !recover_removed_container(
                &legacy,
                &template,
                &session,
                &mounts,
                &workspace,
                &ProcessExecutor
            )
            .unwrap()
        );
        checked(
            &host,
            vec![
                "exec".into(),
                plan.name.clone(),
                "test".into(),
                "-f".into(),
                format!("{workspace}/uncheckpointed"),
            ],
            &ProcessExecutor,
        )
        .unwrap();
    }
}
