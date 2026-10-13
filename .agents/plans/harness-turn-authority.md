# Preserve harness authority over active turns

This plan follows `.agents/PLANS.md`.

## Purpose / Big Picture

A harness that still owns an ACP `session/prompt` request must remain in its turn even when Jev thinks its last words sound finished or ask a question. Jev continues to detect unfinished work after the harness replies. Historical assistant text must never replace the current turn's reply evidence.

## Progress

- [x] (2026-10-11) Diagnosed session dcab8fbbe67dcb98bd18af4b41257d10: two fresh prompts each ended after 60 seconds because old assistant text was classified as a current question.
- [x] (2026-10-11) Remove classifier authority to complete pending ACP prompts and correct reply evidence ownership.
- [x] (2026-10-11) Replace obsolete running-classifier tests with protocol regressions and test stale history at the durable assessment boundary.
- [x] (2026-10-11) Validate: both focused regressions, all 698 worker tests (11 ignored), and workspace clippy pass.
- [ ] Commit on the current branch, push upstream, then install from the unchanged commit.

## Context and Orientation

`mj-worker/src/acp/session.rs` owns the active harness prompt future. A competing Jev future currently emits `PromptFinished` before that future completes. `mj-worker/src/acp/verdict_client.rs` runs that classifier loop and also bounds completed-turn evidence. Its `bound_authorization` helper overwrites current reply text with the most recent assistant message anywhere in history. `mj-transcript/src/turn_context.rs` already resets current reply state when a prompt starts. `mj-worker/src/relay/verdict.rs` owns durable assessments after actual turn completion and remains the place for Jev to detect remaining work.

## Plan of Work

First remove the running classifier future, its terminal transition, and its now-unused implementation. Preserve configured stall limits, explicit cancellation, and genuine harness completion. Keep durable completed-turn assessments unchanged. Then stop `bound_authorization` from synthesizing current reply text from history. Replace tests for the obsolete policy with a real ACP wire exchange proving an unanswered prompt remains active despite old activity timestamps, and extend durable assessment coverage so old questions remain history while an unanswered new turn has no reply.

## Validation and Acceptance

Use the configured mbx Cargo wrapper from the repository root. Run focused worker regressions outside the sandbox, then the full worker suite and `cargo clippy --all-targets -- -D warnings` once after the final code changes. Run `cargo fmt --all -- --check` and `git diff --check`. Existing isolated completed-turn continuation tests must still pass. No new binary may address the default live instance.

## Decision Log

The pending ACP request is the authoritative active-turn owner. Removing the competing completion branch makes classifier-driven premature completion impossible, instead of adjusting its confidence or timer. Preserve old serialized stop reasons for compatibility; new running turns will no longer generate them. Existing explicit stall policies remain user-owned cancellation policy.

## Surprises & Discoveries

The reset path already clears both current and completed reply text. The observed stale reply is reintroduced by the authorization-bounding helper, which chooses an assistant message before newer user input.

## Outcomes & Retrospective

Implementation and validation are complete. The pending harness request is the only ordinary turn-completion owner. Jev still assesses completed turns and can continue authorized unfinished work. Current reply evidence remains empty when a new turn produced no reply, even after recovery. Commit, push, and installation follow.

## Idempotence and Recovery

No schema or persisted format change is required. Do not touch the affected live session during implementation. Commit only this task's files, then push the current branch to its configured upstream; merge upstream if the push is rejected.

## Artifacts and Notes

The diagnosis is captured in `/tmp/dcab-jev-two.jsonl`: both applied decisions show `silent_for_s: 60` and an assistant reply predating the current user prompt. Validation logs will use `/tmp/mj-turn-authority-*`.

Revision: initial plan records the user's requirement that positive harness turn state takes precedence over Jev.

Revision: the user also requested installation after pushing. Use `scripts/install.sh` only after the final commit and upstream merge/push, and make no commits during the build so the host and portable worker stamps agree. The durable completed-turn inference already rejects both explicit prompts and harness-owned autonomous turns, so removing the ACP loop leaves one authoritative lifecycle path.

Validation evidence: `/tmp/mj-turn-authority-focused.log` and `/tmp/mj-turn-authority-evidence.log` each pass their regression; `/tmp/mj-turn-authority-full.log` records 698 passed, 11 ignored, no failures. `/tmp/mj-turn-authority-clippy.log` records successful workspace clippy with warnings denied. Existing serialized classifier stop reasons are retained for old worker records.
