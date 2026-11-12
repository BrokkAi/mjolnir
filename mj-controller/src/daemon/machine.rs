//! Machine reads belong to the daemon, including for host placement.
use super::*;

pub(super) async fn execute(
    action: DaemonAction,
    state: &RuntimeState,
    cancelled: Arc<AtomicBool>,
) -> Result<DaemonReply> {
    let config = state.with_config(Clone::clone);
    tokio::task::spawn_blocking(move || {
        let executor = CancellableProcessExecutor::new(cancelled);
        // Preserve the dashboard's existing command deadlines, including the
        // bound on preflight when it applies confirmed Git repairs.
        let executor = match &action {
            DaemonAction::CompletePath { .. } => executor.with_deadline(Duration::from_secs(5)),
            DaemonAction::DiscoverProjects { .. } => {
                executor.with_deadline(Duration::from_secs(15))
            }
            DaemonAction::ResolveContainerPath { .. }
            | DaemonAction::ResolveSetupPath { .. }
            | DaemonAction::ValidateMountSource { .. }
            | DaemonAction::ValidateProjectDirectory { .. }
            | DaemonAction::PreflightNewSession { .. } => {
                executor.with_deadline(Duration::from_secs(30))
            }
            _ => executor,
        };
        let controller = crate::controller::config_only_controller(config);
        match action {
            DaemonAction::PreflightNewSession { request } => {
                crate::server_runtime::preflight::preflight_new_request(
                    controller.config,
                    request,
                    executor,
                )
                .map(DaemonReply::NewSessionPreflight)
            }
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
            DaemonAction::CompletePath { host, prefix, kind } => controller
                .complete_path(&host, &prefix, kind, &executor)
                .map(DaemonReply::PathCompletion),
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
            DaemonAction::DiscoverProjects { request } => {
                crate::project_picker::discover(&request, &executor)
                    .map(DaemonReply::ProjectDiscovery)
            }
            DaemonAction::ResolveContainerPath { target_id, source } => controller
                .resolve_input_path(&target_id, &source, &executor)
                .map(DaemonReply::ResolvedPath),
            DaemonAction::ResolveSetupPath { machine, source } => {
                crate::controller::resolve_machine_input_path(&machine, &source, &executor)
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
    .context("machine request task panicked")?
}
