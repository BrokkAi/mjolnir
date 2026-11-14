# Store one container size per session so capacity accounting counts what containers use

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

Issue: https://github.com/BrokkAi/mjolnir/issues/1538


## Purpose / Big Picture

Mjolnir (the `mj` command, its daemon and its workers) runs coding agents in containers on a host. Before it starts a container it checks that the host has room for it: that check is called "admission" in this repository. On Apple container hosts RAM is a hard limit, so admission refuses a container that does not fit; elsewhere it only warns about overcommit.

Today a session's container size is stored twice. The new-session wizard, the Resume and Move dialogs and `mj new --cpus/--memory-gib` write `resource_allocation`, an integer CPU count plus a memory size in bytes. The Container settings editor (Ctrl+E in the terminal dashboard, the Container settings dialog in the web viewer, and the `set-container-settings` API action) writes two free-form strings, `container_cpus` and `container_memory` (for example `"1.5"` and `"8g"`). The strings win when the container is created, but admission and the host capacity totals read only `resource_allocation`. So a session resized in Container settings is counted at its old size, the resize is never checked against the host, and on Apple hosts a larger container can pass admission and then fail. There is a second defect: a size chosen in the Resume or Move dialog loses to an older Container settings string.

After this change each session has one size, `resource_allocation`. Container settings edits it, validates it the way the wizard validates a size, and the next container creation is admitted at that size. Existing strings are converted once by a database migration. A person can resize a session in Container settings, suspend and resume it, and see the host capacity shown in the dashboard and web viewer account for the new size; on an Apple host a resize that does not fit is refused at resume with the normal capacity message instead of failing in the container engine.


## Progress

- [x] Separated Move applied-source recovery from frozen keep-request inheritance; extended the existing failed-outcome regression for two failures and keep acceptance.
- [x] Final review-fix validation: workspace Clippy and fmt pass; core 651 passed/4 ignored; controller 2634 passed/27 ignored with unrelated mbx probe failure (picked user-bin 1.31 instead of symlink 1.30); TUI 588 passed/2 ignored with the two already documented base goldens failing. Extended Move retry/keep regression passes. Logs: `.mj/agents/ecce6678b0ecd0f8e26f6a3e960d0c78/{clippy,fmt,suites}.log`; failure diagnostics in `failures.md` there.


- [x] (2026-11-14) Follow-up request ownership: Resume/Move acceptance alone writes explicit size/mount choices; lifecycle publications and rollback preserve newer requests. Central applied-settings lookup and all-reader audit complete, including inherited TUI Move choices and removed-locator upgrade recovery. Final controller suite: 2635 passed/27 ignored; core: 651 passed/4 ignored; final Move module: 61 passed; workspace Clippy and formatting passed. TUI: 588 passed/2 ignored, with only the two documented baseline goldens failing. Evidence: `.mj/agents/f5843c6b3c11ad265083711bafa5ee82/validation.md`.

- [x] Follow-up #1538: revision 131 now converts embedded Move recovery/transfer records, effective source recovery sizing, retained sources and environment snapshots. Existing regression extended with the requested Hard-won assertion; final targeted migration tests (2) and controller-only Clippy passed.

- [x] (2026-11-13 23:00Z) Map the current code (read-only child report, summarized in Context and Orientation).
- [x] (2026-11-13 23:00Z) User chose the one-size design and the conversion rule (Decision Log).
- [x] (2026-11-13) Read AGENTS.md and PLANS.md; inspected store, capacity, editor and action contracts.
- [x] Parent clarified public mount-history behavior; preserve the current contract.
- [x] Milestone 1: revision/floor 131 migration implemented; previous-store conversion and incompatibility regression passed. Existing lifecycle/checkpoint stale-write regressions passed after separating allocation ownership.
- [x] Milestone 2: allocation-only runtime/API, applied-size counting, Move commitments, projection updates and protocol 97 implemented; workspace Clippy passes.
- [x] Milestone 3: terminal/browser editors, copy launch and documentation updated; both terminal goldens and the browser dialog golden passed.
- [x] Milestone 4: isolated cs1538 Podman scenario and cleanup passed; full touched-crate suites ran, all change-related failures were fixed and targeted reruns passed. Formatting and workspace Clippy pass. Only the two documented base-tree TUI goldens remain failing.


