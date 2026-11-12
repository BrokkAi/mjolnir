# Reduce daemon allocation churn and retained heap

This ExecPlan follows `.agents/PLANS.md` and remains a living record of the investigation and implementation.

## Purpose / Big Picture

The running Mjolnir daemon has returned to several CPU cores and roughly 15 GiB resident memory. Another 23 GiB is swapped out. Preserve all session, relay, viewer, and upgrade behavior while removing needless copying and whole-binary allocations. Prove the memory diagnosis with allocator data, validate the changed ownership and file-reading behavior in isolated tests, and retain the previous serialized contracts.

## Progress

- [x] (2026-11-11) Identified installed daemon PID 223952, build 4732bd096, and collected CPU and memory evidence under private directory `/tmp/mj-daemon-regression-20261111`.
- [x] (2026-11-11) Read allocator counters without calling inferior functions: 450 arenas own 42.7 GB; a bounded free-list inspection found at least 34.65 GB already freed within 37.24 GB of inspected arenas. The dominant observed growth is retained free heap.
- [x] (2026-11-11) CPU samples show allocation/free/copy/comparison churn. Debugger samples identify operational-state publication, SessionWiki tool JSON copying, and repeated full worker-binary reads. Initial CPU windows averaged 5.83 and 2.53 cores during live session activity.
- [x] Preserve existing shared operational-state and subagent-queue publications through relay caching, managed snapshots, and runtime views.
- [x] Remove redundant reads of verified immutable pinned workers and make actual file verification stream rather than allocate the whole executable.
- [ ] Validate ownership, wire fidelity, file boundaries, and measured allocation behavior. Address remaining measured heap retention if the corrected paths still reproduce it.
- [x] Full touched-crate suites, integration binaries, and workspace Clippy completed. The full controller run had one ETXTBSY failure in the rsync launcher; the exact isolated rerun passed. Both commits received clean reviews and were pushed.
- [x] Installed `f35271839` with optimized host debug symbols and verified daemon PID 3407563 running that build.
- [x] Added jemalloc for Linux CLI/daemon; workspace Clippy, full CLI/API/startup/PTY regressions, license generation and musl CLI checks passed. Review clean. Merged concurrent upstream fixes and installed optimized symbols build `312cdbd23`; daemon PID 419766 runs it.
- [x] Live hourly usage HTTP checks succeeded: 1.2038 s for 65 active rows initially and 0.1574 s for 4,663 rows including suspended sessions.
- [x] Audited application retention owners at the user's request; recorded the ranked candidates in `.agents/docs/daemon-retention-candidates.md`. Read-only production probes confirmed old unfinished tools pin 56,041/83 MB and 36,246/55 MB live windows.
- [ ] Validate and publish removal of the historical unfinished-tool pin, preserving late-update rehydration and current turns; install and repeat live measurements.

## Surprises & Discoveries

Resident memory understates the problem because about 23 GiB is swapped. Most inspected memory is already free inside libc allocator arenas, so growing RSS alone does not establish a leak of live application objects. Largest arenas have hundreds of megabytes free while their attached threads retain only a small fraction.

The installed WSL perf binary lacks libelf and DWARF support. The official Ubuntu `linux-perf` package was downloaded and extracted into the private profiling directory, without installing system packages. It resolves symbols from the recorded profile. Unwinding remains incomplete, so brief debugger snapshots established callers; every debugger script detaches in a finally block and never calls functions in the daemon.

`StandaloneSession` already owns a `SharedSnapshot<RelayOperationalState>`, but `snapshot()` calls `clone().into_inner()`, forcing a full copy. `ManagedSessionSnapshot` and `RuntimeSessionView` then carry owned operational values. The relay client's known-state cache also clones operational values. `WorkerBinarySourceSnapshot::resolve` rereads and verifies an artifact that was already verified and atomically pinned at capture; the existing file-identity index covered its original source but did not cover that pinned copy; `verify_worker_build` allocates the entire file with `std::fs::read`.

## Decision Log

