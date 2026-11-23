use super::*;

mod operation;
pub(super) use operation::validate_operation_state_tar_reader;

/// A command boundary that lets Git collection and restore be tested without
/// invoking the host's Git executable.
pub trait GitCommandRunner: Sync {
    fn run(&self, repository: &Path, command: &GitCommand) -> Result<GitOutput>;

    /// Run `command` with its stdout written directly to `output` rather than
    /// buffered in memory. The returned `GitOutput::stdout` is empty.
    fn run_to_file(
        &self,
        repository: &Path,
        command: &GitCommand,
        mut output: fs::File,
    ) -> Result<GitOutput> {
        let result = self.run(repository, command)?;
        output.write_all(&result.stdout)?;
        Ok(GitOutput {
            status: result.status,
            stdout: Vec::new(),
            stderr: result.stderr,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitCommand {
    pub arguments: Vec<OsString>,
    pub stdin: Vec<u8>,
    /// Extra environment for this one command. The review capture points Git
    /// at a scratch index through `GIT_INDEX_FILE`, which is the only way to
    /// stage a tree without touching the repository's real index.
    pub env: Vec<(OsString, OsString)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Environment that keeps a Git child from waiting on a person. No command run
/// through [`SystemGit`] has an operator watching its terminal - a checkpoint
/// export shares the controller's - so a credential prompt or a promisor lazy
/// fetch would hold the command open until its caller's deadline. Both have to
/// fail fast instead.
pub const NON_INTERACTIVE_GIT_ENV: [(&str, &str); 2] =
    [("GIT_TERMINAL_PROMPT", "0"), ("GIT_NO_LAZY_FETCH", "1")];

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemGit;

impl GitCommandRunner for SystemGit {
    fn run(&self, repository: &Path, command: &GitCommand) -> Result<GitOutput> {
        let mut process = prepare_command(repository, command);
        let output = mj_core::subprocess::run_with_input(&mut process, &command.stdin)
            .with_context(|| format!("run git in {}", repository.display()))?;
        Ok(GitOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn run_to_file(
        &self,
        repository: &Path,
        command: &GitCommand,
        output: fs::File,
    ) -> Result<GitOutput> {
        let mut process = prepare_command(repository, command);
        let output =
            mj_core::subprocess::run_with_input_to_file(&mut process, &command.stdin, output)
                .with_context(|| format!("run git in {}", repository.display()))?;
        Ok(GitOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

fn prepare_command(repository: &Path, command: &GitCommand) -> Command {
    let mut process = Command::new("git");
    process.args(&command.arguments).current_dir(repository);
    for (name, value) in NON_INTERACTIVE_GIT_ENV {
        process.env(name, value);
    }
    for (name, value) in &command.env {
        process.env(name, value);
    }
    if !command
        .env
        .iter()
        .any(|(name, _)| name == "GIT_SSH_COMMAND")
        && let Some(default) = mj_core::local_git::local_git_ssh_command()
    {
        process.env("GIT_SSH_COMMAND", default);
    }
    process
}

/// Stable identity for internal commits and reflogs; never written to user config.
pub(crate) fn synthetic_git_identity() -> Vec<(OsString, OsString)> {
    [
        ("GIT_AUTHOR_NAME", "Mjolnir"),
        ("GIT_AUTHOR_EMAIL", "mjolnir@localhost"),
        ("GIT_COMMITTER_NAME", "Mjolnir"),
        ("GIT_COMMITTER_EMAIL", "mjolnir@localhost"),
    ]
    .into_iter()
    .map(|(name, value)| (name.into(), value.into()))
    .collect()
}
/// Commit objects use a fixed date for stable IDs; reflogs must use current
/// time because Git does not list epoch-zero stash reflog entries.
pub(crate) fn synthetic_git_commit_identity() -> Vec<(OsString, OsString)> {
    let mut identity = synthetic_git_identity();
    identity.extend([
        ("GIT_AUTHOR_DATE".into(), "@0 +0000".into()),
        ("GIT_COMMITTER_DATE".into(), "@0 +0000".into()),
    ]);
    identity
}

/// How much committed history a snapshot has to carry. Committed work that is
/// already reachable from an origin ref is durable at its source, so it is
/// never bundled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHistoryMode {
    /// Bundle commits reachable from HEAD but from no `refs/remotes/origin/*`
    /// ref. Errors when the repository has no origin refs at all.
    SessionDelta,
    /// Preserve every local ref and stash entry beyond a recorded source commit.
    CloneFrom(String),
    /// Bundle commits since `merge-base(HEAD, rev)`. Errors when the revision
    /// or the merge base cannot be resolved.
    DeltaFrom(String),
    /// No committed bundle: origin serves all committed history. Dirty state
    /// and identity are still collected.
    NoBundle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCollectionSpec {
    pub id: String,
    pub relative_destination: PathBuf,
    pub history: GitHistoryMode,
    pub origin_override: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitSnapshotProgress {
    UntrackedFile {
        current: usize,
        total: usize,
        path: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntrackedFiles {
    Include,
    Exclude,
}

trait CapturePayloads: Sync {
    type Body: Default + Send + RepositoryPayload;

    fn git(
        &self,
        runner: &dyn GitCommandRunner,
        repository: &Path,
        command: GitCommand,
        action: &str,
    ) -> Result<Self::Body>;

    fn write(&self, write: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<Self::Body>;
}

struct InlinePayloads;

impl CapturePayloads for InlinePayloads {
    type Body = Vec<u8>;

    fn git(
        &self,
        runner: &dyn GitCommandRunner,
        repository: &Path,
        command: GitCommand,
        action: &str,
    ) -> Result<Self::Body> {
        git_success(runner, repository, command, action)
    }

    fn write(&self, write: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<Self::Body> {
        let mut bytes = Vec::new();
        write(&mut bytes)?;
        Ok(bytes)
    }
}

struct FilePayloads<'a>(&'a Path);

impl CapturePayloads for FilePayloads<'_> {
    type Body = StagedPayload;

    fn git(
        &self,
        runner: &dyn GitCommandRunner,
        repository: &Path,
        mut command: GitCommand,
        action: &str,
    ) -> Result<Self::Body> {
        let file = tempfile::NamedTempFile::new_in(self.0)?;
        let output =
            if command
                .arguments
                .starts_with(&["bundle".into(), "create".into(), "-".into()])
            {
                command.arguments[2] = file.path().as_os_str().to_owned();
                let output = runner.run(repository, &command)?;
                restrict_archive_permissions(file.path())?;
                output
            } else {
                runner.run_to_file(repository, &command, file.reopen()?)?
            };
        ensure!(output.status == 0, "{}", git_failure(action, &output));
        Ok(StagedPayload::File(file.into_temp_path().keep()?))
    }

    fn write(&self, write: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<Self::Body> {
        let mut file = tempfile::NamedTempFile::new_in(self.0)?;
        write(file.as_file_mut())?;
        Ok(StagedPayload::File(file.into_temp_path().keep()?))
    }
}

pub(crate) fn collect_file_backed_git_snapshot(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    spec: &GitCollectionSpec,
    untracked_files: UntrackedFiles,
    directory: &Path,
) -> Result<RepositorySnapshot<StagedPayload>> {
    collect_git_snapshot_into(
        runner,
        repository,
        spec,
        untracked_files,
        &|_| Ok(()),
        &FilePayloads(directory),
    )
}

pub fn collect_git_snapshot(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    spec: &GitCollectionSpec,
) -> Result<RepositorySnapshot> {
    collect_git_snapshot_with_progress(runner, repository, spec, UntrackedFiles::Include, &|_| {
        Ok(())
    })
}

pub fn collect_git_snapshot_with_progress(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    spec: &GitCollectionSpec,
    untracked_files: UntrackedFiles,
    progress: &(dyn Fn(GitSnapshotProgress) -> Result<()> + Sync),
) -> Result<RepositorySnapshot> {
    collect_git_snapshot_into(
        runner,
        repository,
        spec,
        untracked_files,
        progress,
        &InlinePayloads,
    )
}

fn collect_git_snapshot_into<P: CapturePayloads>(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    spec: &GitCollectionSpec,
    untracked_files: UntrackedFiles,
    progress: &(dyn Fn(GitSnapshotProgress) -> Result<()> + Sync),
    payloads: &P,
) -> Result<RepositorySnapshot<P::Body>> {
    validate_component(&spec.id, "repository id")?;
    validate_archive_relative_path(&spec.relative_destination)?;

    // A session can work in a subdirectory of its checkout, but its work is
    // the whole checkout's, and a managed checkout is deleted on Stop. Every
    // command runs from the top level, so work outside the subdirectory is
    // kept and every path is named from where a restore starts.
    let checkout = locate_checkout(runner, repository)?;
    let repository = checkout.top_level.as_path();
    let identity = collect_git_identity(runner, repository, spec.origin_override.as_deref())?;
    let clone_state = match &spec.history {
        GitHistoryMode::CloneFrom(base) => {
            Some(collect_clone_refs(runner, repository, base, payloads)?)
        }
        _ => None,
    };
    // Temporary refs made for linked worktrees; removed with the clone's on
    // every exit path below.
    let mut worktree_refs = Vec::new();
    let captured = (|| -> Result<RepositorySnapshot<P::Body>> {
        let mut history = match &clone_state {
            Some(state) => state.history.clone(),
            None => select_git_history(runner, repository, &spec.history, &identity.head_commit)?,
        };
        let review_refs = if clone_state.is_none() {
            collect_review_refs(runner, repository)?
        } else {
            BTreeMap::new()
        };
        include_review_history(&mut history, &review_refs);
        let linked = discover_linked_worktrees(runner, repository)?;
        let linked_directories = linked
            .iter()
            .map(|worktree| worktree.directory.clone())
            .collect::<BTreeSet<_>>();
        let prepared = prepare_linked_worktrees(
            runner,
            linked,
            clone_state.is_some(),
            &mut worktree_refs,
            payloads,
        )?;
        include_worktree_history(
            runner,
            repository,
            &spec.history,
            &mut history,
            &prepared,
            &worktree_refs,
        )?;
        let unmerged_paths = clone_state
            .as_ref()
            .and_then(|state| state.operation.as_ref())
            .map(operation::CollectedOperation::unmerged_paths)
            .unwrap_or_default();
        let mut snapshot = collect_git_contents(
            runner,
            &checkout,
            spec,
            identity,
            history,
            &unmerged_paths,
            untracked_files,
            &linked_directories,
            progress,
            payloads,
        )?;
        snapshot.linked_worktrees = collect_linked_worktree_contents(
            runner,
            prepared,
            untracked_files,
            &linked_directories,
            progress,
            payloads,
        )?;
        if clone_state.is_none() && !review_refs.is_empty() {
            snapshot.metadata.review_baseline_ref = Some(session_start_scope(runner, repository)?);
        }
        snapshot.metadata.saved_refs = review_refs;
        Ok(snapshot)
    })();
    let mut temporary_refs = worktree_refs;
    if let Some(state) = &clone_state {
        temporary_refs.extend(state.temporary_refs.iter().cloned());
    }
    let cleanup = remove_temporary_refs(runner, repository, &temporary_refs);
    let mut snapshot = match (captured, cleanup) {
        (Ok(snapshot), Ok(())) => snapshot,
        (Ok(_), Err(cleanup)) => return Err(cleanup),
        (Err(error), Ok(())) => return Err(error),
        (Err(error), Err(cleanup)) => {
            return Err(error.context(format!("also failed to remove temporary refs: {cleanup:#}")));
        }
    };
    if let Some(state) = clone_state {
        if state
            .saved_refs
            .keys()
            .any(|name| name.starts_with(mj_core::archive::MJOLNIR_REF_NAMESPACE))
        {
            snapshot.metadata.review_baseline_ref = Some(session_start_scope(runner, repository)?);
        }
        snapshot.metadata.saved_refs = state.saved_refs;
        snapshot.metadata.stash_stack = state.stash_stack;
        if let Some(operation) = state.operation {
            snapshot.metadata.git_operation = Some(operation.metadata);
            snapshot.operation_payloads = Some(operation.payloads);
        }
    }
    Ok(snapshot)
}

/// Collect only enough Git identity to associate native harness state with an
/// existing project, together with its durable review ref names and object IDs.
/// The project remains the recovery source for objects and working files.
pub fn collect_git_metadata_snapshot(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    spec: &GitCollectionSpec,
) -> Result<RepositorySnapshot> {
    collect_git_metadata_snapshot_with(runner, repository, spec)
}

pub(crate) fn collect_git_metadata_snapshot_with<T: Default>(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    spec: &GitCollectionSpec,
) -> Result<RepositorySnapshot<T>> {
    validate_component(&spec.id, "repository id")?;
    validate_archive_relative_path(&spec.relative_destination)?;
    let identity = collect_git_identity(runner, repository, spec.origin_override.as_deref())?;
    let saved_refs = collect_review_refs(runner, repository)?;
    // Move handoffs omit workspace payloads but still own the immutable base
    // needed to finalize the independently transferred destination checkout.
    let managed_base = remote_workspace_base(runner, repository)?;
    Ok(RepositorySnapshot {
        origin_delta_pack: None,
        metadata: RepositoryMetadata {
            origin_delta: None,
            // Every capture here is metadata only, whether or not it carries
            // refs: restore must never check out its commit into a checkout
            // other sessions or the user may be working in.
            review_refs_metadata_only: true,
            review_baseline_ref: if saved_refs.is_empty() {
                None
            } else {
                Some(session_start_scope(runner, repository)?)
            },
            saved_refs,
            stash_stack: Vec::new(),
            git_operation: None,
            id: spec.id.clone(),
            relative_destination: spec.relative_destination.clone(),
            checkout_subdirectory: None,
            origin: identity.origin,
            push_urls: identity.push_urls,
            remote_workspace: managed_base.is_some(),
            plain_directory: false,
            base_commit: managed_base.unwrap_or_else(|| identity.head_commit.clone()),
            head_commit: identity.head_commit,
            branch: identity.branch,
        },
        committed_bundle: T::default(),
        staged_patch: T::default(),
        unstaged_patch: T::default(),
        untracked_tar: T::default(),
        operation_payloads: None,
        linked_worktrees: Vec::new(),
    })
}

/// A snapshot of a bare local session's directory when it is not a Git
/// checkout.
///
/// Nothing about the folder is captured: it is the user's own directory, not a
/// managed checkout, so a restore never replaces its files. The metadata still
/// names the directory so an archive describes the shape of the session it
/// belongs to, and it carries no commit, so nothing claims Git provenance.
pub fn collect_plain_directory_snapshot(spec: &GitCollectionSpec) -> Result<RepositorySnapshot> {
    collect_plain_directory_snapshot_with(spec)
}

pub(crate) fn collect_plain_directory_snapshot_with<T: Default>(
    spec: &GitCollectionSpec,
) -> Result<RepositorySnapshot<T>> {
    validate_component(&spec.id, "repository id")?;
    validate_archive_relative_path(&spec.relative_destination)?;
    ensure!(
        spec.origin_override.is_none(),
        "a plain project directory has no origin to override"
    );
    Ok(RepositorySnapshot {
        origin_delta_pack: None,
        metadata: RepositoryMetadata {
            origin_delta: None,
            review_refs_metadata_only: false,
            review_baseline_ref: None,
            saved_refs: BTreeMap::new(),
            stash_stack: Vec::new(),
            git_operation: None,
            id: spec.id.clone(),
            relative_destination: spec.relative_destination.clone(),
            checkout_subdirectory: None,
            origin: String::new(),
            push_urls: Vec::new(),
            remote_workspace: false,
            plain_directory: true,
            base_commit: String::new(),
            head_commit: String::new(),
            branch: None,
        },
        committed_bundle: T::default(),
        staged_patch: T::default(),
        unstaged_patch: T::default(),
        untracked_tar: T::default(),
        operation_payloads: None,
        linked_worktrees: Vec::new(),
    })
}

/// A Git checkout, found from a directory somewhere inside it.
pub(crate) struct CheckoutLocation {
    pub(crate) top_level: PathBuf,
    /// The directory's path below `top_level`; `None` at the top level.
    pub(crate) subdirectory: Option<PathBuf>,
}

pub(crate) fn locate_checkout(
    runner: &dyn GitCommandRunner,
    directory: &Path,
) -> Result<CheckoutLocation> {
    let output = git_bytes(
        runner,
        directory,
        ["rev-parse", "--show-toplevel", "--show-prefix"],
        &[],
        "locate the Git checkout's top level",
    )?;
    // One line each; the prefix line is empty at the top level.
    let output = output.strip_suffix(b"\n").unwrap_or(&output);
    let (top_level, prefix) = output
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|split| (&output[..split], &output[split + 1..]))
        .context("Git did not report the checkout's top level and prefix")?;
    let top_level = mj_core::path_input::from_git_bytes(top_level)?;
    ensure!(
        top_level.is_absolute(),
        "Git reported a relative checkout top level {}",
        top_level.display()
    );
    let subdirectory: PathBuf = mj_core::path_input::from_git_bytes(prefix)?
        .components()
        .collect();
    let subdirectory = if subdirectory.as_os_str().is_empty() {
        None
    } else {
        validate_archive_relative_path(&subdirectory)?;
        Some(subdirectory)
    };
    Ok(CheckoutLocation {
        top_level,
        subdirectory,
    })
}

struct CollectedGitIdentity {
    origin: String,
    push_urls: Vec<String>,
    head_commit: String,
    branch: Option<String>,
}

fn collect_git_identity(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    origin_override: Option<&str>,
) -> Result<CollectedGitIdentity> {
    let head_commit = git_text(runner, repository, ["rev-parse", "--verify", "HEAD"])
        .context("repository has no valid Git HEAD")?;
    let origin = if let Some(origin) = origin_override {
        redact_origin_credentials(origin)?
    } else {
        let output = run_git(runner, repository, ["remote", "get-url", "origin"], &[])?;
        if output.status == 0 {
            redact_origin_credentials(&trim_output(&output.stdout, "read Git origin")?)?
        } else {
            String::new()
        }
    };
    let push_urls = collect_git_push_urls(runner, repository)?;
    let branch_output = run_git(
        runner,
        repository,
        ["symbolic-ref", "--quiet", "--short", "HEAD"],
        &[],
    )?;
    let branch = if branch_output.status == 0 {
        Some(trim_output(&branch_output.stdout, "read Git branch")?)
    } else if branch_output.status == 1 {
        None
    } else {
        return Err(git_failure("read Git branch", &branch_output));
    };
    Ok(CollectedGitIdentity {
        origin,
        push_urls,
        head_commit,
        branch,
    })
}

/// Read only explicitly configured push destinations. `git remote get-url
/// --push` falls back to the fetch URL when no push URL is configured, which
/// would lose the distinction that lets a managed workspace keep its host's
/// default push destination after restore.
fn collect_git_push_urls(runner: &dyn GitCommandRunner, repository: &Path) -> Result<Vec<String>> {
    let output = run_git(
        runner,
        repository,
        ["config", "--local", "--get-all", "remote.origin.pushurl"],
        &[],
    )?;
    match output.status {
        0 => std::str::from_utf8(&output.stdout)
            .context("decode Git push URLs")?
            .lines()
            .filter(|url| !url.trim().is_empty())
            .map(|url| redact_origin_credentials(url.trim()))
            .collect(),
        1 => Ok(Vec::new()),
        _ => Err(git_failure("read Git push URLs", &output)),
    }
}

/// Read the immutable launch base recorded on a managed network workspace.
/// Returning `None` for an absent or false marker keeps raw local DeltaFrom
/// captures separate from the managed restore path.
pub fn remote_workspace_base(
    runner: &dyn GitCommandRunner,
    repository: &Path,
) -> Result<Option<String>> {
    let marker = run_git(
        runner,
        repository,
        ["config", "--local", "--bool", "--get", "mj.remoteWorkspace"],
        &[],
    )?;
    let marker = match marker.status {
        0 => trim_output(&marker.stdout, "read managed workspace marker")?,
        1 => return Ok(None),
        _ => return Err(git_failure("read managed workspace marker", &marker)),
    };
    if marker != "true" {
        return Ok(None);
    }

    let base = run_git(
        runner,
        repository,
        ["config", "--local", "--get", "mj.baseCommit"],
        &[],
    )?;
    ensure!(
        base.status == 0,
        "managed workspace is marked true but mj.baseCommit is missing: {}",
        git_failure("read managed workspace base commit", &base)
    );
    let base = trim_output(&base.stdout, "read managed workspace base commit")?;
    ensure!(
        !base.is_empty(),
        "managed workspace is marked true but mj.baseCommit is empty"
    );
    Ok(Some(base))
}

/// Whether the recorded managed base is an available commit in this checkout.
/// A missing legacy object is distinct from a Git execution/storage failure.
pub fn managed_base_is_available(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    base: &str,
) -> Result<bool> {
    let output = run_git(
        runner,
        repository,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{base}^{{commit}}"),
        ],
        &[],
    )?;
    match output.status {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(git_failure("inspect managed base object", &output)),
    }
}

fn merge_base(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    base_revision: &str,
) -> Result<Option<String>> {
    let base = run_git(
        runner,
        repository,
        ["rev-parse", "--verify", base_revision],
        &[],
    )?;
    if base.status != 0 {
        return Ok(None);
    }
    let base = trim_output(&base.stdout, "decode Git base revision")?;
    if base.is_empty() {
        return Ok(None);
    }
    let merged = run_git(runner, repository, ["merge-base", "HEAD", &base], &[])?;
    match merged.status {
        0 => {
            let merged = trim_output(&merged.stdout, "decode Git merge base")?;
            Ok((!merged.is_empty()).then_some(merged))
        }
        1 => Ok(None),
        _ => Err(git_failure("find Git merge base", &merged)),
    }
}

/// True when the repository has at least one `refs/remotes/origin/*` ref, the
/// exclusion set every session delta is measured against.
pub fn has_origin_refs(runner: &dyn GitCommandRunner, repository: &Path) -> Result<bool> {
    let refs = git_text(
        runner,
        repository,
        [
            "for-each-ref",
            "--format=%(objectname)",
            "refs/remotes/origin",
        ],
    )
    .context("list origin refs")?;
    Ok(!refs.is_empty())
}

/// Where linked worktrees scoped their review state, as
/// `refs/mj/worktrees/<name>/review-baseline`.
const WORKTREE_REVIEW_SCOPES: &str = "refs/mj/worktrees/";

/// The name that scopes the session-start refs of the worktree `repository`
/// is. It is a name only: the ref it spells is no longer written, and this
/// reads and writes nothing in the repository.
///
/// Every linked worktree shares its repository's refs, so a session's start
/// ref is keyed by a hash of this name to keep one worktree's start from being
/// taken as another's. The main worktree uses `refs/mj/review-baseline`; a
/// linked worktree uses its own name under `refs/mj/worktrees/`.
pub fn session_start_scope(runner: &dyn GitCommandRunner, repository: &Path) -> Result<String> {
    let (git_dir, common_dir) = git_directories(runner, repository)?;
    if same_directory(&git_dir, &common_dir)? {
        return Ok("refs/mj/review-baseline".to_owned());
    }
    let name = git_dir
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("name the worktree of {}", git_dir.display()))?;
    Ok(format!("{WORKTREE_REVIEW_SCOPES}{name}/review-baseline"))
}

/// The selected push review base for a worktree, separate from obsolete
/// per-turn tree baselines and from repository-wide reviewed-tree receipts.
pub fn push_review_base_ref(scope: &str) -> String {
    format!("refs/mj/push-base/{}", digest_bytes(scope.as_bytes()))
}

/// Move a legacy session-start ref without overwriting a concurrent
/// initializer or deleting a legacy value that changed after it was read.
pub fn migrate_review_ref(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    legacy: &str,
    reference: &str,
) -> Result<()> {
    let current = run_git(
        runner,
        repository,
        ["show-ref", "--verify", "--quiet", reference],
        &[],
    )?;
    if current.status == 0 {
        return Ok(());
    }
    let old = run_git(
        runner,
        repository,
        ["rev-parse", "--verify", "--quiet", legacy],
        &[],
    )?;
    if old.status != 0 {
        return Ok(());
    }
    let object = trim_output(&old.stdout, "read the legacy session-start ref")?;
    // One Git transaction publishes the new ref and removes the old one only
    // if neither value changed. Failure is visible; no baseline is discarded.
    let transaction = format!("create {reference} {object}\ndelete {legacy} {object}\n");
    git_bytes(
        runner,
        repository,
        ["update-ref", "--stdin"],
        transaction.as_bytes(),
        "migrate the legacy session-start ref",
    )?;
    Ok(())
}

/// The worktree's own Git directory and the repository's common one, which
/// are the same directory in the main worktree.
/// Resolve Git's path output without requiring --path-format (Git 2.31).
pub(crate) fn absolute_git_path(repository: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let path = mj_core::path_input::from_git_bytes(bytes)?;
    Ok(std::path::absolute(repository.join(path))?)
}
pub(crate) fn git_directories(
    runner: &dyn GitCommandRunner,
    repository: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let git_dir = git_bytes(
        runner,
        repository,
        ["rev-parse", "--absolute-git-dir"],
        &[],
        "locate Git directory",
    )?;
    let common = git_bytes(
        runner,
        repository,
        ["rev-parse", "--git-common-dir"],
        &[],
        "locate common Git directory",
    )?;
    Ok((
        absolute_git_path(repository, &git_dir)?,
        absolute_git_path(repository, &common)?,
    ))
}

fn same_directory(left: &Path, right: &Path) -> Result<bool> {
    let canonical = |path: &Path| {
        std::fs::canonicalize(path).with_context(|| format!("resolve {}", path.display()))
    };
    Ok(canonical(left)? == canonical(right)?)
}

/// A scratch index of its own for one capture or merge, named to Git through
/// `GIT_INDEX_FILE`, so the repository's real index, working tree and HEAD are
/// never touched.
///
/// The file lives in the Git directory and is removed when this is dropped, on
/// every exit path; leaving it behind would grow the Git directory by one file
/// per review.
struct ScratchIndex {
    path: tempfile::TempPath,
}

impl ScratchIndex {
    fn create(runner: &dyn GitCommandRunner, repository: &Path) -> Result<Self> {
        let git_dir = git_text(runner, repository, ["rev-parse", "--absolute-git-dir"])
            .context("locate the Git directory for a review capture")?;
        let index = tempfile::Builder::new()
            .prefix("hel-review-index-")
            .tempfile_in(&git_dir)
            .with_context(|| format!("create a scratch Git index in {git_dir}"))?;
        // Git wants to create the index itself; an existing empty file is read
        // as a corrupt index.
        let path = index.into_temp_path();
        std::fs::remove_file(&path).ok();
        Ok(Self { path })
    }

    fn env(&self) -> Vec<(OsString, OsString)> {
        vec![(
            OsString::from("GIT_INDEX_FILE"),
            self.path.as_os_str().to_os_string(),
        )]
    }

    fn run(
        &self,
        runner: &dyn GitCommandRunner,
        repository: &Path,
        arguments: impl IntoIterator<Item = impl Into<OsString>>,
        stdin: Vec<u8>,
        action: &str,
    ) -> Result<Vec<u8>> {
        git_success(
            runner,
            repository,
            GitCommand {
                arguments: arguments.into_iter().map(Into::into).collect(),
                stdin,
                env: self.env(),
            },
            action,
        )
    }

    fn write_tree(&self, runner: &dyn GitCommandRunner, repository: &Path) -> Result<String> {
        let output = self.run(
            runner,
            repository,
            ["write-tree"],
            Vec::new(),
            "write the review capture tree",
        )?;
        let tree = trim_output(&output, "decode the review capture tree id")?;
        ensure!(!tree.is_empty(), "git write-tree produced no tree id");
        Ok(tree)
    }
}

/// Records `start` plus the current content of `paths` as a Git tree object
/// and returns its id. `start` is the commit or tree to begin from, or `None`
/// for an unborn branch, which has no tree to start from.
///
/// This is deliberately proportional to `paths` and not to the working tree.
/// The scratch index is built with `read-tree`, so it has no stat cache and
/// `git add` reads exactly the paths it is given: every other tracked file is
/// carried over from `start` without being opened, and an untracked file that
/// is not named is simply not in the tree. A capture of the whole working tree
/// would instead hash every untracked file in it, which in a large workspace is
/// hundreds of thousands of files and object writes.
///
/// After this call `git status` reports exactly what it reported before.
/// `git add -A` with a pathspec records a deletion as a deletion, and honours
/// the ignore rules, which is what makes two captures comparable as "what the
/// agent changed".
pub fn capture_paths(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    start: Option<&str>,
    paths: &[PathBuf],
) -> Result<String> {
    let scratch = ScratchIndex::create(runner, repository)?;
    scratch.run(
        runner,
        repository,
        ["read-tree", start.unwrap_or("--empty")],
        Vec::new(),
        "initialize the scratch index",
    )?;
    if !paths.is_empty() {
        scratch.run(
            runner,
            repository,
            ["add", "-A", "--"]
                .into_iter()
                .map(OsString::from)
                .chain(paths.iter().map(OsString::from)),
            Vec::new(),
            "stage the changed paths into a scratch index",
        )?;
    }
    scratch.write_tree(runner, repository)
}

/// The tree a review point names, the commit HEAD was at when it was taken,
/// where that sits in HEAD's reflog, the turn's commits no review has seen
/// yet, and the session that took it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewPoint {
    pub tree: String,
    /// `None` for an unborn branch, and for a bare tree id pinned by a release
    /// that did not record HEAD.
    pub head: Option<String>,
    /// `None` for a point recorded by a release that did not record it, and
    /// for one taken while HEAD's reflog did not end at `head`.
    pub reflog: Option<ReflogAnchor>,
    /// Commits the turn made that were not in the workspace when this point
    /// was captured, such as work committed on a branch it then left. A later
    /// review still counts them as the turn's.
    pub pending: Vec<String>,
    /// `None` for a point recorded by a release that did not record it.
    pub session: Option<String>,
}

/// Where a review point sits in its worktree's HEAD reflog: how many entries
/// the reflog held when the point was taken, and the time of the newest one.
///
/// Entries expire only from the oldest end, so the count finds the entry even
/// when one second holds several entries for one commit; the time finds it
/// again after expiry has shifted the count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReflogAnchor {
    pub entries: usize,
    /// `None` when the reflog was empty.
    pub timestamp: Option<i64>,
}

/// Records `point` as one Git object and returns its id: the id a review
/// hands on as its capture and later pins as a baseline.
///
/// The object is an annotated tag pointing at the tree, with one line per
/// field as its message: `head <commit>`, `reflog <entries> [<timestamp>]`,
/// one `pending <commit>` per pending commit, and `session <id>`. The tree and
/// its commit are one fact, so they travel as one id, and the session lets a
/// pin say whose it is. A tag is still a tree-ish, so `git diff`, `git ls-tree`
/// and `^{tree}` read it exactly as they read the bare tree ids earlier
/// releases stored, with no change to the controller's records or the relay
/// protocol. Unlike a commit, a tag pointing at a tree does not appear in
/// `git log --all`. The fixed tagger and date make equal captures equal ids.
pub fn record_review_point(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    point: &ReviewPoint,
) -> Result<String> {
    let mut tag = format!(
        "object {}\ntype tree\ntag mjolnir-review-point\n\
         tagger Mjolnir <mjolnir@localhost> 0 +0000\n\n",
        point.tree
    );
    if let Some(head) = &point.head {
        tag.push_str(&format!("head {head}\n"));
    }
    if let Some(anchor) = point.reflog {
        tag.push_str(&format!("reflog {}", anchor.entries));
        if let Some(timestamp) = anchor.timestamp {
            tag.push_str(&format!(" {timestamp}"));
        }
        tag.push('\n');
    }
    for commit in &point.pending {
        tag.push_str(&format!("pending {commit}\n"));
    }
    if let Some(session) = &point.session {
        tag.push_str(&format!("session {session}\n"));
    }
    let output = git_bytes(
        runner,
        repository,
        ["mktag"],
        tag.as_bytes(),
        "record the review point",
    )?;
    trim_output(&output, "decode the review point id")
}

/// Reads a review point, or a bare tree id from an earlier release as a point
/// with no recorded HEAD.
pub fn read_review_point(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    id: &str,
) -> Result<ReviewPoint> {
    let kind = git_bytes(
        runner,
        repository,
        ["cat-file", "-t", id],
        &[],
        "read the review point type",
    )?;
    match trim_output(&kind, "decode the review point type")?.as_str() {
        "tree" => Ok(ReviewPoint {
            tree: id.to_owned(),
            ..ReviewPoint::default()
        }),
        "tag" => {
            let tag = git_bytes(
                runner,
                repository,
                ["cat-file", "tag", id],
                &[],
                "read the review point",
            )?;
            let tag = String::from_utf8_lossy(&tag);
            let (header, message) = tag.split_once("\n\n").unwrap_or((&tag, ""));
            let field = |text: &str, name: &str| {
                text.lines()
                    .find_map(|line| line.strip_prefix(name))
                    .map(str::to_owned)
            };
            ensure!(
                field(header, "type ").as_deref() == Some("tree"),
                "review point {id} does not name a tree"
            );
            let tree = field(header, "object ")
                .with_context(|| format!("review point {id} names no object"))?;
            // An anchor that does not parse is treated as absent, which makes
            // the next review diff the whole tree rather than misattribute.
            let reflog = field(message, "reflog ").and_then(|anchor| {
                let mut parts = anchor.split_whitespace();
                let entries = parts.next()?.parse().ok()?;
                let timestamp = match parts.next() {
                    Some(timestamp) => Some(timestamp.parse().ok()?),
                    None => None,
                };
                Some(ReflogAnchor { entries, timestamp })
            });
            Ok(ReviewPoint {
                tree,
                head: field(message, "head "),
                reflog,
                pending: message
                    .lines()
                    .filter_map(|line| line.strip_prefix("pending "))
                    .map(str::to_owned)
                    .collect(),
                session: field(message, "session "),
            })
        }
        other => bail!("review point {id} is a {other}, not a tag or tree"),
    }
}

/// One entry of a worktree's HEAD reflog: the commit HEAD was set to, that
/// commit's tree and parents, when, and the action Git recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogEntry {
    pub commit: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub timestamp: i64,
    pub message: String,
}

impl ReflogEntry {
    /// Whether this entry only moved HEAD to a commit that already existed,
    /// as a checkout, reset, fast-forward, clone, or the start, end and
    /// `reset` steps of a rebase do, rather than recording a commit made in
    /// this worktree. `git rebase --rebase-merges` resets to its `onto` label
    /// between the branches it replays.
    ///
    /// Git names the action at the start of every message (`commit:`,
    /// `checkout: moving from …`, `merge topic: Fast-forward`, `rebase (pick):
    /// …`). A commit's own subject can only follow a `commit`, `cherry-pick`,
    /// `revert`, `am` or rebase action, so the moves are told apart by their
    /// action alone. A message this does not recognize, such as one written
    /// under a custom `GIT_REFLOG_ACTION`, counts as a commit made here: a
    /// review that covers too much is the safe way to be wrong.
    #[must_use]
    pub fn moves_only(&self) -> bool {
        let message = self.message.as_str();
        if message.starts_with("commit") {
            return false;
        }
        let (action, rest) = message.split_once(": ").unwrap_or((message, ""));
        message.starts_with("checkout: moving from ")
            || message.starts_with("reset: moving to ")
            || message.starts_with("clone: from ")
            || message.starts_with("branch: ")
            // `merge topic: Fast-forward` and `pull …: Fast-forward`. After
            // `cherry-pick` the same words could be a commit's subject, so
            // `cherry-pick --ff` counts as a commit.
            || ((action.starts_with("merge ") || action == "pull" || action.starts_with("pull "))
                && rest.eq_ignore_ascii_case("fast-forward"))
            || ["(start)", "(finish)", "(abort)", "(reset)"]
                .iter()
                .any(|step| action.ends_with(step))
    }

    /// Whether this entry records an amend, which replaces the commit HEAD
    /// held before it.
    #[must_use]
    pub fn amends(&self) -> bool {
        self.action().ends_with("(amend)")
    }

    /// Whether this entry records a commit replaying another one: a rebase
    /// step or a cherry-pick. Such a commit keeps the author, the authoring
    /// time and, unless it was reworded, the subject of the one it replays.
    #[must_use]
    pub fn replays(&self) -> bool {
        let action = self.action();
        action.starts_with("rebase")
            || action.contains("--rebase")
            || action.starts_with("cherry-pick")
    }

    fn action(&self) -> &str {
        self.message
            .split_once(": ")
            .map_or(self.message.as_str(), |(action, _)| action)
    }
}

/// The worktree's HEAD reflog, oldest entry first, or no entries when HEAD has
/// no reflog.
///
/// A linked worktree has a HEAD reflog of its own, so this is the history of
/// one checkout. `git log -g` shows the current commit as an entry even when
/// the reflog is gone, so its existence is checked first.
pub fn read_head_reflog(
    runner: &dyn GitCommandRunner,
    repository: &Path,
) -> Result<Vec<ReflogEntry>> {
    let exists = run_git(runner, repository, ["reflog", "exists", "HEAD"], &[])?;
    match exists.status {
        0 => {}
        1 => return Ok(Vec::new()),
        _ => return Err(git_failure("check for a HEAD reflog", &exists)),
    }
    let output = git_bytes(
        runner,
        repository,
        [
            "log",
            "--walk-reflogs",
            "--no-abbrev",
            "--no-show-signature",
            "--date=raw",
            "-z",
            "--format=%H%x1f%T%x1f%P%x1f%gd%x1f%gs",
            "HEAD",
        ],
        &[],
        "read the HEAD reflog",
    )?;
    let mut entries = output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            let record = String::from_utf8_lossy(record);
            let fields = record.splitn(5, '\x1f').collect::<Vec<_>>();
            let [commit, tree, parents, selector, message] = fields[..] else {
                bail!("malformed HEAD reflog entry {record:?}");
            };
            // `--date=raw` makes the selector `HEAD@{<seconds> <zone>}`.
            let timestamp = selector
                .split_once('{')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .and_then(|seconds| seconds.parse().ok())
                .with_context(|| format!("HEAD reflog selector {selector:?} has no time"))?;
            Ok(ReflogEntry {
                commit: commit.trim().to_owned(),
                tree: tree.to_owned(),
                parents: parents.split_whitespace().map(str::to_owned).collect(),
                timestamp,
                message: message.to_owned(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    entries.reverse();
    Ok(entries)
}

/// `tree` with `paths` removed, written as a new tree object through a scratch
/// index. Paths `tree` does not hold are ignored.
pub fn tree_without_paths(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    tree: &str,
    paths: &[PathBuf],
) -> Result<String> {
    let scratch = ScratchIndex::create(runner, repository)?;
    scratch.run(
        runner,
        repository,
        ["read-tree", tree],
        Vec::new(),
        "read the tree into a scratch index",
    )?;
    let mut stdin = Vec::new();
    for path in paths {
        stdin.extend_from_slice(path.as_os_str().as_encoded_bytes());
        stdin.push(0);
    }
    scratch.run(
        runner,
        repository,
        ["update-index", "--force-remove", "-z", "--stdin"],
        stdin,
        "remove paths from the scratch index",
    )?;
    scratch.write_tree(runner, repository)
}

/// What [`merge_trees`] records where both sides changed the same lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeConflicts {
    /// Conflict markers holding both sides.
    Mark,
    /// The `ours` side's lines.
    KeepOurs,
}

/// Three-way merges `theirs` into `ours` from their common `ancestor` (`None`
/// for histories with no common ancestor) and returns the resulting tree.
///
/// This is Git's resolve strategy run in a scratch index, and it works on any
/// Git a worker may have; `git merge-tree --write-tree` needs Git 2.38.
/// `read-tree -m` settles every path only one side changed. A path both sides
/// changed as regular files is merged with `git merge-file`, which settles
/// lines where the changes overlap as `conflicts` says. Any other path both
/// sides changed, and a file `merge-file` cannot merge such as a binary one,
/// keeps `ours`. There is no rename detection: a file `theirs` renamed while
/// `ours` changed it ends up under both names.
pub fn merge_trees(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    ancestor: Option<&str>,
    ours: &str,
    theirs: &str,
    conflicts: MergeConflicts,
) -> Result<String> {
    let ancestor = match ancestor {
        Some(ancestor) => ancestor.to_owned(),
        None => empty_tree_id(runner, repository)?,
    };
    let scratch = ScratchIndex::create(runner, repository)?;
    scratch.run(
        runner,
        repository,
        [
            "read-tree",
            "-m",
            "-i",
            "--aggressive",
            &ancestor,
            ours,
            theirs,
        ],
        Vec::new(),
        "merge the trees into a scratch index",
    )?;
    let unmerged = scratch.run(
        runner,
        repository,
        ["ls-files", "--unmerged", "-z"],
        Vec::new(),
        "list the paths both sides changed",
    )?;
    let unmerged = unmerged_paths(&unmerged)?;
    if !unmerged.is_empty() {
        let resolutions = resolve_conflicts(runner, repository, &unmerged, conflicts)?;
        scratch.run(
            runner,
            repository,
            ["update-index", "-z", "--index-info"],
            resolutions,
            "record the resolved paths",
        )?;
    }
    scratch.write_tree(runner, repository)
}

/// One index entry: its mode and object id.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexEntry {
    mode: String,
    object: String,
}

impl IndexEntry {
    fn is_regular_file(&self) -> bool {
        self.mode == "100644" || self.mode == "100755"
    }
}

/// A path `read-tree -m` left unmerged, with its ancestor, ours and theirs
/// entries (index stages 1, 2 and 3); a side without the path has no entry.
#[derive(Debug, Default)]
struct UnmergedPath {
    path: Vec<u8>,
    stages: [Option<IndexEntry>; 3],
}

impl UnmergedPath {
    fn ours(&self) -> Option<&IndexEntry> {
        self.stages[1].as_ref()
    }

    /// The ancestor, ours and theirs entries when both sides changed the path
    /// as regular files, which is the one kind of conflict merged by content.
    fn content_merge(&self) -> Option<(Option<&IndexEntry>, &IndexEntry, &IndexEntry)> {
        let [ancestor, Some(ours), Some(theirs)] = &self.stages else {
            return None;
        };
        let ancestor = ancestor.as_ref().filter(|entry| entry.is_regular_file());
        (ours.is_regular_file() && theirs.is_regular_file()).then_some((ancestor, ours, theirs))
    }

    /// The `update-index -z --index-info` record that settles this path at
    /// `entry`, or removes it when there is none.
    fn index_info(&self, entry: Option<&IndexEntry>) -> Vec<u8> {
        let mut record = match entry {
            Some(entry) => format!("{} {}\t", entry.mode, entry.object).into_bytes(),
            // Mode 0 removes every stage of the path. Git checks the object id
            // only for the repository's id length, which any stage shows.
            None => {
                let length = self
                    .stages
                    .iter()
                    .flatten()
                    .next()
                    .map_or(40, |entry| entry.object.len());
                format!("0 {}\t", "0".repeat(length)).into_bytes()
            }
        };
        record.extend_from_slice(&self.path);
        record.push(0);
        record
    }
}

/// Reads `git ls-files --unmerged -z`: `<mode> <object> <stage>\t<path>\0`,
/// one record per stage, grouped by path.
fn unmerged_paths(output: &[u8]) -> Result<Vec<UnmergedPath>> {
    let mut paths: Vec<UnmergedPath> = Vec::new();
    for record in output.split(|byte| *byte == 0).filter(|r| !r.is_empty()) {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .context("an unmerged index entry has no path")?;
        let fields = std::str::from_utf8(&record[..tab]).context("decode an unmerged entry")?;
        let [mode, object, stage] = fields.split(' ').collect::<Vec<_>>()[..] else {
            bail!("malformed unmerged index entry {fields:?}");
        };
        let stage: usize = stage.parse().context("parse an unmerged entry's stage")?;
        ensure!((1..=3).contains(&stage), "unexpected index stage {stage}");
        let path = &record[tab + 1..];
        if paths.last().is_none_or(|last| last.path != path) {
            paths.push(UnmergedPath {
                path: path.to_vec(),
                ..UnmergedPath::default()
            });
        }
        paths.last_mut().expect("pushed above").stages[stage - 1] = Some(IndexEntry {
            mode: mode.to_owned(),
            object: object.to_owned(),
        });
    }
    Ok(paths)
}

/// Settles every unmerged path and returns the `update-index -z --index-info`
/// input that records the results.
fn resolve_conflicts(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    conflicts: &[UnmergedPath],
    lines: MergeConflicts,
) -> Result<Vec<u8>> {
    let blobs = read_blobs(
        runner,
        repository,
        conflicts
            .iter()
            .filter_map(UnmergedPath::content_merge)
            .flat_map(|(ancestor, ours, theirs)| ancestor.into_iter().chain([ours, theirs]))
            .map(|entry| entry.object.as_str()),
    )?;
    let files = tempfile::tempdir().context("create a directory for merging conflicted files")?;
    let mut index_info = Vec::new();
    let mut merged = Vec::new();
    for (number, conflict) in conflicts.iter().enumerate() {
        let file = match conflict.content_merge() {
            Some(sides) => merge_file(
                runner,
                repository,
                &files.path().join(number.to_string()),
                sides,
                &blobs,
                lines,
            )?,
            None => None,
        };
        match file {
            Some(file) => merged.push((conflict, file)),
            None => index_info.extend(conflict.index_info(conflict.ours())),
        }
    }
    if merged.is_empty() {
        return Ok(index_info);
    }
    let list = merged
        .iter()
        .flat_map(|(_, (_, file))| {
            let mut line = file.as_os_str().as_encoded_bytes().to_vec();
            line.push(b'\n');
            line
        })
        .collect::<Vec<_>>();
    let ids = git_bytes(
        runner,
        repository,
        ["hash-object", "-w", "--no-filters", "--stdin-paths"],
        &list,
        "store the merged files",
    )?;
    let ids = std::str::from_utf8(&ids).context("decode the merged file ids")?;
    let ids = ids.lines().collect::<Vec<_>>();
    ensure!(
        ids.len() == merged.len(),
        "git hash-object stored {} of {} merged files",
        ids.len(),
        merged.len()
    );
    for ((conflict, (mode, _)), object) in merged.into_iter().zip(ids) {
        let entry = IndexEntry {
            mode,
            object: object.to_owned(),
        };
        index_info.extend(conflict.index_info(Some(&entry)));
    }
    Ok(index_info)
}

/// Merges one file's three versions with `git merge-file` and returns the
/// merged file and its mode, or `None` when Git cannot merge it, such as a
/// binary file.
fn merge_file(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    stem: &Path,
    (ancestor, ours, theirs): (Option<&IndexEntry>, &IndexEntry, &IndexEntry),
    blobs: &BTreeMap<String, Vec<u8>>,
    conflicts: MergeConflicts,
) -> Result<Option<(String, PathBuf)>> {
    let write = |side: &str, entry: Option<&IndexEntry>| -> Result<OsString> {
        let path = stem.with_extension(side);
        let content = match entry {
            Some(entry) => blobs
                .get(&entry.object)
                .with_context(|| format!("blob {} was not read", entry.object))?
                .as_slice(),
            None => &[],
        };
        std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
        Ok(path.into_os_string())
    };
    let current = write("ours", Some(ours))?;
    let mut arguments: Vec<OsString> = ["merge-file", "-q"]
        .into_iter()
        .chain((conflicts == MergeConflicts::KeepOurs).then_some("--ours"))
        .chain(["-L", "reviewed", "-L", "ancestor", "-L", "merged in"])
        .map(OsString::from)
        .collect();
    arguments.extend([
        current.clone(),
        write("ancestor", ancestor)?,
        write("theirs", Some(theirs))?,
    ]);
    let output = runner.run(
        repository,
        &GitCommand {
            arguments,
            stdin: Vec::new(),
            env: Vec::new(),
        },
    )?;
    // The status is the number of conflicts, capped at 127; anything else is
    // an error and leaves the file unmerged.
    if !(0..=127).contains(&output.status) {
        return Ok(None);
    }
    // A mode only one side changed takes that side's mode.
    let mode = match ancestor {
        Some(ancestor) if ancestor.mode == ours.mode => theirs.mode.clone(),
        _ => ours.mode.clone(),
    };
    Ok(Some((mode, PathBuf::from(current))))
}

/// Reads blobs in one `git cat-file --batch`, keyed by object id.
fn read_blobs<'a>(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    objects: impl Iterator<Item = &'a str>,
) -> Result<BTreeMap<String, Vec<u8>>> {
    let objects = objects.collect::<BTreeSet<_>>();
    if objects.is_empty() {
        return Ok(BTreeMap::new());
    }
    let request = objects
        .iter()
        .map(|object| format!("{object}\n"))
        .collect::<String>();
    let output = git_bytes(
        runner,
        repository,
        ["cat-file", "--batch"],
        request.as_bytes(),
        "read the conflicted files",
    )?;
    // Each answer is `<id> <type> <size>\n<content>\n`, in request order.
    let mut blobs = BTreeMap::new();
    let mut rest = &output[..];
    for object in objects {
        let end = rest
            .iter()
            .position(|byte| *byte == b'\n')
            .context("git cat-file --batch ended early")?;
        let header = std::str::from_utf8(&rest[..end]).context("decode a cat-file header")?;
        let size: usize = header
            .rsplit(' ')
            .next()
            .and_then(|size| size.parse().ok())
            .with_context(|| format!("git cat-file could not read {object}: {header}"))?;
        let content = rest
            .get(end + 1..end + 1 + size)
            .context("git cat-file --batch output is truncated")?;
        blobs.insert(object.to_owned(), content.to_vec());
        rest = rest.get(end + 2 + size..).unwrap_or_default();
    }
    Ok(blobs)
}

/// Records the working tree, tracked and untracked alike, as a Git tree object
/// and returns its id.
///
/// The staging runs against a scratch index file named by `GIT_INDEX_FILE`, so
/// the repository's real index, working tree and HEAD are untouched: after this
/// call `git status` reports exactly what it reported before. Untracked ignored files stay
/// out, because `git add -A` honours the ignore rules, which is what makes two
/// captures comparable as "what the agent changed".
pub fn capture_worktree_tree(runner: &dyn GitCommandRunner, repository: &Path) -> Result<String> {
    let scratch = ScratchIndex::create(runner, repository)?;
    let (_, real_index) = operation::git_paths(runner, repository, ["index"].into_iter())
        .context("locate the real Git index")?
        .pop()
        .context("Git omitted the real index location")?;
    match std::fs::copy(&real_index, &scratch.path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let head = run_git(
                runner,
                repository,
                ["rev-parse", "--verify", "--quiet", "HEAD"],
                &[],
            )?;
            ensure!(
                head.status == 0 || head.status == 1,
                "{}",
                git_failure("resolve capture HEAD", &head)
            );
            let base = if head.status == 0 { "HEAD" } else { "--empty" };
            scratch.run(
                runner,
                repository,
                ["read-tree", base],
                Vec::new(),
                "initialize the scratch index",
            )?;
        }
        Err(error) => return Err(error).context("copy the real Git index for capture"),
    }
    // `:/` names the whole working tree even when `repository` is a
    // subdirectory of its checkout; `.` would stage only that subdirectory.
    scratch.run(
        runner,
        repository,
        ["add", "-A", "--", ":/"],
        Vec::new(),
        "stage the working tree into a scratch index",
    )?;
    scratch.write_tree(runner, repository)
}

/// Points `reference` at `tree` so the capture survives garbage collection.
///
/// A failure here is not fatal to a review -- the objects are still reachable
/// until the next gc -- so the caller logs it rather than losing the capture.
pub fn pin_review_tree(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    reference: &str,
    tree: &str,
) -> Result<()> {
    git_bytes(
        runner,
        repository,
        ["update-ref", reference, tree],
        &[],
        "pin the review capture tree",
    )
    .map(|_| ())
}

/// Records a clean review of `tree` at `commit`.
pub fn record_reviewed_tree(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    tree: &str,
    commit: &str,
) -> Result<()> {
    pin_review_tree(
        runner,
        repository,
        &format!("refs/mj/reviewed/{tree}"),
        commit,
    )
}

/// Records an overridden review of `tree` at `commit`. A push of the tree is
/// allowed, but capture never takes the commit as the base of the next review.
pub fn record_overridden_tree(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    tree: &str,
    commit: &str,
) -> Result<()> {
    pin_review_tree(
        runner,
        repository,
        &format!("refs/mj/overridden/{tree}"),
        commit,
    )
}

/// The empty tree of this repository's object format, read from Git rather
/// than hardcoded so a SHA-256 repository works as well as a SHA-1 one.
pub fn empty_tree_id(runner: &dyn GitCommandRunner, repository: &Path) -> Result<String> {
    let output = runner.run(
        repository,
        &GitCommand {
            arguments: vec![OsString::from("mktree")],
            stdin: Vec::new(),
            env: Vec::new(),
        },
    )?;
    ensure!(
        output.status == 0,
        "{}",
        git_failure("compute the empty tree id", &output)
    );
    trim_output(&output.stdout, "decode the empty tree id")
}

/// Unified diff between two captured trees. `base` of `None` diffs against the
/// empty tree, which renders the whole capture as additions -- what a review
/// wants the first time it runs in a repository. Submodule changes are shown
/// whatever `diff.ignoreSubmodules` says, so a review cannot miss one.
pub fn diff_between_trees(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    base: Option<&str>,
    current: &str,
) -> Result<String> {
    let base = match base {
        Some(base) => base.to_owned(),
        None => empty_tree_id(runner, repository)?,
    };
    let patch = git_bytes(
        runner,
        repository,
        [
            "diff",
            "--binary",
            "--no-ext-diff",
            "--ignore-submodules=none",
            &base,
            current,
        ],
        &[],
        "diff the captured review trees",
    )?;
    Ok(String::from_utf8_lossy(&patch).into_owned())
}

/// `git diff --numstat -z` between two trees, with rename detection: one
/// record per changed file, as Git emits it. A rename's record carries an
/// empty path followed by the old and new paths; a binary file's counts are
/// `-`. [`diff_between_trees`] supplies the same comparison as a patch.
pub fn numstat_between_trees(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    base: &str,
    current: &str,
) -> Result<Vec<u8>> {
    git_bytes(
        runner,
        repository,
        [
            "diff",
            "--numstat",
            "-z",
            "-M",
            "--no-ext-diff",
            "--ignore-submodules=none",
            base,
            current,
        ],
        &[],
        "count the captured review trees' changed lines per file",
    )
}

#[derive(Clone)]
struct GitHistorySelection {
    /// Informational only; an empty string for session deltas.
    base_commit: String,
    /// Arguments for the bundle command, or None when nothing has to be sent.
    bundle_arguments: Option<Vec<String>>,
}

fn collect_review_refs(
    runner: &dyn GitCommandRunner,
    repository: &Path,
) -> Result<BTreeMap<String, String>> {
    git_text(
        runner,
        repository,
        [
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/mj/",
        ],
    )?
    .lines()
    .filter_map(|line| {
        let Some((name, oid)) = line.split_once(' ') else {
            return Some(Err(anyhow!("malformed review ref")));
        };
        // Old baseline pins are not carried: nothing reads them.
        (!mj_core::archive::is_review_baseline_pin(name))
            .then(|| Ok((name.to_owned(), oid.to_owned())))
    })
    .collect()
}

fn include_review_history(history: &mut GitHistorySelection, refs: &BTreeMap<String, String>) {
    if refs.is_empty() {
        return;
    }
    let arguments = history
        .bundle_arguments
        .get_or_insert_with(|| vec!["bundle".to_owned(), "create".to_owned(), "-".to_owned()]);
    // Before --not: these are positive tips even for SessionDelta bundles.
    arguments.splice(3..3, refs.keys().cloned());
}

struct CloneRefState<T> {
    history: GitHistorySelection,
    saved_refs: std::collections::BTreeMap<String, String>,
    stash_stack: Vec<mj_core::archive::SavedStashEntry>,
    operation: Option<operation::CollectedOperation<T>>,
    temporary_refs: Vec<String>,
}

fn collect_clone_refs<P: CapturePayloads>(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    base: &str,
    payloads: &P,
) -> Result<CloneRefState<P::Body>> {
    // An unfinished operation is part of the clone's work: it travels in the
    // snapshot, and its objects in the bundle, like a stash entry's.
    let operation = operation::collect(runner, repository, payloads)?;
    git_text(
        runner,
        repository,
        ["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )
    .context("managed clone source commit is unavailable")?;
    let mut list_refs = [
        "for-each-ref",
        "--format=%(refname) %(objectname)",
        "refs/heads",
        "refs/tags",
        "refs/notes",
        "refs/mj/",
    ]
    .map(String::from)
    .to_vec();
    if operation.is_some() {
        list_refs.extend(operation::OPERATION_REF_NAMESPACES.map(String::from));
    }
    let lines = trim_output(
        &git_bytes_owned(runner, repository, &list_refs, "list the clone's refs")?,
        "decode the clone's refs",
    )?;
    let mut saved_refs = std::collections::BTreeMap::new();
    for line in lines.lines() {
        let (name, oid) = line.split_once(' ').context("malformed saved Git ref")?;
        ensure!(!oid.is_empty(), "saved Git ref {name} has no object");
        if mj_core::archive::is_review_baseline_pin(name) {
            continue;
        }
        saved_refs.insert(name.to_owned(), oid.to_owned());
    }
    let stash_present = run_git(
        runner,
        repository,
        ["show-ref", "--verify", "--quiet", "refs/stash"],
        &[],
    )?;
    let stash_stack = match stash_present.status {
        0 => git_text(
            runner,
            repository,
            ["reflog", "show", "--format=%H%x09%gs", "refs/stash"],
        )?
        .lines()
        .map(|line| {
            let (commit, message) = line.split_once('\t').context("malformed stash reflog")?;
            Ok(mj_core::archive::SavedStashEntry {
                commit: commit.to_owned(),
                message: message.to_owned(),
            })
        })
        .collect::<Result<Vec<_>>>()?,
        1 => Vec::new(),
        _ => return Err(git_failure("inspect clone stash", &stash_present)),
    };
    let mut temporary_refs = Vec::new();
    let capture_id = mj_core::state::new_session_id()?;
    let staged = (|| -> Result<GitHistorySelection> {
        for (index, entry) in stash_stack.iter().enumerate() {
            let name = format!("refs/mj-checkpoint/{capture_id}/stash/{index:08}");
            git_bytes(
                runner,
                repository,
                ["update-ref", &name, &entry.commit],
                &[],
                "preserve stash entry for checkpoint bundle",
            )?;
            temporary_refs.push(name);
        }
        if let Some(operation) = &operation {
            operation.carry_objects(
                runner,
                repository,
                &format!("refs/mj-checkpoint/{capture_id}/operation"),
                &mut temporary_refs,
            )?;
        }
        let mut tips = vec!["HEAD".to_owned()];
        tips.extend(saved_refs.keys().cloned());
        tips.extend(temporary_refs.iter().cloned());
        let mut count_args = vec!["rev-list".to_owned(), "--count".to_owned()];
        count_args.extend(tips.iter().cloned());
        count_args.push(format!("^{base}"));
        let count = String::from_utf8(git_bytes_owned(
            runner,
            repository,
            &count_args,
            "count clone-owned commits",
        )?)?
        .trim()
        .parse::<u64>()
        .context("count clone-owned commits")?;
        let non_commit_tip = saved_refs
            .values()
            .map(|oid| git_text(runner, repository, ["cat-file", "-t", oid]))
            .collect::<Result<Vec<_>>>()?
            .iter()
            .any(|kind| kind != "commit")
            || operation
                .as_ref()
                .is_some_and(operation::CollectedOperation::carries_tags);
        let bundle_arguments = (count > 0 || non_commit_tip).then(|| {
            let mut args = vec!["bundle".to_owned(), "create".to_owned(), "-".to_owned()];
            args.extend(tips);
            args.push(format!("^{base}"));
            args
        });
        Ok(GitHistorySelection {
            base_commit: base.to_owned(),
            bundle_arguments,
        })
    })();
    match staged {
        Ok(history) => Ok(CloneRefState {
            history,
            saved_refs,
            stash_stack,
            operation,
            temporary_refs,
        }),
        Err(error) => {
            remove_temporary_refs(runner, repository, &temporary_refs)?;
            Err(error)
        }
    }
}

/// Removes the refs a capture made so the committed bundle carries stash
/// entries and operation objects.
fn remove_temporary_refs(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    names: &[String],
) -> Result<()> {
    // Every ref is attempted, so one failure leaves no others behind.
    let mut first_failure = None;
    for name in names {
        if let Err(error) = git_bytes(
            runner,
            repository,
            ["update-ref", "-d", name],
            &[],
            "remove temporary checkpoint ref",
        ) {
            first_failure.get_or_insert(error);
        }
    }
    first_failure.map_or(Ok(()), Err)
}

fn select_git_history(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    mode: &GitHistoryMode,
    head_commit: &str,
) -> Result<GitHistorySelection> {
    match mode {
        GitHistoryMode::CloneFrom(_) => unreachable!("clone history has a separate ref collector"),
        GitHistoryMode::SessionDelta => {
            // Collection stays side-effect free; callers repair missing origin
            // refs before asking for a session delta.
            ensure!(
                has_origin_refs(runner, repository)?,
                "repository has no origin refs to delta against"
            );
            let count = git_text(
                runner,
                repository,
                ["rev-list", "--count", "HEAD", "--not", "--remotes=origin"],
            )?
            .parse::<u64>()
            .context("parse committed delta count")?;
            Ok(GitHistorySelection {
                base_commit: String::new(),
                bundle_arguments: (count > 0).then(|| {
                    ["bundle", "create", "-", "HEAD", "--not", "--remotes=origin"]
                        .map(String::from)
                        .to_vec()
                }),
            })
        }
        GitHistoryMode::DeltaFrom(revision) => {
            let base_commit = merge_base(runner, repository, revision)?
                .with_context(|| format!("delta base {revision} is unresolvable"))?;
            let count = git_text(
                runner,
                repository,
                ["rev-list", "--count", &format!("{base_commit}..HEAD")],
            )?
            .parse::<u64>()
            .context("parse committed delta count")?;
            let bundle_arguments = (count > 0).then(|| {
                vec![
                    "bundle".to_owned(),
                    "create".to_owned(),
                    "-".to_owned(),
                    "HEAD".to_owned(),
                    format!("^{base_commit}"),
                ]
            });
            Ok(GitHistorySelection {
                base_commit,
                bundle_arguments,
            })
        }
        GitHistoryMode::NoBundle => Ok(GitHistorySelection {
            base_commit: head_commit.to_owned(),
            bundle_arguments: None,
        }),
    }
}

/// Git describes an unmerged path as "Unmerged path" or as a combined diff,
/// neither of which `git apply` accepts. The operation capture carries those
/// paths, so both patches leave them out and keep everything else, such as the
/// files a merge staged cleanly.
fn patch_command(arguments: &[&str], unmerged_paths: &[PathBuf]) -> GitCommand {
    GitCommand {
        arguments: arguments
            .iter()
            .map(OsString::from)
            .chain((!unmerged_paths.is_empty()).then(|| OsString::from("--")))
            .chain(unmerged_paths.iter().map(|path| {
                let mut pathspec = OsString::from(":(exclude,literal)");
                pathspec.push(path);
                pathspec
            }))
            .collect(),
        stdin: Vec::new(),
        env: Vec::new(),
    }
}

fn collect_staged_patch<P: CapturePayloads>(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    unmerged_paths: &[PathBuf],
    payloads: &P,
) -> Result<P::Body> {
    payloads.git(
        runner,
        repository,
        patch_command(
            &["diff", "--binary", "--cached", "--no-ext-diff"],
            unmerged_paths,
        ),
        "collect staged Git patch",
    )
}

fn collect_unstaged_patch<P: CapturePayloads>(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    unmerged_paths: &[PathBuf],
    payloads: &P,
) -> Result<P::Body> {
    payloads.git(
        runner,
        repository,
        patch_command(&["diff", "--binary", "--no-ext-diff"], unmerged_paths),
        "collect unstaged Git patch",
    )
}

fn collect_untracked_tar<P: CapturePayloads>(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    untracked_files: UntrackedFiles,
    linked_worktrees: &BTreeSet<PathBuf>,
    progress: &(dyn Fn(GitSnapshotProgress) -> Result<()> + Sync),
    payloads: &P,
) -> Result<P::Body> {
    if untracked_files == UntrackedFiles::Exclude {
        return Ok(P::Body::default());
    }
    let untracked = git_bytes(
        runner,
        repository,
        ["ls-files", "--others", "--exclude-standard", "-z"],
        &[],
        "list nonignored untracked files",
    )?;
    payloads.write(|output| {
        write_untracked_tar(repository, &untracked, linked_worktrees, progress, output)
    })
}

#[allow(clippy::too_many_arguments)]
fn collect_git_contents<P: CapturePayloads>(
    runner: &dyn GitCommandRunner,
    checkout: &CheckoutLocation,
    spec: &GitCollectionSpec,
    identity: CollectedGitIdentity,
    history: GitHistorySelection,
    unmerged_paths: &[PathBuf],
    untracked_files: UntrackedFiles,
    linked_worktrees: &BTreeSet<PathBuf>,
    progress: &(dyn Fn(GitSnapshotProgress) -> Result<()> + Sync),
    payloads: &P,
) -> Result<RepositorySnapshot<P::Body>> {
    let repository = checkout.top_level.as_path();
    let GitHistorySelection {
        base_commit,
        bundle_arguments,
    } = history;
    // These commands only inspect repository state and produce independent
    // payloads. Nested joins share Rayon's bounded worker pool, including when
    // several repositories are being collected at once.
    let ((committed_bundle, staged_patch), (unstaged_patch, untracked_tar)) = rayon::join(
        || {
            rayon::join(
                || match &bundle_arguments {
                    Some(arguments) => payloads.git(
                        runner,
                        repository,
                        GitCommand {
                            arguments: arguments.iter().map(OsString::from).collect(),
                            ..Default::default()
                        },
                        "create committed delta bundle",
                    ),
                    None => Ok(P::Body::default()),
                },
                || collect_staged_patch(runner, repository, unmerged_paths, payloads),
            )
        },
        || {
            rayon::join(
                || collect_unstaged_patch(runner, repository, unmerged_paths, payloads),
                || {
                    collect_untracked_tar(
                        runner,
                        repository,
                        untracked_files,
                        linked_worktrees,
                        progress,
                        payloads,
                    )
                },
            )
        },
    );
    let committed_bundle = committed_bundle?;
    let staged_patch = staged_patch?;
    let unstaged_patch = unstaged_patch?;
    let untracked_tar = untracked_tar?;

    Ok(RepositorySnapshot {
        origin_delta_pack: None,
        metadata: RepositoryMetadata {
            origin_delta: None,
            review_baseline_ref: None,
            review_refs_metadata_only: false,
            saved_refs: Default::default(),
            stash_stack: Vec::new(),
            git_operation: None,
            id: spec.id.clone(),
            relative_destination: spec.relative_destination.clone(),
            checkout_subdirectory: checkout.subdirectory.clone(),
            origin: identity.origin,
            push_urls: identity.push_urls,
            remote_workspace: false,
            plain_directory: false,
            base_commit,
            head_commit: identity.head_commit,
            branch: identity.branch,
        },
        committed_bundle,
        staged_patch,
        unstaged_patch,
        untracked_tar,
        operation_payloads: None,
        linked_worktrees: Vec::new(),
    })
}

/// A linked worktree of the captured repository that lives inside the
/// captured checkout.
#[derive(Clone)]
pub(crate) struct LinkedWorktreeDirectory {
    /// The directory below the checkout's top level.
    pub(crate) relative: PathBuf,
    /// The directory, with symbolic links resolved.
    pub(crate) directory: PathBuf,
}

/// A linked worktree whose state has been read and whose commits are held by
/// temporary refs.
struct PreparedWorktree<T> {
    location: LinkedWorktreeDirectory,
    head_commit: String,
    branch: Option<String>,
    lock_reason: Option<String>,
    operation: Option<operation::CollectedOperation<T>>,
}

/// Decode Git C-quoted paths, including octal-escaped native bytes.
pub(crate) fn unquote_git_path(path: &[u8]) -> Result<PathBuf> {
    let Some(quoted) = path.strip_prefix(b"\"") else {
        return mj_core::path_input::from_git_bytes(path);
    };
    let mut quoted = quoted
        .strip_suffix(b"\"")
        .context("unterminated quoted worktree path")?;
    let mut decoded = Vec::new();
    while let Some((&byte, rest)) = quoted.split_first() {
        quoted = rest;
        if byte != b'\\' {
            decoded.push(byte);
            continue;
        }
        let (&escape, rest) = quoted
            .split_first()
            .context("incomplete worktree path escape")?;
        quoted = rest;
        decoded.push(match escape {
            b'a' => 7,
            b'b' => 8,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 11,
            b'f' => 12,
            b'r' => b'\r',
            b'\\' | b'"' => escape,
            b'0'..=b'7' => {
                ensure!(
                    quoted.len() >= 2 && quoted[..2].iter().all(|byte| matches!(byte, b'0'..=b'7')),
                    "invalid octal worktree path escape"
                );
                let value = u16::from(escape - b'0') * 64
                    + u16::from(quoted[0] - b'0') * 8
                    + u16::from(quoted[1] - b'0');
                quoted = &quoted[2..];
                u8::try_from(value).context("worktree path octal escape exceeds one byte")?
            }
            _ => anyhow::bail!("invalid worktree path escape"),
        });
    }
    mj_core::path_input::from_git_bytes(&decoded)
}

/// Read registered checkout paths without pruning them. Old-Git path checks
/// apply only inside the requested checkout.
pub(crate) fn registered_worktree_paths(
    runner: &dyn GitCommandRunner,
    repository: &Path,
) -> Result<Vec<PathBuf>> {
    let top_level = fs::canonicalize(repository)
        .with_context(|| format!("resolve {}", repository.display()))?;
    let mut listed = run_git(
        runner,
        repository,
        ["worktree", "list", "--porcelain", "-z"],
        &[],
    )?;
    let delimiter = if listed.status == 129 {
        // Older target Git rejects -z. Its line format cannot represent a
        // newline path, so inspect registration paths before interpreting it.
        ensure!(
            !top_level.as_os_str().as_encoded_bytes().contains(&b'\n'),
            "Git cannot list newline worktree paths; upgrade Git to 2.36 or newer"
        );
        let (_, common_directory) = git_directories(runner, repository)?;
        match fs::read_dir(common_directory.join("worktrees")) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    if entry.file_type()?.is_dir() {
                        let path = match fs::read(entry.path().join("gitdir")) {
                            Ok(path) => path,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                            Err(error) => {
                                return Err(error).context("read legacy worktree registration");
                            }
                        };
                        let path = path.strip_suffix(b"\n").unwrap_or(&path);
                        let registered = mj_core::path_input::from_git_bytes(path)?;
                        let checkout = registered
                            .parent()
                            .context("worktree registration has no checkout")?;
                        if !checkout.starts_with(repository) && !checkout.starts_with(&top_level) {
                            continue;
                        }
                        ensure!(
                            !path.contains(&b'\n'),
                            "Git cannot list newline worktree paths; upgrade Git to 2.36 or newer"
                        );
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect legacy worktree registrations"),
        }
        listed = run_git(runner, repository, ["worktree", "list", "--porcelain"], &[])?;
        b'\n'
    } else {
        0
    };
    ensure!(
        listed.status == 0,
        "{}",
        git_failure("list linked worktrees", &listed)
    );
    let mut paths = Vec::new();
    for line in listed.stdout.split(|byte| *byte == delimiter) {
        let Some(path) = line.strip_prefix(b"worktree ") else {
            continue;
        };
        let path = if delimiter == b'\n' {
            unquote_git_path(path)?
        } else {
            mj_core::path_input::from_git_bytes(path)?
        };
        paths.push(path);
    }
    Ok(paths)
}

/// The linked worktrees of `repository`'s checkout that live inside it, at any
/// depth, parents before children.
///
/// A worktree is "ours" by Git's own definition of the same repository: the
/// common Git directory that `git rev-parse` names inside it is the checkout's.
/// A registered worktree outside the checkout, or whose directory is gone,
/// holds nothing this checkout's session owns and is left out; this never
/// prunes a registration.
pub(crate) fn discover_linked_worktrees(
    runner: &dyn GitCommandRunner,
    repository: &Path,
) -> Result<Vec<LinkedWorktreeDirectory>> {
    let top_level = fs::canonicalize(repository)
        .with_context(|| format!("resolve {}", repository.display()))?;
    let (_, common_directory) = git_directories(runner, repository)?;
    let mut found = Vec::new();
    for path in registered_worktree_paths(runner, repository)? {
        // `worktree list` names the user's worktrees anywhere on disk. Only a
        // path that can lie inside the checkout may fail the capture, so
        // decide that lexically, against the top level as given and resolved.
        if !path.starts_with(repository) && !path.starts_with(&top_level) {
            continue;
        }
        let directory = match fs::canonicalize(&path) {
            Ok(directory) => directory,
            // Registered, but the directory is gone: nothing to capture.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("resolve {}", path.display()));
            }
        };
        let Ok(relative) = directory.strip_prefix(&top_level) else {
            continue;
        };
        if relative.as_os_str().is_empty() || !directory.join(".git").is_file() {
            continue;
        }
        let relative = relative.to_path_buf();
        validate_worktree_relative_path(&relative)?;
        // A directory Git no longer reads as a repository is not one of ours.
        let Ok((_, common)) = git_directories(runner, &directory) else {
            continue;
        };
        if !same_directory(&common, &common_directory)? {
            continue;
        }
        found.push(LinkedWorktreeDirectory {
            relative,
            directory,
        });
    }
    found.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(found)
}

/// The absolute directories of the checkout's linked worktrees, for checks
/// that must run in each of them.
pub(crate) fn linked_worktree_directories(
    runner: &dyn GitCommandRunner,
    repository: &Path,
) -> Result<Vec<PathBuf>> {
    let checkout = locate_checkout(runner, repository)?;
    Ok(discover_linked_worktrees(runner, &checkout.top_level)?
        .into_iter()
        .map(|worktree| worktree.directory)
        .collect())
}

/// Reads each worktree's HEAD, lock and (for a clone) operation, and makes
/// temporary refs so the committed bundle carries every worktree's commits,
/// including a detached HEAD that no branch holds. Each ref is added to
/// `temporary_refs` as soon as it exists, so the caller removes it on every
/// path.
fn prepare_linked_worktrees<P: CapturePayloads>(
    runner: &dyn GitCommandRunner,
    worktrees: Vec<LinkedWorktreeDirectory>,
    capture_operations: bool,
    temporary_refs: &mut Vec<String>,
    payloads: &P,
) -> Result<Vec<PreparedWorktree<P::Body>>> {
    if worktrees.is_empty() {
        return Ok(Vec::new());
    }
    let capture_id = mj_core::state::new_session_id()?;
    let mut prepared = Vec::with_capacity(worktrees.len());
    for (index, location) in worktrees.into_iter().enumerate() {
        let directory = location.directory.as_path();
        let name = location.relative.display().to_string();
        let describe = |action: &str| format!("{action} of linked worktree {name}");
        let head_commit = git_text(
            runner,
            directory,
            ["rev-parse", "--verify", "HEAD^{commit}"],
        )
        .with_context(|| describe("read the HEAD"))?;
        let symbolic = run_git(runner, directory, ["symbolic-ref", "--quiet", "HEAD"], &[])?;
        let branch = match symbolic.status {
            0 => {
                let reference = trim_output(&symbolic.stdout, "decode worktree branch")?;
                Some(
                    reference
                        .strip_prefix("refs/heads/")
                        .with_context(|| {
                            format!("linked worktree {name} has HEAD at {reference}, not a branch")
                        })?
                        .to_owned(),
                )
            }
            1 => None,
            _ => {
                return Err(git_failure(
                    "read the branch of a linked worktree",
                    &symbolic,
                ));
            }
        };
        let locked = operation::git_paths(runner, directory, ["locked"].into_iter())
            .with_context(|| describe("find the lock"))?;
        let lock_reason = match fs::read(&locked[0].1) {
            Ok(reason) => Some(
                String::from_utf8(reason)
                    .with_context(|| describe("decode the lock reason"))?
                    .trim_end_matches('\n')
                    .to_owned(),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| describe("read the lock"));
            }
        };
        let pin = format!("refs/mj-checkpoint/{capture_id}/worktree/{index:08}");
        git_bytes(
            runner,
            directory,
            ["update-ref", &pin, &head_commit],
            &[],
            "preserve a linked worktree HEAD for the checkpoint bundle",
        )?;
        temporary_refs.push(pin);
        let operation = if capture_operations {
            let operation = operation::collect(runner, directory, payloads)
                .with_context(|| describe("capture the Git operation"))?;
            if let Some(operation) = &operation {
                operation.carry_objects(
                    runner,
                    directory,
                    &format!("refs/mj-checkpoint/{capture_id}/worktree-operation/{index:08}"),
                    temporary_refs,
                )?;
            }
            operation
        } else {
            None
        };
        prepared.push(PreparedWorktree {
            location,
            head_commit,
            branch,
            lock_reason,
            operation,
        });
    }
    Ok(prepared)
}

/// Adds every worktree's temporary refs to the committed bundle as tips, with
/// the same exclusions the checkout's own history uses.
fn include_worktree_history(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    mode: &GitHistoryMode,
    history: &mut GitHistorySelection,
    worktrees: &[PreparedWorktree<impl Sized>],
    tips: &[String],
) -> Result<()> {
    if worktrees.is_empty() {
        return Ok(());
    }
    let exclusions = match mode {
        GitHistoryMode::SessionDelta => vec!["--not".to_owned(), "--remotes=origin".to_owned()],
        GitHistoryMode::CloneFrom(_) | GitHistoryMode::DeltaFrom(_) => {
            vec![format!("^{}", history.base_commit)]
        }
        // Origin serves all committed history.
        GitHistoryMode::NoBundle => return Ok(()),
    };
    if let Some(arguments) = &mut history.bundle_arguments {
        // Before any exclusion: these are positive tips.
        arguments.splice(3..3, tips.iter().cloned());
        return Ok(());
    }
    let mut count_arguments = vec!["rev-list".to_owned(), "--count".to_owned()];
    count_arguments.extend(tips.iter().cloned());
    count_arguments.extend(exclusions.iter().cloned());
    let count = String::from_utf8(git_bytes_owned(
        runner,
        repository,
        &count_arguments,
        "count linked worktree commits",
    )?)?
    .trim()
    .parse::<u64>()
    .context("count linked worktree commits")?;
    let carries_tags = worktrees.iter().any(|worktree| {
        worktree
            .operation
            .as_ref()
            .is_some_and(operation::CollectedOperation::carries_tags)
    });
    if count > 0 || carries_tags {
        let mut arguments = vec!["bundle".to_owned(), "create".to_owned(), "-".to_owned()];
        arguments.extend(tips.iter().cloned());
        arguments.extend(exclusions);
        history.bundle_arguments = Some(arguments);
    }
    Ok(())
}

fn collect_linked_worktree_contents<P: CapturePayloads>(
    runner: &dyn GitCommandRunner,
    worktrees: Vec<PreparedWorktree<P::Body>>,
    untracked_files: UntrackedFiles,
    linked_worktrees: &BTreeSet<PathBuf>,
    progress: &(dyn Fn(GitSnapshotProgress) -> Result<()> + Sync),
    payloads: &P,
) -> Result<Vec<LinkedWorktreeSnapshot<P::Body>>> {
    worktrees
        .into_par_iter()
        .map(|worktree| {
            let directory = worktree.location.directory.as_path();
            let unmerged_paths = worktree
                .operation
                .as_ref()
                .map(operation::CollectedOperation::unmerged_paths)
                .unwrap_or_default();
            let relative = worktree.location.relative.as_path();
            let worktree_progress = |event: GitSnapshotProgress| match event {
                GitSnapshotProgress::UntrackedFile {
                    current,
                    total,
                    path,
                } => progress(GitSnapshotProgress::UntrackedFile {
                    current,
                    total,
                    path: relative.join(path),
                }),
            };
            let ((staged_patch, unstaged_patch), untracked_tar) = rayon::join(
                || {
                    rayon::join(
                        || collect_staged_patch(runner, directory, &unmerged_paths, payloads),
                        || collect_unstaged_patch(runner, directory, &unmerged_paths, payloads),
                    )
                },
                || {
                    collect_untracked_tar(
                        runner,
                        directory,
                        untracked_files,
                        linked_worktrees,
                        &worktree_progress,
                        payloads,
                    )
                },
            );
            let (git_operation, operation_payloads) = match worktree.operation {
                Some(operation) => (Some(operation.metadata), Some(operation.payloads)),
                None => (None, None),
            };
            Ok(LinkedWorktreeSnapshot {
                metadata: mj_core::archive::LinkedWorktreeMetadata {
                    path: worktree.location.relative.clone(),
                    head_commit: worktree.head_commit,
                    branch: worktree.branch,
                    lock_reason: worktree.lock_reason,
                    git_operation,
                },
                staged_patch: staged_patch
                    .with_context(|| format!("linked worktree {}", relative.display()))?,
                unstaged_patch: unstaged_patch
                    .with_context(|| format!("linked worktree {}", relative.display()))?,
                untracked_tar: untracked_tar
                    .with_context(|| format!("linked worktree {}", relative.display()))?,
                operation_payloads,
            })
        })
        .collect()
}

/// A worktree name changes when a session moves into another checkout. Keep
/// every archived ref except old baseline pins, which are not restored, and
/// publish that session's start under the destination checkout's name as well.
fn relocated_review_refs(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    metadata: &RepositoryMetadata,
    session: Option<&str>,
) -> Result<BTreeMap<String, String>> {
    let mut refs = BTreeMap::new();
    for (name, oid) in &metadata.saved_refs {
        if mj_core::archive::is_review_baseline_pin(name) {
            continue;
        }
        if metadata.review_refs_metadata_only {
            let object = run_git(runner, repository, ["cat-file", "-e", oid], &[])?;
            if object.status != 0 {
                tracing::debug!(reference = %name, object = %oid, repository = %repository.display(), "skip raw review ref whose object is unavailable");
                continue;
            }
        }
        refs.insert(name.clone(), oid.clone());
    }
    let mut restored = refs.clone();
    let Some(source) = metadata.review_baseline_ref.as_deref() else {
        return Ok(restored);
    };
    let destination = session_start_scope(runner, repository)?;
    if let Some(base) = refs.get(&push_review_base_ref(source)) {
        restored.insert(push_review_base_ref(&destination), base.clone());
    }
    let Some(session) = session else {
        return Ok(restored);
    };
    let prefix = format!(
        "refs/mj/session-start/{}/",
        digest_bytes(session.as_bytes())
    );
    for source in [
        source.to_owned(),
        source.replacen("refs/mj/", "refs/hel/", 1),
    ] {
        if let Some(start) = refs.get(&format!("{prefix}{}", digest_bytes(source.as_bytes()))) {
            restored.insert(
                format!("{prefix}{}", digest_bytes(destination.as_bytes())),
                start.clone(),
            );
            break;
        }
    }
    Ok(restored)
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct LegacyRestoreOptions {
    pub(crate) detach_on_branch_collision: bool,
}

/// Restore a snapshot into `repository`, the directory its paths are named
/// from. For a snapshot that records a `checkout_subdirectory` that is the
/// destination checkout's top level: its untracked files are named from
/// there, and `git apply` run from a subdirectory skips every path outside it.
pub fn restore_git_snapshot(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    snapshot: &RepositorySnapshot,
) -> Result<()> {
    restore_git_snapshot_for_session(runner, repository, snapshot, None, true)?;
    Ok(())
}

pub(crate) fn restore_git_snapshot_for_session(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    snapshot: &RepositorySnapshot,
    session: Option<&str>,
    restore_contents: bool,
) -> Result<Vec<String>> {
    restore_git_snapshot_for_session_with_options(
        runner,
        repository,
        snapshot,
        session,
        restore_contents,
        LegacyRestoreOptions::default(),
    )
}

pub(crate) fn restore_git_snapshot_for_session_with_options(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    snapshot: &RepositorySnapshot,
    session: Option<&str>,
    restore_contents: bool,
    options: LegacyRestoreOptions,
) -> Result<Vec<String>> {
    let mut notices = Vec::new();
    // Raw snapshots restore ref metadata only, even when the caller asks
    // to restore repositories. Other sessions may be using this checkout.
    let restore_contents = restore_contents && !snapshot.metadata.review_refs_metadata_only;
    if !restore_contents
        && !snapshot
            .metadata
            .saved_refs
            .keys()
            .any(|name| name.starts_with(mj_core::archive::MJOLNIR_REF_NAMESPACE))
    {
        return Ok(notices);
    }
    if snapshot.metadata.remote_workspace {
        mj_core::remote_git::validate_network_url(&snapshot.metadata.origin)?;
        for url in &snapshot.metadata.push_urls {
            mj_core::remote_git::validate_network_url(url)?;
        }
    }
    if restore_contents && let Some(branch) = &snapshot.metadata.branch {
        ensure_branch_available_for_checkout(runner, repository, branch)?;
    }
    let operation = match (
        &snapshot.metadata.git_operation,
        &snapshot.operation_payloads,
    ) {
        (None, None) => None,
        (Some(operation), Some(payloads)) => Some((operation, payloads)),
        _ => bail!("the snapshot's Git operation metadata and payloads disagree"),
    };
    // A bundle fetched through HEAD cannot carry a linked worktree's commits,
    // and it may have no HEAD of its own, so such a bundle is only unpacked
    // and the checkout names its commit.
    let imports_without_head = !snapshot.metadata.saved_refs.is_empty()
        || !snapshot.metadata.stash_stack.is_empty()
        || !snapshot.linked_worktrees.is_empty();
    if !snapshot.committed_bundle.is_empty() {
        let mut bundle = tempfile::NamedTempFile::new_in(repository)
            .with_context(|| format!("create temporary Git bundle in {}", repository.display()))?;
        bundle
            .write_all(&snapshot.committed_bundle)
            .context("write temporary Git bundle")?;
        bundle.flush().context("flush temporary Git bundle")?;
        let bundle_path = bundle.path().as_os_str().to_os_string();
        git_success(
            runner,
            repository,
            GitCommand {
                arguments: if imports_without_head {
                    vec![
                        OsString::from("bundle"),
                        OsString::from("unbundle"),
                        bundle_path,
                    ]
                } else {
                    vec![OsString::from("fetch"), bundle_path, OsString::from("HEAD")]
                },
                stdin: Vec::new(),
                env: Vec::new(),
            },
            "import committed delta bundle",
        )?;
    }
    let saved_refs = relocated_review_refs(runner, repository, &snapshot.metadata, session)?;
    let clone_refs =
        !snapshot.metadata.saved_refs.is_empty() || !snapshot.metadata.stash_stack.is_empty();
    let mut preserved_colliding_branch: Option<String> = None;
    if restore_contents {
        let checkout_target = if snapshot.committed_bundle.is_empty() || imports_without_head {
            snapshot.metadata.head_commit.as_str()
        } else {
            "FETCH_HEAD"
        };
        if let Some(branch) = &snapshot.metadata.branch {
            git_bytes(
                runner,
                repository,
                ["check-ref-format", "--branch", branch],
                &[],
                "validate restored branch",
            )?;
            let mut detach_for_collision = false;
            if options.detach_on_branch_collision {
                let existing = run_git(
                    runner,
                    repository,
                    [
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        &format!("refs/heads/{branch}^{{commit}}"),
                    ],
                    &[],
                )?;
                if existing.status == 0 {
                    let existing_commit = trim_output(&existing.stdout, "read existing branch")?;
                    let target_commit = trim_output(
                        &git_bytes(
                            runner,
                            repository,
                            [
                                "rev-parse",
                                "--verify",
                                &format!("{checkout_target}^{{commit}}"),
                            ],
                            &[],
                            "resolve restored commit",
                        )
                        .with_context(|| checkout_advice(snapshot))?,
                        "resolve restored commit",
                    )?;
                    if existing_commit != target_commit {
                        detach_for_collision = true;
                        preserved_colliding_branch = Some(format!("refs/heads/{branch}"));
                        notices.push(format!(
                            "Checked out detached HEAD at {target_commit} because branch '{branch}' already exists at {existing_commit} in the fresh clone."
                        ));
                    }
                } else if existing.status != 1 {
                    return Err(git_failure("read existing branch", &existing));
                }
            }
            if detach_for_collision {
                git_bytes(
                    runner,
                    repository,
                    ["checkout", "--detach", checkout_target],
                    &[],
                    "restore detached commit on branch collision",
                )
                .with_context(|| checkout_advice(snapshot))?;
            } else {
                git_bytes(
                    runner,
                    repository,
                    ["checkout", "-B", branch, checkout_target],
                    &[],
                    "restore committed branch",
                )
                .with_context(|| checkout_advice(snapshot))?;
            }
        } else {
            git_bytes(
                runner,
                repository,
                ["checkout", "--detach", checkout_target],
                &[],
                "restore detached commit",
            )
            .with_context(|| checkout_advice(snapshot))?;
        }
    }
    // Check out before moving saved refs: changing the currently checked-out
    // branch first leaves the old index and worktree staged against its new tip.
    if clone_refs {
        for (name, oid) in &saved_refs {
            if !restore_contents && !name.starts_with(mj_core::archive::MJOLNIR_REF_NAMESPACE) {
                continue;
            }
            if preserved_colliding_branch.as_deref() == Some(name.as_str()) {
                continue;
            }
            if operation.is_some()
                && operation::OPERATION_REF_NAMESPACES
                    .iter()
                    .any(|namespace| name.starts_with(namespace))
            {
                continue;
            }
            ensure!(
                name.starts_with("refs/heads/")
                    || name.starts_with("refs/tags/")
                    || name.starts_with("refs/notes/")
                    || name.starts_with(mj_core::archive::MJOLNIR_REF_NAMESPACE),
                "saved Git ref has an unsupported namespace: {name}"
            );
            git_bytes(
                runner,
                repository,
                ["check-ref-format", name],
                &[],
                "validate saved Git ref",
            )?;
            git_bytes(
                runner,
                repository,
                ["cat-file", "-e", oid],
                &[],
                "verify saved Git ref object",
            )?;
            if name.starts_with(mj_core::archive::MJOLNIR_REF_NAMESPACE) {
                let existing = run_git(
                    runner,
                    repository,
                    ["rev-parse", "--verify", "--quiet", name],
                    &[],
                )?;
                let old = if existing.status == 0 {
                    trim_output(&existing.stdout, "read destination review ref")?
                } else {
                    String::new()
                };
                if !old.is_empty() {
                    if name.starts_with("refs/mj/reviewed/")
                        || name.starts_with("refs/mj/overridden/")
                    {
                        continue;
                    }
                    let owned_start = session.is_some_and(|session| {
                        name.starts_with(&format!(
                            "refs/mj/session-start/{}/",
                            digest_bytes(session.as_bytes())
                        ))
                    });
                    if !owned_start {
                        continue;
                    }
                }
                git_bytes(
                    runner,
                    repository,
                    ["update-ref", name, oid, &old],
                    &[],
                    "restore durable review ref",
                )?;
            } else {
                git_bytes(
                    runner,
                    repository,
                    ["update-ref", name, oid],
                    &[],
                    "restore saved Git ref",
                )?;
            }
        }
        for entry in snapshot
            .metadata
            .stash_stack
            .iter()
            .rev()
            .filter(|_| restore_contents)
        {
            git_bytes(
                runner,
                repository,
                ["cat-file", "-e", &entry.commit],
                &[],
                "verify saved stash commit",
            )?;
            git_bytes(
                runner,
                repository,
                [
                    "update-ref",
                    "--create-reflog",
                    "-m",
                    &entry.message,
                    "refs/stash",
                    &entry.commit,
                ],
                &[],
                "restore saved stash entry",
            )?;
        }
    }
    if !restore_contents {
        return Ok(notices);
    }
    restore_remote_workspace_configuration(runner, repository, &snapshot.metadata)?;
    if !snapshot.staged_patch.is_empty() {
        git_bytes(
            runner,
            repository,
            ["apply", "--binary", "--index", "-"],
            &snapshot.staged_patch,
            "restore staged Git patch",
        )?;
    }
    if !snapshot.unstaged_patch.is_empty() {
        git_bytes(
            runner,
            repository,
            ["apply", "--binary", "-"],
            &snapshot.unstaged_patch,
            "restore unstaged Git patch",
        )?;
    }
    restore_untracked_tar(repository, &snapshot.untracked_tar)?;
    if let Some((operation, payloads)) = operation {
        notices.push(operation::restore(repository, operation, payloads)?);
    }
    // After the checkout is whole: a worktree cannot take a branch the
    // checkout holds, and its directory must not exist when it is added.
    notices.extend(restore_linked_worktrees(
        runner,
        repository,
        &snapshot.linked_worktrees,
    )?);
    Ok(notices)
}

/// Recreates the checkout's linked worktrees, parents before children, each
/// with the HEAD, index, files, operation and lock it was captured with.
pub(crate) fn restore_linked_worktrees(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    worktrees: &[LinkedWorktreeSnapshot],
) -> Result<Vec<String>> {
    let mut notices = Vec::new();
    let mut ordered = worktrees.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        let depth = |worktree: &LinkedWorktreeSnapshot| worktree.metadata.path.components().count();
        (depth(left), &left.metadata.path).cmp(&(depth(right), &right.metadata.path))
    });
    for worktree in ordered {
        notices.extend(
            restore_linked_worktree(runner, repository, worktree).with_context(|| {
                format!(
                    "restore linked worktree {}",
                    worktree.metadata.path.display()
                )
            })?,
        );
    }
    Ok(notices)
}

/// One entry of `git worktree list --porcelain`.
struct ListedWorktree {
    path: PathBuf,
    branch: Option<String>,
    locked: bool,
}

fn list_worktrees(runner: &dyn GitCommandRunner, repository: &Path) -> Result<Vec<ListedWorktree>> {
    let listed = git_bytes(
        runner,
        repository,
        ["worktree", "list", "--porcelain"],
        &[],
        "list restore worktrees",
    )?;
    let mut worktrees: Vec<ListedWorktree> = Vec::new();
    for line in listed.split(|byte| *byte == b'\n') {
        if let Some(path) = line.strip_prefix(b"worktree ") {
            worktrees.push(ListedWorktree {
                path: mj_core::path_input::from_git_bytes(path)?,
                branch: None,
                locked: false,
            });
        } else if let Some(worktree) = worktrees.last_mut() {
            if let Some(branch) = line.strip_prefix(b"branch refs/heads/") {
                worktree.branch = Some(
                    String::from_utf8(branch.to_vec()).context("decode a worktree branch name")?,
                );
            } else if line == b"locked" || line.starts_with(b"locked ") {
                worktree.locked = true;
            }
        }
    }
    Ok(worktrees)
}

fn restore_linked_worktree(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    worktree: &LinkedWorktreeSnapshot,
) -> Result<Vec<String>> {
    let metadata = &worktree.metadata;
    validate_worktree_relative_path(&metadata.path)?;
    ensure_no_symlink_ancestors(repository, &metadata.path)?;
    let directory = repository.join(&metadata.path);
    git_bytes(
        runner,
        repository,
        [
            "cat-file",
            "-e",
            &format!("{}^{{commit}}", metadata.head_commit),
        ],
        &[],
        "verify the linked worktree's commit",
    )?;
    match fs::symlink_metadata(&directory) {
        Ok(_) => {
            // A retry finds what an earlier attempt made; anything else
            // there is not ours to overwrite.
            ensure!(
                is_restored_linked_worktree(runner, repository, &directory, &metadata.head_commit)?,
                "{} already exists and is not a worktree of this repository at {}",
                directory.display(),
                metadata.head_commit
            );
            return Ok(Vec::new());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", directory.display()));
        }
    }

    let canonical_repository = fs::canonicalize(repository)
        .with_context(|| format!("resolve {}", repository.display()))?;
    let target = canonical_repository.join(&metadata.path);
    let listed = list_worktrees(runner, repository)?;
    // A registration whose directory is gone is reused rather than refused.
    let stale = listed.iter().find(|entry| entry.path == target);
    let mut arguments = vec![OsString::from("worktree"), OsString::from("add")];
    if let Some(stale) = stale {
        arguments.push(OsString::from("-f"));
        if stale.locked {
            arguments.push(OsString::from("-f"));
        }
    }
    let mut branch_arguments = None;
    if let Some(branch) = &metadata.branch {
        git_bytes(
            runner,
            repository,
            ["check-ref-format", "--branch", branch],
            &[],
            "validate the linked worktree's branch",
        )?;
        let current = run_git(
            runner,
            repository,
            [
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}^{{commit}}"),
            ],
            &[],
        )?;
        let existing = match current.status {
            0 => Some(trim_output(
                &current.stdout,
                "decode a worktree branch commit",
            )?),
            1 => None,
            _ => return Err(git_failure("read the linked worktree's branch", &current)),
        };
        let held_elsewhere = listed
            .iter()
            .any(|entry| entry.path != target && entry.branch.as_deref() == Some(branch));
        match existing {
            _ if held_elsewhere => tracing::warn!(
                worktree = %metadata.path.display(), branch = %branch,
                "linked worktree restored detached: its branch is checked out in another worktree"
            ),
            Some(commit) if commit != metadata.head_commit => tracing::warn!(
                worktree = %metadata.path.display(), branch = %branch,
                "linked worktree restored detached: its branch moved since the checkpoint"
            ),
            Some(_) => branch_arguments = Some(vec![OsString::from(branch)]),
            None => {
                branch_arguments = Some(Vec::new());
                arguments.extend([OsString::from("-b"), OsString::from(branch)]);
            }
        }
    }
    arguments.push(directory.clone().into_os_string());
    match branch_arguments {
        // An existing branch is named; a new one is made from the commit.
        Some(named) if !named.is_empty() => arguments.extend(named),
        Some(_) => arguments.push(OsString::from(&metadata.head_commit)),
        None => arguments.extend([
            OsString::from("--detach"),
            OsString::from(&metadata.head_commit),
        ]),
    }
    git_success(
        runner,
        repository,
        GitCommand {
            arguments,
            stdin: Vec::new(),
            env: Vec::new(),
        },
        "add the linked worktree",
    )?;
    // All or nothing: a worktree left half-restored would be taken for a
    // finished one by a retry, which skips it and loses its dirty state.
    let notices = match restore_linked_worktree_state(runner, repository, &directory, worktree) {
        Ok(notices) => notices,
        Err(error) => {
            let removed = git_success(
                runner,
                repository,
                GitCommand {
                    arguments: vec![
                        OsString::from("worktree"),
                        OsString::from("remove"),
                        OsString::from("--force"),
                        OsString::from("--force"),
                        directory.clone().into_os_string(),
                    ],
                    stdin: Vec::new(),
                    env: Vec::new(),
                },
                "remove the partly restored linked worktree",
            );
            return Err(match removed {
                Ok(_) => error,
                Err(removal) => error.context(format!("also failed to clean up: {removal:#}")),
            });
        }
    };
    Ok(notices)
}

/// Applies a linked worktree's dirty state, operation and lock to its fresh
/// checkout of the recorded commit.
fn restore_linked_worktree_state(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    directory: &Path,
    worktree: &LinkedWorktreeSnapshot,
) -> Result<Vec<String>> {
    let mut notices = Vec::new();
    let metadata = &worktree.metadata;
    let directory = directory.to_path_buf();
    if !worktree.staged_patch.is_empty() {
        git_bytes(
            runner,
            &directory,
            ["apply", "--binary", "--index", "-"],
            &worktree.staged_patch,
            "restore the linked worktree's staged Git patch",
        )?;
    }
    if !worktree.unstaged_patch.is_empty() {
        git_bytes(
            runner,
            &directory,
            ["apply", "--binary", "-"],
            &worktree.unstaged_patch,
            "restore the linked worktree's unstaged Git patch",
        )?;
    }
    restore_untracked_tar(&directory, &worktree.untracked_tar)?;
    match (&metadata.git_operation, &worktree.operation_payloads) {
        (None, None) => {}
        (Some(operation), Some(payloads)) => {
            notices.push(operation::restore(&directory, operation, payloads)?);
        }
        _ => bail!("the linked worktree's Git operation metadata and payloads disagree"),
    }
    if let Some(reason) = &metadata.lock_reason {
        let mut arguments = vec![OsString::from("worktree"), OsString::from("lock")];
        if !reason.is_empty() {
            arguments.extend([OsString::from("--reason"), OsString::from(reason)]);
        }
        arguments.push(directory.into_os_string());
        git_success(
            runner,
            repository,
            GitCommand {
                arguments,
                stdin: Vec::new(),
                env: Vec::new(),
            },
            "lock the linked worktree",
        )?;
    }
    Ok(notices)
}

/// Whether `directory` is already a worktree of `repository` at `head_commit`.
fn is_restored_linked_worktree(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    directory: &Path,
    head_commit: &str,
) -> Result<bool> {
    let Ok((_, common)) = git_directories(runner, directory) else {
        return Ok(false);
    };
    let (_, expected) = git_directories(runner, repository)?;
    if !same_directory(&common, &expected)? {
        return Ok(false);
    }
    let head = run_git(
        runner,
        directory,
        ["rev-parse", "--verify", "HEAD^{commit}"],
        &[],
    )?;
    Ok(head.status == 0 && trim_output(&head.stdout, "decode worktree HEAD")? == head_commit)
}

/// Rebuild the Git configuration that makes a managed network workspace
/// resumable. This runs after the checkout has been selected so a caller that
/// moved a session onto a different branch keeps that branch while still
/// inheriting the archived push destinations and immutable launch base.
fn restore_remote_workspace_configuration(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    metadata: &RepositoryMetadata,
) -> Result<()> {
    if !metadata.remote_workspace {
        return Ok(());
    }
    ensure!(
        !metadata.origin.is_empty(),
        "managed network workspace has no Git origin"
    );
    ensure!(
        !metadata.base_commit.is_empty(),
        "managed network workspace has no immutable base commit"
    );
    git_success(
        runner,
        repository,
        GitCommand {
            arguments: vec![
                "config".into(),
                "--local".into(),
                "--replace-all".into(),
                "remote.origin.url".into(),
                metadata.origin.clone().into(),
            ],
            stdin: Vec::new(),
            env: Vec::new(),
        },
        "restore Git origin",
    )?;

    let unset = run_git(
        runner,
        repository,
        ["config", "--local", "--unset-all", "remote.origin.pushurl"],
        &[],
    )?;
    ensure!(
        matches!(unset.status, 0 | 1 | 5),
        "{}",
        git_failure("clear restored Git push URLs", &unset)
    );
    let push_refspec = run_git(
        runner,
        repository,
        ["config", "--local", "--unset-all", "remote.origin.push"],
        &[],
    )?;
    ensure!(
        matches!(push_refspec.status, 0 | 1 | 5),
        "{}",
        git_failure("clear restored Git push refspec", &push_refspec)
    );
    for push_url in &metadata.push_urls {
        git_success(
            runner,
            repository,
            GitCommand {
                arguments: vec![
                    "config".into(),
                    "--local".into(),
                    "--add".into(),
                    "remote.origin.pushurl".into(),
                    push_url.clone().into(),
                ],
                stdin: Vec::new(),
                env: Vec::new(),
            },
            "restore Git push URL",
        )?;
    }
    // Older archives could retain the recorded base without carrying its
    // object. Adding a GC root must not turn those existing restores into
    // failures; their marker/config behavior remains unchanged.
    let retain_base = managed_base_is_available(runner, repository, &metadata.base_commit)?;
    if !retain_base {
        tracing::warn!(repository = %repository.display(), base_commit = %metadata.base_commit,
            "legacy checkpoint base object is absent; preserving its recorded base without a GC ref");
    }
    mj_core::remote_git::finalize_managed_checkout(
        &metadata.base_commit,
        retain_base,
        |arguments| {
            git_success(
                runner,
                repository,
                GitCommand {
                    arguments: arguments.iter().map(OsString::from).collect(),
                    stdin: Vec::new(),
                    env: Vec::new(),
                },
                "restore managed Git configuration",
            )?;
            Ok(())
        },
    )?;
    Ok(())
}

/// Refuse to move a checkout onto a branch owned by another worktree.
///
/// `git checkout -B` permits this on some Git versions, leaving one branch
/// attached to two worktrees. The restore path must detect that before it
/// fetches or changes the destination checkout.
fn ensure_branch_available_for_checkout(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    branch: &str,
) -> Result<()> {
    let current = run_git(
        runner,
        repository,
        ["symbolic-ref", "--quiet", "--short", "HEAD"],
        &[],
    )?;
    if current.status == 0 {
        if trim_output(&current.stdout, "decode current restore branch")? == branch {
            return Ok(());
        }
    } else {
        ensure!(
            current.status == 1,
            "{}",
            git_failure("read current restore branch", &current)
        );
    }

    // Git 2.31 has no `worktree list -z`. A branch line holds a ref name,
    // which cannot contain a newline.
    let worktrees = run_git(runner, repository, ["worktree", "list", "--porcelain"], &[])?;
    ensure!(
        worktrees.status == 0,
        "{}",
        git_failure("list restore worktrees", &worktrees)
    );
    let branch_field = format!("branch refs/heads/{branch}");
    ensure!(
        !worktrees
            .stdout
            .split(|byte| *byte == b'\n')
            .any(|field| field == branch_field.as_bytes()),
        "restore committed branch: branch {branch:?} is checked out in another worktree"
    );
    Ok(())
}

/// A snapshot without a bundle relies on origin for its committed history, so
/// name the missing commit and how to make it reachable again.
fn checkout_advice(snapshot: &RepositorySnapshot) -> String {
    let head_commit = &snapshot.metadata.head_commit;
    if snapshot.committed_bundle.is_empty() {
        format!(
            "restore commit {head_commit}: the archive carries no committed bundle, so this commit must be reachable from the repository's origin; fetch the origin ref that contains it and retry"
        )
    } else {
        format!("restore commit {head_commit}")
    }
}

pub(crate) fn run_git<const N: usize>(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    arguments: [&str; N],
    stdin: &[u8],
) -> Result<GitOutput> {
    runner.run(
        repository,
        &GitCommand {
            arguments: arguments.into_iter().map(OsString::from).collect(),
            stdin: stdin.to_vec(),
            env: Vec::new(),
        },
    )
}

fn git_success(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    command: GitCommand,
    action: &str,
) -> Result<Vec<u8>> {
    let output = runner.run(repository, &command)?;
    ensure!(output.status == 0, "{}", git_failure(action, &output));
    Ok(output.stdout)
}

pub(crate) fn git_bytes<const N: usize>(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    arguments: [&str; N],
    stdin: &[u8],
    action: &str,
) -> Result<Vec<u8>> {
    let output = run_git(runner, repository, arguments, stdin)?;
    ensure!(output.status == 0, "{}", git_failure(action, &output));
    Ok(output.stdout)
}

fn git_bytes_owned(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    arguments: &[String],
    action: &str,
) -> Result<Vec<u8>> {
    let output = runner.run(
        repository,
        &GitCommand {
            arguments: arguments.iter().map(OsString::from).collect(),
            stdin: Vec::new(),
            env: Vec::new(),
        },
    )?;
    ensure!(output.status == 0, "{}", git_failure(action, &output));
    Ok(output.stdout)
}

/// Index flags that can hide tracked edits from Git's stash machinery.
pub fn hidden_tracked_paths(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<Vec<PathBuf>> {
    let listed = git_bytes(
        runner,
        checkout,
        ["ls-files", "-v", "-z"],
        &[],
        "inspect hidden tracked files",
    )?;
    listed
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .filter(|entry| entry[0] == b'S' || entry[0].is_ascii_lowercase())
        .map(|entry| {
            ensure!(
                entry.get(1) == Some(&b' '),
                "malformed Git index flag entry"
            );
            mj_core::path_input::from_git_bytes(&entry[2..])
        })
        .collect()
}

pub(crate) fn git_text<const N: usize>(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    arguments: [&str; N],
) -> Result<String> {
    let output = run_git(runner, repository, arguments, &[])?;
    ensure!(
        output.status == 0,
        "{}",
        git_failure("run Git command", &output)
    );
    trim_output(&output.stdout, "decode Git output")
}

fn trim_output(output: &[u8], action: &str) -> Result<String> {
    Ok(std::str::from_utf8(output)
        .with_context(|| action.to_string())?
        .trim()
        .to_string())
}

pub(crate) fn git_failure(action: &str, output: &GitOutput) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow!(
        "{action} failed with status {}: {}",
        output.status,
        stderr.trim()
    )
}

/// Which working-tree files a tar holds, which decides the rules it is built,
/// checked and restored under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorktreeTar {
    /// Untracked files. A symbolic link must point inside the checkout, and a
    /// restore never replaces a file.
    Untracked,
    /// The working-tree files of unmerged paths. They are tracked content and
    /// travel as exactly as the patches carry the rest of the tracked tree:
    /// nothing is left out, a link keeps its target, and a restore replaces
    /// the file the checkout wrote at the path.
    Conflicted,
}

impl WorktreeTar {
    fn label(self) -> &'static str {
        match self {
            Self::Untracked => "untracked",
            Self::Conflicted => "conflicted",
        }
    }
}

/// `linked_worktrees` are the directories of this repository's linked
/// worktrees, which are captured as entries of their own, so a walk skips them
/// instead of refusing them as foreign nested repositories.
#[cfg(test)]
pub(super) fn build_untracked_tar(
    repository: &Path,
    nul_paths: &[u8],
    _policy: UntrackedFiles,
    linked_worktrees: &BTreeSet<PathBuf>,
    progress: &(dyn Fn(GitSnapshotProgress) -> Result<()> + Sync),
) -> Result<Vec<u8>> {
    InlinePayloads.write(|output| {
        write_untracked_tar(repository, nul_paths, linked_worktrees, progress, output)
    })
}

fn write_untracked_tar(
    repository: &Path,
    nul_paths: &[u8],
    linked_worktrees: &BTreeSet<PathBuf>,
    progress: &(dyn Fn(GitSnapshotProgress) -> Result<()> + Sync),
    output: &mut dyn Write,
) -> Result<()> {
    let paths = nul_paths
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    let total = paths.len();
    {
        let mut builder = tar::Builder::new(output);
        for (index, raw_path) in paths.into_iter().enumerate() {
            let relative = mj_core::path_input::from_git_bytes(raw_path)?;
            validate_worktree_relative_path(&relative)?;
            progress(GitSnapshotProgress::UntrackedFile {
                current: index + 1,
                total,
                path: relative.clone(),
            })?;
            let source = repository.join(&relative);
            ensure_no_symlink_ancestors(repository, &relative)?;
            let metadata = fs::symlink_metadata(&source)
                .with_context(|| format!("stat untracked path {}", source.display()))?;
            if !linked_worktrees.is_empty()
                && metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && linked_worktrees.contains(
                    &fs::canonicalize(&source)
                        .with_context(|| format!("resolve {}", source.display()))?,
                )
            {
                continue;
            }
            append_worktree_entry(
                &mut builder,
                repository,
                &relative,
                &metadata,
                WorktreeTar::Untracked,
            )?;
        }
        builder.finish().context("finish untracked-file tar")?;
    }
    Ok(())
}

/// Archives the working-tree files of unmerged paths, and returns the tar and
/// the paths that had no file.
pub(super) fn build_conflicted_worktree_tar(
    repository: &Path,
    paths: &[PathBuf],
    output: &mut dyn Write,
) -> Result<Vec<PathBuf>> {
    let mut missing = Vec::new();
    {
        let mut builder = tar::Builder::new(output);
        for relative in paths {
            validate_worktree_relative_path(relative)?;
            ensure_no_symlink_ancestors(repository, relative)?;
            let source = repository.join(relative);
            let metadata = match fs::symlink_metadata(&source) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing.push(relative.clone());
                    continue;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("stat conflicted path {}", source.display()));
                }
            };
            append_worktree_entry(
                &mut builder,
                repository,
                relative,
                &metadata,
                WorktreeTar::Conflicted,
            )?;
        }
        builder.finish().context("finish conflicted-file tar")?;
    }
    Ok(missing)
}

