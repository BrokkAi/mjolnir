# Make `mj-agents wait` dependable for children of any duration (#1034)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`,
`Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.
It must be maintained in accordance with `.agents/PLANS.md` (read that file from the
repository root before revising this one).

## Purpose / Big Picture

A Mjolnir parent session delegates work to child sessions through the `mj-agents` MCP
server and collects a child's report by calling the `wait` tool. "MCP" is the Model
Context Protocol: the coding harness (Claude Code or Codex) is the MCP *client*, and
Mjolnir runs a small MCP *server* inside the session's worker process. A tool call is
one JSON-RPC request from the harness to that server, and the harness abandons a call
that stays silent too long.

Today a parent that asks for a long wait can get no answer at all. The tool advertises
`timeout_seconds` up to 3600, the harness Claude Code abandons a silent stdio tool call
after 1800 seconds, and Mjolnir's own wait can answer far later than the deadline the
caller asked for. The user-visible effect, quoted from issue #1034, is

    MCP server "mj-agents" tool "wait" sent no response or progress for 1801s; aborting.

with no child result returned and no way for the parent to tell whether the children
failed, finished, or are still working.

After this change a parent can wait for a child of any duration without a spurious
failure, and it can still ask for the full hour the tool advertises. While a `wait` is
open the server sends an MCP progress notification every 30 seconds, so Claude Code's
idle timer never fires and the call survives to its own deadline. Every `wait` is answered
within the time the caller asked for, by the process nearest the caller, and the answer
says plainly either "these children are finished, here is each one's report" or "still
running, call `wait` again". You can see it working with a scaled-down live test
(45-second wait, daemon restarted underneath it) that fails on today's build and passes
after the change, and with one real wait past the old 1800-second abort point on each
harness.

The limit stays at 3600 seconds for every harness. Lowering it for everyone because one
client watches for silence would make every parent pay for a client-specific limit; the
progress notification removes that limit instead. A cap is applied only where a harness is
measured to need one, from the harness kind the worker already knows.

## Progress

- [x] (2026-09-17) Read issue #1034 and its follow-up comment; established the three
      distinct defects with evidence (see `Surprises & Discoveries`).
- [x] (2026-09-17) Measured the harness limits from the shipped client binaries and from
      a live Codex session.
- [x] (2026-09-17) Reproduced the server missing its own deadline live at 45-second
      scale in a private instance.
- [x] (2026-09-18) Revised after the maintainer's decision: no 600-second cap, progress
      notifications are the fix rather than defence in depth, and any cap is per harness
      and only where measured.
- [ ] Milestone 1: one shared timeout rule, an unambiguous result shape, the daemon's
      deadline counted from the caller's request, and one log line per wait.
- [ ] Milestone 2: the worker answers at the caller's deadline even when the daemon does
      not.
- [ ] Milestone 3: MCP progress notifications while a call is in flight.
- [ ] Milestone 4: measure the real ceiling on each harness with one wait past 1900
      seconds, and cap only a harness that is measured to need it.

## Surprises & Discoveries

- Observation: Claude Code's limit is an *idle* limit of 1800 seconds for stdio MCP
  servers, it is reset by progress notifications, and a per-server `timeout` in the MCP
  config raises it. The total-call cap defaults to 100000 seconds, so silence, not
  duration, is what kills the call.
  Evidence: strings extracted from the shipped client
  `/home/jonathan/.local/share/claude/versions/2.1.275`:

      var Fr=300000,$r=1800000,Br=new Set(["sse-ide","ws-ide","sdk"]);
      function Ur(e){let n=e?.type??"stdio";if(Br.has(n))return 0;
        let r=a.CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT??(n==="stdio"?$r:Fr);
        if(r<=0)return 0;let s=e?.timeout!==void 0&&e.timeout>=1000?e.timeout:0;
        return Math.min(Math.max(r,s,1000),Ro(e))}
      function Ro(e){let r=(e?.timeout!==void 0&&e.timeout>=1000?e.timeout:void 0)
        ??a.MCP_TOOL_TIMEOUT??Er;return Math.min(Math.max(r,1000),Ug)}
      var Er=1e8, Ug=2147483647

  and, in the tool-call function, the watchdog that fires the message from the issue
  runs every 30 seconds against a timestamp `j` that both the response path and the
  progress path reset:

      Ce=setInterval(()=>{ ... if(q>0&&Date.now()-j>q){ ... "sent no response or
        progress for " ... }},30000)
      onprogress:(Ze)=>{ ve.armedAt=0, j=Date.now(), ... }

  The stdio server config schema accepts an optional `timeout`, so the value is settable
  per server:

      type:R("stdio").optional(),command:o(),args:C(o()).optional(),
      env:fe(o(),o()).optional(),timeout:Ii(), ...

- Observation: Codex tolerates a `wait` far longer than 60 seconds on the delivery path
  Mjolnir uses, so the frequently assumed 60-second MCP default does not apply here. The
  exact Codex ceiling is unknown.
  Evidence: in a private instance, a `deepseek` (Codex) parent was told to call the tool
  once with `timeout_seconds: 150`. The turn finished normally and the model reported

      Wall time: 151.4291 seconds
      Output: {"request_id":"7ec1...","is_error":false,"message":"{\"agents\":[{
        \"child_session_id\":\"7e93...\",\"state\":\"running\",\"output\":null}],
        \"timed_out\":true}"}

  The Codex binary contains `tool_timeout_sec` (a per-server config key) and
  `notifications/progress` / `progressToken`, but Mjolnir hands Codex this server over
  ACP (`mj-worker/src/acp/launch.rs:171`, `McpServerStdio::new("mj-agents", worker)`),
  and that wire format carries only a name, command and arguments, so `tool_timeout_sec`
  is not reachable from Mjolnir on this path. Confidence: the 150-second measurement is
  confirmed; the true ceiling is unknown.

- Observation: Kimi never receives these tools, so its MCP timeouts are out of scope.
  Evidence: `subagent_tools_enabled` in
  `mj-controller/src/controller/worker_binary/launch.rs:432` returns true only for
  `HarnessKind::Claude | HarnessKind::Codex`. For the record, Kimi's MCP client applies a
  *total* 60000 ms request timeout by default and does not pass `resetTimeoutOnProgress`,
  so progress notifications would not help it; if Kimi is ever added as a parent harness,
  the cap chosen here (600 seconds or less) would still be too long for it without a
  staged `toolTimeoutMs`.

- Observation (the reproduced defect): Mjolnir's wait deadline is measured from the
  moment the daemon starts executing the request, and any re-execution restarts it from
  zero. A 45-second wait answered after 61 seconds because the daemon was restarted 15
  seconds into it.
  Evidence: in a private instance `plan1034`, driving the real MCP shim
  (`hel worker subagent-mcp --socket <worker root>/subagents.sock`) directly:

      [wait] answered after 61.0s
      ... "message": "{\"agents\":[{\"child_session_id\":\"7e93...\",
          \"state\":\"running\",\"output\":null}],\"timed_out\":true}"

  with `mj -i plan1034 daemon stop` run at t=15 s and the daemon restarted at t=18 s
  (`daemon.json` `started_at: 2026-09-17T23:55:13Z`, inside the call's window). An
  undisturbed 45-second control run answered in 30.3 s for a 30-second request, so the
  loop itself is accurate when nothing interrupts it. The mechanism is in
  `mj-controller/src/server_runtime/api.rs`, where the wait deadline is

      let deadline = tokio::time::Instant::now()
          + Duration::from_secs(timeout_seconds.unwrap_or(DEFAULT_WAIT_SECONDS)
              .clamp(1, MAX_WAIT_SECONDS));

  and in `mj-controller/src/server_runtime/run.rs:509`, where an in-memory
  `active_subagent_requests` set is the only thing that stops a queued request from being
  executed again. The set is lost on daemon restart, and it is also cleared when
  completing a result back to the worker fails, so both a restart and a failed delivery
  restart the full timeout. Scaled to the issue: one restart inside a 1700-second wait
  pushes the answer past the harness's 1800-second abort. Confidence: the mechanism is
  confirmed in code and reproduced live; whether it is what happened in the maintainer's
  run is unknown.

- Observation: a residual discrepancy in the issue is still unexplained. The shim's own
  budget for a `wait` is `timeout_seconds + 60` (`WAIT_REPLY_GRACE` in
  `mj-worker/src/subagent_mcp.rs`), so the 1700-second call should have produced a tool
  error at about 1760 seconds, before the harness's 1800-second abort, and the maintainer
  saw nothing. Possible explanations, none ruled out: the reported numbers are
  approximate; the shim process was replaced mid-call by a worker or bridge restart; the
  response was written but the harness had already given up on that request id.
  Confidence: unknown. Milestone 4 adds the log line that would settle it. The design
  below does not depend on which explanation is right, because it stops relying on that
  backstop for correctness.

- Observation: the model sees the answer twice-encoded, which makes "still running" easy
  to misread. The shim returns the daemon's `SubagentToolResult` envelope as the tool's
  structured content, and the real payload is a JSON *string* inside its `message` field
  (see the transcripts above). The only signal that the children have not finished is
  `"timed_out": true` nested inside that string, and the word "timed out" reads like a
  failure rather than "ask again".

## Decision Log

- Decision: put the authoritative deadline in the worker's socket handler
  (`mj-worker/src/worker_runtime/subagents.rs`), not in the daemon and not in the shim.
  Rationale: it is the closest process to the caller that knows when the request arrived,
  it uses a monotonic clock in the same process that owns the request queue, and it is
  unaffected by daemon restarts, by failed result delivery, and by clock skew between a
  remote worker and the daemon host. The daemon keeping its own deadline remains useful
  (it stops pointless work) but is no longer what the model depends on.
  Date/Author: 2026-09-17, plan author.

- Decision (superseded): lower the advertised and effective `wait` ceiling to 600 seconds
  for every harness and teach the model to call `wait` again.
  Rationale at the time: the Codex ceiling was unknown, so a bounded call looked like the
  only thing reliable on both clients.
  Superseded by the maintainer on 2026-09-18: the ceiling stays at 3600 for every
  harness. Claude Code's limit is a limit on *silence*, and the protocol has a way to
  break silence, so the fix is to use it rather than to shorten everyone's waits. A cap
  is only justified where a harness is measured to enforce a total limit that progress
  does not reset, and then only for that harness.
  Date/Author: 2026-09-17 proposed, 2026-09-18 superseded.

- Decision: MCP progress notifications are the main fix. The server reports every 30
  seconds while a `wait` is open, and only when the client supplied a `progressToken` in
  the call's `_meta`, as the protocol requires.
  Rationale: Claude Code's watchdog resets on a progress notification (confirmed in the
  shipped client, quoted above), so a call that reports every half minute never reaches
  the 1800-second idle limit whatever the caller asked for. A client that did not ask for
  progress is sent none, so nothing is pushed at a client that would not expect it.
  Date/Author: 2026-09-18, maintainer's decision.

- Decision: any cap is per harness, measured, and computed from the harness kind the
  worker already knows.
  Rationale: a harness that enforces a *total* call timeout cannot be rescued by progress,
  and the honest response is to bound the wait for that harness only. The measurement is
  part of the live test: one real wait past 1900 seconds per harness, which is past the
  point where the reported failure occurred.
  Date/Author: 2026-09-18, maintainer's decision.

- Decision: a deadline reached is a *result*, never a tool error.
  Rationale: the issue's requirement is that the parent must not mistake a timeout for
  completion or for failure. `isError: true` invites the model to treat the children as
  broken. Every deadline answer will carry `"status": "still_running"` and an explicit
  `next_action`.
  Date/Author: 2026-09-17, plan author.

- Decision: `MAX_WAIT_SECONDS` keeps its value of 3600 and its two users. The shim, the
  worker and the daemon all resolve a caller's `timeout_seconds` through one function,
  `mj_core::subagent::subagent_wait_timeout`, so the three cannot disagree about when an
  answer is due.
  Rationale: the constant is also the cap of the HTTP endpoint behind `mj wait`
  (`mj-controller/src/server/api/wait.rs`, via the `MAX_WAIT_SECS` re-export in
  `mj-controller/src/server/api.rs:59`). One shared resolver is what was actually missing;
  three copies of a clamp is how the three processes came to disagree.
  Date/Author: 2026-09-18, plan author.

## Outcomes & Retrospective

To be written when the milestones land. The bar has two parts. A live run of the
Milestone 2 scenario answers a 45-second wait in about 45 seconds while the daemon is
restarted underneath it, where the unfixed build answers at about 61 seconds. And one real
wait of at least 1900 seconds returns its own answer on each harness, where today Claude
Code abandons the call at 1800 seconds.

## Context and Orientation

Read these files before editing; they are the whole surface of this change.

`mj-core/src/subagent.rs` holds the shared contracts. `MAX_WAIT_SECONDS = 3600` and
`DEFAULT_WAIT_SECONDS = 300` live here, as do `SubagentToolRequest` (`request_id`,
`created_at_ms`, `action`) and `SubagentToolResult` (`request_id`, `completed_at_ms`,
`is_error`, `message`). Both are serialized with `deny_unknown_fields` and are persisted
in the worker's queue file, so adding a field to either is a compatibility question (see
"Idempotence and Recovery").

`mj-worker/src/subagent_mcp.rs` is the MCP server the harness talks to. It runs as a
separate short-lived process, `hel worker subagent-mcp --socket <worker root>/subagents.sock`,
started by the harness. It defines the tool schemas (`tool_definitions`), converts a
`tools/call` into a `SubagentToolRequest`, writes it to the worker's Unix socket, and
waits for the answer. `reply_timeout` gives a `wait` call a budget of the requested
timeout plus `WAIT_REPLY_GRACE` (60 s) and everything else 120 s. If the budget expires
it returns `unanswered_reply`, which is a tool *error*.

`mj-worker/src/mcp_stdio.rs` is the JSON-RPC plumbing shared by the worker's three MCP
servers (project memory, review dispatch, sub-agents). `serve` reads request lines,
answers `initialize`, `ping`, `tools/list` and `tools/call`, and with
`Dispatch::Concurrent` runs each tool call on its own thread, writing responses under a
shared output mutex. It has no notion of notifications, `progressToken` or
`notifications/progress`. `socket_request` does the one-line request/one-line reply
exchange with the worker and bounds the read with `set_read_timeout`.

`mj-worker/src/worker_runtime/subagents.rs` is the worker side of that socket.
`serve_one` reads the request, calls `SubagentEndpoint::enqueue` (which persists it to
`subagents.json` in the worker root and returns a cached result if the same `request_id`
was already completed), then blocks in `await_result` until the daemon completes the
request or `SOCKET_WAIT_CEILING` (`MAX_WAIT_SECONDS + 60`) elapses, and writes the reply.

`mj-controller/src/server_runtime/run.rs` is the daemon's main loop. Around line 509 it
reads each worker snapshot's `subagent_requests`, skips ids already in the in-memory
`active_subagent_requests` set, and spawns a job that runs
`ApiBackend::execute_subagent_tool` and then completes the result back to the worker over
the session's relay connection.

`mj-controller/src/server_runtime/api.rs` implements the actions.
`SubagentToolAction::WaitAgents` computes a deadline from "now", then polls every 250 ms:
it loads each child's materialized summary, asks `start_status`, and decides whether every
child is finished. When they are finished, or at the deadline, it reads each child's last
finished turn message and returns
`{"agents":[{"child_session_id","state","output"}],"timed_out":bool}`.

`mj-controller/src/controller/worker_binary/staging.rs:89` (`configure_claude_subagent_mcp`)
writes the `mj-agents` entry into the session's staged `.claude.json`. This is the only
place where a Claude-specific per-server `timeout` could be set.

Terms used below. "The shim" is the `hel worker subagent-mcp` process. "The worker" is the
long-lived `hel worker run` process that owns the session. "The daemon" is the per-user
`mj` background process. "Long poll" means a call that blocks for a bounded time and
returns either the answer or "not yet, ask again".

## Plan of Work

The work is four ordered, separately testable commits. Each one is useful on its own.

**Milestone 1: one timeout rule, one unambiguous answer shape, and a deadline that counts
from the caller's request.** In `mj-core/src/subagent.rs`, keep `MAX_WAIT_SECONDS` at
3600 and add `pub fn subagent_wait_timeout(requested: Option<u64>) -> Duration`, which
defaults to `DEFAULT_WAIT_SECONDS` and clamps to `1..=MAX_WAIT_SECONDS`. The shim
(`reply_timeout`), the worker and the daemon all call it, so the three processes cannot
disagree about when an answer is due. Add
`pub fn remaining_subagent_wait(created_at_ms, requested, now_ms) -> Duration`, clamped
into `0..=requested` so clock skew between a remote worker and the daemon can neither
extend a wait nor make it negative, and use it in `mj-controller/src/server_runtime/api.rs`
in place of `Instant::now() + timeout`. A request executed again after a daemon restart
then answers at the caller's original deadline instead of starting over. Add one
`tracing::info!` when a wait starts (children, requested seconds, remaining budget) and
one when it answers (complete, waited seconds); with the existing warning on a failed
delivery, that is enough to settle the unexplained part of the issue if it recurs.

Still in Milestone 1, change what the model reads. In `api.rs`, replace the wait result
with

    {"status":"complete"|"still_running",
     "waited_seconds":<integer>,
     "agents":[{"child_session_id":"...","state":"...","finished":true|false,
                "output":<string or null>}],
     "next_action":"<one sentence>"}

where `next_action` for `still_running` names how many children are still running, says
that this is not a failure, and tells the caller to call `wait` again, and for `complete`
says the reports are in each agent's `output`. Drop `timed_out`; `status` carries the same
fact without reading as a failure. In `mj-worker/src/subagent_mcp.rs`, stop handing the
model the `SubagentToolResult` envelope: when `message` parses as a JSON object, return
that object as the tool's structured content (merging `request_id` into it so retry advice
still has an id), otherwise return the envelope as today. Update the `wait` tool
description and `SERVER_INSTRUCTIONS` to state the loop: the timeout is still up to 3600
seconds, and a child may run far longer than any single wait, so `still_running` means
call `wait` again.

**Milestone 2: the worker answers at the caller's deadline.** In
`mj-worker/src/worker_runtime/subagents.rs`, give `serve_one` its own deadline instead of
the blanket `SOCKET_WAIT_CEILING`. For a `WaitAgents` action the deadline is
`Instant::now() + subagent_wait_timeout(timeout_seconds) + WORKER_WAIT_GRACE` where
`WORKER_WAIT_GRACE` is 5 seconds; for every other action keep a bounded ceiling. When the
deadline passes with no daemon result, answer anyway, with the Milestone 1 shape and
`status: "still_running"`, listing the requested `child_session_ids` with
`"state":"unknown"` and a `note` saying Mjolnir did not answer in time and the call should
be repeated. Leave the request in the queue: the daemon will complete it later and the
cached result is harmless. In `mj-worker/src/subagent_mcp.rs`, make the shim's own
give-up answer the same still-running result with `is_error` false, instead of
`unanswered_reply`, and keep the request-id retry advice in the `note`. After this
milestone the model gets a correct, honest answer inside its deadline no matter what the
daemon does.

**Milestone 3: progress notifications, which is what makes a long wait survive.** In
`mj-worker/src/mcp_stdio.rs`, read `params._meta.progressToken` from a `tools/call`
request and pass a `Progress` handle to the call handler. The handle owns the token and a
closure that writes one JSON line under the existing output mutex:

    {"jsonrpc":"2.0","method":"notifications/progress",
     "params":{"progressToken":<token>,"progress":<seconds elapsed>,
               "total":<the call's timeout in seconds>,
               "message":"waiting for 2 child session(s) to finish their turn:
                          7e93…, ab12…; 120s elapsed"}}

With no token the handle does nothing, because MCP only allows progress for a request
whose caller asked for it. Add a `progress_interval: Duration` field to `McpServer`
(`PROGRESS_INTERVAL`, 30 seconds) so tests can shorten it; do not use an environment
variable. In `mj-worker/src/subagent_mcp.rs`, run the socket exchange on a helper thread
and loop on `mpsc::Receiver::recv_timeout(progress.interval())`, emitting one notification
per tick until the reply arrives. The message must be worth reading: it names the children
being waited on and how long the call has been open. The other two servers
(`mj-worker/src/memory_mcp.rs`, `mj-worker/src/review/mcp.rs`) answer at once and only need
their handler signatures updated; they report nothing.

**Milestone 4: measure each harness's real ceiling, and cap only where one exists.** The
first three milestones are the fix; this one establishes whether any harness still needs a
bound. Run one real wait per harness, Claude and Codex, with `timeout_seconds` at 3600 (or
at least 1900, which is past the point where the reported failure happened) against a child
that stays busy that long, and record when the call returns. If a harness returns the
answer at its own deadline, it needs no cap. If a harness aborts at a fixed total time that
progress did not reset, add a cap for that harness alone: the worker knows its own harness
kind in `mj_core::worker_launch`, so the shim can clamp `timeout_seconds` from it, and the
`wait` tool description must then state the real number for that harness. Record the
measured numbers in `Surprises & Discoveries` either way.

## Concrete Steps

Work from the repository root of your worktree. Build and test outside the sandbox.

Run the full test suite and lints after each milestone:

    cargo test
    cargo clippy --all-targets -- -D warnings

Both must be clean on the dev profile. A known flaky test unrelated to this work
(#1036, `codex_usage`, "Text file busy") can fail when other agents are building; rerun
that test alone before treating it as yours.

New or changed unit tests, all of which run in milliseconds:

- `mj-core/src/subagent.rs`: `a_wait_timeout_is_clamped_into_the_advertised_range`,
  `the_remaining_wait_counts_from_the_callers_request_and_survives_clock_skew`, and
  `the_still_running_answer_names_the_children_and_tells_the_model_to_ask_again`.
- `mj-worker/src/subagent_mcp.rs`: keep
  `wait_advertises_the_shared_runtime_timeout_limit` (the schema maximum stays
  `MAX_WAIT_SECONDS`); keep `wait_calls_get_their_own_timeout_plus_grace` against the
  shared resolver; keep `an_unanswered_call_becomes_a_tool_error_instead_of_hanging` for a
  non-wait action and add `an_unanswered_wait_is_a_still_running_answer_rather_than_a_failure`,
  which uses the fake worker that never answers, a 200 ms budget, and asserts `is_error` is
  false, `status` is `still_running`, and `next_action` tells the model to call wait again;
  add `the_models_answer_is_the_payload_itself_not_the_result_envelope`.
- `mj-worker/src/mcp_stdio.rs`:
  `a_call_with_a_progress_token_gets_progress_lines_before_its_response` (handler sleeps
  120 ms, `progress_interval` 20 ms, assert at least two `notifications/progress` lines
  carrying the client's token arrive before the response line, with the `total` the caller
  is working towards) and
  `a_call_without_a_progress_token_is_answered_with_no_notifications`.
- `mj-worker/src/worker_runtime/subagents.rs`: a `#[tokio::test(start_paused = true)]`
  that sends a `WaitAgents` request with `timeout_seconds: 45` over a real Unix socket,
  never completes it, and asserts the answer arrives between 45 and 55 seconds of virtual
  time with `status` `still_running` and the requested child ids listed.

