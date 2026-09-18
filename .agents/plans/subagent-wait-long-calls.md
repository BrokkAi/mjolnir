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
failure. Every `wait` call is answered within the time the caller asked for, by the
process nearest the caller, and the answer says plainly either "these children are
finished, here is each one's report" or "still running, call `wait` again". The advertised
limit equals the limit every supported harness honours, and while a call is in flight the
server sends MCP progress notifications so a harness that watches for silence sees
activity. You can see it working with a scaled-down live test (45-second wait, daemon
restarted underneath it) that fails on today's build and passes after the change.

## Progress

- [x] (2026-09-17) Read issue #1034 and its follow-up comment; established the three
      distinct defects with evidence (see `Surprises & Discoveries`).
- [x] (2026-09-17) Measured the harness limits from the shipped client binaries and from
      a live Codex session.
- [x] (2026-09-17) Reproduced the server missing its own deadline live at 45-second
      scale in a private instance.
- [ ] Milestone 1: one shared timeout rule and an unambiguous result shape.
- [ ] Milestone 2: the worker answers at the caller's deadline even when the daemon does
      not.
- [ ] Milestone 3: MCP progress notifications while a call is in flight.
- [ ] Milestone 4: the daemon's own wait deadline survives a restart, plus one log line
      per wait.

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

- Decision: lower the advertised and effective `wait` ceiling to a value every supported
  harness honours, and teach the model to call `wait` again, instead of trying to make
  one call survive for an hour.
  Rationale: progress notifications fix Claude Code (confirmed) but the Codex ceiling is
  unknown and unreachable over ACP, so a single long call cannot be made reliable across
  both clients. A bounded call that always returns is reliable on any client whose
  ceiling is above the cap.
  Date/Author: 2026-09-17, plan author.

- Decision: add progress notifications anyway, as defence in depth rather than as the
  fix.
  Rationale: they are cheap, they are ignored by clients that do not ask for them, and
  they keep a call alive if a future cap is raised or a call overruns.
  Date/Author: 2026-09-17, plan author.

- Decision: a deadline reached is a *result*, never a tool error.
  Rationale: the issue's requirement is that the parent must not mistake a timeout for
  completion or for failure. `isError: true` invites the model to treat the children as
  broken. Every deadline answer will carry `"status": "still_running"` and an explicit
  `next_action`.
  Date/Author: 2026-09-17, plan author.

- Decision: introduce a separate constant for the sub-agent tool ceiling instead of
  lowering `MAX_WAIT_SECONDS`.
  Rationale: `MAX_WAIT_SECONDS` is also the cap of the HTTP endpoint used by `mj wait`
  (`mj-controller/src/server/api/wait.rs`, via the `MAX_WAIT_SECS` re-export in
  `mj-controller/src/server/api.rs:59`). That surface has no MCP client and no 1800-second
  limit, and lowering it would be an unrelated regression.
  Date/Author: 2026-09-17, plan author.

## Outcomes & Retrospective

To be written when the milestones land. The bar: a live run of the Milestone 2 test
answers a 45-second wait in about 45 seconds while the daemon is restarted underneath it,
and the same run on the unfixed build answers at about 61 seconds.

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

**Milestone 1: one timeout rule and one unambiguous answer shape.** In
`mj-core/src/subagent.rs`, add `pub const MAX_SUBAGENT_WAIT_SECONDS: u64 = 600;` with a
comment stating why the number is what it is (Claude Code aborts a silent stdio call at
1800 s; Codex's ceiling is unknown but measured above 150 s; the cap must leave room for
grace on top). Add `pub fn subagent_wait_timeout(requested: Option<u64>) -> Duration`,
which clamps to `1..=MAX_SUBAGENT_WAIT_SECONDS` and defaults to `DEFAULT_WAIT_SECONDS`,
and make the shim (`reply_timeout`) and the daemon (`api.rs`) both call it so the three
processes cannot disagree. Leave `MAX_WAIT_SECONDS` and the HTTP endpoint alone.

Still in Milestone 1, change what the model reads. In `api.rs`, replace the wait result
with

    {"status":"complete"|"still_running",
     "waited_seconds":<integer>,
     "agents":[{"child_session_id":"...","state":"...","finished":true|false,
                "output":<string or null>}],
     "next_action":"<one sentence>"}

