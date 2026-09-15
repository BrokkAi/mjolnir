# ZCode background-task lifecycle is lossy (Mjolnir #1024), 2026-09-15

## What prompted this

A ZCode/GLM project-harness session running long EC2/rsync work reported four
failures, filed as Mjolnir issue #1024. This note records where each symptom
comes from, why Mjolnir and the `zcode-acp` adapter can only mitigate rather
than fix it, and the options a future decision has. No Mjolnir or adapter code
was changed for #1024; the follow-up is an upstream issue and this note.

## The layering

A ZCode session runs three stacked processes:

- `mj-worker` (Rust): the ACP client and durable relay. It renders and forwards
  events; it does not model ZCode's background exec tasks at all.
- `zcode-acp-server` (TypeScript, `BrokkAi/zcode-acp`): the ACP↔ZCode bridge. It
  forwards ZCode's task events as ACP tool-call notifications and offers a
  cancel proxy. It owns presentation, not the task lifecycle.
- `zcode.cjs` (the pinned ZCode CLI runtime, vendored, no source in either
  repo): owns the `exec_` task registry, the shutdown abort, the model-facing
  `TaskStop`/`TaskOutput` tools, and the auto-background policy.

The pinned CLI is downloaded and version-locked by
`mj-worker/src/worker_runtime/harness.rs` (the harness install machinery). All
four #1024 symptom strings exist only in `zcode.cjs`; none exist in `mj-worker`,
`mj-core`, `mj-controller`, or the `zcode-acp` source.

## Where each symptom lives (all in `zcode.cjs`)

- "Execution adapter is shutting down" / "Command was aborted before
  completion": `NodeExecutionAdapterLifecycle`. The task registry
  (`this.backgroundTasks`) is in-memory; only the command's *output* is
  persisted to files, so a shutdown leaves no durable terminal record for the
  task. In-flight commands become a "cancelled" result when `closePromise` is
  set. This is symptom 1.
- "No task found with ID": the model-facing `TaskStop`/`TaskOutput` tools
  resolve against `runtimeTaskRegistry`, which appears scoped to a branch
  generation. Completion notifications ride a separate emitter (the background
  task record's event stream, forwarded by the adapter's `BackgroundTaskListener`
  in `src/handlers/background-tasks.ts`). When the registry evicts or rescopes a
  record but the emitter keeps firing, `TaskStop` returns `TASK_NOT_FOUND` while
  completion notifications for that same `exec_` id still arrive. This is symptom
  2. Mjolnir is not in this path: its stop plumbing (`mj-worker/src/acp.rs`
  `stop_background_task`) knows only Hosted-terminal and Claude-async-task
  targets, and `HarnessKind::Zcode` maps to `HostedTerminals`
  (`mj-worker/src/worker_runtime/unix.rs`), so Mjolnir never tracks a ZCode
  `exec_` id.
- "TaskOutput was cancelled while waiting for the task": `TaskOutput` with
  `block=true`. This is symptom 3. The one adapter-side lever is to audit
  whether the bridge propagates a spurious ACP cancel/abort into the blocking
  wait.
- Auto-backgrounding an un-backgrounded command: `isBashAutoBackgroundEligible`
  plus `runBashWithBackgroundLifecycle` in `auto_on_timeout` mode. Any non-empty,
  non-`sleep` Bash command that did not request backgrounding is eligible; when
  it outlives its `timeoutMs` (model-chosen, else a large default) it commits to
  background and prints "Command running in background with ID: exec_…". This is
  symptom 4. Mjolnir sets no Bash timeout for ZCode, so this is entirely the
  CLI's tool policy.

## Why we are not fixing it in-repo now

The true fixes belong to the pinned ZCode CLI, which is a downloaded artifact we
have no source for. Mjolnir and the adapter can only paper over it, and the
mitigations are not free:

- Mjolnir durability: add a `ZcodeTasks` variant to the relay's
  `BackgroundWorkPolicy` (`mj-worker/src/relay.rs`) and wire
  `HarnessKind::Zcode` to it (`unix.rs`), so the relay records a terminal state
  for an exec task when the adapter shuts down, and can stop it. This is real new
  surface that duplicates state the CLI already (poorly) owns.
- Adapter mitigations (`zcode-acp`): on shutdown, synthesize a final
  failed/cancelled `session/update` for each tracked task before exit; audit the
  abort-signal path for the blocking `TaskOutput`; clamp the Bash tool's default
  timeout to reduce spurious auto-backgrounding. These need a rebuild and repin
  of the adapter to take effect.

Given the root cause is upstream, the chosen path is to document (this note) and
file one upstream issue rather than build mitigations speculatively.

## Anchors

- ZCode CLI (vendored, no source): `zcode.cjs` — `NodeExecutionAdapterLifecycle`,
  the `TaskStop`/`TaskOutput` handlers, `runtimeTaskRegistry`,
  `isBashAutoBackgroundEligible`, `runBashWithBackgroundLifecycle`.
- Adapter: `BrokkAi/zcode-acp` — `src/handlers/background-tasks.ts`,
  `src/handlers/dispatch.ts`, `src/handlers/extensions.ts`.
- Mjolnir: `mj-worker/src/relay.rs`, `mj-worker/src/worker_runtime/unix.rs`,
  `mj-worker/src/acp.rs`, `mj-worker/src/worker_runtime/harness.rs`.

## Follow-up

Upstream issue filed against `william0wang/zcode-acp` (the fork `BrokkAi/zcode-acp`
has issues disabled): https://github.com/william0wang/zcode-acp/issues/194. It
requests a durable terminal record on adapter shutdown; consistent `exec_` id
resolution between `TaskStop`/`TaskOutput` and the notification stream;
blocking-wait correctness for `TaskOutput`; and not auto-backgrounding
un-requested commands (or a knob to disable it). Findings also recorded on
Mjolnir #1024.