Live test (this is the one that fails on the unfixed build). Everything runs in a private
instance named `fix1034` on port 4134; never touch another instance. The config is
prepared at `~/.config/mjolnir/instances/fix1034/config.toml` with a `localhost`
local-bare target; it holds an API key, so do not print or commit it.

1. Build everything, because a local-bare session runs `target/debug/mj-worker` and
   `cargo test` does not rebuild it:

       cargo build

2. In a tmux session named `fix1034`, run the TUI once so the instance has a workspace
   (`mj new` on a fresh instance otherwise fails with a generic 500, #1080):

       tmux new-session -d -s fix1034 -x 160 -y 45
       tmux send-keys -t fix1034 'export MJ_INSTANCE=fix1034 && \
         export MJ_WORKER_BINARY=$PWD/target/debug/mj-worker && ./target/debug/mj go' Enter

   Press Escape to dismiss the new-session dialog.

3. Create a parent session per harness under test:

       MJ_INSTANCE=fix1034 MJ_WORKER_BINARY=$PWD/target/debug/mj-worker \
         ./target/debug/mj new --profile deepseek --target localhost \
         --project-directory $PWD --title fix1034-codex

   and the same with `--profile claude2` for the Claude parent. Note each session id; a
   worker root is `~/.local/share/mjolnir/instances/fix1034/workers/<session id>` and the
   socket is `subagents.sock` inside it. Confirm the running worker is your build, for
   example `strings <worker root>/hel | grep still_running`; `cargo build --bin mj` and
   `cargo test` do not rebuild `target/debug/mj-worker`, so a stale worker is the usual
   reason a live test seems to prove nothing.

4. Deadline scenario, driven through the real MCP shim with the script in "Artifacts and
   Notes". Spawn a child that sleeps for 900 seconds, then start a `wait` with
   `timeout_seconds: 45` and, 15 seconds later, run
   `MJ_INSTANCE=fix1034 ./target/debug/mj daemon stop` followed by
   `MJ_INSTANCE=fix1034 ./target/debug/mj sessions` to bring the daemon back.

   On today's build the shim answers after about 61 seconds, because the restarted daemon
   began the 45-second wait again:

       [wait] answered after 61.0s

   After Milestones 1 and 2 it answers at about 45 to 50 seconds with
   `"status": "still_running"`. A run with no restart answers at about 45 seconds both
   before and after, which is the control.

5. Ceiling scenario, one per harness, through the harness itself rather than the script,
   because it is the harness's own MCP client that is under test. Spawn a child that
   stays busy for at least 2000 seconds, then prompt the parent to call `wait` once with
   `timeout_seconds` at 3600 (or at least 1900). Record when the call returns and what it
   returned. Passing means the call survives past 1800 seconds and comes back with
   `status: still_running` or `complete`, rather than with the harness's abort message.
   Read the emitted progress on the Claude side in its MCP log if you want to see the
   notifications arriving.

6. Clean up completely: close every session with `mj close --session <id> --force`, stop
   the daemon, kill the tmux session, check for survivors with
   `pgrep -af instances/fix1034`, and remove the instance's config and data directories.

## Validation and Acceptance

Acceptance is behavioural, in this order.

A `wait` with `timeout_seconds: 3600` runs to its own deadline on a real harness instead
of being abandoned at 1800 seconds. This is the issue's headline and the reason progress
notifications exist here. `tools/list` still advertises a maximum of 3600, so the schema
and reality agree — that is now true because the limit was removed, not because the
advertised number was lowered.

A `wait` on children that are still working returns within its own timeout, with
`"status": "still_running"` at the top level of the tool's structured content, each child
listed with `finished: false`, and a `next_action` sentence telling the model to call
`wait` again. It is not a tool error. This is what stops the parent from reading a timeout
as a failure.

A `wait` on children that have finished returns `"status": "complete"` with each child's
own report in `output`, unchanged from the behaviour that commit 529a5144 established
(the report is the child's last finished turn, bounded by that turn's span).

The deadline holds under interference: the live restart scenario above answers at about 45
to 50 seconds instead of about 61. Run it on the unfixed build first to see the failure.

While a call is in flight, a client that sent a `progressToken` receives
`notifications/progress` lines about every 30 seconds, each naming the children being
waited on and how long the call has been open, and a client that sent no token receives
none. Verify with the unit tests and, end to end, by sending a `tools/call` whose params
include `"_meta":{"progressToken":1}` through the shim script and watching the lines
arrive.

`cargo test` and `cargo clippy --all-targets -- -D warnings` pass on the dev profile.

## Idempotence and Recovery

All steps are repeatable. Re-running the live test creates new sessions in a private
instance and never touches the default instance. If the instance is left behind, the
cleanup in step 6 removes it entirely.

No database migration is involved; nothing in this change writes to `mj.sqlite3`.

No daemon protocol change is required, so `PROTOCOL_VERSION` in `mj-client/src/daemon.rs`
does not move. This holds because the plan adds no field to `SubagentToolRequest` or
`SubagentToolResult`: the daemon already receives `created_at_ms`, and everything else is a
change of *content* inside `SubagentToolResult.message`, which is an opaque string on the
wire. Keep it that way. Both structs are `deny_unknown_fields` and `SubagentToolRequest`
is persisted in the worker's `subagents.json`, so a new field would make a new worker's
queue file unreadable by an older worker after a downgrade.

The model-facing result shape does change. Nothing stores it; it is read once by the
parent model and shown in transcripts. Old transcripts keep their old shape, which is
expected.

If Milestone 2 is deployed without Milestone 1, a worker-generated deadline answer must
still parse on the old shim: keep it a JSON object inside `message`, which it is.

## Artifacts and Notes

The driver used for the measurements above, which speaks JSON-RPC to the real shim and
times each call. Save it outside the repository and do not commit it:

    import json, subprocess, sys, threading, time
    WORKER = "<worker root>/hel"
    SOCK = "<worker root>/subagents.sock"
    proc = subprocess.Popen([WORKER, "worker", "subagent-mcp", "--socket", SOCK],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
    responses, lock = {}, threading.Condition()
    def reader():
        for line in proc.stdout:
            if not line.strip():
                continue
            msg = json.loads(line)
            with lock:
                responses[msg.get("id")] = (time.time(), msg)
                lock.notify_all()
    threading.Thread(target=reader, daemon=True).start()
    seq = [0]
    def send(method, params):
        seq[0] += 1
        proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": seq[0],
            "method": method, "params": params}) + "\n")
        proc.stdin.flush()
        return seq[0], time.time()
    def wait_for(rid, budget):
        end = time.time() + budget
        with lock:
            while rid not in responses:
                if end - time.time() <= 0:
                    return None, None
                lock.wait(1.0)
            return responses.pop(rid)
    rid, _ = send("initialize", {"protocolVersion": "2025-03-26"})
    wait_for(rid, 30)
    rid, sent = send("tools/call", {"name": "wait", "arguments":
        {"child_session_ids": [sys.argv[1]], "timeout_seconds": int(sys.argv[2])}})
    got, msg = wait_for(rid, int(sys.argv[2]) + 300)
    print("answered after %.1fs" % (got - sent) if msg else "no response")
    print(json.dumps(msg, indent=2)[:2000])

