# Add `mj migrate` for Minimal-Downtime Host-to-Host Daemon Migration

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This repository maintains its execution-plan standard in `.agents/PLANS.md`. Maintain this document in accordance with `.agents/PLANS.md` so a reader with only the current working tree, `.agents/PLANS.md`, and this file can execute every milestone end to end.


## Purpose / Big Picture

Today a Mjolnir daemon lives permanently on the machine where its store was created. Moving a daemon with tens of gigabytes of transcript history (`mj.sqlite3`, `tool-details/`, `sessions/`, and a multi-gigabyte shared SessionWiki index), harness profiles (both managed and unmanaged), EC2 keys, external token files, and live sessions onto a new machine requires manual copying, error-prone SQL and TOML editing, and long downtime, and cannot represent live local-bare sessions on the old machine over SSH without risking deletion of the user's project directories.

After this plan is implemented, a user can run `mj migrate <destination>` on the old machine (host A) to move the daemon to a new Unix machine (host B) with minimal downtime while keeping history intact and sessions reachable. The old daemon stops permanently and stays fenced against accidental restarts (multi-daemon is not a goal). Specifically, a user will be able to:

1. Run `mj migrate <dest> --dry-run` to check preconditions, SSH connectivity (A-to-B, B-to-A when keeping local sessions, and effective B-to-third-host SSH resolution), EC2 security-group ingress for B, disk space, B destination store state, SessionWiki state, unknown files, external dependencies (`[github].session_token_path`, `github.app.private_key_path`, unmanaged profile homes, `phone.tls_cert`/`tls_key`, `repositories[].local`, `{ from_env = ... }` secrets, AWS profiles), and placements with missing `machine_id`, without modifying either machine.
2. Run `mj migrate <dest> --source-host <stable-ssh-name-from-B>` to pre-sync almost all state (including ~28 GiB `mj.sqlite3`, ~17 GiB shared SessionWiki index, `sessions/`, `tool-details/`, `projects/`, `config/`, unmanaged profile login files, and external token files) while host A's daemon keeps running with zero downtime, then perform a brief atomic cutover (fencing A, transferring only the pinned SQLite WALs and changed small files, publishing into B while fenced, clearing B's fence as a single SQLite commit under B's daemon locks, starting B's daemon, and finally detaching Codex `auth.json` symlinks on A).
3. Keep live and historical `LocalBare` sessions on host A reachable from host B over SSH through a typed `SshHostBare { host, data_root }` locator that derives A's exact worker root and nested profile root via `DataLayout::at(data_root)`, structurally cannot delete a user checkout, probes profile-home ownership live over SSH (failing closed to shared), and unlinks whole-directory personal-home symlinks cleanly on restage.
4. Keep `LocalPodman`, `LocalDocker`, and `AppleContainer` sessions on host A reachable as `SshPodman`, `SshDocker`, and `SshAppleContainer`, while leaving existing SSH sessions already hosted on B as SSH-to-self and letting new sessions on B use `local`.
5. Defer `retired` machine support to a follow-up (`--disconnect-local` uses a normal SSH machine entry and existing best-effort destroy).
6. Refuse cleanly in preflight if host B already has an `mj.sqlite3` or `config.toml` (directing the user to clean B manually or pass `--dest-instance <name>`), or if any copied unmanaged profile home / external token path already exists under B's `$HOME`.
7. Resume an interrupted Phase 1 pre-sync or interrupted Phase 2 publication cleanly without starting over, and automatically roll back host A to running (with zero modifications on A) if a Phase 2 failure occurs before B commits its activation.

Scope for v1: native host-to-host Unix daemons within the same OS family (container-hosted daemons recorded via `daemon-container.json` are refused cleanly with an explanation). On default-instance migrations, the shared SessionWiki database (`<dirs::data_dir()>/sessionwiki/index.db`) is migrated via the same separate-process pin-plus-WAL protocol as `mj.sqlite3` unless skipped; writes made to A's shared SessionWiki by standalone `sessionwiki` or other instances after cutover are not carried.


## Progress

- [x] (2026-11-26 16:22Z) Read prep handoff and `.agents/plans/daemon-migration-prep.md`; verified all four prep milestones are on `origin/master` (`d098d793a`, `SCHEMA_VERSION = 162`, `CONFIG_VERSION = 24`).
- [x] (2026-11-26 16:34Z) Completed initial three-track read-only reconnaissance (`locator-design.md`, `state-rewrite-inventory.md`, `transport-and-ops.md`).
- [x] (2026-11-26 17:07Z) Confirmed user design decisions: add typed `SshHostBare` locator; keep existing B-hosted SSH sessions as SSH-to-self; native host-to-host daemons for v1 plus migrating shared SessionWiki on default-instance moves.
- [x] (2026-11-26 17:13Z) Completed deep-dive read-only spec checks (`ssh-host-bare-spec.md`, `rewrite-and-transfer-spec.md`).
- [x] (2026-11-26 17:18Z) Authored initial ExecPlan (`9b52676eb`), started Milestone 1 implementation, and requested MJ Advisor review.
- [x] (2026-11-26 18:15Z) Received MJ Advisor's complete two-part review (including live SQLite lock-loss experiments and live-store measurements across 5 read-only reports); sent Milestone 1 revision instructions incorporating Advisor's simplifications and defect fixes.
- [x] (2026-11-26 18:35Z) Confirmed user policy decisions for Milestones 2–4: copy referenced unmanaged profile homes and token/key files to the same relative paths under B's `$HOME` (refusing if any path already exists on B); transfer shared SessionWiki via the same separate-process pin + WAL protocol as `mj.sqlite3`; defer `retired` machine support to a follow-up; always refuse if B already has an `mj.sqlite3` or `config.toml` (suggesting manual cleanup or `--dest-instance`).
- [x] (2026-11-26 18:55Z) Milestone 1: `SshHostBare { host, data_root }` locator, schema revision 163 (one `data_root` column, drop evolving `CHECK` constraints, preserve 3-column index & `runtime_json` last, validate on write), live SSH profile-home check + unlink-on-restage, wildcard-site audit, protocol bump, and 21 targeted tests passing + workspace clippy clean.
- [x] (2026-11-26 22:22Z) Milestone 2: Layout classification cleanup (`attachments.lock`/`deleted`, `backups`, `moves`), `session_memory_keys` sidecar (rev 164, breaking/floor 164), semantic preflight inspector (complete unmanaged `$HOME` profile homes, EC2 ingress & B-side SSH checks, non-transferred trees not traversed), fenced-snapshot memory-key capture (skipping sessions with no legacy identity), atomic store/config/SessionMoves/ProjectMemory/EC2/token rewriter (preserving historical discovery target kinds and legacy container template fallback), post-commit Codex detachment, and focused tests + workspace clippy clean.
- [ ] Milestone 3: Separate-process SQLite WAL pin (`mj migrate-pin`), simplified 3-frame (`PutFile`, live-only `PatchBlocks`, `RemoveFile`) zstd streaming transfer engine for both `mj.sqlite3` and SessionWiki, single-commit fenced publication under B's daemon locks (refusing any existing B `mj.sqlite3`/`config.toml`), resumable/re-runnable receiver, and separate-process WAL/pipe tests.
- [ ] Milestone 4: `mj migrate` CLI and hidden `mj migrate-receive` / `mj migrate-pin` subcommands, progress reporting, and real-binary localhost-`sshd` end-to-end tests (including crash-window, competing B autostarter, terminal Move + waiting worker-restart intent, existing-B refusal, and rollback tests).


## Surprises & Discoveries

- Observation: **POSIX `fcntl` lock loss on same-process raw file I/O (confirmed experimentally by MJ Advisor).** Opening and closing `mj.sqlite3` via ordinary file I/O in the same OS process that holds a SQLite WAL read transaction (`R_pin`) silently drops all POSIX `fcntl` byte-range locks held by that process on `mj.sqlite3`. When the separate daemon process closes its last SQLite connection, SQLite checkpoints and deletes `-wal` and `-shm` while `R_pin` is still open; a subsequent fence commit in the pin process returns success into an all-zero-header WAL and vanishes on fresh open while `PRAGMA integrity_check` still reports `ok`.
  Evidence: Advisor report `bb29c01c` (`experiments.py` E1/E2 on SQLite 3.46.1; bundled SQLite 3.54.1 `libsqlite3-sys-0.40.0/sqlite3/sqlite3.c:42995-43020` defers close only for SQLite-managed descriptors). Structural fix: dedicate a separate child process (`mj migrate-pin`) that touches `mj.sqlite3` (and separately `sessionwiki/index.db`) ONLY through SQLite; the streaming sender process performs raw byte reads and NEVER holds SQLite locks on the pinned database.

