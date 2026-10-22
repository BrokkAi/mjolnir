//! Machine storage shared by native build tools, independently of mbx.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use mj_core::config::{Config, TargetTemplate, stored_target};
use mj_core::state::ToolCachePlacement;

use super::cache_host::{CacheHost, HostPlatform};
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
    let container = !matches!(
        template,
        TargetTemplate::LocalBare | TargetTemplate::SshBare { .. }
    );
    // A container shares these directories through a bind mount, which needs
    // the host's own kernel. A bare session on a Mac runs on the Mac itself.
    // Other hosts keep native defaults.
    match host.platform(executor)? {
        HostPlatform::Linux => {}
        HostPlatform::Darwin if !container => {}
        HostPlatform::Darwin | HostPlatform::Windows => return Ok(None),
    }
    let home = host.home(executor)?;
    let directory = match settings.and_then(|settings| settings.tools_directory.clone()) {
        Some(directory) => directory,
        None => mj_core::target_path::join(&home, Path::new(".cache/mjolnir/build")),
    };
    let nx_home = mj_core::target_path::join(&home, Path::new(".nx"));
    let nx_destination = if container {
        mj_core::target_path::join(
            Path::new(
                template
                    .container()
                    .and_then(|container| container.environment.resolved().get("HOME"))
                    .map(String::as_str)
                    .unwrap_or("/home/hel"),
            ),
            Path::new(".nx"),
        )
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
                "sh" if command.purpose == "probe mount source filesystem" => (0, "ext4\n", ""),
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

    /// A Mac host whose commands all succeed, recording what ran.
    #[cfg(unix)]
    #[derive(Default)]
    struct MacHost {
        ran: Mutex<Vec<String>>,
    }

    #[cfg(unix)]
    impl CommandExecutor for MacHost {
        fn execute(&self, command: &targets::CommandSpec) -> Result<targets::CommandOutput> {
            self.ran.lock().unwrap().push(command.program.clone());
            let stdout = match command.program.as_str() {
                "uname" => "Darwin arm64\n",
                "sh" if command.purpose == "probe mount source filesystem" => "apfs\n",
                "mkdir" => "",
                other => anyhow::bail!("unexpected preparation program {other}"),
            };
            Ok(targets::CommandOutput {
                status: 0,
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    // Hard-won: #1330: bare sessions on a Mac got no native build caches or mbx launchers.
    #[cfg(unix)]
    #[test]
    fn a_mac_prepares_native_caches_for_bare_sessions_but_not_containers() {
        let mut config = Config::default().with_local_targets();
        config.machines.insert(
            "local".into(),
            mj_core::config::Machine::Local {
                build_cache: Some(mj_core::config::TargetBuildCache {
                    tools_directory: Some("/cache/native".into()),
                    ..Default::default()
                }),
            },
        );

        let bare = MacHost::default();
        let mut mounts = Vec::new();
        let placement = prepare(&config, "localhost", &mut mounts, &bare).unwrap();
        assert_eq!(placement.directory, Path::new("/cache/native"));
        assert!(mounts.is_empty(), "a bare session mounts nothing");

        // Its containers run Linux in a VM that these host paths do not describe.
        let container = MacHost::default();
        assert!(prepare(&config, "podman", &mut mounts, &container).is_none());
        assert!(mounts.is_empty());
        assert_eq!(*container.ran.lock().unwrap(), ["uname"]);
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
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
