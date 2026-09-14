# Choose native or Mjolnir sub-agents per session

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept current as implementation proceeds. Maintain it in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture


Mjolnir can already replace a harness's built-in delegation with its own "sub-agents" (child Mjolnir sessions that share the parent's target and files; see `.agents/plans/mjolnir-subagent-toolset.md`). Today that replacement is all-or-nothing: a global `[subagents] enabled` setting turns Mjolnir sub-agents on for every Claude and Codex session, and there is no way to say "this session should use Claude's own `Agent` tool instead".

After this change, the new-session wizard in both the terminal UI and the web UI shows a checkbox labelled `Use Mjolnir sub-agents` whenever the selected profile is Claude or Codex. It defaults to the global setting. Leaving it checked gives the session Mjolnir's MCP delegation tools and hides the harness's native delegation tools. Unchecking it leaves the harness's native delegation tools alone and injects nothing. The choice is stored on the session so resume, reconnect, and daemon restart keep it. The HTTP API accepts the same choice when creating a session.

This change also fixes a defect in the current code: the worker hides the native delegation tools unconditionally, so turning the global setting off today produces a session with neither native nor Mjolnir sub-agents.

## Progress


- [x] (2026-09-14) Milestone 1: session record, create request, API request, database column and migration 33.
- [x] (2026-09-14) Milestone 2: controller resolves the per-session value; worker keys native suppression on the MCP socket.
- [x] (2026-09-14) Milestone 3: terminal new-session wizard checkbox.
- [ ] Milestone 4: web new-session wizard checkbox.
- [ ] Validation: `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `git diff --check`, web unit and Playwright tests; commit.

## Surprises & Discoveries


- Observation: `mj-tui/src/dialogs.rs` has no second new-session creation path. The `create_managed_worktree` references there belong to `ImportBundleConfirmation`, the dialog that confirms importing an existing native session, plus three `DashboardAction::CreateSession` literals inside that file's own tests. The wizard in `mj-tui/src/wizards/` is the only surface that builds a new-session request in the terminal, so the checkbox lives there alone.
  Evidence: `mj-tui/src/dialogs.rs:346` is `ImportBundleConfirmation::create_managed_worktree`; `dialogs.rs:2575`, `:2655`, and `:3853` are all inside `#[cfg(test)] mod tests`.
- Observation: resume does not build its launch configuration on a different path. `mj-controller/src/controller/worker_binary.rs::prepare_worker_files` has three callers -- `controller/provisioning.rs:109`, `controller/provisioning.rs:538`, and `controller/resume.rs:1165` -- so the one expression covers first launch, worker payload install, and resume.
  Evidence: `grep -rn prepare_worker_files mj-controller/src`.
- Observation: the compatibility floor is already 32, not 30, because migration 32 (ZCode harness constraints) was breaking. Three tests in `mj-controller/src/database/schema.rs` asserted `minimum_compatible == Some(SCHEMA_VERSION)`, which held only by coincidence while the newest migration was the breaking one. Milestone 1 introduced a `MINIMUM_COMPATIBLE_VERSION` constant in that file's `reader_tests` module and pinned the two real assertions to it, so a future compatible migration no longer breaks the test.
  Evidence: `assertion \`left == right\` failed; left: Some(32); right: Some(33)` at `mj-controller/src/database/schema.rs:1758`.
- Observation: every migration-downgrade fixture that rewinds `PRAGMA user_version` below 33 must also drop the new column, or migration 33 re-runs and fails with `duplicate column name: mjolnir_subagents`. Nine fixtures in `mj-controller/src/database/tests.rs` and one in `schema.rs` needed the extra `ALTER TABLE sessions DROP COLUMN mjolnir_subagents;`.
  Evidence: `called \`Result::unwrap()\` on an \`Err\` value: duplicate column name: mjolnir_subagents`.
- Observation: `cargo check --workspace` fails in this environment because the non-default member `mj-desktop` needs GTK system libraries. Plain `cargo check` / `cargo test` use `default-members`, which excludes it, so use the plain commands.
  Evidence: `error: failed to run custom build command for \`glib-sys v0.18.1\``; `Cargo.toml:7` lists `default-members` without `mj-desktop`.
