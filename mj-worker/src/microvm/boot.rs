//! The kernel and initrd every microVM boots.
//!
//! The kernel is the host's own EFI-stub `vmlinuz`. The initrd is built with
//! dracut for each `(kernel version, engine build)` pair: it carries the
//! guest `/init` script and a copy of this binary, which `/init` starts as
//! the guest agent, so the agent always matches the engine that launched the
//! VM.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use super::state::Layout;
use super::sys;

const INIT_SCRIPT: &str = include_str!("guest-init.sh");

/// Kernel modules the initrd carries: the boot, disk, vsock and network set
/// first, then what nested containers and nftables use inside the guest.
const GUEST_DRIVERS: &str = "virtio_pci virtio_blk virtio_net ext4 vmw_vsock_virtio_transport nsm overlay fuse tun veth bridge br_netfilter nf_tables nft_ct nft_nat nft_masq nft_chain_nat nft_compat nf_conntrack nf_nat nft_reject_inet nft_reject nf_reject_ipv4 xt_MASQUERADE xt_conntrack xt_addrtype xt_comment xt_mark xt_nat iptable_nat";

/// Programs `/init` runs.
const INIT_PROGRAMS: &str = "mount umount modprobe ip switch_root mkdir hostname cat cut sleep cp ls seq grep head tee echo ln chmod chown stat";

/// The assets directory name for this kernel and engine build.
fn current_tag() -> Result<String> {
    Ok(format!(
        "{}-{}",
        sys::kernel_release()?,
        mj_core::worker_build::WORKER_BUILD_ID
    ))
}

/// Remove boot assets built for other kernels or engine builds, and scratch
/// from interrupted builds. Running VMs have already loaded theirs; a later
/// `start` rebuilds what it needs. The caller holds the global lock, which
/// is also what builders hold while they write scratch.
pub fn prune(layout: &Layout) -> Result<Vec<String>> {
    let current = current_tag()?;
    let mut removed = Vec::new();
    let entries = match fs::read_dir(layout.boot()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(removed),
        Err(error) => {
            return Err(error).with_context(|| format!("list {}", layout.boot().display()));
        }
    };
    for entry in entries {
        let entry = entry.context("list boot assets")?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != current {
            fs::remove_dir_all(entry.path())
                .with_context(|| format!("remove boot assets {name}"))?;
            removed.push(name);
        }
    }
    Ok(removed)
}

pub struct BootAssets {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    /// The directory name, `<kernel version>-<engine build>`.
    pub tag: String,
}

/// The assets for this kernel and engine build, built if absent.
pub fn ensure(layout: &Layout) -> Result<BootAssets> {
    if !cfg!(target_env = "musl") {
        bail!(
            "this mj-worker is not a statically linked (musl) build, so it cannot run as the guest agent inside a VM image; use the portable worker"
        );
    }
    let kernel_version = sys::kernel_release()?;
    let tag = format!(
        "{kernel_version}-{}",
        mj_core::worker_build::WORKER_BUILD_ID
    );
    let directory = layout.boot().join(&tag);
    let assets = BootAssets {
        kernel: directory.join("vmlinuz"),
        initrd: directory.join("initrd.img"),
        tag,
    };
    if assets.kernel.is_file() && assets.initrd.is_file() {
        return Ok(assets);
    }
    layout.ensure()?;
    // One builder at a time; the second finds the first's result.
    let _lock = layout.lock_global()?;
    if assets.kernel.is_file() && assets.initrd.is_file() {
        return Ok(assets);
    }
    let scratch = layout
        .boot()
        .join(format!(".tmp-{}-{}", assets.tag, sys::scratch_suffix()));
    fs::create_dir_all(&scratch).with_context(|| format!("create {}", scratch.display()))?;
    let built = build(&scratch, &kernel_version);
    if let Err(error) = built {
        if let Err(cleanup) = fs::remove_dir_all(&scratch) {
            tracing::warn!(%cleanup, scratch = %scratch.display(), "could not remove boot scratch directory");
        }
        return Err(error.context("build the microvm kernel and initrd"));
    }
    if directory.exists() {
        fs::remove_dir_all(&directory)
            .with_context(|| format!("replace {}", directory.display()))?;
    }
    fs::rename(&scratch, &directory).with_context(|| format!("publish {}", directory.display()))?;
    Ok(assets)
}

fn build(scratch: &std::path::Path, kernel_version: &str) -> Result<()> {
    let boot_kernel = PathBuf::from(format!("/boot/vmlinuz-{kernel_version}"));
    fs::copy(&boot_kernel, scratch.join("vmlinuz"))
        .with_context(|| format!("copy {}", boot_kernel.display()))?;
    let init = scratch.join("init");
    fs::write(&init, INIT_SCRIPT).context("write the guest init script")?;
    fs::set_permissions(&init, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    let engine = std::env::current_exe().context("locate the engine binary")?;
    let initrd = scratch.join("initrd.img");
    let dracut = sys::find_tool("dracut")?;
    sys::run_checked(
        &dracut,
        &[
            "--force",
            "--no-hostonly",
            "--reproducible",
            "--no-early-microcode",
            "--modules",
            "bash",
            "--add-drivers",
            GUEST_DRIVERS,
            "--install",
            INIT_PROGRAMS,
            "--include",
            init.to_str().context("init path is not UTF-8")?,
            "/init",
            "--include",
            engine.to_str().context("engine path is not UTF-8")?,
            "/mj-worker",
            "--kver",
            kernel_version,
            initrd.to_str().context("initrd path is not UTF-8")?,
        ],
    )?;
    fs::remove_file(&init).context("remove the staged init script")?;
    Ok(())
}
