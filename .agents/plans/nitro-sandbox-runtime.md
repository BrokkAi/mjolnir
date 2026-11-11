# Run EC2 sessions in Nitro Sandboxes (Firecracker microVMs)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md`. Tracking issue: GitHub #1494.

## Purpose / Big Picture

Today an mj session on an EC2 host runs in a rootless Podman container. All sessions on that host share one Linux kernel, so a kernel bug or container escape in one session reaches every other session and the host. AWS Nitro Sandboxes (announced November 2026) run a workload in its own lightweight virtual machine whose memory and CPU boundary is enforced by the Nitro hypervisor, on ordinary (non-metal) EC2 instances, at no extra charge.

After this work a user can run `mj ec2 create --runtime nitro`, get a host registered as a target of kind `ssh_nitro`, and start sessions on it exactly as they do on a Podman target. Each session runs the same OCI image (by default `ghcr.io/brokkai/mjolnir/agent-dev`) inside its own microVM with its own kernel. The session behaves like any other: the harness runs a turn, the session survives a daemon restart, it can be checkpointed, closed, resumed and destroyed, and destroying it leaves nothing behind on the host.

To see it working: create a Nitro host, start a session on it with `mj new --target <ec2-nitro-target> ...`, ask the agent to run `uname -r; cat /proc/1/comm`, and observe a kernel and PID 1 that belong to the guest, not the host.

## Progress

- [x] (2026-11-10) Spike on a real m7i.4xlarge AL2023 host: agent-dev boots as a Nitro Sandbox in direct mode, vsock, two drives, NAT with IMDS refused, nested rootless Podman, stop/relaunch from retained disks. Notes and scripts: `.agents/docs/nitro-sandbox-spike.md` and `.agents/docs/nitro-sandbox-spike/`.
- [ ] Milestone 1: host engine `mj-worker microvm` and guest agent, proven by hand on the spike host.
- [x] (2026-11-10) Milestone 2: `ssh_nitro` target kind in mj-core, mj-controller, mj-tui, with breaking migration 124. Implemented against the Milestone 1 contract without the engine: unit-level and golden tests only. The controller's dependencies on the engine are listed under Artifacts and Notes, "Controller use of the engine"; nothing has run against a real host.
- [ ] Milestone 3: `mj ec2 create --runtime nitro` host bootstrap; agent-dev image runs nested Podman without a host socket.
- [ ] Milestone 4: integration and live acceptance on EC2.
- [ ] Milestone 5: Firecracker jailer.
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

- Observation (Milestone 2): `environment_workers`, the physical-ownership ledger from revision 122, carries its own `kind` CHECK, so a new target kind has two constraints to widen, not one. Rebuilding that table with foreign keys on would clear every `environments.target_lease_id` (`ON DELETE SET NULL` on the drop); migration 124 turns enforcement off for the rebuild and the migration test checks the lease survives.
- Observation (Milestone 2): `sudo` resets the environment and logs the full command line. A value that came from a reference (`from_env`, `from_secret`) or from the controller's own environment is therefore never put on the line: `container_environment` (mj-controller `controller/backend.rs`) emits a valueless `--env NAME` and returns the value, provisioning feeds the values on the ssh command's stdin (`CommandPlan::provide_target_environment_secrets`, which exports them in the remote shell), and the `run` prefix preserves exactly the valueless names (`--preserve-env=A,B`, derived from the same argument list). The engine must inherit a valueless `--env NAME` from its environment. Literal values stay as `--env NAME=value`.
- Observation (Milestone 2): The admission refusal text was written for Macs ("this Mac has N GiB left after the macOS reserve"). Plan acceptance step 9 needs a message that names the pool, so the text (in `mj-core/src/state.rs` and the web viewer) now speaks of "this host" and explains both hard limits.
- Observation (Milestone 2): The generic 20% host RAM reserve would have applied on top of the pool, which is already net of the host's own memory. A capacity reading now says whether its memory total is a boot-time pool (`DeploymentCapacityUsage::memory_pool`, set when the probe carries the engine's report), and `host_container_capacity` applies no reserve to a pool.
- Observation (Milestone 2, review): two admission gaps are left for integration. (1) Admission charges sessions against the pool total, not the engine's reported free pool, so memory used by another daemon's sandboxes or the 2 MiB per-sandbox overhead is not seen until the engine's `run` refuses (which cleans up and names the pool). (2) A host that mixes Nitro with other runtimes keeps its physical RAM as the capacity total.

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
- Decision: The daemon installs the host-architecture worker binary on the EC2 host with the same machinery it uses for `ssh_bare` hosts and invokes the engine from that path. The engine's on-disk state format and the guest-agent vsock protocol are versioned and stay readable by newer builds, because VMs and their agents outlive engine upgrades.
  Rationale: upgrades complete without user intervention (AGENTS.md).
  Date/Author: 2026-11-10, lead.

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

