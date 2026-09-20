# Reduce warm prompt submission latency


This ExecPlan follows `.agents/PLANS.md` and is updated during implementation.

## Purpose / Big Picture


Issue #1095 concerns seconds between Enter and the authoritative user message in a warm conversation, on both TUI and browser. Immediate local rows already exist. This work measures and reduces the actual submission-to-projection delay, prioritizing local workers and the user's morannon-podman target (SSH plus Podman).

## Progress


- [x] (2026-09-19) Inspected the submission, synchronization, relay, and publication paths; confirmed both primary target types.
- [x] (2026-09-19) Added isolated endpoint, browser DOM, and terminal frame probes plus correlated monotonic stage timings.
- [x] (2026-09-19) Reused browser projections, avoided collapsed-body copies, and aligned terminal reuse across sliding history windows; added repair/collapse and diffstat-preservation regressions.
- [x] (2026-09-19) Validated actual local and morannon-podman paths with 48 MiB histories; full Rust tests, Clippy, formatting, Python checks, and browser regressions passed.
- [x] (2026-09-19) Completed task-owned changes for commit on master; merge and push explicitly authorized by the user.

## Surprises & Discoveries


The controller already returns relay acceptance before catch-up, but the same actor awaits entire sync cycles before taking another command. A cycle polls history, catches up a fixed event frontier, acknowledges it, and polls subagents before publishing. The terminal daemon additionally waits for best-effort prompt-history storage before returning acceptance. These were hypotheses, not the reproduced causes. Measurements found full browser ACP decoding plus copies of collapsed tool bodies dominating long histories. Terminal stage timings then showed ~240 ms applying each bounded history window, versus <1 ms drawing its frame: positional zip reuse was invalidated whenever the tail slid forward. Aligning by durable item position reduced application to ~13 ms.

Read-only live-store aggregates established that a 48 MiB history is representative of the largest stored session; no live session received test prompts. The morannon NFS runbook was read; no evidence implicated NFS and no mounts or host configuration were changed. The remote fixture requires its own disposable image because configured targets do not expose arbitrary container run arguments. Its Git rewrite points only at the image's isolated fixture repository.

## Decision Log


Use monotonic process-local timings correlated by command ID and relay event ordinal; never infer cross-machine duration by subtracting wall clocks. Exclude optimistic rows from authoritative-display measurements. Use isolated stores and fake harnesses for benchmarks; do not mutate active user sessions or weaken durability. Preserve per-session command order and connection ownership. Changes must follow measurements rather than implementing every speculative optimization.

Do not rewrite actor scheduling or weaken journal acknowledgement for this fix: acceptance stayed in tens of milliseconds locally while presentation dominated. Keep existing serialization, leases, reconnect, and queue behavior. Browser projectors retain parsed entries per active session, trading retained memory for avoiding repeated decoding; lifecycle forget/error drops the cache. The two-task projection concurrency limit and generation guards remain in force. Source equality guards reuse across same-millisecond changes and integrity repairs.

## Outcomes & Retrospective


Local 48 MiB synthetic tool-history measurements (10 samples, dev profile): original browser authoritative API availability p50/p95 1221/1634 ms; final 104/262 ms. The daemon submission endpoint followed by authoritative browser availability changed from 1075/1660 ms to 72/75 ms. Final real browser Enter-to-authoritative-DOM p50/p95 is 101/129 ms. The TUI, measured after the browser fix but before the terminal fix, took 726/847 ms; final terminal input-to-authoritative-frame preparation is 145/156 ms. Frame preparation excludes terminal emulator paint, and includes a durable queue row if that is the first authoritative representation. No optimistic row is counted.

The original local short-history API baseline was already fast (browser availability 46/88 ms, acceptance 19/50 ms). Real short-history surfaces in a later run measured browser 121/138 ms and terminal 113/127 ms. These are separate runs under shared-host load, not a claim that small histories improve by the same factor.

Remote short-history baseline on actual morannon-podman succeeded: browser endpoint acceptance 16/512 ms and authoritative availability 90/553 ms; daemon endpoint acceptance 39/220 ms and authoritative browser availability 71/252 ms. Large-history remote before/after (10 samples): browser authoritative availability 893/1533 ms → 76/237 ms; daemon submission followed by authoritative browser availability 810/1484 ms → 77/89 ms. Endpoint acceptance remains approximately 14–21 ms median. Final real browser DOM p50/p95 is 107/145 ms and terminal frame preparation is 163/189 ms. Both targets therefore reproduce the browser cost, and both surfaces are responsive after the fixes.

Full Rust tests passed before the terminal alignment fix, as did 79 deterministic browser tests (3 skipped) and JavaScript unit tests. The full final Rust suite passed, including the new window-alignment regression. Final Clippy passed. Repeated test/build work and other host activity can affect tails; these small samples diagnose the reproduced cost rather than establish a production SLO.

