//! OCI images as read-only ext4 base images.
//!
//! `crane` exports an image's filesystem and configuration without a
//! container engine. The engine extracts the filesystem as root (so
//! ownership survives), builds an ext4 image from it with `mkfs.ext4 -d`, and
//! keeps both next to the image's configuration. Each VM gets a reflink clone
//! of the base as its writable root disk. The image id is the SHA-256 of the
//! OCI configuration blob, the id Podman reports.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use mj_core::file_lock::FileLock;

use super::state::{ImageConfig, ImageRefs, Layout, METADATA_FORMAT, VmState};
use super::sys;

/// What `--pull` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullPolicy {
    Always,
    Missing,
    Never,
}

impl PullPolicy {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "always" | "newer" => Ok(Self::Always),
            "missing" => Ok(Self::Missing),
            "never" => Ok(Self::Never),
            other => {
                bail!("unsupported --pull value {other:?}; use always, missing, never or newer")
            }
        }
    }
}

/// Add the default tag so `alpine` and `alpine:latest` name one image.
pub fn normalize_reference(reference: &str) -> String {
    let name_start = reference.rfind('/').map(|i| i + 1).unwrap_or(0);
    if reference.contains('@') || reference[name_start..].contains(':') {
        reference.to_owned()
    } else {
        format!("{reference}:latest")
    }
}

/// Resolve `reference` to an image id, pulling according to `policy`.
pub fn ensure_image(
    layout: &Layout,
    reference: &str,
    platform: Option<&str>,
    policy: PullPolicy,
) -> Result<(String, FileLock)> {
    layout.ensure()?;
    if policy != PullPolicy::Always
        && let Some(id) = lookup(layout, reference)?
    {
        // Name the image and reserve it in one step: a prune decides under
        // this lock, so it cannot remove the image between the two.
        let lock = layout.lock_image(&id)?;
        if layout.image(&id).join("config.json").is_file() {
            return Ok((id, lock));
        }
        // Pruned while we waited for the lock.
        drop(lock);
    }
    if policy == PullPolicy::Never {
        bail!("{reference}: image not known");
    }
    pull_reserved(layout, reference, platform)
}

/// The id a reference or id names on this host, if any.
pub fn lookup(layout: &Layout, reference: &str) -> Result<Option<String>> {
    let refs = ImageRefs::read(layout)?;
    if let Some(id) = refs.refs.get(&normalize_reference(reference)) {
        return Ok(Some(id.clone()));
    }
    // A full image id, or an unambiguous prefix of one, also names an image.
    if reference.len() >= 4 && reference.bytes().all(|b| b.is_ascii_hexdigit()) {
        let mut matches = Vec::new();
        if let Ok(entries) = fs::read_dir(layout.images()) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(&reference.to_ascii_lowercase())
                    && layout.image(&name).join("config.json").is_file()
                {
                    matches.push(name);
                }
            }
        }
        if matches.len() == 1 {
            return Ok(matches.pop());
        }
    }
    Ok(None)
}

fn platform_for(requested: Option<&str>) -> Result<String> {
    let host = sys::host_architecture()?;
    let platform = requested
        .map(str::to_owned)
        .unwrap_or_else(|| format!("linux/{host}"));
    let (os, architecture) = platform
        .split_once('/')
        .with_context(|| format!("invalid platform {platform:?}; use os/architecture"))?;
    ensure!(
        os == "linux" && architecture == host,
        "platform {platform} cannot run on this host: a microVM runs {} only",
        format_args!("linux/{host}")
    );
    Ok(platform)
}

/// Fetch `reference` and flatten it unless this host already holds that exact image.
pub fn pull(layout: &Layout, reference: &str, platform: Option<&str>) -> Result<String> {
    pull_reserved(layout, reference, platform).map(|(id, _lock)| id)
}

