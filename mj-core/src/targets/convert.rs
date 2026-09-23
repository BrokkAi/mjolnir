//! Conversions from the stored target types to the execution-plan types.
//!
//! Two families of target types exist on purpose. [`crate::config`] and
//! [`crate::state`] hold what is written to the configuration file and the
//! session store: a kebab-case serde tag and `PathBuf` paths. This module's
//! targets hold what an execution plan needs: a snake_case tag and `String`
//! paths, because every path here ends up as an argv element or inside a
//! POSIX-quoted remote command string.
//!
//! Path text becomes `String` here and nowhere else, so the rest of the
//! workspace keeps handling paths as `Path`/`PathBuf`.

use std::path::Path;

use crate::config::{ContainerTemplate, PodmanWorkspaceStorage, SshConnection, TargetTemplate};
use crate::state::{
    PodmanWorkspaceLocator, TargetConnection, TargetLocator, TargetRuntimeSettings,
};
use crate::targets;

/// The single place path text crosses into an execution plan.
fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Why a stored locator and its target template cannot describe one target.
///
/// A session records its locator and its template independently, so a
/// configuration edit can leave the pair disagreeing. Each variant names what
/// disagreed rather than collapsing into one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetConversionError {
    RecordedKindMismatch {
        locator: &'static str,
        recorded: String,
    },
    InvalidRecordedConnection,
    /// The locator's target kind is not the template's target kind.
    KindMismatch {
        locator: &'static str,
        template: &'static str,
    },
    /// Both sides are SSH targets of the same kind, but name different hosts.
    SshHostMismatch {
        locator: String,
        template: String,
    },
    /// An EC2 locator was stored before its instance reported an address.
    MissingAwsAddress,
}

impl std::fmt::Display for TargetConversionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RecordedKindMismatch { locator, recorded } => write!(
                formatter,
                "session target kind {locator} differs from recorded kind {recorded}"
            ),
            Self::InvalidRecordedConnection => {
                formatter.write_str("recorded target connection has the wrong kind")
            }
            Self::KindMismatch { locator, template } => write!(
                formatter,
                "session locator/template mismatch: locator is {locator}, template is {template}"
            ),
            Self::SshHostMismatch { locator, template } => write!(
                formatter,
                "session locator/template SSH host mismatch: locator is {locator:?}, template is {template:?}"
            ),
            Self::MissingAwsAddress => formatter.write_str("AWS locator has no address"),
        }
    }
}

impl std::error::Error for TargetConversionError {}

/// OpenSSH arguments that keep `ssh` non-interactive.
///
/// Mjolnir drives ssh from a TUI; a host-key or password prompt would steal
/// the terminal and wedge provisioning. `BatchMode` fails fast instead of
/// prompting, and `accept-new` trusts a first-seen host key (fresh EC2
/// instances are always first-seen) while still rejecting changed keys.
/// User-supplied arguments come last so they can override.
pub fn ssh_args_with_identity(args: &[String], identity: Option<&Path>) -> Vec<String> {
    let mut result = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "ConnectTimeout=15".into(),
    ];
    result.extend(args.iter().cloned());
    if let Some(identity) = identity {
        result.push("-i".into());
        result.push(path_text(identity));
    }
    result
}

impl From<&SshConnection> for targets::SshTarget {
    fn from(ssh: &SshConnection) -> Self {
        let destination = match &ssh.user {
            Some(user) => format!("{user}@{}", ssh.host),
            None => ssh.host.clone(),
        };
        Self {
            destination,
            ssh_args: ssh_args_with_identity(&ssh.extra_args, ssh.identity_file.as_deref()),
        }
    }
}

impl TargetTemplate {
    /// The host that downloads this configured target's image, with the
    /// container settings that name the image. `None` for targets that run no
    /// image.
    ///
    /// This mirrors [`targets::TargetTemplate::image_host`] for the stored
    /// form of a target, which is what the daemon reads when it decides what
    /// to download; the two template families never share a type.
    pub fn image_host(&self) -> Option<(targets::ImageHost, &ContainerTemplate)> {
        match self {
            Self::LocalPodman { container } => Some((targets::ImageHost::LocalPodman, container)),
            Self::LocalDocker { container } => Some((targets::ImageHost::LocalDocker, container)),
            Self::AppleContainer { container } => {
                Some((targets::ImageHost::AppleContainer, container))
            }
            Self::SshPodman { ssh, container } => Some((
                targets::ImageHost::SshPodman(targets::SshTarget::from(ssh)),
                container,
            )),
            Self::SshDocker { ssh, container } => Some((
                targets::ImageHost::SshDocker(targets::SshTarget::from(ssh)),
                container,
            )),
            Self::LocalBare | Self::AwsEc2 { .. } | Self::SshBare { .. } => None,
        }
    }
}