fn is_nested_repository(source: &Path, metadata: &fs::Metadata) -> Result<bool> {
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(false);
    }
    source
        .join(".git")
        .try_exists()
        .with_context(|| format!("inspect nested repository directory {}", source.display()))
}

fn append_worktree_entry(
    builder: &mut tar::Builder<&mut dyn Write>,
    repository: &Path,
    relative: &Path,
    metadata: &fs::Metadata,
    kind: WorktreeTar,
) -> Result<()> {
    let source = repository.join(relative);
    if metadata.file_type().is_symlink() {
        let target =
            fs::read_link(&source).with_context(|| format!("read symlink {}", source.display()))?;
        if kind == WorktreeTar::Untracked {
            validate_symlink_target(relative, &target)?;
        }
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_link_name(&target)?;
        header.set_cksum();
        builder.append_data(&mut header, relative, std::io::empty())?;
    } else if metadata.is_file() {
        let mut header = tar::Header::new_gnu();
        header.set_metadata(metadata);
        header.set_cksum();
        let mut file = File::open(&source)
            .with_context(|| format!("open {} file {}", kind.label(), source.display()))?;
        builder.append_data(&mut header, relative, &mut file)?;
    } else if metadata.is_dir() {
        if kind == WorktreeTar::Untracked && is_nested_repository(&source, metadata)? {
            bail!(
                "{NESTED_REPOSITORY_CHECKPOINT_ERROR}{}: \
                 for a disposable scratch worktree, exclude this path using the outer \
                 repository's .git/info/exclude; otherwise preserve its work separately",
                source.display()
            );
        }
        bail!(
            "unsupported {} directory at {}",
            kind.label(),
            source.display()
        );
    } else {
        bail!(
            "unsupported {} file type {:?} at {}",
            kind.label(),
            metadata.file_type(),
            source.display()
        );
    }
    Ok(())
}

