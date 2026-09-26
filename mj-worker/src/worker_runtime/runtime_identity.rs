//! Inspect the selected installation on the worker, never the controller's pins.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use mj_core::config::HarnessKind;
use mj_core::harness_runtime::{RuntimeComponent, RuntimeIdentity, RuntimeProvenance};
use mj_core::worker_launch::worker_executable_digest;
use sha2::{Digest, Sha256};

pub(super) fn inspect(
    harness: HarnessKind,
    command: &Path,
    environment: &BTreeMap<String, String>,
    managed_root: Option<&Path>,
) -> RuntimeIdentity {
    let mut identity = RuntimeIdentity {
        id: None,
        harness,
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        provenance: if managed_root.is_some() {
            RuntimeProvenance::ManagedInstallation
        } else {
            RuntimeProvenance::TargetInstallation
        },
        components: Vec::new(),
        unavailable_reason: None,
    };
    if let Err(error) = inspect_components(&mut identity, command, environment, managed_root) {
        tracing::warn!(%error, "target runtime identity is unavailable");
        identity.unavailable_reason = Some(
            "The selected bridge/provider installation could not be fully identified on the target"
                .into(),
        );
    }
    // Serialization contains only strings and enums.
    if let Err(error) = identity.refresh_id() {
        tracing::error!(%error, "runtime identity serialization failed");
        identity.id = None;
        identity.unavailable_reason = Some("Runtime identity could not be encoded".into());
    }
    identity
}

fn inspect_components(
    identity: &mut RuntimeIdentity,
    command: &Path,
    environment: &BTreeMap<String, String>,
    managed_root: Option<&Path>,
) -> Result<()> {
    let command = resolve_command(command, environment)?;
    if let Some(root) = managed_root {
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("mj-harness.json"))?)?;
        ensure!(
            manifest["harness"] == identity.harness.id(),
            "managed runtime manifest names a different harness"
        );
        let version = manifest["install_id"]
            .as_str()
            .context("managed installation has no identity")?;
        identity.components.push(RuntimeComponent {
            name: "managed_installation".into(),
            version: Some(version.into()),
            sha256: Some(installation_digest(root)?),
        });
    }
    identity.components.push(RuntimeComponent {
        name: "bridge_entrypoint".into(),
        version: None,
        sha256: Some(worker_executable_digest(&command)?),
    });
    match identity.harness {
        HarnessKind::Codex => {
            let bridge = package_root(&command, "@brokkai/codex-acp")?;
            add_package(identity, "acp_bridge", &bridge)?;
            // Both the default image and the managed installer select this
            // explicitly. Without it we cannot prove the bridge's provider.
            let provider = environment
                .get("CODEX_PATH")
                .context("CODEX_PATH is not explicit")?;
            let provider = resolve_command(Path::new(provider), environment)?;
            add_package(
                identity,
                "provider_cli",
                &package_root(&provider, "@openai/codex")?,
            )?;
        }
        HarnessKind::Claude => {
            let bridge = package_root(&command, "@agentclientprotocol/claude-agent-acp")?;
            add_package(identity, "acp_bridge", &bridge)?;
            let sdk = bridge
                .ancestors()
                .map(|directory| directory.join("node_modules/@anthropic-ai/claude-agent-sdk"))
                .find(|directory| directory.join("package.json").is_file())
                .context("provider SDK installation is unavailable")?;
            add_package(identity, "provider_sdk", &sdk)?;
        }
        HarnessKind::Muse => {
            let provider = environment
                .get("MUSE_CLI")
                .context("MUSE_CLI is not explicit")?;
            let provider = resolve_command(Path::new(provider), environment)?;
            let version = if managed_root.is_some() {
                None // The leased installation manifest names both versions.
            } else {
                let metadata: serde_json::Value =
                    serde_json::from_slice(&std::fs::read("/opt/mjolnir/muse-runtime.json")?)?;
                Some(
                    metadata["muse_version"]
                        .as_str()
                        .context("Muse version is unavailable")?
                        .to_owned(),
                )
            };
            identity.components.push(RuntimeComponent {
                name: "provider_cli".into(),
                version,
                sha256: Some(worker_executable_digest(&provider)?),
            });
        }
        HarnessKind::Kimi | HarnessKind::Grok => {
            ensure!(
                managed_root.is_some(),
                "native target runtime has no managed installation manifest"
            );
        }
    }
    Ok(())
}

