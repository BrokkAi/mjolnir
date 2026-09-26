# Start bundle sessions at an exact commit

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Issue #1162 lets an API or ACP scheduler create an isolated bundle checkout at a recorded full commit ID, optionally on a new private branch. A moving remote branch must not change the starting commit. Existing `launch_base` continues to mean a diff baseline for bundle sessions.

## Progress

- [x] (2026-09-26) Read the issue, current checkout implementation, public API, ACP ownership, and database migration rules; claimed the issue.
- [x] (2026-09-26) Added the scoped request, persistence, API receipt, and ACP launch flags.
- [x] (2026-09-26) Implemented strict checkout preparation and interruption recovery.
- [x] (2026-09-26) Added disposable repository and fake-service behavior regressions and public documentation.
- [x] (2026-09-26) Nine focused controller/API tests, all-target Clippy, and formatting passed.
- [x] (2026-09-26 12:25Z) Reviewed and merged PR #1164 after CI run 36239938784 passed every job; issue #1162 is closed.

## Surprises & Discoveries

Bundle initialization currently applies `launch_base` and `launch_branch` to every repository. Its existing boolean completion marker returns early without validating identity. Exact checkout needs its own preparation marker that names the session and immutable selection, and must validate clean HEAD before readiness. ACP already records ownership before awaiting preparation, so its exit policies can retain or clean up failed preparation without attachment support.

## Decision Log

- Decision: Add `checkout: {repository_id, commit, branch?}` with a required configured repository ID and full hexadecimal commit object ID. Reject its use with raw project directories or legacy launch selectors. Other bundle repositories retain normal defaults.
  Rationale: This is explicit for both single- and multi-repository bundles and preserves the old interface.
  Date/Author: 2026-09-26, Codex.
- Decision: Persist the immutable request and expose it in the session receipt; readiness means preparation verified the selected HEAD, branch, and clean tree. Live diff metadata continues to report subsequent changes.
  Rationale: The original selection must remain explainable after the agent changes the checkout.
  Date/Author: 2026-09-26, Codex.
- Decision: Migration 54 is breaking and advances the read/write floor atomically.
  Rationale: An older daemon cannot enforce the new preparation contract, so older daemons must not recover or provision these records.
  Date/Author: 2026-09-26, Codex.

## Outcomes & Retrospective

Implementation is complete. Nine focused controller/API tests passed; Clippy passed for all targets. The user requested that CI handle broad validation, so the local full Cargo suite was stopped during compilation. CI run 36239938784 passed the full suite, Clippy, Linux/Windows/macOS, desktop, portable-worker, web, and reliability gates. Reviewed PR #1164 merged as cd3bffd1dc5089eb498699885b468d429b7ad67d; issue #1162 is closed.

## Context and Orientation

`mj-core/src/state.rs` stores sessions. `mj-controller/src/database/state_io.rs` loads and saves them; `database/schema.rs` retains forward migrations. HTTP create requests in `server/api/types.rs` become `ControllerAction::New`, then `CreateSessionRequest` in `mj-client/src/daemon.rs`, then `SessionLaunchOptions` in `controller.rs`. `controller/network_git.rs` initializes cloned bundle repositories before the harness starts. A harness is the agent process that accepts prompts. `mj-cli/src/acp.rs` translates ACP session creation into HTTP creation, records the Mjolnir session ID, then waits for readiness. Viewer session projection feeds public session receipts.

## Plan of Work

First add a shared serializable exact-checkout type and thread an optional value through creation, session storage, viewer projection, and API receipts. Validate full commit syntax and repository scope before provisioning. Add explicit ACP flags for repository, commit, and optional branch; require a bundle and a complete selection. Persist through migration 54 and bump the daemon protocol because creation messages change.

Second extend bundle initialization only for the selected repository. Resolve the full object ID as a commit, fetching that exact object from origin if absent and failing visibly if unavailable. Validate branch names with Git. Refuse dirty workspaces and incompatible occupied branches. Record a session-specific preparation marker before changing HEAD, create a private untracked branch (or detached HEAD when omitted), and verify the exact selection and clean tree before the completion marker. Retrying interrupted preparation may only continue the same recorded selection and must not reset work. Completed sessions are resumed from checkpoints without reapplying their original starting revision.

Third add behavior tests with real disposable Git repositories containing A and later B: verify HEAD A, clean worktree, no remote publication, new branch, unavailable objects, incompatible branches, dirty retry, interruption, and repository scope. Fake HTTP/ACP services verify request forwarding, readiness refusal, and existing ownership/exit policies. Document flags, selection scope, receipts, failure, and retry behavior in human API/ACP documentation.

## Concrete Steps

Work from the repository root. Run focused tests during implementation, then run these outside the restricted sandbox:

    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

Use existing temporary configuration/data isolation in automated tests. Any manual invocation of the built application must use `--instance exact-checkout-1162`; never open the default store. Push only the feature branch, open a PR closing #1162, inspect its diff and all required checks, and merge only when green, as requested by the user.

## Validation and Acceptance

A bundle session with a full commit A and a previously nonexistent private branch starts clean at A even when origin's default branch is B. Invalid, unavailable, ambiguous, or incompatible selections fail before the harness can prompt. The public receipt identifies session, bundle, repository, starting commit, and requested branch. Reusing bundle/workspace records creates independent sessions. Existing diff-base regressions still pass. ACP forwards the exact selection and preserves cleanup while readiness is pending. The isolated store migration retains old records and prevents older readers/writers from bypassing the new contract.

## Idempotence and Recovery

Preparation never pushes, changes a source checkout, or force-resets an occupied checkout. A durable session ID exists before provisioning. The preparation marker binds retries to that session and selection. Errors remain recorded; a new dispatch creates a new session. Existing suspend/destroy policies own cleanup. Keep all tests in temporary repositories and isolated stores.

## Artifacts and Notes

The initial focused controller/API exact-checkout run passed 9 tests in 0.92 seconds. The shared `/tmp` tmpfs was full during the first compilation; all subsequent build/test commands use `TMPDIR=/home/ryan/mj-tmp-9767`, an isolated directory on the main filesystem, with Cargo output retained under repository `target/`. No other tasks' files were removed.

The audited baseline is commit `4d6c0ce9`. The prior regression `a_launch_base_sets_diff_base_without_moving_selected_branch` proves the existing baseline semantics and must remain intact.

## Interfaces and Dependencies

Use a shared `mj_core::remote_git::ExactCheckout` value, the existing `CommandExecutor` for Git, and existing session lifecycle supervision. No new crate or dependency. Public JSON uses `checkout`, `repository_id`, `commit`, and optional `branch`. ACP adds `--checkout-repository`, `--checkout-commit`, and `--checkout-branch`.

Initial plan recorded 2026-09-26 before implementation.

Implementation update 2026-09-26: preparation markers also retain the original HEAD and branch, so interrupted retries cannot reset newly committed work. Public receipts expose immutable intent, with readiness distinguishing verified preparation.

Validation update 2026-09-26: the user explicitly delegated the big validation run to CI. Stopped the local full-suite build without deleting build files; subsequent broad validation belongs to CI.

Completion update 2026-09-26: CI exposed an old parked-state migration assertion fixed to require its minimum supported floor without assuming no later migration exists. All final checks passed before merge.
