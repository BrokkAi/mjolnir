//! Machine storage shared by native build tools, independently of mbx.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use mj_core::config::{Config, TargetTemplate, stored_target};
use mj_core::state::ToolCachePlacement;

use super::cache_host::{CacheHost, HostPlatform};
use crate::targets::{self, CommandExecutor};

pub fn prepare(
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
    // Apple's runtime mounts supplemental directories read-only from its own
    // VM, so there is no host directory to share, locally or over SSH; EC2
    // copies the directory instead of mounting it.
    if matches!(
        template,
        TargetTemplate::AwsEc2 { .. }
            | TargetTemplate::AppleContainer { .. }
            | TargetTemplate::SshAppleContainer { .. }
            // A sandbox is a VM with no way to mount a host directory.
            | TargetTemplate::SshNitro { .. }
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
    let (platform, home) = host.platform_and_home(executor)?;
    match platform {
        HostPlatform::Linux => {}
        HostPlatform::Darwin if !container => {}
        HostPlatform::Darwin | HostPlatform::Windows => return Ok(None),
    }
    let home = home?;
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
    let caches = [(&directory, &directory), (&nx_home, &nx_destination)];
    // Both are checked before the host is touched.
    if container {
        for (directory, destination) in caches {
            ensure!(
                !mounts
                    .iter()
                    .any(|mount| mount.destination.starts_with(destination)
                        || destination.starts_with(&mount.destination)),
                "an attached directory overlaps native build cache {}",
                directory.display()
            );
        }
    }
    let sources: Vec<_> = caches.iter().map(|(source, _)| (*source).clone()).collect();
    let mut create = vec![
        "mkdir".to_owned(),
        "-p".into(),
        "-m".into(),
        "700".into(),
        "--".into(),
    ];
    create.extend(
        sources
            .iter()
            .map(|source| source.to_string_lossy().into_owned()),
    );
    let output = executor.execute(&host.command(create, "create shared native build cache"))?;
    ensure!(
        output.status == 0,
        "create native build cache: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let filesystems = targets::probe_filesystem_types(host.ssh(), &sources, executor)?;
    for ((directory, _), filesystem) in caches.iter().zip(&filesystems) {
        ensure!(
            !targets::overlay_unsupported_filesystem(filesystem)
                .is_some_and(|reason| matches!(reason, "network filesystem" | "FUSE filesystem")),
            "native build cache {} needs a local filesystem; configure build_cache.tools_directory on this machine",
            directory.display()
        );
    }
    if container {
        for (directory, destination) in caches {
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
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &targets::CommandSpec) -> Result<targets::CommandOutput> {
            let (status, stdout, stderr) = match command.program.as_str() {
                "uname" => (0, "Linux x86_64\n", ""),
                "sh" if command.purpose == "probe mount source filesystem" => {
                    (0, "ext4\next4\n", "")
                }
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
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &targets::CommandSpec) -> Result<targets::CommandOutput> {
            self.ran.lock().unwrap().push(command.program.clone());
            let stdout = match command.program.as_str() {
                "uname" => "Darwin arm64\n".to_owned(),
                "sh" if command.purpose == "probe mount source filesystem" => {
                    // One answer per directory, after the script and its label.
                    "apfs\n".repeat(command.args.len() - 3)
                }
                "mkdir" => String::new(),
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
        assert!(prepare(&config, "docker", &mut mounts, &container).is_none());
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
    /// An SSH host that answers each kind of command by its purpose and keeps
    /// the remote command lines it was given.
    struct RemoteHost {
        /// The filesystems of the cache directory and of `.nx`, in that order.
        filesystems: [&'static str; 2],
        lines: Mutex<Vec<(String, String)>>,
        notices: Mutex<Vec<String>>,
    }

    impl RemoteHost {
        fn new(filesystems: [&'static str; 2]) -> Self {
            Self {
                filesystems,
                lines: Mutex::default(),
                notices: Mutex::default(),
            }
        }

        fn purposes(&self) -> Vec<String> {
            self.lines
                .lock()
                .unwrap()
                .iter()
                .map(|(purpose, _)| purpose.clone())
                .collect()
        }
    }

    impl CommandExecutor for RemoteHost {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            false
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            _input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute(&self, command: &targets::CommandSpec) -> Result<targets::CommandOutput> {
            assert_eq!(command.program, "ssh", "{command:?}");
            let line = command.args.last().cloned().unwrap_or_default();
            self.lines
                .lock()
                .unwrap()
                .push((command.purpose.clone(), line));
            let stdout = match command.purpose.as_str() {
                "detect build cache host platform and home" => {
                    targets::ssh_podman_probe_fixture(&[
                        ("platform", 0, "Linux x86_64", ""),
                        ("home", 0, "/home/builder", ""),
                    ])
                }
                "create shared native build cache" => Vec::new(),
                "probe mount source filesystem" => {
                    format!("{}\n{}\n", self.filesystems[0], self.filesystems[1]).into_bytes()
                }
                other => anyhow::bail!("unexpected preparation command {other}"),
            };
            Ok(targets::CommandOutput {
                status: 0,
                stdout,
                stderr: Vec::new(),
            })
        }

        fn notify_notice(&self, notice: &str) {
            self.notices.lock().unwrap().push(notice.to_owned());
        }
    }

    fn remote_config() -> Config {
        let mut config = Config::default().with_local_targets();
        let template: TargetTemplate = serde_json::from_value(serde_json::json!({
            "kind": "ssh-podman", "host": "tools.test", "user": "builder", "image": "fixture",
        }))
        .unwrap();
        config.targets.insert("remote".into(), template);
        config
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn an_ssh_host_prepares_both_native_caches_in_three_round_trips() {
        let executor = RemoteHost::new(["ext4", "xfs"]);
        let mut mounts = Vec::new();
        let placement = prepare(&remote_config(), "remote", &mut mounts, &executor).unwrap();
        assert_eq!(
            placement.directory,
            Path::new("/home/builder/.cache/mjolnir/build")
        );
        assert_eq!(mounts.len(), 2);
        // Platform and home together, one `mkdir` for both directories, and one
        // filesystem probe for both, where each used to be a call of its own.
        assert_eq!(
            executor.purposes(),
            [
                "detect build cache host platform and home",
                "create shared native build cache",
                "probe mount source filesystem",
            ]
        );
        let lines = executor.lines.lock().unwrap();
        for directory in ["/home/builder/.cache/mjolnir/build", "/home/builder/.nx"] {
            assert!(lines[1].1.contains(directory), "{}", lines[1].1);
            assert!(lines[2].1.contains(directory), "{}", lines[2].1);
        }
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn a_network_filesystem_under_either_directory_or_an_overlapping_mount_refuses_before_any_mount()
     {
        // The second directory's filesystem is the one that is refused.
        let executor = RemoteHost::new(["ext4", "nfs"]);
        let mut mounts = Vec::new();
        assert!(prepare(&remote_config(), "remote", &mut mounts, &executor).is_none());
        assert!(mounts.is_empty(), "a refused cache leaves no mount behind");
        let notices = executor.notices.lock().unwrap();
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("/home/builder/.nx needs a local filesystem")),
            "{notices:?}"
        );

        // A mount over the second cache is refused before the host is changed.
        let executor = RemoteHost::new(["ext4", "ext4"]);
        let original = vec![targets::AdditionalMount {
            source: "/data".into(),
            destination: "/home/hel/.nx/shared".into(),
            access: targets::MountAccess::Ro,
        }];
        let mut mounts = original.clone();
        assert!(prepare(&remote_config(), "remote", &mut mounts, &executor).is_none());
        assert_eq!(mounts, original);
        assert_eq!(
            executor.purposes(),
            ["detect build cache host platform and home"],
            "nothing was created on the host"
        );
    }
}
