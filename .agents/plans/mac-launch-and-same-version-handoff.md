# Verify Mac launch paths and replace obsolete same-version daemons

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Finish the accessible Mac launch verification in #1135 and fix #1184 so a client
installed from a changed executable replaces an obsolete daemon even when the
release version, wire protocol and database revision did not change. The daemon
owns session records; independent worker processes own agent turns. Replacement
must use the existing admission gate, preserve workers and accepted operations,
and continue to reuse a daemon already running the invoking build.

## Progress

- [x] (2026-09-30) Claim both issues and inspect the previous Mac evidence.
- [x] (2026-09-30) Confirm automatic startup misses executable identity; add the
  existing identity probe under the startup lock and extend the isolated
  concurrent-client regression to unchanged version, protocol and schema.
- [x] (2026-09-30) Linux startup suite: 12 passed; identity tests: 3 passed.
  Formatting and diff checks passed.
- [x] (2026-09-30) Commit #1184 separately as `79f2ad0a`.
- [x] (2026-09-30) Build native CLI/worker in a disposable Mac source directory.
- [x] (2026-09-30) Mac startup: 11 passed; checkpoint: 69 passed, 2 ignored;
  terminal PTY: 11 passed. Real Codex handoff, checkpoint/suspend/resume, staged
  login and viewer durable logout passed; kitty custom prefix and packaged
  Docker smoke passed. Fix the discovered legacy checkpoint home-alias defect.
- [x] (2026-09-30) Commit #1135 fix and acceptance evidence separately as
  `f896b3a4`; merge upstream cleanly as `adb1a4d3`.
- [x] (2026-09-30) Full dev-profile `cargo test` passed on `adb1a4d3`;
  all-targets Clippy passed. Web unit tests: 70 passed; documentation check:
  zero diagnostics. Push `adb1a4d3` to origin/master succeeded.
- [x] (2026-09-30) Publish finding and verification comments, clear both work
  labels, and confirm #1184 closed. #1135 remains open for unexercised missions.

## Surprises & Discoveries

The Mac SSH host is `jonathans-macbook-air`, not the configured target name
`macbook`. It runs Apple silicon macOS 15.7.3 with Homebrew mj 2.21.0 and has the
2.20.0 formula retained. Earlier evidence in #1135 passed kitty keyboard/image
and title probes but did not complete the Homebrew active-turn upgrade. Apple
container requires a newer macOS. Terminal.app and iTerm2 automation previously
lacked assistive-access permission. Those limits are evidence gaps, not passes.

Two unrelated dirty files in the local checkout contain Move timing instrumentation
and must remain untouched: `mj-controller/src/controller/move_session.rs` and
`mj-controller/src/daemon/session_move.rs`.

A released 2.21.0-to-2.24.0 handoff preserved the running turn but checkpoint
failed because the old memory root and new compatibility link used different
spellings of one physical directory. The worker launch file owns the source
spelling. The final fixed Mac run changed daemon PID in 0.813 seconds, preserved
worker PID and accepted turn 31, then passed checkpoint and recovery.

## Decision Log

- Decision: Use `mj-client/src/executable.rs::process_runs_this_executable` in
  `mj-cli/src/daemon.rs::prepare_existing_daemon`, after a successful ping and store compatibility check,
  only for equal release versions.
  Rationale: Identity already has shared Linux/macOS implementations used by
  explicit restart and doctor. Newer releases retain the existing downgrade
  protection; changed builds use `replace_daemon`, never forced termination.
  Date/Author: 2026-09-30, Codex.
- Decision: Use named instances and disposable repositories on the Mac, preserve
  the default store and existing login, and record inaccessible GUI/OS missions.
  Rationale: Verification must not disrupt live work or mint replacement login
  tokens merely to exercise a mission card.
  Date/Author: 2026-09-30, Codex.

- Decision: Share the installed-home parser in `mj-core/src/worker_launch.rs`
  and resolve checkpoint source against that owner in both the checkpoint
  collector and local controller export. Verify physical identity before using
  the installed spelling; retain the memory containment check.
  Rationale: The compatible old exporter remains in use across daemon handoff,
  so a collector-only fix cannot repair existing workers. This corrects the
  source rather than weakening containment or forcing worker replacement.
  Date/Author: 2026-09-30, Codex.

## Outcomes & Retrospective

Both implementations pass focused checks and real Mac acceptance. The launch
verification found a released upgrade checkpoint failure that required resolving
the installed worker home before invoking even a compatible legacy exporter.
The evidence report records five passing real-harness checks and remaining
manual/authentication/OS missions. Full integrated validation passed and `adb1a4d3` was pushed to origin/master.
#1135 remains open for its unexercised missions. All isolated Mac daemons and
workers were cleaned up; Docker Desktop was restored to its stopped state.

## Context and Orientation

