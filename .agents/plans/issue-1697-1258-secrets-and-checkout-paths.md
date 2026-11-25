# Keep resolved secret environment values out of persistence and stop storing derivable checkout paths (#1697, #1258)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This repository keeps its ExecPlan standard in `.agents/PLANS.md`; maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Two related persistence defects currently duplicate data in SQLite and external runtime metadata where it does not belong. First (#1697), configured target and profile `environment` entries backed by `{ from_env = "..." }` or `{ from_secret = "..." }` have their resolved secret values persisted verbatim into `sessions.target_runtime_json` (and embedded copies in `session_moves`, `retained_move_sources`, `environments.placement_json`, and `environment_workers.runtime_json`), returned in daemon HTTP session-record responses, copied into reviewer `subagent_sessions.record_json`, and emitted onto container launch `--env` arguments where `podman inspect` records them permanently. Second (#1258), session records duplicate checkout paths that are already derivable: sub-agents copy their parent's `project_directory`, and managed worktrees store both the `managed_worktree` descriptor and a redundant `project_directory` copy.

Because both changes require a breaking SQLite schema revision (raising the minimum compatible schema revision to 157 so older daemons cannot silently drop `environment_references` or fail on pathless managed-worktree records), this plan batches them into one breaking revision 157. After this change, resolved secret values never appear in SQLite, HTTP session responses, or container `inspect` metadata — they are resolved from secret stores once per launch and delivered solely through `0600` `launch.json`, letting secret rotation take effect on Restart and letting us delete Nitro's special-case out-of-band secret-env plumbing and dead reviewer environment persistence. Meanwhile, `sessions.project_directory` stores a path only for attached checkouts (and legacy orphan records), managed worktree paths are derived from `ManagedWorktree`, and project catalog discovery and workspace classification handle pathless managed worktrees cleanly.

## Progress

- [x] (2026-11-25 00:08Z) Audited all secret-environment leaks (`#1697`), mapped all `project_directory` writers/readers/triggers (`#1258`), confirmed owner approval for breaking schema revision 157 and excluding secret-sourced vars from pre-worker provisioning `exec`, and authored this ExecPlan.
- [x] (2026-11-25) Implemented `#1697`: `TargetRuntimeSettings.environment` holds literals only and `environment_references` the references; `launch_environment()` is the one resolver, called once per launch (staging `worker_launch_config`, `session_launch_config`, `current_worker_launch_config`, `aws_session_inputs`); `container_environment` emits literal `--env` only for every engine and Nitro's `secrets_out_of_band` plumbing is gone (`GH_TOKEN` keeps its path); `inspect_retained_container` sheds recorded reference keys; `ReviewerLaunchConfig.environment` is `#[serde(default, skip_serializing)]` and no longer written or applied.
- [x] (2026-11-25) Implemented `#1258`: `ManagedWorktree::checkout_directory` derives the path; `Checkout::ManagedWorktree.project_directory` and `Checkout::project_directory()` use `Cow<Path>`; settled managed worktrees, resume clone upgrades, workspace-to-raw conversion and borrowed sub-agents store `project_directory = NULL`; null-sensitive consumers (`projects.rs`, `agent_repo_urls`, `mbx/release.rs`, `EnvironmentPlacement::from_session`, review host, startup context, discovery) and the record-only display/launch paths (`configuration_issue`, `publication_state`, project-memory identity) now use the State-resolved checkout.
- [x] (2026-11-25) Implemented breaking revision 157 (`SCHEMA_VERSION = MINIMUM_COMPATIBLE_VERSION = 157`) in `schema.rs`, plus `target_secrets.rs`: `scrub_resolved_target_secrets(&config)` runs at daemon start before the first state load and moves resolved values out of every recorded runtime copy (`sessions`, `session_moves`, `retained_move_sources`, `environments`, `environment_workers`).
- [x] (2026-11-25) Documented the secret-sourced provisioning-`exec` exclusion in `docs/src/content/docs/targets.md`; ran fmt, strict Clippy and the targeted suites listed under Outcomes; committed on the detached HEAD.

