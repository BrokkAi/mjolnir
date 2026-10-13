# Keep worker and daemon diagnostics alive on a full disk

This living ExecPlan follows `.agents/PLANS.md`. Changes remain uncommitted by explicit delegation. A worker is the process owning a session harness; the daemon owns controller state and its HTTP API.

## Purpose / Big Picture

When logging runs out of space, processes must keep serving accepted work. After storage recovers, logs explain which lines were lost. Workers retain recent diagnostics in their existing preallocated exit record, including on panic. Verification uses /dev/full, never a full real disk or live instance.

## Progress

- [x] (2026-10-13) Fast-forward pull and inspect logging and exit records.
- [x] (2026-10-13) Implement shared failure-tolerant diagnostic writers and worker tail.
- [x] (2026-10-13) Keep daemon writer running and remove panic-prone long-running prints.
- [x] (2026-10-13) Validate workspace build/clippy/fmt, four full suites, focused fixes, enabled panic backtrace with /dev/full, and real isolated worker/daemon processes.

## Surprises & Discoveries

The daemon log thread breaks on write or flush failure. Its channel then returns errors to tracing, causing repeated stderr diagnostics. Worker startup record errors use eprintln. Worker and CLI panic hooks chain the default hook. Controller exit-record deserialization already tolerates extra JSON fields. An initial child test accidentally inherited stderr because the shared subprocess helper overrides it; corrected the child to dup2 /dev/full itself and assert ENOSPC. JSON escaping requires trimming encoded bytes, not raw bytes, to preserve newest text.

## Decision Log

- Decision: Use one serialized owner per diagnostic sink for output, failure interval and bounded tail; retry only on the next incoming line.
  Rationale: Output and its loss accounting cannot race, and failed bytes never cause a busy retry loop.
  Date/Author: 2026-10-13, delegated implementation agent.
- Decision: Preserve daemon asynchronous logging and create_new semantics; synchronously write only panic messages. Reuse the worker's 16 KiB record instead of inventing a daemon record.
  Rationale: Ordinary diagnostics stay off the UI loop, log filename collisions remain visible, and panic output cannot be lost solely because its queue has not drained.
  Date/Author: 2026-10-13, delegated implementation agent.
- Decision: Treat log preparation, worker log redirection and exit reservation as best effort with explicit diagnostics.
  Rationale: A diagnostic file must never be the reason an otherwise viable process cannot start.
  Date/Author: 2026-10-13, delegated implementation agent.

## Outcomes & Retrospective

Worker and daemon diagnostic errors no longer fail tracing output. The worker retains up to 12 KiB of recent text (including recovery summaries in output order) and writes the newest tail, reason and metadata into the existing 16 KiB exit record. Daemon panic text bypasses its queue synchronously; there is no daemon exit-record mechanism. Real /dev/full worker and daemon checks passed. Full suites reported one unrelated controller mbx test failure and one tail-escaping defect, corrected and passed its targeted rerun. The mbx shell-profile test passed serially and in the final parallel suite; it is a parallel/environment-sensitive baseline failure. A daemon-reuse assertion also passed serially with stable binaries after a parallel build overlapped its earlier run. Normalized full-suite counts are 3435 passing tests and 26 ignored, with zero remaining failures after targeted reruns.

## Context and Orientation

`mj-core/src/logging.rs` will own safe stdout/stderr diagnostics and a reusable resilient writer. `mj-worker/src/main.rs` installs stderr tracing and panic handling. `mj-worker/src/exit_record.rs` overwrites a reserved worker-exit.json. `mj-cli/src/logging.rs` owns the daemon log thread; `mj-cli/src/main.rs` installs its panic hook. Long-running worker runtime and controller paths must use safe diagnostic macros.

## Plan of Work