## Surprises & Discoveries

- Observation: removing deny_unknown_fields SessionRecord fields also invalidates old MoveOperation.recovery_session and workspace_transfer.source JSON. Revision 131 must convert those records before readers decode them. Additional durable copies exist in retained_move_sources.source_json, environments.placement_json (runtime and prepared destination), and environment_workers.runtime_json. MoveOperation has no separate source CPU/memory string fields. Its typed source_resource_allocation nevertheless needs folding with recovery_session overrides: the old failed-Move Resume combined precisely those values (daemon/session_move.rs in the base tree). Revision 131 preserves that effective source size before removing the recovery strings; when recovery_session is absent, it uses the session row strings that legacy Resume would have retained. The follow-up converts only known persisted SessionRecord and ContainerSettings JSON locations, preserving unrelated JSON including retired harness identities and environment variables named container_cpus.

- Observation: the chat transcript golden uses UTC timestamps; the inherited America/Chicago timezone rendered 18:00 instead of 00:00. Its targeted rerun passed with TZ=UTC, which was then used for the remaining full suites. No golden was changed for this environment difference.

- Observation: older-store regressions also rewind schema through drop_environment_schema and a separate daemon_startup old_store fixture. They must restore removed size columns; the revision-129 watch fixture must remove all later ledger entries and assert the current compatibility floor. Full-suite failures exposed these fixture-only assumptions.
- Observation: the live regression must compare engine IDs, since the container locator keeps the same name across recreation; the viewer calls stopped sessions "suspended". The generic lab wait was shorter than measured container worker setup, so the scenario uses the existing container regression’s 120-second wait. All intermediate attempts cleaned up their owned resources.

- Observation: the existing stale lifecycle/checkpoint regressions expose that generic lifecycle saves own resource_allocation today. With settings now writing that field, they overwrite a newer settings edit. Removed allocation writes from generic lifecycle updates; the later ownership follow-up moves explicit Resume/Move request changes into acceptance transactions and removes them from all lifecycle publications, including destination adoption. Existing regression names: lifecycle_save_preserves_container_settings_and_mounts and checkpointed_save_preserves_container_settings_and_mounts.

- Observation: Move source reservations compare their allocation with the live source allocation; both now use counted_container_allocation so resized live sources are not charged twice or at their pending request.
- Observation: fixtures rewinding the current schema must restore removed columns before replaying revision 131; updated stamp_schema_version. Fresh baseline construction supplies historical columns inside its transaction, then revision 131 removes them.

- Observation: Interfaces and Dependencies says the public `SetContainerSettings` action carries `mount_history` "as now", but the existing action has no such field. `mj-controller/src/server/actions.rs:165` defines only session ID, CPU, memory and mounts; `server_runtime/actions.rs:549` derives mount history from controller state. The TUI internal action does carry it (`mj-tui/src/dialogs/container.rs:352`). The parent approved retaining existing public mount-history behavior and replacing only CPU/memory with resource_allocation; implementation resumed.

- Observation: the live store on the development host has no session with a Container settings override, so the conversion only matters for other installations.
  Evidence: `sqlite3 -readonly ~/.local/share/mjolnir/mj.sqlite3 "select count(*) from sessions where container_cpus is not null or container_memory is not null"` printed `0` on 2026-11-13.


## Decision Log

- Decision: Move source allocation/mounts describe only the applied source via `applied_container_settings`; separate defaulted `inherited_resource_allocation` and `inherited_additional_mounts` freeze the request at acceptance for keep destinations. Capacity source commitments already use `counted_container_allocation`.
  Rationale: a failed explicit destination choice remains the request, so retry recovery must not mistake it for applied source settings. These fields extend the unshipped breaking revision 131; no new migration is needed. `MoveOperation` denies unknown fields and is exposed in protocol-97 snapshots/feeds: earlier 97 clients reject populated new fields, whereas this client accepts older payloads with defaults.
  Date/Author: 2026-11-14, parent decision implemented by worker.