- Observation: `controller::update::tests::npm_upgrade_restarts_after_the_running_package_is_removed` fails intermittently under the full parallel suite and passes in isolation. It is unrelated to this change.
  Evidence: `cargo test -p brokk-mj-controller --lib npm_upgrade_restarts_after_the_running_package_is_removed` -> `1 passed`.
- Observation: the worker's `session_request_meta` in `mj-worker/src/acp.rs` adds `disallowedTools` for Claude and Codex regardless of whether Mjolnir's MCP tools are injected. The launch flag `subagent_tools` never reaches `LaunchSpec`.
  Evidence: `grep -rn subagent_tools mj-worker/src` shows only `worker_runtime/unix.rs:305`, which uses it to decide whether to start the MCP socket, and `session_request_meta` has no conditional around the `disallowedTools` insertions.

## Decision Log


- Decision: store the choice as `Option<bool>` named `mjolnir_subagents` on `SessionRecord`, `CreateSessionRequest`, and the HTTP create request. `None` means "use the global `[subagents] enabled` setting at launch time"; `Some(true)` and `Some(false)` are explicit.
  Rationale: this is exactly the shape and semantics of the existing `create_managed_worktree` field, so every layer already has a worked example to copy. Resolving `None` at launch time rather than at creation means sessions created by older code or by non-wizard callers follow the global setting as they do today.
  Date/Author: 2026-09-14, Fable.
- Decision: the worker decides whether to hide native delegation tools by checking whether it was given a Mjolnir sub-agent MCP socket (`LaunchSpec.subagent_mcp_socket.is_some()`). No new worker field.
  Rationale: the socket already exists only when the controller asked for Mjolnir sub-agents, so it is the single source of truth inside the worker, and the two halves (inject MCP, hide native) can never disagree again.
  Date/Author: 2026-09-14, Fable.
- Decision: database migration 33 is compatible. It adds one nullable column `mjolnir_subagents INTEGER CHECK(mjolnir_subagents IN (0, 1))` to `sessions` and does not raise the minimum compatible revision.
  Rationale: older readers ignore the column. The older writer's session upsert in `mj-controller/src/database.rs` lists columns explicitly and its `ON CONFLICT ... DO UPDATE SET` touches only those columns, so an older binary updating a session preserves the new column's value. An older executable launching such a session falls back to the global setting, which is a behaviour difference, not data loss or corruption. This matches the classification used for revision 29 (`create_managed_worktree`).
  Date/Author: 2026-09-14, Fable.
- Decision: imported sessions keep `mjolnir_subagents: None` and no import-dialog checkbox. The plan expected a second creation path in `mj-tui/src/dialogs.rs`, but that file's dialog confirms a session *import*, not a creation. `None` there preserves today's behaviour exactly, and adding a control to the import dialog was not requested.
  Rationale: match the existing behaviour rather than invent a new control the user did not ask for.
  Date/Author: 2026-09-14, Fable.
- Decision: the checkbox appears only when the selected profile's harness kind is Claude or Codex. For any other kind the wizard sends `None`.
  Rationale: only those two harnesses receive Mjolnir sub-agent tools, so the checkbox would be meaningless elsewhere.
  Date/Author: 2026-09-14, user.
- Decision: keep the global `[subagents] enabled` setting. It becomes the default for the checkbox and the fallback for sessions with `None`.
  Rationale: the user asked for a per-session control, not for removal of the global one.
  Date/Author: 2026-09-14, user.

## Outcomes & Retrospective


To be written when the work is complete.

## Context and Orientation


Mjolnir is a Rust workspace. The crates that matter here, in dependency order, are:

`mj-core` holds shared types. `mj-core/src/state.rs` defines `SessionRecord`, the durable description of one session (profile, target, workspace, and per-session options such as `create_managed_worktree: Option<bool>`). `mj-core/src/config.rs` defines `SubagentConfig` with the global `enabled` flag. `mj-core/src/worker_launch.rs` defines the launch configuration handed to the worker process; it already carries `pub subagent_tools: bool`.

`mj-client/src/daemon.rs` defines the request types a client sends to the daemon, including `CreateSessionRequest`, which mirrors the session record's creation-time fields.

