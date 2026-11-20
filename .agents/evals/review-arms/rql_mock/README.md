# RQL writer transport

The R and R-loc writers use the same `rql_query` MCP tool. The tool runs on the
host and consumes complete, stable-ordered engine results. OpenSSH carries
newline-delimited JSON-RPC over stdio. The host maps an authenticated writer to
one manifest snapshot and arm. Run the SSH service and engine outside the writer.

## Host setup

Keep the transport files, frozen manifest, config, and query logs outside the
writer's mounts. Install an individual Ed25519 key for each R or R-loc writer
session. Give the writer only its own private key and give the SSH account only
the privileges needed to run the endpoint. Use a dedicated SSH endpoint and
account configured for public-key authentication and the pilot keys. Do not
enable client-supplied environment variables or user login scripts for it.

Create a JSON config at an absolute host path. OpenSSH's forced command selects
the config, and the config selects the engine callable and that writer's log.

```json
{
  "manifest": "/mnt/optane/review-arms-pilot/manifest.json",
  "engine": "rql_mock.stub_engine:answer",
  "sessions": {
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa": {
      "diff": "mjolnir/123",
      "intro": "1111111111111111111111111111111111111111",
      "arm": "R-loc",
      "log": "/mnt/optane/review-arms-pilot/runs/mjolnir/123/R-loc/r1/writer/queries.jsonl"
    }
  }
}
```

Use the actual frozen manifest path. The server reads `page_rows` from it on
each connection and verifies the assigned diff and introduction commit against
it. The `engine` is an importable Python `module:callable`; its callable receives
`answer(intro, query_text)`. The forced command can import modules next to
`server.py` by package name, such as `rql_mock.engine:answer`. Keep engine stdout
free for the JSON-RPC server. The engine owns the cache shared by R and R-loc.

Create the log parent before launching the tool. Replace the assignment file
atomically when registering a session. Use a serialized registrar if multiple
launchers update the same file. A reconnect reads a fresh config. Keep each
writer assignment fixed while its key is active.

Once you have the full Mjolnir session ID, set the public key's
`authorized_keys` entry, using the real absolute paths and that same ID.

```text
restrict,command="/absolute/rql_mock/serve-rql.sh /absolute/transport.json aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" ssh-ed25519 AAAA...
```

`restrict` disables forwarding, PTY allocation and user RC files. The wrapper
compares `SSH_ORIGINAL_COMMAND` with the ID bound by the key entry. The server
then verifies the ID exists in the host config before loading the engine. Revoke
the key when the writer is done. The writer cannot select a different commit,
arm, engine, config, or log path.

## Slice 5 writer setup

Install `review_arms_rql.config.toml` as
`<profile Codex home>/review_arms_rql.config.toml`. Use
`codex_profile = "review_arms_rql"` on the R and R-loc writer profiles. G and
reviewer profiles should have no overlay or pilot key.

Provide these to the writer's environment before its first turn.

| Variable | Value visible inside the writer |
| --- | --- |
| `RQL_SSH_CONFIG` | Absolute path of the supplied `ssh_config`. |
| `RQL_SSH_KEY` | Absolute path to that writer's private key, readable only by its user (mode 0600 or 0400). |
| `RQL_SSH_KNOWN_HOSTS` | Absolute path to a file with the trusted endpoint's pinned host public key. For a nondefault port the host token is `[host]:port`. |
| `RQL_SSH_TARGET` | `user@host` or `ssh://user@host:port`. Use an address reachable from the writer. |
| `MJ_SESSION_ID` | Mjolnir's full 32-character lowercase hex session ID. The overlay passes this alone as the remote command. |

Place the SSH config and host pin inside the writer. Install `client.py`
alongside the SSH config and use a private directory for its key. Files can
be staged in the writer's own container or provided through a writer-specific
read-only directory mount. The key path must be absent on initial startup.
Configure the sealed Podman environment so that its sole egress exception is
the dedicated SSH endpoint.
The client configuration verifies the host key and disables ambient SSH identity
agents and forwarding. Container egress restrictions belong to the launcher.
A writer with its own key can open another connection, but every connection
executes the same forced command for the same session.

Create the writer without a prompt. Use the registered ID from `mj new` to
register its host assignment and activate its key entry. Publish the private
key last, with an atomic rename into the configured path, then wait for the
writer to become ready before sending its first turn. Codex can launch the
MCP command while the worker is still starting. The command waits for the
key within Codex's 60-second MCP startup timeout. Before allowing the first
turn, the Slice 5 hook must execute `python3 -B /writer/path/client.py --preflight`
inside the writer with the same environment as the overlay. That command
checks MCP initialization, tool discovery and ping through the authenticated
SSH connection and emits `{"session_id":"<id>","tools":["rql_query"]}` on stdout
on success. A nonzero exit fails the job before its prompt. Profile
discovery may start a throwaway Codex thread without a Mjolnir session ID.
The command exits immediately in that case. An allocation or resume that
changes the session ID needs a new key binding and assignment before any turn.

## Results and logs

The success payload is
`{"status":"ok","rows":[...],"total":N,"page":P,"pages":K,"coverage":{...}}`
as JSON text in a single MCP text content item. Every page carries the same
answer-level `coverage` object with `complete` and `open`. An empty answer
returns page 1 with `pages: 0` and preserves any open frontiers. A request
beyond the last page gets `page_out_of_range`.
Parser and engine errors return their `status` and `message` with MCP
`isError: true`. The host sends rows as received in R and projects them to
`{sites: [{path, start_line, end_line}]}` in R-loc. Every site remains in
its original row and in order. Each open coverage frontier contains `site`
and `reason` in R. R-loc keeps only the frontier's `site` and preserves the
answer's `complete` flag.

The host appends one JSONL record per tool call. It includes UTC `time`,
`session`, `diff`, `intro`, `arm`, `query`, requested `page`, `status`, `total`, `coverage`,
`answer_sha256`, `latency_ms`, the delivered `payload` with its rows, and engine
`provenance`. The hash covers the UTF-8 bytes of the exact JSON string sent in
the MCP text content. The log contains only the delivered page and may contain
source text. Keep it host-private. Rejected SSH sessions never reach a tool call.

The stub needs no source checkout or model. Use queries `paged` (123 rows),
`empty`, `open-empty`, `large` (a 96 KiB snippet in R), `parse-error` and `engine-error` when
checking the transport. Run the test suite from the evaluation directory with
`python3 -B -m unittest discover -v -s rql_mock -p test_server.py`.

Unlike the Oz overlay, this overlay takes private file paths rather than a
base64 key in the environment. The server takes the snapshot from the host
assignment rather than Git HEAD supplied by the writer. The session ID is an
authentication binding as well as a log field. RQL answers come from the
engine callable and use manifest-driven pagination.
