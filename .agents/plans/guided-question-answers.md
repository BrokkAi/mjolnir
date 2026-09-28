# Make questions deliberate and readable

This ExecPlan follows `.agents/PLANS.md` and is maintained throughout implementation.

## Purpose / Big Picture

Terminal and browser users should answer one question at a time, see their progress, and never send untouched defaults accidentally. Long questions, including explicit newlines, must remain readable in narrow panes. The reference is the question flow in the sibling Codex checkout; mj retains its own controls and validation.

## Progress

- [x] (2026-09-27) Inspected both clients, Codex's confirmation tracking, rendering, draft handoff, and browser fixtures; user approved guided flow on both clients and an explicit unanswered warning.
- [x] (2026-09-27) Implemented terminal flow and multiline prompts; 44 focused regressions are covered by the passing 626-test chat suite.
- [x] (2026-09-27) Implemented browser flow; all 17 focused browser tests pass, including phone wrapping and exact partial-answer payloads.
- [x] (2026-09-27) Completed workspace validation, focused retries, formatting, Clippy, and final diff review; delivery is a commit on the existing master branch containing only task files.

## Surprises & Discoveries

The terminal selects the first option before any interaction, and Submit serializes all values. The browser also preselects required choices. Schema validity therefore does not mean the user answered. Terminal field titles are rendered with a one-row unwrapped Paragraph, independent of the otherwise scrollable message.

## Decision Log

The user selected both surfaces, guided navigation, and deliberate partial submission. Track confirmation independently from suggested values and invalidate confirmation on edits. Required constraints remain authoritative: partial submission omits unconfirmed optional fields, never bypasses required validation. Defaults are accepted only by an explicit answer action. Go back is the warning's default action. These decisions preserve the existing response protocol (2026-09-27).

## Outcomes & Retrospective

Both surfaces now implement guided answers, deliberate partial submission, and readable multiline prompts. Confirmation belongs to each logical question rather than to focus or a suggested value. Drafts retain answers and confirmation across terminal handoff or browser refresh. Phone screenshots confirmed that compact navigation leaves more space for question text. Final Q&A tests, formatting, and Clippy pass. The broader web suite retains unrelated fixture failures; the full Rust run had one unrelated timing failure that passed in isolation. No build configuration, live store, branch, or remote was changed.

## Context and Orientation

`mj-chat/src/chat/elicitation.rs` owns the terminal form, rendering, pointer routing, and serialized `ElicitationDraft`; its tests are in `mj-chat/src/chat/elicitation/tests.rs`. The shared component Form owns control focus. `mj-controller/src/web/viewer.js` builds browser elicitation cards and retains them across snapshot updates; `viewer.css` styles them. `mj-core/src/elicitation.rs` defines requests, fields, and response validation. Custom-answer metadata pairs a text field with a choice field as one logical question. `tests/e2e/web/plan-mode.spec.js` supplies isolated browser fixtures without a live daemon.

## Plan of Work

First separate terminal question navigation from control focus. Give each logical question confirmation state and retain it in backward-compatible draft fields. Answer and next validates and confirms the current page; Submit all on the last page confirms that page, validates required answers, then either sends confirmed values or opens a warning listing unconfirmed questions. Previous/Next navigation does not confirm. Go back selects the first unanswered question; Submit anyway deliberately omits unconfirmed optional answers. Editing clears confirmation. Keep Skip/Decline and Cancel. Zero-field forms submit empty content.

Move full question text into a wrapping, scrollable prompt region, preserving explicit newlines and logical scroll anchors across resizing. Keep controls accessible, retain plan-review copying and scrolling, and route page/wheel scrolling for ordinary questions too. Then apply the same progression to browser cards using native controls and paired custom fields. Keep browser state across refreshes and failed sends, and clear it when the request identity or content changes. Guard duplicate submissions through the existing sent-request state.

## Concrete Steps