## Surprises & Discoveries

- Observation: `TargetRuntimeSettings.environment` is not only persisted in `sessions.target_runtime_json` and embedded move/environment JSON, it is also returned directly by HTTP session-record endpoints (`mj-server/src/server/api/dashboard.rs:337-365`, `SessionRecordWithVerdict`). Separating literals from `environment_references` on `TargetRuntimeSettings` closes the HTTP leak automatically.
  Evidence: `.mj/agents/e5ed2235e36a92a00c378a4b24121366/1697_analysis.md` section 2a.
- Observation: `ReviewerLaunchConfig.environment` is dead weight. Its sole reader (`mj-worker-launch/src/worker_launch.rs:875-881`) extends a launch already built from `config.profiles[session.last_profile]`, which already applies `profile.environment.resolved()`, `exclude_harness_environment`, and `IS_SANDBOX`. Because `ReviewerLaunchConfig` has `#[serde(deny_unknown_fields)]`, the field must remain deserializable (`#[serde(default, skip_serializing)]`) for legacy rows while no longer written.
  Evidence: `.mj/agents/e5ed2235e36a92a00c378a4b24121366/1697_analysis.md` sections 2c and 4.
- Observation: Several SQL queries and Rust predicates currently treat `project_directory IS NULL` as synonymous with "managed bundle workspace": `mj-store/src/database/projects.rs:169,207`, `mj-daemon/src/server_runtime/api.rs:4103`, `mj-build-cache/src/mbx/release.rs:521`, and project catalog triggers in `project_catalog_v67.sql:54-70` / `schema.rs:1802-1818`. All of these must explicitly check `managed_worktree` presence so pathless managed worktrees are not misclassified.
  Evidence: `.mj/agents/43b868f90802b28fbb4db80aeab00f58/1258_map.md`.
- Observation: `master` advanced `SCHEMA_VERSION` to 156 (`ec2_move_grants` `region`/`aws_profile` columns in `mj-store/src/database/schema.rs:3655-3676`), so this combined breaking migration takes revision 157.
  Evidence: `mj-store/src/database.rs:34` and `mj-store/src/database/schema.rs:10,3658-3676`.

- Observation: `baseline.sql` is the revision-33 schema; fresh stores replay every migration after it, so a new revision never edits it and fresh/upgraded parity holds by construction (`recent_revisions_upgrade_directly_and_preserve_user_data` and `removed_plan_review_tables_raise_the_floor_and_preserve_turn_reviews` pass unchanged).
- Observation: `SessionRecord::checkout()` cannot know that a sub-agent borrows its owner's checkout. Dropping the copied path made three record-only readers wrong for children (`configuration_issue` flagged "missing bundle" and `session_launch_config` refused to launch; project-memory identity failed with "raw session project directory is missing"; `publication_state` reported an owned clone). They now take the checkout `State` resolved (`configuration_issue_in`, `validate_configuration_in`, `publication_state_in`, `ProjectMemoryIdentity::for_legacy_session(session, checkout, ..)`, `State::configuration_issue`). The `a_subagent_of_an_isolated_session_shares_its_parents_project_memory` test caught the project-memory case; the phone-snapshot golden loses `publication_state: "unknown"` for a borrowed child.
- Observation: `project_discovery_changes.directory` is now NULL for a managed checkout, so `ProjectDiscoveryChange.directory` is `Option<PathBuf>` and discovery resolves the path through `State::checkout`.
- Observation: a runtime recorded beside no template id (a prepared Move destination, a worker lease) cannot be matched to a target, so the scrub applies a key only when every target that names it agrees on one reference and none gives it a plain value; worker leases use their session's target and Move destinations the Move's selected target.

## Decision Log

- Decision: Batch `#1697` and `#1258` into a single breaking SQLite migration revision 157 (`minimum_compatible_version = 157`).
  Rationale: Owner explicitly approved breaking schema revision for `#1697` and directed batching `#1258` into it so we pay only one compatibility bump.
  Date/Author: 2026-11-25 / Owner & Coordinator.
