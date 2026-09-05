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

# ExecPlans

Use an ExecPlan for a complex feature or a significant refactor. Follow `.agents/PLANS.md` from design through implementation.

Use `.agents/` as the only repository namespace for planning and design artifacts that agents own. Do not create `.agent/`.

Store each ExecPlan in `.agents/plans/`.

Keep `.agents/PLANS.md` as the standard for ExecPlans. Do not store individual ExecPlans next to `.agents/PLANS.md`.

Store design notes for LLMs or agents in `.agents/docs/`. These notes can include agent context, publication runbooks, parity notes, and similar internal information. Do not publish these notes as product documentation.

Reserve `docs/` for future documentation for human readers. Do not store ExecPlans, agent runbooks, or LLM-only context in `docs/`.

# Releases

Follow `RELEASING.md` for every release. The release version must be committed
in the workspace manifest, synchronized internal dependency constraints,
`Cargo.lock`, and the generated license report before creating the tag. Run
`node scripts/release-version.mjs check vX.Y.Z` against the clean release
commit, and never create or push the tag until that check and the release
validations pass.

# Repository Guidelines

# Git / version control

After completing and validating requested implementation changes, commit them
to the current branch without waiting for a separate request to commit. For a
larger task, also commit each logically distinct, validated set of changes when
it forms a coherent checkpoint. Treat committing as part of finishing the task
unless the user explicitly says not to commit. Do not push unless the user
explicitly asks.

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

Fix the source of a problem. Do not add a narrow fallback that hides a failure
in the primary design.

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

## Coding Style & Naming Conventions

Use idiomatic Rust formatted by rustfmt. Prefer clear module boundaries that match the existing runtime/UI split. Name files and modules with `snake_case`; use `PascalCase` for types and enum variants, `snake_case` for functions and variables, and `SCREAMING_SNAKE_CASE` for constants. Keep comments short and useful, especially around async runtime behavior, terminal ownership, or protocol edge cases. Repository-facing text, code comments, and documentation should be written in English.

## Testing Guidelines

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

Do not write tests for reversible, low-impact changes that mirror the implementation. If you do choose to verify your work with tests, make sure that the tests are meaningful and necessary to verify implementation.

Run tests appropriate to the change and complete required checks. Once those pass, broaden or repeat testing only when new changes, failures, or unresolved concerns justify it; otherwise, continue toward completing the task.

## GitHub Authentication

Do not run `gh auth login` or ask the user to reauthenticate because a sandboxed authentication check failed. Run normal GitHub push and PR operations with escalated sandbox permissions; treat authentication as blocked only when the actual escalated operation returns an explicit authentication error.
