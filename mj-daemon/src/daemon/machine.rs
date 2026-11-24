use super::terminal_operations::process_probe;
use super::*;
use crate::server::api::MachinesOperations;
use mj_client::daemon::PreflightNew;
use mj_client::discovery::SetupDetection;
use mj_client::http::machines::*;
use mj_client::session::BoxFuture;

impl MachinesOperations for RuntimeState {
    fn machine_preview_build_cache(
        self: Arc<Self>,
        request: PreviewBuildCacheRequest,
    ) -> BoxFuture<'static, Result<BuildCacheAvailability>> {
        Box::pin(async move {
            let PreviewBuildCacheRequest { machine } = request;
            process_probe(Some(Duration::from_secs(60)), move |executor| {
                let preview = mj_controller::controller::preview_build_cache(&machine, executor)?;
                let available = preview.as_ref().is_some_and(|preview| {
                    mj_controller::controller::mbx_install_kind(preview).is_some()
                });
                Ok((preview, available))
            })
            .await
        })
    }

    fn machine_install_mbx(
        self: Arc<Self>,
        request: InstallMbxRequest,
    ) -> BoxFuture<'static, Result<String>> {
        Box::pin(async move {
            let InstallMbxRequest { machine } = request;
            process_probe(None, move |executor| {
                let result = mj_controller::controller::install_mbx(&machine, executor)?;
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
                Ok(message)
            }).await
        })
    }

    fn machine_validate_project_directory(
        self: Arc<Self>,
        request: ValidateProjectDirectoryRequest,
    ) -> BoxFuture<'static, Result<PreflightNew>> {
        Box::pin(async move {
            let ValidateProjectDirectoryRequest {
                target_id,
                directory,
            } = request;
            let config = self.with_config(Clone::clone);
            process_probe(Some(Duration::from_secs(30)), move |executor| {
                crate::server_runtime::preflight::run_new_preflight_with_executor(
                    config,
                    String::new(),
                    target_id,
                    Some(directory),
                    executor,
                )
            })
            .await
        })
    }

    fn machine_checkpoint_archive_sizes(
        self: Arc<Self>,
        request: CheckpointArchiveSizesRequest,
    ) -> BoxFuture<'static, Result<ArchiveSizes>> {
        Box::pin(async move {
            let CheckpointArchiveSizesRequest { paths } = request;
            process_probe(None, move |_| {
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
                Ok(sizes)
            })
            .await
        })
    }

    fn machine_test_target(
        self: Arc<Self>,
        request: TestTargetRequest,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let TestTargetRequest { target_id } = request;
            let config = self.with_config(Clone::clone);
            process_probe(Some(Duration::from_secs(15)), move |executor| {
                mj_controller::controller::config_only_controller(config)
                    .test_target(&target_id, executor)
            })
            .await
        })
    }

    fn machine_profile_login_statuses(
        self: Arc<Self>,
    ) -> BoxFuture<'static, Result<ProfileLoginStatuses>> {
        Box::pin(async move {
            let config = self.with_config(Clone::clone);
            process_probe(None, move |executor| {
                Ok(mj_controller::profile_login::statuses(
                    &config,
                    executor.cancellation_flag(),
                ))
            })
            .await
        })
    }

    fn machine_discover_setup(
        self: Arc<Self>,
        request: DiscoverSetupRequest,
    ) -> BoxFuture<'static, Result<SetupDetection>> {
        Box::pin(async move {
            let DiscoverSetupRequest { scope, machines } = request;
            process_probe(Some(Duration::from_secs(30)), move |executor| {
                use mj_setup::setup::{self, DEFAULT_IMAGE};
                let mut config = Config::default();
                let mut rejected_runtimes = Vec::new();
                let mut machine_probes = Vec::new();
                match scope {
                    mj_client::discovery::DetectScope::Profiles => {
                        config = setup::profiles_config(&setup::discover_profiles(executor));
                        setup::reference_secret_api_keys(
                            &mut config,
                            &mj_core::config::config_path(),
                        );
                    }
                    mj_client::discovery::DetectScope::Runtimes => {
                        // The machines are other hosts, probed while this one is.
                        let (local, machines) = std::thread::scope(|scope| {
                            let machines =
                                scope.spawn(|| setup::probe_machines(&machines, executor));
                            let local = setup::discover_runtimes(executor);
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
                            mj_setup::setup::profile_presence(id, profile, executor)
                                .err()
                                .map(|failure| failure.detail),
                        )
                    })
                    .collect();
                Ok(mj_client::discovery::SetupDetection {
                    profile_readiness,
                    scope,
                    config,
                    rejected_runtimes,
                    machines: machine_probes,
                })
            })
            .await
        })
    }

    fn machine_check_project_directories(
        self: Arc<Self>,
        request: CheckProjectDirectoriesRequest,
    ) -> BoxFuture<'static, Result<Vec<(PathBuf, bool)>>> {
        Box::pin(async move {
            let CheckProjectDirectoriesRequest { paths } = request;
            process_probe(None, move |_| {
                paths
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
            })
            .await
        })
    }

    fn machine_resolve_container_path(
        self: Arc<Self>,
        request: ResolveContainerPathRequest,
    ) -> BoxFuture<'static, Result<PathBuf>> {
        Box::pin(async move {
            let ResolveContainerPathRequest { target_id, source } = request;
            let config = self.with_config(Clone::clone);
            process_probe(Some(Duration::from_secs(30)), move |executor| {
                mj_controller::controller::config_only_controller(config)
                    .resolve_input_path(&target_id, &source, executor)
            })
            .await
        })
    }

    fn machine_resolve_setup_path(
        self: Arc<Self>,
        request: ResolveSetupPathRequest,
    ) -> BoxFuture<'static, Result<PathBuf>> {
        Box::pin(async move {
            let ResolveSetupPathRequest { machine, source } = request;
            process_probe(Some(Duration::from_secs(30)), move |executor| {
                mj_controller::controller::resolve_machine_input_path(&machine, &source, executor)
            })
            .await
        })
    }

    fn machine_validate_mount_source(
        self: Arc<Self>,
        request: ValidateMountSourceRequest,
    ) -> BoxFuture<'static, Result<MountValidation>> {
        Box::pin(async move {
            let ValidateMountSourceRequest { target_id, source } = request;
            let config = self.with_config(Clone::clone);
            process_probe(Some(Duration::from_secs(30)), move |executor| {
                let controller = mj_controller::controller::config_only_controller(config);
                let path = controller.resolve_input_path(&target_id, &source, executor)?;
                let forced = controller.validate_mount_source(&target_id, &path, executor)?;
                Ok((path, forced))
            })
            .await
        })
    }

    fn machine_validate_session_mounts(
        self: Arc<Self>,
        request: ValidateSessionMountsRequest,
    ) -> BoxFuture<'static, Result<SessionMountValidation>> {
        Box::pin(async move {
            let ValidateSessionMountsRequest { target_id, mounts } = request;
            let config = self.with_config(Clone::clone);
            process_probe(None, move |executor| {
                let controller = mj_controller::controller::config_only_controller(config);
                for mount in mounts {
                    if let Err(error) =
                        controller.validate_mount_source(&target_id, &mount.source, executor)
                    {
                        return Ok(Some((
                            mount.source.to_string_lossy().into_owned(),
                            format!("{error:#}"),
                        )));
                    }
                }
                Ok(None)
            })
            .await
        })
    }
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
