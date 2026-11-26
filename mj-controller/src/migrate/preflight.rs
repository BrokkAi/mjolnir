//! Read-only host migration inventory and safety checks.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use mj_core::config::{
    Classification, Config, ConfigLayout, DaemonRuntime, DataLayout, EnvironmentValue, HarnessKind,
    Machine, MigrateAction, classify_config_entry, classify_data_entry,
};
use mj_store::database::{
    MigrationPreconditions, StoreMigrationInventory, inspect_migration_inventory,
    inspect_migration_preconditions,
};
use rusqlite::Connection;

#[derive(Debug)]
pub struct ClassifiedEntry {
    pub path: PathBuf,
    pub classification: Classification,
    pub size_bytes: u64,
}

#[derive(Debug)]
pub struct HomeCopy {
    pub source: PathBuf,
    pub destination: PathBuf,
    /// External credentials are copied with owner-only permissions.
    pub private: bool,
}

#[derive(Debug, Clone)]
pub struct Ec2Resource {
    pub instance_id: String,
    pub region: String,
    pub aws_profile: Option<String>,
}

#[derive(Debug)]
pub struct MigrationPreflight {
    pub preconditions: MigrationPreconditions,
    pub inventory: StoreMigrationInventory,
    pub data_entries: Vec<ClassifiedEntry>,
    pub config_entries: Vec<ClassifiedEntry>,
    pub home_copies: Vec<HomeCopy>,
    pub warnings: Vec<String>,
    pub sessionwiki_db: PathBuf,
    pub ec2_resources: Vec<Ec2Resource>,
}

