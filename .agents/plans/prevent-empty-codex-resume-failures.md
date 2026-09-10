# Prevent empty Codex sessions from becoming unresumable

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

An unused Codex thread may not have a durable native history file. Restarting it by ID can fail with `no rollout found for thread id`, taking down the worker and leaving the UI showing only `Unreachable`. Restart unused threads as new threads within the same Mjolnir session, preserving the workspace, settings, and queued work. Never recreate threads that may contain existing conversation history.

## Progress

- [x] (2026-09-10) Read the affected worker's exit record and journal; confirmed repeated resume failures and zero dispatched or accepted commands.
- [x] (2026-09-10) Pulled origin/master to 0d4503cc before editing.
- [x] (2026-09-10) Implemented bounded journal proof of an unused native session and applied it to worker restarts.
- [x] (2026-09-10) Applied the same policy to ACP bridge replacement, tracking prompts before transmission and agent content even during startup.
- [x] (2026-09-10) Added behavior tests for unused, used, imported, checkpointed, and pending-work sessions, plus a fake agent reproducing missing native history.
- [x] (2026-09-10) Full Cargo suite, final worker suite, focused Codex regressions, Clippy, formatting, and diff checks passed; changes reviewed for commit on master.
- [x] (2026-09-10) Built the controller and native worker, restarted the local controller using `target/debug/mj daemon restart`, and verified automatic recovery of the affected session through its socket.

## Surprises & Discoveries

The worker journal contains a locally created native session followed only by configuration and available-command metadata. Six subsequent restarts attempt to resume that same unavailable ID. The visible relay disconnect is a consequence of the failed agent resume, not the primary failure.

`CommandStarted` means admission, not delivery to the agent. The durable dispatch state distinguishes pending work from an in-flight claim. An initial regression exposed that distinction; the final proof preserves pending prompts while refusing to recreate threads after any possible delivery. Recreating a thread must also retain the relay's original startup context, since installing it again can fail when a pending prompt already owns it.

## Decision Log

- Decision: Prevent unsafe resume selection before starting the agent, rather than matching a provider error and silently falling back to an empty conversation.
  Rationale: Mjolnir owns the durable command journal and can prove that no prompt reached a locally created thread. Unknown or truncated history must remain resumable by its original identity.
  Date/Author: 2026-09-10, Codex.
- Decision: Limit recreation to Codex. Treat explicit imported identities and previously resumed sessions conservatively.
  Rationale: Other harnesses have different persistence and autonomous-turn behavior.
  Date/Author: 2026-09-10, Codex.
- Decision: Prefer the journal's latest native identity over the initial launch identity.
  Rationale: After a replacement, the original launch file can still name the old unused thread. A launch identity seeds a session only until the journal records the current one.
  Date/Author: 2026-09-10, Codex.
- Decision: Classify only available-command, configuration-option, and current-mode announcements as metadata. All other ACP updates require preserving native history.
  Rationale: Unknown future updates must not permit silent loss of conversation content.
  Date/Author: 2026-09-10, Codex.

## Outcomes & Retrospective

The prevention is implemented for both worker restarts and agent-process replacement. No journal schema migration or provider-error string matcher was needed. Unknown, imported, checkpointed, and used history stays on the resume path. The affected local session recovered through the normal controller recovery mechanism, with no manual deletion or rewriting of its durable files. Direct protocol verification reported a successful handshake, `execution: idle`, and `acp_ready: true` at event ordinal 116. This fixes empty-thread resume failure; it does not reconstruct genuinely lost history from a used native thread.

## Context and Orientation

`mj-worker/src/hel_worker_runtime/unix.rs` opens the durable worker journal and selects the native agent identity before starting ACP, the protocol used to talk to an agent. `src/hel_worker.rs` owns that journal and its snapshot. The snapshot's recovery floor marks history that may have been removed after checkpointing; absence of retained prompts beyond that floor cannot prove an empty session. `src/hel_acp.rs` also replaces failed agent processes within a running worker and currently always resumes the identity returned by session creation.

## Plan of Work

First expose a conservative, fallible query on `DurableRelay` proving that the current native session was created locally and has never dispatched a prompt or emitted conversation content. Read retained history in bounded pages only when it starts at genesis. Worker restart selection can then omit resume for a proven unused Codex identity. Preserve imported identities and all sessions with unknown history. Prefer the relay's latest native identity over the initial launch identity after a replacement.

Second track whether a newly opened ACP session requires resume when its process is replaced. A resumed identity always requires resume; a new Codex identity begins unused and becomes resumable before its first prompt is sent or whenever conversation content arrives. Preserve existing restart limits, settings restoration, and command interruption behavior. Retain the existing relay startup context when recreating a native identity instead of installing it again.

## Concrete Steps

Work in `/home/ryan/code/mjolnir`. Edit the three modules above and their colocated tests. Run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Cargo tests must run outside the restricted sandbox. Stage only the changed files and commit on the existing branch without pushing.

Completed validation also included `cargo test -p brokk-mj-core -p brokk-mj-worker codex --lib` after a lint-only correction, and `cargo test -p brokk-mj-worker --lib` after preserving startup context. The final worker suite passed 116 tests with 2 ignored. Full-suite and final-check output is available locally in `target/codex-resume-tests.log`, `target/codex-resume-regressions.log`, `target/codex-resume-worker-tests.log`, and `target/codex-resume-clippy.log`; these generated logs are not committed.

## Validation and Acceptance

Tests must show that restarting a locally created unused Codex session selects a new native thread, while a sent prompt, imported native identity, or checkpointed unknown history preserves resume. A fake ACP process should reject attempts to load an unused ID, terminate after first creation, and still receive a new-session request after replacement. A used thread must continue receiving a resume/load request. The full required Cargo suite and Clippy must pass.

## Idempotence and Recovery

The proof reads journal state without mutating it. A fresh identity is recorded through the existing SessionOpened event. Fail closed on unavailable evidence or journal errors; never delete native history or relay files. Repeating validation is safe. User environment repair, if performed after validation, must use normal worker lifecycle commands and retain durable files.

## Artifacts and Notes

Observed underlying failure:

    resume ACP session ...: no rollout found for thread id ...

Observed durable command counts:

    handled command count 0
    dispatch count 0

## Interfaces and Dependencies

Reuse `DurableRelay`, `RelayObservation`, bounded journal replay, `LaunchSpec`, and `OpenedSession`. No new crates or external dependencies are required. Resume selection becomes fallible because proving the absence of history must propagate journal read failures.

Revision note: Created after investigating the user's affected journal and incorporating the request to prioritize prevention.

Revision note: Updated after implementation and validation to record admission-versus-dispatch evidence, startup-context preservation, latest-identity selection, and successful recovery of the actual affected session.
