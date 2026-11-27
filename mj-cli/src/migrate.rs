//! Host migration commands and the source cutover owner.

#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, bail};
use clap::Args;
use std::path::PathBuf;

#[derive(Debug, Args)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct MigratePinArgs {
    #[arg(long)]
    db: PathBuf,
}

#[derive(Debug, Args)]
pub(crate) struct MigrateReceiveArgs {
    #[arg(long)]
    data_dir: Option<PathBuf>,
    #[arg(long)]
    config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct MigrateArgs {
    /// Configured SSH machine ID or OpenSSH destination for the new host.
    destination: String,
    /// Stable SSH destination that the new host can use to reach this host.
    #[arg(long)]
    source_host: Option<String>,
    /// Machine ID to assign to this host in the migrated configuration.
    #[arg(long)]
    source_machine_id: Option<String>,
    /// OpenSSH argument used from the destination to reach this host (repeatable).
    #[arg(long, allow_hyphen_values = true)]
    source_ssh_arg: Vec<String>,
    /// OpenSSH argument used to reach the destination (repeatable).
    #[arg(long, allow_hyphen_values = true)]
    ssh_arg: Vec<String>,
    /// Destination instance name.
    #[arg(long)]
    dest_instance: Option<String>,
    /// Override the destination data directory.
    #[arg(long)]
    dest_data_dir: Option<PathBuf>,
    /// Override the destination configuration directory.
    #[arg(long)]
    dest_config_dir: Option<PathBuf>,
    /// Path to the mj executable on the destination.
    #[arg(long, default_value = "mj")]
    remote_command: String,
    /// Inspect and report the migration without changing either host.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Copy logs, diagnostics, backups, and other optional data.
    #[arg(long)]
    include_logs: bool,
    /// Copy the SessionWiki index.
    #[arg(long)]
    include_sessionwiki: bool,
    /// Leave the destination SessionWiki index untouched.
    #[arg(long, conflicts_with = "include_sessionwiki")]
    skip_sessionwiki: bool,
    /// Replace an existing destination SessionWiki index.
    #[arg(long)]
    overwrite_sessionwiki: bool,
    /// Skip source-host connectivity checks when retiring this host.
    #[arg(long)]
    disconnect_local: bool,
    /// Report EC2 SSH ingress gaps without changing security groups.
    #[arg(long)]
    no_aws_modify: bool,
    /// Do not start the destination daemon after activation.
    #[arg(long)]
    no_start: bool,
}

pub(crate) fn run_pin(args: MigratePinArgs) -> Result<()> {
    #[cfg(unix)]
    {
        mj_controller::migrate::pin::run_pin_helper(
            &args.db,
            std::io::stdin().lock(),
            std::io::stdout().lock(),
        )
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        bail!("host migration only supports Unix hosts")
    }
}

pub(crate) fn apply_receive_env(args: &MigrateReceiveArgs, instance: Option<&str>) -> Result<()> {
    // Called from main before starting any threads.
    unsafe {
        if let Some(path) = &args.data_dir {
            std::env::set_var("MJ_DATA_DIR", path);
        }
        if let Some(path) = &args.config_dir {
            std::env::set_var("MJ_CONFIG_DIR", path);
        }
    }
    mj_core::config::apply_instance_flag(instance)
}

pub(crate) fn run_receive(_args: MigrateReceiveArgs) -> Result<()> {
    #[cfg(not(unix))]
    bail!("host migration only supports Unix hosts");
    #[cfg(unix)]
    {
        use mj_controller::migrate::transfer::{PublicationStep, ReceiverConfig};
        use mj_core::config::{ConfigLayout, DataLayout};
        use std::sync::Arc;
        let data = DataLayout::current();
        mj_core::config::ensure_may_control_store(data.root(), "receive a migrated Mjolnir store")?;
        let config = ReceiverConfig {
            data_dir: data.root().to_owned(),
            config_dir: ConfigLayout::current().root().to_owned(),
            home_dir: mj_core::path_input::user_home().context("locate home directory")?,
            sessionwiki_db: mj_core::config::session_index_db_path()?,
            start_daemon: Some(Arc::new(|| {
                Box::pin(async {
                    crate::daemon::connect_or_start_with_held_start_lock()
                        .await
                        .map(|_| ())
                })
            })),
            hook: mj_core::test_hooks::test_hooks_enabled().then(|| {
                Arc::new(|step| {
                    mj_core::test_hooks::reach_test_hook(match step {
                        PublicationStep::BeforeStoreRename => "migrate_before_store_rename",
                        PublicationStep::BeforeStorePlacement => "migrate_before_store_placement",
                        PublicationStep::BeforeUnfence => "migrate_before_unfence",
                    })
                }) as _
            }),
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(mj_controller::migrate::transfer::run_receiver(
                config,
                tokio::io::stdin(),
                tokio::io::stdout(),
            ))
    }
}

#[cfg(not(unix))]
pub(crate) async fn run_migrate(_args: MigrateArgs) -> Result<()> {
    bail!("host migration only supports Unix hosts")
}

#[cfg(unix)]
pub(crate) async fn run_migrate(args: MigrateArgs) -> Result<()> {
    unix::run(args).await
}

#[cfg(unix)]
mod unix {
    use super::*;
    use anyhow::ensure;
    use mj_controller::controller::ControllerStoreGuard;
    use mj_controller::database::{self, StoreFenceRecord};
    use mj_controller::migrate::preflight::inspect_preflight;
    use mj_controller::migrate::protocol::*;
    use mj_controller::migrate::transfer::{CutoverHooks, MigrationStream, SenderPlan};
    use mj_core::config::{
        Config, ConfigLayout, DaemonRuntime, DataLayout, Machine, MachineId, SshConnection,
        TargetTemplate,
    };
    use mj_core::targets::{SshTarget, remote_command_line};
    use rusqlite::{Connection, OpenFlags};
    use sha2::{Digest, Sha256};
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    fn open_db(path: &Path) -> Result<Connection> {
        Ok(Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?)
    }

    fn config_fingerprint(path: &Path) -> Result<Option<[u8; 32]>> {
        match std::fs::read(path) {
            Ok(contents) => Ok(Some(Sha256::digest(contents).into())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    fn copied_ssh_credentials(
        ssh: &SshConnection,
        old_config: &Path,
        old_home: &Path,
    ) -> Result<bool> {
        use mj_core::config::migration_paths::{
            extract_ssh_identity_arg_paths, extract_ssh_known_hosts_arg_paths, strip_normal_prefix,
        };
        let identities = ssh
            .identity_file
            .iter()
            .cloned()
            .chain(extract_ssh_identity_arg_paths(&ssh.extra_args));
        for path in identities {
            let path = mj_core::path_input::expand_home(&path, Some(old_home))?;
            if strip_normal_prefix(&path, old_config)
                .is_some_and(|rel| !rel.starts_with("instances"))
                || strip_normal_prefix(&path, old_home).is_some()
            {
                return Ok(true);
            }
        }
        for path in extract_ssh_known_hosts_arg_paths(&ssh.extra_args) {
            let path = mj_core::path_input::expand_home(&path, Some(old_home))?;
            if strip_normal_prefix(&path, &old_config.join("ec2")).is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn ssh_target(ssh: &SshConnection) -> SshTarget {
        let mut ssh_args = ssh.extra_args.clone();
        if let Some(path) = &ssh.identity_file {
            ssh_args.push("-i".into());
            ssh_args.push(path.to_string_lossy().into_owned());
        }
        SshTarget {
            destination: ssh
                .user
                .as_ref()
                .map_or_else(|| ssh.host.clone(), |user| format!("{user}@{}", ssh.host)),
            ssh_args,
        }
    }

    fn template_sets(config: &Config) -> (BTreeMap<String, BTreeSet<String>>, BTreeSet<String>) {
        let mut local = BTreeMap::new();
        let mut remote = BTreeSet::new();
        for (id, target) in &config.targets {
            if matches!(
                target,
                TargetTemplate::LocalBare
                    | TargetTemplate::LocalPodman { .. }
                    | TargetTemplate::LocalDocker { .. }
                    | TargetTemplate::AppleContainer { .. }
            ) {
                local.insert(id.clone(), BTreeSet::from([target.kind_name().to_owned()]));
            } else {
                remote.insert(id.clone());
            }
        }
        (local, remote)
    }

    fn default_machine_id() -> Result<MachineId> {
        let host = mj_core::storage::local_host_name();
        let id: String = host
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let id = id.get(..id.len().min(64)).unwrap_or(&id).trim_matches('-');
        MachineId::new(if id.is_empty() || id == "local" {
            "source"
        } else {
            id
        })
    }

    pub(super) async fn run(args: MigrateArgs) -> Result<()> {
        let data = DataLayout::current();
        let config_layout = ConfigLayout::current();
        mj_core::config::ensure_may_control_store(data.root(), "migrate this Mjolnir store")?;
        let initial_config_sha256 = config_fingerprint(&config_layout.config_toml())?;
        let config = Config::load()?;
        ensure!(
            initial_config_sha256 == config_fingerprint(&config_layout.config_toml())?,
            "config.toml changed while loading; re-run mj migrate"
        );
        ensure!(
            !matches!(config.daemon.runtime, DaemonRuntime::Ssh),
            "run mj migrate on the daemon host"
        );
        ensure!(
            !matches!(
                config.daemon.runtime,
                DaemonRuntime::Podman | DaemonRuntime::Docker
            ) && !crate::daemon::daemon_configured_for_container().await,
            "container-hosted daemons cannot migrate"
        );
        ensure!(
            !args.destination.starts_with('-'),
            "SSH destination cannot start with '-'"
        );
        let configured = config.machines.get(args.destination.as_str());
        let mut dest = match configured {
            Some(Machine::Ssh { ssh, .. }) => ssh_target(ssh),
            Some(_) => bail!("destination machine is not an SSH host"),
            None => SshTarget {
                destination: args.destination.clone(),
                ssh_args: Vec::new(),
            },
        };
        ensure!(
            !dest.destination.starts_with('-'),
            "SSH destination cannot start with '-'"
        );
        dest.ssh_args.extend(args.ssh_arg.clone());
        let mut argv = vec![args.remote_command.clone()];
        if let Some(instance) = &args.dest_instance {
            argv.extend(["--instance".into(), instance.clone()]);
        }
        argv.push("migrate-receive".into());
        if let Some(path) = &args.dest_data_dir {
            argv.extend([
                "--data-dir".into(),
                path.to_str()
                    .context("destination data path is not UTF-8")?
                    .into(),
            ]);
        }
        if let Some(path) = &args.dest_config_dir {
            argv.extend([
                "--config-dir".into(),
                path.to_str()
                    .context("destination config path is not UTF-8")?
                    .into(),
            ]);
        }
        let mut command = tokio::process::Command::new("ssh");
        command.args([
            "-T",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "BatchMode=yes",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
        ]);
        if args.dry_run {
            // A dry run must not accept or update keys in the caller's home.
            command.args(["-o", "StrictHostKeyChecking=yes", "-o", "UpdateHostKeys=no"]);
            eprintln!("Dry run uses existing SSH host keys and does not update them.");
        }
        command
            .args(&dest.ssh_args)
            .arg(&dest.destination)
            .arg(remote_command_line(&argv));
        let mut child = mj_core::subprocess::SupervisedPipeChild::spawn(&mut command)?;
        let stream = MigrationStream::new(
            child.take_stdout().context("SSH stdout")?,
            child.take_stdin().context("SSH stdin")?,
        );
        let result = run_stream(
            &args,
            config,
            data,
            config_layout,
            &dest.destination,
            initial_config_sha256,
            stream,
        )
        .await;
        let status = child.wait().await;
        let stderr = String::from_utf8_lossy(&child.stderr()).trim().to_owned();
        if !stderr.is_empty() {
            eprintln!("{stderr}");
        }
        result?;
        ensure!(status?.success(), "SSH receiver exited unsuccessfully");
        Ok(())
    }

    type ProbeRequest = (PathBuf, tokio::sync::oneshot::Sender<Result<bool>>);

    async fn run_stream<R, W>(
        args: &MigrateArgs,
        config: Config,
        data: DataLayout,
        config_layout: ConfigLayout,
        destination_ssh: &str,
        initial_config_sha256: Option<[u8; 32]>,
        mut stream: MigrationStream<R, W>,
    ) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let configured_destination = config.machines.contains_key(args.destination.as_str());
        let hello = stream.hello().await?;
        let fence = database::read_configured_store_fence()?;
        let resuming = match (&fence, &hello.destination_status) {
            (Some(f), DestinationStatus::ResumablePublication { expected_fence })
                if f.matches_snapshot(expected_fence) =>
            {
                Some(f.clone())
            }
            (Some(f), _) => return Err(f.refusal(data.root()).into()),
            (None, DestinationStatus::ResumablePublication { .. }) => {
                eprintln!(
                    "Destination holds an unfinished migration attempt; it will be cleaned up before publishing."
                );
                None
            }
            _ => None,
        };
        let source_host = args
            .source_host
            .clone()
            .unwrap_or_else(mj_core::storage::local_host_name);
        ensure!(
            !source_host.starts_with('-'),
            "source SSH destination cannot start with '-'"
        );
        if args
            .source_host
            .as_deref()
            .and_then(|s| s.rsplit('@').next())
            .and_then(|s| s.parse::<std::net::IpAddr>().ok())
            .is_some()
        {
            eprintln!("Warning: --source-host is an IP address; use a stable SSH name if possible");
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ProbeRequest>();
        let source_config = config.clone();
        let old_data = data.root().to_path_buf();
        let old_config = config_layout.root().to_path_buf();
        let old_home = mj_core::path_input::user_home().context("locate source home")?;
        let new_home = hello.home_dir.clone();
        let new_data = hello.data_dir.clone();
        let new_config = hello.config_dir.clone();
        let inspect_host = source_host.clone();
        let inspect_old_home = old_home.clone();
        let inspect_old_config = old_config.clone();
        let mut inspect = mj_core::runtime::spawn_blocking(move || {
            let connection = open_db(&old_data.join("mj.sqlite3"))?;
            let cache = std::cell::RefCell::new(BTreeMap::new());
            inspect_preflight(
                &connection,
                &source_config,
                &old_data,
                &inspect_old_config,
                &inspect_old_home,
                &new_home,
                &new_data,
                &new_config,
                &inspect_host,
                |path| {
                    if let Some(value) = cache.borrow().get(path) {
                        return Ok(*value);
                    }
                    let (reply, response) = tokio::sync::oneshot::channel();
                    tx.send((path.to_owned(), reply))
                        .context("destination path probe stopped")?;
                    let value = response
                        .blocking_recv()
                        .context("destination path probe stopped")??;
                    cache.borrow_mut().insert(path.to_owned(), value);
                    Ok(value)
                },
            )
        });
        let preflight = loop {
            tokio::select! {
                result = &mut inspect => break result.context("join migration preflight")??,
                Some((path, reply)) = rx.recv() => {
                    let result = stream.probe_paths(vec![path.clone()]).await.and_then(|mut found| found.remove(&path).context("destination omitted path probe"));
                    let _ = reply.send(result);
                }
            }
        };
        ensure!(
            preflight.preconditions.blockers.is_empty(),
            "migration is blocked:\n{}",
            preflight.preconditions.blockers.join("\n")
        );
        let (local_templates, remote_templates) = template_sets(&config);
        let inventory = &preflight.inventory;
        let local_placements = inventory
            .session_targets_by_kind
            .iter()
            .any(|(kind, count)| {
                (kind.starts_with("local-") || kind == "apple-container") && *count > 0
            })
            || inventory.environment_workers_by_kind_and_phase.iter().any(
                |((kind, _, _), count)| {
                    (kind.starts_with("local-") || kind == "apple-container") && *count > 0
                },
            );
        let configured_local_templates = !local_templates.is_empty();
        ensure!(
            args.disconnect_local
                || args.source_host.is_some()
                || !(local_placements || configured_local_templates),
            "local sessions, environments or templates require --source-host; use --disconnect-local if retiring this host"
        );
        let migrate_wiki = if args.skip_sessionwiki {
            false
        } else if args.include_sessionwiki {
            ensure!(
                preflight.sessionwiki_db.exists(),
                "SessionWiki index does not exist: {}",
                preflight.sessionwiki_db.display()
            );
            true
        } else {
            mj_core::config::instance_name().is_none()
                && args.dest_instance.is_none()
                && preflight.sessionwiki_db.exists()
        };
        ensure!(
            !migrate_wiki
                || hello.sessionwiki_status != SessionWikiStatus::Existing
                || args.overwrite_sessionwiki,
            "destination SessionWiki already exists; use --overwrite-sessionwiki or --skip-sessionwiki"
        );
        let (source_user, source_name) = source_host
            .rsplit_once('@')
            .map_or((None, source_host.as_str()), |(user, host)| {
                (Some(user.to_owned()), host)
            });
        ensure!(
            !source_name.is_empty()
                && !source_name.starts_with('-')
                && !source_name.chars().any(char::is_whitespace)
                && source_user
                    .as_ref()
                    .is_none_or(|user| !user.is_empty() && !user.contains('@')),
            "invalid --source-host SSH destination"
        );
        let source_ssh = SshConnection {
            host: source_name.to_owned(),
            user: source_user,
            identity_file: None,
            extra_args: args.source_ssh_arg.clone(),
        };
        let machine_id = args
            .source_machine_id
            .as_ref()
            .map_or_else(default_machine_id, |id| MachineId::new(id.clone()))?;
        ensure!(!machine_id.is_local(), "source machine ID cannot be local");
        ensure!(
            !config.machines.contains_key(&machine_id),
            "source machine ID {machine_id} already exists; choose --source-machine-id"
        );
        ensure!(
            !config
                .machines
                .values()
                .any(|machine| matches!(machine, Machine::Ssh { ssh, .. } if ssh == &source_ssh)),
            "the source SSH connection is already configured"
        );
        let mut engines = Vec::new();
        for (kind, engine) in [("local-podman", "podman"), ("local-docker", "docker")] {
            if inventory
                .session_targets_by_kind
                .get(kind)
                .copied()
                .unwrap_or(0)
                > 0
                || inventory
                    .environment_workers_by_kind_and_phase
                    .iter()
                    .any(|((k, _, _), n)| k == kind && *n > 0)
            {
                engines.push(engine.to_owned());
            }
        }
        let source = if !args.disconnect_local && (local_placements || args.source_host.is_some()) {
            Some(SourceProbe {
                ssh: ssh_target(&source_ssh),
                data_root: data.root().to_owned(),
                expected_identity: std::fs::read_to_string(data.instance_identity())?
                    .trim()
                    .to_owned(),
                engines,
            })
        } else {
            None
        };
        let mut third_hosts = Vec::new();
        for (id, machine) in &config.machines {
            if configured_destination && id.as_str() == args.destination {
                continue;
            }
            if let Machine::Ssh { ssh, .. } = machine {
                if copied_ssh_credentials(ssh, &old_config, &old_home)? {
                    eprintln!(
                        "Skipping preflight SSH probe for machine {id}: its SSH credentials will be copied during migration"
                    );
                } else {
                    third_hosts.push(ssh_target(ssh));
                }
            }
        }
        let probe = stream
            .probe_ssh(ProbeSshRequest {
                read_only: args.dry_run,
                source,
                third_hosts,
            })
            .await?;
        ensure!(
            probe.failures.is_empty(),
            "destination SSH probes failed:\n{}",
            probe.failures.join("\n")
        );
        let destination_ip = destination_ssh
            .rsplit('@')
            .next()
            .and_then(|host| host.parse::<std::net::Ipv4Addr>().ok())
            .filter(|ip| !ip.is_private() && !ip.is_loopback() && !ip.is_link_local());
        if let Some(ip) = destination_ip {
            for resource in &preflight.ec2_resources {
                let check_resource = resource.clone();
                let check = mj_core::runtime::spawn_blocking(move || {
                    mj_controller::migrate::preflight::check_ec2_ingress(
                        &Default::default(),
                        &check_resource,
                        ip,
                        false,
                    )
                })
                .await
                .context("join EC2 ingress check")?;
                let check = match check {
                    Ok(check) => check,
                    Err(error) => {
                        eprintln!(
                            "Warning: could not check EC2 SSH ingress for {} in {} ({error:#}); verify that {ip}/32 is allowed before proceeding",
                            resource.instance_id, resource.region
                        );
                        continue;
                    }
                };
                if !check.admitted {
                    eprintln!(
                        "Warning: EC2 instance {} does not allow SSH from {ip}",
                        check.instance_id
                    );
                    use std::io::{IsTerminal, Write};
                    if !args.dry_run && !args.no_aws_modify && std::io::stdin().is_terminal() {
                        eprint!("Authorize SSH ingress for this instance? [y/N] ");
                        std::io::stderr().flush()?;
                        let mut answer = String::new();
                        std::io::stdin().read_line(&mut answer)?;
                        if answer.trim().eq_ignore_ascii_case("y")
                            || answer.trim().eq_ignore_ascii_case("yes")
                        {
                            let resource = preflight
                                .ec2_resources
                                .iter()
                                .find(|r| r.instance_id == check.instance_id)
                                .context("EC2 resource disappeared")?
                                .clone();
                            mj_core::runtime::spawn_blocking(move || {
                                mj_controller::migrate::preflight::check_ec2_ingress(
                                    &Default::default(),
                                    &resource,
                                    ip,
                                    true,
                                )
                            })
                            .await
                            .context("join EC2 ingress authorization")??;
                        }
                    }
                }
            }
        } else {
            for resource in &preflight.ec2_resources {
                eprintln!(
                    "Warning: destination public IPv4 is unknown; check EC2 SSH ingress for {} in {}",
                    resource.instance_id, resource.region
                );
            }
        }
        let data_size: u64 = preflight
            .data_entries
            .iter()
            .filter(|entry| {
                (args.include_logs
                    || entry.classification
                        != mj_core::config::Classification::Known(
                            mj_core::config::MigrateAction::CopyOptional,
                        ))
                    && !(!migrate_wiki
                        && preflight.sessionwiki_db == data.root().join("sessionwiki/index.db")
                        && entry.path == Path::new("sessionwiki"))
            })
            .map(|entry| entry.size_bytes)
            .sum();
        let config_size: u64 = preflight
            .config_entries
            .iter()
            .filter(|entry| {
                args.include_logs
                    || entry.classification
                        != mj_core::config::Classification::Known(
                            mj_core::config::MigrateAction::CopyOptional,
                        )
            })
            .map(|entry| entry.size_bytes)
            .sum();
        if let Some(available) = hello.available_bytes_data {
            ensure!(
                available >= data_size,
                "destination data disk has {available} bytes available, estimated {data_size} required"
            );
        }
        if let Some(available) = hello.available_bytes_config {
            ensure!(
                available >= config_size,
                "destination config disk has {available} bytes available, estimated {config_size} required"
            );
        }
        if migrate_wiki && let Some(available) = hello.available_bytes_sessionwiki {
            let size = std::fs::metadata(&preflight.sessionwiki_db)?.len();
            ensure!(
                available >= size,
                "destination SessionWiki disk has {available} bytes available, estimated {size} required"
            );
        }
        eprintln!(
            "Destination data: {}\nDestination config: {}\nDestination home: {}\nSessionWiki: {}",
            hello.data_dir.display(),
            hello.config_dir.display(),
            hello.home_dir.display(),
            if migrate_wiki { "migrate" } else { "skip" }
        );
        eprintln!("Estimated data: {data_size} bytes; configuration: {config_size} bytes");
        for copy in &preflight.home_copies {
            eprintln!(
                "Copy home path: {} -> {}",
                copy.source.display(),
                copy.destination.display()
            );
        }
        for warning in &preflight.warnings {
            eprintln!("Warning: {warning}");
        }
        if args.dry_run {
            stream.done().await?;
            eprintln!("Dry run complete");
            return Ok(());
        }
        let executable = std::env::current_exe().context("locate migration pin helper")?;
        let plan = SenderPlan {
            data_dir: data.root().to_owned(),
            config_dir: config_layout.root().to_owned(),
            sessionwiki_db: preflight.sessionwiki_db.clone(),
            home_copies: preflight.home_copies.clone(),
            dereference_config_paths: preflight.dereference_config_paths.clone(),
            include_logs: args.include_logs,
            migrate_sessionwiki: migrate_wiki,
            warm_base_pass: true,
            pin_command: Box::new(move |path| {
                let mut command = tokio::process::Command::new(&executable);
                command.arg("migrate-pin").arg("--db").arg(path);
                command
            }),
        };
        // Move the buffered stream out without rebuilding either buffer.
        let mut sender = stream.into_sender(plan);
        let mut hooks = Hooks {
            config,
            data,
            config_layout,
            initial_config_sha256,
            source_host: source_ssh,
            machine_id,
            destination_label: args.destination.clone(),
            local_templates,
            remote_templates,
            migrate_wiki,
            overwrite_wiki: args.overwrite_sessionwiki,
            no_start: args.no_start,
            resuming,
            wrote_fence: false,
            stopped_daemon: false,
            start_guard: None,
            store_guard: None,
            worker_roots: Vec::new(),
        };
        let phase = sender.phase1_with_hello(hello).await?;
        #[cfg(feature = "test-hooks")]
        if mj_core::test_hooks::test_hooks_enabled() {
            mj_core::runtime::spawn_blocking(|| {
                mj_core::test_hooks::reach_test_hook("migrate_after_phase1")
            })
            .await
            .context("join migration test hook")??;
        }
        let outcome = sender.phase2(phase, &mut hooks).await?;
        let summary = &outcome.summary;
        eprintln!(
            "Migration activated: {} sessions, {} environments, {} targets, {} environment workers, {} moves and {} pinned memory keys rewritten; {} home paths and {} data entries copied; SessionWiki {}; destination daemon {}",
            summary.store.sessions,
            summary.store.environments,
            summary.store.session_targets,
            summary.store.environment_workers,
            summary.store.moves,
            summary.store.pinned_memory_keys,
            summary.home_targets,
            summary.data_entries,
            if summary.sessionwiki {
                "migrated"
            } else {
                "skipped"
            },
            if outcome.daemon_started {
                "started"
            } else {
                "not started"
            }
        );
        for warning in &summary.warnings {
            eprintln!("Warning: {warning}");
        }
        Ok(())
    }

    struct Hooks {
        config: Config,
        data: DataLayout,
        config_layout: ConfigLayout,
        initial_config_sha256: Option<[u8; 32]>,
        source_host: SshConnection,
        machine_id: MachineId,
        destination_label: String,
        local_templates: BTreeMap<String, BTreeSet<String>>,
        remote_templates: BTreeSet<String>,
        migrate_wiki: bool,
        overwrite_wiki: bool,
        no_start: bool,
        resuming: Option<StoreFenceRecord>,
        wrote_fence: bool,
        stopped_daemon: bool,
        start_guard: Option<crate::daemon::DaemonStartGuard>,
        store_guard: Option<ControllerStoreGuard>,
        worker_roots: Vec<PathBuf>,
    }

    impl CutoverHooks for Hooks {
        async fn stop_and_lock(&mut self) -> Result<()> {
            let guard = crate::daemon::acquire_start_guard(self.data.daemon_start_lock()).await?;
            self.start_guard = Some(guard);
            if self.resuming.is_none() {
                self.stopped_daemon =
                    crate::daemon::stop_instance_holding(self.start_guard.as_ref().unwrap(), false)
                        .await?;
            }
            self.store_guard = Some(
                mj_core::runtime::spawn_blocking(ControllerStoreGuard::acquire)
                    .await
                    .context("join controller lock")??,
            );
            Ok(())
        }
        async fn recheck(&mut self, pinned_user_version: i64) -> Result<()> {
            ensure!(
                config_fingerprint(&self.config_layout.config_toml())?
                    == self.initial_config_sha256,
                "config.toml changed during Phase 1 pre-sync; re-run mj migrate so preflight and copied files match the updated configuration"
            );
            ensure!(
                !self.data.config_rename_journal().exists(),
                "finish the pending configuration rename before migrating"
            );
            let connection = open_db(&self.data.database())?;
            let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            ensure!(
                version == pinned_user_version && version == database::STORE_SCHEMA_VERSION,
                "source store schema changed during migration"
            );
            let inventory = database::inspect_migration_inventory(&connection)?;
            let remote_templates = self
                .remote_templates
                .union(&inventory.remote_template_ids)
                .cloned()
                .collect();
            let check = database::inspect_migration_preconditions_with_templates(
                &connection,
                self.data.root(),
                &self.local_templates,
                &remote_templates,
            )?;
            ensure!(
                check.blockers.is_empty(),
                "migration is blocked at cutover:\n{}",
                check.blockers.join("\n")
            );
            Ok(())
        }
        async fn fence(&mut self) -> Result<StoreFenceRecord> {
            if let Some(expected) = &self.resuming {
                let actual =
                    database::read_configured_store_fence()?.context("source fence disappeared")?;
                ensure!(
                    actual.matches_snapshot(expected),
                    "source fence changed during migration"
                );
                return Ok(actual);
            }
            ensure!(
                database::read_configured_store_fence()?.is_none(),
                "source became fenced during cutover"
            );
            let (fence, previous) =
                database::fence_configured_store(&self.destination_label, Some("mj migrate"))?;
            self.wrote_fence = true;
            ensure!(previous.is_none(), "source became fenced during cutover");
            Ok(fence)
        }
        async fn clear_fence(&mut self) -> Result<()> {
            if self.wrote_fence {
                database::unfence_configured_store()?;
                self.wrote_fence = false;
            }
            Ok(())
        }
        async fn build_commit(
            &mut self,
            fence: &StoreFenceRecord,
            destination: &HelloAck,
            home_targets: &[HomeTarget],
        ) -> Result<CommitPublication> {
            let memory_keys = mj_controller::migrate::capture_fenced_pinned_memory_keys(
                &self.data.database(),
                fence,
                &self.config,
            )?;
            let connection = open_db(&self.data.database())?;
            let inventory = database::inspect_migration_inventory(&connection)?;
            let remote_templates = self
                .remote_templates
                .union(&inventory.remote_template_ids)
                .cloned()
                .collect();
            let mut roots = connection.prepare(
                "SELECT session_id,workspace FROM session_targets WHERE kind='local-bare' \
                 UNION SELECT worker_session_id,workspace FROM environment_workers WHERE kind='local-bare'",
            )?;
            self.worker_roots = roots
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
                })?
                .map(|row| {
                    row.map(|(id, workspace)| {
                        workspace.map_or_else(
                            || self.data.worker_root(&id),
                            |path| PathBuf::from(OsStr::from_bytes(&path)),
                        )
                    })
                })
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let home = mj_core::path_input::user_home().context("locate source home")?;
            Ok(CommitPublication {
                rewrite_spec: database::MigrationRewriteSpec {
                    expected_fence: fence.clone(),
                    old_data_root: self.data.root().to_owned(),
                    new_data_root: destination.data_dir.clone(),
                    old_config_root: self.config_layout.root().to_owned(),
                    new_config_root: destination.config_dir.clone(),
                    old_home: home.clone(),
                    new_home: destination.home_dir.clone(),
                    remote_template_ids: remote_templates,
                    source_ssh: self.source_host.clone(),
                    source_machine_id: self.machine_id.clone(),
                    source_templates: BTreeMap::new(),
                    pinned_memory_keys: memory_keys,
                    staged_sessions_dir: PathBuf::new(),
                },
                config_rewrite_spec: mj_controller::migrate::rewrite::StagedConfigRewriteSpec {
                    old_config_root: self.config_layout.root().to_owned(),
                    new_config_root: destination.config_dir.clone(),
                    old_home: home,
                    new_home: destination.home_dir.clone(),
                    source_machine_id: self.machine_id.clone(),
                    source_ssh: self.source_host.clone(),
                    historical_local_templates: inventory.local_template_kinds,
                },
                home_targets: home_targets.to_vec(),
                migrate_sessionwiki: self.migrate_wiki,
                overwrite_sessionwiki: self.overwrite_wiki,
                start_daemon: !self.no_start,
            })
        }
        async fn rollback(&mut self) -> Result<()> {
            self.store_guard.take();
            let result = if self.stopped_daemon && self.resuming.is_none() {
                crate::daemon::connect_or_start_holding(
                    self.start_guard
                        .as_ref()
                        .context("missing daemon start guard")?,
                )
                .await
                .map(|_| ())
            } else {
                Ok(())
            };
            self.start_guard.take();
            result
        }
        async fn activated(&mut self, _summary: &PublicationSummary) -> Result<()> {
            for root in &self.worker_roots {
                if let Err(error) = mj_controller::migrate::rewrite::detach_codex_auth_symlink(root)
                {
                    eprintln!(
                        "Warning: could not detach Codex login at {}: {error:#}",
                        root.display()
                    );
                }
            }
            self.store_guard.take();
            self.start_guard.take();
            Ok(())
        }
    }
}