`mj-controller` is the daemon. `mj-controller/src/controller/worker_binary.rs` builds the worker launch configuration; at about line 101 it sets `launch.subagent_tools` from the global config, the harness kind, and whether the session is itself a child. At about line 184 it stages the Claude MCP configuration when `launch.subagent_tools` is set. `mj-controller/src/controller/subagents.rs` implements spawning children and at line 37 checks the global flag. `mj-controller/src/database.rs` and `mj-controller/src/database/schema.rs` persist `SessionRecord` as explicit SQLite columns; the migration for revision 29 (search for `create_managed_worktree` in `schema.rs`) is the pattern to copy, and `SCHEMA_VERSION` lives at `mj-controller/src/database.rs:35`. `mj-controller/src/server/api.rs` defines the HTTP session-create request (its struct with `create_managed_worktree: Option<bool>` near line 279) and maps it into the daemon request near line 1344.

`mj-worker` runs on the target. `mj-worker/src/worker_runtime/unix.rs` starts the Mjolnir sub-agent MCP socket when `config.subagent_tools` is true (line 305) and passes it as `subagent_mcp_socket` into `LaunchSpec` (line 361). `mj-worker/src/acp.rs` builds the ACP session request; `session_request_meta` (around line 155) inserts `disallowedTools` for Codex (`["spawn_agent"]`) and Claude (`["Agent", "Task", "TaskOutput", "TaskStop"]`), and `LaunchSpec` (line 107) has `subagent_mcp_socket: Option<PathBuf>` (line 120).

`mj-tui` is the terminal UI. The new-session wizard is `mj-tui/src/wizards/dashboard.rs`. Search for `create_managed_worktree` there: line 488 toggles it on a key press, line 1370 sends it in the create request, and line 1775 sets its default when the wizard learns the worktree options. `mj-tui/src/dialogs.rs` has a second, dialog-style creation path (search `create_managed_worktree` there too, lines 346, 2122, 2294, 2311) that must gain the same field. The wizard tests are in `mj-tui/src/wizards/tests.rs`.

The web UI is a single page at `mj-controller/src/web/viewer.js`, `viewer.html`, and `viewer.css`. The new-session draft object is defined near `viewer.js:1188` (`createManagedWorktree: false`), the review step renders its checkbox near line 1374, and the create payload is built near line 1578. Web unit tests and Playwright tests live under `tests/e2e/web`.

"Native delegation tools" means the harness's own built-in way of starting sub-agents: Claude's `Agent`/`Task` tools and Codex's `spawn_agent`. "Mjolnir sub-agents" means child Mjolnir sessions started through the MCP server the worker injects.

## Plan of Work


Milestone 1 adds the data. Add `pub mjolnir_subagents: Option<bool>` with `#[serde(default, skip_serializing_if = "Option::is_none")]` to `SessionRecord` beside `create_managed_worktree`, and a plain `#[serde(default)] pub mjolnir_subagents: Option<bool>` to `CreateSessionRequest` and to the HTTP create request struct in `server/api.rs`, mapping it through where `create_managed_worktree` is mapped. Every struct literal that constructs a `SessionRecord` or `CreateSessionRequest` (tests, test support, provisioning, subagents.rs, resume.rs, backend.rs) needs the new field; the compiler will list them. Children created in `subagents.rs` must get `Some(false)` so a child never receives the tools. Add migration 33 in `schema.rs` copying the revision 29 block: `ALTER TABLE sessions ADD COLUMN mjolnir_subagents INTEGER CHECK(mjolnir_subagents IN (0, 1))`, ledger insert, `PRAGMA user_version = 33`, all in one `BEGIN IMMEDIATE ... COMMIT`. Do not touch the compatibility floor. Bump `SCHEMA_VERSION` to 33. Add the column to the session `SELECT`, row mapping, and upsert in `database.rs` exactly as `create_managed_worktree` is handled. Add a short comment above the migration classifying it as compatible with the reason from the Decision Log. Extend the existing isolated-store migration tests in `mj-controller/src/database/tests.rs` (see the `DROP COLUMN create_managed_worktree` pattern near lines 731 and 1183) so a store at revision 32 migrates to 33 and a record with `Some(false)` survives a round trip.

