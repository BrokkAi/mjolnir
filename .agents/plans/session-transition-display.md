# Consistent session-transition display

This ExecPlan follows `.agents/PLANS.md`. The user approved implementation, live verification in `plandiag`, and the commit/push handoff on 2026-09-07.

## Purpose / Big Picture

During New, Resume, Move, Stop, force-stop, target destruction, or retained cleanup, both terminal and browser show a compact session placeholder with operation, current stages, and elapsed time. Selecting it shows a small status panel and supported cancellation instead of a transcript or composer. Other sessions remain usable. Move stays one transition across intermediate stopped/running records. Normal checkpointing, brief reconnects, and importing an already-stopped session do not hide conversations.

## Progress

- [x] Inspected lifecycle ownership, TUI rendering/attachment, browser projection, and stage reporting; user selected both interfaces, current-stage progress, and a compact status panel.
- [x] Implement shared transition classification, coherent lifecycle updates, operation identities, and missing shutdown stages.
- [x] Implement TUI placeholders and transcript/input gating with behavior tests (integration validation in progress).
- [x] Implement web placeholders, stale-request protection, and shared creation progress/cancellation with behavior tests. Browser unit tests (15) and compact-card Playwright tests (7) passed; an additional status-clock regression is being added.
- [x] Complete automated integration checks: Rust suites, affected-crate reruns, all-target Clippy, formatting, browser regressions, and isolated local-worker Move acceptance.
- [ ] Complete real-provider tmux/browser tests in disposable plandiag sessions. Blocked on detaching older TUI clients that replace the new daemon; user coordination requested.
- [x] Stop the isolated test resources and record evidence. No new real-provider session was created; the private tmux startup attempt exited.
- [x] Commit the validated implementation checkpoint on the current branch.
- [ ] Finish live acceptance after client coordination and push only task changes.

## Surprises & Discoveries

TUI expanded rows still draw message excerpts while operations run. Conversation visibility checks only pending attachment, not lifecycle ownership. Separate runtime record/lifecycle watch channels clear overlays using intermediate durable state; this can briefly expose Move's old conversation. Stop/Destroy ignore active stages in their displayed label. Browser transcript generation guards only cover navigation, and SSE launches snapshot refresh and transcript reload concurrently. Web New uses a separate phone-action executor without daemon stage reporting. Stopped records with cleanup operations disappear from the browser list.

## Decision Log

Use a shared, pure transition classification with the authoritative operation taking precedence over durable state. Deliver records and lifecycle views from one runtime snapshot together; do not infer operation completion from intermediate session states. Add per-operation identity for stale-result protection without a database migration. Use existing scoped stage guards, adding recovery-copy and close stages; concurrent stages are displayed together, with the oldest active stage's start time. Read-only preflight remains outside transition display.

Preserve drafts/history without rendering or routing execution input to a transitioning chat. Keep the transition detail route selectable; defer transcript delivery and discard late attachment/fetch results. A finished failed operation shows the actual recoverable state and a visible error, not an endless spinner. Preserve web creation's atomic cancel-versus-commit gate and early publication while moving its ownership/progress into the shared daemon workflow. Cleanup failures must be visible in both surfaces.

## Context and Orientation

`src/hel_state.rs` owns persisted session states; `src/hel_targets.rs` owns `ProvisionStage` and balanced `ProvisionStageGuard`. `mj-cli/src/daemon.rs` owns lifecycle admission, background execution, revision snapshots, and stage reporting. `mj-cli/src/pollers.rs` currently splits records and lifecycle data before `mj-cli/src/dashboard.rs` consumes them. `mj-tui/src/render.rs`, `combined.rs`, and `ingest.rs` own row layout, conversation area, and operation display. `mj-cli/src/server.rs` projects browser data and runs web actions; `mj-controller/src/hel_server.rs` defines its public types and routes. `mj-controller/src/web/viewer.js` renders cards/conversations and fetches snapshots/transcripts.

