//! Settings belonging to an already selected target, independent of its template.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{SessionRecord, SessionResourceAllocation, TargetLocator};
use crate::config::{
    Environment, EnvironmentValue, ExecutionPolicy, Machine, SshConnection, TargetTemplate,
};
use crate::targets::AdditionalMount;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetRuntimeSettings {
    pub kind: String,
    pub connection: TargetConnection,
    pub execution_policy: ExecutionPolicy,
    /// Literal target environment entries. Values that name a secret live in
    /// `environment_references`, never here.
    pub environment: BTreeMap<String, String>,
    /// Entries whose value is read from the daemon's environment or secrets
    /// file at launch. Only the reference is persisted: the resolved secret
    /// must not reach SQLite, API responses, or container metadata, and a
    /// rotated secret applies at the next launch.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment_references: BTreeMap<String, EnvironmentValue>,
    /// Shared native build caches selected when this environment was created.
    /// Container mounts cannot follow subsequent machine configuration edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_cache: Option<ToolCachePlacement>,
    /// Immutable per-session selection used to create this container.
    /// Missing on legacy containers: their applied configuration is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_settings: Option<ContainerSettings>,
}

impl TargetRuntimeSettings {
    /// The target environment as a launch delivers it: the literal entries
    /// and the current value of every reference. This is the only place a
    /// recorded reference becomes a value, so a rotated secret applies at the
    /// next launch and a reference that no longer resolves fails by name.
    pub fn launch_environment(&self) -> anyhow::Result<BTreeMap<String, String>> {
        let mut environment = self.environment.clone();
        if !self.environment_references.is_empty() {
            let references = Environment::from_sources(self.environment_references.clone());
            references.ensure_resolved()?;
            environment.extend(references.into_resolved());
        }
        Ok(environment)
    }

    /// Whether the harness runs without approvals here: the recorded
    /// execution policy the worker launch enforces, so containers and SSH
    /// bare targets with `permissions = "yolo"` both qualify. The recorded
    /// policy is used instead of the template so a session keeps the mode
    /// it was launched with.
    pub fn is_yolo(&self) -> bool {
        self.execution_policy == ExecutionPolicy::Unconstrained
    }
}

/// Desired settings and the applied creation snapshot share one representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerSettings {
    pub additional_mounts: Vec<AdditionalMount>,
    pub resource_allocation: Option<SessionResourceAllocation>,
}

impl ContainerSettings {
    /// Candidate for an explicit engine inspection; this grants no authority
    /// to replace the environment or infer what image it actually runs.
    pub fn upgrade_supported(session: &SessionRecord) -> bool {
        Self::owns_container(session)
            && !matches!(
                session.target.as_deref(),
                Some(
                    TargetLocator::AppleContainer { .. } | TargetLocator::SshAppleContainer { .. }
                )
            )
    }

    pub fn requested(session: &SessionRecord) -> Self {
        Self {
            additional_mounts: session.additional_mounts.clone(),
            resource_allocation: session.resource_allocation.clone(),
        }
    }

