//! Managed worktrees and raw-to-workspace project conversion.

use mj_core::targets::SshTrafficClass;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

#[cfg(any(test, feature = "test-support"))]
use mj_core::config::ProjectRepository;
use mj_core::config::{Config, TargetTemplate};
use mj_core::local_git::canonical_repository;
use mj_core::remote_git::{GITHUB_SSH_REWRITE_PREFIXES, Shorthand, uses_github_ssh_rewrite_prefix};
use mj_core::repository::{ProjectBundleSnapshot, RepositoryIdentity};
use mj_core::state::{
    ManagedCheckoutKind, ManagedWorktree, ManagedWorktreeOptions, ManagedWorktreeTarget,
    ProjectSourceIdentity, SessionRecord,
};
use mj_core::target_path;

use crate::targets::{
    self, CommandExecutor, CommandOutput, CommandSpec, SshTarget, managed_target_command,
    managed_target_ssh, path_exists_on_managed_target,
};
pub(crate) use mj_client::target::managed_worktree_target;
pub use mj_client::target::{ResumePlan, resume_compatibility, resume_compatibility_with_checkout};

use super::{BranchDisposition, Controller, execute_checked, now};

/// The persisted checkout intent; materialization may run alongside worker setup.
pub(super) struct PreparedManagedCheckout {
    worktree: ManagedWorktree,
    upstream: Option<String>,
}

impl PreparedManagedCheckout {
    pub fn execute(&self, executor: &impl CommandExecutor) -> Result<()> {
        create_managed_worktree(
            executor,
            &self.worktree,
            self.upstream.as_deref(),
            PrimaryCheckoutRequirement::Clean,
        )
    }
}

impl Controller {
    /// Inspect in a supervised worker, never on a UI event loop.
    pub fn managed_worktree_options(
        &self,
        target_id: &str,
        directory: &Path,
        executor: &impl CommandExecutor,
    ) -> Result<ManagedWorktreeOptions> {
        let template = self
            .config
            .targets
            .get(target_id)
            .with_context(|| format!("unknown target template {target_id:?}"))?;
        if !mj_core::config::is_bare_project_target(template) {
            return Ok(ManagedWorktreeOptions::default());
        }
        let target = managed_worktree_target(template)?;
        if matches!(target, ManagedWorktreeTarget::Local)
            && local_project_repository(directory, executor)?.is_none()
        {
            return Ok(ManagedWorktreeOptions::default());
        }
        let inspection = inspect_raw_project(executor, &target, directory)?;
        Ok(ManagedWorktreeOptions {
            available: true,
            default_create: inspection.primary_checkout,
        })
    }

    /// Resolve first so validation, review, and launch use the same path.
    pub fn resolve_project_directory(
        &self,
        target_id: &str,
        directory: &Path,
        executor: &impl CommandExecutor,
    ) -> Result<PathBuf> {
        self.resolve_and_classify_project_directory(target_id, directory, executor)
            .map(|(directory, _)| directory)
    }

    /// [`Self::resolve_project_directory`], also saying whether the directory
    /// is a plain folder without Git commits rather than a committed checkout.
    pub fn resolve_and_classify_project_directory(
        &self,
        target_id: &str,
        directory: &Path,
        executor: &impl CommandExecutor,
    ) -> Result<(PathBuf, bool)> {
        mj_core::path_input::validate_absolute_input(directory)?;
        let directory = self.resolve_input_path(target_id, directory, executor)?;
        let plain_directory = self.classify_project_directory(target_id, &directory, executor)?;
        Ok((directory, plain_directory))
    }

    /// Verify a bare project before leaving the project-directory dialog.
    pub fn validate_project_directory(
        &self,
        target_id: &str,
        directory: &Path,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        self.classify_project_directory(target_id, directory, executor)
            .map(|_| ())
    }

    /// Verify a bare project, and say whether it is a plain folder. Only a
    /// local target accepts one; a remote bare project must be a repository.
    fn classify_project_directory(
        &self,
        target_id: &str,
        directory: &Path,
        executor: &impl CommandExecutor,
    ) -> Result<bool> {
        let target = self
            .config
            .targets
            .get(target_id)
            .with_context(|| format!("unknown target template {target_id:?}"))?;
        match target {
            TargetTemplate::LocalBare => {
                ensure!(
                    directory.is_dir(),
                    "project directory does not exist or is not a directory"
                );
                // This also accepts a plain folder, a repository without its
                // first commit, and a host where Git is not installed.
                Ok(
                    mj_core::repository::resolve_local_directory(directory, executor)?
                        .plain_directory,
                )
            }
            TargetTemplate::SshBare { ssh, .. } => {
                targets::validate_bare_project_directory(
                    &SshTarget::from(ssh),
                    directory,
                    executor,
                )?;
                mj_core::remote_git::resolve_local_repository(
                    directory,
                    &targets::RemoteGitExecutor {
                        executor,
                        ssh: SshTarget::from(ssh),
                    },
                )?;
                Ok(false)
            }
            _ => bail!("project directory validation requires a bare target"),
        }
    }

