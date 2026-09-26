# Collaboration

Treat requests to build or fix something as instructions to complete the work.
Use the conversation to infer scope, resolve routine choices, and continue
through implementation, validation, and the required commit. State material
assumptions. Ask for clarification when the answer changes the intended result
and cannot be inferred; continue independent authorized work while waiting.

Authorization persists across turns. Before requesting new approval, complete
the authorized preparation so the user can review a concrete result. Follow
the explicit Git and release rules below when deciding what is authorized.

User instructions take precedence over skill guidelines. If a skill causes a
pause, permission request, or unfinished work, link the exact `SKILL.md`, quote
the relevant instruction, and explain how it applies. Distinguish an explicit
requirement from your interpretation; do not invent approval requirements.

Lead updates and final responses with the result or finding. Use concise,
connected prose and plain language; use lists when they improve clarity.
Explain what changed, why, how it was validated, and any remaining limitation.
Keep messages between agents equally clear and readable.

# Coordination

Multiple people and their agents work on this project concurrently. Self-assign and add the `agent-in-progress`
label to any issue you begin working on to avoid overlapping work. Remove the
label if you stand down without resolving the issue. Avoid working on
tasks assigned to other people unless explicitly directed to do so.

# ExecPlans

Use an ExecPlan for a complex feature or a significant refactor. Follow `.agents/PLANS.md` from design through implementation.

Use `.agents/` as the only repository namespace for planning and design artifacts that agents own. Do not create `.agent/`.

Store each ExecPlan in `.agents/plans/`.

Keep `.agents/PLANS.md` as the standard for ExecPlans. Do not store individual ExecPlans next to `.agents/PLANS.md`.

Store design notes for LLMs or agents in `.agents/docs/`. These notes can include agent context, publication runbooks, parity notes, and similar internal information. Do not publish these notes as product documentation.

Reserve `docs/` for future documentation for human readers. Do not store ExecPlans, agent runbooks, or LLM-only context in `docs/`.

# Releases

"Cut a new release" and equivalent release requests explicitly authorize
pushing the release commit to the configured upstream, pushing the release
tag, and publishing remotely through the normal release workflows, including
required package-channel updates. Treat this as an explicit push request;
do not ask for separate push or publication confirmation.

Follow `RELEASING.md` for every release. Tag the selected known-good commit;
it need not be current master or merged into master. Reuse passing validation
for that exact commit instead of rerunning it for the release. The committed
workspace version, internal dependency constraints, and `Cargo.lock` must match
the tag; the release workflow checks version consistency and generates license
reports from that tagged commit instead of committing them.
Do not add a new pre-tag checklist or repeat registry authorization audits.

# Repository Guidelines

# Git / version control

After completing and validating requested implementation changes, commit them
to the current branch without waiting for a separate request to commit. For a
larger task, also commit each logically distinct, validated set of changes when
it forms a coherent checkpoint. Treat committing as part of finishing the task
unless the user explicitly says not to commit. Do not push unless the user
explicitly asks. Release requests are explicit push authorization as defined
under Releases above.

Commit directly to the current branch. This rule also applies when the current branch is `master`.
When explicitly asked to push, push to upstream, even when upstream is `master`.

Do not create a branch, change branches, rebase, or open a pull request unless the user gives an explicit instruction.

Do not run `git checkout -b`.

The instruction "commit" means that you must commit on the current branch. It does not mean that you must create a branch first. This rule overrides other default branch procedures.

Stage and commit only the files that you changed. Do not run `git add -A`. Do not include unrelated working-tree changes in the commit.

## Engineering Guidance

Continue when there is a clear next step toward the requested goal. Do not stop
for unnecessary approval.

The TUI and web control surfaces must never perform blocking I/O or long-running
work on their event/render loops. Run filesystem scans, network calls, process
execution, provisioning, checkpointing, imports, and similar work in supervised
background tasks. Independent operations must be able to run concurrently.
Represent in-flight work immediately in UI state, make it cancellable where the
underlying operation permits rollback, and report background failures instead
of dropping them. Quitting a UI must remain responsive while cleanup is bounded.

Prefer behavior tests that prove the advertised interface. Do not add tests that
only duplicate implementation lists or internal construction order.

Do not create a new workspace crate only to reorganize code. Create one only
when a clear dependency, compilation, publication, or ownership boundary
requires it.

Build for correctness and general use.

Concurrency: make races structurally impossible, the way Cliff Click would,
instead of patching each one as it appears. One owner decides each fact; every
reader asks that owner rather than recomputing the fact from its own copy of
the inputs. Two predicates for the same question ("is this session busy") are a
bug even while they agree. Prefer a single state machine with explicit
transitions over independent flags that must be kept consistent, and a single
serialized decision point over checks that run on different snapshots. Acquire
resources after the decision that needs them, release them on every exit path,
and make every retry loop carry a backoff or a state change that ends it.
When you find a race, fix the ownership so the race cannot be expressed, and say
so in the commit body; a patch that narrows the window is not a fix.

