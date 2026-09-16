//! Target-side checkpoint collection and controller-side verified transfer.
//!
//! Targets own the Git worktrees and native harness history, so they build the
//! archive. The controller downloads into a same-directory temporary file and
//! only returns a teardown gate after reopening and verifying the installed
//! archive.

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
/// Clock-skew slack subtracted from a Codex session's own creation time before
/// it is used as an mtime floor for content probes.
const CODEX_PROBE_FLOOR_SLACK_MS: i64 = 48 * 3600 * 1000;
const CODEX_SCAN_CACHE_FILE: &str = "codex-scan-cache.json";

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointRestoreSpec {
    pub archive_path: PathBuf,
    pub workspace_root: PathBuf,
    pub relay_root: PathBuf,
    pub harness_home: PathBuf,
    pub restore_repositories: bool,
    pub restore_native: bool,
    pub discard_queued_prompts: bool,
    /// Where the primary repository actually sits, when that is not
    /// `workspace_root` joined with the archived destination. A resume that
    /// moves a session between representations puts the checkout somewhere the
    /// archive could not have named, and the restored harness session has to
    /// point at the real working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_repository_root: Option<PathBuf>,
}

pub fn restore_checkpoint(spec: &CheckpointRestoreSpec, git: &dyn GitCommandRunner) -> Result<()> {
    restore_checkpoint_with_native_state(spec, git, &NoNativeCheckpointState)
}
pub fn restore_checkpoint_with_native_state(
    spec: &CheckpointRestoreSpec,
    git: &dyn GitCommandRunner,
    native_state: &dyn NativeCheckpointState,
) -> Result<()> {
    ensure!(spec.workspace_root.is_dir(), "restore workspace is missing");
    // A restore seeds a relay that has no durable state yet. Existing state
    // means either a leaked worker still writing here or an unfinished
    // teardown, and the seed would be ignored in favour of the stale
    // snapshot, leaving a frontier no journal can support.
    let existing_relay_state = spec.relay_root.join(mj_core::relay::RELAY_STATE_FILE);
    ensure!(
        !existing_relay_state.exists(),
        "relay state already present in {}; a previous worker may still be running, \
         refusing to restore over it",
        spec.relay_root.display()
    );
    let archive = read_archive_verified(&spec.archive_path)?;
    // Deserialize and validate the schema-2 canonical projection before any
    // repository, relay, or native-session state can be mutated.
    let canonical_session = archive.canonical_session()?;
    // The relay that opens next needs the frontier it continues from and the
    // commands still queued, and nothing else. The transcript stays in the
    // archive; the controller already holds it as the durable projection.
    let mut seed = mj_core::relay::RestoredRelaySeed {
        event_frontier: canonical_session.event_frontier,
        event_frontier_digest: canonical_session.event_frontier_digest,
        queued_prompts: canonical_session.queued_prompts,
    };
    if spec.discard_queued_prompts {
        seed.queued_prompts.clear();
    }
    seed.validate()?;
    if spec.restore_repositories {
        restore_repositories_from_archive(&archive, &spec.workspace_root, git)?;
    }

    // Attachment data belongs to the relay, even when native harness history
    // is deliberately not restored. Install before publishing the queue seed.
    let image_store = mj_core::attachment::AttachmentStore::worker(&spec.relay_root);
    for descriptor in &archive.manifest.payloads {
        if let PayloadRole::NativeArtifact { relative_path } = &descriptor.role
            && relative_path.starts_with(mj_core::attachment::ARCHIVE_ATTACHMENT_DIR)
        {
            image_store.restore_artifact(relative_path, archive.payload(descriptor)?)?;
        }
    }
    for queued in &seed.queued_prompts {
        let blocks: Vec<agent_client_protocol::schema::v1::ContentBlock> = queued
            .content
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<std::result::Result<_, _>>()?;
        for reference in mj_core::attachment::references(&blocks)? {
            image_store.read(&reference)?;
        }
    }
    fs::create_dir_all(&spec.relay_root)?;
    mj_core::relay::clear_native_session_identity(&spec.relay_root)?;
    write_private_file(
        &spec.relay_root,
        Path::new(mj_core::relay::RESTORED_RELAY_SEED_FILE),
        &serde_json::to_vec(&seed)?,
        0o600,
    )?;

    if spec.restore_native {
        let target_cwd = spec.primary_repository_root.clone().or_else(|| {
            target_primary_cwd(
                &archive.manifest.bundle.primary_repository,
                &archive.manifest.repositories,
                &spec.workspace_root,
            )
        });
        for descriptor in &archive.manifest.payloads {
            let PayloadRole::NativeArtifact { relative_path } = &descriptor.role else {
                continue;
            };
            if relative_path.starts_with(mj_core::attachment::ARCHIVE_ATTACHMENT_DIR) {
                continue;
            }
            let native_data = archive.payload(descriptor)?;
            if native_state.restore(
                &archive.manifest.session,
                &spec.harness_home,
                relative_path,
                native_data,
            )? {
                continue;
            }
            ensure!(
                !relative_path.starts_with(mj_core::goal::NATIVE_ARTIFACT_ROOT),
                "this checkpoint requires native goal restoration by the target worker"
            );
            let relative_path = restored_native_relative_path(
                archive.manifest.session.harness_kind,
                relative_path,
                target_cwd.as_deref(),
            )?;
            let native_data = restored_native_artifact_bytes(
                archive.manifest.session.harness_kind,
                &relative_path,
                native_data,
                target_cwd.as_deref(),
                &spec.harness_home,
            )?;
            validate_relative_path(&relative_path)?;
            ensure!(
                !is_secret_like_path(&relative_path),
                "native artifact path is secret-like"
            );
            write_private_file(
                &spec.harness_home,
                &relative_path,
                &native_data,
                descriptor.mode,
            )?;
        }
    }
    Ok(())
}

/// Reads the controller-owned projection without restoring target artifacts.
pub fn read_checkpoint_session(path: &Path) -> Result<CanonicalSessionSnapshot> {
    Ok(verify_archive_streaming(path)?.canonical_session)
}

pub fn restore_repositories(
    archive_path: &Path,
    workspace_root: &Path,
    git: &dyn GitCommandRunner,
) -> Result<()> {
    ensure!(workspace_root.is_dir(), "restore workspace is missing");
    let archive = read_archive_verified(archive_path)?;
    restore_repositories_from_archive(&archive, workspace_root, git)
}

fn restore_repositories_from_archive(
    archive: &crate::archive::VerifiedArchive,
    workspace_root: &Path,
    git: &dyn GitCommandRunner,
) -> Result<()> {
    for repository in &archive.manifest.repositories {
        let id = &repository.metadata.id;
        let snapshot = archived_repository_snapshot(archive, repository)?;
        let path = workspace_root.join(&repository.metadata.relative_destination);
        restore_git_snapshot(git, &path, &snapshot)
            .with_context(|| format!("restore repository {id:?}"))?;
    }
    Ok(())
}

fn archived_repository_snapshot(
    archive: &crate::archive::VerifiedArchive,
    repository: &crate::archive::RepositoryManifest,
) -> Result<RepositorySnapshot> {
    let id = &repository.metadata.id;
    Ok(RepositorySnapshot {
        metadata: repository.metadata.clone(),
        committed_bundle: archive
            .payload_by_role(&PayloadRole::GitBundle {
                repository_id: id.clone(),
            })?
            .to_vec(),
        staged_patch: archive
            .payload_by_role(&PayloadRole::GitStagedPatch {
                repository_id: id.clone(),
            })?
            .to_vec(),
        unstaged_patch: archive
            .payload_by_role(&PayloadRole::GitUnstagedPatch {
                repository_id: id.clone(),
            })?
            .to_vec(),
        untracked_tar: archive
            .payload_by_role(&PayloadRole::GitUntrackedTar {
                repository_id: id.clone(),
            })?
            .to_vec(),
    })
}