pub(super) fn resolve_command(
    command: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<PathBuf> {
    let selected = if command.is_absolute() {
        command.to_path_buf()
    } else {
        ensure!(
            command.components().count() == 1,
            "relative runtime command is ambiguous"
        );
        std::env::split_paths(
            environment
                .get("PATH")
                .context("runtime PATH is unavailable")?,
        )
        .map(|directory| directory.join(command))
        .find(|path| super::harness::entrypoint_is_executable(path))
        .context("runtime command is not on the selected PATH")?
    };
    ensure!(
        super::harness::entrypoint_is_executable(&selected),
        "selected runtime command is not executable"
    );
    selected
        .canonicalize()
        .context("resolve selected runtime command")
}

fn package_root(command: &Path, expected: &str) -> Result<PathBuf> {
    for directory in command.ancestors().skip(1) {
        let path = directory.join("package.json");
        match std::fs::read(&path) {
            Ok(bytes) => {
                let metadata: serde_json::Value = serde_json::from_slice(&bytes)?;
                ensure!(
                    metadata["name"] == expected,
                    "selected runtime belongs to an unexpected package"
                );
                return Ok(directory.to_path_buf());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("selected runtime has no package metadata")
}

fn add_package(identity: &mut RuntimeIdentity, name: &str, root: &Path) -> Result<()> {
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("package.json"))?)?;
    let version = metadata["version"]
        .as_str()
        .context("runtime package version is unavailable")?;
    identity.components.push(RuntimeComponent {
        name: name.into(),
        version: Some(version.into()),
        // npm may hoist the provider binary and dependencies beside the package.
        sha256: Some(installation_digest(
            root.ancestors()
                .find(|path| path.file_name().is_some_and(|name| name == "node_modules"))
                .unwrap_or(root),
        )?),
    });
    Ok(())
}

/// Content comparison, unlike checkpoint's metadata/mtime change detector.
/// Never traverse a linked directory outside the installation. Interpreter
/// links are files; their contents participate without publishing their paths.
fn installation_digest(root: &Path) -> Result<String> {
    fn visit(root: &Path, directory: &Path, digest: &mut Sha256) -> Result<()> {
        let mut entries = std::fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name();
            if name == ".lease"
                || name == "__pycache__"
                || entry.path().extension().is_some_and(|ext| ext == "pyc")
            {
                continue;
            }
            let path = entry.path();
            digest.update(path.strip_prefix(root)?.as_os_str().as_encoded_bytes());
            digest.update([0]);
            let kind = entry.file_type()?;
            if kind.is_dir() {
                digest.update(b"directory\0");
                visit(root, &path, digest)?;
            } else if kind.is_file() || (kind.is_symlink() && path.is_file()) {
                digest.update(b"file\0");
                digest.update(worker_executable_digest(&path)?.as_bytes());
            } else if kind.is_symlink() {
                let target = path.canonicalize()?;
                ensure!(
                    target.starts_with(root),
                    "runtime links a directory outside its installation"
                );
                digest.update(b"link\0");
                digest.update(target.strip_prefix(root)?.as_os_str().as_encoded_bytes());
            } else {
                anyhow::bail!("runtime installation contains a non-file entry");
            }
        }
        Ok(())
    }
    let mut digest = Sha256::new();
    visit(root, root, &mut digest)?;
    Ok(mj_core::hex::lower_hex(digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn executable(path: &Path, contents: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn package(root: &Path, name: &str, version: &str) -> PathBuf {
        let command = root.join("bin/cli.js");
        executable(&command, b"#!/usr/bin/env node\n");
        std::fs::write(
            root.join("package.json"),
            serde_json::to_vec(&serde_json::json!({"name": name, "version": version})).unwrap(),
        )
        .unwrap();
        command
    }

    #[test]
    fn runtime_identity_uses_target_versions_and_provider_contents_without_secrets() {
        let temp = tempfile::tempdir().unwrap();
        let bridge = package(
            &temp.path().join("node_modules/@brokkai/codex-acp"),
            "@brokkai/codex-acp",
            "custom-bridge",
        );
        let provider = package(
            &temp.path().join("node_modules/@openai/codex"),
            "@openai/codex",
            "custom-provider",
        );
        let environment = BTreeMap::from([
            ("CODEX_PATH".into(), provider.display().to_string()),
            ("PRIVATE_TOKEN".into(), "never-publish-this".into()),
        ]);
        let before = inspect(HarnessKind::Codex, &bridge, &environment, None);
        assert!(before.id.is_some(), "{before:?}");
        assert_eq!(before.provenance, RuntimeProvenance::TargetInstallation);
        assert!(
            before
                .components
                .iter()
                .any(|part| part.version.as_deref() == Some("custom-provider"))
        );
        let public = serde_json::to_string(&before).unwrap();
        assert!(!public.contains("never-publish-this"));
        assert!(!public.contains(&temp.path().display().to_string()));
        before.require(before.id.as_ref().unwrap()).unwrap();
        // Same package version, different hoisted provider binary bytes.
        executable(
            &temp
                .path()
                .join("node_modules/@openai/codex-linux-x64/codex"),
            &vec![b'x'; 100_000],
        );
        let after = inspect(HarnessKind::Codex, &bridge, &environment, None);
        assert!(after.require(before.id.as_ref().unwrap()).is_err());
        assert_ne!(before.id, after.id);
    }

    #[test]
    fn runtime_identity_unknown_custom_image_never_matches() {
        let temp = tempfile::tempdir().unwrap();
        let command = temp.path().join("custom-bridge");
        executable(&command, b"#!/bin/sh\nexit 0\n");
        let identity = inspect(HarnessKind::Codex, &command, &BTreeMap::new(), None);
        assert!(identity.id.is_none());
        assert!(identity.unavailable_reason.is_some());
        assert!(
            identity
                .require("saved-selection")
                .unwrap_err()
                .to_string()
                .contains("unavailable")
        );
    }

    #[test]
    fn runtime_identity_managed_installation_survives_lease_and_python_cache_changes() {
        let temp = tempfile::tempdir().unwrap();
        let command = temp.path().join("bin/kimi");
        executable(&command, b"#!/bin/sh\nexit 0\n");
        std::fs::write(
            temp.path().join("mj-harness.json"),
            r#"{"harness":"kimi","install_id":"kimi-test"}"#,
        )
        .unwrap();
        let first = inspect(
            HarnessKind::Kimi,
            &command,
            &BTreeMap::new(),
            Some(temp.path()),
        );
        assert!(first.id.is_some(), "{first:?}");
        assert_eq!(first.provenance, RuntimeProvenance::ManagedInstallation);
        std::fs::write(temp.path().join(".lease"), b"locked").unwrap();
        std::fs::create_dir(temp.path().join("__pycache__")).unwrap();
        std::fs::write(temp.path().join("__pycache__/x.pyc"), b"compiled").unwrap();
        let second = inspect(
            HarnessKind::Kimi,
            &command,
            &BTreeMap::new(),
            Some(temp.path()),
        );
        assert_eq!(first.id, second.id);
    }
}
