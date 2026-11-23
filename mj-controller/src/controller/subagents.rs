//! Registration and ownership rules for child sessions on a parent's target.

use mj_core::targets::SshTrafficClass;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::{Controller, now};
use mj_core::config::HarnessKind;
use mj_core::state::{SessionRecord, SessionState, new_session_id};
use mj_core::subagent::SubagentRecord;

pub use mj_session_types::RegisterSubagentRequest;
use mj_store::database::subagent_has_handed_back;

/// Creates a report directory on a target and prints its absolute path.
///
/// `$1` is the directory, which may be relative to the login home the way an
/// SSH or EC2 workspace is. `$2`, when not empty, is a repository whose
/// `info/exclude` must list [`mj_core::subagent::PROJECT_REPORT_ROOT_DIR`], so a
/// report root inside a bare project never shows in `git status`. A directory
/// that is not a repository has no status to keep clean. The absolute path is
/// what the parent and child are told, because neither runs in the login home.
const PREPARE_REPORT_DIR_SCRIPT: &str = r#"set -eu
cd
mkdir -p -- "$1"
if [ -n "$2" ] && exclude=$(git -C "$2" rev-parse --git-path info/exclude 2>/dev/null); then
  case "$exclude" in /*) ;; *) exclude="$2/$exclude" ;; esac
  line="/$3/"
  if ! grep -qxF -- "$line" "$exclude" 2>/dev/null; then
    mkdir -p -- "$(dirname -- "$exclude")"
    if [ -s "$exclude" ] && [ -n "$(tail -c 1 -- "$exclude")" ]; then
      printf '
' >> "$exclude"
    fi
    printf '%s
' "$line" >> "$exclude"
  fi
fi
cd -- "$1"
pwd -P
"#;

/// The argv that runs [`PREPARE_REPORT_DIR_SCRIPT`] for `directory`, adding
/// the exclude line to `exclude_in` when given.
fn prepare_report_dir_argv(directory: &str, exclude_in: Option<&str>) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        PREPARE_REPORT_DIR_SCRIPT.into(),
        "sh".into(),
        directory.into(),
        exclude_in.unwrap_or_default().into(),
        mj_core::subagent::PROJECT_REPORT_ROOT_DIR.into(),
    ]
}

/// Run [`PREPARE_REPORT_DIR_SCRIPT`] on `backend` and return the absolute
/// directory it printed.
fn prepare_report_dir(
    executor: &impl mj_core::targets::CommandExecutor,
    backend: &mj_core::targets::TargetLocator,
    session_id: &str,
    directory: &str,
    exclude_in: Option<&str>,
) -> Result<String> {
    let command = mj_core::targets::command_on_locator(
        SshTrafficClass::Control,
        backend,
        session_id,
        prepare_report_dir_argv(directory, exclude_in),
        "create the sub-agent report directory",
    )?;
    let output = super::execute_checked(executor, command)?;
    let absolute = String::from_utf8(output.stdout)
        .context("the sub-agent report directory is not UTF-8")?
        .trim_end_matches('\n')
        .to_owned();
    ensure!(
        absolute.starts_with('/'),
        "the target did not report an absolute sub-agent report directory: {absolute:?}"
    );
    Ok(absolute)
}

// Work in the root's pinned current directory: rm never follows a child
// symlink. A report root, or the `.mj` holding a project's root, that is a
// symlink is refused before any deletion; symlinked ancestors above it are
// ordinary (macOS `/var`, a home directory on another volume) and allowed.
// An absent root is already clean.
const REMOVE_REPORT_DIRS_SCRIPT: &str = r#"set -eu
cd
root=$1
shift
case "$root" in /*) ;; *) root="$(pwd -P)/$root" ;; esac
if [ -L "$root" ]; then echo 'refusing a symlinked report root' >&2; exit 1; fi
case "$root" in
*/.mj/agents)
    if [ -L "${root%/agents}" ]; then echo 'refusing a symlinked report root' >&2; exit 1; fi ;;
esac
[ -e "$root" ] || exit 0
cd -- "$root"
status=0
for child do
    case "$child" in ''|*[!0123456789abcdefABCDEF]*) echo 'invalid report child id' >&2; exit 1;; esac
    rm -rf -- "$child" || status=1
done
# A shared bare-project root survives while anyone else's reports remain.
name=${root##*/}
cd ..
remaining=$(ls -A -- "$name")
if [ -z "$remaining" ]; then
    rmdir -- "$name"
fi
exit "$status"
"#;

/// A pre-close snapshot survives the cascade deleting the children's rows.
struct SubagentReportCleanup {
    backend: mj_core::targets::TargetLocator,
    session_id: String,
    root: String,
    child_ids: Vec<String>,
}

impl SubagentReportCleanup {
    fn execute(&self, executor: &impl mj_core::targets::CommandExecutor) -> Result<()> {
        validate_report_root(&self.root)?;
        for child in &self.child_ids {
            ensure!(
                !child.is_empty() && child.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "invalid sub-agent report child id {child:?}"
            );
        }
        let mut argv = vec![
            "sh".into(),
            "-c".into(),
            REMOVE_REPORT_DIRS_SCRIPT.into(),
            "mj-remove-reports".into(),
            self.root.clone(),
        ];
        argv.extend(self.child_ids.iter().cloned());
        let command = mj_core::targets::command_on_locator(
            SshTrafficClass::Control,
            &self.backend,
            &self.session_id,
            argv,
            "remove sub-agent report directories",
        )?;
        super::execute_checked(executor, command)?;
        Ok(())
    }
}

pub(super) fn cleanup_owned_reports(
    _authority: &super::environment::Reclamation,
    backend: &mj_core::targets::TargetLocator,
    session_id: &str,
    reports: &[(String, String)],
    executor: &impl mj_core::targets::CommandExecutor,
) -> Result<()> {
    for (child_id, directory) in reports {
        let directory = Path::new(directory);
        ensure!(
            directory.file_name().and_then(|name| name.to_str()) == Some(child_id),
            "environment report path does not name its child"
        );
        let root = directory
            .parent()
            .and_then(|path| path.to_str())
            .context("environment report root is unavailable")?;
        SubagentReportCleanup {
            backend: backend.clone(),
            session_id: session_id.to_owned(),
            root: root.to_owned(),
            child_ids: vec![child_id.clone()],
        }
        .execute(executor)?;
    }
    Ok(())
}

fn validate_report_root(root: &str) -> Result<()> {
    let path = Path::new(root);
    ensure!(
        path.ends_with(mj_core::subagent::REPORT_ROOT_DIR)
            || path.ends_with(mj_core::subagent::PROJECT_REPORT_ROOT_DIR),
        "invalid sub-agent report root {root:?}"
    );
    ensure!(
        !path.components().any(|component| matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )),
        "sub-agent report root contains traversal: {root:?}"
    );
    Ok(())
}

