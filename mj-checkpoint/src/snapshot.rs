//! Origin-relative Git snapshots, packing pinned objects without temporary refs.
//! Filesystem paths belong to the machine running the supplied Git runner.

use crate::archive::git::{
    GitCommand, GitCommandRunner, discover_linked_worktrees, git_bytes, git_failure, git_text,
    run_git, synthetic_git_commit_identity, synthetic_git_identity,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckoutKey {
    pub repository_id: String,
    pub checkout_path: PathBuf,
}
impl Default for CheckoutKey {
    fn default() -> Self {
        Self {
            repository_id: "workspace".into(),
            checkout_path: PathBuf::new(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Scoped<T> {
    pub checkout: CheckoutKey,
    pub value: T,
}
fn scoped<T>(checkout: &CheckoutKey, value: T) -> Scoped<T> {
    Scoped {
        checkout: checkout.clone(),
        value,
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SnapshotHead {
    Branch { name: String, commit: String },
    Detached(String),
}
impl SnapshotHead {
    pub fn commit(&self) -> &str {
        match self {
            Self::Branch { commit, .. } | Self::Detached(commit) => commit,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotStashEntry {
    pub commit: String,
    pub message: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotConfigEntry {
    pub name: String,
    pub value: String,
}
mod relative_path_serde {
    use super::*;
    use serde::{Deserializer, Serializer};
    pub fn serialize<S: Serializer>(
        path: &Path,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer
            .serialize_str(&crate::archive::slash_path(path).map_err(serde::ser::Error::custom)?)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<PathBuf, D::Error> {
        let text = String::deserialize(deserializer)?;
        let path = PathBuf::from(&text);
        let normalized = crate::archive::slash_path(&path).map_err(serde::de::Error::custom)?;
        if normalized != text {
            return Err(serde::de::Error::custom(
                "snapshot path is not slash-separated",
            ));
        }
        Ok(path)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotSubmodule {
    pub origin_urls: Vec<String>,
    pub push_urls: Vec<String>,
    pub object_format: String,
    pub worktree_config_enabled: bool,
    pub url_rules: Vec<SnapshotConfigEntry>,
    pub worktree_url_rules: Vec<SnapshotConfigEntry>,
    #[serde(with = "relative_path_serde")]
    pub path: PathBuf,
    pub commit: String,
    pub name: String,
    pub url: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotGitlink {
    #[serde(with = "relative_path_serde")]
    pub path: PathBuf,
    pub commit: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotWorktree {
    pub push_evidence: PushEvidence,
    pub has_uncommitted_state: bool,
    pub url_rules: Vec<SnapshotConfigEntry>,
    #[serde(with = "relative_path_serde")]
    pub relative_path: PathBuf,
    pub head: SnapshotHead,
    pub working_state: Option<String>,
    pub working_state_skipped: bool,
    pub initialized_submodules: Vec<SnapshotSubmodule>,
    pub index_gitlinks: Vec<SnapshotGitlink>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BranchTracking {
    pub remote: Option<String>,
    pub merge: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PushEvidence {
    Resolved(Vec<String>),
    Unavailable { reason: String },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    /// False when origin_urls contains only a capture/restore source override.
    #[serde(default = "source_had_origin_default")]
    pub source_had_origin: bool,
    pub object_format: String,
    pub worktree_config_enabled: bool,
    pub origin_urls: Vec<String>,
    pub push_urls: Vec<String>,
    pub push_evidence: PushEvidence,
    pub url_rules: Vec<SnapshotConfigEntry>,
    pub worktree_url_rules: Vec<SnapshotConfigEntry>,
    pub notices: Vec<String>,
    pub head: SnapshotHead,
    pub refs: BTreeMap<String, String>,
    pub refs_complete: bool,
    pub deleted_refs: Vec<String>,
    pub branch_tracking: BTreeMap<String, BranchTracking>,
    pub stash: Vec<SnapshotStashEntry>,
    pub working_state: Option<String>,
    pub working_state_skipped: bool,
    pub has_uncommitted_state: bool,
    pub worktrees: Vec<SnapshotWorktree>,
    pub initialized_submodules: Vec<SnapshotSubmodule>,
    pub index_gitlinks: Vec<SnapshotGitlink>,
    pub remote_refs_stale: bool,
    /// Non-thin Git pack; `None` when origin already supplies every object.
    pub pack: Option<PathBuf>,
}
fn source_had_origin_default() -> bool {
    true
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CheckoutOwnership {
    Managed,
    Raw { launch: Option<LaunchRecord> },
}
/// Version 1 is a UTF-8 JSON object followed by a newline. Paths use serde's
/// platform PathBuf representation. Stashes are newest first; refs are full names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchRecord {
    pub version: u32,
    pub refs: BTreeMap<String, String>,
    pub stash: Vec<SnapshotStashEntry>,
    pub worktrees: Vec<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct FileIdentity(String);
/// Use the same metadata observation for this identity and the displayed size.
/// The controller can use this for .mj/keep entries as well as untracked files.
pub fn file_identity(metadata: &fs::Metadata) -> Result<FileIdentity> {
    let mut identity = blake3::Hasher::new();
    hash_file_metadata(&mut identity, metadata)?;
    Ok(FileIdentity(identity.finalize().to_hex().to_string()))
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum UntrackedItem {
    Path {
        checkout: CheckoutKey,
        path: PathBuf,
        identity: FileIdentity,
    },
    NestedRepository {
        checkout: CheckoutKey,
        path: PathBuf,
        identity: FileIdentity,
    },
    Stash {
        checkout: CheckoutKey,
        commit: String,
        message: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UntrackedConsent {
    None,
    Any,
    Shown(Vec<UntrackedItem>),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OperationConsent {
    None,
    Any,
    Shown(Vec<ShownOperation>),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureConsent {
    pub untracked: UntrackedConsent,
    pub operations: OperationConsent,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnrepresentedWorktree {
    pub locked: bool,
    pub can_move_inside: bool,
    pub git_dir: PathBuf,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleLocalWork {
    pub path: PathBuf,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UninitializedOrigin {
    pub owner: PathBuf,
    pub config_key: String,
    pub route: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginDependency {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_url_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_provider: Option<PathBuf>,
    pub route: PathBuf,
    pub canonical_path: PathBuf,
    pub target_is_removed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryBlocker {
    #[serde(flatten)]
    pub kind: SnapshotRefused,
    pub paths: Vec<Scoped<PathBuf>>,
    pub remedy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum SnapshotRefused {
    Untracked(Vec<UntrackedItem>),
    OperationInProgress(ShownOperation),
    DirtySubmodule(Scoped<PathBuf>),
    UnpublishedSubmoduleWork(Vec<Scoped<ModuleLocalWork>>),
    SubmoduleOperations(Vec<Scoped<GitOperationKind>>),
    ShallowRepository(CheckoutKey),
    MissingManagedOrigin(CheckoutKey),
    OriginRemovedByTeardown(Vec<Scoped<OriginDependency>>),
    UnrepresentedGitState {
        directories: Vec<Scoped<PathBuf>>,
        worktrees: Vec<Scoped<UnrepresentedWorktree>>,
    },
    HiddenTrackedEdits(Vec<Scoped<PathBuf>>),
    IntentToAdd(Vec<Scoped<PathBuf>>),
    UntrackedSubmoduleRegistration(Vec<Scoped<PathBuf>>),
    TrackedKeepFiles {
        keep_dir: PathBuf,
        paths: Vec<Scoped<PathBuf>>,
    },
}
impl std::fmt::Display for SnapshotRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingManagedOrigin(checkout) => write!(f,
                "managed checkout {}:{} has no origin remote or source override; restore its origin with `git remote add origin <url>`, or use `mj suspend --without-checkpoint`",
                checkout.repository_id, checkout.checkout_path.display()),
            Self::OriginRemovedByTeardown(origins) => {
                for (index, origin) in origins.iter().enumerate() {
                    if index != 0 { write!(f, "; ")?; }
                    if let Some(provider) = &origin.value.object_provider {
                        let path = mj_core::targets::posix_quote(&origin.value.canonical_path.to_string_lossy());
                        write!(f, "local origin {} relies on provider {} inside the managed teardown; make the origin an independent repository outside that set; to dissociate borrowed objects run `git -C {path} repack -a -d`, then remove its objects/info/alternates, and suspend again, or use `mj suspend --without-checkpoint`", origin.value.route.display(), provider.display())?;
                        continue;
                    }
                    let directory = if origin.checkout.checkout_path.as_os_str().is_empty() { String::new() }
                        else { format!(" -C {}", mj_core::targets::posix_quote(&origin.checkout.checkout_path.to_string_lossy())) };
                    write!(f, "origin route {} for {}:{} depends on the managed teardown; ", origin.value.route.display(), origin.checkout.repository_id, origin.checkout.checkout_path.display())?;
                    let setting = match &origin.value.module_url_key {
                        Some(key) => format!("config -f .gitmodules {}", mj_core::targets::posix_quote(key)),
                        None => "remote set-url origin".to_owned(),
                    };
                    if origin.value.target_is_removed {
                        write!(f, "move the mirror {} outside the clone and run `git{directory} {setting} <new path>`", origin.value.canonical_path.display())?;
                    } else {
                        write!(f, "run `git{directory} {setting} {}`", mj_core::targets::posix_quote(&origin.value.canonical_path.to_string_lossy()))?;
                    }
                    write!(f, ", then suspend again, or use `mj suspend --without-checkpoint`")?;
                }
                Ok(())
            }
            Self::TrackedKeepFiles { keep_dir, paths } => write!(
                f,
                "{} tracked keep files would enter the snapshot; run git rm -r --cached {} and commit any removal from HEAD before suspending",
                paths.len(),
                keep_dir.display()
            ),
            Self::UntrackedSubmoduleRegistration(paths) => write!(f, "{}", paths.iter().map(|entry| {
                let directory = if entry.checkout.checkout_path.as_os_str().is_empty() { String::new() } else { format!(" -C {}", mj_core::targets::posix_quote(&entry.checkout.checkout_path.to_string_lossy())) };
                format!("submodule registration {}:{}:{} is untracked and would not survive discard; run `git{directory} add {}` to preserve it before suspending", entry.checkout.repository_id, entry.checkout.checkout_path.display(), entry.value.display(), mj_core::targets::posix_quote(&entry.value.to_string_lossy()))
            }).collect::<Vec<_>>().join("; ")),
            Self::IntentToAdd(paths) => write!(f, "{}", paths.iter().map(|entry| {
                let directory = if entry.checkout.checkout_path.as_os_str().is_empty() { String::new() }
                    else { format!(" -C {}", mj_core::targets::posix_quote(&entry.checkout.checkout_path.to_string_lossy())) };
                let path = mj_core::targets::posix_quote(&entry.value.to_string_lossy());
                format!("intent-to-add {}:{}:{} cannot be captured; run `git{directory} add {path}` to stage its contents, or `git{directory} reset -- {path}` to leave it untracked",
                    entry.checkout.repository_id, entry.checkout.checkout_path.display(), entry.value.display())
            }).collect::<Vec<_>>().join("; ")),
            Self::HiddenTrackedEdits(paths) => write!(
                f,
                "{} have skip-worktree or assume-unchanged flags; clear those flags before suspending or moving so all tracked edits can be verified",
                paths
                    .iter()
                    .map(|path| format!(
                        "{}:{}:{}",
                        path.checkout.repository_id,
                        path.checkout.checkout_path.display(),
                        path.value.display()
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::UnrepresentedGitState { directories, worktrees } => {
                let mut items = directories.iter().map(|item| format!("unrepresented Git directory {}; push or copy out anything wanted, then delete this directory before suspending", item.value.display())).collect::<Vec<_>>();
                items.extend(worktrees.iter().map(|item| {
                    let repo = mj_core::targets::posix_quote(&item.value.git_dir.to_string_lossy());
                    let path = mj_core::targets::posix_quote(&item.value.path.to_string_lossy());
                    let alternative = if item.value.can_move_inside && !item.value.locked { " or move it inside the clone" } else { "" };
                    {
                    let locked = if item.value.locked { format!("; this worktree is locked: once it is available and its contents are safe, deliberately run `git --git-dir {repo} --work-tree {repo} worktree unlock {path}` first") } else { String::new() };
                    format!("unrepresented worktree {}{locked}; push or copy out anything wanted, then run `git --git-dir {repo} --work-tree {repo} worktree remove --force {path}`{alternative} before suspending", item.value.path.display())
                }
                }));
                write!(f, "{}", items.join("; "))
            }
            Self::ShallowRepository(checkout) => write!(f, "snapshot for {}:{} needs full history; run `git fetch --unshallow`. If origin is itself shallow, run `git fetch --unshallow <url>` from any remote with full history. If none exists, use `mj suspend --without-checkpoint` or keep the session running", checkout.repository_id, checkout.checkout_path.display()),
            Self::SubmoduleOperations(items) => write!(f, "{}", items.iter().map(|item| {
                let [command, argument] = abort_operation_arguments(item.value);
                let path = mj_core::targets::posix_quote(&item.checkout.checkout_path.to_string_lossy());
                format!("{:?} in submodule {}:{} is not captured; finish it, or run `git -C {path} {command} {argument}` before suspending",
                    item.value, item.checkout.repository_id, item.checkout.checkout_path.display())
            }).collect::<Vec<_>>().join("; ")),
            Self::UnpublishedSubmoduleWork(items) => write!(
                f,
                "{} would be lost; push the work, or delete the ref or drop the stash, then suspend again",
                items
                    .iter()
                    .map(|item| format!(
                        "{}:{}:{}",
                        item.checkout.repository_id,
                        item.checkout.checkout_path.display(),
                        item.value.description
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::DirtySubmodule(path) => write!(
                f,
                "submodule {}:{}:{} has tracked changes or an unpublished checkout; commit and push inside the submodule, or discard the change; if uninitialized, run `git submodule update --init {}` or unstage the pointer change; when origin is absent and its .gitmodules URL is relative, initialize the module or change that URL to an absolute one, then suspend again",
                path.checkout.repository_id,
                path.checkout.checkout_path.display(),
                path.value.display(),
                path.value.display()
            ),
            Self::OperationInProgress(operation) => {
                write!(
                    f,
                    "{:?} in progress at {}:{}; abort consent is required",
                    operation.kind,
                    operation.checkout.repository_id,
                    operation.checkout.checkout_path.display()
                )
            }
            Self::Untracked(items) => {
                write!(
                    f,
                    "{} untracked items require discard consent:",
                    items.len()
                )?;
                for item in items {
                    match item {
                        UntrackedItem::Path { checkout, path, .. } => write!(
                            f,
                            " {}:{}:{}",
                            checkout.repository_id,
                            checkout.checkout_path.display(),
                            path.display()
                        )?,
                        UntrackedItem::NestedRepository { checkout, path, .. } => write!(
                            f,
                            " {}:{} nested repository {}",
                            checkout.repository_id,
                            checkout.checkout_path.display(),
                            path.display()
                        )?,
                        UntrackedItem::Stash {
                            checkout,
                            commit,
                            message,
                        } => write!(
                            f,
                            " {}:{} stash {commit} ({message})",
                            checkout.repository_id,
                            checkout.checkout_path.display()
                        )?,
                    }
                }
                Ok(())
            }
        }
    }
}
impl std::error::Error for SnapshotRefused {}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum GitOperationKind {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Am,
    Bisect,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitOperation {
    pub checkout: CheckoutKey,
    pub kind: GitOperationKind,
    pub identity: String,
}
pub type ShownOperation = GitOperation;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UntrackedPath {
    pub checkout: CheckoutKey,
    pub path: PathBuf,
    pub bytes: u64,
    pub identity: FileIdentity,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInventory {
    pub notices: Vec<String>,
    pub checkout: CheckoutKey,
    pub untracked: Vec<UntrackedPath>,
    pub untracked_stashes: Vec<Scoped<SnapshotStashEntry>>,
    pub nested_repositories: Vec<UntrackedPath>,
    pub tracked_keep_files: Vec<Scoped<PathBuf>>,
    pub keep_dir: Option<PathBuf>,
    pub dirty_submodules: Vec<Scoped<PathBuf>>,
    pub untracked_submodule_registration: Vec<Scoped<PathBuf>>,
    pub submodule_local_work: Vec<Scoped<ModuleLocalWork>>,
    pub submodule_operations: Vec<Scoped<GitOperationKind>>,
    pub unrepresented_worktrees: Vec<Scoped<UnrepresentedWorktree>>,
    pub unrepresented_git_dirs: Vec<Scoped<PathBuf>>,
    pub origins_removed_by_teardown: Vec<Scoped<OriginDependency>>,
    #[serde(skip)]
    pub(crate) uninitialized_origins: Vec<UninitializedOrigin>,
    pub shallow: bool,
    pub blockers: Vec<InventoryBlocker>,
    pub in_place_only: bool,
    pub origin_missing: bool,
    pub hidden_tracked_edits: Vec<Scoped<PathBuf>>,
    pub intent_to_add: Vec<Scoped<PathBuf>>,
    pub initialized_submodules: Vec<Scoped<SnapshotSubmodule>>,
    pub index_gitlinks: Vec<Scoped<SnapshotGitlink>>,
    pub operation: Option<GitOperation>,
    pub raw: bool,
    pub remote_refs_stale: bool,
    pub worktrees: Vec<(PathBuf, WorkspaceInventory)>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct OriginQueryKey {
    repository: PathBuf,
    arguments: Vec<OsString>,
    configuration: [u8; 32],
}
type CachedOriginQuery = std::result::Result<crate::archive::GitOutput, String>;
/// Capture owns network evidence for its duration and disables maintenance in
/// every Git command, including lazy promisor fetches through child config.
struct CaptureGit<'a> {
    inner: &'a dyn GitCommandRunner,
    origin_queries: std::sync::Mutex<BTreeMap<OriginQueryKey, CachedOriginQuery>>,
}
impl<'a> CaptureGit<'a> {
    fn new(inner: &'a dyn GitCommandRunner) -> Self {
        Self {
            inner,
            origin_queries: Default::default(),
        }
    }
}
impl GitCommandRunner for CaptureGit<'_> {
    fn run(&self, repository: &Path, command: &GitCommand) -> Result<crate::archive::GitOutput> {
        let mut command = command.clone();
        let mut arguments = vec![
            "-c".into(),
            "gc.auto=0".into(),
            "-c".into(),
            "maintenance.auto=false".into(),
        ];
        arguments.extend(command.arguments);
        command.arguments = arguments;
        // SystemGit's noninteractive SSH default must not hide the source's
        // identity/proxy command. Resolve it through the same runner so target
        // and worktree configuration, including includes, retain precedence.
        if std::env::var_os("GIT_SSH_COMMAND").is_none()
            && !command
                .env
                .iter()
                .any(|(name, _)| name == "GIT_SSH_COMMAND")
        {
            let configured = self.inner.run(
                repository,
                &GitCommand {
                    arguments: [
                        "-c",
                        "gc.auto=0",
                        "-c",
                        "maintenance.auto=false",
                        "config",
                        "--get",
                        "core.sshCommand",
                    ]
                    .map(OsString::from)
                    .to_vec(),
                    env: command.env.clone(),
                    ..Default::default()
                },
            )?;
            ensure!(
                configured.status == 0 || configured.status == 1,
                "{}",
                git_failure("read source SSH command", &configured)
            );
            if configured.status == 0 {
                let value = configured
                    .stdout
                    .strip_suffix(b"\n")
                    .unwrap_or(&configured.stdout);
                command.env.push((
                    "GIT_SSH_COMMAND".into(),
                    mj_core::path_input::from_git_bytes(value)?.into_os_string(),
                ));
            }
        }
        // Snapshot packs must hydrate promised objects they carry. The -c
        // maintenance settings above also reach Git's lazy-fetch children.
        command.env.push(("GIT_NO_LAZY_FETCH".into(), "0".into()));
        // Inspection must not refresh or lock a user's real index.
        command.env.push(("GIT_OPTIONAL_LOCKS".into(), "0".into()));
        let named_origin_query = command
            .arguments
            .windows(2)
            .any(|pair| pair[0] == "ls-remote" && pair[1] == "origin");
        let neutral_probe = command.arguments.iter().any(|arg| arg == "ls-remote")
            && command.env.iter().any(|(name, value)| {
                name == "GIT_CEILING_DIRECTORIES" && value == repository.as_os_str()
            });
        let objects_only_fetch = command.arguments.iter().any(|arg| arg == "fetch")
            && command
                .arguments
                .iter()
                .any(|arg| arg == "--no-write-fetch-head")
            && command.arguments.iter().any(|arg| arg == "--refmap=");
        if !named_origin_query && !neutral_probe && !objects_only_fetch {
            return self.inner.run(repository, &command);
        }
        // Keep one network observation per effective configuration within this
        // capture. Local refs, indexes, files and consent are still reread.
        let config_command = GitCommand {
            arguments: [
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                "config",
                "--null",
                "--list",
                "--includes",
            ]
            .map(OsString::from)
            .to_vec(),
            env: command.env.clone(),
            ..Default::default()
        };
        let config = self.inner.run(repository, &config_command)?;
        ensure!(
            config.status == 0,
            "{}",
            git_failure("read origin query configuration", &config)
        );
        let mut configuration = blake3::Hasher::new();
        configuration.update(&config.stdout);
        for (name, value) in &command.env {
            // The ceiling's random directory only prevents parent discovery;
            // neutral probes share the same host config and literal URL.
            let value = if neutral_probe && name == "GIT_CEILING_DIRECTORIES" {
                std::ffi::OsStr::new("")
            } else {
                value.as_os_str()
            };
            for bytes in [name.as_encoded_bytes(), value.as_encoded_bytes()] {
                configuration.update(&(bytes.len() as u64).to_be_bytes());
                configuration.update(bytes);
            }
        }
        let key = OriginQueryKey {
            repository: if neutral_probe {
                PathBuf::new()
            } else {
                repository.to_path_buf()
            },
            arguments: command.arguments.clone(),
            configuration: *configuration.finalize().as_bytes(),
        };
        let cached = self
            .origin_queries
            .lock()
            .map_err(|_| anyhow::anyhow!("origin query cache poisoned"))?
            .get(&key)
            .cloned();
        let result = if let Some(cached) = cached {
            cached
        } else {
            let observed = self
                .inner
                .run(repository, &command)
                .map_err(|error| format!("{error:#}"));
            let after = self.inner.run(repository, &config_command)?;
            ensure!(
                after.status == 0,
                "{}",
                git_failure("verify origin query configuration", &after)
            );
            if config.stdout != after.stdout {
                return observed.map_err(anyhow::Error::msg);
            }
            self.origin_queries
                .lock()
                .map_err(|_| anyhow::anyhow!("origin query cache poisoned"))?
                .entry(key)
                .or_insert(observed)
                .clone()
        };
        result.map_err(anyhow::Error::msg)
    }
}

pub(crate) fn git_path(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    name: &str,
) -> Result<PathBuf> {
    let bytes = git_bytes(
        runner,
        checkout,
        ["rev-parse", "--git-path", name],
        &[],
        "resolve Git path",
    )?;
    // Strip only Git's terminating newline: spaces in paths are significant.
    crate::archive::git::absolute_git_path(checkout, &bytes)
}
fn launch_path(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    session_id: &str,
) -> Result<PathBuf> {
    ensure!(
        !session_id.is_empty()
            && session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "invalid session id"
    );
    let (_, common) = crate::archive::git::git_directories(runner, checkout)?;
    Ok(common.join("mj/sessions").join(session_id).join("launch"))
}
impl LaunchRecord {
    pub fn read(
        runner: &dyn GitCommandRunner,
        checkout: &Path,
        session_id: &str,
    ) -> Result<Option<Self>> {
        let path = launch_path(runner, checkout, session_id)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let record: Self = serde_json::from_slice(&bytes).context("decode launch record")?;
        ensure!(
            record.version == 1,
            "unsupported launch record version {}",
            record.version
        );
        Ok(Some(record))
    }
}
pub fn record_launch(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    session_id: &str,
) -> Result<()> {
    let record = LaunchRecord {
        version: 1,
        refs: read_refs(runner, checkout)?,
        stash: read_stash(runner, checkout)?,
        worktrees: discover_linked_worktrees(runner, checkout)?
            .into_iter()
            .map(|w| w.relative)
            .collect(),
    };
    let path = launch_path(runner, checkout, session_id)?;
    let parent = path.parent().context("launch path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut bytes = serde_json::to_vec(&record)?;
    bytes.push(b'\n');
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut file, &bytes)?;
    file.as_file().sync_all()?;
    file.persist(&path)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
fn local_snapshot_ref(name: &str) -> bool {
    name.starts_with("refs/heads/")
        || name.starts_with("refs/tags/")
        || name.starts_with("refs/notes/")
        || (name.starts_with("refs/mj/")
            && !name.starts_with("refs/mj/snapshot/")
            && !name.starts_with("refs/mj/restore/")
            && !mj_core::archive::is_review_baseline_pin(name))
}
fn read_all_refs(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<BTreeMap<String, String>> {
    let text = git_text(
        runner,
        checkout,
        [
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(symref)",
            "refs/heads",
            "refs/tags",
            "refs/notes",
            "refs/mj/",
            "refs/remotes/origin/",
        ],
    )?;
    let mut refs = BTreeMap::new();
    for line in text.lines() {
        let mut fields = line.split('\0');
        let name = fields.next().context("missing Git ref name")?;
        let oid = fields.next().context("missing Git ref object")?;
        let symbolic = fields.next().context("missing Git symbolic ref field")?;
        // Keep local symbolic aliases as ordinary refs at their resolved OID.
        if local_snapshot_ref(name)
            || (symbolic.is_empty() && name.starts_with("refs/remotes/origin/"))
        {
            refs.insert(name.to_owned(), oid.to_owned());
        }
    }
    Ok(refs)
}
fn read_refs(runner: &dyn GitCommandRunner, checkout: &Path) -> Result<BTreeMap<String, String>> {
    Ok(read_all_refs(runner, checkout)?
        .into_iter()
        .filter(|(name, _)| local_snapshot_ref(name))
        .collect())
}
fn selected_stash(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
) -> Result<Vec<SnapshotStashEntry>> {
    if matches!(ownership, CheckoutOwnership::Managed)
        && !owns_git_directory(runner, checkout, ownership)?
    {
        return Ok(Vec::new());
    }
    let stash = read_stash(runner, checkout)?;
    let CheckoutOwnership::Raw {
        launch: Some(launch),
    } = ownership
    else {
        return Ok(stash);
    };
    let mut old = BTreeMap::<(String, String), usize>::new();
    for entry in &launch.stash {
        *old.entry((entry.commit.clone(), entry.message.clone()))
            .or_default() += 1;
    }
    // Oldest-first matching leaves newer repeated entries in the snapshot.
    let mut added = Vec::new();
    for entry in stash.into_iter().rev() {
        let count = old
            .entry((entry.commit.clone(), entry.message.clone()))
            .or_default();
        if *count > 0 {
            *count -= 1;
        } else {
            added.push(entry);
        }
    }
    added.reverse();
    Ok(added)
}
fn stash_has_untracked(runner: &dyn GitCommandRunner, checkout: &Path, oid: &str) -> Result<bool> {
    let parents = git_text(runner, checkout, ["rev-list", "--parents", "-n", "1", oid])?;
    Ok(parents.split_whitespace().count() > 3)
}
/// A stash's third parent contains untracked files. Rebuild only its tracked
/// tree and index parents, using the shared internal identity and original message.
fn tracked_stash(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    entry: &SnapshotStashEntry,
) -> Result<SnapshotStashEntry> {
    if !stash_has_untracked(runner, checkout, &entry.commit)? {
        return Ok(entry.clone());
    }
    let raw = git_bytes(
        runner,
        checkout,
        ["cat-file", "commit", &entry.commit],
        &[],
        "read stash message",
    )?;
    let message_start = raw
        .windows(2)
        .position(|bytes| bytes == b"\n\n")
        .context("stash commit has no message separator")?
        + 2;
    let mut message = raw[message_start..].to_vec();
    if !message.ends_with(b"\n") {
        message.push(b'\n');
    }
    // Distinct -u stashes can have identical tracked parents and messages.
    // Keep their identities distinct without retaining the discarded parent.
    message.extend_from_slice(format!("\nMjolnir-Source-Stash: {}\n", entry.commit).as_bytes());
    let output = runner.run(
        checkout,
        &GitCommand {
            arguments: [
                "commit-tree".to_owned(),
                format!("{}^{{tree}}", entry.commit),
                "-p".to_owned(),
                format!("{}^1", entry.commit),
                "-p".to_owned(),
                format!("{}^2", entry.commit),
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
            stdin: message,
            env: synthetic_git_commit_identity(),
        },
    )?;
    ensure!(
        output.status == 0,
        "{}",
        git_failure("strip stash untracked parent", &output)
    );
    Ok(SnapshotStashEntry {
        commit: String::from_utf8(output.stdout)?.trim().to_owned(),
        message: entry.message.clone(),
    })
}
fn read_stash(runner: &dyn GitCommandRunner, checkout: &Path) -> Result<Vec<SnapshotStashEntry>> {
    let bytes = git_bytes(
        runner,
        checkout,
        ["stash", "list", "--format=%H%x00%gs"],
        &[],
        "read stash list",
    )?;
    let mut entries = std::str::from_utf8(&bytes)?
        .lines()
        .map(|line| {
            let (commit, message) = line.split_once('\0').context("malformed stash entry")?;
            Ok(SnapshotStashEntry {
                commit: commit.to_owned(),
                message: message.to_owned(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let current = run_git(
        runner,
        checkout,
        ["rev-parse", "--verify", "--quiet", "refs/stash"],
        &[],
    )?;
    ensure!(
        matches!(current.status, 0 | 1),
        "{}",
        git_failure("read current stash ref", &current)
    );
    if current.status == 0 {
        let commit = std::str::from_utf8(&current.stdout)?.trim();
        if entries.first().is_none_or(|entry| entry.commit != commit) {
            entries.insert(
                0,
                SnapshotStashEntry {
                    commit: commit.to_owned(),
                    message: git_text(runner, checkout, ["show", "-s", "--format=%s", commit])?,
                },
            );
        }
    }
    Ok(entries)
}
fn read_head(runner: &dyn GitCommandRunner, checkout: &Path) -> Result<SnapshotHead> {
    let symbolic = run_git(runner, checkout, ["symbolic-ref", "-q", "HEAD"], &[])?;
    match symbolic.status {
        0 => {
            let name = String::from_utf8(symbolic.stdout)?;
            let name = name.strip_suffix('\n').unwrap_or(&name).to_owned();
            let commit = git_text(runner, checkout, ["rev-parse", "HEAD"])?;
            Ok(SnapshotHead::Branch { name, commit })
        }
        1 => Ok(SnapshotHead::Detached(git_text(
            runner,
            checkout,
            ["rev-parse", "HEAD"],
        )?)),
        _ => Err(git_failure("read symbolic HEAD", &symbolic)),
    }
}
pub(crate) fn operation(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<Option<GitOperationKind>> {
    for (name, kind) in [
        ("rebase-merge", GitOperationKind::Rebase),
        ("rebase-apply", GitOperationKind::Rebase),
        ("MERGE_HEAD", GitOperationKind::Merge),
        ("CHERRY_PICK_HEAD", GitOperationKind::CherryPick),
        ("REVERT_HEAD", GitOperationKind::Revert),
        ("sequencer", GitOperationKind::CherryPick),
        ("BISECT_LOG", GitOperationKind::Bisect),
        ("BISECT_START", GitOperationKind::Bisect),
    ] {
        let path = git_path(runner, checkout, name)?;
        if path.try_exists()? {
            if name == "rebase-apply" && path.join("applying").try_exists()? {
                return Ok(Some(GitOperationKind::Am));
            }
            if name == "sequencer" {
                let todo = fs::read_to_string(path.join("todo"))?;
                return Ok(Some(if todo.lines().any(|l| l.starts_with("revert ")) {
                    GitOperationKind::Revert
                } else {
                    kind
                }));
            }
            return Ok(Some(kind));
        }
    }
    Ok(None)
}
fn hash_file_metadata(identity: &mut blake3::Hasher, metadata: &fs::Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        identity.update(&metadata.dev().to_be_bytes());
        identity.update(&metadata.ino().to_be_bytes());
    }
    #[cfg(windows)]
    {
        // Stable Rust has no Windows file-index accessor. Git's lockfile rename
        // gives each occurrence a new creation timestamp with 100 ns precision.
        identity.update(format!("{:?}", metadata.created()?).as_bytes());
    }
    // Shown discard consent includes the displayed length, even when a writer
    // preserves the inode and restores its previous modification timestamp.
    identity.update(&metadata.len().to_be_bytes());
    let modified = metadata.modified()?;
    match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => {
            identity.update(&[0]);
            identity.update(&duration.as_nanos().to_be_bytes());
        }
        Err(error) => {
            identity.update(&[1]);
            identity.update(&error.duration().as_nanos().to_be_bytes());
        }
    }
    Ok(())
}
fn operation_record(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    key: &CheckoutKey,
) -> Result<Option<GitOperation>> {
    let Some(kind) = operation(runner, checkout)? else {
        return Ok(None);
    };
    let paths: &[&str] = match kind {
        GitOperationKind::Merge => &[
            "MERGE_HEAD",
            "MERGE_MSG",
            "MERGE_MODE",
            "ORIG_HEAD",
            "MERGE_AUTOSTASH",
        ],
        GitOperationKind::Rebase => &[
            "rebase-merge/head-name",
            "rebase-merge/onto",
            "rebase-merge/orig-head",
            "rebase-merge/autostash",
            "rebase-apply/autostash",
            "rebase-apply/head-name",
            "rebase-apply/onto",
            "rebase-apply/orig-head",
        ],
        GitOperationKind::CherryPick => &["CHERRY_PICK_HEAD", "sequencer/head", "sequencer/todo"],
        GitOperationKind::Revert => &["REVERT_HEAD", "sequencer/head", "sequencer/todo"],
        GitOperationKind::Am => &[
            "rebase-apply/head-name",
            "rebase-apply/onto",
            "rebase-apply/orig-head",
            "rebase-apply/applying",
            "rebase-apply/next",
        ],
        GitOperationKind::Bisect => &["BISECT_START", "BISECT_LOG"],
    };
    let mut identity = blake3::Hasher::new();
    identity.update(format!("{kind:?}").as_bytes());
    identity.update(&serde_json::to_vec(&read_head(runner, checkout)?)?);
    for name in paths {
        identity.update(name.as_bytes());
        let path = git_path(runner, checkout, name)?;
        match fs::File::open(&path) {
            Ok(mut file) => {
                let metadata = file.metadata()?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                identity.update(&[1]);
                let bytes = if *name == "BISECT_LOG" {
                    bytes.split(|byte| *byte == b'\n').next().unwrap_or(&[])
                } else {
                    &bytes
                };
                identity.update(&(bytes.len() as u64).to_be_bytes());
                identity.update(bytes);
                // Recreating an operation with identical inputs is a new consent
                // decision too. Git rewrites these state files for the new run.
                hash_file_metadata(&mut identity, &metadata)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                identity.update(&[0]);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read operation state {}", path.display()));
            }
        }
    }
    Ok(Some(GitOperation {
        checkout: key.clone(),
        kind,
        identity: identity.finalize().to_hex().to_string(),
    }))
}
fn abort_operation_arguments(kind: GitOperationKind) -> [&'static str; 2] {
    match kind {
        GitOperationKind::Merge => ["merge", "--abort"],
        GitOperationKind::Rebase => ["rebase", "--abort"],
        GitOperationKind::CherryPick => ["cherry-pick", "--abort"],
        GitOperationKind::Revert => ["revert", "--abort"],
        GitOperationKind::Am => ["am", "--abort"],
        GitOperationKind::Bisect => ["bisect", "reset"],
    }
}
/// Managed callers must stop every writer and hold exclusive checkout ownership
/// from inventory/consent through capture completion. External Git writers are
/// outside this contract; raw checkouts are never aborted.
fn prepare_operation(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
    operation: Option<GitOperationKind>,
) -> Result<bool> {
    if let Some(kind) = operation {
        if matches!(ownership, CheckoutOwnership::Raw { .. }) {
            return Ok(true);
        }
        let args = abort_operation_arguments(kind);
        git_text(runner, checkout, args)?;
    }
    Ok(false)
}
fn submodule_uncommitted_state(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    inventory: &WorkspaceInventory,
) -> Result<bool> {
    for module in &inventory.initialized_submodules {
        if !inventory.index_gitlinks.iter().any(|link| {
            link.value.path == module.value.path && link.value.commit == module.value.commit
        }) || staged_gitlink_changes(runner, &checkout.join(&module.value.path))?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn staged_gitlink_changes(runner: &dyn GitCommandRunner, checkout: &Path) -> Result<bool> {
    let raw = git_bytes(
        runner,
        checkout,
        [
            "diff",
            "--cached",
            "--ignore-submodules=none",
            "--raw",
            "--no-renames",
            "--no-ext-diff",
            "-z",
        ],
        &[],
        "inspect staged gitlinks",
    )?;
    let fields = raw
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let (pairs, remainder) = fields.as_chunks::<2>();
    ensure!(remainder.is_empty(), "malformed staged Git diff");
    Ok(pairs.iter().any(|pair| {
        pair[0].starts_with(b":160000 ")
            || pair[0].split(|byte| *byte == b' ').nth(1) == Some(b"160000".as_slice())
    }))
}
pub(crate) fn intent_to_add_paths(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<Vec<PathBuf>> {
    let changes = |visibility| -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        let raw = git_bytes(
            runner,
            checkout,
            [
                "diff",
                "--cached",
                "--raw",
                "--no-abbrev",
                "--no-renames",
                "--no-ext-diff",
                "--ignore-submodules=all",
                visibility,
                "-z",
            ],
            &[],
            "inspect intent-to-add index entries",
        )?;
        let fields = raw
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        let (pairs, remainder) = fields.as_chunks::<2>();
        ensure!(remainder.is_empty(), "malformed intent-to-add index diff");
        Ok(pairs
            .iter()
            .map(|pair| (pair[1].to_vec(), pair[0].to_vec()))
            .collect())
    };
    let invisible = changes("--ita-invisible-in-index")?;
    let visible = changes("--ita-visible-in-index")?;
    // Compare complete entry identities in both directions. For a HEAD path,
    // ITA can be a modification when visible and a deletion when invisible;
    // an empty HEAD blob may disappear from the visible diff altogether.
    invisible
        .keys()
        .chain(visible.keys())
        .filter(|path| invisible.get(*path) != visible.get(*path))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|path| mj_core::path_input::from_git_bytes(path))
        .collect()
}

/// A private index beside the real one preserves split-index lookup while
/// letting snapshot inspection remove gitlinks or create stashes safely.
pub(crate) fn private_index(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<tempfile::NamedTempFile> {
    let index = git_path(runner, checkout, "index")?;
    let temporary =
        tempfile::NamedTempFile::new_in(index.parent().context("index has no parent")?)?;
    if index.try_exists()? {
        fs::copy(&index, temporary.path())?;
    } else {
        fs::remove_file(temporary.path())?;
    }
    Ok(temporary)
}
fn working_state(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    skipped: bool,
) -> Result<Option<String>> {
    if skipped {
        return Ok(None);
    }
    // stash create writes its index even with optional locks disabled.
    let temporary = private_index(runner, checkout)?;
    let mut env = synthetic_git_commit_identity();
    env.push((
        "GIT_INDEX_FILE".into(),
        temporary.path().as_os_str().to_owned(),
    ));
    // Editing files needs no user identity; synthetic stash commits use ours.
    let output = runner.run(
        checkout,
        &GitCommand {
            arguments: vec!["stash".into(), "create".into()],
            env,
            ..Default::default()
        },
    )?;
    ensure!(
        output.status == 0,
        "{}",
        git_failure("capture tracked working state", &output)
    );
    let state = String::from_utf8(output.stdout)?.trim().to_owned();
    Ok((!state.is_empty()).then_some(state))
}
fn saved_head(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    skipped: bool,
) -> Result<SnapshotHead> {
    if skipped {
        match operation(runner, checkout)? {
            Some(GitOperationKind::Rebase) => {
                for dir in ["rebase-merge", "rebase-apply"] {
                    let path = git_path(runner, checkout, dir)?.join("head-name");
                    if path.try_exists()? {
                        let name = fs::read_to_string(path)?;
                        let name = name.strip_suffix('\n').unwrap_or(&name).to_owned();
                        // A forced sibling can advance the shared branch ref
                        // while this checkout is rebasing. The operation owns
                        // its saved position, regardless of that live ref.
                        let oid =
                            fs::read_to_string(git_path(runner, checkout, dir)?.join("orig-head"))?;
                        let commit = oid.trim().to_owned();
                        return Ok(if name.starts_with("refs/heads/") {
                            SnapshotHead::Branch { name, commit }
                        } else {
                            SnapshotHead::Detached(commit)
                        });
                    }
                }
            }
            Some(GitOperationKind::CherryPick | GitOperationKind::Revert) => {
                let path = git_path(runner, checkout, "sequencer/head")?;
                if path.try_exists()? {
                    let commit = fs::read_to_string(path)?.trim().to_owned();
                    return Ok(match read_head(runner, checkout)? {
                        SnapshotHead::Branch { name, .. } => SnapshotHead::Branch { name, commit },
                        SnapshotHead::Detached(_) => SnapshotHead::Detached(commit),
                    });
                }
            }
            Some(GitOperationKind::Am) => {
                let commit = git_text(runner, checkout, ["rev-parse", "ORIG_HEAD"])?;
                return Ok(match read_head(runner, checkout)? {
                    SnapshotHead::Branch { name, .. } => SnapshotHead::Branch { name, commit },
                    SnapshotHead::Detached(_) => SnapshotHead::Detached(commit),
                });
            }
            Some(GitOperationKind::Bisect) => {
                let start = fs::read_to_string(git_path(runner, checkout, "BISECT_START")?)?;
                let start = start.strip_suffix('\n').unwrap_or(&start);
                let branch = format!("refs/heads/{start}");
                let result = run_git(
                    runner,
                    checkout,
                    ["show-ref", "--verify", "--quiet", &branch],
                    &[],
                )?;
                return if result.status == 0 {
                    Ok(SnapshotHead::Branch {
                        commit: git_text(runner, checkout, ["rev-parse", &branch])?,
                        name: branch,
                    })
                } else {
                    Ok(SnapshotHead::Detached(git_text(
                        runner,
                        checkout,
                        ["rev-parse", start],
                    )?))
                };
            }
            _ => {}
        }
    }
    read_head(runner, checkout)
}
fn relative_local_remote(remote: &str) -> bool {
    // A colon before the first slash denotes Git's scp or transport syntax.
    // Native drive letters are handled first by Path::is_absolute.
    !Path::new(remote).is_absolute()
        && !remote
            .find(':')
            .is_some_and(|colon| remote.find('/').is_none_or(|slash| colon < slash))
}
/// Git's explicit helper syntax wraps an underlying URL (https::https://...).
fn transport_url(value: &str) -> (&str, &str) {
    let mut address = value;
    while let Some((helper, inner)) = address.split_once("::") {
        if helper.is_empty()
            || !helper
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"+.-_".contains(&byte))
        {
            break;
        }
        address = inner;
    }
    (&value[..value.len() - address.len()], address)
}
/// Strip credentials even from incomplete rewrite prefixes, which URL parsers
/// reject (for example https://user:token@ without a host).
fn redact_snapshot_url(value: &str) -> Result<String> {
    let (transport, address) = transport_url(value);
    if !transport.is_empty() {
        return Ok(format!("{transport}{}", redact_snapshot_url(address)?));
    }
    if let Ok(mut url) = url::Url::parse(value) {
        if matches!(url.scheme(), "http" | "https") {
            if url.username().is_empty() && url.password().is_none() {
                return Ok(value.to_owned());
            }
            url.set_username("")
                .map_err(|()| anyhow::anyhow!("cannot redact Git URL username"))?;
            url.set_password(None)
                .map_err(|()| anyhow::anyhow!("cannot redact Git URL password"))?;
            return Ok(url.to_string());
        }
        if url.password().is_some() {
            url.set_password(None)
                .map_err(|()| anyhow::anyhow!("cannot redact Git URL password"))?;
            return Ok(url.to_string());
        }
    }
    if let Some(scheme) = value.find("://") {
        let authority = scheme + 3;
        let end = value[authority..]
            .find(['/', '?', '#'])
            .map_or(value.len(), |offset| authority + offset);
        if let Some(at) = value[authority..end].rfind('@') {
            let userinfo = &value[authority..authority + at];
            if value[..scheme].eq_ignore_ascii_case("http")
                || value[..scheme].eq_ignore_ascii_case("https")
            {
                return Ok(format!(
                    "{}{}",
                    &value[..authority],
                    &value[authority + at + 1..]
                ));
            }
            if let Some((username, _)) = userinfo.split_once(':') {
                return Ok(format!(
                    "{}{username}@{}",
                    &value[..authority],
                    &value[authority + at + 1..]
                ));
            }
        }
    } else if !Path::new(value).is_absolute()
        && let Some(at) = value.find('@')
        && value[at + 1..].contains(':')
        && let Some((username, _)) = value[..at].split_once(':')
    {
        return Ok(format!("{username}@{}", &value[at + 1..]));
    }
    Ok(value.to_owned())
}
fn capture_push_evidence(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    has_origin: bool,
    in_container: bool,
) -> Result<PushEvidence> {
    let unavailable = |reason: &str| PushEvidence::Unavailable {
        reason: reason.to_owned(),
    };
    if !has_origin {
        return Ok(unavailable("repository has no named origin"));
    }
    let mut configured = config_values(runner, checkout, "remote.origin.pushurl")?;
    if configured.is_empty() {
        configured = config_values(runner, checkout, "remote.origin.url")?;
    }
    if configured.iter().any(|url| url.contains('\n')) {
        return Ok(unavailable("a push destination contains a newline"));
    }
    let output = run_git(
        runner,
        checkout,
        ["remote", "get-url", "--push", "--all", "origin"],
        &[],
    )?;
    if output.status != 0 {
        return Ok(unavailable(
            "Git could not resolve origin's push destinations",
        ));
    }
    let Ok(text) = std::str::from_utf8(&output.stdout) else {
        return Ok(unavailable("push destinations could not be safely decoded"));
    };
    let Some(text) = text.strip_suffix('\n') else {
        return Ok(unavailable("Git returned ambiguous push destinations"));
    };
    let values = text.split('\n').collect::<Vec<_>>();
    if values.len() != configured.len() {
        return Ok(unavailable("Git returned ambiguous push destinations"));
    }
    let mut destinations = Vec::new();
    for value in values {
        let (_, address) = transport_url(value);
        if address.is_empty() || (address.contains("://") && url::Url::parse(address).is_err()) {
            return Ok(unavailable(
                "a push destination could not be safely redacted",
            ));
        }
        if !address.contains("://")
            && !relative_local_remote(address)
            && !Path::new(address).is_absolute()
        {
            let Some((host, _)) = address.split_once(':') else {
                return Ok(unavailable(
                    "an opaque push destination could not be safely redacted",
                ));
            };
            if host.is_empty()
                || address
                    .find('@')
                    .is_some_and(|at| address[..at].contains(':'))
            {
                return Ok(unavailable(
                    "an ambiguous push destination could not be safely redacted",
                ));
            }
        }
        let stored = stored_remote(runner, checkout, value)?;
        let (_, stored_address) = transport_url(&stored);
        if in_container
            && (Path::new(stored_address).is_absolute()
                || url::Url::parse(stored_address).is_ok_and(|url| url.scheme() == "file"))
        {
            return Ok(unavailable("container-local destination"));
        }
        destinations.push(stored);
    }
    if destinations.is_empty() {
        Ok(unavailable("origin has no push destinations"))
    } else {
        Ok(PushEvidence::Resolved(destinations))
    }
}
pub(crate) fn resolve_remote_path(checkout: &Path, remote: &str) -> Result<String> {
    let path = Path::new(remote);
    if !path.is_absolute() && !relative_local_remote(remote) {
        return Ok(remote.to_owned());
    }
    let absolute = std::path::absolute(checkout.join(path))?;
    let absolute = match fs::canonicalize(&absolute) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => absolute,
        Err(error) => return Err(error).context("resolve Git remote path"),
    };
    Ok(absolute
        .to_str()
        .context("Git remote path is not UTF-8")?
        .to_owned())
}
fn absolute_config_url(checkout: &Path, value: &str) -> Result<String> {
    if value.is_empty() || !relative_local_remote(value) {
        return Ok(value.to_owned());
    }
    // Preserve URL text for prefix rewrites; filesystem canonicalization can
    // move a symlinked destination outside its configured pushInsteadOf prefix.
    let anchor = if checkout.is_absolute() {
        checkout.to_path_buf()
    } else {
        std::path::absolute(checkout)?
    };
    let mut stored = anchor
        .join(value)
        .to_str()
        .context("Git config path is not UTF-8")?
        .to_owned();
    if value.ends_with('/') && !stored.ends_with('/') {
        stored.push('/');
    }
    Ok(stored)
}
pub(crate) fn stored_remote(
    _runner: &dyn GitCommandRunner,
    checkout: &Path,
    configured: &str,
) -> Result<String> {
    redact_snapshot_url(&absolute_config_url(checkout, configured)?)
}
pub(crate) struct CapturedUrlRules {
    pub rules: Vec<SnapshotConfigEntry>,
    pub notices: Vec<String>,
}
fn url_rule_parts(name: &str) -> Result<(&str, &str)> {
    name.strip_prefix("url.")
        .and_then(|key| key.rsplit_once('.'))
        .context("malformed local URL rule key")
}
fn url_has_userinfo(value: &str) -> bool {
    let (_, value) = transport_url(value);
    if let Some(scheme) = value.find("://") {
        let authority = scheme + 3;
        let end = value[authority..]
            .find(['/', '?', '#'])
            .map_or(value.len(), |offset| authority + offset);
        if let Some(at) = value[authority..end].rfind('@') {
            let userinfo = &value[authority..authority + at];
            return value[..scheme].eq_ignore_ascii_case("http")
                || value[..scheme].eq_ignore_ascii_case("https")
                || userinfo.contains(':');
        }
        false
    } else if !Path::new(value).is_absolute()
        && let Some(at) = value.find('@')
        && value[at + 1..].contains(':')
    {
        value[..at].contains(':')
    } else {
        false
    }
}
fn read_config_entries(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    worktree: bool,
    pattern: &str,
) -> Result<Vec<SnapshotConfigEntry>> {
    let output = run_git(
        runner,
        checkout,
        [
            "config",
            if worktree { "--worktree" } else { "--local" },
            "--includes",
            "--null",
            "--get-regexp",
            pattern,
        ],
        &[],
    )?;
    ensure!(
        output.status == 0 || output.status == 1,
        "{}",
        git_failure("read local URL rules", &output)
    );
    let mut rules = Vec::new();
    for entry in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let split = entry
            .iter()
            .position(|byte| *byte == b'\n')
            .context("malformed local URL rule")?;
        let name = std::str::from_utf8(&entry[..split])?.to_owned();
        rules.push(SnapshotConfigEntry {
            name,
            value: std::str::from_utf8(&entry[split + 1..])?.to_owned(),
        });
    }
    Ok(rules)
}
fn absolute_url_rule(checkout: &Path, rule: &SnapshotConfigEntry) -> Result<SnapshotConfigEntry> {
    let (base, kind) = url_rule_parts(&rule.name)?;
    Ok(SnapshotConfigEntry {
        name: format!("url.{}.{kind}", absolute_config_url(checkout, base)?),
        // The RHS is Git's literal match prefix, not a filesystem path.
        value: rule.value.clone(),
    })
}
/// Let Git own rewrite selection. Anchoring may merge distinct URL spellings,
/// so a transfer refuses when that would change its effective destination.
pub(crate) fn validate_origin_rewrite_anchoring(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    origin_override: Option<&str>,
) -> Result<()> {
    let mut urls = config_values(runner, checkout, "remote.origin.url")?;
    let has_origin = !urls.is_empty();
    if !has_origin {
        urls.extend(origin_override.map(str::to_owned));
    }
    if urls.is_empty() {
        return Ok(());
    }
    let pushes = if has_origin {
        config_values(runner, checkout, "remote.origin.pushurl")?
    } else {
        Vec::new()
    };
    static NEXT_PROBE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = loop {
        let name = format!(
            "mj-snapshot-url-{}-{}",
            std::process::id(),
            NEXT_PROBE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let existing = run_git(
            runner,
            checkout,
            ["config", "--get-regexp", &format!(r"^remote\.{name}\.")],
            &[],
        )?;
        ensure!(
            matches!(existing.status, 0 | 1),
            "{}",
            git_failure("select temporary URL probe name", &existing)
        );
        if existing.status == 1 {
            break name;
        }
    };
    let resolve = |url: &str, push: bool, explicit_push: bool| -> Result<String> {
        let mut arguments = vec!["-c".into(), format!("remote.{name}.url={url}").into()];
        let push_listing = push && !explicit_push;
        if push_listing {
            arguments.extend(["remote".into(), "-v".into()]);
        } else {
            // Explicit pushurls use insteadOf, never pushInsteadOf. A named
            // command-scoped remote also keeps a literal URL like "fork" from
            // being confused with an existing source remote of that name.
            arguments.extend(["ls-remote".into(), "--get-url".into(), name.clone().into()]);
        }
        let output = runner.run(
            checkout,
            &GitCommand {
                arguments,
                ..Default::default()
            },
        )?;
        ensure!(
            output.status == 0,
            "{}",
            git_failure("resolve snapshot URL with Git", &output)
        );
        let text = std::str::from_utf8(&output.stdout)?;
        let resolved = if push_listing {
            // get-url requires a repository-scoped remote even though -v and
            // ls-remote see command-scoped ones (also on Git 2.31). Reject
            // ambiguous human-readable row delimiters before using the result.
            let prefix = format!("{name}\t");
            let rows = text
                .split('\n')
                .filter_map(|line| line.strip_prefix(&prefix))
                .collect::<Vec<_>>();
            let fetch = rows.first().and_then(|line| line.strip_suffix(" (fetch)"));
            let push = rows.get(1).and_then(|line| line.strip_suffix(" (push)"));
            ensure!(
                !url.contains(['\t', '\n'])
                    && rows.len() == 2
                    && fetch.is_some_and(|url| !url.contains('\t'))
                    && push.is_some_and(|url| !url.contains('\t')),
                "origin push URL {} has ambiguous tab/newline delimiters in Git's rewrite result; configure an unambiguous absolute destination with `git remote set-url --push origin <absolute>` and adjust its URL rule, then retry",
                redact_snapshot_url(url)?
            );
            push.context("missing synthetic push URL")?
        } else {
            text.strip_suffix('\n').unwrap_or(text)
        };
        absolute_config_url(checkout, resolved)
    };
    for (url, push, explicit_push) in urls
        .iter()
        .map(|url| (url, false, false))
        .chain(
            urls.iter()
                .filter(|_| has_origin && pushes.is_empty())
                .map(|url| (url, true, false)),
        )
        .chain(pushes.iter().map(|url| (url, true, true)))
    {
        let anchored = absolute_config_url(checkout, url)?;
        let original_outcome = resolve(url, push, explicit_push)?;
        let anchored_outcome = if url == &anchored {
            original_outcome.clone()
        } else {
            resolve(&anchored, push, explicit_push)?
        };
        ensure!(
            original_outcome == anchored_outcome,
            "anchoring origin {} URL {} changes Git's destination from {} to {}; configure an absolute URL with `git remote {}{}origin <absolute>` and adjust its URL rewrite rule to retain the intended destination, then retry",
            if push { "push" } else { "fetch" },
            redact_snapshot_url(url)?,
            redact_snapshot_url(&original_outcome)?,
            redact_snapshot_url(&anchored_outcome)?,
            if has_origin { "set-url " } else { "add " },
            if push { "--push " } else { "" }
        );
    }
    Ok(())
}

fn capture_url_rules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    worktree: bool,
) -> Result<CapturedUrlRules> {
    let mut captured = CapturedUrlRules {
        rules: Vec::new(),
        notices: Vec::new(),
    };
    for rule in read_config_entries(runner, checkout, worktree, r"^url\.")? {
        let (base, kind) = url_rule_parts(&rule.name)?;
        if !matches!(kind, "insteadof" | "pushinsteadof") {
            continue;
        }
        if url_has_userinfo(base) || url_has_userinfo(&rule.value) {
            captured.notices.push(format!(
                "omitted credential-bearing URL rule url.{}.{kind}",
                redact_snapshot_url(base)?
            ));
        } else {
            captured.rules.push(absolute_url_rule(checkout, &rule)?);
        }
    }
    Ok(captured)
}
pub(crate) fn local_url_rules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<CapturedUrlRules> {
    capture_url_rules(runner, checkout, false)
}
pub(crate) fn worktree_config_enabled(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<bool> {
    let enabled = run_git(
        runner,
        checkout,
        ["config", "--bool", "--get", "extensions.worktreeConfig"],
        &[],
    )?;
    ensure!(
        enabled.status == 0 || enabled.status == 1,
        "{}",
        git_failure("read worktree config mode", &enabled)
    );
    Ok(enabled.status == 0 && enabled.stdout == b"true\n")
}
pub(crate) fn worktree_url_rules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<CapturedUrlRules> {
    if worktree_config_enabled(runner, checkout)? {
        capture_url_rules(runner, checkout, true)
    } else {
        Ok(CapturedUrlRules {
            rules: Vec::new(),
            notices: Vec::new(),
        })
    }
}
pub(crate) fn worktree_remote_notice(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<Option<String>> {
    if worktree_config_enabled(runner, checkout)?
        && !read_config_entries(runner, checkout, true, r"^remote\.origin\.")?.is_empty()
    {
        Ok(Some("worktree-specific remote settings are not restored; every restored worktree uses the primary checkout's origin".to_owned()))
    } else {
        Ok(None)
    }
}
fn direct_config_keys(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    scope: &str,
    pattern: &str,
) -> Result<std::collections::BTreeSet<String>> {
    let names = run_git(
        runner,
        checkout,
        [
            "config",
            scope,
            "--no-includes",
            "--null",
            "--name-only",
            "--get-regexp",
            pattern,
        ],
        &[],
    )?;
    ensure!(
        matches!(names.status, 0 | 1),
        "{}",
        git_failure("list direct repository configuration", &names)
    );
    names
        .stdout
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8(name.to_vec()).map_err(Into::into))
        .collect()
}
fn clear_direct_url_rules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    worktree: bool,
) -> Result<()> {
    let scope = if worktree { "--worktree" } else { "--local" };
    // Included rules are destination-owned, like the host's global configuration.
    for name in direct_config_keys(runner, checkout, scope, r"^url\.")? {
        git_text(runner, checkout, ["config", scope, "--unset-all", &name])?;
    }
    Ok(())
}

fn validate_url_rules<'a>(rules: impl IntoIterator<Item = &'a SnapshotConfigEntry>) -> Result<()> {
    for rule in rules {
        let (base, kind) = url_rule_parts(&rule.name)?;
        ensure!(
            !base.is_empty()
                && (kind.eq_ignore_ascii_case("insteadof")
                    || kind.eq_ignore_ascii_case("pushinsteadof")),
            "unsupported snapshot URL rewrite key {:?}",
            rule.name
        );
    }
    Ok(())
}
fn restore_url_rules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    rules: &[SnapshotConfigEntry],
) -> Result<()> {
    validate_url_rules(rules)?;
    clear_direct_url_rules(runner, checkout, false)?;
    for rule in rules {
        git_text(
            runner,
            checkout,
            ["config", "--local", "--add", &rule.name, &rule.value],
        )?;
    }
    Ok(())
}
/// Resolve .gitmodules URLs against the only remote restore recreates: origin.
pub(crate) fn resolve_submodule_url(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    url: &str,
    origin_override: Option<&str>,
) -> Result<String> {
    if !url.starts_with("./") && !url.starts_with("../") {
        return absolute_config_url(checkout, url);
    }
    let base = config_values(runner, checkout, "remote.origin.url")?;
    match origin_override.or_else(|| base.last().map(String::as_str)) {
        Some(base) => relative_submodule_url(base, url, checkout),
        None => resolve_remote_path(checkout, url),
    }
}
/// Git treats the superproject URL as a directory when resolving ../sub.git.
fn relative_submodule_url(base: &str, relative: &str, checkout: &Path) -> Result<String> {
    let is_url = base.contains("://");
    let scp_colon = (!is_url && !Path::new(base).is_absolute())
        .then(|| base.find(':'))
        .flatten();
    let mut base = if is_url || scp_colon.is_some() {
        base.to_owned()
    } else {
        absolute_config_url(checkout, base)?
    };
    #[cfg(windows)]
    if !is_url && scp_colon.is_none() {
        base = base.replace('\\', "/");
    }
    let minimum = if let Some((scheme, address)) = base.split_once("://") {
        scheme.len() + 3 + address.find('/').unwrap_or(address.len())
    } else {
        scp_colon.map_or(0, |colon| colon + 1)
    };
    base.truncate(base.trim_end_matches('/').len());
    let mut suffix = relative;
    // Git consumes only the leading ./ and ../ components. Preserve the
    // remaining suffix literally: an internal x/.. may cross a symlink.
    loop {
        if let Some(rest) = suffix.strip_prefix("./") {
            suffix = rest;
        } else if let Some(rest) = suffix.strip_prefix("../") {
            let split = base
                .rfind('/')
                .filter(|split| *split >= minimum)
                .or_else(|| {
                    scp_colon
                        .filter(|_| base.len() > minimum)
                        .map(|colon| colon + 1)
                });
            let split = split.context("relative submodule URL escapes its remote")?;
            base.truncate(split);
            suffix = rest;
        } else {
            break;
        }
    }
    let separator = if scp_colon.is_some() && base.ends_with(':') {
        ""
    } else {
        "/"
    };
    Ok(format!("{base}{separator}{suffix}"))
}

fn append_pack_extension(base: &Path, hash: &str, extension: &str) -> PathBuf {
    let mut path = base.as_os_str().to_owned();
    path.push(format!("-{hash}.{extension}"));
    path.into()
}
fn untracked_covered(consent: &UntrackedConsent, item: &UntrackedItem) -> bool {
    match consent {
        UntrackedConsent::None => false,
        UntrackedConsent::Any => true,
        UntrackedConsent::Shown(items) => items.contains(item),
    }
}
fn check_stash_consent(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    stash: &[SnapshotStashEntry],
    consent: &UntrackedConsent,
    key: &CheckoutKey,
) -> Result<()> {
    let mut missing = Vec::new();
    for entry in stash {
        if stash_has_untracked(runner, checkout, &entry.commit)? {
            let item = UntrackedItem::Stash {
                checkout: key.clone(),
                commit: entry.commit.clone(),
                message: entry.message.clone(),
            };
            if !untracked_covered(consent, &item) {
                missing.push(item);
            }
        }
    }
    ensure!(missing.is_empty(), SnapshotRefused::Untracked(missing));
    Ok(())
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedWorktree {
    pub relative_path: PathBuf,
    pub head: SnapshotHead,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedWorkspaceCut {
    pub refs: BTreeMap<String, String>,
    pub head: SnapshotHead,
    pub worktrees: Vec<PinnedWorktree>,
}
#[derive(Debug)]
struct CaptureCutChanged;
impl std::fmt::Display for CaptureCutChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("workspace cut changed while reading HEAD")
    }
}
impl std::error::Error for CaptureCutChanged {}
/// Read refs and HEADs without fetching, aborting operations, creating stashes
/// or changing anything. Branch commit IDs come from the one ref enumeration.
pub fn pin_workspace_cut(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
) -> Result<PinnedWorkspaceCut> {
    // Nested worktrees travel even if they existed at launch, by explicit policy.
    let nested = discover_linked_worktrees(runner, checkout)?;
    let refs = read_all_refs(runner, checkout)?;
    let read = |path: &Path| -> Result<SnapshotHead> {
        let raw_operation = matches!(ownership, CheckoutOwnership::Raw { .. })
            && operation(runner, path)?.is_some();
        let mut head = saved_head(runner, path, raw_operation)?;
        if !raw_operation && let SnapshotHead::Branch { name, commit } = &mut head {
            *commit = refs.get(name).ok_or(CaptureCutChanged)?.clone();
        }
        Ok(head)
    };
    let head = read(checkout)?;
    let mut worktrees = Vec::new();
    for tree in nested {
        worktrees.push(PinnedWorktree {
            relative_path: tree.relative,
            head: read(&tree.directory)?,
        });
    }
    Ok(PinnedWorkspaceCut {
        refs,
        head,
        worktrees,
    })
}
fn prepare_capture(
    runner: &dyn GitCommandRunner,
    repository: &RepositoryInput,
    consent: &CaptureConsent,
) -> Result<WorkspaceInventory> {
    let checkout = repository.checkout.as_path();
    let ownership = &repository.ownership;
    let inventory = repository_inventory(runner, repository)?;
    check_repository_consent(repository, &inventory, consent)?;
    prepare_operation(
        runner,
        checkout,
        ownership,
        inventory.operation.as_ref().map(|operation| operation.kind),
    )?;
    for (path, tree) in &inventory.worktrees {
        prepare_operation(
            runner,
            &checkout.join(path),
            ownership,
            tree.operation.as_ref().map(|operation| operation.kind),
        )?;
    }
    if !inventory.raw
        && (inventory.operation.is_some()
            || inventory
                .worktrees
                .iter()
                .any(|(_, tree)| tree.operation.is_some()))
    {
        // Abort can unregister initialized modules. Recheck unconditional
        // structural blockers against the final HEAD/index before pinning.
        let inventory = repository_inventory(runner, repository)?;
        check_repository_consent(repository, &inventory, consent)?;
        return Ok(inventory);
    }
    Ok(inventory)
}
struct ConsentInputs {
    untracked: Vec<UntrackedItem>,
    operations: Vec<ShownOperation>,
}
fn check_consent(inventory: &WorkspaceInventory, consent: &CaptureConsent) -> Result<()> {
    check_consent_inputs(
        inventory.raw,
        check_unconditional_blockers(inventory)?,
        consent,
    )
}
fn check_unconditional_blockers(inventory: &WorkspaceInventory) -> Result<ConsentInputs> {
    if let Some(blocker) = inventory.blockers.first() {
        return Err(blocker.kind.clone().into());
    }
    let mut untracked = Vec::new();
    let mut operations = Vec::new();
    for tree in std::iter::once(inventory).chain(inventory.worktrees.iter().map(|(_, tree)| tree)) {
        untracked.extend(tree.untracked.iter().map(|entry| UntrackedItem::Path {
            checkout: entry.checkout.clone(),
            path: entry.path.clone(),
            identity: entry.identity.clone(),
        }));
        untracked.extend(tree.nested_repositories.iter().map(|entry| {
            UntrackedItem::NestedRepository {
                checkout: entry.checkout.clone(),
                path: entry.path.clone(),
                identity: entry.identity.clone(),
            }
        }));
        untracked.extend(
            tree.untracked_stashes
                .iter()
                .map(|entry| UntrackedItem::Stash {
                    checkout: entry.checkout.clone(),
                    commit: entry.value.commit.clone(),
                    message: entry.value.message.clone(),
                }),
        );
        operations.extend(tree.operation.iter().cloned());
    }
    Ok(ConsentInputs {
        untracked,
        operations,
    })
}
fn inventory_blockers(inventory: &WorkspaceInventory) -> Result<Vec<InventoryBlocker>> {
    if inventory.in_place_only {
        return Ok(Vec::new());
    }
    let mut reasons = Vec::new();
    if !inventory.raw && inventory.origin_missing {
        reasons.push(SnapshotRefused::MissingManagedOrigin(
            inventory.checkout.clone(),
        ));
    }
    if !inventory.raw && !inventory.origins_removed_by_teardown.is_empty() {
        reasons.push(SnapshotRefused::OriginRemovedByTeardown(
            inventory.origins_removed_by_teardown.clone(),
        ));
    }
    if !inventory.raw && inventory.shallow {
        reasons.push(SnapshotRefused::ShallowRepository(
            inventory.checkout.clone(),
        ));
    }
    let mut directories = Vec::new();
    let mut worktrees = Vec::new();
    for tree in std::iter::once(inventory).chain(inventory.worktrees.iter().map(|(_, tree)| tree)) {
        if !tree.tracked_keep_files.is_empty() {
            reasons.push(SnapshotRefused::TrackedKeepFiles {
                keep_dir: tree
                    .keep_dir
                    .clone()
                    .context("tracked keep inventory has no keep directory")?,
                paths: tree.tracked_keep_files.clone(),
            });
        }
        if !(tree.raw && tree.operation.is_some()) {
            if !tree.untracked_submodule_registration.is_empty() {
                reasons.push(SnapshotRefused::UntrackedSubmoduleRegistration(
                    tree.untracked_submodule_registration.clone(),
                ));
            }
            if !tree.intent_to_add.is_empty() {
                reasons.push(SnapshotRefused::IntentToAdd(tree.intent_to_add.clone()));
            }
            if !tree.hidden_tracked_edits.is_empty() {
                reasons.push(SnapshotRefused::HiddenTrackedEdits(
                    tree.hidden_tracked_edits.clone(),
                ));
            }
        }
        if !tree.raw {
            if !tree.submodule_operations.is_empty() {
                reasons.push(SnapshotRefused::SubmoduleOperations(
                    tree.submodule_operations.clone(),
                ));
            }
            if !tree.submodule_local_work.is_empty() {
                reasons.push(SnapshotRefused::UnpublishedSubmoduleWork(
                    tree.submodule_local_work.clone(),
                ));
            }
            directories.extend(tree.unrepresented_git_dirs.clone());
            worktrees.extend(tree.unrepresented_worktrees.clone());
        }
        reasons.extend(
            tree.dirty_submodules
                .iter()
                .cloned()
                .map(SnapshotRefused::DirtySubmodule),
        );
    }
    if !directories.is_empty() || !worktrees.is_empty() {
        reasons.push(SnapshotRefused::UnrepresentedGitState {
            directories,
            worktrees,
        });
    }
    Ok(reasons
        .into_iter()
        .map(|kind| {
            let paths = match &kind {
                SnapshotRefused::TrackedKeepFiles { paths, .. }
                | SnapshotRefused::IntentToAdd(paths)
                | SnapshotRefused::HiddenTrackedEdits(paths)
                | SnapshotRefused::UntrackedSubmoduleRegistration(paths) => paths.clone(),
                SnapshotRefused::OriginRemovedByTeardown(origins) => origins
                    .iter()
                    .map(|origin| scoped(&origin.checkout, origin.value.route.clone()))
                    .collect(),
                SnapshotRefused::DirtySubmodule(path) => vec![path.clone()],
                SnapshotRefused::UnpublishedSubmoduleWork(items) => items
                    .iter()
                    .map(|item| scoped(&item.checkout, item.value.path.clone()))
                    .collect(),
                SnapshotRefused::SubmoduleOperations(items) => items
                    .iter()
                    .map(|item| scoped(&item.checkout, PathBuf::new()))
                    .collect(),
                SnapshotRefused::ShallowRepository(checkout)
                | SnapshotRefused::MissingManagedOrigin(checkout) => {
                    vec![scoped(checkout, PathBuf::new())]
                }
                SnapshotRefused::UnrepresentedGitState {
                    directories,
                    worktrees,
                } => directories
                    .iter()
                    .cloned()
                    .chain(
                        worktrees
                            .iter()
                            .map(|item| scoped(&item.checkout, item.value.path.clone())),
                    )
                    .collect(),
                SnapshotRefused::Untracked(_) | SnapshotRefused::OperationInProgress(_) => {
                    Vec::new()
                }
            };
            InventoryBlocker {
                remedy: kind.to_string(),
                paths,
                kind,
            }
        })
        .collect())
}
fn check_consent_inputs(raw: bool, inputs: ConsentInputs, consent: &CaptureConsent) -> Result<()> {
    let ConsentInputs {
        mut untracked,
        operations,
    } = inputs;
    untracked.retain(|entry| !untracked_covered(&consent.untracked, entry));
    ensure!(untracked.is_empty(), SnapshotRefused::Untracked(untracked));
    if !raw {
        for operation in operations {
            let covered = match &consent.operations {
                OperationConsent::None => false,
                OperationConsent::Any => true,
                OperationConsent::Shown(shown) => shown.contains(&operation),
            };
            ensure!(covered, SnapshotRefused::OperationInProgress(operation));
        }
    }
    Ok(())
}
pub(crate) struct OriginExclusions {
    pub objects: Vec<String>,
    pub stale: bool,
    pub notice: Option<String>,
}
fn refspec_suffix(pattern: &str, reference: &str) -> Option<String> {
    if let Some((prefix, suffix)) = pattern.split_once('*') {
        reference
            .strip_prefix(prefix)?
            .strip_suffix(suffix)
            .map(str::to_owned)
    } else {
        (pattern == reference).then(String::new)
    }
}
/// All effective source refs that could have populated a destination. Origin
/// shares the same provenance rule as other remotes; one eligible mapping is
/// not proof when another mapping can put pull/review-only history there.
fn mapped_fetch_sources(specs: &[String], destination: &str) -> std::collections::BTreeSet<String> {
    specs
        .iter()
        .filter(|spec| !spec.starts_with('^'))
        .filter_map(|spec| spec.trim_start_matches('+').split_once(':'))
        .filter_map(|(source, pattern)| {
            let suffix = refspec_suffix(pattern, destination)?;
            let source = source.replacen('*', &suffix, 1);
            (!specs
                .iter()
                .filter_map(|spec| spec.strip_prefix('^'))
                .any(|negative| refspec_suffix(negative, &source).is_some()))
            .then_some(source)
        })
        .collect()
}
pub(crate) struct OriginTracking {
    pub objects: Vec<String>,
    pub ambiguous: bool,
}
pub(crate) fn origin_tracking_objects(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<OriginTracking> {
    let specs = config_values(runner, checkout, "remote.origin.fetch")?;
    let mut other_mappings = Vec::new();
    for remote in git_text(runner, checkout, ["remote"])?
        .lines()
        .filter(|remote| *remote != "origin")
    {
        other_mappings.push(config_values(
            runner,
            checkout,
            &format!("remote.{remote}.fetch"),
        )?);
    }
    let mut ambiguous = false;
    let listed = git_text(
        runner,
        checkout,
        [
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(symref)",
        ],
    )?;
    let mut objects = Vec::new();
    for line in listed.lines() {
        let mut fields = line.split('\0');
        let name = fields.next().context("missing tracking ref name")?;
        let oid = fields.next().context("missing tracking ref object")?;
        if !fields
            .next()
            .context("missing tracking symref field")?
            .is_empty()
        {
            continue;
        }
        let sources = mapped_fetch_sources(&specs, name);
        let eligible =
            |source: &str| source.starts_with("refs/heads/") || source.starts_with("refs/tags/");
        if name.starts_with("refs/remotes/") && sources.iter().any(|source| eligible(source)) {
            if sources.iter().any(|source| !eligible(source))
                || other_mappings
                    .iter()
                    .any(|other| !mapped_fetch_sources(other, name).is_empty())
            {
                ambiguous = true;
            } else {
                objects.push(oid.to_owned());
            }
        }
    }
    objects.sort();
    objects.dedup();
    Ok(OriginTracking { objects, ambiguous })
}
/// Capture and dry-run share origin queries and objects-only downloads.
fn origin_exclusions(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    remote: &str,
    _refs: &BTreeMap<String, String>,
) -> Result<OriginExclusions> {
    let fallback = origin_tracking_objects(runner, checkout)?;
    let mut result = remote_objects(runner, checkout, remote, &fallback.objects)?;
    if result.stale && result.objects.is_empty() {
        result.notice = Some(if fallback.ambiguous {
            "snapshot carries full history because origin was unreachable and its offline refs were ambiguous"
        } else {
            "snapshot carries full history because origin was unreachable and no offline refs were available"
        }.to_owned());
        result.stale = false;
    }
    Ok(result)
}
pub(crate) fn remote_objects(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    remote: &str,
    fallback: &[String],
) -> Result<OriginExclusions> {
    let advertised = run_git(runner, checkout, ["ls-remote", remote], &[]);
    let mut objects = Vec::new();
    let mut stale = false;
    match advertised {
        Ok(output) if output.status == 0 => {
            let mut tips = Vec::new();
            for line in std::str::from_utf8(&output.stdout)?.lines() {
                let (oid, name) = line
                    .split_once('\t')
                    .context("malformed origin advertisement")?;
                if name.starts_with("refs/heads/") || name.starts_with("refs/tags/") {
                    tips.push(oid.to_owned());
                }
            }
            tips.sort();
            tips.dedup();
            let missing = missing_tips(runner, checkout, &tips)?;
            if !missing.is_empty() {
                let output = runner.run(
                    checkout,
                    &GitCommand {
                        arguments: [
                            "fetch",
                            "--no-write-fetch-head",
                            "--no-tags",
                            "--no-prune",
                            "--no-prune-tags",
                            "--refmap=",
                            "--no-recurse-submodules",
                            "--stdin",
                            remote,
                        ]
                        .into_iter()
                        .map(OsString::from)
                        .collect(),
                        stdin: format!("{}\n", missing.join("\n")).into_bytes(),
                        ..Default::default()
                    },
                );
                match output {
                    Ok(output) if output.status != 0 => tracing::warn!(
                        status = output.status,
                        "could not download advertised origin tips; packing locally missing history"
                    ),
                    Err(error) => tracing::warn!(%error,
                        "could not download advertised origin tips; packing locally missing history"),
                    _ => {}
                }
            }
            let unavailable = missing_tips(runner, checkout, &tips)?
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>();
            // A successful advertisement is authoritative even if downloading
            // a missing tip fails. Stale refs may cover force-pushed-away work.
            objects.extend(tips.into_iter().filter(|tip| !unavailable.contains(tip)));
        }
        _ => {
            // Deliberate policy even after an offline origin URL change: callers
            // warn on stale refs, and restore verifies all missing dependencies.
            objects.extend_from_slice(fallback);
            stale = true;
        }
    }
    objects.sort();
    objects.dedup();
    Ok(OriginExclusions {
        objects,
        stale,
        notice: None,
    })
}
fn snapshot_exclusions(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    remote: &str,
    refs: &BTreeMap<String, String>,
    source: &ExclusionSource,
) -> Result<OriginExclusions> {
    match source {
        ExclusionSource::Origin => origin_exclusions(runner, checkout, remote, refs),
        ExclusionSource::Commits(commits) => {
            let mut objects = Vec::new();
            for commit in commits {
                objects.push(
                    git_text(
                        runner,
                        checkout,
                        [
                            "rev-parse",
                            "--verify",
                            "--end-of-options",
                            &format!("{commit}^{{commit}}"),
                        ],
                    )
                    .with_context(|| format!("cutoff exclusion commit {commit} is unavailable"))?,
                );
            }
            objects.sort();
            objects.dedup();
            Ok(OriginExclusions {
                objects,
                stale: false,
                notice: None,
            })
        }
    }
}
pub(crate) fn config_values(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    key: &str,
) -> Result<Vec<String>> {
    let output = run_git(
        runner,
        checkout,
        ["config", "--null", "--get-all", key],
        &[],
    )?;
    if output.status == 1 {
        return Ok(Vec::new());
    }
    ensure!(
        output.status == 0,
        "{}",
        git_failure("read branch tracking", &output)
    );
    let bytes = output.stdout.strip_suffix(b"\0").unwrap_or(&output.stdout);
    bytes
        .split(|byte| *byte == 0)
        .map(|value| String::from_utf8(value.to_vec()).map_err(Into::into))
        .collect()
}
type BranchOwners<'a> = BTreeMap<&'a str, (&'a str, &'a Path)>;

fn branch_tracking(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    refs: &BTreeMap<String, String>,
    owners: &BranchOwners<'_>,
) -> Result<BTreeMap<String, BranchTracking>> {
    let mut tracking = BTreeMap::new();
    for reference in refs.keys() {
        let Some(name) = reference.strip_prefix("refs/heads/") else {
            continue;
        };
        let owner = checkout.join(
            owners
                .get(reference.as_str())
                .map(|(_, path)| *path)
                .unwrap_or_else(|| Path::new("")),
        );
        let remote = config_values(runner, &owner, &format!("branch.{name}.remote"))?
            .into_iter()
            .last();
        let merge = config_values(runner, &owner, &format!("branch.{name}.merge"))?;
        if remote.is_some() || !merge.is_empty() {
            tracking.insert(name.to_owned(), BranchTracking { remote, merge });
        }
    }
    Ok(tracking)
}
fn unset_config(runner: &dyn GitCommandRunner, checkout: &Path, key: &str) -> Result<()> {
    let output = run_git(runner, checkout, ["config", "--unset-all", key], &[])?;
    ensure!(
        matches!(output.status, 0 | 5),
        "{}",
        git_failure("clear cloned tracking configuration", &output)
    );
    Ok(())
}
// Choose once per cut: primary first, then nested checkout discovery order.
fn branch_owners(cut: &PinnedWorkspaceCut) -> BranchOwners<'_> {
    let mut owners = BTreeMap::new();
    for (head, path) in std::iter::once((&cut.head, Path::new(""))).chain(
        cut.worktrees
            .iter()
            .map(|tree| (&tree.head, tree.relative_path.as_path())),
    ) {
        if let SnapshotHead::Branch { name, commit } = head {
            owners
                .entry(name.as_str())
                .or_insert((commit.as_str(), path));
        }
    }
    owners
}
struct SelectedRefs {
    refs: BTreeMap<String, String>,
    complete: bool,
}
fn selected_refs(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    cut: &PinnedWorkspaceCut,
    ownership: &CheckoutOwnership,
    owners: &BTreeMap<&str, (&str, &Path)>,
) -> Result<SelectedRefs> {
    let legacy = matches!(ownership, CheckoutOwnership::Managed)
        && !owns_git_directory(runner, checkout, ownership)?;
    let launch = match ownership {
        CheckoutOwnership::Raw { launch } => launch.as_ref(),
        CheckoutOwnership::Managed => None,
    };
    let complete = !legacy && launch.is_none();
    let mut refs: BTreeMap<_, _> = cut
        .refs
        .iter()
        .filter(|(name, oid)| {
            if !local_snapshot_ref(name) {
                return false;
            }
            if legacy {
                return false;
            }
            launch.is_none_or(|launch| launch.refs.get(*name) != Some(*oid))
        })
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect();
    for (name, (commit, _)) in owners {
        if let Some(live) = cut.refs.get(*name).filter(|live| live.as_str() != *commit)
            && name.starts_with("refs/heads/")
        {
            refs.insert(format!("refs/mj/displaced/{live}"), live.clone());
        }
        refs.insert((*name).to_owned(), (*commit).to_owned());
    }
    Ok(SelectedRefs { refs, complete })
}
fn write_snapshot_pack(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    tips: &[String],
    exclusions: &[String],
    base: &Path,
) -> Result<Option<tempfile::TempPath>> {
    let mut tips = tips.to_vec();
    tips.sort();
    tips.dedup();
    let mut exclusions = exclusions.to_vec();
    exclusions.sort();
    exclusions.dedup();
    let mut revisions = tips.join("\n");
    revisions.push_str("\n--not\n");
    for oid in &exclusions {
        revisions.push_str(oid);
        revisions.push('\n');
    }
    let base = std::path::absolute(base)?;
    if let Some(parent) = base.parent() {
        fs::create_dir_all(parent)?;
    }
    // Git owns its scratch files until it exits; the directory then removes
    // incomplete packs on every failure before any final artifact is published.
    let scratch = tempfile::tempdir_in(base.parent().context("pack has no parent")?)?;
    let scratch_base = scratch.path().join("objects");
    // A snapshot has one pack artifact; ignore repository-configured splitting.
    let output = runner.run(
        checkout,
        &GitCommand {
            arguments: vec![
                "-c".into(),
                "pack.packSizeLimit=0".into(),
                "pack-objects".into(),
                "--revs".into(),
                scratch_base.as_os_str().to_owned(),
            ],
            stdin: revisions.into_bytes(),
            ..Default::default()
        },
    )?;
    ensure!(
        output.status == 0,
        "{}",
        git_failure("pack snapshot objects", &output)
    );
    let hash = String::from_utf8(output.stdout)?.trim().to_owned();
    ensure!(
        (hash.len() == 40 || hash.len() == 64) && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Git returned an invalid pack hash"
    );
    let path = append_pack_extension(&scratch_base, &hash, "pack");
    let mut header = [0_u8; 12];
    fs::File::open(&path)?.read_exact(&mut header)?;
    ensure!(
        header.starts_with(b"PACK"),
        "Git returned an invalid snapshot pack"
    );
    if header[8..12] == [0, 0, 0, 0] {
        fs::remove_file(path)?;
        Ok(None)
    } else {
        // Every in-flight capture owns a distinct path, even for identical
        // bytes and the same caller-provided base. No other capture can borrow
        // an artifact whose owner may still roll it back.
        let published = tempfile::Builder::new()
            .prefix(base.file_name().context("pack base has no filename")?)
            .suffix(".pack")
            .tempfile_in(base.parent().context("pack has no parent")?)?
            .into_temp_path();
        fs::rename(path, &published)?;
        Ok(Some(published))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaEstimate {
    pub committed_bytes: u64,
    /// Additional working-state pack bytes; unknown during an operation.
    pub working_state_bytes: Option<u64>,
    /// Combined pack bytes; unknown when tracked working state cannot be read.
    pub total_bytes: Option<u64>,
    pub remote_refs_stale: bool,
}
/// Dry run changes no ref, index, working file or stash list, and may add
/// unreferenced objects that garbage collection removes. It never aborts operations.
pub fn estimate_delta(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
    origin_override: Option<String>,
) -> Result<DeltaEstimate> {
    estimate_delta_with_source(
        runner,
        checkout,
        ownership,
        origin_override,
        &ExclusionSource::Origin,
    )
}
pub fn estimate_repository_delta(
    runner: &dyn GitCommandRunner,
    repository: &RepositoryInput,
) -> Result<DeltaEstimate> {
    estimate_delta_with_source(
        runner,
        &repository.checkout,
        &repository.ownership,
        repository.origin_override.clone(),
        &repository.exclusions,
    )
}
fn estimate_delta_with_source(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
    origin_override: Option<String>,
    source: &ExclusionSource,
) -> Result<DeltaEstimate> {
    let capture = CaptureGit::new(runner);
    let runner: &dyn GitCommandRunner = &capture;
    let configured = run_git(runner, checkout, ["remote", "get-url", "origin"], &[])?;
    let remote = if configured.status == 0 {
        Some("origin".to_owned())
    } else {
        origin_override
    };
    let Some(remote) = remote else {
        ensure!(
            matches!(ownership, CheckoutOwnership::Raw { .. }),
            "managed snapshot requires an origin remote or source override"
        );
        return Ok(DeltaEstimate {
            committed_bytes: 0,
            working_state_bytes: None,
            total_bytes: None,
            remote_refs_stale: false,
        });
    };
    let inventory = workspace_inventory_scoped(
        runner,
        checkout,
        ownership,
        &CheckoutKey::default(),
        Some(&remote),
    )?;
    let cut = pin_workspace_cut(runner, checkout, ownership)?;
    // Preflight above verifies that anchoring preserves literal URL rewrites.
    // An absolute override cannot be mistaken for an existing remote name.
    let remote = if configured.status == 0 {
        remote
    } else {
        absolute_config_url(checkout, &remote)?
    };
    let exclusions = snapshot_exclusions(runner, checkout, &remote, &cut.refs, source)?;
    let owners = branch_owners(&cut);
    let refs = selected_refs(runner, checkout, &cut, ownership, &owners)?.refs;
    let mut committed: Vec<_> = refs.values().cloned().collect();
    committed.push(cut.head.commit().to_owned());
    committed.extend(
        cut.worktrees
            .iter()
            .map(|tree| tree.head.commit().to_owned()),
    );
    for entry in selected_stash(runner, checkout, ownership)? {
        committed.push(tracked_stash(runner, checkout, &entry)?.commit);
    }
    committed.sort();
    committed.dedup();
    let temp = tempfile::tempdir().context("create Git delta estimate directory")?;
    let bytes = |tips: &[String], exclusions: &[String], name: &str| -> Result<u64> {
        write_snapshot_pack(runner, checkout, tips, exclusions, &temp.path().join(name))?
            .map(|path| {
                fs::metadata(path)
                    .map(|metadata| metadata.len())
                    .map_err(Into::into)
            })
            .transpose()
            .map(|bytes| bytes.unwrap_or(0))
    };
    let committed_bytes = bytes(&committed, &exclusions.objects, "committed")?;
    if inventory.operation.is_some()
        || inventory
            .worktrees
            .iter()
            .any(|(_, inventory)| inventory.operation.is_some())
    {
        return Ok(DeltaEstimate {
            committed_bytes,
            working_state_bytes: None,
            total_bytes: None,
            remote_refs_stale: exclusions.stale
                || inventory.remote_refs_stale
                || inventory
                    .worktrees
                    .iter()
                    .any(|(_, tree)| tree.remote_refs_stale),
        });
    }
    let mut working = Vec::new();
    working.extend(working_state(runner, checkout, false)?);
    for tree in &cut.worktrees {
        working.extend(working_state(
            runner,
            &checkout.join(&tree.relative_path),
            false,
        )?);
    }
    let mut working_exclusions = exclusions.objects.clone();
    working_exclusions.extend(committed.clone());
    let working_state_bytes = if working.is_empty() {
        0
    } else {
        bytes(&working, &working_exclusions, "working")?
    };
    let mut combined = committed;
    combined.extend(working);
    let total_bytes = bytes(&combined, &exclusions.objects, "combined")?;
    Ok(DeltaEstimate {
        committed_bytes,
        working_state_bytes: Some(working_state_bytes),
        total_bytes: Some(total_bytes),
        remote_refs_stale: exclusions.stale
            || inventory.remote_refs_stale
            || inventory
                .worktrees
                .iter()
                .any(|(_, tree)| tree.remote_refs_stale),
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum ExclusionSource {
    #[default]
    Origin,
    Commits(Vec<String>),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryInput {
    pub in_container: bool,
    pub id: String,
    pub checkout: PathBuf,
    pub ownership: CheckoutOwnership,
    pub origin_override: Option<String>,
    pub pack_path: PathBuf,
    #[serde(default)]
    pub exclusions: ExclusionSource,
    pub keep_dir: Option<PathBuf>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionCaptureConsent {
    /// Consent is scoped by repository ID, then by the path/item inside it.
    pub repositories: BTreeMap<String, CaptureConsent>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub repositories: BTreeMap<String, WorkspaceSnapshot>,
}
fn session_error(id: &str, error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<SnapshotRefused>().is_some() {
        error
    } else {
        error.context(format!("snapshot repository {id}"))
    }
}
fn repository_key(id: &str) -> CheckoutKey {
    CheckoutKey {
        repository_id: id.to_owned(),
        checkout_path: PathBuf::new(),
    }
}
fn check_repository_consent(
    _repository: &RepositoryInput,
    inventory: &WorkspaceInventory,
    consent: &CaptureConsent,
) -> Result<()> {
    check_consent(inventory, consent)
}
fn in_place_only_mode(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
    origin_override: Option<&str>,
) -> Result<bool> {
    Ok(matches!(ownership, CheckoutOwnership::Raw { .. })
        && origin_override.is_none()
        && run_git(runner, checkout, ["remote", "get-url", "origin"], &[])?.status != 0)
}
fn in_place_only_inventory(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    key: &CheckoutKey,
) -> Result<WorkspaceInventory> {
    let shallow = git_text(runner, checkout, ["rev-parse", "--is-shallow-repository"])? == "true";
    let mut notices = vec!["raw checkout has no origin; this snapshot resumes only in place and keeps files, index and operations unchanged".to_owned()];
    if shallow {
        notices.push("raw repository is shallow; snapshots do not preserve shallow metadata; the source checkout is kept".to_owned());
    }
    Ok(WorkspaceInventory {
        notices,
        checkout: key.clone(),
        in_place_only: true,
        origin_missing: true,
        shallow,
        operation: operation_record(runner, checkout, key)?,
        raw: true,
        hidden_tracked_edits: crate::archive::git::hidden_tracked_paths(runner, checkout)?
            .into_iter()
            .map(|path| scoped(key, path))
            .collect(),
        untracked: Vec::new(),
        untracked_stashes: Vec::new(),
        nested_repositories: Vec::new(),
        tracked_keep_files: Vec::new(),
        keep_dir: None,
        dirty_submodules: Vec::new(),
        untracked_submodule_registration: Vec::new(),
        submodule_local_work: Vec::new(),
        submodule_operations: Vec::new(),
        unrepresented_worktrees: Vec::new(),
        unrepresented_git_dirs: Vec::new(),
        origins_removed_by_teardown: Vec::new(),
        uninitialized_origins: Vec::new(),
        blockers: Vec::new(),
        intent_to_add: Vec::new(),
        initialized_submodules: Vec::new(),
        index_gitlinks: Vec::new(),
        remote_refs_stale: false,
        worktrees: Vec::new(),
    })
}

/// Inventory every repository and decide consent for the complete session before
/// the first abort or other mutation. Paths inside consent remain repository-relative.
/// The caller must have stopped all writers to managed checkouts and hold their
/// exclusive ownership until capture returns. Raw checkouts are never aborted.
pub fn capture_session_snapshot(
    runner: &dyn GitCommandRunner,
    repositories: &[RepositoryInput],
    consent: &SessionCaptureConsent,
) -> Result<SessionSnapshot> {
    let capture = CaptureGit::new(runner);
    let runner: &dyn GitCommandRunner = &capture;
    let none = CaptureConsent {
        untracked: UntrackedConsent::None,
        operations: OperationConsent::None,
    };
    enum CaptureMode {
        InPlaceOnly,
        Transfer(Box<WorkspaceInventory>),
    }
    let mut inventories = BTreeMap::new();
    for repository in repositories {
        ensure!(
            !inventories.contains_key(&repository.id),
            "duplicate snapshot repository ID {}",
            repository.id
        );
        let in_place = in_place_only_mode(
            runner,
            &repository.checkout,
            &repository.ownership,
            repository.origin_override.as_deref(),
        )?;
        let mode = if in_place {
            CaptureMode::InPlaceOnly
        } else {
            CaptureMode::Transfer(Box::new(
                repository_inventory(runner, repository)
                    .map_err(|error| session_error(&repository.id, error))?,
            ))
        };
        inventories.insert(repository.id.clone(), mode);
    }
    let deletion_roots = session_teardown_roots(runner, repositories)?;
    for repository in repositories {
        if let CaptureMode::Transfer(inventory) = inventories
            .get_mut(&repository.id)
            .context("missing capture inventory")?
        {
            inventory_origin_dependencies(
                runner,
                &repository.checkout,
                inventory,
                repository.origin_override.as_deref(),
                &deletion_roots,
            )?;
            inventory.blockers = inventory_blockers(inventory)?;
        }
    }
    let mut pending_consents = Vec::new();
    for repository in repositories {
        if let CaptureMode::Transfer(inventory) = &inventories[&repository.id] {
            let inputs = check_unconditional_blockers(inventory)
                .map_err(|error| session_error(&repository.id, error))?;
            pending_consents.push((repository, inventory.raw, inputs));
        }
    }
    for (repository, raw, inputs) in pending_consents {
        check_consent_inputs(
            raw,
            inputs,
            consent.repositories.get(&repository.id).unwrap_or(&none),
        )
        .map_err(|error| session_error(&repository.id, error))?;
    }
    let mut snapshots = BTreeMap::new();
    let mut pending_packs = Vec::new();
    for repository in repositories {
        if matches!(inventories[&repository.id], CaptureMode::InPlaceOnly) {
            snapshots.insert(
                repository.id.clone(),
                capture_in_place_metadata(runner, repository)?,
            );
            continue;
        }
        let snapshot = capture_prepared_workspace(
            runner,
            repository,
            consent.repositories.get(&repository.id).unwrap_or(&none),
            &repository_key(&repository.id),
            &mut pending_packs,
        )
        .map_err(|error| session_error(&repository.id, error))?;
        snapshots.insert(repository.id.clone(), snapshot);
    }
    for repository in repositories {
        if matches!(inventories[&repository.id], CaptureMode::InPlaceOnly) {
            continue;
        }
        let mut inventory = repository_inventory(runner, repository)
            .map_err(|error| session_error(&repository.id, error))?;
        inventory_origin_dependencies(
            runner,
            &repository.checkout,
            &mut inventory,
            repository.origin_override.as_deref(),
            &deletion_roots,
        )?;
        inventory.blockers = inventory_blockers(&inventory)?;
        check_repository_consent(
            repository,
            &inventory,
            consent.repositories.get(&repository.id).unwrap_or(&none),
        )
        .map_err(|error| session_error(&repository.id, error))?;
    }
    for pack in &mut pending_packs {
        pack.disable_cleanup(true);
    }
    Ok(SessionSnapshot {
        repositories: snapshots,
    })
}
/// A one-repository session, retaining direct typed workspace refusals.
pub fn capture_workspace_snapshot(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
    pack_path: &Path,
    consent: &CaptureConsent,
    origin_override: Option<String>,
) -> Result<WorkspaceSnapshot> {
    let id = "workspace".to_owned();
    let repository = RepositoryInput {
        in_container: false,
        id: id.clone(),
        checkout: checkout.to_path_buf(),
        ownership: ownership.clone(),
        origin_override,
        pack_path: pack_path.to_path_buf(),
        exclusions: ExclusionSource::Origin,
        keep_dir: None,
    };
    let consent = SessionCaptureConsent {
        repositories: BTreeMap::from([(id.clone(), consent.clone())]),
    };
    let mut session = capture_session_snapshot(runner, &[repository], &consent)?;
    session
        .repositories
        .remove(&id)
        .context("single-repository snapshot was not captured")
}

/// Inventory, consent and all aborts precede the pinned cut. Concurrent ref or
/// HEAD changes retry the complete cut; capture never creates temporary refs.
/// Raw callers load their session's launch record with [`LaunchRecord::read`].
fn owns_git_directory(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
) -> Result<bool> {
    if matches!(ownership, CheckoutOwnership::Raw { .. }) {
        return Ok(false);
    }
    let (_, common) = crate::archive::git::git_directories(runner, checkout)?;
    Ok(fs::canonicalize(common)?.starts_with(fs::canonicalize(checkout)?))
}
fn capture_in_place_metadata(
    runner: &dyn GitCommandRunner,
    repository: &RepositoryInput,
) -> Result<WorkspaceSnapshot> {
    let checkout = &repository.checkout;
    let inventory = in_place_only_inventory(runner, checkout, &repository_key(&repository.id))?;
    let skipped = inventory.operation.is_some();
    let mut notices = inventory.notices;
    let status = run_git(
        runner,
        checkout,
        [
            "status",
            "--porcelain",
            "--untracked-files=no",
            "--ignore-submodules=none",
        ],
        &[],
    );
    let dirty = match status {
        Ok(output) if output.status == 0 => !output.stdout.is_empty(),
        _ => {
            tracing::warn!(checkout = %checkout.display(), "could not inspect raw in-place tracked state; checkout is kept");
            notices
                .push("tracked state could not be inspected; the raw checkout is kept".to_owned());
            true
        }
    };
    let hidden = inventory.hidden_tracked_edits;
    Ok(WorkspaceSnapshot {
        object_format: git_text(runner, checkout, ["rev-parse", "--show-object-format"])?,
        worktree_config_enabled: worktree_config_enabled(runner, checkout)?,
        source_had_origin: false,
        origin_urls: Vec::new(),
        push_urls: Vec::new(),
        push_evidence: PushEvidence::Unavailable {
            reason: "raw snapshot resumes only in place without origin".to_owned(),
        },
        url_rules: Vec::new(),
        worktree_url_rules: Vec::new(),
        notices,
        head: saved_head(runner, checkout, skipped)?,
        refs: BTreeMap::new(),
        refs_complete: false,
        deleted_refs: Vec::new(),
        branch_tracking: BTreeMap::new(),
        stash: Vec::new(),
        working_state: None,
        working_state_skipped: skipped,
        has_uncommitted_state: skipped || dirty || !hidden.is_empty(),
        worktrees: Vec::new(),
        initialized_submodules: Vec::new(),
        index_gitlinks: Vec::new(),
        remote_refs_stale: false,
        pack: None,
    })
}

fn capture_prepared_workspace(
    runner: &dyn GitCommandRunner,
    repository: &RepositoryInput,
    consent: &CaptureConsent,
    key: &CheckoutKey,
    pending_packs: &mut Vec<tempfile::TempPath>,
) -> Result<WorkspaceSnapshot> {
    let checkout = repository.checkout.as_path();
    let ownership = &repository.ownership;
    let pack_path = repository.pack_path.as_path();
    let source = &repository.exclusions;
    // Raw checkouts may have live writers. Retry the complete configuration,
    // remote evidence and ref cut together, using the same stability policy.
    for attempt in 0..3 {
        let result = (|| -> Result<Option<WorkspaceSnapshot>> {
            let origin_configuration = git_bytes(
                runner,
                checkout,
                ["config", "--null", "--list", "--includes"],
                &[],
                "read capture origin configuration",
            )?;
            let origin_override = repository.origin_override.clone();
            let inventory = prepare_capture(runner, repository, consent)?;
            let object_format = git_text(runner, checkout, ["rev-parse", "--show-object-format"])?;
            let worktree_config_enabled =
                crate::snapshot::worktree_config_enabled(runner, checkout)?;
            let configured = run_git(runner, checkout, ["remote", "get-url", "origin"], &[])?;
            let has_origin = configured.status == 0;
            let originals = if has_origin {
                config_values(runner, checkout, "remote.origin.url")?
            } else {
                origin_override.into_iter().collect()
            };
            let origin_urls = originals
                .iter()
                .map(|url| stored_remote(runner, checkout, url))
                .collect::<Result<Vec<_>>>()?;
            let captured_rules = local_url_rules(runner, checkout)?;
            let push_evidence =
                capture_push_evidence(runner, checkout, has_origin, repository.in_container)?;
            let url_rules = captured_rules.rules;
            let captured_worktree_rules = worktree_url_rules(runner, checkout)?;
            let worktree_url_rules = captured_worktree_rules.rules;
            let mut notices = captured_rules.notices;
            notices.extend(captured_worktree_rules.notices);
            notices.extend(inventory.notices.iter().cloned());
            if matches!(ownership, CheckoutOwnership::Managed)
                && !owns_git_directory(runner, checkout, ownership)?
            {
                notices.push("legacy linked-worktree snapshot omits durable review refs; resume starts with a fresh review baseline".to_owned());
            }
            for (path, nested) in &inventory.worktrees {
                notices.extend(
                    nested
                        .notices
                        .iter()
                        .map(|notice| format!("{}: {notice}", path.display())),
                );
            }
            ensure!(
                !origin_urls.is_empty() || matches!(ownership, CheckoutOwnership::Raw { .. }),
                "managed snapshot requires an origin remote or source override"
            );
            if origin_urls.is_empty() {
                return capture_in_place_metadata(runner, repository).map(Some);
            }
            // Anchoring was checked before operations were aborted. Do not let
            // a literal local source override resolve as a named Git remote.
            let literal_override;
            let remote = if has_origin {
                "origin"
            } else {
                literal_override = absolute_config_url(
                    checkout,
                    originals.first().context("missing snapshot origin")?,
                )?;
                &literal_override
            };
            let push_urls = if has_origin {
                config_values(runner, checkout, "remote.origin.pushurl")?
                    .iter()
                    .map(|url| stored_remote(runner, checkout, url))
                    .collect::<Result<Vec<_>>>()?
            } else {
                Vec::new()
            };
            let exclusions =
                snapshot_exclusions(runner, checkout, remote, &BTreeMap::new(), source)?;
            if let Some(notice) = &exclusions.notice {
                notices.push(notice.clone());
            }
            let inventory = prepare_capture(runner, repository, consent)?;
            let skipped = inventory.raw && inventory.operation.is_some();
            let cut = pin_workspace_cut(runner, checkout, ownership)?;
            let owners = branch_owners(&cut);
            let source_refs = &cut.refs;
            let head = &cut.head;
            let original_stash = selected_stash(runner, checkout, ownership)?;
            // This exact list becomes pack tips; a newer stash cannot inherit a
            // previous inventory's consent just because it stays stable afterward.
            check_stash_consent(runner, checkout, &original_stash, &consent.untracked, key)?;
            let working = working_state(runner, checkout, skipped)?;
            let mut worktrees = Vec::new();
            let mut tips = vec![cut.head.commit().to_owned()];
            tips.extend(working.clone());
            let mut stable = true;
            let mut verify_state = |path: &Path, state: &Option<String>, oid: &str| -> Result<()> {
                if let Some(state) = state {
                    stable &= git_text(runner, path, ["rev-parse", &format!("{state}^1")])? == oid;
                }
                Ok(())
            };
            verify_state(checkout, &working, cut.head.commit())?;
            for tree in &cut.worktrees {
                let directory = checkout.join(&tree.relative_path);
                let skipped = inventory.raw
                    && inventory
                        .worktrees
                        .iter()
                        .find(|(path, _)| path == &tree.relative_path)
                        .is_some_and(|(_, tree)| tree.operation.is_some());
                let state = working_state(runner, &directory, skipped)?;
                verify_state(&directory, &state, tree.head.commit())?;
                tips.push(tree.head.commit().to_owned());
                tips.extend(state.clone());
                let own_rules = crate::snapshot::worktree_url_rules(runner, &directory)?;
                notices.extend(
                    own_rules
                        .notices
                        .into_iter()
                        .map(|notice| format!("{}: {notice}", tree.relative_path.display())),
                );
                let has_origin =
                    run_git(runner, &directory, ["remote", "get-url", "origin"], &[])?.status == 0;
                let has_uncommitted_state = state.is_some()
                    || skipped
                    || staged_gitlink_changes(runner, &directory)?
                    || inventory
                        .worktrees
                        .iter()
                        .find(|(path, _)| path == &tree.relative_path)
                        .map(|(_, inventory)| {
                            submodule_uncommitted_state(runner, &directory, inventory)
                        })
                        .transpose()?
                        .unwrap_or(false);
                let saved_head = if let SnapshotHead::Branch { name, commit } = &tree.head {
                    let (owner_commit, owner_path) = owners[name.as_str()];
                    if commit != owner_commit {
                        let owner = if owner_path.as_os_str().is_empty() {
                            "primary checkout".to_owned()
                        } else {
                            format!("worktree {}", owner_path.display())
                        };
                        notices.push(format!("worktree {} saved detached at {commit}: branch {name} belongs to {owner} at {owner_commit}", tree.relative_path.display()));
                        SnapshotHead::Detached(commit.clone())
                    } else {
                        tree.head.clone()
                    }
                } else {
                    tree.head.clone()
                };
                worktrees.push(SnapshotWorktree {
                    push_evidence: capture_push_evidence(
                        runner,
                        &directory,
                        has_origin,
                        repository.in_container,
                    )?,
                    has_uncommitted_state,
                    url_rules: own_rules.rules,

                    relative_path: tree.relative_path.clone(),
                    head: saved_head,
                    working_state: state,
                    working_state_skipped: skipped,
                    index_gitlinks: inventory
                        .worktrees
                        .iter()
                        .find(|(path, _)| path == &tree.relative_path)
                        .map(|(_, tree)| {
                            tree.index_gitlinks
                                .iter()
                                .map(|entry| entry.value.clone())
                                .collect()
                        })
                        .unwrap_or_default(),
                    initialized_submodules: inventory
                        .worktrees
                        .iter()
                        .find(|(path, _)| path == &tree.relative_path)
                        .map(|(_, tree)| {
                            tree.initialized_submodules
                                .iter()
                                .map(|entry| entry.value.clone())
                                .collect()
                        })
                        .unwrap_or_default(),
                });
            }
            stable &= selected_stash(runner, checkout, ownership)? == original_stash;
            // Compare fresh source values, never the branch-owner-normalized output refs.
            stable &= pin_workspace_cut(runner, checkout, ownership)? == cut;
            stable &= same_checkout_cut(&inventory, &repository_inventory(runner, repository)?);
            stable &= origin_configuration
                == git_bytes(
                    runner,
                    checkout,
                    ["config", "--null", "--list", "--includes"],
                    &[],
                    "verify capture origin configuration",
                )?;
            if !stable {
                return Ok(None);
            }
            for (branch, (owner_commit, _)) in &owners {
                if let Some(live) = source_refs
                    .get(*branch)
                    .filter(|live| live.as_str() != *owner_commit)
                {
                    notices.push(format!("branch {branch} saved at {owner_commit}; its live tip {live} is preserved as refs/mj/displaced/{live}"));
                }
            }
            let selected = selected_refs(runner, checkout, &cut, ownership, &owners)?;
            let snapshot_refs = selected.refs;
            let stash = original_stash
                .iter()
                .map(|entry| tracked_stash(runner, checkout, entry))
                .collect::<Result<Vec<_>>>()?;
            tips.extend(snapshot_refs.values().cloned());
            tips.extend(stash.iter().map(|entry| entry.commit.clone()));
            tips.sort();
            tips.dedup();
            if let Some(keep) = keep_relative(repository)? {
                let mut tracked = Vec::new();
                let states = working
                    .iter()
                    .cloned()
                    .chain(stash.iter().map(|entry| entry.commit.clone()))
                    .collect::<Vec<_>>();
                for state in states
                    .iter()
                    .flat_map(|state| [state.clone(), format!("{state}^2")])
                {
                    tracked.extend(tracked_keep_paths(
                        runner,
                        checkout,
                        key,
                        &keep,
                        Some(&state),
                    )?);
                }
                for tree in &worktrees {
                    if let Some(state) = &tree.working_state {
                        let nested_key = CheckoutKey {
                            repository_id: key.repository_id.clone(),
                            checkout_path: tree.relative_path.clone(),
                        };
                        tracked.extend(tracked_keep_paths(
                            runner,
                            &checkout.join(&tree.relative_path),
                            &nested_key,
                            &keep,
                            Some(state),
                        )?);
                    }
                }
                if !tracked.is_empty() {
                    return Err(SnapshotRefused::TrackedKeepFiles {
                        keep_dir: repository.keep_dir.clone().unwrap_or(keep),
                        paths: tracked,
                    }
                    .into());
                }
            }
            let owned_pack =
                write_snapshot_pack(runner, checkout, &tips, &exclusions.objects, pack_path)?;
            let pack = owned_pack.as_ref().map(|path| path.to_path_buf());
            pending_packs.extend(owned_pack);
            check_repository_consent(
                repository,
                &repository_inventory(runner, repository)?,
                consent,
            )?;
            let mut tracking = branch_tracking(runner, checkout, &snapshot_refs, &owners)?;
            let dropped = tracking
                .iter()
                .filter(|(_, tracking)| tracking.remote.as_deref() != Some("origin"))
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            if !dropped.is_empty() {
                notices.push(format!(
                    "upstreams for branches {} were omitted because restore recreates only origin",
                    dropped.join(", ")
                ));
            }
            tracking.retain(|_, tracking| tracking.remote.as_deref() == Some("origin"));
            Ok(Some(WorkspaceSnapshot {
                source_had_origin: has_origin,
                object_format,
                worktree_config_enabled,
                origin_urls,
                push_urls,
                push_evidence,
                url_rules,
                worktree_url_rules,

                notices,
                head: head.clone(),
                refs_complete: selected.complete,
                deleted_refs: match ownership {
                    CheckoutOwnership::Raw {
                        launch: Some(launch),
                    } => launch
                        .refs
                        .keys()
                        .filter(|name| local_snapshot_ref(name) && !source_refs.contains_key(*name))
                        .cloned()
                        .collect(),
                    _ => Vec::new(),
                },
                branch_tracking: tracking,
                refs: snapshot_refs,
                stash,
                has_uncommitted_state: working.is_some()
                    || skipped
                    || staged_gitlink_changes(runner, checkout)?
                    || submodule_uncommitted_state(runner, checkout, &inventory)?,
                working_state: working,
                working_state_skipped: skipped,
                worktrees,
                initialized_submodules: inventory
                    .initialized_submodules
                    .into_iter()
                    .map(|entry| entry.value)
                    .collect(),
                index_gitlinks: inventory
                    .index_gitlinks
                    .into_iter()
                    .map(|entry| entry.value)
                    .collect(),
                remote_refs_stale: exclusions.stale
                    || inventory.remote_refs_stale
                    || inventory
                        .worktrees
                        .iter()
                        .any(|(_, tree)| tree.remote_refs_stale),
                pack,
            }))
        })();
        match result {
            Ok(Some(snapshot)) => return Ok(snapshot),
            Ok(None) => {}
            Err(error) if error.downcast_ref::<CaptureCutChanged>().is_some() => {}
            Err(error) => return Err(error),
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    anyhow::bail!(
        "workspace refs, HEAD, origin configuration or submodules kept changing during snapshot capture; retry after the checkout is idle"
    )
}

fn nested_repository_inventory(root: &Path) -> Result<(u64, FileIdentity)> {
    let mut pending = vec![(root.to_path_buf(), PathBuf::new())];
    let mut identity = blake3::Hasher::new();
    let mut bytes = 0_u64;
    while let Some((path, relative)) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect nested repository {}", path.display()))?;
        let encoded = relative.as_os_str().as_encoded_bytes();
        identity.update(&(encoded.len() as u64).to_be_bytes());
        identity.update(encoded);
        identity.update(file_identity(&metadata)?.0.as_bytes());
        if metadata.is_dir() {
            let mut children = fs::read_dir(&path)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()?;
            children.sort();
            for child in children.into_iter().rev() {
                let name = child.file_name().context("nested path has no name")?;
                pending.push((child.clone(), relative.join(name)));
            }
        } else {
            bytes = bytes
                .checked_add(metadata.len())
                .context("nested repository byte size overflow")?;
        }
    }
    Ok((
        bytes,
        FileIdentity(identity.finalize().to_hex().to_string()),
    ))
}
fn keep_relative(repository: &RepositoryInput) -> Result<Option<PathBuf>> {
    let Some(keep) = &repository.keep_dir else {
        return Ok(None);
    };
    let root = std::path::absolute(&repository.checkout)?;
    let keep = std::path::absolute(root.join(keep))?;
    let relative = keep
        .strip_prefix(&root)
        .context("keep directory is outside its repository")?
        .to_path_buf();
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
        "keep directory must be a nonempty path inside its repository"
    );
    Ok(Some(relative))
}
fn tracked_keep_paths(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    key: &CheckoutKey,
    keep: &Path,
    state: Option<&str>,
) -> Result<Vec<Scoped<PathBuf>>> {
    let bytes = if let Some(state) = state {
        git_bytes(
            runner,
            checkout,
            [
                "ls-tree",
                "-r",
                "-z",
                "--name-only",
                &format!("{state}^{{tree}}"),
            ],
            &[],
            "inspect keep files in working state",
        )?
    } else {
        git_bytes(
            runner,
            checkout,
            ["ls-files", "-z"],
            &[],
            "inspect tracked keep files",
        )?
    };
    let mut paths = Vec::new();
    for path in bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = mj_core::path_input::from_git_bytes(path)?;
        if key.checkout_path.join(&path).starts_with(keep) {
            paths.push(scoped(key, path));
        }
    }
    paths.sort_by(|left, right| left.value.cmp(&right.value));
    paths.dedup();
    Ok(paths)
}
fn repository_inventory(
    runner: &dyn GitCommandRunner,
    repository: &RepositoryInput,
) -> Result<WorkspaceInventory> {
    let mut inventory = workspace_inventory_scoped(
        runner,
        &repository.checkout,
        &repository.ownership,
        &repository_key(&repository.id),
        repository.origin_override.as_deref(),
    )?;
    if inventory.in_place_only {
        return Ok(inventory);
    }
    if let Some(keep) = keep_relative(repository)? {
        inventory.keep_dir = repository.keep_dir.clone();
        inventory.tracked_keep_files = tracked_keep_paths(
            runner,
            &repository.checkout,
            &inventory.checkout,
            &keep,
            None,
        )?;
        inventory.tracked_keep_files.extend(tracked_keep_paths(
            runner,
            &repository.checkout,
            &inventory.checkout,
            &keep,
            Some("HEAD"),
        )?);
        inventory
            .tracked_keep_files
            .sort_by(|left, right| left.value.cmp(&right.value));
        inventory.tracked_keep_files.dedup();
        for (path, nested) in &mut inventory.worktrees {
            nested.keep_dir = repository.keep_dir.clone();
            nested.tracked_keep_files = tracked_keep_paths(
                runner,
                &repository.checkout.join(path.as_path()),
                &nested.checkout,
                &keep,
                None,
            )?;
            nested.tracked_keep_files.extend(tracked_keep_paths(
                runner,
                &repository.checkout.join(path.as_path()),
                &nested.checkout,
                &keep,
                Some("HEAD"),
            )?);
            nested
                .tracked_keep_files
                .sort_by(|left, right| left.value.cmp(&right.value));
            nested.tracked_keep_files.dedup();
        }
    }
    inventory.blockers = inventory_blockers(&inventory)?;
    Ok(inventory)
}
pub fn workspace_inventory(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
) -> Result<WorkspaceInventory> {
    let capture = CaptureGit::new(runner);
    let runner: &dyn GitCommandRunner = &capture;
    workspace_inventory_scoped(runner, checkout, ownership, &CheckoutKey::default(), None)
}
pub fn session_inventory(
    runner: &dyn GitCommandRunner,
    repositories: &[RepositoryInput],
) -> Result<BTreeMap<String, WorkspaceInventory>> {
    let capture = CaptureGit::new(runner);
    let runner: &dyn GitCommandRunner = &capture;
    let mut result = BTreeMap::new();
    for repository in repositories {
        ensure!(
            !result.contains_key(&repository.id),
            "duplicate snapshot repository ID {}",
            repository.id
        );
        result.insert(
            repository.id.clone(),
            repository_inventory(runner, repository)?,
        );
    }
    let deletion_roots = session_teardown_roots(runner, repositories)?;
    for repository in repositories {
        let inventory = result
            .get_mut(&repository.id)
            .context("missing session inventory")?;
        inventory_origin_dependencies(
            runner,
            &repository.checkout,
            inventory,
            repository.origin_override.as_deref(),
            &deletion_roots,
        )?;
        inventory.blockers = inventory_blockers(inventory)?;
    }
    Ok(result)
}
fn modules_after_abort(
    runner: &dyn GitCommandRunner,
    root: &Path,
    inventory: &WorkspaceInventory,
) -> Result<Vec<PathBuf>> {
    let Some(operation) = inventory.operation.as_ref() else {
        return Ok(inventory
            .initialized_submodules
            .iter()
            .map(|module| root.join(&module.value.path))
            .collect());
    };
    let target = if operation.kind == GitOperationKind::Am {
        git_text(runner, root, ["rev-parse", "ORIG_HEAD"])?
    } else {
        saved_head(runner, root, true)?.commit().to_owned()
    };
    let mut config_commit = target.clone();
    let markers: &[&str] = match operation.kind {
        GitOperationKind::Merge => &["MERGE_AUTOSTASH"],
        GitOperationKind::Rebase => &["rebase-merge/autostash", "rebase-apply/autostash"],
        _ => &[],
    };
    for marker in markers {
        match fs::read_to_string(git_path(runner, root, marker)?) {
            Ok(oid) => {
                config_commit = oid.trim().to_owned();
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("read abort autostash"),
        }
    }
    let registered =
        crate::checkpoint::registered_gitlinks_at(runner, root, &target, &config_commit)?;
    let mut paths = Vec::new();
    for module in &inventory.initialized_submodules {
        let outer = inventory
            .initialized_submodules
            .iter()
            .filter(|candidate| module.value.path.starts_with(&candidate.value.path))
            .min_by_key(|candidate| candidate.value.path.components().count())
            .context("module has no owning checkout")?;
        if registered.contains(&outer.value.path) {
            paths.push(root.join(&module.value.path));
        }
    }
    Ok(paths)
}

fn teardown_roots(runner: &dyn GitCommandRunner, checkout: &Path) -> Result<Vec<PathBuf>> {
    Ok(vec![
        fs::canonicalize(checkout)?,
        fs::canonicalize(crate::archive::git::git_directories(runner, checkout)?.0)?,
    ])
}
fn session_teardown_roots(
    runner: &dyn GitCommandRunner,
    repositories: &[RepositoryInput],
) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    for repository in repositories {
        if matches!(repository.ownership, CheckoutOwnership::Managed) {
            roots.extend(teardown_roots(runner, &repository.checkout)?);
        }
    }
    Ok(roots)
}
fn local_file_origin_route(address: &str) -> Result<PathBuf> {
    // Git decodes the entire file URL path literally, including dot segments,
    // question marks and backslashes on Unix. Url::parse normalizes those.
    #[cfg(windows)]
    let address = address.replace('\\', "/");
    let after_scheme = address
        .split_once("://")
        .context("invalid local file origin")?
        .1;
    let (authority, path) = after_scheme
        .split_once('/')
        .context("local file origin has no path")?;
    let bytes = percent_encoding::percent_decode_str(&format!("/{path}")).collect::<Vec<_>>();
    ensure!(
        !bytes.contains(&0),
        "file origin contains an invalid NUL path byte"
    );
    #[cfg(windows)]
    {
        let path = std::str::from_utf8(&bytes).context("local file origin path is not UTF-8")?;
        let drive = |value: &str| {
            value.len() >= 2
                && value.as_bytes()[0].is_ascii_alphabetic()
                && value.as_bytes()[1] == b':'
        };
        let native = if drive(authority) {
            format!("{authority}{path}")
        } else if path.starts_with('/') && drive(&path[1..]) {
            path[1..].to_owned()
        } else if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
            format!("//{authority}{path}")
        } else {
            path.to_owned()
        };
        Ok(PathBuf::from(native))
    }
    #[cfg(not(windows))]
    {
        let _ = authority;
        mj_core::path_input::from_git_bytes(&bytes)
    }
}

fn origin_route_is_removed(
    route: &Path,
    roots: &[PathBuf],
    inspected: &mut std::collections::BTreeSet<PathBuf>,
) -> Result<bool> {
    let route = std::path::absolute(route)?;
    if !inspected.insert(route.clone()) {
        return Ok(false);
    }
    let mut prefix = PathBuf::new();
    for component in route.components() {
        prefix.push(component.as_os_str());
        match fs::canonicalize(&prefix) {
            Ok(path) if roots.iter().any(|root| path.starts_with(root)) => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("resolve origin route component"),
        }
        match fs::symlink_metadata(&prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = fs::read_link(&prefix)?;
                let target = prefix
                    .parent()
                    .context("origin symlink has no parent")?
                    .join(target);
                if origin_route_is_removed(&target, roots, inspected)? {
                    return Ok(true);
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect origin route component"),
        }
    }
    Ok(false)
}

pub(crate) fn local_origin_route(repository: &Path, text: &str) -> Result<Option<PathBuf>> {
    let (transport, address) = transport_url(text);
    if !transport.is_empty()
        && transport
            .split("::")
            .filter(|part| !part.is_empty())
            .any(|part| part != "file")
    {
        return Ok(None);
    }
    if address
        .split_once("://")
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("file"))
    {
        Ok(Some(local_file_origin_route(address)?))
    } else if Path::new(address).is_absolute() || relative_local_remote(address) {
        Ok(Some(repository.join(address)))
    } else {
        Ok(None)
    }
}

fn teardown_path_dependency(absolute: &Path, roots: &[PathBuf]) -> Result<Option<(PathBuf, bool)>> {
    let mut cursor = absolute;
    let mut missing = Vec::new();
    let mut canonical = loop {
        match fs::canonicalize(cursor) {
            Ok(path) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    cursor
                        .components()
                        .next_back()
                        .context("local origin has no component")?
                        .as_os_str()
                        .to_owned(),
                );
                cursor = cursor
                    .parent()
                    .context("local origin has no existing ancestor")?;
            }
            Err(error) => return Err(error).context("resolve local origin dependency"),
        }
    };
    for component in missing.into_iter().rev() {
        if component == ".." {
            canonical.pop();
        } else if component != "." {
            canonical.push(component);
        }
    }
    let target_is_removed = roots.iter().any(|root| canonical.starts_with(root));
    if target_is_removed || origin_route_is_removed(absolute, roots, &mut Default::default())? {
        canonical
            .to_str()
            .context("origin dependency must be UTF-8; rename it before suspending")?;
        Ok(Some((canonical, target_is_removed)))
    } else {
        Ok(None)
    }
}

/// Native Git expands recursive object providers. Retain literal alternates
/// routes too: canonical provider names alone hide symlinks through teardown.
enum LocalOriginProviders {
    Unavailable(String),
    Ready(Vec<PathBuf>),
}

fn local_origin_object_providers(
    runner: &dyn GitCommandRunner,
    context: &Path,
    origin: &Path,
) -> Result<LocalOriginProviders> {
    let marker = if origin.is_dir() && origin.join(".git").try_exists()? {
        origin.join(".git")
    } else {
        origin.to_path_buf()
    };
    let output = runner.run(
        context,
        &GitCommand {
            arguments: vec![
                "rev-parse".into(),
                "--resolve-git-dir".into(),
                marker.as_os_str().to_owned(),
            ],
            ..Default::default()
        },
    )?;
    if output.status != 0 {
        return Ok(LocalOriginProviders::Unavailable(format!(
            "{:#}",
            git_failure("resolve local origin Git directory", &output)
        )));
    }
    let private = crate::archive::git::absolute_git_path(context, &output.stdout)?;
    let query = |args: &[&str]| -> Result<Vec<u8>> {
        let mut arguments = vec!["--git-dir".into(), private.as_os_str().to_owned()];
        arguments.extend(args.iter().map(OsString::from));
        let output = runner.run(
            context,
            &GitCommand {
                arguments,
                ..Default::default()
            },
        )?;
        ensure!(
            output.status == 0,
            "{}",
            git_failure("inspect local origin object providers", &output)
        );
        Ok(output.stdout)
    };
    let common = crate::archive::git::absolute_git_path(
        context,
        &query(&["rev-parse", "--git-common-dir"])?,
    )?;
    let objects = crate::archive::git::absolute_git_path(
        context,
        &query(&["rev-parse", "--git-path", "objects"])?,
    )?;
    let listing = query(&["count-objects", "-v"])?;
    let mut object_directories = std::collections::BTreeSet::from([objects.clone()]);
    for line in listing.split(|byte| *byte == b'\n') {
        if let Some(path) = line.strip_prefix(b"alternate: ") {
            object_directories.insert(context.join(crate::archive::git::unquote_git_path(path)?));
        }
    }
    let mut providers =
        std::collections::BTreeSet::from([marker, private.clone(), common, objects]);
    if private.join("commondir").try_exists()? {
        providers.insert(private.join("commondir"));
    }
    for directory in object_directories {
        providers.insert(directory.clone());
        let alternates = directory.join("info/alternates");
        let entries = match fs::read(&alternates) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("read local origin alternates routes"),
        };
        providers.insert(alternates.clone());
        for path in entries
            .split(|byte| *byte == b'\n')
            .filter(|path| !path.is_empty())
        {
            providers.insert(directory.join(
                crate::archive::git::unquote_git_path(path).with_context(|| {
                    format!("invalid alternate route in {}", alternates.display())
                })?,
            ));
        }
    }
    Ok(LocalOriginProviders::Ready(providers.into_iter().collect()))
}

fn inventory_origin_dependencies(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    inventory: &mut WorkspaceInventory,
    origin_override: Option<&str>,
    roots: &[PathBuf],
) -> Result<()> {
    inventory.origins_removed_by_teardown.clear();
    if inventory.raw {
        return Ok(());
    }
    let mut repositories = vec![(
        checkout.to_path_buf(),
        inventory.checkout.clone(),
        origin_override,
    )];
    for (path, tree) in std::iter::once((Path::new(""), &*inventory)).chain(
        inventory
            .worktrees
            .iter()
            .map(|(path, tree)| (path.as_path(), tree)),
    ) {
        for module in &tree.initialized_submodules {
            let key = CheckoutKey {
                repository_id: tree.checkout.repository_id.clone(),
                checkout_path: tree.checkout.checkout_path.join(&module.value.path),
            };
            repositories.push((checkout.join(path).join(&module.value.path), key, None));
        }
    }
    let mut candidates = Vec::new();
    for (_, tree) in std::iter::once((Path::new(""), &*inventory)).chain(
        inventory
            .worktrees
            .iter()
            .map(|(path, tree)| (path.as_path(), tree)),
    ) {
        for origin in &tree.uninitialized_origins {
            candidates.push((
                CheckoutKey {
                    repository_id: tree.checkout.repository_id.clone(),
                    checkout_path: tree.checkout.checkout_path.join(&origin.owner),
                },
                Some(origin.config_key.clone()),
                origin.route.clone(),
            ));
        }
    }
    for (repository, key, override_url) in repositories {
        let mut output = run_git(runner, &repository, ["remote", "get-url", "origin"], &[])?;
        if output.status != 0 {
            let Some(override_url) = override_url else {
                continue;
            };
            let literal_override = absolute_config_url(&repository, override_url)?;
            output = run_git(
                runner,
                &repository,
                ["ls-remote", "--get-url", &literal_override],
                &[],
            )?;
        }
        ensure!(
            output.status == 0,
            "{}",
            git_failure("resolve effective origin", &output)
        );
        let text =
            std::str::from_utf8(output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout))?;
        if let Some(local) = local_origin_route(&repository, text)? {
            candidates.push((key, None, local));
        }
    }
    let mut provider_cache = BTreeMap::new();
    for (key, module_url_key, local) in candidates {
        let absolute = std::path::absolute(&local)?;
        if let Some((canonical, target_is_removed)) = teardown_path_dependency(&absolute, roots)? {
            inventory.origins_removed_by_teardown.push(scoped(
                &key,
                OriginDependency {
                    module_url_key,
                    object_provider: None,
                    route: absolute,
                    canonical_path: canonical,
                    target_is_removed,
                },
            ));
            continue;
        }
        if !absolute.try_exists()? {
            continue;
        }
        let providers = match provider_cache.entry(absolute.clone()) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let providers = match local_origin_object_providers(runner, checkout, &absolute).with_context(|| format!("cannot inspect local origin object storage at {}; repair its objects/info/alternates, or make it independent with `git -C {} repack -a -d` and remove its alternates file, then retry", absolute.display(), mj_core::targets::posix_quote(&absolute.to_string_lossy())))? {
                    LocalOriginProviders::Ready(providers) => providers,
                    LocalOriginProviders::Unavailable(error) => {
                        // Only an absent/unresolvable repository uses the
                        // existing offline policy. Errors after resolution
                        // must never masquerade as an empty provider set.
                        inventory.notices.push(format!("could not inspect local origin object storage at {}: {error}", absolute.display()));
                        Vec::new()
                    }
                };
                entry.insert(providers)
            }
        };
        for provider in providers {
            if teardown_path_dependency(provider, roots)?.is_some() {
                inventory.origins_removed_by_teardown.push(scoped(
                    &key,
                    OriginDependency {
                        module_url_key: module_url_key.clone(),
                        object_provider: Some(provider.clone()),
                        route: absolute.clone(),
                        canonical_path: fs::canonicalize(&absolute)?,
                        target_is_removed: false,
                    },
                ));
            }
        }
    }
    Ok(())
}

fn workspace_inventory_scoped(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    ownership: &CheckoutOwnership,
    key: &CheckoutKey,
    origin_override: Option<&str>,
) -> Result<WorkspaceInventory> {
    if in_place_only_mode(runner, checkout, ownership, origin_override)? {
        return in_place_only_inventory(runner, checkout, key);
    }
    if origin_override.is_some() && config_values(runner, checkout, "remote.origin.url")?.is_empty()
    {
        // Validate the caller's original spelling before deriving the anchored
        // origin used solely as the relative-submodule base below.
        validate_origin_rewrite_anchoring(runner, checkout, origin_override)?;
    }
    fn collect(
        runner: &dyn GitCommandRunner,
        checkout: &Path,
        raw: bool,
        nested: &[PathBuf],
        key: &CheckoutKey,
        origin_override: Option<&str>,
    ) -> Result<WorkspaceInventory> {
        let bytes = git_bytes(
            runner,
            checkout,
            ["ls-files", "--others", "--exclude-standard", "-z"],
            &[],
            "list untracked files",
        )?;
        let mut submodules = crate::checkpoint::inspect_snapshot_submodules(
            runner,
            checkout,
            origin_override,
            !raw,
        )?;
        for path in submodules
            .index_gitlinks
            .iter()
            .map(|link| &link.path)
            .chain(submodules.initialized.iter().map(|module| &module.path))
        {
            crate::archive::slash_path(path).with_context(|| {
                format!(
                    "snapshot paths must be UTF-8; rename {} before suspending",
                    path.display()
                )
            })?;
        }
        if let Some(notice) = worktree_remote_notice(runner, checkout)? {
            submodules.notices.push(notice);
        }
        if raw && !submodules.local_work.is_empty() {
            submodules.notices.push(format!("{}; submodule local refs, stashes, staged index and operation state do not travel with this snapshot; the raw checkout is kept", submodules.local_work.iter().map(|work| work.description.as_str()).collect::<Vec<_>>().join(", ")));
        }
        if raw {
            submodules.notices.extend(submodules.operations.iter().map(|(path, kind)|
                format!("submodule {} has {kind:?} in progress; its operation state does not travel with the snapshot; the raw checkout is kept", path.display())));
        }
        let mut paths = bytes
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(mj_core::path_input::from_git_bytes)
            .collect::<Result<Vec<_>>>()?;
        paths.extend(submodules.untracked);
        paths.sort();
        paths.dedup();
        let mut untracked = Vec::new();
        let mut nested_repositories = Vec::new();
        for path in &submodules.unregistered {
            if nested.iter().any(|nested| path.starts_with(nested)) {
                continue;
            }
            let directory = checkout.join(path);
            if directory.try_exists()? {
                let (bytes, identity) = nested_repository_inventory(&directory)?;
                nested_repositories.push(UntrackedPath {
                    checkout: key.clone(),
                    path: path.clone(),
                    bytes,
                    identity,
                });
            }
        }
        let mut intent = intent_to_add_paths(runner, checkout)?
            .into_iter()
            .map(|path| scoped(key, path))
            .collect::<Vec<_>>();
        intent.extend(
            submodules
                .intent_to_add
                .into_iter()
                .map(|(module, path)| Scoped {
                    checkout: CheckoutKey {
                        repository_id: key.repository_id.clone(),
                        checkout_path: key.checkout_path.join(module),
                    },
                    value: path,
                }),
        );
        let mut hidden = crate::archive::git::hidden_tracked_paths(runner, checkout)?;
        hidden.extend(submodules.hidden_tracked_edits);
        for path in paths {
            if nested.iter().any(|nested| path.starts_with(nested))
                || nested_repositories
                    .iter()
                    .any(|repository| path.starts_with(&repository.path))
            {
                continue;
            }
            let metadata = fs::symlink_metadata(checkout.join(&path))
                .with_context(|| format!("inspect {}", path.display()))?;
            let (bytes, identity) = if metadata.is_dir() {
                // Git collapses an embedded untracked repository to one path.
                // Consent must cover its files, not just the directory inode.
                nested_repository_inventory(&checkout.join(&path))?
            } else {
                (metadata.len(), file_identity(&metadata)?)
            };
            untracked.push(UntrackedPath {
                checkout: key.clone(),
                identity,
                path,
                bytes,
            });
        }
        Ok(WorkspaceInventory {
            notices: submodules.notices,
            checkout: key.clone(),
            untracked,
            untracked_stashes: Vec::new(),
            nested_repositories,
            tracked_keep_files: Vec::new(),
            keep_dir: None,
            untracked_submodule_registration: submodules
                .untracked_registration
                .into_iter()
                .map(|path| scoped(key, path))
                .collect(),
            dirty_submodules: submodules
                .dirty
                .into_iter()
                .map(|path| scoped(key, path))
                .collect(),
            initialized_submodules: submodules
                .initialized
                .into_iter()
                .map(|value| scoped(key, value))
                .collect(),
            index_gitlinks: submodules
                .index_gitlinks
                .into_iter()
                .map(|value| scoped(key, value))
                .collect(),
            unrepresented_worktrees: Vec::new(),
            unrepresented_git_dirs: Vec::new(),
            origins_removed_by_teardown: Vec::new(),
            uninitialized_origins: submodules.uninitialized_origins,
            blockers: Vec::new(),
            in_place_only: false,
            origin_missing: origin_override.is_none()
                && run_git(runner, checkout, ["remote", "get-url", "origin"], &[])?.status != 0,
            shallow: git_text(runner, checkout, ["rev-parse", "--is-shallow-repository"])?
                == "true",
            submodule_operations: submodules
                .operations
                .into_iter()
                .map(|(path, kind)| Scoped {
                    checkout: CheckoutKey {
                        repository_id: key.repository_id.clone(),
                        checkout_path: key.checkout_path.join(path),
                    },
                    value: kind,
                })
                .collect(),
            submodule_local_work: submodules
                .local_work
                .into_iter()
                .map(|work| scoped(key, work))
                .collect(),
            intent_to_add: intent,
            hidden_tracked_edits: hidden.into_iter().map(|path| scoped(key, path)).collect(),
            operation: operation_record(runner, checkout, key)?,
            raw,
            remote_refs_stale: submodules.remote_refs_stale,
            worktrees: Vec::new(),
        })
    }
    // Restore shares the primary checkout's anchored origin with its linked
    // worktrees, even when their source config contains a relative URL.
    let primary_origin = config_values(runner, checkout, "remote.origin.url")?;
    let restore_origin = primary_origin
        .last()
        .map(String::as_str)
        .or(origin_override)
        .map(|origin| absolute_config_url(checkout, origin))
        .transpose()?;
    let origin_override = restore_origin.as_deref();
    let nested = discover_linked_worktrees(runner, checkout)?;
    let paths: Vec<_> = nested.iter().map(|tree| tree.relative.clone()).collect();
    for path in &paths {
        crate::archive::slash_path(path).with_context(|| {
            format!(
                "snapshot paths must be UTF-8; rename {} before suspending",
                path.display()
            )
        })?;
    }
    let raw = matches!(ownership, CheckoutOwnership::Raw { .. });
    let mut result = collect(runner, checkout, raw, &paths, key, origin_override)?;
    for entry in selected_stash(runner, checkout, ownership)? {
        if stash_has_untracked(runner, checkout, &entry.commit)? {
            result.untracked_stashes.push(scoped(key, entry));
        }
    }
    for tree in nested {
        let children: Vec<_> = paths
            .iter()
            .filter_map(|path| path.strip_prefix(&tree.relative).ok())
            .filter(|path| !path.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .collect();
        let nested_key = CheckoutKey {
            repository_id: key.repository_id.clone(),
            checkout_path: key.checkout_path.join(&tree.relative),
        };
        result.worktrees.push((
            tree.relative,
            collect(
                runner,
                &tree.directory,
                raw,
                &children,
                &nested_key,
                origin_override,
            )?,
        ));
    }
    let mut roots = vec![checkout.to_path_buf()];
    let mut modules = result
        .initialized_submodules
        .iter()
        .map(|module| checkout.join(&module.value.path))
        .collect::<Vec<_>>();
    for (path, tree) in &result.worktrees {
        let root = checkout.join(path);
        roots.push(root.clone());
        modules.extend(
            tree.initialized_submodules
                .iter()
                .map(|module| root.join(&module.value.path)),
        );
    }
    if !raw {
        inventory_origin_dependencies(
            runner,
            checkout,
            &mut result,
            origin_override,
            &teardown_roots(runner, checkout)?,
        )?;
    }
    let structure =
        crate::checkpoint::inspect_snapshot_git_structure(runner, checkout, &roots, &modules)?;
    result.unrepresented_git_dirs = structure
        .directories
        .into_iter()
        .map(|path| scoped(key, path))
        .collect();
    result.unrepresented_worktrees = structure
        .worktrees
        .into_iter()
        .map(|value| scoped(key, value))
        .collect();
    if !raw
        && (result.operation.is_some()
            || result
                .worktrees
                .iter()
                .any(|(_, tree)| tree.operation.is_some()))
    {
        let mut predicted_modules = modules_after_abort(runner, checkout, &result)?;
        for (path, tree) in &result.worktrees {
            predicted_modules.extend(modules_after_abort(runner, &checkout.join(path), tree)?);
        }
        let predicted = crate::checkpoint::inspect_snapshot_git_structure(
            runner,
            checkout,
            &roots,
            &predicted_modules,
        )?;
        for path in predicted.directories {
            if !result
                .unrepresented_git_dirs
                .iter()
                .any(|entry| entry.value == path)
            {
                result.unrepresented_git_dirs.push(scoped(key, path));
            }
        }
        for value in predicted.worktrees {
            if !result
                .unrepresented_worktrees
                .iter()
                .any(|entry| entry.value == value)
            {
                result.unrepresented_worktrees.push(scoped(key, value));
            }
        }
    }
    if raw
        && (!result.unrepresented_git_dirs.is_empty() || !result.unrepresented_worktrees.is_empty())
    {
        let paths = result
            .unrepresented_git_dirs
            .iter()
            .map(|entry| entry.value.display().to_string())
            .chain(
                result
                    .unrepresented_worktrees
                    .iter()
                    .map(|entry| entry.value.path.display().to_string()),
            )
            .collect::<Vec<_>>();
        result.notices.push(format!("unrepresented Git state at {}; these paths do not travel with the snapshot; the raw checkout is kept", paths.join(", ")));
    }
    if raw && result.shallow {
        result.notices.push("raw repository is shallow; snapshots do not preserve shallow metadata; the source checkout is kept".to_owned());
    }
    result.blockers = inventory_blockers(&result)?;
    Ok(result)
}

fn require_object(
    runner: &dyn GitCommandRunner,
    destination: &Path,
    name: &str,
    oid: &str,
) -> Result<()> {
    let output = run_git(
        runner,
        destination,
        ["cat-file", "-e", &format!("{oid}^{{object}}")],
        &[],
    )?;
    ensure!(
        output.status == 0,
        "snapshot object {oid} for {name} is missing; origin may have rewritten history"
    );
    Ok(())
}
fn same_checkout_cut(left: &WorkspaceInventory, right: &WorkspaceInventory) -> bool {
    left.operation == right.operation
        && left.index_gitlinks == right.index_gitlinks
        && left.untracked_submodule_registration == right.untracked_submodule_registration
        && left.initialized_submodules == right.initialized_submodules
        && left.submodule_local_work == right.submodule_local_work
        && left.submodule_operations == right.submodule_operations
        && left.unrepresented_worktrees == right.unrepresented_worktrees
        && left.unrepresented_git_dirs == right.unrepresented_git_dirs
        && left.shallow == right.shallow
        && left.origin_missing == right.origin_missing
        && left.uninitialized_origins == right.uninitialized_origins
        && left.intent_to_add == right.intent_to_add
        && left.worktrees.len() == right.worktrees.len()
        && left.worktrees.iter().zip(&right.worktrees).all(
            |((left_path, left), (right_path, right))| {
                left_path == right_path && same_checkout_cut(left, right)
            },
        )
}
fn checkout_head(
    runner: &dyn GitCommandRunner,
    destination: &Path,
    head: &SnapshotHead,
) -> Result<()> {
    let oid = head.commit();
    require_object(runner, destination, "HEAD", oid)?;
    match head {
        SnapshotHead::Branch { name, .. } => {
            ensure!(
                name.starts_with("refs/heads/"),
                "saved HEAD is not a branch"
            );
            git_text(runner, destination, ["symbolic-ref", "HEAD", name])?;
        }
        SnapshotHead::Detached(_) => {
            git_text(
                runner,
                destination,
                ["update-ref", "--no-deref", "HEAD", oid],
            )?;
        }
    }
    git_text(
        runner,
        destination,
        ["reset", "--hard", "--no-recurse-submodules", oid],
    )?;
    Ok(())
}
/// Native graph walks treat shallow boundaries as roots, hiding missing parents.
fn require_full_restore_history(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    after_fetch: bool,
) -> Result<()> {
    if git_text(runner, repository, ["rev-parse", "--is-shallow-repository"])? == "true" {
        let boundary = git_path(runner, repository, "shallow")?;
        let commits = fs::read_to_string(&boundary)
            .with_context(|| format!("read shallow boundary {}", boundary.display()))?;
        let remedy = if after_fetch {
            "origin bootstrap returned truncated ancestry; use an origin with full ancestry, or provision full ancestry up to a history cutoff without fetching past it, then retry".to_owned()
        } else {
            // A cached module Git directory survives worktree rollback. The
            // command must still name the right repository after attachment fails.
            let (admin, _) = crate::archive::git::git_directories(runner, repository)?;
            format!(
                "run `git --git-dir {} fetch --unshallow origin` using an origin with full ancestry, or, for a history-cutoff workspace, provision full ancestry up to the cutoff without fetching past it, then retry",
                mj_core::targets::posix_quote(&admin.to_string_lossy())
            )
        };
        anyhow::bail!(
            "snapshot restore repository {} is shallow at {} ({}); {remedy}",
            repository.display(),
            boundary.display(),
            commits.trim().replace('\n', ", ")
        );
    }
    Ok(())
}

/// Provisioning grants access to this worktree, not an enclosing checkout or
/// a bare object store. Validate its root and ancestry before the first write.
fn validate_restore_worktree_root(runner: &dyn GitCommandRunner, directory: &Path) -> Result<()> {
    let inside = run_git(
        runner,
        directory,
        ["rev-parse", "--is-inside-work-tree"],
        &[],
    )?;
    ensure!(
        inside.status == 0
            && inside.stdout.strip_suffix(b"\n").unwrap_or(&inside.stdout) == b"true",
        "provisioned restore destination {} must be an exact Git worktree root",
        directory.display()
    );
    let root = git_bytes(
        runner,
        directory,
        ["rev-parse", "--show-toplevel"],
        &[],
        "locate provisioned worktree root",
    )?;
    let root = mj_core::path_input::from_git_bytes(root.strip_suffix(b"\n").unwrap_or(&root))?;
    ensure!(
        fs::canonicalize(directory)? == fs::canonicalize(&root)?,
        "provisioned restore destination {} is not its Git worktree root {}; supply that worktree root",
        directory.display(),
        root.display()
    );
    require_full_restore_history(runner, directory, false)
}

fn validate_restored_module_admin(
    runner: &dyn GitCommandRunner,
    owner: &Path,
    directory: &Path,
    name: &str,
) -> Result<PathBuf> {
    validate_restore_worktree_root(runner, directory)?;
    crate::archive::slash_path(Path::new(name))?;
    let modules = git_path(runner, owner, "modules")?;
    crate::archive::ensure_no_symlink_ancestors(
        modules.parent().context("module store has no parent")?,
        &Path::new("modules").join(name),
    )?;
    let expected = modules.join(name);
    let (private, common) = crate::archive::git::git_directories(runner, directory)?;
    let private = fs::canonicalize(private)?;
    let common = fs::canonicalize(common)?;
    let checkout = fs::canonicalize(directory)?;
    let standalone = private.starts_with(&checkout) && common.starts_with(&checkout);
    let absorbed = expected.try_exists()? && fs::symlink_metadata(&expected)?.is_dir() && {
        let expected = fs::canonicalize(expected)?;
        common == expected && private.starts_with(&expected)
    };
    ensure!(
        standalone || absorbed,
        "submodule {} points outside its destination-owned Git directories ({} and {}); provision an independent submodule checkout here and retry",
        directory.display(),
        private.display(),
        common.display()
    );
    Ok(private)
}

/// Own the displaced destination until every step for this module succeeds.
/// On failure put it back, even when initialization already created a checkout.
struct SubmoduleRestoreOverlay {
    backup: Option<tempfile::TempDir>,
    destination: PathBuf,
    moved: Vec<(PathBuf, PathBuf)>,
}
impl SubmoduleRestoreOverlay {
    fn take(directory: &Path) -> Result<Option<Self>> {
        if (directory.is_dir() && directory.join(".git").try_exists()?)
            || !directory.try_exists()?
        {
            return Ok(None);
        }
        let backup = tempfile::tempdir_in(directory.parent().context("module has no parent")?)?;
        fs::rename(directory, backup.path().join("working"))
            .context("preserve provisioned module files")?;
        Ok(Some(Self {
            backup: Some(backup),
            destination: directory.to_path_buf(),
            moved: Vec::new(),
        }))
    }
    fn restore_files(&mut self, relative: &Path, index: &[u8]) -> Result<()> {
        let mut overlays = std::collections::BTreeSet::new();
        for entry in index
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty() && !entry.starts_with(b"160000 "))
        {
            let tab = entry
                .iter()
                .position(|byte| *byte == b'\t')
                .context("invalid saved overlay entry")?;
            let path = mj_core::path_input::from_git_bytes(&entry[tab + 1..])?;
            overlays.insert(
                path.strip_prefix(relative)
                    .context("overlay is outside module")?
                    .to_path_buf(),
            );
        }
        let working = self
            .backup
            .as_ref()
            .context("missing module backup")?
            .path()
            .join("working");
        if !fs::symlink_metadata(&working)?.is_dir() {
            // stash apply can materialize a staged file replacing this gitlink;
            // the saved initialized checkout owns its working directory.
            ensure!(
                overlays.contains(Path::new("")),
                "provisioned file {} collides with the restored module; move it and retry",
                self.destination.display()
            );
            return Ok(());
        }
        let mut directories = Vec::new();
        let mut files = Vec::new();
        let mut pending = vec![PathBuf::new()];
        let mut collisions = Vec::new();
        while let Some(parent) = pending.pop() {
            for entry in fs::read_dir(working.join(&parent))? {
                let entry = entry?;
                let path = parent.join(entry.file_name());
                let metadata = fs::symlink_metadata(entry.path())?;
                let target = self.destination.join(&path);
                let existing = match fs::symlink_metadata(&target) {
                    Ok(metadata) => Some(metadata),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error).context("inspect module file collision"),
                };
                if metadata.is_dir() {
                    if existing.as_ref().is_some_and(|metadata| !metadata.is_dir()) {
                        collisions.push(relative.join(&path));
                    } else {
                        directories.push(path.clone());
                        pending.push(path);
                    }
                } else {
                    if existing.is_some() && !overlays.contains(&path) {
                        collisions.push(relative.join(&path));
                    }
                    files.push(path);
                }
            }
        }
        collisions.sort();
        ensure!(
            collisions.is_empty(),
            "provisioned files collide with restored module files: {}; move these paths and retry",
            collisions
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        directories.sort_by_key(|path| path.components().count());
        for path in directories {
            fs::create_dir_all(self.destination.join(path))?;
        }
        for path in files {
            crate::archive::ensure_no_symlink_ancestors(&self.destination, &path)?;
            let source = working.join(&path);
            let target = self.destination.join(&path);
            match fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&target)?,
                Ok(_) => fs::remove_file(&target)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("replace saved module overlay"),
            }
            fs::rename(&source, &target)?;
            self.moved.push((source, target));
        }
        Ok(())
    }
    fn commit(mut self) -> Result<()> {
        let backup = self.backup.take().context("missing module backup")?;
        backup
            .close()
            .context("remove completed module restore backup")
    }
    fn rollback_owned(mut self) -> Result<()> {
        let backup = self.backup.take().context("missing module backup")?.keep();
        self.rollback(&backup).with_context(|| {
            format!(
                "module rollback failed for {}; backup preserved at {}",
                self.destination.display(),
                backup.display()
            )
        })
    }
    fn rollback(&mut self, backup: &Path) -> Result<()> {
        for (source, target) in self.moved.iter().rev() {
            fs::rename(target, source).context("return moved module file to backup")?;
        }
        match fs::symlink_metadata(&self.destination) {
            Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&self.destination)?,
            Ok(_) => fs::remove_file(&self.destination)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove incomplete module checkout"),
        }
        fs::rename(backup.join("working"), &self.destination)
            .context("put provisioned files back")?;
        fs::remove_dir(backup).context("remove empty module rollback backup")
    }
}
impl Drop for SubmoduleRestoreOverlay {
    fn drop(&mut self) {
        if let Some(backup) = self.backup.take() {
            // Disable automatic deletion before any fallible rollback step.
            let backup = backup.keep();
            if let Err(error) = self.rollback(&backup) {
                tracing::error!(path = %self.destination.display(), backup = %backup.display(), error = %error, "module restore rollback failed; preserved backup must be recovered before retrying");
            }
        }
    }
}

fn restore_submodules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    submodules: &[SnapshotSubmodule],
    gitlinks: Option<&[SnapshotGitlink]>,
    allow_origin_fetch: bool,
) -> Result<()> {
    let mut submodules: Vec<_> = submodules.iter().collect();
    submodules.sort_by_key(|entry| entry.path.components().count());
    for path in submodules
        .iter()
        .map(|entry| &entry.path)
        .chain(gitlinks.into_iter().flatten().map(|entry| &entry.path))
    {
        crate::archive::validate_worktree_relative_path(path)?;
        ensure!(
            !path.as_os_str().is_empty()
                && path
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_))),
            "unsafe submodule path"
        );
        match fs::symlink_metadata(checkout.join(path)) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "refusing symlink at submodule restore path {}",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect submodule restore path"),
        }
    }
    for submodule in &submodules {
        crate::archive::slash_path(Path::new(&submodule.name))
            .with_context(|| format!("unsafe submodule admin name {:?}", submodule.name))?;
    }
    let owning_checkout = |path: &Path| -> &Path {
        submodules
            .iter()
            .filter(|parent| parent.path != path && path.starts_with(&parent.path))
            .max_by_key(|parent| parent.path.components().count())
            .map(|parent| parent.path.as_path())
            .unwrap_or_else(|| Path::new(""))
    };
    let apply_index = |parent: &Path| -> Result<BTreeMap<PathBuf, String>> {
        let owner = checkout.join(parent);
        let current = crate::checkpoint::snapshot_index_gitlinks(runner, &owner)?
            .into_iter()
            .map(|entry| (entry.path, entry.commit))
            .collect::<BTreeMap<_, _>>();
        let Some(gitlinks) = gitlinks else {
            return Ok(current);
        };
        let desired = gitlinks
            .iter()
            .filter(|entry| owning_checkout(&entry.path) == parent)
            .map(|entry| {
                Ok((
                    entry.path.strip_prefix(parent)?.to_path_buf(),
                    entry.commit.clone(),
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut updates = Vec::new();
        for (path, oid) in &current {
            if !desired.contains_key(path) {
                updates.extend(format!("0 {oid}\t").as_bytes());
                updates.extend(crate::archive::slash_path(path)?.as_bytes());
                updates.push(0);
            }
        }
        for (path, oid) in &desired {
            if current.get(path) != Some(oid) {
                updates.extend(format!("160000 {oid}\t").as_bytes());
                updates.extend(crate::archive::slash_path(path)?.as_bytes());
                updates.push(0);
            }
        }
        if !updates.is_empty() {
            git_bytes(
                runner,
                &owner,
                ["update-index", "-z", "--index-info"],
                &updates,
                "restore submodule index gitlinks",
            )?;
        }
        Ok(desired)
    };
    // Each owning index is restored once. Siblings only undo their temporary
    // absorbgitdirs entry; they never rewrite the parent's entire gitlink set.
    let mut restored_indices = BTreeMap::new();
    restored_indices.insert(PathBuf::new(), apply_index(Path::new(""))?);
    let mut omitted = Vec::<PathBuf>::new();
    for submodule in &submodules {
        if omitted
            .iter()
            .any(|parent| submodule.path.starts_with(parent))
        {
            continue;
        }
        let parent = owning_checkout(&submodule.path);
        let owner = checkout.join(parent);
        let mut submodule = (*submodule).clone();
        let relative = submodule.path.strip_prefix(parent)?;
        if gitlinks.is_none() {
            // Raw operations skip every uncommitted overlay, including a
            // submodule checkout position differing from the saved HEAD.
            let Some(position) = restored_indices
                .get(parent)
                .and_then(|index| index.get(relative))
            else {
                omitted.push(submodule.path.clone());
                continue;
            };
            submodule.commit = position.clone();
        }
        crate::archive::ensure_no_symlink_ancestors(checkout, &submodule.path)?;
        let saved_index = runner.run(
            &owner,
            &GitCommand {
                arguments: vec![
                    "--literal-pathspecs".into(),
                    "ls-files".into(),
                    "--stage".into(),
                    "-z".into(),
                    "--".into(),
                    relative.as_os_str().to_owned(),
                ],
                ..Default::default()
            },
        )?;
        ensure!(
            saved_index.status == 0,
            "{}",
            git_failure("save submodule overlay index entry", &saved_index)
        );
        let index_entry = saved_index.stdout;
        let directory = checkout.join(&submodule.path);
        let had_worktree = directory.is_dir() && directory.join(".git").try_exists()?;
        let mut overlay = SubmoduleRestoreOverlay::take(&directory)?;
        let result = (|| -> Result<()> {
            let admin = if directory.join(".git").try_exists()? {
                validate_restored_module_admin(runner, &owner, &directory, &submodule.name)?
            } else {
                let modules = git_path(runner, &owner, "modules")?;
                let relative = Path::new("modules").join(&submodule.name);
                crate::archive::ensure_no_symlink_ancestors(
                    modules.parent().context("module store has no parent")?,
                    &relative,
                )?;
                let admin = modules.join(&submodule.name);
                match fs::symlink_metadata(&admin) {
                    Ok(metadata) => ensure!(
                        !metadata.file_type().is_symlink(),
                        "submodule admin path is a symlink"
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).context("inspect submodule admin path"),
                }
                admin
            };
            let first_origin = submodule.origin_urls.first().with_context(|| {
                format!(
                    "submodule {} has no recorded origin",
                    submodule.path.display()
                )
            })?;
            let fresh = !admin.try_exists()?;
            if fresh {
                ensure!(
                    allow_origin_fetch,
                    "submodule {} needs a locally provisioned repository; origin fetching is disabled",
                    submodule.path.display()
                );
                initialize_repository(
                    runner,
                    &directory,
                    first_origin,
                    &submodule.object_format,
                    &submodule.url_rules,
                    &submodule.worktree_url_rules,
                    submodule.worktree_config_enabled,
                )?;
            } else {
                if !directory.join(".git").try_exists()? {
                    let output = runner.run(
                        &owner,
                        &GitCommand {
                            arguments: vec![
                                "init".into(),
                                format!("--object-format={}", submodule.object_format).into(),
                                "--separate-git-dir".into(),
                                admin.as_os_str().to_owned(),
                                "--".into(),
                                directory.as_os_str().to_owned(),
                            ],
                            ..Default::default()
                        },
                    )?;
                    ensure!(
                        output.status == 0,
                        "{}",
                        git_failure("reattach provisioned submodule", &output)
                    );
                }
                restore_url_rules(runner, &directory, &submodule.url_rules)?;
                if submodule.worktree_config_enabled {
                    git_text(
                        runner,
                        &directory,
                        ["config", "--local", "extensions.worktreeConfig", "true"],
                    )?;
                }
                restore_worktree_rules(runner, &directory, &submodule.worktree_url_rules, true)?;
                git_text(
                    runner,
                    &directory,
                    ["config", "--replace-all", "remote.origin.url", first_origin],
                )?;
                git_text(
                    runner,
                    &directory,
                    [
                        "config",
                        "--replace-all",
                        "remote.origin.fetch",
                        "+refs/heads/*:refs/remotes/origin/*",
                    ],
                )?;
                unset_config(runner, &directory, "remote.origin.pushurl")?;
            }
            for url in submodule.origin_urls.iter().skip(1) {
                git_text(
                    runner,
                    &directory,
                    ["remote", "set-url", "--add", "origin", url],
                )?;
            }
            for url in &submodule.push_urls {
                git_text(
                    runner,
                    &directory,
                    ["config", "--add", "remote.origin.pushurl", url],
                )?;
            }
            if allow_origin_fetch {
                fetch_origin(runner, &directory)?;
            }
            require_full_restore_history(runner, &directory, false)?;
            let mut needed = crate::checkpoint::snapshot_gitlink_commits(runner, &owner, relative)?;
            needed.push(submodule.commit.clone());
            needed.sort();
            needed.dedup();
            for oid in &needed {
                if run_git(runner, &directory, ["cat-file", "-e", oid], &[])?.status != 0 {
                    ensure!(
                        allow_origin_fetch,
                        "submodule {} is missing {oid}; origin fetching is disabled",
                        submodule.path.display()
                    );
                    let output = restore_object_fetch(runner, &directory, "origin", oid)?;
                    ensure!(
                        output.status == 0,
                        "{}",
                        git_failure("fetch missing submodule object", &output)
                    );
                }
                require_object(
                    runner,
                    &directory,
                    &format!("submodule {}", submodule.path.display()),
                    oid,
                )?;
            }
            verify_submodule_objects(
                runner,
                &directory,
                &submodule.path,
                &needed,
                allow_origin_fetch,
            )?;
            // Give Git a real HEAD before registering/absorbing the module. Its
            // index gitlink can intentionally differ from this checked-out commit.
            let mut command = GitCommand {
                arguments: ["checkout", "--detach", "--no-recurse-submodules"]
                    .map(Into::into)
                    .to_vec(),
                ..Default::default()
            };
            if !had_worktree {
                // A cached admin index may still describe files removed by
                // rollback. We own this newly created working directory; its
                // provisioned files remain protected in the overlay backup.
                command.arguments.push("--force".into());
            }
            command.arguments.push(submodule.commit.clone().into());
            let output = runner.run(&directory, &command)?;
            ensure!(
                output.status == 0,
                "{}",
                git_failure("check out restored module", &output)
            );
            git_text(
                runner,
                &owner,
                [
                    "config",
                    &format!("submodule.{}.url", submodule.name),
                    &submodule.url,
                ],
            )?;
            // absorbgitdirs resolves its path through the index, even when the
            // saved index intentionally deletes this still-initialized checkout.
            let temporary_gitlink = !restored_indices
                .get(parent)
                .context("missing restored parent index")?
                .contains_key(relative);
            if temporary_gitlink {
                let mut paths = Vec::new();
                for entry in index_entry
                    .split(|byte| *byte == 0)
                    .filter(|entry| !entry.is_empty())
                {
                    let tab = entry
                        .iter()
                        .position(|byte| *byte == b'\t')
                        .context("invalid saved index entry")?;
                    paths.extend_from_slice(&entry[tab + 1..]);
                    paths.push(0);
                }
                if !paths.is_empty() {
                    git_bytes(
                        runner,
                        &owner,
                        ["update-index", "--force-remove", "-z", "--stdin"],
                        &paths,
                        "temporarily remove submodule overlay index entries",
                    )?;
                }
                let mut entry = OsString::from(format!("160000,{},", submodule.commit));
                entry.push(relative.as_os_str());
                let output = runner.run(
                    &owner,
                    &GitCommand {
                        arguments: vec![
                            "update-index".into(),
                            "--add".into(),
                            "--cacheinfo".into(),
                            entry,
                        ],
                        ..Default::default()
                    },
                )?;
                ensure!(
                    output.status == 0,
                    "{}",
                    git_failure("register deleted submodule gitlink", &output)
                );
            }
            let output = runner.run(
                &owner,
                &GitCommand {
                    arguments: vec![
                        "--literal-pathspecs".into(),
                        "submodule".into(),
                        "absorbgitdirs".into(),
                        "--".into(),
                        relative.as_os_str().to_owned(),
                    ],
                    ..Default::default()
                },
            )?;
            ensure!(
                output.status == 0,
                "{}",
                git_failure("register restored submodule repository", &output)
            );
            if temporary_gitlink {
                let output = runner.run(
                    &owner,
                    &GitCommand {
                        arguments: vec![
                            "update-index".into(),
                            "--force-remove".into(),
                            "--".into(),
                            relative.as_os_str().to_owned(),
                        ],
                        ..Default::default()
                    },
                )?;
                ensure!(
                    output.status == 0,
                    "{}",
                    git_failure("remove temporary module gitlink", &output)
                );
            }
            if temporary_gitlink && !index_entry.is_empty() {
                git_bytes(
                    runner,
                    &owner,
                    ["update-index", "-z", "--index-info"],
                    &index_entry,
                    "restore submodule overlay index entry",
                )?;
            }
            if let Some(overlay) = &mut overlay {
                overlay.restore_files(relative, &index_entry)?;
            }
            restored_indices.insert(submodule.path.clone(), apply_index(&submodule.path)?);
            Ok(())
        })();
        if let Err(error) = result {
            if let Some(overlay) = overlay
                && let Err(rollback) = overlay.rollback_owned()
            {
                return Err(error.context(format!("{rollback:#}")));
            }
            return Err(error);
        }
        if let Some(overlay) = overlay {
            overlay.commit()?;
        }
    }
    for parent in std::iter::once(Path::new("")).chain(
        submodules
            .iter()
            .map(|module| module.path.as_path())
            .filter(|path| !omitted.iter().any(|omitted| path.starts_with(omitted))),
    ) {
        for path in restored_indices
            .get(parent)
            .context("missing restored module index")?
            .keys()
        {
            let path = parent.join(path);
            if !submodules.iter().any(|module| module.path == path)
                && checkout.join(&path).join(".git").try_exists()?
            {
                tracing::warn!(path = %checkout.join(&path).display(),
                    "snapshot recorded submodule as uninitialized; restore left the destination's checkout as it was");
            }
        }
    }
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RestoreBase {
    Clone {
        origin: String,
        seed: Option<PathBuf>,
    },
    /// The caller has already verified this workspace's history boundary.
    Provisioned { allow_origin_fetch: bool },
}
fn install_snapshot_pack(reader: &mut impl Read, path: &Path) -> Result<()> {
    let mut temporary =
        tempfile::NamedTempFile::new_in(path.parent().context("pack has no parent")?)?;
    std::io::copy(reader, temporary.as_file_mut()).context("copy snapshot pack into clone")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .context("publish snapshot pack into clone")?;
    Ok(())
}
fn restore_fetch(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    arguments: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> Result<crate::archive::GitOutput> {
    // Child modules are restored separately, after installing their own
    // recorded remotes.
    let mut args = ["fetch", "--no-recurse-submodules"]
        .map(OsString::from)
        .to_vec();
    args.extend(arguments.into_iter().map(|arg| arg.as_ref().to_owned()));
    runner.run(
        checkout,
        &GitCommand {
            arguments: args,
            ..Default::default()
        },
    )
}
fn verify_submodule_objects(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    path: &Path,
    tips: &[String],
    allow_origin_fetch: bool,
) -> Result<()> {
    let mut attempted = std::collections::BTreeSet::new();
    loop {
        let output = runner.run(
            checkout,
            &GitCommand {
                arguments: [
                    "rev-list",
                    "--objects",
                    "--quiet",
                    "--missing=print",
                    "--stdin",
                ]
                .map(Into::into)
                .to_vec(),
                stdin: format!("{}\n", tips.join("\n")).into_bytes(),
                // Verification must not implicitly hydrate through a provisioner's
                // promisor. Recovery below uses only the restored origin.
                env: vec![("GIT_NO_LAZY_FETCH".into(), "1".into())],
            },
        )?;
        let missing = std::str::from_utf8(&output.stdout)?
            .lines()
            .filter_map(|line| line.strip_prefix('?'))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        ensure!(
            output.status == 0,
            "submodule {} has incomplete saved history for {}; {}",
            path.display(),
            tips.join(", "),
            git_failure("verify submodule objects", &output)
        );
        if missing.is_empty() {
            return Ok(());
        }
        ensure!(
            allow_origin_fetch,
            "submodule {} is missing objects {} behind saved pins {}; origin fetching is disabled",
            path.display(),
            missing.join(", "),
            tips.join(", ")
        );
        for oid in missing {
            ensure!(
                attempted.insert(oid.clone()),
                "submodule {} is still missing {oid}; origin may have rewritten history",
                path.display()
            );
            let fetched = restore_object_fetch(runner, checkout, "origin", &oid)?;
            ensure!(
                fetched.status == 0,
                "submodule {} is missing {oid}; origin may have rewritten history; {}",
                path.display(),
                git_failure("fetch submodule dependency", &fetched)
            );
        }
    }
}

fn restore_object_fetch(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    remote: &str,
    oid: &str,
) -> Result<crate::archive::GitOutput> {
    // Recovery can run after saved refs are installed. Fetch objects without
    // pruning them, following tags, or applying destination ref mappings.
    restore_fetch(
        runner,
        checkout,
        [
            "--no-prune",
            "--no-prune-tags",
            "--no-tags",
            "--refmap=",
            remote,
            oid,
        ],
    )
}
fn fetch_missing_object(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    oid: &str,
    remote: Option<&str>,
    attempted: &mut std::collections::BTreeSet<String>,
    failures: &mut Vec<String>,
) -> Result<()> {
    if attempted.insert(oid.to_owned()) {
        let Some(remote) = remote else {
            failures.push(format!("origin fetch is disabled for missing object {oid}"));
            return Ok(());
        };
        match restore_object_fetch(runner, checkout, remote, oid) {
            Ok(output) if output.status == 0 => {}
            Ok(output) => {
                failures.push(git_failure("fetch missing snapshot object", &output).to_string())
            }
            Err(error) => failures.push(format!("fetch {oid}: {error:#}")),
        }
    }
    Ok(())
}
fn missing_tips(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    tips: &[String],
) -> Result<Vec<String>> {
    if tips.is_empty() {
        return Ok(Vec::new());
    }
    let output = runner.run(
        checkout,
        &GitCommand {
            arguments: vec![
                "cat-file".into(),
                "--batch-check=%(objectname) %(objecttype)".into(),
            ],
            stdin: format!("{}\n", tips.join("\n")).into_bytes(),
            ..Default::default()
        },
    )?;
    ensure!(
        output.status == 0,
        "{}",
        git_failure("check saved snapshot tips", &output)
    );
    std::str::from_utf8(&output.stdout)?
        .lines()
        .filter_map(|line| line.strip_suffix(" missing"))
        .map(|oid| Ok(oid.to_owned()))
        .collect()
}
fn clear_worktree_upstreams(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    branches: &[&str],
) -> Result<()> {
    if worktree_config_enabled(runner, checkout)? {
        let names = direct_config_keys(
            runner,
            checkout,
            "--worktree",
            r"^branch\..*\.(remote|merge)$",
        )?;
        if names.is_empty() {
            return Ok(());
        }
        let branches = branches
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        // Enumerate actual overrides once rather than spawning two no-op
        // writes per saved branch in every worktree. Includes remain owned by
        // provisioning, just as they are for URL configuration.
        for name in names {
            let branch = name
                .strip_prefix("branch.")
                .and_then(|name| name.rsplit_once('.'))
                .map(|(branch, _)| branch);
            if branch.is_some_and(|branch| branches.contains(branch)) {
                let output = run_git(
                    runner,
                    checkout,
                    ["config", "--worktree", "--unset-all", &name],
                    &[],
                )?;
                ensure!(
                    matches!(output.status, 0 | 5),
                    "{}",
                    git_failure("clear provisioned worktree upstream", &output)
                );
            }
        }
    }
    Ok(())
}

fn restore_worktree_rules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    rules: &[SnapshotConfigEntry],
    replace_origin: bool,
) -> Result<()> {
    validate_url_rules(rules)?;
    // Origin and push URLs are restored in shared config. Clear direct worktree values
    // which would otherwise add stale destinations, including on retry.
    if replace_origin && worktree_config_enabled(runner, checkout)? {
        for key in ["remote.origin.url", "remote.origin.pushurl"] {
            let output = run_git(
                runner,
                checkout,
                ["config", "--worktree", "--unset-all", key],
                &[],
            )?;
            ensure!(
                matches!(output.status, 0 | 5),
                "{}",
                git_failure("clear provisioned worktree origin URLs", &output)
            );
        }
    }
    // `worktree add` copies config.worktree from the primary checkout. Replace
    // those inherited URL rules so each restored checkout keeps its own scope.
    if worktree_config_enabled(runner, checkout)? {
        clear_direct_url_rules(runner, checkout, true)?;
    }
    if !rules.is_empty() {
        git_text(
            runner,
            checkout,
            ["config", "extensions.worktreeConfig", "true"],
        )?;
        for rule in rules {
            git_text(
                runner,
                checkout,
                ["config", "--worktree", "--add", &rule.name, &rule.value],
            )?;
        }
    }
    Ok(())
}
fn initialize_repository(
    runner: &dyn GitCommandRunner,
    destination: &Path,
    origin: &str,
    object_format: &str,
    url_rules: &[SnapshotConfigEntry],
    worktree_rules: &[SnapshotConfigEntry],
    worktree_config_enabled: bool,
) -> Result<()> {
    let parent = destination.parent().context("destination has no parent")?;
    fs::create_dir_all(parent)?;
    ensure!(
        !destination.try_exists()? || fs::read_dir(destination)?.next().is_none(),
        "snapshot destination {} is not empty",
        destination.display()
    );
    ensure!(
        matches!(object_format, "sha1" | "sha256"),
        "unsupported Git object format {object_format}"
    );
    let output = runner.run(
        parent,
        &GitCommand {
            arguments: vec![
                "init".into(),
                "--initial-branch=main".into(),
                format!("--object-format={object_format}").into(),
                "--".into(),
                destination.as_os_str().to_owned(),
            ],
            ..Default::default()
        },
    )?;
    ensure!(
        output.status == 0,
        "{}",
        git_failure("initialize snapshot restore", &output)
    );
    restore_url_rules(runner, destination, url_rules)?;
    if worktree_config_enabled {
        git_text(
            runner,
            destination,
            ["config", "extensions.worktreeConfig", "true"],
        )?;
    }
    restore_worktree_rules(runner, destination, worktree_rules, true)?;
    git_text(runner, destination, ["remote", "add", "origin", origin])?;
    Ok(())
}
fn borrow_objects(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    objects: &Path,
) -> Result<PathBuf> {
    let path = git_path(runner, repository, "objects/info/alternates")?;
    fs::create_dir_all(path.parent().context("alternates path has no parent")?)?;
    let quoted = objects
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|byte| format!("\\{byte:03o}"))
        .collect::<String>();
    fs::write(&path, format!("\"{quoted}\"\n"))?;
    Ok(path)
}
fn configure_remote_lists(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    origins: &[String],
    pushes: &[String],
) -> Result<()> {
    for url in origins.iter().skip(1) {
        git_text(
            runner,
            checkout,
            ["remote", "set-url", "--add", "origin", url],
        )?;
    }
    for url in pushes {
        git_text(
            runner,
            checkout,
            ["config", "--add", "remote.origin.pushurl", url],
        )?;
    }
    Ok(())
}
fn fetch_origin(runner: &dyn GitCommandRunner, destination: &Path) -> Result<()> {
    for arguments in [
        &["origin", "+refs/heads/*:refs/remotes/origin/*"][..],
        &["--tags", "origin"][..],
    ] {
        let output = restore_fetch(runner, destination, arguments)?;
        ensure!(
            output.status == 0,
            "{}",
            git_failure("fetch origin", &output)
        );
    }
    require_full_restore_history(runner, destination, true)?;
    let default_head = run_git(
        runner,
        destination,
        ["remote", "set-head", "origin", "--auto"],
        &[],
    )?;
    if default_head.status != 0 {
        tracing::warn!(error = %git_failure("set origin default branch", &default_head), "origin has no usable default branch");
    }
    Ok(())
}
pub fn restore_workspace_snapshot(
    runner: &dyn GitCommandRunner,
    snapshot: &WorkspaceSnapshot,
    destination: &Path,
    base: &RestoreBase,
) -> Result<()> {
    ensure!(
        !snapshot.origin_urls.is_empty(),
        "cannot restore snapshot without an origin remote; this snapshot is in-place-only: resume its raw checkout without restoring Git state"
    );
    let modules = snapshot.initialized_submodules.iter().chain(
        snapshot
            .worktrees
            .iter()
            .flat_map(|tree| &tree.initialized_submodules),
    );
    validate_url_rules(
        snapshot
            .url_rules
            .iter()
            .chain(&snapshot.worktree_url_rules)
            .chain(snapshot.worktrees.iter().flat_map(|tree| &tree.url_rules))
            .chain(
                modules
                    .flat_map(|module| module.url_rules.iter().chain(&module.worktree_url_rules)),
            ),
    )?;
    // Validate every persisted working-tree path before creating or changing
    // any repository, including paths below Git administration directories.
    for path in snapshot
        .initialized_submodules
        .iter()
        .map(|module| &module.path)
        .chain(snapshot.index_gitlinks.iter().map(|link| &link.path))
        .chain(snapshot.worktrees.iter().map(|tree| &tree.relative_path))
        .chain(snapshot.worktrees.iter().flat_map(|tree| {
            tree.initialized_submodules
                .iter()
                .map(|module| &module.path)
        }))
        .chain(
            snapshot
                .worktrees
                .iter()
                .flat_map(|tree| tree.index_gitlinks.iter().map(|link| &link.path)),
        )
    {
        crate::archive::validate_worktree_relative_path(path)?;
    }
    let destination = std::path::absolute(destination)?;
    let destination = destination.as_path();
    if matches!(base, RestoreBase::Provisioned { .. }) {
        validate_restore_worktree_root(runner, destination)?;
        let root = fs::canonicalize(destination)?;
        let (private, common) = crate::archive::git::git_directories(runner, destination)?;
        ensure!(
            fs::canonicalize(&private)?.starts_with(&root)
                && fs::canonicalize(&common)?.starts_with(&root),
            "provisioned restore destination {} shares Git administration outside its checkout ({} and {}); provision an independent clone here and retry",
            destination.display(),
            private.display(),
            common.display()
        );
        for (tree_path, modules) in
            std::iter::once((Path::new(""), snapshot.initialized_submodules.as_slice())).chain(
                snapshot.worktrees.iter().map(|tree| {
                    (
                        tree.relative_path.as_path(),
                        tree.initialized_submodules.as_slice(),
                    )
                }),
            )
        {
            let tree = destination.join(tree_path);
            for module in modules {
                let directory = tree.join(&module.path);
                if directory.is_dir() && directory.join(".git").try_exists()? {
                    let parent = modules
                        .iter()
                        .filter(|parent| {
                            parent.path != module.path && module.path.starts_with(&parent.path)
                        })
                        .max_by_key(|parent| parent.path.components().count())
                        .map(|parent| parent.path.as_path())
                        .unwrap_or_else(|| Path::new(""));
                    validate_restored_module_admin(
                        runner,
                        &tree.join(parent),
                        &directory,
                        &module.name,
                    )?;
                }
            }
        }
    }
    let allow_origin_fetch = match base {
        RestoreBase::Clone { origin, seed } => {
            initialize_repository(
                runner,
                destination,
                origin,
                &snapshot.object_format,
                &snapshot.url_rules,
                &snapshot.worktree_url_rules,
                snapshot.worktree_config_enabled,
            )?;
            configure_remote_lists(
                runner,
                destination,
                &snapshot.origin_urls,
                &snapshot.push_urls,
            )?;
            let mut alternates = None;
            if let Some(seed) = seed.as_deref()
                && seed.try_exists()?
                && let Ok((_, common)) = crate::archive::git::git_directories(runner, seed)
            {
                let objects = common.join("objects");
                if objects.is_dir() {
                    let path = borrow_objects(runner, destination, &objects)?;
                    alternates = Some(path);
                }
            }
            fetch_origin(runner, destination)?;
            if let Some(alternates) = alternates {
                git_text(runner, destination, ["repack", "-a", "-d"])?;
                fs::remove_file(alternates)?;
            }
            true
        }
        RestoreBase::Provisioned { allow_origin_fetch } => {
            restore_url_rules(runner, destination, &snapshot.url_rules)?;
            if snapshot.worktree_config_enabled {
                git_text(
                    runner,
                    destination,
                    ["config", "extensions.worktreeConfig", "true"],
                )?;
            }
            restore_worktree_rules(
                runner,
                destination,
                &snapshot.worktree_url_rules,
                snapshot.source_had_origin,
            )?;
            if snapshot.source_had_origin
                && let Some(first) = snapshot.origin_urls.first()
            {
                git_text(
                    runner,
                    destination,
                    ["config", "--replace-all", "remote.origin.url", first],
                )?;
                for url in snapshot.origin_urls.iter().skip(1) {
                    git_text(
                        runner,
                        destination,
                        ["config", "--add", "remote.origin.url", url],
                    )?;
                }
                unset_config(runner, destination, "remote.origin.pushurl")?;
                for url in &snapshot.push_urls {
                    git_text(
                        runner,
                        destination,
                        ["config", "--add", "remote.origin.pushurl", url],
                    )?;
                }
            }
            *allow_origin_fetch
        }
    };
    if let Some(pack) = &snapshot.pack {
        let format = git_text(runner, destination, ["rev-parse", "--show-object-format"])?;
        let hash_bytes = match format.as_str() {
            "sha1" => 20,
            "sha256" => 32,
            _ => anyhow::bail!("unsupported Git object format {format}"),
        };
        let mut file = fs::File::open(pack)
            .with_context(|| format!("open snapshot pack {}", pack.display()))?;
        file.seek(SeekFrom::End(-hash_bytes))?;
        let mut hash = vec![0; hash_bytes as usize];
        file.read_exact(&mut hash)?;
        let hash = mj_core::hex::lower_hex(&hash);
        let directory = git_path(runner, destination, "objects/pack")?;
        fs::create_dir_all(&directory)?;
        let path = directory.join(format!("pack-{hash}.pack"));
        // index-pack makes installed packs read-only. A provisioned retry can
        // reuse this content-addressed file; index-pack still verifies it below.
        if !path.try_exists()? {
            file.seek(SeekFrom::Start(0))?;
            install_snapshot_pack(&mut file, &path)?;
        }
        let output = runner.run(
            destination,
            &GitCommand {
                arguments: vec!["index-pack".into(), path.as_os_str().to_owned()],
                ..Default::default()
            },
        )?;
        ensure!(
            output.status == 0,
            "{}",
            git_failure("import snapshot pack", &output)
        );
    }
    let mut tips: Vec<String> = snapshot.refs.values().cloned().collect();
    tips.push(snapshot.head.commit().to_owned());
    tips.extend(snapshot.working_state.clone());
    tips.extend(snapshot.stash.iter().map(|entry| entry.commit.clone()));
    for tree in &snapshot.worktrees {
        tips.push(tree.head.commit().to_owned());
        tips.extend(tree.working_state.clone());
    }
    tips.sort();
    tips.dedup();
    let fetch_remote = if !allow_origin_fetch {
        None
    } else if matches!(base, RestoreBase::Provisioned { .. }) && !snapshot.source_had_origin {
        snapshot.origin_urls.first().map(String::as_str)
    } else {
        Some("origin")
    };
    let mut attempted = std::collections::BTreeSet::new();
    let mut fetch_failures = Vec::new();
    for oid in missing_tips(runner, destination, &tips)? {
        fetch_missing_object(
            runner,
            destination,
            &oid,
            fetch_remote,
            &mut attempted,
            &mut fetch_failures,
        )?;
    }
    let remaining_tips = missing_tips(runner, destination, &tips)?;
    ensure!(
        remaining_tips.is_empty(),
        "snapshot objects {} for HEAD, {} are missing; origin may have rewritten history; {}",
        remaining_tips.join(", "),
        snapshot.refs.keys().cloned().collect::<Vec<_>>().join(", "),
        fetch_failures.join("; ")
    );
    // Origin tags remain available to raw conversions, except when replaced,
    // deleted, or prefix-conflicting with a saved tag.
    let fetched_tags = git_text(
        runner,
        destination,
        [
            "for-each-ref",
            "--format=%(refname)%00%(objectname)",
            "refs/tags",
            "refs/heads",
            "refs/notes",
            "refs/mj",
        ],
    )?;
    for line in fetched_tags.lines() {
        let (name, oid) = line.split_once('\0').context("malformed clone ref")?;
        let conflict = snapshot.refs.iter().any(|(saved, saved_oid)| {
            (saved == name && saved_oid != oid)
                || saved
                    .strip_prefix(name)
                    .is_some_and(|suffix| suffix.starts_with('/'))
                || name
                    .strip_prefix(saved)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        });
        if conflict
            || (snapshot.refs_complete
                && local_snapshot_ref(name)
                && !snapshot.refs.contains_key(name))
        {
            git_text(
                runner,
                destination,
                ["update-ref", "--no-deref", "-d", name],
            )?;
        }
    }
    for name in &snapshot.deleted_refs {
        ensure!(
            local_snapshot_ref(name),
            "deleted snapshot ref {name} is outside the local-ref allowlist"
        );
        git_text(
            runner,
            destination,
            ["update-ref", "--no-deref", "-d", name],
        )?;
    }
    let mut restore_refs = snapshot.refs.clone();
    for head in
        std::iter::once(&snapshot.head).chain(snapshot.worktrees.iter().map(|tree| &tree.head))
    {
        if let SnapshotHead::Branch { name, commit } = head {
            restore_refs.insert(name.clone(), commit.clone());
        }
    }
    for (name, oid) in &restore_refs {
        require_object(runner, destination, name, oid)?;
        git_text(runner, destination, ["update-ref", "--no-deref", name, oid])?;
    }
    require_full_restore_history(runner, destination, false)?;
    let graph_command = GitCommand {
        arguments: [
            "rev-list",
            "--objects",
            "--quiet",
            "--missing=print",
            "--stdin",
        ]
        .into_iter()
        .map(OsString::from)
        .collect(),
        stdin: format!("{}\n", tips.join("\n")).into_bytes(),
        ..Default::default()
    };
    // --quiet suppresses ordinary object lines while --missing=print still
    // reports missing commits, trees AND blobs. No object listing is buffered.
    let mut commit_graph_command = graph_command.clone();
    commit_graph_command
        .arguments
        .push("--filter=tree:0".into());
    let git_version =
        mj_core::local_git::parse_git_version(&git_text(runner, destination, ["--version"])?)
            .unwrap_or(mj_core::local_git::MINIMUM_GIT_VERSION);
    // Before Git 2.43, missing interior commits fail visibly; only saved tips get SHA recovery.
    if git_version >= (2, 43, 0) {
        let first = runner.run(destination, &commit_graph_command)?;
        for line in first.stdout.split(|byte| *byte == b'\n') {
            if let Some(oid) = line.strip_prefix(b"?") {
                fetch_missing_object(
                    runner,
                    destination,
                    std::str::from_utf8(oid)?,
                    fetch_remote,
                    &mut attempted,
                    &mut fetch_failures,
                )?;
            }
        }
    }
    let graph = runner.run(destination, &graph_command)?;
    let missing = graph
        .stdout
        .split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"?"));
    ensure!(
        graph.status == 0 && !missing,
        "snapshot history for {} is incomplete; origin may have rewritten history; missing objects: {}; {} {}{}",
        std::iter::once("HEAD".to_owned())
            .chain(restore_refs.keys().cloned())
            .collect::<Vec<_>>()
            .join(", "),
        String::from_utf8_lossy(&graph.stdout).trim(),
        String::from_utf8_lossy(&graph.stderr).trim(),
        fetch_failures.join("; "),
        if git_version < (2, 43, 0) {
            "; install Git 2.43 or newer and resume again"
        } else {
            ""
        }
    );
    let restored_branches = restore_refs
        .keys()
        .filter_map(|name| name.strip_prefix("refs/heads/"))
        .collect::<Vec<_>>();
    clear_worktree_upstreams(runner, destination, &restored_branches)?;
    for name in &restored_branches {
        unset_config(runner, destination, &format!("branch.{name}.remote"))?;
        unset_config(runner, destination, &format!("branch.{name}.merge"))?;
    }
    for (name, tracking) in &snapshot.branch_tracking {
        if tracking.remote.as_deref() != Some("origin") {
            continue;
        }
        if let Some(remote) = &tracking.remote {
            git_text(
                runner,
                destination,
                ["config", &format!("branch.{name}.remote"), remote],
            )?;
        }
        for merge in &tracking.merge {
            git_text(
                runner,
                destination,
                ["config", "--add", &format!("branch.{name}.merge"), merge],
            )?;
        }
    }
    // Provisioned restore may retry after a later step needs local provisioning.
    // Rebuild the saved list rather than appending another copy on each attempt.
    git_text(
        runner,
        destination,
        ["update-ref", "--no-deref", "-d", "refs/stash"],
    )?;
    let mut previous_stash: Option<&str> = None;
    for entry in snapshot.stash.iter().rev() {
        require_object(runner, destination, "stash", &entry.commit)?;
        let repeated = previous_stash == Some(entry.commit.as_str());
        if repeated {
            // update-ref and stash store both elide unchanged OIDs. Force a
            // real transition, then remove its temporary reflog entry below.
            let parent = git_text(
                runner,
                destination,
                ["rev-parse", &format!("{}^1", entry.commit)],
            )?;
            let output = runner.run(
                destination,
                &GitCommand {
                    arguments: [
                        "update-ref",
                        "-m",
                        "snapshot repeated stash transition",
                        "refs/stash",
                        &parent,
                    ]
                    .map(OsString::from)
                    .to_vec(),
                    env: synthetic_git_identity(),
                    ..Default::default()
                },
            )?;
            ensure!(
                output.status == 0,
                "{}",
                git_failure("prepare repeated stash entry", &output)
            );
        }
        let output = runner.run(
            destination,
            &GitCommand {
                arguments: vec![
                    "stash".into(),
                    "store".into(),
                    "-m".into(),
                    entry.message.clone().into(),
                    entry.commit.clone().into(),
                ],
                env: synthetic_git_identity(),
                ..Default::default()
            },
        )?;
        ensure!(
            output.status == 0,
            "{}",
            git_failure("restore stash list", &output)
        );
        if repeated {
            git_text(
                runner,
                destination,
                ["reflog", "delete", "--rewrite", "refs/stash@{1}"],
            )?;
        }
        previous_stash = Some(&entry.commit);
    }
    checkout_head(runner, destination, &snapshot.head)?;
    if let Some(state) = &snapshot.working_state {
        require_object(runner, destination, "working state", state)?;
        git_text(runner, destination, ["stash", "apply", "--index", state])?;
    }
    restore_submodules(
        runner,
        destination,
        &snapshot.initialized_submodules,
        (!snapshot.working_state_skipped).then_some(snapshot.index_gitlinks.as_slice()),
        allow_origin_fetch,
    )?;
    // A provisioned retry may have created some of these before a later
    // module needed provisioning. Reuse only this repository's registrations.
    let existing_worktrees = discover_linked_worktrees(runner, destination)?;
    for tree in &snapshot.worktrees {
        ensure!(
            !tree.relative_path.as_os_str().is_empty()
                && tree
                    .relative_path
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "unsafe nested worktree path"
        );
        crate::archive::ensure_no_symlink_ancestors(destination, &tree.relative_path)?;
        let path = destination.join(&tree.relative_path);
        let mut arguments: Vec<OsString> = vec!["worktree".into(), "add".into(), "--force".into()];
        let head_oid = tree.head.commit();
        require_object(
            runner,
            destination,
            &format!("worktree {} HEAD", tree.relative_path.display()),
            head_oid,
        )?;
        let rev = match &tree.head {
            SnapshotHead::Branch { name, .. } => name
                .strip_prefix("refs/heads/")
                .context("worktree HEAD is not a branch")?,
            SnapshotHead::Detached(oid) => {
                arguments.push("--detach".into());
                oid
            }
        };
        if !existing_worktrees
            .iter()
            .any(|existing| existing.relative == tree.relative_path)
        {
            arguments.extend(["--".into(), path.as_os_str().to_owned(), rev.into()]);
            let output = runner.run(
                destination,
                &GitCommand {
                    arguments,
                    ..Default::default()
                },
            )?;
            ensure!(
                output.status == 0,
                "{}",
                git_failure("restore nested worktree", &output)
            );
        }
        checkout_head(runner, &path, &tree.head)?;
        if let Some(state) = &tree.working_state {
            require_object(
                runner,
                destination,
                &format!("worktree {} working state", tree.relative_path.display()),
                state,
            )?;
            git_text(runner, &path, ["stash", "apply", "--index", state])?;
        }
        restore_worktree_rules(
            runner,
            &path,
            &tree.url_rules,
            snapshot.source_had_origin || matches!(base, RestoreBase::Clone { .. }),
        )?;
        clear_worktree_upstreams(runner, &path, &restored_branches)?;
        restore_submodules(
            runner,
            &path,
            &tree.initialized_submodules,
            (!tree.working_state_skipped).then_some(tree.index_gitlinks.as_slice()),
            allow_origin_fetch,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::SystemGit;

    fn git_arguments(command: &GitCommand) -> &[OsString] {
        let mut arguments = command.arguments.as_slice();
        while arguments.len() >= 2 && arguments[0] == "-c" {
            arguments = &arguments[2..];
        }
        arguments
    }
    struct FileGit;
    impl GitCommandRunner for FileGit {
        fn run(
            &self,
            repository: &Path,
            command: &GitCommand,
        ) -> Result<crate::archive::GitOutput> {
            let mut command = command.clone();
            let mut arguments = vec!["-c".into(), "protocol.file.allow=always".into()];
            arguments.extend(command.arguments);
            command.arguments = arguments;
            SystemGit.run(repository, &command)
        }
    }
    struct HostGit {
        global: PathBuf,
    }
    impl GitCommandRunner for HostGit {
        fn run(
            &self,
            repository: &Path,
            command: &GitCommand,
        ) -> Result<crate::archive::GitOutput> {
            let mut command = command.clone();
            command.env.extend([
                (
                    "GIT_CONFIG_GLOBAL".into(),
                    self.global.as_os_str().to_owned(),
                ),
                (
                    "GIT_CONFIG_SYSTEM".into(),
                    self.global.with_extension("system").into_os_string(),
                ),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ]);
            SystemGit.run(repository, &command)
        }
    }
    fn host_git(path: &Path, name: &str) -> HostGit {
        let global = path.join(name);
        fs::write(&global, "").unwrap();
        fs::write(global.with_extension("system"), "").unwrap();
        HostGit { global }
    }
    fn add_submodule(root: &Repo, module: &Repo) -> PathBuf {
        git(
            &root.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                module.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&root.checkout, &["commit", "-am", "registered submodule"]);
        git(&root.checkout, &["push", "origin", "main"]);
        root.checkout.join("sub")
    }
    fn scoped_item_path(
        root: &Path,
        checkout: CheckoutKey,
        path: impl Into<PathBuf>,
    ) -> UntrackedItem {
        let path = path.into();
        let identity = file_identity(&fs::symlink_metadata(root.join(&path)).unwrap()).unwrap();
        UntrackedItem::Path {
            checkout,
            path,
            identity,
        }
    }
    fn item_path(root: &Path, path: impl Into<PathBuf>) -> UntrackedItem {
        scoped_item_path(root, CheckoutKey::default(), path)
    }
    fn item_stash(commit: String, message: String) -> UntrackedItem {
        UntrackedItem::Stash {
            checkout: CheckoutKey::default(),
            commit,
            message,
        }
    }
    fn clone_base(snapshot: &WorkspaceSnapshot, seed: Option<&Path>) -> RestoreBase {
        RestoreBase::Clone {
            origin: snapshot.origin_urls.first().cloned().unwrap_or_default(),
            seed: seed.map(Path::to_path_buf),
        }
    }
    struct Repo {
        temp: tempfile::TempDir,
        origin: PathBuf,
        checkout: PathBuf,
    }
    fn git(path: &Path, args: &[&str]) -> String {
        let output = SystemGit
            .run(
                path,
                &GitCommand {
                    arguments: args.iter().map(OsString::from).collect(),
                    env: [
                        "GIT_AUTHOR_NAME",
                        "GIT_AUTHOR_EMAIL",
                        "GIT_COMMITTER_NAME",
                        "GIT_COMMITTER_EMAIL",
                    ]
                    .into_iter()
                    .map(|k| (k.into(), "snapshot@example.test".into()))
                    .collect(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            output.status,
            0,
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end_matches('\n')
            .to_owned()
    }
    impl Repo {
        fn new() -> Self {
            Self::new_with_format("sha1")
        }
        fn new_with_format(object_format: &str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let origin = temp.path().join("origin.git");
            let checkout = temp.path().join("checkout");
            git(
                temp.path(),
                &[
                    "init",
                    "--bare",
                    "--initial-branch=main",
                    &format!("--object-format={object_format}"),
                    origin.to_str().unwrap(),
                ],
            );
            git(
                temp.path(),
                &[
                    "clone",
                    origin.to_str().unwrap(),
                    checkout.to_str().unwrap(),
                ],
            );
            // stash create requires an identity even though it doesn't change refs.
            git(&checkout, &["config", "user.name", "Snapshot Test"]);
            git(
                &checkout,
                &["config", "user.email", "snapshot@example.test"],
            );
            fs::write(checkout.join("file"), "first\nsecond\nthird\n").unwrap();
            git(&checkout, &["add", "file"]);
            git(&checkout, &["commit", "-m", "base"]);
            git(&checkout, &["push", "origin", "main"]);
            Self {
                temp,
                origin,
                checkout,
            }
        }
        fn capture(&self, ownership: &CheckoutOwnership) -> WorkspaceSnapshot {
            capture_workspace_snapshot(
                &FileGit,
                &self.checkout,
                ownership,
                &self.temp.path().join("snapshot.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap()
        }
        fn restore(&self, snapshot: &WorkspaceSnapshot, seed: Option<&Path>) -> PathBuf {
            let path = self.temp.path().join("restored");
            restore_workspace_snapshot(&FileGit, snapshot, &path, &clone_base(snapshot, seed))
                .unwrap();
            path
        }
        fn commit(&self, text: &str) {
            fs::write(self.checkout.join("file"), text).unwrap();
            git(&self.checkout, &["commit", "-am", "change"]);
        }
        fn conflict(&self) {
            git(&self.checkout, &["branch", "side"]);
            self.commit("main version\n");
            git(&self.checkout, &["checkout", "side"]);
            self.commit("side version\n");
            git(&self.checkout, &["checkout", "main"]);
        }
        fn start_conflict(&self, kind: &str) {
            let output = run_git(&FileGit, &self.checkout, [kind, "side"], &[]).unwrap();
            assert_ne!(output.status, 0);
        }
    }

    #[test]
    fn pushed_branch_records_refs_without_pack() {
        let r = Repo::new();
        git(
            &r.checkout,
            &["tag", "-a", "origin-tag", "-m", "already published"],
        );
        git(&r.checkout, &["push", "origin", "origin-tag"]);
        git(
            &r.checkout,
            &["branch", "--set-upstream-to=origin/main", "main"],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_none());
        assert_eq!(
            snapshot.branch_tracking["main"],
            BranchTracking {
                remote: Some("origin".into()),
                merge: vec!["refs/heads/main".into()]
            }
        );
        assert!(snapshot.refs.contains_key("refs/tags/origin-tag"));
        assert!(!r.temp.path().join("snapshot.pack").exists());
        assert_eq!(
            snapshot.refs["refs/heads/main"],
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
        let restored = r.restore(&snapshot, Some(&r.checkout));
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            snapshot.refs["refs/heads/main"]
        );
        assert!(!restored.join(".git/objects/info/alternates").exists());
        assert_eq!(git(&restored, &["config", "branch.main.remote"]), "origin");
        assert_eq!(
            git(&restored, &["config", "branch.main.merge"]),
            "refs/heads/main"
        );
    }
    #[test]
    fn unpushed_commit_and_tag_survive() {
        let r = Repo::new();
        r.commit("unpublished\n");
        // Incompressible content exercises both pack stdout and import stdin
        // beyond the 64 KiB pipe buffer boundary.
        let mut state = 0x12345678_u32;
        let large: Vec<u8> = (0..262_144)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        fs::write(r.checkout.join("large"), &large).unwrap();
        git(&r.checkout, &["add", "large"]);
        git(&r.checkout, &["commit", "-m", "large tracked file"]);
        git(&r.checkout, &["tag", "-a", "v1", "-m", "release"]);
        for name in [
            "refs/mj/reviewed/test",
            "refs/mj/overridden/test",
            "refs/mj/push-base/test",
        ] {
            git(&r.checkout, &["update-ref", name, "HEAD"]);
        }
        git(&r.checkout, &["config", "fetch.pruneTags", "true"]);
        git(&r.checkout, &["config", "remote.origin.pruneTags", "true"]);
        git(
            &r.checkout,
            &[
                "config",
                "--add",
                "remote.origin.fetch",
                "+refs/tags/*:refs/tags/*",
            ],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_some());
        assert!(snapshot.refs.contains_key("refs/tags/v1"));
        assert_eq!(git(&r.checkout, &["tag", "--list", "v1"]), "v1");
        assert!(snapshot.refs.contains_key("refs/mj/reviewed/test"));
        assert!(snapshot.refs.contains_key("refs/mj/overridden/test"));
        assert!(snapshot.refs.contains_key("refs/mj/push-base/test"));
        assert!(fs::metadata(snapshot.pack.as_ref().unwrap()).unwrap().len() > 65_536);
        let restored = r.restore(&snapshot, None);
        for (name, oid) in &snapshot.refs {
            assert_eq!(&git(&restored, &["rev-parse", name]), oid);
        }
        assert_eq!(fs::read(restored.join("large")).unwrap(), large);
        assert_eq!(git(&r.checkout, &["for-each-ref", "refs/mj/snapshot"]), "");
        assert_eq!(git(&restored, &["for-each-ref", "refs/mj/restore"]), "");
        assert_eq!(git(&restored, &["status", "--porcelain"]), "");
    }
    // Hard-won: ffd652c27: reflog expiration hid the still-applicable root stash ref.
    #[test]
    fn expired_root_stash_ref_survives_without_reflog_entries() {
        let root = Repo::new();
        fs::write(root.checkout.join("file"), "stash retained by ref\n").unwrap();
        git(&root.checkout, &["stash", "push", "-m", "retained stash"]);
        let stash = git(&root.checkout, &["rev-parse", "refs/stash"]);
        git(
            &root.checkout,
            &["reflog", "expire", "--expire=all", "refs/stash"],
        );
        assert!(git(&root.checkout, &["stash", "list"]).is_empty());
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert_eq!(snapshot.stash[0].commit, stash);
        let restored = root.restore(&snapshot, None);
        git(&restored, &["stash", "apply"]);
        assert_eq!(
            fs::read_to_string(restored.join("file")).unwrap(),
            "stash retained by ref\n"
        );
    }

    // Hard-won: ffd652c27: invalid .gitmodules blocked inventory before raw/managed operation policy.
    #[test]
    fn conflicted_gitmodules_obeys_raw_skip_and_managed_abort_policy() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        git(
            &root.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.branch",
                "base",
            ],
        );
        git(&root.checkout, &["commit", "-am", "base module setting"]);
        git(&root.checkout, &["switch", "-c", "side"]);
        git(
            &root.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.branch",
                "side",
            ],
        );
        git(&root.checkout, &["commit", "-am", "side module setting"]);
        git(&root.checkout, &["checkout", "main"]);
        git(
            &root.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.branch",
                "main",
            ],
        );
        git(&root.checkout, &["commit", "-am", "main module setting"]);
        assert_ne!(
            run_git(&FileGit, &root.checkout, ["merge", "side"], &[])
                .unwrap()
                .status,
            0
        );
        let original = fs::read(root.checkout.join(".gitmodules")).unwrap();
        let raw = root.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(raw.working_state_skipped);
        assert_eq!(
            fs::read(root.checkout.join(".gitmodules")).unwrap(),
            original
        );
        assert!(root.checkout.join(".git/MERGE_HEAD").exists());
        let restored = root.restore(&raw, None);
        assert_eq!(
            git(
                &restored,
                &["config", "--file", ".gitmodules", "submodule.sub.branch"]
            ),
            "main"
        );
        let refused = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("refused.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                refused.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::OperationInProgress(_))
            ),
            "{refused:#}"
        );
        root.capture(&CheckoutOwnership::Managed);
        assert!(!root.checkout.join(".git/MERGE_HEAD").exists());
        assert_eq!(
            git(
                &root.checkout,
                &["config", "--file", ".gitmodules", "submodule.sub.branch"]
            ),
            "main"
        );
    }

    // Hard-won: ffd652c27: scoped origin push URLs silently collapsed to the shared primary remote.
    #[test]
    fn scoped_remote_settings_emit_primary_remote_restore_notice() {
        let root = Repo::new();
        let module = Repo::new();
        let primary_push = Repo::new();
        let scoped_push = Repo::new();
        let module_dir = add_submodule(&root, &module);
        for directory in [&root.checkout, &module_dir] {
            git(directory, &["config", "extensions.worktreeConfig", "true"]);
        }
        git(
            &root.checkout,
            &[
                "config",
                "--worktree",
                "remote.origin.pushurl",
                primary_push.origin.to_str().unwrap(),
            ],
        );
        git(
            &module_dir,
            &[
                "config",
                "--worktree",
                "remote.origin.pushurl",
                scoped_push.origin.to_str().unwrap(),
            ],
        );
        let linked = root.checkout.join("lane");
        git(
            &root.checkout,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        );
        git(
            &linked,
            &[
                "config",
                "--worktree",
                "--replace-all",
                "remote.origin.pushurl",
                scoped_push.origin.to_str().unwrap(),
            ],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.notices.iter().any(|notice| notice.contains("lane")
            && notice.contains("worktree-specific remote settings are not restored")));
        assert!(snapshot.notices.iter().any(|notice| notice.contains("sub:")
            && notice.contains("worktree-specific remote settings are not restored")));
        assert_eq!(
            snapshot.worktrees[0].push_evidence,
            PushEvidence::Resolved(vec![scoped_push.origin.to_str().unwrap().to_owned()])
        );
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(
                &restored.join("lane"),
                &["remote", "get-url", "--push", "origin"]
            ),
            primary_push.origin.to_str().unwrap()
        );
        assert_eq!(
            git(
                &restored.join("sub"),
                &["remote", "get-url", "--push", "origin"]
            ),
            scoped_push.origin.to_str().unwrap()
        );
    }

    #[test]
    fn three_stash_messages_survive() {
        let r = Repo::new();
        for message in ["one", "two", "three"] {
            fs::write(r.checkout.join("file"), message).unwrap();
            git(&r.checkout, &["stash", "push", "-m", message]);
        }
        let before = read_stash(&FileGit, &r.checkout).unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(read_stash(&FileGit, &r.checkout).unwrap(), before);
        let restored = r.restore(&snapshot, None);
        assert_eq!(read_stash(&FileGit, &restored).unwrap(), before);
    }
    // Hard-won: 42f6271ee: stash create writes and locks the real index even with optional locks disabled.
    #[test]
    fn staged_and_unstaged_edits_to_same_file_survive() {
        let r = Repo::new();
        fs::write(r.checkout.join("file"), "STAGED\nsecond\nthird\n").unwrap();
        git(&r.checkout, &["add", "file"]);
        fs::write(r.checkout.join("file"), "STAGED\nsecond\nUNSTAGED\n").unwrap();
        fs::write(r.checkout.join("ignored"), "ignored").unwrap();
        fs::write(r.checkout.join("untracked\n spaced"), "lost").unwrap();
        fs::write(r.checkout.join(".git/info/exclude"), "ignored\n").unwrap();
        let index = git(&r.checkout, &["diff", "--cached"]);
        let working = git(&r.checkout, &["diff"]);
        git(&r.checkout, &["config", "user.name", ""]);
        git(&r.checkout, &["config", "user.email", ""]);
        git(&r.checkout, &["config", "user.useConfigOnly", "true"]);
        assert_ne!(
            run_git(&FileGit, &r.checkout, ["stash", "create"], &[])
                .unwrap()
                .status,
            0
        );
        let index_path = git_path(&FileGit, &r.checkout, "index").unwrap();
        let index_before = fs::read(&index_path).unwrap();
        let lock = index_path.with_extension("lock");
        fs::write(&lock, "another Git process owns the index\n").unwrap();
        estimate_delta(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Raw { launch: None },
            None,
        )
        .unwrap();
        assert_eq!(fs::read(&index_path).unwrap(), index_before);
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert_eq!(fs::read(&index_path).unwrap(), index_before);
        assert_eq!(
            fs::read_to_string(&lock).unwrap(),
            "another Git process owns the index\n"
        );
        fs::remove_file(lock).unwrap();
        let inventory =
            workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(
            inventory.untracked,
            vec![UntrackedPath {
                checkout: CheckoutKey::default(),
                path: "untracked\n spaced".into(),
                bytes: 4,
                identity: file_identity(
                    &fs::symlink_metadata(r.checkout.join("untracked\n spaced")).unwrap()
                )
                .unwrap(),
            }]
        );
        assert_eq!(git(&r.checkout, &["diff", "--cached"]), index);
        assert_eq!(git(&r.checkout, &["diff"]), working);
        let restored = r.restore(&snapshot, None);
        assert_eq!(git(&restored, &["diff", "--cached"]), index);
        assert_eq!(git(&restored, &["diff"]), working);
        assert!(!restored.join("untracked\n spaced").exists());
        assert!(!restored.join("ignored").exists());
    }
    // Hard-won: 42f6271ee: newline worktree paths disappeared from line-delimited discovery.
    #[test]
    fn nested_detached_worktree_and_dirty_state_survive() {
        let r = Repo::new();
        fs::create_dir_all(r.checkout.join(".git/worktrees/incomplete")).unwrap();
        let outside = r.temp.path().join("outside\nworktree");
        git(
            &r.checkout,
            &["worktree", "add", "--detach", outside.to_str().unwrap()],
        );
        workspace_inventory(
            &crate::test_support::Git231,
            &r.checkout,
            &CheckoutOwnership::Managed,
        )
        .unwrap();
        let dependency = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("outside-root.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                dependency.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::UnrepresentedGitState { .. })
            ),
            "{dependency:#}"
        );
        assert!(dependency.to_string().contains("move it inside the clone"));
        let nested = r.checkout.join("lanes/nested\nline");
        git(
            &r.checkout,
            &["worktree", "add", "--detach", nested.to_str().unwrap()],
        );
        fs::write(nested.join("file"), "nested commit\n").unwrap();
        git(&nested, &["commit", "-am", "nested"]);
        fs::write(nested.join("file"), "nested working state\n").unwrap();
        let legacy = workspace_inventory(
            &crate::test_support::Git231,
            &r.checkout,
            &CheckoutOwnership::Managed,
        )
        .unwrap_err();
        assert!(
            legacy.to_string().contains("newline worktree paths"),
            "{legacy:#}"
        );
        git(
            &r.checkout,
            &["worktree", "remove", outside.to_str().unwrap()],
        );
        fs::remove_dir_all(r.checkout.join(".git/worktrees/incomplete")).unwrap();
        let encoded = serde_json::to_value(r.capture(&CheckoutOwnership::Managed)).unwrap();
        assert_eq!(
            encoded["worktrees"][0]["relative_path"],
            "lanes/nested\nline"
        );
        let snapshot: WorkspaceSnapshot = serde_json::from_value(encoded).unwrap();
        let inventory =
            workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert!(inventory.untracked.is_empty());
        assert_eq!(inventory.worktrees.len(), 1);
        let restored = r.restore(&snapshot, None).join("lanes/nested\nline");
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&nested, &["rev-parse", "HEAD"])
        );
        assert_eq!(git(&restored, &["diff"]), git(&nested, &["diff"]));
    }
    // Hard-won: 42f6271ee: raw operation capture rejected index state it deliberately skips.
    #[test]
    fn conflicted_merge_aborts_managed_and_leaves_raw_untouched() {
        let r = Repo::new();
        fs::write(r.checkout.join("base"), "hidden marker\n").unwrap();
        git(&r.checkout, &["add", "base"]);
        git(&r.checkout, &["commit", "-m", "hidden marker"]);
        r.conflict();
        r.start_conflict("merge");
        let before = fs::read(r.checkout.join("file")).unwrap();
        let estimate =
            estimate_delta(&FileGit, &r.checkout, &CheckoutOwnership::Managed, None).unwrap();
        assert!(estimate.working_state_bytes.is_none());
        assert!(r.checkout.join(".git/MERGE_HEAD").exists());
        fs::write(r.checkout.join("draft"), "intent during merge\n").unwrap();
        git(&r.checkout, &["add", "-N", "draft"]);
        git(&r.checkout, &["update-index", "--assume-unchanged", "base"]);
        let index_path = git_path(&FileGit, &r.checkout, "index").unwrap();
        let index_before = fs::read(&index_path).unwrap();
        let raw = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert_eq!(fs::read(&index_path).unwrap(), index_before);
        assert!(raw.working_state_skipped);
        git(
            &r.checkout,
            &["update-index", "--no-assume-unchanged", "base"],
        );
        git(&r.checkout, &["reset", "--", "draft"]);
        fs::remove_file(r.checkout.join("draft")).unwrap();
        assert!(r.checkout.join(".git/MERGE_HEAD").exists());
        assert_eq!(fs::read(r.checkout.join("file")).unwrap(), before);
        let managed = r.capture(&CheckoutOwnership::Managed);
        assert!(!managed.working_state_skipped);
        assert!(!r.checkout.join(".git/MERGE_HEAD").exists());
        let restored = r.restore(&managed, None);
        assert_eq!(fs::read(restored.join("file")).unwrap(), b"main version\n");
    }
    #[test]
    fn raw_launch_excludes_user_refs_and_stashes_but_keeps_nested_worktrees() {
        let r = Repo::new();
        git(&r.checkout, &["tag", "upstream-unchanged"]);
        git(&r.checkout, &["push", "origin", "upstream-unchanged"]);
        let origin_tag = git(&r.checkout, &["rev-parse", "refs/tags/upstream-unchanged"]);
        git(&r.checkout, &["branch", "user-branch"]);
        fs::write(r.checkout.join("file"), "old stash").unwrap();
        git(&r.checkout, &["stash", "push", "-m", "user stash"]);
        let legacy = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(legacy.refs.contains_key("refs/heads/user-branch"));
        assert_eq!(legacy.stash.len(), 1);
        let existing = r.checkout.join("user-worktree");
        git(
            &r.checkout,
            &["worktree", "add", "--detach", existing.to_str().unwrap()],
        );
        record_launch(&FileGit, &r.checkout, "session-1").unwrap();
        fs::write(existing.join("user-untracked"), "not session work").unwrap();
        fs::write(existing.join("file"), "nested edit\n").unwrap();
        let launch = LaunchRecord::read(&FileGit, &r.checkout, "session-1")
            .unwrap()
            .unwrap();
        r.commit("session commit\n");
        git(&r.checkout, &["branch", "agent-branch"]);
        fs::write(r.checkout.join("file"), "new stash").unwrap();
        git(&r.checkout, &["stash", "push", "-m", "agent stash"]);
        let snapshot = r.capture(&CheckoutOwnership::Raw {
            launch: Some(launch),
        });
        assert!(!snapshot.refs.contains_key("refs/heads/user-branch"));
        assert!(!snapshot.refs.contains_key("refs/tags/upstream-unchanged"));
        assert!(snapshot.refs.contains_key("refs/heads/agent-branch"));
        assert_eq!(snapshot.stash.len(), 1);
        assert!(snapshot.stash[0].message.ends_with("agent stash"));
        assert_eq!(snapshot.worktrees.len(), 1);
        let owned = CheckoutOwnership::Raw {
            launch: LaunchRecord::read(&FileGit, &r.checkout, "session-1").unwrap(),
        };
        let inventory = workspace_inventory(&FileGit, &r.checkout, &owned).unwrap();
        assert_eq!(
            inventory.worktrees[0].1.untracked[0].path,
            Path::new("user-untracked")
        );
        capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &owned,
            &r.temp.path().join("second.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            fs::read_to_string(restored.join("user-worktree/file")).unwrap(),
            "nested edit\n"
        );
        assert!(!restored.join("user-worktree/user-untracked").exists());
        assert_eq!(git(&restored, &["branch", "--list", "user-branch"]), "");
        assert_eq!(
            git(&restored, &["rev-parse", "refs/tags/upstream-unchanged"]),
            origin_tag
        );
    }
    #[test]
    fn unreachable_origin_uses_stale_remote_tracking_refs() {
        let r = Repo::new();
        r.commit("unpushed\n");
        let away = r.temp.path().join("away.git");
        fs::rename(&r.origin, &away).unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.remote_refs_stale);
        fs::create_dir(&r.origin).unwrap();
        let unresolvable = r.capture(&CheckoutOwnership::Managed);
        assert!(unresolvable.remote_refs_stale);
        assert!(
            unresolvable
                .notices
                .iter()
                .any(|notice| notice.contains("could not inspect local origin object storage"))
        );
        fs::remove_dir(&r.origin).unwrap();
        fs::rename(&away, &r.origin).unwrap();
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
    }
    // Hard-won: 2b451f9b4: checking only tips missed a rewritten-away parent.
    #[test]
    fn rewritten_origin_reports_missing_parent_object() {
        let r = Repo::new();
        r.commit("pushed then deleted\n");
        git(&r.checkout, &["push", "origin", "main"]);
        r.commit("unpublished child\n");
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_some());
        let base = git(&r.checkout, &["rev-parse", "HEAD~2"]);
        git(&r.origin, &["update-ref", "refs/heads/main", &base]);
        git(&r.origin, &["reflog", "expire", "--expire=now", "--all"]);
        git(&r.origin, &["gc", "--prune=now"]);
        let error = restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &r.temp.path().join("restored"),
            &clone_base(&snapshot, None),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("refs/heads/main"), "{error}");
        assert!(error.contains("history for"), "{error}");
        assert!(
            error.contains("origin may have rewritten history"),
            "{error}"
        );
    }
    #[test]
    fn shown_merge_consent_does_not_cover_rebase() {
        let r = Repo::new();
        r.conflict();
        r.start_conflict("rebase");
        let head = git(&r.checkout, &["rev-parse", "HEAD"]);
        let before = fs::read(r.checkout.join("file")).unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Shown(vec![ShownOperation {
                    checkout: CheckoutKey::default(),
                    kind: GitOperationKind::Merge,
                    identity: "previous merge".into(),
                }]),
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::OperationInProgress(ShownOperation {
                kind: GitOperationKind::Rebase,
                ..
            }))
        ));
        assert_eq!(git(&r.checkout, &["rev-parse", "HEAD"]), head);
        assert_eq!(fs::read(r.checkout.join("file")).unwrap(), before);
        assert_eq!(
            operation(&FileGit, &r.checkout).unwrap(),
            Some(GitOperationKind::Rebase)
        );
    }
    #[test]
    fn raw_rebase_records_original_branch_without_aborting() {
        let r = Repo::new();
        r.conflict();
        let original = git(&r.checkout, &["rev-parse", "HEAD"]);
        r.start_conflict("rebase");
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert_eq!(
            snapshot.head,
            SnapshotHead::Branch {
                name: "refs/heads/main".into(),
                commit: git(&r.checkout, &["rev-parse", "refs/heads/main"])
            }
        );
        assert_eq!(snapshot.refs["refs/heads/main"], original);
        assert!(snapshot.working_state_skipped);
        assert_eq!(
            operation(&FileGit, &r.checkout).unwrap(),
            Some(GitOperationKind::Rebase)
        );
        let restored = r.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", "HEAD"]), original);
    }
    #[test]
    fn detached_primary_unpushed_commit_survives() {
        let r = Repo::new();
        git(&r.checkout, &["checkout", "--detach"]);
        r.commit("detached unpushed\n");
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_some());
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
        assert_eq!(git(&restored, &["status", "--porcelain"]), "");
        assert!(matches!(
            read_head(&FileGit, &restored).unwrap(),
            SnapshotHead::Detached(_)
        ));
    }
    // Hard-won: 7469d2e8b: a collapsed untracked repository used only its directory's metadata for consent.
    #[test]
    fn untracked_repository_child_edit_invalidates_shown_discard_consent() {
        let root = Repo::new();
        let embedded = root.checkout.join("embedded");
        git(
            root.temp.path(),
            &[
                "clone",
                root.origin.to_str().unwrap(),
                embedded.to_str().unwrap(),
            ],
        );
        let directory_identity = file_identity(&fs::symlink_metadata(&embedded).unwrap()).unwrap();
        let inventory =
            workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
        let shown = inventory
            .untracked
            .iter()
            .map(|entry| UntrackedItem::Path {
                checkout: entry.checkout.clone(),
                path: entry.path.clone(),
                identity: entry.identity.clone(),
            })
            .collect::<Vec<_>>();
        fs::write(embedded.join("file"), "edited existing embedded file\n").unwrap();
        assert_eq!(
            file_identity(&fs::symlink_metadata(&embedded).unwrap()).unwrap(),
            directory_identity
        );
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("refused.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Shown(shown.clone()),
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items)) if !items.is_empty() && items != &shown),
            "{error:#}"
        );
        assert_eq!(
            fs::read_to_string(embedded.join("file")).unwrap(),
            "edited existing embedded file\n"
        );
    }

    // Hard-won: 7469d2e8b: linked HEAD keep entries escaped the current-state refusal after staged deletion.
    #[test]
    fn linked_worktree_head_under_keep_is_refused_after_staged_deletion() {
        let root = Repo::new();
        let keep = root.checkout.join(".mj/keep");
        fs::create_dir_all(&keep).unwrap();
        let linked = keep.join("lane");
        git(
            &root.checkout,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        );
        git(&linked, &["rm", "file"]);
        let input = RepositoryInput {
            in_container: false,
            id: "root".into(),
            checkout: root.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: root.temp.path().join("refused.pack"),
            exclusions: ExclusionSource::Origin,
            keep_dir: Some(keep),
        };
        let consent = SessionCaptureConsent {
            repositories: BTreeMap::from([(
                "root".into(),
                CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
            )]),
        };
        let error = capture_session_snapshot(&FileGit, &[input], &consent).unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::TrackedKeepFiles { paths, .. })
            if paths.iter().any(|entry| entry.checkout.checkout_path == Path::new(".mj/keep/lane") && entry.value == Path::new("file"))),
            "{error:#}"
        );
    }

    // Hard-won: d7c636759: explicit HTTP remote helpers leaked their underlying URL credentials.
    #[test]
    fn explicit_http_helper_urls_redact_metadata_and_omit_credential_rules() {
        let root = Repo::new();
        let origin = "https::https://ORIGIN_TOKEN@host/origin.git";
        let push = "https::https://PUSH_USER:PUSH_PASSWORD@host/publish.git";
        git(&root.checkout, &["remote", "set-url", "origin", origin]);
        git(
            &root.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", root.origin.display()),
                origin,
            ],
        );
        git(
            &root.checkout,
            &[
                "config",
                "url.https::https://RULE_TOKEN@host/.insteadOf",
                "unused:",
            ],
        );
        git(&root.checkout, &["config", "remote.origin.pushurl", push]);
        assert_eq!(
            git(&root.checkout, &["remote", "get-url", "origin"]),
            root.origin.to_str().unwrap()
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let json = serde_json::to_string(&snapshot).unwrap();
        for secret in ["ORIGIN_TOKEN", "PUSH_USER", "PUSH_PASSWORD", "RULE_TOKEN"] {
            assert!(!json.contains(secret), "metadata leaked {secret}");
        }
        assert_eq!(snapshot.origin_urls, vec!["https::https://host/origin.git"]);
        assert_eq!(snapshot.push_urls, vec!["https::https://host/publish.git"]);
        assert_eq!(
            snapshot.push_evidence,
            PushEvidence::Resolved(vec!["https::https://host/publish.git".to_owned()])
        );
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.contains("credential-bearing"))
        );
    }

    // Hard-won: d7c636759: metadata-only raw suspension incorrectly applied transfer/discard refusals.
    #[test]
    fn raw_without_origin_keeps_hidden_edits_dirty_modules_and_intent_to_add() {
        struct NoNetworkGit;
        impl GitCommandRunner for NoNetworkGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                assert!(!command.arguments.iter().any(|argument| {
                    ["fetch", "clone", "ls-remote"]
                        .iter()
                        .any(|verb| argument == verb)
                }));
                FileGit.run(repository, command)
            }
        }
        let root = Repo::new();
        let module = Repo::new();
        let module_dir = add_submodule(&root, &module);
        fs::write(root.checkout.join("hidden"), "original hidden file\n").unwrap();
        git(&root.checkout, &["add", "hidden"]);
        git(&root.checkout, &["commit", "-m", "hidden fixture"]);
        root.conflict();
        root.start_conflict("merge");
        git(&root.checkout, &["remote", "remove", "origin"]);
        git(
            &root.checkout,
            &["update-index", "--assume-unchanged", "hidden"],
        );
        fs::write(root.checkout.join("hidden"), "hidden edit stays\n").unwrap();
        fs::write(module_dir.join("file"), "module edit stays\n").unwrap();
        fs::write(root.checkout.join("draft"), "intent stays\n").unwrap();
        git(&root.checkout, &["add", "-N", "draft"]);
        fs::write(root.checkout.join("untracked"), "untracked stays\n").unwrap();
        let index = fs::read(root.checkout.join(".git/index")).unwrap();
        let module_index = fs::read(git_path(&FileGit, &module_dir, "index").unwrap()).unwrap();
        let merge = fs::read(root.checkout.join(".git/MERGE_HEAD")).unwrap();
        let repository = RepositoryInput {
            in_container: false,
            id: "raw".into(),
            checkout: root.checkout.clone(),
            ownership: CheckoutOwnership::Raw { launch: None },
            origin_override: None,
            pack_path: root.temp.path().join("unused.pack"),
            exclusions: ExclusionSource::Origin,
            keep_dir: Some(root.temp.path().join("outside-unused-keep")),
        };
        let inventory = session_inventory(&NoNetworkGit, &[repository]).unwrap();
        assert!(inventory["raw"].in_place_only && inventory["raw"].blockers.is_empty());
        assert!(
            inventory["raw"]
                .notices
                .iter()
                .any(|notice| notice.contains("only in place"))
        );
        let snapshot = capture_workspace_snapshot(
            &NoNetworkGit,
            &root.checkout,
            &CheckoutOwnership::Raw { launch: None },
            &root.temp.path().join("metadata.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap();
        assert!(
            snapshot.origin_urls.is_empty()
                && snapshot.pack.is_none()
                && snapshot.working_state.is_none()
        );
        assert!(snapshot.working_state_skipped && snapshot.has_uncommitted_state);
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.contains("resumes only in place"))
        );
        assert_eq!(fs::read(root.checkout.join(".git/index")).unwrap(), index);
        assert_eq!(
            fs::read(git_path(&FileGit, &module_dir, "index").unwrap()).unwrap(),
            module_index
        );
        assert_eq!(
            fs::read(root.checkout.join(".git/MERGE_HEAD")).unwrap(),
            merge
        );
        for (path, expected) in [
            (root.checkout.join("hidden"), "hidden edit stays\n"),
            (module_dir.join("file"), "module edit stays\n"),
            (root.checkout.join("draft"), "intent stays\n"),
            (root.checkout.join("untracked"), "untracked stays\n"),
        ] {
            assert_eq!(fs::read_to_string(path).unwrap(), expected);
        }
    }

    // Hard-won: d7c636759: intent-to-add could fail capture after an already-approved managed abort.
    #[test]
    fn intent_to_add_refuses_before_aborting_a_managed_merge() {
        let root = Repo::new();
        root.conflict();
        root.start_conflict("merge");
        fs::write(root.checkout.join("draft"), "intent payload\n").unwrap();
        git(&root.checkout, &["add", "-N", "draft"]);
        let index = fs::read(root.checkout.join(".git/index")).unwrap();
        let merge = fs::read(root.checkout.join(".git/MERGE_HEAD")).unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("refused.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::IntentToAdd(_))
            ),
            "{error:#}"
        );
        for step in ["draft", "git add", "git reset --"] {
            assert!(error.to_string().contains(step), "{error:#}");
        }
        assert_eq!(fs::read(root.checkout.join(".git/index")).unwrap(), index);
        assert_eq!(
            fs::read(root.checkout.join(".git/MERGE_HEAD")).unwrap(),
            merge
        );
        assert_eq!(
            fs::read_to_string(root.checkout.join("draft")).unwrap(),
            "intent payload\n"
        );
    }

    // Hard-won: d7c636759: complete provisioned restore resurrected absent branches/notes/durable refs.
    #[test]
    fn complete_snapshot_removes_absent_provisioned_local_namespaces() {
        let root = Repo::new();
        git(&root.checkout, &["checkout", "--detach"]);
        git(&root.checkout, &["branch", "-D", "main"]);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let destination = root.temp.path().join("provisioned");
        git(
            root.temp.path(),
            &[
                "clone",
                root.origin.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        git(
            &destination,
            &["notes", "add", "-m", "destination-only note"],
        );
        git(
            &destination,
            &["update-ref", "refs/mj/reviewed/destination", "HEAD"],
        );
        git(
            &destination,
            &["update-ref", "refs/mj/restore/inflight", "HEAD"],
        );
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        let refs = read_all_refs(&FileGit, &destination).unwrap();
        for reference in [
            "refs/heads/main",
            "refs/notes/commits",
            "refs/mj/reviewed/destination",
        ] {
            assert!(!refs.contains_key(reference));
        }
        assert_eq!(
            git(&destination, &["rev-parse", "refs/mj/restore/inflight"]),
            snapshot.head.commit()
        );
        assert_eq!(
            git(&destination, &["rev-parse", "HEAD"]),
            snapshot.head.commit()
        );
    }

    // Hard-won: 7469d2e8b: no-origin raw snapshots marked ordinary tracked edits as clean.
    #[test]
    fn raw_without_origin_is_in_place_only() {
        let r = Repo::new();
        git(&r.checkout, &["remote", "remove", "origin"]);
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(!snapshot.has_uncommitted_state);
        assert!(snapshot.origin_urls.is_empty());
        assert!(snapshot.pack.is_none());
        assert!(snapshot.working_state.is_none());
        assert_eq!(
            snapshot.head,
            SnapshotHead::Branch {
                name: "refs/heads/main".into(),
                commit: git(&r.checkout, &["rev-parse", "refs/heads/main"])
            }
        );
        let error = restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &r.temp.path().join("restored"),
            &clone_base(&snapshot, None),
        )
        .unwrap_err();
        assert!(error.to_string().contains("without an origin remote"));
        let clean = fs::read(r.checkout.join("file")).unwrap();
        fs::write(r.checkout.join("file"), "unstaged tracked edit\n").unwrap();
        let dirty = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(dirty.has_uncommitted_state);
        assert!(dirty.working_state.is_none());
        git(&r.checkout, &["add", "file"]);
        let index = git(&r.checkout, &["write-tree"]);
        fs::write(r.checkout.join("file"), &clean).unwrap();
        // Comparing only the working file to HEAD would miss this staged edit.
        let staged = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(staged.has_uncommitted_state);
        assert_eq!(git(&r.checkout, &["write-tree"]), index);
        assert_eq!(fs::read(r.checkout.join("file")).unwrap(), clean);
    }
    #[test]
    fn shown_untracked_consent_refuses_only_new_paths_before_mutation() {
        let r = Repo::new();
        r.conflict();
        r.start_conflict("merge");
        fs::write(r.checkout.join("shown"), "shown").unwrap();
        fs::write(r.checkout.join("new"), "new").unwrap();
        let before = fs::read(r.checkout.join("file")).unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Shown(vec![item_path(&r.checkout, "shown")]),
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        match error.downcast_ref::<SnapshotRefused>().unwrap() {
            SnapshotRefused::Untracked(paths) => {
                assert_eq!(paths, &vec![item_path(&r.checkout, "new")])
            }
            other => panic!("unexpected refusal {other:?}"),
        }
        assert!(r.checkout.join(".git/MERGE_HEAD").exists());
        assert_eq!(fs::read(r.checkout.join("file")).unwrap(), before);
        assert!(!r.temp.path().join("snapshot.pack").exists());
    }
    #[test]
    fn any_consent_covers_untracked_and_managed_operation() {
        let r = Repo::new();
        r.conflict();
        r.start_conflict("merge");
        fs::write(r.checkout.join("untracked"), "discard").unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(!r.checkout.join(".git/MERGE_HEAD").exists());
        assert!(r.checkout.join("untracked").exists());
        let restored = r.restore(&snapshot, None);
        assert!(!restored.join("untracked").exists());
        assert_eq!(fs::read(restored.join("file")).unwrap(), b"main version\n");
    }
    #[test]
    fn stash_untracked_parent_requires_consent_and_never_restores() {
        let r = Repo::new();
        record_launch(&FileGit, &r.checkout, "raw-stash").unwrap();
        let ownership = CheckoutOwnership::Raw {
            launch: LaunchRecord::read(&FileGit, &r.checkout, "raw-stash").unwrap(),
        };
        fs::write(r.checkout.join("file"), "tracked stash\n").unwrap();
        let mut state = 0xabcdef01_u32;
        let scratch: Vec<_> = (0..262_144)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        fs::write(r.checkout.join("secret"), scratch).unwrap();
        git(&r.checkout, &["stash", "push", "-u", "-m", "mixed stash"]);
        let original = read_stash(&FileGit, &r.checkout).unwrap();
        let estimate = estimate_delta(&FileGit, &r.checkout, &ownership, None).unwrap();
        assert!(
            estimate.total_bytes.unwrap() < 65_536,
            "scratch must not inflate the real transfer pack"
        );
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &ownership,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items)) if items == &vec![item_stash(original[0].commit.clone(), original[0].message.clone())])
        );
        let snapshot = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &ownership,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Shown(vec![item_stash(
                    original[0].commit.clone(),
                    original[0].message.clone(),
                )]),
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap();
        assert_eq!(
            estimate.total_bytes,
            Some(fs::metadata(snapshot.pack.as_ref().unwrap()).unwrap().len())
        );
        assert_ne!(snapshot.stash[0].commit, original[0].commit);
        assert_eq!(read_stash(&FileGit, &r.checkout).unwrap(), original);
        let restored = r.restore(&snapshot, None);
        assert!(!stash_has_untracked(&FileGit, &restored, &snapshot.stash[0].commit).unwrap());
        let missing = run_git(
            &FileGit,
            &restored,
            ["cat-file", "-e", &original[0].commit],
            &[],
        )
        .unwrap();
        assert_ne!(
            missing.status, 0,
            "original untracked stash must not travel"
        );
        git(&restored, &["stash", "apply"]);
        assert_eq!(fs::read(restored.join("file")).unwrap(), b"tracked stash\n");
        assert!(!restored.join("secret").exists());
    }
    #[test]
    fn nested_cherry_pick_abort_records_post_abort_shared_branch() {
        let r = Repo::new();
        git(&r.checkout, &["branch", "lane"]);
        let nested = r.checkout.join("lane");
        git(
            &r.checkout,
            &["worktree", "add", nested.to_str().unwrap(), "lane"],
        );
        r.commit("main first\n");
        let first = git(&r.checkout, &["rev-parse", "HEAD"]);
        r.commit("main second\n");
        let second = git(&r.checkout, &["rev-parse", "HEAD"]);
        fs::write(nested.join("file"), "nested change\n").unwrap();
        git(&nested, &["commit", "-am", "lane"]);
        let before = git(&nested, &["rev-parse", "HEAD"]);
        // First pick succeeds and moves the shared lane ref; the second conflicts.
        fs::write(r.checkout.join("extra"), "first pick\n").unwrap();
        git(&r.checkout, &["add", "extra"]);
        git(&r.checkout, &["commit", "-m", "independent"]);
        let independent = git(&r.checkout, &["rev-parse", "HEAD"]);
        let output = run_git(
            &FileGit,
            &nested,
            ["cherry-pick", &independent, &first, &second],
            &[],
        )
        .unwrap();
        assert_ne!(output.status, 0);
        assert_ne!(git(&nested, &["rev-parse", "HEAD"]), before);
        let raw = capture_workspace_snapshot(
            &FileGit,
            &nested,
            &CheckoutOwnership::Raw { launch: None },
            &r.temp.path().join("raw.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        assert_eq!(
            raw.head,
            SnapshotHead::Branch {
                name: "refs/heads/lane".into(),
                commit: before.clone()
            }
        );
        assert_eq!(raw.refs["refs/heads/lane"], before);
        let raw_destination = r.temp.path().join("raw-restored");
        restore_workspace_snapshot(&FileGit, &raw, &raw_destination, &clone_base(&raw, None))
            .unwrap();
        assert_eq!(git(&raw_destination, &["rev-parse", "HEAD"]), before);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(snapshot.refs["refs/heads/lane"], before);
        assert_eq!(git(&nested, &["rev-parse", "HEAD"]), before);
        let restored = r.restore(&snapshot, None).join("lane");
        assert_eq!(git(&restored, &["rev-parse", "HEAD"]), before);
        assert_eq!(git(&restored, &["status", "--porcelain"]), "");
    }

    #[test]
    fn raw_unchanged_unpushed_head_with_working_changes_survives() {
        let r = Repo::new();
        git(&r.checkout, &["switch", "-c", "preexisting"]);
        r.commit("preexisting unpublished\n");
        record_launch(&FileGit, &r.checkout, "raw-1").unwrap();
        let launch = LaunchRecord::read(&FileGit, &r.checkout, "raw-1").unwrap();
        fs::write(r.checkout.join("file"), "session edits\n").unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch });
        assert!(snapshot.refs.contains_key("refs/heads/preexisting"));
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
        assert_eq!(git(&restored, &["diff"]), git(&r.checkout, &["diff"]));
    }
    #[test]
    fn ref_move_during_working_capture_retries_the_pinned_cut() {
        struct MovingGit {
            moved: std::sync::atomic::AtomicBool,
        }
        impl GitCommandRunner for MovingGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                let output = SystemGit.run(repository, command)?;
                if git_arguments(command)
                    == [OsString::from("stash"), OsString::from("create")].as_slice()
                    && !self.moved.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    git(repository, &["commit", "-am", "concurrent commit"]);
                }
                Ok(output)
            }
        }
        let r = Repo::new();
        fs::write(r.checkout.join("file"), "concurrent edits\n").unwrap();
        let runner = MovingGit {
            moved: std::sync::atomic::AtomicBool::new(false),
        };
        let snapshot = capture_workspace_snapshot(
            &runner,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        assert!(runner.moved.load(std::sync::atomic::Ordering::SeqCst));
        assert!(snapshot.working_state.is_none());
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
        assert_eq!(
            fs::read(restored.join("file")).unwrap(),
            b"concurrent edits\n"
        );
        assert_eq!(git(&restored, &["status", "--porcelain"]), "");
    }
    // Hard-won: 2b451f9b4: relative local origins changed meaning at restore.
    #[test]
    fn relative_origin_and_push_paths_restore_from_another_directory() {
        let r = Repo::new();
        git(
            &r.checkout,
            &["remote", "set-url", "origin", "../origin.git"],
        );
        git(
            &r.checkout,
            &["config", "remote.origin.pushurl", "../origin.git"],
        );
        // Raw keeps the lexical origin route; managed capture offers its canonical remedy.
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(Path::new(snapshot.origin_urls.first().unwrap()).is_absolute());
        assert!(Path::new(&snapshot.push_urls[0]).is_absolute());
        let inside = tempfile::Builder::new()
            .prefix("snapshot-relative-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let destination = PathBuf::from(inside.path().file_name().unwrap()).join("restored");
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &clone_base(&snapshot, None),
        )
        .unwrap();
        assert_eq!(
            git(&destination, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
        git(
            &r.checkout,
            &["config", "remote.origin.pushurl", "../fork.git"],
        );
        git(
            &r.checkout,
            &[
                "config",
                "url.https://host/org/fork.git.insteadOf",
                "../fork.git",
            ],
        );
        let rewritten = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(Path::new(&rewritten.push_urls[0]).is_absolute());
    }
    #[test]
    fn embedded_http_credentials_never_enter_snapshot() {
        let r = Repo::new();
        let credentialed = "http://token:secret@127.0.0.1:1/unreachable.git";
        git(&r.checkout, &["remote", "set-url", "origin", credentialed]);
        git(
            &r.checkout,
            &["config", "remote.origin.pushurl", credentialed],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            snapshot.origin_urls.first().map(String::as_str),
            Some("http://127.0.0.1:1/unreachable.git")
        );
        assert_eq!(
            snapshot.push_urls,
            vec!["http://127.0.0.1:1/unreachable.git"]
        );
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("token"));
    }
    #[test]
    fn raw_capture_never_prunes_user_remote_tracking_refs() {
        let r = Repo::new();
        git(&r.checkout, &["branch", "deleted"]);
        git(&r.checkout, &["push", "origin", "deleted"]);
        let tracked = git(&r.checkout, &["rev-parse", "refs/remotes/origin/deleted"]);
        git(&r.origin, &["update-ref", "-d", "refs/heads/deleted"]);
        r.capture(&CheckoutOwnership::Raw { launch: None });
        assert_eq!(
            git(&r.checkout, &["rev-parse", "refs/remotes/origin/deleted"]),
            tracked
        );
    }
    #[test]
    fn snapshot_refuses_tracked_or_unpublished_submodule_changes() {
        let r = Repo::new();
        let sub = Repo::new();
        let nested_sub = Repo::new();
        git(
            &sub.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                nested_sub.origin.to_str().unwrap(),
                "nested",
            ],
        );
        git(&sub.checkout, &["commit", "-am", "nested submodule"]);
        git(&sub.checkout, &["push", "origin", "main"]);
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        let nested = r.checkout.join("sub");
        let clean = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            clean.initialized_submodules,
            vec![SnapshotSubmodule {
                origin_urls: vec![sub.origin.to_str().unwrap().to_owned()],
                push_urls: Vec::new(),
                object_format: "sha1".into(),
                worktree_config_enabled: false,
                url_rules: Vec::new(),
                worktree_url_rules: Vec::new(),

                path: "sub".into(),
                commit: git(&nested, &["rev-parse", "HEAD"]),
                name: "sub".into(),
                url: sub.origin.to_str().unwrap().to_owned(),
            }]
        );
        let restored = r.temp.path().join("clean-restored");
        restore_workspace_snapshot(&FileGit, &clean, &restored, &clone_base(&clean, None)).unwrap();
        assert_eq!(
            fs::read(restored.join("sub/file")).unwrap(),
            fs::read(nested.join("file")).unwrap()
        );
        assert!(
            !restored.join("sub/nested/.git").exists(),
            "uninitialized nested submodule must stay uninitialized"
        );
        git(&nested, &["update-index", "--assume-unchanged", "file"]);
        fs::write(nested.join("file"), "hidden edit\n").unwrap();
        let hidden = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("hidden.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(hidden.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::HiddenTrackedEdits(paths)) if paths.iter().any(|p| p.value == Path::new("sub/file"))));
        git(&nested, &["update-index", "--no-assume-unchanged", "file"]);
        fs::write(nested.join("file"), "dirty tracked\n").unwrap();
        let inventory =
            workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(
            inventory.dirty_submodules,
            vec![scoped(&CheckoutKey::default(), PathBuf::from("sub"))]
        );
        let consent = CaptureConsent {
            untracked: UntrackedConsent::Any,
            operations: OperationConsent::Any,
        };
        let capture = || {
            capture_workspace_snapshot(
                &FileGit,
                &r.checkout,
                &CheckoutOwnership::Managed,
                &r.temp.path().join("snapshot.pack"),
                &consent,
                None,
            )
        };
        let error = capture().unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::DirtySubmodule(path)) if path.value == Path::new("sub"))
        );
        assert!(
            error
                .to_string()
                .contains("commit and push inside the submodule")
        );
        git(&nested, &["commit", "-am", "unpushed submodule"]);
        git(&r.checkout, &["add", "sub"]);
        assert!(matches!(
            capture().unwrap_err().downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::DirtySubmodule(_))
        ));
        git(&nested, &["push", "origin", "HEAD:main"]);
        assert!(
            capture().is_ok(),
            "published checked-out submodule commit is durable"
        );
        git(&r.checkout, &["reset", "HEAD", "sub"]);
        let advanced = capture().unwrap();
        assert!(advanced.working_state.is_none());
        assert!(advanced.has_uncommitted_state);
        git(&r.checkout, &["add", "sub"]);
        let staged = capture().unwrap();
        assert!(staged.has_uncommitted_state);
        assert!(
            staged.working_state.is_none(),
            "Git ignores gitlink-only changes; explicit metadata preserves them"
        );
        let staged_destination = r.temp.path().join("staged-restored");
        restore_workspace_snapshot(
            &FileGit,
            &staged,
            &staged_destination,
            &clone_base(&staged, None),
        )
        .unwrap();
        let expected = git(&nested, &["rev-parse", "HEAD"]);
        assert_eq!(git(&staged_destination, &["rev-parse", ":sub"]), expected);
        assert_eq!(
            git(&staged_destination.join("sub"), &["rev-parse", "HEAD"]),
            expected
        );
        fs::write(nested.join("untracked"), "untracked submodule work").unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("scratch.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items)) if items == &vec![item_path(&r.checkout, "sub/untracked")])
        );
        capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("shown-scratch.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Shown(vec![item_path(&r.checkout, "sub/untracked")]),
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap();
    }
    // Hard-won: 2b451f9b4: clone-created refs blocked valid saved prefixes.
    #[test]
    fn saved_branch_and_tag_prefixes_replace_clone_local_namespace() {
        let r = Repo::new();
        git(&r.checkout, &["tag", "release"]);
        git(&r.checkout, &["push", "origin", "release"]);
        git(&r.checkout, &["tag", "-d", "release"]);
        git(&r.checkout, &["tag", "release/topic"]);
        git(&r.checkout, &["checkout", "--detach"]);
        git(&r.checkout, &["branch", "-D", "main"]);
        git(&r.checkout, &["switch", "-c", "main/topic"]);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["symbolic-ref", "HEAD"]),
            "refs/heads/main/topic"
        );
        assert_eq!(git(&restored, &["tag", "--list"]), "release/topic");
        let provisioned = r.temp.path().join("provisioned");
        git(
            r.temp.path(),
            &[
                "clone",
                r.origin.to_str().unwrap(),
                provisioned.to_str().unwrap(),
            ],
        );
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &provisioned,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert_eq!(
            git(&provisioned, &["symbolic-ref", "HEAD"]),
            "refs/heads/main/topic"
        );
        assert_eq!(git(&provisioned, &["tag", "--list"]), "release/topic");
    }
    #[test]
    fn source_override_restores_managed_checkout_without_origin() {
        let r = Repo::new();
        git(&r.checkout, &["remote", "remove", "origin"]);
        r.commit("local-source session\n");
        let snapshot = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            Some(r.origin.to_str().unwrap().to_owned()),
        )
        .unwrap();
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
    }
    // Hard-won: 1d7cc60a7: missing-parent recovery obeyed destination tag pruning after saved refs were installed.
    #[test]
    fn deleted_origin_branch_recovers_missing_tip_or_parent_by_sha() {
        for child in [false, true] {
            let r = Repo::new();
            git(
                &r.origin,
                &["config", "uploadpack.allowAnySHA1InWant", "true"],
            );
            let origin = url::Url::from_directory_path(&r.origin)
                .unwrap()
                .to_string();
            git(&r.checkout, &["remote", "set-url", "origin", &origin]);
            git(&r.checkout, &["switch", "-c", "feature"]);
            r.commit("published feature\n");
            git(&r.checkout, &["push", "origin", "feature"]);
            if child {
                r.commit("unpublished child\n");
            }
            git(&r.checkout, &["tag", "saved-local-only"]);
            git(&r.checkout, &["tag", "saved-conflict"]);
            git(
                &r.origin,
                &["update-ref", "refs/tags/saved-conflict", "refs/heads/main"],
            );
            let snapshot = r.capture(&CheckoutOwnership::Managed);
            assert_eq!(snapshot.pack.is_some(), child);
            git(&r.origin, &["update-ref", "-d", "refs/heads/feature"]);
            let host = host_git(r.temp.path(), "pruning-host");
            git(
                r.temp.path(),
                &[
                    "config",
                    "--file",
                    host.global.to_str().unwrap(),
                    "fetch.prune",
                    "true",
                ],
            );
            git(
                r.temp.path(),
                &[
                    "config",
                    "--file",
                    host.global.to_str().unwrap(),
                    "fetch.pruneTags",
                    "true",
                ],
            );
            let restored = r.temp.path().join("restored");
            restore_workspace_snapshot(&host, &snapshot, &restored, &clone_base(&snapshot, None))
                .unwrap();
            for name in ["refs/tags/saved-local-only", "refs/tags/saved-conflict"] {
                assert_eq!(git(&restored, &["rev-parse", name]), snapshot.refs[name]);
            }
            assert_eq!(
                git(&restored, &["rev-parse", "HEAD"]),
                git(&r.checkout, &["rev-parse", "HEAD"])
            );
            assert_eq!(
                git(&restored, &["rev-list", "HEAD"]),
                git(&r.checkout, &["rev-list", "HEAD"])
            );
        }
    }
    #[test]
    fn raw_reachable_origin_does_not_exclude_force_pushed_tracking_tip() {
        let r = Repo::new();
        r.commit("old published branch\n");
        git(&r.checkout, &["push", "origin", "main"]);
        let base = git(&r.checkout, &["rev-parse", "HEAD~"]);
        git(&r.origin, &["update-ref", "refs/heads/main", &base]);
        git(&r.origin, &["gc", "--prune=now"]);
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(snapshot.pack.is_some());
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&r.checkout, &["rev-parse", "HEAD"])
        );
    }
    #[test]
    fn raw_new_worktree_keeps_unchanged_preexisting_branch_head() {
        let r = Repo::new();
        git(&r.checkout, &["switch", "-c", "preexisting"]);
        r.commit("preexisting unpublished branch\n");
        git(&r.checkout, &["switch", "main"]);
        record_launch(&FileGit, &r.checkout, "raw-tree").unwrap();
        let ownership = CheckoutOwnership::Raw {
            launch: LaunchRecord::read(&FileGit, &r.checkout, "raw-tree").unwrap(),
        };
        let nested = r.checkout.join("new-worktree");
        git(
            &r.checkout,
            &["worktree", "add", nested.to_str().unwrap(), "preexisting"],
        );
        fs::write(nested.join("file"), "new lane edits\n").unwrap();
        let snapshot = r.capture(&ownership);
        assert!(snapshot.refs.contains_key("refs/heads/preexisting"));
        assert!(
            matches!(&snapshot.worktrees[0].head, SnapshotHead::Branch { name, commit } if name == "refs/heads/preexisting" && commit == &git(&nested, &["rev-parse", "HEAD"]))
        );
        let restored = r.restore(&snapshot, None).join("new-worktree");
        assert_eq!(
            git(&restored, &["symbolic-ref", "HEAD"]),
            "refs/heads/preexisting"
        );
        assert_eq!(git(&restored, &["diff"]), git(&nested, &["diff"]));
    }
    // Hard-won: 2b451f9b4: a stash added after inventory escaped discard consent.
    #[test]
    fn new_stash_after_inventory_requires_its_own_consent() {
        struct StashingGit {
            added: std::sync::atomic::AtomicBool,
        }
        impl GitCommandRunner for StashingGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                let output = SystemGit.run(repository, command)?;
                if git_arguments(command)
                    .first()
                    .is_some_and(|arg| arg == "for-each-ref")
                    && !self.added.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    fs::write(repository.join("file"), "new tracked stash\n")?;
                    fs::write(repository.join("new-untracked"), "new untracked stash\n")?;
                    git(repository, &["stash", "push", "-u", "-m", "new stash"]);
                }
                Ok(output)
            }
        }
        let r = Repo::new();
        let runner = StashingGit {
            added: std::sync::atomic::AtomicBool::new(false),
        };
        let error = capture_workspace_snapshot(
            &runner,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items)) if matches!(items.as_slice(), [UntrackedItem::Stash { message, .. }] if message.ends_with("new stash")))
        );
        assert_eq!(read_stash(&FileGit, &r.checkout).unwrap().len(), 1);
    }
    #[test]
    fn missing_blob_is_detected_by_full_graph_check() {
        struct MissingBlobGit {
            blob: String,
            removed: std::sync::atomic::AtomicBool,
        }
        impl GitCommandRunner for MissingBlobGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                if command.arguments.iter().any(|arg| arg == "--missing=print")
                    && !self.removed.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    // This fixture's small push is already unpacked into loose objects.
                    let path = git_path(
                        &FileGit,
                        repository,
                        &format!("objects/{}/{}", &self.blob[..2], &self.blob[2..]),
                    )?;
                    fs::remove_file(path)?;
                }
                SystemGit.run(repository, command)
            }
        }
        let r = Repo::new();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let runner = MissingBlobGit {
            blob: git(&r.checkout, &["rev-parse", "HEAD:file"]),
            removed: std::sync::atomic::AtomicBool::new(false),
        };
        let error = restore_workspace_snapshot(
            &runner,
            &snapshot,
            &r.temp.path().join("restored"),
            &clone_base(&snapshot, None),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(&runner.blob), "{error}");
        assert!(
            error.contains("origin may have rewritten history"),
            "{error}"
        );
    }
    #[test]
    fn hidden_assume_unchanged_edit_refuses_before_any_mutation() {
        let r = Repo::new();
        r.conflict();
        // Use an unrelated tracked path so the hidden flag can coexist with
        // a conflict and prove refusal precedes a consented operation abort.
        fs::write(r.checkout.join("hidden"), "committed\n").unwrap();
        git(&r.checkout, &["add", "hidden"]);
        git(&r.checkout, &["commit", "-m", "hidden fixture"]);
        r.start_conflict("merge");
        git(
            &r.checkout,
            &["update-index", "--assume-unchanged", "hidden"],
        );
        fs::write(r.checkout.join("hidden"), "hidden edits\n").unwrap();
        let inventory =
            workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(
            inventory.hidden_tracked_edits,
            vec![scoped(&CheckoutKey::default(), PathBuf::from("hidden"))]
        );
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::HiddenTrackedEdits(paths)) if paths == &vec![scoped(&CheckoutKey::default(), PathBuf::from("hidden"))])
        );
        assert!(r.checkout.join(".git/MERGE_HEAD").exists());
        assert_eq!(
            fs::read(r.checkout.join("hidden")).unwrap(),
            b"hidden edits\n"
        );
    }
    #[test]
    fn raw_detached_rebase_records_original_detached_commit() {
        let r = Repo::new();
        r.conflict();
        let original = git(&r.checkout, &["rev-parse", "HEAD"]);
        git(&r.checkout, &["checkout", "--detach"]);
        r.start_conflict("rebase");
        let transient = git(&r.checkout, &["rev-parse", "HEAD"]);
        assert_ne!(transient, original);
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert_eq!(snapshot.head, SnapshotHead::Detached(original.clone()));
        assert!(snapshot.working_state_skipped);
        assert_eq!(git(&r.checkout, &["rev-parse", "HEAD"]), transient);
        let restored = r.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", "HEAD"]), original);
        assert!(matches!(
            read_head(&FileGit, &restored).unwrap(),
            SnapshotHead::Detached(_)
        ));
    }
    #[test]
    fn shown_merge_identity_does_not_consent_to_a_replacement_merge() {
        let r = Repo::new();
        r.conflict();
        git(&r.checkout, &["switch", "-c", "alternate", "side"]);
        r.commit("alternate side\n");
        git(&r.checkout, &["switch", "main"]);
        r.start_conflict("merge");
        let shown = workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed)
            .unwrap()
            .operation
            .unwrap();
        git(&r.checkout, &["merge", "--abort"]);
        assert_ne!(
            run_git(&FileGit, &r.checkout, ["merge", "alternate"], &[])
                .unwrap()
                .status,
            0
        );
        let before = fs::read(r.checkout.join("file")).unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Shown(vec![shown.clone()]),
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::OperationInProgress(ShownOperation {
                kind: GitOperationKind::Merge,
                ..
            }))
        ));
        assert!(r.checkout.join(".git/MERGE_HEAD").exists());
        assert_eq!(fs::read(r.checkout.join("file")).unwrap(), before);
        for branch in ["other-main", "main"] {
            git(&r.checkout, &["merge", "--abort"]);
            if branch == "other-main" {
                git(&r.checkout, &["switch", "-c", branch]);
            } else {
                git(&r.checkout, &["switch", branch]);
            }
            r.start_conflict("merge");
            let current = workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed)
                .unwrap()
                .operation
                .unwrap();
            assert_ne!(current.identity, shown.identity);
            let error = capture_workspace_snapshot(
                &FileGit,
                &r.checkout,
                &CheckoutOwnership::Managed,
                &r.temp.path().join("occurrence.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Shown(vec![shown.clone()]),
                },
                None,
            )
            .unwrap_err();
            assert!(matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::OperationInProgress(_))
            ));
        }
    }
    #[test]
    fn staged_uninitialized_gitlink_is_kept_without_initializing_it() {
        let r = Repo::new();
        let sub = Repo::new();
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        git(&r.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(r.checkout.join(".git/modules/sub")).unwrap();
        sub.commit("new published gitlink\n");
        git(&sub.checkout, &["push", "origin", "main"]);
        let commit = git(&sub.checkout, &["rev-parse", "HEAD"]);
        git(
            &r.checkout,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{commit},sub"),
            ],
        );
        git(&r.checkout, &["config", "diff.ignoreSubmodules", "all"]);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.has_uncommitted_state);
        assert!(snapshot.initialized_submodules.is_empty());
        assert_eq!(
            snapshot.index_gitlinks,
            vec![SnapshotGitlink {
                path: "sub".into(),
                commit: commit.clone()
            }]
        );
        let restored = r.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", ":sub"]), commit);
        assert!(!restored.join("sub/.git").exists());
    }
    #[test]
    fn session_consent_checks_all_repositories_before_aborting_any() {
        let a = Repo::new();
        let b = Repo::new();
        a.conflict();
        a.start_conflict("merge");
        fs::write(a.checkout.join("notes.txt"), "approved only in a").unwrap();
        fs::write(b.checkout.join("notes.txt"), "not approved in b").unwrap();
        let mut repositories: Vec<_> = [("a", &a), ("b", &b)]
            .into_iter()
            .map(|(id, repo)| RepositoryInput {
                in_container: false,
                id: id.into(),
                checkout: repo.checkout.clone(),
                ownership: CheckoutOwnership::Managed,
                origin_override: None,
                pack_path: repo.temp.path().join("snapshot.pack"),
                exclusions: ExclusionSource::Origin,
                keep_dir: None,
            })
            .collect();
        let consent = SessionCaptureConsent {
            repositories: BTreeMap::from([
                (
                    "a".into(),
                    CaptureConsent {
                        untracked: UntrackedConsent::Shown(vec![scoped_item_path(
                            &a.checkout,
                            repository_key("a"),
                            "notes.txt",
                        )]),
                        operations: OperationConsent::Any,
                    },
                ),
                (
                    "b".into(),
                    CaptureConsent {
                        untracked: UntrackedConsent::Shown(vec![scoped_item_path(
                            &a.checkout,
                            repository_key("a"),
                            "notes.txt",
                        )]),
                        operations: OperationConsent::Any,
                    },
                ),
            ]),
        };
        let error = capture_session_snapshot(&FileGit, &repositories, &consent).unwrap_err();
        let refusal = error.downcast_ref::<SnapshotRefused>().unwrap();
        assert!(
            matches!(refusal, SnapshotRefused::Untracked(items) if items == &vec![scoped_item_path(&b.checkout, repository_key("b"), "notes.txt")])
        );
        assert!(a.checkout.join(".git/MERGE_HEAD").exists());
        let c = Repo::new();
        c.commit("shallow blocker\n");
        git(&c.checkout, &["push", "origin", "main"]);
        git(&c.checkout, &["fetch", "--depth=1", "origin"]);
        repositories.push(RepositoryInput {
            in_container: false,
            id: "c".into(),
            checkout: c.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: c.temp.path().join("blocked.pack"),
            exclusions: ExclusionSource::Origin,
            keep_dir: None,
        });
        let inventory = session_inventory(&FileGit, &repositories).unwrap();
        assert!(
            inventory["c"]
                .blockers
                .iter()
                .any(|blocker| matches!(blocker.kind, SnapshotRefused::ShallowRepository(_)))
        );
        let error =
            capture_session_snapshot(&FileGit, &repositories, &SessionCaptureConsent::default())
                .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::ShallowRepository(key)) if key.repository_id == "c"),
            "{error:#}"
        );
        assert!(a.checkout.join(".git/MERGE_HEAD").exists());
    }
    #[test]
    fn provisioned_restore_never_fetches_past_its_history_boundary() {
        struct NoNetworkGit;
        impl GitCommandRunner for NoNetworkGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                assert!(
                    !command
                        .arguments
                        .iter()
                        .any(|arg| ["clone", "fetch", "ls-remote"]
                            .iter()
                            .any(|verb| arg == verb)),
                    "provisioned restore contacted origin: {:?}",
                    command.arguments
                );
                SystemGit.run(repository, command)
            }
        }
        let r = Repo::new();
        let initial = git(&r.checkout, &["rev-parse", "HEAD"]);
        r.commit("later published history\n");
        git(&r.checkout, &["push", "origin", "main"]);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_none());
        let provisioned = r.temp.path().join("provisioned");
        git(r.temp.path(), &["init", provisioned.to_str().unwrap()]);
        git(
            &provisioned,
            &["remote", "add", "origin", r.origin.to_str().unwrap()],
        );
        git(&provisioned, &["fetch", "origin", &initial]);
        git(&provisioned, &["checkout", "--detach", &initial]);
        let error = restore_workspace_snapshot(
            &NoNetworkGit,
            &snapshot,
            &provisioned,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("origin may have rewritten history"),
            "{error:#}"
        );
        assert_eq!(git(&provisioned, &["rev-parse", "HEAD"]), initial);
    }
    #[test]
    fn submodule_remote_must_match_effective_restore_url_and_redacts_tokens() {
        let r = Repo::new();
        let sub = Repo::new();
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        let nested = r.checkout.join("sub");
        let fork = r.temp.path().join("fork.git");
        git(
            r.temp.path(),
            &[
                "clone",
                "--bare",
                sub.origin.to_str().unwrap(),
                fork.to_str().unwrap(),
            ],
        );
        git(&nested, &["remote", "add", "fork", fork.to_str().unwrap()]);
        fs::write(nested.join("file"), "only on fork\n").unwrap();
        git(&nested, &["commit", "-am", "fork-only"]);
        git(&nested, &["push", "fork", "HEAD:main"]);
        let consent = CaptureConsent {
            untracked: UntrackedConsent::Any,
            operations: OperationConsent::Any,
        };
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("snapshot.pack"),
            &consent,
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::DirtySubmodule(_))
        ));
        git(
            &r.checkout,
            &["config", "submodule.sub.url", fork.to_str().unwrap()],
        );
        assert!(matches!(
            capture_workspace_snapshot(
                &FileGit,
                &r.checkout,
                &CheckoutOwnership::Managed,
                &r.temp.path().join("still-wrong-origin.pack"),
                &consent,
                None
            )
            .unwrap_err()
            .downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::DirtySubmodule(_))
        ));
        git(
            &nested,
            &["remote", "set-url", "origin", fork.to_str().unwrap()],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let restored = r.temp.path().join("fork-restored");
        restore_workspace_snapshot(&FileGit, &snapshot, &restored, &clone_base(&snapshot, None))
            .unwrap();
        assert_eq!(
            git(&restored.join("sub"), &["rev-parse", "HEAD"]),
            git(&nested, &["rev-parse", "HEAD"])
        );
        let credentialed = "http://submodule-token:submodule-secret@127.0.0.1:1/fork.git";
        git(&r.checkout, &["config", "submodule.sub.url", credentialed]);
        git(&nested, &["remote", "set-url", "fork", credentialed]);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            snapshot.initialized_submodules[0].url,
            "http://127.0.0.1:1/fork.git"
        );
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("submodule-token"));
        assert!(!json.contains("submodule-secret"));
    }

    #[test]
    fn raw_untracked_file_added_during_pack_requires_fresh_consent() {
        struct WritingGit(PathBuf);
        impl GitCommandRunner for WritingGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                let output = SystemGit.run(repository, command)?;
                if command
                    .arguments
                    .iter()
                    .any(|argument| argument == "pack-objects")
                {
                    fs::write(self.0.join("new-during-pack"), "new").unwrap();
                }
                Ok(output)
            }
        }
        let r = Repo::new();
        r.commit("nonempty snapshot pack\n");
        let previous = r.capture(&CheckoutOwnership::Managed).pack.unwrap();
        let error = capture_workspace_snapshot(
            &WritingGit(r.checkout.clone()),
            &r.checkout,
            &CheckoutOwnership::Raw { launch: None },
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items)) if items == &vec![item_path(&r.checkout, "new-during-pack")])
        );
        assert!(
            previous.exists(),
            "a failed retry deleted an earlier successful pack"
        );
        let packs = fs::read_dir(r.temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "pack")
            })
            .collect::<Vec<_>>();
        assert_eq!(packs, vec![previous]);
    }
    #[test]
    fn cutoff_exclusions_pack_published_session_work_without_origin_fetch() {
        let r = Repo::new();
        let cutoff = git(&r.checkout, &["rev-parse", "HEAD"]);
        r.commit("published beyond cutoff\n");
        git(&r.checkout, &["push", "origin", "main"]);
        git(&r.checkout, &["remote", "remove", "origin"]);
        let repository = RepositoryInput {
            in_container: false,
            id: "cutoff".into(),
            checkout: r.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: Some(r.origin.to_str().unwrap().to_owned()),
            pack_path: r.temp.path().join("cutoff.pack"),
            exclusions: ExclusionSource::Commits(vec![cutoff.clone()]),
            keep_dir: None,
        };
        let consent = SessionCaptureConsent {
            repositories: BTreeMap::from([(
                "cutoff".into(),
                CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
            )]),
        };
        let estimate = estimate_repository_delta(&FileGit, &repository).unwrap();
        let session = capture_session_snapshot(&FileGit, &[repository], &consent).unwrap();
        let snapshot = &session.repositories["cutoff"];
        assert!(!snapshot.source_had_origin);
        assert!(snapshot.pack.is_some());
        assert_eq!(
            estimate.total_bytes,
            Some(fs::metadata(snapshot.pack.as_ref().unwrap()).unwrap().len())
        );
        let destination = r.temp.path().join("cutoff-base");
        git(r.temp.path(), &["init", destination.to_str().unwrap()]);
        git(
            &destination,
            &["fetch", r.origin.to_str().unwrap(), &cutoff],
        );
        git(&destination, &["checkout", "--detach", &cutoff]);
        restore_workspace_snapshot(
            &FileGit,
            snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert!(git(&destination, &["remote"]).is_empty());
        assert_eq!(
            git(&destination, &["rev-parse", "HEAD"]),
            snapshot.head.commit()
        );
        assert_eq!(
            fs::read(destination.join("file")).unwrap(),
            b"published beyond cutoff\n"
        );
    }
    #[test]
    fn linked_managed_capture_does_not_prune_the_users_common_directory() {
        let r = Repo::new();
        git(&r.checkout, &["branch", "deleted"]);
        git(&r.checkout, &["push", "origin", "deleted"]);
        let tracked = git(&r.checkout, &["rev-parse", "refs/remotes/origin/deleted"]);
        git(&r.origin, &["update-ref", "-d", "refs/heads/deleted"]);
        let legacy = r.temp.path().join("legacy");
        git(
            &r.checkout,
            &["worktree", "add", "-b", "session", legacy.to_str().unwrap()],
        );
        fs::write(r.checkout.join("file"), "user stash\n").unwrap();
        git(
            &r.checkout,
            &["stash", "push", "-m", "unrelated user stash"],
        );
        let module = Repo::new();
        add_submodule(&r, &module);
        let primary_module = r.checkout.join("sub");
        git(&primary_module, &["checkout", "--detach"]);
        fs::write(
            primary_module.join("file"),
            "unrelated primary module work\n",
        )
        .unwrap();
        git(
            &primary_module,
            &["commit", "-am", "unrelated primary module work"],
        );
        git(&r.checkout, &["rm", "-f", "sub"]);
        let scope = crate::archive::session_start_scope(&FileGit, &legacy).unwrap();
        let own_pin = crate::archive::push_review_base_ref(&scope);
        git(&legacy, &["update-ref", &own_pin, "HEAD"]);
        git(
            &legacy,
            &["update-ref", "refs/mj/reviewed/unrelated", "HEAD"],
        );
        let snapshot = capture_workspace_snapshot(
            &FileGit,
            &legacy,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("legacy.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        assert!(!snapshot.refs_complete);
        assert!(snapshot.stash.is_empty());
        assert!(!snapshot.refs.contains_key(&own_pin));
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.contains("fresh review baseline"))
        );
        assert!(snapshot.refs.contains_key("refs/heads/session"));
        assert!(!snapshot.refs.contains_key("refs/heads/main"));
        assert!(!snapshot.refs.contains_key("refs/mj/reviewed/unrelated"));
        assert_eq!(
            git(&r.checkout, &["rev-parse", "refs/remotes/origin/deleted"]),
            tracked
        );
    }
    #[test]
    fn capture_and_restore_need_no_user_identity_or_populated_home() {
        struct EmptyHomeGit {
            home: PathBuf,
        }
        impl GitCommandRunner for EmptyHomeGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                let mut command = command.clone();
                command.env.extend([
                    ("HOME".into(), self.home.as_os_str().to_owned()),
                    ("XDG_CONFIG_HOME".into(), self.home.as_os_str().to_owned()),
                    ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
                ]);
                SystemGit.run(repository, &command)
            }
        }
        let r = Repo::new();
        fs::write(r.checkout.join("file"), "saved tracked stash\n").unwrap();
        fs::write(r.checkout.join("scratch"), "drop").unwrap();
        git(&r.checkout, &["stash", "push", "-u", "-m", "without login"]);
        git(&r.checkout, &["config", "--unset", "user.name"]);
        git(&r.checkout, &["config", "--unset", "user.email"]);
        fs::write(r.checkout.join("file"), "working tracked edits\n").unwrap();
        let home = r.temp.path().join("empty-home");
        fs::create_dir(&home).unwrap();
        let runner = EmptyHomeGit { home };
        let snapshot = capture_workspace_snapshot(
            &runner,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("identity.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        let destination = r.temp.path().join("identity-restored");
        restore_workspace_snapshot(
            &runner,
            &snapshot,
            &destination,
            &clone_base(&snapshot, None),
        )
        .unwrap();
        assert_eq!(
            fs::read(destination.join("file")).unwrap(),
            b"working tracked edits\n"
        );
        assert_eq!(
            read_stash(&FileGit, &destination).unwrap()[0].message,
            snapshot.stash[0].message
        );
    }
    #[test]
    fn reachable_submodule_force_push_does_not_trust_stale_tracking_refs() {
        let r = Repo::new();
        let sub = Repo::new();
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        let module = r.checkout.join("sub");
        let original = git(&module, &["rev-parse", "HEAD"]);
        fs::write(module.join("file"), "formerly published\n").unwrap();
        git(&module, &["commit", "-am", "removed"]);
        git(&module, &["push", "origin", "HEAD:main"]);
        git(&r.checkout, &["add", "sub"]);
        git(&sub.origin, &["update-ref", "refs/heads/main", &original]);
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("submodule.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::DirtySubmodule(_))
        ));
    }
    // Hard-won: 2b451f9b4: removing third parents collapsed distinct stash entries.
    #[test]
    fn stash_replacements_keep_duplicate_tracked_states() {
        let r = Repo::new();
        for payload in ["first scratch", "second scratch"] {
            fs::write(r.checkout.join("file"), "same tracked state\n").unwrap();
            fs::write(r.checkout.join("scratch"), payload).unwrap();
            let output = SystemGit
                .run(
                    &r.checkout,
                    &GitCommand {
                        arguments: ["stash", "push", "-u", "-m", "same message"]
                            .into_iter()
                            .map(OsString::from)
                            .collect(),
                        env: [
                            ("GIT_AUTHOR_DATE", "@1000000000 +0000"),
                            ("GIT_COMMITTER_DATE", "@1000000000 +0000"),
                        ]
                        .into_iter()
                        .map(|(key, value)| (key.into(), value.into()))
                        .collect(),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(
                output.status,
                0,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let original = read_stash(&FileGit, &r.checkout).unwrap();
        assert_eq!(original.len(), 2);
        assert_ne!(original[0].commit, original[1].commit);
        let rendered = git(&r.checkout, &["stash", "list", "--format=%gs"]);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_ne!(snapshot.stash[0].commit, snapshot.stash[1].commit);
        let restored = r.restore(&snapshot, None);
        assert_eq!(git(&restored, &["stash", "list", "--format=%gs"]), rendered);
        for (saved, source) in snapshot.stash.iter().zip(original) {
            assert_eq!(
                git(&restored, &["show", "-s", "--format=%s", &saved.commit]),
                git(&r.checkout, &["show", "-s", "--format=%s", &source.commit])
            );
            assert!(
                git(&restored, &["show", "-s", "--format=%B", &saved.commit])
                    .contains(&format!("Mjolnir-Source-Stash: {}", source.commit))
            );
        }
    }
    #[test]
    fn shown_path_identity_refuses_a_replacement_file_at_the_same_name() {
        let r = Repo::new();
        let path = r.checkout.join("notes.txt");
        fs::write(&path, "shown").unwrap();
        let shown = item_path(&r.checkout, "notes.txt");
        // Keep the old inode alive, so even aggressive inode reuse cannot make
        // this fixture's replacement accidentally have the same identity.
        fs::rename(&path, r.temp.path().join("old-notes")).unwrap();
        fs::write(&path, "new!!").unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Raw { launch: None },
            &r.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Shown(vec![shown.clone()]),
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items)) if items == &vec![item_path(&r.checkout, "notes.txt")] && items[0] != shown)
        );
        assert!(error.to_string().contains("notes.txt"));
    }
    // Hard-won: a2932eae2: initialized staged deletions failed absorption; nested published positions were refused.
    #[test]
    fn nested_submodule_positions_and_initialized_staged_deletions_restore() {
        let root = Repo::new();
        let module = Repo::new();
        let leaf = Repo::new();
        add_submodule(&module, &leaf);
        let parent = add_submodule(&root, &module);
        git(
            &root.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "--recursive",
            ],
        );
        let nested = parent.join("sub");
        let pin = git(&parent, &["rev-parse", ":sub"]);
        leaf.commit("published nested checkout\n");
        git(&leaf.checkout, &["push", "origin", "main"]);
        let published = git(&leaf.checkout, &["rev-parse", "HEAD"]);
        git(&nested, &["fetch", "origin"]);
        git(&nested, &["checkout", "--detach", &published]);
        let encoded = serde_json::to_value(root.capture(&CheckoutOwnership::Managed)).unwrap();
        assert!(
            encoded["initialized_submodules"]
                .as_array()
                .unwrap()
                .iter()
                .any(|module| module["path"] == "sub/sub")
        );
        let snapshot: WorkspaceSnapshot = serde_json::from_value(encoded).unwrap();
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(&restored.join("sub/sub"), &["rev-parse", "HEAD"]),
            published
        );
        assert_eq!(git(&restored.join("sub"), &["rev-parse", ":sub"]), pin);
        fs::remove_dir_all(restored).unwrap();
        git(&root.checkout, &["rm", "--cached", "-f", "sub"]);
        git(&root.checkout, &["config", "diff.ignoreSubmodules", "all"]);
        let before = fs::read(parent.join("file")).unwrap();
        fs::write(parent.join("file"), "uncommitted removed-module file\n").unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("refused.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::DirtySubmodule(_))
            ),
            "{error:#}"
        );
        fs::write(parent.join("file"), before).unwrap();

        let deleted = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&deleted, None);
        assert!(git(&restored, &["ls-files", "--stage", "--", "sub"]).is_empty());
        assert_eq!(
            git(&restored.join("sub/sub"), &["rev-parse", "HEAD"]),
            published
        );
    }

    // Hard-won: a2932eae2: submodule index changes could escape the ref/HEAD cut check.
    #[test]
    fn submodule_gitlink_update_during_stash_retries_the_snapshot_cut() {
        struct SwitchingGit {
            root: PathBuf,
            commit: String,
            changed: std::sync::atomic::AtomicBool,
        }
        impl GitCommandRunner for SwitchingGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                if repository == self.root
                    && git_arguments(command).starts_with(&["stash".into(), "create".into()])
                    && !self.changed.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    git(
                        repository,
                        &[
                            "update-index",
                            "--cacheinfo",
                            &format!("160000,{},sub", self.commit),
                        ],
                    );
                }
                FileGit.run(repository, command)
            }
        }
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        module.commit("published changed gitlink\n");
        git(&module.checkout, &["push", "origin", "main"]);
        let commit = git(&module.checkout, &["rev-parse", "HEAD"]);
        let runner = SwitchingGit {
            root: root.checkout.clone(),
            commit: commit.clone(),
            changed: false.into(),
        };
        let snapshot = capture_workspace_snapshot(
            &runner,
            &root.checkout,
            &CheckoutOwnership::Raw { launch: None },
            &root.temp.path().join("race.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        assert_eq!(snapshot.index_gitlinks[0].commit, commit);
        let restored = root.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", ":sub"]), commit);
    }

    // Hard-won: a2932eae2: exact-tip evidence for uninitialized modules must come from .gitmodules, never cached forks.
    #[test]
    fn uninitialized_submodule_ignores_cached_forks_and_gives_an_init_step() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        module.commit("staged published ancestor\n");
        let staged = git(&module.checkout, &["rev-parse", "HEAD"]);
        module.commit("advanced tip\n");
        git(&module.checkout, &["push", "origin", "main"]);
        let tip = git(&module.checkout, &["rev-parse", "HEAD"]);
        git(
            &root.checkout,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{staged},sub"),
            ],
        );
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        let fork = root.temp.path().join("fork.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                fork.to_str().unwrap(),
            ],
        );
        git(&fork, &["update-ref", "refs/heads/main", &staged]);
        let admin = root.checkout.join(".git/modules/sub");
        git(
            &admin,
            &["remote", "set-url", "origin", fork.to_str().unwrap()],
        );
        for cached in [true, false] {
            if !cached {
                fs::remove_dir_all(&admin).unwrap();
            }
            let error = capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("no-proof.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("git submodule update --init sub"),
                "{error:#}"
            );
            assert!(
                format!("{error:#}").contains("unstage the pointer change"),
                "{error:#}"
            );
        }
        git(
            &root.checkout,
            &["update-index", "--cacheinfo", &format!("160000,{tip},sub")],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.initialized_submodules.is_empty());
        let restored = root.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", ":sub"]), tip);
        assert!(!restored.join("sub/.git").exists());
    }

    // Hard-won: a2932eae2: gitlink pathspec metacharacters selected sibling histories.
    #[test]
    fn submodule_paths_with_globs_use_only_their_own_history() {
        let root = Repo::new();
        for path in ["libs[1]", "libs1"] {
            let module = Repo::new();
            module.commit(path);
            git(&module.checkout, &["push", "origin", "main"]);
            git(
                &root.checkout,
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    module.origin.to_str().unwrap(),
                    path,
                ],
            );
            // Keep each origin alive after its fixture goes out of scope.
            let origin = root.temp.path().join(format!("{path}.git"));
            git(
                root.temp.path(),
                &[
                    "clone",
                    "--bare",
                    module.origin.to_str().unwrap(),
                    origin.to_str().unwrap(),
                ],
            );
            git(
                &root.checkout.join(path),
                &["remote", "set-url", "origin", origin.to_str().unwrap()],
            );
            git(
                &root.checkout,
                &[
                    "config",
                    &format!("submodule.{path}.url"),
                    origin.to_str().unwrap(),
                ],
            );
        }
        git(
            &root.checkout,
            &["commit", "-am", "registered literal modules"],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&snapshot, None);
        for path in ["libs[1]", "libs1"] {
            assert_eq!(
                git(&restored.join(path), &["rev-parse", "HEAD"]),
                git(&root.checkout.join(path), &["rev-parse", "HEAD"])
            );
        }
    }

    // Hard-won: a2932eae2: raw-operation conversion restored live submodule overlays.
    #[test]
    fn raw_merge_skips_uncommitted_submodule_checkout_positions() {
        let root = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&root, &module);
        let saved = git(&directory, &["rev-parse", "HEAD"]);
        root.conflict();
        module.commit("different published checkout\n");
        git(&module.checkout, &["push", "origin", "main"]);
        let live = git(&module.checkout, &["rev-parse", "HEAD"]);
        git(&directory, &["fetch", "origin"]);
        git(&directory, &["checkout", "--detach", &live]);
        root.start_conflict("merge");
        let snapshot = root.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(snapshot.working_state_skipped);
        let restored = root.restore(&snapshot, None);
        assert_eq!(git(&restored.join("sub"), &["rev-parse", "HEAD"]), saved);
        assert!(git(&restored, &["status", "--porcelain"]).is_empty());
    }

    // Hard-won: a2932eae2: lexical parent removal changed remotes that traversed symlinks.
    #[cfg(unix)]
    #[test]
    fn relative_remote_symlink_parent_keeps_fetch_and_push_destination() {
        let root = Repo::new();
        let target = Repo::new();
        std::os::unix::fs::symlink(&target.checkout, root.checkout.join("link")).unwrap();
        git(
            &root.checkout,
            &["remote", "set-url", "origin", "link/../origin.git"],
        );
        root.commit("work for symlink destination\n");
        // Raw keeps the lexical origin route; managed capture offers its canonical remedy.
        let snapshot = root.capture(&CheckoutOwnership::Raw { launch: None });
        assert_eq!(
            resolve_remote_path(&root.checkout, &snapshot.origin_urls[0]).unwrap(),
            target.origin.to_str().unwrap()
        );
        let restored = root.restore(&snapshot, None);
        git(&restored, &["push", "origin", "main"]);
        assert_eq!(
            git(&target.origin, &["rev-parse", "main"]),
            snapshot.head.commit()
        );
        assert_ne!(
            git(&root.origin, &["rev-parse", "main"]),
            snapshot.head.commit()
        );
    }

    // Hard-won: 3a4aef552: Git's configured pack splitting produced multiple snapshot artifacts.
    #[test]
    fn snapshot_and_estimate_override_configured_pack_splitting() {
        let root = Repo::new();
        git(&root.checkout, &["config", "pack.packSizeLimit", "1m"]);
        let mut state = 0x34ab_9812_76ff_1229_u64;
        for name in ["a", "b", "c", "d"] {
            let bytes = (0..600_000)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect::<Vec<_>>();
            fs::write(root.checkout.join(name), bytes).unwrap();
        }
        git(&root.checkout, &["add", "a", "b", "c", "d"]);
        git(&root.checkout, &["commit", "-m", "incompressible delta"]);
        let split = git(
            &root.checkout,
            &[
                "pack-objects",
                "--all",
                root.temp.path().join("split").to_str().unwrap(),
            ],
        );
        assert!(
            split.lines().count() > 1,
            "fixture did not exercise splitting"
        );
        let estimate =
            estimate_delta(&FileGit, &root.checkout, &CheckoutOwnership::Managed, None).unwrap();
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            estimate.total_bytes,
            Some(fs::metadata(snapshot.pack.as_ref().unwrap()).unwrap().len())
        );
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD^{tree}"]),
            git(&root.checkout, &["rev-parse", "HEAD^{tree}"])
        );
        git(&restored, &["fsck", "--full"]);
    }

    // Hard-won: 3a4aef552: the leaf submodule symlink could redirect restore into another checkout.
    #[cfg(unix)]
    #[test]
    fn submodule_restore_rejects_a_leaf_symlink_without_touching_its_target() {
        let root = Repo::new();
        let module = Repo::new();
        let outside = Repo::new();
        add_submodule(&root, &module);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let destination = root.temp.path().join("destination");
        fs::create_dir(&destination).unwrap();
        git(&destination, &["init"]);
        std::os::unix::fs::symlink(&outside.checkout, destination.join("sub")).unwrap();
        let refs = read_all_refs(&FileGit, &outside.checkout).unwrap();
        let config = fs::read(outside.checkout.join(".git/config")).unwrap();
        let index = fs::read(outside.checkout.join(".git/index")).unwrap();
        let error = restore_submodules(
            &FileGit,
            &destination,
            &snapshot.initialized_submodules,
            Some(&snapshot.index_gitlinks),
            true,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");
        assert_eq!(read_all_refs(&FileGit, &outside.checkout).unwrap(), refs);
        assert_eq!(
            fs::read(outside.checkout.join(".git/config")).unwrap(),
            config
        );
        assert_eq!(
            fs::read(outside.checkout.join(".git/index")).unwrap(),
            index
        );
    }

    // Hard-won: efc122a75: module names could escape the administrative module store.
    #[test]
    fn submodule_admin_name_cannot_attach_an_outside_repository() {
        let root = Repo::new();
        let module = Repo::new();
        let outside = Repo::new();
        add_submodule(&root, &module);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let destination = root.temp.path().join("destination");
        fs::create_dir(&destination).unwrap();
        git(&destination, &["init"]);
        let config = fs::read(outside.checkout.join(".git/config")).unwrap();
        let head = git(&outside.checkout, &["rev-parse", "HEAD"]);
        for name in [
            outside.checkout.join(".git").to_str().unwrap(),
            "../../outside",
        ] {
            let mut modules = snapshot.initialized_submodules.clone();
            modules[0].name = name.to_owned();
            let error = restore_submodules(
                &FileGit,
                &destination,
                &modules,
                Some(&snapshot.index_gitlinks),
                true,
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("unsafe submodule admin name"),
                "{error:#}"
            );
        }
        assert_eq!(
            fs::read(outside.checkout.join(".git/config")).unwrap(),
            config
        );
        assert_eq!(git(&outside.checkout, &["rev-parse", "HEAD"]), head);
        assert!(!destination.join("sub").exists());
    }

    // Hard-won: 3a4aef552: a parent-local rewrite could authorize an uninitialized mirror-only pin.
    #[test]
    fn uninitialized_submodule_probe_ignores_parent_local_mirror_rewrites() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        let mirror = root.temp.path().join("mirror.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        module.commit("published only on mirror\n");
        git(
            &module.checkout,
            &["push", mirror.to_str().unwrap(), "HEAD:main"],
        );
        let pin = git(&module.checkout, &["rev-parse", "HEAD"]);
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(root.checkout.join(".git/modules/sub")).unwrap();
        git(
            &root.checkout,
            &["update-index", "--cacheinfo", &format!("160000,{pin},sub")],
        );
        git(
            &root.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", mirror.display()),
                module.origin.to_str().unwrap(),
            ],
        );
        assert!(
            git(
                &root.checkout,
                &["ls-remote", module.origin.to_str().unwrap()]
            )
            .contains(&pin)
        );
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("mirror.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::DirtySubmodule(_))
            ),
            "{error:#}"
        );
        assert!(
            error
                .to_string()
                .contains("git submodule update --init sub")
        );
    }

    // Hard-won: 8251715a1: inactive submodule branches and stashes escaped snapshot loss checks.
    #[test]
    fn inactive_submodule_work_refuses_managed_capture_and_warns_raw() {
        let root = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&root, &module);
        let published = git(&directory, &["rev-parse", "HEAD"]);
        git(&directory, &["switch", "-c", "private"]);
        fs::write(directory.join("file"), "inactive unpublished work\n").unwrap();
        git(&directory, &["commit", "-am", "private work"]);
        git(&directory, &["checkout", "--detach", &published]);
        fs::write(directory.join("file"), "stashed work\n").unwrap();
        git(&directory, &["stash", "push", "-m", "private stash"]);
        let capture = |ownership| {
            capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                ownership,
                &root.temp.path().join("work.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
        };
        let refused = capture(&CheckoutOwnership::Managed).unwrap_err();
        assert!(matches!(
            refused.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::UnpublishedSubmoduleWork(_))
        ));
        for text in [
            "submodule sub",
            "refs/heads/private",
            "stash@{0}",
            "push",
            "drop",
        ] {
            assert!(refused.to_string().contains(text), "{refused:#}");
        }
        let raw = capture(&CheckoutOwnership::Raw { launch: None }).unwrap();
        assert!(
            raw.notices
                .iter()
                .any(|notice| notice.contains("refs/heads/private")
                    && notice.contains("stash@{0}")
                    && notice.contains("raw checkout is kept"))
        );
        git(&directory, &["push", "origin", "private"]);
        assert!(
            capture(&CheckoutOwnership::Managed)
                .unwrap_err()
                .to_string()
                .contains("stash@{0}")
        );
        git(&directory, &["stash", "drop"]);
        capture(&CheckoutOwnership::Managed).unwrap();
    }

    // Hard-won: 8251715a1: operation state could change after inventory without moving HEAD.
    #[test]
    fn raw_merge_started_during_capture_retries_with_working_state_skipped() {
        struct MergingGit {
            root: PathBuf,
            started: std::sync::atomic::AtomicBool,
        }
        impl GitCommandRunner for MergingGit {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                if repository == self.root
                    && git_arguments(command).starts_with(&["stash".into(), "create".into()])
                    && !self.started.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    git(repository, &["merge", "--no-commit", "--no-ff", "side"]);
                }
                FileGit.run(repository, command)
            }
        }
        let root = Repo::new();
        git(&root.checkout, &["switch", "-c", "side"]);
        fs::write(root.checkout.join("side"), "merge result\n").unwrap();
        git(&root.checkout, &["add", "side"]);
        git(&root.checkout, &["commit", "-m", "side"]);
        git(&root.checkout, &["checkout", "main"]);
        let runner = MergingGit {
            root: root.checkout.clone(),
            started: false.into(),
        };
        let snapshot = capture_workspace_snapshot(
            &runner,
            &root.checkout,
            &CheckoutOwnership::Raw { launch: None },
            &root.temp.path().join("merge.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        assert!(snapshot.working_state_skipped);
        assert!(snapshot.working_state.is_none());
        assert!(root.checkout.join(".git/MERGE_HEAD").exists());
        let restored = root.restore(&snapshot, None);
        assert!(!restored.join("side").exists());
        assert!(git(&restored, &["status", "--porcelain"]).is_empty());
    }

    // Hard-won: 8251715a1: canonicalizing a symlinked URL broke pushInsteadOf prefixes.
    #[cfg(unix)]
    #[test]
    fn symlinked_fetch_remote_preserves_textual_push_rewrites() {
        let root = Repo::new();
        fs::create_dir(root.temp.path().join("mirrors")).unwrap();
        fs::create_dir(root.temp.path().join("publish")).unwrap();
        std::os::unix::fs::symlink(&root.origin, root.temp.path().join("mirrors/repo.git"))
            .unwrap();
        let publication = root.temp.path().join("publish/repo.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                root.origin.to_str().unwrap(),
                publication.to_str().unwrap(),
            ],
        );
        let original = git(&root.origin, &["rev-parse", "main"]);
        git(
            &root.checkout,
            &["remote", "set-url", "origin", "../mirrors/repo.git"],
        );
        git(
            &root.checkout,
            &["config", "url.../publish/.pushInsteadOf", "../mirrors/"],
        );
        root.commit("published through rewrite\n");
        // Raw keeps the lexical origin route; managed capture offers its canonical remedy.
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Raw { launch: None },
            &root.temp.path().join("refused.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("anchoring origin push URL"),
            "{error:#}"
        );
        // Explicit absolute publication URLs retain Git's source behavior when
        // the original literal relative-prefix rewrite cannot be relocated.
        git(
            &root.checkout,
            &[
                "remote",
                "set-url",
                "--push",
                "origin",
                publication.to_str().unwrap(),
            ],
        );
        let snapshot = root.capture(&CheckoutOwnership::Raw { launch: None });
        let restored = root.restore(&snapshot, None);
        git(&restored, &["push", "origin", "main"]);
        assert_eq!(
            git(&publication, &["rev-parse", "main"]),
            snapshot.head.commit()
        );
        assert_eq!(git(&root.origin, &["rev-parse", "main"]), original);
    }

    // Hard-won: 8251715a1: URL parsing treated an SCP host as a non-hierarchical scheme.
    #[test]
    fn scp_default_remote_resolves_uninitialized_relative_module_urls() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        module.commit("new staged module tip\n");
        git(&module.checkout, &["push", "origin", "main"]);
        let tip = git(&module.checkout, &["rev-parse", "HEAD"]);
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(root.checkout.join(".git/modules/sub")).unwrap();
        git(
            &root.checkout,
            &["update-index", "--cacheinfo", &format!("160000,{tip},sub")],
        );
        git(
            &root.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.url",
                "../lib.git",
            ],
        );
        git(&root.checkout, &["add", ".gitmodules"]);
        git(
            &root.checkout,
            &["remote", "set-url", "origin", "git-host:team/app.git"],
        );
        git(
            &root.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", root.origin.display()),
                "git-host:team/app.git",
            ],
        );
        git(
            &root.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", module.origin.display()),
                "git-host:team/lib.git",
            ],
        );
        // A same-named directory relative to the checkout is a different repo.
        let decoy = root.temp.path().join("lib.git");
        git(
            root.temp.path(),
            &["init", "--bare", decoy.to_str().unwrap()],
        );
        let host = host_git(root.temp.path(), "scp-host-config");
        for (base, prefix) in [
            (&root.origin, "git-host:team/app.git"),
            (&module.origin, "git-host:team/lib.git"),
        ] {
            git(
                root.temp.path(),
                &[
                    "config",
                    "--file",
                    host.global.to_str().unwrap(),
                    &format!("url.{}.insteadOf", base.display()),
                    prefix,
                ],
            );
        }
        let snapshot = capture_workspace_snapshot(
            &host,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("scp.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        let restored = root.temp.path().join("restored");
        restore_workspace_snapshot(&host, &snapshot, &restored, &clone_base(&snapshot, None))
            .unwrap();
        assert_eq!(git(&restored, &["rev-parse", ":sub"]), tip);
        git(&restored, &["submodule", "init", "sub"]);
        assert_eq!(
            git(&restored, &["config", "submodule.sub.url"]),
            "git-host:team/lib.git"
        );
        assert!(!restored.join("sub/.git").exists());
    }

    // Hard-won: a2932eae2: failed tip downloads reused force-pushed-away tracking refs.
    #[test]
    fn advertised_but_unservable_tips_never_use_stale_tracking_evidence() {
        let r = Repo::new();
        let unavailable = "1234567890123456789012345678901234567890";
        // ls-remote advertises a loose ref without reading its object; upload-pack
        // cannot serve it. This exercises a successful query and a real fetch failure.
        fs::write(r.origin.join("refs/heads/main"), format!("{unavailable}\n")).unwrap();
        assert!(git(&r.checkout, &["ls-remote", "origin"]).contains(unavailable));
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(!snapshot.remote_refs_stale);
        assert!(snapshot.pack.is_some());
        let restored = r.temp.path().join("provisioned");
        fs::create_dir(&restored).unwrap();
        git(&restored, &["init"]);
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &restored,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            snapshot.head.commit()
        );
        git(&restored, &["fsck", "--no-reflogs"]);

        let root = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&root, &module);
        fs::write(
            module.origin.join("refs/heads/main"),
            format!("{unavailable}\n"),
        )
        .unwrap();
        assert!(git(&directory, &["ls-remote", "origin"]).contains(unavailable));
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("refused.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::DirtySubmodule(_))
            ),
            "{error:#}"
        );
    }

    // Hard-won: bae1ae4d3: custom origin refs excluded trees absent from a normal clone.
    #[test]
    fn custom_origin_refs_do_not_exclude_shared_trees() {
        let r = Repo::new();
        r.commit("only in custom origin ref\n");
        git(&r.checkout, &["push", "origin", "HEAD:refs/pull/1/head"]);
        git(
            &r.checkout,
            &["commit", "--amend", "-m", "distinct commit sharing tree"],
        );
        let file_origin = format!("file://{}", r.origin.display());
        git(&r.checkout, &["remote", "set-url", "origin", &file_origin]);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            fs::read_to_string(restored.join("file")).unwrap(),
            "only in custom origin ref\n"
        );
        git(&restored, &["fsck", "--no-reflogs"]);
    }

    // Hard-won: bae1ae4d3: clone tags resurrected a locally deleted managed tag.
    #[test]
    fn complete_and_raw_deleted_refs_do_not_resurrect_origin_tags() {
        let r = Repo::new();
        git(&r.checkout, &["tag", "deleted"]);
        git(&r.checkout, &["push", "origin", "deleted"]);
        record_launch(&FileGit, &r.checkout, "tag-session").unwrap();
        let launch = LaunchRecord::read(&FileGit, &r.checkout, "tag-session").unwrap();
        git(&r.checkout, &["tag", "-d", "deleted"]);
        let raw = r.capture(&CheckoutOwnership::Raw { launch });
        assert!(!raw.refs_complete);
        assert_eq!(raw.deleted_refs, vec!["refs/tags/deleted"]);
        let restored = r.restore(&raw, None);
        assert_eq!(git(&restored, &["tag", "--list", "deleted"]), "");
        fs::remove_dir_all(&restored).unwrap();
        let managed = r.capture(&CheckoutOwnership::Managed);
        assert!(managed.refs_complete);
        let restored = r.restore(&managed, None);
        assert_eq!(git(&restored, &["tag", "--list", "deleted"]), "");
    }

    // Hard-won: bae1ae4d3: worktree restore rejected a branch shared with the primary.
    #[test]
    fn forced_shared_branch_worktree_restores_independent_edits() {
        let r = Repo::new();
        let nested = r.checkout.join("nested");
        git(
            &r.checkout,
            &[
                "worktree",
                "add",
                "--force",
                nested.to_str().unwrap(),
                "main",
            ],
        );
        fs::write(nested.join("file"), "nested unstaged\n").unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored.join("nested"), &["symbolic-ref", "HEAD"]),
            "refs/heads/main"
        );
        assert_eq!(
            fs::read_to_string(restored.join("nested/file")).unwrap(),
            "nested unstaged\n"
        );
        assert_eq!(
            git(
                &restored,
                &["status", "--porcelain", "--untracked-files=no"]
            ),
            ""
        );
    }

    // Hard-won: bae1ae4d3: raw stash filtering discarded a newly stored old object.
    #[test]
    fn raw_new_stash_message_keeps_a_reused_launch_object() {
        let r = Repo::new();
        fs::write(r.checkout.join("file"), "old stash\n").unwrap();
        git(&r.checkout, &["stash", "push", "-m", "old"]);
        let old = git(&r.checkout, &["rev-parse", "refs/stash"]);
        record_launch(&FileGit, &r.checkout, "stash-session").unwrap();
        fs::write(r.checkout.join("file"), "new stash\n").unwrap();
        git(&r.checkout, &["stash", "push", "-m", "new"]);
        git(
            &r.checkout,
            &["stash", "store", "-m", "reused object", &old],
        );
        let snapshot = r.capture(&CheckoutOwnership::Raw {
            launch: LaunchRecord::read(&FileGit, &r.checkout, "stash-session").unwrap(),
        });
        assert_eq!(snapshot.stash.len(), 2);
        assert_eq!(snapshot.stash[0].message, "reused object");
        let restored = r.restore(&snapshot, None);
        assert_eq!(read_stash(&FileGit, &restored).unwrap(), snapshot.stash);
    }

    #[test]
    fn raw_origin_refresh_fetches_objects_without_changing_refs_or_fetch_head() {
        let r = Repo::new();
        record_launch(&FileGit, &r.checkout, "raw-refresh").unwrap();
        let launch = LaunchRecord::read(&FileGit, &r.checkout, "raw-refresh").unwrap();
        let before = read_all_refs(&FileGit, &r.checkout).unwrap();
        let fetch_head = r.checkout.join(".git/FETCH_HEAD");
        fs::write(&fetch_head, "unchanged sentinel\n").unwrap();
        let publisher = r.temp.path().join("publisher");
        git(
            r.temp.path(),
            &[
                "clone",
                r.origin.to_str().unwrap(),
                publisher.to_str().unwrap(),
            ],
        );
        fs::write(publisher.join("published"), "published after raw launch\n").unwrap();
        git(&publisher, &["add", "published"]);
        git(&publisher, &["commit", "-m", "published"]);
        git(&publisher, &["push", "origin", "main"]);
        let published = git(&publisher, &["rev-parse", "HEAD"]);
        r.commit("raw session\n");
        let expected_refs = read_all_refs(&FileGit, &r.checkout).unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch });
        assert_eq!(read_all_refs(&FileGit, &r.checkout).unwrap(), expected_refs);
        assert_eq!(
            fs::read_to_string(&fetch_head).unwrap(),
            "unchanged sentinel\n"
        );
        assert!(
            snapshot
                .refs
                .keys()
                .all(|name| !name.starts_with("refs/remotes/"))
        );
        assert!(!snapshot.remote_refs_stale);
        git(&r.checkout, &["cat-file", "-e", &published]);
        assert_eq!(
            before["refs/remotes/origin/main"],
            expected_refs["refs/remotes/origin/main"]
        );
        let restored = r.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", "origin/main"]), published);
    }

    // Hard-won: 37050d816: objects-only fetch must override tag pruning before refs are pinned.
    #[test]
    fn objects_only_fetch_preserves_unpublished_tags_under_prune_configuration() {
        let r = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&r, &module);
        for (source, origin, name) in [
            (&r.checkout, &r.origin, "root"),
            (&directory, &module.origin, "module"),
        ] {
            git(source, &["config", "fetch.prune", "true"]);
            git(source, &["config", "fetch.pruneTags", "true"]);
            git(source, &["config", "fetch.recurseSubmodules", "true"]);
            git(source, &["config", "remote.origin.prune", "true"]);
            git(source, &["config", "remote.origin.pruneTags", "true"]);
            git(source, &["tag", "private"]);
            let publisher = r.temp.path().join(format!("publisher-{name}"));
            git(
                r.temp.path(),
                &[
                    "clone",
                    origin.to_str().unwrap(),
                    publisher.to_str().unwrap(),
                ],
            );
            fs::write(publisher.join("advanced"), "advanced origin\n").unwrap();
            git(&publisher, &["add", "advanced"]);
            git(&publisher, &["commit", "-m", "advance origin"]);
            git(&publisher, &["push", "origin", "main"]);
        }
        let refs = read_all_refs(&FileGit, &r.checkout).unwrap();
        let module_refs = read_all_refs(&FileGit, &directory).unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            snapshot.refs["refs/tags/private"],
            refs["refs/tags/private"]
        );
        assert_eq!(read_all_refs(&FileGit, &r.checkout).unwrap(), refs);
        assert_eq!(read_all_refs(&FileGit, &directory).unwrap(), module_refs);
        assert!(!snapshot.remote_refs_stale);
    }

    #[test]
    fn unregistered_gitlinks_require_loss_consent_before_mutation() {
        let r = Repo::new();
        let nested = r.checkout.join("nested-repo");
        git(
            &r.checkout,
            &[
                "clone",
                r.origin.to_str().unwrap(),
                nested.to_str().unwrap(),
            ],
        );
        fs::write(nested.join("scratch"), "nested data\n").unwrap();
        git(&r.checkout, &["add", "nested-repo"]);
        let before = git(&r.checkout, &["write-tree"]);
        let inventory =
            workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(inventory.nested_repositories.len(), 1);
        assert!(inventory.nested_repositories[0].bytes > 12);
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("unregistered.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items))
            if matches!(&items[0], UntrackedItem::NestedRepository { path, .. } if path == Path::new("nested-repo")))
        );
        assert_eq!(git(&r.checkout, &["write-tree"]), before);
        assert!(nested.join("scratch").exists());
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let restored = r.restore(&snapshot, None);
        assert!(!restored.join("nested-repo/scratch").exists());
    }

    #[test]
    fn force_tracked_keep_files_refuse_even_with_discard_consent() {
        let r = Repo::new();
        fs::create_dir_all(r.checkout.join(".mj/keep")).unwrap();
        fs::write(r.checkout.join(".mj/keep/private"), "keep transport only\n").unwrap();
        git(&r.checkout, &["add", "-f", ".mj/keep/private"]);
        let repository = RepositoryInput {
            in_container: false,
            id: "root".into(),
            checkout: r.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: r.temp.path().join("keep.pack"),
            exclusions: ExclusionSource::Origin,
            keep_dir: Some(r.checkout.join(".mj/keep")),
        };
        let consent = SessionCaptureConsent {
            repositories: BTreeMap::from([(
                "root".into(),
                CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
            )]),
        };
        let error = capture_session_snapshot(&FileGit, std::slice::from_ref(&repository), &consent)
            .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::TrackedKeepFiles { keep_dir, paths })
            if keep_dir == &r.checkout.join(".mj/keep") && paths[0].value == Path::new(".mj/keep/private"))
        );
        assert!(r.checkout.join(".mj/keep/private").exists());
        assert!(!r.temp.path().join("keep.pack").exists());
        git(
            &r.checkout,
            &["stash", "push", "-m", "keep was force tracked"],
        );
        assert_eq!(git(&r.checkout, &["ls-files", ".mj/keep"]), "");
        let error = capture_session_snapshot(&FileGit, std::slice::from_ref(&repository), &consent)
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SnapshotRefused>(),
            Some(SnapshotRefused::TrackedKeepFiles { .. })
        ));
        git(&r.checkout, &["stash", "pop"]);
        git(&r.checkout, &["commit", "-m", "keep file was committed"]);
        let committed = git(&r.checkout, &["rev-parse", "HEAD"]);
        git(&r.checkout, &["rm", ".mj/keep/private"]);
        assert!(
            capture_session_snapshot(&FileGit, std::slice::from_ref(&repository), &consent)
                .is_err()
        );
        git(&r.checkout, &["commit", "-m", "remove keep file"]);
        // Historical tracked state remains faithful; only current HEAD/index
        // force-add mistakes are refused, not commits that already removed it.
        let session = capture_session_snapshot(&FileGit, &[repository], &consent).unwrap();
        let restored = r.restore(&session.repositories["root"], None);
        assert_eq!(
            git(
                &restored,
                &["show", &format!("{committed}:.mj/keep/private")]
            ),
            "keep transport only"
        );
    }

    // Hard-won: daa70eca2: anchoring removed literal dots and changed push-prefix matching.
    #[test]
    fn literal_dot_remote_does_not_gain_a_push_rewrite_match() {
        let root = Repo::new();
        let publisher = Repo::new();
        let mirror = root.checkout.join("repo.git");
        git(
            &root.checkout,
            &[
                "clone",
                "--bare",
                root.origin.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        git(
            &root.checkout,
            &["remote", "set-url", "origin", "./repo.git"],
        );
        git(
            &root.checkout,
            &[
                "config",
                &format!("url.{}.pushInsteadOf", publisher.origin.display()),
                "repo.git",
            ],
        );
        root.commit("new work for literal dot destination\n");
        assert_eq!(
            git(&root.checkout, &["remote", "get-url", "--push", "origin"]),
            "./repo.git"
        );
        // Raw keeps the lexical origin route; managed capture offers its canonical remedy.
        let snapshot = root.capture(&CheckoutOwnership::Raw { launch: None });
        let restored = root.restore(&snapshot, None);
        git(&restored, &["push", "origin", "main"]);
        assert_eq!(git(&mirror, &["rev-parse", "main"]), snapshot.head.commit());
        assert_ne!(
            git(&publisher.origin, &["rev-parse", "main"]),
            snapshot.head.commit()
        );
    }

    // Hard-won: daa70eca2: absorbgitdirs' temporary gitlink erased a staged regular-file replacement.
    #[test]
    fn staged_regular_file_replacing_initialized_gitlink_restores_its_index_entry() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        let held = root.temp.path().join("held-module");
        fs::rename(root.checkout.join("sub"), &held).unwrap();
        git(&root.checkout, &["rm", "--cached", "sub"]);
        fs::write(root.checkout.join("sub"), "staged replacement file\n").unwrap();
        git(&root.checkout, &["add", "sub"]);
        fs::remove_file(root.checkout.join("sub")).unwrap();
        fs::rename(held, root.checkout.join("sub")).unwrap();
        let staged = git(&root.checkout, &["ls-files", "--stage", "sub"]);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&snapshot, None);
        assert_eq!(git(&restored, &["ls-files", "--stage", "sub"]), staged);
        assert_eq!(
            git(&restored.join("sub"), &["rev-parse", "HEAD"]),
            snapshot.initialized_submodules[0].commit
        );
    }

    // Hard-won: 811fb3d48: partial copy left a final pack name that poisoned provisioned retries.
    #[test]
    fn interrupted_snapshot_pack_copy_publishes_nothing_and_can_retry() {
        struct Interrupted {
            file: fs::File,
            remaining: usize,
        }
        impl Read for Interrupted {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.remaining == 0 {
                    return Err(std::io::Error::other("injected interrupted copy"));
                }
                let length = buffer.len().min(self.remaining);
                let read = self.file.read(&mut buffer[..length])?;
                self.remaining -= read;
                Ok(read)
            }
        }
        let root = Repo::new();
        let mut state = 0x2cab_93de_1942_3344_u64;
        let bytes = (0..200_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect::<Vec<_>>();
        fs::write(root.checkout.join("file"), bytes).unwrap();
        git(&root.checkout, &["commit", "-am", "incompressible pack"]);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let pack = snapshot.pack.as_ref().unwrap();
        let directory = root.temp.path().join("publication");
        fs::create_dir(&directory).unwrap();
        let final_path = directory.join("snapshot.pack");
        let mut interrupted = Interrupted {
            file: fs::File::open(pack).unwrap(),
            remaining: 65_537,
        };
        assert!(install_snapshot_pack(&mut interrupted, &final_path).is_err());
        assert!(!final_path.exists());
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        install_snapshot_pack(&mut fs::File::open(pack).unwrap(), &final_path).unwrap();
        git(
            &root.checkout,
            &["index-pack", final_path.to_str().unwrap()],
        );
        assert_eq!(fs::read(final_path).unwrap(), fs::read(pack).unwrap());
    }

    // Hard-won: 0984f61a3: indexed descendants blocked initialized module restoration.
    #[test]
    fn staged_directory_replacing_initialized_gitlink_restores_all_index_entries() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        let held = root.temp.path().join("held-module");
        fs::rename(root.checkout.join("sub"), &held).unwrap();
        git(&root.checkout, &["rm", "--cached", "sub"]);
        fs::create_dir_all(root.checkout.join("sub/deeper")).unwrap();
        fs::write(root.checkout.join("sub/one"), "first staged replacement\n").unwrap();
        fs::write(
            root.checkout.join("sub/deeper/two"),
            "second staged replacement\n",
        )
        .unwrap();
        git(&root.checkout, &["add", "sub"]);
        fs::remove_dir_all(root.checkout.join("sub")).unwrap();
        fs::rename(held, root.checkout.join("sub")).unwrap();
        fs::write(
            root.checkout.join("sub/one"),
            "first unstaged replacement\n",
        )
        .unwrap();
        fs::create_dir_all(root.checkout.join("sub/deeper")).unwrap();
        fs::write(
            root.checkout.join("sub/deeper/two"),
            "second unstaged replacement\n",
        )
        .unwrap();
        let working = git(&root.checkout, &["diff"]);
        let module_status = git(&root.checkout.join("sub"), &["status", "--porcelain"]);
        let staged = git(&root.checkout, &["ls-files", "--stage", "sub"]);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&snapshot, None);
        assert_eq!(git(&restored, &["ls-files", "--stage", "sub"]), staged);
        assert_eq!(
            git(&restored.join("sub"), &["rev-parse", "HEAD"]),
            snapshot.initialized_submodules[0].commit
        );
        assert_eq!(git(&restored, &["diff"]), working);
        assert_eq!(
            git(&restored.join("sub"), &["status", "--porcelain"]),
            module_status
        );
        assert_eq!(
            fs::read_to_string(restored.join("sub/one")).unwrap(),
            "first unstaged replacement\n"
        );
    }

    // Hard-won: 0984f61a3: raw sequencer HEADs at different commits collapsed a shared branch.
    #[test]
    fn raw_sequencer_primary_owns_branch_and_conflicting_nested_head_restores_detached() {
        for primary_owns in [true, false] {
            let root = Repo::new();
            git(&root.checkout, &["switch", "-c", "topic"]);
            fs::write(root.checkout.join("extra"), "first pick\n").unwrap();
            git(&root.checkout, &["add", "extra"]);
            git(&root.checkout, &["commit", "-m", "first pick"]);
            let first = git(&root.checkout, &["rev-parse", "HEAD"]);
            root.commit("topic conflict\n");
            let second = git(&root.checkout, &["rev-parse", "HEAD"]);
            git(&root.checkout, &["switch", "main"]);
            root.commit("main conflict\n");
            let start = git(&root.checkout, &["rev-parse", "HEAD"]);
            let active = if primary_owns {
                root.checkout.clone()
            } else {
                git(&root.checkout, &["switch", "-c", "primary"]);
                let active = root.checkout.join("a-owner");
                git(
                    &root.checkout,
                    &["worktree", "add", active.to_str().unwrap(), "main"],
                );
                active
            };
            let nested = root.checkout.join("z-other");
            git(
                &root.checkout,
                &[
                    "worktree",
                    "add",
                    "--force",
                    nested.to_str().unwrap(),
                    "main",
                ],
            );
            let picked = run_git(&FileGit, &active, ["cherry-pick", &first, &second], &[]).unwrap();
            assert_ne!(picked.status, 0);
            let current = git(&nested, &["rev-parse", "HEAD"]);
            assert_ne!(start, current);
            let nested_index = git(&nested, &["write-tree"]);
            let snapshot = root.capture(&CheckoutOwnership::Raw { launch: None });
            assert_eq!(snapshot.refs["refs/heads/main"], start);
            assert_eq!(
                snapshot
                    .worktrees
                    .iter()
                    .find(|tree| tree.relative_path == Path::new("z-other"))
                    .unwrap()
                    .head,
                SnapshotHead::Detached(current.clone())
            );
            assert!(
                snapshot
                    .notices
                    .iter()
                    .any(|notice| notice.contains("z-other")
                        && notice.contains(if primary_owns {
                            "primary checkout"
                        } else {
                            "a-owner"
                        })
                        && notice.contains("refs/heads/main")
                        && notice.contains(&start)
                        && notice.contains(&current))
            );
            assert_eq!(git(&active, &["rev-parse", "HEAD"]), current);
            let restored = root.restore(&snapshot, None);
            assert_eq!(git(&restored, &["rev-parse", "HEAD"]), start);
            assert_eq!(
                git(&restored, &["symbolic-ref", "HEAD"]),
                if primary_owns {
                    "refs/heads/main"
                } else {
                    "refs/heads/primary"
                }
            );
            let restored_owner = if primary_owns {
                restored.clone()
            } else {
                restored.join("a-owner")
            };
            assert_eq!(git(&restored_owner, &["rev-parse", "HEAD"]), start);
            assert_eq!(
                git(&restored_owner, &["symbolic-ref", "HEAD"]),
                "refs/heads/main"
            );
            assert_eq!(
                git(&restored.join("z-other"), &["rev-parse", "HEAD"]),
                current
            );
            assert_ne!(
                run_git(
                    &FileGit,
                    &restored.join("z-other"),
                    ["symbolic-ref", "--quiet", "HEAD"],
                    &[]
                )
                .unwrap()
                .status,
                0
            );
            assert_eq!(
                git(&restored.join("z-other"), &["write-tree"]),
                nested_index
            );
            assert!(!restored.join("extra").exists());
        }
    }

    // Hard-won: 82cc09103: an attached rebase read a shared branch moved by its forced sibling.
    #[test]
    fn raw_attached_rebase_keeps_orig_head_when_forced_sibling_moves_branch() {
        let root = Repo::new();
        root.conflict();
        let original = git(&root.checkout, &["rev-parse", "HEAD"]);
        let nested = root.checkout.join("sibling");
        git(
            &root.checkout,
            &[
                "worktree",
                "add",
                "--force",
                nested.to_str().unwrap(),
                "main",
            ],
        );
        root.start_conflict("rebase");
        let conflict = fs::read(root.checkout.join("file")).unwrap();
        fs::write(nested.join("extra"), "sibling branch advance\n").unwrap();
        git(&nested, &["add", "extra"]);
        git(&nested, &["commit", "-m", "sibling branch advance"]);
        let advanced = git(&nested, &["rev-parse", "HEAD"]);
        assert_ne!(advanced, original);
        struct CountWorkingCaptures {
            sibling: PathBuf,
            calls: std::sync::atomic::AtomicUsize,
        }
        impl GitCommandRunner for CountWorkingCaptures {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                let args = git_arguments(command);
                if repository == self.sibling
                    && args.first().is_some_and(|arg| arg == "stash")
                    && args.get(1).is_some_and(|arg| arg == "create")
                {
                    self.calls
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                FileGit.run(repository, command)
            }
        }
        let runner = CountWorkingCaptures {
            sibling: nested.clone(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let snapshot = capture_workspace_snapshot(
            &runner,
            &root.checkout,
            &CheckoutOwnership::Raw { launch: None },
            &root.temp.path().join("rebase-stable.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        assert_eq!(
            runner.calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "a stable source cut must not retry because snapshot refs differ"
        );
        assert_eq!(snapshot.head.commit(), original);
        assert_eq!(snapshot.refs["refs/heads/main"], original);
        assert_eq!(
            snapshot.worktrees[0].head,
            SnapshotHead::Detached(advanced.clone())
        );
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.contains("sibling")
                    && notice.contains("refs/heads/main")
                    && notice.contains(&original)
                    && notice.contains(&advanced))
        );
        assert!(operation(&FileGit, &root.checkout).unwrap().is_some());
        assert_eq!(fs::read(root.checkout.join("file")).unwrap(), conflict);
        assert_eq!(
            git(&root.checkout, &["rev-parse", "refs/heads/main"]),
            advanced
        );
        let restored = root.restore(&snapshot, None);
        assert_eq!(git(&restored, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        assert_eq!(git(&restored, &["rev-parse", "HEAD"]), original);
        assert_eq!(
            git(&restored.join("sibling"), &["rev-parse", "HEAD"]),
            advanced
        );
        assert!(matches!(
            read_head(&FileGit, &restored.join("sibling")).unwrap(),
            SnapshotHead::Detached(_)
        ));
        assert_eq!(
            fs::read_to_string(restored.join("sibling/extra")).unwrap(),
            "sibling branch advance\n"
        );
    }

    // Hard-won: 4dba9ad05: cache-first traversal resolved a live child's relative origin from its admin.
    #[cfg(unix)]
    #[test]
    fn initialized_nested_module_resolves_relative_origin_before_cached_loss_scan() {
        let root = Repo::new();
        let parent = Repo::new();
        let child = Repo::new();
        add_submodule(&parent, &child);
        add_submodule(&root, &parent);
        git(
            &root.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "--recursive",
            ],
        );
        let live = root.checkout.join("sub/sub");
        child.commit("new published child position\n");
        git(&child.checkout, &["push", "origin", "main"]);
        git(&live, &["fetch", "origin", "main"]);
        let commit = git(&child.checkout, &["rev-parse", "HEAD"]);
        git(&live, &["checkout", "--detach", &commit]);
        git(&live, &["update-ref", "-d", "refs/remotes/origin/main"]);
        std::os::unix::fs::symlink(&child.origin, live.join("publisher.git")).unwrap();
        git(&live, &["remote", "set-url", "origin", "publisher.git"]);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            snapshot
                .initialized_submodules
                .iter()
                .find(|entry| entry.path == Path::new("sub/sub"))
                .unwrap()
                .commit,
            commit
        );
    }

    // Hard-won: 4dba9ad05: provisioned direct URL rules won equal-prefix ties and accumulated on retry.
    #[test]
    fn provisioned_url_rules_replace_direct_scopes_and_preserve_destination_includes() {
        let root = Repo::new();
        let good = root.temp.path().join("good.git");
        let bad = root.temp.path().join("bad.git");
        for path in [&good, &bad] {
            git(
                root.temp.path(),
                &[
                    "clone",
                    "--bare",
                    root.origin.to_str().unwrap(),
                    path.to_str().unwrap(),
                ],
            );
        }
        let local = format!("url.{}.pushInsteadOf", good.display());
        let scoped = format!("url.{}.insteadOf", good.display());
        git(&root.checkout, &["config", &local, "publish:"]);
        git(&root.checkout, &["remote", "set-url", "origin", "publish:"]);
        git(
            &root.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", root.origin.display()),
                "publish:",
            ],
        );
        assert_eq!(
            git(&root.checkout, &["remote", "get-url", "--push", "origin"]),
            good.to_str().unwrap()
        );
        git(
            &root.checkout,
            &["config", "extensions.worktreeConfig", "true"],
        );
        git(
            &root.checkout,
            &["config", "--worktree", &scoped, "scoped:"],
        );
        root.commit("snapshot rule destination\n");
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let destination = root.temp.path().join("provisioned");
        git(
            root.temp.path(),
            &[
                "clone",
                root.origin.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        git(
            &destination,
            &[
                "config",
                &format!("url.{}.pushInsteadOf", bad.display()),
                "publish:",
            ],
        );
        git(
            &destination,
            &["config", "extensions.worktreeConfig", "true"],
        );
        git(
            &destination,
            &[
                "config",
                "--worktree",
                &format!("url.{}.insteadOf", bad.display()),
                "scoped:",
            ],
        );
        let include = root.temp.path().join("destination.config");
        git(
            root.temp.path(),
            &[
                "config",
                "--file",
                include.to_str().unwrap(),
                &format!("url.{}.insteadOf", bad.display()),
                "destination-owned:",
            ],
        );
        let before = fs::read(&include).unwrap();
        git(
            &destination,
            &["config", "include.path", include.to_str().unwrap()],
        );
        for _ in 0..2 {
            restore_workspace_snapshot(
                &FileGit,
                &snapshot,
                &destination,
                &RestoreBase::Provisioned {
                    allow_origin_fetch: false,
                },
            )
            .unwrap();
        }
        assert_eq!(
            config_values(&FileGit, &destination, &local).unwrap(),
            vec!["publish:"]
        );
        assert_eq!(
            config_values(&FileGit, &destination, &scoped).unwrap(),
            vec!["scoped:"]
        );
        assert_eq!(fs::read(include).unwrap(), before);
        assert_eq!(
            git(&destination, &["config", "--get", "include.path"]),
            root.temp
                .path()
                .join("destination.config")
                .to_str()
                .unwrap()
        );
        git(&destination, &["push", "origin", "main"]);
        assert_eq!(git(&good, &["rev-parse", "main"]), snapshot.head.commit());
        assert_ne!(git(&bad, &["rev-parse", "main"]), snapshot.head.commit());
    }

    // Hard-won: 35bdaa417: equal saved positions stay attached while a third forced rebase detaches.
    #[test]
    fn three_shared_branch_worktrees_restore_two_agreeing_attached_and_one_rebasing_detached() {
        let root = Repo::new();
        root.conflict();
        let original = git(&root.checkout, &["rev-parse", "HEAD"]);
        git(&root.checkout, &["switch", "-c", "primary"]);
        let owner = root.checkout.join("a-owner");
        let agree = root.checkout.join("b-agree");
        let rebase = root.checkout.join("z-rebase");
        for path in [&owner, &agree, &rebase] {
            git(
                &root.checkout,
                &["worktree", "add", "--force", path.to_str().unwrap(), "main"],
            );
        }
        assert_ne!(
            run_git(&FileGit, &rebase, ["rebase", "side"], &[])
                .unwrap()
                .status,
            0
        );
        fs::write(owner.join("extra"), "owner branch advance\n").unwrap();
        git(&owner, &["add", "extra"]);
        git(&owner, &["commit", "-m", "owner advance"]);
        let current = git(&owner, &["rev-parse", "HEAD"]);
        let snapshot = root.capture(&CheckoutOwnership::Raw { launch: None });
        assert_eq!(snapshot.refs["refs/heads/main"], current);
        let restored = root.restore(&snapshot, None);
        for path in ["a-owner", "b-agree"] {
            assert_eq!(
                git(&restored.join(path), &["symbolic-ref", "HEAD"]),
                "refs/heads/main"
            );
            assert_eq!(git(&restored.join(path), &["rev-parse", "HEAD"]), current);
        }
        assert_eq!(
            read_head(&FileGit, &restored.join("z-rebase")).unwrap(),
            SnapshotHead::Detached(original.clone())
        );
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.contains("z-rebase")
                    && notice.contains("a-owner")
                    && notice.contains(&current)
                    && notice.contains(&original))
        );
    }

    // Hard-won: d00d140bd: even fully published leftover state is not represented by a snapshot.
    #[test]
    fn structural_git_state_refuses_published_leftovers_and_clean_module_worktrees_and_warns_raw() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        git(
            &root.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                module.origin.to_str().unwrap(),
                "retired",
            ],
        );
        git(
            &root.checkout,
            &["commit", "-am", "published retired module"],
        );
        git(&root.checkout, &["push", "origin", "main"]);
        git(&root.checkout, &["submodule", "deinit", "-f", "retired"]);
        let leftover = root.checkout.join(".git/modules/retired");
        let source = root.checkout.join("sub");
        let linked = source.join("local-lane");
        git(
            &source,
            &[
                "worktree",
                "add",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        );
        fs::write(
            git_path(&FileGit, &source, "info/exclude").unwrap(),
            "local-lane/\n",
        )
        .unwrap();
        git(&source, &["worktree", "lock", linked.to_str().unwrap()]);
        let outside = root.temp.path().join("outside-root-with-module");
        git(
            &root.checkout,
            &["worktree", "add", "--detach", outside.to_str().unwrap()],
        );
        git(
            &outside,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "sub",
            ],
        );
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("structure.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::UnrepresentedGitState { .. })
            ),
            "{error:#}"
        );
        for path in [&leftover, &linked, &outside] {
            assert!(
                error.to_string().contains(path.to_str().unwrap()),
                "{error:#}"
            );
        }
        assert!(error.to_string().contains("worktree remove --force"));
        assert!(
            error.to_string().contains("locked")
                && error.to_string().contains("worktree unlock")
                && error.to_string().contains("once it is available")
        );
        assert!(!error.to_string().contains("move it inside"));
        let raw = root.capture(&CheckoutOwnership::Raw { launch: None });
        for path in [&leftover, &linked, &outside] {
            assert!(
                raw.notices
                    .iter()
                    .any(|notice| notice.contains(path.to_str().unwrap())
                        && notice.contains("raw checkout is kept"))
            );
        }
        assert!(linked.join(".git").exists());
        assert!(leftover.exists());
    }

    // Hard-won: 5e28ea249: bisect reset can unregister a represented module and leave its admin behind.
    #[test]
    fn bisect_abort_structural_prediction_refuses_before_operation_consent() {
        let root = Repo::new();
        let module = Repo::new();
        let base = git(&root.checkout, &["rev-parse", "HEAD"]);
        add_submodule(&root, &module);
        root.commit("middle with module\n");
        root.commit("tip with module\n");
        let bad = git(&root.checkout, &["rev-parse", "HEAD"]);
        git(&root.checkout, &["checkout", "--detach", &base]);
        git(&root.checkout, &["bisect", "start", &bad, &base]);
        assert!(root.checkout.join(".gitmodules").exists());
        let before = operation_record(&FileGit, &root.checkout, &CheckoutKey::default()).unwrap();
        let position = git(&root.checkout, &["rev-parse", "HEAD"]);
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("post-abort.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::UnrepresentedGitState { .. })
            ),
            "{error:#}"
        );
        assert!(
            error
                .to_string()
                .contains(root.checkout.join(".git/modules/sub").to_str().unwrap()),
            "{error:#}"
        );
        assert_eq!(git(&root.checkout, &["rev-parse", "HEAD"]), position);
        assert_eq!(
            operation_record(&FileGit, &root.checkout, &CheckoutKey::default()).unwrap(),
            before
        );
        assert!(root.checkout.join(".git/modules/sub").exists());
    }

    // Hard-won: f2e45560f: abort reapplies autostashed .gitmodules instead of its committed registration.
    #[test]
    fn autostashed_gitmodules_removal_is_a_structural_blocker_before_merge_abort_consent() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        root.conflict();
        fs::remove_file(root.checkout.join(".gitmodules")).unwrap();
        assert_ne!(
            run_git(
                &FileGit,
                &root.checkout,
                ["merge", "--autostash", "side"],
                &[]
            )
            .unwrap()
            .status,
            0
        );
        assert!(root.checkout.join(".gitmodules").exists());
        let marker = git_path(&FileGit, &root.checkout, "MERGE_AUTOSTASH").unwrap();
        let before = fs::read(&marker).unwrap();
        let inventory =
            workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
        let blocker = inventory
            .blockers
            .iter()
            .find(|blocker| matches!(blocker.kind, SnapshotRefused::UnrepresentedGitState { .. }))
            .unwrap();
        assert!(
            blocker
                .paths
                .iter()
                .any(|path| path.value == root.checkout.join(".git/modules/sub"))
        );
        let encoded = serde_json::to_value(blocker).unwrap();
        assert_eq!(encoded["kind"], "unrepresented_git_state");
        assert!(encoded["remedy"].as_str().unwrap().contains("delete"));
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("autostash-blocked.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), blocker.remedy);
        assert_eq!(fs::read(marker).unwrap(), before);
        assert_eq!(
            operation(&FileGit, &root.checkout).unwrap(),
            Some(GitOperationKind::Merge)
        );
    }

    // Hard-won: d00d140bd: no boundary coverage test guarantees a full-history restore.
    #[test]
    fn shallow_managed_checkout_requires_full_history_and_raw_gets_notice() {
        let origin = Repo::new();
        origin.commit("second commit\n");
        git(&origin.checkout, &["push", "origin", "main"]);
        let checkout = origin.temp.path().join("shallow");
        let url = url::Url::from_directory_path(&origin.origin)
            .unwrap()
            .to_string();
        git(
            origin.temp.path(),
            &["clone", "--depth=1", &url, checkout.to_str().unwrap()],
        );
        let capture = |ownership| {
            capture_workspace_snapshot(
                &FileGit,
                &checkout,
                ownership,
                &origin.temp.path().join("shallow.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
        };
        let error = capture(&CheckoutOwnership::Managed).unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::ShallowRepository(_))
            ),
            "{error:#}"
        );
        for text in [
            "needs full history",
            "git fetch --unshallow",
            "git fetch --unshallow <url>",
            "mj suspend --without-checkpoint",
            "keep the session running",
        ] {
            assert!(error.to_string().contains(text), "{error:#}");
        }
        let raw = capture(&CheckoutOwnership::Raw { launch: None }).unwrap();
        assert!(
            raw.notices
                .iter()
                .any(|notice| notice.contains("shallow")
                    && notice.contains("source checkout is kept"))
        );
        git(&checkout, &["fetch", "--unshallow"]);
        capture(&CheckoutOwnership::Managed).unwrap();
        let rebasing = Repo::new();
        rebasing.conflict();
        git(&rebasing.checkout, &["push", "origin", "main", "side"]);
        rebasing.start_conflict("rebase");
        git(&rebasing.checkout, &["fetch", "--depth=1", "origin"]);
        assert_eq!(
            git(
                &rebasing.checkout,
                &["rev-parse", "--is-shallow-repository"]
            ),
            "true"
        );
        let before =
            operation_record(&FileGit, &rebasing.checkout, &CheckoutKey::default()).unwrap();
        let index = fs::read(git_path(&FileGit, &rebasing.checkout, "index").unwrap()).unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &rebasing.checkout,
            &CheckoutOwnership::Managed,
            &rebasing.temp.path().join("blocked-rebase.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::ShallowRepository(_))
            ),
            "{error:#}"
        );
        assert_eq!(
            operation_record(&FileGit, &rebasing.checkout, &CheckoutKey::default()).unwrap(),
            before
        );
        assert_eq!(
            fs::read(git_path(&FileGit, &rebasing.checkout, "index").unwrap()).unwrap(),
            index
        );
    }

    // Hard-won: 3a482c228: owner normalization could drop an uncaptured checkout's newer live tip.
    #[test]
    fn displaced_live_tip_survives_two_raw_rebases_and_a_second_managed_snapshot() {
        let root = Repo::new();
        root.conflict();
        let first = git(&root.checkout, &["rev-parse", "HEAD"]);
        git(&root.checkout, &["switch", "-c", "primary"]);
        let a = root.checkout.join("a-rebase");
        let b = root.checkout.join("b-rebase");
        let outside = root.temp.path().join("outside-advancer");
        for path in [&a, &outside] {
            git(
                &root.checkout,
                &["worktree", "add", "--force", path.to_str().unwrap(), "main"],
            );
        }
        assert_ne!(
            run_git(&FileGit, &a, ["rebase", "side"], &[])
                .unwrap()
                .status,
            0
        );
        fs::write(outside.join("extra-one"), "second saved tip\n").unwrap();
        git(&outside, &["add", "extra-one"]);
        git(&outside, &["commit", "-m", "second tip"]);
        let second = git(&outside, &["rev-parse", "HEAD"]);
        git(
            &root.checkout,
            &["worktree", "add", "--force", b.to_str().unwrap(), "main"],
        );
        assert_ne!(
            run_git(&FileGit, &b, ["rebase", "side"], &[])
                .unwrap()
                .status,
            0
        );
        fs::write(outside.join("extra-two"), "newer unpublished live tip\n").unwrap();
        git(&outside, &["add", "extra-two"]);
        git(&outside, &["commit", "-m", "live tip"]);
        let live = git(&outside, &["rev-parse", "HEAD"]);
        let displaced = format!("refs/mj/displaced/{live}");
        let snapshot = root.capture(&CheckoutOwnership::Raw { launch: None });
        assert_eq!(snapshot.refs["refs/heads/main"], first);
        assert_eq!(snapshot.refs[&displaced], live);
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(&restored.join("a-rebase"), &["rev-parse", "HEAD"]),
            first
        );
        assert_eq!(
            read_head(&FileGit, &restored.join("b-rebase")).unwrap(),
            SnapshotHead::Detached(second.clone())
        );
        assert_eq!(git(&restored, &["rev-parse", &displaced]), live);
        let next = capture_workspace_snapshot(
            &FileGit,
            &restored,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("again.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        assert_eq!(next.refs[&displaced], live);
        let again = root.temp.path().join("restored-again");
        restore_workspace_snapshot(&FileGit, &next, &again, &clone_base(&next, None)).unwrap();
        for commit in [&first, &second, &live] {
            git(&again, &["cat-file", "-e", commit]);
        }
        assert_eq!(git(&again, &["rev-parse", &displaced]), live);
    }

    // Hard-won: d60a6c566: clean module bisect state passed capture without a loss warning.
    #[test]
    fn clean_initialized_module_operation_refuses_managed_and_warns_raw() {
        let root = Repo::new();
        let module = Repo::new();
        module.commit("published middle\n");
        module.commit("published tip\n");
        git(&module.checkout, &["push", "origin", "main"]);
        add_submodule(&root, &module);
        let source = root.checkout.join("sub");
        git(&source, &["bisect", "start", "HEAD", "HEAD~2"]);
        assert_eq!(git(&source, &["status", "--porcelain"]), "");
        let log = git_path(&FileGit, &source, "BISECT_LOG").unwrap();
        let before = fs::read(&log).unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("module-op.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::SubmoduleOperations(_))
            ),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("sub")
                && error.to_string().contains("Bisect")
                && error.to_string().contains("bisect reset"),
            "{error:#}"
        );
        assert_eq!(fs::read(&log).unwrap(), before);
        let snapshot = root.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(snapshot.notices.iter().any(|notice| notice.contains("sub")
            && notice.contains("Bisect")
            && notice.contains("operation state does not travel")));
        assert_eq!(fs::read(log).unwrap(), before);
        assert_eq!(
            operation(&FileGit, &source).unwrap(),
            Some(GitOperationKind::Bisect)
        );
    }

    // Hard-won: d60a6c566: canonicalizing origin selected the symlink target's sibling module.
    #[cfg(unix)]
    #[test]
    fn relative_module_pin_uses_configured_origin_siblings_before_resolving_symlinks() {
        let root = Repo::new();
        let module = Repo::new();
        let other = Repo::new();
        add_submodule(&root, &module);
        let mirrors = root.temp.path().join("mirrors");
        let storage = root.temp.path().join("storage");
        fs::create_dir(&mirrors).unwrap();
        fs::create_dir(&storage).unwrap();
        fs::rename(&root.origin, storage.join("app.git")).unwrap();
        std::os::unix::fs::symlink(storage.join("app.git"), mirrors.join("app.git")).unwrap();
        std::os::unix::fs::symlink(&module.origin, mirrors.join("lib.git")).unwrap();
        std::os::unix::fs::symlink(&other.origin, storage.join("lib.git")).unwrap();
        git(
            &root.checkout,
            &[
                "remote",
                "set-url",
                "origin",
                mirrors.join("app.git").to_str().unwrap(),
            ],
        );
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(root.checkout.join(".git/modules/sub")).unwrap();
        git(
            &root.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.url",
                "../lib.git",
            ],
        );
        git(&root.checkout, &["add", ".gitmodules"]);
        module.commit("new pin only beside configured origin\n");
        git(&module.checkout, &["push", "origin", "main"]);
        let commit = git(&module.checkout, &["rev-parse", "HEAD"]);
        git(
            &root.checkout,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{commit},sub"),
            ],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&snapshot, None);
        git(
            &restored,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "sub",
            ],
        );
        assert_eq!(git(&restored.join("sub"), &["rev-parse", "HEAD"]), commit);
    }

    // Hard-won: 1d7cc60a7: without origin, an override cannot authorize the future relative module URL.
    #[test]
    fn origin_override_cannot_authorize_a_relative_uninitialized_submodule_pin() {
        let root = Repo::new();
        let module = Repo::new();
        let target = Repo::new();
        add_submodule(&root, &module);
        let authoritative = target.temp.path().join("lib.git");
        git(
            target.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                authoritative.to_str().unwrap(),
            ],
        );
        module.commit("only the source sibling publishes this pin\n");
        let pin = git(&module.checkout, &["rev-parse", "HEAD"]);
        let local = root.temp.path().join("lib.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                module.checkout.to_str().unwrap(),
                local.to_str().unwrap(),
            ],
        );
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(root.checkout.join(".git/modules/sub")).unwrap();
        git(
            &root.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.url",
                "../lib.git",
            ],
        );
        git(&root.checkout, &["add", ".gitmodules"]);
        git(
            &root.checkout,
            &["update-index", "--cacheinfo", &format!("160000,{pin},sub")],
        );
        git(&root.checkout, &["remote", "remove", "origin"]);
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("override.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            Some(target.origin.to_str().unwrap().to_owned()),
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::DirtySubmodule(_))
            ),
            "{error:#}"
        );
    }

    // Hard-won: daa70eca2: retry failed on a nested registration left by module provisioning.
    #[test]
    fn provisioned_nested_worktree_restore_reuses_its_registration_after_module_failure() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        let nested = root.checkout.join("lanes/retry");
        git(
            &root.checkout,
            &["worktree", "add", "--detach", nested.to_str().unwrap()],
        );
        git(
            &nested,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "sub",
            ],
        );
        fs::write(nested.join("file"), "detached nested commit\n").unwrap();
        git(&nested, &["commit", "-am", "detached nested commit"]);
        fs::write(nested.join("file"), "nested staged edits\n").unwrap();
        git(&nested, &["add", "file"]);
        fs::write(nested.join("file"), "nested tracked edits\n").unwrap();
        fs::write(root.checkout.join("file"), "primary staged edits\n").unwrap();
        git(&root.checkout, &["add", "file"]);
        fs::write(root.checkout.join("file"), "primary unstaged edits\n").unwrap();
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let destination = root.temp.path().join("provisioned");
        git(
            root.temp.path(),
            &[
                "clone",
                root.origin.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        git(
            &destination,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "sub",
            ],
        );
        let base = RestoreBase::Provisioned {
            allow_origin_fetch: false,
        };
        let error =
            restore_workspace_snapshot(&FileGit, &snapshot, &destination, &base).unwrap_err();
        assert!(
            error.to_string().contains("locally provisioned repository"),
            "{error:#}"
        );
        let restored = destination.join("lanes/retry");
        let registration = crate::archive::git::git_directories(&FileGit, &restored)
            .unwrap()
            .0;
        git(
            &restored,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "sub",
            ],
        );
        restore_workspace_snapshot(&FileGit, &snapshot, &destination, &base).unwrap();
        assert_eq!(
            crate::archive::git::git_directories(&FileGit, &restored)
                .unwrap()
                .0,
            registration
        );
        assert_eq!(
            discover_linked_worktrees(&FileGit, &destination)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&nested, &["rev-parse", "HEAD"])
        );
        assert_eq!(git(&restored, &["diff"]), git(&nested, &["diff"]));
        assert_eq!(
            git(&restored, &["diff", "--cached"]),
            git(&nested, &["diff", "--cached"])
        );
        assert_eq!(git(&destination, &["diff"]), git(&root.checkout, &["diff"]));
        assert_eq!(
            git(&destination, &["diff", "--cached"]),
            git(&root.checkout, &["diff", "--cached"])
        );
    }

    // Hard-won: bae1ae4d3: provisioned offline restore still fetched submodules.
    #[test]
    fn provisioned_submodules_require_local_objects_when_origin_fetch_is_disabled() {
        let r = Repo::new();
        let sub = Repo::new();
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        for message in ["first saved stash", "second saved stash"] {
            fs::write(r.checkout.join("file"), message).unwrap();
            git(&r.checkout, &["stash", "push", "-m", message]);
        }
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let destination = r.temp.path().join("provisioned");
        git(
            r.temp.path(),
            &[
                "clone",
                r.origin.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        let error = restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("locally provisioned repository"),
            "{error:#}"
        );
        git(
            &destination,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "sub",
            ],
        );
        let unreachable = sub.temp.path().join("offline.git");
        fs::rename(&sub.origin, &unreachable).unwrap();
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert_eq!(read_stash(&FileGit, &destination).unwrap(), snapshot.stash);
        assert_eq!(
            git(&destination.join("sub"), &["rev-parse", "HEAD"]),
            snapshot.initialized_submodules[0].commit
        );
    }
    #[test]
    fn rebase_abort_rechecks_unpublished_restored_submodule_gitlinks() {
        let r = Repo::new();
        let sub = Repo::new();
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "base submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        git(&r.checkout, &["branch", "side"]);
        r.commit("main conflict\n");
        // A later unpublished gitlink commit has not been replayed when rebase
        // stops on the earlier file conflict.
        let nested = r.checkout.join("sub");
        fs::write(nested.join("file"), "unpublished submodule\n").unwrap();
        git(&nested, &["commit", "-am", "unpublished"]);
        let unpublished = git(&nested, &["rev-parse", "HEAD"]);
        git(&r.checkout, &["commit", "-am", "unpublished gitlink"]);
        let original = git(&r.checkout, &["rev-parse", "HEAD"]);
        git(&r.checkout, &["checkout", "side"]);
        let published = git(&r.checkout, &["rev-parse", ":sub"]);
        git(&nested, &["checkout", "--detach", &published]);
        r.commit("side conflict\n");
        git(&r.checkout, &["checkout", "main"]);
        git(&nested, &["checkout", "--detach", &unpublished]);
        assert_ne!(
            run_git(&FileGit, &r.checkout, ["rebase", "side"], &[])
                .unwrap()
                .status,
            0
        );
        // Match the currently rebasing superproject, as users do while resolving.
        let indexed = git(&r.checkout, &["rev-parse", ":sub"]);
        git(&nested, &["checkout", "--detach", &indexed]);
        // Isolate the post-abort gitlink guard from inactive-branch loss refusal.
        git(&nested, &["branch", "-D", "main"]);
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("abort.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::DirtySubmodule(path))
            if path.value == Path::new("sub")),
            "{error:#}"
        );
        assert_eq!(git(&r.checkout, &["rev-parse", "HEAD"]), original);
        assert_eq!(git(&r.checkout, &["rev-parse", ":sub"]), unpublished);
        assert!(operation(&FileGit, &r.checkout).unwrap().is_none());
    }
    #[test]
    fn uninitialized_relative_submodule_urls_follow_origin_and_drop_other_upstreams() {
        let r = Repo::new();
        let sub = Repo::new();
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "base submodule"]);
        git(&r.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(r.checkout.join(".git/modules/sub")).unwrap();
        git(
            &r.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.url",
                "../lib.git",
            ],
        );
        git(&r.checkout, &["add", ".gitmodules"]);
        git(
            &r.checkout,
            &["remote", "set-url", "origin", "https://host/org/app.git"],
        );
        assert_eq!(
            resolve_submodule_url(&FileGit, &r.checkout, "../lib.git", None).unwrap(),
            "https://host/org/lib.git"
        );
        git(
            &r.checkout,
            &["remote", "add", "upstream", "https://upstream/org/app.git"],
        );
        git(&r.checkout, &["config", "branch.main.remote", "upstream"]);
        assert_eq!(
            resolve_submodule_url(&FileGit, &r.checkout, "../lib.git", None).unwrap(),
            "https://host/org/lib.git"
        );
        // Exercise that resolved URL with real Git transport via insteadOf.
        git(
            &r.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", sub.origin.display()),
                "https://host/org/lib.git",
            ],
        );
        git(
            &r.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", r.origin.display()),
                "https://host/org/app.git",
            ],
        );
        sub.commit("published next pin\n");
        git(&sub.checkout, &["push", "origin", "main"]);
        let published = git(&sub.checkout, &["rev-parse", "HEAD"]);
        git(
            &r.checkout,
            &["remote", "set-url", "origin", r.origin.to_str().unwrap()],
        );
        git(
            r.temp.path(),
            &[
                "clone",
                "--bare",
                sub.origin.to_str().unwrap(),
                r.temp.path().join("lib.git").to_str().unwrap(),
            ],
        );
        git(
            &r.checkout,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{published},sub"),
            ],
        );
        let inventory =
            workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert!(inventory.dirty_submodules.is_empty());
        assert!(!r.checkout.join("sub/.git").exists());
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.contains("upstreams") && notice.contains("main"))
        );
        let restored = r.restore(&snapshot, None);
        assert!(
            config_values(&FileGit, &restored, "branch.main.remote")
                .unwrap()
                .is_empty()
        );
        assert!(
            config_values(&FileGit, &restored, "branch.main.merge")
                .unwrap()
                .is_empty()
        );

        let provisioned = r.temp.path().join("provisioned-upstream");
        git(
            r.temp.path(),
            &[
                "clone",
                r.origin.to_str().unwrap(),
                provisioned.to_str().unwrap(),
            ],
        );
        git(&provisioned, &["config", "branch.main.remote", "upstream"]);
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &provisioned,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert!(
            config_values(&FileGit, &provisioned, "branch.main.remote")
                .unwrap()
                .is_empty()
        );
        assert!(
            config_values(&FileGit, &provisioned, "branch.main.merge")
                .unwrap()
                .is_empty()
        );
        sub.commit("unpublished next pin\n");
        let unpublished = git(&sub.checkout, &["rev-parse", "HEAD"]);
        git(
            &r.checkout,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{unpublished},sub"),
            ],
        );
        let inventory =
            workspace_inventory(&FileGit, &r.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(inventory.dirty_submodules[0].value, Path::new("sub"));
    }
    #[test]
    fn initialized_submodule_old_published_pin_fetches_only_new_remote_objects() {
        let r = Repo::new();
        let sub = Repo::new();
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "base submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        let nested = r.checkout.join("sub");
        let old = git(&nested, &["rev-parse", "HEAD"]);
        let refs = read_all_refs(&FileGit, &nested).unwrap();
        let fetch_head = git_path(&FileGit, &nested, "FETCH_HEAD").unwrap();
        fs::write(&fetch_head, "submodule sentinel\n").unwrap();
        sub.commit("newer published tip\n");
        git(&sub.checkout, &["push", "origin", "main"]);
        let index_path = git_path(&FileGit, &nested, "index").unwrap();
        let before_index = fs::read(&index_path).unwrap();
        let unchanged = fs::read(nested.join("file")).unwrap();
        fs::write(nested.join("file"), unchanged).unwrap();
        estimate_delta(&FileGit, &r.checkout, &CheckoutOwnership::Managed, None).unwrap();
        assert_eq!(fs::read(&index_path).unwrap(), before_index);
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(snapshot.initialized_submodules[0].commit, old);
        assert_eq!(read_all_refs(&FileGit, &nested).unwrap(), refs);
        assert_eq!(
            fs::read_to_string(&fetch_head).unwrap(),
            "submodule sentinel\n"
        );
        let destination = r.temp.path().join("restored");
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &clone_base(&snapshot, None),
        )
        .unwrap();
        assert_eq!(git(&destination.join("sub"), &["rev-parse", "HEAD"]), old);
    }
    #[test]
    fn initialized_relative_local_submodule_url_restores_from_another_directory() {
        let r = Repo::new();
        let sub = Repo::new();
        let remotes = r.temp.path().join("remotes");
        fs::create_dir(&remotes).unwrap();
        let super_origin = remotes.join("super.git");
        let lib_origin = remotes.join("lib.git");
        fs::rename(&r.origin, &super_origin).unwrap();
        fs::rename(&sub.origin, &lib_origin).unwrap();
        git(
            &r.checkout,
            &["remote", "set-url", "origin", "../remotes/super.git"],
        );
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                lib_origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(
            &r.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.url",
                "../lib.git",
            ],
        );
        git(&r.checkout, &["commit", "-am", "relative local submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        git(&r.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(r.checkout.join(".git/modules/sub")).unwrap();
        git(&r.checkout, &["submodule", "init", "sub"]);
        assert_eq!(
            git(&r.checkout, &["config", "submodule.sub.url"]),
            "../remotes/lib.git"
        );
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "sub",
            ],
        );
        // Raw keeps the lexical origin route; managed capture offers its canonical remedy.
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        assert_eq!(
            PathBuf::from(
                resolve_remote_path(&r.checkout, &snapshot.initialized_submodules[0].url).unwrap()
            ),
            lib_origin
        );
        let destination = r.temp.path().join("another-directory/restored");
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &clone_base(&snapshot, None),
        )
        .unwrap();
        assert_eq!(
            fs::read(destination.join("sub/file")).unwrap(),
            b"first\nsecond\nthird\n"
        );
        assert_eq!(
            git(&destination, &["config", "submodule.sub.url"]),
            snapshot.initialized_submodules[0].url
        );
    }

    #[test]
    fn bisect_reset_rechecks_files_that_were_ignored_before_abort() {
        let r = Repo::new();
        let good = git(&r.checkout, &["rev-parse", "HEAD"]);
        fs::write(r.checkout.join(".gitignore"), "scratch\n").unwrap();
        git(&r.checkout, &["add", ".gitignore"]);
        git(&r.checkout, &["commit", "-m", "ignore at midpoint"]);
        git(&r.checkout, &["rm", ".gitignore"]);
        git(
            &r.checkout,
            &["commit", "-m", "do not ignore at original head"],
        );
        let original = git(&r.checkout, &["rev-parse", "HEAD"]);
        git(&r.checkout, &["bisect", "start", &original, &good]);
        fs::write(r.checkout.join("scratch"), "only ignored at midpoint\n").unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("bisect.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<SnapshotRefused>(), Some(SnapshotRefused::Untracked(items))
            if items == &vec![item_path(&r.checkout, "scratch")]),
            "{error:#}"
        );
        assert_eq!(git(&r.checkout, &["rev-parse", "HEAD"]), original);
        assert!(operation(&FileGit, &r.checkout).unwrap().is_none());
        assert!(r.checkout.join("scratch").exists());
    }

    #[test]
    fn source_push_destinations_survive_explicit_and_fallback_configuration() {
        let r = Repo::new();
        let push = Repo::new();
        let push_url = format!("file://{}", push.origin.display());
        git(
            &r.checkout,
            &["remote", "set-url", "origin", "../origin.git"],
        );
        git(
            &r.checkout,
            &[
                "config",
                &format!("url.{push_url}.pushInsteadOf"),
                "../origin.git",
            ],
        );
        let fallback = r.capture(&CheckoutOwnership::Raw { launch: None });
        let fallback_destination = r.temp.path().join("fallback");
        restore_workspace_snapshot(
            &FileGit,
            &fallback,
            &fallback_destination,
            &clone_base(&fallback, None),
        )
        .unwrap();
        assert_eq!(
            config_values(&FileGit, &fallback_destination, "remote.origin.url")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            config_values(&FileGit, &fallback_destination, "remote.origin.pushurl")
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            git(
                &fallback_destination,
                &["remote", "get-url", "--push", "--all", "origin"]
            ),
            push_url
        );
        git(
            &r.checkout,
            &["config", "remote.origin.pushurl", "push:repo"],
        );
        git(
            &r.checkout,
            &["config", &format!("url.{push_url}.insteadOf"), "push:repo"],
        );
        // Explicit pushurl disables this conflicting pushInsteadOf rule.
        git(
            &r.checkout,
            &["config", "url.missing:repo.pushInsteadOf", "push:repo"],
        );
        let explicit = r.capture(&CheckoutOwnership::Raw { launch: None });
        let explicit_destination = r.temp.path().join("explicit");
        restore_workspace_snapshot(
            &FileGit,
            &explicit,
            &explicit_destination,
            &clone_base(&explicit, None),
        )
        .unwrap();
        assert_eq!(
            git(
                &explicit_destination,
                &["remote", "get-url", "--push", "--all", "origin"]
            ),
            push_url
        );
    }
    #[test]
    fn sha256_root_and_submodule_restore_with_an_independent_seed() {
        let r = Repo::new_with_format("sha256");
        let sub = Repo::new_with_format("sha256");
        git(
            &r.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&r.checkout, &["commit", "-am", "sha256 submodule"]);
        git(&r.checkout, &["push", "origin", "main"]);
        r.commit("unpushed sha256 work\n");
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(snapshot.object_format, "sha256");
        assert_eq!(snapshot.initialized_submodules[0].object_format, "sha256");
        let destination = r.temp.path().join("sha256-restored");
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &clone_base(&snapshot, Some(&r.checkout)),
        )
        .unwrap();
        assert_eq!(
            git(&destination, &["rev-parse", "--show-object-format"]),
            "sha256"
        );
        assert_eq!(
            git(
                &destination.join("sub"),
                &["rev-parse", "--show-object-format"]
            ),
            "sha256"
        );
        assert!(!destination.join(".git/objects/info/alternates").exists());
        assert_eq!(
            git(&destination, &["rev-parse", "HEAD"]),
            snapshot.head.commit()
        );
    }
    #[test]
    fn multiple_urls_push_only_to_the_subset_rewritten_by_git() {
        let r = Repo::new();
        let second = Repo::new();
        let first_url = r.origin.to_str().unwrap().to_owned();
        let second_url = second.origin.to_str().unwrap().to_owned();
        git(
            &r.checkout,
            &["config", "--add", "remote.origin.url", "second:repo"],
        );
        git(
            &r.checkout,
            &[
                "config",
                &format!("url.{second_url}.pushInsteadOf"),
                "second:repo",
            ],
        );
        let source = git(
            &r.checkout,
            &["remote", "get-url", "--push", "--all", "origin"],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let destination = r.restore(&snapshot, None);
        assert_eq!(
            config_values(&FileGit, &destination, "remote.origin.url").unwrap(),
            vec![first_url, "second:repo".into()]
        );
        assert_eq!(
            git(
                &destination,
                &["remote", "get-url", "--push", "--all", "origin"]
            ),
            source
        );
        assert!(source.ends_with(&second_url));
    }
    #[test]
    fn explicit_push_and_fetch_apply_their_local_rewrite_once() {
        let r = Repo::new();
        let push = Repo::new();
        let fetch_input = format!("file://{}", push.origin.display());
        let fetch_effective = format!("file://{}", r.origin.display());
        git(&r.checkout, &["remote", "set-url", "origin", &fetch_input]);
        git(
            &r.checkout,
            &[
                "config",
                &format!("url.{fetch_effective}.insteadOf"),
                &fetch_input,
            ],
        );
        git(
            &r.checkout,
            &["config", "remote.origin.pushurl", "push-alias:repo"],
        );
        git(
            &r.checkout,
            &[
                "config",
                &format!("url.{fetch_input}.insteadOf"),
                "push-alias:repo",
            ],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let restored = r.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["remote", "get-url", "origin"]),
            fetch_effective
        );
        assert_eq!(
            git(
                &restored,
                &["remote", "get-url", "--push", "--all", "origin"]
            ),
            fetch_input
        );
    }
    #[test]
    fn local_included_rules_travel_with_relative_bases_and_credential_notices() {
        let r = Repo::new();
        fs::create_dir(r.temp.path().join("mirrors")).unwrap();
        let mirror = r.temp.path().join("mirrors/origin.git");
        fs::rename(&r.origin, &mirror).unwrap();
        let include = r.temp.path().join("source-rules.inc");
        git(
            &r.checkout,
            &["config", "include.path", include.to_str().unwrap()],
        );
        for (name, value) in [
            ("url.../mirrors/.insteadOf", "corp:"),
            ("url.https://user:KEY_TOKEN@host/.insteadOf", "unused-key:"),
            ("url.https://safe/.insteadOf", "https://user:VALUE_TOKEN@"),
            ("url.https://USER_TOKEN@host/.insteadOf", "unused-user:"),
            ("url.ssh://git@host/.insteadOf", "https://host/"),
        ] {
            git(
                r.temp.path(),
                &[
                    "config",
                    "--file",
                    include.to_str().unwrap(),
                    "--add",
                    name,
                    value,
                ],
            );
        }
        git(
            &r.checkout,
            &["remote", "set-url", "origin", "corp:origin.git"],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(
            snapshot
                .url_rules
                .iter()
                .any(|rule| rule.name == "url.ssh://git@host/.insteadof")
        );
        assert!(snapshot.url_rules.iter().any(|rule| {
            rule.value == "corp:" && Path::new(url_rule_parts(&rule.name).unwrap().0).is_absolute()
        }));
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.contains("omitted credential-bearing URL rule"))
        );
        let serialized = serde_json::to_string(&snapshot).unwrap();
        for secret in ["KEY_TOKEN", "VALUE_TOKEN", "USER_TOKEN"] {
            assert!(!serialized.contains(secret));
        }
        let destination = r.temp.path().join("another/depth/restored");
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &clone_base(&snapshot, None),
        )
        .unwrap();
        assert_eq!(
            resolve_remote_path(
                &destination,
                &git(&destination, &["remote", "get-url", "origin"])
            )
            .unwrap(),
            mirror.to_str().unwrap()
        );
        // Host includes remain host-owned; only repository-local includes are flattened.
        let include = FileGit
            .run(
                &destination,
                &GitCommand {
                    arguments: ["config", "--local", "--get-all", "include.path"]
                        .into_iter()
                        .map(OsString::from)
                        .collect(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(include.status, 1);
        assert!(include.stdout.is_empty());
        git(&destination, &["fetch", "origin"]);
    }

    #[test]
    fn configured_chained_rewrites_apply_once_after_restore() {
        let r = Repo::new();
        let effective = format!("file://{}", r.origin.display());
        let missing = format!("file://{}/missing.git", r.temp.path().display());
        git(&r.checkout, &["remote", "set-url", "origin", "source:repo"]);
        git(
            &r.checkout,
            &[
                "config",
                &format!("url.{effective}.insteadOf"),
                "source:repo",
            ],
        );
        git(
            &r.checkout,
            &["config", &format!("url.{missing}.insteadOf"), &effective],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert_eq!(snapshot.origin_urls, vec!["source:repo"]);
        let restored = r.restore(&snapshot, None);
        assert_eq!(git(&restored, &["remote", "get-url", "origin"]), effective);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            snapshot.head.commit()
        );
    }

    #[test]
    fn initialized_submodule_restores_its_own_origin_rules_and_pushurl() {
        let r = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&r, &module);
        git(
            &r.checkout,
            &["config", "submodule.sub.url", "parent:unused"],
        );
        git(
            &r.checkout,
            &["config", "url./missing-parent/.insteadOf", "corp:"],
        );
        git(
            &directory,
            &["remote", "set-url", "origin", "module:origin"],
        );
        git(
            &directory,
            &[
                "config",
                &format!("url.{}.insteadOf", module.origin.display()),
                "module:origin",
            ],
        );
        git(
            &directory,
            &["config", "remote.origin.pushurl", "corp:target"],
        );
        let host = host_git(r.temp.path(), "empty-host");
        let snapshot = capture_workspace_snapshot(
            &host,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("module-own.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        let restored = r.temp.path().join("own-scope-restored");
        restore_workspace_snapshot(&host, &snapshot, &restored, &clone_base(&snapshot, None))
            .unwrap();
        assert_eq!(
            git(&restored.join("sub"), &["remote", "get-url", "origin"]),
            module.origin.to_str().unwrap()
        );
        assert_eq!(
            git(
                &restored.join("sub"),
                &["remote", "get-url", "--push", "origin"]
            ),
            "corp:target"
        );
        assert_eq!(
            git(&restored, &["config", "submodule.sub.url"]),
            "parent:unused"
        );
        assert!(git(&restored, &["submodule", "status"]).starts_with(' '));
        assert_eq!(
            git(
                &restored.join("sub"),
                &["symbolic-ref", "refs/remotes/origin/HEAD"]
            ),
            "refs/remotes/origin/main"
        );
    }

    #[test]
    fn scoped_rules_and_push_evidence_stay_with_each_linked_checkout() {
        let r = Repo::new();
        git(
            &r.checkout,
            &["config", "extensions.worktreeConfig", "true"],
        );
        git(
            &r.checkout,
            &[
                "config",
                "--worktree",
                "url.https://root/.insteadOf",
                "scope:",
            ],
        );
        let push_one = Repo::new();
        let push_two = Repo::new();
        for (name, target) in [("one", &push_one.origin), ("two", &push_two.origin)] {
            let path = r.checkout.join(name);
            git(
                &r.checkout,
                &["worktree", "add", "--detach", path.to_str().unwrap()],
            );
            // Git copies the primary config.worktree when creating a worktree.
            git(
                &path,
                &[
                    "config",
                    "--worktree",
                    "--unset-all",
                    "url.https://root/.insteadOf",
                ],
            );
            git(
                &path,
                &[
                    "config",
                    "--worktree",
                    &format!("url.https://{name}/.insteadOf"),
                    "scope:",
                ],
            );
            git(
                &path,
                &[
                    "config",
                    "--worktree",
                    &format!("url.{}.pushInsteadOf", target.display()),
                    r.origin.to_str().unwrap(),
                ],
            );
        }
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        let restored = r.restore(&snapshot, None);
        for (name, target) in [("one", &push_one.origin), ("two", &push_two.origin)] {
            assert_eq!(
                git(
                    &restored.join(name),
                    &["ls-remote", "--get-url", "scope:repo"]
                ),
                format!("https://{name}/repo")
            );
            let saved = snapshot
                .worktrees
                .iter()
                .find(|tree| tree.relative_path == Path::new(name))
                .unwrap();
            assert_eq!(
                saved.push_evidence,
                PushEvidence::Resolved(vec![target.to_str().unwrap().to_owned()])
            );
        }
        assert_eq!(
            git(&restored, &["ls-remote", "--get-url", "scope:repo"]),
            "https://root/repo"
        );
        let input = RepositoryInput {
            in_container: true,
            id: "container".into(),
            checkout: r.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: r.temp.path().join("container.pack"),
            exclusions: ExclusionSource::Origin,
            keep_dir: None,
        };
        let captured = capture_session_snapshot(
            &FileGit,
            &[input],
            &SessionCaptureConsent {
                repositories: BTreeMap::from([(
                    "container".into(),
                    CaptureConsent {
                        untracked: UntrackedConsent::Any,
                        operations: OperationConsent::Any,
                    },
                )]),
            },
        )
        .unwrap();
        let saved = &captured.repositories["container"];
        assert!(
            matches!(&saved.push_evidence, PushEvidence::Unavailable { reason } if reason == "container-local destination")
        );
        assert!(saved.worktrees.iter().all(|tree| matches!(&tree.push_evidence, PushEvidence::Unavailable { reason } if reason == "container-local destination")));
    }

    #[test]
    fn relative_fetch_push_and_rule_prefixes_keep_their_targets_elsewhere() {
        let r = Repo::new();
        fs::create_dir(r.temp.path().join("push")).unwrap();
        let push = r.temp.path().join("push/origin.git");
        git(
            r.temp.path(),
            &[
                "clone",
                "--bare",
                r.origin.to_str().unwrap(),
                push.to_str().unwrap(),
            ],
        );
        let old = git(&r.origin, &["rev-parse", "main"]);
        git(
            &r.checkout,
            &["remote", "set-url", "origin", "../origin.git"],
        );
        git(
            &r.checkout,
            &["config", "url.../push/.pushInsteadOf", "../"],
        );
        r.commit("publication target\n");
        // Raw keeps the lexical origin route; managed capture offers its canonical remedy.
        let error = capture_workspace_snapshot(
            &FileGit,
            &r.checkout,
            &CheckoutOwnership::Raw { launch: None },
            &r.temp.path().join("refused.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("anchoring origin push URL"),
            "{error:#}"
        );
        // Explicit absolute publication URLs retain Git's source behavior when
        // the original literal relative-prefix rewrite cannot be relocated.
        git(
            &r.checkout,
            &[
                "remote",
                "set-url",
                "--push",
                "origin",
                push.to_str().unwrap(),
            ],
        );
        let snapshot = r.capture(&CheckoutOwnership::Raw { launch: None });
        let restored = r.temp.path().join("deep/another/restored");
        restore_workspace_snapshot(&FileGit, &snapshot, &restored, &clone_base(&snapshot, None))
            .unwrap();
        git(&restored, &["push", "origin", "main"]);
        assert_eq!(git(&push, &["rev-parse", "main"]), snapshot.head.commit());
        assert_eq!(git(&r.origin, &["rev-parse", "main"]), old);
    }

    #[test]
    fn offline_origin_evidence_follows_refspecs_and_refuses_shared_namespaces() {
        let r = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&r, &module);
        for (path, namespace) in [(&r.checkout, "root"), (&directory, "module")] {
            git(
                path,
                &[
                    "config",
                    "remote.origin.fetch",
                    &format!("+refs/heads/*:refs/remotes/custom-{namespace}/*"),
                ],
            );
            git(path, &["fetch", "origin"]);
        }
        let root_offline = r.temp.path().join("root-offline.git");
        let module_offline = module.temp.path().join("module-offline.git");
        fs::rename(&r.origin, &root_offline).unwrap();
        fs::rename(&module.origin, &module_offline).unwrap();
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.remote_refs_stale);
        assert!(snapshot.pack.is_none());
        git(
            &directory,
            &["remote", "add", "fork", module.origin.to_str().unwrap()],
        );
        git(
            &directory,
            &[
                "config",
                "remote.fork.fetch",
                "+refs/heads/*:refs/remotes/custom-module/*",
            ],
        );
        let consent = CaptureConsent {
            untracked: UntrackedConsent::Any,
            operations: OperationConsent::Any,
        };
        let capture = || {
            capture_workspace_snapshot(
                &FileGit,
                &r.checkout,
                &CheckoutOwnership::Managed,
                &r.temp.path().join("ambiguous.pack"),
                &consent,
                None,
            )
        };
        assert!(format!("{:#}", capture().unwrap_err()).contains("ambiguous"));
        git(&directory, &["remote", "remove", "fork"]);
        // Removing fork deletes its mapped namespace, so restore origin's cache.
        fs::rename(&module_offline, &module.origin).unwrap();
        git(&directory, &["fetch", "origin"]);
        fs::rename(&module.origin, &module_offline).unwrap();
        git(
            &r.checkout,
            &["remote", "add", "fork", r.origin.to_str().unwrap()],
        );
        git(
            &r.checkout,
            &[
                "config",
                "remote.fork.fetch",
                "+refs/heads/*:refs/remotes/custom-root/*",
            ],
        );
        fs::rename(&module_offline, &module.origin).unwrap();
        let full = capture().unwrap();
        assert!(!full.remote_refs_stale);
        assert!(full.pack.is_some());
        assert!(
            full.notices
                .iter()
                .any(|notice| notice.contains("full history") && notice.contains("ambiguous"))
        );
        fs::rename(&root_offline, &r.origin).unwrap();
        let restored = r.temp.path().join("offline-restored");
        restore_workspace_snapshot(&FileGit, &snapshot, &restored, &clone_base(&snapshot, None))
            .unwrap();
        // A fetched PR tip is not available to ordinary restore fetches, even
        // when origin's configured destination resembles its tracking namespace.
        let pull = Repo::new();
        pull.commit("base published only under pull ref\n");
        let base = git(&pull.checkout, &["rev-parse", "HEAD"]);
        git(&pull.checkout, &["push", "origin", "HEAD:refs/pull/1/head"]);
        git(
            &pull.checkout,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/pull/*/head:refs/remotes/origin/pr/*",
            ],
        );
        git(&pull.checkout, &["fetch", "origin"]);
        pull.commit("child of pull-only base\n");
        fs::rename(&pull.origin, pull.temp.path().join("offline.git")).unwrap();
        let snapshot = pull.capture(&CheckoutOwnership::Managed);
        assert!(!snapshot.remote_refs_stale);
        assert!(snapshot.pack.is_some());
        let restored = pull.temp.path().join("provisioned");
        fs::create_dir(&restored).unwrap();
        git(&restored, &["init"]);
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &restored,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        git(&restored, &["cat-file", "-e", &base]);
        git(&restored, &["fsck", "--no-reflogs"]);
        // Fetch mappings into local branches do not certify their later commits.
        let local = Repo::new();
        git(
            &local.checkout,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/main:refs/heads/integration",
            ],
        );
        git(&local.checkout, &["fetch", "origin"]);
        git(&local.checkout, &["checkout", "integration"]);
        local.commit("local integration work\n");
        let unpublished = git(&local.checkout, &["rev-parse", "HEAD"]);
        fs::rename(&local.origin, local.temp.path().join("offline.git")).unwrap();
        let snapshot = local.capture(&CheckoutOwnership::Managed);
        assert!(!snapshot.remote_refs_stale);
        assert!(snapshot.pack.is_some());
        let restored = local.temp.path().join("provisioned");
        fs::create_dir(&restored).unwrap();
        git(&restored, &["init"]);
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &restored,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert_eq!(git(&restored, &["rev-parse", "HEAD"]), unpublished);
        git(&restored, &["fsck", "--no-reflogs"]);
    }

    #[test]
    fn push_evidence_redacts_host_rewrites_and_marks_unsafe_or_missing_context() {
        let r = Repo::new();
        let host = host_git(r.temp.path(), "push-evidence-global");
        let capture = || {
            capture_workspace_snapshot(
                &host,
                &r.checkout,
                &CheckoutOwnership::Managed,
                &r.temp.path().join("push-evidence.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap()
        };
        for (base, expected) in [
            (
                "https://USER_TOKEN:PASSWORD@push.example/repo",
                "https://push.example/repo",
            ),
            (
                "ssh://git:SSH_PASSWORD@push.example/repo",
                "ssh://git@push.example/repo",
            ),
            ("git@push.example:repo", "git@push.example:repo"),
        ] {
            fs::write(&host.global, "").unwrap();
            git(
                r.temp.path(),
                &[
                    "config",
                    "--file",
                    host.global.to_str().unwrap(),
                    &format!("url.{base}.pushInsteadOf"),
                    r.origin.to_str().unwrap(),
                ],
            );
            let snapshot = capture();
            assert_eq!(
                snapshot.push_evidence,
                PushEvidence::Resolved(vec![expected.into()])
            );
            let json = serde_json::to_string(&snapshot).unwrap();
            for token in ["USER_TOKEN", "PASSWORD", "SSH_PASSWORD"] {
                assert!(!json.contains(token));
            }
        }
        fs::write(&host.global, "").unwrap();
        git(
            r.temp.path(),
            &[
                "config",
                "--file",
                host.global.to_str().unwrap(),
                "url.https://user:BAD_TOKEN@.pushInsteadOf",
                r.origin.to_str().unwrap(),
            ],
        );
        assert!(matches!(
            capture().push_evidence,
            PushEvidence::Unavailable { .. }
        ));
        git(&r.checkout, &["remote", "remove", "origin"]);
        let snapshot = capture_workspace_snapshot(
            &host,
            &r.checkout,
            &CheckoutOwnership::Managed,
            &r.temp.path().join("no-named-origin.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            Some(r.origin.to_str().unwrap().to_owned()),
        )
        .unwrap();
        assert!(matches!(
            snapshot.push_evidence,
            PushEvidence::Unavailable { .. }
        ));
    }

    #[test]
    fn staged_submodule_tag_ancestor_and_missing_remote_head_restore() {
        let r = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&r, &module);
        let actual = git(&directory, &["rev-parse", "HEAD"]);
        git(&module.checkout, &["checkout", "--orphan", "tag-history"]);
        git(&module.checkout, &["commit", "-am", "separate history"]);
        let ancestor = git(&module.checkout, &["rev-parse", "HEAD"]);
        module.commit("tagged successor\n");
        git(&module.checkout, &["tag", "published-only"]);
        git(&module.checkout, &["push", "origin", "published-only"]);
        git(&directory, &["fetch", "--tags", "origin"]);
        git(
            &r.checkout,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{ancestor},sub"),
            ],
        );
        let snapshot = r.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.has_uncommitted_state);
        git(
            &module.origin,
            &["symbolic-ref", "HEAD", "refs/heads/deleted-default"],
        );
        let restored = r.temp.path().join("tag-ancestor-restored");
        restore_workspace_snapshot(&FileGit, &snapshot, &restored, &clone_base(&snapshot, None))
            .unwrap();
        assert_eq!(git(&restored, &["rev-parse", ":sub"]), ancestor);
        assert_eq!(git(&restored.join("sub"), &["rev-parse", "HEAD"]), actual);
        assert_eq!(
            git(&restored.join("sub"), &["tag", "--list", "published-only"]),
            "published-only"
        );
        assert!(!git(&restored, &["submodule", "status"]).starts_with('-'));
        git(
            &module.origin,
            &["update-ref", "-d", "refs/tags/published-only"],
        );
        let recovered = r.temp.path().join("sha-recovered");
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &recovered,
            &clone_base(&snapshot, None),
        )
        .unwrap();
        git(&recovered.join("sub"), &["cat-file", "-e", &ancestor]);
    }
    // Hard-won: b445ceae5: the runner's SSH default hid repository identity/proxy configuration.
    #[cfg(unix)]
    #[test]
    fn integration_review_capture_honors_source_ssh_command() {
        let root = Repo::new();
        let ssh = root.temp.path().join("ssh.sh");
        fs::write(
            &ssh,
            format!("exec git-upload-pack '{}'\n", root.origin.display()),
        )
        .unwrap();
        git(
            &root.checkout,
            &[
                "config",
                "core.sshCommand",
                &format!("sh '{}'", ssh.display()),
            ],
        );
        git(
            &root.checkout,
            &["remote", "set-url", "origin", "ssh://snapshot.invalid/repo"],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert!(!snapshot.remote_refs_stale);
        assert!(snapshot.pack.is_none());
    }

    // Hard-won: b445ceae5: a partially applied am saved the live HEAD instead of its abort position.
    #[test]
    fn integration_review_raw_am_restores_pre_series_head() {
        for detached in [false, true] {
            let root = Repo::new();
            let base = git(&root.checkout, &["rev-parse", "HEAD"]);
            git(&root.checkout, &["checkout", "-b", "patches"]);
            fs::write(root.checkout.join("added"), "first patch\n").unwrap();
            git(&root.checkout, &["add", "added"]);
            git(&root.checkout, &["commit", "-m", "first patch"]);
            root.commit("second patch conflicts\n");
            let patches = git(
                &root.checkout,
                &["format-patch", "--stdout", &format!("{base}..HEAD")],
            );
            git(&root.checkout, &["checkout", "main"]);
            root.commit("recipient conflict\n");
            if detached {
                git(&root.checkout, &["checkout", "--detach"]);
            }
            let before = read_head(&FileGit, &root.checkout).unwrap();
            let output = run_git(&FileGit, &root.checkout, ["am"], patches.as_bytes()).unwrap();
            assert_ne!(output.status, 0);
            assert_ne!(git(&root.checkout, &["rev-parse", "HEAD"]), before.commit());
            let snapshot = root.capture(&CheckoutOwnership::Raw { launch: None });
            assert_eq!(snapshot.head, before);
            assert_eq!(
                operation(&FileGit, &root.checkout).unwrap(),
                Some(GitOperationKind::Am)
            );
            let restored = root.restore(&snapshot, None);
            assert_eq!(read_head(&FileGit, &restored).unwrap(), before);
            assert!(!restored.join("added").exists());
        }
    }

    // Hard-won: b445ceae5: linked-checkout inventory used a different relative origin than restore.
    #[test]
    fn integration_review_nested_relative_origin_uses_primary_endpoint() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        let authoritative = root.temp.path().join("lib.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                authoritative.to_str().unwrap(),
            ],
        );
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(root.checkout.join(".git/modules/sub")).unwrap();
        git(
            &root.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.url",
                "../lib.git",
            ],
        );
        git(&root.checkout, &["commit", "-am", "relative module"]);
        git(
            &root.checkout,
            &["remote", "set-url", "origin", "../origin.git"],
        );
        let nested = root.checkout.join("lanes/agent");
        git(
            &root.checkout,
            &["worktree", "add", "--detach", nested.to_str().unwrap()],
        );
        module.commit("only the wrong nested endpoint has this pin\n");
        let pin = git(&module.checkout, &["rev-parse", "HEAD"]);
        let wrong = root.checkout.join("lanes/lib.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                module.checkout.to_str().unwrap(),
                wrong.to_str().unwrap(),
            ],
        );
        git(
            &nested,
            &["update-index", "--cacheinfo", &format!("160000,{pin},sub")],
        );
        let inventory =
            workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
        assert!(
            inventory.worktrees[0]
                .1
                .dirty_submodules
                .iter()
                .any(|module| module.value == Path::new("sub"))
        );
    }

    // Hard-won: b445ceae5: persisted paths could initialize a repository inside Git administration.
    #[test]
    fn integration_review_restore_rejects_git_directory_paths_before_writes() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        let nested = root.checkout.join("lanes/agent");
        git(
            &root.checkout,
            &["worktree", "add", "--detach", nested.to_str().unwrap()],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        for path in [".git/modules/injected", "safe/.GiT/injected"] {
            for kind in 0..3 {
                let mut invalid = snapshot.clone();
                match kind {
                    0 => invalid.initialized_submodules[0].path = path.into(),
                    1 => invalid.index_gitlinks[0].path = path.into(),
                    _ => invalid.worktrees[0].relative_path = path.into(),
                }
                let destination = root.temp.path().join("unsafe-destination");
                let error = restore_workspace_snapshot(
                    &FileGit,
                    &invalid,
                    &destination,
                    &clone_base(&invalid, None),
                )
                .unwrap_err();
                assert!(
                    format!("{error:#}").contains("inside a Git directory"),
                    "{error:#}"
                );
                assert!(!destination.exists());
            }
        }
    }

    // Hard-won: b445ceae5: skipping symrefs also dropped their names from a complete snapshot.
    #[test]
    fn integration_review_symbolic_local_alias_keeps_name_and_commit() {
        let root = Repo::new();
        git(
            &root.checkout,
            &["symbolic-ref", "refs/heads/alias", "refs/heads/main"],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert_eq!(snapshot.refs["refs/heads/alias"], snapshot.head.commit());
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "refs/heads/alias"]),
            snapshot.head.commit()
        );
        let output = run_git(
            &FileGit,
            &restored,
            ["symbolic-ref", "-q", "refs/heads/alias"],
            &[],
        )
        .unwrap();
        assert_ne!(output.status, 0);
    }
    // Hard-won: 3f919bd96: snapshot rewrite keys could set arbitrary executable Git configuration.
    #[test]
    fn integration_review_rewrite_keys_are_validated_in_every_scope_before_restore() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        let nested = root.checkout.join("lanes/agent");
        git(
            &root.checkout,
            &["worktree", "add", "--detach", nested.to_str().unwrap()],
        );
        git(
            &nested,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
            ],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        for key in ["core.sshCommand", "include.path", "url.evil.unsupported"] {
            for scope in 0..7 {
                let mut invalid = snapshot.clone();
                let rules = match scope {
                    0 => &mut invalid.url_rules,
                    1 => &mut invalid.worktree_url_rules,
                    2 => &mut invalid.worktrees[0].url_rules,
                    3 => &mut invalid.initialized_submodules[0].url_rules,
                    4 => &mut invalid.initialized_submodules[0].worktree_url_rules,
                    5 => &mut invalid.worktrees[0].initialized_submodules[0].url_rules,
                    _ => &mut invalid.worktrees[0].initialized_submodules[0].worktree_url_rules,
                };
                rules.push(SnapshotConfigEntry {
                    name: key.into(),
                    value: "injected".into(),
                });
                let destination = root.temp.path().join("invalid-config");
                assert!(
                    restore_workspace_snapshot(
                        &FileGit,
                        &invalid,
                        &destination,
                        &clone_base(&invalid, None)
                    )
                    .is_err()
                );
                assert!(
                    !destination.exists(),
                    "key {key}, scope {scope} changed destination"
                );
                let before = fs::read(root.checkout.join(".git/config")).unwrap();
                assert!(
                    restore_workspace_snapshot(
                        &FileGit,
                        &invalid,
                        &root.checkout,
                        &RestoreBase::Provisioned {
                            allow_origin_fetch: false
                        }
                    )
                    .is_err()
                );
                assert_eq!(fs::read(root.checkout.join(".git/config")).unwrap(), before);
            }
        }
    }

    // Hard-won: 3f919bd96: indexed gitlinks hid ordinary files in uninitialized module directories.
    #[test]
    fn integration_review_uninitialized_module_files_require_discard_consent() {
        let root = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&root, &module);
        fs::write(
            root.checkout.join(".gitignore"),
            "sub/*.ignored\nsub/cache/\n",
        )
        .unwrap();
        git(&root.checkout, &["add", ".gitignore"]);
        git(
            &root.checkout,
            &["commit", "-m", "ignore generated module files"],
        );
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(root.checkout.join(".git/modules/sub")).unwrap();
        fs::create_dir_all(directory.join("deep")).unwrap();
        fs::create_dir_all(directory.join("cache")).unwrap();
        fs::write(directory.join("notes"), "user note").unwrap();
        fs::write(directory.join("deep/file\nname"), "nested note").unwrap();
        fs::write(directory.join("generated.ignored"), "ignored").unwrap();
        fs::write(directory.join("cache/data"), "ignored").unwrap();
        let index = fs::read(root.checkout.join(".git/index")).unwrap();
        let inventory =
            workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(
            inventory
                .untracked
                .iter()
                .map(|path| (path.path.clone(), path.bytes))
                .collect::<Vec<_>>(),
            vec![
                (PathBuf::from("sub/deep/file\nname"), 11),
                (PathBuf::from("sub/notes"), 9)
            ]
        );
        assert_eq!(fs::read(root.checkout.join(".git/index")).unwrap(), index);
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("refused.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::Untracked(_))
            ),
            "{error:#}"
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&snapshot, None);
        assert!(!restored.join("sub/notes").exists());
        assert_eq!(fs::read(directory.join("notes")).unwrap(), b"user note");
    }

    // Hard-won: 3f919bd96: staged nested gitlinks were omitted from the uncommitted-state flag.
    #[test]
    fn integration_review_staged_nested_gitlinks_mark_each_checkout_uncommitted() {
        let root = Repo::new();
        let module = Repo::new();
        let leaf = Repo::new();
        add_submodule(&module, &leaf);
        add_submodule(&root, &module);
        let nested = root.checkout.join("lanes/agent");
        git(
            &root.checkout,
            &["worktree", "add", "--detach", nested.to_str().unwrap()],
        );
        for checkout in [&root.checkout, &nested] {
            git(
                checkout,
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "update",
                    "--init",
                    "--recursive",
                ],
            );
        }
        leaf.commit("new published nested pin\n");
        git(&leaf.checkout, &["push", "origin", "main"]);
        let pin = git(&leaf.checkout, &["rev-parse", "HEAD"]);
        for checkout in [&root.checkout, &nested] {
            git(&checkout.join("sub/sub"), &["fetch", "origin"]);
            git(&checkout.join("sub/sub"), &["checkout", "--detach", &pin]);
            git(&checkout.join("sub"), &["add", "sub"]);
        }
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.has_uncommitted_state);
        assert!(snapshot.worktrees[0].has_uncommitted_state);
        assert!(snapshot.working_state.is_none());
        assert!(snapshot.worktrees[0].working_state.is_none());
        let restored = root.restore(&snapshot, None);
        for checkout in [&restored, &restored.join("lanes/agent")] {
            assert_eq!(git(&checkout.join("sub"), &["rev-parse", ":sub"]), pin);
            assert!(!git(&checkout.join("sub"), &["diff", "--cached"]).is_empty());
        }
    }

    // Hard-won: 3f919bd96: capture inherited the archival runner's lazy-promisor-fetch prohibition.
    #[test]
    fn integration_review_partial_clone_hydrates_removed_origin_history_for_pack() {
        let mut root = Repo::new();
        let historical = "promised historical content\n".repeat(4000);
        fs::write(root.checkout.join("historical"), &historical).unwrap();
        git(&root.checkout, &["add", "historical"]);
        git(
            &root.checkout,
            &["commit", "-m", "history retained only locally"],
        );
        git(&root.checkout, &["push", "origin", "main"]);
        let blob = git(&root.checkout, &["rev-parse", "HEAD:historical"]);
        git(&root.origin, &["config", "uploadpack.allowFilter", "true"]);
        git(
            &root.origin,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        let partial = root.temp.path().join("partial");
        let url = url::Url::from_directory_path(&root.origin)
            .unwrap()
            .to_string();
        git(
            root.temp.path(),
            &[
                "clone",
                "--filter=blob:none",
                "--no-checkout",
                &url,
                partial.to_str().unwrap(),
            ],
        );
        git(&partial, &["branch", "retained", "HEAD"]);
        git(&root.checkout, &["checkout", "--orphan", "replacement"]);
        git(&root.checkout, &["rm", "-rf", "."]);
        fs::write(root.checkout.join("current"), "new origin root\n").unwrap();
        git(&root.checkout, &["add", "current"]);
        git(&root.checkout, &["commit", "-m", "replace origin history"]);
        git(&root.checkout, &["push", "--force", "origin", "HEAD:main"]);
        git(&partial, &["fetch", "origin"]);
        git_text(
            &CaptureGit::new(&FileGit),
            &partial,
            ["checkout", "-B", "main", "origin/main"],
        )
        .unwrap();
        let absent = run_git(&FileGit, &partial, ["cat-file", "-e", &blob], &[]).unwrap();
        assert_ne!(
            absent.status, 0,
            "fixture unexpectedly hydrated the historical blob"
        );
        root.checkout = partial;
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_some());
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["show", "retained:historical"]),
            historical.trim_end_matches('\n')
        );
        git(&restored, &["fsck", "--full"]);
    }
    // Hard-won: 267680121: legacy worktree porcelain C-quoted paths were silently omitted.
    #[cfg(unix)]
    #[test]
    fn integration_review_legacy_git_preserves_quoted_nested_worktree_paths() {
        let root = Repo::new();
        let relative = Path::new("lanes/tab\tquote\"slash\\café");
        let nested = root.checkout.join(relative);
        git(
            &root.checkout,
            &["worktree", "add", "--detach", nested.to_str().unwrap()],
        );
        fs::write(nested.join("file"), "unpublished nested commit\n").unwrap();
        git(&nested, &["commit", "-am", "nested commit"]);
        fs::write(nested.join("file"), "nested working edits\n").unwrap();
        let snapshot = capture_workspace_snapshot(
            &crate::test_support::Git231,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("quoted.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap();
        assert_eq!(snapshot.worktrees.len(), 1);
        assert_eq!(snapshot.worktrees[0].relative_path, relative);
        let restored = root.restore(&snapshot, None).join(relative);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&nested, &["rev-parse", "HEAD"])
        );
        assert_eq!(git(&restored, &["diff"]), git(&nested, &["diff"]));
    }
    // Hard-won: 267680121: an override fetch source became an origin in provisioned no-origin checkouts.
    #[test]
    fn integration_review_source_override_fetches_without_adding_provisioned_origin() {
        let root = Repo::new();
        git(&root.checkout, &["remote", "remove", "origin"]);
        let snapshot = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("override.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            Some(root.origin.to_str().unwrap().to_owned()),
        )
        .unwrap();
        assert!(!snapshot.source_had_origin);
        let destination = root.temp.path().join("provisioned");
        git(root.temp.path(), &["init", destination.to_str().unwrap()]);
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: true,
            },
        )
        .unwrap();
        assert_eq!(
            git(&destination, &["rev-parse", "HEAD"]),
            snapshot.head.commit()
        );
        assert!(git(&destination, &["remote"]).is_empty());
        let cloned = root.restore(&snapshot, None);
        assert_eq!(
            git(&cloned, &["remote", "get-url", "origin"]),
            root.origin.to_str().unwrap()
        );
        let mut old = serde_json::to_value(&snapshot).unwrap();
        old.as_object_mut().unwrap().remove("source_had_origin");
        assert!(
            serde_json::from_value::<WorkspaceSnapshot>(old)
                .unwrap()
                .source_had_origin
        );
    }

    // Hard-won: 267680121: a later repository failure leaked every earlier completed snapshot pack.
    #[test]
    fn integration_review_failed_session_cleans_all_new_snapshot_packs() {
        struct NewFileAfterPack(PathBuf);
        impl GitCommandRunner for NewFileAfterPack {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                let output = FileGit.run(repository, command)?;
                if repository == self.0 && command.arguments.iter().any(|arg| arg == "pack-objects")
                {
                    fs::write(repository.join("late-note"), "needs consent")?;
                }
                Ok(output)
            }
        }
        let first = Repo::new();
        let second = Repo::new();
        first.commit("first unpublished work\n");
        second.commit("second unpublished work\n");
        let inputs = [("first", &first), ("second", &second)].map(|(id, root)| RepositoryInput {
            in_container: false,
            id: id.into(),
            checkout: root.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: root.temp.path().join("session"),
            exclusions: ExclusionSource::Origin,
            keep_dir: None,
        });
        let error = capture_session_snapshot(
            &NewFileAfterPack(second.checkout.clone()),
            &inputs,
            &SessionCaptureConsent::default(),
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::Untracked(_))
            ),
            "{error:#}"
        );
        for root in [&first, &second] {
            assert!(
                fs::read_dir(root.temp.path()).unwrap().all(|entry| {
                    let name = entry.unwrap().file_name();
                    name == "checkout" || name == "origin.git"
                }),
                "failed session left pack artifacts"
            );
        }
    }

    // Hard-won: 267680121: each inventory repeated a submodule's unreachable-origin round trip.
    #[test]
    fn integration_review_capture_reuses_network_evidence_but_rechecks_local_consent() {
        struct CountOrigin {
            module: PathBuf,
            root: PathBuf,
            queries: std::sync::atomic::AtomicUsize,
            add_note: std::sync::atomic::AtomicBool,
        }
        impl GitCommandRunner for CountOrigin {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                let args = git_arguments(command);
                if repository == self.module
                    && args.starts_with(&["ls-remote".into(), "origin".into()])
                {
                    self.queries
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                let output = FileGit.run(repository, command)?;
                if repository == self.root
                    && args.iter().any(|arg| arg == "pack-objects")
                    && self
                        .add_note
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    fs::write(repository.join("late-note"), "local consent must be reread")?;
                }
                Ok(output)
            }
        }
        let root = Repo::new();
        let module = Repo::new();
        let module_path = add_submodule(&root, &module);
        fs::rename(&module.origin, module.temp.path().join("offline.git")).unwrap();
        let runner = CountOrigin {
            module: module_path,
            root: root.checkout.clone(),
            queries: 0.into(),
            add_note: false.into(),
        };
        let consent = CaptureConsent {
            untracked: UntrackedConsent::None,
            operations: OperationConsent::None,
        };
        let first = capture_workspace_snapshot(
            &runner,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("cache.pack"),
            &consent,
            None,
        )
        .unwrap();
        assert!(first.remote_refs_stale);
        assert_eq!(runner.queries.load(std::sync::atomic::Ordering::SeqCst), 1);
        runner
            .add_note
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let error = capture_workspace_snapshot(
            &runner,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("cache.pack"),
            &consent,
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::Untracked(_))
            ),
            "{error:#}"
        );
        assert_eq!(runner.queries.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
    // Hard-won: 80921f6c5: an origin switch split recorded URLs from authoritative exclusions.
    #[test]
    fn capture_race_origin_switch_retries_metadata_and_exclusions_together() {
        struct SwitchOrigin {
            root: PathBuf,
            destination: PathBuf,
            switched: std::sync::atomic::AtomicBool,
        }
        impl GitCommandRunner for SwitchOrigin {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                if repository == self.root
                    && git_arguments(command).starts_with(&["ls-remote".into(), "origin".into()])
                    && !self
                        .switched
                        .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    git(
                        repository,
                        &[
                            "remote",
                            "set-url",
                            "origin",
                            self.destination.to_str().unwrap(),
                        ],
                    );
                }
                FileGit.run(repository, command)
            }
        }
        let root = Repo::new();
        root.commit("advertised only by new origin\n");
        let new_origin = root.temp.path().join("new-origin.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                root.checkout.to_str().unwrap(),
                new_origin.to_str().unwrap(),
            ],
        );
        let runner = SwitchOrigin {
            root: root.checkout.clone(),
            destination: new_origin.clone(),
            switched: false.into(),
        };
        let snapshot = capture_workspace_snapshot(
            &runner,
            &root.checkout,
            &CheckoutOwnership::Raw { launch: None },
            &root.temp.path().join("origin-race"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap();
        assert_eq!(snapshot.origin_urls, vec![new_origin.to_str().unwrap()]);
        assert!(snapshot.pack.is_none());
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&root.checkout, &["rev-parse", "HEAD"])
        );
    }

    // Hard-won: 80921f6c5: missing newly-created HEAD branches escaped both cut retry sites.
    #[test]
    fn capture_race_new_head_branch_retries_initial_and_validation_cuts() {
        struct SwitchBranch {
            root: PathBuf,
            switch_at: usize,
            enumerations: std::sync::atomic::AtomicUsize,
        }
        impl GitCommandRunner for SwitchBranch {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                let output = FileGit.run(repository, command)?;
                let args = git_arguments(command);
                if repository == self.root
                    && args.first().is_some_and(|arg| arg == "for-each-ref")
                    && args.iter().any(|arg| arg == "refs/heads")
                    && self
                        .enumerations
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                        + 1
                        == self.switch_at
                {
                    git(repository, &["switch", "-c", "created-during-cut"]);
                }
                Ok(output)
            }
        }
        for switch_at in [1, 2] {
            let root = Repo::new();
            root.commit("local branch work\n");
            let runner = SwitchBranch {
                root: root.checkout.clone(),
                switch_at,
                enumerations: 0.into(),
            };
            let snapshot = capture_workspace_snapshot(
                &runner,
                &root.checkout,
                &CheckoutOwnership::Raw { launch: None },
                &root.temp.path().join("branch-race"),
                &CaptureConsent {
                    untracked: UntrackedConsent::None,
                    operations: OperationConsent::None,
                },
                None,
            )
            .unwrap();
            assert!(
                runner
                    .enumerations
                    .load(std::sync::atomic::Ordering::SeqCst)
                    > switch_at
            );
            assert!(
                matches!(&snapshot.head, SnapshotHead::Branch { name, .. } if name == "refs/heads/created-during-cut")
            );
            let restored = root.restore(&snapshot, None);
            assert_eq!(read_head(&FileGit, &restored).unwrap(), snapshot.head);
        }
    }

    // Hard-won: 80921f6c5: successful overlapping captures borrowed another capture's rollback-owned pack.
    #[test]
    fn capture_race_each_inflight_capture_owns_its_pack_until_success() {
        struct InterleavedCapture {
            root: PathBuf,
            base: PathBuf,
            after_pack: std::sync::atomic::AtomicBool,
            successful: std::sync::Mutex<Option<WorkspaceSnapshot>>,
        }
        impl GitCommandRunner for InterleavedCapture {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                if repository == self.root
                    && self
                        .after_pack
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    let snapshot = capture_workspace_snapshot(
                        &FileGit,
                        &self.root,
                        &CheckoutOwnership::Raw { launch: None },
                        &self.base,
                        &CaptureConsent {
                            untracked: UntrackedConsent::None,
                            operations: OperationConsent::None,
                        },
                        None,
                    )?;
                    *self.successful.lock().unwrap() = Some(snapshot);
                    fs::write(
                        self.root.join("late-note"),
                        "first capture must fail consent",
                    )?;
                }
                let output = FileGit.run(repository, command)?;
                if repository == self.root
                    && command.arguments.iter().any(|arg| arg == "pack-objects")
                {
                    self.after_pack
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
                Ok(output)
            }
        }
        let root = Repo::new();
        root.commit("identical concurrent pack input\n");
        let runner = InterleavedCapture {
            root: root.checkout.clone(),
            base: root.temp.path().join("same-base"),
            after_pack: false.into(),
            successful: Default::default(),
        };
        let error = capture_workspace_snapshot(
            &runner,
            &root.checkout,
            &CheckoutOwnership::Raw { launch: None },
            &runner.base,
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::Untracked(_))
            ),
            "{error:#}"
        );
        let successful = runner.successful.lock().unwrap().take().unwrap();
        assert!(successful.pack.as_ref().unwrap().exists());
        let packs = fs::read_dir(root.temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "pack")
            })
            .collect::<Vec<_>>();
        assert_eq!(packs, vec![successful.pack.clone().unwrap()]);
        let restored = root.restore(&successful, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            successful.head.commit()
        );
    }
    // Hard-won: 5fb537cc2: managed teardown could delete the origin whose objects were omitted.
    #[test]
    fn review_edge_managed_origins_must_survive_all_selected_teardown_roots() {
        let root = Repo::new();
        let mirror = root.checkout.join("hidden.git");
        fs::write(root.checkout.join(".git/info/exclude"), "hidden.git\n").unwrap();
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                root.origin.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        root.conflict();
        root.start_conflict("merge");
        for origin in [
            mirror.to_str().unwrap().to_owned(),
            url::Url::from_directory_path(&mirror).unwrap().to_string(),
        ] {
            git(&root.checkout, &["remote", "set-url", "origin", &origin]);
            let inventory =
                workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
            assert!(matches!(
                inventory.blockers[0].kind,
                SnapshotRefused::OriginRemovedByTeardown(_)
            ));
            let error = capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("refused"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<SnapshotRefused>(),
                    Some(SnapshotRefused::OriginRemovedByTeardown(_))
                ),
                "{error:#}"
            );
            assert!(
                error
                    .to_string()
                    .contains("git remote set-url origin <new path>")
            );
            assert!(root.checkout.join(".git/MERGE_HEAD").exists());
        }
        #[cfg(unix)]
        {
            let alias = root.temp.path().join("outside-alias.git");
            std::os::unix::fs::symlink(&mirror, &alias).unwrap();
            git(
                &root.checkout,
                &["remote", "set-url", "origin", alias.to_str().unwrap()],
            );
            assert!(matches!(
                workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed)
                    .unwrap()
                    .blockers[0]
                    .kind,
                SnapshotRefused::OriginRemovedByTeardown(_)
            ));
        }
        git(
            &root.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", mirror.display()),
                "internal:",
            ],
        );
        git(
            &root.checkout,
            &["remote", "set-url", "origin", "internal:"],
        );
        assert!(matches!(
            workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed)
                .unwrap()
                .blockers[0]
                .kind,
            SnapshotRefused::OriginRemovedByTeardown(_)
        ));
        assert!(
            workspace_inventory(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Raw { launch: None }
            )
            .unwrap()
            .origins_removed_by_teardown
            .is_empty()
        );

        let parent = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&parent, &module);
        let module_mirror = parent.checkout.join("module-mirror.git");
        fs::write(
            parent.checkout.join(".git/info/exclude"),
            "module-mirror.git\n",
        )
        .unwrap();
        git(
            parent.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                module_mirror.to_str().unwrap(),
            ],
        );
        git(
            &parent.checkout,
            &[
                "config",
                "submodule.sub.url",
                module_mirror.to_str().unwrap(),
            ],
        );
        git(
            &directory,
            &[
                "remote",
                "set-url",
                "origin",
                module_mirror.to_str().unwrap(),
            ],
        );
        let inventory =
            workspace_inventory(&FileGit, &parent.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(
            inventory.origins_removed_by_teardown[0]
                .checkout
                .checkout_path,
            Path::new("sub")
        );

        let dependent = Repo::new();
        git(
            &dependent.checkout,
            &[
                "remote",
                "set-url",
                "origin",
                module_mirror.to_str().unwrap(),
            ],
        );
        let inputs =
            [("dependent", &dependent), ("parent", &parent)].map(|(id, root)| RepositoryInput {
                in_container: false,
                id: id.into(),
                checkout: root.checkout.clone(),
                ownership: CheckoutOwnership::Managed,
                origin_override: None,
                pack_path: root.temp.path().join("cross-root"),
                exclusions: ExclusionSource::Origin,
                keep_dir: None,
            });
        let inventory = session_inventory(&FileGit, &inputs).unwrap();
        assert!(matches!(
            inventory["dependent"].blockers[0].kind,
            SnapshotRefused::OriginRemovedByTeardown(_)
        ));
        let error = capture_session_snapshot(&FileGit, &inputs, &SessionCaptureConsent::default())
            .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::OriginRemovedByTeardown(_))
            ),
            "{error:#}"
        );
    }

    // Hard-won: 5fb537cc2: raw pre-operation restore skipped an introduced parent but visited its child.
    #[test]
    fn review_edge_raw_operation_omits_the_entire_introduced_module_subtree() {
        let root = Repo::new();
        let module = Repo::new();
        let leaf = Repo::new();
        add_submodule(&module, &leaf);
        git(&root.checkout, &["switch", "-c", "introduce-module"]);
        add_submodule(&root, &module);
        git(
            &root.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "--recursive",
            ],
        );
        git(&root.checkout, &["switch", "main"]);
        git(
            &root.checkout,
            &["merge", "--no-ff", "--no-commit", "introduce-module"],
        );
        let snapshot = root.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(snapshot.working_state_skipped);
        assert_eq!(snapshot.initialized_submodules.len(), 2);
        let restored = root.restore(&snapshot, None);
        assert!(!restored.join("sub").exists());
        assert!(root.checkout.join("sub/sub/.git").exists());
        assert!(root.checkout.join(".git/MERGE_HEAD").exists());
    }

    // Hard-won: 5fb537cc2: line-delimited push destinations split a single newline-containing path.
    #[test]
    fn review_edge_newline_push_paths_are_unavailable() {
        let root = Repo::new();
        let newline = root.temp.path().join("publish\nrepo.git");
        git(
            &root.checkout,
            &["config", "remote.origin.pushurl", newline.to_str().unwrap()],
        );
        assert!(matches!(
            capture_push_evidence(&FileGit, &root.checkout, true, false).unwrap(),
            PushEvidence::Unavailable { .. }
        ));
        let carriage_return = root.temp.path().join("publish\rrepo.git");
        git(
            &root.checkout,
            &[
                "config",
                "remote.origin.pushurl",
                carriage_return.to_str().unwrap(),
            ],
        );
        assert_eq!(
            capture_push_evidence(&FileGit, &root.checkout, true, false).unwrap(),
            PushEvidence::Resolved(vec![carriage_return.to_str().unwrap().to_owned()])
        );
    }

    // Hard-won: 5fb537cc2: non-UTF-8 saved paths failed serialization only after a managed abort.
    #[cfg(unix)]
    #[test]
    fn review_edge_non_utf8_snapshot_paths_refuse_before_managed_abort() {
        use std::os::unix::ffi::OsStringExt;
        for worktree in [false, true] {
            let root = Repo::new();
            let relative = PathBuf::from(OsString::from_vec(b"bad-\xff".to_vec()));
            if worktree {
                let output = FileGit
                    .run(
                        &root.checkout,
                        &GitCommand {
                            arguments: vec![
                                "worktree".into(),
                                "add".into(),
                                "--detach".into(),
                                root.checkout.join(&relative).into_os_string(),
                            ],
                            ..Default::default()
                        },
                    )
                    .unwrap();
                assert_eq!(
                    output.status,
                    0,
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            root.conflict();
            root.start_conflict("merge");
            if !worktree {
                let mut entry = OsString::from(format!(
                    "160000,{},",
                    git(&root.checkout, &["rev-parse", "HEAD"])
                ));
                entry.push(relative.as_os_str());
                let output = FileGit
                    .run(
                        &root.checkout,
                        &GitCommand {
                            arguments: vec![
                                "update-index".into(),
                                "--add".into(),
                                "--cacheinfo".into(),
                                entry,
                            ],
                            ..Default::default()
                        },
                    )
                    .unwrap();
                assert_eq!(output.status, 0);
            }
            let before = fs::read(root.checkout.join(".git/index")).unwrap();
            let error = capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("non-utf8"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("UTF-8"), "{error:#}");
            assert!(root.checkout.join(".git/MERGE_HEAD").exists());
            assert_eq!(fs::read(root.checkout.join(".git/index")).unwrap(), before);
        }
    }

    // Hard-won: 5fb537cc2: fresh neutral directories defeated reuse of uninitialized-pin advertisements.
    #[test]
    fn review_edge_uninitialized_pin_reuses_one_neutral_advertisement() {
        struct CountProbe {
            url: String,
            offline: PathBuf,
            queries: std::sync::atomic::AtomicUsize,
        }
        impl GitCommandRunner for CountProbe {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                let probe = git_arguments(command)
                    .starts_with(&["ls-remote".into(), self.url.clone().into()]);
                let output = FileGit.run(repository, command)?;
                if probe
                    && self
                        .queries
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                        == 0
                {
                    fs::rename(&self.url, &self.offline)?;
                }
                Ok(output)
            }
        }
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(root.checkout.join(".git/modules/sub")).unwrap();
        module.commit("new published uninitialized pin\n");
        git(&module.checkout, &["push", "origin", "main"]);
        let pin = git(&module.checkout, &["rev-parse", "HEAD"]);
        git(
            &root.checkout,
            &["update-index", "--cacheinfo", &format!("160000,{pin},sub")],
        );
        let runner = CountProbe {
            url: module.origin.to_str().unwrap().to_owned(),
            offline: module.temp.path().join("offline.git"),
            queries: 0.into(),
        };
        let snapshot = capture_workspace_snapshot(
            &runner,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("probe"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap();
        assert_eq!(runner.queries.load(std::sync::atomic::Ordering::SeqCst), 1);
        fs::rename(&runner.offline, &module.origin).unwrap();
        let restored = root.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", ":sub"]), pin);
        assert!(!restored.join("sub/.git").exists());
    }
    // Hard-won: 5fb537cc2: an outside mirror remained but its saved route traversed the deleted checkout.
    #[test]
    fn review_edge_deleted_origin_routes_offer_the_canonical_absolute_command() {
        let root = Repo::new();
        git(
            &root.checkout,
            &["remote", "set-url", "origin", "../origin.git"],
        );
        let check = |origin: &Path| {
            let inventory =
                workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
            let blocker = &inventory.blockers[0];
            let SnapshotRefused::OriginRemovedByTeardown(origins) = &blocker.kind else {
                panic!("{}", blocker.remedy);
            };
            assert!(!origins[0].value.target_is_removed);
            assert_eq!(
                origins[0].value.canonical_path,
                fs::canonicalize(origin).unwrap()
            );
            assert!(blocker.remedy.contains(&format!(
                "git remote set-url origin {}",
                mj_core::targets::posix_quote(fs::canonicalize(origin).unwrap().to_str().unwrap())
            )));
        };
        check(&root.origin);
        let file_route = format!(
            "{}../origin.git",
            url::Url::from_directory_path(&root.checkout).unwrap()
        );
        git(
            &root.checkout,
            &["remote", "set-url", "origin", &file_route],
        );
        check(&root.origin);
        #[cfg(unix)]
        {
            let punctuated = root.temp.path().join("outside?literal#name.git");
            git(
                root.temp.path(),
                &[
                    "clone",
                    "--bare",
                    root.origin.to_str().unwrap(),
                    punctuated.to_str().unwrap(),
                ],
            );
            let raw_file_route = format!(
                "{}../outside?literal#name.git",
                url::Url::from_directory_path(&root.checkout).unwrap()
            );
            git(
                &root.checkout,
                &["remote", "set-url", "origin", &raw_file_route],
            );
            check(&punctuated);
        }
        #[cfg(unix)]
        {
            let link = root.checkout.join("origin-link.git");
            std::os::unix::fs::symlink(&root.origin, &link).unwrap();
            git(
                &root.checkout,
                &["remote", "set-url", "origin", link.to_str().unwrap()],
            );
            check(&root.origin);
        }
        let raw = root.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(raw.source_had_origin);
        git(
            &root.checkout,
            &["remote", "set-url", "origin", root.origin.to_str().unwrap()],
        );
        let managed = root.capture(&CheckoutOwnership::Managed);
        let pack = managed.pack.clone();
        let restored = root.temp.path().join("after-teardown");
        fs::remove_dir_all(&root.checkout).unwrap();
        restore_workspace_snapshot(&FileGit, &managed, &restored, &clone_base(&managed, None))
            .unwrap();
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            managed.head.commit()
        );
        assert!(pack.is_none());
        let parent = Repo::new();
        let module = Repo::new();
        let directory = add_submodule(&parent, &module);
        let outside = parent.temp.path().join("module-origin.git");
        git(
            parent.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                outside.to_str().unwrap(),
            ],
        );
        git(
            &parent.checkout,
            &["config", "submodule.sub.url", outside.to_str().unwrap()],
        );
        git(
            &directory,
            &["remote", "set-url", "origin", "../../module-origin.git"],
        );
        let inventory =
            workspace_inventory(&FileGit, &parent.checkout, &CheckoutOwnership::Managed).unwrap();
        let blocker = &inventory.blockers[0];
        let canonical = fs::canonicalize(&outside).unwrap();
        assert!(blocker.remedy.contains(&format!(
            "git -C {} remote set-url origin {}",
            mj_core::targets::posix_quote("sub"),
            mj_core::targets::posix_quote(canonical.to_str().unwrap())
        )));
        git(
            &directory,
            &["remote", "set-url", "origin", canonical.to_str().unwrap()],
        );
        assert!(
            workspace_inventory(&FileGit, &parent.checkout, &CheckoutOwnership::Managed)
                .unwrap()
                .origins_removed_by_teardown
                .is_empty()
        );
    }
    // Hard-won: 1d7cc60a7: consecutive identical stash OIDs did not create consecutive reflog entries.
    #[test]
    fn restore_edge_adjacent_stash_entries_preserve_every_oid_and_message() {
        let root = Repo::new();
        fs::write(root.checkout.join("file"), "stash A\n").unwrap();
        let a = git(&root.checkout, &["stash", "create"]);
        fs::write(root.checkout.join("file"), "stash B\n").unwrap();
        let b = git(&root.checkout, &["stash", "create"]);
        git(&root.checkout, &["stash", "store", "-m", "oldest A", &a]);
        for message in ["middle A", "newest A"] {
            git(&root.checkout, &["stash", "store", "-m", "temporary B", &b]);
            git(&root.checkout, &["stash", "store", "-m", message, &a]);
            git(&root.checkout, &["stash", "drop", "stash@{1}"]);
        }
        git(&root.checkout, &["reset", "--hard", "HEAD"]);
        let before = read_stash(&FileGit, &root.checkout).unwrap();
        assert_eq!(before.len(), 3);
        assert!(before.iter().all(|entry| entry.commit == a));
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&snapshot, None);
        assert_eq!(read_stash(&FileGit, &restored).unwrap(), before);
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &restored,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert_eq!(read_stash(&FileGit, &restored).unwrap(), before);
    }

    // Hard-won: 1d7cc60a7: no-origin provisioned restores resolve relative module URLs from a different cwd.
    #[test]
    fn restore_edge_no_origin_relative_staged_pin_requires_initialization_or_absolute_url() {
        let root = Repo::new();
        let module = Repo::new();
        let target = Repo::new();
        add_submodule(&root, &module);
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(root.checkout.join(".git/modules/sub")).unwrap();
        module.commit("new pin only under the override sibling\n");
        let pin = git(&module.checkout, &["rev-parse", "HEAD"]);
        let authoritative = target.temp.path().join("lib.git");
        git(
            target.temp.path(),
            &[
                "clone",
                "--bare",
                module.checkout.to_str().unwrap(),
                authoritative.to_str().unwrap(),
            ],
        );
        git(
            &root.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.url",
                "../lib.git",
            ],
        );
        git(&root.checkout, &["add", ".gitmodules"]);
        git(
            &root.checkout,
            &["update-index", "--cacheinfo", &format!("160000,{pin},sub")],
        );
        git(&root.checkout, &["remote", "remove", "origin"]);
        let capture = || {
            capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("no-origin"),
                &CaptureConsent {
                    untracked: UntrackedConsent::None,
                    operations: OperationConsent::None,
                },
                Some(target.origin.to_str().unwrap().to_owned()),
            )
        };
        let error = capture().unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::DirtySubmodule(_))
            ),
            "{error:#}"
        );
        for step in ["sub", "git submodule update --init", "absolute"] {
            assert!(error.to_string().contains(step), "{error:#}");
        }
        git(
            &root.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.url",
                authoritative.to_str().unwrap(),
            ],
        );
        git(&root.checkout, &["add", ".gitmodules"]);
        let snapshot = capture().unwrap();
        assert!(!snapshot.source_had_origin);
        let destination = target.temp.path().join("elsewhere/provisioned");
        git(
            target.temp.path(),
            &[
                "clone",
                root.checkout.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        git(&destination, &["remote", "remove", "origin"]);
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert!(git(&destination, &["remote"]).is_empty());
        git(
            &destination,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "sub",
            ],
        );
        assert_eq!(git(&destination.join("sub"), &["rev-parse", "HEAD"]), pin);
    }

    // Hard-won: 1d7cc60a7: generic URL joins treated literal file-origin punctuation as query/fragment syntax.
    #[cfg(unix)]
    #[test]
    fn restore_edge_relative_file_module_urls_keep_literal_path_punctuation() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(root.checkout.join(".git/modules/sub")).unwrap();
        let directory = root.temp.path().join("org#1?team");
        fs::create_dir(&directory).unwrap();
        let app = directory.join("app.git");
        fs::rename(&root.origin, &app).unwrap();
        git(
            &root.checkout,
            &[
                "remote",
                "set-url",
                "origin",
                &format!("file://{}", app.display()),
            ],
        );
        let wrong = root.temp.path().join("lib.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                wrong.to_str().unwrap(),
            ],
        );
        let old = git(&module.checkout, &["rev-parse", "HEAD"]);
        module.commit("pin published only inside literal directory\n");
        let pin = git(&module.checkout, &["rev-parse", "HEAD"]);
        let authoritative = directory.join("lib.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                module.checkout.to_str().unwrap(),
                authoritative.to_str().unwrap(),
            ],
        );
        git(
            &root.checkout,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.sub.url",
                "../lib.git",
            ],
        );
        git(&root.checkout, &["add", ".gitmodules"]);
        git(
            &root.checkout,
            &["update-index", "--cacheinfo", &format!("160000,{pin},sub")],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&snapshot, None);
        git(
            &restored,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "sub",
            ],
        );
        assert_eq!(git(&restored.join("sub"), &["rev-parse", "HEAD"]), pin);
        git(
            &wrong,
            &["fetch", module.checkout.to_str().unwrap(), "main"],
        );
        git(&wrong, &["update-ref", "refs/heads/main", &pin]);
        git(&authoritative, &["update-ref", "refs/heads/main", &old]);
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("wrong-origin"),
            &CaptureConsent {
                untracked: UntrackedConsent::None,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::DirtySubmodule(_))
            ),
            "{error:#}"
        );
    }

    // Hard-won: 1d7cc60a7: parent bootstrap fetched child modules before installing their recorded origins.
    #[test]
    fn restore_edge_parent_fetches_never_recurse_into_unconfigured_children() {
        let root = Repo::new();
        let module = Repo::new();
        let leaf = Repo::new();
        let source_child = add_submodule(&module, &leaf);
        add_submodule(&root, &module);
        git(
            &root.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "--recursive",
            ],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let saved = git(&root.checkout.join("sub/sub"), &["rev-parse", "HEAD"]);
        for recursion in ["true", "on-demand"] {
            let destination = root.temp.path().join(format!("provisioned-{recursion}"));
            git(
                root.temp.path(),
                &[
                    "clone",
                    root.origin.to_str().unwrap(),
                    destination.to_str().unwrap(),
                ],
            );
            git(
                &destination,
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "update",
                    "--init",
                    "--recursive",
                ],
            );
            git(
                &destination,
                &["config", "fetch.recurseSubmodules", recursion],
            );
            git(
                &destination.join("sub"),
                &["config", "fetch.recurseSubmodules", recursion],
            );
            git(
                &destination.join("sub/sub"),
                &[
                    "remote",
                    "set-url",
                    "origin",
                    root.temp.path().join("unreachable.git").to_str().unwrap(),
                ],
            );
            leaf.commit(&format!("new child commit for {recursion}\n"));
            git(&leaf.checkout, &["push", "origin", "main"]);
            git(&source_child, &["fetch", "origin"]);
            git(
                &source_child,
                &[
                    "checkout",
                    "--detach",
                    &git(&leaf.checkout, &["rev-parse", "HEAD"]),
                ],
            );
            git(&module.checkout, &["add", "sub"]);
            git(
                &module.checkout,
                &["commit", "-m", "advance published child"],
            );
            git(&module.checkout, &["push", "origin", "main"]);
            restore_workspace_snapshot(
                &FileGit,
                &snapshot,
                &destination,
                &RestoreBase::Provisioned {
                    allow_origin_fetch: true,
                },
            )
            .unwrap();
            assert_eq!(
                git(
                    &destination.join("sub/sub"),
                    &["remote", "get-url", "origin"]
                ),
                leaf.origin.to_str().unwrap()
            );
            assert_eq!(
                git(&destination.join("sub/sub"), &["rev-parse", "HEAD"]),
                saved
            );
        }
    }
    // Hard-won: 49a393b57: an origin-less metadata snapshot could reset its own source on restore.
    #[test]
    fn review_seventh_in_place_metadata_never_restores_into_a_provisioned_checkout() {
        let root = Repo::new();
        fs::write(root.checkout.join("file"), "saved stash\n").unwrap();
        git(&root.checkout, &["stash", "push", "-m", "keep me"]);
        fs::write(root.checkout.join("file"), "staged\n").unwrap();
        git(&root.checkout, &["add", "file"]);
        fs::write(root.checkout.join("file"), "unstaged\n").unwrap();
        git(&root.checkout, &["remote", "remove", "origin"]);
        let snapshot = root.capture(&CheckoutOwnership::Raw { launch: None });
        let index = fs::read(root.checkout.join(".git/index")).unwrap();
        let refs = git(&root.checkout, &["show-ref"]);
        let stash = git(&root.checkout, &["stash", "list"]);
        for allow_origin_fetch in [false, true] {
            let error = restore_workspace_snapshot(
                &FileGit,
                &snapshot,
                &root.checkout,
                &RestoreBase::Provisioned { allow_origin_fetch },
            )
            .unwrap_err();
            assert!(error.to_string().contains("in-place-only"), "{error:#}");
            assert_eq!(fs::read(root.checkout.join(".git/index")).unwrap(), index);
            assert_eq!(git(&root.checkout, &["show-ref"]), refs);
            assert_eq!(git(&root.checkout, &["stash", "list"]), stash);
            assert_eq!(fs::read(root.checkout.join("file")).unwrap(), b"unstaged\n");
        }
    }

    // Hard-won: 49a393b57: a later missing managed origin was discovered after aborting earlier operations.
    #[test]
    fn review_seventh_missing_managed_origin_blocks_every_abort_in_the_session() {
        let first = Repo::new();
        let second = Repo::new();
        for root in [&first, &second] {
            root.conflict();
            root.start_conflict("merge");
        }
        git(&second.checkout, &["remote", "remove", "origin"]);
        let input = |id: &str, root: &Repo| RepositoryInput {
            id: id.into(),
            checkout: root.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: root.temp.path().join("snapshot.pack"),
            exclusions: ExclusionSource::Origin,
            keep_dir: None,
            in_container: false,
        };
        let inputs = [input("first", &first), input("second", &second)];
        let consent = SessionCaptureConsent {
            repositories: inputs
                .iter()
                .map(|input| {
                    (
                        input.id.clone(),
                        CaptureConsent {
                            untracked: UntrackedConsent::Any,
                            operations: OperationConsent::Any,
                        },
                    )
                })
                .collect(),
        };
        let before: Vec<_> = [&first, &second]
            .map(|root| {
                (
                    fs::read(root.checkout.join(".git/index")).unwrap(),
                    fs::read(root.checkout.join("file")).unwrap(),
                )
            })
            .into();
        let error = capture_session_snapshot(&FileGit, &inputs, &consent).unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::MissingManagedOrigin(_))
            ),
            "{error:#}"
        );
        for (root, (index, file)) in [&first, &second].into_iter().zip(before) {
            assert!(root.checkout.join(".git/MERGE_HEAD").exists());
            assert_eq!(fs::read(root.checkout.join(".git/index")).unwrap(), index);
            assert_eq!(fs::read(root.checkout.join("file")).unwrap(), file);
        }
    }

    // Hard-won: 49a393b57: publication of an uninitialized pin depended on a mirror deleted at teardown.
    #[test]
    fn review_seventh_uninitialized_module_mirrors_must_survive_teardown() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(root.checkout.join(".git/modules")).unwrap();
        let mirror = root.checkout.join("mirror.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        fs::write(root.checkout.join(".git/info/exclude"), "mirror.git\n").unwrap();
        let host = host_git(root.temp.path(), "module-routes");
        git(
            root.temp.path(),
            &[
                "config",
                "--file",
                host.global.to_str().unwrap(),
                &format!("url.{}.insteadOf", mirror.display()),
                "module-mirror:",
            ],
        );
        git(
            &root.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", module.origin.display()),
                "module-mirror:",
            ],
        );
        git(
            &root.checkout,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.sub.url",
                "module-mirror:",
            ],
        );
        for changed_pin in [false, true] {
            if changed_pin {
                module.commit("new pin\n");
                git(
                    &module.checkout,
                    &["push", mirror.to_str().unwrap(), "main"],
                );
                git(
                    &root.checkout,
                    &[
                        "update-index",
                        "--cacheinfo",
                        &format!(
                            "160000,{},sub",
                            git(&module.checkout, &["rev-parse", "HEAD"])
                        ),
                    ],
                );
            }
            let inventory =
                workspace_inventory(&host, &root.checkout, &CheckoutOwnership::Managed).unwrap();
            assert_eq!(inventory.origins_removed_by_teardown.len(), 1);
            let dependency = &inventory.origins_removed_by_teardown[0];
            assert_eq!(
                dependency.value.canonical_path,
                fs::canonicalize(&mirror).unwrap()
            );
            assert_eq!(
                dependency.value.module_url_key.as_deref(),
                Some("submodule.sub.url")
            );
            let error = capture_workspace_snapshot(
                &host,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("refused.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<SnapshotRefused>(),
                    Some(SnapshotRefused::OriginRemovedByTeardown(_))
                ),
                "{error:#}"
            );
            assert!(error.to_string().contains("config -f .gitmodules"));
            assert!(
                workspace_inventory(
                    &host,
                    &root.checkout,
                    &CheckoutOwnership::Raw { launch: None }
                )
                .unwrap()
                .origins_removed_by_teardown
                .is_empty()
            );
        }
        // Updating .gitmodules to an independent origin removes the dependency.
        git(
            &root.checkout,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.sub.url",
                mirror.to_str().unwrap(),
            ],
        );
        git(
            &root.checkout,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.sub.url",
                module.origin.to_str().unwrap(),
            ],
        );
        assert!(
            workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed)
                .unwrap()
                .origins_removed_by_teardown
                .is_empty()
        );
    }

    // Hard-won: 49a393b57: destination-owned initialized modules were recursively reset before restoration.
    #[test]
    fn review_seventh_provisioned_extra_module_keeps_its_checkout_and_reports_it() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(root.checkout.join(".git/modules")).unwrap();
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let destination = root.temp.path().join("provisioned");
        git(
            root.temp.path(),
            &[
                "clone",
                root.origin.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        git(
            &destination,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
            ],
        );
        let child = destination.join("sub");
        git(&child, &["checkout", "-b", "destination-owned"]);
        fs::write(child.join("file"), "destination commit\n").unwrap();
        git(&child, &["commit", "-am", "destination work"]);
        fs::write(child.join("file"), "staged destination\n").unwrap();
        git(&child, &["add", "file"]);
        fs::write(child.join("file"), "unstaged destination\n").unwrap();
        fs::write(child.join("untracked"), "keep\n").unwrap();
        git(&destination, &["config", "submodule.recurse", "true"]);
        let head = git(&child, &["rev-parse", "HEAD"]);
        let diff = git(&child, &["diff"]);
        let staged = git(&child, &["diff", "--cached"]);
        // Observe the notice on the same real restore path without changing its API.
        struct Notices(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        impl tracing::Subscriber for Notices {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                struct Fields(String);
                impl tracing::field::Visit for Fields {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        self.0.push_str(&format!("{}={value:?} ", field.name()));
                    }
                }
                let mut fields = Fields(String::new());
                event.record(&mut fields);
                self.0.lock().unwrap().push(fields.0);
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        let notices = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        tracing::subscriber::with_default(Notices(notices.clone()), || {
            restore_workspace_snapshot(
                &FileGit,
                &snapshot,
                &destination,
                &RestoreBase::Provisioned {
                    allow_origin_fetch: true,
                },
            )
        })
        .unwrap();
        assert_eq!(git(&child, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(&child, &["diff"]), diff);
        assert_eq!(git(&child, &["diff", "--cached"]), staged);
        assert_eq!(fs::read(child.join("untracked")).unwrap(), b"keep\n");
        assert!(git(&destination, &["status", "--porcelain"]).contains("sub"));
        assert!(
            notices
                .lock()
                .unwrap()
                .iter()
                .any(|notice| notice.contains("uninitialized") && notice.contains("sub")),
            "{:?}",
            notices.lock().unwrap()
        );
    }

    // Hard-won: 49a393b57: uninitialized modules each copied and scanned the entire parent index.
    #[test]
    fn review_seventh_uninitialized_inventory_batches_large_path_sets() {
        let root = Repo::new();
        let oid = git(&root.checkout, &["rev-parse", "HEAD"]);
        let mut config = String::new();
        let mut stdin = Vec::new();
        let mut expected = Vec::new();
        for n in 0..180 {
            let path = PathBuf::from(format!(
                "modules/{n:03}-{}-{}",
                "a".repeat(110),
                "b".repeat(110)
            ))
            .join("c".repeat(230));
            let text = crate::archive::slash_path(&path).unwrap();
            stdin.extend(format!("160000 {oid}\t{text}\0").as_bytes());
            config.push_str(&format!(
                "[submodule \"module-{n}\"]\npath = {text}\nurl = {}\n",
                root.origin.display()
            ));
            if n < 160 {
                fs::create_dir_all(root.checkout.join(&path)).unwrap();
                fs::write(root.checkout.join(&path).join("note"), b"discard consent").unwrap();
                expected.push(path.join("note"));
            } else if n < 170 {
                fs::create_dir_all(root.checkout.join(&path)).unwrap();
            } else if n == 179 {
                fs::create_dir_all(root.checkout.join(&path).parent().unwrap()).unwrap();
                fs::write(root.checkout.join(&path), b"ordinary replacement file").unwrap();
                expected.push(path);
            }
        }
        assert!(stdin.len() > 65_536);
        let output = FileGit
            .run(
                &root.checkout,
                &GitCommand {
                    arguments: ["update-index", "-z", "--index-info"]
                        .map(Into::into)
                        .to_vec(),
                    stdin,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            output.status,
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::write(root.checkout.join(".gitmodules"), config).unwrap();
        git(&root.checkout, &["add", ".gitmodules"]);
        git(
            &root.checkout,
            &["commit", "-m", "many uninitialized modules"],
        );
        let before = fs::read(root.checkout.join(".git/index")).unwrap();
        #[derive(Default)]
        struct Count(std::sync::Mutex<(usize, usize, usize, usize)>);
        impl GitCommandRunner for Count {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                let args = git_arguments(command);
                {
                    let mut count = self.0.lock().unwrap();
                    if args.iter().any(|arg| arg == "ls-files")
                        && args.iter().any(|arg| arg == "--stage")
                    {
                        count.0 += 1;
                    }
                    if args.iter().any(|arg| arg == "update-index")
                        && command.env.iter().any(|(key, _)| key == "GIT_INDEX_FILE")
                    {
                        count.1 += 1;
                        count.2 = command.stdin.len();
                        count.3 = command.stdin.iter().filter(|byte| **byte == 0).count();
                    }
                }
                FileGit.run(repository, command)
            }
        }
        let runner = Count::default();
        let inspection =
            crate::checkpoint::inspect_snapshot_submodules(&runner, &root.checkout, None, true)
                .unwrap();
        assert_eq!(inspection.untracked, expected);
        let count = *runner.0.lock().unwrap();
        assert_eq!(
            count.0, 2,
            "index reads must be independent of module count"
        );
        assert_eq!(
            count.1, 1,
            "one private index serves all nonempty directories"
        );
        assert!(count.2 > 65_536, "exercise concurrent pipe draining");
        assert_eq!(
            count.3, 161,
            "absent and empty directories need no private-index edits"
        );
        assert_eq!(fs::read(root.checkout.join(".git/index")).unwrap(), before);
    }
    // Hard-won: 49a393b57: recursive resets ran before saved child objects and origins were restored.
    #[test]
    fn review_seventh_recursive_reset_configuration_cannot_reset_unprepared_children() {
        let root = Repo::new();
        let module = Repo::new();
        let leaf = Repo::new();
        let source_leaf = add_submodule(&module, &leaf);
        let source_module = add_submodule(&root, &module);
        git(
            &root.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "--recursive",
            ],
        );
        let destination = root.temp.path().join("provisioned");
        git(
            root.temp.path(),
            &[
                "clone",
                root.origin.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        git(
            &destination,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "--recursive",
            ],
        );
        leaf.commit("new child unavailable in provisioned object store\n");
        git(&leaf.checkout, &["push", "origin", "main"]);
        git(&source_leaf, &["fetch", "origin"]);
        let leaf_head = git(&leaf.checkout, &["rev-parse", "HEAD"]);
        git(&source_leaf, &["checkout", "--detach", &leaf_head]);
        git(&module.checkout, &["commit", "-am", "advance child"]);
        git(&module.checkout, &["push", "origin", "main"]);
        git(&source_module, &["fetch", "origin"]);
        git(
            &source_module,
            &[
                "checkout",
                "--detach",
                &git(&module.checkout, &["rev-parse", "HEAD"]),
            ],
        );
        git(
            &source_module,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "--recursive",
            ],
        );
        git(&root.checkout, &["commit", "-am", "advance module"]);
        git(&root.checkout, &["push", "origin", "main"]);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        for checkout in [&destination, &destination.join("sub")] {
            git(checkout, &["config", "submodule.recurse", "true"]);
        }
        let child_before = run_git(
            &FileGit,
            &destination.join("sub/sub"),
            ["cat-file", "-e", &leaf_head],
            &[],
        )
        .unwrap();
        assert_ne!(child_before.status, 0);
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: true,
            },
        )
        .unwrap();
        assert_eq!(
            git(&destination.join("sub/sub"), &["rev-parse", "HEAD"]),
            leaf_head
        );
        assert!(git(&destination, &["status", "--porcelain"]).is_empty());
    }
    // Hard-won: 49a393b57: route preflight must not parse conflict markers before operation consent.
    #[test]
    fn review_seventh_uninitialized_routes_use_committed_gitmodules_during_conflicts() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(root.checkout.join(".git/modules")).unwrap();
        git(
            &root.checkout,
            &["checkout", "-b", "conflicting-module-url"],
        );
        git(
            &root.checkout,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.sub.url",
                "https://side.invalid/repo.git",
            ],
        );
        git(&root.checkout, &["commit", "-am", "side module URL"]);
        git(&root.checkout, &["checkout", "main"]);
        git(
            &root.checkout,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.sub.url",
                "https://main.invalid/repo.git",
            ],
        );
        git(&root.checkout, &["commit", "-am", "main module URL"]);
        assert_ne!(
            run_git(
                &FileGit,
                &root.checkout,
                ["merge", "conflicting-module-url"],
                &[]
            )
            .unwrap()
            .status,
            0
        );
        let before = fs::read(root.checkout.join(".gitmodules")).unwrap();
        assert!(String::from_utf8_lossy(&before).contains("<<<<<<<"));
        let inventory =
            workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
        assert!(inventory.dirty_submodules.is_empty());
        assert!(inventory.origins_removed_by_teardown.is_empty());
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("snapshot.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::None,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::OperationInProgress(_))
            ),
            "{error:#}"
        );
        assert_eq!(fs::read(root.checkout.join(".gitmodules")).unwrap(), before);
        assert!(root.checkout.join(".git/MERGE_HEAD").exists());
    }
    // Hard-won: 1548f4aa4: copied module .git files could make destination restore rewrite the source.
    #[test]
    fn review_eighth_copied_submodule_admin_is_rejected_before_writes() {
        let root = Repo::new();
        let module = Repo::new();
        let source = add_submodule(&root, &module);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let destination = root.temp.path().join("provisioned");
        git(
            root.temp.path(),
            &[
                "clone",
                root.origin.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        let copied = destination.join("sub");
        fs::create_dir_all(&copied).unwrap();
        let admin = crate::archive::git::git_directories(&FileGit, &source)
            .unwrap()
            .0;
        fs::write(
            copied.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        for lie_about_worktree in [false, true] {
            if lie_about_worktree {
                git(
                    &source,
                    &["config", "core.worktree", copied.to_str().unwrap()],
                );
            }
            let config = fs::read(admin.join("config")).unwrap();
            let index = fs::read(admin.join("index")).unwrap();
            let head = fs::read(admin.join("HEAD")).unwrap();
            let destination_config = fs::read(destination.join(".git/config")).unwrap();
            let refs = git(&destination, &["show-ref"]);
            let error = restore_workspace_snapshot(
                &FileGit,
                &snapshot,
                &destination,
                &RestoreBase::Provisioned {
                    allow_origin_fetch: true,
                },
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("worktree root")
                    || error.to_string().contains("destination-owned"),
                "{error:#}"
            );
            assert_eq!(fs::read(admin.join("config")).unwrap(), config);
            assert_eq!(fs::read(admin.join("index")).unwrap(), index);
            assert_eq!(fs::read(admin.join("HEAD")).unwrap(), head);
            assert_eq!(
                fs::read(destination.join(".git/config")).unwrap(),
                destination_config
            );
            assert_eq!(git(&destination, &["show-ref"]), refs);
        }
    }

    // Hard-won: 1548f4aa4: failed module initialization discarded destination files moved aside.
    #[test]
    fn review_eighth_provisioned_module_files_survive_failure_collisions_and_success() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        for collision in [false, true] {
            let destination = root.temp.path().join(format!("provisioned-{collision}"));
            git(
                root.temp.path(),
                &[
                    "clone",
                    root.origin.to_str().unwrap(),
                    destination.to_str().unwrap(),
                ],
            );
            fs::create_dir_all(destination.join("sub/drafts")).unwrap();
            fs::write(destination.join("sub/drafts/notes"), b"destination notes").unwrap();
            if collision {
                fs::write(destination.join("sub/file"), b"destination collision").unwrap();
            }
            let error = restore_workspace_snapshot(
                &FileGit,
                &snapshot,
                &destination,
                &RestoreBase::Provisioned {
                    allow_origin_fetch: false,
                },
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("locally provisioned"),
                "{error:#}"
            );
            assert_eq!(
                fs::read(destination.join("sub/drafts/notes")).unwrap(),
                b"destination notes"
            );
            assert!(!destination.join("sub/.git").exists());
            let base = RestoreBase::Provisioned {
                allow_origin_fetch: true,
            };
            if collision {
                let error = restore_workspace_snapshot(&FileGit, &snapshot, &destination, &base)
                    .unwrap_err();
                assert!(
                    error.to_string().contains("collide") && error.to_string().contains("sub/file"),
                    "{error:#}"
                );
                assert_eq!(
                    fs::read(destination.join("sub/file")).unwrap(),
                    b"destination collision"
                );
                assert_eq!(
                    fs::read(destination.join("sub/drafts/notes")).unwrap(),
                    b"destination notes"
                );
                assert!(!destination.join("sub/.git").exists());
                fs::rename(
                    destination.join("sub/file"),
                    destination.join("kept-collision"),
                )
                .unwrap();
            }
            restore_workspace_snapshot(&FileGit, &snapshot, &destination, &base).unwrap();
            assert!(destination.join("sub/.git").exists());
            assert_eq!(
                fs::read(destination.join("sub/file")).unwrap(),
                fs::read(module.checkout.join("file")).unwrap()
            );
            assert_eq!(
                fs::read(destination.join("sub/drafts/notes")).unwrap(),
                b"destination notes"
            );
        }
    }

    // Hard-won: 1548f4aa4: bare repositories and checkout subdirectories passed provisioned validation.
    #[test]
    fn review_eighth_provisioned_restore_requires_the_exact_worktree_root() {
        let source = Repo::new();
        source.commit("snapshot-only commit\n");
        let snapshot = source.capture(&CheckoutOwnership::Managed);
        let destination = Repo::new();
        let subdir = destination.checkout.join("ordinary");
        fs::create_dir(&subdir).unwrap();
        for (path, owner, config) in [
            (
                &destination.origin,
                &destination.origin,
                destination.origin.join("config"),
            ),
            (
                &subdir,
                &destination.checkout,
                destination.checkout.join(".git/config"),
            ),
        ] {
            let before = fs::read(&config).unwrap();
            let refs = git(owner, &["show-ref"]);
            let error = restore_workspace_snapshot(
                &FileGit,
                &snapshot,
                path,
                &RestoreBase::Provisioned {
                    allow_origin_fetch: false,
                },
            )
            .unwrap_err();
            assert!(error.to_string().contains("worktree root"), "{error:#}");
            assert_eq!(fs::read(config).unwrap(), before);
            assert_eq!(git(owner, &["show-ref"]), refs);
        }
    }

    // Hard-won: 1548f4aa4: destination symrefs redirected saved-ref updates and deletions to another ref.
    #[test]
    fn review_eighth_saved_refs_replace_destination_aliases_without_following_them() {
        let root = Repo::new();
        git(&root.checkout, &["checkout", "-b", "release"]);
        root.commit("release commit\n");
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let destination = root.temp.path().join("provisioned");
        git(
            root.temp.path(),
            &[
                "clone",
                root.origin.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        for alias in [
            "refs/heads/release",
            "refs/heads/destination-only",
            "refs/stash",
        ] {
            git(&destination, &["symbolic-ref", alias, "refs/heads/main"]);
        }
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert_eq!(
            git(&destination, &["rev-parse", "refs/heads/main"]),
            snapshot.refs["refs/heads/main"]
        );
        assert_eq!(
            git(&destination, &["rev-parse", "refs/heads/release"]),
            snapshot.refs["refs/heads/release"]
        );
        assert_ne!(
            run_git(
                &FileGit,
                &destination,
                ["symbolic-ref", "-q", "refs/heads/release"],
                &[]
            )
            .unwrap()
            .status,
            0
        );
        assert_ne!(
            run_git(
                &FileGit,
                &destination,
                ["show-ref", "--verify", "refs/heads/destination-only"],
                &[]
            )
            .unwrap()
            .status,
            0
        );
        assert!(git(&destination, &["stash", "list"]).is_empty());
    }

    // Hard-won: 1548f4aa4: Unicode whitespace was stripped from symbolic HEAD ref names.
    #[test]
    fn review_eighth_symbolic_head_preserves_unicode_whitespace() {
        let root = Repo::new();
        git(&root.checkout, &["branch", "topic"]);
        let name = "topic\u{2003}";
        git(&root.checkout, &["checkout", "-b", name]);
        root.commit("distinct branch with Unicode whitespace\n");
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            snapshot.head,
            SnapshotHead::Branch {
                name: format!("refs/heads/{name}"),
                commit: git(&root.checkout, &["rev-parse", "HEAD"])
            }
        );
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["symbolic-ref", "HEAD"]),
            format!("refs/heads/{name}")
        );
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            snapshot.head.commit()
        );
        assert_ne!(
            git(&restored, &["rev-parse", "topic"]),
            snapshot.head.commit()
        );
    }

    // Hard-won: 1548f4aa4: each sibling rewrote every gitlink in the parent index.
    #[test]
    fn review_eighth_restored_siblings_do_not_rewrite_the_parent_index() {
        let root = Repo::new();
        let module = Repo::new();
        for n in 0..12 {
            git(
                &root.checkout,
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    module.origin.to_str().unwrap(),
                    &format!("module-{n:02}"),
                ],
            );
        }
        git(&root.checkout, &["commit", "-am", "many siblings"]);
        git(&root.checkout, &["push", "origin", "main"]);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let destination = root.temp.path().join("provisioned");
        git(
            root.temp.path(),
            &[
                "clone",
                root.origin.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        struct Count {
            root: PathBuf,
            counts: std::sync::Mutex<(usize, usize)>,
        }
        impl GitCommandRunner for Count {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                if repository == self.root {
                    let args = git_arguments(command);
                    let mut counts = self.counts.lock().unwrap();
                    if args.iter().any(|arg| arg == "update-index") {
                        counts.0 += 1;
                    }
                    if args.iter().any(|arg| arg == "ls-files")
                        && args.iter().any(|arg| arg == "--stage")
                        && !args.iter().any(|arg| arg == "--")
                    {
                        counts.1 += 1;
                    }
                }
                FileGit.run(repository, command)
            }
        }
        let runner = Count {
            root: destination.clone(),
            counts: Default::default(),
        };
        restore_workspace_snapshot(
            &runner,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: true,
            },
        )
        .unwrap();
        assert_eq!(
            *runner.counts.lock().unwrap(),
            (0, 1),
            "matching sibling gitlinks need one read and zero parent updates"
        );
        for n in 0..12 {
            assert_eq!(
                git(
                    &destination.join(format!("module-{n:02}")),
                    &["rev-parse", "HEAD"]
                ),
                git(&module.checkout, &["rev-parse", "HEAD"])
            );
        }
        // A changed wide index is one NUL-delimited update, beyond pipe capacity.
        let oid = snapshot.head.commit();
        let links: Vec<_> = (0..180)
            .map(|n| SnapshotGitlink {
                path: PathBuf::from(format!("wide/{n:03}-{}", "a".repeat(225)))
                    .join("b".repeat(230)),
                commit: oid.into(),
            })
            .collect();
        struct Large(std::sync::Mutex<Vec<usize>>);
        impl GitCommandRunner for Large {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                if git_arguments(command)
                    .iter()
                    .any(|arg| arg == "update-index")
                {
                    self.0.lock().unwrap().push(command.stdin.len());
                }
                FileGit.run(repository, command)
            }
        }
        let runner = Large(Default::default());
        restore_submodules(&runner, &destination, &[], Some(&links), false).unwrap();
        let updates = runner.0.lock().unwrap();
        assert_eq!(updates.len(), 1);
        assert!(updates[0] > 65_536);
        assert_eq!(
            crate::checkpoint::snapshot_index_gitlinks(&FileGit, &destination).unwrap(),
            links
        );
    }
    // Hard-won: 1548f4aa4: an unsuccessful rollback must retain and name the destination backup.
    #[test]
    fn review_eighth_failed_module_rollback_keeps_the_named_backup() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let destination = root.temp.path().join("provisioned");
        git(
            root.temp.path(),
            &[
                "clone",
                root.origin.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        fs::write(destination.join("sub/notes"), b"never discard").unwrap();
        struct Interrupt {
            destination: PathBuf,
            backup: std::sync::Mutex<Option<PathBuf>>,
        }
        impl GitCommandRunner for Interrupt {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                let output = FileGit.run(repository, command)?;
                if repository == self.destination.join("sub")
                    && git_arguments(command).iter().any(|arg| arg == "checkout")
                {
                    // Simulate a filesystem failure of the rollback source:
                    // keep its bytes elsewhere in the same backup directory.
                    let backup = fs::read_dir(&self.destination)?
                        .map(|entry| entry.map(|entry| entry.path()))
                        .collect::<std::io::Result<Vec<_>>>()?
                        .into_iter()
                        .find(|path| path.join("working/notes").is_file())
                        .context("locate restore backup")?;
                    fs::rename(backup.join("working"), backup.join("preserved"))?;
                    *self.backup.lock().unwrap() = Some(backup);
                    anyhow::bail!("injected module checkout interruption");
                }
                Ok(output)
            }
        }
        let runner = Interrupt {
            destination: destination.clone(),
            backup: Default::default(),
        };
        let error = restore_workspace_snapshot(
            &runner,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: true,
            },
        )
        .unwrap_err();
        let backup = runner.backup.lock().unwrap().clone().unwrap();
        assert!(
            error.to_string().contains("backup preserved at"),
            "{error:#}"
        );
        assert!(
            error.to_string().contains(backup.to_str().unwrap()),
            "{error:#}"
        );
        assert_eq!(
            fs::read(backup.join("preserved/notes")).unwrap(),
            b"never discard"
        );
        assert!(format!("{error:#}").contains("injected module checkout interruption"));
    }
    // Hard-won: cc6b6a560: anchoring an inactive rewrite prefix could activate it on an absolute origin.
    #[test]
    fn review_ninth_absolute_origin_keeps_inactive_literal_rewrite_prefixes() {
        for kind in ["insteadOf", "pushInsteadOf"] {
            let root = Repo::new();
            let publisher = Repo::new();
            let mirror = root.checkout.join("repo.git");
            git(
                root.temp.path(),
                &[
                    "clone",
                    "--bare",
                    root.origin.to_str().unwrap(),
                    mirror.to_str().unwrap(),
                ],
            );
            fs::write(root.checkout.join(".git/info/exclude"), "repo.git\n").unwrap();
            git(
                &root.checkout,
                &["remote", "set-url", "origin", mirror.to_str().unwrap()],
            );
            git(
                &root.checkout,
                &[
                    "config",
                    &format!("url.{}.{kind}", publisher.origin.display()),
                    "repo.git",
                ],
            );
            root.commit("unpublished work for the actual mirror\n");
            let snapshot = root.capture(&CheckoutOwnership::Raw { launch: None });
            assert_eq!(snapshot.url_rules[0].value, "repo.git");
            let restored = root.restore(&snapshot, None);
            assert_eq!(
                git(&restored, &["remote", "get-url", "origin"]),
                mirror.to_str().unwrap()
            );
            assert_eq!(
                git(&restored, &["remote", "get-url", "--push", "origin"]),
                mirror.to_str().unwrap()
            );
            git(&restored, &["push", "origin", "main"]);
            assert_eq!(git(&mirror, &["rev-parse", "main"]), snapshot.head.commit());
            assert_ne!(
                git(&publisher.origin, &["rev-parse", "main"]),
                snapshot.head.commit()
            );
        }
    }

    // Hard-won: cc6b6a560: relative URL anchoring could change fetch/push rewrite winners after abort consent.
    #[test]
    fn review_ninth_ambiguous_relative_rewrites_refuse_before_aborting() {
        for kind in ["insteadOf", "pushInsteadOf"] {
            let root = Repo::new();
            let target = Repo::new();
            git(
                &root.checkout,
                &["remote", "set-url", "origin", "../origin.git"],
            );
            git(
                &root.checkout,
                &[
                    "config",
                    &format!("url.{}.{kind}", target.origin.display()),
                    "../origin.git",
                ],
            );
            root.conflict();
            root.start_conflict("merge");
            let error = capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("refused.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap_err();
            let text = format!("{error:#}");
            assert!(
                text.contains("anchoring origin")
                    && text.contains("../origin.git")
                    && text.contains("from")
                    && text.contains("to")
                    && text.contains("git remote set-url"),
                "{error:#}"
            );
            assert!(root.checkout.join(".git/MERGE_HEAD").exists());
            assert!(!root.temp.path().join("refused.pack").exists());
            if kind == "pushInsteadOf" {
                assert!(text.contains("--push origin"));
                for runner in [
                    &FileGit as &dyn GitCommandRunner,
                    &crate::test_support::Git231,
                ] {
                    let listing = run_git(
                        runner,
                        &root.checkout,
                        [
                            "-c",
                            "remote.source-synthetic.url=../origin.git",
                            "remote",
                            "-v",
                        ],
                        &[],
                    )
                    .unwrap();
                    assert_eq!(listing.status, 0);
                    let listing = std::str::from_utf8(&listing.stdout).unwrap();
                    assert!(
                        listing.lines().any(|line| line
                            == format!("source-synthetic\t{} (push)", target.origin.display())),
                        "{listing}"
                    );
                    let error = validate_origin_rewrite_anchoring(runner, &root.checkout, None)
                        .unwrap_err();
                    assert!(
                        error.to_string().contains(target.origin.to_str().unwrap()),
                        "{error:#}"
                    );
                }
                git(
                    &root.checkout,
                    &[
                        "config",
                        "--unset-all",
                        &format!("url.{}.pushInsteadOf", target.origin.display()),
                    ],
                );
                for delimiter in ["\t", "\n"] {
                    let key = format!("url.{}\tambiguous.pushInsteadOf", target.origin.display());
                    if delimiter == "\t" {
                        git(&root.checkout, &["config", &key, "../origin.git"]);
                    } else {
                        // Git disallows a newline in a config key, but permits
                        // one in a remote URL value and emits it literally.
                        git(
                            &root.checkout,
                            &["remote", "set-url", "origin", "../origin.git\nambiguous"],
                        );
                    }
                    let error = validate_origin_rewrite_anchoring(&FileGit, &root.checkout, None)
                        .unwrap_err();
                    assert!(
                        error
                            .to_string()
                            .contains("ambiguous tab/newline delimiters"),
                        "{error:#}"
                    );
                    assert!(root.checkout.join(".git/MERGE_HEAD").exists());
                    if delimiter == "\t" {
                        git(&root.checkout, &["config", "--unset-all", &key]);
                    }
                }
            }
        }
    }

    // Hard-won: cc6b6a560: an untracked .gitmodules was treated as durable registration for initialized pins.
    #[test]
    fn review_ninth_untracked_module_registration_must_be_staged_before_discard() {
        let root = Repo::new();
        let module = Repo::new();
        git(
            &root.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                module.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&root.checkout, &["rm", "--cached", ".gitmodules"]);
        let index = fs::read(root.checkout.join(".git/index")).unwrap();
        let config = fs::read(root.checkout.join(".gitmodules")).unwrap();
        let consent = CaptureConsent {
            untracked: UntrackedConsent::Any,
            operations: OperationConsent::Any,
        };
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("refused.pack"),
            &consent,
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::UntrackedSubmoduleRegistration(_))
            ),
            "{error:#}"
        );
        assert!(error.to_string().contains("add '.gitmodules'"));
        assert_eq!(fs::read(root.checkout.join(".git/index")).unwrap(), index);
        assert_eq!(fs::read(root.checkout.join(".gitmodules")).unwrap(), config);
        git(&root.checkout, &["add", ".gitmodules"]);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&snapshot, None);
        assert_eq!(fs::read(restored.join(".gitmodules")).unwrap(), config);
        assert_eq!(
            git(&restored.join("sub"), &["rev-parse", "HEAD"]),
            git(&module.checkout, &["rev-parse", "HEAD"])
        );
        // Raw operation conversion omits the introduced module rather than
        // attempting to transport its untracked registration; source stays.
        let raw = Repo::new();
        raw.conflict();
        raw.start_conflict("merge");
        git(
            &raw.checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                module.origin.to_str().unwrap(),
                "sub",
            ],
        );
        git(&raw.checkout, &["rm", "--cached", ".gitmodules"]);
        let snapshot = raw.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(snapshot.working_state_skipped);
        let restored = raw.restore(&snapshot, None);
        assert!(!restored.join("sub/.git").exists());
        assert!(!restored.join(".gitmodules").exists());
        assert!(raw.checkout.join(".git/MERGE_HEAD").exists());
        assert!(raw.checkout.join("sub/.git").exists());
        assert!(raw.checkout.join(".gitmodules").exists());
    }

    // Hard-won: cc6b6a560: Git uses the last configured origin URL when resolving relative submodules.
    #[test]
    fn review_ninth_relative_module_probe_uses_gits_last_origin_url() {
        let root = Repo::new();
        let module = Repo::new();
        let other = Repo::new();
        add_submodule(&root, &module);
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(root.checkout.join(".git/modules")).unwrap();
        let first = root.temp.path().join("first");
        let last = root.temp.path().join("last");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&last).unwrap();
        for directory in [&first, &last] {
            git(
                root.temp.path(),
                &[
                    "clone",
                    "--bare",
                    root.origin.to_str().unwrap(),
                    directory.join("app.git").to_str().unwrap(),
                ],
            );
        }
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                last.join("lib.git").to_str().unwrap(),
            ],
        );
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                other.origin.to_str().unwrap(),
                first.join("lib.git").to_str().unwrap(),
            ],
        );
        module.commit("pin advertised by the last URL sibling\n");
        git(
            &module.checkout,
            &["push", last.join("lib.git").to_str().unwrap(), "main"],
        );
        let commit = git(&module.checkout, &["rev-parse", "HEAD"]);
        git(
            &root.checkout,
            &[
                "config",
                "remote.origin.url",
                first.join("app.git").to_str().unwrap(),
            ],
        );
        git(
            &root.checkout,
            &[
                "config",
                "--add",
                "remote.origin.url",
                last.join("app.git").to_str().unwrap(),
            ],
        );
        git(
            &root.checkout,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.sub.url",
                "../lib.git",
            ],
        );
        git(&root.checkout, &["add", ".gitmodules"]);
        git(
            &root.checkout,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{commit},sub"),
            ],
        );
        assert!(
            workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed)
                .unwrap()
                .dirty_submodules
                .is_empty()
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&snapshot, None);
        git(
            &restored,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "sub",
            ],
        );
        assert_eq!(git(&restored.join("sub"), &["rev-parse", "HEAD"]), commit);
        assert_eq!(
            git(&restored.join("sub"), &["remote", "get-url", "origin"]),
            last.join("lib.git").to_str().unwrap()
        );
    }

    // Hard-won: cc6b6a560: canceling suffix x/.. lexically crossed the wrong side of a symlink.
    #[cfg(unix)]
    #[test]
    fn review_ninth_relative_module_suffix_keeps_filesystem_symlink_semantics() {
        let root = Repo::new();
        let module = Repo::new();
        let wrong = Repo::new();
        add_submodule(&root, &module);
        git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(root.checkout.join(".git/modules")).unwrap();
        let namespace = root.temp.path().join("namespace");
        let actual = root.temp.path().join("actual");
        fs::create_dir_all(actual.join("target")).unwrap();
        fs::create_dir_all(&namespace).unwrap();
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                root.origin.to_str().unwrap(),
                namespace.join("app.git").to_str().unwrap(),
            ],
        );
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                actual.join("lib.git").to_str().unwrap(),
            ],
        );
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                wrong.origin.to_str().unwrap(),
                namespace.join("lib.git").to_str().unwrap(),
            ],
        );
        std::os::unix::fs::symlink(actual.join("target"), namespace.join("mirror-link")).unwrap();
        module.commit("published only beyond the symlink\n");
        let commit = git(&module.checkout, &["rev-parse", "HEAD"]);
        git(
            &module.checkout,
            &["push", actual.join("lib.git").to_str().unwrap(), "main"],
        );
        git(
            &root.checkout,
            &[
                "remote",
                "set-url",
                "origin",
                namespace.join("app.git").to_str().unwrap(),
            ],
        );
        git(
            &root.checkout,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.sub.url",
                "../mirror-link/../lib.git",
            ],
        );
        git(&root.checkout, &["add", ".gitmodules"]);
        git(
            &root.checkout,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{commit},sub"),
            ],
        );
        assert!(
            workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed)
                .unwrap()
                .dirty_submodules
                .is_empty()
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&snapshot, None);
        git(
            &restored,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "sub",
            ],
        );
        assert_eq!(git(&restored.join("sub"), &["rev-parse", "HEAD"]), commit);
    }

    // Hard-won: cc6b6a560: a consented bisect reset revealed a module remote inside another managed checkout.
    #[test]
    fn review_ninth_post_abort_routes_use_the_complete_session_teardown_set() {
        let first = Repo::new();
        let second = Repo::new();
        let module = Repo::new();
        add_submodule(&first, &module);
        git(&first.checkout, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(first.checkout.join(".git/modules")).unwrap();
        let mirror = second.checkout.join("mirror.git");
        git(
            second.temp.path(),
            &[
                "clone",
                "--bare",
                module.origin.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        fs::write(second.checkout.join(".git/info/exclude"), "mirror.git\n").unwrap();
        git(
            &first.checkout,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.sub.url",
                mirror.to_str().unwrap(),
            ],
        );
        git(&first.checkout, &["commit", "-am", "unsafe module route"]);
        git(&first.checkout, &["branch", "unsafe"]);
        git(
            &first.checkout,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.sub.url",
                module.origin.to_str().unwrap(),
            ],
        );
        git(&first.checkout, &["commit", "-am", "safe module route"]);
        first.commit("safe middle\n");
        first.commit("bad tip\n");
        git(&first.checkout, &["checkout", "unsafe"]);
        git(&first.checkout, &["bisect", "start", "main", "unsafe"]);
        assert_eq!(
            git(
                &first.checkout,
                &["config", "-f", ".gitmodules", "submodule.sub.url"]
            ),
            module.origin.to_str().unwrap()
        );
        let inputs = [("first", &first), ("second", &second)].map(|(id, root)| RepositoryInput {
            id: id.into(),
            checkout: root.checkout.clone(),
            ownership: CheckoutOwnership::Managed,
            origin_override: None,
            pack_path: root.temp.path().join("pending.pack"),
            exclusions: ExclusionSource::Origin,
            keep_dir: None,
            in_container: false,
        });
        let consent = SessionCaptureConsent {
            repositories: inputs
                .iter()
                .map(|input| {
                    (
                        input.id.clone(),
                        CaptureConsent {
                            untracked: UntrackedConsent::Any,
                            operations: OperationConsent::Any,
                        },
                    )
                })
                .collect(),
        };
        let error = capture_session_snapshot(&FileGit, &inputs, &consent).unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::OriginRemovedByTeardown(_))
            ),
            "{error:#}"
        );
        assert!(
            error.to_string().contains(mirror.to_str().unwrap()),
            "{error:#}"
        );
        assert!(
            !git_path(&FileGit, &first.checkout, "BISECT_START")
                .unwrap()
                .exists()
        );
        for root in [&first, &second] {
            assert!(!fs::read_dir(root.temp.path()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "pack")
            }));
        }
    }
    // Hard-won: 58c5ca099: an external shared origin lost its only objects during source teardown.
    #[test]
    fn review_tenth_shared_origin_and_recursive_alternates_must_be_independent() {
        let mut root = Repo::new();
        let moved = root.temp.path().join(if cfg!(unix) {
            "checkout\towned"
        } else {
            "checkout-owned"
        });
        fs::rename(&root.checkout, &moved).unwrap();
        root.checkout = moved;
        root.commit("only source owns this commit\n");
        let first = root.temp.path().join("shared-first.git");
        let last = root.temp.path().join("shared-recursive.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--shared",
                "--bare",
                root.checkout.to_str().unwrap(),
                first.to_str().unwrap(),
            ],
        );
        git(
            root.temp.path(),
            &[
                "clone",
                "--shared",
                "--bare",
                first.to_str().unwrap(),
                last.to_str().unwrap(),
            ],
        );
        git(
            &root.checkout,
            &["remote", "set-url", "origin", last.to_str().unwrap()],
        );
        let inventory =
            workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
        assert!(inventory.origins_removed_by_teardown.iter().any(|origin| {
            origin
                .value
                .object_provider
                .as_ref()
                .is_some_and(|provider| provider.starts_with(&root.checkout))
        }));
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("refused.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(SnapshotRefused::OriginRemovedByTeardown(_))
            ),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("repack -a -d")
                && error.to_string().contains("objects/info/alternates")
                && error.to_string().contains(last.to_str().unwrap()),
            "{error:#}"
        );
        assert!(
            workspace_inventory(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Raw { launch: None }
            )
            .unwrap()
            .origins_removed_by_teardown
            .is_empty()
        );
        let alternates = first.join("objects/info/alternates");
        let original = fs::read(&alternates).unwrap();
        let mut malformed = original.clone();
        malformed.extend_from_slice(b"\"unterminated\n");
        fs::write(&alternates, malformed).unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("invalid-provider.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("cannot inspect local origin object storage")
                && message.contains(last.to_str().unwrap())
                && message.contains("repair its objects/info/alternates"),
            "{message}"
        );
        fs::write(alternates, original).unwrap();
        git(&last, &["repack", "-a", "-d"]);
        fs::remove_file(last.join("objects/info/alternates")).unwrap();
        #[cfg(unix)]
        {
            // Native provider names are canonical. The literal route can
            // still cross a deleted symlink even when its target survives.
            let route = root.checkout.join("borrowed-objects");
            std::os::unix::fs::symlink(root.origin.join("objects"), &route).unwrap();
            fs::write(
                last.join("objects/info/alternates"),
                format!("{}\n", route.display()),
            )
            .unwrap();
            let inventory =
                workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
            assert!(
                inventory
                    .origins_removed_by_teardown
                    .iter()
                    .any(|origin| origin.value.object_provider.as_ref() == Some(&route))
            );
            fs::remove_file(last.join("objects/info/alternates")).unwrap();
            fs::remove_file(route).unwrap();
        }
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_none());
        let saved = snapshot.head.commit().to_owned();
        fs::remove_dir_all(&root.checkout).unwrap();
        let restored = root.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", "HEAD"]), saved);
        assert_eq!(
            fs::read(restored.join("file")).unwrap(),
            b"only source owns this commit\n"
        );
    }

    // Hard-won: 58c5ca099: module origins outside the clone could borrow their objects from its admin store.
    #[test]
    fn review_tenth_module_origins_cannot_borrow_from_initialized_or_cached_admins() {
        for initialized in [true, false] {
            let root = Repo::new();
            let module = Repo::new();
            let directory = add_submodule(&root, &module);
            let mirror = root.temp.path().join("module-shared.git");
            git(
                root.temp.path(),
                &[
                    "clone",
                    "--shared",
                    "--bare",
                    directory.to_str().unwrap(),
                    mirror.to_str().unwrap(),
                ],
            );
            git(
                &directory,
                &["remote", "set-url", "origin", mirror.to_str().unwrap()],
            );
            if !initialized {
                git(
                    &root.checkout,
                    &[
                        "config",
                        "-f",
                        ".gitmodules",
                        "submodule.sub.url",
                        mirror.to_str().unwrap(),
                    ],
                );
                git(&root.checkout, &["add", ".gitmodules"]);
                git(&root.checkout, &["submodule", "deinit", "-f", "sub"]);
            }
            let inventory =
                workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
            assert!(inventory.origins_removed_by_teardown.iter().any(|origin| {
                origin
                    .value
                    .object_provider
                    .as_ref()
                    .is_some_and(|provider| {
                        provider.starts_with(root.checkout.join(".git/modules"))
                    })
            }));
            let error = capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("refused.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<SnapshotRefused>(),
                    Some(SnapshotRefused::OriginRemovedByTeardown(_))
                ),
                "{error:#}"
            );
            git(&mirror, &["repack", "-a", "-d"]);
            fs::remove_file(mirror.join("objects/info/alternates")).unwrap();
            if !initialized {
                fs::remove_dir_all(root.checkout.join(".git/modules")).unwrap();
            }
            let snapshot = root.capture(&CheckoutOwnership::Managed);
            fs::remove_dir_all(&root.checkout).unwrap();
            fs::rename(&module.origin, module.temp.path().join("offline.git")).unwrap();
            let restored = root.restore(&snapshot, None);
            if !initialized {
                git(
                    &restored,
                    &[
                        "-c",
                        "protocol.file.allow=always",
                        "submodule",
                        "update",
                        "--init",
                        "sub",
                    ],
                );
            }
            assert_eq!(
                git(&restored.join("sub"), &["rev-parse", "HEAD"]),
                git(&module.checkout, &["rev-parse", "HEAD"])
            );
            assert_eq!(
                git(&restored.join("sub"), &["remote", "get-url", "origin"]),
                mirror.to_str().unwrap()
            );
        }
    }

    // Hard-won: 58c5ca099: an origin-less override bypassed literal rewrite anchoring preflight.
    #[test]
    fn review_tenth_source_override_anchoring_is_checked_before_aborting() {
        let root = Repo::new();
        let mirror = root.temp.path().join("mirror.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                root.checkout.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        git(&root.checkout, &["remote", "remove", "origin"]);
        git(
            &root.checkout,
            &[
                "config",
                &format!("url.{}.insteadOf", mirror.display()),
                "../source.git",
            ],
        );
        root.conflict();
        root.start_conflict("merge");
        let consent = CaptureConsent {
            untracked: UntrackedConsent::Any,
            operations: OperationConsent::Any,
        };
        for ownership in [
            CheckoutOwnership::Managed,
            CheckoutOwnership::Raw { launch: None },
        ] {
            let error = capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                &ownership,
                &root.temp.path().join("refused.pack"),
                &consent,
                Some("../source.git".into()),
            )
            .unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains("../source.git")
                    && message.contains(mirror.to_str().unwrap())
                    && message.contains("git remote add origin"),
                "{error:#}"
            );
            assert!(root.checkout.join(".git/MERGE_HEAD").exists());
        }
        let in_place = root.capture(&CheckoutOwnership::Raw { launch: None });
        assert!(in_place.origin_urls.is_empty());
        assert!(root.checkout.join(".git/MERGE_HEAD").exists());
        let snapshot = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("accepted.pack"),
            &consent,
            Some(mirror.to_str().unwrap().into()),
        )
        .unwrap();
        assert!(!root.checkout.join(".git/MERGE_HEAD").exists());
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            snapshot.head.commit()
        );
    }

    // Hard-won: 58c5ca099: name-only ITA subtraction missed tracked nonempty and empty HEAD paths.
    #[test]
    fn review_tenth_existing_head_paths_with_intent_to_add_refuse_before_abort() {
        for empty in [false, true] {
            let root = Repo::new();
            if empty {
                fs::write(root.checkout.join("file"), b"").unwrap();
                git(&root.checkout, &["commit", "-am", "empty HEAD path"]);
            }
            git(&root.checkout, &["checkout", "-b", "side"]);
            fs::write(root.checkout.join("other"), b"side\n").unwrap();
            git(&root.checkout, &["add", "other"]);
            git(&root.checkout, &["commit", "-m", "side path"]);
            git(&root.checkout, &["checkout", "main"]);
            fs::write(root.checkout.join("other"), b"main\n").unwrap();
            git(&root.checkout, &["add", "other"]);
            git(&root.checkout, &["commit", "-m", "main path"]);
            root.start_conflict("merge");
            git(&root.checkout, &["rm", "--cached", "file"]);
            git(&root.checkout, &["add", "-N", "file"]);
            let index = fs::read(root.checkout.join(".git/index")).unwrap();
            let inventory =
                workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
            assert!(
                inventory
                    .intent_to_add
                    .iter()
                    .any(|entry| entry.value == Path::new("file"))
            );
            let error = capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("refused.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Any,
                    operations: OperationConsent::Any,
                },
                None,
            )
            .unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<SnapshotRefused>(),
                    Some(SnapshotRefused::IntentToAdd(_))
                ),
                "{error:#}"
            );
            assert_eq!(fs::read(root.checkout.join(".git/index")).unwrap(), index);
            assert!(root.checkout.join(".git/MERGE_HEAD").exists());
        }
    }

    // Hard-won: 58c5ca099: primary-scope reads dropped upstreams defined by the branch's linked worktree.
    #[test]
    fn review_tenth_nested_branch_upstreams_come_from_the_owning_worktree() {
        let root = Repo::new();
        git(
            &root.checkout,
            &["config", "extensions.worktreeConfig", "true"],
        );
        let nested = root.checkout.join("lanes/upstream");
        git(
            &root.checkout,
            &["worktree", "add", "-b", "lane", nested.to_str().unwrap()],
        );
        git(
            &nested,
            &["config", "--worktree", "branch.lane.remote", "origin"],
        );
        git(
            &nested,
            &[
                "config",
                "--worktree",
                "branch.lane.merge",
                "refs/heads/main",
            ],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            snapshot.branch_tracking["lane"],
            BranchTracking {
                remote: Some("origin".into()),
                merge: vec!["refs/heads/main".into()]
            }
        );
        let restored = root.restore(&snapshot, None);
        let restored_lane = restored.join("lanes/upstream");
        assert_eq!(
            git(
                &restored_lane,
                &["rev-parse", "--abbrev-ref", "@{upstream}"]
            ),
            "origin/main"
        );
        git(&restored_lane, &["pull", "--ff-only"]);
        assert_eq!(
            git(&restored_lane, &["symbolic-ref", "HEAD"]),
            "refs/heads/lane"
        );
    }
    // Hard-won: ecc543144: an origin pull mapping could contaminate its own ordinary tracking namespace.
    #[test]
    fn review_eleventh_offline_origin_mixed_sources_keep_pull_only_history_in_pack() {
        let root = Repo::new();
        root.commit("pull-only unpublished branch history\n");
        let tip = git(&root.checkout, &["rev-parse", "HEAD"]);
        git(&root.checkout, &["push", "origin", "HEAD:refs/pull/1/head"]);
        git(
            &root.checkout,
            &[
                "config",
                "--add",
                "remote.origin.fetch",
                "+refs/pull/1/head:refs/remotes/origin/pull-cache",
            ],
        );
        git(&root.checkout, &["fetch", "origin"]);
        let evidence = origin_tracking_objects(&FileGit, &root.checkout).unwrap();
        assert!(evidence.ambiguous);
        assert!(!evidence.objects.contains(&tip));
        let away = root.temp.path().join("offline.git");
        fs::rename(&root.origin, &away).unwrap();
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_some());
        assert!(snapshot.remote_refs_stale);
        fs::rename(&away, &root.origin).unwrap();
        git(&root.origin, &["update-ref", "-d", "refs/pull/1/head"]);
        git(&root.origin, &["reflog", "expire", "--expire=now", "--all"]);
        git(&root.origin, &["gc", "--prune=now"]);
        assert_ne!(
            run_git(&FileGit, &root.origin, ["cat-file", "-e", &tip], &[])
                .unwrap()
                .status,
            0
        );
        fs::remove_dir_all(&root.checkout).unwrap();
        let restored = root.restore(&snapshot, None);
        assert_eq!(git(&restored, &["rev-parse", "HEAD"]), tip);
        assert_eq!(
            fs::read(restored.join("file")).unwrap(),
            b"pull-only unpublished branch history\n"
        );
    }

    // Hard-won: ecc543144: shallow roots masked missing ancestry during provisioned connectivity checks.
    #[test]
    fn review_eleventh_shallow_restore_boundaries_refuse_before_provisioned_writes() {
        let root = Repo::new();
        root.commit("full-history tip\n");
        git(&root.checkout, &["push", "origin", "main"]);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert!(snapshot.pack.is_none());
        let destination = root.temp.path().join("depth-one");
        let uri = url::Url::from_directory_path(&root.origin)
            .unwrap()
            .to_string();
        git(
            root.temp.path(),
            &["clone", "--depth", "1", &uri, destination.to_str().unwrap()],
        );
        let config = fs::read(destination.join(".git/config")).unwrap();
        let index = fs::read(destination.join(".git/index")).unwrap();
        let boundary = fs::read(destination.join(".git/shallow")).unwrap();
        let refs = git(&destination, &["show-ref"]);
        for allow_origin_fetch in [false, true] {
            let error = restore_workspace_snapshot(
                &FileGit,
                &snapshot,
                &destination,
                &RestoreBase::Provisioned { allow_origin_fetch },
            )
            .unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains("is shallow at")
                    && message.contains(snapshot.head.commit())
                    && message.contains("fetch --unshallow origin")
                    && message.contains("without fetching past"),
                "{message}"
            );
            assert_eq!(fs::read(destination.join(".git/config")).unwrap(), config);
            assert_eq!(fs::read(destination.join(".git/index")).unwrap(), index);
            assert_eq!(
                fs::read(destination.join(".git/shallow")).unwrap(),
                boundary
            );
            assert_eq!(git(&destination, &["show-ref"]), refs);
        }
        git(&destination, &["fetch", "--unshallow", "origin"]);
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert_eq!(
            git(&destination, &["rev-parse", "HEAD"]),
            snapshot.head.commit()
        );
        git(
            &destination,
            &["cat-file", "-e", &format!("{}^", snapshot.head.commit())],
        );

        let shallow_origin = root.temp.path().join("shallow-server.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                "--depth",
                "1",
                &uri,
                shallow_origin.to_str().unwrap(),
            ],
        );
        let bootstrap = root.temp.path().join("bootstrap");
        let error = restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &bootstrap,
            &RestoreBase::Clone {
                origin: shallow_origin.to_str().unwrap().into(),
                seed: None,
            },
        )
        .unwrap_err();
        let message = format!("{error:#}");
        if bootstrap.join(".git/shallow").exists() {
            assert!(
                message.contains("is shallow at") && message.contains(snapshot.head.commit()),
                "{message}"
            );
        } else {
            // Some Git versions reject installing the server's shallow ref;
            // they must still fail graph recovery rather than declare success.
            assert!(
                message.contains("missing") || message.contains("shallow"),
                "{message}"
            );
        }
        let parent = Repo::new();
        let module = Repo::new();
        module.commit("full module ancestry\n");
        git(&module.checkout, &["push", "origin", "main"]);
        add_submodule(&parent, &module);
        let module_uri = url::Url::from_directory_path(&module.origin)
            .unwrap()
            .to_string();
        git(
            &parent.checkout,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.sub.url",
                &module_uri,
            ],
        );
        git(
            &parent.checkout,
            &["commit", "-am", "module URL for shallow provisioning"],
        );
        git(&parent.checkout, &["push", "origin", "main"]);
        let snapshot = parent.capture(&CheckoutOwnership::Managed);
        let destination = parent.temp.path().join("cached-module");
        git(
            parent.temp.path(),
            &[
                "clone",
                parent.origin.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        git(
            &destination,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "--depth",
                "1",
            ],
        );
        let admin = crate::archive::git::git_directories(&FileGit, &destination.join("sub"))
            .unwrap()
            .0;
        assert_eq!(
            git(
                &destination.join("sub"),
                &["rev-parse", "--is-shallow-repository"]
            ),
            "true"
        );
        git(&destination, &["submodule", "deinit", "-f", "sub"]);
        fs::write(
            destination.join("sub/notes"),
            b"destination notes survive rollback",
        )
        .unwrap();
        let error = restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("is shallow at")
                && message.contains(admin.to_str().unwrap())
                && message.contains("git --git-dir"),
            "{message}"
        );
        assert_eq!(
            fs::read(destination.join("sub/notes")).unwrap(),
            b"destination notes survive rollback"
        );
        git(
            &destination,
            &[
                "--git-dir",
                admin.to_str().unwrap(),
                "fetch",
                "--unshallow",
                "origin",
            ],
        );
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        assert_eq!(
            git(
                &destination.join("sub"),
                &["rev-parse", "--is-shallow-repository"]
            ),
            "false"
        );
        assert_eq!(
            fs::read(destination.join("sub/notes")).unwrap(),
            b"destination notes survive rollback"
        );
    }

    // Hard-won: ecc543144: config.worktree upstream values overrode the shared snapshot settings on retry.
    #[test]
    fn review_eleventh_provisioned_upstreams_override_direct_worktree_values() {
        let root = Repo::new();
        git(
            &root.checkout,
            &["config", "extensions.worktreeConfig", "true"],
        );
        git(&root.checkout, &["config", "branch.main.remote", "origin"]);
        git(
            &root.checkout,
            &["config", "branch.main.merge", "refs/heads/main"],
        );
        let nested = root.checkout.join("lanes/upstream");
        git(
            &root.checkout,
            &["worktree", "add", "-b", "lane", nested.to_str().unwrap()],
        );
        git(
            &nested,
            &["config", "--worktree", "branch.lane.remote", "origin"],
        );
        git(
            &nested,
            &[
                "config",
                "--worktree",
                "branch.lane.merge",
                "refs/heads/main",
            ],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let destination = root.temp.path().join("provisioned");
        git(
            root.temp.path(),
            &[
                "clone",
                root.origin.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        git(
            &destination,
            &["config", "extensions.worktreeConfig", "true"],
        );
        git(
            &destination,
            &["remote", "add", "fork", root.origin.to_str().unwrap()],
        );
        let reused = destination.join("lanes/upstream");
        git(
            &destination,
            &["worktree", "add", "-b", "lane", reused.to_str().unwrap()],
        );
        for checkout in [&destination, &reused] {
            for name in ["main", "lane"] {
                git(
                    checkout,
                    &[
                        "config",
                        "--worktree",
                        &format!("branch.{name}.remote"),
                        "fork",
                    ],
                );
                git(
                    checkout,
                    &[
                        "config",
                        "--worktree",
                        &format!("branch.{name}.merge"),
                        "refs/heads/obsolete",
                    ],
                );
            }
            git(
                checkout,
                &["config", "--worktree", "mj.keep", "destination-owned"],
            );
        }
        restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &destination,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap();
        for (checkout, branch) in [(&destination, "main"), (&reused, "lane")] {
            assert_eq!(
                config_values(&FileGit, checkout, &format!("branch.{branch}.remote")).unwrap(),
                vec!["origin"]
            );
            assert_eq!(
                config_values(&FileGit, checkout, &format!("branch.{branch}.merge")).unwrap(),
                vec!["refs/heads/main"]
            );
            assert_eq!(
                git(checkout, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
                "origin/main"
            );
            git(checkout, &["pull", "--ff-only"]);
            assert_eq!(
                git(checkout, &["config", "--worktree", "mj.keep"]),
                "destination-owned"
            );
        }
    }

    // Hard-won: ecc543144: absent working .gitmodules hid still-registered absorbed module checkouts.
    #[test]
    fn review_eleventh_missing_gitmodules_uses_index_then_head_registration() {
        for staged in [false, true] {
            let root = Repo::new();
            let module = Repo::new();
            add_submodule(&root, &module);
            if staged {
                git(&root.checkout, &["rm", ".gitmodules"]);
            } else {
                fs::remove_file(root.checkout.join(".gitmodules")).unwrap();
            }
            let indexed = git(
                &root.checkout,
                &["ls-files", "--stage", "--", ".gitmodules"],
            );
            let inventory =
                workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
            assert_eq!(inventory.initialized_submodules.len(), 1);
            assert!(inventory.unrepresented_git_dirs.is_empty());
            assert!(inventory.nested_repositories.is_empty());
            let snapshot = root.capture(&CheckoutOwnership::Managed);
            let restored = root.restore(&snapshot, None);
            assert!(!restored.join(".gitmodules").exists());
            assert_eq!(
                git(&restored, &["ls-files", "--stage", "--", ".gitmodules"]),
                indexed
            );
            assert_eq!(
                git(&restored.join("sub"), &["rev-parse", "HEAD"]),
                git(&module.checkout, &["rev-parse", "HEAD"])
            );
        }
    }

    // Hard-won: ecc543144: missing working registration let discard consent bypass standalone-module work guards.
    #[test]
    fn review_eleventh_missing_gitmodules_keeps_standalone_module_publication_checks() {
        let root = Repo::new();
        let module = Repo::new();
        add_submodule(&root, &module);
        let standalone = root.temp.path().join("standalone");
        git(
            root.temp.path(),
            &[
                "clone",
                module.origin.to_str().unwrap(),
                standalone.to_str().unwrap(),
            ],
        );
        fs::remove_dir_all(root.checkout.join("sub")).unwrap();
        fs::remove_dir_all(root.checkout.join(".git/modules")).unwrap();
        fs::rename(standalone, root.checkout.join("sub")).unwrap();
        fs::remove_file(root.checkout.join(".gitmodules")).unwrap();
        let directory = root.checkout.join("sub");
        fs::write(directory.join("file"), b"unpublished standalone work\n").unwrap();
        git(&directory, &["commit", "-am", "standalone work"]);
        let consent = CaptureConsent {
            untracked: UntrackedConsent::Any,
            operations: OperationConsent::Any,
        };
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("refused.pack"),
            &consent,
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<SnapshotRefused>(),
                Some(
                    SnapshotRefused::UnpublishedSubmoduleWork(_)
                        | SnapshotRefused::DirtySubmodule(_)
                )
            ),
            "{error:#}"
        );
        git(&directory, &["push", "origin", "main"]);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(&restored.join("sub"), &["rev-parse", "HEAD"]),
            git(&directory, &["rev-parse", "HEAD"])
        );
        assert!(!restored.join(".gitmodules").exists());
    }
    // Hard-won: e0cc04156/9a8ec9177: direct worktree origin/push URLs survived provisioned replacement.
    #[test]
    fn review_twelfth_provisioned_push_urls_clear_all_direct_worktree_scopes() {
        for explicit in [false, true] {
            let root = Repo::new();
            let module = Repo::new();
            add_submodule(&root, &module);
            let nested = root.checkout.join("lane");
            git(
                &root.checkout,
                &["worktree", "add", "--detach", nested.to_str().unwrap()],
            );
            if explicit {
                git(
                    &root.checkout,
                    &["config", "remote.origin.pushurl", "saved:publish"],
                );
            }
            let snapshot = root.capture(&CheckoutOwnership::Managed);
            let destination = root.restore(&snapshot, None);
            let module_dir = destination.join("sub");
            for checkout in [&destination, &destination.join("lane"), &module_dir] {
                git(checkout, &["config", "extensions.worktreeConfig", "true"]);
                git(
                    checkout,
                    &[
                        "config",
                        "--worktree",
                        "remote.origin.url",
                        "obsolete:fetch-and-push",
                    ],
                );
                git(
                    checkout,
                    &[
                        "config",
                        "--worktree",
                        "remote.origin.pushurl",
                        "obsolete:publish",
                    ],
                );
            }
            restore_workspace_snapshot(
                &FileGit,
                &snapshot,
                &destination,
                &RestoreBase::Provisioned {
                    allow_origin_fetch: false,
                },
            )
            .unwrap();
            for checkout in [&destination, &destination.join("lane")] {
                assert_eq!(
                    config_values(&FileGit, checkout, "remote.origin.url").unwrap(),
                    snapshot.origin_urls
                );
                let expected = if explicit {
                    "saved:publish"
                } else {
                    root.origin.to_str().unwrap()
                };
                assert_eq!(
                    git(
                        checkout,
                        &["remote", "get-url", "--push", "--all", "origin"]
                    ),
                    expected
                );
            }
            assert_eq!(
                git(
                    &module_dir,
                    &["remote", "get-url", "--push", "--all", "origin"]
                ),
                module.origin.to_str().unwrap()
            );
        }
    }

    // Hard-won: e0cc04156: preserving inode and mtime let changed file sizes reuse discard consent.
    #[test]
    fn review_twelfth_shown_file_size_changes_refuse_even_with_preserved_mtime() {
        for nested in [false, true] {
            let root = Repo::new();
            let path = if nested {
                let embedded = root.checkout.join("embedded");
                git(
                    root.temp.path(),
                    &[
                        "clone",
                        root.origin.to_str().unwrap(),
                        embedded.to_str().unwrap(),
                    ],
                );
                embedded.join("file")
            } else {
                root.checkout.join("untracked")
            };
            if !nested {
                fs::write(&path, b"before").unwrap();
            }
            let inventory =
                workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap();
            let shown = inventory
                .untracked
                .iter()
                .map(|entry| UntrackedItem::Path {
                    checkout: entry.checkout.clone(),
                    path: entry.path.clone(),
                    identity: entry.identity.clone(),
                })
                .collect();
            let before = fs::metadata(&path).unwrap();
            let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_len(before.len() + 123).unwrap();
            file.set_times(fs::FileTimes::new().set_modified(before.modified().unwrap()))
                .unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().modified().unwrap(),
                before.modified().unwrap()
            );
            let error = capture_workspace_snapshot(
                &FileGit,
                &root.checkout,
                &CheckoutOwnership::Managed,
                &root.temp.path().join("refused.pack"),
                &CaptureConsent {
                    untracked: UntrackedConsent::Shown(shown),
                    operations: OperationConsent::None,
                },
                None,
            )
            .unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<SnapshotRefused>(),
                    Some(SnapshotRefused::Untracked(_))
                ),
                "{error:#}"
            );
        }
    }

    // Hard-won: e0cc04156: a literal source override was queried as a different configured remote.
    #[test]
    fn review_twelfth_literal_override_cannot_select_a_same_named_remote() {
        let root = Repo::new();
        let literal = root.checkout.join("mirror");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                root.origin.to_str().unwrap(),
                literal.to_str().unwrap(),
            ],
        );
        let fork = root.temp.path().join("fork.git");
        git(
            root.temp.path(),
            &[
                "clone",
                "--bare",
                root.origin.to_str().unwrap(),
                fork.to_str().unwrap(),
            ],
        );
        root.commit("published only to the other remote\n");
        git(&root.checkout, &["push", fork.to_str().unwrap(), "main"]);
        git(&root.checkout, &["remote", "remove", "origin"]);
        git(
            &root.checkout,
            &["remote", "add", "mirror", fork.to_str().unwrap()],
        );
        let ownership = CheckoutOwnership::Raw { launch: None };
        let estimate = estimate_delta_with_source(
            &FileGit,
            &root.checkout,
            &ownership,
            Some("mirror".into()),
            &ExclusionSource::Origin,
        )
        .unwrap();
        assert!(estimate.committed_bytes > 0);
        let snapshot = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &ownership,
            &root.temp.path().join("literal.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::None,
            },
            Some("mirror".into()),
        )
        .unwrap();
        assert!(snapshot.pack.is_some());
        let restored = root.restore(&snapshot, None);
        assert_eq!(
            git(&restored, &["rev-parse", "HEAD"]),
            git(&root.checkout, &["rev-parse", "HEAD"])
        );
    }

    // Hard-won: e0cc04156: an available staged module commit hid missing promised trees and blobs.
    #[test]
    fn review_twelfth_partial_module_requires_every_saved_pin_object() {
        let root = Repo::new();
        let module = Repo::new();
        let source_module = add_submodule(&root, &module);
        let base = git(&source_module, &["rev-parse", "HEAD"]);
        fs::write(
            module.checkout.join("promised"),
            "missing saved-pin content\n".repeat(4000),
        )
        .unwrap();
        git(&module.checkout, &["add", "promised"]);
        git(&module.checkout, &["commit", "-m", "published staged pin"]);
        git(&module.checkout, &["push", "origin", "main"]);
        let pin = git(&module.checkout, &["rev-parse", "HEAD"]);
        let blob = git(&module.checkout, &["rev-parse", "HEAD:promised"]);
        git(&source_module, &["fetch", "origin"]);
        git(
            &root.checkout,
            &["update-index", "--cacheinfo", &format!("160000,{pin},sub")],
        );
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        git(
            &module.origin,
            &["config", "uploadpack.allowFilter", "true"],
        );
        git(
            &module.origin,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        for filter in ["blob:none", "tree:0"] {
            let destination = root.temp.path().join(filter.replace(':', "-"));
            git(
                root.temp.path(),
                &[
                    "clone",
                    root.origin.to_str().unwrap(),
                    destination.to_str().unwrap(),
                ],
            );
            let partial = destination.join("sub");
            let uri = url::Url::from_directory_path(&module.origin)
                .unwrap()
                .to_string();
            git(
                root.temp.path(),
                &[
                    "clone",
                    &format!("--filter={filter}"),
                    "--no-checkout",
                    &uri,
                    partial.to_str().unwrap(),
                ],
            );
            // Checking out a historical commit in a treeless clone can
            // hydrate additional advertised trees. Leave that fixture unbuilt.
            if filter == "blob:none" {
                git_text(
                    &CaptureGit::new(&FileGit),
                    &partial,
                    ["checkout", "--detach", &base],
                )
                .unwrap();
            }
            let missing = if filter == "tree:0" {
                git(&module.checkout, &["rev-parse", "HEAD^{tree}"])
            } else {
                blob.clone()
            };
            assert_ne!(
                run_git(&FileGit, &partial, ["cat-file", "-e", &missing], &[])
                    .unwrap()
                    .status,
                0
            );
            let error = restore_workspace_snapshot(
                &FileGit,
                &snapshot,
                &destination,
                &RestoreBase::Provisioned {
                    allow_origin_fetch: false,
                },
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains(&missing)
                    && format!("{error:#}").contains("origin fetching is disabled"),
                "{error:#}"
            );
            assert_ne!(
                run_git(&FileGit, &partial, ["cat-file", "-e", &blob], &[])
                    .unwrap()
                    .status,
                0
            );
            restore_workspace_snapshot(
                &FileGit,
                &snapshot,
                &destination,
                &RestoreBase::Provisioned {
                    allow_origin_fetch: true,
                },
            )
            .unwrap();
            git(&partial, &["cat-file", "-e", &blob]);
            git(
                &destination,
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "update",
                    "--no-fetch",
                    "sub",
                ],
            );
            assert_eq!(git(&partial, &["rev-parse", "HEAD"]), pin);
        }
    }

    // Hard-won: e0cc04156: a published exact tip was checked only after ancestry walks through earlier tags.
    #[test]
    fn review_twelfth_exact_module_tips_need_no_ancestry_walk() {
        struct Count(std::sync::atomic::AtomicUsize);
        impl GitCommandRunner for Count {
            fn run(
                &self,
                repository: &Path,
                command: &GitCommand,
            ) -> Result<crate::archive::GitOutput> {
                if git_arguments(command)
                    .first()
                    .is_some_and(|arg| arg == "merge-base")
                {
                    self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                FileGit.run(repository, command)
            }
        }
        let root = Repo::new();
        let module = Repo::new();
        // Guarantee a smaller advertised OID distinct from the current branch tip.
        let tip = git(&module.checkout, &["rev-parse", "HEAD"]);
        for n in 0u64.. {
            let tag = format!("release-{n}");
            git(&module.checkout, &["tag", "-a", &tag, "-m", &tag]);
            let oid = git(&module.checkout, &["rev-parse", &tag]);
            if oid < tip {
                break;
            }
            git(&module.checkout, &["tag", "-d", &tag]);
        }
        git(&module.checkout, &["push", "--tags", "origin"]);
        add_submodule(&root, &module);
        let runner = Count(0.into());
        workspace_inventory(&runner, &root.checkout, &CheckoutOwnership::Managed).unwrap();
        assert_eq!(runner.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
    // Hard-won: 9a8ec9177: an exact linked root still let restore mutate another checkout's common state.
    #[test]
    fn review_thirteenth_provisioned_linked_root_refuses_before_shared_writes() {
        let saved = Repo::new();
        let snapshot = saved.capture(&CheckoutOwnership::Managed);
        let owner = Repo::new();
        git(&owner.checkout, &["branch", "keep-primary"]);
        git(&owner.checkout, &["tag", "keep-tag"]);
        fs::write(owner.checkout.join("file"), "stash owned by primary\n").unwrap();
        git(&owner.checkout, &["stash", "push", "-m", "keep stash"]);
        let linked = owner.temp.path().join("provisioned-linked");
        git(
            &owner.checkout,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        );
        let config = fs::read(owner.checkout.join(".git/config")).unwrap();
        let refs = git(&owner.checkout, &["show-ref"]);
        let stashes = git(&owner.checkout, &["stash", "list"]);
        let index = fs::read(git_path(&FileGit, &linked, "index").unwrap()).unwrap();
        let error = restore_workspace_snapshot(
            &FileGit,
            &snapshot,
            &linked,
            &RestoreBase::Provisioned {
                allow_origin_fetch: false,
            },
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("shares Git administration outside")
                && format!("{error:#}").contains("independent clone"),
            "{error:#}"
        );
        assert_eq!(
            fs::read(owner.checkout.join(".git/config")).unwrap(),
            config
        );
        assert_eq!(git(&owner.checkout, &["show-ref"]), refs);
        assert_eq!(git(&owner.checkout, &["stash", "list"]), stashes);
        assert_eq!(
            fs::read(git_path(&FileGit, &linked, "index").unwrap()).unwrap(),
            index
        );
    }

    // Hard-won: 9a8ec9177: excluded replacement refs could change saved content and hold the only copy of objects.
    #[test]
    fn review_thirteenth_replacement_refs_refuse_before_a_managed_abort() {
        let root = Repo::new();
        root.conflict();
        root.start_conflict("merge");
        let original = git(&root.checkout, &["rev-parse", "HEAD"]);
        let tree = git(&root.checkout, &["rev-parse", "HEAD^{tree}"]);
        let replacement = git(
            &root.checkout,
            &[
                "commit-tree",
                &tree,
                "-m",
                "replacement only in refs/replace",
            ],
        );
        git(&root.checkout, &["replace", &original, &replacement]);
        let index = fs::read(root.checkout.join(".git/index")).unwrap();
        let conflict = fs::read(root.checkout.join("file")).unwrap();
        let error = capture_workspace_snapshot(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Managed,
            &root.temp.path().join("refused.pack"),
            &CaptureConsent {
                untracked: UntrackedConsent::Any,
                operations: OperationConsent::Any,
            },
            None,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("replacement refs")
                && format!("{error:#}").contains("independent repository"),
            "{error:#}"
        );
        assert!(root.checkout.join(".git/MERGE_HEAD").exists());
        assert_eq!(fs::read(root.checkout.join(".git/index")).unwrap(), index);
        assert_eq!(fs::read(root.checkout.join("file")).unwrap(), conflict);
        let raw = workspace_inventory(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Raw { launch: None },
        )
        .unwrap();
        assert!(
            raw.notices
                .iter()
                .any(|notice| notice.contains("replacement refs")
                    && notice.contains("raw checkout is kept"))
        );
        git(&root.checkout, &["replace", "-d", &original]);
        root.capture(&CheckoutOwnership::Managed);
        assert!(!root.checkout.join(".git/MERGE_HEAD").exists());
        // The same omission applies inside an initialized module's object store.
        let module = Repo::new();
        let directory = add_submodule(&root, &module);
        let original = git(&directory, &["rev-parse", "HEAD"]);
        let tree = git(&directory, &["rev-parse", "HEAD^{tree}"]);
        let replacement = git(
            &directory,
            &["commit-tree", &tree, "-m", "module replacement"],
        );
        git(&directory, &["replace", &original, &replacement]);
        let error =
            workspace_inventory(&FileGit, &root.checkout, &CheckoutOwnership::Managed).unwrap_err();
        assert!(
            format!("{error:#}").contains(directory.to_str().unwrap())
                && format!("{error:#}").contains("replacement refs"),
            "{error:#}"
        );
        let raw = workspace_inventory(
            &FileGit,
            &root.checkout,
            &CheckoutOwnership::Raw { launch: None },
        )
        .unwrap();
        assert!(
            raw.notices
                .iter()
                .any(|notice| notice.contains("replacement refs"))
        );
    }

    // Hard-won: 9a8ec9177: trim() corrupted a configured module URL whose literal path ended in whitespace.
    #[test]
    fn review_thirteenth_submodule_urls_keep_literal_trailing_whitespace() {
        let root = Repo::new();
        let mut module = Repo::new();
        let spaced = module.temp.path().join("origin with space ");
        fs::rename(&module.origin, &spaced).unwrap();
        module.origin = spaced;
        add_submodule(&root, &module);
        let snapshot = root.capture(&CheckoutOwnership::Managed);
        assert_eq!(
            snapshot.initialized_submodules[0].url,
            module.origin.to_str().unwrap()
        );
        let restored = root.restore(&snapshot, None);
        let configured = config_values(&FileGit, &restored, "submodule.sub.url").unwrap();
        assert_eq!(configured, vec![module.origin.to_str().unwrap()]);
        git(&restored, &["ls-remote", &configured[0]]);
        git(&restored, &["submodule", "deinit", "-f", "sub"]);
        fs::remove_dir_all(restored.join(".git/modules/sub")).unwrap();
        git(
            &restored,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "sub",
            ],
        );
        assert_eq!(
            git(&restored.join("sub"), &["rev-parse", "HEAD"]),
            snapshot.initialized_submodules[0].commit
        );
    }
}
