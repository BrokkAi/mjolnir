# Quiet chat feedback and immediate submissions


This ExecPlan follows `.agents/PLANS.md` and is maintained during implementation.

## Purpose / Big Picture


Submitting a prompt or shell command must show its content on the next terminal or browser render, even when the session relay takes seconds to answer. Routine delivery acknowledgements disappear. Local validation and progress belong beside the composer or affected control; meaningful session outcomes belong in the conversation through existing system rows and relay notices.

## Progress


- [x] (2026-09-19) Inspected terminal submission, projection, notices, browser submission, and controller admission.
- [x] (2026-09-19) Implemented command-correlated immediate submissions on both surfaces, including uncertain delivery and draft recovery.
- [x] (2026-09-19) Relocated chat feedback, separated connection and operation state, and added confirmed relay conversation notices.
- [x] (2026-09-19) Added behavior tests; full Rust suite, Clippy, formatting, and browser checks pass.
- [x] (2026-09-19) Created RTT follow-up issue #1095.
- [x] (2026-09-19) Prepared the validated change for the authorized commit and push on master; fetched origin/master matches the starting commit and requires no merge.

## Surprises & Discoveries


The test environment exports NO_COLOR, which disables theme colors and causes unrelated terminal color assertions to fail. Run the Rust suite with `env -u NO_COLOR cargo test` outside the restricted sandbox.

The durable projection creates a user transcript row at CommandStarted, while CommandQueued appears separately in queue state. Acceptance cannot simply retire the local row. Browser POST /api/actions returns admission, before relay submission completes, so HTTP success is not proof of relay acceptance.

## Decision Log


Use command identity, never text, to reconcile local submissions with the queue and transcript. Identical consecutive prompts must remain distinct. Keep existing durable projection semantics and add local presentation only. Actual round-trip latency investigation is a separate issue, as requested by the user. No database migration or new crate is needed. Dashboard notices remain unchanged.

## Outcomes & Retrospective


Implementation and validation are complete. Both surfaces display submissions immediately and reconcile them by command identity. Routine acknowledgements are suppressed; local feedback stays beside the composer and meaningful outcomes use conversation notices. Browser validation passes (38 unit tests, 79 deterministic scenarios, 3 pre-existing skips). The full dev-profile Rust suite, Clippy with warnings denied, formatting, and whitespace checks pass. No database changes or live-store upgrades were performed. Actual transport latency remains the separate investigation in issue #1095.

## Context and Orientation


`mj-chat/src/chat/active/dispatch.rs` dispatches terminal gestures to background work in `mj-chat/src/chat/remote.rs`. `ChatState::apply_materialized` receives authoritative session snapshots, and `mj-chat/src/chat/transcript.rs` appends local rows that survive projection rebuilds. Shared transient `Notices` currently replace keyboard hints. The browser lives in `mj-controller/src/web/viewer.js`; its action requests become `ControllerAction` in `mj-controller/src/server/actions.rs` and execute in `mj-controller/src/server_runtime/actions.rs`. Browser transcript entries are defined in `mj-client/src/web.rs` and converted in `mj-client/src/transcript.rs`. Existing RecordNotice relay commands become deduplicated system conversation rows.

## Plan of Work


The three milestones are immediate submission display, feedback relocation, and validation with publication. Each is described below and verified through the corresponding behavior tests.

First add local pending submissions keyed by session and command ID. Register before queueing network work, clear only the submitted draft, render content with Sending, and reconcile with queue command IDs or user:/shell: transcript identities. Acceptance retains visible feedback until projection arrives. Definite refusal uses existing unsent payload recovery; uncertain transport failure retains an unconfirmed row without automatic resubmission. Handle image-only submissions, plan prerequisites, and review follow-ups. Add optional browser request command IDs and transcript correlation while preserving old request compatibility and numeric cursors.

Next audit chat feedback. Remove routine acknowledgements. Separate composer/control feedback from shared dashboard notices, preserving footer hints. Show meaningful failures in local system rows if no durable projected failure exists; use RecordNotice for session changes that need shared durable narration. Background operations must remain supervised and failures must not disappear. Do not infer completion from admission.