/// `destination_exists` runs on B (for example over the already established
/// SSH connection). It must check the path itself, including dangling symlinks.
/// The caller must run this filesystem scan and remote probes in a supervised
/// background task, away from UI event and render loops.
#[allow(clippy::too_many_arguments)]
pub fn inspect_preflight(
    connection: &Connection,
    config: &Config,
    old_data_root: &Path,
    old_config_root: &Path,
    old_home: &Path,
    new_home: &Path,
    new_data_root: &Path,
    new_config_root: &Path,
    source_host: &str,
    destination_exists: impl Fn(&Path) -> Result<bool>,
) -> Result<MigrationPreflight> {
    let data = DataLayout::at(old_data_root);
    let config_layout = ConfigLayout::at(old_config_root);
    ensure!(
        !exists(&data.daemon_container())?,
        "container-hosted daemon cannot migrate; run a native daemon"
    );
    ensure!(
        !matches!(
            config.daemon.runtime,
            DaemonRuntime::Podman | DaemonRuntime::Docker
        ),
        "container-hosted daemon cannot migrate; run a native daemon"
    );
    ensure!(
        !exists(&data.config_rename_journal())?,
        "finish the pending configuration rename before migrating"
    );
    ensure!(
        source_host.rsplit('@').next().unwrap_or(source_host) != "local",
        "source host alias 'local' is reserved; choose another --source-host alias for host A"
    );
    ensure!(
        !destination_exists(&DataLayout::at(new_data_root).database())?,
        "destination already has an mj.sqlite3; clean it manually or choose --dest-instance"
    );
    ensure!(
        !destination_exists(&ConfigLayout::at(new_config_root).config_toml())?,
        "destination already has a config.toml; clean it manually or choose --dest-instance"
    );
    let (ec2_resources, ec2_warnings) = inspect_ec2_resources(old_config_root)?;
    let preconditions = inspect_migration_preconditions(connection, old_data_root)?;
    let inventory = inspect_migration_inventory(connection)?;
    let mut home_copies = Vec::new();
    let mut warnings = ec2_warnings;
    let mut checked_destinations = BTreeSet::new();
    for (id, profile) in &config.profiles {
        if cfg!(target_os = "macos") && profile.kind == HarnessKind::Claude {
            warnings.push(format!("profile {id} may use a macOS Keychain login"));
        }
        for (key, value) in profile.environment.sources() {
            match value {
                EnvironmentValue::FromEnv(name) => {
                    warnings.push(format!("profile {id} uses environment variable {name}"))
                }
                EnvironmentValue::Literal(value)
                    if matches!(key.as_str(), "AWS_PROFILE" | "AWS_DEFAULT_PROFILE") =>
                {
                    warnings.push(format!("profile {id} uses AWS profile {value}"));
                }
                EnvironmentValue::Literal(value)
                    if key == "SSH_AUTH_SOCK" || Path::new(value).is_absolute() =>
                {
                    warnings.push(format!(
                        "profile {id} environment {key} points at host-local path {value}"
                    ))
                }
                _ => {}
            }
        }
        let managed = profile.kind.managed_profile_home_in(old_config_root, id);
        if profile.home == managed {
            continue;
        }
        let Ok(relative) = profile.home.strip_prefix(old_home) else {
            warnings.push(format!(
                "profile {id} uses unmanaged home outside source HOME: {}",
                profile.home.display()
            ));
            if !destination_exists(&profile.home)? {
                warnings.push(format!(
                    "profile {id} home is absent on destination: {}",
                    profile.home.display()
                ));
            }
            continue;
        };
        if exists(&profile.home)? {
            let destination = new_home.join(relative);
            check_destination(&destination, &destination_exists, &mut checked_destinations)?;
            home_copies.push(HomeCopy {
                source: profile.home.clone(),
                destination,
                private: false,
            });
        } else {
            warnings.push(format!(
                "profile {id} home does not exist on source: {}",
                profile.home.display()
            ));
        }
        if matches!(profile.kind, HarnessKind::Muse | HarnessKind::OpenCode)
            && let Some(data) = profile.kind.nested_data_dir(&profile.home)
            && !data.starts_with(&profile.home)
        {
            plan_extra_home_file(
                &data,
                old_home,
                new_home,
                &destination_exists,
                &mut checked_destinations,
                &mut home_copies,
                &mut warnings,
            )?;
        }
    }
    for (label, path) in [
        (
            "GitHub session token",
            config.github.session_token_path.as_ref(),
        ),
        (
            "GitHub App key",
            config.github.app.as_ref().map(|app| &app.private_key_path),
        ),
    ] {
        if let Some(path) = path {
            if let Ok(relative) = path.strip_prefix(old_home) {
                let destination = new_home.join(relative);
                check_destination(&destination, &destination_exists, &mut checked_destinations)?;
                home_copies.push(HomeCopy {
                    source: path.clone(),
                    destination,
                    private: true,
                });
            } else {
                warnings.push(format!(
                    "{label} is outside source HOME: {}",
                    path.display()
                ));
                if !destination_exists(path)? {
                    warnings.push(format!(
                        "{label} is absent on destination: {}",
                        path.display()
                    ));
                }
            }
        }
    }
    if let Some(path) = &config.phone.tls_cert {
        warnings.push(format!("phone TLS certificate: {}", path.display()));
    }
    if let Some(path) = &config.phone.tls_key {
        warnings.push(format!("phone TLS key: {}", path.display()));
    }
    warnings.push(format!("phone bind is host-local: {}", config.phone.bind));
    for (id, bundle) in &config.bundles {
        for repository in &bundle.repositories {
            if let Some(path) = &repository.local {
                warnings.push(format!(
                    "bundle {id} repository {} uses local path {}",
                    repository.id,
                    path.display()
                ));
            }
        }
    }
    for (id, machine) in &config.machines {
        if let Machine::AwsEc2 {
            aws_profile: Some(profile),
            ..
        } = machine
        {
            warnings.push(format!("machine {id} uses AWS profile {profile}"));
        }
    }
    for (id, target) in &config.targets {
        if let Some(container) = target.container() {
            for (key, value) in container.environment.sources() {
                if let ("AWS_PROFILE" | "AWS_DEFAULT_PROFILE", EnvironmentValue::Literal(value)) =
                    (key.as_str(), value)
                {
                    warnings.push(format!("target {id} uses AWS profile {value}"));
                }
            }
        }
    }
    for resource in &ec2_resources {
        if let Some(profile) = &resource.aws_profile {
            warnings.push(format!(
                "EC2 instance {} uses AWS profile {profile}",
                resource.instance_id
            ));
        }
    }
    fn collect_env_refs(value: &serde_json::Value, names: &mut BTreeSet<String>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(name) = map.get("from_env").and_then(serde_json::Value::as_str) {
                    names.insert(name.to_owned());
                }
                for value in map.values() {
                    collect_env_refs(value, names);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    collect_env_refs(value, names);
                }
            }
            _ => {}
        }
    }
    let mut environment_names = BTreeSet::new();
    collect_env_refs(&serde_json::to_value(config)?, &mut environment_names);
    for name in environment_names {
        warnings.push(format!("configuration uses environment variable {name}"));
    }
    if let Some(path) = std::env::var_os("SSH_AUTH_SOCK") {
        warnings.push(format!(
            "SSH_AUTH_SOCK is host-local: {}",
            PathBuf::from(path).display()
        ));
    }
    if source_host
        .rsplit('@')
        .next()
        .unwrap_or(source_host)
        .trim_matches(['[', ']'])
        .parse::<std::net::IpAddr>()
        .is_ok()
    {
        warnings.push("source host is an IP literal; a stable SSH name is recommended".into());
    }
    if !inventory.unidentified_placements.is_empty() {
        warnings.push(format!(
            "{} placements have no machine ID",
            inventory.unidentified_placements.len()
        ));
    }
    let go_path = config_layout.go_json();
    if exists(&go_path)? {
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&go_path)?).context("parse go.json")?;
        fn collect_go_paths(value: &serde_json::Value, paths: &mut BTreeSet<String>) {
            match value {
                serde_json::Value::Object(map) => {
                    for (key, value) in map {
                        if matches!(key.as_str(), "directory" | "project_directory")
                            && let Some(path) = value.as_str()
                        {
                            paths.insert(path.to_owned());
                        }
                        collect_go_paths(value, paths);
                    }
                }
                serde_json::Value::Array(values) => {
                    for value in values {
                        collect_go_paths(value, paths);
                    }
                }
                _ => {}
            }
        }
        let mut paths = BTreeSet::new();
        collect_go_paths(&json, &mut paths);
        for path in paths {
            warnings.push(format!(
                "fast-start preferences contain host-local path {path}"
            ));
        }
    }
    // Keep private entries even inside a copied profile so their restrictive mode is applied.
    home_copies.sort_by(|a, b| {
        a.source
            .components()
            .count()
            .cmp(&b.source.components().count())
    });
    let mut deduplicated: Vec<HomeCopy> = Vec::new();
    for copy in home_copies {
        if !copy.private
            && deduplicated.iter().any(|parent| {
                !parent.private
                    && copy.source.starts_with(&parent.source)
                    && copy.destination.starts_with(&parent.destination)
            })
        {
            continue;
        }
        if copy.private
            && deduplicated.iter().any(|other| {
                other.private
                    && other.source == copy.source
                    && other.destination == copy.destination
            })
        {
            continue;
        }
        deduplicated.push(copy);
    }
    let home_copies = deduplicated;
    Ok(MigrationPreflight {
        preconditions,
        inventory,
        data_entries: inventory_layout(old_data_root, true)?,
        config_entries: inventory_layout(old_config_root, false)?,
        home_copies,
        warnings,
        sessionwiki_db: mj_core::config::session_index_db_path()?,
        ec2_resources,
    })
}

