---
title: HTTP API reference
description: Drive Mjolnir sessions from another program with the documented /api/v1 routes, bearer-token auth, and the mj subcommands built on them.
---

Mjolnir's daemon serves a documented HTTP API beside the web viewer. It exists
so another program — an orchestrating agent, a script, a CI job — can run a
session as a subagent: start it with a prompt, block until the turn ends, read a
structured outcome, send the next prompt, page the transcript, and take the work
out as a diff, a file, a pushed branch, or a git bundle.

The `mj` subcommands in the [CLI reference](/cli-reference/#drive-a-session-from-another-agent)
are thin clients for these routes. Use them when you can; use the routes
directly when your caller is not a shell.

The web viewer's own `/api/...` routes are a different thing: they serve the
browser, they are cookie-only, and they change whenever the browser needs
something new. Only `/api/v1/...` is documented and stable.

## Find the URL and the token

```console
mj api-info
```

It prints the base URL, the file holding the bearer token, and the contract
version:

```text
base url   http://127.0.0.1:3765/api/v1
token file /home/you/.local/share/mjolnir/api-token
version    Mjolnir API 1
```

The API is served by the web viewer's listener, so it follows the `[phone]`
section of `config.toml`. By default that is a loopback address reachable only
from this machine. When Tailscale detection succeeds, the same routes are also
reachable over your tailnet at the `https://…ts.net:PORT` address the viewer
advertises. The API is not served when the viewer is disabled; `mj api-info`
then says so, and `mj daemon status` reports the viewer's state.

## Authenticate

Send the token as a bearer token:

```console
curl -sS -H "Authorization: Bearer $(cat ~/.local/share/mjolnir/api-token)" \
  http://127.0.0.1:3765/api/v1/sessions
```

- The token file is created with mode 0600 when the viewer first starts, and it
  survives daemon restarts.
- Delete the file and restart the daemon to rotate the token. The old token
  stops working immediately; nothing else changes.
- A browser already signed in to the viewer may call these routes with its
  viewer session cookie instead, which is how a page served by the viewer can
  reach them without a second secret.

Without either credential every route answers `401` with the usual error body.

## Version header

Every response, `401`s included, carries:

```text
Mj-Api-Version: 1
Cache-Control: no-store
```

Check the header before parsing a body. A response without it did not come from
this API; a response naming another major version speaks a contract your client
does not know. `mj` refuses both.

## Errors

A failure is a JSON object with one field:

```json
{ "error": "this session cannot take a prompt right now" }
```

| Status | Meaning |
| --- | --- |
| `400` | The request was malformed: an empty prompt, an over-long idempotency key, a timeout outside 1–3600 seconds, a file path that is absolute or contains `..`. |
| `401` | No bearer token and no valid viewer cookie. |
| `404` | No such session, or no transcript recorded for it. |
| `409` | The session cannot do this now: no prompt capability, no live target, a turn still running, no commits to bundle, no recorded base for a diff, no push remote configured. |
| `500` | The operation was attempted and failed. The message says what failed. |
| `503` | The daemon is shutting down, or the controller is not accepting actions. |

Messages are written for you, the owner of the daemon, and they name real
profiles, targets, and git errors. Treat them as diagnostics, not as strings to
match on.

## Routes

### List sessions

```text
GET /api/v1/sessions
```

```json
{
  "sessions": [
    {
      "id": "session-1",
      "workspace_id": "workspace-1",
      "title": "add a README line",
      "harness_kind": "claude",
      "profile_id": "profile-1",
      "target_id": "target-1",
      "bundle_id": "bundle-1",
      "state": "Running",
      "lifecycle": "live",
      "chat_phase": "idle",
      "is_idle": true,
      "has_error": false,
      "created_at": "2026-09-11T07:00:00Z",
      "updated_at": "2026-09-11T07:04:21Z"
    }
  ]
}
```

`lifecycle` is one of `live`, `starting`, `stopping`, `stopped`, `failed`.
`chat_phase` is one of `idle`, `running`, `closing`, `closed`.

### Get one session

```text
GET /api/v1/sessions/{session_id}
```

The same object, plus `last_turn_outcome` when a prompt has finished on it:

```json
{
  "id": "session-1",
  "last_turn_outcome": {
    "command_id": "api-7f3c",
    "accepted_ordinal": 42,
    "turn_start_position": 43,
    "completed_ordinal": 61,
    "completed_at_ms": 1788000000000,
    "outcome": { "kind": "completed", "stop_reason": "end_turn" }
  }
}
```

`outcome.kind` is `completed` with a `stop_reason`, `rejected` with a `message`,
or `interrupted` with a `message`. The list route omits the field: it is built
from the dashboard projection, which carries no turn identity.

### Create a session

```text
POST /api/v1/sessions
```

```json
{
  "workspace_id": "workspace-1",
  "profile_id": "codex",
  "target_id": "local",
  "bundle_id": "bundle-1",
  "project_directory": "/home/you/project",
  "title": "add a README line",
  "model": "gpt-5",
  "effort": "high",
  "prompt": "add a README line",
  "idempotency_key": "run-2026-09-11-a"
}
```

`profile_id` and `target_id` are required. Supply `bundle_id`, or
`project_directory`, or both: a directory with no bundle is bundled the way the
viewer's own form does it. Everything else is optional.

```json
{ "session_id": "session-1", "turn_id": null }
```

The reply is `201` as soon as the controller has published an id.
**`turn_id` is null on creation**: provisioning a target takes minutes, so the
first prompt is submitted in the background once the harness is ready, after
`model` and `effort` are applied. Wait on the session without a `turn_id` and
the wait picks up that first turn by itself.

`idempotency_key` (1–128 characters) makes a retry safe. A second call with a
key that already created a session answers `200` with the same `session_id`,
and with its `turn_id` once the first prompt has been accepted.

### Send a prompt

```text
POST /api/v1/sessions/{session_id}/prompt
```

```json
{ "text": "now add a test" }
```

```json
{ "turn_id": 57 }
```

The reply is `202`: the prompt was accepted, not finished. `turn_id` is the
relay acceptance ordinal, and it is what `wait` takes. A session that cannot
take a prompt right now answers `409`.

### Wait for a turn

```text
POST /api/v1/sessions/{session_id}/wait
```

```json
{ "turn_id": 57, "timeout_secs": 600 }
```

Both fields are optional. `timeout_secs` defaults to 600 and may not exceed
3600. With no `turn_id` the wait ends when the session is idle with nothing
queued, which is what a caller that lost its turn id — or that created the
session and never had one — wants. Queued prompts are why the explicit id
matters: without it, an earlier prompt's outcome could answer for a later one.

`wait` reports how the named turn ended, and nothing else. An error the
session recorded for some earlier, unrelated action does not fail the turn you
asked about; the wait keeps waiting until that turn actually ends.

```json
{
  "outcome": "finished",
  "stop_reason": "end_turn",
  "final_message": "I added the line and ran the tests.",
  "turn_id": 57,
  "turn_number": 3,
  "elapsed_ms": 42318,
  "relay": { "state": "connected" },
  "session": { "id": "session-1" }
}
```

| `outcome` | What happened |
| --- | --- |
| `finished` | The turn completed normally. |
| `error` | The turn failed or was rejected, the session could not be launched or started, or the harness gave a stop reason Mjolnir does not recognize. `stop_reason` and `message` say which. |
| `cancelled` | The turn was cancelled or interrupted, which is what `cancel-turn` produces. |
| `quota_limit` | The model was at capacity and no retry is armed. |
| `timeout` | The deadline passed with the turn still running. Wait again with the same `turn_id`. |
| `stopped` | The session is stopped, stopping, or failed, so no turn can finish on it. |

- `turn_number` is the one-based position of this turn in the conversation, and
  `elapsed_ms` is how long it took from its first item to its last change. Both
  are absent when the wait ended without a finished turn.
- `final_message` is the agent's last message of the turn, flattened to text.
- **A capacity retry keeps the wait waiting.** When the model reports capacity
  limits, the worker arms its own retry and submits it; returning `quota_limit`
  then would make your next prompt collide with that retry. If the deadline
  passes first, the response carries
  `"capacity_retry": { "attempt": 2, "retry_at_ms": 1788000000000 }`. Do not
  send a prompt while that field is present.
- `relay` describes the daemon's live view of this session, when a live actor
  holds it: `{ "state": "unreachable", "detail": "ssh: connection refused" }`.
  `state` is `connected`, `disconnected` (attaching, or between tries),
  `unreachable`, `target_missing`, or `projection_integrity`; `detail` is the
  view's own description of the problem. It is on every wait response, and it
  never changes the outcome — it is how a `timeout` tells "the turn is still
  working" from "the daemon cannot see the worker". A session with no live
  actor carries no `relay` field rather than an invented reading.

### Read the transcript

```text
GET /api/v1/sessions/{session_id}/transcript?after_seq=0&limit=200
```

```json
{
  "session_id": "session-1",
  "latest_seq": 61,
  "execution": { "state": "idle" },
  "items": [
    {
      "stable_id": "item-12",
      "position": 43,
      "seq": 43,
      "role": "user",
      "text": "add a README line",
      "created_at_ms": 1788000000000,
      "last_changed_at_ms": 1788000000000,
      "body": { }
    }
  ]
}
```

Page by `seq`, not by `position`: an agent message that is still streaming keeps
its position but takes a new `seq` each time its content grows, so passing the
highest `seq` you have seen as the next `after_seq` gives you the updated item
again rather than missing it. A page whose last `seq` equals `latest_seq` is up
to date. `limit` defaults to 200 and is clamped to 1000.

`role` is a stable word — `user`, `agent`, `thought`, `tool`, `terminal`,
`plan`, `plan_proposal`, or `system` — and `text` is the
item flattened the way the other surfaces flatten it. `body` is the raw stored
item, shaped by the harness and the ACP traffic behind it. **`body` is not
stable**: read it when you need structure the text cannot carry, and expect its
shape to change between Mjolnir versions.

The transcript is read from the durable projection, so it answers the same way
while the session runs and long after it stopped.

### Close a session, cancel a turn

```text
POST /api/v1/sessions/{session_id}/close
POST /api/v1/sessions/{session_id}/cancel-turn
```

Both take no body and answer `202` with no content: the action was accepted, and
the session's own state is where you see it take effect.

### Get the work out

```text
GET  /api/v1/sessions/{session_id}/diff
GET  /api/v1/sessions/{session_id}/files?path=src/main.rs
POST /api/v1/sessions/{session_id}/export
```

`diff` answers `text/x-diff` — a unified diff of everything the session changed
against the commit it started from, committed and uncommitted work alike,
including files the agent never told git about.

`files` answers `application/octet-stream`. The path is relative to the session
workspace; an absolute path or one containing `..` is `400`, a path that leaves
the workspace through a symlink is `409`, and a file over 16 MiB is `409` — take
the bundle instead.

`export` takes the form you want:

```json
{ "kind": "patch" }
{ "kind": "branch", "branch": "review/one" }
{ "kind": "bundle" }
```

- `patch` answers the same `text/x-diff` body as `diff`.
- `branch` pushes the session's current HEAD to the repository's push remote
  (`remote.pushDefault`, else `origin`) and answers
  `{ "branch": "review/one", "remote": "origin" }`.
- `bundle` answers `application/octet-stream` with a
  `Content-Disposition: attachment; filename="<session>-<repo>.bundle"` header.
  The bytes are a git bundle `git bundle verify` accepts.

Preconditions, all answering `409` with the reason:

| Export | Needs |
| --- | --- |
| `diff`, `files`, `branch` | A live target. A stopped session has none; use the bundle. |
| `branch` | An idle session — a push mid-turn would publish a tree the agent is still changing — a valid branch name, and a configured push remote. |
| `diff` | A recorded base commit, or a session branch whose reflog still names where it started. |
| `bundle` | Commits beyond the session base. A live session is checkpointed first; a stopped one is read from its last checkpoint, so this is the one export that still works after the target is gone. |

## CLI equivalents

| Command | Route |
| --- | --- |
| `mj api-info` | — (prints the base URL and token path) |
| `mj sessions` | `GET /sessions` |
| `mj sessions --session <id>` | `GET /sessions/{id}` |
| `mj new` | `POST /sessions` |
| `mj prompt --session <id>` | `POST /sessions/{id}/prompt` |
| `mj prompt --session <id> --wait` | `POST /sessions/{id}/prompt`, then `POST /sessions/{id}/wait` on the turn it returned |
| `mj wait --session <id>` | `POST /sessions/{id}/wait` |
| `mj transcript --session <id>` | `GET /sessions/{id}/transcript` |
| `mj diff --session <id>` | `GET /sessions/{id}/diff` |
| `mj export --session <id> --kind patch\|branch\|bundle` | `POST /sessions/{id}/export` |
| `mj export --session <id> --kind file --path <path>` | `GET /sessions/{id}/files?path=` |
| `mj close --session <id>` | `POST /sessions/{id}/close` |
| `mj cancel-turn --session <id>` | `POST /sessions/{id}/cancel-turn` |

Every one of them takes `--json` and then prints the route's response unchanged,
which is the quickest way to see a shape before you write a client for it.

## A whole run

```console
mj new --profile codex --target local --project-directory . \
  --idempotency-key run-a "add a README line"
mj wait --session <id>
mj prompt --session <id> --wait "now add a test"
mj diff --session <id>
mj export --session <id> --kind bundle --out work.bundle
mj close --session <id>
```

`mj transcript --session <id>` still answers after the close: the projection
outlives the session. See [session lifecycle](/sessions/) for what closing and
resuming do, and [security boundaries](/security/) for what the token reaches.


## Discover and change model settings

`GET /api/v1/profiles/{profile_id}/config` returns `model`, `models`, `efforts`,
and `observed_at` (Unix seconds). Each choice contains its exact `value`, display
`name`, and optional `description`. Add `?model=<value>` to discover effort
choices for that model. An empty cache is populated automatically with a
prompt-free harness probe; no project session or container is created. The
first lookup may install the managed harness on the controller and take several
minutes. Discovery currently requires a Unix controller with the harness's
installation prerequisites and access to the profile's provider.

The cache persists across daemon restarts, expires after 24 hours, and is keyed
by profile settings and harness installation version. Efforts are cached per
model. Concurrent cold lookups share discovery. Probe failures remain retryable
and return `503` with their cause. Live managed workers with a matching build
refresh their model-specific entries. Container harnesses are still authoritative
about the settings they actually accept.

`POST /sessions` automatically discovers and validates requested `model` and
`effort` before bundling or provisioning. An unknown selector returns `400` with
available values, after refreshing cached choices. Settings are applied model
first, then effort using the model's updated choices. A later target-side
configuration failure leaves the session available for repair and is reported
by `wait`. It does not consume a prompt turn.

Session responses include `config_options` with each setting's `key`, `label`,
`current`, and `choices`. Apply one setting with:

```http
PATCH /api/v1/sessions/{session_id}/config
Content-Type: application/json

{"key":"model","value":"kimi-code/k3"}
```

The route waits for the setting to be applied and returns the updated session.
Invalid advertised choices return `400`; a rejected configuration command returns
`409` with its reason. Initialization must finish before callers change settings
or send another prompt. After repairing a failed initialization, submit a new
prompt explicitly; the original first prompt is not replayed.

```sh
mj models --profile kimi --json
mj models --profile kimi --model kimi-code/k3 --json
mj set-config --session "$id" --key model --value kimi-code/k3 --json
mj set-config --session "$id" --key effort --value high --json
```

## Workspace selection and stopping provisioning

`GET /api/v1/sessions?workspace_id=<id>` filters the session list. The CLI resolves
`mj sessions --workspace <name>` to that workspace ID. Without a selector, the
list includes all workspaces.

`close` is accepted while provisioning or another lifecycle operation is in
flight. It cancels cancellable work, prevents the initial prompt, waits for the
old operation to release ownership, and then cleans up. Repeated closes join the
same operation. A stopping session's `wait` returns `stopped`, including when an
earlier initialization failed. Cleanup errors remain visible in session state.

The CLI and `mj api-info` probe API support before reading the token file. A
daemon predating this API produces an explicit `mj daemon restart` instruction;
no restart is performed automatically. Disabled viewers, connection failures,
incompatible API versions, and missing tokens have separate diagnostics.