- Decision: Fix observed ownership and whole-file allocation paths before selecting an allocator or adding any memory-management policy.
  Rationale: The previous optimizations already introduced shared snapshots and immutable pinned worker sources. Forward those facts rather than undoing their benefits at each consumer. No speculative thread limit, timer, cache size, or admission gate is justified by this evidence.
  Date/Author: 2026-11-11 / Codex.
- Decision: Keep serialized operational state and queue shapes identical by using the existing serde-transparent `SharedSnapshot` type.
  Rationale: A daemon-side ownership correction should not require a relay protocol bump and a fleet-wide worker rollout. Copy-on-write preserves readers of older snapshots when a real mutation is required.
  Date/Author: 2026-11-11 / Codex.

## Outcomes & Retrospective

Profiling has established severe allocator retention and concrete avoidable copy paths. Ownership and streaming changes are implemented. The first focused run passed all 6 client cases and 147 controller cases (3 ignored), but exposed the existing stale-restored-pin regression, so verification now reuses the file-identity index rather than trusting existence alone. The passing controller cases include the extended real-relay snapshot identity/copy-on-write/wire check and SessionWiki/indexing cases. The corrected stale-pin recovery case and all six core worker-build checks passed in the targeted rerun. Commit `2e04846ef` received a clean independent review. Workspace Clippy passed after adapting chat/CLI fixtures and cloning the assessment field when the dashboard needs its own value. Full core/client/controller validation passed 634 core tests, 53 client tests and their integration/doctests, plus 2,546 controller cases. One controller rsync test failed with ETXTBSY in its generated wrapper and passed on the exact isolated rerun; no transfer source changed. Chat/CLI validation remains in flight. The first churn fixes are installed as `f35271839` with debug symbols (`file` confirms debug_info, not stripped). Normal startup handed off to daemon PID 3407563. To relieve continued memory pressure while validation ran, the installed daemon was restarted through its normal management command; PID 2059555 replaced 223952 and initially used 2.6 GiB RSS with zero swap. Workers continued across the restart. This fresh old-build RSS is pressure relief, not evidence that the fixes worked. On 2026-11-11 the user explicitly authorized installation after pushing, with debug symbols, and a second profiling pass after memory pressure subsides.

## Context and Orientation

`mj-core/src/snapshot_map.rs` defines `SharedSnapshot<T>`, an Arc-backed immutable value with transparent serde and copy-on-write mutation. `mj-core/src/state.rs::ManagedSessionSnapshot` and `mj-client/src/daemon.rs::RuntimeSessionView` currently hold owned operational state. `mj-controller/src/session_manager/standalone.rs::snapshot` converts shared state back into that owned form. `mj-controller/src/worker_client/relay.rs` caches a worker state beside the digest the worker assigned to it; preserve that association and never mutate the cached state through an acknowledgement overlay.

`mj-core/src/worker_build.rs` parses the embedded worker build stamp. `mj-controller/src/controller/worker_binary/binary_source.rs` verifies sources and publishes immutable content-addressed copies atomically. The daemon's source snapshot owns the startup verification result; a missing pinned file must still use the existing recovery path. `mj-controller/src/sessionwiki.rs` projects summaries into SessionWiki and currently clones tool JSON before decoding it. That path remains a measured follow-up if it is still material after the ownership fixes.

## Plan of Work

First carry `SharedSnapshot` through managed snapshots, runtime views, and the relay client's operational cache and attachments. Use the existing copy-on-write mutation when the controller overlays acknowledgement facts. Change consuming adapters to borrow through Deref or explicitly take ownership only when they actually mutate. Preserve serialized field names, shapes, and compatibility with older workers.

Next make worker verification incremental over a reader while retaining the current parser's exact stamp rules, including duplicate matching stamps, conflicting stamps, truncated candidates, and stamps crossing read boundaries. Reuse one stamp interpretation helper for bytes and streamed files. A captured local source is already verified and pinned. Index the pinned file itself with the existing build/path/size/mtime/inode/ctime index, and use that same verifier during resolution. An unchanged file avoids executable reads; a modified or restored file still misses the index and is verified, while a missing file retains the existing recovery path.