impl From<&PodmanWorkspaceStorage> for targets::PodmanWorkspaceStorage {
    fn from(storage: &PodmanWorkspaceStorage) -> Self {
        match storage {
            PodmanWorkspaceStorage::PodmanVolume => Self::PodmanVolume,
            PodmanWorkspaceStorage::HostHelper { root, helper } => Self::HostHelper {
                root: path_text(root),
                helper: helper.clone(),
            },
            PodmanWorkspaceStorage::ContainerLayer => Self::ContainerLayer,
        }
    }
}

impl From<&PodmanWorkspaceLocator> for targets::PodmanWorkspaceLocator {
    fn from(storage: &PodmanWorkspaceLocator) -> Self {
        match storage {
            PodmanWorkspaceLocator::ContainerLayer => Self::ContainerLayer,
            PodmanWorkspaceLocator::Volume { name } => Self::Volume { name: name.clone() },
            PodmanWorkspaceLocator::HostPath {
                path,
                helper,
                resource,
            } => Self::HostPath {
                path: path_text(path),
                helper: helper.clone(),
                resource: resource.clone(),
            },
        }
    }
}

impl From<targets::PodmanWorkspaceLocator> for PodmanWorkspaceLocator {
    fn from(storage: targets::PodmanWorkspaceLocator) -> Self {
        match storage {
            targets::PodmanWorkspaceLocator::ContainerLayer => Self::ContainerLayer,
            targets::PodmanWorkspaceLocator::Volume { name } => Self::Volume { name },
            targets::PodmanWorkspaceLocator::HostPath {
                path,
                helper,
                resource,
            } => Self::HostPath {
                path: std::path::PathBuf::from(path),
                helper,
                resource,
            },
        }
    }
}

/// One session's stored target: what the store holds, plus the configuration
/// and the session identity the stored form deliberately does not repeat.
///
/// An SSH locator records only the host and an EC2 locator only the instance
/// and its resolved address, while an execution plan needs the whole `ssh`
/// invocation, the AWS profile and region, and the session's workspace path.
#[derive(Debug, Clone, Copy)]
pub struct StoredTarget<'a> {
    pub locator: &'a TargetLocator,
    pub template: &'a TargetTemplate,
    pub session_id: &'a str,
}

/// A provisioned target paired with its durable access settings.
pub struct RecordedTarget<'a> {
    pub locator: &'a TargetLocator,
    pub runtime: Option<&'a TargetRuntimeSettings>,
    pub session_id: &'a str,
}

impl TryFrom<StoredTarget<'_>> for targets::TargetLocator {
    type Error = TargetConversionError;
    fn try_from(stored: StoredTarget<'_>) -> Result<Self, Self::Error> {
        if locator_kind_name(stored.locator) != stored.template.kind_name() {
            return Err(TargetConversionError::KindMismatch {
                locator: locator_kind_name(stored.locator),
                template: stored.template.kind_name(),
            });
        }
        Self::try_from(RecordedTarget {
            locator: stored.locator,
            runtime: Some(&TargetRuntimeSettings::from(stored.template)),
            session_id: stored.session_id,
        })
    }
}

