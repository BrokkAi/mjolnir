//! Settings belonging to an already selected target, independent of its template.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{SessionRecord, SessionResourceAllocation, TargetLocator};
use crate::config::{ExecutionPolicy, SshConnection, TargetTemplate};
use crate::targets::AdditionalMount;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetRuntimeSettings {
    pub kind: String,
    pub connection: TargetConnection,
    pub execution_policy: ExecutionPolicy,
    pub environment: BTreeMap<String, String>,
    /// Shared native build caches selected when this environment was created.
    /// Container mounts cannot follow subsequent machine configuration edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_cache: Option<ToolCachePlacement>,
    /// Immutable per-session selection used to create this container.
    /// Missing on legacy containers: their applied configuration is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_settings: Option<ContainerSettings>,
}

/// Desired settings and the applied creation snapshot share one representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerSettings {
    pub additional_mounts: Vec<AdditionalMount>,
    pub resource_allocation: Option<SessionResourceAllocation>,
    pub cpus: Option<String>,
    pub memory: Option<String>,
}

impl ContainerSettings {
    /// One policy for Restart and the daemon's confirmation projections.
    pub fn recreation_required(session: &SessionRecord) -> bool {
        let owns_container = matches!(
            session.target,
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
                }
            )
        );
        owns_container
            && session
                .target_runtime
                .as_ref()
                .and_then(|runtime| runtime.container_settings.as_ref())
                != Some(&Self::requested(session))
    }

    pub fn requested(session: &SessionRecord) -> Self {
        Self {
            additional_mounts: session.additional_mounts.clone(),
            resource_allocation: session.resource_allocation.clone(),
            cpus: session.container_cpus.clone(),
            memory: session.container_memory.clone(),
        }
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
        let location = |ssh: &SshConnection| super::ManagedWorktreeTarget::Ssh {
            destination: match &ssh.user {
                Some(user) => format!("{user}@{}", ssh.host),
                None => ssh.host.clone(),
            },
            ssh_args: ssh.extra_args.clone(),
        };
        if !location(recorded).same_location(&location(now)) {
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

impl From<&TargetTemplate> for TargetRuntimeSettings {
    fn from(template: &TargetTemplate) -> Self {
        let connection = match template {
            TargetTemplate::SshBare { ssh, .. }
            | TargetTemplate::SshPodman { ssh, .. }
            | TargetTemplate::SshDocker { ssh, .. } => TargetConnection::Ssh { ssh: ssh.clone() },
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
        let environment = match template {
            TargetTemplate::LocalPodman { container }
            | TargetTemplate::LocalDocker { container }
            | TargetTemplate::AppleContainer { container }
            | TargetTemplate::SshPodman { container, .. }
            | TargetTemplate::SshDocker { container, .. } => {
                container.environment.resolved().clone()
            }
            _ => BTreeMap::new(),
        };
        Self {
            kind: template.kind_name().into(),
            connection,
            execution_policy: template.execution_policy(),
            environment,
            tool_cache: None,
            container_settings: None,
        }
    }
}
