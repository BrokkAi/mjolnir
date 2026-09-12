# Restore session authentication for branch export

This ExecPlan is maintained under `.agents/PLANS.md`. It is a living record of implementation and validation.

## Purpose / Big Picture

Branch export must push with the session's synchronized GitHub credentials even though its worker starts with a clean login environment. The worker must remain noninteractive and must not inherit launcher credentials or rewrite a running session's authentication files.

## Progress

- [x] (2026-09-12) Traced the missing authentication handoff and obtained approval for implementation and an upstream push.
- [x] (2026-09-12 21:08Z) Added the explicit worker root and read-only authentication attachment.
- [x] (2026-09-12 21:08Z) Demonstrated the regression through the real worker executable and passed all five worker-environment integration tests.
- [x] (2026-09-12) Passed the full Rust suite with `RUST_TEST_THREADS=8 cargo test` and formatting/diff checks.
- [x] (2026-09-12) Passed Clippy with warnings denied; the validated change is ready for the required commit and upstream push.

## Surprises & Discoveries

The login capture already runs the account shell with `-l -c`. Commit `66bf1421` added clean re-exec to `push-branch`, but only normal worker startup installs the session Git configuration. Before this fix, export passed only repository and branch arguments.

The checkout's `target` symlink pointed to a removed managed cache directory. Removed only the dangling symlink and let normal Cargo/MBX recreate its managed build target. No mount or build-storage configuration changed.

## Decision Log

Use a required `--root` argument rather than discovering a session from the repository or executable. The controller already knows the root. Attach its existing authentication files without rewriting them, so exports cannot overwrite a token rotation or change a running harness's configuration. Keep the existing upgrade refusal for workers that do not support the new argument.

## Outcomes & Retrospective

Implementation and validation are complete. Before the fix, the real worker failed the regression with `missing session Git config` from the Git pre-push hook. After the fix, that hook performs real GitHub credential lookup through the production wrapper and the push reaches a local bare repository. All five worker-environment tests pass, including token rotation/removal, stale target-helper precedence, missing/unreadable setup, and SSH transport through a fixture executable. The full Rust suite, Clippy with warnings denied, and formatting checks pass. The production target was not contacted. Deliver this validated change through the required current-branch commit and authorized upstream push; existing installed workers upgrade through normal session resume.

## Context and Orientation

`mj-cli/src/server/api.rs` constructs branch export commands on the target. `mj-worker/src/main.rs` cleans the environment before executing Git. `mj-worker/src/hel_worker_runtime/unix.rs` writes a private `bin/gh` wrapper and `gitconfig` during normal session startup. The wrapper reads `github-token` only when invoked. The generated configuration includes the user's Git settings and points HTTPS credential lookup at the absolute wrapper path.

## Plan of Work

Extend `PushBranch` with a required worker root and pass it from the export backend. In its clean startup, overlay the target settings in `launch.json`, then attach the existing Git configuration and wrapper PATH. Remove inherited token variables and indexed Git configuration already represented by the generated file. Share path construction and PATH attachment with normal setup. Report missing or unreadable authentication setup with instructions to resume the session. Do not require a token file: account authentication and SSH can work without one.

## Milestones

First establish a real-worker regression in `mj-worker/tests/worker_environment.rs`. Use production authentication setup, a fixture GitHub CLI executable, a real Git pre-push hook, and a local bare repository. The hook performs real credential lookup for github.com before the push completes. Pollute the launcher's environment and assert token rotation, removal, branch contents, and non-disclosure.

Second implement the root handoff and read-only attachment. Verify that authentication succeeds after the actual re-exec, configuration files remain untouched, and missing setup fails clearly. Preserve non-GitHub pushes and existing user Git settings. Finally run all required checks and publish the commit to the configured upstream without changing branches.

## Concrete Steps

Work in `/home/jonathan/Projects/hel3`. Run focused elevated `cargo test -p brokk-mj-worker --test worker_environment`, then elevated `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --all -- --check`. Use normal build storage. Stage only this task's files, commit on `hel3`, and push `HEAD` to its configured upstream `origin/master`.

## Validation and Acceptance

The real push command must cross clean login startup and resolve the synchronized token through Git's credential protocol, while stale launcher credentials and repository settings remain absent. The local remote's destination branch must equal the source commit. Rotation must take effect on the next export; removal must not recover launcher credentials. Missing setup must produce a useful error without secrets. Existing environment cleanup and shell authentication tests must pass, along with the full suite and clippy.

## Idempotence and Recovery

Export only reads existing session setup. No account Git configuration or running session is modified. Tests use temporary fixtures and bounded shared subprocess executors. Old workers reject the new option using the existing resume-to-upgrade response. Do not restart sessions automatically.

## Interfaces and Dependencies

The internal worker CLI gains `push-branch --root <path>`. HTTP requests and responses remain unchanged. The worker runtime exports `attach_session_git_environment(root: &Path, environment: &mut BTreeMap<String, String>) -> Result<()>` for read-only attachment and exposes the existing `configure_github_cli` setup for production-backed integration fixtures. Both share `GithubCliPaths` and PATH construction. The launch configuration reader, subprocess helpers, and Git runner remain unchanged; no crate or dependency was added.

## Artifacts and Notes

The initial regression failed with `push the session branch failed with status 1: missing session Git config`. After the handoff fix, `cargo test -p brokk-mj-worker --test worker_environment` reported `5 passed; 0 failed`. Full validation output is in the untracked build artifacts `target/branch-export-tests.log` and `target/branch-export-clippy.log`: both Cargo invocations exited 0. `cargo fmt --all -- --check` and `git diff --check` also passed. The user explicitly authorized pushing upon completion.

Revision note: initial implementation plan records the approved design and current repository findings.

Revision note: recorded the demonstrated regression, focused passing tests, and repair of the dangling build-cache symlink. The regression uses a real Git hook rather than a Git executable fake, so it also verifies a completed push.

Revision note: recorded the full passing suite and final helper interfaces; Clippy and publication are the only remaining steps.

Revision note: all implementation and validation milestones are complete; the final snapshot is ready for the required commit and authorized publication to `origin/master`.
