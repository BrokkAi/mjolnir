# Own raw worker processes through teardown

This ExecPlan follows `.agents/PLANS.md`. Keep its progress, discoveries, decisions and outcomes current. It continues the same user request as `.agents/plans/tui-exit-after-daemon-acceptance.md`; issue #1510 is included in that work, not deferred to another task. Installation is excluded.

## Purpose / Big Picture

Stopping a raw session must stop the worker and MJ-managed harness, terminal and user-shell groups before deleting their working files. A raw session runs directly on a local or SSH host rather than inside a container. Process IDs observed in a process listing are not ownership: an ID can be reused, and a child can detach before the next listing. The latency checkpoint deliberately removed an unsafe attempted tree fence.

Give each new worker one independent process owner. It launches the worker and MJ-managed groups, retains their real child handles, and answers stop requests even when the worker is hung. It passes existing input/output pipe descriptors to the worker rather than copying workload traffic through a new proxy. A successful stop requires completed cleanup, not merely delivery of SIGKILL or exit of the group leader. Unknown cleanup retains the affected root and publishes the failure. Containers retain engine-owned teardown. Existing workers use explicitly legacy stop semantics; they cannot acquire missing parent ownership retroactively.

The guarantee covers MJ-created groups and descendants remaining in those groups. Existing bounded bridge cleanup gives native harnesses an opportunity to stop their internal tools. It does not establish containment of arbitrary host processes that deliberately create independent sessions. Test supported harness behavior where possible and report any concrete remaining escape at its actual launch boundary.

## Progress

- [x] Established that ancestry snapshots cannot fix PID reuse or fork/detach races.
- [x] Mapped actual bridge, terminal and user-shell launch owners and the existing worker root lock.
- [x] Chose a single per-worker owner, descriptor handoff and non-consuming child-exit observation.
- [x] Identified explicit lock-delegation and generation-completion requirements before implementation.
- [x] Implemented the identity-preserving owned-group primitive. Three real Linux process tests passed, and independent source review found no concrete blocker. Native macOS validation remains part of integration acceptance.
- [ ] Implement the private owner protocol, descriptor handoff and serialized launch/stop registry (draft sources complete and unwired; compilation and tests pending).
- [ ] Implement bootstrap delegation, generation metadata and controller stop integration.
- [ ] Adapt bridge, terminal and user-shell callers without changing ACP or output-buffer behavior.
- [ ] Validate actual hung-worker cleanup, independent process groups, partial failures, streams and isolated upgrades; run final required checks and review.
- [ ] Commit and push reviewed changes on the current branch. Do not install.

## Surprises & Discoveries

The existing bridge supervisor is a separate process and owns the bridge's actual child handle. Killing that supervisor with the worker bypasses its bounded cleanup. Terminal and user-shell groups are owned inside the worker, so a worker-only stop request cannot handle a hung or dead worker.

Tokio 1.52.3 does not automatically reap every owned child, but polling `Child::wait`, `try_wait`, or dropping its wrapper can consume status or transfer the child to an orphan reaper. The new owner must retain a private standard-library Child and never expose a wait-capable handle to callers. `waitid` with WNOWAIT observes exit while leaving the child unreaped; Linux and macOS support it. The group leader's unreaped identity prevents its numeric group ID from being reused while signalling remains possible.

An unreaped zombie may keep a process group observable. Do not wait for group absence while retaining that zombie. Final group signalling must happen before permanent disarming and reaping; absence queries happen afterward and never authorize another signal. A recycled group ID can cause conservative failure, but cannot authorize killing unrelated processes.

`FileLock::drop` explicitly unlocks its shared open-file description. Wrapping inherited duplicates in two FileLock objects would let worker exit unlock the owner's exclusion. Keep the sole unlocking FileLock in the owner. The worker's delegated duplicate is a plain File/OwnedFd that only closes on drop. Its lifetime must survive login-shell reexecution and must not leak into workloads.

Best-effort worker-startup.json cannot serve as ownership metadata: startup preparation clears and reconstructs it. A separate required generation record must survive owner or worker failure. A free lock or missing socket does not establish that separately grouped children stopped.

## Decision Log

Decision: Use one independent process owner rather than an in-worker RPC or per-workload supervisor inventory. Rationale: the owner survives worker failure, owns all four direct launch kinds, and makes one serialized launch-versus-stop decision. This is a session process using the existing worker binary, not an OS service, new workspace crate or general command broker.

Decision: Preserve byte streams through Unix descriptor passing. Rationale: bridge ACP, terminal output and shell buffers already stream correctly. Only launch, exit observation and stop authority move; output is not journaled or proxied through the owner.