## Plan of Work

First establish shared classification and stable operation ownership, with records/lifecycles applied atomically in clients. Add scoped recovery capture/verification/close stages and preserve terminal cleanup visibility. In parallel implement compact TUI rows/status panels and browser cards/status panels using that contract, including cancellation and asynchronous generation checks. Integrate web Create through shared daemon admission while retaining safe publication and cancellation. Add regression coverage around intermediate states, late replies, and consecutive operations.

Then run full Rust checks and browser tests. Build the actual client/daemon, deploy through supported daemon management if necessary while preserving detached primary workers, and use only disposable real-provider sessions in `plandiag` on localhost/Podman. Observe New/Stop/Resume/Move from tmux and Chromium, preserve a marker file and draft, and capture progress and suppression of conversation. Stop only the test sessions through verified recovery before removing any owned process resources.

## Concrete Steps

Work in `/home/jonathan/Projects/hel2`, remain on the current branch, use `apply_patch`, and coordinate disjoint agent ownership. Run `cargo test -q -- --test-threads=1` outside the sandbox, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and applicable browser unit/Playwright checks. Use a private tmux socket and existing configured real-provider profiles; do not alter primary sessions or global target configuration for tests. Store uncommitted captures under `target/live-session-transitions-20260907/`.

## Milestones

The first milestone establishes daemon-owned transitions. `RuntimeLifecycleView` carries an operation identifier and cancellability; `RuntimeStateUpdate` delivers records, Move intents, and lifecycle owners together. Stop keeps ownership visible while handing retained cleanup to another supervised task. Completion reloads durable records before publishing its result. Tests must show that a joined operation keeps its identifier, the next operation gets a different identifier, and a stale completion cannot remove that newer operation.

The second milestone makes both surfaces consume that ownership. The TUI draws one compact row and a status panel; the browser exposes `ViewerSession.transitioning`, rejects transcript reads while it is true, and invalidates outstanding conversation requests. Web creation uses the same daemon operation and atomic cancel-versus-commit control as other clients. Browser and TUI behavior tests must prove hidden conversation/composer content, current stages and ticking elapsed time, preserved ordinary checkpoint behavior, and visible recovery after failures.

The third milestone validates the integrated product. Rust tests, Clippy, formatting, browser unit tests, and browser regression suites must pass. The isolated local-worker Move harness is additional evidence, not a substitute for the requested configured-provider test. With older clients safely detached, build the latest `target/debug/mj`, run its supported daemon restart, open `tmux -L mj-transition-live` in `plandiag`, and observe New, Stop, Resume, and Move through both tmux and the verified HTTPS loopback viewer. Only disposable sessions backed by `/tmp/mj-transition-live-gTrdnn` are in scope. Record stages and conversation suppression in the artifact directory, then stop test sessions and commit/push the completed result.

## Validation and Acceptance

Tests must show one compact placeholder per transition in every TUI size and desktop/mobile web layout; no transcript, composer, or message excerpts; correct stage updates; and another live session still usable. Cover source-live preflight failure, cancellation, destruction/cleanup, restart recovery, overlapping stages, web-created cross-client progress, reordered records/lifecycles, late transcript responses, and consecutive operation identities. After successful startup, only the ready destination conversation appears. Stop completion removes the row according to existing navigation; failures expose recovery controls. Ordinary checkpoints/reconnects and historical reads outside transitions remain unchanged.

## Idempotence and Recovery

No database migration is intended. Wire changes use the repository's daemon protocol version mechanism. Failed builds leave source editable and cannot justify downgrading the live database. Tests use disposable sessions and preserve verified checkpoints. Stop owning process groups/containers before deleting working files. Keep live credentials out of captures and commits. Commit only owned files and push to upstream under the approved plan.

## Outcomes & Retrospective