impl Controller {
    /// Prepare the shared report directory before registering a child.
    pub fn prepare_subagent_report_root(
        &self,
        parent_session_id: &str,
        executor: &impl mj_core::targets::CommandExecutor,
    ) -> Result<String> {
        let parent = self
            .state
            .sessions
            .get(parent_session_id)
            .with_context(|| format!("unknown parent session {parent_session_id}"))?;
        let locator = parent
            .target
            .as_ref()
            .context("parent session has no live target")?;
        let backend = super::backend::backend_locator(locator, parent, &self.config)?;
        let checkout = self.state.checkout(parent_session_id)?;
        let (root, exclude_in) = subagent_report_root(parent, &backend, &checkout);
        prepare_report_dir(
            executor,
            &backend,
            parent_session_id,
            &root,
            exclude_in.as_deref(),
        )
    }

    /// Create a registered child's own report directory on its target. A
    /// child registered without one has nothing to create.
    pub(super) fn prepare_subagent_report_dir(
        &self,
        session_id: &str,
        backend: &mj_core::targets::TargetLocator,
        executor: &impl mj_core::targets::CommandExecutor,
    ) -> Result<()> {
        if crate::database::load_subagent(session_id)?.is_none() {
            return Ok(());
        }
        let Some(directory) = crate::database::load_subagent_report(session_id)?.report_dir else {
            return Ok(());
        };
        prepare_report_dir(executor, backend, session_id, &directory, None)?;
        Ok(())
    }

    /// Store-only fixtures have no launching daemon. Production must register
    /// with the manager first, or sends can misclassify a starting child.
    #[doc(hidden)]
    pub fn register_subagent(
        &mut self,
        request: RegisterSubagentRequest,
    ) -> Result<SubagentRecord> {
        self.register_subagent_inner(request, None)
    }

    /// Register managed launches before the committed relation becomes addressable.
    pub fn register_subagent_with_manager(
        &mut self,
        request: RegisterSubagentRequest,
        manager: &crate::session_manager::SessionManagerControl,
    ) -> Result<SubagentRecord> {
        self.register_subagent_inner(request, Some(manager))
    }

