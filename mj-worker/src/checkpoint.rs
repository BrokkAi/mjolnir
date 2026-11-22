//! Target checkpoint command entrypoints.
mod codex_goal;
use anyhow::{Context, Result, ensure};
use mj_checkpoint::archive::SystemGit;
use mj_checkpoint::archive::{NativeArtifact, SessionManifest};
use mj_checkpoint::checkpoint::*;
use mj_core::config::HarnessKind;
use std::fs;
use std::path::Path;
struct NativeState;
impl NativeCheckpointState for NativeState {
    fn collect(&self, session: &SessionManifest, home: &Path) -> Result<Vec<NativeArtifact>> {
        if session.harness_kind == HarnessKind::Codex {
            Ok(codex_goal::collect(home, &session.native_session_id)?
                .into_iter()
                .collect())
        } else {
            Ok(Vec::new())
        }
    }
    fn restore_missing(
        &self,
        session: &SessionManifest,
        home: &Path,
        path: &Path,
        data: &[u8],
    ) -> Result<Option<bool>> {
        if !codex_goal::is_artifact(path) {
            return Ok(None);
        }
        ensure!(
            session.harness_kind == HarnessKind::Codex,
            "goal artifact belongs to a non-Codex session"
        );
        Ok(Some(codex_goal::restore_missing(
            home,
            &session.native_session_id,
            path,
            data,
        )?))
    }
    fn restore(
        &self,
        session: &SessionManifest,
        home: &Path,
        path: &Path,
        data: &[u8],
    ) -> Result<bool> {
        if !codex_goal::is_artifact(path) {
            return Ok(false);
        }
        ensure!(
            session.harness_kind == HarnessKind::Codex,
            "goal artifact belongs to a non-Codex session"
        );
        codex_goal::restore(home, &session.native_session_id, path, data)?;
        Ok(true)
    }
}
fn export_checkpoint(spec: &CheckpointExportSpec) -> Result<TargetCheckpoint> {
    export_checkpoint_with_native_state(spec, &SystemGit, &NativeState)
}

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

/// Hidden target CLI entry point: `mj worker suspend-preflight --spec PATH|-`.
pub fn suspend_preflight_from_spec_file(
    path: &Path,
) -> Result<mj_core::suspend_consent::WorkerSuspendPreflightResult> {
    let spec: mj_core::suspend_consent::WorkerSuspendPreflightSpec =
        if path == Path::new(EXPORT_SPEC_STDIN) {
            read_json_from(&mut std::io::stdin().lock(), "suspend preflight spec")?
        } else {
            serde_json::from_slice(&fs::read(path).with_context(|| {
                format!("read suspend preflight spec {}", path.display())
            })?)
            .context("decode suspend preflight spec")?
        };
    run_suspend_preflight(&spec, &SystemGit)
}