- Decision: Emit container `--env` arguments for literal target environment entries only across every container engine, deleting Nitro's `secrets_out_of_band` environment-reference workaround while preserving `GH_TOKEN`'s existing path, and accept/document that secret-sourced target variables do not reach Mjolnir's pre-worker provisioning `podman exec` commands.
  Rationale: The worker already receives the full resolved `target_environment` via `0600` `launch.json` and applies it after login discovery; keeping container `--env` literal-only eliminates `podman inspect` / `Config.Env` / `CreateCommand` / `ps` exposure uniformly across Podman, Docker, and Nitro and removes Nitro-specific code. Owner approved this tradeoff.
  Date/Author: 2026-11-25 / Owner & Coordinator.
- Decision: Keep `EnvironmentPlacement` consistent with `SessionRecord` by not duplicating managed-worktree paths into `EnvironmentPlacement.project_directory` when a `managed_worktree` descriptor is present, updating `mj-controller/src/controller/environment.rs` and `mj-build-cache/src/mbx/release.rs` to inspect `managed_worktree` explicitly. Preserve legacy rows on read and never scrub existing `sessions.project_directory` values in SQL so orphaned legacy sub-agents without a parent row keep their stored path.
  Rationale: Avoids leaving a second duplicate path copy in `environments.placement_json` while maintaining safe orphan-record reads.
  Date/Author: 2026-11-25 / Coordinator.

- Decision: the legacy-secret scrub runs once at daemon startup (`process.rs`, before `Controller::load`) over the store, not in `prepare_persisted_sessions`, so the committed state the API serves is already clean. Entries whose target template was removed keep their resolved value until the session ends.
  Rationale: the database cannot tell which stored values were secrets; only the configuration can. Doing it before the first state load avoids a window in which a rewritten row and the in-memory copy disagree.
  Date/Author: 2026-11-25 / Implementer.
- Decision: a reviewer's recorded launch no longer carries `environment`, and its in-memory field stays only for legacy rows (`skip_serializing`).
  Rationale: the launch is built from the live profile, which already applies the profile environment, `exclude_harness_environment` and `IS_SANDBOX`; the recorded copy only froze resolved secrets.
  Date/Author: 2026-11-25 / Implementer.

## Outcomes & Retrospective

Both issues landed in one commit on the detached HEAD with breaking revision 157. A resolved target secret no longer appears in `sessions.target_runtime_json`, its embedded copies, the HTTP session record, reviewer `record_json`, or container `--env`; managed worktrees and borrowed sub-agents store no `project_directory`.

Validation (all on the dev profile): `cargo fmt --all -- --check`; strict Clippy over the ten requested crates plus tui, setup, sessionwiki and client; `brokk-mj-core --lib` (717), `brokk-mj-store --lib` (333, including `revision_157_*` and `target_secrets`), `brokk-mj-targets --lib` (193), `brokk-mj-controller --lib` (519 tests, 2 ignored), `brokk-mj-import --lib` (168), `brokk-mj-worker-launch --lib` (5), `brokk-mj-worker-staging --lib` (145), `brokk-mj-build-cache --lib` (96), `brokk-mj-daemon --lib` (661), `brokk-mj-server --lib` (317), `brokk-mj-tui --lib` (617), `brokk-mjolnir --bins` (304). Up to four controller tests (`preparing_cross_harness_move_without_utility_profiles_succeeds`, `failed_node_preflight_retains_error_before_provisioning`, `failed_resume_provisioning_preserves_checkpoint_and_projection_lineage`, `failed_resume_retires_a_checkout_it_recreated`) fail in a whole-suite run with `MJ_WORKER_BINARY ... worker executable changed while being read` and pass alone or together. The same failure reproduces at the base commit (three of them, in a clean checkout of `213c34ff1`), so it is not caused by this change.

Limitations: an entry whose target template was removed before the upgrade keeps its resolved value in the recorded runtime; a container created before this change keeps its secret in `Config.Env` until an explicit container upgrade (which now sheds it); secret-sourced variables do not reach pre-worker provisioning `exec` (documented).

