//! Export a stopped session's Git work from its checkpoint.
//!
//! A stopped session has no checkout to read. The checkpoint holds its work as
//! a delta against `origin` (or, for older archives, as a bundle and patches),
//! so the work is rebuilt in a scratch checkout on the daemon's host with the
//! same seeding and restore code a resume uses, and read from there with the
//! same Git commands a live export runs. The scratch checkout is a directory
//! that disappears with its [`ScratchCheckout`]. Every kind of export of a
//! stopped session starts from that one scratch checkout.

use std::path::PathBuf;

use anyhow::{Context, anyhow};
use mj_checkpoint::archive::{
    ArchiveManifest, RepositoryManifest, RepositoryMetadata, SystemGit, managed_base_is_available,
    session_diff_patch_bytes,
};
use mj_core::state::{
    ManagedCheckoutKind, ManagedWorktree, ManagedWorktreeTarget, SessionRecord, SessionState,
};

use crate::controller::resume::verify_resume_checkpoint;
use crate::controller::worktree::{
    ManagedCloneRestoreOutcome, restore_legacy_worktree_as_managed_clone,
    restore_managed_clone_origin_delta,
};
use crate::targets::CommandExecutor;

/// Why a checkpoint could not be exported.
#[derive(Debug)]
pub enum CheckpointExportError {
    /// The checkpoint cannot produce this export, and the caller can act on
    /// the reason.
    Refused(String),
    /// Reading or restoring the checkpoint failed.
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for CheckpointExportError {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error)
    }
}

/// Whether an export must come from the stored checkpoint because the session
/// has no checkout to read. A stopped session can keep a locator while its
/// cleanup is deferred; its checkpoint is still the only copy of its work.
pub fn export_reads_checkpoint(record: &SessionRecord) -> bool {
    record.target.is_none() || record.state == SessionState::Stopped
}

/// A session's primary repository rebuilt from its checkpoint in a temporary
/// non-bare checkout. The directory is removed when this is dropped.
pub struct ScratchCheckout {
    _directory: tempfile::TempDir,
    /// The top level of the restored checkout.
    pub top_level: PathBuf,
    /// The directory the session worked in: the top level, or the
    /// subdirectory of it the checkpoint recorded.
    pub repository: PathBuf,
    pub metadata: RepositoryMetadata,
    /// The launch base the session record names, when it names one.
    pub recorded_base: Option<String>,
}

/// Verify the session's stored checkpoint and restore its primary repository
/// into a scratch checkout.
///
/// Seeding is the resume's: the session's own source repository when it is on
/// this host and uses the same object format, otherwise a clone of the origin
/// the checkpoint names. A checkpoint that cannot be rebuilt here is refused,
/// and the refusal says how to get the work another way.
pub fn materialize_scratch_checkout(
    record: &SessionRecord,
    executor: &impl CommandExecutor,
) -> Result<ScratchCheckout, CheckpointExportError> {
    let session_id = &record.id;
    let checkpoint = record.checkpoint.as_deref().ok_or_else(|| {
        CheckpointExportError::Refused(format!(
            "session {session_id} has no checkpoint to export from"
        ))
    })?;
    if !checkpoint.archive_path.is_file() {
        return Err(CheckpointExportError::Refused(format!(
            "the checkpoint archive for session {session_id} is missing at {}",
            checkpoint.archive_path.display()
        )));
    }
    let verified = verify_resume_checkpoint(session_id, checkpoint)?;
    let primary = primary_repository(&verified.manifest)?;
    if primary.metadata.plain_directory {
        return Err(CheckpointExportError::Refused(
            "this session's project is a directory without a committed Git checkout, so its checkpoint holds no Git work to export".into(),
        ));
    }
    let origin_delta = primary.metadata.origin_delta.as_deref();
    match origin_delta {
        Some(delta) if delta.history_cutoff => {
            return Err(CheckpointExportError::Refused(
                "this session's checkpoint covers a history-cutoff checkout, which cannot be rebuilt outside its workspace without fetching later history; resume the session and export from there".into(),
            ));
        }
        Some(delta) if delta.origin_url.is_none() => {
            return Err(CheckpointExportError::Refused(
                "this session's checkpoint has no Git origin to rebuild its work from: the raw checkout stayed in place on its original target, so resume the session there and export from it".into(),
            ));
        }
        Some(_) => {}
        None if verified.manifest.repositories.len() != 1 => {
            return Err(CheckpointExportError::Refused(
                "this older checkpoint holds several repositories and cannot be rebuilt for export; resume the session and export from there".into(),
            ));
        }
        None => {}
    }
    // Restore reads one repository, so give it a manifest that names only the
    // primary. The payload list stays whole; the restore looks payloads up by
    // path and role.
    let mut manifest = verified.manifest.clone();
    manifest.repositories = vec![primary.clone()];

    let directory = tempfile::Builder::new()
        .prefix("mj-export-")
        .tempdir()
        .context("create the scratch checkout directory")?;
    let top_level = directory.path().join("checkout");
    let scratch_worktree = scratch_worktree(record, &directory, &top_level, &primary.metadata);
    let restored = if origin_delta.is_some() {
        restore_managed_clone_origin_delta(
            executor,
            &scratch_worktree,
            &verified.archive_path,
            &manifest,
        )
        .and_then(|outcome| match outcome {
            ManagedCloneRestoreOutcome::RestoredLocally(notices) => Ok(notices),
            ManagedCloneRestoreOutcome::SeededForRemoteRestore => Err(anyhow!(
                "the scratch checkout was seeded for a remote restore"
            )),
        })
    } else {
        restore_legacy_worktree_as_managed_clone(
            executor,
            &scratch_worktree,
            &verified.archive_path,
            &manifest,
        )
    };
    restored.map_err(|error| {
        CheckpointExportError::Failed(error.context(
            "rebuild the session's checkout from its checkpoint on this host; if its origin or source repository is not reachable from here, resume the session and export from there",
        ))
    })?;
    let repository = match &primary.metadata.checkout_subdirectory {
        Some(subdirectory) => top_level.join(subdirectory),
        None => top_level.clone(),
    };
    Ok(ScratchCheckout {
        _directory: directory,
        top_level,
        repository,
        metadata: primary.metadata.clone(),
        recorded_base: record
            .managed_worktree
            .as_ref()
            .and_then(|worktree| worktree.base_commit.clone()),
    })
}

