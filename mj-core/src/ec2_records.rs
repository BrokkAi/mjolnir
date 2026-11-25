//! Reader for the local EC2 provisioning records that `mj ec2 create` writes to
//! `<config_dir>/ec2/<operation>/resources.json`.
//!
//! These records are the only authority for which EC2 instance backs an SSH
//! host: a direct Move never asks a remote host about itself (IMDS would be
//! answered by whatever the host, or a container on it, chooses to say).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::targets::{SshTarget, TargetLocator};

/// Directory holding one `<operation>/resources.json` record per managed host.
pub fn ec2_records_dir() -> PathBuf {
    crate::config::config_dir().join("ec2")
}

/// The EC2 instance behind a target and where to reach the AWS API for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ec2InstanceRef {
    pub instance_id: String,
    pub region: String,
    pub aws_profile: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StoredRecord {
    operation: String,
    region: String,
    #[serde(default)]
    aws_profile: Option<String>,
    #[serde(default)]
    instance_id: Option<String>,
    #[serde(default)]
    state: StoredState,
}

/// Mirrors `mj ec2`'s record state; legacy records without one are active.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StoredState {
    Preparing,
    #[default]
    Active,
    CleanedUp,
}

/// The EC2 instance behind `locator`, from the local records in
/// [`ec2_records_dir`].
pub fn lookup_ec2_host_for_locator(locator: &TargetLocator) -> Option<Ec2InstanceRef> {
    lookup_ec2_host_for_locator_in(locator, &ec2_records_dir())
}

/// [`lookup_ec2_host_for_locator`] against an explicit records root.
pub fn lookup_ec2_host_for_locator_in(
    locator: &TargetLocator,
    records_root: &Path,
) -> Option<Ec2InstanceRef> {
    match locator {
        TargetLocator::AwsEc2 {
            profile,
            region,
            instance_id,
            ..
        } => Some(Ec2InstanceRef {
            instance_id: instance_id.clone(),
            region: region.clone(),
            aws_profile: Some(profile.clone()).filter(|profile| !profile.is_empty()),
        }),
        TargetLocator::SshPodman { ssh, .. }
        | TargetLocator::SshNitro { ssh, .. }
        | TargetLocator::SshBare { ssh, .. } => identity_files(ssh)
            .into_iter()
            .find_map(|identity| record_for_identity(&identity, records_root)),
        _ => None,
    }
}

/// Identity files named by `-i <path>` or `-i<path>` in `ssh.ssh_args`.
fn identity_files(ssh: &SshTarget) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut args = ssh.ssh_args.iter();
    while let Some(arg) = args.next() {
        if arg == "-i" {
            if let Some(path) = args.next() {
                files.push(PathBuf::from(path));
            }
        } else if let Some(path) = arg.strip_prefix("-i").filter(|path| !path.is_empty()) {
            files.push(PathBuf::from(path));
        }
    }
    files
}

