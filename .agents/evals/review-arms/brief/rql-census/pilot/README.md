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

The pilot runs on a development build of mj, because the installed build
can't serve the engine's host-only Git source to a container (fixed in
`020257d9f`, narrowed in `a90bd7e75`; frozen at merged master `f0beee634`). Frozen copies live outside the checkout, so a later build can't
replace them mid-run: `/mnt/optane/review-arms-pilot/bin/mj-f0beee634` and its
matching worker `mj-worker-f0beee634`. Export both for every pilot command;
`mjx` then runs that binary and passes the worker to any daemon it starts:

```sh
export REVIEW_ARMS_MJ=/mnt/optane/review-arms-pilot/bin/mj-f0beee634
export REVIEW_ARMS_MJ_WORKER=/mnt/optane/review-arms-pilot/bin/mj-worker-f0beee634
```

Use a dedicated `mj --instance rql-census-pilot` configuration at
`~/.config/mjolnir/instances/rql-census-pilot/config.toml`. Configure exactly
one Codex profile, the allotted login, using its existing native home.
Select the `review_arms_rql` overlay on that profile in the private
instance only. The runner selects and verifies Sol/high for writers and
Terra/high for reviewers after launch. Claude engine profiles can also
be enabled. Configure the Podman target with
a pinned image ID and `pull_policy = "never"`. The writer/reviewer bundles
are `mjolnir` and `bifrost-history`. Set the primary repository's destination
in each bundle. For the engine, configure the local Git daemon and bundle
as described in the engine setup. Use its controller-side source descriptor
as the bundle's `local` source:

```toml
[bundles.rql-pilot-engine]
primary_repo = "engine-trees"

[[bundles.rql-pilot-engine.repositories]]
id = "engine-trees"
destination = "engine-trees"
local = "/absolute/path/to/engine-source"
```

The source clone paths default to `~/Projects/mjolnir` and
`/mnt/optane/tmp/rust10/bifrost-src`. Override them with
`--mjolnir-clone` or `--bifrost-clone` when needed. Keep other work out of
this instance. The runner stops its daemon after each run, including a quota
hold. Workers needed to resume an interrupted attempt can remain active.

## N/G smoke after the codex3 reset

From the review-arms directory with the private instance configured for
codex3 and its pinned image, set `RQL_HOOK` to the absolute path of the
transport installer executable and run:

```sh
unset REVIEW_ARMS_INSTANCE
pilot_instance=rql-census-pilot
pilot_config="$HOME/.config/mjolnir/instances/$pilot_instance"
./mjx --instance "$pilot_instance" workspaces create rql-census-pilot --json > "$pilot_config/workspace.json"
./mjx --instance "$pilot_instance" daemon stop
python3 -B pilot_run.py --instance "$pilot_instance" --profile codex3 \
  --rql-hook "${RQL_HOOK:?set the transport installer path}" \
  --manifest brief/rql-census/pilot/manifest.json \
  --menu-steps brief/rql-census/menu-steps.md \
  --diff mjolnir/activity-recorder-kills-api --arm N --arm G --replicate 1 --attest-cache
```

This schedules one Sol writer and four Terra reviewers and requires more
than 26.23% weekly quota at admission. Every writer and reviewer attests
its Git cache before its first model prompt. The engine's smoke does the
equivalent with its orphan commit.

## Per-session RQL assignment

Preinstall the transport client in the pinned image as described in
the RQL transport setup. The `--rql-hook` executable receives one argument,
the path to the host-side `tool-assignment.json`, as soon as the session
ID is available. Its fields are `session_id`, `arm` and `manifest`. For
R and R-loc writers it additionally receives `diff`, `intro` and `log`.
The hook publishes the container-local record with only `session_id` and
`arm` at `$RQL_SESSION_ASSIGNMENT_DIR/<session-id>.json`. For R and R-loc
it stages the bound SSH assignment and key before publishing the local
record. G writers and every reviewer get `arm = "none"` and need no SSH
credentials.