Work from `/home/jonathan/Projects/mjolnir`. Run `cargo test -p brokk-mj-chat elicitation` outside the restricted sandbox for focused terminal checks. From `tests/e2e/web`, run `npx playwright test plan-mode.spec.js --project=deterministic` and `npm test`. Finish with `cargo test`, `cargo clippy --all-targets -- -D warnings`, and formatting checks in the repository root. Keep normal mbx build storage. All live checks, if needed, must use `--instance qa-guided-test` and isolated data. Stage only task files and commit on the current branch without pushing.

## Validation and Acceptance

Prove navigation cannot send untouched defaults, the primary action advances only one question, backtracking retains answers, editing invalidates confirmation, and the warning defaults to returning rather than sending. Assert actual response content for partial answers, required fields, booleans, multi-selects, custom answers, single and zero-question forms. Test mouse as well as keyboard paths, old/new draft handoff, request refresh/replacement, failed sends, and duplicate submit prevention. Render a long JPEG-style question with explicit newlines at narrow/short sizes; scrolling must expose its final text and resizing preserve position while navigation remains usable. Existing plan-review and compact-pane regressions must continue passing.

## Idempotence and Recovery

No migrations, protocol changes, external publication, or new dependencies are required. Tests use isolated fixtures. Preserve unrelated untracked files. Draft formats gain defaulted fields so old drafts retain text but start unconfirmed; old readers may ignore new fields. Failed sends leave the form editable for retry.

## Artifacts and Notes

Original clipping evidence: `1/2 How should we handle JPEG support given that our existing linearization differs` ends at the pane edge. The regression must include and reveal the remainder of that question.

## Interfaces and Dependencies

Keep `ElicitationRequest`, `ElicitationResponse`, and server API unchanged. Extend only process-local draft state. Reuse Form, ButtonRow, ChoiceList, native browser inputs, and existing response submission. Each client has one owner of confirmation state used by progress, warning, and serialization.

Initial plan recorded 2026-09-27 after approval; execution evidence will be appended as work proceeds.

2026-09-27: Completed both implementation milestones and recorded focused passing results. Browser tests require escalation because Chromium cannot initialize its sockets in the restricted sandbox. No live instance has been used.

2026-09-27 validation findings: the full web unit run has 49 passes and two failures, both ReferenceError: sessionById is not defined (conversation deltas and background-task stop). Both reproduce from an unchanged HEAD archive. The full browser run also encounters existing snapshot-refresh fixture timeouts; compact-cards transition completion reproduces on unchanged HEAD. These failures are outside the question interface. The first workspace Rust test run found one integration expectation for two Enter presses; updated it to one explicit submission. Clippy found one single-character push_str, now corrected.

Rust validation is queued behind a shared mbx registrar lock held by another project's stalled nextest process. Read-only inspection identified PID 474639 (bifrost-ci). Requested the user's choice before intervening, as required by host instructions; no cache layout, lock file, or other process has been changed.

2026-09-27: The user chose to wait; the cache owner released its lock without intervention. Clippy passed. The first complete controller run passed 1903 tests and exposed a DOM fake missing setAttribute; updated the fake and kept warning lists stable across snapshots. The focused embedded-viewer regression now passes. Final guided browser run: 17 passed. Full browser run: 86 passed, 3 skipped, 33 failures in older compact-card/new-session/resume snapshot fixtures; representative compact-card and new-session failures reproduced on unchanged HEAD. Phone screenshots were reviewed and navigation buttons compacted to preserve prompt space. Final workspace tests are running with --no-fail-fast to cover all crates.

2026-09-27 final validation: cargo test --no-fail-fast completed every default-workspace target and doctest. Its sole failure was the existing worker test continuous_overall_activity_does_not_postpone_or_invalidate_parent_check (a 200 ms deadline); cargo test continuous_overall_activity_does_not_postpone_or_invalidate_parent_check passed in isolation. After preserving the keyboard-toggle and mouse-scroll hints, cargo test elicitation passed across the workspace, including all 44 chat elicitation checks and the embedded viewer regression. cargo fmt --all --check and cargo clippy --all-targets -- -D warnings passed. The final guided browser run passed all 17 tests. Full web results remain 49/51 unit tests passing and 86 browser tests passing, 3 skipped, 33 failing in unrelated snapshot fixtures; baseline reproductions are described above. No further implementation work remains for this task.