    fn register_subagent_inner(
        &mut self,
        request: RegisterSubagentRequest,
        manager: Option<&crate::session_manager::SessionManagerControl>,
    ) -> Result<SubagentRecord> {
        let request_key = request.kind.request_key(&request.request_key);
        let reviewer = request.kind == mj_core::subagent::SubagentKind::Reviewer;
        let oracle = request.kind == mj_core::subagent::SubagentKind::Oracle;
        if let Some(existing) =
            crate::database::lookup_subagent_request(&request.parent_session_id, &request_key)?
        {
            return Ok(existing);
        }
        ensure!(
            !request.request_key.trim().is_empty(),
            "sub-agent request key cannot be empty"
        );
        ensure!(
            !request.task_name.trim().is_empty(),
            "sub-agent task name cannot be empty"
        );
        ensure!(
            !request.initial_prompt.trim().is_empty(),
            "sub-agent instructions cannot be empty"
        );
        let parent = self
            .state
            .sessions
            .get(&request.parent_session_id)
            .with_context(|| format!("unknown parent session {}", request.parent_session_id))?
            .clone();
        // Every top-level session has the message-only server that carries
        // ask_oracle, so the oracle does not depend on delegation.
        ensure!(
            reviewer || oracle || parent.harness_kind.supports_delegation_tools(),
            "{} sessions cannot spawn Mjolnir sub-agents",
            parent.harness_kind.display_name()
        );
        if !reviewer && !oracle {
            ensure_parent_may_delegate(&parent)?;
        }
        ensure!(
            parent.state != SessionState::StartupCleanup,
            "parent startup cleanup is pending"
        );
        ensure!(parent.state.is_active(), "parent session is not active");
        ensure!(parent.target.is_some(), "parent session has no live target");
        let parent_subagent = crate::database::load_subagent(&parent.id)?;
        mj_core::subagent::validate_subagent_parent(
            parent_subagent.as_ref(),
            request.kind,
            request.supervisor,
        )?;
        if request.supervisor {
            ensure!(
                parent.supervisors,
                "supervisors are not enabled for this session"
            );
            ensure!(
                parent
                    .subagents
                    .as_ref()
                    .is_some_and(mj_core::subagent::SubagentPolicy::uses_mjolnir),
                "supervisors require Mjolnir delegation"
            );
        }
        let profile = self
            .config
            .enabled_profile(&request.profile_id)
            .with_context(|| {
                format!("sub-agent profile {:?} is unavailable", request.profile_id)
            })?;
        profile.ensure_ready(&request.profile_id)?;
        if profile.kind == HarnessKind::Muse {
            let multiple_roots = !mj_core::state::applied_container_settings(&parent)
                .additional_mounts
                .is_empty()
                || (self
                    .state
                    .checkout(&parent.id)?
                    .project_directory()
                    .is_none()
                    && parent
                        .project_bundle(&self.config)
                        .is_some_and(|bundle| bundle.repositories.len() > 1));
            ensure!(
                !multiple_roots,
                "{} ACP supports one workspace root; this parent exposes multiple roots",
                profile.kind.display_name()
            );
        }
        if !reviewer && !oracle {
            self.ensure_subagent_slot_available_for(&parent.id, None, request.supervisor)?;
        }

        let child_id = new_session_id()?;
        let target = borrowed_locator(
            parent.target.as_ref().expect("live target checked above"),
            &parent.id,
            &child_id,
        )?;
        let created_at = now();
        let session = SessionRecord {
            oracle_choice: None,
            oracle: false,
            supervisors: false,
            notifications_ignored: true,
            project: parent.project.clone(),
            target_runtime: Some(match &parent.target_runtime {
                Some(runtime) => runtime.clone(),
                None => parent
                    .target_runtime_settings(&self.config)?
                    .into_owned()
                    .into(),
            }),
            launch_base: None,
            launch_branch: None,
            checkout: None,
            publication: None,
            // A child shares its parent's container, so it shares the build
            // cache that container was created with.
            build_cache: parent.build_cache.clone(),
            // Supervisors inherit their parent's Mjolnir delegation policy
            // (including straightforward/subtle overrides); other children
            // never receive ordinary delegation tools.
            subagents: if request.supervisor {
                parent.subagents.clone()
            } else {
                Some(mj_core::subagent::SubagentPolicy::None)
            },
            create_managed_worktree: Some(false),
            archived: false,
            // A child runs inside its parent's container, so it works in the
            // parent's workspace, including the legacy shared one.
            container_workspace: parent.container_workspace.clone(),
            id: child_id.clone(),
            workspace_id: parent.workspace_id.clone(),
            title: request.task_name.clone(),
            harness_kind: profile.kind,
            last_profile: request.profile_id.clone(),
            bundle_id: parent.bundle_id.clone(),
            project_directory: parent.project_directory.clone(),
            managed_worktree: None,
            review: None,
            // A child works in its parent's tree and gets what its parent got,
            // except a reviewer: it reads the change it is asked about, so it
            // carries no project memory, which was 99.96% of what each
            // reviewer sent to its target (two copies of the memory, #1580).
            no_project_memory: parent.no_project_memory || reviewer,
            no_github_events: parent.no_github_events,
            no_github_credentials: parent.no_github_credentials
                || request.no_github_credentials
                || !self.config.github.session_credentials,
            target_template_id: parent.target_template_id.clone(),
            resource_allocation: mj_core::state::applied_container_settings(&parent)
                .resource_allocation
                .cloned(),
            additional_mounts: mj_core::state::applied_container_settings(&parent)
                .additional_mounts
                .to_vec(),
            state: SessionState::Provisioning,
            target: Some(Box::new(target)),
            native_session_id: None,
            acp_session_title: None,
            session_title_override: Some(request.task_name.clone()),
            created_at: created_at.clone(),
            updated_at: created_at.clone(),
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            dismissed_failure: None,
            failure_generation: 0,
            last_checkpoint_error: None,
            checkpoint: None,
        };
        let handback_tool = reviewer || child_gets_handback_tool(profile.kind);
        let report_dir = request
            .report_root
            .as_deref()
            .map(|root| format!("{}/{child_id}", root.trim_end_matches('/')));
        // The assignment and directory are per-task data; standing role policy
        // is composed by the worker from the durable subagent record.
        let assignment = if oracle {
            format!(
                "{}\n\n{}",
                mj_core::subagent::READ_ONLY_TASK_SCOPE,
                request.initial_prompt
            )
        } else {
            request.initial_prompt
        };
        let initial_prompt = match &report_dir {
            Some(report_dir) => format!(
                "{}\n\n{}",
                mj_core::subagent::report_directory_note(report_dir),
                assignment
            ),
            None => assignment,
        };
        let relation = SubagentRecord {
            kind: request.kind,
            reviewer_launch: request.reviewer_launch,
            child_session_id: child_id.clone(),
            parent_session_id: parent.id,
            task_name: request.task_name,
            profile_id: request.profile_id,
            model: request.model,
            effort: request.effort,
            working_directory: request.working_directory,
            initial_prompt,
            request_key: request.request_key,
            created_at,
            noticed_turn: None,
            reported_finish: None,
            handback_tool,
            supervisor: request.supervisor,
        };
        let startup = initial_startup_deliveries(&relation, request.fast_mode)?;
        let registration = manager
            .map(|manager| manager.register_starting_blocking(&child_id))
            .transpose()?;
        crate::database::register_subagent_with_startup(
            &session,
            &relation,
            startup,
            report_dir.as_deref(),
            Some(request.category.as_str()),
        )?;
        if let Some(registration) = registration {
            registration.publish()?;
        }
        self.state.sessions.insert(child_id, session);
        self.state
            .subagents
            .insert(relation.child_session_id.clone(), (&relation).into());
        Ok(relation)
    }

    /// Refuse to start another child process tree for `parent_session_id`
    /// when it already has the maximum number of live children.
    ///
    /// `starting` is the child a `send_message` is about to start again from
    /// parked; it is not counted against itself. A spawn passes `None`.
    ///
    /// Every child whose worker may be holding processes in the parent's
    /// container counts, idle or not: those processes are what the cap
    /// protects (#1161). A parked, stopped, failed or lost child holds none.
    ///
    /// Only ordinary workers count. A reviewer or the oracle holds processes
    /// but never uses a delegation slot.
    pub fn ensure_subagent_slot_available(
        &self,
        parent_session_id: &str,
        starting: Option<&str>,
    ) -> Result<()> {
        self.ensure_subagent_slot_available_for(parent_session_id, starting, false)
    }

    /// Container-wide worker slot check with anti-starvation supervisor sub-cap.
    pub fn ensure_subagent_slot_available_for(
        &self,
        parent_session_id: &str,
        starting: Option<&str>,
        starting_supervisor: bool,
    ) -> Result<()> {
        let root_id = self
            .state
            .workspace_owner(parent_session_id)
            .map(|session| session.id.as_str())
            .unwrap_or(parent_session_id);
        let mut seen = std::collections::BTreeSet::new();
        let mut live = Vec::new();
        let mut live_supervisors = 0usize;
        for child_id in self.state.descendant_subagent_ids(root_id) {
            if Some(child_id.as_str()) == starting || !seen.insert(child_id.clone()) {
                continue;
            }
            let Some(meta) = self.state.subagents.get(&child_id) else {
                continue;
            };
            if meta.kind != mj_core::subagent::SubagentKind::Worker {
                continue;
            }
            let Some(session) = self.state.sessions.get(&child_id) else {
                continue;
            };
            if session.state.has_live_worker() {
                if meta.supervisor {
                    live_supervisors += 1;
                }
                live.push(LiveSubagent {
                    child_session_id: child_id,
                    title: session.message_name(),
                    state: live_subagent_state(session),
                });
            }
        }
        // Also check direct DB rows in case state hasn't caught up yet.
        for child in crate::database::list_subagents(parent_session_id)? {
            if child.kind != mj_core::subagent::SubagentKind::Worker
                || Some(child.child_session_id.as_str()) == starting
                || !seen.insert(child.child_session_id.clone())
            {
                continue;
            }
            let Some(session) = self.state.sessions.get(&child.child_session_id) else {
                continue;
            };
            if session.state.has_live_worker() {
                if child.supervisor {
                    live_supervisors += 1;
                }
                live.push(LiveSubagent {
                    child_session_id: child.child_session_id.clone(),
                    title: session.message_name(),
                    state: live_subagent_state(session),
                });
            }
        }
        let maximum = self.config.subagents.max_concurrent;
        ensure!(live.len() < maximum, "{}", slot_refusal(&live, maximum));
        if starting_supervisor {
            let max_supervisors = maximum / 2;
            ensure!(
                maximum >= 2 && live_supervisors < max_supervisors,
                "{}",
                slot_refusal(&live, max_supervisors)
            );
        }
        Ok(())
    }
}