## Context and Orientation


`mj-chat/src/chat/remote.rs` sends terminal operations through `mj-client` session handles. Browser actions enter the controller server runtime. Both converge on `mj-controller/src/session_manager/actor.rs`, which owns a sequential relay connection and publishes managed session views. `standalone.rs` synchronizes durable event pages into the controller database. The relay worker accepts commands into its journal before replying. CommandQueued represents acceptance; CommandStarted adds the authoritative user transcript row. `tests/e2e/reliability_lab.py` provides isolated stores, fake ACP harnesses, HTTP and terminal clients.

## Plan of Work


Milestone 1 instruments client dispatch, actor queueing, relay calls, durable projection, publication, and client render without recording prompt contents. Reproduce idle and busy warm sessions with short and large histories; measure attachment preparation separately. Record p50/p95 on local and actual morannon-podman paths. Inspect storage mounts before attributing waits to NFS; follow the host runbook if an NFS problem is suspected.

Milestone 2 removes measured unnecessary waits. Candidate changes are resumable actor sync phases, publication before housekeeping, background best-effort prompt-history storage, and removal of client refresh dependencies. Do not cancel an in-flight sequential relay exchange and reuse its connection. Preserve bounded sync progress, configuration-before-prompt ordering, fixed replay frontiers, durable acknowledgements, lifecycle leases, and idempotent reconnect. Introduce no new crate, protocol, or database migration without a demonstrated requirement.

Milestone 3 repeats the identical workload and validates behavior. Add barrier-based tests for scheduling and early publication, plus client input-to-authoritative-row coverage. Run `cargo test -- --test-threads=8` outside the sandbox, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, and `npm test` in `tests/e2e/web`. Record actual target limitations rather than claiming unperformed validation. Commit only task-owned paths; existing unrelated plans, `1q`, and `mj.sqlite3` remain untouched.

## Validation and Acceptance


Account for a reproduced multi-second delay with stage timings, then demonstrate its removal in the same workload. Report acceptance and authoritative-display p50/p95 separately for each surface/target. Tests must cover late acknowledgements, busy queue state, reconnect deduplication, configuration ordering, lifecycle leases, and background failure reporting. Retain existing optimistic presentation. Do not close #1095 solely because a synthetic or optimistic-display test passes.

## Idempotence and Recovery


Benchmark resources are uniquely named and isolated; stop owned processes before removing their files. Never send test prompts to a user's active conversation. Re-running a benchmark creates fresh state. Existing protocol command IDs provide retry deduplication. All production state migrations remain out of scope.

## Artifacts and Notes


Keep build and measurement output under `/mnt/optane` or repository target directories, not `/tmp`. Persist concise results and reproduction commands here or in `.agents/docs/`.

## Interfaces and Dependencies


Reuse `tracing`, `Instant`, command IDs, existing relay cursors and session views. Reuse the existing Python/Playwright reliability harness and Rust colocated fakes. Any new timing fields are internal observations, not public payload requirements.

Revision (2026-09-19): initial execution plan after user approval, with real local and morannon-podman validation prioritized.

Revision (2026-09-19): measured presentation costs replaced speculative actor scheduling changes. Existing scheduling, busy queue, late acknowledgement, reconnect, lease, and projection-generation tests provide regression coverage; no scheduling barrier tests were added because scheduling semantics were not changed.

Reproduction: `python3 tests/e2e/prompt_latency.py --hel target/debug/mj --worker target/x86_64-unknown-linux-musl/debug/mj-worker --history-mib 48 --count 10 --surfaces`. Add `--target morannon-podman` for the actual remote target. Build the portable worker with `cargo build -p brokk-mj-worker --bin mj-worker --target x86_64-unknown-linux-musl`. The fixture writes per-sample JSON under `target/reliability-artifacts/`, cleans its owned processes/containers/image, and preserves logs. Timing targets are `mj_controller::latency` and `mj_chat::latency`, at debug level, with no prompt content.

Final artifacts: `/mnt/optane/hel-latency-tests-final.log`, `/mnt/optane/hel-latency-clippy.log`, `/mnt/optane/hel-latency-web-tests.log`, `/mnt/optane/hel-latency-final-local.log`, `/mnt/optane/hel-latency-final-morannon.log`, and `/mnt/optane/hel-latency-large-morannon-before.log`. Final fixture JSON is in `target/reliability-artifacts/prompt-latency-seed-1095-4004027/` (local), `...-4006650/` (remote), and `...-4034367/` (remote baseline). Owned remote containers, images, volumes, clone snapshots, and fixture directories were removed after their processes stopped. No database migration, transport change, actor scheduling change, or live-store upgrade was needed. Busy-queue correctness is covered by the existing suite; no busy-load latency SLO is claimed.
