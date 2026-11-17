//! The GitHub CLI approval every harness reads from its own configuration.
//!
//! An agent's issue, pull-request and review work is ordinary session work, so
//! Mjolnir trusts `gh` in the configuration file the harness already reads
//! rather than interrupting the person once per invocation.
//! [`mj_core::config::HarnessKind::gh_approval`] names where each harness keeps
//! that rule; this module writes it on every worker start so a profile staged
//! by an older release (or a changed execution policy) converges too.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use mj_core::config::{ExecutionPolicy, GhApproval, HarnessKind};

/// Kimi's and Grok's command patterns.
const GH_PATTERNS: [&str; 2] = ["Bash(gh *)", "Bash(gh)"];

/// Codex's Starlark execpolicy rule: any command whose first word is `gh`.
const CODEX_GH_RULE: &str = "prefix_rule(pattern=[\"gh\"], decision=\"allow\")\n";

/// The Muse adapter's variable that lists command prefixes it approves.
const MUSE_PREFIXES: &str = "MUSE_ACP_ALLOW_COMMAND_PREFIXES";

/// Install this session's native permissions: where the selected harness
/// accepts Mjolnir's `gh` approval, and the tools OpenCode must not offer.
///
/// `root` and `session_id` identify the session's private staged profile: a
/// home that is not one may be the person's own, so it is never edited.
pub(super) fn configure_native_rules(
    root: &Path,
    session_id: &str,
    home: &Path,
    harness: HarnessKind,
    policy: ExecutionPolicy,
    denials: super::json_gh_rules::OpenCodeDenials,
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    match harness.gh_approval() {
        // OpenCode reads its documents in order and takes the last matching
        // rule, so its approval and denials are asserted in the document
        // Mjolnir supplies last. Nothing outside the launch environment is
        // read or written for it, so no staged home is involved.
        GhApproval::SessionConfigDocument(_) => {
            super::json_gh_rules::configure_opencode_rules(policy, denials, environment)
        }
        GhApproval::AdapterEnvironment(_) => {
            // Muse keeps command prefixes in its adapter, not its settings.
            if policy == ExecutionPolicy::ConfiguredApprovals {
                configure_muse_prefixes(environment)?;
            }
            Ok(())
        }
        // A session's own request options, and the destinations another part
        // of the launch owns, are written where their contents are decided:
        // `session_request_meta` for Claude, `stage_antigravity_grants` for
        // agy's project grants, and the ACP permission callback for DSH.
        GhApproval::SessionOptions | GhApproval::LaunchFile(_) | GhApproval::AcpRequest => Ok(()),
        GhApproval::StagedFile(relative) => {
            // Codex reads the rule under every policy, so changing the
            // execution policy never leaves an older profile behind.
            if harness != HarnessKind::Codex && policy != ExecutionPolicy::ConfiguredApprovals {
                return Ok(());
            }
            if !super::subagents::private_staged_home(root, session_id, home) {
                return Ok(());
            }
            match harness {
                HarnessKind::Codex => configure_codex(home, relative),
                HarnessKind::Kimi | HarnessKind::Grok => configure_toml_gh_rules(home, harness),
                HarnessKind::Claude
                | HarnessKind::Muse
                | HarnessKind::OpenCode
                | HarnessKind::Dsh
                | HarnessKind::Antigravity => Ok(()),
            }
        }
    }
}

/// Write the managed Codex rule, creating its `rules/` directory when the
/// profile has none. Codex loads every `*.rules` file there and its own
/// `default.rules` stays untouched.
fn configure_codex(home: &Path, relative: &str) -> Result<()> {
    let rule = home.join(relative);
    let directory = rule
        .parent()
        .context("the Codex rule path must name a file")?;
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) => ensure!(
            metadata.is_dir(),
            "Codex rules directory {} must be a real directory",
            directory.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(directory)
                .with_context(|| format!("create Codex rules directory {}", directory.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect Codex rules {}", directory.display()));
        }
    }
    ensure!(
        super::subagents::regular_file_or_absent(&rule),
        "managed Codex rule {} must be a regular file",
        rule.display()
    );
    mj_core::config::atomic_write(&rule, CODEX_GH_RULE.as_bytes())
        .with_context(|| format!("write managed Codex rule {}", rule.display()))
}

