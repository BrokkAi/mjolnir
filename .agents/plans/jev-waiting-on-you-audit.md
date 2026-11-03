# Measure Jev on real “waiting on you” replies

This plan follows `.agents/PLANS.md` and records the audit requested on 2026-11-03.

## Purpose / Big Picture

Measure how often Jev recognizes a real request for user guidance in Mjolnir agent replies containing “waiting on you”. The UI propagation fix is committed as `684033081`; this audit separates classification quality from that display defect. Search only Mjolnir's stored conversations, never native harness files. Preserve evidence provenance and report missing historical classifier evidence separately from misses.

## Progress

- [x] Fix the TUI propagation defect; complete the changed-crate suites, workspace Clippy and format checks.
- [x] Search the connected history index and complete a read-only search of the controller's materialized transcript table: 351 matching agent messages in 36 sessions.
- [x] Extract candidate replies and preceding context; collapse three repeated closing-message identities to leave 336 unique replies (315 still contain the phrase in their closing text).
- [x] Have straightforward subagents label every candidate without Jev results; review disputed approval scopes with chronological context and parent adjudication. Freeze 240 required, 90 optional/none, 4 redundant-permission and 2 uncertain labels before scoring.
- [x] Match 31 retained worker decisions. The input gate detected 23 of their 30 confirmed questions; all 30 selected `required`, and seven failed the probability margin.
- [x] Replay the other 305 cases three times (915 calls), as explicitly authorized by the user. All calls succeeded without retries; no known historical case was replayed.
- [x] Save detection rates, coverage, false alerts, evidence limitations and representative failures in `.agents/docs/jev-waiting-on-you-20261103.md`; preserve the reproducible private audit scripts and source artifacts outside Git.
- [x] Verify every case/repetition and request hash, the frozen label digest, script syntax and the 19 existing offline evaluator tests. Review the repository documentation diff before committing and pushing this completed audit.

## Surprises & Discoveries

The history index includes native Claude sessions as well as Mjolnir sessions. The controller table `materialized_transcript_items` is the source of truth for the corpus. The live database is about 16 GiB; query it read-only and do not copy the entire store.

Large judging assignments produced incomplete handbacks, so the audit used small batches and collected returned JSON when label-writing scripts failed. Earlier full-session artifacts contained later user answers; chronology review therefore received only context preceding each target reply. This caught scope confusions between resolver design A/B and a later memo-versus-rounds choice. The parent reviewed disagreements before any Jev scoring. Keep the original labels and all adjudications rather than silently replacing them.

Three candidate intervals ended at the same final message as another interval. Deduplicate by session and final stable message ID, preferring a recorded turn boundary. The final corpus has 299 recorded boundaries and 37 boundaries inferred between user messages.

## Decision Log

Use case-insensitive literal substring matching for “waiting on you”, including “waiting on your …”. Let judges reject quoted, historical, third-party or optional handoffs. Exclude this audit session to avoid evaluating its own descriptions of the phrase. Keep raw conversations and classifier requests under `/mnt/optane/tmp/jev-waiting-on-you-20261103`; commit only tooling, aggregate results and short evidence excerpts.

Use the configured `straightforward` task type through the daemon's existing subagent API because this worker does not expose an MCP spawn tool. The user's request explicitly authorizes delegation. Freeze labels before inspecting or replaying Jev classifications. Label real user guidance separately from redundant permission requests and optional offers.

This parent has delegation disabled, so create a dedicated coordinator (`9592bba2961bdc098994a47e6615c455`) with delegation enabled instead of changing this session's policy. The configured straightforward judges ran `gpt-6-luna` at high effort. Their IDs and outputs are retained in the audit directory.

The first proposed replay was rejected by automatic approval review because it exported private conversation excerpts. The user then explicitly authorized replay only where historical decisions are unknown. Reuse the 31 known decisions; send only the 305 unknown cases to `https://api.typesafe.ai/v1/systemone`, three repeats each. No request was sent before that approval.

## Outcomes & Retrospective

The UI fix was pushed in merge `7f5c3765e`, after cleanly merging upstream. It passed 2,234 controller tests, 553 core tests, 560 TUI tests and 232 CLI tests plus their integration suites, workspace Clippy and formatting. The prose-question regression also passed after the upstream merge. Ignored live tests retained their existing isolation.

The audit is complete. The unknown-case replay detected 206/210 confirmed questions (98.1%) by majority across three calls, with 10/93 control alerts (10.8%). All 210 had `required` as their most likely input class; the four misses failed the margin. Three cases varied across repeats. The served model was `jev-1.14.0` on all 915 calls, with no failures or retries.

