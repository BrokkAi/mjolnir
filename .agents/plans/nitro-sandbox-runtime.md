# Run EC2 sessions in Nitro Sandboxes (Firecracker microVMs)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md`. Tracking issue: GitHub #1494.

## Purpose / Big Picture

Today an mj session on an EC2 host runs in a rootless Podman container. All sessions on that host share one Linux kernel, so a kernel bug or container escape in one session reaches every other session and the host. AWS Nitro Sandboxes (announced November 2026) run a workload in its own lightweight virtual machine whose memory and CPU boundary is enforced by the Nitro hypervisor, on ordinary (non-metal) EC2 instances, at no extra charge.

After this work a user can run `mj ec2 create --runtime nitro`, get a host registered as a target of kind `ssh_nitro`, and start sessions on it exactly as they do on a Podman target. Each session runs the same OCI image (by default `ghcr.io/brokkai/mjolnir/agent-dev`) inside its own microVM with its own kernel. The session behaves like any other: the harness runs a turn, the session survives a daemon restart, it can be checkpointed, closed, resumed and destroyed, and destroying it leaves nothing behind on the host.

To see it working: create a Nitro host, start a session on it with `mj new --target <ec2-nitro-target> ...`, ask the agent to run `uname -r; cat /proc/1/comm`, and observe a kernel and PID 1 that belong to the guest, not the host.

## Progress

- [x] (2026-11-10) Spike on a real m7i.4xlarge AL2023 host: agent-dev boots as a Nitro Sandbox in direct mode, vsock, two drives, NAT with IMDS refused, nested rootless Podman, stop/relaunch from retained disks. Notes and scripts: `.agents/docs/nitro-sandbox-spike.md` and `.agents/docs/nitro-sandbox-spike/`.
- [x] (2026-11-10) Milestone 1: host engine `mj-worker microvm` and guest agent, proven by hand on the spike host. Code in `mj-worker/src/microvm/`; the transcript is under Artifacts and Notes. Scope change from the user: nested containers inside Nitro sessions are out of scope for #1494 and tracked in #1496, so the nested `podman run` acceptance check was dropped (see Surprises).
- [x] (2026-11-10) Milestone 2: `ssh_nitro` target kind in mj-core, mj-controller, mj-tui, with breaking migration 124. Implemented against the Milestone 1 contract without the engine: unit-level and golden tests only. The controller's dependencies on the engine are listed under Artifacts and Notes, "Controller use of the engine"; nothing has run against a real host.
- [x] (2026-11-10) Milestone 3: `mj ec2 create --runtime nitro` host bootstrap; AWS-boundary fixtures cover the Nitro launch requests and bootstrap. Registration and image preload remain for Milestone 4.
- [x] (2026-11-11) Milestone 4: integration and live acceptance on EC2. Registration and preload wired (`e0630dd9c`); four defects found live and fixed (`21b955298`, `dcc7aec87`, `9b0bd6e88`, `c95b16983`, see Surprises). Acceptance passed on a fresh m7i.4xlarge from an isolated `--instance nitro-live` (transcript under Artifacts and Notes). Remaining: per-volume lock files left after destroy (engine follow-up).
- [x] (2026-11-11) Milestone 5: Firecracker jailer. Every VM's Firecracker now runs under the jailer 1.18.0 as its own unprivileged user (commit 556b7f144); proven on the spike host with the Milestone 1 transcript, the old-layout upgrade case, the isolation checks and a host reboot (see Artifacts and Notes, "Milestone 5 transcript").
- [ ] Milestone 6: storage-preserving image upgrade for Nitro sessions.

## Surprises & Discoveries

- Observation: The host can still use unclaimed sandbox pool memory for page cache, because the pool is a CMA (contiguous memory allocator) region; a large sandbox launch migrates page cache out (4.4 s for 42 GiB).
  Evidence: `.agents/docs/nitro-sandbox-spike.md`, "Measurements".
- Observation: Amazon Linux 2023 has no podman, skopeo or buildah packages. The spike flattened images with `crane export`.
  Evidence: `Error: Unable to find a match: podman`.
- Observation: The agent-dev image's `/usr/local/bin/podman` is a symlink to `podman-remote`, which needs a host socket that a VM does not have; the image has no `ip`, `modprobe`, init, or `poweroff`.
- Observation: Without udev in the guest, `/dev/net/tun` and `/dev/fuse` must be made mode 0666 by init, and `/` must be made rshared for rootless Podman. A bind mount of `/lib/modules` is deleted by `switch_root`; copying into a tmpfs works.
- Observation: The Firecracker vsock Unix socket is `root:root 0755`; only root can connect to it.
- Observation: Launch to vsock-ready is 2.6–3.3 s; a reflink clone of an 11 GiB root image takes 4–5 ms and no space on XFS.
- Observation: Milestone 1 timings on the spike host (m7i.4xlarge, debug build of the engine): `pull` of agent-dev 97 s (flattened root image 14,994,636,800 bytes apparent, about 8.3 GiB used); `run` to a running container 2.2–2.5 s once the initrd exists (8 s the first time, which includes the dracut build); `stop --time 0` 0.6 s including the guest flush; `start` 2.5 s; a 200 MiB stdin round trip through `exec -i` 0.4 s.
  Evidence: transcript under Artifacts and Notes.
- Observation: std's `Command::spawn` asserts that waiting for a child whose exec failed succeeds. A PID 1 reaper that calls `waitpid(-1)` collects that child first, so any failed spawn (for example `exec -w /nonexistent`) panicked inside std and poisoned the reaper's lock, breaking every later exec in that VM. Waiting for SIGCHLD with `sigwait` is also fragile, because the signal goes to any thread that does not block it.
  Evidence: `exec -w /nonexistent rb pwd` returned "connection closed", then "reaper lock was poisoned". The reaper now blocks in `waitid(WNOWAIT)` and reaps under the spawn lock; `mj-worker/tests/microvm_reaper.rs` covers it.
- Observation: `WORKER_BUILD_ID`, which keys the cached kernel and initrd (`/var/lib/mj-microvm/boot/<kver>-<build>/`), hashes the committed worker inputs. Uncommitted edits keep the old id, so a rebuilt engine silently boots VMs with the old guest agent until the change is committed (or the boot directory is deleted).
- Observation: A debug build of the musl worker is 245 MB, which does not belong in an initrd that sits in guest RAM. Stripping it gives 67 MB and boots as fast as the spike (2.2–2.5 s). Release builds are smaller still.
- Observation: Killing Firecracker (`systemctl kill -s KILL`) loses whatever the guest had not yet written to disk; ext4 recovers its journal but the last seconds of writes are gone. `stop` asks the guest to flush first and does not lose them. After a host reboot every unit, TAP device, nftables table and the slot count (back to 4) are gone, and `start` rebuilt all of them from the retained disks.
- Observation: The published agent-dev image has no `/usr/bin/podman` (only the `podman-remote` symlink at `/usr/local/bin/podman`), so the nested-Podman check could not run against it. As a side check that the guest environment is sound, Podman was installed with apt into a disposable VM and `/usr/bin/podman run --rm docker.io/library/alpine echo ok` ran as the image user `hel` (rootless, `XDG_RUNTIME_DIR` created by the agent, `/dev/net/tun` and `/dev/fuse` mode 0666 from init) in 0.65 s.
- Observation: The Cargo package is named `brokk-mj-worker`; `cargo -p mj-worker` does not match anything.

- Observation (Milestone 2): `environment_workers`, the physical-ownership ledger from revision 122, carries its own `kind` CHECK, so a new target kind has two constraints to widen, not one. Rebuilding that table with foreign keys on would clear every `environments.target_lease_id` (`ON DELETE SET NULL` on the drop); migration 124 turns enforcement off for the rebuild and the migration test checks the lease survives.
- Observation (Milestone 2): `sudo` resets the environment and logs the full command line. A value that came from a reference (`from_env`, `from_secret`) or from the controller's own environment is therefore never put on the line: `container_environment` (mj-controller `controller/backend.rs`) emits a valueless `--env NAME` and returns the value, provisioning feeds the values on the ssh command's stdin (`CommandPlan::provide_target_environment_secrets`, which exports them in the remote shell), and the `run` prefix preserves exactly the valueless names (`--preserve-env=A,B`, derived from the same argument list). The engine must inherit a valueless `--env NAME` from its environment. Literal values stay as `--env NAME=value`.
- Observation (Milestone 2): The admission refusal text was written for Macs ("this Mac has N GiB left after the macOS reserve"). Plan acceptance step 9 needs a message that names the pool, so the text (in `mj-core/src/state.rs` and the web viewer) now speaks of "this host" and explains both hard limits.
- Observation (Milestone 2): The generic 20% host RAM reserve would have applied on top of the pool, which is already net of the host's own memory. A capacity reading now says whether its memory total is a boot-time pool (`DeploymentCapacityUsage::memory_pool`, set when the probe carries the engine's report), and `host_container_capacity` applies no reserve to a pool.
- Observation (Milestone 2, review): two admission gaps are left for integration. (1) Admission charges sessions against the pool total, not the engine's reported free pool, so memory used by another daemon's sandboxes or the 2 MiB per-sandbox overhead is not seen until the engine's `run` refuses (which cleans up and names the pool). (2) A host that mixes Nitro with other runtimes keeps its physical RAM as the capacity total.

