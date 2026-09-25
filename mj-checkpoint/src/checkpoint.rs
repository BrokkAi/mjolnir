//! Target-side checkpoint collection and controller-side verified transfer.
//!
//! Targets own the Git worktrees and native harness history, so they build the
//! archive. The controller downloads into a same-directory temporary file and
//! only returns a teardown gate after reopening and verifying the installed
//! archive.

mod capture;
mod harness_paths;
mod native_scan;
mod restore;
pub use capture::*;
pub use harness_paths::*;
pub use native_scan::*;
pub use restore::*;

use mj_core::hex::lower_hex;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail, ensure};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::archive::{
    ArchiveInput, BundleManifest, CanonicalSessionSnapshot, CanonicalTranscriptBody,
    GitCollectionSpec, GitCommand, GitCommandRunner, GitHistoryMode, NativeArtifact, PayloadRole,
    RepositorySnapshot, SessionManifest, SystemGit, TargetManifest, collect_git_metadata_snapshot,
    collect_git_snapshot, ensure_no_symlink_ancestors, has_origin_refs, is_secret_like_path,
    read_archive_verified, remote_workspace_base, restore_git_snapshot, validate_component,
    verify_archive_streaming, write_archive_hashed, write_archive_hashed_borrowed,
};
use mj_core::config::HarnessKind;
/// Native state whose storage format is owned by the target runtime.
/// Shared archive algorithms never open a provider's database themselves.
pub trait NativeCheckpointState: Sync {
    fn collect(&self, session: &SessionManifest, home: &Path) -> Result<Vec<NativeArtifact>>;
    fn restore(
        &self,
        session: &SessionManifest,
        home: &Path,
        path: &Path,
        data: &[u8],
    ) -> Result<bool>;
}
struct NoNativeCheckpointState;
impl NativeCheckpointState for NoNativeCheckpointState {
    fn collect(&self, _session: &SessionManifest, _home: &Path) -> Result<Vec<NativeArtifact>> {
        Ok(Vec::new())
    }
    fn restore(
        &self,
        _session: &SessionManifest,
        _home: &Path,
        _path: &Path,
        _data: &[u8],
    ) -> Result<bool> {
        Ok(false)
    }
}

const MAX_NATIVE_FILE: u64 = 1024 * 1024 * 1024;
const MAX_NATIVE_TOTAL: u64 = 8 * 1024 * 1024 * 1024;
/// Version of the controller-to-exporter checkpoint specification contract.
pub const CHECKPOINT_EXPORT_PROTOCOL_VERSION: u32 = 2;
/// Version of the two-phase capture/pack contract used by ordinary checkpoints.
pub const CHECKPOINT_STAGING_PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointRepositorySpec {
    pub id: String,
    pub relative_destination: PathBuf,
    pub capture: CheckpointRepositoryCapture,
    pub origin_override: Option<String>,
}

/// Required repository capture semantics for the checkpoint protocol floor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckpointRepositoryCapture {
    /// Bundle commits reachable from HEAD but from no origin ref.
    SessionDelta,
    /// Save all clone-owned refs and stashes beyond the source launch commit.
    ManagedClone { base_commit: String },
    /// Bundle the repository relative to an explicit commit.
    DeltaFrom { base_commit: String },
    /// Bundle a managed network workspace relative to its immutable launch
    /// base recorded in `mj.baseCommit`, even after the workspace has pushed
    /// and moved its origin-tracking refs.
    RemoteWorkspace,
    /// Preserve Git provenance only. The existing worktree remains the source.
    MetadataOnly,
}

/// Uploaded target-side input. It contains provenance and paths, never secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointExportSpec {
    pub protocol_version: u32,
    pub session: SessionManifest,
    pub target: TargetManifest,
    pub bundle: BundleManifest,
    pub relay_root: PathBuf,
    pub harness_home: PathBuf,
    pub workspace_root: PathBuf,
    pub repositories: Vec<CheckpointRepositorySpec>,
    /// Controller projection latched at the relay's checkpoint barrier.
    pub canonical_session: CanonicalSessionSnapshot,
    pub output_path: PathBuf,
}

