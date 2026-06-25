# Session Rewind Extension Plan

Issue #213 asked whether `mjolnir` should support rewinding an ACP session to an
earlier point in time. ACP 0.14 has no standard `session/rewind` method, but the
Rust SDK exposes `_meta` on `session/fork`, `session/load`, and `session/resume`.
This note records the proposed experiment so `mjolnir` and Anvil, as the first
reference implementation, can evolve the same behavior without inventing a
client-only UX. `mjolnir` support must be gated only on advertised protocol
metadata, never on the agent binary name.

## Recommendation

Model rewind as a fork from a checkpoint, not as mutation of the current
session.

The first experimental path should be:

1. Agent advertises a namespaced rewind capability in
   `InitializeResponse.agentCapabilities._meta.symposium`.
2. Client asks the agent to fork the current session with a rewind target in the
   `session/fork` request `_meta`.
3. Agent returns a new session id whose transcript and execution context start
   at the selected checkpoint.
4. `mjolnir` switches to the forked session using a rewind-specific
   fork-plus-cleanup transition.

This keeps the original session intact, matches the existing user mental model
for `/fork`, and avoids pretending tool side effects can be undone in-place.

## Extension Shape

Use a Brokk-owned key while the behavior is experimental. A `session/fork`
request should carry the extension payload directly in request `_meta`:

```json
{
  "_meta": {
    "ai.brokk.sessionRewind": {
      "version": 1,
      "expectedSourceSessionId": "current-session-id",
      "target": {
        "kind": "checkpoint",
        "id": "agent-checkpoint-id"
      }
    }
  }
}
```

With the Rust SDK, `ForkSessionRequest::meta(...)` receives the metadata map that
serializes as `_meta`; do not wrap that argument in another `_meta` object. The
agent must reject the request if `expectedSourceSessionId`, the fork request's
source `sessionId`, the checkpoint owner, workspace, cwd, or current user context
do not match. Checkpoint ids should be opaque, unforgeable, and session-scoped.

Avoid metadata on `session/load` or `session/resume` for the first experiment
because those methods imply returning to an existing session, not creating a new
branch.

The capability advertisement should use the existing ACP meta-capability
location, and must also require ordinary `session/fork` support:

```json
{
  "agentCapabilities": {
    "sessionCapabilities": {
      "fork": {}
    },
    "_meta": {
      "symposium": {
        "version": "1.0",
        "ai.brokk.sessionRewind": {
          "version": 1,
          "supportsFork": true,
          "targetKinds": ["checkpoint"],
          "checkpointListing": {
            "method": "ai.brokk/sessionRewindCheckpoints",
            "pageSizeLimit": 50
          },
          "guarantees": {
            "filesystem": "agentSnapshot | currentDisk | worktreeRequired",
            "terminals": "notRestored",
            "externalSideEffects": "notUndone",
            "permissions": "freshApprovalRequired"
          }
        }
      }
    }
  }
}
```

`mjolnir` should enable `/rewind` only when `sessionCapabilities.fork` is
present, `version == 1`, `supportsFork == true`, `targetKinds` includes
`checkpoint`, and `checkpointListing.method` is supported. Unknown versions,
malformed metadata, or partial advertisements must disable the command and emit
debug logs explaining the decision.

If the extension graduates into ACP proper, the key can move to a standard
capability and method name.

## Checkpoint Discovery

Checkpoint discovery should use the advertised
`ai.brokk/sessionRewindCheckpoints` method until ACP standardizes this surface.
The UI must not issue ad hoc JSON-RPC calls directly; it should request typed
checkpoint data through an ACP-runtime command such as
`UiCommand::ListRewindCheckpoints { responder }`.

The listing contract should be bounded:

- Request includes the active `sessionId`, optional cursor, and page size.
- Response returns checkpoint id, label, creation time, optional warning text,
  guarantee overrides, and next cursor.
- Agent enforces `pageSizeLimit`; client uses timeout and cancellation behavior
  matching other session operations.
- Picker labels and warnings must wrap or scroll rather than truncate critical
  content.

## Target Identity

Prefer agent-defined checkpoint ids over transcript indexes, timestamps, or
message ids.

Checkpoint ids let the agent decide what is actually replayable. A transcript
row is only a rendering artifact; it may not map cleanly to model context,
filesystem state, terminal state, or tool side effects. Timestamps are also too
imprecise and race-prone.

The client should display checkpoints using agent-provided labels such as:

- turn title or first prompt line
- creation time
- short checkpoint id
- optional warning text when filesystem or terminal state cannot be restored

## UX

Initial UI should be conservative:

- Gate the command behind advertised support.
- Add a `/rewind` command only when the extension is present.
- Open a picker of agent-provided checkpoints.
- Label the action as "fork from checkpoint" in confirmation/status text.
- After success, show the new session title and keep the source session
  available through `/load`.

Unsupported agents should behave like unsupported `/fork`: keep the command out
of autocomplete unless advertised, and surface a short warning if invoked through
remote or stale UI paths.

## Semantics

The extension must not promise impossible undo behavior.

- Filesystem effects: agent must describe whether it restores files, starts from
  current disk, or requires a worktree/snapshot integration. If stronger
  isolation is needed, prefer the existing `mj --worktree` parallel-workspace
  flow documented in the README before adding a separate client-side snapshot
  mechanism.
- Tool side effects: external side effects are not undone by the client.
- Terminal state: existing terminal processes should not be inherited into the
  rewound fork. Rewind must either shut down source-session terminals or keep
  them visibly attached to the source session with a warning that they continue
  running.
- Permissions: permission history should not be replayed as approvals for new
  tool calls. While a rewind fork is in flight, permission requests for the
  source session should be cancelled or queued so the user cannot approve a
  destructive action while believing the session is rewinding.
- Config: the fork should return session config options and current values in
  the `ForkSessionResponse`, as ordinary `session/fork` does.

## Implementation Stages

1. Add Anvil-side `ai.brokk/sessionRewindCheckpoints` listing and `session/fork`
   `_meta` handling as the first reference implementation.
2. Add a small `mjolnir` parser for `ai.brokk.sessionRewind` initialize metadata.
3. Add `UiCommand::ListRewindCheckpoints { responder }` and keep all agent I/O
   in the ACP runtime.
4. Add `UiCommand::RewindSession { checkpoint_id }` and wire it to a fork
   request with the rewind metadata.
5. Add a fork-plus-cleanup transition for rewind: cancel or guard source-session
   permission prompts, handle source-session terminals explicitly, log cleanup
   failures, then switch to the forked session.
6. Add tests covering unsupported agents, malformed/unknown metadata, mock-agent
   checkpoint listing, successful fork-from-checkpoint, fork failure, permission
   requests racing with rewind, live terminal cleanup, and checkpoint picker
   cancellation.

## Open Questions

- Whether the experimental checkpoint listing method should eventually move into
  ACP proper or become metadata on existing session listing.
- Whether Anvil can provide filesystem snapshots, or whether rewind should
  require the existing `mj --worktree` flow for stronger isolation.
- Whether ACP should standardize checkpoint ids and labels before standardizing
  a rewind method.