/// [`pull`], returning the image's lock so the caller can keep the image from
/// being pruned until it has recorded its own use of it.
fn pull_reserved(
    layout: &Layout,
    reference: &str,
    platform: Option<&str>,
) -> Result<(String, FileLock)> {
    layout.ensure()?;
    let platform = platform_for(platform)?;
    let crane = sys::find_tool("crane")?;
    // Resolve a mutable tag once. The configuration and the filesystem are both
    // fetched by digest, so a tag that moves in between cannot pair one image's
    // configuration with another's files.
    let pinned = String::from_utf8(
        sys::run_checked(&crane, &["digest", "--full-ref", reference])
            .with_context(|| format!("resolve {reference}"))?
            .stdout,
    )
    .context("crane printed a digest that is not UTF-8")?
    .trim()
    .to_owned();
    let config_bytes = sys::run_checked(
        &crane,
        &["config", "--platform", platform.as_str(), pinned.as_str()],
    )
    .with_context(|| format!("fetch the configuration of {pinned}"))?
    .stdout;
    let id = hex(&Sha256::digest(&config_bytes));
    let config: serde_json::Value =
        serde_json::from_slice(&config_bytes).context("decode the OCI image configuration")?;
    // The image lock covers flattening and recording the reference, so a prune
    // (which decides under the same lock) never sees the image between the two.
    // Lock order everywhere is image, then global.
    let image_lock = layout.lock_image(&id)?;
    if !layout.image(&id).join("config.json").is_file() {
        flatten(layout, &crane, &pinned, &platform, &id, &config)
            .with_context(|| format!("flatten {reference}"))?;
    }
    let _global = layout.lock_global()?;
    let mut refs = ImageRefs::read(layout)?;
    refs.format = METADATA_FORMAT;
    refs.refs.insert(normalize_reference(reference), id.clone());
    refs.write(layout)?;
    Ok((id, image_lock))
}

