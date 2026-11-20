//! Fixtures shared by the move tests here and in mj-daemon, which reaches
//! them through the `test-support` feature.

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::path::{Path, PathBuf};

#[cfg(unix)]
use anyhow::Result;

#[cfg(unix)]
use super::Controller;
use crate::controller::test_support::IsolatedTest;
#[cfg(unix)]
use crate::controller::test_support::checkpoint_test_session;
#[cfg(unix)]
use mj_core::config::{Config, HarnessKind, HarnessProfile};
#[cfg(unix)]
use mj_core::state::CheckpointMetadata;
use mj_core::state::{MoveOperation, MovePhase, MoveSelection, ResumeQueueDisposition};
#[cfg(unix)]
use mj_core::test_hooks::install_fake_command;

#[cfg(unix)]
use crate::controller::checkpoint::stand_in::*;
#[cfg(unix)]
use crate::targets::{CommandExecutor, CommandOutput, CommandSpec, ProcessExecutor};
#[cfg(unix)]
use mj_core::state::{MaterializedSession, SessionState, State, TargetLocator};

pub const MOVE_QUEUE_SESSION_ID: &str = "0123456789abcdef0123456789abcdef";

pub fn isolated_test_child(test_name: &str, marker: &str) -> bool {
    if std::env::var_os(marker).is_some() {
        return true;
    }
    let directory = tempfile::tempdir().unwrap();
    IsolatedTest::new(test_name)
        .env(marker, "1")
        .isolated_store(directory.path())
        .env(
            "MJ_WORKER_BINARY",
            mj_core::worker_build::test_worker_executable(),
        )
        .run();
    false
}

#[cfg(unix)]
pub fn shell_literal(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// A fixture owns every pipeline launched by its reconnect command. Register
/// atomically before spawning the pipeline; shutdown closes registration first.
#[cfg(unix)]
#[must_use]
pub struct TestProxyGroupGuard {
    directory: PathBuf,
}

#[cfg(unix)]
impl TestProxyGroupGuard {
    pub fn new(root: &Path) -> Self {
        let directory = root.join("proxy-groups");
        fs::create_dir_all(&directory).unwrap();
        Self { directory }
    }

    pub fn registration_script(&self) -> String {
        format!(
            "set -e\nproxy_groups={}\nprintf '%s\\n' \"$$\" > \"$proxy_groups/$$.pending\"\nmv \"$proxy_groups/$$.pending\" \"$proxy_groups/$$\"\n",
            shell_literal(&self.directory)
        )
    }

    pub fn registered_group(&self) -> i32 {
        fs::read_dir(&self.directory)
            .unwrap()
            .filter_map(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse().ok())
            })
            .next()
            .expect("proxy registered its group before answering hello")
    }

    pub fn group_exists(group: i32) -> io::Result<bool> {
        // SAFETY: signal zero only queries the positive, fixture-owned group.
        if unsafe { libc::kill(-group, 0) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(false)
        } else {
            Err(error)
        }
    }
}

#[cfg(unix)]
impl Drop for TestProxyGroupGuard {
    fn drop(&mut self) {
        let closed = self.directory.with_extension("reaping");
        match fs::rename(&self.directory, &closed) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                tracing::error!(%error, "close test proxy registration");
                return;
            }
        }
        let groups = match fs::read_dir(&closed).and_then(|entries| {
            entries
                .map(|entry| {
                    let entry = entry?;
                    Ok(entry
                        .file_name()
                        .to_str()
                        .and_then(|name| name.parse::<i32>().ok())
                        .filter(|pid| *pid > 1))
                })
                .collect::<io::Result<Vec<_>>>()
        }) {
            Ok(groups) => groups.into_iter().flatten().collect::<Vec<_>>(),
            Err(error) => {
                tracing::error!(%error, "read test proxy groups");
                return;
            }
        };
        // Kill every owned group before waiting or removing fixture files.
        for &group in &groups {
            mj_core::subprocess::terminate_process_group(group, libc::SIGKILL);
        }
        for group in groups {
            loop {
                match Self::group_exists(group) {
                    Ok(false) => break,
                    Ok(true) => std::thread::sleep(std::time::Duration::from_millis(5)),
                    Err(error) => {
                        tracing::error!(group, %error, "wait for test proxy reaping");
                        break;
                    }
                }
            }
        }
        // RelayClient's existing reaper remains the sole Child/waitpid owner.
        if let Err(error) = fs::remove_dir_all(closed) {
            tracing::error!(%error, "remove stopped test proxy registrations");
        }
    }
}

