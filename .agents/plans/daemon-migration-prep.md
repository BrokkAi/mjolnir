# Prepare Daemon State and Host Identity for Host-to-Host Migration

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This repository maintains its execution-plan standard in `.agents/PLANS.md`. Maintain this document in accordance with `.agents/PLANS.md` so a reader with only the current working tree, `.agents/PLANS.md`, and this file can execute every milestone end to end.


## Purpose / Big Picture

A future `mj migrate` command will move a Mjolnir daemon from one machine (host A) to another (host B), with history intact and sessions reachable. Downtime while the database is copied and edited is acceptable. The move is possible in principle because workers never dial the daemon: the daemon opens every connection outward, by local exec, container exec, or SSH. What makes it hard today is state that assumes the daemon never changes host:

- Nothing stops a daemon from starting on host A against the old store after the store was copied to B. Two daemons would then drive the same workers.
- Worker ownership stamps are a hash of the data directory path, so the stamp is tied to one path on one host.
- Checkpoint archive paths are stored as absolute paths under A's data directory.
- No single list says which files under the data and configuration directories must be copied, left behind, or rebuilt.
- Session placements do not record which configured machine they run on. Migrate needs that to retarget every session on a given machine.

After this plan:

1. `mj daemon fence --to <destination>` atomically stops the daemon and marks the store so that no daemon starts on it, and no client spawns one, until `mj daemon unfence`. The fence row travels with a copied database, so a copy on B also refuses to start until it is edited and unfenced.
2. Worker ownership identity is a value stored in `<data_dir>/instance-identity`, read once by the daemon. A copied data directory keeps it.
3. Managed checkpoint archives are stored by file name relative to `<db_parent>/sessions/`.
4. One manifest in `mj-core/src/config/layout.rs` names every file and directory Mjolnir writes under the data and configuration directories, and says what migrate does with each. Clippy forbids building those paths anywhere else.
5. `session_targets` and `environment_workers` carry a `machine_id` column, backfilled for existing rows where it can be known.

Each milestone is useful and verifiable on its own, before `mj migrate` exists. This plan does not make local-bare sessions movable; see "Notes for `mj migrate`" for why and what that work involves.

Scope of "another machine": a move between hosts of the same OS family (Unix to Unix). SQLite BLOB path columns are written as raw bytes on Unix and UTF-16LE on Windows (`mj-store/src/database/values.rs:9-35`); this plan does not change that encoding.


## Progress

- [x] (2026-11-25 23:55Z) Read-only reconnaissance at `HEAD` `be82cc207` (`SCHEMA_VERSION = 159`, `CONFIG_VERSION = 24`).
- [x] (2026-11-26 00:08Z) First draft, five milestones; requested Advisor review.
- [x] (2026-11-26 02:30Z) Advisor review received (verdict: do not implement as written). Nine read-only children checked the draft against the code; reports in `/home/jonathan/Projects/mjolnir/.mj/clones/91233b0efdd193066e7b4eeb46c8db10/.mj/agents/*/` (files dated 2026-11-25 20:2x–20:4x local time, notably `layout-inventory.md`, `daemon-fence-verification.md`, `store-fence-verification.md`, `instance-identity-verification.md`, `milestone2-archive-paths.md`, `milestone2-home-config-verification.md`, `milestone4-worker-root.md`, `milestone5-verification.md`, `daemon-migration-local-host.md`, `migrate-credential-connection.md`).
- [x] (2026-11-26 02:52Z) User decisions: cut the `SshBare.worker_root` milestone (replaced by notes), cut `CONFIG_VERSION` 25 (migrate rewrites managed homes instead), cut `HostKey`, accept that local-bare sessions lose a working `mj` CLI after a migration. Plan rewritten into four milestones.
- [x] (2026-11-26 03:50Z) Milestone 1: path manifest `mj-core/src/config/layout.rs` and clippy ban on raw `data_dir()` / `config_dir()` (89 files across 17 crates; reviewed clean).
- [x] (2026-11-26 03:50Z) Milestone 3: relative managed checkpoint archive paths (schema revision 160; two review rounds, then clean).
- [x] (2026-11-26 04:22Z) Milestone 4: `machine_id` on `session_targets` and `environment_workers` with backfill (schema revision 161), and machine-first SSH option refresh (three review rounds fixed, fourth overridden).
- [x] (2026-11-26 04:22Z) Milestone 2: durable `instance-identity` resolved once by the daemon; store fence (schema revision 162) with `mj daemon fence` / `unfence` (one review round fixed).
- [x] (2026-11-26 04:47Z) Pushed all four milestones to origin/master (`a30e83083`).


## Surprises & Discoveries

- Observation: `verify_schema_once` runs `migrate_schema`, so a check placed after it runs too late.
  Evidence: `mj-store/src/database/schema.rs:376-398`. A newer binary would migrate a fenced store before refusing it.

- Observation: after `mj daemon stop` returns, another `mj` command, a TUI action, or an SSH client refresh can start the daemon again before a separate fence command runs. `restart_daemon` already closes this window: it holds `daemon-start.lock` from before the stop until the next daemon answers, and every client that starts a daemon takes that lock first.
  Evidence: `mj-cli/src/daemon.rs:1534-1556`, `:1609-1622`; `mj-client` `connector.rs:113-114`.

- Observation: `check_store_readiness` runs against an already-running daemon and after a spawn, not before one. An exited `daemon-run` child is never relaunched, so no special exit code is needed to prevent a restart loop.
  Evidence: `mj-cli/src/daemon.rs:435`, `:1240`, `:1736-1743`, `:473-483`; `wait_exit.rs:91-92`.

- Observation: local-bare shells export `MJ_DATA_DIR`, `MJ_CONFIG_DIR` and `MJ_INSTANCE`, so an agent running `mj` inside a local-bare session on A after a migration meets the fenced store.
  Evidence: `mj-worker-launch` `launch.rs:426-452`.