Measure representative large operational and tool payloads through actual publication and store-reader surfaces. Preserve whole rendered/serialized results, previous snapshot fidelity, and cancellation/recovery behavior. If freed heap still grows under the corrected workload, investigate that observed residual before adding any allocator dependency or reclamation policy.

## Concrete Steps

Run all Cargo tests outside the restricted sandbox through the existing mbx Cargo wrapper. Do not change target directories or build-cache configuration. Keep package selection stable across focused and final runs. Use `MJ_INSTANCE=daemon-allocation-test` for focused checks; full unit suites may instead use explicit isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR` with the inherited instance name unset, because the existing settings golden expects the default port while isolating its own store. Every daemon, CLI, TUI, or end-to-end invocation of a new build must use a named instance.

Profile evidence includes `baseline-pidstat.txt`, `cpu.data`, `cpu.script`, `cpu-stable-dwarf.data`, `arenas.txt`, `free-bins.txt`, and `stacks-all-*.txt` in the private temporary directory. Raw profiler artifacts must not be committed. For live measurements, take a fresh CPU window outside debugger attachments and distinguish provisioning activity from steady service work.

## Validation and Acceptance

Extend existing relay/publication integration cases to establish shared identity across unchanged reads, copy-on-write isolation after a real mutation, and identical serialized output. Exercise worker verification with real files larger than 64 KiB and stamps on buffer boundaries; preserve stale/conflicting-stamp rejection and pinned-source deletion recovery. New tests must protect integration or resource/ordering conditions, not restate ordinary examples.

After the final implementation, run the full suites for every touched crate and `cargo clippy --all-targets -- -D warnings` in the development profile. Fix failures and rerun only the failing cases; do not repeat whole suites to confirm a small correction. Complete independent review before pushing. Report measured memory and CPU effects separately from any live effect that still requires installation consent.

## Idempotence and Recovery

All new-build runtime tests use isolated instances and data. Do not clear caches or inject allocator calls while profiling. Installation and its ordinary daemon handoff are authorized after the validated changes are pushed; do not edit or commit source during that build. The already-pushed compatible usage indexes will apply through normal startup. Debugger inspections read counters and stacks only and detach on every exit. Worker artifacts remain immutable, and removing a pinned file must still recover through the existing resolver. Keep the current branch and commit only task-owned files.

## Artifacts and Notes

Allocator sample: 42,744,504,320 bytes of arena memory, 3,226,930,032 bytes free in top chunks. Free-list sample: 201 arenas examined, 37,242,613,760 bytes owned, and 34,654,598,064 bytes already free. This is a lower bound on freed memory, not an exact live-object census, because inspection was bounded and skipped locked arenas.

## Interfaces and Dependencies

Reuse `SharedSnapshot`, existing relay digests, and existing worker source pinning. Introduce no new crate or configuration option for the ownership fixes. Preserve worker and daemon wire protocols and serialized JSON. Any allocator change requires a separate measured justification from an isolated reproduction rather than being assumed to solve the source of the churn.

## Installation and second pass

After checks, review and push, compare HEAD with the installed build to avoid replacing a newer concurrent installation. Build the normal optimized install with debug information using `scripts/install.sh --config profile.release.debug=1`; this preserves optimization and adds profiler symbols without changing repository build settings. The script forwards this Cargo override to the host binaries while retaining the normal portable-worker profile, avoiding unnecessary worker debug data. Do not edit or commit until the complete host/native/musl build finishes. Trigger normal startup with the installed `mj sessions`, verify the new daemon build and PID, and allow startup recovery and worker preparation to settle. Sample CPU and RSS/swap over multiple windows, inspect allocations again if growth persists, and use the symbolized CPU profile to choose the next correction. A lower fresh-process RSS alone is not proof that the churn is fixed.

## Allocator decision and history

On 2026-11-11 the user explicitly requested adding jemalloc before spending more time diagnosing glibc-specific retention. Use `tikv-jemallocator` 0.7 for the Linux `mj` executable, which owns the daemon as well as the TUI and CLI. Enable upstream `background_threads` so jemalloc purges asynchronously, and retain its ordinary decay settings. Do not add an allocator selector or glibc tuning. Other platforms and the portable worker allocator are outside this change.

Read-only session searches for both allocator names found the original musl decision in `6ddf055a9312`, messages 22–29: the user approved musl Linux builds with mimalloc; the implementation scoped it to musl rather than benchmarking glibc plus mimalloc. Commit `2145cd3d9` subsequently moved the allocator declaration from the combined CLI into the portable worker. No mj benchmark rejecting mimalloc on glibc was found. Bifrost session `51a8864993190bfa7f45b7711daf1d70`, messages 2097–2132, records a separate comparison: jemalloc 1.28 GiB RSS versus mimalloc 1.40 GiB, rooted graph 48 s versus 60–77 s, and the user's choice of jemalloc. These are historical analyzer results, not daemon measurements.

The current running workload fell from about 155 top-level sessions to 59 during validation. Immediate pre-install measurements of the restarted old build were still 1.5–3.5 CPU cores, roughly 9 GiB RSS plus 3 GiB swap. Save session counts with the new profile and avoid crediting all of a CPU drop to the fix. Do not resume a glibc allocator investigation before installing the jemalloc change. Existing daemon startup, API, instance isolation and PTY/upgrade regressions exercise the new allocator; do not add a unit test that restates the global-allocator declaration. Include jemalloc's vendored BSD COPYING in the existing supplemental notice generator and clarify the Rust bindings' Apache license deterministically.

## Confirmed application-retention follow-up

The user requested a retention-candidate list while jemalloc built. The audit found that the old unfinished-tool exception defeats the live actor's 1,024-item target. The production-matching read-only probe retained 56,041 items/82,923,551 serialized bytes in one active actor and 36,246/54,592,707 bytes in another. A control without an old mutable entry retained 1,028 items/1,634,688 bytes. See `.agents/docs/daemon-retention-candidates.md` for the other candidates and evidence limits.

`StandaloneSession::apply_event_page` already calls `load_projection_references` for omitted historical rows before applying relay events. That reader handles explicitly identified tools, messages, streams and terminal references. Use that durable owner instead of keeping every intervening turn in memory. Remove pending/in-progress tools from the pinning predicate in both `ProjectionWindow::trim` and the matching SQL loader, while retaining the newest turn, whole turns around the existing tail target, and open text streams. Anonymous text chunks choose the latest open stream implicitly, and many event kinds close open streams; those existing semantics require retaining their state. Both measured ancient pins are tools, so the narrow ownership correction covers the observed defect without a new reference-query mechanism. Preserve serialized and durable history contracts; introduce no migration, protocol change, cache selector or new cap.

Extend the existing trimming/store agreement test and the late-tool-completion integration case. The latter now begins with an unfinished historical tool carrying more than 64 KiB of partial output, proves it is absent from the live cache, rehydrates it, applies its late completion, and verifies the full partial output and durable history remain. Run focused core/controller window cases first with the stable core/client/controller package selection, then complete touched-crate suites and workspace Clippy after the final code round. Review and push on master, install with the same host symbol override, and compare the actual production window sizes and CPU after handoff.

The final-rule read-only probe reduced the two sampled windows to 1,029 items / 1,951,417 bytes and 1,029 items / 1,457,784 bytes, about 97.5% less serialized body data. Their old tool calls were last changed on October 31 and October 5. The first broad trim prototype passed its focused cases, but source inspection found anonymous-stream and bulk-stream-closure semantics that those cases did not exercise. The final change therefore preserves open text streams, and the store/live trimming agreement regression now covers no pending content, an old pending tool, and an old open stream. The existing large-partial-output late-completion case covers actual historical rehydration.
