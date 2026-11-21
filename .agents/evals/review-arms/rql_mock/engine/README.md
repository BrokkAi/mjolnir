# Pilot RQL engine

Set the SSH server's `engine` to `rql_mock.engine:load_engine`. Keep the engine
configuration, source Git repositories, manifest, cache and audit files on the
host. Each uncached query launches a sealed Sonnet/high session that also
handles its repairs. R and R-loc share cached answers. The host checks sites
and copied bytes against the frozen tree and runs the verifier for added steps.

## Private study instance

The host entry point reads `/mnt/optane/review-arms-pilot/engine.json`. Fill in
the study's named instance, profiles, source clones and manifest. The engine
keeps its cache beside this file. For an isolated smoke, set
`REVIEW_ARMS_ENGINE_ROOT=/mnt/optane/review-arms-pilot-smoke` for the host
answer function and lifecycle commands and put the smoke's `engine.json`
there. An explicit `--config` also selects the cache beside that file for
teardown. Keep smoke data out of the pilot run root.

Keep the manifest's relative paths intact. The engine verifies the catalogue, menu
semantics, menu and eligible-label sidecar against the manifest's SHA-256 pins
before serving any answer. Only the menu steps in `served_steps` are available.
The sample port is shared with the bundle configuration.

```json
{
  "instance": "rql-census-pilot",
  "profile": "claude5",
  "engine_profile": "claude5-engine",
  "manifest": "/absolute/path/to/review-arms/brief/rql-census/pilot/manifest.json",
  "repos": {
    "mjolnir": "/absolute/path/to/mjolnir",
    "bifrost-dev": "/absolute/path/to/bifrost-dev"
  },
  "snapshot_repo": "/mnt/optane/review-arms-pilot/engine-trees.git",
  "verifier": "/mnt/optane/review-arms-pilot/bin/rql-verify-<rev>",
  "git_port": 9438,
  "bundle": "rql-pilot-engine",
  "destination": "engine-trees",
  "seal_probe": ["python3", "-B", "/absolute/path/to/rql_mock/engine/record_egress.py", "--out", "/mnt/optane/review-arms-pilot/egress.jsonl"]
}
```

Use `claude5` for the existing login, quota checks and fidelity adjudicator.
Add `claude5-engine` only to the private instance's
`~/.config/mjolnir/instances/rql-census-pilot/config.toml`:

```toml
[profiles.claude5-engine]
kind = "claude"
home = "/home/jonathan/.config/mjolnir/instances/rql-census-pilot/claude-engine"
```

From the review-arms directory, create the private profile with the engine
settings:

```sh
engine_home="$HOME/.config/mjolnir/instances/rql-census-pilot/claude-engine"
install -d -m 700 "$engine_home"
python3 -B -m rql_mock.engine.verify_tools --settings > "$engine_home/settings.json"
```

At every engine launch the runner copies the current claude5 access credential
into the private profile without its refresh token. It requires enough
validity for a full turn and uses the original claude5 five-hour and weekly
windows for all engine admission checks. The frozen worker stages the settings
for that engine session. The runner verifies the staged tool policy before
prompting and checks every recorded tool call before accepting an answer.

The SDK verification uses a loopback-only network namespace and dummy
credentials. Pass the pinned SDK's `sdk.mjs` path to
`python3 -B -m rql_mock.engine.verify_tools` to check outbound tool schemas
and outside-checkout file-tool rejection.

Use the same named instance and local Podman target as the pilot writers.
Create a controller-side Git source descriptor and configure its remote:

```sh
engine_source="$HOME/.config/mjolnir/instances/rql-census-pilot/engine-source"
git init --template= "$engine_source"
git -C "$engine_source" remote add origin git://127.0.0.1:9438/engine-trees.git
```

Set the bundle's `local` to that descriptor's absolute path. The descriptor
only supplies the network remote for the isolated checkout.

```toml
[bundles.rql-pilot-engine]
primary_repo = "engine-trees"

[[bundles.rql-pilot-engine.repositories]]
id = "engine-trees"
destination = "engine-trees"
local = "/absolute/path/to/engine-source"
```

The engine measures egress with `record_egress.py` before the first prompt.
The probe receives the actual Podman container ID and records which
non-provider hosts it can reach. The engine checks a clean singleton checkout
and the exact tree hash. Its built-in checks
unpack and enumerate the mounted cutoff bundle and test the Git daemon's port
from inside the session. Mjolnir must prepare this cutoff bundle on the host,
where loopback Git is reachable.

Mjolnir build `2.73.0+8d8e2db1` only prepares host Git cache snapshots for
recognized GitHub sources. The loopback repository needs host-side cutoff
bundle support for non-GitHub sources before this launch can succeed.

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

The launcher sends no prompt. The engine checks the advertised Sonnet version,
high effort, and the provider model ID recorded in usage against the manifest.
Each paid turn requires more than 25% of the overall weekly quota and more than
20% of the five-hour quota. The agent receives the catalogue, the served menu's
semantic sections and the query. The worked examples, coverage ledger, manifest
and census labels stay on the host.

Set `verifier` to the absolute path of the frozen executable. Its SHA-256
participates in the engine-inputs hash. The engine records candidates, errors,
and unchecked items in `verifier.json`. Repairs continue while the normalized
errors strictly shrink. Exclusions stay in the host evidence.

The per-commit cache and session records live under
`/mnt/optane/review-arms-pilot/engine-cache/`. A session with an uncertain launch
record needs reconciliation with the private instance before another launch.
The engine moves each completed session under `retired/<session>/`, with its
transcript, usage and answer attempts. It audits the saved transcript and
records SHA-256 for the captured files in `capture.json` before requesting
destruction. An isolation failure allows one replacement session. A capture
failure preserves the container for recovery.

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