- Observation: `instance_identity()` is recomputed on every call, and every ownership stamp is written by the daemon process, which takes `ControllerStoreGuard` before anything else.
  Evidence: `mj-core/src/config/loading.rs:410-425`; `worker_launch.rs:273`, `mj-targets/src/targets/preflight.rs:15-23`, `mj-controller/src/controller/recovery_scan.rs:60`, `:437`; `mj-daemon` `process.rs:138`.

- Observation: every container-native daemon hashes the same in-container path `/var/lib/mjolnir/data` and sets no `MJ_INSTANCE`, so all of them share one ownership identity today. Nothing writes an EC2 tag `dev.mj.instance`; recovery only reads it.
  Evidence: `mj-cli/src/daemon/container.rs:876-877`; `recovery_scan.rs:1452`; `ec2.rs:406-412`.

- Observation: the absolute checkpoint archive path is also embedded in JSON: `session_moves.operation_json` (`MoveOperation`, `mj-core/src/state/session_move.rs:663-747`), `session_restart_intents.handoff_json` (`session_restart.rs:629-640`) and `retained_move_sources.source_json` (a full `SessionRecord`, `move_workspace.rs:32-37`).

- Observation: all 5,566 checkpoint rows in the user's live store are direct children of `sessions/`. Live target rows: 275 local-bare, 1 local-podman, 462 ssh-podman, 0 ssh-bare.

- Observation: an older `config.toml` is converted in memory only; startup never saves it. A lazy "strip on next save" leaves unsaved files with absolute `home` lines. The Go port requires `home` on every profile and rejects newer versions.
  Evidence: `mj-core/src/config.rs:1842-1846`, `:2574-2588`; `go/internal/core/config.go:2689-2708`.

- Observation: the draft's file inventory was incomplete, and real data directories hold names no current code writes (`backups`, `git-brokers`, `git-seeds`, `mjolnir.db`, `moves`, `node.log`, `recovery`, `state.json.migrated-v1`, `update-check.json`; in config, about 40 hand-made `config.toml.*` copies). Several top-level directories mix classes. See Milestone 1.

- Observation: `environment_workers` is written with `INSERT ... ON CONFLICT DO UPDATE` of named columns, so an older binary re-storing an existing lease keeps a new column. Only `session_targets` is `DELETE` plus `INSERT`.
  Evidence: `mj-store/src/database/environments.rs:234`; `mj-store/src/database/state_io.rs:1006-1011`, `:1213-1215`.

- Observation: an `SshBare` locator cannot represent a local-bare session. `SshBare` teardown runs `rm -rf` on `workspace`, the worker root and the profile home (`mj-targets/src/targets/cleanup.rs`), which is safe only because validation requires a non-borrowed workspace to end in the session ID (`mj-core/src/state.rs:1431-1436`). A local-bare checkout is the user's own project directory. See "Notes for `mj migrate`".


- Observation (implementation): SQLite's `Connection::path()` is normalized (absolute, symlinks possibly resolved) and is `None` for non-UTF-8 paths, so it cannot serve as the checkpoint root. Writers store `data_dir()` verbatim.
  Evidence: Milestone 3 review; the fix derives the root from the configured database path, as `tool_details` does, and migration 160 also matches rows by canonical parent so a store opened through a symlink still converts.

- Observation (implementation): `kind LIKE 'local-%'` misses `apple-container`, which is also a local kind.
  Evidence: Milestone 4 backfill sets `'local'` on `local-bare`, `local-podman`, `local-docker` and `apple-container` explicitly.

- Observation (implementation): a template may have been repointed to another host after a session was placed, so the template's current machine is not proof of where a session runs.
  Evidence: Milestone 4 review; the backfill fills a row only when the machine's location matches the placement's recorded host, user and port. Legacy EC2 rows are never filled: the frozen runtime has no launch template, so profile, region and user cannot tell two machines in one account apart (second Milestone 4 review).

- Observation (implementation): `Controller` is constructed per call (`Controller::load()` at about 145 sites), so it cannot carry the ownership identity.
  Evidence: Milestone 2 carries it on the daemon's database writer (`start_database_writer(identity)`, read through `database::ownership_identity()`), which errors in a process without a daemon writer.


## Decision Log

- Decision: Assign schema revision numbers in landing order: 160 checkpoint paths, 161 `machine_id`, 162 fence.
  Rationale: three milestones were implemented in parallel worktrees; schema tests need contiguous revisions.
  Date/Author: 2026-11-26 / Coordinator.

- Decision: `SessionRecord.machine_id` is not serialized; environment placements read `environment_workers.machine_id`.
  Rationale: `SessionRecord` and `MoveOperation` go to clients, and older clients reject unknown fields (`deny_unknown_fields`), so serializing the field would break attached terminals across a daemon upgrade (third Milestone 4 review; the second review had asked for it to travel in Move snapshots). A record restored from a Move's `recovery_session` saves NULL until the next daemon load refills the column where the placement proves its machine. Every reader of a placement's machine asks the column that owns it.
  Date/Author: 2026-11-26 / Coordinator.

- Decision: The backfill leaves an SSH placement with no recorded runtime NULL.
  Rationale: the host name alone cannot tell users or ports on one host apart.
  Date/Author: 2026-11-26 / Coordinator, from Milestone 4 review.

- Decision: Setup and doctor smoke containers carry the fixed ownership label `setup-smoke`.
  Rationale: they run in the CLI with no daemon. Letting the CLI create `instance-identity` outside `controller.lock` could race a daemon that writes a different random value. A leftover smoke container is listed by `mj recover --all-instances`.
  Date/Author: 2026-11-26 / Coordinator.

- Decision: `mj daemon fence` refuses, with an explanation, when the instance's daemon runs in a container.
  Rationale: that store lives in the container's volume. Moving container-hosted daemons belongs to `mj migrate`.
  Date/Author: 2026-11-26 / Coordinator.

