# Run the registered non-leaking oracle arm (ON) and round M

This ExecPlan follows `.agents/PLANS.md`. Registration: `.agents/evals/review-arms/brief/PLAN-bifrost-oracle-noleak.md`, copied verbatim from the supervisor's plan (sha256 728b4f5b…) at 2383bf474, with the edit-scope clarification at b2e18f62b, both before any edit. The predecessor arm is `bifrost-oracle-arm.md`, whose reading (Facts carry, upper bound) is in `brief/bifrost-oracle/READOUT.md`.

## Purpose / Big Picture

Round L's oracle lift sat entirely on briefs that leaked the defect. This arm asks whether the decisive fact still lifts Terra/high reviewers when it is stated so that two Opus/high leak judges both read the brief as `missed`. The result is ON − A on the diffs whose brief passes, with ON − OT showing how much of round L's lift came from leaking wording.

## Progress

- [x] Plan registered, and the scope clarification recorded before any edit.
- [x] Harness generalized (b439150b6): `brief_oracle.py noleak-*` steps, `brief_writer_terra.py --kind oracle-noleak --round 1..3`, driver arm ON, brief_score reading `bifrost-oracle-noleak`. Existing offline tests pass.
- [x] Round 1 (2026-11-18): 17 edits, all fact-checked and all passing both Opus leak judges; rounds 2–3 not needed. Diagnostics: Opus flags 15/17 OT briefs, Sol passes 17/17 ON briefs.
- [x] Published all 17; ON/AF jobs interleaved (34).
- [x] ON/AF reviewers ran 05:25–06:02Z on one pinned image; 102 handbacks; 11 reviewers drawn to codex4.
- [x] Round M: 204 outputs, judges agreed on all, 67 hand-checked; Facts carry (ON − AF +37.3, +17.6 to +58.8).

## Decision Log

- Readings confirmed by the supervisor: round 1 edits all 17 diffs; each round runs edit → blind fact check → paired leak check; a fact-check rejection uses up the round, and the next round edits the last accepted text; the editor sees the previous check's reasons, and round 1 sees the stored OT leak verdicts of the brief it edits; one blind fact-checker session per round.
- Clarification (supervisor, recorded in the plan): edits may touch items the leak judges cite, not only carrying items. No item is deleted, and anchors are unchanged (the collector enforces the same line count, unchanged anchors per edited line, and lint).
- A fact-checker quote must be a whole line of the hashed brief; only accepted texts become `texts/<diff>/round-N.md`, the only text a leak round can judge.
- Round M reuses `brief_score.py` with `--arms ON,AF,OT --reading bifrost-oracle-noleak --reference-round bifrost-judge-l`, and `verify_driver.py` judge and task modes with `--model opus --profiles claude7`.

## Context and Orientation

ROOT is `/mnt/optane/review-arms-bifrost-20261116`. Work: `ROOT/briefs/bifrost-oracle-noleak-work/round-N/{edit,check}`, `texts/`, `publication.json`. Leak rounds: `ROOT/briefs/bifrost-oracle-noleak-leak-N/` with records in `ROOT/briefs/bifrost-oracle-noleak-leak-records/`. Published briefs: `ROOT/briefs/oracle-noleak/<diff>/brief.md`. Stage records: `ROOT/oracle-noleak-arm/`, mirrored under `brief/bifrost-oracle-noleak/` in the repository. The environment is `ROOT/arms/environment.json`, with `REVIEW_ARMS_QUOTA_FLOOR=claude7=25` for Claude sessions and a Codex profile at 25% or more for reviewers.

## Concrete Steps

For N in 1..3, with `T="--census ROOT/census-results/missing-facts.jsonl --bugs-from ROOT/oracle-arm/jobs-ot.json --name bifrost-oracle-noleak"`:

    brief_oracle.py noleak-edit-build --round N $T
    verify_driver.py ROOT/briefs/bifrost-oracle-noleak-work/round-N/edit --kind task --output edit.json --profiles claude7 --model opus --target podman --bundle bifrost-dev --repo-commit 35933693…
    brief_oracle.py noleak-edit-collect --round N $T
    brief_oracle.py noleak-check-build --round N $T
    verify_driver.py ROOT/briefs/bifrost-oracle-noleak-work/round-N/check --kind task --output factcheck.jsonl (same options)
    brief_oracle.py noleak-check-collect --round N $T
    brief_writer_terra.py leak-build --kind oracle-noleak --brief-root ROOT/briefs/bifrost-oracle-noleak-work/texts --bugs-from ROOT/oracle-arm/jobs-ot.json --name bifrost-oracle-noleak --round N
    verify_driver.py ROOT/briefs/bifrost-oracle-noleak-leak-N --kind judge (same options)
    brief_writer_terra.py leak-collect (same flags as leak-build)

Then `brief_oracle.py noleak-publish $T --jobs-out ROOT/oracle-noleak-arm/jobs-on-af.json` (ON and AF interleaved per diff), then `driver.py --jobs` on that file with `--same-image ROOT/oracle-noleak-arm/image-pin.json`, then round M with `brief_score.py build --arms ON,AF,OT --models gpt-6.1-terra --bugs-from <jobs-on-af> --reading bifrost-oracle-noleak --reference-round bifrost-judge-l`.

ON − OT is a historical comparison: ON and OT ran in different review windows, and only ON has a same-window control (AF). The readout reports ON − OT as wording plus any window or cohort difference, and plus image if the pinned ON/AF image differs from the OT carriers' (`89128665…`), never as wording alone.

## Validation and Acceptance

Every retained brief's final text is bound by hash to an accepted fact-check decision and to a leak round in which both judges said `missed`. Round M covers ON, A and OT on exactly the retained diffs, 3 sessions each. The readout follows the registered reading and lists every excluded diff with its exclusion round and its round L OT − A.

## Idempotence and Recovery

Each build step refuses to overwrite an existing round directory, and each collect step validates every unit before writing. verify_driver's session records make a model step resumable without a second session per unit.

## Outcomes & Retrospective

Facts carry: ON 45.1%, AF 7.8%, A 17.6%, OT 45.1%. Round L's lift survives removing the leak. The same-window control matters, because the fresh no-brief cohort caught 9.8 points less than the stored one. Readout: `.agents/evals/review-arms/brief/bifrost-oracle-noleak/READOUT.md`.
