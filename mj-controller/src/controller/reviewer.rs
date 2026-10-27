//! Launch settings and private MCP configuration for reviewer child sessions.

use std::path::Path;

use anyhow::{Context, Result, bail};

use super::Controller;
use super::worker_binary::bridge_launch;
use mj_core::worker_launch::{ReviewMcpServer, ReviewerLaunchConfig};

impl Controller {
    pub(crate) fn reviewer_launch_config(
        &self,
        session_id: &str,
        profile_id: &str,
        generation: u64,
        mcp_servers: &[ReviewMcpServer],
    ) -> Result<ReviewerLaunchConfig> {
        let profile = self
            .config
            .enabled_profile(profile_id)
            .context("reviewer profile is unavailable")?;
        profile.ensure_ready(profile_id)?;
        let session = self
            .state
            .sessions
            .get(session_id)
            .context("review parent is missing")?;
        let execution_policy = session
            .target_runtime_settings(&self.config)?
            .execution_policy;
        let (backend, _) = self.worker_placement(session_id)?;
        let (bridge_command, bridge_args) = bridge_launch(profile.kind, execution_policy);
        let mut environment = profile.environment.resolved().clone();
        // The worker sets the harness home from the directory it staged, so
        // sending one here could only point the reviewer somewhere it must not
        // read.
        environment.remove(profile.home_env());
        // A reviewer on a ChatGPT Codex profile must not fall back to an API
        // key any more than a session may.
        let excluded_environment = profile.exclude_harness_environment(&mut environment);
        // A Podman session runs as uid 0, where Claude Code rejects
        // bypassPermissions unless told it is already sandboxed. The session's
        // own launch sets this only when the session itself runs Claude.
        if backend.container_engine() == Some("podman")
            && profile.kind == mj_core::config::HarnessKind::Claude
        {
            environment.insert("IS_SANDBOX".into(), "1".into());
        }
        Ok(ReviewerLaunchConfig {
            profile_id: profile_id.to_owned(),
            harness: profile.kind,
            bridge_command: bridge_command.into(),
            bridge_args,
            environment,
            excluded_environment,
            execution_policy,
            model: None,
            effort: None,
            fast_mode: None,
            generation,
            // Keep ownership metadata for approval policy even when connection
            // configuration lives in the staged profile. The worker owns ACP delivery.
            mcp_servers: mcp_servers.to_vec(),
        })
    }
}

/// Writes `servers` into the staged profile of a harness that reads its MCP
/// configuration from disk.
///
/// Claude Code reads `mcpServers` from `.claude.json` in its config directory;
/// Kimi reads `mcpServers` from `mcp.json` in its home, and needs the runtime
/// id its own schema carries. Both files are the reviewer's private copy, so
/// nothing here can reach the user's own configuration.
pub(super) fn configure_staged_review_mcp(
    harness: mj_core::config::HarnessKind,
    profile_stage: &Path,
    servers: &[ReviewMcpServer],
) -> Result<()> {
    let Some(file) = harness.mcp_config_file() else {
        bail!("{harness:?} does not read MCP servers from its profile");
    };
    let kimi = harness == mj_core::config::HarnessKind::Kimi;
    let path = profile_stage.join(file);
    let mut document = match std::fs::read(&path) {
        Ok(body) => serde_json::from_slice::<serde_json::Value>(&body)
            .with_context(|| format!("parse staged reviewer configuration {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(serde_json::Map::new())
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read staged reviewer configuration {}", path.display()));
        }
    };
    let root = document.as_object_mut().with_context(|| {
        format!(
            "staged reviewer configuration {} must contain a JSON object",
            path.display()
        )
    })?;
    let configured = root
        .entry("mcpServers")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .with_context(|| {
            format!(
                "mcpServers in staged reviewer configuration {} must be a JSON object",
                path.display()
            )
        })?;
    for server in servers {
        let mut entry = serde_json::json!({
            "command": server.command,
            "args": server.args,
        });
        if kimi {
            let object = entry
                .as_object_mut()
                .expect("the server entry is a JSON object");
            object.insert("transport".into(), "stdio".into());
            object.insert("runtime_id".into(), "local".into());
        } else {
            let object = entry
                .as_object_mut()
                .expect("the server entry is a JSON object");
            object.insert("type".into(), "stdio".into());
        }
        configured.insert(server.name.clone(), entry);
    }
    let mut body = serde_json::to_vec_pretty(&document)?;
    body.push(b'\n');
    mj_core::config::atomic_write(&path, &body)
        .with_context(|| format!("write staged reviewer configuration {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_support::checkpoint_test_session;
    use mj_core::config::{Config, HarnessKind, HarnessProfile, TargetTemplate};
    use mj_core::state::{SessionState, State};
    use std::collections::BTreeMap;
    const SESSION_ID: &str = "0123456789abcdef0123456789abcdef";
    fn fixture(directory: &Path, locator: mj_core::state::TargetLocator) -> (Controller, String) {
        let session_id = SESSION_ID;
        let mut session = checkpoint_test_session(session_id);
        session.target_template_id = "local".into();
        session.state = SessionState::Running;
        let template = match &locator {
            mj_core::state::TargetLocator::LocalPodman { .. } => {
                serde_json::from_str(r#"{"kind":"local-podman","image":"test"}"#).unwrap()
            }
            mj_core::state::TargetLocator::LocalDocker { .. } => {
                serde_json::from_str(r#"{"kind":"local-docker","image":"test"}"#).unwrap()
            }
            mj_core::state::TargetLocator::SshPodman { .. } => serde_json::from_str(
                r#"{"kind":"ssh-podman","host":"builder.test","image":"test"}"#,
            )
            .unwrap(),
            _ => TargetTemplate::LocalBare,
        };
        session.target = Some(locator);
        let mut config = Config::default();
        config.targets.insert("local".into(), template);
        for (id, kind) in [
            ("codex", HarnessKind::Codex),
            ("claude", HarnessKind::Claude),
        ] {
            let home = directory.join(id);
            std::fs::create_dir_all(&home).unwrap();
            config.profiles.insert(
                id.to_owned(),
                HarnessProfile {
                    enabled: true,
                    kind,
                    home,
                    environment: BTreeMap::from([("EXTRA".into(), "1".into())]).into(),
                    context_window_bytes: None,
                    legacy_subagents: Default::default(),
                    guardian_review_model: None,
                    codex_profile: None,
                    muse_auto_review: None,
                },
            );
        }
        (
            Controller {
                config,
                state: State {
                    sessions: [(session_id.into(), session)].into_iter().collect(),
                    ..State::default()
                },
            },
            session_id.to_owned(),
        )
    }

    #[test]
    fn a_claude_reviewer_in_a_podman_session_is_told_it_is_sandboxed() {
        let directory = tempfile::tempdir().unwrap();
        let container_id = crate::targets::resource_name(SESSION_ID).unwrap();
        let podman = mj_core::state::TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: container_id.clone(),
            workspace_storage: Default::default(),
        };
        let docker = mj_core::state::TargetLocator::LocalDocker {
            borrowed_from: None,
            container_id,
        };
        for (locator, profile_id, sandboxed) in [
            (podman.clone(), "claude", true),
            (podman, "codex", false),
            (docker, "claude", false),
        ] {
            let (controller, session_id) = fixture(directory.path(), locator);
            let config = controller
                .reviewer_launch_config(&session_id, profile_id, 0, &[])
                .unwrap();
            assert_eq!(
                config.environment.get("IS_SANDBOX").map(String::as_str),
                sandboxed.then_some("1"),
                "{profile_id}"
            );
        }
    }
}