- Decision: Land the path manifest first, as one short-lived commit.
  Rationale: it touches about 16 crates on a branch other agents commit to, and landing it first means `instance-identity` is classified from the start.
  Date/Author: 2026-11-26 / Coordinator, on Advisor advice.

- Decision: Enforce the manifest with a clippy `disallowed_methods` ban on `mj_core::config::data_dir` and `mj_core::config::config_dir`, allowed only inside `layout.rs`; no live-directory completeness test.
  Rationale: a test proves only the entries a short run creates. The ban makes the manifest the only place paths are built, so the name list and the call sites cannot drift apart. `clippy.toml` already bans `wait_with_output` this way.
  Date/Author: 2026-11-26 / Coordinator, on Advisor advice.

- Decision: Classify entries by what migrate does with them (five actions), not by nine content classes; classify mixed directories one level deeper; copy and report unknown names.
  Rationale: the action is the only thing migrate consumes. Real directories always contain unknown names, so a rule is needed; silently dropping user files is worse than copying extras.
  Date/Author: 2026-11-26 / Coordinator, on Advisor advice.

- Decision: No `cache_dir()` and no `MJ_CACHE_DIR`.
  Rationale: no production caller would use them.
  Date/Author: 2026-11-26 / Coordinator, on Advisor advice.

- Decision: Keep worker-ownership identity in `<data_dir>/instance-identity`, resolved once in `ControllerStoreGuard` and passed to the daemon code that stamps or filters ownership. No fallback reader. `control_dir_identity` stays on the path hash.
  Rationale: one owner for the fact. A fallback that recomputes the path hash when the file is missing would change the identity mid-run while the guard fails closed for the same condition: two answers to one question. ControlMaster directories are host-local runtime state and must not follow a moved store.
  Date/Author: 2026-11-26 / Coordinator, on Advisor advice.

- Decision: Store the fence in SQLite (`store_fence` singleton, revision 162, breaking), checked before schema migration; `mj daemon fence` takes `daemon-start.lock`, stops the daemon itself, takes `controller.lock`, writes the fence, then releases; clients check the fence before spawning.
  Rationale: deleting or renaming `mj.sqlite3` makes the next `mj` command create an empty store. Holding the start lock across stop and fence makes the restart race impossible to express instead of narrowing it, and removes a "stop the daemon first" refusal that a TUI restarting the daemon would turn into a dead end. The breaking floor makes pre-fence binaries refuse the store.
  Date/Author: 2026-11-26 / Coordinator, on Advisor advice.

- Decision: Record where the fence was written (`fenced_host`, `fenced_data_dir`) so the refusal can tell "this store moved away" from "this is a copy waiting for activation".
  Rationale: the copied fence on B is what stops B's daemon from running A's local sessions on B before migrate edits the rows. The same row must not tell the user on B that the store "was migrated to" B. Comparing the current host name and data directory with the recorded ones decides only the wording; the refusal itself is the same either way, so a host-name collision cannot weaken safety.
  Date/Author: 2026-11-26 / Coordinator.

- Decision: Accept that existing container-native stores keep their shared identity.
  Rationale: they are seeded with the old hash so running workers stay owned. New container-native stores get a random token.
  Date/Author: 2026-11-26 / Coordinator.

- Decision: Encode only `session_checkpoints.archive_path` (revision 160). Do not rewrite the archive paths embedded in move, restart-intent, and retained-source JSON; `mj migrate` requires no in-flight move or restart and handles retained move sources itself.
  Rationale: those rows describe short-lived operations or rollback copies. Teaching three JSON shapes a path encoding adds risk for state that migrate can wait out or rewrite once.
  Date/Author: 2026-11-26 / Coordinator.

- Decision: Leave the BLOB path encoding unchanged and limit the move to the same OS family.
  Rationale: writing new relative names as UTF-8 on Windows would make legacy UTF-16LE absolute blobs and new relative blobs ambiguous to decode. A cross-OS daemon move has no current user.
  Date/Author: 2026-11-26 / Coordinator.

- Decision (user): No `CONFIG_VERSION` 25. Managed `HarnessProfile.home` stays absolute in `config.toml`; `mj migrate` rewrites each home that equals `managed_profile_home_in(old_config_root, id)` to the new root.
  Rationale: migrate edits `config.toml` anyway. This avoids a version bump, a lazy-strip trap, and breaking the Go port's parser.
  Date/Author: 2026-11-26 / User.

- Decision (user): Cut the `SshBare.worker_root` milestone and record the local-bare representation problem in "Notes for `mj migrate`".
  Rationale: it cannot represent a local-bare session safely, and nothing would set it until migrate exists.
  Date/Author: 2026-11-26 / User.

- Decision (user): Cut `HostKey`. Leave `project_locations`, `host_container_sizes`, and `mount_history` keyed by host string.
  Rationale: typing the string key does not change what it keys on and adds a second "which machine" key beside `MachineId`. Re-keying by machine ID, if needed, belongs to migrate.
  Date/Author: 2026-11-26 / User.

- Decision (user): Accept that local-bare sessions lose a working `mj` CLI after a migration.
  Rationale: user choice. The fence refusal text must tell an agent reader what happened and not to unfence.
  Date/Author: 2026-11-26 / User.

- Decision: `machine_id` is owned by the `session_targets` / `environment_workers` column only, not also by `TargetRuntimeSettings` JSON. Backfill local kinds in SQL and SSH rows once at daemon load from the template.
  Rationale: runtime JSON is nullable and absent on some rows, so it cannot own the fact, and two copies would be two owners. Without a backfill the template fallback stays forever and migrate cannot rely on the column.
  Date/Author: 2026-11-26 / Coordinator, on Advisor advice.

- Decision: No uncalled helpers (`remap_machine_in_store` and similar).
  Rationale: uncalled migration code decays before `mj migrate` is written.
  Date/Author: 2026-11-26 / Coordinator.