## Context and Orientation

Target and profile environment configuration lives in `mj-core/src/config/secrets.rs` (`Environment`, `EnvironmentValue`, `SecretResolver`). `Environment::sources()` holds the configured map of literal strings and `{ from_env = ... }` / `{ from_secret = ... }` references; `Environment::resolved()` holds the resolved string values in daemon memory. `TargetRuntimeSettings` (`mj-core/src/state/target_runtime.rs`) is embedded in `SessionRecord` (`sessions.target_runtime_json`), move operations (`session_moves.operation_json`), retained move sources (`retained_move_sources.source_json`), environment placements (`environments.placement_json`), and environment workers (`environment_workers.runtime_json`), and is serialized into daemon HTTP responses.

Checkout ownership lives in `mj-core/src/state.rs`: `SessionRecord` carries `project_directory: Option<PathBuf>` and `managed_worktree: Option<ManagedWorktree>`, and `State::checkout` resolves where a session's code lives through the physical environment owner ledger. SQLite schema revisions live in `mj-store/src/database.rs` (`SCHEMA_VERSION`) and `mj-store/src/database/schema.rs` (`migrate`, `MINIMUM_COMPATIBLE_VERSION`, and migration tests).

## Plan of Work

### Part 1: `#1697` — Secret environment references and delivery

In `mj-core/src/state/target_runtime.rs`, keep `environment: BTreeMap<String, String>` for literal values only and add:

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment_references: BTreeMap<String, EnvironmentValue>,

Ensure `From<&TargetTemplate> for TargetRuntimeSettings` splits `container.environment.sources()` into literals (`environment`) and references (`environment_references`) rather than copying `container.environment.resolved()`. Add a single authoritative resolution method on `TargetRuntimeSettings`:

    pub fn launch_environment(&self) -> Result<BTreeMap<String, String>>

which starts from `self.environment`, resolves `self.environment_references` via `Environment::from_sources(self.environment_references.clone()).ensure_resolved()` (using the active `SecretResolver`), and returns the merged map (returning a visible error naming the missing reference if resolution fails). Audit every reader of `TargetRuntimeSettings.environment` across `mj-worker-staging/src/worker_binary/launch.rs`, `mj-worker-launch/src/worker_launch.rs`, `mj-targets/src/aws_credentials.rs`, and `mj-controller`: route launch-time consumers (AWS credential gating and `launch.json` `target_environment`) through `launch_environment()` called once per launch, so secret rotation takes effect on Restart and provisioning equality (`**recorded == runtime`) compares references rather than resolved values.

In `mj-controller/src/controller/backend.rs` (`container_environment`, `backend_container_with`, `backend_target`) and `mj-controller/src/controller/provisioning.rs`, emit `--env KEY=VALUE` only for literal entries (`environment`), never for secret-sourced references (`environment_references` or `is_secret_sourced`), across every container engine (Podman, Docker, and Nitro). Delete the now-unnecessary Nitro-only environment-reference out-of-band plumbing (`secrets_out_of_band`, the `secrets` half of `ContainerEnvironment`, and `provisioning.rs:945-957`), keeping `provide_target_environment_secrets` strictly for `GH_TOKEN`. In `mj-targets/src/targets/container_upgrade.rs:921-960`, skip any environment keys recorded in `environment_references` when copying `Config.Env` during explicit container upgrades so legacy containers shed stale secret values naturally without forcing container recreation.

