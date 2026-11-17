# Nitro Sandbox spike for #1494 (2026-11-10/11)

**Result: the whole path works on a real EC2 Nitro host.** The `agent-dev` OCI image boots as a Nitro
Sandbox (direct mode, `cpu_overcommit`), talks over vsock, has two drives, NAT networking with IMDS
refused, and runs nested rootless Podman as `hel`. Two sandboxes ran concurrently; `systemctl stop`
returned slot and memory; the same id relaunched from retained disks. Everything below was run on the
host; every PASS is in `evidence/`.

## Host (left running)

| | |
|---|---|
| Instance | `i-0b0932db78fa86aaf`, **m7i.4xlarge** (16 vCPU, 64 GiB; first choice, no capacity problem), us-east-1d |
| Public IP | `<spike-host-ip>` (changes on stop/start; stored locally in `~/.ssh/mj-nitro-spike.ip`) |
| SSH | `ssh -i ~/.ssh/mj-nitro-spike.pem ec2-user@<spike-host-ip>` (key pair `mj-nitro-spike`, ed25519) |
| Security group | `sg-0ee133d75d641d355`, SSH 22 only from `<operator-ip>/32` |
| Root EBS | `vol-058cc1b0d566e128d`, gp3 200 GiB, 6000 IOPS, 500 MiB/s, delete on termination |
| OS / kernel | AL2023 `2023.12.20261109`, kernel `6.18.56-126.188.amzn2023` (already >= 6.18.51-120.162; no upgrade needed) |
| Firecracker | v1.18.0 + jailer (jailer **not exercised**) in `/usr/local/bin` |
| Pool | `nitro_enclaves.mempool=48G@0`; `pcie_slots` raised 4 -> 12 (resets at reboot) |
| Tags | `Name=mj-nitro-spike`, `Purpose=mj-1494` on instance, volume, key pair, security group |
| Cost | about $0.81/h compute plus about $0.035/h for EBS. Stop sandboxes freely; stop the instance when idle. |

State on the host: `~/spike/` has the scripts (via `scripts/deploy.sh`), `/var/lib/sandboxes/{base,kernel,sbx}`
has images, kernel and per-sandbox disks. All sandboxes are currently **stopped** (instance running).
TAP devices and the nft table do not persist across a reboot: rerun `~/spike/04-host-net.sh`.

## Recipe (scripts in `scripts/`, run order)

0. `00-provision.sh` (local): key pair, security group, instance with `--enclave-options Enabled=true`.
1. `01-host-prep.sh` (on host): nftables, socat, `crane`, Firecracker 1.18.0, `grubby ... mempool=48G@0`,
   reboot. `01-host-prep.sh verify` afterwards.
2. `02-build-image.sh`: `crane export` -> extract -> chroot `apt-get install` -> `mkfs.ext4 -d` root image
   plus an empty 20 GiB workspace ext4.
3. `03-build-initrd.sh`: copy `/boot/vmlinuz-$(uname -r)` (EFI stub), dracut initrd containing `guest/sandbox-init`
   as `/init` (`--modules bash`, no `base`).
4. `04-host-net.sh`: ip_forward and the `ip sandbox` nft table (wildcard `tap*`).
5. `05-launch.sh N [--fresh] [--mem] [--vcpus] [--no-debug] [--confidential]`: reflink-clone disks, write
   Firecracker JSON, `systemd-run` the transient unit `sandbox-N`, wait for vsock.
6. `06-stop.sh N`: `systemctl stop`. The unit does its own cleanup (see "Lifecycle").
7. `08-acceptance.sh N` (14 checks, all PASS on two sandboxes after a clean rebuild: `evidence/acceptance.log`),
   `09-vsock-bulk.sh N`. `deploy.sh` (local) pushes everything to the host.

Working Firecracker config: `firecracker-config.sbx1.json`. The key parts:

```json
"drives": [ {"drive_id":"rootfs","path_on_host":".../root.ext4","is_root_device":true,"is_read_only":false},
            {"drive_id":"workspace","path_on_host":".../workspace.ext4","is_root_device":false,"is_read_only":false} ],
"vsock":   {"guest_cid":3,"uds_path":"/run/sandbox/1/v.sock"},
"enclave": {"direct_mode":true,"cpu_overcommit":true,"debug_mode":true}
```

Run as: `systemd-run --unit=sandbox-1 --collect -p RuntimeDirectory=sandbox/1 -p RuntimeDirectoryPreserve=no
-p ExecStartPre="sbx-tap up 1" -p ExecStopPost="sbx-tap down 1" firecracker --no-api --config-file fc.json`.

## Measurements

