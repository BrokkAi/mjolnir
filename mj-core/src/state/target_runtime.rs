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
    /// Which image the container was created from, as far as upgrades care:
    /// `Some(Some(image))` for the default image of the Mjolnir that created
    /// it, `Some(None)` for a configured image, and `None` for a container
    /// recorded before Mjolnir tracked this. A release's default is its own
    /// version's image, so upgrading Mjolnir moves the default away from the
    /// recorded one and the container is due to be recreated from the new
    /// image. A configured image moves only when the person edits it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "recorded_default_image"
    )]
    pub default_image: Option<Option<String>>,
}

/// A present `default_image`, `null` included, is a recorded answer; only a
/// missing one is unknown.
fn recorded_default_image<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

impl ContainerSettings {
    /// One policy for Restart and the daemon's confirmation projections. A
    /// container whose image was never recorded counts as changed: it was
    /// created by an older Mjolnir, most likely from an older image.
    pub fn recreation_required(session: &SessionRecord) -> bool {
        Self::owns_container(session)
            && Self::applied_to(session).is_none_or(|applied| {
                applied.default_image.is_none() || *applied != Self::requested(session)
            })
    }

    /// Whether the session's container was created from a default image this
    /// Mjolnir no longer names: its image, and the worker baked into it, are
    /// an older release's.
    pub fn default_image_moved(session: &SessionRecord) -> bool {
        Self::owns_container(session)
            && Self::applied_to(session)
                .and_then(|applied| applied.default_image.as_ref())
                .and_then(Option::as_deref)
                .is_some_and(|image| image != crate::config::DEFAULT_CONTAINER_IMAGE)
    }

    /// Whether the session owns a container whose image was never recorded,
    /// so only the container itself can say what it runs.
    pub fn image_unrecorded(session: &SessionRecord) -> bool {
        Self::owns_container(session)
            && Self::applied_to(session).is_none_or(|applied| applied.default_image.is_none())
    }

    pub fn requested(session: &SessionRecord) -> Self {
        Self {
            additional_mounts: session.additional_mounts.clone(),
            resource_allocation: session.resource_allocation.clone(),
            cpus: session.container_cpus.clone(),
            memory: session.container_memory.clone(),
            // A container on the default image follows the default to the one
            // this Mjolnir names; a configured image stays as it was.
            default_image: Self::applied_to(session)
                .and_then(|applied| applied.default_image.as_ref())
                .map(|image| {
                    image
                        .as_ref()
                        .map(|_| crate::config::DEFAULT_CONTAINER_IMAGE.to_owned())
                }),
        }
    }

    /// The snapshot to record for a container just created from `image`.
    pub fn applied(session: &SessionRecord, image: &str) -> Self {
        Self {
            default_image: Some(
                (image == crate::config::DEFAULT_CONTAINER_IMAGE).then(|| image.to_owned()),
            ),
            ..Self::requested(session)
        }
    }

    fn applied_to(session: &SessionRecord) -> Option<&Self> {
        session
            .target_runtime
            .as_ref()
            .and_then(|runtime| runtime.container_settings.as_ref())
    }

    fn owns_container(session: &SessionRecord) -> bool {
        matches!(
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