## Outcomes & Retrospective

All four milestones are implemented, reviewed, and pushed (`a30e83083`).

- The manifest and clippy ban hold: workspace clippy fails on a raw `data_dir()` or `config_dir()` call outside `layout.rs`.
- A copied data directory keeps its ownership identity and its managed checkpoint archives, and a fenced store refuses daemon startup before any migration, with a separate message for a copy. The end-to-end test in `mj-cli/tests/daemon_startup.rs` runs fence, copy, and unfence with a real binary in isolated directories.
- Placements record their machine where it can be proved; SSH rows whose template is gone or was repointed stay NULL and are listed for `mj migrate` to map by hand.
- Declined review findings, left as follow-ups: (1) the existing `backfill_target_runtime` fills a missing session runtime from the current template, and the machine backfill then trusts it, so a legacy SSH placement whose template was repointed to another user or port on the same host can take the template's machine; (2) `SessionRecord.machine_id` is not serialized, so a Move recovery snapshot restored after a daemon restart loses a known machine until the next load can prove it again.
- Gaps: no controller-level test resumes a session from a copied data directory (the store-level relocation test covers read and swap); the orphan-adoption `machine_id` line has no test (adoption tests need a live worker).


## Context and Orientation

Mjolnir has a control plane and a data plane. The control plane is the daemon (`mj daemon-run`, built from `mj-daemon`, `mj-controller`, `mj-server`, and `mj-store`). It owns the SQLite database `mj.sqlite3`, session lifecycle, `config.toml`, and the client endpoints. The data plane is one worker per session (`mj-worker`, launched by `mj-worker-launch`, staged by `mj-worker-staging`), running locally, in a container, or on a remote host. A worker binds `<worker_root>/control.sock` and never connects to the daemon; the daemon reaches it by running `hel worker proxy --root <worker_root>` through local exec, `podman`/`docker exec`, or `ssh`.

Two directories hold durable daemon files, resolved in `mj-core/src/config/loading.rs`: the configuration directory (`config_dir()`, `MJ_CONFIG_DIR`, default `<OS config dir>/mjolnir`) and the data directory (`data_dir()`, `MJ_DATA_DIR`, default `<OS local data dir>/mjolnir`). `--instance <name>` (or `MJ_INSTANCE`) nests both defaults under `instances/<name>`; each named instance is a complete, separate store (`loading.rs:213`, `:311-338`). Binaries run from a Cargo `target/` directory refuse to control the default instance (`ensure_may_control_store()`), so all testing uses `--instance <name>` or temporary `MJ_DATA_DIR` / `MJ_CONFIG_DIR`.

Terms used below:

- Ownership identity: a short string stamped into each worker's `ownership.json` and each container's `dev.mj.instance` label. Recovery scans treat workers with this daemon's identity as its own. Today it is `--instance`'s name, or the first 16 hex characters of SHA-256 of the data directory path (`instance_identity_for`, `loading.rs:405-425`).
- `ControllerStoreGuard` (`mj-controller/src/controller.rs`, around line 214): a process-lifetime OS lock on `<data_dir>/controller.lock`, taken by the daemon before it opens the store for writing.
- `daemon-start.lock` (`mj-cli/src/daemon.rs:326`): taken by any client before it spawns a daemon, and held by `restart_daemon` across stop and restart.
- Compatibility floor: `schema_compatibility.minimum_compatible_version`. A binary whose `SCHEMA_VERSION` is below it refuses the store with `StoreSchemaMismatch`. Currently `SCHEMA_VERSION = 159` (`mj-store/src/database.rs`), migrations in `mj-store/src/database/schema.rs`. Production stores exceed 25 GiB; every migration here must be O(1) on large tables (`.agents/docs/schema-migrations.md`).
- Checkpoint archive: a `*.hel.zip` written under `sessions_dir()` (`<data_dir>/sessions/`) when a session suspends; its path is in `session_checkpoints.archive_path` (BLOB).
- Target template and machine: `[targets.<id>]` and `[machines.<id>]` in `config.toml` (`mj-core/src/config/targets.rs`, `machines.rs`; `"local"` is implicit). A session records `sessions.target_template_id`, a locator in `session_targets`, and a frozen `TargetRuntimeSettings` JSON snapshot (`mj-core/src/state/target_runtime.rs`). `target_runtime_settings_for()` (`mj-core/src/state.rs:3743-3773`) refreshes SSH `identity_file` and `extra_args` from the template when the template still exists and host, user, and port match.

`go/` is out of scope (AGENTS.md).


## Plan of Work

Four milestones, in order. Validate, review, and commit each before starting the next.


### Milestone 1: Path manifest and clippy ban

Goal: one place builds every path under the data and configuration directories, and records what migrate does with each.

Create `mj-core/src/config/layout.rs`, exported from `mj-core/src/config.rs`. Define:

    pub enum MigrateAction {
        /// Copy to the new host (store, keys, checkpoints, config, secrets).
        Copy,
        /// Leave on the old host beside the fenced store while local sessions live.
        KeepWithLocalWorkers,
        /// Do not copy; the new host creates its own (host-bound certificates, ControlMaster dir).
        Recreate,
        /// Do not copy (locks, endpoints, sockets, temporary files, caches).
        Skip,
        /// Copy if the user asks (logs, diagnostics, decision logs).
        CopyOptional,
    }

and two path types, `DataLayout { root }` and `ConfigLayout { root }`, with `current()` constructors that are the only callers of `data_dir()` / `config_dir()`, and one accessor per entry. Also provide `classify_data_entry(rel: &Path) -> Classification` and `classify_config_entry(rel: &Path) -> Classification`, where `Classification` is `Known(MigrateAction)`, `NestedStore` (an `instances/<name>` child, which is a complete store with its own layout), or `Unknown`. Migrate's rule for `Unknown` is: copy it and report it, never drop it silently. State that rule in the doc comment.