fn first_number(output: &[u8]) -> Option<u64> {
    let field = output.split(|b| b.is_ascii_whitespace()).next()?;
    std::str::from_utf8(field).ok()?.parse().ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn strings(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn flatten(
    layout: &Layout,
    crane: &Path,
    reference: &str,
    platform: &str,
    id: &str,
    config: &serde_json::Value,
) -> Result<()> {
    let scratch = layout
        .images()
        .join(format!(".tmp-{id}-{}", sys::scratch_suffix()));
    fs::create_dir_all(scratch.join("rootfs"))
        .with_context(|| format!("create {}", scratch.display()))?;
    let result = flatten_into(&scratch, crane, reference, platform, id, config);
    if result.is_err() {
        // The scratch tree is ours alone; the processes that wrote it have exited.
        if let Err(error) = fs::remove_dir_all(&scratch) {
            tracing::warn!(%error, scratch = %scratch.display(), "could not remove image scratch directory");
        }
        return result;
    }
    fs::rename(&scratch, layout.image(id)).with_context(|| format!("publish image {id}"))?;
    Ok(())
}

fn flatten_into(
    scratch: &Path,
    crane: &Path,
    reference: &str,
    platform: &str,
    id: &str,
    config: &serde_json::Value,
) -> Result<()> {
    let rootfs = scratch.join("rootfs");
    extract(crane, reference, platform, scratch, &rootfs)?;
    for (directory, mode) in [
        ("proc", 0o755),
        ("sys", 0o755),
        ("dev", 0o755),
        ("run", 0o755),
        ("tmp", 0o1777),
    ] {
        let path = rootfs.join(directory);
        if !path.exists() {
            fs::create_dir(&path).with_context(|| format!("create {}", path.display()))?;
            fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(mode))?;
        }
    }
    let du = sys::find_tool("du")?;
    let rootfs_arg = rootfs.to_str().context("rootfs path is not UTF-8")?;
    let bytes =
        first_number(&sys::run_checked(&du, &["-sb", "--apparent-size", rootfs_arg])?.stdout)
            .context("read the size of the extracted image")?;
    let inodes = first_number(&sys::run_checked(&du, &["--inodes", "-s", rootfs_arg])?.stdout)
        .context("read the file count of the extracted image")?;
    // Room for the guest to install packages and write outside its volumes.
    let size = ((bytes + bytes / 4 + (4 << 30)) >> 20 << 20).max(64 << 20);
    let root_image = scratch.join("root.ext4");
    let file = fs::File::create(&root_image)?;
    file.set_len(size)?;
    drop(file);
    let inode_count = (inodes + inodes / 4 + 16384).to_string();
    sys::run_tool(
        "mkfs.ext4",
        &[
            "-q",
            "-F",
            "-m",
            "0",
            "-N",
            inode_count.as_str(),
            "-L",
            "rootfs",
            "-E",
            "root_owner=0:0,lazy_itable_init=1,lazy_journal_init=1,nodiscard",
            "-d",
            rootfs.to_str().context("rootfs path is not UTF-8")?,
            root_image.to_str().context("image path is not UTF-8")?,
        ],
    )?;
    fs::remove_dir_all(&rootfs).context("remove the extracted tree")?;
    let inner = config.get("config").cloned().unwrap_or_default();
    let labels = inner
        .get("Labels")
        .and_then(serde_json::Value::as_object)
        .map(|labels| {
            labels
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    let image = ImageConfig {
        format: METADATA_FORMAT,
        id: id.to_owned(),
        architecture: config
            .get("architecture")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        env: strings(&inner, "Env"),
        user: inner
            .get("User")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        working_dir: inner
            .get("WorkingDir")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        entrypoint: strings(&inner, "Entrypoint"),
        cmd: strings(&inner, "Cmd"),
        labels,
        root_bytes: size,
    };
    super::state::write_json_atomic(&scratch.join("config.json"), &image)?;
    Ok(())
}

/// `crane export | tar -x`. The two processes share a pipe the parent never
/// touches, and each one's diagnostics go to a file, so nothing here can block
/// on an unread pipe.
fn extract(
    crane: &Path,
    reference: &str,
    platform: &str,
    scratch: &Path,
    rootfs: &Path,
) -> Result<()> {
    let crane_log = scratch.join("crane.log");
    let tar_log = scratch.join("tar.log");
    let mut exporter = Command::new(crane)
        .args(["export", "--platform", platform, reference, "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(fs::File::create(&crane_log)?)
        .spawn()
        .context("start crane export")?;
    let pipe = exporter
        .stdout
        .take()
        .context("crane export has no output pipe")?;
    let tar = sys::find_tool("tar")?;
    let unpacker = Command::new(tar)
        .args([
            "-x",
            "--numeric-owner",
            "--xattrs",
            "--xattrs-include=*",
            "-p",
            "-C",
        ])
        .arg(rootfs)
        .stdin(Stdio::from(pipe))
        .stdout(Stdio::null())
        .stderr(fs::File::create(&tar_log)?)
        .status();
    let exported = exporter.wait().context("wait for crane export");
    let unpacked = unpacker.context("run tar")?;
    let exported = exported?;
    ensure!(
        exported.success(),
        "crane export failed ({exported}): {}",
        sys::tail(&fs::read_to_string(&crane_log).unwrap_or_default(), 10)
    );
    ensure!(
        unpacked.success(),
        "extracting the image failed ({unpacked}): {}",
        sys::tail(&fs::read_to_string(&tar_log).unwrap_or_default(), 10)
    );
    Ok(())
}

/// Remove images nothing refers to, and scratch left by interrupted pulls.
/// An image is in use if a reference names it or a VM was created from it.
pub fn prune(layout: &Layout) -> Result<Vec<String>> {
    layout.ensure()?;
    let mut removed = Vec::new();
    let mut names = Vec::new();
    for entry in fs::read_dir(layout.images()).context("list images")? {
        names.push(
            entry
                .context("list images")?
                .file_name()
                .to_string_lossy()
                .into_owned(),
        );
    }
    for name in names {
        if let Some(scratch) = name.strip_prefix(".tmp-") {
            // A pull holds its image's lock while it writes scratch, and the
            // scratch name carries the id; waiting for the lock waits for it.
            let id = scratch.split('-').next().unwrap_or_default();
            let Ok(_image_lock) = layout.lock_image(id) else {
                continue;
            };
            match fs::remove_dir_all(layout.images().join(&name)) {
                Ok(()) => {}
                // The pull we waited for published it.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).with_context(|| format!("remove {name}")),
            }
        } else if name.len() == 64 {
            // Decide under the image's lock, which pulls and runs hold from
            // before they name the image until they have recorded their use
            // of it. Lock order: image, then global.
            let _image_lock = layout.lock_image(&name)?;
            let _global = layout.lock_global()?;
            let refs = ImageRefs::read(layout)?;
            let in_use = refs.refs.values().any(|id| *id == name)
                || VmState::list(layout)?.iter().any(|vm| vm.image_id == name);
            if !in_use && layout.image(&name).exists() {
                fs::remove_dir_all(layout.image(&name))
                    .with_context(|| format!("remove image {name}"))?;
                removed.push(name);
            }
        }
    }
    let _global = layout.lock_global()?;
    removed.extend(super::boot::prune(layout)?);
    Ok(removed)
}

/// Paths to an image's parts.
pub fn root_image(layout: &Layout, id: &str) -> PathBuf {
    layout.image(id).join("root.ext4")
}
