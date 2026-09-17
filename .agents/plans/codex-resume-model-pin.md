# Restore a Codex session's accepted model at launch

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

When a Codex session is restarted, the transcript currently shows a warning like:

    Warning: This session was recorded with model deepseek-flash but is resuming with deepseek-v4-pro. Consider switching back to deepseek-flash as it may affect Codex performance.

The session did record `deepseek-flash`, the user did select it, and Hel does remember it. But the message is not noise: on the resume path Codex really does build the resumed thread's turn context from the profile default in `config.toml`, and only Hel's post-handshake `session/set_config_option` call puts the accepted model back. After this change a restarted Codex session opens on the model the session accepted: the warning disappears, the harness reports the accepted model in its first startup advertisement, and the resumed thread context is correct from the start. The behavior is observable by restarting a Codex session whose model differs from the profile default, watching the transcript for the warning, and reading the launch spec at `<worker root>/acp-supervisor.json`.

## Progress

- [x] (2026-09-16 UTC) Reproduced the warning against the pinned Codex 0.153.4 source and installed `@brokkai/codex-acp` 1.11.4, and located the exact code paths that drop and skip the recorded model.
- [x] (2026-09-16 UTC) Proved with a live ACP probe that a `model` key in the bridge's `CODEX_CONFIG` removes the warning and makes the resumed session report the pinned model, and that no other startup state changes.
- [x] (2026-09-16 UTC) Implemented the launch-time pin in `mj-worker/src/worker_runtime.rs` and called it from both launch paths, moving the one accepted-configuration read ahead of the spec write in `mj-worker/src/worker_runtime/unix.rs` and `mj-worker/src/worker_runtime/reviewer.rs`.
- [x] (2026-09-16 UTC) Added the regressions: the pin's contract in `model_pin_tests`, the daemon-level launch spec in `a_codex_resume_launches_its_bridge_on_the_accepted_model`, and the reviewer's relaunch in `a_relaunched_codex_reviewer_pins_the_model_it_accepted`. Confirmed the reviewer regression fails when the call is removed.
- [x] (2026-09-16 UTC) Ran `cargo test --no-fail-fast` and `cargo clippy --all-targets -- -D warnings` on the dev profile outside the sandbox, updated this document, and committed the change.

## Surprises & Discoveries

The warning is emitted by Codex itself, not by Hel. In the pinned Codex 0.153.4 sources (`codex-rs/core/src/session/mod.rs`, `InitialHistory::Resumed` arm) Codex compares the current turn's model against the last `previous_turn_settings.model` recorded in the thread's rollout and emits `EventMsg::Warning` with exactly the text above. It fires once, when the rollout is reopened, and never on later turns.

`@brokkai/codex-acp` 1.11.4 never sends a model on resume. Its `resumeSession` and `loadSession` call `threadResume({excludeTurns, config, cwd, modelProvider, threadId})`. There is no `model` field; the provider comes from `getResumeModelProvider()`, which falls back to `MODEL_PROVIDER` and then to the profile's `model_provider`. Hel sets no `MODEL_PROVIDER`, so the request always carries `model_provider` and never a model.

That missing model has a second effect. Codex's app server can restore a thread's persisted model metadata on resume, but that path (`merge_persisted_resume_metadata`) early-returns when the request already carries a model override, and the request carrying `model_provider` alone is enough to take that branch. It also requires a state database that is not open on this path even though `thread/list` reads the same store successfully and reports the recorded model. Inspecting or repairing that store changes nothing: setting the thread's recorded model in the probe's sqlite database did not change what the resumed session used, and the resume warning still named the recorded model.

The accepted model is durably recorded and already restored on the live session. The relay's `relay-state.json` holds `config = {"effort":"high","model":"deepseek-flash"}`, Codex's `state_5.sqlite` `threads.model` is `deepseek-flash`, the rollout's `turn_context` entries say `deepseek-flash`, and Hel sends `session/set_config_option(model=deepseek-flash)` after the session is configured. Only the resume-time turn context and the warning are wrong.

Live probe (installed `@brokkai/codex-acp` 1.11.4 with Codex 0.153.4, a throwaway `CODEX_HOME`, a real recorded thread, and no prompts):

    CODEX_CONFIG unset:              warning reproduced verbatim; models.currentModelId = "deepseek-v4-pro[high]"
    {"model":"deepseek-flash"}:      no warning; models.currentModelId = "deepseek-flash[high]"
                                     modes and configOptions byte-identical to the run above
    {"model":"bogus-model-xyz"}:     accepted silently, no error; the warning names bogus-model-xyz

