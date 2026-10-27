# Keep startup and moving prompts editable with clipboard images

This living ExecPlan follows `.agents/PLANS.md`. Keep the progress, discoveries,
decisions, and outcomes current as implementation and validation proceed.

## Purpose / Big Picture

Users can type and paste text or images into the terminal prompt while a session
starts or moves. Enter queues the message for delivery once the destination is
ready. Unsent drafts and queued messages retain their images through registration
and asynchronous attachment to the session. Clipboard and image processing must
run outside the terminal event loop.

The requested scope is the TUI's existing standby prompt: the ordinary composer
shown before a session is ready. The browser currently has no corresponding
startup composer and is outside this change.

## Progress

- [x] (2026-10-27 14:00Z) Map standby input, clipboard processing, and move rendering.
- [x] (2026-10-27 14:04Z) Settle movable composer ownership and structured queue admission.
- [x] (2026-10-27 15:24Z) Complete shared background clipboard/image composer, live Moving rendering, exact-origin staging, FIFO/receipt recovery, and focused frontend regressions.
- [x] (2026-10-27 14:46Z) Complete structured backend queue, schema/protocol changes, and focused Move/image/retry regressions.
- [x] (2026-10-27 15:07Z) Bind queued Move prompts to the exact Move operation; late cancellation and historical-failure regressions pass.
- [x] (2026-10-27 15:20Z) Validate final shared stale-source guard and historical-failure regression; both pass and source accepted by independent re-review.
- [x] (2026-10-27 15:20Z) Full backend/core/client validation passes: core 552 plus 3 Jev integration, client 43, controller 2,162; no failures.
- [x] (2026-10-27 15:40Z) Verify transition ordering, native reply ownership, immutable retry requests, and once-only recovered image drafts; independent review accepts fixes.
- [x] (2026-10-27 15:40Z) Full touched-crate suites, workspace Clippy, formatting, and diff checks pass. A concurrent-build doctest artifact failure passed its docs-only rerun.
- [x] (2026-10-27 16:11Z) Commit implementation as `8f49f1413`, merge upstream through `77c0143b7`, and pass focused integration, upgrade, Clippy, and formatting checks.
- [x] (2026-10-27 16:11Z) Fix the user-reported lifecycle regression call layout; direct pinned rustfmt file check and workspace formatting pass.
- [x] (2026-10-27 16:39Z) Merge latest upstream, compose protocol 67, complete missing profile fixture fields, and pass final workspace Clippy, workspace formatting, direct lifecycle-file formatting, and diff checks.
- [ ] Publish the validated final integration commit to origin/master.

## Surprises & Discoveries

Terminal bracketed text paste already enters standby drafts, but Ctrl/Command-V
explicitly displays a refusal in `mj-tui/src/dashboard_standby.rs`. Moving is
excluded from `standby_prompt_session`, even though Starting and Resuming can
edit and queue prompts.

`ChatState::draft` uses the existing saved-draft encoding for image payloads.
The standby submission branch currently drops images and returns only the marker
text. Enabling the shortcut alone would therefore be incomplete.

The daemon startup queue currently constructs one text content block. Its
ready-worker check can select the source during Moving before that source is
sealed. Structured queue support and delivery admission must therefore be fixed
together with the UI. Existing encoded string drafts already preserve images;
those interfaces need no replacement solely because they use strings.

Independent review found that moving only input fields and clipboard jobs strands
native prompt completions when the outgoing worker view is dropped. Submission
results must follow the surviving composer at the producer/channel boundary.
Review also found a pre-existing retry-exhaustion path that turns unknown worker
acceptance into an editable draft; only definite refusals may be recovered that
way. Focused regressions must cover both interruption boundaries.

The origin follow-up initially narrowed the completed-destination identity check
to Move-bound prompts. Independent re-review found that ordinary input queued
before Move also needs that guard while a stale source actor is still visible.
The parent corrected this in `startup_can_submit`, sharing one final actor check
for both origins, and extended the existing restart regression to exercise both.

## Decision Log

Decision: reuse the standby composer for Moving and the existing prompt payload
and attachment machinery for images. Rationale: one draft owner and the existing
structured image representation prevent images from disappearing during handoff.
Date/Author: 2026-10-27, Codex.

Decision: extend the existing durable startup queue to structured content with
stable caller identities, and serialize delivery with Move lifecycle ownership.
Rationale: pasted images must reach the destination once; source readiness alone
cannot decide delivery during a move. Preserve original encoded drafts on a
definite refusal. Classify any stored-format extension against older readers and
add a breaking forward revision if old code could silently discard images.
Date/Author: 2026-10-27, Codex.

