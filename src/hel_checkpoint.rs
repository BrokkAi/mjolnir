//! Target-side checkpoint collection and controller-side verified transfer.
//!
//! Targets own the Git worktrees and native harness history, so they build the
//! archive. The controller downloads into a same-directory temporary file and
//! only returns a teardown gate after reopening and verifying the installed
//! archive.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail, ensure};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::hel_archive::{
    ArchiveInput, BundleManifest, CanonicalSessionSnapshot, CanonicalTranscriptBody,
    GitCollectionSpec, GitCommand, GitCommandRunner, GitHistoryMode, NativeArtifact, PayloadRole,
    RepositorySnapshot, SessionManifest, SystemGit, TargetManifest, collect_git_metadata_snapshot,
    collect_git_snapshot, ensure_no_symlink_ancestors, has_origin_refs, is_secret_like_path,
    read_archive_verified, restore_git_snapshot, validate_component, verify_archive_streaming,
    write_archive_hashed,
};
use crate::hel_config::HarnessKind;
use crate::hel_targets::{
    CommandExecutor, CommandPlan, CommandSpec, SshTarget, TargetLocator, join_remote_command,
    worker_root,
};
const MAX_NATIVE_FILE: u64 = 1024 * 1024 * 1024;
const MAX_NATIVE_TOTAL: u64 = 8 * 1024 * 1024 * 1024;
/// Version of the controller-to-exporter checkpoint specification contract.
pub const CHECKPOINT_EXPORT_PROTOCOL_VERSION: u32 = 1;
/// Version of the two-phase capture/pack contract used by ordinary checkpoints.
pub const CHECKPOINT_STAGING_PROTOCOL_VERSION: u32 = 1;
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

/// Hidden target CLI entry point: `mj worker export-checkpoint --spec PATH|-`.
pub fn export_from_spec_file(path: &Path) -> Result<TargetCheckpoint> {
    if path == Path::new(EXPORT_SPEC_STDIN) {
        return export_from_spec_reader(&mut std::io::stdin().lock());
    }
    export_checkpoint(&CheckpointExportSpec::read(path)?)
}

pub fn export_from_spec_reader(reader: &mut impl std::io::Read) -> Result<TargetCheckpoint> {
    export_checkpoint(&CheckpointExportSpec::read_from(reader)?)
}

pub fn capture_from_spec_reader(reader: &mut impl std::io::Read) -> Result<CapturedCheckpoint> {
    let spec: CheckpointCaptureSpec = read_json_from(reader, "checkpoint capture spec")?;
    capture_checkpoint(&spec, &SystemGit)
}

pub fn pack_from_spec_reader(reader: &mut impl std::io::Read) -> Result<TargetCheckpoint> {
    let spec: CheckpointPackSpec = read_json_from(reader, "checkpoint pack spec")?;
    pack_checkpoint(&spec)
}