`mj-cli/src/daemon.rs` serializes client startup using `daemon-start.lock` and
routes replacement through `replace_daemon`. That function asks the old daemon
to drain its own work; worker turns survive. The release comparison refuses a
semantic-version downgrade. `mj-client/src/executable.rs` answers whether two
processes run the same executable using OS file identity; an unsupported OS
answers unknown. `mj-cli/tests/daemon_startup.rs` runs a separate management
fixture executable holding an isolated writer lock, starts three concurrent
clients, and proves they replace it once and reuse the resulting daemon.

The Mac already has a separate previous verification checkout at
`~/Projects/mjolnir-tf-2026-09-29` and evidence under
`~/mj-campaign/track-m-1135`. Do not overwrite either. Transfer a tracked source
snapshot plus this task's edits to `~/Projects/mjolnir-tier1135-20260930` and use
normal Cargo storage there. Do not change branches, redirect targets or alter
the Linux mbx Cargo shim.

## Plan of Work

First validate the identity change with the daemon startup integration suite and
existing executable-identity tests. Commit only the implementation, regression
and this plan. Build the same source on the Mac. Run its isolated startup suite
and prepare a `tier1135` instance with a disposable project and the existing
Codex login copied into an isolated profile; Claude currently reports signed out. Exercise replacement with a running turn and record daemon and
worker identities, the unique reply, and follow-up usability. Retest accessible
setup, doctor and checkpoint/staged-home paths. Use previous terminal evidence
where valid and record fresh evidence for supported automation. Store the
result in `.agents/docs/mac-launch-verification-2026-09-30.md`, with exact
versions, commands, findings and remaining manual checks. Fix concrete failures
found in this scope, with focused regression tests when appropriate.

## Concrete Steps

From `/home/jonathan/Projects/mjolnir2`, run the normal mbx-wrapped commands outside
the restricted sandbox:

    cargo test -p brokk-mjolnir --test daemon_startup
    cargo test -p mj-client executable::tests
    cargo fmt --all -- --check

Resolve package names from Cargo.toml if needed. On `jonathans-macbook-air`, add
`$HOME/.cargo/bin:/opt/homebrew/bin:$HOME/.local/bin:/usr/local/bin` to PATH and
run native dev-profile Cargo builds/tests in the disposable snapshot directory.
Every application invocation uses `--instance tier1135` (or its explicitly
isolated automated fixture directories). Preserve logs in the named evidence
directory. Stop test workers and daemons before removing any working files.

## Milestones

The first milestone proves three clients observing an obsolete same-version
daemon safely converge on one replacement and subsequent clients reuse it, with
no schema change. Existing newer-version compatibility tests must keep passing.

The second milestone provides fresh Mac evidence for a running turn surviving
handoff and normal follow-up operations. Additional mission checks establish
what was actually exercised and what needs a newer OS or interactive access.

The final milestone commits each coherent change separately, completes full
dev-profile `cargo test` and `cargo clippy --all-targets -- -D warnings`, and
pushes the current branch to its configured upstream under the user's standing
push authorization. Report issue disposition accurately.

## Validation and Acceptance

The unchanged-version/protocol/schema fixture must be replaced automatically,
with three successful concurrent clients and a stable daemon PID afterward.
The newer-compatible-daemon regression must preserve its PID and future data. A different same-version daemon with an incompatible store
must retain its PID while the client refuses.
The Mac must retain the worker and finish exactly one real turn across handoff;
the session must then answer another prompt, checkpoint and recover normally.
Never describe an unexercised terminal, credential replacement, desktop GUI or
Apple-container mission as passed.

## Idempotence and Recovery

All test stores and repositories are isolated. Existing installs, live default
stores and credentials remain usable. If cleanup fails, keep evidence and stop
the exact owning process group before deleting files. Unsupported host features
remain explicit gaps on #1135 rather than being silently bypassed.

## Artifacts and Notes

The implementation reuses OS executable identity and the existing serialized
safe replacement path. The regression adds the missing unchanged-schema case
to the concurrent-client behavior test rather than asserting helper internals.

## Interfaces and Dependencies

No new crate or protocol/database revision is required. The existing
`process_runs_this_executable(pid: u32) -> Result<Option<bool>>` runs in
`tokio::task::spawn_blocking`, so process inspection never blocks a UI loop.

Revision note (2026-09-30): Created after confirming the automatic identity gap
and available Mac target; includes prior mission limits and isolated validation.

Revision note (2026-09-30): Put executable replacement after the store read
compatibility check and add a refusal regression; focused Linux checks pass.

Revision note (2026-09-30): Record real Mac acceptance and the released legacy
checkpoint-home defect; preserve manual mission limits in the evidence report.

Revision note (2026-09-30): Full validation passed after an unrelated two-second
chat image-delivery test timeout on the first run. The same full-suite binary
passed that test directly in 0.04 seconds, its Cargo-focused recheck passed,
and the full rerun passed. No source change was required. Publication succeeded.

Publication evidence: #1135 finding comment is
`https://github.com/BrokkAi/mjolnir/issues/1135#issuecomment-5920101253`; the
verification comment is
`https://github.com/BrokkAi/mjolnir/issues/1135#issuecomment-5920101526`. #1184
closed through its fixing commit. The final documentation-only checkpoint
records these outcomes; it does not change the validated implementation.
