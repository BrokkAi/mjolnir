# Thor Advisor Mode

Source: `src/advisor.rs`, `src/mcp.rs`, `src/acp.rs`

Thor advisor mode is the orchestrator for an ordinary `mj` turn. Rather than
having Rust prescribe a fixed route → worker → review → judge → fix sequence,
`mj` opens one read-only ACP session for Thor and gives that session an
`mj mcp` stdio server. Thor uses MCP tools to reserve one configured delegate,
delegate work, observe progress, answer permissions, interrupt or redirect that
worker, obtain a fresh read-only review when appropriate, and write the final
answer.

The split of responsibility is deliberate:

- **Thor owns choreography.** It decides whether a request is simple enough to
  answer directly, when to delegate, what a worker should do next, whether a
  review finding warrants a fix, and what to tell the user.
- **Rust owns the primitive and the safety envelope.** It starts ACP processes,
  attaches the MCP server during session creation, streams the transcript,
  preserves connection and review rules, enforces limits, and independently proves
  that orchestration completed through the server.

It remains transcript-first: Thor's own MCP calls are visible as expandable
tool cards, and progress returned by nested worker/reviewer polls is projected
back into the same transcript the user is watching.

> **Not the arena.** This is distinct from Ragnarok in `src/ragnarok.rs`.
> Ragnarok runs a tournament between multiple competitors in disposable
> worktrees and uses its score catalog to rank fighters. Thor advisor supervises
> one normal turn in the user's real workspace. It does not probe or rank the
> Ragnarok pool, record a match, or change the score catalog.

---

## Where it runs

`advisor::run_turn` owns one advisor turn. Two command proxies build an
`AdvisorConfig` and invoke it:

- **TUI** — `src/main.rs` intercepts `UiCommand::SendPrompt`. Only one advisor
  turn may run at a time; a second prompt receives the warning *"Thor advisor
  is already running a turn."*
- **Remote** — `src/remote.rs` uses the same `run_turn` path for prompts sent
  to a remote session, with the same one-turn guard and cancellation behavior.

Both entry points pass cancellation through a `watch<bool>` channel. On cancel
or shutdown, the wrapper clears pending permission UI, gives the task
`ADVISOR_SHUTDOWN_TIMEOUT` (5 seconds) to unwind, and then force-aborts only if
it has not exited.

### `AdvisorConfig`

```rust
pub(crate) struct AdvisorConfig {
    pub cwd: PathBuf,                       // the user's real working directory
    pub additional_directories: Vec<PathBuf>,
    pub config_path: PathBuf,               // configured agents and session defaults
    pub thor_agent_source_id: String,       // the selected default agent = Thor
    pub thor_launch: Launch,                // program / args / environment to launch it
}
```

Thor is the user's configured default agent. Its own source is excluded from
nested connections to prevent recursion. The attached MCP server reserves one
configured non-Thor agent (a custom agent when the default is Thor) for normal
delegation. A branch/PR review reserves only one read-only reviewer; it does
not first create an implementation worker or a second review.

Normal delegation therefore needs one configured non-Thor agent. If the default
agent is Thor and there is no configured custom agent, Thor answers direct
questions but reports a setup blocker for delegated work instead of spawning
itself or discovering a pool of installed agents.

---

## How Thor receives the MCP bridge

Before launching Thor, `run_turn` builds this stdio descriptor:

```text
<current-mj-binary> --cwd <cwd> \
  [--additional-directory <path> ...] mcp
```

It passes that descriptor as `McpServer::Stdio` in the Thor ACP session's
`NewSessionRequest.mcp_servers`. `AcpRuntimeConfig` carries the same descriptor
through ACP `session/new`, `session/resume`, `session/load`, and `session/fork`,
so a session transition cannot silently lose its MCP tools.

The advisor descriptor also supplies private environment values to its `mj mcp`
child:

| Value | Purpose |
|---|---|
| parent Thor source id | Excludes Thor from nested connections and blocks recursive self-connection. |
| advisor-mode flag | Enables the strict reservation and completion policy. |
| optional image manifest | Lets worker prompts inherit the user's original attachments when Thor does not explicitly supply images. |
| completion-marker path and random token | Lets the parent verify server-accepted completion independently of Thor's text. |

The marker and token are deliberately removed from every nested worker/reviewer
process environment. A full-access worker therefore cannot forge the
parent-owned completion proof.

---

## The control loop

The only fixed ACP turn in the advisor is Thor's. Inside that turn, Thor calls
MCP tools in a loop of its own design, within server-side caps.