/// A patch of the session's tracked work and what it is relative to.
#[derive(Debug, Clone, Default)]
pub struct CheckpointPatch {
    pub bytes: Vec<u8>,
    /// Set when the patch is not relative to the session's launch base.
    pub notice: Option<String>,
}

/// The patch a live export would produce, rebuilt from the stored checkpoint.
///
/// The base is the one the live export compares against: the base the session
/// record names, else the base the checkpoint recorded. A checkpoint that
/// recorded neither is compared with its saved `HEAD`, and the notice says so
/// because commits the session made are then not in the patch.
pub fn checkpoint_patch(
    record: &SessionRecord,
    executor: &impl CommandExecutor,
) -> Result<CheckpointPatch, CheckpointExportError> {
    let scratch = materialize_scratch_checkout(record, executor)?;
    patch_from_scratch(&scratch).map_err(CheckpointExportError::Failed)
}

fn patch_from_scratch(scratch: &ScratchCheckout) -> anyhow::Result<CheckpointPatch> {
    let candidates = [
        scratch.recorded_base.as_deref(),
        Some(scratch.metadata.base_commit.as_str()).filter(|base| !base.is_empty()),
    ];
    let mut unavailable = None;
    for base in candidates.into_iter().flatten() {
        if managed_base_is_available(&SystemGit, &scratch.repository, base)? {
            let bytes = session_diff_patch_bytes(&SystemGit, &scratch.repository, Some(base), None)
                .map_err(|error| anyhow!("{error}"))?;
            return Ok(CheckpointPatch {
                bytes,
                notice: None,
            });
        }
        unavailable.get_or_insert(base);
    }
    let bytes = session_diff_patch_bytes(&SystemGit, &scratch.repository, Some("HEAD"), None)
        .map_err(|error| anyhow!("{error}"))?;
    let reason = match unavailable {
        Some(base) => format!("the session's launch base {base} is not in the restored repository"),
        None => "this checkpoint records no launch base".to_owned(),
    };
    Ok(CheckpointPatch {
        bytes,
        notice: Some(format!(
            "{reason}, so the patch holds the uncommitted tracked changes relative to the saved HEAD; commits the session made are in `mj export --kind bundle`"
        )),
    })
}

fn primary_repository(
    manifest: &ArchiveManifest,
) -> Result<&RepositoryManifest, CheckpointExportError> {
    manifest
        .repositories
        .iter()
        .find(|repository| repository.metadata.id == manifest.bundle.primary_repository)
        .ok_or_else(|| {
            CheckpointExportError::Failed(anyhow!(
                "the checkpoint has no repository {:?}",
                manifest.bundle.primary_repository
            ))
        })
}