pub fn source_recovery_operation(session: &mj_core::state::SessionRecord) -> MoveOperation {
    MoveOperation {
        quota_stop_reason: None,
        quota_continuation: None,
        prepared_destination: None,
        accepted_preparation: None,
        acknowledge_interruption: false,
        workspace_transfer: None,
        handoff: None,
        in_place: false,
        source_checkpoint_only: false,
        operation_id: "move-source-recovery".into(),
        selection: MoveSelection {
            oracle: None,
            oracle_choice: None,
            subagents: None,
            workspace: Default::default(),
            clear_resource_allocation: false,
            session_id: session.id.clone(),
            profile_id: Some("destination".into()),
            target_template_id: Some("local-bare".into()),
            additional_mounts: None,
            resource_allocation: None,
        },
        source_profile_id: session.last_profile.clone(),
        source_target_template_id: session.target_template_id.clone(),
        source_target: session.target.clone().map(|v| *v),
        source_native_session_id: session.native_session_id.clone(),
        inherited_resource_allocation: None,
        inherited_additional_mounts: Vec::new(),
        source_additional_mounts: Vec::new(),
        source_resource_allocation: None,
        destination_target: None,
        destination_native_session_id: None,
        destination_store_id: None,
        configuration_fingerprint: "source-recovery-test".into(),
        checkpoint: None,
        recovery_session: None,
        queue: ResumeQueueDisposition::Discard,
        phase: MovePhase::ClosingSource,
        queue_admission_started: false,
        queue_admission_finished: false,
        cancellation_requested: false,
        created_at: session.created_at.clone(),
        updated_at: session.updated_at.clone(),
        error: None,
    }
}

/// A `CommandExecutor` that really runs what it is given and remembers the
/// purpose of every command. An in-place move is judged by what it did *not*
/// run, so the commands have to be real and the record complete.
#[cfg(unix)]
pub struct RecordingProcessExecutor {
    /// The worker's checkpoint restore and export, run in this process.
    pub worker: WorkerCheckpoint,
    pub purposes: std::sync::Mutex<Vec<String>>,
    /// Every command line, so a test can prove which paths were never named.
    pub command_lines: std::sync::Mutex<Vec<String>>,
    pub notices: std::sync::Mutex<Vec<String>>,
    pub move_stop_calls: std::sync::atomic::AtomicUsize,
    /// Purpose substring whose command fails instead of running.
    pub fail_purpose: Option<&'static str>,
    pub cancelled: std::sync::atomic::AtomicBool,
    pub drain_wait: Option<std::sync::Arc<tokio::sync::Notify>>,
    pub handoff_gate: Option<std::sync::Arc<crate::upgrade::Gate>>,
    pub handoff_work: std::sync::Mutex<Option<crate::upgrade::Work>>,
    pub shutdown: std::sync::atomic::AtomicBool,
}

/// The worker's checkpoint operations, which mj-controller cannot name outside
/// its own tests: each test binary passes mj-worker's.
#[cfg(unix)]
pub struct WorkerCheckpoint {
    pub restore: fn(&Path) -> Result<()>,
    /// Export from the spec on the reader and return the checkpoint as JSON.
    pub export: fn(&mut (dyn io::Read + Send)) -> Result<Vec<u8>>,
}

#[cfg(unix)]
impl RecordingProcessExecutor {
    pub fn new(worker: WorkerCheckpoint) -> Self {
        Self {
            worker,
            purposes: Default::default(),
            command_lines: Default::default(),
            notices: Default::default(),
            move_stop_calls: Default::default(),
            fail_purpose: None,
            cancelled: Default::default(),
            drain_wait: None,
            handoff_gate: None,
            handoff_work: Default::default(),
            shutdown: Default::default(),
        }
    }

    pub fn purposes(&self) -> Vec<String> {
        self.purposes.lock().unwrap().clone()
    }

    pub fn notices(&self) -> Vec<String> {
        self.notices.lock().unwrap().clone()
    }

