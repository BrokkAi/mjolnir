# Remove wasted daemon update work

This ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective during implementation.

## Purpose / Big Picture

The daemon is Mjolnir's background controller. Streaming a few lines must not make it copy every conversation, repeatedly read unchanged wait state, query every child's progress, or parse the same provider file for every session. Preserve all existing visible behavior while making work proportional to changed inputs. Validate in named isolated instances; do not install a new binary or replace the live daemon.

## Progress

- [x] (2026-10-02) Profiled the installed daemon and confirmed all four expensive paths in current source.
- [x] (2026-10-02) User selected all four paths and authorized implementation, commit, and push to origin/master.
- [x] (2026-10-02) Capture a dev baseline executable and isolated measurement: eight retained conversations and waits, one 20 Hz stream, 31.30% CPU over ten seconds.
- [x] (2026-10-02) Share browser conversations and select deltas before copying.
- [x] (2026-10-02) Share committed records and filter waits using session-specific durable changes.
- [x] (2026-10-02) Publish compact durable turn facts and remove child-progress polling.
- [x] (2026-10-02) Publish credential inputs only on relevant changes; refresh provider interpretation in background.
- [x] (2026-10-02) Add behavior regressions and pass the complete dev suite with four test threads.
- [x] (2026-10-02) Observe staged credential homes off the event loop, including filesystem-only transitions.
- [ ] Complete the final profile and performance comparison, merge concurrent upstream commits, validate, and push.

## Surprises & Discoveries

The live profile measured 1.58–2.36 daemon CPU cores, versus 0.18–0.23 for the terminal. Approximately 46% of valid daemon user-space leaf samples were allocation/free/copy/compare functions. Caller snapshots showed browser map clones, whole CommittedState clones from startup status, repeated child-progress SQLite reads, and provider TOML parsing from delegation policy. Sampling cannot assign exact inclusive percentages to these paths.

The existing wait regression counts only turn-state reads. Startup and report accessors run before its unchanged-input filter, so the test misses the observed work. Existing persistent session maps do not make ordinary mount-history maps or the separate browser map cheap to clone.

The workspace is shared with other contributors. At implementation start master is clean and ahead of origin/master by two existing commits, with HEAD 3eefab64. Preserve their changes and do not change branches.

## Decision Log

Decision: Cover all four measured paths. Rationale: the user explicitly selected this scope. Date/author: 2026-10-02, user and Codex.

Decision: Keep wire formats and durable schema unchanged. Rationale: shared immutable publications and connection-local observation solve these costs without an upgrade migration. Date/author: 2026-10-02, Codex.

Decision: Keep the database writer and runtime owner authoritative. Rationale: independent caches must not decide session lifecycle eligibility or whether a turn finished. Date/author: 2026-10-02, Codex.

Decision: Convert mount history and container-size preferences to existing SnapshotMap as well. Rationale: sharing CommittedState would otherwise move the history copy into every writer publication; transparent serialization preserves the existing JSON shape. Date/author: 2026-10-02, Codex.

Decision: Child wait output reads carry the observed completed turn’s span and batch text reads in one transaction at response time. Rationale: preserve timeout partial text and keep a later resume notice or turn from replacing the observed answer. Date/author: 2026-10-02, Codex.

Decision: Cache the complete configuration for delegation adoption, while credential targets have their own background publication. Rationale: subagent eligibility changes also update the catalog even when profiles are unchanged. Date/author: 2026-10-02, Codex.

Decision: Publish pollable worker inputs as an Arc owned by RuntimeStateOwner, keyed by immutable record differences, configuration, moves, and the owner’s eligible worker IDs. Rationale: the initial 32-session comparison still spent work rebuilding copied input records on every unrelated update; consumers and admission now compare the same publication identity. No second lifecycle predicate or manual invalidation flag is introduced. Date/author: 2026-10-02, Codex.

Decision: Observe staged-home eligibility in the existing 500 ms background refresh, alongside provider files. Rationale: replacing a legacy profile symlink with a directory changes credential eligibility without changing a durable session record. The owner still decides session eligibility, and stale prepared records cannot install after ownership changes. Date/author: 2026-10-02, Codex.

## Context and Orientation

`mj-controller/src/server_runtime/run.rs` owns browser conversations and feeds `server::ServerOptions`; `server_runtime/projection.rs` supervises background transcript conversion. `mj-core/src/snapshot_map.rs` already shares tree branches and values between snapshots. HTTP conversation deltas are built in `mj-controller/src/server/handlers.rs`.

