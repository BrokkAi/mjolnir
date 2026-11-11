//! VM lifecycle: run, start, stop, remove, and the facts about them.
//!
//! The engine is the only owner of VM state. Every mutation holds the VM's
//! lock; slot allocation, admission and the firewall table hold the global
//! lock, taken after the VM lock and never the other way round. A running VM
//! is a systemd transient unit, so the Firecracker process dies before any
//! file under it is removed, and the unit's runtime directory (which holds
//! the vsock socket) is created and deleted by systemd.

use std::collections::BTreeMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};

use super::client::{self, Client};
use super::image::{self, PullPolicy};
use super::net::{self, Link};
use super::protocol::{AGENT_PORT, StartRequest, VolumeSpec};
use super::state::{
    ImageConfig, Layout, STATE_FORMAT, VmState, VmVolume, validate_label, validate_name,
};
use super::{boot, sys, volume};

/// The Nitro hypervisor takes this much from the pool on top of the guest's memory.
const POOL_OVERHEAD_BYTES: u64 = 2 << 20;
/// The kernel driver grants PCIe slots in groups of eight.
const SLOT_STEP: u32 = 8;
/// Firecracker's documented maximum for `vcpu_count`.
const MAX_VCPUS: u32 = 32;
/// Podman's documented default `stop` timeout, used to bound the guest flush.
pub const DEFAULT_STOP_SECONDS: u64 = 10;
const NITRO_SYSFS: &str = "/sys/class/misc/nitro_enclaves";
const RUNTIME_ROOT: &str = "/run/mj-microvm";

pub fn unit_name(name: &str) -> String {
    format!("mj-microvm-{name}.service")
}

pub fn socket_path(name: &str) -> PathBuf {
    Path::new(RUNTIME_ROOT).join(name).join("v.sock")
}

/// systemd's `ActiveState`, reduced to what the engine decides on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Running,
    Stopping,
    Stopped,
}

pub fn activity(name: &str) -> Result<Activity> {
    let systemctl = sys::find_tool("systemctl")?;
    let unit = unit_name(name);
    let output = sys::run_checked(
        &systemctl,
        &["show", "--property=ActiveState", "--value", unit.as_str()],
    )?;
    Ok(match String::from_utf8_lossy(&output.stdout).trim() {
        "active" | "activating" | "reloading" => Activity::Running,
        "deactivating" => Activity::Stopping,
        _ => Activity::Stopped,
    })
}

