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

A failure is a JSON object with an `error` sentence:

```json
{ "error": "this session cannot take a prompt right now" }
```

Some refusals also carry a `code` that names the reason, so a client can add
advice that fits its own interface. The sentence names no interface. Today the
only code is `subagent_choice_unavailable`: a new session asked for a
single-model sub-agent model or effort that no eligible profile offers.

| Status | Meaning |
| --- | --- |
| `400` | The request was malformed: an empty prompt, a timeout outside 1–3600 seconds, a file path that is absolute or contains `..`. |
| `401` | No bearer token and no valid viewer cookie. |
| `404` | No such session, or no transcript recorded for it. |
| `409` | The session cannot do this now: no prompt capability, no live target, a turn still running, no commits to bundle, no recorded base for a diff, no push remote configured. |
| `429` | Concurrent action limit: retry after the running action finishes. |
| `500` | The operation was attempted and failed. The message says what failed. |
| `503` | The daemon is shutting down, or the controller is not accepting actions. |

Messages are written for you, the owner of the daemon, and they name real
profiles, targets, and git errors. Treat them as diagnostics, not as strings to
match on.

## Routes

### Get a GitHub App installation token

```text
GET /api/v1/github-token?owner=acme
GET /api/v1/github-token?repo=acme/project&repo=acme/tools
```

