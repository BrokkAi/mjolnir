# Cross-Target Orchestrators, Tool Split, Target Telemetry, and Nested TUI Navigation

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. This document lives at `.agents/plans/cross-target-orchestrators.md` and must be maintained in accordance with `.agents/PLANS.md` from the repository root.


## Purpose / Big Picture


Today a coordinating Mjolnir session can only spawn managed children inside its own target, container, and working tree. When a coordinator needs a helper session on another target — such as an EC2 host with NVMe caches, a larger container host, or a different set of repositories — it falls back to `mj new` and shell scripts, losing parent links, finish-report delivery, `list_agents` visibility, and managed `close` cleanup (GitHub issue #1552). At the same time, Mjolnir already has a depth-1 `supervisor` subagent role drawn from `[[subagents.primary]]`, cross-session `send_message` with startup queueing (#1553), cross-target `fetch`, and a durable environment ownership ledger (`environments` + `environment_bindings` from migration 122), but `supervisor` is hard-wired to borrow the parent's environment and checkout, lacks `mj-review` and `ask_oracle`, cannot run on another target, stores its toggle in a parallel `sessions.supervisors` + `supervisor_preference` + `MoveSelection.supervisors` pipeline outside `SubagentPolicy`, and is gated behind a per-target `supervisors_available_for_target` check.

After this change, the `supervisor` delegation role is renamed to **orchestrator** across user- and agent-facing surfaces, and the per-session orchestrator opt-in moves to a **single owner**: `SubagentPolicy::Mjolnir { ..., orchestrators: bool }`, eliminating the parallel preference table, Move field, feed maps, and non-Mjolnir validation boilerplate. The `mj-agents` tool surface splits `spawn` into `spawn_worker` (same shared-workspace behavior as today, with a two-variant `task_type` enum `{straightforward, subtle}` so `"orchestrator"` is unrepresentable, reusing the existing `Spawn` wire action) and `spawn_orchestrator` (no `task_type`, `working_directory`, `profile`, `model`, `effort`, `cpus`, or `memory`; optional `target` and `repos` list of repository URLs; always provisioned in its own isolated container or managed clone using the destination target's default container size and `[[subagents.primary]]` load balancing). A new read-only `list_targets` tool lets a coordinator inspect enabled targets, brief host specs (EC2 instance type read from host DMI, total CPUs and RAM, default container size, committed/remaining capacity), and recent plus rolling 1-hour host usage (60 fixed per-minute buckets) before choosing a target. `list_agents` adds `role` (`straightforward`, `subtle`, or `orchestrator`), `target`, `repos`, live `model`/`effort`, and active depth-2 child counts. Capabilities follow one physical-ownership rule backed by the existing environment ledger (`environments.owner_session_id == session_id`): only the top-level session owning a container or managed clone (either a user-started session or a standalone orchestrator) receives `mj-review` (with its pre-push gate) and `ask_oracle`, while borrowed workers inside that container do not (`mj-memory` gating and SessionWiki indexing isolation are left untouched). Default `max_concurrent` is `8`, `max_concurrent / 2` and `>= 2` are deleted, and each direct live orchestrator counts as `1` toward its parent's `max_concurrent` while its depth-2 workers count only in the orchestrator's own container. Orchestrators are never auto-suspended on finish (their idle worker parks inside the intact container so untracked files survive); instead, manual `close` routes through ordinary checkpointed suspend (saving tracked git state via origin-delta checkpoint, tearing down the container and report directory, keeping the transcript, and refusing further `send_message`). Parent `move` leaves orchestrators completely untouched; parent `suspend` warns and runs a concurrent `pmap` suspend across its orchestrators (marking `resume_with_parent = true` on those suspended), and parent `resume` runs a concurrent `pmap` resume over them. In the TUI, orchestrators stay hidden from the flat top-level workspace list outside parent drill-down, entering an orchestrator lets the user enter its subagents with a single-line `Root › Orchestrator [X]` breadcrumb in the Workspaces pane (matching Web), and the profile usage report splits subagent spend by type (`Orchestrators`, `Subtle subagents`, `Straightforward subagents`, `Oracles`). Notifications default to enabled for `mj new` sessions and default to muted (`notifications_ignored = true`) for new subagents (no backfill migration), with `notifications_ignored` as the single owner so users can opt in via the existing TUI/Web toggle. Finally, `mj ec2 create` requires explicit default container `--cpus` and `--memory-gib` flags so newly provisioned EC2 hosts never fall back to `4 CPU / 8 GiB`.

A user or contributor can verify this end to end in an isolated `--instance` daemon by configuring container targets (or running `mj ec2 create --cpus ... --memory-gib ...`), enabling Orchestrators on a top-level session, calling `list_targets` to see recent and rolling 1-hour usage plus host info, calling `spawn_orchestrator` onto another target with explicit `repos`, observing the orchestrator spawn depth-2 workers via `spawn_worker` and run `request_review` and `ask_oracle` inside its own container, moving the top-level session without disturbing the orchestrator, suspending and resuming the top-level session and observing the orchestrator suspend and resume with it, drilling two levels deep in the TUI with the `Root › Orchestrator [X]` breadcrumb in the Workspaces pane, opening the profile usage dialog to see split `Orchestrators` / `Subtle` / `Straightforward` rows, and calling `close` on the orchestrator to verify its tracked commits are checkpointed and its container is reclaimed while its transcript remains readable.


## Progress


- [x] (2026-11-22 21:44Z) Completed initial read-only reconnaissance across rename/config/schema, provisioning/EC2/`mj hosts` capacity polling, lifecycle/capability gates/cascades, and TUI/Web nesting.
- [x] (2026-11-22 21:44Z) Drafted initial ExecPlan and sent it to MJ Advisor (`91233b0efdd193066e7b4eeb46c8db10`) for review.
- [x] (2026-11-22 21:58Z) Mapped and folded in TUI profile usage split, removal of `mj new` muted-by-default hack, and single-owner `notifications_ignored` for subagents.
- [x] (2026-11-22 22:09Z) Incorporated MJ Advisor's review (`91233b0efdd193066e7b4eeb46c8db10`, five static audits at `d7de33a91`).
- [x] (2026-11-23 01:38Z) Completed three simplification audits (`36f11109`, `3aea9888`, `4f76e726`), merged `origin/master` (`e50f987ee`), claimed issue #1552, and locked user decisions: single-owner `SubagentPolicy::Mjolnir { orchestrators }`, `default_subagent_limit() = 8`, single-line `Root › Orchestrator [X]` breadcrumb, no notification backfill migration, leave SessionWiki indexing untouched, and keep parent Suspend/Resume as a lean concurrent `pmap` (collapsing the whole plan into 3 milestones and 1 breaking migration 152).
- [x] (2026-11-23 02:37Z) Milestone 1 (zero migrations): TUI/Web `Root › Orchestrator [X]` breadcrumb, TUI profile usage split, `mj new` / subagent single-owner `notifications_ignored` (no backfill), `mj ec2 create --cpus/--memory-gib` + single container-size resolver, EC2 DMI probe + 60-bucket `CapacityFeed` + `list_targets` from shared target projection, and enriched `list_agents`.
- [x] (2026-11-23 03:50Z) Milestone 2 (single breaking migration 152): Owned children & structural ownership simplification (ledger ownership in atomic `register_subagent_with_startup`, incremental `environment_owners: SnapshotMap<String, String>` projection, collapsed `state.rs` primitives, deleted second parent launch build, unified `provisioning.rs`/`controller/lifecycle.rs` physical teardown, Parked-owner already-quiescent origin-delta capture + cancel-to-Parked rollback, `origin_delta.rs` environment-scoped quiesce, auto-archive safety without touching SessionWiki, container-upgrade `environment_id` peer gating, Workspace Close roots, Move exclusion, `Plan::ChildSwitch` verification, and lean `pmap` parent Suspend/Resume sharing `close_workspace.rs`'s `JoinSet` helper).
- [x] (2026-11-23 05:08Z) Milestone 3 (zero new migrations): Single-owner `SubagentPolicy::Mjolnir { orchestrators }` (deleting parallel `supervisors` column/table/Move/RPC/feed/availability plumbing and Milestone 2's temporary preference sync bridge), `spawn_worker` (reusing `Spawn` wire) / `spawn_orchestrator` split, consolidated MCP capability builder, `CreateProject` `repos`, `mj-review` + `review_gate` + `ask_oracle` for owners, slot rule (`1` per orchestrator, `default_subagent_limit() = 8`, drop `/ 2` and `>= 2`), and user/agent/code `supervisor -> orchestrator` rename.


## Surprises & Discoveries


- Observation: Physical workspace ownership already has an authoritative durable owner: the environment ledger (`environments.owner_session_id` + `environment_bindings.session_id -> environment_id`, migration 122, `mj-store/src/database/environments.sql:1-60`). However, `capture_session_environment` (`mj-store/src/database/environments.rs:180-245`) unconditionally rebinds any target-bearing linked child to its parent's active environment on every placement write. Even without adding SQL columns, a revision-151 writer writing a placement update on an owned child would rebind that child back to its parent's environment (`environments.rs:194-224`, `state_io.rs:923-938`). Therefore, Milestone 2 (the first milestone that can persist owned-child rows) raises `MINIMUM_COMPATIBLE_VERSION` to `152` independently of retained-worker wire compatibility.
  Evidence: Direct read of `mj-store/src/database/environments.sql:1-60`, `mj-store/src/database/environments.rs:180-245`, Advisor audit `17ed9edf`, and simplification audit `36f11109`.

- Observation: The per-session `supervisors` toggle sprawled across 93 files only because it was bolted on beside `SubagentPolicy` as a second boolean (`sessions.supervisors`), a second preference table (`supervisor_preference`), a second Move selection field (`MoveSelection.supervisors`), a second RPC/HTTP field, and two extra daemon feed maps — even though `subagent_preference` already stores full JSON `SubagentPolicy`, `sessions` already stores `SubagentPolicy`, `MoveSelection` and `StartSessionRequest` already carry `SubagentPolicy`, and `SubagentPolicy::Wire` (`mj-core/src/subagent.rs:26-44`) does not have `deny_unknown_fields`. Moving `orchestrators: bool` inside `SubagentPolicy::Mjolnir` makes illegal states (`Native`/`None` + `orchestrators`) unrepresentable and deletes every parallel preference, Move, feed, and validation path in one sweep while staying backward-compatible with retained workers.
  Evidence: Direct read of `mj-core/src/subagent.rs:10-84`, `mj-store/src/database/schema.rs:895, 1870`, `mj-store/src/database/sessions.rs:74, 1392`, and simplification audit `36f11109`.

- Observation: `mj-daemon/src/daemon/close_workspace.rs:100-160` is literally a ~40-line concurrent `pmap` (`tokio::task::JoinSet` calling `suspend_session` for each ID, dropping `upgrade_work` before waiting, and collecting failures) with no group preflight. Reusing that `pmap` helper for parent Suspend (using blanket untracked consent authorized by the parent dialog's warning about orchestrators) and a matching `pmap` of `resume_session` on parent Resume eliminates multi-owner preflight RPCs, per-owner consent dialogs, persisted `SuspendCause` in `Closing` intent, and a daemon startup resume sweep — turning the cascade into ~80 lines piggybacking on Milestone 2's single breaking migration 152 (`SubagentRecord.resume_with_parent`).
  Evidence: Direct read of `mj-daemon/src/daemon/close_workspace.rs:95-165`.

- Observation: `session_launch_config` (`mj-worker-launch/src/worker_launch.rs:665-698, 858-899`) builds the child's entire `WorkerLaunchConfig` and then builds a second complete `WorkerLaunchConfig` for the parent just to overwrite `cwd` and `additional_directories`, requiring a live parent target and profile. Computing path facts once from the physical owner's checkout/bundle/mounts deletes that duplicate build and makes owned orchestrators launch cleanly even while their parent is moving or stopped.
  Evidence: Direct read of `mj-worker-launch/src/worker_launch.rs:858-899` and simplification audit `36f11109`.

- Observation: `mj-store/src/database/committed.rs:46-65` does not currently trigger on `environment_bindings`, and `mj-tui/src/dashboard_sessions.rs:666` computes `project_identity_session` from the incremental client feed. `environment_bindings` must be registered in `committed.rs` and projected incrementally alongside sessions/relations so `environment_owners` never goes stale.
  Evidence: Simplification audit `36f11109` (`committed.rs:46-65, 410-450`, `runtime_feed.rs:105, 1023-1055`).

- Observation: `close_route` (`mj-daemon/src/daemon.rs:544-563`), six branches in `mj-controller/src/controller/lifecycle.rs:707, 2304, 2467, 2606, 2713, 2883`, and `mj-controller/src/controller/lifecycle/origin_delta.rs:387, 516-517` dispatch on relational child membership rather than physical ownership. Moreover, `Parked` sessions route to `SettleWithoutCheckpoint` (`daemon.rs:544-561`, `lifecycle.rs:3067-3081`), and `Closing` cancel rollback currently restores `Running` (`lifecycle.rs:3170-3175`) instead of `Parked`.
  Evidence: Direct read of `mj-daemon/src/daemon.rs:535-575`, Advisor audit `ad132332`, and simplification audit `3aea9888`.

- Observation: `mj-store/src/web_push.rs:270-276` checks `holder.notifications_ignored` rather than the card's owning session (`session`), while `attention.rs:504-527` projects a human-owned child question onto every active ancestor. Checking the card's publication owner in `web_push.rs` and choosing one publication owner per promoted form in `attention.rs` ensures muted-by-default subagents never suppress promoted parent push alerts and opted-in subagents never emit duplicate cards.
  Evidence: Direct read of `mj-store/src/web_push.rs:245-295` and simplification audit `3aea9888`.

- Observation: Recent `origin/master` (`e50f987ee`) split several worker crates: `mj-worker-mcp/src/subagent_mcp.rs`, `mj-worker-review/src/{capture,hook}.rs`, `mj-microvm/src/cli.rs`, `mj-github-watch/src/service.rs`, and `mj-session-types/src/viewer_move_recovery.rs`.
  Evidence: Fast-forward merge `d7de33a91..e50f987ee`.


## Decision Log


- Decision: Make `SubagentPolicy::Mjolnir { straightforward, subtle, orchestrators: bool }` the single owner of the per-session orchestrator toggle, eliminating `SessionRecord.supervisors`, `supervisor_preference`, `State.last_supervisor_choices`, `MoveSelection.supervisors`, `MoveRecoveryView.supervisors`, `StartSessionRequest.supervisors`, toggle-only Move restart, and runtime/viewer preference maps. Fold existing `sessions.supervisors = 1` and `supervisor_preference.enabled = 1` rows into `SubagentPolicy::Mjolnir { orchestrators: true }` in migration 152, and keep `WorkerLaunchConfig.supervisors_enabled` (derived from `launch.subagents.orchestrators_enabled()`) on the worker wire for retained older workers.
  Rationale: User directive (2026-11-22: rewrite with a single owner rather than removing). `subagent_preference` and `sessions` already store JSON `SubagentPolicy`, `MoveSelection` and `StartSessionRequest` already carry `SubagentPolicy`, and `SubagentPolicy::Wire` has no `deny_unknown_fields`.
  Date/Author: 2026-11-23 / User & Coordinator

- Decision: Set `default_subagent_limit()` (`mj-core/src/config.rs:726`) to `8`, delete `max_concurrent / 2` and `>= 2`, and count each direct live owning orchestrator as `1` toward its parent's `max_concurrent` while depth-2 workers count strictly against the orchestrator's own container `max_concurrent`.
  Rationale: User confirmed `8` (2026-11-22) and directive 2b.
  Date/Author: 2026-11-23 / User & Coordinator

- Decision: Render TUI two-level nesting in the Workspaces pane as a single-line `Root › Orchestrator [X]` breadcrumb (fixed 3-row pane height, matching Web), derived from `subagent_workspace_chain`.
  Rationale: User confirmed `Breadcrumb` (2026-11-22).
  Date/Author: 2026-11-23 / User & Coordinator

- Decision: Implement parent Suspend/Resume cascade as a lean concurrent `pmap` reusing `close_workspace.rs`'s `JoinSet` executor (releasing outer `upgrade_work` while waiting): parent Suspend warns that orchestrators will also be suspended (untracked files dropped), `pmap`s ordinary `suspend_session` with blanket untracked consent over direct non-terminal owning orchestrators, sets `SubagentRecord.resume_with_parent = true` on each child that transitions to `Stopped` via parent suspend (cleared on resume or explicit `close`), and completes parent suspend regardless of individual child failures; parent Resume `pmap`s ordinary `resume_session` over `resume_with_parent == true && state == Stopped` children (and re-runs the same `pmap` if Resume is called again on a `Running` parent). No multi-owner preflight RPC, no per-owner consent dialogs, no persisted `SuspendCause`, and no startup resume sweep.
  Rationale: User pushback (2026-11-22: "it's literally a pmap"). Eliminates ~500 lines of defensive state machinery and collapses the breaking schema change into the single migration 152.
  Date/Author: 2026-11-23 / User & Coordinator

- Decision: Use the existing environment ledger (`environment_bindings -> environments.owner_session_id == session_id`) as the sole source of truth for physical workspace ownership, projected both on full load (`state_io.rs`) and incrementally (`committed.rs`, controller inserts, TUI feed) as fallible `State::owns_environment(session_id) -> Option<bool>`, with zero `owns_workspace` field in `SubagentRecord`, `SubagentMetadata`, or `WorkerLaunchConfig`. Fence owned-child persistence with a single breaking migration 152 in Milestone 2.
  Rationale: Advisor review A1 and audit `36f11109`.
  Date/Author: 2026-11-23 / Advisor & Coordinator

- Decision: Keep atomic child registration in `register_subagent_with_startup` with `own_environment: bool`, collapse `state.rs` ancestry walkers onto `environment_owner_id` + `active_borrowers`, delete the second parent `WorkerLaunchConfig` build in `worker_launch.rs`, collapse `provisioning.rs` and `controller/lifecycle.rs` physical teardown, keep `Plan::ChildSwitch` for orchestrator quota swaps, and leave SessionWiki `top_level.rs` indexing exclusion untouched (keeping only auto-archive retention protection in `resume.rs:395-495`).
  Rationale: Advisor reviews A2–A4/A6, audits `36f11109`/`3aea9888`, and user confirmation on SessionWiki (2026-11-22).
  Date/Author: 2026-11-23 / User, Advisor & Coordinator

- Decision: Split MCP `spawn` into `spawn_worker` (two-variant `WorkerTaskType::{Straightforward, Subtle}`, reusing existing `Spawn` wire action) and `spawn_orchestrator` (new `SpawnOrchestrator` wire action), plus `list_targets` (`ListTargets`), consolidating MCP tool definitions, call acceptance, and harness allow-lists onto one capability builder and parsing `launch.json` once. Persist explicit `repos` via daemon `CreateProject` / `create_bundle_from_sources` (no `1..=32` cap), require positive `--cpus` and `--memory-gib` on `mj ec2 create` writing canonical `ContainerTemplate.{cpus, memory}`, unify container-size resolution into one helper, read EC2 `instance_type` from Linux DMI in `mj-targets/src/targets/resources.rs`, and retain 60 fixed per-minute buckets in `CapacityFeed`.
  Rationale: User directives 1a–1f, Advisor reviews A7/B2/B4, and audits `36f11109`/`4f76e726`.
  Date/Author: 2026-11-23 / User, Advisor & Coordinator

- Decision: Apply container-top-level capability gating (`owns_environment`) strictly to `mj-review` (`review_trigger = Push`, `review_owner`, `review_gate() = review_tools() && reviewer_launch.is_none()`) and `ask_oracle` (all four blockers), leaving `mj-memory` untouched and `no_github_events = true` for orchestrators.
  Rationale: User directive 1g and Advisor review A8.
  Date/Author: 2026-11-22 / User, Advisor & Coordinator

- Decision: Remove `mj new`'s `--with-notifications` inversion (default `notifications_ignored: false`, add `--ignore-notifications`), default new subagents to `notifications_ignored = true` with **no backfill migration**, delete hardcoded role-based notification filters across all four surfaces (`attention.rs`, `dashboard_sessions.rs`, `notify.rs`, `notifications_pane.rs`, `viewer.js`), and select one publication owner per promoted form in `attention.rs` + check card publication owner in `web_push.rs:270-276`.
  Rationale: User directives (2026-11-22: no backfill needed) and audit `3aea9888`.
  Date/Author: 2026-11-23 / User & Coordinator

- Decision: Rename user-, agent-, and code-facing `supervisor` / `supervisors` terms to `orchestrator` / `orchestrators` (no hidden CLI/API aliases per 2026-10-18 ruling), delete `supervisors_available_for_target` in favor of `Config::orchestrators_configured()`, and preserve SQLite (`category = 'supervisor'`, `record_json` key `"supervisor"`) and `WorkerLaunchConfig` wire keys (`"supervisor"`, `"supervisors_enabled"`) via `#[serde(rename = ...)]` for retained-worker compatibility.
  Rationale: User directives i and 2c, Advisor review A7.
  Date/Author: 2026-11-23 / User, Advisor & Coordinator


## Outcomes & Retrospective


All three milestones landed cleanly with a single breaking database migration (revision 152) and zero new `CONFIG_VERSION` bumps:

- **Milestone 1** (`e24e251b8`): Shipped TUI/Web `Root › Orchestrator [X]` single-line breadcrumb navigation, split TUI profile usage rows (`Orchestrators`, `Subtle subagents`, `Straightforward subagents`, `Oracles`, `Reviewers`, `Other subagents`, `Unclassified subagents`), enabled-by-default `mj new` notifications (`--ignore-notifications`), muted-by-default new subagents with single-owner `notifications_ignored` (no backfill), required positive `--cpus`/`--memory-gib` on `mj ec2 create` with unified `resolve_default_container_size`, EC2 DMI `instance_type` probe + 60-bucket rolling `CapacityFeed`, read-only `list_targets`, and enriched worker-scoped `list_agents`.
- **Milestone 2** (`3388edd0f`): Shipped breaking migration 152 (fencing revision 151, folding legacy `sessions.supervisors`/`supervisor_preference`, adding `SubagentRecord.resume_with_parent`), atomic `own_environment` registration, incremental `environment_owners: SnapshotMap<String, String>` projection across store/daemon/TUI/Web, unified container-upgrade peer gates, collapsed `State` ownership primitives, unified create/provisioning/launch/teardown (deleting the duplicate second parent launch build), Parked-owner checkpoint capture with cancel-to-Parked rollback, archive FK protection for non-peer children, resumed `report_dir` refresh, Workspace Close roots, Move exclusion, `Plan::ChildSwitch`, and lean `pmap` parent Suspend/Resume with durable recovery.
- **Milestone 3**: Shipped single-owner `SubagentPolicy::Mjolnir { orchestrators }` (net -234 lines deleting parallel `supervisors` plumbing while folding sealed Move intents on read), `default_subagent_limit() = 8` with serialized writer slot admission (`1` per direct live owning orchestrator), `supervisor -> orchestrator` rename preserving SQLite and retained-worker `WorkerLaunchConfig` wire spellings (`PROTOCOL_VERSION = 122`), `spawn_worker` / `spawn_orchestrator` split with unified MCP + harness capability builder, `CreateProject` `repos` resolution, `close` routing owning orchestrators through checkpointed Suspend returning dropped-untracked evidence, and container-top-level `mj-review` (`review_gate()`) + `ask_oracle` for owning orchestrators.

Lessons:
- Replacing parallel `supervisors` plumbing with `SubagentPolicy::Mjolnir { orchestrators }` and using the existing `environments` ledger (`SnapshotMap<String, String>`) eliminated entire classes of state drift and shrank the codebase.
- Pre-push review caught real edge cases in Milestone 2 (archive FK corruption for manually stopped owning children, cascade recovery on restart, durable pre-commit `resume_with_parent`, and resumed `report_dir` refresh).
- Live cross-target container provisioning and external oracle/reviewer runs remain to be exercised in an interactive session or isolated `--instance` smoke check after installing the new build.


## Context and Orientation


Mjolnir is a Rust workspace (`Cargo.toml`) split into a control-plane daemon (`mj-daemon` over `mj-controller`, `mj-server`, `mj-store`, `mj-accounts`, `mj-profile-catalog`, `mj-core`, `mj-client`, `mj-targets`, `mj-github-watch`, `mj-session-types`) and a data-plane worker (`mj-worker`, `mj-worker-bin`, `mj-worker-mcp`, `mj-worker-review`, `mj-worker-launch`, `mj-worker-staging`, `mj-microvm`). User surfaces are the CLI (`mj-cli`), terminal UI (`mj-tui`), and browser UI (`mj-server/src/web/viewer.js`, fed by `mj-server/src/server/viewer_types.rs` and `viewer_wire.rs`).

Key terms used in this plan:

- **Environment ledger (`mj-store/src/database/environments.sql`, `environments.rs`):** Migration 122 introduced durable physical environment ownership. `environments` rows carry `environment_id`, `owner_session_id`, `phase` (`active`, `retiring`, `reclaimed`), and `placement_json`. `environment_bindings` maps every `session_id` to its `environment_id`. `environment_workers` tracks live worker leases per environment, and `environment_reports` attaches each child's `report_dir` to that child's bound `environment_id`. A session **owns its environment** iff its binding points to an environment whose `owner_session_id == session_id`.
- **Borrowed worker vs. Owning orchestrator:** A **borrowed worker** (`straightforward`, `subtle`, `oracle`, `reviewer`, or pre-upgrade legacy `supervisor`) binds to its workspace owner's `environment_id` (`owner_session_id != session_id`) and runs in a shared container/checkout via `borrowed_locator`. An **owning orchestrator** is a depth-1 child spawned via `spawn_orchestrator` that keeps a `subagent_sessions` parent link (`parent_session_id`, finish reports, `list_agents`, `close`, drill-down UI) **and** owns its own `environments` row (`owner_session_id == child_session_id`), container or managed clone, `report_dir`, slot budget, `mj-review` (with push gate), and `ask_oracle`.
- **`SubagentPolicy` (`mj-core/src/subagent.rs:10-84`):** A session's delegation policy (`Native`, `Mjolnir { straightforward, subtle, orchestrators }`, or `None`), persisted as JSON in `sessions`, `subagent_preference`, `MoveSelection`, `StartSessionRequest`, and `WorkerLaunchConfig`.
- **Target (`TargetTemplate` in `mj-core/src/config/targets.rs`):** A configured runtime (`LocalBare`, `SshBare`, `LocalPodman`, `SshPodman`, `LocalDocker`, `SshDocker`, `AppleContainer`, `SshNitro`, `AwsEc2`) on a `Machine` (`mj-core/src/config/machines.rs`). Persistent EC2 hosts created by `mj ec2 create` (`mj-cli/src/ec2.rs`) are stored as `Machine::Ssh` + `TargetTemplate::SshPodman` or `SshNitro` (plus a disabled `SshBare` sibling).
- **Worker parking vs. Checkpointed suspend (`close`):** Worker parking (`mj-controller/src/controller/subagent_park.rs:151-223`) stops only the worker and harness processes inside an intact container (`SessionState::Parked`), preserving untracked files and waking on `send_message`. Ordinary checkpointed suspend (`mj-daemon/src/daemon/close.rs`, `mj-controller/src/controller/lifecycle.rs`) saves an origin-delta git checkpoint (tracked changes, refs, stashes — dropping untracked files), retires the container/environment, preserves the row and transcript (`SessionState::Stopped`), and causes `send_message` to refuse.
- **Versions at HEAD (`e50f987ee`):** `CONFIG_VERSION = 24` (`mj-core/src/config.rs:1257`), `SCHEMA_VERSION = 151` (`mj-store/src/database.rs:34`), `MINIMUM_COMPATIBLE_VERSION = 151` (`mj-store/src/database/schema.rs:14`), `PROTOCOL_VERSION = SUPERVISORS_PROTOCOL` (`mj-client/src/daemon.rs:3934`).


## Plan of Work


The work proceeds in three independently shippable milestones with only one breaking database migration (migration 152 in Milestone 2).


### Milestone 1: Independent UI, Telemetry, EC2 Defaults, Usage Split, and Notification Foundations (Zero Migrations)


Goal: Ship all independent improvements that already benefit today's sessions and prepare the surface for cross-target orchestrators, with zero database migrations.

1. **TUI single-line `Root › Orchestrator [X]` breadcrumb and Web breadcrumb parity (`mj-tui/src/dashboard_workspaces.rs`, `mj-tui/src/workspaces.rs`, `mj-tui/src/dashboard_input.rs`, `mj-tui/src/dashboard_sessions.rs`, `mj-server/src/web/viewer.js`):**
   Add `DashboardState::subagent_workspace_chain(&self) -> Vec<ChildWorkspace>` in `mj-tui/src/dashboard_workspaces.rs` walking upward from `self.child_workspace` via `child_workspace_for` with a cycle-guard `BTreeSet`, returning levels outermost-first. Keep `workspace_pane_height` at fixed `3` rows (`mj-tui/src/workspaces.rs:1161-1166`). Update `render_workspace_tabs` (`mj-tui/src/workspaces.rs:1214-1248`) to render the breadcrumb joined with ` › ` (e.g., `Root › Orchestrator` or `Root › Reviewers · Orchestrator`, truncating gracefully on narrow panes while keeping the deepest segment readable) alongside `[X]` (stepping up one level on `[X]`, `Esc`, or `p`, restoring selection on the exited parent). Keep `!is_subagent_session()` filtering in the flat top-level list (`dashboard_sessions.rs:563-576`) unchanged, and verify `Right`, `Sub-agents` (`ctrl+b shift+a`), and `Reviewers` drill cleanly from a selected depth-1 child into its depth-2 children. In `mj-server/src/web/viewer.js`, derive the ancestor chain from `subagent_parent_id` to render a matching `Root › Orchestrator` breadcrumb with clickable ancestor navigation and 1-level-up back behavior.

2. **TUI profile usage breakdown by subagent type (`mj-core/src/usage/report.rs`, `mj-store/src/database/usage_report.rs`, `mj-tui/src/session_usage_report.rs`):**
   Expand `UsageAgentRole` (`mj-core/src/usage/report.rs:128-146`) to `Primary` (`"Primary agents"`), `Orchestrator` (`"Orchestrators"`), `Subtle` (`"Subtle subagents"`), `Straightforward` (`"Straightforward subagents"`), `Oracle` (`"Oracles"`), `Reviewer` (`"Reviewers"`), `Subagent` (`"Other subagents"` for pre-migration-150 `worker` rows whose `category` is `NULL`), and `UnclassifiedSubagent` (`"Unclassified subagents"` for legacy destroyed children lacking both kind and category). Update `REPORT_TURNS_SQL` and role classification in `mj-store/src/database/usage_report.rs:13-18, 152-202` to read `COALESCE(session_turn_selections.category, subagent_accounting.category)` alongside `subagent_accounting.kind`, mapping `"supervisor"` and `"orchestrator"` -> `Orchestrator`, `"subtle"` -> `Subtle`, `"straightforward"` -> `Straightforward`, `"oracle"` / `kind = 'oracle'` -> `Oracle`, `"reviewer"` / `"brief_writer"` / `kind = 'reviewer'` -> `Reviewer`, `kind = 'worker'` with `NULL` category -> `Subagent`, and neither -> `UnclassifiedSubagent`. Update `mj-tui/src/dialogs/tests.rs` and `mj-tui/tests/golden/dashboard-dialogs.txt`.

3. **Remove `mj new` muted-by-default hack and unify subagent notifications onto `notifications_ignored` (`mj-cli/src/api_commands.rs`, `mj-controller/src/controller/subagents.rs`, `mj-daemon/src/daemon/create.rs`, `mj-daemon/src/daemon/attention.rs`, `mj-tui/src/dashboard_sessions.rs`, `mj-tui/src/notify.rs`, `mj-tui/src/notifications_pane.rs`, `mj-server/src/web/viewer.js`, `mj-store/src/web_push.rs`):**
   In `mj-cli/src/api_commands.rs:202-205, 1333`, remove `--with-notifications` and its `notifications_ignored: !args.with_notifications` inversion so `mj new` defaults `notifications_ignored: false`, add `--ignore-notifications` for CLI opt-out, and update `docs/src/content/docs/cli-reference.md:308, 385`. Default `notifications_ignored = true` for every newly registered subagent (`mj-controller/src/controller/subagents.rs:396` and `mj-daemon/src/daemon/create.rs:1109, 1287`) with **no backfill migration**.
   Delete the five hardcoded role-based notification filters so `SessionRecord.notifications_ignored` is the sole owner: `mj-daemon/src/daemon/attention.rs:704-708`, `mj-tui/src/dashboard_sessions.rs:841, 891`, `mj-tui/src/notify.rs:382-392`, `mj-tui/src/notifications_pane.rs:486-498`, and `mj-server/src/web/viewer.js:1323-1332` (keeping flat top-level Sessions list membership at `dashboard_sessions.rs:563-576` and `viewer.js:6576-6579`, and reusing child-aware focus at `dashboard_sessions.rs:1055-1094` for global attention stepping `:958-985`).
   Settle promoted human-owned question publication cleanly: in `mj-daemon/src/daemon/attention.rs:504-527, 704-715` publish a promoted form on **one** card owner (the child itself if unmuted, else its nearest unmuted active ancestor), and in `mj-store/src/web_push.rs:270-276` check the card's publication session `session`'s `notifications_ignored` rather than letting a muted-by-default child silently mute a question promoted onto an unmuted parent (updating `mj-store/src/database/web_push.rs:772-807` accordingly).

4. **`mj ec2 create` required container defaults and single container-size resolver (`mj-cli/src/ec2.rs`, `mj-core/src/config/targets.rs`, `mj-core/src/state.rs`, `mj-daemon/src/server_runtime/snapshot.rs`, `mj-server/src/server/viewer_types.rs`, `mj-server/src/server/api/sessions.rs`, `mj-controller/src/controller/move_session.rs`):**
   Add required positive `--cpus <u32>` and `--memory-gib <u32>` flags to `CreateArgs` (`mj-cli/src/ec2.rs:60-121`), validated `>= 1` before calling AWS, and write canonical `cpus: Some(args.cpus.to_string())` and `memory: Some(format!("{}g", args.memory_gib))` onto the created `ContainerTemplate` (`ec2.rs:1357-1366`). Share the Podman-format memory parser from `mj-microvm/src/cli.rs:570` in `mj-core`, add `ContainerTemplate::default_host_container_size(&self) -> Option<HostContainerSize>`, and centralize container default resolution into `resolve_default_container_size(template_default, remembered, limits)` (`remembered -> template_default -> BASELINE(4 CPU / 8 GiB)` clamped to `limits`) used across `snapshot.rs:509-533`, `viewer_types.rs:508-529`, `sessions.rs:94-115, 217-218`, `move_session.rs:4444`, and `list_targets`.

5. **EC2 DMI instance-type probe, 60-bucket rolling capacity history, `list_targets` from shared target projection, and enriched `list_agents` (`mj-targets/src/targets/resources.rs`, `mj-core/src/targets.rs`, `mj-core/src/targets/capacity_report.rs`, `mj-daemon/src/pollers/capacity.rs`, `mj-daemon/src/daemon/capacity.rs`, `mj-client/src/runtime_feed.rs`, `mj-core/src/subagent.rs`, `mj-worker-mcp/src/subagent_mcp.rs`, `mj-worker/src/worker_runtime/subagents.rs`, `mj-daemon/src/server_runtime/api.rs`):**
   Extend `DeploymentCapacityUsage` (`mj-core/src/targets.rs:713-728`) with `#[serde(default, skip_serializing_if = "Option::is_none")] pub ec2_instance_type: Option<String>`. Append optional Linux DMI inspection (`/sys/devices/virtual/dmi/id/sys_vendor` == `"Amazon EC2"` -> emit `ec2.instance_type=<product_name>`) into `ssh_host_capacity_command` and local Linux probe (`mj-targets/src/targets/resources.rs:59-78`, `mj-daemon/src/pollers/capacity.rs`) and parse it in `parse_host_capacity` (`resources.rs:222-238`).
   Inside `CapacityFeed` (`mj-daemon/src/daemon/capacity.rs`), retain **60 fixed per-minute buckets** per host (`MinuteCapacityBucket { minute_epoch, sample_count, cpu_sum, cpu_count, cpu_max, mem_sum_bytes, mem_max_bytes }`), updated on every clean 30-second poll and dropping buckets older than 60 minutes. Project `CapacityWindowSummary { window_seconds, sample_count, cpu_percent_avg, cpu_percent_max, memory_used_bytes_avg, memory_used_bytes_max }` onto `RuntimeHostCapacity` (`mj-client/src/runtime_feed.rs:416-423`), deriving `window_seconds` honestly from the actual covered span.
   Add `SubagentToolAction::ListTargets` and expose read-only `list_targets` in `mj-worker-mcp/src/subagent_mcp.rs` (and harness allow-lists in `mj-worker/src/worker_runtime/subagents.rs`) whenever orchestrators/supervisors are enabled. Build `list_targets` in `mj-daemon/src/server_runtime/api.rs` by reusing the existing target + host-capacity join (`api/options.rs:71-85`, `snapshot.rs:514-526`), returning enabled targets with `target_id`, `kind`, `permissions`, `is_current_target`, `host` (`machine_id`, `ec2_instance_type`, `total_cpus`, `total_memory_gib`), `default_container_size`, `active_sessions`, container `capacity` (`committed`/`remaining`), and `usage` (`recent`, `last_1h`, `probe_error`).
   Enrich `list_agents` (`mj-daemon/src/server_runtime/api.rs:1469-1558`) with `role` (`"orchestrator"` / `"subtle"` / `"straightforward"`), `target` (`session.target_template_id`), `repos` (canonical URLs from `session.project_bundle(config)`), live `profile_id` / `model` / `effort` from the session record / latest turn selection with spawn-relation fallback, and `active_children` (live direct child count).

Acceptance: All TUI goldens pass under `TZ=UTC` (including the depth-2 `Root › Orchestrator [X]` breadcrumb golden and updated usage dialog golden); `mj new` sends `notifications_ignored: false` by default while new subagents default to `true`, promoted child questions still alert unmuted parents without duplication, and toggling `"Stop ignoring"` surfaces a subagent's notifications cleanly; `mj ec2 create` refuses missing/zero `--cpus`/`--memory-gib` and sets the target default immediately; `list_targets` reports EC2 instance type, host totals, default size, and honest `recent` + `last_1h` bucket summaries; and `list_agents` returns the enriched fields.


### Milestone 2: Owned Children, Ledger Ownership Simplification, Lean `pmap` Suspend/Resume, and Breaking Migration 152


Goal: Make physical environment ownership (`environments.owner_session_id == session_id`) the single source of truth across store, controller, daemon, launch, lifecycle teardown, Parked-owner capture, Move, `Plan::ChildSwitch`, and lean `pmap` parent Suspend/Resume — fenced by one breaking migration 152.

1. **Single breaking migration 152 (`mj-store/src/database.rs`, `mj-store/src/database/schema.rs`, `mj-core/src/subagent.rs`):**
   Advance `SCHEMA_VERSION` to `152` and raise `MINIMUM_COMPATIBLE_VERSION` to `152` in one `BEGIN IMMEDIATE ... COMMIT` transaction. Classification: **breaking**, because (a) revision-151 writers run `capture_session_environment` (`environments.rs:194-224`) and would silently rebind target-bearing owned children to their parent's environment, (b) `SubagentRecord` (`record_json`, `deny_unknown_fields`) gains `#[serde(default, skip_serializing_if = "std::ops::Not::not")] pub resume_with_parent: bool`, and (c) migration 152 folds `sessions.supervisors = 1` and `supervisor_preference.enabled = 1` into `SubagentPolicy::Mjolnir { orchestrators: true }` in `sessions.subagents` and `subagent_preference.policy` (leaving migration 151's DDL dormant for rewound fixtures).

2. **Environment ledger ownership choice, incremental projection, and collapsed `state.rs` walkers (`mj-store/src/database/{sessions.rs, environments.rs, state_io.rs, committed.rs}`, `mj-core/src/state.rs`, `mj-client/src/runtime_feed.rs`, `mj-daemon/src/daemon/feed.rs`):**
   Add `own_environment: bool` to `register_subagent_with_startup` (`mj-store/src/database/sessions.rs:144-215`): when `false`, call `ensure_container_parent_available` (using the bound physical owner) and `insert_session_with_parent(&tx, &session, Some(&parent_id))`; when `true`, skip the physical parent-upgrade gate (retaining the logical parent `Closing` fence) and call `insert_session_with_parent(&tx, &session, None)` so the child atomically creates its own `environments` row (`owner_session_id == child_id`) in the same `IMMEDIATE` transaction.
   Update `capture_session_environment` (`mj-store/src/database/environments.rs:180-245`) so an existing self-owned binding (`bound.owner_session_id == session.id`) is always preserved on placement writes, while borrowed bindings keep the borrower rebind rule.
   Project `environment_owners: BTreeMap<String, String>` (`session_id -> owner_session_id`) from `environment_bindings JOIN environments` in `state_io.rs:117-125`, register `environment_bindings` triggers in `committed.rs:46-65, 410-450` for incremental delta updates, update synchronous controller inserts (`controller.rs:1239`, `subagents.rs:511`), and propagate ownership (or resolved display identity) through `runtime_feed.rs` for TUI `dashboard_sessions.rs:666`. Add `database::session_owns_environment(session_id) -> Result<bool>`.
   Collapse `mj-core/src/state.rs:2706-3055` onto `environment_owner_id`, fallible `owns_environment` (never returning `true` for unknown IDs, native child views, or dropped-parent rows), `active_borrowers` / `borrowed_members`, and `direct_subagents` / `direct_owned_child_ids`. Rewrite `checkout_for_record_inner`, `workspace_owner`, `project_identity_session` (updating `project_sources.rs:64-105`), and `review_owner` / `effective_review*` to use `environment_owner_id` with zero recursive upward walkers.

3. **One create lifecycle, deleted second parent launch build, child-owned `report_dir`, and unified upgrade gating (`mj-daemon/src/daemon/create.rs`, `mj-controller/src/controller/provisioning.rs`, `mj-worker-launch/src/worker_launch.rs`, `mj-controller/src/controller/checkpoint/layout.rs`, `mj-store/src/database/{session_restart.rs, worker_restart.rs}`):**
   - Replace `CreateInputs::TopLevel|Subagent` and `CreateKind::TopLevel|Subagent` as physical-ownership proxies (`create.rs:787-803, 926-944`) with independent physical ownership (`session_owns_environment`) and logical child metadata.
   - Collapse duplicated verify/connect/commit (`provisioning.rs:193-274` vs `:349-462`) and reset/failure-cleanup (`:285-346` vs `:473-564`) into one create lifecycle parameterized by ledger ownership, ensuring failed owned-child startup retires its allocated container/worktree and clears its target while preserving logical child failure reporting.
   - In `mj-worker-launch/src/worker_launch.rs:665-698, 858-899`, delete the duplicate second parent `WorkerLaunchConfig` build: resolve the physical owner once, compute effective `cwd` / `additional_directories` once from its checkout/bundle/mounts (resolving relative `working_directory` against the spawning parent's effective `cwd` for borrowed children only, and using the child's own checkout for owned children without requiring a live parent), and reuse that path math in memory sync (`:978-1015`).
   - For owned children, create `report_dir` on the provisioned child target before first worker launch (`worker_launch.rs:721-730`) and persist via `record_subagent_report_dir_with` so the `environment_reports` SQL trigger attaches it to the child's own `environment_id`. In `checkpoint/layout.rs:15-36`, export report dirs only for children sharing the session's `environment_id`.
   - Unify container-upgrade and wake gates in `session_restart.rs:469-539` and `worker_restart.rs:52-74` onto one `environment_id` peer-binding query (keeping `recreating_container` vs `container_upgrade_owns_target` phase semantics distinct).

4. **Collapsed physical teardown, Parked-owner capture, auto-archive safety, Workspace Close, Move exclusion, `Plan::ChildSwitch`, and lean `pmap` parent Suspend/Resume (`mj-daemon/src/daemon.rs`, `mj-daemon/src/daemon/{close.rs, close_workspace.rs, support.rs, resume.rs, continuation/quota.rs}`, `mj-controller/src/controller/{lifecycle.rs, lifecycle/origin_delta.rs, move_session.rs}`):**
   - Route `close_route` (`daemon.rs:544-563`), origin-delta acceptance (`lifecycle.rs:120-169`, `close.rs:185-220`), and all six physical branches (`lifecycle.rs:707, 2301-2329, 2467-2482, 2601-2618, 2713-2724, 2882-2892`) on physical ownership (`owns_environment`), rejecting unknown IDs directly.
   - Extract one physical teardown operation (borrower stop / owner quiesce / owner retire) shared across verified-stop, settle, and no-checkpoint close, deleting `execute_target_cleanup` (`lifecycle.rs:3088-3100`), the unreachable borrower shortcut in `has_nothing_to_checkpoint` (`:3067-3081`), and `stop_subagents_and_remove_reports` (`close.rs:361-366`). Use one `stop_borrowed_workers` for Move and Suspend, replacing Suspend's misleading `before_move_source_stop()` call (`lifecycle.rs:994-1003`) with an explicit pre-quiesce borrower-stop hook.
   - Support **Parked-owner checkpointed capture**: add an already-quiescent entry into origin-delta capture (`lifecycle.rs:1004-1024, 1134-1180`, `origin_delta.rs:483-535`) booting `CheckpointOnly` directly without starting a live harness, preserving a durable continuation across `Closing`, and rolling back to `Parked` (fixing `lifecycle.rs:3170-3175`) on cancellation or pre-commit failure. Scope `origin_delta.rs:387, 516-517` quiesce children to the same `environment_id`.
   - Fix **auto-archive retention and disposal** (`resume.rs:395-495`): give owning orchestrators top-level auto-archive retention and prevent auto-archiving a stopped parent from destroying live or `resume_with_parent` owning orchestrators (leaving `mj-sessionwiki/src/sessionwiki/top_level.rs` untouched!).
   - Scope Workspace Close root elision (`close_workspace.rs:19-35`) to same-environment borrowers so every owning orchestrator is its own close root.
   - Scope Move (`move_session.rs:1571, 1648-1650, 2820-2836, 4224-4276`, `support.rs:309-313`) strictly to `active_borrowers` (collapsing `live_move_children` + `roleless_move_children_error` into one formatter) so Move never touches owning orchestrators.
   - Keep `Plan::ChildSwitch` for owning orchestrators (`continuation/quota.rs:411-479`, `subagent_park.rs:281-413`) with `Placement::Orchestrator` and category `"orchestrator"`.
   - **Lean `pmap` Parent Suspend/Resume cascade:** Extract the ~30-line concurrent `JoinSet` suspend helper from `close_workspace.rs:100-140` (which drops outer `upgrade_work` while waiting and aggregates failures). Extend parent Suspend warning payloads/dialogs (`turns.rs:494-532`, TUI/Web/CLI) to note that live/parked direct owning orchestrators will also be suspended (untracked files dropped). When standalone parent Suspend runs (skipped when invoked as a Workspace Close member so owners are never double-submitted), `pmap` ordinary `suspend_session` (with blanket untracked consent, using Parked-owner capture when `Parked`) over direct non-terminal owning children, marking `SubagentRecord.resume_with_parent = true` on each orchestrator that finishes suspending (and clearing it on explicit `close` or resume), then complete parent suspend regardless of child failures while reporting any failed child. On parent `Resume` (and repeated `Resume` on an already-`Running` parent), `pmap` ordinary `resume_session` over direct owning children with `resume_with_parent == true && state == Stopped`, clearing `resume_with_parent = false` on resume and surfacing any failed child resumes in a notice.

Acceptance: Migration 151 -> 152 raises `MINIMUM_COMPATIBLE_VERSION = 152` and folds legacy `sessions.supervisors`/`supervisor_preference` into `SubagentPolicy::Mjolnir`; controller/daemon integration tests verify preserved ledger bindings, incremental `committed.rs` updates, failure cleanup retiring owned containers, Parked-owner `close` producing a verified origin-delta checkpoint (and rolling back to `Parked` on cancel), auto-archive safety, container-upgrade independence, parent Move leaving owned children untouched, `Plan::ChildSwitch` on owned orchestrators, and `pmap` parent Suspend/Resume suspending and reviving owning orchestrators (while skipping manually `close`d ones and reporting partial failures without blocking the parent).


### Milestone 3: The Switch — Single-Owner `SubagentPolicy`, Tool Split, `CreateProject` `repos`, `mj-review` & `ask_oracle`, Slot Rule, and Rename (Zero New Migrations)


Goal: Switch user- and agent-facing surfaces to single-owner `SubagentPolicy::Mjolnir { orchestrators }`, split `spawn` into `spawn_worker` and `spawn_orchestrator`, remove `supervisors_available_for_target`, enable `mj-review` and `ask_oracle` for owning orchestrators, enforce `default_subagent_limit() = 8` and `1`-per-orchestrator slot counting, and finish the `supervisor -> orchestrator` rename.

1. **Single-owner `SubagentPolicy::Mjolnir { orchestrators }` and removal of `supervisors_available_for_target`:**
   Add `#[serde(default, skip_serializing_if = "std::ops::Not::not")] orchestrators: bool` to `SubagentPolicy::Mjolnir` (`mj-core/src/subagent.rs:15-20`, with `Wire` deserializing `orchestrators` and `SubagentPolicy::for_launch` forcing `orchestrators = false` on child launches). Delete `SessionRecord.supervisors`, `State.last_supervisor_choices`, `MoveSelection.supervisors`, `MoveRecoveryView.supervisors`, `StartSessionRequest.supervisors`, daemon RPC `supervisors`, toggle-only Move restart checks, non-Mjolnir reset/validation boilerplate, `Config::supervisors_available_for_target` (`mj-core/src/config.rs:1124`), and all `supervisors_available` / `last_supervisor_choices` feed and viewer maps! Replace availability gating with `Config::orchestrators_configured(&self) -> bool { !self.subagents.primary.is_empty() }`. Keep CLI `--orchestrators` on `mj new` / `mj move` as a tiny helper setting `orchestrators = true` inside `SubagentPolicy::Mjolnir` (refusing if combined with `--subagents native|none`; no hidden `--supervisors` alias). Update TUI and Web wizards to toggle `orchestrators` directly inside their `SubagentPolicy::Mjolnir` draft whenever `orchestrators_configured()` is true (automatically remembered per profile via existing `subagent_preference` / `last_subagent_choices` and carried across Move with zero extra fields). Compute `WorkerLaunchConfig.supervisors_enabled` from `subagent.is_none() && launch.subagents.orchestrators_enabled()` so retained older workers stay 100% wire-compatible.

2. **MCP tool split (`spawn_worker` / `spawn_orchestrator`), wire reuse, and unified capability builder (`mj-core/src/subagent.rs`, `mj-worker-mcp/src/subagent_mcp.rs`, `mj-worker/src/worker_runtime/subagents.rs`, `mj-core/src/session_instructions.rs`, `mj-core/assets/orchestrator-delegation.md`):**
   - Define `WorkerTaskType { Straightforward, Subtle }` in `mj-core/src/subagent.rs` so `"orchestrator"` is unrepresentable on `spawn_worker`.
   - Split `spawn` into `spawn_worker` (exact current worker signature, emitting the **existing `SubagentToolAction::Spawn` wire variant** with `Straightforward`/`Subtle`) and `spawn_orchestrator` (exposed only to `Parent` when `orchestrators_enabled` is true; emitting new `SubagentToolAction::SpawnOrchestrator`; parameters: required `task_name`, required `instructions`, optional `target`, optional `repos: Vec<String>`, optional `no_github_credentials`; no `task_type`, `working_directory`, `profile`, `model`, `effort`, `cpus`, or `memory`). Document in `spawn_orchestrator`'s description that omitting `repos` inherits the parent's project bundle (with its image/tooling), explicit `repos` creates/reuses a bundle for those URLs, bare targets require an existing directory (so use a container target for URL-only `repos`), use `fetch` for remote files/reports, and `close` checkpoints tracked commits and tears down the container and `report_dir`.
   - Keep legacy `Spawn { task_type: Supervisor }` from retained older workers creating borrowed supervisors until workers upgrade naturally.
   - Consolidate MCP role tool definitions, call acceptance (`subagent_mcp.rs:405-456, 583-590, 693-704`), and harness allow-lists (`worker_runtime/subagents.rs:217-229` + ACP) onto one capability builder; parse `launch.json` once; remove inert `ParentTools.agent_mailboxes_enabled`; stop exposing the undisclosed `SendInput` alias on new MCP (retaining edge decode/ack + shared own-child delivery); and remove `LegacyInterruptAgent` from the mutating lane.

3. **`spawn_orchestrator` handler (`CreateProject` `repos`, explicit container allocation, `close` as Suspend):**
   - Resolve destination `target` (defaulting to parent's `target_template_id`, refusing unknown/disabled targets).
   - Resolve `repos`: omitted passes parent's `bundle_id` and accepted `ProjectBundleSnapshot` (refusing clearly if parent is a plain directory without cloneable URLs); explicit `repos` validates non-empty (no speculative `1..=32` cap), refuses URL-only `repos` on bare targets naming a container target, and persists/deduplicates via existing daemon `CreateProject` / `create_bundle_from_sources` (`mj-daemon/src/daemon/state.rs:896`, `mj-controller/src/controller.rs:283`).
   - Always pass an explicit `SessionResourceAllocation::Container` computed by Milestone 1's `resolve_default_container_size` for container targets.
   - Select `(profile_id, model, effort)` from `[[subagents.primary]]` using destination target's `ExecutionPolicy` and `Placement::Orchestrator` (`harness.supports_delegation_tools()`) under atomic `PlacementReservation`.
   - Enforce slot check (below), inherit `workspace_id`, `no_github_credentials = parent.no_github_credentials || req.no_github_credentials`, `no_github_events = true`, `notifications_ignored = true`, `subagents = parent.subagents.for_launch(...)` (Mjolnir with `orchestrators: false`), and register atomically via `register_subagent_with_startup(..., own_environment = true)`.
   - On `close` of an owning orchestrator (`api.rs:1667-1685`), invoke checkpointed Suspend with blanket untracked consent (clearing `resume_with_parent = false` even if already `Stopped`) and return any dropped-untracked summary in the `close` tool result (updating `close`'s description accordingly).

4. **`mj-review` (with `review_gate`) and `ask_oracle` for owning orchestrators (`mj-worker-launch/src/worker_launch.rs`, `mj-core/src/worker_launch.rs`, `mj-controller/src/controller/subagents.rs`, `mj-worker-mcp/src/subagent_mcp.rs`, `mj-daemon/src/server_runtime/api/oracle.rs`, `mj-core/src/subagent.rs`):**
   - Enable `launch.review_trigger` in `worker_launch.rs:842-855` iff `owns_environment == Some(true)`.
   - Simplify `WorkerLaunchConfig::review_gate()` (`mj-core/src/worker_launch.rs:532-549`) to `self.review_tools() && self.reviewer_launch.is_none()` (and `prepares_review()` to `self.review_trigger == Push && self.reviewer_launch.is_none()`), enforcing the pre-push hook on owning orchestrators with zero new `WorkerLaunchConfig` wire fields.
   - Clear all four `ask_oracle` blockers for owning orchestrators (`subagents.rs:392-397`, `worker_launch.rs:782`, unified MCP capability builder + harness allow-lists, `api/oracle.rs:18-35`, and `validate_subagent_parent` permitting owning orchestrators to spawn `Worker`, `Oracle`, and `Reviewer` while forbidding depth-2 orchestrators). Leave `mj-memory` / project-memory gating untouched.

5. **Slot rule (`default_subagent_limit() = 8`, orchestrator counts `1`) and `supervisor -> orchestrator` rename:**
   - Update `default_subagent_limit()` (`mj-core/src/config.rs:726`) from `6` to `8` (updating settings fallback/docs in `mj-client/src/settings/schema.rs` and `mj-core/assets/skills/mj/references/configuration.md`).
   - Simplify `ensure_subagent_slot_available_for` (`mj-controller/src/controller/subagents.rs:548-620`): delete `max_supervisors = maximum / 2` and `maximum >= 2`; count live physically borrowed workers in `owner_id`'s environment (`active_borrowers(owner_id)` where `kind == Worker && has_live_worker()`) **plus** live direct owning orchestrators of `owner_id` (`direct_owned_child_ids(owner_id)` where `has_live_worker()`, each counting as `1` without recursing into depth-2 workers), and enforce `live.len() < max_concurrent`.
   - Rename user-, agent-, and code-facing `supervisor` / `supervisors` terms to `orchestrator` / `orchestrators` (Rust identifiers, CLI `--orchestrators`, `PROTOCOL_VERSION` bump `ORCHESTRATORS_PROTOCOL`, TUI/Web/Settings labels `"Orchestrators"` and `"Primary and orchestrator"`, `mj-core/assets/orchestrator-delegation.md`, goldens), preserving SQLite (`category = 'supervisor'`, `record_json` key `"supervisor"`) and `WorkerLaunchConfig` wire keys (`"supervisor"`, `"supervisors_enabled"`) via `#[serde(rename = ...)]`.

Acceptance: End-to-end tests prove `SubagentPolicy::Mjolnir { orchestrators }` works across New, Move, remembered preferences, and CLI with zero parallel fields; `spawn_orchestrator` provisions isolated remote and same-target containers with `repos` via `CreateProject`, enables `mj-review` (with push gate active) and `ask_oracle`, counts as `1` toward `max_concurrent` (default `8`), keeps legacy `Spawn` from older workers creating borrowed supervisors, passes in-place Move against a retained older `WorkerLaunchConfig` decoder, and `close` checkpoints and reclaims the container while reporting dropped untracked files.


## Concrete Steps


All Mjolnir commands run from the repository root (`/home/jonathan/Projects/mjolnir/.mj/clones/53e6a94d7d00a621701f78fff35430e9` or active detached worktree) on the dev profile outside the restricted sandbox (per `AGENTS.md`, `cargo test` needs loopback TCP and Unix sockets).

Milestone 1 validation:

    TZ=UTC cargo test -p brokk-mj-tui golden
    cargo test -p brokk-mj-tui subagent_workspace
    cargo test -p brokk-mj-store usage_report
    cargo test -p brokk-mj-store web_push
    cargo test -p brokk-mjolnir ec2
    cargo test -p brokk-mj-targets resources
    cargo test -p brokk-mj-daemon capacity
    cargo test -p brokk-mj-daemon list_targets
    cargo test -p brokk-mj-daemon list_agents
    cargo test -p brokk-mj-daemon attention
    cargo clippy -p brokk-mj-core -p brokk-mj-targets -p brokk-mj-store -p brokk-mj-client -p brokk-mj-daemon -p brokk-mj-server -p brokk-mj-tui -p brokk-mjolnir -p brokk-mj-worker-runtime -p brokk-mj-worker-mcp --all-targets -- -D warnings

Milestone 2 validation:

    cargo test -p brokk-mj-store environments
    cargo test -p brokk-mj-store schema
    cargo test -p brokk-mj-store committed
    cargo test -p brokk-mj-core state
    cargo test -p brokk-mj-controller lifecycle
    cargo test -p brokk-mj-controller move_session
    cargo test -p brokk-mj-controller provisioning
    cargo test -p brokk-mj-daemon close
    cargo test -p brokk-mj-daemon close_workspace
    cargo test -p brokk-mj-daemon resume
    cargo test -p brokk-mj-daemon continuation::quota
    cargo clippy -p brokk-mj-core -p brokk-mj-store -p brokk-mj-controller -p brokk-mj-daemon -p brokk-mj-worker-launch --all-targets -- -D warnings

Milestone 3 validation:

    cargo test -p brokk-mj-core subagent
    cargo test -p brokk-mj-core worker_launch
    cargo test -p brokk-mj-core config
    cargo test -p brokk-mj-worker-mcp subagent_mcp
    cargo test -p brokk-mj-controller subagents
    cargo test -p brokk-mj-daemon spawn_orchestrator
    cargo test -p brokk-mj-daemon oracle
    TZ=UTC cargo test -p brokk-mj-tui golden
    cargo clippy -p brokk-mj-core -p brokk-mj-client -p brokk-mj-controller -p brokk-mj-daemon -p brokk-mj-server -p brokk-mj-worker-runtime -p brokk-mj-worker-mcp -p brokk-mj-worker-launch -p brokk-mj-tui -p brokk-mjolnir --all-targets -- -D warnings

Isolated live end-to-end validation (never touch the default instance; stop the test daemon when done):

    cargo build -p brokk-mjolnir
    ./target/debug/mj --instance orch-e2e daemon status
    # exercise list_targets, spawn_orchestrator across targets, depth-2 spawn_worker,
    # request_review + ask_oracle inside orchestrator, parent Move, parent Suspend + Resume,
    # TUI Root > Orchestrator breadcrumb + usage dialog, and manual close
    ./target/debug/mj --instance orch-e2e daemon stop


## Validation and Acceptance


Per `AGENTS.md` testing rules, add only golden, end-to-end, integration, and tricky-condition tests:

1. **Goldens (`mj-tui`, `tests/e2e/web`):**
   - New TUI depth-2 nesting golden proving the Workspaces pane renders the single-line `Root › Orchestrator [X]` breadcrumb (fixed 3-row height) while orchestrators stay absent from the flat top-level session list, plus interactive test covering `Right` -> depth 2, `[X]`/`Esc`/`p` -> depth 1 with orchestrator selected -> top-level with root selected.
   - Updated `mj-tui/tests/golden/dashboard-dialogs.txt` proving profile usage splits `Orchestrators`, `Subtle subagents`, and `Straightforward subagents` (plus `Oracles`, `Other subagents`, and `Unclassified subagents`).
   - Updated TUI/Web wizard and settings goldens reflecting `"Orchestrators"` / `"Primary and orchestrator"`, single-owner `SubagentPolicy::Mjolnir { orchestrators }`, and removal of `supervisors_available`.
2. **Integration — Notifications, `mj ec2 create`, `list_targets`, and Migration 152 (`mj-store`, `mj-targets`, `mj-cli`, `mj-daemon`):**
   - `mj new` sends `notifications_ignored: false` by default and `true` with `--ignore-notifications`; new subagents default to `notifications_ignored: true`; promoted human-owned questions still push through an unmuted parent without duplicate cards when a child opts in; toggling a subagent's `notifications_ignored` to `false` surfaces its notifications in daemon, TUI, and Web cleanly.
   - `mj ec2 create` refuses missing/zero `--cpus`/`--memory-gib` and populates `ContainerTemplate.{cpus, memory}`; `resolve_default_container_size` honors `remembered -> template_default -> BASELINE`.
   - `parse_host_capacity` parses optional Linux DMI `ec2.instance_type`; `CapacityFeed` 60-minute fixed-bucket ring computes accurate `recent` + `last_1h` avg/max with honest `window_seconds` on partial history and exposes them via `list_targets`.
   - Breaking migration 152 raises `MINIMUM_COMPATIBLE_VERSION = 152`, migrates `sessions.supervisors = 1` / `supervisor_preference.enabled = 1` into `SubagentPolicy::Mjolnir { orchestrators: true }`, and round-trips `SubagentRecord.resume_with_parent`; `WorkerLaunchConfig` remains accepted by a strict retained-worker decoder (`relay >= 81`) during in-place Move; daemon accepts legacy `SubagentToolAction::Spawn`.
3. **Tricky Conditions — Ledger Ownership, Incremental Projection, Parked Capture, Capabilities, Slots, and `pmap` Suspend/Resume (`mj-store`, `mj-controller`, `mj-daemon`, `mj-worker-launch`):**
   - `capture_session_environment` preserves a self-owned binding across placement writes; `committed.rs` updates `environment_owners` incrementally on binding changes; `State::owns_environment` returns `None` (never `true`) for unknown IDs, native child views, and dropped-parent rows.
   - Failed owned-child startup retires its allocated container/clone and clears its target; container upgrade on parent does not block owned-child registration/wake/unpark and vice versa.
   - Closing a `Parked` owning orchestrator enters already-quiescent origin-delta capture without booting a live harness, rolls back to `Parked` on cancel, saves unpushed tracked commits, retires the container, preserves the transcript past auto-archive classification, reports dropped untracked paths, and makes `send_message` refuse.
   - Owning orchestrator receives `mj-review` with `review_gate() == true` and `ask_oracle`, cannot spawn depth-2 orchestrators, and launches cleanly without a live parent; borrowed workers get neither tool and keep existing `mj-memory`.
   - Slot cap: default `max_concurrent` is `8`; with `max_concurrent = 2`, a root running 1 live orchestrator (itself running 2 live depth-2 workers in its own container) can still spawn 1 more child, while the orchestrator refuses a 3rd depth-2 worker.
   - In-place and cross-target parent Move leaves owning orchestrators completely untouched while stopping/reconciling borrowed workers; `Plan::ChildSwitch` swaps an owning orchestrator's profile in place using `Placement::Orchestrator` without disturbing depth-2 workers.
   - Parent Suspend `pmap` suspends owning orchestrators concurrently (without blocking parent on a single child failure, without double-submitting under Workspace Close), sets `resume_with_parent = true`, skips already-closed orchestrators, and parent Resume `pmap` brings them back (clearing `resume_with_parent`, never reviving manually `close`d orchestrators, and retrying any remaining `resume_with_parent` children on a second Resume).


## Idempotence and Recovery


Breaking migration 152 (Milestone 2) runs inside `BEGIN IMMEDIATE ... COMMIT` and raises `MINIMUM_COMPATIBLE_VERSION = 152` atomically in the same transaction so older binaries refuse rather than corrupting owned-child bindings or `resume_with_parent`. Milestones 1 and 3 require zero database migrations.

`spawn_orchestrator` reuses `register_subagent_with_startup`'s single `IMMEDIATE` transaction and durable `request_id` / `delegation_effects` idempotency, so retries or daemon crashes mid-provision never leak unlinked top-level sessions or spawn duplicate containers, and create recovery finishes or cleans up owned environments deterministically.

Parent Suspend releases outer parent handoff admission while the shared `close_workspace.rs` `JoinSet` helper waits on child suspends, records `resume_with_parent = true` only on orchestrators that actually suspended via parent suspend, and clears it on resume or explicit `close`; any orchestrator left `Stopped` with `resume_with_parent = true` after a partial failure can be resumed individually from drill-down or by re-invoking Resume on the parent.

`CapacityFeed`'s 60 fixed per-minute buckets live in daemon memory and repopulate automatically on restart, reporting the actual elapsed `window_seconds` and `sample_count`.


## Artifacts and Notes


Reconnaissance, Advisor, and simplification audit reports backing this plan:

- Initial maps under `.mj/agents/`: `f626096ff8433f7d31e054648db51215/rename-and-config-map.md`, `d4a6b5087d31fa481e428b74aaf48dbb/provisioning-ec2-targets-map.md`, `bf77066c38780b455eca49c82805856d/lifecycle-tools-cascades-map.md`, `b44fae74a5ad6deefc9f581f03fc006c/tui-web-nesting-map.md`, `d37ef09d53120d46b41dc5c6b20b963f/usage-and-notifications-map.md`.
- MJ Advisor (`91233b0efdd193066e7b4eeb46c8db10`) review and fetched static audits at `d7de33a91`: `17ed9edfd13e1ddf2bc9621d840a29d9/findings.md`, `ad132332da308c80e190f5f44e74131b/findings.md`, `8a7a24177f947aa5615402c9fd9483b8/findings.md`.
- Simplification audits under `.mj/agents/`: `36f1110996da6805f8994ca2aff5605b/simplify-ownership-and-mcp.md`, `3aea9888508f7d96a42dc6a4969251ed/simplify-lifecycle-and-notifications.md`, `4f76e7267a5ae929c2f79f1c1b4edb01/simplify-targets-usage-ui.md`.


## Interfaces and Dependencies


Key types, MCP tool schemas, and function signatures to establish:

In `mj-core/src/subagent.rs`:

    pub enum SubagentPolicy {
        #[default]
        Native,
        Mjolnir {
            #[serde(skip_serializing_if = "Option::is_none")]
            straightforward: Option<Box<crate::config::SubagentChoice>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            subtle: Option<Box<crate::config::SubagentChoice>>,
            #[serde(default, skip_serializing_if = "std::ops::Not::not")]
            orchestrators: bool,
        },
        None,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum WorkerTaskType {
        Straightforward,
        Subtle,
    }

    pub enum SubagentTaskType {
        Straightforward,
        Subtle,
        Oracle,
        #[serde(rename = "supervisor", alias = "orchestrator")]
        Orchestrator,
    }

    // On SubagentRecord (Milestone 2, breaking migration 152):
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resume_with_parent: bool,

    // Wire actions: spawn_worker reuses existing Spawn; add only:
    SpawnOrchestrator {
        task_name: String,
        instructions: String,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        repos: Option<Vec<String>>,
        #[serde(default)]
        no_github_credentials: bool,
    },
    ListTargets,

In `mj-core/src/state.rs`:

    // Projected from environment_bindings JOIN environments (full load + committed deltas):
    pub environment_owners: SnapshotMap<String, String>,

    // Minimal ownership and membership primitives (fail-closed; never true by absence):
    pub fn environment_owner_id(&self, session_id: &str) -> Option<&str>;
    pub fn owns_environment(&self, session_id: &str) -> Option<bool>;
    pub fn active_borrowers(&self, owner_id: &str) -> Vec<String>;
    pub fn direct_owned_child_ids(&self, parent_id: &str) -> Vec<String>;

    pub fn resolve_default_container_size(
        template_default: Option<HostContainerSize>,
        remembered: Option<HostContainerSize>,
        limits: Option<HostContainerSize>,
    ) -> HostContainerSize;

In `mj-store/src/database/sessions.rs` and `mj-store/src/database/environments.rs`:

    pub fn register_subagent_with_startup(
        session: &SessionRecord,
        subagent: &mj_core::subagent::SubagentRecord,
        startup: Vec<super::startup::StartupDelivery>,
        report_dir: Option<&str>,
        category: Option<&str>,
        own_environment: bool,
    ) -> Result<()>;

    pub fn session_owns_environment(session_id: &str) -> Result<bool>;

In `mj-core/src/config.rs` and `mj-core/src/config/targets.rs`:

    // default_subagent_limit() -> 8
    impl Config {
        pub fn orchestrators_configured(&self) -> bool;
    }

    impl ContainerTemplate {
        pub fn default_host_container_size(&self) -> Option<HostContainerSize>;
    }

In `mj-core/src/targets.rs` and `mj-core/src/targets/capacity_report.rs`:

    // Added to DeploymentCapacityUsage:
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ec2_instance_type: Option<String>,

    pub struct CapacityWindowSummary {
        pub window_seconds: u64,
        pub sample_count: u32,
        pub cpu_percent_avg: Option<f64>,
        pub cpu_percent_max: Option<u8>,
        pub memory_used_bytes_avg: u64,
        pub memory_used_bytes_max: u64,
    }

In `mj-core/src/usage/report.rs`:

    pub enum UsageAgentRole {
        Primary,
        Orchestrator,
        Subtle,
        Straightforward,
        Oracle,
        Reviewer,
        Subagent,             // pre-migration-150 worker rows with NULL category
        UnclassifiedSubagent, // legacy destroyed children lacking kind
    }

In `mj-tui/src/dashboard_workspaces.rs`:

    pub(crate) fn subagent_workspace_chain(&self) -> Vec<ChildWorkspace>;

MCP `list_targets` JSON shape (one entry per enabled target):

    {
      "targets": [
        {
          "target_id": "ec2-c8id16",
          "kind": "ssh-podman",
          "permissions": "yolo",
          "is_current_target": false,
          "host": {
            "machine_id": "ec2-c8id16",
            "ec2_instance_type": "c8id.16xlarge",
            "total_cpus": 64,
            "total_memory_gib": 128.0
          },
          "default_container_size": { "cpus": 16, "memory_gib": 32.0 },
          "active_sessions": 2,
          "capacity": {
            "committed_cpus": 32,
            "remaining_cpus": 32,
            "committed_memory_gib": 64.0,
            "remaining_memory_gib": 64.0
          },
          "usage": {
            "recent": {
              "sampled_age_seconds": 14,
              "cpu_percent": 38,
              "memory_used_gib": 41.2,
              "memory_total_gib": 128.0
            },
            "last_1h": {
              "window_seconds": 3600,
              "sample_count": 120,
              "cpu_percent_avg": 31.4,
              "cpu_percent_max": 74,
              "memory_used_gib_avg": 38.9,
              "memory_used_gib_max": 52.1
            },
            "probe_error": null
          }
        }
      ]
    }

MCP `list_agents` enriched entry shape (additive fields alongside existing fields):

    {
      "child_session_id": "...",
      "task_name": "...",
      "name": "...",
      "role": "orchestrator",
      "target": "ec2-c8id16",
      "repos": ["https://github.com/BrokkAi/mjolnir"],
      "profile_id": "codex3",
      "model": "gpt-6.1-sol",
      "effort": "high",
      "state": "running",
      "active_children": 2,
      "parked": false,
      "promptable": true
    }

---

Revision note (2026-11-23 02:02Z): Final streamlined 3-milestone revision incorporating user's pushback and simplifications: dropped the notification backfill migration (new subagents default muted; zero migrations in Milestone 1), left SessionWiki `top_level.rs` indexing untouched (keeping auto-archive retention only), simplified parent Suspend/Resume cascade to a lean concurrent `pmap` sharing `close_workspace.rs`'s `JoinSet` executor (no multi-owner preflight/consent UX, no persisted `SuspendCause`, no daemon startup resume sweep), collapsing the entire plan to 3 independently shippable milestones and a single breaking migration 152 (in Milestone 2), and updated crate paths for `origin/master` `e50f987ee` (`mj-worker-mcp`, `mj-microvm`, `mj-worker-review`, `mj-github-watch`, `mj-session-types`).
