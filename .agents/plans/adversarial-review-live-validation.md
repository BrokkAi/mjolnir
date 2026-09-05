# Repair turn review failures found in a live container session

This ExecPlan is maintained according to `.agents/PLANS.md`. Its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective sections are living records.

## Purpose / Big Picture

Validate the restored adversarial review feature through the actual terminal interface in tmux, using the `codex3` profile with `gpt-5.6-sol` at medium effort as the primary agent and `claude2` with the advertised Opus model at medium effort as reviewer. A short Python task runs in a disposable Podman container. Fix failures that prevent the advertised workflow: reviewing the first turn, showing actionable failures, retrying review, and forwarding findings for correction. Record UX recommendations grounded in observed behavior.

## Progress

- [x] (2026-09-05 15:33Z) Inspected configuration, restored review implementation, and built current host and musl worker binaries.
- [x] (2026-09-05 15:38Z) Created isolated tmux workspace and container; confirmed Sol at medium effort; completed the first five-test Python task.
- [x] (2026-09-05 15:42Z) Reproduced first-turn coverage loss, hidden failed-review diagnostic, disabled default action, and failed reviewer retry after correcting the model ID.
- [x] (2026-09-05 16:21Z) Repaired baseline initialization, verdict display/action selection, and reviewer conversation lifetime; committed the validated baseline checkpoint as `ff0512c5`.
- [x] (2026-09-05 16:28Z) Rebuilt and reran first-turn automatic review, visible failure/dismissal, retry, extended intent, cancellation, findings navigation/forwarding, correction, and clean follow-up review.
- [x] (2026-09-05 16:29Z) Reviewed integrated changes; full Cargo suite passed 2,260 tests and Clippy passed; documented evidence and UX recommendations.
- [x] (2026-09-05 16:32Z) Final full rerun after the forwarding-copy correction passed: 2,260 tests, 0 failed, 12 ignored; Clippy with warnings denied, rustfmt, and diff-whitespace checks passed. Stopped the isolated daemon, stopped and removed the three owned containers, and closed the isolated tmux server.
- [x] (2026-09-05 16:32Z) Prepared the remaining reviewed source, tests, report, and this plan for the final commit on `hel2`; no push requested.

## Surprises & Discoveries

The first completed task creates a review baseline after its edits, so no reviewer launches. The terminal says `Review coverage starts here; the next completed turn is reviewed`. This is inconsistent with automatic review of every completed changed turn.

Claude's live bridge advertises `opus[1m]`, rather than `opus`. The initial review correctly rejected the invalid configuration, but the terminal displayed only `Turn review · failed` and `Enter to act · Tab switches agent`. The published failure text was not rendered. Forward findings remained selected while disabled.

After correcting the model ID and retrying the same delta, the reviewer tried to load its old native conversation and failed with `Resource not found`. Controller staging replaces the default reviewer's harness home; the worker retains and reloads the old conversation ID. Review generations also restart from zero in each host review slot.

The findings verdict removed the driver's role list, making completed reviewer transcripts unreachable through Tab. Clean review was recorded as manual dismissal. Extended review incorrectly identified the first-ever prompt as the current task, even after a later parser task; the intent analyst explicitly rejected the parser request as superseded. The repaired chronological context correctly identifies the parser. The forwarding wrapper also overstated validator participation for extended reviews.

## Decision Log

Use isolated state under `target/adversarial-live/`, a separate tmux socket named `mj-adv-live`, and a local fixture repository. Reuse the authorized profile homes as credential sources without changing their configuration. Use the actual advertised Opus ID, `opus[1m]`, to fulfill the requested model choice.

Capture the initial worktree before primary edits, including dirty and untracked files already present. Do not substitute HEAD as the baseline: an agent may commit its changes. Preserve valid baselines across worker restart and report failures instead of silently losing coverage.

Delegate independent fixes to Luna agents with separate file ownership. The parent owns design integration, live evidence, full validation, and the final commit. Preserve native conversation reuse only where the profile and conversation lifetime remain compatible; never remove an active harness's files.

Use a fresh random generation identity per independent role launch, including across host restarts. Stage immutable per-generation profile snapshots separately from every role's live harness home. Archive prior relay state and journals before opening a fresh native conversation, and record the generation only after copying succeeds. Run bulk profile and archive work in supervised blocking tasks. Retain snapshots and archived journals until worker teardown; retention limits are a future improvement.

Keep the driver's authoritative role statuses available at a verdict and add a separate scrollable Verdict tab in the TUI. Preserve typed verdict context through resolution for accurate notices. For intent, use the latest non-control user prompt plus all chronological real user requirements; keep trajectory restricted to the review boundary. This preserves earlier requirements when the latest message is only steering. These decisions follow live failures observed on 2026-09-05.

## Outcomes & Retrospective

The repaired live workflow now reviews the first changed turn, explains configuration failures, retries with a fresh native conversation, displays actionable findings while preserving role transcripts, forwards findings to Sol, and automatically reviews the correction. The final parser fixture passed all 10 tests and received a clean review. Cancelling a running supervisor restored the prompt within two seconds; retry still reviewed the seeded defect. Exact final model/effort values were verified in journals. Extended review used Intent and Supervisor but elected not to launch specialists, so live fan-out is not claimed. Automated role tests cover isolation. The final full suite and required Clippy checks passed, and cleanup is complete. Remaining UX recommendations, evidence, and limitations are recorded in `.agents/docs/adversarial-review-live-2026-09-05.md`.

