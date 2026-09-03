# Make a cross-harness resume fail fast and compact cheaply

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan must be maintained in accordance with `.agents/PLANS.md` (from the repository root). Read that file before revising this one.

## Purpose / Big Picture

Mjolnir (the `mj` binary in this repository) can resume a stopped coding session onto a different agent harness than the one that created it. That is a "cross-harness resume": the archived transcript was written by, say, Claude, and the session restarts on Codex. Because the new harness cannot read the old harness's native session file, Mjolnir first rewrites the whole transcript into a single "handoff" message using a cheap utility model. That rewriting step is called compaction.

On 2026-09-03 a real cross-harness resume behaved badly enough to be worth fixing twice over. Session `91fc44af75cfee0756d472edf3b6526a` (archived from Claude, resumed into the `codex3` Codex profile, target `morannon-podman`, which is an ssh-podman remote) sat in the `Compacting` stage for fourteen minutes. The controller log shows 65 utility-model requests, all `profile_id="codex3" model="gpt-5.6-luna"`, from 17:15:38Z to 17:29:38Z: about 26 seconds each, never more than two in flight. The resume then failed with

    resume failed: no Linux worker for x86_64-unknown-linux-musl; install mj-worker-x86_64-unknown-linux-musl beside mj, set MJ_WORKER_DIR/MJ_WORKER_BINARY, or configure MJ_WORKER_URL and MJ_WORKER_SHA256

That last check is local, needs no network, and takes under a millisecond. It ran only after compaction, from inside provisioning: `mj-controller/src/hel_controller/provisioning.rs` calls `install_worker_payload`, which calls `prepare_worker_files` in `mj-controller/src/hel_controller/worker_binary.rs`, which calls `worker_binary_for`, which calls `worker_binary_prerequisite_for_arch`. Fourteen minutes and 65 paid model requests were spent on work that was doomed from the start.

Why 65 requests, specifically. In `mj-controller/src/hel_compaction.rs`:

* The byte budget used to size each page of transcript sent to the summarizer was the *target profile's* `context_window_bytes`. That field is unset in the configuration, so the code fell back to `DEFAULT_CONTEXT_BYTES` = 256 KiB. The transcript sharded into about 33 pages.
* `reduce_summaries` merged the resulting page summaries two at a time (`summaries.chunks(2)`), so N pages cost N−1 further requests: 33 + 32 = 65.
* Both the paging phase and the reduction phase ran with `try_buffered(2)`, and a test named `independent_pages_run_with_at_most_two_requests_in_flight` asserted that cap of two.

A third problem sits underneath the first. The daemon that ran that compaction (pid 3000379) was started from `/home/jonathan/Projects/hel/target/debug/mj`. Later, a tool renamed that checkout's `target/` directory to `.mbx-target-backup-vHXvCs/`, rebuilt a fresh `target/`, and deleted the backup. On Linux, `/proc/<pid>/exe` follows renames and marks deletions, so inside the still-running daemon `std::env::current_exe()` now answers `/home/jonathan/Projects/hel/.mbx-target-backup-vHXvCs/target/debug/mj (deleted)`. `worker_binary_prerequisite_for_arch` already copes with the controller's own file vanishing (`stable_running_executable` copies `/proc/self/exe` into a cache), but the sibling lookup `select_sibling_worker` derives its search directory from that dead path, finds nothing, and the user is told to install a worker binary that may well already be sitting beside the real one. The honest answer is "the running mj binary was replaced; restart the daemon".

After this change:

* A resume that cannot possibly produce a Linux worker binary fails in milliseconds, before any compaction request, with the same actionable message as before, wrapped in the context `preflight the worker binary before resuming`.
* A compaction of that same 33-page transcript issues a handful of wide, concurrent requests instead of 65 narrow, mostly-serial ones: pages are sized from the summarizer's own context window (up to 1 MiB) rather than the target profile's unset budget, up to eight requests run at once, and the reduction phase packs as many summaries into one prompt as fit, so 33 page summaries reduce in a single request.
* A controller whose own binary was replaced on disk says so, and says to restart the daemon, instead of blaming a missing worker binary.

You can see all three working from the test suite (named tests below) and, on a real machine, by resuming a cross-harness session with no worker binary installed for the target architecture: it fails immediately instead of after minutes of `Compacting`.

