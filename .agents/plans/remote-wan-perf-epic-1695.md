# Remote EC2 / WAN Latency, Transfer Efficiency, and Code Simplification (Epic #1695)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain this document in accordance with `.agents/PLANS.md`.


## Purpose / Big Picture

When Mjolnir runs agent sessions on remote EC2 hosts over a residential internet connection (20 to 40 ms round-trip latency and 7 to 40 Mbps uplink) rather than a fast local area network, inefficiencies that were invisible on a LAN dominate user wait times. Warm remote container session launches take 26 to 29 seconds across 50 to 79 sequential remote command invocations (8 to 14 seconds of which is pure round-trip and wrapper overhead), cold worker binary uploads take 55 to 87 seconds uncompressed, warm profile staging copies 2,214 files repeatedly for 6 to 13 seconds even when zero new objects are sent, remote suspend downloads a checkpoint archive and immediately re-uploads the exact same bytes back to the same EC2 host, cross-host container moves double-hop through the local daemon, standing up a new EC2 host takes nearly 5 minutes, and active-turn relay sync issues four sequential stop-and-wait RPCs every 150 ms.

After this plan is complete, every step is grounded in before-and-after measurements on an isolated test instance against a real EC2 Podman host, and every change makes the codebase simpler and less fragile rather than layering new caches, timers, or wire-protocol versions. Background SSH noise stops first (prewarm host-keying, `CONTROL_PERSIST=600`, `SshBare` capacity-target stability, and `mbx_service` failed-host backoff) so every measurement is clean; a single gzip stream option in the SSH execution layer compresses worker uploads, single-file fetches, `canonical_session` stdin, and worker pre-staging; existing batching helpers (`step_script` and `ProbeFrames`) replace scattered sequential SSH round-trips; suspend promotes the already-staged remote checkpoint archive into the host snapshot cache instead of re-uploading it; container worker installs collapse into one host script and delete the leaking `static LAST` map; `image_pull_gate` becomes a writer-preferring shared/exclusive gate so concurrent container creates run in parallel; direct EC2-to-EC2 container moves wire the existing `Ec2GrantManager` from CLI EC2 records plus AWS `describe-instances`; same-host sub-agent `fetch` moves zero WAN bytes; `mj ec2 create` associates the Elastic IP once the instance is `running`, reuses `ssh_command`, and pulls images concurrently; and relay read-only RPCs pipeline over the existing persistent connection via `call_many` with zero protocol version bump.


## Progress

- [x] (2026-11-24 16:42Z) Fetched Epic #1695 and all 10 child issues (#1685 through #1694), verified none are assigned, and self-assigned `#1695` with label `agent-in-progress`.
- [x] (2026-11-24 16:49Z) Completed five parallel read-only reconnaissance passes covering all 10 child issues.
- [x] (2026-11-24 16:56Z) Authored initial ExecPlan in `.agents/plans/remote-wan-perf-epic-1695.md` and sent it to Advisor session `91233b0efdd193066e7b4eeb46c8db10` for pre-implementation review.
- [x] (2026-11-24 17:15Z) Received Advisor review and revised ExecPlan to incorporate all five global rules, all per-issue rulings, all additional cuts (Protocol 91, resume helper, `FICLONE`, `supported()` cache, prewarm timer, `podman info` memo, borrowed-child shim skip and parent-facts reuse, `MASTER_CHECK_INTERVAL`, `MJMV0002`, IMDS discovery, config-absence retirement timer, remote `curl` path, restore fallback, and `static LAST`), and one-issue-per-push ordering.
- [x] (2026-11-24 17:31Z) Baseline measurement completed at unmodified `HEAD` (`3ce12540a`) on `ec2-m8id16-d`, recorded in this plan, and sent to Advisor `91233b0efdd193066e7b4eeb46c8db10` (with `#1688` profile-install split).
- [x] (2026-11-24 17:35Z) Incorporated Advisor's post-baseline revisions: cut `#1688` `tar | exec tar` pipe, delete `cp -pRP` everywhere (use 0.07 s Perl copy for `EXDEV` fallback too), leave `supported()` receiver probe untouched (162 ms), added top-priority read-only timing split of `prepare exact managed harness before worker startup` (`11.9 s` warm, 35% of launch!), added closure of measurement gaps (compressible real-plugin-size cold profile upload, realistic-archive suspend, borrowed-child launch), and reordered pushes after Push 1 to `#1685` -> `#1688` -> harness-prep report -> `#1694` -> `#1686` -> `#1689` slices by measured gain (>0.3 s).
- [x] (2026-11-24 17:58Z) Diagnostic split & measurement gap closure completed (`diag-harness-prep-split`): identified `prepare exact managed harness` (`11.9–21.0 s` cold Node + `npm ci` 512 MB into ephemeral container overlay on every new container vs `0.37–0.43 s` warm), measured borrowed-child launch (`14.79 s`, 21 SSH calls), measured real-size compressible plugin cold upload (`12.89 s` / `10.93 MB` sent), filed `#1696` under `#1695`, and received Advisor ruling recommending a host-local derived image layer (`podman commit`) over a writable host mount (put to user before coding `#1696`).
- [x] (2026-11-24 18:06Z) Push 1 (`#1690`) reviewed and pushed to `origin/master` (`b290732b1`): `CONTROL_PERSIST=600`, `CommandSpec::ssh_bulk_variant` + `bulk_relay_command` routing skills/attachments/live-baselines onto Bulk relay with `RelayTransportDead` isolation and best-effort recovery caching, and `store.rs` link/publish scripts moved to Control.
- [ ] Push 2 (`#1685` — in progress): Rule 3 shared SSH gzip stdin/stdout option + compressed `mj-worker` upload + single container worker-store script + delete `static LAST`. Measure, review, push.
- [ ] Push 3 (`#1688` — prewarm & profile staging): host+fingerprint prewarm deduplication (success-only, no timer, clear on launch upload) + empty-manifest short-circuit; leave `supported()` probe untouched; `StoreGate` serialization of negotiated transfers + cross-process busy backoff; drop double-gzip (`private.tar`); replace `cp -pRP` everywhere with O(1) `rename` + Perl copy `EXDEV` fallback (~2.5 s off warm launch, zero ZFS 2.4.1 `cp` exposure); fold `launch.json`/`ownership.json` into the existing container install script (no tar pipe); split remaining `send negotiated profile content` time after rename. Measure, review, push.
- [ ] Push 4 (`#1694` — stop background churn): stabilize `SshBare` `DeploymentCapacityTarget` storage paths and add per-host failed-host backoff in `mj-daemon/src/daemon/mbx_service.rs`. Measure, review, push.
- [ ] Push 5 (`#1686` — Suspend host-cache promotion): retain host staging with a durable owner surviving daemon death, verify digest via existing portable SHA helper (`upgrade.rs:710-730`), publish through a temp file inside the cache directory and atomically rename into `host_snapshot_cache` (acceptance: 0 archive bytes uploaded during suspend, including one realistically sized archive run). Measure, review, push.
- [ ] Push 6 (`#1689` — Launch critical path slices ordered by measured gain, dropping slices <~0.3 s): `ProbeFrames` single-script `mbx::inspect_host` + in-place single-flight `RESOLUTIONS` entry; typed same-launch `already_synced` proof; `ProbeFrames` batching for launch preflight (`uname`, `$HOME`, `podman info`, `tool_cache` dirs + multi-path `probe_filesystem_types`, `git_cache` `last-gc`) and borrowed-child probes; writer-preferring shared/exclusive `image_pull_gate` with miss loop-back; deferrable/admitted detached `git_cache` GC; combined Podman run/home-init/inspect; `step_script` per-repo parallel `network_git`; concurrent owned-child lanes; `SshBare` duplicate validation removal and batching. Measure per slice, review, push.
- [ ] Push 7 (`#1687` — Direct EC2 container Move & same-host `fetch`): enable Podman and Nitro in `direct_ssh_endpoint` (proven with real EC2-to-EC2 moves); wire `Ec2GrantManager` from CLI EC2 records + AWS `describe-instances` (never IMDS!) with per-grant region/profile; skip duplicate `cleanup_interrupted_move_credentials` when `DirectTransferCleanup` succeeds; same-host MCP `fetch` via single host SSH script with private temp dir + `EXIT HUP INT TERM` trap + full-disk visible failure; cross-host single-file `fetch` using Rule 3 SSH gzip. Measure, review, push.
- [ ] Push 8 (`#1692` — Remote Suspend batching & leftovers): per-decision-point `ProbeFrames` snapshots consumed by each decision in `lifecycle/origin_delta.rs`; bounded remote wait in `wait_for_stop`; `restore-checkpoint --spec -` (no fallback, since restore always installs fresh build) and Rule 3 gzip on `export-checkpoint`/`restore-checkpoint` stdin; `step_script` Phase-2 container teardown; destroy-event hook at `ec2.rs:1763, 1849-1870` retiring `host_snapshot_cache` rows + concurrent `leftovers.rs` sweep with per-host backoff. Measure, review, push.
- [ ] Push 9 (`#1693` — `mj ec2 create` speedup): wait for `running` -> associate EIP -> single SSH wait; delete raw `Command::new("ssh")` helper and reuse `mj_core::targets::ssh_command`; hoist plain invariant AWS values before placement loop (keeping keypair/SG attempt-owned); collapse `mbx/install.rs` SSH round-trips (keeping controller upload unless upload measures > a few seconds); single Nitro engine ensure + concurrent distinct-image pulls + skip `podman image prune` when `current_image_id` is `None` + concurrent Rule 3 compressed `mj-worker` host pre-stage. Measure, review, push.
- [ ] Push 10 (`#1691` — Relay read pipelining, zero protocol bump): add `call_many` in `mj-session-runtime/src/worker_client/exchange.rs` for independent read-only RPCs (tick reads, credential state reads, attachment presence checks) with single batch deadline, handler-error-as-connection-failure, never pipelining writes/installs, verifying older worker loop tolerance; conditional no-op `Acknowledge` skip preserving periodic `Acknowledge` for worker GC retry (`requests.rs:484`); `WaitForChange`/backoff in `readiness.rs`. Measure, review, push.


