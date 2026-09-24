# Make empty-session recovery reproducible and verify worker instance attribution

This completed ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective as work proceeds. It covers #1063 only. The user requested planning and implementation one issue at a time. #965, #1018, and #1063 are implemented, validated, pushed, and closed. The later queue remains #1073, then #1083.

## Purpose / Big Picture


QA must be able to exercise recovery of a never-prompted Codex or Claude session without guessing which process is safe to stop. Mjolnir should attempt to reload the existing native session, replace it only when the adapter reports it missing and durable evidence proves it was unused, and retain the Mjolnir session identity. Used conversations must never be replaced with empty ones.

Issue #1063 reports that restarting the controller daemon left the session workers running, so no native resume occurred. The tester could not identify an instance-owned worker through its environment. Existing code has since addressed the attribution half and exposes a session Restart action. The proposed work is to verify those existing paths, cover the process boundary in tests, and document a reproducible procedure.

## Progress


- [x] (2026-09-23) Finished #1018 in `d81fd67c`, pushed it to origin/master, recorded validation, closed the issue, and removed its active-work label.
- [x] (2026-09-23) Read #1063 and its history; it was open and unassigned. Self-assigned it and added `agent-in-progress` before investigation.
- [x] (2026-09-23) Located existing instance stamping, clean worker re-execution, TUI Restart, CLI suspend/resume, and empty-native-session recovery tests.
- [x] (2026-09-23) Prepared this narrowed plan without changing application code for #1063.
- [x] (2026-09-23) Received user approval for the isolated proof and implementation.
- [x] (2026-09-23) Proved native reload, empty-session replacement, queued prompt delivery, and process attribution in disposable real-worker fixtures. The existing selected-session suspend/resume route is covered at its controller boundary; no full TUI automation was added.
- [x] (2026-09-23) Added Codex and Claude process-boundary regressions, including used-history refusal, strengthened the instance environment test, and documented the selected-session procedure and QA fixture.
- [x] (2026-09-23) Validated the full dev-profile `cargo test` suite, final three-case worker recovery test, controller same-harness resume test, `cargo clippy --all-targets -- -D warnings`, formatting, `npm run check`, `npm run build`, and diff whitespace.
- [x] (2026-09-23) Committed as `fdd49cc1`, merged concurrent upstream controller changes, revalidated the controller suite and clippy, pushed `d68e0ea1` to origin/master, posted evidence to #1063, closed it, and removed `agent-in-progress`.
- [x] (2026-09-23) Prepared #1073's separate review plan after closing #1063; implementation awaits its own approval.

## Surprises & Discoveries