fn exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn check_destination(
    path: &Path,
    probe: &impl Fn(&Path) -> Result<bool>,
    checked: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if checked.insert(path.to_path_buf()) {
        ensure!(
            !probe(path)?,
            "destination path already exists: {}",
            path.display()
        );
    }
    Ok(())
}

fn plan_extra_home_file(
    source: &Path,
    old_home: &Path,
    new_home: &Path,
    probe: &impl Fn(&Path) -> Result<bool>,
    checked: &mut BTreeSet<PathBuf>,
    copies: &mut Vec<HomeCopy>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if !exists(source)? {
        return Ok(());
    }
    if let Ok(relative) = source.strip_prefix(old_home) {
        let destination = new_home.join(relative);
        check_destination(&destination, probe, checked)?;
        copies.push(HomeCopy {
            source: source.to_path_buf(),
            destination,
            private: false,
        });
    } else {
        warnings.push(format!(
            "profile data outside source HOME: {}",
            source.display()
        ));
        if !probe(source)? {
            warnings.push(format!(
                "profile data is absent on destination: {}",
                source.display()
            ));
        }
    }
    Ok(())
}

fn tree_size(path: &Path) -> Result<u64> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut total = 0_u64;
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        total = total
            .checked_add(tree_size(&entry.path())?)
            .context("migration inventory size exceeds u64")?;
    }
    Ok(total)
}