## Surprises & Discoveries

- Observation: Two general-purpose remote batching helpers already exist in the workspace and must be reused instead of inventing custom framings.
  Evidence: `mj-core/src/targets.rs:2740-2890` provides `step_script` for named mutating steps with per-step failure reporting, and `mj-targets/src/targets/preflight.rs:1530-1688` provides `ProbeFrames` / `parse_framed_probes` for named reads returning stdout, stderr, and exit status.

- Observation: IMDSv2 discovery from a remote host for `Ec2GrantManager` is a security flaw (`#1687`).
  Evidence: Advisor review noted that every IMDS value is host-reported; feeding host-reported security-group IDs into `aws ec2 authorize-security-group-ingress` would allow a compromised host to open port 22 on any security group reachable by the daemon's AWS credentials. Authoritative EC2 identity (`instance_id`, `region`, `aws_profile`, `account`) already lives in the local CLI record (`mj-cli/src/ec2.rs:168-182`), which should be read by a shared helper and resolved via AWS `describe-instances`. `Ec2GrantManager` was simply left unwired by commit `d64ca35d9` during the origin-delta Move rewrite.

- Observation: Relay read pipelining (`call_many` in `exchange.rs`) achieves the `#1691` RTT reduction with zero protocol bump, making Protocol 91 unnecessary.
  Evidence: `mj-session-runtime/src/worker_client/exchange.rs:119-186` and `mj-worker/src/worker_runtime/unix.rs:3470, 4067` run newline-delimited JSON over one persistent connection where the worker serves requests serially. Writing independent read-only RPCs together and reading their ordered replies drops active ticks from 4 RTTs to 2 (and idle ticks to 1 with conditional `Acknowledge` skipping), and drops 4 to 6 credential-state reads or N attachment checks to 1 RTT. A compound `SyncPoll` cannot acknowledge events it just returned, so it saves nothing beyond pipelining. Note also `mj-worker/src/worker_runtime/unix/requests.rs:484`: skipping `Acknowledge` forever would also skip worker GC retry, so keep a periodic `Acknowledge`.

- Observation: `install.rs:999-1018` already batches container `engine cp`, `chown`, `chmod`, and staging cleanup into one SSH script (`#1688`).
  Evidence: What remains in `#1688` is the two host-side receiver copies (`File::Copy::copy` and `cp -pRP`), `engine cp` itself, and the separate `launch.json`/`ownership.json` uploads. Furthermore, piping `tar` into `<engine> exec -i` assumes `tar --no-same-owner` exists inside the container image, which is not an invariant today; we must time receiver materialization vs `engine cp` vs `chown`/`chmod` vs `tar | exec tar` on a real EC2 host before deciding whether `tar` earns its keep, and never make `tar` a hard launch requirement.