/// The checkout record the resume code restores into, placed in the scratch
/// directory on this host. The session's own source repository seeds it only
/// when that repository is on this host; a path on another machine names
/// nothing here, so it is replaced by one that does not exist and the origin
/// seeds the checkout.
fn scratch_worktree(
    record: &SessionRecord,
    directory: &tempfile::TempDir,
    top_level: &std::path::Path,
    metadata: &RepositoryMetadata,
) -> ManagedWorktree {
    let recorded = record
        .managed_worktree
        .as_ref()
        .filter(|worktree| matches!(worktree.target, ManagedWorktreeTarget::Local));
    ManagedWorktree {
        kind: ManagedCheckoutKind::Clone,
        source_project_directory: recorded
            .map(|worktree| worktree.source_project_directory.clone())
            .unwrap_or_default(),
        source_repository: recorded.map_or_else(
            || directory.path().join("no-local-source"),
            |worktree| worktree.source_repository.clone(),
        ),
        worktree_root: top_level.to_path_buf(),
        branch: metadata.branch.clone().unwrap_or_default(),
        target: ManagedWorktreeTarget::Local,
        base_commit: record
            .managed_worktree
            .as_ref()
            .and_then(|worktree| worktree.base_commit.clone()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use mj_checkpoint::archive::{
        GitCollectionSpec, GitHistoryMode, RepositorySnapshot, SystemGit,
    };
    use mj_checkpoint::snapshot::{
        CaptureConsent, CheckoutOwnership, ExclusionSource, RepositoryInput, SessionCaptureConsent,
    };

    use super::*;
    use crate::controller::test_support::{
        checkpoint_archive_input, checkpoint_test_session, test_git, write_checkpoint_archive_input,
    };
    use crate::targets::ProcessExecutor;

    const SESSION: &str = "0123456789abcdef0123456789abcdef";

    struct Fixture {
        temp: tempfile::TempDir,
        origin_url: String,
        /// A clone of the origin at the launch base, standing in for the
        /// user's own checkout a managed clone is made from.
        source: PathBuf,
        /// The session's checkout, with its work in it.
        work: PathBuf,
        base: String,
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }

    /// An origin with one commit, and a session checkout holding what an
    /// agent leaves behind: one commit (when asked), and edits that are
    /// staged, unstaged, binary and not UTF-8.
    fn fixture(commit: bool) -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let origin = temp.path().join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        test_git(&origin, &["init", "--bare", "--initial-branch=main"]);
        let origin_url = origin.to_string_lossy().into_owned();
        let source = temp.path().join("source");
        test_git(
            temp.path(),
            &["clone", &origin_url, source.to_str().unwrap()],
        );
        test_git(&source, &["config", "user.name", "Test"]);
        test_git(&source, &["config", "user.email", "test@example.com"]);
        write_file(&source.join("latin.txt"), b"caf\xe9\nkeep\n");
        write_file(&source.join("both.txt"), b"one\ntwo\nthree\n");
        write_file(&source.join("blob.bin"), &[0, 1, 2, 3, 255]);
        test_git(&source, &["add", "."]);
        test_git(&source, &["commit", "-m", "base"]);
        test_git(&source, &["push", "-u", "origin", "main"]);
        let base = test_git(&source, &["rev-parse", "HEAD"]);

        let work = temp.path().join("work");
        test_git(temp.path(), &["clone", &origin_url, work.to_str().unwrap()]);
        test_git(&work, &["config", "user.name", "Test"]);
        test_git(&work, &["config", "user.email", "test@example.com"]);
        test_git(&work, &["checkout", "-b", "feature/export"]);
        if commit {
            write_file(&work.join("committed.txt"), b"committed\n");
            test_git(&work, &["add", "committed.txt"]);
            test_git(&work, &["commit", "-m", "session commit"]);
        }
        write_file(&work.join("latin.txt"), b"caf\xe9 \xff au lait\nkeep\n");
        write_file(&work.join("blob.bin"), &[0, 9, 8, 7, 254, 253]);
        write_file(&work.join("both.txt"), b"ONE\ntwo\nthree\n");
        test_git(&work, &["add", "both.txt"]);
        write_file(&work.join("both.txt"), b"ONE\ntwo\nTHREE\n");
        Fixture {
            temp,
            origin_url,
            source,
            work,
            base,
        }
    }

    /// Write the checkpoint of `repository` and a stopped session that names it.
    fn stopped_session(fixture: &Fixture, repository: RepositorySnapshot) -> SessionRecord {
        let directory = fixture.temp.path().join("checkpoints");
        std::fs::create_dir_all(&directory).unwrap();
        let input = checkpoint_archive_input(SESSION, 0, vec![repository], Vec::new());
        let mut record = checkpoint_test_session(SESSION);
        record.state = SessionState::Stopped;
        record.checkpoint = Some(Box::new(write_checkpoint_archive_input(
            &directory, SESSION, &input,
        )));
        record
    }

    fn origin_delta_repository(fixture: &Fixture, base_commit: &str) -> RepositorySnapshot {
        let mut snapshot = mj_checkpoint::snapshot::capture_session_snapshot(
            &SystemGit,
            &[RepositoryInput {
                id: "project".into(),
                checkout: fixture.work.clone(),
                ownership: CheckoutOwnership::Managed,
                origin_override: Some(fixture.origin_url.clone()),
                pack_path: fixture.temp.path().join("origin.pack"),
                exclusions: ExclusionSource::Origin,
                keep_dir: None,
            }],
            &SessionCaptureConsent {
                repositories: [("project".to_owned(), CaptureConsent::default())]
                    .into_iter()
                    .collect(),
            },
        )
        .unwrap()
        .repositories
        .remove("project")
        .unwrap();
        RepositorySnapshot {
            metadata: RepositoryMetadata {
                id: "project".into(),
                relative_destination: PathBuf::from("project"),
                checkout_subdirectory: None,
                origin: fixture.origin_url.clone(),
                push_urls: Vec::new(),
                remote_workspace: false,
                plain_directory: false,
                base_commit: base_commit.to_owned(),
                head_commit: snapshot.head.commit().to_owned(),
                branch: Some("feature/export".into()),
                review_refs_metadata_only: false,
                review_baseline_ref: None,
                saved_refs: Default::default(),
                stash_stack: Vec::new(),
                git_operation: None,
                origin_delta: Some(Box::new(snapshot.metadata)),
            },
            origin_delta_pack: snapshot.pack.take(),
            committed_bundle: Vec::new(),
            staged_patch: Vec::new(),
            unstaged_patch: Vec::new(),
            untracked_tar: Vec::new(),
            operation_payloads: None,
            linked_worktrees: Vec::new(),
        }
    }

    /// What a live export of the session's checkout would answer.
    fn live_patch(fixture: &Fixture, base: &str) -> Vec<u8> {
        session_diff_patch_bytes(&SystemGit, &fixture.work, Some(base), None).unwrap()
    }

    /// Apply `patch` to a fresh checkout of the base and compare the files the
    /// session changed with the session's own.
    fn assert_patch_reproduces_the_work(fixture: &Fixture, patch: &[u8], files: &[&str]) {
        let fresh = fixture.temp.path().join("fresh");
        test_git(
            fixture.temp.path(),
            &["clone", &fixture.origin_url, fresh.to_str().unwrap()],
        );
        test_git(&fresh, &["checkout", "-q", &fixture.base]);
        let patch_file = fixture.temp.path().join("export.patch");
        std::fs::write(&patch_file, patch).unwrap();
        test_git(&fresh, &["apply", "--binary", patch_file.to_str().unwrap()]);
        for file in files {
            assert_eq!(
                std::fs::read(fresh.join(file)).unwrap(),
                std::fs::read(fixture.work.join(file)).unwrap(),
                "{file}"
            );
        }
        std::fs::remove_dir_all(&fresh).unwrap();
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1292: a local managed checkout runs mkdir and requires POSIX paths"
    )]
    fn a_stopped_session_with_edits_and_no_commits_exports_a_patch_that_applies_to_its_base() {
        let fixture = fixture(false);
        let mut record = stopped_session(&fixture, origin_delta_repository(&fixture, ""));
        // The record names the launch base, as every managed checkout's does.
        let mut worktree = ManagedWorktree {
            kind: ManagedCheckoutKind::Clone,
            source_project_directory: fixture.source.clone(),
            source_repository: fixture.source.clone(),
            worktree_root: fixture.work.clone(),
            branch: "feature/export".into(),
            target: ManagedWorktreeTarget::Local,
            base_commit: Some(fixture.base.clone()),
        };
        record.managed_worktree = Some(Box::new(worktree.clone()));

        let patch = checkpoint_patch(&record, &ProcessExecutor).unwrap();
        assert_eq!(patch.notice, None);
        assert_eq!(patch.bytes, live_patch(&fixture, &fixture.base));
        assert!(String::from_utf8(patch.bytes.clone()).is_err());
        assert!(String::from_utf8_lossy(&patch.bytes).contains("GIT binary patch"));
        assert_patch_reproduces_the_work(
            &fixture,
            &patch.bytes,
            &["latin.txt", "both.txt", "blob.bin"],
        );

        // A source repository on another machine is not seeded from here; the
        // origin seeds the checkout and the export is the same.
        worktree.target = ManagedWorktreeTarget::Ssh {
            destination: "remote.example.test".into(),
            ssh_args: Vec::new(),
        };
        record.managed_worktree = Some(Box::new(worktree));
        let from_origin = checkpoint_patch(&record, &ProcessExecutor).unwrap();
        assert_eq!(from_origin.bytes, patch.bytes);
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1292: a local managed checkout runs mkdir and requires POSIX paths"
    )]
    fn a_stopped_session_exports_its_commits_and_edits_from_the_base_the_checkpoint_recorded() {
        let fixture = fixture(true);
        let record = stopped_session(
            &fixture,
            origin_delta_repository(&fixture, &fixture.base.clone()),
        );

        let patch = checkpoint_patch(&record, &ProcessExecutor).unwrap();
        assert_eq!(patch.notice, None);
        assert_eq!(patch.bytes, live_patch(&fixture, &fixture.base));
        assert!(String::from_utf8_lossy(&patch.bytes).contains("committed.txt"));
        assert_patch_reproduces_the_work(
            &fixture,
            &patch.bytes,
            &["committed.txt", "latin.txt", "both.txt", "blob.bin"],
        );
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1292: a local managed checkout runs mkdir and requires POSIX paths"
    )]
    fn a_checkpoint_with_no_recorded_base_exports_edits_from_its_saved_head_and_says_so() {
        let fixture = fixture(true);
        // An archive written before the base was recorded, for a session whose
        // record names none either.
        let record = stopped_session(&fixture, origin_delta_repository(&fixture, ""));

        let patch = checkpoint_patch(&record, &ProcessExecutor).unwrap();
        assert_eq!(patch.bytes, live_patch(&fixture, "HEAD"));
        let patch_text = String::from_utf8_lossy(&patch.bytes);
        assert!(patch_text.contains("latin.txt"));
        assert!(!patch_text.contains("committed.txt"));
        let notice = patch.notice.expect("a patch from the saved HEAD says so");
        assert!(notice.contains("records no launch base"), "{notice}");
        assert!(notice.contains("saved HEAD"), "{notice}");
        assert!(notice.contains("mj export --kind bundle"), "{notice}");
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1292: a local managed checkout runs mkdir and requires POSIX paths"
    )]
    fn a_stopped_session_with_a_legacy_checkpoint_exports_its_patch() {
        let fixture = fixture(true);
        let mut repository = mj_checkpoint::archive::collect_git_snapshot(
            &SystemGit,
            &fixture.work,
            &GitCollectionSpec {
                id: "project".into(),
                relative_destination: "project".into(),
                history: GitHistoryMode::DeltaFrom(fixture.base.clone()),
                origin_override: None,
            },
        )
        .unwrap();
        repository.metadata.base_commit = fixture.base.clone();
        let record = stopped_session(&fixture, repository);

        let patch = checkpoint_patch(&record, &ProcessExecutor).unwrap();
        assert_eq!(patch.notice, None);
        assert_eq!(patch.bytes, live_patch(&fixture, &fixture.base));
        assert_patch_reproduces_the_work(
            &fixture,
            &patch.bytes,
            &["committed.txt", "latin.txt", "both.txt", "blob.bin"],
        );
    }

    #[test]
    fn a_checkpoint_that_cannot_be_rebuilt_is_refused_with_a_way_forward() {
        let fixture = fixture(false);
        let mut record = stopped_session(&fixture, origin_delta_repository(&fixture, ""));
        let refusal = |record: &SessionRecord| match checkpoint_patch(record, &ProcessExecutor) {
            Err(CheckpointExportError::Refused(reason)) => reason,
            other => panic!("expected a refusal, got {other:?}"),
        };

        // A raw checkout with no origin stayed in place on its target.
        let mut originless = origin_delta_repository(&fixture, "");
        originless.metadata.origin.clear();
        originless.origin_delta_pack = None;
        originless
            .metadata
            .origin_delta
            .as_mut()
            .unwrap()
            .origin_url = None;
        let originless_record = stopped_session(&fixture, originless);
        let reason = refusal(&originless_record);
        assert!(reason.contains("stayed in place"), "{reason}");
        assert!(!reason.contains("bundle"), "{reason}");

        // A suspend without a checkpoint leaves nothing to read.
        record.checkpoint = None;
        let reason = refusal(&record);
        assert!(reason.contains("no checkpoint"), "{reason}");
    }
}