    pub fn command_lines(&self) -> Vec<String> {
        self.command_lines.lock().unwrap().clone()
    }

    pub fn move_stop_calls(&self) -> usize {
        self.move_stop_calls
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn record(&self, command: &CommandSpec) {
        self.purposes.lock().unwrap().push(command.purpose.clone());
        self.command_lines.lock().unwrap().push(format!(
            "{} {}",
            command.program,
            command.args.join(" ")
        ));
    }
}

#[cfg(unix)]
impl CommandExecutor for RecordingProcessExecutor {
    fn begin_resumable_work(&self) -> Result<()> {
        self.handoff_work.lock().unwrap().take();
        Ok(())
    }
    fn end_resumable_work(&self, label: &'static str) -> Result<()> {
        if let Some(gate) = &self.handoff_gate {
            *self.handoff_work.lock().unwrap() = Some(gate.enter_unless_draining(label)?);
        }
        Ok(())
    }
    fn shutdown_interrupted(&self) -> bool {
        self.shutdown.load(std::sync::atomic::Ordering::Acquire)
    }
    fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
        mj_core::targets::DeadlinePause::none()
    }

    fn cancellation_requested(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.cancellation_requested()
    }

    fn execute_without_deadline(
        &self,
        command: &mj_core::targets::CommandSpec,
    ) -> anyhow::Result<mj_core::targets::CommandOutput> {
        self.execute(command)
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &mj_core::targets::CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> anyhow::Result<mj_core::targets::CommandOutput> {
        self.execute_with_stdin(command, input)
    }

    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.record(command);
        if let Some(failing) = self.fail_purpose
            && command.purpose.contains(failing)
        {
            anyhow::bail!("{} failed in this test", command.purpose);
        }
        if command.purpose == "restore target checkpoint" {
            let spec = command.args.last().expect("restore spec path");
            (self.worker.restore)(Path::new(spec))?;
            return Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }
        ProcessExecutor.execute(command)
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn io::Read + Send),
    ) -> Result<CommandOutput> {
        self.record(command);
        if command.purpose == "export target checkpoint" {
            return Ok(CommandOutput {
                status: 0,
                stdout: (self.worker.export)(input)?,
                stderr: Vec::new(),
            });
        }
        ProcessExecutor.execute_with_stdin(command, input)
    }

    fn notify_notice(&self, notice: &str) {
        self.notices.lock().unwrap().push(notice.to_owned());
        if notice == "Waiting for subagent requests to finish"
            && let Some(wait) = &self.drain_wait
        {
            wait.notify_one();
        }
    }

    fn before_move_source_stop(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        self.move_stop_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

/// Install the stand-in worker binary for a LocalBare in-place move.
///
/// The controller installs whatever `MJ_WORKER_BINARY` names as
/// `<worker_root>/hel`, and the checked-in fake dispatcher reads its behaviour
/// from `<worker_root>/hel.script`. The in-place reset unlinks `hel` but not
/// `hel.script`, so one script serves the harness before and after the swap:
/// `worker proxy` becomes the relay server the controller reconnects to, and
/// `worker restore-checkpoint` leaves the relay seed a real restore would.
#[cfg(unix)]
pub fn install_in_place_worker_script(worker_root: &Path, frontier: u64, digest: &str) {
    let script = format!(
        r#"case "${{2:-}}" in
build-id)
    if [ -f {root}/worker-build ]; then cat {root}/worker-build; else printf '%s\n' '{worker_build}'; fi
    ;;
proxy)
    unset {checkpoint_only} MJ_TEST_LATCH_RELAY_MAX_PROTOCOL
    {relay_root}={root}
    export {relay_root}
    {binary} --exact {child} --nocapture | grep --line-buffered '^{{'
    ;;
restore-checkpoint)
    cat >{root}/relay-seed.json <<'SEED'
{{"event_frontier":{frontier},"event_frontier_digest":"{digest}","queued_prompts":[]}}
SEED
    ;;
esac
exit 0
"#,
        checkpoint_only = LATCH_CHECKPOINT_ONLY,
        worker_build = mj_core::worker_build::worker_build_id(),
        relay_root = LATCH_RELAY_ROOT,
        root = shell_literal(worker_root),
        binary = shell_literal(&std::env::current_exe().unwrap()),
        child = mj_core::test_hooks::latch_relay::CHILD_TEST,
    );
    install_fake_command(worker_root, "hel", &script);
}

