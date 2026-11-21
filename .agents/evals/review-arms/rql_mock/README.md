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
the config, and the config selects the engine loader and that writer's log.

```json
{
  "manifest": "/mnt/optane/review-arms-pilot/manifest.json",
  "engine": "rql_mock.stub_engine:load_engine",
  "sessions": {
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa": {
      "diff": "mjolnir/123",
      "intro": "1111111111111111111111111111111111111111",
      "arm": "R-loc",
      "manifest_sha256": "03adec56bc64db060dc092614ab2d8a8a672d42c69936403c88774ac5686eb8c",
      "engine_inputs_sha256": "48289d6a9100c185b175af78378819566630419d000910c4acc3f63d6f437c74",
      "log": "/mnt/optane/review-arms-pilot/runs/mjolnir/123/R-loc/r1/writer/queries.jsonl"
    },
    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb": {"arm": "none"}
  }
}
```

Register G writers and reviewers with arm `none` and issue keys only for the
R and R-loc sessions. Use the actual frozen manifest path. The server reads `page_rows` from it on
each connection and verifies the assigned diff and introduction commit against
it. The `engine` is an importable Python `module:loader` that returns an object
with `identity` and `answer(intro, query_text)`. Before calling `answer`, the
server checks both assignment digests against the loaded engine's identity.
Live assignments use the frozen manifest digest and the input digest used to key
the answer cache. The example uses the fixed `STUB_IDENTITY` for smoke tests.
Keep engine stdout free for the JSON-RPC server. The live engine owns the cache
shared by R and R-loc.

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

### Pilot enrollment hook

Pass `rql_mock/install-assignment.py` to the pilot's `--rql-hook`. The hook reads
the client settings from the private pilot's Codex profile in
`~/.config/mjolnir/instances/rql-census-pilot/config.toml` or the file named by
`RQL_PILOT_CONFIG`. Set `RQL_SSH_TARGET` in that profile's environment so both
preflight and the Codex client use the same address. Use a host address the
container can reach, such as `ssh://PILOT_USER@host.containers.internal:22222`.

Export these paths for the launcher.

| Variable | Host path |
| --- | --- |
| `RQL_TRANSPORT_CONFIG` | The private transport JSON. The hook creates it if absent. |
| `RQL_AUTHORIZED_KEYS` | The pilot sshd's dedicated key file, outside `~/.ssh`. |
| `RQL_SSH_HOST_PUBLIC_KEY` | The trusted public half of the pilot sshd's host key. |

Configure a dedicated sshd with a private host key and an unprivileged port.
Substitute the Podman-reachable host address, dedicated login and absolute paths.
The account running an unprivileged sshd must be the allowed login and have
access to the forced command, engine files and logs.

```text
ListenAddress PODMAN_REACHABLE_HOST_ADDRESS
Port 22222
HostKey /absolute/pilot/ssh_host_ed25519_key
PidFile /absolute/pilot/sshd.pid
AuthorizedKeysFile /absolute/pilot/authorized_keys
AllowUsers PILOT_USER
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitUserEnvironment no
PermitUserRC no
```

Run `/usr/sbin/sshd -D -e -f /absolute/pilot/sshd_config` as the pilot login.
Use the absolute path of the installed `sshd` binary. Keep the
host key and the dedicated authorization file private to that login. The
hook enrolls the session, publishes its local record, and runs the container's
MCP preflight so an unreachable address or wrong host pin stops setup. After
capture, `install-assignment.py --revoke /absolute/path/to/tool-assignment.json`
removes that session's authorization, transport assignment and staged key. It
also clears the assignment and SSH credential files in a running container.
Both operations can be repeated. The pilot records a pending revocation when
the hook fails after capture and retries it on the next run. The accepted brief
and its completion receipt remain available.

