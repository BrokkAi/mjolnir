# Prepare clone-cache snapshots concurrently

This ExecPlan is a living document maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

Concurrent container launches for the same repository should share an overlapping Git mirror refresh, then build their own snapshots concurrently. A snapshot is a private bare Git repository with independent refs and hardlinked immutable object files. History-limited launches convert it into a bundle containing only the selected commits and their ancestors. Unrelated dashboard updates must not redisplay an unchanged lifecycle notice. Issue: https://github.com/BrokkAi/mjolnir/issues/1388.

## Progress

- [x] (2026-10-28) Diagnose six cache lock timeouts during a preparation taking 85 seconds; identify unconditional lifecycle notice replay.
- [x] (2026-10-28) Inspect existing Git cache, garbage collection, subprocess, and dashboard ownership paths; claim issue #1388.
- [x] (2026-10-28) Shorten the shared mirror critical section and share overlapping refreshes.
- [x] (2026-10-28) Retain each remote lifecycle's identity and last notice together so unchanged notices are not redisplayed.
- [x] (2026-10-28) Add and run isolated concurrency, eviction, failure cleanup, and notification replay regressions; the real-Git test and both lifecycle-notice tests pass.
- [x] (2026-10-28) Run touched-crate suites, workspace clippy, and formatting; review the diff and merge upstream cleanly.
- [x] (2026-10-28) Commit the final host-only pin correction and push all work to origin/master (`bcbf7dd75`).

## Surprises & Discoveries

The existing shell script holds one host-side directory lock per repository through network refresh, local cloning, history bundle packing, and recursive chmod. It gives competing callers only approximately five seconds. Cache garbage collection uses the same repository lock, so moving the read outside the lock without pinning all its objects would race eviction. The dashboard already retains remote operation identities; extending that state with the observed notice avoids creating a second owner for operation identity.

## Decision Log

Use Git's existing local mirror clone under the shared lock to pin a private immutable object snapshot. Release the lock before history filtering, bundle packing, publication, and permissions. This preserves the existing cache layout and permits garbage collection to remove a shared mirror after every reader owns its files. Reuse the refresh completed since a waiter observed the mirror's generation; use an opaque unique preparation token for generations so eviction and recreation cannot produce the same generation. Keep a bounded, cancellable host-side lock wait long enough for an ordinary network refresh. Preserve compatibility with the existing directory-lock protocol across daemon upgrades. Date/author: 2026-10-28, Codex.

## Outcomes & Retrospective

Implementation is complete and focused regressions pass. The real-Git concurrency regression passed in 6.46 seconds, including a wait exceeding the former five-second limit, one refresh shared by both preparations, eviction during a paused reader, exact cutoff history, and failure cleanup. The lifecycle-notice filter passed both selected tests. Upstream merged cleanly and the first full controller/CLI test run passed. Final review moved temporary full-history pins out of session-mounted subtrees; the strengthened concurrency regression also checks this boundary. Final validation is complete: the corrected concurrency regression passes; the final controller suite passed 2,188 tests with one timing failure in the existing local-bare hung-launch test (5.37 seconds against a five-second bound), which passed alone in 1.62 seconds. The earlier full controller suite passed all 2,189 tests, and the unchanged CLI suite and isolated integrations passed. Workspace `cargo clippy --all-targets -- -D warnings` and formatting passed. No live daemon or session was restarted for validation.

## Context and Orientation

`mj-controller/src/controller/git_cache.rs` runs shell scripts on the container host through `CacheHost` and `CommandExecutor`. `prepare_repository` refreshes `mirrors/<repository-key>/repo.git` and creates a session-specific snapshot. `CACHE_GC_SCRIPT` evicts mirrors only while holding their repository lock. `mj-cli/src/dashboard/drains.rs::apply_runtime_lifecycles` installs lifecycle views received from the daemon and currently writes every retained notice to the notice bar on each update. `mj-cli/src/dashboard.rs` holds the surface's observed lifecycle identities. `mj-chat::chat::Notices` is the shared notice bar and history.

## Plan of Work

First, revise the preparation script to capture a successful refresh generation before acquiring the shared lock. Under the lock, refresh only when no overlapping caller has published a new generation. Clone the mirror locally into a private temporary repository in an unmounted host preparation directory beside the mirror, then release the shared lock exactly once. Perform subsequent work only against this private repository; remove it on success, failure, and handled cancellation. Keep partial files distinct from the final snapshot and do not expose full history in a completed history-limited session's mounted directory.

