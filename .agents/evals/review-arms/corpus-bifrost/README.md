# Bifrost corpus and census: Milestone1

Completed under the recorded quota amendment, before any study brief, reviewer or judge run. Data: `/mnt/optane/review-arms-bifrost-20261116`. The temporal boundary is now explicit UTC after the review correction recorded below; all 30 bug introductions still qualify.

## Corpus

30 unique confirmed introducing diffs, all canonical Hard-won source, plus40 ordinary controls with no known defect. The controls are not confirmed-clean and receive no census labels. The final selection reaches source-group127:30 selected,1 qualified spread exclusion,96 rejected or unconfirmed. There are24 feature families, at most2 per family. Attribution was checked independently against source/history and regression pins; no build or test reproduction was executed.

The initial22-comment inventory missed12932 canonical attributes. The corrected4168-group source order used seed20261118, recorded before drawing. Fallback closed-bug order was retained but unused. All429 identified introduction boundaries, including rejected/unselected alternatives, were excluded from the ordinary draw with seed20261117. The corrected ordinary pool contains8230 eligible commits. The initial source selection and inputs were frozen before census runs in local commit a80ad7838; the UTC-corrected ordinary controls were redrawn after census and before arms as recorded below.

## Census

| Role | Model / effort | Labels | Relation labels |
|---|---|---:|---:|
| Primary | GPT-6.1 Sol / high |30|17 (56.7%)|
| Partial replication | Gemini4 Carbon / high |5|0|

The primary pass has4 affected-callers,3 path,10 value-flow and13 none labels (11 in-diff,2 external), with27 high and3 medium confidence. Relation membership is primary kind!=none. The one primary defect check left unconfirmed is MCP timed-out progress: the labeller could not independently verify the unvendored rmcp dependency implementation. The source attribution audit checked the pinned dependency separately; the census uncertainty is preserved verbatim, not adjudicated.

Agreement is **4/5** for both relation membership and exact kind, on the5 diffs both models labelled. All5 Carbon labels are none/in_diff; the single disagreement is Kotlin value-class this projection (Sol value flow, Carbon none). This small, mostly-negative overlap is partial replication, not agreement over30 diffs. The remaining25 replication entries are explicitly unavailable, not negative labels.

Quota forced the role amendment after Carbon's first unit. The complete Sol pass became primary and no other Carbon unit launched. The exact amendment is preserved in `registration/census-role-amendment.json` and the amended plan snapshot. Original pre-census plan and input manifests remain unchanged. No selection or adjudication used label outcomes. The primary labeller differs from mjolnir's census, so the codebases' relation shares are not directly comparable.

## Source and discovery strata

All30 selected cases are `source=hard-won`; bug-issue fallback contributes0. Primary relation counts by recorded discovery provenance:

| Found-by metadata | Cases | Primary relation labels |
|---|---:|---:|
| no recorded found-by label | 17 | 11 |
| unknown | 12 | 5 |
| found-by:policy-oracle | 1 | 1 |

Known-empty means no recorded found-by label, not human-only discovery. Unknown remains distinct. No discovery share was excluded or capped.

## Yield and differences from mjolnir

The96 unqualified groups have primary documented obstacles:29 unconfirmed attribution/exposure,20 oversized introduction,20 not established product behavior,11 cutoff,10 preflagged limitation and6 external/findability. A rejected group is not proof that every member marker was disproved. Full per-position reasons, secondary flags and discarded preparation overhead are in `corpus-build/yield-report.json` and `.md`.

Bifrost known-bug median is827 changed lines (109–2936), versus688.5 for mjolnir's known bugs. Bifrost ordinary median is387 (91–1473), versus326 for mjolnir's ordinary controls. Bifrost cases emphasize symbol/type resolution, semantic models and analysis completeness; the exact raw category counts are in the yield report. These size/kind observations imply no reviewer-performance result.

## Cost and preservation

Recorded API-equivalent estimates total **$61.15**, an incomplete total rather than invoice spend. This includes completed parent reports, helper/orchestration reports and the Sol census (**$1.8451**,6/6 full-turn reports). Carbon's one report has unspecified scope and no price; one helper report is also unpriced, and the current parent reporting turn is unfinished. Zero raw ticks for Carbon is not zero cost. The observed shared-account Gemini week moved8% to a fresh3%; that rounded account delta is not an isolated monetary cost. Raw coverage and scope are in `accounting/final-cost-summary.json`.

Canonical results came through the same parameterized `label_facts.py` path as mjolnir: full Sol collect, then collect-second with the explicitly declared original Carbon batch00 five-item subset. Schema, exact coverage, frozen inputs and byte identity with untouched validated outputs passed. All7 census sessions are suspended; no more model work remains. No main configuration, unrelated sessions, briefs, reviewers or judges were changed/run.

The final immutable file hashes are in `FREEZE.json`. Selected corpus SHA256 is `f6fda0200ac7fecbb7f03bc3a27fb193fd08620c82476118f33533972c8ea749`; primary census `cec120df43bc890efbfbd5fee79fc2236ff654e1249b3dcb1f9309c054536089`; partial replication `f6c98e750a84f9654db8b531f49cbe131458a5fbd5c8dd1d68c4684a99e23e0c`; relations `a5e9a2d5d4f731cbc95cd30a5cdd2b385a79212b07e6a03258b217743a4a6efa`. Source/helper evidence is archived with an old-to-archived prefix map, so temporary report cleanup does not erase provenance.

Harness aliases: `census-primary-sol.jsonl`, `census-replication-carbon.jsonl`, `relations.jsonl`, `census-summary.json`, `cost-summary.json`, `census-role-amendment.json`. The data-root paths above resolve the full raw evidence. `selection-freeze.json` and `FREEZE.json` are the current UTC-corrected freezes. Original pre-correction bytes, including the pre-census selection snapshot, are preserved under `utc-cutoff-correction/pre-correction/`. All commits are local only.

## UTC correction before arms

Review found the timezone-free cutoff admitted control `0549477459` at `2026-11-01T01:14:32Z`. The tools and SPEC now require a strict `2026-11-01T00:00:00Z` cutoff. All30 bug introductions qualify unchanged, as do all887 saved fallback issues; the original frozen mjolnir70 selected records have zero matches in the affected00:00–06:00UTC window and were not changed. Redrawing all40 controls with seed20261117 changes39 entries because the eligible pool and its sampling slices shrink (8269 to8230). The full old/new order, membership, dates and original freezes are retained in `utc-cutoff-correction/`. The bug corpus, census labels, relation set, exclusion list, selection seed and other eligibility rules are unchanged. No briefs or reviews preceded this correction.