- Decision: size and mounts on SessionRecord are requests for the next creation. Only creation, explicit Resume/Move choices, and Container settings write them; lifecycle publications and rollback preserve them. Keep selections remain omitted through Move preparation. Commit explicit choices with operation acceptance.
  Rationale: a provisioning controller's stale snapshot must never overwrite settings saved while provisioning runs. Failed operations retain the person's requested settings.
  Date/Author: 2026-11-14, parent.

- Decision: one mj-core applied_container_settings lookup owns applied allocation and mounts. Use an existing applied snapshot even when its size is absent (unknown). Fall back to the request only when no snapshot exists (legacy). Capacity reservations for a provisioning session without a target count its next-creation request, independently of any retained prior snapshot.
  Rationale: the legacy request is the only creation record available; no inspection or reconstruction is needed. Every applied-mount reader shares this answer with capacity accounting.
  Date/Author: 2026-11-14, parent.

- Decision: the public SetContainerSettings action keeps no mount_history field; its handler derives history from controller state. The internal TUI action and controller method retain their parameter.
  Rationale: correct the plan’s description to preserve existing behavior; replace only CPU/memory fields.
  Date/Author: 2026-11-13, parent approval.

- Decision: keep only `resource_allocation`; remove `container_cpus` and `container_memory` from the session record, the database, the applied snapshot, the `set-container-settings` action and `CreateSessionRequest`.
  Rationale: admission, reservations, Move commitments, the published capacity and the applied snapshot already key on `resource_allocation`. Keeping the strings would need a second interpreter of engine size syntax (fractions, `m`/`g`/`GiB` suffixes) used everywhere and would keep the Resume/Move defect. The repository rules prefer one representation over parallel paths.
  Date/Author: 2026-11-13, user decision on the parent's recommendation.

- Decision: the migration rounds fractional CPU counts up to the next whole CPU, converts memory strings to exact bytes, drops strings it cannot parse, and logs every changed or dropped session at WARN.
  Rationale: the user chose "round up, log it" so no container gets less than it had. Memory needs no rounding because `memory_bytes` is in bytes, so exact bytes already meet that rule and stay closer to the original than rounding to whole GiB (the user was offered "next GiB"; exact bytes is never smaller and is reported back to them).
  Date/Author: 2026-11-13, user decision; memory detail by the parent.

- Decision: the migration is breaking. It drops the two columns and raises the minimum compatible store revision in the same transaction.
  Rationale: an older daemon that still writes the strings would have its writes ignored by the new one, so older writes are not preserved. AGENTS.md treats that as breaking. The user chose removing the columns.
  Date/Author: 2026-11-13, parent.

- Decision: bump the daemon wire protocol (`PROTOCOL_VERSION` in `mj-client/src/daemon.rs`, now 96) to 97.
  Rationale: the `set-container-settings` body changes shape and `CreateSessionRequest` loses two fields. A protocol bump is how this repository makes an older client and a newer daemon replace one another instead of exchanging requests the other side misreads. Older web pages and `mj api` scripts that still send the string fields must get a clear refusal naming the new fields, not a dropped connection.
  Date/Author: 2026-11-13, parent.

- Decision: count a session that owns a running container at the size recorded in its applied snapshot (`ContainerSettings.resource_allocation` in `target_runtime.container_settings`), and a session still provisioning at its requested `resource_allocation`. Use the requested size for a legacy container that has no snapshot.
  Rationale: with one size, Container settings changes `resource_allocation` while the running container keeps its old limits until it is recreated. Counting the request would charge the host for a size nothing uses yet, and on a reduction would undercount what the container still holds. The snapshot is the record of what was created; the request is what admission checks at the next creation. Legacy containers have no snapshot, and their request is the best record that exists.
  Date/Author: 2026-11-13, parent.