Decision: Retain actual child identity until the final group signal. Rationale: ancestry/birth snapshots and numeric PGID inventories cannot establish safe ownership. A final signal is followed by permanent disarming, exactly one reap and query-only completion checks. Retry after reap never sends another signal.

Decision: Keep the existing worker.lock with one unlocking owner and an explicitly delegated worker role. Rationale: a separate owner lock would not exclude old launchers, while a second FileLock wrapper can unlock the first owner. Preserve the existing owned-reexec role for outer bootstrap and root fixtures. Only the plain duplicate lock descriptor crosses delegated login reexecution; the existing generation-bound owner socket/Hello provides control handshake without a second inherited control channel.

Decision: Route new `worker run` invocations without a delegated role through the same owner bootstrap, including direct existing launch commands. Actual runtime wrappers receive an explicit ProcessOwnerClient, and direct tests supply their real owner fixture. Rationale: legacy means already-running old worker binaries, not an optional unmanaged execution path inside the new worker. No new-runtime local-spawn fallback is allowed on owner failure.

Decision: Persist only a generation record and terminal cleanup fact, not a generic task journal. Rationale: a lost stop acknowledgement must be recoverable after owner exit, while incomplete ownership must never silently become legacy absence. New generation launch may replace only a certified stopped generation under the root lock. New-format incomplete ownership is a root-specific visible failure; it cannot be overwritten because the lock happens to be free.

Decision: Keep explicit legacy semantics and existing upgrade policy. Rationale: running historical workers cannot be adopted portably. Do not force a busy upgrade to destroy a worker, fabricate an owner certificate for old processes, or add unsafe process-discovery fallback when a new owner fails.

## Outcomes & Retrospective

The independent OwnedGroup primitive is implemented. The frozen-source five-package focused run passed three real process tests: an exited leader remains unreaped until group stop, cancellation retains authority and post-reap retry never signals a reused group, and external reaping is an ownership failure. Independent review found no concrete correctness blocker while explicitly retaining the assumptions about sole reaping authority and unknown group completion. The helper is not yet wired into worker launch, so no new end-to-end teardown guarantee has been delivered. The independently reviewable latency checkpoint stops each historical root once in parallel and lets the TUI exit after acceptance; it does not fix this ownership defect by itself. The earlier unsafe census implementation remains removed.

## Context and Orientation

`mj-controller/src/controller/worker_binary/process.rs::start_worker_command` constructs worker launch commands for local, SSH and container targets. `mj-worker/src/main.rs` owns command parsing, initial root locking and login-environment reexecution. WorkerRootOwner is defined under the worker runtime and currently wraps the root lock. `mj-worker/src/worker_runtime/unix.rs` runs the worker with that owner; its runtime implementation only needs the root while the lock guard stays alive.

`mj-worker/src/worker_runtime/unix/supervisor.rs` constructs the actual ACP bridge command from AcpSupervisorSpec. `mj-worker/src/acp.rs` currently owns its supervisor process and stream transport. `mj-worker/src/terminal.rs` constructs terminal shells and retains output buffers; `mj-worker/src/user_shell.rs` constructs user-shell commands and supervises cancellation/output draining. These are the only workload launch seams being moved. Other short subprocess helpers remain unchanged.

Put the identity primitive in `mj-core/src/subprocess/owned_group.rs`, exported conditionally on Unix from subprocess.rs. Put the private owner implementation under `mj-worker/src/process_owner/`, with a small parent module. Keep protocol/client/server implementation boundaries separate so integration can proceed without multiple writers editing the same file. The exact bootstrap delegation types belong beside WorkerRootOwner, not in a new global lock framework.

## Plan of Work

First implement OwnedGroup around a private standard-library Child. Spawn in a fresh process group through the shared subprocess layer; expose detachable standard stdin/stdout/stderr handles, leader ID for diagnostics, non-consuming asynchronous leader-exit observation, TERM request and final stop. Register SIGCHLD notification before checking WNOWAIT|WNOHANG. Handle ECHILD as lost authority, never success. Cancellation of an await must retain the child and any reap task. The existing three one-second post-KILL confirmation attempts may be reused; do not introduce guessed time budgets. After reaping, only query the group. Successful stop returns an opaque GroupStopped; repeats return its cached result. Unexpected Drop attempts a final signal only while identity remains reserved, then transfers reaping to a named cleanup thread and logs failures without issuing a success certificate. Normal cleanup does not create a reaper thread.

