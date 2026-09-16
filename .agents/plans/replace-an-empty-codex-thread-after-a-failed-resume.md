# Replace an empty Codex thread after a failed resume

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It must be maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Mjolnir runs coding agents in long-lived sessions. A session's "worker" is the
process that owns one agent and its durable history; a "harness" is the agent
program the worker talks to over ACP (the Agent Client Protocol), such as Codex
or Claude Code. Codex calls its own conversation a "thread" and writes a
thread's file on disk, its "rollout", only when the first user message arrives.
A Codex session that a person created but never prompted therefore has no
thread on disk at all.

Before this change, such a session could never start again. On restart the
worker asked Codex to resume the recorded thread, Codex answered `no rollout
found for thread id …`, the worker treated that as fatal and exited, and the
controller relaunched it about once a minute forever. The session showed as
Unreachable in the TUI and no prompt could ever be delivered.

After this change, the worker still asks Codex to resume. If Codex says it has
no such thread and Mjolnir's own durable state shows the thread was never used,
the worker says so in the transcript and opens a new empty thread inside the
same Mjolnir session, keeping queued prompts, settings, and workspace. If
Mjolnir's state shows the thread was used, the worker fails with an error that
names the missing native history instead of silently starting a new
conversation. Any other resume failure behaves exactly as before.

You can see it working by launching a Codex session, never prompting it,
restarting its worker, and watching the session come back with a transcript
warning and a new native thread instead of a dead worker.

## Progress

