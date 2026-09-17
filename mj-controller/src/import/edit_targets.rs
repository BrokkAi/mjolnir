use super::*;

pub fn session_edit_targets(
    transcript: &ClaudeTranscript,
    profile_home: &Path,
) -> Result<SessionEditTargets> {
    session_edit_targets_with_scratch_prefixes(transcript, profile_home, &scratch_prefixes())
}

/// Directories whose repositories are throwaway workspaces rather than
/// projects. A session that writes into one of them is still anchored on its
/// own repository.
pub(super) fn scratch_prefixes() -> Vec<PathBuf> {
    let mut prefixes = Vec::new();
    let mut remember = |path: PathBuf| {
        let path = fs::canonicalize(&path).unwrap_or(path);
        if !prefixes.contains(&path) {
            prefixes.push(path);
        }
    };
    remember(std::env::temp_dir());
    for literal in ["/tmp", "/var/tmp", "/dev/shm"] {
        remember(PathBuf::from(literal));
    }
    prefixes
}

pub(super) fn session_edit_targets_with_scratch_prefixes(
    transcript: &ClaudeTranscript,
    profile_home: &Path,
    scratch_prefixes: &[PathBuf],
) -> Result<SessionEditTargets> {
    let profile_home =
        fs::canonicalize(profile_home).unwrap_or_else(|_| profile_home.to_path_buf());
    let mut paths = transcript
        .edited_paths
        .iter()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                transcript.cwd.join(path)
            }
        })
        .filter(|path| {
            let comparable = canonicalize_existing_ancestor(path);
            !comparable.starts_with(&profile_home)
        })
        .collect::<Vec<_>>();
    if paths.is_empty() {
        paths.push(transcript.cwd.clone());
    }

    let cwd_root = git_root_for_path(&transcript.cwd)?.with_context(|| {
        format!(
            "session cwd is not in a usable Git worktree: {}",
            transcript.cwd.display()
        )
    })?;
    // The session's own repository is authoritative even when it lives under a
    // temporary directory.
    let mut git_roots = BTreeSet::from([cwd_root.clone()]);
    let mut scratch_git_roots = BTreeSet::new();
    let mut non_git_dirs = BTreeSet::new();
    for path in paths {
        if let Some(root) = git_root_for_path(&path)? {
            if root != cwd_root && is_scratch_root(&root, scratch_prefixes) {
                scratch_git_roots.insert(root);
            } else {
                git_roots.insert(root);
            }
        } else {
            non_git_dirs.insert(edited_directory(&path));
        }
    }
    Ok(SessionEditTargets {
        git_roots: git_roots.into_iter().collect(),
        scratch_git_roots: scratch_git_roots.into_iter().collect(),
        non_git_dirs: non_git_dirs.into_iter().collect(),
    })
}

pub(super) fn is_scratch_root(root: &Path, scratch_prefixes: &[PathBuf]) -> bool {
    scratch_prefixes
        .iter()
        .any(|prefix| root.starts_with(prefix))
}

pub(super) fn canonicalize_existing_ancestor(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut suffix = Vec::new();
    loop {
        if let Ok(mut canonical) = fs::canonicalize(existing) {
            for component in suffix.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
        let Some(name) = existing.file_name() else {
            return path.to_path_buf();
        };
        suffix.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            return path.to_path_buf();
        };
        existing = parent;
    }
}

pub(super) fn edited_directory(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().unwrap_or(path).to_path_buf()
    }
}

pub(super) fn git_root_for_path(path: &Path) -> Result<Option<PathBuf>> {
    let mut probe = edited_directory(path);
    while !probe.is_dir() {
        if !probe.pop() {
            return Ok(None);
        }
    }
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&probe)
        .output()
        .with_context(|| format!("start git in {}", probe.display()))?;
    if !output.status.success() {
        return Ok(None);
    }
    let root = String::from_utf8(output.stdout).context("decode Git repository root")?;
    let root = PathBuf::from(root.trim());
    Ok(Some(fs::canonicalize(&root).unwrap_or(root)))
}
