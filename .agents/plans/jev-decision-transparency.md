# Keep Jev diagnostics in logs

This living ExecPlan follows `.agents/PLANS.md`. The 2026-09-21 user correction supersedes the inspector design implemented in 7e82fae6.

## Purpose / Big Picture

Keep classifier inputs, assessments, and actual actions available in rotating local logs without Jev controls or attribution in the terminal or web interface. A running turn classified as awaiting input shows exactly: “Classifier: The agent appears to be waiting for you. The harness may still be running.”

## Progress

- [x] (2026-09-21) Removed terminal/web inspectors, status attribution, entry points, and their remote read interfaces.
- [x] (2026-09-21) Preserved activity/continuation diagnostics and restored the simple automatic-continuation notice.
- [x] (2026-09-21) Changed the awaiting-input message to a plain runtime notice and updated documentation and behavior tests.
- [x] (2026-09-21) Full Rust suite, strict Clippy, formatting, exact-notice regression, 43 web unit tests, and affected browser menu test passed; diff reviewed. Commit is the final step.

## Surprises & Discoveries

The old message used a warning event, whose transcript projection adds “warning: ”. Changing only its text would not satisfy the requested display. The existing durable Notice observation already renders plain text; a runtime Notice event now feeds that observation.

A later browser test expected the added Jev menu item and focused it after removing Rename. Its expected first available action must return to Cancel operation.

## Decision Log

2026-09-21: Remove the complete inspector and its API plumbing, retaining the dedicated four-segment, 8 MiB diagnostic logs. This follows the user's explicit correction that diagnostics belong in logs. Preserve classifier thresholds, evidence, cancellation, and accepted/rejected action recording.

2026-09-21: Keep already-published protocol revision numbers rather than reusing older revision identities. No database or durable observation schema changes are needed: plain Notice observations already exist.

## Outcomes & Retrospective

Implementation and validation are complete. The original transparency change exposed substantially more UI than the user wanted. This correction retains its diagnostic value with a simpler user-facing message.

## Context and Orientation

`mj-core/src/jev.rs` owns rotating JSON-line diagnostics. `mj-worker/src/acp/verdict_client.rs` records activity checks, and `mj-controller/src/daemon/continuation.rs` records continuation checks and worker acceptance. These remain intact. The removed inspector spanned mj-tui, mj-cli dashboard tasks, mj-chat click handling, mj-client transport, controller APIs, and worker read requests.

`mj-core/src/acp.rs` defines runtime events emitted by the agent bridge. `mj-worker/src/worker_runtime/unix/dispatch.rs` converts these into durable observations. `mj-transcript/src/projection/observation.rs` renders a Notice as plain system text, unlike a Warning.

## Plan of Work

Reverse only the UI and read-interface changes from 7e82fae6, preserving subsequent unrelated work. Remove the decision IDs propagated solely for UI attribution, while preserving the log handle in TurnContext. Restore the automatic-continuation copy to “Continuing requested work automatically · 1 of 3”. Emit the exact requested classifier message as a Notice. Update the existing quiet-prompt behavior test to require that event and exact text, and verify its transcript projection has no warning prefix.

## Concrete Steps

Work in `/home/jonathan/Projects/hel2`. Use the installed Cargo binary `/home/jonathan/.rustup/toolchains/1.96.0-x86_64-unknown-linux-gnu/bin/cargo` to avoid unrelated wrapper cache locks. Run `cargo test` outside the sandbox, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check`. In `tests/e2e/web`, run `node --test *.unit.test.mjs` and the affected deterministic compact-card browser tests. Review and commit only this task's changes on the current branch.

## Validation and Acceptance

No Jev decisions menu, modal, status label, transcript link, or inspection API remains. Existing log tests continue to prove exact submitted input, rotation, cancellation, and actual outcomes. The quiet-prompt test proves the exact classifier notice precedes awaiting-input completion without closing the harness. A projection test proves the displayed message lacks a warning prefix. Web tests prove ordinary session actions and focus behavior still work.

## Idempotence and Recovery

No live daemon, release, or store upgrade is needed. Unit tests use their existing isolated directories. Any manual new-build invocation must use a named test instance and isolated MJ_CONFIG_DIR/MJ_DATA_DIR. No push is required for this correction unless requested.

## Artifacts and Notes

Validation passed: full dev-profile Cargo suite, strict Clippy, rustfmt, diff whitespace checks, the focused notice projection test, all 43 web unit tests, and the deterministic compact-card menu test. Validation output is under `/mnt/optane/mj-jev-log-only-*`. Logs remain at `jev-decisions/decisions.*.jsonl` under the controller data directory and each worker root. They contain conversation evidence and omit configured authentication keys and HTTP headers.

## Interfaces and Dependencies

Keep DecisionLog and correlated Attempt records. Remove JevDecisions requests/replies and live jev_decision_id fields. Add RuntimeEvent::Notice { message: String }, mapped directly to the existing RelayObservation::Notice. No new dependencies or persistent schema are required.

Revision note (2026-09-21): Replaced the completed inspector plan with the user's requested log-only correction; prior implementation and validation remain available in Git history.
