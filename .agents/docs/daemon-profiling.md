# Profiling the live daemon

How to measure the production daemon's CPU and heap, and how to read the results without being misled. These lessons come from the 2026-11-11/12 retention and CPU work. Its results are in `daemon-retention-candidates.md`, and dated baselines are at the end of this file.

## Before measuring

- **Measure the daemon that is actually running.** Other sessions install often, and each install replaces the daemon. Read the PID and build from `mj daemon status` at measurement time, and record the build with every number. A script that waits for a PID it read earlier can end up measuring a process that has already exited. Re-read the PID in the wait loop.
- **Wait 15 minutes after start.** The first minutes include worker upgrade preparation (profile copies, checkpoint archive checks), SessionWiki's startup sync and recovery work. After a relay protocol bump, worker replacement can keep CPU high for about 20 minutes.
- **Install with frame pointers if you need stacks.** `RUSTFLAGS="-C force-frame-pointers=yes" scripts/install.sh --config profile.release.debug=1`, then run an ordinary `mj` command (for example `mj sessions`) to hand off. DWARF unwinding loses most samples. Installs made by other sessions usually have no frame pointers: perf then shows most samples with no mj frame. Their CPU totals are still exact.
- **Do not judge C-library cost from dev builds.** In the dev profile Cargo builds C dependencies (zstd-sys, SQLite) without optimization, so dev measurements overstate their cost several times. Measure the installed release build.

## CPU

- **Totals:** sample `/proc/<pid>/stat` fields 14 and 15 (user and system ticks, 100 per second) over 60 seconds, three times.
- **Profile tool:** the WSL `/usr/bin/perf` lacks the libraries needed for symbols and unwinding. An unpacked Ubuntu perf exists at `/tmp/mj-daemon-regression-20261111/perf-tools/usr/bin/perf`. `/tmp` is cleaned, so unpack Ubuntu's perf package again if it is gone.
- **Profile command:** `DEBUGINFOD_URLS= perf record -F 49 -e cpu-clock:u --no-inherit --no-buildid --no-buildid-cache -m 64 -g -p <pid> -o profile.data -- sleep 30`. Without `--no-buildid-cache`, perf copies the 1 GB binary into `~/.debug`. Without `--no-inherit`, it follows forked ssh and shell children. Keep the buffer small (`-m 64` works with `--no-inherit`), because the daemon's ~300 threads exceed perf's mlock limit with larger buffers.
- **Reading it:** `perf script` gives one stack per paragraph. Count the share of samples whose stack contains a frame (`capture_runtime`, `attention`, `zstd`) instead of reading only the top frame.

## Heap

- **Enabling:** jemalloc is built with `stats` and `profiling` but inactive. The daemon inherits the environment of the `mj` command that starts it, either the post-install `mj sessions` or `mj daemon restart`:
  `_RJEM_MALLOC_CONF='prof:true,prof_active:true,lg_prof_sample:19,lg_prof_interval:33,prof_final:true,prof_prefix:<dir>/heap'`.
  It then writes a dump every 8 GiB allocated, about every 15–20 seconds. Restart without the variable afterwards.
- **Symbolizing:** generate `jeprof` from `~/.cargo/registry/src/*/tikv-jemalloc-sys-*/jemalloc/bin/jeprof.in` by replacing `@jemalloc_version@` with the jemalloc version (5.3.1) and `@JEMALLOC_PREFIX@` with `_rjem_`. Run `jeprof --collapsed --inuse_space ~/.cargo/bin/mj <dump>`. The binary must be the one that wrote the dump: the next install makes older dumps unreadable, so copy the binary if you need them later.
- **Aggregating:** the collapsed format is `frame;frame;... bytes`. Sum bytes by the nearest and the outermost mj frame, after skipping generic frames (serde, clone, alloc, collect, std, tokio). Also sum the stacks that contain a named owner. The script used in this work is at the end of this file.

## Reading heap results

- **One dump is one moment.** Temporary jobs dominate some dumps:
  - SessionWiki indexing holds one whole session's converted transcript, up to 150 MB, then frees it;
  - recovery work held about 110 MB in one dump.

  Across the dumps of a single daemon, the total varied from 644 to 1,050 MB. Compare owners across several dumps, or subtract SessionWiki and recovery, before comparing totals between builds.
