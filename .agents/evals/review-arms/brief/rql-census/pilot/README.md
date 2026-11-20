# RQL census pilot launch

Run `pilot_run.py` from the review-arms directory. It reads the frozen
`brief/rql-census/pilot/manifest.json` and finishes a diff's writers before
its reviewers. The runner reserves the missing jobs' estimated cost against
the selected login's weekly quota before starting or resuming the diff.
It stops before a diff that would leave 25% or less.

The pilot launches reviewers as independent sealed sessions. Native
review children lose their transcripts at teardown, which prevents a complete
transcript audit. Each Terra/high session receives the native review rubric
and the native review prompt rendered with the introducing diff and, for a
brief arm, the matching brief. The session's final assistant response is the
review output. The pilot records its full transcript and audits it before
publishing the output.

## Private instance

Use a dedicated `mj --instance rql-census-pilot` configuration at
`~/.config/mjolnir/instances/rql-census-pilot/config.toml`. Enable the allotted
Codex login and its Sol-only alias when running R or R-loc. The alias uses the
same Codex home, selects Sol as the overlay's default model and adds the
`rql` MCP server. Use an ordinary profile without RQL for G and all reviewers.
Claude engine profiles can also be enabled. Configure the Podman target with
a pinned image ID and `pull_policy = "never"`. The writer/reviewer bundles
are `mjolnir` and `bifrost-history`. Set the primary repository's destination
in each bundle. For the engine, configure the local Git daemon and bundle
as described in the engine setup. The bundle in this instance is:

```toml
[bundles.rql-pilot-engine]
primary_repo = "engine-trees"

[[bundles.rql-pilot-engine.repositories]]
id = "engine-trees"
destination = "engine-trees"
github = "git://127.0.0.1:<engine-git-port>/engine-trees.git"
```

The source clone paths default to `~/Projects/mjolnir` and
`/mnt/optane/tmp/rust10/bifrost-src`. Override them with
`--mjolnir-clone` or `--bifrost-clone` when needed. Keep other work out of
this instance. The runner stops its daemon after each run, including a quota
hold. Workers needed to resume an interrupted attempt can remain active.

## N/G smoke after the codex3 reset

From the review-arms directory with the private instance configured for
codex3 and its pinned image, run:

```sh
unset REVIEW_ARMS_INSTANCE
pilot_instance=rql-census-pilot
pilot_config="$HOME/.config/mjolnir/instances/$pilot_instance"
./mjx --instance "$pilot_instance" workspaces create rql-census-pilot --json > "$pilot_config/workspace.json"
./mjx --instance "$pilot_instance" daemon stop
python3 -B pilot_run.py --instance "$pilot_instance" --profile codex3 \
  --manifest brief/rql-census/pilot/manifest.json \
  --menu-steps brief/rql-census/menu-steps.md \
  --diff mjolnir/activity-recorder-kills-api --arm N --arm G --replicate 1 --attest-cache
```

This schedules one Sol writer and four Terra reviewers and requires more
than 26.23% weekly quota at admission. The smoke saves a Git cache
attestation from inside one writer and one reviewer before their first
prompts. The engine's smoke does the equivalent with its orphan commit.

## R and R-loc

Use `--rql-profile` with the configured Sol-only alias and `--rql-hook`
with the transport installer executable. The installer receives one argument,
the path to the writer's host-side `tool-assignment.json`. Its fields are
`session_id`, `diff`, `intro`, `arm`, `manifest` and `log`. It must
install the bound SSH assignment, publish the writer-specific key and run
the transport client's `--preflight` from inside the writer. Return its
JSON result on stdout, including the writer ID and exactly
`{"tools":["rql_query"]}` as the tool list, and use stderr for diagnostics.
An interrupted hook must be safe to repeat. The RQL transport setup
describes the required profile environment.

G uses `writer-grep.md`, appended to the existing `brief/WRITER.md` prompt.
R and R-loc get that same checklist plus `writer-rql.md`, the pinned
catalogue and the frozen menu semantics. The private gold cases and pilot
bug IDs are excluded from every prompt.

## Evidence and recovery

The default root is `/mnt/optane/review-arms-pilot`. Under
`runs/<repo>/<diff>/<arm>/r<rep>`, accepted `brief.md` is in `writer/`
and accepted `output.md` is in `reviewers/<slot>/`. Each job saves
`usage.json`, the prompt, model settings, transcript, inventory,
`audit.json` and provenance in `done.json`. The writer's RQL delivery
log is `writer/queries.jsonl`. The audit screens the recorded tool calls
and output and leaves network egress under the target's existing policy.
It cannot establish unobserved activity.

Rerun the same selection after a quota hold or interruption. The active
diff takes priority. A terminal rejected or void attempt, and a completed
attempt whose inputs changed, are retained in a sibling
`<job>.superseded-<n>` directory before the next attempt. Inspect
`failed.json` and `audit.json` there for a void reason. A launch intent
without a returned session ID is ambiguous. Recover or retire that session
before starting a replacement. The title suffix is derived from the full
job key and attempt number.
