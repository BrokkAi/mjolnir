# Bifrost build and cache-health benchmark, 2026-09-20

Bifrost `cargo build` took 320.726 seconds without mbx. The released mbx 1.15.0
attempt failed after 20.170 seconds because a shared compiler shim pointed into
another container. A fresh session using the existing local development binary
with private-shim support succeeded in 266.766 seconds: 16.8% less elapsed time,
or 1.20 times faster. That successful run had 434 hits, 289 misses, and four
compilations without a lookup; it was not fully warm for this exact build.

Continuous observation found no evictions in 17.4 minutes, but also found a
failed automatic-GC invocation. Zero observed evictions must not be interpreted
as proof that cache maintenance is healthy or that hourly eviction cannot churn.

## Reproduction and isolation

All runs used the existing isolated instance `mbx-bench-20260920`, mj 2.14.0,
target `morannon-podman`, the live configuration's `BrokkAi/bifrost-dev` bundle,
and commit `fc6dd11df3c5aa6a9e6bafcb27dac775b00aa125`. Each used a new container,
checkout, and target directory. Rust 1.97.1 was installed before each measured
build, as required by the repository's toolchain file. The command was exactly
`cargo build`, submitted via mj's `run-shell` action, equivalent to entering
`! cargo build`. No agent prompts were sent. The builds ran sequentially.

The image ID was
`c73bc719f75b397084701ec84ac5a5772c409da5ac22892cf927a701e9df833c`.
Container CPU and memory limits were unset. Registry fetching remained part of
normal build timing. Other host builds continued throughout, so these single
observations are not a controlled repeated performance study.

| Run | mj session | Elapsed | Result |
| --- | --- | ---: | --- |
| Cache disabled | `e1e1492c84858f1f9ba379f78d038210` | 320.726 s | Success |
| Released mbx 1.15.0 | `aa36ef01a1b87c8a666a50b9600126fe` | 20.170 s | Failed: shared C shim |
| Development mbx with private shims | `0834ac49cd704aaa7fb7b66474c95cd9` | 266.766 s | Success |

The development binary was already built at
`/home/jonathan/Projects/mr-boxington/target/release/mbx` and was copied only into
the fresh test container, replacing that worker's `mbx` and `cargo` hardlinks.
It reports 1.15.0 but is not the released binary. Its SHA-256 is
`8b5820844bacc025751d151c6159c802c4f80750b29db1776d1ae267350cd70c`.
The source fix is mr-boxington commit `3da3fd81ff5b0c1943f4195a7897f2a7a75d2c4c`,
which adds `MBX_SHIMS_DIR` support after the 1.15.0 release. The existing binary's
exact source revision was not established; its private-shim behavior was
verified in the container. No source or live executable was modified.

The test sessions remain available through
`/mnt/optane/mj-mbx-bench-20260920/mj`. Configuration and data overrides are the
same as the earlier Hel benchmark. The live config SHA-256 was unchanged.

## Why the released build failed

The failed run restored 353 actions, recorded no misses or unconsulted actions,
and wrote no compiled outputs before failing. C build scripts reported:

    failed to find tool "/mnt/nvme/mbx/shims/mbx-c": No such file or directory

Inspection showed both shared C shims pointing at
`/var/lib/hel/workers/50f57c0acc2c39dfb2559fee988cefee/bin/cargo`, a path belonging
to another container and absent in the benchmark container. mj already exports
`MBX_SHIMS_DIR` under its worker's private directory, but released mbx 1.15.0
predates support for that setting. Concurrent containers can replace a shared
shim with a link usable only inside the last writer's container.

The corrected run created its C shim under
`/var/lib/hel/workers/0834ac49cd704aaa7fb7b66474c95cd9/mbx-shims/`, pointing at
its own worker's `bin/cargo`. The build then completed successfully. Shared
cache settings and existing shared shims were not manually altered.

## Why the successful run was only partially warm