- Observation: **With a valid separate-process pin, a torn live base copy plus the complete same-generation WAL recovers exact source state without a Phase 2 base-file delta (confirmed experimentally in 24/24 trials, 12 demonstrably torn).** Every page modified in the main DB file during streaming also has its frame in that WAL generation.
  Evidence: Advisor report `bb29c01c` (E3). This eliminates the entire Phase 2 `stat` fast path, in-memory block-hash vector, Phase 2 `PatchBlocks`, and post-Phase-1 convergence check. Conversely, a stale base from a prior interrupted run + a newer WAL with the fence row still passes both the fence check and `integrity_check` (E4/`fence_proof.py`), so resumed runs must always verify current source blocks against the current pin.

- Observation: **Storing two raw paths (`worker_root`, `nested_profile_root`) in `SshHostBare` fails to prove checkout disjointness and can drift from `DataLayout`.** A managed clone at `<project>/.mj/clones/<sid>` passes an absolute/no-`..`/ends-with-`sid` check. On the live store, all 286 `local-bare` rows in `session_targets` and all 21,685 in `environment_workers` equal `<data_root>/workers/<session_id>` exactly.
  Evidence: Advisor Milestone 1 review item 1. Storing `SshHostBare { host, data_root }` (one `data_root BLOB` column, `workspace IS NULL`) and deriving both `<data_root>/workers/<id>` and `<data_root>/profiles/<id>` via `DataLayout::at(data_root)` makes checkout deletion structurally impossible and simplifies child `borrowed_locator` to cloning `data_root`.

- Observation: **Storing `shared_profile_home` in the locator creates mutable state in an immutable identity record.** Restaging a profile (`install.rs:1217-1231`) unlinks a whole-directory link and installs an owned directory; a stored flag goes stale (silently blocking sync forever if stale `true`, or writing into a personal home if stale `false`, since `refuse_symlinked_token` in `credentials.rs:170-175` checks only the final path component).
  Evidence: Advisor Milestone 1 review items 2 & 10, report `3ab8bf97`. Removing `shared_profile_home` from the locator/schema and instead probing A live over SSH on attach (cached per attachment, failing closed to shared) plus unlinking whole-directory symlinks on `SshHostBare` restage eliminates both stale flags and the parked-child profile-switch refusal.

- Observation: **`environment_workers_environment` is a 3-column covering index `(environment_id, worker_session_id, lease_id)` (`schema.rs:3352-3353`), NOT `(environment_id)` alone, and `retired_harnesses.rs:214-223` + revision-122 `read_targets` also decode target rows.**
  Evidence: Advisor report `3ab8bf97` §A.1–A.2. Revision 163 must recreate the exact 3-column index, keep `runtime_json` LAST on `environment_workers_v163`, support pre-163 projections in all three readers (`state_io.rs`, `environments.rs`, `retired_harnesses.rs`), validate on write in `insert_target`/`store_worker`, and update stamped-162 test fixtures.

