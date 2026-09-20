# Morannon Podman mbx build comparison, 2026-09-20

Plain `cargo build` through Mjolnir's user-shell action took 160.554 seconds
without mbx, 147.687 seconds with the preexisting shared cache, and 39.343 seconds
in another fresh session after the first mbx build warmed that cache. The warmed
run was 4.08 times faster than the baseline (75.5% less elapsed time). All builds
succeeded. No agent prompts were sent.

## Setup and measurements

The isolated instance is `mbx-bench-20260920`, using installed mj 2.14.0 and
`morannon-podman`. Configuration and data overrides live under
`/mnt/optane/mj-mbx-bench-20260920/{config,data}`. Run
`/mnt/optane/mj-mbx-bench-20260920/mj` to open it, or append `sessions --json`
to inspect it. This wrapper supplies both directory overrides and `--instance`.
The instance and its three idle sessions were left available for inspection.

Each session cloned Hel commit `69aa5c317a248da299a58758a712630cb2cec9f4`
into its own workspace volume and started without a Cargo target directory.
All used image ID
`c73bc719f75b397084701ec84ac5a5772c409da5ac22892cf927a701e9df833c`,
Rust 1.96.0, and Cargo's default dev profile. No CPU or memory container limit
was configured. Builds ran sequentially on the shared 96-logical-CPU host.
Registry downloads were included, as they are in a normal new-session build.
These are single observations, not repeated statistical benchmarks; unrelated
host workloads continued running.

The benchmark used `POST /api/actions` with `action: run-shell` and
`command: cargo build`, the same action as entering `! cargo build` in the UI.
`mj prompt` rejects leading `!` commands. Elapsed times below come from mj's
completed shell records and include wrapper startup and shutdown.

| Session | Cache | Shell elapsed | Cache hits | Misses | No lookup |
| --- | --- | ---: | ---: | ---: | ---: |
| `aeae2eb6e86ba69201666f998e6f4773` | Disabled | 160.554 s | — | — | — |
| `41e6da423129ad97bfd4910cb78b3eca` | Existing shared store | 147.687 s | 372 | 513 | 52 |
| `264b250b1ffa8caffe862a5469def0b0` | Store warmed by previous row | 39.343 s | 729 | 181 | 3 |

The existing-cache run reduced elapsed time by 8.0%. It had no cache hits for
Hel's own crates. In that run, `mj_controller` compiled for 62.75 seconds,
`mj_core` for 22.03 seconds, and the final `mj` compilation for 17.61 seconds.
The warmed run hit every Hel crate, including the controller and executables.
Its 729 hits comprise 523 Rust and 206 C actions.

## Findings

The integration works. In the enabled session, `cargo` resolves to mj's
worker-local mbx executable, mbx reports 1.15.0, and `MBX_CACHE_DIR` points to
`/mnt/nvme/mbx`. Both workspace storage and cache are local ZFS datasets on
Morannon, not client NFS paths. The warmed build restored about 2.79 GB,
including 2.76 GB via reflink. Missing native host mbx is not a blocker:
Mjolnir provisions its pinned executable inside the session.

All 180 remaining C misses in the warmed run have the `path-specific C object`
diagnostic. These objects embed absolute paths; mbx intentionally limits their
reuse to matching paths. A different mj session changes those paths. The
slowest was `cc:bcm.c` at 7.29 seconds. `mbx explain --last` confirms the cause.
Setting `MBX_CC_STORE_PATH_SPECIFIC=0` would avoid storing these disposable-path
objects, but would not make them cache hits; that setting was not changed.

The sole Rust miss was `mime_guess`. Comparing the recorded key components
shows only `environment MIME_TYPES_GENERATED_PATH` changed. Its compilation
took 0.73 seconds. Thus additional reuse is limited by path-sensitive C outputs
and this generated-path environment variable, rather than a missing cache
mount or cargo shim.

The warmed build recorded 36 bypasses: 16 C compiler queries, 16 C invocations
without an output, one unsupported C flag, two Rust compiler queries, and one
stdin invocation. It recorded no cache divergences or remote failures. Summed
avoided compiler duration was 519.8 seconds; that is parallel compiler work,
not elapsed time saved.

## Evidence and isolation

Raw conversations, readable transcripts, and mbx JSONL events are saved under
`/mnt/optane/mj-mbx-bench-20260920/`. The first enabled mbx event session is
`1789919541089-1477-7tod`; the warmed one is `1789919713492-1257-qlwb`.
Use per-session event statistics rather than differences in lifetime totals,
since other users of the shared store were active.

Only the required profile and Morannon settings were copied into the isolated
configuration. The test viewer uses port 14765; remote workspace prefix is
`.local/share/hel/mbx-bench-20260920`. The live configuration was read only, and
its SHA-256 remained identical after the benchmark. No live session, mount,
source, or shared cache configuration was changed. The benchmark populated
the existing cache through ordinary mbx builds without clearing it.