On a host using rootless Podman with pasta, verify how the guest reaches the
host before starting sshd. Podman 5.7's default `host.containers.internal`
mapping need not reach a listener bound to host loopback. A port-specific pasta
splice can forward guest loopback port 22222 to host loopback on compatible
versions. On this configuration, bind sshd to `127.0.0.1`, set the private
profile's `RQL_SSH_TARGET` to `ssh://PILOT_USER@127.0.0.1:22222`, and start the
private daemon with `CONTAINERS_CONF_OVERRIDE` pointing to an instance-local
file containing:

```toml
[network]
pasta_options = ["-T", "22222"]
```

Verify that the guest reaches the SSH port, another host loopback port is
unreachable from the guest, and sshd has no listener on a non-loopback address.
Restart the private daemon with the override before each resumed run. When
using frozen Mjolnir binaries, set `REVIEW_ARMS_MJ` and
`REVIEW_ARMS_MJ_WORKER` in the dedicated sshd's server-side `SetEnv` as well as
in the invoking shell. The forced command starts the engine independently.

Use a separate transport JSON preconfigured with
`"engine": "rql_mock.stub_engine:load_engine"` for a no-model transport test.
Use both digests from `rql_mock.stub_engine.STUB_IDENTITY` in smoke assignments.
Enrollment preserves the engine selected in the private config and defaults a
new config to `rql_mock.engine:load_engine`.

## Slice 5 session setup

Install `review_arms_rql.config.toml` as
`<profile Codex home>/review_arms_rql.config.toml`. Use
`codex_profile = "review_arms_rql"` on the single private pilot Codex profile.
Install `client.py` and `protocol.py` alongside `ssh_config` before starting
sessions. These files must also be available at the configured path when the
profile discovery probe runs on the host.

| Variable | Value visible inside the session |
| --- | --- |
| `RQL_SESSION_ASSIGNMENT_DIR` | Absolute path to the container-local directory of assignment files, named `<MJ_SESSION_ID>.json`. |
| `RQL_SSH_CONFIG` | Absolute path of the supplied `ssh_config`. |
| `RQL_SSH_KEY` | Absolute path to that writer's private key, readable only by its user (mode 0600 or 0400). |
| `RQL_SSH_KNOWN_HOSTS` | Absolute path to a file with the trusted endpoint's pinned host public key. For a nondefault port the host token is `[host]:port`. |
| `RQL_SSH_TARGET` | `user@host` or `ssh://user@host:port`. Use an address reachable from the writer. |
| `MJ_SESSION_ID` | Mjolnir's full 32-character lowercase hex session ID. The overlay passes this alone as the remote command for enabled writers. |

Create the session without its first model prompt. Use the registered ID
from `mj new` to publish the host assignment and then the minimal per-session
file `<MJ_SESSION_ID>.json` in the container. The file contains, for example,
`{"session_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","arm":"R"}`. Use `none`
for G and reviewers. For R and R-loc, publish their key authorization, host pin
and private key before atomically renaming the local assignment into place.
Keep private keys isolated to their own writer containers. Configure the
sealed Podman environment so that a writer's sole egress exception is the
dedicated SSH endpoint.

The client uses no SSH for an explicit `none` assignment or an absent record.
Both return an empty catalog immediately. Profile discovery with no session ID
also stays local and empty. For a real session whose record has yet to appear,
the client closes after answering the first `tools/list` so Codex can start a
fresh client after the launch hook publishes the assignment.

After publishing and waiting for the session to be ready, run Codex's local
`/mcp reconnect` before the first model prompt and verify the actual thread's
RQL tool count. Require one for R and R-loc and zero for G and reviewers. The
only supported nonempty catalog is `["rql_query"]`. The optional diagnostic
`python3 -B /container/path/client.py --preflight` checks the exact names via
initialize/list/ping in a new process using the same environment and prints
`{"session_id":"<id>","tools":[...]}` on success. The launch check still needs
the live thread's result, since the launcher may publish its assignment after
Codex's first MCP connection.

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
