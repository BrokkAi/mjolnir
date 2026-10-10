//! Machine storage shared by native build tools, independently of mbx.

use anyhow::{Context, Result, ensure};
use mj_core::config::{Config, TargetTemplate, stored_target};
use mj_core::state::ToolCachePlacement;

use super::cache_host::CacheHost;
use crate::targets::{self, CommandExecutor};

pub(super) fn prepare(
    config: &Config,
    template_id: &str,
    mounts: &mut Vec<targets::AdditionalMount>,
    executor: &impl CommandExecutor,
) -> Option<ToolCachePlacement> {
    // Publish all mounts with their placement, or none. A cache failure must
    // not leave an unrecorded mount or prevent the requested session starting.
    let mut prepared_mounts = mounts.clone();
    match prepare_checked(config, template_id, &mut prepared_mounts, executor) {
        Ok(cache) => {
            *mounts = prepared_mounts;
            cache
        }
        Err(error) => {
            tracing::warn!(
                target = template_id,
                "native build caches unavailable: {error:#}"
            );
            executor.notify_notice(&format!("Native build caches are unavailable: {error:#}. This session will start without them."));
            None
        }
    }
}

fn prepare_checked(
    config: &Config,
    template_id: &str,
    mounts: &mut Vec<targets::AdditionalMount>,
    executor: &impl CommandExecutor,
) -> Result<Option<ToolCachePlacement>> {
    let template = config
        .targets
        .get(template_id)
        .context("cache target disappeared")?;
    if matches!(
        template,
        TargetTemplate::AwsEc2 { .. } | TargetTemplate::AppleContainer { .. }
    ) {
        return Ok(None);
    }
    // Use the same mapping as configuration serialization, including implicit
    // local machines and raw SSH runtimes whose template has no cache fields.
    let mut machines = config.machines.clone();
    let stored = stored_target(template_id, template, &mut machines);
    let settings = machines
        .get(stored.machine())
        .and_then(|machine| machine.build_cache());
    if settings.and_then(|settings| settings.enabled) == Some(false) {
        return Ok(None);
    }
    let host = CacheHost::for_path_target(template)?;
    // Container bind mounts and host filesystem probes here require Linux,
    // as the existing mbx integration does. Other hosts keep native defaults.
    if !super::mbx::host_supports_cache(&host, executor)? {
        return Ok(None);
    }
    let home = host.home(executor)?;
    let directory = match settings.and_then(|settings| settings.tools_directory.clone()) {
        Some(directory) => directory,
        None => home.join(".cache/mjolnir/build"),
    };
    let nx_home = home.join(".nx");
    let container = !matches!(
        template,
        TargetTemplate::LocalBare | TargetTemplate::SshBare { .. }
    );
    let nx_destination = if container {
        std::path::Path::new(
            template
                .container()
                .and_then(|container| container.environment.resolved().get("HOME"))
                .map(String::as_str)
                .unwrap_or("/home/hel"),
        )
        .join(".nx")
    } else {
        nx_home.clone()
    };
    for (directory, destination) in [(&directory, &directory), (&nx_home, &nx_destination)] {
        if container {
            ensure!(
                !mounts
                    .iter()
                    .any(|mount| mount.destination.starts_with(destination)
                        || destination.starts_with(&mount.destination)),
                "an attached directory overlaps native build cache {}",
                directory.display()
            );
        }
        let command = host.command(
            vec![
                "mkdir".into(),
                "-p".into(),
                "-m".into(),
                "700".into(),
                "--".into(),
                directory.to_string_lossy().into_owned(),
            ],
            "create shared native build cache",
        );
        let output = executor.execute(&command)?;
        ensure!(
            output.status == 0,
            "create native build cache: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let filesystems =
            targets::probe_filesystem_types(host.ssh(), std::slice::from_ref(directory), executor)?;
        let filesystem = filesystems
            .first()
            .context("native cache filesystem probe returned no result")?;
        ensure!(
            !targets::overlay_unsupported_filesystem(filesystem)
                .is_some_and(|reason| matches!(reason, "network filesystem" | "FUSE filesystem")),
            "native build cache {} needs a local filesystem; configure build_cache.tools_directory on this machine",
            directory.display()
        );
        if container {
            mounts.push(targets::AdditionalMount {
                source: directory.clone(),
                destination: destination.clone(),
                access: targets::MountAccess::Rw,
            });
        }
    }
    Ok(Some(ToolCachePlacement {
        host: host.key(),
        directory,
        nx_home: nx_destination,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FailSecondDirectory {
        notices: Mutex<Vec<String>>,
    }

    impl CommandExecutor for FailSecondDirectory {
        fn execute(&self, command: &targets::CommandSpec) -> Result<targets::CommandOutput> {
            let (status, stdout, stderr) = match command.program.as_str() {
                "uname" => (0, "Linux x86_64\n", ""),
                "stat" => (0, "ext4\n", ""),
                "mkdir"
                    if command
                        .args
                        .last()
                        .is_some_and(|path| path.ends_with("/.nx")) =>
                {
                    (1, "", "fixture Nx volume is unavailable")
                }
                "mkdir" => (0, "", ""),
                other => anyhow::bail!("unexpected preparation program {other}"),
            };
            Ok(targets::CommandOutput {
                status,
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
            })
        }

        fn notify_notice(&self, notice: &str) {
            self.notices.lock().unwrap().push(notice.to_owned());
        }
    }

    #[test]
    fn failed_cache_preparation_cannot_publish_a_partial_mount_set() {
        let mut config = Config::default().with_local_targets();
        let settings = mj_core::config::TargetBuildCache {
            tools_directory: Some("/cache/native".into()),
            ..Default::default()
        };
        config.machines.insert(
            "local".into(),
            mj_core::config::Machine::Local {
                build_cache: Some(settings.clone()),
            },
        );
        let template: TargetTemplate = serde_json::from_value(serde_json::json!({
            "kind": "local-podman", "image": "fixture", "build_cache": settings,
        }))
        .unwrap();
        config.targets.insert("fixture".into(), template);
        let original = vec![targets::AdditionalMount {
            source: "/data".into(),
            destination: "/data".into(),
            access: targets::MountAccess::Ro,
        }];
        let mut mounts = original.clone();
        let executor = FailSecondDirectory::default();
        assert!(prepare(&config, "fixture", &mut mounts, &executor).is_none());
        assert_eq!(mounts, original);
        assert!(
            executor
                .notices
                .lock()
                .unwrap()
                .iter()
                .any(|notice| notice.contains("fixture Nx volume is unavailable"))
        );
    }
}