The cost of calling `wait` again, for the record. Each extra call is one tool call and one
small result: roughly 300 to 600 tokens including the model's own framing. With the
ceiling left at 3600 seconds a three-hour child costs two or three extra calls; a parent
that keeps the 300-second default pays about 36. Nothing forces the loop to be expensive,
which is the argument for leaving the ceiling where the tool advertises it.

## Interfaces and Dependencies

In `mj-core/src/subagent.rs`, keep `MAX_WAIT_SECONDS = 3_600` and define:

    pub const WAIT_STATUS_COMPLETE: &str = "complete";
    pub const WAIT_STATUS_STILL_RUNNING: &str = "still_running";

    pub fn subagent_wait_timeout(requested: Option<u64>) -> std::time::Duration;

    pub fn remaining_subagent_wait(
        created_at_ms: i64,
        requested: Option<u64>,
        now_ms: i64,
    ) -> std::time::Duration;

    pub fn still_running_payload(
        child_session_ids: &[String],
        waited_seconds: u64,
        note: Option<&str>,
    ) -> serde_json::Value;

    pub fn next_action(complete: bool, unfinished: usize, total: usize) -> String;

In `mj-worker/src/mcp_stdio.rs`, extend the server description:

    pub struct McpServer<F> {
        pub name: &'static str,
        pub instructions: &'static str,
        pub tools: Vec<Value>,
        pub dispatch: Dispatch,
        pub progress_interval: Duration,
        pub call: F,
    }

    pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

    /// Writes `notifications/progress` for one in-flight call. Does nothing when
    /// the client sent no `_meta.progressToken`.
    pub struct Progress { /* token + interval + writer */ }

    impl Progress {
        pub fn interval(&self) -> Duration;
        pub fn notify(&self, elapsed: u64, total: Option<u64>, message: &str);
    }

