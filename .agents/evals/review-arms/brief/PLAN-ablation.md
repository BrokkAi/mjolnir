# Relation-brief ablation: is the decisive relation what the blind brief adds?

Stage C (`PLAN-bifrost.md`) found that Bifrost's `relation_brief` does not
help reviewers, while the blind brief does. The blind brief differs in two
ways at once: it states the decisive relation (the census fact) and it is
written by an LLM that selects and frames everything else. This ablation
separates the two before engine work (#13211) is sized around the first.
It was proposed by the second-opinion reviewer (c43ff075) and approved by
Jonathan on 2026-11-16, before any run.

## Diffs

The 10 Hard-won diffs where the blind brief lifted catches over no brief
(pooled over Terra and Sol, more than 5 points):
catalog-cache-key-collision, child-model-wrong-efforts,
codex-effort-alias-lost, held-prompt-ignores-interrupt,
projection-mixed-sqlite-snapshots, stale-model-launch-spec,
symbol-change-keeps-cached-transcript, terminal-id-reused,
web-feed-self-deadlock (primary) and metadata-checkpoint-overwrites-checkout
(secondary). On them, round F's catch rate is 23.3% with the Bifrost brief
and 90.0% with the blind one for Terra/high (gap 66.7 points), 40.0% and
70.0% for Sol/high (gap 30.0).

The diffs were chosen by their outcome, so the arms can only be compared
with each other on these diffs, not generalised to the corpus.

## Arms

Each arm runs 3 Terra/high and 3 Sol/high reviewers per diff, as in
stage C: 10 × 2 arms × 6 = 120 sessions.

- **D (Bifrost plus the decisive item)**, `DT`/`DS`, brief kind
  `bifrost-plus`: the stage C Bifrost brief with one item added, the
  decisive item, rewritten in the Bifrost brief's form.
  - Source: the stage A editor's carrier item for the diff
    (`brief_oracle.carrier_item`): its matching item when the blind brief
    contained the fact, otherwise the item it inserted. That item states the
    census relation without the consequence and passed stage A's leak check
    inside the oracle brief.
  - Form: the anchor style of the brief's other items (qualified names and
    `file:line` in backticks), the section and diff-order position where the
    relation belongs, and the same completeness tag as its neighbours. The
    item may state a relation Bifrost cannot compute today; that is the
    point. No other item changes. A value-flow fact goes among the items of
    `## Paths` or `## Affected callers`, not under the Bifrost brief's
    `## Value flow` note ("Not computed"), which stays unchanged (ruling of
    2026-11-16, before any edited item existed). "Must not stand out"
    means: no item longer than the brief's longest existing item; a compound
    fact may be split into at most two consecutive items within that length
    (clarified 2026-11-16 after the first edit came back 1.3 to 2.5 times
    too long, before any brief was built or judged).
  - Checks, both bound to the hash of the brief text: a judge
    applies the stage A editor's rule (`bifrost-contains`) and must answer
    `yes`; `judge/SPEC.md` run on the brief as a review output must answer
    `missed`. A failure goes back to the editor once, and then to a hand
    decision recorded with the brief.
- **X (blind minus the decisive item)**, `XT`/`XS`, brief kind
  `blind-minus`: the blind brief with the item the stage A editor matched to
  the census fact removed (its `matching_item`, present verbatim in all 10
  blind briefs). For the 6 diffs labelled partial, that item is the partial
  anchor. Nothing else changes.

## Judging

Round G: `brief_score.py build --arms DT,DS,XT,XS,FT,FS,BT,BS --bugs-from
ablation-jobs.json --name brief-judge-g`. It judges the new arms together
with the stored no-brief, Bifrost and blind sessions of the 10 diffs, in
blind batches. Arms F and B are not rerun. Two judges
(`verify_driver.py --kind judge`), a hand check of every caught or partial
verdict and every disagreement, as in round F.

Judge and editor model: gemini-4-carbon at high effort, on Jonathan's
instruction of 2026-11-16 to start new helpers as carbon/high (rounds A to F
used Opus/high). Because the judge changes, every comparison in the Reading
uses round G's labels for all arms; round F's labels for F, B and no-brief on
the 10 diffs are reported beside them as a judge-consistency check, and a
difference of more than 10 points for any arm and model is reported as a
caveat.

## Reading

Terra/high is primary, as in stage C; Sol/high is reported beside it. With
F, B, D and X the catch rates on the 10 diffs (mean over diffs) and
G0 = B − F:

- **Closure** c = (D − F) / G0, the share of the gap the decisive item
  closes on its own.
- **Dependence** of the blind brief on the item: B − X.

Outcomes, first that holds:

1. **The relation carries it:** c ≥ 0.6. Engine facts of the census kinds
   (#13211 site context, shared state first) can close most of the gap.
2. **Selection and framing carry it:** c ≤ 0.25. Adding the relation to
   today's brief does not help; the rest of the blind writer's output
   matters. Measure a writer that uses Bifrost as a tool next.
3. **Both matter:** otherwise.

B − X is reported with each outcome. If it is under 10 points the blind
brief's lift does not depend on the decisive item alone (its other items
carry the relation too). Intervals are paired 95% bootstrap intervals over
the 10 diffs; with 10 diffs they are wide, so the outcome is a direction for
#13211, not a measurement of it.