/// Small target-side input used while the relay checkpoint barrier is held.
/// Canonical controller history is deliberately absent: it is streamed only
/// after the captured target state has been sealed and ACP dispatch resumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointCaptureSpec {
    pub protocol_version: u32,
    pub session: SessionManifest,
    pub target: TargetManifest,
    pub bundle: BundleManifest,
    pub relay_root: PathBuf,
    pub harness_home: PathBuf,
    pub workspace_root: PathBuf,
    pub repositories: Vec<CheckpointRepositorySpec>,
    pub allow_empty_native: bool,
    pub stage_path: PathBuf,
    /// Refresh a prestaged generation when its native source tree is unchanged.
    #[serde(default)]
    pub refresh_existing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointPackSpec {
    pub protocol_version: u32,
    pub relay_root: PathBuf,
    pub stage_path: PathBuf,
    pub canonical_session: CanonicalSessionSnapshot,
    pub output_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedCheckpoint {
    pub stage_path: PathBuf,
    pub native_bytes: u64,
    pub repository_bytes: u64,
    pub reused_native: bool,
}

impl CheckpointExportSpec {
    pub fn read(path: &Path) -> Result<Self> {
        let body = fs::read(path)
            .with_context(|| format!("read checkpoint export spec {}", path.display()))?;
        serde_json::from_slice(&body)
            .with_context(|| format!("parse checkpoint export spec {}", path.display()))
    }

    pub fn read_from(reader: &mut impl std::io::Read) -> Result<Self> {
        let mut body = Vec::new();
        reader
            .read_to_end(&mut body)
            .context("read checkpoint export spec from standard input")?;
        serde_json::from_slice(&body).context("parse checkpoint export spec from standard input")
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let body = serde_json::to_vec_pretty(self)?;
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        std::io::Write::write_all(&mut file, &body)?;
        file.sync_all()?;
        restrict_permissions(path)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetCheckpoint {
    pub path: PathBuf,
    pub sha256: String,
    pub event_frontier: u64,
    pub event_frontier_digest: String,
    /// Phase timings measured on the target. A worker that predates this field
    /// simply omits it, so the controller must treat it as optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<CheckpointExportTimings>,
}

/// Wall-clock cost of each target-side checkpoint phase, in milliseconds.
///
/// The worker runs as a child process over ssh or `podman exec`, so its own
/// tracing output never reaches the daemon log. These numbers ride back in the
/// JSON result instead, which is the only channel the controller reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointExportTimings {
    /// Collecting native harness artifacts from the relay and harness home.
    pub native_ms: u64,
    /// Collecting Git bundles and patches for every workspace repository.
    pub repositories_ms: u64,
    /// Writing and hashing the archive.
    pub archive_ms: u64,
    /// The whole target-side operation, including validation and path resolution.
    pub total_ms: u64,
}

/// Spec argument that means "read the export spec from standard input".
///
/// Streaming the spec saves one round trip to the target, which is time the
/// relay spends with ACP dispatch frozen behind the checkpoint barrier.
pub const EXPORT_SPEC_STDIN: &str = "-";

pub fn read_json_from<T: serde::de::DeserializeOwned>(
    reader: &mut impl std::io::Read,
    description: &str,
) -> Result<T> {
    let mut body = Vec::new();
    reader
        .read_to_end(&mut body)
        .with_context(|| format!("read {description} from standard input"))?;
    serde_json::from_slice(&body)
        .with_context(|| format!("parse {description} from standard input"))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointStageManifest {
    protocol_version: u32,
    session: SessionManifest,
    target: TargetManifest,
    bundle: BundleManifest,
    source_fingerprint: Option<String>,
    native_artifacts: Vec<StagedNativeArtifact>,
    repositories: Vec<StagedRepository>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StagedNativeArtifact {
    relative_path: PathBuf,
    mode: u32,
    size: u64,
    body_path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StagedRepository {
    metadata: crate::archive::RepositoryMetadata,
    committed_bundle_path: PathBuf,
    staged_patch_path: PathBuf,
    unstaged_patch_path: PathBuf,
    untracked_tar_path: PathBuf,
}

/// A session delta is measured against every origin ref, so a repository that
/// lost its remote-tracking refs would silently have nothing to exclude. Try
/// one repair fetch, then fail the checkpoint instead of bundling full history.
///
/// This is the one network call in an export. It cannot stop on a prompt:
/// [`SystemGit`] runs every child with
/// [`NON_INTERACTIVE_GIT_ENV`](crate::archive::NON_INTERACTIVE_GIT_ENV).
///
/// Public because the same rule applies wherever a session delta is bundled,
/// including the raw-checkout conversion in the controller.
pub fn repair_origin_refs(git: &dyn GitCommandRunner, path: &Path, id: &str) -> Result<()> {
    let listed = || has_origin_refs(git, path).with_context(|| format!("repository '{id}'"));
    if listed()? {
        return Ok(());
    }
    let fetch = git.run(
        path,
        &GitCommand {
            arguments: vec!["fetch".into(), "origin".into()],
            stdin: Vec::new(),
            env: Vec::new(),
        },
    )?;
    if listed()? {
        return Ok(());
    }
    let outcome = if fetch.status == 0 {
        "repair fetch produced no origin refs".to_owned()
    } else {
        format!(
            "repair fetch failed with status {}: {}",
            fetch.status,
            String::from_utf8_lossy(&fetch.stderr).trim()
        )
    };
    bail!(
        "repository '{id}' has no origin refs to delta against; refusing to bundle full history ({outcome})"
    )
}

/// Whether the native session the checkpoint continues ever received a
/// prompt. `/clear` replaces the native session, so only the conversation
/// after the newest context boundary belongs to the current one. Codex writes
/// a rollout, and Claude Code a transcript, only when a turn runs, so a
/// session with no prompt here has no native history to lose.
pub fn current_native_session_received_prompt(snapshot: &CanonicalSessionSnapshot) -> bool {
    let start = snapshot.current_context_start();
    snapshot.transcript.iter().any(|item| {
        item.position >= start && matches!(&item.body, CanonicalTranscriptBody::User { .. })
    })
}

/// Refuse a repository with modified submodule content. A snapshot records
/// the superproject's gitlink, not the submodule's working tree, so dirty
/// submodule work would be lost silently. Public for the same reason as
/// [`repair_origin_refs`].
pub fn reject_dirty_submodules(runner: &dyn GitCommandRunner, repository: &Path) -> Result<()> {
    let output = runner.run(
        repository,
        &GitCommand {
            arguments: [
                "submodule",
                "foreach",
                "--recursive",
                "--quiet",
                "git status --porcelain",
            ]
            .into_iter()
            .map(Into::into)
            .collect(),
            stdin: Vec::new(),
            env: Vec::new(),
        },
    )?;
    ensure!(
        output.status == 0,
        "failed to inspect submodules: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        output.stdout.iter().all(u8::is_ascii_whitespace),
        "dirty submodule is unsupported"
    );
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty() && !path.is_absolute(),
        "invalid relative path"
    );
    ensure!(
        path.components()
            .all(|part| matches!(part, Component::Normal(_))),
        "relative path traversal"
    );
    Ok(())
}

pub fn resolve_target_path(path: &Path) -> Result<PathBuf> {
    ensure!(
        !path.components().any(|part| part == Component::ParentDir),
        "target path traverses a parent"
    );
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let mut components = path.components();
    if components
        .next()
        .is_some_and(|part| part.as_os_str() == "~")
    {
        let home = std::env::var_os("HOME").context("HOME is required to expand target path")?;
        return mj_core::path_input::expand_home(path, Some(Path::new(&home)));
    }
    ensure!(false, "target path must be absolute or start with '~'");
    unreachable!()
}

/// SSH and EC2 worker launch files use login-home-relative paths, matching the
/// working directory of their remote commands. Checkpoint collection runs in
/// the same account but resolves paths explicitly instead of relying on cwd.
fn resolve_home_relative_target_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute()
        || path
            .components()
            .next()
            .is_some_and(|part| part.as_os_str() == "~")
    {
        return resolve_target_path(path);
    }
    resolve_target_path(&Path::new("~").join(path))
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn file_mode(_metadata: &fs::Metadata) -> u32 {
    0o600
}

pub fn restrict_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub use mj_core::config::sync_directory;

pub fn remove_failed_checkpoint_install(path: &Path, error: anyhow::Error) -> anyhow::Error {
    match fs::remove_file(path) {
        Ok(()) => error,
        Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => error,
        Err(remove_error) => error.context(format!(
            "also failed to remove incomplete checkpoint install {}: {remove_error}",
            path.display()
        )),
    }
}

pub fn checkpoint_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(lower_hex(digest.finalize()))
}

#[cfg(test)]
mod tests;