In `mj-controller/src/controller/reviewer.rs` (`reviewer_launch_config`), `mj-core/src/worker_launch.rs` (`ReviewerLaunchConfig`), and `mj-worker-launch/src/worker_launch.rs:875-881`, stop populating `ReviewerLaunchConfig.environment` and delete the redundant `launch.environment.extend(reviewer.environment)` line (the launch already gets the reviewer profile's environment, exclusions, and `IS_SANDBOX` from `worker_launch_config_with_checkout`). Keep `ReviewerLaunchConfig.environment` with `#[serde(default, skip_serializing)]` so `deny_unknown_fields` continues accepting legacy rows if encountered before scrubbing.

At controller load (alongside `backfill_target_runtime` in `mj-controller/src/controller.rs:807-830`), add an idempotent repair pass over the persisted `TargetRuntimeSettings` JSON paths enumerated in `mj-store/src/database/container_size.rs:235-290`: for any session/environment/move/retained-source record whose current target template defines a key as a reference (`from_env` or `from_secret`) while the stored `TargetRuntimeSettings.environment` still holds a plain string value for that key, move the key from `environment` into `environment_references` with the template's `EnvironmentValue` reference and persist the scrubbed record. Update `docs/src/content/docs/targets.md` to document that secret-sourced target environment entries are delivered to the worker, harness, and shells through `0600` `launch.json` rather than container `--env` / pre-worker provisioning `exec`.

Also verify via grep across `mj-controller`, `mj-targets`, `mj-worker-staging`, and `mj-daemon` that no other logging or debug-formatting site prints resolved secret environment values from `CommandSpec` or `TargetRuntimeSettings`.

### Part 2: `#1258` — Stop storing derivable checkout paths

In `mj-core/src/state.rs`, add a canonical derivation method on `ManagedWorktree` (for example `pub fn checkout_directory(&self) -> Result<PathBuf>` or `Option<PathBuf>` computing `worktree_root.join(source_project_directory.strip_prefix(source_repository))` — reuse the exact relative-path logic currently inside `ManagedWorktree::validate` at `mj-core/src/state.rs:1173-1213`). Update `ManagedWorktree::validate` so `project_directory: None` is valid (and when legacy `Some(path)` is present, it still validates equality with the derived path so inconsistent legacy records are rejected). Update `derive_record_checkout` and `Checkout` (`mj-core/src/state.rs:2828-2884`) so a `ManagedWorktree` checkout derives its path from the descriptor (returning `Cow<'a, Path>` or owned `PathBuf` as cleanest across callers; prefer minimal churn and zero unnecessary duplication) when `record.project_directory` is `None`, while preserving `Attached` (`project_directory: Some`, `managed_worktree: None`), `ManagedWorkspace` (`project_directory: None`, `managed_worktree: None`), and `Borrowed` (resolving via `workspace_owner` with legacy orphan fallback only when reading an orphaned record directly).

Stop writing redundant `project_directory` values in runtime writers:
- Borrowed sub-agents (`mj-controller/src/controller/subagents.rs:483`): leave `project_directory: None` instead of copying `parent.project_directory` (keep `placement.directory` only on the transitional owned-placement path at `:537-539` before its managed worktree is created, then clear it when the managed descriptor is installed).
- Settled managed worktrees (`mj-controller/src/controller/worktree.rs:456`, `worktree.rs:977`, `mj-controller/src/controller/resume.rs:2539`): install `record.managed_worktree = Some(descriptor)` and set `record.project_directory = None`. Keep transitional pre-creation source directory (`controller.rs:1183,1202` while `create_managed_worktree` is true) and failed-create rollback (`provisioning.rs:509`, restoring `source_project_directory` when discarding the planned managed descriptor) working as expected, and ensure failed-resume rollback (`resume.rs:3222-3225`) faithfully restores whichever prior `(project_directory, managed_worktree)` pair was captured.
- `EnvironmentPlacement::from_session` (`mj-store/src/database/environments.rs:34,58,534-571`): do not copy `project_directory` when `managed_worktree` is present (and keep reading legacy placements that have both).

Update every null-sensitive consumer so `project_directory.is_none()` is never mistaken for a managed bundle workspace when `managed_worktree.is_some()`:
- `mj-store/src/database/projects.rs:169,207`: require `project_directory IS NULL AND managed_worktree_json IS NULL` when deciding which sessions receive a project snapshot.
- `mj-daemon/src/server_runtime/api.rs:4103` (`agent_repo_urls`): check checkout kind (`record.project_directory.is_none() && record.managed_worktree.is_none()`, or `record.checkout().is_managed_workspace()`).
- `mj-build-cache/src/mbx/release.rs:521`: check `placement.project_directory.is_none() && placement.managed_worktree.is_none()`.
- `mj-controller/src/controller/environment.rs:635`: verify managed_worktree vs attached handling when `placement.project_directory` is `None`.
- `mj-daemon/src/daemon/state.rs:1601-1615` and `mj-daemon/src/review_host/controller_environment.rs:82`: resolve checkout through `State::checkout` (with safe fallback when the session/parent is missing) instead of assuming a borrowed child carries a stored copy.
- Audit the remaining null-sensitive checkout users noted in reconnaissance (`mj-core/src/state.rs:2305,3305,3361`, `mj-import/src/project_catalog.rs:59,360-375`, `mj-daemon/src/server_runtime/project_sources.rs:20,35,101`, `mj-worker-launch/src/worker_launch.rs:830,1112`, `mj-worker-staging/src/worker_binary/launch.rs:193`, `mj-setup/src/doctor.rs:4038,4053`) to ensure every one uses `Checkout` or explicitly accounts for `managed_worktree`.

### Part 3: Breaking schema migration revision 157

In `mj-store/src/database.rs:34`, advance `SCHEMA_VERSION` from 156 to 157. In `mj-store/src/database/schema.rs`, advance `MINIMUM_COMPATIBLE_VERSION` (test constant at `:10`) to 157 and add the `if version < 157` immediate-transaction migration block right after revision 156 with a comment classifying it as **breaking** (older daemons rewriting `target_runtime_json` would drop `environment_references`, and older daemons reject managed-worktree records whose `project_directory` is `NULL`). Inside the single immediate transaction:
1. Scrub legacy `reviewer_launch.environment` from `subagent_sessions` (O(small table), no multi-GB blob tables touched):

       UPDATE subagent_sessions
          SET record_json = json_remove(record_json, '$.reviewer_launch.environment')
        WHERE json_type(record_json, '$.reviewer_launch.environment') IS NOT NULL;

2. Drop and recreate the project-catalog discovery triggers originally defined in `project_catalog_v67.sql` / revision 69 (`schema.rs:1802-1818`) so inserts and updates enqueue into `project_discovery_changes` whenever `NEW.project_directory IS NOT NULL OR NEW.managed_worktree_json IS NOT NULL` (and keep `baseline.sql` in exact sync with fresh-DB schema so `fresh_and_upgraded_schemas_match` passes!). Do NOT clear existing `sessions.project_directory` rows in SQL so legacy orphaned sub-agents keep their stored path.
3. Raise `schema_compatibility.minimum_compatible_version = 157`, insert `(157, ...)` into `schema_migrations`, and set `PRAGMA user_version = 157`.

## Concrete Steps

Work exclusively inside the detached worktree `/home/jonathan/Projects/mjolnir/.mj/clones/62f67a3f25ad58150b0b60eb2a982e94/.mj/worktrees/secrets-1697-1258`.

1. Inspect the exact current files listed in Plan of Work and verify `mj-store/src/database/baseline.sql` vs `schema.rs` triggers.
2. Implement Parts 1, 2, and 3 cleanly, keeping all tests isolated with their own temp `MJ_CONFIG_DIR` and `MJ_DATA_DIR` (never touching the live store).
3. Add targeted integration and tricky-condition tests (per `AGENTS.md` "Which tests to add"):
   - Breaking migration revision 157 test in `mj-store/src/database/schema.rs` (following `revision_155_persists_staged_aws_profile_and_refuses_revision_154` at `:6067-6120`): upgrades a rev-156 store, verifies rev-156 reader refusal, verifies `reviewer_launch.environment` is scrubbed, and verifies catalog triggers fire for pathless `managed_worktree_json IS NOT NULL` inserts/updates while fresh and upgraded schemas match.
   - Secret non-persistence and rotation tests: creating a session with `{ from_secret = "..." }` persists no resolved value in `sessions.target_runtime_json`, `environments.placement_json`, HTTP `SessionRecord`, or reviewer `record_json`; `container_environment` emits only literals (no secret references); rotating the secret updates `launch.json` on Restart without tripping provisioning equality; removing the secret fails visibly naming the reference; controller-load scrub idempotently migrates legacy resolved rows; container upgrade skips recorded reference keys.
   - Derived checkout path tests: settled managed worktree and borrowed sub-agent persist `project_directory = None`; `State::checkout` derives the exact managed worktree path and borrowed child path; legacy records with copied paths and orphaned legacy sub-agents still load; project snapshot queries (`projects.rs`), `agent_repo_urls`, `mbx/release.rs`, and catalog discovery classify pathless managed worktrees correctly rather than treating them as bundle workspaces.
4. Run validation in the foreground (waiting for completion before handing back):

       cargo fmt --all -- --check
       cargo clippy -p brokk-mj-core -p brokk-mj-targets -p brokk-mj-store -p brokk-mj-build-cache -p brokk-mj-import -p brokk-mj-worker-staging -p brokk-mj-worker-launch -p brokk-mj-controller -p brokk-mj-server -p brokk-mj-daemon --all-targets -- -D warnings
       cargo test -p brokk-mj-core --lib state::
       cargo test -p brokk-mj-store --lib database::schema::
       cargo test -p brokk-mj-store --lib database::projects
       cargo test -p brokk-mj-targets --lib container_upgrade
       cargo test -p brokk-mj-controller --lib backend::
       cargo test -p brokk-mj-controller --lib reviewer::
       cargo test -p brokk-mj-controller --lib worktree::
       cargo test -p brokk-mj-controller --lib subagents::
       cargo test -p brokk-mj-import --lib project_catalog
       cargo test -p brokk-mj-worker-launch --lib

5. Update this ExecPlan's living sections and commit all changes cleanly on the detached HEAD (staging only changed files). Do not push and do not create a branch.

## Validation and Acceptance

- Raw SQL queries against an isolated rev-157 SQLite database confirm zero resolved secret values in `sessions.target_runtime_json`, `session_moves.operation_json`, `retained_move_sources.source_json`, `environments.placement_json`, `environment_workers.runtime_json`, and `subagent_sessions.record_json`.
- `container_environment` emits `--env` only for literal target entries, and `launch.json` receives the resolved secret values on launch and picks up rotated values on Restart.
- New managed-worktree sessions and borrowed sub-agents store `project_directory = NULL` in SQLite while resolving their checkout paths identically via `State::checkout` and triggering project-catalog discovery.
- Revision-156 daemons are refused by `minimum_compatible_version = 157`, and all schema-ladder migration tests (`recent_revisions_upgrade_directly_and_preserve_user_data`, fresh-vs-upgraded schema parity) pass.

## Idempotence and Recovery

Migration 157 runs inside a single immediate SQLite transaction (`json_remove` and `DROP TRIGGER IF EXISTS` / `CREATE TRIGGER`), and controller-load legacy secret scrubbing is idempotent across restarts. All testing runs in isolated temporary directories and never touches the live host store.

## Artifacts and Notes

Detailed reconnaissance reports are at `.mj/agents/e5ed2235e36a92a00c378a4b24121366/1697_analysis.md` and `.mj/agents/43b868f90802b28fbb4db80aeab00f58/1258_map.md`.

## Interfaces and Dependencies

In `mj-core/src/state/target_runtime.rs`:

    pub struct TargetRuntimeSettings {
        ...
        pub environment: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        pub environment_references: BTreeMap<String, EnvironmentValue>,
    }

    impl TargetRuntimeSettings {
        pub fn launch_environment(&self) -> Result<BTreeMap<String, String>>;
    }

In `mj-core/src/state.rs`, `ManagedWorktree` provides a single canonical checkout-directory derivation method used by both `ManagedWorktree::validate` and `derive_record_checkout` / `Checkout::project_directory()`.