Milestone 2 makes the value effective. In `worker_binary.rs` replace `self.config.subagents.enabled` in the `launch.subagent_tools` expression with `session.mjolnir_subagents.unwrap_or(self.config.subagents.enabled)`. Confirm this function is on the path for both first launch and resume; if resume builds its launch elsewhere, apply the same expression there. In `subagents.rs` line 37, additionally require that the parent session's resolved value is true, with an error such as `this session uses native sub-agents`. In the worker, wrap both `disallowedTools` insertions in `session_request_meta` with `if spec.subagent_mcp_socket.is_some()`. Add a unit test in `mj-worker/src/acp/tests.rs` proving that a Claude spec without a socket produces no `disallowedTools` and one with a socket does; do the same for Codex. Add a controller test proving `subagent_tools` is false for a Claude session with `Some(false)` even when the global setting is on, and true for `None` when the global setting is on.

Milestone 3 adds the terminal checkbox. In `mj-tui/src/wizards/dashboard.rs` add a `mjolnir_subagents: bool` wizard field defaulting to the global `config.subagents.enabled`. Render a checkbox row `Use Mjolnir sub-agents` on the same review step that shows the managed-worktree checkbox, only when the selected profile's kind is Claude or Codex, with a one-line help text such as `Unchecked keeps the harness's own Agent or spawn_agent tools.` Toggle it with the same key and mouse handling used for the worktree checkbox. When building the create request, send `Some(wizard.mjolnir_subagents)` for Claude/Codex and `None` otherwise. Do the same in the dialog-style creation path in `mj-tui/src/dialogs.rs`. Add wizard tests in `mj-tui/src/wizards/tests.rs` in the existing descriptive style, for example `new_session_wizard_shows_subagent_checkbox_only_for_claude_and_codex` and `new_session_wizard_sends_subagent_choice`.

Milestone 4 adds the web checkbox. In `viewer.js` add `mjolnirSubagents` to the new-session draft, defaulting from the global setting. The page needs to know the global default and each profile's harness kind; check what the profiles endpoint already returns and, if the global default is not exposed, add it to the existing configuration or profiles response rather than a new endpoint. Render the checkbox next to the managed-worktree checkbox on the review step, only for Claude/Codex profiles, with an accessible label. Send `mjolnir_subagents` in the create payload. Add a unit test and extend the existing Playwright new-session test so it checks the box is present for a Claude profile and absent for another kind.

## Concrete Steps


Work in `/home/jonathan/Projects/hel3` on branch `hel3`. Commit each milestone once it builds and its tests pass, staging only the files you changed. Never run `git add -A`.

Run Rust checks outside the restricted sandbox:

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings
    git diff --check

Run web checks:

    npm --prefix tests/e2e/web test:unit
    npm --prefix tests/e2e/web test

## Validation and Acceptance


Automated tests prove: an old database migrates to revision 33 and preserves an explicit `false`; the controller sets `subagent_tools` from the session value with the global setting as fallback; the worker omits `disallowedTools` when no MCP socket is present and includes it when one is; both wizards show the checkbox only for Claude and Codex and send the choice; children are always created with `Some(false)`.

Manual acceptance: start the daemon and TUI, open the new-session wizard with a Claude profile, and see `Use Mjolnir sub-agents` checked by default on the review step. Uncheck it and create the session. In the session, ask Claude to list its tools; `Agent` appears and no `mj-subagents` MCP tools appear, and the prompt border shows no Sub-agents control. Create a second session with the box checked; `Agent` is absent and the `mj-subagents` tools are present. Repeat with a Codex profile. Select a Grok or Kimi profile and confirm the checkbox is absent. Do the same in the web UI.

## Idempotence and Recovery


Migration 33 is additive and guarded by `if version < 33`. Re-running it is a no-op. A session with `None` behaves exactly as before this change. Nothing here changes target provisioning or cleanup.

## Interfaces and Dependencies


No new crates. New field `mjolnir_subagents: Option<bool>` on `SessionRecord`, `CreateSessionRequest`, and the HTTP create request. No change to `LaunchSpec`; the worker keys on `subagent_mcp_socket`. Schema revision 33, compatible.

Revision 2026-09-14: created from the confirmed design after finding that the 2026-09-13 implementation added only a global toggle and that native suppression was unconditional.