| What | Result |
|---|---|
| Memory pool | `nitro_enclaves: CMA pool: 49152 MiB reserved in 16 region(s)`; `pcie_slots=4`, `pcie_slots_total=80`, `cpu_pool/free=14` (16 vCPU minus first core's 2 threads) |
| Host memory before pool | `free -m`: total 63258, free 62349, avail 62318 |
| Host memory after pool, no sandbox | total 63330, free 62506, avail 62217; `CmaTotal` 50331648 kB, `CmaFree` 49900904 kB |
| **Can the host use unclaimed pool memory?** | **Yes, for movable pages (page cache).** The pool is CMA, so it still counts as free. After reading four 11 GiB images, `CmaFree` fell to 7283 MiB and page cache was 45 GiB. A 44000 MiB sandbox then still launched: the 42 GiB `CMA alloc` took 4.4 s (migrating page cache out), 7.8 s launch total. Pool memory is returned on stop. Non-movable kernel allocations cannot use it. |
| Pool cost per sandbox | mem + 2 MiB (4096 MiB -> 4098 MiB in `mem_pool/free`); +128 MiB per extra 8 slots |
| OCI pull + extract (8.3 GiB, 92k inodes) | 61 s (`crane export`, anonymous ghcr pull) |
| Customize (apt podman etc.) | 8-10 s (+123 MiB) |
| `mkfs.ext4 -d` | 35 s, 11207 MiB sparse image, about 8.3 GiB used |
| **Clone of both disks, `cp --reflink=always`** | **4-5 ms**; XFS `reflink=1`; `df` used identical before/after clone (0 MiB) |
| **Launch -> vsock ready** (2 vCPU, 4 GiB, retained or fresh disks) | **2.6-3.3 s** (median about 2.9 s), includes 0.1 s poll loop with `sudo`+python. About 0.55 s Firecracker EIF build and memory donation, about 1.4 s firmware+kernel to `/init`, about 0.3 s module loading and mounts in init, rest agent start. `--no-debug` 2.6 s; `--confidential` 3.3 s (stock AL2023 kernel has DMB; boots fine). |
| 4 sandboxes launched in parallel (1 GiB, 1 vCPU) | all ready in 2.7-2.9 s, wall 2.95 s |
| `systemctl stop` of a running guest | 0.27 s |
| vsock throughput | guest->host 1 GiB in 1.58 s (about 680 MB/s); host-initiated stream 512 MiB in 1.69 s (about 320 MB/s, includes `tr` in guest) |
| Idle sandbox host cost | `firecracker` RSS 4.2 GiB for a 4 GiB guest (the mapped CMA, not extra memory), 0.1-0.4% CPU |
| Dedicated CPUs (`cpu_overcommit:false`), 2 vCPUs | works; `cpu_pool/free` 14 -> 12 |

## What had to be added (vs "boot the image")

Image side (it is a Debian 13 container image: no init, no kernel, no net tools):
- Packages installed in the tree: `podman crun iproute2 kmod socat uidmap fuse-overlayfs passt netavark aardvark-dns
  nftables iputils-ping dnsutils ca-certificates util-linux`. The image already has `git curl bash sudo setpriv runuser`
  and user `hel` (1000) with `/etc/subuid` 100000:65536, WORKDIR `/workspace`.
- **`/usr/local/bin/podman` is a symlink to `podman-remote`** (talks to a host socket). Nested containers need the
  real engine: call `/usr/bin/podman`, or the real agent must repoint the symlink / set `CONTAINER_HOST`.
- `/etc/containers/containers.conf.d/50-sandbox.conf` with `cgroup_manager="cgroupfs"` (no systemd in the guest).
- `/etc/hosts` is absent in the image; init writes it.

Guest init (`guest/sandbox-init`, in the initrd; no udev, no systemd):
- Modules loaded: `virtio_pci virtio_blk virtio_net ext4 vmw_vsock_virtio_transport nsm overlay fuse tun veth bridge
  br_netfilter nf_tables nft_ct nft_nat nft_masq nft_chain_nat nft_compat nf_conntrack nf_nat nft_reject{,_inet}
  nf_reject_ipv4 xt_MASQUERADE xt_conntrack xt_addrtype xt_comment xt_mark xt_nat iptable_nat` (all present in the
  AL2023 kernel as `.ko.zst`; `ip_tables` is built in). `nsm` is a no-op in direct mode.
  **Only the first group is needed for boot + vsock + net + disks; `overlay fuse tun` are what rootless podman used.**
  The nft/veth/bridge set is for future rootful nested networking and was not exercised.
- `chmod 666 /dev/net/tun /dev/fuse` (udev normally does this), `/dev/fd,stdin,stdout,stderr` links, `/dev/pts`, `/dev/shm`,
  `/run` tmpfs, cgroup2 on `/sys/fs/cgroup`, `mount --make-rshared /newroot` before `switch_root`.
- `/lib/modules/<kver>` must exist in the new root for later `modprobe`. It is **copied into a tmpfs**
  mounted at `/newroot/usr/lib/modules` (about a few MiB). A bind mount does not work: it shares the ramfs
  `st_dev`, so `switch_root` deletes it.
- Network from the cmdline (`sbx.ip= sbx.gw= sbx.dns= sbx.name=`), hostname, `/etc/hosts`, `/etc/resolv.conf`
  (`169.254.169.253` works through the NAT).
- Agent start-up must create `/run/user/1000` (`XDG_RUNTIME_DIR`) owned by `hel`.

## Networking (verified from the guest)

Per sandbox `tapN`, 10.77.N.0/30 (host .1, guest .2). Rules in `04-host-net.sh` (table `ip sandbox`):
postrouting masquerade for `iifname "tap*"`, forward `reject` to 169.254.169.254, forward `drop` tap-to-tap,
input policy for taps = established/related + ICMP echo only (so the parent's SSH port is closed to guests).

| Check | Result |
|---|---|
| DNS (`getent hosts github.com`), `curl https://github.com` -> 200 (0.11 s), `git clone --depth 1` (0.5 s) | pass |
| IMDS token PUT | refused (`curl: (7) Failed to connect ... after 0 ms`). **Control:** with the reject rule removed, the same PUT returns **200** through IMDSv2 and hop limit 2, so the rule is what blocks it. |
| Parent SSH via tap IP and via parent VPC IP <parent-vpc-ip> | times out (input drop) |
| sbx2 -> sbx1 ping | 100% loss (forward drop); sbx -> own gateway ping works |
| IPv6 IMDS `fd00:ec2::254` | unreachable (guest has no v6 route) |
| VPC resolver `172.31.0.2` | **reachable** (DNS answers) |

Not restricted yet, and a policy decision for mj: sandboxes can reach any other VPC address (and the
VPC resolver) through the NAT. Add an egress deny for the VPC CIDR / RFC1918 if tenants must not reach
siblings or internal services.

## Nested containers

`/usr/bin/podman run --rm docker.io/library/alpine echo nested-ok` as `hel` works (rootless, `overlay` via
`fuse-overlayfs` mount program, `pasta` networking, `cgroupfs`), including outbound HTTPS from inside the
nested container. First run (pull + run) took 0.55 s with a cached layer. Limits: `podman info` shows
`cgroup=cgroupfs`; `hel` cannot create cgroups (root-owned cgroup2 root), so `--memory/--cpus` limits on
nested containers will not work without delegating a cgroup subtree to `hel` (not tested).

## Lifecycle (step 7)

- Two sandboxes ran concurrently on separate taps/sockets. With them: `pcie_slots_in_use=2`,
  `mem_pool/free` fell by 4098 MiB each.
- `systemctl stop sandbox-1`: `pcie_slots_in_use` 2 -> 1 and `mem_pool/free` back up by 4098 MiB immediately.
  CPU pool unchanged under `cpu_overcommit`.
- **Cleanup is structural:** `RuntimeDirectory=sandbox/N` (systemd creates the socket directory empty at start and
  deletes it at stop, so no stale `v.sock`) and `ExecStopPost=sbx-tap down N` (TAP removal on every exit path:
  stop, crash, failed start). Disks are kept.
- Relaunch of the same id from retained disks: ready in 2.8-3.1 s, `/workspace/marker.txt` written in the
  previous run was present, and the Podman storage in `~hel` persisted.
- Guest-initiated exit: `echo o > /proc/sysrq-trigger` (power off) and a guest kernel panic (`panic=1`
  reboots) both make Firecracker exit 0 within about 1 s; unit goes inactive, slot and pool return, TAP
  and socket removed. The image has **no `poweroff`/`reboot` binary**; the real agent must call `reboot(2)`.
- After every failed launch (see below) `pcie_slots_in_use` and `mem_pool/free` were unchanged: no leak.

## Surprises and failure modes (exact text)

1. **AL2023 has no `podman`, `skopeo` or `buildah`** (`Error: Unable to find a match: podman`). I used `crane`
   (static, anonymous pull from public ghcr) to export the image; Docker was avoided because it sets the
   `FORWARD` policy to DROP and would fight the sandbox nft rules. The mj implementation must flatten
   images without Podman on the parent, or install Podman from elsewhere.
2. **Root drive works in Nitro direct mode as documented.** `is_root_device:true` makes Firecracker append
   `console=ttyS0 root=/dev/vda rw` (the console part only with `debug_mode`); my initrd `/init` ignores
   `root=` and mounts `/dev/vda` itself. Block devices appear in config order: `vda` root, `vdb` workspace.
3. **vsock socket is `root:root 0755`**, so a non-root client gets `PermissionError: [Errno 13] Permission denied`
   on `connect()`. mj's daemon must run Firecracker as root, chown the socket, or use the jailer (untested).
4. **Slot exhaustion** (5th sandbox with 4 slots): `Failed to create enclave VM: No space left on device`
   (`Os { code: 28, kind: StorageFull }`). `echo 12 | tee /sys/class/misc/nitro_enclaves/pcie_slots` fixes it live and
   costs exactly 128 MiB of pool.
5. **Pool exhaustion** (asked 60000 MiB with 49 GiB pool): `CMA allocation failed: size=60767076352 bytes ...
   Cannot create mmap region: Out of memory (os error 12)`, then `Failed to allocate CMA memory`.
6. **Stale socket:** `Vsock device error: Cannot create backend for vsock device: Error binding to the host-side
   Unix socket: Address in use (os error 98)`. Solved structurally by `RuntimeDirectory`.
7. **Config without `drives`:** `Invalid JSON: missing field 'drives' at line 27 column 1`.
8. **Unsupported device:** `Enclave builder error: A Nitro Enclave does not support a balloon device`.
9. **Memory below minimum** (32 MiB): `Failed to start enclave: NE_ERR_ENCLAVE_MEM_MIN_SIZE (errno=268): the enclave
   memory size is lower than the minimum supported`.
10. Bad kernel path: `The kernel file cannot be opened: No such file or directory (os error 2)`.
11. Without udev, `/dev/net/tun` is `crw------- root` and pasta fails: `Failed to open() /dev/net/tun: Permission
    denied` / `Failed to set up tap device in namespace`. Also `"/" is not a shared mount` warning until
    `make-rshared`.
12. **Corrupt rootless storage after a failed first run:** after the pasta failure left a half-pulled image, the next
    run failed with `Error: mounting storage for container ...: readlink .../overlay/l: invalid argument`, and
    `rm -rf` as `hel` could not delete the userns-owned files (needs `podman unshare rm -rf`). A fresh disk
    does not hit this; the agent should not leave first-run podman state on a failed bring-up.
13. **Reflink clones do not share page cache.** Four reflinked 11 GiB roots read from four files cost 4x
    page cache on the parent (45 GiB cached). Reads went through the host page cache (observed: cache grew to 45 GiB), so host cache
    plus guest cache can both hold hot blocks. It is reclaimable (and CMA-resident), but memory pressure on the
    parent and cold-read speed of gp3 (reading 4 x 8.7 GiB took 113 s) matter at scale.
14. My `vsh` client first buffered output with quadratic byte concatenation and hung on 512 MiB; the final
    one streams. Not a vsock problem. Also `pkill -f <socket name>` over ssh kills its own shell; use pids.
15. Direct mode has no entropy device configured; guest booted with no crng stall observed. `debug_mode` puts the
    guest console in the unit journal (about 150 lines per boot); `--no-debug` prints about 33 lines.

## Not tested / open questions

- **Jailer** was installed but not used. Needed for the real design (privilege drop, chroot access to
  `/dev/nitro_enclaves` per the Firecracker docs); the socket ownership and `uds_path` layout change under it.
- **Graviton/arm64** not tried (the image is multi-arch; the AMI/instance would change).
- **Rootful nested networking** (netavark bridge/veth, nft inside the guest) was not exercised, only rootless
  pasta. Modules for it are in the initrd.
- **Parent reboot:** docs say stop/reboot of the parent terminates sandboxes; the
  mj daemon must relaunch from retained disks after reboot (works, per the relaunch test) and re-run
  `04-host-net.sh`, the slot raise and the TAPs. The memory pool is a kernel argument and persists.
- **Slot cap** is a Nitro limit (`pcie_slots_total=80` here), not a cost in memory beyond 128 MiB per 8.
- **Page-cache vs CMA under sustained pressure** (item 13) with many concurrent launches is unmeasured beyond
  the 42 GiB test.
- **Egress policy** to the VPC (above).
- **Real guest agent**: needs to be PID 1 (or run under a tiny init) to reap zombies, create
  `/run/user/1000`, own the exec protocol, call `reboot(2)` on shutdown, and handle SIGTERM from the
  `switch_root` hand-off. The spike agent is `socat VSOCK-LISTEN:1024,fork EXEC:vsock-exec`.

## Files

- `scripts/` provisioning, host prep, image, initrd, net, launch, stop, acceptance, bulk vsock, deploy
- `guest/sandbox-init` (initrd `/init`), `guest/vsh` (host vsock client)
- `firecracker-config.sbx1.json` (the config that worked)
- `evidence/` `acceptance.log`, `slots.log` (slot/pool/error battery), `image-build.log`, `vsock.log`,
  `boundary-checks.log`, `host-final.txt`, `instance.txt`


# Appendix: Integration sites

Read-only map of where a new target kind must be handled (2026-11-10, against 6c8db87a6).

### 1. Enum and target-kind integration sites

The runtime is modeled by parallel config, runtime-plan, and durable-state enums rather than a backend trait. The compiler will expose many missed exhaustive cases, but `matches!`, `if let`, string classification, and intentional refusal paths also need review.

#### Core model, conversion, validation, persistence

- `mj-core/src/config/targets.rs:517-570` defines the user config `TargetTemplate` (Serde tagged `kind`, snake_case); `:573-835` has `container()`, `kind_name()`, launch refusal, resource-allocation kind, `is_container_target()`, and host classification (`mount_history_host`, `project_history_host`, `container_size_host`). These classify template shape and remote host behavior. `:350-413` is `ContainerTemplate` and workspace-storage configuration; `:773` is the shared container classifier.
- `mj-core/src/targets.rs:2316-2358` `ImageHost`: `engine`, human log label, and direct-vs-SSH command wrapping. `:2399-2455` `image_refresh`: inspect/pull/prune syntax, platform support, and pull policy. `:2482-2504` runtime-plan `TargetTemplate`; `:2527-2600` runtime `TargetLocator`; `:2602-2629` template engine/image-host mapping; `:2633-2666` locator CPU accounting, kind spelling, and engine classification. `:2671-2685` recovery-plan contract. `:2820-2891` target command dispatch and worker-root selection; `:2902-2956` exec/SSH output-drain wrapping.
- `mj-core/src/targets/convert.rs:119-145` config template → image host; `:220-356` DB recorded-target → runtime locator, joining saved host/SSH configuration and runtime settings; `:368-390` locator kind string and whether a live SSH connection is needed; `:407-445` connection/target consistency logic. `mj-core/src/targets/ssh.rs:682-790` verifies a locator against session identity, validates the host/resource and determines borrowed ownership. `mj-core/src/state.rs:869-920` is the separately serialized DB/API `state::TargetLocator`; `:3373-3462` equality/identity comparisons must include the new shape. `mj-core/src/state/target_runtime.rs` carries connection/runtime settings captured at launch.
- DB storage is explicit rather than serializing the complete locator: `mj-controller/src/database/schema.rs:1763-1804` migration 98 `session_targets.kind` CHECK and shape constraints; `mj-controller/src/database/state_io.rs:845-979` encodes `kind,host,resource_id,address,workspace,worker_id,workspace_storage,borrowed_from`; `:1041-1125` decodes it. `mj-controller/src/database/state_io.rs:565-650, 688-783` separately serializes `target_runtime_json` as JSON. A new SSH VM kind needs both schema allowed-kind/shape and encode/decode branches.
- Config `TargetTemplate` persists in `config.toml` as a tagged TOML table, not a DB migration (`mj-core/src/config/targets.rs:517`; `mj-core/src/config/mod.rs` config loading). Durable session locator is the `session_targets` row above; worker-restart intents and move/checkpoint records also carry JSON locator snapshots (`schema.rs:984-990`, `:4763+`; inspect the matching state IO for any new persisted operation that embeds it). API session snapshots use the shared `mj-core::state::SessionRecord`/`TargetLocator` Serde types; `mj-client` does not define a second target enum. Viewer/session projections generally carry target-template IDs and rendered location, not a new runtime enum (`mj-controller/src/server/viewer_types.rs:273,329`).

#### Controller runtime decisions and command plans

- `mj-controller/src/controller/backend.rs:1062-1120` creates a durable locator from selected config target. `:1126-1157` converts recorded locator plus captured target runtime settings back to current runtime behavior. `:761-810` merges configured target image-refresh plans by image host.
- `mj-controller/src/targets/provision.rs:25-90` treats remote Apple and Docker by planning the local engine operation and wrapping it over SSH; `:91-207` matches each target to create/install-git/clone plans. Nitro needs its own create semantics if `mj-worker vm` isn't a drop-in container CLI. `mj-controller/src/targets/container.rs:1-43,145-225,312-386` selects engine and builds Podman flags, mounts, volumes, labels, keepalive and run script; volume/host-helper branches are especially Podman-specific.
- Recovery and destruction: `mj-controller/src/targets/recovery.rs:45-139` returns existence/inspect/start plans by locator (currently only Podman and local Docker recover; Apple deliberately no plan); `:188-225` parses a one-element inspect JSON array, checks managed/session labels and `/State/Status`. `mj-controller/src/targets/cleanup.rs:39-94,278-330,432-500,548-620` decides quiesce, container/volume/helper removal, ownership proof and post-delete absence; remote Podman commands are shell scripts sent through SSH. Include retained-storage and borrowed-target ownership semantics.
- Worker lifecycle: `mj-controller/src/controller/worker_binary/process.rs:283-391` launches worker detached (remote Podman exact `podman exec --detach`); `:447-550` probes startup/exit records and live PIDs. `mj-controller/src/controller/worker_binary/install.rs:815-1017` stages launch/ownership/profile into local/remote containers using `exec`, `cp`, `scp`, ownership/chmod; `:611-690` has explicit Podman/Docker-only mbx config linking. `mj-controller/src/controller/worker_binary/upgrade.rs:344-370,588-716` selects remote SSH and local/remote engine copy/replacement command; also audit its target matches for unsupported fallback. `mj-controller/src/controller/worker_binary/launch.rs:970-981,1332-...` classifies managed-worktree target and harness placement. `mj-controller/src/controller/checkpoint/staging.rs:315-372` copies checkpoint data into/out of local/remote target. `mj-controller/src/controller/provisioning.rs:67-75,1527-1562,1585-1663,1803-1812` selects target identity, target/SSH conversion and container-specific setup.
- Capacity, storage and admission: `mj-controller/src/targets/process_limit.rs:62-72` is the container classification for in-container PID limits; `:74-106` parses exactly two whitespace tokens (`pids.current`, `pids.max`/`max`). `mj-controller/src/target_storage.rs:264-297,341-347` maps locator to storage host/filesystem and filesystem path; `mj-core/src/targets/storage.rs:123+` projects session storage paths. `mj-controller/src/daemon/capacity.rs:280-315` distinguishes Apple hard RAM admission from cgroup ceilings; add VM resource enforcement/admission policy explicitly. `mj-controller/src/controller/cache_host.rs:80-122`, `mj-controller/src/controller/target_storage.rs` (actual file is `mj-controller/src/target_storage.rs`), `mj-controller/src/controller/mbx.rs`, `mj-controller/src/controller/tool_cache.rs`, `mj-controller/src/controller/network_git.rs`, `mj-controller/src/controller/subagents.rs`, `mj-controller/src/checkpoint_transfer.rs`, and `mj-controller/src/pollers/quota.rs` contain host-placement/target-classification matches. These decide whether cache/build operations run locally, on SSH host, or inside target; use `rg 'TargetTemplate::|TargetLocator::|ImageHost::'` to review all current production hits.
- Preflight and doctor: `mj-controller/src/targets/preflight.rs:805-885,888-987,990+` probes Podman version/info/rootless/remote service/UID mappings/cgroup controllers; `:937-963` judges cgroup v2 controller presence; SSH batching/host probes elsewhere in same file. These are Podman prerequisites, not a VM preflight. `mj-controller/src/doctor.rs:210-230,1418-1645,2128-2474,3182-3240` assembles local/SSH runtime checks, image check, host process limits, readiness IDs and default-target remediation; add distinct VM diagnostics and inventory.
- Container retained upgrade is a large special path: `mj-controller/src/targets/container_upgrade.rs:132-149` selects supported engine; `:152-187` parses inspect JSON; `:190-310` ownership, entrypoint, resource, environment and mount validation; `:620-671,680-900` image pull/pin, replacement, retained storage and rollback; `:1036+` resource cleanup. `mj-controller/src/controller/container_upgrade.rs:238-240,291-294` maps SSH and template for orchestration. Do not assume this lifecycle applies to a whole microVM; define VM snapshot/root-disk replacement semantics or explicitly reject upgrade while preserving current VM.

#### UI/configuration/runtime discovery and equivalent classifiers

- `mj-tui/src/availability.rs:294-310` separates local vs SSH runtime readiness. `mj-tui/src/dialogs/render.rs:966-976` target label. `mj-tui/src/wizards.rs:704-714,737-...` target labels and special EC2 selection. `mj-tui/src/wizards/render.rs:5-6,1425-1437` sizing and mount-storage explanation. `mj-tui/src/wizards/dashboard/targets.rs:292-329` target editor behavior. `mj-tui/src/dashboard_sessions.rs:1380-1385` groups managed container session targets. `mj-tui/src/dialogs/render.rs`, `mj-tui/src/wizards.rs`, `mj-tui/src/wizards/dashboard/targets.rs`, `mj-tui/src/wizards/render.rs`, and config setup editors are the UI sites changed by Apple precedent.
- Runtime discovery / setup / doctor selection: search `mj-controller/src/setup/machines.rs` (SSH target discovery and runtime inventory), `mj-controller/src/doctor.rs`, `mj-cli/src/ec2.rs:552-629` (EC2 registration and image preload), `mj-controller/src/project_catalog.rs:68-...` (bare-target project acceptance), and `mj-controller/src/controller/backend.rs`. There isn't a universal discovery enum: runtime readiness is currently a set of named runtime checks and target IDs.
- CLI, HTTP/API and client: config target creation flows through `mj-core::config::TargetTemplate`; HTTP/session responses serialize shared core state. The wire-compatibility gate is `mj-cli/src/main.rs:2301-2306` comparing daemon protocol versions, but the Apple commit did not touch protocol constants or client API types. New API request fields would be a different question; just adding a target enum member changes serialized data and DB kind, not command shape.

#### Apple-container precedent: commit 5a8619892

Diffstat: 58 files, +1,351/-124. It added the plan/docs/TUI, but the key production spread is `mj-core` config/state/conversion/command routing; controller provisioning, worker upload/start/upgrade, checkpoint, storage/cache/network/subagent, recovery/cleanup/preflight/doctor/setup/capacity/quota; DB schema and state IO; and CLI EC2 registration. Migration 98 explicitly widens the `session_targets.kind` CHECK and sets minimum compatible revision to 98 because older readers treat unknown target kinds as impossible (`schema.rs:1763-1804`). New variant support needs the analogous breaking migration; this precedent did not require a daemon protocol bump.

### 2. Persistence, migration, protocol

- `TargetTemplate` is config TOML with `#[serde(tag="kind", rename_all="snake_case")]` (`mj-core/src/config/targets.rs:517`); the runtime plan enum has the same tagged shape (`mj-core/src/targets.rs:2480-2504`).
- The durable locator is not one JSON blob in `sessions`: `session_targets` stores normalized columns, with discriminator `kind`; workspace storage is JSON in its own column. The database enforces kinds and shape (`mj-controller/src/database/schema.rs:1771-1794`) and state IO maps both directions (`state_io.rs:855-979,1062-1125`). Session runtime options are independent `target_runtime_json` (`state_io.rs:565-650`; schema introduced it separately).
- API/session serialization uses `mj-core::state::TargetLocator` (`state.rs:867-920`) embedded in `SessionRecord.target` (`:1620-1623`). Its kebab-case tag is a different serialization boundary from config's snake_case tag. `mj-client` consumes shared API DTO/state definitions; no duplicate enum was found.
- Under the repository's migration rule, a target-kind DB CHECK and decoder change is **breaking**: old daemons reject the new `kind`; widen table CHECK and shape constraints, set `minimum_compatible_version` to the new schema revision in the same transaction, and increment migration revision. Migration 98 did exactly this for `ssh-apple-container`. Do not rewrite it. Config TOML accepting a new kind alone is additive, but no session of that kind may be persisted before schema gate is raised.
- Apple commit `5a8619892` did not alter protocol version files (its changed-file list contains no protocol constant/client wire schema); it used breaking schema migration 98. Thus follow that precedent: schema migration required, protocol bump not implied for this target enum alone. Re-evaluate if the VM CLI introduces a new worker-daemon relay message or changes an existing wire representation; target-kind label/locator serialization is separate from relay protocol version.

### 3. Existing Podman-over-SSH command and output contract

The key is that current SSH Podman directly executes the container engine over SSH, while the proposed sudo-only `mj-worker vm` can only be a drop-in if it reproduces every relevant CLI contract—or code is changed to use its distinct contract.

- **Provision/create:** `mj-controller/src/targets/container.rs:312-386` builds Podman `run` with `--name`, labels, keepalive, resource flags, env, mounts and workspace storage; wraps script/argv through `ssh_command_owned` for SSH. `targets/provision.rs:184-206` adds git install and clone via container exec. The new VM command needs an equivalent create/run/create lifecycle and persistent guest filesystem.
- **Detached worker:** `mj-controller/src/controller/worker_binary/process.rs:295-319,344-357` creates a remote script that writes a per-attempt log and runs `hel worker run --root ... --config ...`; exact remote command is `ssh <opts> host podman exec --detach <container> sh -c <script>`. Exit status means accepted launch, worker readiness is separately probed.
- **Worker attach / relay commands:** `mj-core/src/targets.rs:2837-2871` routes arbitrary target argv. For SSH container it builds remote `podman exec -i <id> <argv>` and wraps through a remote POSIX drain shell. `:2902-2956` defines `exec -i`, with `CONTAINER_EXEC_DRAIN` preserving stdout, stderr, stdin and remote exit status even when local SSH output closes. The drain behavior is important for attached long-running commands; `:2964-2990` documents the 200 KiB regression fixture. A VM exec implementation must support interactive stdin when requested, detach when requested, and reliable attached streams/exit code; it must not have container exec's `--user` semantics assumed blindly.
- **File copy/staging:** `mj-controller/src/controller/worker_binary/install.rs:917-1017` uses SSH mkdir/scp to home-relative remote staging, then remote `podman cp <staged-file> <container>:<path>` and `podman exec` for mkdir/chown/chmod. Checkpoint staging is `mj-controller/src/controller/checkpoint/staging.rs:315-372`; container upgrade copies through engine `cp`. Contract: `cp HOST_PATH ID:/guest/path`, stdin/tar streaming only if separately implemented. Existing code parses no copy stdout on success; nonzero exit/stderr is failure. A VM `cp` must handle directories, permissions, large streams, and paths consistently, or controller must stage through SSH/SCP into an explicit guest agent channel.
- **Inspect:** recovery asks `podman container inspect ID`; stdout must be a JSON array of exactly one object. `recovery.rs:188-225` parses `/Config/Labels` (`dev.mj.managed=true`, `dev.mj.session=<id>`) and `/State/Status` (e.g. `running`, `exited`). Retained replacement parses a one-element JSON array into `container_upgrade.rs:46-116`: `Id`/`ID`, `Name`, `Image`, optional `ImageName`, `Config.Image`, `Config.Labels`, `Config.User`, `Config.Env`, `Config.WorkingDir`, `Config.Cmd`, `Config.Entrypoint`, `HostConfig.AutoRemove`, `Memory`, `NanoCpus`, `PidsLimit`, `NetworkMode`, `Privileged`, `ReadonlyRootfs`, `Devices`, `PortBindings`, `UsernsMode`, `SecurityOpt`, `CapAdd`, `CapDrop`, `Mounts` entries (`Type`, `Name`, `Source`, `Destination`, `RW`, `Options`, `Propagation`), and `State.Running`. Absence in this path is identified by known stderr substrings (`:157-180`). Also used as go-template output in cleanup: labels and `.State.Running` (`cleanup.rs:278-310`).
- **Stop / rm:** `mj-controller/src/targets/cleanup.rs:278-330` verifies exact session labels, `podman stop --time 0 --ignore ID`, then inspect `--format '{{.State.Running}}'`; expected stdout exactly `false`. `:432-451` runs ownership check and `podman rm --force --ignore ID`; then volume/helper cleanup. These scripts mostly suppress stdout and treat nonzero status as error. VM stop/rm must preserve ordering: stop/confirm guest before deleting durable guest storage.
- **Health/existence:** recovery uses `podman container exists ID` (`recovery.rs:93-101,120-127`): status 0 = exists, 1 = absent, other = engine/transport error (SSH preserves remote exit status); then inspect and perhaps `podman start ID`, followed by re-inspect status. Cleanup uses inspect/`podman info` to distinguish missing container from unavailable engine (`cleanup.rs:278-292`). Provisioning checks container-created command status; worker readiness is not inferred from detached exec result.
- **Image inspect/pull/prune:** `mj-core/src/targets.rs:2399-2455`: `podman image inspect --format '{{.Id}}' IMAGE`, stdout is image ID text; `podman pull [--platform=PLATFORM] IMAGE`, no parsed stdout; `podman image prune -f`, status only. `container_upgrade.rs:628-644` repeats inspect ID parsing for immutable image pinning. Startup image checks also use image inspect status (`doctor.rs:1590-1645`). Pulls can be long-running and use SSH without preflight's short-lived probe behavior.
- **Host preflight:** `mj-controller/src/targets/preflight.rs`: `podman --version` stdout version text; `podman info --format json` stdout JSON parsed for rootless state, serviceIsRemote and uidMappings (`:805-885,990+`). cgroup check runs `podman info --format '{{json .Host}}'`, stdout JSON `/cgroupVersion` and `/cgroupControllers` (`:937-987`). SSH probes batch commands and parse labeled records (test fixtures describe format at `targets/tests.rs:465-480`; implementation in preflight). Doctor additionally checks SSH limits/processes (`doctor.rs:2128-2474`). These semantics are not useful as a proxy for microVM host readiness.
- **Space/storage probes:** `mj-controller/src/target_storage.rs:264-297` associates remote `SshPodman` to the SSH host but maps writes inside guest/container paths to the host Podman graphroot; Apple paths are unattributed because VM disk is not measured. Controller storage board checks filesystem availability with host paths and helper paths (`target_storage.rs:303-338`); host-helper storage does status/create/destroy at `targets/container.rs` and `targets/cleanup.rs:484-500`. Workspace storage can be Podman named volume or a host helper, and temporary storage uses volumes. VM requires its own persistent disk/volume accounting and guest free-space probe.
- **Process limits:** `mj-controller/src/targets/process_limit.rs:62-106` identifies container locators, runs `sh -c` inside target via `command_on_locator`, reads `/sys/fs/cgroup/pids.current` and `pids.max`, parses two whitespace tokens; this is guest cgroup data in current containers, not host pid accounting. Container creation's `--pids-limit` is generated in `targets/container.rs` and surfaced by inspect.

### 4. Shared-kernel/host-filesystem assumptions to replace

A new executable name alone is insufficient. The existing path assumes a container process shares the host kernel and has engine-managed guest-like namespaces:

1. **Host bind mounts and workspace:** Podman `run` bind-mounts workspace, additional mounts, git/build cache, credentials and temp directories (`targets/container.rs:145-225,312-386`). `PodmanWorkspaceStorage::HostHelper` invokes a host helper and mounts its host path. A microVM cannot use arbitrary host paths as guest paths without a file-sharing device/protocol; translate each mount into a VM share or guest copy/sync scheme, with explicit read/write/COW behavior.
2. **Path ownership and worker roots:** host staging paths are passed to `podman cp`; worker root `/var/lib/hel/workers/<session>` is inside container. `worker_root` and storage mapping in `mj-core/src/targets.rs:2874-2891` assume container-root paths while SSH/control paths are directly reachable through engine exec. VM needs guest path mapping and a reliable host-to-guest copy/exec channel.
3. **Control socket and relay:** `command_on_locator` executes process inside container (`targets.rs:2837-2871`), worker sockets/journal live under guest root, and daemon attaches over current worker/relay scheme. MicroVM guest Unix sockets are not directly reachable from host; implement explicit vsock/SSH/forwarded relay and lifecycle reconnect semantics. Worker must remain alive through daemon restart and guest stop/restart behavior must be defined.
4. **Process tree, signals, PID/cgroup:** PID probes read guest `/sys/fs/cgroup` via `exec`; cgroup preflight probes host Podman cgroup v2 delegation (`preflight.rs:888-987`). Linux process groups/signals currently stop the container/worker; microVM's guest kernel/pid namespace and host VM process are distinct, so resource monitoring and teardown target both levels appropriately.
5. **`podman unshare`, volume and host-helper storage:** Podman rootless volume ownership/setup and host helper code use rootless user namespace and host mounted paths (`targets/container.rs:228-310,351-367`; `target_storage.rs:283-295`). VM disk image/guest filesystem needs separate ownership, persistence, space and cleanup semantics; don't report a VM disk as Podman storage path.
6. **Retained container upgrade:** replacement inspects/replays container host config, mounts, network, environment and resource settings (`container_upgrade.rs:209-310,620-900`). VM replacement has a different unit (image + root disk + guest config); decide whether disk is retained, cloned, snapshotted or immutable, and preserve worker journals/control state.
7. **Image cache and host controls:** `ImageHost` operates an engine image store and `image pull/prune`; VM image pull/inspect/prune likely manages host artifacts under sudo. Must avoid running sudo helper from event/render loops; current task orchestration already places work in controller/background plans. Define atomic naming/ownership so a `sudo -n` CLI cannot mutate foreign VMs.
8. **Runtime trust boundary:** current Podman target is rootless; VM command is planned under `sudo -n`, so host privileged operations become a separate security boundary. Validate every ID/path/label in the privileged CLI and ensure sudo non-interactive failures are reported clearly. The host CLI should be the sole owner of VM lifecycle state rather than making controller shell scripts reimplement it.


#### Additional production matches found in the Apple change / current source

These are easy to miss because they do not live under `targets/`; each makes a concrete placement or ownership decision and must be reviewed for the VM kind:

- `mj-controller/src/checkpoint_transfer.rs:212-268` routes checkpoint archive transfer to local engine, SSH host, or container copy; VM guest transfer needs explicit transport.
- `mj-controller/src/controller.rs:770-789` decides whether additional mount source paths are controller-local or must be inspected on SSH host.
- `mj-controller/src/controller/backend.rs:183-...` groups configured targets by host for reuse; `:1062-1120` creates locators; `:761-810` image refresh collection.
- `mj-controller/src/controller/cache_host.rs:80-122` says where harness/build cache lives and whether a target is bare/container/SSH. `mj-controller/src/controller/tool_cache.rs:52-71` chooses tool cache placement. `mj-controller/src/controller/mbx.rs:255-263` allows mbx only for Podman/Docker and explicitly excludes Apple; `controller/mbx/release.rs:487-...` deploys/release-copies caches by locator.
- `mj-controller/src/controller/move_session/transfer.rs:675-...` selects target-specific move/transfer plans (remote Bare, Podman, Docker, Apple have different handling). `controller/network_git.rs:81-...` routes Git network/cache path to container vs host. `controller/subagents.rs:694-...` inherits/constructs borrowed-target locators. `controller/recovery_scan.rs:474-...` identifies managed container targets during worker recovery.
- `mj-controller/src/controller/worker_binary/binary_select.rs:14-58` selects worker architecture from configured target platform; `worker_binary/harness.rs:13-...` decides harness runtime/install location; `worker_binary/launch.rs:970-981` handles worktree placement; `worker_binary/upgrade.rs:588-716` uploads/replaces binary; `targets/worker_daemon.rs:260-...` selects daemon attach endpoint.
- `mj-controller/src/controller/checkpoint/staging.rs:315-372` and `mj-controller/src/controller/target_storage` (the storage module is actually `mj-controller/src/target_storage.rs`) decide file staging/path locations. `mj-controller/src/controller/recovery_scan.rs` and `mj-controller/src/daemon/restart.rs` decide which targets are recovered and how session worker restart resumes.
- `mj-controller/src/setup/machines.rs` handles configured SSH machines/runtime setup and discovery; `mj-controller/src/doctor.rs` handles target runtime checks; `mj-cli/src/ec2.rs:552-629` creates existing EC2 SSH Podman and SSH Bare config and preloads image.
- Core secondary classifiers: `mj-core/src/targets.rs:1941-1980` creates target-specific project/workspace command plans; `mj-core/src/targets/ssh.rs:682-790` validates resource/session identity and borrowed ownership; `mj-core/src/targets/storage.rs:123-...` projects state locator into worker-root/workspace storage paths; `mj-core/src/state/target_runtime.rs:66-125` overlays captured SSH options and engine settings; `mj-core/src/state.rs:1193-...` exposes locator identity/resource/host.
- UI target picker/editor and render cases are `mj-tui/src/availability.rs:294-310`, `mj-tui/src/dialogs/render.rs:966-976`, `mj-tui/src/wizards.rs:704-714`, `mj-tui/src/wizards/dashboard/targets.rs:292-329`, `mj-tui/src/wizards/render.rs:5-6,1425-1437`, and `mj-tui/src/dashboard_sessions.rs:1380-1385`. API request values remain target IDs/config data; response locator comes from shared core state.

A practical exhaustive sweep during implementation should query production source (exclude tests) for `TargetTemplate::`, `TargetLocator::`, `ImageHost::`, `container_engine()`, `kind_name()`, `is_container_target`, and `is_container(` across `mj-core`, `mj-controller`, `mj-cli`, and `mj-tui`. Review even wildcard arms: some deliberately reject or treat all non-container targets alike.
