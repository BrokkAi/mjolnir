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
        let output = mj_core::subprocess::run_with_input(&mut process, &command.stdin)
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

    // A session can work in a subdirectory of its checkout, but its work is
    // the whole checkout's, and a managed checkout is deleted on Stop. Every
    // command runs from the top level, so work outside the subdirectory is
    // kept and every path is named from where a restore starts.
    let checkout = locate_checkout(runner, repository)?;
    let repository = checkout.top_level.as_path();
    let identity = collect_git_identity(runner, repository, spec.origin_override.as_deref())?;
    let clone_state = match &spec.history {
        GitHistoryMode::CloneFrom(base) => Some(collect_clone_refs(runner, repository, base)?),
        _ => None,
    };
    let history = match &clone_state {
        Some(state) => state.history.clone(),
        None => select_git_history(runner, repository, &spec.history, &identity.head_commit)?,
    };
    let result = collect_git_contents(
        runner,
        &checkout,
        spec,
        identity,
        history,
        include_untracked,
        progress,
    );
    if let Some(state) = clone_state {
        let cleanup = remove_temporary_stash_refs(runner, repository, &state.temporary_refs);
        let mut snapshot = result?;
        cleanup?;
        snapshot.metadata.saved_refs = state.saved_refs;
        snapshot.metadata.stash_stack = state.stash_stack;
        Ok(snapshot)
    } else {
        result
    }
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
            saved_refs: Default::default(),
            stash_stack: Vec::new(),
            id: spec.id.clone(),
            relative_destination: spec.relative_destination.clone(),
            checkout_subdirectory: None,
            origin: identity.origin,
            push_urls: identity.push_urls,
            remote_workspace: false,
            base_commit: identity.head_commit.clone(),
            head_commit: identity.head_commit,
            branch: identity.branch,
        },
        committed_bundle: Vec::new(),
        staged_patch: Vec::new(),
        unstaged_patch: Vec::new(),
        untracked_tar: Vec::new(),
    })
}

/// A Git checkout, found from a directory somewhere inside it.
struct CheckoutLocation {
    top_level: PathBuf,
    /// The directory's path below `top_level`; `None` at the top level.
    subdirectory: Option<PathBuf>,
}

fn locate_checkout(runner: &dyn GitCommandRunner, directory: &Path) -> Result<CheckoutLocation> {
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

/// Ref pinning the review point a completed review advanced its baseline to,
/// for a repository's main worktree; a linked worktree has its own (see
/// [`review_baseline_ref`]).
///
/// This is the one kind of ref Mjolnir writes into a user's repository, and it
/// earns its place. A baseline is HEAD's tree plus one blob for each file the
/// checkout was already dirty with; HEAD's own objects are reachable from the
/// branch, but those few blobs are reachable from nothing else. A session can
/// run for days between reviews, so without this ref an ordinary `git gc`
/// would eventually collect them, `tree_exists` would then find no usable
/// baseline, and that repository's next review would report no coverage
/// instead of the turn's work.
///
/// The ref names a review point (see [`record_review_point`]); one pinned by an
/// earlier release names the bare tree. Either resolves with `^{tree}`.
///
/// A capture taken during a review is deliberately not pinned. It is consumed
/// inside that review, and if the review completes it is pinned here as the
/// new baseline.
pub const REVIEW_BASELINE_REF: &str = "refs/hel/review-baseline";

/// Where linked worktrees pin their review baselines, as
/// `refs/hel/worktrees/<name>/review-baseline`.
const WORKTREE_REVIEW_PINS: &str = "refs/hel/worktrees/";

/// The ref that pins the review baseline of the worktree `repository` is.
///
/// Every linked worktree shares its repository's refs, so a single name would
/// give every session on a repository one pin: a new session would take
/// another's baseline as its own, and each completed review would unpin the
/// others'. The main worktree keeps [`REVIEW_BASELINE_REF`]; a linked worktree
/// pins under its name in the repository's `worktrees` directory, which Git
/// keeps a valid ref component. The pins stay in the shared namespace because
/// `git gc` keeps only the objects that the worktree it runs in reaches through
/// `refs/worktree/`; a pin there would protect nothing from a gc run elsewhere.
pub fn review_baseline_ref(runner: &dyn GitCommandRunner, repository: &Path) -> Result<String> {
    let (git_dir, common_dir) = git_directories(runner, repository)?;
    if same_directory(&git_dir, &common_dir)? {
        return Ok(REVIEW_BASELINE_REF.to_owned());
    }
    let name = git_dir
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("name the worktree of {}", git_dir.display()))?;
    Ok(format!("{WORKTREE_REVIEW_PINS}{name}/review-baseline"))
}

