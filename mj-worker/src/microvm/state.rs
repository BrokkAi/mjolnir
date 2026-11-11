//! The engine's on-disk state: layout, formats, names, and locks.
//!
//! VMs and their guest agents outlive engine upgrades, so every file here is
//! versioned and a newer engine reads every older format. An older engine
//! that meets a newer format refuses to touch it and says why.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use mj_core::file_lock::FileLock;
use serde::{Deserialize, Serialize};

/// The format of `state.json` this build writes.
pub const STATE_FORMAT: u32 = 1;
/// The format of image and volume metadata files this build writes.
pub const METADATA_FORMAT: u32 = 1;

/// The engine's state root.
pub const DEFAULT_ROOT: &str = "/var/lib/mj-microvm";

/// Where everything lives.
#[derive(Clone, Debug)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn system() -> Self {
        Self::new(DEFAULT_ROOT)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn images(&self) -> PathBuf {
        self.root.join("images")
    }

    pub fn image(&self, id: &str) -> PathBuf {
        self.images().join(id)
    }

    pub fn image_refs(&self) -> PathBuf {
        self.images().join("refs.json")
    }

    pub fn vms(&self) -> PathBuf {
        self.root.join("vms")
    }

    pub fn vm(&self, name: &str) -> PathBuf {
        self.vms().join(name)
    }

    pub fn volumes(&self) -> PathBuf {
        self.root.join("volumes")
    }

    pub fn volume(&self, name: &str) -> PathBuf {
        self.volumes().join(name)
    }

    pub fn boot(&self) -> PathBuf {
        self.root.join("boot")
    }

    pub fn locks(&self) -> PathBuf {
        self.root.join("locks")
    }

    /// Create the directory tree. Safe to repeat.
    pub fn ensure(&self) -> Result<()> {
        for directory in [
            self.root.clone(),
            self.images(),
            self.vms(),
            self.volumes(),
            self.boot(),
            self.locks(),
        ] {
            fs::create_dir_all(&directory)
                .with_context(|| format!("create {}", directory.display()))?;
        }
        // Flattened images hold setuid binaries and guest data: nobody but root
        // may traverse the state tree, whatever the umask was.
        fs::set_permissions(
            &self.root,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .with_context(|| format!("restrict {}", self.root.display()))?;
        Ok(())
    }

    /// The engine-wide lock: slot allocation, admission, image references.
    pub fn lock_global(&self) -> Result<FileLock> {
        self.lock_file("global.lock")
    }

    /// The lock for one VM name, held across every mutation of that VM.
    pub fn lock_vm(&self, name: &str) -> Result<FileLock> {
        validate_name("container", name)?;
        self.lock_file(&format!("vm-{name}.lock"))
    }

    /// The lock for one volume name.
    pub fn lock_volume(&self, name: &str) -> Result<FileLock> {
        validate_name("volume", name)?;
        self.lock_file(&format!("volume-{name}.lock"))
    }

    /// The lock for one image id.
    pub fn lock_image(&self, id: &str) -> Result<FileLock> {
        ensure!(
            id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid image id {id:?}"
        );
        self.lock_file(&format!("image-{id}.lock"))
    }

    /// Reserve a boot-assets directory: held (shared) from before a launch
    /// names the assets until Firecracker has opened them, so a prune by another
    /// engine build cannot remove them in between.
    pub fn lock_boot_shared(&self, tag: &str) -> Result<FileLock> {
        let file = self.open_lock_file(&format!("boot-{tag}.lock"))?;
        file.lock_shared()
            .with_context(|| format!("lock boot assets {tag}"))?;
        Ok(FileLock::new(file))
    }

    /// The exclusive lock on boot assets, or `None` while a launch holds them.
    pub fn try_lock_boot_exclusive(&self, tag: &str) -> Result<Option<FileLock>> {
        let file = self.open_lock_file(&format!("boot-{tag}.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(FileLock::new(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("lock boot assets {tag}"))
            }
        }
    }

    fn open_lock_file(&self, file_name: &str) -> Result<File> {
        fs::create_dir_all(self.locks())
            .with_context(|| format!("create {}", self.locks().display()))?;
        let path = self.locks().join(file_name);
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open lock {}", path.display()))
    }

    /// Lock the named file. A resource's lock file is deleted with the
    /// resource (see the `forget_*` methods), by a holder of the lock; a
    /// locker that was waiting then holds a lock on an unlinked file, which
    /// excludes nobody. So after acquiring, check that the file held is still
    /// the one at the path, and start over if not. Each retry follows a delete
    /// by another process, so the loop cannot spin.
    fn lock_file(&self, file_name: &str) -> Result<FileLock> {
        let path = self.locks().join(file_name);
        loop {
            let file = self.open_lock_file(file_name)?;
            file.lock()
                .with_context(|| format!("lock {}", path.display()))?;
            if lock_file_is_linked(&file, &path)? {
                return Ok(FileLock::new(file));
            }
        }
    }

    /// Delete a VM's lock file with the VM. The caller holds the VM's lock and
    /// must do nothing more to the VM afterwards: a locker arriving after the
    /// delete creates a new file and does not wait for the holder.
    pub fn forget_vm_lock(&self, name: &str, held: &FileLock) -> Result<()> {
        self.forget_lock(&format!("vm-{name}.lock"), held)
    }

    /// Delete a volume's lock file with the volume. The caller holds its lock.
    pub fn forget_volume_lock(&self, name: &str, held: &FileLock) -> Result<()> {
        self.forget_lock(&format!("volume-{name}.lock"), held)
    }

    /// Delete an image's lock file with the image. The caller holds its lock.
    pub fn forget_image_lock(&self, id: &str, held: &FileLock) -> Result<()> {
        self.forget_lock(&format!("image-{id}.lock"), held)
    }

    /// Unlink a lock file while its lock is held; `held` is the proof.
    fn forget_lock(&self, file_name: &str, _held: &FileLock) -> Result<()> {
        let path = self.locks().join(file_name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
        }
    }
}