- Observation: Skipping `install_build_cache_shim` for borrowed sub-agents (`#1689`) would break borrowed children, and memoizing `podman info` cgroups would miss host reconfigurations.
  Evidence: `mj-worker-launch/src/worker_launch.rs:460-553` writes per-worker-root `worker_root/bin` cargo and `mbx` shim scripts (not just host-level sync), and `provisioning.rs:1960-1987` uses `podman info` cgroup controllers to decide CPU/memory flags. Batching probes with `ProbeFrames` and passing the same-launch `already_synced` proof keeps correctness intact without fragile caches.

- Observation: In `#1685`, deleting `static LAST` is cleaner than re-keying it, and checking the container store inside the single host script eliminates any "fresh container" caller flag.
  Evidence: `static LAST` (`store.rs:299-314`) existed only to avoid extra SSH round-trips for collection; once the container-store check, host-store copy/publish, and collection share one host script, `static LAST` becomes unnecessary and deleting it plugs the per-container key leak completely.

- Observation: In `#1686`, Suspend staging promotion cannot assume the host staging directory and snapshot cache share a filesystem, and an in-memory RAII guard alone cannot clean up a retained staging file if the daemon crashes.
  Evidence: Publishing must copy/stage through a temp file inside the cache directory before atomic `rename`, use the existing portable SHA helper (`mj-worker-staging/src/worker_binary/upgrade.rs:710-730`), and assign a durable owner so daemon crash recovery cleans up any abandoned staging file. Meanwhile, Resume's pack loader (`host_cache.rs:727-744`) is an optional cache warm whose packs are small by design and where a host worker binary matching the host OS/arch is not guaranteed for image installs; it stays cut unless baseline pack bytes prove material.

- Observation: In `#1692`, one `SuspendDirSnapshot` across the entire suspend flow is incorrect, and `restore-checkpoint --spec -` does not need a fallback.
  Evidence: `lifecycle.rs:1184-1387` has ten distinct decision points where reads after upgrade, delivery readback, polling, post-stop, and post-start must be fresh; batching must scope strictly to a single decision point whose value is consumed by that decision. On Resume (`resume.rs:1183-1244`), restore always runs the freshly installed worker binary (and older workers would interpret `-` as a literal filename rather than an unsupported flag), so no fallback is needed for `restore-checkpoint --spec -`; export may run an older session worker, so gzip compression for export stays at the SSH transport layer (Rule 3).

- Observation: In `#1694`, `mbx_service.rs` does not call `Freshness::Fresh` on no-op revisions; fresh inspection runs only on actual application (`mbx.rs:334`), and the observed churn comes from retrying failed hosts on every session-set change (`mbx_service.rs:71-79, 217-240`).
  Evidence: Adding per-host failed-host backoff directly fixes the churn without switching `mbx_service.rs` away from fresh application checks. Availability probes are request-driven by the picker with no measured storm and are cut/deferred.

- Observation: `podman image prune -f` (`mj-core/src/targets.rs:3541-3546`) is host-wide and not excluded by a per-image lock (`#1689`).
  Evidence: The soundness invariant for a shared/exclusive per-(host, image) gate is specifically that `prune -f` removes only untagged images, and a tag cannot move while a container `run` holds the shared lock (while `pull` holds the exclusive lock). The in-process lock must be writer-preferring and acquired before the cross-process `File::try_lock` polling loop so `pull` cannot starve under continuous `run` traffic, and upgrades must release shared before acquiring exclusive and re-checking image presence.


## Decision Log

- Decision: Enforce five cross-cutting engineering rules across all ten pushes: (1) measure baseline and every push on an isolated instance against one real EC2 host, cutting anything with no measured share; (2) use only existing `step_script` and `ProbeFrames` for SSH batching; (3) add one shared gzip-wrapped stdin/stdout option in the SSH execute layer (no ad-hoc `gzip -d |` wrappers, no SSH `Compression`); (4) add no new cross-launch caches or timers without measured evidence; (5) ship one issue per push, pulling background-noise stoppers (`#1690`, `#1694`, `#1688`) to the front so all later timings are clean.
  Rationale: Directly adopted from Advisor review (`91233b0e`, 2026-11-24) to maximize simplicity, minimize review churn, and ensure every change produces measured WAN improvement.
  Date/Author: 2026-11-24 / Coordinator + Advisor (`91233b0e`).

- Decision: Cut 15 mechanisms/additions beyond the initial 5 cuts: Protocol 91 (use `call_many` pipelining in `exchange.rs`), `#1686` Resume host-worker pack helper (unless baseline proves pack bytes material), `FICLONE` in `profile_receiver.pl`, `supported()` host cache (delete probe via read-site exit 2/127 classification), prewarm 6-hour timer (clear key on launch upload instead), `podman info` host memo, borrowed-subagent shim skip and parent-facts reuse, `MASTER_CHECK_INTERVAL` change, `MJMV0002` move-stream format, IMDSv2 discovery (use CLI EC2 records + AWS `describe-instances`), config-absence leftover retirement timer (use destroy hook + concurrent sweep/backoff), remote `curl` path in `mbx/install.rs` (unless upload measures > a few seconds), `restore-checkpoint --spec -` legacy fallback, `#1694` availability TTL and `mbx_service` memoization switch, and `static LAST` (delete instead of re-key).
  Rationale: Each cut removes unnecessary state, dual code paths, security risks, or unmeasured complexity while preserving or improving correctness and WAN performance.
  Date/Author: 2026-11-24 / Coordinator + Advisor (`91233b0e`).

- Decision: Keep `mj-cli/src/ec2.rs` hoisted AWS metadata lookups as plain values computed before the placement retry loop (no cache struct), and enable `#1687` direct container Move only for `SshPodman` and `SshNitro` (the engines `mj ec2 create` builds, proven with real EC2-to-EC2 moves; `SshDocker` only if tested for real; `SshAppleContainer` stays disabled).
  Rationale: Avoids speculative caching and untested engine paths.
  Date/Author: 2026-11-24 / Coordinator + Advisor (`91233b0e`).

- Decision: Measure the `#1688` profile-install timing split (receiver materialization, `engine cp`, `chown`/`chmod`, and `tar | exec tar`) and cold un-prewarmed reviewer upload on a real EC2 host before implementing Push 3. If cold un-prewarmed reviewer upload is only a few seconds, report that finding to the user to ask whether to delete prewarm altogether (never delete their feature unasked).
  Rationale: Ensures we do not add `tar` fallback complexity unless it measurably beats `engine cp` on EC2, and respects user ownership of the prewarm feature.
  Date/Author: 2026-11-24 / Coordinator + Advisor (`91233b0e`).


## Outcomes & Retrospective

Not yet implemented. Baseline and per-push measurements will be appended here as each push completes.


## Baseline & Post-Push Measurements