/// The state an in-place move starts its destination half from: the source
/// sealed, its checkpoint installed, and its target still on the record. That
/// is exactly what `an_in_place_move_close_seals_the_source_and_keeps_its_target`
/// proves a retained close leaves behind.
#[cfg(unix)]
pub struct InPlaceFixture {
    pub _directory: tempfile::TempDir,
    pub worker_root: PathBuf,
    pub checkout: PathBuf,
    pub source_home: PathBuf,
    pub destination_home: PathBuf,
    pub checkpoint: CheckpointMetadata,
    pub controller: Controller,
}

#[cfg(unix)]
pub const IN_PLACE_SOURCE_PROFILE: &str = "claude-source";

#[cfg(unix)]
pub const IN_PLACE_DESTINATION_PROFILE: &str = "claude-destination";

#[cfg(unix)]
pub fn in_place_fixture(destination_kind: HarnessKind, source_kind: HarnessKind) -> InPlaceFixture {
    let directory = tempfile::tempdir().unwrap();
    // A LocalBare worker root has to end in the session id.
    let worker_root = directory.path().join(LATCH_RELAY_SESSION);
    let checkout = directory.path().join("checkout");
    let archives = directory.path().join("archives");
    let source_home = directory.path().join("source-profile");
    let destination_home = directory.path().join("destination-profile");
    for path in [
        &worker_root,
        &checkout,
        &archives,
        &source_home,
        &destination_home,
    ] {
        fs::create_dir_all(path).unwrap();
    }
    let git = crate::controller::test_support::test_git;
    git(&checkout, &["init", "-b", "main"]);
    git(&checkout, &["config", "user.email", "move@example.invalid"]);
    git(&checkout, &["config", "user.name", "Move test"]);
    git(&checkout, &["commit", "--allow-empty", "-m", "fixture"]);
    git(
        &checkout,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/project.git",
        ],
    );
    // `stage_profile` copies an allowlist per harness kind, so the file that
    // tells the two profiles apart has to be one of those names.
    for (home, owner) in [(&source_home, "source"), (&destination_home, "destination")] {
        fs::write(
            home.join("settings.json"),
            format!(r#"{{"profile":"{owner}"}}"#),
        )
        .unwrap();
        fs::write(
            home.join("auth.json"),
            format!(r#"{{"profile":"{owner}"}}"#),
        )
        .unwrap();
    }
    // Work the swap must not touch: an untracked file beside the project and
    // anything else the session left in its worker root.
    fs::write(checkout.join("untracked.txt"), b"agent work").unwrap();
    fs::write(worker_root.join("build-cache.txt"), b"warm cache").unwrap();
    // What the sealed source left installed in the worker root.
    fs::create_dir_all(worker_root.join("profile")).unwrap();
    fs::write(
        worker_root.join("profile").join("settings.json"),
        br#"{"profile":"source"}"#,
    )
    .unwrap();
    // Nothing copies this file and nothing overwrites it: only removing the
    // previous profile home can make it disappear.
    fs::write(
        worker_root.join("profile").join("source-only.txt"),
        b"old harness state",
    )
    .unwrap();
    fs::write(worker_root.join("launch.json"), b"{}").unwrap();
    fs::write(
        worker_root.join("ownership.json"),
        serde_json::to_vec(&serde_json::json!({"profile_id": IN_PLACE_SOURCE_PROFILE})).unwrap(),
    )
    .unwrap();
    fs::write(worker_root.join(mj_core::relay::RELAY_STATE_FILE), b"{}").unwrap();
    fs::create_dir_all(worker_root.join(mj_core::relay::RELAY_JOURNAL_DIR)).unwrap();

    let checkpoint = crate::controller::test_support::write_checkpoint_gate_archive_for_harness(
        &archives,
        LATCH_RELAY_SESSION,
        2,
        source_kind,
        IN_PLACE_SOURCE_PROFILE,
        // The source has a conversation, which a same-harness move continues.
        true,
    );
    install_in_place_worker_script(&worker_root, checkpoint.event_frontier, &"a".repeat(64));

    // `validate_move_checkpoint` re-reads the configuration from disk, so the
    // controller's configuration has to be the persisted one.
    let (config, ()) = Config::update(|config| {
        for (id, kind, home) in [
            (IN_PLACE_SOURCE_PROFILE, source_kind, &source_home),
            (
                IN_PLACE_DESTINATION_PROFILE,
                destination_kind,
                &destination_home,
            ),
        ] {
            config.profiles.insert(
                id.into(),
                HarnessProfile {
                    enabled: true,
                    environment: Default::default(),
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
                    ..mj_core::config::HarnessProfile::new(kind, home.clone())
                },
            );
        }
        config.targets.insert(
            "local-bare".into(),
            mj_core::config::TargetTemplate::LocalBare,
        );
        Ok(())
    })
    .unwrap();

    let mut session = checkpoint_test_session(LATCH_RELAY_SESSION);
    session.harness_kind = source_kind;
    session.last_profile = IN_PLACE_SOURCE_PROFILE.into();
    session.target_template_id = "local-bare".into();
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    }));
    session.project_directory = Some(checkout.clone());
    session.state = SessionState::Running;
    session.checkpoint = Some(Box::new(checkpoint.clone()));
    crate::database::save_session(&session).unwrap();
    crate::database::save_materialized_session(&MaterializedSession::empty(LATCH_RELAY_SESSION))
        .unwrap();

    let controller = Controller {
        config,
        state: State {
            sessions: [(LATCH_RELAY_SESSION.into(), session)]
                .into_iter()
                .collect(),
            ..State::default()
        },
    };
    InPlaceFixture {
        _directory: directory,
        worker_root,
        checkout,
        source_home,
        destination_home,
        checkpoint,
        controller,
    }
}

