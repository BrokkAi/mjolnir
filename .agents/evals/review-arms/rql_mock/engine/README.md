# Pilot RQL engine

Set the SSH server's `engine` to `rql_mock.engine:answer`. Keep the engine
configuration, source Git repositories, manifest, cache and audit files on the
host. The engine keeps one Fable/high session per introducing commit across
queries, writer arms and replicates. It serializes turns for that session, clears
the native conversation before each query or repair, and drops the previous
turn's workspace artifacts. It replays validated results for repeat queries and
checks locations and copied byte slices against the pinned host Git tree.

## Private study instance

The host entry point reads `/mnt/optane/review-arms-pilot/engine.json`. Fill in
the study's actual named instance, profile, source clones and manifest. Keep
the manifest's relative paths intact. The engine verifies the catalogue, menu
semantics, menu and eligible-label sidecar against the manifest's SHA-256 pins
before serving any answer. Only the menu steps in `served_steps` are available.
The sample port is shared with the bundle configuration.

```json
{
  "instance": "rql-pilot",
  "profile": "<allotted Claude profile>",
  "manifest": "/absolute/path/to/review-arms/brief/rql-census/pilot/manifest.json",
  "repos": {
    "mjolnir": "/absolute/path/to/mjolnir",
    "bifrost-dev": "/absolute/path/to/bifrost-dev"
  },
  "snapshot_repo": "/mnt/optane/review-arms-pilot/engine-trees.git",
  "git_port": 9438,
  "bundle": "rql-pilot-engine",
  "destination": "engine-trees",
  "seal_probe": ["/absolute/path/to/operator-provisioned-engine-isolation-check"]
}
```

Use the same named instance and local Podman target as the pilot writers. Add
this bundle to the private instance's configuration.

```toml
[bundles.rql-pilot-engine]
primary_repo = "engine-trees"

[[bundles.rql-pilot-engine.repositories]]
id = "engine-trees"
destination = "engine-trees"
github = "git://127.0.0.1:9438/engine-trees.git"
```

Engine egress policy must allow only the Claude provider transport. Writer
sessions' RQL SSH exception must be unavailable to the engine. Provision the
target's policy and supply a trusted host command in `seal_probe`. The command
receives the actual Podman container ID and must check its effective egress and
mount policy, including that it cannot reach the host Git listener or access
other workspaces, real history, source mounts or census material. Its exit
status controls whether the engine may send its first prompt. The built-in
checks also verify a clean singleton checkout and the exact tree hash. They
unpack and enumerate the mounted cutoff bundle and test the Git daemon's port
from inside the session. Mjolnir must prepare this cutoff bundle on the host,
where loopback Git is reachable.

## Snapshots and run lifecycle

From the review-arms directory, prepare the Git repository before starting
the listener. Preparation packs the tree and blobs for each introducing commit
in the frozen eligible-label sidecar. It creates a parentless commit and records
the mapping in `engine-trees.json`. Verification checks refs, tree equality
and every commit object, including unreachable ones. The SSH transport still
restricts each writer to its assigned pilot snapshot. Other eligible snapshots
are available through the host fidelity path.

```sh
python3 -B -m rql_mock.engine prepare
rql_mock/engine/start-git-daemon.sh
# Run the pilot while the read-only, loopback Git listener is available.
python3 -B -m rql_mock.engine teardown
rql_mock/engine/stop-git-daemon.sh
```

The listener serves only `engine-trees.git` and uses
`engine-git-daemon.pid` and `engine-git-daemon.log` alongside the bare
repository. Run it only during pilot work and stop it after the run. For
staging tests, put the staging review-arms directory first on `PYTHONPATH`,
then the real review-arms directory. For a staged smoke also set
`REVIEW_ARMS_SCRIPTS` to the real review-arms directory for `mjx`.

The engine uses the writer's sealed launch function. With the sample bundle
the exact command is

```sh
mjx --instance "$instance" new --workspace-id "$workspace_id" --target podman \
    --bundle rql-pilot-engine --at "$orphan" --base "$orphan" --no-later-history \
    --profile "$claude_profile" --effort high --review off --no-project-memory \
    --no-github-events --no-github-credentials --subagents none --oracle disabled \
    --title "RQL pilot engine $intro_prefix" --json
```

The launcher sends no prompt. Before the first turn the engine sets and
verifies `model=fable` and `effort=high`, then checks the concrete Fable model
in the provider's reported usage against `model_versions.fable_provider_id`.
A synthetic prior-query recall probe runs on first use and is recorded under
`context_probe`. A failed probe prevents serving answers from the session. Each
query and repair then waits for the command-specific `context-cleared` boundary
and records its own context ID. The agent receives the catalogue, the served
menu's semantic sections and the query. The worked examples, coverage ledger,
manifest and census labels stay on the host.

The per-commit cache and session records live under
`/mnt/optane/review-arms-pilot/engine-cache/`. A session with an uncertain launch
record needs reconciliation with the private instance before another launch.
The same session serves cache misses while it remains live. An isolation audit
failure moves that session's evidence to `retired/<session>/`. The engine
captures the whole audit transcript, usage with reported provider model IDs,
and all answer attempts, including rejected output. It audits the saved
transcript and records SHA-256 for each captured file in `capture.json` before
requesting destruction of the recorded session and retrying once in a new
sealed session. If capture fails, the container remains available for
recovery.

After writer and fidelity requests have finished, use `teardown` to apply the
same process to the remaining engine sessions in the named study instance.
It reports capture failures and exits nonzero, leaving those sessions intact.
The receipt records Mjolnir's acceptance of destruction. An interrupted
destroy request with no receipt requires reconciling the recorded request
with the study instance before trying to destroy that session again.

For the host fidelity stability check, call
`rql_mock.engine.answer(commit, query, fresh=True)`. A successful re-ask records
`fresh: true` and its independently confirmed context ID, and stores the result
under `stability/<query-cache-key>/<context-id>.json`. Writer requests cannot
select a fresh re-ask or change the shared cached answer.