All measurements use one isolated instance (`env -i` with dedicated `MJ_INSTANCE`, `MJ_CONFIG_DIR`, `MJ_DATA_DIR`, `XDG_CACHE_HOME`, and copied/synthetic profile home — never touching the default instance or real credential homes), with `RUST_LOG` setting `mj_worker_launch::launch_timing=debug` and `mj_core::targets=debug`, a quiet daemon, one existing EC2 Podman host, and fixed image, repository, and profile. Each run records wall time, per-phase `elapsed_ms` from `session launch phase ended` (`mj-worker-launch/src/launch_timing.rs:14-36`, never summing nested phases), and counts of `program=ssh` commands grouped by purpose (`mj-core/src/targets.rs:1376-1390`), labeled cold or warm.

    Baseline (commit 3ce12540a, 2026-11-24 UTC, host: ec2-m8id16-d / ubuntu@3.210.102.187, image: ghcr.io/brokkai/mjolnir/mj-dev:30fdcc1fce93c76b, repo: octocat/Hello-World, synthetic Claude profile: 2,214 files / 35,001,126 bytes; warm SSH true RTT: 100.5–106.5 ms; musl worker: 74,225,976 bytes):
      - Cold launch:             136.497 s create lifecycle | 96 SSH completions (plan+provision 6.141 s, materialize 1.413 s, prepare worker files 113.704 s, prepare harness 12.429 s, init workspace 1.177 s, start+await 2.990 s)
      - Cold worker upload:      68.783 s (`stage replacement Mjolnir worker`, 74.2 MB raw)
      - Cold un-prewarmed prof:  35.793 s (`send negotiated profile content`, send_ms=30.564 s, 35.48 MB / 2,214 files, 0 reused)
      - Warm launch:             33.726 s create lifecycle | 61 SSH completions (plan+provision 4.033 s, materialize 0.802 s, prepare worker files 13.340 s [negotiated profile 5.120 s w/ 0 sent + install remote container worker 3.272 s + worker store ~3.0 s], prepare harness 11.875 s, init workspace 1.243 s, start+await 3.184 s)
      - Borrowed-child launch:   Not measured (no prompt-free borrowed spawn API without scheduling an LLM turn)
      - Suspend:                 ~15.7 s total (CLI accept 2.304 s) | 54 SSH completions (delete-then-reupload verified; 6× suspend-operation.json = 1.258 s, 2× container stop = 1.012 s)
      - Resume (+ pack bytes):   ~36.4 s to running+idle | 63 SSH completions | archive 1,897 bytes, OriginDelta pack bytes = 0 (clean origin lower bound; reused verified host snapshot cache)
      - Same-host 5 MB fetch:    7.529 s wall (read 0.699 s, store 4.849 s over WAN uplink) | 6 SSH completions
      - Profile split (#1688, first / warm1 / warm2 on 2,214 files / 35.0 MB):
          Perl File::Copy::copy+chmod: 0.071 s / 0.070 s / 0.072 s
          cp -pRP:                     2.506 s / 2.540 s / 2.543 s
          rename (same FS):            0.000050 s / 0.000045 s / 0.000045 s
          podman cp:                   1.273 s / 1.323 s / 1.425 s
          chown + chmod:               0.249 s / 0.244 s / 0.358 s
          tar | podman exec tar+perms: 0.782 s / 0.907 s / 0.810 s (GNU tar --no-same-owner verified)
      - Reusable driver: /tmp/mj-epic1695-bench/bench.py (mirrored to $MJ_KEEP_DIR/bench/), raw logs in .mj/agents/e3d73c53fdf7edc97f99b255f7eee781/baseline.md


## Context and Orientation

Mjolnir is a Rust workspace whose local control plane (`mj-daemon` over `mj-controller` and `mj-session-runtime`) manages agent sessions running in worker processes (`mj-worker`, packaged as `mj-worker-bin`) locally or on remote hosts over OpenSSH. A target locator (`mj_core::targets::TargetLocator`) identifies local bare checkouts, remote bare hosts (`SshBare`, `AwsEc2`), or remote containers/microVMs (`SshPodman`, `SshDocker`, `SshAppleContainer`, `SshNitro`).

Remote SSH command execution and multiplexing live in `mj-core/src/targets/ssh.rs` and `mj-core/src/targets.rs`. Traffic splits into `SshTrafficClass::Control` and `SshTrafficClass::Bulk` ControlMaster shards under a per-instance socket directory. Two shared remote-batching primitives already exist and govern all batching work in this plan: `step_script` (`mj-core/src/targets.rs:2740-2890`) runs ordered mutating steps with per-step failure attribution, and `ProbeFrames` / `parse_framed_probes` (`mj-targets/src/targets/preflight.rs:1530-1688`) runs read-only probes returning framed stdout, stderr, and status per named probe.

Remote session launch flows through `mj-controller/src/controller/{provisioning.rs,backend.rs,network_git.rs,worktree.rs,subagents.rs}`, `mj-targets/src/{image_pull_gate.rs,targets/{preflight.rs,container.rs,bootstrap.rs,recovery.rs}}`, `mj-build-cache/src/{mbx.rs,mbx/install.rs,tool_cache.rs,git_cache.rs,cache_host.rs}`, `mj-worker-staging/src/worker_binary/{store.rs,install.rs,upgrade.rs,profile_transfer.rs,profile_receiver.pl}`, and `mj-worker-launch/src/{worker_launch.rs,launch_timing.rs}`. Background profile prewarming, host capacity polling, and `mbx` service reconciliation live in `mj-daemon/src/daemon/{prewarm.rs,capacity.rs,mbx_service.rs}` and `mj-daemon/src/pollers/{capacity.rs,quota.rs}`.

Active sessions communicate with workers over a persistent newline-delimited JSON relay connection (`mj-core/src/relay.rs`, `mj-core/src/relay/protocol.rs`, `mj-session-runtime/src/worker_client/{exchange.rs,relay.rs,credential_sync.rs}`, `mj-session-runtime/src/session_manager/{actor.rs,standalone.rs,readiness.rs}`, and worker handlers in `mj-worker/src/worker_runtime/unix.rs` and `unix/requests.rs`).

Suspend, resume, and move live in `mj-controller/src/controller/lifecycle.rs`, `mj-controller/src/controller/lifecycle/origin_delta.rs`, `mj-controller/src/controller/checkpoint/{latched.rs,host_cache.rs,staging.rs,leftovers.rs}`, `mj-session-runtime/src/checkpoint_transfer.rs`, `mj-controller/src/controller/{resume.rs,move_session.rs}`, `mj-controller/src/controller/move_session/transport.rs`, `mj-controller/src/controller/move_session/transport/ec2_grant.rs`, and `mj-worker/src/checkpoint.rs`. Sub-agent `fetch` flows through `mj-daemon/src/server_runtime/{api.rs,api/fetch_transfer.rs}` and `mj-checkpoint/src/archive/fetch.rs`. Managed EC2 creation and destruction live in `mj-cli/src/ec2.rs`.


## Plan of Work

Work proceeds strictly one issue per push after capturing the real EC2 baseline and profile-install timing split. Each push is locally validated with targeted tests and Clippy, committed on `master`, reviewed via `request_review`, and pushed to `origin/master`, followed by a post-push measurement update in this plan.

### Step 0: Baseline Measurement and Profile-Install Split

Before changing production code, run an isolated test instance (`env -i`, fresh `MJ_INSTANCE`/`MJ_CONFIG_DIR`/`MJ_DATA_DIR`, empty or synthetic profile home, zero access to live profiles or default daemon) targeting one existing EC2 Podman host with `mj_worker_launch::launch_timing=debug,mj_core::targets=debug`. Measure and record warm launch, borrowed-child launch, cold worker upload, suspend, resume (including OriginDelta pack byte size), same-host fetch, cold un-prewarmed reviewer profile upload, and the `#1688` profile-install timing split on that EC2 host (receiver materialization, `engine cp`, `chown`/`chmod`, and a controlled comparison of `engine cp` vs `tar | podman exec tar` for the same tree into the same destination). Stop the isolated daemon cleanly, record all measurements in this ExecPlan, and send the baseline numbers and timing split to Advisor `91233b0efdd193066e7b4eeb46c8db10`.

### Push 1 (`#1690`): SSH ControlPersist & Bulk Relay Lane Hygiene

In `mj-core/src/targets/ssh.rs`, raise `CONTROL_PERSIST` from `"60"` to `"600"` with an inline comment citing the 14.5-hour live EC2 audit (151 Bulk ControlMaster reopens/host with 230–277 s median inter-burst gaps). Leave `MASTER_CHECK_INTERVAL`, `SessionLedger`, `MJOLNIR-SSH-SESSION-STARTED`, and `GIT_SSH_COMMAND` untouched. In `mj-targets/src/targets/recovery.rs` and `mj-session-runtime/src/worker_client/relay.rs`, generalize the existing Bulk relay proxy helper (`relay_proxy_plan` / `project_memory_relay_command`) so `InstallSkills`, `InstallAttachment`, `ReadAttachment`, and `ReadLiveBaselinePage` run on `SshTrafficClass::Bulk` while keeping per-worker skills verification and installation intact. Reclassify small metadata-only SSH commands in `mj-worker-staging/src/worker_binary/store.rs:464` from `Bulk` to `Control` (leaving `network_git.rs` for Push 6's rewrite).

### Push 2 (`#1685`): Shared SSH Gzip Stream Helper & Container Worker Store Simplification

First implement Rule 3 in `mj-core/src/targets.rs` and `mj-core/src/targets/ssh.rs`: add one shared gzip-wrapped stdin/stdout transport option in the SSH execution layer (using `flate2::Compression::fast()` on the controller side and `gzip -d` / `gzip -c` on the remote shell side, never enabling OpenSSH `Compression`). Use this shared helper for SSH `mj-worker` binary uploads in `mj-worker-staging/src/worker_binary/store.rs` (cutting cold 74.2 MB upload by ~47 s).

Then simplify container worker installation in `mj-worker-staging/src/worker_binary/store.rs` and `upgrade.rs`: replace the multi-call miss/verify/collect/pin/cp/publish/link sequence with one host script (using `step_script` where steps mutate) that inspects the container store, links immediately on a hit, and otherwise copies from the host store, verifies SHA-256 once on upload/publication, publishes and links into the container, and folds store collection into that same script (skipping collection when the store is empty while still collecting long-lived containers; cuts ~2.4 s off warm launch). Do not thread a "fresh container" flag from callers, and delete `static LAST` (`store.rs:299-314`) completely.

### Push 3 (`#1688`): Profile Prewarm Cleanup & Profile Staging Copy Reduction

In `mj-daemon/src/daemon/prewarm.rs`, change coverage keying from per-session to host-scoped `(host_key, profile_content_fingerprint)` recorded only on success, with no expiration timer; clear the host's coverage key whenever a real session launch for that covered key actually has to upload objects (`objects_sent > 0`). In `mj-worker-staging/src/worker_binary/profile_transfer.rs`, short-circuit immediately before opening SSH when `destination` is `None` and `manifest.objects.is_empty()`. Leave the `supported()` SSH probe untouched (162 ms once per launch does not justify touching error classification). Serialize concurrent controller-side negotiated transfers to the same host through the existing per-host `StoreGate`, and use bounded exponential backoff (250 ms to 2 s) only for remaining cross-process `busy` collisions.

In `mj-worker-staging/src/worker_binary/profile_transfer.rs`, `profile_receiver.pl`, and `install.rs`, remove double-gzip by sending uncompressed `private.tar` inside the already-gzipped outer negotiation archive. In `profile_receiver.pl`, keep plain Perl byte copies (0.07 s for 35 MB, zero OpenZFS 2.4.1 exposure; no `FICLONE`), replace `cp -pRP` with O(1) `rename($tree, $destination)` (0.000045 s vs 2.54 s!), and delete `cp -pRP` everywhere by using the same Perl recursive copy helper for the `EXDEV` fallback as well. Fold `launch.json` and `ownership.json` into the existing batched container install script (`install.rs:999-1018`) rather than two separate SSH uploads. Do NOT add a `tar | exec tar` pipe (measured 0.8 s gain does not justify a second transport and container-image `tar` requirement). After landing, split any remaining time inside `send negotiated profile content` rather than guessing.

### Push 4 (`#1694`): Stop `SshBare` Capacity Churn & `mbx_service` Failed-Host Retries

In `mj-controller/src/controller/backend.rs` and `mj-daemon/src/daemon/capacity.rs`, normalize and stabilize `SshBare` storage mount paths in `DeploymentCapacityTarget` so starting or stopping bare sessions under the same mount root does not alter target equality and trigger immediate 1-second capacity re-probes. In `mj-daemon/src/daemon/mbx_service.rs`, add per-host exponential backoff when applying/inspecting a host fails so unrelated session-set changes do not hammer failing hosts (retaining `Freshness::Fresh` on actual application). Do not add an availability TTL cache.

### Push 5 (`#1686`): Suspend Host-Cache Promotion Without Re-Upload

In `mj-session-runtime/src/checkpoint_transfer.rs`, `mj-controller/src/controller/checkpoint/latched.rs`, and `mj-controller/src/controller/checkpoint/host_cache.rs`, retain the remote host staging archive through controller SHA-256 verification on `HoldThroughClose` (Suspend) under a durable ownership record (so daemon crash recovery sweeps any abandoned host staging file rather than relying solely on an in-memory RAII guard). Once verified, promote the retained host archive into `host_snapshot_cache` in one remote step: verify its SHA-256 using the existing portable SHA helper (`mj-worker-staging/src/worker_binary/upgrade.rs:710-730`), copy or stage into a temporary file inside the cache directory (never assuming staging and cache share a filesystem), and atomically `rename` to the final cache path, then remove the staging file and update the durable record. Reuse the already-known remote `$HOME` instead of running `resolve_ssh_home`. Keep the Resume pack helper cut unless Step 0 baseline pack bytes prove material.

### Push 6 (`#1689`): Remote Launch Critical Path Collapse

In `mj-build-cache/src/mbx.rs`, collapse `inspect_host()`'s 10 to 12 sequential SSH probes into one `ProbeFrames` invocation (`mj-targets/src/targets/preflight.rs:1530-1688`), keeping all parsing and decision logic in Rust. Make the `RESOLUTIONS` entry itself the single-flight in-flight owner (no second map) so concurrent cold launches share one inspection. Pass a typed same-launch `already_synced` proof from `mbx::prepare()` to `mj-worker-launch/src/worker_launch.rs`'s `install_build_cache_shim` so it skips repeating `probe_native_version` and `sync_mbx_binary_from_native` while still writing per-worker-root `worker_root/bin` cargo and `mbx` shims. For borrowed sub-agents (`mj-controller/src/controller/subagents.rs`), keep `install_build_cache_shim` (for per-worker-root shims) and batch the child's probes with `ProbeFrames` (no parent-facts cache).

In `mj-controller/src/controller/{provisioning.rs,backend.rs}`, `mj-targets/src/targets/preflight.rs`, and `mj-build-cache/src/{tool_cache.rs,git_cache.rs,cache_host.rs}`, batch launch host preflight (`uname -sm`, `$HOME`, `podman info` arch + cgroup controllers fetched fresh every launch, `tool_cache` directory creation + multi-path `probe_filesystem_types`, and `git_cache` `last-gc` check) into one `ProbeFrames` call (using `ssh_probe_session`'s `BatchMode=yes` and `StrictHostKeyChecking=yes` so it replaces the separate `true` probe). Move overdue `git_cache` 24-hour GC (`podman ps --all` plus `CACHE_GC_SCRIPT`) off the synchronous launch path into a detached daemon-owned background task registered as deferrable (`activity_unless_draining`) with its own handoff admission label per `AGENTS.md`.

In `mj-targets/src/image_pull_gate.rs` and `mj-controller/src/controller/provisioning.rs`, upgrade the per-(host, image) gate to a writer-preferring in-process shared/exclusive lock acquired before cancellable cross-process `File::try_lock_shared` / `File::try_lock` polling. Container creation takes shared, checks `podman image exists`, and runs `podman run --pull=never`; on a miss (either before run or if run reports image absent), it releases shared, takes exclusive, re-checks, pulls once, releases exclusive, re-acquires shared, and loops back to run. Document the exact safety argument: `podman image prune -f` is host-wide and removes only untagged images, and a tag cannot move while a run holds shared.

In `mj-targets/src/targets/{container.rs,bootstrap.rs}` and `mj-controller/src/controller/worker_launch.rs`, collapse `PODMAN_VOLUME_RUN_SCRIPT`, single-exec `initialize_container_home`, and `inspect_container_storage` into one remote script (dropping redundant `podman info` and post-create `volume inspect` calls). In `mj-controller/src/controller/network_git.rs`, replace sequential per-command `podman exec git` calls with `step_script` (one named step per existing diagnostic, preserving stdin `GH_TOKEN` delivery and existing finalization batch) and run bundle repositories concurrently. Parallelize owned-child Lane A, `prepare_subagent_report_dir`, and Lane B in `provisioning.rs:323-328`. In `mj-controller/src/controller/worktree.rs`, carry the same-launch `SshBare` project classification from selection into launch (eliminating the duplicate validation pass) and batch reads with `ProbeFrames` and mutations with `step_script`.

### Push 7 (`#1687`): Direct EC2 Container Move & Same-Host MCP Fetch

Add a small shared EC2 record reader for the existing local CLI records (`mj-cli/src/ec2.rs:168-182`, containing `instance_id`, `region`, `aws_profile`, `account`) used by both Move and Destroy. In `mj-controller/src/controller/move_session.rs` and `transport.rs`, enable `SshPodman` and `SshNitro` in `direct_ssh_endpoint` (keeping `SshAppleContainer` disabled; enabling `SshDocker` only if verified against a real Docker host), resolving the host worker binary cleanly and verifying Nitro forced-command quoting. Look up authoritative security groups, VPC ID, and private IPv4 from AWS `describe-instances` under the record's profile and region (never IMDS!), using private IPv4 when both peers share a VPC and leaving hosts without a local EC2 record with `None` grants (direct only if already reachable, else visible daemon relay). Extend `Ec2GrantKey` / `Ec2GrantManager` with per-grant `region` and `aws_profile`. Report cleanup completion out of `DirectTransferCleanup` so `move_session.rs:4507-4523` skips redundant `cleanup_interrupted_move_credentials` SSH calls on clean transfers while retaining durable retry on failure. Prove both Podman and Nitro direct moves between two real EC2 hosts (keeping all automated tests on a fake AWS client and warning reviewers never to hit real AWS).

In `mj-daemon/src/server_runtime/{api.rs,api/fetch_transfer.rs}`, detect same-host source and destination sessions (normalized `SshTarget`) and run one host-side SSH script creating a private temporary directory with an `EXIT HUP INT TERM` cleanup trap, running source `fetch-path` into it (failing visibly on a full disk), measuring exact byte length, and piping into destination `store-fetch --length`, moving zero WAN bytes while preserving all containment and symlink guards. For cross-host single regular-file fetches (`!is_directory`), compress stdout/stdin using the Rule 3 SSH gzip helper. Do not add `MJMV0002` (if baseline shows `.mj/keep/` dominates a move, wrap the stream with Rule 3 gzip instead).

### Push 8 (`#1692`): Remote Suspend Batching & Destroy-Hook Leftovers Retirement

In `mj-controller/src/controller/lifecycle/origin_delta.rs` and `lifecycle.rs:1184-1387`, batch wire-file reads and liveness probes inside each individual decision point using `ProbeFrames`, passing a consumed single-decision snapshot value so stale data can never cross an upgrade, delivery readback, wait, post-stop, or post-start boundary. In `wait_for_stop`, replace 2-SSH/100ms polling with a bounded remote shell wait loop (cancellable via process kill, preserving old-worker `identity-alive` safety).

In `mj-worker/src/checkpoint.rs` and `mj-controller/src/controller/checkpoint/{staging.rs,latched.rs}`, add `--spec -` stdin support to `restore-checkpoint` with no legacy fallback (Resume always installs the fresh build, and an old worker would interpret `-` as a filename), and compress `export-checkpoint` and `restore-checkpoint` stdin using the Rule 3 SSH gzip helper (so export keeps working with older session workers). In `environment.rs` / `lifecycle.rs`, batch Phase-2 container teardown (`stop` if running, `rm`, `volume rm` both volumes, git-cache snapshot removal) using `step_script` so per-step progress and durable retry semantics are preserved.

In `mj-cli/src/ec2.rs:1763, 1849-1870` and `mj-controller/src/controller/checkpoint/leftovers.rs`, wire the EC2 destroy event to retire `host_snapshot_cache` rows for the destroyed host's SSH destination, and update `leftovers.rs` to sweep overdue hosts concurrently with per-host backoff (no config-absence retirement timer).

### Push 9 (`#1693`): `mj ec2 create` Speedup & `mbx` Install Consolidation

In `mj-cli/src/ec2.rs`, replace `aws ec2 wait instance-status-ok` with a `running` state wait, immediately allocate and associate the Elastic IP, and perform one `wait_for_ec2_ssh` directly on the EIP (deleting ephemeral-IP SSH setup and the second wait). Delete the raw `Command::new("ssh")` helper (`ec2.rs:1502-1516`) and route through `mj_core::targets::SshTarget` + `ssh_command` + `ProcessExecutor`. Compute create-wide and per-region/type/arch AWS metadata as plain local values before the placement retry loop (no cache type; keep key-pair and security-group creation attempt-owned so `cleanup_record` stays intact).

In `mj-build-cache/src/mbx/install.rs`, collapse the 11 sequential SSH calls into one `ProbeFrames` discovery script plus transfer plus one `step_script` install/setup/profile/verify script, keeping controller upload (not adding a remote `curl` path unless measured upload exceeds a few seconds). In `mj-cli/src/ec2.rs` and `mj-daemon/src/pollers/quota.rs`, ensure the Nitro engine once before image preloads, run distinct-image `refresh_host_image` calls concurrently, skip `podman image prune -f` when `current_image_id` is `None`, and pre-stage the Rule-3-compressed `mj-worker` binary onto fresh Podman hosts concurrently with image pulls.

### Push 10 (`#1691`): Zero-Protocol-Bump Relay Pipelining (`call_many`) & Startup Readiness

In `mj-session-runtime/src/worker_client/exchange.rs`, add `call_many` to write a batch of independent read-only relay JSON lines back-to-back over the persistent connection and read their ordered responses under one batch deadline, treating any handler error or timeout as a connection failure (closing the connection so late responses can never desynchronize subsequent calls) and never pipelining writes or installs. Verify worker loop tolerance on older worker protocols (or gate on an existing negotiated protocol constant if older loops drop buffered lines). Use `call_many` for active/idle tick reads (`HistoryRequests`, `Attach`, `GuardianRequests`, optional `CpuUsage`), 60-second `credential_sync.rs` state reads (all 4 to 6 read-only state queries in 1 RTT), and prompt-submit `AttachmentPresent` checks.

In `mj-session-runtime/src/session_manager/standalone.rs`, skip `Acknowledge` when `Attach` returned zero new events and reported a matching watermark, while retaining a periodic `Acknowledge` so the worker's GC retry (`mj-worker/src/worker_runtime/unix/requests.rs:484`) still runs. In `mj-session-runtime/src/session_manager/readiness.rs`, replace tight 100 ms startup polling with `WaitForChange` (protocol >= 58, bounded timeout) and bounded backoff for older workers.


## Concrete Steps

All Cargo/mbx builds and tests run from `/home/jonathan/Projects/mjolnir/.mj/clones/112f9f78f35b003b036e070e798e1feb`. Every `cargo test` run executes outside the sandbox with elevated permissions per `AGENTS.md`. Use `mbx` if the checkout's `target` is an `mbx` symlink. Never target the host's default instance; every live measurement uses `env -i` with isolated `MJ_INSTANCE`, `MJ_CONFIG_DIR`, `MJ_DATA_DIR`, and `XDG_CACHE_HOME`, and stops its test daemon immediately afterward.

For each push (1 through 10):

1. Claim the child issue on GitHub (`gh -R BrokkAi/mjolnir issue edit <N> --add-assignee @me --add-label agent-in-progress`).
2. Implement the issue's edits (delegating focused slices with disjoint file ownership and one build owner at a time).
3. Run targeted tests for changed modules and new tricky/boundary tests (reporting exact passed counts per filter) plus `cargo clippy -p <edited-crates> --all-targets -- -D warnings`.
4. Commit only changed files on `master`, call `mcp_mj-review_request_review`, fix P1s/regressions (overriding minor P2s as follow-ups after 2 to 3 rounds), and push to `origin/master`.
5. Re-run the relevant isolated EC2 measurement, record before/after numbers in `Baseline & Post-Push Measurements`, and close/update the child issue if complete.


## Validation and Acceptance

Following `AGENTS.md`'s "Which tests to add" rules and the Advisor review, do not add trivial unit tests (such as asserting a constant value or testing simple string parsing). Add only integration and tricky-condition tests covering real boundaries, concurrency, failure/cancellation atomicity, and >64 KB streams:

- Rule 3 SSH gzip (`#1685` / `#1687` / `#1692`): stream >64 KB (multi-MB fixture) through the shared SSH gzip stdin/stdout helper against real `gzip -d` / `gzip -c`, verifying exact bytes, reduced wire size, and pipe-deadlock freedom.
- `#1686`: suspend host-staging promotion under cancellation, cross-filesystem copy-then-rename, and SHA-256 mismatch (verifying durable cleanup and zero silent corruption).
- `#1688`: `profile_receiver.pl` integration test running real Perl covering `private.tar`, O(1) `rename` plus Perl-copy `EXDEV` fallback (verifying `cp -pRP` is gone), shared-store immutability, and `StoreGate` concurrent cold-host serialization.
- `#1689`: `image_pull_gate` concurrency test proving two `run` holders proceed in parallel under shared lock while `pull` waits writer-preferring, executes exclusively, and respects cancellation; plus `RESOLUTIONS` single-flight concurrency test and `git_cache` deferrable GC admission test.
- `#1687`: direct move tests with a fake AWS client (explicitly never touching real AWS) verifying CLI record lookup, `describe-instances` SG/VPC/private-IP selection, cross-region grant routing, and same-host `fetch` symlink-escape refusal + full-disk visible failure; plus one real EC2-to-EC2 Podman move and one real EC2-to-EC2 Nitro move.
- `#1692`: `wait_for_stop` with old-worker unsupported `identity-alive`, `restore-checkpoint --spec -`, `step_script` teardown partial-failure durability, and `leftovers.rs` concurrent sweep + destroy-hook retirement.
- `#1693`: `ec2.rs` tests with fake AWS/SSH covering `running` wait + early EIP association, shared `ssh_command` construction, concurrent image pulls, fresh-host prune skip, and `mbx/install.rs` round-trip consolidation.
- `#1691`: `call_many` pipelining tested against the real worker relay loop (ordered multi-reply, mid-batch handler error closing the connection, timeout teardown, write rejection) plus periodic `Acknowledge` GC retry preservation.
- Live empirical acceptance: recorded before/after wall times, `session launch phase ended` timings, and `program=ssh` counts in `Baseline & Post-Push Measurements`.


## Idempotence and Recovery

No database schema migrations are required for any push. Every remote staging operation uses a private temporary path with shell cleanup traps (`EXIT HUP INT TERM`) and, where staging survives a daemon crash (`#1686`), a durable ownership record swept on recovery. Every push ships independently on `master` so regressions are isolated to a single small commit.


## Artifacts and Notes

Reconnaissance reports (read-only):

    .mj/agents/98227274e372adcfcab6d218bf899e44/recon-1695.md      (#1685, #1688)
    .mj/agents/44878dfe90cb42c09e40b526e33d439a/recon.md           (#1686, #1687, #1692)
    .mj/agents/88e62c4167c0395629ea7835c60ae8f4/reconnaissance.md  (#1689, #1694)
    .mj/agents/734969972cb95bc12cf348a9600743a9/recon-1690-1691.md (#1690, #1691)
    .mj/agents/5207ae288e24871c9ce564aa1d320100/recon-1693.md      (#1693)

Advisor pre-implementation verification reports (`91233b0efdd193066e7b4eeb46c8db10`):

    /home/jonathan/Projects/mjolnir/.mj/clones/91233b0efdd193066e7b4eeb46c8db10/.mj/agents/e647fcc4…/relay-rtt-no-protocol-change.md
    /home/jonathan/Projects/mjolnir/.mj/clones/91233b0efdd193066e7b4eeb46c8db10/.mj/agents/eaab8524…/findings.md
    /home/jonathan/Projects/mjolnir/.mj/clones/91233b0efdd193066e7b4eeb46c8db10/.mj/agents/5dc132fe…/details.md
    /home/jonathan/Projects/mjolnir/.mj/clones/91233b0efdd193066e7b4eeb46c8db10/.mj/agents/9837d2d8…/details.md


## Interfaces and Dependencies

No new external Cargo dependencies are added (`flate2`, `tar`, `zip`, `sha2` already exist in workspace `Cargo.toml`).

In `mj-core/src/targets.rs` and `mj-core/src/targets/ssh.rs` (Rule 3):

    // Single shared SSH gzip stdin/stdout compression option for bulk uncompressed streams
    // (worker binary upload, single-file fetch, canonical_session stdin, worker pre-stage).
    pub enum StreamCompression { None, Gzip }

In `mj-targets/src/image_pull_gate.rs` (`#1689`):

    // Writer-preferring in-process shared/exclusive lock + cross-process flock polling.
    // Shared for `podman image exists` + `podman run --pull=never`; exclusive for pull/prune.

In `mj-session-runtime/src/worker_client/exchange.rs` (`#1691`):

    // Pipeline independent read-only requests over one persistent relay connection under one deadline.
    // Handler error or timeout closes the connection.
    pub async fn call_many(&mut self, requests: &[RelayRequest], timeout: Duration) -> Result<Vec<RelayResponse>>;

In `mj-controller/src/controller/move_session/transport/ec2_grant.rs` (`#1687`):

    // Extend grant key with authoritative region + optional aws_profile from local CLI EC2 records.
    pub struct Ec2GrantKey {
        pub region: Option<String>,
        pub aws_profile: Option<String>,
        pub security_group_id: String,
        pub cidr_ip: String,
        pub port: u16,
    }


## Revision History

- 2026-11-24 17:15Z: Revised entire ExecPlan after pre-implementation review from Advisor `91233b0efdd193066e7b4eeb46c8db10`. Adopted all five global rules (real EC2 baseline + post-push measurements, reuse `step_script`/`ProbeFrames`, one shared SSH gzip stream helper, no unmeasured caches/timers, one issue per push starting with background-noise stoppers) and applied all 15 additional cuts (Protocol 91 replaced by `exchange.rs` `call_many`, Resume pack helper, `FICLONE`, `supported()` cache, prewarm timer, `podman info` memo, borrowed-subagent shim skip/parent-facts reuse, `MASTER_CHECK_INTERVAL`, `MJMV0002`, IMDSv2 discovery replaced by CLI records + AWS `describe-instances`, config-absence leftover retirement timer, remote `curl` path, restore `--spec -` fallback, `#1694` availability TTL and `mbx_service` memoization switch, and `static LAST` deleted instead of re-keyed).
- 2026-11-24 17:36Z: Incorporated post-baseline empirical revisions from Advisor `91233b0efdd193066e7b4eeb46c8db10` after measuring unmodified `3ce12540a` on `ec2-m8id16-d`: cut `#1688` `tar | exec tar` pipe (saves only 0.8 s vs extra transport/image requirement), delete `cp -pRP` everywhere in `profile_receiver.pl` using 0.07 s Perl copy for `EXDEV` fallback too (eliminates 2.5 s copy and ZFS 2.4.1 `cp` exposure), leave `supported()` probe untouched (162 ms), added top-priority read-only timing split of `prepare exact managed harness before worker startup` (`11.9 s` warm, 35% of launch), added measurement gap closures (borrowed-child launch, realistic compressible plugin upload, realistic-archive suspend), and reordered pushes after Push 1 to `#1685` -> `#1688` -> harness-prep report -> `#1694` -> `#1686` -> `#1689` slices (>0.3 s).