/// Add narrow GitHub CLI approvals to the staged TOML configuration, keeping
/// every rule the person configured.
fn configure_toml_gh_rules(home: &Path, harness: HarnessKind) -> Result<()> {
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

/// Add `gh` to the prefixes muse-acp approves before sending a permission
/// request. Muse keeps command rules in its adapter, not its settings file.
fn configure_muse_prefixes(environment: &mut BTreeMap<String, String>) -> Result<()> {
    let mut prefixes: Vec<Vec<String>> = environment
        .get(MUSE_PREFIXES)
        .map(|value| serde_json::from_str(value))
        .transpose()
        .context("parse Muse native command prefixes")?
        .unwrap_or_default();
    ensure!(
        prefixes
            .iter()
            .all(|prefix| !prefix.is_empty() && prefix.iter().all(|word| !word.is_empty())),
        "Muse native command prefixes must contain nonempty argv words"
    );
    if !prefixes.iter().any(|prefix| prefix.as_slice() == ["gh"]) {
        prefixes.push(vec!["gh".into()]);
    }
    environment.insert(MUSE_PREFIXES.into(), serde_json::to_string(&prefixes)?);
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

    fn configure(
        root: &Path,
        session_id: &str,
        home: &Path,
        harness: HarnessKind,
        policy: ExecutionPolicy,
        environment: &mut BTreeMap<String, String>,
    ) -> Result<()> {
        configure_native_rules(
            root,
            session_id,
            home,
            harness,
            policy,
            super::super::json_gh_rules::OpenCodeDenials {
                native_subagent: false,
            },
            environment,
        )
    }

    #[test]
    fn codex_profile_prep_installs_the_rule_under_every_policy() {
        for policy in [
            ExecutionPolicy::ConfiguredApprovals,
            ExecutionPolicy::Unconstrained,
        ] {
            let (_temp, root, home) = staged_profile();
            let rules = home.join("rules");
            std::fs::create_dir(&rules).unwrap();
            let user = "prefix_rule(pattern=[\"git\"], decision=\"prompt\")\n";
            std::fs::write(rules.join("user.rules"), user).unwrap();
            let mut environment = BTreeMap::new();
            configure(
                &root,
                SESSION,
                &home,
                HarnessKind::Codex,
                policy,
                &mut environment,
            )
            .unwrap();
            let path = home.join("rules/mj-gh.rules");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), CODEX_GH_RULE);
            assert_eq!(
                std::fs::read_to_string(rules.join("user.rules")).unwrap(),
                user
            );
            std::fs::write(&path, "obsolete managed rule").unwrap();
            configure(
                &root,
                SESSION,
                &home,
                HarnessKind::Codex,
                policy,
                &mut environment,
            )
            .unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), CODEX_GH_RULE);
        }
    }

    /// Claude's approval rides the session-open request, so its staged
    /// settings — whether Mjolnir staged them or they are the person's own —
    /// are never edited for it.
    #[test]
    fn claude_profile_settings_are_never_edited_for_the_gh_approval() {
        let (_temp, root, home) = staged_profile();
        let path = home.join("settings.json");
        let original =
            r#"{"model":"opus","permissions":{"allow":["Bash(ls:*)"],"deny":["WebFetch"]}}"#;
        std::fs::write(&path, original).unwrap();
        let mut environment = BTreeMap::new();
        configure(
            &root,
            SESSION,
            &home,
            HarnessKind::Claude,
            ExecutionPolicy::ConfiguredApprovals,
            &mut environment,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(environment.is_empty());
        assert_eq!(
            HarnessKind::Claude.gh_approval(),
            GhApproval::SessionOptions
        );
    }

    #[test]
    fn non_guardian_launches_leave_the_json_configuration_alone() {
        let (_temp, root, home) = staged_profile();
        let settings = home.join("settings.json");
        std::fs::write(&settings, b"{}").unwrap();
        let mut environment = BTreeMap::new();
        configure(
            &root,
            SESSION,
            &home,
            HarnessKind::Kimi,
            ExecutionPolicy::Unconstrained,
            &mut environment,
        )
        .unwrap();
        assert_eq!(std::fs::read(&settings).unwrap(), b"{}");
        assert!(!home.join("config.toml").exists());
        assert!(environment.is_empty());
    }

    /// A YOLO OpenCode launch keeps the denials it needs in the document
    /// Mjolnir supplies last, and carries no `gh` allow; a guardian launch
    /// carries both. Nothing is read from or written to a staged home.
    #[test]
    fn opencode_rules_reach_the_document_mjolnir_supplies_last() {
        let (_temp, root, home) = staged_profile();
        for (policy, approved) in [
            (ExecutionPolicy::Unconstrained, false),
            (ExecutionPolicy::ConfiguredApprovals, true),
        ] {
            let mut environment = BTreeMap::new();
            configure(
                &root,
                SESSION,
                &home,
                HarnessKind::OpenCode,
                policy,
                &mut environment,
            )
            .unwrap();
            let content = &environment["OPENCODE_CONFIG_CONTENT"];
            assert_eq!(
                content.contains("\"gh\""),
                approved,
                "{policy:?}: {content}"
            );
            assert!(content.contains("\"execute\""), "{policy:?}: {content}");
        }
        assert!(
            !home.join("opencode.json").exists(),
            "the staged configuration file is not rewritten"
        );
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

        let mut environment = BTreeMap::new();
        configure(
            &root,
            SESSION,
            &home,
            HarnessKind::Kimi,
            ExecutionPolicy::ConfiguredApprovals,
            &mut environment,
        )
        .unwrap();
        let once = std::fs::read(&path).unwrap();
        configure(
            &root,
            SESSION,
            &home,
            HarnessKind::Kimi,
            ExecutionPolicy::ConfiguredApprovals,
            &mut environment,
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

        let mut environment = BTreeMap::new();
        configure(
            &root,
            SESSION,
            &home,
            HarnessKind::Grok,
            ExecutionPolicy::ConfiguredApprovals,
            &mut environment,
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

    #[test]
    fn muse_profile_prep_adds_the_gh_prefix_once() {
        let (_temp, root, home) = staged_profile();
        // The table names where each harness accepts the approval; the writer
        // must use that same variable.
        assert_eq!(
            HarnessKind::Muse.gh_approval(),
            GhApproval::AdapterEnvironment(MUSE_PREFIXES)
        );
        let mut environment = BTreeMap::from([(MUSE_PREFIXES.into(), r#"[["gh","pr"]]"#.into())]);
        configure(
            &root,
            SESSION,
            &home,
            HarnessKind::Muse,
            ExecutionPolicy::ConfiguredApprovals,
            &mut environment,
        )
        .unwrap();
        let prefixes: Vec<Vec<String>> = serde_json::from_str(&environment[MUSE_PREFIXES]).unwrap();
        assert_eq!(prefixes, [vec!["gh", "pr"], vec!["gh"]]);
        configure(
            &root,
            SESSION,
            &home,
            HarnessKind::Muse,
            ExecutionPolicy::ConfiguredApprovals,
            &mut environment,
        )
        .unwrap();
        let again: Vec<Vec<String>> = serde_json::from_str(&environment[MUSE_PREFIXES]).unwrap();
        assert_eq!(again, prefixes);
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
        let mut environment = BTreeMap::new();
        configure(
            &root,
            SESSION,
            &root.join("profile"),
            HarnessKind::Kimi,
            ExecutionPolicy::ConfiguredApprovals,
            &mut environment,
        )
        .unwrap();
        assert_eq!(std::fs::read(&source_config).unwrap(), original);

        let home = root.join("profiles").join(SESSION);
        std::fs::create_dir_all(&home).unwrap();
        let outside = temp.path().join("outside.toml");
        std::fs::write(&outside, original).unwrap();
        symlink(&outside, home.join("config.toml")).unwrap();
        assert!(
            configure(
                &root,
                SESSION,
                &home,
                HarnessKind::Kimi,
                ExecutionPolicy::ConfiguredApprovals,
                &mut environment,
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), original);

        // Claude's approval never touches the settings file, so even a linked
        // one — including the person's own — is left exactly as it was.
        let outside_settings = temp.path().join("outside-settings.json");
        std::fs::write(&outside_settings, b"{}").unwrap();
        symlink(&outside_settings, home.join("settings.json")).unwrap();
        configure(
            &root,
            SESSION,
            &home,
            HarnessKind::Claude,
            ExecutionPolicy::ConfiguredApprovals,
            &mut environment,
        )
        .unwrap();
        assert_eq!(std::fs::read(&outside_settings).unwrap(), b"{}");
    }
}
