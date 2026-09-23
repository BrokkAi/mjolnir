# Keep existing targets accessible after their configuration template is removed

This ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective throughout implementation. This is the approved implementation plan for #1018 only. The user requested planning and implementation one issue at a time, with review before implementation. #965 is implemented, validated, published, and closed. The user approved implementation after confirming that the breaking migration preserves existing data and that testing leaves the live store untouched.

## Purpose / Big Picture


An existing session must remain exportable and destroyable when its named target template disappears from config.toml. A target template describes how to create a target; the durable session record must contain the access settings needed to operate the target that was actually created. Removing a template or reusing its name must not redirect an existing session to another host.

Issue #1018 reports six running SSH Podman sessions whose template was removed. Branch export, bundle export, and force destroy all failed with a generic 500 even though the containers still existed. The fix belongs in Mjolnir's durable target resolver, not an upstream agent adapter.

## Progress


- [x] (2026-09-23) Finished and published #965 (`e0e9dd83`, merged through `3391a719`) before beginning this plan.
- [x] (2026-09-23) Read #1018, confirmed it is unassigned, self-assigned it, and added `agent-in-progress`.
- [x] (2026-09-23) Traced shared target conversion, lifecycle teardown, API exports, target persistence, and existing config-change validation.
- [x] (2026-09-23) Prepared this concrete plan and identified the legacy-data and database-compatibility limits.
- [x] (2026-09-23) Received user approval before implementation.
- [x] (2026-09-23) Added durable target settings on sessions and breaking migration 46, with data-preservation and older-build refusal tests.
- [x] (2026-09-23) Shared resolution now uses recorded access, with registration, backfill, adoption, resume, borrowed-child, worker launch, raw-checkout and cache-host integration.
- [x] (2026-09-23) Added regression coverage for config removal/name reuse/restart, destroy retry, borrowed children, backfill/refusal, migration, target replacement and rollback, SSH/AWS connections, and HTTP export conflicts.
- [x] (2026-09-23) Focused regressions pass, including complete SSH/AWS access, cleanup retry, and concurrent deletion during reload.
- [x] (2026-09-23) Final `env -u NO_COLOR cargo test`, `cargo clippy --all-targets -- -D warnings`, formatting, and diff checks pass.
- [x] (2026-09-23) Required validation passed in isolated stores.
- [x] (2026-09-23) Committed `d81fd67c` on the current branch and pushed to origin/master.
- [x] (2026-09-23) Closed #1018 with validation evidence and removed `agent-in-progress`; then prepared #1063's separate review plan. The later queue is #1073 followed by #1083.

## Surprises & Discoveries


`backend_locator` in `mj-controller/src/controller/backend.rs` unconditionally looks up `config.targets[session.target_template_id]` before converting any stored locator. This is the shared failure for existing-target operations, including `force_destroy_session_with` in `controller/lifecycle.rs`. Several worker-launch and reviewer paths also consult the template directly and need an audit for operations on already existing targets.

`mj_core::state::TargetLocator` records local roots, container IDs, workspace ownership, SSH host names, and EC2 instance/address information. It does not record the complete SSH connection (user, key path, extra arguments) or AWS profile/region. Recovering a remote connection from the host string alone would discard real access requirements. The image tag is not needed to export or remove an existing container: its recorded container ID identifies that resource.

`mj-core/src/targets/convert.rs` already owns conversion from stored paths and target metadata into execution plans. Reuse that boundary rather than creating per-operation reconstruction rules. It validates the SSH Docker host but does not currently apply equivalent host validation to every SSH variant; legacy backfill must validate kind and host consistently.

`State::validate_setup_update` already prevents Setup from changing targets used by active sessions. That does not prevent a user editing config.toml directly. Rejecting configuration changes alone would leave the reported orphaned sessions inaccessible.

`database/state_io.rs::replace_targets` deletes and reinserts a session's target row on updates, but access must also survive partial provisioning before any target row exists. The implementation stores `target_runtime_json` on `sessions`. Older writers could replace a target while keeping the access snapshot for its previous resource; revision 46 therefore raises the minimum compatible read/write revision in the same transaction. Migration only adds a nullable column and preserves existing session data. No live store is upgraded during implementation or testing.

## Decision Log


Decision (2026-09-23, approved): persist the minimal access settings for an existing target alongside its locator. Use the current template for provisioning a new target; use the recorded access settings for operating an existing target. A template with the same name must never override an existing session's recorded destination.

Decision (2026-09-23, approved): retain paths as Path/PathBuf in durable structures and use the existing conversion module at the command boundary. Store identity-file paths and configured connection options, not private-key contents or a copy of unrelated launch configuration.