```text
                  user prompt (+ optional images)
                              │
                   ┌──────────▼──────────┐
                   │ Thor ACP session    │  ReadOnly + `mj mcp`
                   │ decides the workflow│
                   └──────┬─────────┬──────────┘
                    direct │   review-only      │ implementation
                           │         │           │
              complete(direct)      │           │
                           │   select(workflow=review)
                           │         │           │
                           │   ┌─────▼────────┐  │
                           │   │ one reviewer │  │
                           │   │ ReadOnly     │  │
                           │   └─────┬────────┘  │
                           │         │           │
                           │  complete(review)  │
                           │                     │
                           │        select(workflow=implementation)
                           │                     │
                           │             ┌───────▼───────────────┐
                           │             │ worker (Full)          │
                           │             │ submit → poll → steer  │
                           │             └───────┬───────────────┘
                           │                     │
                           │             ┌───────▼───────────────┐
                           │             │ fresh reviewer         │
                           │             │ ReadOnly, bound audit  │
                           │             └───────┬───────────────┘
                           │                     │
                           └─────────────────────▼────────────────┐
                              complete(delegated, final_response)  │
                              parent verifies receipt and renders it│
                              └───────────────────────────────────┘
```

### Thor's operating contract

`thor_prompt` is an operating manual, not a JSON router. It instructs Thor to:

1. For a small factual, explanatory, or otherwise trivial request, prepare the
   exact answer, then call `complete_orchestration` with `mode: "direct"` and
   that text in `final_response`. That tool call is the answer-delivery
   contract; Thor must not send user-facing prose before or after it.
2. For a branch, PR, diff, or code review that must not modify files, call
   `select_advisor_agents` once with `workflow: "review"`. It returns one
   recommended reviewer. Connect only that read-only reviewer, submit the
   review without `review_of`, monitor it, and complete with `mode: "review"`.
   Do not open an implementation worker or ask a second agent to review this
   review.
3. For implementation, repair, or substantial repository work, call
   `select_advisor_agents` once with `workflow: "implementation"` and use the
   reserved worker and reviewer connections. Reuse that reservation rather
   than opening another worker.
4. Connect the one worker, submit a precise implementation prompt, and poll
   from the returned `since_seq` cursor. Thor must advance its cursor and act
   on actual progress rather than blind-polling. It must steer or re-prompt
   that connection rather than open another worker.
5. Resolve a pending permission using only the option ids returned by the
   server. On drift, a stall, excess scope, or unsupported completion claim,
   cancel the worker, wait for terminal state, and re-prompt or adjust an
   advertised session setting with a concrete correction.
6. After implementation, connect the fresh reviewer session and submit an
   adversarial read-only review bound to the exact worker `connection_id` and
   `turn_id` being audited.
7. Judge the implementation review itself. It may reject speculation, ask the worker to fix
   evidence-backed findings, and repeat its monitoring loop where useful.
8. Call `complete_orchestration` with `mode: "delegated"` and the exact
   user-facing result in `final_response` as its final tool call. It must not
   send user-facing prose before or after completion; the MCP server tears down
   any remaining nested connections when its stdio session closes.

Thor is explicitly told not to expose MCP JSON to the user. It must explain the
result, validation, review/fixes, and any remaining risk in ordinary prose.

There is no Rust requirement that Thor run every optional phase in every turn.
For example, a direct answer submits no nested prompt, a review-only turn has
one read-only reviewer, and an implementation turn may need multiple worker
prompts before it asks for an independent review. The relevant completion
conditions are enforced by the MCP server rather than a hardcoded phase list.

---

## MCP tool surface

`mj mcp` is a long-lived, non-blocking ACP-client adapter. `connect` starts an
agent and returns immediately after its ACP session is ready; `submit_prompt`
returns a turn id and cursor immediately; later calls inspect or control that
same connection.

| Tool | Thor uses it to |
|---|---|
| `list_agents` | Inspect ordinary configured/default ACP agents in standalone use. |
| `select_advisor_agents` | Reserve one configured non-Thor delegate without probing or ranking agents. `workflow: "implementation"` returns a worker plus a fresh reviewer reservation; `workflow: "review"` returns one reviewer reservation. |
| `connect` | Open a nested ACP session with a `worker` or read-only `reviewer` purpose. |
| `list_config_options` / `set_config_option` | Inspect and change an advertised session setting between prompts. |
| `submit_prompt` | Start a non-blocking nested prompt, optionally with config overrides, images, or reviewer provenance. |
| `poll_progress` | Read cursor-addressable messages, thoughts, tool updates, turn status, usage, and pending permissions. |
| `respond_permission` | Choose one option advertised by a pending permission request, or reject it. |
| `cancel_prompt` | Interrupt an in-flight nested turn and reject its pending permissions. |
| `get_result` | Retrieve final text, stop reason, usage, or wait briefly for a terminal result. |
| `complete_orchestration` | Submit the exact `final_response`, validate direct, implementation, or review-only completion, seal further orchestration changes, and deliver the accepted response. |
| `disconnect` / `list_connections` | Clean up or inspect nested ACP sessions. |

