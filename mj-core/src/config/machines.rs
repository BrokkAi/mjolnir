//! Machines (the hosts sessions run on) and the stored form of the runtimes
//! that run on them.
//!
//! The configuration file keeps these apart. `[machines.<id>]` describes a
//! host: this computer (always present, always called `local`), an SSH host,
//! or an EC2 launch template. Each machine owns its SSH connection details,
//! the remote directory bare runtimes keep workspaces in, and the mbx build
//! cache every runtime on that machine shares. `[targets.<id>]` describes a
//! runtime -- `bare`, `podman`, `docker` or `apple-container` -- and names the
//! machine it runs on.
//!
//! In memory nothing is split: every consumer still matches on the fused
//! [`TargetTemplate`], which pairs a host with a runtime. This module is the
//! only place that joins a stored runtime to its machine ([`resolve_target`])
//! and takes a fused target back apart ([`stored_target`]).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::{
    AwsAddressSource, ContainerTemplate, PermissionMode, SshConnection, TargetBuildCache,
    TargetTemplate, default_named_machine_prefix, deserialize_target_build_cache,
    is_default_target_build_cache, unique_config_id, validate_id, validate_workspace_prefix,
};

/// The id of the machine Mjolnir itself runs on. It always exists, whether or
/// not the file has a `[machines.local]` table.
pub const LOCAL_MACHINE_ID: &str = "local";

fn local_machine_id() -> String {
    LOCAL_MACHINE_ID.to_owned()
}

fn is_local_machine_id(id: &str) -> bool {
    id == LOCAL_MACHINE_ID
}

/// One host that runtimes run on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Machine {
    /// This computer. Only the id `local` may hold it.
    Local {
        #[serde(
            default,
            skip_serializing_if = "is_default_target_build_cache",
            deserialize_with = "deserialize_target_build_cache"
        )]
        build_cache: Option<TargetBuildCache>,
    },
    Ssh {
        #[serde(flatten)]
        ssh: SshConnection,
        /// Where bare runtimes on this machine keep their workspaces,
        /// relative to the login home.
        #[serde(default = "default_named_machine_prefix")]
        workspace_prefix: PathBuf,
        #[serde(
            default,
            skip_serializing_if = "is_default_target_build_cache",
            deserialize_with = "deserialize_target_build_cache"
        )]
        build_cache: Option<TargetBuildCache>,
    },
    /// An EC2 launch template. Instances from it run a bare harness only.
    AwsEc2 {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aws_profile: Option<String>,
        region: String,
        launch_template: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch_template_version: Option<String>,
        ssh_user: String,
        #[serde(default)]
        address_source: AwsAddressSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_file: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ssh_args: Vec<String>,
    },
}

impl Machine {
    /// The `kind` spelling used in the configuration file.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Local { .. } => "local",
            Self::Ssh { .. } => "ssh",
            Self::AwsEc2 { .. } => "aws-ec2",
        }
    }

    /// The build cache every runtime on this machine shares, when it has
    /// overrides of its own.
    #[must_use]
    pub const fn build_cache(&self) -> Option<&TargetBuildCache> {
        match self {
            Self::Local { build_cache } | Self::Ssh { build_cache, .. } => build_cache.as_ref(),
            // An EC2 instance is created per session, so it shares no cache.
            Self::AwsEc2 { .. } => None,
        }
    }

    fn set_build_cache(&mut self, value: Option<TargetBuildCache>) {
        match self {
            Self::Local { build_cache } | Self::Ssh { build_cache, .. } => *build_cache = value,
            Self::AwsEc2 { .. } => {}
        }
    }

    pub(super) fn validate(&self, id: &str) -> Result<()> {
        validate_id("machine", id)?;
        match self {
            Self::Local { build_cache } => {
                if !is_local_machine_id(id) {
                    bail!(
                        "machine {id:?} is this machine, which is always called {LOCAL_MACHINE_ID:?}"
                    );
                }
                if let Some(build_cache) = build_cache {
                    build_cache.validate(id)?;
                }
                Ok(())
            }
            Self::Ssh {
                ssh,
                workspace_prefix,
                build_cache,
            } => {
                if is_local_machine_id(id) {
                    bail!("machine {LOCAL_MACHINE_ID:?} is reserved for this machine");
                }
                ssh.validate(id)?;
                validate_workspace_prefix(&format!("machine {id}"), workspace_prefix)?;
                if let Some(build_cache) = build_cache {
                    build_cache.validate(id)?;
                }
                Ok(())
            }
            Self::AwsEc2 {
                aws_profile,
                region,
                launch_template,
                launch_template_version,
                ssh_user,
                ..
            } => {
                if is_local_machine_id(id) {
                    bail!("machine {LOCAL_MACHINE_ID:?} is reserved for this machine");
                }
                if region.trim().is_empty()
                    || launch_template.trim().is_empty()
                    || ssh_user.trim().is_empty()
                {
                    bail!("AWS machine {id:?} requires region, launch_template, and ssh_user");
                }
                if aws_profile.as_deref().is_some_and(str::is_empty)
                    || launch_template_version
                        .as_deref()
                        .is_some_and(str::is_empty)
                {
                    bail!("AWS machine {id:?} contains an empty optional value");
                }
                Ok(())
            }
        }
    }
}

