# Echo elicitation replies in the conversation

This living ExecPlan follows `.agents/PLANS.md`. Keep its progress, discoveries, decisions, and outcomes current.

## Purpose / Big Picture


After a person answers a harness question, the conversation should retain their answer beside the question, including asynchronous Codex questions answered after a turn finishes. The terminal, web viewer, and reviewer conversation should all show the same user reply. Fields marked secret must be masked before anything is recorded.

## Progress


- [x] (2026-10-07) Pulled master with `git pull --ff-only`; inspected question acceptance, worker events, and conversation projection.
- [x] (2026-10-07) Carried masked reply text on resolution events for harness, permission, plan, and recovery questions; projected primary and reviewer user entries.
- [x] (2026-10-07) Extended the Claude/Codex integration, transcript goldens, journal compatibility, and database integration coverage. Focused harness and terminal golden tests pass; the macOS golden preserves its existing Cmd-V variation.
- [x] (2026-10-07) Workspace `cargo clippy --all-targets -- -D warnings`, formatting, and diff checks pass. Chat and controller suites completed; both failed controller cases passed their targeted reruns after correcting the database fixture.
- [x] (2026-10-07) Core (477), transcript (80), worker (664 plus binary/integration suites), and evaluator fixture suites pass. Worker tests include accepted-answer withdrawal ordering, harness-originated cancellation, and async Codex replies after turn completion.
- [x] (2026-10-07) Committed the validated implementation on master; the requested upstream push is the final repository operation.

## Surprises & Discoveries


The live `RespondElicitation` request intentionally avoids journaling its raw content. Existing `ElicitationResolved` events retain only the action, so neither conversation projection nor reconnect replay has the answer. ACP user-message notifications are ignored to avoid duplicating ordinary prompts. Explicit replies and automatic withdrawals must remain distinguishable.

User transcript items normally mark turn starts and provide provisional session titles. Reply items therefore use the reserved `elicitation-reply:` identity prefix, and `TranscriptItem::is_user_prompt` excludes them. Existing database queries already restrict prompts to `user:` and `user-` identities; database integration coverage checks agreement with the shared in-memory predicate.

The original answer wait raced a ready answer against cancellation, so it could lose an accepted answer. The pending map now decides both acceptance and withdrawal under its existing mutex. Cancellation that finds no entry receives the already decided answer rather than discarding it. A deterministic test makes both notifications ready together and checks both ownership outcomes.

The first full run passed the chat suite (411 passed, 2 ignored) and 1,925 controller tests. The database integration's new prompt-history assertion initially failed because the existing turn fixture had no prompt content; it now supplies a real prompt. An existing shell-profile selection test failed under full load and passed immediately in isolation. The stopped run had not reached the remaining crates, which are being validated separately rather than repeating completed suites.

## Decision Log


Decision: add optional rendered `reply` text to runtime and durable resolution events. Format question labels, option titles, custom answers, primitive values, and masking in the shared elicitation module before sending the event. Rationale: the existing resolution owns the answer; one recorded event carries both the pending-question transition and the user message, without storing raw secret content or changing command admission. Date/Author: 2026-10-07, Codex.

Decision: advance relay protocol from 34 to 35. Rationale: older controllers would drop the new reply field when recomputing event digests. New controllers must continue reading old events, with absent replies omitted during serialization so existing digests remain unchanged. Date/Author: 2026-10-07, Codex.

Decision: distinguish replies from prompts with a shared transcript identity prefix, retaining it through legacy-entry materialization. Rationale: replies are visible user messages but must preserve turn recovery, prompt history, and provisional titles. Date/Author: 2026-10-07, Codex.

Decision: serialize cancellation and acceptance through pending-entry removal. Rationale: the accepted reply must survive simultaneous harness withdrawal; readiness order is not authority to erase an accepted answer. Date/Author: 2026-10-07, Codex.

## Outcomes & Retrospective


Implemented reply echoes for all harnesses through the shared resolution path, including reviewer and recovery questions. Replies retain user authorship, visible option labels, custom text, and secret masking. Prompt history, session titles, and turn boundaries exclude reply items. Acceptance and withdrawal share one pending-map decision, so an accepted reply survives simultaneous cancellation.

The affected crate suites, workspace dev-profile clippy, formatting, and diff checks pass. The initial database fixture error was corrected and its test passed; the unrelated existing shell-selection test passed in isolation. Linux and macOS terminal goldens and the expanded elicitation transcript golden are committed with the feature. No database migration or harness pin change was needed. The implementation is complete and committed on master for the requested upstream push.

## Context and Orientation