pub(super) fn validate_untracked_tar(bytes: &[u8]) -> Result<()> {
    validate_untracked_tar_reader(Cursor::new(bytes))
}

pub(super) fn validate_untracked_tar_reader(reader: impl Read) -> Result<()> {
    validate_worktree_tar_reader(reader, WorktreeTar::Untracked)
}

pub(super) fn validate_conflicted_worktree_tar_reader(reader: impl Read) -> Result<()> {
    validate_worktree_tar_reader(reader, WorktreeTar::Conflicted)
}

fn validate_worktree_tar_reader(reader: impl Read, kind: WorktreeTar) -> Result<()> {
    let label = kind.label();
    let mut archive = tar::Archive::new(reader);
    for entry in archive
        .entries()
        .with_context(|| format!("read {label}-file tar"))?
    {
        let entry = entry.with_context(|| format!("read {label}-file tar entry"))?;
        let relative = entry
            .path()
            .with_context(|| format!("read {label}-file tar path"))?;
        validate_worktree_relative_path(&relative)?;
        let entry_type = entry.header().entry_type();
        ensure!(
            entry_type.is_file() || entry_type.is_symlink(),
            "{label} tar contains unsupported entry type for '{}'",
            relative.display()
        );
        if entry_type.is_symlink() {
            let target = entry
                .link_name()
                .with_context(|| format!("read {label} symlink target"))?
                .ok_or_else(|| anyhow!("{label} symlink '{}' has no target", relative.display()))?;
            if kind == WorktreeTar::Untracked {
                validate_symlink_target(&relative, &target)?;
            }
        }
    }
    Ok(())
}