Data directory entries (builders confirmed at `be82cc207`; see `layout-inventory.md` in the Advisor reports):

    mj.sqlite3, mj.sqlite3-wal           Copy        (schema.rs:136; daemon stopped)
    mj.sqlite3-shm                       Skip
    tool-details/                        Copy        (mj-store tool_details.rs:23)
    sessions/                            per child:
      *.hel.zip                          Copy
      <sid>/attachments/                 Copy        (mj-core attachment.rs:327)
      .hel-checkpoint-*                  Skip
    projects/                            Copy        (worker_binary/project_memory.rs:76)
    config-rename.json                   Copy        (controller.rs:75, :1694)
    instance-identity                    Copy        (Milestone 2)
    api-token                            Copy        (mj-server api/token.rs:7)
    phone-cookie-key                     Copy        (mj-targets signing_key.rs:22)
    phone-cookie-revocations.json        Copy        (mj-server server.rs:267)
    web-push-vapid.pk8                   Copy        (mj-store web_push.rs:80)
    sessionwiki/                         Copy        (contains index.db, a SQLite database; copy only while stopped)
    workers/                             per child:
      pinned/, worker-binaries/          Skip        (binary_select.rs:251)
      <session-id>/                      KeepWithLocalWorkers (backend.rs:1053)
    profiles/                            KeepWithLocalWorkers (per-session nested homes, cleanup.rs:253)
    viewer/                              Recreate    (host certificates, server_runtime/args.rs:38)
    ssh/                                 Recreate    (ControlMaster sockets, targets/ssh.rs:306)
    controller.lock, daemon-start.lock   Skip
    setup.lock, setup-state              Skip        (mj-setup startup.rs:100-101)
    daemon.json, api-endpoint.json       Skip        (mj-client daemon.rs:48, :65)
    daemon-container.json                Skip        (mj-client daemon.rs:182)
    daemon-exits/                        Skip        (mj-core daemon_exit.rs:34)
    image-pulls/                         Skip        (lock files, image_pull_gate.rs:106)
    terminal-upgrades/                   Skip        (mj-cli dashboard/upgrade.rs:42)
    uploads/, fetch-spool/               Skip        (process.rs:174; mj-checkpoint fetch.rs:275)
    snapshot-cache/                      Skip        (host_cache.rs:121)
    github-student/, github-student-native/  Skip    (mj-github-relevance github_student.rs:666, :731)
    models/                              Skip        (guardian_classifier.rs:302)
    mbx/, mbx-download/                  Skip        (mj-build-cache release.rs:298, install.rs:1017)
    mbx-release-failures.jsonl           CopyOptional (release.rs:42, :551)
    power-ranking-cache.json             Skip        (mj-accounts power_ranking.rs:145)
    sessionwiki-built                    Skip        (mj-sessionwiki sessionwiki.rs:1569)
    daemon.log, logs/, diagnostics/      CopyOptional
    jev-decisions/                       CopyOptional (mj-core jev.rs:182)
    instances/<name>/                    NestedStore

Configuration directory entries:

    config.toml, go.json                 Copy        (loading.rs:331; go.rs:183)
    config.toml.lock, go.json.lock       Skip        (config.rs:2651)
    secrets.toml                         Copy        (config/secrets.rs:31)
    profiles/                            Copy        (managed logins, harness.rs:642)
    ec2/                                 Copy        (ec2_records.rs:16)
    client-hints.json                    Copy        (mj-cli hints.rs:44)
    instances/<name>/                    NestedStore

Anything else (for example `config.toml~`, `config.toml.before-*`, `backups/`, `mjolnir.db`) is `Unknown`.

Add to `clippy.toml` `disallowed-methods` entries for `mj_core::config::data_dir` and `mj_core::config::config_dir` (and `sessions_dir` if it remains public), each with a reason pointing at `DataLayout` / `ConfigLayout`, following the existing `wait_with_output` entries (`clippy.toml:33`, `:39`). Put `#[allow(clippy::disallowed_methods)]` only on the two `current()` constructors. Then convert every caller (about 40 files and 88 join sites across `mj-cli`, `mj-core`, `mj-controller`, `mj-build-cache`, `mj-worker-staging`, `mj-daemon`, `mj-targets`, `mj-worker-launch`, `mj-server`, `mj-import`, `mj-client`, `mj-store`, `mj-sessionwiki`, `mj-setup`, `mj-checkpoint`, `mj-github-relevance`, `mj-accounts`). Where a function in another crate takes a bare root and joins inside (`github_student`, `guardian_classifier`, `power_ranking`), give it the specific path from a layout accessor instead. Test code may keep raw joins under its own isolated temporary roots.

Do not move any existing file on disk.

Result: `cargo clippy --workspace --all-targets -- -D warnings` fails if new code builds a data or config path outside `layout.rs`, and migrate has a complete action table.


### Milestone 2: Durable ownership identity and store fence

Goal: a copied data directory keeps its ownership identity, and no daemon starts on a store that has been moved away or not yet activated.

Identity. In `mj-core/src/config/loading.rs`, rename today's computation to `legacy_instance_identity_for(instance, data_dir)` and add `ensure_instance_identity_at(instance, data_dir) -> Result<InstanceIdentity>`. It reads `DataLayout::instance_identity()`. If present, it must be valid (`is_valid_instance_name`) or the call fails with an error naming the file. If absent, it writes atomically, mode `0600`: the instance name when `instance` is set; else the legacy hash when `mj.sqlite3` or `workers/` already exists (an existing store upgrading in place, so stamped workers stay owned); else 16 random lowercase hex characters (`getrandom`). Call it in `ControllerStoreGuard::try_acquire_at` right after taking `controller.lock`, store the result in the guard, and pass it from there to the daemon code that stamps or filters ownership (`worker_launch.rs:273`, `preflight.rs:15-23`, `recovery_scan.rs:60`, `:437`). Remove the free `instance_identity()` function so the compiler finds every caller; any caller outside the daemon must get the value from the daemon or use the same strict reader. Change `control_dir_identity()` (`mj-core/src/targets/ssh.rs:310-324`) to call `legacy_instance_identity_for` directly; it is the only production caller of the path form.

