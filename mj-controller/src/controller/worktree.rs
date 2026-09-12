//! Managed worktrees and raw-to-workspace project conversion.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use mj_core::config::{Config, ProjectBundle, TargetTemplate};
use mj_core::local_git::canonical_repository;
use mj_core::state::{
    ManagedWorktree, ManagedWorktreeTarget, ProjectSourceIdentity, SessionRecord,
};

use crate::targets::{
    self, CancellableProcessExecutor, CommandExecutor, CommandOutput, CommandSpec, SshTarget,
};
pub(super) use mj_client::target::managed_worktree_target;
pub use mj_client::target::{ResumePlan, resume_compatibility};

use super::{Controller, backend_ssh, execute_checked, now, ssh_command_spec};

impl Controller {
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
                targets::validate_bare_project_directory(&backend_ssh(ssh), directory, executor)?;
                mj_core::remote_git::resolve_local_repository(
                    directory,
                    &RemoteGitExecutor {
                        executor,
                        ssh: backend_ssh(ssh),
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
        // A fresh branch starts at the repository's HEAD, so that is what an
        // export diffs against. A retained branch already carries the session's
        // commits; its own creation point is what its reflog names.
        if !reuse_existing_branch {
            worktree.base_commit =
                Some(read_checkout_position(executor, &worktree.target, source)?.head_commit);
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
            return Ok(false);
        }
        let inspection = inspect_raw_project(executor, &target, selected)?;
        if !inspection.primary_checkout {
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
            .join("worktrees")
            .join(session_id);
        // The worktree branch is created from the repository's HEAD, so record
        // that commit as the session base rather than rediscovering it later.
        let base_commit =
            read_checkout_position(executor, &target, &inspection.source_repository)?.head_commit;
        let managed = ManagedWorktree {
            source_project_directory: inspection.source_project_directory,
            source_repository: inspection.source_repository,
            worktree_root: worktree_root.clone(),
            branch: format!("mj/{session_id}"),
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
        cleanup_managed_worktree(executor, worktree)
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
            .execute(&ssh_command_spec(&self.ssh, arguments).purpose(&command.purpose))
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
            ssh_command_spec(&ssh, remote)
        }
    }
}

fn managed_git_command(
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
    Ok(RawToWorkspaceConversion {
        checkout,
        repository,
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
            let template = config
                .targets
                .get(&session.target_template_id)
                .context("the bare target this session last used is missing")?;
            managed_worktree_target(template)?
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
    recorded: Option<&mj_core::archive::RepositoryMetadata>,
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
) -> Result<()> {
    let check = managed_git_command(
        target,
        repository,
        [
            "check-ignore",
            "--quiet",
            "--no-index",
            "--",
            ".mj/worktrees/",
        ],
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
    const ENTRY: &str = "/.mj/worktrees/";
    match target {
        ManagedWorktreeTarget::Local => {
            use std::io::Write;
            let existing = match std::fs::read_to_string(&exclude_path) {
                Ok(existing) => existing,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => return Err(error.into()),
            };
            if existing.lines().any(|line| line.trim() == ENTRY) {
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
            writeln!(file, "# Hel managed worktrees\n{ENTRY}")?;
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
                    ENTRY,
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
    ensure_managed_worktree_excluded(executor, &worktree.target, &worktree.source_repository)?;
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
                "HEAD",
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
    if !remove_managed_worktree_checkout(executor, worktree)? {
        return Ok(());
    }
    remove_empty_managed_worktree_directories(executor, worktree)
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

pub(super) fn cleanup_managed_worktree(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
) -> Result<()> {
    if !remove_managed_worktree_checkout(executor, worktree)? {
        return Ok(());
    }
    let branch_ref = format!("refs/heads/{}", worktree.branch);
    let check = managed_git_command(
        &worktree.target,
        &worktree.source_repository,
        ["show-ref", "--verify", "--quiet", &branch_ref],
        "check managed worktree branch",
    );
    let output = executor.execute(&check)?;
    match output.status {
        0 => {
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
        1 => {}
        status => bail!(
            "check managed worktree branch failed with status {status}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
    remove_empty_managed_worktree_directories(executor, worktree)
}

fn remove_empty_managed_worktree_directories(
    executor: &impl CommandExecutor,
    worktree: &ManagedWorktree,
) -> Result<()> {
    let worktrees = worktree.source_repository.join(".mj").join("worktrees");
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
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::Result;

    use crate::controller::Controller;
    use crate::controller::resume::apply_failed_resume_rollback;
    use crate::controller::test_support::{
        checkpoint_test_session, committed_repository, local_bundle, managed_raw_session,
        managed_worktree_session, raw_session_on, resume_compatibility_config, ssh_worktree_target,
        test_git,
    };
    use mj_core::archive::RepositoryMetadata;
    use mj_core::config::{
        Config, HarnessProfile, ProjectBundle, ProjectRepository, TargetTemplate,
    };
    use mj_core::state::{ManagedWorktree, ManagedWorktreeTarget, SessionState, State};

    use crate::targets::{
        CancellableProcessExecutor, CommandExecutor, CommandOutput, CommandSpec, ProcessExecutor,
    };

    use super::*;

    #[test]
    fn local_bare_validation_accepts_projects_and_plain_directories_but_rejects_missing_paths() {
        let project = committed_repository();
        let plain = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config
            .targets
            .insert("localhost".into(), TargetTemplate::LocalBare);
        let controller = Controller {
            config,
            state: State::default(),
        };
        controller
            .validate_project_directory("localhost", project.path(), &ProcessExecutor)
            .unwrap();
        controller
            .validate_project_directory("localhost", plain.path(), &ProcessExecutor)
            .unwrap();
        assert!(
            controller
                .validate_project_directory(
                    "localhost",
                    &plain.path().join("missing"),
                    &ProcessExecutor
                )
                .is_err()
        );
    }

    #[test]
    fn a_plain_local_directory_starts_without_creating_a_git_worktree() {
        let plain = tempfile::tempdir().unwrap();
        let session = raw_session_on("localhost", plain.path().to_str().unwrap());
        let session_id = session.id.clone();
        let mut config = Config::default();
        config
            .targets
            .insert("localhost".into(), TargetTemplate::LocalBare);
        let mut controller = Controller {
            config,
            state: State {
                sessions: [(session_id.clone(), session)].into_iter().collect(),
                ..State::default()
            },
        };
        assert!(
            !controller
                .prepare_managed_raw_worktree(&session_id, &ProcessExecutor)
                .unwrap()
        );
        let session = &controller.state.sessions[&session_id];
        assert_eq!(session.project_directory.as_deref(), Some(plain.path()));
        assert!(session.managed_worktree.is_none());
        assert!(!plain.path().join(".git").exists());
    }

    #[test]
    fn raw_linked_worktree_origin_matches_the_configured_github_project() {
        struct OriginExecutor;
        impl CommandExecutor for OriginExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                assert_eq!(
                    command.args,
                    [
                        "-C",
                        "/mnt/optane/bifrost-fird",
                        "config",
                        "--get",
                        "remote.origin.url",
                    ]
                );
                Ok(CommandOutput {
                    status: 0,
                    stdout: b"git@github.com:BrokkAi/bifrost-dev.git\n".to_vec(),
                    stderr: Vec::new(),
                })
            }
        }

        let mut config = Config::default();
        config
            .targets
            .insert("localhost".into(), TargetTemplate::LocalBare);
        let session = raw_session_on("localhost", "/mnt/optane/bifrost-fird");
        let session_id = session.id.clone();
        let controller = Controller {
            config,
            state: State {
                sessions: [(session_id.clone(), session)].into_iter().collect(),
                ..State::default()
            },
        };

        let source = controller
            .resolve_session_project_source(&session_id, &OriginExecutor)
            .unwrap();

        assert_eq!(source.key, "github:brokkai/bifrost-dev");
        assert_eq!(source.short, "bifrost-dev");
        assert_eq!(source.full, "BrokkAi/bifrost-dev");
    }
    #[test]
    fn managed_worktree_origin_uses_source_repository_while_checkout_is_retired() {
        struct OriginExecutor;
        impl CommandExecutor for OriginExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                assert_eq!(
                    command.args,
                    [
                        "-C",
                        "/home/dev/project",
                        "config",
                        "--get",
                        "remote.origin.url",
                    ]
                );
                Ok(CommandOutput {
                    status: 0,
                    stdout: b"git@github.com:example/project.git\n".to_vec(),
                    stderr: Vec::new(),
                })
            }
        }

        let session = managed_raw_session(ManagedWorktreeTarget::Local);
        let session_id = session.id.clone();
        let controller = Controller {
            config: Config::default(),
            state: State {
                sessions: [(session_id.clone(), session)].into_iter().collect(),
                ..State::default()
            },
        };

        let source = controller
            .resolve_session_project_source(&session_id, &OriginExecutor)
            .unwrap();

        assert_eq!(source.key, "github:example/project");
    }

    #[test]
    fn raw_no_origin_uses_the_canonical_main_repository_root() {
        struct NoOriginExecutor {
            commands: RefCell<Vec<CommandSpec>>,
        }
        impl CommandExecutor for NoOriginExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.commands.borrow_mut().push(command.clone());
                if command.args.iter().any(|argument| argument == "config") {
                    return Ok(CommandOutput {
                        status: 1,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    });
                }
                let stdout = if command
                    .args
                    .iter()
                    .any(|argument| argument == "--show-toplevel")
                {
                    "/worktrees/project-side\n"
                } else if command
                    .args
                    .iter()
                    .any(|argument| argument == "--git-common-dir")
                {
                    "/projects/project/.git\n"
                } else {
                    panic!("unexpected command {:?}", command.args);
                };
                Ok(CommandOutput {
                    status: 0,
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: Vec::new(),
                })
            }
        }

        let mut config = Config::default();
        config
            .targets
            .insert("localhost".into(), TargetTemplate::LocalBare);
        let session = raw_session_on("localhost", "/worktrees/project-side");
        let session_id = session.id.clone();
        let controller = Controller {
            config,
            state: State {
                sessions: [(session_id.clone(), session)].into_iter().collect(),
                ..State::default()
            },
        };
        let executor = NoOriginExecutor {
            commands: RefCell::new(Vec::new()),
        };

        let source = controller
            .resolve_session_project_source(&session_id, &executor)
            .unwrap();

        assert_eq!(source.key, "path:/projects/project");
        assert_eq!(source.short, "project");
        assert_eq!(source.full, "/projects/project");
        assert_eq!(executor.commands.borrow().len(), 3);
    }

    #[test]
    fn raw_non_git_directory_keeps_its_local_path_source() {
        struct NonGitExecutor {
            commands: RefCell<Vec<CommandSpec>>,
        }
        impl CommandExecutor for NonGitExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.commands.borrow_mut().push(command.clone());
                let status = if command.args.iter().any(|argument| argument == "config") {
                    1
                } else {
                    assert!(
                        command
                            .args
                            .iter()
                            .any(|argument| argument == "--show-toplevel")
                    );
                    128
                };
                Ok(CommandOutput {
                    status,
                    stdout: Vec::new(),
                    stderr: b"fatal: not a git repository\n".to_vec(),
                })
            }
        }

        let mut config = Config::default();
        config
            .targets
            .insert("localhost".into(), TargetTemplate::LocalBare);
        let session = raw_session_on("localhost", "/scratch/project");
        let session_id = session.id.clone();
        let controller = Controller {
            config,
            state: State {
                sessions: [(session_id.clone(), session)].into_iter().collect(),
                ..State::default()
            },
        };
        let executor = NonGitExecutor {
            commands: RefCell::new(Vec::new()),
        };

        let source = controller
            .resolve_session_project_source(&session_id, &executor)
            .unwrap();

        assert_eq!(source.key, "path:/scratch/project");
        assert_eq!(source.full, "/scratch/project");
        assert_eq!(executor.commands.borrow().len(), 2);
    }

    #[test]
    fn project_root_lookup_reports_git_failures_instead_of_treating_them_as_non_git() {
        struct FailedGit;
        impl CommandExecutor for FailedGit {
            fn execute(&self, _: &CommandSpec) -> Result<CommandOutput> {
                Ok(CommandOutput {
                    status: 128,
                    stdout: Vec::new(),
                    stderr: b"fatal: detected dubious ownership in repository".to_vec(),
                })
            }
        }
        let error = resolve_git_root(
            &ManagedWorktreeTarget::Local,
            Path::new("/project"),
            &FailedGit,
        )
        .unwrap_err();
        assert!(error.to_string().contains("dubious ownership"));
    }

    /// Answers the two Git reads that locate a checkout, and nothing else.
    struct CheckoutPositionExecutor {
        head_commit: String,
        branch: Option<String>,
    }
    impl CommandExecutor for CheckoutPositionExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let stdout = if command.args.iter().any(|argument| argument == "rev-parse") {
                self.head_commit.clone()
            } else if command
                .args
                .iter()
                .any(|argument| argument == "symbolic-ref")
            {
                match &self.branch {
                    Some(branch) => branch.clone(),
                    None => {
                        return Ok(CommandOutput {
                            status: 1,
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                        });
                    }
                }
            } else {
                panic!("unexpected command {:?}", command.args);
            };
            Ok(CommandOutput {
                status: 0,
                stdout: format!("{stdout}\n").into_bytes(),
                stderr: Vec::new(),
            })
        }
    }
    fn recorded_repository(head_commit: &str, branch: Option<&str>) -> RepositoryMetadata {
        RepositoryMetadata {
            push_urls: Vec::new(),
            remote_workspace: false,
            id: "project".into(),
            relative_destination: PathBuf::from("project"),
            origin: "mj-local:project".into(),
            base_commit: String::new(),
            head_commit: head_commit.into(),
            branch: branch.map(str::to_owned),
        }
    }
    #[test]
    fn a_raw_checkout_that_moved_while_stopped_gets_a_conversation_line() {
        let config = resume_compatibility_config();
        let session = managed_raw_session(ManagedWorktreeTarget::Local);
        let directory = session.project_directory.clone().unwrap();
        let executor = CheckoutPositionExecutor {
            head_commit: "b".repeat(40),
            branch: Some("mj/0123456789abcdef0123456789abcdef".into()),
        };

        let live = raw_checkout_position(&session, &config, &directory, &executor).unwrap();
        let notice = raw_checkout_divergence_notice(
            &directory,
            Some(&recorded_repository(&"a".repeat(40), Some("main"))),
            &live,
        )
        .expect("a moved checkout is reported");

        assert!(
            notice.contains(&directory.display().to_string()),
            "{notice}"
        );
        assert!(notice.contains("aaaaaaaaaaaa (main)"), "{notice}");
        assert!(
            notice.contains("bbbbbbbbbbbb (mj/0123456789abcdef0123456789abcdef)"),
            "{notice}"
        );
        assert!(
            notice.contains("while this session was stopped"),
            "{notice}"
        );
    }
    #[test]
    fn a_raw_checkout_that_stayed_put_gets_no_conversation_line() {
        let config = resume_compatibility_config();
        let session = managed_raw_session(ManagedWorktreeTarget::Local);
        let directory = session.project_directory.clone().unwrap();
        let executor = CheckoutPositionExecutor {
            head_commit: "a".repeat(40),
            branch: Some("main".into()),
        };

        let live = raw_checkout_position(&session, &config, &directory, &executor).unwrap();

        assert_eq!(
            raw_checkout_divergence_notice(
                &directory,
                Some(&recorded_repository(&"a".repeat(40), Some("main"))),
                &live,
            ),
            None
        );
    }
    #[test]
    fn a_checkpoint_without_recorded_git_identity_reports_nothing() {
        let live = CheckoutPosition {
            head_commit: "b".repeat(40),
            branch: None,
        };

        assert_eq!(
            raw_checkout_divergence_notice(Path::new("/home/dev/project"), None, &live),
            None
        );
        assert_eq!(
            raw_checkout_divergence_notice(
                Path::new("/home/dev/project"),
                Some(&recorded_repository("", None)),
                &live,
            ),
            None
        );
    }
    #[test]
    fn a_detached_checkout_is_named_as_detached() {
        let config = resume_compatibility_config();
        let session = managed_raw_session(ManagedWorktreeTarget::Local);
        let directory = session.project_directory.clone().unwrap();
        let executor = CheckoutPositionExecutor {
            head_commit: "c".repeat(40),
            branch: None,
        };

        let live = raw_checkout_position(&session, &config, &directory, &executor).unwrap();
        let notice = raw_checkout_divergence_notice(
            &directory,
            Some(&recorded_repository(&"a".repeat(40), Some("main"))),
            &live,
        )
        .expect("a moved checkout is reported");

        assert!(notice.contains("cccccccccccc (detached)"), "{notice}");
    }
    #[test]
    fn bundle_sessions_resume_on_any_workspace_target() {
        let config = resume_compatibility_config();
        let session = checkpoint_test_session("0123456789abcdef0123456789abcdef");

        assert_eq!(
            resume_compatibility(&session, &config, "podman"),
            Ok(ResumePlan::InPlace)
        );
        assert_eq!(
            resume_compatibility(&session, &config, "ssh-bare"),
            Ok(ResumePlan::InPlace)
        );
    }
    #[test]
    fn a_single_local_repository_can_become_a_checkout() {
        let mut config = resume_compatibility_config();
        let mut session = checkpoint_test_session("0123456789abcdef0123456789abcdef");
        session.bundle_id = "project".into();
        config.bundles.insert(
            "project".into(),
            local_bundle(Path::new("/home/dev/project")),
        );

        assert_eq!(
            resume_compatibility(&session, &config, "local-bare"),
            Ok(ResumePlan::WorkspaceToRaw)
        );
    }
    #[test]
    fn a_github_project_cannot_become_a_checkout() {
        let mut config = resume_compatibility_config();
        let mut session = checkpoint_test_session("0123456789abcdef0123456789abcdef");
        session.bundle_id = "project".into();
        let mut bundle = local_bundle(Path::new("/home/dev/project"));
        bundle.repositories[0].local = None;
        bundle.repositories[0].github = Some("example/project".into());
        config.bundles.insert("project".into(), bundle);

        let reason = resume_compatibility(&session, &config, "local-bare").unwrap_err();

        assert!(reason.contains("came from GitHub"), "{reason}");
        assert!(
            reason.contains("resume it on a container, SSH, or EC2 target"),
            "{reason}"
        );
    }
    #[test]
    fn a_multi_repository_project_cannot_become_a_checkout() {
        let mut config = resume_compatibility_config();
        let mut session = checkpoint_test_session("0123456789abcdef0123456789abcdef");
        session.bundle_id = "project".into();
        let mut bundle = local_bundle(Path::new("/home/dev/project"));
        bundle.repositories.push(ProjectRepository {
            id: "tools".into(),
            github: None,
            local: Some(PathBuf::from("/home/dev/tools")),
            destination: PathBuf::from("tools"),
            git_ref: None,
        });
        config.bundles.insert("project".into(), bundle);

        let reason = resume_compatibility(&session, &config, "local-bare").unwrap_err();

        assert!(reason.contains("2 repositories"), "{reason}");
        assert!(reason.contains("one checkout"), "{reason}");
    }
    #[test]
    fn bundle_sessions_refuse_a_local_bare_target_with_a_reason() {
        let config = resume_compatibility_config();
        let session = checkpoint_test_session("0123456789abcdef0123456789abcdef");

        let reason = resume_compatibility(&session, &config, "local-bare").unwrap_err();

        assert!(reason.contains("created from a project bundle"), "{reason}");
        assert!(
            reason.contains("resume it on a container, SSH, or EC2 target"),
            "{reason}"
        );
    }
    #[test]
    fn managed_raw_sessions_resume_on_their_own_worktree_host() {
        let config = resume_compatibility_config();

        assert_eq!(
            resume_compatibility(
                &managed_raw_session(ManagedWorktreeTarget::Local),
                &config,
                "local-bare",
            ),
            Ok(ResumePlan::InPlace)
        );
        assert_eq!(
            resume_compatibility(
                &managed_raw_session(ssh_worktree_target()),
                &config,
                "ssh-bare",
            ),
            Ok(ResumePlan::InPlace)
        );
    }
    #[test]
    fn managed_raw_sessions_refuse_a_bare_target_on_another_host() {
        let config = resume_compatibility_config();

        let reason = resume_compatibility(
            &managed_raw_session(ManagedWorktreeTarget::Local),
            &config,
            "ssh-bare",
        )
        .unwrap_err();
        assert!(reason.contains("this machine"), "{reason}");

        let reason = resume_compatibility(
            &managed_raw_session(ssh_worktree_target()),
            &config,
            "local-bare",
        )
        .unwrap_err();
        assert!(reason.contains("dev@builder"), "{reason}");
    }
    #[test]
    fn raw_checkpoints_cannot_move_to_an_isolated_target() {
        let config = resume_compatibility_config();
        for session in [
            managed_raw_session(ManagedWorktreeTarget::Local),
            raw_session_on("local-bare", "/home/dev/project"),
        ] {
            let reason = resume_compatibility(&session, &config, "podman").unwrap_err();
            assert!(reason.contains("network repository provenance"), "{reason}");
            assert!(reason.contains("bare target"), "{reason}");
        }
    }
    #[test]
    fn a_raw_checkout_on_an_ssh_host_cannot_convert() {
        let config = resume_compatibility_config();

        let reason = resume_compatibility(
            &managed_raw_session(ssh_worktree_target()),
            &config,
            "podman",
        )
        .unwrap_err();
        assert!(reason.contains("works directly in"), "{reason}");
        assert!(reason.contains("dev@builder"), "{reason}");

        let reason = resume_compatibility(
            &raw_session_on("ssh-bare", "/srv/project"),
            &config,
            "podman",
        )
        .unwrap_err();
        assert!(reason.contains("on an SSH host"), "{reason}");
    }
    #[test]
    fn a_session_that_opens_a_subdirectory_of_its_worktree_cannot_convert() {
        let config = resume_compatibility_config();
        let mut session = managed_raw_session(ManagedWorktreeTarget::Local);
        let worktree = session.managed_worktree.as_mut().unwrap();
        worktree.source_project_directory = worktree.source_repository.join("crate");
        session.project_directory = Some(worktree.worktree_root.join("crate"));

        let reason = resume_compatibility(&session, &config, "podman").unwrap_err();

        assert!(reason.contains("subdirectory of its checkout"), "{reason}");
    }
    #[test]
    fn unmanaged_raw_sessions_require_the_same_bare_target_kind() {
        let config = resume_compatibility_config();
        let local = raw_session_on("local-bare", "/home/dev/project");
        let remote = raw_session_on("ssh-bare", "/srv/project");

        assert_eq!(
            resume_compatibility(&local, &config, "local-bare"),
            Ok(ResumePlan::InPlace)
        );
        assert_eq!(
            resume_compatibility(&remote, &config, "ssh-bare"),
            Ok(ResumePlan::InPlace)
        );
        for (session, target) in [(&local, "ssh-bare"), (&remote, "local-bare")] {
            let reason = resume_compatibility(session, &config, target).unwrap_err();
            assert!(reason.contains("directly on its host"), "{reason}");
        }
    }
    #[test]
    fn resume_compatibility_names_a_target_that_is_gone() {
        let config = resume_compatibility_config();
        let session = checkpoint_test_session("0123456789abcdef0123456789abcdef");

        let reason = resume_compatibility(&session, &config, "retired").unwrap_err();

        assert!(reason.contains("retired"), "{reason}");
    }
    #[test]
    fn managed_raw_worktree_inherits_upstream_and_cleans_up_owned_artifacts() {
        let repository = committed_repository();
        let remote_parent = tempfile::tempdir().unwrap();
        let remote = remote_parent.path().join("remote.git");
        let output = Command::new("git")
            .args(["init", "--bare"])
            .arg(&remote)
            .output()
            .unwrap();
        assert!(output.status.success());
        test_git(
            repository.path(),
            &["remote", "add", "origin", &remote.to_string_lossy()],
        );
        test_git(
            repository.path(),
            &["push", "--set-upstream", "origin", "master"],
        );

        let target = ManagedWorktreeTarget::Local;
        let inspection =
            inspect_raw_project(&ProcessExecutor, &target, &repository.path().join("nested"))
                .unwrap();
        assert!(inspection.primary_checkout);
        assert_eq!(inspection.upstream.as_deref(), Some("origin/master"));
        // git rev-parse canonicalizes symlinks (macOS tempdirs live behind the
        // /var -> /private/var link), so compare against the canonical path.
        assert_eq!(
            inspection.source_project_directory,
            repository.path().canonicalize().unwrap().join("nested")
        );

        let session_id = "0123456789abcdef0123456789abcdef";
        let worktree = ManagedWorktree {
            source_project_directory: inspection.source_project_directory,
            source_repository: inspection.source_repository,
            worktree_root: repository.path().join(".mj/worktrees").join(session_id),
            branch: format!("mj/{session_id}"),
            target,
            base_commit: None,
        };
        create_managed_worktree(
            &ProcessExecutor,
            &worktree,
            inspection.upstream.as_deref(),
            PrimaryCheckoutRequirement::Clean,
        )
        .unwrap();
        assert!(worktree.worktree_root.join("nested/file.txt").is_file());
        assert_eq!(
            test_git(
                &worktree.worktree_root,
                &[
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}"
                ]
            ),
            "origin/master"
        );
        assert_eq!(test_git(repository.path(), &["status", "--porcelain"]), "");
        std::fs::write(worktree.worktree_root.join("dirty.txt"), "session\n").unwrap();

        cleanup_managed_worktree(&ProcessExecutor, &worktree).unwrap();
        assert!(!worktree.worktree_root.exists());
        assert!(!repository.path().join(".mj").exists());
        let output = Command::new("git")
            .arg("-C")
            .arg(repository.path())
            .args([
                "show-ref",
                "--verify",
                &format!("refs/heads/{}", worktree.branch),
            ])
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    #[test]
    fn retired_worktree_can_be_recreated_from_its_retained_branch() {
        let repository = committed_repository();
        let session_id = "0123456789abcdef0123456789abcdef";
        let session = managed_worktree_session(repository.path(), session_id);
        let worktree = session.managed_worktree.unwrap();
        // The checkout is dirty by design: its dirty state moved into the target.
        std::fs::write(worktree.worktree_root.join("dirty.txt"), "session\n").unwrap();

        retire_managed_worktree(&ProcessExecutor, &worktree).unwrap();

        assert!(!worktree.worktree_root.exists());
        assert!(!repository.path().join(".mj").exists());
        let branch = Command::new("git")
            .arg("-C")
            .arg(repository.path())
            .args([
                "show-ref",
                "--verify",
                &format!("refs/heads/{}", worktree.branch),
            ])
            .output()
            .unwrap();
        assert!(
            branch.status.success(),
            "the session branch must survive: later checkpoints are deltas against it"
        );

        assert!(restore_managed_worktree(&ProcessExecutor, &worktree).unwrap());
        assert!(worktree.worktree_root.join("nested/file.txt").is_file());
        assert!(!restore_managed_worktree(&ProcessExecutor, &worktree).unwrap());
    }
    #[test]
    fn retiring_a_remote_worktree_prunes_registration_after_target_removed_checkout() {
        struct RemoteExecutor {
            path_checks: RefCell<usize>,
            commands: RefCell<Vec<CommandSpec>>,
        }

        impl CommandExecutor for RemoteExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.commands.borrow_mut().push(command.clone());
                let status = if command.purpose == "check managed worktree path" {
                    let mut checks = self.path_checks.borrow_mut();
                    let status = i32::from(*checks != 0);
                    *checks += 1;
                    status
                } else {
                    0
                };
                Ok(CommandOutput {
                    status,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let worktree = ManagedWorktree {
            source_project_directory: PathBuf::from("/srv/project"),
            source_repository: PathBuf::from("/srv/project"),
            worktree_root: PathBuf::from("/srv/project/.mj/worktrees/session"),
            branch: "mj/session".into(),
            target: ManagedWorktreeTarget::Ssh {
                destination: "builder".into(),
                ssh_args: Vec::new(),
            },
            base_commit: None,
        };
        let executor = RemoteExecutor {
            path_checks: RefCell::new(0),
            commands: RefCell::new(Vec::new()),
        };

        retire_managed_worktree(&executor, &worktree).unwrap();

        let purposes = executor
            .commands
            .borrow()
            .iter()
            .map(|command| command.purpose.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            purposes,
            [
                "check managed worktree path",
                "check managed worktree path",
                "prune managed worktree metadata",
                "remove empty managed worktree directories",
            ]
        );
    }
    #[test]
    fn a_managed_conversion_carries_the_session_worktree_not_the_primary_checkout() {
        let repository = committed_repository();
        let session_id = "0123456789abcdef0123456789abcdef";
        let session = managed_worktree_session(repository.path(), session_id);
        let worktree = session.managed_worktree.clone().unwrap();

        let conversion =
            plan_raw_to_workspace(&session, &Config::default(), &ProcessExecutor).unwrap();

        assert_eq!(conversion.checkout, worktree.worktree_root);
        assert_eq!(conversion.repository, repository.path());
        assert_eq!(conversion.retire, Some(worktree));
        let bundle = conversion.new_bundle.expect("a bundle is synthesized");
        assert_eq!(bundle.repositories.len(), 1);
        assert_eq!(bundle.primary_repo, bundle.repositories[0].id);
        assert_eq!(
            bundle.repositories[0].local.as_deref(),
            Some(repository.path())
        );
        assert_eq!(bundle.repositories[0].github, None);
        // The archive names the session directory as the repository, and the
        // restored harness session points inside the target at that name.
        assert_eq!(
            bundle.repositories[0].destination,
            PathBuf::from(session_id)
        );
    }
    #[test]
    fn an_unmanaged_conversion_serves_the_main_repository_behind_a_linked_worktree() {
        let repository = committed_repository();
        let session_id = "0123456789abcdef0123456789abcdef";
        let linked = managed_worktree_session(repository.path(), session_id);
        let checkout = linked.managed_worktree.unwrap().worktree_root;
        let mut session = checkpoint_test_session(session_id);
        session.state = SessionState::Stopped;
        session.target_template_id = "local-bare".into();
        session.project_directory = Some(checkout.clone());

        let conversion =
            plan_raw_to_workspace(&session, &Config::default(), &ProcessExecutor).unwrap();

        assert_eq!(conversion.checkout, checkout.canonicalize().unwrap());
        assert_eq!(
            conversion.repository,
            repository.path().canonicalize().unwrap()
        );
        assert_eq!(conversion.retire, None);
    }
    /// The recorded project directory may reach the checkout through a
    /// symlink, as the system temp directory does on macOS. Git reports
    /// canonical paths, so the whole-checkout rule must not compare across
    /// the two domains.
    #[cfg(unix)]
    #[test]
    fn an_unmanaged_conversion_accepts_a_checkout_reached_through_a_symlink() {
        let repository = committed_repository();
        let session_id = "0123456789abcdef0123456789abcdef";
        let linked = managed_worktree_session(repository.path(), session_id);
        let checkout = linked.managed_worktree.unwrap().worktree_root;
        let alias = tempfile::tempdir().unwrap();
        let symlink = alias.path().join("checkout");
        std::os::unix::fs::symlink(&checkout, &symlink).unwrap();
        let mut session = checkpoint_test_session(session_id);
        session.state = SessionState::Stopped;
        session.target_template_id = "local-bare".into();
        session.project_directory = Some(symlink);

        let conversion =
            plan_raw_to_workspace(&session, &Config::default(), &ProcessExecutor).unwrap();

        assert_eq!(conversion.checkout, checkout.canonicalize().unwrap());
        assert_eq!(
            conversion.repository,
            repository.path().canonicalize().unwrap()
        );
        assert_eq!(conversion.retire, None);
    }
    #[test]
    fn a_conversion_reuses_a_bundle_that_already_describes_the_checkout() {
        let repository = PathBuf::from("/home/dev/project");
        let destination = PathBuf::from("project");
        let existing = ProjectBundle {
            primary_repo: "project".into(),
            repositories: vec![ProjectRepository {
                id: "project".into(),
                github: None,
                local: Some(repository.clone()),
                destination: destination.clone(),
                git_ref: None,
            }],
        };
        let mut config = Config::default();
        config.bundles.insert("existing".into(), existing);

        assert_eq!(
            converted_raw_bundle(&config, "remote-project-abcdef", &repository, &destination),
            ("existing".to_owned(), None)
        );

        // A different destination is a different checkout location inside the
        // target, so it cannot stand in for this one.
        let (id, synthesized) = converted_raw_bundle(
            &config,
            "remote-project-abcdef",
            &repository,
            Path::new("elsewhere"),
        );
        assert_ne!(id, "existing");
        assert_eq!(
            synthesized.unwrap().repositories[0].destination,
            PathBuf::from("elsewhere")
        );
    }
    #[test]
    fn a_converted_record_is_a_valid_bundle_session() {
        let session_id = "0123456789abcdef0123456789abcdef";
        let mut config = resume_compatibility_config();
        let mut record = managed_raw_session(ManagedWorktreeTarget::Local);
        record.state = SessionState::Running;
        record.target_template_id = "podman".into();
        let conversion = RawToWorkspaceConversion {
            checkout: record.project_directory.clone().unwrap(),
            repository: PathBuf::from("/home/dev/project"),
            bundle_id: "project".into(),
            new_bundle: Some(ProjectBundle {
                primary_repo: "project".into(),
                repositories: vec![ProjectRepository {
                    id: "project".into(),
                    github: None,
                    local: Some(PathBuf::from("/home/dev/project")),
                    destination: PathBuf::from(session_id),
                    git_ref: None,
                }],
            }),
            retire: record.managed_worktree.clone(),
        };

        config.bundles.insert(
            conversion.bundle_id.clone(),
            conversion.new_bundle.clone().unwrap(),
        );
        config.profiles.insert(
            record.last_profile.clone(),
            HarnessProfile {
                enabled: true,
                kind: record.harness_kind,
                home: PathBuf::from("/profiles/codex"),
                environment: BTreeMap::new(),
                context_window_bytes: None,
            },
        );
        apply_raw_to_workspace(&mut record, &conversion);

        assert_eq!(record.project_directory, None);
        assert_eq!(record.managed_worktree, None);
        assert_eq!(record.bundle_id, "project");
        let state = State {
            sessions: BTreeMap::from([(session_id.into(), record)]),
            ..State::default()
        };
        state.validate_against_config(&config).unwrap();
    }
    #[test]
    fn a_session_leaving_its_target_claims_a_worktree_of_its_own_repository() {
        let repository = committed_repository();
        let session_id = "0123456789abcdef0123456789abcdef";
        let mut session = checkpoint_test_session(session_id);
        session.state = SessionState::Stopped;
        session.bundle_id = "project".into();
        let mut config = resume_compatibility_config();
        config
            .bundles
            .insert("project".into(), local_bundle(repository.path()));
        let controller = Controller {
            config,
            state: State {
                sessions: BTreeMap::from([(session_id.into(), session.clone())]),
                ..State::default()
            },
        };

        let conversion = controller
            .plan_workspace_to_raw(&session, "local-bare", &ProcessExecutor)
            .unwrap();

        assert_eq!(
            conversion.worktree,
            ManagedWorktree {
                source_project_directory: repository.path().to_path_buf(),
                source_repository: repository.path().to_path_buf(),
                worktree_root: repository.path().join(".mj/worktrees").join(session_id),
                branch: format!("mj/{session_id}"),
                target: ManagedWorktreeTarget::Local,
                // The new branch starts at the repository's HEAD, which is
                // what an export of this session diffs against.
                base_commit: Some(test_git(repository.path(), &["rev-parse", "HEAD"])),
            }
        );

        // The dirty primary checkout is beside the point: the worktree's
        // contents come from the checkpoint.
        std::fs::write(repository.path().join("dirty.txt"), "primary\n").unwrap();
        create_managed_worktree(
            &ProcessExecutor,
            &conversion.worktree,
            None,
            PrimaryCheckoutRequirement::Any,
        )
        .unwrap();
        assert!(conversion.worktree.worktree_root.is_dir());

        // A second attempt refuses rather than taking over a live worktree.
        let error = controller
            .plan_workspace_to_raw(&session, "local-bare", &ProcessExecutor)
            .unwrap_err();
        assert!(format!("{error:#}").contains("already exists"), "{error:#}");
    }
    #[test]
    fn a_return_to_local_reuses_its_retained_branch_and_preserves_its_tip() {
        let repository = committed_repository();
        let session_id = "0123456789abcdef0123456789abcdef";
        let branch = format!("mj/{session_id}");
        let original_tip = test_git(repository.path(), &["rev-parse", "HEAD"]);
        test_git(repository.path(), &["branch", &branch]);
        let mut config = resume_compatibility_config();
        config
            .bundles
            .insert("project".into(), local_bundle(repository.path()));
        let session = checkpoint_test_session(session_id);
        let controller = Controller {
            config,
            state: State {
                sessions: BTreeMap::from([(session_id.into(), session.clone())]),
                ..State::default()
            },
        };

        let conversion = controller
            .plan_workspace_to_raw(&session, "local-bare", &ProcessExecutor)
            .unwrap();

        assert!(conversion.reuse_existing_branch);
        assert!(!conversion.worktree.worktree_root.exists());
        let recovery_ref =
            preserve_retained_managed_worktree_branch(&ProcessExecutor, &conversion.worktree)
                .unwrap();
        assert_eq!(
            recovery_ref,
            format!("refs/mj/recovery/{session_id}/{original_tip}")
        );
        assert_eq!(
            test_git(
                repository.path(),
                &["show-ref", "--hash", recovery_ref.as_str()],
            ),
            original_tip
        );
        assert_eq!(
            preserve_retained_managed_worktree_branch(&ProcessExecutor, &conversion.worktree)
                .unwrap(),
            recovery_ref
        );

        test_git(repository.path(), &["checkout", &branch]);
        test_git(
            repository.path(),
            &["commit", "--allow-empty", "-m", "later retained tip"],
        );
        let later_tip = test_git(repository.path(), &["rev-parse", "HEAD"]);
        test_git(repository.path(), &["checkout", "master"]);
        let later_recovery_ref =
            preserve_retained_managed_worktree_branch(&ProcessExecutor, &conversion.worktree)
                .unwrap();
        assert_eq!(
            later_recovery_ref,
            format!("refs/mj/recovery/{session_id}/{later_tip}")
        );
        assert_ne!(later_recovery_ref, recovery_ref);
        assert_eq!(
            test_git(
                repository.path(),
                &["show-ref", "--hash", recovery_ref.as_str()],
            ),
            original_tip
        );
    }
    #[test]
    fn a_return_to_local_rejects_a_retained_branch_checked_out_elsewhere() {
        let repository = committed_repository();
        let session_id = "0123456789abcdef0123456789abcdef";
        let branch = format!("mj/{session_id}");
        test_git(repository.path(), &["branch", &branch]);
        let elsewhere = repository.path().join("other-worktree");
        test_git(
            repository.path(),
            &["worktree", "add", &elsewhere.to_string_lossy(), &branch],
        );
        let mut config = resume_compatibility_config();
        config
            .bundles
            .insert("project".into(), local_bundle(repository.path()));
        let session = checkpoint_test_session(session_id);
        let controller = Controller {
            config,
            state: State {
                sessions: BTreeMap::from([(session_id.into(), session.clone())]),
                ..State::default()
            },
        };

        let error = controller
            .plan_workspace_to_raw(&session, "local-bare", &ProcessExecutor)
            .unwrap_err();

        assert!(error.to_string().contains("still checked out"), "{error:#}");
        assert!(
            !repository
                .path()
                .join(".mj/worktrees")
                .join(session_id)
                .exists()
        );
        assert_eq!(
            test_git(repository.path(), &["rev-parse", &branch]),
            test_git(repository.path(), &["rev-parse", "HEAD"])
        );
    }
    #[test]
    fn a_session_that_left_its_target_is_a_valid_raw_session() {
        let session_id = "0123456789abcdef0123456789abcdef";
        let repository = PathBuf::from("/home/dev/project");
        let mut config = resume_compatibility_config();
        config
            .bundles
            .insert("project".into(), local_bundle(&repository));
        config.profiles.insert(
            "codex".into(),
            HarnessProfile {
                enabled: true,
                kind: mj_core::config::HarnessKind::Codex,
                home: PathBuf::from("/profiles/codex"),
                environment: BTreeMap::new(),
                context_window_bytes: None,
            },
        );
        let mut record = checkpoint_test_session(session_id);
        record.bundle_id = "project".into();
        record.target_template_id = "local-bare".into();
        let conversion = WorkspaceToRawConversion {
            worktree: ManagedWorktree {
                source_project_directory: repository.clone(),
                source_repository: repository.clone(),
                worktree_root: repository.join(".mj/worktrees").join(session_id),
                branch: format!("mj/{session_id}"),
                target: ManagedWorktreeTarget::Local,
                base_commit: None,
            },
            reuse_existing_branch: false,
        };

        apply_workspace_to_raw(&mut record, &conversion);

        assert_eq!(
            record.project_directory.as_deref(),
            Some(conversion.worktree.worktree_root.as_path())
        );
        assert_eq!(record.bundle_id, "project", "the bundle still describes it");
        let state = State {
            sessions: BTreeMap::from([(session_id.into(), record)]),
            ..State::default()
        };
        state.validate_against_config(&config).unwrap();
    }
    #[test]
    fn a_failed_departure_returns_the_session_to_its_bundle() {
        let session_id = "0123456789abcdef0123456789abcdef";
        let repository = PathBuf::from("/home/dev/project");
        let previous = {
            let mut record = checkpoint_test_session(session_id);
            record.state = SessionState::Stopped;
            record.bundle_id = "project".into();
            record
        };
        let mut converted = previous.clone();
        converted.state = SessionState::Provisioning;
        apply_workspace_to_raw(
            &mut converted,
            &WorkspaceToRawConversion {
                worktree: ManagedWorktree {
                    source_project_directory: repository.clone(),
                    source_repository: repository.clone(),
                    worktree_root: repository.join(".mj/worktrees").join(session_id),
                    branch: format!("mj/{session_id}"),
                    target: ManagedWorktreeTarget::Local,
                    base_commit: None,
                },
                reuse_existing_branch: false,
            },
        );

        apply_failed_resume_rollback(&mut converted, &previous, "podman is unavailable", None);

        assert_eq!(converted.project_directory, None);
        assert_eq!(converted.managed_worktree, None);
        assert_eq!(converted.bundle_id, "project");
    }
    #[test]
    fn a_failed_conversion_returns_the_session_to_its_checkout() {
        let previous = managed_raw_session(ManagedWorktreeTarget::Local);
        let mut converted = previous.clone();
        converted.state = SessionState::Provisioning;
        converted.target_template_id = "podman".into();
        apply_raw_to_workspace(
            &mut converted,
            &RawToWorkspaceConversion {
                checkout: previous.project_directory.clone().unwrap(),
                repository: PathBuf::from("/home/dev/project"),
                bundle_id: "project".into(),
                new_bundle: None,
                retire: previous.managed_worktree.clone(),
            },
        );

        let mut cleaned = converted.clone();
        apply_failed_resume_rollback(&mut cleaned, &previous, "podman is unavailable", None);
        assert_eq!(cleaned.project_directory, previous.project_directory);
        assert_eq!(cleaned.managed_worktree, previous.managed_worktree);
        assert_eq!(cleaned.bundle_id, previous.bundle_id);

        // Even when the leftover target could not be removed, the record must
        // describe the checkout it still owns.
        let mut stranded = converted;
        apply_failed_resume_rollback(
            &mut stranded,
            &previous,
            "podman is unavailable",
            Some("podman rm failed".into()),
        );
        assert_eq!(stranded.state, SessionState::Error);
        assert_eq!(stranded.project_directory, previous.project_directory);
        assert_eq!(stranded.managed_worktree, previous.managed_worktree);
        assert_eq!(stranded.bundle_id, previous.bundle_id);
    }
    #[test]
    fn cancelled_new_session_cleanup_removes_managed_worktree_and_branch() {
        let repository = committed_repository();
        let session_id = "0123456789abcdef0123456789abcdef";
        let worktree = ManagedWorktree {
            source_project_directory: repository.path().to_path_buf(),
            source_repository: repository.path().to_path_buf(),
            worktree_root: repository.path().join(".mj/worktrees").join(session_id),
            branch: format!("mj/{session_id}"),
            target: ManagedWorktreeTarget::Local,
            base_commit: None,
        };
        create_managed_worktree(
            &ProcessExecutor,
            &worktree,
            None,
            PrimaryCheckoutRequirement::Clean,
        )
        .unwrap();

        let mut session = checkpoint_test_session(session_id);
        session.project_directory = Some(worktree.worktree_root.clone());
        session.managed_worktree = Some(worktree.clone());
        let controller = Controller {
            config: Config::default(),
            state: State {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..State::default()
            },
        };
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let executor = CancellableProcessExecutor::new(cancelled);

        controller
            .cleanup_new_session_worktree_after_failure(session_id, &executor)
            .unwrap();

        assert!(!worktree.worktree_root.exists());
        assert!(!repository.path().join(".mj").exists());
        let branch = Command::new("git")
            .arg("-C")
            .arg(repository.path())
            .args([
                "show-ref",
                "--verify",
                &format!("refs/heads/{}", worktree.branch),
            ])
            .output()
            .unwrap();
        assert!(!branch.status.success());
    }
    #[test]
    fn managed_raw_worktree_refuses_dirty_primary_and_skips_existing_worktree() {
        let repository = committed_repository();
        std::fs::write(repository.path().join("dirty.txt"), "dirty\n").unwrap();
        let target = ManagedWorktreeTarget::Local;
        let inspection = inspect_raw_project(&ProcessExecutor, &target, repository.path()).unwrap();
        let session_id = "fedcba9876543210fedcba9876543210";
        let managed = ManagedWorktree {
            source_project_directory: inspection.source_project_directory,
            source_repository: inspection.source_repository,
            worktree_root: repository.path().join(".mj/worktrees").join(session_id),
            branch: format!("mj/{session_id}"),
            target: target.clone(),
            base_commit: None,
        };
        let error = create_managed_worktree(
            &ProcessExecutor,
            &managed,
            None,
            PrimaryCheckoutRequirement::Clean,
        )
        .unwrap_err();
        assert!(error.to_string().contains("uncommitted changes"));
        assert!(!managed.worktree_root.exists());

        std::fs::remove_file(repository.path().join("dirty.txt")).unwrap();
        let existing = repository.path().join("existing-worktree");
        test_git(
            repository.path(),
            &[
                "worktree",
                "add",
                "--detach",
                &existing.to_string_lossy(),
                "HEAD",
            ],
        );
        let linked = inspect_raw_project(&ProcessExecutor, &target, &existing).unwrap();
        assert!(!linked.primary_checkout);
    }
    #[test]
    fn managed_worktree_preflight_preserves_colliding_branch_and_directory() {
        let repository = committed_repository();
        let target = ManagedWorktreeTarget::Local;
        let session_id = "abcdef0123456789abcdef0123456789";
        let branch = format!("mj/{session_id}");
        test_git(repository.path(), &["branch", &branch]);
        let worktree = ManagedWorktree {
            source_project_directory: repository.path().to_path_buf(),
            source_repository: repository.path().to_path_buf(),
            worktree_root: repository.path().join(".mj/worktrees").join(session_id),
            branch: branch.clone(),
            target,
            base_commit: None,
        };

        let error = ensure_managed_worktree_available(&ProcessExecutor, &worktree).unwrap_err();
        assert!(error.to_string().contains("branch already exists"));
        assert!(
            !test_git(
                repository.path(),
                &["show-ref", "--verify", &format!("refs/heads/{branch}")]
            )
            .is_empty()
        );
        std::fs::create_dir_all(&worktree.worktree_root).unwrap();
        let error = ensure_managed_worktree_available(&ProcessExecutor, &worktree).unwrap_err();
        assert!(error.to_string().contains("path already exists"));
        assert!(worktree.worktree_root.is_dir());
    }
    #[test]
    fn managed_worktree_ssh_commands_preserve_hostile_path_boundaries() {
        let target = ManagedWorktreeTarget::Ssh {
            destination: "builder".into(),
            ssh_args: vec!["-o".into(), "BatchMode=yes".into()],
        };
        let command = managed_git_command(
            &target,
            Path::new("/srv/project with ' quote"),
            ["worktree", "prune"],
            "prune test",
        );
        assert_eq!(command.program, "ssh");
        assert_eq!(&command.args[..3], ["-o", "BatchMode=yes", "builder"]);
        assert_eq!(
            command.args[3],
            "'git' '-C' '/srv/project with '\\'' quote' 'worktree' 'prune'"
        );
    }
}