The authenticated daemon client receives a currently valid installation token
for the configured GitHub App. An owner selects its installation and returns
an installation-wide token. Repeated `repo=OWNER/NAME` values discover one
installation and limit the token to those repositories. The response is
`{ "token": "..." }`; treat it as a secret and never log or persist it. The
route returns an error when no App is configured.
The [`mj github-token`](/cli-reference/#github-app-token) command wraps this
route.

### List workspaces

```text
GET /api/v1/workspaces
```

```json
{
  "workspaces": [
    {
      "id": "workspace-1",
      "name": "Release work",
      "created_at": "2026-09-11T07:00:00Z",
      "last_opened_at": "2026-09-17T09:12:03Z",
      "session_count": 2
    }
  ]
}
```

Most recently opened first, which is the order the terminal's workspace tabs
use. A fresh instance answers with an empty list.

### Create a workspace

```text
POST /api/v1/workspaces
{"name": "Release work"}
```

```json
{
  "workspace": {
    "id": "workspace-1",
    "name": "Release work",
    "created_at": "2026-09-17T09:12:03Z",
    "last_opened_at": "2026-09-17T09:12:03Z",
    "session_count": 0
  }
}
```

The name is the identity: it is trimmed, must be 1–64 characters with no control
characters, and is unique case-insensitively. Naming a workspace that already
exists returns that workspace rather than making a second one, so a script may
call this before every run. An unusable name returns **400**.

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

`lifecycle` is one of `live`, `starting`, `suspending`, `suspended`, `failed`.
`chat_phase` is one of `idle`, `running`, `closing`, `closed`.

Session detail and the session embedded in wait responses also include optional
`background_work` from the current connected worker snapshot. Its `known` field
is `true` when background state is synchronized, `false` when it is not yet
synchronized, and `null` when the worker does not report this capability. `tasks`
contains known task records (`id`, `started_at_ms`, `command`, `can_stop`). The
whole field is omitted when no connected snapshot is available.

All session responses also include `background_tasks` from the daemon's current
projection. Each record has `id`, `command`, `started_at_ms`, and `can_stop`.
Treat IDs as opaque when requesting a stop.

```text
POST /api/v1/sessions/{session_id}/background-tasks/stop
```

Send `{"background_task_id":"<opaque task id>"}`. The daemon validates that
the task is still present and stoppable, then waits for the worker's
acknowledgement and answers **202** with no body. A task that has ended or cannot
be stopped returns **409**; an unavailable worker returns **503**; a provider
stop failure returns **500**. Inspect the session again to confirm task exit.

A finished prompt or idle chat phase does not prove background tasks have ended.
For Kimi, a routine checkpoint requires `known: true` and an empty task list,
in addition to the normal checkpoint prerequisites. A deferred bundle export
returns **409** with the prerequisite that blocked it; actual checkpoint failures
return **500**. Retry after state synchronizes and tasks finish. Missing worker
support requires an updated worker; do not infer safety from an empty list alone.


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
or `interrupted` with a `message`. This field retains the raw harness evidence:
`completed` means the harness returned, and its `stop_reason` may report failure.
Use `mj wait` or the normalized `turn_ended` event outcome to determine success.
The list route omits the field: it is built from the dashboard projection,
which carries no turn identity.

### List launch options

```text
GET /api/v1/options
```

```json
{
  "revision": 1790192456730219,
  "profiles": [{ "id": "codex", "harness": "codex" }],
  "targets": [
    {
      "id": "localhost",
      "kind": "local-bare",
      "requires_project_directory": true,
      "availability": "ready",
      "host": "local"
    },
    {
      "id": "docker",
      "kind": "local-docker",
      "requires_project_directory": false,
      "availability": "ready",
      "host": "local"
    }
  ],
  "bundles": [
    {
      "id": "fixture",
      "primary_repository": "fixture",
      "repositories": [{ "id": "fixture", "destination": "fixture" }]
    }
  ],
  "hosts": [
    {
      "id": "local",
      "label": "local",
      "targets": ["docker", "localhost"],
      "stale": false,
      "refreshing": false,
      "has_error": false
    }
  ],
  "default": { "profile_id": "codex", "target_id": "localhost" }
}
```

This route lists the profiles, targets, and bundles a new session can use,
without reading `config.toml`. It never fails.

- `revision` identifies the configuration the lists came from. Two replies
  with the same revision describe the same configuration.
- `profiles[].harness` is the harness kind, such as `codex` or `claude`.
- `targets[].requires_project_directory` is `true` when the target needs an
  existing Git directory (`project_directory` on create) instead of a bundle.
- `targets[].availability` is `ready` (the last check passed), `stale` (the
  reading is old), `unavailable` (the last check failed), or `unknown` (no
  check yet, normal just after startup). A local Podman, Docker, or Apple
  container target is also `unavailable` when its engine is missing or does
  not answer, as `mj doctor` and the dashboard report it. An `unavailable`
  target may carry `unavailable_reason`, a sentence for a person.
- `bundles[].repositories[].github` appears only for a repository with a
  GitHub source.
- `hosts` has one entry per host that has reported capacity. It is omitted
  when empty.
- `default` is the profile and target a create request uses when it names
  none. It is omitted when no default has been saved.

The reply contains no credentials, harness homes, SSH hosts or keys, container
environments, AWS details, or controller paths.

### Create a session

```text
POST /api/v1/sessions
```

```json
{
  "workspace_id": "workspace-1",
  "profile_id": "codex",
  "target_id": "localhost",
  "bundle_id": "bundle-1",
  "project_directory": "/home/you/project",
  "branch": "main",
  "base": "origin/main",
  "title": "add a README line",
  "model": "<model-id-from-profile-config>",
  "effort": "high",
  "prompt": "add a README line"
}
```

`profile_id` and `target_id` may be omitted; each then follows the saved
default that [`GET /api/v1/options`](#list-launch-options) reports. Supply `bundle_id`, or
`project_directory`, or both: a directory with no bundle is bundled the way the
viewer's own form does it.

When `model` is supplied without `profile_id`, Mjolnir checks every enabled,
usable profile and chooses the one offering the exact model and, when named,
the exact effort. It ranks by remaining quota, using the saved default profile
to break ties. If no configured profile supports that selection, the request
fails without changing the requested model or effort. Supplying both
`profile_id` and `model` pins the profile and validates the model and effort on
it.

Three fields choose where the session starts:

- `at`: a full commit object ID to start the workspace at. Before the first
  prompt, the bundle's primary repository is checked out at that commit. HEAD
  is detached unless `branch` is given. Requires `bundle_id`.
- `branch`: with `at`, the name of a new local branch created at that commit.
  Without `at`, the existing branch checked out in an isolated clone; otherwise
  the remote default is used.
- `base`: the Git revision `mj diff` compares against, when it is not `at`. It
  defaults to `at`. It does not move the checkout. A bundle session resolves it
  in the fresh clone, so name a commit SHA, a tag, or `origin/<branch>`. For a
  raw project's managed worktree without `at`, the worktree also starts here.

To start a bundle session at an exact commit on a new private branch:

```json
{
  "workspace_id": "workspace-1",
  "profile_id": "codex",
  "target_id": "builder",
  "bundle_id": "product",
  "at": "0123456789abcdef0123456789abcdef01234567",
  "branch": "town/run-123"
}
```

`at` always applies to the bundle's primary repository (its configured
`primary_repo`). Other repositories in the bundle start at their remote
defaults. `at` must be a full commit object ID, not a branch, tag, abbreviated
SHA, or revision expression. Mjolnir fetches the exact object from origin if
needed and refuses an unavailable commit. A moving remote branch cannot change
the selected revision.

With `at`, `branch` creates a new local branch without an upstream. Preparation
never pushes or modifies the source checkout. Existing branches, dirty
workspaces, and invalid selections fail visibly. `at` cannot be combined with
`project_directory` alone or with `create_managed_worktree: false`.

`subagents` sets the session's delegation policy:

```json
{"subagents": {"mode": "single_model", "model": "<model-id>", "effort": "low"}}
```

`mode` is `native` (the harness's own sub-agents), `single_model`, or `none`
(delegation disabled). `single_model` requires `model`, which must be one of the
eligible profiles' models, and takes an optional `effort`. Mjolnir selects an
eligible profile offering that exact model and effort by remaining quota; a
single model does not mean a single profile. The other modes take no other
fields. The API spells the modes with underscores; `mj new --subagents` spells
them with hyphens (`single-model`); both interfaces offer Native, single model,
and None. An explicit policy overrides the profile for this session without
changing the profile's settings. Existing sessions retain their recorded policy
when profile defaults change or sessions resume; Move can explicitly change it.
Only Claude and Codex sessions accept a mode other than `native`. When
`subagents` is omitted, the session uses the selected profile’s creation default
(Native when unset). An unsupported mode or an unavailable model answers
`422` with the reason in the body.

`review` sets the session's own turn review, overriding `[review]` for this
session only:

```json
{"review": {"mode": "on", "model": "<model-id>", "effort": "high"}}
{"review": {"mode": "off"}}
```

`on` turns on automatic review for the session, with optional `model` and
`effort` replacing those in `[review]`. `off` turns off automatic review; a
manual review still runs. A blank or overlong `model` or `effort` answers `400`.
When `review` is omitted, the session follows `[review]`. See
[per-session settings](/turn-review/#per-session-settings).

On a container target, the session gets the same CPU and memory limit that
the viewer's create form selects: the size last chosen for that host, else
8 CPUs and 32 GiB, capped at the host's totals. `cpus` and `memory_bytes`
override either one:

```json
{"cpus": 16, "memory_bytes": 68719476736}
```

A size above the host's reported totals answers `400`, and so does either
field on a target that is not a container target.

`all_models` is retired for new sessions and explicit Move overrides, which
answer `422` with supported alternatives. Existing multi-model sessions retain
their recorded policy on resume and on a Move with no policy override.

Session creation still returns its ID before preparation finishes. Wait for
readiness or inspect the session's failure before prompting. Readiness guarantees
that preparation verified the selected commit, branch, and clean working tree.
Session receipts report the immutable starting selection as `at`, `branch` and
`base`, the same names the request uses, alongside the session and bundle IDs.
`base` reads as `at` when no other base was named. While provisioning, these
record intent, not completed work.
After prompting or resuming a checkpoint, the current checkout can differ: use
the diff endpoint's repository metadata to inspect it. Resume preserves saved
work instead of resetting to the original selection.

Reuse the same bundle and workspace IDs for successive dispatches; each dispatch
gets its own session checkout. Interrupted preparation retains its session and
failure outcome. Retrying preparation may continue only the same selection in an
unchanged checkout; it refuses to overwrite work. Cleanup follows ordinary
session suspension/destruction and ACP exit policy. Repeating HTTP creation makes
a new session rather than retrying the previous one.

Everything else is optional. `idempotency_key` is no
longer accepted: a request that still carries it is rejected as an unknown
field.

```json
{ "session_id": "session-1", "turn_id": null }
```

The reply is `201` as soon as the controller has published an id.
**`turn_id` is null on creation**: provisioning a target takes minutes, so the
first prompt is submitted in the background once the harness is ready, after
`model` and `effort` are applied. Wait on the session without a `turn_id` and
the wait picks up that first turn by itself.

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
relay acceptance ordinal, and it is what `wait` takes. A session that is still
starting holds the prompt for up to 60 seconds and accepts it once its worker
attaches. An `interrupt-turn` sent during that hold withdraws the prompt: it
answers `409` and never becomes a turn. Any other session that cannot take a
prompt right now answers `409`.

For recovery operations, supply an optional `command_id` (also exposed as
`mj prompt --command-id ID`). Retrying the identical command with the same ID
returns its original relay acceptance ordinal. Keep the body unchanged; use a
new ID for a different command. Ordinary clients may omit it.

Typed `/clear` clears the native conversation in the same session and checkout.
Its acceptance is not completion: observe the `command_ended` event and the
context divider in the transcript before sending the new task. A rejected or
failed clear does not authorize resetting the checkout.

### Cancel queued prompts

```text
POST /api/v1/sessions/{session_id}/queued-prompts/clear
```

```json
{ "command_id": "cancel-obsolete-input-1" }
```

The ID is optional. This returns `202` with the relay acceptance `turn_id`,
cancels queued input, and preserves the active turn and environment. The CLI
is `mj clear-queue --session ID [--command-id ID] --json`. Interrupt an active
turn separately when needed.

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
  "tool_calls": 6,
  "relay": { "state": "connected" },
  "session": { "id": "session-1" }
}
```

For `mj wait --json` and `mj prompt --wait --json`, stdout contains the JSON
response even when the command exits unsuccessfully. `finished` and
`input_required` exit **0**. `timeout`, `error`, `cancelled`, `quota_limit`, and
`stopped` exit **1**, with a diagnostic on stderr. A timeout does not cancel the
turn. Transport or authentication failures may have no JSON response.

Preserve stdout before interpreting the exit status, for example:

```python
result = subprocess.run(
    ["mj", "wait", "--session", session_id, "--json"],
    capture_output=True, text=True, check=False,
)
if not result.stdout.strip():
    raise RuntimeError(result.stderr)
response = json.loads(result.stdout)
# Handle response["outcome"], including timeout, before deciding whether to retry.
```

| `outcome` | What happened |
| --- | --- |
| `finished` | The turn completed normally. |
| `error` | The turn failed or was rejected, the session could not be launched or started, or the harness gave a stop reason Mjolnir does not recognize. `stop_reason` and `message` say which. |
| `cancelled` | The turn was cancelled or interrupted, which is what `interrupt-turn` produces. |
| `quota_limit` | Reported usage quota is exhausted, or the model was at capacity and no retry is armed. |
| `timeout` | The deadline passed with the turn still running. Wait again with the same `turn_id`. |
| `stopped` | The session is suspended or failed, so no turn can finish on it. `message` carries the session's own recorded reason when it has one, such as a resume that failed. |

- `turn_number` is the one-based position of this turn in the conversation, and
  `elapsed_ms` is how long it took from its first item to its last change. Both
  are absent when the wait ended without a finished turn. `tool_calls` is how
  many tool calls the turn made, absent in the same case.
- `final_message` is the agent's last message of the turn, flattened to text.
- `diagnostic`, when available, preserves the provider failure's `message`,
  optional `code`, `http_status`, and provider-supplied `reset_at`. The same
  explanation appears in `message`; session detail exposes `last_turn_diagnostic`.
  Diagnostic details persist with the turn and its completion event.
- **Usage quota exhaustion does not schedule a retry.** A recognized Kimi usage
  limit returns `quota_limit` with stop reason `QuotaLimit`. The caller decides
  when to send another prompt. HTTP 403 or an authentication error alone is not
  enough to classify a quota limit. Reset text is not an invented retry deadline.
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
while the session runs and long after it was suspended.

### Read earlier messages

`GET /api/v1/sessions/{session_id}/history` returns up to 128 stored items in
chronological order as `{ "items": [{ "role": "agent", "text": "..." }],
"before": { "position": 123, "stable_id": "agent:123" }, "frontier": 456 }`.
The first request returns the newest page. For the preceding page, pass both
`before_position` and `before_id` from `before`. A null `before` means the
beginning of the conversation. The cursor is exclusive and uses the original
message position, so new messages and streaming revisions do not shift pages.

The browser's **Earlier messages** reader and the terminal's **Ctrl+PgUp**
reader use this durable history. Live views retain recent complete turns;
checkpoints still contain the complete conversation.

### Suspend, destroy, or interrupt a turn

```text
POST /api/v1/sessions/{session_id}/suspend
POST /api/v1/sessions/{session_id}/destroy
POST /api/v1/sessions/{session_id}/interrupt-turn
```

These endpoints answer `202` when accepted. Destroy and interrupt turn answer
with no content; suspend answers with the body below. Acceptance is not
completion: inspect the session's lifecycle and error, or use `mj wait` for
suspension. An idle conversation alone says nothing about lifecycle completion.

Suspend saves a verified recovery copy before releasing the environment. Once
that copy is verified, it stops the session's active Mjolnir sub-agents,
without a recovery copy of their own, and suspends the session alone. A
suspend that fails before then leaves the sub-agents running. A sub-agent that has not handed back
its report loses the work it has not reported. The answer says how many
sub-agents the suspend stops and warns about those:

```json
{
  "session_id": "0123abcd…",
  "stopped_subagents": 2,
  "subagents_not_handed_back": 1,
  "warning": "1 sub-agent has not handed back; suspending stops it"
}
```

`warning` is left out when every stopped sub-agent had handed back. The
`acknowledge_active_subagents` field that older clients send is accepted and
ignored. For an independent clone whose publication is unverified, supply
`{"acknowledge_unpublished_work": true}` after reviewing the warning. A failed
suspension reports its error and preserves recoverable resources; it never
silently switches to destruction.

Destroy permanently removes the environment, recovery archive, and session
record, including sub-agents, after indexing their conversations in SessionWiki.
The optional `{"delete_branch": true}` body applies
only to older linked-worktree sessions and deletes their managed source branch.
New managed clones have no source branch to delete. Once destruction completes,
`GET /sessions/{id}` returns `404`. The indexed conversation remains available
through the wiki routes and `mj sessions --session <id>`; restoring it starts
a new session from conversation context, not from the deleted recovery archive.

Interrupt turn keeps the environment and session available for further prompts.
For a session that is still starting, it withdraws the prompts held for it.
There is no `/close` endpoint or `force` parameter. Destruction is available only
through its dedicated authenticated endpoint, not the generic viewer actions.

### Turn review

```text
POST /api/v1/sessions/{session_id}/review
GET  /api/v1/sessions/{session_id}/review
POST /api/v1/sessions/{session_id}/review/{forward|dismiss|cancel}
```

`POST` starts a one-off review of the turn the session just finished, with the
session's reviewer, as `/review` does in Prompt. It answers `202` once the
review has opened, which can take minutes while a reviewer is chosen and
started:

```json
{ "session_id": "0123abcd…", "started": true }
```

A review that cannot start answers `409` with the reason in `error`: the
session is still working, prompts are queued, it is a sub-agent, a review is
already open, or no reviewer is usable.

`GET` answers `200` with the review the session has open, as the viewer shows
it, or `"review": null` when none is open:

```json
{
  "session_id": "0123abcd…",
  "review": {
    "status": "sending findings to the primary agent…",
    "roles": [{ "label": "reviewer", "state": "findings" }],
    "verdict": {
      "kind": "findings",
      "text": "[P1] src/lib.rs:1 -- unbounded retry loop",
      "allowed": ["forward", "dismiss", "cancel"]
    }
  }
}
```

A role's `state` is `pending`, `running`, `done`, `findings`, or `failed`.
`verdict` appears once the review has one; its `kind` is `clean`, `findings`,
or `failed`, and `allowed` lists the resolutions it accepts. An unknown session
answers `404`.

`POST …/review/{resolution}` resolves the open review as the viewer's buttons
do and answers `202` with `{ "session_id": "…", "resolution": "dismiss" }`.
Findings are forwarded automatically, so `forward` only retries a forward the
primary refused. `cancel` is always accepted while a review is open; `forward`
and `dismiss` only when the verdict's `allowed` lists them. No open review, a
resolution the verdict does not allow, or another word answers `400`; a review
that closed before the daemon reached it answers `409` with the reason.

### Resume a suspended session

```text
POST /api/v1/sessions/{session_id}/resume
```

```json
{
  "profile_id": "codex",
  "target_id": "localhost",
  "workspace_id": "workspace-1",
  "queue": "start"
}
```

The body is optional and so is every field in it: the session's own record
supplies the profile, target, and workspace it last ran with, so a bodiless POST
means "continue this session where it left off". `queue` is `start` or
`discard`, and decides what happens to prompts that were queued when the session
suspended; it defaults to `start`.

```json
{
  "session_id": "session-1",
  "workspace_id": "workspace-1",
  "profile_id": "codex",
  "target_id": "localhost"
}
```

The reply is `202` with the settings the resume will actually use. Like
creation, it answers before the session is up: restoring a checkpoint onto a
fresh target takes minutes. Wait on the session with no `turn_id` and the wait
blocks while the resume runs, then returns once the session is live; a resume
that fails answers `stopped` with the reason.

This is the same operation the terminal's Resume wizard runs, with the same
repository preflight and the same cross-harness handoff. A running session is
refused with `409` — suspend it first, or use a move to change where a live
session runs. A session with no checkpoint to restore, such as one whose launch
failed before its first checkpoint, is refused with `409` too; remove it with
`mj destroy`. Model and effort are not part of this request: apply them with
`PATCH /sessions/{id}/config` once the wait returns.

### Get the work out

```text
GET  /api/v1/sessions/{session_id}/diff
GET  /api/v1/sessions/{session_id}/files?path=src/main.rs
POST /api/v1/sessions/{session_id}/export
```

`diff` answers `text/x-diff` — a unified diff of everything the session changed
against the commit it started from, committed and uncommitted work alike,
including files the agent never told git about.

Add `?base=<revision>` to choose an explicit comparison commit without changing
the recorded launch baseline. Git revisions such as `HEAD~2` are accepted;
unknown revisions return `409`. Add `json=true` to receive `application/json`
with `diff`, `base`, `head`, and `head_descends_from_base` fields. `base` and
`head` are resolved commit IDs; the patch still includes uncommitted and
untracked files. `head_descends_from_base` is `false` when the session's HEAD is
no longer a descendant of the base — the agent rebased, amended, or reset — so
the diff carries history changes as well as session work; a worker from before
this field omits it. For example,
`GET /api/v1/sessions/{session_id}/diff?base=HEAD~2&json=true` reports both the
chosen baseline and the current checked-out commit. JSON metadata requires an
updated session worker; older workers return an explicit upgrade refusal.

`files` answers `application/octet-stream`. The path is relative to the
directory the session's agent runs in — the primary repository's directory,
whatever the target kind — so a path the agent would use means the same here.
A `..` reaches a sibling repository of a multi-repo bundle, as in
`../other/src/main.rs`, and stops at the workspace root those repositories
share. A session with a single repository has no sibling to reach, so there
`..` stops at the agent's own directory. An absolute path is `400`; a path that
climbs past the boundary is `409`, as is one that leaves it through a symlink,
and a file over 16 MiB is `409` — take the bundle instead.

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
| `diff`, `files`, `branch` | A live target. A suspended session has none; use the bundle. |
| `branch` | An idle session — a push mid-turn would publish a tree the agent is still changing — a valid branch name, and a configured push remote. |
| `diff` | A recorded base commit, or a session branch whose reflog still names where it started. |
| `bundle` | Commits beyond the session base. A live session is checkpointed first; a suspended one is read from its last checkpoint, so this export also works after the target is gone. |

## CLI equivalents

| Command | Route |
| --- | --- |
| `mj api-info` | — (prints the base URL and token path) |
| `mj workspaces list` | `GET /workspaces` |
| `mj workspaces create <name>` | `POST /workspaces` |
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
| `mj suspend --session <id>` | `POST /sessions/{id}/suspend` |
| `mj resume --session <id>` | `POST /sessions/{id}/resume` |
| `mj destroy --session <id> [--delete-branch]` | `POST /sessions/{id}/destroy` |
| `mj interrupt-turn --session <id>` | `POST /sessions/{id}/interrupt-turn` |
| `mj review start --session <id>` | `POST /sessions/{id}/review` |
| `mj review status --session <id>` | `GET /sessions/{id}/review` |
| `mj review forward\|dismiss\|cancel --session <id>` | `POST /sessions/{id}/review/{resolution}` |
| `mj stop-task --session <id> <task-id>` | `POST /sessions/{id}/background-tasks/stop` |

These commands accept `--json`. `mj stop-task` prints its aggregate report of
accepted, skipped, and failed task IDs; the other commands print the route's
response unchanged.

## A whole run

```console
mj workspaces create scripts
mj new --workspace scripts --profile codex --target localhost --project-directory . \
  "add a README line"
mj wait --session <id>
mj prompt --session <id> --wait "now add a test"
mj diff --session <id>
mj export --session <id> --kind bundle --out work.bundle
mj suspend --session <id> --acknowledge-unpublished-work
```

`mj transcript --session <id>` still answers after suspension: the projection
and session record are retained. See [session lifecycle](/sessions/) for what suspending and
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
available values, after refreshing cached choices. With an omitted `profile_id`,
the daemon uses the saved default profile as the eligibility anchor and ranks
profiles that offer the requested selection by quota. Settings are applied
model first, then effort using the model's updated choices. A later target-side
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

`POST /api/v1/sessions` accepts `workspace_id`. When the request names none, it
uses the instance's only workspace when there is exactly one. An instance with
no workspace answers `409` and says how to create one, and an instance with
several needs an explicit `workspace_id`. No session is placed in a workspace
that the dashboard and the viewer do not list. The CLI's `mj new` always names
its workspace. `GET /api/v1/workspaces` and `POST /api/v1/workspaces`
are the routes for choosing deliberately.

`close` is accepted while provisioning or another lifecycle operation is in
flight. It cancels cancellable work, prevents the initial prompt, waits for the
old operation to release ownership, and then cleans up. Repeated closes join the
same operation. A suspending session's `wait` returns `stopped`, including when an
earlier initialization failed. Cleanup errors remain visible in session state. A
close whose worker restart fails leaves the session in `error` with the failure
recorded in its state. Retry suspension after resolving the error. Use `mj destroy` only when you intend to discard the environment and recovery copy.

The CLI and `mj api-info` probe API support before reading the token file. A
daemon predating this API produces an explicit `mj daemon restart` instruction;
no restart is performed automatically. Disabled viewers, connection failures,
incompatible API versions, and missing tokens have separate diagnostics.


## Usage and filtered transcripts

`GET /api/v1/sessions/{id}/usage?after_seq=0&limit=200` returns recorded
`turns`, `next_after_seq`, `latest_seq`, `totals`, and `coverage`. Resume with
`next_after_seq`. Each turn includes command identity, completion sequence,
outcome, and optional `usage`. Wait responses also include that turn's usage
when reported. Records persist after suspension and daemon restart. History starts
with events projected by this version; older usage is not backfilled.

Usage scope is `turn`, `last_request`, or `unspecified`. The managed Claude
adapter reports the whole turn. Managed Codex adapter 1.11.2 reports consumption
across a prompt’s model requests, including cancellation. Unknown resumed baselines,
missing reports, or counter resets retain incomplete (`unspecified`) coverage. Older
Codex adapters and historical reports retain `last_request` scope. Grok's native
prompt-ledger metadata and matching completion notifications report whole-turn
consumption; explicitly incomplete reports retain `unspecified` scope. Kimi
reports retain `unspecified` scope. The managed Muse adapter 0.4.3 and
later report whole-turn usage on the prompt response and omit the report when the
backend reported no model legs; older Muse adapters produce no token reports.
Only known whole-turn reports contribute to `totals`. Each counter includes
`tokens` and `reported_turns`; `coverage` counts recorded turns, full reports, partial
last-request reports, unspecified reports, and missing reports. An absent counter
stays absent. These are reported totals for covered turns, not a billing estimate.
Grok and Muse usage may also carry `provider_details`: optional `model_calls`,
`api_duration_ms`, provider `elapsed_ms`, `cost`, and a `model_usage` map keyed by
the provider's exact model IDs (for example, `grok-4.6-build`). Muse reports
`model_calls`, `api_duration_ms`, and
`model_usage`, and no cost: the Muse plan is subscription-metered, so a turn
carries no price. Each model row uses the same normalized token fields. Full
input already includes cache reads and cache creation; output already includes reasoning. Do not add those subsets to
input/output again, or add model rows to the top-level turn total.

Turn `cost` carries exact integer `usd_ticks` (10¹⁰ ticks per USD), its exact
`usd` decimal **string**, and `is_partial`. Partial cost is a reported subtotal,
not a complete bill. Missing cost means unknown, not free. Provider elapsed time
is separate from Mjolnir's top-level wait `elapsed_ms`. Duplicate response and
notification reports are reconciled by native prompt identity, not summed.
These details are collected for future turns; historical missing usage remains
missing. Upgrade the controller before workers: new workers require relay
protocol 13 readers to preserve provider details when verifying event hashes. New controllers can still read older workers and
records.

`provider_session_cost`, when present, is the latest provider-reported cumulative
session amount, currency, and observation timestamp. It is not summed across
provider session resets. Context-window occupancy is not consumed tokens.

`GET /api/v1/sessions/{id}/transcript?role=agent&role=user&after_seq=0&limit=200`
filters the selected roles before applying the limit. Send `role` once for one
role or repeat it to select several. Roles are `user`, `agent`, `thought`,
`tool`, `terminal`, `plan`, `plan_proposal`, and `system`. Omit `role` for all
items; that API default is unchanged. Use `next_after_seq` to resume, including
empty filtered pages. When no more matching items remain, the cursor advances
across excluded roles to `latest_seq`. The requested limit is soft when several
items share an event sequence: all tied items travel together so paging cannot
skip them. Streaming updates are still returned when their sequence advances.

Add `finished_only=true` to request only agent items whose `body.streaming` is
`false`; this opt-in does not change the default response. It implies
`role=agent`, so omitting `role` or supplying only `role=agent` works, while
including any other role returns `400 Bad Request`. In this mode,
`next_after_seq` is an exclusive resume cursor for the finished prefix: it
never passes the sequence of the earliest currently open agent item. When the
page limit is reached before that barrier, the cursor is the last returned
sequence; otherwise it advances through filtered non-agent rows to just before
the barrier, or to `latest_seq` when no agent item is open. Pass that exact
cursor on the next poll; the open item remains eligible and is returned when
its stream closes. `latest_seq` remains the newest sequence across all roles
and is not a resume cursor for this mode.

Closed means `body.streaming` was `false` in the projection read. ACP can still
append late chunks to the same `stable_id` afterward, advancing that item's
sequence. A consumer that deduplicates only by `stable_id` may ignore that late
revision; finished-only polling does not promise delivery of post-close chunks.

```sh
mj usage --session SESSION --json
mj transcript --session SESSION --role agent --limit 20 --json
```


## Follow durable events

`GET /api/v1/events` streams server-sent events using the same authentication and
version headers as the other v1 routes. Optional `session_id` and `workspace_id`
filters apply together. Workspace filtering uses the session's current workspace.

Without a cursor, the stream follows new events from the current database
frontier. Use `after_seq=0` to replay all retained history, or resume after the
last delivered sequence with `after_seq=N` or the `Last-Event-ID: N` header.
Conflicting cursors, negative or out-of-range values, and cursors ahead of this
database return 400. Sequence IDs are global; gaps in a filtered stream are normal.

Each SSE frame has an `id` equal to its durable sequence and an `event` matching
the JSON `type`. For example:

```text
id: 42
event: input_resolved
data: {"seq":42,"session_id":"SESSION","recorded_at_ms":1789200000000,"type":"input_resolved","data":{"elicitation_id":"REQUEST","turn_id":12,"action":"accept"}}

```

| Event | `data` |
| --- | --- |
| `turn_started` | `turn`: prompt command ID, `accepted_ordinal` (the API turn ID, when known), transcript start position, and start timestamp. |
| `turn_ended` | `turn`: command and turn identity, completion ordinal and timestamp, outcome, and optional reported usage. |
| `command_ended` | Command identity, owner, kind, structured outcome, and optional cause and correlation. |
| `session_fault` | Typed `reason`, `message`, and nullable `command_id` for a session-level failure. |
| `legacy_notice` | `original_type`, `message`, and nullable `command_id` for unclassified historical diagnostics. |
| `input_required` | Nullable `turn_id`; `request` contains the normalized ACP elicitation and response schema for structured input. It is absent when Jev ends a turn with `awaiting_input`. |
| `input_resolved` | `elicitation_id`, nullable `turn_id`, and `action`; `cleared` means the request disappeared without an explicit response observation. |
| `activity_changed` | `activity`: the UI's `state`, nullable `details`, `is_idle`, `waiting_for_input`, and `capacity_retry`. |

Terminal events describe the object that ended. A failed `turn_ended` is the
signal that an accepted prompt failed. A failed `command_ended` describes its
control or shell operation; it does not establish a failed agent turn.
`session_fault` records a session-level problem, not a retrospective change to
completed turns. These events do not prescribe retries or automatic pauses.

`turn.outcome.kind` is `completed`, `input_required`, `cancelled`, `rejected`,
`interrupted`, or `failed`. Unsuccessful outcomes include `reason` and `message`;
`stop_reason` preserves the original harness value. Provider diagnostics and
usage remain on `turn`. Quota exhaustion is `failed` with reason `quota_limit`.
Unknown harness endings are `failed` with reason `unrecognized_stop_reason`.
Events, `mj wait`, and transcript rendering share this classification; `wait`
retains its existing exit codes and continuation policy.

`command_ended` includes `owner` (`worker` or `daemon`), `command_id`,
`command_kind`, and `outcome` (`succeeded`, `cancelled`, `rejected`, or `failed`).
Unsuccessful results include `reason` and `message`. A successful cancellation
command has outcome `succeeded`; its target has outcome `cancelled`.
Daemon checkpoint results can include `related_command_ids` for their worker
barriers. A checkpoint succeeds only after its verified archive and metadata
are durable. Barrier release alone is not proof of checkpoint success.

For example, controller disconnection releases a worker checkpoint barrier:

```json
{"type":"command_ended","data":{"owner":"worker","command_id":"opaque-id","command_kind":"begin_checkpoint","outcome":"cancelled","reason":"controller_disconnected","message":"Checkpoint barrier cancelled because its controller disconnected"}}
```

Other reasons include `owner_lost_on_restart`, `requested_cancellation`,
`admission_rejected`, `command_failed`, `worker_restarted`, `runtime_stopped`,
`runtime_failure`, `nonzero_exit`, `signaled`, `timed_out`, `startup_failed`,
`lifecycle_failed`, `runtime_unavailable`, `checkpoint_failed`,
`checkpoint_deferred`, `controller_restarted`, and `provider_failure`.
Reasons are structured facts; diagnostic wording and command-ID prefixes are
not classification contracts.

This contract replaces new generic `error` events. Upgrade preserves historical
sequence numbers and timestamps, normalizes retained turn results, and exposes
old generic errors as `legacy_notice` with `original_type: "error"`. It does not
guess their domain from messages or IDs. Older workers without typed terminal
causes can also produce legacy notices until their normal idle upgrade. Missing
historical turn causes are `legacy_unclassified`, never assumed harmless.
The new stored event variants require a forward database migration; older
incompatible executables cannot access that upgraded store.

Prompt starts, completions, and explicit input transitions are journaled with
their relay projection transaction, including intermediate transitions within one
projection batch. Activity events record observed UI snapshots and may coalesce
brief intermediate activity. The first observed activity is also recorded.
Activity `details.kind` is `turn`, `step`, `background`, `idle`, or `lifecycle`,
with available start/idle timestamps and a label. Unknown activity has no details.
Idle uses the existing UI classification: no foreground or background work.
Elapsed silence does not produce a separate stalled state. Pending input and
capacity retries remain separate facts.

While a turn or a tool call is in flight, `details.last_activity_at_ms` reports
when anything last arrived from the harness. Subtract it from the current time
to get the silence age. Mjolnir publishes this and does not act on it: a turn
waiting on a long build is silent and healthy, and ending a turn on a guess
destroys real work. `mj wait` includes the silence age in its `timeout`
message, and `mj sessions --session` prints it, so an orchestrator can decide
whether to keep waiting or call `mj interrupt-turn`. The field is absent for an
idle session, for a session the daemon currently cannot see, and from workers
too old to report the ACP clock.

History survives session stop and daemon restart and is deleted when its session
is forgotten. Recording begins with this version; old transitions are not
backfilled. Consumers should save the last processed ID and deduplicate replayed
IDs after reconnecting. Slow readers are buffered within fixed limits and cannot
block session execution. Keepalive comments carry no application event. A storage
failure may send an unnumbered `stream_error` frame and end the stream; reconnect
from the last processed ID. The viewer's existing `/api/events` route is separate.

```sh
mj events --session SESSION --after-seq 0
mj events --workspace-id WORKSPACE_ID --after-seq 42
mj --workspace WORKSPACE_NAME events
```

`mj events` prints one JSON event per line. Omit the cursor to follow new events
only; press Ctrl-C to stop. After an interrupted connection, restart with the last
printed `seq` as `--after-seq`.

## Upload files and answer structured questions

`PUT /api/v1/sessions/{id}/files?path=input.json` accepts a raw binary
body up to 16 MiB and returns `{ "path": "input.json", "bytes": 123 }`.
The path is relative to the agent's directory, as with file reads, so an
upload lands where a read of the same path finds it, and a `..` stops at the
same boundary. Parent directories are created. Absolute paths, paths past the
boundary, and symlink paths are refused. Existing files require
`overwrite=true`; publication is atomic.
The worker verifies the expected byte count before publication, so interrupted
transfers cannot publish truncated files.

The session must have a live idle worker, with no initialization, queued work,
or active background work. The controller holds a worker barrier through the
transfer, then releases it without making a checkpoint. Transfers have a
five-minute deadline. Cancelling the request signals the subprocess to stop;
the supervised transfer keeps its ownership until the subprocess exits. A
complete file published before cancellation remains in place. An older installed worker
must be upgraded by resuming the session before this command is available.

`GET /api/v1/sessions/{id}/elicitations` lists pending structured input requests.
They also appear as `pending_elicitations` in session detail. Respond with
`POST /api/v1/sessions/{id}/elicitations/{request_id}` and one of:

```json
{"action":"accept","content":{"name":"example"}}
```

```json
{"action":"decline"}
```

```json
{"action":"cancel"}
```

Free-text answers use an ACP form with a string field, such as the `name` field
above. Choice questions and plan approvals use the schema supplied by the harness;
inspect the pending request before constructing the response. These explicit
requests emit `input_required`, and their resolution emits `input_resolved` on
the event stream. An assistant question written only in chat text is not an ACP
elicitation. When Jev confidently identifies such a question during a quiet turn,
an `input_required` event without `request` precedes `turn_ended`; answer with a
new prompt instead of the elicitation response endpoint.

The response is checked against the actual request, including required fields
and allowed choices, before dispatch. Successful dispatch returns 202; an
unknown request returns 404 and an invalid answer returns 400.

Add `"return_on_input": true` to a wait request to return `input_required` with
`pending_elicitations` when a structured answer is needed. This does not mean
the turn finished. Send the response, then wait again. Ordinary waits retain
their existing behavior for structured requests. A turn completed with
`awaiting_input` returns `input_required` even without `return_on_input`; its
`pending_elicitations` may be empty.

```sh
mj put-file --session SESSION --path project/input.json ./input.json
mj put-file --session SESSION --path project/input.json --overwrite ./input.json
mj wait --session SESSION --return-on-input --json
mj elicitations --session SESSION --json
mj respond --session SESSION --elicitation REQUEST --response-file answer.json
```

`mj put-file` accepts `-` as the source for binary stdin. `mj respond` accepts a
positional JSON response or `-` for stdin. `mj prompt --wait` also accepts
`--return-on-input`. The CLI exits successfully on `input_required`
results so an orchestrator can answer and resume waiting.