/// Restore a checkpoint's only repository into an existing checkout, on a
/// branch the caller names.
///
/// A resume that moves a session out of its workspace restores it into a
/// worktree of the user's own repository, where the archived branch is usually
/// already checked out somewhere else and `git checkout -B` would refuse it.
/// Returns the branch the checkpoint recorded.
pub fn restore_single_repository_onto_branch(
    archive_path: &Path,
    repository_path: &Path,
    branch: &str,
    git: &dyn GitCommandRunner,
) -> Result<Option<String>> {
    ensure!(repository_path.is_dir(), "restore checkout is missing");
    let archive = read_archive_verified(archive_path)?;
    let [repository] = archive.manifest.repositories.as_slice() else {
        bail!(
            "this checkpoint holds {} repositories; exactly one can be restored into a checkout",
            archive.manifest.repositories.len()
        );
    };
    let mut snapshot = archived_repository_snapshot(&archive, repository)?;
    let archived_branch = snapshot.metadata.branch.replace(branch.to_owned());
    // This explicit host restore uses a linked worktree whose local Git
    // configuration is shared with the user's main checkout; keep managed
    // network configuration out of that shared config.
    snapshot.metadata.remote_workspace = false;
    restore_git_snapshot(git, repository_path, &snapshot)
        .with_context(|| format!("restore repository {:?}", repository.metadata.id))?;
    Ok(archived_branch)
}

/// Native session files use harness-specific working-directory keys. Rewrite
fn restored_native_relative_path(
    harness: HarnessKind,
    relative_path: &Path,
    target_cwd: Option<&Path>,
) -> Result<PathBuf> {
    let Some(target_cwd) = target_cwd else {
        return Ok(relative_path.to_path_buf());
    };
    let mut components = relative_path.components();
    match harness {
        HarnessKind::Claude => {
            if components.next() != Some(Component::Normal("projects".as_ref()))
                || components.next().is_none()
            {
                return Ok(relative_path.to_path_buf());
            }
            let mut rewritten = PathBuf::from("projects");
            rewritten.push(claude_project_slug(target_cwd));
            rewritten.extend(components);
            Ok(rewritten)
        }
        HarnessKind::Kimi => {
            if components.next() != Some(Component::Normal("sessions".as_ref()))
                || components.next().is_none()
            {
                return Ok(relative_path.to_path_buf());
            }
            let mut rewritten = PathBuf::from("sessions");
            rewritten.push(kimi_workspace_key(target_cwd));
            rewritten.extend(components);
            Ok(rewritten)
        }
        HarnessKind::Grok => {
            if components.next() != Some(Component::Normal("sessions".as_ref()))
                || components.next().is_none()
            {
                return Ok(relative_path.to_path_buf());
            }
            let mut rewritten = PathBuf::from("sessions");
            rewritten.push(grok_cwd_key(target_cwd));
            rewritten.extend(components);
            Ok(rewritten)
        }
        HarnessKind::Codex | HarnessKind::Muse => Ok(relative_path.to_path_buf()),
    }
}

fn target_primary_cwd(
    primary_repository: &str,
    repositories: &[crate::archive::RepositoryManifest],
    workspace_root: &Path,
) -> Option<PathBuf> {
    repositories
        .iter()
        .find(|repository| repository.metadata.id == primary_repository)
        .map(|primary| workspace_root.join(&primary.metadata.relative_destination))
}
fn restored_native_artifact_bytes(
    harness: HarnessKind,
    relative_path: &Path,
    data: &[u8],
    target_cwd: Option<&Path>,
    harness_home: &Path,
) -> Result<Vec<u8>> {
    let Some(target_cwd) = target_cwd else {
        return Ok(data.to_vec());
    };
    if harness == HarnessKind::Muse
        && relative_path
            .file_name()
            .is_some_and(|name| name == "session.jsonl")
    {
        return crate::native::muse::relocate(data, target_cwd);
    }
    if !matches!(harness, HarnessKind::Kimi | HarnessKind::Grok) {
        return Ok(data.to_vec());
    }
    if harness == HarnessKind::Grok {
        return if is_grok_session_summary(relative_path) {
            rewrite_grok_session_summary(data, target_cwd, harness_home)
        } else {
            Ok(data.to_vec())
        };
    }
    if relative_path == Path::new("workspaces.json") {
        return rewrite_kimi_workspace_registry(data, target_cwd);
    }
    if relative_path == Path::new("session_index.jsonl") {
        return rewrite_kimi_session_index(data, target_cwd, harness_home);
    }
    if !is_kimi_session_state(relative_path) {
        return Ok(data.to_vec());
    }
    let mut state: Value =
        serde_json::from_slice(data).context("parse Kimi native session state")?;
    let object = state
        .as_object_mut()
        .context("Kimi native session state is not a JSON object")?;
    for key in ["workDir", "cwd"] {
        if object.contains_key(key) {
            object.insert(
                key.into(),
                Value::String(target_cwd.to_string_lossy().into_owned()),
            );
        }
    }
    Ok(serde_json::to_vec(&state)?)
}

fn rewrite_kimi_workspace_registry(data: &[u8], target_cwd: &Path) -> Result<Vec<u8>> {
    let mut registry: Value =
        serde_json::from_slice(data).context("parse Kimi workspace registry")?;
    let workspaces = registry
        .get_mut("workspaces")
        .and_then(Value::as_object_mut)
        .context("Kimi workspace registry has no workspaces object")?;
    ensure!(
        workspaces.len() == 1,
        "Kimi workspace registry must contain one imported workspace"
    );
    let (_, mut workspace) = std::mem::take(workspaces)
        .into_iter()
        .next()
        .expect("one workspace was checked");
    let workspace = workspace
        .as_object_mut()
        .context("Kimi workspace registry entry is not an object")?;
    workspace.insert(
        "root".into(),
        Value::String(target_cwd.to_string_lossy().into_owned()),
    );
    if let Some(name) = target_cwd.file_name().and_then(|name| name.to_str()) {
        workspace.insert("name".into(), Value::String(name.to_owned()));
    }
    workspaces.insert(kimi_workspace_key(target_cwd), workspace.clone().into());
    Ok(serde_json::to_vec(&registry)?)
}

