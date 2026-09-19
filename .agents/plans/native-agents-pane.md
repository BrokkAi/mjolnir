# Show harness-native agents in the subagents pane

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Claude and Codex users must see harness-created agents in the same subagents pane as Mjolnir-managed children, with names, tasks, lifecycle, nesting and live transcripts. Controls follow each provider's capabilities. Kimi's detached agents use this presentation without limiting Claude or Codex. Shell processes remain background tasks.

## Progress

- [x] (2026-09-19) Inspect native ACP contracts, current worker routing, background tasks and pane ownership; agree scope with user.
- [x] (2026-09-19) Implement native agent contracts, durable projection and atomic staged recovery.
- [x] (2026-09-19) Negotiate and route Claude/Codex children; retain Kimi agent classification.
- [x] (2026-09-19) Present native children, paged transcripts, nesting, and capability-gated stop controls.
- [x] (2026-09-19) Extend Codex cancellation and replay generation recovery; typecheck, 592 tests, build, real guardian/yolo cancellation, and real replay/resumption passed; committed in sibling fork.
- [x] (2026-09-19) Finish Rust validation, merge upstream into current branch, and push both upstreams as requested. Implementation reached origin/master at 7f6948f6 and brokkai/main at a95367e.

## Surprises & Discoveries

Both pinned Claude ACP 0.79.0 and the sibling Codex fork expose `subagent_spawned`, `subagent_state_update`, and ordinary updates addressed to child session IDs after negotiating `_meta.jetbrains.air` version 1 capability `nativeSubagentSessions`. Both initially advertise empty child control capabilities. The fork now advertises cancel for live children. Live replay required seeding the live router with historical generations to avoid identity reuse. Mjolnir currently discards the addressed session ID before relay ingestion. Claude background levels include `task_type`, and Kimi wire records include `kind`, but existing Mjolnir task types discard this distinction.

## Decision Log

The user selected viewing plus supported stop controls, expanded coverage to Claude and Kimi, and explicitly made Claude/Codex capability the scope driver. Native children remain owned by the parent worker, never independent provisioned sessions. Changes to `/home/jonathan/Projects/codex-acp` are authorized. Publication, live-store incompatible migration, and restarting the user's live work are not part of implementation. Date: 2026-09-19.

## Outcomes & Retrospective

Implementation is present across the worker, durable relay, database, runtime feed, and TUI. Native children use presentation-only session rows and never enter controller provisioning. Database migration 40 is breaking; all validation uses isolated stores. Full merged Rust tests passed, as did focused ACP recovery and mixed managed/native navigation tests, cargo fmt, and cargo clippy --all-targets -- -D warnings. The adapter changes are committed and pushed, and the merged mj implementation is committed and pushed.

## Context and Orientation

`mj-worker/src/acp/drive.rs` receives raw ACP updates; `session.rs` negotiates capabilities. Runtime events enter the worker's durable relay through `mj-worker/src/worker_runtime/unix/dispatch.rs`. `mj-transcript/src/projection` reduces relay observations to database mutations. `mj-controller/src/database/materialized.rs` persists projections; daemon snapshots deliver state to the TUI. `mj-tui/src/dashboard_workspaces.rs` and `dashboard_sessions.rs` currently list only independently managed child sessions. The sibling Codex adapter's `src/subagents/CodexSubagentEventRouter.ts` owns native child IDs, nesting and generation identities.

## Plan of Work

First add shared native identities and lifecycle observations, keeping child transcript changes isolated from parent transcript/configuration/goal/usage. Persist metadata separately from transcript rows; load summaries without loading complete transcripts. Replayed native history replaces a staged child projection only after load succeeds, retaining prior data on failure. Repeated loads must not duplicate content.

Next negotiate the extension and route child updates, permissions and status. Preserve Claude `local_agent` and Kimi agent/process classification so agent entries are not counted twice. Unknown task kinds stay background tasks. Disconnects settle as disconnected, never completed.

