# Derive session checkout ownership once

This ExecPlan is maintained under `.agents/PLANS.md`. It records the behaviour-preserving derived-checkout spike requested for this workspace.

## Purpose / Big Picture

Session records currently describe checkout location and ownership through several optional fields. Controller operations independently interpret those fields, which lets provisioning, checkpointing, cleanup, resume, and review disagree about whether a directory belongs to the user, Mjolnir, or a parent session. The spike adds one borrowed, computed checkout view and moves ownership-sensitive readers to it without changing persisted records or user-visible behavior.

## Progress

- [x] (2026-10-06 16:17Z) Read the supplied divergence map and identify raw attached paths, managed worktrees/clones, bundle workspaces, and borrowed child sessions.
- [x] (2026-10-06 16:17Z) Add `State::checkout`, the session-local checkout derivation, and focused shape coverage in `mj-core`; the focused test passed.
- [x] (2026-10-06 16:41Z) Move lifecycle, checkpoint, provisioning, resume/move, project-source, storage, publication, SessionWiki, doctor, report-directory, and dashboard ownership/location readers to the derived checkout. Leave field writes and DB serialization in place; direct display/projection formats are being audited.
- [x] (2026-10-06 17:03Z) Close nested-child archival and orphan-child lifecycle compatibility gaps; add projected-record checkout coverage and pass its focused test.
- [x] (2026-10-06 17:33Z) Final touched-crate suites, timezone goldens, workspace Clippy, formatting, and direct-read audit pass; report evidence without committing.

## Surprises & Discoveries

- A missing `project_directory` and missing `managed_worktree` currently means a target workspace checkout even when its bundle came from a saved project snapshot rather than current config.
- A non-Git directory on a bare target is a supported attached checkout; a Git project snapshot is not required.
- A managed child stores no managed worktree of its own and copies its parent's `project_directory`; its ownership must come from the durable sub-agent relationship.
- Child relations can be nested in existing SessionWiki records. `State::checkout` therefore keeps a chain of `Borrowed` values until it reaches the original owner; cycles fail explicitly. If a legacy orphan child has no parent row, its stored checkout shape is used so cleanup retains the prior behavior.
- `State::is_subagent_session` also recognizes native harness child view IDs, but those are projections rather than durable `SessionRecord`s and have no independently addressable checkout record.
- Invalid legacy data can contain a managed-worktree descriptor and an unrelated `project_directory`; normal record validation rejects the mismatch. The total derivation gives the descriptor cleanup authority and retains the stored path for location consumers, with an explicit derivation test.
- A workspace-wide `cargo check` cannot reach Rust checks because this container lacks `javascriptcoregtk-4.1` and `gdk-3.0`; use touched library packages for compile feedback, while preserving the failed log.

## Decision Log

- Decision: put the only field-to-checkout mapping in `derive_record_checkout`, reached through `SessionRecord::checkout`, and resolve durable ownership in `State::checkout` / `State::checkout_for_record`.
  Rationale: projected records need the same parent relation as stored records; a record alone cannot distinguish an attached raw directory from a child borrowing that same directory.
  Date/Author: 2026-10-06 / Codex sub-agent.
- Decision: classify a record with both `managed_worktree` and `project_directory` as managed raw; retain the stored project path for the actual repository directory. If a legacy/incomplete managed record lacks the path, keep managed ownership and expose the path as absent.
  Rationale: `managed_worktree` is the cleanup authority; `project_directory` may point below `worktree_root` for a repository in a subdirectory.
  Date/Author: 2026-10-06 / Codex sub-agent.
- Decision: records with neither location field map to a managed bundle workspace regardless of whether `project` contains a saved bundle snapshot.
  Rationale: bundle source selection and checkout ownership are independent; this matches current provisioning and checkpoint behavior.
  Date/Author: 2026-10-06 / Codex sub-agent.
- Decision: a borrowed child exposes its parent's effective path but has no session-owned managed worktree. Resume planning continues to interpret the child's copied path as the old raw-session record did, while cleanup and retirement inspect the outer `Borrowed`/owned variant and never retire the parent's checkout.
  Rationale: this keeps existing path planning behavior while enforcing the new lifetime owner.
  Date/Author: 2026-10-06 / Codex sub-agent.
- Decision: nested children produce nested `Borrowed` values; an orphan child with a missing parent record falls back to its own persisted checkout fields.
  Rationale: nested child archival already exists, and orphan child close/destroy paths must remain operable after the owner record is gone.
  Date/Author: 2026-10-06 / Codex sub-agent.

## Outcomes & Retrospective

The derived checkout is implemented in `mj-core/src/state.rs`; ownership-sensitive consumers use `State::checkout` or `State::checkout_for_record` when their state snapshot is available. Existing behavior tests pass unchanged. The new core shape test covers raw local/SSH attached directories, managed worktree and clone, configured/saved/legacy bundle workspaces, incomplete and inconsistent legacy rows, and a child of each ownership kind. Nested child and missing-parent legacy cases are covered by existing controller tests.

