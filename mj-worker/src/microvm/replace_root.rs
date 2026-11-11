//! Replace a stopped VM's root disk with a clone of another image, keeping
//! its volumes: the engine half of the controller's retained-storage image
//! upgrade.
//!
//! Podman's upgrade creates the replacement container while the old one still
//! exists, because two containers may mount one volume. Two kernels writing one
//! ext4 volume would corrupt it, so the engine never lets two VMs list a
//! volume; the replacement is the same VM with a different root disk. The old
//! root disk stays beside it as `root.previous` until the controller says the
//! new worker is ready, which is the role of Podman's renamed source container.
//!
//! Three verbs, each safe to repeat, each under the VM's lock:
//!
//! * `stage` records the target image in `replace-root.json` in the VM's
//!   directory. The record is the pin: `image prune` keeps the image while a
//!   record names it, and removing the VM removes the record.
//! * `apply` swaps the root disk of a stopped VM and gives the VM a new id (a
//!   container created from another image has another id, and the controller
//!   tells source from replacement by it). Every step is ordered so that a
//!   crash at any point leaves a state a second `apply` finishes.
//! * `release` ends the operation: it deletes the previous root disk if the
//!   swap happened, and the record either way.
//!
//! The record carries the id the VM will have after the swap, chosen when the
//! record is written. "Applied" is `state.id == record.new_id`, a fact in the
//! one file that `apply` writes last, so no pair of files has to agree.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};

use super::image;
use super::state::{
    Layout, VmState, refuse_newer_format, validate_label, validate_name, write_json_atomic,
};
use super::{sys, vm};

/// The format of `replace-root.json` this build writes.
const RECORD_FORMAT: u32 = 1;
const RECORD: &str = "replace-root.json";
const ROOT: &str = "root.ext4";
/// The clone of the target image, until it takes the place of the root disk.
const NEXT: &str = "root.next";
/// The root disk the VM had before the swap.
const PREVIOUS: &str = "root.previous";

/// What `stage` records.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Record {
    format: u32,
    /// The controller's operation id: every verb names it.
    operation: String,
    image_id: String,
    /// The reference the controller asked for, shown by `inspect` afterwards.
    image_reference: String,
    /// Merged into the VM's labels by the swap.
    labels: BTreeMap<String, String>,
    /// The VM's id once the swap has happened.
    new_id: String,
}

fn record_path(layout: &Layout, name: &str) -> PathBuf {
    layout.vm(name).join(RECORD)
}