Decision: on failed or cancelled Move, return definitely unaccepted queued
prompts to the editable draft rather than sending them to the retained source.
Rationale: the intended destination may use a different agent or profile. Keep
uncertain accepted deliveries on worker-receipt reconciliation. Date/Author:
2026-10-27, Codex.

Decision: bind each prompt entered during Moving to the existing
`MovePreparation.operation_id`, and carry that optional origin through the
startup request and persisted step. Rationale: a late request after cancellation
must never run on the source, while a later Resume must not inherit an earlier
Move's failure. Local pre-admission drafts wait for the matching daemon-owned
origin before durable enqueue. No-origin startup prompts obey current lifecycle
ownership and ignore historical terminal Move records. Date/Author: 2026-10-27,
Codex.

Decision: retain structured startup terminal rows as stable ID receipts until
session deletion, expose terminal outcomes by requested command IDs, and dispatch
frontend enqueue/withdraw requests through one FIFO mailbox per session.
Rationale: retries must not restore a refusal twice, local recovery must preserve
newer typing, and recall must not overtake admission. The existing startup table
remains the sole durable owner; notices only explain outcomes to the user.
Date/Author: 2026-10-27, Codex.

Decision: preserve startup Enter semantics during a move: queue once for the
destination and permit recall where the existing queue supports it. Rationale:
the user asked for the same live behavior as a not-yet-started session.
Date/Author: 2026-10-27, Codex.

## Outcomes & Retrospective

Startup and Moving prompts now share background text/image paste and preserve
their live composer and native results across registration, navigation, and
attachment. Structured queued prompts retain stable IDs, exact Move origin,
installed image references, and the complete immutable request across restart.
Refusal recovery consumes matching saved receipt metadata atomically so reopening
never appends the image twice. Independent review accepts the ownership, origin,
and persistence fixes.

All six touched crate suites and workspace Clippy pass. Core has 552 passing
tests plus three Jev integration tests, client has 43, controller has 2,162,
chat has 414, TUI has 567, and the CLI suite and isolated automatic upgrade/PTY
regressions pass. Existing platform-dependent ignored tests remain ignored. A
doctest initially could not find an rlib while another build updated the shared
target; its docs-only rerun passed after that build completed. Formatting and
diff checks pass. No default/live daemon or actual platform clipboard was used.

Upstream master advanced to protocol 65 during implementation. The validated
implementation was committed as `8f49f1413`. The first upstream merge uses combined
protocol 66 and store floor 100. Its targeted checks pass: client 43, controller
startup 54 plus direct historical migration, TUI ingest 22 and combined render 13,
CLI startup requests four, attach two, and isolated daemon startup/upgrade 16.
Workspace Clippy and formatting pass. The user-reported call in
`mj-controller/src/controller/lifecycle/tests.rs` was explicitly formatted with
the pinned rustfmt; its direct edition-2024 check passes.

Upstream subsequently advanced to `58f8eec8b` with protocol 66. Combine those
updates at protocol 67 before the final push; the schema floor remains 100.

That merge's focused CLI startup/automatic-away and TUI combined-render checks
pass, and the requested direct lifecycle-file rustfmt check still passes. Its
new optional `HarnessProfile.muse_auto_review` field was missing from existing
fixture literals across the workspace. The missing initializers now use `None`;
controller Clippy passes after 87 exact field additions in 28 controller files,
with the other workspace omissions fixed similarly. No feature behavior changed.
Final workspace Clippy, workspace formatting, the pinned direct lifecycle-file
formatting check, and staged/unstaged diff checks all pass. The validated
integration commit is ready for publication. No implementation or validation
work remains; publication and peer notifications complete delivery.

## Context and Orientation

`mj-tui/src/dashboard_standby.rs` owns `ChatState` composers for unregistered
launches and registered sessions in transition. Its `standby_prompt_session`
selects the composer, `handle_standby_prompt_key` routes keys, and
`take_standby_prompt_draft` transfers the saved draft to an attached chat.
`mj-tui/src/dashboard_input.rs` routes bracketed text paste. The prompt band is
rendered in `mj-tui/src/combined.rs`.

`mj-chat/src/chat/input_state.rs` owns image markers, saved drafts, queued previews,
and standby Enter handling. `mj-chat/src/chat/active/dispatch.rs` reads clipboard
content in background tasks; `mj-chat/src/chat/active/io.rs` processes the result
and installs attachments off-thread. `mj-cli/src/dashboard/` is the asynchronous
host of the TUI, attachment operations, and daemon queue requests. A daemon is
the controller service; a worker runs the actual agent session. The daemon store
and protocol also need changes because startup delivery currently carries text
only. Classify stored-format compatibility and add a forward migration as needed.

