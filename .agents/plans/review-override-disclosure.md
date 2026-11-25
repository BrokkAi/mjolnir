# Collapse review override notices on both conversation surfaces

This ExecPlan follows `.agents/PLANS.md` and is maintained as work proceeds.

## Purpose / Big Picture

Review override notices currently fill the conversation with declined findings, reasons, the original review and repository details. Show just the list of declined findings by default. The TUI arrow and web summary expand the entire original notice. Reuse the mailbox disclosure rendering and click handling so both kinds of system messages behave alike.

## Progress

- [x] (2026-11-25 00:28Z) Located the producer, shared transcript projection and both disclosure renderers.
- [x] (2026-11-25 00:28Z) Generalized the shared disclosure description and rendered override summaries through it.
- [x] (2026-11-25 00:28Z) Extended and verified the transcript and browser goldens for expansion, collapse and updates.
- [x] Validate the changed surfaces, Rust formatting and Clippy for all three affected crates.
- [x] Committed the implementation and received the first review.
- [x] Validated all four review fixes with the focused goldens, producer-to-reader test and affected-crate Clippy.
- [ ] Commit the review fixes, obtain a clean review and push to origin/master.

## Surprises & Discoveries

Mailbox disclosure boundaries are already interpreted in `mj-client/src/transcript.rs` for both surfaces. Their current representation assumes one summary line; overrides need a list of summary lines and the complete notice as their body.

## Decision Log

- Decision: Use one disclosure per override notice, with one summary line per declined finding and the original message as the expanded body. Rationale: the user asked for the full message on expansion, including reasons and review context, without repeating that context under every finding.
- Decision: Preserve the full notice text while making multiline field boundaries unambiguous. Rationale: ordinary existing transcripts can use the new rendering immediately, with no worker or store changes. Newly emitted notices indent all continuation lines so quoted field labels cannot be mistaken for real finding or section boundaries.

## Outcomes & Retrospective

The TUI shows only declined findings until its arrow or any finding row is clicked. The web uses the same disclosure description with native touch and keyboard expansion. All 34 selected Rust goldens and the browser conversation golden pass without regeneration. Rust formatting and the initial Clippy checks also passed. First review found two daemon test literals missed by the field rename, clipped expanded mailbox titles, ambiguous raw multiline reason boundaries, and a hidden whole-list truncation marker. All four fixes are implemented and their focused checks pass. The notice producer-to-reader test checks quoted field and section labels, and the browser golden checks touch expansion of a long header-only mailbox event.

## Context and Orientation

`mj-review-host/src/review_host/override_push.rs` records a system notice whose first line identifies the review override, followed by `- Finding:` and `  Reason:` entries, the overridden synthesis and repository details. `mj-client/src/transcript.rs` supplies terminal and browser presentation rules. `mj-chat/src/chat/transcript/render.rs` draws collapsed mailbox events, while `mj-chat/src/chat/transcript.rs` owns their cached rows and click targets. `mj-server/src/web/viewer.js` renders browser entries with native HTML details/summary elements, styled by `viewer.css`. The browser receives its presentation data from `mj-client/src/web.rs`.

## Plan of Work

Replace the mailbox-only list-of-lines shape with a disclosure that has summary lines and body lines. Interpret mailbox batches and override notices once in the shared client. Keep mailbox summary and report boundaries intact. The TUI draws an arrow beside the first summary line, subsequent findings below it, and the full body when opened. Raw transcript rendering continues to expose the stored text. The browser uses the same native details element and styles for mailbox and override disclosures, with each summary line clipped independently and touch or keyboard expansion.

Extend the existing mailbox browser projection golden with an override notice including multiline findings, Unicode and literal markup. Feed this exact response into the existing browser conversation golden. Extend the TUI transcript golden with the same notice and exercise open, close, resize and Raw mode. Keep disclosure identities stable across live row updates.

## Concrete Steps

From the repository root, use `mbx test -p brokk-mj-client golden` and `TZ=UTC mbx test -p brokk-mj-chat golden` outside the restricted sandbox. Regenerate intentional golden changes with `MJ_UPDATE_GOLDEN=1`, inspect the diffs, and run the same checks without that variable. Run `mbx clippy -p brokk-mj-client -p brokk-mj-chat -p brokk-mj-server --all-targets -- -D warnings` for affected crates. Install the existing browser test dependencies with `npm ci` in `tests/e2e/web`, then run `npx playwright test plan-mode.spec.js --project=deterministic -g golden_viewer_conversation`. No test daemon or live instance is needed: the Rust tests use isolated fixtures and the browser spec serves shipped assets with recorded API responses.

## Validation and Acceptance

The TUI and phone initially show only the declined findings. Opening the arrow or touching the summary reveals the unchanged complete notice. Collapsing, resizing and refreshing preserve the expected disclosure state. Mailbox reports still expand independently. Literal markup is displayed as text. Expected outputs are checked into the existing golden files; all selected tests must report a nonzero passing test count.

## Idempotence and Recovery

The change touches presentation only. Tests may be rerun safely. No database migrations, daemon replacement, settings changes or Rust output redirection are needed. Golden regeneration changes only checked-in expected output and must be reviewed before committing.

## Artifacts and Notes

Verified without regeneration: `TZ=UTC mbx test -p brokk-mj-client -p brokk-mj-chat golden` reported 31 chat and 3 client tests passed; `npx playwright test plan-mode.spec.js --project=deterministic -g golden_viewer_conversation` reported 1 passed. The browser used a real touch-enabled context, and its expanded body matched every line of the shared notice fixture. Rust formatting and `git diff --check` passed. `mbx clippy -p brokk-mj-client -p brokk-mj-chat -p brokk-mj-server --all-targets -- -D warnings` passed on the dev profile.

## Interfaces and Dependencies

Use a shared serializable `TranscriptDisclosure` with `summary: Vec<String>` and `body: Vec<String>`, returned by `transcript_disclosures(role, lines)` in `mj-client/src/transcript.rs`. Browser entries carry this same description. Reuse the existing TUI identity and row tracking mechanisms, renamed to describe disclosures rather than mailbox-only events. Use existing ratatui rendering and native HTML details/summary; no new dependencies.

Plan created after inspecting the existing mailbox presentation path; no implementation changes yet.

Updated 2026-11-25 00:28Z: shared rendering and interaction goldens are complete; recorded the exact passing commands and retained review and publication as remaining work.

Updated 2026-11-25 00:29Z: all local validation passed; ready for the required committed review.

Updated 2026-11-25 00:33Z: implemented all four first-review findings. Expanded web summaries now wrap; notices indent multiline findings and reasons; truncated lists disclose their omission marker; daemon test literals use the generalized field. Extended the producer-to-reader integration test and the existing goldens.

Updated 2026-11-25 00:35Z: the client and TUI goldens and the existing override integration test each report one passing test, without regeneration; the browser conversation golden reports one passed. Clippy passed for client, review host and daemon with all targets, including the previously missed daemon test fixtures. Ready for another committed review.
