//! First-session preparation, executed by the supervised creation worker.

use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, bail, ensure};
use hel::hel_config::{HelConfig, TargetTemplate, is_bare_project_target, raw_project_context_id};
use hel::hel_targets::CancellableProcessExecutor;
use hel_tui::DashboardAction;
use mj_controller::hel_controller::{create_quick_bundle_in_config, local_project_repository};
use mj_controller::hel_doctor::{
    CheckStatus, PROBE_TIMEOUT, local_docker_runtime_check, local_podman_runtime_check,
};
use mj_controller::hel_setup::{DEFAULT_IMAGE, RuntimeKind, local_runtime_target};

pub(super) fn prepare_session_launch(
    profile_id: String,
    explicit_target: Option<String>,
    directory: PathBuf,
    cancelled: &Arc<AtomicBool>,
) -> Result<(HelConfig, DashboardAction)> {
    prepare_session_launch_at(
        &hel::hel_config::config_path(),
        profile_id,
        explicit_target,
        directory,
        cancelled,
    )
}

fn prepare_session_launch_at(
    config_path: &Path,
    profile_id: String,
    explicit_target: Option<String>,
    directory: PathBuf,
    cancelled: &Arc<AtomicBool>,
) -> Result<(HelConfig, DashboardAction)> {
    ensure!(!cancelled.load(Ordering::Acquire), "operation cancelled");
    let mut config = HelConfig::load_from(config_path)?;
    let (target_id, target) = if let Some(id) = explicit_target {
        let target = config
            .targets
            .get(&id)
            .with_context(|| format!("startup target {id:?} is not configured"))?
            .clone();
        (id, target)
    } else {
        let executor =
            || CancellableProcessExecutor::new(cancelled.clone()).with_deadline(PROBE_TIMEOUT);
        // A container's local source is a Git repository. Ordinary directories
        // take the local target, preserving the directory the user launched in.
        let repository = local_project_repository(&directory, &executor())?;
        let podman = repository.is_some()
            && local_podman_runtime_check(&executor()).status == CheckStatus::Ready;
        let docker = repository.is_some()
            && !podman
            && local_docker_runtime_check(&executor()).status == CheckStatus::Ready;
        automatic_target(&config, podman, docker)?
    };
    ensure!(!cancelled.load(Ordering::Acquire), "operation cancelled");
    if !config.targets.contains_key(&target_id) {
        // Resolve against a fresh configuration under its existing write lock.
        config = HelConfig::update_to(config_path, |fresh| {
            if let Some(existing) = fresh.targets.get(&target_id) {
                ensure!(
                    existing == &target,
                    "startup target {target_id:?} changed during preparation"
                );
            } else {
                fresh.targets.insert(target_id.clone(), target.clone());
            }
            Ok(())
        })?
        .0;
    }
    let bare = is_bare_project_target(&target);
    let bundle_id = if bare {
        raw_project_context_id(&directory.to_string_lossy())
    } else {
        let source = directory
            .to_str()
            .context("container project path is not UTF-8")?;
        let (updated, bundle_id) = HelConfig::update_to(config_path, |fresh| create_quick_bundle_in_config(fresh, source))
            .context("prepare the current repository for the startup target; use a local-bare startup target for a plain directory")?;
        config = updated;
        bundle_id
    };
    ensure!(!cancelled.load(Ordering::Acquire), "operation cancelled");
    Ok((
        config,
        DashboardAction::CreateSession {
            profile_id,
            target_template_id: target_id,
            bundle_id,
            project_directory: bare.then_some(directory),
            additional_mounts: Vec::new(),
            // Starting in a working directory means using its current contents.
            allow_dirty_local: true,
            resource_allocation: None,
        },
    ))
}