## Progress

- [x] (2026-09-03 18:05Z) Merged `origin/master` (fast-forward `3d1929d` → `1390be2`) into branch `hel2`.
- [x] (2026-09-03 18:20Z) Read `.agents/PLANS.md`, `mj-controller/src/hel_compaction.rs`, `mj-controller/src/hel_utility_llm.rs`, `mj-controller/src/hel_controller/resume.rs`, `mj-controller/src/hel_controller/worker_binary.rs`, `src/hel_config.rs`, `src/hel_targets.rs`, and `brokk-anvil-llm-0.27.1/src/llm_client.rs`; wrote this plan.
- [x] (2026-09-03 19:10Z) Milestone A: worker-binary preflight before compaction, with tests.
- [x] (2026-09-03 20:05Z) Milestone B: two budgets, wider pages, greedy reduction, concurrency 8, one diagnostic log line, with tests.
- [x] (2026-09-03 20:45Z) Milestone C: a replaced controller binary is reported plainly instead of mis-resolved, with tests.
- [x] (2026-09-03 20:55Z) `cargo fmt`, full `cargo test`, `cargo clippy --all-targets -- -D warnings`, three commits on `hel2`.

## Surprises & Discoveries

- Observation: the configuration-level target template (`hel::hel_config::TargetTemplate` in `src/hel_config.rs`, line ~669) is a different type from the runtime one (`hel::hel_targets::TargetTemplate` in `src/hel_targets.rs`, line ~1613). Only the runtime one carries an EC2 `instance_type`; the configuration one names a launch template and never an instance type. The only architecture a *configured* template can state is a container `platform` such as `linux/arm64`.
  Evidence: `grep -n "pub enum TargetTemplate" -A 45 src/hel_config.rs` shows `AwsEc2 { aws_profile, region, launch_template, launch_template_version, ssh_user, address_source, identity_file, ssh_args }`, with no instance type; `ContainerTemplate` (line ~585) has `pub platform: Option<String>`.
- Observation: the generic "no Linux worker for ..." message the user actually saw should not have been reachable on that Linux x86_64 host from this branch's code, because `worker_binary_prerequisite_for_arch` falls back to the running Linux binary for a matching architecture (line ~731) before it gives up. The most likely explanation is that the daemon was built from the `hel` checkout, which carries an unpushed commit `23715a6 Keep remote workers on portable binaries` that stops a remote target from using the host's glibc binary. The target in the incident was ssh-podman, i.e. remote. Under that build only the musl sibling could have served the request, and the sibling lookup was exactly what the replaced `target/` directory broke.
  Evidence: `git log --oneline` in `/home/jonathan/Projects/hel` shows `23715a6 Keep remote workers on portable binaries`, which is not present in this worktree; this worktree's fallback at `mj-controller/src/hel_controller/worker_binary.rs:731` is unconditional on target locality.
- Observation: greedy packing needs a progress guard. If two adjacent summaries are each small enough to be a legal single-summary prompt but too large to share one prompt, every round packs the same number of groups and the loop never terminates.
  Evidence: constructed by reasoning about `pack_reduction_groups`; guarded with `ensure!(groups.len() < summaries.len(), ...)` in `reduce_summaries` and covered by `reduction_that_cannot_pack_any_pair_is_an_error`.
- Observation: deriving the page budget from a model's published context window can produce a budget below `MIN_CONTEXT_BYTES` (32 KiB) for a small model, which would make `compact_snapshot` reject the whole compaction before making a single request. A 16k-token model yields exactly 32 KiB; an 8k-token model yields 16 KiB.
  Evidence: `page_bytes = context_length * 4 / 2`, so `context_length = 8192` gives 16 384. Handled by clamping the backend's reported page budget up to `MIN_CONTEXT_BYTES`; an oversize rejection from such a model is already handled by the adaptive halving path in `summarize_page_adaptively`.

## Decision Log

- Decision: the preflight runs for every resume, not only a cross-harness one.
  Rationale: every resume installs a worker (`install_worker_payload` runs for all target kinds, including local-bare), so the check is always meaningful, and it costs microseconds. Restricting it to cross-harness resumes would leave the same late failure in place for same-harness resumes.
  Date/Author: 2026-09-03, implementation agent.
