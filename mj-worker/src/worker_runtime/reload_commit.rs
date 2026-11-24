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

pub fn commit_pending_reload(root: &Path, pending: &PendingInPlaceReload) -> Result<()> {
    validate_candidate_id(&pending.candidate_id)?;
    atomic_private_write(
        &root.join(PENDING_RELOAD_FILE),
        &serde_json::to_vec(pending)?,
    )
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

fn copy_overlay(source: &Path, destination: &Path) -> Result<()> {
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
            }
        }
    }
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
    let candidate = root.join(RELOAD_CANDIDATES_DIR).join(&pending.candidate_id);
    let home = Path::new(&manifest.target_profile_home);
    let home = if home.is_absolute() {
        home.to_owned()
    } else {
        std::env::current_dir()?.join(home)
    };
    let home = home.as_path();
    directory(home)?;
    for clear in &manifest.clear_paths {
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
        } else if path.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                let entry = entry?;
                if !protected.contains(&entry.file_name()) {
                    remove(&entry.path())?;
                }
            }
        }
    }
    copy_overlay(&candidate.join("profile"), home)?;
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
            std::os::unix::fs::symlink(&login.source_path, marker)?;
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("shared reload login requires Unix symlinks");
        }
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
    std::fs::remove_file(root.join(PENDING_RELOAD_FILE))?;
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use mj_core::worker_launch::ReloadSharedLogin;

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
        std::fs::write(candidate.join("launch.json"), "destination launch").unwrap();
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
                b"destination launch"
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