#[cfg(unix)]
pub fn in_place_operation(controller: &Controller, destination_profile: &str) -> MoveOperation {
    let selection = MoveSelection {
        oracle: None,
        oracle_choice: None,
        subagents: None,
        workspace: Default::default(),
        clear_resource_allocation: false,
        session_id: LATCH_RELAY_SESSION.into(),
        profile_id: Some(destination_profile.into()),
        target_template_id: Some("local-bare".into()),
        additional_mounts: Some(Vec::new()),
        resource_allocation: None,
    };
    let source = &controller.state.sessions[LATCH_RELAY_SESSION];
    MoveOperation {
        quota_stop_reason: None,
        quota_continuation: None,
        prepared_destination: None,
        accepted_preparation: None,
        acknowledge_interruption: false,
        workspace_transfer: None,
        handoff: None,
        in_place: true,
        source_checkpoint_only: false,
        operation_id: "move-in-place".into(),
        configuration_fingerprint: controller
            .move_configuration_fingerprint(&selection)
            .unwrap(),
        selection,
        source_profile_id: source.last_profile.clone(),
        source_target_template_id: source.target_template_id.clone(),
        source_target: source.target.clone().map(|v| *v),
        source_native_session_id: source.native_session_id.clone(),
        inherited_resource_allocation: None,
        inherited_additional_mounts: Vec::new(),
        source_additional_mounts: Vec::new(),
        source_resource_allocation: None,
        destination_target: None,
        destination_native_session_id: None,
        destination_store_id: None,
        checkpoint: None,
        recovery_session: None,
        queue: ResumeQueueDisposition::Discard,
        phase: MovePhase::Preparing,
        queue_admission_started: false,
        queue_admission_finished: false,
        cancellation_requested: false,
        created_at: "2026-09-17T12:00:00Z".into(),
        updated_at: "2026-09-17T12:00:00Z".into(),
        error: None,
    }
}

/// A session manager serving the source relay. One of these is one daemon
/// lifetime, so a test models a restart by shutting one down and starting the
/// next against the same journal.
#[cfg(unix)]
pub fn start_source_relay_manager(
    source_relay: &Path,
) -> crate::session_manager::SessionManagerChannels {
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    channels
        .targets
        .send(vec![latch_relay_target(
            source_relay,
            None,
            ReleaseSupport::Supported,
            false,
        )])
        .unwrap();
    channels
}

/// The path of the checked-in fake command dispatcher, which these tests hand
/// the controller as the worker binary to install.
#[cfg(unix)]
pub fn fake_worker_dispatcher() -> PathBuf {
    mj_core::test_hooks::fake_worker_dispatcher()
}