- Decision: the preflight determines the architecture in this order: an architecture the configured template names (a container `platform`), else the host architecture for a local target kind, else "either x86_64 or aarch64 resolves" for a remote target kind.
  Rationale: `target_architecture` normally learns the answer by running `uname -m` on the live target, which does not exist during a resume. A container `platform` is the only architecture a configured template can state, and it overrides the host for local container targets too, so it is checked first for every kind rather than only for remote ones. For a remote target with nothing recorded, accepting either architecture keeps the preflight strictly weaker than the real check, so it can never block a resume that would have worked.
  Date/Author: 2026-09-03, implementation agent.
- Decision: no architecture is inferred from an AWS EC2 instance type.
  Rationale: the brief suggested it as a possibility, but the *configured* `AwsEc2` template in `src/hel_config.rs` has no instance type at all — it names a launch template whose instance type is only discovered by calling the AWS API. There is nothing to read without a network call, so an EC2 target falls into the "either architecture" case.
  Date/Author: 2026-09-03, implementation agent.
- Decision: `UtilityCompactionBackend::page_bytes` clamps its answer up to `MIN_CONTEXT_BYTES`.
  Rationale: the value is the minimum over candidates, and one small-window candidate would otherwise abort the entire compaction with a budget-validation error before any request. The pipeline already shrinks a page and retries when a backend rejects it as oversize, so an optimistic floor degrades gracefully while a pessimistic one fails hard.
  Date/Author: 2026-09-03, implementation agent.
- Decision: the single diagnostic `tracing::info!` line is emitted from a small helper called at two places — once when the single-request path is about to be taken, once when the paged plan is known — rather than at one point in the function.
  Rationale: the point of the line is to make a slow compaction diagnosable, so it has to precede the requests it describes. The page count is only known after the pages are built, and the pages cannot be built before the single-request attempt without changing which budget check a short transcript hits first. Both call sites log the same event with the same fields; a compaction that falls through from the single-request path to the paged path logs twice, and that transition is itself worth seeing.
  Date/Author: 2026-09-03, implementation agent.
- Decision: `worker_binary_prerequisite_for_arch` keeps its public signature and delegates to a private `worker_binary_prerequisite_for_current(arch, current, is_file)`.
  Rationale: Milestone C's behavior depends on whether the controller's own path still exists, which is untestable while the function reads `std::env::current_exe()` and the real filesystem itself. Passing the controller path and a file probe makes both new behaviors testable without touching the machine. The environment-variable branches stay inside the function so that a test can still prove they survive a replaced controller; that test runs in a child process, following the existing `RAW_CONVERSION_TEST_CHILD` pattern in `resume.rs`, because environment variables are process-global and the mj-controller test binary runs tests in parallel.
  Date/Author: 2026-09-03, implementation agent.
- Decision: Milestone C was added to this plan after Milestones A and B were specified, at the user's request, and is implemented as its own commit.
  Rationale: it shares the incident and the file (`worker_binary.rs`) with Milestone A but is an independent behavior: A decides *when* the worker check runs, C decides *what the check says* when the controller binary itself was replaced.
  Date/Author: 2026-09-03, user request relayed by the coordinating agent.

## Outcomes & Retrospective

All three milestones are implemented, tested, and committed on branch `hel2`.

What was achieved, measured against the purpose:

* A resume that cannot resolve a worker binary now fails before compaction. `a_resume_preflights_the_worker_binary_before_compacting` drives a real cross-harness resume with `MJ_WORKER_BINARY` pointing at a path that does not exist and asserts the failure carries the preflight context and never mentions the compaction step.
* The same 33-page transcript that cost 65 mostly-serial requests now costs 33 page requests plus one reduction request, at up to eight in flight, and with a 1 MiB page budget derived from the summarizer it would page far more coarsely than 33 pages in the first place. `page_summaries_that_fit_one_prompt_reduce_in_a_single_request` pins the reduction count at one; `independent_pages_run_at_the_compaction_concurrency_limit` pins the concurrency at eight.
* A replaced controller binary produces its own message. `a_replaced_controller_is_reported_instead_of_a_missing_worker` asserts the sibling probe is never consulted and that the message names the stale path and says to restart the daemon.

