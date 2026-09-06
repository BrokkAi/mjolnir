# Remember pane sizes per workspace


This ExecPlan is maintained according to `.agents/PLANS.md`. Its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective sections are living records.

## Purpose / Big Picture


Each workspace should remember the requested size of the Sessions, Targets, and Quota panes across workspace switches and application or daemon restarts. The modes are Minimized, Standard, and Maximized. Only one pane may be maximized. Existing workspaces start with all panes Standard. Focus, selections, scroll positions, and live synchronization between clients are outside this change.

## Progress


- [x] (2026-09-06) Inspected pane transitions, workspace storage, daemon requests, and dashboard shutdown. User approved reuse of existing PaneSize and PaneSizes types.
- [x] (2026-09-06) Move existing data types into shared workspace module and expose dashboard capture/restore.
- [x] (2026-09-06) Implement migration 25, atomic workspace layout storage, and daemon save action; update schema rewind fixtures.
- [x] (2026-09-06) Integrate restoration and supervised, ordered background saves with bounded final flush.
- [x] (2026-09-06) Validate behavior and review integrated changes. Deliver the validated set in a commit on the existing hel4 branch without pushing.

## Surprises & Discoveries


Pane sizes already have a three-field value type in `mj-tui/src/lib.rs`; persistence does not require a second layout model. Workspace mutations use the daemon (the background controller process), which owns the database writer. The dashboard already tracks critical operations during shutdown, but that tracker does not itself impose a timeout. Pane persistence therefore needs its own bounded flush.

## Decision Log


On 2026-09-06 the user explicitly chose to retain the names PaneSize and PaneSizes and reuse their definitions. Move these definitions to the shared workspace module and re-export them from the TUI; keep SupportPane-specific access in the TUI. This avoids duplicate representations or a dependency from database code on the terminal UI.

On 2026-09-06 the implementation design selected a per-workspace database row and atomic complete-layout writes. The most recently completed save wins between different clients. A single client serializes saves and retains only the newest pending layout, preventing slow writes from restoring an older choice over a newer choice.

On 2026-09-06 the save coordinator was implemented in `mj-cli/src/dashboard/pane_sizes.rs` using a watch channel (a channel that retains only the newest value) and a supervised asynchronous task. Its final five-second flush runs after releasing the terminal rather than extending the existing unbounded critical-operation tracker. The daemon metadata read in connect_existing now uses a blocking background task so the saver never performs filesystem I/O on the UI runtime thread. Save requests use the existing daemon; connection failures are reported instead of launching processes from the save path.

## Outcomes & Retrospective


Implementation and integrated review are complete. Each workspace now restores its three requested modes before the first draw and saves changes through a supervised, ordered background task. An isolated real-terminal check passed for different layouts in two workspaces, workspace switching, immediate Alt-G/Alt-Z followed by quit, application restart, and daemon restart. All groups in the full `cargo test --no-fail-fast` run passed, as did `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check`.

An initial compile caught a missing oneshot channel type annotation in a new cancellation test; this was corrected. The first full suite stopped at the unchanged worker test obsolete_install_waits_for_the_last_shared_lease; that test passed in isolation and in the final full run. No unrelated worker changes were needed. Shared PaneSize and PaneSizes definitions avoided duplicate persistence types. The main integration concern was preserving ordering without extending the UI's shutdown wait indefinitely; a watch channel and a five-second final flush resolved that concern.

## Context and Orientation


`src/hel_workspace.rs` defines durable workspace identities and now exports PaneSize and PaneSizes. `src/hel_database.rs` owns database access and its `schema.rs` submodule applies numbered SQLite migrations; this change advances schema 24 to 25. `mj-tui/src/lib.rs` re-exports the existing type names and owns DashboardState transitions for mouse controls, Alt-Z, and Alt-G. `mj-cli/src/daemon.rs` defines the serialized request protocol and calls the database writer. `mj-cli/src/dashboard.rs` creates the terminal dashboard, routes input, and handles shutdown. Its `pane_sizes.rs` submodule supervises layout saves; `io.rs` provides the other background jobs and result reporting. Tests live alongside these modules, including the database's existing tests submodule.

## Plan of Work


Milestone 1 makes the existing PaneSize and PaneSizes reusable for persistence. Move data definitions to hel_workspace, derive serde serialization, expose the three fields, and validate that no more than one field is Maximized. Re-export from hel_tui so current callers retain their interface. Add DashboardState methods to capture and restore the complete layout; restoration validates before changing state and ignores content-dependent maximize availability. Colocated behavior tests must prove restoration and existing transitions.

