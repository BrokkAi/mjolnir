use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// Replace a session's shared Codex login with a private copy after cutover.
pub fn detach_codex_auth_symlink(worker_root: &Path) -> Result<bool> {
    let profile = worker_root.join("profile");
    let metadata = std::fs::symlink_metadata(&profile)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(false);
    }
    let auth = profile.join("auth.json");
    let metadata = match std::fs::symlink_metadata(&auth) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_symlink() {
        return Ok(false);
    }
    let bytes = std::fs::read(&auth).context("read Codex login through symlink")?;
    let _: serde_json::Value =
        serde_json::from_slice(&bytes).context("validate Codex login JSON")?;
    let mut temporary = tempfile::NamedTempFile::new_in(&profile)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&auth)
        .context("replace Codex login symlink")?;
    Ok(true)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn codex_detachment_preserves_target_and_skips_shared_profile_directory() {
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        let original = shared.join("auth.json");
        std::fs::write(&original, br#"{"token":"secret"}"#).unwrap();
        let owned = root.path().join("owned");
        std::fs::create_dir_all(owned.join("profile")).unwrap();
        symlink(&original, owned.join("profile/auth.json")).unwrap();
        assert!(detach_codex_auth_symlink(&owned).unwrap());
        let auth = owned.join("profile/auth.json");
        assert!(std::fs::symlink_metadata(&auth).unwrap().is_file());
        assert_eq!(
            std::fs::metadata(&auth).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read(&auth).unwrap(),
            std::fs::read(&original).unwrap()
        );
        let linked = root.path().join("linked");
        std::fs::create_dir(&linked).unwrap();
        symlink(&shared, linked.join("profile")).unwrap();
        assert!(!detach_codex_auth_symlink(&linked).unwrap());
        assert!(
            std::fs::symlink_metadata(linked.join("profile"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(original).unwrap(), br#"{"token":"secret"}"#);
    }
}

/// Inputs captured on the source before its staged configuration is edited.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StagedConfigRewriteSpec {
    pub old_config_root: std::path::PathBuf,
    pub new_config_root: std::path::PathBuf,
    pub old_home: std::path::PathBuf,
    pub new_home: std::path::PathBuf,
    pub source_machine_id: mj_core::config::MachineId,
    pub source_ssh: mj_core::config::SshConnection,
    /// Every old template ID and local kind recorded by the store inventory.
    pub historical_local_templates:
        std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
}

/// Add a machine for the source and equivalent SSH templates while keeping
/// this host's local templates available for new sessions.
pub fn rewrite_staged_config_and_files(
    source_config_path: &Path,
    output_config_path: &Path,
    spec: &StagedConfigRewriteSpec,
) -> Result<mj_store::database::SourceTemplateMap> {
    use anyhow::{Context, ensure};
    use mj_core::config::{
        Config, DaemonConfig, Machine, MachineId, PermissionMode, TargetTemplate, migration_paths,
    };
    ensure!(
        source_config_path != output_config_path,
        "source and output config paths must differ"
    );
    ensure!(
        spec.source_ssh.host != "local",
        "source host alias 'local' is reserved; choose another --source-host alias for host A"
    );
    let mut config = Config::load_from(source_config_path)?.with_local_targets();
    let mut source_ssh = spec.source_ssh.clone();
    if let Some(path) = &mut source_ssh.identity_file {
        migration_paths::relocate_ec2_path(path, &spec.old_config_root, &spec.new_config_root);
    }
    migration_paths::relocate_ec2_ssh_args(
        &mut source_ssh.extra_args,
        &spec.old_config_root,
        &spec.new_config_root,
    );
    ensure!(
        !spec.source_machine_id.is_local(),
        "source machine ID must differ from local"
    );
    ensure!(
        !config.machines.contains_key(&spec.source_machine_id),
        "source machine ID already exists"
    );
    ensure!(
        !config
            .machines
            .values()
            .any(|machine| matches!(machine,Machine::Ssh { ssh, .. } if ssh == &source_ssh)),
        "source SSH connection is already configured"
    );
    for machine in config.machines.values_mut() {
        match machine {
            Machine::Ssh { ssh, .. } => {
                if let Some(path) = &mut ssh.identity_file {
                    migration_paths::relocate_ssh_identity_path(
                        path,
                        &spec.old_config_root,
                        &spec.new_config_root,
                        &spec.old_home,
                        &spec.new_home,
                    );
                }
                migration_paths::relocate_ssh_args(
                    &mut ssh.extra_args,
                    &spec.old_config_root,
                    &spec.new_config_root,
                    &spec.old_home,
                    &spec.new_home,
                );
            }
            Machine::AwsEc2 {
                identity_file,
                ssh_args,
                ..
            } => {
                if let Some(path) = identity_file {
                    migration_paths::relocate_ssh_identity_path(
                        path,
                        &spec.old_config_root,
                        &spec.new_config_root,
                        &spec.old_home,
                        &spec.new_home,
                    );
                }
                migration_paths::relocate_ssh_args(
                    ssh_args,
                    &spec.old_config_root,
                    &spec.new_config_root,
                    &spec.old_home,
                    &spec.new_home,
                );
            }
            Machine::Local { .. } => {}
        }
    }
    // Fused target settings carry a second copy of SSH options; keep the
    // resolved templates consistent with their machine when saving.
    for target in config.targets.values_mut() {
        match target {
            TargetTemplate::SshBare { ssh, .. }
            | TargetTemplate::SshPodman { ssh, .. }
            | TargetTemplate::SshDocker { ssh, .. }
            | TargetTemplate::SshAppleContainer { ssh, .. }
            | TargetTemplate::SshNitro { ssh, .. } => {
                if let Some(path) = &mut ssh.identity_file {
                    migration_paths::relocate_ssh_identity_path(
                        path,
                        &spec.old_config_root,
                        &spec.new_config_root,
                        &spec.old_home,
                        &spec.new_home,
                    );
                }
                migration_paths::relocate_ssh_args(
                    &mut ssh.extra_args,
                    &spec.old_config_root,
                    &spec.new_config_root,
                    &spec.old_home,
                    &spec.new_home,
                );
            }
            TargetTemplate::AwsEc2 {
                identity_file,
                ssh_args,
                ..
            } => {
                if let Some(path) = identity_file {
                    migration_paths::relocate_ssh_identity_path(
                        path,
                        &spec.old_config_root,
                        &spec.new_config_root,
                        &spec.old_home,
                        &spec.new_home,
                    );
                }
                migration_paths::relocate_ssh_args(
                    ssh_args,
                    &spec.old_config_root,
                    &spec.new_config_root,
                    &spec.old_home,
                    &spec.new_home,
                );
            }
            _ => {}
        }
    }
    let build_cache = match config
        .machines
        .entry(MachineId::local())
        .or_insert(Machine::Local { build_cache: None })
    {
        Machine::Local { build_cache } => build_cache.take(),
        _ => anyhow::bail!("local machine is not local"),
    };
    config.machines.insert(
        spec.source_machine_id.clone(),
        Machine::Ssh {
            ssh: source_ssh.clone(),
            workspace_prefix: std::path::PathBuf::from(mj_core::config::DEFAULT_WORKSPACE_PREFIX),
            build_cache,
        },
    );
    fn kind_and_suffix(target: &TargetTemplate) -> Option<(&'static str, &'static str)> {
        match target {
            TargetTemplate::LocalBare => Some(("local-bare", "bare")),
            TargetTemplate::LocalPodman { .. } => Some(("local-podman", "podman")),
            TargetTemplate::LocalDocker { .. } => Some(("local-docker", "docker")),
            TargetTemplate::AppleContainer { .. } => Some(("apple-container", "apple-container")),
            _ => None,
        }
    }
    fn ssh_template(
        target: TargetTemplate,
        ssh: &mj_core::config::SshConnection,
    ) -> TargetTemplate {
        let ssh = ssh.clone();
        match target {
            TargetTemplate::LocalBare => TargetTemplate::SshBare {
                ssh,
                permissions: PermissionMode::Guardian,
                workspace_prefix: std::path::PathBuf::from(
                    mj_core::config::DEFAULT_WORKSPACE_PREFIX,
                ),
            },
            TargetTemplate::LocalPodman { container } => {
                TargetTemplate::SshPodman { ssh, container }
            }
            TargetTemplate::LocalDocker { container } => {
                TargetTemplate::SshDocker { ssh, container }
            }
            TargetTemplate::AppleContainer { container } => {
                TargetTemplate::SshAppleContainer { ssh, container }
            }
            _ => unreachable!(),
        }
    }
    let local: Vec<_> = config
        .targets
        .iter()
        .filter_map(|(id, target)| {
            kind_and_suffix(target).map(|(kind, suffix)| (id.clone(), kind, suffix, target.clone()))
        })
        .collect();
    let mut mapping = mj_store::database::SourceTemplateMap::new();
    let mut by_kind = std::collections::BTreeMap::<String, String>::new();
    for (old_id, kind, suffix, target) in local {
        let base = format!("{}-{suffix}", spec.source_machine_id);
        let id = if by_kind.contains_key(kind) {
            format!("{base}-{old_id}")
        } else {
            base
        };
        ensure!(
            !config.targets.contains_key(&id),
            "migrated target ID {id:?} already exists"
        );
        config
            .targets
            .insert(id.clone(), ssh_template(target, &source_ssh));
        by_kind.entry(kind.into()).or_insert_with(|| id.clone());
        mapping.entry(old_id).or_default().insert(kind.into(), id);
    }
    for (old_id, kinds) in &spec.historical_local_templates {
        for kind in kinds {
            if mapping
                .get(old_id)
                .and_then(|entries| entries.get(kind))
                .is_some()
            {
                continue;
            }
            let id = if let Some(id) = by_kind.get(kind) {
                id.clone()
            } else {
                let defaults = Config::default().with_local_targets();
                let container = defaults
                    .targets
                    .values()
                    .find_map(|target| target.container().cloned())
                    .context("no default container settings")?;
                let target = match kind.as_str() {
                    "local-bare" => TargetTemplate::LocalBare,
                    "local-podman" => TargetTemplate::LocalPodman { container },
                    "local-docker" => TargetTemplate::LocalDocker { container },
                    "apple-container" => TargetTemplate::AppleContainer { container },
                    _ => anyhow::bail!("unknown historical local runtime {kind:?}"),
                };
                let (_, suffix) = kind_and_suffix(&target).expect("local runtime");
                let id = format!("{}-{suffix}", spec.source_machine_id);
                ensure!(
                    !config.targets.contains_key(&id),
                    "migrated target ID {id:?} already exists"
                );
                config
                    .targets
                    .insert(id.clone(), ssh_template(target, &source_ssh));
                by_kind.insert(kind.clone(), id.clone());
                id
            };
            mapping
                .entry(old_id.clone())
                .or_default()
                .insert(kind.clone(), id);
        }
    }
    for (id, profile) in &mut config.profiles {
        let managed = profile
            .kind
            .managed_profile_home_in(&spec.old_config_root, id);
        if profile.home == managed {
            profile.home = profile
                .kind
                .managed_profile_home_in(&spec.new_config_root, id);
        } else if let Ok(relative) = profile.home.strip_prefix(&spec.old_home) {
            profile.home = spec.new_home.join(relative);
        }
    }
    let relocate_home = |path: &mut std::path::PathBuf| {
        if let Ok(relative) = path.strip_prefix(&spec.old_home) {
            *path = spec.new_home.join(relative);
        }
    };
    if let Some(path) = &mut config.github.session_token_path {
        relocate_home(path);
    }
    if let Some(app) = &mut config.github.app {
        relocate_home(&mut app.private_key_path);
    }
    config.daemon = DaemonConfig::default();
    config.validate()?;
    config.save_to(output_config_path)?;
    Ok(mapping)
}

#[cfg(test)]
mod config_migration_tests {
    use super::*;
    use mj_core::config::{
        Config, DaemonRuntime, HarnessKind, HarnessProfile, Machine, MachineId, SshConnection,
    };

    #[test]
    fn staged_config_rewrites_profile_homes_and_ec2_paths_with_native_daemon() {
        let root = tempfile::tempdir().unwrap();
        let old_home = root.path().join("old-home");
        let new_home = root.path().join("new-home");
        let old_config = old_home.join(".config/mjolnir");
        let new_config = new_home.join(".config/mjolnir");
        let staged = root.path().join("raw");
        let out = root.path().join("out");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        let path = staged.join("config.toml");
        let output = out.join("config.toml");
        let mut config = Config::default();
        config.profiles.insert(
            "managed".into(),
            HarnessProfile::new(
                HarnessKind::Codex,
                HarnessKind::Codex.managed_profile_home_in(&old_config, "managed"),
            ),
        );
        config.profiles.insert(
            "personal".into(),
            HarnessProfile::new(HarnessKind::Codex, old_home.join(".codex-personal")),
        );
        config.profiles.get_mut("personal").unwrap().environment =
            mj_core::config::Environment::from_sources(std::collections::BTreeMap::from([(
                "API_KEY".into(),
                mj_core::config::EnvironmentValue::FromSecret("KEY".into()),
            )]));
        std::fs::write(staged.join("secrets.toml"), "KEY = \"retained-secret\"\n").unwrap();
        std::fs::write(out.join("secrets.toml"), "KEY = \"retained-secret\"\n").unwrap();
        config.github.session_token_path = Some("/outside/token".into());
        config.machines.insert(
            MachineId::local(),
            Machine::Local {
                build_cache: Some(mj_core::config::TargetBuildCache {
                    enabled: Some(true),
                    directory: Some("/cache/source".into()),
                    ..Default::default()
                }),
            },
        );
        config.github.app = Some(mj_core::config::GithubAppConfig {
            app_id: 1,
            private_key_path: old_home.join(".secrets/app.pem"),
            installations: Default::default(),
            session_permissions: None,
            token_permissions: None,
        });
        config.daemon.runtime = DaemonRuntime::Ssh;
        config.daemon.host = Some("source-daemon".into());
        let ssh = SshConnection {
            host: "ec2-host".into(),
            user: Some("user".into()),
            identity_file: Some(old_config.join("ec2/instance/key")),
            extra_args: vec![
                "-o".into(),
                format!(
                    "UserKnownHostsFile={}",
                    old_config.join("ec2/instance/known_hosts").display()
                ),
            ],
        };
        config.machines.insert(
            MachineId::new("ec2-ssh").unwrap(),
            Machine::Ssh {
                ssh,
                workspace_prefix: ".local/share/mjolnir/workspaces".into(),
                build_cache: None,
            },
        );
        config.machines.insert(
            MachineId::new("ec2-template").unwrap(),
            Machine::AwsEc2 {
                aws_profile: Some("default".into()),
                region: "us-east-1".into(),
                launch_template: "lt-test".into(),
                launch_template_version: None,
                ssh_user: "ubuntu".into(),
                address_source: Default::default(),
                identity_file: Some(old_config.join("ec2/other/key")),
                ssh_args: vec![format!(
                    "-oUserKnownHostsFile={}",
                    old_config.join("ec2/other/known_hosts").display()
                )],
            },
        );
        let mut personal_ssh = SshConnection {
            host: "personal-host".into(),
            user: None,
            identity_file: Some(old_home.join(".ssh/id_key")),
            extra_args: vec![format!("-i{}", old_home.join(".ssh/other").display())],
        };
        config.machines.insert(
            MachineId::new("personal-ssh").unwrap(),
            Machine::Ssh {
                ssh: personal_ssh.clone(),
                workspace_prefix: ".local/share/mjolnir/workspaces".into(),
                build_cache: None,
            },
        );
        personal_ssh.host = "target-host".into();
        config.targets.insert(
            "personal-ssh-target".into(),
            mj_core::config::TargetTemplate::SshBare {
                ssh: personal_ssh,
                permissions: mj_core::config::PermissionMode::Guardian,
                workspace_prefix: ".local/share/mjolnir/workspaces".into(),
            },
        );
        config.machines.insert(
            MachineId::new("personal-aws").unwrap(),
            Machine::AwsEc2 {
                aws_profile: None,
                region: "us-east-1".into(),
                launch_template: "lt-personal".into(),
                launch_template_version: None,
                ssh_user: "ubuntu".into(),
                address_source: Default::default(),
                identity_file: Some(old_home.join(".ssh/aws-key")),
                ssh_args: vec![
                    "-i".into(),
                    old_home.join(".ssh/aws-extra").to_string_lossy().into(),
                ],
            },
        );
        config.save_to(&path).unwrap();
        std::fs::write(staged.join("go.json"),r#"{"version":2,"default":{"profile_id":"managed","target_id":"localhost","bundle_id":null,"project_directory":null,"create_managed_worktree":null,"additional_mounts":[],"resource_allocation":null},"recent":{"target_id":"podman"},"projects":[]}"#).unwrap();
        let spec = StagedConfigRewriteSpec {
            old_config_root: old_config,
            new_config_root: new_config.clone(),
            old_home: old_home.clone(),
            new_home: new_home.clone(),
            source_machine_id: MachineId::new("source").unwrap(),
            source_ssh: SshConnection {
                host: "source-host".into(),
                user: None,
                identity_file: Some(old_home.join(".ssh/source-key")),
                extra_args: vec![],
            },
            historical_local_templates: std::collections::BTreeMap::from([
                (
                    "localhost".into(),
                    std::collections::BTreeSet::from(["local-bare".into(), "local-docker".into()]),
                ),
                (
                    "retired-template".into(),
                    std::collections::BTreeSet::from(["local-bare".into()]),
                ),
            ]),
        };
        let original = std::fs::read(&path).unwrap();
        let go_original = std::fs::read(staged.join("go.json")).unwrap();
        let mapping = rewrite_staged_config_and_files(&path, &output, &spec).unwrap();
        let first = std::fs::read(&output).unwrap();
        assert_eq!(
            mapping,
            rewrite_staged_config_and_files(&path, &output, &spec).unwrap()
        );
        assert_eq!(first, std::fs::read(&output).unwrap());
        assert_eq!(original, std::fs::read(&path).unwrap());
        assert_eq!(go_original, std::fs::read(staged.join("go.json")).unwrap());
        let reloaded = Config::load_from(&output).unwrap().with_local_targets();
        reloaded.validate().unwrap();
        assert_eq!(
            reloaded.profiles["managed"].home,
            HarnessKind::Codex.managed_profile_home_in(&new_config, "managed")
        );
        assert_eq!(
            reloaded.profiles["personal"].home,
            new_home.join(".codex-personal")
        );
        assert_eq!(
            reloaded.github.session_token_path.as_deref(),
            Some(Path::new("/outside/token"))
        );
        assert_eq!(
            reloaded.github.app.as_ref().unwrap().private_key_path,
            new_home.join(".secrets/app.pem")
        );
        assert_eq!(
            reloaded.profiles["personal"]
                .environment
                .resolved()
                .get("API_KEY")
                .map(String::as_str),
            Some("retained-secret")
        );
        assert_eq!(reloaded.daemon.runtime, DaemonRuntime::Host);
        let Machine::Ssh { ssh, .. } = &reloaded.machines["ec2-ssh"] else {
            panic!("SSH machine expected")
        };
        assert_eq!(
            ssh.identity_file.as_deref(),
            Some(new_config.join("ec2/instance/key").as_path())
        );
        assert!(
            ssh.extra_args
                .iter()
                .any(|arg| arg.contains(new_config.to_str().unwrap()))
        );
        let Machine::Ssh { ssh, .. } = &reloaded.machines["personal-ssh"] else {
            panic!("SSH machine expected")
        };
        assert_eq!(
            ssh.identity_file.as_deref(),
            Some(new_home.join(".ssh/id_key").as_path())
        );
        assert_eq!(
            ssh.extra_args[0],
            format!("-i{}", new_home.join(".ssh/other").display())
        );
        let mj_core::config::TargetTemplate::SshBare { ssh, .. } =
            &reloaded.targets["personal-ssh-target"]
        else {
            panic!("SSH target expected")
        };
        assert_eq!(
            ssh.identity_file.as_deref(),
            Some(new_home.join(".ssh/id_key").as_path())
        );
        assert_eq!(
            ssh.extra_args[0],
            format!("-i{}", new_home.join(".ssh/other").display())
        );
        let Machine::AwsEc2 {
            identity_file,
            ssh_args,
            ..
        } = &reloaded.machines["personal-aws"]
        else {
            panic!("AWS machine expected")
        };
        assert_eq!(
            identity_file.as_deref(),
            Some(new_home.join(".ssh/aws-key").as_path())
        );
        assert_eq!(
            ssh_args[1],
            new_home.join(".ssh/aws-extra").to_string_lossy()
        );
        let Machine::Ssh { ssh, .. } = &reloaded.machines["source"] else {
            panic!("source machine expected")
        };
        assert_eq!(
            ssh.identity_file.as_deref(),
            Some(old_home.join(".ssh/source-key").as_path())
        );
        assert!(reloaded.machines.contains_key("source"));
        assert!(matches!(
            reloaded.machines.get("local"),
            None | Some(Machine::Local { build_cache: None })
        ));
        assert!(
            matches!(&reloaded.machines["source"], Machine::Ssh { build_cache:Some(cache),.. } if cache.directory.as_deref() == Some(Path::new("/cache/source")))
        );
        let Machine::AwsEc2 {
            identity_file,
            ssh_args,
            ..
        } = &reloaded.machines["ec2-template"]
        else {
            panic!("EC2 machine expected")
        };
        assert_eq!(
            identity_file.as_ref().unwrap(),
            &new_config.join("ec2/other/key")
        );
        assert!(ssh_args[0].contains(new_config.to_str().unwrap()));
        assert_eq!(mapping["localhost"]["local-bare"], "source-bare");
        assert_eq!(mapping["localhost"]["local-docker"], "source-docker");
        assert_eq!(mapping["retired-template"]["local-bare"], "source-bare");
        let go: serde_json::Value =
            serde_json::from_slice(&std::fs::read(staged.join("go.json")).unwrap()).unwrap();
        assert_eq!(go["default"]["target_id"], "localhost");
        assert_eq!(go["recent"]["target_id"], "podman");
    }

    #[test]
    fn malformed_go_preferences_are_left_untouched() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        Config::default().save_to(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        std::fs::write(root.path().join("go.json"), "{").unwrap();
        let spec = StagedConfigRewriteSpec {
            old_config_root: root.path().join("old-config"),
            new_config_root: root.path().join("new-config"),
            old_home: root.path().join("old-home"),
            new_home: root.path().join("new-home"),
            source_machine_id: MachineId::new("source").unwrap(),
            source_ssh: SshConnection {
                host: "source-host".into(),
                user: None,
                identity_file: None,
                extra_args: vec![],
            },
            historical_local_templates: Default::default(),
        };
        let output = root.path().join("out/config.toml");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        rewrite_staged_config_and_files(&path, &output, &spec).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::read_to_string(root.path().join("go.json")).unwrap(),
            "{"
        );
    }
}
