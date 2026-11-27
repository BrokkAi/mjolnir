//! Durable, replayable promotion of a privately staged harness profile.
use anyhow::{Context, Result, ensure};
use mj_core::worker_launch::{
    PENDING_RELOAD_FILE, PendingInPlaceReload, RELOAD_CANDIDATE_MANIFEST_FILE,
    RELOAD_CANDIDATES_DIR, ReloadCandidateManifest, validate_candidate_id,
};
use std::path::{Component, Path};

fn relative(path: &str) -> Result<&Path> {
    let path = Path::new(path);
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "unsafe overlay path"
    );
    Ok(path)
}

pub fn validate_candidate(
    root: &Path,
    candidate_id: &str,
    operation_id: &str,
    expected_store_id: &str,
) -> Result<ReloadCandidateManifest> {
    validate_candidate_id(candidate_id)?;
    let candidate = root.join(RELOAD_CANDIDATES_DIR).join(candidate_id);
    mj_core::confined_path::reject_symlinks(
        root,
        &Path::new(RELOAD_CANDIDATES_DIR).join(candidate_id),
        false,
    )?;
    ensure!(
        std::fs::symlink_metadata(&candidate)?.is_dir(),
        "candidate is not a directory"
    );
    ensure!(
        std::fs::symlink_metadata(candidate.join(RELOAD_CANDIDATE_MANIFEST_FILE))?.is_file(),
        "candidate manifest is not a regular file"
    );
    let manifest: ReloadCandidateManifest = serde_json::from_slice(&std::fs::read(
        candidate.join(RELOAD_CANDIDATE_MANIFEST_FILE),
    )?)?;
    ensure!(
        manifest.candidate_id == candidate_id
            && manifest.operation_id == operation_id
            && manifest.expected_store_id == expected_store_id,
        "reload candidate identity mismatch"
    );
    for name in ["launch.json", "ownership.json"] {
        ensure!(
            std::fs::symlink_metadata(candidate.join(name))?.is_file(),
            "candidate {name} is not a regular file"
        );
    }
    ensure!(
        std::fs::symlink_metadata(candidate.join("profile"))?.is_dir(),
        "candidate profile is not a directory"
    );
    for path in manifest.clear_paths.iter().chain(&manifest.protected_paths) {
        relative(path)?;
    }
    if let Some(login) = &manifest.shared_login {
        relative(&login.relative_marker)?;
    }
    let home = Path::new(&manifest.target_profile_home);
    ensure!(
        !home.as_os_str().is_empty()
            && home.components().all(|part| matches!(
                part,
                Component::Normal(_) | Component::RootDir | Component::Prefix(_)
            )),
        "unsafe reload profile home"
    );
    if home.is_relative() {
        relative(&manifest.target_profile_home)?;
    }
    Ok(manifest)
}

pub(super) fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_private_write_nosync_dir(path, bytes)?;
    mj_core::config::sync_directory(path.parent().context("atomic write parent")?)
}

fn atomic_private_write_nosync_dir(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("atomic write parent")?;
    let temporary = parent.join(mj_core::relay::new_command_id("reload-write")?);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn sync_tree(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            sync_tree(&entry?.path())?;
        }
        mj_core::config::sync_directory(path)?;
    } else if metadata.is_file() {
        #[cfg(unix)]
        std::fs::File::open(path)?.sync_all()?;
    }
    Ok(())
}

pub fn commit_pending_reload(root: &Path, pending: &PendingInPlaceReload) -> Result<()> {
    validate_candidate_id(&pending.candidate_id)?;
    if let Some(name) = &pending.git_setup_name {
        validate_candidate_id(name)?;
    }
    sync_tree(&root.join(RELOAD_CANDIDATES_DIR).join(&pending.candidate_id))?;
    mj_core::config::sync_directory(&root.join(RELOAD_CANDIDATES_DIR))?;
    if let Some(name) = &pending.git_setup_name {
        sync_tree(&root.join(name))?;
    }
    mj_core::config::sync_directory(root)?;
    // The receipt can be refreshed until the WAL freezes this retirement.
    // Replay only consumes that receipt, never the partially overlaid home.
    capture_retired_login(root, pending)?;
    atomic_private_write(
        &root.join(PENDING_RELOAD_FILE),
        &serde_json::to_vec(pending)?,
    )
}

pub fn sync_pending_reload_dir(root: &Path) -> Result<()> {
    mj_core::config::sync_directory(root)
}

fn remove(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn directory(path: &Path) -> Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        ensure!(
            metadata.is_dir(),
            "overlay directory is a symlink or non-directory: {}",
            path.display()
        );
    }
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(super) fn copy_overlay(source: &Path, destination: &Path) -> Result<()> {
    directory(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        let metadata = entry.file_type()?;
        if metadata.is_dir() {
            copy_overlay(&entry.path(), &target)?;
        } else {
            ensure!(
                metadata.is_file(),
                "unexpected symlink or special file in reload overlay"
            );
            remove(&target)?;
            std::fs::copy(entry.path(), &target)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = entry.metadata()?.permissions().mode() & 0o700;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
                std::fs::File::open(&target)?.sync_all()?;
            }
        }
    }
    mj_core::config::sync_directory(destination)?;
    Ok(())
}

