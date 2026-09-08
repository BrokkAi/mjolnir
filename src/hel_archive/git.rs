use super::*;

/// A command boundary that lets Git collection and restore be tested without
/// invoking the host's Git executable.
pub trait GitCommandRunner: Sync {
    fn run(&self, repository: &Path, command: &GitCommand) -> Result<GitOutput>;
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

/// SSH asks about an unknown host key itself, which `GIT_TERMINAL_PROMPT` does
/// not reach. An operator who set `GIT_SSH_COMMAND` already chose how to reach
/// the host and keeps their command; otherwise Git gets a batch-mode one.
pub const NON_INTERACTIVE_GIT_SSH_COMMAND: &str =
    "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15";

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemGit;

impl GitCommandRunner for SystemGit {
    fn run(&self, repository: &Path, command: &GitCommand) -> Result<GitOutput> {
        let mut process = Command::new("git");
        process.args(&command.arguments).current_dir(repository);
        for (name, value) in NON_INTERACTIVE_GIT_ENV {
            process.env(name, value);
        }
        for (name, value) in &command.env {
            process.env(name, value);
        }
        if std::env::var_os("GIT_SSH_COMMAND").is_none() {
            process.env("GIT_SSH_COMMAND", NON_INTERACTIVE_GIT_SSH_COMMAND);
        }
        let output = crate::hel_subprocess::run_with_input(&mut process, &command.stdin)
            .with_context(|| format!("run git in {}", repository.display()))?;
        Ok(GitOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// How much committed history a snapshot has to carry. Committed work that is
/// already reachable from an origin ref is durable at its source, so it is
/// never bundled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHistoryMode {
    /// Bundle commits reachable from HEAD but from no `refs/remotes/origin/*`
    /// ref. Errors when the repository has no origin refs at all.
    SessionDelta,
    /// Exclude only history supplied by this named source remote.
    SessionDeltaFromRemote(String),
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

pub fn collect_git_snapshot(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    spec: &GitCollectionSpec,
) -> Result<RepositorySnapshot> {
    collect_git_snapshot_with_progress(runner, repository, spec, true, &|_| Ok(()))
}

pub fn collect_git_snapshot_with_progress(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    spec: &GitCollectionSpec,
    include_untracked: bool,
    progress: &(dyn Fn(GitSnapshotProgress) -> Result<()> + Sync),
) -> Result<RepositorySnapshot> {
    validate_component(&spec.id, "repository id")?;
    validate_archive_relative_path(&spec.relative_destination)?;

    let identity = collect_git_identity(runner, repository, spec.origin_override.as_deref())?;
    let history = select_git_history(runner, repository, &spec.history, &identity.head_commit)?;
    collect_git_contents(
        runner,
        repository,
        spec,
        identity,
        history,
        include_untracked,
        progress,
    )
}

/// Collect only enough Git identity to associate native harness state with an
/// existing project. The project itself remains the recovery source.
pub fn collect_git_metadata_snapshot(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    spec: &GitCollectionSpec,
) -> Result<RepositorySnapshot> {
    validate_component(&spec.id, "repository id")?;
    validate_archive_relative_path(&spec.relative_destination)?;
    let identity = collect_git_identity(runner, repository, spec.origin_override.as_deref())?;
    Ok(RepositorySnapshot {
        metadata: RepositoryMetadata {
            id: spec.id.clone(),
            relative_destination: spec.relative_destination.clone(),
            origin: identity.origin,
            base_commit: identity.head_commit.clone(),
            head_commit: identity.head_commit,
            branch: identity.branch,
            session_branch: identity.session_branch,
        },
        committed_bundle: Vec::new(),
        staged_patch: Vec::new(),
        unstaged_patch: Vec::new(),
        untracked_tar: Vec::new(),
    })
}

struct CollectedGitIdentity {
    origin: String,
    head_commit: String,
    branch: Option<String>,
    session_branch: Option<String>,
}

fn collect_git_identity(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    origin_override: Option<&str>,
) -> Result<CollectedGitIdentity> {
    let head_commit = git_text(runner, repository, ["rev-parse", "--verify", "HEAD"])
        .context("repository has no valid Git HEAD")?;
    let origin = if let Some(origin) = origin_override {
        origin.to_owned()
    } else {
        let output = run_git(runner, repository, ["remote", "get-url", "origin"], &[])?;
        if output.status == 0 {
            redact_origin_credentials(&trim_output(&output.stdout, "read Git origin")?)?
        } else {
            String::new()
        }
    };
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
    let policy = run_git(
        runner,
        repository,
        ["config", "--get", "mj.sessionBranch"],
        &[],
    )?;
    let session_branch = match policy.status {
        0 => Some(trim_output(&policy.stdout, "read session branch policy")?),
        1 => None,
        _ => return Err(git_failure("read session branch policy", &policy)),
    };
    Ok(CollectedGitIdentity {
        origin,
        head_commit,
        branch,
        session_branch,
    })
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
    has_remote_refs(runner, repository, "origin")
}

pub fn has_remote_refs(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    remote: &str,
) -> Result<bool> {
    crate::hel_config::validate_id("Git baseline remote", remote)?;
    let refs = git_text(
        runner,
        repository,
        [
            "for-each-ref",
            "--format=%(objectname)",
            &format!("refs/remotes/{remote}/"),
        ],
    )
    .context("list remote refs")?;
    Ok(!refs.is_empty())
}

/// Ref that keeps the objects of the most recent review capture reachable, so
/// a `git gc` between two reviews cannot collect the tree a baseline names.
pub const REVIEW_CAPTURE_REF: &str = "refs/hel/review-capture";
/// Ref pointing at the tree a completed review advanced its baseline to.
pub const REVIEW_BASELINE_REF: &str = "refs/hel/review-baseline";

/// Records the working tree, tracked and untracked alike, as a Git tree object
/// and returns its id.
///
/// The staging runs against a scratch index file named by `GIT_INDEX_FILE`, so
/// the repository's real index, working tree and HEAD are untouched: after this
/// call `git status` reports exactly what it reported before. Ignored files stay
/// out, because `git add -A` honours the ignore rules, which is what makes two
/// captures comparable as "what the agent changed".
pub fn capture_worktree_tree(runner: &dyn GitCommandRunner, repository: &Path) -> Result<String> {
    let git_dir = git_text(runner, repository, ["rev-parse", "--absolute-git-dir"])
        .context("locate the Git directory for a review capture")?;
    let index = tempfile::Builder::new()
        .prefix("hel-review-index-")
        .tempfile_in(&git_dir)
        .with_context(|| format!("create a scratch Git index in {git_dir}"))?;
    // Git wants to create the index itself; an existing empty file is read as
    // a corrupt index.
    let index_path = index.into_temp_path();
    std::fs::remove_file(&index_path).ok();
    let scratch = [(
        OsString::from("GIT_INDEX_FILE"),
        index_path.as_os_str().to_os_string(),
    )];
    let staged = git_success(
        runner,
        repository,
        GitCommand {
            arguments: ["add", "-A", "--", "."]
                .into_iter()
                .map(OsString::from)
                .collect(),
            stdin: Vec::new(),
            env: scratch.to_vec(),
        },
        "stage the working tree into a scratch index",
    );
    let tree = staged.and_then(|_| {
        let output = runner.run(
            repository,
            &GitCommand {
                arguments: vec![OsString::from("write-tree")],
                stdin: Vec::new(),
                env: scratch.to_vec(),
            },
        )?;
        ensure!(
            output.status == 0,
            "{}",
            git_failure("write the review capture tree", &output)
        );
        trim_output(&output.stdout, "decode the review capture tree id")
    });
    // The scratch index is this capture's alone; leaving it behind would grow
    // the git dir by one file per review.
    let _ = std::fs::remove_file(&index_path);
    let tree = tree?;
    ensure!(!tree.is_empty(), "git write-tree produced no tree id");
    pin_review_tree(runner, repository, REVIEW_CAPTURE_REF, &tree)?;
    Ok(tree)
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
/// wants the first time it runs in a repository.
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
        ["diff", "--binary", "--no-ext-diff", &base, current],
        &[],
        "diff the captured review trees",
    )?;
    Ok(String::from_utf8_lossy(&patch).into_owned())
}

struct GitHistorySelection {
    /// Informational only; an empty string for session deltas.
    base_commit: String,
    /// Arguments for the bundle command, or None when nothing has to be sent.
    bundle_arguments: Option<Vec<String>>,
}

fn select_git_history(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    mode: &GitHistoryMode,
    head_commit: &str,
) -> Result<GitHistorySelection> {
    match mode {
        GitHistoryMode::SessionDelta | GitHistoryMode::SessionDeltaFromRemote(_) => {
            let remote = match mode {
                GitHistoryMode::SessionDeltaFromRemote(remote) => remote.as_str(),
                _ => "origin",
            };
            let excluded = format!("--remotes={remote}");
            // Collection stays side-effect free; callers repair missing origin
            // refs before asking for a session delta.
            ensure!(
                has_remote_refs(runner, repository, remote)?,
                "repository has no {remote} refs to delta against"
            );
            let count = git_text(
                runner,
                repository,
                ["rev-list", "--count", "HEAD", "--not", &excluded],
            )?
            .parse::<u64>()
            .context("parse committed delta count")?;
            Ok(GitHistorySelection {
                base_commit: String::new(),
                bundle_arguments: (count > 0).then(|| {
                    ["bundle", "create", "-", "HEAD", "--not", &excluded]
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

fn collect_git_contents(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    spec: &GitCollectionSpec,
    identity: CollectedGitIdentity,
    history: GitHistorySelection,
    include_untracked: bool,
    progress: &(dyn Fn(GitSnapshotProgress) -> Result<()> + Sync),
) -> Result<RepositorySnapshot> {
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
                    Some(arguments) => git_bytes_owned(
                        runner,
                        repository,
                        arguments,
                        "create committed delta bundle",
                    ),
                    None => Ok(Vec::new()),
                },
                || {
                    git_bytes(
                        runner,
                        repository,
                        ["diff", "--binary", "--cached", "--no-ext-diff"],
                        &[],
                        "collect staged Git patch",
                    )
                },
            )
        },
        || {
            rayon::join(
                || {
                    git_bytes(
                        runner,
                        repository,
                        ["diff", "--binary", "--no-ext-diff"],
                        &[],
                        "collect unstaged Git patch",
                    )
                },
                || {
                    if !include_untracked {
                        return Ok(Vec::new());
                    }
                    let untracked = git_bytes(
                        runner,
                        repository,
                        ["ls-files", "--others", "--exclude-standard", "-z"],
                        &[],
                        "list nonignored untracked files",
                    )?;
                    build_untracked_tar(repository, &untracked, progress)
                },
            )
        },
    );
    let committed_bundle = committed_bundle?;
    let staged_patch = staged_patch?;
    let unstaged_patch = unstaged_patch?;
    let untracked_tar = untracked_tar?;

    Ok(RepositorySnapshot {
        metadata: RepositoryMetadata {
            id: spec.id.clone(),
            relative_destination: spec.relative_destination.clone(),
            origin: identity.origin,
            base_commit,
            head_commit: identity.head_commit,
            branch: identity.branch,
            session_branch: identity.session_branch,
        },
        committed_bundle,
        staged_patch,
        unstaged_patch,
        untracked_tar,
    })
}

pub fn restore_git_snapshot(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    snapshot: &RepositorySnapshot,
) -> Result<()> {
    if let Some(branch) = &snapshot.metadata.branch {
        ensure_branch_available_for_checkout(runner, repository, branch)?;
    }
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
                arguments: vec![OsString::from("fetch"), bundle_path, OsString::from("HEAD")],
                stdin: Vec::new(),
                env: Vec::new(),
            },
            "fetch committed delta bundle",
        )?;
    }
    let checkout_target = if snapshot.committed_bundle.is_empty() {
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
        git_bytes(
            runner,
            repository,
            ["checkout", "-B", branch, checkout_target],
            &[],
            "restore committed branch",
        )
        .with_context(|| checkout_advice(snapshot))?;
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
    if let Some(branch) = &snapshot.metadata.session_branch {
        git_bytes(
            runner,
            repository,
            ["config", "mj.sessionBranch", branch],
            &[],
            "restore session branch policy",
        )?;
    } else {
        let cleared = run_git(
            runner,
            repository,
            ["config", "--unset-all", "mj.sessionBranch"],
            &[],
        )?;
        ensure!(
            matches!(cleared.status, 0 | 5),
            "clear session branch policy failed"
        );
    }
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
    restore_untracked_tar(repository, &snapshot.untracked_tar)
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

    let worktrees = run_git(
        runner,
        repository,
        ["worktree", "list", "--porcelain", "-z"],
        &[],
    )?;
    ensure!(
        worktrees.status == 0,
        "{}",
        git_failure("list restore worktrees", &worktrees)
    );
    let branch_field = format!("branch refs/heads/{branch}");
    ensure!(
        !worktrees
            .stdout
            .split(|byte| *byte == 0)
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

fn run_git<const N: usize>(
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

fn git_bytes<const N: usize>(
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

fn git_text<const N: usize>(
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

fn git_failure(action: &str, output: &GitOutput) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow!(
        "{action} failed with status {}: {}",
        output.status,
        stderr.trim()
    )
}

pub(super) fn build_untracked_tar(
    repository: &Path,
    nul_paths: &[u8],
    progress: &(dyn Fn(GitSnapshotProgress) -> Result<()> + Sync),
) -> Result<Vec<u8>> {
    let paths = nul_paths
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    let total = paths.len();
    let mut output = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut output);
        for (index, raw_path) in paths.into_iter().enumerate() {
            let relative = path_from_git_bytes(raw_path)?;
            validate_archive_relative_path(&relative)?;
            // In addition to Git's ignore rules, skip conventional credential
            // paths so they can never enter the untracked payload.
            if ensure_not_secret_path(&relative).is_err() {
                continue;
            }
            progress(GitSnapshotProgress::UntrackedFile {
                current: index + 1,
                total,
                path: relative.clone(),
            })?;
            let source = repository.join(&relative);
            ensure_no_symlink_ancestors(repository, &relative)?;
            let metadata = fs::symlink_metadata(&source)
                .with_context(|| format!("stat untracked path {}", source.display()))?;
            if metadata.file_type().is_symlink() {
                let target = fs::read_link(&source)
                    .with_context(|| format!("read symlink {}", source.display()))?;
                validate_symlink_target(&relative, &target)?;
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                header.set_mode(0o777);
                header.set_link_name(&target)?;
                header.set_cksum();
                builder.append_data(&mut header, &relative, std::io::empty())?;
            } else if metadata.is_file() {
                let mut header = tar::Header::new_gnu();
                header.set_metadata(&metadata);
                header.set_cksum();
                let mut file = File::open(&source)
                    .with_context(|| format!("open untracked file {}", source.display()))?;
                builder.append_data(&mut header, &relative, &mut file)?;
            } else {
                bail!("unsupported untracked file type at {}", source.display());
            }
        }
        builder.finish().context("finish untracked-file tar")?;
    }
    Ok(output)
}

fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        Ok(PathBuf::from(
            std::str::from_utf8(bytes).context("Git path is not UTF-8")?,
        ))
    }
}

pub(super) fn validate_untracked_tar(bytes: &[u8]) -> Result<()> {
    validate_untracked_tar_reader(Cursor::new(bytes))
}

pub(super) fn validate_untracked_tar_reader(reader: impl Read) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().context("read untracked-file tar")? {
        let entry = entry.context("read untracked-file tar entry")?;
        let relative = entry.path().context("read untracked-file tar path")?;
        validate_archive_relative_path(&relative)?;
        ensure_not_secret_path(&relative)?;
        let entry_type = entry.header().entry_type();
        ensure!(
            entry_type.is_file() || entry_type.is_symlink(),
            "untracked tar contains unsupported entry type for '{}'",
            relative.display()
        );
        if entry_type.is_symlink() {
            let target = entry
                .link_name()
                .context("read untracked symlink target")?
                .ok_or_else(|| {
                    anyhow!("untracked symlink '{}' has no target", relative.display())
                })?;
            validate_symlink_target(&relative, &target)?;
        }
    }
    Ok(())
}

pub(super) fn restore_untracked_tar(repository: &Path, bytes: &[u8]) -> Result<()> {
    validate_untracked_tar(bytes)?;
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    for entry in archive.entries().context("read untracked-file tar")? {
        let mut entry = entry.context("read untracked-file tar entry")?;
        let relative = entry.path()?.into_owned();
        ensure_no_symlink_ancestors(repository, &relative)?;
        let destination = repository.join(&relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create directory {}", parent.display()))?;
            ensure_no_symlink_ancestors(repository, &relative)?;
        }
        ensure!(
            !destination.exists(),
            "refusing to overwrite restored untracked path '{}'",
            destination.display()
        );
        if entry.header().entry_type().is_symlink() {
            let target = entry.link_name()?.ok_or_else(|| {
                anyhow!("untracked symlink '{}' has no target", relative.display())
            })?;
            create_symlink(&target, &destination)?;
        } else {
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&destination)
                .with_context(|| format!("create untracked file {}", destination.display()))?;
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

fn validate_symlink_target(link_path: &Path, target: &Path) -> Result<()> {
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