Finally add tests driving actual input/result/projection transitions and browser delayed requests, update expectations for inline feedback, run checks, create the follow-up issue, and commit only task changes. Keep all unrelated untracked files untouched.

## Concrete Steps


Work from `/home/jonathan/Projects/hel`. Run focused cargo tests outside the restricted sandbox while developing, then `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`. Run `npm test` from `tests/e2e/web`. Do not redirect builds into /tmp. Inspect `git diff --check` and the final diff. Search GitHub issues in BrokkAi/mjolnir for an equivalent RTT investigation before creating “Investigate multi-second prompt submission round-trip latency”. Its body must separate dispatch, controller admission, relay acceptance, projection, and render timings. Stage explicit task paths, commit on the current branch, and push to its configured upstream. The user subsequently authorized merge and push; the current branch is master and the fetched upstream matches the starting commit, so no merge is needed unless upstream advances.

## Validation and Acceptance


With a submission response deliberately withheld, the next render shows the text and image markers and accepts a new draft. When queue or transcript updates arrive there is exactly one representation. Cover result-before-projection and projection-before-result, duplicate text, shell commands, queue saturation, refusal, ambiguous transport errors, navigation, feed resets, and partial plan failure. Local feedback must not overwrite newer drafts or unrelated operation errors. Durable notices appear once on both surfaces; routine acknowledgements no longer obscure hints. All required checks must pass, with environmental limitations recorded here if encountered.

## Idempotence and Recovery


This changes presentation and additive API fields, not stored schema. Keep pending rows client-local and never automatically resend an uncertain operation. Preserve existing saved draft and unsent recovery. Repeated snapshots and late responses reconcile by identity. Existing callers omitting command IDs retain server-generated IDs.

## Artifacts and Notes


Initial working tree has unrelated untracked plans, `1q`, and `mj.sqlite3`. They are outside this task.

## Interfaces and Dependencies


Terminal remote results must retain submission command IDs. Browser prompt/shell actions accept optional command IDs, and BrowserTranscriptEntry exposes optional correlation derived from TranscriptSource. Reuse ChatEntry roles, local trailing rows, RecordNotice, background supervision, and existing queue state rather than introducing a second durable conversation format.

Revision: initial implementation plan recorded after user approval on 2026-09-19.

Implementation update (2026-09-19): command-correlated pending rows now exist on both surfaces, including terminal draft persistence for uncertain delivery. Composer feedback is separate from dashboard notices, operation progress has its own state, and confirmed configuration/mode/goal changes use the relay's existing Notice observation and deduplication. Browser correlated prompt/shell requests await relay submission while legacy uncorrelated requests retain admission responses. The old daemon protocol has string-only errors; the bridge conservatively treats those as unconfirmed rather than asserting the prompt was not sent. The RTT follow-up is https://github.com/BrokkAi/mjolnir/issues/1095. Browser checks require execution outside the sandbox; within it the spawned Playwright listing produced empty output. Final validation results are recorded below.

Validation update (2026-09-19): an unchanged checkpoint transfer test hit its five-second interleaving timeout during a heavily parallel run and passed immediately in isolation. The final full suite uses eight test threads to reduce timing contention. The browser follow-up test now also covers transcript command correlation, and a failed mode prerequisite keeps its never-sent follow-up recoverable. The user authorized merge and push after implementation.

Completion evidence (2026-09-19): `env -u NO_COLOR cargo test -- --quiet --test-threads=8` exited 0; `cargo clippy --all-targets -- -D warnings` exited 0; `cargo fmt --check` and `git diff --check` passed. `npm test` in `tests/e2e/web` passed all 38 unit tests and 79 deterministic browser scenarios, with 3 existing skips. Logs are `/mnt/optane/hel-quiet-chat-tests-final.log`, `/mnt/optane/hel-quiet-chat-clippy-final.log`, and `/mnt/optane/hel-quiet-chat-browser-final.log`. The initial color-environment and timing failures were resolved or isolated as described above; the final full run passes. Final publication commits only this task’s changes and pushes master to origin/master, as authorized.