fn read_json_from<T: serde::de::DeserializeOwned>(
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
    metadata: crate::hel_archive::RepositoryMetadata,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryRestoreSpec {
    pub archive_path: PathBuf,
    pub workspace_root: PathBuf,
}

pub fn restore_repositories_from_spec_file(path: &Path) -> Result<()> {
    let body = fs::read(path)
        .with_context(|| format!("read repository restore spec {}", path.display()))?;
    let mut spec: RepositoryRestoreSpec = serde_json::from_slice(&body)
        .with_context(|| format!("parse repository restore spec {}", path.display()))?;
    spec.archive_path = resolve_target_path(&spec.archive_path)?;
    spec.workspace_root = resolve_target_path(&spec.workspace_root)?;
    restore_repositories(&spec.archive_path, &spec.workspace_root, &SystemGit)
}

pub fn restore_from_spec_file(path: &Path) -> Result<()> {
    let body = fs::read(path)
        .with_context(|| format!("read checkpoint restore spec {}", path.display()))?;
    let mut spec: CheckpointRestoreSpec = serde_json::from_slice(&body)
        .with_context(|| format!("parse checkpoint restore spec {}", path.display()))?;
    spec.archive_path = resolve_target_path(&spec.archive_path)?;
    spec.workspace_root = resolve_target_path(&spec.workspace_root)?;
    spec.relay_root = resolve_target_path(&spec.relay_root)?;
    spec.harness_home = resolve_target_path(&spec.harness_home)?;
    restore_checkpoint(&spec, &SystemGit)
}

pub fn restore_checkpoint(spec: &CheckpointRestoreSpec, git: &dyn GitCommandRunner) -> Result<()> {
    ensure!(spec.workspace_root.is_dir(), "restore workspace is missing");
    // A restore seeds a relay that has no durable state yet. Existing state
    // means either a leaked worker still writing here or an unfinished
    // teardown, and the seed would be ignored in favour of the stale
    // snapshot, leaving a frontier no journal can support.
    let existing_relay_state = spec.relay_root.join(crate::hel_worker::RELAY_STATE_FILE);
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
    let mut seed = crate::hel_worker::RestoredRelaySeed {
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

    fs::create_dir_all(&spec.relay_root)?;
    crate::hel_worker::clear_native_session_identity(&spec.relay_root)?;
    write_private_file(
        &spec.relay_root,
        Path::new(crate::hel_worker::RESTORED_RELAY_SEED_FILE),
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
            let native_data = archive.payload(descriptor)?;
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
    archive: &crate::hel_archive::VerifiedArchive,
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
    archive: &crate::hel_archive::VerifiedArchive,
    repository: &crate::hel_archive::RepositoryManifest,
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
        HarnessKind::Codex | HarnessKind::Deepseek => Ok(relative_path.to_path_buf()),
    }
}

fn target_primary_cwd(
    primary_repository: &str,
    repositories: &[crate::hel_archive::RepositoryManifest],
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
    if !matches!(harness, HarnessKind::Kimi | HarnessKind::Grok) {
        return Ok(data.to_vec());
    }
    let Some(target_cwd) = target_cwd else {
        return Ok(data.to_vec());
    };
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
    let started = std::time::Instant::now();
    ensure!(
        spec.protocol_version == CHECKPOINT_EXPORT_PROTOCOL_VERSION,
        "unsupported checkpoint export protocol version {}; worker supports {}",
        spec.protocol_version,
        CHECKPOINT_EXPORT_PROTOCOL_VERSION
    );
    let mut resolved = spec.clone();
    resolved.relay_root = resolve_target_path(&resolved.relay_root)?;
    resolved.harness_home = resolve_target_path(&resolved.harness_home)?;
    resolved.workspace_root = resolve_target_path(&resolved.workspace_root)?;
    resolved.output_path = resolve_target_path(&resolved.output_path)?;
    let spec = &resolved;
    validate_export_spec(spec)?;
    let event_frontier = spec.canonical_session.event_frontier;
    let event_frontier_digest = spec.canonical_session.event_frontier_digest.clone();
    // A session that never accepted a prompt legitimately has no native
    // harness artifacts yet; requiring them would make an unused session
    // impossible to close cleanly.
    let prompted = canonical_session_contains_prompt(&spec.canonical_session);
    let native_started = std::time::Instant::now();
    let native_artifacts = collect_checkpoint_native_artifacts(
        &spec.session,
        &spec.relay_root,
        &spec.harness_home,
        !prompted,
    )?;
    let native_ms = native_started.elapsed().as_millis() as u64;
    let repositories_started = std::time::Instant::now();
    let repositories =
        collect_checkpoint_repositories(&spec.workspace_root, &spec.repositories, git)?;
    let repositories_ms = repositories_started.elapsed().as_millis() as u64;
    // The export runs while the relay's barrier freezes ACP dispatch, so it
    // hashes the archive it just wrote instead of structurally re-reading it.
    // `CheckpointTransfer::execute` performs the one full structural verify,
    // on the copy the controller actually installs.
    let archive_started = std::time::Instant::now();
    let sha256 = write_archive_hashed(
        &spec.output_path,
        &ArchiveInput {
            session: spec.session.clone(),
            target: spec.target.clone(),
            bundle: spec.bundle.clone(),
            canonical_session: spec.canonical_session.clone(),
            native_artifacts,
            repositories,
        },
    )?;
    let archive_ms = archive_started.elapsed().as_millis() as u64;
    Ok(TargetCheckpoint {
        path: spec.output_path.clone(),
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
            let history = match &repository.capture {
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
                    GitHistoryMode::SessionDelta
                }
                CheckpointRepositoryCapture::DeltaFrom { base_commit } => {
                    GitHistoryMode::DeltaFrom(base_commit.clone())
                }
            };
            reject_dirty_submodules(git, &path)
                .with_context(|| format!("repository '{}'", repository.id))?;
            collect_git_snapshot(
                git,
                &path,
                &GitCollectionSpec {
                    id: repository.id.clone(),
                    relative_destination: repository.relative_destination.clone(),
                    history,
                    origin_override: repository.origin_override.clone(),
                },
            )
            .with_context(|| format!("repository '{}'", repository.id))
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
/// [`NON_INTERACTIVE_GIT_ENV`](crate::hel_archive::NON_INTERACTIVE_GIT_ENV).
fn repair_origin_refs(git: &dyn GitCommandRunner, path: &Path, id: &str) -> Result<()> {
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

fn validate_export_spec(spec: &CheckpointExportSpec) -> Result<()> {
    spec.canonical_session.validate()?;
    validate_checkpoint_source(
        &spec.session,
        &spec.relay_root,
        &spec.harness_home,
        &spec.workspace_root,
        &spec.repositories,
    )?;
    validate_stage_path(&spec.relay_root, &spec.output_path, "checkpoint archive")
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

/// Rollouts whose `session_meta` header named a different session. Codex
/// writes that header once, when it creates the file, so a negative verdict
/// never turns positive and is safe to remember across exports.
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
        HarnessKind::Kimi | HarnessKind::Grok | HarnessKind::Deepseek => &["sessions"],
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
        HarnessKind::Deepseek => inside && name.starts_with("session.jsonl"),
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

/// Content-probe fallback for continuation rollouts, whose filename carries a
/// different file UUID than the session they belong to. Opening every rollout
/// costs gigabytes of reads on a busy `~/.codex`, so two gates come first.
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
    if codex_rollout_has_session_id(path, session_id) {
        return true;
    }
    if let Some(cache) = probe.cache.as_mut() {
        cache.not_ours.insert(relative.to_path_buf());
    }
    false
}

fn codex_rollout_has_session_id(path: &Path, session_id: &str) -> bool {
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
            return record
                .pointer("/payload/session_id")
                .and_then(Value::as_str)
                == Some(session_id);
        }
    }
    false
}

fn reject_dirty_submodules(runner: &dyn GitCommandRunner, repository: &Path) -> Result<()> {
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

/// Export by streaming the spec to the worker's standard input.
///
/// Every wrapper this builds keeps the target's stdin attached: the container
/// engines are invoked with `exec -i` and `ssh` forwards stdin by default.
pub fn export_stdin_command(locator: &TargetLocator, session_id: &str) -> Result<CommandSpec> {
    export_command(locator, session_id, EXPORT_SPEC_STDIN)
}

pub fn capture_stdin_command(locator: &TargetLocator, session_id: &str) -> Result<CommandSpec> {
    checkpoint_stdin_command(
        locator,
        session_id,
        "capture-checkpoint",
        "capture target checkpoint",
    )
}

pub fn pack_stdin_command(locator: &TargetLocator, session_id: &str) -> Result<CommandSpec> {
    checkpoint_stdin_command(
        locator,
        session_id,
        "pack-checkpoint",
        "pack target checkpoint",
    )
}

fn checkpoint_stdin_command(
    locator: &TargetLocator,
    session_id: &str,
    subcommand: &str,
    purpose: &str,
) -> Result<CommandSpec> {
    let root = worker_root(locator, session_id)?;
    let args = vec![format!("{root}/hel"), "worker".into(), subcommand.into()];
    let command = match locator {
        TargetLocator::LocalBare { .. } => {
            let mut args = args;
            CommandSpec::new(args.remove(0), args)
        }
        TargetLocator::LocalPodman { container_id, .. } => {
            container_exec("podman", container_id, args)
        }
        TargetLocator::LocalDocker { container_id } => container_exec("docker", container_id, args),
        TargetLocator::AppleContainer { container_id } => {
            container_exec("container", container_id, args)
        }
        TargetLocator::AwsEc2 { ssh, .. } | TargetLocator::SshBare { ssh, .. } => {
            ssh_command(ssh, args)
        }
        TargetLocator::SshPodman {
            ssh, container_id, ..
        } => {
            let mut remote = vec![
                "podman".into(),
                "exec".into(),
                "-i".into(),
                container_id.clone(),
            ];
            remote.extend(args);
            ssh_command(ssh, remote)
        }
    };
    Ok(command.purpose(purpose))
}

pub fn export_command(
    locator: &TargetLocator,
    session_id: &str,
    spec_path: &str,
) -> Result<CommandSpec> {
    validate_remote_path(spec_path)?;
    let root = worker_root(locator, session_id)?;
    let args = vec![
        format!("{root}/hel"),
        "worker".into(),
        "export-checkpoint".into(),
        "--spec".into(),
        spec_path.into(),
    ];
    let command = match locator {
        TargetLocator::LocalBare { .. } => {
            let mut args = args;
            CommandSpec::new(args.remove(0), args)
        }
        TargetLocator::LocalPodman { container_id, .. } => {
            container_exec("podman", container_id, args)
        }
        TargetLocator::LocalDocker { container_id } => container_exec("docker", container_id, args),
        TargetLocator::AppleContainer { container_id } => {
            container_exec("container", container_id, args)
        }
        TargetLocator::AwsEc2 { ssh, .. } | TargetLocator::SshBare { ssh, .. } => {
            ssh_command(ssh, args)
        }
        TargetLocator::SshPodman {
            ssh, container_id, ..
        } => {
            let mut remote = vec![
                "podman".into(),
                "exec".into(),
                "-i".into(),
                container_id.clone(),
            ];
            remote.extend(args);
            ssh_command(ssh, remote)
        }
    };
    Ok(command.purpose("export target checkpoint"))
}

pub fn restore_command(
    locator: &TargetLocator,
    session_id: &str,
    spec_path: &str,
) -> Result<CommandSpec> {
    validate_remote_path(spec_path)?;
    let root = worker_root(locator, session_id)?;
    let args = vec![
        format!("{root}/hel"),
        "worker".into(),
        "restore-checkpoint".into(),
        "--spec".into(),
        spec_path.into(),
    ];
    let command = match locator {
        TargetLocator::LocalBare { .. } => {
            let mut args = args;
            CommandSpec::new(args.remove(0), args)
        }
        TargetLocator::LocalPodman { container_id, .. } => {
            container_exec("podman", container_id, args)
        }
        TargetLocator::LocalDocker { container_id } => container_exec("docker", container_id, args),
        TargetLocator::AppleContainer { container_id } => {
            container_exec("container", container_id, args)
        }
        TargetLocator::AwsEc2 { ssh, .. } | TargetLocator::SshBare { ssh, .. } => {
            ssh_command(ssh, args)
        }
        TargetLocator::SshPodman {
            ssh, container_id, ..
        } => {
            let mut remote = vec![
                "podman".into(),
                "exec".into(),
                "-i".into(),
                container_id.clone(),
            ];
            remote.extend(args);
            ssh_command(ssh, remote)
        }
    };
    Ok(command.purpose("restore target checkpoint"))
}

#[derive(Debug, Clone)]
pub struct CheckpointTransfer<'a> {
    pub locator: &'a TargetLocator,
    pub session_id: &'a str,
    pub remote_archive: &'a str,
    pub destination: &'a Path,
    pub expected_event_frontier: Option<u64>,
    pub expected_event_frontier_digest: Option<&'a str>,
}

/// Unforgeable outside this module: proof that a controller-local archive was
/// verified after its atomic install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCheckpoint {
    session_id: String,
    archive_path: PathBuf,
    sha256: String,
    event_frontier: u64,
    event_frontier_digest: String,
}

impl VerifiedCheckpoint {
    pub fn archive_path(&self) -> &Path {
        &self.archive_path
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    pub fn event_frontier(&self) -> u64 {
        self.event_frontier
    }
    pub fn event_frontier_digest(&self) -> &str {
        &self.event_frontier_digest
    }
    pub const fn teardown_allowed(&self) -> bool {
        true
    }
}

impl CheckpointTransfer<'_> {
    pub fn execute(&self, executor: &impl CommandExecutor) -> Result<VerifiedCheckpoint> {
        validate_remote_path(self.remote_archive)?;
        let parent = self.destination.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let temporary = tempfile::Builder::new()
            .prefix(".hel-checkpoint-")
            .tempfile_in(parent)?;
        let path = temporary.path().to_path_buf();
        transfer_plan(self.locator, self.session_id, self.remote_archive, &path)?
            .execute(executor)
            .context("download target checkpoint")?;
        let verified = verify_archive_streaming(&path).context("verify downloaded checkpoint")?;
        ensure!(
            verified.manifest.session.id == self.session_id,
            "checkpoint session mismatch"
        );
        let canonical_session = verified.canonical_session;
        let event_frontier = canonical_session.event_frontier;
        let event_frontier_digest = canonical_session.event_frontier_digest;
        if let Some(expected) = self.expected_event_frontier {
            ensure!(
                event_frontier == expected,
                "checkpoint event frontier mismatch"
            );
        }
        if let Some(expected) = self.expected_event_frontier_digest {
            ensure!(
                event_frontier_digest == expected,
                "checkpoint event frontier digest mismatch"
            );
        }
        let sha256 = verified.archive_sha256;
        temporary
            .persist(self.destination)
            .map_err(|error| error.error)?;
        // The bytes were already verified in this same directory and the rename
        // is atomic, so installation only has to make the copy private and
        // durable; re-reading it would hash the same archive a second time.
        let post_install = (|| -> Result<()> {
            restrict_permissions(self.destination)?;
            sync_directory(parent)
        })();
        if let Err(error) = post_install {
            return Err(remove_failed_checkpoint_install(self.destination, error));
        }
        Ok(VerifiedCheckpoint {
            session_id: self.session_id.to_owned(),
            archive_path: self.destination.to_path_buf(),
            sha256,
            event_frontier,
            event_frontier_digest,
        })
    }

    pub fn cleanup_plan(&self, gate: &VerifiedCheckpoint) -> Result<CommandPlan> {
        ensure!(
            gate.session_id == self.session_id,
            "checkpoint gate belongs to another session"
        );
        cleanup_plan(self.locator, self.session_id, self.remote_archive)
    }
}

pub fn transfer_plan(
    locator: &TargetLocator,
    session_id: &str,
    remote_archive: &str,
    local_temporary: &Path,
) -> Result<CommandPlan> {
    validate_remote_path(remote_archive)?;
    ensure!(
        local_temporary.is_absolute(),
        "local temporary path must be absolute"
    );
    worker_root(locator, session_id)?;
    let local = local_temporary.to_string_lossy().into_owned();
    let mut commands = match locator {
        TargetLocator::LocalBare { .. } => vec![
            CommandSpec::new("cp", [remote_archive, local.as_str()])
                .purpose("copy local bare checkpoint"),
        ],
        TargetLocator::LocalPodman { container_id, .. } => vec![
            CommandSpec::new(
                "podman",
                ["cp", &format!("{container_id}:{remote_archive}"), &local],
            )
            .purpose("download checkpoint from local Podman"),
        ],
        TargetLocator::LocalDocker { container_id } => vec![
            CommandSpec::new(
                "docker",
                ["cp", &format!("{container_id}:{remote_archive}"), &local],
            )
            .purpose("download checkpoint from local Docker"),
        ],
        TargetLocator::AppleContainer { container_id } => vec![
            CommandSpec::new(
                "container",
                ["cp", &format!("{container_id}:{remote_archive}"), &local],
            )
            .purpose("download checkpoint from Apple container"),
        ],
        TargetLocator::AwsEc2 { ssh, .. } | TargetLocator::SshBare { ssh, .. } => {
            vec![scp_command(ssh, remote_archive, &local).purpose("download checkpoint over SSH")]
        }
        TargetLocator::SshPodman {
            ssh, container_id, ..
        } => {
            let staging = remote_staging_path(session_id)?;
            vec![
                ssh_command(ssh, ["mkdir", "-p", ".local/share/hel/transfers"])
                    .purpose("create remote checkpoint staging directory"),
                ssh_command(
                    ssh,
                    [
                        "podman",
                        "cp",
                        &format!("{container_id}:{remote_archive}"),
                        &staging,
                    ],
                )
                .purpose("stage remote Podman checkpoint"),
            ]
        }
    };
    if let TargetLocator::SshPodman { ssh, .. } = locator {
        commands.push(
            scp_command(ssh, &remote_staging_path(session_id)?, &local)
                .purpose("download remote Podman checkpoint over SSH"),
        );
    }
    Ok(CommandPlan {
        description: format!("download checkpoint for {session_id}"),
        commands,
    })
}

fn cleanup_plan(locator: &TargetLocator, session_id: &str, remote: &str) -> Result<CommandPlan> {
    validate_remote_path(remote)?;
    worker_root(locator, session_id)?;
    let commands = match locator {
        TargetLocator::LocalBare { .. } => vec![
            CommandSpec::new("rm", ["-f", "--", remote])
                .purpose("remove local bare checkpoint staging"),
        ],
        TargetLocator::LocalPodman { container_id, .. } => vec![container_exec(
            "podman",
            container_id,
            ["rm", "-f", "--", remote],
        )],
        TargetLocator::LocalDocker { container_id } => vec![container_exec(
            "docker",
            container_id,
            ["rm", "-f", "--", remote],
        )],
        TargetLocator::AppleContainer { container_id } => vec![container_exec(
            "container",
            container_id,
            ["rm", "-f", "--", remote],
        )],
        TargetLocator::AwsEc2 { ssh, .. } | TargetLocator::SshBare { ssh, .. } => {
            vec![ssh_command(ssh, ["rm", "-f", "--", remote])]
        }
        TargetLocator::SshPodman {
            ssh, container_id, ..
        } => vec![
            ssh_command(
                ssh,
                ["podman", "exec", container_id, "rm", "-f", "--", remote],
            ),
            ssh_command(ssh, ["rm", "-f", "--", &remote_staging_path(session_id)?]),
        ],
    };
    Ok(CommandPlan {
        description: format!("clean checkpoint for {session_id}"),
        commands,
    })
}

fn remote_staging_path(session_id: &str) -> Result<String> {
    validate_component(session_id, "session ID")?;
    Ok(format!(".local/share/hel/transfers/{session_id}.hel.zip"))
}

fn scp_command(ssh: &SshTarget, remote: &str, local: &str) -> CommandSpec {
    let mut args = ssh.ssh_args.clone();
    for argument in &mut args {
        if argument == "-p" {
            *argument = "-P".into();
        }
    }
    args.push(format!("{}:{remote}", ssh.destination));
    args.push(local.into());
    CommandSpec::new("scp", args)
}

fn ssh_command(ssh: &SshTarget, args: impl IntoIterator<Item = impl AsRef<str>>) -> CommandSpec {
    let remote = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect::<Vec<_>>();
    let mut command = ssh.ssh_args.clone();
    command.push(ssh.destination.clone());
    command.push(join_remote_command(&remote));
    CommandSpec::new("ssh", command)
}

fn container_exec(
    engine: &str,
    id: &str,
    args: impl IntoIterator<Item = impl Into<String>>,
) -> CommandSpec {
    let mut command = vec!["exec".into(), "-i".into(), id.into()];
    command.extend(args.into_iter().map(Into::into));
    CommandSpec::new(engine, command)
}

fn validate_remote_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty());
    ensure!(
        path.bytes()
            .all(|byte| byte.is_ascii_alphanumeric()
                || matches!(byte, b'/' | b'~' | b'.' | b'-' | b'_')),
        "unsafe remote path"
    );
    ensure!(
        !path.split('/').any(|component| component == ".."),
        "remote path traverses parent"
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

fn resolve_target_path(path: &Path) -> Result<PathBuf> {
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
        let mut expanded = PathBuf::from(home);
        expanded.extend(components);
        return Ok(expanded);
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

fn restrict_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn remove_failed_checkpoint_install(path: &Path, error: anyhow::Error) -> anyhow::Error {
    match fs::remove_file(path) {
        Ok(()) => error,
        Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => error,
        Err(remove_error) => error.context(format!(
            "also failed to remove incomplete checkpoint install {}: {remove_error}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::process::Command;
    use std::sync::Mutex;

    use crate::hel_archive::{
        CanonicalExecutionState, CanonicalQueuedCommandKind, CanonicalQueuedPrompt,
        CanonicalSessionState, CanonicalTranscriptItem, GitOutput,
    };
    use crate::hel_targets::CommandOutput;

    const SESSION: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";
    const NATIVE: &str = "0190aabb-ccdd-7eef-9000-abcdef012345";

    /// Runs real Git, but counts repair fetches and can make them fail without
    /// reaching a network remote.
    struct RecordingGit {
        fetch_failure: bool,
        fetches: Mutex<usize>,
    }

    impl RecordingGit {
        fn forwarding() -> Self {
            Self {
                fetch_failure: false,
                fetches: Mutex::new(0),
            }
        }

        fn with_fetch_failure() -> Self {
            Self {
                fetch_failure: true,
                fetches: Mutex::new(0),
            }
        }

        fn fetches(&self) -> usize {
            *self.fetches.lock().unwrap()
        }
    }

    impl GitCommandRunner for RecordingGit {
        fn run(&self, repository: &Path, command: &GitCommand) -> Result<GitOutput> {
            if command
                .arguments
                .first()
                .is_some_and(|first| first == "fetch")
            {
                *self.fetches.lock().unwrap() += 1;
                if self.fetch_failure {
                    return Ok(GitOutput {
                        status: 128,
                        stdout: Vec::new(),
                        stderr: b"fatal: could not read from remote repository".to_vec(),
                    });
                }
            }
            SystemGit.run(repository, command)
        }
    }

    fn ssh() -> SshTarget {
        SshTarget {
            destination: "dev@example.test".into(),
            ssh_args: vec!["-p".into(), "2222".into()],
        }
    }

    fn locators() -> Vec<TargetLocator> {
        let name = crate::hel_targets::resource_name(SESSION).unwrap();
        vec![
            TargetLocator::LocalBare {
                worker_root: format!("/var/lib/hel/workers/{SESSION}"),
            },
            TargetLocator::LocalPodman {
                container_id: name.clone(),
                workspace_storage: Default::default(),
            },
            TargetLocator::AppleContainer {
                container_id: name.clone(),
            },
            TargetLocator::AwsEc2 {
                profile: "default".into(),
                region: "us-east-1".into(),
                instance_id: "i-0123456789abcdef0".into(),
                ssh: ssh(),
                workspace: format!("~/hel/{SESSION}"),
            },
            TargetLocator::SshBare {
                ssh: ssh(),
                workspace: format!("~/hel/{SESSION}"),
            },
            TargetLocator::SshPodman {
                ssh: ssh(),
                container_id: name,
                workspace_storage: Default::default(),
            },
        ]
    }

    #[test]
    fn transfer_plans_cover_all_target_boundaries() {
        let locators = locators();
        let plans = locators
            .iter()
            .map(|locator| {
                transfer_plan(
                    locator,
                    SESSION,
                    "/var/lib/hel/workers/checkpoint.hel.zip",
                    Path::new("/var/tmp/checkpoint.zip"),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(plans[0].commands[0].program, "cp");
        assert_eq!(plans[1].commands[0].program, "podman");
        assert_eq!(plans[2].commands[0].program, "container");
        assert_eq!(plans[3].commands[0].program, "scp");
        assert_eq!(plans[4].commands[0].program, "scp");
        assert_eq!(plans[5].commands.len(), 3);
        assert!(
            plans[5].commands[1]
                .args
                .last()
                .unwrap()
                .contains("'podman' 'cp'")
        );
        assert!(
            !plans[5]
                .commands
                .iter()
                .flat_map(|command| &command.args)
                .any(|arg| arg == "--remote")
        );
        assert!(plans[3].commands[0].args.contains(&"-P".into()));
    }

    #[test]
    fn empty_native_artifacts_allowed_only_for_unprompted_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let error = collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, false)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "no session artifacts found");
        let artifacts =
            collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, true).unwrap();
        assert!(artifacts.is_empty());
    }
    #[test]
    fn codex_collection_ignores_malformed_unrelated_rollouts() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions/2026/08/10");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("rollout-unrelated.jsonl"), b"{malformed\n").unwrap();
        let selected = sessions.join("rollout-renamed.jsonl");
        fs::write(
            &selected,
            format!("{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"{NATIVE}\"}}}}\n"),
        )
        .unwrap();

        let artifacts =
            collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, false).unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(
            artifacts[0].relative_path,
            PathBuf::from("sessions/2026/08/10/rollout-renamed.jsonl")
        );
    }

    /// Unix milliseconds encoded in `NATIVE`, a UUIDv7.
    const NATIVE_CREATED_MS: i64 = 0x0190_aabb_ccdd;
    const OTHER_NATIVE: &str = "0190aabb-ccdd-7eef-9000-ffffffffffff";

    fn write_rollout(path: &Path, session_id: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"{session_id}\"}}}}\n"
            ),
        )
        .unwrap();
    }

    fn set_modified_ms(path: &Path, millis: i64) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(millis as u64))
            .unwrap();
    }

    #[test]
    fn uuid_v7_timestamp_decodes_only_version_seven_uuids() {
        assert_eq!(uuid_v7_timestamp_ms(NATIVE), Some(NATIVE_CREATED_MS));
        assert_eq!(uuid_v7_timestamp_ms(SESSION), Some(0x018f_9dd2_a3b4));
        assert_eq!(
            uuid_v7_timestamp_ms("0190aabb-ccdd-4eef-9000-abcdef012345"),
            None
        );
        for malformed in [
            "",
            "not-a-uuid",
            "0190aabb-ccdd-7eef-9000-abcdef01234",
            "0190aabb-ccdd-7eef-9000-abcdef012345-extra",
            "0190AABB-CCDD-7EEF-9000-ABCDEF012345",
            "0190aabbccdd7eef9000abcdef012345",
            "0190aabg-ccdd-7eef-9000-abcdef012345",
        ] {
            assert_eq!(uuid_v7_timestamp_ms(malformed), None, "{malformed}");
        }
    }

    #[test]
    fn codex_content_probe_skips_rollouts_older_than_the_session() {
        let temp = tempfile::tempdir().unwrap();
        let rollout = temp.path().join("sessions/2026/08/10/rollout-fork.jsonl");
        write_rollout(&rollout, NATIVE);

        let artifacts =
            collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, false).unwrap();
        assert_eq!(artifacts.len(), 1);

        // Three days before the session's own UUIDv7 creation time, so past the
        // 48 hour skew slack the floor allows.
        set_modified_ms(&rollout, NATIVE_CREATED_MS - 72 * 3600 * 1000);
        let artifacts =
            collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, true).unwrap();
        assert!(artifacts.is_empty());
    }

    #[test]
    fn codex_name_matched_rollout_is_collected_whatever_its_mtime() {
        let temp = tempfile::tempdir().unwrap();
        let rollout = temp
            .path()
            .join("sessions/2026/08/10")
            .join(format!("rollout-{NATIVE}.jsonl"));
        write_rollout(&rollout, OTHER_NATIVE);
        set_modified_ms(&rollout, 1_000_000_000_000);

        let artifacts =
            collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, false).unwrap();
        assert_eq!(artifacts.len(), 1);
    }

    #[test]
    fn codex_scan_cache_makes_a_negative_probe_verdict_permanent() {
        let temp = tempfile::tempdir().unwrap();
        let foreign = temp
            .path()
            .join("sessions/2026/08/10/rollout-foreign.jsonl");
        write_rollout(&foreign, OTHER_NATIVE);
        let mut cache = CodexScanCache::empty(NATIVE);

        let artifacts = collect_native_artifacts_cached(
            HarnessKind::Codex,
            temp.path(),
            NATIVE,
            true,
            Some(&mut cache),
        )
        .unwrap();
        assert!(artifacts.is_empty());
        assert!(
            cache
                .not_ours
                .contains(Path::new("sessions/2026/08/10/rollout-foreign.jsonl"))
        );

        write_rollout(&foreign, NATIVE);
        let later = temp.path().join("sessions/2026/08/10/rollout-later.jsonl");
        write_rollout(&later, NATIVE);
        let artifacts = collect_native_artifacts_cached(
            HarnessKind::Codex,
            temp.path(),
            NATIVE,
            true,
            Some(&mut cache),
        )
        .unwrap();
        assert_eq!(
            artifacts
                .iter()
                .map(|artifact| artifact.relative_path.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("sessions/2026/08/10/rollout-later.jsonl")]
        );
    }

    #[test]
    fn codex_probe_stops_at_the_first_session_meta_header() {
        let temp = tempfile::tempdir().unwrap();
        let rollout = temp.path().join("sessions/2026/08/10/rollout-fork.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"{OTHER_NATIVE}\"}}}}\n\
                 {{\"type\":\"event_msg\"}}\n\
                 {{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"{NATIVE}\"}}}}\n\
                 {{malformed\n"
            ),
        )
        .unwrap();

        let artifacts =
            collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, true).unwrap();
        assert!(artifacts.is_empty());
    }

    #[test]
    fn codex_export_rebuilds_a_corrupt_scan_cache_and_records_verdicts() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _) = fixture(temp.path());
        let cache_path = spec.relay_root.join(CODEX_SCAN_CACHE_FILE);
        fs::write(&cache_path, b"{not json").unwrap();
        write_rollout(
            &spec
                .harness_home
                .join("sessions/2026/08/09/rollout-x.jsonl"),
            OTHER_NATIVE,
        );

        export_checkpoint(&spec).unwrap();

        let cache: CodexScanCache =
            serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
        assert_eq!(cache.session_id, NATIVE);
        assert_eq!(
            cache.not_ours,
            BTreeSet::from([PathBuf::from("sessions/2026/08/09/rollout-x.jsonl")])
        );
        assert_eq!(
            load_codex_scan_cache(&spec.relay_root, OTHER_NATIVE)
                .not_ours
                .len(),
            0
        );
    }

    #[test]
    fn prompt_detection_reads_the_materialized_transcript() {
        let mut snapshot = CanonicalSessionSnapshot {
            event_frontier: 1,
            event_frontier_digest: "a".repeat(64),
            session: CanonicalSessionState {
                execution: CanonicalExecutionState::Idle,
                last_activity_at_ms: Some(1),
                session_title: None,
                configuration: Default::default(),
            },
            transcript: Vec::new(),
            queued_prompts: Vec::new(),
        };
        assert!(!canonical_session_contains_prompt(&snapshot));
        snapshot.transcript.push(CanonicalTranscriptItem {
            stable_id: "user-1".into(),
            position: 1,
            latest_content_event_ordinal: None,
            created_at_ms: 1,
            last_changed_at_ms: 1,
            body: CanonicalTranscriptBody::User {
                content: vec![json!({"type": "text", "text": "hi"})],
            },
        });
        assert!(canonical_session_contains_prompt(&snapshot));
    }

    #[test]
    fn native_allowlist_excludes_credentials_and_other_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let session = temp
            .path()
            .join("sessions/workspace")
            .join(format!("session_{NATIVE}"));
        fs::create_dir_all(session.join("agents/main")).unwrap();
        fs::write(session.join("state.json"), b"state").unwrap();
        fs::write(session.join("agents/main/wire.jsonl"), b"events").unwrap();
        fs::create_dir_all(session.join("agents/main/tasks/bash-noise")).unwrap();
        fs::write(
            session.join("agents/main/tasks/bash-noise/output.log"),
            b"tool output",
        )
        .unwrap();
        fs::write(
            session.join("agents/main/wire.jsonl.bak-before-edit"),
            b"backup",
        )
        .unwrap();
        fs::create_dir_all(session.join("logs")).unwrap();
        fs::write(session.join("logs/kimi-code.log"), b"log").unwrap();
        fs::write(session.join("credentials.json"), b"secret").unwrap();
        let other = temp.path().join("sessions/workspace/other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("state.json"), b"other").unwrap();
        fs::write(
            temp.path().join("workspaces.json"),
            r#"{"version":1,"deleted_workspace_ids":[],"workspaces":{"workspace":{"root":"/work/app","name":"app"}}}"#,
        )
        .unwrap();
        fs::write(
            temp.path().join("session_index.jsonl"),
            format!(
                "{{\"sessionId\":\"{NATIVE}\",\"workDir\":\"/work/app\",\"sessionDir\":\"/sessions/workspace/session_{NATIVE}\"}}\n{{\"sessionId\":\"other\"}}\n"
            ),
        )
        .unwrap();
        let artifacts =
            collect_native_artifacts(HarnessKind::Kimi, temp.path(), NATIVE, false).unwrap();
        assert_eq!(artifacts.len(), 4);
        assert!(
            artifacts.iter().any(|artifact| {
                artifact.relative_path.as_path() == Path::new("workspaces.json")
            })
        );
        let index = artifacts
            .iter()
            .find(|artifact| artifact.relative_path.as_path() == Path::new("session_index.jsonl"))
            .unwrap();
        assert!(std::str::from_utf8(&index.data).unwrap().contains(NATIVE));
        assert!(!std::str::from_utf8(&index.data).unwrap().contains("other"));
        assert!(artifacts.iter().all(|artifact| {
            !artifact
                .relative_path
                .to_string_lossy()
                .contains("credentials")
        }));
        assert!(artifacts.iter().all(|artifact| {
            let path = artifact.relative_path.to_string_lossy();
            !path.contains("tasks") && !path.contains(".bak") && !path.contains("logs")
        }));
    }

    #[test]
    fn grok_allowlist_collects_one_session_directory_without_runtime_state() {
        const NATIVE: &str = "01a00c3a-553f-71e0-95ab-aa04396d3ad7";
        let temp = tempfile::tempdir().unwrap();
        let session = temp.path().join("sessions/%2Fhome%2Fme%2Fapp").join(NATIVE);
        fs::create_dir_all(&session).unwrap();
        for name in [
            "chat_history.jsonl",
            "events.jsonl",
            "prompt_context.json",
            "summary.json",
            "system_prompt.txt",
        ] {
            fs::write(session.join(name), b"payload").unwrap();
        }
        fs::write(session.join("summary.json.lock"), b"").unwrap();
        let other = temp
            .path()
            .join("sessions/%2Fhome%2Fme%2Fapp/01a00c40-55c5-78b0-85c8-ac1b99985fd0");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("summary.json"), b"other").unwrap();
        fs::write(temp.path().join("sessions/session_search.sqlite"), b"index").unwrap();

        let artifacts =
            collect_native_artifacts(HarnessKind::Grok, temp.path(), NATIVE, false).unwrap();

        let paths = artifacts
            .iter()
            .map(|artifact| artifact.relative_path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                "chat_history.jsonl",
                "events.jsonl",
                "prompt_context.json",
                "summary.json",
                "system_prompt.txt",
            ]
            .map(|name| format!("sessions/%2Fhome%2Fme%2Fapp/{NATIVE}/{name}"))
        );
    }

    #[test]
    fn deepseek_allowlist_collects_only_the_selected_session_log() {
        let temp = tempfile::tempdir().unwrap();
        let session = temp.path().join("sessions/--workspace-app--").join(NATIVE);
        fs::create_dir_all(&session).unwrap();
        fs::write(session.join("session.jsonl.zstd"), b"zstd frames").unwrap();
        fs::write(session.join("runtime.lock"), b"ephemeral").unwrap();
        let other = temp.path().join("sessions/--workspace-app--/other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("session.jsonl.zstd"), b"other").unwrap();

        let artifacts =
            collect_native_artifacts(HarnessKind::Deepseek, temp.path(), NATIVE, false).unwrap();

        assert_eq!(artifacts.len(), 1);
        assert_eq!(
            artifacts[0].relative_path,
            PathBuf::from(format!(
                "sessions/--workspace-app--/{NATIVE}/session.jsonl.zstd"
            ))
        );
    }

    #[test]
    fn restore_rewrites_grok_cwd_key_and_session_summary_for_target_workspace() {
        const NATIVE: &str = "01a00c3a-553f-71e0-95ab-aa04396d3ad7";
        let repositories = vec![crate::hel_archive::RepositoryManifest {
            metadata: crate::hel_archive::RepositoryMetadata {
                id: "app".into(),
                relative_destination: "app".into(),
                origin: "owner/app".into(),
                base_commit: "a".repeat(40),
                head_commit: "a".repeat(40),
                branch: Some("main".into()),
            },
            committed_bundle_path: "repositories/app/committed.bundle".into(),
            staged_patch_path: "repositories/app/staged.patch".into(),
            unstaged_patch_path: "repositories/app/unstaged.patch".into(),
            untracked_tar_path: "repositories/app/untracked.tar".into(),
        }];
        let path = restored_native_relative_path(
            HarnessKind::Grok,
            Path::new(&format!(
                "sessions/%2Fhome%2Fjonathan%2FProjects%2Fapp/{NATIVE}/summary.json"
            )),
            target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
        )
        .unwrap();
        assert_eq!(
            path,
            PathBuf::from(format!("sessions/%2Fworkspace%2Fapp/{NATIVE}/summary.json"))
        );

        let summary = restored_native_artifact_bytes(
            HarnessKind::Grok,
            &path,
            br#"{"info":{"id":"01a00c3a","cwd":"/home/jonathan/Projects/app"},"grok_home":"/home/jonathan/.grok","num_messages":3}"#,
            target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
            Path::new("/profiles/imported"),
        )
        .unwrap();
        let summary: Value = serde_json::from_slice(&summary).unwrap();
        assert_eq!(summary["info"]["cwd"], "/workspace/app");
        assert_eq!(summary["grok_home"], "/profiles/imported");
        assert_eq!(summary["num_messages"], 3);

        // Transcript files travel unchanged.
        let history = restored_native_artifact_bytes(
            HarnessKind::Grok,
            Path::new(&format!(
                "sessions/%2Fworkspace%2Fapp/{NATIVE}/chat_history.jsonl"
            )),
            b"{\"role\":\"user\"}\n",
            target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
            Path::new("/profiles/imported"),
        )
        .unwrap();
        assert_eq!(history, b"{\"role\":\"user\"}\n");
    }

    #[test]
    fn grok_cwd_key_url_encodes_short_paths_and_hashes_long_ones() {
        assert_eq!(
            grok_cwd_key(Path::new("/home/jonathan")),
            "%2Fhome%2Fjonathan"
        );
        assert_eq!(
            grok_cwd_key(Path::new("/workspace/app")),
            "%2Fworkspace%2Fapp"
        );
        // Unreserved characters survive; everything else is percent-encoded.
        assert_eq!(
            grok_cwd_key(Path::new("/a-b_c.d~e/f g")),
            "%2Fa-b_c.d~e%2Ff%20g"
        );
        let long = Path::new(
            "/Users/test/\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}",
        );
        let key = grok_cwd_key(long);
        assert!(key.len() <= 255);
        assert!(
            !key.starts_with("%2F"),
            "long paths use the hash form: {key}"
        );
        assert!(key.starts_with("workspace-"), "unslugifiable leaf: {key}");
    }

    #[test]
    fn claude_allowlist_collects_transcript_and_session_subtree_only() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("projects/-workspace-app");
        let subagents = project.join(NATIVE).join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(project.join(format!("{NATIVE}.jsonl")), b"transcript").unwrap();
        fs::write(subagents.join("agent-a.jsonl"), b"subagent").unwrap();
        fs::write(project.join("other-session.jsonl"), b"other").unwrap();
        fs::write(project.join("settings.json"), b"secret config").unwrap();

        let artifacts =
            collect_native_artifacts(HarnessKind::Claude, temp.path(), NATIVE, false).unwrap();
        assert_eq!(artifacts.len(), 2);
        assert!(
            artifacts
                .iter()
                .any(|artifact| artifact.relative_path.ends_with(format!("{NATIVE}.jsonl")))
        );
        assert!(
            artifacts
                .iter()
                .any(|artifact| { artifact.relative_path.ends_with("subagents/agent-a.jsonl") })
        );
    }

    #[test]
    fn claude_allowlist_collects_project_memory_for_the_session_slug_only() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("projects/-workspace-app");
        let nested = project.join("memory/notes");
        fs::create_dir_all(&nested).unwrap();
        fs::write(project.join(format!("{NATIVE}.jsonl")), b"transcript").unwrap();
        fs::write(project.join("memory/root.md"), b"root memory").unwrap();
        fs::write(nested.join("deep.md"), b"nested memory").unwrap();

        // Memory belonging to an unrelated project in the same harness home.
        let other = temp.path().join("projects/-workspace-other/memory");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("leak.md"), b"other project memory").unwrap();

        let artifacts =
            collect_native_artifacts(HarnessKind::Claude, temp.path(), NATIVE, false).unwrap();
        let paths: Vec<_> = artifacts
            .iter()
            .map(|artifact| artifact.relative_path.clone())
            .collect();
        assert!(paths.contains(&PathBuf::from(format!(
            "projects/-workspace-app/{NATIVE}.jsonl"
        ))));
        assert!(paths.contains(&PathBuf::from("projects/-workspace-app/memory/root.md")));
        assert!(paths.contains(&PathBuf::from(
            "projects/-workspace-app/memory/notes/deep.md"
        )));
        assert!(
            !paths
                .iter()
                .any(|path| path.starts_with("projects/-workspace-other")),
            "unrelated project memory leaked: {paths:?}"
        );
    }

    #[test]
    fn claude_project_memory_skips_secret_like_names() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("projects/-workspace-app");
        fs::create_dir_all(project.join("memory")).unwrap();
        fs::write(project.join(format!("{NATIVE}.jsonl")), b"transcript").unwrap();
        fs::write(project.join("memory/settings.json"), b"secret config").unwrap();
        fs::write(project.join("memory/.env"), b"TOKEN=1").unwrap();
        fs::write(project.join("memory/keep.md"), b"safe memory").unwrap();

        let artifacts =
            collect_native_artifacts(HarnessKind::Claude, temp.path(), NATIVE, false).unwrap();
        let paths: Vec<_> = artifacts
            .iter()
            .map(|artifact| artifact.relative_path.clone())
            .collect();
        assert!(paths.contains(&PathBuf::from("projects/-workspace-app/memory/keep.md")));
        assert!(!paths.contains(&PathBuf::from(
            "projects/-workspace-app/memory/settings.json"
        )));
        assert!(!paths.contains(&PathBuf::from("projects/-workspace-app/memory/.env")));
    }

    /// Collection and the archive gate read one shared rule set, so every
    /// credential name is dropped while walking the session subtree instead of
    /// failing the archive write afterwards.
    #[test]
    fn claude_allowlist_skips_credential_names_in_the_session_subtree() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("projects/-workspace-app");
        let session = project.join(NATIVE);
        fs::create_dir_all(&session).unwrap();
        fs::write(project.join(format!("{NATIVE}.jsonl")), b"transcript").unwrap();
        fs::write(session.join("notes.jsonl"), b"kept").unwrap();
        for name in [
            ".credentials.json",
            "auth.toml",
            "vendor-credentials.json",
            "vendor_credentials.json",
        ] {
            fs::write(session.join(name), b"secret").unwrap();
        }

        let artifacts =
            collect_native_artifacts(HarnessKind::Claude, temp.path(), NATIVE, false).unwrap();
        let paths: Vec<_> = artifacts
            .iter()
            .map(|artifact| artifact.relative_path.clone())
            .collect();
        let expected = [
            PathBuf::from(format!("projects/-workspace-app/{NATIVE}.jsonl")),
            PathBuf::from(format!("projects/-workspace-app/{NATIVE}/notes.jsonl")),
        ];
        assert_eq!(
            paths.len(),
            expected.len(),
            "credential names leaked into the native artifacts: {paths:?}"
        );
        for path in expected {
            assert!(paths.contains(&path), "{path:?} was not collected");
        }
    }

    #[test]
    fn claude_project_memory_is_skipped_without_a_session_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("projects/-workspace-app");
        fs::create_dir_all(project.join("memory")).unwrap();
        fs::write(project.join("memory/root.md"), b"root memory").unwrap();

        let artifacts =
            collect_native_artifacts(HarnessKind::Claude, temp.path(), NATIVE, true).unwrap();
        assert!(artifacts.is_empty(), "collected {artifacts:?}");
    }

    #[test]
    fn claude_project_slug_matches_captured_local_rollout_fixtures() {
        // These cwd/directory pairs come from the local Claude home used to
        // establish the import format. Dots are substituted just like slash.
        assert_eq!(
            claude_project_slug(Path::new("/home/jonathan/Projects/mjolnir/.mjolnir/repro")),
            "-home-jonathan-Projects-mjolnir--mjolnir-repro"
        );
        assert_eq!(
            claude_project_slug(Path::new("/tmp/mj-live-transcript.59w2Hg")),
            "-tmp-mj-live-transcript-59w2Hg"
        );
    }

    #[test]
    fn restore_rewrites_claude_project_artifacts_for_target_workspace() {
        let repositories = vec![crate::hel_archive::RepositoryManifest {
            metadata: crate::hel_archive::RepositoryMetadata {
                id: "app".into(),
                relative_destination: "app".into(),
                origin: "owner/app".into(),
                base_commit: "a".repeat(40),
                head_commit: "a".repeat(40),
                branch: Some("main".into()),
            },
            committed_bundle_path: "repositories/app/committed.bundle".into(),
            staged_patch_path: "repositories/app/staged.patch".into(),
            unstaged_patch_path: "repositories/app/unstaged.patch".into(),
            untracked_tar_path: "repositories/app/untracked.tar".into(),
        }];
        let path = restored_native_relative_path(
            HarnessKind::Claude,
            Path::new("projects/-home-me-app/session.jsonl"),
            target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("projects/-workspace-app/session.jsonl"));

        // Project memory rides along under the rewritten slug.
        let memory = restored_native_relative_path(
            HarnessKind::Claude,
            Path::new("projects/-home-me-app/memory/foo.md"),
            target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
        )
        .unwrap();
        assert_eq!(
            memory,
            PathBuf::from("projects/-workspace-app/memory/foo.md")
        );
    }

    #[test]
    fn restore_rewrites_kimi_workspace_and_state_for_target_workspace() {
        let repositories = vec![crate::hel_archive::RepositoryManifest {
            metadata: crate::hel_archive::RepositoryMetadata {
                id: "app".into(),
                relative_destination: "app".into(),
                origin: "owner/app".into(),
                base_commit: "a".repeat(40),
                head_commit: "a".repeat(40),
                branch: Some("main".into()),
            },
            committed_bundle_path: "repositories/app/committed.bundle".into(),
            staged_patch_path: "repositories/app/staged.patch".into(),
            unstaged_patch_path: "repositories/app/unstaged.patch".into(),
            untracked_tar_path: "repositories/app/untracked.tar".into(),
        }];
        let path = restored_native_relative_path(
            HarnessKind::Kimi,
            Path::new(
                "sessions/wd_kimi-code_78153cfca00c/session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2/state.json",
            ),
            target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
        )
        .unwrap();
        assert_eq!(
            path,
            PathBuf::from(
                "sessions/wd_app_af7e243d70b1/session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2/state.json",
            )
        );
        let state = restored_native_artifact_bytes(
            HarnessKind::Kimi,
            &path,
            br#"{"workDir":"/home/jonathan/Projects/kimi-code","cwd":"/home/jonathan/Projects/kimi-code"}"#,
            target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
            Path::new("/profiles/imported"),
        )
        .unwrap();
        let state: Value = serde_json::from_slice(&state).unwrap();
        assert_eq!(state["workDir"], "/workspace/app");
        assert_eq!(state["cwd"], "/workspace/app");
        let registry = restored_native_artifact_bytes(
            HarnessKind::Kimi,
            Path::new("workspaces.json"),
            br#"{"version":1,"deleted_workspace_ids":[],"workspaces":{"wd_kimi-code_78153cfca00c":{"root":"/home/jonathan/Projects/kimi-code","name":"kimi-code"}}}"#,
            target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
            Path::new("/profiles/imported"),
        )
        .unwrap();
        let registry: Value = serde_json::from_slice(&registry).unwrap();
        assert_eq!(
            registry["workspaces"]["wd_app_af7e243d70b1"]["root"],
            "/workspace/app"
        );
        assert!(
            registry["workspaces"]
                .get("wd_kimi-code_78153cfca00c")
                .is_none()
        );

        let index = restored_native_artifact_bytes(
            HarnessKind::Kimi,
            Path::new("session_index.jsonl"),
            br#"{"sessionId":"session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2","workDir":"/home/jonathan/Projects/kimi-code","sessionDir":"/home/jonathan/.kimi-code/sessions/wd_kimi-code_78153cfca00c/session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2"}"#,
            target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
            Path::new("/profiles/imported"),
        )
        .unwrap();
        let index: Value = serde_json::from_slice(&index).unwrap();
        assert_eq!(index["workDir"], "/workspace/app");
        assert_eq!(
            index["sessionDir"],
            "/profiles/imported/sessions/wd_app_af7e243d70b1/session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2"
        );
    }

    fn git(repository: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repository)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    fn fixture(temp: &Path) -> (CheckpointExportSpec, PathBuf) {
        let worker_root = temp.join("worker");
        fs::create_dir_all(&worker_root).unwrap();
        let harness_home = temp.join("codex");
        let native = harness_home.join("sessions/2026/08/09");
        fs::create_dir_all(&native).unwrap();
        fs::write(native.join(format!("rollout-{NATIVE}.jsonl")), b"native").unwrap();
        let workspace = temp.join("workspace");
        let repository = workspace.join("app");
        fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init"]);
        git(&repository, &["config", "user.email", "hel@example.test"]);
        git(&repository, &["config", "user.name", "Hel Test"]);
        fs::write(repository.join("README.md"), b"hello").unwrap();
        git(&repository, &["add", "."]);
        git(&repository, &["commit", "-m", "base"]);
        git(
            &repository,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/example/app.git",
            ],
        );
        let base = git(&repository, &["rev-parse", "HEAD"]);
        let output = worker_root.join("source.hel.zip");
        (
            CheckpointExportSpec {
                protocol_version: CHECKPOINT_EXPORT_PROTOCOL_VERSION,
                session: SessionManifest {
                    id: SESSION.into(),
                    title: "test".into(),
                    harness_kind: HarnessKind::Codex,
                    profile_id: "codex-1".into(),
                    native_session_id: NATIVE.into(),
                    created_at: "2026-08-09T00:00:00Z".into(),
                    checkpointed_at: "2026-08-09T00:01:00Z".into(),
                    hel_version: "0.1.0".into(),
                    relay_version: "0.1.0".into(),
                    adapter_version: "test".into(),
                },
                target: TargetManifest {
                    template_id: "local".into(),
                    target_kind: "podman".into(),
                    details: Default::default(),
                },
                bundle: BundleManifest {
                    id: "bundle".into(),
                    primary_repository: "app".into(),
                },
                relay_root: worker_root,
                harness_home,
                workspace_root: workspace,
                repositories: vec![CheckpointRepositorySpec {
                    id: "app".into(),
                    relative_destination: "app".into(),
                    capture: CheckpointRepositoryCapture::DeltaFrom { base_commit: base },
                    origin_override: None,
                }],
                canonical_session: CanonicalSessionSnapshot {
                    event_frontier: 1,
                    event_frontier_digest: "a".repeat(64),
                    session: CanonicalSessionState {
                        execution: CanonicalExecutionState::Idle,
                        last_activity_at_ms: Some(1),
                        session_title: Some("test".into()),
                        configuration: Default::default(),
                    },
                    transcript: vec![CanonicalTranscriptItem {
                        stable_id: "user-1".into(),
                        position: 1,
                        latest_content_event_ordinal: None,
                        created_at_ms: 1,
                        last_changed_at_ms: 1,
                        body: CanonicalTranscriptBody::User {
                            content: vec![json!({"type": "text", "text": "hello"})],
                        },
                    }],
                    queued_prompts: vec![CanonicalQueuedPrompt {
                        command_id: "queued-1".into(),
                        kind: CanonicalQueuedCommandKind::Prompt,
                        content: vec![json!({"type": "text", "text": "next"})],
                        queued_at_ms: 2,
                    }],
                },
                output_path: output.clone(),
            },
            output,
        )
    }

    #[test]
    fn staged_checkpoint_preserves_export_contents_and_defers_canonical_history() {
        let temp = tempfile::tempdir().unwrap();
        let (export_spec, legacy_path) = fixture(temp.path());
        let large_native = export_spec
            .harness_home
            .join(format!("sessions/2026/08/09/rollout-{NATIVE}.jsonl"));
        fs::write(&large_native, vec![b'n'; 128 * 1024]).unwrap();
        export_checkpoint(&export_spec).unwrap();

        let stage_path = export_spec.relay_root.join("checkpoint-stage-test");
        let capture_spec = CheckpointCaptureSpec {
            protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
            session: export_spec.session.clone(),
            target: export_spec.target.clone(),
            bundle: export_spec.bundle.clone(),
            relay_root: export_spec.relay_root.clone(),
            harness_home: export_spec.harness_home.clone(),
            workspace_root: export_spec.workspace_root.clone(),
            repositories: export_spec.repositories.clone(),
            allow_empty_native: false,
            stage_path: stage_path.clone(),
            refresh_existing: false,
        };
        let capture_json = serde_json::to_vec(&capture_spec).unwrap();
        assert!(
            !String::from_utf8_lossy(&capture_json).contains("queued-1"),
            "canonical history crossed the barrier in the capture request"
        );
        let captured = capture_checkpoint(&capture_spec, &SystemGit).unwrap();
        assert!(!captured.reused_native);
        assert!(captured.native_bytes >= 128 * 1024);
        assert!(stage_path.join(CHECKPOINT_STAGE_MANIFEST).is_file());
        let native_stage = stage_path.join("native/00000000");
        let native_stage_modified = fs::metadata(&native_stage).unwrap().modified().unwrap();
        let mut refresh_spec = capture_spec.clone();
        refresh_spec.refresh_existing = true;
        let refreshed = capture_checkpoint(&refresh_spec, &SystemGit).unwrap();
        assert!(refreshed.reused_native);
        assert_eq!(refreshed.native_bytes, captured.native_bytes);
        assert_eq!(
            fs::metadata(&native_stage).unwrap().modified().unwrap(),
            native_stage_modified,
            "barrier catch-up rewrote unchanged native history"
        );

        let staged_path = export_spec.relay_root.join("staged.hel.zip");
        let pack_spec = CheckpointPackSpec {
            protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
            relay_root: export_spec.relay_root.clone(),
            stage_path: stage_path.clone(),
            canonical_session: export_spec.canonical_session.clone(),
            output_path: staged_path.clone(),
        };
        let packed = pack_checkpoint(&pack_spec).unwrap();
        assert_eq!(
            packed.event_frontier,
            export_spec.canonical_session.event_frontier
        );
        assert!(!stage_path.exists(), "consumed stage was not removed");

        let legacy = read_archive_verified(&legacy_path).unwrap();
        let staged = read_archive_verified(&staged_path).unwrap();
        assert_eq!(staged.manifest, legacy.manifest);
        assert_eq!(staged.payloads, legacy.payloads);
    }

    #[test]
    fn prestage_catch_up_recaptures_native_history_that_changed_before_the_barrier() {
        let temp = tempfile::tempdir().unwrap();
        let (export_spec, _) = fixture(temp.path());
        let stage_path = export_spec.relay_root.join("checkpoint-stage-test");
        let mut capture_spec = CheckpointCaptureSpec {
            protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
            session: export_spec.session,
            target: export_spec.target,
            bundle: export_spec.bundle,
            relay_root: export_spec.relay_root,
            harness_home: export_spec.harness_home,
            workspace_root: export_spec.workspace_root,
            repositories: export_spec.repositories,
            allow_empty_native: false,
            stage_path,
            refresh_existing: false,
        };
        let first = capture_checkpoint(&capture_spec, &SystemGit).unwrap();
        assert!(!first.reused_native);
        let rollout = capture_spec
            .harness_home
            .join(format!("sessions/2026/08/09/rollout-{NATIVE}.jsonl"));
        fs::write(&rollout, b"native changed before barrier").unwrap();

        capture_spec.refresh_existing = true;
        let refreshed = capture_checkpoint(&capture_spec, &SystemGit).unwrap();
        assert!(!refreshed.reused_native);
        assert!(refreshed.native_bytes > first.native_bytes);
    }

    #[test]
    #[ignore = "timing measurement against MJ_CHECKPOINT_BENCH_ARCHIVE"]
    fn checkpoint_packaging_throughput() {
        let source = std::env::var_os("MJ_CHECKPOINT_BENCH_ARCHIVE")
            .map(PathBuf::from)
            .expect("set MJ_CHECKPOINT_BENCH_ARCHIVE");
        let read_started = std::time::Instant::now();
        let archive = read_archive_verified(&source).unwrap();
        let canonical_session = archive.canonical_session().unwrap();
        let native_artifacts = archive
            .manifest
            .payloads
            .iter()
            .filter_map(|descriptor| {
                let PayloadRole::NativeArtifact { relative_path } = &descriptor.role else {
                    return None;
                };
                Some(NativeArtifact {
                    relative_path: relative_path.clone(),
                    data: archive.payload(descriptor).unwrap().to_vec(),
                    mode: descriptor.mode,
                })
            })
            .collect::<Vec<_>>();
        let repositories = archive
            .manifest
            .repositories
            .iter()
            .map(|repository| archived_repository_snapshot(&archive, repository).unwrap())
            .collect::<Vec<_>>();
        let payload_bytes = native_artifacts
            .iter()
            .map(|artifact| artifact.data.len() as u64)
            .chain(repositories.iter().flat_map(|repository| {
                [
                    repository.committed_bundle.len() as u64,
                    repository.staged_patch.len() as u64,
                    repository.unstaged_patch.len() as u64,
                    repository.untracked_tar.len() as u64,
                ]
            }))
            .sum::<u64>()
            + serde_json::to_vec(&canonical_session).unwrap().len() as u64;
        let read_elapsed = read_started.elapsed();
        let stage_fixture = tempfile::tempdir().unwrap();
        let relay_root = stage_fixture.path().join("worker");
        let harness_home = stage_fixture.path().join("harness");
        let workspace_root = stage_fixture.path().join("workspace");
        let repository_root = workspace_root.join("app");
        fs::create_dir_all(&relay_root).unwrap();
        fs::create_dir_all(&harness_home).unwrap();
        fs::create_dir_all(&repository_root).unwrap();
        for artifact in &native_artifacts {
            write_private_file(
                &harness_home,
                &artifact.relative_path,
                &artifact.data,
                artifact.mode,
            )
            .unwrap();
        }
        git(&repository_root, &["init"]);
        git(
            &repository_root,
            &["config", "user.email", "hel@example.test"],
        );
        git(&repository_root, &["config", "user.name", "Hel Test"]);
        fs::write(repository_root.join("README.md"), b"benchmark").unwrap();
        git(&repository_root, &["add", "."]);
        git(&repository_root, &["commit", "-m", "benchmark"]);
        let capture_spec = CheckpointCaptureSpec {
            protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
            session: archive.manifest.session.clone(),
            target: archive.manifest.target.clone(),
            bundle: archive.manifest.bundle.clone(),
            relay_root,
            harness_home,
            workspace_root,
            repositories: vec![CheckpointRepositorySpec {
                id: "app".into(),
                relative_destination: "app".into(),
                capture: CheckpointRepositoryCapture::MetadataOnly,
                origin_override: None,
            }],
            allow_empty_native: false,
            stage_path: stage_fixture.path().join("worker/checkpoint-stage"),
            refresh_existing: false,
        };
        let prestage_started = std::time::Instant::now();
        capture_checkpoint(&capture_spec, &SystemGit).unwrap();
        let prestage_elapsed = prestage_started.elapsed();
        let catch_up_started = std::time::Instant::now();
        capture_checkpoint(
            &CheckpointCaptureSpec {
                refresh_existing: true,
                ..capture_spec
            },
            &SystemGit,
        )
        .unwrap();
        let catch_up_elapsed = catch_up_started.elapsed();
        let output_directory = tempfile::tempdir().unwrap();
        let output = output_directory.path().join("benchmark.hel.zip");
        let pack_started = std::time::Instant::now();
        write_archive_hashed(
            &output,
            &ArchiveInput {
                session: archive.manifest.session,
                target: archive.manifest.target,
                bundle: archive.manifest.bundle,
                canonical_session,
                native_artifacts,
                repositories,
            },
        )
        .unwrap();
        eprintln!(
            "checkpoint benchmark: payload_bytes={payload_bytes} read_ms={} prestage_ms={} catch_up_ms={} pack_ms={}",
            read_elapsed.as_millis(),
            prestage_elapsed.as_millis(),
            catch_up_elapsed.as_millis(),
            pack_started.elapsed().as_millis()
        );
    }

    #[test]
    fn checkpoint_collects_the_configured_memory_replica_for_non_claude_harnesses() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _) = fixture(temp.path());
        let memory_root = spec.harness_home.join("projects/replica/memory");
        fs::create_dir_all(&memory_root).unwrap();
        fs::write(memory_root.join("MEMORY.md"), "remember this").unwrap();
        crate::hel_worker_launch::WorkerLaunchConfig {
            session_id: SESSION.into(),
            harness: HarnessKind::Codex,
            bridge_command: "codex-acp".into(),
            bridge_args: Vec::new(),
            environment: Default::default(),
            cwd: spec.workspace_root.join("app"),
            additional_directories: Vec::new(),
            native_session_id: Some(NATIVE.into()),
            project_memory: Some(crate::hel_worker_launch::ProjectMemoryLaunchConfig {
                project_key: "project".into(),
                root: memory_root,
                baseline_root: spec
                    .harness_home
                    .join("projects/replica/.hel-memory-baseline"),
                repository_roots: Default::default(),
                mcp_delivery: crate::hel_worker_launch::ProjectMemoryMcpDelivery::Acp,
            }),
            execution_policy: crate::hel_config::ExecutionPolicy::Unconstrained,
        }
        .write(&spec.relay_root.join("launch.json"))
        .unwrap();

        let artifacts = collect_checkpoint_native_artifacts(
            &spec.session,
            &spec.relay_root,
            &spec.harness_home,
            false,
        )
        .unwrap();
        assert!(artifacts.iter().any(|artifact| {
            artifact.relative_path == Path::new("projects/replica/memory/MEMORY.md")
                && artifact.data == b"remember this"
        }));
    }

    #[test]
    fn checkpoint_collects_memory_from_a_legacy_worker_launch_config() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _) = fixture(temp.path());
        let memory_root = spec.harness_home.join("projects/replica/memory");
        fs::create_dir_all(&memory_root).unwrap();
        fs::write(memory_root.join("MEMORY.md"), "legacy memory").unwrap();
        let legacy_launch = json!({
            "session_id": SESSION,
            "harness": "codex",
            "bridge_command": "codex-acp",
            "bridge_args": [],
            "environment": {},
            "cwd": spec.workspace_root.join("app"),
            "native_session_id": NATIVE,
            "project_memory": {
                "project_key": "project",
                "root": memory_root,
                "baseline_root": spec.harness_home.join("projects/replica/.hel-memory-baseline"),
                "repository_roots": {}
            },
            "force_unrestricted_mode": true
        });
        fs::write(
            spec.relay_root.join("launch.json"),
            serde_json::to_vec_pretty(&legacy_launch).unwrap(),
        )
        .unwrap();

        let artifacts = collect_checkpoint_native_artifacts(
            &spec.session,
            &spec.relay_root,
            &spec.harness_home,
            false,
        )
        .unwrap();

        assert!(artifacts.iter().any(|artifact| {
            artifact.relative_path == Path::new("projects/replica/memory/MEMORY.md")
                && artifact.data == b"legacy memory"
        }));
    }

    #[test]
    fn project_memory_replica_accepts_an_ssh_home_relative_path() {
        let relative = Path::new(".local/share/hel/profiles/session/projects/replica/memory");
        let home = PathBuf::from(std::env::var_os("HOME").expect("test HOME is missing"));

        assert_eq!(
            resolve_home_relative_target_path(relative).unwrap(),
            home.join(relative)
        );
        assert!(resolve_home_relative_target_path(Path::new("../memory")).is_err());
    }

    #[test]
    fn a_checkout_restore_lands_on_the_branch_the_caller_names() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, archive_path) = fixture(temp.path());
        let repository = spec.workspace_root.join("app");
        fs::write(repository.join("feature.txt"), b"session work").unwrap();
        git(&repository, &["add", "."]);
        git(&repository, &["commit", "-m", "session work"]);
        fs::write(repository.join("README.md"), b"edited").unwrap();
        let head = git(&repository, &["rev-parse", "HEAD"]);
        let archived_branch = git(&repository, &["rev-parse", "--abbrev-ref", "HEAD"]);
        export_checkpoint(&spec).unwrap();

        let checkout = temp.path().join("worktrees/session");
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "mj/session",
                &checkout.to_string_lossy(),
                "HEAD~1",
            ],
        );

        let restored = restore_single_repository_onto_branch(
            &archive_path,
            &checkout,
            "mj/session",
            &SystemGit,
        )
        .unwrap();

        assert_eq!(restored.as_deref(), Some(archived_branch.as_str()));
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            git(&checkout, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "mj/session"
        );
        assert_eq!(
            fs::read_to_string(checkout.join("README.md")).unwrap(),
            "edited",
            "the session's uncommitted work comes with it"
        );

        // Restoring onto the archived branch is exactly what the override
        // avoids: that branch is checked out in the user's own working tree.
        let error = restore_single_repository_onto_branch(
            &archive_path,
            &checkout,
            &archived_branch,
            &SystemGit,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("restore committed branch"),
            "{error:#}"
        );
        assert_eq!(
            git(&checkout, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "mj/session",
            "a rejected restore leaves the checkout on its session branch"
        );
        assert_eq!(
            fs::read_to_string(checkout.join("README.md")).unwrap(),
            "edited",
            "a rejected restore leaves the worktree unchanged"
        );
    }

    #[test]
    fn a_workspace_restore_refuses_a_branch_checked_out_in_another_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, archive_path) = fixture(temp.path());
        let repository = spec.workspace_root.join("app");
        fs::write(repository.join("feature.txt"), b"session work").unwrap();
        git(&repository, &["add", "."]);
        git(&repository, &["commit", "-m", "session work"]);
        let archived_branch = git(&repository, &["rev-parse", "--abbrev-ref", "HEAD"]);
        export_checkpoint(&spec).unwrap();

        // The restore destination is a sibling worktree of the same repository,
        // so the archived branch is already owned by the main checkout.
        let restore_root = temp.path().join("restore-workspace");
        let destination = restore_root.join("app");
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "mj/session",
                &destination.to_string_lossy(),
                "HEAD~1",
            ],
        );
        let before = git(&destination, &["rev-parse", "HEAD"]);

        let error = restore_repositories(&archive_path, &restore_root, &SystemGit).unwrap_err();

        assert!(
            format!("{error:#}").contains("is checked out in another worktree"),
            "{error:#}"
        );
        assert_eq!(
            git(&destination, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "mj/session",
            "a rejected restore leaves the destination on its own branch"
        );
        assert_eq!(
            git(&destination, &["rev-parse", "HEAD"]),
            before,
            "a rejected restore leaves the destination checkout unchanged"
        );
        assert_eq!(
            git(&repository, &["rev-parse", "--abbrev-ref", "HEAD"]),
            archived_branch
        );
    }

    fn copy_archive_with_schema(source: &Path, destination: &Path, schema_version: u32) {
        let source = File::open(source).unwrap();
        let mut archive = zip::ZipArchive::new(source).unwrap();
        let mut entries = Vec::new();
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let name = entry.name().to_owned();
            let mode = entry.unix_mode().unwrap_or(0o600);
            let mut body = Vec::new();
            entry.read_to_end(&mut body).unwrap();
            if name == "manifest.json" {
                let mut manifest: Value = serde_json::from_slice(&body).unwrap();
                manifest["schema_version"] = json!(schema_version);
                body = serde_json::to_vec_pretty(&manifest).unwrap();
            }
            entries.push((name, mode, body));
        }
        let output = File::create(destination).unwrap();
        let mut writer = zip::ZipWriter::new(output);
        for (name, mode, body) in entries {
            writer
                .start_file(
                    name,
                    zip::write::SimpleFileOptions::default().unix_permissions(mode),
                )
                .unwrap();
            writer.write_all(&body).unwrap();
        }
        writer.finish().unwrap();
    }

    fn copy_archive_with_canonical_session(
        source: &Path,
        destination: &Path,
        canonical_session: &CanonicalSessionSnapshot,
    ) {
        let source = File::open(source).unwrap();
        let mut archive = zip::ZipArchive::new(source).unwrap();
        let mut entries = Vec::new();
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let name = entry.name().to_owned();
            let mode = entry.unix_mode().unwrap_or(0o600);
            let mut body = Vec::new();
            entry.read_to_end(&mut body).unwrap();
            entries.push((name, mode, body));
        }

        let canonical_body = serde_json::to_vec_pretty(canonical_session).unwrap();
        let canonical_sha256 = format!("{:x}", Sha256::digest(&canonical_body));
        entries
            .iter_mut()
            .find(|(name, _, _)| name == "canonical/session.json")
            .unwrap()
            .2 = canonical_body.clone();
        let manifest_body = &mut entries
            .iter_mut()
            .find(|(name, _, _)| name == "manifest.json")
            .unwrap()
            .2;
        let mut manifest: Value = serde_json::from_slice(manifest_body).unwrap();
        let descriptor = manifest["payloads"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|payload| payload["path"] == "canonical/session.json")
            .unwrap();
        descriptor["size"] = json!(canonical_body.len());
        descriptor["sha256"] = json!(canonical_sha256);
        *manifest_body = serde_json::to_vec_pretty(&manifest).unwrap();

        let output = File::create(destination).unwrap();
        let mut writer = zip::ZipWriter::new(output);
        for (name, mode, body) in entries {
            writer
                .start_file(
                    name,
                    zip::write::SimpleFileOptions::default().unix_permissions(mode),
                )
                .unwrap();
            writer.write_all(&body).unwrap();
        }
        writer.finish().unwrap();
    }

    fn assert_canonical_restore_rejected_before_mutation(
        spec: &CheckpointExportSpec,
        invalid_archive: &Path,
        canonical_session: &CanonicalSessionSnapshot,
        expected_error: &str,
    ) {
        copy_archive_with_canonical_session(&spec.output_path, invalid_archive, canonical_session);
        let readme = spec.workspace_root.join("app/README.md");
        let before = fs::read(&readme).unwrap();
        let relay_root = invalid_archive.with_extension("relay");
        let harness_home = invalid_archive.with_extension("harness");

        let error = restore_checkpoint(
            &CheckpointRestoreSpec {
                archive_path: invalid_archive.to_path_buf(),
                workspace_root: spec.workspace_root.clone(),
                relay_root: relay_root.clone(),
                harness_home: harness_home.clone(),
                restore_repositories: true,
                restore_native: true,
                discard_queued_prompts: false,
                primary_repository_root: None,
            },
            &SystemGit,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains(expected_error), "{error:#}");
        assert_eq!(fs::read(&readme).unwrap(), before);
        assert!(!relay_root.exists());
        assert!(!harness_home.exists());
    }

    struct CopyExecutor {
        source: PathBuf,
        calls: RefCell<usize>,
    }
    impl CommandExecutor for CopyExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            *self.calls.borrow_mut() += 1;
            fs::copy(
                &self.source,
                command.args.last().context("missing destination")?,
            )?;
            Ok(CommandOutput {
                status: 0,
                stdout: vec![],
                stderr: vec![],
            })
        }
    }

    #[test]
    fn export_and_transfer_only_gate_after_local_verification() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, source) = fixture(temp.path());
        let target = export_checkpoint(&spec).unwrap();
        assert_eq!(target.event_frontier, 1);
        assert_eq!(
            target.event_frontier_digest,
            spec.canonical_session.event_frontier_digest
        );
        let destination = temp.path().join("controller/session.hel.zip");
        let locator = &locators()[0];
        let gate = CheckpointTransfer {
            locator,
            session_id: SESSION,
            remote_archive: "/var/lib/hel/workers/source.hel.zip",
            destination: &destination,
            expected_event_frontier: Some(1),
            expected_event_frontier_digest: Some(&spec.canonical_session.event_frontier_digest),
        }
        .execute(&CopyExecutor {
            source,
            calls: RefCell::new(0),
        })
        .unwrap();
        assert!(gate.teardown_allowed());
        assert_eq!(gate.event_frontier(), 1);
        assert_eq!(
            gate.event_frontier_digest(),
            spec.canonical_session.event_frontier_digest
        );
        assert_eq!(
            read_archive_verified(&destination).unwrap().archive_sha256,
            gate.sha256()
        );
    }

    /// The controller streams the export spec to save a round trip to the
    /// target. Both spellings have to produce the same archive.
    #[test]
    fn a_streamed_spec_exports_the_same_archive_as_a_spec_file() {
        let temp = tempfile::tempdir().unwrap();
        let (mut spec, _) = fixture(temp.path());
        let from_file = export_from_spec_file(&spec.output_path.with_extension("spec.json"))
            .err()
            .map(|error| format!("{error:#}"));
        assert!(
            from_file.is_some_and(|error| error.contains("read checkpoint export spec")),
            "a missing spec file must still be reported as a read failure"
        );

        let spec_path = temp.path().join("checkpoint-spec.json");
        spec.write(&spec_path).unwrap();
        let from_file = export_from_spec_file(&spec_path).unwrap();
        let file_archive = fs::read(&spec.output_path).unwrap();

        spec.output_path = temp.path().join("worker/streamed.hel.zip");
        let body = serde_json::to_vec(&spec).unwrap();
        let streamed = export_from_spec_reader(&mut body.as_slice()).unwrap();
        let streamed_archive = fs::read(&spec.output_path).unwrap();

        assert_eq!(streamed.sha256, from_file.sha256);
        assert_eq!(streamed.event_frontier, from_file.event_frontier);
        assert_eq!(
            streamed.event_frontier_digest,
            from_file.event_frontier_digest
        );
        assert_eq!(streamed_archive, file_archive);
        assert_eq!(
            read_archive_verified(&spec.output_path)
                .unwrap()
                .archive_sha256,
            streamed.sha256
        );
    }

    /// A second opinion keeps its whole world — profile, native session and
    /// relay — inside the primary worker root. A v1 checkpoint is single
    /// session, so none of it may end up in the archive.
    #[test]
    fn a_checkpoint_excludes_everything_the_reviewer_owns() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _) = fixture(temp.path());
        let reviewer = spec.relay_root.join("reviewer");
        let reviewer_home = reviewer.join("profile");
        // A reviewer that ran: a staged profile with its own native rollout,
        // its own relay journal, and its own supervisor spec.
        let reviewer_native = reviewer_home.join("sessions/2026/08/09");
        fs::create_dir_all(&reviewer_native).unwrap();
        fs::write(
            reviewer_native.join("rollout-reviewer-native.jsonl"),
            b"reviewer native session",
        )
        .unwrap();
        fs::create_dir_all(reviewer.join("relay-journal")).unwrap();
        fs::write(
            reviewer.join("relay-journal/active.jsonl"),
            b"reviewer relay events",
        )
        .unwrap();
        fs::write(reviewer.join("relay-state.json"), b"reviewer relay state").unwrap();
        fs::write(reviewer.join("acp-supervisor.json"), b"reviewer bridge").unwrap();

        export_checkpoint(&spec).unwrap();

        let archive = read_archive_verified(&spec.output_path).unwrap();
        let native = archive
            .manifest
            .payloads
            .iter()
            .filter_map(|payload| match &payload.role {
                PayloadRole::NativeArtifact { relative_path } => {
                    Some(relative_path.to_string_lossy().into_owned())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            native.iter().all(|path| !path.contains("reviewer")),
            "a reviewer's files must stay out of the checkpoint: {native:?}"
        );
        let bytes = fs::read(&spec.output_path).unwrap();
        for secret in [
            b"reviewer native session".as_slice(),
            b"reviewer relay events".as_slice(),
            b"reviewer relay state".as_slice(),
        ] {
            assert!(
                !bytes.windows(secret.len()).any(|window| window == secret),
                "the archive must not carry the reviewer's content"
            );
        }
        // The primary's own native session is still captured, so this proves
        // exclusion rather than an export that captured nothing.
        assert!(
            !native.is_empty(),
            "the primary's native session must still be exported"
        );
    }

    #[test]
    fn transfer_rejects_a_same_ordinal_frontier_digest_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, source) = fixture(temp.path());
        export_checkpoint(&spec).unwrap();
        let destination = temp.path().join("controller/session.hel.zip");
        let unexpected_digest = "b".repeat(64);

        let error = CheckpointTransfer {
            locator: &locators()[0],
            session_id: SESSION,
            remote_archive: "/var/lib/hel/workers/source.hel.zip",
            destination: &destination,
            expected_event_frontier: Some(1),
            expected_event_frontier_digest: Some(&unexpected_digest),
        }
        .execute(&CopyExecutor {
            source,
            calls: RefCell::new(0),
        })
        .unwrap_err();

        assert!(format!("{error:#}").contains("event frontier digest mismatch"));
        assert!(!destination.exists());
    }

    #[test]
    fn raw_project_export_keeps_git_metadata_without_git_contents() {
        let temp = tempfile::tempdir().unwrap();
        let (mut spec, _) = fixture(temp.path());
        spec.repositories[0].capture = CheckpointRepositoryCapture::MetadataOnly;
        let repository = spec.workspace_root.join("app");
        fs::write(repository.join("README.md"), b"dirty").unwrap();
        fs::write(repository.join("untracked.txt"), b"untracked").unwrap();

        export_checkpoint(&spec).unwrap();
        let archive = read_archive_verified(&spec.output_path).unwrap();
        let repository = &archive.manifest.repositories[0];
        assert_eq!(
            repository.metadata.base_commit,
            repository.metadata.head_commit
        );
        for role in [
            PayloadRole::GitBundle {
                repository_id: "app".into(),
            },
            PayloadRole::GitStagedPatch {
                repository_id: "app".into(),
            },
            PayloadRole::GitUnstagedPatch {
                repository_id: "app".into(),
            },
            PayloadRole::GitUntrackedTar {
                repository_id: "app".into(),
            },
        ] {
            assert!(archive.payload_by_role(&role).unwrap().is_empty());
        }
    }

    #[test]
    fn session_delta_without_origin_refs_repairs_once_then_fails() {
        let temp = tempfile::tempdir().unwrap();
        let (mut spec, _) = fixture(temp.path());
        spec.repositories[0].capture = CheckpointRepositoryCapture::SessionDelta;
        let git = RecordingGit::with_fetch_failure();

        let error = export_checkpoint_with_git(&spec, &git).unwrap_err();

        assert_eq!(git.fetches(), 1);
        let error = format!("{error:#}");
        assert!(
            error.contains("repository 'app' has no origin refs"),
            "{error}"
        );
        assert!(error.contains("refusing to bundle full history"), "{error}");
        assert!(error.contains("repair fetch failed"), "{error}");
    }

    #[test]
    fn invalid_canonical_session_is_rejected_before_repository_repair() {
        let temp = tempfile::tempdir().unwrap();
        let (mut spec, _) = fixture(temp.path());
        spec.repositories[0].capture = CheckpointRepositoryCapture::SessionDelta;
        spec.canonical_session.session.execution =
            CanonicalExecutionState::Running { started_at_ms: 3 };
        let git = RecordingGit::with_fetch_failure();

        let error = export_checkpoint_with_git(&spec, &git).unwrap_err();

        assert!(format!("{error:#}").contains("not idle at the checkpoint barrier"));
        assert_eq!(git.fetches(), 0);
        assert!(!spec.output_path.exists());
    }

    #[test]
    fn session_delta_export_succeeds_when_the_repair_fetch_restores_origin_refs() {
        let temp = tempfile::tempdir().unwrap();
        let (mut spec, _) = fixture(temp.path());
        spec.repositories[0].capture = CheckpointRepositoryCapture::SessionDelta;
        let repository = spec.workspace_root.join("app");
        let origin = temp.path().join("origin.git");
        git(
            &spec.workspace_root,
            &["clone", "-q", "--bare", "app", origin.to_str().unwrap()],
        );
        git(
            &repository,
            &["remote", "set-url", "origin", origin.to_str().unwrap()],
        );
        fs::write(repository.join("later.txt"), b"later").unwrap();
        git(&repository, &["add", "."]);
        git(&repository, &["commit", "-qm", "later"]);
        let git_runner = RecordingGit::forwarding();

        export_checkpoint_with_git(&spec, &git_runner).unwrap();

        assert_eq!(git_runner.fetches(), 1);
        let archive = read_archive_verified(&spec.output_path).unwrap();
        assert_eq!(archive.manifest.repositories[0].metadata.base_commit, "");
        assert!(
            !archive
                .payload_by_role(&PayloadRole::GitBundle {
                    repository_id: "app".into(),
                })
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn raw_project_without_git_metadata_fails_checkpoint_export() {
        let temp = tempfile::tempdir().unwrap();
        let (mut spec, _) = fixture(temp.path());
        spec.repositories[0].capture = CheckpointRepositoryCapture::MetadataOnly;
        fs::remove_dir_all(spec.workspace_root.join("app/.git")).unwrap();

        let error = export_checkpoint(&spec).unwrap_err();

        assert!(format!("{error:#}").contains("repository has no valid Git HEAD"));
    }

    #[test]
    fn export_reports_phase_timings_to_the_controller() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _) = fixture(temp.path());
        let git_runner = RecordingGit::forwarding();

        let exported = export_checkpoint_with_git(&spec, &git_runner).unwrap();

        let timings = exported.timings.expect("export reports its phase timings");
        assert!(
            timings.total_ms
                >= timings
                    .native_ms
                    .max(timings.repositories_ms)
                    .max(timings.archive_ms),
            "{timings:?}"
        );
    }

    #[test]
    fn target_checkpoint_from_a_worker_without_timings_still_decodes() {
        let target = serde_json::from_value::<TargetCheckpoint>(json!({
            "path": "/worker/checkpoint.hel.zip",
            "sha256": "abc",
            "event_frontier": 7,
            "event_frontier_digest": "def"
        }))
        .unwrap();

        assert_eq!(target.timings, None);
        // The field is also absent from the wire form when nothing measured it.
        let encoded = serde_json::to_value(&target).unwrap();
        assert!(encoded.get("timings").is_none(), "{encoded}");
    }

    #[test]
    fn checkpoint_wire_requires_the_new_capture_mode_and_rejects_legacy_fields() {
        let legacy = serde_json::from_value::<CheckpointRepositorySpec>(json!({
            "id": "app",
            "relative_destination": "app",
            "base_commit": "HEAD"
        }));
        assert!(legacy.is_err());

        let repository: CheckpointRepositorySpec = serde_json::from_value(json!({
            "id": "app",
            "relative_destination": "app",
            "capture": { "mode": "delta_from", "base_commit": "HEAD" },
            "origin_override": null
        }))
        .unwrap();
        assert_eq!(
            repository.capture,
            CheckpointRepositoryCapture::DeltaFrom {
                base_commit: "HEAD".into()
            }
        );

        let mixed = serde_json::from_value::<CheckpointRepositorySpec>(json!({
            "id": "app",
            "relative_destination": "app",
            "capture": { "mode": "session_delta" },
            "origin_override": null,
            "session_delta": true
        }));
        assert!(mixed.is_err());

        // The compatibility reset does not reinterpret the old event frontier.
        let target = serde_json::from_value::<TargetCheckpoint>(json!({
            "path": "/worker/checkpoint.hel.zip",
            "sha256": "abc",
            "event_sequence": 7,
            "full_history_fallbacks": ["app"]
        }));
        assert!(target.is_err());

        let restore = json!({
            "archive_path": "/relay/checkpoint.hel.zip",
            "workspace_root": "/workspace",
            "relay_root": "/relay",
            "harness_home": "/harness",
            "restore_repositories": true,
            "restore_native": true,
            "discard_queued_prompts": false
        });
        assert!(serde_json::from_value::<CheckpointRestoreSpec>(restore.clone()).is_ok());
        let mut missing_flag = restore.clone();
        missing_flag
            .as_object_mut()
            .unwrap()
            .remove("discard_queued_prompts");
        assert!(serde_json::from_value::<CheckpointRestoreSpec>(missing_flag).is_err());
        let mut retired_root = restore;
        retired_root
            .as_object_mut()
            .unwrap()
            .insert("worker_root".into(), json!("/legacy"));
        assert!(serde_json::from_value::<CheckpointRestoreSpec>(retired_root).is_err());

        let repository_restore = serde_json::from_value::<RepositoryRestoreSpec>(json!({
            "archive_path": "/relay/checkpoint.hel.zip",
            "workspace_root": "/workspace",
            "legacy": true
        }));
        assert!(repository_restore.is_err());
    }

    #[test]
    fn checkpoint_export_wire_uses_relay_root_and_rejects_retired_worker_root() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _) = fixture(temp.path());
        let mut value = serde_json::to_value(&spec).unwrap();
        assert_eq!(
            value["protocol_version"],
            CHECKPOINT_EXPORT_PROTOCOL_VERSION
        );
        assert!(value.get("relay_root").is_some());
        assert!(value.get("worker_root").is_none());

        let mut unversioned = value.clone();
        unversioned
            .as_object_mut()
            .unwrap()
            .remove("protocol_version");
        assert!(serde_json::from_value::<CheckpointExportSpec>(unversioned).is_err());

        value
            .as_object_mut()
            .unwrap()
            .insert("worker_root".into(), json!("/legacy"));
        assert!(serde_json::from_value::<CheckpointExportSpec>(value).is_err());
    }

    #[test]
    fn checkpoint_export_rejects_an_unsupported_protocol_before_interpreting_paths() {
        let temp = tempfile::tempdir().unwrap();
        let (mut spec, _) = fixture(temp.path());
        spec.protocol_version = CHECKPOINT_EXPORT_PROTOCOL_VERSION + 1;
        spec.relay_root = PathBuf::from("relative/worker");

        let error = export_checkpoint(&spec).unwrap_err();

        assert_eq!(
            error.to_string(),
            "unsupported checkpoint export protocol version 2; worker supports 1"
        );
    }

    #[test]
    fn checkpoint_round_trips_the_latched_materialized_session() {
        let temp = tempfile::tempdir().unwrap();
        let (mut spec, _) = fixture(temp.path());
        spec.canonical_session.event_frontier = 9;
        spec.canonical_session.transcript[0].position = 7;

        let target = export_checkpoint(&spec).unwrap();
        assert_eq!(target.event_frontier, 9);
        assert_eq!(
            read_checkpoint_session(&spec.output_path).unwrap(),
            spec.canonical_session
        );
    }

    fn restore_into(
        temp: &Path,
        spec: &CheckpointExportSpec,
        discard_queued_prompts: bool,
    ) -> PathBuf {
        let relay_root = temp.join(format!("restored-relay-{discard_queued_prompts}"));
        restore_checkpoint(
            &CheckpointRestoreSpec {
                archive_path: spec.output_path.clone(),
                workspace_root: spec.workspace_root.clone(),
                relay_root: relay_root.clone(),
                harness_home: temp.join("restored-harness"),
                restore_repositories: false,
                restore_native: false,
                discard_queued_prompts,
                primary_repository_root: None,
            },
            &SystemGit,
        )
        .unwrap();
        relay_root
    }

    fn restored_seed(relay_root: &Path) -> crate::hel_worker::RestoredRelaySeed {
        serde_json::from_slice(
            &fs::read(crate::hel_worker::restored_relay_seed_path(relay_root)).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn restore_seeds_the_relay_frontier_and_can_discard_queued_prompts() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _) = fixture(temp.path());
        export_checkpoint(&spec).unwrap();

        let kept = restored_seed(&restore_into(temp.path(), &spec, false));
        assert_eq!(kept.event_frontier, spec.canonical_session.event_frontier);
        assert_eq!(
            kept.event_frontier_digest,
            spec.canonical_session.event_frontier_digest
        );
        assert_eq!(kept.queued_prompts, spec.canonical_session.queued_prompts);

        let relay_root = restore_into(temp.path(), &spec, true);
        let discarded = restored_seed(&relay_root);
        assert_eq!(
            discarded.event_frontier,
            spec.canonical_session.event_frontier
        );
        assert!(discarded.queued_prompts.is_empty());
        assert!(!relay_root.join("events.jsonl").exists());
    }

    /// The relay seed must stay proportional to the queue, never to the
    /// conversation: a long session used to write its whole transcript into the
    /// target's relay root for three fields nobody else read.
    #[test]
    fn the_relay_seed_does_not_grow_with_the_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let (mut spec, _) = fixture(temp.path());
        let item = spec.canonical_session.transcript[0].clone();
        spec.canonical_session.transcript = (1..=20_000_u64)
            .map(|position| CanonicalTranscriptItem {
                stable_id: format!("user-{position}"),
                position,
                body: CanonicalTranscriptBody::User {
                    content: vec![json!({"type": "text", "text": "x".repeat(256)})],
                },
                ..item.clone()
            })
            .collect();
        spec.canonical_session.event_frontier = 20_000;
        export_checkpoint(&spec).unwrap();

        let relay_root = restore_into(temp.path(), &spec, false);
        let seed = fs::metadata(crate::hel_worker::restored_relay_seed_path(&relay_root))
            .unwrap()
            .len();

        assert!(
            seed < 4096,
            "the relay seed embedded the transcript: {seed} bytes"
        );
        assert_eq!(
            restored_seed(&relay_root).queued_prompts,
            spec.canonical_session.queued_prompts
        );
    }

    /// A restore seeds a relay that has none of its own state yet. Existing
    /// state means the previous worker was never fully torn down, and the seed
    /// would silently lose to it.
    #[test]
    fn restore_refuses_a_relay_root_that_already_holds_relay_state() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _) = fixture(temp.path());
        export_checkpoint(&spec).unwrap();
        let relay_root = temp.path().join("occupied-relay");
        fs::create_dir_all(&relay_root).unwrap();
        fs::write(
            relay_root.join(crate::hel_worker::RELAY_STATE_FILE),
            b"{\"format_version\":1}",
        )
        .unwrap();

        let error = restore_checkpoint(
            &CheckpointRestoreSpec {
                archive_path: spec.output_path.clone(),
                workspace_root: spec.workspace_root.clone(),
                relay_root: relay_root.clone(),
                harness_home: temp.path().join("restored-harness"),
                restore_repositories: false,
                restore_native: false,
                discard_queued_prompts: false,
                primary_repository_root: None,
            },
            &SystemGit,
        )
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("relay state already present"),
            "{error:#}"
        );
        assert!(!crate::hel_worker::restored_relay_seed_path(&relay_root).exists());
    }

    /// `Path::exists` follows links, so a *dangling* symlink at the seed path
    /// reads as "no file here" and used to send the write to the link target.
    #[cfg(unix)]
    #[test]
    fn restore_refuses_to_seed_the_relay_through_a_dangling_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _) = fixture(temp.path());
        export_checkpoint(&spec).unwrap();
        let relay_root = temp.path().join("symlinked-relay");
        fs::create_dir_all(&relay_root).unwrap();
        let outside = temp.path().join("outside/seed.json");
        fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(
            &outside,
            relay_root.join(crate::hel_worker::RESTORED_RELAY_SEED_FILE),
        )
        .unwrap();

        let error = restore_checkpoint(
            &CheckpointRestoreSpec {
                archive_path: spec.output_path.clone(),
                workspace_root: spec.workspace_root.clone(),
                relay_root,
                harness_home: temp.path().join("restored-harness"),
                restore_repositories: false,
                restore_native: false,
                discard_queued_prompts: false,
                primary_repository_root: None,
            },
            &SystemGit,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("symlink"), "{error:#}");
        assert!(
            !outside.exists(),
            "the relay seed was written through the symlink"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_refuses_a_native_artifact_under_a_symlinked_directory() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _) = fixture(temp.path());
        export_checkpoint(&spec).unwrap();
        let harness_home = temp.path().join("symlinked-harness");
        fs::create_dir_all(&harness_home).unwrap();
        let outside = temp.path().join("outside-sessions");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, harness_home.join("sessions")).unwrap();

        let error = restore_checkpoint(
            &CheckpointRestoreSpec {
                archive_path: spec.output_path.clone(),
                workspace_root: spec.workspace_root.clone(),
                relay_root: temp.path().join("symlinked-native-relay"),
                harness_home,
                restore_repositories: false,
                restore_native: true,
                discard_queued_prompts: false,
                primary_repository_root: None,
            },
            &SystemGit,
        )
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("traverses a symlink"),
            "{error:#}"
        );
        assert!(
            fs::read_dir(&outside).unwrap().next().is_none(),
            "the restore wrote through the symlinked directory"
        );
    }

    #[test]
    fn restore_writes_native_artifacts_privately_under_the_harness_home() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _) = fixture(temp.path());
        export_checkpoint(&spec).unwrap();
        let harness_home = temp.path().join("restored-native-harness");

        restore_checkpoint(
            &CheckpointRestoreSpec {
                archive_path: spec.output_path.clone(),
                workspace_root: spec.workspace_root.clone(),
                relay_root: temp.path().join("restored-native-relay"),
                harness_home: harness_home.clone(),
                restore_repositories: false,
                restore_native: true,
                discard_queued_prompts: false,
                primary_repository_root: None,
            },
            &SystemGit,
        )
        .unwrap();

        let restored = harness_home.join(format!("sessions/2026/08/09/rollout-{NATIVE}.jsonl"));
        assert_eq!(fs::read(&restored).unwrap(), b"native");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&restored).unwrap().permissions().mode() & 0o077,
                0,
                "restored native artifact is group- or world-accessible"
            );
        }
    }

    #[test]
    fn incompatible_schema_is_rejected_before_restore_mutates_target() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _) = fixture(temp.path());
        export_checkpoint(&spec).unwrap();
        let readme = spec.workspace_root.join("app/README.md");
        let before = fs::read(&readme).unwrap();

        for schema_version in [1, crate::hel_archive::ARCHIVE_SCHEMA_VERSION + 1] {
            let incompatible = temp
                .path()
                .join(format!("incompatible-{schema_version}.hel.zip"));
            copy_archive_with_schema(&spec.output_path, &incompatible, schema_version);
            let relay_root = temp.path().join(format!("relay-{schema_version}"));
            let error = restore_checkpoint(
                &CheckpointRestoreSpec {
                    archive_path: incompatible,
                    workspace_root: spec.workspace_root.clone(),
                    relay_root: relay_root.clone(),
                    harness_home: temp.path().join("restored-harness"),
                    restore_repositories: true,
                    restore_native: true,
                    discard_queued_prompts: false,
                    primary_repository_root: None,
                },
                &SystemGit,
            )
            .unwrap_err();

            assert!(
                format!("{error:#}").contains("incompatible Mjolnir archive schema"),
                "{error:#}"
            );
            assert_eq!(fs::read(&readme).unwrap(), before);
            assert!(!relay_root.exists());
        }
    }

    #[test]
    fn non_idle_canonical_session_is_rejected_before_restore_mutates_target() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _) = fixture(temp.path());
        export_checkpoint(&spec).unwrap();
        let mut invalid = spec.canonical_session.clone();
        invalid.session.execution = CanonicalExecutionState::Running { started_at_ms: 3 };

        assert_canonical_restore_rejected_before_mutation(
            &spec,
            &temp.path().join("non-idle.hel.zip"),
            &invalid,
            "not idle at the checkpoint barrier",
        );
    }

    #[test]
    fn invalid_frontier_digest_is_rejected_before_restore_mutates_target() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _) = fixture(temp.path());
        export_checkpoint(&spec).unwrap();
        let mut invalid = spec.canonical_session.clone();
        invalid.event_frontier_digest = "A".repeat(64);

        assert_canonical_restore_rejected_before_mutation(
            &spec,
            &temp.path().join("invalid-digest.hel.zip"),
            &invalid,
            "64 lowercase hexadecimal characters",
        );
    }

    #[test]
    fn unrestorable_queue_is_rejected_before_restore_mutates_target() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, _) = fixture(temp.path());
        export_checkpoint(&spec).unwrap();

        let mut empty = spec.canonical_session.clone();
        empty.queued_prompts[0].content.clear();
        assert_canonical_restore_rejected_before_mutation(
            &spec,
            &temp.path().join("empty-prompt.hel.zip"),
            &empty,
            "has no content",
        );

        let mut malformed = spec.canonical_session.clone();
        malformed.queued_prompts[0].content = vec![json!({"type": "not_an_acp_block"})];
        assert_canonical_restore_rejected_before_mutation(
            &spec,
            &temp.path().join("malformed-prompt.hel.zip"),
            &malformed,
            "has invalid ACP content block 0",
        );
    }

    #[test]
    fn parallel_repository_collection_preserves_manifest_order() {
        let temp = tempfile::tempdir().unwrap();
        let (mut spec, _) = fixture(temp.path());
        git(&spec.workspace_root, &["clone", "-q", "app", "worker"]);
        let worker = spec.workspace_root.join("worker");
        let base = git(&worker, &["rev-parse", "HEAD"]);
        spec.repositories.push(CheckpointRepositorySpec {
            id: "worker".into(),
            relative_destination: "worker".into(),
            capture: CheckpointRepositoryCapture::DeltaFrom { base_commit: base },
            origin_override: None,
        });

        export_checkpoint(&spec).unwrap();
        let verified = read_archive_verified(&spec.output_path).unwrap();
        let repository_ids = verified
            .manifest
            .repositories
            .iter()
            .map(|repository| repository.metadata.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(repository_ids, ["app", "worker"]);
    }

    #[test]
    fn corrupt_transfer_preserves_previous_checkpoint() {
        let temp = tempfile::tempdir().unwrap();
        let corrupt = temp.path().join("bad.zip");
        fs::write(&corrupt, b"bad").unwrap();
        let destination = temp.path().join("session.hel.zip");
        fs::write(&destination, b"previous").unwrap();
        let locator = &locators()[0];
        let result = CheckpointTransfer {
            locator,
            session_id: SESSION,
            remote_archive: "/var/lib/hel/workers/source.hel.zip",
            destination: &destination,
            expected_event_frontier: None,
            expected_event_frontier_digest: None,
        }
        .execute(&CopyExecutor {
            source: corrupt,
            calls: RefCell::new(0),
        });
        assert!(result.is_err());
        assert_eq!(fs::read(destination).unwrap(), b"previous");
    }
}
