//! The parent half of the stand-in leased relay: a relay target that runs this
//! test binary again as the relay (`mj_worker::stand_in_relay`). mj-daemon's
//! tests use it through the `test-support` feature.

#[cfg(unix)]
use crate::session_manager::RelaySessionTarget;
#[cfg(unix)]
use crate::targets::CommandSpec;
pub use mj_core::test_hooks::leased_relay::*;

/// A relay target served by this test binary over stdio.
#[cfg(unix)]
pub fn leased_relay_target(relay_root: &std::path::Path) -> RelaySessionTarget {
    // `RelayClient` parses every stdout line as JSON, so libtest's own
    // progress lines are dropped before they reach the protocol reader.
    let script = format!(
        "\"$0\" --exact {} --nocapture | grep --line-buffered '^{{'",
        CHILD_TEST
    );
    let mut spec = CommandSpec::new(
        "sh",
        [
            "-c".to_owned(),
            script,
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ],
    )
    .purpose("test leased relay");
    spec.env.insert(
        LEASED_RELAY_ROOT.to_owned(),
        relay_root.to_string_lossy().into_owned(),
    );
    if let Some(value) = std::env::var_os(DROP_ADMISSION_OPEN_REPLY) {
        spec.env.insert(
            DROP_ADMISSION_OPEN_REPLY.into(),
            value.to_string_lossy().into_owned(),
        );
    }
    RelaySessionTarget {
        session_id: LEASED_RELAY_SESSION.to_owned(),
        spec,
        worker_recovery: None,
        project_memory: None,
    }
}

/// Register the session the projection writes to. `apply_projection_event`
/// rejects events for sessions the controller database does not know.
#[cfg(unix)]
pub fn register_leased_relay_session() {
    crate::database::save_session(&mj_core::state::SessionRecord {
        oracle_choice: None,
        oracle: true,
        notifications_ignored: false,
        project: None,
        target_runtime: None,
        machine_id: None,
        launch_base: None,
        launch_branch: None,
        checkout: None,
        publication: None,
        build_cache: None,
        container_workspace: None,
        subagents: None,
        create_managed_worktree: None,
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        id: LEASED_RELAY_SESSION.into(),
        title: "leased relay".into(),
        harness_kind: mj_core::config::HarnessKind::Codex,
        last_profile: "codex".into(),
        staged_aws_profile: None,
        bundle_id: "project".into(),
        project_directory: None,
        managed_worktree: None,
        review: None,
        no_project_memory: false,
        no_github_events: false,
        no_github_credentials: false,
        target_template_id: "podman".into(),
        resource_allocation: None,
        additional_mounts: Vec::new(),
        state: mj_core::state::SessionState::Running,
        target: None,
        native_session_id: None,
        acp_session_title: None,
        session_title_override: None,
        created_at: "2026-08-12T00:00:00Z".into(),
        updated_at: "2026-08-12T00:00:00Z".into(),
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        dismissed_failure: None,
        failure_generation: 0,
        last_checkpoint_error: None,
        checkpoint: None,
    })
    .expect("register the test session");
}