Final evidence: `cargo test -p brokk-mj-core` (455 unit and 2 scenario tests), `cargo test -p brokk-mj-client` (37 unit and 1 integration test), `cargo test -p brokk-mj-controller` (1878 pass, 10 ignored), `TZ=America/Chicago cargo test -p brokk-mj-tui` (521 pass, 2 ignored), `cargo test -p brokk-mjolnir`, `TZ=America/Chicago cargo test -p brokk-mj-chat`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check`. Full logs and direct-read reasons are under the assigned `.mj-agents` report directory. A separate workspace `cargo check` attempt stopped before project crates because GTK/WebKit development packages are absent; the required workspace Clippy succeeded.

## Context and Orientation

`mj-core/src/state.rs` defines `SessionRecord`, `ManagedWorktree`, and `State`; `State.subagents` maps a child session ID to its parent. `project_directory` is the active repository directory for a raw session, `managed_worktree` is the durable description of a raw checkout Mjolnir created and may retire, and neither field is set for an ordinary bundle workspace. `create_managed_worktree` is a launch-time choice and is not evidence of current ownership. The supplied read-only map is `/workspace/1872507e4d7eeafd43b66594471ab0a3/.mj-agents/8d8b5fcdc58f4c48f788800d32fc4fe0/divergence-map.md`.

The checkout view is derived at read time and borrows its path and managed-worktree metadata from records. Its ownership variants are attached/user-owned, managed raw worktree or clone, managed bundle workspace, and borrowed from a parent; a borrowed view also exposes the parent's effective checkout. No serialized field, database column, API shape, or creation-time writer changes.

## Plan of Work

Add the checkout enum and `State::checkout` in `mj-core/src/state.rs`. Derive borrowed children before reading their copied path fields; otherwise a child looks like an attached directory. Test local and SSH bare attached paths, non-Git and saved-snapshot raw shapes, managed worktrees and clones, bundle workspaces, missing legacy fields, and children borrowing each parent checkout kind.

In `mj-controller`, migrate checkpoint layout, provisioning selection, stop/destroy cleanup, resume and move compatibility/previews, review/API diff-base selection, report-directory placement, target preflight, and import/catalog paths that interpret current checkout ownership. Keep creation and conversion assignments, serialization/database mappings, and display-only projections unchanged. Search all Rust sources afterward and document each direct read that remains.

## Milestones

The first milestone is the core derivation: `State::checkout` resolves a session's record and its durable sub-agent parent, then returns a borrowed ownership variant. Its acceptance evidence is the colocated record-shape test, including non-Git and SSH bare directories, both managed checkout kinds, bundle and saved-project workspaces, incomplete legacy records, and borrowed children.

The second milestone migrates controller and viewer readers by area. Each consumer gets the same derived value for provisioning, checkpoint layout, cleanup, resume/move planning, project-source selection, report and storage paths, publication, archival, and preflight. The existing field assignments remain the persistence boundary. Focused package tests and compilation expose signature or lifetime mistakes before final validation.

The final milestone checks all touched crate suites, workspace Clippy, and formatting. The acceptance review also searches every Rust source for direct field reads and records any remaining writer, database, display, or compatibility projection with its reason. The full workspace check may fail before reaching project code when GTK development libraries are absent; the controller and other available touched packages remain the useful compilation boundary in that case.

## Concrete Steps

From the repository root (`/workspace/1872507e4d7eeafd43b66594471ab0a3/hel`), run focused checks for each changed crate area while implementing. At the end run:

    cargo test -p brokk-mj-core
    cargo test -p brokk-mj-controller
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

Run Cargo validation in the dev profile and with a named isolated instance for any CLI, daemon, TUI, or end-to-end invocation. The expected result is that all touched crate tests, workspace Clippy, and formatting succeed. If the documented environmental golden failures occur, record the exact test and log and rerun only that failing test with the documented timezone or checkout-kind condition corrected.

## Validation and Acceptance

The core derivation tests must distinguish attached paths from managed raw checkouts and bundle workspaces, and must show that a child of each parent kind is `Borrowed` while resolving to that parent's effective location. Existing controller tests must preserve checkpoint archive layout, cleanup boundaries, resume/move plans, review diffs, report paths, and preflight results. Final `rg 'project_directory|managed_worktree'` review must show no ownership-sensitive reader outside the derivation.

## Idempotence and Recovery

All edits are source changes and can be repeated safely. No migration, persisted format change, or external resource operation is part of this spike. Do not commit or push; the parent agent owns acceptance and any later commit.

## Artifacts and Notes

The implementation report and test logs are written under `/workspace/1872507e4d7eeafd43b66594471ab0a3/.mj-agents/a84af5a9a4ef917232a86b140c8576c2/`.

## Interfaces and Dependencies

The derived value belongs in `mj-core/src/state.rs` and is exposed as `State::checkout(&self, session_id: &str) -> anyhow::Result<Checkout<'_>>`. It must borrow `&Path` and `&ManagedWorktree` from the records, ignore `create_managed_worktree`, and retain the parent relationship for borrowed children. Controller consumers depend on this core API; it must not require config or target details to decide directory ownership.