with `F: Fn(Option<&Value>, &Progress) -> Result<(Value, bool)> + Send + Sync + 'static`
and `W: Write + Send + Sync + 'static`, because the writer is now shared with the
notification path as well as the response path.

In `mj-worker/src/worker_runtime/subagents.rs`, `serve_one` gains a per-action deadline and
a fallback answer builder:

    const WORKER_WAIT_GRACE: Duration = Duration::from_secs(5);

    fn wait_budget(action: &SubagentToolAction) -> Duration;
    fn waiting_children(action: &SubagentToolAction) -> Option<Vec<String>>;
    fn late_daemon_reply(request_id: &str, child_session_ids: &[String], waited_seconds: u64)
        -> SubagentToolResult;

In `mj-controller/src/server_runtime/api.rs`, `execute_subagent_tool` takes the request's
`created_at_ms` and the `WaitAgents` arm builds its deadline with
`remaining_subagent_wait`, returning the `status` / `waited_seconds` / `agents` /
`next_action` shape described above.

## Decisions the maintainer has taken

The ceiling stays at 3600 seconds for every harness. Progress notifications remove
Claude Code's idle limit rather than every parent paying for it, and a cap is added only
where a harness is measured to enforce a total limit that progress does not reset,
computed from the harness kind rather than applied to all.