Decision (2026-09-23, approved): backfill older records from a still-present, matching template before normal operations and before discarding old configuration during a reload. Local targets can derive their access entirely from their locator. Remote records whose template has already disappeared and whose access settings were never saved cannot be fully reconstructed: return a typed conflict naming the session and target, explain that the matching template must be restored once, and preserve the session. Do not guess SSH users, keys, proxy options, AWS regions, or a similarly named template.

Decision (2026-09-23, approved): define access settings as belonging to the current provisioned target. When an explicit move or resume creates a different target, replace its locator and access settings together. A borrowed child must inherit its owning target's recorded access and preserve borrowed-resource cleanup rules.

Decision (2026-09-23, approved): classify the migration as breaking because older writers can replace a target without replacing its saved access settings. Tests use isolated MJ_CONFIG_DIR and MJ_DATA_DIR. Approval of this feature does not authorize an incompatible upgrade of the user's live database.

## Context and Orientation


`mj-core/src/state.rs` contains SessionRecord and its durable TargetLocator. `mj-core/src/config/targets.rs` contains TargetTemplate and SshConnection. `mj-core/src/targets/convert.rs` contains StoredTarget and its conversion to the execution-plan TargetLocator; this is the shared interpretation point.

`mj-controller/src/database/state_io.rs` loads and saves session and target rows. `database/schema.rs`, the baseline schema, and schema reader tests define schema revision and older-reader/writer compatibility. Add a new migration; do not rewrite an applied migration.

`controller/backend.rs::backend_locator` resolves an existing session for target operations. `controller/lifecycle.rs::force_destroy_session_with` cleans up the target before deleting the durable session. `controller/checkpoint/layout.rs` and backend export helpers use the same resolver. `server/api/files.rs::export` dispatches patch, branch, and bundle exports. Follow those operations through the daemon backend and its API error conversion to preserve a typed legacy-repair conflict instead of a 500.

`controller/provisioning.rs`, `controller/resume.rs`, recovery code, and the persisted move workflow decide when a locator becomes durable or is replaced. Those transitions must carry the matching access settings. Existing target cleanup plans and shared subprocess helpers remain responsible for stopping the process group before removing files and for reporting cleanup errors.

## Plan of Work


### Milestone 1: Durable access belongs to the provisioned target


Introduce a small serde-compatible access type in mj-core with local, SSH, and EC2 access information. Preserve target-kind validation. Extend the stored target record with optional access metadata for legacy rows and store it in `sessions.target_runtime_json`. Use the existing SshConnection representation where possible. Include AWS profile, region, SSH user/options/key path where required; keep the instance ID and resolved address in the locator.

Add the next schema migration and update compatibility metadata atomically. Classify it as breaking with a comment explaining older writers retaining access settings for the wrong target. Exercise isolated upgrades from historical revisions and prove older readers/writers refuse the upgraded store. Test access metadata round trips and survives ordinary state updates and reopen.

Capture access settings from the selected template before provisioning side effects, retain them when partial provisioning creates a cleanup obligation, and persist them with the resulting locator. Explicit target replacement must replace both facts atomically through existing lifecycle persistence. Account for move boundaries and borrowed children so a failed move can still clean up each actual resource on its original host.

Acceptance: a persisted session's complete connection survives restart without consulting its template. A later config edit cannot rewrite its destination.

### Milestone 2: Existing-target operations use one durable resolver


Refactor the existing StoredTarget conversion and backend_locator to take the recorded access settings. Keep template conversion for creating new targets and discovering unowned resources; remove unconditional current-template lookup for operating a known target. Audit direct template reads in worker launch and reviewer code, distinguishing provisioning inputs from access to a running resource.

Backfill legacy access only from an available template that matches the locator's kind and host. Persist backfill before admitting operations that rely on it and before replacing an old config during reload. Local locators need no template. For incomplete remote legacy records, use a typed error that becomes HTTP 409 and a useful CLI message identifying the session and missing target. A present but mismatched template must also fail without executing any remote command.

Do not change the actual export or cleanup machinery. Preserve existing background execution, command cancellation, borrowed-container ownership, and process-before-files teardown. If cleanup fails, retain the locator, access settings, and durable session so the user can retry.

Acceptance: remove the template, restart the controller, resolve the shared branch/bundle export layout and branch command, then force destroy an isolated session. All commands target the originally recorded resource. Separately exercise checkpoint export after template removal and HTTP branch/bundle conflict mapping; retain the existing export-content and API route tests. Reusing the same template name for a different host has no effect on that session.