Note in code comments: existing container-native stores keep the identity they share today because they are seeded with the old hash; a copied data directory carries the same identity, and the fence is what keeps two daemons with one identity from running at once.

Fence schema. Advance `SCHEMA_VERSION` to 162. Migration 162 is breaking (floor 162; pre-162 binaries do not read the fence and would start a daemon on a fenced store) and O(1):

    CREATE TABLE IF NOT EXISTS store_fence (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        destination TEXT NOT NULL,
        fenced_at TEXT NOT NULL,
        fenced_host TEXT NOT NULL,
        fenced_data_dir BLOB NOT NULL,
        note TEXT
    ) STRICT;

Validate non-empty fields in Rust. Add `mj-store/src/database/store_fence.rs` with `StoreFenceRecord`, `read_store_fence(connection) -> Result<Option<StoreFenceRecord>>` (returns `None` when the table does not exist), `set_store_fence`, and `clear_store_fence`. In `schema.rs::open_writable`, read the fence before `verify_schema_once` and return `StoreFenced` (a typed error in `mj-core/src/storage.rs` beside `StoreSchemaMismatch`) if a row exists. Reader opens (`open_reader_strict`) stay allowed so offline inspection works.

`StoreFenced`'s message has two forms. When the current host name and data directory match the recorded ones: "This Mjolnir store was moved to <destination> on <fenced_at>. No daemon will start here." When they differ: "This Mjolnir store is a copy fenced on <fenced_host> at <fenced_data_dir>. Finish editing it, then run `mj daemon unfence` to activate it here." Both forms add, for agent readers: "If you are an agent running inside a Mjolnir session, `mj` commands on this host no longer reach a daemon. Do not run `mj daemon unfence`; tell the user." Include `note` if set.

Fence commands. Add `mj daemon fence --to <destination> [--note <text>]` and `mj daemon unfence` to `DaemonCommand` (`mj-cli/src/main.rs:309`). `fence`: take `daemon-start.lock` the way `restart_daemon` does (`mj-cli/src/daemon.rs:1534-1622`), stop a running daemon using the same stop path, take `ControllerStoreGuard`, open the store writable without the fence check (this migrates it to revision 162 if needed), write the row, and release both locks. `unfence`: same locks, clear the row, do not start a daemon. Both print what they did. `mj daemon status` reports a fence when no daemon is running.

Pre-spawn check. In `connect_or_start` (`mj-cli/src/daemon.rs`), before spawning `daemon-run`, open the store read-only and check the fence; refuse with `StoreFenced` instead of spawning. Clients that only connect to an already-running daemon need no change. No new exit code: an exited `daemon-run` child is not relaunched.

Named instances are separate stores; fencing the default store does not fence them. Say so in `mj daemon fence --help`.

Result: in an isolated instance, `mj daemon fence --to ssh://b` stops a running daemon and leaves it stopped; any client command is refused before spawning; copying the data and config directories to a new path and running the same binary there shows the "copy" message; `mj daemon unfence` restores normal startup. The copied directory has the same `instance-identity`.


### Milestone 3: Relative managed checkpoint archive paths

Goal: managed checkpoint archives resolve under whatever directory the store now lives in.

In `mj-store/src/database/state_io.rs`, add two helpers taking `sessions_root: &Path`, derived from the database file (`db_path.parent().join("sessions")`, matching how `tool_details.rs:14` derives its root):

    fn encode_checkpoint_archive_path(path: &Path, sessions_root: &Path) -> PathBuf
    fn decode_checkpoint_archive_path(stored: &Path, sessions_root: &Path) -> Result<PathBuf>

`encode` returns the bare file name when `path` is a direct child of `sessions_root`, else `path` unchanged. `decode` joins a relative single-component name onto `sessions_root`, returns absolute paths unchanged, and rejects relative values with more than one component or `..`. Writers store the raw `sessions_dir()` join, and resume canonicalizes without saving the result (`resume.rs:1575-1588`), so plain prefix stripping is correct.

Use `encode` in `replace_checkpoint` (`state_io.rs:1016-1034`) and in `replace_checkpoint_archive_if_current` (`mj-store/src/database/sessions.rs:1534-1561`), whose `WHERE` compares `encode(expected)` only; after revision 160 every managed row is relative and callers hold decoded paths. Use `decode` in `read_checkpoints` (`state_io.rs:1470-1496`). `resume.rs`, `purge.rs`, `checkpoint/archives.rs` and `checkpoint/staging.rs` keep receiving absolute paths.

Advance `SCHEMA_VERSION` to 160. Migration 160 is breaking (floor 160; older binaries read the column verbatim and `resume.rs` rejects relative paths). It reads `(session_id, archive_path)` from the small `session_checkpoints` table and rewrites rows that `encode` shortens; in-memory databases and rows outside `sessions_root` are left alone.

Do not touch the archive paths inside `session_moves.operation_json`, `session_restart_intents.handoff_json`, or `retained_move_sources.source_json`. Record in "Notes for `mj migrate`" that migrate requires no in-flight move or restart and handles retained move sources.

Result: a store whose data directory was copied to a new path resumes a suspended session from the copied archive.


### Milestone 4: `machine_id` on placements

Goal: every placement records which configured machine it runs on, so migrate can retarget all sessions on a machine, and SSH option edits on `[machines.<id>]` refresh sessions whose template is gone.

In `mj-core/src/config/machines.rs`, add a validated `MachineId` newtype (`serde(transparent)`, validated with `validate_id("machine", ...)`, `MachineId::local()`, `is_local()`). Use it where the code passes machine ids today; do not add a parallel string key.