`mj-controller/src/database/writer.rs` serializes writes and publishes durable records before replying. `database/committed.rs` uses temporary connection-local triggers to collect affected keys, then reads actual committed differences, including rollback handling. `daemon/owner.rs` joins these records with lifecycle ownership. The API's wait implementation is `server/api/wait.rs`; child listing and waits are in `server_runtime/api.rs`. Workers retain agent state across controller restarts.

`daemon/delegation/policy.rs` currently rebuilds credential targets on every global revision. `pollers/worker_targets.rs` calls `HarnessProfile::auth_scheme` per eligible session, which reads Codex provider configuration. `worker_client/credential_sync.rs` owns reconciliation and lifecycle admission. The manager target refresher in `daemon/process.rs` already checks external configuration every 500 ms in background work.

## Plan of Work

First replace the browser conversation map with SnapshotMap, preserving the public BrowserTranscript response. Publish its watch channel only when a conversation changes or is removed. Select response delta entries before cloning them, preserving presentation-key mismatch, window reset, and transitioning-session behavior. Keep background projection generation checks intact.

Second put CommittedState behind Arc in its watch publication and runtime owner. Accessors select one session without copying State. Add session-specific durable change tokens owned by the writer, changed only when startup, relationship/report, or compact turn inputs actually change. Wait compares this cheap token and existing viewer session facts before calling expensive accessors, while keeping actor changes and explicit deadlines effective.

Third extend the existing committed publication with compact execution, active-turn, and completed-turn records. Bootstrap from materialized sessions, observe projection mutations through temporary triggers, and remove entries on deletion. Reads and notifications must describe actual commits, not attempted writes. Child progress is derived from one coherent shared committed snapshot and the runtime owner's lifecycle facts, using one shared observation builder. Replace 250 ms child polling with notifications from parent/child actors and durable publications, filtered to relevant sessions, with explicit wait and handback-grace deadlines. Read detailed completed/failure messages only when needed and retain results by completed-turn identity; never load full history just to observe progress.

Fourth publish credential-target inputs from the runtime owner only when profile/configuration, session eligibility, staged-home, target, or lifecycle ownership changes. Prepare command specifications and provider interpretation in supervised background work. Revalidate generation under the owner before installation. Extend the existing 500 ms background configuration refresh to read each relevant provider file once, parsing only changed bytes. Credential admission revalidates the owner's current inputs rather than reloading Config and State for every worker. Keep periodic reconciliation, authentication triggers, per-profile serialization, cancellation, and error reporting.

## Milestones

Browser sharing is an independently testable first checkpoint: unchanged conversations retain identity, operational-only changes do not notify the conversation feed, and all HTTP delta behavior stays the same.

Shared durable publications and child observations form the second checkpoint. Commit/rollback tests prove authoritative publication, API tests prove unrelated updates do no detailed reads, and child tests cover startup, handback, failed login, parked/resumed actors, pending inputs, and deadline expiry without polling.

Credential changes form the third checkpoint. Tests prove unrelated transcript updates do no extra provider reads, shared profiles are interpreted once, external provider edits are detected, and lifecycle changes cannot install or use a stale target.

The final checkpoint runs complete validation and matched isolated CPU measurements, then commits all task changes and pushes the current branch to origin/master.

## Concrete Steps

Work from `/home/jonathan/Projects/mjolnir`. Use normal Cargo/mbx storage; do not redirect target. Preserve the pre-change dev executable in existing build storage. Run focused controller and API tests outside the sandbox during each milestone, then:

    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    git diff --check

Every cargo test runs with elevated sandbox permissions. Runtime invocations use `--instance cpu-update-test` and isolated MJ_CONFIG_DIR/MJ_DATA_DIR. The existing reliability lab provides deterministic fake harnesses and process-group cleanup. Build a performance scenario around many retained conversations, idle waiters, children sharing a profile, and one streaming session. Measure baseline and changed dev executables with the same input, warmup, and duration. Keep captures and validation results here as work proceeds.

## Validation and Acceptance

Behavior tests must prove no unchanged transcript deep-copy, detailed wait-state read, child-progress database read, or extra provider parse from unrelated publications. Include retained text exceeding 64 KB. Preserve cursor/reset/removal behavior, named turn outcomes, handoff, startup failure, pending questions, handback grace, deferred inputs, parked/resumed children, and failure details. Rollback publishes nothing and writer replies see committed publications. Credential tests use controlled preparation to prove stale results are rejected and admitted sync validates current ownership.

