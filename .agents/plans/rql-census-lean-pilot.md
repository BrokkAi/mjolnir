# Build the RQL census lean pilot (Stage 2)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log` and `Outcomes & Retrospective` must be kept up to date as work proceeds, following `.agents/PLANS.md`.

## Purpose / Big Picture

The study plan `.agents/evals/review-arms/brief/PLAN-rql-census.md` asks whether a suitably enhanced RQL helps a brief writer, and through its brief a reviewer, catch relation bugs. Its Stage 2 pilot was registered in a lean form on 2026-11-20 (the "Lean pilot" section of that plan, which governs the pilot): arms N, G, R and R-loc on 35 development relation diffs, a mock RQL engine that answers by documented semantics under one display budget, and Fable/high judges. This ExecPlan builds the harness that runs it. When it's done, one command runs the frozen job matrix diff by diff: for each admitted diff, its writers and then its reviewers, so diffs complete whole. A second command judges completed diffs, and a third computes the readout comparing R against G and R-loc against R. Each command is resumable and records usage.

The study plan says *what* is measured and *why*. This ExecPlan says how the harness is built. Where they differ, the study plan wins.

## Progress

- [x] Slices 1 to 6 built and integrated (2026-11-20): selection and freeze (`brief/rql-census/pilot/select_pilot.py`), menu-step semantics (`menu-steps.md`, with worked cases kept apart in `menu-steps-cases.md`), the mock engine (`rql_mock/engine/`), the transport (`rql_mock/server.py`, `client.py`), the launch harness (`pilot_run.py`, `pilot_review.py`) and judging and readout (`pilot_judge.py`, `pilot_readout.py`), plus the fidelity gate (`rql_mock/fidelity*.py`).
- [ ] Re-freeze the manifest under the plan commit that adds pooled prescience, and commit it before any writer runs.
- [ ] Deploy the private instance `rql-census-pilot` and run the engine-only smoke on Fable.
- [ ] Run the live fidelity gate and commit `fidelity/gate-report.json`'s summary; re-freeze with `--served-steps` if a step fails.
- [ ] Smoke: one diff end to end, on codex3 just after its reset. codex2 (22%) is below the study's 25% floor.
- [ ] Launch: on codex3 after its reset (about 2026-11-22 14Z), then codex4 after its reset (about 2026-11-24 15Z), both above the 25% floor, running whole diffs in turn.

## Surprises & Discoveries

- 2026-11-20: existing tooling covers most of the harness (map: `.mj/agents/256b90931c859e1110251b8e383011ba/pilot-tooling-map.md`). Existing run paths key only by diff and arm, so the second briefs would collide; replicate is now part of every key.

## Decision Log