The implementation is complete across daemon, terminal, and browser. Automated validation passes, including the isolated end-to-end Move scenario and cleanup. Review found and fixed the cleanup handoff visibility gap, stale browser record/operation pairing, late conversation rendering, question-control reset ordering, missing Stop recovery-copy reporting, and cleanup error persistence. Real-provider plandiag acceptance remains incomplete because older TUI clients restart the prior daemon. Their workers have not been stopped. The implementation checkpoint can be committed, but push remains pending the requested live acceptance.

Revision note (2026-09-07): recorded the approved plan, observed gaps, ownership strategy, acceptance checks, and live-test boundaries before implementation.

Integration note (2026-09-07): the integrated Cargo check passes. Stop now hands retained cleanup to its daemon-owned supervisor and remains visible across that handoff, even if the requesting client disconnects. Browser publication samples in-memory records and operations together and publishes lifecycle revisions immediately, preventing a stale database reload from exposing the old conversation. Retained cleanup failure reporting and stale local completion handling are receiving final review while the full Rust suite runs.

Validation note (2026-09-07): the broader browser suite exposed an ordering bug where the transition generation reset cleared freshly rendered question controls on ordinary conversation navigation. Resetting before rendering those controls fixes it. Compact-card tests (7), plan-mode/question tests (6), project grouping (1), quota (4), and browser unit tests (15) now pass. Both phone and desktop transition visibility are covered. The focused TUI suite passes 342 tests with two ignored. The full integrated Rust suite is being rerun after a test-only import fix and the two new cleanup persistence tests.

Live setup note (2026-09-07): created only the disposable fixture `/tmp/mj-transition-live-gTrdnn`, with committed `transition-marker.txt` containing `LIVE_SESSION_TRANSITION_OK`. No new live session has been created yet. The advertised Tailnet viewer endpoint was rejected by the safety reviewer; `ss` independently verified local port 3765 belongs to the daemon and authenticated HTTPS loopback access succeeds. The supported daemon replacement did not stick because older open TUI clients bring back the earlier daemon. Detaching those clients requires user coordination; an asynchronous request is pending. Existing primary workers have not been stopped. The isolated local-worker Move harness is being run independently and must not be reported as the requested real-provider plandiag validation.

End-to-end note (2026-09-07): `python3 tests/e2e/session_move.py --hel target/debug/mj` passed after fixing the harness to accept the viewer URL's optional trailing slash. The harness now also redacts viewer codes from startup errors. Evidence is `target/reliability-artifacts/session-move-seed-1-3462004/trace.json`; owned workers were stopped and integrity checks passed. The provider in this harness is deterministic, not a configured real provider.

Final-review note (2026-09-07): graceful Stop called the ordinary checkpoint wrapper, which originally disabled RecoveryCopy reporting. The wrapper now enables that stage for HoldThroughClose, and the existing isolated checkpoint reuse/export test verifies it is active during archive export and unwinds on failure. The compatibility test no longer pins a literal protocol version; it checks a request one version behind the current daemon. These final affected-crate checks and Clippy remain in progress.

Final affected-crate result (2026-09-07): `cargo test -q -p brokk-mj-controller -p brokk-mjolnir -- --test-threads=1` passed: 678 controller tests, 192 CLI unit tests, and the CLI integration suites. The earlier full workspace run passed the unchanged chat/core/TUI/worker suites (409/838/342/108 tests respectively) before encountering the now-corrected protocol assertion. Clippy is the remaining automated check.

Checkpoint handoff note (2026-09-07): `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `git diff --check`, and JavaScript syntax checking pass. Clippy requested only equivalent control-flow and test-expression simplifications. All automated validation is complete. No permission to detach the older primary TUI clients has arrived; do not stop their workers or silently downgrade the requested live acceptance to the isolated fake-provider harness. After coordination, rebuild the latest binary before restarting the shared daemon because the final Stop-stage fix was made after the earlier live-test binary build.