Second implement the local private owner protocol. Requests carry a private format version, owner generation and unique request/group token. Keep operations limited to Launch, StopGroup and StopWorker plus the handshake/result messages they require. Launch carries one of the known managed kinds and the existing resolved command/environment/cwd description. Send child-side CLOEXEC descriptors through SCM_RIGHTS over a local Unix connection; worker-side streams remain with the worker. Close duplicate child descriptors in both processes after transfer/spawn. Do not pass descriptors across SSH; owner and worker are on the same target. Preserve paths and argument boundaries without joining commands into shell text unnecessarily.

The owner has one Running to Stopping to Stopped decision. Record an admitted launch before dispatching blocking spawn work, register its actual Child before acknowledging it, and include every pending launch in stop. Reject launches after Stopping before spawn. A lost Launch acknowledgement must not cause automatic replay or a second spawn. Worker disconnect/death triggers owned cleanup; daemon attachment loss does not. Group leader exit is a status event, distinct from group cleanup completion. Registries explicitly end the group lifetime after their existing output drain or release.

Third implement bootstrap and stop integration. The outer owner acquires existing worker.lock before diagnostics or generation replacement and retains its sole unlocking FileLock throughout cleanup. Bind the generation endpoint, persist required incomplete-generation metadata and start accepting before child launch. The worker receives a delegated plain lock descriptor and explicit generation, validates root file identity/record, performs Hello using the existing owner socket protocol before login or diagnostic writes, and restores CLOEXEC before workload launch. Preserve that descriptor and the delegated role through login reexecution. The runtime then receives its explicit asynchronous owner client. Keep the outer owned/reexec behavior distinct; do not globally change FileLock Drop. Retain the delegated guard across the worker runtime's final writes.

Generation metadata records format, generation, endpoint and incomplete or stopped state. Capability advertisement refers to that same generation, and cannot upgrade absence into new ownership. Owner loss leaves incomplete metadata in place. Once launch admission is closed, pending launches resolved, all groups stopped, the worker reaped and all remaining root writers finished, stop the owner's socket/file writers and atomically persist the stopped certificate as its final filesystem write. Response delivery and closing sockets may follow; no new root writes may follow. Retain the sole lock guard through successful process exit rather than dropping it before late runtime/tracing teardown; kernel descriptor closure releases the lock. Lost acknowledgement or owner exit can be resolved from that same generation's terminal certificate.

Controller raw stop checks new-format metadata first. It calls the owner or re-reads the exact terminal certificate after transport loss. It must obtain the root lock and revalidate the same stopped generation before accepting completion; never treat a replaced generation or free lock alone as a cleanup proof. Keep existing managed lifecycle permits through removal, and use a composed target helper where a target-side lock must span stop-to-remove. Standalone stop remains distinct from authority to delete a later replacement. New-owner errors retain that root and report the failed operation; they never select the legacy PS stop path. Only roots without new ownership metadata/capability use the existing legacy path. Reuse current bounded command/control mechanisms; if their contracts do not cover unresponsive owner observation, report the missing bound to the parent before inventing a new timeout.

Fourth adapt the three workload seams. The actual bridge command, terminal shell and user-shell command become managed launches; they receive pipe handles and leader-exit observations instead of mutable Child/PGID ownership. Keep ACP framing, terminal retention, shell output timing, exit status and cancellation behavior. Terminal/UserShell stop requests address their registered group token. Preserve existing bridge EOF and TERM grace under the new owner and always finish the group lifetime after leader exit. All command preparation and optional downloads happen before launch admission. Do not add a general launch service for unrelated subprocesses.

## Concrete Steps

Work in the current checkout and branch. Do not change Cargo storage, branches, the user's installed binaries or live daemon. Use the existing mbx configuration. Every cargo test runs elevated with TZ=UTC, with the consistent five-package graph used for the preceding changes:

    TZ=UTC cargo test -p brokk-mjolnir -p brokk-mj-controller -p brokk-mj-client -p brokk-mj-core -p brokk-mj-worker --features brokk-mjolnir/test-hooks --lib --bins <affected filters>

Actual daemon, CLI and TUI tests require a named isolated instance; retain existing private data/config fixtures. Do not set MJ_INSTANCE globally on unit suites. Run only affected/new/failed tests during implementation rounds. After the final round, run one full suite of newly touched crates and one workspace Clippy run, reusing the preceding accepted suite where its sources and behavior remain applicable. Failed tests are repaired and rerun individually.

    TZ=UTC cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    git diff --check

Commit logically coherent validated changes on the current branch, stage explicit owned paths, and request the configured review before push. Merge moved origin/master as already authorized, never rebase. After platform-sensitive changes land on master, dispatch nightly.yml as required by AGENTS.md. Do not install.

## Validation and Acceptance

