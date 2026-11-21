//! A reviewer uses the session's login and provider settings in a private home
//! with no session-owned hooks, permission exceptions, or MCP registrations.

use super::*;
#[cfg(unix)]
use std::path::Path;

pub(super) const SESSION_ENVIRONMENT: &[&str] = &[
    mj_core::worker_launch::SESSION_MANAGED_SUBAGENT_ENV,
    mj_core::worker_launch::SESSION_MESSAGE_MCP_ENV,
    mj_core::worker_launch::SESSION_RAW_LAUNCH_RECORD_ENV,
    "MJOLNIR_MAILBOX_HOOK",
    "MUSE_ACP_ALLOW_COMMAND_PREFIXES",
];

pub(super) fn remove_session_environment(environment: &mut BTreeMap<String, String>) {
    for name in SESSION_ENVIRONMENT {
        environment.remove(*name);
    }
}

#[cfg(unix)]
pub(super) fn stage(
    harness: HarnessKind,
    environment: &mut BTreeMap<String, String>,
    args: &mut [String],
) -> Result<Option<tempfile::TempDir>> {
    if matches!(harness, HarnessKind::OpenCode | HarnessKind::Dsh) {
        return Ok(None);
    }
    let Some(source) = environment.get(harness.home_env()) else {
        // Standalone bridges can supply a self-contained command instead of
        // a worker-staged harness profile.
        return Ok(None);
    };
    let source = harness.home_from_environment(source);
    let profile = tempfile::Builder::new().prefix("mj-guardian-").tempdir()?;
    let home = if harness.nested_home() {
        profile.path().join(harness.id())
    } else {
        profile.path().to_owned()
    };
    std::fs::create_dir_all(&home)?;
    if harness == HarnessKind::Kimi {
        // Keep the OAuth files and their locks under one owner. Nothing else
        // from the session's Kimi home (notably mcp.json) is shared.
        let credentials = source.join("credentials");
        if credentials.try_exists()? {
            std::os::unix::fs::symlink(credentials, home.join("credentials"))?;
        }
        copy_file(&source, &home, "device_id")?;
    } else {
        copy_file(&source, &home, harness.credential_file_name())?;
    }
    match harness {
        HarnessKind::Codex => {
            stage_toml(&source, &home, |config| {
                for key in ["hooks", "mcp_servers", "plugins", "sandbox_mode"] {
                    config.remove(key);
                }
                config.insert("default_permissions".into(), ":read-only".into());
                if let Some(catalog) = config.get_mut("model_catalog_json")
                    && let Some(path) = catalog
                        .as_str()
                        .map(Path::new)
                        .filter(|path| path.is_relative())
                {
                    *catalog = mj_core::target_path::text(&source.join(path)).into();
                }
            })?;
        }
        HarnessKind::Claude => {
            stage_json(&source, &home, ".claude.json", &["mcpServers", "projects"])?;
            stage_json(
                &source,
                &home,
                "settings.json",
                &[
                    "hooks",
                    "permissions",
                    "enabledPlugins",
                    "extraKnownMarketplaces",
                ],
            )?;
        }
        HarnessKind::Antigravity => {
            let home_args = harness.home_cli_args(&home);
            if let Some(index) = args.windows(2).position(|pair| pair[0] == home_args[0]) {
                args[index + 1].clone_from(&home_args[1]);
            }
        }
        HarnessKind::Kimi | HarnessKind::Grok => {
            stage_toml(&source, &home, |config| {
                for key in ["hooks", "mcp_servers", "plugins", "permission"] {
                    config.remove(key);
                }
            })?;
            if harness == HarnessKind::Grok {
                copy_file(&source, &home, "agent_id")?;
            }
        }
        HarnessKind::Muse => {
            stage_json(&source, &home, "settings.json", &["hooks", "permissions"])?;
            copy_file(&source, &home, "trust.json")?;
            let catalog = Path::new(".data/muse/model-catalog");
            if source.join(catalog).is_dir() {
                for entry in std::fs::read_dir(source.join(catalog))? {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        copy_file(&source, &home, catalog.join(entry.file_name()))?;
                    }
                }
            }
        }
        HarnessKind::OpenCode | HarnessKind::Dsh => unreachable!(),
    }
    harness.configure_staged_home_environment(&home, environment);
    Ok(Some(profile))
}

#[cfg(unix)]
fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read reviewer input {}", path.display())),
    }
}