/// Deletes the review pins of linked worktrees the repository no longer has,
/// and returns the refs it deleted.
///
/// A pin outlives its worktree, so without this every session in a managed
/// worktree would leave a ref behind in the user's repository. The pins are
/// listed before the worktrees, and each is deleted only while it still holds
/// the listed value. A worktree exists before anything can pin for it, so a
/// pin that was listed belongs to a worktree that the later listing still
/// shows unless it was removed; a pin rewritten in between is left alone.
pub fn remove_departed_worktree_pins(
    runner: &dyn GitCommandRunner,
    repository: &Path,
) -> Result<Vec<String>> {
    let pins = git_text(
        runner,
        repository,
        [
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            WORKTREE_REVIEW_PINS,
        ],
    )
    .context("list the worktree review pins")?;
    if pins.is_empty() {
        return Ok(Vec::new());
    }
    let (_, common_dir) = git_directories(runner, repository)?;
    let worktrees = match std::fs::read_dir(common_dir.join("worktrees")) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<BTreeSet<_>>>()
            .context("list the repository's worktrees")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
        Err(error) => return Err(error).context("list the repository's worktrees"),
    };
    let mut removed = Vec::new();
    for line in pins.lines() {
        let Some((object, reference)) = line.split_once(' ') else {
            continue;
        };
        let Some(name) = reference
            .strip_prefix(WORKTREE_REVIEW_PINS)
            .and_then(|rest| rest.strip_suffix("/review-baseline"))
        else {
            continue;
        };
        if worktrees.contains(std::ffi::OsStr::new(name)) {
            continue;
        }
        git_bytes(
            runner,
            repository,
            ["update-ref", "-d", reference, object],
            &[],
            "delete a departed worktree's review pin",
        )?;
        removed.push(reference.to_owned());
    }
    Ok(removed)
}