impl TryFrom<RecordedTarget<'_>> for targets::TargetLocator {
    type Error = TargetConversionError;
    fn try_from(stored: RecordedTarget<'_>) -> Result<Self, Self::Error> {
        let RecordedTarget {
            locator,
            runtime,
            session_id,
        } = stored;
        if let Some(runtime) = runtime {
            if locator_kind_name(locator) != runtime.kind {
                return Err(TargetConversionError::RecordedKindMismatch {
                    locator: locator_kind_name(locator),
                    recorded: runtime.kind.clone(),
                });
            }
            if !locator_needs_connection(locator) && runtime.connection != TargetConnection::Local {
                return Err(TargetConversionError::InvalidRecordedConnection);
            }
        }
        let ssh = |host: &str| -> Result<targets::SshTarget, TargetConversionError> {
            let Some(TargetConnection::Ssh { ssh }) = runtime.map(|runtime| &runtime.connection)
            else {
                return Err(TargetConversionError::InvalidRecordedConnection);
            };
            if host != ssh.host {
                return Err(TargetConversionError::SshHostMismatch {
                    locator: host.into(),
                    template: ssh.host.clone(),
                });
            }
            Ok(ssh.into())
        };
        Ok(match locator {
            TargetLocator::LocalBare { worker_root } => Self::LocalBare {
                worker_root: path_text(worker_root),
            },
            TargetLocator::LocalPodman {
                container_id,
                workspace_storage,
                borrowed_from,
            } => Self::LocalPodman {
                container_id: container_id.clone(),
                workspace_storage: workspace_storage.into(),
                borrowed_from: borrowed_from.clone(),
            },
            TargetLocator::LocalDocker {
                container_id,
                borrowed_from,
            } => Self::LocalDocker {
                container_id: container_id.clone(),
                borrowed_from: borrowed_from.clone(),
            },
            TargetLocator::AppleContainer {
                container_id,
                borrowed_from,
            } => Self::AppleContainer {
                container_id: container_id.clone(),
                borrowed_from: borrowed_from.clone(),
            },
            TargetLocator::SshBare {
                host,
                workspace,
                worker_id,
            } => Self::SshBare {
                ssh: ssh(host)?,
                workspace: path_text(workspace),
                worker_id: worker_id.clone(),
            },
            TargetLocator::SshPodman {
                host,
                container_id,
                workspace_storage,
                borrowed_from,
            } => Self::SshPodman {
                ssh: ssh(host)?,
                container_id: container_id.clone(),
                workspace_storage: workspace_storage.into(),
                borrowed_from: borrowed_from.clone(),
            },
            TargetLocator::SshDocker {
                host,
                container_id,
                borrowed_from,
            } => Self::SshDocker {
                ssh: ssh(host)?,
                container_id: container_id.clone(),
                borrowed_from: borrowed_from.clone(),
            },
            TargetLocator::AwsEc2 {
                instance_id,
                address,
            } => {
                let Some(TargetConnection::Aws {
                    profile,
                    region,
                    ssh_user,
                    identity_file,
                    ssh_args,
                }) = runtime.map(|runtime| &runtime.connection)
                else {
                    return Err(TargetConversionError::InvalidRecordedConnection);
                };
                let address = address
                    .as_deref()
                    .ok_or(TargetConversionError::MissingAwsAddress)?;
                Self::AwsEc2 {
                    profile: profile.clone(),
                    region: region.clone(),
                    instance_id: instance_id.clone(),
                    ssh: targets::SshTarget {
                        destination: format!("{ssh_user}@{address}"),
                        ssh_args: ssh_args_with_identity(ssh_args, identity_file.as_deref()),
                    },
                    workspace: targets::aws_workspace(session_id),
                }
            }
        })
    }
}

/// The stored locator's target kind, spelled as [`TargetTemplate::kind_name`]
/// spells it so a mismatch names both sides the same way.
const fn locator_kind_name(locator: &TargetLocator) -> &'static str {
    match locator {
        TargetLocator::LocalBare { .. } => "local-bare",
        TargetLocator::LocalPodman { .. } => "local-podman",
        TargetLocator::LocalDocker { .. } => "local-docker",
        TargetLocator::AppleContainer { .. } => "apple-container",
        TargetLocator::AwsEc2 { .. } => "aws-ec2",
        TargetLocator::SshBare { .. } => "ssh-bare",
        TargetLocator::SshPodman { .. } => "ssh-podman",
        TargetLocator::SshDocker { .. } => "ssh-docker",
    }
}