/// Every machine in `machines`, validated, plus the checks that only make
/// sense across the whole collection.
pub(super) fn validate_machines(machines: &BTreeMap<String, Machine>) -> Result<()> {
    for (id, machine) in machines {
        machine.validate(id)?;
    }
    let ssh: Vec<(&String, &SshConnection)> = machines
        .iter()
        .filter_map(|(id, machine)| match machine {
            Machine::Ssh { ssh, .. } => Some((id, ssh)),
            _ => None,
        })
        .collect();
    for (index, (id, connection)) in ssh.iter().enumerate() {
        if let Some((other, _)) = ssh[..index]
            .iter()
            .find(|(_, candidate)| *candidate == *connection)
        {
            bail!("machines {other:?} and {id:?} describe the same host");
        }
    }
    Ok(())
}

/// One runtime as the configuration file stores it: a kind, the machine it
/// runs on, and the settings that belong to the runtime rather than the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StoredTarget {
    Bare {
        #[serde(
            default = "local_machine_id",
            skip_serializing_if = "is_local_machine_id"
        )]
        machine: String,
        /// Only meaningful on an SSH machine; a bare runtime on this machine
        /// always uses the configured approvals.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permissions: Option<PermissionMode>,
    },
    Podman {
        #[serde(
            default = "local_machine_id",
            skip_serializing_if = "is_local_machine_id"
        )]
        machine: String,
        #[serde(flatten)]
        container: ContainerTemplate,
    },
    Docker {
        #[serde(
            default = "local_machine_id",
            skip_serializing_if = "is_local_machine_id"
        )]
        machine: String,
        #[serde(flatten)]
        container: ContainerTemplate,
    },
    AppleContainer {
        #[serde(
            default = "local_machine_id",
            skip_serializing_if = "is_local_machine_id"
        )]
        machine: String,
        #[serde(flatten)]
        container: ContainerTemplate,
    },
}

/// The four runtime kinds a version 11 configuration may name.
pub(super) const STORED_TARGET_KINDS: [&str; 4] = ["bare", "podman", "docker", "apple-container"];

impl StoredTarget {
    /// The machine this runtime runs on.
    #[must_use]
    pub fn machine(&self) -> &str {
        match self {
            Self::Bare { machine, .. }
            | Self::Podman { machine, .. }
            | Self::Docker { machine, .. }
            | Self::AppleContainer { machine, .. } => machine,
        }
    }

    fn container(&self) -> Option<&ContainerTemplate> {
        match self {
            Self::Bare { .. } => None,
            Self::Podman { container, .. }
            | Self::Docker { container, .. }
            | Self::AppleContainer { container, .. } => Some(container),
        }
    }
}

/// The machine a runtime names, with `local` supplied even when the file has
/// no `[machines.local]` table.
fn machine_for(
    id: &str,
    machine_id: &str,
    machines: &BTreeMap<String, Machine>,
) -> Result<Machine> {
    if let Some(machine) = machines.get(machine_id) {
        return Ok(machine.clone());
    }
    if is_local_machine_id(machine_id) {
        return Ok(Machine::Local { build_cache: None });
    }
    bail!("target {id:?} names machine {machine_id:?}, which is not defined")
}

