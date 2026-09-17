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
- [ ] Milestone 1: worker startup breadcrumbs, so a worker can never die or stall silently.
- [ ] Milestone 2: readiness wait that watches the process and the breadcrumbs.
- [ ] Milestone 3: take the review-baseline capture off the pre-socket critical path.
- [ ] Milestone 4: carry the real reason back to the parent model.
- [ ] Milestone 5: bounded automatic retry, and a child the parent can act on.
- [ ] Milestone 6: retire the relay actor of a session that reached a terminal state.
- [ ] Milestone 7: re-resolve the worker source per session instead of only at daemon start.
- [ ] Live acceptance run on `local-bare`, local Podman, and `morannon` ssh-podman.

## Surprises & Discoveries

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

- Decision: keep the review baseline, but stop letting it gate the control socket.
  Rationale: the baseline exists so a later review can tell this turn's work from what was
  already in the checkout. The invariant it needs is "pinned before the harness can edit
  anything", not "pinned before the controller can connect". The harness starts strictly
  later than the socket bind, so the capture can run concurrently with the rest of startup
  and be awaited at the point where the first prompt is dispatched.
  Date/Author: 2026-09-17, plan author.

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

Not started. Fill in at the end of each milestone: what was achieved, what remains, and
whether the live acceptance run in `Validation and Acceptance` passed on all three target
kinds.

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

## Failure inventory

Each entry is marked **confirmed** (evidence on current master), **likely** (strong
indirect evidence), or **possible** (mechanism exists, not observed).

1. **Unbounded review-baseline capture blocks the control socket. Confirmed.** The cause of
   #1065. `initialize_review_baselines` runs `git add -A -- .` against the session's working
   tree before `control.sock` is bound. In the workspace named in the report,
   `/home/jonathan/Projects/brokkbench`, `git ls-files --others --exclude-standard` reports
   424,715 untracked files and `git ls-files` reports 566,005 tracked files, in a 524 GB
   working tree with a 43 GB `.git`. Measured hashing throughput there is 15,085 files in 30
   seconds with `git hash-object --stdin-paths` (which does not even write objects), so the
   capture needs at least fourteen minutes. The repository has no `refs/hel/review-baseline`
   ref at all, meaning the pin has never once completed there; the sibling child that
   succeeded ran in `/home/jonathan/Projects/assay`, which has 19 untracked files and does
   have the ref. Reproduced directly: see `Artifacts and Notes`.

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

### Milestone 3: take the review baseline off the pre-socket critical path

Goal: nothing that scans the user's workspace can delay the control socket.

In `mj-worker/src/worker_runtime/unix.rs`, `run_daemon`: move the
`initialize_review_baselines` block from before `DurableRelay::open` to after the socket is
bound, and start it as a `tokio::task::spawn_blocking` handle that is *not* awaited there.
Await it at the single point where the first harness work can begin: immediately before the
ACP supervisor is started, which is after `harness::resolve`. The harness cannot touch the
workspace before that point, so the invariant the baseline needs is preserved.

Bound the capture. Add a constant `REVIEW_BASELINE_CAPTURE_TIMEOUT` of 120 seconds in
`mj-worker/src/review/capture.rs` and pass it down so the Git commands run under
`mj_core::targets::BoundedProcessExecutor`, the same helper `login_environment::discover`
already uses. On timeout, do not fail the session: record the failure in the relay as a
session notice with the text
`review baseline unavailable for <path>: the working tree is too large to capture in <n>s;
turn review will report no baseline for this repository`, and continue. A session that runs
is worth more than a review baseline, and today's behaviour trades the session for the
baseline.

Change `initialize_review_baselines` to report per-repository outcomes rather than failing
the whole call on the first error, so one enormous repository among several does not cost
the baselines of the others.

Tests: a behaviour test in `mj-worker/src/review/capture.rs` using a temporary repository
with a `clean` filter configured to `sleep`, which is exactly the fixture this plan used to
reproduce #1065 (see `Artifacts and Notes`). Assert that `initialize_review_baselines`
returns within the bound and reports the repository as not captured, rather than blocking.
Add a relay test asserting the worker binds its socket while that capture is still running:
point the launch config's `cwd` at the blocking repository and assert `control.sock` exists
within a few seconds.

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
Milestone 3: `worker.pid` and `control.sock` exist within a few seconds, while the baseline
capture is still blocked, and `worker-startup.json` reports `serving`.

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

If a milestone changes the daemon protocol, bump `PROTOCOL_VERSION` in
`mj-client/src/daemon.rs` by one and say so in the commit message. As specified, none of the
seven milestones changes the protocol: the startup record is a file in the worker root that
the controller reads over the existing command channel, not a relay message. There is no
database migration.

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

In `mj-worker/src/review/capture.rs`, change:

    pub fn initialize_review_baselines(
        git: &dyn GitCommandRunner,
        repositories: &[PathBuf],
        timeout: Duration,
    ) -> Vec<(PathBuf, Result<()>)>;

so one repository's failure or timeout cannot cost the others their baseline, and so the
caller decides what a failure means. No new crate is needed anywhere in this plan; use
`mj_core::targets::BoundedProcessExecutor` for the bound and
`mj_core::config::atomic_write` for the startup record.

## Decisions that belong to the maintainer

1. **When a workspace is too large to capture, what should happen?** Recommendation: bound
   the capture at 120 seconds, continue the session, and record a notice that turn review
   has no baseline for that repository. The alternative, failing the session, is today's
   behaviour and is what #1065 is. A third option is to skip capture entirely above a file
   count threshold, which is cheaper to detect but adds a number that will be wrong for
   someone.

2. **Should a failed spawn be retried automatically?** Recommendation: yes, exactly once,
   and only when the worker provably never bound its socket. The alternative is to return
   the reason and let the parent model decide, which is simpler and avoids doubling the cost
   of a systematic failure, but it makes every parent prompt responsible for retry logic.

3. **Should an errored child be re-promptable under the same id?** Recommendation: no. A
   fresh `spawn` with a new `request_key` is cleaner and the idempotency key already makes
   that safe. Re-using the id would need a provisioning path that can start over on a record
   that is not `Provisioning`.

4. **#1068: fail daemon startup when no worker source resolves, or re-resolve per session?**
   Recommendation: re-resolve per session, and copy the resolved worker into the daemon's
   own state directory so it cannot be reaped again. Failing daemon startup is the stricter
   reading of the issue but it would stop a daemon that can still run Grok sessions, which
   do not use the portable worker.

5. **Should `WORKER_STARTUP_CONNECT_TIMEOUT` simply be raised?** Recommendation: no. It is
   the cheapest change and it would have masked #1065 for a while, but the blocking step is
   proportional to the user's workspace, so no constant is right. Raising it also makes
   every genuinely dead worker take longer to report. The progress-aware wait of Milestone 2
   gives both a fast failure and an unlimited-in-practice patience for real progress.

6. **How much of #1078 to take here?** Recommendation: take item 1 (Milestone 6), because a
   permanent reconnect loop for a failed child is part of "spawn working completely". Leave
   item 2 (CLI/daemon protocol mismatch) out; it has a clear message already and belongs
   with #1039.

---

Revision note (2026-09-18): first version of this plan. Written after mapping the spawn path,
reproducing the #1065 signature with the real worker binary against a repository whose
`git add` blocks, and confirming from the filesystem that the workspace named in the report
has never completed a review-baseline pin. The reason the plan leads with the review-baseline
ordering rather than with the 30-second timeout is that the timeout is a symptom: the step it
waits on is unbounded and proportional to the user's workspace.