## Plan of Work

First establish how existing structured drafts and daemon startup prompts carry
images. Then enable Moving in the standby selection and expose clipboard work as
supervised background operations through the CLI host, reusing shared clipboard
and attachment helpers. Every result must belong to the composer that requested
it even if selection changes, a launch receives its real ID, or attachment to a
live session completes. Do not install files, read clipboard content, or perform
other blocking work on render or event loops.

Preserve typed text and images through launch registration and chat attachment.
Queue a structured payload rather than bare image marker text; queued previews,
recall, restoration after refusal, and destination delivery must agree. Reuse
existing draft serialization and daemon startup admission instead of adding a
parallel delivery mechanism. Report task failures visibly.

Extend existing transition tests or golden renders for Moving and image paste.
Add focused ordering coverage for asynchronous clipboard completion across
adoption and selection changes where no existing test protects that path.

## Concrete Steps

Work in `/workspace/386351293e1410b9a9d089053453c3b1/hel` on the current `master`
branch. Use normal Cargo/mbx configuration and do not change target storage.
During implementation run focused tests for the changed behavior, such as:

    mbx test -p brokk-mj-tui standby
    mbx test -p brokk-mj-chat <affected-filter>
    mbx test -p brokk-mjolnir <affected-filter>

After the last change run each touched crate's complete suite once, then:

    mbx clippy --all-targets -- -D warnings
    mbx fmt --all -- --check
    git diff --check

Execution is unrestricted in this container; do not request an unavailable
sandbox permission. Any direct CLI/daemon/TUI invocation must specify a distinct
`--instance live-transition-prompts-test` and isolated configuration/data. Keep
existing tests' isolation. Stage only changed files, commit directly on the
current branch, and push to upstream as the user explicitly requested. If the
push is rejected by concurrent updates, fetch and merge upstream, then push.

## Validation and Acceptance

During launch, Starting, Resuming, and Moving, text paste edits the visible
composer and image paste shows an image marker. Images remain attached when
typing continues, when launch registration occurs, and when the live chat opens.
Enter queues the complete message for delivery once, and recall/restoration
preserves its contents. Stopping and Destroying continue to show their lifecycle
status. Switching sessions while a paste runs never inserts into the wrong
session. Failure is shown as a useful notice. Focused regression tests should
fail on the old implementation and pass on the final implementation.

## Idempotence and Recovery

All tests use isolated state, and validation may be repeated without affecting
live sessions. No release, installation, or live daemon changes are necessary.
Validate migrations only in an isolated store. Commit and push only after
validation; retain unrelated
changes in the checkout without staging them.

## Artifacts and Notes

Child investigation reports remain in their Mjolnir report directories until
reviewed. Record decisive commands, results, and limitations here at completion.

## Interfaces and Dependencies

Reuse `ChatState`, existing clipboard readers, attachment installation helpers,
the saved prompt payload representation, and dashboard background task channels.
Keep one authoritative draft throughout each transition and avoid additional
workspace crates or new external dependencies.

The frontend transfers the composer editing state together with its clipboard,
attachment, and native submission result receivers. Clipboard reads can start without a real session
ID; installation waits until registration binds the same composer to its actual
session. Attach preparation snapshots the draft while the live composer remains
editable; accepted attachment moves the live composer into the attached chat.
Standby content wins over an older warm chat, including when the draft was cleared.

The backend startup queue accepts a stable command ID, visible text, structured
content blocks carrying installed image references, and the original encoded
draft for refusal recovery. Withdrawal uses the same command ID. Older stored
text steps remain readable. Current writers need a breaking store revision because
older readers would discard structured content. A per-session delivery gate
serializes worker acceptance with Move lifecycle admission and source sealing;
the lifecycle owner decides whether destination delivery is permitted. Waiting
for another session never holds this gate. A durable unfinished Move blocks
startup delivery after daemon restart until destination queue admission completes.

Revision note: created after mapping the current TUI paths; structured image
queue and asynchronous completion details are the next design checkpoint.
Actual validation confirmed `mbx test` / `mbx fmt` / `mbx clippy` are required here; `mbx cargo` is rejected. The cache is healthy.
Revision note: the source-worker delivery race and text-only durable startup
queue require backend work in addition to the input changes.

Revision note: preserve the live composer and its pending I/O receiver across
attach rather than encoding and recreating pending markers. A shared core draft
merge helper will preserve images when refused work meets a newer saved draft.