pub(super) fn restore_untracked_tar(repository: &Path, bytes: &[u8]) -> Result<()> {
    restore_worktree_tar(repository, bytes, WorktreeTar::Untracked)
}

pub(super) fn restore_worktree_tar(
    repository: &Path,
    bytes: &[u8],
    kind: WorktreeTar,
) -> Result<()> {
    let label = kind.label();
    validate_worktree_tar_reader(Cursor::new(bytes), kind)?;
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    for entry in archive
        .entries()
        .with_context(|| format!("read {label}-file tar"))?
    {
        let mut entry = entry.with_context(|| format!("read {label}-file tar entry"))?;
        let relative = entry.path()?.into_owned();
        ensure_no_symlink_ancestors(repository, &relative)?;
        let destination = repository.join(&relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create directory {}", parent.display()))?;
            ensure_no_symlink_ancestors(repository, &relative)?;
        }
        match kind {
            WorktreeTar::Untracked => ensure!(
                !destination.exists(),
                "refusing to overwrite restored untracked path '{}'",
                destination.display()
            ),
            // The checkout wrote the path's committed version, which the
            // conflicted file replaces.
            WorktreeTar::Conflicted => remove_worktree_file(&destination)?,
        }
        if entry.header().entry_type().is_symlink() {
            let target = entry
                .link_name()?
                .ok_or_else(|| anyhow!("{label} symlink '{}' has no target", relative.display()))?;
            create_symlink(&target, &destination)?;
        } else {
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&destination)
                .with_context(|| format!("create {label} file {}", destination.display()))?;
            std::io::copy(&mut entry, &mut output)?;
            output.sync_all()?;
            #[cfg(unix)]
            if let Ok(mode) = entry.header().mode() {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&destination, fs::Permissions::from_mode(mode & 0o777))?;
            }
        }
    }
    Ok(())
}