What remains: nothing in scope. Two things were deliberately left alone. `prune_old_tool_outputs` keeps its OpenCode-v2 policy untouched, as the brief required. And the derived page budget is only as good as the provider catalog: Codex publishes no `context_length`, so it is trusted to be large (1 MiB), which is safe because an oversize rejection still splits the page in half and retries.

Lesson worth carrying: the cost of this incident was not any single wrong number but the ordering of a cheap certain check after an expensive uncertain one. When a pipeline has a step that can only fail locally, run it first.

## Context and Orientation

This repository builds `mj` (historically `hel`), a controller that supervises coding-agent sessions running on "targets". A target is where the agent process actually runs: the local machine, a local Podman or Docker container, an Apple container, a remote machine over SSH, a Podman container on a remote machine over SSH, or an AWS EC2 instance. Each target runs a small "worker" binary that the controller uploads; the worker must be a Linux binary for the target's CPU architecture, and the controller finds it with `worker_binary_prerequisite_for_arch`.

A "harness" is the agent CLI itself: Claude, Codex, Grok, Kimi, or DeepSeek. Each keeps its own native session file, so moving a session from one harness to another means replaying its history as one synthetic first message. That message is the "handoff", and building it is "compaction".

The files this plan touches, all paths from the repository root:

* `mj-controller/src/hel_compaction.rs` — the compaction pipeline. It folds an archived transcript into turns, splits them into pages, asks a model to summarize each page, merges the summaries, and assembles the handoff. It knows nothing about which model answers; it talks to a `CompactionBackend` trait.
* `mj-controller/src/hel_utility_llm.rs` — chooses which cheap model answers those requests and implements `CompactionBackend` over it. A `UtilityCandidate` is one usable (profile, model) pair; `UtilityCompactionBackend` tries candidates in order and disables ones that fail hard.
* `mj-controller/src/hel_controller/resume.rs` — the resume flow. The relevant function is the one containing `let same_harness = profile.kind == archive_manifest.session.harness_kind;` (around line 753 before this change). It decides whether compaction is needed, runs it inside a `ProvisionStage::Compacting` guard, then rewrites the session record and provisions the target.
* `mj-controller/src/hel_controller/worker_binary.rs` — worker binary discovery and installation. `worker_binary_prerequisite_for_arch(arch)` (around line 690) answers "where would a worker for this architecture come from", without downloading anything; `worker_binary_for(locator, executor)` asks the live target for its architecture with `uname -m` and then downloads if needed.
* `src/hel_config.rs` — the on-disk configuration types, including `HarnessProfile` (with the optional `context_window_bytes`), `ContainerTemplate` (with the optional `platform`), and the configured `TargetTemplate`.

Two terms used below. "Page" means one chunk of rendered transcript small enough to send to the summarizer in a single request. "Handoff" means the final text handed to the new harness as its first user message; it must fit the *target harness's* context, which is a different and usually smaller number than what the summarizer can read.

## Plan of Work

### Milestone A: the worker binary is checked before compaction

Scope: one new function in `mj-controller/src/hel_controller/worker_binary.rs` and one call to it in `mj-controller/src/hel_controller/resume.rs`. Nothing else moves. At the end of this milestone a resume that cannot produce a worker binary fails in milliseconds rather than after minutes of compaction, and the message it prints is the one the user could already act on.

In `worker_binary.rs`, near `worker_binary_prerequisite_for_arch`, add:

    /// Architectures a resume must be able to serve, knowing only the
    /// configured template. Provisioning learns the real answer by running
    /// `uname -m` on the live target; a resume has no target yet.
    fn preflight_architectures(template: &hel::hel_config::TargetTemplate) -> Vec<&'static str>

It returns a one-element vector holding the architecture named by a container `platform` when the template has one; otherwise a one-element vector holding `std::env::consts::ARCH` for `LocalBare`, `LocalPodman`, `LocalDocker`, and `AppleContainer`; otherwise `vec!["x86_64", "aarch64"]` for `SshBare`, `SshPodman`, and `AwsEc2`. The platform string is matched loosely: any slash-separated component equal to `x86_64`/`amd64` maps to `x86_64`, and `aarch64`/`arm64` maps to `aarch64`, so `linux/arm64`, `arm64`, and `linux/arm64/v8` all work.

Then:

    pub(super) fn preflight_worker_binary(template: &hel::hel_config::TargetTemplate) -> Result<()>

