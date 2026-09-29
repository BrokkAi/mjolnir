# Return subagent tool results as tool results, not injected user turns

## Context

Every subagent MCP tool call (`spawn_agent`, `list_agents`, `list_profiles`, `wait_agents`, `send_input`, `interrupt_agent`, `close_agent`) is answered with a placeholder, and its real result is delivered by injecting a fake user turn into the parent conversation. That injection is the `<subagent_tool_result request_id=…>` that showed up in the composer.

The mechanism today:
1. The harness calls a tool over MCP. `mj-worker/src/subagent_mcp.rs::call` opens the worker's subagent socket and calls `send`, one blocking request/one reply.
2. The worker's `serve_one` calls `SubagentEndpoint::enqueue` (`mj-worker/src/worker_runtime/subagents.rs`). `enqueue` parks the request in a file-backed queue and returns `None` unless the exact result is already cached, so the socket replies `{accepted:true, result:None}` immediately.
3. `call` sees no result and returns a placeholder to the harness: `{request_id, accepted:true, note:"…the result will arrive in this conversation…"}`. That placeholder is the tool result the model sees.
4. The daemon observes the queued request in the worker snapshot (`server_runtime.rs` ~1176), runs `execute_subagent_tool` (`server_runtime/api.rs:172`), and delivers the real result twice: `deliver_subagent_result` (`api.rs:468`) injects it as a user turn (the bug), and `complete_subagent_request` writes it into the worker endpoint via `RelayRequest::CompleteSubagentRequest` (`worker_runtime/unix.rs:1838` → `endpoint.complete`).

So injection is the *only* channel that reaches the model on the first call, for all tools, and `wait_agents` blocking in the daemon does not return to the model as its own tool result either. A separate, genuinely-unsolicited event, a child finishing a turn, is also injected: `deliver_subagent_completion` (`api.rs:489`, called from `server_runtime.rs` ~1236) injects `<subagent_completion …>`.

Two things are wrong: solicited results should be the tool call's own response, and the one unsolicited event should be a Mjolnir notice, not a forged user turn.

### Intended outcome

- A subagent tool call blocks and returns its real result as the tool result. `spawn_agent` returns the child id immediately; `wait_agents` returns each child's output; the read tools return their data. No injected user turns for solicited results.
- The only thing surfaced into the parent conversation on its own is "a child finished," and it is a Mjolnir notice (`RecordNotice`), not a prompt and not a turn.
- `wait_agents` describes what it does and has a timeout that fits real tasks.

### What is already correct and stays

- `send_input` prompts the *child* (`api.rs` Spawn/SendInput arm): the parent is the child's user, so a prompt to the child is legitimate. Keep it; it just returns synchronously like the others.
- `complete_subagent_request` → `endpoint.complete` is the durable result path and becomes the sole delivery. Keep it.
- The file-backed queue and its idempotent replay (`enqueue` returns a cached result on a repeat request id) give restart safety for free. Keep it.
- `MAX_WAIT_SECONDS = 3600` (`mj-core/src/subagent.rs:8`) is fine as the cap.

## Part 1: Make the tool call synchronous (remove solicited-result injection)

### 1a. Endpoint waits for its own result — `mj-worker/src/worker_runtime/subagents.rs`

- Add a completion notifier to `SubagentEndpoint`: an `Arc<tokio::sync::Notify>`. `complete()` calls `notify_waiters()` after inserting the result.
- Add `async fn await_result(&self, request_id: &str, deadline: Instant) -> Option<SubagentToolResult>`: register `notified()` *before* checking `results` (to avoid a lost wakeup), check the map, then loop `tokio::select!` on the notify future and a `sleep_until(deadline)`, rechecking the map on each wake. Return `Some(result)` when it lands, `None` at the deadline.
- Keep `enqueue` as is (persist + return cached-if-present).

### 1b. `serve_one` blocks until the daemon answers — same file

- On a request: `enqueue`; if it returned `Some(cached)`, reply with it. Otherwise `await_result(request_id, now + SOCKET_WAIT_CEILING)`.
- `SOCKET_WAIT_CEILING` = `MAX_WAIT_SECONDS + 60s`. The daemon bounds `wait_agents` to `MAX_WAIT_SECONDS`, so a well-behaved daemon always completes the request before this ceiling; the ceiling only exists so a lost daemon can never wedge the socket task forever.
- If `await_result` returns `None` (ceiling hit), reply `{accepted:true, result:None}` — the old placeholder — so the model gets an honest "still running, call wait_agents to collect" instead of a hang. This should be effectively unreachable in practice.
- The request stays in the durable queue until `complete()`; a worker restart mid-wait re-serves, the daemon still completes it, and a re-call returns the cached result.

### 1c. MCP client returns the result — `mj-worker/src/subagent_mcp.rs`

