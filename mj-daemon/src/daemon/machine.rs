//! Machine reads belong to the daemon, including for host placement.
use super::*;

pub(super) async fn execute(
    action: DaemonAction,
    state: &RuntimeState,
    cancelled: Arc<AtomicBool>,
) -> Result<DaemonReply> {
    let config = state.with_config(Clone::clone);
    let install_machine = match &action {
        DaemonAction::InstallMbx { machine } => Some(machine.clone()),
        _ => None,
    };
    let reply = mj_core::runtime::spawn_blocking(move || {
        let quit = cancelled.clone();
        let executor = CancellableProcessExecutor::new(cancelled);
        // Preserve the dashboard's deadlines for host commands.
        let executor = match &action {
            DaemonAction::PreviewBuildCache { .. } => executor.with_deadline(Duration::from_secs(60)),
            DaemonAction::TestTarget { .. } => executor.with_deadline(Duration::from_secs(15)),
            DaemonAction::DiscoverSetup { .. } => executor.with_deadline(Duration::from_secs(30)),
            DaemonAction::ResolveContainerPath { .. }
            | DaemonAction::ResolveSetupPath { .. }
            | DaemonAction::ValidateMountSource { .. }
            | DaemonAction::ValidateProjectDirectory { .. } => {
                executor.with_deadline(Duration::from_secs(30))
            }
            _ => executor,
        };
        let controller = mj_controller::controller::config_only_controller(config);
        match action {
            DaemonAction::PreviewBuildCache { machine } => {
                let preview = mj_controller::controller::preview_build_cache(&machine, &executor)?;
                let available = preview.as_ref().is_some_and(|preview| {
                    mj_controller::controller::mbx_install_kind(preview).is_some()
                });
                Ok(DaemonReply::BuildCachePreview(preview, available))
            }
            DaemonAction::InstallMbx { machine } => {
                let result = mj_controller::controller::install_mbx(&machine, &executor)?;
                let verb = match result.kind {
                    mj_controller::controller::MbxInstallKind::Install => "Installed",
                    mj_controller::controller::MbxInstallKind::Upgrade => "Upgraded",
                };
                let profile = if result.profile_changed {
                    format!("updated {}", result.profile_file)
                } else {
                    format!(
                        "left {} unchanged because its marked PATH block already exists",
                        result.profile_file
                    )
                };
                let mut message = format!(
                    "{verb} mbx at {} (version {}). {profile}. New login shells will pick up the PATH change.",
                    result.program.display(),
                    result.version
                );
                if let Some(warning) = result.profile_warning {
                    message.push(' ');
                    message.push_str(&warning);
                }
                if let Some(path_line) = result.manual_path_line {
                    message.push(' ');
                    message.push_str(&path_line);
                }
                Ok(DaemonReply::MbxInstalled(message))
            }

            DaemonAction::TestTarget { target_id } => {
                controller.test_target(&target_id, &executor)?;
                Ok(DaemonReply::Done)
            }
            DaemonAction::CheckpointArchiveSizes { paths } => {
                let sizes = paths
                    .into_iter()
                    .map(|(id, path)| {
                        let size = match fs::metadata(&path) {
                            Ok(metadata) => Ok(Some(metadata.len())),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                            Err(error) => Err(format!("read {}: {error}", path.display())),
                        };
                        (id, size)
                    })
                    .collect();
                Ok(DaemonReply::CheckpointArchiveSizes(sizes))
            }
            DaemonAction::DiscoverSetup { scope, machines } => {
                use mj_setup::setup::{self, DEFAULT_IMAGE};
                let mut config = Config::default();
                let mut rejected_runtimes = Vec::new();
                let mut machine_probes = Vec::new();
                match scope {
                    mj_client::discovery::DetectScope::Profiles => {
                        config = setup::profiles_config(&setup::discover_profiles(&executor));
                        setup::reference_secret_api_keys(
                            &mut config,
                            &mj_core::config::config_path(),
                        );
                    }
                    mj_client::discovery::DetectScope::Runtimes => {
                        // The machines are other hosts, probed while this one is.
                        let (local, machines) = std::thread::scope(|scope| {
                            let machines =
                                scope.spawn(|| setup::probe_machines(&machines, &executor));
                            let local = setup::discover_runtimes(&executor);
                            let machines = machines
                                .join()
                                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                            (local, machines)
                        });
                        machine_probes = machines.into_iter().map(machine_runtime_probe).collect();
                        for runtime in local {
                            if runtime.usable() {
                                let (id, target) =
                                    setup::local_runtime_target(runtime.kind, DEFAULT_IMAGE);
                                config.targets.insert(id.to_owned(), target);
                            } else {
                                rejected_runtimes.push(mj_client::settings::RejectedRuntime {
                                    kind: runtime.kind.name().to_owned(),
                                    label: runtime.kind.label().to_owned(),
                                    detail: runtime.detail,
                                    remediation: runtime.remediation,
                                });
                            }
                        }
                        #[cfg(unix)]
                        config.targets.insert(
                            "localhost".into(),
                            mj_core::config::TargetTemplate::LocalBare,
                        );
                    }
                }
                let profile_readiness = config
                    .profiles
                    .iter()
                    .map(|(id, profile)| {
                        (
                            id.clone(),
                            mj_setup::setup::profile_presence(id, profile, &executor)
                                .err()
                                .map(|failure| failure.detail),
                        )
                    })
                    .collect();
                Ok(DaemonReply::SetupDiscovery(Box::new(
                    mj_client::discovery::SetupDetection {
                        profile_readiness,
                        scope,
                        config,
                        rejected_runtimes,
                        machines: machine_probes,
                    },
                )))
            }
            DaemonAction::ProfileLoginStatuses => Ok(DaemonReply::ProfileLoginStatuses(
                mj_controller::profile_login::statuses(&controller.config, quit.clone()),
            )),
            DaemonAction::ValidateProjectDirectory {
                target_id,
                directory,
            } => crate::server_runtime::preflight::run_new_preflight_with_executor(
                controller.config,
                String::new(),
                target_id,
                Some(directory),
                &executor,
            )
            .map(DaemonReply::NewSessionPreflight),
            DaemonAction::CheckProjectDirectories { paths } => paths
                .into_iter()
                .map(|path| {
                    let exists = match fs::metadata(&path) {
                        Ok(metadata) => metadata.is_dir(),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                        Err(error) => {
                            return Err(anyhow::Error::new(error)
                                .context(format!("read {}", path.display())));
                        }
                    };
                    Ok((path, exists))
                })
                .collect::<Result<Vec<_>>>()
                .map(DaemonReply::ProjectDirectories),
            DaemonAction::ResolveContainerPath { target_id, source } => controller
                .resolve_input_path(&target_id, &source, &executor)
                .map(DaemonReply::ResolvedPath),
            DaemonAction::ResolveSetupPath { machine, source } => {
                mj_controller::controller::resolve_machine_input_path(&machine, &source, &executor)
                    .map(DaemonReply::ResolvedPath)
            }
            DaemonAction::ValidateMountSource { target_id, source } => {
                let path = controller.resolve_input_path(&target_id, &source, &executor)?;
                let forced = controller.validate_mount_source(&target_id, &path, &executor)?;
                Ok(DaemonReply::MountValidation(path, forced))
            }
            DaemonAction::ValidateSessionMounts { target_id, mounts } => {
                for mount in mounts {
                    if let Err(error) =
                        controller.validate_mount_source(&target_id, &mount.source, &executor)
                    {
                        return Ok(DaemonReply::SessionMountValidation(Some((
                            mount.source.to_string_lossy().into_owned(),
                            format!("{error:#}"),
                        ))));
                    }
                }
                Ok(DaemonReply::SessionMountValidation(None))
            }
            _ => bail!("action is not a machine read"),
        }
    })
    .await
    .context("machine request task panicked")??;
    if let Some(machine) = install_machine {
        state.notify_mbx_installed(&machine);
    }
    Ok(reply)
}

fn machine_runtime_probe(
    probe: mj_setup::setup::MachineProbe,
) -> mj_client::availability::MachineRuntimeProbe {
    use mj_setup::setup::MachineProbeOutcome;
    let finding = match &probe.outcome {
        MachineProbeOutcome::Answered(_) => mj_client::availability::MachineFinding::Answered {
            runtimes: probe.runtimes(),
            rejected: probe
                .rejected()
                .map(|runtime| mj_client::settings::RejectedRuntime {
                    kind: runtime.kind.name().to_owned(),
                    label: runtime.kind.label().to_owned(),
                    detail: runtime.detail.clone(),
                    remediation: runtime.remediation.clone(),
                })
                .collect(),
        },
        MachineProbeOutcome::Unreachable {
            detail,
            remediation,
            answered_nothing,
        } => mj_client::availability::MachineFinding::Unreachable {
            detail: detail.clone(),
            remediation: remediation.clone(),
            answered_nothing: *answered_nothing,
        },
    };
    mj_client::availability::MachineRuntimeProbe {
        machine_id: probe.machine_id,
        machine: probe.machine,
        finding,
    }
}