Next, extend the existing UI lifecycle observation to retain operation identity and notice together. A new operation or changed notice is reported once; an unchanged retained notice never replaces a newer notice or reappears after dismissal. Forget observations when operations disappear.

Finally, exercise the real shell and Git protocol with isolated temporary repositories and explicit synchronization markers. Hold one session's bundle phase while another snapshot finishes and garbage collection removes the shared mirror; both resulting bundles must remain readable and must exclude later history. Exercise overlapping refreshes and a wait exceeding the former five-second limit. Use a real dashboard notice bar to verify replay suppression and new operation/notice handling.

## Concrete Steps

Work in the current repository and branch. Use the configured mbx Cargo wrapper without changing target storage. Run focused regressions first outside the restricted sandbox:

    cargo test -p brokk-mj-controller controller::git_cache::tests
    cargo test -p brokk-mjolnir lifecycle_notice

After the last change, run the full touched suites and workspace lint once:

    cargo test -p brokk-mj-controller -p brokk-mjolnir
    cargo clippy --all-targets -- -D warnings

Tests must keep their own temporary filesystem and isolated data directories. Any manual binary invocation must name `--instance clone-cache-1388`. Stage only changed files, commit on the current branch, and push HEAD to origin/master. If upstream moved, fetch and merge origin/master without rebasing.

## Validation and Acceptance

The concurrency regression must demonstrate that a blocked bundle no longer blocks a second snapshot of the same repository, and that deleting the shared mirror cannot break the first reader. Overlapping preparations must perform one mirror refresh rather than one refresh per waiter. A failed bundle must leave no private full-history repository behind. A retained lifecycle warning must not replace a later notice, return after dismissal, or fill history on unrelated updates; a changed warning and a new operation must still report. Existing cutoff fidelity, hardlink, token secrecy, and cache eviction tests must pass. Required touched-crate suites and workspace clippy must pass.

## Idempotence and Recovery

Preparation publishes only completed snapshots and removes its own temporary paths on handled failures. Repository directory locking remains interoperable with older daemons. All regressions use temporary repositories and shared subprocess helpers. Do not test against the default instance or alter live cache files. Follow-up runs can repeat the same tests without cleanup of live state.

## Artifacts and Notes

Original evidence: six failed preparations took approximately 5.3–5.6 seconds; an overlapping successful preparation took 85.1 seconds. The TUI unconditionally called `set_notice` for every retained lifecycle notice.

## Interfaces and Dependencies

No new crates, runtime host dependencies, protocol revisions, or store migrations are required. Continue to use `git`, POSIX `sh`, the existing per-repository directory lock, `CacheHost::git_shell_command`, and `CommandExecutor`. Keep private snapshot ownership and lock ownership explicit in the shell cleanup path. Preserve the existing user-visible direct-clone notice for actual preparation failures.

Plan created 2026-10-28 after diagnosis and repository inspection.

Plan updated 2026-10-28: implemented private object pins, successful-refresh generation sharing, a five-minute bounded cancellable wait, and a single UI lifecycle observation map; added deterministic real-Git and notice replay regressions. Shell syntax checks pass; targeted Rust tests are compiling.

Plan updated 2026-10-28: focused regressions pass; checkpoint the coherent fix before merging the concurrent upstream release and running final validation. The initial test closure ownership error was corrected before the successful regression run.

Plan updated 2026-10-28: final review placed the full-history pin in a private preparation directory beside the shared mirror. Keeping that workspace owned until cleanup also avoids deleting a vacated temporary path after publication. The first full merged controller/CLI suites passed; rerun them for this final boundary correction, then run workspace clippy.

Plan updated 2026-10-28: final code and regression validation is complete. The sole final-suite failure was an existing launch-cancellation timing assertion, unrelated to the container Git cache, and passed on its focused rerun; no timing bounds were changed. Required workspace clippy passed in the dev profile. Commit and push remain.

Plan completed 2026-10-28: implementation commits `6ffe73392` and `bcbf7dd75`, with upstream merge `cab7a32f4`, were pushed successfully to origin/master. The shared mirror refresh and pin are the only serialized work; session-specific bundle work runs independently, and the TUI reports retained notices only on transitions. The fix is committed and published to the repository; no installation or live restart was performed.