    /// Resolves a session's canonical project without doing process work on a
    /// UI loop. Raw checkouts use their Git origin when available, then their
    /// canonical Git root or local directory.
    pub fn resolve_session_project_source(
        &self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<ProjectSourceIdentity> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        if session.project.is_some() {
            return Ok(session.project_source(&self.config));
        }
        let checkout = self.state.checkout(session_id)?;
        let Some(directory) = checkout.project_directory() else {
            return Ok(session.project_source(&self.config));
        };
        let (target, origin_directory) = match checkout.effective() {
            // The source repository is the durable owner of a linked
            // worktree's shared Git configuration and remains available while
            // a stopped session's checkout is retired.
            mj_core::state::Checkout::ManagedWorktree { worktree, .. } => (
                worktree.target.clone(),
                worktree.source_repository.as_path(),
            ),
            mj_core::state::Checkout::Attached { .. } => (
                managed_worktree_target(
                    self.config
                        .targets
                        .get(&session.target_template_id)
                        .with_context(|| {
                            format!(
                                "session {session_id} target {:?} is no longer configured",
                                session.target_template_id
                            )
                        })?,
                )?,
                directory,
            ),
            mj_core::state::Checkout::ManagedWorkspace => {
                return Ok(session.project_source(&self.config));
            }
            mj_core::state::Checkout::Borrowed { .. } => {
                unreachable!("effective checkout resolves borrowing")
            }
        };
        let output = executor.execute(&managed_git_command(
            SshTrafficClass::Control,
            &target,
            origin_directory,
            ["config", "--get", "remote.origin.url"],
            "resolve project Git origin",
        ))?;
        match output.status {
            0 => {
                let origin =
                    String::from_utf8(output.stdout).context("project Git origin was not UTF-8")?;
                if let Some(identity) =
                    ProjectSourceIdentity::git_remote(origin.trim(), Shorthand::Rejected)
                {
                    return Ok(identity);
                }
            }
            // Git uses 1 when no origin is configured.
            1 => {}
            status => bail!(
                "resolve project Git origin failed with status {status}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
        let root = resolve_git_root(&target, origin_directory, executor)?
            .unwrap_or_else(|| origin_directory.to_path_buf());
        let remote = match &target {
            ManagedWorktreeTarget::Local => None,
            ManagedWorktreeTarget::Ssh { destination, .. } => Some(destination.as_str()),
        };
        Ok(ProjectSourceIdentity::path(&root, remote))
    }

    /// Resolve the checkout a bundle session is moving into, and check that it
    /// is free, before the session record names it.
    pub(super) fn plan_workspace_to_raw(
        &self,
        session: &SessionRecord,
        target_id: &str,
        origin_delta: bool,
        executor: &impl CommandExecutor,
    ) -> Result<WorkspaceToRawConversion> {
        let bundle = session
            .project_bundle(&self.config)
            .context("session bundle is missing")?;
        let [repository] = bundle.repositories.as_slice() else {
            bail!("a checkout holds exactly one repository");
        };
        let source = repository
            .local
            .as_deref()
            .context("only a repository already on this machine can become a checkout")?;
        self.validate_project_directory(target_id, source, executor)
            .context("this session's repository is unavailable")?;
        let mut worktree = ManagedWorktree {
            kind: Default::default(),
            source_project_directory: source.to_path_buf(),
            source_repository: source.to_path_buf(),
            worktree_root: target_path::join(
                source,
                &Path::new(".mj").join("worktrees").join(&session.id),
            ),
            branch: format!("mj/{}", session.id),
            target: managed_worktree_target(
                self.config
                    .targets
                    .get(target_id)
                    .with_context(|| format!("unknown target template {target_id:?}"))?,
            )?,
            base_commit: None,
        };
        let reuse_existing_branch =
            !origin_delta && retained_managed_worktree_branch_available(executor, &worktree)?;
        if origin_delta {
            worktree.kind = ManagedCheckoutKind::Clone;
            worktree.worktree_root =
                target_path::join(source, &Path::new(".mj").join("clones").join(&session.id));
            worktree.branch = format!("mj/{}", session.id);
            worktree.base_commit = None;
        } else if !reuse_existing_branch {
            // The archived delta restored into this clone needs the session's
            // base, which the network remote served after the host's last fetch.
            refresh_source_origin(executor, &worktree.target, source)?;
            let (branch, remote_branch) = managed_clone_starting_branch(
                executor,
                &worktree.target,
                source,
                session.launch_branch.as_deref(),
            )?;
            worktree.kind = ManagedCheckoutKind::Clone;
            worktree.worktree_root =
                target_path::join(source, &Path::new(".mj").join("clones").join(&session.id));
            worktree.branch = branch.clone();
            worktree.base_commit = Some(managed_git_stdout(
                executor,
                &worktree.target,
                source,
                [
                    "rev-parse",
                    "--verify",
                    &format!(
                        "{}^{{commit}}",
                        if remote_branch {
                            format!("refs/remotes/origin/{branch}")
                        } else {
                            format!("refs/heads/{branch}")
                        }
                    ),
                ],
                "resolve converted checkout source commit",
            )?);
        }
        if !reuse_existing_branch {
            ensure_managed_worktree_available(executor, &worktree)?;
        }
        Ok(WorkspaceToRawConversion {
            worktree,
            reuse_existing_branch,
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn prepare_managed_raw_worktree(
        &mut self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<bool> {
        let checkout = self.plan_managed_raw_worktree(session_id, executor)?;
        if let Some(checkout) = &checkout {
            checkout.execute(executor)?;
        }
        Ok(checkout.is_some())
    }

    pub(super) fn plan_managed_raw_worktree(
        &mut self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<Option<PreparedManagedCheckout>> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        let checkout = self.state.checkout(session_id)?;
        let selected = match checkout {
            mj_core::state::Checkout::Attached { path } => Some(path.to_path_buf()),
            mj_core::state::Checkout::ManagedWorktree { .. }
            | mj_core::state::Checkout::ManagedWorkspace
            | mj_core::state::Checkout::Borrowed { .. } => None,
        };
        drop(checkout);
        let Some(selected) = selected else {
            return Ok(None);
        };
        if session.create_managed_worktree == Some(false) {
            return Ok(None);
        }
        let template = self
            .config
            .targets
            .get(&session.target_template_id)
            .context("raw session target template disappeared during provisioning")?;
        if matches!(template, TargetTemplate::SshBare { .. }) {
            self.validate_project_directory(&session.target_template_id, &selected, executor)?;
        }
        let target = managed_worktree_target(template)?;
        if matches!(target, ManagedWorktreeTarget::Local)
            && local_project_repository(&selected, executor)?.is_none()
        {
            // A requested launch base asks for the same worktree an explicit
            // request does, so it must fail here rather than launch without
            // one and silently ignore the base.
            ensure!(
                session.create_managed_worktree != Some(true) && session.launch_base.is_none(),
                "managed worktree creation requires a Git project"
            );
            return Ok(None);
        }
        let inspection = inspect_raw_project(executor, &target, &selected)?;
        if !inspection.primary_checkout
            && session.create_managed_worktree != Some(true)
            && session.launch_base.is_none()
        {
            return Ok(None);
        }
        let relative_directory = inspection
            .source_project_directory
            .strip_prefix(&inspection.source_repository)
            .context("raw project directory is outside its repository")?
            .to_path_buf();
        let worktree_root = target_path::join(
            &inspection.source_repository,
            &Path::new(".mj").join("clones").join(session_id),
        );
        refresh_source_origin(executor, &target, &inspection.source_repository)?;
        // The worktree branch is created from the repository's HEAD, or from
        // the requested launch base, so record that commit as the session base
        // rather than rediscovering it later.
        let (branch, remote_branch) = managed_clone_starting_branch(
            executor,
            &target,
            &inspection.source_repository,
            session.launch_branch.as_deref(),
        )?;
        let base_commit = match session.launch_base.as_deref() {
            Some(revision) => managed_git_stdout(
                executor,
                &target,
                &inspection.source_repository,
                [
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    &format!("{revision}^{{commit}}"),
                ],
                "resolve the launch base",
            )?
            .trim()
            .to_owned(),
            None => managed_git_stdout(
                executor,
                &target,
                &inspection.source_repository,
                [
                    "rev-parse",
                    "--verify",
                    &format!(
                        "{}^{{commit}}",
                        if remote_branch {
                            format!("refs/remotes/origin/{branch}")
                        } else {
                            format!("refs/heads/{branch}")
                        }
                    ),
                ],
                "resolve selected branch tip",
            )?,
        };
        let managed = ManagedWorktree {
            kind: ManagedCheckoutKind::Clone,
            source_project_directory: inspection.source_project_directory,
            source_repository: inspection.source_repository,
            worktree_root: worktree_root.clone(),
            branch,
            target,
            base_commit: Some(base_commit),
        };
        ensure_managed_worktree_available(executor, &managed)?;
        let record = self.state.sessions.get_mut(session_id).unwrap();
        record.project_directory = Some(target_path::join(&worktree_root, &relative_directory));
        record.managed_worktree = Some(Box::new(managed.clone()));
        record.updated_at = now();
        self.persist_session_state(session_id)?;
        Ok(Some(PreparedManagedCheckout {
            worktree: managed,
            upstream: inspection.upstream,
        }))
    }

    pub(super) fn cleanup_new_session_worktree_after_failure(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        if !self.state.sessions.contains_key(session_id) {
            return Ok(());
        }
        let mj_core::state::Checkout::ManagedWorktree { worktree, .. } =
            self.state.checkout(session_id)?
        else {
            return Ok(());
        };
        // A session that never started has a branch Mjolnir just created and
        // nobody has worked on, so the rollback takes the branch too.
        let _ = worktree;
        super::environment::retire_session_environment(
            &self.state.sessions[session_id],
            &self.config,
            executor,
            BranchDisposition::Delete,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawProjectInspection {
    source_project_directory: PathBuf,
    source_repository: PathBuf,
    primary_checkout: bool,
    upstream: Option<String>,
}

fn managed_clone_starting_branch(
    executor: &impl CommandExecutor,
    target: &ManagedWorktreeTarget,
    repository: &Path,
    selected: Option<&str>,
) -> Result<(String, bool)> {
    if let Some(branch) = selected {
        let format = executor.execute(&managed_git_command(
            SshTrafficClass::Control,
            target,
            repository,
            ["check-ref-format", "--branch", branch],
            "validate selected branch",
        ))?;
        ensure!(format.status == 0, "invalid selected Git branch {branch:?}");
        for (reference, remote) in [
            (format!("refs/heads/{branch}"), false),
            (format!("refs/remotes/origin/{branch}"), true),
        ] {
            let present = executor.execute(&managed_git_command(
                SshTrafficClass::Control,
                target,
                repository,
                ["show-ref", "--verify", "--quiet", &reference],
                "find selected branch",
            ))?;
            match present.status {
                0 => return Ok((branch.to_owned(), remote)),
                1 => {}
                status => bail!("find selected branch failed with status {status}"),
            }
        }
        bail!("selected branch {branch:?} is unavailable in the source repository");
    }
    let remote_head = managed_git_command(
        SshTrafficClass::Control,
        target,
        repository,
        [
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
        "resolve origin default branch",
    );
    let output = executor.execute(&remote_head)?;
    match output.status {
        0 => {
            let reference = String::from_utf8(output.stdout)?;
            let branch = reference
                .trim()
                .strip_prefix("origin/")
                .context("origin/HEAD does not name an origin branch")?;
            ensure!(!branch.is_empty(), "origin/HEAD has no branch");
            Ok((branch.to_owned(), true))
        }
        1 => {
            let origin = executor.execute(&managed_git_command(
                SshTrafficClass::Control,
                target,
                repository,
                ["config", "--get", "remote.origin.url"],
                "inspect origin remote",
            ))?;
            if origin.status == 0 {
                let remote = origin_network_git_stdout(
                    SshTrafficClass::Control,
                    executor,
                    target,
                    repository,
                    String::from_utf8_lossy(&origin.stdout).trim(),
                    &["ls-remote", "--symref", "origin", "HEAD"],
                    "resolve remote default branch",
                )?;
                let branch = remote
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("ref: refs/heads/")?
                            .strip_suffix("\tHEAD")
                    })
                    .context("origin did not advertise a default branch")?;
                let cached = executor.execute(&managed_git_command(
                    SshTrafficClass::Control,
                    target,
                    repository,
                    [
                        "show-ref",
                        "--verify",
                        "--quiet",
                        &format!("refs/remotes/origin/{branch}"),
                    ],
                    "find remote default branch in source",
                ))?;
                ensure!(
                    cached.status == 0,
                    "origin default branch {branch:?} is not in the source repository; fetch it before starting a session"
                );
                return Ok((branch.to_owned(), true));
            }
            ensure!(
                origin.status == 1,
                "inspect origin remote failed with status {}",
                origin.status
            );
            managed_git_stdout(
                executor,
                target,
                repository,
                ["symbolic-ref", "--quiet", "--short", "HEAD"],
                "resolve source checkout branch",
            )
            .map(|branch| (branch, false))
            .context("source checkout is detached; select a starting branch explicitly")
        }
        status => bail!(
            "resolve origin default branch failed with status {status}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// Bring the source's cached `origin` branches up to date before a new clone
/// takes its starting commit from them, as a person fetches before branching.
/// The fetch lands in the source repository rather than the clone because
/// clone recovery requires the session's base commit there.
fn refresh_source_origin(
    executor: &impl CommandExecutor,
    target: &ManagedWorktreeTarget,
    repository: &Path,
) -> Result<()> {
    let origin = executor.execute(&managed_git_command(
        SshTrafficClass::Control,
        target,
        repository,
        ["config", "--get", "remote.origin.url"],
        "inspect origin remote",
    ))?;
    match origin.status {
        0 => {}
        // Without origin the clone starts from the source's own branch.
        1 => return Ok(()),
        status => bail!(
            "inspect origin remote failed with status {status}: {}",
            String::from_utf8_lossy(&origin.stderr).trim()
        ),
    }
    // Sessions provision concurrently, and two fetches into one repository
    // race on its ref locks. One fetch at a time per repository.
    let lock = source_fetch_lock(target, repository);
    let _fetching = loop {
        match lock.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                ensure!(
                    !executor.cancellation_requested(),
                    "operation cancelled while waiting to fetch origin"
                );
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    };
    let purpose = "fetch origin";
    origin_network_git_stdout(
        SshTrafficClass::Bulk,
        executor,
        target,
        repository,
        String::from_utf8_lossy(&origin.stdout).trim(),
        &[
            "fetch",
            "--no-write-fetch-head",
            "--no-tags",
            "--no-recurse-submodules",
            "origin",
        ],
        purpose,
    )
    .with_context(|| {
        format!(
            "could not update origin in {} to find the latest starting commit",
            repository.display()
        )
    })?;
    Ok(())
}

/// Run a network Git command against `origin`. A host can hold an HTTPS
/// credential for GitHub, such as a GitHub CLI login, without an SSH key;
/// sessions there push over HTTPS. So when a GitHub SSH origin fails, the
/// command runs once more over HTTPS with the host's own credential helpers.
fn origin_network_git_stdout(
    class: SshTrafficClass,
    executor: &impl CommandExecutor,
    target: &ManagedWorktreeTarget,
    repository: &Path,
    origin_url: &str,
    args: &[&str],
    purpose: &str,
) -> Result<String> {
    let result = command_stdout(
        executor.execute(&managed_network_git_command(
            class, target, repository, args, purpose,
        ))?,
        purpose,
    );
    let Err(ssh_error) = result else {
        return result;
    };
    if !uses_github_ssh_rewrite_prefix(origin_url) {
        return Err(ssh_error);
    }
    let mut https_args = Vec::new();
    for prefix in GITHUB_SSH_REWRITE_PREFIXES {
        https_args.extend([
            "-c".to_owned(),
            format!("url.https://github.com/.insteadOf={prefix}"),
        ]);
    }
    https_args.extend(args.iter().map(|arg| (*arg).to_owned()));
    let purpose = format!("{purpose} over HTTPS");
    command_stdout(
        executor.execute(&managed_network_git_command(
            class,
            target,
            repository,
            https_args,
            purpose.as_str(),
        ))?,
        &purpose,
    )
    .map_err(|https_error| anyhow::anyhow!("{ssh_error:#}; {https_error:#}"))
}

fn source_fetch_lock(
    target: &ManagedWorktreeTarget,
    repository: &Path,
) -> std::sync::Arc<std::sync::Mutex<()>> {
    use std::sync::{Arc, Mutex, OnceLock, Weak};
    static LOCKS: OnceLock<Mutex<std::collections::BTreeMap<String, Weak<Mutex<()>>>>> =
        OnceLock::new();
    let key = match target {
        ManagedWorktreeTarget::Local => format!("local|{}", repository.display()),
        ManagedWorktreeTarget::Ssh { destination, .. } => {
            format!("ssh|{destination}|{}", repository.display())
        }
    };
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    let slot = locks.entry(key).or_default();
    if let Some(lock) = slot.upgrade() {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    *slot = Arc::downgrade(&lock);
    lock
}

/// A Git command that contacts the remote, which must fail rather than wait
/// for credentials no one is present to give. Over SSH the far side has no
/// terminal, so Git cannot prompt there.
pub(super) fn managed_network_git_command(
    class: SshTrafficClass,
    target: &ManagedWorktreeTarget,
    directory: &Path,
    args: impl IntoIterator<Item = impl AsRef<str>>,
    purpose: impl Into<String>,
) -> CommandSpec {
    let mut command = managed_git_command(class, target, directory, args, purpose);
    if matches!(target, ManagedWorktreeTarget::Local) {
        mj_core::local_git::unattended_git_environment(&mut command);
    }
    command
}

pub(super) fn managed_git_command(
    class: SshTrafficClass,
    target: &ManagedWorktreeTarget,
    directory: &Path,
    args: impl IntoIterator<Item = impl AsRef<str>>,
    purpose: impl Into<String>,
) -> CommandSpec {
    let mut command_args = vec!["-C".to_owned(), directory.to_string_lossy().into_owned()];
    command_args.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
    managed_target_command(class, target, "git", command_args).purpose(purpose)
}

fn command_stdout(output: CommandOutput, purpose: &str) -> Result<String> {
    if output.status != 0 {
        bail!(
            "{purpose} failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("{purpose} produced non-UTF-8 output"))?;
    Ok(stdout.trim_end_matches(['\r', '\n']).to_owned())
}

fn managed_git_stdout(
    executor: &impl CommandExecutor,
    target: &ManagedWorktreeTarget,
    directory: &Path,
    args: impl IntoIterator<Item = impl AsRef<str>>,
    purpose: &str,
) -> Result<String> {
    let command = managed_git_command(SshTrafficClass::Control, target, directory, args, purpose);
    command_stdout(executor.execute(&command)?, purpose)
}

/// An absolute path Git printed on `target`: canonical on this machine, so it
/// compares equal to every other local repository path, and kept as POSIX
/// text on a remote one.
fn git_output_path(target: &ManagedWorktreeTarget, path: PathBuf) -> Result<PathBuf> {
    match target {
        ManagedWorktreeTarget::Local => mj_core::local_git::canonical_git_output_path(&path),
        ManagedWorktreeTarget::Ssh { .. } => Ok(path),
    }
}

/// Resolve a checkout's stable repository root, collapsing linked worktrees
/// onto the main worktree when Git exposes the shared `.git` directory.
fn resolve_git_root(
    target: &ManagedWorktreeTarget,
    directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<Option<PathBuf>> {
    // The expected non-repository diagnostic must be stable across locales;
    // every other Git failure remains an error.
    let args = [
        "-C".to_owned(),
        directory.to_string_lossy().into_owned(),
        "rev-parse".into(),
        "--show-toplevel".into(),
    ];
    let top_level = match target {
        ManagedWorktreeTarget::Local => {
            let mut command = CommandSpec::new("git", args);
            command.env.insert("LC_ALL".into(), "C".into());
            command
        }
        ManagedWorktreeTarget::Ssh { .. } => managed_target_command(
            SshTrafficClass::Control,
            target,
            "env",
            ["LC_ALL=C".to_owned(), "git".into()]
                .into_iter()
                .chain(args),
        ),
    }
    .purpose("resolve project Git root");
    let output = executor.execute(&top_level)?;
    if output.status != 0 {
        if output.status == 128
            && String::from_utf8_lossy(&output.stderr).starts_with("fatal: not a git repository")
        {
            return Ok(None);
        }
        bail!(
            "resolve project Git root failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let root = PathBuf::from(
        String::from_utf8(output.stdout)
            .context("project Git root was not UTF-8")?
            .trim_end_matches(['\r', '\n']),
    );
    // A local project's root is a host path; a remote one is POSIX.
    if !mj_core::target_path::is_absolute_on_host_or_target(&root) {
        bail!(
            "resolve project Git root returned a non-absolute path: {}",
            root.display()
        );
    }

    let common = mj_core::local_git::resolve_git_path(
        &root,
        &managed_git_stdout(
            executor,
            target,
            directory,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
            "resolve project Git common directory",
        )?,
    )?;
    let repository = match common.parent() {
        Some(main_root) if common.file_name() == Some(std::ffi::OsStr::new(".git")) => {
            main_root.to_path_buf()
        }
        _ => root,
    };
    git_output_path(target, repository).map(Some)
}

/// Inspect a local launch directory using the same Git error handling and
/// linked-worktree identity as existing sessions.
pub fn local_project_repository(
    directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<Option<PathBuf>> {
    let resolved = mj_core::repository::resolve_local_directory(directory, executor)?;
    Ok((!resolved.plain_directory).then_some(resolved.repository_root))
}

/// Which checkout each still-empty target repository is seeded from, or `None`
/// when this connect must not seed at all. A converting resume carries the
/// session's own checkout; every other seed comes from the bundle's local path.
/// Reshape a raw session's record for the workspace target it is moving into.
pub(super) fn apply_raw_to_workspace(
    record: &mut SessionRecord,
    conversion: &RawToWorkspaceConversion,
) -> Result<()> {
    let project = record
        .project
        .get_or_insert_with(|| conversion.project.clone().into());
    ensure!(
        project.bundle == conversion.project.bundle
            && project.identities == conversion.project.identities,
        "the accepted project changed while preparing the checkout transition"
    );
    std::sync::Arc::make_mut(project)
        .network_sources
        .insert(conversion.repository_id.clone(), conversion.source.clone());
    record.project_directory = None;
    record.managed_worktree = None;
    Ok(())
}

/// A resume that changes how a session is represented, resolved before the
/// session record or the configuration changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ResumeConversion {
    RawToWorkspace(Box<RawToWorkspaceConversion>),
    WorkspaceToRaw(WorkspaceToRawConversion),
}

impl ResumeConversion {
    pub(super) fn raw_to_workspace(&self) -> Option<&RawToWorkspaceConversion> {
        match self {
            Self::RawToWorkspace(conversion) => Some(conversion),
            Self::WorkspaceToRaw(_) => None,
        }
    }

    pub(super) fn workspace_to_raw(&self) -> Option<&WorkspaceToRawConversion> {
        match self {
            Self::WorkspaceToRaw(conversion) => Some(conversion),
            Self::RawToWorkspace(_) => None,
        }
    }
}

/// Everything a workspace-to-raw resume needs. The worktree does not exist yet:
/// the record names it first, so a failure cleans it up through the same path
/// as a new raw session's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkspaceToRawConversion {
    pub(super) worktree: ManagedWorktree,
    /// The first move retires this session's checkout but deliberately keeps
    /// its `mj/<session>` branch for source recovery. Reattach that branch on
    /// the return move instead of trying to create it a second time.
    pub(super) reuse_existing_branch: bool,
}

/// Reshape a bundle session's record for the checkout it is moving into. The
/// bundle stays: it still describes the repository the checkout came from.
pub(super) fn apply_workspace_to_raw(
    record: &mut SessionRecord,
    conversion: &WorkspaceToRawConversion,
) {
    record.project_directory = Some(conversion.worktree.worktree_root.clone());
    record.managed_worktree = Some(Box::new(conversion.worktree.clone()));
}

/// Everything a raw-to-workspace transition needs before the session record
/// changes. The accepted bundle remains the session's bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RawToWorkspaceConversion {
    /// The checkout whose branch, head commit, and dirty state move into the
    /// target. For a managed session this is the session's own worktree, not
    /// the user's primary checkout.
    pub(super) checkout: PathBuf,
    /// The repository in the accepted bundle that corresponds to this checkout.
    pub(super) repository_id: String,
    /// The accepted repository's target layout.
    pub(super) destination: PathBuf,
    /// The accepted bundle snapshot, retained if the older record had only a
    /// configuration-backed definition.
    pub(super) project: ProjectBundleSnapshot,
    /// The network source the workspace target will use. Configured GitHub
    /// sources remain authoritative; local-source bundles use the live
    /// checkout's resolved remote.
    pub(super) source: mj_core::remote_git::NetworkGitSource,
    /// Removed once the target holds the checkout, and only then.
    pub(super) retire: Option<ManagedWorktree>,
}

/// Plan a raw-to-workspace transition from an `OriginDelta` archive without
/// touching the (possibly retired or deleted) local checkout on disk.
pub(super) fn plan_raw_to_workspace_from_origin_delta(
    session: &SessionRecord,
    archive_manifest: &mj_checkpoint::archive::ArchiveManifest,
    config: &Config,
    executor: &impl CommandExecutor,
) -> Result<RawToWorkspaceConversion> {
    let [archived] = archive_manifest.repositories.as_slice() else {
        bail!("a raw checkout can only transition with a single-repository archive");
    };
    let metadata = &archived.metadata;
    let delta = metadata
        .origin_delta
        .as_deref()
        .context("origin delta metadata is missing")?;
    ensure!(
        !delta.history_cutoff,
        "a history-cutoff origin-delta archive cannot convert from a raw checkout"
    );
    ensure!(
        !metadata.origin.is_empty(),
        "This raw session has no Git origin remote and can only be resumed on its original target."
    );
    ensure!(
        metadata.checkout_subdirectory.is_none(),
        "only a whole checkout can move into a target"
    );
    mj_core::remote_git::validate_network_url(&metadata.origin).with_context(|| {
        format!(
            "archived origin {:?} is not a network Git URL that an isolated target can clone; resume this session on its original target",
            metadata.origin
        )
    })?;
    let project = accepted_project_snapshot(session, config, executor)?;
    ensure!(
        !project.primary_is_plain_directory(),
        "a directory project without a committed Git checkout cannot move into a target"
    );
    ensure!(
        project.bundle.repositories.len() == 1,
        "a raw checkout can only transition with a single-repository bundle"
    );
    project.key()?;
    let repository = project
        .bundle
        .repositories
        .iter()
        .find(|repository| repository.id == project.bundle.primary_repo)
        .context("accepted project primary repository is missing")?;
    let archive_source = mj_core::remote_git::NetworkGitSource {
        fetch_url: metadata.origin.clone(),
        push_urls: metadata.push_urls.clone(),
    };
    let source = if repository.github.is_some() {
        let configured = mj_core::remote_git::resolve_repository(repository, executor)?;
        let configured_id = RepositoryIdentity::from_remote(&configured.fetch_url);
        let archive_id = RepositoryIdentity::from_remote(&archive_source.fetch_url);
        if configured_id.is_some() && configured_id == archive_id {
            configured
        } else {
            archive_source
        }
    } else {
        archive_source
    };
    let retire = match session.managed_worktree.as_ref() {
        Some(worktree)
            if path_exists_on_managed_target(
                executor,
                &worktree.target,
                &worktree.worktree_root,
            )? =>
        {
            Some((**worktree).clone())
        }
        _ => None,
    };
    let checkout = session
        .checkout()
        .project_directory()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| metadata.relative_destination.clone());
    Ok(RawToWorkspaceConversion {
        checkout,
        repository_id: repository.id.clone(),
        destination: repository.destination.clone(),
        project,
        source,
        retire,
    })
}

/// Resolve a raw session's accepted project and plan its checkout transition.
/// Reads Git; changes neither the record nor configuration.
pub(super) fn plan_raw_to_workspace_for_session(
    session: &SessionRecord,
    checkout: &mj_core::state::Checkout<'_>,
    config: &Config,
    executor: &impl CommandExecutor,
) -> Result<RawToWorkspaceConversion> {
    let project = accepted_project_snapshot(session, config, executor)?;
    plan_raw_to_workspace_with_checkout(checkout, &project, executor)
}

fn accepted_project_snapshot(
    session: &SessionRecord,
    config: &Config,
    executor: &impl CommandExecutor,
) -> Result<ProjectBundleSnapshot> {
    if let Some(project) = &session.project {
        return Ok((**project).clone());
    }
    let bundle = session
        .project_bundle(config)
        .context("raw session has no accepted project bundle")?;
    crate::project_catalog::snapshot(bundle, executor, false)
}

/// Resolve where a raw session's checkout lives and which accepted bundle
/// repository it represents. Reads Git; changes nothing.
#[cfg(any(test, feature = "test-support"))]
pub(super) fn plan_raw_to_workspace(
    session: &SessionRecord,
    executor: &impl CommandExecutor,
) -> Result<RawToWorkspaceConversion> {
    let checkout = session.checkout();
    let project = if let Some(project) = &session.project {
        (**project).clone()
    } else {
        let project_directory = checkout
            .project_directory()
            .context("a raw session has no project directory")?;
        let repository = match &checkout {
            mj_core::state::Checkout::ManagedWorktree { worktree, .. } => {
                worktree.source_repository.clone()
            }
            mj_core::state::Checkout::Attached { .. }
            | mj_core::state::Checkout::Borrowed { .. } => {
                inspect_raw_project(executor, &ManagedWorktreeTarget::Local, project_directory)?
                    .source_repository
            }
            mj_core::state::Checkout::ManagedWorkspace => {
                bail!("a bundle workspace cannot be converted as a raw checkout")
            }
        };
        let repository = canonical_repository(&repository)?;
        let destination = PathBuf::from(
            project_directory
                .file_name()
                .context("a raw project directory cannot be the filesystem root")?,
        );
        crate::project_catalog::snapshot(
            &mj_core::config::ProjectBundle {
                primary_repo: "project".to_owned(),
                repositories: vec![ProjectRepository {
                    id: "project".to_owned(),
                    github: None,
                    local: Some(repository),
                    destination,
                    git_ref: None,
                    mbx_namespace: None,
                }],
            },
            executor,
            false,
        )?
    };
    plan_raw_to_workspace_with_checkout(&checkout, &project, executor)
}

pub(super) fn plan_raw_to_workspace_with_checkout(
    checkout: &mj_core::state::Checkout<'_>,
    project: &ProjectBundleSnapshot,
    executor: &impl CommandExecutor,
) -> Result<RawToWorkspaceConversion> {
    let project_directory = checkout
        .project_directory()
        .context("a raw session has no project directory")?;
    // A plain folder has no repository to clone from and no commits to carry,
    // so it cannot become a bundle workspace. Say so instead of letting Git
    // answer for a checkout that does not exist.
    ensure!(
        !project.primary_is_plain_directory(),
        "{} is a directory project without a committed Git checkout, so it cannot move into a target",
        project_directory.display()
    );
    // The checkpoint describes the session's directory as if it were the
    // repository root, so only a whole checkout can move. Each branch checks
    // this against paths from one domain: the record's own paths for a managed
    // worktree, Git's canonical paths for an inspected checkout — the record
    // may reach the same checkout through a symlink (macOS temp directories).
    let (checkout, source_repository, retire) = match checkout {
        mj_core::state::Checkout::ManagedWorktree { worktree, .. } => {
            ensure!(
                worktree.worktree_root == project_directory,
                "{} is a subdirectory of its checkout; only a whole checkout can move into a target",
                project_directory.display()
            );
            (
                worktree.worktree_root.clone(),
                canonical_repository(&worktree.source_repository)?,
                Some((**worktree).clone()),
            )
        }
        mj_core::state::Checkout::Attached { .. } | mj_core::state::Checkout::Borrowed { .. } => {
            let inspection =
                inspect_raw_project(executor, &ManagedWorktreeTarget::Local, project_directory)?;
            ensure!(
                inspection.source_project_directory == inspection.source_repository,
                "{} is a subdirectory of its checkout; only a whole checkout can move into a target",
                project_directory.display()
            );
            let source_repository = canonical_repository(&inspection.source_repository)?;
            (inspection.source_project_directory, source_repository, None)
        }
        mj_core::state::Checkout::ManagedWorkspace => {
            bail!("a bundle workspace cannot be converted as a raw checkout")
        }
    };
    ensure!(
        project.bundle.repositories.len() == 1,
        "a raw checkout can only transition with a single-repository bundle"
    );
    project.key()?;
    let repository = project
        .bundle
        .repositories
        .iter()
        .find(|repository| repository.id == project.bundle.primary_repo)
        .context("accepted project primary repository is missing")?;
    let source = if repository.github.is_some() {
        mj_core::remote_git::resolve_repository(repository, executor)?
    } else {
        mj_core::remote_git::resolve_local_repository(&checkout, executor).with_context(|| {
            format!(
                "bundle repository {:?} has no network source for {}: add one (for example `git remote add origin <url>`) or resume this session on a bare target",
                repository.id,
                checkout.display()
            )
        })?
    };
    let source_identity = RepositoryIdentity::from_remote(&source.fetch_url)
        .context("resolved checkout source has no repository identity")?;
    let accepted_identity = project
        .identities
        .get(&repository.id)
        .context("accepted project repository identity is missing")?;
    ensure!(
        accepted_identity == &source_identity
            || matches!(accepted_identity, RepositoryIdentity::Local(path) if path == &source_repository),
        "resolved checkout source does not match repository {:?} in the accepted bundle",
        repository.id
    );
    Ok(RawToWorkspaceConversion {
        checkout,
        repository_id: repository.id.clone(),
        destination: repository.destination.clone(),
        project: project.clone(),
        source,
        retire,
    })
}

pub(super) struct LegacyRawConversionSnapshot {
    pub(super) repository: mj_checkpoint::archive::RepositorySnapshot,
    pub(super) untracked_paths: Vec<PathBuf>,
    _pack_dir: tempfile::TempDir,
}

/// Capture an untouched legacy raw checkout using Milestone 1's
/// `capture_workspace_snapshot` (`CheckoutOwnership::Raw { launch: None }`,
/// `UntrackedConsent::Any`). Returns the origin-delta `RepositorySnapshot`
/// (whose pack file lives in `_pack_dir`) and the list of untracked paths that
/// remain in the untouched raw checkout for the resume notice.
pub(super) fn capture_legacy_raw_checkout_snapshot(
    checkout: &Path,
    repository_id: &str,
    source: &mj_core::remote_git::NetworkGitSource,
    destination: &Path,
    git: &dyn mj_checkpoint::archive::GitCommandRunner,
) -> Result<LegacyRawConversionSnapshot> {
    use mj_checkpoint::snapshot::{
        CaptureConsent, CheckoutOwnership, ExclusionSource, OperationConsent, RepositoryInput,
        SessionCaptureConsent, SnapshotHead, UntrackedConsent, capture_session_snapshot,
        session_inventory,
    };

    let pack_dir = tempfile::tempdir().context("create temporary directory for raw snapshot")?;
    let pack_path = pack_dir.path().join(format!("{repository_id}.pack"));
    let repo_input = RepositoryInput {
        id: repository_id.to_owned(),
        checkout: checkout.to_path_buf(),
        ownership: CheckoutOwnership::Raw { launch: None },
        origin_override: Some(source.fetch_url.clone()),
        pack_path,
        exclusions: ExclusionSource::Remote(source.fetch_url.clone()),
        keep_dir: None,
    };

    let inventory = session_inventory(git, std::slice::from_ref(&repo_input))
        .with_context(|| format!("inspect untracked files in {}", checkout.display()))?;
    let mut untracked_paths = inventory
        .get(repository_id)
        .map(|repo| {
            repo.untracked
                .iter()
                .map(|item| item.path.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    untracked_paths.sort();

    let mut consent = SessionCaptureConsent::default();
    consent.repositories.insert(
        repository_id.to_owned(),
        CaptureConsent {
            untracked: UntrackedConsent::Any,
            operations: OperationConsent::None,
        },
    );
    let snapshot = capture_session_snapshot(git, std::slice::from_ref(&repo_input), &consent)
        .with_context(|| format!("capture raw checkout snapshot at {}", checkout.display()))?
        .repositories
        .remove(repository_id)
        .context("raw checkout snapshot missing captured repository")?;

    let origin = mj_checkpoint::archive::redact_origin_credentials(&source.fetch_url)?;
    let branch = match &snapshot.metadata.head {
        SnapshotHead::Branch { name, .. } => Some(name.clone()),
        SnapshotHead::Detached(_) => None,
    };
    let head_commit = snapshot.metadata.head.commit().to_owned();
    let repository = mj_checkpoint::archive::RepositorySnapshot {
        metadata: mj_checkpoint::archive::RepositoryMetadata {
            id: repository_id.to_owned(),
            relative_destination: destination.to_path_buf(),
            checkout_subdirectory: None,
            origin,
            push_urls: Vec::new(),
            remote_workspace: false,
            plain_directory: false,
            base_commit: String::new(),
            head_commit,
            branch,
            review_refs_metadata_only: false,
            review_baseline_ref: None,
            saved_refs: Default::default(),
            stash_stack: Vec::new(),
            git_operation: None,
            origin_delta: Some(Box::new(snapshot.metadata)),
        },
        origin_delta_pack: snapshot.pack,
        committed_bundle: Vec::new(),
        staged_patch: Vec::new(),
        unstaged_patch: Vec::new(),
        untracked_tar: Vec::new(),
        operation_payloads: None,
        linked_worktrees: Vec::new(),
    };
    Ok(LegacyRawConversionSnapshot {
        repository,
        untracked_paths,
        _pack_dir: pack_dir,
    })
}

/// Describe a raw-to-workspace conversion for a person to confirm. Reads Git
/// and asks the remote for its default branch; changes nothing.
#[cfg(any(test, feature = "test-support"))]
pub(super) fn raw_conversion_preview(
    session: &SessionRecord,
    conversion: &RawToWorkspaceConversion,
    executor: &(impl CommandExecutor + Sync),
) -> Result<mj_core::state::RawConversionPreview> {
    raw_conversion_preview_with_checkout(session, conversion, executor)
}

pub(super) fn raw_conversion_preview_with_checkout(
    session: &SessionRecord,
    conversion: &RawToWorkspaceConversion,
    executor: &(impl CommandExecutor + Sync),
) -> Result<mj_core::state::RawConversionPreview> {
    let checkout = conversion.checkout.as_path();
    // A dirty submodule cannot be captured, so say so now rather than failing
    // after the session has been stopped.
    reject_dirty_submodules_for_move(&ExecutorGit::local(executor), checkout)?;
    let default_branch = mj_core::remote_git::default_branch(&conversion.source, executor)?;
    let position = read_checkout_position(executor, &ManagedWorktreeTarget::Local, checkout)?;
    let unpushed_commits = unpushed_commit_count(executor, checkout)?;
    let dirty = dirty_file_counts(executor, checkout)?;
    // A raw session has no container, so the move builds it one and the
    // checkout lands in the per-session workspace this preview names. A session
    // that predates per-session workspaces and still records none keeps the
    // shared one only if it already has a container, which a raw session never
    // does.
    let container_workspace = match session.container_workspace.clone() {
        Some(workspace) => workspace,
        None => mj_core::targets::new_container_workspace(&session.id)?,
    };
    Ok(mj_core::state::RawConversionPreview {
        checkout: checkout.to_path_buf(),
        destination: target_path::join(&container_workspace, &conversion.destination),
        branch: position.branch,
        fetch_url: conversion.source.fetch_url.clone(),
        push_urls: conversion.source.push_urls.clone(),
        default_branch,
        unpushed_commits,
        staged_files: dirty.staged_files,
        unstaged_files: dirty.unstaged_files,
        untracked_files: dirty.untracked_files,
        untracked_bytes: untracked_bytes(executor, checkout)?,
        host_checkout_retained: conversion.retire.is_none(),
    })
}

/// The conversion snapshot carries each gitlink as the commit it points to,
/// so uncommitted work in a submodule would not arrive. A gitlink with no
/// `.gitmodules` entry never blocks the move; it is logged because its files
/// stay behind.
fn reject_dirty_submodules_for_move(
    git: &dyn mj_checkpoint::archive::GitCommandRunner,
    checkout: &Path,
) -> Result<()> {
    let inspection = mj_checkpoint::checkpoint::inspect_submodules(git, checkout)
        .with_context(|| format!("checkout {}", checkout.display()))?;
    for gitlink in &inspection.unregistered {
        tracing::warn!(
            checkout = %checkout.display(),
            gitlink = %gitlink.display(),
            "gitlink has no .gitmodules entry; the move carries the commit it points to, not its files"
        );
    }
    if let Some(dirty) = inspection.dirty_summary() {
        bail!(
            "{}: {dirty}, which cannot move into a target; commit or stash them first",
            checkout.display()
        );
    }
    Ok(())
}

/// Runs checkpoint Git policy through the owning command executor, retaining
/// cancellation and the source target's filesystem namespace.
pub(super) struct ExecutorGit<'a, E> {
    executor: &'a E,
    location: ExecutorGitLocation<'a>,
}

enum ExecutorGitLocation<'a> {
    Local,
    Session {
        backend: &'a crate::targets::TargetLocator,
        id: &'a str,
    },
}

impl<'a, E> ExecutorGit<'a, E> {
    pub(super) fn local(executor: &'a E) -> Self {
        Self {
            executor,
            location: ExecutorGitLocation::Local,
        }
    }

    pub(super) fn for_session(
        executor: &'a E,
        backend: &'a crate::targets::TargetLocator,
        id: &'a str,
    ) -> Self {
        Self {
            executor,
            location: ExecutorGitLocation::Session { backend, id },
        }
    }
}

impl<E: CommandExecutor + Sync> mj_checkpoint::archive::GitCommandRunner for ExecutorGit<'_, E> {
    fn run(
        &self,
        repository: &Path,
        command: &mj_checkpoint::archive::GitCommand,
    ) -> Result<mj_checkpoint::archive::GitOutput> {
        ensure!(
            matches!(self.location, ExecutorGitLocation::Local)
                || (command.stdin.is_empty() && command.env.is_empty()),
            "a target Git command takes no standard input or extra environment"
        );
        let mut environment: Vec<_> = mj_checkpoint::archive::NON_INTERACTIVE_GIT_ENV
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        // Git on this computer inherits an operator's command exactly; Git on
        // a target receives it as text.
        let runs_here = matches!(
            self.location,
            ExecutorGitLocation::Local
                | ExecutorGitLocation::Session {
                    backend: crate::targets::TargetLocator::LocalBare { .. },
                    ..
                }
        );
        let ssh_command = if runs_here {
            mj_core::local_git::local_git_ssh_command().map(str::to_owned)
        } else {
            Some(mj_core::local_git::remote_git_ssh_command()?)
        };
        if let Some(ssh_command) = ssh_command {
            environment.push(("GIT_SSH_COMMAND".into(), ssh_command));
        }
        let mut args = vec![
            "git".into(),
            "-C".into(),
            repository.to_string_lossy().into_owned(),
        ];
        args.extend(
            command
                .arguments
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned()),
        );
        let command = match self.location {
            ExecutorGitLocation::Local => {
                let mut spec = CommandSpec::new("git", args.into_iter().skip(1))
                    .purpose("inspect local Git checkout");
                spec.env.extend(environment);
                for (name, value) in &command.env {
                    spec.env.insert(
                        name.to_str()
                            .context("Git environment name is not UTF-8")?
                            .to_owned(),
                        value
                            .to_str()
                            .context("Git environment value is not UTF-8")?
                            .to_owned(),
                    );
                }
                if !command.stdin.is_empty() {
                    spec = spec.with_sensitive_stdin(command.stdin.clone());
                }
                spec
            }
            ExecutorGitLocation::Session {
                backend: backend @ crate::targets::TargetLocator::LocalBare { .. },
                id,
            } => {
                let mut command = crate::targets::command_on_locator(
                    SshTrafficClass::Control,
                    backend,
                    id,
                    args,
                    "inspect source checkout base",
                )?;
                command.env.extend(environment);
                command
            }
            ExecutorGitLocation::Session { backend, id } => {
                // Environment belongs to Git on the target, not to the local
                // SSH/container transport process.
                let mut remote = vec!["env".into()];
                remote.extend(
                    environment
                        .into_iter()
                        .map(|(name, value)| format!("{name}={value}")),
                );
                remote.extend(args);
                crate::targets::command_on_locator(
                    SshTrafficClass::Control,
                    backend,
                    id,
                    remote,
                    "inspect source checkout base",
                )?
            }
        };
        let output = self.executor.execute(&command)?;
        Ok(mj_checkpoint::archive::GitOutput {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// Commits the conversion archive has to carry. A checkout whose origin refs
/// are missing even after a repair fetch reports nothing rather than counting
/// its entire history as unpushed.
fn unpushed_commit_count(executor: &impl CommandExecutor, checkout: &Path) -> Result<u64> {
    if !origin_refs_available(executor, checkout)? {
        return Ok(0);
    }
    let counted = managed_git_stdout(
        executor,
        &ManagedWorktreeTarget::Local,
        checkout,
        ["rev-list", "--count", "HEAD", "--not", "--remotes=origin"],
        "count commits outside origin",
    )?;
    counted
        .trim()
        .parse()
        .with_context(|| format!("parse the commit count {counted:?}"))
}

fn origin_refs_available(executor: &impl CommandExecutor, checkout: &Path) -> Result<bool> {
    if origin_refs_listed(executor, checkout)? {
        return Ok(true);
    }
    // A checkout that has never fetched has no origin refs yet. Try once; a
    // remote that cannot be reached leaves the count unreported, not failed.
    let fetch = managed_network_git_command(
        SshTrafficClass::Bulk,
        &ManagedWorktreeTarget::Local,
        checkout,
        ["fetch", "origin"],
        "fetch origin refs",
    );
    executor.execute(&fetch)?;
    origin_refs_listed(executor, checkout)
}

fn origin_refs_listed(executor: &impl CommandExecutor, checkout: &Path) -> Result<bool> {
    managed_git_stdout(
        executor,
        &ManagedWorktreeTarget::Local,
        checkout,
        [
            "for-each-ref",
            "--format=%(objectname)",
            "refs/remotes/origin",
        ],
        "list origin refs",
    )
    .map(|refs| !refs.trim().is_empty())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DirtyFileCounts {
    staged_files: u64,
    unstaged_files: u64,
    untracked_files: u64,
}

/// Count what `git status` reports, one entry per path. A rename's second
/// record names the original path, so it is consumed rather than counted.
fn dirty_file_counts(executor: &impl CommandExecutor, checkout: &Path) -> Result<DirtyFileCounts> {
    let command = managed_git_command(
        SshTrafficClass::Control,
        &ManagedWorktreeTarget::Local,
        checkout,
        ["status", "--porcelain=v1", "-z"],
        "read checkout status",
    );
    let output = executor.execute(&command)?;
    ensure!(
        output.status == 0,
        "read checkout status failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let mut counts = DirtyFileCounts::default();
    let mut records = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        let [index, worktree, ..] = record else {
            bail!("git status produced a record shorter than its status field");
        };
        if *index == b'?' && *worktree == b'?' {
            counts.untracked_files += 1;
            continue;
        }
        if !matches!(index, b' ' | b'?') {
            counts.staged_files += 1;
        }
        if !matches!(worktree, b' ' | b'?') {
            counts.unstaged_files += 1;
        }
        if *index == b'R' || *index == b'C' || *worktree == b'R' || *worktree == b'C' {
            records.next();
        }
    }
    Ok(counts)
}

/// How much untracked content the conversion archive has to carry. `git status`
/// collapses an untracked directory into one entry, so the bytes come from the
/// file list instead.
fn untracked_bytes(executor: &impl CommandExecutor, checkout: &Path) -> Result<u64> {
    let command = managed_git_command(
        SshTrafficClass::Control,
        &ManagedWorktreeTarget::Local,
        checkout,
        ["ls-files", "--others", "--exclude-standard", "-z"],
        "list untracked files",
    );
    let output = executor.execute(&command)?;
    ensure!(
        output.status == 0,
        "list untracked files failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let mut total = 0;
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let relative = mj_core::path_input::from_git_bytes(record)?;
        let path = checkout.join(relative);
        // Do not follow links, and tolerate a file the agent removed between
        // the listing and this read.
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => total += metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("measure {}", path.display()));
            }
        }
    }
    Ok(total)
}

/// Where a checkout stands: its head commit and, unless detached, its branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CheckoutPosition {
    pub(super) head_commit: String,
    branch: Option<String>,
}

fn read_checkout_position(
    executor: &impl CommandExecutor,
    target: &ManagedWorktreeTarget,
    directory: &Path,
) -> Result<CheckoutPosition> {
    let head_commit = managed_git_stdout(
        executor,
        target,
        directory,
        ["rev-parse", "HEAD"],
        "resolve checkout head commit",
    )?;
    let branch_command = managed_git_command(
        SshTrafficClass::Control,
        target,
        directory,
        ["symbolic-ref", "--quiet", "--short", "HEAD"],
        "resolve checkout branch",
    );
    let branch_output = executor.execute(&branch_command)?;
    let branch = match branch_output.status {
        0 => Some(
            String::from_utf8(branch_output.stdout)
                .context("checkout branch was not UTF-8")?
                .trim()
                .to_owned(),
        ),
        // A detached head reports no branch rather than failing.
        1 | 128 => None,
        status => bail!(
            "resolve checkout branch failed with status {status}: {}",
            String::from_utf8_lossy(&branch_output.stderr).trim()
        ),
    };
    Ok(CheckoutPosition {
        head_commit,
        branch,
    })
}

/// The commit the session branch was created at, as the base for diffs and
/// checkpoint bundles. Prefers the recorded base; sessions created before it
/// was recorded fall back to the branch reflog, like `branch_creation_commit`
/// in mj-checkpoint. A reflog that has expired leaves only the live head,
/// which yields an empty bundle rather than a failed checkpoint.
pub(super) fn managed_worktree_base_commit(
    worktree: &ManagedWorktree,
    executor: &impl CommandExecutor,
) -> Result<String> {
    if let Some(base) = &worktree.base_commit {
        return Ok(base.clone());
    }
    let reference = format!("refs/heads/{}", worktree.branch);
    let reflog_command = managed_git_command(
        SshTrafficClass::Control,
        &worktree.target,
        &worktree.source_repository,
        ["reflog", "show", "--format=%H", &reference],
        "read the session branch reflog",
    );
    let reflog_output = executor.execute(&reflog_command)?;
    if reflog_output.status == 0 {
        let text = String::from_utf8(reflog_output.stdout)
            .context("the session branch reflog was not UTF-8")?;
        // The oldest entry is the branch's creation, so it is where the session
        // started.
        if let Some(creation) = text.lines().rfind(|line| !line.trim().is_empty()) {
            return Ok(creation.trim().to_owned());
        }
    }
    let head = read_checkout_position(executor, &worktree.target, &worktree.worktree_root)?;
    tracing::warn!(
        branch = %worktree.branch,
        "the reflog for this session branch is gone, so its checkpoint bundle will carry no commits"
    );
    Ok(head.head_commit)
}

/// Read where a raw session's checkout stands right now, on whichever host
/// owns it.
#[cfg(any(test, feature = "test-support"))]
pub(super) fn raw_checkout_position(
    session: &SessionRecord,
    config: &Config,
    project_directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<CheckoutPosition> {
    let checkout = match session.checkout() {
        checkout @ mj_core::state::Checkout::ManagedWorktree { .. } => checkout,
        _ => mj_core::state::Checkout::Attached {
            path: project_directory,
        },
    };
    raw_checkout_position_with_checkout(session, &checkout, config, project_directory, executor)
}

pub(super) fn raw_checkout_position_with_checkout(
    session: &SessionRecord,
    checkout: &mj_core::state::Checkout<'_>,
    config: &Config,
    project_directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<CheckoutPosition> {
    let target = match checkout.effective() {
        mj_core::state::Checkout::ManagedWorktree { worktree, .. } => worktree.target.clone(),
        mj_core::state::Checkout::Attached { .. } => {
            let runtime = session.target_runtime_settings(config)?;
            match (&*runtime.kind, &runtime.connection) {
                ("local-bare", mj_core::state::TargetConnection::Local) => {
                    ManagedWorktreeTarget::Local
                }
                ("ssh-bare", mj_core::state::TargetConnection::Ssh { ssh }) => {
                    let ssh = targets::SshTarget::from(ssh);
                    ManagedWorktreeTarget::Ssh {
                        destination: ssh.destination,
                        ssh_args: ssh.ssh_args,
                    }
                }
                _ => bail!("the session's recorded target is not a bare checkout"),
            }
        }
        mj_core::state::Checkout::ManagedWorkspace | mj_core::state::Checkout::Borrowed { .. } => {
            bail!("the session's recorded target is not a bare checkout")
        }
    };
    read_checkout_position(executor, &target, project_directory)
}

/// One conversation line for a raw session whose checkout moved on while the
/// session was stopped. `None` when the checkout is where the checkpoint left
/// it, or when the checkpoint recorded no repository to compare against.
///
/// This reports; it never reconciles. The working tree is the truth.
pub(super) fn raw_checkout_divergence_notice(
    directory: &Path,
    recorded: Option<&mj_checkpoint::archive::RepositoryMetadata>,
    live: &CheckoutPosition,
) -> Option<String> {
    let recorded = recorded?;
    if recorded.head_commit.is_empty()
        || (recorded.head_commit == live.head_commit && recorded.branch == live.branch)
    {
        return None;
    }
    Some(format!(
        "The working tree at {} moved from {} to {} while this session was stopped.",
        directory.display(),
        checkout_position_text(&recorded.head_commit, recorded.branch.as_deref()),
        checkout_position_text(&live.head_commit, live.branch.as_deref()),
    ))
}

fn checkout_position_text(head_commit: &str, branch: Option<&str>) -> String {
    let short = head_commit.get(..12).unwrap_or(head_commit);
    match branch {
        Some(branch) => format!("{short} ({branch})"),
        None => format!("{short} (detached)"),
    }
}

fn inspect_raw_project(
    executor: &impl CommandExecutor,
    target: &ManagedWorktreeTarget,
    selected: &Path,
) -> Result<RawProjectInspection> {
    let repository = git_output_path(
        target,
        PathBuf::from(managed_git_stdout(
            executor,
            target,
            selected,
            ["rev-parse", "--show-toplevel"],
            "resolve raw project repository root",
        )?),
    )?;
    let prefix = managed_git_stdout(
        executor,
        target,
        selected,
        ["rev-parse", "--show-prefix"],
        "resolve raw project relative directory",
    )?;
    let git_dir = git_output_path(
        target,
        PathBuf::from(managed_git_stdout(
            executor,
            target,
            selected,
            ["rev-parse", "--absolute-git-dir"],
            "resolve raw project Git directory",
        )?),
    )?;
    let common_git_dir = git_output_path(
        target,
        mj_core::local_git::resolve_git_path(
            &repository,
            &managed_git_stdout(
                executor,
                target,
                selected,
                ["rev-parse", "--path-format=absolute", "--git-common-dir"],
                "resolve raw project common Git directory",
            )?,
        )?,
    )?;
    let branch_command = managed_git_command(
        SshTrafficClass::Control,
        target,
        selected,
        ["symbolic-ref", "--quiet", "--short", "HEAD"],
        "resolve raw project branch",
    );
    let branch_output = executor.execute(&branch_command)?;
    let branch = match branch_output.status {
        0 => Some(
            String::from_utf8(branch_output.stdout)
                .context("raw project branch was not UTF-8")?
                .trim()
                .to_owned(),
        ),
        1 | 128 => None,
        status => bail!(
            "resolve raw project branch failed with status {status}: {}",
            String::from_utf8_lossy(&branch_output.stderr).trim()
        ),
    };
    let upstream = match branch {
        Some(branch) => {
            let reference = format!("refs/heads/{branch}");
            let upstream = managed_git_stdout(
                executor,
                target,
                selected,
                ["for-each-ref", "--format=%(upstream:short)", &reference],
                "resolve raw project upstream",
            )?;
            (!upstream.is_empty()).then_some(upstream)
        }
        None => None,
    };
    Ok(RawProjectInspection {
        source_project_directory: target_path::join(&repository, Path::new(&prefix)),
        source_repository: repository,
        primary_checkout: git_dir == common_git_dir,
        upstream,
    })
}

fn ensure_managed_worktree_excluded(
    executor: &impl CommandExecutor,
    target: &ManagedWorktreeTarget,
    repository: &Path,
    kind: ManagedCheckoutKind,
) -> Result<()> {
    let (path, entry) = match kind {
        ManagedCheckoutKind::Worktree => (".mj/worktrees/", "/.mj/worktrees/"),
        ManagedCheckoutKind::Clone => (".mj/clones/", "/.mj/clones/"),
    };
    let check = managed_git_command(
        SshTrafficClass::Control,
        target,
        repository,
        ["check-ignore", "--quiet", "--no-index", "--", path],
        "check managed worktree exclusion",
    );
    let output = executor.execute(&check)?;
    match output.status {
        0 => return Ok(()),
        1 => {}
        status => bail!(
            "check managed worktree exclusion failed with status {status}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
    let exclude_path = mj_core::local_git::resolve_git_path(
        repository,
        &managed_git_stdout(
            executor,
            target,
            repository,
            [
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "info/exclude",
            ],
            "resolve repository-local exclude file",
        )?,
    )?;
    match target {
        ManagedWorktreeTarget::Local => {
            use std::io::Write;
            let existing = match std::fs::read_to_string(&exclude_path) {
                Ok(existing) => existing,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => return Err(error.into()),
            };
            if existing.lines().any(|line| line.trim() == entry) {
                return Ok(());
            }
            if let Some(parent) = exclude_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&exclude_path)
                .with_context(|| format!("open {}", exclude_path.display()))?;
            if !existing.is_empty() && !existing.ends_with('\n') {
                writeln!(file)?;
            }
            writeln!(file, "# Mjolnir managed checkouts\n{entry}")?;
        }
        ManagedWorktreeTarget::Ssh { .. } => {
            const SCRIPT: &str = "set -eu\nexclude=$1\nentry=$2\nmkdir -p \"$(dirname \"$exclude\")\"\ntouch \"$exclude\"\nif ! grep -Fqx \"$entry\" \"$exclude\"; then\n  if [ -s \"$exclude\" ] && [ \"$(tail -c 1 \"$exclude\" | wc -l)\" -eq 0 ]; then printf '\\n' >>\"$exclude\"; fi\n  printf '# Hel managed worktrees\\n%s\\n' \"$entry\" >>\"$exclude\"\nfi";
            let command = managed_target_command(
                SshTrafficClass::Control,
                target,
                "sh",
                [
                    "-c",
                    SCRIPT,
                    "hel-exclude",
                    &exclude_path.to_string_lossy(),
                    entry,
                ],
            )
            .purpose("update remote repository-local exclude file");
            execute_checked(executor, command)?;
        }
    }
    Ok(())
}

pub(super) fn managed_worktree_checkout_exists(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
) -> Result<bool> {
    path_exists_on_managed_target(executor, &worktree.target, &worktree.worktree_root)
}

/// Whether a managed worktree's checkout holds work that removing it would
/// destroy. A checkout that is already gone holds nothing.
///
/// This asks the session's own worktree the porcelain question
/// [`create_managed_worktree`] asks of the primary checkout.
pub(super) fn managed_worktree_checkout_is_dirty(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
) -> Result<bool> {
    if !path_exists_on_managed_target(executor, &worktree.target, &worktree.worktree_root)? {
        return Ok(false);
    }
    let status = managed_git_stdout(
        executor,
        &worktree.target,
        &worktree.worktree_root,
        ["status", "--porcelain=v1", "--untracked-files=all"],
        "inspect managed worktree changes",
    )?;
    Ok(!status.is_empty())
}

/// Whether a new managed worktree needs the primary checkout to be clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PrimaryCheckoutRequirement {
    /// A new raw session starts from the primary checkout's HEAD, so work that
    /// is only in its working tree would be silently left behind.
    Clean,
    /// A session moving out of its target replaces the worktree's contents from
    /// its checkpoint, so the primary checkout's own changes are beside the
    /// point.
    Any,
}

pub(super) fn create_managed_worktree(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
    upstream: Option<&str>,
    requirement: PrimaryCheckoutRequirement,
) -> Result<()> {
    ensure_managed_worktree_excluded(
        executor,
        &worktree.target,
        &worktree.source_repository,
        worktree.kind,
    )?;
    if worktree.kind == ManagedCheckoutKind::Clone {
        return create_managed_clone(executor, worktree);
    }
    if requirement == PrimaryCheckoutRequirement::Clean {
        let status = managed_git_stdout(
            executor,
            &worktree.target,
            &worktree.source_repository,
            ["status", "--porcelain=v1", "--untracked-files=all"],
            "inspect primary checkout changes",
        )?;
        if !status.is_empty() {
            let paths = status.lines().take(20).collect::<Vec<_>>().join("\n  ");
            bail!(
                "primary checkout has uncommitted changes; commit or stash them before creating a raw session worktree:\n  {paths}"
            );
        }
    }
    let parent = worktree
        .worktree_root
        .parent()
        .context("managed worktree root has no parent")?;
    execute_checked(
        executor,
        managed_target_command(
            SshTrafficClass::Control,
            &worktree.target,
            "mkdir",
            ["-p", &parent.to_string_lossy()],
        )
        .purpose("create managed worktree directory"),
    )?;
    execute_checked(
        executor,
        managed_git_command(
            SshTrafficClass::Control,
            &worktree.target,
            &worktree.source_repository,
            [
                "worktree",
                "add",
                "-b",
                &worktree.branch,
                &worktree.worktree_root.to_string_lossy(),
                worktree.base_commit.as_deref().unwrap_or("HEAD"),
            ],
            "create managed raw-session worktree",
        ),
    )?;
    if let Some(upstream) = upstream {
        execute_checked(
            executor,
            managed_git_command(
                SshTrafficClass::Control,
                &worktree.target,
                &worktree.worktree_root,
                ["branch", "--set-upstream-to", upstream, &worktree.branch],
                "set managed worktree branch upstream",
            ),
        )?;
    }
    Ok(())
}

enum CloneSeedMode<'a> {
    FreshBranch,
    LocalSnapshot(&'a mj_checkpoint::snapshot::WorkspaceSnapshot),
    RemoteSnapshot {
        object_format: &'a str,
        origin_url: Option<&'a str>,
    },
}

impl<'a> CloneSeedMode<'a> {
    fn snapshot_object_format(&self) -> Option<&'a str> {
        match self {
            Self::FreshBranch => None,
            Self::LocalSnapshot(snapshot) => Some(snapshot.metadata.object_format.as_str()),
            Self::RemoteSnapshot { object_format, .. } => Some(*object_format),
        }
    }

    fn snapshot_origin_url(&self) -> Option<&'a str> {
        match self {
            Self::FreshBranch => None,
            Self::LocalSnapshot(snapshot) => snapshot.metadata.origin_url.as_deref(),
            Self::RemoteSnapshot { origin_url, .. } => *origin_url,
        }
    }

    fn local_snapshot(&self) -> Option<&'a mj_checkpoint::snapshot::WorkspaceSnapshot> {
        match self {
            Self::LocalSnapshot(snapshot) => Some(snapshot),
            Self::FreshBranch | Self::RemoteSnapshot { .. } => None,
        }
    }

    fn is_fresh_branch(&self) -> bool {
        matches!(self, Self::FreshBranch)
    }
}

pub(super) enum ManagedCloneRestoreOutcome {
    /// Snapshot was applied directly into the local managed clone.
    RestoredLocally(Vec<String>),
    /// Empty clone was seeded on the remote SSH target; the target worker
    /// must run `restore_repositories` during `restore_into_target`.
    SeededForRemoteRestore,
}

fn create_managed_clone(executor: &impl CommandExecutor, checkout: &ManagedWorktree) -> Result<()> {
    create_managed_clone_with_snapshot(executor, checkout, CloneSeedMode::FreshBranch).map(drop)
}

fn create_managed_clone_with_snapshot(
    executor: &impl CommandExecutor,
    checkout: &ManagedWorktree,
    mode: CloneSeedMode<'_>,
) -> Result<Vec<String>> {
    let parent = checkout
        .worktree_root
        .parent()
        .context("managed clone has no parent")?;
    let staging = checkout.worktree_root.with_extension("provisioning");
    ensure!(
        !path_exists_on_managed_target(executor, &checkout.target, &staging)?
            && !path_exists_on_managed_target(executor, &checkout.target, &checkout.worktree_root)?,
        "managed clone path is already occupied: {}",
        checkout.worktree_root.display()
    );
    execute_checked(
        executor,
        managed_target_command(
            SshTrafficClass::Control,
            &checkout.target,
            "mkdir",
            ["-p", &parent.to_string_lossy()],
        )
        .purpose("create managed clone parent"),
    )?;
    let mut published = false;
    let seed_from_source = match mode.snapshot_object_format() {
        None => true,
        Some(expected_object_format) => {
            path_exists_on_managed_target(executor, &checkout.target, &checkout.source_repository)?
                && executor
                    .execute(&managed_git_command(
                        SshTrafficClass::Control,
                        &checkout.target,
                        &checkout.source_repository,
                        ["rev-parse", "--show-object-format"],
                        "check source repository object format before seeding clone",
                    ))
                    .is_ok_and(|output| {
                        output.status == 0
                            && String::from_utf8_lossy(&output.stdout).trim()
                                == expected_object_format
                    })
        }
    };
    let create = (|| -> Result<Vec<String>> {
        if seed_from_source {
            execute_checked(
                executor,
                managed_target_command(
                    SshTrafficClass::Bulk,
                    &checkout.target,
                    "git",
                    [
                        "clone",
                        "--local",
                        "--dissociate",
                        "--no-checkout",
                        "--",
                        &checkout.source_repository.to_string_lossy(),
                        &staging.to_string_lossy(),
                    ],
                )
                .purpose("seed independent managed clone"),
            )?;
            let origin = executor.execute(&managed_git_command(
                SshTrafficClass::Control,
                &checkout.target,
                &checkout.source_repository,
                ["config", "--get", "remote.origin.url"],
                "read source origin URL",
            ))?;
            execute_checked(
                executor,
                managed_git_command(
                    SshTrafficClass::Control,
                    &checkout.target,
                    &staging,
                    ["remote", "remove", "origin"],
                    "discard local seed as clone remote",
                ),
            )?;
            match origin.status {
                0 => {
                    let url = String::from_utf8(origin.stdout)?;
                    execute_checked(
                        executor,
                        managed_git_command(
                            SshTrafficClass::Control,
                            &checkout.target,
                            &staging,
                            ["remote", "add", "origin", url.trim()],
                            "set clone fetch and push remote",
                        ),
                    )?;
                    copy_clone_push_configuration(executor, checkout, &staging)?;
                    copy_source_origin_refs(executor, checkout, &staging)?;
                }
                1 => {}
                status => bail!(
                    "read source origin URL failed with status {status}: {}",
                    String::from_utf8_lossy(&origin.stderr).trim()
                ),
            }
            copy_clone_local_git_preferences(executor, checkout, &staging)?;
            if mode.is_fresh_branch() {
                execute_checked(
                    executor,
                    managed_git_command(
                        SshTrafficClass::Control,
                        &checkout.target,
                        &staging,
                        [
                            "switch",
                            "--no-track",
                            "-C",
                            &checkout.branch,
                            checkout
                                .base_commit
                                .as_deref()
                                .context("managed clone has no launch commit")?,
                        ],
                        "select managed clone starting branch",
                    ),
                )?;
                if origin.status == 0 {
                    execute_checked(
                        executor,
                        managed_git_command(
                            SshTrafficClass::Control,
                            &checkout.target,
                            &staging,
                            [
                                "config",
                                "--local",
                                &format!("branch.{}.remote", checkout.branch),
                                "origin",
                            ],
                            "set clone branch push remote",
                        ),
                    )?;
                    execute_checked(
                        executor,
                        managed_git_command(
                            SshTrafficClass::Control,
                            &checkout.target,
                            &staging,
                            [
                                "config",
                                "--local",
                                &format!("branch.{}.merge", checkout.branch),
                                &format!("refs/heads/{}", checkout.branch),
                            ],
                            "set clone branch tracking name",
                        ),
                    )?;
                }
            }
        } else {
            let origin = mode
                .snapshot_origin_url()
                .context("origin-seeded managed clone requires a snapshot origin URL")?;
            execute_checked(
                executor,
                managed_target_command(
                    SshTrafficClass::Bulk,
                    &checkout.target,
                    "git",
                    [
                        "clone",
                        "--no-checkout",
                        "--",
                        origin,
                        &staging.to_string_lossy(),
                    ],
                )
                .purpose("seed independent managed clone from origin"),
            )?;
        }
        let notices = if let Some(snapshot) = mode.local_snapshot() {
            let mut notices = snapshot.notices.clone();
            // Restore runs Git fetches; keep them off the async workers.
            notices.extend(mj_core::runtime::off_async_worker(|| {
                mj_checkpoint::snapshot::restore_workspace_snapshot(
                    &mj_checkpoint::archive::SystemGit,
                    snapshot,
                    &staging,
                )
            })?);
            notices
        } else {
            Vec::new()
        };
        execute_checked(
            executor,
            managed_target_command(
                SshTrafficClass::Control,
                &checkout.target,
                "mv",
                [
                    "--",
                    &staging.to_string_lossy(),
                    &checkout.worktree_root.to_string_lossy(),
                ],
            )
            .purpose("publish managed clone checkout"),
        )?;
        published = true;
        if let Some(snapshot) = mode.local_snapshot() {
            mj_checkpoint::snapshot::repair_workspace_snapshot(
                &mj_checkpoint::archive::SystemGit,
                snapshot,
                &checkout.worktree_root,
            )?;
        }
        Ok(notices)
    })();
    let cleanup = if published {
        &checkout.worktree_root
    } else {
        &staging
    };
    if create.is_err() && path_exists_on_managed_target(executor, &checkout.target, cleanup)? {
        execute_checked(
            executor,
            managed_target_command(
                SshTrafficClass::Control,
                &checkout.target,
                "rm",
                ["-rf", "--", &cleanup.to_string_lossy()],
            )
            .purpose("remove failed managed clone staging directory"),
        )?;
    }
    create
}

fn copy_clone_push_configuration(
    executor: &impl CommandExecutor,
    checkout: &ManagedWorktree,
    staging: &Path,
) -> Result<()> {
    let output = executor.execute(&managed_git_command(
        SshTrafficClass::Control,
        &checkout.target,
        &checkout.source_repository,
        ["config", "--local", "--get-all", "remote.origin.pushurl"],
        "read source push destinations",
    ))?;
    match output.status {
        0 => {
            for url in String::from_utf8(output.stdout)?
                .lines()
                .filter(|line| !line.is_empty())
            {
                execute_checked(
                    executor,
                    managed_git_command(
                        SshTrafficClass::Control,
                        &checkout.target,
                        staging,
                        ["remote", "set-url", "--push", "--add", "origin", url],
                        "preserve clone push destination",
                    ),
                )?;
            }
        }
        1 => {}
        status => bail!("read source push destinations failed with status {status}"),
    }
    Ok(())
}

fn copy_source_origin_refs(
    executor: &impl CommandExecutor,
    checkout: &ManagedWorktree,
    staging: &Path,
) -> Result<()> {
    let refs = managed_git_stdout(
        executor,
        &checkout.target,
        &checkout.source_repository,
        ["for-each-ref", "--format=%(refname)", "refs/remotes/origin"],
        "read cached origin branches",
    )?;
    if refs.lines().any(|name| name != "refs/remotes/origin/HEAD") {
        // Dissociation copies objects reachable from the clone's own refs, not
        // source-only remote refs whose objects live in an alternate store.
        // Fetch their objects and refs together from the trusted local seed.
        execute_checked(
            executor,
            managed_git_command(
                SshTrafficClass::Bulk,
                &checkout.target,
                staging,
                [
                    "fetch",
                    "--no-write-fetch-head",
                    "--no-tags",
                    "--no-recurse-submodules",
                    "--",
                    &checkout.source_repository.to_string_lossy(),
                    "+refs/remotes/origin/*:refs/remotes/origin/*",
                    "^refs/remotes/origin/HEAD",
                ],
                "preserve cached origin branches",
            ),
        )?;
    }
    Ok(())
}

fn copy_clone_local_git_preferences(
    executor: &impl CommandExecutor,
    checkout: &ManagedWorktree,
    staging: &Path,
) -> Result<()> {
    let config = executor.execute(&managed_git_command(
        SshTrafficClass::Control,
        &checkout.target,
        &checkout.source_repository,
        ["config", "--local", "--null", "--list"],
        "read source Git preferences",
    ))?;
    ensure!(
        config.status == 0,
        "read source Git preferences failed with status {}",
        config.status
    );
    for entry in config
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let Some(split) = entry.iter().position(|byte| *byte == b'\n') else {
            bail!("source Git configuration contains a malformed entry");
        };
        let key = std::str::from_utf8(&entry[..split])?;
        if !clone_local_preference(key) {
            continue;
        }
        let value = std::str::from_utf8(&entry[split + 1..])?;
        execute_checked(
            executor,
            managed_git_command(
                SshTrafficClass::Control,
                &checkout.target,
                staging,
                ["config", "--local", "--add", key, value],
                "preserve Git identity and local preferences",
            ),
        )?;
    }
    let source_exclude = mj_core::local_git::resolve_git_path(
        &checkout.source_repository,
        &managed_git_stdout(
            executor,
            &checkout.target,
            &checkout.source_repository,
            [
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "info/exclude",
            ],
            "locate source Git exclusions",
        )?,
    )?;
    if path_exists_on_managed_target(executor, &checkout.target, &source_exclude)? {
        let clone_exclude = mj_core::local_git::resolve_git_path(
            staging,
            &managed_git_stdout(
                executor,
                &checkout.target,
                staging,
                [
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "info/exclude",
                ],
                "locate clone Git exclusions",
            )?,
        )?;
        execute_checked(
            executor,
            managed_target_command(
                SshTrafficClass::Control,
                &checkout.target,
                "cp",
                [
                    "--",
                    &source_exclude.to_string_lossy(),
                    &clone_exclude.to_string_lossy(),
                ],
            )
            .purpose("preserve source Git exclusions"),
        )?;
    }
    Ok(())
}

fn clone_local_preference(key: &str) -> bool {
    key.starts_with("user.")
        || key.starts_with("commit.")
        || key.starts_with("gpg.")
        || key.starts_with("credential.")
        || key.starts_with("url.")
        || key.starts_with("push.")
        || matches!(
            key,
            "core.hookspath" | "core.excludesfile" | "core.attributesfile" | "core.sshcommand"
        )
}

/// Recreate a retired checkout from the session branch. Returns whether this
/// call created it, so a failed resume can put the session back into its
/// stopped, checkout-free state.
pub(super) fn restore_managed_clone_origin_delta(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
    archive: &Path,
    manifest: &mj_checkpoint::archive::ArchiveManifest,
) -> Result<ManagedCloneRestoreOutcome> {
    use mj_checkpoint::archive::CheckpointPayloadReader;
    use mj_checkpoint::snapshot::WorkspaceSnapshot;

    ensure!(
        worktree.kind == ManagedCheckoutKind::Clone,
        "origin delta resume requires an independent managed clone"
    );
    let [repository] = manifest.repositories.as_slice() else {
        bail!("origin delta resume requires exactly one repository");
    };
    let metadata = repository
        .metadata
        .origin_delta
        .as_deref()
        .context("origin delta metadata is missing")?
        .clone();
    if path_exists_on_managed_target(executor, &worktree.target, &worktree.source_repository)?
        && executor
            .execute(&managed_git_command(
                SshTrafficClass::Control,
                &worktree.target,
                &worktree.source_repository,
                ["rev-parse", "--git-dir"],
                "check whether source repository is a Git repository",
            ))
            .is_ok_and(|output| output.status == 0)
    {
        ensure_managed_worktree_excluded(
            executor,
            &worktree.target,
            &worktree.source_repository,
            worktree.kind,
        )?;
    }
    match &worktree.target {
        ManagedWorktreeTarget::Local => {
            let pack = repository
                .origin_delta_pack_path
                .as_ref()
                .map(|path| -> Result<_> {
                    let descriptor = manifest
                        .payloads
                        .iter()
                        .find(|payload| &payload.path == path)
                        .context("origin delta pack descriptor is missing")?;
                    let mut pack = tempfile::NamedTempFile::new()?;
                    CheckpointPayloadReader::open(archive)?
                        .copy_payload(descriptor, pack.as_file_mut())?;
                    Ok(pack)
                })
                .transpose()?;
            let snapshot = WorkspaceSnapshot {
                metadata,
                pack: pack.as_ref().map(|pack| pack.path().to_owned()),
            };
            let notices = create_managed_clone_with_snapshot(
                executor,
                worktree,
                CloneSeedMode::LocalSnapshot(&snapshot),
            )?;
            Ok(ManagedCloneRestoreOutcome::RestoredLocally(notices))
        }
        ManagedWorktreeTarget::Ssh { .. } => {
            // Seed the empty clone on the SSH host; `restore_into_target` then
            // invokes `mj-worker worker restore-checkpoint` with
            // `restore_repositories = true` so the snapshot is unpacked and
            // applied directly on the SSH host.
            create_managed_clone_with_snapshot(
                executor,
                worktree,
                CloneSeedMode::RemoteSnapshot {
                    object_format: &metadata.object_format,
                    origin_url: metadata.origin_url.as_deref(),
                },
            )?;
            Ok(ManagedCloneRestoreOutcome::SeededForRemoteRestore)
        }
    }
}

/// Upgrade a legacy linked-worktree record to a fresh independent managed clone
/// under `<source_repository>/.mj/clones/<session_id>`, preserving any relative
/// subdirectory in `project_directory`.
pub(super) fn upgrade_legacy_worktree_to_clone(
    worktree: &ManagedWorktree,
    session_id: &str,
    project_directory: Option<&Path>,
) -> (ManagedWorktree, Option<PathBuf>) {
    let clone_root = target_path::join(
        &worktree.source_repository,
        &Path::new(".mj").join("clones").join(session_id),
    );
    let upgraded = ManagedWorktree {
        worktree_root: clone_root.clone(),
        kind: ManagedCheckoutKind::Clone,
        ..worktree.clone()
    };
    let upgraded_project_directory = project_directory.map(|dir| {
        if let Ok(relative) = dir.strip_prefix(&worktree.worktree_root)
            && !relative.as_os_str().is_empty()
        {
            target_path::join(&clone_root, relative)
        } else {
            clone_root
        }
    });
    (upgraded, upgraded_project_directory)
}

/// Resume a legacy `ManagedCheckoutKind::Worktree` archive into a fresh
/// independent managed clone:
/// 1. Clone `metadata.origin` (or `source_repository` when `origin` is empty).
/// 2. If `base_commit` is missing from that clone, fetch it once from
///    `source_repository` on the original target.
/// 3. Apply the archived legacy delta and check out the saved commit on the
///    saved branch name, or as a detached HEAD (with a notice) on collision.
pub(super) fn restore_legacy_worktree_as_managed_clone(
    executor: &impl CommandExecutor,
    checkout: &ManagedWorktree,
    archive: &Path,
    manifest: &mj_checkpoint::archive::ArchiveManifest,
) -> Result<Vec<String>> {
    ensure!(
        checkout.kind == ManagedCheckoutKind::Clone,
        "legacy worktree resume requires an upgraded managed clone target"
    );
    ensure!(
        matches!(&checkout.target, ManagedWorktreeTarget::Local),
        "legacy worktree resume into a fresh clone currently requires a local target"
    );
    let [repository] = manifest.repositories.as_slice() else {
        bail!("legacy worktree resume requires exactly one archived repository");
    };
    if path_exists_on_managed_target(executor, &checkout.target, &checkout.source_repository)?
        && executor
            .execute(&managed_git_command(
                SshTrafficClass::Control,
                &checkout.target,
                &checkout.source_repository,
                ["rev-parse", "--git-dir"],
                "check whether source repository is a Git repository",
            ))
            .is_ok_and(|output| output.status == 0)
    {
        ensure_managed_worktree_excluded(
            executor,
            &checkout.target,
            &checkout.source_repository,
            checkout.kind,
        )?;
    }

    let parent = checkout
        .worktree_root
        .parent()
        .context("managed clone has no parent")?;
    let staging = checkout.worktree_root.with_extension("provisioning");
    ensure!(
        !path_exists_on_managed_target(executor, &checkout.target, &staging)?
            && !path_exists_on_managed_target(executor, &checkout.target, &checkout.worktree_root)?,
        "managed clone path is already occupied: {}",
        checkout.worktree_root.display()
    );
    execute_checked(
        executor,
        managed_target_command(
            SshTrafficClass::Control,
            &checkout.target,
            "mkdir",
            ["-p", &parent.to_string_lossy()],
        )
        .purpose("create managed clone parent"),
    )?;

    let mut moved_to_destination = false;
    let create = (|| -> Result<Vec<String>> {
        let origin = repository.metadata.origin.trim();
        if !origin.is_empty() {
            execute_checked(
                executor,
                managed_target_command(
                    SshTrafficClass::Bulk,
                    &checkout.target,
                    "git",
                    [
                        "clone",
                        "--no-checkout",
                        "--",
                        origin,
                        &staging.to_string_lossy(),
                    ],
                )
                .purpose("clone origin for legacy worktree resume"),
            )?;
        } else {
            ensure!(
                path_exists_on_managed_target(
                    executor,
                    &checkout.target,
                    &checkout.source_repository
                )?,
                "managed worktree source repository is unavailable: {}",
                checkout.source_repository.display()
            );
            execute_checked(
                executor,
                managed_target_command(
                    SshTrafficClass::Bulk,
                    &checkout.target,
                    "git",
                    [
                        "clone",
                        "--local",
                        "--dissociate",
                        "--no-checkout",
                        "--",
                        &checkout.source_repository.to_string_lossy(),
                        &staging.to_string_lossy(),
                    ],
                )
                .purpose("clone local source repository for originless legacy worktree resume"),
            )?;
        }

        let base_commit = repository.metadata.base_commit.trim();
        if !base_commit.is_empty() {
            let has_base = executor
                .execute(&managed_git_command(
                    SshTrafficClass::Control,
                    &checkout.target,
                    &staging,
                    ["cat-file", "-e", &format!("{base_commit}^{{commit}}")],
                    "check whether archive base commit is present in fresh clone",
                ))
                .is_ok_and(|output| output.status == 0);
            if !has_base {
                ensure!(
                    path_exists_on_managed_target(
                        executor,
                        &checkout.target,
                        &checkout.source_repository
                    )?,
                    "archive base commit {base_commit} is missing from origin and source repository {} is unavailable",
                    checkout.source_repository.display()
                );
                let _ = executor.execute(&managed_git_command(
                    SshTrafficClass::Bulk,
                    &checkout.target,
                    &staging,
                    [
                        "fetch",
                        "--no-tags",
                        "--",
                        &checkout.source_repository.to_string_lossy(),
                        "+refs/heads/*:refs/mj-legacy-source/*",
                        "+HEAD:refs/mj-legacy-source/HEAD",
                    ],
                    "fetch missing base commit from source repository",
                ));
                let has_base_after_heads = executor
                    .execute(&managed_git_command(
                        SshTrafficClass::Control,
                        &checkout.target,
                        &staging,
                        ["cat-file", "-e", &format!("{base_commit}^{{commit}}")],
                        "recheck archive base commit after fetching source heads",
                    ))
                    .is_ok_and(|output| output.status == 0);
                if !has_base_after_heads {
                    let _ = executor.execute(&managed_git_command(
                        SshTrafficClass::Bulk,
                        &checkout.target,
                        &staging,
                        [
                            "fetch",
                            "--no-tags",
                            "--",
                            &checkout.source_repository.to_string_lossy(),
                            base_commit,
                        ],
                        "fetch archive base commit directly from source repository",
                    ));
                }
                if let Ok(refs) = executor.execute(&managed_git_command(
                    SshTrafficClass::Control,
                    &checkout.target,
                    &staging,
                    [
                        "for-each-ref",
                        "--format=%(refname)",
                        "refs/mj-legacy-source/",
                    ],
                    "list temporary legacy source refs",
                )) && refs.status == 0
                {
                    for refname in String::from_utf8_lossy(&refs.stdout).lines() {
                        let refname = refname.trim();
                        if !refname.is_empty() {
                            let _ = executor.execute(&managed_git_command(
                                SshTrafficClass::Control,
                                &checkout.target,
                                &staging,
                                ["update-ref", "-d", refname],
                                "remove temporary legacy source ref",
                            ));
                        }
                    }
                }
                execute_checked(
                    executor,
                    managed_git_command(
                        SshTrafficClass::Control,
                        &checkout.target,
                        &staging,
                        ["cat-file", "-e", &format!("{base_commit}^{{commit}}")],
                        "verify archive base commit after fetching from source repository",
                    ),
                )?;
            }
        }

        let notices = mj_checkpoint::checkpoint::restore_single_repository_into_clone(
            archive,
            &staging,
            &mj_checkpoint::archive::SystemGit,
        )?;
        execute_checked(
            executor,
            managed_target_command(
                SshTrafficClass::Control,
                &checkout.target,
                "mv",
                [
                    &staging.to_string_lossy(),
                    &checkout.worktree_root.to_string_lossy(),
                ],
            )
            .purpose("publish resumed legacy managed clone"),
        )?;
        moved_to_destination = true;
        if !repository.linked_worktrees.is_empty() {
            let mut args = vec!["worktree".to_owned(), "repair".to_owned()];
            for nested in &repository.linked_worktrees {
                args.push(
                    checkout
                        .worktree_root
                        .join(&nested.metadata.path)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            execute_checked(
                executor,
                managed_git_command(
                    SshTrafficClass::Control,
                    &checkout.target,
                    &checkout.worktree_root,
                    args,
                    "repair nested linked worktrees after publishing legacy managed clone",
                ),
            )?;
        }
        Ok(notices)
    })();
    if let Err(error) = create {
        let cleanup_path = if moved_to_destination {
            &checkout.worktree_root
        } else {
            &staging
        };
        let _ = executor.execute(
            &managed_target_command(
                SshTrafficClass::Control,
                &checkout.target,
                "rm",
                ["-rf", &cleanup_path.to_string_lossy()],
            )
            .purpose("remove failed legacy managed clone"),
        );
        return Err(error);
    }
    create
}

pub(super) fn restore_managed_worktree(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
) -> Result<bool> {
    if managed_worktree_checkout_exists(executor, worktree)? {
        return Ok(false);
    }
    if worktree.kind == ManagedCheckoutKind::Clone {
        create_managed_worktree(executor, worktree, None, PrimaryCheckoutRequirement::Any)?;
        return Ok(true);
    }
    ensure!(
        path_exists_on_managed_target(executor, &worktree.target, &worktree.source_repository)?,
        "managed worktree source repository is unavailable: {}",
        worktree.source_repository.display()
    );
    let branch_ref = format!("refs/heads/{}", worktree.branch);
    let check = managed_git_command(
        SshTrafficClass::Control,
        &worktree.target,
        &worktree.source_repository,
        ["show-ref", "--verify", "--quiet", &branch_ref],
        "check retired managed worktree branch",
    );
    let output = executor.execute(&check)?;
    match output.status {
        0 => {}
        1 => bail!(
            "managed worktree branch is unavailable: {}",
            worktree.branch
        ),
        status => bail!(
            "check retired managed worktree branch failed with status {status}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
    if matches!(worktree.target, ManagedWorktreeTarget::Ssh { .. }) {
        // A remote bare target may already have removed the checkout directory
        // without unregistering the worktree. Clean up only this worktree's
        // stale admin registration before adding the retained branch again.
        remove_missing_linked_worktree_admin(executor, worktree)?;
    }
    let parent = worktree
        .worktree_root
        .parent()
        .context("managed worktree root has no parent")?;
    execute_checked(
        executor,
        managed_target_command(
            SshTrafficClass::Control,
            &worktree.target,
            "mkdir",
            ["-p", &parent.to_string_lossy()],
        )
        .purpose("recreate managed worktree directory"),
    )?;
    execute_checked(
        executor,
        managed_git_command(
            SshTrafficClass::Control,
            &worktree.target,
            &worktree.source_repository,
            [
                "worktree",
                "add",
                "--",
                &worktree.worktree_root.to_string_lossy(),
                &worktree.branch,
            ],
            "restore managed raw-session worktree",
        ),
    )?;
    Ok(true)
}

pub(super) fn ensure_managed_worktree_available(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
) -> Result<()> {
    if path_exists_on_managed_target(executor, &worktree.target, &worktree.worktree_root)? {
        bail!(
            "managed worktree path already exists: {}",
            worktree.worktree_root.display()
        );
    }
    if worktree.kind == ManagedCheckoutKind::Clone {
        return Ok(());
    }
    let branch_ref = format!("refs/heads/{}", worktree.branch);
    let check = managed_git_command(
        SshTrafficClass::Control,
        &worktree.target,
        &worktree.source_repository,
        ["show-ref", "--verify", "--quiet", &branch_ref],
        "check managed worktree branch availability",
    );
    let output = executor.execute(&check)?;
    match output.status {
        0 => bail!(
            "managed worktree branch already exists: {}",
            worktree.branch
        ),
        1 => Ok(()),
        status => bail!(
            "check managed worktree branch availability failed with status {status}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// Check whether the deterministic branch left by this session's earlier
/// raw-to-workspace move can be reattached. A branch with this session's id is
/// session-owned, but an active checkout elsewhere is still a collision: the
/// restore must not make one branch belong to two worktrees.
fn retained_managed_worktree_branch_available(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
) -> Result<bool> {
    if path_exists_on_managed_target(executor, &worktree.target, &worktree.worktree_root)? {
        bail!(
            "managed worktree path already exists: {}",
            worktree.worktree_root.display()
        );
    }
    let branch_ref = format!("refs/heads/{}", worktree.branch);
    let check = managed_git_command(
        SshTrafficClass::Control,
        &worktree.target,
        &worktree.source_repository,
        ["show-ref", "--verify", "--quiet", &branch_ref],
        "check retained managed worktree branch",
    );
    let output = executor.execute(&check)?;
    match output.status {
        1 => Ok(false),
        0 => {
            // Git 2.31 has no `worktree list -z`. A branch line holds a ref
            // name, which cannot contain a newline.
            let worktrees = managed_git_stdout(
                executor,
                &worktree.target,
                &worktree.source_repository,
                ["worktree", "list", "--porcelain"],
                "check retained managed worktree checkout",
            )?;
            let branch_field = format!("branch {branch_ref}");
            if worktrees.lines().any(|field| field == branch_field) {
                bail!(
                    "managed worktree branch is still checked out: {}",
                    worktree.branch
                );
            }
            Ok(true)
        }
        status => bail!(
            "check retained managed worktree branch failed with status {status}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// Preserve the ref that a return-to-local restore is about to reset. The
/// retained `mj/<session>` branch is the source-recovery point; keeping a
/// second ref makes a later commit on that branch recoverable as well.
pub(super) fn preserve_retained_managed_worktree_branch(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
) -> Result<String> {
    let session_id = worktree
        .branch
        .strip_prefix("mj/")
        .context("managed worktree branch is not session-owned")?;
    let branch_ref = format!("refs/heads/{}", worktree.branch);
    let tip = managed_git_stdout(
        executor,
        &worktree.target,
        &worktree.source_repository,
        ["rev-parse", "--verify", &branch_ref],
        "read retained managed worktree branch tip",
    )?;
    let recovery_ref = format!("refs/mj/recovery/{session_id}/{tip}");
    let existing = managed_git_command(
        SshTrafficClass::Control,
        &worktree.target,
        &worktree.source_repository,
        ["show-ref", "--verify", "--quiet", &recovery_ref],
        "check retained managed worktree recovery ref",
    );
    let output = executor.execute(&existing)?;
    match output.status {
        0 => {
            let existing_tip = managed_git_stdout(
                executor,
                &worktree.target,
                &worktree.source_repository,
                ["rev-parse", "--verify", &recovery_ref],
                "verify retained managed worktree recovery ref",
            )?;
            ensure!(
                existing_tip == tip,
                "retained managed worktree recovery ref {recovery_ref} points to {existing_tip}, expected {tip}"
            );
            Ok(recovery_ref)
        }
        1 => {
            execute_checked(
                executor,
                managed_git_command(
                    SshTrafficClass::Control,
                    &worktree.target,
                    &worktree.source_repository,
                    ["update-ref", &recovery_ref, &tip],
                    "preserve retained managed worktree branch",
                ),
            )?;
            Ok(recovery_ref)
        }
        status => bail!(
            "check retained managed worktree recovery ref failed with status {status}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// Whether retiring a managed clone still depends on its launch commit in
/// `source_repository`.
///
/// Legacy checkpoints resume a clone by re-cloning `source_repository` and
/// selecting `base_commit`, so retiring the clone requires that commit to
/// remain reachable there. `OriginDelta` checkpoints seed from `origin` and
/// carry every other required object in their pack, and permanent destroys or
/// rollbacks do not retain a legacy recovery checkpoint, so they do not depend
/// on `source_repository`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloneRetirementPrerequisite {
    LegacySourceCommit,
    None,
}

pub(super) fn clone_retirement_prerequisite_for_checkpoint(
    checkpoint: &mj_core::state::CheckpointMetadata,
) -> CloneRetirementPrerequisite {
    match mj_checkpoint::archive::checkpoint_archive_uses_origin_delta(&checkpoint.archive_path) {
        Ok(true) => CloneRetirementPrerequisite::None,
        _ => CloneRetirementPrerequisite::LegacySourceCommit,
    }
}

/// Remove a managed worktree's checkout and keep its branch.
///
/// A session that moved into a target still checkpoints as a delta against
/// `hel/<session>`, so deleting that branch could let the commits those deltas
/// depend on be collected. The checkout itself is dirty by design; its dirty
/// state has already been carried into the target.
pub(super) fn retire_managed_worktree(
    authority: &super::environment::CheckoutRemoval<'_>,
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
    prerequisite: CloneRetirementPrerequisite,
) -> Result<()> {
    if worktree.kind == ManagedCheckoutKind::Clone
        && prerequisite == CloneRetirementPrerequisite::LegacySourceCommit
    {
        let base = worktree
            .base_commit
            .as_deref()
            .context("managed clone has no source commit")?;
        execute_checked(
            executor,
            managed_git_command(
                SshTrafficClass::Control,
                &worktree.target,
                &worktree.source_repository,
                ["cat-file", "-e", &format!("{base}^{{commit}}")],
                "verify clone recovery prerequisite in source repository",
            ),
        )
        .context("verify clone recovery prerequisite in source repository")?;
    }
    cleanup_managed_worktree(authority, executor, worktree, BranchDisposition::Keep)
}

/// Remove the checkout and its own registration without pruning sibling
/// worktrees. Returns whether the repository is still there to act on at all.
///
/// Every caller has stopped the session's worker first, so once the checkout
/// is gone nothing can build in it again, and mbx is told to drop its build
/// state for that path.
fn remove_managed_worktree_checkout(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
) -> Result<bool> {
    let release = || {
        super::mbx::release::BuildStateRelease::managed_checkout(
            managed_target_ssh(&worktree.target),
            &worktree.worktree_root,
        )
        .run(executor);
    };
    if worktree.kind == ManagedCheckoutKind::Clone {
        if path_exists_on_managed_target(executor, &worktree.target, &worktree.worktree_root)? {
            execute_checked(
                executor,
                managed_target_command(
                    SshTrafficClass::Control,
                    &worktree.target,
                    "rm",
                    ["-rf", "--", &worktree.worktree_root.to_string_lossy()],
                )
                .purpose("remove managed clone after its worker stopped"),
            )?;
            release();
        }
        return Ok(true);
    }
    if !path_exists_on_managed_target(executor, &worktree.target, &worktree.source_repository)? {
        return Ok(false);
    }
    if path_exists_on_managed_target(executor, &worktree.target, &worktree.worktree_root)? {
        // Use a single `--force` so Git removes dirty checkouts while still
        // refusing locked registrations (a second `--force` would override the lock).
        execute_checked(
            executor,
            managed_git_command(
                SshTrafficClass::Control,
                &worktree.target,
                &worktree.source_repository,
                [
                    "worktree",
                    "remove",
                    "--force",
                    "--",
                    &worktree.worktree_root.to_string_lossy(),
                ],
                "remove managed raw-session worktree",
            ),
        )?;
        release();
    } else {
        remove_missing_linked_worktree_admin(executor, worktree)?;
    }
    Ok(true)
}

/// Canonicalize as much of `path` as exists on disk so missing checkouts under
/// symlinked parents (such as `/var` vs `/private/var`) compare equal.
fn canonicalize_missing_path(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut suffix = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            break;
        };
        suffix.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            break;
        };
        existing = parent.to_path_buf();
    }
    let mut resolved = std::fs::canonicalize(&existing).unwrap_or(existing);
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    resolved
}

/// Remove only the single `<common_dir>/worktrees/<name>` admin directory whose
/// `gitdir` points at `worktree.worktree_root/.git`, without running
/// repository-wide `git worktree prune`. Keeps locked registrations untouched.
fn remove_missing_linked_worktree_admin(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
) -> Result<()> {
    match &worktree.target {
        ManagedWorktreeTarget::Local => {
            let common_raw = managed_git_stdout(
                executor,
                &worktree.target,
                &worktree.source_repository,
                ["rev-parse", "--git-common-dir"],
                "locate managed worktree git common directory",
            )?;
            let common_path = PathBuf::from(common_raw);
            let common_dir = if common_path.is_relative() {
                worktree.source_repository.join(common_path)
            } else {
                common_path
            };
            let worktrees_dir = common_dir.join("worktrees");
            if !worktrees_dir.is_dir() {
                return Ok(());
            }
            let target_root = canonicalize_missing_path(&worktree.worktree_root);
            for entry in std::fs::read_dir(&worktrees_dir)
                .with_context(|| format!("read worktrees directory {}", worktrees_dir.display()))?
            {
                let entry = entry?;
                let admin_dir = entry.path();
                if !admin_dir.is_dir() {
                    continue;
                }
                let gitdir_file = admin_dir.join("gitdir");
                let Ok(raw_gitdir) = std::fs::read_to_string(&gitdir_file) else {
                    continue;
                };
                let trimmed = raw_gitdir.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let gitdir_path = PathBuf::from(trimmed);
                let resolved_gitdir = if gitdir_path.is_relative() {
                    admin_dir.join(gitdir_path)
                } else {
                    gitdir_path
                };
                let Some(checkout_root) = resolved_gitdir.parent() else {
                    continue;
                };
                if canonicalize_missing_path(checkout_root) == target_root {
                    if admin_dir.join("locked").exists() {
                        tracing::warn!(
                            worktree_root = %worktree.worktree_root.display(),
                            admin_dir = %admin_dir.display(),
                            "keeping locked managed worktree registration after checkout removal"
                        );
                        return Ok(());
                    }
                    std::fs::remove_dir_all(&admin_dir).with_context(|| {
                        format!(
                            "remove managed worktree admin directory {}",
                            admin_dir.display()
                        )
                    })?;
                    return Ok(());
                }
            }
            Ok(())
        }
        ManagedWorktreeTarget::Ssh { .. } => {
            const SCRIPT: &str = r#"set -eu
src="$1"
target="$2"
common=$(git -C "$src" rev-parse --git-common-dir)
case "$common" in
  /*) ;;
  *) common="$src/$common" ;;
esac
wt_dir="$common/worktrees"
[ -d "$wt_dir" ] || exit 0
norm_path() {
  p="$1"
  suffix=""
  while [ -n "$p" ] && [ "$p" != "/" ] && [ ! -d "$p" ]; do
    base=$(basename "$p")
    suffix="/$base$suffix"
    p=$(dirname "$p")
  done
  if [ -d "$p" ]; then
    p=$(cd "$p" 2>/dev/null && pwd -P) || p="$p"
  fi
  case "$p" in
    /) printf '%s' "${suffix:-/}" ;;
    *) printf '%s%s' "$p" "$suffix" ;;
  esac
}
norm_target=$(norm_path "$target")
for admin in "$wt_dir"/*; do
  [ -d "$admin" ] && [ -f "$admin/gitdir" ] || continue
  IFS= read -r gd < "$admin/gitdir" || [ -n "$gd" ] || continue
  [ -n "$gd" ] || continue
  case "$gd" in
    /*) ;;
    *) gd="$admin/$gd" ;;
  esac
  parent=$(dirname "$gd")
  if [ "$(norm_path "$parent")" = "$norm_target" ]; then
    if [ -e "$admin/locked" ]; then
      printf 'LOCKED\n'
      exit 0
    fi
    rm -rf -- "$admin"
    exit 0
  fi
done
"#;
            let output = execute_checked(
                executor,
                managed_target_command(
                    SshTrafficClass::Control,
                    &worktree.target,
                    "sh",
                    [
                        "-c",
                        SCRIPT,
                        "--",
                        &worktree.source_repository.to_string_lossy(),
                        &worktree.worktree_root.to_string_lossy(),
                    ],
                )
                .purpose("remove missing managed worktree registration"),
            )?;
            if String::from_utf8_lossy(&output.stdout).trim() == "LOCKED" {
                tracing::warn!(
                    worktree_root = %worktree.worktree_root.display(),
                    "keeping locked remote managed worktree registration after checkout removal"
                );
            }
            Ok(())
        }
    }
}

/// Whether the session branch is contained in a branch that is not a Mjolnir
/// session branch, so deleting it loses no commits. `Ok(None)` means the
/// source repository is gone and there is nothing to answer about.
///
/// This is git's own meaning of "merged": the branch tip is an ancestor of
/// another ref. A squash merge or a rebase rewrites the commits, so it does
/// not count and the branch is kept.
fn managed_branch_is_merged(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
) -> Result<Option<bool>> {
    if !path_exists_on_managed_target(executor, &worktree.target, &worktree.source_repository)? {
        return Ok(None);
    }
    let branch_ref = format!("refs/heads/{}", worktree.branch);
    let refs = managed_git_stdout(
        executor,
        &worktree.target,
        &worktree.source_repository,
        [
            "for-each-ref",
            "--contains",
            &branch_ref,
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
        ],
        "list the branches containing a managed worktree branch",
    )?;
    Ok(Some(refs.lines().any(containing_ref_is_not_a_session)))
}

/// A ref that proves the session branch's commits live somewhere else: any
/// branch outside `refs/heads/mj/`, including a remote-tracking branch, since
/// work merged upstream and fetched is merged. A remote's symbolic `HEAD` is
/// not a branch of its own and never counts.
fn containing_ref_is_not_a_session(reference: &str) -> bool {
    let reference = reference.trim();
    let remote_head = reference.starts_with("refs/remotes/") && reference.ends_with("/HEAD");
    !reference.is_empty() && !reference.starts_with("refs/heads/mj/") && !remote_head
}

/// Remove a managed worktree's checkout, and its branch only when the caller
/// asks for that. The branch can hold work the user still wants, so deleting
/// it is always an explicit decision; see [`BranchDisposition`].
pub(super) fn cleanup_managed_worktree(
    authority: &super::environment::CheckoutRemoval<'_>,
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
    branch: BranchDisposition,
) -> Result<()> {
    authority.covers(worktree)?;
    if !remove_managed_worktree_checkout(executor, worktree)? {
        return Ok(());
    }
    if worktree.kind == ManagedCheckoutKind::Clone {
        return remove_empty_managed_worktree_directories(executor, worktree);
    }
    if branch == BranchDisposition::Keep {
        return remove_empty_managed_worktree_directories(executor, worktree);
    }
    cleanup_managed_worktree_branch(executor, worktree, branch)?;
    remove_empty_managed_worktree_directories(executor, worktree)
}

/// Retained branch metadata is independent of a reclaimed checkout path.
/// Git refuses deleting a branch which a replacement checkout has in use.
pub(super) fn cleanup_managed_worktree_branch(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
    branch: BranchDisposition,
) -> Result<()> {
    if worktree.kind == ManagedCheckoutKind::Clone
        || branch == BranchDisposition::Keep
        || !path_exists_on_managed_target(executor, &worktree.target, &worktree.source_repository)?
    {
        return Ok(());
    }
    let branch_ref = format!("refs/heads/{}", worktree.branch);
    let check = managed_git_command(
        SshTrafficClass::Control,
        &worktree.target,
        &worktree.source_repository,
        ["show-ref", "--verify", "--quiet", &branch_ref],
        "check managed worktree branch",
    );
    let output = executor.execute(&check)?;
    let present = match output.status {
        0 => true,
        1 => false,
        status => bail!(
            "check managed worktree branch failed with status {status}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    };
    let delete = match branch {
        BranchDisposition::Delete => present,
        BranchDisposition::DeleteIfMerged if present => {
            let merged = managed_branch_is_merged(executor, worktree)?;
            let delete = merged == Some(true);
            tracing::info!(
                branch = %worktree.branch,
                delete,
                reason = match merged {
                    Some(true) => "another branch already contains its commits",
                    Some(false) => "it holds commits no other branch contains",
                    None => "its repository is gone",
                },
                "archiving decided what to do with a session branch"
            );
            delete
        }
        BranchDisposition::DeleteIfMerged | BranchDisposition::Keep => false,
    };
    if delete {
        execute_checked(
            executor,
            managed_git_command(
                SshTrafficClass::Control,
                &worktree.target,
                &worktree.source_repository,
                ["branch", "-D", "--", &worktree.branch],
                "delete managed raw-session branch",
            ),
        )?;
    }
    Ok(())
}

fn remove_empty_managed_worktree_directories(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
) -> Result<()> {
    let hel = target_path::join(&worktree.source_repository, Path::new(".mj"));
    let worktrees = target_path::join(
        &hel,
        Path::new(match worktree.kind {
            ManagedCheckoutKind::Worktree => "worktrees",
            ManagedCheckoutKind::Clone => "clones",
        }),
    );
    match &worktree.target {
        ManagedWorktreeTarget::Local => {
            for directory in [&worktrees, &hel] {
                match std::fs::remove_dir(directory) {
                    Ok(()) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        ManagedWorktreeTarget::Ssh { .. } => {
            let command = managed_target_command(
                SshTrafficClass::Control,
                &worktree.target,
                "rmdir",
                ["--", &worktrees.to_string_lossy(), &hel.to_string_lossy()],
            )
            .purpose("remove empty managed worktree directories");
            let _ = executor.execute(&command)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
