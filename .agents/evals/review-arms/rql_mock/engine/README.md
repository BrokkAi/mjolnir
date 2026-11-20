# Pilot RQL engine

Set the SSH server's `engine` to `rql_mock.engine:answer`. Keep the engine
configuration, source Git repositories, manifest, cache and audit files on the
host. The engine keeps one Fable/high session per introducing commit across
queries, writer arms and replicates. It serializes turns for that session,
replays validated results for repeat queries, and checks reported locations and
copied byte slices against the pinned host Git tree before publishing an answer.

## Private study instance

The host entry point reads `/mnt/optane/review-arms-pilot/engine.json`. Fill in
the study's actual named instance, profile, source clones, manifest and resolved
Fable version. The sample port is shared with the bundle configuration.

```json
{
  "instance": "rql-pilot",
  "profile": "<allotted Claude profile>",
  "model_version": "claude-fable-5-1",
  "manifest": "/mnt/optane/review-arms-pilot/manifest.json",
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
the listener. Preparation packs only each introducing commit's tree and blobs,
creates a parentless commit and records the mapping in `engine-trees.json`.
Verification checks the manifest refs, tree equality and every commit object,
including unreachable objects.

```sh
python3 -B -m rql_mock.engine prepare
rql_mock/engine/start-git-daemon.sh
# Run the pilot while the read-only, loopback Git listener is available.
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
in the provider's reported usage. The agent receives the catalogue, the menu's
semantic sections and one query at a time. The worked examples, coverage ledger,
manifest and census labels stay on the host.

The per-commit cache and session records live under
`/mnt/optane/review-arms-pilot/engine-cache/`. A session with an uncertain launch
record needs reconciliation with the private instance before another launch.
The same session serves cache misses while it remains live. Isolated cutoff
sessions cannot be resumed. Keep them live while their diffs are in progress.
