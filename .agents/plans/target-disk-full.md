# Check for a full disk before writing to a target, and say so plainly

This ExecPlan is maintained according to `.agents/PLANS.md`. It is a living document; keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective up to date.

## Purpose / Big Picture

On 2026-10-02 the root filesystem of the SSH bare host precision-3260 filled up. The SSH user had 0 bytes available. `df` showed about 23 GB free, but ext4 keeps that 5% reserve for root. Every Mjolnir worker on the host stopped with "No space left on device" and could not even write its exit record. The daemon's automatic recovery then uploaded a replacement worker binary again and again. Each failed `scp` left a truncated `hel.prepared-upgrade-stage-*.next` file, 39 of them in one session directory. The user saw only "unreachable".

After this change, the daemon has one owner for "this filesystem on a target is full", called the storage board. The board keeps one record per filesystem per host. The board learns from the daemon's capacity probe, which also measures free space, and from any target command that fails with "No space left on device". Large writes check the board first and are refused with a plain sentence. An example refusal:

    Cannot stage the replacement Mjolnir worker (98.2 MB): precision-3260 has 0 B free on / (the filesystem reserves 24.69 GB more for root). Mjolnir keeps 1.07 GB free for running sessions; free space on precision-3260 to continue.

Automatic recovery stops retrying and waits for the board to change. Every surface shows "disk full" instead of a bare "unreachable": the TUI row, the session menu, the Targets pane, `mj sessions`, the web viewer, and `mj doctor`. A failed staging upload removes its partial file. Each new staging also removes partial files that earlier failed attempts left behind.

## Progress

- [x] (2026-10-02) Mapped recovery, write paths, surfaces and the worker exit path.
- [x] (2026-10-02) Removed partial `hel.prepared-*` uploads, with a test (commit "Remove partial worker staging uploads instead of leaving them behind").
- [x] (2026-10-02) Added the classifier and storage view in `mj-core/src/targets/storage.rs`. The process executors now report full-disk failures.
- [x] (2026-10-02) Added the storage board in `mj-controller/src/target_storage.rs`. The capacity poller now runs as a daemon service (`mj-controller/src/daemon/capacity.rs`), and the web viewer subscribes to it.
- [x] (2026-10-02) Pre-write checks added. Recovery waits on the board and reads the dead worker's exit record.
- [x] (2026-10-02) The worker reserves its exit record and writes the reason in place.
- [x] (2026-10-02) Surfaces done: runtime feed `storage`, `ApiSession.storage_problem`, `ViewerSession.storage_problem`, `ViewerTargetCapacity.storage`, the TUI row, menu and Targets pane, `mj sessions`, and the doctor check.
- [x] (2026-10-02) Dev-profile tests, clippy and fmt pass. See the final report for the commands run.
- [x] (2026-10-02) Revised to one record per filesystem, after the user's decision. Each write checks the filesystem it lands on. Recovery waits only on the worker root's filesystem. A session is "Disk full" only when one of its own filesystems is full. The probe now also measures session project directories and clones, profile homes, the mbx cache and `/tmp`.

## Surprises & Discoveries

- Observation: the staging guard that deletes `hel.prepared-*` was built only after the upload succeeded. Every failed upload therefore leaked its partial `.next` file.
  Evidence: `stage_worker_binary_for_upgrade` called `plan.execute(executor)?` before constructing `PreparedWorkerBinary`.
- Observation: the daemon ran a capacity poller only inside the web viewer server, and only when `[phone].enabled` was set. Neither recovery nor uploads had a capacity owner they could ask.
- Observation: a dead worker's exit record stays on disk until a restart. If recovery took every record that mentions "No space left" as current, it would mark the host full again after space was freed. It would then never restart the worker.
  Resolution: a failure observed at time T is ignored once a measurement taken at least 15 seconds after T exists (`observe_no_space_at`).

## Decision Log

- Decision (superseded): key the board by host and keep one number for the tightest filesystem.
  Date/Author: 2026-10-02, fix agent.
