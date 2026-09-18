# Make sub-agent spawn either work or say exactly why it failed

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`,
`Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

The rules for ExecPlans in this repository are in `.agents/PLANS.md`, at the repository
root under `.agents/`. This document must be maintained in accordance with that file.

## Purpose / Big Picture

Today a parent session can call the `spawn` tool, be told the spawn succeeded, and end up
with a child session that is permanently `Error` with the message `worker relay did not
accept a connection in 30s` and an empty `worker.log`. The parent model cannot tell whether
the child died, is slow, or was never eligible, and it cannot recover the child. That is
issue #1065.

After this work, a sub-agent spawn on every target kind the maintainer uses (a `local-bare`
target, a local Podman target, and the `ssh-podman` target on host `morannon`) does one of
exactly two things:

* The child starts and takes its first prompt.
* The spawn fails with a specific reason that names the startup step that failed, the
  reason is stored on the child record, and the same reason is the text the parent model
  reads back from the `mj-agents` `wait` and `list_agents` tools.

There is no third outcome. In particular there is no "timed out with no evidence", because
a worker will no longer be able to die or stall before it has written anything, and because
the readiness wait will watch the worker process and its recorded startup progress rather
than only a clock.

There is a second, equally important outcome. Starting a session stops doing expensive and
destructive things to the user's Git repository. Today a session start reads, hashes and
writes a Git object for every untracked file in the working directory, into the user's own
`.git`, and pins them with two refs so garbage collection cannot remove them; one abandoned
attempt left 243 MiB of unreachable objects and a 70 MB stray index file in a real
repository (see `Artifacts and Notes`). After this work, starting a session reads no file
the session has not changed, and Mjolnir writes no object and no ref into the user's
repository at all, at start, at review, or at diff.

You can see the difference with one command pair. Before the change, spawning a child whose
working directory is a repository with a very large untracked working tree fails after 35
seconds with an unexplained timeout. After the change, the same spawn succeeds, and a
deliberately broken spawn (for example, a worker binary that cannot run in the target)
fails in under a second with the loader error as its reason.

## Progress

- [x] (2026-09-17 23:30Z) Mapped the whole spawn path with file and symbol (see `Context and
      Orientation`).
- [x] (2026-09-17 23:45Z) Built the failure inventory and marked each entry confirmed,
      likely, or possible on current master (see `Failure inventory`).
- [x] (2026-09-17 23:55Z) Reproduced the #1065 signature exactly, without any model API
      cost, by running the real worker binary against a repository whose `git add` blocks
      (see `Artifacts and Notes`).
- [x] (2026-09-18 00:05Z) Confirmed from the filesystem that the reported failing workspace
      has never completed a review-baseline pin, and the reported succeeding workspace has.
- [x] (2026-09-18 01:50Z) Milestone 1 (de9b1cb2): worker startup breadcrumbs, so a worker can never die or stall silently.
- [x] (2026-09-18 02:10Z) Milestone 2 (5723c397): readiness wait that watches the process and the breadcrumbs.
- [x] (2026-09-18 01:10Z) After maintainer review of e5b11913: established what the review
      baseline is for and who reads it, measured the object-store cost, listed every other
      whole-tree walk, and replaced the bounding approach with a root-cause design.
- [x] (2026-09-18 01:20Z) Reproduced the object-store growth on the unfixed build: a scratch
      repository with 3,000 untracked files gained 3,002 objects and two `refs/hel` refs from
      one session start.
- [x] (2026-09-18 02:00Z) Maintainer decisions recorded: capture only for reviewed parent
      sessions, refuse above 50,000 untracked paths, doctor reports and deletes nothing.
- [x] (2026-09-18 03:10Z) Milestone 3a, as option (b) (6c180949): capture writes objects into a worker-owned object directory, never the
      user's repository; the two `refs/hel` refs are replaced by `review-baselines.json`.
- [x] (2026-09-18 02:40Z) Milestone 3b (35a6360b): no capture at all unless the session is reviewed; when it is, the
      baseline costs one stat-walk plus the dirty tracked files.
- [x] (2026-09-18 02:55Z) Milestone 3c (69745068): review-time capture costs only the turn's changed paths.
- [x] (2026-09-18 03:05Z) Milestone 3e (7375d17f): refuse a session whose workspace review cannot cover.
- [x] (2026-09-18 03:30Z) Milestone 8 (eeb44e4f, cd5c9907): `mj doctor` reports stray `refs/hel/*` and `hel-review-index-*`.
- [x] (2026-09-18 02:25Z) Milestone 4 (e4711938): carry the real reason back to the parent model.
- [x] (2026-09-18 03:15Z) Milestone 5 (d0a38a23): bounded automatic retry, and a child the parent can act on.
- [x] (2026-09-18 03:20Z) Milestone 6 (b05b3c72): retire the relay actor of a session that reached a terminal state.
- [x] (2026-09-18 03:25Z) Milestone 7 (3c6e6952): re-resolve the worker source per session instead of only at daemon start.
- [x] (2026-09-18 04:30Z) Live acceptance run: `local-bare` 24 of 24 and `morannon`
      ssh-podman 7 of 7 with nothing failing; local Podman 11 of 14, see
      `Outcomes & Retrospective`.
- [ ] Local Podman: the two remaining failure modes. Both are named, and neither should
      happen: a concurrent start that still exceeds the 300-second harness wait, and a
      spawn refused with "sub-agent ... has no child session" after earlier children were
      closed.

## Surprises & Discoveries

- Observation: the liveness probe identified a worker by its command line, so a worker whose
  command line does not carry the expected text was reported as gone while it was running.
  Evidence: an injected stuck worker came back as "the worker process is gone" rather than
  as stuck on its step. Fixed in cd5c9907 by believing the pid the worker records in its
  startup file, which the launch clears, so that pid can only be this launch's.

- Observation: a daemon pins `MJ_WORKER_BINARY` from the environment it was started with.
  Changing it for a later `mj new` does nothing until the daemon restarts, which cost one
  confusing injected-failure result before it was understood.

- Observation: the reported cause in #1065 ("the 30 s relay timeout may be too short when
  the codex bridge has to run `npx -y @brokkai/codex-acp` on a cold cache") is wrong. The
  managed harness is resolved after the control socket is bound, not before.
  Evidence: in `mj-worker/src/worker_runtime/unix.rs`, `run_daemon` calls
  `bind_unix_listener(&socket)` at line 191, and `super::harness::resolve(...)` at line 259,
  after it. A cold `npx` therefore delays the 300-second native-session readiness wait
  (`NATIVE_SESSION_STARTUP_TIMEOUT` in `mj-controller/src/controller/readiness.rs`), not the
  30-second relay connect wait that actually failed.

- Observation: what does run before the socket is bound is a full Git capture of the
  session's working tree, and it is unbounded.
  Evidence: `run_daemon` calls `crate::review::capture::initialize_review_baselines` at
  `mj-worker/src/worker_runtime/unix.rs:131`, inside a `spawn_blocking` that is awaited with
  `??`, before `write_worker_pidfile` (line 182) and `bind_unix_listener` (line 191).
  `initialize_review_baselines` (`mj-worker/src/review/capture.rs:69`) calls
  `capture_worktree_tree` (`mj-checkpoint/src/archive/git.rs:327`), which runs
  `git add -A -- .` against a scratch index. On a working tree with hundreds of thousands of
  untracked files that takes many minutes.

- Observation: an empty `worker.log` is the normal appearance of a healthy worker, not a
  sign of death.
  Evidence: `install_stderr_logging` in `mj-worker/src/main.rs` defaults the filter to
  `warn` when `RUST_LOG` is unset, and nothing in the pre-socket path logs at `warn`. So
  "empty `worker.log`" in the #1065 report carries no information at all. This is why the
  reporter could not distinguish "died instantly" from "still working".

- Observation: the readiness wait can only notice a dead worker if that worker wrote an exit
  record. A worker that is killed by a signal, or that is simply slow, is indistinguishable
  from one that is about to succeed.
  Evidence: `connect_to_starting_worker` in `mj-controller/src/controller/readiness.rs:212`
  consults `probe.death_report()`, and `StartingWorkerConnection::death_report` returns
  `None` unless `worker_last_words` output contains `--- worker-exit.json ---`. The same
  probe already collects an `alive`/`absent` process section
  (`mj-controller/src/controller/worker_binary/process.rs:212`), but the readiness loop
  throws that part away.

- Observation: the capture writes Git objects into the user's own repository and pins them
  so garbage collection cannot remove them.
  Evidence: `capture_worktree_tree` (`mj-checkpoint/src/archive/git.rs:327`) redirects only
  the index, with `GIT_INDEX_FILE`. It sets no `GIT_OBJECT_DIRECTORY`, so every blob that
  `git add -A` hashes and every tree that `git write-tree` builds lands in the repository's
  real object store. It then calls `pin_review_tree(runner, repository, REVIEW_CAPTURE_REF,
  &tree)`, which runs `git update-ref refs/hel/review-capture <tree>` in the user's
  repository. Measured at controlled small scale: one session start in a scratch repository
  with 3,000 untracked files took its object store from 3 objects and 12 KiB to 3,005
  objects and 11.8 MiB, and created `refs/hel/review-baseline` and `refs/hel/review-capture`.

- Observation: the expensive part is untracked files, not tracked ones.
  Evidence: `capture_worktree_tree` copies the repository's real index into the scratch
  index before running `git add -A`, so Git's stat cache lets it skip re-hashing unchanged
  tracked files. Tracked files cost one `stat` each; untracked files cost a full read, hash
  and object write each. In the reported workspace that is 566,005 cheap entries and
  424,715 expensive ones. This is what makes a cheap baseline possible at all.

