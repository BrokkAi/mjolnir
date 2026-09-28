# Share machine-owned mbx configuration and expose both budgets


This ExecPlan follows `.agents/PLANS.md` and is maintained through implementation.

## Purpose / Big Picture


Issue #1179 concerns the independent managed-target limit beneath mbx's combined cache limit. Users should configure Total cache budget and Worktree build budget on the machine's existing cache page. mj privately installs and configures mbx for its containers; a compatible native installation is user-managed and its configuration is never rewritten. Containers read one shared configuration directory per machine. Existing containers adopt machine configuration during worker upgrade; there is no retained legacy launch path.

## Progress


- [x] (2026-09-28) Investigated configuration, host inspection, launch propagation, and mbx 1.16.0 defaults; agreed scope with user and claimed #1179.
- [x] Add machine settings, resolved budget reporting, and shared configuration application.
- [x] Integrate shared configuration lookup for new containers and upgrades.
- [x] Apply machine changes in supervised daemon work and display application state.
- [x] Add behavior tests and verify live configuration updates in two isolated containers.
- [x] Complete final dev-profile validation and prepare delivery on the current branch; the user authorized pushing to the configured upstream.

## Surprises & Discoveries


The current inspector reports `gc.max_size` (action store only) as a combined limit. Worker launch injects a saved session total while installation copies the host configuration into each container. These are snapshots of machine policy. mbx 1.16.0 has no effective-configuration JSON command; default reporting must match its pinned disk-scaling rules. A directory mount is necessary because mbx configuration updates replace files atomically.

## Decision Log


The user chose two ordinary budget fields, no advanced mbx page, and private mj management rather than managing a general-purpose host installation. The user superseded the earlier recreation-only transition: changing cache behavior on upgrade is desired, and the legacy launch path must be removed. Retain old session fields for reading legacy records; no new session budget field or database migration is needed. Allocation changes belong to mbx separately.

## Outcomes & Retrospective


Implementation is complete. The first full dev-profile suite passed after correcting TUI rendering during an in-flight preview. Two isolated rootless Ubuntu containers running mbx 1.16.0 read the same total and worktree budgets through the actual installation script; one atomic update changed the reported worktree budget in both containers and both reviewer homes without changing container PIDs. Final Clippy passes with warnings denied. Upstream integration changed sub-agent guidance without updating two wording assertions; the assertions now check the equivalent updated guidance, and all 24 sub-agent MCP tests pass. The final full `cargo test` suite passes, including historical migrations, worker upgrades, relay regressions, the CLI, and doc tests. `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` pass. Delivery consists of the cache feature commit and a separate small correction to the inherited sub-agent wording tests, pushed to `origin/master` as requested.

## Context and Orientation


`mj-core/src/config/targets.rs` defines `TargetBuildCache`, stored under machines by `config/machines.rs`. `mj-controller/src/controller/mbx.rs` inspects local and SSH hosts, resolves caches, and attaches mounts during provisioning. `controller/worker_binary/launch.rs` builds worker environments; `install.rs` installs the private mbx binary and currently copies configuration. `mj-tui/src/setup.rs` and `setup/schema.rs` implement the machine cache page and asynchronous preview. `mj-controller/src/daemon/process.rs` owns supervised daemon background services. The daemon is the control process; workers run sessions independently.

## Plan of Work


### Milestone 1: machine resolution and application


Add optional `target_max_size` using the existing size parser. A compatible native mbx selects user-managed mode and supplies its directory and configuration, ignoring inactive mj budgets. Otherwise, create an mj-owned configuration outside the general host mbx configuration. Preserve the once-computed automatic total across later inspections. Use atomic, serialized, idempotent application per host/configuration path. Resolve and display both budgets, sources, and pending/applied/error status. Keep inspection read-only. Separate action-store and incremental constraints from the combined total; match defaults to mbx 1.16.0.

### Milestone 2: shared configuration consumers


Use the cache directory that every existing container already mounts to expose `.mjolnir/config/mbx/config.toml`. Link each relevant mbx configuration lookup to this shared file without changing other programs' XDG roots. Native configuration is projected once per machine into this shared location, never once per container; the native original remains read-only. There is no new mount requirement, no generated budget variable, and no private configuration copy. Worker upgrade prepares these links and removes the session-budget injection. Cache placement remains recorded, but budgets do not gain new session persistence.