    fn owns_container(session: &SessionRecord) -> bool {
        matches!(
            session.target.as_deref(),
            Some(
                TargetLocator::LocalPodman {
                    borrowed_from: None,
                    ..
                } | TargetLocator::LocalDocker {
                    borrowed_from: None,
                    ..
                } | TargetLocator::AppleContainer {
                    borrowed_from: None,
                    ..
                } | TargetLocator::SshPodman {
                    borrowed_from: None,
                    ..
                } | TargetLocator::SshDocker {
                    borrowed_from: None,
                    ..
                } | TargetLocator::SshAppleContainer {
                    borrowed_from: None,
                    ..
                } | TargetLocator::SshNitro {
                    borrowed_from: None,
                    ..
                }
            )
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCachePlacement {
    pub host: String,
    pub directory: PathBuf,
    /// Nx 23.2+ owns the cache and its database together under ~/.nx.
    pub nx_home: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TargetConnection {
    Local,
    Ssh {
        ssh: SshConnection,
    },
    Aws {
        profile: String,
        region: String,
        ssh_user: String,
        identity_file: Option<PathBuf>,
        ssh_args: Vec<String>,
    },
}

impl TargetRuntimeSettings {
    /// These recorded settings with the SSH options `current` now gives the
    /// same target, or `None` when there is nothing to take from it.
    ///
    /// A session's recorded access pins where its worker lives: the host, the
    /// login user, and the port. The other options (a key, a `ControlPath`,
    /// keepalives, host-key policy) say how to connect, so they follow the
    /// machine's current configuration, as a resume does since J-21. Settings
    /// of another kind, or that reach another place, are not taken.
    pub fn with_current_ssh_options(&self, current: &Self) -> Option<Self> {
        let (TargetConnection::Ssh { ssh: recorded }, TargetConnection::Ssh { ssh: now }) =
            (&self.connection, &current.connection)
        else {
            return None;
        };
        if self.kind != current.kind || recorded == now {
            return None;
        }
        if !same_ssh_place(recorded, now) {
            return None;
        }
        let mut refreshed = self.clone();
        refreshed.connection = TargetConnection::Ssh {
            ssh: SshConnection {
                identity_file: now.identity_file.clone(),
                extra_args: now.extra_args.clone(),
                ..recorded.clone()
            },
        };
        Some(refreshed)
    }
}

/// Whether two SSH connections reach the same place: the same host, login
/// user and port. The other options say how to connect, not where.
fn same_ssh_place(recorded: &SshConnection, now: &SshConnection) -> bool {
    let location = |ssh: &SshConnection| super::ManagedWorktreeTarget::Ssh {
        destination: match &ssh.user {
            Some(user) => format!("{user}@{}", ssh.host),
            None => ssh.host.clone(),
        },
        ssh_args: ssh.extra_args.clone(),
    };
    location(recorded).same_location(&location(now))
}

impl TargetRuntimeSettings {
    /// These recorded settings as `machine` now configures them, or `None`
    /// when `machine` is not where these settings reach: not an SSH machine,
    /// or another host, user or port. `Some` means the machine answers for
    /// this placement, whether or not its SSH options changed, so a caller
    /// stops there instead of asking the target template.
    pub fn following_machine(&self, machine: &Machine) -> Option<Self> {
        let Machine::Ssh { ssh, .. } = machine else {
            return None;
        };
        let TargetConnection::Ssh { ssh: recorded } = &self.connection else {
            return None;
        };
        if !same_ssh_place(recorded, ssh) {
            return None;
        }
        let mut refreshed = self.clone();
        refreshed.connection = TargetConnection::Ssh {
            ssh: SshConnection {
                identity_file: ssh.identity_file.clone(),
                extra_args: ssh.extra_args.clone(),
                ..recorded.clone()
            },
        };
        Some(refreshed)
    }
}

/// Whether `machine` is where a stored placement runs, judged from what the
/// placement itself recorded: the locator's SSH `host` and the frozen access
/// settings. A template that was repointed since the placement was made names
/// a machine that fails this test, and the placement stays unassigned rather
/// than taking the wrong machine. A legacy EC2 placement is never assigned:
/// its frozen settings hold the AWS profile, region and user but not the
/// launch template, so they cannot tell two machines in one account apart.
#[must_use]
pub fn machine_hosts_placement(
    machine: &Machine,
    locator_host: Option<&str>,
    runtime: Option<&TargetRuntimeSettings>,
) -> bool {
    match machine {
        Machine::Local { .. } => false,
        Machine::Ssh { ssh, .. } => {
            if locator_host != Some(ssh.host.as_str()) {
                return false;
            }
            match runtime.map(|runtime| &runtime.connection) {
                Some(TargetConnection::Ssh { ssh: recorded }) => same_ssh_place(recorded, ssh),
                // Without recorded access settings only the host name is known,
                // which cannot tell users or ports on one host apart.
                Some(_) | None => false,
            }
        }
        Machine::AwsEc2 { .. } => false,
    }
}

impl From<&TargetTemplate> for TargetRuntimeSettings {
    fn from(template: &TargetTemplate) -> Self {
        let connection = match template {
            TargetTemplate::SshBare { ssh, .. }
            | TargetTemplate::SshPodman { ssh, .. }
            | TargetTemplate::SshDocker { ssh, .. }
            | TargetTemplate::SshAppleContainer { ssh, .. }
            | TargetTemplate::SshNitro { ssh, .. } => TargetConnection::Ssh { ssh: ssh.clone() },
            TargetTemplate::AwsEc2 {
                aws_profile,
                region,
                ssh_user,
                identity_file,
                ssh_args,
                ..
            } => TargetConnection::Aws {
                profile: aws_profile.clone().unwrap_or_else(|| "default".into()),
                region: region.clone(),
                ssh_user: ssh_user.clone(),
                identity_file: identity_file.clone(),
                ssh_args: ssh_args.clone(),
            },
            _ => TargetConnection::Local,
        };
        let (environment, environment_references) = match template {
            TargetTemplate::LocalPodman { container }
            | TargetTemplate::LocalDocker { container }
            | TargetTemplate::AppleContainer { container }
            | TargetTemplate::SshPodman { container, .. }
            | TargetTemplate::SshDocker { container, .. }
            | TargetTemplate::SshAppleContainer { container, .. }
            | TargetTemplate::SshNitro { container, .. } => {
                let mut literals = BTreeMap::new();
                let mut references = BTreeMap::new();
                for (key, source) in container.environment.sources() {
                    match source {
                        EnvironmentValue::Literal(value) => {
                            literals.insert(key.clone(), value.clone());
                        }
                        reference => {
                            references.insert(key.clone(), reference.clone());
                        }
                    }
                }
                (literals, references)
            }
            _ => (BTreeMap::new(), BTreeMap::new()),
        };
        Self {
            kind: template.kind_name().into(),
            connection,
            execution_policy: template.execution_policy(),
            environment,
            environment_references,
            tool_cache: None,
            container_settings: None,
        }
    }
}

impl From<&TargetTemplate> for std::sync::Arc<TargetRuntimeSettings> {
    fn from(template: &TargetTemplate) -> Self {
        Self::new(TargetRuntimeSettings::from(template))
    }
}
