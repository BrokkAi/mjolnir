use anyhow::{Context, Result};
use mj_core::worker_launch::ProjectMemoryLaunchConfig;

/// The worker is the only seeder; a retained or restored replica always wins.
pub(super) fn ensure_project_memory_replica(memory: &ProjectMemoryLaunchConfig) -> Result<()> {
    let seed = mj_core::worker_launch::project_memory_seed_root(&memory.baseline_root);
    if !memory.root.exists() {
        anyhow::ensure!(
            seed.is_dir(),
            "project memory seed is missing: {}",
            seed.display()
        );
        if memory.baseline_root.exists() {
            std::fs::remove_dir_all(&memory.baseline_root)?;
        }
        let parent = memory
            .root
            .parent()
            .context("project memory root has no parent")?;
        std::fs::create_dir_all(parent)?;
        let temporary = tempfile::tempdir_in(parent)?;
        let baseline = tempfile::tempdir_in(parent)?;
        super::reload_commit::copy_overlay(&seed, temporary.path())?;
        super::reload_commit::copy_overlay(&seed, baseline.path())?;
        std::fs::rename(baseline.path(), &memory.baseline_root)?;
        std::fs::rename(temporary.path(), &memory.root)?;
    } else if !memory.baseline_root.exists() {
        let parent = memory
            .baseline_root
            .parent()
            .context("project memory baseline has no parent")?;
        std::fs::create_dir_all(parent)?;
        let baseline = tempfile::tempdir_in(parent)?;
        super::reload_commit::copy_overlay(&memory.root, baseline.path())?;
        std::fs::rename(baseline.path(), &memory.baseline_root)?;
    }
    if seed.exists() {
        std::fs::remove_dir_all(&seed)?;
    }
    Ok(())
}