/// Removes the file or symbolic link at `path`, if there is one. A directory
/// there is an error: the restored tree disagrees with the archived one.
pub(super) fn remove_worktree_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                !metadata.is_dir(),
                "refusing to replace directory '{}' with a conflicted file",
                path.display()
            );
            fs::remove_file(path).with_context(|| format!("remove {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

/// Refuse a path whose already-existing intermediate directories include a
/// symlink, so a restore can never be redirected out of `root`.
pub(crate) fn ensure_no_symlink_ancestors(root: &Path, relative: &Path) -> Result<()> {
    let mut current = root.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        let Component::Normal(component) = component else {
            bail!("unsafe path '{}'", relative.display());
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "path '{}' traverses a symlink",
                relative.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", current.display()));
            }
        }
    }
    Ok(())
}

pub fn validate_symlink_target(link_path: &Path, target: &Path) -> Result<()> {
    ensure!(
        !target.is_absolute(),
        "symlink '{}' has an absolute target",
        link_path.display()
    );
    let mut depth = link_path
        .parent()
        .map(|parent| parent.components().count())
        .unwrap_or(0);
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir => {
                bail!("symlink '{}' escapes the repository", link_path.display())
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("symlink '{}' escapes the repository", link_path.display())
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, destination: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, destination)
        .with_context(|| format!("create symlink {}", destination.display()))
}