/// The worktree's own Git directory and the repository's common one, which
/// are the same directory in the main worktree.
fn git_directories(runner: &dyn GitCommandRunner, repository: &Path) -> Result<(PathBuf, PathBuf)> {
    let output = git_text(
        runner,
        repository,
        ["rev-parse", "--absolute-git-dir", "--git-common-dir"],
    )
    .context("locate the Git directories")?;
    let mut lines = output.lines();
    let (Some(git_dir), Some(common_dir)) = (lines.next(), lines.next()) else {
        bail!("git rev-parse named no common directory: {output}");
    };
    // `--git-common-dir` may be relative to the directory Git ran in.
    Ok((PathBuf::from(git_dir), repository.join(common_dir)))
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
    /// as a checkout, reset, fast-forward, clone or the start and end of a
    /// rebase do, rather than recording a commit made in this worktree.
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
            || ["(start)", "(finish)", "(abort)"]
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
    let real_index = git_text(
        runner,
        repository,
        ["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )
    .context("locate the real Git index")?;
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

struct CloneRefState {
    history: GitHistorySelection,
    saved_refs: std::collections::BTreeMap<String, String>,
    stash_stack: Vec<mj_core::archive::SavedStashEntry>,
    temporary_refs: Vec<String>,
}

fn collect_clone_refs(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    base: &str,
) -> Result<CloneRefState> {
    let unmerged = git_bytes(
        runner,
        repository,
        ["ls-files", "--unmerged", "-z"],
        &[],
        "inspect clone merge conflicts",
    )?;
    ensure!(
        unmerged.is_empty(),
        "cannot checkpoint a clone with unresolved Git conflicts"
    );
    for operation in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "REBASE_HEAD",
        "BISECT_START",
    ] {
        let present = run_git(
            runner,
            repository,
            ["rev-parse", "--verify", "--quiet", operation],
            &[],
        )?;
        ensure!(
            matches!(present.status, 0 | 1),
            "{}",
            git_failure("inspect Git operation state", &present)
        );
        ensure!(
            present.status != 0,
            "cannot checkpoint a clone during Git operation {operation}"
        );
    }
    git_text(
        runner,
        repository,
        ["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )
    .context("managed clone source commit is unavailable")?;
    let lines = git_text(
        runner,
        repository,
        [
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/tags",
            "refs/notes",
        ],
    )?;
    let mut saved_refs = std::collections::BTreeMap::new();
    for line in lines.lines() {
        let (name, oid) = line.split_once(' ').context("malformed saved Git ref")?;
        ensure!(!oid.is_empty(), "saved Git ref {name} has no object");
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
            let name = format!("refs/mj/checkpoint/{capture_id}/stash/{index:08}");
            git_bytes(
                runner,
                repository,
                ["update-ref", &name, &entry.commit],
                &[],
                "preserve stash entry for checkpoint bundle",
            )?;
            temporary_refs.push(name);
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
        let annotated_tag = saved_refs
            .iter()
            .filter(|(name, _)| name.starts_with("refs/tags/"))
            .map(|(_, oid)| git_text(runner, repository, ["cat-file", "-t", oid]))
            .collect::<Result<Vec<_>>>()?
            .iter()
            .any(|kind| kind == "tag");
        let bundle_arguments = (count > 0 || annotated_tag).then(|| {
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
            temporary_refs,
        }),
        Err(error) => {
            remove_temporary_stash_refs(runner, repository, &temporary_refs)?;
            Err(error)
        }
    }
}

fn remove_temporary_stash_refs(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    names: &[String],
) -> Result<()> {
    for name in names {
        git_bytes(
            runner,
            repository,
            ["update-ref", "-d", name],
            &[],
            "remove temporary stash checkpoint ref",
        )?;
    }
    Ok(())
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

fn collect_git_contents(
    runner: &dyn GitCommandRunner,
    checkout: &CheckoutLocation,
    spec: &GitCollectionSpec,
    identity: CollectedGitIdentity,
    history: GitHistorySelection,
    include_untracked: bool,
    progress: &(dyn Fn(GitSnapshotProgress) -> Result<()> + Sync),
) -> Result<RepositorySnapshot> {
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
            saved_refs: Default::default(),
            stash_stack: Vec::new(),
            id: spec.id.clone(),
            relative_destination: spec.relative_destination.clone(),
            checkout_subdirectory: checkout.subdirectory.clone(),
            origin: identity.origin,
            push_urls: identity.push_urls,
            remote_workspace: false,
            base_commit,
            head_commit: identity.head_commit,
            branch: identity.branch,
        },
        committed_bundle,
        staged_patch,
        unstaged_patch,
        untracked_tar,
    })
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
    if snapshot.metadata.remote_workspace {
        mj_core::remote_git::validate_network_url(&snapshot.metadata.origin)?;
        for url in &snapshot.metadata.push_urls {
            mj_core::remote_git::validate_network_url(url)?;
        }
    }
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
        let clone_refs =
            !snapshot.metadata.saved_refs.is_empty() || !snapshot.metadata.stash_stack.is_empty();
        git_success(
            runner,
            repository,
            GitCommand {
                arguments: if clone_refs {
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
    let clone_refs =
        !snapshot.metadata.saved_refs.is_empty() || !snapshot.metadata.stash_stack.is_empty();
    let checkout_target = if snapshot.committed_bundle.is_empty() || clone_refs {
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
    // Check out before moving saved refs: changing the currently checked-out
    // branch first leaves the old index and worktree staged against its new tip.
    if clone_refs {
        for (name, oid) in &snapshot.metadata.saved_refs {
            ensure!(
                name.starts_with("refs/heads/")
                    || name.starts_with("refs/tags/")
                    || name.starts_with("refs/notes/"),
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
            git_bytes(
                runner,
                repository,
                ["update-ref", name, oid],
                &[],
                "restore saved Git ref",
            )?;
        }
        for entry in snapshot.metadata.stash_stack.iter().rev() {
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
    restore_untracked_tar(repository, &snapshot.untracked_tar)
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
    for (key, value) in [
        ("mj.remoteWorkspace", "true"),
        ("mj.baseCommit", metadata.base_commit.as_str()),
        ("push.default", "current"),
        ("push.autoSetupRemote", "true"),
        ("remote.pushDefault", "origin"),
        ("remote.origin.mirror", "false"),
    ] {
        git_success(
            runner,
            repository,
            GitCommand {
                arguments: vec![
                    "config".into(),
                    "--local".into(),
                    "--replace-all".into(),
                    key.into(),
                    value.into(),
                ],
                stdin: Vec::new(),
                env: Vec::new(),
            },
            "restore managed Git configuration",
        )?;
    }
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
            let relative = mj_core::path_input::from_git_bytes(raw_path)?;
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