- Observation (Milestone 4): AL2023 ships `curl-minimal`, so the bootstrap's `dnf install ... curl` conflicted and cloud-init stopped before setting the pool; the verify step reported only "CMA pool was not initialized". The CLI now waits for cloud-init with the Podman path's log-reporting check, and owns the reboot: it records the boot ID, issues `systemctl --no-block reboot`, and waits for SSH from a different boot ID (`wait_for_ec2_reboot`), so an answer from the old boot cannot pass for the new one.
- Observation (Milestone 4): the engine install sat in the background poller's `host_is_running`, but `mj ec2 create` preloads through `refresh_host_image` directly, so the preload ran the engine before anything installed it ("command not found"). The install now lives in `refresh_host_image`, the entry point every refresh path shares.
- Observation (Milestone 4): the controller uploads the harness profile with Podman's contents form `cp DIR/. CTR:DEST`, which the engine refused. The engine now implements it in both directions.
- Observation (Milestone 4): gap (2) above bit in the common case: `mj ec2 create` registers a disabled bare sibling, so every EC2 Nitro host was sampled as physical RAM less the 20% reserve (a 48 GiB session was refused with 50.7 GiB free in the pool). A disabled target with no live session no longer decides how a host is sampled; the probe also fails visibly when an installed engine cannot report, instead of falling back to physical memory.

- Observation: Jailer 1.18.0 runs the Nitro backend in direct mode with nothing special. It creates `dev/nitro_enclaves` (and `kvm`, `urandom`, `userfaultfd`) in the chroot itself, so the engine does not bind or mknod the device; the guest booted to "listening" on the first manual try. Firecracker then runs as the VM's user with no capabilities (`CapEff` 0), `NoNewPrivs` 1 and the jailer's seccomp filter, and the pool, slots and 0.2 s stop behave as before. An unprivileged Firecracker attaches to a TAP only if that TAP is owned by its user, so the engine now creates it with `ip tuntap add ... user U group U`.
  Evidence: manual probe on the spike host, then the Milestone 5 transcript.
- Observation: Without `--new-pid-ns` the jailer `exec`s Firecracker, so the systemd unit's main process is the VMM itself and every property of the unit (stop, `ExecStopPost`, accounting) is unchanged. With `--new-pid-ns` the jailer parent exits and the unit deactivated at once in the probe, so systemd could not track the VMM.
- Observation: The jail holds hard links, not copies, to the root disk, the volume disks, the kernel and the initrd, so a launch copies nothing and a disk is one file inside and outside. Hard links need the jail on the same filesystem as the disks (the engine puts it under `/var/lib/mj-microvm/jail`) and make the kernel and initrd shared inodes, so those two files are now mode 0644 (they hold only the host kernel and the engine). The links also pin the files of a running VM against `image prune`.
- Observation: `sudo -u '#UID'` refuses a numeric user that has no passwd entry; the isolation checks use `setpriv --reuid --regid --clear-groups` to act as a VM's user.

## Decision Log

- Decision: Use Firecracker 1.18+ with the Nitro backend (`enclave` object) in direct mode with `cpu_overcommit: true`.
  Rationale: Firecracker is the only VMM AWS supports for Nitro Sandboxes outside EKS/ECS. Direct mode is for a trusted host (we run the host); confidential mode adds per-device buffers and attestation that buy us nothing. CPU overcommit matches mj's existing policy of warning, not refusing, on CPU.
  Date/Author: 2026-11-10, lead.
- Decision: Do not use AWS's EKS runtime (`containerd-shim-nitro-sandbox-v2`).
  Rationale: it is built around kubelet, a CRI proxy, a kubeconfig, Cedar policies in pod annotations, and guest images keyed by Kubernetes version. Owning the Firecracker path is smaller than emulating that environment.
  Date/Author: 2026-11-10, lead.
- Decision: The host side is a subcommand of the existing worker binary, `mj-worker microvm <verb>`, run as root through `sudo -n`. It is the sole owner of microVM state on the host and offers a Podman-compatible subset of verbs and output formats, so the controller drives it the way it drives `podman` over SSH.
  Rationale: one owner per fact (AGENTS.md concurrency rule); the controller already has SSH command plans for exec, cp, inspect, exists, start, stop, rm and image inspect/pull/prune with specific parsed outputs, and reproducing those outputs keeps recovery and cleanup code shared. Root is required for `/dev/nitro_enclaves`, TAP devices, nftables, and the vsock socket; `ec2-user` on AL2023 has passwordless sudo. No new workspace crate (AGENTS.md).
  Date/Author: 2026-11-10, lead.
- Decision: The engine owns compound operations that Podman needs controller shell scripts for. `run` creates missing named volumes with the given labels, refuses a volume whose labels name another session, and removes everything it created if the launch fails. The controller does not reimplement these steps for Nitro.
  Rationale: a privileged CLI must validate every id and label itself, and a single serialized owner removes the races that the Podman shell scripts work around.
  Date/Author: 2026-11-10, lead.
- Decision: Guest boot path is the host's own AL2023 kernel (an EFI-stub `vmlinuz`) plus an initrd that the engine builds with dracut. The initrd `/init` loads modules, mounts the root disk and volumes, configures the network, copies the kernel modules and the engine's own binary into tmpfs mounts in the new root, and `switch_root`s to `mj-worker microvm guest-agent` as PID 1. The initrd is rebuilt when the kernel version or the engine build changes.
  Rationale: proven in the spike; no package installation into user images; the guest agent always matches the engine that launched the VM.
  Date/Author: 2026-11-10, lead.
- Decision: Images are flattened with a pinned `crane` into a read-only ext4 base image per image digest (`mkfs.ext4 -d`), with the OCI image config (Env, User, WorkingDir, Entrypoint, Cmd, Labels) saved next to it. Each VM gets a reflink clone of the base as its writable root. mj never installs packages into a user's image.
  Rationale: AL2023 has no Podman; crane is a single static binary with registry auth support; reflink clones are instant and free on XFS.
  Date/Author: 2026-11-10, lead.
- Decision: Named volumes (`--volume name:/path`) become per-VM sparse ext4 disk files, created at the size of the host filesystem that holds them, labeled, and kept until `volume rm` or `rm --volumes`. Host-path bind mounts (user additional mounts) and the shared mbx build cache are not available in Nitro sessions in this plan: the session is created without them and the user is told so. A follow-up issue covers serving host paths into guests (for example NFS over the TAP link).
  Rationale: Firecracker has no virtio-fs. Sizing a sparse disk to the filesystem avoids a guessed cap; the real limit is host disk. Apple container sessions already set the precedent of reduced mount support and no mbx.
  Date/Author: 2026-11-10, lead.
- Decision: Networking is one TAP device and one /30 per VM from `10.200.0.0/16`, NAT through the host's default interface, IMDS (169.254.169.254) rejected, no VM-to-VM traffic, no connections from a VM to the host itself. VMs may reach the VPC, as Podman containers on the same host can. The engine applies the nftables table idempotently before every launch, so a host reboot needs no separate setup step. The engine refuses to launch if `10.200.0.0/16` overlaps a host route.
  Rationale: spike-proven rules; parity with Podman egress; structural reboot recovery.
  Date/Author: 2026-11-10, lead.
- Decision: Each running VM is a systemd transient unit `mj-microvm-<name>.service` with `RuntimeDirectory=mj-microvm/<name>` (socket directory created empty and removed by systemd) and `ExecStopPost=` TAP removal. Stopping the unit kills the Firecracker process before any file is removed.
  Rationale: spike-proven; process teardown before file removal on every exit path (AGENTS.md subprocess rules); units survive the SSH session.
  Date/Author: 2026-11-10, lead.
- Decision: Memory is a hard admission commitment for `ssh_nitro` targets, as it is for Apple containers; CPU follows the overcommit-and-warn policy. The memory pool is reserved at boot with `nitro_enclaves.mempool=95%-1G`, the value AWS's EKS AMI build guide uses. The engine raises `/sys/class/misc/nitro_enclaves/pcie_slots` (in steps of 8) under its global lock when the next launch needs a slot.
  Rationale: sandbox memory comes from the boot-time pool and cannot be overcommitted or ballooned; slot raises are live and cheap (128 MiB per 8).
  Date/Author: 2026-11-10, lead.
- Decision: New target kind `ssh_nitro` in config (`TargetTemplate::SshNitro`) and `ssh-nitro` in durable state (`TargetLocator::SshNitro`). Widening `session_targets.kind` is a breaking schema migration, following migration 98 (`ssh-apple-container`); no relay protocol bump.
  Rationale: older daemons treat unknown target kinds as impossible.
  Date/Author: 2026-11-10, lead.
- Decision: Pin Firecracker v1.18.0 archives to the SHA-256 values from each upstream `firecracker-v1.18.0-<arch>.tgz.sha256.txt` release asset; pin crane v0.22.1 to its release `checksums.txt` asset. The checksums were verified against the downloaded Firecracker archives by the milestone lead.
  Date/Author: 2026-11-10, milestone lead and implementer.
- Decision: The daemon installs the host-architecture worker binary on the EC2 host with the same machinery it uses for `ssh_bare` hosts and invokes the engine from that path. The engine's on-disk state format and the guest-agent vsock protocol are versioned and stay readable by newer builds, because VMs and their agents outlive engine upgrades.
  Rationale: upgrades complete without user intervention (AGENTS.md).
  Date/Author: 2026-11-10, lead.
