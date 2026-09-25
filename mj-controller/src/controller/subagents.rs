//! Registration and ownership rules for child sessions on a parent's target.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use super::{Controller, now};
use mj_core::config::HarnessKind;
use mj_core::state::{SessionRecord, SessionState, new_session_id};
use mj_core::subagent::SubagentRecord;

#[derive(Debug, Clone)]
pub struct RegisterSubagentRequest {
    pub parent_session_id: String,
    pub task_name: String,
    pub profile_id: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Empty means the parent's working directory. An absolute path is used
    /// as-is; a relative path is resolved against the parent's working
    /// directory. The path is interpreted on the parent's target and must
    /// exist there; no other restriction applies.
    pub working_directory: PathBuf,
    pub initial_prompt: String,
    pub request_key: String,
    /// The absolute directory on the parent's target that holds its
    /// children's report directories, from [`Controller::prepare_subagent_report_root`].
    /// `None` registers a child without one.
    pub report_root: Option<String>,
}

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

impl Controller {
    /// Create the directory that holds a parent's children's report
    /// directories on the parent's target, and return its absolute path.
    ///
    /// It sits outside every repository: under the workspace root that a
    /// bundle session's repositories are checked out below, or, for a bare
    /// project whose workspace root is the user's own directory, inside the
    /// project under a path its `info/exclude` lists.
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
        let (root, exclude_in) = subagent_report_root(parent, &backend);
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

