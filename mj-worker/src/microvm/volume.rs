//! Named volumes: sparse ext4 disk files that outlive the VMs using them.
//!
//! A volume is sized to the filesystem that holds it. Firecracker has no
//! shared-directory device, so a volume is a block device; being sparse, it
//! costs host space only as the guest writes, and the host disk is the real
//! limit.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};

use super::state::{
    Layout, METADATA_FORMAT, VmState, VolumeMetadata, validate_label, validate_name,
};
use super::sys;

/// The label that ties a resource to a Mjolnir session.
pub const SESSION_LABEL: &str = "dev.mj.session";

pub fn disk_path(layout: &Layout, name: &str) -> PathBuf {
    layout.volume(name).join("disk.ext4")
}

/// Bytes on the filesystem that holds the volumes.
fn filesystem_bytes(layout: &Layout) -> Result<u64> {
    let path = std::ffi::CString::new(layout.volumes().as_os_str().as_bytes())?;
    // SAFETY: statvfs is plain old data and is filled by the call below.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stat` is a valid statvfs.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error()).context("statvfs the volume directory");
    }
    Ok(stat.f_blocks as u64 * stat.f_frsize as u64)
}

/// Create a volume. Fails if the name is taken.
pub fn create(layout: &Layout, name: &str, labels: BTreeMap<String, String>) -> Result<VolumeMetadata> {
    validate_name("volume", name)?;
    for (key, value) in &labels {
        validate_label(key, value)?;
    }
    layout.ensure()?;
    let _lock = layout.lock_volume(name)?;
    ensure!(
        VolumeMetadata::read(layout, name)?.is_none(),
        "volume {name} already exists"
    );
    let scratch = layout.volumes().join(format!(".tmp-{name}-{}", sys::scratch_suffix()));
    fs::create_dir(&scratch).with_context(|| format!("create {}", scratch.display()))?;
    let built = (|| -> Result<()> {
        let disk = scratch.join("disk.ext4");
        let file = fs::File::create(&disk)?;
        file.set_len(filesystem_bytes(layout)?)?;
        drop(file);
        sys::run_tool(
            "mkfs.ext4",
            &[
                "-q", "-F", "-m", "0",
                "-E", "lazy_itable_init=1,lazy_journal_init=1,nodiscard",
                disk.to_str().context("volume path is not UTF-8")?,
            ],
        )?;
        let metadata = VolumeMetadata {
            format: METADATA_FORMAT,
            name: name.to_owned(),
            created: sys::now_rfc3339(),
            labels: labels.clone(),
        };
        super::state::write_json_atomic(&scratch.join("labels.json"), &metadata)
    })();
    if let Err(error) = built {
        if let Err(cleanup) = fs::remove_dir_all(&scratch) {
            tracing::warn!(%cleanup, scratch = %scratch.display(), "could not remove volume scratch directory");
        }
        return Err(error.context(format!("create volume {name}")));
    }
    fs::rename(&scratch, layout.volume(name)).with_context(|| format!("publish volume {name}"))?;
    Ok(VolumeMetadata {
        format: METADATA_FORMAT,
        name: name.to_owned(),
        created: sys::now_rfc3339(),
        labels,
    })
}

/// What `run` needs of a volume: it exists, created now if it did not, and it
/// does not belong to another session. Returns whether this call created it.
pub fn ensure_for_run(
    layout: &Layout,
    name: &str,
    labels: &BTreeMap<String, String>,
) -> Result<bool> {
    match VolumeMetadata::read(layout, name)? {
        Some(existing) => {
            check_ownership(&existing, labels)?;
            Ok(false)
        }
        None => match create(layout, name, labels.clone()) {
            Ok(_) => Ok(true),
            // Another run created it between the check and the create; it
            // must pass the same ownership test.
            Err(_) if VolumeMetadata::read(layout, name)?.is_some() => {
                let existing = VolumeMetadata::read(layout, name)?.context("volume vanished")?;
                check_ownership(&existing, labels)?;
                Ok(false)
            }
            Err(error) => Err(error),
        },
    }
}

fn check_ownership(existing: &VolumeMetadata, wanted: &BTreeMap<String, String>) -> Result<()> {
    let theirs = existing.labels.get(SESSION_LABEL);
    let ours = wanted.get(SESSION_LABEL);
    if let (Some(theirs), Some(ours)) = (theirs, ours) {
        ensure!(
            theirs == ours,
            "refusing volume {}: it belongs to session {theirs}, not {ours}",
            existing.name
        );
    }
    if theirs.is_some() != ours.is_some() {
        bail!(
            "refusing volume {}: its {SESSION_LABEL} label does not match this container's",
            existing.name
        );
    }
    Ok(())
}

/// Remove a volume no VM uses.
pub fn remove(layout: &Layout, name: &str, ignore_missing: bool) -> Result<()> {
    validate_name("volume", name)?;
    let _lock = layout.lock_volume(name)?;
    if VolumeMetadata::read(layout, name)?.is_none() {
        if ignore_missing {
            return Ok(());
        }
        bail!("no volume with name {name:?} found: no such volume");
    }
    if let Some(user) = VmState::list(layout)?
        .into_iter()
        .find(|vm| vm.volumes.iter().any(|volume| volume.name == name))
    {
        bail!("volume {name} is being used by container {}", user.name);
    }
    // Unpublish first so a crash leaves scratch, not a volume without a disk.
    let doomed = layout.volumes().join(format!(".removing-{name}-{}", sys::scratch_suffix()));
    fs::rename(layout.volume(name), &doomed).with_context(|| format!("unpublish volume {name}"))?;
    fs::remove_dir_all(&doomed).with_context(|| format!("remove {}", doomed.display()))?;
    Ok(())
}
