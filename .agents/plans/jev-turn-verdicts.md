# Jev turn verdicts implementation

This ExecPlan is maintained according to `.agents/PLANS.md`. It implements the supplied Jev turn verdict design, reproduced below so this document is self-contained.

## Progress

- [x] (2026-09-19) Read the supplied plan and repository instructions; working tree starts clean.
- [x] (2026-09-19) Implement bounded core evidence, verdicts, activity state, and unit tests; focused activity tests passed.
- [x] (2026-09-19) Implement worker HTTP classification, scheduling, continuation lifetime, and integration tests. The real 60-second cadence test passes, including late-reply discard; continuation integration tests pass.
- [x] (2026-09-19) Implement completion routing, daemon key forwarding, TUI presentation, documentation, and breaking database revision 39.
- [x] (2026-09-19) Review integration and run dev-profile tests (4,079 passed, 25 ignored), clippy, formatting, and portable musl build; prepared validated implementation for commit on the current branch.

## Surprises & Discoveries

TypeSafe's documented Noul response is an object such as `{"type":"noul","noul":0.99}`, rather than the bare probability described in the supplied plan. Implementation follows that documented shape. The existing API `InputRequired` event required a structured elicitation; inferred handoffs have none, so `request` is now optional and omitted for inferred input. Existing structured payloads remain unchanged. Older event clients requiring the field cannot consume the new inferred event without upgrading.

The inherited environment sets `NO_COLOR=1`, which causes existing TUI color assertion failures. Validation unsets that variable; classifier behavior tests themselves passed in the initial run.

## Decision Log

Completed-turn classification is supervised by the relay coordinator in `mj-worker/src/worker_runtime/unix/dispatch.rs`, after completion is recorded. Its owned task set aborts requests on exit; generation checks reject responses after new activity. This also covers harness-started turns at their settled boundary without delaying prompt completion. A newer pending classification cancels the older request.

Shared client rendering gains an `Expecting` kind so CLI, terminal, and viewer labels remain consistent. The web viewer carries it as a lifecycle label without claiming active computation.

The supplied plan explicitly delegates implementation, so independent core, worker, and presentation tasks may run in parallel using the available agents. The named Fable, Opus, and Sonnet models are unavailable; use the session model instead. The primary agent owns integration, review, validation, and commits. Migration revision 39 is breaking: newly stored inferred `input_required` events omit the previously mandatory `request` object, so old readers fail deserialization. Raise the existing shared minimum read/write revision in the same transaction. Only isolated test stores may be upgraded during this feature work. The new activity variant is forward-compatible through older readers' `Unrecognized` state; it must never be interpreted by old readers as idle.

## Outcomes & Retrospective

All 420 core tests, core clippy, and all 84 transcript tests have passed. A synthetic live TypeSafe request succeeded and confirmed the typed Noul response (`noul: 0.98`) and Choice contract (`user`, confidence `1.0`), without sending repository/session content. TUI validation passed (663 tests, 2 ignored), as did isolated schema validation (8 tests). Workspace clippy and the portable musl build passed. The first full suite exposed invalid command IDs in four new fixtures; corrected IDs pass all four focused tests. Final full-suite validation passed: 4,079 tests passed, 25 ignored, no failures. Final workspace clippy also passed.

## Milestones

First establish the core contract: capped evidence, typed verdict parsing, key resolution, and the nonblocking `Expecting` activity state. Verify decision boundaries, UTF-8 tails, and compatibility with focused core tests.

Then connect evidence to the worker's durable relay and classify running and completed turns through bounded asynchronous HTTP requests. Preserve normal prompt completion latency, reject stale replies, keep shutdown responsive, and verify both directions using local fake servers and bridges.

Finally expose awaiting input through wait/transcript/UI and expected continuation through status presentation. Forward the key to workers, document the automatic behavior, review all integration points, and run the full required validation before committing. Each milestone's implementation details and acceptance criteria follow below.

## Concrete Steps