    /// Register a child without provisioning another target or checkout.
    pub fn register_subagent(
        &mut self,
        request: RegisterSubagentRequest,
    ) -> Result<SubagentRecord> {
        if let Some(existing) = crate::database::lookup_subagent_request(
            &request.parent_session_id,
            &request.request_key,
        )? {
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
        ensure!(
            matches!(
                parent.harness_kind,
                HarnessKind::Claude | HarnessKind::Codex
            ),
            "only Claude and Codex sessions can spawn sub-agents"
        );
        ensure_parent_may_delegate(&parent)?;
        ensure!(parent.state.is_active(), "parent session is not active");
        ensure!(parent.target.is_some(), "parent session has no live target");
        ensure!(
            crate::database::load_subagent(&parent.id)?.is_none(),
            "sub-agents cannot spawn other sub-agents"
        );
        ensure!(
            self.config
                .subagents
                .profile_is_eligible(&parent.last_profile, &request.profile_id),
            "profile {:?} is not eligible for sub-agent use",
            request.profile_id
        );
        let profile = self
            .config
            .enabled_profile(&request.profile_id)
            .with_context(|| {
                format!("sub-agent profile {:?} is unavailable", request.profile_id)
            })?;
        if profile.kind == HarnessKind::Muse {
            let multiple_roots = !parent.additional_mounts.is_empty()
                || (parent.project_directory.is_none()
                    && self
                        .config
                        .bundles
                        .get(&parent.bundle_id)
                        .is_some_and(|bundle| bundle.repositories.len() > 1));
            ensure!(
                !multiple_roots,
                "{} ACP supports one workspace root; this parent exposes multiple roots",
                profile.kind.display_name()
            );
        }
        let occupied = crate::database::list_subagents(&parent.id)?
            .into_iter()
            .filter(|child| {
                self.subagent_occupies_slot(&child.child_session_id)
                    .unwrap_or(true)
            })
            .count();
        ensure!(
            occupied < self.config.subagents.max_concurrent,
            "parent session already has the maximum {} active sub-agents",
            self.config.subagents.max_concurrent
        );

        let child_id = new_session_id()?;
        let target = borrowed_locator(
            parent.target.as_ref().expect("live target checked above"),
            &parent.id,
            &child_id,
        )?;
        let created_at = now();
        let session = SessionRecord {
            target_runtime: Some(parent.target_runtime_settings(&self.config)?.into_owned()),
            launch_base: None,
            launch_branch: None,
            publication: None,
            // A child shares its parent's container, so it shares the build
            // cache that container was created with.
            build_cache: parent.build_cache.clone(),
            // A child never receives the Mjolnir sub-agent tools, so it can
            // never spawn a grandchild.
            mjolnir_subagents: Some(false),
            create_managed_worktree: Some(false),
            archived: false,
            container_cpus: None,
            container_memory: None,
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
            target_template_id: parent.target_template_id.clone(),
            resource_allocation: parent.resource_allocation.clone(),
            additional_mounts: parent.additional_mounts.clone(),
            state: SessionState::Provisioning,
            target: Some(target),
            native_session_id: None,
            acp_session_title: None,
            session_title_override: Some(request.task_name.clone()),
            created_at: created_at.clone(),
            updated_at: created_at.clone(),
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        };
        let handback_tool = child_gets_handback_tool(profile.kind);
        let report_dir = request
            .report_root
            .as_deref()
            .map(|root| format!("{}/{child_id}", root.trim_end_matches('/')));
        // The first prompt names the tool only when the child will have it.
        let initial_prompt = match (handback_tool, &report_dir) {
            (true, Some(report_dir)) => format!(
                "{}\n\n{}",
                mj_core::subagent::handback_prompt_note(report_dir),
                request.initial_prompt
            ),
            _ => request.initial_prompt,
        };
        let relation = SubagentRecord {
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
            handback_tool,
        };
        crate::database::save_subagent_session(&session, &relation)?;
        if let Some(report_dir) = &report_dir {
            crate::database::record_subagent_report_dir(&child_id, report_dir)?;
        }
        self.state.sessions.insert(child_id, session);
        self.state
            .subagents
            .insert(relation.child_session_id.clone(), relation.clone());
        Ok(relation)
    }

    pub fn ensure_subagent_slot_available(
        &self,
        parent_session_id: &str,
        child_id: &str,
    ) -> Result<()> {
        let occupied = crate::database::list_subagents(parent_session_id)?
            .into_iter()
            .filter(|child| child.child_session_id != child_id)
            .filter(|child| {
                self.subagent_occupies_slot(&child.child_session_id)
                    .unwrap_or(true)
            })
            .count();
        ensure!(
            occupied < self.config.subagents.max_concurrent,
            "parent session already has the maximum {} active sub-agents",
            self.config.subagents.max_concurrent
        );
        Ok(())
    }

    fn subagent_occupies_slot(&self, child_id: &str) -> Result<bool> {
        let Some(session) = self.state.sessions.get(child_id) else {
            return Ok(false);
        };
        if matches!(
            session.state,
            SessionState::Provisioning | SessionState::Closing | SessionState::Checkpointing
        ) {
            return Ok(true);
        }
        if !session.state.is_active() {
            return Ok(false);
        }
        Ok(
            crate::database::load_materialized_session_summary(child_id)?.is_none_or(|summary| {
                !matches!(
                    summary.execution,
                    mj_core::state::MaterializedExecutionState::Idle
                )
            }),
        )
    }
}

/// Whether a sub-agent child has handed back its report for its parent's
/// newest task, from what the store holds; see
/// [`mj_core::subagent::has_handed_back`]. A session that is not a child, or
/// that has never finished a turn, has not.
pub fn subagent_has_handed_back(child_session_id: &str) -> Result<bool> {
    let Some(relation) = crate::database::load_subagent(child_session_id)? else {
        return Ok(false);
    };
    let Some((execution, active_turn, last_turn)) =
        crate::database::load_materialized_turn_outcome(child_session_id)?
    else {
        return Ok(false);
    };
    let report = crate::database::load_subagent_report(child_session_id)?;
    let working = active_turn.is_some()
        || !matches!(execution, mj_core::state::MaterializedExecutionState::Idle);
    Ok(mj_core::subagent::has_handed_back(
        relation.handback_tool,
        &report,
        working,
        last_turn.as_ref(),
        mj_core::clock::epoch_millis(),
    ))
}

/// What a parent's record keeps about a child its suspend stops: the child's
/// listed title, one line of its task, and whether it had handed back.
pub fn stopped_subagent(
    state: &mj_core::state::State,
    child_session_id: &str,
) -> Result<mj_core::subagent::StoppedSubagent> {
    let relation = state
        .subagents
        .get(child_session_id)
        .with_context(|| format!("unknown sub-agent session {child_session_id}"))?;
    let title = state
        .sessions
        .get(child_session_id)
        .map_or(relation.task_name.as_str(), SessionRecord::listed_title)
        .to_owned();
    let report_dir = crate::database::load_subagent_report(child_session_id)?.report_dir;
    Ok(mj_core::subagent::StoppedSubagent {
        child_session_id: child_session_id.to_owned(),
        title,
        task: mj_core::subagent::task_summary(&relation.initial_prompt, report_dir.as_deref()),
        handed_back: subagent_has_handed_back(child_session_id)?,
    })
}

fn sibling_path(path: &Path, parent_id: &str, child_id: &str) -> Result<PathBuf> {
    ensure!(
        path.ends_with(parent_id),
        "parent target path does not end in its session id"
    );
    Ok(path
        .parent()
        .context("parent target path has no parent")?
        .join(child_id))
}

fn borrowed_locator(
    target: &mj_core::state::TargetLocator,
    parent_id: &str,
    child_id: &str,
) -> Result<mj_core::state::TargetLocator> {
    use mj_core::state::TargetLocator;
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
        // and records the parent as the container's owner, so cleanup and
        // whole-target operations stay with the parent.
        TargetLocator::LocalPodman {
            container_id,
            workspace_storage,
            ..
        } => TargetLocator::LocalPodman {
            container_id: container_id.clone(),
            workspace_storage: workspace_storage.clone(),
            borrowed_from: Some(parent_id.to_owned()),
        },
        TargetLocator::LocalDocker { container_id, .. } => TargetLocator::LocalDocker {
            container_id: container_id.clone(),
            borrowed_from: Some(parent_id.to_owned()),
        },
        TargetLocator::AppleContainer { container_id, .. } => TargetLocator::AppleContainer {
            container_id: container_id.clone(),
            borrowed_from: Some(parent_id.to_owned()),
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
            borrowed_from: Some(parent_id.to_owned()),
        },
        TargetLocator::SshDocker {
            host, container_id, ..
        } => TargetLocator::SshDocker {
            host: host.clone(),
            container_id: container_id.clone(),
            borrowed_from: Some(parent_id.to_owned()),
        },
        // EC2 children keep the parent's locator unchanged, as they did before
        // container borrowing was recorded.
        other @ TargetLocator::AwsEc2 { .. } => other.clone(),
    })
}