- Decision: saving Container settings validates the size like the wizard does (whole CPUs, memory in GiB, the target's limits), but does not run admission. Admission runs at the next creation (resume, non-in-place Move), as it already does.
  Rationale: saving changes nothing that is running. The size takes effect at the next creation, and that is where host room is decided. Checking free room at save would be a second admission against a moment that is not the launch. A size that is already stored and smaller than the current minimum for new sizes stays valid on resave, so old sessions are not forced up.
  Date/Author: 2026-11-13, parent.

- Decision: a CPU-only or memory-only override takes its other half from the session's existing allocation. With no allocation, it is dropped and logged.
  Rationale: an allocation needs both values. Migrations must not depend on configuration files, so the target template's free-form defaults are not read. The running container keeps its limits until it is recreated, so nothing changes at upgrade.
  Date/Author: 2026-11-13, parent.

- Decision: the target template's own `cpus` and `memory` strings (`ContainerTemplate` in `mj-core/src/config/targets.rs`) stay free-form and out of scope.
  Rationale: they are configuration defaults, not per-session state, and changing them is a separate configuration change.
  Date/Author: 2026-11-13, parent.


## Outcomes & Retrospective

Follow-up ownership fix: explicit Resume and Move settings are committed atomically at acceptance; even an unchanged Move accepts explicit requests. Later Container settings edits survive converted-checkpoint publication, destination adoption and failed-operation rollback. The applied lookup owns legacy fallback for size and mounts; a snapshot remains authoritative if recovery has removed the locator. Provisioning capacity independently reserves the next-creation request. Move creates from its admitted request snapshot while subsequent edits remain requested. Final checks passed except the two existing TUI goldens listed above. The parent’s Hard-won unknown-size regression remains intact.

Implementation is complete across store, runtime, and clients. Migration, stale lifecycle/checkpoint writes, terminal goldens, and browser dialog goldens have passed their targeted checks. One requested allocation now controls editors, launch, admission and provisioning; running owners are counted at their applied snapshot. Revision 131 converts and removes the legacy strings and fences revision-130 writers. Protocol 97 carries the replacement allocation. The real cs1538 resize/recreation and cleanup passed. Full touched-crate suites ran; all change-related failures were corrected and their targeted reruns passed. Formatting and workspace Clippy pass. The two documented base-tree TUI goldens remain outside this work. Live Apple/Nitro engines and macOS/Windows were not exercised.


## Context and Orientation

Terms used here. The daemon is the long-running `mj daemon-run` process (crate `mj-controller`). It owns the SQLite store at `~/.local/share/mjolnir/mj.sqlite3` (for an isolated instance, under that instance's data directory). Workers (crate `mj-worker`) run the agents. The terminal dashboard is crate `mj-tui`, run by `mj` (crate `mj-cli`). The web viewer is served by the daemon from `mj-controller/src/web/viewer.js`, `viewer.html` and `viewer.css`. `mj-client` holds the wire types the daemon and its clients share. A session record is `mj_core::state::SessionRecord` (`mj-core/src/state.rs`, around line 1612). A target template is a configured place to run sessions (`TargetTemplate`, `mj-core/src/config/targets.rs`); a container target is one where `mj_core::config::is_container_target` returns true.

Sizes. `SessionResourceAllocation` (`mj-core/src/state.rs` around line 1209) is either `Container { cpus: u64, memory_bytes: u64 }` or `AwsEc2 { .. }`. The strings to remove are `SessionRecord.container_cpus` and `container_memory` (around lines 1699 and 1703), stored in the `sessions` table columns of the same names (`mj-controller/src/database/baseline.sql` around 284; added by `ensure_session_container_override_columns` in `mj-controller/src/database/legacy_schema.rs` around 763; row reads and writes in `mj-controller/src/database/state_io.rs` around 649-723 and 1485). The applied snapshot is `ContainerSettings` in `mj-core/src/state/target_runtime.rs` (around line 30): it records `additional_mounts`, `resource_allocation`, `cpus` and `memory` as they were when the container was created (`mj-controller/src/controller/provisioning.rs` around 919-929). It is `None` for containers created before it existed.

Who writes the strings today. The terminal editor `mj-tui/src/dialogs/container.rs` (save around 339-352, load around 562-563). The daemon action `SetContainerSettings` (`mj-controller/src/server/actions.rs` around 160; handler `mj-controller/src/server_runtime/actions.rs` around 540-573; validation `mj-controller/src/server/validation.rs` around 733), used by the web viewer dialog (`viewer.js` around 6120-6215) and `mj api`. `Controller::update_session_container_settings` (`mj-controller/src/controller.rs` around 1491-1572) stores them through `mj-controller/src/database/sessions.rs` around 1153-1205. The copy launch "New session with these settings" (`mj-tui/src/wizards/dashboard/new_session.rs`, `new_session_like_request`) carries them through `CarriedLaunchSettings` (`mj-tui/src/lib.rs`), `mj-cli/src/dashboard/io/spawn.rs` and `CreateSessionRequest` (`mj-client/src/daemon.rs` around 655) into `controller.rs` around 1078-1128, which refuses them for non-container targets. A Move with `clear_resource_allocation` clears them (`mj-controller/src/controller/move_session.rs` around 2362).

Who reads them. `ContainerOverrides::for_session` (`mj-controller/src/controller/backend.rs` around 757) passes them to `backend_container_with` (around 1040-1075), which picks the override string, else the allocation, else the template string, and emits `--cpus=` and `--memory=` arguments. Consumers include `mj-controller/src/targets/container.rs` around 702, `nitro.rs` around 204-209 and `provisioning.rs` around 1752.

Capacity. `mj_core::state::host_container_capacity` (`mj-core/src/state.rs` around 1835) sums `resource_allocation` over sessions for which `session_owns_container` is true; a live owner with no `Container` allocation marks the total incomplete, which counts the host as full. Admission is `admit_container_launch` in `mj-controller/src/daemon/capacity.rs` (around 286), called from create (`daemon/create.rs` around 475), resume (`daemon/resume.rs` around 5-30) and non-in-place Move (`daemon/session_move.rs` around 112-168). Reservations, Move source commitments, lease joining and the published projection (`capacity.rs` around 21-33, 233, 421, 502, 573-645) key on the allocation.

Size helpers to reuse. `mj-tui/src/wizards/resources.rs` (around 148, `parse_memory_gib`, and its formatters) and the web twin `parseMemoryGiB` in `viewer.js` (around 2360). Size validation for new sizes is `validate_minimum_useful_container` and the general check is `validate` (find them with `rg -n "fn validate_minimum_useful_container|fn validate_resource_allocation" mj-core mj-controller`).

Store revisions. `SCHEMA_VERSION` is the current revision and `MINIMUM_COMPATIBLE_VERSION` (131 in `mj-controller/src/database/schema.rs`, test-side constant) is the floor. Find how a breaking migration raises the floor in the same transaction by reading the most recent breaking migration (`git log -S MINIMUM_COMPATIBLE -- mj-controller/src/database`). Never edit an applied migration; add a new revision.


## Plan of Work

Milestone 1, migration and store. Add a new migration revision after the current one. In one transaction it converts and then drops the columns, and raises the minimum compatible revision to the new revision. Conversion, per session row with a non-null override: parse CPUs as a decimal number and round up to a whole CPU (values at or below zero, or unparseable, are dropped); parse memory as digits with an optional fraction and an optional suffix `b`, `k`/`kb`/`kib`, `m`/`mb`/`mib`, `g`/`gb`/`gib`, `t`/`tb`/`tib` (case-insensitive; treat the single letters and `ib` forms as powers of 1024, matching how container engines read them; treat `kb`/`mb`/`gb`/`tb` the same way, because Docker and Podman do) into bytes rounded up to a whole byte; unparseable values are dropped. If both halves resolve, or one resolves and the row already has a `Container` allocation to supply the other, write `resource_allocation` as a `Container` allocation. If the row's allocation is `AwsEc2`, leave the allocation as it is and drop the strings. Apply the same rule to each session's applied snapshot JSON (`target_runtime.container_settings`): fold its `cpus`/`memory` into its `resource_allocation` and remove the two keys. Return a list of changed and dropped sessions from the migration so startup logs one WARN line per session with the old strings and the new allocation. Remove the two fields from `SessionRecord`, `ContainerSettings`, baseline schema and row I/O; keep `legacy_schema.rs` able to upgrade older stores through the new revision. Add the migration regression described under Validation.

Milestone 2, daemon and wire. Remove `cpus` and `memory` from `ContainerOverrides`; `backend_container_with` picks the allocation, else the template string. Change `SetContainerSettings` to carry `resource_allocation: Option<SessionResourceAllocation>` alongside the mounts, with `deny_unknown_fields` kept and a refusal message for bodies that still name `cpus` or `memory` ("Container settings now take resource_allocation (whole CPUs and memory bytes); reload the page or update the script."). Change `update_session_container_settings` to take the allocation, validate it for the session's target as the wizard does (keeping an already stored smaller size valid), and store it. Remove `container_cpus` and `container_memory` from `CreateSessionRequest`, `SessionLaunchOptions` and the registration path, including the non-container refusal added for them. Remove the Move clearing logic that existed only for the strings. Change `host_container_capacity` so an owner with a target locator is counted at its applied snapshot allocation when the snapshot has one, and otherwise at its `resource_allocation`; a session with no locator yet (provisioning) is counted at its `resource_allocation` as now. Make the published projection republish when an owner's counted size changes. Bump `PROTOCOL_VERSION` to 97 and follow whatever the repository does on a bump (search the last bump with `git log -S "PROTOCOL_VERSION: u32 = 96" -- mj-client`).

Milestone 3, clients and documentation. The terminal Container settings editor shows CPUs as a whole number and memory in GiB using the `resources.rs` helpers, saves an allocation, and shows the validation error inline. The web dialog does the same with `parseMemoryGiB`. The copy launch drops its two carried fields; it already copies the owner's `resource_allocation`, which is now the whole size. Update the user documentation under `docs/src/content/docs/` that describes Container settings and `mj api set-container-settings`, and the help text that says sizes apply "the next time the container is created" if its wording changes.

Milestone 4, end to end. Run the isolated scenario in Validation and the full checks.


## Concrete Steps

All commands run from the repository root. Run `cargo test` outside the restricted sandbox (it uses loopback sockets). Do not redirect Cargo's target directory. While iterating, run only the tests you touched, for example:

    cargo test -p brokk-mj-controller --lib -- database::tests::<name> capacity
    cargo test -p brokk-mj-tui --lib -- container new_session_like

After the last milestone run once:

    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test -p brokk-mj-core -p brokk-mj-client -p brokk-mj-controller -p brokk-mj-tui -p brokk-mjolnir

Two mj-tui goldens fail on the base tree and are not part of this work: `pane_controls::tests::golden_support_pane_title_menu` and `setup::tests::goldens::golden_settings_root` ("8 settings" versus "9 settings").


## Validation and Acceptance

Migration regression (integration between the store schema and the data-access code): build a store at the previous revision with rows for each case and migrate it. Expect: `"4"`/`"8g"` with no allocation becomes `Container { cpus: 4, memory_bytes: 8589934592 }`; `"1.5"`/`"700m"` becomes `cpus: 2, memory_bytes: 734003200`; CPU-only `"6"` with an existing `Container { cpus: 2, memory_bytes: 4 GiB }` becomes `cpus: 6` with 4 GiB; CPU-only with no allocation and `"lots"` memory are dropped and reported; an `AwsEc2` row keeps its allocation; a snapshot with `cpus: "3"` and `memory: "6g"` is folded the same way; the columns are gone and the minimum compatible revision equals the new revision. An older daemon revision must then refuse the store as incompatible (extend the existing incompatible-store regression rather than adding a new family).

Capacity (tricky condition: requested versus applied): a running owner created at 4 CPUs / 8 GiB whose `resource_allocation` was then edited to 8 CPUs / 16 GiB is counted at 4 / 8 until its container is recreated; a provisioning owner is counted at its request; resume of the edited session is admitted at 8 / 16 and, on a hard-RAM host without room, refused with the existing capacity refusal.

Goldens: extend the Container settings editor golden in mj-tui so it shows CPUs and GiB fields and a validation error; extend the web viewer test that covers the Container settings dialog (find it under `tests/e2e/web/`).

End to end, in an isolated instance (never the default instance): build, start `mj --instance cs1538 ...` with its own `MJ_CONFIG_DIR` and `MJ_DATA_DIR`, create a Podman container session at 2 CPUs / 4 GiB, change it to 4 CPUs / 8 GiB through `mj api` `set-container-settings` with the new body, confirm a body with `cpus` is refused with the message above, suspend and resume it, and confirm the new container runs with `--cpus=4` and `--memory=8589934592` (`podman inspect` shows `NanoCpus` 4000000000 and `Memory` 8589934592) and that `mj api` capacity output counts 4 / 8 GiB for it.


## Idempotence and Recovery

The migration runs once per store inside one transaction; a failure rolls it back and the daemon reports the store error. Before running a new build against any store that matters, copy the store. The live store on the development host must not be migrated by a test build; installing this release migrates it, and because the migration is breaking, installation needs the user's explicit approval at that time.


## Artifacts and Notes

Follow-up embedded-record audit and recovery semantics: `.mj/agents/7389d8c6d3737a994d268a69f4be5992/move-json-audit.md`. Final targeted migration tests passed (2 tests) in `move-json-verified-tests.log`; final controller-only Clippy passed in `move-json-verified-clippy.log`.

Contract inspection evidence: `.mj/agents/7389d8c6d3737a994d268a69f4be5992/contract-mismatch.txt`. Migration regression passed; initial targeted failures from stale column indices were corrected. A subsequent run passed 101 controller tests and exposed two existing stale-write regressions, now addressed by allocation writer ownership. Browser dialog golden passed (`web-final.log`). `cargo fmt --check` and workspace `cargo clippy --all-targets -- -D warnings` passed (`fmt.log`, `clippy-final.log`). Terminal editor/session menu goldens passed (`tui-goldens.log`); Isolated cs1538 Podman recreation passed (`e2e-final.log`): NanoCpus=4000000000, Memory=8589934592, capacity committed=4 CPUs/8 GiB. Nineteen migration fixture reruns passed (`migration-fixtures.log`); the final usage-report fixture rerun passed in `usage-fixture.log`. Three startup regressions passed (`startup-fixtures.log`). Copy launch golden rerun passed in `copy-golden.log`. Final formatting and workspace Clippy passed (`fmt.log`, `clippy-complete.log`). Final validation detail and failure-by-failure resolutions are in `validation.md` and `failures.md` in the same report directory.


## Interfaces and Dependencies

In `mj-controller/src/server/actions.rs`, `SetContainerSettings` carries `session_id`, `mounts`, `resource_allocation: Option<SessionResourceAllocation>`. In `mj-controller/src/controller.rs`:

    pub fn update_session_container_settings(
        &mut self,
        session_id: &str,
        resource_allocation: Option<SessionResourceAllocation>,
        additional_mounts: Vec<targets::AdditionalMount>,
        mount_history: Vec<std::path::PathBuf>,
        executor: &impl CommandExecutor,
    ) -> Result<()>

`ContainerOverrides` keeps only `no_github_credentials`. `mj_core::state::ContainerSettings` keeps `additional_mounts`, `resource_allocation` and `default_image`. `mj_client::daemon::CreateSessionRequest` has no container size strings.


Revision note: initial plan written 2026-11-13 by the parent from the read-only map and the user's choices.