Advance `SCHEMA_VERSION` to 161. Migration 161 is breaking because older binaries rewrite `session_targets` with `DELETE` plus `INSERT` (`state_io.rs:1006-1011`, `:1213-1215`) and would drop the column; `environment_workers` alone would be compatible (`ON CONFLICT DO UPDATE`, `environments.rs:234`). O(1) DDL plus a small-table update:

    ALTER TABLE session_targets ADD COLUMN machine_id TEXT;
    ALTER TABLE environment_workers ADD COLUMN machine_id TEXT;
    UPDATE session_targets SET machine_id = 'local' WHERE kind LIKE 'local-%';
    UPDATE environment_workers SET machine_id = 'local' WHERE kind LIKE 'local-%';

(Check the exact local kind strings in the migration-124 schema before writing the `WHERE`.) Read and write the column in `state_io.rs` and `environments.rs`. Provisioning sets it from the template's machine (`"local"` for local kinds). At daemon load, next to the existing `target_runtime_json` backfill (`controller.rs:809-831`, `state_io.rs:1501-1512`), fill NULL `machine_id` on SSH rows from `config.targets[target_template_id].machine` when that template still exists. Rows whose template was removed stay NULL; record that in a code comment and in the notes for migrate.

Do not add `machine_id` to `TargetRuntimeSettings`; the column is the only owner.

In `target_runtime_settings_for` (`mj-core/src/state.rs:3743-3773`): when the placement has a `machine_id` that resolves to `Machine::Ssh` in `config.machines` with the same host, user and port as the frozen connection, refresh `identity_file` and `extra_args` from the machine; otherwise use the existing template refresh; otherwise the frozen settings. The benefit beyond today is narrow (sessions whose template was removed or renamed by hand); the main value is the column itself.

Result: after upgrade, every local row has `machine_id = 'local'`, SSH rows with a surviving template have their machine, and editing `[machines.<id>].identity_file` refreshes a session whose template was deleted.


## Notes for `mj migrate`

These are not part of this plan. They record what review found so migrate's design starts from them.