pub fn apply_committed_reload(
    root: &Path,
    pending: &PendingInPlaceReload,
) -> Result<ReloadCandidateManifest> {
    let manifest = validate_candidate(
        root,
        &pending.candidate_id,
        &pending.operation_id,
        &pending.expected_store_id,
    )?;
    let retired = read_retired_login_record(root, pending)?;
    let candidate = root.join(RELOAD_CANDIDATES_DIR).join(&pending.candidate_id);
    let home = Path::new(&manifest.target_profile_home);
    let home = if home.is_absolute() {
        home.to_owned()
    } else {
        std::env::current_dir()?.join(home)
    };
    let home = home.as_path();
    if std::fs::symlink_metadata(home).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        std::fs::remove_file(home).with_context(|| {
            format!(
                "unlink earlier session profile home link {}",
                home.display()
            )
        })?;
    }
    directory(home)?;
    for clear in &manifest.clear_paths {
        ensure!(
            !root
                .join(RETIRED_LOGINS)
                .starts_with(home.join(relative(clear)?)),
            "reload would clear retired logins"
        );
        let relative = relative(clear)?;
        if let Some(parent) = relative.parent() {
            mj_core::confined_path::reject_symlinks(home, parent, true)?;
        }
        let path = home.join(relative);
        let protected = manifest
            .protected_paths
            .iter()
            .filter_map(|p| {
                Path::new(p)
                    .strip_prefix(clear)
                    .ok()
                    .and_then(|tail| tail.components().next())
                    .map(|c| c.as_os_str().to_owned())
            })
            .collect::<Vec<_>>();
        if protected.is_empty()
            || std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink())
        {
            remove(&path)?;
            if let Some(parent) = path.parent()
                && parent != home
                && parent.is_dir()
            {
                mj_core::config::sync_directory(parent)?;
            }
        } else if path.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                let entry = entry?;
                if !protected.contains(&entry.file_name()) {
                    remove(&entry.path())?;
                }
            }
            mj_core::config::sync_directory(&path)?;
        }
    }
    copy_overlay(&candidate.join("profile"), home)?;
    #[cfg(unix)]
    let mut launch =
        mj_core::worker_launch::WorkerLaunchConfig::read(&candidate.join("launch.json"))?;
    #[cfg(unix)]
    super::resolve_relative_harness_home(&mut launch, &std::env::current_dir()?);
    #[cfg(unix)]
    if let Some(memory) = &launch.project_memory {
        super::project_memory::ensure_project_memory_replica(memory)?;
    }
    if let Some(login) = &manifest.shared_login {
        let relative = relative(&login.relative_marker)?;
        if let Some(parent) = relative.parent() {
            mj_core::confined_path::reject_symlinks(home, parent, true)?;
            directory(&home.join(parent))?;
        }
        let marker = home.join(relative);
        remove(&marker)?;
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&login.source_path, &marker)?;
            mj_core::config::sync_directory(marker.parent().context("shared login parent")?)?;
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("shared reload login requires Unix symlinks");
        }
    }
    if let Some(retired) = &retired {
        reconcile_retired_login(&candidate, home, retired, &manifest)?;
    }
    for name in ["ownership.json", "launch.json"] {
        atomic_private_write(&root.join(name), &std::fs::read(candidate.join(name))?)?;
    }
    if let Some(name) = &pending.git_setup_name {
        validate_candidate_id(name)?;
        ensure!(
            std::fs::symlink_metadata(root.join(name))?.is_dir(),
            "missing committed Git setup"
        );
        #[cfg(unix)]
        super::unix::git_env::publish_git_setup_name(root, name)?;
    }
    if manifest.clear_github_token {
        mj_core::credentials::remove_github_token(&root.join("github-token"))?;
    }
    mj_core::config::sync_directory(home)?;
    if let Some(parent) = home.parent() {
        mj_core::config::sync_directory(parent)?;
    }
    mj_core::config::sync_directory(root)?;
    std::fs::remove_file(root.join(PENDING_RELOAD_FILE))?;
    mj_core::config::sync_directory(root)?;
    std::fs::remove_dir_all(candidate)?;
    Ok(manifest)
}