### Milestone 3: EC2 host bootstrap and image

Add `--runtime podman|nitro` (default `podman`) to `mj ec2 create`. For `nitro`: reject instance types that do not support Nitro Enclaves (`aws ec2 describe-instance-types` field `NitroEnclavesSupport`), use AMI `resolve:ssm:/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-6.18-<arch>`, pass `--enclave-options Enabled=true`, SSH user `ec2-user`, and bootstrap: `dnf install -y nftables e2fsprogs dracut git tar`, pinned Firecracker and jailer 1.18.0 and pinned `crane` downloaded with SHA-256 verification into `/usr/local/bin`, `grubby --update-kernel=ALL --args="nitro_enclaves.mempool=95%-1G"`, reboot, then verify kernel ≥ 6.18.51 and the `nitro_enclaves: CMA pool` line. Register an `ssh_nitro` target with the host's architecture as platform and preload the image through the existing refresher. Keep the Podman path unchanged.

In `containers/Containerfile.agent-dev`, install the real Podman engine pieces the spike needed (`podman crun uidmap fuse-overlayfs passt`, `containers.conf.d` with `cgroup_manager="cgroupfs"` only when no systemd) and replace the `/usr/local/bin/podman` symlink with a small wrapper that uses `podman-remote` when the host socket that container sessions mount is present and the real engine otherwise, so nested containers work in both runtimes.

### Milestone 4: integration and live acceptance

Merge milestones 1–3, create a fresh host with `mj ec2 create --runtime nitro` from an isolated mj instance (`--instance nitro-live`, own `MJ_CONFIG_DIR` and `MJ_DATA_DIR`), and run the acceptance in Validation. Fix what it finds. Write the transcript into Artifacts.

### Milestone 5: jailer

Run Firecracker under the jailer (unprivileged uid, chroot, own namespaces) as Firecracker's production guide recommends; hard-link or bind the disks and the `/dev/nitro_enclaves` node into the chroot and adapt the socket path. Prove with the Milestone 1 transcript and the acceptance.

### Milestone 6: storage-preserving image upgrade

Support the controller's retained-storage image replacement for `ssh_nitro`: the engine replaces a stopped VM's root disk with a clone of the new image while keeping its volumes, which hold the workspace and worker control state. Prove that a session keeps its workspace across an image change.

## Concrete Steps

Work happens in detached worktrees, one per milestone owner: `git worktree add --detach ~/Projects/mjolnir-wt/<name> HEAD`. Build the portable worker for the host with `cargo build -p mj-worker --target x86_64-unknown-linux-musl` and copy it with `scp -i ~/.ssh/mj-nitro-spike.pem target/x86_64-unknown-linux-musl/debug/mj-worker ec2-user@$(cat ~/.ssh/mj-nitro-spike.ip):`. Run focused tests with `cargo test -p <crate> <filter>`; run the full suites of touched crates and `cargo clippy --all-targets -- -D warnings` once at the end.

## Validation and Acceptance

On a host created by `mj ec2 create --runtime nitro`, using an isolated instance: (1) `mj doctor` reports the Nitro target ready with pool size and Firecracker version; (2) a new session on it reaches a first turn and the agent reports a guest kernel and PID 1 `mj-worker`; (3) the agent runs `git clone` and `curl https://github.com`, and `curl -s -m 3 http://169.254.169.254/` fails; (4) the agent runs `podman run --rm alpine echo ok` (nested); (5) `mj daemon restart --instance nitro-live` leaves the turn running and the session reattaches; (6) close and resume the session, with workspace contents intact; (7) destroy it, after which `sudo mj-worker microvm container exists`, `volume ls`, `ip link`, and `/sys/class/misc/nitro_enclaves/pcie_slots_in_use` show nothing left of it; (8) two sessions run concurrently and cannot reach each other; (9) a session asking for more memory than the pool has free is refused at admission with a message naming the pool.

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

## Interfaces and Dependencies

`mj-worker/src/microvm/mod.rs` exposes the CLI entry `pub fn main(args: Vec<OsString>) -> anyhow::Result<i32>` dispatched from the worker binary's argument parser for the `microvm` subcommand. The vsock protocol types live in `mj-worker/src/microvm/protocol.rs` with `pub const PROTOCOL_VERSION: u32`. External tools on the host: Firecracker and jailer 1.18.0, crane (pinned), dracut, e2fsprogs, nftables, systemd. No new Rust crates beyond what the workspace already uses unless a milestone records why in the Decision Log.