/// Join a stored runtime to its machine, producing the fused target every
/// other part of Mjolnir works with.
pub fn resolve_target(
    id: &str,
    target: &StoredTarget,
    machines: &BTreeMap<String, Machine>,
) -> Result<TargetTemplate> {
    let machine_id = target.machine();
    let machine = machine_for(id, machine_id, machines)?;
    if target
        .container()
        .is_some_and(|container| !is_default_target_build_cache(&container.build_cache))
    {
        bail!("target {id:?} sets build_cache, which now belongs to [machines.{machine_id}]");
    }
    let with_cache = |container: &ContainerTemplate| {
        let mut container = container.clone();
        container.build_cache = machine.build_cache().cloned();
        container
    };
    match target {
        StoredTarget::Bare { permissions, .. } => match &machine {
            Machine::Local { .. } => {
                if permissions.is_some() {
                    bail!(
                        "target {id:?} sets permissions, which only applies to a bare runtime on an SSH machine"
                    );
                }
                Ok(TargetTemplate::LocalBare)
            }
            Machine::Ssh {
                ssh,
                workspace_prefix,
                ..
            } => Ok(TargetTemplate::SshBare {
                ssh: ssh.clone(),
                permissions: permissions.unwrap_or(PermissionMode::Guardian),
                workspace_prefix: workspace_prefix.clone(),
            }),
            Machine::AwsEc2 {
                aws_profile,
                region,
                launch_template,
                launch_template_version,
                ssh_user,
                address_source,
                identity_file,
                ssh_args,
            } => {
                if permissions.is_some() {
                    bail!(
                        "target {id:?} sets permissions, which only applies to a bare runtime on an SSH machine"
                    );
                }
                Ok(TargetTemplate::AwsEc2 {
                    aws_profile: aws_profile.clone(),
                    region: region.clone(),
                    launch_template: launch_template.clone(),
                    launch_template_version: launch_template_version.clone(),
                    ssh_user: ssh_user.clone(),
                    address_source: address_source.clone(),
                    identity_file: identity_file.clone(),
                    ssh_args: ssh_args.clone(),
                })
            }
        },
        StoredTarget::Podman { container, .. } => match &machine {
            Machine::Local { .. } => Ok(TargetTemplate::LocalPodman {
                container: with_cache(container),
            }),
            Machine::Ssh { ssh, .. } => Ok(TargetTemplate::SshPodman {
                ssh: ssh.clone(),
                container: with_cache(container),
            }),
            Machine::AwsEc2 { .. } => {
                bail!("target {id:?}: an EC2 machine runs a bare harness only")
            }
        },
        StoredTarget::Docker { container, .. } => match &machine {
            Machine::Local { .. } => Ok(TargetTemplate::LocalDocker {
                container: with_cache(container),
            }),
            Machine::Ssh { ssh, .. } => Ok(TargetTemplate::SshDocker {
                ssh: ssh.clone(),
                container: with_cache(container),
            }),
            Machine::AwsEc2 { .. } => {
                bail!("target {id:?}: an EC2 machine runs a bare harness only")
            }
        },
        StoredTarget::AppleContainer { container, .. } => match &machine {
            Machine::Local { .. } => Ok(TargetTemplate::AppleContainer {
                container: with_cache(container),
            }),
            _ => bail!("target {id:?}: Apple container runs only on this machine"),
        },
    }
}

/// The id of the machine in `machines` equal to `wanted`, inserting it under a
/// collision-free id derived from `base` when it is not there yet.
fn machine_id_for(machines: &mut BTreeMap<String, Machine>, wanted: Machine, base: &str) -> String {
    if let Some((id, _)) = machines.iter().find(|(_, candidate)| **candidate == wanted) {
        return id.clone();
    }
    let id = unique_config_id(machines, base);
    machines.insert(id.clone(), wanted);
    id
}

/// The id of this machine, making sure `machines` holds it and that it owns
/// the build cache settings a local container runtime carried.
fn local_machine_for(
    machines: &mut BTreeMap<String, Machine>,
    build_cache: Option<&TargetBuildCache>,
    target_id: &str,
) -> String {
    let entry = machines
        .entry(LOCAL_MACHINE_ID.to_owned())
        .or_insert(Machine::Local { build_cache: None });
    fold_build_cache(entry, build_cache, LOCAL_MACHINE_ID, target_id);
    LOCAL_MACHINE_ID.to_owned()
}

/// The id of the SSH machine in `machines` reaching `ssh`, creating it when
/// absent. At most one machine may hold a given connection, so the connection
/// alone identifies it.
fn ssh_machine_id(
    machines: &mut BTreeMap<String, Machine>,
    ssh: &SshConnection,
    workspace_prefix: Option<&PathBuf>,
    build_cache: Option<&TargetBuildCache>,
    target_id: &str,
) -> String {
    let existing = machines
        .iter()
        .find(|(_, machine)| matches!(machine, Machine::Ssh { ssh: other, .. } if other == ssh))
        .map(|(id, _)| id.clone());
    let id = match existing {
        Some(id) => id,
        None => {
            let base = if validate_id("machine", &ssh.host).is_ok() {
                ssh.host.clone()
            } else {
                target_id.to_owned()
            };
            let id = unique_config_id(machines, &base);
            machines.insert(
                id.clone(),
                Machine::Ssh {
                    ssh: ssh.clone(),
                    workspace_prefix: workspace_prefix
                        .cloned()
                        .unwrap_or_else(default_named_machine_prefix),
                    build_cache: None,
                },
            );
            id
        }
    };
    let machine = machines.get_mut(&id).expect("machine just resolved");
    if let (
        Some(prefix),
        Machine::Ssh {
            workspace_prefix: current,
            ..
        },
    ) = (workspace_prefix, &mut *machine)
        && current != prefix
    {
        tracing::warn!(
            machine = id,
            target = target_id,
            "keeping the workspace directory already set for this machine and ignoring the one on this runtime"
        );
    }
    fold_build_cache(machine, build_cache, &id, target_id);
    id
}

