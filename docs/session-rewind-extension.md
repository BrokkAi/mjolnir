# Session Rewind Extension Plan

Issue #213 asked whether `mjolnir` should support rewinding an ACP session to an
earlier point in time. ACP 0.14 has no standard `session/rewind` method, but the
Rust SDK exposes `_meta` on `session/fork`, `session/load`, and `session/resume`.
This note records the proposed experiment so `mjolnir` and Anvil can evolve the
same behavior without inventing a client-only UX.

## Recommendation

Model rewind as a fork from a checkpoint, not as mutation of the current
session.

The first experimental path should be:

1. Agent advertises a namespaced rewind capability in `InitializeResponse`
   metadata.
2. Client asks the agent to fork the current session with a rewind target in the
   `session/fork` request `_meta`.
3. Agent returns a new session id whose transcript and execution context start
   at the selected checkpoint.
4. `mjolnir` switches to the forked session using the same state transition path
   as ordinary `session/fork`.

This keeps the original session intact, matches the existing user mental model
for `/fork`, and avoids pretending tool side effects can be undone in-place.

## Extension Shape

Use a Brokk-owned key while the behavior is experimental:

```json
{
  "_meta": {
    "ai.brokk.sessionRewind": {
      "version": 1,
      "target": {
        "kind": "checkpoint",
        "id": "agent-checkpoint-id"
      }
    }
  }
}
```

The same object can be carried by `ForkSessionRequest::meta(...)`. Avoid
metadata on `session/load` or `session/resume` for the first experiment because
those methods imply returning to an existing session, not creating a new branch.

The capability advertisement can use initialize metadata:

```json
{
  "_meta": {
    "ai.brokk.sessionRewind": {
      "version": 1,
      "supportsFork": true,
      "targetKinds": ["checkpoint"]
    }
  }
}
```

If the extension graduates into ACP proper, the key can move to a standard
capability and method name.

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
  current disk, or requires a worktree/snapshot integration.
- Tool side effects: external side effects are not undone by the client.
- Terminal state: existing terminal processes should not be inherited into the
  rewound fork.
- Permissions: permission history should not be replayed as approvals for new
  tool calls.
- Config: the fork should return session config options and current values in
  the `ForkSessionResponse`, as ordinary `session/fork` does.

## Implementation Stages

1. Add Anvil-side checkpoint listing and `session/fork` `_meta` handling.
2. Add a small `mjolnir` parser for `ai.brokk.sessionRewind` initialize metadata.
3. Add `UiCommand::RewindSession { checkpoint_id }` and wire it to a fork
   request with the rewind metadata.
4. Reuse the existing fork transition, including stale-session permission and
   terminal cleanup behavior.
5. Add tests covering unsupported agents, successful fork-from-checkpoint, fork
   failure, and checkpoint picker cancellation.

## Open Questions

- Whether checkpoint listing should be a new extension method or another
  metadata payload on existing session listing.
- Whether Anvil can provide filesystem snapshots, or whether rewind should
  require `mj --worktree` for stronger isolation.
- Whether ACP should standardize checkpoint ids and labels before standardizing
  a rewind method.