fn rewrite_kimi_session_index(
    data: &[u8],
    target_cwd: &Path,
    harness_home: &Path,
) -> Result<Vec<u8>> {
    let mut rewritten = Vec::new();
    let target_workspace = kimi_workspace_key(target_cwd);
    for (line_number, line) in std::str::from_utf8(data)
        .context("decode Kimi session index")?
        .lines()
        .enumerate()
    {
        if line.trim().is_empty() {
            continue;
        }
        let mut entry: Value = serde_json::from_str(line)
            .with_context(|| format!("parse Kimi session index line {}", line_number + 1))?;
        let session_id = entry
            .get("sessionId")
            .and_then(Value::as_str)
            .context("Kimi session index entry lacks sessionId")?
            .to_owned();
        let entry = entry
            .as_object_mut()
            .context("Kimi session index entry is not an object")?;
        entry.insert(
            "workDir".into(),
            Value::String(target_cwd.to_string_lossy().into_owned()),
        );
        entry.insert(
            "sessionDir".into(),
            Value::String(
                harness_home
                    .join("sessions")
                    .join(&target_workspace)
                    .join(session_id)
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
        serde_json::to_writer(&mut rewritten, &entry)?;
        rewritten.push(b'\n');
    }
    ensure!(
        !rewritten.is_empty(),
        "Kimi session index has no imported sessions"
    );
    Ok(rewritten)
}

fn is_kimi_session_state(relative_path: &Path) -> bool {
    let mut components = relative_path.components();
    matches!(components.next(), Some(Component::Normal(component)) if component == "sessions")
        && matches!(components.next(), Some(Component::Normal(_)))
        && matches!(components.next(), Some(Component::Normal(component)) if component.to_string_lossy().starts_with("session_"))
        && matches!(components.next(), Some(Component::Normal(component)) if component == "state.json")
        && components.next().is_none()
}

fn is_grok_session_summary(relative_path: &Path) -> bool {
    grok_session_components(relative_path)
        .is_some_and(|components| components.file == "summary.json")
}

/// Grok Build records the session's working directory and home in
/// `summary.json`; both must follow the restored session to its new workspace.
fn rewrite_grok_session_summary(
    data: &[u8],
    target_cwd: &Path,
    harness_home: &Path,
) -> Result<Vec<u8>> {
    let mut summary: Value =
        serde_json::from_slice(data).context("parse Grok Build session summary")?;
    let object = summary
        .as_object_mut()
        .context("Grok Build session summary is not a JSON object")?;
    if object.contains_key("grok_home") {
        object.insert(
            "grok_home".into(),
            Value::String(harness_home.to_string_lossy().into_owned()),
        );
    }
    if let Some(info) = object.get_mut("info").and_then(Value::as_object_mut)
        && info.contains_key("cwd")
    {
        info.insert(
            "cwd".into(),
            Value::String(target_cwd.to_string_lossy().into_owned()),
        );
    }
    Ok(serde_json::to_vec(&summary)?)
}

struct GrokSessionPath<'a> {
    session: &'a str,
    file: &'a str,
}

/// Split `sessions/<cwd-key>/<session-uuid>/<file>` into the parts Hel needs.
/// Anything with a different shape is not a Grok Build session artifact.
fn grok_session_components(relative: &Path) -> Option<GrokSessionPath<'_>> {
    let components = relative
        .components()
        .map(|component| match component {
            Component::Normal(component) => component.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let [root, _cwd_key, session, file] = components.as_slice() else {
        return None;
    };
    (*root == "sessions").then_some(GrokSessionPath { session, file })
}

/// Runtime state that must not travel in a checkpoint: advisory lock files and
/// the sessions-wide search index.
fn grok_session_artifact(relative: &Path, session_id: &str) -> bool {
    grok_session_components(relative).is_some_and(|components| {
        components.session == session_id
            && !components.file.ends_with(".lock")
            && !components.file.starts_with("session_search.sqlite")
    })
}

/// Grok Build's on-disk cwd-key algorithm, replicated from grok-build
/// `xai-grok-config::paths::encode_cwd_dirname`: URL-encode the working
/// directory, or fall back to `{slug}-{blake3-hex-16}` when that would exceed
/// one filesystem name.
fn grok_cwd_key(cwd: &Path) -> String {
    /// macOS APFS, Linux ext4, and NTFS all cap a name at 255 bytes.
    const MAX_DIRNAME_BYTES: usize = 255;
    const MAX_SLUG_CHARS: usize = 40;

    let cwd = cwd.to_string_lossy();
    let encoded = url_encode(&cwd);
    if encoded.len() <= MAX_DIRNAME_BYTES {
        return encoded;
    }
    let digest = blake3::hash(cwd.as_bytes()).to_hex();
    let leaf = Path::new(cwd.as_ref())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");
    let slug = grok_slug(leaf, MAX_SLUG_CHARS);
    let slug = if slug.is_empty() { "workspace" } else { &slug };
    format!("{slug}-{}", &digest[..16])
}

/// Percent-encode every byte outside the RFC 3986 unreserved set, matching the
/// `urlencoding` crate Grok Build uses.
fn url_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Grok Build's `slugify`: lowercase, non-alphanumerics collapse to a single
/// dash, trim dashes, truncate to `max_chars`.
fn grok_slug(input: &str, max_chars: usize) -> String {
    let mut slug = String::with_capacity(input.len());
    let mut previous_dash = false;
    for character in input.to_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
            previous_dash = false;
        } else if !previous_dash {
            slug.push('-');
            previous_dash = true;
        }
    }
    slug.trim_matches('-').chars().take(max_chars).collect()
}

/// Claude Code's on-disk project-key algorithm, captured from local rollouts:
/// every non-ASCII-alphanumeric cwd character becomes a hyphen.
pub fn claude_project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

/// Kimi Code keys session directories by the final cwd component and the
/// first 12 hexadecimal digits of SHA-256(cwd), captured from real rollouts.
fn kimi_workspace_key(cwd: &Path) -> String {
    let basename = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace");
    let digest = format!("{:x}", Sha256::digest(cwd.to_string_lossy().as_bytes()));
    format!("wd_{basename}_{}", &digest[..12])
}

/// Write `relative` under `root` with owner-only permissions, refusing to
/// follow a symlink anywhere along the way.
///
/// A restore lands in a directory the target user can write, so a planted
/// symlink must never redirect the write outside `root`. `Path::exists`
/// follows links and reports a dangling symlink as missing, so the destination
/// is inspected with `symlink_metadata` and, on Unix, opened with `O_NOFOLLOW`
/// so the check cannot be raced.
fn write_private_file(root: &Path, relative: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    validate_relative_path(relative)?;
    let path = root.join(relative);
    ensure_no_symlink_ancestors(root, relative)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
        // `create_dir_all` walks through symlinked directories, so re-inspect
        // the ancestors it just materialized.
        ensure_no_symlink_ancestors(root, relative)?;
    }
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(
            !metadata.file_type().is_symlink(),
            "refusing to write through symlink {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode & 0o700).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("write {}", path.display()))?;
    std::io::Write::write_all(&mut file, bytes)
        .with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode & 0o700))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

const CHECKPOINT_STAGE_MANIFEST: &str = "stage.json";

/// Capture mutable target-owned state into a sealed, uncompressed generation.
/// The caller holds the relay barrier only for this operation; packaging the
/// generation is intentionally a separate command.
pub fn capture_checkpoint(
    spec: &CheckpointCaptureSpec,
    git: &dyn GitCommandRunner,
) -> Result<CapturedCheckpoint> {
    capture_checkpoint_with_native_state(spec, git, &NoNativeCheckpointState)
}
pub fn capture_checkpoint_with_native_state(
    spec: &CheckpointCaptureSpec,
    git: &dyn GitCommandRunner,
    native_state: &dyn NativeCheckpointState,
) -> Result<CapturedCheckpoint> {
    ensure!(
        spec.protocol_version == CHECKPOINT_STAGING_PROTOCOL_VERSION,
        "unsupported checkpoint staging protocol version {}; worker supports {}",
        spec.protocol_version,
        CHECKPOINT_STAGING_PROTOCOL_VERSION
    );
    let mut resolved = spec.clone();
    resolved.relay_root = resolve_target_path(&resolved.relay_root)?;
    resolved.harness_home = resolve_target_path(&resolved.harness_home)?;
    resolved.workspace_root = resolve_target_path(&resolved.workspace_root)?;
    resolved.stage_path = resolve_target_path(&resolved.stage_path)?;
    validate_capture_spec(&resolved)?;

    let source_fingerprint_before = native_source_fingerprint(&resolved.harness_home)?;
    if resolved.refresh_existing && resolved.stage_path.is_dir() {
        if let Some(captured) =
            refresh_checkpoint_stage(&resolved, &source_fingerprint_before, git)?
        {
            return Ok(captured);
        }
        fs::remove_dir_all(&resolved.stage_path).with_context(|| {
            format!(
                "remove stale checkpoint prestage {}",
                resolved.stage_path.display()
            )
        })?;
    }

    let native_artifacts = collect_checkpoint_native_artifacts(
        &resolved.session,
        &resolved.relay_root,
        &resolved.harness_home,
        resolved.allow_empty_native,
        native_state,
    )?;
    let repositories =
        collect_checkpoint_repositories(&resolved.workspace_root, &resolved.repositories, git)?;
    let native_bytes = native_artifacts
        .iter()
        .try_fold(0_u64, |total, artifact| {
            total.checked_add(artifact.data.len() as u64)
        })
        .context("native checkpoint size overflow")?;
    let repository_bytes = checkpoint_repository_bytes(&repositories)?;

    let parent = resolved
        .stage_path
        .parent()
        .context("checkpoint stage has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create checkpoint stage parent {}", parent.display()))?;
    ensure!(
        !resolved.stage_path.exists(),
        "checkpoint stage already exists at {}",
        resolved.stage_path.display()
    );
    let temporary = tempfile::Builder::new()
        .prefix(".checkpoint-capture-")
        .tempdir_in(parent)
        .with_context(|| format!("create temporary checkpoint stage in {}", parent.display()))?;

    let mut staged_native = Vec::with_capacity(native_artifacts.len());
    for (index, artifact) in native_artifacts.into_iter().enumerate() {
        let body_path = PathBuf::from(format!("native/{index:08}"));
        write_private_file(temporary.path(), &body_path, &artifact.data, 0o600)?;
        staged_native.push(StagedNativeArtifact {
            relative_path: artifact.relative_path,
            mode: artifact.mode,
            size: artifact.data.len() as u64,
            body_path,
        });
    }
    let staged_repositories = write_staged_repositories(temporary.path(), repositories)?;
    let source_fingerprint_after = native_source_fingerprint(&resolved.harness_home)?;
    let manifest = CheckpointStageManifest {
        protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
        session: resolved.session,
        target: resolved.target,
        bundle: resolved.bundle,
        source_fingerprint: (source_fingerprint_before == source_fingerprint_after)
            .then_some(source_fingerprint_after),
        native_artifacts: staged_native,
        repositories: staged_repositories,
    };
    write_private_file(
        temporary.path(),
        Path::new(CHECKPOINT_STAGE_MANIFEST),
        &serde_json::to_vec(&manifest).context("serialize checkpoint stage manifest")?,
        0o600,
    )?;
    fs::rename(temporary.path(), &resolved.stage_path)
        .with_context(|| format!("seal checkpoint stage {}", resolved.stage_path.display()))?;

    Ok(CapturedCheckpoint {
        stage_path: resolved.stage_path,
        native_bytes,
        repository_bytes,
        reused_native: false,
    })
}