#[cfg(windows)]
fn create_symlink(target: &Path, destination: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(target, destination)
        .with_context(|| format!("create symlink {}", destination.display()))
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &Path, _destination: &Path) -> Result<()> {
    bail!("symlink restore is not supported on this platform")
}

// ---------------------------------------------------------------------------
// Session export
// ---------------------------------------------------------------------------

/// Exit code the worker's export subcommands use for a precondition the caller
/// can fix, such as a repository with no push remote or a session with no
/// recorded base. Anything else is an ordinary failure, and clap's exit code 2
/// stays reserved for a worker too old to know these subcommands at all.
pub const EXPORT_REFUSED_EXIT_CODE: i32 = 3;

/// Why a session export could not be produced.
///
/// A refusal is a precondition the caller can act on, and it survives the
/// worker process boundary as [`EXPORT_REFUSED_EXIT_CODE`] plus the reason on
/// standard error, so the API can answer 409 with that reason instead of 500.
#[derive(Debug)]
pub enum SessionExportError {
    Refused(String),
    Failed(anyhow::Error),
}

impl SessionExportError {
    pub fn refused(reason: impl Into<String>) -> Self {
        Self::Refused(reason.into())
    }
}

impl std::fmt::Display for SessionExportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(reason) => write!(formatter, "{reason}"),
            Self::Failed(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for SessionExportError {}

impl From<anyhow::Error> for SessionExportError {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error)
    }
}