The worker runs the agent harness and owns its journal, which is the durable sequence of conversation events. The daemon manages sessions and projects that journal into stored conversation items. `mj-core/src/elicitation.rs` defines normalized questions and responses for every harness. `mj-worker/src/acp/drive.rs` receives ACP (Agent Client Protocol) requests and waits for their answers outside the main notification path. Explicit answers arrive through `resolve_pending_elicitation` in `mj-worker/src/acp/permissions.rs`, which validates and atomically removes the pending entry before sending its answer. Session-configuration and Codex-goal recovery questions have their own existing resolution paths.

`RuntimeEvent::ElicitationResolved` in `mj-core/src/acp.rs` is converted to `RelayObservation::ElicitationResolved` by `mj-worker/src/worker_runtime/unix/dispatch.rs`. The latter is defined in `mj-core/src/relay/snapshot.rs`. `mj-transcript/src/projection/observation.rs` currently only removes the pending question when it sees that observation. Both main UIs read the resulting materialized conversation. Reviewer conversations also consume runtime events through `mj-transcript/src/transcript.rs`.

## Plan of Work


First add `ElicitationRequest::reply_text(&ElicitationResponse) -> String`. Include the question message and each supplied answer with its question title; map selection values to visible option titles, pair custom answers with their parent question, and mask any secret parent or custom field. Explicit skip/cancel replies should say Skipped/Cancelled. Keep raw responses on the live channel only.

Add optional `reply: Option<String>` fields with serde default and omission when absent to both resolution types. Every harness resolution task retains its request and renders a received response. Cancellation with no response sets no reply. Recovery resolutions do the same for explicit answers and set no reply for automatic cleanup. Advance the relay protocol, preserving its old-worker read range.

Then extend resolution projection to close open assistant streams and append one `TranscriptBody::User` item at the resolution event's ordinal when a reply exists. Use the ordinal for stable identity because harness question IDs can repeat across process restarts. Leave turn clocks and active-turn ownership untouched. Add the equivalent user entry for reviewer runtime-event consumption.

Finally extend `form_elicitation_is_advertised_rendered_and_answered` to verify the recorded reply for Claude and for Codex after prompt completion, with invalid or repeated answers unable to create entries. Extend existing transcript goldens with real normalized questions and their rendered replies, plus an old serialized resolution fixture whose digest remains valid. Verify secret content is absent from the serialized event as well as the visible reply.

## Concrete Steps


Work in `/home/jonathan/Projects/mjolnir/.mj/clones/c9c409513af4b11ac9a4e8332aa03116`. Use the normal Cargo/mbx storage and dev profile; do not redirect target directories. Run every `cargo test` outside the restricted sandbox. Use isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR` for tests; any live binary must use `--instance elicitation-echo`.

    cargo test -p brokk-mj-worker form_elicitation_is_advertised_rendered_and_answered
    cargo test -p brokk-mj-worker a_form_the_harness_withdraws_still_resolves
    MJ_UPDATE_GOLDEN=1 cargo test -p brokk-mj-transcript golden_plan_proposal_transcript
    cargo test -p brokk-mj-core -p brokk-mj-transcript -p brokk-mj-worker -p brokk-mj-chat -p brokk-mj-controller
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    git diff --check

Review generated golden changes before the final full test run. Stage only changed files, commit on the current master branch, then `git push` to its upstream. If another person advances master, preserve this branch and do not rebase without instruction.

## Validation and Acceptance


The harness integration must show `Choose an architecture` followed by `Architecture: Thin callers`, including when Codex's turn has already finished. Golden transcripts must retain that user reply between assistant messages, retain replies after reload/replay, and show custom text under its question title. Secret field values must never appear in the serialized resolution. Automatic harness withdrawal adds no user reply. Legacy resolution JSON must still deserialize and serialize unchanged so its existing digest remains valid. The affected crate suites and workspace clippy must pass before commit.

## Idempotence and Recovery


Existing atomic answer acceptance stays intact: stale or invalid answers are refused, and duplicate replies cannot consume a pending question twice. Event replay remains idempotent through existing ordinal/digest checks. The new optional fields preserve old journals. Use only isolated tests; no default daemon or user data is modified.

## Artifacts and Notes


Expected visible answer:

    Choose an architecture

    Architecture: Thin callers

The completed golden files and test results will provide the final evidence.

## Interfaces and Dependencies


Reuse existing `ElicitationRequest`, `ElicitationResponse`, `RuntimeEvent`, `RelayObservation`, and `TranscriptBody::User`; add no crate, database column, or raw response ledger. All formatting belongs in the shared elicitation module so primary, reviewer, and recovery paths agree. Main transcript projection reads the already masked string rather than reconstructing it from a separate snapshot.

Plan created 2026-10-07 after tracing why answers disappear from the conversation.

Plan revised 2026-10-07 to preserve prompt and recovery boundaries and make accepted replies survive concurrent cancellation; both interactions were identified during implementation review.

Validation completed 2026-10-07 after correcting the empty-prompt fixture and rerunning only the failed controller cases, then completing the suites that Cargo had not reached.