- Observation: the real cause is computed and stored, then discarded on the way to the
  parent model.
  Evidence: `provision_subagent_session_controlled`
  (`mj-controller/src/controller/provisioning.rs:149`) writes
  `sub-agent startup failed: <full cause>` into `record.last_error`, and `subagent_status`
  (`mj-controller/src/server_runtime/api.rs:731`) would return it for
  `SessionState::Error`. But the first line of `subagent_status` returns early for
  `StartStatus::Failed`, and the first-prompt follow-up fails first with the generic
  `session … is Error and will not take a first prompt` produced by `still_starting`
  (`mj-controller/src/server_runtime/api.rs:779`), which reads only the state and never the
  `last_error`. The parent model therefore sees the least informative of the two messages.

- Observation: `start_worker` proves nothing about whether the worker started.
  Evidence: `start_worker_command`
  (`mj-controller/src/controller/worker_binary/process.rs:57`) builds
  `nohup … >worker.log 2>&1 </dev/null &` for bare targets and `podman exec --detach` for
  container targets. Both return success as soon as the launcher returns.

## Decision Log

- Decision: treat #1065 as a design defect in worker startup ordering, not as a timeout
  that needs a larger number.
  Rationale: the blocking step is unbounded. Any constant chosen for
  `WORKER_STARTUP_CONNECT_TIMEOUT` is wrong for some workspace. Reproduction showed the
  blocking step is `git add -A -- .` over the session's working tree, which is proportional
  to the user's workspace and not to anything Mjolnir controls.
  Date/Author: 2026-09-17, plan author.

- Decision (superseded on 2026-09-18, kept for the record): keep the review baseline as it
  is, but stop letting it gate the control socket, run it concurrently and bound it at 120
  seconds.
  Rationale at the time: the invariant the baseline needs is "pinned before the harness can
  edit anything", not "pinned before the controller can connect".
  Why it was wrong: it leaves a session start that runs `git add -A -- .` over the user's
  working tree. That walk hashes and **writes** a Git object for every untracked
  non-ignored file into the user's own repository, and pins them against garbage collection
  with two refs. Moving it off the critical path hides the cost; it does not remove it. The
  maintainer's review named this directly: spawn does potentially pathologically bad Git
  things, and the root cause has to be fixed rather than bounded.
  Date/Author: 2026-09-17, plan author.

