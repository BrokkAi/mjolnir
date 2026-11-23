use super::*;

pub fn import_safety_issues(targets: &SessionEditTargets) -> Result<ImportSafetyIssues> {
    let mut dirty_git_roots = Vec::new();
    let mut has_untracked_files = false;
    for root in &targets.git_roots {
        let output = Command::new("git")
            .args(["status", "--porcelain=v1", "--untracked-files=normal"])
            .current_dir(root)
            .output()
            .with_context(|| format!("inspect Git status in {}", root.display()))?;
        ensure!(
            output.status.success(),
            "could not inspect Git status in {}",
            root.display()
        );
        let (tracked, untracked) = String::from_utf8_lossy(&output.stdout).lines().fold(
            (0_usize, 0_usize),
            |(tracked, untracked), line| {
                if line.starts_with("??") {
                    (tracked, untracked + 1)
                } else {
                    (tracked + 1, untracked)
                }
            },
        );
        has_untracked_files |= untracked > 0;
        if tracked + untracked > 0 {
            let mut parts = Vec::new();
            if tracked > 0 {
                parts.push(format!(
                    "{tracked} tracked change{}",
                    if tracked == 1 { "" } else { "s" }
                ));
            }
            if untracked > 0 {
                parts.push(format!(
                    "{untracked} untracked path{}",
                    if untracked == 1 { "" } else { "s" }
                ));
            }
            dirty_git_roots.push((root.clone(), parts.join(" · ")));
        }
    }
    Ok(ImportSafetyIssues {
        dirty_git_roots,
        omitted_non_git_dirs: targets.non_git_dirs.clone(),
        scratch_git_roots: targets.scratch_git_roots.clone(),
        has_untracked_files,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedClaudeSession {
    pub remapped_bundle: Option<ProjectBundle>,
    pub session_id: String,
    pub native_session_id: String,
    pub source_jsonl: PathBuf,
    pub source_cwd: PathBuf,
    pub bundle_id: String,
    pub archive_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportArchiveProgress {
    Repository {
        current: usize,
        total: usize,
        id: String,
    },
    UntrackedFile {
        repository_id: String,
        current: usize,
        total: usize,
        path: PathBuf,
    },
    WritingArchive,
}

pub struct ImportControl<'a> {
    pub cancelled: &'a AtomicBool,
    pub progress: &'a (dyn Fn(ImportArchiveProgress) + Sync),
    pub include_untracked: bool,
}

impl ImportControl<'_> {
    pub(super) fn check_cancelled(&self) -> Result<()> {
        ensure!(!self.cancelled.load(Ordering::Acquire), "import cancelled");
        Ok(())
    }

    pub fn report(&self, progress: ImportArchiveProgress) -> Result<()> {
        self.check_cancelled()?;
        (self.progress)(progress);
        Ok(())
    }
}

/// Resolve a harness's configuration home without ever modifying it.
///
/// The environment override wins. Standalone dsh-acp defaults to a directory
/// in the current workspace. Other harnesses default to the user's home.
pub fn harness_config_home(kind: HarnessKind) -> Result<PathBuf> {
    resolve_harness_config_home(kind, std::env::var_os(kind.home_env()), || {
        std::env::current_dir().ok()
    })
    .with_context(|| {
        format!(
            "cannot determine {} home; set {}",
            kind.display_name(),
            kind.home_env()
        )
    })
}

fn resolve_harness_config_home(
    kind: HarnessKind,
    environment_home: Option<std::ffi::OsString>,
    current_dir: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    environment_home
        .filter(|path| !path.is_empty())
        .map(|path| kind.home_from_environment(path))
        .or_else(|| {
            if kind == HarnessKind::Dsh {
                current_dir().map(|cwd| cwd.join(".dsh-acp"))
            } else {
                mj_core::path_input::user_home().map(|home| home.join(kind.default_home_leaf()))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsh_import_home_matches_standalone_acp_environment_and_cwd() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().join("workspace");
        let override_dir = directory.path().join("custom-acp-home");
        let resolve =
            |env| resolve_harness_config_home(HarnessKind::Dsh, env, || Some(cwd.clone())).unwrap();
        assert_eq!(
            resolve(Some(override_dir.clone().into_os_string())),
            override_dir
        );
        assert_eq!(resolve(None), cwd.join(".dsh-acp"));
        assert_eq!(resolve(Some("".into())), cwd.join(".dsh-acp"));
    }
}
