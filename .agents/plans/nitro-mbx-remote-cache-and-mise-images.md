# Nitro sandboxes: a shared read-only mbx build cache, and alignment with mise session images

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain it in accordance with `.agents/PLANS.md` (repository root).

Tracking issues: BrokkAi/mjolnir#1495 (remote build cache for Nitro). The image work follows BrokkAi/mjolnir#1493 (per-repository session images from mise; plan `.agents/plans/mise-session-images.md`) and BrokkAi/mjolnir#1494 (the Nitro runtime itself; plan `.agents/plans/nitro-sandbox-runtime.md`).

## Purpose / Big Picture

Mjolnir (`mj`) runs coding agents in sessions. A session can run in a Nitro Sandbox: one Firecracker microVM per session on an EC2 host. Firecracker is a minimal virtual machine monitor, and our guests have no way to mount a host directory. Two things are worse there than in a Podman container session today.

First, builds start cold. Podman sessions share the host's mbx build cache through a read-write bind mount. mbx ("mr-boxington", jdx/mr-boxington) is the compiler cache and build wrapper Mjolnir installs for Rust and C/C++ builds. A Nitro guest gets no mbx at all, and each session recompiles everything. After this change, a Nitro guest runs the same pinned mbx as other sessions. If the host has a remote cache configured, mbx in the guest also reads build outputs that trusted CI uploaded to an S3 bucket. It reads them through a small read-only proxy on the EC2 host. The proxy signs S3 requests with the host's instance role, keeps a verified copy of immutable objects on the host's disk, and refuses every write. A guest can read only the cache namespaces of the repositories in its own session.

