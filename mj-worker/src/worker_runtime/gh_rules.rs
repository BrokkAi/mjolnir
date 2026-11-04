//! Native `gh` trust rules for staged Kimi and Grok profiles.

use std::path::Path;

use anyhow::{Context, Result, bail};
use mj_core::config::{ExecutionPolicy, HarnessKind};

const GH_PATTERNS: [&str; 2] = ["Bash(gh *)", "Bash(gh)"];

/// Add narrow GitHub CLI approvals to a private profile when approvals apply.
pub(super) fn configure_toml_gh_rules(
    root: &Path,
    session_id: &str,
    home: &Path,
    harness: HarnessKind,
    policy: ExecutionPolicy,
) -> Result<()> {
    if policy != ExecutionPolicy::ConfiguredApprovals
        || !matches!(harness, HarnessKind::Kimi | HarnessKind::Grok)
        || !super::subagents::private_staged_home(root, session_id, home)
    {
        return Ok(());
    }

    let path = home.join("config.toml");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            bail!(
                "harness configuration {} must be a regular file",
                path.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }

    let mut config = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<toml::Table>(&text)
            .with_context(|| format!("parse staged harness configuration {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let changed = match harness {
        HarnessKind::Kimi => add_kimi_rules(&mut config, &path)?,
        HarnessKind::Grok => add_grok_rules(&mut config, &path)?,
        _ => false,
    };
    if changed {
        mj_core::config::atomic_write(&path, toml::to_string(&config)?.as_bytes())
            .with_context(|| format!("write staged harness configuration {}", path.display()))?;
    }
    Ok(())
}

fn permission_table<'a>(config: &'a mut toml::Table, path: &Path) -> Result<&'a mut toml::Table> {
    config
        .entry("permission")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .with_context(|| format!("permission in {} must be a table", path.display()))
}

fn add_kimi_rules(config: &mut toml::Table, path: &Path) -> Result<bool> {
    let permission = permission_table(config, path)?;
    let rules = permission
        .entry("rules")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .with_context(|| format!("permission.rules in {} must be an array", path.display()))?;
    let mut changed = false;
    for pattern in GH_PATTERNS {
        let exists = rules.iter().any(|rule| {
            rule.as_table().is_some_and(|table| {
                table.get("decision").and_then(toml::Value::as_str) == Some("allow")
                    && table.get("pattern").and_then(toml::Value::as_str) == Some(pattern)
            })
        });
        if !exists {
            rules.push(toml::Value::Table(toml::Table::from_iter([
                ("decision".into(), toml::Value::String("allow".into())),
                ("pattern".into(), toml::Value::String(pattern.into())),
            ])));
            changed = true;
        }
    }
    Ok(changed)
}

fn add_grok_rules(config: &mut toml::Table, path: &Path) -> Result<bool> {
    let permission = permission_table(config, path)?;
    let allow = permission
        .entry("allow")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .with_context(|| format!("permission.allow in {} must be an array", path.display()))?;
    let mut changed = false;
    for pattern in GH_PATTERNS {
        let value = toml::Value::String(pattern.into());
        if !allow.contains(&value) {
            allow.push(value);
            changed = true;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "gh-policy-session";

    fn staged_profile() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("worker");
        let home = root.join("profile");
        std::fs::create_dir_all(&home).unwrap();
        (temp, root, home)
    }

    #[test]
    fn kimi_profile_prep_preserves_configured_rules_and_is_idempotent() {
        let (_temp, root, home) = staged_profile();
        let path = home.join("config.toml");
        std::fs::write(
            &path,
            r#"
                [agent]
                name = "reviewer"
                [permission]
                allow = ["Read"]
                deny = ["Bash(rm *)"]
                ask = ["Bash(git push *)"]
                rules = [{ decision = "deny", pattern = "Bash(gh *)" }]
            "#,
        )
        .unwrap();

        configure_toml_gh_rules(
            &root,
            SESSION,
            &home,
            HarnessKind::Kimi,
            ExecutionPolicy::ConfiguredApprovals,
        )
        .unwrap();
        let once = std::fs::read(&path).unwrap();
        configure_toml_gh_rules(
            &root,
            SESSION,
            &home,
            HarnessKind::Kimi,
            ExecutionPolicy::ConfiguredApprovals,
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), once);

        let config: toml::Table = toml::from_str(std::str::from_utf8(&once).unwrap()).unwrap();
        let permission = config["permission"].as_table().unwrap();
        for (key, value) in [("deny", "Bash(rm *)"), ("ask", "Bash(git push *)")] {
            assert_eq!(permission[key].as_array().unwrap()[0].as_str(), Some(value));
        }
        assert_eq!(config["agent"]["name"].as_str(), Some("reviewer"));
        assert!(permission["rules"].as_array().unwrap().iter().any(|rule| {
            rule["decision"].as_str() == Some("allow")
                && rule["pattern"].as_str() == Some("Bash(gh)")
        }));
    }

    #[test]
    fn grok_profile_prep_preserves_deny_and_ask_rules() {
        let (_temp, root, home) = staged_profile();
        let path = home.join("config.toml");
        std::fs::write(
            &path,
            r#"[permission]
allow = ["Read"]
deny = ["Bash(rm *)"]
ask = ["Bash(git push *)"]
rules = [{ decision = "deny", pattern = "Bash(gh)" }]
"#,
        )
        .unwrap();

        configure_toml_gh_rules(
            &root,
            SESSION,
            &home,
            HarnessKind::Grok,
            ExecutionPolicy::ConfiguredApprovals,
        )
        .unwrap();
        let config: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let permission = config["permission"].as_table().unwrap();
        assert_eq!(
            permission["allow"].as_array().unwrap()[0].as_str(),
            Some("Read")
        );
        assert_eq!(
            permission["deny"].as_array().unwrap()[0].as_str(),
            Some("Bash(rm *)")
        );
        assert_eq!(
            permission["ask"].as_array().unwrap()[0].as_str(),
            Some("Bash(git push *)")
        );
    }

    #[cfg(unix)]
    #[test]
    fn profile_and_config_symlinks_are_never_modified() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("worker");
        std::fs::create_dir_all(&root).unwrap();
        let source = temp.path().join("source-profile");
        std::fs::create_dir(&source).unwrap();
        let source_config = source.join("config.toml");
        let original = b"[permission]\ndeny = [\"Bash(rm *)\"]\n";
        std::fs::write(&source_config, original).unwrap();
        symlink(&source, root.join("profile")).unwrap();
        configure_toml_gh_rules(
            &root,
            SESSION,
            &root.join("profile"),
            HarnessKind::Kimi,
            ExecutionPolicy::ConfiguredApprovals,
        )
        .unwrap();
        assert_eq!(std::fs::read(&source_config).unwrap(), original);

        let home = root.join("profiles").join(SESSION);
        std::fs::create_dir_all(&home).unwrap();
        let outside = temp.path().join("outside.toml");
        std::fs::write(&outside, original).unwrap();
        symlink(&outside, home.join("config.toml")).unwrap();
        assert!(
            configure_toml_gh_rules(
                &root,
                SESSION,
                &home,
                HarnessKind::Kimi,
                ExecutionPolicy::ConfiguredApprovals,
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), original);
    }
}
