# Count each session container once in CPU rollups

This is a living ExecPlan maintained according to `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

The CPU report must accurately show a session's CPU consumption including its subagents. The user does not need to divide that consumption between subagents sharing a container. Today workers count their current process descendants. Background work can become a child of container init instead, and short-lived processes can disappear between observations. The report therefore misses real CPU time. Read the Linux kernel's cumulative CPU counter for the session container, and include that counter exactly once in session and machine rollups. Borrowed workers sharing that container also read only the cheap counter; rollups select one measurement per physical container.

This changes CPU accounting and its display, not container isolation or resource limits. Bare workers retain existing process-tree accounting. A bare worker inside a shared daemon container must never count that entire container. Independent child containers still contribute to their logical parent's rollup.

## Progress

- [x] (2026-11-10 01:56Z) Measured the discrepancy and traced host sampling, worker accounting, rollup, and rendering.
- [x] (2026-11-10 02:06Z) User selected accurate session totals including subagents; individual shared-container subagent attribution is unnecessary.
- [x] (2026-11-10 02:24Z) Implemented explicit accounting ownership, default legacy scope, and stop-before-launch-refresh upgrade ordering.
- [x] (2026-11-10 02:24Z) Implemented fixed-path cgroup counter reads for all container workers; 7 CPU tests and 1 relay CPU regression passed.
- [x] (2026-11-10 02:24Z) Added shared physical-scope deduplication and whole rendered rollup golden; focused CPU report tests and dashboard-dialog golden passed.
- [x] (2026-11-10 02:46Z) Full touched-crate validation covers 4,731 passing tests after two focused reruns; workspace Clippy and formatting pass. Rendered rollups, placement/wire compatibility and counter-read cost are verified.
- [x] (2026-11-10 03:12Z) Real container-only end-to-end regression passed with a handshake-controlled independent exec, two fresh sampling events, retained post-exit CPU and verified container cleanup.
- [x] (2026-11-10 03:12Z) Validated implementation and rendered-golden commits are reviewed; final test-driver updates and this evidence are prepared for remote delivery using the Git steps below.

## Surprises & Discoveries

The host sampler and Recent session report are separate. All 17 measured the reported host sessions used the correct 96-core denominator. A container consumed approximately 24 cores while its owning worker reported zero. That container also contained several subagent workers: applying its whole counter to each worker would multiply-count consumption.

The host has workloads whose session IDs are absent from this daemon's store. Not all of the host-versus-report discrepancy is necessarily an attribution defect. Per-process before/after sampling also omits short-lived tasks, so it cannot establish exact total attribution.

Reading 85 existing cgroup CPU counters on the reported host took median 4.286 milliseconds per pass, versus 0.190 milliseconds for one host `/proc/stat` read. Counter discovery through Podman took roughly 0.7 seconds and must not become recurring per-session engine commands. A cgroup is the kernel's resource-accounting group; its cumulative counter retains CPU time after a process exits or is adopted by another process.

`WorkerLaunchConfig` rejects unknown JSON fields. Adding launch metadata needs deliberate upgrade handling so a live older worker does not read an unsupported file. Existing sample readers must distinguish legacy process-tree samples from new container samples.

## Decision Log

Use the existing session-container boundary as the accounting owner. The user explicitly accepts rolling subagent CPU into the parent without further attribution. This avoids creating per-worker kernel groups or scanning process environments repeatedly. Date: 2026-11-10, parent agent with user clarification.

Keep actual measurement scope explicit and default old samples to process-tree scope. All container workers read the cheap cumulative counter, so measurements remain available when the owning session moves or stops. Rollups select one sample per physical container, prefer its active owning session, and use a deterministic remaining borrower if needed. Do not manufacture zero-percent samples to represent inclusion. Date: 2026-11-10, parent agent.

Resolve counter paths once, then read only the small counter file each interval. Report errors as unavailable CPU instead of silently switching to incomplete process-tree accounting. Preserve online-host-CPU normalization and existing Recent/hourly history. Date: 2026-11-10, parent agent.

## Outcomes & Retrospective

Diagnosis and a bounded counter-read measurement are complete. Implementation is committed as 24b81d888 and the menu golden follow-up as 48f039aa3. Production tests, Clippy, formatting, and source review are complete. The real isolated container exercise also passed. A current base64 reply fixture exceeds 64 KiB and passes through the shared Python relay decoder. Container cleanup was verified by checking the exact created resource no longer exists. No installation or live worker upgrade is authorized by this task; delivery is a validated source commit and push.

## Context and Orientation

`mj-worker/src/cpu_usage.rs` owns CPU sampling and the one-hour exponentially weighted average. `mj-worker/src/cpu_usage/linux.rs` currently scans `/proc` for worker descendants. `mj-core/src/cpu_usage.rs` defines the sample sent through the relay. `mj-core/src/worker_launch.rs` defines persisted launch JSON. Container locators in `mj-core/src/state.rs` carry `borrowed_from`, identifying shared environments. Controller launch and worker-upgrade paths under `mj-controller/src/controller/worker_binary/` choose placement and rebuild launch descriptions.

`mj-client/src/runtime_feed.rs` carries measurements to clients. `mj-tui/src/session_view.rs` derives shared CPU rollups used by rows and the report. `mj-tui/src/session_cpu_report.rs` currently sums each measured worker's own share. That summation must instead respect measured physical accounting scope (engine, host, and container identity), so a container and its borrowers contribute once. Legacy workers still emit process-tree samples until replaced, and must continue to render accurately as that older measurement type.

## Plan of Work

First define a single placement interpretation for CPU ownership, and add defaulted launch and sample metadata. All managed session containers select container accounting, including borrowed environments. Bare placement selects process-tree accounting even if the daemon itself runs in a container. Rebuild launch metadata during normal worker replacement, retaining safe ordering for old launch readers.

Implement Linux cgroup discovery using `/proc/self/cgroup` and `/proc/self/mountinfo`. Resolve namespace membership against the visible mount root with `Path` and `PathBuf`; decode mount path escapes without corrupting non-UTF-8 paths. Cgroup v2 reads `usage_usec` from `cpu.stat`; v1 CPU accounting reads `cpuacct.usage` nanoseconds. A private namespace and a host namespace may expose different paths to the same group. Never substitute a host-root counter for an inaccessible session counter. Non-Linux unsupported container accounting must fail visibly through the existing unavailable-data path.

Update the shared UI derivation to count each measured scope once. A measured container covers its borrowers, so those borrowers do not make the parent's total partial; another borrower can supply the measurement when the owning session moves or stops. Independent physical scopes among descendants are added once. Shared child rows say that their CPU is included in the owning session; parent summaries do not label the full container counter as the parent's own CPU. Machine totals and session rows must use the same ownership interpretation.

Extend existing CPU report rendering cases and isolated CPU end-to-end tooling. Exercise detached or sibling-exec work that exits before the next sample; cumulative accounting must retain its CPU. Exercise two workers sharing a container, independent child scopes, legacy worker samples, and missing measurements. Test upgrade metadata separately from live installation.

## Concrete Steps

Work in the repository root. Preserve normal Cargo/mbx configuration and target storage. Run all Cargo tests with elevated permissions. Use a separate named instance for every new runtime invocation, and keep existing isolated test directories.

During implementation run focused CPU tests in touched packages. After the final integration round run full suites of touched crates once and `cargo clippy --all-targets -- -D warnings`, then `cargo fmt --all -- --check`. Reuse successful checks unless later changes invalidate them. Save detailed logs in the responsible child report directory. Focused evidence: `cargo test -p brokk-mj-worker cpu_usage::` passed 7 tests, and `cargo test -p brokk-mj-worker cpu_usage_answers_from_the_sampler_without_journaling` passed 1 test. TUI CPU-report and dashboard-dialog golden checks passed. Final validation uses `cargo test -p brokk-mj-core -p brokk-mj-controller -p brokk-mj-tui -p brokk-mj-worker -p brokk-mj-checkpoint --no-fail-fast`. The focused isolated driver is `python3 tests/e2e/session_cpu_usage.py --container-only --mj <new-cli>`, with `MJ_WORKER_BINARY` pointing at the matching musl debug worker; both binary paths come from Cargo output and are resolved through any target symlink before the run, preserving normal mbx storage. Set MJ_RELIABILITY_ARTIFACTS outside target/ so build-cache cleanup cannot remove evidence.

Inspect `git diff --check`, stage only changed files, and commit on the current branch. Request the configured mj review, resolve consequential findings, and push to `origin/master`. If upstream moved, fetch and merge it, preserving the repository's no-rebase policy.

## Validation and Acceptance

An isolated parent session and borrowed subagent in one container must show the container's cumulative CPU contribution exactly once in the parent's rollup and in the machine total. The total must include work launched outside the parent's current process ancestry and retain CPU of exited tasks. Container workers must perform no recurring CPU process scan. Separate child containers must add to the logical parent's total. Legacy process-tree samples must remain distinguishable and avoid incorrect coverage claims.

Acceptance includes a whole rendered CPU report expectation, meaningful launch/upgrade compatibility evidence, a real isolated worker/container exercise where available, and a bounded measurement showing counter reads rather than engine calls or full process scans on the recurring path. Sampling failures must appear as unavailable or partial rather than as exact-looking zero.

## Idempotence and Recovery

Tests use named isolated instances and disposable containers, stop owning processes before deleting their files, and leave the host's default daemon and sessions alone. Source edits are reversible through their scoped commits. No database migration or cgroup-topology change is intended. Failed source initialization remains visible through the existing unavailable-data publication at sampling cadence; it must not cause a hot retry loop or substitute an incomplete source. A worker replacement resolves its source again.

## Artifacts and Notes

Local diagnosis and measurements are under `.mj/agents/e78911c9520a1919de3a4c0d5bccbfee/` and `.mj/agents/112fc2349165aac825e6c3e842ac4ebb/`. These are supporting local evidence rather than checked-in product documentation. An unused reader prototype is saved at `.mj/agents/d09c7c5ac2c3066d1acd4a80d6e606f5/cgroup-reader.patch` and was removed from the source tree before implementation resumed.

## Interfaces and Dependencies

Use the existing worker sampling task, relay CPU request, runtime feed, and shared TUI CPU derivation. Add no crate or external service. Core launch metadata selects CpuAccountingMode::ProcessTree or CpuAccountingMode::Container from authoritative placement. SessionCpuUsage.scope uses CpuSampleScope::ProcessTree (the backward-compatible default) or CpuSampleScope::Container. TargetLocator::shares_cpu_container compares physical scope in one location. WorkerLaunchConfig.cpu_accounting defaults to process-tree accounting; container placement explicitly selects the container mode.

Revision 2026-11-10: created after the user chose session totals with shared subagents rolled up; rejected per-subagent cgroup partitioning because it adds lifecycle work the user does not need.

Revision 2026-11-10: all container workers retain cheap counter sampling rather than disabling borrowed workers. This preserves source-container measurements after the owner moves or stops, without adding sampler-election lifecycle state. The UI chooses one sample per physical group.

Revision 2026-11-10: recorded completed implementation and focused regressions; broader dev-profile suites and real container validation are running before commit/review/push.

Revision 2026-11-10: full suites and lint are complete. Updated the legacy CPU test driver for required RuntimeChanges fields, base64 relay fragments and current report controls. Its separate bare-idle timing assertion remained at 1.0% and blocked a combined run; keep that assertion intact and validate the affected container path independently.

Revision 2026-11-10: final focused container acceptance passed after holding load through two fresh coverage events. The daemon reported container-scoped recent 17 permille while direct cumulative accounting rose 36,100,954 microseconds over 19.58 seconds (15.37 permille averaged over that entire interval); a later sample after exit retained an 8-permille hourly mean over 39 seconds. The exact test container and prior owned test resources were verified removed. Full bare-idle acceptance was not declared passed: its unchanged strict threshold stopped at 10 permille. Only the affected container path and protocol helper were rerun after script corrections; no production code or broad Rust suite rerun was needed.

Revision 2026-11-10: final review exposed two fixture failure paths. A selected sample is now created only after the second fresh coverage event, and ExitStack guarantees process/container cleanup still runs after daemon shutdown or another cleanup step raises. A failure-injection cleanup test and the current base64 protocol test both passed. The focused container run passed again with 36,698,655 microseconds of direct cumulative CPU (15.22 permille mean versus 17 permille in the selected interval), and verified its exact container and runtime processes were absent afterward. Production Rust sources and their successful validation remain unchanged.

Revision 2026-11-10: removed the fixture's separate created-container ID manifest after review found that it could miss partial provisioning. The container engine now supplies the single existence/ownership fact: each run uses its own named instance, and cleanup queries all managed resources with that exact label after stopping the daemon/processes. Real cached-image verification removed an owned unstarted container while preserving a differently labeled fixture. The focused CPU run and both Python checks passed again, with no owned container or daemon remaining.