/// Older daemons registered children before saving their initial input. A
/// replay of a still-provisioning registration completes that old transaction
/// boundary before any worker can start. Existing groups retain their payload.
pub fn recover_registered_subagent_startup(
    relation: &SubagentRecord,
    fast_mode: bool,
) -> Result<()> {
    if relation.kind != mj_core::subagent::SubagentKind::Reviewer
        && crate::database::load_startup_group(
            &relation.child_session_id,
            &mj_core::continuation::GeneratedPrompt::subagent_spawn_id(&relation.request_key),
        )?
        .is_empty()
    {
        crate::database::enqueue_startup_deliveries(initial_startup_deliveries(
            relation, fast_mode,
        )?)?;
    }
    Ok(())
}

fn initial_startup_deliveries(
    relation: &SubagentRecord,
    fast_mode: bool,
) -> Result<Vec<crate::database::StartupDelivery>> {
    if relation.kind == mj_core::subagent::SubagentKind::Reviewer {
        return Ok(Vec::new());
    }
    let group_id = mj_core::continuation::GeneratedPrompt::subagent_spawn_id(&relation.request_key);
    crate::database::api_followup_steps(
        &group_id,
        crate::database::StartFollowup {
            model: relation.model.clone(),
            effort: relation.effort.clone(),
            fast_mode,
            prompt: Some(relation.initial_prompt.clone()),
        },
    )
    .into_iter()
    .map(|(command_id, step)| {
        Ok(crate::database::StartupDelivery {
            session_id: relation.child_session_id.clone(),
            command_id,
            step_json: serde_json::to_string(&step)?,
            phase: "pending".into(),
            group_id: Some(group_id.clone()),
            accepted_ordinal: None,
            error: None,
        })
    })
    .collect()
}

/// One child that holds processes on its parent's target, as a cap refusal
/// names it.
struct LiveSubagent {
    child_session_id: String,
    title: String,
    state: &'static str,
}

/// The word a cap refusal uses for a live child's state. An idle child is
/// told apart from a working one, because only an idle child can be closed
/// without losing work.
fn live_subagent_state(session: &SessionRecord) -> &'static str {
    match session.state {
        SessionState::Provisioning => "starting",
        SessionState::Checkpointing => "checkpointing",
        SessionState::Closing | SessionState::Destroying | SessionState::StartupCleanup => {
            "stopping"
        }
        SessionState::Disconnected => "disconnected",
        _ => match crate::database::load_materialized_session_summary(&session.id) {
            Ok(Some(summary))
                if matches!(
                    summary.execution,
                    mj_core::state::MaterializedExecutionState::Idle
                ) =>
            {
                "idle"
            }
            _ => "running",
        },
    }
}