The result shape changes: `status` and `next_action` replace `timed_out`, and the
`SubagentToolResult` envelope is unwrapped so the payload is the tool's structured
content. This is model-facing only and nothing persists it.

The staged Claude config does not carry a per-server `timeout`
(`configure_claude_subagent_mcp` is left alone). Progress makes it unnecessary, and a
Claude-only knob would hide the difference rather than fix it.

`wait` still does not take a `request_key`. A wait that always answers makes restarting
the wait the simpler contract, and a stale key would return a stale snapshot of the
children.

---

Revision note, 2026-09-18: revised after the maintainer rejected the 600-second cap. The
previous version treated Claude Code's idle timer as a constraint every harness had to be
shortened to fit, which is backwards: the protocol has a way to break silence, and the
client resets its timer on it. Progress notifications are now the main fix, the advertised
3600-second ceiling stays for every harness, and a cap is only added where a harness is
measured to enforce a total limit that progress does not reset. The worker-authoritative
deadline, the result shape and the log line are unchanged from the first version; the
milestones were reordered so the daemon's deadline lands with the shared timeout rule, and
the live test now includes one real wait past 1900 seconds per harness. Nothing in
`Surprises & Discoveries` changed: the evidence is the reason the decision could be taken.

---

Revision note: initial version, 2026-09-17. Written after reading issue #1034 and its
follow-up, extracting the timeout rules from the shipped Claude Code, Codex and Kimi
clients, measuring a 150-second Codex tool call live, and reproducing the server's missed
deadline at 45-second scale in a private instance. The design changed once during writing:
an earlier draft put the authoritative deadline in the daemon, which the reproduction
showed cannot hold across a daemon restart, so it moved into the worker.