- 2026-11-20: engine and judges run on Fable/high, in pilot and test alike, so menu steps are qualified on the model that serves them (study plan, Lean pilot).
- 2026-11-20: the pilot cohort is the 54 eligible development relation diffs in mjolnir (26) and bifrost-dev (28), the registered development-only pilot repos. The 28 outside development bugs feed only the menu.
- 2026-11-20: the pilot runs on a frozen development build of mj in its private instance (`/mnt/optane/review-arms-pilot/bin/`), because the installed build couldn't serve the engine's host-only (loopback) Git source to a container. Fixed in mj by 020257d9f and narrowed by a90bd7e75: only host-only sources require the host snapshot. Re-freeze the binaries at the reviewed commit before launch.
- 2026-11-20: Fable quota, not Codex, is the binding limit. Engine smoke on claude5: one Fable/high answer took 100-110 s, 320K-365K tokens, about 5-6 points of the 5-hour window and about 0.4 weekly points (coarse). The estimated need is 330-770 answer-equivalents against about 110 available above the floor that day. Coordinator ruling: no design change (no cheaper engine, no smaller gate, no paid API). Codex work (N and G writers, their reviewers, the gate's Terra enumerations) starts at codex3's reset (about 11-22 14Z). The Fable work (gate, R and R-loc) starts at claude4's reset (about 11-23 05Z) on claude4, claude5 and claude7. Judging follows later resets, through about 11-27.
- 2026-11-20: coordinator ruling: every Fable launch and paid engine turn holds while the login's 5-hour window is at or below 20%, besides the 25% weekly floor. One engine answer costs about 5-6 points of the 5-hour window, so the floor leaves room for other sessions sharing the login. A hold reads the daemon's quota report and resumes when a later check finds the window recovered (no timers). A provider quota refusal is a hold, not an engine error, and the writer attempt that met it is interrupted, not scored.

## Outcomes & Retrospective

(none yet)

## Context and Orientation

All tooling lives in `.agents/evals/review-arms/`. Run data goes under `/mnt/optane/review-arms-pilot/`, never in the repository; manifests, prompts and readouts are committed. Reused pieces, from the tooling map:
- `brief_driver.py`: isolated Sol writer launch (Podman, intro/base checkout, no later history), lint and repair, brief and usage artifacts, recovery markers.
- `driver.py`: reviewer launch on a brief in carrier context, explicit reviewer rosters, handback and usage capture, resume.
- `oz/`: the Wizard-of-Oz pattern for serving a tool to a sealed session over SSH (forced command, JSON-RPC, a dedicated key, strict known_hosts, host-side logs).
- `brief_score.py` and `judge/SPEC.md`: blinded mixing behind opaque IDs, plus a one-defect judge. `brief_writer_terra.py`: hash-bound leak and containment scaffolding.
- `census_label.py` and `census_menu.py`: the current-label reader.

Terms: an **arm** is one condition (N, G, R, R-loc). A **replicate** is an independent brief from the same arm on the same diff (1, or 2 on the replicate subset). A **job** is one writer or reviewer session in the matrix.

## Interfaces (fixed here, so the slices can be built in parallel)

**Manifest**, `brief/rql-census/pilot/manifest.json`, written by Slice 1 and read by every other slice:

    {"version": 1, "seed": 20261120, "plan_commit": "<sha of PLAN-rql-census.md at freeze>",
     "inputs": {"eligible_labels_sha256": "<eligible-labels.jsonl: all 54 eligible labels the draw reads>",
                "labels_snapshot_sha256": "<labels-snapshot.jsonl: the 35 chosen labels the readout reads>",
                "plan_sha256": "<PLAN-rql-census.md at plan_commit>", "menu_sha256": "...", "catalogue_sha256": "...",
                "steps_sha256": "<menu-steps.md, semantics only>"},
     "models": {"writer": "gpt-6.1-sol/high", "reviewer": "gpt-6.1-terra/high", "engine": "fable/high", "judge": "fable/high"},
     "model_versions": {"fable": "Fable 5.2"},
     "page_rows": 50,
     "diffs": [{"key": "<repo>/<id>", "repo": "...", "id": "...", "intro": "<sha>", "base": "<sha>",
                "label_line": N, "replicates": {"G": 1|2, "R": 1|2, "R-loc": 1}}],
     "jobs": [{"job": "<key>/<arm>/r<rep>/writer" | "<key>/<arm>/r<rep>/reviewer-<slot>",
               "diff": "<key>", "arm": "N|G|R|R-loc", "replicate": 1|2, "kind": "writer|reviewer", "slot": 1|2|null}]}

Selection: sort eligible keys, seeded shuffle within each repo, take 17 from mjolnir and 18 from bifrost-dev (proportional to 26:28, rounded to 35). Seeded draws, also within repo, pick the 10 replicate diffs (5 and 5). The script records the eligible pool, its hash and every draw.

**Served steps and the fidelity gate**: the manifest's `served_steps` lists the added menu steps the engine and writers serve (by default all three). The engine's registry and prompt, and the writer prompts, include only those. Before any R or R-loc writer, the fidelity run writes `/mnt/optane/review-arms-pilot/fidelity/gate-report.json`: `{"manifest_sha256", "served_steps", "steps": {step: {"pass", "reasons", "decisive_n", "random_n", "prescience": "estimated"|"not_estimable", "metrics"}}, "documented": {"reported_only": true, ...}, "created_at"}`. pilot_run admits R and R-loc only when that report matches the current manifest's hash and `served_steps`, and passes every served step. A failing step is removed by re-freezing with `select_pilot.py --served-steps` (a new manifest hash, so the gate runs again for the remaining steps before writers start).

**Run layout**: `/mnt/optane/review-arms-pilot/runs/<diff key>/<arm>/r<rep>/`, holding `writer/` (brief.md, usage.json, transcript pointer, query log for R and R-loc) and `reviewers/<slot>/` (handback, usage.json). N has only `reviewers/`.

**Engine** (host side, Slice 3): `answer(commit, query_text) -> {"status": "ok"|"parse_error"|"engine_error", "rows": [...], "total": int, "coverage": {"complete": bool, "open": [{"reason": str, "site": {...}}]}, "provenance": {...}}`. `coverage` is answer-level: an answer with an open frontier (unresolved dispatch, an unsupported transformation, a depth boundary) says so, so an open answer with no rows is never mistaken for a complete empty one. The engine's instructions are `menu-steps.md` and the catalogue only, never `menu-steps-cases.md`. They tell it that the catalogue's budget, exhaustion and cancellation outcomes are Bifrost's execution limits: it answers completely, and reports `open` only for a semantic boundary. Rows are complete under the step's semantics and sorted in a stable order. The cache key is `(commit, normalized query, engine inputs hash)`, shared by R and R-loc. The engine inputs hash covers the RQL reference, the frozen menu-step semantics, the engine model's resolved version and the engine prompt, so a rerun after any of them changes never reuses an answer made under the old rules. A query that doesn't parse against the frozen grammar (documented forms from the catalogue, plus the Slice 2 steps) gets `parse_error` with the parser's message, and no free text is answered. Provenance records the engine model, tokens and the steps answered, for H's later token matching.

**Model versions**: the harness offers `fable` as an alias, which resolves to the newest Fable a profile advertises, and has no exact ID for Fable 5.2. Every engine, judge and hand-check session therefore verifies at start that its model resolves to the manifest's `model_versions.fable`, records the resolved version, and fails visibly if it differs. A catalog change can't swap the model under an unchanged manifest or cache key. The fidelity run and the launch use the same verified version.

**Row shape** (Slices 3, 4 and 6): every engine row is `{"kind": "<row kind>", "sites": [{"path": "<repo-relative>", "start_line": N, "end_line": N}], "fields": {...}}`. `sites` lists every source location the row carries (for a flow row, both ends). `fields` holds the row's other content: values copied verbatim, names and signatures. R-loc's reduction keeps only `sites` and drops `kind` and `fields`. A union query can mix row kinds, and a kind such as read versus write is semantic evidence beyond the location.

**Transport** (Slice 4): one MCP tool, `rql_query(query: string, page: int = 1)`, served over SSH into the sealed writer session as Oz does. The host server looks up the session's assigned diff and arm. It calls the engine (cached, so later pages cost nothing extra), then returns page `page` of `page_rows` rows in the engine's stable order, with `total`, `page`, `pages` and the answer's `coverage`, repeated on every page. R-loc keeps `coverage.complete` and each open frontier's site, and drops its reason text, which is semantic. A `parse_error` or `engine_error` is returned to the writer as that status with its message, never as an empty page, and is logged with the status, so a tool failure can't be read as a zero-match query. Every row stays reachable by paging. For R-loc it first reduces every row to its source locations (each path and line span the row carries, with the row kind, snippets, signatures and values removed). Every call is logged with session, diff, arm, query, the requested page, the delivered rows in full, the answer hash and latency. The readout uses this to tell a decisive fact on a delivered page from one the writer never paged to.

**Judgments** (Slice 6): `/mnt/optane/review-arms-pilot/judge/<opaque id>.json` holds `{"catch": bool, "recovered_facts": [fact ids], "matched_defects": [...]}` for reviewer outputs and `{"contains_decisive": bool, "contained_facts": [...], "states_defect": bool}` for briefs, with the private ID map kept apart as in `brief_score.py`. Every judged output also gets a blind Fable/high hand check, as in the full design, and the readout uses the hand-checked labels.

**Isolation and audit** (all slices): the existing Podman target doesn't enforce provider-only egress. It mounts each session's own git cache at `/run/hel/git-cache`. For a no-later-history session that cache is cut off at the session's pin, so it holds no fix commit, and the engine's holds a single orphan commit. Every writer, reviewer and engine session attests its mounted cache from inside the container before its first prompt (`attest_pilot_cache`; the engine also requires a singleton), so the cutoff is checked structurally in each session, not by the transcript audit. Egress is measured inside each engine container (`rql_mock/engine/record_egress.py`) and reported, not enforced. The audit (`audit.py`) matches strings in recorded tool calls and output, so it's a partial check: a subprocess could reach a source without its path or URL appearing. Network egress isn't restricted to the provider; that remains an unenforced gap, which the readout states. Every writer, reviewer, engine, judge and hand-check session is audited before its output counts. A session that read `/run/hel/git-cache`, ran a history or diff command beyond its own checkout's single commit or pin, or used the network other than through its harness's provider is void, and its job reruns. The engine is the most sensitive, since reading the real history would show it the fix. The readout reports the void count per session kind, and states that isolation was enforced by audit, not by the target.

**Reviewers are ordinary sealed sessions** (Slice 5): a native review child's transcript is deleted at teardown (`destroy_reviewer_child`, mj-daemon/src/daemon/resume.rs), so it can't be audited. Pilot reviewers are therefore launched like writers (sealed, pinned profile, Terra/high) with exactly the reviewer prompt mj's native review renders, filled in with the diff and, for brief arms, the brief. Every arm's reviewers launch the same way, so the arm contrasts are unaffected. Comparisons with earlier stages, which used native review children, carry that difference.

## Plan of Work

The slices and their ownership. Each slice owns its files, and no two slices edit the same file.
1. Manifest: new `brief/rql-census/pilot/select_pilot.py` and its output. Small. I review and commit the manifest before any writer runs.
2. Step semantics: the helper drafts, then I edit and freeze them in `brief/rql-census/menu-steps.md`, whose hash goes into the manifest.
3. Engine: new `rql_mock/` package (parser, answerer, cache, fidelity sampler). It depends on 2 for semantics; the parser for documented forms can start now from the catalogue.
4. Transport: new `rql_mock/server.py` and an `oz`-style wrapper script; tested with a stub engine first.
5. Launch: additive changes to `brief_driver.py` and `driver.py` behind new options (`--manifest`, `--arm`, `--replicate`), so earlier studies' commands behave as before.
6. Judging and readout: new `pilot_judge.py` and `pilot_readout.py`, reusing `brief_score.py`'s blinding.

Builds are Python only. Tests use hand-written fakes for the engine and the sessions. Live model use before launch is the smoke run and the fidelity measurement: real Fable engine answers on 20 queries per served step, Terra/high gold enumeration, and fresh re-asks for stability (study plan, Lean pilot).

## Validation and Acceptance

- The manifest's draws are reproducible: rerunning `select_pilot.py` reproduces the file byte for byte.
- Smoke: for one diff, every arm's writer and both reviewers run, the R and R-loc query logs show paging and the R-loc reduction, and judges produce records for every output.
- Before launch, the engine's fidelity sample is measured live and reported, as the study plan's pilot gate defines it: for each added step, every available in-scope decisive query plus 10 random ones; for documented steps, one pooled random sample of 20. That is about 72 engine answers (about 22 decisive, 30 random from added steps, 20 documented). An added step that fails its accuracy bars is removed by re-freezing with `--served-steps`. Prescience is pooled across the added steps; a pooled gap above 10 points stops the pilot before any writer.
- Tests, run from `.agents/evals/review-arms/`: `python3 -B -m pytest -q -p no:cacheprovider test_*.py rql_mock/test_server.py rql_mock/engine/test_engine.py` and `python3 -B -m unittest rql_mock.test_fidelity` (the fidelity tests use package-relative imports, so they run as a module).
- The readout reproduces every number from stored inputs alone: the judgments, the writers' delivery logs (queries.jsonl, which site coverage needs), the manifest, and `pilot/labels-snapshot.jsonl`, a frozen copy of the 35 diffs' census labels committed with the manifest, whose hash the manifest records. The readout reads census sites and facts only from that snapshot.

## Idempotence and Recovery

Every job's output records the hashes of the inputs that produced it: manifest, prompt file, image digest, RQL reference and menu-step semantics. A stage skips a job only when its output exists, validates and carries the current input hashes. A job whose inputs changed (for example a prompt fixed after the smoke run) has its old output moved aside, never deleted, and runs again. That also invalidates its reviews and judgments, which rerun on the new brief. Admission is per diff: a diff starts only when the profile's remaining weekly quota minus the diff's estimated cost stays above the 25% floor, and an interrupted diff's missing jobs run before any new diff. The readout includes only diffs whose every job finished.