- Decision (user, overrides the one above): keep one record per filesystem per host, keyed by the mount point `df` reports.
  - Each record lists the measured paths that live on it. A write belongs to the filesystem of the longest measured path that contains it; relative and `~/` paths are first resolved against the home directory the probe prints.
  - What each write checks:
    - Worker staging, replacement and install: the worker root.
    - Harness preparation: the harness cache `~/.cache/mjolnir/harnesses`, the staged profile home and the worker root.
    - Checkpoint staging: the worker root.
    - Restore: the worker root, the workspace (the project directory or managed clone) and the harness home, sized by the archive.
    - Move: keeps its own fresher `df` check.
  - Recovery waits only when the worker root's filesystem is full.
  - A session shows "Disk full" only when its worker root, workspace, staged profile home or `/tmp` is on a full filesystem.
  - A no-space failure marks the filesystem of the path it names. The path is taken from scp's `write remote "PATH"`, from `cp: error writing 'PATH'`, or from a `/path:` prefix. A failure that names no path holds every filesystem on the host until the measurement it requested places it.
  Date/Author: 2026-10-02, user via coordinator.
- Decision: a write needs `available >= size + 1 GiB`. "Full" means less than 1 GiB available, or a no-space failure that no later measurement has replaced. "Low" means less than 5 GiB and only shows a warning.
  Rationale: the reserve protects the journals and logs of the sessions already running there, and those are what failed in the incident. A percentage margin is unreasonable on large disks: 2% of 4 TB is 80 GB.
  Date/Author: 2026-10-02, fix agent.
- Decision: a host the board has not measured is not refused.
  Rationale: an unknown is not a failure. A write that then fails is still classified, marks the host full and starts a new measurement.
  Date/Author: 2026-10-02, fix agent.
- Decision: no `ViewError` variant was added. The unreachable detail starts with "disk full: …", and surfaces read the board through the runtime feed.
  Rationale: a new enum variant would break older clients that deserialize `ViewError`. The board stays the only judge.
  Date/Author: 2026-10-02, fix agent.
- Decision: the worker writes `worker-exit.json` as `null` padded to 16 KiB after durable recovery, and overwrites it in place at exit.
  Rationale: the existing probe already reads `null` as "no exit record". An in-place overwrite needs no new blocks on ext4 or XFS.
  Date/Author: 2026-10-02, fix agent.

## Outcomes & Retrospective

All five requested behaviors are implemented and tested. The remaining gaps:

- With the web viewer disabled and a TUI attached, each host is probed twice per 30 seconds: once by the daemon service and once by the TUI's own CPU and RAM poller. The TUI should read capacity from the daemon feed too. That is a follow-up.
- On a copy-on-write filesystem (btrfs, ZFS) the in-place exit record can still fail.
- Repository clones have no pre-write check. Their size is unknown and the locator is not in scope there. A failure is still classified.
- The mbx cache is measured at its default `~/.cache/mbx`, and at a container target's `build_cache.directory` when one is set. A machine-level cache directory that `mbx` resolves at run time is not measured.
- Rootful Podman storage over SSH (`/var/lib/containers`) is not measured. Only rootless storage under the user's home is.

## Context and Orientation

- `mj-core/src/targets/storage.rs`: `reports_no_space` (the classifier), `STORAGE_PROBE_SCRIPT` (POSIX `df -Pk` at the nearest existing ancestor of each path), `parse_storage_lines`, `TargetStorageView::{evaluate, explanation, refuse_write}`, and the hook that executors call through `observe_command_output`.
- `mj-controller/src/target_storage.rs`: the board, with `record_samples`, `observe_no_space[_at]`, `ensure_room`, `full`, `session_problem` and `subscribe`.
- `mj-controller/src/daemon/capacity.rs`: the daemon capacity service. It feeds the board and broadcasts readings to the web viewer.
- `mj-controller/src/session_manager/actor.rs` and `recovery.rs`: the recovery wait and `WorkerRecoveryOutcome::StorageFull`.
- `mj-worker/src/exit_record.rs`: the reserved exit record.

## Validation and Acceptance

Run these from the repository root on the dev profile, outside the sandbox:

    cargo test -p brokk-mj-core --lib storage
    cargo test -p brokk-mj-worker --lib exit_record
    cargo test -p brokk-mj-controller --lib -- target_storage storage_check full_disk relay_actor_waits_for_disk_space worker_staging host_capacity_probe embedded_viewer_says
    cargo test -p brokk-mj-tui --lib -- disk_full full_disk
    cargo test -p brokk-mjolnir -- full_disk

`relay_actor_waits_for_disk_space_then_recovers_its_worker` is the end-to-end check. The relay actor publishes "disk full: …" and does not restart the worker while the board says full. Once a measurement with room is recorded, it reconnects within seconds.