Historical retained evidence covers only 31 of 336 replies and is heavily clustered, so its 23/30 detection rate must not be presented as the whole corpus's result. The one retained control generated an alert; that is not a useful estimate of specificity on its own. All seven historical misses also selected `required` and failed the margin.

The TUI fix addresses propagation; this audit leaves classifier thresholds unchanged. False alerts include already granted permission and resolved or optional handoffs, so a threshold change needs evidence beyond recall on one phrase. The final report separates historical decisions from reconstructed-input replay and records the two excluded ambiguous cases.

## Context and Orientation

The controller database is `/home/jonathan/.local/share/mjolnir/mj.sqlite3`. Agent text lives in JSON bodies in `materialized_transcript_items`; `api_events` can identify completed turns. Worker Jev records live in each worker's `jev-decisions/decisions.*.jsonl`. Retention is bounded, so historical coverage is incomplete. Current questions are in `mj-core/src/activity/verdict_questions.json`. The Python evaluator in `scripts/jev-scenarios-eval.py` implements the same probability threshold as Rust: `required` must win with probability at least 0.50 and at least 2.5 times the strongest alternative.

## Plan of Work

First extract matching agent messages and their session, stable ID, transcript position and timestamps. Keep only actual agent text, then gather prior user and assistant context and determine turn boundaries where recorded. Create blinded JSON batches with stable case IDs and a rubric requiring an unresolved decision, missing fact, review, approval or external action assigned to the user. Judges must distinguish optional follow-up, already-granted permission, quoting and waiting on somebody else.

Have straightforward subagents write case labels and reasons. Score the frozen labels against recorded decisions and against fresh current-contract replay separately. A replay using reconstructed runtime facts must be named as such and cannot establish what the installed classifier did historically. Report transport failures independently. Use multiple replay repetitions if affordable to quantify variability; do not tune prompts during this measurement.

## Concrete Steps

Read and write scripts from the repository root. Keep raw corpus, labels, requests and responses in the audit directory. Use installed `mj api-info` for the daemon API and its token-file location; never print the token. POST `/api/v1/sessions/<parent>/subagents` with `task_type: straightforward` and the self-contained judging assignment. Use the existing direct TypeSafe evaluator or hosted verdict endpoint with frozen bundled questions, keeping labels separate from requests.

## Validation and Acceptance

Every reported denominator must resolve to frozen case IDs. Report counts of matches, unique sessions, labeled genuine questions, nonquestions and ambiguous cases. Distinguish recorded classification coverage from replay coverage. Show the input gate's recall on confirmed questions and false alerts on confirmed controls, with representative session IDs and transcript positions. Validate scripts with Python syntax checks and small offline fixtures that protect provenance or scoring, rather than live-model tests in CI.

## Idempotence and Recovery

Read the live database only through `mode=ro`. Persist intermediate artifacts so the audit can resume without relabeling or repeating successful API calls. Never restart or alter the user's active sessions. Resume requests only after inspecting saved results and child status; do not repeat an acknowledged spawn.

## Artifacts and Notes

Raw matches are in `matching-agent-messages.json`; the deduplicated corpus is `selected-cases.json`. `labels-final.jsonl` and `labels-frozen.json` freeze labels and digests. The label digest is `cb82c4f8bb04c3abab22406738c9b4bb48cefac687835322431211d24cacb2a3`. `parent-adjudications.json` and `chronology-review.json` preserve review provenance. `historical-scored.json` holds the 31 matched runtime decisions. `requests-unknown.jsonl` and `replay-execution.json` define the user-approved replay. `results.jsonl` is append-only; successful calls must not be replayed on resume.

## Interfaces and Dependencies

Use Python's standard library for SQLite, JSON and HTTP. Reuse the repository's production question JSON and existing evaluator rather than inventing thresholds. A label record must carry its stable case ID, judge session ID, guidance category and short evidence-based reason; a scored record must carry the evidence source and the full input distribution.

Plan created during the requested follow-up; the existing UI fix remains a separate validated commit.

Updated after corpus extraction, blind judging, chronological adjudication and historical scoring. The user narrowed replay authorization to unknown historical decisions, reducing the run from 1,101 to 915 calls.

Updated at completion with replay results, zero-replay verification for known historical cases, the final report and completed validation. Private conversation artifacts remain outside Git; only this plan and the aggregate report are committed.