Ten Bifrost library compilations hit, including core and the C#, PHP, Ruby, Go,
JavaScript/TypeScript, Python, JVM, C++, and Rust language crates. However,
`brokk_bifrost_analysis` missed and took 188.37 seconds. Policy took 34.73 seconds,
flow 30.52 seconds, and RQL 26.72 seconds. A C parser compilation took 49.63
seconds. Some of these run concurrently; their durations cannot be summed into
elapsed build time.

`mbx explain --last` compared the analysis crate with earlier recorded builds
and identified changed feature/configuration arguments, dependency artifacts,
and four source inputs:

- `crates/bifrost-analysis/src/analyzer/csharp/external.rs`
- `crates/bifrost-analysis/src/analyzer/csharp/mod.rs`
- `crates/bifrost-analysis/src/analyzer/ruby/type_flow.rs`
- `crates/bifrost-analysis/src/analyzer/usages/get_definition/csharp.rs`

This is evidence that earlier cached Bifrost builds were not equivalent to the
current default-branch build. It does not identify the cause of every miss or
prove that no artifact was evicted before observation began. Most C misses had
no comparable earlier key details, so their individual causes remain unresolved.
The successful run restored 1.664 GB, almost entirely via reflink, and reported
285.3 seconds of summed compiler work avoided, no divergences, and no remote
failures. One unsupported crate-type bypass consumed 20.14 seconds.

## Cache pressure and eviction observations

A read-only remote sampler followed newly appended action events and the
`savings/v1/tally.json` ledger every five seconds. Existing events were skipped
at startup. It collected 209 samples spanning 1043.326 seconds with no read or
parse errors. Sampling was stopped after evidence collection. Counts include
other active users of the same store as well as the benchmark.

| Metric | Observed |
| --- | ---: |
| Hits | 2,618 |
| Misses | 378 |
| Compilations without lookup | 228 |
| Bypasses | 308 |
| Store bytes evicted | 0 |
| Target bytes pruned | 0 |
| Eviction-counter increases | 0 |
| Peak five-minute miss rate, with at least 20 lookups | 39.1% |
| Windows satisfying mbx's thrash heuristic | 0 |

The initial action store held 289,933,051,137 logical bytes (270.0 GiB); the
post-build snapshot held 315,006,576,250 bytes (293.4 GiB). The effective store
budget was 252,329,328,640 bytes (235 GiB). The configured 1000 GB limit is a
separate combined action/target/incremental budget. Target and incremental
budgets were each 100 GiB.

A pre-build `mbx gc --dry-run --json` projected removing 41,232,243,715 bytes
(38.4 GiB) from the action store, 100,545,585,245 target bytes, and 28,993,413,789
incremental bytes. It did not remove anything. No explicit real GC or cache
clearing was performed.

## Automatic cleanup failure

The existing shared-store log `actions/gc/v1/sweep.log` contained:

    error: no such command: `gc`
    help: view all installed commands with `cargo --list`

The sweep stamp and log were dated 15:45 UTC; the code defaults to an hourly
sweep interval. In mr-boxington `crates/mbx/src/cli/gc.rs`, `spawn_collector`
launches `current_exe()` with `gc --automatic` and removes the Cargo-shim mode
environment variables. However, `crates/mbx/src/cli/shim.rs::is_cargo_shim`
also dispatches from the executable's `cargo` basename. mj installs a hardlink
under that name, so clearing the variables does not force mbx CLI dispatch.
The child is sent down the Cargo command path, explaining the observed error.
This failure is not repaired by the private-compiler-shim fix. No automatic
cleanup configuration or shared ledger was changed in this investigation.

The results establish no observed thrashing during the test, plus an over-budget
store and a failed automatic-cleanup path. They cannot establish long-term
cache health across successful hourly sweeps.

## Evidence

Raw conversations, readable transcripts, before/after statistics, the GC
preview, key explanations, and cache samples are under
`/mnt/optane/mj-mbx-bench-20260920/bifrost/`. The failed mbx event session is
`1789921615537-1927-tket`; the successful one is `1789921865956-1431-mrwy`.
`thrash-summary.json` summarizes the fixed observation interval. The earlier
Hel report is `.agents/docs/morannon-mbx-build-benchmark-20260920.md`.