where `next_action` for `still_running` is "Two of three children are still running. Call
wait again with the same child_session_ids to keep waiting." and for `complete` is "All
children finished; their reports are in output." Drop `timed_out`; `status` carries the
same fact without reading as a failure. In `mj-worker/src/subagent_mcp.rs`, stop handing
the model the `SubagentToolResult` envelope: when `message` parses as a JSON object,
return that object as the tool's structured content (merging `request_id` into it so retry
advice still has an id), otherwise return the envelope as today. Update the `wait` tool
description and `SERVER_INSTRUCTIONS` to state the ceiling and the loop: "wait blocks
until the named children finish their current turn or until timeout_seconds, whichever
comes first. timeout_seconds defaults to 300 and is capped at 600. If the answer says
status still_running, call wait again with the same child_session_ids; a child can run far
longer than one wait call."

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

**Milestone 3: progress notifications.** In `mj-worker/src/mcp_stdio.rs`, read
`params._meta.progressToken` from a `tools/call` request and pass a progress handle to the
call handler. The handle owns the token and a closure that writes one JSON line under the
existing output mutex:

    {"jsonrpc":"2.0","method":"notifications/progress",
     "params":{"progressToken":<token>,"progress":<seconds elapsed>,
               "message":"waiting for child sessions"}}

With no token, the handle does nothing. Add a `progress_interval: Duration` field to
`McpServer` (default 30 seconds) so tests can shorten it; do not use an environment
variable. In `mj-worker/src/subagent_mcp.rs`, run the socket exchange on a helper thread
and loop on `mpsc::Receiver::recv_timeout(progress_interval)`, emitting one progress
notification per tick until the reply arrives. The other two servers
(`mj-worker/src/memory_mcp.rs`, `mj-worker/src/review/mcp.rs`) only need their handler
signatures updated; they emit nothing.

**Milestone 4: the daemon stops restarting its own clock, and says what it did.** In
`mj-controller/src/server_runtime/api.rs`, compute the wait deadline from the request's
`created_at_ms` instead of from "now": add
`pub fn remaining_subagent_wait(created_at_ms: i64, requested: Option<u64>, now_ms: i64) -> Duration`
to `mj-core/src/subagent.rs`, clamped to `0..=MAX_SUBAGENT_WAIT_SECONDS` so that a clock
skew between a remote worker and the daemon can neither extend a wait nor make it negative,
and pass `created_at_ms` through `execute_subagent_tool`. A re-executed request then
answers at, or immediately after, the caller's original deadline rather than starting over.
Add one `tracing::info!` per wait recording `request_id`, `parent_session_id`, requested
timeout, computed remaining budget and whether the completion was delivered, and keep the
existing warning when delivery fails. This is what would settle the unexplained part of
the issue if it recurs.

## Concrete Steps

Work from the repository root of your worktree. Build and test outside the sandbox.

Run the full test suite and lints after each milestone:

    cargo test
    cargo clippy --all-targets -- -D warnings

