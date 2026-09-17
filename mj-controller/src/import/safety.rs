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

    pub(super) fn report(&self, progress: ImportArchiveProgress) -> Result<()> {
        self.check_cancelled()?;
        (self.progress)(progress);
        Ok(())
    }
}

/// Resolve a harness's configuration home without ever modifying it.
///
/// The environment override wins; otherwise the harness's default directory
/// beneath the user's home is used, the same pair `mj setup` discovers.
pub fn harness_config_home(kind: HarnessKind) -> Result<PathBuf> {
    let name = kind.display_name();
    let home = std::env::var_os(kind.home_env())
        .map(|path| kind.home_from_environment(path))
        .or_else(|| dirs::home_dir().map(|home| home.join(kind.default_home_leaf())))
        .with_context(|| format!("cannot determine {name} home; set {}", kind.home_env()))?;
    ensure!(
        home.is_dir(),
        "{name} home is not a directory: {}",
        home.display()
    );
    Ok(home)
}