- Decision (maintainer, 2026-09-18): a session that is not reviewed does no Git capture at
  session start at all, not even a stat-walk. The predicate is
  `config.review.reviewer_profile().is_some()` (`mj-core/src/config.rs:219`, "whether a turn
  review can run at all: it needs a reviewer, armed or not"), and a sub-agent child is never
  reviewed, so a child never does baseline work and never reads a parent's baseline.
  Rationale: work done for a feature the session cannot use is waste, and it was the whole
  of the #1065 failure. This also removes the need for a per-child inheritance rule.
  Date/Author: 2026-09-18, maintainer decision 1.

- Decision (maintainer, 2026-09-18): when review is configured and the working tree has more
  than 50,000 untracked paths, refuse to start the session with a precondition refusal
  naming the count and the limit. No partial-coverage mode.
  Rationale: a review that silently does not cover untracked files is a review the user
  cannot trust. Refusing is honest and the user has two clear remedies: clean the tree, or
  run the session without review. The refusal mechanism is `mj_core::refusal::Refusal` from
  #1057 (commit f0ad7d1e), which answers 409 with the sentence instead of a generic 500.
  Date/Author: 2026-09-18, maintainer decision 2.

- Decision: session start must not walk or stage the whole working tree, and Mjolnir must
  never write objects into the user's repository.
  Rationale: measured on the workspace in the report, one abandoned capture left 243 MiB of
  unreachable loose objects, a 70 MB scratch index file, and an abandoned temporary object
  inside `/home/jonathan/Projects/brokkbench/.git` (see `Artifacts and Notes`). At session
  start, all the baseline actually needs is the pre-turn content of files the turn can
  change, which is the dirty tracked set, plus a record of which untracked paths already
  existed. Both are proportional to the session's changes, not to the tree. Objects that
  capture does need go into a worker-owned object directory with the repository's own
  object store as a read-only alternate, so the user's store never grows.
  Date/Author: 2026-09-18, plan author, after maintainer review of e5b11913.

- Decision: put the diagnostics commits first and the behaviour fixes after.
  Rationale: every later failure becomes explainable the moment the breadcrumbs and the
  process-aware wait land, including failures this plan has not predicted. It also means
  that if the maintainer stops after Milestone 2, he still gains "spawn tells me why".
  Date/Author: 2026-09-17, plan author.

- Decision: do not attempt to fix #1019 (ssh-podman host kernel limits) here.
  Rationale: the maintainer's own analysis in #1019 concludes those are host-side limits
  and belong in `mj doctor`, and the memory note `host-limits-doctor-only` records the rule
  that keys-quota and `MaxStartups` facts belong in `mj doctor` only. This plan only has to
  make a spawn that hits such a limit report that limit as its reason.
  Date/Author: 2026-09-17, plan author.

## Outcomes & Retrospective

### What the volume acceptance measured (2026-09-18)

Spawns were driven through the daemon's own HTTP route rather than through a parent model,
so the numbers measure the spawn path instead of a model's ability to call a tool the right
number of times. Each child ran one trivial prompt on the `deepseek` profile and was closed.

    target            set        result                 time to running
    local-bare        20 + 4     24 of 24                2.0-7.0s, mean 2.8s
    local podman      10 + 4     11 of 14                1.0-7.0s for those that came up
    morannon ssh      5 + 2      7 of 7                  2.0-5.0s, mean 4.1s

`local-bare` and `morannon` meet the bar: nothing failed, explained or otherwise. Local
Podman does not yet, and the three that did not come up are described below.

### What the run found that the plan had not predicted

**A spawn over the HTTP route answered 404 for a child that had started.** Fixed in
93de6b7b. The handler built its answer by looking the child up in the viewer snapshot,
which is republished on a tick, so a child registered milliseconds earlier was almost never
in it. Every spawn in the first volume run reported "unknown session" while every child was
in fact running. A caller that retried on 404 would have spawned a duplicate.

**Concurrent starts inside one container defeated the harness, not the worker.** Fixed in
9d19d876. Ten children started one after another each reached their harness in about seven
seconds. Four started at once left two or three of them past the 300-second harness-startup
wait, with the worker itself healthy and serving in under 200 milliseconds. A sub-agent
start on a container target now takes one of two admission slots for that container, which
turned a repeated 1-of-4 and 2-of-4 into 4 of 4 in 1, 7, 9 and 15 seconds. Bare targets have
no gate and do not need one.

**A container can be filled with the process trees of children that are on their way out.**
Closing a child is accepted asynchronously, and a harness that closes them as fast as it can
spawn them ran a container out of process slots: `sh: 1: Cannot fork`. The message is clear
and the product behaved correctly, but it is worth knowing that a close is not finished when
it is accepted.

### What is still open on local Podman

1. **A concurrent start still occasionally exceeds the 300-second harness wait.** In the
   final 10 + 4 run, two of the four concurrent starts failed this way even with the
   admission gate. The gate reduced the rate; it did not remove it. The next step is to
   measure where those 300 seconds go inside the container, because the worker is serving
   within 200 milliseconds and the wait is entirely the harness bridge.

2. **A spawn was refused with "sub-agent ... has no child session".** This comes from
   `State::validate` in `mj-core/src/state.rs`: the loaded state holds a sub-agent relation
   whose child session is not in the loaded session map. It appeared only after earlier
   children had been closed, so it is a residue problem. Two candidate mechanisms, neither
   confirmed: the `ON DELETE CASCADE` on `subagent_sessions.child_session_id` not firing on
   some delete path, or `load_state`'s inner join of `sessions` with `session_contexts`
   dropping a session whose context row went first. This was deliberately not guessed at
   under time pressure: a wrong fix in state consistency is worse than the bug.

### The original purpose, measured against

A sub-agent spawn that failed used to leave a child permanently in `Error` with
"worker relay did not accept a connection in 30s", an empty log, and no way to tell a dead
worker from a slow one. Every failure in this run named its own cause: the startup step the
worker reached, the harness wait that expired, the container that ran out of process slots,
or the state check that refused. That part of the goal is met on every target kind. The
remaining work is not about diagnosis any more; it is about the two local-Podman failures
above actually not happening.

## Context and Orientation

This section assumes you have never seen this repository. Read it in full before editing.

### The words used here

* **Controller / daemon.** The long-running process started by `mj daemon` (crate
  `mj-controller`). It owns the session database and starts workers. One daemon per
  *instance*; an instance is a named configuration and data directory selected with
  `mj -i <name>`.
* **Worker.** A separate binary (`mj-worker`, installed into the target as a file called
  `hel`) that runs inside the session's target: on the same machine for a `local-bare`
  target, inside a container for a Podman target, over `ssh` for a remote one. It owns the
  harness process and a durable event journal.
* **Worker root.** A directory on the target that belongs to one session, for example
  `~/.local/share/mjolnir/workers/<session id>/`. It holds the staged worker binary `hel`,
  the launch configuration `launch.json`, the ownership stamp `ownership.json`, the staged
  harness home `profile/`, the worker's own log `worker.log`, its process id `worker.pid`,
  its control socket `control.sock`, and its journal directory `relay-journal/`.
* **Relay.** The protocol the controller speaks to a worker over `control.sock`.
* **Harness.** The coding agent the session runs: Claude Code, Codex, Kimi, Grok, Muse.
  A *profile* names one harness plus its home directory and credentials.
* **ACP bridge.** The adapter process that speaks the Agent Client Protocol to the harness.
  For the Codex harness it is `codex-acp`, resolved from `PATH` or fetched with
  `npx -y @brokkai/codex-acp@<version>`.
* **Sub-agent / child session.** A second session created by a parent session's model
  through Mjolnir's own delegation tools, running in the parent's target and workspace.
* **Target kind.** `local-bare` (same machine, no container), `local-podman`,
  `ssh-podman` (a Podman container on a remote host over ssh), and others.

### The spawn path, step by step

Each step below names the file and symbol, what can fail there, what the user sees today,
whether it is retried, and where the evidence ends up.

**1. The model calls the tool.** `mj-worker/src/subagent_mcp.rs`, function `call` and
`call_with_budget`. The worker serves an MCP server named `mj-agents` over stdio to the
harness. A `spawn` call becomes a `SubagentToolAction::Spawn` and is sent over the worker's
own Unix socket to the daemon, with a 120-second budget (`REPLY_TIMEOUT`). If the daemon
does not answer in time the model gets `unanswered_reply`, which says the request may still
be queued and to repeat it with the same `request_key`. No retry happens on its own. No
evidence is written anywhere except the harness transcript.

**2. The daemon handles the action.** `mj-controller/src/server_runtime/api.rs`, the
`SubagentToolAction::Spawn` arm (around line 360). It validates the model and effort
selectors against the warm profile catalogue only, builds the child's first prompt with
`build_subagent_prompt`, then calls `start_subagent` and `start_followup`. The tool result
returned to the model contains only `child_session_id`, `task_name`, and `profile_id`. This
is why a spawn "reported success" in #1065: the result is produced before the child has
started.

**3. Registration.** `mj-controller/src/controller/subagents.rs`, `register_subagent`.
Checks: the request key is not empty; the parent is a Claude or Codex session; the parent
is allowed to delegate (`ensure_parent_may_delegate`); the parent is active and has a live
target; the parent is not itself a child; the profile is eligible
(`config.subagents.profile_is_eligible`); the profile is enabled; the concurrency cap
`config.subagents.max_concurrent` is not reached. Then it builds the child's target with
`borrowed_locator`: a bare child gets a sibling worker root next to the parent's, and a
container child reuses the parent's container and records `borrowed_from`. The child record
is saved in state `Provisioning`. A failure here is returned to the tool call directly and
the model sees it, which is the one part of the path that already behaves well.

**4. Lifecycle start.** `mj-controller/src/daemon/create.rs`, `start_subagent_session`.
Registers the relation, then runs `provision_subagent_session_controlled` inside
`start_or_join_lifecycle_controlled`. The executor is a
`DaemonStageReportingExecutor` wrapping a `CancellableProcessExecutor`, which reports
provisioning stages to the UI.

**5. Placement and file staging.** `mj-controller/src/controller/provisioning.rs`,
`provision_subagent_session_controlled`, then
`mj-controller/src/controller/worker_binary/launch.rs`, `worker_placement` and
`prepare_worker_files`. This writes `launch.json` and `ownership.json`, stages the harness
home into `profile/`, picks the worker binary with `worker_binary_for`, installs the files
into the worker root with `install_worker_files`, and finally runs
`prepare_installed_managed_harness`, which is where a managed harness such as `codex-acp` is
fetched with `npx` **before** the worker is started. Failures here are returned and reach
`record.last_error`.

**6. Worker binary selection.** `mj-controller/src/controller/worker_binary/binary_source.rs`
and `binary_select.rs`. The daemon pins its worker sources once, at startup, in
`pin_worker_binary_sources`. If the directory those sources came from is deleted after the
daemon starts, every later session fails with "worker source for x86_64 (PortableLinux) was
unavailable when the daemon started". That is issue #1068.

**7. Launch.** `mj-controller/src/controller/worker_binary/process.rs`, `start_worker` and
`start_worker_command`. For a bare target this is
`sh -c 'rm -f …/worker-exit.json …/control.sock; nohup <root>/hel worker run --root <root>
--config <root>/launch.json >…/worker.log 2>&1 </dev/null &'`. For a container target it is
`podman exec --detach … sh -c 'exec … >worker.log 2>&1'`. Both return success as soon as
the launcher returns, so this step cannot fail in any useful sense.

**8. What the worker does before it is reachable.** `mj-worker/src/main.rs`, `main` →
`run_worker` → `bootstrap_login_environment` → `run_command` → `run_daemon`. In order:
install a stderr subscriber filtered at `warn` unless `RUST_LOG` says otherwise; parse the
command line; install the panic hook that writes `worker-exit.json`; capture a clean login
environment by running the account's login shell with a five-second bound
(`mj_core::login_environment::discover`, which uses `BoundedProcessExecutor::new(timeout)`);
re-exec itself with `env_clear()` and that environment; build a Tokio runtime; call
`lead_process_group()` (a `setsid`, so that until this point the worker is still in the
launcher's process group); then, in `run_daemon`
(`mj-worker/src/worker_runtime/unix.rs:80`):

    enforce_execution_policy
    create the worker root
    refuse a second daemon if control.sock already answers
    remove a stale control.sock
    initialize_review_baselines   <-- unbounded git add -A over the workspace
    DurableRelay::open            <-- recovers the journal, unbounded on a large journal
    write_worker_pidfile          <-- worker.pid appears here
    remove a stale worker-exit.json
    bind_unix_listener            <-- control.sock appears here, the worker is now reachable
    …
    login_environment::resolve, configure_github_cli
    harness::resolve              <-- managed npx harness resolution happens HERE, after bind
    start the ACP supervisor and the bridge

Nothing between process start and `bind_unix_listener` writes anything a controller can
read, and nothing in it is bounded.

**9. The readiness wait.** `mj-controller/src/controller/readiness.rs`. `connect_started_worker`
calls `connect_to_starting_worker` with `WORKER_STARTUP_CONNECT_TIMEOUT`, which is 30
seconds (`readiness.rs:19`), retrying every 500 ms. It gives up with `worker relay did not
accept a connection in {waited}s` (`readiness.rs:282`). It stops early only if
`death_report()` finds a `worker-exit.json`. It never asks whether the process is alive,
even though the probe it calls already answers that.

**10. The bridge handshake and first prompt.** After the relay connects,
`wait_for_native_session_in_stage` waits up to `NATIVE_SESSION_STARTUP_TIMEOUT` (300
seconds) for the harness to report its session. Separately,
`mj-controller/src/server_runtime/api.rs`, `apply_followup`, applies the model, the effort,
and the first prompt, guarded by `still_starting`.

**11. Failure handling.** `provision_subagent_session_controlled` stops the worker, logs a
warning, sets the child record to `SessionState::Error`, and stores
`sub-agent startup failed: <cause>` in `last_error`. There is no retry at any level. The
child cannot be re-prompted: `still_starting` refuses, so the parent's `wait` returns
`session … is Error and will not take a first prompt`.

### Where evidence ends up today

* `sessions.last_error` in `~/.local/share/mjolnir/instances/<name>/mj.sqlite3` holds the
  full cause. This is the only place the real reason survives.
* The `api_events` table holds an `error` row for the child.
* `worker.log` in the worker root holds the worker's stderr, which is empty unless
  something logged at `warn` or the process printed to stderr.
* `worker-exit.json` in the worker root holds a structured exit record, written by the
  panic hook and by `main` when `run_worker` returns an error.
* The daemon log holds nothing about sub-agent startup at the default level.

## What the review baseline is for

Turn review is the feature that tells the user what the agent changed during a turn. It has
two consumers and both want Git *tree objects*, not a list of paths:

* The textual diff. `capture_repository_deltas`
  (`mj-worker/src/review/capture.rs:22`) captures the current worktree as a tree, picks a
  baseline tree, and calls `diff_between_trees` to produce the patch, the diffstat and the
  changed-line count that end up in the `RepoDelta` on the relay protocol
  (`mj-core/src/relay/protocol.rs:232`).
* The semantic analysis. The reviewer sidecar's `analyze_delta`
  (`mj-worker/src/worker_runtime/reviewer.rs:598`) builds
  `mj_review::bifrost::AnalyzeRequest { base_tree, target_tree }` from the same pair. A
  repository with no baseline is analysed against its own empty tree, which Bifrost reads as
  "everything here is new".

The question the baseline answers is: *what did this turn change, relative to the workspace
as it stood when the session started, or when the last review finished?* Two refs carry it,
both in the user's repository today:

* `refs/hel/review-baseline` (`REVIEW_BASELINE_REF`, `mj-checkpoint/src/archive/git.rs:317`)
  points at the tree the last completed review reviewed through. It is written at session
  start by `initialize_review_baselines` (`mj-worker/src/review/capture.rs:69`), and moved
  forward after each completed review by `advance_baselines`
  (`mj-worker/src/review/capture.rs:127`), reached from the controller through
  `advance_review_baseline` (`mj-controller/src/worker_client/reviewer.rs:214`) and the
  session actor (`mj-controller/src/session_manager/actor.rs:954`).
* `refs/hel/review-capture` (`REVIEW_CAPTURE_REF`) exists only to keep the most recent
  capture's objects reachable so a `git gc` between two reviews cannot collect them.

A third caller of the same capture is `session_diff`
(`mj-checkpoint/src/archive/git.rs:1249`), which backs `mj-worker worker diff`, the command
that shows a session's work. It diffs the recorded session base against a fresh full capture
of the worktree, so it pays the same cost as a review.

Which sessions pin a baseline at start: every session whose worker runs `run_daemon` without
an existing relay state file, which is every fresh session, parent or child, on every target
kind. A resumed or restarted worker skips it, because `relay_state_exists` is true. A
container session normally pays little, because its workspace is a fresh clone with almost
nothing untracked; a `local-bare` session pointed at a real project directory pays the full
price.

A sub-agent child is the worst case and the least justified one. Its `cwd` is its parent's
workspace (`session_launch_config` in
`mj-controller/src/controller/worker_binary/launch.rs` sets `launch.cwd` from the parent's
launch config), so the child re-captures a tree the parent already captured. When the
parent's pin is still present, `pinned_review_baseline` makes the child's capture a no-op;
when the child's `working_directory` points at a different repository, as in #1065, the
child pays a first full capture in a repository no session has ever captured.

## Other places that walk or stage the working tree

The fix must not leave a second walk on the same path. These are every walk found, with its
cost class. "Stat-walk" means one `stat` or `readdir` per entry and no file contents read.
"Hash-and-write" means every file's content is read, hashed, and written as a Git object
into the repository's object store.

* `initialize_review_baselines` at session start
  (`mj-worker/src/worker_runtime/unix.rs:131`) → `capture_worktree_tree`: **hash-and-write,
  on the startup critical path**. This is the defect.
* `capture_repository_deltas` at every turn review
  (`mj-worker/src/worker_runtime/reviewer.rs:560`) → `capture_worktree_tree`:
  **hash-and-write, once per review.** Same cost, on a path the user triggers rather than
  one that gates startup, but just as pathological in a large workspace. The fix must cover
  this one too.
* `session_diff` (`mj-checkpoint/src/archive/git.rs:1249`) → `capture_worktree_tree`:
  **hash-and-write**, on `mj-worker worker diff`.
* `collect_git_snapshot` for checkpoint export
  (`mj-checkpoint/src/archive/git.rs:604`): `ls-files --others --exclude-standard` followed
  by `build_untracked_tar`, which copies every untracked file into the archive.
  **Stat-walk plus a full copy**, but only when `include_untracked` is set, which comes from
  an explicit user choice in the import dialog (`mj-tui/src/dialogs.rs:1141`,
  `mj-cli/src/import.rs:640`). Leave it alone: the user asked for those bytes.
* `ensure_primary_checkout` when creating a managed raw worktree
  (`mj-controller/src/controller/worktree.rs:1401`):
  `status --porcelain=v1 --untracked-files=all`. **Stat-walk**, only for sessions that
  create a managed worktree; a sub-agent child sets `create_managed_worktree: Some(false)`
  so it never runs there.
* `dirty_file_counts` and `untracked_bytes`
  (`mj-controller/src/controller/worktree.rs:977` and `:1020`): **stat-walk**, in the
  worktree-conversion confirmation that shows the user what an archive would carry.
* `import::safety` (`mj-controller/src/import/safety.rs:8`): **stat-walk**, on import.
* `reject_dirty_submodules` (`mj-checkpoint/src/checkpoint.rs:331`): **stat-walk**, scoped
  to submodules, on checkpoint.

Only the first three are hash-and-write, and all three are the same function. Fixing
`capture_worktree_tree` and its callers fixes all of them at once; nothing else on a session
start, checkpoint, close, export or move path reads whole-file contents without the user
having asked for it.

## Failure inventory

Each entry is marked **confirmed** (evidence on current master), **likely** (strong
indirect evidence), or **possible** (mechanism exists, not observed).

1. **Unbounded review-baseline capture blocks the control socket. Confirmed.** The cause of
   #1065. `initialize_review_baselines` runs `git add -A -- .` against the session's working
   tree before `control.sock` is bound. In the workspace named in the report,
   `/home/jonathan/Projects/brokkbench`, `git ls-files --others --exclude-standard` reports
   424,715 untracked files, in a 524 GB working tree with a 43 GB `.git`. Tracked files are
   cheap, because the scratch index is seeded from the real one and Git's stat cache applies;
   the 424,715 untracked files are not, because each is read, hashed, and written as a new
   Git object. Measured hashing throughput there is 15,085 files in 30 seconds with
   `git hash-object --stdin-paths`, which does not even write the objects, so the capture
   needs at least fourteen minutes and writes hundreds of thousands of objects into the
   user's repository. The repository has no `refs/hel/review-baseline` ref at all, meaning
   the pin has never once completed there; the sibling child that succeeded ran in
   `/home/jonathan/Projects/assay`, which has 19 untracked files and does have the ref.
   Reproduced directly, and the object-store damage measured: see `Artifacts and Notes`.

2. **The readiness wait cannot tell "slow" from "dead". Confirmed.** `connect_to_starting_worker`
   only short-circuits on an exit record. A worker killed by a signal, or one that is
   progressing but slow, both produce the same generic 30-second message.

3. **A worker can be killed before it can record anything. Likely.** Until `run_command`
   calls `lead_process_group()`, the detached worker is still a member of the process group
   the controller created for the launching `sh` (`cancellable_command` in
   `mj-core/src/targets.rs:858` calls `process_group(0)`, and
   `terminate_cancellable_child` sends `SIGKILL` to that whole group). A cancellation or
   deadline that fires against a command in that group while the worker is still in its
   login-environment phase kills the worker with no stderr and no exit record. Not observed
   in the #1065 report, because the launch command returns immediately, but the window is
   real and it produces exactly the "empty log, no record" signature.

4. **The real reason is replaced by a generic one on the way to the model. Confirmed.**
   `subagent_status` prefers `StartStatus::Failed`, whose message comes from `still_starting`
   and says only `session … is Error and will not take a first prompt`.

5. **Stale or mismatched worker binary. Confirmed as a class, from the memory note
   `musl-worker-repin-trap`.** After a change to `WorkerLaunchConfig`, a container session
   dies at startup with `parse worker launch config …: unknown field '<name>'` unless the
   musl worker is rebuilt and the daemon restarted. This one does produce a good message,
   because the worker gets far enough to write an exit record.

6. **Reaped worker source directory. Confirmed, issue #1068.** A daemon whose build
   directory was garbage-collected keeps accepting sessions and fails each at bootstrap with
   "worker source … was unavailable when the daemon started". The message is good; the
   behaviour (accepting work it cannot do) is not.

7. **Host limits under concurrent launches on `ssh-podman`. Confirmed, issue #1019.** On
   `morannon`, three separate per-user kernel or daemon limits have each blocked launches:
   the session keyring quota (`kernel.keys.maxkeys`), `fs.inotify.max_user_instances`, and
   sshd `MaxStartups`. `with_ssh_admission` in `mj-core/src/targets.rs:554` already caps
   concurrent ssh invocations per destination and retries transport rejections. The
   remaining gap for this plan is that such a failure must reach the parent model as
   "the host is at its capacity limit", not as a timeout.

8. **Protocol mismatch between an older CLI and a newer daemon. Confirmed, issue #1078
   item 2.** It has a clear message today; it affects the maintainer's ability to
   investigate, not the spawn itself.

9. **Endless reconnect loop for a dead worker. Confirmed, issue #1078 item 1.** The session
   actor in `mj-controller/src/session_manager/actor.rs` backs off to
   `RECONNECT_BACKOFF_CEILING` (30 seconds) and then retries forever. Nothing retires the
   actor when its session record reaches a terminal state, so a failed child leaves a
   permanent once-per-30-seconds `ERROR` line naming a socket that will never exist.

10. **Cold `npx` resolution of the ACP bridge. Confirmed not to be the #1065 cause;
    possible as a separate failure.** It happens twice: once in the controller before the
    worker starts (`prepare_installed_managed_harness`) and once in the worker after the
    socket is bound (`harness::resolve`). Neither is inside the 30-second window. A slow or
    failing `npx` shows up as a native-session readiness failure after up to 300 seconds.

11. **Unbounded durable-journal recovery before bind. Possible.** `DurableRelay::open` runs
    before `bind_unix_listener` and is proportional to journal size. A fresh child has no
    journal, so this cannot be the #1065 cause, but a restarted worker with a large journal
    hits the same 30-second wall. There is already a separate entry point
    (`connect_started_worker_with_timeout`) used by checkpoint restarts for exactly this
    reason, which is evidence the problem is known and was patched locally rather than
    fixed.

12. **Profile not eligible, quota exhausted, parent container gone. Confirmed to be handled
    well.** These fail inside `register_subagent` or `backend_locator` and their messages
    reach the tool call synchronously.

### What the 2026-09-17 work already changed, and what it did not

The shared activity mechanism (`mj-core/src/activity.rs`, plan
`.agents/plans/unified-session-activity.md`) answers "is this session working, idle, or
stalled" from facts the worker publishes over the relay. It does not apply before the relay
exists, so it does not help startup. Startup needs its own progress channel, which
Milestone 1 adds; Milestone 2 then feeds that progress into the same shape of decision, so
there is still one answer per question rather than two.

Startup reconciliation (commit 7aea6c5c) fixes records left `Provisioning` when the daemon
stopped: at startup, a record in an in-flight lifecycle state that no owner claims becomes
`Error` with a stated cause. That covers the daemon-restart case. It does not cover a
provision that fails while the daemon keeps running, which is the #1065 case, and it does
not end the reconnect loop of #1078 item 1, because that loop belongs to a live actor of a
session that has already reached `Error`. Milestone 6 ends it.

Model re-pin at every bridge launch (21f785d2), turn-span bounded wait results (529a5144),
`mj resume` (1d5c9fad), and `RUST_LOG` reaching workers all help after startup. `RUST_LOG`
reaching the worker (in `worker_launch_config`, `mj-controller/src/controller/worker_binary/launch.rs`)
is useful here: it is the reason a maintainer can turn the breadcrumbs of Milestone 1 into
full debug logging without a rebuild.

## Plan of Work

Seven milestones. Milestones 1 and 2 are the diagnostics and land first. Milestone 3 is the
fix for the confirmed #1065 cause. Milestones 4 and 5 are what "either succeeds or fails
with an actionable reason" means at the tool boundary. Milestones 6 and 7 close the two
neighbouring issues that make failures look mysterious.

Each milestone is a separate commit, compiles on its own, and has its own test.

### Milestone 1: a worker that cannot die or stall silently

Goal: at every moment between process start and the control socket being bound, the worker
root contains a file that says which step the worker is on and when it started.

Add to `mj-worker/src/main.rs` a small module (keep it in `main.rs`; it is single-use and
the repository guidance says to keep small single-use types near their use) that writes a
file `worker-startup.json` in the worker root. Its content is a JSON object:

    {"step":"login-environment","started_at":"2026-09-18T00:01:02Z","pid":12345,
     "version":"2.10.0","steps":[{"step":"parse","at":"…"},{"step":"login-environment","at":"…"}]}

Write it with the existing `mj_core::config::atomic_write` so a reader never sees a partial
file. Record a step before each of these, in this order:

* `start` — first statement in `main`, immediately after `install_stderr_logging` and
  `Cli::parse`, before `install_worker_last_words`.
* `login-environment` — before `bootstrap_login_environment`.
* `re-exec` — immediately before the `exec` call in `bootstrap_login_environment`. The
  re-executed process writes `start` again with the same root, which is how a reader sees
  the re-exec happened.
* `runtime` — before building the Tokio runtime.
* `policy`, `review-baseline`, `durable-relay`, `bind-socket` — inside `run_daemon`, before
  each of `enforce_execution_policy`, the review-baseline block, `DurableRelay::open`, and
  `bind_unix_listener`.
* `serving` — immediately after the socket permissions are set.

For the steps inside `run_daemon`, put the writer behind one small function in
`mj-worker/src/worker_runtime/unix.rs` so `run_daemon` stays readable.

Also, in the same commit, stamp `MJ_INSTANCE` into the worker's target environment in
`worker_launch_config` (`mj-controller/src/controller/worker_binary/launch.rs`), next to the
existing `RUST_LOG` carry-over, so a worker process can be attributed to its instance from
`/proc/<pid>/environ`. This is the observability half of #1063 and it costs one line.

Test: a `#[cfg(test)]` test in `mj-worker/src/main.rs` that calls the step writer twice
against a temporary directory and asserts the file names both steps in order and that the
latest step is the last one written. Do not test the exact JSON shape beyond the fields a
reader uses.

### Milestone 2: a readiness wait that watches the worker, not the clock

Goal: the wait ends as soon as the answer is known, and its error says which step the worker
reached.

In `mj-controller/src/controller/worker_binary/process.rs`, extend `worker_last_words` so
the script also emits the startup record, under a new marker
`--- worker-startup.json ---`, before the existing exit-record section. Add a
`pub(in crate::controller)` function `worker_startup_progress` that runs the same probe and
returns a small struct with three fields: whether the worker process is alive, the name of
the latest step, and the time that step was recorded.

In `mj-controller/src/controller/readiness.rs`, replace the fixed
`WORKER_STARTUP_CONNECT_TIMEOUT` behaviour of `connect_to_starting_worker` with a wait that,
on every failed connection attempt, asks the probe for progress and then decides:

* An exit record exists: fail now with it, as today.
* The process is absent and no exit record exists: fail now with
  `the worker process is gone; it reached the step '<step>' and left no exit record`, plus
  the binary probe from `worker_binary_probe_failure`, which is what catches a loader or
  glibc mismatch.
* The process is alive and the latest step changed since the previous probe: the worker is
  making progress; extend the deadline by `WORKER_STARTUP_PROGRESS_GRACE` (60 seconds) up to
  a hard ceiling `WORKER_STARTUP_CONNECT_CEILING` (300 seconds, the same ceiling the native
  session wait already uses, so the two cannot disagree).
* The process is alive and the step has not changed for longer than the grace: fail with
  `the worker has been on the step '<step>' for <n>s without progress`.
* The hard ceiling passes: fail with the step name and the elapsed time.

Keep the 30-second value as the *initial* deadline so a genuinely dead-quiet worker still
fails fast. Probing costs one command per 500 ms attempt on a bare target and one
`podman exec`/`ssh` per attempt otherwise, which is too expensive at that rate: probe at
most once every 3 seconds, and only after the first 5 seconds have passed.

Delete `connect_started_worker_with_timeout` if the new wait makes the checkpoint-restart
caller's longer timeout unnecessary; if it is still needed, keep it and give it the same
progress logic, because a restart over a large journal is exactly the case the progress
logic is designed for.

Tests: extend the existing `FakeStartingWorker` tests in `readiness.rs` with a probe that
reports progress. Three behaviour tests: a worker that keeps changing step is waited for
past 30 seconds and then accepted; a worker whose process goes absent fails in under one
attempt interval with the step name in the message; a worker stuck on one step fails after
the grace with that step named. Use `#[tokio::test(start_paused = true)]` as the existing
tests do.

### Milestone 3: a baseline whose cost is the session's changes, not the tree

Goal: session start never reads the content of a file the session has not changed, never
writes an object into the user's repository, and never creates a ref there. Turn review and
`worker diff` get the same guarantee, because they share the same function.

This milestone has three parts. Do them as three commits in this order; each is separately
testable and the first two are useful on their own.

**3a. Stop writing into the user's object store.** In
`mj-checkpoint/src/archive/git.rs`, `capture_worktree_tree` already redirects the index with
`GIT_INDEX_FILE`. Redirect the objects the same way. Give the function a parameter naming a
Mjolnir-owned object directory, and set two more variables in the `scratch` environment it
already passes to every Git command:

    GIT_OBJECT_DIRECTORY=<worker root>/review-objects
    GIT_ALTERNATE_OBJECT_DIRECTORIES=<repository>/.git/objects

`GIT_OBJECT_DIRECTORY` is where Git writes new objects. `GIT_ALTERNATE_OBJECT_DIRECTORIES`
is a list of extra stores Git may read from. With both set, `git add`, `git write-tree`,
`git diff-tree` and Bifrost all read the repository's real history and write only into the
worker's directory. Nothing lands in the user's `.git`.

Every command that later reads a captured tree must carry the same two variables, or it will
not find the tree: `diff_between_trees`, `empty_tree_id`, the `cat-file -e` in `tree_exists`
(`mj-worker/src/review/capture.rs:111`), and the Bifrost analysis requests built in
`analyze_delta` (`mj-worker/src/worker_runtime/reviewer.rs:598`). Carry the pair in one small
struct, defined once in `mj-checkpoint/src/archive/git.rs`, so no call site can forget it.

With the objects outside the repository, the two refs lose their purpose and become harmful:
`refs/hel/review-capture` exists only to protect objects from the user's `git gc`, which can
no longer reach them, and `refs/hel/review-baseline` is a ref in someone else's repository
holding Mjolnir's own bookkeeping. Delete both. Record the baseline and capture tree ids in
a small JSON file in the worker root instead, `review-baselines.json`, keyed by repository
path. `pin_review_tree` and the `REVIEW_CAPTURE_REF` / `REVIEW_BASELINE_REF` constants go
away with them.

Note what this changes for a resumed session on a fresh target: the worker's object directory
is part of the worker root, so a moved or restored session loses its captured trees and
starts from no baseline, exactly as a repository whose `refs/hel/*` had been garbage
collected does today. That is the same outcome, reached more predictably.

**3b. Do nothing at all unless the session is reviewed, and then only what the dirty set
costs.** First the gate. Add a field `review_capture: bool` to `WorkerLaunchConfig`
(`mj-core/src/worker_launch.rs`), defaulted to `false` so an older config still parses. The
controller sets it in `session_launch_config`
(`mj-controller/src/controller/worker_binary/launch.rs`), beside the existing
`launch.subagent_tools` line, to

    self.config.review.reviewer_profile().is_some() && subagent.is_none()

`reviewer_profile()` is the repository's own answer to "can a turn review run at all"
(`mj-core/src/config.rs:219`); `subagent.is_none()` is how that function already
distinguishes a parent from a child. In `run_daemon`, skip the whole baseline block when
`review_capture` is false. A session with no `[review] profile`, and every sub-agent child,
then runs no Git command at startup whatsoever.

In the same commit, make the review host refuse a child explicitly, so the two halves cannot
drift: add to `refuse_start` (`mj-controller/src/review_host/begin.rs:175`) a check that the
session has no sub-agent record, refusing with "sub-agent sessions are not reviewed". Without
it, a `/review` on a child would ask for a delta against a baseline that was never taken.

Then, when the gate is open, replace `initialize_review_baselines` with a function that does
no content reading beyond the files that are already dirty:

1. Read `git status --porcelain=v1 --untracked-files=all -z` once. This is a stat-walk: one
   `stat` per tracked file and a `readdir` per directory, and it reads no file contents. On
   the reported 524 GB workspace the equivalent untracked listing takes 1.8 seconds warm.
2. Build the baseline tree from HEAD plus only the dirty tracked paths: `read-tree HEAD`
   into the scratch index, then `git add --` with those paths as an explicit pathspec. A
   clean checkout, which is the normal case, adds nothing and the baseline tree is simply
   HEAD's tree.
3. Do **not** put pre-existing untracked files in the baseline tree. Instead record their
   paths, sizes and modification times in `review-baselines.json` as the
   `untracked_at_start` list, so review can later tell a file created during the turn from
   one that already existed.
4. Bound the untracked list at 50,000 paths. Past that, refuse to start the session. See
   Milestone 3e; there is no partial-coverage mode.

Cost at session start becomes one stat-walk and the hashing of the dirty tracked files,
which is what the user has already changed by hand. In a clean checkout it is a walk and
nothing else. Keep it where it is in `run_daemon`, before the socket bind: at this cost there
is no reason to move it, and keeping it there preserves the invariant that the baseline
exists before the harness can edit anything, with no concurrency to reason about. Bound it
anyway with `mj_core::targets::BoundedProcessExecutor` at 120 seconds, as a backstop against
a pathological filesystem rather than as the design, and on timeout record the repository as
uncovered and continue rather than failing the session.

**3c. Make the review-time capture proportional too.** `capture_repository_deltas` has the
same problem: it captures the whole worktree as a tree on every review. Change it to capture
only what changed:

1. Read `git status --porcelain=v1 --untracked-files=all -z` again.
2. The changed set is: every tracked path `status` reports, plus every untracked path that
   is not in `untracked_at_start`, plus every untracked path that is in it with a different
   size or modification time.
3. Build the current tree from the baseline tree plus those paths: `read-tree <baseline>`
   into the scratch index, then `git add --` with the changed paths, then `write-tree`.
   Hashing is bounded by the turn's own changes.
4. Diff baseline against current as today.

Every repository reaching this point has a bounded `untracked_at_start` list, because
Milestone 3e refuses the session otherwise, so there is no fallback mode to write.

**What review loses.** One thing: for an untracked file that already existed when the session
started and that the turn modified, the baseline holds no content, so the diff shows the
whole file as an addition rather than as a modification. The `untracked_at_start` record
means review still knows it is not a new file and can label it. Everything else is
unchanged: tracked modifications, additions, deletions and renames all keep exact before and
after content, and files created during the turn are exact.

Two alternatives were considered and rejected. Deferring all capture to review time and
bounding it there keeps the pathological hash-and-write, only later, and it cannot
reconstruct the pre-turn state at all, so it loses more, not less. Recording only HEAD and
a path list with no tree, and synthesising trees entirely at review time, is very nearly the
design above but has no way to represent a checkout that was already dirty at session start,
which is the common case for a `local-bare` session on a real project directory.

Tests. In `mj-checkpoint/src/archive/git.rs`: a test that captures a worktree in a temporary
repository with an object directory set, then asserts `git count-objects -v` in the
repository is unchanged and the tree still resolves with the alternates set. In
`mj-worker/src/review/capture.rs`: a test with one dirty tracked file and several untracked
files, asserting the startup baseline hashes only the dirty tracked file (assert on the
object count in the worker's object directory) and that a later review of a turn that
modified one untracked file reports it with `untracked_at_start` set. A test with more than
the bound of untracked paths asserting the list is recorded as unbounded and review reports
tracked changes only. Keep the blocking-filter fixture from Part A as a regression test that
startup does not block.

**3e. Refuse a workspace review cannot cover.** The stat-walk of 3b also counts untracked
paths. When a repository reports more than 50,000, stop and fail the provision with

    Refusal::precondition(format!(
        "turn review cannot cover {repository}: it has {count} untracked files and the limit \
         is {LIMIT}. Commit or ignore them, or start this session without review by clearing \
         [review] profile in config.toml."
    ))

`Refusal` is `mj_core::refusal::Refusal` from #1057. A failure carrying one is answered as
409 with that sentence by `ActionOutcome`, instead of the generic 500, so `mj new` and the
browser viewer both print it.

Bound the walk itself. Run `git status` under `mj_core::targets::BoundedProcessExecutor` with
a 120-second deadline, and on timeout refuse with

    Refusal::precondition(format!(
        "turn review could not read the state of {repository} within {n}s; the working tree \
         is too large or too slow to review. Start this session without review by clearing \
         [review] profile in config.toml."
    ))

so a pathological filesystem is a refusal the user can act on rather than a hang. Keep the
walk before the socket bind: it is cheap, it is the precondition for starting at all, and
nothing after it depends on ordering.

The refusal has to reach the controller, and the walk happens in the worker. Carry it the
way worker startup failures already travel: the worker writes its exit record and exits
non-zero, and the controller's readiness path reads it. Add a `refusal` field to the exit
record that `write_worker_exit_record` (`mj-worker/src/main.rs`) writes, and have
`worker_last_words` and the Milestone 2 progress probe lift it into a `Refusal` on the error
chain so `provision_subagent_session_controlled` and the ordinary create path both answer
409. This is the one place the worker needs to say something to the user rather than to the
log.

### Milestone 4: carry the real reason back to the parent model

Goal: what the parent model reads is the cause, not a restatement of the symptom.

* In `mj-controller/src/server_runtime/api.rs`, `still_starting`: take the record, not just
  the state, and include `last_error` in the bail message, for example
  `session <id> is Error and will not take a first prompt: sub-agent startup failed: …`.
* In `subagent_status`: when the record is in `SessionState::Error` and has a `last_error`,
  prefer that over a `StartStatus::Failed` message. Keep `StartStatus::Failed` for the case
  where the record is otherwise fine and only the follow-up failed (for example, an
  unsupported model selector), because there the follow-up message *is* the cause.
* In `mj-worker/src/subagent_mcp.rs`: nothing to change in the transport, but update the
  `spawn` tool description to state plainly that the returned `child_session_id` means the
  child was registered, not that it started, and that the startup result is collected
  through `wait` or `list_agents`. The current description already hints at this for
  selectors; make it general.

Tests: a unit test on `subagent_status` asserting that an `Error` record with a `last_error`
and a `StartStatus::Failed` returns the record's cause; a unit test on `still_starting`
asserting the cause is in the message.

### Milestone 5: bounded automatic retry, and a child the parent can act on

Goal: a spawn that failed for a reason that is safe to retry is retried once, automatically;
everything else ends in a terminal state whose reason the parent can act on.

Classify the startup failure at the point where it is already handled, in
`provision_subagent_session_controlled` (`mj-controller/src/controller/provisioning.rs`).
Introduce a small enum in that module with two values, `Retryable` and `Terminal`, and one
function that maps an `anyhow::Error` to it. Retryable means the child provably did no work
that a second attempt would duplicate or corrupt, which is true when the failure happened
before the worker bound its socket: no relay, no journal, no harness, no prompt. Concretely:
a readiness failure where the probe says the process is absent or never progressed, and an
ssh transport rejection. Terminal means everything else, including a worker that bound its
socket and then failed, a launch-config parse failure, a missing worker source, an
ineligible profile, and a host-capacity refusal.

On `Retryable`, stop the worker, clear the worker root's runtime files, and run the same
provisioning once more. Cap it at one retry; record both attempts' causes in `last_error`
so a repeated failure is legible ("attempt 1: …; attempt 2: …"). Do not retry on a cancelled
operation.

Leave the child in `Error` with its cause on a terminal failure. Do **not** make an errored
child re-promptable: a child that failed startup has no relay and no journal, and re-using
its id would confuse the parent's own bookkeeping. Instead, make the failure legible enough
that the parent model's correct action is obvious: the `wait` and `list_agents` results say
`"state":"error"` with the cause as `output`, and the tool description tells the model that
the remedy is a fresh `spawn` with a new `request_key`.

Tests: a behaviour test in `mj-controller/src/controller/provisioning/tests.rs` driving
`provision_subagent_session_controlled` with a scripted executor that fails the first
readiness probe with an absent process and succeeds the second, asserting exactly two start
attempts and a `Running` child; and a second test asserting a terminal failure is not
retried and its cause is stored.

### Milestone 6: retire the relay actor of a terminal session

Goal: end the once-per-30-seconds reconnect loop of #1078 item 1.

In `mj-controller/src/session_manager/actor.rs`, in the reconnect arm, before scheduling the
next attempt, read the session record's state. If it is terminal (`Error`, `Lost`,
`Stopped`, `DestroyedWithDataLoss`), publish a final view carrying the record's `last_error`
and `break` out of the loop, the same way `WorkerRecoveryOutcome::Suppressed` already does.
The existing `Suppressed` path is the precedent for stopping an actor, so follow its shape.

Test: a session-manager test that puts a record into `Error`, drives one reconnect failure,
and asserts the actor stops rather than scheduling another attempt.

### Milestone 7: re-resolve the worker source per session

Goal: close #1068 without making daemon startup fragile.

In `mj-controller/src/controller/worker_binary/binary_source.rs`, when the pinned snapshot
for the needed architecture is an error, or when the pinned path no longer exists on disk,
re-run the resolution once at session start instead of returning the stored error. Keep the
pin as the fast path; it exists so that every session does not repeat the search. Copy the
resolved worker into the pinned cache under `data_dir()/workers/pinned` (which
`copy_worker_source_to_cache` already does) so a later reap of the build directory cannot
take it away again.

Test: a unit test that seeds a snapshot whose pinned path has been deleted and asserts the
resolution is retried and the new path returned.

### Milestone 8: report what earlier releases left in user repositories

Goal: the user can find and remove the objects, refs and stray files Mjolnir wrote into their
repositories before Milestone 3a, without Mjolnir deleting anything on its own.

Add a check to `mj-controller/src/doctor.rs` that, for each repository reachable from the
configured bundles and project directories, reports whether it holds
`refs/hel/review-baseline` or `refs/hel/review-capture`, and whether its Git directory holds
any `hel-review-index-*` file, with the sizes. Print the exact commands to clear them:

    git -C <repo> update-ref -d refs/hel/review-capture
    git -C <repo> update-ref -d refs/hel/review-baseline
    rm -f <git dir>/hel-review-index-*
    git -C <repo> gc --prune=now

Delete nothing. Deleting refs and running `gc` in someone's repository without asking is the
same class of mistake as writing to it without asking, which is what this whole milestone
sequence exists to stop.

Test: a unit test over a temporary repository with one such ref and one such file, asserting
the report names both and the commands, and that the repository is unchanged afterwards.

## Concrete Steps

All commands run from the repository root of your worktree.

Build and check, on the dev profile, outside the sandbox, as `CLAUDE.md` requires:

    cargo build
    cargo test
    cargo clippy --all-targets -- -D warnings

Note the trap recorded in the memory note `stale-worker-binary-live-tests`: `cargo build
--bin mj` and `cargo test` do **not** rebuild `target/debug/mj-worker`, and a `local-bare`
live test runs that binary. Run a full `cargo build` before any live test of worker code and
prove the running worker is current, for example:

    strings ~/.local/share/mjolnir/instances/plan1065/workers/<child id>/hel | grep worker-startup

For a container target, rebuild the musl worker as the memory note `musl-worker-repin-trap`
requires, then restart the daemon so it re-pins:

    cargo build --target-dir target/worker --target x86_64-unknown-linux-musl \
      -p brokk-mj-worker --bin mj-worker
    mj -i plan1065 daemon restart

Set up a private instance for live work:

    cp ~/.config/mjolnir/instances/campaign0916/config.toml \
       ~/.config/mjolnir/instances/plan1065/config.toml
    # edit [phone] bind to 127.0.0.1:4165, and add a local target:
    #   [targets.localhost]
    #   kind = "local-bare"

Clean up afterwards: stop the `plan1065` daemon, confirm no worker of that instance
survives (`pgrep -af instances/plan1065`, and check `MJ_INSTANCE` in
`/proc/<pid>/environ` of any `daemon-run` process), then remove
`~/.config/mjolnir/instances/plan1065` and `~/.local/share/mjolnir/instances/plan1065`.
Do not run another `mj -i plan1065` command after `daemon stop`; it starts the daemon again.

## Validation and Acceptance

Acceptance is a live test the maintainer can rerun. It has three parts. All of it must fail
on the unfixed build and pass on the fixed one.

### Part A: the #1065 regression, cheap and deterministic

This needs no model API calls and no instance. Create the blocking repository fixture:

    mkdir /tmp/slowrepo && cd /tmp/slowrepo
    git init -q .
    printf 'hello\n' > a.txt
    git add a.txt && git commit -qm init
    git config filter.slow.clean 'sleep 120 && cat'
    printf '* filter=slow\n' > .gitattributes

Write a minimal worker launch config whose `cwd` is `/tmp/slowrepo`, then run the worker
directly and watch the worker root:

    mkdir /tmp/wroot
    cat > /tmp/wroot/launch.json <<'JSON'
    {"session_id":"0123456789abcdef0123456789abcdef","harness":"codex",
     "bridge_command":"/bin/false","bridge_args":[],"harness_runtime":"ambient",
     "environment":{},"cwd":"/tmp/slowrepo","execution_policy":"unconstrained"}
    JSON
    ./target/debug/mj-worker worker run --root /tmp/wroot --config /tmp/wroot/launch.json \
      > /tmp/wroot/worker.log 2>&1 &
    sleep 35 && ls -la /tmp/wroot

Expected before the change (this is the #1065 signature, reproduced):

    -rw-r--r-- 1 user user 339 … launch.json
    -rw-r--r-- 1 user user   0 … worker.log

Expected after Milestone 1: `worker-startup.json` exists and names the step. Expected after
Milestone 3: the worker never runs the blocking filter at all, because it never stages a
file the session has not changed, so `worker.pid` and `control.sock` exist within a second
and `worker-startup.json` reports `serving`.

Then the volume half of Part A, which is the acceptance the maintainer asked for. Build a
scratch repository with half a million untracked files. Do **not** use
`/home/jonathan/Projects/brokkbench` for this; it is a real 524 GB workspace and this test
must be disposable:

    mkdir /tmp/bigrepo && cd /tmp/bigrepo
    git init -q . && git config user.email t@t && git config user.name t
    printf 'seed\n' > README.md && git add README.md && git commit -qm init
    python3 - <<'PY'
    import os
    for d in range(500):
        os.makedirs('data/%03d' % d, exist_ok=True)
        for f in range(1000):
            open('data/%03d/f%04d.txt' % (d, f), 'w').write('x' * 256)
    PY
    git count-objects -v > /tmp/before.txt

Run it twice. **With `[review] profile` unset**, start one session whose working directory is
`/tmp/bigrepo` on the `local-bare` target: the control socket must appear within seconds, the
worker must run no Git command at all, and `count-objects` must be unchanged. **With
`[review] profile` set**, the same session must be refused with HTTP 409 and a sentence
naming the untracked count and the 50,000 limit, and `count-objects` must still be unchanged.
Then check both halves of the acceptance:

    git -C /tmp/bigrepo count-objects -v > /tmp/after.txt
    diff /tmp/before.txt /tmp/after.txt        # must report no change
    git -C /tmp/bigrepo for-each-ref refs/hel  # must print nothing
    ls /tmp/bigrepo/.git | grep hel-review-index  # must print nothing

The session's control socket must appear within seconds, and `count-objects` must be
identical before and after. On the unfixed build both fail: the socket does not appear
within 30 seconds, and the object count grows by roughly the number of untracked files.
Measure the same before-and-after on the second and third turns of a review to confirm 3c:
a turn that changes three files must add a handful of objects, not half a million, and all
of them in the worker's `review-objects` directory rather than in `/tmp/bigrepo/.git`.

Remove `/tmp/bigrepo` afterwards; it is about 1 GB of inodes.

### Part B: injected failures, one specific reason each

For each injection, spawn one child and record the reason the parent's `wait` returns. Every
row must produce its own reason, and none may produce a bare timeout.

* Kill the worker process during `login-environment`: expect
  `the worker process is gone; it reached the step 'login-environment'`.
* Kill it during `review-baseline`: expect the same shape with that step named.
* Replace the staged `hel` with a binary that cannot run in the target (for a container
  target, a glibc-linked build): expect the loader error from
  `worker_binary_probe_failure`.
* Add an unknown field to the staged `launch.json`: expect
  `parse worker launch config …: unknown field`.
* Point the daemon at a worker source and delete it: expect the worker-source message, and
  after Milestone 7 expect the session to succeed instead, because the source is
  re-resolved.
* Name an ineligible profile: expect `profile … is not eligible for sub-agent use`, returned
  synchronously to the tool call.
* Name an unsupported model selector: expect the selector error as the child's startup
  error.

### Part C: volume, per target kind

Per target kind, run N sequential and M concurrent spawns with a trivial instruction (for
example "print the current directory and stop"), on the cheap `deepseek` profile:

* `local-bare` on this machine: N = 20 sequential, M = 4 concurrent, twice; once with the
  managed harness cache warm and once cold.
* local Podman: N = 10, M = 4.
* `ssh-podman` on `morannon`: N = 5, M = 2. `morannon` is a real host with known kernel
  limits (#1019), so do not run heavy load against it.

Acceptance: zero failures with no reason. A failure that names a host capacity limit is an
acceptable outcome on `morannon` and counts as a pass for this plan, because the reason is
specific and actionable; a timeout with no reason is a failure.

Record, per spawn, the time from the tool call to the child's first prompt being accepted,
and the time each startup step took, read from `worker-startup.json`. The step timings are
the thing to watch on a regression: they say which step got slower.

### About the harness cache in the cold-cache run

The managed harness cache is **shared across instances**, not instance-scoped: `cache_root`
in `mj-worker/src/worker_runtime/harness.rs:87` derives it from `XDG_CACHE_HOME` or
`$HOME/.cache`, which on a `local-bare` target is the maintainer's own home directory. Do
not move it aside to force a cold run, because that would affect every other instance on the
machine. Instead, force a cold resolution by pointing the child's profile at a bridge
version that is not yet cached, or accept a warm cache and say so. This plan's live runs
have not moved any cache.

## Idempotence and Recovery

Every step here is repeatable. The instance `plan1065` is disposable; deleting its config
and data directories returns the machine to its previous state. The `/tmp/slowrepo` fixture
is disposable. The worker probe in Part A writes only into its own worker root.

The one irreversible thing to avoid: do not run `git add -A` inside
`/home/jonathan/Projects/brokkbench` to reproduce the timing. It would write hundreds of
thousands of objects into that repository's 43 GB object store. The measurements in this
plan used `git hash-object --stdin-paths` without `-w`, which writes nothing.

No protocol change and no database migration. The startup record and the exit record are
files in the worker root that the controller reads over the existing command channel, not
relay messages, and the refusal travels on the exit record rather than as a new message.
`WorkerLaunchConfig` gains `review_capture`, which is a file the controller writes and the
worker reads, not the daemon protocol; because the worker deserializes it with an implicit
deny-unknown, a stale worker binary rejects the new field, so rebuild the musl worker and
restart the daemon before testing container sessions, as
`.agents/docs` and the repository's own experience with `seed_image_environment` record.

Milestone 3a stops writing `refs/hel/review-baseline` and `refs/hel/review-capture` into user
repositories, but it does not remove refs that earlier releases already wrote. They are inert
once nothing reads them; an existing ref only keeps some objects reachable. Leaving them is
safe, and question 7 below asks whether to clean them up.

## Artifacts and Notes

Reproduction of #1065, run on 2026-09-17 against `e3e1d1fc` with `target/debug/mj-worker`,
using the blocking-repository fixture from Part A. After 35 seconds the worker root holds
nothing but the launch config and an empty log, exactly as the issue reports:

    $ ls -la .../scratchpad/wroot
    -rw-r--r-- 1 jonathan jonathan 339 Sep 17 18:52 launch.json
    -rw-r--r-- 1 jonathan jonathan   0 Sep 17 18:52 worker.log

The worker is alive and blocked in the Git capture:

    $ pgrep -af "mj-worker worker run"
    2728521 ./target/debug/mj-worker worker run --root .../wroot --config .../launch.json --login-environment-ready
    $ pgrep -af "git add"
    2728698 git add -A -- .

Evidence that the reported workspace is the cause rather than a coincidence:

    $ cd /home/jonathan/Projects/brokkbench
    $ git rev-parse --verify --quiet 'refs/hel/review-baseline^{tree}'   # exit 1: never pinned
    $ git ls-files --others --exclude-standard | wc -l
    424715
    $ git ls-files | wc -l
    566005
    $ du -sh .
    524G

    $ cd /home/jonathan/Projects/assay
    $ git rev-parse --verify --quiet 'refs/hel/review-baseline^{tree}'
    f93e3a1a1525fb5b91020da86e44810c87a2d7bc                              # pinned, so fast
    $ git ls-files --others --exclude-standard | wc -l
    19

Damage already done to that repository by abandoned captures. The capture writes objects into
the user's own store, and a capture killed partway leaves them behind unreachable:

    $ cd /home/jonathan/Projects/brokkbench && git count-objects -v
    warning: garbage found: .git/objects/7d/tmp_obj_jhwCL5
    count: 5264
    size: 958592          # KiB, so about 936 MiB of loose objects
    in-pack: 3043145
    garbage: 1

    $ ls -la .git/hel-review-index-*
    -rw-r--r-- 1 jonathan jonathan 69939358 Sep 16 19:35 .git/hel-review-index-gyX0Gx
    $ ls -la .git/objects/7d/tmp_obj_jhwCL5
    -r--r--r-- 1 jonathan jonathan  6750208 Sep 16 19:35 .git/objects/7d/tmp_obj_jhwCL5

    # objects written on the day of the incident, all unreachable: refs/hel is empty
    $ find .git/objects -type f -path '*/??/*' -newermt '2026-09-16 00:00' \
        ! -newermt '2026-09-17 00:00' -printf '%s\n' | awk '{s+=$1} END {print NR, s}'
    2331 254574868          # 243 MiB

So one abandoned capture on 2026-09-16 left 243 MiB of unreachable loose objects, a 70 MB
scratch index file, and a 6.75 MB abandoned temporary object inside the user's repository.
`refs/hel/review-baseline` and `refs/hel/review-capture` do not exist there, which confirms
the capture never completed and those objects will never be used.

Controlled reproduction of the object growth, at small scale, on `e3e1d1fc`. A scratch
repository with 3,000 untracked files, one session start with the real worker:

    $ git count-objects -v        # before
    count: 3
    size: 12
    $ ./target/debug/mj-worker worker run --root …/wroot2 --config …/launch.json
    $ git count-objects -v        # after
    count: 3005
    size: 12092
    $ git for-each-ref refs/hel
    e399ce1d… tree refs/hel/review-baseline
    e399ce1d… tree refs/hel/review-capture

3,002 objects and about 11.8 MiB written into a repository the user did not ask Mjolnir to
write to, and two refs pinning them against garbage collection. Scaled to the reported
workspace that is 424,715 objects.

Throughput measurement in that workspace, read-only (no objects written):

    $ git hash-object --stdin-paths < untracked-list.txt   # 30 second bound
    15085 hashes

424,715 untracked files at that rate is about 14 minutes of hashing alone, before the
object writes that `git add -A` also performs. The 30-second relay window is not close.

What this planning run did **not** do, and why: it did not run 20 sequential and 4
concurrent live spawns per target kind, and it ran nothing against `morannon`. The cause was
established deterministically and reproduced without any model API calls, so a volume run
would have cost model spend and remote-host load without changing the finding. The volume
run belongs to acceptance, after the fixes land, and is specified in Part C above.

## Interfaces and Dependencies

In `mj-worker/src/main.rs`, add:

    /// Record which startup step this worker is on, in `worker-startup.json`
    /// inside the worker root, so a controller can tell a stalled worker from
    /// a dead one before the control socket exists.
    fn record_startup_step(root: &Path, step: &str);

In `mj-controller/src/controller/worker_binary/process.rs`, add:

    pub(in crate::controller) struct WorkerStartupProgress {
        pub alive: bool,
        pub step: Option<String>,
        pub step_recorded_at_ms: Option<i64>,
    }

    pub(in crate::controller) fn worker_startup_progress(
        executor: &impl CommandExecutor,
        locator: &targets::TargetLocator,
        worker_root: &str,
    ) -> Option<WorkerStartupProgress>;

In `mj-controller/src/controller/readiness.rs`, extend the existing private trait so the
fake worker in the tests can drive the new decisions:

    trait StartingWorkerProbe {
        type Relay;
        async fn connect(&mut self) -> Result<Self::Relay>;
        fn death_report(&self) -> Option<String>;
        fn startup_progress(&self) -> Option<WorkerStartupProgress>;
    }

and replace the single `WORKER_STARTUP_CONNECT_TIMEOUT` with three constants:
`WORKER_STARTUP_CONNECT_TIMEOUT` (30 seconds, the initial deadline),
`WORKER_STARTUP_PROGRESS_GRACE` (60 seconds), and `WORKER_STARTUP_CONNECT_CEILING` (300
seconds).

In `mj-checkpoint/src/archive/git.rs`, add the object-redirection pair and thread it through
every command that writes or reads a captured tree:

    /// Where capture puts the objects it creates, and where it may read from.
    /// Mjolnir never writes into the user's repository; `store` is a directory
    /// in the worker root and `alternates` is the repository's own objects.
    pub struct CaptureObjects {
        pub store: PathBuf,
        pub alternates: PathBuf,
    }

    impl CaptureObjects {
        /// GIT_OBJECT_DIRECTORY and GIT_ALTERNATE_OBJECT_DIRECTORIES.
        pub fn env(&self) -> Vec<(OsString, OsString)>;
    }

    pub fn capture_paths(
        runner: &dyn GitCommandRunner,
        repository: &Path,
        objects: &CaptureObjects,
        base: CaptureBase,       // HEAD, or an existing tree id
        paths: &[PathBuf],       // explicit pathspec; never `.`
    ) -> Result<String>;

`capture_worktree_tree` goes away, and with it `pin_review_tree`, `REVIEW_CAPTURE_REF` and
`REVIEW_BASELINE_REF`. `diff_between_trees`, `empty_tree_id` and `session_diff` all take a
`&CaptureObjects`.

In `mj-worker/src/review/capture.rs`, change:

    /// What a repository looked like when the session started: the tree of
    /// HEAD plus the files that were already dirty, and a record of which
    /// untracked paths already existed so a later review can tell a file the
    /// turn created from one it modified.
    pub struct RepositoryBaseline {
        pub tree: String,
        pub untracked_at_start: UntrackedAtStart,  // Recorded(Vec<UntrackedEntry>) | Unbounded
    }

    pub fn initialize_review_baselines(
        git: &dyn GitCommandRunner,
        objects: &CaptureObjects,
        repositories: &[PathBuf],
        timeout: Duration,
    ) -> Vec<(PathBuf, Result<RepositoryBaseline>)>;

returning per repository, so one repository's failure or timeout cannot cost the others their
baseline. The results are persisted as `<worker root>/review-baselines.json` with
`mj_core::config::atomic_write`.

No new crate is needed anywhere in this plan; use `mj_core::targets::BoundedProcessExecutor`
for the bounds and `mj_core::config::atomic_write` for both JSON records.

`RepoDelta` needs no new field: with Milestone 3e refusing a workspace review cannot cover,
there is no partial-coverage state to report, so the relay protocol is unchanged by this
plan.

## Decisions the maintainer has made

These were open questions in c6d6917e and are now settled. They are kept here with their
reasoning so the next reader does not reopen them.

1. **Capture only when review is configured.** A session start does Git work only when
   `config.review.reviewer_profile().is_some()`, and never for a sub-agent child. With
   review unconfigured there is no capture and no stat-walk at all.

2. **A workspace review cannot cover is a refusal, not a degraded mode.** More than 50,000
   untracked paths refuses the session with a 409 naming the count and the limit, and telling
   the user to clean the tree or clear `[review] profile`. The stat-walk that counts is
   itself bounded, and its timeout is its own refusal.

3. **The untracked-file before-content loss is accepted.** An untracked file that existed at
   session start and was modified during the turn shows in review as a whole-file addition,
   labelled as pre-existing from the `untracked_at_start` record. Hashing those files at
   start is exactly the cost being removed; hashing them lazily on first modification needs
   a filesystem watcher and is a much larger change for a narrow gain.

4. **`mj doctor` reports, and deletes nothing** (Milestone 8). There is 243 MiB of
   unreachable loose objects, a 70 MB stray `hel-review-index-*` file and an abandoned
   temporary object in `brokkbench` right now, and every repository a session has ever run in
   has two `refs/hel/*` refs. Deleting refs and running `gc` in someone's repository without
   asking is the same class of mistake as writing to it without asking.

5. **A failed spawn is retried once**, and only when the worker provably never bound its
   socket (Milestone 5).

6. **An errored child is not re-promptable**; the parent spawns a fresh child with a new
   `request_key` (Milestone 5).

7. **Worker sources are re-resolved per session** rather than failing daemon startup
   (Milestone 7), because a daemon with no portable worker can still run Grok sessions.

8. **#1078 item 1 only** (Milestone 6). The CLI and daemon protocol mismatch belongs
   with #1039.

9. **`WORKER_STARTUP_CONNECT_TIMEOUT` is not simply raised.** The blocking step was
   proportional to the user's workspace, so no constant was right; Milestone 2 replaces the
   constant with a progress-aware wait.

---

Revision note (2026-09-18, third revision, after maintainer decisions on c6d6917e): the
capture is now gated on review being configured at all, so an unreviewed session and every
sub-agent child do no Git work at session start, not even a stat-walk; the predicate is
`config.review.reviewer_profile().is_some()` and a new `review_capture` field on
`WorkerLaunchConfig`. Partial coverage is gone: a reviewed session whose workspace has more
than 50,000 untracked paths is refused with a 409 carrying the count and the limit, using the
`Refusal` mechanism from #1057, and the walk that counts is bounded with its own refusal on
timeout. Milestone 3d is dropped, because a child now does no baseline work and must not read
a parent's. The `mj doctor` report became Milestone 8. The open questions became a settled
decisions list. `RepoDelta` no longer needs an `untracked_coverage` field, so the plan makes
no protocol change. The commit order for implementation is: diagnostics (1, 2), the Git
change (3a, 3b, 3c), the refusal (3e), the retry and actor retirement (5, 6), worker source
re-resolution (7), and the doctor report (8); Milestone 4 lands with the diagnostics because
it is what makes them reach the parent model.

Revision note (2026-09-18, second revision, after maintainer review of e5b11913): Milestone 3
was replaced. The first version moved the review-baseline capture off the startup critical
path, ran it concurrently and bounded it at 120 seconds. The maintainer rejected that,
because it leaves a session start that runs `git add -A -- .` over the user's working tree,
which is the defect itself and not merely its timing. Investigating the cost showed the
capture also writes a Git object per untracked file into the user's own repository and pins
them with two refs, and that one abandoned capture had already left 243 MiB of unreachable
objects and a 70 MB stray index file in a real repository. Milestone 3 is now three commits:
redirect capture's objects into a worker-owned store with the repository as a read-only
alternate and drop the two refs; make the startup baseline cost one stat-walk plus the
already-dirty tracked files; and make the review-time capture cost only the turn's changed
paths. The sections `What the review baseline is for` and `Other places that walk or stage
the working tree` are new, acceptance Part A gained a half-million-file object-count test,
and the artifacts gained the measured object-store damage and a controlled reproduction of
it. Milestones 1, 2, 4, 5, 6 and 7 are unchanged.

Revision note (2026-09-18): first version of this plan. Written after mapping the spawn path,
reproducing the #1065 signature with the real worker binary against a repository whose
`git add` blocks, and confirming from the filesystem that the workspace named in the report
has never completed a review-baseline pin. The reason the plan leads with the review-baseline
ordering rather than with the 30-second timeout is that the timeout is a symptom: the step it
waits on is unbounded and proportional to the user's workspace.