pub fn run_suspend_preflight(
    spec: &mj_core::suspend_consent::WorkerSuspendPreflightSpec,
    git: &dyn mj_checkpoint::archive::GitCommandRunner,
) -> Result<mj_core::suspend_consent::WorkerSuspendPreflightResult> {
    let mut result = mj_core::suspend_consent::WorkerSuspendPreflightResult::default();
    let mut git_delta_total: Option<u64> = if spec.dry_run { Some(0) } else { None };
    let mut git_delta_unavailable: Option<String> = None;

    for repo in &spec.repositories {
        let mut input = if repo.raw {
            mj_checkpoint::checkpoint::raw_snapshot_input(
                &repo.id,
                &repo.path,
                None,
                None,
                std::path::PathBuf::new(),
            )
        } else if let Some(exclude_commits) = &repo.exclude_commits {
            mj_checkpoint::checkpoint::cutoff_snapshot_input(
                &repo.id,
                &repo.path,
                Some(&spec.session_id),
                exclude_commits.clone(),
                std::path::PathBuf::new(),
            )?
        } else {
            mj_checkpoint::checkpoint::managed_clone_snapshot_input(
                &repo.id,
                &repo.path,
                Some(&spec.session_id),
                std::path::PathBuf::new(),
                repo.origin_override.clone(),
            )?
        };
        input.keep_dir = repo.keep_dir.clone();

        let inventories =
            mj_checkpoint::snapshot::session_inventory(git, std::slice::from_ref(&input))?;
        let (_, inventory) = inventories
            .into_iter()
            .next()
            .context("session inventory returned no repository")?;

        if let Some(keep_dir) = &input.keep_dir {
            let (entries, bytes) =
                mj_checkpoint::snapshot::scan_keep_dir(&inventory.checkout, keep_dir)?;
            result.keep_dir_entries.extend(entries);
            result.keep_dir_bytes = result.keep_dir_bytes.saturating_add(bytes);
        }

        if spec.dry_run && git_delta_unavailable.is_none() {
            if let Some(op) = &inventory.operation {
                git_delta_total = None;
                git_delta_unavailable =
                    Some(format!("Git size estimate unavailable during {}", op.kind));
            } else {
                match mj_checkpoint::snapshot::estimate_repository_delta(git, &input) {
                    Ok(est) => {
                        if let (Some(acc), Some(bytes)) = (git_delta_total, est.total_bytes) {
                            git_delta_total = Some(acc.saturating_add(bytes));
                        } else {
                            git_delta_total = None;
                        }
                    }
                    Err(err) => {
                        git_delta_total = None;
                        git_delta_unavailable = Some(format!("Git size estimate unavailable: {err}"));
                    }
                }
            }
        }

        result.untracked.extend(inventory.untracked);
        result.nested_repositories.extend(inventory.nested_repositories);
        result.untracked_stashes.extend(
            inventory
                .untracked_stashes
                .into_iter()
                .map(|s| mj_core::suspend_consent::SuspendUntrackedStash {
                    checkout: s.checkout,
                    commit: s.value.commit,
                    message: s.value.message,
                }),
        );
        if let Some(op) = inventory.operation {
            result.operations.push(op);
        }
        result.raw |= inventory.raw;
        result.blockers.extend(
            inventory
                .blockers
                .into_iter()
                .map(|b| mj_core::suspend_consent::SuspendBlocker {
                    kind: format!("{:?}", b.kind),
                    paths: b
                        .paths
                        .into_iter()
                        .map(|p| p.value.display().to_string())
                        .collect(),
                    remedy: b.remedy,
                }),
        );
    }

    if spec.dry_run {
        result.dry_run = Some(mj_core::suspend_consent::DryRunEstimate {
            git_delta_bytes: git_delta_total,
            git_delta_unavailable_reason: git_delta_unavailable,
            session_state_bytes: None,
            keep_dir_bytes: result.keep_dir_bytes,
            transfer_route: None,
        });
    }

    Ok(result)
}
pub fn capture_from_spec_reader(reader: &mut impl std::io::Read) -> Result<CapturedCheckpoint> {
    let spec: CheckpointCaptureSpec = read_json_from(reader, "checkpoint capture spec")?;
    capture_checkpoint_with_native_state(&spec, &SystemGit, &NativeState)
}
pub fn pack_from_spec_reader(reader: &mut impl std::io::Read) -> Result<TargetCheckpoint> {
    let spec: CheckpointPackSpec = read_json_from(reader, "checkpoint pack spec")?;
    pack_checkpoint(&spec)
}
/// The root lock excludes a live worker; the relay's preservation mode never
/// promotes accepted work while this command journals its queue disposition.
pub fn discard_queued_prompts_before_start(
    root: &Path,
    session_id: &str,
    command_id: &str,
) -> Result<()> {
    let _owner = crate::worker_runtime::WorkerRootOwner::acquire(root)?;
    crate::relay::DurableRelay::discard_queued_prompts_before_start(root, session_id, command_id)
}