- **The heap profile does not see C allocations.** jemalloc is linked with the `_rjem_` prefix, so `malloc` calls made by C libraries go to glibc. SQLite's page caches and zstd's per-thread contexts are therefore invisible to jeprof, and they count only in RSS. RSS minus jemalloc's resident bytes is roughly glibc heap plus stacks and binary pages. Check RSS alongside the heap profile.
- **`clone_subtree` under `capture_runtime` means a field is copied into every retained publication.** `RuntimeHistory` (`mj-controller/src/daemon/feed.rs`) keeps up to 4,096 runtime publications. A field that is not a shared or persistent structure is copied into each one: a per-session `BTreeMap` cost 313 MB (44031f82f). A persistent map built fresh each time shares nothing, so edit the previous one.
- **Work that grows with the number of records is multiplied by every poll.** `capture_runtime` runs for each `runtime_publication` and each `runtime_changes_for_subagent_parents` call. A computation over all ~26,000 records there cost about a core (fixed in 680df098f by computing it incrementally from `SnapshotMap::changes`).
- **Strings decoded through serde's internally tagged enums show no type names.** `TranscriptBody` is `#[serde(tag = "kind")]`, so serde first buffers each body into `Content`. The strings allocated there are moved into the final fields and stay allocated, so they appear under `ContentVisitor::visit_str` stacks. Identify them from the nesting depth (field → array → object → string), or measure field sizes in the store as below.

## Measuring field sizes in the store

Open the store read-only (`file:<path>?mode=ro`) and sum the bytes per field of `materialized_transcript_items.body_json` for the newest items of the live sessions. That is how tool-call JSON, presentation sources and terminal output were sized for the zstd work (scripts in `/mnt/optane/tmp/zstd-m3/`). A compression dictionary trained on these rows must never be compiled into the published binary, because it would contain text from private sessions.

## Baselines (production host, about 60 running sessions, about 26,000 session records)

| Date and build | Sampled live heap at 15 min | Decoded windows | RSS | Settled CPU |
| --- | ---: | ---: | ---: | ---: |
| 2026-11-12, before the retention work | 1.4–1.8 GB | 450–680 MB | 3.0–3.6 GB with swap | |
| 2026-11-12, 891d942a9 | 798 MB (97 MB of it SessionWiki) | 184 MB | 1.77 GB | |
| 2026-11-12, 3ac0c649d (zstd) | 766 MB (about 110 MB of it recovery) | 128 MB | 1.76 GB | 136–149% |
| 2026-11-12, 85fc12c3 (incremental attention) | not measured | not measured | 1.36–1.42 GB | 76–86% |

## Aggregation script

```python
import sys, re, collections
own = re.compile(r'(mj_controller|mj_core|mj_client|mj_transcript|mj_checkpoint|sessionwiki|mjolnir|mj::)')
skip = re.compile(r'(::deserialize|::clone|Clone>::clone|::from_str|::from_slice|serde|alloc::|core::|std::|::collect|::from_iter|::to_owned|::to_vec|::with_capacity|::push|::extend|::insert|::new\b|block_on|spawn_blocking|run_blocking|::poll|closure#\d+\}>?\[inline\]$)')
def clean(f): return re.sub(r'<[0-9a-f]{16}>|\[inline\]', '', f)
near, outer, total = collections.Counter(), collections.Counter(), 0
for line in open(sys.argv[1]):
    stack, _, w = line.rstrip('\n').rpartition(' ')
    try: w = float(w)
    except ValueError: continue
    total += w
    mine = [clean(f) for f in stack.split(';') if own.search(f)]
    meaningful = [f for f in mine if not skip.search(f)]
    near[(meaningful[-1] if meaningful else (mine[-1] if mine else '(no mj frame)'))[:150]] += w
    outer[(meaningful[0] if meaningful else '(none)')[:150]] += w
mb = 1048576
print(f"total {total/mb:.1f} MB")
for name, c in (("nearest", near), ("outermost", outer)):
    print("==", name)
    for k, v in c.most_common(25): print(f"{v/mb:8.1f} MB {100*v/total:5.1f}% {k}")
```