After publishing the record, the hook runs the client with `--preflight`
inside the container and prints its JSON response on stdout. Tools must
be exactly `["rql_query"]` for R and R-loc writers and `[]` for all other
sessions. Use stderr for diagnostics. Repeating an interrupted hook must
be safe. The runner then uses the Codex bridge's local `/mcp reconnect`
command to verify the live thread's tool count. It saves the response and
any reported usage in `mcp-reconnect-result.json` and binds that evidence
to `done.json` by hash. A catalog mismatch leaves the session available
for inspection and triggers a fresh attempt if quota permits.

## Fidelity gate

G, R and R-loc all get `writer-grep.md`, the pinned catalogue and the frozen
semantics for the manifest's `served_steps` alongside `brief/WRITER.md`. G has
no query tool. R and R-loc also get the tool instructions in `writer-rql.md`.
The tool arms require `fidelity/gate-report.json` in the run root to match
the manifest and pass each served added step. N and G can run independently
before that report exists. A failed pooled prescience verdict stops all pilot
launches, including G, and a re-freeze cannot clear it. The private gold cases and pilot bug IDs are
excluded from every prompt.

## Diff reading

Codex cuts one `exec_command` output at the model's 10,000-token policy (about 40,000 bytes) and
keeps only the head and tail, whatever `max_output_tokens` the model asks for. Stage 2a sessions
read the diff in one command, so 25 of its 35 diffs were cut (writers too: 117 of 125 writer
rollouts carry the marker). `--writer-diff-reading` and `--reviewer-diff-reading` choose how each
role reads the diff; they are independent, so a reviewer-only rerun keeps the accepted briefs.

- `single-command` (default) is the Stage 2a prompt, byte for byte, and adds nothing to a job's
  inputs, so the frozen Stage 2a artifacts still verify.
- `bounded-reads-v1` lists numbered `git diff` reads of at most 32,000 bytes each (small files
  batched, large files read in byte windows ending at line boundaries). Together they print the
  whole diff once. Each job's `inputs.json` records `writer_diff_reading` or
  `reviewer_diff_reading` and a digest of the instruction text and budget, and `prompt_sha256`
  covers the rendered list.

`tool_output_token_limit` in the Codex overlay would raise the policy, but it applies to every
session of the profile and the model still chooses `max_output_tokens` (the reviewers chose 30,000,
below the 48,800 tokens of the largest diff), so the harness splits the diff instead.

## Evidence and recovery

The default root is `/mnt/optane/review-arms-pilot`. Under
`runs/<repo>/<diff>/<arm>/r<rep>`, accepted `brief.md` is in `writer/`
and accepted `output.md` is in `reviewers/<slot>/`. Each job saves
`usage.json`, the prompt, model settings, transcript, inventory,
`audit.json` and provenance in `done.json`. The writer's RQL delivery
log is `writer/queries-<session_id>.jsonl`. The audit screens the recorded commands
and arguments. Network egress stays under the target's existing policy.
It cannot establish unobserved activity.

Before teardown, the runner hashes and flushes the stored transcript,
usage with model attribution, output, cache attestation and reconnect
evidence. It audits the stored transcript and destroys the recorded
session ID only once those checks pass. A capture or check failure
leaves the session in the private instance and records its ID and the
reason in `failed.json`.

Rerun the same selection after a quota hold or interruption. The active
diff takes priority. A failed attempt's evidence moves to a sibling
`<job>.superseded-<n>` directory before one fresh attempt, subject to a
quota recheck. If that attempt fails, the run stops before any later diff
and reports the retained sessions and their receipt paths. Completed
attempts whose inputs changed use the same archive layout. A launch intent
without a returned session ID is ambiguous. Reconcile it before starting a
replacement.

A writer's pending question holds the diff on its recorded session. After
answering it, rerun the same selection to collect the original turn.

Use `pilot_readout.py --archived` with the judge key, judgments, delivery logs,
manifest and label snapshot when the original outputs are no longer present.
Judge preparation freezes the isolation findings and the attempt set in the
key. If the engine probe wrote outside the run root, pass `--egress-log` to
the first judge preparation. Later judge commands reuse the frozen egress
records. An explicit different path replaces them and records the previous
source in the key. The default readout checks current outputs for staleness
and recomputes isolation from the selected runs and the current probe log.