/// Whether `file` is still the file at `path` (same device and inode).
fn lock_file_is_linked(file: &File, path: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let held = file
        .metadata()
        .with_context(|| format!("stat lock {}", path.display()))?;
    match fs::metadata(path) {
        Ok(linked) => Ok(linked.dev() == held.dev() && linked.ino() == held.ino()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("stat lock {}", path.display())),
    }
}

/// Container and volume names reach file paths, unit names and kernel
/// command lines, so they are restricted to a safe alphabet.
pub fn validate_name(kind: &str, name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    ensure!(
        valid,
        "invalid {kind} name {name:?}: use 1 to 63 letters, digits, '_', '.' or '-', starting with a letter or digit"
    );
    Ok(())
}

/// Labels are metadata a person or the controller attaches; keys are restricted
/// the way container engines restrict them.
pub fn validate_label(key: &str, value: &str) -> Result<()> {
    ensure!(
        !key.is_empty()
            && key.len() <= 128
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'/')),
        "invalid label key {key:?}"
    );
    ensure!(
        !value.contains('\n') && !value.contains('\0'),
        "label {key} has an invalid value"
    );
    Ok(())
}

/// A volume mounted into a VM.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VmVolume {
    pub name: String,
    pub destination: String,
    pub read_write: bool,
    /// Podman's `U` option.
    #[serde(default)]
    pub chown: bool,
}

/// Everything the engine remembers about a VM. Fields added in later formats
/// carry `serde(default)` so older files still load.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VmState {
    pub format: u32,
    pub id: String,
    pub name: String,
    pub created: String,
    pub image_reference: String,
    pub image_id: String,
    pub labels: BTreeMap<String, String>,
    pub vcpus: u32,
    pub nano_cpus: i64,
    pub memory_bytes: u64,
    /// Environment of the container: the image's plus `--env`.
    pub env: Vec<String>,
    pub user: String,
    pub working_dir: String,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub volumes: Vec<VmVolume>,
    /// Network slot: selects the TAP device and the /30 subnet.
    pub slot: u32,
    /// The engine build that created the VM.
    pub engine_build: String,
}

impl VmState {
    pub fn path(layout: &Layout, name: &str) -> PathBuf {
        layout.vm(name).join("state.json")
    }

    pub fn write(&self, layout: &Layout) -> Result<()> {
        write_json_atomic(&Self::path(layout, &self.name), self)
    }