#[cfg(unix)]
fn copy_file(source: &Path, home: &Path, file: impl AsRef<Path>) -> Result<()> {
    let file = file.as_ref();
    if let Some(bytes) = read_optional(&source.join(file))? {
        mj_core::config::atomic_write(&home.join(file), &bytes)?;
    }
    Ok(())
}

#[cfg(unix)]
fn stage_toml(source: &Path, home: &Path, edit: impl FnOnce(&mut toml::Table)) -> Result<()> {
    let Some(bytes) = read_optional(&source.join("config.toml"))? else {
        return Ok(());
    };
    let mut config = toml::from_str(std::str::from_utf8(&bytes)?)
        .context("parse staged reviewer TOML configuration")?;
    edit(&mut config);
    mj_core::config::atomic_write(
        &home.join("config.toml"),
        toml::to_string(&config)?.as_bytes(),
    )
}

#[cfg(unix)]
fn stage_json(source: &Path, home: &Path, file: &str, remove: &[&str]) -> Result<()> {
    let Some(bytes) = read_optional(&source.join(file))? else {
        return Ok(());
    };
    let mut config: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&bytes).with_context(|| format!("parse staged reviewer {file}"))?;
    for key in remove {
        config.remove(*key);
    }
    mj_core::config::atomic_write(&home.join(file), &serde_json::to_vec(&config)?)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn guardian_profiles_keep_login_material_without_session_hooks_and_mcp() {
        let source = tempfile::tempdir().unwrap();
        for harness in [
            HarnessKind::Codex,
            HarnessKind::Claude,
            HarnessKind::Antigravity,
            HarnessKind::Kimi,
        ] {
            let home = source.path().join(harness.id());
            let login = home.join(harness.credential_file_name());
            mj_core::config::atomic_write(&login, b"synthetic login").unwrap();
            mj_core::config::atomic_write(
                &home.join("mcp.json"),
                br#"{"mcpServers":{"mj-agents": {}}}"#,
            )
            .unwrap();
            let mut args = harness.home_cli_args(&home);
            let mut environment = BTreeMap::new();
            harness.configure_staged_home_environment(&home, &mut environment);
            match harness {
                HarnessKind::Codex => {
                    std::fs::write(
                        home.join("config.toml"),
                        "[hooks]\n[mcp_servers.mj-agents]\n",
                    )
                    .unwrap();
                }
                HarnessKind::Claude => {
                    std::fs::write(
                        home.join("settings.json"),
                        br#"{"hooks":{"PostToolBatch": []},"env":{"CLAUDE_MODEL":"custom"}}"#,
                    )
                    .unwrap();
                    std::fs::write(
                        home.join(".claude.json"),
                        br#"{"mcpServers":{"mj-agents": {}}}"#,
                    )
                    .unwrap();
                }
                HarnessKind::Antigravity => {
                    mj_core::config::atomic_write(
                        &home.join("config/plugins/mjolnir-mailbox/hooks.json"),
                        b"{}",
                    )
                    .unwrap();
                }
                _ => {}
            }
            let profile = stage(harness, &mut environment, &mut args)
                .unwrap()
                .unwrap();
            let reviewer = harness.home_from_environment(&environment[harness.home_env()]);
            assert!(reviewer.starts_with(profile.path()));
            assert_eq!(
                std::fs::read(reviewer.join(harness.credential_file_name())).unwrap(),
                b"synthetic login"
            );
            assert!(!reviewer.join("mcp.json").exists());
            assert!(!reviewer.join("config/plugins").exists());
            match harness {
                HarnessKind::Codex => {
                    let config: toml::Table = toml::from_str(
                        &std::fs::read_to_string(reviewer.join("config.toml")).unwrap(),
                    )
                    .unwrap();
                    assert!(!config.contains_key("hooks") && !config.contains_key("mcp_servers"));
                    assert_eq!(config["default_permissions"].as_str(), Some(":read-only"));
                }
                HarnessKind::Claude => {
                    let settings: serde_json::Value = serde_json::from_slice(
                        &std::fs::read(reviewer.join("settings.json")).unwrap(),
                    )
                    .unwrap();
                    let config: serde_json::Value = serde_json::from_slice(
                        &std::fs::read(reviewer.join(".claude.json")).unwrap(),
                    )
                    .unwrap();
                    assert!(settings.get("hooks").is_none());
                    assert_eq!(settings["env"]["CLAUDE_MODEL"], "custom");
                    assert!(config.get("mcpServers").is_none());
                }
                HarnessKind::Antigravity => assert_eq!(args, harness.home_cli_args(&reviewer)),
                _ => {}
            }
            assert!(home.join("mcp.json").exists());
        }
    }
}
