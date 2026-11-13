# Preserve an explicitly accepted push review base

This ExecPlan follows `.agents/PLANS.md`. Its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective sections are maintained as work proceeds.

## Purpose / Big Picture

After `set_review_baseline` accepts a reviewed commit, merging a separately reviewed branch must not cause the next review to cover the repository's entire branch history. The next capture must retain that accepted base and report the changes since it. A later review that covers both histories can still advance the base automatically.

## Progress

- [x] (2026-11-13) Claim #1531 and recover the original tool responses and Git history.
- [x] (2026-11-13) Confirm that two incomparable reviewed ancestors cause selection to return no reviewed base.
- [x] (2026-11-13) Reproduce through the worker review request boundary using isolated repositories and relay state; the original implementation failed by choosing the main merge base.
- [x] (2026-11-13) Persist the explicit selection per worktree and retain it when reviewed histories are incomparable; extend checkpoint relocation for renamed worktrees.
- [x] (2026-11-13) All 30 focused Git/capture/checkpoint tests pass, including the original regression.
- [x] (2026-11-13) Run both touched crates' full suites and remaining binary/documentation targets; one unrelated Claude profile socket timeout passes in isolation.
- [x] (2026-11-13) Workspace clippy with all targets and warnings denied, rustfmt, and diff checks pass.

## Surprises & Discoveries

The issue calls `f04501ac` the baseline, but this is the tool request ID. The actual accepted commit is `56e6e4dbe7396f920d61d59d13c2c1a99395862f`, tree `b30042c361105312289e223edfec8487247147b7`. Review `4f9cec32` captured commit `63ee27b2262210acdce426188271204eec9a9eaa` from merge base `d78f8ff582c3c4b3d350428822cc56c4bd595ce7`, reporting 174 files. The direct diff from the accepted commit contains only `workspace_authority_classification.tsv` and `workspace_authority_gate.rs`.

Read-only inspection of the affected container confirms that, before the merge itself was reviewed, `git merge-base --independent` on eligible reviewed refs produced two commits: the accepted commit `56e6e4dbe7` and the separately reviewed second parent `75069212a2`. Neither is an ancestor of the other. `furthest_reviewed_ancestor` returns `None` in this case. The setter only writes the ordinary tree-keyed reviewed ref, so capture cannot distinguish an explicit selection from other reviewed commits.

## Decision Log

Preserve the selected base as a private Git commit ref scoped with the existing worktree identity helper. Explicit baseline acceptance and clean review completion both update that choice atomically with the reviewed receipt; overrides do not advance it. This makes the worker repository the durable owner of the decision, just as it already owns reviewed refs and capture operations. Do not introduce daemon state or change the relay protocol or database schema. Include that ref in ordinary ancestry and merge-boundary filtering so branch switches and newer default-branch history retain existing coverage rules. Prefer a unique reviewed commit that includes all eligible reviewed ancestors; when histories are incomparable retain the eligible worktree selection. The existing conservative behavior remains appropriate for legacy repositories without a selection.

Read the selected ref and reviewed receipts together in one filtered `for-each-ref` enumeration. Do not reread the selected ref after obtaining candidate commits; selection must use the same captured candidate set. Use a Git ref transaction so an explicit-ref write failure cannot partially promote an overridden tree.

Use the same ancestry selection for the setter's existing later-base no-op path. Its current arbitrary first matching ref can report a base that capture would not select. A no-op must also persist its confirmed selection so the next capture honors the response.

## Outcomes & Retrospective

Investigation, implementation, and validation are complete. All 30 focused tests pass. Both touched crates' full suites ran: the worker library had 838 passed, 11 ignored, and one unrelated timeout in `claude_delegation_launches_from_a_checkout_after_remote_profile_repair`, at its existing 10-second socket-accept guard (`subagents.rs:1967`). That test passed alone in 0.19 seconds without changes. The remaining five binary tests pass and both documentation targets contain no tests. The checkpoint library, workspace clippy with all targets and warnings denied, rustfmt, and diff checks pass.