Matched isolated measurement targets at least 50% lower daemon CPU without adding UI latency. Timing thresholds are measured evidence, not flaky automated assertions; deterministic work counts guard against regressions. Required dev tests, clippy, formatting, and diff checks must pass. Keep isolated upgrade regressions intact.

## Idempotence and Recovery

No installation, live migration, or live daemon restart is authorized for validation. Tests and captures use named isolated instances. Stop fixture process groups before deleting their files. Commit only this task's changed files on current master, without staging unrelated changes. Push origin/master after validation; never force-push or rebase.

## Artifacts and Notes

The live diagnostic report is `/tmp/mj-live-cpu-20261002/findings.md`, with pidstat, perf, debugger, and syscall captures beside it. Profiling was read-only; all tools detached. Preserve its distinction between different live workloads and controlled before/after measurements.

## Interfaces and Dependencies

Use existing SnapshotMap, Arc, Tokio watch channels and supervised task/cancellation patterns. Browser map watch types change internally while HTTP JSON stays identical. CommittedState becomes a shared immutable publication with compact per-session turn facts and change tokens. Extend the internal SubagentBackend observation interface for cheap wait-input tokens. Add no workspace crate or schema revision.

## Outcomes & Retrospective

Implementation is complete; validation is in progress. The baseline executable is target/debug/mj-cpu-baseline, built before source edits from HEAD 3eefab64, SHA256 1f6441e2334c0a39dc06476a053b6197987204c0c559c8b9bb007461be52a03c. Baseline CPU: 28.50% user + 2.80% system = 31.30%, 240.20 minor faults/second, 197091328 bytes RSS. Evidence: target/reliability-artifacts/daemon-update-cpu-seed-10202-1049149/cpu.json. The fixture uses explicit --instance, isolated config/data, a matching dev worker, retained text over 64 KB, eight API waiters, and one 20 Hz stream. It stops owned processes before deleting runtime files. The optional desktop is not a default workspace member and requires unavailable GTK dependencies; use the specified default-member checks, without --workspace. The eight-session diagnostic comparison was 31.30% to 29.45% CPU and 240.20 to 30.35 minor faults/second. Scaling to 32 sessions gave a 100.90% baseline versus 72.36% for the first implementation, with RSS 644358144 versus 320577536 bytes. This did not meet the 50% CPU target, motivating shared worker-input publication. Baseline evidence: target/reliability-artifacts/daemon-update-cpu-seed-10202-1261506/cpu.json. Initial optimized evidence: target/reliability-artifacts/daemon-update-cpu-seed-10202-1507335/cpu.json. The scaled comparison measures the browser, API wait, and credential paths; child progress is verified separately by durable-publication and notification regressions. Record final results and limitations before completion.

The first unrestricted-concurrency dev test run passed 2168 controller tests but failed two existing timing regressions under host load and the new compact-turn fixture, which incorrectly inserted projections already created by save_session. A bounded eight-thread rerun passed both timing regressions (2171 controller passes); the fixture is corrected to use those existing rows and treat bootstrap wait tokens as zero. Subsequent checks run against the final corrected source.

The complete dev suite passed with `RUST_TEST_THREADS=4 cargo test --quiet`, including the 2172 controller tests, worker restart integration tests, CLI PTY tests, and isolated upgrade regressions. Clippy passed in the dev profile. The extra staging regression passed separately after the full suite. An earlier eight-thread run hit an existing eight-second shell timeout under host load; its focused rerun and the subsequent complete four-thread suite both passed.

At 86 sessions, the baseline used 239.10% CPU, 2993.38 minor faults/s and 1900670976 bytes RSS, at 19.93 stream ordinals/s. The shared-input implementation used 159.53% CPU, 295.83 minor faults/s and 515530752 bytes RSS, at 19.00 stream ordinals/s. All 86 waits remained pending. Evidence directories end in 1763310 and 2035258 respectively. This is a 33.28% CPU reduction and a 72.88% memory reduction; the 50% CPU target is not yet achieved. Follow-up profiling in an isolated 32-session fixture found remaining serialization/deserialization and allocation work, but 4096-byte DWARF captures were mostly truncated. Do not present their call chains as reliable inclusive attribution.

Upstream advanced independently to 540c9202 with three commits while this task was running. Preserve them with a merge on current master after the implementation checkpoint, then validate the combined result. No rebase, branch change, or force push.

Revision: Created on 2026-10-02 from the user's accepted four-path CPU plan and the installed-daemon profile.

Revision: Updated on 2026-10-02 with final behavior checks, filesystem-only staging observation, the 86-session CPU target miss, and the concurrent upstream merge requirement.