fn read_record(layout: &Layout, name: &str) -> Result<Option<Record>> {
    let path = record_path(layout, name);
    match fs::read(&path) {
        Ok(bytes) => {
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("decode {}", path.display()))?;
            refuse_newer_format("root replacement record", &value, RECORD_FORMAT)?;
            serde_json::from_value(value)
                .with_context(|| format!("decode {}", path.display()))
                .map(Some)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// The image ids that a staged replacement still needs. `image prune` calls
/// this under the image's lock, which `stage` also holds while it writes the
/// record, so a prune cannot miss a record being written.
pub fn pinned_images(layout: &Layout) -> Result<BTreeSet<String>> {
    let mut pinned = BTreeSet::new();
    for state in VmState::list(layout)? {
        if let Some(record) = read_record(layout, &state.name)? {
            pinned.insert(record.image_id);
        }
    }
    Ok(pinned)
}

fn no_such_container(name: &str) -> anyhow::Error {
    anyhow!("no container with name or ID {name:?} found: no such container")
}

fn read_state(layout: &Layout, name: &str) -> Result<VmState> {
    VmState::read(layout, name)?.ok_or_else(|| no_such_container(name))
}

/// Record the replacement of `name`'s root disk by the image `reference`
/// names, and pin that image. Returns the image id.
pub fn stage(
    layout: &Layout,
    name: &str,
    operation: &str,
    reference: &str,
    labels: &BTreeMap<String, String>,
) -> Result<String> {
    validate_name("container", name)?;
    validate_operation(operation)?;
    for (key, value) in labels {
        validate_label(key, value)?;
    }
    let _vm_lock = layout.lock_vm(name)?;
    read_state(layout, name)?;
    let id =
        image::lookup(layout, reference)?.ok_or_else(|| anyhow!("{reference}: image not known"))?;
    // Held while the record is written: a prune decides under this lock.
    let _image_lock = layout.lock_image(&id)?;
    ensure!(
        layout.image(&id).join("config.json").is_file(),
        "{reference}: image not known"
    );
    if let Some(existing) = read_record(layout, name)? {
        ensure!(
            existing.operation == operation,
            "container {name} already has a root replacement in progress (operation {}); release it first",
            existing.operation
        );
        ensure!(
            existing.image_id == id,
            "operation {operation} staged another image for container {name}"
        );
        return Ok(id);
    }
    write_json_atomic(
        &record_path(layout, name),
        &Record {
            format: RECORD_FORMAT,
            operation: operation.to_owned(),
            image_id: id.clone(),
            image_reference: reference.to_owned(),
            labels: labels.clone(),
            new_id: vm::random_id()?,
        },
    )?;
    Ok(id)
}

fn validate_operation(operation: &str) -> Result<()> {
    ensure!(
        !operation.is_empty()
            && operation.len() <= 128
            && operation
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')),
        "invalid operation id {operation:?}"
    );
    Ok(())
}

/// The VM as an observer must see it: once the old root disk has been set
/// aside, the swap has started and the VM is the replacement, whether or not
/// the crash that interrupted it came before `state.json` was written. The
/// controller tells source from replacement by this id; showing the source
/// there would send recovery to capture control state from a half-swapped root.
pub fn observed(layout: &Layout, name: &str, mut state: VmState) -> Result<VmState> {
    if let Some(record) = read_record(layout, name)?
        && state.id != record.new_id
        && layout.vm(name).join(PREVIOUS).exists()
    {
        state.id = record.new_id;
        state.image_id = record.image_id;
        state.image_reference = record.image_reference;
        state.labels.extend(record.labels);
    }
    Ok(state)
}

/// Finish a swap that a crash interrupted, before the VM is started. Without
/// a swap in progress this does nothing.
pub fn resume_interrupted(layout: &Layout, name: &str) -> Result<()> {
    validate_name("container", name)?;
    let _vm_lock = layout.lock_vm(name)?;
    let Some(state) = VmState::read(layout, name)? else {
        return Ok(());
    };
    let Some(record) = read_record(layout, name)? else {
        return Ok(());
    };
    if state.id != record.new_id && layout.vm(name).join(PREVIOUS).exists() {
        ensure!(
            vm::activity(name)? == vm::Activity::Stopped,
            "container {name} is running with an interrupted root replacement"
        );
        apply_stopped(layout, name, &record.operation, &vm::reflink)?;
    }
    Ok(())
}

/// Swap the root disk of the stopped VM `name`. Returns the VM's new id.
pub fn apply(layout: &Layout, name: &str, operation: &str) -> Result<String> {
    validate_name("container", name)?;
    validate_operation(operation)?;
    ensure!(
        sys::is_root(),
        "the microvm engine must run as root (use sudo -n)"
    );
    let _vm_lock = layout.lock_vm(name)?;
    ensure!(
        vm::activity(name)? == vm::Activity::Stopped,
        "container {name} is running; stop it before replacing its root disk"
    );
    apply_stopped(layout, name, operation, &vm::reflink)
}

/// `apply` after the checks that need the host. `clone` copies an image's root
/// disk to a new file; tests give it a plain copy.
fn apply_stopped(
    layout: &Layout,
    name: &str,
    operation: &str,
    clone: &dyn Fn(&Path, &Path) -> Result<()>,
) -> Result<String> {
    let mut state = read_state(layout, name)?;
    let record = read_record(layout, name)?
        .ok_or_else(|| anyhow!("container {name} has no staged root replacement"))?;
    ensure!(
        record.operation == operation,
        "container {name} stages operation {}, not {operation}",
        record.operation
    );
    if state.id == record.new_id {
        return Ok(state.id);
    }
    let directory = layout.vm(name);
    let (root, next, previous) = (
        directory.join(ROOT),
        directory.join(NEXT),
        directory.join(PREVIOUS),
    );
    if !previous.exists() {
        // The swap has not started. A half-written clone from an interrupted
        // attempt is scratch.
        remove_if_present(&next)?;
        {
            let _image_lock = layout.lock_image(&record.image_id)?;
            ensure!(
                layout.image(&record.image_id).join("config.json").is_file(),
                "the staged image {} is gone; the staging record should have pinned it",
                record.image_id
            );
            clone(&image::root_image(layout, &record.image_id), &next)?;
        }
        sync_file(&next)?;
        fs::rename(&root, &previous).with_context(|| format!("set aside {}", root.display()))?;
    }
    // From here the old root disk is `previous` and `next` is complete; a
    // second `apply` continues exactly here.
    if next.exists() {
        fs::rename(&next, &root).with_context(|| format!("install {}", root.display()))?;
    }
    ensure!(
        root.is_file(),
        "container {name} has no root disk after the swap; {} holds the old one",
        previous.display()
    );
    sync_directory(&directory)?;
    state.id = record.new_id.clone();
    state.image_id = record.image_id;
    state.image_reference = record.image_reference;
    state.labels.extend(record.labels);
    state.write(layout)?;
    Ok(state.id)
}

/// End the operation: delete the previous root disk if the swap happened, and
/// the record. A container that no longer exists has nothing to release.
pub fn release(layout: &Layout, name: &str, operation: &str) -> Result<()> {
    validate_name("container", name)?;
    validate_operation(operation)?;
    let _vm_lock = layout.lock_vm(name)?;
    let Some(state) = VmState::read(layout, name)? else {
        return Ok(());
    };
    let Some(record) = read_record(layout, name)? else {
        return Ok(());
    };
    ensure!(
        record.operation == operation,
        "container {name} stages operation {}, not {operation}",
        record.operation
    );
    let directory = layout.vm(name);
    let previous = directory.join(PREVIOUS);
    if state.id == record.new_id {
        remove_if_present(&previous)?;
    } else {
        // Never swapped, or interrupted mid-swap; only the former may be
        // released, because the old root disk is the only one in the latter.
        ensure!(
            !previous.exists(),
            "the root replacement of container {name} was interrupted; apply it again"
        );
        remove_if_present(&directory.join(NEXT))?;
    }
    remove_if_present(&record_path(layout, name))?;
    sync_directory(&directory)
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn sync_file(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("sync {}", path.display()))
}

fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::microvm::state::{ImageRefs, STATE_FORMAT, VmVolume};

    const OLD_IMAGE: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const NEW_IMAGE: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn copy(source: &Path, destination: &Path) -> Result<()> {
        fs::copy(source, destination)?;
        Ok(())
    }

    /// A VM named `t1` on `OLD_IMAGE` with a workspace volume, and `NEW_IMAGE`
    /// known under the reference `agent:new`.
    fn host() -> (tempfile::TempDir, Layout) {
        let directory = tempfile::tempdir().unwrap();
        let layout = Layout::new(directory.path());
        layout.ensure().unwrap();
        for (id, content) in [(OLD_IMAGE, "old image"), (NEW_IMAGE, "new image")] {
            fs::create_dir_all(layout.image(id)).unwrap();
            fs::write(layout.image(id).join("root.ext4"), content).unwrap();
            fs::write(layout.image(id).join("config.json"), "{}").unwrap();
        }
        let mut refs = ImageRefs::default();
        refs.refs
            .insert("agent:new".to_owned(), NEW_IMAGE.to_owned());
        refs.refs
            .insert("agent:old".to_owned(), OLD_IMAGE.to_owned());
        refs.write(&layout).unwrap();
        fs::create_dir_all(layout.vm("t1")).unwrap();
        fs::write(layout.vm("t1").join(ROOT), "old root with session edits").unwrap();
        VmState {
            format: STATE_FORMAT,
            id: "a".repeat(64),
            name: "t1".to_owned(),
            created: "2026-11-11T00:00:00Z".to_owned(),
            image_reference: "agent:old".to_owned(),
            image_id: OLD_IMAGE.to_owned(),
            labels: BTreeMap::from([("dev.mj.session".to_owned(), "s1".to_owned())]),
            vcpus: 2,
            nano_cpus: 2_000_000_000,
            memory_bytes: 4 << 30,
            env: Vec::new(),
            user: "0:0".to_owned(),
            working_dir: "/workspace".to_owned(),
            entrypoint: Vec::new(),
            cmd: vec!["sleep".to_owned(), "infinity".to_owned()],
            volumes: vec![VmVolume {
                name: "t1-workspace".to_owned(),
                destination: "/workspace".to_owned(),
                read_write: true,
                chown: true,
            }],
            slot: 0,
            engine_build: "test".to_owned(),
        }
        .write(&layout)
        .unwrap();
        (directory, layout)
    }

    fn labels() -> BTreeMap<String, String> {
        BTreeMap::from([("dev.mj.container-upgrade".to_owned(), "op1".to_owned())])
    }

    fn root(layout: &Layout) -> String {
        fs::read_to_string(layout.vm("t1").join(ROOT)).unwrap()
    }

    // The swap's steps, in order, so a test can stop after any of them the way
    // a crash would.
    fn crash_after(layout: &Layout, steps: usize) {
        let directory = layout.vm("t1");
        let record = read_record(layout, "t1").unwrap().unwrap();
        if steps >= 1 {
            copy(
                &image::root_image(layout, &record.image_id),
                &directory.join(NEXT),
            )
            .unwrap();
        }
        if steps >= 2 {
            fs::rename(directory.join(ROOT), directory.join(PREVIOUS)).unwrap();
        }
        if steps >= 3 {
            fs::rename(directory.join(NEXT), directory.join(ROOT)).unwrap();
        }
    }

    // A crash between any two steps of the swap is finished by a second apply,
    // which then changes nothing further, and release keeps the new root only.
    #[test]
    fn apply_finishes_after_a_crash_between_any_two_steps() {
        for steps in 0..=3 {
            let (_directory, layout) = host();
            stage(&layout, "t1", "op1", "agent:new", &labels()).unwrap();
            crash_after(&layout, steps);
            let seen = observed(
                &layout,
                "t1",
                VmState::read(&layout, "t1").unwrap().unwrap(),
            )
            .unwrap();
            let record = read_record(&layout, "t1").unwrap().unwrap();
            assert_eq!(seen.id == record.new_id, steps >= 2, "after {steps} steps");
            let id = apply_stopped(&layout, "t1", "op1", &copy).unwrap();
            let state = VmState::read(&layout, "t1").unwrap().unwrap();
            assert_eq!(state.id, id, "after {steps} steps");
            assert_ne!(id, "a".repeat(64));
            assert_eq!(
                (state.image_id.as_str(), state.image_reference.as_str()),
                (NEW_IMAGE, "agent:new")
            );
            assert_eq!(state.labels["dev.mj.session"], "s1");
            assert_eq!(state.labels["dev.mj.container-upgrade"], "op1");
            assert_eq!(state.volumes.len(), 1);
            assert_eq!(root(&layout), "new image", "after {steps} steps");
            assert_eq!(
                fs::read_to_string(layout.vm("t1").join(PREVIOUS)).unwrap(),
                "old root with session edits"
            );
            assert_eq!(apply_stopped(&layout, "t1", "op1", &copy).unwrap(), id);

            release(&layout, "t1", "op1").unwrap();
            assert!(!layout.vm("t1").join(PREVIOUS).exists());
            assert!(!layout.vm("t1").join(NEXT).exists());
            assert!(read_record(&layout, "t1").unwrap().is_none());
            release(&layout, "t1", "op1").unwrap();
            assert_eq!(root(&layout), "new image");
        }
    }

    // A release before the swap (a cancelled upgrade) drops the pin and leaves
    // the VM alone; a release of a swap that was interrupted is refused,
    // because the old root disk is then the only one.
    #[test]
    fn release_keeps_the_only_root_disk_of_an_interrupted_swap() {
        let (_directory, layout) = host();
        stage(&layout, "t1", "op1", "agent:new", &labels()).unwrap();
        assert!(pinned_images(&layout).unwrap().contains(NEW_IMAGE));
        crash_after(&layout, 2);
        let error = release(&layout, "t1", "op1").unwrap_err();
        assert!(format!("{error:#}").contains("interrupted"), "{error:#}");
        assert!(layout.vm("t1").join(PREVIOUS).exists());

        let (_directory, layout) = host();
        stage(&layout, "t1", "op1", "agent:new", &labels()).unwrap();
        release(&layout, "t1", "op1").unwrap();
        assert!(pinned_images(&layout).unwrap().is_empty());
        assert_eq!(root(&layout), "old root with session edits");
    }

    // Stage is repeatable for its own operation and refuses another one's
    // image or a second operation while one is in progress.
    #[test]
    fn stage_is_idempotent_and_refuses_a_second_operation() {
        let (_directory, layout) = host();
        let first = stage(&layout, "t1", "op1", "agent:new", &labels()).unwrap();
        let record = read_record(&layout, "t1").unwrap().unwrap();
        assert_eq!(
            stage(&layout, "t1", "op1", "agent:new", &labels()).unwrap(),
            first
        );
        assert_eq!(read_record(&layout, "t1").unwrap().unwrap(), record);
        assert!(stage(&layout, "t1", "op1", "agent:old", &labels()).is_err());
        let error = stage(&layout, "t1", "op2", "agent:new", &labels()).unwrap_err();
        assert!(format!("{error:#}").contains("in progress"), "{error:#}");
        assert!(apply_stopped(&layout, "t1", "op2", &copy).is_err());
        assert!(stage(&layout, "absent", "op1", "agent:new", &labels()).is_err());
    }
}