It succeeds if `worker_binary_prerequisite_for_arch` succeeds for any of those architectures. It downloads nothing: a `WorkerBinaryAvailability::Remote` answer counts as available, because the download happens later in provisioning and is not what this check is about. On failure it returns the last error with the context `preflight the worker binary before resuming`, so the user still reads the actionable "install mj-worker-… beside mj, set MJ_WORKER_DIR/MJ_WORKER_BINARY, or configure MJ_WORKER_URL and MJ_WORKER_SHA256" text.

In `resume.rs`, immediately before `let same_harness = ...`, insert the call with a comment saying why it is there:

    super::worker_binary::preflight_worker_binary(target_template)?;

`target_template` is already in scope at that point, bound from `self.config.targets.get(target_id)`.

### Milestone B: compaction makes few, wide, concurrent requests

Scope: `mj-controller/src/hel_compaction.rs`, `mj-controller/src/hel_utility_llm.rs`, and the compaction call site in `resume.rs`. At the end of this milestone the number of model requests a compaction makes is driven by the summarizer's real context window and by how much can be merged per request, rather than by an unset configuration field and a binary tree.

Five changes, in the order they are easiest to make:

1. Concurrency. Replace both `try_buffered(2)` calls with `try_buffered(COMPACTION_CONCURRENCY)` where `pub const COMPACTION_CONCURRENCY: usize = 8;` sits beside the other constants at the top of `hel_compaction.rs`.

2. Two budgets. Introduce

        pub struct CompactionBudget {
            pub page_bytes: usize,
            pub handoff_bytes: usize,
        }

   with `pub fn uniform(bytes: usize) -> Self`, and change `compact_snapshot(snapshot, context_bytes, backend)` to `compact_snapshot(snapshot, budget, backend)`. `page_bytes` bounds every prompt sent to the summarizer: the single-request fast path, the page limit inside the paging phase, the fragments of an oversize turn, and the reduction prompts. `handoff_bytes` keeps the old meaning: the size check inside `handoff()`, the `context_bytes / 3` reserve inside `exact_tail_start`, and the "user messages alone exceed the budget" check. Both are validated against `MIN_CONTEXT_BYTES`.

3. Greedy reduction. Replace the `chunks(2)` tree in `reduce_summaries` with `pack_reduction_groups(&summaries, page_bytes)`, which walks the summaries in order and starts a new group whenever adding the next summary would push `reduction_prompt(&group).len()` past `page_bytes`. A group of one passes through without a request. A single summary that cannot fit a prompt on its own is still an error, with the existing message. A round that fails to reduce the count at all is an error too, or the loop would never end.

4. A page budget derived from the summarizer. `anvil_llm::llm_client::ModelMetadata` carries `pub context_length: Option<u32>`, in tokens, and is `None` for Codex. Give `UtilityCandidate` a `pub page_bytes: usize` computed in `UtilityLlmRuntime::resolve` from the chosen metadata: `min(MAX_PAGE_BYTES, tokens * 4 / 2)` when the window is published — four bytes per token, and half the window left for the system prompt and the response — `MAX_PAGE_BYTES` when it is not published and the harness is Codex, whose GPT-5 family window is far larger than the cap, and `DEFAULT_CONTEXT_BYTES` otherwise. Define `pub const MAX_PAGE_BYTES: usize = 1024 * 1024;`. Give `UtilityCompactionBackend` a `pub fn page_bytes(&self) -> usize` returning the minimum over its candidates, because failover means any candidate may serve any request, clamped up to `MIN_CONTEXT_BYTES`. In `resume.rs`, build the backend first and pass `CompactionBudget { page_bytes: backend.page_bytes(), handoff_bytes: context_bytes }`.

5. One log line. At the point where the paging decision is known, and always before the requests it describes, emit a single `tracing::info!` carrying the rendered transcript size, the page count, both budgets, and whether the single-request path was taken.

### Milestone C: a replaced mj binary is reported, not silently mis-resolved

Scope: `worker_binary_prerequisite_for_arch` in `mj-controller/src/hel_controller/worker_binary.rs`. At the end of this milestone a controller whose own binary was renamed or deleted out from under it says exactly that.