### Milestone 3: background application and presentation


Supervise machine applications off event loops, serialize decisions per machine, retry with backoff, cancel bounded work during daemon replacement, and recover desired state from configuration after restart. Provisioning waits for the same application operation. The setup page shows Total cache budget and Worktree build budget, sources, management mode, application errors, and application status. User-managed budgets are read-only. Neither inspection nor settings rendering scans target trees or runs garbage collection.

### Milestone 4: validation and delivery


Test defaults, explicit and inherited budgets, configuration preservation, concurrent/idempotent application, atomic replacement visibility, remote failure/recovery, all launch environments, and automatic adoption on worker upgrade. Use test executors and isolated filesystem fixtures, then exercise real shared configuration consumption where available. Tests must not touch live host mbx configuration or the default mj instance.

## Concrete Steps


From `/home/jonathan/Projects/mjolnir2`, run focused package tests during development, then `cargo test` and `cargo clippy --all-targets -- -D warnings` using the normal dev profile and existing mbx build storage. Every cargo test runs outside the restricted sandbox. Any test-build CLI or daemon invocation uses `--instance mbx-1179` plus isolated configuration/data directories. Do not redirect Cargo output. Review the final diff and commit only task files on the current branch; push to the configured upstream, as explicitly requested by the user.

## Validation and Acceptance


Two containers on the same machine must read the same configuration; replacing its file changes the next mbx invocation in both without restarting their workers. A managed total over 100 GiB must not hide the independent target limit. Explicit worktree budget must reach mbx through shared configuration. A native installation's configuration must remain byte-for-byte unchanged. Repeated provisioning must not rewrite unchanged policy. Existing containers must stop receiving the recorded session budget when upgraded, and their next builds must read shared machine policy. UI operations remain responsive when SSH is slow or unavailable.

## Idempotence and Recovery


Apply configuration using atomic replacement and host-side serialization; reuse equal contents. Reconstruct work after daemon restart from machine configuration and shared files. Never rewrite native mbx configuration. Use the existing cache mount to update containers without recreation. Worker replacement still requires its existing atomic idle reservation; never stop a busy build. Record failures rather than silently claiming an application succeeded.

## Artifacts and Notes


Issue: https://github.com/BrokkAi/mjolnir/issues/1179. The smoke check used `mbx gc --dry-run --json` with an isolated empty cache; mj itself does not run collection during application or preview. mbx 1.16.0 has no `settings get` command. Source reference for pinned behavior: `../mr-boxington` tag `v1.16.0`, `crates/mbx/src/config.rs` and `cli/gc.rs`. Target defaults use 10% of their disk, clamped to 10..100 GiB and rounded down in 5 GiB steps; action-store defaults use 5%, clamped to 5..500 GiB. Combined collection reserves action-store allowance and retained incremental state before sizing targets.

## Interfaces and Dependencies


Extend `TargetBuildCache` and `BuildCachePreview`; keep old `SessionBuildCache` deserialization. Reuse `CacheHost`, `CommandExecutor`, shared subprocess helpers, existing supervision/admission mechanisms, and TOML parsing. Do not create a crate. Configuration application and UI reporting share one resolver, so launch behavior cannot drift from preview. No mbx pin change is planned.

Initial plan recorded 2026-09-28 after the user authorized implementation.

Revised during implementation: the user explicitly removed legacy behavior. Shared configuration now lives beneath the already-mounted cache, enabling upgrades without container recreation; user-managed configuration is projected once per machine and refreshed in background.

Review adjustment: collect distinct cache directories still mounted by existing sessions and publish the same machine document to those paths too. This preserves machine policy after placement changes without recreating containers or reviving recorded session budgets. Upstream was fast-forwarded from 9f545a8b to 05e688d3 before final validation.

Upgrade preparation links configuration separately from private binary installation. It never copies over an mbx executable that an existing build may still be running. The shared policy is ready before the existing atomic idle reservation replaces the worker.

Validation artifacts are `/tmp/mj-1179-complete-tests.log` (final full suite), `/tmp/mj-1179-delivery-clippy.log` (all targets, warnings denied), and `/tmp/mj-1179-upstream-tests.log` (24 passing sub-agent MCP tests). The two-container smoke check passed twice and removed its containers before deleting their isolated cache. Formatting and diff whitespace checks also pass.