fn refresh_checkpoint_stage(
    spec: &CheckpointCaptureSpec,
    source_fingerprint_before: &str,
    git: &dyn GitCommandRunner,
) -> Result<Option<CapturedCheckpoint>> {
    let manifest_body = read_staged_file(
        &spec.stage_path,
        Path::new(CHECKPOINT_STAGE_MANIFEST),
        8 * 1024 * 1024,
    )?;
    let mut manifest: CheckpointStageManifest =
        serde_json::from_slice(&manifest_body).context("parse checkpoint stage manifest")?;
    ensure!(
        manifest.protocol_version == CHECKPOINT_STAGING_PROTOCOL_VERSION,
        "unsupported sealed checkpoint stage version {}",
        manifest.protocol_version
    );
    if manifest.session != spec.session
        || manifest.target != spec.target
        || manifest.bundle != spec.bundle
        || manifest.source_fingerprint.as_deref() != Some(source_fingerprint_before)
    {
        return Ok(None);
    }

    let repositories =
        collect_checkpoint_repositories(&spec.workspace_root, &spec.repositories, git)?;
    let repository_bytes = checkpoint_repository_bytes(&repositories)?;
    let source_fingerprint_after = native_source_fingerprint(&spec.harness_home)?;
    if source_fingerprint_after != source_fingerprint_before {
        return Ok(None);
    }
    let repositories_root = spec.stage_path.join("repositories");
    if repositories_root.exists() {
        fs::remove_dir_all(&repositories_root).with_context(|| {
            format!(
                "replace prestaged repository state {}",
                repositories_root.display()
            )
        })?;
    }
    manifest.repositories = write_staged_repositories(&spec.stage_path, repositories)?;
    manifest.source_fingerprint = Some(source_fingerprint_after);
    write_private_file(
        &spec.stage_path,
        Path::new(CHECKPOINT_STAGE_MANIFEST),
        &serde_json::to_vec(&manifest).context("serialize refreshed checkpoint stage manifest")?,
        0o600,
    )?;
    let native_bytes = manifest
        .native_artifacts
        .iter()
        .try_fold(0_u64, |total, artifact| total.checked_add(artifact.size))
        .context("native checkpoint size overflow")?;
    Ok(Some(CapturedCheckpoint {
        stage_path: spec.stage_path.clone(),
        native_bytes,
        repository_bytes,
        reused_native: true,
    }))
}

fn checkpoint_repository_bytes(repositories: &[RepositorySnapshot]) -> Result<u64> {
    repositories
        .iter()
        .try_fold(0_u64, |total, repository| {
            [
                repository.committed_bundle.len(),
                repository.staged_patch.len(),
                repository.unstaged_patch.len(),
                repository.untracked_tar.len(),
            ]
            .into_iter()
            .try_fold(total, |total, size| total.checked_add(size as u64))
        })
        .context("repository checkpoint size overflow")
}

fn write_staged_repositories(
    stage_root: &Path,
    repositories: Vec<RepositorySnapshot>,
) -> Result<Vec<StagedRepository>> {
    repositories
        .into_iter()
        .enumerate()
        .map(|(index, repository)| {
            let root = PathBuf::from(format!("repositories/{index:08}"));
            let committed_bundle_path = root.join("committed.bundle");
            let staged_patch_path = root.join("staged.patch");
            let unstaged_patch_path = root.join("unstaged.patch");
            let untracked_tar_path = root.join("untracked.tar");
            write_private_file(
                stage_root,
                &committed_bundle_path,
                &repository.committed_bundle,
                0o600,
            )?;
            write_private_file(
                stage_root,
                &staged_patch_path,
                &repository.staged_patch,
                0o600,
            )?;
            write_private_file(
                stage_root,
                &unstaged_patch_path,
                &repository.unstaged_patch,
                0o600,
            )?;
            write_private_file(
                stage_root,
                &untracked_tar_path,
                &repository.untracked_tar,
                0o600,
            )?;
            Ok(StagedRepository {
                metadata: repository.metadata,
                committed_bundle_path,
                staged_patch_path,
                unstaged_patch_path,
                untracked_tar_path,
            })
        })
        .collect()
}

