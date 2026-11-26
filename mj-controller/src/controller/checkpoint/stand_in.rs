//! The parent half of the stand-in latch relay: a relay target that runs this
//! test binary again as the relay (`mj_worker::stand_in_relay`), and the
//! controller fixtures built on it. mj-daemon's tests use these through the
//! `test-support` feature.

#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use mj_core::config::{Config, HarnessProfile, ProjectBundle, ProjectRepository, TargetTemplate};
#[cfg(unix)]
use mj_core::state::{SessionState, TargetLocator};

#[cfg(unix)]
use crate::controller::Controller;
#[cfg(unix)]
use crate::controller::test_support::checkpoint_test_session;
#[cfg(unix)]
use crate::session_manager::ManagedSessionHandle;
#[cfg(unix)]
use crate::targets::CommandSpec;
pub use mj_core::test_hooks::latch_relay::*;

/// Whether the scripted relay understands the early checkpoint release.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReleaseSupport {
    Supported,
    /// Answer a release exactly as a worker that predates the command does:
    /// its `RelayCommand` cannot deserialize the variant at all.
    Rejected,
}

/// Metadata reported at the cut by this stand-in's current resumed relay.
#[cfg(unix)]
pub fn current_latch_archive_metadata(input: &mut mj_checkpoint::archive::ArchiveInput) {
    let canonical = &mut input.canonical_session;
    let mut assessment = mj_core::assessment::Checkpoint::decode(
        canonical
            .assessment_state
            .as_ref()
            .expect("fixture assessment"),
    )
    .expect("valid fixture assessment");
    assessment.open_request = Some(mj_core::assessment::CheckpointOpenRequest {
        request: None,
        person_input_through: 0,
    });
    canonical.assessment_state = Some(serde_json::to_value(assessment).unwrap());
    canonical.notification_stack = Some(Default::default());
    canonical.native_use_evidence = Some(mj_core::relay::NativeSessionUseEvidence {
        version: 1,
        native_session_id: "native-session".into(),
        opened_ordinal: Some(1),
        usage: mj_core::relay::NativeSessionUse::Used,
    });
}

/// The shell command a stand-in `hel` runs for `worker proxy`: one more
/// connection to the stand-in relay over `relay_root`. A checkpoint opens such
/// a connection for its project-memory transaction from the worker root, not
/// from the session manager's target, so a fixture whose session has project
/// memory needs its `hel` to answer it.
#[cfg(unix)]
pub fn latch_relay_proxy_command(relay_root: &Path) -> String {
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    format!(
        "{LATCH_RELAY_ROOT}={root} {binary} --exact {child} --nocapture | \
         grep --line-buffered '^{{'",
        root = quote(relay_root),
        binary = quote(&std::env::current_exe().unwrap()),
        child = CHILD_TEST,
    )
}

/// A relay target served by this test binary over stdio. Each start of the
/// server appends to `starts`, if given.
#[cfg(unix)]
pub fn latch_relay_target(
    relay_root: &Path,
    starts: Option<&Path>,
    release: ReleaseSupport,
    running: bool,
) -> crate::session_manager::RelaySessionTarget {
    // `RelayClient` parses every stdout line as JSON, so libtest's own
    // progress lines are dropped before they reach the protocol reader.
    let script = format!("\"$0\" --exact {CHILD_TEST} --nocapture | grep --line-buffered '^{{'");
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
    .purpose("test latch relay");
    spec.env.insert(
        LATCH_RELAY_ROOT.to_owned(),
        relay_root.to_string_lossy().into_owned(),
    );
    if let Some(starts) = starts {
        spec.env.insert(
            LATCH_RELAY_STARTS.to_owned(),
            starts.to_string_lossy().into_owned(),
        );
    }
    if std::env::var_os(LATCH_CHECKPOINT_ONLY).is_some() {
        spec.env.insert(LATCH_CHECKPOINT_ONLY.into(), "1".into());
    }
    for key in [
        "MJ_TEST_QUOTA_BARRIER_RACE",
        LATCH_RELAY_MAX_PROTOCOL,
        LATCH_REJECT_PROJECT_MEMORY,
        LATCH_RECORD_SUBAGENT_ADMISSION,
        LATCH_DROP_ADMISSION_CLOSE_REPLY,
    ] {
        if let Some(value) = std::env::var_os(key) {
            spec.env
                .insert(key.into(), value.to_string_lossy().into_owned());
        }
    }
    if release == ReleaseSupport::Rejected {
        spec.env
            .insert(LATCH_RELAY_REJECT_RELEASE.to_owned(), "1".to_owned());
    }
    if running {
        spec.env
            .insert(LATCH_RELAY_RUNNING.to_owned(), "1".to_owned());
    }
    crate::session_manager::RelaySessionTarget {
        session_id: LATCH_RELAY_SESSION.to_owned(),
        spec,
        worker_recovery: None,
        project_memory: None,
    }
}