A narrow fallback usually indicates a design problem. Find the source of the
problem and correct the root cause, even when the correction affects a larger
area. Report the failure; do not paper over it with a second path that hides
the primary design not working. This includes options in third-party tools
that quietly degrade, such as OpenSSH's `ControlMaster=auto` opening a direct
connection when sharing fails: choose the configuration that fails visibly.

Upgrades must complete without user intervention after installation or initial
upgrade consent. Ordinary startup must coordinate daemon replacement, database
migration, and service readiness before serving the upgraded client, including
when the wire protocol is unchanged. Retain forward migrations for every shipped
database revision; never require an intermediate release, manual daemon restart,
or data reset. Preserve active workers and terminal drafts across handoff, and
never automatically downgrade a newer daemon or incompatible store. Changes to
startup, schema, protocol, or release installation must preserve the isolated
upgrade regressions. Keep terminal upgrade handoff formats backward compatible.

## Control plane and data plane

The daemon (`mj daemon-run`, mj-controller) is the control plane. It owns the
database, session records, lifecycle operations (provision, checkpoint, close,
destroy, recovery), the HTTP API, review orchestration, and client attachments.
It holds no agent state that cannot be rebuilt from the database and from
workers.

Workers (mj-worker, one per session, local or remote) are the data plane. They
run the harness process and the agent's turn, own the relay journal and
checkpoint barriers, and keep running when no daemon is attached. A worker's
turn, its pending questions, and its review sessions continue across a daemon
restart; the next daemon reattaches and replays from the journal.

A consequence for design: any daemon-side task must be either short and bounded,
finishing with the command or response that started it, or resumable from durable
state at daemon startup. Never make daemon liveness or daemon replacement depend
on worker state. Never store in daemon memory anything a worker or the database
cannot give back.

A consequence for upgrades: replacing the daemon waits only for its own bounded
control operations; replacing a worker waits for that worker to be idle, because
the worker holds the turn.

Automatic upgrades must never cancel accepted work that cannot start again from
durable state, or use a timeout as permission to stop a busy process. Daemon
handoff waits only for daemon-owned work: lifecycle operations, startup and
session recovery, admissions, automatic continuations, and in-flight request and
response delivery, including after the originating client disconnects. It never
waits for worker turns, pending questions, or reviews; those live in workers and
survive the handoff. The wait must take seconds, not minutes. Work that can take
minutes but is safe to stop and starts again under the next daemon does not hold
admission: recovery copies, worker upgrade preparation, and sub-agent waits. The
handoff cancels it. Only the worker swap itself, from the idle reservation to
the reconnect, holds admission. While a handoff waits, the gate drains:
deferrable work (`activity_unless_draining`) is refused so the wait can only
shrink. Give each admission site its own label. New daemon-owned background
operations must participate in this ownership. Close admission and verify no
outstanding work in one decision, and name the blocking work in the wait notice
so the user can see what the upgrade is waiting for. Worker replacement is
separate and still requires atomic idle admission: reserve workers only after
that admission, and prepare downloads before taking their control connection.
Retry only explicitly unaccepted requests, preserving command IDs and steering
targets; a lost acknowledgement does not authorize replay of an arbitrary
mutation. Test these races in isolated instances. Legacy daemons without atomic
admission can only provide an observed idle check; never describe that bootstrap
as having the new guarantee.

Keep file and path handling independent of the operating system. Use `Path` and
`PathBuf`; normalize path text only at protocol or rendering boundaries.

Do not silently discard errors from spawned threads, tasks, or Rayon work.
Propagate or report failures with useful context.

Before adding a helper that interprets paths, strings, or shared data shapes,
search for an existing helper. Put shared interpretation in one location.

Keep small single-use types and computations near the code that uses them.
Prefer hand-written test fakes over mocking or dependency-injection frameworks.

Do not redirect Cargo or other build output into `/tmp`. If sandbox restrictions
block normal build storage, run the build outside the sandbox.

## Subprocess Rules

Run child processes through the shared subprocess helpers. Do not hand-roll
`std::process` pipe handling at call sites; a clippy `disallowed_methods`
entry enforces this for `wait_with_output`, and an explicit scoped `allow`
with a stated reason is required anywhere raw use really is safe.

Never write a child's full stdin before reading its stdout. Pipes buffer
64KB; a child that produces output while consuming input blocks on its full
stdout pipe, stops reading stdin, and deadlocks both processes. Drain output
concurrently while feeding input (the shared helper does this).