- Decision: Admission uses the engine's own ledger. A launch needs `memory + 2 MiB` (the Nitro overhead measured in the spike) to fit in the pool total minus the same figure summed over every VM whose unit is not inactive, evaluated under the global lock that also covers starting the unit; the kernel's free figure is checked as well, to notice memory the engine did not hand out. Slots follow the same count and are raised in steps of 8 up to `pcie_slots_total`. The refusal names the pool. Three parallel 20 GiB runs into a 48 GiB pool admitted exactly two.
  Rationale: one owner decides the fact (AGENTS.md); the kernel's momentary free figure lags the unit start, so two racing launches could both pass it.
  Date/Author: 2026-11-10, Milestone 1 implementer.
- Decision: `stop` always gives the guest a chance to flush: it sends a stop request over vsock, the agent kills the remaining processes, syncs, unmounts and answers, and then the unit is stopped. The grace period is `--time N`, and 10 seconds (Podman's documented default) for `--time 0` or no `--time`, after which the unit is stopped anyway.
  Rationale: a kill-only stop loses guest page cache; the controller always passes `--time 0`.
  Date/Author: 2026-11-10, Milestone 1 implementer.
- Decision: The CLI is stricter than Podman where a VM cannot honor Podman's meaning: `run` requires `--detach` and `--memory` (a VM has no unlimited memory), `--cpus` defaults to the host CPU count capped at Firecracker's 32, `--volume` accepts named volumes with options `rw`, `ro`, `U` only, `--stop-signal` must be KILL, and `cp`/`exec` need a running VM. `U` chowns the whole volume to the container user at every start, as Podman does. A new volume (only `lost+found` on it) is filled from the image directory it covers, with that directory's owner and mode, which is Podman's copy-up. Nested volume destinations are mounted shallowest first. `inspect` has no `State.ExitCode`: the guest powers off when the command exits and the status is not kept, and a made-up zero would be wrong.
  Rationale: the controller passes `--user 0:0`, `--volume name:/workspace:rw,U` and `--volume name:/tmp:rw`, and `/tmp` must end up 1777.
  Date/Author: 2026-11-10, Milestone 1 implementer.
- Decision: `volume rm` refuses a volume that a VM's state lists, even with `--force` (which only ignores missing volumes); `rm --volumes` removes the VM's named volumes after the VM; a failed `run` removes only volumes that call created. Copy-in entries are owned by the container user (Docker's rule); copy-out entries belong to `SUDO_UID:SUDO_GID` when run under sudo, so the SSH user can read what `scp` then fetches. Unpacking accepts only files, directories and symlinks, rejects `..` and absolute names, and never writes through a symlink, because the guest is the less trusted side.
  Rationale: a privileged engine must not delete a VM's disks by accident or unpack a hostile tar.
  Date/Author: 2026-11-10, Milestone 1 implementer.
- Decision: Image ids are the SHA-256 of the OCI configuration blob (Podman's image id), references map to ids in `images/refs.json`, and `image prune -f` removes images that no reference or VM names, scratch from interrupted pulls, and boot assets built for other kernels or engine builds. Flattening pipes `crane export` into the system `tar` (no pipe handling in the engine; each process logs to a file).
  Rationale: matches what the controller pins and compares; avoids the Rust `tar` crate's gaps with device nodes and xattrs.
  Date/Author: 2026-11-10, Milestone 1 implementer.
- Decision: There is no timeout on waiting for the guest agent. The wait ends when the unit dies (the error carries the last 30 journal lines) or when SIGINT, SIGTERM or SIGHUP arrives, which rolls the launch back. Nothing has been observed to hang, and a cap would be a guess.
  Rationale: AGENTS.md on speculative caps.
  Date/Author: 2026-11-10, Milestone 1 implementer.
- Decision: The guest agent reaps through one thread that blocks in `waitid(WNOWAIT)` and collects under the lock spawns hold, and exec output pumps poll an eventfd that fires when the process exits, so a descendant that keeps a pipe open cannot hold an exec session open (Podman behaves the same). Firecracker runs with `debug_mode: true` so boot failures reach the unit journal; the guest console shows kernel errors and agent errors only.
  Rationale: see Surprises; spike measured about 150 console lines per boot.
  Date/Author: 2026-11-10, Milestone 1 implementer.
- Decision: `info --format json` keys are the contract for Milestone 2 preflight: `engine_version`, `engine_build`, `state_format`, `protocol_version`, `kernel_version`, `architecture`, `cpus`, `nitro_enclaves`, `memory_pool_total_mib`, `memory_pool_free_mib`, `memory_committed_mib`, `slots_in_use`, `slots_total`, `slots_enabled`, `firecracker_path`, `firecracker_version`, `crane_path`, `notes`. `--format` templates support `{{.A.B}}`, `{{json .A.B}}` and `{{index .Map "key"}}`; anything else fails naming the template.
  Rationale: the plan listed the facts but not their names; the Podman scripts in the controller use exactly those template forms.
  Date/Author: 2026-11-10, Milestone 1 implementer.
- Decision: Engine lock order is VM, image, volumes in name order, global. `pull` holds the image lock from flattening through recording the reference; `run` holds the image lock until the root disk is cloned and every volume lock until the VM state that lists the volumes is written; `image prune` and `volume rm` decide under those same locks. A volume can be listed by only one VM, because two kernels writing one ext4 image would corrupt it. `pull` resolves a tag to a digest first and fetches the configuration and the filesystem by that digest. The module is compiled on Linux only.
  Rationale: review of the first implementation found each of these as a race between two engine verbs or a hang; a single lock order makes them structurally impossible (AGENTS.md concurrency rule). Verified live: a read-only volume mounts read-only, `cp` with a missing source fails at once, a second VM on an attached volume is refused and leaves nothing.
  Date/Author: 2026-11-10, Milestone 1 implementer.
- Decision: A launch holds a shared lock on its boot-assets directory (`locks/boot-<tag>.lock`) from before it names the assets until the guest agent answers, and `image prune` removes other builds' assets only when it can take the exclusive lock without waiting. `stop --time 0` keeps the 10-second flush bound on purpose (review asked for an immediate kill): the controller stops sessions with `--time 0` and keeps their volumes, and a kill loses the guest's unwritten data. IPv6 from TAP devices is dropped by an `ip6` table beside the `ip` one, and a root guest agent always applies the requested group.
  Rationale: review findings; one lock order, no unobserved cap added.
  Date/Author: 2026-11-10, Milestone 1 implementer.
- Decision: The engine also offers `ps [--all] [--filter label=KEY[=VALUE]]... [--format json|TEMPLATE]` (a JSON array in Podman's shape with `Id`, `Names`, `Labels`, `State`, `Image`, `ImageID`, `Created`, `Mounts`), for the controller's recovery scan. `--env KEY` without a value takes the engine's own value and is omitted when unset, so the controller can pass secrets through `sudo --preserve-env` instead of the command line.
  Rationale: contract additions from the target-kind implementer (Milestone 2).
  Date/Author: 2026-11-10, Milestone 1 implementer.
- Decision: The flattened root disk is sparse and sized to the filesystem that holds it, like a volume (196 GiB on the spike host, 8.2 GiB used), with the default inode density; no headroom figure is baked into an image. Operands that name a container accept the id `run` printed or an unambiguous prefix of 12 or more hex characters; a name wins. A new volume is filled from the image only at its first read-write attach (the host records `initialized` after a container starts with it), so deleting a volume's contents does not bring them back, and a volume first attached read-only stays new. Fractional `--cpus` is refused: a vCPU is a whole core given to the hypervisor and nothing on the host can throttle it.
  Rationale: review findings on speculative sizes, Podman compatibility and resurrected data.
  Date/Author: 2026-11-10, Milestone 1 implementer.

- Decision: In configuration the runtime is `kind = "nitro"` on an `ssh` machine (`StoredTarget::Nitro`), fused in memory into `TargetTemplate::SshNitro` (`kind_name` `ssh-nitro`), exactly as `apple-container` resolves to `SshAppleContainer`. The plan text said `ssh_nitro`; the code spells kinds in kebab-case and splits machine from runtime since config version 11. A `nitro` runtime on `local` or an EC2-template machine is refused at load. `mj ec2 create --runtime nitro` should build `TargetTemplate::SshNitro` and let `stored_target` write the file.
  Rationale: one shape for every remote VM-backed runtime.
  Date/Author: 2026-11-10, Milestone 2 worker.
- Decision: One owner for the engine's command line. `mj_core::targets::engine_argv`/`engine_shell_words` render the prefix `sh -c 'exec sudo -n [--preserve-env=NAMES] "$HOME/.local/share/hel/microvm/hel" microvm "$@"' mj-microvm` (the `--preserve-env` list appears only on `run`) from the single constant `MICROVM_ENGINE_WORKER`; every Podman call site that built `[engine, verb...]` now goes through `engine_argv`, `ssh_engine_command` or `with_engine_argv`. The prefix expands `$HOME` itself because `sudo` needs an absolute path and ssh quotes every argument. The container engine name used as a selector (`container_engine()`) is `"microvm"`.
  Rationale: the controller never recomputes the path; scripts get the same words through `engine_shell_words`.
  Date/Author: 2026-11-10, Milestone 2 worker.
- Decision: The host engine is installed by `ensure_microvm_engine` (`mj-controller/src/controller/worker_binary/microvm.rs`) with the `ssh_bare` machinery: platform probe, the shared digest-keyed worker store, `install_worker_binary_at` with a host-scoped destination. It runs at every launch preflight (`verify_target`), before the image refresher pulls to the host, and in the Targets pane's Test action. `mj doctor` only reads and reports "not installed yet" with the fix.
  Rationale: the engine is this build's worker and must match the daemon that launches VMs; a host created for sandboxes is preloaded before any session exists.
  Date/Author: 2026-11-10, Milestone 2 worker.
- Decision: Nitro sessions run as root with `HOME=/home/hel` (`run --user 0:0 --env HOME=/home/hel`), as Podman sessions do, so the Podman upload, ownership and checkpoint paths apply unchanged and no `exec --user` is needed. Nested rootless Podman in the guest therefore runs as root unless Milestone 3 decides otherwise.
  Rationale: smallest difference from the Podman call sites; the VM is the security boundary.
  Date/Author: 2026-11-10, Milestone 2 worker.
- Decision: Dropped for a Nitro launch, with a notice naming each: additional host-path mounts (the session's stored choice stays), the mbx build cache, the Git clone cache and the tool cache. The target is not offered attached directories (`mount_history_host` is `None`). Move to or from a Nitro target is refused in the assessment with a blocker that says to close and start anew. Retained container upgrade is unsupported until Milestone 6 (`ContainerSettings::upgrade_supported`). The PID-limit probe does not apply.
  Rationale: no virtio-fs; Move's transport needs host mounts or `run --rm --entrypoint`.
  Date/Author: 2026-11-10, Milestone 2 worker.
- Decision: Migration 124 is breaking and rebuilds `session_targets` and `environment_workers` to admit `ssh-nitro`; it raises the minimum compatible revision to 124 in the same transaction. Older daemons would hit `unreachable!` decoding a kind their CHECK admitted, so the floor is the only protection.
  Date/Author: 2026-11-10, Milestone 2 worker.
- Decision: Containers inside Nitro sessions (nested Podman or Docker) are out of scope; the agent-dev image stays unchanged.
  Rationale: ordinary Podman sessions cannot run containers either (the image's `podman` is `podman-remote` with no socket in sessions), so this would be a new capability, not parity, and it would grow every session's image. The user chose to drop it on 2026-11-10; it is tracked in #1496. The guest init keeps the modules and device setup the spike used, which cost nothing at boot.
  Date/Author: 2026-11-10, lead (user decision).

- Decision (Milestone 5): Each VM gets its own user and group id, `3,100,000 + network slot`, so VMs cannot read each other's files or signal each other, and an id is unique among existing VMs because the slot is. The base sits above ordinary users and the rootless subordinate range (100000-165535). The ids are not allocated or recorded anywhere else; they are derived from the slot in `jail_ids`.
  Rationale: one owner for the fact (the slot); a shared id would let one compromised VMM read every VM's disks.
  Date/Author: 2026-11-11, Milestone 5 implementer.
- Decision (Milestone 5): The jail is `/var/lib/mj-microvm/jail/firecracker/vm<slot>/root`, rebuilt at every launch (a stale one from a crash or reboot is removed first) and removed by the VM's unit through `ExecStopPost=<engine> microvm host-jail-down <slot>`, which also removes the TAP, and by the engine's own teardown. Paths in the Firecracker configuration are relative to the jail (`/root.ext4`, `/vol<i>.ext4`, `/vmlinuz`, `/initrd.img`, `/v.sock`). The configuration is no longer written under `vms/<name>/`. The vsock socket lives in the jail, so the old systemd `RuntimeDirectory` is not used for a jailed VM. Disks are chowned to the VM's user at launch and stay owned by it afterwards; the engine runs as root and does not mind.
  Rationale: process teardown before file removal on every exit path (a guest that powers itself off still gets its TAP and jail removed); a fresh jail per launch means no stale socket.
  Date/Author: 2026-11-11, Milestone 5 implementer.
- Decision (Milestone 5): The jailer is given no cgroup settings and no `--new-pid-ns` and no `--netns`. The unit's own cgroup already holds the VMM and gives accounting and a kill boundary, and Nitro memory and vCPUs are not governed by host cgroups. A PID namespace would break the unit's tracking (see Surprises), and a network namespace would need a veth pair to the NAT table in place of the TAP; the unprivileged user, empty capability set, chroot, mount namespace and seccomp filter, plus the TAP owned by the VM's user, are the isolation the production guide asks for that matters here.
  Rationale: smallest change that keeps every existing guarantee; revisit if a network namespace becomes necessary.
  Date/Author: 2026-11-11, Milestone 5 implementer.
- Decision (Milestone 5): `state.json` format 2 adds `jailed`. Format 1 files read back with `jailed: false` and behave as before: `exec`, `cp`, `stop` (with its flush), `rm` and inspect use the old runtime-directory socket, and the old unit's `ExecStopPost` (`host-tap-down`, still supported) cleans its TAP. The next `start` of such a VM launches it in a jail and rewrites its state as format 2, so a running session on an existing host survives an engine upgrade untouched and moves into a jail the next time it is stopped and started. An engine that only knows format 1 refuses a format 2 file, as designed. `inspect` reports `HostConfig.Isolation` as `jailer` or `none`. `info` reports `jailer_path`, and a launch needs the jailer installed.
  Rationale: upgrades must complete without intervention (AGENTS.md); no running VM is restarted by an upgrade.
  Date/Author: 2026-11-11, Milestone 5 implementer.

## Outcomes & Retrospective

None yet.

## Context and Orientation

Terms. A *target* is where a session runs; its configured shape is `TargetTemplate` in `mj-core/src/config/targets.rs` (TOML in the user's `config.toml`), and the durable record of a created session target is `TargetLocator` (runtime form in `mj-core/src/targets.rs`, API/state form in `mj-core/src/state.rs`, database row `session_targets` written by `mj-controller/src/database/state_io.rs`). The *daemon* (`mj daemon-run`, crate `mj-controller`) is the control plane; the *worker* (`mj-worker` crate, one per session) runs the harness inside the session's container and keeps running without the daemon. A *Nitro Sandbox* is a VM that the Nitro hypervisor runs beside an EC2 instance (the *parent*) using vCPUs and memory the parent contributes; *Firecracker* is the virtual machine monitor process on the parent that creates it through `/dev/nitro_enclaves`. *vsock* is a socket family that connects a VM to its host; Firecracker exposes the guest's vsock as a Unix socket on the host, and a host client connects to guest port P by writing `CONNECT P\n` and reading `OK <n>\n`. *Direct mode* means the parent can read the sandbox's memory (no attestation), which is fine because mj owns the host. A *reflink* clone is a copy-on-write file copy that shares blocks until written (`cp --reflink=always`, XFS with `reflink=1`).

How Podman sessions on SSH hosts work today. `mj-controller/src/targets/container.rs` builds `podman run` (flags in `container_run_args`: `--init --stop-signal KILL --pull=… --detach --name N --cap-add --user --env --security-opt --volume tmpvol:/tmp:rw --volume workspacevol:/workspace:rw [additional mounts] IMAGE keepalive`) and wraps it in shell scripts that create labeled volumes and clean up on failure (`PODMAN_VOLUME_RUN_SCRIPT`). Everything is sent through SSH. The worker is started with `podman exec --detach <id> sh -c <script>` (`mj-controller/src/controller/worker_binary/process.rs:283-390`). The daemon talks to a worker by running `podman exec -i <id> <argv>` under a drain wrapper that preserves stdout, stderr and the exit status (`mj-core/src/targets.rs:2837-2990`). Files go in with SCP to the host and then `podman cp` (`mj-controller/src/controller/worker_binary/install.rs:917-1017`, checkpoint staging in `mj-controller/src/controller/checkpoint/staging.rs:315-372`). Recovery uses `podman container exists` (exit 0 present, 1 absent, other error), `podman container inspect` (JSON array of one object; reads `Config.Labels` `dev.mj.managed=true` and `dev.mj.session=<id>`, and `State.Status`), and `podman start` (`mj-controller/src/targets/recovery.rs:45-225`). Cleanup uses `stop --time 0 --ignore`, `inspect --format '{{.State.Running}}'` expecting `false`, then `rm --force --ignore` and volume removal (`mj-controller/src/targets/cleanup.rs:278-500`). Images use `image inspect --format '{{.Id}}'`, `pull [--platform=P]`, `image prune -f` (`mj-core/src/targets.rs:2399-2455`). The Apple container target (commit 5a8619892, 58 files) is the closest precedent for adding a VM-backed kind: hard RAM admission in `mj-controller/src/daemon/capacity.rs:280-315`, reduced mount semantics in `container.rs:13-42`, no mbx in `mj-controller/src/controller/mbx.rs:255-263`, and breaking migration 98 in `mj-controller/src/database/schema.rs:1763-1804`. A full list of sites that match on the target enums is in `.mj`-independent form below under Milestone 2.

`mj ec2 create` (`mj-cli/src/ec2.rs`) launches an Ubuntu instance, bootstraps rootless Podman (`bootstrap()` around line 980, ZFS script `mj-cli/src/ec2_zfs.sh`), allocates an Elastic IP, registers an `ssh_podman` target (lines 552-603) and preloads the image through the daemon's image refresher (lines 604-629).

The spike recipe that every milestone builds on is `.agents/docs/nitro-sandbox-spike.md` with scripts in `.agents/docs/nitro-sandbox-spike/` (`sandbox-init` is the proven guest `/init`; `04-host-net.sh` the proven nftables table; `05-launch.sh` the proven systemd-run invocation; `firecracker-config.sbx1.json` the proven Firecracker configuration). The spike host (instance `i-0b0932db78fa86aaf`, us-east-1d, AL2023 kernel 6.18.56, Firecracker 1.18.0, pool 48G) is kept running for Milestone 1 and 4 testing; its SSH key is `~/.ssh/mj-nitro-spike.pem`, user `ec2-user`, address in `~/.ssh/mj-nitro-spike.ip`.

## Plan of Work

### Milestone 1: host engine and guest agent

Add a `microvm` subcommand family to the `mj-worker` binary (new module directory `mj-worker/src/microvm/`). At the end of this milestone, on the spike host, the following works by hand with the binary copied to the host, and is captured in a transcript in this plan:

    sudo -n ./mj-worker microvm info --format json
    sudo -n ./mj-worker microvm pull ghcr.io/brokkai/mjolnir/agent-dev:latest
    sudo -n ./mj-worker microvm image inspect --format '{{.Id}}' ghcr.io/brokkai/mjolnir/agent-dev:latest
    sudo -n ./mj-worker microvm run --detach --name t1 --label dev.mj.managed=true --label dev.mj.session=s1 --cpus 2 --memory 4g --env A=1 --volume t1-ws:/workspace:rw --volume t1-tmp:/tmp:rw ghcr.io/brokkai/mjolnir/agent-dev:latest sleep infinity
    sudo -n ./mj-worker microvm exec -i t1 sh -c 'cat >/workspace/big; wc -c /workspace/big' < 200MiB-file
    sudo -n ./mj-worker microvm exec --detach t1 sh -c 'sleep 100'
    sudo -n ./mj-worker microvm cp ./dir t1:/workspace/dir ; sudo -n ./mj-worker microvm cp t1:/workspace/dir ./back
    sudo -n ./mj-worker microvm container exists t1 ; echo $?        # 0
    sudo -n ./mj-worker microvm container inspect t1                 # JSON array with Config.Labels, State.Status "running"
    sudo -n ./mj-worker microvm stop --time 0 --ignore t1 ; sudo -n ./mj-worker microvm start t1   # workspace data persists
    sudo -n ./mj-worker microvm rm --force --ignore t1 ; sudo -n ./mj-worker microvm volume rm t1-ws t1-tmp
    sudo -n ./mj-worker microvm image prune -f

The command-line contract is exactly the subset of Podman verbs, flags and output formats that mj's Podman SSH code uses (see Context). Unknown flags and unknown `--format` templates fail with a message naming them; the engine never ignores an argument silently. `--init` and `--stop-signal KILL` are accepted (the agent is always an init and stop always kills). `--cap-add`, `--security-opt` and `--user` are accepted with their container meaning applied inside the guest where it has one (`--user` sets the default exec user; capabilities and security options are no-ops because the VM is the boundary, documented in `info`). `--pull=missing|always|never` controls image flattening. `container exists` exits 0 or 1, and any other code means an engine error. `container inspect` prints a JSON array with one object carrying at least `Id`, `Name`, `Image`, `ImageName`, `Config.{Image,Labels,User,Env,WorkingDir,Cmd,Entrypoint}`, `HostConfig.{Memory,NanoCpus}`, `Mounts[{Type:"volume",Name,Destination,RW}]`, `State.{Status,Running}`; `State.Status` is `running` while the unit is active and `exited` otherwise. `info --format json` reports the engine version and state-format version, Firecracker version, kernel version, memory pool total and free (MiB), slots in use and total, and the CPU count.

State lives under `/var/lib/mj-microvm/` (images in `images/<digest>/{root.ext4,config.json}`, VMs in `vms/<name>/{state.json,root.ext4}`, volumes in `volumes/<name>/{disk.ext4,labels.json}`, kernels and initrds in `boot/<kver>-<engine-build>/`). All mutations take a per-name `flock`; IP and slot allocation take a global lock. `state.json` carries a `format` integer; readers accept every earlier format.

Image flattening runs `crane` (path from `info`; installed by Milestone 3, and on the spike host by hand) to export the filesystem and the config, extracts it as root preserving ownership, and runs `mkfs.ext4 -d` sized to the content plus headroom. The flattened image id reported by `image inspect` is the OCI image digest.

`run` allocates the IP and TAP, creates or validates volumes, reflink-clones the root, writes the Firecracker configuration (root drive first, then one drive per volume in argument order, network interface, vsock, `enclave: {direct_mode: true, cpu_overcommit: true}`), and starts `systemd-run --unit mj-microvm-<name> --collect -p RuntimeDirectory=mj-microvm/<name> -p ExecStopPost=<engine> microvm host-tap-down <name> firecracker --no-api --config-file …`. The kernel command line carries the name, address, gateway, DNS server (`169.254.169.253`), and the volume device-to-path map. It then waits until the guest agent answers on vsock and runs the container command (the image Entrypoint/Cmd or the given argv) as the container's main process with the image environment plus `--env`; when the main process exits the agent powers the VM off, so `State.Status` follows container semantics. A failed `run` stops the unit and removes the VM directory and any volume this call created, then exits non-zero with the reason.

The guest agent (`mj-worker microvm guest-agent`, PID 1 in the guest) reaps children, creates `/run/user/<uid>` for the image's user, mounts nothing that init already mounted, listens on vsock port 1024, serves exec and copy requests, and calls `reboot(2)` with power-off when the main process exits or a stop request arrives. The vsock protocol is a length-prefixed frame stream: a hello with protocol version in both directions, then one request (exec with argv, env, cwd, user, stdin flag, detach flag; copy-in with destination and a tar stream; copy-out with source producing a tar stream), then data frames (stdin, stdin-eof, stdout, stderr, tar chunk) and a final exit frame with code or signal. Detached exec starts the process in a new session and replies immediately. The engine propagates the exit code; it reads all streams concurrently and never writes all of stdin before reading output (AGENTS.md subprocess rules). Use the workspace `tar` crate.

The initrd `/init` follows `.agents/docs/nitro-sandbox-spike/sandbox-init` with these changes: the module list keeps the spike's full list; volumes are mounted from the command-line map; the engine binary is copied into a tmpfs at `/.mj-microvm/` in the new root and executed there as PID 1. The engine builds the initrd with dracut on first use for a `(kernel version, engine build)` pair, using the spike's `03-build-initrd.sh` arguments plus `--include <engine binary>`.

Tests to add in this milestone (AGENTS.md "Which tests to add"): an integration test of the vsock protocol between an engine client and the guest agent server across a Unix socket pair, driving more than 64 KiB in each direction with stdin closed early and late, exit codes and signals, and a hello with an older protocol version; a test that `state.json` written by the current format is read back and that an unknown newer format is refused with a clear message. The real end-to-end proof is the transcript on the spike host.

### Milestone 2: the `ssh_nitro` target kind

Add `TargetTemplate::SshNitro { ssh: SshConnection, container: ContainerTemplate }` (serde kind `ssh_nitro`, `kind_name` `ssh-nitro`) and the runtime and state `TargetLocator::SshNitro { ssh, container_id, … }` mirroring `SshPodman` minus Podman-only workspace storage (workspace is always a named volume). Handle the new variants at every site that matches on `TargetTemplate`, `TargetLocator`, `ImageHost`, `container_engine()`, `kind_name()`, `is_container_target` or `is_container(` in `mj-core`, `mj-controller`, `mj-cli` and `mj-tui` (a full site list with line numbers is in `.agents/docs/nitro-sandbox-spike.md` appendix "Integration sites"). The command prefix for this kind is `ssh <host> sudo -n <installed worker path> microvm`, used wherever SSH Podman uses `podman`. Provisioning builds the `run` command directly (no `PODMAN_VOLUME_RUN_SCRIPT`), because the engine owns volume creation and failure cleanup. Worker start, attach (`exec -i` under the existing drain wrapper), copies, recovery, cleanup, image refresh and preflight reuse the Podman code paths with the new prefix where their parsed outputs match the contract in Milestone 1; preflight replaces the Podman rootless and cgroup probes with `microvm info --format json` checks (Firecracker present, pool reserved, kernel ≥ 6.18.51). Capacity treats `ssh_nitro` memory like Apple's hard RAM admission and uses the engine's pool size as the host total. Additional host-path mounts and the mbx build cache are dropped for this kind with a visible notice at session creation. Storage attribution maps VM disks to `/var/lib/mj-microvm` on the SSH host. The PID-limit probe does not apply (each VM has its own kernel). The worker binary for the host is installed with the `ssh_bare` install machinery before the first engine call; worker binaries inside the guest follow the existing container rules (the release image carries its own). The TUI and target editors show the kind as "EC2 Nitro sandbox". Add the breaking migration widening `session_targets.kind` and its shape constraint, raising the minimum compatible revision in the same transaction, with a classification comment, as migration 98 did.

Tests: extend the existing schema migration tests for the new revision; extend an existing golden for the target list or doctor output to include an `ssh_nitro` target rather than adding a new family.

### Milestone 3: EC2 host bootstrap

Add `--runtime podman|nitro` (default `podman`) to `mj ec2 create`. For `nitro`: reject instance types that do not support Nitro Enclaves (`aws ec2 describe-instance-types` field `NitroEnclavesSupport`), use AMI `resolve:ssm:/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-6.18-<arch>`, pass `--enclave-options Enabled=true`, SSH user `ec2-user`, and bootstrap: `dnf install -y nftables e2fsprogs dracut git tar`, pinned Firecracker and jailer 1.18.0 and pinned `crane` downloaded with SHA-256 verification into `/usr/local/bin`, `grubby --update-kernel=ALL --args="nitro_enclaves.mempool=95%-1G"`, reboot, then verify kernel ≥ 6.18.51 and the `nitro_enclaves: CMA pool` line. Register an `ssh_nitro` target with the host's architecture as platform and preload the image through the existing refresher. Keep the Podman path unchanged.

The agent-dev image is unchanged; containers inside Nitro sessions are out of scope (see the Decision Log).

### Milestone 4: integration and live acceptance

Merge milestones 1–3, create a fresh host with `mj ec2 create --runtime nitro` from an isolated mj instance (`--instance nitro-live`, own `MJ_CONFIG_DIR` and `MJ_DATA_DIR`), and run the acceptance in Validation. Fix what it finds. Write the transcript into Artifacts.

### Milestone 5: jailer

Run Firecracker under the jailer (unprivileged uid, chroot, own namespaces) as Firecracker's production guide recommends; hard-link or bind the disks and the `/dev/nitro_enclaves` node into the chroot and adapt the socket path. Prove with the Milestone 1 transcript and the acceptance.

### Milestone 6: storage-preserving image upgrade

Support the controller's retained-storage image replacement for `ssh_nitro`: the engine replaces a stopped VM's root disk with a clone of the new image while keeping its volumes, which hold the workspace and worker control state. Prove that a session keeps its workspace across an image change.

## Concrete Steps

Work happens in detached worktrees, one per milestone owner: `git worktree add --detach ~/Projects/mjolnir-wt/<name> HEAD`. Build the portable worker for the host with `cargo build -p mj-worker --target x86_64-unknown-linux-musl` and copy it with `scp -i ~/.ssh/mj-nitro-spike.pem target/x86_64-unknown-linux-musl/debug/mj-worker ec2-user@$(cat ~/.ssh/mj-nitro-spike.ip):`. Run focused tests with `cargo test -p <crate> <filter>`; run the full suites of touched crates and `cargo clippy --all-targets -- -D warnings` once at the end.

## Validation and Acceptance

On a host created by `mj ec2 create --runtime nitro`, using an isolated instance: (1) `mj doctor` reports the Nitro target ready with pool size and Firecracker version; (2) a new session on it reaches a first turn and the agent reports a guest kernel and PID 1 `mj-worker`; (3) the agent runs `git clone` and `curl https://github.com`, and `curl -s -m 3 http://169.254.169.254/` fails; (5) `mj daemon restart --instance nitro-live` leaves the turn running and the session reattaches; (6) close and resume the session, with workspace contents intact; (7) destroy it, after which `sudo mj-worker microvm container exists`, `volume ls`, `ip link`, and `/sys/class/misc/nitro_enclaves/pcie_slots_in_use` show nothing left of it; (8) two sessions run concurrently and cannot reach each other; (9) a session asking for more memory than the pool has free is refused at admission with a message naming the pool.

## Idempotence and Recovery

Every engine verb is safe to repeat: `run` on an existing name fails without side effects; `stop`/`rm` with `--ignore` succeed on absent VMs; the nftables table and sysctl are reapplied before each launch. After a host reboot all units are gone and `start <name>` relaunches from retained disks. The spike host and any test hosts must be terminated when the work is accepted (`mj ec2 destroy` for mj-created hosts; `aws ec2 terminate-instances` for the spike host, then delete its security group and key pair tagged `Purpose=mj-1494`).

## Artifacts and Notes

Spike transcript and measurements: `.agents/docs/nitro-sandbox-spike.md`.

### Controller use of the engine (Milestone 2)

Every command is `ssh <host> <engine prefix> <verb...>`, so the engine sees exactly the verbs below. "Contract" means named in Milestone 1; "needs" means the controller depends on behavior Milestone 1 does not state, and the engine must provide it or the controller must change.

| Controller path | Command | Parsed output | Status |
| --- | --- | --- | --- |
| Launch preflight, doctor, Targets Test | `info --format json` | keys `engine_version`, `state_format`, `firecracker_version`, `kernel_version`, `memory_pool_total_mib`, `memory_pool_free_mib`, `slots_in_use`, `slots_total`, `cpus`; checks Firecracker present, pool > 0, kernel >= 6.18.51 | needs: exact key names |
| Capacity probe (every few seconds) | `info --format json` inside the host probe script, optional | pool total and free replace host RAM | needs: same keys |
| Image refresh | `image inspect --format '{{.Id}}' IMG`, `pull [--platform=P] IMG`, `image prune -f` | id text before and after | contract |
| Provision | `run --pull=missing\|always\|never --init --stop-signal KILL --detach --name N --label dev.mj.session=ID --label dev.mj.managed=true --label dev.mj.instance=I [--cpus C] [--memory M] [--env K=V]... [--env NAME]... --user 0:0 --env HOME=/home/hel --volume N-tmp:/tmp:rw --volume N-workspace:/workspace/ID:rw IMG sh -c KEEPALIVE` | exit status | contract, except valueless `--env NAME` for every secret-sourced value and `GH_TOKEN` (inherit from the engine's environment, which `sudo --preserve-env=NAME,...` supplies) and `--memory` given in bytes |
| Provision | `exec N sh -c 'chown 0:0 /tmp; chmod 1777 /tmp'` | status | contract |
| Git install, clone, worker install, all attached commands | `exec -i N argv...` under the existing drain wrapper | exit status, streams | contract |
| Worker start | `exec --detach N sh -c SCRIPT` | status | contract |
| Worker, profile and checkpoint files | `cp HOST ID:PATH`, `cp ID:PATH HOST` | status | contract |
| Recovery, Restart | `container exists N` (0, 1, other), `container inspect N` (JSON array of one with `Config.Labels`, `State.Status`), `start N` | as Podman | contract |
| Container storage | `container inspect N`, reads `Mounts[].Name/Destination` | JSON | contract |
| Quiesce | `stop --time 0 --ignore N`, `container exists N`, `container inspect --format '{{.State.Running}}' N` == `false` | text | contract (template named in Context) |
| Destroy | `stop --time 0 --ignore N`, `rm --force --ignore N`, `volume rm --force N-workspace N-tmp` | status | needs: `volume rm --force` ignores an absent volume |
| Absence check after a failed destroy | `container exists N`, `volume exists N-workspace`, `volume exists N-tmp` (0, 1, other) | status | needs: `volume exists` |
| Recovery scan | `ps --all --filter label=dev.mj.managed=true --format json` | one JSON object per line with `Labels` as `k=v,k=v` | needs: `ps` |

The controller does not use `exec --user`, `--mount`, `--cap-add`, `--security-opt`, `--pids-limit`, `--platform` on `run` (a VM runs the host architecture; the platform applies to `pull`), `rm --volumes`, or `run --rm --entrypoint`.

### Milestone 1 transcript

Milestone 1 transcript, 2026-11-10, on the spike host (m7i.4xlarge, AL2023 kernel 6.18.56, Firecracker 1.18.0, pool 48 GiB) with a stripped dev-profile musl build of `mj-worker` copied to `~/mj-worker`, run as `ec2-user` with `sudo -n`, from a clean `/var/lib/mj-microvm`. Commands are shown after `$`; `[exit N]` is the shell status. The first part is the lifecycle run (`transcript.sh`); the network part is a second run (`transcript-net.sh`) with the same engine because the first run's script guessed the second VM's address wrongly (its curl controls timed out for that reason, and nothing else in that run depended on it).


    $ sudo -n ./mj-worker microvm info --format json | head -24
    {
      "engine_version": "2.70.0",
      "state_format": 1,
      "protocol_version": 1,
      "kernel_version": "6.18.56-126.188.amzn2023.x86_64",
      "cpus": 16,
      "nitro_enclaves": true,
      "memory_pool_total_mib": 49152,
      "memory_pool_free_mib": 49148,
      "slots_in_use": 0,
      "slots_total": 80,
      "slots_enabled": 4,
      "firecracker_path": "/usr/local/bin/firecracker",
      "firecracker_version": "1.18.0",
      "crane_path": "/usr/local/bin/crane",
      "notes": [
        "--cap-add, --cap-drop and --security-opt are accepted and have no effect: the VM is the isolation boundary",
        "--user sets the default user of the container command and of exec",
        "volume option U changes only the owner of the volume's root directory"
    [exit 0]

    $ time sudo -n ./mj-worker microvm pull ghcr.io/brokkai/mjolnir/agent-dev:latest
    a398831a90a6832eeb0d3395bef6f35c722eba9ae481ef882f9effd0c048b470

    real	1m37.153s
    user	1m14.444s
    sys	0m19.323s
    [exit 0]

    $ sudo -n ./mj-worker microvm image inspect --format '{{.Id}}' ghcr.io/brokkai/mjolnir/agent-dev:latest
    a398831a90a6832eeb0d3395bef6f35c722eba9ae481ef882f9effd0c048b470
    [exit 0]

    $ time sudo -n ./mj-worker microvm run --detach --name t1 --label dev.mj.managed=true --label dev.mj.session=s1 --cpus 2 --memory 4g --env A=1 --volume t1-ws:/workspace:rw --volume t1-tmp:/tmp:rw ghcr.io/brokkai/mjolnir/agent-dev:latest sleep infinity
    2750a345b051ef840df60f1efe3962670b365855df31e80767aed426304adad6

    real	0m8.101s
    user	0m12.984s
    sys	0m1.776s
    [exit 0]

    $ sudo -n ./mj-worker microvm exec t1 sh -c 'uname -r; cat /proc/1/comm; id -un; echo A=$A; df -h /workspace | tail -1'
    6.18.56-126.188.amzn2023.x86_64
    mj-worker
    hel
    A=1
    /dev/vdb        196G   28K  196G   1% /workspace
    [exit 0]

    $ sha256sum /tmp/big200
    3d3d0a828c9b678513cb5edbb25ca491a1482df3ea8cb2714a6f37dc78c9ff68  /tmp/big200
    [exit 0]

    $ time sudo -n ./mj-worker microvm exec -i t1 sh -c 'cat >/workspace/big; wc -c /workspace/big; sha256sum /workspace/big' < /tmp/big200
    209715200 /workspace/big
    3d3d0a828c9b678513cb5edbb25ca491a1482df3ea8cb2714a6f37dc78c9ff68  /workspace/big

    real	0m0.398s
    user	0m0.041s
    sys	0m0.050s
    [exit 0]

    $ sudo -n ./mj-worker microvm exec -i t1 cat < /tmp/big200 | sha256sum
    3d3d0a828c9b678513cb5edbb25ca491a1482df3ea8cb2714a6f37dc78c9ff68  -
    [exit 0]

    $ sudo -n ./mj-worker microvm exec --detach t1 sh -c 'sleep 100'
    [exit 0]

    $ sudo -n ./mj-worker microvm exec t1 sh -c 'ps -eo pid,ppid,comm | grep -E "sleep|Z"'
     1018     1 sleep
     1045  1044 sleep
    [exit 0]

    $ sudo -n ./mj-worker microvm cp ./dir t1:/workspace/dir
    [exit 0]

    $ sudo -n ./mj-worker microvm cp t1:/workspace/dir ./back
    [exit 0]

    $ diff -r ~/dir ~/back && echo directories identical
    directories identical
    [exit 0]

    $ sudo -n ./mj-worker microvm container exists t1
    [exit 0]

    $ sudo -n ./mj-worker microvm container inspect t1 | python3 -c 'import json,sys; d=json.load(sys.stdin)[0]; print(len(json.load(open("/dev/null")) if False else [d]), d["State"], d["Config"]["Labels"], d["Mounts"][0]["Name"], d["HostConfig"])'
    1 {'Status': 'running', 'Running': True, 'Paused': False, 'ExitCode': 0} {'dev.mj.managed': 'true', 'dev.mj.session': 's1'} t1-ws {'Memory': 4294967296, 'NanoCpus': 2000000000, 'AutoRemove': False, 'Privileged': False, 'ReadonlyRootfs': False, 'NetworkMode': 'nitro'}
    [exit 0]

    $ sudo -n ./mj-worker microvm container inspect --format '{{index .Config.Labels "dev.mj.managed"}}|{{index .Config.Labels "dev.mj.session"}}' t1
    true|s1
    [exit 0]

    $ sudo -n ./mj-worker microvm exec t1 sh -c 'echo kept-across-stop > /workspace/marker.txt'
    [exit 0]

    $ time sudo -n ./mj-worker microvm stop --time 0 --ignore t1
    t1

    real	0m0.612s
    user	0m0.015s
    sys	0m0.013s
    [exit 0]

    $ sudo -n ./mj-worker microvm container inspect --format '{{.State.Status}}' t1
    exited
    [exit 0]

    $ sudo -n ./mj-worker microvm exec t1 true
    Error: can only use a running container: container state improper
    [exit 125]

    $ time sudo -n ./mj-worker microvm start t1
    t1

    real	0m2.451s
    user	0m0.048s
    sys	0m0.004s
    [exit 0]

    $ sudo -n ./mj-worker microvm exec t1 sh -c 'cat /workspace/marker.txt; sha256sum /workspace/big'
    kept-across-stop
    3d3d0a828c9b678513cb5edbb25ca491a1482df3ea8cb2714a6f37dc78c9ff68  /workspace/big
    [exit 0]

The network part, with controls that prove each blocked path would work without the firewall rules (the host reaches both the VM and itself):

    $ sudo -n ./mj-worker microvm run --detach --name t1 --label dev.mj.managed=true --label dev.mj.session=s1 --cpus 2 --memory 4g --volume t1-ws:/workspace:rw ghcr.io/brokkai/mjolnir/agent-dev:latest sleep infinity
    b8025ffb15cab36985eac83e3f5c0990656e39e576d875886624c99b03d2405c
    [exit 0]

    $ sudo -n ./mj-worker microvm run --detach --name t2 --label dev.mj.managed=true --label dev.mj.session=s2 --cpus 1 --memory 2g --volume t2-ws:/workspace:rw ghcr.io/brokkai/mjolnir/agent-dev:latest sleep infinity
    32028c5a4aa7811faf9f46d7068c3c64407db5a31f3748ad03ed8e1e2e2985d0
    [exit 0]

    $ cat /sys/class/misc/nitro_enclaves/pcie_slots_in_use
    2
    [exit 0]
    t2 address: 10.200.0.6   host VPC address: 172.31.8.23

    $ curl -s -m 3 -o /dev/null -w 'control: host -> t2 http %{http_code}\n' http://10.200.0.6:8080/
    control: host -> t2 http 200
    [exit 0]

    $ curl -s -m 3 -o /dev/null -w 'control: host -> host http %{http_code}\n' http://172.31.8.23:8099/
    control: host -> host http 200
    [exit 0]

    $ sudo -n ./mj-worker microvm exec t1 curl -s -m 3 -o /dev/null -w 't1 -> t2 http %{http_code}\n' http://10.200.0.6:8080/
    t1 -> t2 http 000
    [exit 28]

    $ sudo -n ./mj-worker microvm exec t2 curl -s -m 3 -o /dev/null -w 't2 -> t1 http %{http_code}\n' http://10.200.0.2:8080/
    t2 -> t1 http 000
    [exit 28]

    $ sudo -n ./mj-worker microvm exec t1 curl -s -m 3 -o /dev/null -w 't1 -> host (VPC address) http %{http_code}\n' http://172.31.8.23:8099/
    t1 -> host (VPC address) http 000
    [exit 28]

    $ sudo -n ./mj-worker microvm exec t1 curl -s -m 3 -o /dev/null -w 't1 -> host (TAP gateway) http %{http_code}\n' http://10.200.0.1:8099/
    t1 -> host (TAP gateway) http 000
    [exit 28]

    $ sudo -n ./mj-worker microvm exec t1 curl -s -m 5 -o /dev/null -w 't1 -> github.com http %{http_code}\n' https://github.com
    t1 -> github.com http 200
    [exit 0]

    $ sudo -n ./mj-worker microvm exec t1 curl -s -m 3 -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60'
    [exit 7]

    $ sudo -n ./mj-worker microvm rm --force --volumes t1 t2
    t1
    t2
    [exit 0]

    $ systemctl list-units 'mj-microvm-*' --no-legend | wc -l; ip -br link | grep -c '^mjt'; ls /var/lib/mj-microvm/vms /var/lib/mj-microvm/volumes | wc -l
    0
    0
    3
    [exit 0]

Removal and failed run, from the lifecycle run:

    === removal leaves nothing behind

    $ leftovers
    units:  2  taps: 2  sockets: 2  vm dirs: 2  volumes: 3  slots in use: 2
    [exit 0]

    $ time sudo -n ./mj-worker microvm rm --force --ignore t1
    t1

    real	0m0.616s
    user	0m0.030s
    sys	0m0.012s
    [exit 0]

    $ sudo -n ./mj-worker microvm volume rm t1-ws t1-tmp
    t1-ws
    t1-tmp
    [exit 0]

    $ sudo -n ./mj-worker microvm rm --force --volumes t2
    t2
    [exit 0]

    $ leftovers
    units:  0  taps: 0  sockets: 0  vm dirs: 0  volumes: 0  slots in use: 0
    [exit 0]

    $ sudo -n ./mj-worker microvm volume ls
    DRIVER    VOLUME NAME
    [exit 0]

    === a failed run leaves nothing behind

    $ sudo -n ./mj-worker microvm run --detach --name big --label dev.mj.session=s9 --memory 60g --volume big-ws:/workspace:rw ghcr.io/brokkai/mjolnir/agent-dev:latest sleep infinity
    Error: the Nitro Sandbox memory pool cannot fit this VM: pool 49152 MiB, 0 MiB committed to 0 running VMs, 61442 MiB needed
    [exit 125]

    $ leftovers
    units:  0  taps: 0  sockets: 0  vm dirs: 0  volumes: 0  slots in use: 0
    [exit 0]

    $ sudo -n ./mj-worker microvm container exists big
    [exit 1]

    === image prune

    $ sudo -n ./mj-worker microvm image prune -f
    [exit 0]

    $ sudo -n ./mj-worker microvm image inspect --format '{{.Id}}' ghcr.io/brokkai/mjolnir/agent-dev:latest
    a398831a90a6832eeb0d3395bef6f35c722eba9ae481ef882f9effd0c048b470
    [exit 0]

What the transcript shows: the engine reports the pool and Firecracker; the image flattens and inspects with the OCI config digest as its id; `run` returns a container id in 8 s the first time and 2.2 s afterwards; the guest reports its own kernel, PID 1 `mj-worker`, the image user `hel`, and a 196 GiB sparse workspace volume; a 200 MiB stdin round trip through `exec -i` has identical SHA-256 sums in both directions; a detached exec keeps running; a directory with a 5 MB file, a subdirectory and a symlink copies in and out identically; `container exists` and `container inspect` (JSON array and `--format`) answer as the controller's scripts expect; `stop --time 0` then `start` keeps the workspace; two VMs run at once and cannot reach each other, the host's VPC address, or the TAP gateway, while GitHub works and the IMDS token request fails with curl exit 7; after `rm` and `volume rm` there is no unit, TAP device, socket directory, VM directory, volume or used slot; a `run` asking for 60 GiB is refused naming the pool and leaves nothing; `image prune -f` keeps the referenced image. Other runs on the same host, not repeated in the transcript: three parallel 20 GiB `run` calls admitted exactly two; SIGTERM during `run` rolled back the unit, TAP, VM directory and volume; a hard kill of the Firecracker process left `exited` and `start` relaunched it; after a host reboot `start` rebuilt the nftables table and TAP and the synced workspace data was present; `exec -w /nonexistent` exits 127 and an unknown user exits 126.

### Milestone 4 live acceptance, 2026-11-11

Fresh host from an isolated instance (`MJ_INSTANCE=nitro-live`, own `MJ_CONFIG_DIR`/`MJ_DATA_DIR`, launcher under `env -i`), build `c95b16983`, profile `claude-aws` (Bedrock), fixture bundle cloned from `github.com/octocat/Hello-World`.

    $ mj ec2 create --runtime nitro --zone us-east-1d --name nitro-live m7i.4xlarge     (6 min 40 s)
    Launched i-0d8d21b935fa48552; waiting for EC2 status checks and cloud-init.
    Rebooting i-0d8d21b935fa48552 to reserve the Nitro Enclaves memory pool.
    Registered Nitro sandbox target nitro-live. Preloading ghcr.io/brokkai/mjolnir/agent-dev:latest.
    Created nitro-live (i-0d8d21b935fa48552) at 44.194.87.72 in us-east-1d.

    $ mj doctor
    ready Nitro sandboxes for target nitro-live: Firecracker 1.18.0, 60124 MiB sandbox memory pool (60120 MiB free), kernel 6.18.56-126.188.amzn2023.x86_64; image ghcr.io/brokkai/mjolnir/agent-dev:latest is present.

Session A (`mj new ... --target nitro-live`, first turn 80 s after `mj new` on a cold host), the agent's command output:

    6.18.56-126.188.amzn2023.x86_64
    mj-worker                                   (PID 1 is the guest agent)
    uid=0(root) gid=0(root) groups=0(root)
    4
    Mem:            7889 ...
    /dev/vda        196G  8.2G  188G   5% /
    /dev/vdb        196G   11M  196G   1% /tmp
    /workspace/07d29f220906f11f9a2129783f690188/fixture
    github 200
    curl: (7) Failed to connect to 169.254.169.254 port 80 after 0 ms
    imds refused rc=7

- Daemon restart during a turn that slept 60 s in the guest: `mj daemon restart` at guest uptime 49 s; the turn completed (62.9 s) with its full output. The workspace volume is `/dev/vdc`.
- Suspend (`--acknowledge-unpublished-work`, the fixture clone is unpublished): no VM, unit or volume left; pool back to 60120 MiB free. Resume: running 27 s later in a new VM (guest uptime 26 s); the uncommitted `MARKER.txt` is back.
- Daemon handoff to a newer build while A ran: A kept its VM (guest uptime 1069 s) and its background HTTP listener.
- Session B with `--memory-gib 48` (sees 48125 MiB): A's listener on 10.200.0.2:8000 (200 from A itself) times out from B, as do the TAP gateway and the host's VPC address on port 22; GitHub answers 200.
- With A (8 GiB) and B (48 GiB) running, an 8 GiB session C is refused: "this host has 2.7 GiB left" (engine: 2772 MiB free).
- Destroy A and B: no VM, unit, TAP, volume, Firecracker process or used slot; pool 60120 MiB free. Per-volume lock files remained (follow-up).

Milestone 5 transcript, 2026-11-11, on the spike host, engine `556b7f144` (stripped dev musl build) over VMs that an engine without the jailer had launched.

Upgrade case. The old engine (state format 1) launched `old1` (left running, workspace marker written) and `old2` (stopped). The engine binary was then replaced in place by the new one:

    $ mj-worker microvm info --format json | grep -E 'engine_build|state_format'    (before)  "state_format": 1
    $ mj-worker microvm info --format json | grep -E 'state_format|jailer_path'      (after)   "state_format": 2, "jailer_path": "/usr/local/bin/jailer"
    $ inspect --format '{{.State.Status}} isolation={{.HostConfig.Isolation}}'      old1: running isolation=none     old2: exited isolation=none
    $ exec old1 cat /workspace/marker           old1-data      (through the old socket path)
    $ cp /etc/hostname old1:/tmp/hn             [exit 0]
    $ start old2                                 old2: running isolation=jailer; marker old2-data; state.json "format": 2, "jailed": true
    $ stop --time 0 old1; start old1             old1: exited, then running isolation=jailer; marker old1-data
    $ ps -o user,uid,comm -C firecracker         3100000 firecracker, 3100001 firecracker
    $ rm -f -v old1 old2                         jails 0, TAPs 0, units 0

Two jailed VMs (`j1` 2 vCPU 2g with two volumes, `j2` 1 vCPU 1g). The full lifecycle and network checks of the Milestone 1 transcript pass unchanged; the lines that differ because of the jailer:

    j1: Uid 3100000 Gid 3100000 CapEff 0000000000000000 NoNewPrivs 1 Seccomp 2   root: /  cwd: /   mount ns mnt:[4026532900]   (host: mnt:[4026531832])
    j2: Uid 3100001 Gid 3100001 CapEff 0000000000000000 NoNewPrivs 1 Seccomp 2
    jail/firecracker/vm0/root:  drwx------ 3100000 .   dev/ (kvm, nitro_enclaves, urandom, userfaultfd, owned by 3100000)
                                 -rw------- 3100000 root.ext4 (2 links), vol0.ext4, vol1.ext4 (2 links); initrd.img, vmlinuz 0644 (3 links); fc.json; firecracker; v.sock
    $ ip -d tuntap show      mjt0: tap vnet_hdr persist user 3100000 group 3100000    mjt1: ... user 3100001 group 3100001
    as the VMM of i2 (setpriv --reuid=3100001):
      cat .../vm0/root/root.ext4     Permission denied        ls .../vm0/root          Permission denied
      ls /var/lib/mj-microvm          Permission denied        kill -0 <VMM of i1>      Operation not permitted
      cat .../volumes/i1-ws/disk.ext4 Permission denied
    $ exec -i j1 ... < 200MiB       209715200 bytes, identical SHA-256     $ cp dir / cp dir/. in and out     identical
    $ stop --time 0 j1              0.48 s, exited jailer     $ start j1      2.27 s, marker and 200 MiB file intact
    j1 -> j2 000 (exit 28), j1 -> host VPC address 000 (exit 28), j1 -> github.com 200, IMDS PUT exit 7; control host -> j2 200
    guest-initiated power off (run ... sh -c 'sleep 2; exit 3'): exited; its jail and TAP gone; start + exec cat /workspace/q works
    run --memory 60g: refused naming the pool, nothing left; SIGTERM mid-run: "interrupted", nothing left
    three parallel 20g runs: 2 admitted, 1 refused; after rm -f -v: units 0, TAPs 0, jails 0, VM dirs 0, slots in use 0
    host reboot with a jailed VM running: afterwards status exited, no unit/TAP/jail; start restored it with its data ("survives-reboot-jailed")

## Interfaces and Dependencies

`mj-worker/src/microvm/mod.rs` exposes the CLI entry `pub fn main(args: Vec<OsString>) -> anyhow::Result<i32>` dispatched from the worker binary's argument parser for the `microvm` subcommand. The vsock protocol types live in `mj-worker/src/microvm/protocol.rs` with `pub const PROTOCOL_VERSION: u32`. External tools on the host: Firecracker and jailer 1.18.0, crane (pinned), dracut, e2fsprogs, nftables, systemd. No new Rust crates beyond what the workspace already uses unless a milestone records why in the Decision Log.
