# Real-harness ACP validation

Issue: <https://github.com/BrokkAi/mjolnir/issues/1129>.

`tests/e2e/acp_real_harness.py` exercises `mj acp` over JSON-RPC stdio with a
real daemon, a real harness, and a Docker or Podman worker. This is an opt-in
acceptance test: it needs a signed-in profile and makes model calls. It is not
part of the offline Cargo suite. Run it on a POSIX host with Python 3.11 or later.

Build the CLI and a portable worker from the same checkout, using the dev
profile (choose `aarch64-unknown-linux-musl` on an ARM host):

```sh
cargo build --bin mj
cargo build --target-dir target/worker --target x86_64-unknown-linux-musl \
  -p brokk-mj-worker --bin mj-worker
python3 tests/e2e/acp_real_harness.py \
  --worker target/worker/x86_64-unknown-linux-musl/debug/mj-worker \
  --profile-home "$HOME/.codex" \
  --engine docker \
  --artifacts /tmp/mj-acp-real-validation
```

The artifact directory must not exist. The configured agent image must already
be installed; `--image` selects it and `--engine podman` selects Podman. A small
public repository, `octocat/Hello-World`, is cloned inside each session's
container. `--repository` can select another accessible GitHub repository.
The test prompts do not change files. `--harness` selects a harness other than
Codex; `--profile-home` must match it. The harness's existing model settings are
used without modification.

Each run creates a named instance and separate configuration, database, and
SessionWiki index under the artifact directory. Every CLI process receives
`--instance` and the explicit directory overrides. The profile is staged by
the normal provisioning path. No default-instance command is run.

The three cases create separate sessions:

1. Ask for a unique marker, assert `session/prompt` returns `end_turn`, and
   verify the marker arrives in an ACP `agent_message_chunk` update.
2. Submit a long prompt, wait until the harness starts a `sleep 120` tool call, send the ACP
   `session/cancel` notification, and assert the outstanding prompt returns
   `cancelled`. Verify the session becomes idle and `mj wait` reports cancelled.
3. Submit another long prompt and kill the consumer process while the session
   is running. That process alone holds the adapter's input/output pipe ends;
   the supervisor stays alive to observe `mj acp` exiting successfully. Verify
   the session becomes idle, its turn is cancelled, and its record remains.
   Suspend it to make a checkpoint and release the container, resume it using
   `mj resume`, and verify another real prompt returns the expected answer.

Every session must appear in `mj sessions --json` and in a SessionWiki search
for its prompt's unique marker. Search is polled because indexing is asynchronous.
The test retains JSON-RPC messages, CLI results, assertion evidence, timestamps,
binary SHA-256 hashes, daemon logs, and per-adapter stderr. It destroys its
sessions and stops its daemon at the end, including after an assertion failure;
cleanup failures are reported. Artifact files remain for diagnosis and review.

Killing the consumer is distinct from killing `mj acp`: a process killed with
SIGKILL cannot perform cleanup. The default `keep` exit policy is used here;
the additional `suspend` and `destroy` policy matrix belongs to #1133 and its
existing adapter tests.

## Recorded execution

The first real session on revision `4205ca80` exposed a startup failure. ACP
`session/new` returned session `24bda8a91ce96f2863446807ff75faa2` at 08:00:47 UTC
on 2026-09-26, while the worker was still provisioning. The immediate prompt
failed at 08:01:47 with `409 Conflict: this session is still starting after 60
seconds`. The container did not start until 08:03:13. Evidence is in
`/tmp/mj-acp-real-1129-run2/transcript.jsonl` and that directory's daemon logs.
Cleanup destroyed the session and stopped the isolated daemon.

The adapter now waits for live readiness before answering `session/new`, with
a ten-minute ceiling. Creation runs as a protocol task so other requests and
consumer EOF are still processed. The admitted id is tracked before this
read-only wait, preserving exit-policy cleanup if the client leaves or launch
fails. Focused regressions cover delayed readiness, concurrent requests,
disconnect, launch failure, a missing session, and the deadline.

The corrected adapter passed all three cases on 2026-09-26, using the Codex
profile's configured `gpt-6-astra` model and a Docker worker. The complete
transcript and daemon logs are retained in `/tmp/mj-acp-real-1129-run3`.
The run used instance `acp-real-868400`, CLI SHA-256
`ca55e68925ab02de0e2860d684030509806ed71a92490ec93ff82adcbc14fb60`, and worker
SHA-256 `d382035540b8335c4a44e714ddb2346e893d2d8edb7b01f11286618d4711c53b`.
The installed image was frozen to
`sha256:4513245a9918993cb10f29f835a52103ac3c678574b3721a0bf32b285fa9b9a4`;
the worker selected its pinned `@brokkai/codex-acp@1.13.2` bridge.

Condensed transcript (UTC; full requests and responses are in `transcript.jsonl`):

```text
08:19:04 complete: session/new requested
08:22:11 complete: session/new -> ab08ce98d5e79b5de2ef19c867fbb145
08:22:20 complete: agent_message_chunk -> ACPREALCOMPLETE868400
08:22:20 complete: session/prompt -> {"stopReason":"end_turn"}
08:22:23 complete: session listed, idle, and searchable for its marker
08:25:01 cancel: session/new -> 99e753eed6b3c372e89e83e18ab90cbe
08:25:12 cancel: sleep 120 in progress; session/cancel sent
08:25:12 cancel: session/prompt -> {"stopReason":"cancelled"}
08:25:16 cancel: mj wait -> cancelled; session listed, idle, and searchable
08:28:25 consumer-death: session/new -> 4d8beb24f129cc8d81983c6e09c0c87f
08:28:36 consumer-death: sleep 120 in progress; consumer killed with SIGKILL
08:28:36 consumer-death: mj acp exited 0
08:28:42 consumer-death: mj wait -> cancelled; session listed, idle, and searchable
08:29:06 consumer-death: suspended after checkpointing
08:29:08 consumer-death: mj resume admitted
08:32:54 consumer-death: mj prompt --wait -> finished
08:32:54 consumer-death: final_message -> ACPREALCONSUMERDEATH868400RESUMED
08:33:44 cleanup: all test sessions destroyed; isolated daemon stopped
```

The supervising Codex server restarted after `mj resume` was admitted, stopping
the Python supervisor. The isolated Mjolnir daemon and its workers survived.
The remaining readiness check, real prompt, and cleanup were continued against
the same instance, and their evidence was appended to the same transcript.
No already-passed case was substituted with a fake or rerun against live data.

`cargo test -p brokk-mjolnir acp::tests` passed all 31 tests. The consumer relay
also passed a one-MiB round trip and an abrupt-consumer-death pipe-closure check.
After merging latest master and repairing its existing CI failures, the full
workspace `cargo test`, `cargo clippy --all-targets -- -D warnings`, and format
checks passed in the dev profile. Logs are retained under
`target/release-validation/`. All six durability crash boundaries and the
browser/TUI convergence scenario with 113 browser checks also passed.
