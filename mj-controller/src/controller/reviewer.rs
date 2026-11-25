//! Launch settings and private MCP configuration for reviewer child sessions.

use anyhow::{Context, Result};

use super::Controller;
use super::worker_binary::bridge_launch;
use mj_core::worker_launch::{ReviewMcpServer, ReviewerLaunchConfig};

impl Controller {
    pub fn reviewer_launch_config(
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
        let (bridge_command, bridge_args) = bridge_launch(profile.kind, execution_policy);
        // The reviewer's launch is built from its profile like any session's,
        // so its environment values are not recorded here: the record outlives
        // a rotated secret and must not hold one. Only the names to remove are
        // recorded.
        let mut environment = profile.environment.resolved().clone();
        environment.remove(profile.home_env());
        let excluded_environment = profile.exclude_harness_environment(&mut environment);
        Ok(ReviewerLaunchConfig {
            role: mj_core::worker_launch::ReviewChildRole::Reviewer,
            profile_id: profile_id.to_owned(),
            harness: profile.kind,
            bridge_command: bridge_command.into(),
            bridge_args,
            environment: Default::default(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_support::checkpoint_test_session;
    use mj_core::config::{Config, HarnessKind, HarnessProfile, TargetTemplate};
    use mj_core::state::{SessionState, State};
    use std::collections::BTreeMap;
    use std::path::Path;
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
        session.target = Some(Box::new(locator));
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
                    environment: BTreeMap::from([("EXTRA".into(), "1".into())]).into(),
                    context_window_bytes: None,
                    models: Default::default(),
                    legacy_subagents: Default::default(),
                    guardian_review_model: None,
                    codex_profile: None,
                    legacy_muse_auto_review: None,
                    load_balancing_may_consume_credits: false,
                    claude_permission_mode: None,
                    mjolnir_guardian: true,
                    aws: None,
                    ..mj_core::config::HarnessProfile::new(kind, home)
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

    // The reviewer's profile environment is applied from the live profile at
    // launch; a recorded copy would persist resolved secrets.
    #[test]
    fn a_reviewer_launch_record_holds_no_environment_values() {
        let directory = tempfile::tempdir().unwrap();
        let podman = mj_core::state::TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: crate::targets::resource_name(SESSION_ID).unwrap(),
            workspace_storage: Default::default(),
        };
        let (controller, session_id) = fixture(directory.path(), podman);
        let config = controller
            .reviewer_launch_config(&session_id, "claude", 0, &[])
            .unwrap();
        assert!(config.environment.is_empty());
        assert!(
            serde_json::to_value(&config)
                .unwrap()
                .get("environment")
                .is_none()
        );
    }
}