- [x] (2026-09-16) Added `native_session_used` to the relay snapshot and folded a resumed `SessionOpened` into it during replay.
- [x] (2026-09-16) Added `DurableRelay::native_session_may_have_history` and `DurableRelay::mark_native_session_used`; removed `native_session_is_pristine`.
- [x] (2026-09-16) Added `RuntimeEvent::NativeSessionUsed`, emitted once per bridge life from the ACP layer and persisted by the worker runtime.
- [x] (2026-09-16) Made `select_resume_session` infallible and recorded an imported native identity as used at startup.
- [x] (2026-09-16) Implemented resume-then-decide in `serve_session`, with the Codex message match in `mj-core`.
- [x] (2026-09-16) Replaced the journal-proof tests with behaviour tests in `mj-worker/src/relay.rs`, `mj-worker/src/worker_runtime/relay_tests.rs`, `mj-worker/src/acp/tests.rs`, and `mj-core/src/acp.rs`.
- [x] (2026-09-16) Ran `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` on the dev profile.
- [x] (2026-09-16) Live verification against the stuck session `1f5b67e4…`: after the release rebuild and a daemon restart on the new build, the worker relaunched, its relay journal recorded the warning "Codex has no thread 01a0a743… and this session never used it; continuing in a new empty thread" followed by `session_opened` with native id 01a0a7fc…, `worker-exit.json` was removed, and the worker processes stayed up. The negative check (renaming a prompted session's rollout) was not performed.

## Surprises & Discoveries

- Observation: the in-memory flag `resume_required` in `mj-worker/src/acp.rs`
  cannot be reused as evidence that a thread was used. It is initialised to
  true whenever `resume_session` is set or the harness is not Codex, so at the
  moment the resume decision is made it is always true.
  Evidence: `mj-worker/src/acp.rs`, `Arc::new(AtomicBool::new(spec.resume_session.is_some() || spec.harness != HarnessKind::Codex))`.
  A second flag, `native_session_used`, starts false for every harness and is
  set only where the thread really gains content.

- Observation: the classifier `session_update_has_native_history` counted
  Mjolnir's own metadata as agent content, which is what defeated the previous
  journal-replay proof. `goal::publish` in `mj-worker/src/acp/goal.rs`
  synthesizes a `session_info_update` on every Codex session.
  Evidence: the new test `mjolnirs_own_session_info_update_is_not_agent_content`
  in `mj-core/src/acp.rs` fails without the added allowlist entry.

## Decision Log

- Decision: persist "this thread has been used" as a snapshot-only boolean,
  `RelaySnapshot::native_session_used`, rather than as a new
  `RelayObservation` variant in the journal.
  Rationale: `RelayObservation` has no unknown-variant fallback, so a new
  variant would make every older journal reader — the controller's projection
  and `mj-checkpoint`'s archive — fail on a journal written by a new worker.
  A snapshot field is local to the worker's own `relay-state.json`, is written
  only when true, and defaults to false when absent. The one signal that does
  belong in the journal, a `SessionOpened { resumed: true }`, is folded into
  the same field by `apply_relay_event`, so it survives replay everywhere.
  Date/Author: 2026-09-16, implementation agent.

- Decision: classify the snapshot change as compatible.
  Rationale: a new worker reads every older snapshot (the field defaults to
  false, and the other three history signals still cover an older session that
  really has history). The field is serialized only when true, so an older
  worker keeps reading the snapshots of sessions that never used their thread.
  Only downgrading a worker on a session that has used its thread would meet a
  field it does not know, and `RelaySnapshot` rejects unknown fields; that is a
  downgrade, not an upgrade, path.
  Date/Author: 2026-09-16, implementation agent.

- Decision: match Codex's message by the substring `no rollout found for thread
  id`, in one helper in `mj-core/src/acp.rs`.
  Rationale: codex-acp wraps the message as `Internal error: {"details": "no
  rollout found for thread id …"}`, so the substring is the stable part. If
  upstream rewords it, the match stops firing and the resume fails loudly, as
  it did before this change. It can never degrade into replacing a thread that
  still exists.
  Date/Author: 2026-09-16, implementation agent.

## Context and Orientation

The relevant code lives in three crates.

`mj-core/src/relay/snapshot.rs` defines the relay's durable state machine: the
`RelayObservation` events a worker appends to its journal, the `RelaySnapshot`
they fold into, and `apply_relay_event`, the single function that applies one
event to the snapshot. The snapshot is written to `relay-state.json` in the
session's worker directory; the journal holds the events after it.

`mj-worker/src/relay.rs` wraps that state machine in `DurableRelay`, the type a
worker uses to record observations, persist the snapshot, and answer questions
about the session's durable state.

`mj-worker/src/acp.rs` is the ACP layer. `run_inner` supervises one bridge
process at a time, restarting it when it dies or when a cancel is never
acknowledged; `serve_session` performs the ACP handshake, opens or reloads the
native session, and then serves commands. `LaunchSpec` is the struct that
carries everything one bridge life needs, including `resume_session`, the
native thread id to reload.

`mj-worker/src/worker_runtime/unix.rs` is the worker's startup and event loop.
It opens the `DurableRelay`, builds the `LaunchSpec`, and turns each
`RuntimeEvent` the ACP layer emits into a durable observation.

## Plan of Work

First, give the relay a durable record of whether the native thread has been
used. In `mj-core/src/relay/snapshot.rs`, add
`pub native_session_used: bool` to `RelaySnapshot`, with
`#[serde(default, skip_serializing_if = "std::ops::Not::not")]` so older
snapshots load and the field is written only once true, and initialise it to
false in `RelaySnapshot::new`. In `apply_relay_event`, set it to true when a
`RelayObservation::SessionOpened { resumed: true, .. }` is applied: a resumed
thread was not created here, so this journal cannot describe everything in it.

Second, in `mj-worker/src/relay.rs`, delete `native_session_is_pristine` and
add two methods. `native_session_may_have_history(&self) -> bool` answers true
if the recovery floor has moved off zero (history was released to an archive),
if `native_session_used` is set, or if any `Prompt` dispatch in
`snapshot.dispatches` is in a state past `Queued` or `Pending`. A prompt that
only waits in the durable queue never reached the agent and must not count,
otherwise an empty thread with a queued prompt can never be replaced and the
restart loop continues. `mark_native_session_used(&mut self) -> Result<()>`
sets the field and persists the snapshot immediately.

Third, report the two live signals from the ACP layer. In `mj-core/src/acp.rs`
add `RuntimeEvent::NativeSessionUsed`. In `mj-worker/src/acp.rs` add an
`Arc<AtomicBool>` named `native_session_used`, starting false for every
harness, carried on `OpenedSession`. Set it, and emit the runtime event once on
the transition, where the agent sends conversation content (the notification
handler, guarded by `session_update_has_native_history`) and where a prompt is
about to be sent. In `mj-worker/src/worker_runtime/unix.rs`, handle the event
by calling `mark_native_session_used`.

Fourth, teach the classifier about Mjolnir's own metadata. In
`mj-core/src/acp.rs`, add `SessionUpdate::SessionInfoUpdate(_)` to the
allowlist in `session_update_has_native_history`, because `goal::publish` in
`mj-worker/src/acp/goal.rs` synthesizes that variant itself; without this, the
live flag would treat Mjolnir's own goal metadata as agent content.

Fifth, decide after the resume rather than before it. In
`mj-core/src/acp.rs` add `codex_error_reports_missing_thread`, matching the
substring `no rollout found for thread id`. In `mj-worker/src/acp.rs` add
`native_session_may_have_history: bool` to `LaunchSpec`, computed at startup in
`unix.rs` from the `DurableRelay` and carried across bridge replacements by
`run_inner` from the dead bridge's flag. In `serve_session`, when the reload
fails: if the harness is not Codex or the message is not the missing-thread
message, fail as before; if it is and the spec says the thread may have
history, fail with added context naming the missing native history; otherwise
emit a `RuntimeEvent::Warning` and fall through to the existing `session/new`
path, which records a `SessionOpened { resumed: false }` with the new id.

Sixth, simplify `select_resume_session` in `unix.rs` to always prefer the
journal's recorded native id over the launch configuration's, returning
`Option<String>` rather than `Result`, and add
`record_imported_native_identity`, which marks the thread used at startup when
the launch configuration names a native id this journal never opened.

## Concrete Steps

Work in `/home/jonathan/Projects/hel`. Edit the files named above and their
colocated `#[cfg(test)]` modules. Then run, outside the restricted sandbox:

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings

`cargo test` builds the default workspace members; `--workspace` additionally
builds `mj-desktop`, which needs GTK development packages.

## Validation and Acceptance

The behaviour tests are the acceptance criteria. In `mj-worker/src/acp/tests.rs`,
`an_unused_codex_thread_codex_cannot_find_is_replaced_in_the_same_session`
drives the real ACP runtime against a scripted agent that refuses
`session/load` with Codex's wrapped message; it expects a warning naming the
missing thread, a `SessionStarted` for a new native id with `resumed: false`,
and the prompt that was queued before startup finishing on the new thread.
`a_used_codex_thread_codex_cannot_find_fails_instead_of_starting_over` uses the
same agent with `native_session_may_have_history: true` and expects the runtime
to fail with `no native history for thread missing-thread` and to open no
session at all. `only_codex_resume_failures_can_report_a_missing_thread` covers
the other harnesses and other error messages.

In `mj-worker/src/relay.rs`, `a_locally_created_empty_native_session_has_no_history`,
`a_dispatched_prompt_gives_the_native_session_history`,
`a_released_recovery_floor_gives_the_native_session_history`,
`a_resumed_native_session_has_history_after_reopening`, and
`a_used_native_session_stays_used_across_persist_and_replay` cover the
predicate and the durability of the flag.

In `mj-worker/src/worker_runtime/relay_tests.rs`,
`an_unused_codex_thread_is_still_resumed_before_any_decision`,
`an_imported_native_identity_is_recorded_as_used_at_startup`, and
`a_used_native_session_is_reported_as_used_after_a_worker_restart` cover
startup.

Live acceptance (performed 2026-09-16, see Progress): rebuild the worker, restart the local
daemon, and watch a stuck unprompted Codex session. Its relay journal should
show the warning and a `session_opened` with a new native id, `worker-exit.json`
should be gone, and the TUI should show the session reachable. The negative
check is to rename a prompted session's rollout file inside its container and
confirm the worker fails with the missing-history error rather than opening a
new thread; restore the file afterwards.

## Idempotence and Recovery

Every step is additive and repeatable. The predicate only reads snapshot state;
`mark_native_session_used` is a one-way transition that is a no-op once set.
Nothing deletes journal files or native history. A session whose thread really
has history is protected by four independent signals, so an older snapshot that
predates the new field is still judged to have history as soon as it has a
dispatched prompt, a moved recovery floor, or a recorded resumed open.

## Artifacts and Notes

The failure this replaces, as it reached the worker's exit record:

    resume ACP session 0199…: Internal error: {"details": "no rollout found for thread id 0199…"}

The new transcript warning:

    Codex has no thread 0199… and this session never used it; continuing in a new empty thread

The new loud failure, when Mjolnir has evidence of history:

    Codex has no native history for thread 0199…, which this session has already used, so the conversation cannot be resumed

## Interfaces and Dependencies

No new crates. At the end of this work these must exist:

In `mj-core/src/relay/snapshot.rs`:

    pub struct RelaySnapshot { /* … */ pub native_session_used: bool, /* … */ }

In `mj-core/src/acp.rs`:

    pub const CODEX_MISSING_THREAD_MESSAGE: &str = "no rollout found for thread id";
    pub fn codex_error_reports_missing_thread(error: &str) -> bool;
    pub enum RuntimeEvent { /* … */ NativeSessionUsed, /* … */ }

In `mj-worker/src/relay.rs`:

    impl DurableRelay {
        pub fn native_session_may_have_history(&self) -> bool;
        pub fn mark_native_session_used(&mut self) -> anyhow::Result<()>;
    }

In `mj-worker/src/acp.rs`:

    pub struct LaunchSpec { /* … */ pub native_session_may_have_history: bool, /* … */ }
    fn codex_reports_missing_thread(spec: &LaunchSpec, error: &anyhow::Error) -> bool;

In `mj-worker/src/worker_runtime/unix.rs`:

    pub(super) fn select_resume_session(config: &WorkerLaunchConfig, relay: &DurableRelay) -> Option<String>;
    pub(super) fn record_imported_native_identity(config: &WorkerLaunchConfig, relay: &mut DurableRelay) -> anyhow::Result<()>;

## Outcomes & Retrospective

The worker no longer dies in a loop over a Codex thread that was created and
never prompted: it opens a new thread in the same Mjolnir session and says so.
A thread with any evidence of use is never replaced; the worker fails with a
message naming the missing native history instead. The journal-replay proof
that tried to answer the same question before attempting the resume is gone,
along with its fallible resume selection.

The lesson is that a proof assembled from a transcript is only as good as the
transcript's vocabulary. The previous design was defeated by Mjolnir adding one
new update variant of its own. Asking the harness first, and using local state
only to veto a replacement, depends on far less.

Revision note: written at implementation time, 2026-09-16, recording the
design as built, the snapshot-versus-journal decision, and the live
verification that remains outstanding.