/// Local resource locators contain everything needed to reach the resource.
pub fn locator_needs_connection(locator: &TargetLocator) -> bool {
    matches!(
        locator,
        TargetLocator::SshBare { .. }
            | TargetLocator::SshPodman { .. }
            | TargetLocator::SshDocker { .. }
            | TargetLocator::AwsEc2 { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ssh_locator_checks_host_and_preserves_the_saved_connection() {
        let connection = SshConnection {
            host: "original.test".into(),
            user: Some("builder".into()),
            identity_file: Some("/keys/my key".into()),
            extra_args: vec!["-p".into(), "2222".into()],
        };
        let locators = [
            TargetLocator::SshBare {
                host: connection.host.clone(),
                workspace: "workspace".into(),
                worker_id: None,
            },
            TargetLocator::SshPodman {
                host: connection.host.clone(),
                container_id: "podman-id".into(),
                workspace_storage: Default::default(),
                borrowed_from: Some("owner".into()),
            },
            TargetLocator::SshDocker {
                host: connection.host.clone(),
                container_id: "docker-id".into(),
                borrowed_from: None,
            },
        ];
        for locator in locators {
            let mut runtime = TargetRuntimeSettings::from(&TargetTemplate::LocalBare);
            runtime.kind = locator_kind_name(&locator).into();
            runtime.connection = TargetConnection::Ssh {
                ssh: connection.clone(),
            };
            let backend = targets::TargetLocator::try_from(RecordedTarget {
                locator: &locator,
                runtime: Some(&runtime),
                session_id: "session",
            })
            .unwrap();
            let ssh = match backend {
                targets::TargetLocator::SshBare { ssh, .. }
                | targets::TargetLocator::SshPodman { ssh, .. }
                | targets::TargetLocator::SshDocker { ssh, .. } => ssh,
                _ => unreachable!(),
            };
            assert_eq!(ssh.destination, "builder@original.test");
            assert!(
                ssh.ssh_args
                    .windows(2)
                    .any(|args| args == ["-i", "/keys/my key"])
            );
            assert!(ssh.ssh_args.windows(2).any(|args| args == ["-p", "2222"]));
            let TargetConnection::Ssh { ssh } = &mut runtime.connection else {
                unreachable!()
            };
            ssh.host = "replacement.test".into();
            assert!(matches!(
                targets::TargetLocator::try_from(RecordedTarget {
                    locator: &locator,
                    runtime: Some(&runtime),
                    session_id: "session"
                }),
                Err(TargetConversionError::SshHostMismatch { .. })
            ));
            runtime.kind = "local-bare".into();
            assert!(matches!(
                targets::TargetLocator::try_from(RecordedTarget {
                    locator: &locator,
                    runtime: Some(&runtime),
                    session_id: "session"
                }),
                Err(TargetConversionError::RecordedKindMismatch { .. })
            ));
        }
    }

    #[test]
    fn recorded_ec2_access_preserves_region_profile_and_identity_without_launch_template() {
        let template: TargetTemplate = serde_json::from_value(serde_json::json!({
            "kind":"aws-ec2", "aws_profile":"production", "region":"eu-west-1",
            "launch_template":"creation-only", "ssh_user":"ubuntu", "identity_file":"/keys/ec2",
            "ssh_args":["-p","2222"]
        }))
        .unwrap();
        let runtime = TargetRuntimeSettings::from(&template);
        let encoded = serde_json::to_string(&runtime).unwrap();
        assert!(!encoded.contains("creation-only"));
        let runtime = serde_json::from_str(&encoded).unwrap();
        let locator = TargetLocator::AwsEc2 {
            instance_id: "i-original".into(),
            address: Some("10.0.0.1".into()),
        };
        let backend = targets::TargetLocator::try_from(RecordedTarget {
            locator: &locator,
            runtime: Some(&runtime),
            session_id: "session",
        })
        .unwrap();
        let targets::TargetLocator::AwsEc2 {
            profile,
            region,
            instance_id,
            ssh,
            ..
        } = backend
        else {
            unreachable!()
        };
        assert_eq!(
            (profile.as_str(), region.as_str(), instance_id.as_str()),
            ("production", "eu-west-1", "i-original")
        );
        assert_eq!(ssh.destination, "ubuntu@10.0.0.1");
        assert!(
            ssh.ssh_args
                .windows(2)
                .any(|args| args == ["-i", "/keys/ec2"])
        );
        assert!(ssh.ssh_args.windows(2).any(|args| args == ["-p", "2222"]));
    }
}