Commit `6f9f65da` (2026-09-17, part of #1065) explicitly implemented the observability half of #1063. `mj-controller/src/controller/worker_binary/launch.rs::worker_launch_config` places `MJ_INSTANCE` into target settings using `mj_core::config::instance_identity()`. A named instance uses its name; an unnamed instance fingerprints its data directory. `mj-worker/src/main.rs::bootstrap_login_environment` overlays `launch.target_environment` before clearing and rebuilding the worker process environment through re-execution. This means checking only a Rust configuration map is weaker evidence than checking the real process after re-execution.

`mj-cli/src/dashboard/actions.rs` already handles `DashboardAction::RestartSession` by suspending an active session and then resuming it with its recorded profile, target, workspace, mounts, and allocation. The command palette exposes this action without a default hotkey. The CLI already exposes `mj suspend --session <id>` and `mj resume --session <id>`. These are candidates for the documented supported procedure; a local proof must establish that a never-prompted session actually reaches the relevant native reload failure and replacement path.

`mj-worker/src/acp/session.rs` replaces a missing Codex/Claude native session only when `native_session_may_have_history` is false. `mj-worker/src/acp/tests.rs` already proves unused replacement, queued prompt delivery, and refusal to replace used history. The remaining coverage should connect worker process startup/restart and instance ownership to that behavior, rather than duplicate the same lower-level assertions.

`mj-worker/tests/worker_environment.rs::checkpoint_worker_remains_visible_and_stoppable_after_clean_reexec` already launches a real isolated worker through `BoundedProcessExecutor`, checks liveness, and stops/joins it before deleting files. Reuse that ownership and cleanup pattern. `tests/e2e/session_restart_chaos.sh` is an existing disposable-container harness; it is not permission to signal arbitrary host workers.

The proof in `mj-worker/tests/empty_session_recovery.rs` uses a genuine `mj-worker` child and a scripted adapter. Both harness cases log `initialize`, `session/load`, `session/new`, then `session/prompt`; each test passes with one new session and one prompt, and the durable journal records the warning and completed queued command. The first prototype used `unconstrained` execution policy while its fake adapter advertised only guardian modes, so the worker correctly rejected `session/new` for lacking `agent-full-access`. Changing the fixture to `configured_approvals` made the adapter policy coherent. The real process environment test passed with a conflicting launcher `MJ_INSTANCE`, proving clean re-execution uses the configured value. The process fixture starts from a persisted relay rather than driving the full TUI; controller suspend/resume coverage and the public CLI route are checked separately.

## Decision Log


Decision (2026-09-23, proposed): start by proving existing supported session Restart/suspend-resume behavior. Do not change `mj daemon restart` to restart every worker: its detached-worker behavior is intentional, and the issue needs a selected-session recovery procedure.

Decision (2026-09-23, proposed): retain the existing instance identity scheme and stamping implementation. Test the real worker process environment after clean re-execution, including a conflicting launcher value, instead of adding a second ownership field or trusting inherited environment.

Decision (2026-09-23, proposed): use scripted ACP adapters, temporary repositories, and isolated config/data directories for deterministic empty-session recovery. A scripted adapter is a small test process speaking the same JSON protocol as the real harness, with controlled new/resume responses. No paid model request or live session is needed.

Decision (2026-09-23, proposed): expect documentation and regression tests to be the main deliverables. If the proof finds a missing link in the existing implementation, fix that source within the selected-session recovery path. If a new public lifecycle command is actually needed, present that finding before expanding the public interface.

Decision (2026-09-23): keep the process-level regression independent of a live provider and of the full controller/TUI lifecycle. The persistent relay records exactly the state handed to a resumed worker, and the existing controller test proves that same-harness resume carries native identity. This isolates the missing process boundary without duplicating the entire provisioning system. The user-facing procedure uses the already supported Restart action or CLI suspend/resume.

## Context and Orientation


`mj-core/src/config/loading.rs::instance_identity` defines the ownership identity. `mj-controller/src/controller/worker_binary/launch.rs` builds the target environment and worker ownership marker. `mj-worker/src/main.rs` constructs a clean login environment and replaces the worker process with itself; its environment at that point is what process-inspection tooling observes.

`mj-cli/src/dashboard/actions.rs`, `mj-cli/src/api_commands.rs` and the daemon lifecycle handlers expose session suspension and resumption. The controller's checkpoint/resume machinery preserves the native session ID and restores the native files. `mj-worker/src/acp/session.rs` decides whether a missing native session may be replaced. `mj-worker/src/worker_runtime/relay_tests.rs` contains process/runtime fixtures around the durable relay, which records session events and whether native history may have been used.

Human instructions belong in `docs/src/content/docs/sessions.md` and, if needed, a short clarification in `docs/src/content/docs/cli-reference.md`. Agent-only QA details and evidence belong in `.agents/docs/empty-session-recovery-qa.md`. Tests should extend existing colocated modules or the existing worker environment integration test; no new crate is needed.

## Plan of Work


### Milestone 1: Prove a selected-session restart and attribution locally


Use a named disposable instance such as `qa-empty-recovery-1063` with temporary `MJ_CONFIG_DIR` and `MJ_DATA_DIR`, a temporary Git repository, and scripted Codex/Claude ACP bridges. Open a native session and send no prompt. Record the Mjolnir session ID, native session ID, worker PID/root and instance stamp. Exercise the existing session Restart operation or equivalent CLI suspend/resume sequence, and have the bridge report the original native ID missing on reload. Observe the native reload request, a replacement native ID, the original Mjolnir ID, and the visible replacement warning.

Verify that the real worker environment after re-execution contains the launch configuration's instance identity, even when the launcher supplied a different value. On Linux inspect only the exact fixture-owned PID; on other Unix hosts use a fixture process reporting its environment. Stop and join every owned process before removing files, with bounded cleanup through shared subprocess helpers. Do not run the new binary against the default instance or signal any live session.

Acceptance: a reproducible selected-session operation reaches the empty-session replacement path and the worker can be attributed to the isolated instance. Record what the existing operation actually does before writing its documentation. If it fails, the failure identifies the specific gap to fix; do not claim the recipe works from code inspection alone.

### Milestone 2: Add behavior coverage and the verified procedure


Extend the existing process/runtime fixtures to cover both Codex and Claude across a genuine worker restart or suspend/resume boundary. Assert that the old native ID is attempted, the missing unused session is replaced within the same Mjolnir session, queued work reaches the replacement once, and used history is refused instead of replaced. Retain existing lower-level recovery tests and reuse their fake adapter behavior where practical.

Add a regression at the actual worker entrypoint proving `MJ_INSTANCE` survives clean re-execution and comes from the controller-generated target settings. Keep the identity check tied to the exact fixture-owned process rather than process-name matching.

Document the proven Restart/suspend-resume workflow and explain that daemon restart preserves detached workers. Record a deterministic QA recipe and expected observations in the agent runbook. Note that a worker launched before instance stamping may need a supported relaunch to acquire the marker. No schema migration is planned.

### Milestone 3: Validate and publish


Run the focused worker environment, recovery and controller lifecycle tests that cover the change, then `env -u NO_COLOR cargo test` and `cargo clippy --all-targets -- -D warnings` in the dev profile with elevated permissions. Use normal mbx build storage. Run formatting and diff checks, plus the documentation site's applicable check if public documentation changes. Record any limitation of fake-adapter verification rather than claiming live-provider coverage.

Commit the validated changes on the current branch and push HEAD:master, as already authorized. Add the evidence to #1063, close it, and remove `agent-in-progress`. Only then investigate #1073 and prepare its separate plan.

## Validation and Acceptance


A tester following the documented selected-session workflow can observe a native resume/load attempt without relying on controller-daemon restart. Both supported empty-session replacement cases preserve the Mjolnir session and pending work; used native history still fails safely. A real fixture worker retains the correct instance identity through environment clearing and re-execution. Tests and the QA recipe operate only on disposable stores and owned processes.

## Idempotence and Recovery


Each proof and test gets fresh temporary storage. Teardown stops the owned process group and joins its owner before deleting files. A failed proof preserves its diagnostic evidence until cleanup has completed. No existing native transcript is deleted to manufacture an empty session, and no production database upgrade or migration is involved.

## Outcomes & Retrospective


The isolated real-worker proof passes for both supported harnesses, including refusal to replace used history, and the process attribution check passes after clean re-execution. The full Rust suite, final focused worker cases, controller resume case, clippy, and docs checks pass. Existing application paths need no production code change; the deliverables are regression coverage and the reproducible selected-session procedure. The result is published and #1063 is closed. #1073 has a separate review plan but no implementation, and #1083 remains untouched.

Revision (2026-09-23): initial review plan, narrowed after finding the attribution fix in `6f9f65da` and the existing Restart/suspend-resume workflow.

Revision (2026-09-23): recorded the approved proof, its adapter-policy correction, process-boundary results, and the intentionally separate controller lifecycle coverage.