    /// The state of `name`, or `None` when there is no such VM.
    pub fn read(layout: &Layout, name: &str) -> Result<Option<Self>> {
        let path = Self::path(layout, name);
        match fs::read(&path) {
            Ok(bytes) => Self::parse(&bytes)
                .with_context(|| format!("read {}", path.display()))
                .map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Decode a `state.json`, refusing a format newer than this build knows.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let value: serde_json::Value =
            serde_json::from_slice(bytes).context("decode state.json")?;
        refuse_newer_format("VM state", &value, STATE_FORMAT)?;
        serde_json::from_value(value).context("decode state.json")
    }

    /// Every VM with a state file, in name order.
    pub fn list(layout: &Layout) -> Result<Vec<Self>> {
        let mut states = Vec::new();
        let entries = match fs::read_dir(layout.vms()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(states),
            Err(error) => {
                return Err(error).with_context(|| format!("list {}", layout.vms().display()));
            }
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.context("list VMs")?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // Dot directories are removal and creation scratch space, not VMs.
            if !name.starts_with('.') {
                names.push(name);
            }
        }
        names.sort();
        for name in names {
            if let Some(state) = Self::read(layout, &name)? {
                states.push(state);
            }
        }
        Ok(states)
    }
}

/// Fail with a clear message when a file was written by a newer engine.
pub fn refuse_newer_format(what: &str, value: &serde_json::Value, supported: u32) -> Result<()> {
    let format = value
        .get("format")
        .and_then(serde_json::Value::as_u64)
        .context("the file has no format number")?;
    if format > u64::from(supported) {
        bail!(
            "{what} format {format} was written by a newer mj-worker than this one (it understands up to format {supported}); install the newer mj-worker on this host"
        );
    }
    Ok(())
}

/// Write `value` to `path` through a temporary file in the same directory.
pub fn write_json_atomic<V: Serialize>(path: &Path, value: &V) -> Result<()> {
    let directory = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    let temporary = directory.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
    ));
    let mut file =
        File::create(&temporary).with_context(|| format!("create {}", temporary.display()))?;
    serde_json::to_writer_pretty(&mut file, value)
        .with_context(|| format!("write {}", temporary.display()))?;
    file.write_all(b"\n")?;
    file.sync_all()
        .with_context(|| format!("sync {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("publish {}", path.display()))?;
    Ok(())
}

/// A volume's metadata.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeMetadata {
    pub format: u32,
    pub name: String,
    pub created: String,
    pub labels: BTreeMap<String, String>,
}

impl VolumeMetadata {
    pub fn path(layout: &Layout, name: &str) -> PathBuf {
        layout.volume(name).join("labels.json")
    }

    pub fn read(layout: &Layout, name: &str) -> Result<Option<Self>> {
        let path = Self::path(layout, name);
        match fs::read(&path) {
            Ok(bytes) => {
                let value: serde_json::Value = serde_json::from_slice(&bytes)
                    .with_context(|| format!("decode {}", path.display()))?;
                refuse_newer_format("volume metadata", &value, METADATA_FORMAT)?;
                serde_json::from_value(value)
                    .with_context(|| format!("decode {}", path.display()))
                    .map(Some)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn write(&self, layout: &Layout) -> Result<()> {
        write_json_atomic(&Self::path(layout, &self.name), self)
    }

    pub fn list(layout: &Layout) -> Result<Vec<Self>> {
        let mut volumes = Vec::new();
        let entries = match fs::read_dir(layout.volumes()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(volumes),
            Err(error) => {
                return Err(error).with_context(|| format!("list {}", layout.volumes().display()));
            }
        };
        let mut names: Vec<String> = Vec::new();
        for entry in entries {
            let name = entry
                .context("list volumes")?
                .file_name()
                .to_string_lossy()
                .into_owned();
            if !name.starts_with('.') {
                names.push(name);
            }
        }
        names.sort();
        for name in names {
            if let Some(volume) = Self::read(layout, &name)? {
                volumes.push(volume);
            }
        }
        Ok(volumes)
    }
}

/// The OCI image configuration kept next to a flattened image.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ImageConfig {
    pub format: u32,
    pub id: String,
    pub architecture: String,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub working_dir: String,
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Size of the flattened root filesystem image in bytes.
    #[serde(default)]
    pub root_bytes: u64,
}

impl ImageConfig {
    pub fn read(layout: &Layout, id: &str) -> Result<Self> {
        let path = layout.image(id).join("config.json");
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).with_context(|| format!("decode {}", path.display()))?;
        refuse_newer_format("image metadata", &value, METADATA_FORMAT)?;
        serde_json::from_value(value).with_context(|| format!("decode {}", path.display()))
    }
}

/// Reference name to image id.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ImageRefs {
    #[serde(default)]
    pub format: u32,
    #[serde(default)]
    pub refs: BTreeMap<String, String>,
}

