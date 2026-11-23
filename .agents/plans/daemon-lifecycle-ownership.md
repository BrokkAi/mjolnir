# Daemon lifecycle ownership, non-blocking observation, and exit-status capture

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document is maintained in accordance with `.agents/PLANS.md`. It tracks Epic #1658 (`epic: daemon-lifecycle`), covering GitHub issues #1568, #1458, and #1644. Issue #1601 (per-session `RecoveryGate` during worker upgrade) was audited and intentionally excluded because it lives in `mj-daemon/src/worker_upgrade.rs` and `mj-controller/src/controller/worker_restart.rs` with no shared state or code dependency on daemon startup, discovery, or process ownership.

## Purpose / Big Picture

Today, Mjolnir's daemon launcher and client layer conflates "this endpoint did not answer an RPC within N seconds" with "that daemon process exited," while simultaneously lacking any persisted engine/container record for container daemons (`daemon.json` records `pid: 0` and skips placement) and discarding the daemon's OS exit status at launch (`spawn_detached` reparents the daemon or drops its handle). That produces three user-visible failures:

1. Under load or during handoffs (#1568), arbitrary 1-second, 2-second, 3-second, 5-second, and 120-second RPC wrappers stall lock acquisition and readiness notices, fabricate a wrong legacy `Host` placement on timeout for `pid: 0`, trap attachment refresh in transparent `UpgradePending` retries, or treat a 1-second silent `Ping` after `PrepareUpgrade` as proof that a container daemon exited while it still owns its store.
2. When a container daemon dies and leaves a `pid: 0` `daemon.json` behind (#1458), `recorded_daemon_is_gone` refuses `pid: 0` unconditionally, blocking automatic upgrades and new commands until the user manually intervenes.
3. When a daemon dies uncleanly without panicking (#1644), the successor daemon can only guess from leftover metadata and log modification times, with no exit code, terminating signal, or core-dump flag recorded.

After completing this plan, the daemon launch and discovery code is measurably simpler than before and provides three observable guarantees: observational RPCs never silently loop across upgrades or pretend silence means death; a dead or in-flight container daemon is decided by its actual engine's process list (`ps` + `top`) without requiring any image-side probe or killing live workers inside the container; and any non-zero or signaled daemon exit writes its raw OS exit status keyed by PID so the next daemon reports exactly why the predecessor vanished.

## Progress

- [x] (2026-11-23 01:50Z) Completed read-only reconnaissance of `#1568`, `#1458`, `#1644`, and `#1601` at `fb408ce04` and verified current `master` at `9f36d0f53`.
- [x] (2026-11-23 02:21Z) Claimed `#1658`, `#1568`, `#1458`, and `#1644` with `agent-in-progress` and drafted this ExecPlan.
- [x] (2026-11-23 02:40Z) Reviewed ExecPlan with MJ Advisor (`91233b0efdd193066e7b4eeb46c8db10`) and reshaped Milestones 1, 2, and 3 to fix three M2 flaws and cut unnecessary machinery.
- [x] (2026-11-23 03:01Z) Milestone 1 (#1568 Phase 1): Single-exchange `request_single` for attachment, concurrent readiness/notice driving, abort-on-drop `UpgradeBlockers`, straight-line two-phase handoff, `pid == 0` placement fix, and store-guard-backed corrupt `daemon.json` recovery.
- [x] (2026-11-23 03:44Z) Milestone 2 (#1458 + #1568 Phase 2): Engine `ps`+`top` container liveness owner, container launch record (`daemon-container.json`), pre-launch adoption gate, legacy `pid: 0` `mj daemon stop --force` recovery, and deletion of `wait_until_answering`, `wait_for_endpoint_down`, and the 3-second absence `Ping`.
- [ ] Milestone 3 (#1644): Minimal detached one-child exit waiter (`mj daemon-wait-exit`) wrapping outside `systemd-run --scope`, PID-keyed non-zero/signaled exit records, and `previous_daemon.rs` reporting.
- [ ] Final validation, code review (`request_review`) and push per milestone, and close `#1568`, `#1458`, `#1644`, and `#1658`.

## Surprises & Discoveries

- Observation: `DaemonMetadata` in `mj-client/src/daemon.rs:55-72` has `#[serde(deny_unknown_fields)]` and marks both `instance_id` and `placement` with `#[serde(skip)]`.
  Evidence: Adding an optional field directly to `daemon.json` would fail deserialization in already-installed Mjolnir binaries during upgrades. Container launch ownership must live in a separate owner-only record (`daemon-container.json`) parsed tolerantly (no `deny_unknown_fields`), while keeping `daemon.json` frozen.

- Observation: Every container daemon upgrade launches into the *new* build's image (`resolve_image` in `mj-cli/src/daemon/container.rs:312-379` requires matching revision label, and `ContainerSpec::for_this_host` names the container from the image ID). Consequently, every handoff or stale-record probe targets the *previous* build's container, which cannot be assumed to carry any new CLI subcommand.
  Evidence: MJ Advisor review of Milestone 2 (Flaw F1). Using engine-level `ps` + `top` works uniformly against every existing container image with zero image-side CLI contract.

- Observation: If a container launcher dies after `exec_daemon` and before writing `daemon.json`, keeping only a token fingerprint in the sidecar would leave the running daemon permanently unreachable (nobody holds its token) while its process still owns the store; and `decide_existing_daemon` ignoring the container launch record when `daemon.json` is absent would let the next client launch on top of it.
  Evidence: MJ Advisor review of Milestone 2 (Flaws F2 and F3). Storing the container launch record `{runtime, container_id, container_name, address, token, started_at}` in the same `0600` directory as `daemon.json` lets the next launcher adopt an in-flight container daemon directly.

- Observation: Of the five client observations (`Ping`, `Identity`, `Attach`, `UpgradeBlockers`, `PrepareUpgrade`), only `Attach` can ever observe `UpgradePending` (`Ping` and `Identity` are exempt in `mj-daemon/src/daemon/actions.rs:1347-1358`; `UpgradeBlockers` and `PrepareUpgrade` are management actions in `serve.rs:181-188`; and `PrepareUpgrade` already bypasses reconnect at `mj-client/src/daemon.rs:2109`).
  Evidence: MJ Advisor review of Milestone 1. Exposing `DaemonClient::request_single` alone suffices without `ObservationReply` or five `_once` wrappers.

- Observation: `mj-cli/src/daemon.rs:1829-1853` (`wait_for_endpoint_down`) treats a 1-second timeout, connect refusal, or non-Pong reply as proof that a container daemon exited after `PrepareUpgrade` or `Stop`, even though `249df74e4` fixed the analogous split-brain bug on discovery (`recorded_daemon_is_gone`).
  Evidence: `wait_for_endpoint_down` returns `Ok(())` whenever `answers` is `false`.

- Observation: Container launch has a duplicate 120-second readiness loop (`wait_until_answering` in `mj-cli/src/daemon/container.rs:128-162`) before the shared 300-second `wait_for_ready_daemon` loop in `mj-cli/src/daemon.rs:1352-1385`, and `LaunchedDaemon::is_alive` plus `processes_in` (`container.rs:166-180,588-618`) turn container `inspect` errors into false `Exited` / `None`.
  Evidence: Inspecting `container.rs:94-180,546-618` and `daemon.rs:1410-1417`.

- Observation: `mj-cli/src/daemon.rs:434-515` (`legacy_placement(0)` and `daemon_identity_or_legacy`) turns `pid: 0` into `DaemonPlacement::Host` on `Identity` error/timeout or cross-protocol upgrade, sending handoff to the wrong store guard.
  Evidence: `pid == 0` is itself proof of a container daemon.

- Observation: On Linux, `systemd-run --user --scope` execs `mj daemon-run` in place (`mj-cli/src/daemon.rs:693-698`). Wrapping `mj daemon-wait-exit -- <existing host command>` *outside* `systemd-run --scope` gives the waiter the daemon's real PID as its direct child while keeping the waiter outside the scope so it survives scope-level kills (such as cgroup OOM kills).
  Evidence: MJ Advisor review of Milestone 3 and `mj-cli/src/daemon.rs:688-702`.

- Observation: Inside this session container, neither a Docker daemon socket nor a Podman daemon socket is mounted (`docker info` and `podman-remote info` fail). Real-engine acceptance requires a host with Podman/Docker, while unit/integration tests here verify recorded Docker and Podman `ps`/`top` outputs plus fake-engine scripts.
  Evidence: Checked `docker info` and `podman-remote info` on 2026-11-23.

## Decision Log

- Decision: Use engine `ps --all --no-trunc --filter id=<id>` + `inspect --format={{.State.Running}}` + `top <id>` (`-o args` on Docker, `args` on Podman, matching `mj daemon-run`, the upcoming `mj daemon-wait-exit`, or the pre-`exec` image launch shell) as the single liveness owner for container daemons.
  Rationale: Works on every existing older container image (fixing Flaw F1), sees an `exec`'d daemon from its first instant (eliminating launch tickets), preserves sleeping-container workers, and is the same authority `remove_idle_predecessors` already trusts. Because `mj daemon-run` opens `controller.lock` close-on-exec, the absence of a `daemon-run` process in the container proves the volume store guard is free.
  Date/Author: 2026-11-23 / Coordinator (incorporating MJ Advisor review)

- Decision: Persist one owner-only (`0600`) host record strictly for container launches (`daemon-container.json`, tolerant parse without `deny_unknown_fields`) holding `{schema, runtime, container_id, container_name, address, token, started_at}`, written under `DaemonStartGuard` before `exec_daemon`. Derive "published" from whether `daemon.json` matches `(address, token)`. Add no sidecar for host daemons.
  Rationale: Fixes Flaws F2 and F3 so a subsequent launcher can adopt an unpublished in-flight container daemon instead of stranding it or launching over it. Cuts `probe-store-lock`, launch tickets (`MJ_DAEMON_REQUIRE_TICKET`, `ticket_armed`), `MJ_DAEMON_GENERATION` / `instance_id` coupling, `published`, `token_sha256`, `OwnerPlacement::Host`, and `volume`/`image` fields.
  Date/Author: 2026-11-23 / Coordinator (incorporating MJ Advisor review)

- Decision: Keep the unpublished owner record when a startup process probe reports exit. Reuse its token if the retry targets the same engine, full container ID, and published loopback address. Clear the record when the retry switches targets or the user explicitly stops an exited pending container.
  Rationale: A detached engine exec can reach the store after the launcher's death and after the next client's process check. The volume store guard admits one daemon, and sharing the token lets the client authenticate to whichever exec wins.
  Date/Author: 2026-11-23 / Milestone 2 review

- Decision: Require a readable container owner record before requesting stop or handoff from a legacy container with a published endpoint. Allow explicit force recovery to clear an unreadable record after an unreachable legacy endpoint has been checked, or when there is no published endpoint.
  Rationale: Stop and handoff need the recorded engine and ID to verify release. A failed ownership query leaves the daemon and its credentials available. A damaged record can be cleared after the operator verifies that there is no container daemon.
  Date/Author: 2026-11-23 / Milestone 2 review

- Decision: Delete `wait_until_answering`, `wait_for_endpoint_down`, and unconditionally delete the 3-second `Ping` in `recorded_daemon_is_gone`.
  Rationale: One authoritative ownership check per placement (`ControllerStoreGuard` + PID check on host; `daemon-container.json` + engine `ps`/`top` on container) replaces three redundant and contradictory RPC liveness loops.
  Date/Author: 2026-11-23 / Coordinator (incorporating MJ Advisor review)

- Decision: For legacy `pid: 0` `daemon.json` with no `daemon-container.json` and no answering endpoint, refuse visibly and offer `mj daemon stop --force` to clear the stale `daemon.json` (which still stops the daemon normally if it answers first, and only removes `daemon.json` if it is an unanswerable legacy `pid: 0` record with no container record).
  Rationale: Obeys `AGENTS.md`'s rule never to leave the user at a dead end without an explicit recovery action the surface performs, while refusing to guess engines (`72f024e29`) or silently delete records that could belong to a live daemon.
  Date/Author: 2026-11-23 / Coordinator

- Decision: Trim Milestone 1: drop `ObservationReply` and five `_once` wrappers (expose only `DaemonClient::request_single` for the monitor's `Attach`); keep `maintain_attachment`'s 2-second tick and 5-second display threshold (no cross-tick exchange or generation tracking); write `hand_off_daemon` as two straight-line sequential functions (`ask_for_handoff` at 250ms cadence, then `wait_for_daemon_release`); make `legacy_placement(0)` never return `Host`; let the host store guard resolve unreadable `daemon.json` (free means `Absent`, held means an error naming the file); and share one abort-on-drop helper for `UpgradeBlockers`.
  Rationale: Simpler code with zero risk of dashboard false-alarm regressions (`dashboard.rs:1005`), structurally impossible handoff regression, and no dead end on corrupt host `daemon.json`.
  Date/Author: 2026-11-23 / Coordinator (incorporating MJ Advisor review)

- Decision: Simplify Milestone 3 (#1644): wrap `mj daemon-wait-exit -- <host command>` *outside* `systemd-run --scope`; key exit records by daemon child PID in `<data_dir>/daemon-exits/<pid>.json`; write a record only on non-zero or signaled exit (excluding a dedicated `STORE_OWNED_EXIT_CODE = 75` when a duplicate launch loses `ControllerStoreGuard`); have `previous_daemon.rs` report and delete every exit record present on startup; drop generation plumbing, 16-file pruning, consumed markers, PID handshake pipe, and 500ms background recheck. Parse `DaemonExitRecord` tolerantly (no `deny_unknown_fields`).
  Rationale: Wrapping outside `systemd-run --scope` preserves cgroups and lets the waiter survive scope-level kills. PID-keyed write-on-failure + delete-on-report removes four state mechanisms while catching crashes before `daemon.json` publication or after watchdog removal.
  Date/Author: 2026-11-23 / Coordinator (incorporating MJ Advisor review)

- Decision: Exclude #1601 from this ExecPlan and epic #1658.
  Rationale: Independent per-session `RecoveryGate` work in `mj-daemon/src/worker_upgrade.rs` and `mj-controller/src/controller/worker_restart.rs`.
  Date/Author: 2026-11-23 / Coordinator

## Outcomes & Retrospective

Milestone 1 passed all dev-profile checks: 14 client daemon tests (`brokk-mj-client`), 30 CLI daemon tests (`brokk-mjolnir`), 22 startup integration tests (`daemon_startup`), `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --all -- --check`. A named-instance launch with corrupt metadata reported the file while another process held the host store guard, then started and published valid metadata after that guard was released.

Startup polls one readiness exchange at a time while the notice, deadline, and process checks run independently. Handoff admission retries `PrepareUpgrade` only before acceptance while concurrently watching for owner replacement or exit (surviving teardown socket drops before the store guard releases), and never re-asks after post-ack release expiry. Lock and handoff notices pre-warm `UpgradeBlockers` 1s before `START_NOTICE_DELAY` in an abort-on-drop background task so responsive daemons name blockers on the very first notice while hung daemons emit generic notices on time without stalling lock acquisition or handoff.

Milestone 2 records the engine and full container ID before starting `mj daemon-run`. A later client can adopt that process and publish `daemon.json` after readiness. Startup, accepted stop, and handoff all check the recorded process through the engine. A stopped or absent daemon can be replaced while workers keep running in the container. Engine failures and unreachable legacy records produce a visible error. The legacy error explains when to use `mj daemon stop --force`.

Dev-profile verification passed 15 client daemon tests, 38 CLI daemon tests, and 23 startup integration tests. Clippy, rustfmt, and `git diff --check` passed, and the removed polling helpers have no remaining references. The live Podman test is opt-in and was skipped because this environment has no engine socket. Fake Podman commands on an isolated `PATH` exercised the real subprocess path, and Docker and Podman process-list fixtures covered the daemon matcher.

Milestone 2 review follow-up passed 15 client daemon tests, 41 CLI daemon tests, and 23 startup integration tests. New fake-engine regressions cover the original exec winning a same-target retry, owner seeding failure before an RPC mutation, and explicit recovery from a corrupt owner record. The live Podman test remains opt-in.

Milestone 3 remains pending.

## Context and Orientation

Mjolnir splits control and execution across a single per-instance daemon (`mj daemon-run`, in `mj-daemon/`) and one worker process per session (`mj-worker/`) that survives daemon restarts. Short-lived clients (`mj` CLI commands, the TUI dashboard, and MCP tools) discover, start, replace, and talk to the daemon through `mj-client/src/daemon.rs`, `mj-cli/src/daemon.rs`, and `mj-cli/src/daemon/container.rs`.

Three local artifacts coordinate daemon startup today:

1. `daemon.json` (`mj_client::daemon::metadata_path()` in `data_dir()`): written by host daemons after binding and startup (`mj-daemon/src/daemon/process.rs:835-842`), or written on the host by the container launcher after the container daemon answers `Ping` (`mj-cli/src/daemon/container.rs:103`). `DaemonMetadata` (`mj-client/src/daemon.rs:55-72`) uses `#[serde(deny_unknown_fields)]` and skips `instance_id` and `placement` on disk; container records write `pid: 0`.
2. `controller.lock` (`mj_controller::controller::ControllerStoreGuard` in `mj-controller/src/controller.rs:169-215`): an OS advisory file lock opened close-on-exec by `mj daemon-run` (`mj-daemon/src/daemon/process.rs:114`) and held for the entire lifetime of the SQLite writer. For host daemons it lives in host `data_dir()`; for container daemons it lives inside the container's named volume at `/var/lib/mjolnir/data/controller.lock`.
3. `daemon-start.lock` (`mj-cli/src/daemon.rs:164-242`): an advisory host lock serializing clients that inspect (`SharedStartGuard`) or launch/replace (`DaemonStartGuard`) the daemon.

On the client wire side, `DaemonClient::request` (`mj-client/src/daemon.rs:2080-2119`) wraps `request_once` in `request_with_reconnect`, retrying indefinitely on `DaemonReply::UpgradePending` for every action except `PrepareUpgrade`. `request_once` (`mj-client/src/daemon.rs:2274-2322`) already uses cancellation-safe `take_stream` and `finish_exchange` so an interrupted exchange drops the stream rather than leaving stale bytes on it.

On the crash-diagnostic side, `report_unclean_previous_daemon` (`mj-daemon/src/daemon/previous_daemon.rs:93-132`) runs right after `ControllerStoreGuard::acquire()` succeeds. It inspects leftover `daemon.json` and log mtimes, but because `mj_core::subprocess::spawn_detached` (`mj-core/src/subprocess.rs:671-727,757-805`) double-forks on Unix and drops the handle on Windows, no process ever collects the daemon's OS wait status.

Core invariants from `AGENTS.md` preserved throughout:

- Never treat RPC silence, connect errors, or engine inspect failures as proof that a daemon exited (`249df74e4`).
- Never stop or recreate the sleeping daemon container during stale-record recovery or automatic handoff (live session workers run inside it).
- Keep `daemon.json` readable by older installed binaries (`deny_unknown_fields` unchanged).
- Keep documented lifecycle clocks (`START_TIMEOUT` = 300s, `STOP_TIMEOUT` = 30s, `STOP_DRAIN_TIMEOUT` = 90s including the 60s destroy bound, TCP connect timeout = 1s) distinct from ad-hoc RPC wrappers.

## Plan of Work

Execute in three sequential milestones, committing, reviewing (`request_review`), and pushing each milestone cleanly to `origin/master` before starting the next.

### Milestone 1: Decouple progress from RPC latency and eliminate ad-hoc client timeouts (#1568 Phase 1)

1. **`mj-client/src/daemon.rs`:** Make `DaemonClient::request_single(&mut self, action: DaemonAction) -> Result<DaemonReply>` public (delegating directly to existing cancellation-safe `request_once` with no transparent `UpgradePending` reconnect). Keep `DaemonClient::request` unchanged for normal user commands.
2. **`mj-cli/src/daemon.rs` — concurrent readiness (`wait_for_ready_daemon`, lines 1352-1385):** Hold one in-flight probe future and drive `START_TIMEOUT` (300s), `START_NOTICE_DELAY`, and periodic `is_alive()` checks concurrently with `tokio::select!`. Retry after `RETRY_DELAY` only when the probe actually returns an error; never cancel a slow valid probe every 2 seconds. Remove the 2-second `tokio::time::timeout` from `ping_daemon` where callers already drive their own lifecycle clock or cancellation. Report the pending probe alongside the last completed error if `START_TIMEOUT` expires.
3. **`mj-cli/src/daemon.rs` — unreadable `daemon.json` handling (`decide_existing_daemon`, lines 1038-1044; `hand_off_daemon`, lines 1644-1660):** Distinguish `daemon_not_running(&error)` (`NotFound`) from I/O or JSON parse errors. In `decide_existing_daemon`, when `daemon.json` exists but cannot be read or parsed, probe the host `ControllerStoreGuard::try_acquire()`: if the guard is free (and no live container record claims ownership once M2 lands), treat as `ExistingDaemon::Absent` so a corrupt host file is never a dead end; if the guard is held, return a clear error naming `daemon.json` rather than silently misclassifying it. In `hand_off_daemon`, only treat `daemon.json` as replaced when it parses with a different `(address, token)` (never on a read/parse error).
4. **`mj-cli/src/daemon.rs` — abort-on-drop `UpgradeBlockers` helper:** Delete the inline 2-second `tokio::time::timeout` in `upgrade_blockers` (lines 1763-1774). Introduce one tiny abort-on-drop helper (`PendingBlockers` holding an optional `tokio::task::JoinHandle<Option<Vec<String>>>` whose `Drop` calls `abort()`) shared by both `acquire_start_lock` (lines 193-242) and `hand_off_daemon` (lines 1627-1745). When `notice_at` fires, start at most one background query if none is running, emit the latest resolved blockers or the generic notice immediately without delaying lock retries or `PrepareUpgrade`, and abort automatically on return.
5. **`mj-cli/src/daemon.rs` — straight-line two-phase handoff (`hand_off_daemon`, lines 1627-1745):** Split into two sequential functions: `ask_for_handoff(metadata, ...)` (asks `PrepareUpgrade` without the 5-second timeout wrapper, selecting one in-flight `PrepareUpgrade` exchange against notices, blocker refreshes, metadata replacement, and pre-acceptance `recorded_daemon_is_gone`; re-asks after 250ms only on explicit `DaemonReply::UpgradePending`, which renews the server's 10-second drain window in `mj-store/src/upgrade.rs:543-560`; returns `HandoffAskOutcome::{Accepted, Gone, Replaced}`) followed by waiting for release (`wait_for_daemon_release` once accepted). Because the two phases are sequential, an expired post-ack wait reports an error and structurally cannot regress into asking for admission again.
6. **`mj-cli/src/daemon.rs` — `pid == 0` placement fix (`legacy_placement` and `daemon_identity_or_legacy`, lines 434-515):** Never return `DaemonPlacement::Host` when `metadata.pid == 0`. Represent the fallback as `Option<DaemonPlacement>` (or explicit unknown container) so `pid == 0` is recognized as a container daemon even when `Identity` fails or the protocol is older; remove the 2-second timeout wrapper from `daemon_identity_or_legacy` where callers do not need it (or route container/host decisions through placement without fabricating `Host`).
7. **`mj-cli/src/daemon.rs` — non-blocking attachment refresh (`maintain_attachment` / `observe_attachment`, lines 1980-2036):** Keep one observation per 2-second tick and keep the 5-second bound as a named display threshold (`ATTACHMENT_REFRESH_DISPLAY_TIMEOUT = Duration::from_secs(5)` so a momentarily busy daemon does not trip the dashboard failure banner at `dashboard.rs:1005`). Inside `observe_attachment`, use `client.request_single(DaemonAction::Attach { client_id })` instead of `client.attach(...)` so `DaemonReply::UpgradePending` immediately surfaces `DaemonPresence::Missing("daemon upgrade handoff is in progress")` rather than transparently looping while the UI displays `Attached`.

### Milestone 2: Engine `ps`+`top` container liveness owner, `daemon-container.json`, and deletion of duplicate loops (#1458 + #1568 Phase 2)

1. **Container launch record (`mj-client/src/daemon.rs`):** Add `CONTAINER_OWNER_SCHEMA: u32 = 1`, `ContainerDaemonRecord` (`schema: u32`, `runtime: DaemonRuntime`, `container_id: String`, `container_name: String`, `address: SocketAddr`, `token: String`, `started_at: String` — **without** `#[serde(deny_unknown_fields)]` for forward-compatible tolerant parsing), `container_owner_path() -> PathBuf` (`data_dir().join("daemon-container.json")`), and atomic `0600` read/write/remove helpers. Leave `DaemonMetadata` (`daemon.json`) completely unchanged.
2. **Engine `ps`+`top` liveness owner (`mj-cli/src/daemon/container.rs`):**
   - Implement `probe_container_daemon(runtime: DaemonRuntime, container_id: &str) -> ContainerDaemonStatus` returning `ContainerDaemonStatus::{Running, Exited, Unknown(String)}`:
     - Run `<engine> ps --all --no-trunc --filter id=<container_id> --format {{.ID}}`. Non-zero exit or spawn error returns `Unknown(reason)` (never parsing engine error strings). Exit 0 with no matching ID line returns `Exited` (container definitively absent).
     - Run `<engine> inspect --format={{.State.Running}} <container_id>`. Non-zero exit returns `Unknown(reason)`. Output `"false"` returns `Exited` (container definitively stopped). Output anything other than `"true"` returns `Unknown(reason)`.
     - Run `<engine> top <container_id> -o args` (Docker) or `<engine> top <container_id> args` (Podman). Non-zero exit returns `Unknown(reason)`. Parse lines after the header: a pure helper `top_output_has_daemon_run(stdout: &str) -> bool` looks for `mj daemon-run`, the Milestone 3 `mj daemon-wait-exit` wrapper, or the corresponding transient `sh -c` launch command targeting `/opt/mjolnir/mj`. Match returns `Running`; no match returns `Exited`.
   - Fix `processes_in` (`container.rs:588-618`, used by `remove_idle_predecessors`) to use the same safe `ps` + `inspect` distinction so inspect errors fail closed (`Err`) instead of returning `Ok(None)` and risking `rm --force` on a container with live workers.
   - Expose `resolve_container_id(runtime: DaemonRuntime, name: &str) -> Result<Option<String>>` using `ps --all --no-trunc --filter name=^/<name>$ --format {{.ID}}` after `ensure_running` (and when seeding a legacy record from authenticated `Identity`).
   - Update `container::launch` (`container.rs:94-108`): after `ensure_running` and `published_port`, resolve `container_id`, build `ContainerDaemonRecord`, atomically write `daemon-container.json` under `DaemonStartGuard` **before** calling `exec_daemon`, call `exec_daemon`, and return `ContainerLaunch { runtime, container_id, container_name, metadata }` directly. **Delete `wait_until_answering`** (`container.rs:128-162`)!
   - Reuse `probe_container_daemon` in `LaunchedDaemon::is_alive` (`mj-cli/src/daemon.rs:1410-1417`), treating `Running` or `Unknown` as alive (never false `Exited` on an inspect error!) and only `Exited` as dead.
3. **Single release authority and pre-launch adoption gate (`mj-cli/src/daemon.rs`):**
   - Introduce `DaemonRelease::{StillOwned, Exited, Replaced, Unknown(String)}` and one `probe_daemon_release(metadata: &DaemonMetadata) -> Result<DaemonRelease>`:
     - If `daemon.json` now parses with a different `(address, token)`, return `Replaced`.
     - If `metadata.pid == 0` (container daemon): read `daemon-container.json` (unreadable or unbound to `(metadata.address, metadata.token)` means "no record" → `Unknown`, never `Exited`). If bound, call `probe_container_daemon(record.runtime, &record.container_id)` and map `Running -> StillOwned`, `Exited -> Exited`, `Unknown(reason) -> Unknown(reason)`.
     - If `metadata.pid > 0` (host daemon): check host `ControllerStoreGuard::try_acquire()`: if held → `StillOwned`; if free, check `host_pid_is_daemon(pid)`. A live daemon yields `StillOwned`, a gone or reused PID yields `Exited`, and an inaccessible command line yields `Unknown`.
   - **Delete** the 3-second `Ping` at the end of `recorded_daemon_is_gone` (`daemon.rs:996-1003`) unconditionally.
   - **Delete** `wait_for_endpoint_down` (`daemon.rs:1829-1853`) and replace both post-ack handoff wait and `wait_after_accepted_stop` with a single `wait_for_daemon_release(metadata, timeout)` powered by `probe_daemon_release` (removing `daemon.json` only if it still matches `(address, token)` and removing `daemon-container.json` only if it matches `(address, token)`).
   - **Seed legacy running containers:** When an authenticated `Identity` names a container and engine for a `pid: 0` record, resolve the full container ID and write a matching `daemon-container.json` under `DaemonStartGuard` before asking for stop or handoff. Shared discovery passes the placement to the lock holder.
   - **Pre-launch container adoption gate** (fixes Flaws F2 and F3): In `decide_existing_daemon` (when `daemon.json` is absent) and in `connect_or_start_holding` (before launching a new container daemon under `DaemonStartGuard`), read `daemon-container.json` if present:
     - `Running`: adopt it — run the unified `wait_for_ready_daemon` against the record's `metadata` and `probe_container_daemon`, publish `daemon.json` after `Ping` and `Identity`, then run the ordinary build and placement decision on the published record.
     - `Exited`: pass the unpublished record to the launcher. A retry targeting the same engine, full container ID, and loopback address reuses the token. A different target clears the prior record.
     - `Unknown(reason)`: fail visibly with `reason` and do NOT launch a second container daemon.
   - **Legacy `pid: 0` recovery UX (`mj-cli/src/main.rs`, `mj-cli/src/daemon.rs`):** Add `--force` to `DaemonStopArgs` (`mj daemon stop --force`). When `daemon.json` has `pid: 0`, no matching `daemon-container.json`, and does not answer `Stop`/`Ping`:
     - Ordinary startup, handoff, and `mj daemon stop` refuse visibly with a message naming the stale `daemon.json`, explaining that Mjolnir cannot verify which container engine owns a pre-tracking `pid: 0` record, and directing the user to confirm no container daemon is running and run `mj daemon stop --force` to clear it.
     - `mj daemon stop --force` first tries to stop the endpoint normally if it answers; if it does not answer and there is no matching `daemon-container.json`, it removes the stale `daemon.json` cleanly; if a matching `daemon-container.json` exists and the daemon is still running/unresponsive, it uses the recorded `(runtime, container_id)` for explicit `container::stop`.

### Milestone 3: Minimal detached one-child exit waiter and startup crash reporting (#1644)

1. **Exit record schema (`mj-core/src/daemon_exit.rs` or `mj-client/src/daemon.rs`):** Define `DAEMON_EXIT_SCHEMA: u32 = 1`, `STORE_OWNED_EXIT_CODE: i32 = 75`, and `DaemonExitRecord` (`schema: u32`, `pid: u32`, `exited_at: String`, `exit_code: Option<i32>`, `signal: Option<i32>`, `signal_name: Option<String>`, `core_dumped: Option<bool>`) with **tolerant parsing** (no `deny_unknown_fields`, `#[serde(default)]` on optional fields) and `daemon_exits_dir() -> PathBuf` (`data_dir().join("daemon-exits")`).
2. **Distinguish lost-store-lock exit in `mj-daemon/src/daemon/process.rs` and `mj-cli/src/main.rs`:** In `mj-controller/src/controller.rs`, attach a typed marker (`StoreAlreadyOwnedError`) when `ControllerStoreGuard::acquire` finds the lock held. In `mj-cli/src/main.rs`'s `daemon-run` handler, print the error as today and exit with `STORE_OWNED_EXIT_CODE` (`75`) when `StoreAlreadyOwnedError` is in the chain, so the waiter ignores benign duplicate-launch lock races while manual `mj daemon-run` still exits non-zero visibly.
3. **Minimal hidden one-child waiter (`mj-cli/src/daemon/wait_exit.rs`, wired in `mj-cli/src/main.rs`):**
   - Hidden CLI command: `mj daemon-wait-exit --exit-dir <dir> -- <program> [args...]`.
   - Ignores `SIGINT`, `SIGTERM`, and `SIGHUP` on Unix before spawning, holds no locks, opens no sockets, and owns no workers.
   - Spawns `<program> [args...]` directly (inheriting stdio/environment; setting `arg0 = "mj"` when `<program>` ends in `mj` / `/proc/.../exe` and `args[0] == "daemon-run"` so `ps` remains `mj daemon-run`), records `child.id()`, and blocks in `child.wait()`.
   - When `child.wait()` returns: if the exit is clean (`exit_code == Some(0)` or `exit_code == Some(STORE_OWNED_EXIT_CODE)`), exit 0 without writing anything. Otherwise (any other non-zero exit code or terminating signal), atomically write `<exit_dir>/<child_pid>.json` (`0600`) with `DaemonExitRecord` (including Unix `signal()`, `core_dumped()`, and human signal name like `SIGKILL`/`SIGABRT`/`SIGSEGV`) and exit 0.
4. **Wrap outside `systemd-run --scope` (`mj-cli/src/daemon.rs`):** In `host_daemon_command`, build the exact existing command (whether `systemd-run --user --scope -- ... daemon-run` on Linux or direct `mj daemon-run`), then wrap it as `<this_exe> daemon-wait-exit --exit-dir <daemon_exits_dir> -- <existing_cmd...>`. Because `systemd-run --scope` execs `mj daemon-run` in place, the waiter's direct child PID IS the daemon's PID, and the waiter sits outside the scope so it survives scope-level kills. `spawn_detached` launches the waiter with the exact existing terminal/fd isolation; no PID handshake pipe is needed because the waiter exits immediately after its child exits (so `process_is_alive` on the spawned waiter tracks launch liveness cleanly) and `daemon.json` + `<pid>.json` both record the daemon's real PID.
   - On container launches (`container.rs:658`), wrap `exec_daemon` with `mj daemon-wait-exit --exit-dir /var/lib/mjolnir/data/daemon-exits -- /usr/local/bin/mj daemon-run` cleanly because launch always targets this build's image (`resolve_image` requires matching revision), and the M2 `top` matcher already treats the waiter as live for its brief startup and exit transitions around the daemon child.
5. **Consume and delete exit records in `mj-daemon/src/daemon/previous_daemon.rs`:**
   - In `report_unclean_previous_daemon` (called while holding `ControllerStoreGuard`), read all `*.json` files in `daemon_exits_dir()` (ignoring any unreadable/corrupt file with a `WARN` rather than panicking).
   - For each valid `DaemonExitRecord`: match against leftover `daemon.json` if `previous.pid == record.pid` (combining `previous_started_at`, `last_seen`, `host_boot`, `kept_logs`, `exit_code`, `signal`, `signal_name`, and `core_dumped` into one rich `WARN` line), or log a standalone `WARN` for exit records whose daemon died before publishing `daemon.json` or after the shutdown watchdog removed `daemon.json` (also renaming matching `mj-daemon-*-<pid>.log` files to crashed names!).
   - Delete each reported exit file from `daemon_exits_dir()` after logging so clean subsequent restarts never re-report it.
   - Preserve the existing behavior when `daemon.json` is left behind with no exit record (e.g., host reboot killed both waiter and daemon — `conclusion(last_seen, boot)` still identifies host restart!). Note: inside a container, `daemon.json` in the volume has the daemon's real in-namespace PID (`process.rs:835`), so `previous_daemon.rs` automatically reports container daemon crashes too!

## Concrete Steps

All commands run from `/workspace/0722abb7c6bef294c60d36cb388681f4/mjolnir` using `mbx` (`cargo` is the `mbx` launcher). Never set `CARGO_TARGET_DIR` or redirect build outputs into `/tmp`.

Milestone 1 validation:

    cargo test -p brokk-mj-client daemon::tests
    cargo test -p brokk-mjolnir --test daemon_startup
    cargo test -p brokk-mjolnir daemon::tests
    cargo clippy -p brokk-mj-client -p brokk-mjolnir --all-targets -- -D warnings
    cargo fmt --all -- --check

Milestone 2 validation:

    cargo test -p brokk-mj-client daemon::tests
    cargo test -p brokk-mjolnir --test daemon_startup
    cargo test -p brokk-mjolnir daemon::
    cargo clippy -p brokk-mj-client -p brokk-mjolnir --all-targets -- -D warnings
    cargo fmt --all -- --check

Milestone 3 validation:

    cargo test -p brokk-mj-core daemon_exit
    cargo test -p brokk-mj-daemon previous_daemon
    cargo test -p brokk-mjolnir --test daemon_startup
    cargo test -p brokk-mjolnir daemon::
    cargo clippy -p brokk-mj-core -p brokk-mj-client -p brokk-mj-daemon -p brokk-mjolnir --all-targets -- -D warnings
    cargo fmt --all -- --check

## Validation and Acceptance

Milestone 1 (#1568 Phase 1):
- Fake-peer tests in `mj-client/src/daemon.rs`, `mj-cli/src/daemon.rs`, and `mj-cli/tests/daemon_startup.rs` prove:
  1. A slow `Ping` lets `wait_for_ready_daemon` fire the slow-start notice and respect `START_TIMEOUT` without dropping a delayed `Pong` on the same exchange.
  2. A slow `UpgradeBlockers` response does not delay `acquire_start_lock` when the startup lock releases, and aborts on drop.
  3. `hand_off_daemon` transitions sequentially from `ask_for_handoff` to release wait and never re-asks `PrepareUpgrade` after post-ack expiry.
  4. `maintain_attachment` uses `request_single(Attach)` so `UpgradePending` publishes `Missing` immediately instead of looping as `Attached`, while keeping the 5-second display threshold.
  5. `legacy_placement(0)` never returns `Host`; corrupt `daemon.json` with a free host store guard recovers as `Absent` while a held guard reports an error naming `daemon.json`.

Milestone 2 (#1458 + #1568 Phase 2):
- Tolerant-parse fixture tests prove `DaemonMetadata` (`daemon.json`) keeps `deny_unknown_fields` compatibility for older readers while `ContainerDaemonRecord` (`daemon-container.json`) tolerates unknown future fields and fails closed (`Unknown`) when corrupt or unbound.
- Recorded Docker (`docker top <id> -o args`) and Podman (`podman top <id> args`) output fixture tests prove `top_output_has_daemon_run` matches `mj daemon-run`, `mj daemon-wait-exit`, and their pre-exec launch shell. It ignores sleeping PID 1 and live `hel` worker processes.
- Isolated fake-engine tests prove:
  1. Stopped or absent recorded container ID yields `DaemonRelease::Exited` and recovers cleanly.
  2. Running container with dead `daemon-run` and live worker processes yields `DaemonRelease::Exited` and relaunches in the same container without calling `engine stop`.
  3. Running container with silent endpoint (`Ping` hangs) and live `daemon-run` yields `DaemonRelease::StillOwned` and does not launch a second daemon (`249df74e4`).
  4. Engine failure / non-zero exit yields `DaemonRelease::Unknown` (failing closed, including in `processes_in` / `remove_idle_predecessors`).
  5. Launcher crash before `daemon.json` publication leaves `daemon-container.json`; the next client adopts the running container daemon, completes readiness wait, and publishes `daemon.json` (fixing Flaws F2 and F3).
  6. Legacy unreachable `pid: 0` with no `daemon-container.json` refuses visibly recommending `mj daemon stop --force`, and `mj daemon stop --force` clears the stale `daemon.json` only after confirming the endpoint does not answer.
  7. An opt-in real-Podman test (`#[ignore]` when Podman is unavailable) covers stopped container and running container with dead daemon + live process so it can be run on any Podman host.
  8. `wait_until_answering`, `wait_for_endpoint_down`, and the 3-second absence `Ping` in `recorded_daemon_is_gone` are completely deleted.

Milestone 3 (#1644):
- Isolated integration tests in `mj-cli/tests/daemon_startup.rs` and `mj-daemon/src/daemon/previous_daemon.rs` prove:
  1. Launching through `mj daemon-wait-exit` lets the launcher exit immediately while the daemon runs.
  2. Killing the daemon with `SIGKILL` (or exiting non-zero) writes `<data_dir>/daemon-exits/<pid>.json` with exact `signal = 9` / `exit_code`, and the waiter exits promptly with no lingering process.
  3. Starting the successor daemon logs `WARN` with `exit_code`/`signal`/`signal_name`/`core_dumped`, preserves crashed logs, and deletes the consumed exit file (including when the predecessor died before publishing `daemon.json` or after removing it).
  4. A clean exit (`0`) or lost-store-lock exit (`75`) writes no exit record and emits no crash warning.
  5. Tolerant-parse fixture test proves `DaemonExitRecord` ignores unknown fields from newer waiter builds.

## Idempotence and Recovery

- `daemon-container.json` and `daemon-exits/<pid>.json` are written atomically with `0600` permissions via `mj_core::config::atomic_write`.
- `daemon-container.json` is bound to `daemon.json` by `(address, token)`; an unbound or unreadable `daemon-container.json` is treated as "no record" (`Unknown`), never `Exited`.
- Consumed exit records in `daemon-exits/` are deleted once reported by the successor daemon, keeping the directory empty during healthy operation.

## Artifacts and Notes

- Original read-only reconnaissance report: `/workspace/0722abb7c6bef294c60d36cb388681f4/.mj-agents/677fbffb5f0f3a7fe1e4dd0bdedc2337/cluster3-analysis.md`
- Plan revision note (2026-11-23 02:40Z): Reshaped Milestones 1, 2, and 3 following MJ Advisor (`91233b0efdd193066e7b4eeb46c8db10`) review: replaced image-side `probe-store-lock` + launch tickets + `MJ_DAEMON_GENERATION` with engine `ps`+`top` + `daemon-container.json` (with token for adoption); trimmed M1 to `request_single`, two-phase handoff, 5s display threshold, abort-on-drop blockers, and `pid == 0` fix; simplified M3 to outer `mj daemon-wait-exit` with PID-keyed write-on-failure + delete-on-report exit files.

## Interfaces and Dependencies

In `mj-client/src/daemon.rs`:

    impl DaemonClient {
        pub async fn request_single(&mut self, action: DaemonAction) -> Result<DaemonReply>;
    }

    pub const CONTAINER_OWNER_SCHEMA: u32 = 1;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ContainerDaemonRecord {
        #[serde(default = "default_container_owner_schema")]
        pub schema: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub protocol_version: Option<u32>,
        pub runtime: mj_core::config::DaemonRuntime,
        pub container_id: String,
        pub container_name: String,
        pub address: SocketAddr,
        pub token: String,
        pub started_at: String,
    }

    pub fn container_owner_path() -> PathBuf;
    pub fn read_container_owner() -> Result<Option<ContainerDaemonRecord>>;
    pub fn write_container_owner(record: &ContainerDaemonRecord) -> Result<()>;
    pub fn remove_container_owner_if_matching(address: SocketAddr, token: &str) -> Result<()>;

In `mj-cli/src/daemon/container.rs`:

    pub(super) enum ContainerDaemonStatus {
        Running,
        Exited,
        Unknown(String),
    }

    pub(super) async fn probe_container_daemon(
        runtime: mj_core::config::DaemonRuntime,
        container_id: &str,
    ) -> ContainerDaemonStatus;

    pub(super) fn top_output_has_daemon_run(stdout: &str) -> bool;

In `mj-cli/src/daemon.rs`:

    enum DaemonRelease {
        StillOwned,
        Exited,
        Replaced,
        Unknown(String),
    }

    async fn probe_daemon_release(metadata: &DaemonMetadata) -> Result<DaemonRelease>;

In `mj-core/src/daemon_exit.rs` (shared by `mj-cli` and `mj-daemon`):

    pub const DAEMON_EXIT_SCHEMA: u32 = 1;
    pub const STORE_OWNED_EXIT_CODE: i32 = 75;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct DaemonExitRecord {
        #[serde(default = "default_daemon_exit_schema")]
        pub schema: u32,
        pub pid: u32,
        #[serde(default)]
        pub exited_at: String,
        #[serde(default)]
        pub exit_code: Option<i32>,
        #[serde(default)]
        pub signal: Option<i32>,
        #[serde(default)]
        pub signal_name: Option<String>,
        #[serde(default)]
        pub core_dumped: Option<bool>,
    }

    pub fn daemon_exits_dir() -> PathBuf;
    pub fn write_daemon_exit_record(exit_dir: &Path, record: &DaemonExitRecord) -> Result<PathBuf>;