- `call` already returns `reply.result` when present; that path now fires for every tool. Keep the placeholder branch only for the ceiling-timeout case.
- `send` uses a blocking `read_line` with no socket timeout, which is what we want: it waits as long as `serve_one` holds the connection. Confirm no read timeout is set anywhere on that stream.
- Optional hardening: `run_mcp_stdio` (`subagent_mcp.rs:12`) reads one JSON-RPC line, dispatches, writes, repeats — single-threaded. A long `wait_agents` blocks unrelated calls like `tools/list`. Spawn each `tools/call` on its own thread with a shared stdout lock so a blocking wait cannot stall the loop. Not required for correctness (each `call` already opens its own socket connection, and harnesses issue one tool call per turn), so treat as a follow-up.

### 1d. Stop injecting solicited results — `mj-controller/src/server_runtime.rs` and `server_runtime/api.rs`

- Delete the `deliver_subagent_result(parent, &result)` call in the subagent job (`server_runtime.rs` ~1190). Keep the `complete_subagent_request(result)` lease that follows it.
- Delete the now-unused `deliver_subagent_result` function (`api.rs:468`).

## Part 2: A finished child is a notice, not a prompt

- `deliver_subagent_completion` (`api.rs:489`) currently calls `self.prompt(parent, "<subagent_completion …>")`. Replace the body with a Mjolnir notice on the parent's conversation.
- Add `submit_notice(handle, text)` mirroring `submit_prompt` (`api.rs:717`): `handle.submit(new_command_id("subagent")?, RelayCommand::RecordNotice { text })`. `RecordNotice` records one conversation line and does not start a turn (`mj-worker/src/relay.rs` — `RelayObservation::Notice`, outcome `NoticeRecorded`).
- Text: `Subagent "<task_name>" finished turn <n> (<outcome>).` `task_name` is on the relation (`server_runtime.rs` ~1236 already reads `relation.task_name`). Keep it terse; the child's output is retrievable via `wait_agents` and the child transcript, so do not paste the output into the notice.
- Rationale for terse-and-no-turn: the design is "spawn, do other work, then `wait_agents` to collect." A parent blocked in `wait_agents` gets the output as that tool's result. An idle parent gets a visible marker and folds the result in on its next turn. Neither path forges a user message.

## Part 3: `wait_agents` honesty and timeout

- Description (`subagent_mcp.rs` ~247): replace "Register interest in completion of one or more children without blocking other tools." with "Block until the named child sessions finish their current turn, or until the timeout, then return each child's latest output. Spawn agents, do other work, then wait to collect results."
- Default timeout: change `timeout_seconds.unwrap_or(30)` (`api.rs:391`) to `unwrap_or(300)`. Cap stays `MAX_WAIT_SECONDS` (3600). Mention the 300s default and 3600s cap in the tool description.
- `spawn_agent` description (`subagent_mcp.rs` ~223): add that it returns `child_session_id` immediately and that results are collected with `wait_agents`.
- Lower-priority hardening (note, do not do now): the daemon `WaitAgents` loop (`api.rs:382`) only checks its own deadline. Threading the parent turn's cancellation into it so Alt-X aborts a blocked wait is a follow-up; today a cancelled wait keeps running in the daemon and caches its result, which is harmless.

## Deeper cleanup (out of scope, note only)

The observe-via-snapshot then complete-via-relay round trip is indirect: the worker enqueues, the daemon polls `SubagentRequests`, executes, then pushes `CompleteSubagentRequest` back. A direct worker→daemon subagent RPC (one request type, one daemon handler, one response) would remove the queue-and-poll dance entirely and make `serve_one` a straight passthrough. Larger change; not needed to fix the behavior.

## Tests

- `subagents.rs`: `serve_one`/`await_result` returns the daemon result once `complete()` is called; returns a cached result on a repeat request id; returns the placeholder at a tiny test ceiling. Reuse the existing `queue_survives_reopen_and_returns_cached_completion_idempotently` fixture style.
- `server_runtime/api.rs` (or its test module): `deliver_subagent_completion` submits a `RecordNotice`, not a `Prompt`. Assert the command variant.
- `subagent_mcp.rs`: the `wait_agents` description test already asserts the schema (`~291`); update it for the new text/default.
- Remove or rewrite any test asserting the old injection (`grep -rn "subagent_tool_result\|subagent_completion\|deliver_subagent_result" --include=*.rs`).

## Verification

1. `cargo test -p brokk-mj-worker -p brokk-mj-controller -p brokk-mj-core`; `cargo clippy` on the three; `cargo fmt -p` on each.
2. Build, `scripts/run.sh -- daemon restart`, confirm `/proc/<pid>/exe` is the new inode.
3. Live: in a parent session, `spawn_agent` returns a `child_session_id` as its tool result (no composer injection). Do other work, then `wait_agents([id])` returns the child's output as the tool result. When a child finishes on its own, the parent conversation shows a "Subagent … finished" notice, not a queued prompt. Check the daemon trace for no `<subagent_tool_result>` / `<subagent_completion>` prompts.

## Delegation

Three parts, one owner each is fine: 1 (worker synchronous return: subagents.rs, subagent_mcp.rs), 2+3 (controller: server_runtime.rs, api.rs, wait description/timeout). Part 1 and Part 2/3 touch disjoint files and can run in parallel. Review each diff, then the live check.