impl ImageRefs {
    pub fn read(layout: &Layout) -> Result<Self> {
        let path = layout.image_refs();
        match fs::read(&path) {
            Ok(bytes) => {
                let value: serde_json::Value = serde_json::from_slice(&bytes)
                    .with_context(|| format!("decode {}", path.display()))?;
                refuse_newer_format("image reference", &value, METADATA_FORMAT)?;
                serde_json::from_value(value).with_context(|| format!("decode {}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                format: METADATA_FORMAT,
                refs: BTreeMap::new(),
            }),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn write(&self, layout: &Layout) -> Result<()> {
        write_json_atomic(&layout.image_refs(), self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str) -> VmState {
        VmState {
            format: STATE_FORMAT,
            id: "a".repeat(64),
            name: name.to_owned(),
            created: "2026-11-10T00:00:00Z".to_owned(),
            image_reference: "ghcr.io/example/image:latest".to_owned(),
            image_id: "b".repeat(64),
            labels: BTreeMap::from([("dev.mj.session".to_owned(), "s1".to_owned())]),
            vcpus: 2,
            nano_cpus: 2_000_000_000,
            memory_bytes: 4 << 30,
            env: vec!["A=1".to_owned()],
            user: "0:0".to_owned(),
            working_dir: "/workspace".to_owned(),
            entrypoint: Vec::new(),
            cmd: vec!["sleep".to_owned(), "infinity".to_owned()],
            volumes: vec![VmVolume {
                name: "t1-ws".to_owned(),
                destination: "/workspace".to_owned(),
                read_write: true,
                chown: false,
            }],
            slot: 3,
            engine_build: "test".to_owned(),
        }
    }

    #[test]
    fn state_written_by_this_build_reads_back_and_a_newer_format_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let layout = Layout::new(directory.path());
        layout.ensure().unwrap();
        fs::create_dir_all(layout.vm("t1")).unwrap();
        let state = sample("t1");
        state.write(&layout).unwrap();
        assert_eq!(VmState::read(&layout, "t1").unwrap(), Some(state.clone()));
        assert_eq!(VmState::read(&layout, "absent").unwrap(), None);

        // A file from a future engine: unknown fields are not ours to drop.
        let mut newer = serde_json::to_value(&state).unwrap();
        newer["format"] = serde_json::json!(STATE_FORMAT + 1);
        newer["quantum_flux"] = serde_json::json!(true);
        fs::write(
            VmState::path(&layout, "t1"),
            serde_json::to_vec(&newer).unwrap(),
        )
        .unwrap();
        let error = format!("{:#}", VmState::read(&layout, "t1").unwrap_err());
        assert!(
            error.contains("newer mj-worker")
                && error.contains(&format!("format {}", STATE_FORMAT + 1)),
            "{error}"
        );
    }

    #[test]
    fn deleting_a_lock_file_while_held_never_lets_two_holders_in() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let directory = tempfile::tempdir().unwrap();
        let layout = Arc::new(Layout::new(directory.path()));
        layout.ensure().unwrap();
        let inside = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let (layout, inside) = (Arc::clone(&layout), Arc::clone(&inside));
                std::thread::spawn(move || {
                    for round in 0..150 {
                        let lock = layout.lock_vm("racing").unwrap();
                        // A holder on an unlinked file would see another inside.
                        assert_eq!(inside.fetch_add(1, Ordering::SeqCst), 0, "two holders");
                        std::thread::yield_now();
                        // Deleting the file is the holder's last act: a locker that
                        // arrives after it creates a new file and may enter at once.
                        inside.fetch_sub(1, Ordering::SeqCst);
                        if (worker + round) % 3 == 0 {
                            layout.forget_vm_lock("racing", &lock).unwrap();
                        }
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let lock = layout.lock_vm("racing").unwrap();
        layout.forget_vm_lock("racing", &lock).unwrap();
        drop(lock);
        assert!(!layout.locks().join("vm-racing.lock").exists());
    }
}
