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
- [x] (2026-09-23T09:19Z) Milestone B: `session/new` creates a Mjolnir session
      from the adapter's flags or the saved default, and `session/prompt` runs
      a turn, emits its final message, and maps its outcome to a stop reason.
      Verified by five tests — three against a hand-written fake daemon and two
      over the request and prompt shapes — by the full binary suite (157
      passed), by `cargo fmt --all -- --check`, and by
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [x] (2026-09-23T09:40Z) Milestone C: `session/cancel` interrupts the turn and
      is answered `Cancelled` even when the interrupt fails, a structured input
      request ends the turn naming what it waits for, and closing the pipe stops
      the turns the consumer can no longer see without destroying the sessions.
      Verified by eight adapter tests, the full binary suite (160 passed),
      `cargo fmt --all -- --check`, and
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [x] (2026-09-23T11:51Z) Milestone D: `docs/src/content/docs/acp-agent.md`
      documents the command for consumers, is listed in the sidebar, and is
      pointed at from the CLI reference. Verified by building the site —
      26 pages, 2059 internal links checked.

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

- Decision: the ACP session id is the Mjolnir session id.
  Rationale: a consumer's logs, `mj sessions`, and the daemon's own records then
  all name one thing, so diagnosing a consumer's failed run starts from a
  session id a person can look up directly instead of from a mapping table only
  this process knows. The adapter still refuses a prompt naming a session it did
  not create, so the identity is shared without widening what the instance can
  be asked to do.
  Date/Author: 2026-09-23, root agent.

- Decision: a prompt contributes only its text blocks; attachments and resource
  links are dropped rather than refused.
  Rationale: the prompt route takes text, and ACP requires agents to accept
  resource links, which name something in the workspace the session already
  runs in — the agent can read it directly. Refusing a turn because a consumer
  attached context would break the consumers this adapter exists for, and
  silently dropping an image is recorded here rather than left to be discovered.
  Date/Author: 2026-09-23, root agent.

- Decision: session creation resolves its identifiers from the adapter's flags,
  which are themselves optional, rather than from the ACP request.
  Rationale: ACP's `session/new` carries a working directory and MCP servers,
  not a notion of which account or host to use. Keeping placement in the agent
  command is what lets one consumer launch the same adapter for a local run and
  a remote one, and it reuses the resolution session creation already performs.
  Date/Author: 2026-09-23, root agent.

- Decision, revising the entry above on structured input: a turn waiting for
  input ends with a JSON-RPC error naming the question, not with a bare
  `Refusal`.
  Rationale: the earlier decision assumed a refusal could carry a sentence.
  `PromptResponse` has nowhere to put one, so a refusal would tell a consumer
  only that something stopped, leaving it to search the session for why — and a
  consumer that cannot see the reason has gained nothing over hanging. An error
  response both fails the turn and carries the explanation. The interrupt is
  issued first so the session is not left waiting for a person after the
  consumer has been told to give up.
  Date/Author: 2026-09-23, root agent.

- Decision: keep a cancellation marker on the adapter instead of relying on the
  daemon's interrupt alone.
  Rationale: the specification requires `Cancelled` when the client sends
  `session/cancel`, even if cancellation raises underneath. If the interrupt
  fails — a dead daemon, a lost race — the turn could otherwise finish normally
  and the consumer would read a successful end of turn for work it asked to
  stop. The marker is consumed once, so it cannot cancel a later turn.
  Date/Author: 2026-09-23, root agent.

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

### Milestone B

Reached 2026-09-23. A consumer can now create a session, send a prompt, and
receive the turn's answer with an honest stop reason. `session/new` resolves the
profile, target, and bundle from the adapter's flags — each optional, falling
back to the saved default — and returns the Mjolnir session id as the ACP
session id. `session/prompt` submits the text of the prompt, waits for the turn,
emits the final message as one update, and answers with `EndTurn` only for a
finished turn; every other outcome is a `Refusal`, with `Cancelled` reserved for
a turn that was actually cancelled.

The handler shape anticipated at the end of Milestone A turned out to be
unnecessary: chained `on_receive_request` handlers each accept their own
`AsyncFnMut` closure, so the adapter shares its state through an `Arc` instead of
implementing a state-owning dispatch loop by hand.

Three discoveries are worth keeping. The SDK marks its protocol types
non-exhaustive, so `SessionNotification` must be built with its constructor
rather than a struct literal — this first appeared as a confusing error against
a different struct in the same function. `WaitResponse` carries a required
`ApiSession`, so any test that stands in for the daemon must supply a complete
public session view, not just an outcome. And the client validates the
`mj-api-version` header on every response, so a fake daemon that omits it is
rejected before a body is ever parsed.

What remains is Milestone C: cancellation, structured input, and the failure
paths that make the adapter safe to run unattended, then Milestone D's docs.

### Milestone C

Reached 2026-09-23. The adapter is now safe to run unattended. A `session/cancel`
interrupts the turn and is answered `Cancelled` even when the interrupt fails or
the daemon is unreachable, because the adapter records the cancellation itself
rather than trusting the daemon's interrupt to be enough. A turn that stops to
ask a question is ended and the question named in the error the consumer sees,
so a program fails loudly with a reason instead of hanging on a person who is
not there. And when the consumer closes the pipe, the adapter stops the turns it
can no longer see while leaving the sessions themselves alone, since they are
durable and a person may still want to resume one.

The one course correction: the plan originally said a structured input request
would end the turn with `Refusal` and a message naming it. `PromptResponse` has
no field for that message, so a refusal would have told a consumer only that
something stopped. The turn now ends with an error carrying the question, and
the decision log records why that beats the narrower reading of the earlier
decision.

Testing this milestone required a second seam. The cancellation rules are about
what happens when the daemon misbehaves, so `cancel_with`, `turn_with`, and
`stop_active_turns_with` take a client the tests can point at a fake daemon that
fails interrupt requests; the production methods differ only in connecting
first. Clippy caught that `cancel_with` was reachable only from tests, which is
the kind of seam that quietly rots, so `cancel` now calls it whenever it can
reach the daemon, and records the cancellation itself when it cannot.

What remains is Milestone D, the documentation a consumer needs to find this
command at all.

### Milestone D

Reached 2026-09-23. The adapter is now documented where a consumer will look.
`docs/src/content/docs/acp-agent.md` explains what `mj acp` is, how to point a
program's agent command at it, what a session becomes, which ACP methods are
supported and which are deliberately not, how a turn ends, and what happens when
the program exits. It sits in the Reference group beside the HTTP API reference,
because its audience and its nature are the same: a program integrating with
Mjolnir against a documented contract. The CLI reference, which otherwise omits
hidden commands on purpose, now names `mj acp` as the exception and links to the
page — without that, the feature would exist and be undiscoverable.

The site build is the verification here, not the test suite: it renders every
page and checks 2059 internal links across 26 pages, so a wrong slug or a page
missing from the sidebar fails the build rather than shipping.

That completes the plan. The adapter answers a handshake, creates and drives
sessions, cancels them, answers the cases a program cannot, stops work when its
consumer leaves, and is documented. What has not been exercised anywhere is a
real harness: every test stands a fake daemon in front of the adapter, and the
first genuine consumer will be the real test.
