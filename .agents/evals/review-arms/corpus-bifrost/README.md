# Bifrost bug corpus and census

Milestone 1 of `brief/PLAN-bifrost-bugs.md`; no brief, reviewer or judge runs. The source amendment retains every qualifying Hard-won defect and fills to 30 from the saved closed-bug issue order, retaining only confirmed introductions under `corpus/SPEC.md`. A bug issue must have a fix that adds or changes a test pinning the defect. Selection follows saved order, with the SPEC feature-spread and size rules. Every record retains its source and found-by labels.

The authoritative repository is the fresh read-only clone at `/mnt/optane/tmp/d2fa-bifrost/bifrost-dev`, pinned by `selection-registration.json`. Data and evidence live in `/mnt/optane/review-arms-bifrost-20261116`. This harness directory will hold the frozen corpus, census, provenance and hashes. No files here are a completed corpus yet.

The issue seed 20261117 was committed before its draw. The ordinary seed is also **20261117**, recorded here before its final draw. Draw 40 with `select_diffs.py`, excluding every traced introducing commit (including rejected and unselected candidates), after the trace set is known. An earlier ordinary eligibility dry run did not record a sample.

The first issue listing stopped at GitHub's 1,000-item cap and omitted issues before 2026-08-17. Its order was discarded before tracing; five titles had been printed. The uncapped paginated REST listing with the same seed produced the final 887-issue pool and order whose hashes are registered.

Tracing uses configured subtle children, GPT-6.1 Sol/medium. The lead clarified that the earlier Opus/Carbon helper wording was descriptive and approved Sol tracing after the quota handoff. Two independent census passes will use Carbon/high and Sol/high, with neither pass reading the other's outputs. Ordinary diffs have no known defect and remain controls; this milestone performs no review to discover defects in them.

Existing mechanisms reused: `select_diffs.py` (sampling), `knownbug.py` (data-root override; no repository hardcode), `label_facts.py` and `label/workflow*.js` (census schema and agreement), and `verify_driver.py --kind task` (cross-target labeller lifecycle). No new scheduler, store or runner is planned.

Source correction before freeze: the initial22 count covered literal comments only. Canonical Rust `#[hardwon(...)]` attributes add12932markers across2244files,11969distinct reason strings and2705distinct issue numbers. These are unqualified source counts, not qualifying defects. Bifrost `AGENTS.md` explicitly defines this attribute as hard-won defect metadata. No corpus was frozen. The lead is deciding a source-order amendment because exhaustive qualification of this pool cannot retain every qualifying defect within the30-item target. The earlier seeded bug-issue order and all completed trace evidence remain unchanged.
