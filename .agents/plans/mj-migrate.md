# Add `mj migrate` for Minimal-Downtime Host-to-Host Daemon Migration

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This repository maintains its execution-plan standard in `.agents/PLANS.md`. Maintain this document in accordance with `.agents/PLANS.md` so a reader with only the current working tree, `.agents/PLANS.md`, and this file can execute every milestone end to end.


## Purpose / Big Picture

Today a Mjolnir daemon lives permanently on the machine where its store was created. Moving a daemon with tens of gigabytes of transcript history (`mj.sqlite3`, `tool-details/`, `sessions/`), managed harness profiles, EC2 keys, and live sessions onto a new machine requires manual copying, error-prone SQL and TOML editing, and long downtime, and cannot represent live local-bare sessions on the old machine over SSH without risking deletion of the user's project directories.

After this plan is implemented, a user can run `mj migrate <destination>` on the old machine (host A) to move the daemon to a new Unix machine (host B) with minimal downtime while keeping history intact and sessions reachable. The old daemon stops permanently and stays fenced against accidental restarts (multi-daemon is not a goal). Specifically, a user will be able to:

1. Run `mj migrate <dest> --dry-run` to check preconditions, SSH connectivity (both A-to-B and, when keeping local sessions, B-to-A), disk space, SessionWiki state, unknown files, external dependencies (non-managed profile homes, SSH keys outside `ec2/`, AWS profiles, `{ from_env = ... }` secrets, local repository paths), and placements with missing `machine_id`, without modifying either machine.
2. Run `mj migrate <dest> --source-host <ssh-dest-from-B>` to pre-sync almost all state (including a ~25–30 GiB `mj.sqlite3`, `tool-details/`, `sessions/`, `projects/`, `config/`, and SessionWiki) while host A's daemon keeps running with zero downtime, then perform a brief atomic cutover (fencing A, syncing only the write-ahead log and changed tails, rewriting A's local placements and config paths on B, unfencing B, and starting B's daemon).
3. Keep live and historical `LocalBare` sessions on host A reachable from host B over SSH through a new typed `SshHostBare` locator that preserves host A's exact worker root and nested profile root and structurally cannot delete a user checkout.
4. Keep `LocalPodman`, `LocalDocker`, and `AppleContainer` sessions on host A reachable as `SshPodman`, `SshDocker`, and `SshAppleContainer`, while leaving existing SSH sessions already hosted on B as SSH-to-self and letting new sessions on B use `local`.
5. Resume an interrupted Phase 1 pre-sync cleanly without restarting from scratch, and automatically roll back host A to running if a Phase 2 failure occurs before host B unfences (while never auto-unfencing A once B's activation begins, making split-brain impossible).

Scope for v1: native host-to-host Unix daemons (Linux/macOS within the same OS family; container-hosted daemons recorded via `daemon-container.json` are refused cleanly with an explanation). On default-instance migrations, the shared SessionWiki database (`<OS data dir>/sessionwiki/index.db`) is migrated via online SQLite backup alongside the Mjolnir store unless skipped.


## Progress

- [x] (2026-11-26 16:22Z) Read prep handoff and `.agents/plans/daemon-migration-prep.md`; verified all four prep milestones are on `origin/master` (`d098d793a`, `SCHEMA_VERSION = 162`, `CONFIG_VERSION = 24`).
- [x] (2026-11-26 16:34Z) Completed initial three-track read-only reconnaissance (`locator-design.md`, `state-rewrite-inventory.md`, `transport-and-ops.md`).
- [x] (2026-11-26 17:07Z) Confirmed three user design decisions: add typed `SshHostBare` locator for live local-bare sessions on A; keep existing B-hosted SSH sessions as SSH-to-self by default; native host-to-host daemons for v1 plus migrating shared SessionWiki on default-instance moves.
- [x] (2026-11-26 17:13Z) Completed deep-dive read-only spec checks (`ssh-host-bare-spec.md`, `rewrite-and-transfer-spec.md`) and verified `PROTOCOL_VERSION`, `detached_drafts` nullability, `ProjectMemoryIdentity::for_legacy_session`, and `SupervisedChild`.
- [x] (2026-11-26 17:18Z) Authored ExecPlan at `.agents/plans/mj-migrate.md`.
- [ ] Milestone 1: `SshHostBare` locator, schema revision 163 (drop evolving `CHECK` constraints, add `nested_profile_root` and `shared_profile_home`), protocol bump, and tests.
- [ ] Milestone 2: Preflight inspector, atomic store/config/EC2/go.json rewriter, Codex `auth.json` symlink detachment, and integration tests.
- [ ] Milestone 3: Two-phase WAL-pinned and block-delta streaming transfer engine, SessionWiki online backup transfer, resumable staging, rollback safety, and concurrency/pipe tests.
- [ ] Milestone 4: `mj migrate` CLI and hidden `mj migrate-receive` subcommand, progress reporting, and real-binary localhost-`sshd` end-to-end tests.


## Surprises & Discoveries

- Observation: `session_targets` and `environment_workers` both carry two evolving kind-dependent SQLite `CHECK` constraints from migration 124 (`CHECK(kind IN (...))` and a kind-shape disjunction) (`mj-store/src/database/schema.rs:4032-4078`), and `decode_target` relies on them with `unwrap()` and `unreachable!` (`mj-store/src/database/state_io.rs:1424-1493`).
  Evidence: Direct inspection of `migrate_nitro_target_kind` in `mj-store/src/database/schema.rs:4026-4095`. Dropping only `CHECK(kind IN ...)` would still reject `'ssh-host-bare'`. Revision 163 must drop both kind-dependent `CHECK`s (per `.agents/docs/schema-migrations.md`), keep non-kind checks, PK/FKs, STRICT, and both `environment_workers` indexes (`environment_worker_identity` and `environment_workers_environment`), and replace `unwrap()`/`unreachable!` with fallible Rust validation.

- Observation: Legacy `LocalBare` sessions can have two distinct symlink patterns under `<worker_root>/profile`: modern Codex symlinks only `<worker_root>/profile/auth.json` into A's managed profile, while older sessions can symlink the entire `<worker_root>/profile` directory to a shared personal home (`~/.claude`, `~/.codex`) (`mj-worker-launch/src/local_profile_homes.rs:9-158`). Once a locator stops being `LocalBare`, current `has_staged_profile_home` returns `true` unconditionally without checking the remote filesystem (`mj-session-runtime/src/worker_client/credential_targets.rs:42-98`).
  Evidence: `ssh-host-bare-spec.md` §"Credential safety". `SshHostBare` must persist a `shared_profile_home: bool` fact captured on host A so B's credential sync never writes into a shared personal home on A. Meanwhile, Codex's file-only `auth.json` symlink (only when `<worker_root>/profile` itself is a real owned directory) must be materialized into a regular `0600` file on A via atomic temp+rename after fencing A (when daemon A's login refresh is stopped), because the worker's `write_credential_file` refuses symlink destinations (`mj-core/src/credentials.rs:580-600`).

- Observation: `SessionRecord` carries `#[serde(deny_unknown_fields)]` and sends `target: Option<TargetLocator>` to attached clients (`mj-core/src/state.rs:1660,1775`), so older clients reject an unknown enum variant even if the store compatibility floor protects old daemons.
  Evidence: `mj-client/src/daemon.rs:2241-2266,2363`. Bumping `PROTOCOL_VERSION` (currently `PRE_HTTP_SESSION_ACTIONS_PROTOCOL + 6`) ensures older clients get the standard upgrade/protocol refusal instead of a serde failure.

- Observation: EC2 operation records embed absolute paths under `<config_dir>/ec2/<operation>/` in both `identity_file` and `extra_args` (`-i` and `UserKnownHostsFile=<path>`), and EC2 identity lookup requires the key's parent directory to lie directly under the current EC2 records root (`mj-core/src/ec2_records.rs:73-116`, `mj-cli/src/ec2.rs:1547-1571`). Those SSH options appear not only in `config.toml` (`Machine::Ssh` and `Machine::AwsEc2`) but also in `sessions.target_runtime_json`, `environment_workers.runtime_json`, `environments.placement_json`, `retained_move_sources.source_json`, and serialized `host_snapshot_cache` `ssh:` keys.
  Evidence: `rewrite-and-transfer-spec.md` §"Config and filesystem rewrites".

- Observation: For default and named `--instance` runs without a non-default `MJ_DATA_DIR`, SessionWiki lives outside Mjolnir's data directory at `<dirs::data_dir()>/sessionwiki/index.db` (e.g., `~/.local/share/sessionwiki/index.db` on Linux), and may be open by standalone `sessionwiki` or another Mjolnir instance (`mj-core/src/config/loading.rs:250-295`). It stores durable archives, notes, tags, and summaries.
  Evidence: `rewrite-and-transfer-spec.md` §"SessionWiki". Migrating it requires `rusqlite::backup::Backup` (already enabled via `rusqlite` `backup` feature in `Cargo.toml:103`) rather than a raw copy assuming daemon A is the only writer.

- Observation: A SQLite WAL read transaction (`BEGIN DEFERRED; SELECT 1 FROM schema_migrations LIMIT 1;`) prevents WAL reset and blocks checkpointing of frames newer than the reader's snapshot, but does not by itself prove zero bytes were backfilled into the main DB from frames committed earlier (if an older reader existed and closed mid-copy).
  Evidence: `rewrite-and-transfer-spec.md` §"Shutdown, fence, WAL and transport". Therefore the minimal-downtime fast path must never assume `mj.sqlite3` stayed unchanged: it must flush pre-pin frames, wait past filesystem timestamp granularity, verify `(inode, len, mtime_ns, ctime_ns)` and WAL salt before and after Phase 1 and post-fence, fall back automatically to 1 MiB SHA-256 block-delta patching whenever any metadata changed, and verify on B that the exact post-fence `store_fence` row is recovered after WAL replay.

- Observation: `mj-store/src/database/schema.rs:3009` declares `detached_drafts.owner_pid INTEGER CHECK(owner_pid IS NULL OR owner_pid > 0)`, so clearing A's host PID to `NULL` on B is schema-valid and preserves all draft text and revisions.
  Evidence: Direct inspection of `schema.rs:3000-3028`.

- Observation: `ProjectMemoryIdentity::for_legacy_session` (`mj-core/src/project_memory.rs:626-671`) calls local `std::fs::canonicalize` on `worktree.source_repository` even when `worktree.target` is SSH, and branches `Local` vs `Remote` on `LocalBare` vs everything else.
  Evidence: Direct inspection of `project_memory.rs:636-670`. On B, calling local `canonicalize` on A's path can hit an unrelated path/symlink on B; only `ManagedWorktreeTarget::Local` should canonicalize locally, and migration must handle legacy raw `LocalBare` memory continuity when an existing `<old-data>/projects/<old-key>/memory` directory exists.

- Observation: No active `rsync` transfer path exists anywhere in the Rust workspace (legacy rsync staging was retired in `mj-core/src/move_workspace.rs:1-6`), while `zstd`, `sha2`, `rusqlite` (`backup`), and `SupervisedChild` are already workspace dependencies.
  Evidence: `transport-and-ops.md` §2 and `Cargo.toml:103-128`.


## Decision Log

- Decision: Add a distinct typed `SshHostBare` variant to both persisted `mj_core::state::TargetLocator` (`"ssh-host-bare"`) and backend `mj_core::targets::TargetLocator` (`"ssh_host_bare"`), carrying `host`, `worker_root`, `nested_profile_root`, and `shared_profile_home`, with no workspace-deletion capability.
  Rationale: Approved by the user. Reusing `SshBare` risks `rm -rf` on user project directories, computes the wrong worker and profile roots, and loses shared-home safety. A distinct variant gives compiler exhaustiveness and isolates host-bare teardown and credential behavior cleanly.
  Date/Author: 2026-11-26 / User & Coordinator.

- Decision: Keep existing SSH sessions already running on destination host B as SSH-to-self by default rather than converting them to `Local*`.
  Rationale: Approved by the user. `SshBare` -> `LocalBare` loses owned-workspace teardown and changes profile layout/skills scope; container SSH targets on B may use a different Unix user or rootless engine than B's daemon. New sessions created on B still use `local` templates natively.
  Date/Author: 2026-11-26 / User & Coordinator.

- Decision: Scope v1 to native host-to-host Unix daemons (refusing container-hosted daemons with a clear explanation) and automatically migrate shared SessionWiki (`<dirs::data_dir()>/sessionwiki/`) on default-instance moves (via SQLite online backup, refusing to clobber an existing non-empty SessionWiki on B unless `--overwrite-sessionwiki` or `--skip-sessionwiki`). Named `--instance` moves skip external shared SessionWiki by default unless `--include-sessionwiki`.
  Rationale: Approved by the user. Container-hosted daemons keep authoritative stores inside Docker/Podman named volumes and are already refused by `mj daemon fence`. Default-instance users keep durable SessionWiki archives/notes/tags in the shared platform dir, while named test instances share that dir and must not clobber B's shared SessionWiki unexpectedly.
  Date/Author: 2026-11-26 / User & Coordinator.

- Decision: Push from host A (`mj migrate <dest>`) over a single supervised bidirectional SSH pipe talking to a hidden `mj migrate-receive` subcommand on host B, using built-in zstd + SHA-256 fixed-block (1 MiB) delta transfer and WAL pinning rather than shelling out to `rsync`.
  Rationale: Pushing from A works both when A has inbound SSH and when A is behind NAT/WSL2 or being retired. Built-in `mj` <-> `mj` streaming avoids missing/divergent `rsync` versions across Linux/macOS, integrates directly with `DataLayout`/`ConfigLayout` classification and `SshTarget`, resumes cleanly from staging on B, avoids copying 28 GiB twice locally on B during cutover, and uses existing workspace crates (`zstd`, `sha2`, `rusqlite`, `serde_json`).
  Date/Author: 2026-11-26 / Coordinator.

- Decision: Advance `SCHEMA_VERSION` to 163 (breaking, floor 163) to rebuild `session_targets` and `environment_workers` without kind-dependent SQLite `CHECK` constraints, add `nested_profile_root BLOB` and `shared_profile_home INTEGER`, and bump `mj_client::daemon::PROTOCOL_VERSION` by 1.
  Rationale: Both tables currently enforce `CHECK(kind IN ...)` and kind-shape disjunctions that reject `'ssh-host-bare'`. Removing evolving enum `CHECK`s matches `.agents/docs/schema-migrations.md` and moves validation into fallible Rust decoding. Raising the store floor and `PROTOCOL_VERSION` prevents older daemons and clients from misreading `SshHostBare` records.
  Date/Author: 2026-11-26 / Coordinator.

- Decision: Leave host B's `local` target templates in `config.toml` pointing at B's `local` machine for new sessions, add `[machines.<source-id>]` for host A, and create distinct SSH target templates (e.g. `<source-id>-podman`, `<source-id>-docker`, `<source-id>-apple-container`, `<source-id>-bare`) for host A's migrated sessions and `environment_workers`.
  Rationale: `mj-daemon/src/daemon/create.rs:216-218`, `restart.rs:821-828`, and `recovery_scan.rs:382-390` look up `target_template_id` in `config.targets`. Leaving migrated A sessions pointing at `"local-podman"` would make sub-agent spawns and recovery scans target B instead of A. Creating distinct A templates keeps B's `"local-*"` clean for new B sessions while keeping migrated A sessions bound to A.
  Date/Author: 2026-11-26 / Coordinator.

- Decision: Keep `SshHostBare` `skills_scope()` as `Isolated` and exclude `SshHostBare` from orphan recovery scans (matching `LocalBare`'s existing rule).
  Rationale: The user accepted in prep that local-bare sessions on A lose a working `mj` CLI pointing at A's fenced store, so localhost daemon tooling no longer applies. Orphan recovery cannot safely infer arbitrary user project directories from bare worker roots.
  Date/Author: 2026-11-26 / Coordinator.

- Decision: Detach Codex's file-only `<worker_root>/profile/auth.json` symlink into a regular `0600` file on A only after A is fenced (daemon A stopped) and only after proving `<worker_root>/profile` itself is a real owned directory via `symlink_metadata`.
  Rationale: Performing detachment after fencing A eliminates any concurrent login-refresh race on A's daemon while leaving the symlink's original target untouched. If migration rolls back before B unfences, a regular `auth.json` file remains valid for A's own relay credential sync.
  Date/Author: 2026-11-26 / Coordinator.

- Decision: Keep all migration code inside existing crates (`mj-store`, `mj-controller`, `mj-cli`, `mj-core`, `mj-targets`, `mj-worker-launch`, `mj-session-runtime`, `mj-client`); do not create a new workspace crate.
  Rationale: Follows `AGENTS.md` crate-creation guidance and respects the existing layering (`mj-store` owns SQLite schema and table rewrites, `mj-controller` owns preflight/rewriting/transfer logic, `mj-cli` owns CLI/daemon lifecycle locking).
  Date/Author: 2026-11-26 / Coordinator.


## Outcomes & Retrospective

Not yet started. Will be updated at the completion of each milestone and at final completion.


## Context and Orientation

Mjolnir splits its architecture into a control plane (the daemon `mj daemon-run`, backed by SQLite store `mj.sqlite3` and `config.toml`) and a data plane (one `mj-worker` process per session, binding `<worker_root>/control.sock`). Workers never initiate connections to the daemon; the daemon always dials outward via local exec, container exec, or SSH (`hel worker proxy --root <worker_root>`). Because workers retain their own relay journals and pending turns across daemon downtime, stopping the daemon on host A and starting a daemon on host B with the copied and retargeted store lets host B reconnect to running workers automatically.

Prep work already committed on `origin/master` (documented in `.agents/plans/daemon-migration-prep.md`) established four foundations:

1. Path manifest (`mj-core/src/config/layout.rs`): `DataLayout` and `ConfigLayout` own every path under `data_dir()` and `config_dir()`, enforced by a `clippy.toml` `disallowed_methods` ban. `classify_data_entry` and `classify_config_entry` classify every entry as `Known(MigrateAction)` (`Copy`, `KeepWithLocalWorkers`, `Recreate`, `Skip`, `CopyOptional`), `NestedStore` (`instances/<name>`), or `Unknown` (copy and report, never silently drop).
2. Durable ownership identity and store fence (schema revision 162): `<data_dir>/instance-identity` holds the worker ownership stamp copied unchanged to B. `mj-store/src/database/store_fence.rs` manages the singleton `store_fence` table checked in `schema.rs::open_writable` before schema migration runs. `mj daemon fence --to <dest>` (`mj-cli/src/daemon.rs:2417-2454`) holds `daemon-start.lock`, stops the daemon, takes `ControllerStoreGuard` (`controller.lock`), and writes the fence atomically. Clients check the fence before spawning.
3. Relative checkpoint archive paths (schema revision 160): managed archives in `session_checkpoints.archive_path` are stored as bare filenames relative to `<db_parent>/sessions/` and resolve automatically under B's data directory.
4. Placement machine identity (schema revision 161): `session_targets.machine_id` and `environment_workers.machine_id` carry a validated `MachineId` (`mj-core/src/config/machines.rs`), with `'local'` backfilled for local kinds and SSH rows backfilled where the template proved the host/user/port.

Key files and terms used in this plan:

- Persisted `TargetLocator` (`mj-core/src/state.rs:963-1047`): serde internally tagged with `kind` in `kebab-case`, stored in `session_targets` and `environment_workers`, embedded in `SessionRecord` and `retained_move_sources.source_json`.
- Backend `TargetLocator` (`mj-core/src/targets.rs:4164-4248`): serde internally tagged with `kind` in `snake_case`, carrying resolved `SshTarget` connection options; built from persisted locator plus `TargetRuntimeSettings` in `mj-core/src/targets/convert.rs:204-271`.
- `TargetRuntimeSettings` (`mj-core/src/state/target_runtime.rs`): frozen per-session/per-lease runtime snapshot containing `kind: String` (e.g. `"local-bare"`, `"ssh-podman"`), `TargetConnection`, environment references, and settings.
- `ManagedWorktree` / `ManagedWorktreeTarget` (`mj-core/src/state.rs:1049-1235`): explicit record of a session-owned git clone or worktree (separate from user-owned attached project directories), carrying `Local` or `Ssh { destination, ssh_args }`.
- `ControllerStoreGuard` (`mj-controller/src/controller.rs:161-254`): exclusive process lock on `<data_dir>/controller.lock` guarding all store writes.
- `DaemonStartGuard` (`mj-cli/src/daemon.rs:193-220`): exclusive lock on `<data_dir>/daemon-start.lock` serializing daemon stop, start, restart, fence, and unfence.
- `SupervisedChild` (`mj-core/src/subprocess.rs:247-320`): process-group-managed async child wrapper ensuring clean termination on drop or cancellation.
- `SessionWiki`: separate SQLite index (`index.db` in WAL mode) holding archived transcripts, notes, tags, and summaries. Unless `MJ_DATA_DIR` is overridden to a non-default path or `SESSIONWIKI_DATA` is set, Mjolnir resolves it to `<dirs::data_dir()>/sessionwiki/index.db` (`mj-core/src/config/loading.rs:250-295`).

`go/` is out of scope (`AGENTS.md`). Never touch the default instance store during development or testing; always run tests in isolated temporary directories or named `--instance <test-name>` instances and stop test daemons afterward. Run all `cargo test` commands outside the restricted sandbox.


## Plan of Work

Four sequential milestones. Validate, run `request_review`, and commit each milestone before starting the next.


### Milestone 1: `SshHostBare` locator, schema revision 163, and protocol bump

Goal: represent a host-local-bare session on host A reached over SSH from host B with exact A worker/profile roots, zero capability to delete a user project directory, safe shared-profile-home handling, and Rust-enforced target validation replacing SQLite's evolving `CHECK` constraints.

1. Core persisted and backend locators (`mj-core`):
   - In `mj-core/src/state.rs`, add `TargetLocator::SshHostBare`:

         SshHostBare {
             host: String,
             worker_root: PathBuf,
             nested_profile_root: PathBuf,
             #[serde(default)]
             shared_profile_home: bool,
         }

     Implement validation in `verify_locator` (`state.rs:1383-1457`): `host` must be non-empty and not start with `-`; `worker_root` and `nested_profile_root` must both be absolute paths, contain no `..` (`Component::ParentDir`), and have their final path component equal to `session_id`. Classify `skills_scope()` as `SkillsScope::Isolated`. Update `same_resource` (`state.rs:3811-3910`) so two `SshHostBare` locators refer to the same resource when their `host` and `worker_root` match (ignoring `nested_profile_root` and `shared_profile_home`). Update `target_runtime_settings_for` and `ManagedWorktreeTarget` helpers as needed.
   - In `mj-core/src/targets.rs`, add backend `TargetLocator::SshHostBare { ssh: SshTarget, worker_root: PathBuf, nested_profile_root: PathBuf, #[serde(default)] shared_profile_home: bool }`. Update `worker_root()` (`targets.rs:4600-4618`) to return `worker_root.clone()` verbatim (never `.local/share/hel/workers/...`), and `locator_command` (`targets.rs:4553-4598`) to wrap remote commands via `ssh_command(class, ssh, ...)`.
   - In `mj-core/src/targets/convert.rs`, map `"ssh-host-bare"` in `locator_kind_name` and `RecordedTarget` conversion: require `runtime.kind == "ssh-host-bare"` and `TargetConnection::Ssh { ssh }` with `ssh.host == host`.
   - In `mj-core/src/targets/ssh.rs`, add `SshHostBare` validation matching the state checks (non-empty non-option destination, absolute no-`..` session-ID-ending `worker_root` and `nested_profile_root`), and report `is_borrowed() == false`.
   - In `mj-core/src/project_memory.rs:636-670`, fix `for_legacy_session` so `std::fs::canonicalize` is called ONLY when `worktree.target` is `ManagedWorktreeTarget::Local` (for `Ssh`, preserve `worktree.source_repository` verbatim without touching the local filesystem). For raw sessions with `SshHostBare`, fall into the `Remote` branch (migration handles any existing legacy memory directory continuity in Milestone 2).

2. SQLite schema revision 163 and store encoding (`mj-store`):
   - Advance `SCHEMA_VERSION` in `mj-store/src/database.rs` from 162 to 163.
   - In `mj-store/src/database/schema.rs`, add migration 163 (breaking, compatibility floor 163 — older binaries cannot decode `'ssh-host-bare'` and older `session_targets` delete+insert writers would drop the new columns). Follow migration 124's exact pattern (`schema.rs:4026-4095`):
     - Turn `PRAGMA foreign_keys = OFF` outside the transaction with an RAII guard that unconditionally restores `PRAGMA foreign_keys = ON` on every exit path.
     - Inside an `IMMEDIATE` transaction, rebuild `session_targets` and `environment_workers` WITHOUT `CHECK(kind IN (...))` and WITHOUT the kind-shape disjunction `CHECK(...)`, preserving all existing columns including `machine_id`, all PK/FK constraints, `STRICT`, and `CHECK(runtime_json IS NULL OR json_valid(runtime_json))`, and adding `nested_profile_root BLOB` and `shared_profile_home INTEGER` columns to both tables.
     - Recreate BOTH indexes on `environment_workers`: `environment_worker_identity ON environment_workers(worker_session_id)` and `environment_workers_environment ON environment_workers(environment_id)`.
     - Verify `has_foreign_key_violation_for_tables(&transaction, &["session_targets", "environment_workers"])` is false.
     - Update `schema_compatibility.minimum_compatible_version = 163`, insert revision 163 into `schema_migrations`, and set `user_version = 163` in the same transaction.
   - In `mj-store/src/database/state_io.rs`:
     - Extend `StoredTargetValues` with `nested_profile_root: Option<Vec<u8>>` and `shared_profile_home: Option<i64>`.
     - Encode `SshHostBare` as `kind = "ssh-host-bare"`, `host = Some(host)`, `workspace = Some(path_to_blob(worker_root))`, `nested_profile_root = Some(path_to_blob(nested_profile_root))`, `shared_profile_home = Some(if shared_profile_home { 1 } else { 0 })`, and all other locator columns `NULL`.
     - Replace every `unwrap()` and `unreachable!` in `decode_target` with fallible `Result` validation returning clear errors on unknown kinds or missing/invalid required columns (including requiring `nested_profile_root` and `shared_profile_home` in `{0, 1}` for `"ssh-host-bare"`, rejecting non-null unexpected columns where appropriate).
     - Add `"ssh-host-bare"` to `REMOTE_TARGET_KINDS`.
   - In `mj-store/src/database/environments.rs`:
     - Update live lease reads/writes to include `nested_profile_root` and `shared_profile_home`.
     - Preserve the historical pre-161/pre-163 column projection helpers used by revision-122 backfills (passing `NULL, NULL` for the two new columns when reading pre-163 schemas so upgrading from older snapshots never fails).

3. Target operations, cleanup, profile home, credentials, and exhaustive callers (`mj-targets`, `mj-worker-launch`, `mj-session-runtime`, `mj-worker-staging`, `mj-controller`, `mj-daemon`, `mj-server`, `mj-import`, `mj-build-cache`, `mj-sessionwiki`, `mj-tui`, `mj-cli`):
   - `mj-targets/src/targets/profile_home.rs`: for `SshHostBare`, return `<worker_root>/profile` for non-nested harnesses and `<nested_profile_root>/<harness-id>` for nested harnesses.
   - `mj-targets/src/targets/cleanup.rs`: for `SshHostBare`, first stop the worker over SSH, re-validate `worker_root` and `nested_profile_root` (absolute, no `..`, ending in `session_id`), and execute `rm -rf -- <worker_root> <nested_profile_root>` over SSH. Structurally never touch or accept a project/checkout path.
   - `mj-targets/src/targets/session_target.rs` and `recovery.rs`: `SshHostBare` connects via `<worker_root>/hel worker proxy --root <worker_root>` over SSH; `workspace_root` mirrors `LocalBare` (project directory comes from the session checkout record).
   - `mj-worker-launch/src/local_profile_homes.rs` and `mj-session-runtime/src/worker_client/credential_targets.rs`: for `SshHostBare`, `has_staged_profile_home` returns `!shared_profile_home` without inspecting the local filesystem on B; `shares_local_profile_login` returns `shared_profile_home` (excluding whole-directory symlinked personal homes from login, skills, and GitHub sync, and ensuring `mj-controller/src/controller/resume/in_place.rs` respects the same predicate).
   - `mj-controller/src/controller/recovery_scan.rs`: treat `SshHostBare` like `LocalBare` (no orphan scan candidates; never infer user project directories from bare worker roots).
   - `mj-controller/src/controller/resume.rs`: keep `SshHostBare`'s absolute paths verbatim without prepending `~/`.
   - `mj-controller/src/controller/worktree.rs`: allow existing non-Git plain-directory checkouts on `SshHostBare` just like `LocalBare`.
   - `mj-worker-staging/src/worker_binary/launch.rs` and all remaining exhaustive match sites across the workspace (identified by `cargo check --workspace --all-targets`): wire `SshHostBare` cleanly, keeping SSH execution transport with host-bare layout semantics.
   - `mj-client/src/daemon.rs:2363`: bump `PROTOCOL_VERSION` from `PRE_HTTP_SESSION_ACTIONS_PROTOCOL + 6` to `PRE_HTTP_SESSION_ACTIONS_PROTOCOL + 7` with a doc comment noting `SshHostBare` wire compatibility. Update protocol-version unit assertions if any hardcode `+ 6`.

Result: a store at revision 162 upgrades cleanly to 163, persists and decodes `SshHostBare` in both placement tables, rejects malformed target rows cleanly in Rust, refuses to delete user checkouts during `SshHostBare` cleanup, skips credential sync when `shared_profile_home == true`, and compiles cleanly with `cargo clippy --workspace --all-targets -- -D warnings`.


### Milestone 2: Preflight inspector, atomic store/config rewriter, and Codex symlink detachment

Goal: build and thoroughly test the pure-Rust migration planning, precondition checking, and offline state-rewriting engine before adding network transfer or CLI orchestration.

1. Store-level migration queries and atomic rewrites (`mj-store/src/database/migrate.rs`, exported from `mj-store/src/database.rs`):
   - `MigrationPreconditions` query helper (`inspect_migration_preconditions(conn) -> Result<MigrationPreconditions>`): queries and returns every blocking row in `session_moves`, `session_restart_intents`, `worker_restart_intents`, `checkpoint_operations`, `session_create_intents`, `sessions` where `state IN ('provisioning','startup-cleanup','checkpointing','closing','destroying')`, and reports non-active `environments` phases (`retiring` vs `reclaimed`).
   - Placement & inventory queries (`inspect_migration_inventory(conn) -> Result<StoreMigrationInventory>`):
     - Counts and lists every `session_targets` and `environment_workers` row grouped by kind (`LocalBare`, `LocalPodman`, `LocalDocker`, `AppleContainer`, `Ssh*`, `AwsEc2`), distinguishing active/live sessions (`running`, `disconnected`) from historical (`stopped`, `parked`, `lost`, `error`, `destroyed-with-data-loss`).
     - Lists every placement with `machine_id IS NULL` (including `session_id`, `kind`, `host`, `target_template_id`, and whether frozen runtime references an `ec2/` path) so preflight reports them clearly.
     - Lists `retained_move_sources`, `host_snapshot_cache` entries, `host_container_sizes`, `mount_history`, `project_locations`, `detached_drafts` count, and managed `session_checkpoints` archive filenames.
   - `MigrationRewriteSpec`: typed input specifying `source_machine_id: MachineId`, `source_ssh: SshConnection` (or retired synthetic host when A is being retired with no live local sessions), `source_templates: BTreeMap<String, String>` (mapping old local `target_template_id` -> new A SSH template ID), `local_bare_profile_roots: BTreeMap<String, LocalBareMigrationInfo>` (`nested_profile_root: PathBuf`, `shared_profile_home: bool`), `old_config_root: PathBuf`, `new_config_root: PathBuf`, `old_data_root: PathBuf`, `new_data_root: PathBuf`, `expected_fence: StoreFenceRecord`, and `keep_source_cache: bool`.
   - `apply_store_migration(db_path: &Path, spec: &MigrationRewriteSpec) -> Result<StoreRewriteSummary>`:
     - Opens `db_path` via `open_writable_ignoring_fence`, verifies `read_store_fence` returns `Some(fence)` matching `spec.expected_fence` (`destination`, `fenced_at`, `fenced_host`, `fenced_data_dir`) — proving WAL recovery reached the final post-fence commit.
     - Executes all table updates in ONE SQLite `IMMEDIATE` transaction:
       - `session_targets`: rewrite `LocalBare` -> `SshHostBare` (using captured `nested_profile_root` and `shared_profile_home`, setting `host = source_ssh.host`, `machine_id = source_machine_id`); rewrite `LocalPodman` -> `SshPodman`, `LocalDocker` -> `SshDocker`, `AppleContainer` -> `SshAppleContainer` (preserving `resource_id`, `workspace_storage`, `borrowed_from`, setting `host = source_ssh.host`, `machine_id = source_machine_id`). Leave existing `Ssh*` (including SSH-to-B) and `AwsEc2` rows untouched.
       - `environment_workers`: apply the exact same locator + `machine_id` conversion to EVERY lease row (including non-current/retained leases), update `target_template_id` via `source_templates`, and rewrite `runtime_json` (converting local `kind` and `TargetConnection::Local` to the corresponding SSH `kind` and `TargetConnection::Ssh`, plus relocating `<old_config_root>/ec2/` paths on all rows).
       - `sessions`: update `target_template_id` for migrated A rows; rewrite `target_runtime_json` (for migrated A rows: convert `kind` to `"ssh-host-bare"`/`"ssh-podman"`/`"ssh-docker"`/`"ssh-apple-container"` and `connection` to `TargetConnection::Ssh { ssh: source_ssh }`; for ALL rows: rewrite `<old_config_root>/ec2/` paths in `identity_file` and `extra_args`); rewrite `managed_worktree` JSON converting `ManagedWorktreeTarget::Local` -> `ManagedWorktreeTarget::Ssh { destination, ssh_args }` while preserving `source_project_directory`, `source_repository`, `worktree_root`, `kind`, `branch`, `base_commit`.
       - `environments.placement_json`: parse typed JSON and rewrite embedded `runtime`, `prepared_destination`, and `managed_worktree` descriptors consistently with their lease and EC2 path relocation.
       - `retained_move_sources.source_json`: parse typed `SessionRecord` and rewrite embedded `target`, `target_runtime`, `managed_worktree`, EC2 paths, and any checkpoint archive path directly under `<old_data_root>/sessions/` to `<new_data_root>/sessions/`.
       - `host_snapshot_cache`: for `host = 'local'`, re-key to `ssh:<serialized SshTarget for A>` when `keep_source_cache` is true, or delete rows when A's local snapshot cache was purged/retired; for existing `ssh:...` keys, parse the JSON `SshTarget`, rewrite any `<old_config_root>/ec2/` paths in `ssh_args`, and re-serialize.
       - `host_container_sizes`, `mount_history`, and `project_locations`: remap `host = 'local'` (and `mount_history` `host = 'project:local'`) to `source_ssh.host` (and `project:<source_ssh.host>`), preserving existing destination rows on primary-key/unique collision rather than overwriting.
       - `detached_drafts`: set `owner_pid = NULL` (preserving all draft text, revisions, and recovery state).
     - Verifies every managed relative `session_checkpoints.archive_path` exists under `<new_data_root>/sessions/` (or staged `sessions/`).
     - Runs `PRAGMA wal_checkpoint(TRUNCATE)` and clears `store_fence` ONLY when `unfence: bool` is set after all rewrites and config validation succeed.

2. Controller-level preflight, config/file rewriter, and Codex symlink detachment (`mj-controller/src/migrate/`):
   - `mj-controller/src/migrate/mod.rs`, `preflight.rs`, `rewrite.rs`:
   - Preflight inspector on host A (`build_source_preflight`):
     - Refuses container-hosted daemon placement (`DataLayout::daemon_container()` exists or `config.daemon` selects container runtime).
     - Checks `config-rename.json` presence (refuses unfinished config rename with instructions to let daemon finish/recover it first).
     - Inspects `<config_dir>/ec2/*/resources.json` and refuses any record with `state == "preparing"` or malformed JSON.
     - Inspects `DataLayout` and `ConfigLayout` directories using `classify_data_entry` and `classify_config_entry`: records exact `Copy` file list, total byte sizes, `CopyOptional` sizes, `Unknown` entries (to be copied and reported), and `NestedStore` entries (skipped and reported).
     - Inspects every `LocalBare` session's `<worker_root>/profile` on A via `std::fs::symlink_metadata`:
       - If symlink or missing: marks `shared_profile_home = true`.
       - If real directory: marks `shared_profile_home = false`, and checks `symlink_metadata(<worker_root>/profile/auth.json)` to record Codex auth symlinks eligible for post-fence atomic detachment.
     - Collects external dependency warnings/checks: non-managed profile homes (`home != managed_profile_home_in(config_root, id)`), `ProjectRepository.local` bundle paths, `go.json` host paths, `{ from_env = ... }` secret variable names (without reading or printing secret values!), AWS profile names referenced by EC2/Bedrock config, external SSH `identity_file` paths outside `ec2/`, and macOS Keychain logins.
     - Resolves SessionWiki location using `mj_core::config::loading`'s exact rule (recording whether it is inside `data_dir` or the external shared `<dirs::data_dir()>/sessionwiki/`).
   - Config & auxiliary file rewriter on host B (`rewrite_staged_config_and_files`):
     - Loads `stage/config/config.toml` with `Config::load_from` (resolving staged `secrets.toml` beside it).
     - Adds `[machines.<source-machine-id>]` (`Machine::Ssh`) with A's verified `SshConnection` (moving A's former `[machines.local].build_cache` to A's machine and clearing B's `[machines.local].build_cache` so B does not inherit A's local build cache path). Validates no machine ID or SSH connection collision with existing machines in `config.toml`.
     - Leaves B's `local` target templates intact for new B sessions; creates distinct SSH target templates for each migrated A local runtime kind used by sessions or environment leases (preserving container/runtime parameters while targeting `<source-machine-id>`).
     - Rewrites each `HarnessProfile.home` ONLY when `home == profile.kind.managed_profile_home_in(old_config_root, id)` -> `profile.kind.managed_profile_home_in(new_config_root, id)`.
     - Component-aware rewrites every `<old_config_root>/ec2/...` path in `Machine::Ssh` (`identity_file` and `extra_args` `-i` / `UserKnownHostsFile=...`) and `Machine::AwsEc2` (`identity_file` and `ssh_args`) to `<new_config_root>/ec2/...`.
     - Sanitizes `[daemon]` in B's `config.toml` so B runs a local native daemon instead of pointing back at A.
     - Validates via `Config::validate()` and saves with `Config::save_to`.
     - Rewrites `stage/config/go.json` if present (updating renamed target IDs only if needed; leaving host paths intact for reporting).
     - Handles legacy raw `LocalBare` project-memory continuity: for any legacy `LocalBare` session without `session.project` whose pre-migration key directory `<stage/data/projects>/<old_key>/memory` exists, copies/preserves it under `<stage/data/projects>/<new_key>/memory` if `<new_key>` does not already exist (never clobbering existing memory).
   - Post-fence Codex symlink detachment (`detach_codex_auth_symlink(worker_root: &Path) -> Result<bool>`):
     - Verifies `symlink_metadata(worker_root.join("profile"))` is a real directory (`file_type().is_dir()`, NOT symlink).
     - Checks `symlink_metadata(worker_root.join("profile/auth.json"))` is a symlink.
     - Reads target file bytes, verifies valid JSON, creates a mode-`0600` temporary regular file inside `worker_root.join("profile")`, writes + `sync_all`, and atomically renames over `auth.json` (replacing only the symlink, never modifying its target).

Result: full integration tests in `mj-store` and `mj-controller` prove every DB table rewrite, EC2 key/`UserKnownHostsFile` relocation, managed vs unmanaged profile home rewrite, collision-safe host table remapping, blocking precondition refusal, fence verification, legacy project-memory preservation, and safe Codex symlink detachment.


### Milestone 3: Minimal-downtime two-phase streaming transfer engine

Goal: implement a pure-Rust, resumable, SHA-256-verified bidirectional streaming protocol over a single supervised SSH connection (`mj migrate` on A <-> `mj migrate-receive` on B) that copies multi-gigabyte state live during Phase 1 and cuts over in seconds during Phase 2 with guaranteed consistency and safe rollback.

1. Supervised full-duplex SSH subprocess helper (`mj-core/src/subprocess.rs`):
   - Add a reusable `SupervisedPipeChild` (built alongside `SupervisedChild`, using `spawn_async_child`, Unix `process_group(0)` / `ProcessGroupGuard`, `kill_on_drop(true)`, piped `stdin`/`stdout`, and concurrent background `stderr` draining) so callers can exchange framed messages over SSH `stdin`/`stdout` with bounded memory and guaranteed process-group teardown on cancellation or error, never hand-rolling raw `std::process` pipe handling.

2. Framed streaming protocol (`mj-controller/src/migrate/protocol.rs`):
   - Length-prefixed frames (`u32` big-endian length + frame type tag + payload, max frame 8 MiB, streaming chunk size 1 MiB, optional `zstd` level 1 compression per chunk):
     - `Hello` / `HelloAck`: exchanges `mj` version, `SCHEMA_VERSION`, `PROTOCOL_VERSION`, OS family (`"unix"`), target `data_dir`, `config_dir`, `sessionwiki_dir`, whether destination already contains `mj.sqlite3`/`config.toml`/container daemon/non-empty SessionWiki, and available disk space on B's target filesystems. Receiver acquires `<B-data-dir>.migrate-stage.lock` and prepares staging dirs `<B-data-dir>.migrate-stage` and `<B-config-dir>.migrate-stage` (and `<B-sessionwiki-dir>.migrate-stage` when migrating external SessionWiki) on the same parent directories as their final targets.
     - `ProbeSsh` / `ProbeSshAck`: asks B to run `mj_core::targets::ssh::ssh_probe_command` back to A (`--source-host`) to verify SSH reachability, remote `id -un`, readability of A's `instance-identity` file matching A's exact token, and (when local containers exist) matching container engine availability (`podman`/`docker`/`container`). Also supports optional non-blocking reachability probes from B to third-host SSH machines C for the preflight report.
     - `StageManifest` / `StageManifestAck`: B reports existing staged regular files `(rel_path, size, sha256)` and 1 MiB block SHA-256 digests of any existing `stage/data/mj.sqlite3` and `stage/sessionwiki/index.db` so interrupted Phase 1 transfers resume without re-sending unchanged blocks.
     - `PutFileStart` / `FileChunk` / `PutFileEnd`: streams a file (zstd-compressed, preserving Unix mode bits such as `0600`) to a temp file in staging, verifies whole-file SHA-256, fsyncs, and atomically renames into staging.
     - `AppendTail`: verifies existing staged prefix length and SHA-256 of the trailing 64 KiB (or whole prefix if smaller) of an append-only `tool-details/*.mjtd` file and streams only the newly appended bytes, verifying the new total length and tail digest.
     - `PatchBlocks`: updates a staged DB file (`mj.sqlite3` or `sessionwiki/index.db`) in place at 1 MiB block offsets (each block zstd-compressed + SHA-256 verified) plus final `set_len(total_size)`.
     - `RemoveFile`: removes a file deleted between Phase 1 and Phase 2 from staging.
     - `ApplyAndActivate` / `ApplyAndActivateAck`: carries `MigrationRewriteSpec` and `start_daemon: bool`. Receiver runs `apply_store_migration` and `rewrite_staged_config_and_files` inside staging, verifies the exact `store_fence` row and managed checkpoints, checkpoints WAL, clears `store_fence`, atomically moves staged entries into B's final `data_dir`/`config_dir` (and `sessionwiki_dir`), removes staging, optionally starts B's daemon and verifies health, and returns `ActivationReport` (or structured error carrying `unfenced_or_published: bool`).

3. Two-phase WAL-pinned sender and receiver (`mj-controller/src/migrate/transfer.rs`):
   - **Phase 1 (daemon A LIVE, 0s downtime):**
     - Sync all `Copy` (plus optional `CopyOptional` and `Unknown`) files from `DataLayout` and `ConfigLayout` (excluding `mj.sqlite3`, `mj.sqlite3-wal`, `mj.sqlite3-shm`, and `sessionwiki/index.db` which have dedicated DB sync steps), skipping files already matching in B's `StageManifest` and using `AppendTail` for `.mjtd` files that grew.
     - Open read-only connection `R_pin` on A's `mj.sqlite3` (`BEGIN DEFERRED; SELECT 1 FROM schema_migrations LIMIT 1;`), verify `PRAGMA journal_mode` is `wal`, run `PRAGMA wal_checkpoint(PASSIVE)` on a separate connection to flush pre-pin frames, sleep 25 ms so any subsequent write advances `mtime_ns`/`ctime_ns`, and record `stat_before = (ino, len, mtime_ns, ctime_ns)` plus `wal_header_before` (first 32 bytes of `mj.sqlite3-wal` if present).
     - Stream `mj.sqlite3` in 1 MiB blocks to B (resuming unchanged blocks from `StageManifest`), retaining the `Vec<[u8; 32]>` block SHA-256 vector in memory (~896 KiB for 28 GiB).
     - Post-Phase-1 live convergence check: re-stat `mj.sqlite3`. If `stat_mid != stat_before`, send changed 1 MiB blocks via `PatchBlocks` while A is still live and refresh `stat_before` and `wal_header_before`.
     - If migrating SessionWiki: snapshot `index.db` on A into a temporary file via `rusqlite::backup::Backup` (stepping 256 pages at a time with 10 ms sleep on busy) and sync it to B using 1 MiB `PatchBlocks`, then remove the temp snapshot.
   - **Phase 2 (atomic cutover — the only downtime window):**
     - Caller (`mj-cli`) acquires `DaemonStartGuard`, stops daemon A cleanly via `stop_instance_holding` (while keeping `R_pin` open), and acquires `ControllerStoreGuard`.
     - Recheck authoritative preconditions under lock. If any blocking operation started during Phase 1: release `ControllerStoreGuard`, close `R_pin`, restart daemon A automatically, release `DaemonStartGuard`, and fail cleanly with the blocking details.
     - Write `store_fence` on A (`destination`, UTC `fenced_at`, host, data dir, optional note) while `R_pin` is still open.
     - Re-stat `mj.sqlite3` and read `wal_header_after`:
       - **Fast path**: if `stat_after == stat_before` AND WAL mode holds AND WAL salt is unchanged (or newly created from empty WAL), skip reading/sending `mj.sqlite3` completely (0 bytes of `mj.sqlite3` read or sent during downtime).
       - **Exact fallback path**: if `stat_after != stat_before` or WAL salt changed for any reason, compute 1 MiB block SHA-256s against the in-memory vector and send only differing blocks via `PatchBlocks`.
     - Send `mj.sqlite3-wal` in full (zstd-compressed, SHA-256 verified) if present; never send `mj.sqlite3-shm`.
     - Delta-sync any files in `tool-details/` (`AppendTail`), `sessions/`, `projects/`, `config/`, `profiles/`, `ec2/` that changed, were added, or were removed since Phase 1.
     - If migrating SessionWiki: run one final fast `rusqlite::backup::Backup` snapshot and `PatchBlocks` delta to B.
     - Close `R_pin` on A.
     - If keeping A's local-bare sessions reachable over SSH: run `detach_codex_auth_symlink` for every eligible Codex session identified on A.
     - Send `ApplyAndActivate` to B.
     - **Rollback invariant**:
       - If any error occurs before sending `ApplyAndActivate`, OR if B returns `ApplyAndActivateAck::Failed { unfenced_or_published: false, .. }`: clear `store_fence` on A while still holding `DaemonStartGuard` and `ControllerStoreGuard`, release `ControllerStoreGuard`, restart daemon A, release `DaemonStartGuard`, and return the error.
       - If SSH disconnects or B returns `unfenced_or_published: true`: NEVER clear `store_fence` on A! Release locks leaving A permanently fenced and instruct the user to check `mj daemon status` on B before taking any manual action.

Result: tricky-condition tests prove (1) WAL-pinned base + post-fence WAL copy under heavy concurrent writes (>1 MiB) recovers all committed rows and the exact `store_fence` row with `PRAGMA integrity_check == ok`; (2) `PatchBlocks` fallback repairs any mid-Phase-1 modification to `mj.sqlite3` identically; (3) framed streaming over real pipes with >1 MiB payloads verifies `PatchBlocks`, `AppendTail`, resume after partial copy, and SHA-256 corruption rejection.


### Milestone 4: User-facing `mj migrate` CLI, hidden receiver, and localhost-`sshd` end-to-end tests

Goal: expose `mj migrate` and hidden `mj migrate-receive` in `mj-cli`, wire full preflight/progress/summary output, and verify the entire flow end-to-end with the real `mj` binary over a real localhost `sshd`.

1. CLI definitions and dispatch (`mj-cli/src/main.rs`, `mj-cli/src/migrate.rs`):
   - Add `Migrate(MigrateArgs)` to top-level `Command` in `mj-cli/src/main.rs` and hidden `#[command(hide = true)] MigrateReceive(MigrateReceiveArgs)`:
     - `MigrateArgs`:
       - `destination: String` — configured `[machines.<id>]` name or SSH destination (e.g., `b` or `user@host`).
       - `--source-host <SSH_DEST>` — how destination B reaches host A over SSH (required when live local sessions exist on A unless `--disconnect-local`).
       - `--source-machine-id <ID>` — machine ID to create for host A on B (defaults to a sanitized valid `MachineId` derived from A's hostname, disambiguated if already present in `config.toml`).
       - `--source-ssh-arg <ARG>` — repeatable extra SSH args for B -> A.
       - `--ssh-arg <ARG>` — repeatable extra SSH args for A -> B (when `destination` is not a configured machine).
       - `--dest-instance <NAME>` — target instance name on B (defaults to current instance; pass `default` or omit for default).
       - `--dest-data-dir <PATH>` / `--dest-config-dir <PATH>` — explicit destination store path overrides on B (passed to remote `mj migrate-receive` for custom layouts and isolated testing).
       - `--remote-command <CMD>` — remote `mj` command on B (default `"mj"`).
       - `--dry-run` — run complete preflight, disk, version/schema, B -> A SSH probe, and external-dependency audit, print the structured migration plan, and exit 0 without modifying A or B.
       - `--include-logs` — include `CopyOptional` entries (`daemon.log`, `logs/`, `diagnostics/`, `jev-decisions/`).
       - `--include-sessionwiki` / `--skip-sessionwiki` / `--overwrite-sessionwiki` — control SessionWiki migration policy.
       - `--disconnect-local` — allow retiring A without `--source-host` when live local sessions exist on A.
       - `--no-start` — leave B unfenced and activated without starting B's daemon immediately.
   - Destination resolution: if `destination` matches a `Machine::Ssh` ID in A's `config.toml`, reuse its `SshConnection` (combining any `--ssh-arg`); otherwise treat it as an SSH destination string (validating it does not start with `-`).
   - Print clear phase-by-phase progress to stderr (preflight summary, Phase 1 live pre-sync bytes/files, Phase 2 cutover start + downtime elapsed milliseconds, B activation status, and final summary naming any unknown copied files, non-managed homes, local repo paths, external env vars, AWS profiles, and NULL `machine_id` rows).
   - Ensure `mj migrate-receive` respects `ensure_may_control_store()` so binaries run from a Cargo `target/` directory never accidentally touch the host's default store during testing.

2. End-to-end real-binary verification (`mj-cli/tests/migrate_e2e.rs`):
   - Reuse the ephemeral localhost `sshd` fixture pattern (`mj-core/src/targets/ssh.rs:3256-3345` / `tests/e2e/ssh_remote_daemon.py`) with isolated temporary `MJ_DATA_DIR` and `MJ_CONFIG_DIR` for host A and host B, running the real `mj` binary (`CARGO_BIN_EXE_mj`) as both sender and remote `--remote-command`.
   - Test 1 (Full happy path + `--dry-run`):
     - Start real daemon A with: a suspended session + managed `.hel.zip` checkpoint archive; two `LocalBare` sessions (one with real `profile/` + Codex `auth.json` symlink, one with whole-directory `profile` symlink); a `LocalPodman` placement + `environment_workers` lease; an `ec2/` operation record + key + `UserKnownHostsFile` referenced in config and frozen runtime; a managed profile home + an external unmanaged profile home; `detached_drafts` with `owner_pid`; `host_snapshot_cache` `local` row + `host_container_sizes` + `mount_history` + `project_locations`; an unknown file in `data_dir`; and a SessionWiki `index.db`.
     - Run `mj migrate --dry-run`: assert exit 0, plan mentions every item, daemon A still running unfenced, B untouched.
     - Run `mj migrate` with `--source-host` targeting the same localhost `sshd`:
       - Assert A is stopped and permanently fenced (`mj daemon status` on A shows moved-to-B fence; client command on A refused before spawning).
       - Assert B is unfenced and running with the identical `instance-identity` token and responds healthy to `mj daemon status`.
       - Assert B's SQLite store converted `LocalBare` -> `SshHostBare` (`shared_profile_home = false` for real dir, `true` for whole-dir symlink), Codex `auth.json` on A was atomically materialized into a regular `0600` file (without modifying the symlink target), `LocalPodman` + `environment_workers` lease converted to `SshPodman` with `machine_id = <source-id>`, EC2 `identity_file` and `UserKnownHostsFile` point under B's `ec2/` dir, managed profile home points under B's `profiles/` dir while unmanaged home is untouched, `detached_drafts.owner_pid` is `NULL`, unknown file copied to B, SessionWiki `index.db` copied to B, and the suspended session's checkpoint archive is intact on B.
   - Test 2 (Rollback before B unfences): inject a blocking `session_moves` row or invalid destination conflict and verify A rolls back to running unfenced with zero data loss.
   - Test 3 (Resumable Phase 1): pre-seed a partial/stale `stage/data/mj.sqlite3` on B, run `mj migrate`, and verify B converges to a byte-verified consistent database (`PRAGMA integrity_check == ok`).

Result: end-to-end real-binary migration over real localhost `sshd` passes cleanly alongside workspace clippy and targeted crate tests.


## Concrete Steps

Run all commands from the repository root (`/home/jonathan/Projects/mjolnir/.mj/clones/9f45d4d9bc69809be6be8c90b0d03c0e`). Run every `cargo test` command outside the restricted sandbox (`BypassSandbox: true`). Always verify every filtered `cargo test` invocation reports at least one executed test.

Milestone 1 validation:

    cargo clippy --workspace --all-targets -- -D warnings
    cargo test -p brokk-mj-core target_locator
    cargo test -p brokk-mj-store schema
    cargo test -p brokk-mj-store target
    cargo test -p brokk-mj-store environment
    cargo test -p brokk-mj-targets cleanup
    cargo test -p brokk-mj-targets profile_home
    cargo test -p brokk-mj-session-runtime credential_targets
    cargo test -p brokk-mj-client daemon

Milestone 2 validation:

    cargo clippy -p brokk-mj-store -p brokk-mj-controller --all-targets -- -D warnings
    cargo test -p brokk-mj-store migrate
    cargo test -p brokk-mj-controller migrate

Milestone 3 validation:

    cargo clippy -p brokk-mj-core -p brokk-mj-controller --all-targets -- -D warnings
    cargo test -p brokk-mj-core subprocess
    cargo test -p brokk-mj-controller migrate

Milestone 4 validation:

    cargo clippy --workspace --all-targets -- -D warnings
    cargo test -p brokk-mjolnir --test migrate_e2e
    cargo test -p brokk-mjolnir --test daemon_startup

Update test filter names in this section if the final test names differ. Commit and run `request_review` after each milestone before moving to the next.


## Validation and Acceptance

Follow `AGENTS.md` "Which tests to add": integration, end-to-end, and tricky-condition tests only (no unit tests of trivial getters or happy-path defaults). Every streaming pipe test must drive >64 KiB (we use >1 MiB multi-block fixtures) per Subprocess Rules.

- Milestone 1 acceptance:
  - Integration (schema 162 -> 163): upgrading a revision-162 store with existing local/SSH rows and environment leases preserves all rows, both `environment_workers` indexes, and FK integrity; storing and loading `SshHostBare` round-trips cleanly; inserting a malformed or unknown `kind` directly in SQLite produces a fallible Rust decode error instead of panicking.
  - Tricky condition (safety): `SshHostBare` cleanup command plan deletes ONLY `worker_root` and `nested_profile_root` and rejects paths not ending in `session_id` or containing `..`, never touching the session's project directory.
  - Tricky condition (credentials): `SshHostBare { shared_profile_home: true, .. }` is excluded from credential/skills sync without inspecting the local filesystem.
- Milestone 2 acceptance:
  - Integration (store + config rewrite): rewriting a fenced revision-163 store + `config.toml` + `ec2/` + `go.json` atomically converts all A-local placements and leases, preserves SSH-to-B placements as SSH-to-self, relocates EC2 `identity_file` and `UserKnownHostsFile` across config and all JSON columns, rewrites managed homes only, clears `detached_drafts.owner_pid`, preserves existing host-table entries on collision, refuses missing/mismatched fences, and refuses every blocking in-flight table.
  - Tricky condition (symlinks): `detach_codex_auth_symlink` replaces only the `auth.json` symlink inside a real owned `profile/` directory with a mode-`0600` regular copy without modifying the symlink target, and refuses when `profile/` itself is a symlink.
- Milestone 3 acceptance:
  - Tricky condition (WAL concurrency): concurrent writes + checkpoints (>1 MiB) during Phase 1 WAL pinning followed by stop + fence commit + WAL transfer produces a consistent B store passing `PRAGMA integrity_check` with the exact `store_fence` row; forcing a main-DB write during Phase 1 triggers the `PatchBlocks` fallback and also produces a byte-identical, integrity-passing DB.
  - Tricky condition (pipes & resume): framed transfer over pipes (>1 MiB) handles `PatchBlocks`, `AppendTail`, interrupted-staging resume, and SHA-256 mismatch refusal.
- Milestone 4 acceptance:
  - End-to-end (real `mj` binary + real localhost `sshd` + isolated A/B directories): `--dry-run` leaves A and B untouched; full `mj migrate` fences A, starts B with matching `instance-identity`, converts all placements/config/EC2/SessionWiki/unknown files, and passes rollback and resume scenarios.


## Idempotence and Recovery

- `--dry-run` is strictly read-only on both A and B.
- Phase 1 writes only into `<B-data-dir>.migrate-stage` and `<B-config-dir>.migrate-stage` (protected by a staging lock), leaving B's real target directories untouched and A's daemon running normally. Re-running `mj migrate` after an interrupted Phase 1 resumes from the staging manifest without re-sending unchanged files or blocks.
- Phase 2 holds `daemon-start.lock` and `ControllerStoreGuard` on A across stop, precondition recheck, fence write, final delta sync, and remote activation. Any failure before `ApplyAndActivate` publishes/unfences on B automatically clears A's fence and restarts daemon A.
- Once B unfences/publishes (or if the SSH channel breaks mid-activation so B's state is uncertain), A's fence is NEVER cleared automatically. The operator inspects `mj daemon status` on B; if B did not activate, running `mj daemon unfence` on A is the explicit rollback.
- Schema migration 163 runs in a single `IMMEDIATE` transaction with an RAII guard restoring `PRAGMA foreign_keys = ON` on error; failed migrations roll back cleanly.


## Artifacts and Notes

Baseline at plan authoring:

    HEAD: d098d793a (origin/master)
    SCHEMA_VERSION: 162 (floor 162)
    CONFIG_VERSION: 24 (unchanged)
    PROTOCOL_VERSION: PRE_HTTP_SESSION_ACTIONS_PROTOCOL + 6

Planned schema & protocol changes:

    Schema 163 (breaking, floor 163): rebuild session_targets and environment_workers without kind-dependent CHECK constraints, add nested_profile_root BLOB and shared_profile_home INTEGER, recreate both environment_workers indexes.
    Protocol bump: PRE_HTTP_SESSION_ACTIONS_PROTOCOL + 7 (SshHostBare wire compatibility).

Read-only reconnaissance reports (in current clone's agent report directories, cited during authoring):

    locator-design.md, state-rewrite-inventory.md, transport-and-ops.md,
    ssh-host-bare-spec.md, rewrite-and-transfer-spec.md


## Interfaces and Dependencies

No new workspace crates and no new external dependencies beyond workspace crates already declared in `Cargo.toml` (`zstd`, `sha2`, `rusqlite` with `backup`, `serde`, `serde_json`, `chrono`, `tempfile`, `tokio`). Add `zstd.workspace = true` to `mj-controller/Cargo.toml` for streaming transfer compression.

In `mj-core/src/state.rs`:

    TargetLocator::SshHostBare {
        host: String,
        worker_root: PathBuf,
        nested_profile_root: PathBuf,
        #[serde(default)]
        shared_profile_home: bool,
    }

In `mj-core/src/targets.rs`:

    TargetLocator::SshHostBare {
        ssh: SshTarget,
        worker_root: PathBuf,
        nested_profile_root: PathBuf,
        #[serde(default)]
        shared_profile_home: bool,
    }

In `mj-core/src/subprocess.rs`:

    pub struct SupervisedPipeChild { /* process-group guarded async child with stdin, stdout, concurrent stderr */ }
    impl SupervisedPipeChild {
        pub fn spawn(command: &mut tokio::process::Command) -> Result<Self>;
        pub fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin>;
        pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout>;
        pub async fn wait_with_stderr(self) -> Result<(ExitStatus, Vec<u8>)>;
    }

In `mj-store/src/database/migrate.rs`:

    pub struct MigrationPreconditions { ... }
    pub struct StoreMigrationInventory { ... }
    pub struct MigrationRewriteSpec { ... }
    pub struct StoreRewriteSummary { ... }
    pub fn inspect_migration_preconditions(connection: &Connection) -> Result<MigrationPreconditions>;
    pub fn inspect_migration_inventory(connection: &Connection) -> Result<StoreMigrationInventory>;
    pub fn apply_store_migration(db_path: &Path, spec: &MigrationRewriteSpec, unfence: bool) -> Result<StoreRewriteSummary>;

In `mj-controller/src/migrate/`:

    pub mod preflight;
    pub mod rewrite;
    pub mod protocol;
    pub mod transfer;
    pub fn build_source_preflight(...) -> Result<MigrationPreflightReport>;
    pub fn rewrite_staged_config_and_files(...) -> Result<ConfigRewriteSummary>;
    pub fn detach_codex_auth_symlink(worker_root: &Path) -> Result<bool>;
    pub async fn run_migration_sender(...) -> Result<MigrationOutcome>;
    pub async fn run_migration_receiver(...) -> Result<()>;

In `mj-cli/src/main.rs` and `mj-cli/src/migrate.rs`:

    Command::Migrate(MigrateArgs)
    #[command(hide = true)]
    Command::MigrateReceive(MigrateReceiveArgs)
