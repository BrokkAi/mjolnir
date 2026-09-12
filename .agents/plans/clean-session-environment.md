# Start sessions from the target login environment

This ExecPlan follows `.agents/PLANS.md` and is maintained during implementation.

## Purpose / Big Picture

Sessions must not inherit Cargo settings, loader paths, virtual environments, or parent-agent state from the terminal that started Mjolnir. Resolve the target account's noninteractive login environment from an empty bootstrap, then apply explicit session settings, covering harnesses, reviewers, probes, installers, user shells and ACP terminals.

## Progress

- [x] Inspected worker, harness, terminal, transport and development launcher paths.
- [x] Confirmed noninteractive login shell and no new configuration feature.
- [x] Implement shared resolution, clean worker re-exec, and explicit subprocess environments.
- [x] Add capture, timeout, terminal and real-worker Git regression tests.
- [x] Update launcher, build profiles, script fixtures and container login configuration.
- [x] Full Rust suite, 14 script tests, and clippy passed.
- [x] Real-worker Git and lifecycle tests passed after preserving argv[0] and the lifecycle command prefix.
- [x] Commit implementation, merge upstream and validate the combined result.
- [x] Prepare the validated master branch for the authorized upstream push.

## Surprises & Discoveries

Existing discovery inherits environment and captures only PATH. ACP terminals inherit worker environment. Supervisor JSON stores explicit overrides; resolved exports must stay in memory.

## Decision Log

Decision (2026-09-12, Codex): use the user-selected noninteractive login model, rather than an ambient-variable blacklist. Rationale: unknown parent exports must be excluded automatically, while intentional login-profile exports remain valid.

Use account database identity and shell, never invoking HOME or SHELL. Use `/usr/bin/env -0` for capture: GNU and current Apple implementations support it, avoiding another worker CLI helper. Capture through the shared bounded subprocess executor with an empty environment and five-second deadline. Keep transport authentication separate. Re-exec the worker itself before runtime initialization so worker-owned Git and other helpers are also clean. Transfer its snapshot through an internal CLI flag and environment, not a file. Checkpoint-only mode uses the account bootstrap without sourcing shell files. An explicit target_environment map in WorkerLaunchConfig preserves target settings separately from primary-profile settings for reviewers. Keep deadline supervision active until pipe readers finish, including after their parent shell exits.

## Milestones

First, introduce `hel_login_environment::resolve`, `bootstrap`, and `with_overrides` in the core library. The account database supplies identity; the login shell exports a framed environment through `/usr/bin/env -0`. Successful discovery is cached in a Tokio OnceCell. The bounded executor must clear its environment and enforce its deadline until both output readers finish. Capture tests prove parent pollution is absent and values beyond pipe capacity are intact.

Second, bootstrap `mj-worker` before constructing its runtime. Re-exec the same executable with a clean environment and an internal `--login-environment-ready` flag; Linux uses `/proc/self/exe` so replacement or unlinking cannot change the executable. Initialize the cache from this explicit handoff. Checkpoint-only workers use the account bootstrap without a shell. Propagate target settings separately from primary profile settings into reviewers. Resolve in-memory launch maps before spawning ACP bridges and user shells, and clear inherited environments for bridges, supervisors, installers and ACP terminal commands. Supervisor files retain only explicit and generated overrides. A real-worker Git test launches with a bogus HOME, SHELL, PATH and GIT_DIR and must still produce the requested repository diff.

Third, replace `cargo run` in the development script with `cargo build --message-format=json-render-diagnostics` and direct execution of the reported artifact. Preserve argument boundaries and build worker profiles consistently, including macOS Bash empty-array behavior. Add image login exports for Codex, Playwright and Muse. Fourteen launcher/install tests must pass. Finish by validating Rust, integrating upstream, committing on the existing branch and pushing.

## Outcomes & Retrospective

Implementation and integration are complete. The full Rust suite passes on the merged branch with `RUST_TEST_THREADS=8`; clippy with `--all-targets -- -D warnings` also passes. All fourteen launcher/install script tests and shell syntax checks pass. Real-worker tests prove that polluted launcher settings cannot redirect Git and that checkpoint workers remain visible and stoppable after re-exec. Existing workers retain their old environment until normal restart; no active session was interrupted.

The first broad run encountered an unrelated loopback timeout, resolved by limiting test concurrency. The new deadline test initially expected the internal cancellation text rather than the executor's public timeout message; its assertion was corrected. Final review identified the need to preserve the worker's command-line identity across re-exec, now covered by a real lifecycle test.

## Context and Orientation

`src/hel_acp.rs` launches bridges and terminal callbacks. `mj-worker/src/hel_worker_runtime/` constructs primary, reviewer and probe launches. `src/hel_targets.rs` owns bounded subprocess execution. `scripts/run.sh` currently launches through Cargo.

## Plan of Work

Add shared login resolution with account-derived bootstrap and framed NUL capture. Cache successful resolution per process. Clear ambient environments at session launches, carry the resolved environment in memory, and keep supervisor JSON limited to explicit overrides. Preserve profile/target settings and Mjolnir-owned credentials and homes. Remove PATH-only discovery. Put required container exports in login profiles. Build and execute Cargo's reported CLI artifact directly.

## Concrete Steps

Work in `/home/jonathan/Projects/hel`. Implement resolver and subprocess boundaries, update script fixtures, then run `cargo fmt`, elevated `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `node --test scripts/run.test.mjs`. Stage only own files, commit master and push upstream.

## Validation and Acceptance

Prove parent-only exports disappear while target startup exports and explicit overrides survive. Cover malformed framing, newlines, output beyond 64KB, shell failures and timeout cleanup. Exercise real terminal and user-shell subprocesses. Checkpoint-only recovery must avoid login initialization. Do not log resolved environment values or persist them in supervisor files.

## Idempotence and Recovery

Do not restart active sessions automatically. Existing processes retain their environment until normal restart. Discovery failure prevents harness startup with useful context. Build artifacts stay in target.

## Interfaces and Dependencies

Reuse subprocess executors and launch maps. Add environment clearing to shared command specifications with backward-compatible defaults. No new crate or configuration interface.

## Artifacts and Notes

Implementation, commit and push are authorized. Leave unrelated untracked files untouched.

Revision note: implementation added worker-level re-exec and a minimal checkpoint bootstrap to cover helpers beyond ACP, while preserving recovery independence from shell startup. Upstream Codex accounting and durable API event updates were merged cleanly and included in final validation.

Validation artifacts: `target/clean-env-tests.log` and `target/clean-env-clippy.log`; these remain untracked build artifacts. Shell validation is `bash -n scripts/run.sh scripts/build-linux-worker.sh` plus `node --test scripts/run.test.mjs scripts/install.test.mjs`.

Final review found that worker liveness matches the installed `hel worker run --root` command prefix. Re-exec now preserves argv[0] and appends the internal global flag after the existing arguments. A real checkpoint-worker test proves it remains visible and stoppable. Both real-worker integration tests pass. The full suite passed before this localized adjustment; the subsequent full merged validation also passed.

Completion evidence: `target/clean-env-merged-tests.log`, `target/clean-env-merged-clippy.log`, and `target/clean-env-worker-regression.log` all record successful runs. Implementation commit: `66bf1421`; upstream merge: `59d10da5`. This final revision records completion without changing runtime code.
