# Repair turn review failures found in a live container session

This ExecPlan is maintained according to `.agents/PLANS.md`. Its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective sections are living records.

## Purpose / Big Picture

Validate the restored adversarial review feature through the actual terminal interface in tmux, using the `codex3` profile with `gpt-5.6-sol` at medium effort as the primary agent and `claude2` with the advertised Opus model at medium effort as reviewer. A short Python task runs in a disposable Podman container. Fix failures that prevent the advertised workflow: reviewing the first turn, showing actionable failures, retrying review, and forwarding findings for correction. Record UX recommendations grounded in observed behavior.

## Progress

- [x] (2026-09-05 15:33Z) Inspected configuration, restored review implementation, and built current host and musl worker binaries.
- [x] (2026-09-05 15:38Z) Created isolated tmux workspace and container; confirmed Sol at medium effort; completed the first five-test Python task.
- [x] (2026-09-05 15:42Z) Reproduced first-turn coverage loss, hidden failed-review diagnostic, disabled default action, and failed reviewer retry after correcting the model ID.
- [ ] Repair baseline initialization, verdict display/action selection, and reviewer conversation lifetime.
- [ ] Rebuild and rerun live quick review, finding forwarding/correction, extended review, and cancellation checks.
- [ ] Run required Cargo tests and Clippy, review integrated changes, document evidence and recommendations, clean up owned live resources, and commit on the current branch.

## Surprises & Discoveries

The first completed task creates a review baseline after its edits, so no reviewer launches. The terminal says `Review coverage starts here; the next completed turn is reviewed`. This is inconsistent with automatic review of every completed changed turn.

Claude's live bridge advertises `opus[1m]`, rather than `opus`. The initial review correctly rejected the invalid configuration, but the terminal displayed only `Turn review · failed` and `Enter to act · Tab switches agent`. The published failure text was not rendered. Forward findings remained selected while disabled.

After correcting the model ID and retrying the same delta, the reviewer tried to load its old native conversation and failed with `Resource not found`. Controller staging replaces the default reviewer's harness home; the worker retains and reloads the old conversation ID. Review generations also restart from zero in each host review slot.

## Decision Log

Use isolated state under `target/adversarial-live/`, a separate tmux socket named `mj-adv-live`, and a local fixture repository. Reuse the authorized profile homes as credential sources without changing their configuration. Use the actual advertised Opus ID, `opus[1m]`, to fulfill the requested model choice.

Capture the initial worktree before primary edits, including dirty and untracked files already present. Do not substitute HEAD as the baseline: an agent may commit its changes. Preserve valid baselines across worker restart and report failures instead of silently losing coverage.

Delegate independent fixes to Luna agents with separate file ownership. The parent owns design integration, live evidence, full validation, and the final commit. Preserve native conversation reuse only where the profile and conversation lifetime remain compatible; never remove an active harness's files.

## Outcomes & Retrospective

Work is in progress. The live test has exposed defects before a successful review; success is not yet claimed.

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

The isolated environment is `target/adversarial-live/env.sh`; it sets `MJ_CONFIG_DIR`, `MJ_DATA_DIR`, and `MJ_WORKER_BINARY`. Its configuration defines only the two requested profiles, the local fixture bundle, Podman target, and `[review]` with automatic quick review and `model = "opus[1m]"`, `effort = "medium"`.

The live terminal is inspected with:

    tmux -L mj-adv-live capture-pane -p -t review

Use `/model gpt-5.6-sol` and `/effort medium` in Prompt, accepting completion and submitting as separate actions where needed. The prompt heading must show both values. `/review` starts an on-demand review. Arrow keys and Enter resolve a verdict; Escape cancels. Change the isolated configuration to `tier = "extended"` for the specialist path.

## Validation and Acceptance

Run every Cargo test outside the restricted sandbox. Focused behavioral tests must demonstrate each defect is fixed, followed by `cargo test` and `cargo clippy --all-targets -- -D warnings`. A fresh live primary's first changed turn must launch review. Invalid reviewer configuration must show its exact failure and an enabled default action. Correcting the config and retrying must reach a new working reviewer conversation. A controlled regression must yield a visible actionable finding that can be forwarded and corrected. A clean review must advance the boundary. Cancellation must release the hold promptly and preserve unreviewed changes.

## Idempotence and Recovery

Operate only on the lab's daemon, tmux socket, and recorded container IDs. Existing user sessions must remain untouched. Detaching a terminal does not stop the daemon or container; stop the owned session through the product before stopping its isolated daemon and tmux server. Keep test evidence but do not commit credential files or runtime databases. Do not delete working files while their owning processes are still running. Commit only changed source, tests, and agent documentation, directly on the current branch; do not push.

## Artifacts and Notes

Live captures are under `target/adversarial-live/`, including `primary.capture`, `auto-review.capture`, and `review-running.capture`. These are local evidence, not release artifacts. Reviewer journals in the owned container provide exact configured model/effort and native-session transitions. Preserve only relevant sanitized excerpts in the final agent-facing report.

## Interfaces and Dependencies

Use existing `GitCommandRunner`, `capture_worktree_tree`, `REVIEW_BASELINE_REF`, and shared subprocess facilities for repository capture. Use Tokio background tasks for blocking work and existing durable relay observations for reviewer state. The UI consumes `RuntimeReviewView` and sends existing `Resolution` values; avoid new protocol commands unless required by a demonstrated lifecycle boundary. No new workspace crate or third-party dependency is needed.

Plan created 2026-09-05 after live failures expanded the task into coordinated lifecycle and rendering fixes.