fn record_for_identity(identity: &Path, records_root: &Path) -> Option<Ec2InstanceRef> {
    if identity.file_name()? != "id_ed25519" {
        return None;
    }
    let directory = identity.parent()?;
    // Only a key directly inside the records root names a record; this also
    // rejects `..` components, which never compare equal to the root.
    if directory.parent()? != records_root {
        return None;
    }
    let operation = directory.file_name()?.to_str()?;
    let bytes = std::fs::read(directory.join("resources.json")).ok()?;
    let record: StoredRecord = serde_json::from_slice(&bytes).ok()?;
    if record.operation != operation || record.state != StoredState::Active {
        return None;
    }
    let instance_id = record.instance_id.filter(|id| !id.is_empty())?;
    Some(Ec2InstanceRef {
        instance_id,
        region: record.region,
        aws_profile: record.aws_profile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPERATION: &str = "0123456789abcdef0123456789abcdef";

    fn ssh(args: &[&str]) -> SshTarget {
        SshTarget {
            destination: "ubuntu@203.0.113.9".into(),
            ssh_args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    fn podman(args: &[&str]) -> TargetLocator {
        TargetLocator::SshPodman {
            ssh: ssh(args),
            container_id: "c1".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        }
    }

    fn write_record(root: &Path, directory: &str, body: &str) -> PathBuf {
        let directory = root.join(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("resources.json"), body).unwrap();
        directory.join("id_ed25519")
    }

    fn record(operation: &str, state: Option<&str>, instance: Option<&str>) -> String {
        let mut value = serde_json::json!({
            "operation": operation, "target": "box", "region": "eu-west-1",
            "account": "123", "aws_profile": "team",
        });
        if let Some(state) = state {
            value["state"] = state.into();
        }
        if let Some(instance) = instance {
            value["instance_id"] = instance.into();
        }
        value.to_string()
    }

    #[test]
    fn aws_ec2_locator_names_its_own_instance_without_reading_records() {
        let locator = TargetLocator::AwsEc2 {
            profile: String::new(),
            region: "us-west-2".into(),
            instance_id: "i-own".into(),
            ssh: ssh(&[]),
            workspace: "/w".into(),
        };
        let found = lookup_ec2_host_for_locator_in(&locator, Path::new("/nonexistent")).unwrap();
        assert_eq!(
            found,
            Ec2InstanceRef {
                instance_id: "i-own".into(),
                region: "us-west-2".into(),
                aws_profile: None,
            }
        );
    }

    #[test]
    fn container_and_bare_targets_resolve_through_the_managed_identity_key() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ec2");
        let key = write_record(
            &root,
            OPERATION,
            &record(OPERATION, Some("active"), Some("i-1")),
        );
        let expected = Ec2InstanceRef {
            instance_id: "i-1".into(),
            region: "eu-west-1".into(),
            aws_profile: Some("team".into()),
        };
        let key = key.to_str().unwrap();
        let attached = format!("-i{key}");
        for locator in [
            podman(&["-i", key]),
            podman(&[&attached]),
            TargetLocator::SshNitro {
                ssh: ssh(&["-o", "IdentitiesOnly=yes", "-i", key]),
                container_id: "vm".into(),
                borrowed_from: None,
            },
            TargetLocator::SshBare {
                ssh: ssh(&["-i", key]),
                workspace: "/w".into(),
                worker_id: None,
            },
        ] {
            assert_eq!(
                lookup_ec2_host_for_locator_in(&locator, &root).as_ref(),
                Some(&expected),
                "{locator:?}"
            );
        }
    }

    #[test]
    fn records_that_do_not_prove_a_live_instance_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ec2");
        let other = "fedcba9876543210fedcba9876543210";
        let lookup = |key: &Path| {
            lookup_ec2_host_for_locator_in(&podman(&["-i", key.to_str().unwrap()]), &root)
        };

        let cleaned = write_record(
            &root,
            OPERATION,
            &record(OPERATION, Some("cleaned_up"), Some("i-1")),
        );
        assert_eq!(lookup(&cleaned), None, "cleaned-up record");

        let preparing = write_record(&root, other, &record(other, Some("preparing"), Some("i-2")));
        assert_eq!(lookup(&preparing), None, "preparing record");

        let renamed = write_record(
            &root,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &record(OPERATION, None, Some("i-3")),
        );
        assert_eq!(lookup(&renamed), None, "operation does not match directory");

        let no_instance = write_record(
            &root,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            &record("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", None, None),
        );
        assert_eq!(lookup(&no_instance), None, "record without an instance");

        let legacy = write_record(
            &root,
            "cccccccccccccccccccccccccccccccc",
            &record("cccccccccccccccccccccccccccccccc", None, Some("i-4")),
        );
        assert_eq!(
            lookup(&legacy).map(|found| found.instance_id).as_deref(),
            Some("i-4"),
            "legacy record without a state is active"
        );
    }

    #[test]
    fn identity_keys_outside_the_records_root_never_name_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ec2");
        let key = write_record(
            &root,
            OPERATION,
            &record(OPERATION, Some("active"), Some("i-1")),
        );
        let lookup = |key: &Path| {
            lookup_ec2_host_for_locator_in(&podman(&["-i", key.to_str().unwrap()]), &root)
        };
        assert!(lookup(&key).is_some());

        // A different key name in a real record directory.
        assert_eq!(lookup(&key.with_file_name("id_rsa")), None);
        // Traversal that lexically lands on a record directory.
        let traversal = root
            .join("..")
            .join("ec2")
            .join(OPERATION)
            .join("id_ed25519");
        assert_eq!(lookup(&traversal), None);
        // A copy of the record under an unrelated directory.
        let elsewhere = write_record(
            dir.path(),
            "unrelated",
            &record("unrelated", Some("active"), Some("i-9")),
        );
        assert_eq!(lookup(&elsewhere), None);
        // One level too deep.
        let deep = write_record(
            &root.join("nested"),
            OPERATION,
            &record(OPERATION, Some("active"), Some("i-8")),
        );
        assert_eq!(lookup(&deep), None);
    }
}