/// The work a session did in one repository, as a unified diff.
///
/// The comparison is the session's base commit against the working tree,
/// tracked and untracked alike, so a caller sees what the agent produced
/// whether or not it committed. The base is resolved in the order the caller
/// can trust: an explicit base, then the immutable base a managed network
/// workspace records, then the commit the session branch was created at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDiff {
    pub diff: String,
    pub base: String,
    pub head: String,
    /// Whether the base is an ancestor of HEAD. `false` means the session
    /// rewrote history the base was on, so the diff carries changes the
    /// session did not make. `None` is an older worker whose JSON predates
    /// this field.
    #[serde(default)]
    pub head_descends_from_base: Option<bool>,
}

pub fn session_diff(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    base: Option<&str>,
    branch: Option<&str>,
) -> Result<String, SessionExportError> {
    Ok(session_diff_details(runner, repository, base, branch)?.diff)
}

pub fn session_diff_details(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    base: Option<&str>,
    branch: Option<&str>,
) -> Result<SessionDiff, SessionExportError> {
    let base = match base.map(str::trim) {
        Some("") => return Err(SessionExportError::refused("diff base must not be empty")),
        Some(base) => base.to_owned(),
        None => {
            let recorded = match branch {
                Some(branch) => match remote_workspace_base(runner, repository)? {
                    Some(base) => Some(base),
                    None => branch_creation_commit(runner, repository, branch)?,
                },
                None => remote_workspace_base(runner, repository)?,
            };
            recorded.ok_or_else(|| SessionExportError::refused("no session base recorded"))?
        }
    };
    let resolved = run_git(
        runner,
        repository,
        [
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{base}^{{commit}}"),
        ],
        &[],
    )?;
    if resolved.status != 0 {
        return Err(SessionExportError::refused(format!(
            "diff base {base:?} does not resolve to a commit"
        )));
    }
    let base = trim_output(&resolved.stdout, "resolve the session diff base")?;
    let head = git_text(
        runner,
        repository,
        ["rev-parse", "--verify", "HEAD^{commit}"],
    )
    .context("resolve the session diff head")?;
    // A base that HEAD no longer descends from means the session rewrote the
    // history the base sits on, so the diff reverts commits the session never
    // touched. Status 1 is Git's plain "not an ancestor", not a failure.
    let ancestry = run_git(
        runner,
        repository,
        ["merge-base", "--is-ancestor", &base, &head],
        &[],
    )?;
    let head_descends_from_base = match ancestry.status {
        0 => Some(true),
        1 => Some(false),
        _ => {
            return Err(SessionExportError::Failed(git_failure(
                "compare the session base with HEAD",
                &ancestry,
            )));
        }
    };
    let base_tree = git_text(
        runner,
        repository,
        ["rev-parse", &format!("{base}^{{tree}}")],
    )
    .context("resolve the session base tree")?;
    let current = capture_worktree_tree(runner, repository)?;
    let diff = diff_between_trees(runner, repository, Some(&base_tree), &current)?;
    Ok(SessionDiff {
        diff,
        base,
        head,
        head_descends_from_base,
    })
}