Real Linux and macOS process tests must show leader exit before descendants, non-consuming status observation, final signalling before reap, no signal after reap even on retry/cancellation, preserved exit status and an unrelated sentinel surviving. The group must not be declared stopped because the leader exited. Completion-query errors are unknown, not empty.

Root-lock tests must show delegated worker exit cannot unlock the owner during cleanup, owner death before readiness still leaves the delegated worker excluding replacement, login reexecution retains the delegated role, and workloads inherit no delegation descriptor. The runtime uses ordinary CLOEXEC owner sockets. Owner plus worker death leaves incomplete metadata that a later launch refuses to overwrite.

A real isolated worker test launches bridge/terminal/UserShell groups, SIGSTOPs only the owned worker, and stops through the independent owner. All managed groups and the worker must be gone before deletion starts, while an unrelated sentinel remains. Stop racing an admitted launch either rejects before spawn or registers and joins it before acknowledgement. Owner loss, failed cleanup and wrong/replaced generation must retain files and expose a useful failure. A lost stop acknowledgement must recover only the same terminal result, never spawn again or delete a newer generation.

Descriptor streaming tests must send more than64KB in both directions concurrently, preserving EOF, output/status and argument/environment boundaries. Extend existing isolated startup/worker-upgrade/reexec coverage for old and new formats. For pinned harnesses, prefer existing real bridge fixtures and report any unsupported native separately grouped tool cleanup rather than claiming group ownership proves it.

## Idempotence and Recovery

Launch is not arbitrarily replayable after acknowledgement uncertainty. Group/worker stop observes its existing registered identity and terminal result. Once a group is reaped it can never regain signal authority. A stopped certificate is valid only for its exact generation under the same root exclusion. Incomplete ownership survives process failure and cannot silently become legacy. No generic daemon task state, DB migration, host service or numeric PGID inventory is introduced.

Tests own their processes and temporary roots. Cleanup must verify ownership before signalling; retained failure fixtures must explicitly release only their own processes. Do not use user sessions for benchmarks or cleanup tests.

## Artifacts and Notes

The issue is https://github.com/BrokkAi/mjolnir/issues/1510. Design evidence is under `.mj/agents/f2c6d8abae249bbaa96ea6880588d309/`: 1510-minimal-ownership-options.md, 1510-launch-stream-seam.md and 1510-contract-challenges.md. Primitive signatures are in `.mj/agents/1cbd92b1e462d4831611d7744363a0df/1510-owned-group-signatures.md`. Bootstrap findings are in `.mj/agents/3015c9deeefcbc11932935c38d210b42/1510-bootstrap-interface.md`.

The initial primitive tests passed in `.mj/agents/b0df254c0e1ea40035ec39f1841037fe/primitive-phone-focused.log` on frozen relevant sources, with the three phone cancellation filters in the same consistent build graph. The new owner protocol, transport, server and client drafts are not exported from lib.rs yet and were absent from the compiled worker dependency file; their own test bodies are not passing-test claims.

Final checkpoint workspace Clippy passed with zero diagnostics in `clippy-primitive-phone-final.log`, and formatting/diff checks passed in the same validator directory. The sole initial lint was a nested-if layout in Drop, mechanically collapsed after the passing tests; no behavior changed and no tests were repeated for it. Full commands and source-hash evidence are in `primitive-phone-validation.md` and its JSON siblings. These results validate the helper and phone correction, not the unwired owner drafts.

These reports supply source pointers and investigation evidence; the contracts required for implementation are stated above. Extend this section with exact validation logs and final public behavior as work completes.

## Interfaces and Dependencies

OwnedGroup owns a private Child and provides `spawn`, `take_stdio`, `leader_id`, `request_terminate`, asynchronous `leader_exit`, and asynchronous `stop`. GroupStopped is opaque except for its leader status accessor. It is the sole source of managed group cleanup completion. Callers supply existing TERM grace policy; the primitive has no arbitrary signal API.

The private owner client returns a ManagedProcess containing group token, standard pipe handles adapted to the existing async readers/writer, and non-consuming leader-exit status observation. It never gives callers a mutable OS Child or killable PGID. Owner generation binds launch, status and stop. WorkerRootDelegation owns only plain inherited descriptors and cannot unlock the root; outer WorkerRootOwner remains sole unlocking authority. Existing relay protocol and database schema need not change merely to add this local private interface; verify rollout and worker compatibility through the existing build-identity mechanism.

Revision note: Created after the user challenged deferring raw ownership. This plan replaces the rejected ancestry-fence proposal with actual process ownership, preserves the validated latency checkpoint, and records no-install, identity, completion and root-lock boundaries before implementation.