fn native_source_fingerprint(root: &Path) -> Result<String> {
    let mut digest = Sha256::new();
    fingerprint_tree(root, root, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn fingerprint_tree(root: &Path, path: &Path, digest: &mut Sha256) -> Result<()> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("scan checkpoint source {}", path.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .context("checkpoint source escaped its root")?;
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect checkpoint source {}", path.display()))?;
        digest.update(relative.to_string_lossy().as_bytes());
        digest.update([0]);
        digest.update(metadata.len().to_le_bytes());
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        digest.update(modified.to_le_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            digest.update(metadata.mode().to_le_bytes());
        }
        if metadata.is_dir() {
            digest.update(b"directory");
            fingerprint_tree(root, &path, digest)?;
        } else if metadata.is_file() {
            digest.update(b"file");
        } else {
            digest.update(b"other");
        }
    }
    Ok(())
}

/// Package a previously sealed target generation with the controller's
/// canonical projection. This operation runs after ACP dispatch resumes.
pub fn pack_checkpoint(spec: &CheckpointPackSpec) -> Result<TargetCheckpoint> {
    let started = std::time::Instant::now();
    ensure!(
        spec.protocol_version == CHECKPOINT_STAGING_PROTOCOL_VERSION,
        "unsupported checkpoint staging protocol version {}; worker supports {}",
        spec.protocol_version,
        CHECKPOINT_STAGING_PROTOCOL_VERSION
    );
    spec.canonical_session.validate()?;
    let relay_root = resolve_target_path(&spec.relay_root)?;
    let stage_path = resolve_target_path(&spec.stage_path)?;
    let output_path = resolve_target_path(&spec.output_path)?;
    validate_stage_path(&relay_root, &stage_path, "checkpoint stage")?;
    validate_stage_path(&relay_root, &output_path, "checkpoint archive")?;
    ensure!(stage_path.is_dir(), "checkpoint stage is missing");

    let result = (|| -> Result<TargetCheckpoint> {
        let manifest_body = read_staged_file(
            &stage_path,
            Path::new(CHECKPOINT_STAGE_MANIFEST),
            8 * 1024 * 1024,
        )?;
        let manifest: CheckpointStageManifest =
            serde_json::from_slice(&manifest_body).context("parse checkpoint stage manifest")?;
        ensure!(
            manifest.protocol_version == CHECKPOINT_STAGING_PROTOCOL_VERSION,
            "unsupported sealed checkpoint stage version {}",
            manifest.protocol_version
        );
        let native_started = std::time::Instant::now();
        let native_artifacts = manifest
            .native_artifacts
            .into_iter()
            .map(|artifact| {
                let data = read_staged_file(&stage_path, &artifact.body_path, MAX_NATIVE_FILE)?;
                ensure!(
                    data.len() as u64 == artifact.size,
                    "staged native artifact size changed"
                );
                Ok(NativeArtifact {
                    relative_path: artifact.relative_path,
                    data,
                    mode: artifact.mode,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let native_ms = native_started.elapsed().as_millis() as u64;
        let repositories_started = std::time::Instant::now();
        let repositories = manifest
            .repositories
            .into_iter()
            .map(|repository| {
                Ok(RepositorySnapshot {
                    metadata: repository.metadata,
                    committed_bundle: read_staged_file(
                        &stage_path,
                        &repository.committed_bundle_path,
                        MAX_NATIVE_TOTAL,
                    )?,
                    staged_patch: read_staged_file(
                        &stage_path,
                        &repository.staged_patch_path,
                        MAX_NATIVE_TOTAL,
                    )?,
                    unstaged_patch: read_staged_file(
                        &stage_path,
                        &repository.unstaged_patch_path,
                        MAX_NATIVE_TOTAL,
                    )?,
                    untracked_tar: read_staged_file(
                        &stage_path,
                        &repository.untracked_tar_path,
                        MAX_NATIVE_TOTAL,
                    )?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let repositories_ms = repositories_started.elapsed().as_millis() as u64;
        let event_frontier = spec.canonical_session.event_frontier;
        let event_frontier_digest = spec.canonical_session.event_frontier_digest.clone();
        let archive_started = std::time::Instant::now();
        let sha256 = write_archive_hashed(
            &output_path,
            &ArchiveInput {
                session: manifest.session,
                target: manifest.target,
                bundle: manifest.bundle,
                canonical_session: spec.canonical_session.clone(),
                native_artifacts,
                repositories,
            },
        )?;
        let archive_ms = archive_started.elapsed().as_millis() as u64;
        Ok(TargetCheckpoint {
            path: output_path.clone(),
            sha256,
            event_frontier,
            event_frontier_digest,
            timings: Some(CheckpointExportTimings {
                native_ms,
                repositories_ms,
                archive_ms,
                total_ms: started.elapsed().as_millis() as u64,
            }),
        })
    })();
    if let Err(cleanup_error) = fs::remove_dir_all(&stage_path)
        && cleanup_error.kind() != std::io::ErrorKind::NotFound
    {
        return match result {
            Ok(_) => Err(cleanup_error).with_context(|| {
                format!("remove consumed checkpoint stage {}", stage_path.display())
            }),
            Err(error) => Err(error.context(format!(
                "also failed to remove checkpoint stage {}: {cleanup_error}",
                stage_path.display()
            ))),
        };
    }
    result
}

fn validate_stage_path(relay_root: &Path, path: &Path, name: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure!(
        parent.starts_with(relay_root),
        "{name} must be beneath relay root"
    );
    Ok(())
}

fn read_staged_file(root: &Path, relative: &Path, maximum: u64) -> Result<Vec<u8>> {
    validate_relative_path(relative)?;
    ensure_no_symlink_ancestors(root, relative)?;
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("inspect staged checkpoint file {}", path.display()))?;
    ensure!(metadata.is_file(), "staged checkpoint path is not a file");
    ensure!(
        metadata.len() <= maximum,
        "staged checkpoint file is too large"
    );
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("open staged checkpoint file {}", path.display()))?;
    let mut body = Vec::with_capacity(metadata.len().min(usize::MAX as u64) as usize);
    std::io::Read::read_to_end(&mut file, &mut body)
        .with_context(|| format!("read staged checkpoint file {}", path.display()))?;
    ensure!(
        body.len() as u64 == metadata.len(),
        "staged checkpoint file size changed"
    );
    Ok(body)
}

pub fn export_checkpoint(spec: &CheckpointExportSpec) -> Result<TargetCheckpoint> {
    export_checkpoint_with_git(spec, &SystemGit)
}

pub fn export_checkpoint_with_git(
    spec: &CheckpointExportSpec,
    git: &dyn GitCommandRunner,
) -> Result<TargetCheckpoint> {
    export_checkpoint_with_native_state(spec, git, &NoNativeCheckpointState)
}
pub fn export_checkpoint_with_native_state(
    spec: &CheckpointExportSpec,
    git: &dyn GitCommandRunner,
    native_state: &dyn NativeCheckpointState,
) -> Result<TargetCheckpoint> {
    let started = std::time::Instant::now();
    ensure!(
        spec.protocol_version == CHECKPOINT_EXPORT_PROTOCOL_VERSION,
        "unsupported checkpoint export protocol version {}; worker supports {}",
        spec.protocol_version,
        CHECKPOINT_EXPORT_PROTOCOL_VERSION
    );
    let relay_root = resolve_target_path(&spec.relay_root)?;
    let harness_home = resolve_target_path(&spec.harness_home)?;
    let workspace_root = resolve_target_path(&spec.workspace_root)?;
    let output_path = resolve_target_path(&spec.output_path)?;
    spec.canonical_session.validate()?;
    validate_checkpoint_source(
        &spec.session,
        &relay_root,
        &harness_home,
        &workspace_root,
        &spec.repositories,
    )?;
    validate_stage_path(&relay_root, &output_path, "checkpoint archive")?;
    let event_frontier = spec.canonical_session.event_frontier;
    let event_frontier_digest = spec.canonical_session.event_frontier_digest.clone();
    // A session that never accepted a prompt legitimately has no native
    // harness artifacts yet; requiring them would make an unused session
    // impossible to close cleanly.
    let prompted = canonical_session_contains_prompt(&spec.canonical_session);
    let native_started = std::time::Instant::now();
    let native_artifacts = collect_checkpoint_native_artifacts(
        &spec.session,
        &relay_root,
        &harness_home,
        !prompted,
        native_state,
    )?;
    let native_ms = native_started.elapsed().as_millis() as u64;
    let repositories_started = std::time::Instant::now();
    let repositories = collect_checkpoint_repositories(&workspace_root, &spec.repositories, git)?;
    let repositories_ms = repositories_started.elapsed().as_millis() as u64;
    // The export runs while the relay's barrier freezes ACP dispatch, so it
    // hashes the archive it just wrote instead of structurally re-reading it.
    // `CheckpointTransfer::execute` checks the controller's copy against this
    // digest; resume/import performs full structural verification when read.
    let archive_started = std::time::Instant::now();
    let sha256 = write_archive_hashed_borrowed(
        &output_path,
        &spec.session,
        &spec.target,
        &spec.bundle,
        &spec.canonical_session,
        &native_artifacts,
        &repositories,
    )?;
    let archive_ms = archive_started.elapsed().as_millis() as u64;
    Ok(TargetCheckpoint {
        path: output_path,
        sha256,
        event_frontier,
        event_frontier_digest,
        timings: Some(CheckpointExportTimings {
            native_ms,
            repositories_ms,
            archive_ms,
            total_ms: started.elapsed().as_millis() as u64,
        }),
    })
}

fn collect_checkpoint_repositories(
    workspace_root: &Path,
    specifications: &[CheckpointRepositorySpec],
    git: &dyn GitCommandRunner,
) -> Result<Vec<RepositorySnapshot>> {
    // Indexed parallel iteration preserves the spec order, which in turn keeps
    // the manifest and ZIP entry order deterministic.
    specifications
        .par_iter()
        .map(|repository| {
            let path = workspace_root.join(&repository.relative_destination);
            ensure!(path.is_dir(), "repository {} is missing", path.display());
            let (history, remote_base) = match &repository.capture {
                CheckpointRepositoryCapture::MetadataOnly => {
                    return collect_git_metadata_snapshot(
                        git,
                        &path,
                        &GitCollectionSpec {
                            id: repository.id.clone(),
                            relative_destination: repository.relative_destination.clone(),
                            history: GitHistoryMode::NoBundle,
                            origin_override: repository.origin_override.clone(),
                        },
                    )
                    .with_context(|| format!("repository '{}'", repository.id));
                }
                CheckpointRepositoryCapture::SessionDelta => {
                    repair_origin_refs(git, &path, &repository.id)?;
                    (GitHistoryMode::SessionDelta, None)
                }
                CheckpointRepositoryCapture::DeltaFrom { base_commit } => {
                    (GitHistoryMode::DeltaFrom(base_commit.clone()), None)
                }
                CheckpointRepositoryCapture::RemoteWorkspace => {
                    let base_commit = remote_workspace_base(git, &path)
                        .with_context(|| format!("repository '{}'", repository.id))?
                        .with_context(|| {
                            format!(
                                "repository '{}' is not marked as a managed remote workspace",
                                repository.id
                            )
                        })?;
                    (
                        GitHistoryMode::DeltaFrom(base_commit.clone()),
                        Some(base_commit),
                    )
                }
            };
            reject_dirty_submodules(git, &path)
                .with_context(|| format!("repository '{}'", repository.id))?;
            let mut snapshot = collect_git_snapshot(
                git,
                &path,
                &GitCollectionSpec {
                    id: repository.id.clone(),
                    relative_destination: repository.relative_destination.clone(),
                    history,
                    origin_override: repository.origin_override.clone(),
                },
            )
            .with_context(|| format!("repository '{}'", repository.id))?;
            if let Some(base_commit) = remote_base {
                // select_git_history records the merge base. Preserve the
                // immutable launch value exactly so a split/fork checkpoint
                // can restore the workspace marker after origin moved.
                ensure!(
                    !snapshot.metadata.origin.is_empty(),
                    "repository '{}' managed workspace has no Git origin",
                    repository.id
                );
                mj_core::remote_git::validate_network_url(&snapshot.metadata.origin)?;
                for url in &snapshot.metadata.push_urls {
                    mj_core::remote_git::validate_network_url(url)?;
                }
                snapshot.metadata.base_commit = base_commit;
                snapshot.metadata.remote_workspace = true;
            }
            Ok(snapshot)
        })
        .collect()
}

/// Codex exports carry the relay-root scan cache so that a long-lived session
/// probes each unrelated rollout at most once.
fn collect_checkpoint_native_artifacts(
    session: &SessionManifest,
    relay_root: &Path,
    harness_home: &Path,
    allow_empty: bool,
    native_state: &dyn NativeCheckpointState,
) -> Result<Vec<NativeArtifact>> {
    let session_id = &session.native_session_id;
    let mut artifacts = if session.harness_kind != HarnessKind::Codex {
        collect_native_artifacts(session.harness_kind, harness_home, session_id, allow_empty)?
    } else {
        let mut cache = load_codex_scan_cache(relay_root, session_id);
        let known = cache.not_ours.len();
        let artifacts = collect_native_artifacts_cached(
            HarnessKind::Codex,
            harness_home,
            session_id,
            allow_empty,
            Some(&mut cache),
        )?;
        if cache.not_ours.len() != known {
            save_codex_scan_cache(relay_root, &cache)?;
        }
        artifacts
    };
    artifacts.extend(native_state.collect(session, harness_home)?);
    artifacts.extend(mj_core::attachment::AttachmentStore::worker(relay_root).archive_artifacts()?);
    let launch_path = relay_root.join("launch.json");
    match read_project_memory_checkpoint_endpoint(&launch_path) {
        Ok(launch) => {
            if let Some(memory) = launch.project_memory {
                let root = resolve_home_relative_target_path(&memory.root)?;
                anyhow::ensure!(
                    root.starts_with(harness_home),
                    "project memory replica is outside the harness home"
                );
                if root.is_dir() {
                    collect_claude_memory_tree(harness_home, &root, &mut artifacts)?;
                }
            }
        }
        Err(_error) if !launch_path.exists() => {}
        Err(error) => return Err(error.context("read project memory checkpoint endpoint")),
    }
    artifacts.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    artifacts.dedup_by(|left, right| left.relative_path == right.relative_path);
    let total = artifacts.iter().try_fold(0_u64, |total, artifact| {
        total.checked_add(artifact.data.len() as u64)
    });
    anyhow::ensure!(
        total.context("native artifact size overflow")? <= MAX_NATIVE_TOTAL,
        "native session artifacts are too large"
    );
    Ok(artifacts)
}

/// Checkpoint exporters can be refreshed independently of the live worker, so
/// read only the stable launch fields needed for collection. Fully parsing the
/// current worker config would reject launch files written by older versions.
#[derive(Deserialize)]
struct ProjectMemoryCheckpointLaunch {
    #[serde(default)]
    project_memory: Option<ProjectMemoryCheckpointEndpoint>,
}

#[derive(Deserialize)]
struct ProjectMemoryCheckpointEndpoint {
    root: PathBuf,
}

fn read_project_memory_checkpoint_endpoint(path: &Path) -> Result<ProjectMemoryCheckpointLaunch> {
    let body =
        fs::read(path).with_context(|| format!("read worker launch config {}", path.display()))?;
    serde_json::from_slice(&body)
        .with_context(|| format!("parse worker launch config {}", path.display()))
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

fn validate_capture_spec(spec: &CheckpointCaptureSpec) -> Result<()> {
    validate_checkpoint_source(
        &spec.session,
        &spec.relay_root,
        &spec.harness_home,
        &spec.workspace_root,
        &spec.repositories,
    )?;
    validate_stage_path(&spec.relay_root, &spec.stage_path, "checkpoint stage")
}

fn validate_checkpoint_source(
    session: &SessionManifest,
    relay_root: &Path,
    harness_home: &Path,
    workspace_root: &Path,
    repositories: &[CheckpointRepositorySpec],
) -> Result<()> {
    validate_component(&session.id, "session ID")?;
    validate_component(&session.native_session_id, "native session ID")?;
    ensure!(relay_root.is_dir(), "relay root is missing");
    ensure!(harness_home.is_dir(), "harness home is missing");
    ensure!(workspace_root.is_dir(), "workspace root is missing");
    ensure!(!repositories.is_empty(), "checkpoint has no repositories");
    let mut ids = BTreeSet::new();
    let mut destinations = BTreeSet::new();
    for repository in repositories {
        validate_component(&repository.id, "repository ID")?;
        validate_relative_path(&repository.relative_destination)?;
        if let CheckpointRepositoryCapture::DeltaFrom { base_commit } = &repository.capture {
            ensure!(!base_commit.trim().is_empty(), "base commit is empty");
        }
        ensure!(ids.insert(&repository.id), "duplicate repository ID");
        ensure!(
            destinations.insert(&repository.relative_destination),
            "duplicate destination"
        );
    }
    Ok(())
}

pub fn canonical_session_contains_prompt(snapshot: &CanonicalSessionSnapshot) -> bool {
    snapshot
        .transcript
        .iter()
        .any(|item| matches!(&item.body, CanonicalTranscriptBody::User { .. }))
}

/// Rollouts whose `session_meta` header named a different resumable thread.
/// Codex writes that header once, when it creates the file, so a negative
/// verdict never turns positive and is safe to remember across exports.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodexScanCache {
    session_id: String,
    not_ours: BTreeSet<PathBuf>,
}

impl CodexScanCache {
    fn empty(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_owned(),
            not_ours: BTreeSet::new(),
        }
    }
}

/// Everything that lets the Codex walk skip a content probe.
#[derive(Default)]
struct CodexProbeContext<'a> {
    /// Rollouts modified before this unix-ms instant cannot belong to the
    /// session, so they are never opened. `None` disables the gate.
    floor_ms: Option<i64>,
    cache: Option<&'a mut CodexScanCache>,
}

/// A missing, unreadable, corrupt, or foreign-session cache is not an error:
/// the cache only ever saves work, so falling back to an empty one is correct.
fn load_codex_scan_cache(relay_root: &Path, session_id: &str) -> CodexScanCache {
    fs::read(relay_root.join(CODEX_SCAN_CACHE_FILE))
        .ok()
        .and_then(|body| serde_json::from_slice::<CodexScanCache>(&body).ok())
        .filter(|cache| cache.session_id == session_id)
        .unwrap_or_else(|| CodexScanCache::empty(session_id))
}

fn save_codex_scan_cache(relay_root: &Path, cache: &CodexScanCache) -> Result<()> {
    let relative = Path::new(CODEX_SCAN_CACHE_FILE);
    write_private_file(relay_root, relative, &serde_json::to_vec(cache)?, 0o600).with_context(
        || {
            format!(
                "write Codex scan cache {}",
                relay_root.join(relative).display()
            )
        },
    )
}

/// Codex native session IDs are UUIDv7, whose leading 48 bits hold the
/// session's creation time in unix milliseconds.
fn uuid_v7_timestamp_ms(id: &str) -> Option<i64> {
    let groups = id.split('-').collect::<Vec<_>>();
    let [first, second, third, _, _] = groups.as_slice() else {
        return None;
    };
    let shaped = groups.iter().zip([8, 4, 4, 4, 12]).all(|(group, width)| {
        group.len() == width
            && group
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    });
    if !shaped || !third.starts_with('7') {
        return None;
    }
    i64::from_str_radix(&format!("{first}{second}"), 16).ok()
}

fn unix_millis(time: SystemTime) -> Option<i64> {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(since_epoch) => i64::try_from(since_epoch.as_millis()).ok(),
        Err(before_epoch) => i64::try_from(before_epoch.duration().as_millis())
            .ok()
            .map(|millis| -millis),
    }
}

pub fn collect_native_artifacts(
    harness: HarnessKind,
    home: &Path,
    session_id: &str,
    allow_empty: bool,
) -> Result<Vec<NativeArtifact>> {
    collect_native_artifacts_cached(harness, home, session_id, allow_empty, None)
}

fn collect_native_artifacts_cached(
    harness: HarnessKind,
    home: &Path,
    session_id: &str,
    allow_empty: bool,
    cache: Option<&mut CodexScanCache>,
) -> Result<Vec<NativeArtifact>> {
    validate_component(session_id, "native session ID")?;
    let roots: &[&str] = match harness {
        HarnessKind::Codex => &["sessions", "archived_sessions"],
        HarnessKind::Claude => &["projects", "session-env", "file-history"],
        HarnessKind::Kimi | HarnessKind::Grok => &["sessions"],
        HarnessKind::Muse => &[".data/muse/sessions"],
    };
    let mut probe = match harness {
        HarnessKind::Codex => CodexProbeContext {
            floor_ms: uuid_v7_timestamp_ms(session_id)
                .map(|created_ms| created_ms - CODEX_PROBE_FLOOR_SLACK_MS),
            cache,
        },
        _ => CodexProbeContext::default(),
    };
    let mut output = Vec::new();
    for relative in roots {
        let root = home.join(relative);
        if root.is_dir() {
            collect_native_tree(
                harness,
                home,
                &root,
                session_id,
                false,
                &mut probe,
                &mut output,
            )?;
        }
    }
    if harness == HarnessKind::Kimi && !output.is_empty() {
        collect_kimi_registry_artifacts(home, session_id, &mut output)?;
    }
    if harness == HarnessKind::Claude {
        collect_claude_memory_artifacts(home, session_id, &mut output)?;
    }
    output.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    ensure!(
        allow_empty || !output.is_empty(),
        "no session artifacts found"
    );
    let total = output
        .iter()
        .try_fold(0_u64, |total, artifact| {
            total.checked_add(artifact.data.len() as u64)
        })
        .context("native artifact size overflow")?;
    ensure!(
        total <= MAX_NATIVE_TOTAL,
        "native session artifacts are too large"
    );
    Ok(output)
}
/// Collect native artifacts for an import whose locator already resolved the
/// exact source artifact. Codex rollouts are standalone JSONL files, so this
/// avoids probing every historical rollout (and any unrelated corrupt one).
pub fn collect_import_native_artifacts(
    harness: HarnessKind,
    home: &Path,
    session_id: &str,
    source_path: &Path,
) -> Result<Vec<NativeArtifact>> {
    if harness == HarnessKind::Muse {
        return collect_muse_import_artifacts(home, session_id, source_path);
    }
    if harness != HarnessKind::Codex {
        return collect_native_artifacts(harness, home, session_id, false);
    }
    validate_component(session_id, "native session ID")?;
    let relative = source_path.strip_prefix(home).with_context(|| {
        format!(
            "Codex rollout {} is outside {}",
            source_path.display(),
            home.display()
        )
    })?;
    validate_relative_path(relative)?;
    ensure!(
        matches!(relative.components().next(), Some(Component::Normal(component)) if component == "sessions" || component == "archived_sessions"),
        "Codex rollout '{}' is outside a session root",
        source_path.display()
    );
    let name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    ensure!(
        name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"),
        "Codex rollout '{}' is not a JSONL artifact",
        source_path.display()
    );
    ensure!(
        !is_secret_like_path(relative),
        "Codex rollout '{}' has a forbidden path",
        source_path.display()
    );
    let metadata = fs::symlink_metadata(source_path)
        .with_context(|| format!("stat Codex rollout {}", source_path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "Codex rollout '{}' is not a regular file",
        source_path.display()
    );
    ensure!(
        metadata.len() <= MAX_NATIVE_FILE,
        "Codex rollout is too large"
    );
    Ok(vec![NativeArtifact {
        relative_path: relative.to_path_buf(),
        data: fs::read(source_path)
            .with_context(|| format!("read Codex rollout {}", source_path.display()))?,
        mode: file_mode(&metadata),
    }])
}

/// External Muse storage has a separate XDG root. Normalize the selected
/// subtree into the same private layout used by ordinary worker checkpoints.
fn collect_muse_import_artifacts(
    home: &Path,
    session_id: &str,
    source_path: &Path,
) -> Result<Vec<NativeArtifact>> {
    validate_component(session_id, "Muse native session ID")?;
    let sessions_root = crate::native::muse_sessions_root(home)?;
    let relative = source_path
        .strip_prefix(&sessions_root)
        .context("Muse session log is outside its native session root")?;
    validate_relative_path(relative)?;
    ensure_no_symlink_ancestors(&sessions_root, relative)?;
    let directory = source_path
        .parent()
        .context("Muse session has no directory")?;
    ensure!(
        source_path
            .file_name()
            .is_some_and(|name| name == "session.jsonl")
            && directory.file_name().is_some_and(|name| name == session_id),
        "Muse native session ID does not match its source path"
    );
    let mut output = Vec::new();
    collect_native_tree(
        HarnessKind::Muse,
        &sessions_root,
        directory,
        session_id,
        false,
        &mut CodexProbeContext::default(),
        &mut output,
    )?;
    ensure!(
        !output.is_empty(),
        "Muse native session contains no durable artifacts"
    );
    let total = output
        .iter()
        .try_fold(0u64, |total, artifact| {
            total.checked_add(artifact.data.len() as u64)
        })
        .context("Muse native artifact size overflow")?;
    ensure!(
        total <= MAX_NATIVE_TOTAL,
        "Muse native session artifacts are too large"
    );
    for artifact in &mut output {
        artifact.relative_path = Path::new(".data/muse/sessions").join(&artifact.relative_path);
    }
    output.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(output)
}

fn collect_kimi_registry_artifacts(
    home: &Path,
    session_id: &str,
    output: &mut Vec<NativeArtifact>,
) -> Result<()> {
    let source_workspace = output
        .iter()
        .find_map(|artifact| kimi_source_workspace(&artifact.relative_path, session_id))
        .context("Kimi native session state artifact is missing")?;
    let workspaces_path = home.join("workspaces.json");
    let metadata = fs::symlink_metadata(&workspaces_path)
        .with_context(|| format!("read Kimi workspace registry {}", workspaces_path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "Kimi workspace registry is not a regular file"
    );
    ensure!(
        metadata.len() <= MAX_NATIVE_FILE,
        "Kimi workspace registry is too large"
    );
    let workspaces: Value = serde_json::from_slice(&fs::read(&workspaces_path)?)
        .context("parse Kimi workspace registry")?;
    let workspace = workspaces
        .pointer(&format!("/workspaces/{source_workspace}"))
        .cloned()
        .with_context(|| format!("Kimi workspace {source_workspace:?} is missing from registry"))?;
    let mut selected_workspaces = serde_json::Map::new();
    selected_workspaces.insert(source_workspace, workspace);
    output.push(NativeArtifact {
        relative_path: PathBuf::from("workspaces.json"),
        data: serde_json::to_vec(&json!({
            "version": workspaces.get("version").cloned().unwrap_or(Value::Null),
            "deleted_workspace_ids": [],
            "workspaces": selected_workspaces,
        }))?,
        mode: file_mode(&metadata),
    });

    let index_path = home.join("session_index.jsonl");
    let metadata = fs::symlink_metadata(&index_path)
        .with_context(|| format!("read Kimi session index {}", index_path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "Kimi session index is not a regular file"
    );
    ensure!(
        metadata.len() <= MAX_NATIVE_FILE,
        "Kimi session index is too large"
    );
    let mut selected = Vec::new();
    for (line_number, line) in fs::read_to_string(&index_path)?.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: Value = serde_json::from_str(line)
            .with_context(|| format!("parse Kimi session index line {}", line_number + 1))?;
        if entry.get("sessionId").and_then(Value::as_str) == Some(session_id) {
            serde_json::to_writer(&mut selected, &entry)?;
            selected.push(b'\n');
        }
    }
    ensure!(
        !selected.is_empty(),
        "Kimi session index does not contain native session {session_id:?}"
    );
    output.push(NativeArtifact {
        relative_path: PathBuf::from("session_index.jsonl"),
        data: selected,
        mode: file_mode(&metadata),
    });
    Ok(())
}

fn kimi_source_workspace(relative_path: &Path, session_id: &str) -> Option<String> {
    let mut components = relative_path.components();
    (components.next() == Some(Component::Normal("sessions".as_ref()))).then_some(())?;
    let workspace = components.next()?.as_os_str().to_str()?.to_owned();
    let session = components.next()?.as_os_str().to_str()?;
    let file = components.next()?.as_os_str().to_str()?;
    (components.next().is_none()
        && file == "state.json"
        && (session == session_id || session == format!("session_{session_id}")))
    .then_some(workspace)
}

/// Claude keeps per-project memory next to the transcripts, outside the
/// session-id subtree the main pass walks, so capture it in a post-pass.
///
/// The memory directory is scoped to the slug that owns this session's
/// transcript. On a LocalBare target the harness home is the user's real
/// `~/.claude`, which holds memory for every unrelated project; only the
/// session's own project memory may leave the machine.
fn collect_claude_memory_artifacts(
    home: &Path,
    session_id: &str,
    output: &mut Vec<NativeArtifact>,
) -> Result<()> {
    let mut slugs: Vec<String> = output
        .iter()
        .filter_map(|artifact| claude_session_project_slug(&artifact.relative_path, session_id))
        .collect();
    slugs.sort();
    slugs.dedup();
    // An unprompted session exported with `allow_empty` has no transcript, so
    // there is no project to scope memory to.
    for slug in slugs {
        let root = home.join("projects").join(&slug).join("memory");
        if root.is_dir() {
            collect_claude_memory_tree(home, &root, output)?;
        }
    }
    Ok(())
}

/// Return the project slug when `relative_path` is this session's transcript
/// (`projects/<slug>/<session_id>.jsonl`) or lives in its session subtree
/// (`projects/<slug>/<session_id>/...`).
fn claude_session_project_slug(relative_path: &Path, session_id: &str) -> Option<String> {
    let mut components = relative_path.components();
    (components.next() == Some(Component::Normal("projects".as_ref()))).then_some(())?;
    let slug = components.next()?.as_os_str().to_str()?.to_owned();
    let entry = components.next()?.as_os_str().to_str()?;
    let is_transcript = entry == format!("{session_id}.jsonl") && components.next().is_none();
    (is_transcript || entry == session_id).then_some(slug)
}

fn collect_claude_memory_tree(
    home: &Path,
    path: &Path,
    output: &mut Vec<NativeArtifact>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            collect_claude_memory_tree(home, &entry?.path(), output)?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Ok(());
    }
    let relative = path.strip_prefix(home)?;
    if is_secret_like_path(relative) {
        return Ok(());
    }
    ensure!(
        metadata.len() <= MAX_NATIVE_FILE,
        "native artifact is too large"
    );
    validate_relative_path(relative)?;
    output.push(NativeArtifact {
        relative_path: relative.to_path_buf(),
        data: fs::read(path)?,
        mode: file_mode(&metadata),
    });
    Ok(())
}

fn collect_native_tree(
    harness: HarnessKind,
    home: &Path,
    path: &Path,
    session_id: &str,
    inside_session: bool,
    probe: &mut CodexProbeContext<'_>,
    output: &mut Vec<NativeArtifact>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    let inside = inside_session
        || path.file_name().is_some_and(|name| {
            name == session_id
                || (harness == HarnessKind::Kimi
                    && name.to_str() == Some(&format!("session_{session_id}")))
        });
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            collect_native_tree(
                harness,
                home,
                &entry?.path(),
                session_id,
                inside,
                probe,
                output,
            )?;
        }
        return Ok(());
    }
    ensure!(metadata.is_file(), "native artifact is not a regular file");
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let relative = path.strip_prefix(home)?;
    let selected = match harness {
        HarnessKind::Codex => {
            (name.contains(session_id)
                && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst")))
                || (name.ends_with(".jsonl")
                    && codex_probe_selects(probe, path, relative, &metadata, session_id))
        }
        HarnessKind::Claude => inside || name == format!("{session_id}.jsonl"),
        HarnessKind::Kimi => inside && kimi_session_artifact(relative, session_id),
        HarnessKind::Grok => inside && grok_session_artifact(relative, session_id),
        HarnessKind::Muse => inside && name == "session.jsonl",
    };
    if !selected || is_secret_like_path(relative) {
        return Ok(());
    }
    ensure!(
        metadata.len() <= MAX_NATIVE_FILE,
        "native artifact is too large"
    );
    validate_relative_path(relative)?;
    output.push(NativeArtifact {
        relative_path: relative.to_path_buf(),
        data: fs::read(path)?,
        mode: file_mode(&metadata),
    });
    Ok(())
}

fn kimi_session_artifact(relative: &Path, session_id: &str) -> bool {
    let components = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(component) => component.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    let Some(session_index) = components.iter().position(|component| {
        *component == session_id || *component == format!("session_{session_id}")
    }) else {
        return false;
    };
    matches!(&components[session_index + 1..], ["state.json"])
        || matches!(
            &components[session_index + 1..],
            ["agents", _, "wire.jsonl"]
        )
}

/// Content-probe fallback for a root rollout whose filename no longer carries
/// its resumable thread ID, for example after Codex archives or renames it.
/// Opening every rollout costs gigabytes of reads on a busy `~/.codex`, so two
/// gates come first.
///
/// The mtime floor is filesystem truth: rollout filenames encode ambiguous
/// local time, historical rollouts are never rewritten, and hel's own restore
/// rewrites the files it installs with a fresh mtime. A rollout last modified
/// before the session was created cannot mention that session.
fn codex_probe_selects(
    probe: &mut CodexProbeContext<'_>,
    path: &Path,
    relative: &Path,
    metadata: &fs::Metadata,
    session_id: &str,
) -> bool {
    // An unreadable mtime or a non-UUIDv7 session ID fails open into the probe.
    if let Some(floor_ms) = probe.floor_ms
        && let Ok(modified) = metadata.modified()
        && unix_millis(modified).is_some_and(|modified_ms| modified_ms < floor_ms)
    {
        return false;
    }
    if probe
        .cache
        .as_ref()
        .is_some_and(|cache| cache.not_ours.contains(relative))
    {
        return false;
    }
    if codex_rollout_has_thread_id(path, session_id) {
        return true;
    }
    if let Some(cache) = probe.cache.as_mut() {
        cache.not_ours.insert(relative.to_path_buf());
    }
    false
}

fn codex_rollout_has_thread_id(path: &Path, thread_id: &str) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    for _ in 0..8 {
        line.clear();
        let Ok(read) = reader.read_line(&mut line) else {
            return false;
        };
        if read == 0 {
            break;
        }
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            return false;
        };
        // A rollout carries exactly one `session_meta` header, so the first one
        // settles the question without parsing the rest of the file.
        if record.get("type").and_then(Value::as_str) == Some("session_meta") {
            let Some(payload) = record.get("payload") else {
                return false;
            };
            // Modern Codex stores the resumable thread ID in `id` and a shared
            // session-tree ID in `session_id`. Child agents therefore have the
            // same `session_id` as the root but must not be archived as roots.
            // Only old records with no `id` field use the legacy fallback.
            if let Some(id) = payload.get("id") {
                return id.as_str() == Some(thread_id);
            }
            return payload.get("session_id").and_then(Value::as_str) == Some(thread_id);
        }
    }
    false
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
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests;
