# Does the brief writer's uplift hold on Bifrost's own bugs?

Every result so far comes from mjolnir's Hard-won bugs. Before the brief
writer ships, Jonathan asked that the mjolnir-derived design be checked on
bifrost-dev's bugs (2026-11-16). bifrost-dev is a different Rust codebase: a
static-analysis engine, larger, with different idioms. Registered before
any corpus is built.

## Corpus

Built from bifrost-dev with the same method as mjolnir's corpus
(`corpus/SPEC.md`, `select_diffs.py`, `knownbug.py`), adapted for that
repository:

- Hard-won tests marked `// Hard-won: <#issue or short SHA>: <defect>` give
  the fix commit. The introducing commit is attributed the same way, and
  each record carries defect, location, trigger, match_rule, near_misses and
  attribution_confidence.
- Only confirmed attributions are kept. The target is 30 diffs, chosen in a
  seeded random order before any brief is written, and capped at the size
  stage 1 used.
- A census labels each diff's decisive fact, using `label/` with two
  independent labellers as for mjolnir, and marks the relation diffs.
- 40 ordinary diffs (no known bug) are drawn the same way for the control.

The selection and the census are frozen, by hash, before any brief or
review runs.

Amendment, 2026-11-16, before any sample was drawn. The helper found 22
Hard-won markers at bifrost-dev HEAD 35933693612806a7, about 15 distinct
defects. Five are CI, release or generator defects that SPEC rule 4
excludes, and 20 of the 22 markers were written on or after 2026-11-10, so
at most about 10 Rust defects qualify, too few for the reading's bar. The
source is therefore:
- every qualifying Hard-won marker; then
- closed issues labelled `bug`, created before 2026-11-01, drawn in a
  seeded random order (seed recorded before the draw), until 30 confirmed
  attributions are reached.

A bug issue qualifies only if its fix commit or PR adds or changes a test
that pins the defect, which keeps the Hard-won meaning: a test pins a defect
found after the code was believed done. Each fix is traced to its
introducing commit under `corpus/SPEC.md`, unchanged. Fuzz and differential
finds are kept at their natural share. Every record carries
`source: hard-won | bug-issue` and its found-by label, and the readout is
reported by source and by found-by beside the pooled result.

Second amendment, 2026-11-17, before the corpus is frozen; it supersedes
the source rule of the first amendment and the clarification below. The
canonical Hard-won inventory is far larger than the first count: 12,932
`#[hardwon(...)]` attributes in 2,244 files, naming 2,705 distinct issues,
plus 22 `// Hard-won:` comments (bifrost-dev AGENTS.md defines the
attribute as the fix-time marker). The source is therefore:

- Group the markers by defect, meaning the issue number they name, or the
  commit for markers that name none. Each group counts once, however many
  tests it annotates.
- A seeded random order over those groups, of all dates, is recorded
  before the draw.
- Trace it under `corpus/SPEC.md`, unchanged, including the
  introducing-commit cutoff, until 30 confirmed attributions are reached.

Closed `bug` issues, in the order already drawn, are used only if the
Hard-won groups run out. A bug-issue trace already done is reused when its
issue comes up as a Hard-won group. Records keep `source` and found-by, and
the readout reports them as before.

Clarification, 2026-11-17, before the corpus is frozen. bifrost-dev also
marks Hard-won tests with an attribute, `#[hardwon("#issue: …")]`, which
has the same meaning as the `// Hard-won:` comment and counts as a Hard-won
marker. The count of 22 above covered comments only. Attribute markers join
the first group, and the bug-issue fill shrinks accordingly; the drawn
bug-issue order is unchanged.

Census roles, declared 2026-11-17 before any label:
- gemini-4-carbon/high is the primary `label/SPEC.md` pass, and GPT-6.1 Sol/high is the independent replication, using the same `label_facts.py` build, shuffles and collector.
- A diff is a relation diff when the primary pass's kind is not `none`, as in mjolnir's primary census. The replication's kind and every disagreement are frozen beside it.
- Both relation shares and the agreement rate are reported. Ties are not adjudicated, and nothing is selected on labels.
- The labels and the relation set are frozen by hash before any brief or review.

Amendment to the census roles, 2026-11-17, before the labels are frozen and
before any brief or review. Carbon's first unit (5 of 30 diffs) used 4 points
of the gemini2 week, which leaves about 4 points, not the roughly 20 that the
other five units need. Quota forces this change, not the labels. The roles
are now:
- GPT-6.1 Sol/high is the primary pass. Its 6 units are already complete.
- Carbon's completed unit is a partial replication. No further Carbon units
  run.
- The relation set is the diffs whose Sol kind is not `none`.
- Agreement is reported on the 5 diffs both models labelled, with that n
  stated.

The readout says that the primary labeller differs from mjolnir's census, so
the relation shares of the two codebases are not directly comparable.

Amendment, 2026-11-17, after census freeze and before any brief or review:
UTC cutoff correction. Review found ordinary control `0549477459` has
committer time `2026-11-01T01:14:32Z`, beyond the intended
`2026-11-01T00:00:00Z` boundary. The sampler's timezone-free cutoff used the
host timezone; the SPEC's local-time wording was also ambiguous. All corpus
tools now use the same strict UTC instant for introducing commits and issue
creation. Reject timezone-free cutoff arguments. Audit all 30 selected bug
introductions before changing anything else; if any fails the corrected
cutoff, stop and report before relabelling. Redraw all 40 controls with the
recorded seed `20261117` and unchanged eligibility, exclusions and sampling
method, then regenerate dependent freezes. Preserve the original freeze as
historical evidence. Audit the original mjolnir selected bug/control corpus
for the six-hour window after UTC midnight; report it without changing that
corpus. No briefs or reviews preceded this correction.

## Arms

3 Terra/high reviewers per diff in each arm:

- **No brief.**
- **Writer brief:** the blind brief from the writer model chosen by
  `PLAN-writer-terra.md` (Terra/high if it passes, otherwise Sol/high), with
  stage B's writer prompt, tools, blinding and leak check.

## Judging

As in `PLAN-writer-terra.md`: one judge model for both arms, two judges, a
hand check of every caught or partial verdict and every disagreement.
Ordinary diffs are scored by stage C's control 2 method.

## Reading

Over the relation diffs (the primary set), mean catch rate over diffs, with
paired 95% bootstrap intervals:

- **Generalizes:** writer brief − no brief ≥ 17 points (half of mjolnir's
  stage B blind lift of 35), with its interval above 0, and on the ordinary
  diffs the writer arm keeps at least 90% of confirmed findings per session.
- **Partly:** a lift with its interval above 0 that misses either bar.
- **Does not generalize:** otherwise.

Also reported: the non-relation diffs, the decisive-fact contains rate, and
the share of relation diffs whose decisive fact a diff-seeded query
template (H6's census) reaches. That share tests the templates on a second
codebase.