Milestone 2 makes workspace layouts durable. Add migration 25 to create workspace_pane_sizes, with workspace_id as a foreign key that cascades deletion, three checked mode columns, and a constraint preventing multiple maxima. Add reader and writer functions, including path-specific test entry points, defaulting to Standard only for an existing workspace with no settings. Write all three fields in one upsert. Add a daemon save request/client method/handler and bump its protocol version to ensure an older daemon is replaced by the existing compatibility workflow. Test migration, reopen, independent workspaces, rename, deletion, invalid settings, and unknown workspace rejection.

Milestone 3 connects the UI. Load the layout before entering the terminal and restore it before the first draw. Capture actual input changes, including presets and changes that return to Standard. Submit saves without performing I/O on the event/render loop. A supervised background save coordinator serializes requests and coalesces pending layouts, reports failures, and does not save merely because a workspace was opened. On switch or shutdown, flush the latest layout with a five-second maximum wait; report final failure after terminal teardown. Keep unrelated operations independent. Test delayed saves, newest-layout ordering, failures, and bounded flushing using hand-written fakes.

## Concrete Steps


Work in `/home/jonathan/Projects/hel4`. After edits, run:

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings

Run every cargo test outside the restricted sandbox because tests use sockets. Use normal build storage, never /tmp. Inspect the final diff, stage only changed task files, and commit directly to the current branch. Do not push.

## Validation and Acceptance


All required checks should exit successfully. New behavior tests must show that opening the database and dashboard again restores the exact three saved modes; changing workspace names does not change ownership; deleting a workspace removes its settings; and rapid input with an in-flight save ultimately stores the newest layout. Failure tests must show that saving errors reach notices and a stalled final save cannot block indefinitely.

For a manual terminal check, open two workspaces with `mj`, use pane controls or Alt-Z and Alt-G to assign different layouts, switch between the workspaces, quit and restart, and restart the daemon before reopening. Each workspace must retain its own modes. If a real interactive check is unavailable, record that limitation and use integrated automated persistence/input tests rather than claiming manual coverage.

## Idempotence and Recovery


The new schema change is additive and transactional. Upserts can be repeated safely. A missing settings row means Standard defaults; malformed data or read errors are reported instead of hidden. A failed save preserves the previous durable layout and is reported. Keep only pending user choices in memory and flush on exit within the timeout; never report success for an unacknowledged save.

## Artifacts and Notes


The type/TUI work and database tests were completed by a Luna agent with exclusive file ownership. The primary agent implemented database storage and migration, daemon requests, background persistence, and integration, and reviewed the actual changes.

The isolated terminal smoke check used `/tmp/hel4-pane-smoke.exp`, the freshly built target/debug/mj, and temporary MJ_CONFIG_DIR/MJ_DATA_DIR overrides. It created two workspaces through the picker, drove Alt-G and Alt-Z, switched using F3, checked durable rows with SQLite, restarted the daemon, and used subsequent size transitions to prove that the reopened UI had restored the saved modes. It stopped the isolated daemon before deleting its storage. Output:

    PASS: isolated pane layouts survive workspace switches, immediate quit, app restart, and daemon restart.

The first completed suite log is target/workspace-pane-tests.log; the full rerun is target/workspace-pane-tests-final.log. The worker cleanup test passed separately:

    cargo test -p brokk-mj-worker --lib obsolete_install_waits_for_the_last_shared_lease
    test result: ok. 1 passed; 0 failed

Final validation completed successfully:

    cargo test --no-fail-fast                         # exit 0; every test group passed
    cargo clippy --all-targets -- -D warnings         # exit 0; target/workspace-pane-clippy.log
    cargo fmt --all -- --check                       # exit 0
    git diff --check                                # exit 0

The six save-coordinator tests cover no write on open/close, rapid changes and final flush ordering, retry after failure, persistent final failure, timeout cancellation, and panic supervision. Storage and TUI tests cover restoration, migration, default behavior, workspace ownership, and validation. The delivery commit message is `Remember pane sizes per workspace` on the existing hel4 branch; no push is authorized.

## Interfaces and Dependencies


The shared module exports PaneSize and PaneSizes, using existing serde and anyhow libraries. PaneSizes has sessions, targets, and quota fields, Standard defaults, and validate() returning anyhow::Result<()>. DashboardState exposes pane_sizes() -> PaneSizes and restore_pane_sizes(PaneSizes) -> anyhow::Result<()>. The TUI adds a direct dependency on the already-used anyhow library to name that result type. Database entry points load_workspace_pane_sizes(workspace_id) and save_workspace_pane_sizes(workspace_id, sizes) read and atomically write this same type. The daemon exposes a SaveWorkspacePaneSizes action, returning Done only after the database save succeeds, under protocol 9. No workspace crate or new external library is added.

Revision note: Final update records passing full-suite, Clippy, formatting, and terminal checks, the completed implementation, and delivery instructions for the current-branch commit.