pub fn roll_forward_pending_reload_if_any(root: &Path) -> Result<Option<String>> {
    let operation = match std::fs::read(root.join(PENDING_RELOAD_FILE)) {
        Ok(bytes) => {
            let pending: PendingInPlaceReload = serde_json::from_slice(&bytes)?;
            apply_committed_reload(root, &pending)?;
            Some(pending.operation_id)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let candidates = root.join(RELOAD_CANDIDATES_DIR);
    match std::fs::symlink_metadata(&candidates) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir(),
                "reload candidates root is not a directory"
            );
            for entry in std::fs::read_dir(candidates)? {
                remove(&entry?.path())?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    Ok(operation)
}

const RETIRED_LOGINS: &str = "retired-logins";

#[derive(serde::Serialize, serde::Deserialize)]
struct RetiredLoginRecord {
    operation_id: String,
    store_id: String,
    // Tolerate empty receipts written before capture moved exclusively pre-WAL.
    login: Option<mj_core::credentials::RetiredLoginSnapshot>,
}

fn retired_path(root: &Path, id: &str) -> Result<std::path::PathBuf> {
    validate_candidate_id(id)?;
    let relative = Path::new(RETIRED_LOGINS).join(format!("{id}.json"));
    mj_core::confined_path::reject_symlinks(root, &relative, true)?;
    Ok(root.join(relative))
}

fn read_retired_login_record(
    root: &Path,
    pending: &PendingInPlaceReload,
) -> Result<Option<RetiredLoginRecord>> {
    match std::fs::read(retired_path(root, &pending.operation_id)?) {
        Ok(bytes) => {
            let record: RetiredLoginRecord = serde_json::from_slice(&bytes)?;
            ensure!(
                record.operation_id == pending.operation_id
                    && record.store_id == pending.expected_store_id,
                "retired login receipt identity mismatch"
            );
            Ok(Some(record))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn capture_retired_login(root: &Path, pending: &PendingInPlaceReload) -> Result<()> {
    use mj_core::worker_launch::{WorkerLaunchConfig, WorkerOwnership};
    let path = retired_path(root, &pending.operation_id)?;
    let login = match std::fs::read(root.join("ownership.json")) {
        Ok(bytes) => {
            let ownership: WorkerOwnership = serde_json::from_slice(&bytes)?;
            let launch = WorkerLaunchConfig::read(&root.join("launch.json"))?;
            let copied =
                mj_core::credentials::read_copied_login(launch.harness, &launch.harness_home)?;
            let copied = match copied {
                Some(bytes) => {
                    match mj_core::credentials::harvestable_login_bytes(launch.harness, &bytes) {
                        Ok(true) => Some(bytes),
                        Ok(false) => None,
                        Err(error) => {
                            tracing::warn!(harness = ?launch.harness, %error, "ignoring corrupt retired source login");
                            None
                        }
                    }
                }
                None => None,
            };
            copied.map(|bytes| mj_core::credentials::RetiredLoginSnapshot {
                id: pending.operation_id.clone(),
                profile_id: ownership.profile_id,
                harness: launch.harness,
                store_id: pending.expected_store_id.clone(),
                login_relative_path: mj_core::credentials::copied_login_relative_path(
                    launch.harness,
                )
                .expect("harvested login has a descriptor")
                .to_owned(),
                bytes,
            })
        }
        // Legacy workers may not have an ownership marker.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(login) = login {
        let record = RetiredLoginRecord {
            operation_id: pending.operation_id.clone(),
            store_id: pending.expected_store_id.clone(),
            login: Some(login),
        };
        directory(&root.join(RETIRED_LOGINS))?;
        atomic_private_write(&path, &serde_json::to_vec(&record)?)?;
        mj_core::config::sync_directory(root)?;
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => mj_core::config::sync_directory(&root.join(RETIRED_LOGINS))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Connection-only reads; a WAL in flight cannot be acknowledged before replay.
pub fn read_retired_logins(root: &Path) -> Result<Vec<mj_core::credentials::RetiredLoginSnapshot>> {
    Ok(read_retired_logins_page(root, None, usize::MAX)?.0)
}

pub fn retired_logins_deferred(root: &Path) -> Result<bool> {
    Ok(root.join(PENDING_RELOAD_FILE).try_exists()?)
}

pub fn read_retired_logins_page(
    root: &Path,
    after: Option<&str>,
    max_raw_bytes: usize,
) -> Result<(Vec<mj_core::credentials::RetiredLoginSnapshot>, bool)> {
    if root.join(PENDING_RELOAD_FILE).try_exists()? {
        return Ok((Vec::new(), false));
    }
    mj_core::confined_path::reject_symlinks(root, Path::new(RETIRED_LOGINS), true)?;
    let entries = match std::fs::read_dir(root.join(RETIRED_LOGINS)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), false));
        }
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.path().extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        ensure!(
            entry.file_type()?.is_file(),
            "retired login is not a regular file"
        );
        paths.push(entry.path());
    }
    paths.sort_by(|a, b| a.file_stem().cmp(&b.file_stem()));
    let mut logins = Vec::new();
    let mut raw_bytes = 0_usize;
    for path in paths {
        let id = path
            .file_stem()
            .and_then(|id| id.to_str())
            .context("retired login filename")?;
        if after.is_some_and(|after| id <= after) {
            continue;
        }
        let record: RetiredLoginRecord = serde_json::from_slice(&std::fs::read(&path)?)?;
        ensure!(
            path == retired_path(root, &record.operation_id)?,
            "retired login filename mismatch"
        );
        if let Some(login) = record.login {
            ensure!(
                login.id == record.operation_id,
                "retired login cursor identity mismatch"
            );
            // Charge raw credential bytes AND JSON metadata so many small
            // grants cannot overflow the frame through metadata expansion.
            let charge = serde_json::to_vec(&login)?.len() - login.bytes.len().div_ceil(3) * 4
                + login.bytes.len();
            if !logins.is_empty() && charge > max_raw_bytes.saturating_sub(raw_bytes) {
                return Ok((logins, true));
            }
            raw_bytes = raw_bytes.saturating_add(charge);
            logins.push(login);
        }
    }
    Ok((logins, false))
}

pub fn ack_retired_login(root: &Path, id: &str, expected_fingerprint: &str) -> Result<()> {
    ensure!(
        !root.join(PENDING_RELOAD_FILE).try_exists()?,
        "reload is still committing"
    );
    let path = retired_path(root, id)?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let record: RetiredLoginRecord = serde_json::from_slice(&bytes)?;
    if let Some(login) = record.login {
        ensure!(
            mj_core::credentials::credential_fingerprint(&login.bytes) == expected_fingerprint,
            "retired login receipt changed before acknowledgement"
        );
    }
    match std::fs::remove_file(path) {
        Ok(()) => mj_core::config::sync_directory(&root.join(RETIRED_LOGINS)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn reconcile_retired_login(
    candidate: &Path,
    home: &Path,
    record: &RetiredLoginRecord,
    manifest: &ReloadCandidateManifest,
) -> Result<()> {
    use mj_core::{
        credentials::{CredentialSnapshot, SyncAction},
        worker_launch::WorkerOwnership,
    };
    if manifest.shared_login.is_some() {
        return Ok(());
    }
    let Some(login) = &record.login else {
        return Ok(());
    };
    let ownership: WorkerOwnership =
        serde_json::from_slice(&std::fs::read(candidate.join("ownership.json"))?)?;
    let launch = mj_core::worker_launch::WorkerLaunchConfig::read(&candidate.join("launch.json"))?;
    if ownership.profile_id != login.profile_id || launch.harness != login.harness {
        return Ok(());
    }
    let mut inspected = home.to_owned();
    let mut absent = false;
    for component in std::iter::once(None).chain(login.login_relative_path.components().map(Some)) {
        if let Some(component) = component {
            inspected.push(component);
        }
        match std::fs::symlink_metadata(&inspected) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(()),
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                absent = true;
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let copy = mj_core::credentials::credential_copy_for_harness(login.harness, &login.bytes);
    let replace = if absent {
        true
    } else {
        let Some(bytes) = mj_core::credentials::read_copied_login(login.harness, home)? else {
            return Ok(());
        };
        copy.as_ref() != bytes
            && !matches!(
                mj_core::credentials::reconcile(
                    &CredentialSnapshot::of(login.harness, &bytes),
                    &CredentialSnapshot::of(login.harness, &login.bytes)
                ),
                SyncAction::Push
            )
    };
    if replace {
        let path = home.join(&login.login_relative_path);
        let mut parent = home.to_owned();
        for component in login
            .login_relative_path
            .parent()
            .context("login parent")?
            .components()
        {
            parent.push(component);
            directory(&parent)?;
        }
        mj_core::confined_path::reject_symlinks(home, &login.login_relative_path, true)?;
        atomic_private_write(&path, &copy)?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use mj_core::worker_launch::ReloadSharedLogin;

    #[test]
    fn acknowledgement_cannot_delete_a_replaced_tentative_receipt() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(RETIRED_LOGINS);
        std::fs::create_dir(&directory).unwrap();
        let old = br#"{"last_refresh":"2026-11-24T00:00:00Z"}"#;
        let newer = br#"{"last_refresh":"2026-11-25T00:00:00Z"}"#;
        let record = RetiredLoginRecord {
            operation_id: "move".into(),
            store_id: "store".into(),
            login: Some(mj_core::credentials::RetiredLoginSnapshot {
                id: "move".into(),
                profile_id: "profile".into(),
                harness: mj_core::config::HarnessKind::Codex,
                store_id: "store".into(),
                login_relative_path: "auth.json".into(),
                bytes: newer.to_vec(),
            }),
        };
        let path = retired_path(root.path(), "move").unwrap();
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        let error = ack_retired_login(
            root.path(),
            "move",
            &mj_core::credentials::credential_fingerprint(old),
        )
        .unwrap_err();
        assert!(error.to_string().contains("changed before acknowledgement"));
        assert_eq!(read_retired_logins(root.path()).unwrap()[0].bytes, newer);
        ack_retired_login(
            root.path(),
            "move",
            &mj_core::credentials::credential_fingerprint(newer),
        )
        .unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn reload_commit_retains_late_source_grants_across_every_replay_cut() {
        use mj_core::{config::HarnessKind, worker_launch::WorkerOwnership};
        use std::os::unix::fs::PermissionsExt;
        for kind in [
            HarnessKind::Codex,
            HarnessKind::Kimi,
            HarnessKind::Grok,
            HarnessKind::Muse,
            HarnessKind::Antigravity,
            HarnessKind::Claude,
        ] {
            for cut in 0..3 {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path();
                let pending = fixture(root);
                let candidate = root.join("reload-candidates/candidate");
                let home = root.join("home");
                let ownership = WorkerOwnership {
                    version: 3,
                    no_github_credentials: false,
                    workspace_id: "default".into(),
                    session_id: "session".into(),
                    profile_id: "source".into(),
                    bundle_id: "bundle".into(),
                    target_template_id: "target".into(),
                    instance_id: None,
                };
                ownership.write(&root.join("ownership.json")).unwrap();
                ownership.write(&candidate.join("ownership.json")).unwrap();
                let mut launch =
                    crate::worker_runtime::relay_tests::launch_config(home.to_str().unwrap());
                launch.harness = kind;
                launch.write(&root.join("launch.json")).unwrap();
                launch.write(&candidate.join("launch.json")).unwrap();
                let bytes = |late: bool| {
                    let time = if late {
                        "2026-11-25T00:00:00Z"
                    } else {
                        "2026-11-24T00:00:00Z"
                    };
                    let epoch = if late { 1795564800_i64 } else { 1795478400_i64 };
                    serde_json::to_vec(&match kind {
                        HarnessKind::Codex => serde_json::json!({"last_refresh":time,"tokens":{"refresh_token":"grant"}}),
                        HarnessKind::Kimi => serde_json::json!({"expires_at":epoch,"refresh_token":"grant"}),
                        HarnessKind::Grok => serde_json::json!({"issuer::grant":{"expires_at":time,"refresh_token":"grant"}}),
                        HarnessKind::Muse => serde_json::json!({"providers":{"meta":{"expires_at":epoch,"refresh_token":"grant"}}}),
                        HarnessKind::Antigravity => serde_json::json!({"token":{"expiry":time,"refresh_token":"grant"}}),
                        HarnessKind::Claude => serde_json::json!({"claudeAiOauth":{"expiresAt":epoch*1000,"refreshToken":"grant"}}),
                        _ => unreachable!(),
                    }).unwrap()
                };
                let relative = mj_core::credentials::copied_login_relative_path(kind).unwrap();
                let live = home.join(relative);
                let staged = candidate.join("profile").join(relative);
                std::fs::create_dir_all(live.parent().unwrap()).unwrap();
                std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
                std::fs::write(&live, bytes(false)).unwrap();
                std::fs::write(
                    staged,
                    mj_core::credentials::credential_copy_for_harness(kind, &bytes(false)),
                )
                .unwrap();
                let mut manifest: ReloadCandidateManifest = serde_json::from_slice(
                    &std::fs::read(candidate.join("manifest.json")).unwrap(),
                )
                .unwrap();
                manifest.harness = kind;
                manifest.shared_login = None;
                manifest.clear_paths.push(
                    relative
                        .components()
                        .next()
                        .unwrap()
                        .as_os_str()
                        .to_str()
                        .unwrap()
                        .into(),
                );
                std::fs::write(
                    candidate.join("manifest.json"),
                    serde_json::to_vec(&manifest).unwrap(),
                )
                .unwrap();
                // The source spends/rotates its grant after candidate preparation.
                std::fs::write(&live, bytes(true)).unwrap();
                commit_pending_reload(root, &pending).unwrap();
                assert!(read_retired_logins(root).unwrap().is_empty());
                assert!(
                    ack_retired_login(
                        root,
                        "move",
                        &mj_core::credentials::credential_fingerprint(&bytes(true))
                    )
                    .is_err()
                );
                if cut >= 1 {
                    std::fs::write(&live, bytes(false)).unwrap();
                }
                if cut >= 2 {
                    let mut destination = ownership.clone();
                    destination.profile_id = "already-promoted".into();
                    destination.write(&root.join("ownership.json")).unwrap();
                    launch.write(&root.join("launch.json")).unwrap();
                }
                roll_forward_pending_reload_if_any(root).unwrap();
                let logins = read_retired_logins(root).unwrap();
                assert_eq!(logins.len(), 1);
                assert_eq!(logins[0].profile_id, "source");
                assert_eq!(logins[0].bytes, bytes(true));
                assert_eq!(
                    std::fs::read(&live).unwrap(),
                    mj_core::credentials::credential_copy_for_harness(kind, &bytes(true)).as_ref()
                );
                assert_eq!(
                    std::fs::metadata(retired_path(root, "move").unwrap())
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
                ack_retired_login(
                    root,
                    "move",
                    &mj_core::credentials::credential_fingerprint(&bytes(true)),
                )
                .unwrap();
                ack_retired_login(
                    root,
                    "move",
                    &mj_core::credentials::credential_fingerprint(&bytes(true)),
                )
                .unwrap();
                assert!(read_retired_logins(root).unwrap().is_empty());
                assert!(ack_retired_login(root, "../launch", "unused").is_err());
            }
        }
    }

    #[test]
    fn reload_commit_never_captures_shared_or_stripped_logins() {
        use mj_core::config::HarnessKind;
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let outside = dir.path().join("canonical");
        std::fs::write(&outside, b"{\"last_refresh\":\"2026-11-25T00:00:00Z\"}").unwrap();
        symlink(&outside, home.join("auth.json")).unwrap();
        assert!(
            mj_core::credentials::read_harvestable_copied_login(HarnessKind::Codex, &home)
                .unwrap()
                .is_none()
        );
        std::fs::create_dir(dir.path().join("credentials")).unwrap();
        symlink(dir.path().join("credentials"), home.join("credentials")).unwrap();
        assert!(
            mj_core::credentials::read_harvestable_copied_login(HarnessKind::Kimi, &home)
                .unwrap()
                .is_none()
        );
        std::fs::write(
            home.join(".credentials.json"),
            b"{\"claudeAiOauth\":{\"expiresAt\":9999999999999}}",
        )
        .unwrap();
        assert!(
            mj_core::credentials::read_harvestable_copied_login(HarnessKind::Claude, &home)
                .unwrap()
                .is_none()
        );
        for kind in [HarnessKind::OpenCode, HarnessKind::Dsh] {
            assert!(
                mj_core::credentials::read_harvestable_copied_login(kind, &home)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn reload_commit_precommit_failure_cannot_pin_an_old_grant_for_retry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let pending = fixture(root);
        let home = root.join("home");
        let launch = crate::worker_runtime::relay_tests::launch_config(home.to_str().unwrap());
        launch.write(&root.join("launch.json")).unwrap();
        std::fs::write(root.join("ownership.json"), serde_json::to_vec(&serde_json::json!({
            "version":3,"session_id":"session","profile_id":"source","bundle_id":"bundle","target_template_id":"target",
        })).unwrap()).unwrap();
        std::fs::write(
            home.join("auth.json"),
            br#"{"last_refresh":"2026-11-24T00:00:00Z"}"#,
        )
        .unwrap();
        let candidate = root.join("reload-candidates/candidate");
        let saved = root.join("saved-candidate");
        std::fs::rename(&candidate, &saved).unwrap();
        assert!(commit_pending_reload(root, &pending).is_err());
        assert!(!root.join(PENDING_RELOAD_FILE).exists());
        assert!(!retired_path(root, "move").unwrap().exists());
        std::fs::rename(saved, candidate).unwrap();
        let refreshed = br#"{"last_refresh":"2026-11-25T00:00:00Z"}"#;
        std::fs::write(home.join("auth.json"), refreshed).unwrap();
        commit_pending_reload(root, &pending).unwrap();
        let record = read_retired_login_record(root, &pending).unwrap().unwrap();
        assert_eq!(record.login.unwrap().bytes, refreshed);
    }

    #[test]
    fn reload_commit_retired_grants_never_cross_profiles_or_replace_shared_links() {
        for shared in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let pending = fixture(root);
            let candidate = root.join("reload-candidates/candidate");
            let home = root.join("home");
            let fresh = br#"{"last_refresh":"2026-11-25T00:00:00Z"}"#;
            let stale = br#"{"last_refresh":"2026-11-24T00:00:00Z"}"#;
            std::fs::write(home.join("auth.json"), fresh).unwrap();
            std::fs::write(candidate.join("profile/auth.json"), stale).unwrap();
            let source = serde_json::json!({"version":3,"session_id":"session","profile_id":"source",
                "bundle_id":"bundle","target_template_id":"target"});
            std::fs::write(
                root.join("ownership.json"),
                serde_json::to_vec(&source).unwrap(),
            )
            .unwrap();
            let mut destination = source.clone();
            if !shared {
                destination["profile_id"] = "different-profile".into();
            }
            std::fs::write(
                candidate.join("ownership.json"),
                serde_json::to_vec(&destination).unwrap(),
            )
            .unwrap();
            let launch = crate::worker_runtime::relay_tests::launch_config(home.to_str().unwrap());
            launch.write(&root.join("launch.json")).unwrap();
            launch.write(&candidate.join("launch.json")).unwrap();
            let mut manifest: ReloadCandidateManifest =
                serde_json::from_slice(&std::fs::read(candidate.join("manifest.json")).unwrap())
                    .unwrap();
            if !shared {
                manifest.shared_login = None;
            }
            std::fs::write(
                candidate.join("manifest.json"),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            commit_pending_reload(root, &pending).unwrap();
            apply_committed_reload(root, &pending).unwrap();
            assert_eq!(read_retired_logins(root).unwrap()[0].bytes, fresh);
            if shared {
                assert!(
                    std::fs::symlink_metadata(home.join("auth.json"))
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
                assert_eq!(
                    std::fs::read(root.join("shared-auth")).unwrap(),
                    b"shared login"
                );
            } else {
                assert_eq!(std::fs::read(home.join("auth.json")).unwrap(), stale);
            }
        }
    }

    #[test]
    fn reload_commit_retired_rotations_win_ties_but_a_strictly_newer_candidate_wins() {
        let equal_retired =
            br#"{"last_refresh":"2026-11-25T00:00:00Z","tokens":{"refresh_token":"retired"}}"#
                .as_slice();
        let equal_candidate =
            br#"{"last_refresh":"2026-11-25T00:00:00Z","tokens":{"refresh_token":"candidate"}}"#
                .as_slice();
        let older =
            br#"{"last_refresh":"2026-11-24T00:00:00Z","tokens":{"refresh_token":"retired"}}"#
                .as_slice();
        let unknown_retired = br#"{"tokens":{"refresh_token":"retired"}}"#.as_slice();
        let unknown_candidate = br#"{"tokens":{"refresh_token":"candidate"}}"#.as_slice();
        for (retired, candidate, expected) in [
            (equal_retired, equal_candidate, equal_retired),
            (unknown_retired, unknown_candidate, unknown_retired),
            (older, equal_candidate, equal_candidate),
        ] {
            let root = tempfile::tempdir().unwrap();
            let pending = copied_fixture(
                root.path(),
                mj_core::config::HarnessKind::Codex,
                retired,
                Some(candidate),
            );
            commit_pending_reload(root.path(), &pending).unwrap();
            apply_committed_reload(root.path(), &pending).unwrap();
            assert_eq!(
                std::fs::read(root.path().join("home/auth.json")).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn reload_commit_legacy_wal_never_captures_a_partially_promoted_destination_as_source() {
        let root = tempfile::tempdir().unwrap();
        let source_a = br#"{"last_refresh":"2026-11-24T00:00:00Z","tokens":{"account_id":"A"}}"#;
        let destination_b =
            br#"{"last_refresh":"2026-11-25T00:00:00Z","tokens":{"account_id":"B"}}"#;
        let pending = copied_fixture(
            root.path(),
            mj_core::config::HarnessKind::Codex,
            source_a,
            Some(destination_b),
        );
        let ownership_path = root
            .path()
            .join("reload-candidates/candidate/ownership.json");
        let mut destination: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&ownership_path).unwrap()).unwrap();
        destination["profile_id"] = "destination-B".into();
        std::fs::write(ownership_path, serde_json::to_vec(&destination).unwrap()).unwrap();
        // Old workers could clear/overlay the home before publishing ownership.
        std::fs::write(root.path().join("home/auth.json"), destination_b).unwrap();
        atomic_private_write(
            &root.path().join(PENDING_RELOAD_FILE),
            &serde_json::to_vec(&pending).unwrap(),
        )
        .unwrap();
        assert!(!root.path().join(RETIRED_LOGINS).exists());
        roll_forward_pending_reload_if_any(root.path()).unwrap();
        assert!(read_retired_logins(root.path()).unwrap().is_empty());
        assert!(!root.path().join(RETIRED_LOGINS).exists());
        assert_eq!(
            std::fs::read(root.path().join("home/auth.json")).unwrap(),
            destination_b
        );
    }

    #[test]
    fn reload_commit_pre_wal_recapture_replaces_or_removes_stale_receipts() {
        for remove_login in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let old = br#"{"last_refresh":"2026-11-24T00:00:00Z"}"#;
            let fresh = br#"{"last_refresh":"2026-11-25T00:00:00Z"}"#;
            let pending = copied_fixture(
                root.path(),
                mj_core::config::HarnessKind::Codex,
                old,
                Some(fresh),
            );
            capture_retired_login(root.path(), &pending).unwrap();
            assert!(
                read_retired_login_record(root.path(), &pending)
                    .unwrap()
                    .is_some()
            );
            assert!(!root.path().join(PENDING_RELOAD_FILE).exists());
            if remove_login {
                std::fs::remove_file(root.path().join("home/auth.json")).unwrap();
            } else {
                std::fs::write(root.path().join("home/auth.json"), fresh).unwrap();
            }
            commit_pending_reload(root.path(), &pending).unwrap();
            let record = read_retired_login_record(root.path(), &pending).unwrap();
            if remove_login {
                assert!(record.is_none());
            } else {
                assert_eq!(record.unwrap().login.unwrap().bytes, fresh);
            }
        }
    }

    fn copied_fixture(
        root: &Path,
        kind: mj_core::config::HarnessKind,
        source: &[u8],
        destination: Option<&[u8]>,
    ) -> PendingInPlaceReload {
        let pending = fixture(root);
        let candidate = root.join("reload-candidates/candidate");
        let home = root.join("home");
        let relative = mj_core::credentials::copied_login_relative_path(kind).unwrap();
        let ownership = serde_json::to_vec(&serde_json::json!({"version":3,"session_id":"session","profile_id":"source","bundle_id":"bundle","target_template_id":"target"})).unwrap();
        std::fs::write(root.join("ownership.json"), &ownership).unwrap();
        std::fs::write(candidate.join("ownership.json"), ownership).unwrap();
        let mut launch = crate::worker_runtime::relay_tests::launch_config(home.to_str().unwrap());
        launch.harness = kind;
        launch.write(&root.join("launch.json")).unwrap();
        launch.write(&candidate.join("launch.json")).unwrap();
        std::fs::create_dir_all(home.join(relative).parent().unwrap()).unwrap();
        std::fs::write(home.join(relative), source).unwrap();
        if let Some(bytes) = destination {
            std::fs::create_dir_all(candidate.join("profile").join(relative).parent().unwrap())
                .unwrap();
            std::fs::write(candidate.join("profile").join(relative), bytes).unwrap();
        }
        let mut manifest: ReloadCandidateManifest =
            serde_json::from_slice(&std::fs::read(candidate.join("manifest.json")).unwrap())
                .unwrap();
        manifest.harness = kind;
        manifest.shared_login = None;
        manifest.clear_paths.push(
            relative
                .components()
                .next()
                .unwrap()
                .as_os_str()
                .to_str()
                .unwrap()
                .into(),
        );
        std::fs::write(
            candidate.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        pending
    }

    #[test]
    fn reload_commit_restores_missing_copies_but_never_shared_logins() {
        use mj_core::config::HarnessKind;
        use std::os::unix::fs::PermissionsExt;
        for kind in [HarnessKind::Codex, HarnessKind::Kimi, HarnessKind::Claude] {
            for shared in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let bytes = match kind {
                    HarnessKind::Claude => {
                        br#"{"claudeAiOauth":{"refreshToken":"retired","expiresAt":1795564800000}}"#
                            .as_slice()
                    }
                    _ => br#"{"last_refresh":"2026-11-25T00:00:00Z","refresh_token":"retired"}"#
                        .as_slice(),
                };
                let pending = copied_fixture(root.path(), kind, bytes, None);
                let relative = mj_core::credentials::copied_login_relative_path(kind).unwrap();
                if shared {
                    let manifest_path = root
                        .path()
                        .join("reload-candidates/candidate/manifest.json");
                    let mut manifest: ReloadCandidateManifest =
                        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
                    manifest.shared_login = Some(ReloadSharedLogin {
                        relative_marker: relative.to_string_lossy().into_owned(),
                        source_path: root
                            .path()
                            .join("shared-auth")
                            .to_string_lossy()
                            .into_owned(),
                    });
                    std::fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
                }
                commit_pending_reload(root.path(), &pending).unwrap();
                apply_committed_reload(root.path(), &pending).unwrap();
                let path = root.path().join("home").join(relative);
                if shared {
                    assert!(
                        std::fs::symlink_metadata(&path)
                            .unwrap()
                            .file_type()
                            .is_symlink()
                    );
                    assert_eq!(std::fs::read(&path).unwrap(), b"shared login");
                } else {
                    assert_eq!(
                        std::fs::read(&path).unwrap(),
                        mj_core::credentials::credential_copy_for_harness(kind, bytes).as_ref()
                    );
                    assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
                    assert_eq!(
                        path.parent()
                            .unwrap()
                            .metadata()
                            .unwrap()
                            .permissions()
                            .mode()
                            & 0o777,
                        0o700
                    );
                }
            }
        }
    }

    #[test]
    fn reload_commit_corrupt_source_login_never_blocks_live_or_wal_promotion() {
        for corrupt in [b"".as_slice(), b"{".as_slice()] {
            for replay in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let valid = br#"{"last_refresh":"2026-11-25T00:00:00Z","tokens":{"refresh_token":"valid"}}"#;
                let pending = copied_fixture(
                    root.path(),
                    mj_core::config::HarnessKind::Codex,
                    corrupt,
                    Some(valid),
                );
                if replay {
                    atomic_private_write(
                        &root.path().join(PENDING_RELOAD_FILE),
                        &serde_json::to_vec(&pending).unwrap(),
                    )
                    .unwrap();
                    roll_forward_pending_reload_if_any(root.path()).unwrap();
                } else {
                    commit_pending_reload(root.path(), &pending).unwrap();
                    assert!(
                        read_retired_login_record(root.path(), &pending)
                            .unwrap()
                            .is_none()
                    );
                    apply_committed_reload(root.path(), &pending).unwrap();
                }
                assert!(read_retired_logins(root.path()).unwrap().is_empty());
                assert_eq!(
                    std::fs::read(root.path().join("home/auth.json")).unwrap(),
                    valid
                );
                assert!(!root.path().join(PENDING_RELOAD_FILE).exists());
            }
        }
    }

    #[test]
    fn reload_commit_retired_pages_fit_frames_and_advance_even_with_a_tiny_budget() {
        use mj_core::credentials::RetiredLoginSnapshot;
        let root = tempfile::tempdir().unwrap();
        directory(&root.path().join(RETIRED_LOGINS)).unwrap();
        for id in ["a", "a-1", "a-1-1", "b"] {
            let login = RetiredLoginSnapshot {
                id: id.into(),
                profile_id: "source".into(),
                harness: mj_core::config::HarnessKind::Codex,
                store_id: "store".into(),
                login_relative_path: "auth.json".into(),
                bytes: serde_json::to_vec(&serde_json::json!({"padding":"x".repeat(800000)}))
                    .unwrap(),
            };
            atomic_private_write(
                &retired_path(root.path(), id).unwrap(),
                &serde_json::to_vec(&RetiredLoginRecord {
                    operation_id: id.into(),
                    store_id: "store".into(),
                    login: Some(login),
                })
                .unwrap(),
            )
            .unwrap();
        }
        let (tiny, has_more) = read_retired_logins_page(root.path(), None, 0).unwrap();
        assert_eq!(tiny.len(), 1);
        assert!(has_more);
        let mut after = None;
        let mut ids = Vec::new();
        let mut pages = 0;
        loop {
            let (logins, has_more) = read_retired_logins_page(
                root.path(),
                after.as_deref(),
                mj_core::credentials::RETIRED_LOGIN_PAGE_RAW_BYTES,
            )
            .unwrap();
            after = logins.last().map(|login| login.id.clone());
            ids.extend(logins.iter().map(|login| login.id.clone()));
            let response = mj_core::relay::RelayResponseEnvelope {
                request_id: "page".into(),
                protocol_version: 93,
                body: mj_core::relay::RelayResponseBody::Ok {
                    payload: mj_core::relay::RelayResponsePayload::RetiredLogins {
                        logins,
                        has_more,
                        deferred: false,
                    },
                },
            };
            let encoded = serde_json::to_vec(&response).unwrap();
            assert!(encoded.len() < mj_core::relay::MAX_FRAME_BYTES);
            let round_trip: mj_core::relay::RelayResponseEnvelope =
                serde_json::from_slice(&encoded).unwrap();
            assert_eq!(round_trip, response);
            pages += 1;
            if !has_more {
                break;
            }
        }
        assert_eq!(pages, 2);
        assert_eq!(ids, ["a", "a-1", "a-1-1", "b"]);
    }

    fn fixture(root: &Path) -> PendingInPlaceReload {
        let home = root.join("home");
        let candidate = root.join("reload-candidates/candidate");
        std::fs::create_dir_all(candidate.join("profile/skills/user")).unwrap();
        std::fs::create_dir_all(home.join("skills/.system")).unwrap();
        std::fs::write(home.join("skills/.system/native"), "native skill").unwrap();
        std::fs::write(home.join("history.jsonl"), "native history").unwrap();
        std::fs::write(home.join("config.toml"), "source").unwrap();
        std::fs::write(candidate.join("profile/config.toml"), "destination").unwrap();
        std::fs::write(candidate.join("profile/skills/user/SKILL.md"), "new skill").unwrap();
        crate::worker_runtime::relay_tests::launch_config(home.to_str().unwrap())
            .write(&candidate.join("launch.json"))
            .unwrap();
        std::fs::write(candidate.join("ownership.json"), "destination ownership").unwrap();
        std::fs::write(root.join("shared-auth"), "shared login").unwrap();
        let manifest = ReloadCandidateManifest {
            operation_id: "move".into(),
            expected_store_id: "store".into(),
            candidate_id: "candidate".into(),
            harness: mj_core::config::HarnessKind::Codex,
            target_profile_home: home.to_string_lossy().into_owned(),
            clear_paths: vec!["config.toml".into(), "skills".into(), "auth.json".into()],
            protected_paths: vec!["skills/.system".into()],
            shared_login: Some(ReloadSharedLogin {
                relative_marker: "auth.json".into(),
                source_path: root.join("shared-auth").to_string_lossy().into_owned(),
            }),
            clear_github_token: true,
        };
        std::fs::write(
            candidate.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::create_dir(root.join("harness-git-candidate")).unwrap();
        PendingInPlaceReload {
            operation_id: "move".into(),
            expected_store_id: "store".into(),
            candidate_id: "candidate".into(),
            git_setup_name: Some("harness-git-candidate".into()),
        }
    }

    // Hard-won: PENDING: in-place Move restaged canonical memory over unsynced replica edits
    #[test]
    fn reload_commit_preserves_unsynced_project_memory_edit_and_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let pending = fixture(root);
        let home = root.join("home");
        let candidate = root.join("reload-candidates/candidate");
        let replica = home.join("projects/replica/memory");
        let baseline = home.join("projects/replica/.hel-memory-baseline");
        let seed = mj_core::worker_launch::project_memory_seed_root(&baseline);
        for path in [&replica, &baseline, &seed] {
            std::fs::create_dir_all(path).unwrap();
        }
        let old = "canonical memory before local edits\n".repeat(4096);
        let edited = "local unsynced changes survive\n".repeat(4096);
        for path in [&baseline, &seed] {
            std::fs::write(path.join("note.md"), &old).unwrap();
            std::fs::write(path.join("deleted.md"), "deleted locally").unwrap();
        }
        std::fs::write(replica.join("note.md"), &edited).unwrap();
        let mut launch = crate::worker_runtime::relay_tests::launch_config(home.to_str().unwrap());
        launch.project_memory = Some(mj_core::worker_launch::ProjectMemoryLaunchConfig {
            history_socket: None,
            project_key: "key".into(),
            root: replica.clone(),
            baseline_root: baseline.clone(),
            repository_roots: Default::default(),
            mcp_delivery: Default::default(),
        });
        launch.write(&candidate.join("launch.json")).unwrap();
        commit_pending_reload(root, &pending).unwrap();
        apply_committed_reload(root, &pending).unwrap();
        assert_eq!(
            std::fs::read_to_string(replica.join("note.md")).unwrap(),
            edited
        );
        assert!(!replica.join("deleted.md").exists());
        assert_eq!(
            std::fs::read_to_string(baseline.join("note.md")).unwrap(),
            old
        );
        assert_eq!(
            std::fs::read_to_string(baseline.join("deleted.md")).unwrap(),
            "deleted locally"
        );
        assert!(!seed.exists());
    }

    #[test]
    fn reload_commit_replaces_legacy_home_link_without_mutating_external_profile() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let pending = fixture(root);
        let external = tempfile::tempdir().unwrap();
        std::fs::write(external.path().join("config.toml"), "user profile").unwrap();
        std::fs::write(external.path().join("history.jsonl"), "user history").unwrap();
        std::fs::remove_dir_all(root.join("home")).unwrap();
        symlink(external.path(), root.join("home")).unwrap();
        commit_pending_reload(root, &pending).unwrap();
        apply_committed_reload(root, &pending).unwrap();
        assert!(
            !std::fs::symlink_metadata(root.join("home"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::metadata(root.join("home"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::read(root.join("home/config.toml")).unwrap(),
            b"destination"
        );
        assert_eq!(
            std::fs::read(external.path().join("config.toml")).unwrap(),
            b"user profile"
        );
        assert_eq!(
            std::fs::read(external.path().join("history.jsonl")).unwrap(),
            b"user history"
        );
    }

    #[test]
    fn reload_commit_reconciles_crash_before_relay_commit_without_downgrading_holds() {
        use crate::relay::DurableRelay;
        use mj_core::relay::*;
        use mj_core::worker_launch::WorkerLaunchConfig;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut relay = DurableRelay::open(root, "session-1", "1.0.0").unwrap();
        let store = relay.operational_state().store_id.clone().unwrap();
        let mut pending = fixture(root);
        pending.expected_store_id = store.clone();
        let candidate = root.join("reload-candidates/candidate");
        let mut manifest: ReloadCandidateManifest =
            serde_json::from_slice(&std::fs::read(candidate.join("manifest.json")).unwrap())
                .unwrap();
        manifest.expected_store_id = store.clone();
        std::fs::write(
            candidate.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let mut launch =
            crate::worker_runtime::relay_tests::launch_config(root.join("home").to_str().unwrap());
        launch.accepted_reload_operation_id = Some("move".into());
        launch.write(&candidate.join("launch.json")).unwrap();
        let response = relay.handle(RelayRequestEnvelope {
            request_id: "hold".into(),
            protocol_version: RELAY_PROTOCOL_VERSION,
            request: RelayRequest::HoldInPlaceMoveDispatch {
                operation_id: "move".into(),
                expected_store_id: store.clone(),
            },
        });
        assert!(matches!(response.body, RelayResponseBody::Ok { .. }));
        commit_pending_reload(root, &pending).unwrap();
        drop(relay); // Crash before the relay's Committed phase is persisted.
        roll_forward_pending_reload_if_any(root).unwrap();
        let launch = WorkerLaunchConfig::read(&root.join("launch.json")).unwrap();
        let mut relay = DurableRelay::open(root, "session-1", "1.0.0").unwrap();
        relay.reconcile_committed_reload("older-move").unwrap();
        assert_eq!(
            relay
                .operational_state()
                .in_place_move_hold
                .as_ref()
                .unwrap()
                .reload_phase,
            None
        );
        relay
            .reconcile_committed_reload(launch.accepted_reload_operation_id.as_deref().unwrap())
            .unwrap();
        drop(relay);
        let mut relay = DurableRelay::open(root, "session-1", "1.0.0").unwrap();
        assert_eq!(
            relay
                .operational_state()
                .in_place_move_hold
                .as_ref()
                .unwrap()
                .reload_phase,
            Some(InPlaceReloadPhase::Committed)
        );
        relay.finish_committed_reload().unwrap();
        relay.reconcile_committed_reload("move").unwrap();
        let hold = relay
            .operational_state()
            .in_place_move_hold
            .as_ref()
            .unwrap()
            .clone();
        assert_eq!(hold.reload_phase, Some(InPlaceReloadPhase::Started));
        assert!(hold.harness_restarted);
        relay
            .mark_in_place_harness_failed_after_commit("move", &store, "failed".into())
            .unwrap();
        relay.reconcile_committed_reload("move").unwrap();
        drop(relay);
        let relay = DurableRelay::open(root, "session-1", "1.0.0").unwrap();
        assert_eq!(
            relay
                .operational_state()
                .in_place_move_hold
                .as_ref()
                .unwrap()
                .reload_phase,
            Some(InPlaceReloadPhase::FailedAfterCommit {
                message: "failed".into()
            })
        );
    }

    #[test]
    fn reload_commit_resolves_relative_ssh_profile_home_from_startup_directory() {
        let cwd = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&cwd).unwrap();
        let pending = fixture(dir.path());
        let path = dir.path().join("reload-candidates/candidate/manifest.json");
        let mut manifest: ReloadCandidateManifest =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest.target_profile_home = dir
            .path()
            .join("home")
            .strip_prefix(&cwd)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        commit_pending_reload(dir.path(), &pending).unwrap();
        roll_forward_pending_reload_if_any(dir.path()).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("home/config.toml")).unwrap(),
            b"destination"
        );
    }

    #[test]
    fn reload_commit_rolls_forward_before_overlay_after_partial_overlay_and_after_launch_promotion()
    {
        use std::os::unix::fs::PermissionsExt;
        for cut in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let pending = fixture(root);
            let expected_launch =
                std::fs::read(root.join("reload-candidates/candidate/launch.json")).unwrap();
            std::fs::create_dir_all(root.join("reload-candidates/orphan")).unwrap();
            std::fs::write(root.join("github-token"), "old secret").unwrap();
            commit_pending_reload(root, &pending).unwrap();
            if cut >= 1 {
                std::fs::remove_file(root.join("home/config.toml")).unwrap();
                std::fs::write(root.join("home/skills/partial"), "partial").unwrap();
            }
            if cut >= 2 {
                atomic_private_write(&root.join("launch.json"), b"destination launch").unwrap();
            }
            assert_eq!(
                roll_forward_pending_reload_if_any(root).unwrap().as_deref(),
                Some("move")
            );
            assert_eq!(
                std::fs::read(root.join("home/config.toml")).unwrap(),
                b"destination"
            );
            assert_eq!(
                std::fs::read(root.join("home/history.jsonl")).unwrap(),
                b"native history"
            );
            assert_eq!(
                std::fs::read(root.join("home/skills/.system/native")).unwrap(),
                b"native skill"
            );
            assert!(!root.join("home/skills/partial").exists());
            assert_eq!(
                std::fs::read(root.join("home/auth.json")).unwrap(),
                b"shared login"
            );
            assert!(
                std::fs::symlink_metadata(root.join("home/auth.json"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(
                std::fs::read(root.join("launch.json")).unwrap(),
                expected_launch
            );
            assert_eq!(
                std::fs::read(root.join("ownership.json")).unwrap(),
                b"destination ownership"
            );
            assert_eq!(
                std::fs::read_to_string(root.join("current-git-setup")).unwrap(),
                "harness-git-candidate"
            );
            assert_eq!(
                root.join("launch.json")
                    .metadata()
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert!(!root.join(PENDING_RELOAD_FILE).exists());
            assert!(!root.join("github-token").exists());
            assert_eq!(
                std::fs::read_dir(root.join(RELOAD_CANDIDATES_DIR))
                    .unwrap()
                    .count(),
                0
            );
            assert_eq!(roll_forward_pending_reload_if_any(root).unwrap(), None);
        }
    }

    #[test]
    fn reload_commit_refuses_wrong_identity_and_overlay_links_before_live_writes() {
        let dir = tempfile::tempdir().unwrap();
        let pending = fixture(dir.path());
        assert!(validate_candidate(dir.path(), "candidate", "other", "store").is_err());
        assert!(validate_candidate(dir.path(), "../candidate", "move", "store").is_err());
        std::os::unix::fs::symlink(
            dir.path().join("shared-auth"),
            dir.path()
                .join("reload-candidates/candidate/profile/escape"),
        )
        .unwrap();
        commit_pending_reload(dir.path(), &pending).unwrap();
        assert!(apply_committed_reload(dir.path(), &pending).is_err());
        assert!(dir.path().join(PENDING_RELOAD_FILE).exists());
        assert!(dir.path().join("reload-candidates/candidate").exists());
        assert_eq!(
            std::fs::read(dir.path().join("shared-auth")).unwrap(),
            b"shared login"
        );
    }
}

#[cfg(all(test, unix))]
#[test]
fn reload_commit_orphan_sweep_refuses_a_linked_candidates_root() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("keep"), "outside data").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join(RELOAD_CANDIDATES_DIR)).unwrap();
    assert!(roll_forward_pending_reload_if_any(root.path()).is_err());
    assert_eq!(
        std::fs::read(outside.path().join("keep")).unwrap(),
        b"outside data"
    );
}
