//! Settings belonging to an already selected target, independent of its template.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::{ExecutionPolicy, SshConnection, TargetTemplate};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetRuntimeSettings {
    pub kind: String,
    pub connection: TargetConnection,
    pub execution_policy: ExecutionPolicy,
    pub environment: BTreeMap<String, String>,
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
            | TargetTemplate::SshDocker { container, .. } => container.environment.clone(),
            _ => BTreeMap::new(),
        };
        Self {
            kind: template.kind_name().into(),
            connection,
            execution_policy: template.execution_policy(),
            environment,
        }
    }
}