First implement a shared Write wrapper that records failures and retains a bounded UTF-8 tail. On recovery write a summary before the next line. Route worker tracing and direct stderr diagnostics through the same shared sink, replace default panic-hook chaining, and attach the tail to the exit record with JSON-size-aware truncation. Wrap the daemon background file writer and keep processing after errors. Keep panic text visible synchronously where feasible. Add resource-failure tests beside the changed code and a /dev/full verification script under tests/e2e.

## Milestones

The first milestone is shared diagnostic ownership and worker exit evidence. Run the mj-core logging tests and worker exit-record/panic tests; failed writes return success, failures are counted, and a real /dev/full panic preserves the reason and newest tail in 16 KiB.

The second milestone is daemon continuity and complete validation. Run the daemon logging tests and the real-process script; the log channel remains connected after ENOSPC, the worker answers 100 status requests, and the isolated daemon serves HTTP despite full stdout/stderr. Complete the workspace checks and classify any unrelated failures through serial reruns.

## Concrete Steps

Run commands from this checkout. Run cargo fmt --check, cargo build --workspace --exclude brokk-mj-desktop, cargo clippy --workspace --exclude brokk-mj-desktop --all-targets -- -D warnings, and cargo test -p brokk-mj-core -p brokk-mj-worker -p brokk-mj-controller -p brokk-mjolnir outside the sandbox. Store logs in `.mj/agents/80bd2cd05bdba08632d90e8e70348b9f/`. Do not change Cargo configuration or target storage. Run `python3 tests/e2e/full_disk_diagnostics.py --worker target/debug/mj-worker --mj target/debug/mj --evidence .mj/agents/80bd2cd05bdba08632d90e8e70348b9f/dev-full` outside the sandbox. It prints one passing worker line and one passing daemon line, and saves the worker exit record. Use the existing isolated test processes, never the user's running processes.

## Validation and Acceptance

ENOSPC tests must return success to diagnostic callers, count failed lines, retain recent lines, and emit exactly one recovery summary before subsequent output. Exit JSON must fit the reserved bytes with Unicode and escaped characters. Isolated process checks redirect stderr/logs to /dev/full and confirm continued liveness and recorded worker exit reason. Rerun unrelated timing failures serially.

## Idempotence and Recovery

All process tests use owned temporary directories and named instances. Stop test processes before cleaning their directories. Live workers and daemon are read-only evidence sources. No commits, pushes, migrations or installation.

## Artifacts and Notes

The output-path audit is in the report directory. Validation results and incident conclusions will be stored there.

## Interfaces and Dependencies

Use std::io::Write, Arc and Mutex with poison recovery, chrono timestamps and serde_json already present in mj-core. Add a ResilientWriter<W> cloneable sink and diagnostic stderr/stdout macros. Keep existing public worker exit-record write signature; add a tail-aware variant. The new recent_log JSON field is additive and old readers ignore it.

Revision note (2026-10-13): Recorded completed implementation, corrected JSON-byte trimming, genuine /dev/full descriptor verification and initial validation results.

Revision note (2026-10-13): Recorded targeted rerun success, exact process-check invocation and independently verifiable milestones.

Revision note (2026-10-13): Recovery summaries now enter the bounded tail before recovered lines; final full validation runs on this shared-writer correction.

Revision note (2026-10-13): Safe hooks now capture explicitly enabled RUST_BACKTRACE output, preserving existing diagnostics without chaining the default stderr hook. The /dev/full child verifies the backtrace across the bounded reason and tail. Keep real-binary integration tests sequential with executable rebuilds to prevent legitimate build-identity handoffs from invalidating reuse assertions.

Completion (2026-10-13): Workspace build, clippy with warnings denied, format and Python syntax passed. Full suites covered 3435 tests with 26 ignored; the final daemon-reuse failure passed serially with stable binaries. The supplementary real-ENOSPC panic/backtrace test passed. Final real-process /dev/full script passed and saved the reserved worker exit record plus daemon startup/shutdown log. No live processes changed and no commits made.