fn inventory_layout(root: &Path, data: bool) -> Result<Vec<ClassifiedEntry>> {
    fn visit(
        root: &Path,
        path: &Path,
        data: bool,
        output: &mut Vec<ClassifiedEntry>,
    ) -> Result<()> {
        let relative = path.strip_prefix(root)?;
        let class = if data {
            classify_data_entry(relative)
        } else {
            classify_config_entry(relative)
        };
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        // These two mixed directories need their individual children classified.
        let depth = relative.components().count();
        let mixed = data
            && metadata.is_dir()
            && (((relative == Path::new("sessions")
                || (relative.starts_with("sessions") && depth == 2))
                && class == Classification::Known(MigrateAction::Copy))
                || relative == Path::new("workers"));
        if mixed {
            let entries = match std::fs::read_dir(path) {
                Ok(entries) => entries
                    .filter_map(|entry| match entry {
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        other => Some(other),
                    })
                    .collect::<std::io::Result<Vec<_>>>()?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            if entries.is_empty() {
                output.push(ClassifiedEntry {
                    path: relative.to_path_buf(),
                    classification: class,
                    size_bytes: 0,
                });
            } else {
                for entry in entries {
                    visit(root, &entry.path(), data, output)?;
                }
            }
        } else {
            output.push(ClassifiedEntry {
                path: relative.to_path_buf(),
                classification: class,
                size_bytes: match class {
                    Classification::Known(
                        MigrateAction::KeepWithLocalWorkers
                        | MigrateAction::Skip
                        | MigrateAction::Recreate,
                    )
                    | Classification::NestedStore => 0,
                    _ => tree_size(path)?,
                },
            });
        }
        Ok(())
    }
    let mut result = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(entries) = entries {
        for entry in entries {
            match entry {
                Ok(entry) => visit(root, &entry.path(), data, &mut result)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

fn inspect_ec2_resources(config_root: &Path) -> Result<(Vec<Ec2Resource>, Vec<String>)> {
    let root = config_root.join("ec2");
    if !root.exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut resources = Vec::new();
    let mut warnings = Vec::new();
    for entry in std::fs::read_dir(&root)? {
        let directory = entry?.path();
        if !directory.is_dir() {
            continue;
        }
        let path = directory.join("resources.json");
        if !exists(&path)? {
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)
            .with_context(|| format!("parse EC2 resource record {}", path.display()))?;
        ensure!(
            value.is_object(),
            "EC2 resource record {} is not an object",
            path.display()
        );
        let state = match value.get("state") {
            None => "active",
            Some(state) => state
                .as_str()
                .with_context(|| format!("invalid state in {}", path.display()))?,
        };
        match state {
            "preparing" => {
                anyhow::bail!("EC2 resource preparation is unfinished: {}", path.display())
            }
            "cleaned_up" => {}
            "active" => {
                let text = |key| {
                    value
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(ToOwned::to_owned)
                        .with_context(|| format!("missing {key} in {}", path.display()))
                };
                let Some(instance_id) = value
                    .get("instance_id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| !id.is_empty())
                else {
                    warnings.push(format!(
                        "active EC2 record {} has no instance ID; ingress cannot be inspected",
                        path.display()
                    ));
                    continue;
                };
                resources.push(Ec2Resource {
                    instance_id: instance_id.to_owned(),
                    region: text("region")?,
                    aws_profile: value
                        .get("aws_profile")
                        .and_then(serde_json::Value::as_str)
                        .map(ToOwned::to_owned),
                });
            }
            _ => anyhow::bail!("unknown EC2 resource state {state:?} in {}", path.display()),
        }
    }
    Ok((resources, warnings))
}

#[derive(Debug)]
pub struct Ec2IngressCheck {
    pub instance_id: String,
    pub security_group_ids: Vec<String>,
    pub admitted: bool,
    pub authorized_group: Option<String>,
}

/// Inspect ingress for B's IPv4, optionally authorizing one source group.
/// The caller must obtain the user's explicit choice before setting `authorize`.
pub fn check_ec2_ingress(
    client: &crate::controller::move_session::transport::ec2_grant::AwsCliEc2Client,
    resource: &Ec2Resource,
    destination_ip: std::net::Ipv4Addr,
    authorize: bool,
) -> Result<Ec2IngressCheck> {
    use crate::controller::move_session::transport::ec2_grant::{
        Ec2ApiError, Ec2GrantKey, Ec2GrantRoute, Ec2SecurityGroupClient,
    };
    let network = client.describe_instance(
        &resource.instance_id,
        Some(&resource.region),
        resource.aws_profile.as_deref(),
    )?;
    let mut admitted = false;
    for group in &network.security_group_ids {
        for cidr in client.describe_ssh_ingress_cidrs(
            group,
            Some(&resource.region),
            resource.aws_profile.as_deref(),
        )? {
            admitted |= cidr_contains(&cidr, destination_ip)?;
        }
    }
    let mut authorized_group = None;
    if !admitted && authorize {
        let group = network
            .security_group_ids
            .first()
            .context("instance has no security group to authorize")?;
        let key = Ec2GrantKey::ssh(group, format!("{destination_ip}/32"));
        let route = Ec2GrantRoute::new(Some(resource.region.clone()), resource.aws_profile.clone());
        match client.authorize_ingress(&key, &route, "Mjolnir migration destination SSH") {
            Ok(()) => authorized_group = Some(group.clone()),
            Err(Ec2ApiError::Duplicate) => {}
            Err(error) => return Err(error.into()),
        }
        admitted = true;
    }
    Ok(Ec2IngressCheck {
        instance_id: resource.instance_id.clone(),
        security_group_ids: network.security_group_ids,
        admitted,
        authorized_group,
    })
}

fn cidr_contains(cidr: &str, ip: std::net::Ipv4Addr) -> Result<bool> {
    let (network, prefix) = cidr.split_once('/').context("invalid IPv4 CIDR")?;
    let network: u32 = network.parse::<std::net::Ipv4Addr>()?.into();
    let prefix: u32 = prefix.parse()?;
    ensure!(prefix <= 32, "invalid IPv4 CIDR prefix");
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    Ok((u32::from(ip) & mask) == (network & mask))
}

/// Check a destination path without following its final symlink.
pub fn destination_path_exists(
    executor: &impl mj_core::targets::CommandExecutor,
    destination: &mj_core::targets::SshTarget,
    path: &Path,
) -> Result<bool> {
    use mj_core::targets::{SshTrafficClass, ssh_command};
    let path = path.to_str().context("destination path is not UTF-8")?;
    let command = ssh_command(
        SshTrafficClass::Control,
        destination,
        ["sh", "-c", "test -e \"$1\" || test -L \"$1\"", "mj", path],
    )
    .purpose("inspect migration destination path");
    let result = executor.execute(&command)?;
    match result.status {
        0 => Ok(true),
        1 => Ok(false),
        status => anyhow::bail!(
            "destination path probe failed ({status}): {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ),
    }
}

/// From B, verify its effective SSH configuration can reach A, the expected
/// store identity is present, and the container engines still exist on A.
pub fn probe_source_from_destination(
    executor: &impl mj_core::targets::CommandExecutor,
    destination: &mj_core::targets::SshTarget,
    source: &mj_core::targets::SshTarget,
    source_data_root: &Path,
    expected_identity: &str,
    engines: &[String],
) -> Result<String> {
    let identity = DataLayout::at(source_data_root).instance_identity();
    let identity = identity
        .to_str()
        .context("source instance-identity path is not UTF-8")?;
    let mut arguments = vec!["sh".to_owned(),"-c".to_owned(),"set -eu; id -un; test \"$(cat \"$1\")\" = \"$2\"; shift 2; for engine do command -v \"$engine\" >/dev/null; done".to_owned(),"mj".to_owned(),identity.to_owned(),expected_identity.to_owned()];
    arguments.extend(engines.iter().cloned());
    let output = probe_nested_ssh(executor, destination, source, arguments)?;
    Ok(String::from_utf8(output)?.trim().to_owned())
}

/// Probe a third host with the SSH configuration and credentials effective on B.
pub fn probe_third_host_from_destination(
    executor: &impl mj_core::targets::CommandExecutor,
    destination: &mj_core::targets::SshTarget,
    third_host: &mj_core::targets::SshTarget,
) -> Result<()> {
    probe_nested_ssh(executor, destination, third_host, vec!["true".into()])?;
    Ok(())
}

fn probe_nested_ssh(
    executor: &impl mj_core::targets::CommandExecutor,
    destination: &mj_core::targets::SshTarget,
    target: &mj_core::targets::SshTarget,
    arguments: Vec<String>,
) -> Result<Vec<u8>> {
    use mj_core::targets::{SshTrafficClass, ssh_command, ssh_command_owned};
    let mut inner_target = target.clone();
    inner_target
        .ssh_args
        .extend(["-o".into(), "BatchMode=yes".into()]);
    let inner = ssh_command_owned(SshTrafficClass::Control, &inner_target, arguments);
    let mut command = vec![inner.program];
    command.extend(inner.args);
    let outer = ssh_command(SshTrafficClass::Control, destination, command)
        .purpose("probe SSH from migration destination");
    let output = executor.execute(&outer)?;
    ensure!(
        output.status == 0,
        "destination cannot reach {} over SSH: {}",
        target.destination,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::config::{HarnessProfile, MigrateAction};
    use mj_core::state::TargetLocator;

    #[test]
    fn preflight_copies_complete_unmanaged_homes_and_refuses_collisions() {
        let root = tempfile::tempdir().unwrap();
        let old_home = root.path().join("old-home");
        let new_home = root.path().join("new-home");
        let old_data = root.path().join("old-data");
        let old_config = old_home.join(".config/mjolnir");
        let new_data = root.path().join("new-data");
        let new_config = new_home.join(".config/mjolnir");
        let profile_home = old_home.join(".codex-personal");
        std::fs::create_dir_all(profile_home.join("skills/.system")).unwrap();
        std::fs::write(
            profile_home.join("auth.json"),
            br#"{"refresh_token":"keep"}"#,
        )
        .unwrap();
        std::fs::write(profile_home.join("skills/.system/owned"), "owned").unwrap();
        std::fs::write(profile_home.join("history.jsonl"), "history").unwrap();
        std::fs::write(profile_home.join("custom-user-file"), "custom").unwrap();
        let opencode_home = old_home.join(".opencode-personal");
        std::fs::create_dir_all(opencode_home.join(".data/opencode")).unwrap();
        std::fs::write(opencode_home.join(".data/opencode/auth.json"), "auth").unwrap();
        std::fs::create_dir_all(old_home.join(".secrets")).unwrap();
        std::fs::write(old_home.join(".secrets/token"), "private").unwrap();
        std::fs::create_dir_all(old_data.join("sessions/one")).unwrap();
        std::fs::write(old_data.join("sessions/one/attachments.deleted"), "").unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut session = mj_store::database::test_session("one", "project");
        session.project_directory = Some(project);
        session.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: DataLayout::at(&old_data).worker_root("one"),
        }));
        session.checkpoint = None;
        let db_path = old_data.join("mj.sqlite3");
        mj_store::database::save_session_to(&db_path, &session).unwrap();
        let connection = Connection::open(db_path).unwrap();
        let mut config = Config::default();
        config.profiles.insert(
            "personal".into(),
            HarnessProfile::new(HarnessKind::Codex, profile_home.clone()),
        );
        config.profiles.insert(
            "shared".into(),
            HarnessProfile::new(HarnessKind::Codex, profile_home.clone()),
        );
        config.profiles.insert(
            "opencode".into(),
            HarnessProfile::new(HarnessKind::OpenCode, opencode_home.clone()),
        );
        config.profiles.insert(
            "missing".into(),
            HarnessProfile::new(HarnessKind::Codex, old_home.join("missing")),
        );
        config.profiles.insert(
            "managed".into(),
            HarnessProfile::new(
                HarnessKind::Codex,
                HarnessKind::Codex.managed_profile_home_in(&old_config, "managed"),
            ),
        );
        config.github.session_token_path = Some(old_home.join(".secrets/token"));
        let inspect = || {
            inspect_preflight(
                &connection,
                &config,
                &old_data,
                &old_config,
                &old_home,
                &new_home,
                &new_data,
                &new_config,
                "192.0.2.1",
                |path| Ok(path.symlink_metadata().is_ok()),
            )
        };
        let preflight = inspect().unwrap();
        assert!(
            preflight.preconditions.blockers.is_empty(),
            "{:?}",
            preflight.preconditions.blockers
        );
        assert!(preflight.data_entries.iter().any(|entry| entry.path
            == Path::new("sessions/one/attachments.deleted")
            && entry.classification == Classification::Known(MigrateAction::Copy)));
        assert_eq!(
            preflight
                .home_copies
                .iter()
                .filter(|entry| entry.source == profile_home)
                .count(),
            1
        );
        assert!(
            preflight
                .home_copies
                .iter()
                .any(|entry| entry.source == opencode_home)
        );
        assert!(
            !preflight
                .home_copies
                .iter()
                .any(|entry| entry.source.starts_with(&opencode_home)
                    && entry.source != opencode_home)
        );
        for file in ["history.jsonl", "custom-user-file", "skills/.system/owned"] {
            assert!(profile_home.join(file).exists());
            assert!(
                preflight
                    .home_copies
                    .iter()
                    .any(|copy| profile_home.join(file).starts_with(&copy.source))
            );
        }
        assert!(
            preflight
                .warnings
                .iter()
                .any(|warning| warning.contains("home does not exist on source"))
        );
        assert!(
            preflight
                .home_copies
                .iter()
                .any(|entry| entry.destination == new_home.join(".secrets/token") && entry.private)
        );
        assert!(
            preflight
                .home_copies
                .iter()
                .any(|entry| entry.source == profile_home)
        );
        assert!(
            preflight
                .warnings
                .iter()
                .any(|warning| warning.contains("IP literal"))
        );
        std::fs::create_dir_all(new_home.join(".codex-personal")).unwrap();
        assert!(
            inspect()
                .unwrap_err()
                .to_string()
                .contains("destination path already exists")
        );
        std::fs::remove_dir(new_home.join(".codex-personal")).unwrap();
        let resource = old_config.join("ec2/operation/resources.json");
        std::fs::create_dir_all(resource.parent().unwrap()).unwrap();
        std::fs::write(&resource, r#"{"state":"preparing"}"#).unwrap();
        assert!(
            inspect()
                .unwrap_err()
                .to_string()
                .contains("preparation is unfinished")
        );
        std::fs::write(&resource, "{").unwrap();
        assert!(format!("{:#}", inspect().unwrap_err()).contains("parse EC2 resource record"));
    }

    #[cfg(unix)]
    #[test]
    fn preflight_does_not_walk_local_worker_or_nested_store_trees() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let worker = data.join("workers/live-session/work");
        let nested = data.join("instances/other");
        std::fs::create_dir_all(&worker).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o000)).unwrap();
        let db = data.join("mj.sqlite3");
        mj_store::database::save_session_to(
            &db,
            &mj_store::database::test_session("one", "project"),
        )
        .unwrap();
        let connection = Connection::open(&db).unwrap();
        let result = inspect_preflight(
            &connection,
            &Config::default(),
            &data,
            &root.path().join("config"),
            &root.path().join("oldhome"),
            &root.path().join("newhome"),
            &root.path().join("newdata"),
            &root.path().join("newconfig"),
            "source",
            |_| Ok(false),
        );
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = result.unwrap();
        for path in ["workers/live-session", "instances"] {
            let entry = result
                .data_entries
                .iter()
                .find(|entry| entry.path == Path::new(path))
                .unwrap();
            assert_eq!(entry.size_bytes, 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn ec2_ingress_checks_port_ranges_and_authorizes_only_when_requested() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let aws = root.path().join("aws");
        std::fs::write(&aws, r##"#!/bin/sh
case "$*" in
  *describe-instances*) printf '%s' '{"Reservations":[{"Instances":[{"SecurityGroups":[{"GroupId":"sg-one"}]}]}]}' ;;
  *describe-security-groups*) printf '%s' '{"SecurityGroups":[{"IpPermissions":[{"IpProtocol":"tcp","FromPort":20,"ToPort":25,"IpRanges":[{"CidrIp":"203.0.113.0/24"}]}]}]}' ;;
  *authorize-security-group-ingress*) printf '%s' "$*" > "$0.auth"; printf '{}' ;;
  *) exit 1 ;;
esac
"##).unwrap();
        std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o700)).unwrap();
        let client = crate::controller::move_session::transport::ec2_grant::AwsCliEc2Client {
            program: aws.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let resource = Ec2Resource {
            instance_id: "i-test".into(),
            region: "region".into(),
            aws_profile: Some("profile".into()),
        };
        let admitted =
            check_ec2_ingress(&client, &resource, "203.0.113.7".parse().unwrap(), false).unwrap();
        assert!(admitted.admitted);
        let absent =
            check_ec2_ingress(&client, &resource, "192.0.2.7".parse().unwrap(), false).unwrap();
        assert!(!absent.admitted);
        assert!(!root.path().join("aws.auth").exists());
        let granted =
            check_ec2_ingress(&client, &resource, "192.0.2.7".parse().unwrap(), true).unwrap();
        assert_eq!(granted.authorized_group.as_deref(), Some("sg-one"));
        let call = std::fs::read_to_string(root.path().join("aws.auth")).unwrap();
        assert!(
            call.contains("192.0.2.7/32") && call.contains("--profile profile"),
            "{call}"
        );
    }
}