So the model key alone is enough, it is not validated against the provider catalogue, and it does not disturb the mode or the other startup options. Claude has the same class of problem and already solves it by pinning the accepted model into its session request metadata (`session_request_meta` in `mj-worker/src/acp.rs`); Codex's ACP surface accepts no model there.

## Decision Log

- Decision: pin the accepted model into the Codex bridge's `CODEX_CONFIG` at launch, worker-side, instead of forking `@brokkai/codex-acp` to accept a model under `_meta.codex.options`.
  Rationale: the probe shows `CODEX_CONFIG.model` reaches `thread/resume` as a config override and removes the mismatch, using only the pinned adapter version. A fork would require owning and publishing a new adapter release and bumping every pin (`mj-worker/assets/harnesses/codex/package.json`, `mj-core/src/harness_runtime.rs`, `mj-controller/src/controller/worker_binary.rs`, `containers/Containerfile.agent-dev`).
  Date/Author: 2026-09-16, agent.

- Decision: pin only the `model` key, never permission or mode keys, and merge into an existing `CODEX_CONFIG` rather than replacing it.
  Rationale: codex-acp spreads `CODEX_CONFIG` before the approval and sandbox keys it derives from the execution policy, so permission keys added here would be discarded at best and misleading at worst; `.agents/plans/codex-guardian-fork.md` records that Hel deliberately stopped injecting unconditional `CODEX_CONFIG` bypasses, and `mj-core/src/config.rs` has a test asserting that execution-policy configuration never rewrites a host `CODEX_CONFIG`. A user-supplied `CODEX_CONFIG` must keep its other keys.
  Date/Author: 2026-09-16, agent.

- Decision: do not add a worker-side check that the accepted model is still offered before pinning.
  Rationale: the worker does not hold the profile's model catalogue (that is a controller-side discovery artifact), and the harness itself is the only authority on what its selectors advertise. The existing post-handshake restoration in `mj-worker/src/acp.rs` already answers an accepted value the harness no longer lists with a warning plus an operator question, without running a user prompt on the unoffered value, and that path stays the single place where availability is interpreted. Pinning a withdrawn slug therefore produces the same operator-visible outcome as today, plus Codex's own honest mismatch warning.
  Date/Author: 2026-09-16, agent.

- Decision: derive the pin from the durable relay on every daemon start rather than persisting it in `launch.json`.
  Rationale: the relay is the durable record of what the session accepted, and recomputing keeps a restarted worker honest even when the controller restarted it from an older launch config. It also keeps the model out of the persisted launch config and out of user shells, which share the session environment but not the bridge spec.
  Date/Author: 2026-09-16, agent.

## Outcomes & Retrospective

The pin now runs in both launch paths, from `mj-worker/src/worker_runtime.rs`:

    pub(crate) fn pin_accepted_codex_model(
        harness: HarnessKind,
        environment: &mut std::collections::BTreeMap<String, String>,
        accepted: &mj_core::acp::AcceptedSessionConfig,
    ) -> Result<()>;

In `run_daemon` (`mj-worker/src/worker_runtime/unix.rs`) the accepted configuration is read from the relay before the `AcpSupervisorSpec` literal, applied to `config.environment`, and then reused for the `Arc<Mutex<...>>` handed to the ACP runtime, so there is still exactly one read. `ReviewerRole::launch` (`mj-worker/src/worker_runtime/reviewer.rs`) does the same once the managed-harness environment has been merged, and reuses the value in its `LaunchSpec`. A Codex bridge now starts with `CODEX_CONFIG` naming the accepted model, so Codex opens the resumed thread on it and never emits the mismatch warning; the worker's post-start `session/set_config_option` still runs, and remains the only place model availability is interpreted.

The inherited environment is deliberately left alone: `session_environment` is cloned before the pin in `unix.rs` and the reviewer's own session environment is separate, so only the supervisor spec carries the pin and the host's `CODEX_CONFIG` value reaches everything else unchanged.

Validation, on the dev profile outside the restricted sandbox, from `/workspace/hel`:

    cargo test --no-fail-fast                  # every target green except two pre-existing setup failures
    cargo clippy --all-targets -- -D warnings  # clean, exit 0