- Observation: **Wildcard / `matches!` / `!matches!` branches on `TargetLocator` silently take the wrong side for `SshHostBare`.** Key dangerous sites include `controller/environment.rs:221-255` (parent teardown skips child roots), `controller/environment.rs:827-855` (mount paths checked on B's local disk), `daemon/restart.rs:399-400` (bare restart eligibility), `server_runtime/api/fetch_transfer.rs:9-13` (missing SSH target), `checkpoint/latched.rs:173-181` (reads launch home locally), `checkpoint/leftovers.rs:124` (B-local sweep), `checkpoint_transfer.rs:540-567` (rejects spaces/Unicode in paths), and `mj-build-cache/src/mbx/release.rs:262,517` (`mbx clean --under`).
  Evidence: Advisor report `779716e9` (70-site table).

- Observation: **Live-store measurements invalidate naive preconditions, SessionWiki backup, `AppendTail`, unmanaged-profile warnings, and `Unknown` classification.**
  Evidence: Advisor reports `1b721309` and `38f04841`:
  - `session_moves` has 84 terminal rows (79 completed, 5 failed) that linger indefinitely; `worker_restart_intents` can sit in `awaiting_readiness`/`swapping` while a worker is busy; `environments` has 2,795 `reclaimed` rows. Preconditions must check active ownership/recovery predicates, not row existence!
  - `session_moves.operation_json` holds controller-side archive paths (`checkpoint.archive_path`, `recovery_session.checkpoint.archive_path`, which follow the store) AND source-target paths (`source_snapshot.archive_path`, which stays on source).
  - Shared SessionWiki is **16.65 GiB** (17.88 GB) — a downtime `rusqlite::backup` would take minutes and require 16.65 GiB temporary disk twice. Using the same pin-plus-WAL method as `mj.sqlite3` gives seconds-long cutover and zero temporary file.
  - `tool-details/` is only 1.44 GB (8,345 files, p99 = 2 MB, max = 12.7 MB) and files can be deleted and recreated (`tool_details.rs:201,223`). Resending changed files whole via `PutFile` is simpler and safer than `AppendTail`.
  - All 23 profiles on the live host have unmanaged homes (`~/.claude*`, `~/.codex*`, etc.), plus `[github].session_token_path` (`~/.secrets/gh_agents_pat`). Warning alone leaves B with zero working logins!
  - `sessions/<id>/attachments.lock` (64,024 files) and `attachments.deleted` (64,009 files) are currently `Unknown` in `layout.rs` (128,033 files!), and `backups` (10.35 GB) + `moves` (683 MB) are also `Unknown`.
  - `mj ec2` security groups only admit A's `/32` public IP (`mj-cli/src/ec2.rs:1153-1198`), blocking B unless B's IP is added.
  - Clearing B's fence in staging before moving directories leaves a race where a B client sees a missing or half-published unfenced store (`store_fence.rs:125-127`). Publishing while fenced under B's `daemon-start.lock` + `controller.lock` and clearing the fence LAST in the final store creates one atomic activation commit point.
  - Project-memory keys change not only for raw `LocalBare`, but also for container sessions without `session.project` (whose `target_template_id` changes) and managed-worktree sessions when canonicalization differs; Milestone 1 must NOT edit `for_legacy_session` (which would change keys on upgrade for existing SSH worktree sessions).


## Decision Log

- Decision: Simplify `SshHostBare` to `{ host, data_root }` (persisted) and `{ ssh, data_root }` (backend), deriving both `<data_root>/workers/<id>` and `<data_root>/profiles/<id>` via `DataLayout::at(data_root)`, and drop `shared_profile_home` from the locator and schema.
  Rationale: Adopted from MJ Advisor Milestone 1 review items 1–2. Makes deleting a user checkout structurally impossible, reduces revision 163 to one new `data_root BLOB` column (`workspace IS NULL` for `ssh-host-bare`), eliminates locator drift between `session_targets` and `environment_workers`, and avoids stale filesystem flags by probing A live over SSH on attach (failing closed to shared) while unlinking whole-dir links cleanly on restage (`install.rs`).
  Date/Author: 2026-11-26 / Coordinator (on Advisor review).

- Decision: Keep existing SSH sessions already hosted on destination host B as SSH-to-self by default.
  Rationale: Approved by the user. `SshBare` -> `LocalBare` loses owned-workspace teardown and changes profile layout/skills scope; container SSH targets on B may use a different Unix user or rootless engine than B's daemon. New sessions on B use `local`.
  Date/Author: 2026-11-26 / User & Coordinator.

- Decision: Scope v1 to native host-to-host Unix daemons (refusing container-hosted daemons cleanly) and migrate shared SessionWiki (`<dirs::data_dir()>/sessionwiki/index.db`) on default-instance moves using the same separate-process pin-plus-WAL protocol as `mj.sqlite3` (unless `--skip-sessionwiki`; named `--instance` moves skip external SessionWiki unless `--include-sessionwiki`). State plainly that writes made by standalone `sessionwiki` on A after cutover are not carried.
  Rationale: User approved native-only + shared SessionWiki; user accepted recommended pin-plus-WAL transfer after Advisor measured 16.65 GiB SessionWiki index. Gives one consistent, zero-temp-file, low-downtime mechanism for both SQLite databases.
  Date/Author: 2026-11-26 / Coordinator (on Advisor review; default accepted by user).

- Decision: Put SQLite WAL pinning in a dedicated child process (`mj migrate-pin`) that touches `mj.sqlite3` (and separately `sessionwiki/index.db`) ONLY through SQLite; the sender process streams raw file bytes and NEVER opens SQLite on the pinned store. Cut the Phase 2 `stat` fast path, in-memory block-hash vector, Phase 2 `PatchBlocks`, post-Phase-1 convergence check, and `AppendTail`.
  Rationale: Adopted from MJ Advisor SQLite experiments (E1–E4). Eliminates POSIX `fcntl` lock loss structurally and simplifies the streaming protocol to just three data frames: `PutFile`, live-only `PatchBlocks` (for warm base + short pin window on the two SQLite DBs), and `RemoveFile`.
  Date/Author: 2026-11-26 / Coordinator (on Advisor review).

- Decision: Copy complete referenced unmanaged profile homes (plus external XDG `nested_data_dir` when outside `profile.home`) and referenced external token/key files (`[github].session_token_path`, `github.app.private_key_path`) under A's `$HOME` to the **same relative paths under B's `$HOME`** (rewriting the `$HOME` prefix in B's `config.toml` when `$HOME` differs), refusing in preflight if any target path already exists on B. Report unmanaged paths outside `$HOME` (e.g. `/mnt/optane/...`), host-local `phone.tls_cert`/`tls_key`, `repositories[].local`, `go.json` paths, `SSH_AUTH_SOCK`, and env paths in preflight (refusing missing out-of-`$HOME` profile/token paths unless already present on B).
  Rationale: Chosen by the user. Preserves the user's familiar `$HOME/.claude*`, `$HOME/.codex*`, and `$HOME/.secrets/...` layout on B while preventing accidental clobbering of existing files on B.
  Date/Author: 2026-11-26 / User & Coordinator.

- Decision: Defer `retired` machine support to a follow-up. `--disconnect-local` registers A as a normal SSH machine entry and relies on existing best-effort destroy cleanup.
  Rationale: Chosen by the user to keep v1 focused and avoid extra config/lifecycle machinery.
  Date/Author: 2026-11-26 / User & Coordinator.

- Decision: Always refuse in preflight if destination host B already has an `mj.sqlite3` or `config.toml` (directing the user to clean B manually or pass `--dest-instance <name>`).
  Rationale: Chosen by the user. Eliminates destination-replacement heuristics and accidental overwrites; note that resuming an interrupted `mj migrate` whose fenced publication marker exists in staging is still permitted.
  Date/Author: 2026-11-26 / User & Coordinator.

- Decision: Make B's activation ONE commit point under B's `daemon-start.lock` and `controller.lock` (held on stable inodes, never replacing the directory holding a held lock): publish config, profiles, secrets, SessionWiki, and data entries while B's `mj.sqlite3` is STILL FENCED (placing the fenced `mj.sqlite3` last), then clear `store_fence` in B's final store in one SQLite commit, release `controller.lock`, and start B's daemon under the held `daemon-start.lock`. Make the receiver re-runnable after crash. Move Codex `auth.json` symlink detachment on A strictly AFTER B's commit succeeds.
  Rationale: Adopted from Advisor review items L, N, and Milestone 1 item 11. Eliminates the half-published unfenced startup race on B and makes rollback before B's commit leave host A 100% unmodified.
  Date/Author: 2026-11-26 / Coordinator (on Advisor review).

- Decision: Preserve legacy project-memory identities by computing each affected session's pre-migration identity on A (for every session without `session.project` whose locator, template ID, or managed-worktree canonicalization would change its key) and persisting an explicit `project_memory_key` / pinned identity column (or `project_json`) on the session rather than modifying `for_legacy_session` in Milestone 1 or copying project directories blindly.
  Rationale: Adopted from Advisor Milestone 1 item 3. Avoids perturbing existing installs on upgrade, handles container sessions without `session.project` as well as raw `LocalBare` and managed-worktree sessions, and keeps running A workers' replicas merging against the exact same canonical directory on B.
  Date/Author: 2026-11-26 / Coordinator (on Advisor review).

- Decision: Advance `SCHEMA_VERSION` to 163 (breaking, floor 163) for `SshHostBare`'s `data_root` column and removal of kind-dependent SQLite `CHECK` constraints, and bump `mj_client::daemon::PROTOCOL_VERSION` by 1 (covering Rust daemon clients; worker relay does not carry `TargetLocator`, direct HTTP/web callers receive `SessionRecord.target`, and the Go port is gated by schema floor 163). Keep `HarnessRuntimePolicy::Managed` for `SshHostBare` (restarting a migrated host-bare session switches it to the managed harness install on A, matching SSH bare).
  Rationale: Verified against compatibility boundaries in Advisor report `779716e9`.
  Date/Author: 2026-11-26 / Coordinator (on Advisor review).

- Decision: Keep all migration code inside existing crates (`mj-core`, `mj-store`, `mj-targets`, `mj-worker-launch`, `mj-worker-staging`, `mj-session-runtime`, `mj-controller`, `mj-daemon`, `mj-client`, `mj-cli`); do not create a new workspace crate.
  Rationale: Follows `AGENTS.md` crate-creation guidance and respects existing crate boundaries.
  Date/Author: 2026-11-26 / Coordinator.


## Outcomes & Retrospective

Not yet completed. Will be updated after each milestone.


## Context and Orientation

Mjolnir splits its architecture into a control plane (the daemon `mj daemon-run`, backed by SQLite store `mj.sqlite3` and `config.toml`) and a data plane (one `mj-worker` process per session, binding `<worker_root>/control.sock`). Workers never initiate connections to the daemon; the daemon always dials outward via local exec, container exec, or SSH (`hel worker proxy --root <worker_root>`). Workers keep their own relay journals and pending turns across daemon downtime, so stopping daemon A and starting daemon B with the copied and retargeted store lets B reconnect to running workers automatically.

Prep work already on `origin/master` (`.agents/plans/daemon-migration-prep.md`) provides:

1. Path manifest (`mj-core/src/config/layout.rs`): `DataLayout` and `ConfigLayout` build every path under `data_dir()` and `config_dir()`, enforced by a `clippy.toml` `disallowed_methods` ban. `classify_data_entry` and `classify_config_entry` classify entries as `Known(MigrateAction)` (`Copy`, `KeepWithLocalWorkers`, `Recreate`, `Skip`, `CopyOptional`), `NestedStore` (`instances/<name>`), or `Unknown` (copy and report).
2. Durable ownership identity and store fence (schema revision 162): `<data_dir>/instance-identity` holds the worker ownership stamp copied unchanged to B. `mj-store/src/database/store_fence.rs` manages the singleton `store_fence` table checked in `schema.rs::open_writable` before schema migration runs. `mj daemon fence --to <dest>` (`mj-cli/src/daemon.rs:2417-2454`) holds `daemon-start.lock`, stops the daemon, acquires `ControllerStoreGuard` (`controller.lock`), and writes the fence atomically. Clients check the fence before spawning.
3. Relative checkpoint archive paths (schema revision 160): managed archives in `session_checkpoints.archive_path` are stored as bare filenames relative to `<db_parent>/sessions/`.
4. Placement machine identity (schema revision 161): `session_targets.machine_id` and `environment_workers.machine_id` carry a validated `MachineId` (`mj-core/src/config/machines.rs`), backfilled `'local'` for local kinds and configured machine IDs for SSH rows where the template proved host/user/port.

Key modules and terms:

- Persisted `TargetLocator` (`mj-core/src/state.rs:963-1047`, serde `kebab-case`) and backend `TargetLocator` (`mj-core/src/targets.rs:4164-4248`, serde `snake_case`, built in `mj-core/src/targets/convert.rs`).
- `TargetRuntimeSettings` (`mj-core/src/state/target_runtime.rs`): frozen per-session/per-lease snapshot with `kind: String` and `TargetConnection`.
- `ManagedWorktree` / `ManagedWorktreeTarget` (`mj-core/src/state.rs:1049-1235`): explicit record of a session-owned git clone or worktree (`Local` or `Ssh { destination, ssh_args }`).
- `ControllerStoreGuard` (`mj-controller/src/controller.rs:161-254`) and `DaemonStartGuard` (`mj-cli/src/daemon.rs:193-220`).
- `SupervisedChild` (`mj-core/src/subprocess.rs:247-320`): process-group-managed async child wrapper.
- `SessionWiki`: separate WAL-mode SQLite index (`index.db`) holding archived transcripts, notes, tags, and summaries, resolved by default to `<dirs::data_dir()>/sessionwiki/index.db` (`mj-core/src/config/loading.rs:250-295`).

`go/` is out of scope (`AGENTS.md`). Never touch the host's default instance store during testing; use isolated temporary directories or named `--instance <test-name>` and stop test daemons afterward. Run all `cargo test` commands outside the restricted sandbox.


## Plan of Work

Four sequential milestones. Validate, run `request_review`, and commit each milestone before starting the next.


### Milestone 1: `SshHostBare { host, data_root }` locator, schema revision 163, live profile probe, and protocol bump

Goal: represent a host-local-bare session on host A reached over SSH from host B with one immutable data root, zero checkout-deletion risk, live fail-closed shared-profile detection + unlink-on-restage, Rust-validated schema revision 163, exhaustive wildcard-site auditing, and client protocol bump.

1. Core persisted and backend locators (`mj-core`):
   - In `mj-core/src/state.rs`, add `TargetLocator::SshHostBare { host: String, data_root: PathBuf }` (`kebab-case` -> `"ssh-host-bare"`).
     - `verify_locator`: `host` non-empty and not starting with `-`; `data_root` absolute (`mj_core::target_path::is_absolute`), no `Component::ParentDir` (`..`), not filesystem root (at least one `Component::Normal`); `session_id` non-empty single normal component.
     - `skills_scope()`: `SkillsScope::Isolated`.
     - `same_resource`: two `SshHostBare` locators match iff `host == other_host && data_root == other_data_root`.
   - In `mj-core/src/targets.rs`, add backend `TargetLocator::SshHostBare { ssh: SshTarget, data_root: PathBuf }` (`snake_case` -> `"ssh_host_bare"`).
     - `worker_root(session_id)` derives `DataLayout::at(data_root).worker_root(session_id)` (`<data_root>/workers/<session_id>`).
     - `locator_command` wraps remote commands via `ssh_command(class, ssh, ...)`.
   - In `mj-core/src/targets/convert.rs`, map `"ssh-host-bare"` in `locator_kind_name` and `RecordedTarget` conversion (requiring `runtime.kind == "ssh-host-bare"` and `TargetConnection::Ssh { ssh }` with `ssh.host == host`).
   - In `mj-core/src/targets/ssh.rs`, validate `SshHostBare` (`ssh.destination`, valid UTF-8 `data_root`, absolute, no `..`, not `/`, valid `session_id`); `is_borrowed()` returns `false`.
   - Do NOT modify `mj-core/src/project_memory.rs::for_legacy_session` in Milestone 1 (legacy project-memory continuity is persisted explicitly in Milestone 2 so existing installs are untouched on upgrade).

2. SQLite schema revision 163 and store encoding (`mj-store`):
   - Bump `SCHEMA_VERSION` from 162 to 163 (`mj-store/src/database.rs`) and raise `MINIMUM_COMPATIBLE_VERSION` in `schema.rs:10` to 163.
   - Add migration 163 (`migrate_host_bare_target_kind`, breaking, floor 163) in `mj-store/src/database/schema.rs`:
     - Use an RAII guard turning `PRAGMA foreign_keys = OFF` outside the transaction and unconditionally restoring `PRAGMA foreign_keys = ON` on drop (declared before `Transaction::new_unchecked(..., Immediate)` so the transaction rolls back before drop restores foreign keys).
     - Rebuild `session_targets` and `environment_workers` dropping BOTH kind-dependent `CHECK` constraints (`CHECK(kind IN ...)` and the kind-shape disjunction), keeping PK, FKs, `STRICT`, `CHECK(runtime_json IS NULL OR json_valid(runtime_json))`, `machine_id`, and adding ONE new column `data_root BLOB` to both tables.
     - Keep `runtime_json` LAST on `environment_workers_v163` (`machine_id TEXT, data_root BLOB, runtime_json TEXT ...`), per `.agents/docs/schema-migrations.md`.
     - Recreate BOTH indexes with their exact original columns: `environment_worker_identity ON environment_workers(worker_session_id)` and `environment_workers_environment ON environment_workers(environment_id, worker_session_id, lease_id)`.
     - Verify `!has_foreign_key_violation_for_tables(&transaction, &["session_targets", "environment_workers"])?`.
     - Update `schema_compatibility.minimum_compatible_version = 163`, insert revision 163 into `schema_migrations`, and set `user_version = 163` in the same transaction.
   - In `mj-store/src/database/state_io.rs`, `environments.rs`, and `retired_harnesses.rs`:
     - Encode `SshHostBare` as `kind = "ssh-host-bare"`, `host = Some(host)`, `data_root = Some(path_to_blob(data_root))`, with `workspace = NULL` and all other locator columns `NULL`. For every other kind, `data_root = NULL`.
     - Validate target rows on WRITE (`insert_target` and `store_worker`) and replace every `unwrap()`/`unreachable!` in `decode_target` with fallible Rust errors naming the session or lease ID.
     - Preserve pre-163 column projections (`NULL` for `data_root` when column absent) in `state_io::read_targets`, `environments.rs`, and `retired_harnesses.rs` so revision-122 backfills and historical upgrade fixtures pass cleanly.
     - Add `"ssh-host-bare"` to `REMOTE_TARGET_KINDS`.
     - Update stamped-162 test fixtures (`schema.rs`, `mj-cli/tests/daemon_startup.rs`) so they construct genuine 162 schemas (with old checks and without `data_root`) that upgrade cleanly to 163.

3. Target operations, live shared-home probe, safe restage, and wildcard-site audit (`mj-targets`, `mj-worker-launch`, `mj-worker-staging`, `mj-session-runtime`, `mj-controller`, `mj-daemon`, `mj-build-cache`, `mj-server`, `mj-client`, etc.):
   - `mj-targets/src/targets/profile_home.rs`: derive non-nested `<data_root>/workers/<id>/profile` and nested `<data_root>/profiles/<id>/<harness-id>` via `DataLayout::at(data_root)`.
   - `mj-targets/src/targets/cleanup.rs`: stop worker over SSH, re-validate `data_root` and `session_id`, and run `rm -rf -- <data_root>/workers/<id> <data_root>/profiles/<id>` over SSH (both full close and borrowed-child close). Structurally never touches any checkout path.
   - `mj-targets/src/targets/session_target.rs` and `recovery.rs`: proxy runs `<data_root>/workers/<id>/hel worker proxy --root <data_root>/workers/<id>` over SSH; `workspace_root` mirrors `LocalBare`.
   - `mj-controller/src/controller/subagents.rs`: `borrowed_locator` clones `SshHostBare { host, data_root }` directly so borrowed sub-agents and reviewers inherit A's data root, parent's `target_runtime` (`"ssh-host-bare"`), and `machine_id`, staging portable remote worker binaries via `uname -sm` and private profiles over SSH; standalone owning sessions/orchestrators created from `[targets.<source-id>-bare]` use ordinary `SshBare`.
   - Live SSH shared-profile probe (`mj-worker-launch/src/local_profile_homes.rs`, `mj-session-runtime/src/worker_client/credential_targets.rs`, `credential_sync.rs`, `resume/in_place.rs`): for non-nested `SshHostBare`, dispatch deduplicated non-blocking background SSH probes (off the worker-target refresher path, cancellable on daemon shutdown) that return `Owned`, `OwnedSharedLogin` (owned dir with symlinked login file), `SharedHome`, or transient `None` (uncached, retried next refresh). Fail closed while unknown; cache `Owned` for the attachment, and non-blockingly re-probe `OwnedSharedLogin` / `SharedHome` on periodic refresh ticks so post-commit Codex `auth.json` detachment on A is discovered without reconnecting the worker!
   - Safe restage (`mj-worker-staging/src/worker_binary/install.rs`): for `SshHostBare`, match `LocalBare`'s restage behavior (`install.rs:1217-1231`) by unlinking a whole-directory symlink (`if [ -L "$1" ]; then rm -f -- "$1"; fi`) before creating and staging an owned profile directory, and refuse in `clear_profile_configuration` if `<profile_home>` is a symlink. Keep `HarnessRuntimePolicy::Managed` (`launch.rs:643`), noting that restarting a migrated host-bare session uses the managed harness install on A.
   - Fix every dangerous wildcard / `matches!` / `!matches!` site from Advisor's 70-site audit (converting `_` wildcards on `TargetLocator` to exhaustive matches where practical): `controller/environment.rs:221-255` & `:827-855`, `daemon/restart.rs:399-400`, `server_runtime/api/fetch_transfer.rs:9-13`, `checkpoint/latched.rs:173-181`, `checkpoint/leftovers.rs:124`, `checkpoint_transfer.rs:501-567` (support real absolute A paths with spaces/Unicode safely via `posix_quote`), `mj-build-cache/src/mbx/release.rs:262,517` (never expose worker root to `mbx clean --under`), `lifecycle.rs`, `lifecycle/origin_delta.rs`, `resume.rs`, `resume/repair.rs`, `move_session/transport.rs`, `subagent_park.rs`, `aws_credentials.rs`, `target_storage.rs`, `targets/storage.rs`, `recovery_scan.rs` (no orphan scan for `SshHostBare`), and `worktree.rs` (allow plain non-Git directories on `SshHostBare`).
   - `mj-client/src/daemon.rs:2363`: bump `PROTOCOL_VERSION` from `PRE_HTTP_SESSION_ACTIONS_PROTOCOL + 6` to `PRE_HTTP_SESSION_ACTIONS_PROTOCOL + 7` (protecting Rust daemon clients from deserializing an unknown locator variant; worker relay does not carry `TargetLocator`, direct HTTP/web callers receive `SessionRecord.target`, and Go is gated by schema floor 163).

Result: rev-162 stores upgrade cleanly to 163 with 3-column `environment_workers_environment` index and `runtime_json` last; `SshHostBare { host, data_root }` round-trips in both placement tables, refuses invalid writes/reads cleanly, structurally prevents checkout deletion, fails closed on SSH shared-home probes, unlinks links safely on restage, and passes workspace clippy `-D warnings` and targeted tests.


### Milestone 2: Layout classification cleanup, `session_memory_keys`, semantic preflight, and atomic store/config/credential rewriter

Goal: fix `layout.rs` misclassifications discovered on the live store, add the `session_memory_keys` sidecar table (rev 164), build the semantic preflight inspector (including EC2 ingress & effective B SSH checks), implement the atomic store/config/unmanaged-profile-under-`$HOME`/token rewriter, and implement post-commit Codex symlink detachment.

1. Layout classification fixes (`mj-core/src/config/layout.rs`):
   - Inspect `mj-core/src/attachment.rs` to verify ownership/lifecycle of `sessions/<id>/attachments.lock` (classify as `MigrateAction::Skip`, eliminating ~64k spurious `Unknown` lock files) and `sessions/<id>/attachments.deleted` (tombstone marker: classify as `MigrateAction::Copy` so deleted attachments never resurrect).
   - Classify legacy top-level residue names seen on real hosts (`backups`, `moves`, `git-brokers`, `git-seeds`, `recovery`, `state.json.migrated-v1`, `update-check.json`, `mjolnir.db`, `node.log`): mark `backups` (10.35 GB) as `CopyOptional` so default Phase 1 skips it while `--include-logs` can carry it, and mark `git-brokers`/`git-seeds`/`update-check.json`/`node.log` as `Skip`, keeping truly unrecognized user files as `Unknown` (copy and report).

2. Pinned pre-migration project-memory identity (`mj-store`, `mj-worker-launch`):
   - Advance `SCHEMA_VERSION` to 164 (breaking, floor rises to 164; O(1) DDL on a tiny sidecar table): create `session_memory_keys(session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE, memory_key TEXT NOT NULL) STRICT`. Older readers do not consult this sidecar and would compute a different project-memory directory from rewritten SSH placements.
   - In `mj-worker-launch/src/worker_launch.rs:1121-1175` (and project memory resolution), when a session has a pinned key in `session_memory_keys`, use it directly in place of recomputing `ProjectMemoryIdentity::for_legacy_session`.
   - During pre-rewrite capture on A, compute the current `ProjectMemoryIdentity::key()` for EVERY session without `session.project` whose locator, `target_template_id`, or managed-worktree canonicalization will change (raw `LocalBare`, container sessions without `session.project`, managed-worktree sessions), and record that key in `session_memory_keys` on B. Zero directory copying needed, zero collision risk, and running A workers' replicas continue merging cleanly against the exact same canonical `<data>/projects/<key>/memory` directory on B.

3. Semantic preconditions and preflight inspector (`mj-store/src/database/migrate.rs` + `mj-controller/src/migrate/preflight.rs`):
   - Semantic blocking predicates (NEVER raw row existence!):
     - `session_moves`: parse `operation_json` and block ONLY on non-terminal/recoverable active moves (matching `mj-daemon/src/daemon/session_move.rs:549-559`); carry completed and failed terminal rows (rewriting their embedded JSON paths).
     - `worker_restart_intents`: carry waiting intents (`prepared`/`swapping`/`awaiting_readiness`) by rewriting their `target_json` cleanly; block only if an intent cannot be safely rewritten.
     - `session_restart_intents`, `session_create_intents`, `checkpoint_operations`: block on active rows (naming exact session IDs and phases).
     - `sessions.state`: block on transitional lifecycle states (`provisioning`, `startup-cleanup`, `checkpointing`, `closing`, `destroying`).
     - `environments.phase`: carry `active` and `reclaimed` (2,795 retained tombstones on live store); report/block active `retiring` environments whose cleanup is still in flight on A.
     - `LocalBare` root provenance check: verify EVERY `LocalBare` row in `session_targets` and `environment_workers` has `worker_root == <old_data_root>/workers/<session_id>`; refuse and list any non-conforming row.
     - Refuse container-hosted daemon (`daemon-container.json` or container `[daemon]` runtime), unfinished `config-rename.json`, and `ec2/*/resources.json` in `state == "preparing"` (or malformed JSON).
   - External dependency & B-readiness preflight:
     - Warn if `--source-host` is an IP literal (since A's host is frozen in every migrated row, recommend a stable DNS/Tailscale/SSH-config alias).
     - Inventory all profiles with unmanaged homes (`home != managed_profile_home_in(old_config_root, id)`), plus `[github].session_token_path` and `github.app.private_key_path`:
       - For every path under A's `$HOME` (`<A-home>/<rel>`), plan to copy the complete unmanaged profile home directory (plus external XDG `nested_data_dir` when outside `profile.home`) or token file (mode `0600`) to `<B-home>/<rel>` on B, and **refuse in preflight if `<B-home>/<rel>` already exists on B**!
       - For any unmanaged profile home or token path outside A's `$HOME` (e.g. `/mnt/optane/...`), report it explicitly in preflight (and warn/refuse if missing on B).
     - Report host-local items that cannot be auto-relocated: `phone.tls_cert`, `phone.tls_key`, `phone.bind`, `bundles.*.repositories[].local`, `go.json` paths, `{ from_env = ... }` secret variable names (never printing values), `SSH_AUTH_SOCK` / path-like `profiles.*.environment` values, AWS profile names, macOS Keychain logins, and `machine_id IS NULL` placements.
     - EC2 security-group ingress check: for active `ec2/*/resources.json` records, inspect whether B's public IPv4 is admitted by the recorded SSH security group (`mj-cli/src/ec2.rs:1153-1198`), offering to authorize B's `/32` using A's AWS credentials during preflight (or printing the exact AWS CLI / warning when `--dry-run`, `--no-aws-modify`, or credentials are absent).
     - Effective B-side SSH probes: ask B over SSH to verify `B -> A` reachability (when keeping live local sessions: check `id -un`, A's `instance-identity`, and matching container engine when local containers exist) AND probe B's effective SSH reachability to configured third-host SSH machines C (reporting any missing aliases/keys/known_hosts on B).

4. Atomic store & config rewriter (`mj-store/src/database/migrate.rs` + `mj-controller/src/migrate/rewrite.rs`):
   - `apply_store_migration(db_path, spec)` runs in ONE SQLite `IMMEDIATE` transaction while `store_fence` remains in place (verified against `spec.expected_fence` before modifying anything):
     - Suppresses duplicate discovery-trigger noise (`project_discovery_changes`) or reconciles `project_discovery_changes` (`directory`, `managed_worktree`, `target_template_id`) after session rewrites in the same transaction.
     - Rewrites `session_targets` and ALL `environment_workers` leases (including non-current/historical): `LocalBare` -> `SshHostBare { host: source_ssh.host, data_root: old_data_root }`, `LocalPodman` -> `SshPodman`, `LocalDocker` -> `SshDocker`, `AppleContainer` -> `SshAppleContainer`, setting `machine_id = source_machine_id`. Existing SSH placements (including SSH-to-B) and `AwsEc2` rows remain untouched.
     - Rewrites `sessions`: `target_template_id`, `target_runtime_json` (converting local kind + `TargetConnection::Local` to SSH for migrated A rows, AND relocating `<old_config_root>/ec2/` paths in `identity_file`/`extra_args` across ALL rows), `managed_worktree` (`ManagedWorktreeTarget::Local` -> `Ssh`, preserving A paths).
     - Rewrites `environments.placement_json` (embedded `runtime`, `prepared_destination`, `managed_worktree`, EC2 paths).
     - Rewrites `session_moves.operation_json` (including retained terminal rows): `checkpoint.archive_path` and `recovery_session.checkpoint.archive_path` under `<old_data_root>/sessions/` -> `<new_data_root>/sessions/`; leave `source_snapshot.archive_path` on its source target untouched; rewrite embedded locators/templates/EC2 paths consistently.
     - Rewrites `worker_restart_intents.target_json` (for carried waiting intents) consistently with the session's rewritten locator.
     - Rewrites `retained_move_sources.source_json` (embedded `SessionRecord` locator/runtime/worktree/archive path/EC2 paths) while preserving `exclusions_json`.
     - Drops `host = 'local'` rows from `host_snapshot_cache` (best-effort cache verified against durable archives) and rewrites `<old_config_root>/ec2/` paths inside remaining `ssh:` cache keys.
     - Remaps `host = 'local'` (and `project:local` in `mount_history`) in `host_container_sizes`, `mount_history`, and `project_locations` to `source_ssh.host`, preserving existing rows on PK/unique collision.
     - Clears controller-local `project_seed_homes` / `project_seed_failures` entries rooted on A so B re-seeds its own local homes cleanly without skipping discovery.
     - Clears `detached_drafts.owner_pid = NULL` (preserving all draft text, revisions, and recovery state).
     - Inserts pre-computed legacy project-memory keys into `session_memory_keys`.
     - Verifies every managed relative `session_checkpoints.archive_path` exists under the staged/final `sessions/` dir.
     - Does NOT clear `store_fence` inside staging (fence clearing happens ONLY as the single final commit point in Milestone 3/4).
   - `rewrite_staged_config_and_files`:
     - Loads staged `config.toml` with staged `secrets.toml` beside it.
     - Adds `[machines.<source-id>]` (`Machine::Ssh`, moving A's `[machines.local].build_cache` to A's machine and clearing B's `[machines.local].build_cache`).
     - Keeps B's `local` templates for new B sessions and creates distinct SSH target templates (`<source-id>-podman`, `<source-id>-docker`, `<source-id>-apple-container`, `<source-id>-bare`) for migrated A sessions/leases.
     - Rewrites managed profile homes (`managed_profile_home_in(old_config_root, id)` -> `new_config_root`), AND rewrites copied unmanaged `$HOME`-relative profile homes (`<old_home>/<rel>` -> `<new_home>/<rel>`) and copied `$HOME`-relative `[github].session_token_path` / `github.app.private_key_path` (`<old_home>/<rel>` -> `<new_home>/<rel>`) when `$HOME` differs between A and B.
     - Component-aware rewrites `<old_config_root>/ec2/...` in `Machine::Ssh` (`identity_file`, `extra_args` `-i`/`UserKnownHostsFile=...`) and `Machine::AwsEc2` (`identity_file`, `ssh_args`).
     - Sanitizes `[daemon]` for native B execution.
     - Validates via `Config::validate()` and writes with `Config::save_to`.
     - Updates `go.json` renamed target IDs if needed.
   - Post-commit Codex symlink detachment (`detach_codex_auth_symlink(worker_root)`):
     - Runs on A strictly AFTER B's activation commit succeeds (so rollback leaves A 100% untouched).
     - Verifies `symlink_metadata(<worker_root>/profile)` is a real directory (not a symlink), reads linked `auth.json`, validates JSON, writes mode-`0600` temp file in the same directory, fsyncs, and atomically renames over `auth.json`. (Document in code/help that detached Codex copies retain their refresh token, matching existing SSH Codex sessions.)

Result: integration tests verify layout classifications, `session_memory_keys` preservation, semantic preconditions on stores with terminal Move rows and waiting worker-restart intents, complete store + config + `$HOME`-relative unmanaged profile + token + EC2 rewrites, collision refusal when a B `$HOME` path already exists, and post-commit Codex symlink detachment.


### Milestone 3: Separate-process WAL-pinned streaming transfer engine and single-commit fenced publication

Goal: build the pure-Rust, zero-external-rsync, experimentally airtight transfer and activation engine over a single supervised SSH pipe (`mj migrate` <-> `mj migrate-receive`) with a dedicated `mj migrate-pin` helper process and one atomic SQLite activation commit point on B.

1. Supervised full-duplex SSH subprocess helper (`mj-core/src/subprocess.rs`):
   - Add `SupervisedPipeChild` (alongside `SupervisedChild`, using `spawn_async_child`, Unix `process_group(0)` / `ProcessGroupGuard`, `kill_on_drop(true)`, piped `stdin`/`stdout`, and concurrent background stderr draining).

2. Dedicated SQLite WAL pin helper (`mj-controller/src/migrate/pin.rs` invoked via hidden `mj migrate-pin`):
   - **Structural invariant**: `mj migrate-pin` touches `mj.sqlite3` (or `sessionwiki/index.db`) ONLY through `rusqlite::Connection`; it NEVER opens database files with raw file I/O. Conversely, the streaming sender process performs raw byte reads and NEVER holds SQLite connections to the pinned database while streaming!
   - Lifecycle of `mj migrate-pin --db <path>`:
     1. Optionally runs a short-busy-timeout (200 ms) `PRAGMA wal_checkpoint(TRUNCATE)` on a temporary connection and closes it (if busy, silently skips — best effort to start from an empty WAL).
     2. Opens read-only connection `R_pin`, verifies `PRAGMA journal_mode` is `wal`, starts `BEGIN DEFERRED`, executes `SELECT 1 FROM sqlite_master LIMIT 1` (establishing the WAL read mark), reads the initial WAL file identity `(exists, ino, header_32_bytes)` via a separate helper/command OR wait: **NEVER call `File::open` on the DB file inside `mj migrate-pin`**! Calling `stat()` on `-wal` (never opening/closing the main DB!) or having the parent sender read `-wal`'s 32-byte header and inode keeps `mj migrate-pin` 100% free of raw DB file descriptors!
     3. Prints `READY` (with schema `user_version`) to stdout and flushes, then blocks reading stdin until EOF or `RELEASE`.
     4. While `mj migrate-pin` stays alive with `R_pin` open, the sender process verifies `mj migrate-pin` has not exited and checks `-wal` inode + 32-byte salt header before and after cutover (allowing only: identical inode + identical 32-byte salt header, OR empty/absent WAL at pin time gaining its first valid non-zero WAL header!). If the pin process died or WAL identity/salts changed unexpectedly, abort cutover, clear fence if written, restart A, and report!
     5. Note on fence write: when Phase 2 writes `store_fence` on A, it runs in a **separate short-lived process** (or `mj daemon fence` / dedicated helper — NOT inside `mj migrate-pin` and NOT after raw DB I/O in the same process!) so `mj migrate-pin`'s POSIX `fcntl` locks are NEVER disturbed! Wait: verify! Because `mj migrate-pin` is a separate OS process from `mj migrate`, when `mj migrate` itself writes `store_fence` (having closed its raw DB reader before calling `fence_configured_store` — or even better, spawning a clean child or running SQLite in `mj migrate` only AFTER raw DB reads finish, while `mj migrate-pin` stays in its own completely independent OS process!), `mj migrate-pin`'s OS process locks are 100% isolated by the kernel!

3. Simplified 3-frame streaming protocol (`mj-controller/src/migrate/protocol.rs`):
   - Length-prefixed frames (`u32` big-endian length + frame tag + payload, max 8 MiB frame, 1 MiB DB block size, `zstd` level 1 per chunk):
     - `Hello` / `HelloAck`: exchanges `mj` version, `SCHEMA_VERSION`, `PROTOCOL_VERSION`, OS family (`"unix"`), B's resolved `$HOME`, `data_dir`, `config_dir`, `sessionwiki_dir`, destination store status (`Empty`, `ExistingStore { has_db, has_config }`, `ContainerDaemon`, `ResumablePublication`), SessionWiki status (`Absent`, `Existing`), collision check on proposed `$HOME`-relative unmanaged profile/token target paths, and free disk space on B's target filesystems. Refuses whenever `ExistingStore` (unless `ResumablePublication`) or any proposed `$HOME`-relative unmanaged path already exists on B!
     - `ProbeSsh` / `ProbeSshAck`: asks B to run `ssh_probe_command` back to A (`--source-host` verifying `id -un`, A's `instance-identity`, and container engine if needed) and optionally to third-host SSH machines C using B's effective OpenSSH config.
     - `StageManifest` / `StageManifestAck`: returns existing staged regular files `(rel_path, size, mtime_ns, sha256, mode, symlink_target)` and 1 MiB block SHA-256s of staged `mj.sqlite3` and `sessionwiki/index.db` for Phase 1 resume.
     - `PutFile` (streamed in chunks, preserving regular file mode bits and symlink targets explicitly, atomic temp+fsync+rename, whole-file SHA-256 verified).
     - `PatchBlocks` (**Phase 1 LIVE ONLY**, for `mj.sqlite3` and `sessionwiki/index.db`): updates 1 MiB blocks in place with per-block zstd + SHA-256 verification and `set_len(total_size)`. (On resume, always re-hashes the current live source under the current pin — never trusts stale hashes across runs!)
     - `RemoveFile`: removes a file from staging deleted on A between Phase 1 and Phase 2.
     - `CommitPublication` / `CommitPublicationAck`: carries `MigrationRewriteSpec`, `expected_fence`, `start_daemon`, `overwrite_sessionwiki`.

4. Two-phase sender and single-commit receiver (`mj-controller/src/migrate/transfer.rs`):
   - **Phase 1 (daemon A LIVE — 0s downtime, keeping pin window SHORT!):**
     1. Walk `DataLayout` + `ConfigLayout` (using corrected `layout.rs` classifications) + referenced unmanaged profile homes + referenced token/key files: sync all non-DB files FIRST (`PutFile`, preserving modes, directory permissions, and symlinks explicitly) so the DB pin is NOT held while copying 45+ GiB of `sessions/` and `tool-details/`!
     2. **Optional warm unpinned base pass** for `mj.sqlite3` (and `sessionwiki/index.db` if migrating SessionWiki): if B's staging does not yet have a base (or on slow links), stream `mj.sqlite3` unpinned first into B's staging via `PatchBlocks` so 99.9% of blocks are already on B before pinning!
     3. **Pin `mj.sqlite3`** via separate `mj migrate-pin` process (and separately pin `sessionwiki/index.db` via a second `mj migrate-pin` process if migrating SessionWiki!). Record each WAL's initial `(exists, ino, header_32_bytes)` and `schema_version` from the sender process.
     4. Run ONE live `PatchBlocks` compare pass on `mj.sqlite3` (and `sessionwiki/index.db`) against B's staging while pinned — hashing the current live source under the current pin so B's staged base is proven to belong to this pin generation! (Even if pages are torn during this pass, E3 proved every changed page has its complete frame in the pinned WAL!)
   - **Phase 2 (atomic cutover — the only downtime window!):**
     1. Acquire `DaemonStartGuard` (`daemon-start.lock`) on A.
     2. Stop daemon A cleanly via `stop_instance_holding` (while `mj migrate-pin` stays open in its separate OS process!).
     3. Acquire `ControllerStoreGuard` (`controller.lock`) on A.
     4. Recheck `SCHEMA_VERSION` (in case daemon A upgraded during Phase 1!) and authoritative semantic preconditions under lock. If any precondition fails: release `ControllerStoreGuard`, stop `mj migrate-pin` processes, restart daemon A automatically, release `DaemonStartGuard`, and report the exact blocker.
     5. Write `store_fence` row on A (via `fence_configured_store` — A's daemon is stopped and `mj migrate-pin` is in a separate OS process, so `mj migrate-pin`'s POSIX locks remain intact!).
     6. **Verify pin invariants**: confirm `mj migrate-pin` is STILL alive with its transaction open, and `-wal` has the same inode and 32-byte salt header as at pin time (or empty WAL gained its first valid non-zero header). If NOT: clear `store_fence` on A, release `ControllerStoreGuard`, stop pin processes, restart daemon A, release `DaemonStartGuard`, and fail cleanly (no repair attempts during downtime!).
     7. Send complete `mj.sqlite3-wal` via `PutFile` (SHA-256 verified; NEVER send `-shm`), send complete `sessionwiki/index.db-wal` via `PutFile` (if migrating SessionWiki), and re-scan + send only changed/added/deleted mutable non-DB files (`tool-details/`, `sessions/`, `projects/`, `config/`, `profiles/`, `ec2/`, `Unknown`, unmanaged profile homes, external tokens) via `PutFile`/`RemoveFile`.
     8. Stop `mj migrate-pin` processes (now that WALs are verified on B!).
     9. Send `CommitPublication` to B!
   - **Single-commit publication on B (`mj migrate-receive`):**
     1. Acquire B's **`daemon-start.lock`** FIRST (created inside B's `data_dir` on a stable inode — NEVER replace B's `data_dir` directory itself, only entries inside it!).
     2. Under `daemon-start.lock`, re-verify destination B has NO existing `mj.sqlite3` or `config.toml` (unless resuming a staged publication whose final `mj.sqlite3` is fenced with this migration's marker!) and NO colliding `$HOME`-relative unmanaged profile/token paths.
     3. Acquire B's **`controller.lock`** (`ControllerStoreGuard`) on a stable inode (never replacing `controller.lock` or `daemon-start.lock`!).
     4. Run `apply_store_migration` and `rewrite_staged_config_and_files` INSIDE staging while B's `mj.sqlite3` is STILL FENCED (verifying A's exact `store_fence` row was recovered from WAL!).
     5. Write a durable publication journal marker in staging, then publish **dependencies FIRST while B's final `mj.sqlite3` is NOT YET UNFENCED**:
        - Copy/move staged `$HOME`-relative unmanaged profile homes and external token files into `<B-$HOME>/<rel>` (refusing if any target already exists unless matching our own publication journal).
        - Copy/move staged config entries (`config.toml`, `secrets.toml`, `profiles/`, `ec2/`, `go.json`, `client-hints.json`, unknown config files) into B's `config_dir`.
        - Move staged SessionWiki `index.db` + `-wal` (omitting `-shm`) into B's SessionWiki dir (if migrating SessionWiki).
        - Move staged data entries (`instance-identity`, `api-token`, `phone-cookie-key`, `phone-cookie-revocations.json`, `web-push-vapid.pk8`, `sessions/`, `tool-details/`, `projects/`, unknown data files) into B's `data_dir`.
        - Move the **rewritten, STILL-FENCED** `stage/data/mj.sqlite3` (checkpointed cleanly during staging rewrite so no `-wal` is needed at final placement!) into B's final `<B-data-dir>/mj.sqlite3` LAST!
     6. **THE SINGLE COMMIT POINT**: open B's final `<B-data-dir>/mj.sqlite3` (still holding B's `daemon-start.lock` and `controller.lock`), run `clear_store_fence` in ONE SQLite commit, and remove staging!
     7. Release B's `controller.lock`, start B's daemon under the held `daemon-start.lock` (unless `--no-start`), verify B's daemon responds healthy, release `daemon-start.lock`, and reply `CommitPublicationAck::Activated`!
     8. **Re-runnability**: if a crash interrupts steps 4–6 before the final unfence commit, B's final `mj.sqlite3` (if placed) is STILL FENCED (or staging still holds the publication marker), so re-running `mj migrate` cleanly completes publication!
   - **Post-commit step on A (strictly AFTER `CommitPublicationAck::Activated`!):**
     - Run `detach_codex_auth_symlink` on A for each eligible `LocalBare` Codex session (logging warnings rather than rolling back if a live Codex worker already rotated or moved a file, since B is already active!).
     - Release `ControllerStoreGuard` and `DaemonStartGuard` on A, leaving A permanently fenced!
   - **Rollback on failure:**
     - Before `CommitPublication` is sent, OR if B explicitly replies `CommitPublicationAck::FailedBeforeCommit` (B's final fence was NEVER cleared): clear `store_fence` on A, release `ControllerStoreGuard`, restart daemon A, release `DaemonStartGuard`, and report the error. Host A is 100% unmodified!
     - On SSH disconnect or after B's commit point: NEVER clear A's fence! Instruct the user to inspect `mj daemon status` on B before taking any manual action.

Result: separate-process OS tests prove POSIX lock preservation across `mj migrate-pin` + raw byte copy + separate daemon process + separate fence process (reproducing E1/E2 failure if run in-process and proving 100% recovery in separate processes!), plus >1 MiB pipe/resume/corruption tests and single-commit publication tests.


### Milestone 4: User-facing `mj migrate` CLI, hidden subcommands, and real-binary localhost-`sshd` end-to-end tests

Goal: wire `mj migrate`, hidden `mj migrate-receive`, and hidden `mj migrate-pin` into `mj-cli`, add clear phase-by-phase progress and preflight reporting, and prove the complete system end-to-end with the real `mj` binary over a real localhost `sshd`.

1. CLI commands (`mj-cli/src/main.rs`, `mj-cli/src/migrate.rs`):
   - Top-level `Command::Migrate(MigrateArgs)` with:
     - `destination: String` (configured `[machines.<id>]` or SSH destination)
     - `--source-host <SSH_DEST>` (stable SSH name for B -> A; warn if IP literal)
     - `--source-machine-id <ID>` (default: sanitized valid `MachineId` from A's hostname)
     - `--source-ssh-arg <ARG>` / `--ssh-arg <ARG>` (repeatable)
     - `--dest-instance <NAME>` / `--dest-data-dir <PATH>` / `--dest-config-dir <PATH>`
     - `--remote-command <CMD>` (default `"mj"`)
     - `--dry-run` (read-only full preflight + plan)
     - `--include-logs` (include `CopyOptional`, including `backups`, `daemon.log`, `logs/`, `diagnostics/`, `jev-decisions/`)
     - `--include-sessionwiki` / `--skip-sessionwiki` / `--overwrite-sessionwiki`
     - `--disconnect-local` (proceeds without `--source-host` when retiring A, registering A as a normal SSH machine entry)
     - `--no-aws-modify` (report EC2 security-group ingress gaps without modifying AWS)
     - `--no-start` (activate and unfence B without starting B's daemon)
   - Hidden `#[command(hide = true)] Command::MigrateReceive(MigrateReceiveArgs)` and `#[command(hide = true)] Command::MigratePin(MigratePinArgs)`.
   - Enforce `ensure_may_control_store()` in both `migrate` and `migrate-receive` so Cargo `target/` test binaries never touch the host's default store.

2. Real-binary end-to-end tests (`mj-cli/tests/migrate_e2e.rs`, using real `CARGO_BIN_EXE_mj`, real localhost `sshd`, and isolated A/B directories):
   - **Test 1 (Full migration + `--dry-run`)**: real daemon A with suspended session + managed `.hel.zip`, two `LocalBare` sessions (real `profile/` + Codex `auth.json` symlink; whole-dir `profile` symlink), `LocalPodman` placement + `environment_workers` lease, terminal `session_moves` rows (completed + failed) with `operation_json` archive paths, waiting `worker_restart_intents` row, `ec2/` record + key + `UserKnownHostsFile`, `$HOME`-relative unmanaged profile home + external `[github].session_token_path`, `detached_drafts` with `owner_pid`, `host_snapshot_cache` local row + `host_container_sizes` + `mount_history` + `project_locations`, unknown data file, and SessionWiki `index.db`. Verify `--dry-run` leaves A & B untouched; verify full `mj migrate` fences A, starts B with identical `instance-identity`, rewrites every table/config/profile/secret/EC2/memory key, copies `$HOME`-relative unmanaged profile/token to B's `$HOME` + SessionWiki + unknown file, detaches Codex `auth.json` on A ONLY after B commits, and verifies A client commands refuse before spawning.
   - **Test 2 (Crash windows & competing B autostarter)**: inject receiver exits before/after publication steps and before/after B's fence commit, AND spawn a concurrent real `mj` client on B attempting auto-start during publication — assert B NEVER starts an unfenced partial store, re-running `mj migrate` completes publication cleanly, and neither side can ever split-brain.
   - **Test 3 (Rollback & destination refusal)**: verify failure before B commits rolls back A cleanly to running unfenced with Codex `auth.json` symlinks untouched; verify existing `mj.sqlite3` or `config.toml` on B (or colliding `$HOME`-relative unmanaged profile/token path on B) is refused in preflight with `--dest-instance` / cleanup guidance!


## Concrete Steps

Run all commands from the repository root (`/home/jonathan/Projects/mjolnir/.mj/clones/9f45d4d9bc69809be6be8c90b0d03c0e`). Run every `cargo test` outside the restricted sandbox (`BypassSandbox: true`). Verify every filtered `cargo test` runs at least one test.

Milestone 1:

    cargo clippy --workspace --all-targets -- -D warnings
    cargo test -p brokk-mj-core target_locator
    cargo test -p brokk-mj-store host_bare
    cargo test -p brokk-mj-store schema
    cargo test -p brokk-mj-targets host_bare
    cargo test -p brokk-mj-worker-launch host_bare
    cargo test -p brokk-mj-worker-staging host_bare
    cargo test -p brokk-mj-session-runtime host_bare
    cargo test -p brokk-mj-client unsupported_protocol
    cargo test -p brokk-mjolnir --test daemon_startup

Milestone 2:

    cargo clippy --workspace --all-targets -- -D warnings
    cargo test -p brokk-mj-core layout
    cargo test -p brokk-mj-store migrate
    cargo test -p brokk-mj-controller migrate

Milestone 3:

    cargo clippy -p brokk-mj-core -p brokk-mj-store -p brokk-mj-controller --all-targets -- -D warnings
    cargo test -p brokk-mj-core subprocess
    cargo test -p brokk-mj-controller migrate

Milestone 4:

    cargo clippy --workspace --all-targets -- -D warnings
    cargo test -p brokk-mjolnir --test migrate_e2e
    cargo test -p brokk-mjolnir --test daemon_startup

Commit and run `request_review` after each milestone before moving to the next.


## Validation and Acceptance

Follow `AGENTS.md` "Which tests to add": integration, end-to-end, and tricky-condition tests only. Drive every pipe/transfer test with >1 MiB payloads (well past 64 KiB pipe buffers).

- Milestone 1: rev-162 -> 163 schema upgrade integration test (verifying 3-column `environment_workers_environment` index, `runtime_json` last, FK integrity, `SshHostBare { host, data_root }` round-trip in both tables, write validation, fallible Rust decode errors naming session ID, and rev-122 historical upgrade); cleanup safety test (only `<data_root>/workers/<id>` and `<data_root>/profiles/<id>`, rejecting unsafe `data_root`); live SSH shared-home probe failing closed + unlink-on-restage test; `daemon_startup` schema-162 fixture test.
- Milestone 2: `layout.rs` classification tests (`attachments.lock`, `attachments.deleted`, `backups`); `session_memory_keys` preservation test; semantic precondition tests (terminal `session_moves` + waiting `worker_restart_intents` allowed, active moves/creates/transitional states blocked); full store + config + `$HOME`-relative unmanaged profile + external token + EC2 + `session_moves.operation_json` rewrite integration test; Codex symlink detachment test.
- Milestone 3: separate-OS-process SQLite WAL-pinning concurrency test (pin process, separate daemon writer process, raw copier process, separate fence process — proving torn live base + complete same-generation WAL recovers exact source state and fence row with `PRAGMA integrity_check == ok`, AND proving pin-loss / WAL-salt mismatch fails cleanly); >1 MiB framed pipe + `PatchBlocks` resume + SHA-256 mismatch tests; B single-commit publication & crash-recovery tests.
- Milestone 4: real-binary localhost-`sshd` end-to-end tests (`--dry-run`, full happy path, crash-window + competing B autostarter race, rollback leaving A untouched, and existing-B / colliding-`$HOME` refusal).


## Idempotence and Recovery

- `--dry-run` is strictly read-only on both A and B.
- Phase 1 writes only into staging directories on B, leaving B's real directories untouched and daemon A running. Re-running `mj migrate` resumes from staging, re-hashing current live source blocks under the current pin so stale blocks are never trusted blindly.
- Phase 2 holds A's `daemon-start.lock` and `ControllerStoreGuard`, and B's receiver holds B's `daemon-start.lock` and `controller.lock` on stable inodes throughout publication and the final unfence commit.
- Failure before B's unfence commit automatically clears A's fence and restarts daemon A (host A is 100% unmodified because Codex `auth.json` detachment runs only after B's commit).
- Crash during B's publication leaves B's store fenced (or absent under lock); re-running `mj migrate` finishes publication cleanly. Once B's commit point is reached or uncertain (e.g., SSH disconnect during commit), A's fence is NEVER cleared automatically.


## Artifacts and Notes

Baseline:

    HEAD: d098d793a (origin/master)
    SCHEMA_VERSION: 162 (floor 162)
    CONFIG_VERSION: 24 (unchanged)
    PROTOCOL_VERSION: PRE_HTTP_SESSION_ACTIONS_PROTOCOL + 6

Schema & protocol changes:

    Schema 163 (breaking, floor 163): rebuild session_targets and environment_workers without kind-dependent CHECK constraints, add data_root BLOB (before runtime_json on environment_workers), recreate environment_workers_environment(environment_id, worker_session_id, lease_id).
    Schema 164 (breaking, floor 164): add narrow sidecar table session_memory_keys(session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE, memory_key TEXT NOT NULL) STRICT for pre-migration legacy project-memory continuity (older readers ignore pinned keys and would compute a different directory from rewritten SSH placements).
    Protocol bump: PRE_HTTP_SESSION_ACTIONS_PROTOCOL + 7 (SshHostBare wire compatibility for Rust daemon clients).


## Interfaces and Dependencies

No new workspace crates and no new external dependencies beyond workspace crates already declared in `Cargo.toml` (`zstd`, `sha2`, `rusqlite`, `serde`, `serde_json`, `chrono`, `tempfile`, `tokio`). Add `zstd.workspace = true` to `mj-controller/Cargo.toml`.

In `mj-core/src/state.rs` and `mj-core/src/targets.rs`:

    TargetLocator::SshHostBare { host: String, data_root: PathBuf }        // state.rs ("ssh-host-bare")
    TargetLocator::SshHostBare { ssh: SshTarget, data_root: PathBuf }      // targets.rs ("ssh_host_bare")

In `mj-core/src/subprocess.rs`:

    pub struct SupervisedPipeChild { ... }

In `mj-store/src/database/migrate.rs`:

    pub fn inspect_migration_preconditions(connection: &Connection) -> Result<MigrationPreconditions>;
    pub fn inspect_migration_inventory(connection: &Connection) -> Result<StoreMigrationInventory>;
    pub fn apply_store_migration(db_path: &Path, spec: &MigrationRewriteSpec) -> Result<StoreRewriteSummary>;

In `mj-controller/src/migrate/`:

    pub mod preflight;
    pub mod rewrite;
    pub mod pin;
    pub mod protocol;
    pub mod transfer;

In `mj-cli/src/main.rs` and `mj-cli/src/migrate.rs`:

    Command::Migrate(MigrateArgs)
    #[command(hide = true)]
    Command::MigrateReceive(MigrateReceiveArgs)
    #[command(hide = true)]
    Command::MigratePin(MigratePinArgs)


---

Revision note (2026-11-26): Revised entire plan following MJ Advisor's two-part review, live-store/SQLite experiments, and user decisions. Simplified `SshHostBare` to `{ host, data_root }` with live SSH shared-home probing and unlink-on-restage (removing `nested_profile_root`, `shared_profile_home`, and parked-child refusal); replaced same-process WAL pinning + Phase 2 `PatchBlocks`/`stat`/`AppendTail` with a dedicated `mj migrate-pin` child process + 3-frame streaming protocol shared by both `mj.sqlite3` and the 16.65 GiB SessionWiki index; added semantic preconditions, `session_moves.operation_json` and discovery/seed rewrites, `session_memory_keys` sidecar (rev 164), `$HOME`-relative unmanaged profile/token carrying (refusing collisions on B), EC2 ingress check, refusal of any existing B `mj.sqlite3`/`config.toml`, single-commit fenced publication under B's daemon locks, and post-commit Codex detachment; deferred `retired` machine support to a follow-up.