Never delete a process's working files as a substitute for stopping the
process. Teardown must terminate the owning process group first and remove
files second; a surviving writer recreates whatever was deleted under it.

When testing code that streams through pipes or bounded buffers, drive it
with more than 64KB of data so buffer-boundary deadlocks and truncation
actually show up; toy-sized fixtures prove nothing about this class of bug.

Unit tests are colocated in module-level `#[cfg(test)]` blocks. `mj-cli/tests/` holds the PTY termination test, and `tests/e2e/` holds the shell/expect harness.

## macOS CI

On check-ins, the macOS runner starts only for macOS-sensitive changes. The
maintained list is `.github/macos-ci-paths.txt`; files with a macOS cfg gate,
such as `target_os = "macos"`, match automatically (the list's header names
every gate). When you add a macOS wrapper or macOS-specific module without
such a gate, add its path in the same change.

## Harness pins

Harness versions are pinned in `mj-core/src/harness_runtime.rs`. When you
change a pin, update the agent-dev container image in the same commit, so
container sessions run the same harness versions as managed bare workers:

- Codex and Claude bridges: update the `npm install --global` line in
  `containers/Containerfile.agent-dev`, as well as the package files in
  `mj-worker/assets/harnesses/`. The test
  `bridge_fallback_pins_match_the_agent_dev_containerfile` fails if the
  Containerfile and the pins disagree.
- Muse: update `mj-worker/assets/muse/runtime.json`. The image installs Muse
  from that file.
- Grok and Kimi are not in the image. They install on demand.

Pushing the commit to master runs `publish-agent-dev-image.yml`, which
publishes `ghcr.io/brokkai/mjolnir/agent-dev:latest`. Check that the run
succeeds.

## Coding Style & Naming Conventions

Use idiomatic Rust formatted by rustfmt. Prefer clear module boundaries that match the existing runtime/UI split. Name files and modules with `snake_case`; use `PascalCase` for types and enum variants, `snake_case` for functions and variables, and `SCREAMING_SNAKE_CASE` for constants. Keep comments short and useful, especially around async runtime behavior, terminal ownership, or protocol edge cases. Repository-facing text, code comments, and documentation should be written in English.

## Testing Guidelines

Always test new code in a separate named instance using `--instance <test-name>`.
Use that instance for every daemon, TUI, CLI, and end-to-end test invocation of
the new build. Never point a test build at the host's default instance or live
session data: protocol and store changes must not disrupt ongoing session work.
Keep automated tests' existing isolated configuration and data directories.

Classify every new database migration as compatible or breaking, with a short
reason beside it. Advance the migration revision for every change; raise the
minimum compatible read/write revision only for breaking changes, in the same
transaction. Compatibility includes older reads and writes, stored JSON and enum
values, constraints, and preservation of new data by older updates. Additive SQL
alone does not prove compatibility; treat uncertainty as breaking.

Test breaking migrations with isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR`. A
feature request does not authorize an incompatible upgrade of the live store.
Keep existing isolated tests isolated even for compatible changes. Never rewrite
an already-applied migration; give subsequent schema changes a new revision.

Plain Cargo commands build for the current host. Build a portable container
worker explicitly with `--target x86_64-unknown-linux-musl` or
`--target aarch64-unknown-linux-musl`.

Run every `cargo test` invocation outside the restricted sandbox with elevated permissions. The suite exercises loopback TCP and Unix sockets; sandboxed runs can fail with `EPERM` or hang and do not provide a valid test result.

Add focused unit tests near the code under test using `#[cfg(test)] mod tests`. Follow the existing descriptive test naming style, e.g. `autocomplete_updates_matches_for_prefix`. For state-machine changes, test the event transition or input handling directly rather than relying only on manual TUI checks.

For Rust code or Cargo dependency changes, run `cargo test` and
`cargo clippy --all-targets -- -D warnings` before submitting changes. For
documentation or agent-configuration-only changes, review the diff and run
applicable format or configuration checks; Cargo checks are not required.
Release work must still pass all validations in `RELEASING.md`.

Run those checks on the dev profile, which is where `debug_assert!` and
overflow checks run; this workspace sets no `[profile.release]` overrides, so a
release test run silently drops both. The build scripts defaulting to release is
not a reason to validate there.

Do not write tests for reversible, low-impact changes that mirror the implementation. If you do choose to verify your work with tests, make sure that the tests are meaningful and necessary to verify implementation.

Run tests appropriate to the change and complete required checks. Once those pass, broaden or repeat testing only when new changes, failures, or unresolved concerns justify it; otherwise, continue toward completing the task.

## GitHub Authentication

Do not run `gh auth login` or ask the user to reauthenticate because a sandboxed authentication check failed. Run normal GitHub push and PR operations with escalated sandbox permissions; treat authentication as blocked only when the actual escalated operation returns an explicit authentication error.
