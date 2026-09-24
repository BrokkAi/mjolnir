//! Managed worktrees and raw-to-workspace project conversion.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use mj_core::config::{Config, ProjectBundle, TargetTemplate};
use mj_core::local_git::canonical_repository;
use mj_core::state::{
    ManagedCheckoutKind, ManagedWorktree, ManagedWorktreeOptions, ManagedWorktreeTarget,
    ProjectSourceIdentity, SessionRecord,
};

use crate::targets::{
    self, CancellableProcessExecutor, CommandExecutor, CommandOutput, CommandSpec, SshTarget,
};
pub(super) use mj_client::target::managed_worktree_target;
pub use mj_client::target::{ResumePlan, resume_compatibility};

use super::{BranchDisposition, Controller, execute_checked, now};

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
        mj_core::path_input::validate_absolute_input(directory)?;
        let directory = self.resolve_input_path(target_id, directory, executor)?;
        self.validate_project_directory(target_id, &directory, executor)?;
        Ok(directory)
    }

    /// Verify a bare project before leaving the project-directory dialog.
    pub fn validate_project_directory(
        &self,
        target_id: &str,
        directory: &Path,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
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
                if local_project_repository(directory, executor)?.is_none() {
                    return Ok(());
                }
                let output = executor.execute(
                    &CommandSpec::new(
                        "git",
                        [
                            "-C",
                            &directory.to_string_lossy(),
                            "rev-parse",
                            "--verify",
                            "HEAD",
                        ],
                    )
                    .purpose("verify local bare Git project"),
                )?;
                ensure!(
                    output.status == 0
                        && !String::from_utf8_lossy(&output.stdout).trim().is_empty(),
                    "project directory has no valid Git HEAD: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                Ok(())
            }
            TargetTemplate::SshBare { ssh, .. } => {
                targets::validate_bare_project_directory(
                    &SshTarget::from(ssh),
                    directory,
                    executor,
                )?;
                mj_core::remote_git::resolve_local_repository(
                    directory,
                    &RemoteGitExecutor {
                        executor,
                        ssh: SshTarget::from(ssh),
                    },
                )?;
                Ok(())
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
        let Some(directory) = session.project_directory.as_deref() else {
            return Ok(session.project_source(&self.config));
        };
        let (target, origin_directory) = match &session.managed_worktree {
            // The source repository is the durable owner of a linked
            // worktree's shared Git configuration and remains available while
            // a stopped session's checkout is retired.
            Some(worktree) => (
                worktree.target.clone(),
                worktree.source_repository.as_path(),
            ),
            None => (
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
        };
        let output = executor.execute(&managed_git_command(
            &target,
            origin_directory,
            ["config", "--get", "remote.origin.url"],
            "resolve project Git origin",
        ))?;
        match output.status {
            0 => {
                let origin =
                    String::from_utf8(output.stdout).context("project Git origin was not UTF-8")?;
                if let Some(identity) = ProjectSourceIdentity::git_remote(origin.trim()) {
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
        executor: &impl CommandExecutor,
    ) -> Result<WorkspaceToRawConversion> {
        let bundle = self
            .config
            .bundles
            .get(&session.bundle_id)
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
            worktree_root: source.join(".mj").join("worktrees").join(&session.id),
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
            retained_managed_worktree_branch_available(executor, &worktree)?;
        if !reuse_existing_branch {
            let (branch, remote_branch) = managed_clone_starting_branch(
                executor,
                &worktree.target,
                source,
                session.launch_branch.as_deref(),
            )?;
            worktree.kind = ManagedCheckoutKind::Clone;
            worktree.worktree_root = source.join(".mj").join("clones").join(&session.id);
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

    pub(super) fn prepare_managed_raw_worktree(
        &mut self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<bool> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        let Some(selected) = session.project_directory.as_deref() else {
            return Ok(false);
        };
        if session.managed_worktree.is_some() {
            return Ok(false);
        }
        if session.create_managed_worktree == Some(false) {
            return Ok(false);
        }
        let template = self
            .config
            .targets
            .get(&session.target_template_id)
            .context("raw session target template disappeared during provisioning")?;
        if matches!(template, TargetTemplate::SshBare { .. }) {
            self.validate_project_directory(&session.target_template_id, selected, executor)?;
        }
        let target = managed_worktree_target(template)?;
        if matches!(target, ManagedWorktreeTarget::Local)
            && local_project_repository(selected, executor)?.is_none()
        {
            // A requested launch base asks for the same worktree an explicit
            // request does, so it must fail here rather than launch without
            // one and silently ignore the base.
            ensure!(
                session.create_managed_worktree != Some(true) && session.launch_base.is_none(),
                "managed worktree creation requires a Git project"
            );
            return Ok(false);
        }
        let inspection = inspect_raw_project(executor, &target, selected)?;
        if !inspection.primary_checkout
            && session.create_managed_worktree != Some(true)
            && session.launch_base.is_none()
        {
            return Ok(false);
        }
        let relative_directory = inspection
            .source_project_directory
            .strip_prefix(&inspection.source_repository)
            .context("raw project directory is outside its repository")?
            .to_path_buf();
        let worktree_root = inspection
            .source_repository
            .join(".mj")
            .join("clones")
            .join(session_id);
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
        record.project_directory = Some(worktree_root.join(relative_directory));
        record.managed_worktree = Some(managed.clone());
        record.updated_at = now();
        self.persist_session_state(session_id)?;
        create_managed_worktree(
            executor,
            &managed,
            inspection.upstream.as_deref(),
            PrimaryCheckoutRequirement::Clean,
        )?;
        Ok(true)
    }

    fn cleanup_new_session_worktree(
        &self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        let Some(worktree) = self
            .state
            .sessions
            .get(session_id)
            .and_then(|session| session.managed_worktree.as_ref())
        else {
            return Ok(());
        };
        // A session that never started has a branch Mjolnir just created and
        // nobody has worked on, so the rollback takes the branch too.
        cleanup_managed_worktree(executor, worktree, BranchDisposition::Delete)
    }

    pub(super) fn cleanup_new_session_worktree_after_failure(
        &self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        if executor.cancellation_requested() {
            let cleanup_executor =
                CancellableProcessExecutor::with_timeout(Duration::from_secs(15));
            self.cleanup_new_session_worktree(session_id, &cleanup_executor)
        } else {
            self.cleanup_new_session_worktree(session_id, executor)
        }
    }
}

/// Reuse the same Git configuration resolver on a remote bare host.
struct RemoteGitExecutor<'a, E> {
    executor: &'a E,
    ssh: SshTarget,
}

impl<E: CommandExecutor> CommandExecutor for RemoteGitExecutor<'_, E> {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        let mut arguments = vec!["env".to_owned()];
        arguments.extend(
            command
                .env
                .iter()
                .map(|(key, value)| format!("{key}={value}")),
        );
        arguments.push(command.program.clone());
        arguments.extend(command.args.clone());
        self.executor
            .execute(&crate::targets::ssh_command(&self.ssh, arguments).purpose(&command.purpose))
    }

    fn cancellation_requested(&self) -> bool {
        self.executor.cancellation_requested()
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
                target,
                repository,
                ["config", "--get", "remote.origin.url"],
                "inspect origin remote",
            ))?;
            if origin.status == 0 {
                let remote = managed_git_stdout(
                    executor,
                    target,
                    repository,
                    ["ls-remote", "--symref", "origin", "HEAD"],
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

fn managed_target_ssh(target: &ManagedWorktreeTarget) -> Option<SshTarget> {
    match target {
        ManagedWorktreeTarget::Local => None,
        ManagedWorktreeTarget::Ssh {
            destination,
            ssh_args,
        } => Some(SshTarget {
            destination: destination.clone(),
            ssh_args: ssh_args.clone(),
        }),
    }
}

fn managed_target_command(
    target: &ManagedWorktreeTarget,
    program: &str,
    args: impl IntoIterator<Item = impl AsRef<str>>,
) -> CommandSpec {
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect::<Vec<_>>();
    match managed_target_ssh(target) {
        None => CommandSpec::new(program, args),
        Some(ssh) => {
            let mut remote = vec![program.to_owned()];
            remote.extend(args);
            crate::targets::ssh_command(&ssh, remote)
        }
    }
}

pub(super) fn managed_git_command(
    target: &ManagedWorktreeTarget,
    directory: &Path,
    args: impl IntoIterator<Item = impl AsRef<str>>,
    purpose: impl Into<String>,
) -> CommandSpec {
    let mut command_args = vec!["-C".to_owned(), directory.to_string_lossy().into_owned()];
    command_args.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
    managed_target_command(target, "git", command_args).purpose(purpose)
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
    let command = managed_git_command(target, directory, args, purpose);
    command_stdout(executor.execute(&command)?, purpose)
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
        "--path-format=absolute".into(),
        "--show-toplevel".into(),
    ];
    let top_level = match target {
        ManagedWorktreeTarget::Local => {
            let mut command = CommandSpec::new("git", args);
            command.env.insert("LC_ALL".into(), "C".into());
            command
        }
        ManagedWorktreeTarget::Ssh { .. } => managed_target_command(
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
    if root.as_os_str().is_empty() {
        bail!("resolve project Git root returned an empty path");
    }

    let common = PathBuf::from(managed_git_stdout(
        executor,
        target,
        directory,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        "resolve project Git common directory",
    )?);
    if common.file_name() == Some(std::ffi::OsStr::new(".git"))
        && let Some(main_root) = common.parent()
    {
        return Ok(Some(main_root.to_path_buf()));
    }
    Ok(Some(root))
}

/// Inspect a local launch directory using the same Git error handling and
/// linked-worktree identity as existing sessions.
pub fn local_project_repository(
    directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<Option<PathBuf>> {
    resolve_git_root(&ManagedWorktreeTarget::Local, directory, executor)
}

/// Which checkout each still-empty target repository is seeded from, or `None`
/// when this connect must not seed at all. A converting resume carries the
/// session's own checkout; every other seed comes from the bundle's local path.
/// Reshape a raw session's record for the workspace target it is moving into.
pub(super) fn apply_raw_to_workspace(
    record: &mut SessionRecord,
    conversion: &RawToWorkspaceConversion,
) {
    record.project_directory = None;
    record.managed_worktree = None;
    record.bundle_id.clone_from(&conversion.bundle_id);
}

/// A resume that changes how a session is represented, resolved before the
/// session record or the configuration changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ResumeConversion {
    RawToWorkspace(RawToWorkspaceConversion),
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
    record.managed_worktree = Some(conversion.worktree.clone());
}

/// Everything a raw-to-workspace resume needs, resolved before the session
/// record or the configuration changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RawToWorkspaceConversion {
    /// The checkout whose branch, head commit, and dirty state move into the
    /// target. For a managed session this is the session's own worktree, not
    /// the user's primary checkout.
    pub(super) checkout: PathBuf,
    /// The source repository represented by the bundle's local path.
    pub(super) repository: PathBuf,
    /// Where the converted workspace fetches from and pushes to. An isolated
    /// workspace always clones from a network remote, so the checkout's own
    /// remote becomes the converted session's provenance.
    pub(super) source: mj_core::remote_git::NetworkGitSource,
    pub(super) bundle_id: String,
    /// Set when the configuration does not already describe this checkout.
    pub(super) new_bundle: Option<ProjectBundle>,
    /// Removed once the target holds the checkout, and only then.
    pub(super) retire: Option<ManagedWorktree>,
}

/// Resolve where a raw session's checkout lives and which bundle will stand in
/// for it. Reads Git; changes nothing.
pub(super) fn plan_raw_to_workspace(
    session: &SessionRecord,
    config: &Config,
    executor: &impl CommandExecutor,
) -> Result<RawToWorkspaceConversion> {
    let project_directory = session
        .project_directory
        .as_deref()
        .context("a raw session has no project directory")?;
    // The checkpoint describes the session's directory as if it were the
    // repository root, so only a whole checkout can move. Each branch checks
    // this against paths from one domain: the record's own paths for a managed
    // worktree, Git's canonical paths for an inspected checkout — the record
    // may reach the same checkout through a symlink (macOS temp directories).
    let (checkout, repository, retire) = match &session.managed_worktree {
        Some(worktree) => {
            ensure!(
                worktree.worktree_root == project_directory,
                "{} is a subdirectory of its checkout; only a whole checkout can move into a target",
                project_directory.display()
            );
            (
                worktree.worktree_root.clone(),
                worktree.source_repository.clone(),
                Some(worktree.clone()),
            )
        }
        None => {
            let inspection =
                inspect_raw_project(executor, &ManagedWorktreeTarget::Local, project_directory)?;
            ensure!(
                inspection.source_project_directory == inspection.source_repository,
                "{} is a subdirectory of its checkout; only a whole checkout can move into a target",
                project_directory.display()
            );
            let repository = canonical_repository(&inspection.source_repository)?;
            (inspection.source_repository, repository, None)
        }
    };
    // The archive names the session's directory as the repository destination,
    // and the restored harness session points at that path inside the target.
    // The bundle has to put the checkout in the same place.
    let destination = PathBuf::from(
        project_directory
            .file_name()
            .context("a raw project directory cannot be the filesystem root")?,
    );
    let (bundle_id, new_bundle) =
        converted_raw_bundle(config, &session.bundle_id, &repository, &destination);
    // An isolated workspace is always a fresh network clone, so a checkout
    // with no network remote cannot become one. Resolve it here, while nothing
    // has changed yet, and say what to do about it.
    let source = mj_core::remote_git::resolve_local_repository(&checkout, executor).with_context(
        || {
            format!(
                "{} has no network Git remote; add one (for example `git remote add origin <url>`) or resume this session on a bare target",
                checkout.display()
            )
        },
    )?;
    Ok(RawToWorkspaceConversion {
        checkout,
        repository,
        source,
        bundle_id,
        new_bundle,
        retire,
    })
}

/// The bundle a converted raw session references: one the configuration already
/// has for exactly this checkout, or a new one for the caller to install.
/// Reusing a match keeps a retried conversion from piling up bundles.
fn converted_raw_bundle(
    config: &Config,
    session_bundle_id: &str,
    repository: &Path,
    destination: &Path,
) -> (String, Option<ProjectBundle>) {
    let describes_checkout = |bundle: &ProjectBundle| {
        bundle.repositories.len() == 1
            && bundle.repositories[0].github.is_none()
            && bundle.repositories[0].local.as_deref() == Some(repository)
            && bundle.repositories[0].destination == destination
    };
    if config
        .bundles
        .get(session_bundle_id)
        .is_some_and(describes_checkout)
    {
        return (session_bundle_id.to_owned(), None);
    }
    if let Some((id, _)) = config
        .bundles
        .iter()
        .find(|(_, bundle)| describes_checkout(bundle))
    {
        return (id.clone(), None);
    }
    let name = repository
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let id = crate::import::unique_bundle_id(config, &crate::import::setup_style_id(&name));
    let bundle = ProjectBundle {
        primary_repo: id.clone(),
        repositories: vec![mj_core::config::ProjectRepository {
            id: id.clone(),
            github: None,
            local: Some(repository.to_path_buf()),
            destination: destination.to_path_buf(),
            git_ref: None,
        }],
    };
    (id, Some(bundle))
}

/// The repository id a converted raw session's archive uses. A raw checkpoint
/// has always described the session's directory as one repository.
const RAW_CONVERSION_REPOSITORY_ID: &str = "project";

/// Snapshot the host checkout as the repository content an isolated workspace
/// arrives with: commits that are on no origin ref, plus staged, unstaged, and
/// untracked work.
///
/// The metadata carries the checkout's own network remote, so the container
/// clones real provenance and its later checkpoints behave like any other
/// workspace session's.
pub(super) fn raw_checkout_snapshot(
    checkout: &Path,
    source: &mj_core::remote_git::NetworkGitSource,
    destination: &Path,
    git: &dyn mj_checkpoint::archive::GitCommandRunner,
    managed_clone: bool,
) -> Result<mj_checkpoint::archive::RepositorySnapshot> {
    // Bundling "everything not on origin" only works when origin refs exist:
    // every bundle prerequisite then sits on the remote the container clones.
    mj_checkpoint::checkpoint::repair_origin_refs(git, checkout, RAW_CONVERSION_REPOSITORY_ID)?;
    mj_checkpoint::checkpoint::reject_dirty_submodules(git, checkout)
        .with_context(|| format!("checkout {}", checkout.display()))?;
    let boundary = origin_boundary_commit(git, checkout)?;
    let history = if managed_clone {
        mj_checkpoint::archive::GitHistoryMode::CloneFrom(
            boundary
                .clone()
                .context("managed clone has no origin boundary commit")?,
        )
    } else {
        mj_checkpoint::archive::GitHistoryMode::SessionDelta
    };
    let mut snapshot = mj_checkpoint::archive::collect_git_snapshot(
        git,
        checkout,
        &mj_checkpoint::archive::GitCollectionSpec {
            id: RAW_CONVERSION_REPOSITORY_ID.to_owned(),
            relative_destination: destination.to_path_buf(),
            history,
            origin_override: None,
        },
    )
    .with_context(|| format!("snapshot the checkout at {}", checkout.display()))?;
    // The resolved remote, not whatever `origin` happens to be: the checkout's
    // branch may track another remote. Credentials stay out of the archive.
    snapshot.metadata.origin =
        mj_checkpoint::archive::redact_origin_credentials(&source.fetch_url)?;
    snapshot.metadata.push_urls = source
        .push_urls
        .iter()
        .map(|url| mj_checkpoint::archive::redact_origin_credentials(url))
        .collect::<Result<Vec<_>>>()?;
    snapshot.metadata.remote_workspace = true;
    snapshot.metadata.base_commit =
        boundary.unwrap_or_else(|| snapshot.metadata.head_commit.clone());
    Ok(snapshot)
}

/// The newest commit the checkout shares with `origin`, which is where a
/// converted workspace measures its own session delta from. `None` when HEAD
/// is already on an origin ref, leaving no boundary to report.
fn origin_boundary_commit(
    git: &dyn mj_checkpoint::archive::GitCommandRunner,
    checkout: &Path,
) -> Result<Option<String>> {
    let listed = git_runner_stdout(
        git,
        checkout,
        [
            "rev-list",
            "--boundary",
            "HEAD",
            "--not",
            "--remotes=origin",
        ],
        "list commits outside origin",
    )?;
    // `--boundary` marks the excluded parents of the listed commits with `-`,
    // and lists them after the commits themselves.
    Ok(listed
        .lines()
        .filter_map(|line| line.strip_prefix('-'))
        .map(|commit| commit.trim().to_owned())
        .find(|commit| !commit.is_empty()))
}

fn git_runner_stdout(
    git: &dyn mj_checkpoint::archive::GitCommandRunner,
    repository: &Path,
    args: impl IntoIterator<Item = impl AsRef<str>>,
    purpose: &str,
) -> Result<String> {
    let output = git.run(
        repository,
        &mj_checkpoint::archive::GitCommand {
            arguments: args
                .into_iter()
                .map(|argument| std::ffi::OsString::from(argument.as_ref()))
                .collect(),
            stdin: Vec::new(),
            env: Vec::new(),
        },
    )?;
    command_stdout(
        CommandOutput {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        },
        purpose,
    )
}

/// Describe a raw-to-workspace conversion for a person to confirm. Reads Git
/// and asks the remote for its default branch; changes nothing.
pub(super) fn raw_conversion_preview(
    session: &SessionRecord,
    conversion: &RawToWorkspaceConversion,
    executor: &impl CommandExecutor,
) -> Result<mj_core::state::RawConversionPreview> {
    let checkout = conversion.checkout.as_path();
    // A dirty submodule cannot be captured, so say so now rather than failing
    // after the session has been stopped.
    reject_dirty_submodules_in_checkout(executor, checkout)?;
    let default_branch = mj_core::remote_git::default_branch(&conversion.source, executor)?;
    let position = read_checkout_position(executor, &ManagedWorktreeTarget::Local, checkout)?;
    let unpushed_commits = unpushed_commit_count(executor, checkout)?;
    let dirty = dirty_file_counts(executor, checkout)?;
    // The archive names the session's own directory, which is where the
    // restored harness session looks for its files inside the target.
    let directory = session
        .project_directory
        .as_deref()
        .context("a raw session has no project directory")?
        .file_name()
        .context("a raw project directory cannot be the filesystem root")?;
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
        destination: container_workspace.join(directory),
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

fn reject_dirty_submodules_in_checkout(
    executor: &impl CommandExecutor,
    checkout: &Path,
) -> Result<()> {
    let listed = managed_git_stdout(
        executor,
        &ManagedWorktreeTarget::Local,
        checkout,
        [
            "submodule",
            "foreach",
            "--recursive",
            "--quiet",
            "git status --porcelain",
        ],
        "inspect submodules",
    )?;
    ensure!(
        listed.trim().is_empty(),
        "{} has a dirty submodule, which cannot move into a target; commit or discard the submodule's changes first",
        checkout.display()
    );
    Ok(())
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
    let fetch = managed_git_command(
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
pub(super) fn raw_checkout_position(
    session: &SessionRecord,
    config: &Config,
    project_directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<CheckoutPosition> {
    let target = match &session.managed_worktree {
        Some(worktree) => worktree.target.clone(),
        None => {
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
    let repository = PathBuf::from(managed_git_stdout(
        executor,
        target,
        selected,
        ["rev-parse", "--path-format=absolute", "--show-toplevel"],
        "resolve raw project repository root",
    )?);
    let prefix = managed_git_stdout(
        executor,
        target,
        selected,
        ["rev-parse", "--show-prefix"],
        "resolve raw project relative directory",
    )?;
    let git_dir = PathBuf::from(managed_git_stdout(
        executor,
        target,
        selected,
        ["rev-parse", "--absolute-git-dir"],
        "resolve raw project Git directory",
    )?);
    let common_git_dir = PathBuf::from(managed_git_stdout(
        executor,
        target,
        selected,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        "resolve raw project common Git directory",
    )?);
    let branch_command = managed_git_command(
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
        source_project_directory: repository.join(prefix),
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
    let exclude_path = PathBuf::from(managed_git_stdout(
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
    )?);
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

pub(crate) fn path_exists_on_managed_target(
    executor: &impl CommandExecutor,
    target: &ManagedWorktreeTarget,
    path: &Path,
) -> Result<bool> {
    match target {
        ManagedWorktreeTarget::Local => path
            .try_exists()
            .with_context(|| format!("check managed project path {}", path.display())),
        ManagedWorktreeTarget::Ssh { .. } => {
            let command = managed_target_command(target, "test", ["-e", &path.to_string_lossy()])
                .purpose("check managed worktree path");
            let output = executor.execute(&command)?;
            match output.status {
                0 => Ok(true),
                1 => Ok(false),
                status => bail!(
                    "check managed worktree path failed with status {status}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            }
        }
    }
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
        managed_target_command(&worktree.target, "mkdir", ["-p", &parent.to_string_lossy()])
            .purpose("create managed worktree directory"),
    )?;
    execute_checked(
        executor,
        managed_git_command(
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
                &worktree.target,
                &worktree.worktree_root,
                ["branch", "--set-upstream-to", upstream, &worktree.branch],
                "set managed worktree branch upstream",
            ),
        )?;
    }
    Ok(())
}

fn create_managed_clone(executor: &impl CommandExecutor, checkout: &ManagedWorktree) -> Result<()> {
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
        managed_target_command(&checkout.target, "mkdir", ["-p", &parent.to_string_lossy()])
            .purpose("create managed clone parent"),
    )?;
    let create = (|| -> Result<()> {
        execute_checked(
            executor,
            managed_target_command(
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
            &checkout.target,
            &checkout.source_repository,
            ["config", "--get", "remote.origin.url"],
            "read source origin URL",
        ))?;
        execute_checked(
            executor,
            managed_git_command(
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
        execute_checked(
            executor,
            managed_git_command(
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
        execute_checked(
            executor,
            managed_target_command(
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
        Ok(())
    })();
    if create.is_err() && path_exists_on_managed_target(executor, &checkout.target, &staging)? {
        execute_checked(
            executor,
            managed_target_command(
                &checkout.target,
                "rm",
                ["-rf", "--", &staging.to_string_lossy()],
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
        [
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/remotes/origin",
        ],
        "read cached origin branches",
    )?;
    for line in refs.lines() {
        let (name, oid) = line
            .split_once(' ')
            .context("malformed source remote ref")?;
        if name == "refs/remotes/origin/HEAD" {
            continue;
        }
        execute_checked(
            executor,
            managed_git_command(
                &checkout.target,
                staging,
                ["update-ref", name, oid],
                "preserve cached origin branch",
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
                &checkout.target,
                staging,
                ["config", "--local", "--add", key, value],
                "preserve Git identity and local preferences",
            ),
        )?;
    }
    let source_exclude = PathBuf::from(managed_git_stdout(
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
    )?);
    if path_exists_on_managed_target(executor, &checkout.target, &source_exclude)? {
        let clone_exclude = PathBuf::from(managed_git_stdout(
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
        )?);
        execute_checked(
            executor,
            managed_target_command(
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
    // A remote bare target may already have removed the checkout directory.
    // Prune its stale registration before adding the retained branch again.
    execute_checked(
        executor,
        managed_git_command(
            &worktree.target,
            &worktree.source_repository,
            ["worktree", "prune"],
            "prune retired managed worktree metadata",
        ),
    )?;
    let parent = worktree
        .worktree_root
        .parent()
        .context("managed worktree root has no parent")?;
    execute_checked(
        executor,
        managed_target_command(&worktree.target, "mkdir", ["-p", &parent.to_string_lossy()])
            .purpose("recreate managed worktree directory"),
    )?;
    execute_checked(
        executor,
        managed_git_command(
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

fn ensure_managed_worktree_available(
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
        &worktree.target,
        &worktree.source_repository,
        ["show-ref", "--verify", "--quiet", &branch_ref],
        "check retained managed worktree branch",
    );
    let output = executor.execute(&check)?;
    match output.status {
        1 => Ok(false),
        0 => {
            let worktrees = managed_git_stdout(
                executor,
                &worktree.target,
                &worktree.source_repository,
                ["worktree", "list", "--porcelain", "-z"],
                "check retained managed worktree checkout",
            )?;
            let branch_field = format!("branch {branch_ref}");
            if worktrees.split('\0').any(|field| field == branch_field) {
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

/// Remove a managed worktree's checkout and keep its branch.
///
/// A session that moved into a target still checkpoints as a delta against
/// `hel/<session>`, so deleting that branch could let the commits those deltas
/// depend on be collected. The checkout itself is dirty by design; its dirty
/// state has already been carried into the target.
pub(super) fn retire_managed_worktree(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
) -> Result<()> {
    if worktree.kind == ManagedCheckoutKind::Clone {
        let base = worktree
            .base_commit
            .as_deref()
            .context("managed clone has no source commit")?;
        execute_checked(
            executor,
            managed_git_command(
                &worktree.target,
                &worktree.source_repository,
                ["cat-file", "-e", &format!("{base}^{{commit}}")],
                "verify clone recovery prerequisite in source repository",
            ),
        )?;
    }
    cleanup_managed_worktree(executor, worktree, BranchDisposition::Keep)
}

/// Remove the checkout and prune its metadata. Returns whether the repository
/// is still there to act on at all.
fn remove_managed_worktree_checkout(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
) -> Result<bool> {
    if !path_exists_on_managed_target(executor, &worktree.target, &worktree.source_repository)? {
        return Ok(false);
    }
    if worktree.kind == ManagedCheckoutKind::Clone {
        if path_exists_on_managed_target(executor, &worktree.target, &worktree.worktree_root)? {
            execute_checked(
                executor,
                managed_target_command(
                    &worktree.target,
                    "rm",
                    ["-rf", "--", &worktree.worktree_root.to_string_lossy()],
                )
                .purpose("remove managed clone after its worker stopped"),
            )?;
        }
        return Ok(true);
    }
    if path_exists_on_managed_target(executor, &worktree.target, &worktree.worktree_root)? {
        execute_checked(
            executor,
            managed_git_command(
                &worktree.target,
                &worktree.source_repository,
                [
                    "worktree",
                    "remove",
                    "--force",
                    &worktree.worktree_root.to_string_lossy(),
                ],
                "remove managed raw-session worktree",
            ),
        )?;
    }
    execute_checked(
        executor,
        managed_git_command(
            &worktree.target,
            &worktree.source_repository,
            ["worktree", "prune"],
            "prune managed worktree metadata",
        ),
    )?;
    Ok(true)
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
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
    branch: BranchDisposition,
) -> Result<()> {
    if !remove_managed_worktree_checkout(executor, worktree)? {
        return Ok(());
    }
    if worktree.kind == ManagedCheckoutKind::Clone {
        return remove_empty_managed_worktree_directories(executor, worktree);
    }
    if branch == BranchDisposition::Keep {
        return remove_empty_managed_worktree_directories(executor, worktree);
    }
    let branch_ref = format!("refs/heads/{}", worktree.branch);
    let check = managed_git_command(
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
                &worktree.target,
                &worktree.source_repository,
                ["branch", "-D", "--", &worktree.branch],
                "delete managed raw-session branch",
            ),
        )?;
    }
    remove_empty_managed_worktree_directories(executor, worktree)
}

fn remove_empty_managed_worktree_directories(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
) -> Result<()> {
    let worktrees = worktree
        .source_repository
        .join(".mj")
        .join(match worktree.kind {
            ManagedCheckoutKind::Worktree => "worktrees",
            ManagedCheckoutKind::Clone => "clones",
        });
    let hel = worktree.source_repository.join(".mj");
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