pub fn restore_checkout_from_spec_file(path: &Path) -> Result<()> {
    use mj_checkpoint::archive::{GitCommand, GitCommandRunner};
    let mut spec: CheckoutRecoverySpec = serde_json::from_slice(&fs::read(path)?)?;
    ensure!(
        spec.version == 1 && spec.restore.restore_repositories && !spec.restore.restore_native,
        "unsupported checkout recovery specification"
    );
    spec.restore.archive_path = resolve_target_path(&spec.restore.archive_path)?;
    spec.restore.workspace_root = resolve_target_path(&spec.restore.workspace_root)?;
    spec.staged_checkout = resolve_target_path(&spec.staged_checkout)?;
    spec.destination_checkout = resolve_target_path(&spec.destination_checkout)?;
    spec.staging_root = resolve_target_path(&spec.staging_root)?;
    let mut publications = vec![CheckoutRecoveryPublication {
        staged_checkout: spec.staged_checkout,
        destination_checkout: spec.destination_checkout,
    }];
    for mut publication in spec.additional_checkouts {
        publication.staged_checkout = resolve_target_path(&publication.staged_checkout)?;
        publication.destination_checkout = resolve_target_path(&publication.destination_checkout)?;
        publications.push(publication);
    }
    let mut missing = false;
    for publication in &publications {
        if !publication.destination_checkout.try_exists()? {
            ensure!(
                publication.staged_checkout.is_dir(),
                "staged checkout is missing"
            );
            missing = true;
        }
    }
    let ready = spec.staging_root.join(".mj-checkouts-restored");
    if missing && !ready.try_exists()? {
        restore_repositories(
            &spec.restore.archive_path,
            &spec.restore.workspace_root,
            &SystemGit,
        )?;
        mj_core::config::atomic_write(&ready, spec.checkpoint_sha256.as_bytes())?;
    }
    if ready.try_exists()? {
        ensure!(
            fs::read_to_string(&ready)? == spec.checkpoint_sha256,
            "staged checkout recovery identity changed"
        );
    }
    let mut published = false;
    for publication in publications {
        if !publication.destination_checkout.try_exists()? {
            if let Some(parent) = publication.destination_checkout.parent() {
                fs::create_dir_all(parent)?;
            }
            published |= rename_directory_without_replacement(
                &publication.staged_checkout,
                &publication.destination_checkout,
            )?;
        }
        if publication.destination_checkout.join(".git").is_file() {
            let output = SystemGit.run(
                &publication.destination_checkout,
                &GitCommand {
                    arguments: vec![
                        "worktree".into(),
                        "repair".into(),
                        publication.destination_checkout.as_os_str().to_owned(),
                    ],
                    ..GitCommand::default()
                },
            )?;
            ensure!(
                output.status == 0,
                "repair published worktree: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    println!("{}", serde_json::json!({"published": published}));
    Ok(())
}

fn rename_directory_without_replacement(source: &Path, destination: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let source = std::ffi::CString::new(source.as_os_str().as_bytes())?;
        let destination = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
        // The kernel makes absence and publication one decision. A child's
        // newly created directory can never be replaced by this archive.
        #[cfg(target_os = "linux")]
        let status = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let status =
            unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        anyhow::bail!("atomic checkout publication is unsupported on this host");
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            if status == 0 {
                return Ok(true);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                return Ok(false);
            }
            Err(error).context("publish recovered checkout without replacing current files")
        }
    }
    #[cfg(not(unix))]
    {
        // Windows refuses replacement of directories, including empty ones.
        match fs::rename(source, destination) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error).context("publish recovered checkout"),
        }
    }
}

pub fn restore_missing_from_spec_file(path: &Path, seed_path: Option<&Path>) -> Result<()> {
    let spec = read_restore_spec(path)?;
    let _owner = crate::worker_runtime::WorkerRootOwner::acquire(&spec.relay_root)?;
    let seed: Option<mj_core::relay::RestoredRelaySeed> = seed_path
        .map(|path| serde_json::from_slice(&fs::read(path)?).map_err(anyhow::Error::from))
        .transpose()?;
    let outcome = restore_missing_parent_checkpoint(&spec, &NativeState, seed.as_ref())?;
    println!("{}", serde_json::to_string(&outcome)?);
    Ok(())
}

pub fn restore_from_spec_file(path: &Path) -> Result<()> {
    let spec = read_restore_spec(path)?;
    let notices = restore_checkpoint_with_notices(&spec, &SystemGit, &NativeState)?;
    if !notices.is_empty() {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({ "notices": notices }))?
        );
    }
    Ok(())
}

fn read_restore_spec(path: &Path) -> Result<CheckpointRestoreSpec> {
    let body = fs::read(path)
        .with_context(|| format!("read checkpoint restore spec {}", path.display()))?;
    let mut spec: CheckpointRestoreSpec = serde_json::from_slice(&body)
        .with_context(|| format!("parse checkpoint restore spec {}", path.display()))?;
    spec.archive_path = resolve_target_path(&spec.archive_path)?;
    spec.workspace_root = resolve_target_path(&spec.workspace_root)?;
    spec.relay_root = resolve_target_path(&spec.relay_root)?;
    spec.harness_home = resolve_target_path(&spec.harness_home)?;
    Ok(spec)
}

#[cfg(all(test, unix))]
mod recovery_tests {
    use super::*;

    #[test]
    fn checkout_publication_never_replaces_a_directory_created_by_another_writer() {
        let temp = tempfile::tempdir().unwrap();
        let staged = temp.path().join("staged");
        let current = temp.path().join("current");
        fs::create_dir(&staged).unwrap();
        fs::write(staged.join("file"), b"archived").unwrap();
        fs::create_dir(&current).unwrap();
        fs::write(current.join("file"), b"new current work").unwrap();
        assert!(!rename_directory_without_replacement(&staged, &current).unwrap());
        assert_eq!(fs::read(current.join("file")).unwrap(), b"new current work");
        assert_eq!(fs::read(staged.join("file")).unwrap(), b"archived");
        let absent = temp.path().join("absent");
        assert!(rename_directory_without_replacement(&staged, &absent).unwrap());
        assert_eq!(fs::read(absent.join("file")).unwrap(), b"archived");
        assert!(!staged.exists());
    }
}