## Context and Orientation

`mj-controller/src/hel_review_host.rs` owns the daemon review state machine and starts background work. `src/hel_review/driver.rs` decides which reviewing roles run. A role is an independent reviewer conversation, such as the general reviewer, validator, supervisor, or specialist. `src/hel_review/delta.rs` captures repository changes by comparing Git tree objects, which represent file contents without modifying the real Git index. `mj-worker/src/hel_worker_runtime/unix.rs` starts the container worker and primary harness. `mj-worker/src/hel_worker_runtime/reviewer.rs` starts reviewer processes and stores their durable journals. `mj-controller/src/hel_controller/reviewer.rs` stages profile files onto the target. `mj-chat/src/hel_chat/turn_review.rs` and `active.rs` project the daemon's review into the terminal.

The lab's initial source commit was `76c094b0`. The fixture contains `ranges.py` with inclusive `clamp(value, lower, upper)` and a unittest suite; reversed bounds must raise ValueError. The first task added five cases, and the second added negative and floating-point cases.

## Plan of Work

First repair initial capture in `delta.rs` and worker startup, running Git work in a background blocking task before the primary can edit. Add behavior tests proving first-turn and committed edits are included while preexisting dirty content is excluded, and restarts preserve the boundary.

Next repair terminal review rendering in `turn_review.rs` and `active.rs`. Render the daemon's authoritative verdict and failure details, keep reviewer transcripts inspectable, and select an enabled action when review state changes. Test actual key transitions and rendered output.

Repair profile staging and worker conversation lifetime so a fresh review cannot reload a conversation whose files were replaced. Preserve compatible reuse and isolate role homes from the staging source. Test retries, repeated reviews, and role separation. Coordinate any host-generation changes with worker behavior.

Finally rebuild the integrated binaries and exercise quick and extended review in tmux. Use a controlled fixture regression to make the findings/forwarding path deterministic, then verify the correction through tests and a subsequent clean review. Cancel a running review and confirm prompt access and responsiveness. Run repository checks and record bounded evidence and UX recommendations in `.agents/docs/`.

## Concrete Steps

From `/home/jonathan/Projects/hel2`, build with:

    cargo build -p brokk-mjolnir --bin mj
    cargo build --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker

The isolated environment is `target/adversarial-live/env.sh`; it sets `MJ_CONFIG_DIR`, `MJ_DATA_DIR`, and `MJ_WORKER_BINARY`. Its configuration defines only the two requested profiles, the local fixture bundle, Podman target, and `[review]` with automatic review and `model = "opus[1m]"`, `effort = "medium"`. It began with quick review and ended with `tier = "extended"`.

The live terminal is inspected with:

    tmux -L mj-adv-live capture-pane -p -t review

Use `/model gpt-5.6-sol` and `/effort medium` in Prompt. With Prompt focused, one Enter submitted each exact command; verify the heading shows both values. `/review` starts an on-demand review. Arrow keys and Enter resolve a verdict; Escape cancels. Avoid sending a second Enter blindly: while review is running, its enabled default action is Cancel. Change the isolated configuration to `tier = "extended"` for the supervisor path, which may dispatch specialists.

## Validation and Acceptance

Run every Cargo test outside the restricted sandbox. Focused behavioral tests must demonstrate each defect is fixed, followed by `cargo test` and `cargo clippy --all-targets -- -D warnings`. A fresh live primary's first changed turn must launch review. Invalid reviewer configuration must show its exact failure and an enabled default action. Correcting the config and retrying must reach a new working reviewer conversation. A controlled regression must yield a visible actionable finding that can be forwarded and corrected. A clean review must advance the boundary. Cancellation must release the hold promptly and preserve unreviewed changes.

## Idempotence and Recovery

Operate only on the lab's daemon, tmux socket, and recorded container IDs. Existing user sessions must remain untouched. Detaching a terminal does not stop the daemon or container. Final cleanup detached the TUI, stopped the isolated daemon, then used `podman stop --time 10` before `podman rm` on exactly `mj-394a8abab76d-52cd23`, `mj-0174f9e2aa11-39acd0`, and `mj-5d1c8d71459f-64670d`. The `mj-adv-live` tmux server was closed. Keep test evidence but do not commit credential files or runtime databases. Do not delete working files while their owning processes are still running. Commit only changed source, tests, and agent documentation, directly on the current branch; do not push.

## Artifacts and Notes

Live captures are under `target/adversarial-live/`, including `primary.capture`, `auto-review.capture`, and `review-running.capture`. These are local evidence, not release artifacts. Reviewer journals in the owned container provide exact configured model/effort and native-session transitions. Preserve only relevant sanitized excerpts in the final agent-facing report.

## Interfaces and Dependencies

Use existing `GitCommandRunner`, `capture_worktree_tree`, `REVIEW_BASELINE_REF`, and shared subprocess facilities for repository capture. Use Tokio background tasks for blocking work and existing durable relay observations for reviewer state. The UI consumes `RuntimeReviewView` and sends existing `Resolution` values; avoid new protocol commands unless required by a demonstrated lifecycle boundary. No new workspace crate or third-party dependency is needed.

Plan created 2026-09-05 after live failures expanded the task into coordinated lifecycle and rendering fixes. Updated after the final live pass to record the additional intent and notice defects, completed acceptance evidence, and the limit that the small extended fixture did not trigger specialist dispatch.