`poll_progress` returns the stable structured envelope
`mj.poll_progress.v1`. Its `items` are sequenced, and `next_seq` is the cursor
for the next poll. The envelope includes connection identity, model identity,
turn status, accumulated text, usage, errors, and pending permission options.
This lets Thor make an informed decision without having to infer state from a
single text response.

---

## Single-agent reservation

`select_advisor_agents` does not call `ragnarok::muster_excluding`, inspect the
registry, probe installed agents, or apply Elo selection. Ragnarok retains all
of that multi-agent behavior; it is not part of a normal advisor turn.

The server instead chooses the first safe configured delegate deterministically:

1. Use the configured default agent if it is not Thor.
2. Otherwise use the first configured custom agent that is not Thor.
3. If no such agent exists, refuse delegation with a clear setup error rather
   than recursively launching Thor or fanning out through installed agents.

The reservation returns opaque `candidate-*` ids. It does not claim a model or
an Elo score before the ACP session starts; the connected agent is the source
of truth for its actual model and reported identity.

In advisor mode this reservation is strict:

- A worker or reviewer connection must use its currently reserved candidate
  id; arbitrary `agent` and `program` launches are disabled.
- A review-only selection rejects a worker connection. Its sole reviewer is
  read-only and cannot carry `review_of`, because it is reviewing the user's
  branch/PR directly rather than another agent's work.
- Advisor mode permits one live connection per role: one worker and, for an
  implementation workflow, one fresh reviewer session. It is not a Ragnarok
  fan-out; Thor must steer the existing worker or disconnect it before a
  replacement connection can open.
- A duplicate reservation is rejected. The first nested prompt freezes it
  completely; Thor cannot reroll a model or replace a connection that is being
  used to satisfy the audit.
- The reviewer is a distinct ACP session and read-only. With only one configured
  delegate it can be the same backend as the worker, but never the same
  connection or turn.

Workers receive `RuntimeAccessMode::Full`; reviewers receive
`RuntimeAccessMode::ReadOnly`. Nested worker and reviewer sessions get no MCP
servers of their own, preventing recursive delegation.

---

## Review provenance and completion

An **implementation** reviewer prompt in advisor mode must include:

```json
{
  "review_of": {
    "worker_connection_id": "conn-1",
    "worker_turn_id": 3
  }
}
```

For implementation work, the MCP server validates that reference against a
successful completed worker turn from the current advisor reservation. It then
prepends its own immutable adversarial-review contract with the original user
task and records the binding outside model-authored text. The reviewer is
read-only and runs in a fresh ACP session rather than the worker connection.
Review-only
requests deliberately omit `review_of`: their one reviewer receives a
server-bound read-only branch/PR review contract instead.

In advisor mode, `complete_orchestration` also requires a nonblank,
64 KiB-or-smaller `final_response`. The server echoes that value only after all
completion checks succeed and writes it with the parent token in a completion
receipt. The parent renders only that token-verified receipt, once. This avoids
relying on a particular Thor binary to resume speaking after its final tool
call or trusting JSON that merely resembles a completion result.

`complete_orchestration` has three modes:

- **`direct`** is accepted only when no nested prompt was submitted.
- **`delegated`** is accepted only when the most recently submitted nested turn
  completed successfully and the server has a nonempty, successful,
  server-bound review of an exact earlier worker turn by a fresh reviewer
  session.
- **`review`** is accepted only after a `workflow: "review"` selection and one
  successful, read-only reviewer turn. It has no implementation worker and no
  second reviewer.

After accepted completion, state-changing orchestration tools are sealed;
polling and connection cleanup remain available for generic MCP clients. In
advisor mode, Thor makes completion its final action; the MCP server shuts down
remaining children when stdio closes. The server writes the private random
token and `final_response` to the parent-owned receipt only *after* these
checks pass. `run_turn` requires a successful Thor ACP turn and that exact
receipt before it accepts the overall turn. Thor cannot substitute JSON-shaped
text or a claim of completion for that proof.

---

## Transcript visibility

Thor's ACP `SessionUpdate` stream is forwarded to the normal transcript. MCP
tool cards are namespaced as `Thor MCP · …` so their ids cannot collide with
other agent tool calls.

Nested worker/reviewer activity would otherwise be hidden inside the result of
Thor's `poll_progress` calls. `AdvisorTranscriptBridge` recognizes the
`mj.poll_progress.v1` payload, de-duplicates each
`(connection_id, turn_id, seq)` item, and projects it back into the transcript:

- Thor itself is shown with its configured source and Model option (when the
  ACP agent advertised one; otherwise an exact saved `model` setting);
- the reserved worker/reviewer connections (or sole review-only reviewer) are
  shown with their configured source before work; a saved `model` setting is
  shown on connection, followed by the ACP agent's reported identity;