1. Local-bare sessions have no valid remote form. The migration-124 `CHECK` requires `host IS NULL` for local kinds, so every local row needs a kind change. Local-podman can become ssh-podman (in-container paths are identical). Local-bare (275 rows on the user's store) cannot become `SshBare`: `SshBare` teardown would `rm -rf` the user's project directory, its profile home is `.local/share/hel/profiles/<sid>` instead of `<worker_root>/profile` or `<data_dir>/profiles/<sid>` (`profile_home.rs:45-79`), the SSH recovery scan searches only `.local/share/hel/workers` (`recovery_scan.rs:1194-1209`; same parent at `targets.rs:4614`, `storage.rs:81`, `:176-185`, `backend.rs:436`), and skills scope changes from `Localhost` to `Isolated` (`state.rs:1370-1375`). Layout (root, profile home, what teardown may delete, scan parent, skills scope) and transport (local exec or SSH) are separate facts; migrate needs a representation for "local-bare layout reached over SSH", either a new locator kind or a transport field. No code prepends `$HOME/` or `~/` to an absolute root today (`resume.rs:926-933`, `store.rs:701`, `staging.rs:145-148`).
2. Local-bare sessions lose a working `mj` CLI after the move (user accepted). Their shells point `MJ_DATA_DIR` / `MJ_CONFIG_DIR` at A's fenced store.
3. A's data directory must stay in place beside the fenced store while local sessions live: their worker roots (`state_io.rs:1051-1056`) and nested profiles are under it (`KeepWithLocalWorkers`).
4. Order: fence on A, copy, edit the copy on B (retarget `local-*` rows, rewrite managed homes, add a machine for A), unfence on B. `unfence` on A is the rollback.
5. Managed profile homes: rewrite each `home` equal to `managed_profile_home_in(old_root, id)` to the new root, comparing typed paths.
6. Preconditions: no in-flight `session_moves` or `session_restart_intents`; rewrite or drop `retained_move_sources`.
7. Local-bare Codex can symlink the profile's own auth file (`harness.rs:917-928`, `local_profile_homes.rs:139-158`) and would keep reading A's stale copy. Other credentials are pushed over the relay once the locator is SSH (`credential_sync.rs:2507-2561`).
8. State outside the two directories must exist on B: SSH identity files, `ssh_config` aliases, `gh auth`, `~/.aws`. The 462 ssh-podman sessions depend on them.
9. Rows that would act on the wrong machine: `host_snapshot_cache` rows with host `local` hold paths under A's cache (`host_cache.rs:209-219`); `host_container_sizes` with host `local` would give B A's chosen size.
10. Project memory for a checkout with no remote is keyed by a hash containing `path:<absolute>` (`repository.rs:162-167`, `project_memory.rs:674`); it survives only if the path string is kept.
11. Placements with `machine_id IS NULL` (SSH rows whose template was removed or repointed or that have no recorded runtime, all legacy EC2 rows, and records restored from a Move snapshot whose template no longer proves the machine) need a manual mapping.
12. Named instances are separate stores and migrate separately.


## Concrete Steps

Run from the repository root. Run every `cargo test` outside the restricted sandbox. Test new binaries only with `--instance <name>` or temporary `MJ_DATA_DIR` / `MJ_CONFIG_DIR`, and stop test daemons afterward. Check that every filtered `cargo test` run reports at least one test; a filter that matches nothing reports `ok`.

Milestone 1:

    cargo clippy --workspace --all-targets -- -D warnings
    cargo test -p brokk-mjolnir --test daemon_startup

Milestone 2:

    cargo test -p brokk-mj-core instance_identity
    cargo test -p brokk-mj-store store_fence
    cargo test -p brokk-mj-store schema
    cargo test -p brokk-mj-controller controller_store_guard
    cargo test -p brokk-mjolnir --test daemon_startup
    cargo clippy -p brokk-mj-core -p brokk-mj-store -p brokk-mj-controller -p brokk-mjolnir -p brokk-mj-targets -p brokk-mj-worker-launch --all-targets -- -D warnings

Milestone 3:

    cargo test -p brokk-mj-store checkpoint
    cargo test -p brokk-mj-store schema
    cargo test -p brokk-mj-controller resume
    cargo clippy -p brokk-mj-store -p brokk-mj-controller --all-targets -- -D warnings

Milestone 4:

    cargo test -p brokk-mj-store schema
    cargo test -p brokk-mj-store target
    cargo test -p brokk-mj-store environment
    cargo test -p brokk-mj-core target_runtime
    cargo clippy -p brokk-mj-core -p brokk-mj-store -p brokk-mj-controller --all-targets -- -D warnings

Test names above are expected filters; adjust them to the names the new tests use. Commit each milestone after its review.


## Validation and Acceptance

Follow AGENTS.md "Which tests to add": integration and tricky-condition tests only.

Milestone 1: the clippy ban is the check. Confirm by adding a temporary raw `data_dir().join("x")` in a non-layout crate and seeing clippy fail, then removing it.

Milestone 2:
- End-to-end (real `mj` binary, isolated instance): start a daemon; `mj daemon fence --to ssh://b --note test` stops it; a client command is refused with the "moved" message and no `daemon-run` process appears; `mj daemon status` shows the fence; copy both directories to new paths and run with `MJ_DATA_DIR` / `MJ_CONFIG_DIR` pointing there, and see the "copy" message; `mj daemon unfence` there, and the daemon starts with the same `instance-identity` as the original.
- Tricky condition (race): with `daemon-start.lock` held by a running `fence`, a concurrent `connect_or_start` waits and then sees the fence instead of spawning.
- Integration (schema): the fence is read before migration. Build a fixture store at revision 161 with a hand-made `store_fence` table and row (standing in for a fenced store that a later binary opens). `open_writable` refuses it with `StoreFenced` and the stored schema version is still 161 afterward. Separately, an unfenced revision-161 store upgrades to 162 with floor 162.
- Tricky condition (fail closed): a corrupt `instance-identity` makes `ControllerStoreGuard::try_acquire_at` fail with an error naming the file.

Milestone 3:
- Integration (schema): a revision-159 store with absolute managed rows and one external row upgrades to relative names for the managed rows only.
- End-to-end or controller-level: suspend a session in an isolated instance, stop, copy the data directory to a new path, start there, resume the session from the copied archive.

Milestone 4:
- Integration (schema): a revision-160 store with local and SSH rows upgrades with `machine_id = 'local'` on local rows; after daemon load, SSH rows with a surviving template have their machine and rows with a removed template stay NULL.
- Tricky condition: with the template deleted, editing `[machines.<id>].identity_file` changes the result of `target_runtime_settings_for`; deleting the machine falls back to the frozen settings.


## Idempotence and Recovery

Each migration runs in an `IMMEDIATE` transaction guarded by its revision and uses `legacy_schema::table_has_column` before `ALTER TABLE ADD COLUMN`, so a failed start rolls back and the next start retries. None touches `materialized_transcript_items`, `api_events`, or `native_agent_transcript`.

`mj daemon unfence` reverses an accidental fence. `instance-identity` is written by atomic rename and never overwritten once valid.

Revisions 160–162 each raise the floor; test only in isolated stores.


## Artifacts and Notes

Baseline:

    HEAD: be82cc207 (master)
    SCHEMA_VERSION: 159 (floor 159)
    CONFIG_VERSION: 24 (unchanged by this plan)

Schema revisions:

    160 (breaking): session_checkpoints.archive_path relative for managed rows; floor 160
    161 (breaking): machine_id on session_targets and environment_workers, local rows backfilled; floor 161
    162 (breaking): store_fence table; floor 162


## Interfaces and Dependencies

No new crates and no new third-party dependencies (`getrandom`, `rusqlite`, `serde` are already in the workspace).

`mj-core/src/config/layout.rs`:

    pub enum MigrateAction { Copy, KeepWithLocalWorkers, Recreate, Skip, CopyOptional }
    pub enum Classification { Known(MigrateAction), NestedStore, Unknown }
    pub struct DataLayout { root: PathBuf }
    pub struct ConfigLayout { root: PathBuf }
    impl DataLayout { pub fn current() -> Self; pub fn at(root: PathBuf) -> Self; /* one accessor per entry */ }
    impl ConfigLayout { pub fn current() -> Self; pub fn at(root: PathBuf) -> Self; /* one accessor per entry */ }
    pub fn classify_data_entry(rel: &Path) -> Classification;
    pub fn classify_config_entry(rel: &Path) -> Classification;

`mj-core/src/config/loading.rs`:

    pub fn legacy_instance_identity_for(instance: Option<&str>, data_dir: &Path) -> String;
    pub fn ensure_instance_identity_at(instance: Option<&str>, data_dir: &Path) -> Result<InstanceIdentity>;

`mj-core/src/storage.rs`:

    pub struct StoreFenced {
        pub destination: String,
        pub fenced_at: String,
        pub fenced_host: String,
        pub fenced_data_dir: PathBuf,
        pub note: Option<String>,
        pub is_copy: bool,
    }

`mj-store/src/database/store_fence.rs`:

    pub struct StoreFenceRecord { destination, fenced_at, fenced_host, fenced_data_dir, note }
    pub fn read_store_fence(connection: &Connection) -> Result<Option<StoreFenceRecord>>;
    pub fn set_store_fence(connection: &Connection, record: &StoreFenceRecord) -> Result<()>;
    pub fn clear_store_fence(connection: &Connection) -> Result<bool>;

`mj-core/src/config/machines.rs`:

    #[serde(transparent)]
    pub struct MachineId(String);
    impl MachineId { pub fn local() -> Self; pub fn parse(raw: impl Into<String>) -> Result<Self>; pub fn as_str(&self) -> &str; pub fn is_local(&self) -> bool; }