Second, Nitro predates per-repository session images built from mise (#1493). It works with them, but has gaps:

- A wrong-architecture image is cached and only fails when the guest starts.
- New EC2 hosts warm only the default image, so the first session on a multi-gigabyte project image waits for the whole download and disk conversion.
- Every rebuilt project image adds a full flattened disk image on the host, and none is ever reclaimed.
- `mj image status` fails outright for a Nitro target with no builder.
- Pull errors for private images suggest a login command that does not exist.

After this change, these gaps are closed, and a live Nitro host has run both the default image and an assigned mise project image.

To see it working:

1. In an isolated instance, run `mj ec2 create --runtime nitro --mbx-remote s3://<bucket>/mbx`.
2. Seed the bucket with one CI-style mbx build of a fixture repository.
3. Start a Nitro session for that repository and run `cargo build` in it. The build reports remote cache hits and finishes far faster than the same build on a host without the remote.
4. Inside the guest, `curl -X PUT` to the proxy returns 405.
5. Inside the guest, a request with another repository's namespace returns 403.

## Progress

- [x] (2026-11-20 10:40Z) Claimed #1495 (assignee + `agent-in-progress`). Three read-only maps done; findings folded into Context and Surprises. Raw reports, not committed: `.mj/agents/86723bf780a4dd9d8b24056c7b6be9b8/findings.md` (mbx crates and protocol), `.mj/agents/f3f4746fa398e3dbff9133f5e48cee2e/findings.md` (engine, network, IAM, controller), `.mj/agents/8f2bc9dd138866074e59bf08f113d596/findings.md` (Nitro and mise images).
- [x] (2026-11-20) Milestone 1: engine image alignment (platform check, `maintain` verb with image reconciliation, pull credential remediation).
- [ ] Milestone 2: controller image alignment (warm effective images at host creation, reconcile images during refresh, `mj image status` without a builder, Doctor, docs).
- [x] (2026-11-20) Milestone 3: mbx in Nitro guests (pinned v1.30.0 musl release per guest architecture, staged through the worker binary store, launchers, guest-local store at /var/lib/hel/mbx). Ten focused filters passed one each; clippy clean on six crates. Live guest execution unverified until Milestone 6.
- [ ] Milestone 4: the host proxy (engine verb, per-VM unit, firewall, host cache, trimming, tests).
- [x] (2026-11-20 16:05Z) Milestone 5: implemented configuration, controller wiring, EC2 instance-role provisioning, the Doctor probe caller, and documentation in `nitro-mbx-wiring`.
- [x] (2026-11-20 16:08Z) Milestone 5: five focused test filters passed one test each, and all 11 touched Rust crates passed Clippy with all targets and warnings denied.
- [ ] Integrate Milestone 5 with the guest and engine changes, including the Milestone 4 `mbx-check` verb.
- [ ] Milestone 6: live EC2 validation and measurements, tool-cache decision.
- [ ] Integrated review, commit, push; update #1495; close it.

## Surprises & Discoveries

- Observation: the microVM engine has no long-running host service. Each `hel microvm <verb>` invocation is a short CLI process over root-only disk state in `/var/lib/mj-microvm`, and each VM runs as its own transient systemd unit. The proxy therefore needs its own supervised process.
  Evidence: `mj-worker/src/microvm/cli.rs:297-354,645-691`, `mj-worker/src/microvm/vm.rs:40-57,718-758`.
- Observation: guest source IP addresses cannot identify a guest. The nftables rules check no source address, and a guest can address packets to another VM's host gateway, which is a local destination that never crosses the TAP-to-TAP forward drop. What the guest cannot change is the TAP device its packets arrive on. A TCP listener bound with `SO_BINDTODEVICE` to one TAP receives only connections that arrived on that TAP. `IP_PKTINFO` cannot be used: it works only for datagrams.
  Evidence: `mj-worker/src/microvm/net.rs:115-151`; socket(7) and IP_PKTINFO man pages.
- Observation: `mj ec2 create` attaches no IAM instance profile and leaves the IMDS hop limit at the account default.
  Evidence: `mj-cli/src/ec2.rs:955-967`.
- Observation: Nitro guests get no mbx client at all. Shared mbx is resolved only for Podman and Docker. Nitro skips the shared launcher installation, and the old agent-dev image never contained mbx.
  Evidence: `mj-build-cache/src/mbx.rs:258-274`, `mj-worker-launch/src/worker_launch.rs:416-489`.
- Observation: the mbx crates `mbx-cache-core` 0.21.0 and `mbx-cache-protocol` 0.5.23 are on crates.io. Both are MIT, with MSRV 1.91. Core implements its own SigV4 signing over reqwest 0.13 (rustls), fetches and renews instance-role credentials over IMDSv2, and verifies blobs. Its API takes one namespace per client. It returns parsed action results rather than raw bytes, and manifests as bytes plus an unquoted ETag. It has no conditional GET and no typed S3 errors.
  Evidence: `mbx-cache-core/src/lib.rs:334-364,417-536`, `remote_s3.rs:278-348,468-654` at mr-boxington 4b48d6c.
- Observation: mbx rejects a `[remote]` table in a project's checked-in `.mbx.toml`. A namespace cannot be shared with CI through a repository file; it must come from `MBX_REMOTE_NAMESPACE` or the user-level mbx config.
  Evidence: `crates/mbx/src/config.rs:1609-1630,1723-1738`.
- Observation: Amazon S3 answers 403 rather than 404 for a missing key when the caller lacks `s3:ListBucket`. mbx treats most such 403s as misses for action results and manifests, and as errors for blobs.
  Evidence: `mbx-cache-core/src/remote_s3.rs:546-615,630-654`.
- Observation: the Nitro engine records an image's actual OS and architecture without comparing them with the requested platform. Crane's `--platform` applies only to multi-platform indexes, so a single-platform image for the wrong architecture is flattened, cached and reused, and fails only at the first exec in the guest.
  Evidence: `mj-worker/src/microvm/image.rs:54-80,109-165,298-320`.
- Observation: each distinct image config becomes a separate flattened ext4 base disk with no layer sharing. The engine's prune keeps every tagged reference, and nothing ever removes a tag. Every `mj image build` of a project therefore adds a disk of several gigabytes to each Nitro host that pulls it. Historically, the 8.3 GiB agent-dev image took 61 s to export and 35 s to convert.
  Evidence: `mj-worker/src/microvm/image.rs:132-178,228-286,397-447`, `mj-worker/src/microvm/cli.rs:98-113`, `.agents/docs/nitro-sandbox-spike.md:65-69`.

- Observation: the current configuration file stores build-cache settings on machines. A runtime's older fused build-cache settings are folded onto its machine during migration, and resolved container targets carry that machine policy.
  Evidence: `mj-core/src/config/machines.rs`, `resolve_target` and `fold_build_cache`.
- Observation: mbx's GitHub Actions writer checks for a protected branch push in addition to the CI environment. The seed example checks both protection and default-branch placement.
  Evidence: mr-boxington `crates/mbx/src/policy.rs`, `trusted_cache_writer`.

## Decision Log

- Decision: use one proxy process per VM, not one host-wide service. The engine starts it as a transient systemd unit, in `launch` right after the VM's TAP device is up and before the guest agent starts. The unit is bound to the VM's unit (`BindsTo=` plus `After=`), so it stops whenever the VM stops. The process binds one TCP socket to the VM's gateway address and port 7461, with `SO_BINDTODEVICE` set to that VM's TAP. All of its parameters come on its command line, rendered by the engine from the VM's state file.
  Rationale: the guest identity, the TAP, and the listener then share one lifetime owned by systemd and the engine. A host-wide service would need a registration protocol, recovery of bindings after its own restart, and revocation when a slot is reused. Each of those is a race to manage. With one process per VM, none of those races can be expressed. Upgrading the engine binary leaves running proxies on the old executable until their VM restarts, which is how running VMs already behave.
  Date/Author: 2026-11-20, parent session.
- Decision: the guest's identity is the TAP device the connection arrived on, enforced by `SO_BINDTODEVICE`. The firewall gets one generic accept rule, `iifname "mjt*" tcp dport 7461 accept`, placed before the TAP input drop.
  Rationale: the kernel delivers a connection only to a socket bound to the device it arrived on. A guest connecting to another VM's gateway therefore reaches no listener and gets a reset. The rule stays static, so the existing atomic table replacement on every launch is unchanged. Port 7461 is an arbitrary fixed choice. Only device-bound proxies listen on it.
  Date/Author: 2026-11-20, parent session.
- Decision: the S3 location is a field of the existing build-cache settings: `remote = "s3://<bucket>/<prefix>"` and an optional `remote_region`. Both are on `TargetBuildCache` in `mj-core/src/config/targets.rs`, which is used at both the machine level (`[machines.<id>.build_cache]`) and the target level (`build_cache` in a container target). They resolve through the existing machine/target merge. Only Nitro targets use it in this plan. Other targets keep their shared host store, and Doctor says so when `remote` is set on one of them. The region defaults to the host's own region, read from IMDS by the proxy.
  Rationale: the remote is read by the host's role on behalf of every session on that host, so it belongs with the host's existing mbx policy. A new top-level table would be a second place to look.
  Date/Author: 2026-11-20, parent session.
- Decision: a repository's namespace defaults to its GitHub `owner/repository`, written exactly as in the repository's `github` setting. That is the value CI gets from `${{ github.repository }}`. An optional `mbx_namespace` field on `ProjectRepository` overrides it. A repository with neither, for example a controller-local repository with no GitHub source, has no namespace. A session's allowlist is the set of namespaces of all repositories in its bundle. The primary repository's namespace becomes `MBX_REMOTE_NAMESPACE`. If the primary has none, the first repository that has one is used. With no namespaces at all, the session gets no remote and no proxy. Namespaces are validated with mbx's S3 grammar: slash-separated segments of ASCII `[A-Za-z0-9._-]`, no empty, `.` or `..` segments, and no leading or trailing slash.
  Rationale: this needs no configuration for GitHub repositories, and both sides read the same string from GitHub. `ProjectRepository` is not `deny_unknown_fields`, so an added optional field that is not serialized when unset leaves persisted bundle snapshots unchanged. An older daemon ignores the field and drops it on rewrite, which fails closed: the session gets no remote, rather than a wrong one.
  Date/Author: 2026-11-20, parent session.
- Decision: the host disk cache is keyed by bucket, prefix, namespace, kind, algorithm, hash and size. Blobs are verified against their digest before they enter the cache. Action results are cached after mbx-cache-core parses and validates them. Manifests are never cached: each request goes to S3. Proxies only add to the cache. The engine trims it, oldest access first, in the new `maintain` verb, which the daemon's hourly image refresh runs. The budget is the build-cache `max_total_size` when set, otherwise the same automatic budget mbx applies to its own store. If a write fails with `ENOSPC`, the proxy serves the object without caching it and logs the failure.
  Rationale: putting the namespace in the key means one namespace's cache never serves another, even for content-addressed blobs. A guest must not learn whether a digest exists in a namespace it cannot read. Trimming has one owner, the engine under its global lock, instead of every proxy racing to evict. The budget reuses the user's existing mbx size setting rather than inventing a new limit.
  Date/Author: 2026-11-20, parent session.
- Decision: the proxy answers `GET /v1/capabilities` with a valid protocol-1 capabilities document that enables no extensions and no compressors. It serves `GET` on `/v1/action-results/blake3/{hash}/{size}`, `/v1/action-manifests/blake3/{hash}/{size}` and `/v1/blobs/{blake3|sha256}/{hash}/{size}`. Every other method or path gets 405. A namespace outside the allowlist, or a missing or invalid one, gets 403. Confirmed absence in S3 gets 404. S3 authentication or authorization failures and transport errors get 502, with a short text body naming the cause. The mj-managed instance role includes `s3:ListBucket` on the bucket, so absence surfaces as a clean 404.
  Rationale: a valid capabilities document keeps `mbx doctor` working, where a 404 would fail its strict probe. Read-only clients never use the batch or pack extensions, because they fall back to single-object requests.
  Date/Author: 2026-11-20, parent session.
- Decision: `mj ec2 create --runtime nitro --mbx-remote s3://<bucket>/<prefix> [--mbx-remote-region <region>]` creates or reuses an IAM role and instance profile, and attaches the profile at launch. The role is named `mjolnir-nitro-mbx-<8 hex chars of SHA-256 of bucket/prefix>`. The role trusts EC2. Its read policy allows `s3:GetObject` on `arn:aws:s3:::<bucket>/<prefix>/*` and unconditioned `s3:ListBucket` on the bucket. The command also pins `HttpPutResponseHopLimit=1` and records `remote` (and the region, if given) in the new machine's `build_cache`.
  If the caller cannot create or pass the role, the host is still created without the profile. The output then states that the remote cache is off for this host and names the missing IAM permissions.
  On `--runtime podman`, the flag is ignored with a warning. Hosts that already exist are not modified: the docs give the `aws ec2 associate-iam-instance-profile` command and the config key.
  Rationale: Mjolnir already creates the security group, key pair and instance, so it can create the role too, and the user does not have to write IAM policy by hand. Creating the host anyway follows the AGENTS.md rule "never leave the user stuck".
  Date/Author: 2026-11-20, parent session.
- Decision: supersede the prefix-conditioned `s3:ListBucket` grant in the original role decision. Keep `s3:GetObject` restricted to cache objects and grant `s3:ListBucket` on the bucket without a condition.
  Rationale: S3 uses bucket-level list permission to choose 404 versus 403 for a missing object in GetObject. GetObject does not carry the ListObjects request's prefix condition. The read-only proxy exposes no bucket listing operation to the guest. Milestone 6 will verify that a miss using the generated role returns 404.
  Date/Author: 2026-11-20, parent session.
- Decision: the Doctor probe is a separate read-only engine verb with a single status line and exit status. It can use the default namespace from any configured bundle because bundle placement is unrestricted. Doctor skips the probe with "no repository namespace to check" when no bundle supplies one.
  Rationale: Doctor does not need to start a sandbox or change the host. The engine can use the same S3 path and instance credentials as the proxy.
  Date/Author: 2026-11-20, parent session.
- Decision: hash the normalized bucket and prefix and put the full SHA-256 in the IAM role description. Confirm it when reusing the deterministic role name.
  Rationale: the public name uses only eight hex characters. The description prevents a short-hash collision from overwriting the policy for another cache.
  Date/Author: 2026-11-20, Milestone 5.
- Decision: Nitro guests get Mjolnir's pinned mbx for the guest's platform, staged through the same controller→host→guest path as the session worker binary. They also get the same launchers, shims and environment as Podman sessions, except that the store is guest-local (`/var/lib/hel/mbx`, on the VM's retained root). The guest store uses mbx's automatic budget. Milestone 6 measures host disk use with concurrent sessions before anyone considers a limit.
  Rationale: one mbx installation path for all container sessions. AGENTS.md forbids speculative caps.
  Date/Author: 2026-11-20, parent session.
- Decision: image alignment covers the following:
  - platform validation before an image is cached or reused;
  - one declarative `hel microvm maintain --keep-image <ref>...` verb that untags references outside the host's wanted set and prunes;
  - warming every effective image of a new EC2 host, using the same planner as the hourly refresh;
  - `mj image status` showing assignments with freshness unknown when there is no builder;
  - Doctor reporting crane and the effective images;
  - host-side remediation text for pull authorization failures (`ssh <host> sudo crane auth login <registry> ...`).
  Out of scope:
  - Moving Nitro sessions across targets. It is blocked today for reasons unrelated to images (`mj-controller/src/controller/move_session/transfer.rs:915-935`).
  - A credential flow for private registries.
  - Live arm64 (Graviton) validation, because there is no native arm64 project image to test with.
  Rationale: these are the gaps the audit found that turn mise images into slow or failed Nitro sessions. Move and private-registry credentials are features of their own.
  Date/Author: 2026-11-20, parent session.
- Decision: whether to build the read-only harness tool cache that #1495 listed as "also in scope" depends on the Milestone 6 measurement of cold-start costs on Noble and on mj-dev. If staging the worker plus installing the bridge costs less than 30 s on a fresh guest, it is recorded as not worth building.
  Rationale: the issue asked for a measurement before building anything. The guest already reuses tools baked into a matching mise image.
  Date/Author: 2026-11-20, parent session.

## Outcomes & Retrospective

Milestone 1 adds validation of the fetched OCI architecture before publishing image state and validates cached image metadata before reuse. The engine's `maintain --keep-image` command reconciles tags and runs the existing unused-image prune; output lists removed references and reclaimed flattened-disk bytes. Crane digest resolution errors now point operators to host-side crane authorization.

The requested package identifier in the concrete command is outdated for this checkout: Cargo names the package `brokk-mj-worker-runtime`, though its library target is `mj_worker`. Use that package name for test and clippy invocations.

Milestone 5 resolves remote policy against the session's accepted bundle, using the primary namespace first and preserving case. Only engine arguments carry the policy. EC2 registration records the remote after AWS accepts the launch with an instance profile. Doctor can report readiness after the Milestone 4 engine is integrated. The isolated AWS and SSH fixture verifies the IAM policy, profile-propagation retry, launch JSON, and persisted settings. Milestone 6 owns live verification of role access and S3 misses.

## Context and Orientation

The daemon (`mj daemon-run`, crate `mj-controller`, with lifecycle code split into crates such as `mj-targets`, `mj-build-cache`, `mj-worker-staging`) is the control plane. It owns configuration (`~/.config/mjolnir/config.toml`, types in `mj-core/src/config/`), the session database and session lifecycle. Each session has a worker (crate `mj-worker`, binary `hel`), which runs inside the session's environment and starts the agent's harness.

A target says where sessions run. The target kind `SshNitro` (`mj-core/src/config/targets.rs` around line 571) runs one Firecracker microVM per session on an EC2 host reached over SSH. Mjolnir drives the host with the same `hel` binary in its "microVM engine" role. That binary lives at `~/.local/share/hel/microvm/hel` on the host and is invoked as `sudo -n .../hel microvm <verb>`. `ensure_microvm_engine` (`mj-worker-staging/src/worker_binary/microvm.rs:21-61`) installs and updates it. The engine code is in `mj-worker/src/microvm/`:

- `cli.rs` holds the verbs `run`, `start`, `stop`, `rm`, `exec`, `cp`, `image ...`, and their argument structs.
- `vm.rs` creates and launches VMs. `run` reserves a slot in `/var/lib/mj-microvm/vms/<name>/state.json`, creates the disks, then calls `launch`. `launch` applies the firewall, creates the TAP device, starts the VM's transient systemd unit, and sends the guest agent its start request, which includes the VM's environment.
- `state.rs` holds `VmState`, the per-VM durable state. Writes are atomic, and the reader refuses future format versions.
- `net.rs`: `Link::for_slot` gives each slot a TAP device `mjt<slot>` and a /30 subnet in `10.200.0.0/16`, with the host gateway at subnet+1 and the guest at subnet+2. `ruleset()` renders the nftables tables that `apply_host_policy()` installs on every launch.
- `image.rs` pulls images with `crane`, flattens them into an ext4 base disk, and caches them by config digest. `replace_root.rs` is the retained-root image upgrade.

A TAP device is the host side of the VM's virtual network card. The guest reaches the internet through NAT ("masquerade" in the rules). It cannot reach the instance metadata service (IMDS, `169.254.169.254`), other guests, or any host port, because the input chain ends with `iifname "mjt*" drop`. EC2 hosts for Nitro come from `mj ec2 create --runtime nitro` (`mj-cli/src/ec2.rs`): it launches an Amazon Linux 2023 instance with `aws ec2 run-instances`, installs Firecracker, the jailer and crane through cloud-init, and registers a machine and target in the config.

The controller side of Nitro is `mj-targets/src/targets/nitro.rs`. There, `nitro_run_args` (line 185) turns the target template and session into engine `run` arguments. During session provisioning, `mj-controller/src/controller/provisioning.rs` calls `drop_nitro_host_directories` (around lines 1777-1804). It removes host mounts and emits two notices: one for attached directories, and one reading "Nitro sandboxes cannot share the host's mbx build cache, so builds in this session start cold." Shared mbx setup is in `mj-build-cache/src/mbx.rs`. `supported_host` (lines 258-274) admits only Podman and Docker. `prepare` (lines 1250-1329) mounts the shared store. `mj-worker-staging/src/worker_binary/launch.rs:246-274` injects `MBX_CACHE_DIR`, `MJ_MBX_CONFIG_DIR` and `MBX_SHIMS_DIR`, and `mj-worker-launch/src/worker_launch.rs:416-489` installs the shared mbx binary and its launchers. `mj-build-cache/src/tool_cache.rs:37-59` skips Nitro.

mbx can use a remote cache. With `MBX_REMOTE_URL=s3://bucket/prefix`, it reads and writes S3 objects at `<prefix>/<namespace>/v1/{blobs|action-results|action-manifests}/<algo>/<hash>/<size>`. With `MBX_REMOTE_URL=http://...`, it speaks mbx's HTTP cache protocol to a server. Every request carries the headers `mbx-cache-protocol: 1` and `mbx-cache-namespace: <namespace>`. `MBX_REMOTE_MODE=read-only` makes mbx read and prefetch but never write. The objects are:

- A "blob" is file content addressed by its BLAKE3 or SHA-256 digest and size. It is immutable and can be verified.
- An "action result" maps a build step's input digest to its outputs. It is immutable once written, but not verifiable: whoever can write the bucket can make readers accept arbitrary outputs. That is why only trusted CI may write the bucket.
- A "manifest" is a mutable prediction list used for prefetch. A manifest response must carry a strong, quoted ETag.

Plain HTTP to a non-loopback address with no token is allowed. mbx logs one warning per build: "using an unauthenticated remote build cache over plain HTTP; cache traffic can be read or modified in transit".

Session images (#1493). `Config::session_image` (`mj-core/src/config.rs:1809-1847`) resolves a session's image. It uses the bundle's assignment for the target's platform (`[images.bundles.<bundle>] "linux/amd64" = "<ref>"`), otherwise the target's `image`, otherwise the default `buildpack-deps:noble`. `mj image build` (`mj-controller/src/session_images.rs`) builds a project image from the repository's `mise.toml`. For a Nitro target, it builds on another container-engine target of the same platform and pushes the image to a registry, and Nitro pulls it from there. The daemon's image refresh poller (`mj-daemon/src/pollers/quota.rs:628-667,890-933`, planner in `mj-targets/src/targets.rs:103-149`) warms each target's base image and every bundle image for the target's platform, about once an hour. `mj ec2 create` preloads only the default image (`mj-cli/src/ec2.rs:1086-1175`).

## Plan of Work

The work is six milestones. Milestones 1, 3 and 4 touch disjoint code and run in parallel, each in its own detached worktree. Milestones 2 and 5 depend on the engine interfaces fixed below. They can start in parallel against those interfaces and merge after the engine work lands. Milestone 6 is the live check.

### Milestone 1: engine image alignment (`mj-worker/src/microvm/image.rs`, `cli.rs`)

After fetching an image's OCI config, compare its `os` and `architecture` (and `variant`, when the requested platform names one) with the requested platform, which defaults to the host's native platform. If they differ, refuse before the image is flattened or cached, with the error "image <ref> is <os>/<arch>, but this Nitro host runs <os>/<arch>; assign an image built for <platform>". Apply the same check on the cached fast path, so an image cached by an older engine is not reused across architectures. Store the checked platform with the cached image.

Add a verb `hel microvm maintain --keep-image <ref> [--keep-image <ref> ...]`. Under the engine's global lock it does two things. It removes every image reference (tag) that is not in the keep set. Then it runs the existing prune, which keeps any base disk used by a VM or by a staged root replacement. It prints one line per removed reference and the bytes reclaimed. Running it repeatedly with the same keep set changes nothing. Milestone 4 adds a trimming option to this verb.

When crane fails because registry authorization was denied, the engine error names the remedy on that host: "run `sudo crane auth login <registry> -u <user> -p <token>` on the Nitro host; Mjolnir pulls as root". Keep crane's own message in the error. The matching `SshNitro` branch of `engine_error` in `mj-controller/src/session_images.rs`, which suggests a nonexistent `microvm login`, is fixed in Milestone 2.

Tests: platform mismatch is a security- and fidelity-relevant tricky condition. Build a single-platform OCI layout for the other architecture with a fixture (a tiny image written by hand in an OCI layout directory, read through crane's `oci-layout` support or a local registry, whichever the existing image tests use). Check that pull refuses it and leaves nothing in the cache. For `maintain`, use a fixture state directory with three references, one of them used by a VM, and keep one: the unused, unkept reference is removed and the in-use disk survives.

### Milestone 2: controller image alignment

`mj ec2 create` replaces its preload of only the default image with the shared refresh planner for the new target. That warms the base image and every bundle image assigned for the host's platform, with visible per-image progress. The registration and the release of the EC2 operation lock stay where they are. A failure warns and leaves the host registered, as today.

For each Nitro host, after the refresh poller has warmed all of the host's wanted images, it runs `hel microvm maintain` with the keep set. The keep set is the union, over all Nitro targets on that host, of each target's base image and every bundle image for its platform. Run `maintain` only after every pull for that host succeeded, so that a failed pull never makes the keep set smaller than the host actually needs.

`mj image status` for a target with no usable builder lists the recorded assignments with freshness "unknown (no builder for <platform>)". It no longer fails. Fix the `SshNitro` branch of `engine_error` to give Milestone 1's remediation text.

Doctor (`mj-setup/src/doctor.rs:3015-3134,3432-3489`) on a Nitro target:

- reports crane's path and version from the engine's host report;
- checks, for each effective image (target base plus bundle assignments for the native platform), that the image is present on the host;
- reports "will download on first use" for images that are not yet present.

Docs: `docs/src/content/docs/project-environments.md` and `mj-core/assets/skills/mj/references/configuration.md`, in the Nitro and registry sections, cover:

- Nitro images must be built for the host's architecture;
- Nitro pulls as root with crane on the host, with the login command;
- the guest runs as root;
- new hosts warm all assigned images;
- obsolete images are reclaimed.

Fix the stale comment in `mj-worker-staging/src/worker_binary/launch.rs:391-397` about non-root Nitro.

Tests: extend the existing Doctor golden (`mj-setup/src/doctor/tests.rs`) with a Nitro target that has one present and one missing bundle image. Extend the existing EC2 fixture test (`mj-cli/src/ec2.rs`, around 2614-2672) so its expected SSH commands include the bundle image pull.

### Milestone 3: mbx in Nitro guests

`supported_host` admits `SshNitro` with guest-local placement. Provisioning stages Mjolnir's pinned mbx binary for the guest platform into the guest. It uses the same artifact source and staging path that put the session worker at `/var/lib/hel/workers/<session>` (see `mj-worker-staging/src/worker_binary/install.rs:967-1053`). The worker launch installs the same launchers and shims as for Podman, with `MBX_CACHE_DIR=/var/lib/hel/mbx` inside the guest. The guest's `MBX_REMOTE_*` variables, when present, must reach mbx unchanged through the worker's environment overlay and harness launch. Provisioning notices belong to Milestone 5, so this milestone does not change the notice text in `provisioning.rs`.

Tests: none of the "which tests to add" kinds fits a unit check here. Proof comes in Milestone 6, where `which cargo` in a Nitro guest resolves to the mbx shim and `mbx --version` prints the pinned version. If the staging path has an existing fixture test for the worker binary, extend that test's expectations to include the mbx artifact rather than adding a new test.

### Milestone 4: the host proxy (`mj-worker/src/microvm/`, new module `mbx_proxy.rs`)

Add the dependencies `mbx-cache-core = "0.21"` and `mbx-cache-protocol = "0.5"` from crates.io to `mj-worker`, using rustls. Use the same versions for both, so the digest types are one crate identity.

`run` accepts `--mbx-remote s3://<bucket>/<prefix>`, `--mbx-remote-region <region>` (optional) and `--mbx-namespace <ns>` (repeatable; the first is the default). Validate each namespace with the grammar in the Decision Log. Store them in `VmState` as a separate typed field, `mbx_remote: Option<MbxRemote>`, with serde defaults so older state reads as `None`. They are not part of the VM's environment list. When `mbx_remote` is set:

- `launch` adds `MBX_REMOTE_URL=http://<gateway>:7461`, `MBX_REMOTE_NAMESPACE=<default>` and `MBX_REMOTE_MODE=read-only` to the guest environment it sends in the start request. These values replace any same-named entries.
- `launch` starts the proxy unit after `tap_up` and before the guest agent's start request.
- The proxy's command line carries everything it needs: device, gateway address, port, bucket, prefix, region, namespaces and cache directory. It never reads VM state.
- `launch` waits until the proxy is listening. Use systemd `Type=notify` with `sd_notify` from the proxy, or poll the socket the way the engine already waits for the guest agent.
- If the proxy cannot start, the launch fails with the proxy's error, and the existing launch-failure teardown runs.
- `start` (restart of a stopped VM) and the retained-root upgrade both go through `launch`, so they restart the proxy with the persisted parameters.

The proxy runs under a dedicated system user that the engine creates idempotently, `mj-mbx-proxy`. It needs only `CAP_NET_RAW` (for `SO_BINDTODEVICE`) and read-write access to the cache directory `/var/lib/mj-microvm/mbx-cache`. Apply the systemd sandboxing options `NoNewPrivileges=yes`, `ProtectSystem=strict`, `ReadWritePaths=<cache dir>`, `PrivateTmp=yes` and `ProtectHome=yes`. If live testing shows the kernel allows unprivileged `SO_BINDTODEVICE` on an unbound socket, drop the capability and record that in Surprises.

In `ruleset()`, insert `iifname "mjt*" tcp dport 7461 accept` in the IPv4 input chain before the final drop. IPv6 stays fully dropped.

Request handling follows the Decision Log:

- One `RemoteCacheClient::new_s3_with_instance_role` per allowed namespace, created lazily, all sharing one `InstanceRoleCredentials`.
- The region is `--mbx-remote-region`, or else the host's region from IMDS.
- Blobs: `get_blob_file` into a temporary file in the cache directory, then rename into place. That function verifies the digest before returning, and the response is served from the renamed file.
- Action results: `get_action_result`, then serialize to JSON with the protocol media type and cache it.
- Manifests: `get_action_manifest` on every request, returning 200 with `ETag: "<value>"`.
- A cache hit updates the file's access time for trimming.
- Map upstream failures as the Decision Log says. Prefer distinguishing S3 absence from authorization failure by S3's error code. If core does not expose it, map core's documented `Ok(None)` to 404 and all errors to 502.

`maintain` gains `--mbx-cache-budget <size>`. It removes cache files, least recently accessed first, until the cache fits the budget. It deletes only files, never directories that another proxy is writing into. A proxy that is streaming a deleted file keeps its open descriptor.

Tests, all required:

- Security boundary, network: a privileged Linux test, gated like other root-only tests in the repository and skipped without root. It creates two veth or TAP links in a network namespace, each with its own /30, starts two proxies bound to their devices, and checks three things:
  - a client in link A's namespace reaches A's proxy;
  - connecting to B's gateway from link A is refused;
  - a client that spoofs B's source address on link A still reaches only A's proxy.
- Security boundary, protocol: against a fake S3 HTTP server written by hand in the test, as a small hyper or axum handler that understands the key layout and ignores signatures, check that:
  - PUT and POST get 405;
  - a namespace outside the allowlist gets 403;
  - a blob whose bytes do not match its digest is neither cached nor served;
  - an S3 AccessDenied becomes 502.
  Point the core client at the fake through its custom endpoint support.
- Integration with the real client: run the real `mbx` binary (the pinned one the repository already downloads for tests, if any; otherwise a fixture build step documented in the test) with `MBX_REMOTE_URL=http://127.0.0.1:<port>` and `MBX_REMOTE_MODE=read-only`. Run it against a proxy over the fake S3, which has been seeded by the same `mbx` running with `MBX_REMOTE_URL=s3://...` and the fake's endpoint. The second build must report remote hits.
- Drive at least one blob larger than 64 KiB through every path.

### Milestone 5: configuration, controller wiring and the EC2 instance role

Configuration (`mj-core/src/config/targets.rs`):

- Add `remote: Option<String>` and `remote_region: Option<String>` to `TargetBuildCache` and its `BuildCacheSettings` deserializer. Validate that `remote` starts with `s3://` and names a bucket.
- Add `mbx_namespace: Option<String>` to `ProjectRepository`, with `skip_serializing_if = "Option::is_none"`.
- Add a resolver that returns, for a bundle, the default namespace and the allowlist described in the Decision Log.
- Add config tests only if an existing config round-trip golden covers `TargetBuildCache` or `ProjectRepository`; in that case extend it.

Provisioning (`mj-controller/src/controller/provisioning.rs` and `mj-targets/src/targets/nitro.rs`): for a Nitro session whose resolved build cache is enabled and has a `remote`, and whose bundle yields at least one namespace, pass `--mbx-remote`, `--mbx-remote-region` and the `--mbx-namespace` list through `nitro_run_args` as a typed input, not as environment entries. Resolve this before the notices are produced. The notices become:

- with a remote: no build-cache notice;
- with build cache enabled and no remote or no namespace: "This Nitro sandbox keeps its mbx build cache inside the VM, so its first build starts cold. Set `build_cache.remote` on the host to share CI's cache (see Project environments)."
- with build cache disabled: no notice.

The attached-directory notice is unchanged.

EC2 (`mj-cli/src/ec2.rs`): add `--mbx-remote` and `--mbx-remote-region` to `CreateArgs`, and implement the role, the instance profile, the hop limit and the config recording described in the Decision Log, using the `aws` CLI as the file already does:

- `aws iam get-role` / `create-role`;
- `put-role-policy`;
- `get-instance-profile` / `create-instance-profile`;
- `add-role-to-instance-profile`;
- `IamInstanceProfile` in the run-instances JSON.

A newly created instance profile can take seconds to become usable in `run-instances`. Handle the `InvalidParameterValue` "Invalid IAM Instance Profile" refusal by retrying with backoff until IAM reports the profile, because the other side reports this limit. Do not add a fixed sleep.

Doctor: for a Nitro target with `remote`, invoke the short-lived `hel microvm mbx-check` read probe using the interface below. Report "remote cache readable", "role cannot read <bucket>/<prefix>: <S3 error>", or "no instance role attached".

Docs: a "Shared build cache on Nitro" section in `docs/src/content/docs/project-environments.md` covering the config keys, the namespace default, the trust model (only trusted CI may write the bucket), the plain-HTTP warning, and a GitHub Actions job snippet:

    env:
      MBX_REMOTE_URL: s3://<bucket>/<prefix>
      MBX_REMOTE_NAMESPACE: ${{ github.repository }}

The snippet runs only on pushes to the default branch, with a role whose trust policy pins `ref` and `event_name`. Mirror the essentials in the skill reference.

Tests: extend the EC2 fixture test with `--mbx-remote` and assert the IAM calls, the `IamInstanceProfile` and the hop limit in the launch JSON. Extend `mj-controller/src/controller/backend/tests.rs` Nitro run-args coverage (around 305-380) so the namespace list reaches `nitro_run_args` and never appears in the environment.

### Milestone 6: live EC2 validation and measurements

The parent runs this, because children cannot run the mj CLI against a daemon. Use an isolated instance launched with `env -i`, as described in the Nitro plan, with the Bedrock profile and a fixture bundle. Create a test bucket `mj-nitro-mbx-test-<random>` in the host's region and a Nitro host with `--mbx-remote s3://<bucket>/mbx`. Seed the bucket from this machine with a CI-style build of a medium Rust repository (for example `BurntSushi/ripgrep` at a fixed tag), using `MBX_REMOTE_URL=s3://<bucket>/mbx`, `MBX_REMOTE_NAMESPACE=BurntSushi/ripgrep`, and the user's AWS credentials. Then:

1. Start a Nitro session for that repository on the default Noble image. Record the cold-start phases: worker staging, git bootstrap, bridge install, and mbx staging.
2. In the session, run `cargo build --release` and record the wall time and the remote hits mbx reports.
3. Compare with the same build in a session on a second host, or with `build_cache.remote` unset.
4. From the guest:
   - `curl -X PUT http://<gateway>:7461/v1/blobs/blake3/<64 hex>/1` → 405;
   - a GET with `mbx-cache-namespace: other/repo` → 403;
   - a connection to another running VM's gateway on port 7461 → refused;
   - an absent object in the permitted namespace → 404 using the generated role.
5. Start a second session on the same host and confirm that the host cache serves it. Check the proxy log, or that the S3 request count falls.
6. Start a session for the mjolnir bundle on its assigned `mj-dev` image. Record the image's cold pull and conversion time, and the cold-start phases.
7. Run `mj doctor` and `mj image status` for the Nitro target.
8. Restart the host engine binary by updating it, then stop and start a VM, and confirm its proxy comes back.

Write the measurements into Surprises, and decide on the tool cache per the Decision Log. Afterwards, delete the bucket, the host (suspend its sessions first), the IAM role and the instance profile, and stop the isolated daemon.

## Concrete Steps

Implementation happens in detached worktrees under `~/Projects/mjolnir-wt/<name>`, created with `git worktree add --detach ~/Projects/mjolnir-wt/<name> origin/master`. Each worktree builds and tests only its own crates. Use the focused commands, run outside any sandbox:

    cd ~/Projects/mjolnir-wt/<name>
    cargo test -p mj-worker microvm
    cargo clippy -p mj-worker --all-targets -- -D warnings

Run the privileged network test with the documented root gate, for example `sudo -E cargo test -p mj-worker mbx_proxy_device_binding -- --ignored`, or however the repository already gates root tests. Report the per-filter test counts, because a filter that matches nothing still reports `ok`.

The parent merges the worktrees onto master in milestone order, runs the boundary suites for the touched crates, requests review, and pushes.

## Validation and Acceptance

The acceptance criteria are the observable results of Milestone 6:

- a warm Nitro build reports remote cache hits and is substantially faster than the cold build;
- writes return 405;
- a foreign namespace returns 403;
- another VM's proxy is unreachable;
- a second session on the same host is served from the host cache;
- `mj doctor` reports the remote readable and crane present;
- `mj image status` lists the Nitro target's assignments;
- a wrong-architecture image is refused at pull with the platform message (proven by the Milestone 1 test);
- a new host's first session on mj-dev does not pay the image conversion, because `mj ec2 create` warmed it.

The automated tests named in each milestone pass. Clippy is clean on every touched crate.

## Idempotence and Recovery

`maintain` is declarative and safe to repeat. A proxy that dies stops serving only its own VM. Stopping and starting the VM restarts it. Cache files are written to a temporary name and renamed, so an interrupted write leaves only a temporary file, which `maintain` removes when it is older than an hour. The IAM role and profile names are deterministic, so a repeated `mj ec2 create` reuses them. Live test resources are listed in Milestone 6 together with their cleanup.

## Artifacts and Notes

Milestone 5 validation ran in the dev profile through mbx. Each focused filter matched one passing test: `nitro_create_uses_enclave_ami_and_verified_bootstrap_before_registration` in `brokk-mjolnir`, `a_nitro_launch_keeps_referenced_environment_values_off_the_command_line` in `brokk-mj-controller`, `config_toml_round_trip_is_atomic` in `brokk-mj-core`, `nitro` in `brokk-mj-targets`, and `golden_doctor_reports_external_aws_ssh_docker_and_podman_observations` in `brokk-mj-setup`. The EC2 test uses AWS CLI and SSH fixture executables. The GitHub Actions workflow example parses as YAML.

## Interfaces and Dependencies

Engine CLI, the contract between Milestones 4/1 and 5/2:

    hel microvm run ... [--mbx-remote s3://<bucket>/<prefix> [--mbx-remote-region <region>] --mbx-namespace <ns> [--mbx-namespace <ns> ...]]
    hel microvm maintain --keep-image <ref> [--keep-image <ref> ...] [--mbx-cache-budget <mbx size string>]
    hel microvm mbx-proxy --device <tap> --listen <gateway-ip>:7461 --bucket <b> [--prefix <p>] [--region <r>] --namespace <ns>... --cache-dir <dir>
        (internal; started only by `launch`)
    hel microvm mbx-check --bucket <b> [--prefix <p>] [--region <r>] --namespace <ns>

Milestone 4 implements `mbx-check`. It prints exactly one line on stdout:
`remote cache readable` (exit 0), `no instance role attached` (nonzero), or
`role cannot read <bucket>/<prefix>: <S3 error>` (nonzero).

In `mj-worker/src/microvm/state.rs`:

    #[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
    pub struct MbxRemote {
        pub bucket: String,
        pub prefix: String,          // may be empty
        pub region: Option<String>,
        pub namespaces: Vec<String>, // non-empty; first is the default
    }
    // VmState gains: #[serde(default, skip_serializing_if = "Option::is_none")] pub mbx_remote: Option<MbxRemote>

In `mj-core/src/config/targets.rs`:

    pub struct TargetBuildCache { ..., pub remote: Option<String>, pub remote_region: Option<String> }
    pub struct ProjectRepository { ..., pub mbx_namespace: Option<String> }
    impl ProjectBundle { pub fn mbx_namespaces(&self) -> Result<Vec<String>> } // default first; empty when none

Dependencies: `mbx-cache-core = "0.21"` and `mbx-cache-protocol = "0.5"` (crates.io, MIT), only in `mj-worker`.

Revision note (2026-11-20): initial plan, written after the three reconnaissance maps.

Revision note (2026-11-20, Milestone 5): documented the Doctor engine verb and the bucket-level list permission required for S3 misses.