/// The session actor absorbs a returned connection on its own task, so the
/// first command after a latch ends may still be refused.
#[cfg(unix)]
pub async fn wait_until_the_actor_serves_again(handle: &ManagedSessionHandle) {
    for attempt in 0.. {
        if handle.sync_now().await.is_ok() {
            return;
        }
        assert!(attempt < 200, "the actor never took its connection back");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// A live session behind the scripted relay, in `state`, whose installed
/// archive holds exactly what that relay journals at startup, so a close
/// reuses the archive and nothing but the scripted write moves the cut.
#[cfg(unix)]
pub async fn close_cut_controller(
    state: SessionState,
) -> (
    Controller,
    crate::session_manager::SessionManagerChannels,
    ManagedSessionHandle,
) {
    close_cut_controller_with(state, false).await
}

/// [`close_cut_controller`], with the stand-in worker holding a running turn
/// when `running`.
#[cfg(unix)]
pub async fn close_cut_controller_with(
    state: SessionState,
    running: bool,
) -> (
    Controller,
    crate::session_manager::SessionManagerChannels,
    ManagedSessionHandle,
) {
    close_cut_controller_for_child(state, running, None).await
}

#[cfg(unix)]
pub async fn close_cut_controller_for_child(
    state: SessionState,
    running: bool,
    relation: Option<&mj_core::subagent::SubagentRecord>,
) -> (
    Controller,
    crate::session_manager::SessionManagerChannels,
    ManagedSessionHandle,
) {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(120));
        eprintln!("the suspend never finished");
        std::process::exit(101);
    });
    let data_directory = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    let relay_root = data_directory.join("relay");
    let profile_home = data_directory.join("profile");
    let archive_directory = data_directory.join("archives");
    for directory in [&relay_root, &profile_home, &archive_directory] {
        std::fs::create_dir_all(directory).unwrap();
    }
    let mut input = crate::controller::test_support::checkpoint_archive_input(
        LATCH_RELAY_SESSION,
        2,
        Vec::new(),
        Vec::new(),
    );
    input.canonical_session.session.configuration.goal = Some(Box::new(
        serde_json::from_value(serde_json::json!({
            "known": true,
            "execution": {"version": 1, "status": "idle"}
        }))
        .unwrap(),
    ));
    if std::env::var(LATCH_RELAY_MAX_PROTOCOL)
        .ok()
        .and_then(|version| version.parse::<u32>().ok())
        .is_some_and(|version| version >= mj_core::relay::RELAY_CHECKPOINT_PROMPT_CONTEXT_PROTOCOL)
    {
        current_latch_archive_metadata(&mut input);
    }
    let checkpoint = crate::controller::test_support::write_checkpoint_archive_input(
        &archive_directory,
        LATCH_RELAY_SESSION,
        &input,
    );

    let mut session = checkpoint_test_session(LATCH_RELAY_SESSION);
    session.state = state;
    session.target_template_id = "removed-local".into();
    session.target_runtime = Some((&TargetTemplate::LocalBare).into());
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: data_directory.join("workers").join(LATCH_RELAY_SESSION),
    }));
    session.checkpoint = Some(Box::new(checkpoint));
    if let Some(relation) = relation {
        crate::database::save_subagent_session(&session, relation).unwrap();
    } else {
        crate::database::save_session(&session).unwrap();
    }

    let mut config = Config::default();
    config.profiles.insert(
        "codex".into(),
        HarnessProfile {
            ..mj_core::config::HarnessProfile::new(
                mj_core::config::HarnessKind::Codex,
                profile_home,
            )
        },
    );
    config.bundles.insert(
        "project".into(),
        ProjectBundle {
            primary_repo: "project".into(),
            repositories: vec![ProjectRepository {
                id: "project".into(),
                github: Some("example/project".into()),
                local: None,
                destination: "project".into(),
                git_ref: None,
                mbx_namespace: None,
            }],
        },
    );
    let controller = Controller {
        config,
        state: crate::database::load_state().unwrap(),
    };

    let channels = crate::session_manager::spawn_session_manager().unwrap();
    channels
        .targets
        .send(vec![latch_relay_target(
            &relay_root,
            None,
            ReleaseSupport::Supported,
            running,
        )])
        .unwrap();
    let handle = channels
        .control
        .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();
    (controller, channels, handle)
}