### Milestone 3: Behavior coverage and delivery


Use existing hand-written CommandExecutor fakes and isolated database/API fixtures. Cover local and remote target families at the conversion boundary, with SSH Podman as the issue's end-to-end regression. Assert command destination, SSH options, resource ownership, exported data, and destruction order rather than a copied list of implementation steps.

Test creation followed by template removal and restart; name reuse for a different host; partial provisioning cleanup; persistence through session updates and target moves; borrowed-child destruction that leaves the parent's container intact; legacy backfill; an already orphaned remote legacy record returning 409 without running commands; and a cleanup failure preserving a retryable record. Retain existing branch/bundle export content tests. Use a temporary local repository and a fake SSH/container executor; do not touch morannon, real running containers, or live sessions.

Run focused tests first, then the full dev-profile workspace suite, all-targets clippy, formatting, and diff review. Any manual new-build exercise must use a separate named instance such as `--instance target-access-1018` plus isolated config/data directories. Do not redirect Rust build output or alter mbx caching. Commit the validated changes on the current branch, push HEAD:master as authorized, close #1018, and remove the active-work label. Then plan #1063 and wait for its separate approval.

## Concrete Steps


Work from `/home/jonathan/Projects/mjolnir3`. Application implementation is approved and underway. Focused checks should use the actual new behavior test names in mj-core conversion tests, controller database/schema tests, lifecycle tests, and API export tests.

Required final checks are:

    cargo fmt --all -- --check
    git diff --check
    env -u NO_COLOR cargo test
    cargo clippy --all-targets -- -D warnings

Every cargo test runs elevated outside the restricted sandbox, with the tests' existing isolated stores. Never run the new daemon or CLI against the default instance. If the implementation reveals a different storage boundary that can safely preserve access under older writes, prove that with an older-writer regression before changing the proposed breaking classification; uncertainty remains breaking.

## Validation and Acceptance


A newly recorded session continues to export and destroy using its original complete access settings after config removal, config name reuse, and daemon restart. The result must not depend on which template happens to exist now. Force destroy stops the resource before deleting files or records, and leaves a retryable record on failure.

Older sessions backfill while their matching template is available. A remote legacy session that already lost required settings produces a 409 identifying exactly what must be restored, preserves its work, and sends no guessed remote command. Schema compatibility tests prove older processes are refused before reading or writing the new revision.

## Idempotence and Recovery


Backfill is idempotent and never overwrites an existing access snapshot. Creation and explicit target replacement persist locator and access settings together. Failed cleanup leaves both available for retry. Migration tests use disposable databases; no live schema or target is changed during the implementation exercise. Session data and private-key contents never enter planning artifacts.

## Outcomes & Retrospective


Implementation and validation are complete. Existing sessions retain access independently of target-template removal or name reuse. Isolated regressions cover SSH Podman restart/export resolution/destroy retry, borrowed children, missing legacy configuration, concurrent backfill/deletion, SSH/AWS conversion, resume rollback, target replacement, migration preservation, checkpoint export and HTTP conflicts. Final workspace tests pass, including 1,594 controller tests and 453 core tests, along with all-targets clippy, formatting, and diff checks. The live database and real remote resources were not touched.

Migration 46 preserves session data and refuses older readers/writers. A remote legacy record that lost its connection configuration before backfill still requires restoring that original template once. Explicit creation/resume still requires the selected destination template. Published as `d81fd67c` to origin/master and closed #1018. #1063 now has a separate review plan; its implementation awaits approval.

Revision (2026-09-23): approval received. Store the snapshot on the session row so access settings become durable before provisioning creates any resource, including partial failures without a locator. Revision 46 is breaking because older writers can change a locator while retaining an access snapshot for the previous resource. Preserve execution policy and target environment as well as connection settings: restarting an existing worker needs those inputs after template removal. Backfill uses a conditional database update and returns the winning snapshot, preventing concurrent backfills from overwriting saved settings.

Revision (2026-09-23): the audit also found raw checkout inspection and mbx host-configuration reads consulting mutable templates. They now use saved access. The SSH Podman regression uses a recording executor to prove destination and destruction behavior without real hosts; transport tests exercise the HTTP boundary separately. No production daemon, remote target, or live store is used.

Revision (2026-09-23): conditional backfill now returns an optional saved snapshot. A missing or retargeted database row is expected concurrent state, so reload proceeds and replaces the stale controller state. Existing snapshots still win concurrent backfills. Added an isolated reload regression and reran required checks after this final source change.
