//! Shared project checks for new-session review. Call only in supervised background work.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use mj_core::config::is_bare_project_target;
use mj_core::local_git::LocalRemoteRepair;
use mj_core::remote_git::{default_branch, display_url, resolve_repository};
use mj_core::state::ManagedWorktreeOptions;

use super::Controller;
use crate::targets::CommandExecutor;

/// Repository endpoints sanitized for presentation in either control surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSessionRepository {
    pub id: String,
    pub fetch_url: String,
    pub default_branch: String,
    pub push_urls: Vec<String>,
}

/// A repair proposal is returned before any remote branch checks are attempted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSessionPreflight {
    pub project_directory: Option<PathBuf>,
    pub managed_worktree: ManagedWorktreeOptions,
    pub remote_repairs: Vec<LocalRemoteRepair>,
    pub remote_repositories: Vec<NewSessionRepository>,
    pub local_changes_excluded: bool,
}

impl Controller {
    /// Inspect the selected project without changing it. The caller owns the
    /// executor's cancellation and deadline, and any repair confirmation.
    pub fn preflight_new_session(
        &self,
        bundle_id: &str,
        target_id: &str,
        project_directory: Option<&Path>,
        executor: &impl CommandExecutor,
    ) -> Result<NewSessionPreflight> {
        ensure!(
            !executor.cancellation_requested(),
            "project preflight cancelled"
        );
        let target_is_bare = self
            .config
            .targets
            .get(target_id)
            .with_context(|| format!("unknown target template {target_id:?}"))
            .map(is_bare_project_target)?;
        if target_is_bare {
            let directory =
                project_directory.context("project directory is required for a bare target")?;
            let directory = self.resolve_project_directory(target_id, directory, executor)?;
            ensure!(
                !executor.cancellation_requested(),
                "project preflight cancelled"
            );
            let managed_worktree =
                self.managed_worktree_options(target_id, &directory, executor)?;
            return Ok(NewSessionPreflight {
                managed_worktree,
                project_directory: Some(directory),
                remote_repairs: Vec::new(),
                remote_repositories: Vec::new(),
                local_changes_excluded: false,
            });
        }
        if project_directory.is_some() {
            bail!("project directory is unsupported for this target");
        }

        let bundle = self
            .config
            .bundles
            .get(bundle_id)
            .context("unknown bundle")?;
        let repairs = mj_core::local_git::repository_remote_repairs(bundle, executor)?;
        if !repairs.is_empty() {
            return Ok(NewSessionPreflight {
                managed_worktree: Default::default(),
                project_directory: None,
                remote_repairs: repairs,
                remote_repositories: Vec::new(),
                local_changes_excluded: true,
            });
        }
        let remote_repositories = bundle
            .repositories
            .iter()
            .map(|repository| {
                ensure!(
                    !executor.cancellation_requested(),
                    "repository preflight cancelled"
                );
                let source = resolve_repository(repository, executor)
                    .with_context(|| format!("repository {:?}", repository.id))?;
                let default_branch = default_branch(&source, executor)
                    .with_context(|| format!("repository {:?}", repository.id))?;
                Ok(NewSessionRepository {
                    id: repository.id.clone(),
                    fetch_url: display_url(&source.fetch_url),
                    default_branch,
                    push_urls: source
                        .push_urls
                        .iter()
                        .map(|url| display_url(url))
                        .collect(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(NewSessionPreflight {
            managed_worktree: Default::default(),
            project_directory: None,
            remote_repairs: Vec::new(),
            remote_repositories,
            local_changes_excluded: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::controller::config_only_controller;
    use crate::controller::test_support::{
        committed_repository, local_bundle, resume_compatibility_config, test_git,
    };
    use crate::targets::{CommandOutput, CommandSpec, ProcessExecutor};
    use mj_core::config::{ProjectBundle, ProjectRepository};

    #[derive(Default)]
    struct RemoteExecutor {
        requests: Cell<usize>,
        cancelled: Cell<bool>,
        cancel_after_request: bool,
        fail: bool,
    }

    impl CommandExecutor for RemoteExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            if command.args.iter().any(|arg| arg == "ls-remote") {
                self.requests.set(self.requests.get() + 1);
                self.cancelled.set(self.cancel_after_request);
                if self.fail {
                    bail!("remote unavailable");
                }
                return Ok(CommandOutput {
                    status: 0,
                    stdout: b"ref: refs/heads/main\tHEAD\n0123456789abcdef0123456789abcdef01234567\tHEAD\n".to_vec(),
                    stderr: Vec::new(),
                });
            }
            ProcessExecutor.execute(command)
        }

        fn cancellation_requested(&self) -> bool {
            self.cancelled.get()
        }
    }

    fn remote_controller() -> Controller {
        let mut config = resume_compatibility_config();
        config.bundles.insert(
            "project".into(),
            ProjectBundle {
                primary_repo: "one".into(),
                repositories: ["one", "two"]
                    .into_iter()
                    .map(|id| ProjectRepository {
                        id: id.into(),
                        github: Some(format!("https://user:secret@example.com/{id}.git")),
                        local: None,
                        destination: id.into(),
                        git_ref: None,
                    })
                    .collect(),
            },
        );
        config_only_controller(config)
    }

    #[test]
    fn preflight_resolves_all_remote_branches_and_sanitizes_preview_urls() {
        let executor = RemoteExecutor::default();
        let result = remote_controller()
            .preflight_new_session("project", "podman", None, &executor)
            .unwrap();
        assert_eq!(executor.requests.get(), 2);
        assert!(result.local_changes_excluded);
        assert!(result.project_directory.is_none());
        assert!(result.remote_repairs.is_empty());
        for (repository, id) in result.remote_repositories.iter().zip(["one", "two"]) {
            assert_eq!(repository.id, id);
            assert_eq!(repository.default_branch, "main");
            assert_eq!(
                repository.fetch_url,
                format!("https://example.com/{id}.git")
            );
            assert_eq!(repository.push_urls, vec![repository.fetch_url.clone()]);
        }
    }

    #[test]
    fn preflight_reports_repository_failure_and_stops_remaining_checks() {
        let executor = RemoteExecutor {
            fail: true,
            ..Default::default()
        };
        let error = remote_controller()
            .preflight_new_session("project", "podman", None, &executor)
            .unwrap_err();
        assert!(format!("{error:#}").contains("repository \"one\""));
        assert!(format!("{error:#}").contains("remote unavailable"));
        assert_eq!(executor.requests.get(), 1);
    }

    #[test]
    fn preflight_cancellation_prevents_initial_and_remaining_remote_checks() {
        for already_cancelled in [true, false] {
            let executor = RemoteExecutor {
                cancelled: Cell::new(already_cancelled),
                cancel_after_request: true,
                ..Default::default()
            };
            let error = remote_controller()
                .preflight_new_session("project", "podman", None, &executor)
                .unwrap_err();
            assert!(format!("{error:#}").contains("cancelled"));
            assert_eq!(executor.requests.get(), usize::from(!already_cancelled));
        }
    }

    #[test]
    fn preflight_proposes_tracking_repairs_without_writing_or_contacting_remotes() {
        let repository = committed_repository();
        test_git(
            repository.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://user:secret@example.com/repo.git",
            ],
        );
        test_git(
            repository.path(),
            &["config", "branch.master.remote", "missing"],
        );
        let mut controller = remote_controller();
        controller
            .config
            .bundles
            .insert("project".into(), local_bundle(repository.path()));
        let executor = RemoteExecutor::default();
        let result = controller
            .preflight_new_session("project", "podman", None, &executor)
            .unwrap();
        assert_eq!(executor.requests.get(), 0);
        assert!(result.remote_repositories.is_empty());
        assert_eq!(result.remote_repairs.len(), 1);
        assert_eq!(result.remote_repairs[0].replacement_remote, "origin");
        assert_eq!(
            result.remote_repairs[0].fetch_url,
            "https://example.com/repo.git"
        );
        assert_eq!(
            test_git(repository.path(), &["config", "branch.master.remote"]),
            "missing"
        );
    }

    #[test]
    fn bare_preflight_resolves_directory_and_worktree_defaults_without_mutation() {
        let repository = committed_repository();
        let controller = config_only_controller(resume_compatibility_config());
        let root = repository.path().canonicalize().unwrap();
        let result = controller
            .preflight_new_session("", "local-bare", Some(&root), &ProcessExecutor)
            .unwrap();
        assert_eq!(result.project_directory.as_ref(), Some(&root));
        assert!(result.managed_worktree.available);
        assert!(result.managed_worktree.default_create);
        assert!(!result.local_changes_excluded);
        assert!(!root.join(".mj/worktrees").exists());

        let linked_parent = tempfile::tempdir().unwrap();
        let linked = linked_parent.path().join("linked");
        test_git(
            &root,
            &["worktree", "add", "-b", "side", linked.to_str().unwrap()],
        );
        let result = controller
            .preflight_new_session("", "local-bare", Some(&linked), &ProcessExecutor)
            .unwrap();
        assert!(result.managed_worktree.available);
        assert!(!result.managed_worktree.default_create);
    }

    #[test]
    fn bare_preflight_accepts_non_git_directories_but_rejects_missing_or_unborn_projects() {
        let directory = tempfile::tempdir().unwrap();
        let controller = config_only_controller(resume_compatibility_config());
        let result = controller
            .preflight_new_session("", "local-bare", Some(directory.path()), &ProcessExecutor)
            .unwrap();
        assert!(!result.managed_worktree.available);
        let missing = directory.path().join("missing");
        assert!(
            controller
                .preflight_new_session("", "local-bare", Some(&missing), &ProcessExecutor)
                .is_err()
        );
        test_git(directory.path(), &["init", "--initial-branch=master"]);
        assert!(
            controller
                .preflight_new_session("", "local-bare", Some(directory.path()), &ProcessExecutor)
                .is_err()
        );
    }

    #[test]
    fn preflight_rejects_a_project_selection_incompatible_with_its_target() {
        let controller = remote_controller();
        let executor = RemoteExecutor::default();
        assert!(
            controller
                .preflight_new_session("project", "podman", Some(Path::new("/project")), &executor)
                .is_err()
        );
        assert!(
            controller
                .preflight_new_session("project", "local-bare", None, &executor)
                .is_err()
        );
        assert_eq!(executor.requests.get(), 0);
    }
}