fn stop_unit(name: &str) -> Result<()> {
    let systemctl = sys::find_tool("systemctl")?;
    let unit = unit_name(name);
    // A unit that is not loaded is already stopped; the check below is the test.
    sys::run_unchecked(&systemctl, &["stop", unit.as_str()])?;
    ensure!(
        activity(name)? == Activity::Stopped,
        "{unit} is still running after systemctl stop"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Pool

struct Pool {
    total: u64,
    free: u64,
    slots: u32,
    slots_total: u32,
}

fn read_number(path: &Path) -> Result<u64> {
    fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?
        .trim()
        .parse()
        .with_context(|| format!("parse {}", path.display()))
}

fn read_pool() -> Result<Option<Pool>> {
    let base = Path::new(NITRO_SYSFS);
    if !base.exists() {
        return Ok(None);
    }
    Ok(Some(Pool {
        total: read_number(&base.join("mem_pool/total"))?,
        free: read_number(&base.join("mem_pool/free"))?,
        slots: read_number(&base.join("pcie_slots"))? as u32,
        slots_total: read_number(&base.join("pcie_slots_total"))? as u32,
    }))
}

/// Admit a launch or refuse it, naming the pool. Called with the global lock held.
///
/// Memory is a commitment: the sum over running VMs of their memory plus the
/// hypervisor's overhead must fit the pool. The engine's own list of running
/// VMs decides this, not the kernel's momentary free figure, so two launches
/// racing for the last gigabyte cannot both be admitted. The kernel's figure
/// is still checked, to notice memory the engine did not hand out.
fn admit(layout: &Layout, vm: &VmState) -> Result<()> {
    let Some(pool) = read_pool()? else {
        bail!(
            "this host has no Nitro Enclaves driver ({NITRO_SYSFS} is missing); launch the instance with enclave support and boot with nitro_enclaves.mempool set"
        );
    };
    let mut committed = 0u64;
    let mut running = 0u32;
    for other in VmState::list(layout)? {
        if other.name != vm.name && activity(&other.name)? != Activity::Stopped {
            committed += other.memory_bytes + POOL_OVERHEAD_BYTES;
            running += 1;
        }
    }
    let needed = vm.memory_bytes + POOL_OVERHEAD_BYTES;
    let mib = |bytes: u64| bytes >> 20;
    ensure!(
        pool.total >= committed + needed,
        "the Nitro Sandbox memory pool cannot fit this VM: pool {} MiB, {} MiB committed to {running} running VMs, {} MiB needed",
        mib(pool.total),
        mib(committed),
        mib(needed)
    );
    ensure!(
        pool.free >= needed,
        "the Nitro Sandbox memory pool has {} MiB free, {} MiB needed (the pool is {} MiB; memory outside this engine's VMs holds the rest)",
        mib(pool.free),
        mib(needed),
        mib(pool.total)
    );
    let slots_needed = running + 1;
    ensure!(
        slots_needed <= pool.slots_total,
        "this host supports {} Nitro Sandboxes at once and {running} are running",
        pool.slots_total
    );
    if slots_needed > pool.slots {
        let raised = slots_needed.div_ceil(SLOT_STEP) * SLOT_STEP;
        let raised = raised.min(pool.slots_total);
        fs::write(
            Path::new(NITRO_SYSFS).join("pcie_slots"),
            raised.to_string(),
        )
        .with_context(|| format!("raise the Nitro Sandbox slot count to {raised}"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Run

#[derive(Clone, Debug)]
pub struct VolumeArg {
    pub name: String,
    pub destination: String,
    pub read_write: bool,
    pub chown: bool,
}

#[derive(Clone, Debug)]
pub struct RunSpec {
    pub name: String,
    pub image: String,
    pub platform: Option<String>,
    pub pull: PullPolicy,
    pub labels: BTreeMap<String, String>,
    pub cpus: Option<f64>,
    pub memory_bytes: u64,
    pub env: Vec<String>,
    pub user: Option<String>,
    pub workdir: Option<String>,
    pub volumes: Vec<VolumeArg>,
    pub command: Vec<String>,
}

fn validate_destination(destination: &str) -> Result<()> {
    let valid = destination.starts_with('/')
        && destination.len() > 1
        && !destination.ends_with('/')
        && !destination.contains("//")
        && destination
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
        && !destination
            .split('/')
            .any(|part| part == ".." || part == ".");
    ensure!(
        valid,
        "unsupported volume destination {destination:?}: use a plain absolute path of letters, digits, '/', '.', '_' and '-'"
    );
    for reserved in ["/proc", "/sys", "/dev", "/run", "/.mj-microvm"] {
        ensure!(
            destination != reserved && !destination.starts_with(&format!("{reserved}/")),
            "cannot mount a volume at {destination}: the guest uses {reserved}"
        );
    }
    Ok(())
}

fn random_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("generate a container id: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn allocate_slot(layout: &Layout) -> Result<u32> {
    let used: std::collections::BTreeSet<u32> = VmState::list(layout)?
        .into_iter()
        .map(|vm| vm.slot)
        .collect();
    (0..net::MAX_SLOTS)
        .find(|slot| !used.contains(slot))
        .ok_or_else(|| anyhow!("no free network slot: {} VMs exist", net::MAX_SLOTS))
}

/// Copy-on-write clone of a file on a reflink-capable filesystem (XFS with
/// `reflink=1`, btrfs). Fails visibly elsewhere: a full copy of an 11 GiB
/// image per VM is not a fallback worth hiding.
fn reflink(source: &Path, destination: &Path) -> Result<()> {
    const FICLONE: u32 = 0x4004_9409;
    let source_file =
        fs::File::open(source).with_context(|| format!("open {}", source.display()))?;
    let destination_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .with_context(|| format!("create {}", destination.display()))?;
    // SAFETY: FICLONE takes the source descriptor as its argument; both are open files.
    let result = unsafe {
        libc::ioctl(
            destination_file.as_raw_fd(),
            FICLONE as _,
            source_file.as_raw_fd(),
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        let _ = fs::remove_file(destination);
        return Err(error).with_context(|| {
            format!(
                "clone {} to {}: the state directory needs a reflink-capable filesystem (XFS created with reflink=1, or btrfs)",
                source.display(),
                destination.display()
            )
        });
    }
    Ok(())
}

pub fn build_state(layout: &Layout, spec: &RunSpec, image_id: &str, slot: u32) -> Result<VmState> {
    let image = ImageConfig::read(layout, image_id)?;
    let cpus = match spec.cpus {
        Some(cpus) => cpus,
        None => std::thread::available_parallelism()
            .map(|n| n.get() as f64)
            .unwrap_or(1.0),
    };
    ensure!(cpus > 0.0, "--cpus must be positive");
    let vcpus = (cpus.ceil() as u32).clamp(1, MAX_VCPUS);
    let mut env = image.env.clone();
    for entry in &spec.env {
        let key = entry.split_once('=').map(|(key, _)| key).unwrap_or(entry);
        env.retain(|existing| existing.split_once('=').map(|(k, _)| k) != Some(key));
        env.push(entry.clone());
    }
    let cmd = if spec.command.is_empty() {
        image.cmd.clone()
    } else {
        spec.command.clone()
    };
    ensure!(
        !(image.entrypoint.is_empty() && cmd.is_empty()),
        "the image has no command; give one after the image name"
    );
    Ok(VmState {
        format: STATE_FORMAT,
        id: random_id()?,
        name: spec.name.clone(),
        created: sys::now_rfc3339(),
        image_reference: spec.image.clone(),
        image_id: image_id.to_owned(),
        labels: spec.labels.clone(),
        vcpus,
        nano_cpus: (cpus * 1e9).round() as i64,
        memory_bytes: spec.memory_bytes,
        env,
        user: spec.user.clone().unwrap_or(image.user),
        working_dir: spec
            .workdir
            .clone()
            .filter(|w| !w.is_empty())
            .unwrap_or(image.working_dir),
        entrypoint: image.entrypoint,
        cmd,
        volumes: spec
            .volumes
            .iter()
            .map(|volume| VmVolume {
                name: volume.name.clone(),
                destination: volume.destination.clone(),
                read_write: volume.read_write,
                chown: volume.chown,
            })
            .collect(),
        slot,
        engine_build: mj_core::worker_build::WORKER_BUILD_ID.to_owned(),
    })
}

/// Create and start a VM. On any failure, remove everything this call made.
pub fn run(layout: &Layout, spec: &RunSpec) -> Result<String> {
    validate_name("container", &spec.name)?;
    for (key, value) in &spec.labels {
        validate_label(key, value)?;
    }
    ensure!(
        spec.volumes.len() <= 24,
        "at most 24 volumes can be attached to a VM"
    );
    let mut destinations = std::collections::BTreeSet::new();
    for volume in &spec.volumes {
        validate_name("volume", &volume.name)?;
        validate_destination(&volume.destination)?;
        ensure!(
            destinations.insert(volume.destination.clone()),
            "two volumes are mounted at {}",
            volume.destination
        );
        ensure!(
            spec.volumes
                .iter()
                .filter(|other| other.name == volume.name)
                .count()
                == 1,
            "volume {} is mounted twice",
            volume.name
        );
    }
    ensure!(
        spec.memory_bytes >= 64 << 20,
        "--memory is required and must be at least 64m: a microVM's memory is reserved from the Nitro pool"
    );
    ensure!(
        sys::is_root(),
        "the microvm engine must run as root (use sudo -n)"
    );
    layout.ensure()?;
    let _vm_lock = layout.lock_vm(&spec.name)?;
    ensure!(
        !layout.vm(&spec.name).exists(),
        "the container name {:?} is already in use",
        spec.name
    );
    // Pulling can take minutes and is not rolled back: a later run reuses the image.
    let (image_id, image_lock) =
        image::ensure_image(layout, &spec.image, spec.platform.as_deref(), spec.pull)?;
    sys::install_interrupt_handlers();
    let mut created_volumes = Vec::new();
    let result = (|| -> Result<VmState> {
        // The image's lock, taken when `run` resolved the image, is held until
        // its root disk is cloned, so a prune cannot remove it in between, and every volume's
        // lock until the state that lists them is written, so a `volume rm`
        // cannot remove one in between. Lock order: VM, image, volumes in name
        // order, global.
        let mut names: Vec<&VolumeArg> = spec.volumes.iter().collect();
        names.sort_by(|a, b| a.name.cmp(&b.name));
        let mut volume_locks = Vec::new();
        for volume in names {
            sys::check_interrupted()?;
            let (lock, created) = volume::prepare_for_run(layout, &volume.name, &spec.labels)
                .with_context(|| format!("prepare volume {}", volume.name))?;
            volume_locks.push(lock);
            if created {
                created_volumes.push(volume.name.clone());
            }
        }
        let state = {
            let _global = layout.lock_global()?;
            // A volume is one ext4 image; two kernels writing it would corrupt
            // it. The volume locks held above make this check and the state
            // write below one decision.
            for other in VmState::list(layout)? {
                if let Some(shared) = other.volumes.iter().find(|mounted| {
                    spec.volumes
                        .iter()
                        .any(|wanted| wanted.name == mounted.name)
                }) {
                    bail!(
                        "volume {} is already attached to container {}",
                        shared.name,
                        other.name
                    );
                }
            }
            let slot = allocate_slot(layout)?;
            let state = build_state(layout, spec, &image_id, slot)?;
            // The state file reserves the slot; the root disk follows.
            fs::create_dir(layout.vm(&spec.name))
                .with_context(|| format!("create {}", layout.vm(&spec.name).display()))?;
            state.write(layout)?;
            state
        };
        drop(volume_locks);
        reflink(
            &image::root_image(layout, &image_id),
            &layout.vm(&state.name).join("root.ext4"),
        )?;
        // The clone owns its blocks now; the base image can be pruned.
        drop(image_lock);
        launch(layout, &state)?;
        Ok(state)
    })();
    match result {
        Ok(state) => Ok(state.id),
        Err(error) => {
            let mut problems = Vec::new();
            if let Err(cleanup) = teardown(layout, &spec.name) {
                problems.push(format!("{cleanup:#}"));
            }
            for name in &created_volumes {
                if let Err(cleanup) = volume::remove(layout, name, true) {
                    problems.push(format!("remove volume {name}: {cleanup:#}"));
                }
            }
            if problems.is_empty() {
                Err(error)
            } else {
                Err(error.context(format!(
                    "cleanup after the failure also failed: {}",
                    problems.join("; ")
                )))
            }
        }
    }
}

/// Start an existing VM from its retained disks.
pub fn start(layout: &Layout, name: &str) -> Result<()> {
    validate_name("container", name)?;
    ensure!(
        sys::is_root(),
        "the microvm engine must run as root (use sudo -n)"
    );
    let _vm_lock = layout.lock_vm(name)?;
    let state = VmState::read(layout, name)?
        .ok_or_else(|| anyhow!("no container with name or ID {name:?} found: no such container"))?;
    match activity(name)? {
        Activity::Running => return Ok(()),
        Activity::Stopping => bail!("container {name} is stopping; try again when it has stopped"),
        Activity::Stopped => {}
    }
    ensure!(
        layout.vm(name).join("root.ext4").is_file(),
        "container {name} has no root disk; it was not created completely, remove it"
    );
    sys::install_interrupt_handlers();
    if let Err(error) = launch(layout, &state) {
        let mut stopped = stop_unit(name);
        if stopped.is_ok() {
            stopped = net::tap_down(&Link::for_slot(state.slot)?.tap);
        }
        return match stopped {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!(
                "stopping the failed launch also failed: {cleanup:#}"
            ))),
        };
    }
    Ok(())
}

fn firecracker_config(
    layout: &Layout,
    state: &VmState,
    boot: &boot::BootAssets,
    link: &Link,
) -> Result<Value> {
    let mut drives = vec![json!({
        "drive_id": "rootfs",
        "path_on_host": layout.vm(&state.name).join("root.ext4"),
        "is_root_device": true,
        "is_read_only": false,
    })];
    // (path depth, entry): init mounts shallow destinations first so a volume
    // at /workspace cannot hide one at /workspace/sub.
    let mut volume_map: Vec<(usize, String)> = Vec::new();
    for (index, volume) in state.volumes.iter().enumerate() {
        let device = format!("vd{}", (b'b' + index as u8) as char);
        drives.push(json!({
            "drive_id": format!("vol{index}"),
            "path_on_host": volume::disk_path(layout, &volume.name),
            "is_root_device": false,
            "is_read_only": !volume.read_write,
        }));
        volume_map.push((
            volume.destination.matches('/').count(),
            format!(
                "{device}:{}:{}",
                volume.destination,
                if volume.read_write { "rw" } else { "ro" }
            ),
        ));
    }
    volume_map.sort_by_key(|(depth, _)| *depth);
    let boot_args = format!(
        "panic=1 initcall_blacklist=i8042_init sbx.name={} sbx.ip={} sbx.gw={} sbx.dns={} sbx.vols={}",
        state.name,
        link.guest_cidr(),
        link.host,
        net::GUEST_DNS,
        volume_map
            .iter()
            .map(|(_, entry)| entry.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );
    Ok(json!({
        "boot-source": {
            "kernel_image_path": boot.kernel,
            "initrd_path": boot.initrd,
            "boot_args": boot_args,
        },
        "machine-config": { "vcpu_count": state.vcpus, "mem_size_mib": state.memory_bytes.div_ceil(1 << 20) },
        "drives": drives,
        "network-interfaces": [{ "iface_id": "eth0", "host_dev_name": link.tap, "guest_mac": link.mac }],
        "vsock": { "guest_cid": 3, "uds_path": socket_path(&state.name) },
        "enclave": { "direct_mode": true, "cpu_overcommit": true, "debug_mode": true },
    }))
}

/// Admit, wire up and start the VM's unit, wait for the guest agent, and
/// start the container's main process. Leaves cleanup to the caller.
fn launch(layout: &Layout, state: &VmState) -> Result<()> {
    let boot = boot::ensure(layout)?;
    let link = Link::for_slot(state.slot)?;
    let firecracker = sys::find_tool("firecracker")?;
    let engine = std::env::current_exe().context("locate the engine binary")?;
    let engine_text = engine.to_str().context("engine path is not UTF-8")?;
    ensure!(
        !engine_text.contains(char::is_whitespace) && !engine_text.contains('"'),
        "the engine path {engine_text:?} contains whitespace or quotes, which a systemd unit cannot carry"
    );
    {
        let _global = layout.lock_global()?;
        sys::check_interrupted()?;
        net::check_no_route_overlap()?;
        net::apply_host_policy()?;
        admit(layout, state)?;
        let config = layout.vm(&state.name).join("fc.json");
        super::state::write_json_atomic(
            &config,
            &firecracker_config(layout, state, &boot, &link)?,
        )?;
        net::tap_up(&link)?;
        let unit = unit_name(&state.name);
        let runtime = format!("RuntimeDirectory=mj-microvm/{}", state.name);
        let stop_post = format!(
            "ExecStopPost={engine_text} microvm host-tap-down {}",
            link.tap
        );
        let description = format!("Nitro Sandbox {}", state.name);
        sys::run_tool(
            "systemd-run",
            &[
                "--quiet",
                "--collect",
                &format!("--unit={}", unit.trim_end_matches(".service")),
                &format!("--description={description}"),
                "--property=KillMode=control-group",
                "--property=TimeoutStopSec=30",
                &format!("--property={runtime}"),
                "--property=RuntimeDirectoryPreserve=no",
                &format!("--property={stop_post}"),
                firecracker
                    .to_str()
                    .context("firecracker path is not UTF-8")?,
                "--no-api",
                "--config-file",
                config.to_str().context("config path is not UTF-8")?,
            ],
        )
        .context("start the VM unit")?;
    }
    let client = wait_for_agent(&state.name)?;
    let argv: Vec<String> = state
        .entrypoint
        .iter()
        .chain(state.cmd.iter())
        .cloned()
        .collect();
    client
        .start(StartRequest {
            argv,
            env: state.env.clone(),
            cwd: if state.working_dir.is_empty() {
                "/".to_owned()
            } else {
                state.working_dir.clone()
            },
            user: state.user.clone(),
            volumes: state
                .volumes
                .iter()
                .map(|volume| VolumeSpec {
                    destination: volume.destination.clone(),
                    chown: volume.chown,
                })
                .collect(),
        })
        .context("start the container command")?;
    Ok(())
}

/// Poll until the guest agent answers. Ends when the unit dies or a signal arrives.
fn wait_for_agent(name: &str) -> Result<Client<UnixStream>> {
    let socket = socket_path(name);
    loop {
        sys::check_interrupted()?;
        if activity(name)? == Activity::Stopped {
            bail!(
                "the VM stopped before its guest agent answered:\n{}",
                journal_tail(name)
            );
        }
        if socket.exists()
            && let Ok(client) = open_agent(name)
        {
            return Ok(client);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn journal_tail(name: &str) -> String {
    let unit = unit_name(name);
    sys::find_tool("journalctl")
        .and_then(|journalctl| {
            sys::run_unchecked(
                &journalctl,
                &["-u", unit.as_str(), "-n", "30", "--no-pager", "-o", "cat"],
            )
        })
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_else(|error| format!("(journal unavailable: {error:#})"))
}

/// Connect to a running VM's guest agent.
pub fn open_agent(name: &str) -> Result<Client<UnixStream>> {
    let stream = client::connect_unix(&socket_path(name), AGENT_PORT)?;
    // Socket options belong to the open file, so the clone clears them for the client too.
    let control = stream.try_clone().context("clone the agent connection")?;
    let client = client::handshake(stream)?;
    control.set_read_timeout(None)?;
    control.set_write_timeout(None)?;
    Ok(client)
}

/// The connected agent of a running VM, or the Podman-style reason there is none.
pub fn agent_for(layout: &Layout, name: &str) -> Result<(VmState, Client<UnixStream>)> {
    validate_name("container", name)?;
    let state = VmState::read(layout, name)?
        .ok_or_else(|| anyhow!("no container with name or ID {name:?} found: no such container"))?;
    if activity(name)? != Activity::Running {
        bail!("can only use a running container: container state improper");
    }
    let client =
        open_agent(name).with_context(|| format!("connect to the guest agent of {name}"))?;
    Ok((state, client))
}

// ---------------------------------------------------------------------------
// Stop and remove

/// Stop a VM: flush the guest's filesystems, then end its unit.
///
/// The guest gets `grace` seconds to flush and power off; without it a stop
/// would lose whatever the guest had not yet written. Stopping an absent or
/// stopped VM succeeds only with `ignore_missing` for the absent case.
pub fn stop(layout: &Layout, name: &str, grace: Duration, ignore_missing: bool) -> Result<()> {
    validate_name("container", name)?;
    let _vm_lock = layout.lock_vm(name)?;
    match VmState::read(layout, name)? {
        Some(_) => stop_locked(name, grace),
        None if ignore_missing => Ok(()),
        None => bail!("no container with name or ID {name:?} found: no such container"),
    }
}

fn stop_locked(name: &str, grace: Duration) -> Result<()> {
    if activity(name)? == Activity::Running {
        let (done_sender, done) = std::sync::mpsc::channel();
        let target = name.to_owned();
        let flusher = std::thread::Builder::new()
            .name("guest-flush".to_owned())
            .spawn(move || {
                let flushed = open_agent(&target).and_then(|client| client.stop());
                // The receiver stops listening when the grace period ends.
                let _ = done_sender.send(flushed);
            })
            .context("start the guest flush")?;
        match done.recv_timeout(grace) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, container = name, "guest flush failed; stopping the VM anyway")
            }
            Err(_) => tracing::warn!(
                container = name,
                "guest flush did not finish within the grace period; stopping the VM anyway"
            ),
        }
        stop_unit(name)?;
        // Stopping the unit closed the guest's connection, so the flush thread is done.
        if flusher.join().is_err() {
            tracing::warn!(container = name, "guest flush thread panicked");
        }
    } else {
        stop_unit(name)?;
    }
    Ok(())
}

/// Stop the unit and delete the VM's directory and TAP. Idempotent; no state is required.
fn teardown(layout: &Layout, name: &str) -> Result<()> {
    stop_unit(name)?;
    if let Some(state) = VmState::read(layout, name).ok().flatten() {
        net::tap_down(&Link::for_slot(state.slot)?.tap)?;
    }
    let directory = layout.vm(name);
    if directory.exists() {
        // Hide the VM atomically, then delete: a crash leaves scratch, never a half VM.
        let doomed = layout
            .vms()
            .join(format!(".removing-{name}-{}", sys::scratch_suffix()));
        fs::rename(&directory, &doomed)
            .with_context(|| format!("unpublish {}", directory.display()))?;
        fs::remove_dir_all(&doomed).with_context(|| format!("remove {}", doomed.display()))?;
    }
    Ok(())
}

/// Remove a VM. `force` stops a running one first; `volumes` also removes its named volumes.
pub fn remove(
    layout: &Layout,
    name: &str,
    force: bool,
    ignore_missing: bool,
    volumes: bool,
) -> Result<()> {
    validate_name("container", name)?;
    ensure!(
        sys::is_root(),
        "the microvm engine must run as root (use sudo -n)"
    );
    let _vm_lock = layout.lock_vm(name)?;
    let state = VmState::read(layout, name)?;
    if state.is_none() && !layout.vm(name).exists() {
        if ignore_missing {
            return Ok(());
        }
        bail!("no container with name or ID {name:?} found: no such container");
    }
    if activity(name)? != Activity::Stopped {
        ensure!(
            force,
            "cannot remove running container {name}: stop it first, or use --force"
        );
        stop_locked(name, Duration::from_secs(DEFAULT_STOP_SECONDS))?;
    }
    teardown(layout, name)?;
    if volumes && let Some(state) = state {
        for mounted in &state.volumes {
            volume::remove(layout, &mounted.name, true)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Inspection

/// The Podman-shaped description of a VM.
pub fn inspect(layout: &Layout, name: &str) -> Result<Value> {
    validate_name("container", name)?;
    let state = VmState::read(layout, name)?
        .ok_or_else(|| anyhow!("no container with name or ID {name:?} found: no such container"))?;
    let activity = activity(name)?;
    let status = match activity {
        Activity::Running => "running",
        Activity::Stopping => "stopping",
        Activity::Stopped => "exited",
    };
    let mounts: Vec<Value> = state
        .volumes
        .iter()
        .map(|volume| {
            json!({
                "Type": "volume",
                "Name": volume.name,
                "Source": layout.volume(&volume.name),
                "Destination": volume.destination,
                "Driver": "local",
                "RW": volume.read_write,
            })
        })
        .collect();
    Ok(json!({
        "Id": state.id,
        "Name": state.name,
        "Created": state.created,
        "Image": state.image_id,
        "ImageName": state.image_reference,
        "State": {
            "Status": status,
            "Running": activity == Activity::Running,
            "Paused": false,
            "ExitCode": 0,
        },
        "Config": {
            "Image": state.image_reference,
            "Labels": state.labels,
            "User": state.user,
            "Env": state.env,
            "WorkingDir": state.working_dir,
            "Cmd": state.cmd,
            "Entrypoint": state.entrypoint,
        },
        "HostConfig": {
            "Memory": state.memory_bytes,
            "NanoCpus": state.nano_cpus,
            "AutoRemove": false,
            "Privileged": false,
            "ReadonlyRootfs": false,
            "NetworkMode": "nitro",
        },
        "Mounts": mounts,
    }))
}

/// The state for `container exists`.
pub fn exists(layout: &Layout, name: &str) -> Result<bool> {
    validate_name("container", name)?;
    Ok(VmState::read(layout, name)?.is_some())
}

/// What `info` reports.
pub fn info(layout: &Layout) -> Result<Value> {
    let pool = read_pool()?;
    let mib = |bytes: u64| bytes >> 20;
    let tool_path = |name: &str| sys::find_tool(name).ok().map(|p| p.display().to_string());
    let firecracker_version = sys::find_tool("firecracker").ok().and_then(|path| {
        let output = sys::run_unchecked(&path, &["--version"]).ok()?;
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .map(|line| {
                line.trim_start_matches("Firecracker")
                    .trim()
                    .trim_start_matches('v')
                    .to_owned()
            })
    });
    let mut running = 0u32;
    let mut committed = 0u64;
    for vm in VmState::list(layout)? {
        if activity(&vm.name)? != Activity::Stopped {
            running += 1;
            committed += vm.memory_bytes + POOL_OVERHEAD_BYTES;
        }
    }
    Ok(json!({
        "engine": "mj-microvm",
        "engine_version": env!("CARGO_PKG_VERSION"),
        "engine_build": mj_core::worker_build::WORKER_BUILD_ID,
        "state_format": STATE_FORMAT,
        "protocol_version": super::protocol::PROTOCOL_VERSION,
        "state_root": layout.root(),
        "kernel_version": sys::kernel_release()?,
        "architecture": sys::host_architecture()?,
        "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "nitro_enclaves": pool.is_some(),
        "memory_pool_total_mib": pool.as_ref().map(|p| mib(p.total)).unwrap_or(0),
        "memory_pool_free_mib": pool.as_ref().map(|p| mib(p.free)).unwrap_or(0),
        "memory_committed_mib": mib(committed),
        "slots_in_use": running,
        "slots_total": pool.as_ref().map(|p| p.slots_total).unwrap_or(0),
        "slots_enabled": pool.as_ref().map(|p| p.slots).unwrap_or(0),
        "firecracker_path": tool_path("firecracker"),
        "firecracker_version": firecracker_version,
        "crane_path": tool_path("crane"),
        "notes": [
            "--cap-add, --cap-drop and --security-opt are accepted and have no effect: the VM is the isolation boundary",
            "--user sets the default user of the container command and of exec",
            "volume option U changes only the owner of the volume's root directory",
        ],
    }))
}