Split the body into `worker_binary_prerequisite_for_current(arch, current, is_file)`, keeping the public `worker_binary_prerequisite_for_arch(arch)` as a thin wrapper that supplies `std::env::current_exe()` and a real `|path| path.is_file()` probe. Inside, when `current` is not an existing file, skip the `select_sibling_worker` branch entirely — its search directory is derived from a path that no longer means anything — and, if every other branch also fails, replace the generic message with

    the running mj binary was replaced or removed on disk ({path}); restart the Mjolnir daemon so it runs the current build, then retry

naming the stale path with any trailing ` (deleted)` marker trimmed for display only. The decision itself is made with `is_file`, never by looking for that suffix. Every branch that does not depend on the controller's directory keeps working: `MJ_WORKER_BINARY`, `MJ_WORKER_DIR`, the `MJ_WORKER_URL` remote source, and the two "native mj binary" branches, which recover through `stable_running_executable`'s `/proc/self/exe` copy. When `current` is a file, the generic "no Linux worker for {triple}" message is unchanged.

## Concrete Steps

Run everything from the repository root, `/home/jonathan/Projects/hel2`, on branch `hel2`.

Merge master first:

    git fetch origin master
    git merge origin/master

Expected, when nothing else has landed:

    Updating 3d1929d..1390be2
    Fast-forward

Then implement each milestone in order and, after each, run:

    cargo fmt
    cargo test
    cargo clippy --all-targets -- -D warnings

`cargo test` must run outside any sandbox with normal permissions; the suite binds loopback TCP and Unix sockets, and a sandboxed run fails with `EPERM` or hangs, which is not a test result. Do not redirect Cargo output elsewhere.

Commit each milestone separately, staging only the files that milestone touched. Do not use `git add -A`, do not create a branch, and do not push.

## Validation and Acceptance

Run `cargo test`. Expect the whole workspace to pass, and specifically these new or renamed tests.

Milestone A, in `mj-controller/src/hel_controller/worker_binary.rs`:

* `preflight_reads_the_architecture_a_template_names` — a Podman template with `platform: Some("linux/arm64")` preflights `aarch64` only; `linux/amd64` gives `x86_64`; `linux/arm64/v8` still gives `aarch64`.
* `preflight_uses_the_host_architecture_for_a_local_target` — `LocalBare` and a platformless `LocalPodman` preflight exactly `std::env::consts::ARCH`.
* `preflight_accepts_either_linux_architecture_for_a_remote_target` — `SshBare`, `SshPodman` without a platform, and `AwsEc2` preflight both `x86_64` and `aarch64`, so either resolving is enough.

Milestone A, in `mj-controller/src/hel_controller/resume.rs`:

* `a_resume_preflights_the_worker_binary_before_compacting` — a cross-harness resume (Codex archive, Claude profile) in a child process with `MJ_WORKER_BINARY` set to a path that does not exist fails with a message containing `preflight the worker binary before resuming` and *not* containing `compact the cross-harness handoff transcript`, and leaves the session `Stopped`. Before the change this test fails, because the resume gets as far as utility-model discovery and reports a compaction error instead.

Milestone B, in `mj-controller/src/hel_compaction.rs`:

* `independent_pages_run_at_the_compaction_concurrency_limit` — the renamed concurrency test, now with at least sixteen pages, asserting the observed maximum in flight equals `COMPACTION_CONCURRENCY` and that nothing is left running.
* `page_summaries_that_fit_one_prompt_reduce_in_a_single_request` — with many pages whose summaries all fit one reduction prompt, exactly one prompt containing `Merge these contiguous historical state snapshots` is sent.
* `a_wide_page_budget_summarizes_in_one_request_under_a_small_handoff` — a transcript far larger than the handoff budget but smaller than the page budget takes exactly one request, and the handoff still respects `handoff_bytes`.
* `reduction_that_cannot_pack_any_pair_is_an_error` — the progress guard.

Milestone B, in `mj-controller/src/hel_utility_llm.rs`:

* `page_bytes_follow_the_summarizer_context_window` — a published window of 400 000 tokens gives 800 000 bytes; 2 000 000 tokens is capped at `MAX_PAGE_BYTES`; an unpublished window on Codex gives `MAX_PAGE_BYTES`; an unpublished window on another harness gives `DEFAULT_CONTEXT_BYTES`.
* `backend_page_bytes_take_the_smallest_candidate` — including the clamp up to `MIN_CONTEXT_BYTES`.