/// Move a container runtime's build cache overrides onto its machine, which
/// owns them now. The first non-default setting for a machine wins.
fn fold_build_cache(
    machine: &mut Machine,
    build_cache: Option<&TargetBuildCache>,
    machine_id: &str,
    target_id: &str,
) {
    let Some(build_cache) = build_cache.filter(|cache| !cache.is_default()) else {
        return;
    };
    match machine.build_cache() {
        Some(existing) if existing != build_cache => tracing::warn!(
            machine = machine_id,
            target = target_id,
            "keeping the build cache settings already set for this machine and ignoring the ones on this runtime"
        ),
        Some(_) => {}
        None => machine.set_build_cache(Some(build_cache.clone())),
    }
}

/// Take a fused target apart: return the runtime as the file stores it, and
/// make sure `machines` holds the host it runs on.
///
/// This is both what saving does and how a configuration written before
/// version 11 is migrated, because a legacy `[targets.<id>]` table parses into
/// exactly the same fused [`TargetTemplate`].
pub fn stored_target(
    id: &str,
    target: &TargetTemplate,
    machines: &mut BTreeMap<String, Machine>,
) -> StoredTarget {
    let without_cache = |container: &ContainerTemplate| {
        let mut container = container.clone();
        container.build_cache = None;
        container
    };
    match target {
        TargetTemplate::LocalBare => StoredTarget::Bare {
            machine: LOCAL_MACHINE_ID.to_owned(),
            permissions: None,
        },
        TargetTemplate::LocalPodman { container } => StoredTarget::Podman {
            machine: local_machine_for(machines, container.build_cache.as_ref(), id),
            container: without_cache(container),
        },
        TargetTemplate::LocalDocker { container } => StoredTarget::Docker {
            machine: local_machine_for(machines, container.build_cache.as_ref(), id),
            container: without_cache(container),
        },
        TargetTemplate::AppleContainer { container } => StoredTarget::AppleContainer {
            machine: local_machine_for(machines, container.build_cache.as_ref(), id),
            container: without_cache(container),
        },
        TargetTemplate::SshBare {
            ssh,
            permissions,
            workspace_prefix,
        } => StoredTarget::Bare {
            machine: ssh_machine_id(machines, ssh, Some(workspace_prefix), None, id),
            permissions: Some(*permissions),
        },
        TargetTemplate::SshPodman { ssh, container } => StoredTarget::Podman {
            machine: ssh_machine_id(machines, ssh, None, container.build_cache.as_ref(), id),
            container: without_cache(container),
        },
        TargetTemplate::SshDocker { ssh, container } => StoredTarget::Docker {
            machine: ssh_machine_id(machines, ssh, None, container.build_cache.as_ref(), id),
            container: without_cache(container),
        },
        TargetTemplate::AwsEc2 {
            aws_profile,
            region,
            launch_template,
            launch_template_version,
            ssh_user,
            address_source,
            identity_file,
            ssh_args,
        } => StoredTarget::Bare {
            machine: machine_id_for(
                machines,
                Machine::AwsEc2 {
                    aws_profile: aws_profile.clone(),
                    region: region.clone(),
                    launch_template: launch_template.clone(),
                    launch_template_version: launch_template_version.clone(),
                    ssh_user: ssh_user.clone(),
                    address_source: address_source.clone(),
                    identity_file: identity_file.clone(),
                    ssh_args: ssh_args.clone(),
                },
                id,
            ),
            permissions: None,
        },
    }
}

/// The version 11 spelling a configuration written before the split should
/// use, named in the error a version 11 file gets for an old `kind`.
pub(super) fn legacy_kind_advice(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "local-bare" => Some(("bare", LOCAL_MACHINE_ID)),
        "local-podman" => Some(("podman", LOCAL_MACHINE_ID)),
        "local-docker" => Some(("docker", LOCAL_MACHINE_ID)),
        "ssh-bare" => Some((
            "bare",
            "the id of an [machines.<id>] entry with kind = \"ssh\"",
        )),
        "ssh-podman" => Some((
            "podman",
            "the id of an [machines.<id>] entry with kind = \"ssh\"",
        )),
        "ssh-docker" => Some((
            "docker",
            "the id of an [machines.<id>] entry with kind = \"ssh\"",
        )),
        "aws-ec2" => Some((
            "bare",
            "the id of an [machines.<id>] entry with kind = \"aws-ec2\"",
        )),
        _ => None,
    }
}
