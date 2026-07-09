# Thor Advisor Mode

Source: `src/advisor.rs`, `src/mcp.rs`, `src/acp.rs`

Thor advisor mode is the orchestrator for an ordinary `mj` turn. Rather than
having Rust prescribe a fixed route → worker → review → judge → fix sequence,
`mj` opens one read-only ACP session for Thor and gives that session an
`mj mcp` stdio server. Thor uses MCP tools to choose a ranked worker, delegate
work, observe progress, answer permissions, interrupt or redirect the worker,
obtain an independent review, and write the final answer.

The split of responsibility is deliberate:

- **Thor owns choreography.** It decides whether a request is simple enough to
  answer directly, when to delegate, what a worker should do next, whether a
  review finding warrants a fix, and what to tell the user.
- **Rust owns the primitive and the safety envelope.** It starts ACP processes,
  attaches the MCP server during session creation, streams the transcript,
  preserves ranking and review rules, enforces limits, and independently proves
  that orchestration completed through the server.

It remains transcript-first: Thor's own MCP calls are visible as expandable
tool cards, and progress returned by nested worker/reviewer polls is projected
back into the same transcript the user is watching.

> **Not the arena.** This is distinct from Ragnarok in `src/ragnarok.rs`.
> Ragnarok runs a tournament between multiple competitors in disposable
> worktrees and uses its score catalog to rank fighters. Thor advisor supervises
> one normal turn in the user's real workspace. It reads Ragnarok's ranked pool
> to choose agents, but it does not record a match or change the score catalog.

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

Thor is the user's configured default agent. The worker and reviewer are not
hardcoded in this structure: they are selected through the attached MCP server
at runtime.

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
| parent Thor source id | Excludes Thor from the nested candidate pool and blocks recursive self-connection. |
| advisor-mode flag | Enables the strict ranked-candidate and completion policy. |
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
                   └──────┬──────────┬───┘
             direct answer│          │delegate
                           │          ▼
             complete(direct)  select_ranked_agents
                           │          │
                           │   ┌──────▼───────────────────┐
                           │   │ connect worker (Full)    │
                           │   │ submit → poll → steer    │
                           │   └──────┬───────────────────┘
                           │          │
                           │   ┌──────▼───────────────────┐
                           │   │ connect reviewer          │
                           │   │ (ReadOnly, bound review)  │
                           │   └──────┬───────────────────┘
                           │          │
                           │     Thor judges findings,
                           │     optionally directs a fix,
                           │     then summarizes for user
                           │          │
                           └──────────▼──────────────────────┐
                              complete(delegated, final_response)
                              stdio close tears down children   │
                              parent verifies receipt           │
                              renders final_response            │
                              └────────────────────────────────┘