Milestone C, in `mj-controller/src/hel_controller/worker_binary.rs`:

* `a_replaced_controller_is_reported_instead_of_a_missing_worker` — with a controller path that does not exist, the sibling probe is never called (the test's probe records every path it is asked about) and the error names the stale path and says to restart the daemon.
* `a_present_controller_still_reports_a_missing_worker_plainly` — with a controller path that does exist and no worker anywhere, the message is still the generic `no Linux worker for {triple}` one.
* `a_replaced_controller_still_honors_the_worker_binary_override` — in a child process with `MJ_WORKER_BINARY` pointing at a real file, a stale controller path still resolves that override.

Beyond the tests, the user-visible acceptance is the incident itself: with no worker binary for the target architecture, `mj` resume of a cross-harness session prints the worker message immediately instead of sitting in `Compacting`. In the controller log, one `compaction paging decided` line now precedes the model requests and states how many pages there will be.

## Idempotence and Recovery

Every step is a source edit plus a test run, so all of it is safe to repeat. The merge in the first step is a fast-forward and re-running it is a no-op. Nothing here migrates data, writes to a target, or touches a user's configuration. If a milestone's tests fail, the milestone can be reverted on its own: the three commits are independent, and Milestone A's call site is a single line.

## Artifacts and Notes

The incident's shape, from the controller log, before the change:

    17:15:38Z utility compaction request completed profile_id="codex3" model="gpt-5.6-luna"
    … 63 more …
    17:29:38Z utility compaction request completed profile_id="codex3" model="gpt-5.6-luna"
    17:29:39Z resume failed: no Linux worker for x86_64-unknown-linux-musl; install mj-worker-x86_64-unknown-linux-musl beside mj, …

The arithmetic that produced 65: 256 KiB pages over that transcript gave 33 pages; a binary reduction of 33 leaves costs 32 internal merges; 33 + 32 = 65.

## Interfaces and Dependencies

In `mj-controller/src/hel_compaction.rs`, at the end of this work:

    pub const COMPACTION_CONCURRENCY: usize = 8;
    pub const MIN_CONTEXT_BYTES: usize = 32 * 1024;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CompactionBudget {
        pub page_bytes: usize,
        pub handoff_bytes: usize,
    }

    impl CompactionBudget {
        pub const fn uniform(bytes: usize) -> Self;
    }

    pub async fn compact_snapshot(
        snapshot: &CanonicalSessionSnapshot,
        budget: CompactionBudget,
        backend: &impl CompactionBackend,
    ) -> Result<String>;

In `mj-controller/src/hel_utility_llm.rs`:

    pub const MAX_PAGE_BYTES: usize = 1024 * 1024;

    pub struct UtilityCandidate {
        // … existing fields …
        pub page_bytes: usize,
    }

    impl UtilityCompactionBackend {
        pub fn page_bytes(&self) -> usize;
    }

In `mj-controller/src/hel_controller/worker_binary.rs`:

    pub(super) fn preflight_worker_binary(
        template: &hel::hel_config::TargetTemplate,
    ) -> Result<()>;

    pub fn worker_binary_prerequisite_for_arch(arch: &str) -> Result<WorkerBinaryAvailability>;

    fn worker_binary_prerequisite_for_current(
        arch: &str,
        current: &Path,
        is_file: &dyn Fn(&Path) -> bool,
    ) -> Result<WorkerBinaryAvailability>;

No new crates, no new workspace members, no new configuration keys.

## Revision Notes

* 2026-09-03, first version: written from the incident evidence before any code changed, covering Milestones A and B.
* 2026-09-03, second version: Milestone C added at the user's request, with its own evidence paragraph in the Purpose section, its own acceptance tests, and a Decision Log entry recording that it was a later addition. The Interfaces section gained the split of `worker_binary_prerequisite_for_arch`, because Milestone C's behavior is only testable once the controller path and the file probe are parameters.
* 2026-09-03, third version: Progress, Surprises & Discoveries, and Outcomes filled in after implementation. The progress guard in the reduction loop and the `MIN_CONTEXT_BYTES` clamp on the derived page budget were both discovered while implementing Milestone B and are recorded as decisions rather than silently added.