Both must be clean on the dev profile. A known flaky test unrelated to this work
(#1036, `codex_usage`, "Text file busy") can fail when other agents are building; rerun
that test alone before treating it as yours.

New or changed unit tests, all of which run in milliseconds:

- `mj-core/src/subagent.rs`: `subagent_wait_timeout_clamps_to_the_advertised_ceiling`
  and `remaining_subagent_wait_survives_clock_skew_in_both_directions`.
- `mj-worker/src/subagent_mcp.rs`: change
  `wait_advertises_the_shared_runtime_timeout_limit` to assert the schema maximum equals
  `MAX_SUBAGENT_WAIT_SECONDS`; change `wait_calls_get_their_own_timeout_plus_grace` to the
  new clamp; replace `an_unanswered_call_becomes_a_tool_error_instead_of_hanging` with
  `an_unanswered_wait_becomes_a_still_running_result_the_model_can_retry`, which uses the
  existing fake worker that never answers, a 200 ms budget, and asserts `is_error` is
  false, `status` is `still_running`, and `next_action` tells the model to call wait again.
- `mj-worker/src/mcp_stdio.rs`:
  `a_call_with_a_progress_token_gets_progress_lines_before_its_response` (handler sleeps
  300 ms, `progress_interval` 50 ms, assert at least two `notifications/progress` lines
  carrying the client's token appear before the response line and that the response still
  parses) and `a_call_without_a_progress_token_gets_no_notifications`.
- `mj-worker/src/worker_runtime/subagents.rs`: a `#[tokio::test(start_paused = true)]`
  that enqueues a `WaitAgents` request with `timeout_seconds: 45`, never completes it,
  advances time, and asserts the socket answer arrives at 50 seconds of virtual time with
  `status` `still_running` and the requested child ids listed.
- `mj-controller/src/server_runtime/api/tests.rs`: a unit test of
  `remaining_subagent_wait` covering a request created 40 seconds ago with a 45-second
  timeout yielding about 5 seconds, and one created 10 minutes ago yielding zero.

Live test (this is the one that fails on the unfixed build). Everything runs in a private
instance named `plan1034` on port 4134; never touch another instance.

1. Build everything, because a local-bare session runs `target/debug/mj-worker` and
   `cargo test` does not rebuild it:

       cargo build

2. Create the instance config by copying an existing one and pointing it at port 4134 and
   a local target. The source file contains an API key; do not print or commit it:

       mkdir -p ~/.config/mjolnir/instances/plan1034
       cp ~/.config/mjolnir/instances/campaign0916/config.toml \
          ~/.config/mjolnir/instances/plan1034/config.toml
       # edit: [phone] bind = "127.0.0.1:4134"; add [targets.localhost] kind = "local-bare"

3. In a tmux session named `plan1034`, run the TUI once so the instance has a workspace
   (`mj new` on a fresh instance otherwise fails with a generic 500, #1080):

       tmux new-session -d -s plan1034 -x 160 -y 45
       tmux send-keys -t plan1034 'export MJ_INSTANCE=plan1034 && \
         export MJ_WORKER_BINARY=$PWD/target/debug/mj-worker && ./target/debug/mj go' Enter

   Press Escape to dismiss the new-session dialog.

4. Create a parent session and a long-running child:

       MJ_INSTANCE=plan1034 MJ_WORKER_BINARY=$PWD/target/debug/mj-worker \
         ./target/debug/mj new --profile deepseek --target localhost \
         --project-directory $PWD --title plan1034-parent

   Note the session id; the worker root is
   `~/.local/share/mjolnir/instances/plan1034/workers/<session id>` and the socket is
   `subagents.sock` inside it. Confirm the running worker is your build, for example
   `strings <worker root>/hel | grep still_running`.

5. Drive the real MCP shim with the script in "Artifacts and Notes". Spawn a child that
   sleeps for 900 seconds, then run the restart scenario: start a `wait` with
   `timeout_seconds: 45`, and 15 seconds later run
   `MJ_INSTANCE=plan1034 ./target/debug/mj daemon stop` followed by
   `MJ_INSTANCE=plan1034 ./target/debug/mj sessions` to bring the daemon back.

   On today's build the shim answers after about 61 seconds:

       [wait] answered after 61.0s

   After Milestone 2 it answers at about 50 seconds (45 plus the worker's 5-second grace)
   with `"status": "still_running"`, and after Milestone 4 the daemon's own answer also
   lands at about 45 seconds. A run with no restart answers at about 45 seconds both
   before and after, which is the control.

6. Clean up completely: close both sessions with `mj close --session <id> --force`, stop
   the daemon, kill the tmux session, check for survivors with
   `pgrep -af instances/plan1034`, and remove
   `~/.config/mjolnir/instances/plan1034` and `~/.local/share/mjolnir/instances/plan1034`.

## Validation and Acceptance

Acceptance is behavioural, in this order.

A `wait` call with `timeout_seconds` above the ceiling is accepted and answered at the
ceiling, and `tools/list` advertises that same ceiling, so the schema and reality agree.
Check by calling `tools/list` through the shim and reading
`inputSchema.properties.timeout_seconds.maximum`.

A `wait` on children that are still working returns within its own timeout, with
`"status": "still_running"` at the top level of the tool's structured content, each child
listed with `finished: false`, and a `next_action` sentence telling the model to call
`wait` again. It is not a tool error. This is what stops the parent from reading a timeout
as a failure.

A `wait` on children that have finished returns `"status": "complete"` with each child's
own report in `output`, unchanged from the behaviour that commit 529a5144 established
(the report is the child's last finished turn, bounded by that turn's span).

The deadline holds under interference: the live restart scenario above answers at about 50
seconds instead of about 61. Run it on the unfixed build first to see the failure.

While a call is in flight, a client that sent a `progressToken` receives
`notifications/progress` lines about every 30 seconds. Verify with the unit test and, if
you want to see it end to end, by sending a `tools/call` whose params include
`"_meta":{"progressToken":1}` through the shim script and watching the emitted lines.

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

The cost of the long-poll loop, for the record. Each extra `wait` call is one tool call and
one small result: roughly 300 to 600 tokens including the model's own framing. At a
600-second ceiling a three-hour child costs about 18 extra calls, near 10000 tokens; at a
300-second ceiling it costs twice that. That is the price of never losing a result, and it
is why the ceiling should be as high as the clients allow rather than as low as possible.

## Interfaces and Dependencies

In `mj-core/src/subagent.rs`, define:

    /// Longest a single `mj-agents wait` call may block. Claude Code aborts a
    /// silent stdio MCP call after 1800 s; Codex's ceiling is unknown but is
    /// above 150 s by measurement. The cap must stay far enough below both that
    /// the worker's and shim's grace still fit underneath them.
    pub const MAX_SUBAGENT_WAIT_SECONDS: u64 = 600;

    pub fn subagent_wait_timeout(requested: Option<u64>) -> std::time::Duration;

    pub fn remaining_subagent_wait(
        created_at_ms: i64,
        requested: Option<u64>,
        now_ms: i64,
    ) -> std::time::Duration;

In `mj-worker/src/mcp_stdio.rs`, extend the server description:

    pub struct McpServer<F> {
        pub name: &'static str,
        pub instructions: &'static str,
        pub tools: Vec<Value>,
        pub dispatch: Dispatch,
        pub progress_interval: Duration,
        pub call: F,
    }

    /// Writes `notifications/progress` for one in-flight call. Does nothing when
    /// the client sent no `_meta.progressToken`.
    pub struct Progress { /* token + writer */ }

    impl Progress {
        pub fn notify(&self, elapsed: Duration, message: &str);
    }

with `F: Fn(Option<&Value>, &Progress) -> Result<(Value, bool)> + Send + Sync + 'static`.

In `mj-worker/src/worker_runtime/subagents.rs`, `serve_one` gains a per-action deadline and
a fallback answer builder:

    const WORKER_WAIT_GRACE: Duration = Duration::from_secs(5);

    fn still_running_result(request_id: &str, child_session_ids: &[String], note: &str)
        -> SubagentToolResult;

In `mj-controller/src/server_runtime/api.rs`, `execute_subagent_tool` takes the request's
`created_at_ms` and the `WaitAgents` arm builds its deadline with
`remaining_subagent_wait`, returning the `status` / `waited_seconds` / `agents` /
`next_action` shape described above.

## Open questions for the maintainer

1. The ceiling. 600 seconds is the recommendation: comfortably under Claude Code's 1800,
   above the 150 seconds measured on Codex, and it halves the number of re-calls compared
   with today's 300-second default. 300 is the conservative choice (it is what works today
   in practice), 900 is the aggressive one. Recommendation: 600, with the Codex ceiling
   verified once during the live test by a single `timeout_seconds: 600` call.

2. The result shape. Replacing `timed_out` with `status` / `next_action`, and unwrapping
   the `SubagentToolResult` envelope so the payload is the tool's structured content,
   changes what every `mj-agents` tool returns to the model. Recommendation: do it; it is
   model-facing only, nothing persists it, and the double encoding is a direct cause of
   "timeout read as failure".

3. Should the staged Claude config also carry a per-server `timeout`
   (`configure_claude_subagent_mcp`)? It would raise Claude's idle window, but only for
   Claude, and it makes behaviour differ between the two harnesses. Recommendation: no.
   Keep one cap that holds everywhere; revisit if the ceiling ever needs to exceed 1800.

4. Is "do only part" acceptable? Milestones 1 and 2 alone remove the reported failure;
   3 and 4 are hardening. Recommendation: land all four, but if time is short, stop after
   2 and keep 4's log line, because without it the unexplained part of the issue stays
   unexplained.

5. Should `wait` keep accepting an explicit `request_key` for idempotency, so a repeated
   wait after a client abort can collect the original call's answer instead of starting a
   new one? `spawn` already has this; `wait` passes `None`. Recommendation: no, not now.
   With a bounded wait that always answers, restarting the wait is the simpler contract,
   and a stale key would return a stale snapshot of the children.

---

Revision note: initial version, 2026-09-17. Written after reading issue #1034 and its
follow-up, extracting the timeout rules from the shipped Claude Code, Codex and Kimi
clients, measuring a 150-second Codex tool call live, and reproducing the server's missed
deadline at 45-second scale in a private instance. The design changed once during writing:
an earlier draft put the authoritative deadline in the daemon, which the reproduction
showed cannot hold across a daemon restart, so it moved into the worker.