/// The commit a branch was created at, read from its reflog.
///
/// The oldest reflog entry is the branch's creation, so its commit is where the
/// session started. Reflogs expire, which is why a recorded base is preferred;
/// this is the fallback for sessions created before one was recorded.
fn branch_creation_commit(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    branch: &str,
) -> Result<Option<String>> {
    let reference = format!("refs/heads/{branch}");
    let output = run_git(
        runner,
        repository,
        ["reflog", "show", "--format=%H", &reference],
        &[],
    )?;
    if output.status != 0 {
        return Ok(None);
    }
    let text = std::str::from_utf8(&output.stdout).context("decode the branch reflog")?;
    Ok(text
        .lines()
        .rfind(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_owned()))
}

/// A branch this push created or advanced, and the remote it reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushedBranch {
    pub remote: String,
    pub branch: String,
}

/// Why a session branch could not be pushed.
///
/// A missing remote is separated from a failed push because it is the one
/// outcome the caller can fix without looking at Git's output: nothing was
/// attempted, and configuring a remote makes the same call work.
#[derive(Debug)]
pub enum PushBranchError {
    /// The repository names neither `remote.pushDefault` nor `origin`.
    NoRemote,
    /// Git would not accept the name as a branch.
    InvalidBranch(String),
    Failed(anyhow::Error),
}

impl std::fmt::Display for PushBranchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRemote => write!(formatter, "no push remote configured"),
            Self::InvalidBranch(branch) => {
                write!(formatter, "{branch:?} is not a valid branch name")
            }
            Self::Failed(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for PushBranchError {}

impl From<anyhow::Error> for PushBranchError {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error)
    }
}

/// Push the session's current HEAD to `branch` on the repository's push remote.
///
/// The refspec names HEAD rather than the session branch so this works the same
/// way on a detached checkout, and the destination is spelled in full so a
/// remote's push rules cannot redirect it somewhere else.
pub fn push_branch(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    branch: &str,
) -> Result<PushedBranch, PushBranchError> {
    if branch.starts_with('-') {
        return Err(PushBranchError::InvalidBranch(branch.to_owned()));
    }
    let checked = run_git(
        runner,
        repository,
        ["check-ref-format", "--branch", branch],
        &[],
    )?;
    if checked.status != 0 {
        return Err(PushBranchError::InvalidBranch(branch.to_owned()));
    }
    let remote = push_remote(runner, repository)?;
    let refspec = format!("HEAD:refs/heads/{branch}");
    let pushed = run_git(runner, repository, ["push", &remote, &refspec], &[])?;
    if pushed.status != 0 {
        return Err(PushBranchError::Failed(git_failure(
            "push the session branch",
            &pushed,
        )));
    }
    Ok(PushedBranch {
        remote,
        branch: branch.to_owned(),
    })
}

/// The remote a push goes to: the configured default, else `origin` when the
/// repository has one.
fn push_remote(
    runner: &dyn GitCommandRunner,
    repository: &Path,
) -> Result<String, PushBranchError> {
    let configured = run_git(
        runner,
        repository,
        ["config", "--get", "remote.pushDefault"],
        &[],
    )?;
    if configured.status == 0 {
        let name = trim_output(&configured.stdout, "read the configured push remote")?;
        if !name.is_empty() {
            return Ok(name);
        }
    }
    let remotes =
        git_text(runner, repository, ["remote"]).context("list the repository remotes")?;
    if remotes.lines().any(|remote| remote.trim() == "origin") {
        return Ok("origin".to_owned());
    }
    Err(PushBranchError::NoRemote)
}

#[cfg(test)]
mod review_record_tests {
    use super::*;

    #[test]
    fn reviewed_tree_ref_points_to_the_reviewed_commit() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git_bytes(
            &SystemGit,
            root,
            ["init", "-q"],
            &[],
            "initialize repository",
        )
        .unwrap();
        let tree = empty_tree_id(&SystemGit, root).unwrap();
        let commit = SystemGit
            .run(
                root,
                &GitCommand {
                    arguments: vec![
                        "commit-tree".into(),
                        tree.clone().into(),
                        "-m".into(),
                        "reviewed".into(),
                    ],
                    stdin: Vec::new(),
                    env: [
                        "GIT_AUTHOR_NAME",
                        "GIT_AUTHOR_EMAIL",
                        "GIT_COMMITTER_NAME",
                        "GIT_COMMITTER_EMAIL",
                    ]
                    .into_iter()
                    .map(|key| (key.into(), "review@example.test".into()))
                    .collect(),
                },
            )
            .unwrap();
        assert_eq!(commit.status, 0);
        let commit = String::from_utf8(commit.stdout).unwrap().trim().to_owned();
        record_reviewed_tree(&SystemGit, root, &tree, &commit).unwrap();
        let recorded = git_text(
            &SystemGit,
            root,
            ["rev-parse", "--verify", &format!("refs/mj/reviewed/{tree}")],
        )
        .unwrap();
        assert_eq!(recorded, commit);
    }
}