Then generalize pane selection and counts while reusing transcript rendering. Native children have no direct prompt composer or independent provisioning/cleanup controls. Route supported cancellation to the owning worker in supervised background operations with visible pending state and errors.

Finally extend Codex cancellation using its owned child-to-thread mapping and active turn IDs; reject stale generations. Do not advertise close until actually implemented. Claude's empty capabilities mean viewing without stop. Keep published dependency pins unchanged pending separate release authorization.

## Milestones

The first milestone proves deterministic parent/child separation and persistence through reducer and database tests. The second proves wire negotiation, nested routing, replay and capability handling with adapter fixtures. The third proves pane navigation, transcript rendering and task separation using input/render behavior tests. The final milestone proves Codex child cancellation leaves parent and sibling turns intact, including live isolated guardian and yolo probes plus history replay and resumption. All milestones are complete.

## Concrete Steps

Work from `/home/jonathan/Projects/hel2`. Run `cargo fmt --all -- --check`, elevated `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Work from `/home/jonathan/Projects/codex-acp` for `npm run typecheck`, `npm test`, `npm run build`, and live probes following its run-codex skill. Use isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR` for new-build integration checks; never launch this build against the live store.

## Validation and Acceptance

Test concurrent and nested children, completion/failure/cancellation, generation reuse, permissions, independent tools with equal IDs, restart/replay deduplication and interrupted replay. Test native agents alongside managed agents and background processes, capability-gated controls and responsive cancellation failures. Adapter probes use guardian and yolo policies. Tests must fail against the old behavior and pass after implementation.

## Idempotence and Recovery

Advance database revision and minimum compatible read/write revision atomically for the new persisted event variants, classified breaking. Advance relay compatibility as necessary. Keep existing journals readable without changing their encoded historical digests. Stage and commit only changed files on existing branches. The user subsequently authorized merge and push on both existing upstreams; do not publish a package or tag. Live user store and sessions remain untouched.

## Interfaces and Dependencies

Use a native child identity scoped by owning mj session and opaque ACP session ID. Preserve immediate parent IDs and adapter generation IDs. Shared records carry names, tasks, lifecycle and cancel/close capabilities. Native child projections reuse `MaterializedSession` transcript types without becoming provisionable `sessions` rows. Existing async operation supervision and subprocess helpers remain authoritative.

## Artifacts and Notes

The adapter has 592 passing tests (26 skipped), a passing typecheck/build, and live guardian/yolo probes that cancelled one child while its sibling completed and parent replied. A fresh adapter replay restored the child transcript and resumed it as generation 2. Probe logs are local ignored artifacts under target/native-live-*.log; probes used target/native-probe-workspace and their own Codex test threads.

Rust validation initially exposed two schema fixture assumptions, event ordering before Connected, and missing mode metadata in the new native fixture. These are fixed. NO_COLOR=1 inherited from the tool environment invalidates 19 existing color assertions; final validation uses env -u NO_COLOR cargo test. Optional desktop workspace checks need system GLib/Pango libraries; required default-member tests and all-target clippy do not include that optional desktop package.

The published Codex dependency stays at 1.11.4 until a separate release: its existing native viewing works with this mj change, while the new cancellation and replay-generation fix require this fork build or its next release. Claude currently advertises no child stop control; Kimi exposes lifecycle without child transcripts or stop. Permission requests remain on the owning session with native-child attribution; child plan requests cannot change the parent policy.

Plan revised 2026-09-19 during implementation to record migration classification, real validation evidence, recovery discoveries, and the user's merge/push authorization.


Final evidence (2026-09-19): `env -u NO_COLOR cargo test` exited 0 (`target/native-final-tests.log`); all ACP tests passed (`target/native-acp-final.log`); the final mixed-ownership pane test passed (`target/native-navigation-final.log`); formatting and all-target clippy exited 0 (`target/native-final-clippy.log`). Additional regression coverage proves unused missing-thread recovery ends its staged replay before live replacement children arrive, and returning from a native child restores its managed owner's ancestor workspace. Both pushes succeeded. No package release, tag, installation, live-store migration, or restart of user sessions was performed.