The observed failure was a missing durable selection, rather than a failed setter or an acknowledgement race. The worker now owns that selection alongside its Git receipts. A real-Git worker regression proves the accepted baseline limits capture to two files, either merge parent can be selected in a separate worktree, a clean merge review advances the choice for a subsequent merge, and a failed ref transaction cannot partly promote an override. Checkpoint restoration carries the selection across worktree names without overwriting a destination's own choice.

## Context and Orientation

`mj-worker/src/review/capture.rs` owns Git capture, reviewed-ref enumeration, ancestry selection, and the baseline setter. `mj-worker/src/worker_runtime/review_capture.rs` serializes review relay requests and runs Git work on the blocking pool. `mj-controller/src/review_host/set_base.rs` forwards a baseline request and clears declined findings after the worker confirms a change. `mj-checkpoint/src/archive/git.rs::session_start_scope` already identifies the main or linked worktree without interpreting filesystem paths at the call site. Private `refs/mj/` refs are already captured by checkpoint handling; obsolete per-turn `review-baseline` refs must remain unused.

## Plan of Work

First add a regression through `ReviewCapture::handle` with real Git commits, two independently reviewed branches, their merge, and an accepted explicit baseline. Capture must name that baseline and only the subsequent files. This test must fail against the original implementation.

Then add a scoped selected commit ref in `capture.rs`, include it with reviewed candidates under the existing Git filters, and use it to resolve incomparable tips. Explicit selection must validate that the commit tree was reviewed or overridden. `record_review_base` records the receipt and selection together for explicit acceptance and for the worker's `RecordReviewed` action. Reuse the selection function in `later_review_base`, and persist a confirmed later base in the worker's no-op path. Extend coverage for linked worktree isolation, review advancement through a second independently reviewed merge, newer default-branch merges, and ref-write atomicity. `mj-checkpoint/src/archive/git.rs::push_review_base_ref` owns the ref naming so checkpoint restoration can relocate that same choice from a linked worktree into a new checkout without duplicating its interpretation. Extend the existing worktree relocation integration test, including preservation of a destination's own selection.

## Concrete Steps

Run commands from the repository root. Keep normal mbx Cargo storage and package selection unchanged. Every `cargo test` runs with elevated sandbox permissions. Worker tests create private temporary repositories and relay journals; they never access the live instance.

    cargo test -p brokk-mj-worker --lib -- review_capture::tests::set_review_baseline_survives_merging_independent_reviewed_histories
    cargo test -p brokk-mj-worker -p brokk-mj-checkpoint --lib -- review::capture::push_tests worker_runtime::review_capture::tests archive::tests::session_starts_and_explicit_review_bases_follow_worktree_names
    cargo test -p brokk-mj-worker -p brokk-mj-checkpoint
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

After validation, stage only this task's files, commit on the current master branch, request the required review, and push to `origin/master`. If upstream moved, fetch and merge it without rebasing. No installation is authorized by this task.

## Validation and Acceptance

The new worker regression fails before the fix because its base is the default branch's merge base, then passes with the accepted base and the expected file set. Verify that a uniquely later reviewed merge supersedes the explicit selection, that another linked worktree cannot overwrite it, and that merging newer default-branch work continues to exclude upstream-only changes. Existing worker capture tests protect orphan history, conflicts, clocks, dirty worktrees, and old-base no-ops.

## Idempotence and Recovery

Tests use disposable repositories. The change adds only private Git refs, leaving existing reviewed refs and older workers compatible. A failed ref write is reported before the setter confirms acceptance. Repeated requests remain valid. No live repositories are modified during investigation or testing.

## Artifacts and Notes

The original baseline acceptance and following capture were read from the affected session's materialized transcript at positions 118736 and 118738. Git inspection was read-only in container `mj-a29c9015a1f9-6956bd` on morannon. This plan records the relevant evidence so no live access is needed to implement the regression.

## Interfaces and Dependencies

Keep `set_review_base` and the relay response shapes unchanged. Use existing `GitCommandRunner`, `session_start_scope`, `resolve_commit`, and checked Git helpers. Keep capture mutation serialization in `ReviewCapture::run`; add no independent in-memory flags.

Plan created 2026-11-13 after confirming the original history and the exact ancestry ambiguity.

Updated 2026-11-13 after the regression failed as expected and implementation added a durable explicit selection plus checkpoint relocation. Validation now includes the touched checkpoint crate with consistent package selection.
