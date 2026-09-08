# Create multi-repository bundles in the session wizard

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

The session wizard advertises bundles but currently accepts only one repository. Users need to collect several local Git paths or GitHub sources, inspect and remove entries, then create one bundle. The first repository is the primary repository. Labels must consistently describe bundle creation, and the source field must not overlap its help text.

## Progress

- [x] (2026-09-08) Traced the TUI input, dashboard background operation, and persisted quick-bundle helper.
- [x] (2026-09-08) Settled the draft/list/add/remove/create design and assigned separate UI and backend ownership.
- [x] (2026-09-08) Implement atomic multi-source persistence and pass five focused backend behavior tests.
- [x] (2026-09-08) Finish editor behavior tests and integration review; remove redundant legacy input handling.
- [x] (2026-09-08) Pass formatting, diff checks, and `cargo clippy --all-targets -- -D warnings`.
- [x] (2026-09-08) Review integration and pass the full `cargo test` suite, including all new UI behavior tests.
- [x] (2026-09-08) Complete the implementation checkpoint for commit on the current branch.

## Surprises & Discoveries

The existing source field is drawn on the same row as the keyboard hint. The existing backend deliberately creates one repository per quick bundle; the missing multi-repository workflow is not merely a mislabeled button. Creation already runs in a supervised background operation in `mj-cli/src/dashboard/io.rs`.

## Decision Log

- Decision: Add repositories to an in-memory draft, then persist all sources together.
  Rationale: This supports inspection/removal and avoids saving partially assembled bundles. Filesystem validation stays off the UI loop.
  Date/Author: 2026-09-08, root.
- Decision: Keep the existing single-source quick-bundle API and add a multi-source API using shared source interpretation.
  Rationale: Web callers retain their existing behavior; terminal callers can submit a complete collection. IDs and destination directories must be unique when source basenames collide.
  Date/Author: 2026-09-08, root.

## Outcomes & Retrospective

The wizard now collects and removes repository sources and creates one bundle atomically in the existing background task. Labels consistently describe bundles, and the source field and keyboard help occupy separate rows. The draft survives failure and back navigation; pending state prevents repeated submits and stale wizard completion. Shared interpretation preserves the legacy quick API while the editor requires an exact unpinned repository set for reuse.

All five focused backend tests pass, including mixed local/GitHub sources and local path aliases. The full `cargo test` suite exits successfully, including 363 passing TUI tests with two existing ignored tests. Integrated coverage exercises multi-source submit, pending/success transitions, input/help separation, and mouse Add. Formatting, diff checks, and `cargo clippy --all-targets -- -D warnings` pass. No interactive production session was created during validation.

## Context and Orientation

`mj-tui/src/wizards.rs` defines and renders wizard drafts and controls. `mj-tui/src/wizards/dashboard.rs` handles keyboard/form transitions and creation results, with behavior tests in `mj-tui/src/wizards/tests.rs`. `DashboardAction` in `mj-tui/src/lib.rs` carries requests to `mj-cli/src/dashboard/actions.rs`. `mj-cli/src/dashboard/io.rs` runs creation in a tracked background task and delivers success/failure. `mj-controller/src/hel_controller.rs` parses sources and saves bundles using `HelConfig::update`, a serialized transaction that loads fresh configuration. A bundle contains repositories with unique IDs and relative destination paths and designates one ID as primary.

## Plan of Work

Implement a repository list and current-source field in the new-bundle draft. Enter in the source adds an entry and clears the field; an explicit Add repository button does the same. Removal operates on the selected entry. Create bundle submits all entries and any nonempty current field, preserves the draft on errors, and prevents duplicate submissions while creation is pending. Correct picker/button labels and allocate separate render rows for the prompt, field, hint, and list.

Change the dashboard action to `CreateBundle { sources: Vec<String> }` and pass the vector through the existing background helper. Add `create_bundle_from_sources(&[String])` and an in-config helper that share source parsing with quick creation. Validate all sources and the resulting candidate configuration before updating the supplied configuration. Preserve single-source reuse, reject duplicate canonical sources, and disambiguate colliding repository names.

## Concrete Steps

Work in `/home/jonathan/Projects/hel3`. Review changes using `git diff --check` and `git diff`. Run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Run Cargo tests outside the restricted sandbox as required by `AGENTS.md`; use existing build storage. Stage only the changed implementation and plan files, then commit on the existing branch without pushing.

## Milestones

The backend milestone is complete: source collection now creates one validated bundle, and five focused tests prove collision handling, duplicate rejection, atomic failure, exact reuse, and mixed local/GitHub sources. The editor milestone is complete: form events manage the repository draft and one background submission. The integration milestone is complete: full workspace tests and Clippy pass, and the change is ready for the required current-branch commit.

## Validation and Acceptance

Behavior tests must exercise adding two repositories, removal, creation, and pending/failure transitions through the advertised input controls. Rendering must expose New bundle, Add repository, and Create bundle without overlapping input/help. Backend tests must show one bundle with both repositories, first primary, unique destinations for equal basenames, no partial configuration mutation on invalid later sources, and preserved single-source reuse. Full tests and warning-free Clippy must pass.

Manual acceptance: open New session, select a target requiring a bundle, choose New bundle, add two GitHub owner/repository sources, and choose Create bundle. The resulting session review must reference the created bundle with both repositories configured. No Git/network/filesystem work occurs in the event handler.

## Idempotence and Recovery

Draft edits do not persist until Create bundle. Invalid sources leave the draft available for correction and do not partially save a bundle. Existing background tracking bounds shutdown behavior. Do not alter unrelated configuration or working-tree edits.

## Artifacts and Notes

Initial evidence: `WizardStep::NewBundle` emitted `DashboardAction::CreateBundle { source }`; `create_quick_bundle_in_config` constructed `repositories: vec![ProjectRepository { ... }]`.

## Interfaces and Dependencies

Reuse `Form`, `TextField`, `ChoiceList`, and `ButtonRow` from `mj-chat`, and `HelConfig::update` and `ProjectBundle` from the existing core. Introduce no crates. CLI completion resets the pending draft through `apply_created_bundle` or `fail_bundle_creation`.

Initial plan records the discovered UI/backend limitation and the agreed implementation contract.

Review update: the new API reuses only exact source sets with the same primary and no pinned Git ref, even for a single source. Legacy quick-bundle reuse remains unchanged. Source set matching and ID sanitization reuse existing import helpers. Pending persistence disables draft navigation to prevent late completions from advancing another wizard; global quit remains owned by the dashboard.

Completion update: recorded successful full-suite and static validation, plus the limitation that no production session was launched during testing.
