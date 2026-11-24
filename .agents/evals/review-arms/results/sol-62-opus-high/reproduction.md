# Reproduce the Sol 6.2 expansion and Opus verification

Run from `.agents/evals/review-arms` with the installed Mjolnir CLI and the
existing default-instance study workspaces.

```sh
set -euo pipefail
export REVIEW_ARMS_DIR=/mnt/optane/review-arms-20261113
export REVIEW_ARMS_INSTANCE=default
export REVIEW_ARMS_CORPUS="$REVIEW_ARMS_DIR/corpus/candidates-all.jsonl"
export REVIEW_ARMS_DIFFS="$REVIEW_ARMS_DIR/corpus/diffs.jsonl"
unset REVIEW_ARMS_MJ REVIEW_ARMS_MJ_WORKER REVIEW_ARMS_SCRIPTS REVIEW_ARMS_REPO
unset REVIEW_ARMS_BUNDLE REVIEW_ARMS_REPO_DIR
R=$REVIEW_ARMS_DIR
C=$R/sol-62-repeats
S=results/sol-62-opus-high
```

The saved `jobs.json` uses the original 70 O jobs in their saved order.
For the zero-based diff index `i`, it emits four new jobs, using arm
`"QRST"[(i+n)%4]` for each `n` from 0 through 3. Q and S select Sol 6.2 low
and R and T select Sol 6.2 medium. `canaries.json` selects Q and R on
`diff-05bc8c1e68` from that manifest. `image.json` matches the original O
pin. The first run captured the live user prompt for each canary with
`mjx transcript --session "$child" --after-seq 0 --limit 20 --role user --json`
into `prompt-Q.json` and `prompt-R.json`.

```sh
set -euo pipefail
python3 -B driver.py --jobs "$C/canaries.json" --concurrency 2 --same-image "$C/image.json" --stop-on-failure
python3 -B "$S/audit_reviewers.py" "$R" --canaries > "$C/canary-audit.json"
python3 -B driver.py --jobs "$C/jobs.json" --concurrency 16 --same-image "$C/image.json"
python3 -B "$S/audit_reviewers.py" "$R" --hash-file "$C/new-reviewers.sha256" > "$C/reviewer-audit.json"
```

The full driver skips completed jobs, including both canaries. The recovered
historical cohort is in the campaign's `preflight` directory with its frozen
job manifest, session manifest and checksums.

After the reviewer driver exits successfully, use the canonical gated
verification and analysis entrypoints in the same shell:

```sh
set -euo pipefail
bash "$S/run_verification.sh"
bash "$S/run_analysis.sh"
```

The verification script prepares or validates the ready and reviewer hash
manifests, checks the exact cohort and raw-source hashes, builds and audits
the blinded pool, then runs batch 0. It requires a successful command-specific
Opus 5.5/high provider audit before launching the remaining batches at 16.
The full invocation reuses the attested batch 0 output. Every batch and the
frozen input/pool hashes must pass audit before the script scores the round.
A failed canary, audit, or verification command stops this sequence before
later judging, scoring, or analysis.

The analysis script uses the fresh Opus groups and complete session key,
preserves the historical sample caps, and allows three samples per Sol 6.2
effort. It computes the frontier and 20,000-resample paired comparisons for
each impact tier. DSF/low stays in judging but its partial coverage excludes
it from the primary 70-diff frontier. Outputs are saved under `verify9` and
`sol-62-repeats`.

These scripts own the operational gates. Once verify9 exists, diagnose an
interruption and resume only the corresponding validated stage, preserving
the original private key and batches. The ordered recovery commands in the
[ExecPlan](../../../../plans/review-sol-62-opus-high.md) use shell error exit
and the same audits; direct judge or score commands are not an alternative
to those gates.

## Missing capture evidence

`done.json` records a collected stochastic review, while the campaign audit
separately validates its measurement evidence. Rerunning the reviewer driver
skips a completed slot and cannot recreate a missed live observation. Preserve
the slot's handback, reviewer/command IDs, selection, usage and capture files;
recover only facts supported by the existing authoritative records. Do not
substitute session creation time for a missing prompt clock or silently run
another reviewer to manufacture an observation.

If required evidence cannot be recovered, stop before judging and record the
gap for an explicit methodology decision by the campaign owner. Keep the
other completed reviews. A missing observation is not an empty finding set,
and the strict audit never silently drops that sample or accepts a fabricated
measurement.
