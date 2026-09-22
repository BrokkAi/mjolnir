# An ACP agent that runs Mjolnir sessions

This ExecPlan is a living document. The sections `Progress`, `Surprises &
Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to
date as work proceeds.

This document must be maintained in accordance with `.agents/PLANS.md` at the
repository root.

## Purpose / Big Picture

Some consumers of Mjolnir are themselves Agent Client Protocol clients: they
start a coding agent as a child process and speak ACP to it over standard input
and output. Brokk Town's repository bots are the motivating example. A bot has
an agent command in its configuration, spawns it, sends one prompt, and reads
the final text back.

Those consumers cannot use any of Mjolnir's advantages today, because Mjolnir is
an ACP client, not an ACP agent. Its daemon drives harnesses; it does not present
itself as one. Adding this lets the same consumer that spawns `codex` or
`claude-agent-acp` spawn `mj acp` instead and get, with no change to the
consumer: any configured target including containers and remote hosts, the
durable relay and checkpoints, credentials and skills synchronization, the mbx
build cache, and indexing into the session wiki.

After this work, `mj acp` is an ACP agent on standard input and output. A
consumer that would otherwise run a local harness runs:

    mj acp --profile codex-work --target builder-podman --bundle product

and receives a normal ACP session whose prompts execute wherever that target
says, while `mj acp` drives the documented HTTP API. Every flag is optional: an
omitted profile, target, or bundle falls back to the same saved default that
`mj new` uses, so the zero-configuration case is `mj acp`.

## Progress

- [x] (2026-09-22T16:05Z) Reconnaissance: confirmed the SDK can host the agent
      role, found the agent-side handler pattern and the v1 stop reasons, and
      confirmed the routes the adapter must drive.
- [x] (2026-09-22T16:10Z) Wrote this plan.
- [x] (2026-09-22T15:56Z) Milestone A: `mj acp` exists, speaks ACP over stdio,
      and answers `initialize` without touching the daemon. Verified by an
      integration test that drives the real binary as an ACP client, by
      `cargo fmt --all -- --check`, and by
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Milestone B: `session/new` creates a Mjolnir session; `session/prompt`
      runs a turn and maps its outcome to a stop reason.
- [ ] Milestone C: cancellation, structured input, and failure paths.
- [ ] Milestone D: document the command for consumers.

## Surprises & Discoveries

- Observation: the Rust SDK this workspace already depends on can host the agent
  role; it is not client-only. `agent_client_protocol::Agent` is a role whose
  `builder()` returns a connection builder, and `role/acp.rs` carries the agent
  half of the protocol. The module named `acp_agent.rs` is a red herring: it
  launches external agents for a client, and has nothing to do with serving one.
  Evidence: `agent-client-protocol-2.2.0/src/role/acp.rs` around `impl Agent`,
  and `component.rs`, whose first example implements `ConnectTo<Client>`.

- Observation: the agent side is written with `MatchDispatchFrom`, matching
  requests by their typed schema rather than by method string, and answering
  with a `Responder`. A `PromptResponse` carries a `StopReason`, so the end of a
  turn is a typed value rather than a JSON shape the adapter invents.
  Evidence: the documented example in
  `agent-client-protocol-2.2.0/src/util/typed.rs`, which matches
  `InitializeRequest` and `PromptRequest`.

- Observation: ACP v1 has exactly five stop reasons — `EndTurn`, `MaxTokens`,
  `MaxTurnRequests`, `Refusal`, and `Cancelled` — and the specification requires
  `Cancelled` when the client sends `session/cancel`, even if the cancellation
  raises errors underneath.
  Evidence: `agent-client-protocol-schema-1.9.1/src/v1/agent.rs`, the
  `StopReason` enum and its `Cancelled` documentation.

- Observation: the adapter needs no new daemon surface. Everything it drives
  already exists on `/api/v1`: create a session, prompt it, wait for the turn,
  read the transcript, interrupt a turn, and read the artifacts. That is the
  point of building it on the documented API rather than on the relay.
  Evidence: `mj-controller/src/server/api/routes.rs`.

## Decision Log

- Decision: ship the adapter as a hidden subcommand of the existing `mj` binary
  (`mj acp`) rather than as a new workspace crate or a new binary.
  Rationale: the requirement is that the adapter ships from this repository so a
  new Mjolnir capability never waits on a consumer release. A subcommand
  satisfies that with no packaging work at all — `mj` is already installed,
  already versioned, and already matched to its daemon — while a new binary
  would mean installer, npm, release-workflow, and license-report changes before
  the first useful session. The binary already hosts hidden operational
  subcommands such as `daemon-run`, so this is an existing pattern rather than a
  new one. `.agents/PLANS.md` and the repository guidelines both discourage a
  new crate that does not have a clear packaging or ownership boundary; this one
  has neither until the adapter outgrows the binary.
  Date/Author: 2026-09-22, root agent.

- Decision: the adapter drives the documented HTTP API, not the daemon's sockets
  and not the worker relay.
  Rationale: an adapter that reaches into internals would be free to depend on
  behavior no other consumer can, and every Mjolnir change would then need the
  adapter updated in the same release — the opposite of the goal. Going through
  `/api/v1` also makes the adapter an honest test of the contract: if the
  adapter can build a session, so can anyone else.
  Date/Author: 2026-09-22, root agent.

- Decision: the adapter's flags mirror `mj new`, and omitting them uses the saved
  default.
  Rationale: one mental model for "where should this run", and it makes the
  zero-configuration case real: `mj acp` with no arguments is a valid agent
  command. It also exercises the same resolution the API already performs, so
  the adapter does not reimplement it.
  Date/Author: 2026-09-22, root agent.

- Decision: correctness before streaming. The first implementation runs a turn,
  waits for the outcome, and emits the turn's final message as one agent
  message; it does not relay incremental chunks as they arrive.
  Rationale: the motivating consumer sends one prompt and reads the final text,
  so chunk-by-chunk fidelity buys nothing there and costs a second live path
  through the events stream that must be reconciled with `wait`. Streaming is
  additive later, and the plan keeps the seam: the adapter already owns the
  translation from Mjolnir's outcome to a stop reason.
  Date/Author: 2026-09-22, root agent.

- Decision: the adapter advertises no client-side helpers, and therefore never
  sends `fs/*` or `terminal/*` requests to its client.
  Rationale: the workspace, the shell, and the file tools belong to the target's
  own worker, which already owns them under the session's execution policy.
  Asking the consumer's process for files it may not have would duplicate that
  ownership and break the case this adapter exists to serve, where the
  consumer's machine is not where the work happens.
  Date/Author: 2026-09-22, root agent.

- Decision: a structured input request that the consumer cannot answer fails the
  turn loudly instead of waiting for a person.
  Rationale: the consumer is a program. Mjolnir's own approval handling already
  happened inside the target, so an elicitation reaching the adapter means the
  model is asking a question that has no answerer. Ending the turn with
  `Refusal` and a message naming the pending request makes the consumer fail
  visibly, which is recoverable; blocking makes it hang, which is not.
  Date/Author: 2026-09-22, root agent.

- Decision: closing the adapter's standard input interrupts every active turn
  and exits, but never destroys or suspends a session.
  Rationale: a consumer that exits mid-turn should stop work it can no longer
  see, and a session is durable and resumable by design; destroying it would
  discard the work and the evidence a person would need to understand what
  happened.
  Date/Author: 2026-09-22, root agent.

## Context and Orientation

The Agent Client Protocol, called ACP, has two roles. A *client* is the program
that wants work done: an editor, a terminal tool, or one of Brokk Town's bots. An
*agent* is the program that does it: Codex, Claude Code, or an adapter. They
exchange line-delimited JSON-RPC over a pipe, normally a child process's
standard input and output. The client sends `initialize`, then `session/new`,
then `session/prompt`, and receives `session/update` notifications before the
prompt's response carries a stop reason. Either side may also send requests to
the other while a turn runs.

Mjolnir is currently only a client. The daemon holds sessions in SQLite, starts a
worker on the chosen target for one session, and exposes a documented HTTP API
at `/api/v1` with a bearer token from the daemon's data directory. `mj api-info`
prints that base URL and token path, which is how any program finds the daemon.

The pieces this plan touches:

`mj-cli/src/main.rs` declares subcommands and their arguments; it already
carries hidden operational subcommands, which is where `acp` belongs.

`mj-cli/src/api_client.rs` is the typed client for the documented routes, and
`mj-cli/src/api_commands.rs` shows how a subcommand uses it, including the
failure messages a person sees.

`mj-controller/src/server/api/` holds the routes and their wire types on the
daemon side, and its tests are the contract this adapter may rely on.

`agent-client-protocol` is the SDK, already a workspace dependency at version 2.
Its `Agent` role and `Stdio` transport are what the adapter connects with.

Terms. A *profile* is one harness account. A *target* is a runtime template and
the machine it runs on. A *bundle* is a set of repositories. A *turn* is one
prompt and everything the agent does before it stops. The *daemon* is the
per-user background process that owns all of it.

## Milestone A: `mj acp` answers initialize

At the end of this milestone a consumer can spawn `mj acp`, send `initialize`,
and receive a compatible response describing an agent that runs Mjolnir
sessions. No daemon is contacted, so this milestone proves the transport and the
role wiring without the API in the picture.

Work:

1. Add the subcommand in `mj-cli/src/main.rs`, hidden from help like the other
   operational subcommands, with the flags from the decision above and the
   global `--instance` it inherits. It runs on a tokio runtime and serves ACP on
   standard input and output until the client closes them.
2. Implement a component that builds `Agent` with `Stdio` and matches
   `InitializeRequest`, answering `InitializeResponse` with the negotiated
   protocol version and an `AgentCapabilities` that promises nothing beyond what
   the adapter does. Answer `authenticate` conservatively: authentication is the
   profile's business, already handled by the daemon, so there is no auth method
   to advertise.
3. Write an integration test that drives the binary as a client. The workspace
   already depends on the same SDK's client role, so the test can connect a
   client to `mj acp` over pipes and assert the initialize response, rather than
   hand-writing JSON.

Acceptance: the test above passes, and the daemon is never started by it.

## Milestone B: a session and a turn

At the end of this milestone a consumer can create a session, send a prompt, and
receive the agent's final text with a correct stop reason.

Work:

1. On `session/new`, resolve the profile, target, and bundle from the flags or
   the saved default and create the session through the API, keeping the mapping
   from the ACP session id to the Mjolnir session id. The ACP request carries a
   working directory; a session whose flags name a bundle ignores it, and one
   that names a project directory uses it, mirroring `mj new`.
2. On `session/prompt`, convert the prompt's content blocks to text (Mjolnir's
   prompt route takes text), submit it, wait for the turn, and answer with a
   stop reason. Map `Finished` to `EndTurn`, `Cancelled` to `Cancelled`,
   `QuotaLimit` and `Error` to `Refusal`, and `Timeout` to `Refusal` as well.
   Every one of those mappings has to be tested, because a consumer that treats
   a refused turn as a failed run depends on it: reporting `EndTurn` for a
   failed or quota-limited turn would turn a visible failure into a silent
   success.
3. Emit the final message as one `session/update` before the response, from the
   wait's final message, so a consumer that only reads updates still sees it.

Acceptance: a test with a hand-written fake API server — the same style the
route tests use, not a mocking framework — asserts the session request the
adapter sent, the prompt it submitted, the update it emitted, and the stop
reason for each outcome above.

## Milestone C: cancellation, input, and failure

At the end of this milestone the adapter behaves correctly when things go
wrong, which for a background consumer is the common case.

Work:

1. Answer `session/cancel` by interrupting the turn through the API and
   answering the outstanding prompt with `Cancelled`, as the specification
   requires even when the interrupt itself reports an error.
2. When a wait returns a pending structured input request, end the turn with
   `Refusal` and a message naming it, per the decision above.
3. Make daemon failures legible: an unreachable daemon, an unknown profile or
   target, and a refused session creation must each end the affected request
   with an error naming the fix, in the shape the CLI already uses.
4. On standard input closing, interrupt active turns and exit without
   destroying sessions.

Acceptance: tests cover each failure path, including a cancelled turn that
still answers `Cancelled` when the interrupt call fails.

## Milestone D: document the command

Consumers need to know the command exists, what the flags mean, and that omitting
them uses the saved default. Add a page under `docs/src/content/docs/` in the
style of the existing guides, link it from the docs index, and state plainly
which ACP methods are supported and which are not. Documentation is a
deliverable here rather than a follow-up: a consumer cannot guess a hidden
subcommand.

## Validation

Each milestone's acceptance is a test in the repository. Beyond those, the
end-to-end proof is a real consumer-shaped run against an isolated instance: a
temporary config directory on its own viewer port, a saved default, and a client
that drives `mj acp` while `mj sessions` shows the session it created. Where a
harness is unavailable, the same run can be made with the fake API server.

Run the repository's required checks for every milestone:

    cargo test --workspace
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --all -- --check

Run them outside the restricted sandbox, on the dev profile. Run the format
check before pushing, not only clippy: a formatting failure is a failed CI run
that clippy will not catch.

## Idempotence and recovery

The adapter adds a new command and changes no existing behavior. It creates
sessions on the consumer's behalf, so a consumer that retries a prompt after an
error creates a second session rather than resuming the first; that is the same
behavior as spawning a fresh harness process, and it is why sessions are durable
and listed by `mj sessions` rather than hidden. Cancellation and standard input
closing interrupt turns without destroying anything, so no work is lost
irrecoverably and a person can resume a session the adapter abandoned.

## Outcomes & Retrospective

Written at completion of each milestone.

### Milestone A

Reached 2026-09-22. `mj acp` is a hidden subcommand that serves the Agent role
on standard input and output and answers `initialize` with the one protocol
version it implements and no optional capabilities. The whole surface is three
files: the module, its wiring in `main.rs`, and one integration test.

The test drives the real binary, not a stand-in: it builds an `AcpAgent` from
`CARGO_BIN_EXE_mj`, connects the SDK's client role to it over pipes, and asserts
the handshake. It also asserts that no `daemon.json` appeared in the temporary
data directory it passed, which is the milestone's other claim — a consumer can
launch the agent before anything is configured, so the handshake must not drag
the daemon up with it.

Two things worth carrying forward. First, the SDK's server-only mode is
`connect_to`, which returns when the client closes the pipe; Milestone B's
prompt handling will need `connect_with` or a handler that owns state, because
answering a prompt means making several HTTP calls and emitting notifications
while the request is in flight. Second, the format check earned its place
immediately: rustfmt orders module declarations, so `mod acp;` had to move above
`mod api_client;`, which clippy would never have caught.