Work in `/home/jonathan/Projects/hel2`. Run `env -u NO_COLOR MJ_CONFIG_DIR=/tmp/mj-jev-validation-config MJ_DATA_DIR=/tmp/mj-jev-validation-data cargo test` outside the sandbox on the dev profile, then `cargo clippy --all-targets -- -D warnings`, and `cargo build -p brokk-mj-worker --bin mj-worker --target x86_64-unknown-linux-musl`. Run `cargo fmt --all -- --check` and `git diff --check`. Record actual results here. Commit only files changed for this implementation on the current branch, without pushing.

## Idempotence and Recovery

The classifier is optional whenever no key exists and fails closed on any request or parse failure. Tests use isolated sessions and loopback fake endpoints; they must not mutate live stores. Expected continuation is process-local and disappears on restart. The new breaking revision is tested only against isolated stores selected with `MJ_CONFIG_DIR` and `MJ_DATA_DIR`; do not run the new binary against the live store. No external publication is needed. Retry interrupted checks normally; do not delete working files of running processes.

## Detailed Design, Interfaces, and Acceptance

# Jev turn verdicts: "awaiting input" vs "waiting on background work"

## Context

mj tracks its own turn separately from the ACP `session/prompt` request. Two states are
ambiguous today and both are handled by either a magic number or by giving up:

1. **Running turn gone quiet.** The harness has not replied. Either the model asked the
   user something the harness does not surface as an elicitation, or it is waiting on a
   build, a subagent, or a slow model. The only tool today is the opt-in millisecond stall
   watchdog (`MJ_TURN_STALL_TIMEOUT_MS`), off by default because silence is not evidence
   (#1020). When it fires it emits `PromptFinished { stop_reason: "harness_inactive" }` and
   leaves the ACP session serving (`mj-worker/src/acp/session.rs:687-721`). The real reply,
   if it ever comes, lands in a dropped future and is discarded. So ending mj's tracked turn
   destroys nothing; it only changes what mj reports.
2. **Harness replied, but the model said it is waiting.** After `EndTurn` the session goes
   Idle and Unread, and the TUI notifies "Finished". mj tracks Claude and Kimi background
   tasks deterministically (`claude_background_tasks`, `kimi_background_tasks` in
   `mj-worker/src/relay.rs:144-150`) and reopens a harness-started turn when output resumes
   (`opens_harness_turn`, `relay.rs:812`). The gap is a model that announces it is waiting on
   something mj cannot see. The session then reads as done when it is not.

Replace both guesses with a classifier call to TypeSafe's Jev model. Jev is a "System One"
model: it takes a text `state` plus typed questions and returns typed answers with
probabilities, no generation. Facts from docs.typesafe.ai: `POST
https://api.typesafe.ai/v1/systemone`, `Authorization: Bearer <key>`, `model: "jev-latest"`,
state is a string, JSON object, or array of text, 32k tokens of state, all questions in one
request are evaluated in parallel, Choice returns `choice`, `probabilities`, `confidence`
(0 to 1), Noul returns a bare probability, input costs $0.042 per million tokens and output
is free. Docs recommend acting automatically above 0.9 confidence and falling back below
0.6; we act at 0.85 and keep today's behaviour otherwise.

Decisions from the conversation: on by default whenever a key is available, graceful
fallback to today's behaviour when no key, the API is unreachable, or Jev is unsure; no
config setting; both directions in this pass; no hosted proxy yet, just a local key from
`TYPESAFE_API_KEY` or `~/.secrets/typesafe_api_key`; HTTP via reqwest with rustls.

## Design

### 1. Pure verdict logic in mj-core (`mj-core/src/activity/verdict.rs`)

New module, no I/O, colocated tests.

`TurnEvidence` is the JSON `state` sent to Jev. Serialized as an object with descriptive
keys (Jev docs recommend objects so instructions can reference keys):

    harness: "claude" | "codex" | "kimi" | ...   (HarnessKind, kebab-case)
    phase: "running" | "replied"                  (which direction is being asked)
    silent_for_s: u64
    tools_in_flight: [{ title: "Bash", running_s: 94 }]
    recent_tools: ["Read", "Edit", "Agent", "Agent", "Bash"]   (last 8 titles, oldest first)
    background_commands: usize
    queued_commands: usize
    user_prompt_tail: "..."           (last 1 KiB of the prompt that started the turn)
    assistant_text_tail: "..."        (last 2 KiB of the latest assistant message)

Text tails are cut on a char boundary. Caps live as named constants next to the struct so
the payload can never exceed a few thousand tokens. Durations are seconds computed before
sending; Jev has no clock.

`questions()` returns the fixed `serde_json::Value` for the request's `questions` field.
Two questions, batched:

    waiting_on: Choice with criteria (structured `what` / `not_for` / `examples`):
      user             it asked the user a question, wants a decision, approval, or a value
      background_work  it launched subagents, a build, tests, or a long command and is
                       waiting for their result; not_for: a finished report of past work
      still_working    a progress note mid-task, not a handoff
      finished         it reported completion with nothing pending
      unclear          the evidence does not support any of the above
    asked_question: Noul, "Does the assistant text end by asking the user for something?"

`TurnVerdict { waiting_on: WaitingOn, confidence: f32, asked_question: f32 }` is parsed
from the response `answers`. Unknown choice strings map to `Unclear`.

`decide(phase, verdict) -> Decision`:

    Running + User with confidence >= 0.85            => AwaitingInput
    Replied + BackgroundWork with confidence >= 0.85  => ExpectContinuation
    anything else                                     => KeepCurrent

`ACT_CONFIDENCE: f32 = 0.85` is the one threshold, documented with the Jev guidance.
`KeepCurrent` is the fail-closed answer everywhere: unreachable API, parse error, low
confidence, `unclear`.

Shared key lookup, `mj_core::activity::verdict::api_key() -> Option<String>`: read
`TYPESAFE_API_KEY`, else read and trim `$HOME/.secrets/typesafe_api_key`; empty or missing
means disabled. Used by both the daemon (to forward) and the worker (to call). No other
config surface.

### 2. Evidence collection in the worker

Widen `InFlightToolCall` (`mj-core/src/activity/tools.rs:26`) with `title: Option<String>`
(serde default, additive; it ships in operational state, which does not deny unknown
fields). Populate it in `observe_at` (`tools.rs:130-166`) from `ToolCall.title`; a
`ToolCallUpdate` keeps the existing title.

Add a shared handle `TurnContext` (`Arc<Mutex<TurnContextState>>`, in
`mj-core/src/activity/verdict.rs`) holding: user prompt tail, assistant text tail for the
current message id plus the last completed message, ring of recent tool titles. Fed from
the one funnel `DurableRelay::record_session_update` (`mj-worker/src/relay.rs:705`), where
the full `SessionUpdate` is in scope, and from the prompt start where `active_prompt` is
set. Reset on prompt start. The existing `CapacityResponse` accumulator
(`mj-core/src/relay/capacity.rs:63-99`) is the precedent; it stays as is since it is
Codex-only and 512 bytes.

`LaunchSpec` (`mj-worker/src/acp/launch.rs`) gains `turn_context: TurnContext` next to
`tools_in_flight`, wired at `mj-worker/src/worker_runtime/unix.rs:372-425` and
`reviewer.rs:842-882`, exactly like `tools_in_flight`. `DurableRelay` exposes
`turn_context()` like `tools_in_flight()`.

`turn_stall_facts` (`mj-worker/src/acp/drive.rs:1004`) grows to carry `queued_commands`
and `background_commands` through a new shared counter or by reading them from the
`TurnContext` handle, updated by the relay in `activity_facts()`.

### 3. HTTP client in the worker (`mj-worker/src/acp/verdict_client.rs`)

`VerdictClient { key, endpoint, client: reqwest::Client }`, timeout 10 s, no redirects,
bounded body read, following `mj-controller/src/zai_usage.rs:49-70`. `endpoint` defaults
to the TypeSafe URL; tests pass a loopback URL. `LaunchSpec.verdict: Option<VerdictSource>`
where `None` means "resolve from environment" and a test injects `Some` with its own
endpoint, following the `stall_policy` override pattern (`launch.rs:46-49`).

`async fn ask(&self, evidence: &TurnEvidence) -> anyhow::Result<TurnVerdict>`. Logs one
`tracing::warn!` per turn on failure (a `warned` flag in the caller), never returns an
error to the turn loop. 429 and 529 are treated as failure for this check, no retry; the
next scheduled check is the retry.

Cargo: add `reqwest` (workspace pin, `rustls-no-provider`), `rustls`, and install the ring
provider in `mj-worker/src/main.rs` the way `mj-cli/src/main.rs:317` does via
`mj_controller::server::install_rustls_crypto_provider`. mj-worker must not depend on
mj-controller (crate layering rule), so move that small function to mj-core or duplicate
the three lines with a comment. Confirm the musl release jobs still build; they already set
a C compiler for ring.

### 4. Direction one: running turn (session.rs watchdog arm)

Enable the arm at `session.rs:645` when `stall_policy.enabled() || verdict_client.is_some()`.
Inside the loop keep the existing `stall_verdict` check. Add a classifier schedule:

- Silence is measured from `acp_activity` as now. First classifier check when silence
  reaches `SILENCE_WORTH_REPORTING` (60 s, `mj-core/src/activity.rs:322`), which is a
  cadence, not a verdict. Then double the gap up to 5 min while silence continues. Any ACP
  activity resets the schedule.
- Build `TurnEvidence` with `phase: Running`, call `ask`. Tag the request with the
  `acp_activity` reading at build time; if activity moved while waiting, discard the answer
  (generation pattern from `mj-core/src/second_opinion.rs:50-66`).
- `Decision::AwaitingInput` emits, in this order: `RuntimeEvent::Warning` with a plain
  message ("mj marked this turn as waiting for you; the harness may still be running"), then
  `RuntimeEvent::PromptFinished { stop_reason: AWAITING_INPUT_STOP_REASON, diagnostic:
  Some(TurnDiagnostic { message, code: Some(AWAITING_INPUT_STOP_REASON), .. }) }`, then
  `break` exactly as the stall arm does, so the ACP session keeps serving and a late reply
  is dropped by the same mechanism.
- `KeepCurrent` continues the loop.

`AWAITING_INPUT_STOP_REASON: &str = "awaiting_input"` lives in `mj-core/src/acp.rs` beside
`PROMPT_UNANSWERED_STOP_REASON` (line 329).

### 5. Direction two: after the reply

At the normal completion site (`session.rs:564-637`) when the stop reason classifies as
Finished, and at `HarnessTurnSettled`, if `background_commands == 0` and no goal is running,
spawn one classifier call with `phase: Replied` (the tracked turn has already ended; this
must not delay the `PromptFinished`). On `ExpectContinuation`, emit a new
`RuntimeEvent::ContinuationExpected { since_ms, note }`; `dispatch.rs` forwards it to
`DurableRelay::expect_continuation(..)`.

The expectation is process-local memory in `DurableRelay`, like `active_agent_terminals`,
not a journal observation, so no relay format change and a worker restart simply forgets it.
It is cleared deterministically by: a harness turn opening (`HarnessTurnStarted`), a new
prompt starting, tracked background work appearing, or the session closing. No timeout.

Publish it: `ActivityFacts.expected_continuation: Option<i64>` (since_ms), filled by
`DurableRelay::activity_facts()` (`relay.rs:497`) and by the operational-state mirror
`RelayOperationalState::facts()` (`mj-core/src/relay/snapshot.rs:519`); the existing
`worker_facts_match_the_published_state` test pins both. `classify` returns a new
`ActivityState::Expecting { since_ms }` after the `Background` check and before `Goal`.
`chat_phase` maps it to Idle, `has_work_in_flight` is false for it (a guess must not block
checkpoints or worker replacement), `is_working` is false. Older readers land in
`Unrecognized` (never idle), which is the module's documented forward-compat rule; note it
in the ExecPlan.

### 6. Stop reason and UI plumbing

- `classify_prompt_completion` (`mj-core/src/state.rs:190`): add `PromptCompletion::InputRequired`
  for `awaitinginput`, or the reason is an Error and `mj wait` exits 1.
- `mj-transcript/src/projection/api_events.rs:35-57`: emit `ApiEventData::InputRequired`
  then `TurnEnded`, not `Error`.
- `mj-controller/src/server/api/wait_policy.rs:10-18`: map to `WaitOutcome::InputRequired`,
  which the CLI already prints and exits 0 on (`mj-cli/src/api_commands.rs:542`).
- `mj-tui/src/dashboard_sessions.rs:50-110` `attention_level`: `Waiting` when
  `last_turn_outcome` (`mj-core/src/state.rs:255`) is Completed with the awaiting-input
  reason and no later prompt; `Working` when the activity state is `Expecting`. Plumb
  whatever `SessionDetail` (`mj-tui/src/ingest.rs`) is missing.
- `mj-tui/src/notify.rs:101` `notification_body`: for `Waiting` without an elicitation, use
  `detail.last_agent_message`, fallback "Waiting for your input".
- Render `Expecting` in `mj-tui/src/render/sessions.rs` with the Working glyph and the
  status text "expecting the agent to continue".
- `mj-chat/src/chat.rs:1083`: no change; the prompt was answered.

### 7. Daemon forwarding

`mj-controller/src/controller/worker_binary/launch.rs:549`: if
`mj_core::activity::verdict::api_key()` resolves on the daemon host, insert
`TYPESAFE_API_KEY` into `target_environment`. This is how a container or SSH worker gets
it. Extend `the_daemon_carries_both_stall_knobs_to_its_workers`
(`mj-worker/src/acp/tests.rs:5032`) to cover the new name.

### 8. Docs and ExecPlan

Write the ExecPlan first at `.agents/plans/jev-turn-verdicts.md` per `.agents/PLANS.md`
(single md fence, Progress / Surprises / Decision Log / Outcomes sections, milestones as
prose). Add `TYPESAFE_API_KEY` to `.agents/docs/internal-environment-variables.md` and a
short paragraph in `docs/src/content/docs/sessions.md` near the stall-timeout prose. Update
the rationale comment at `drive.rs:975-988` so it no longer claims every automatic ending
is deterministic.

## Files

- `mj-core/src/activity/verdict.rs` (new), `mj-core/src/activity.rs` (facts, state,
  classify), `mj-core/src/activity/tools.rs` (title), `mj-core/src/acp.rs` (stop reason),
  `mj-core/src/state.rs` (completion class), `mj-core/src/relay/snapshot.rs` (facts mirror)
- `mj-worker/Cargo.toml`, `mj-worker/src/main.rs`, `mj-worker/src/acp/verdict_client.rs`
  (new), `mj-worker/src/acp/launch.rs`, `mj-worker/src/acp/session.rs`,
  `mj-worker/src/acp/drive.rs`, `mj-worker/src/relay.rs`,
  `mj-worker/src/worker_runtime/unix.rs`, `reviewer.rs`, `unix/dispatch.rs`
- `mj-transcript/src/projection/api_events.rs`, `mj-controller/src/server/api/wait_policy.rs`,
  `mj-controller/src/controller/worker_binary/launch.rs`
- `mj-tui/src/dashboard_sessions.rs`, `ingest.rs`, `notify.rs`, `render/sessions.rs`
- `.agents/plans/jev-turn-verdicts.md`, `.agents/docs/internal-environment-variables.md`,
  `docs/src/content/docs/sessions.md`

Delegation: Fable writes the ExecPlan and reviews; implementation milestones go to Opus
(worker and mj-core changes, with the fake-bridge tests) and Sonnet (TUI, wait, docs).

## Verification

Unit, mj-core: evidence serialization respects the byte caps and char boundaries;
`decide` table (each phase x each choice x confidence above/below 0.85); response parsing
tolerates unknown choices; `classify` places `Expecting` correctly and `has_work_in_flight`
stays false for it; `api_key` prefers the env var over the file and treats blank as absent.

Worker, `mj-worker/src/acp/tests.rs` using `silent_bridge_spec` plus a loopback fake Jev
server (tokio `TcpListener`, canned JSON): a quiet turn whose fake answers `user` at 0.95
ends with `awaiting_input` and the diagnostic code; `background_work` keeps the turn
running past the check; an unreachable endpoint keeps the turn running and logs once; the
existing "late reply is discarded" behaviour still holds; a Finished reply whose fake
answers `background_work` publishes `Expecting`, which clears when the fake bridge opens a
harness turn. Drive the fake server with a body over 64 KiB once to honour the pipe rule
in AGENTS.md even though this path is HTTP.

TUI: `attention_level` returns Waiting for an awaiting-input outcome and Working for
`Expecting`; notification body falls back correctly.

Commands, run outside the sandbox with elevated permissions, dev profile:

    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo build -p brokk-mj-worker --bin mj-worker --target x86_64-unknown-linux-musl

Manual (optional follow-up, isolated store only): with `~/.secrets/typesafe_api_key` present, run a tmux-driven TUI using explicit `MJ_CONFIG_DIR` and `MJ_DATA_DIR`, send a Claude session a prompt that ends in a question the harness
holds open (plan mode is a known case), and confirm the session moves to Waiting within
about a minute with the "waiting for you" notice. Then send a prompt that spawns a
background agent and confirm the session shows "expecting the agent to continue" instead
of Unread until the agent reports back.

Commit each validated milestone to the current branch; do not push.


Revision note (2026-09-19): Created the implementation ExecPlan before source changes, preserving the supplied design and adding execution tracking.

Revision note (2026-09-19): Recorded actual API shape, optional input-event payload compatibility, supervised completed-turn scheduling, and initial validation results.

Revision note (2026-09-19): Classified the stored input-event payload change as breaking and added revision 39, isolated validation instructions, and live API contract evidence.


## Validation Evidence

The synthetic live request returned `waiting_on.choice = user`, confidence `1.0`, and `asked_question.noul = 0.98`. It contained only a fabricated choice question.

`cargo clippy --all-targets -- -D warnings` completed successfully on the dev profile. `cargo build -p brokk-mj-worker --bin mj-worker --target x86_64-unknown-linux-musl` completed successfully. Logs are `/mnt/optane/jev-clippy.log` and `/mnt/optane/jev-musl-build.log`.

The production-cadence integration test `classifier_marks_a_quiet_prompt_as_awaiting_input_without_closing_the_session` passed after 60 seconds, asserting warning order, diagnostic code, continued session service, and discarded late reply. Four completed-turn fixtures passed after correcting command IDs to the existing minimum length. Final full-suite results are in `/mnt/optane/jev-cargo-test-final.log`: 4,079 passed, 25 ignored, no failures. The final clippy log is `/mnt/optane/jev-clippy-final.log`, with warnings denied. `cargo fmt --all -- --check` and `git diff --check` passed.

Live Claude/TUI scenario testing has not been performed. The referenced development-loop memory uses the live store; the new breaking revision must not upgrade it during feature validation. Isolated fake bridges and loopback HTTP tests exercise the complete state transitions, with separate TUI behavior tests and a synthetic live API contract check.

Revision note (2026-09-19): Marked implementation milestones complete and recorded passing targeted tests, portable build, clippy, fixture correction, and the live-store validation constraint.

Revision note (2026-09-19): Recorded final passing workspace validation. Both classifier directions, graceful fallback, UI/API handoffs, continuation lifetime, daemon forwarding, and compatibility guard are complete. No live store was upgraded and no branch or remote publication was created.