- each connection identifies its actual ACP agent/version, selected model, and
  connection id;
- worker/reviewer messages and thoughts retain role/model provenance instead
  of being rendered as anonymous `agent` text;
- status updates show the nested session and turn state;
- tool calls and title-less updates retain the original action title, so a
  completion says what finished rather than merely `tool update`;
- permission requests, warnings, and server information remain visible.

Nested tool input and raw output are not duplicated into the transcript by
default because they may contain credentials or other sensitive values. The
safe title, kind, and status show the current action while the surrounding MCP
tool card remains available for inspection.

After the parent sees a token-verified completion receipt, it suppresses later
Thor message and thought chunks while retaining tool cards. The receipt is the
canonical user-facing answer, so a post-tool status line cannot overwrite or
compete with it.

The MCP server bounds retained telemetry: each connection retains at most
10,000 progress entries and 16 MiB of progress JSON, and each accumulated final
text is capped at 1 MiB. A poll reports dropped progress and truncation so Thor
does not mistake a bounded view for complete history.

### Attachments

For an advisor turn with images, `run_turn` writes a temporary image manifest
and gives its path only to the MCP child. If Thor submits a worker prompt
without explicit images, the server inherits the original user images for that
prompt. Reviewer prompts do not inherit them by default, so Thor
must not claim that a reviewer saw an attachment unless it explicitly passes
one.

---

## Permissions, limits, and cancellation

Nested agent permissions are interactive to Thor. `poll_progress` surfaces each
request with a server-generated `perm_id` and the only valid option ids;
`respond_permission` chooses one or rejects it. A cancellation drains pending
permissions. The MCP server also declines ACP elicitation forms/URLs because it
cannot render an interactive form inside an MCP tool result.

Advisor mode has these Rust-owned budgets:

| Guardrail | Limit |
|---|---:|
| Whole Thor orchestration | 40 minutes |
| Live nested connections | 4 |
| Submitted nested turns | 8 |
| Budgeted MCP tool calls | 128 |
| Worker turn | 15 minutes |
| Reviewer turn | 7 minutes |
| Pending permission | 120 seconds |

A watchdog checks each nested connection every 100 ms. It cancels expired
permissions and cancels a turn that exceeds its role budget. A guardrail-cancelled
turn stays latched until its ACP runtime emits a terminal event; the server will
not accept a replacement prompt whose state could be confused with a late
completion from the cancelled turn.

The entire MCP server also constrains requested working directories to the
operator's `--cwd` and `--additional-directory` roots. In advisor mode, it
refuses ad-hoc executables, excluded Thor identities, stale reservations, and
model changes that contradict a model explicitly reserved for the connection.

---

## Failure behavior

- **Thor exits without a valid completion marker or accepted final response** —
  the advisor turn fails, even if Thor wrote an otherwise plausible message.
- **Nested worker/reviewer fails, times out, or needs a denied permission** —
  `poll_progress` exposes the state. Thor may steer, retry within the caps, or
  stop and explain the blocker; Rust does not invent a replacement phase plan.
- **A reviewer lacks valid provenance or a fresh connection** — `submit_prompt` or
  delegated completion is rejected by the MCP server.
- **A cap or whole-turn deadline is reached** — further orchestration calls are
  rejected. Thor is instructed to clean up and report the concrete blocker
  rather than loop indefinitely.
- **User cancellation / shutdown** — the abort channel ends the Thor turn,
  clears pending permission UI, and emits a cancelled result. Live agent
  process trees are torn down during connection cleanup.

### Agent compatibility risk

The Rust, ACP, and MCP boundary is deterministic, but productive behavior still
depends on the configured Thor agent actually performing sustained multi-step
MCP-tool orchestration. A no-edit smoke test with each supported Thor binary is
the right compatibility check: ask it to select a worker, run a harmless
inspection, poll it, obtain a bound review, and complete the turn.

---

## Relationship to Ragnarok (quick reference)

| Aspect | Thor advisor (`advisor.rs` + `mcp.rs`) | Ragnarok arena (`ragnarok.rs`) |
|---|---|---|
| Purpose | Supervise one normal user turn | Tournament between competing agents |
| Workflow owner | Thor via MCP tools | Rust battle state machine |
| Agents | Thor plus one reserved worker and optional fresh reviewer | Multiple fighters and a judge |
| Workspace | User's real cwd, in place | Disposable git worktrees |
| Elo scores | Not consulted | Read to rank fighters |
| Review | Server-bound, fresh read-only reviewer | Judge/ranking outcome for battle results |
| Transcript | Thor cards plus projected nested progress | Arena/event feed |
| Shared machinery | ACP runtime and connection management | ACP runtime, agent probing, candidates, score store |
