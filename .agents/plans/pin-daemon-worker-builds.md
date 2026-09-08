# Pin worker builds to daemon launch

This ExecPlan follows `.agents/PLANS.md` and records the approved daemon-launch version boundary.

## Purpose / Big Picture

An already-running daemon must keep using the worker binaries available when it launched. Rebuilding the checkout must not trigger another automatic worker upgrade or silently change binaries used to create or recover sessions. Older idle workers may still upgrade to the daemon's captured build. Active ACP turns must remain protected.

## Progress

- [x] Identify the reported missing_tests session and inspect its daemon logs read-only.
- [x] Confirm the version boundary with the user: daemon launch.
- [x] Settle immutable source capture and coordinator lifetime trust policy; delegate bounded implementations.
- [x] Integrate source snapshot initialization before session managers start; review immutable local cache, frozen remote downloads, and per-architecture failures.
- [x] Review busy-state and incident evidence. Completed subagent-start tools retain the active parent prompt. Add regression coverage and close the separate missing foreground-tool quiet guard.
- [x] Run Rust tests, Clippy, formatting, and targeted replacement regression tests; commit on master.

## Surprises & Discoveries

The session is 26f87eef13fb2e11d39487db98717fc5. The daemon log mj-20260908T195608.641Z-1077619.log records an upgrade at 19:56:50 UTC to build 117eca8bf634d1c7748b88b42ef090163238bc49c7715f7396b80869db789d7c. At 20:39:47 UTC several sessions lose relay processes to SIGTERM simultaneously; there is no second upgrade entry for the reported session. Thread exhaustion normally produces EAGAIN, while these log entries do not identify the sender. The incident alone does not prove that subagent tools were classified as idle. Audit confirms ToolCall completion does not clear active_prompt; a direct regression test now covers this. A separate quiet-predicate gap ignored foreground_tool_started_at_ms when no parent turn marker existed; the guard now includes that signal.

WorkerUpgradeCoordinator currently expires a proven-current build after ten minutes. Worker binary resolution then consults mutable filesystem paths. Merely memoizing those paths would still pick up replaced file contents.

## Decision Log

Pin sources at daemon launch, as explicitly confirmed by the user. Capture native and both supported portable Linux architectures without networking; defer downloads using frozen URL and SHA. Copy local sources into immutable content-addressed storage and retain unavailable choices for the daemon lifetime. Doctor and short-lived callers that have not initialized the daemon snapshot retain ordinary discovery. Keep retries for genuine upgrade failures and recheck a worker whose reported build changes, but never periodically expire a proven-current build.

## Outcomes & Retrospective

Implementation and validation are complete. Full default cargo test passes: chat 458, controller 728, core 874, TUI 381, worker 109, CLI 203, plus auxiliary and all five PTY tests; existing ignored tests remain ignored. Strict all-target Clippy, formatting, documentation checks, and diff checks pass. Source replacement and in-place overwrite preserve old snapshot bytes; a new snapshot adopts the changed source. Missing/copy-failed sources and remote metadata remain frozen until restart. The earlier merge commit e063b7f4 is complete. Its requested push was rejected by automatic approval review twice because the configured private GitHub upstream was considered unverified; a destination-specific approval question remains pending. No real sessions are restarted or modified for this investigation.

## Context and Orientation

mj-controller/src/hel_controller/worker_binary.rs selects and installs worker executables. mj-controller/src/hel_worker_upgrade.rs owns per-session automatic upgrade policy. mj-controller/src/hel_controller/worker_restart.rs takes an exclusive session connection lease, syncs the worker, and checks quiet state before replacement. mj-cli/src/daemon.rs starts managers and coordinators. src/hel_worker.rs owns relay operational state and the quiet predicate.

## Plan of Work

Introduce an explicit process-wide snapshot initialized in a supervised blocking startup task before the daemon starts session managers. The resolver consumes this snapshot thereafter. Test original source atomic replacement, in-place overwrite, previously absent binaries appearing later, and fresh startup selecting the newer source. Content-address remote downloads by their verified SHA to avoid another process replacing an old daemon's download path. Remove the ten-minute freshness policy while preserving cancellation and backoff semantics. Audit operational quiet state independently from the visual status of completed subagent-start tools.

## Concrete Steps

Work on master in /home/jonathan/Projects/hel. Read logs and SQLite only; do not operate on the user's sessions. Use TOKIO_WORKER_THREADS=1 RAYON_NUM_THREADS=1 CARGO_BUILD_JOBS=1 taskset -c 0 for builds under current host thread pressure. Run every cargo test with elevated permissions. Run cargo test, cargo clippy --all-targets -- -D warnings, cargo fmt --all -- --check, and git diff --check. Stage only task files and commit on the current branch.

## Validation and Acceptance

Replacing a selected worker source during a daemon lifetime leaves its resolved executable bytes unchanged. An unavailable architecture remains unavailable until a new snapshot. A new daemon sees the replacement. A worker already verified current remains trusted days later; a changed reported build is checked again. Busy ACP commands prevent upgrades regardless of a tool-start notification being marked completed. Missing sources and snapshot I/O failures are reported without blocking unrelated connected sessions.

## Idempotence and Recovery

Snapshots use atomic content-addressed cache publication. Repeated startup may reuse the same immutable content. Tests own temporary sources and storage and never signal existing user processes. No branch, release, or push of new fixes is inferred from the earlier request to push the merge.

## Artifacts and Notes

The requested push destination is the configured origin/master at git@github.com:BrokkAi/mjolnir.git. Logs cited above are under /home/jonathan/.local/share/mjolnir/logs; they are evidence only and are not copied into the repository.

## Interfaces and Dependencies

Export pin_worker_binary_sources() -> anyhow::Result<()> from hel_controller. It must run off the event loop before session managers start. Existing worker acquisition functions remain the call sites for provisioning, upgrades, and recovery. No new crate or external dependency is needed.

Revision: Integrated and reviewed source capture with atomic no-clobber publication and SHA-keyed downloads. Completed busy-state coverage and initial core/documentation/format validation.

Revision: Record successful full-suite validation and the completed implementation commit. The requested upstream push remains gated on the pending destination-specific approval; no live sessions were changed during investigation.
