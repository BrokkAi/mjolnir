# Report harness installation during launch

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. This plan follows `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

When a default harness bridge is absent or does not match the pinned version, Mjolnir's fallback launcher can spend tens of seconds resolving or installing it with `npx` or an official installer. Today the session row calls that entire interval `Start`, which makes a package installation look like an unexplained worker-launch delay. After this change, the terminal and web launch projections show `Installing Codex`, `Installing Claude Code`, or the corresponding harness product name while an install-capable default bridge is becoming ready. The label uses the harness kind, not the configured profile ID.

## Progress

- [x] (2026-09-03 23:25Z) Traced launch stage reporting from controller executors through daemon lifecycle snapshots to terminal and web rendering.
- [x] (2026-09-03 23:25Z) Located the worker-socket boundary that separates worker startup from default bridge package resolution.
- [x] (2026-09-03 23:29Z) Added a harness-valued installation stage and selected it only for install-capable default bridge launchers.
- [x] (2026-09-03 23:29Z) Applied the stage consistently to new-session and resume readiness waits.
- [x] (2026-09-03 23:31Z) Added focused behavior tests for labels, stage selection, and balanced stage lifetime.
- [x] (2026-09-03 23:37Z) Ran formatting, focused tests, the full test suite, and clippy successfully.
- [x] (2026-09-03 23:39Z) Committed the validated implementation and plan to the current branch.

## Surprises & Discoveries

- Observation: The worker publishes `control.sock` before starting the ACP bridge, so the controller can distinguish worker startup from the later bridge resolution interval without adding a new worker protocol message.
  Evidence: `mj-worker/src/hel_worker_runtime/unix.rs::run_daemon` binds the socket before spawning `hel_acp::run`, while `mj-controller/src/hel_controller/provisioning.rs::connect_and_start_worker` connects before `wait_for_native_session`.

- Observation: The default DSH launcher may install Node as a prerequisite but deliberately does not install DSH itself, so labeling it `Installing DSH` would be misleading.
  Evidence: `bridge_launch` invokes `ensure_node_22_script` and then errors when `dsh` or `dsh-acp-server` is absent.

## Decision Log

- Decision: Represent installation as `ProvisionStage::Installing(HarnessKind)`.
  Rationale: The lifecycle event then carries the product identity directly and every consumer renders the same product name. Looking up a profile in the UI would confuse profile IDs such as `codex3` with harness names such as `Codex` and would make daemon logs less informative.
  Date/Author: 2026-09-03 / Codex

- Decision: Transition from `Start` to `Installing <Harness>` after the worker socket accepts a connection, and retain `Start` for explicit executables and default launchers that do not install their harness.
  Rationale: The socket proves the worker has started. The remaining wait is where default Codex and Claude `npx` fallback resolution and Kimi or Grok official installation occurs. Explicit executables are only being started, and DSH has no harness installer fallback.
  Date/Author: 2026-09-03 / Codex

## Outcomes & Retrospective

Both new-session and resume launches now report worker startup as `Start` until `control.sock` accepts a connection, then report install-capable default bridge readiness as `Installing <Harness>`. The stage carries `HarnessKind`, so the TUI, phone API, and daemon logs agree on product names without consulting profile IDs. Explicit executables and default DSH launches remain `Start`. Focused tests, the complete workspace suite, and clippy all pass. No worker protocol or durable-state migration was needed.

## Context and Orientation

`src/hel_targets.rs` defines `ProvisionStage`, the ordered, serializable lifecycle stage sent through controller executors. `mj-cli/src/daemon.rs` records active stages and publishes them to clients. `mj-tui/src/render.rs` and `mj-cli/src/server.rs` turn stage labels into terminal and browser-visible status text.

`mj-controller/src/hel_controller/worker_binary.rs::bridge_launch` constructs each harness bridge command. A profile with an explicit `executable` runs that command directly. Default Codex and Claude commands use pinned `npx` fallbacks, while Kimi and Grok use official installer fallbacks. Default DSH only validates prerequisites and therefore is not install-capable. `mj-controller/src/hel_controller/provisioning.rs::connect_and_start_worker` handles new sessions, and `mj-controller/src/hel_controller/resume.rs` contains the equivalent restored-session path. Both launch the detached worker, connect to its socket, and then wait for the harness to report a native ACP session.

## Plan of Work

Extend `ProvisionStage` with an `Installing(HarnessKind)` value and make its label formatter return owned text where necessary so it can include `HarnessKind::display_name()`. Update daemon logging and terminal/web renderers for the new label return type.

In `worker_binary.rs`, add one shared function that chooses the readiness stage from a `HarnessProfile`: default Codex, Claude, Kimi, and Grok profiles return their harness-valued installation stage; explicit executables and DSH return `Starting`. Test this mapping directly so future fallback changes must update the progress contract.

Refactor both create and resume launch paths into two scoped stage guards. Keep `Starting` active while issuing the detached worker command and connecting to `control.sock`. Once connected, drop that guard and start the selected readiness stage while waiting for the native ACP session. Use the existing scoped guard so errors and cancellation always clear the active stage.

Add a focused asynchronous readiness test with a recording executor and an immediately-ready fake relay. It must observe `Installing(Codex)` entering and leaving exactly once, proving that the scoped wait cannot strand the UI stage. Extend rendering tests to assert the exact `Installing Codex` user-facing text.

## Concrete Steps

Work from `/home/jonathan/Projects/hel`.

Edit the files named above with `apply_patch`, then run:

    cargo fmt --all -- --check
    cargo test -p brokk-mj-controller readiness_stage_names_only_install_capable_default_harnesses
    cargo test -p brokk-mj-controller native_session_readiness_stage_is_balanced
    cargo test -p brokk-mj-tui install_stage_names_the_harness_not_the_profile
    cargo test
    cargo clippy --all-targets -- -D warnings

Every `cargo test` command must run outside the restricted sandbox because the suite exercises local sockets. The expected result is that all tests pass and clippy emits no warnings. Finally inspect `git diff --check` and commit only this plan and the implementation files.

Completed validation produced:

    cargo test
    ...
    test result: ok. 731 passed; 0 failed; 3 ignored
    test result: ok. 275 passed; 0 failed; 1 ignored
    test result: ok. 90 passed; 0 failed; 1 ignored
    test result: ok. 121 passed; 0 failed
    ... integration and doc tests passed

    cargo clippy --all-targets -- -D warnings
    Finished `dev` profile ...

## Validation and Acceptance

The unit-level acceptance is an exact label assertion: `ProvisionStage::Installing(HarnessKind::Codex)` renders as `Installing Codex`, never as a profile ID such as `codex3`. Stage selection tests must show that default install-capable harness profiles use this stage and explicit executables or DSH use `Start`. The readiness behavior test must show a balanced start and finish around the native-session wait.

The user-visible acceptance is that launching a default Codex profile on a host where `npx -y @agentclientprotocol/codex-acp@...` takes time changes the session status from `Start <clock>` to `Installing Codex <clock>` shortly after the worker socket appears. A warm or explicitly configured bridge continues through quickly, while an explicit executable remains labeled `Start`.

## Idempotence and Recovery

The changes add no durable migration and can be retried safely. `ProvisionStageGuard` balances the stage on normal completion, errors, and cancellation. If validation fails, preserve unrelated working-tree files, repair only the files listed by this plan, and rerun the focused command before the full suite.

## Artifacts and Notes

The motivating launch created its row at 23:07:12Z, started the target-side worker at about 23:07:22Z, spawned `codex-acp` only at about 23:08:10Z, and became ready at 23:08:12Z. Roughly 48 seconds therefore occurred after the worker socket existed but before the ACP bridge process appeared, matching the default `npx` fallback interval this stage identifies.

## Interfaces and Dependencies

`hel::hel_targets::ProvisionStage` must gain `Installing(hel::hel_config::HarnessKind)` and its label method must produce `Installing {HarnessKind::display_name()}`. The controller must expose one shared readiness-stage selector beside `bridge_launch`. No new crate, external dependency, process helper, or worker protocol message is required.

Revision note (2026-09-03): Created this plan after tracing the slow SSH-bare Codex launch. It deliberately scopes the label to harness identity and the post-worker-connect readiness interval, correcting the initially considered but rejected profile-name label.

Revision note (2026-09-03 23:37Z): Recorded completed implementation and validation. The socket boundary worked without a protocol extension, and testing confirmed exact harness naming plus balanced stage cleanup.

Revision note (2026-09-03 23:39Z): Marked the plan complete for the final validated commit.