`brokk-mj-worker` ran 446 lib tests plus its integration targets with no failures, including `worker_runtime::model_pin_tests` (5 tests), `worker_runtime::relay_tests::a_codex_resume_launches_its_bridge_on_the_accepted_model`, and `worker_runtime::reviewer_tests::a_relaunched_codex_reviewer_pins_the_model_it_accepted`. The reviewer regression was confirmed to fail with the production call removed and to pass with it restored.

Two `brokk-mj-controller` tests fail in this container only: `setup::tests::first_terminal_launch_writes_a_local_codex_config_once` and `setup::tests::local_startup_preserves_existing_settings_and_ignores_disabled_startup`. Both fail identically on a clean tree with this change stashed, because `CODEX_HOME` in this environment points at a profile whose `config.toml` sets `model_catalog_json`, which the loader rejects by design. They are environmental, not regressions.

Two things the probe evidence does not cover. The real acceptance is still the restart the user reported: nothing automated here can prove Codex stopped warning against a live session, only that the launch which produced the warning now carries the model. And when a session's recorded model has since been withdrawn from the provider catalogue, the bridge starts on the recorded name and the existing post-start selector path reports it as dropped — the same outcome as today's resume, minus the warning.

## Context and Orientation

Hel is a Rust workspace that drives coding agents (harnesses) through ACP, the JSON protocol between a client and an agent adapter. The `mj-worker` binary runs inside the target machine or container as a per-session daemon. It owns a durable event journal called the relay, whose newest state is also the authoritative record of what the session accepted: `RelaySnapshot.config` (selector values) and `RelaySnapshot.config_options` (the harness's advertised selectors). `mj_core::acp::AcceptedSessionConfig` reads the model and reasoning effort out of that pair and is the type Hel uses whenever it restores what a session accepted.

The daemon never runs the harness directly. It writes an `AcpSupervisorSpec` to `<worker root>/acp-supervisor.json` describing the bridge command, its arguments, its environment, and its working directory, then starts `hel worker acp-supervisor --spec <path>` as a child process. That supervisor owns the bridge's process group and spawns the bridge with `mj_core::login_environment::with_overrides(&spec.environment)`, which is simply the login environment extended by the spec's entries. So `AcpSupervisorSpec::environment` is the only place a worker can set environment variables for the bridge before it starts. For Codex the bridge is `@brokkai/codex-acp`, which parses the `CODEX_CONFIG` environment variable as a JSON object and passes it as the `config` override of every `thread/start` and `thread/resume` request. In `mj-worker/src/worker_runtime/unix.rs` the spec is built and written inside `run_daemon`; `mj-worker/src/worker_runtime/reviewer.rs` builds the reviewer's equivalent spec inside `ReviewerSidecar::launch`. Both files currently read the accepted configuration from the relay only after writing the spec.

Tests live beside the code. `mj-worker/src/worker_runtime/relay_tests.rs` drives the real `run_daemon` with scripted bridges and asserts on the files it leaves behind; its `launch_config` helper builds a Codex launch config, and `DurableRelay::open` plus `record_observation` seeds relay state before the daemon starts. `mj-worker/src/worker_runtime.rs` owns the worker-behavior helpers shared by both runtime files, such as `enforce_execution_policy`.

## Plan of Work

First add one helper to `mj-worker/src/worker_runtime.rs`, next to `enforce_execution_policy`: a function that takes the harness kind, a mutable environment map, and an `AcceptedSessionConfig`, and does nothing unless the harness is Codex and the accepted configuration names a model. When it does apply, it parses the existing `CODEX_CONFIG` value as a JSON object (starting from an empty object when the variable is absent), inserts the accepted model, and writes the object back as compact JSON. A value present but not a JSON object is an error carrying the variable name, because codex-acp would fail to parse it at startup anyway and a clear message beats a child's parse error. The doc comment must state why this exists: Codex rebuilds a resumed thread from the launch request, codex-acp sends a provider but no model, so the profile default wins until Hel restores the accepted selector, and Codex warns about the mismatch.

Then call it from both launch paths, and move the accepted-configuration read ahead of the spec write so that the environment is final before it is persisted. In `mj-worker/src/worker_runtime/unix.rs`, read `AcceptedSessionConfig::from_configuration(&state.config, &state.config_options)` from the relay immediately before the `AcpSupervisorSpec` literal, call the pin on `config.environment`, and reuse that value where the code already builds `Arc::new(Mutex::new(...))` for the `LaunchSpec`. In `mj-worker/src/worker_runtime/reviewer.rs`, read the same value from the reviewer relay right after `self.open_relay()`, call the pin on the local `environment` map after the managed harness environment has been extended into it and before the spec is written, and reuse the value in the existing `LaunchSpec` tuple. Do not touch the discovery probe in `mj-worker/src/worker_runtime/discovery.rs`: it creates a throwaway session that is never resumed, so it has no accepted configuration to restore.

Finally add the regressions described under Validation and Acceptance, run the required checks, update this document, and commit only the files this task changes.

## Concrete Steps

Work from `/workspace/hel`.

    cargo test -p brokk-mj-worker
    cargo test
    cargo clippy --all-targets -- -D warnings

Every Cargo invocation must run outside the restricted sandbox with normal build storage, on the dev profile; the suite exercises loopback sockets and the dev profile is where `debug_assert!` and overflow checks run. Expect the new unit test and the new daemon-level test to fail before the change and pass after it, and expect no failures and no lint warnings at the end.

## Validation and Acceptance

A unit test in `mj-worker/src/worker_runtime.rs` proves the helper's contract: with no `CODEX_CONFIG` it produces a JSON object naming the accepted model; with an existing `CODEX_CONFIG` object it preserves every other key and replaces only `model`; a non-Codex harness and an accepted configuration with no model leave the environment untouched; and a `CODEX_CONFIG` that is not a JSON object is reported as an error naming the variable.

A daemon-level test in `mj-worker/src/worker_runtime/relay_tests.rs` proves the interface that matters. It seeds a relay whose accepted model differs from the profile default, runs the real `run_daemon` with a bridge command that cannot start and an inherited `CODEX_CONFIG` carrying unrelated keys, and then reads `<worker root>/acp-supervisor.json`. The spec's environment must contain a `CODEX_CONFIG` object whose `model` is the accepted model and whose unrelated keys survived. That spec is also the only place the pin may land: `run_daemon` clones the environment it hands the session and user shells before the pin, so the inherited `CODEX_CONFIG` reaches them unchanged.

Acceptance for the real behavior is the restart the user reported. Restart a Codex session whose accepted model differs from the profile's `config.toml` model and confirm that the transcript no longer contains "was recorded with ... but is resuming with ...", and that the restart opens on the accepted model. The earlier probe already establishes this against the pinned adapter, so the automated tests above are the acceptance gate for the code change; the probe is repeated only if the adapter version or the `CODEX_CONFIG` handling changes.

## Idempotence and Recovery

Nothing here migrates stored data or changes a protocol, so no migration, cache invalidation, or rollback plan is needed. The pin is derived on every daemon start, so a worker that starts without it simply behaves as it does today, and removing the code restores today's behavior exactly. The unit and daemon tests write only into `tempfile` directories. Do not restart, reconfigure, or publish any live session or deployment as part of this work.

## Artifacts and Notes

The probe that established the root cause ran against the installed adapter in a throwaway `CODEX_HOME` with a real recorded thread, sending the same ACP sequence Hel sends: `initialize`, `session/resume`, `session/prompt`. Its decisive outputs are quoted in `Surprises & Discoveries`. Reproduce it by exporting `CODEX_CONFIG` with and without a `model` key and comparing the resumed session's startup warnings and reported `currentModelId`.

The relevant upstream code, for anyone repeating the investigation: Codex emits the warning in the `InitialHistory::Resumed` arm of `codex-rs/core/src/session/mod.rs` (0.153.4), and codex-acp builds the resume request in `resumeSession`/`loadSession` in `@brokkai/codex-acp/dist/index.js` (1.11.4), merging `CODEX_CONFIG` into the request's `config` field in `createSessionConfig`.

## Interfaces and Dependencies

In `mj-worker/src/worker_runtime.rs`, define:

    pub(crate) fn pin_accepted_codex_model(
        harness: HarnessKind,
        environment: &mut std::collections::BTreeMap<String, String>,
        accepted: &mj_core::acp::AcceptedSessionConfig,
    ) -> Result<()>;

It is the only new API. It uses `serde_json` (already a dependency of the crate) and the existing `mj_core::acp::AcceptedSessionConfig` type; no new crate, dependency, or ACP protocol message is introduced, and the pinned adapter version stays at `@brokkai/codex-acp` 1.11.4 with Codex 0.153.4.

Revision: created from the reproduced root cause, the live probe evidence, and the chosen launch-environment fix.

Revision (2026-09-16 UTC): implementation, regressions, and the required checks are complete; recorded the outcomes above and committed the change. No decision in the log changed.