```

### Thor's operating contract

`thor_prompt` is an operating manual, not a JSON router. It instructs Thor to:

1. For a small factual, explanatory, or otherwise trivial request, prepare the
   exact answer, then call `complete_orchestration` with `mode: "direct"` and
   that text in `final_response`. That tool call is the answer-delivery
   contract; Thor must not send user-facing prose before or after it.
2. For implementation, repair, or substantial repository work, call
   `select_ranked_agents` with the original task and use the recommended
   worker and reviewer candidates.
3. Connect a worker, submit a precise implementation prompt, and poll from the
   returned `since_seq` cursor. Thor must advance its cursor and act on actual
   progress rather than blind-polling.
4. Resolve a pending permission using only the option ids returned by the
   server. On drift, a stall, excess scope, or unsupported completion claim,
   cancel the worker, wait for terminal state, and re-prompt or adjust an
   advertised session setting with a concrete correction.
5. Connect the distinct reviewer and submit an adversarial read-only review
   bound to the exact worker `connection_id` and `turn_id` being audited.
6. Judge the review itself. It may reject speculation, ask the worker to fix
   evidence-backed findings, and repeat its monitoring loop where useful.
7. Call `complete_orchestration` with `mode: "delegated"` and the exact
   user-facing result in `final_response` as its final tool call. It must not
   send user-facing prose before or after completion; the MCP server tears down
   any remaining nested connections when its stdio session closes.

Thor is explicitly told not to expose MCP JSON to the user. It must explain the
result, validation, review/fixes, and any remaining risk in ordinary prose.

There is no Rust requirement that Thor run every optional phase in every turn.
For example, a direct answer submits no nested prompt, while a delegated turn
may need multiple worker prompts before it asks for review. The mandatory
review and completion conditions are enforced by the MCP server rather than a
hardcoded phase list.

---

## MCP tool surface

`mj mcp` is a long-lived, non-blocking ACP-client adapter. `connect` starts an
agent and returns immediately after its ACP session is ready; `submit_prompt`
returns a turn id and cursor immediately; later calls inspect or control that
same connection.

| Tool | Thor uses it to |
|---|---|
| `list_agents` | Inspect ordinary configured/default ACP agents in standalone use. |
| `select_ranked_agents` | Probe the Ragnarok pool, apply Elo/diversity selection, and obtain opaque worker/reviewer candidate ids. |
| `connect` | Open a nested ACP session with a `worker` or `reviewer` purpose. |
| `list_config_options` / `set_config_option` | Inspect and change an advertised session setting between prompts. |
| `submit_prompt` | Start a non-blocking nested prompt, optionally with config overrides, images, or reviewer provenance. |
| `poll_progress` | Read cursor-addressable messages, thoughts, tool updates, turn status, usage, and pending permissions. |
| `respond_permission` | Choose one option advertised by a pending permission request, or reject it. |
| `cancel_prompt` | Interrupt an in-flight nested turn and reject its pending permissions. |
| `get_result` | Retrieve final text, stop reason, usage, or wait briefly for a terminal result. |
| `complete_orchestration` | Submit the exact `final_response`, validate direct/delegated completion, seal further orchestration changes, and deliver the accepted response. |
| `disconnect` / `list_connections` | Clean up or inspect nested ACP sessions. |

`poll_progress` returns the stable structured envelope
`mj.poll_progress.v1`. Its `items` are sequenced, and `next_seq` is the cursor
for the next poll. The envelope includes connection identity, model identity,
turn status, accumulated text, usage, errors, and pending permission options.
This lets Thor make an informed decision without having to infer state from a
single text response.

---

## Ranked role selection

`select_ranked_agents` builds its pool with `ragnarok::muster_excluding`.
Excluding Thor occurs *before* model probes start, so the system never opens a
second nested Thor session merely to reject it later.

The pool is ranked with the existing Ragnarok Elo and diversity policy:

1. `ensure_scores` and `muster_excluding` load configured agents, probe their
   available models, and join them to the score store.
2. `select_fighters` recommends the worker from the ranked pool.
3. `select_judge_only_reviewer` recommends an independent reviewer, preferring
   a different model and vendor where possible.
4. The server returns opaque `candidate-*` ids together with model, vendor,
   Elo, and provisional status. Thor must pass those ids back to `connect`.

In advisor mode this selection is strict:

- A worker or reviewer connection must use its currently recommended candidate
  id; arbitrary `agent` and `program` launches are disabled.
- The selected candidate's model is armed from the ranked record, and Thor
  cannot change it to a different model through `set_config_option`.
- The first nested prompt freezes the selection. A later re-ranking cannot
  quietly replace a connection that is being used to satisfy the audit.
- The reviewer must have a distinct server identity from the worker.

Workers receive `RuntimeAccessMode::Full`; reviewers receive
`RuntimeAccessMode::ReadOnly`. Nested worker and reviewer sessions get no MCP
servers of their own, preventing recursive delegation.

---

## Review provenance and completion

A reviewer prompt in advisor mode must include:

```json
{
  "review_of": {
    "worker_connection_id": "conn-1",
    "worker_turn_id": 3
  }
}
```

The MCP server validates that reference against a successful completed worker
turn from the current ranked selection. It then prepends its own immutable
adversarial-review contract with the original user task and records the binding
outside model-authored text. The reviewer is read-only and must be independent
from the worker it audits.

In advisor mode, `complete_orchestration` also requires a nonblank,
64 KiB-or-smaller `final_response`. The server echoes that value only after all
completion checks succeed and writes it with the parent token in a completion
receipt. The parent renders only that token-verified receipt, once. This avoids
relying on a particular Thor binary to resume speaking after its final tool
call or trusting JSON that merely resembles a completion result.

`complete_orchestration` has two modes:

- **`direct`** is accepted only when no nested prompt was submitted.
- **`delegated`** is accepted only when the most recently submitted nested turn
  completed successfully and the server has a nonempty, successful,
  server-bound review of an exact earlier worker turn by a distinct reviewer.

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
- the selected worker and reviewer are shown with their source, model, Elo,
  and provisional status before either connection begins work;
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
refuses ad-hoc executables, excluded Thor identities, stale candidates, and
model changes that contradict the ranked candidate.

---

## Failure behavior

- **Thor exits without a valid completion marker or accepted final response** —
  the advisor turn fails, even if Thor wrote an otherwise plausible message.
- **Nested worker/reviewer fails, times out, or needs a denied permission** —
  `poll_progress` exposes the state. Thor may steer, retry within the caps, or
  stop and explain the blocker; Rust does not invent a replacement phase plan.
- **A reviewer lacks valid provenance or independence** — `submit_prompt` or
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
| Agents | Thor plus Thor-chosen worker/reviewer connections | Multiple fighters and a judge |
| Workspace | User's real cwd, in place | Disposable git worktrees |
| Elo scores | Read to rank candidates | Read to rank fighters |
| Review | Server-bound, independent, read-only reviewer | Judge/ranking outcome for battle results |
| Transcript | Thor cards plus projected nested progress | Arena/event feed |
| Shared machinery | ACP runtime, `AgentHandle`, `muster_excluding`, candidate selection, score store | ACP runtime, agent probing, candidates, score store |