/// Whether a child can be given the `handback` tool. Codex takes Mjolnir's MCP
/// servers over ACP; Claude reads them from its staged profile, which every
/// session has. Other harnesses keep reporting through their last message.
fn child_gets_handback_tool(harness: HarnessKind) -> bool {
    matches!(harness, HarnessKind::Codex | HarnessKind::Claude)
}

/// A parent may delegate to Mjolnir children only if its own stored choice
/// says so; `None` means native sub-agents, same as `Some(false)`. A parent
/// using its harness's native delegation never received the Mjolnir tools, so
/// a request from it is stale.
fn ensure_parent_may_delegate(parent: &SessionRecord) -> Result<()> {
    match parent.mjolnir_subagents {
        Some(true) => Ok(()),
        _ => bail!("this session uses native sub-agents"),
    }
}

/// Where a parent's children keep their report directories, before the target
/// resolves it, and the repository whose `info/exclude` must list it.
fn subagent_report_root(
    parent: &SessionRecord,
    backend: &mj_core::targets::TargetLocator,
) -> (String, Option<String>) {
    match &parent.project_directory {
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
        assert_eq!(
            subagent_report_root(&parent, &backend),
            ("/var/lib/hel/workers/parent-1/.mj-agents".to_owned(), None)
        );
        parent.project_directory = Some("/home/dev/project/".into());
        assert_eq!(
            subagent_report_root(&parent, &backend),
            (
                "/home/dev/project/.mj/agents".to_owned(),
                Some("/home/dev/project".to_owned())
            )
        );
    }

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
    }

    #[test]
    fn a_parent_using_native_delegation_cannot_spawn_mjolnir_children() {
        let parent = |choice| {
            let mut session = crate::controller::test_support::checkpoint_test_session("parent");
            session.mjolnir_subagents = choice;
            session
        };

        assert_eq!(
            ensure_parent_may_delegate(&parent(Some(false)))
                .unwrap_err()
                .to_string(),
            "this session uses native sub-agents"
        );
        assert_eq!(
            ensure_parent_may_delegate(&parent(None))
                .unwrap_err()
                .to_string(),
            "this session uses native sub-agents"
        );
        assert!(ensure_parent_may_delegate(&parent(Some(true))).is_ok());
    }

    #[test]
    fn borrowed_bare_locator_gets_a_private_worker_identity() {
        let locator = mj_core::state::TargetLocator::LocalBare {
            worker_root: PathBuf::from("/workers/parent"),
        };
        assert_eq!(
            borrowed_locator(&locator, "parent", "child").unwrap(),
            mj_core::state::TargetLocator::LocalBare {
                worker_root: PathBuf::from("/workers/child")
            }
        );
    }

    #[test]
    fn borrowed_ssh_locator_keeps_parent_workspace_with_private_worker_identity() {
        let locator = mj_core::state::TargetLocator::SshBare {
            host: "builder".into(),
            workspace: PathBuf::from(".local/share/hel/workspaces/parent-session"),
            worker_id: None,
        };
        assert_eq!(
            borrowed_locator(&locator, "parent-session", "child-session").unwrap(),
            mj_core::state::TargetLocator::SshBare {
                host: "builder".into(),
                workspace: PathBuf::from(".local/share/hel/workspaces/parent-session"),
                worker_id: Some("child-session".into()),
            }
        );
    }
}
