//! Target checkpoint command entrypoints.
use anyhow::{Context, Result};
use mj_core::archive::SystemGit;
use mj_core::checkpoint::*;
use std::fs;
use std::path::Path;
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