/// The refusal a parent model reads when it asks for one child too many: how
/// many are live, the maximum, which ones they are, and how a slot frees up.
fn slot_refusal(live: &[LiveSubagent], maximum: usize) -> String {
    let listed = live
        .iter()
        .map(|child| {
            format!(
                "{} \"{}\" ({})",
                mj_core::state::short_id(&child.child_session_id),
                child.title,
                child.state
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "this session already has {} live sub-agents and the maximum is {maximum}. \
         Live sub-agents: {listed}. A sub-agent frees its slot when it hands back its \
         report (it is then parked and holds no processes until you send it input) or \
         when you close it.",
        live.len()
    )
}

/// What a parent's record keeps about a child its suspend stops: the child's
/// listed title, one line of its task, and whether it had handed back.
pub fn stopped_subagent(
    state: &mj_core::state::State,
    child_session_id: &str,
    liveness: &mj_core::subagent::Liveness,
) -> Result<mj_core::subagent::StoppedSubagent> {
    let relation = state
        .subagents
        .get(child_session_id)
        .with_context(|| format!("unknown sub-agent session {child_session_id}"))?;
    let title = state
        .sessions
        .get(child_session_id)
        .map_or_else(|| relation.task_name.clone(), SessionRecord::message_name);
    let report_dir = crate::database::load_subagent_report(child_session_id)?.report_dir;
    let launch = crate::database::load_subagent(child_session_id)?
        .context("sub-agent launch record is missing")?;
    Ok(mj_core::subagent::StoppedSubagent {
        child_session_id: child_session_id.to_owned(),
        title,
        task: mj_core::subagent::task_summary(&launch.initial_prompt, report_dir.as_deref()),
        handed_back: subagent_has_handed_back(child_session_id, liveness)?,
    })
}

fn sibling_path(path: &Path, parent_id: &str, child_id: &str) -> Result<PathBuf> {
    ensure!(
        path.ends_with(parent_id),
        "parent target path does not end in its session id"
    );
    let parent = path.parent().context("parent target path has no parent")?;
    Ok(mj_core::target_path::join(parent, Path::new(child_id)))
}

fn borrowed_locator(
    target: &mj_core::state::TargetLocator,
    parent_id: &str,
    child_id: &str,
) -> Result<mj_core::state::TargetLocator> {
    use mj_core::state::TargetLocator;
    let container_owner = match target {
        TargetLocator::LocalPodman { borrowed_from, .. }
        | TargetLocator::LocalDocker { borrowed_from, .. }
        | TargetLocator::AppleContainer { borrowed_from, .. }
        | TargetLocator::SshPodman { borrowed_from, .. }
        | TargetLocator::SshDocker { borrowed_from, .. }
        | TargetLocator::SshAppleContainer { borrowed_from, .. }
        | TargetLocator::SshNitro { borrowed_from, .. } => {
            borrowed_from.as_deref().unwrap_or(parent_id)
        }
        _ => parent_id,
    };
    Ok(match target {
        TargetLocator::LocalBare { worker_root } => TargetLocator::LocalBare {
            worker_root: sibling_path(worker_root, parent_id, child_id)?,
        },
        TargetLocator::SshBare {
            host,
            workspace,
            worker_id: _,
        } => TargetLocator::SshBare {
            host: host.clone(),
            workspace: workspace.clone(),
            worker_id: Some(child_id.to_owned()),
        },
        // A container child runs its own worker inside the parent's container
        // and preserves its original owner even when the direct parent also
        // borrows it. Cleanup and whole-target operations stay with that owner.
        TargetLocator::LocalPodman {
            container_id,
            workspace_storage,
            ..
        } => TargetLocator::LocalPodman {
            container_id: container_id.clone(),
            workspace_storage: workspace_storage.clone(),
            borrowed_from: Some(container_owner.to_owned()),
        },
        TargetLocator::LocalDocker { container_id, .. } => TargetLocator::LocalDocker {
            container_id: container_id.clone(),
            borrowed_from: Some(container_owner.to_owned()),
        },
        TargetLocator::AppleContainer { container_id, .. } => TargetLocator::AppleContainer {
            container_id: container_id.clone(),
            borrowed_from: Some(container_owner.to_owned()),
        },
        TargetLocator::SshPodman {
            host,
            container_id,
            workspace_storage,
            ..
        } => TargetLocator::SshPodman {
            host: host.clone(),
            container_id: container_id.clone(),
            workspace_storage: workspace_storage.clone(),
            borrowed_from: Some(container_owner.to_owned()),
        },
        TargetLocator::SshDocker {
            host, container_id, ..
        } => TargetLocator::SshDocker {
            host: host.clone(),
            container_id: container_id.clone(),
            borrowed_from: Some(container_owner.to_owned()),
        },
        TargetLocator::SshAppleContainer {
            host, container_id, ..
        } => TargetLocator::SshAppleContainer {
            host: host.clone(),
            container_id: container_id.clone(),
            borrowed_from: Some(container_owner.to_owned()),
        },
        TargetLocator::SshNitro {
            host, container_id, ..
        } => TargetLocator::SshNitro {
            host: host.clone(),
            container_id: container_id.clone(),
            borrowed_from: Some(container_owner.to_owned()),
        },
        // EC2 children keep the parent's locator unchanged, as they did before
        // container borrowing was recorded.
        other @ TargetLocator::AwsEc2 { .. } => other.clone(),
    })
}

/// Whether a child can be given the `handback` tool: it can whenever its
/// harness receives Mjolnir's delegation tools. Other harnesses keep reporting
/// through their last message.
fn child_gets_handback_tool(harness: HarnessKind) -> bool {
    harness.supports_delegation_tools()
}

/// A parent may delegate to Mjolnir children only if its own stored choice
/// says so; historical records without a policy use native delegation. A parent
/// using its harness's native delegation never received the Mjolnir tools, so
/// a request from it is stale.
fn ensure_parent_may_delegate(parent: &SessionRecord) -> Result<()> {
    ensure!(
        parent
            .subagents
            .as_ref()
            .is_some_and(mj_core::subagent::SubagentPolicy::uses_mjolnir),
        "this session does not allow Mjolnir sub-agents"
    );
    Ok(())
}

/// Where a parent's children keep their report directories, before the target
/// resolves it, and the repository whose `info/exclude` must list it.
pub(super) fn subagent_report_root(
    parent: &SessionRecord,
    backend: &mj_core::targets::TargetLocator,
    checkout: &mj_core::state::Checkout<'_>,
) -> (String, Option<String>) {
    match checkout.project_directory() {
        Some(project) => {
            let project = project.to_string_lossy().trim_end_matches('/').to_owned();
            (
                format!("{project}/{}", mj_core::subagent::PROJECT_REPORT_ROOT_DIR),
                Some(project),
            )
        }
        None => {
            let workspace =
                super::network_git::workspace_root(backend, parent.container_workspace.as_deref());
            (
                format!(
                    "{}/{}",
                    workspace.trim_end_matches('/'),
                    mj_core::subagent::REPORT_ROOT_DIR
                ),
                None,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    // Hard-won: #1331: report cleanup must not escape into unrelated reports or symlink targets.
    #[test]
    fn report_cleanup_preserves_other_children_and_refuses_path_and_symlink_escapes() {
        use mj_core::targets::{ProcessExecutor, TargetLocator};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".mj/agents");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(root.join("aa")).unwrap();
        std::fs::create_dir_all(root.join("bb")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), "outside report").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("cc")).unwrap();
        let mut cleanup = SubagentReportCleanup {
            backend: TargetLocator::LocalBare {
                worker_root: temp
                    .path()
                    .join("0123456789abcdef0123456789abcdef")
                    .to_string_lossy()
                    .into_owned(),
            },
            session_id: "0123456789abcdef0123456789abcdef".into(),
            root: root.to_str().unwrap().into(),
            child_ids: vec!["aa".into(), "cc".into()],
        };
        cleanup.execute(&ProcessExecutor).unwrap();
        assert!(!root.join("aa").exists());
        assert!(!root.join("cc").exists());
        assert!(root.join("bb").is_dir(), "another parent's child survives");
        assert!(
            outside.join("keep").is_file(),
            "child symlinks are never followed"
        );

        cleanup.child_ids = vec!["../outside".into()];
        assert!(cleanup.execute(&ProcessExecutor).is_err());
        cleanup.child_ids = vec!["bb".into()];
        cleanup.root = format!("{}/../.mj/agents", temp.path().display());
        assert!(cleanup.execute(&ProcessExecutor).is_err());
        cleanup.root = outside.to_str().unwrap().into();
        assert!(cleanup.execute(&ProcessExecutor).is_err());

        // A symlinked `.mj` or report root is refused.
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("agents/bb")).unwrap();
        let linked_project = temp.path().join("linked-project");
        std::fs::create_dir(&linked_project).unwrap();
        std::os::unix::fs::symlink(&elsewhere, linked_project.join(".mj")).unwrap();
        cleanup.root = linked_project.join(".mj/agents").to_str().unwrap().into();
        assert!(cleanup.execute(&ProcessExecutor).is_err());
        assert!(elsewhere.join("agents/bb").is_dir());
        let linked_root = temp.path().join("linked-root");
        std::fs::create_dir_all(&linked_root).unwrap();
        std::os::unix::fs::symlink(elsewhere.join("agents"), linked_root.join(".mj-agents"))
            .unwrap();
        cleanup.root = linked_root.join(".mj-agents").to_str().unwrap().into();
        assert!(cleanup.execute(&ProcessExecutor).is_err());
        assert!(elsewhere.join("agents/bb").is_dir());

        // A symlinked ancestor above the root is ordinary and allowed.
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(temp.path(), &alias).unwrap();
        std::fs::create_dir(root.join("dd")).unwrap();
        cleanup.root = alias.join(".mj/agents").to_str().unwrap().into();
        cleanup.child_ids = vec!["dd".into()];
        cleanup.execute(&ProcessExecutor).unwrap();
        assert!(!root.join("dd").exists());
        assert!(root.join("bb").is_dir());
        cleanup.child_ids = vec!["bb".into()];

        cleanup.root = root.to_str().unwrap().into();
        cleanup.execute(&ProcessExecutor).unwrap();
        assert!(!root.exists(), "only an empty report root is removed");
        cleanup.execute(&ProcessExecutor).unwrap();
        assert!(outside.join("keep").is_file());
    }

    fn run_prepare_script(directory: &Path, exclude_in: Option<&Path>) -> String {
        let argv = prepare_report_dir_argv(
            &directory.to_string_lossy(),
            exclude_in
                .map(|path| path.to_string_lossy().into_owned())
                .as_deref(),
        );
        let output = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .expect("run the report directory script");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end()
            .to_owned()
    }

    /// A bare project's report root is inside the project, so the script
    /// lists it in the repository's `info/exclude` exactly once and the
    /// project's `git status` stays clean.
    #[test]
    #[cfg_attr(windows, ignore = "#1297: runs sh, du or touch on the controller")]
    fn the_report_directory_script_creates_the_directory_and_keeps_git_status_clean() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&project)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).unwrap()
        };
        git(&["init", "-q"]);
        // An exclude file without a trailing newline must not have its last
        // line joined to the new one.
        std::fs::write(project.join(".git/info/exclude"), "*.tmp").unwrap();
        let root = project.join(mj_core::subagent::PROJECT_REPORT_ROOT_DIR);
        let printed = run_prepare_script(&root, Some(&project));
        assert_eq!(
            Path::new(&printed),
            root.canonicalize().unwrap(),
            "the script prints the absolute directory"
        );
        run_prepare_script(&root, Some(&project));
        std::fs::write(root.join("report.md"), "details").unwrap();
        let exclude = std::fs::read_to_string(project.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude, "*.tmp\n/.mj/agents/\n");
        assert_eq!(git(&["status", "--porcelain", "--ignored=no"]), "");

        // Outside a repository there is no exclude to write.
        let plain = temp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let reports = plain.join(".mj/agents/child");
        run_prepare_script(&reports, Some(&plain));
        assert!(reports.is_dir());
    }

    #[test]
    fn a_report_root_is_under_the_workspace_or_inside_a_bare_project() {
        let mut parent = super::super::test_support::checkpoint_test_session("parent-1");
        let backend = mj_core::targets::TargetLocator::LocalBare {
            worker_root: "/var/lib/hel/workers/parent-1".into(),
        };
        parent.project_directory = None;
        let checkout = parent.checkout();
        assert_eq!(
            subagent_report_root(&parent, &backend, &checkout),
            ("/var/lib/hel/workers/parent-1/.mj-agents".to_owned(), None)
        );
        parent.project_directory = Some("/home/dev/project/".into());
        let checkout = parent.checkout();
        assert_eq!(
            subagent_report_root(&parent, &backend, &checkout),
            (
                "/home/dev/project/.mj/agents".to_owned(),
                Some("/home/dev/project".to_owned())
            )
        );
    }

    // Hard-won: 5cea6d11569a: a container child inherited its parent's locator and was refused before starting.
    #[test]
    fn a_container_child_borrows_its_parents_container() {
        use mj_core::state::{PodmanWorkspaceLocator, TargetLocator};

        let parent_id = "0123456789abcdef0123456789abcdef";
        let child_id = "fedcba9876543210fedcba9876543210";
        let container = mj_core::targets::resource_name(parent_id).unwrap();

        let local = borrowed_locator(
            &TargetLocator::LocalPodman {
                container_id: container.clone(),
                workspace_storage: PodmanWorkspaceLocator::Volume {
                    name: "parent-volume".to_owned(),
                },
                borrowed_from: None,
            },
            parent_id,
            child_id,
        )
        .unwrap();
        assert_eq!(
            local,
            TargetLocator::LocalPodman {
                container_id: container.clone(),
                workspace_storage: PodmanWorkspaceLocator::Volume {
                    name: "parent-volume".to_owned(),
                },
                borrowed_from: Some(parent_id.to_owned()),
            }
        );

        let remote = borrowed_locator(
            &TargetLocator::SshPodman {
                host: "builder".to_owned(),
                container_id: container.clone(),
                workspace_storage: PodmanWorkspaceLocator::ContainerLayer,
                borrowed_from: None,
            },
            parent_id,
            child_id,
        )
        .unwrap();
        assert_eq!(
            remote,
            TargetLocator::SshPodman {
                host: "builder".to_owned(),
                container_id: container,
                workspace_storage: PodmanWorkspaceLocator::ContainerLayer,
                borrowed_from: Some(parent_id.to_owned()),
            }
        );
        let reviewer_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        for locator in [local, remote] {
            assert_eq!(
                borrowed_locator(&locator, child_id, reviewer_id).unwrap(),
                locator,
                "a child's reviewer must retain the original container owner"
            );
        }
    }

    #[test]
    fn a_parent_using_native_delegation_cannot_spawn_mjolnir_children() {
        let parent = |choice: Option<bool>| {
            let mut session = crate::controller::test_support::checkpoint_test_session("parent");
            session.subagents = choice.map(|enabled| {
                if enabled {
                    mj_core::subagent::SubagentPolicy::Mjolnir {
                        straightforward: None,
                        subtle: None,
                    }
                } else {
                    mj_core::subagent::SubagentPolicy::Native
                }
            });
            session
        };

        assert_eq!(
            ensure_parent_may_delegate(&parent(Some(false)))
                .unwrap_err()
                .to_string(),
            "this session does not allow Mjolnir sub-agents"
        );
        assert_eq!(
            ensure_parent_may_delegate(&parent(None))
                .unwrap_err()
                .to_string(),
            "this session does not allow Mjolnir sub-agents"
        );
        assert!(ensure_parent_may_delegate(&parent(Some(true))).is_ok());
    }

    /// #1161: idle children held the processes that exhausted their parent's
    /// container, yet did not count against the cap. Every child that holds
    /// processes counts now; a parked one does not. The refusal is what the
    /// parent model reads, so it names the live children and how to free a
    /// slot.
    // Hard-won: 6927da2ba976: the live-child cap ignored idle workers until their shared container ran out of process slots.
    #[test]
    fn the_cap_counts_every_child_holding_processes_and_names_them() {
        const MARKER: &str = "MJ_TEST_SUBAGENT_CAP_CHILD";
        if std::env::var_os(MARKER).is_none() {
            let directory = tempfile::tempdir().unwrap();
            super::super::test_support::IsolatedTest::new(super::super::test_support::test_name(
                module_path!(),
                "the_cap_counts_every_child_holding_processes_and_names_them",
            ))
            .env(MARKER, "1")
            .isolated_store(directory.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let parent = super::super::test_support::checkpoint_test_session("parent-1");
        crate::database::save_session(&parent).unwrap();
        let children = [
            ("aaaaaaaa-idle", "Audit the lockfile", SessionState::Running),
            ("bbbbbbbb-busy", "Run the suite", SessionState::Running),
            ("cccccccc-park", "Map the parser", SessionState::Parked),
            ("dddddddd-done", "Old task", SessionState::Stopped),
        ];
        for (id, title, state) in children {
            let mut child = super::super::test_support::checkpoint_test_session(id);
            child.state = state;
            child.session_title_override = Some(title.into());
            crate::database::save_subagent_session(
                &child,
                &SubagentRecord {
                    kind: mj_core::subagent::SubagentKind::Worker,
                    reviewer_launch: None,
                    child_session_id: id.into(),
                    parent_session_id: "parent-1".into(),
                    task_name: title.into(),
                    profile_id: "codex".into(),
                    model: None,
                    effort: None,
                    working_directory: PathBuf::new(),
                    initial_prompt: "do it".into(),
                    request_key: format!("request-{id}"),
                    created_at: "2026-09-25T00:00:00Z".into(),
                    noticed_turn: None,
                    reported_finish: None,
                    handback_tool: true,
                    supervisor: false,
                },
            )
            .unwrap();
        }
        let mut controller = Controller {
            config: mj_core::config::Config::default(),
            state: crate::database::load_state().unwrap(),
        };
        controller.config.subagents.max_concurrent = 2;

        // Two live children fill a cap of two, idle or not.
        let refusal = controller
            .ensure_subagent_slot_available("parent-1", None)
            .unwrap_err()
            .to_string();
        assert!(
            refusal.contains("already has 2 live sub-agents and the maximum is 2"),
            "{refusal}"
        );
        assert!(
            refusal.contains("aaaaaaaa \"Audit the lockfile\"")
                && refusal.contains("bbbbbbbb \"Run the suite\""),
            "the refusal names every live child: {refusal}"
        );
        assert!(
            !refusal.contains("Map the parser") && !refusal.contains("Old task"),
            "parked and stopped children hold no processes: {refusal}"
        );
        assert!(
            refusal.contains("hands back")
                && refusal.contains("parked")
                && refusal.contains("close it"),
            "the refusal says how a slot frees up: {refusal}"
        );
        // Starting the parked child again is refused the same way.
        assert!(
            controller
                .ensure_subagent_slot_available("parent-1", Some("cccccccc-park"))
                .is_err()
        );
        // Input to a live child starts nothing, so the cap never refuses it.
        controller
            .ensure_subagent_slot_available("parent-1", Some("aaaaaaaa-idle"))
            .unwrap();

        // Once one live child is parked, a spawn and a restart both fit.
        controller
            .state
            .sessions
            .get_mut("aaaaaaaa-idle")
            .unwrap()
            .state = SessionState::Parked;
        controller
            .ensure_subagent_slot_available("parent-1", None)
            .unwrap();
        controller
            .ensure_subagent_slot_available("parent-1", Some("cccccccc-park"))
            .unwrap();
    }

    #[test]
    fn supervisor_cap_and_depth_two_slot_accounting() {
        const MARKER: &str = "MJ_TEST_SUPERVISOR_SLOT_ACCOUNTING";
        if std::env::var_os(MARKER).is_none() {
            let directory = tempfile::tempdir().unwrap();
            super::super::test_support::IsolatedTest::new(super::super::test_support::test_name(
                module_path!(),
                "supervisor_cap_and_depth_two_slot_accounting",
            ))
            .env(MARKER, "1")
            .isolated_store(directory.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let mut parent = super::super::test_support::checkpoint_test_session("primary-1");
        parent.supervisors = true;
        crate::database::save_session(&parent).unwrap();

        let save_child = |id: &str, parent_id: &str, state: SessionState, supervisor: bool| {
            let mut child = super::super::test_support::checkpoint_test_session(id);
            child.state = state;
            child.session_title_override = Some(id.into());
            crate::database::save_subagent_session(
                &child,
                &SubagentRecord {
                    kind: mj_core::subagent::SubagentKind::Worker,
                    reviewer_launch: None,
                    child_session_id: id.into(),
                    parent_session_id: parent_id.into(),
                    task_name: id.into(),
                    profile_id: "codex".into(),
                    model: None,
                    effort: None,
                    working_directory: PathBuf::new(),
                    initial_prompt: "do it".into(),
                    request_key: format!("req-{id}"),
                    created_at: "2026-11-21T00:00:00Z".into(),
                    noticed_turn: None,
                    reported_finish: None,
                    handback_tool: true,
                    supervisor,
                },
            )
            .unwrap();
        };

        // Two live supervisors fill the supervisor cap (4 / 2 = 2).
        save_child("sup-1", "primary-1", SessionState::Running, true);
        save_child("sup-2", "primary-1", SessionState::Running, true);
        save_child("sup-3-parked", "primary-1", SessionState::Parked, true);
        // A live depth-2 worker under sup-1 consumes one total slot (3 of 4).
        save_child("d2-worker-1", "sup-1", SessionState::Running, false);

        let mut controller = Controller {
            config: mj_core::config::Config::default(),
            state: crate::database::load_state().unwrap(),
        };
        controller.config.subagents.max_concurrent = 4;

        // Spawning or unparking a 3rd live supervisor is refused (cap 4 / 2 = 2).
        let err = controller
            .ensure_subagent_slot_available_for("primary-1", None, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("maximum is 2"), "{err}");
        assert!(
            controller
                .ensure_subagent_slot_available_for("primary-1", Some("sup-3-parked"), true)
                .is_err()
        );

        // A 4th non-supervisor worker still fits (3 live < 4).
        controller
            .ensure_subagent_slot_available_for("sup-1", None, false)
            .unwrap();

        // Add a 4th live worker (depth-2 under sup-2) -> total live = 4 == max_concurrent.
        save_child("d2-worker-2", "sup-2", SessionState::Running, false);
        controller.state = crate::database::load_state().unwrap();
        assert!(
            controller
                .ensure_subagent_slot_available_for("sup-1", None, false)
                .is_err(),
            "depth-1 + depth-2 live workers together are capped by max_concurrent"
        );

        // Parking sup-1 frees its slot, while its live depth-2 worker STILL consumes a slot
        // (live count drops from 4 to 3: sup-2, d2-worker-1, d2-worker-2).
        controller.state.sessions.get_mut("sup-1").unwrap().state = SessionState::Parked;
        // Now 1 live supervisor < 2 and 3 total live < 4, so unparking sup-3 fits.
        controller
            .ensure_subagent_slot_available_for("primary-1", Some("sup-3-parked"), true)
            .unwrap();
    }

    #[test]
    fn register_subagent_enforces_supervisor_and_depth_two_rules() {
        const MARKER: &str = "MJ_TEST_REGISTER_SUPERVISOR_DEPTH_RULES";
        if std::env::var_os(MARKER).is_none() {
            let directory = tempfile::tempdir().unwrap();
            super::super::test_support::IsolatedTest::new(super::super::test_support::test_name(
                module_path!(),
                "register_subagent_enforces_supervisor_and_depth_two_rules",
            ))
            .env(MARKER, "1")
            .isolated_store(directory.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let workdir = tempfile::tempdir().unwrap();
        let mut config = mj_core::config::Config::default();
        config.subagents.max_concurrent = 4;
        config.profiles.insert(
            "codex".into(),
            mj_core::config::HarnessProfile::new(HarnessKind::Codex, workdir.path()),
        );

        config.targets.insert(
            "localhost".into(),
            mj_core::config::TargetTemplate::LocalBare,
        );
        let make_primary = |id: &str, supervisors: bool| {
            let mut s = super::super::test_support::checkpoint_test_session(id);
            s.state = SessionState::Running;
            s.target_template_id = "localhost".into();
            s.subagents = Some(mj_core::subagent::SubagentPolicy::Mjolnir {
                straightforward: None,
                subtle: None,
            });
            s.supervisors = supervisors;
            s.project_directory = Some(workdir.path().to_path_buf());
            s.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
                worker_root: PathBuf::from(format!("/workers/{id}")),
            }));
            crate::database::save_session(&s).unwrap();
        };
        make_primary("primary-enabled", true);
        make_primary("primary-disabled", false);

        let mut controller = Controller {
            config,
            state: crate::database::load_state().unwrap(),
        };

        let make_req = |parent: &str,
                        task: &str,
                        kind: mj_core::subagent::SubagentKind,
                        supervisor: bool| RegisterSubagentRequest {
            no_github_credentials: false,
            kind,
            supervisor,
            reviewer_launch: None,
            parent_session_id: parent.into(),
            task_name: task.into(),
            profile_id: "codex".into(),
            model: None,
            effort: None,
            fast_mode: false,
            working_directory: workdir.path().to_path_buf(),
            initial_prompt: "do work".into(),
            request_key: format!("req-{parent}-{task}"),
            report_root: None,
            category: if supervisor {
                "supervisor".into()
            } else {
                "straightforward".into()
            },
        };

        // Top-level with supervisors = false refuses supervisor: true.
        assert!(
            controller
                .register_subagent(make_req(
                    "primary-disabled",
                    "sup-denied",
                    mj_core::subagent::SubagentKind::Worker,
                    true,
                ))
                .is_err()
        );

        // Top-level with supervisors = true spawns depth-1 supervisor.
        let sup = controller
            .register_subagent(make_req(
                "primary-enabled",
                "sup-1",
                mj_core::subagent::SubagentKind::Worker,
                true,
            ))
            .expect("depth-1 supervisor should register");
        assert!(sup.supervisor);

        // Depth-1 supervisor spawns depth-2 non-supervisor worker.
        let d2 = controller
            .register_subagent(make_req(
                &sup.child_session_id,
                "d2-worker",
                mj_core::subagent::SubagentKind::Worker,
                false,
            ))
            .expect("depth-2 worker under supervisor should register");
        assert!(!d2.supervisor);

        let stored = crate::database::load_state().unwrap();
        assert!(stored.sessions[&sup.child_session_id].notifications_ignored);
        assert!(stored.sessions[&d2.child_session_id].notifications_ignored);
        assert!(!stored.sessions["primary-enabled"].notifications_ignored);

        // Depth-1 supervisor refuses depth-2 supervisor.
        assert!(
            controller
                .register_subagent(make_req(
                    &sup.child_session_id,
                    "d2-sup-denied",
                    mj_core::subagent::SubagentKind::Worker,
                    true,
                ))
                .is_err()
        );

        // Depth-1 supervisor refuses depth-2 oracle.
        assert!(
            controller
                .register_subagent(make_req(
                    &sup.child_session_id,
                    "d2-oracle-denied",
                    mj_core::subagent::SubagentKind::Oracle,
                    false,
                ))
                .is_err()
        );

        // Depth-2 worker refuses depth-3 worker.
        assert!(
            controller
                .register_subagent(make_req(
                    &d2.child_session_id,
                    "d3-denied",
                    mj_core::subagent::SubagentKind::Worker,
                    false,
                ))
                .is_err()
        );
    }
}