fn automatic_target(
    config: &HelConfig,
    podman: bool,
    docker: bool,
) -> Result<(String, TargetTemplate)> {
    let desired = if podman {
        local_runtime_target(RuntimeKind::Podman, DEFAULT_IMAGE)
    } else if docker {
        local_runtime_target(RuntimeKind::Docker, DEFAULT_IMAGE)
    } else if cfg!(unix) {
        ("localhost", TargetTemplate::LocalBare)
    } else {
        bail!(
            "No usable Podman or Docker runtime. Configure a startup target or press Alt-W to choose one."
        );
    };
    if let Some((id, target)) = config.targets.iter().find(|(_, target)| {
        matches!(
            (target, &desired.1),
            (
                TargetTemplate::LocalPodman { .. },
                TargetTemplate::LocalPodman { .. }
            ) | (
                TargetTemplate::LocalDocker { .. },
                TargetTemplate::LocalDocker { .. }
            ) | (TargetTemplate::LocalBare, TargetTemplate::LocalBare)
        )
    }) {
        return Ok((id.clone(), target.clone()));
    }
    // Keep custom targets whose names happen to match a runtime.
    let mut id = desired.0.to_owned();
    for suffix in 2_u32.. {
        if !config.targets.contains_key(&id) {
            break;
        }
        id = format!("{}-{suffix}", desired.0);
    }
    Ok((id, desired.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(directory: &Path) -> PathBuf {
        let path = directory.join("config.toml");
        let mut config = HelConfig::default();
        config.profiles.insert(
            "codex".into(),
            hel::hel_config::HarnessProfile {
                kind: hel::hel_config::HarnessKind::Codex,
                home: directory.join("codex"),
                environment: Default::default(),
                context_window_bytes: None,
            },
        );
        config.targets.insert(
            "custom-container".into(),
            local_runtime_target(RuntimeKind::Docker, "custom-image:latest").1,
        );
        config.save_to(&path).unwrap();
        path
    }

    #[test]
    fn explicit_container_startup_prepares_the_current_dirty_repository_and_reuses_its_bundle() {
        use hel::hel_targets::{CommandExecutor, CommandSpec, ProcessExecutor};
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("current project");
        std::fs::create_dir(&project).unwrap();
        let output = ProcessExecutor
            .execute(&CommandSpec::new(
                "git",
                ["-C", project.to_str().unwrap(), "init"],
            ))
            .unwrap();
        assert_eq!(output.status, 0);
        std::fs::write(project.join("draft.txt"), "uncommitted work").unwrap();
        let path = write_config(directory.path());
        let cancelled = Arc::new(AtomicBool::new(false));
        for _ in 0..2 {
            // No Docker process is needed: explicit selection does not probe
            // another runtime or silently change the configured destination.
            let (config, action) = prepare_session_launch_at(
                &path,
                "codex".into(),
                Some("custom-container".into()),
                project.clone(),
                &cancelled,
            )
            .unwrap();
            let DashboardAction::CreateSession {
                target_template_id,
                bundle_id,
                project_directory,
                allow_dirty_local,
                ..
            } = action
            else {
                panic!("expected launch")
            };
            assert_eq!(target_template_id, "custom-container");
            assert!(project_directory.is_none());
            assert!(allow_dirty_local);
            assert_eq!(config.bundles.len(), 1);
            assert_eq!(
                config.bundles[&bundle_id].repositories[0].local,
                Some(project.canonicalize().unwrap())
            );
            assert_eq!(HelConfig::load_from(&path).unwrap(), config);
        }
    }

    #[cfg(unix)]
    #[test]
    fn plain_directory_startup_adds_the_local_fallback_and_explicit_errors_do_not_fall_back() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_config(directory.path());
        let cancelled = Arc::new(AtomicBool::new(false));
        let (config, action) = prepare_session_launch_at(
            &path,
            "codex".into(),
            None,
            directory.path().to_path_buf(),
            &cancelled,
        )
        .unwrap();
        let DashboardAction::CreateSession {
            target_template_id,
            project_directory,
            ..
        } = action
        else {
            panic!("expected launch")
        };
        assert!(matches!(
            config.targets[&target_template_id],
            TargetTemplate::LocalBare
        ));
        assert_eq!(project_directory.as_deref(), Some(directory.path()));
        assert!(config.bundles.is_empty());
        assert!(config.targets.contains_key("custom-container"));
        let before = std::fs::read(&path).unwrap();
        assert!(
            prepare_session_launch_at(
                &path,
                "codex".into(),
                Some("missing".into()),
                directory.path().to_path_buf(),
                &cancelled
            )
            .unwrap_err()
            .to_string()
            .contains("not configured")
        );
        cancelled.store(true, Ordering::Release);
        assert!(
            prepare_session_launch_at(
                &path,
                "codex".into(),
                None,
                directory.path().to_path_buf(),
                &cancelled
            )
            .unwrap_err()
            .to_string()
            .contains("cancelled")
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn automatic_startup_prefers_available_podman_then_docker_then_local() {
        let config = HelConfig::default();
        assert!(matches!(
            automatic_target(&config, true, true).unwrap().1,
            TargetTemplate::LocalPodman { .. }
        ));
        assert!(matches!(
            automatic_target(&config, false, true).unwrap().1,
            TargetTemplate::LocalDocker { .. }
        ));
        #[cfg(unix)]
        assert!(matches!(
            automatic_target(&config, false, false).unwrap().1,
            TargetTemplate::LocalBare
        ));
    }

    #[test]
    fn automatic_startup_reuses_configured_runtime_settings_and_preserves_other_targets() {
        let mut config = HelConfig::default();
        let (_, custom) = local_runtime_target(RuntimeKind::Podman, "custom-image:latest");
        config.targets.insert("my-podman".into(), custom.clone());
        config
            .targets
            .insert("docker".into(), TargetTemplate::LocalBare);
        assert_eq!(
            automatic_target(&config, true, true).unwrap(),
            ("my-podman".into(), custom)
        );
        let (id, target) = automatic_target(&config, false, true).unwrap();
        assert_eq!(id, "docker-2");
        assert!(matches!(target, TargetTemplate::LocalDocker { .. }));
        assert!(matches!(
            config.targets["docker"],
            TargetTemplate::LocalBare
        ));
    }
}
