# Relation-brief dilution: does the decisive fact survive among its siblings?

The ablation (`PLAN-ablation.md`) found that adding the one decisive relation
to the Bifrost brief lifts Terra/high's catch rate on 10 Hard-won diffs from
20.0% to 73.3% (closure 0.76). That item was chosen with knowledge of the bug.
An engine that computes these relations does not know the bug: it emits every
fact of the same kinds for the sites in the brief's scope, and the decisive
one is a line among many. This arm measures how much of the lift survives
that dilution before the engine work (#13211 and its splits) is sized around
emitting the facts in the brief. Requested by Jonathan on 2026-11-16 ("do
it"), registered before any brief was built.

## Diffs

The ablation's 10 diffs, in its order (catalog-cache-key-collision,
child-model-wrong-efforts, codex-effort-alias-lost,
held-prompt-ignores-interrupt, projection-mixed-sqlite-snapshots,
stale-model-launch-spec, symbol-change-keeps-cached-transcript,
terminal-id-reused, web-feed-self-deadlock,
metadata-checkpoint-overwrites-checkout). They were chosen by outcome, so the
arms compare with each other on these diffs only.

## Arm

**W (diluted)**, `WT`/`WS`, brief kind `bifrost-diluted`: arm D's brief
(Bifrost plus the decisive item, unchanged) with every sibling fact added.
3 Terra/high and 3 Sol/high reviewers per diff: 10 × 6 = 60 sessions.

### Siblings

A sibling is a true fact of one of the following kinds about a site in scope.
The kinds generalise the census kinds of the 10 decisive facts; every diff
gets all six, so on each diff at most one kind is the decisive one.

1. **Argument values at call sites:** a literal, constant, `None`, default,
   or a value computed from one in the same function, passed for a named
   parameter.
2. **Field and state writes:** the field, static, map entry, file or table
   row a function writes, with the value written and the branch it runs
   under.
3. **Held scopes at a call:** a lock guard, transaction or similar scope live
   at a call site; the locks and transactions a function acquires.
4. **Keys and comparisons:** literal keys used to read or write maps,
   configuration, caches or tables; the key columns of a table written; the
   values a cache check or condition compares.
5. **Lifetimes:** where a value is created relative to a loop, connection,
   request or process start (once, per connection, per call), and the counter
   or format that names it.
6. **Shared state:** the state (fields, statics, channels, files, tables) a
   changed function shares with another function the brief names, or that it
   shares none.

Scope: every site inside the functions whose bodies the diff adds or
changes, at the introducing commit, and each call site the brief cites
(`file:line` of an edge; for an unchanged caller only that call site, not its
whole body). No budget: the editor lists every fact in scope.

### Construction

1. **Editor**, one per diff, gemini-4-carbon/high on `morannon-podman`
   (inputs by `mj put-file`). It sees the Bifrost brief (stage C, without the
   decisive item), the introducing and base commits, the kinds and the scope,
   and not the census, the bug, arm D's item or any eval result. Form rules
   are arm D's: one line per fact in the brief's anchor style (qualified
   names and `file:line` in backticks), the neighbours' completeness tag, no
   judgment words and no consequence, placed in `## Affected callers` or
   `## Paths` (never under `## Value flow`, which stays "Not computed") in
   diff order. No item longer than the brief's longest existing item; a
   compound fact may take two consecutive items.
2. **Form check** (script): one line, `- ` prefix, length rule, the anchor
   line it follows exists. Failures go back to the editor once, then are
   dropped and recorded.
3. **Truth check**, one carbon/high verifier per diff, also blind to the bug:
   each sibling is `true`, `false` or `unclear` against the code at the
   introducing commit. `false` siblings are dropped. If more than 20% of a
   diff's siblings are false, its list goes back to the editor once.
4. **Duplicate check**: a carbon/high judge with the census record and the
   stage A editor's rule marks any sibling that states the decisive fact
   (`yes`) or part of it (`partial`). `yes` siblings are dropped (arm D's
   item already states the fact once); `partial` ones stay. The share of
   diffs with a `yes` or `partial` sibling is reported as the editor's recall
   of the decisive fact.
5. **Build** (script): siblings are inserted into arm D's brief after their
   anchor lines, in file order. Where siblings and arm D's item follow the
   same anchor line, their order is a seeded shuffle, so position does not
   mark the decisive item.
6. **Brief checks**, bound to the brief hash: the stage A contains rule must
   answer `yes`; `judge/SPEC.md` run on the brief as a review output must
   answer `missed`. A leak is traced to its sibling, which is dropped and
   recorded.

Reported per diff: siblings proposed, dropped at each step, kept, and brief
lines and bytes before and after.

## Judging

Round H: `brief_score.py build --arms WT,WS,DT,DS,FT,FS --bugs-from
dilution-jobs.json --name brief-judge-h`, two carbon/high judges
(`verify_driver.py --kind judge`), hand check of every caught or partial
verdict and every disagreement, as in round G. Arms D and F are not rerun;
their stored sessions are re-judged with W in blind batches, and round G's
labels for them are reported beside round H's (a difference of more than 10
points for any arm and model is a caveat).

## Reading

Terra/high is primary; Sol/high is reported beside it. With round H's catch
rates on the 10 diffs (mean over diffs):

- **Retention** r = (W − F) / (D − F), the share of the decisive item's lift
  that survives its siblings.

Outcomes, first that holds:

1. **Selection is not the bottleneck:** r ≥ 0.7. A brief that states every
   fact of these kinds keeps most of the lift; build the facts (#13211 and
   splits) and emit them in the brief.
2. **Selection is needed:** r ≤ 0.3. The facts drown the decisive one;
   Bifrost alone would need to rank them, so measure an LLM writer that uses
   Bifrost as a tool next.
3. **Partly diluted:** otherwise.

W − F, W − D and B − W (B from round G) are reported with paired 95%
bootstrap intervals over the 10 diffs. With 10 diffs the intervals are wide;
the outcome is a direction, not a measurement. A size budget, as Bifrost's
brief has today, would need a ranking rule, which this arm does not test.
