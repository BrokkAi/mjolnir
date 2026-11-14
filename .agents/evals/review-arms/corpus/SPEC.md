# Bug corpus specification

Purpose: build a set of 30 known mjolnir bugs to test code reviewers on. For
each bug, a reviewer will see the repository at the commit that introduced
the bug, with no later history, and will review the diff `base..intro`. We
then check whether any review finding identifies the known defect.

The reviewers being compared are gpt-6.1-terra and gpt-6.1-sol. Neither model
reviewed mjolnir commits before about 2026-11-05, so the introducing commit
must be older than the cutoff below. Otherwise the bug was already filtered by
one of the setups under test, and the corpus would be biased.

## Source

`// Hard-won: <#issue or short SHA>: <defect>` markers (Python: `#`) in the
repository at a mjolnir checkout with full history (the path is given in your instructions)
(branch `master`, full history). Each marker sits on a test that pins a defect
found after the code was believed done.

Work read-only with git. Do not modify the repository, check out other
commits in it, or create branches. Use `git show`, `git log`, `git blame`,
`git log -L` and `git worktree add --detach` into your own report directory if
you need a checkout. Remove worktrees you create before you finish.

## Selection rules

A candidate qualifies only if all of these hold:

1. **Cutoff.** The introducing commit's committer date is before
   2026-11-01T00:00 (local time in the commit).
2. **Attribution.** You identified the commit (or short contiguous series of
   commits from the same change) that introduced the defective code, and you
   checked it: the defective lines are added or changed in `base..intro`, or
   the diff removes something that was needed. Use the fix commit (find it
   with `git log -S`, `git blame` on the marker line, or the SHA or issue in
   the marker) and trace the fixed lines back with `git log -L` or `git blame
   <fix>^`.
3. **Findable from the diff.** A careful reviewer who sees only the repository
   at `intro` and the diff `base..intro` could find the defect. Reject bugs
   that need knowledge the repository at `intro` did not contain: a later
   harness or third-party behavior change, a production-only environment
   fact, or code added after `intro`.
4. **Real behavior.** The defect changes runtime behavior for a user or
   client: wrong result, crash, hang, race, data loss, security hole, leak,
   wrong error. Reject test-only defects, flaky tests, style, docs, and
   performance trivia.
5. **Not pre-flagged.** Neither the commit message nor a code comment in the
   diff already names the problem.
6. **Ordinary commits.** `base` is an ancestor of `intro`, both lie on
   `master`'s history (`git merge-base --is-ancestor <intro> master`), and
   `intro` is not a merge commit. Normally `base = intro^`. Use an earlier
   base only when the bug spans a short series from one change, and say why.
7. **Size.** Prefer diffs up to about 1,500 changed lines, not counting
   goldens, fixtures and lock files. Hard maximum 4,000.
8. **Spread.** At most two candidates from the same feature or commit
   series. Prefer a mix of bug kinds (concurrency, lifecycle, error handling,
   protocol or format compatibility, paths and encodings, security, state
   machines, UI state, and so on).

Skip markers you cannot attribute with confidence after a reasonable trace
(about 15 minutes). Do not force a candidate.

## Output

Append one JSON object per line to `candidates.jsonl` in your report
directory. Use full 40-character SHAs. Fields:

```json
{
  "id": "short-kebab-slug",
  "marker": "path/to/file.rs:LINE",
  "marker_text": "the Hard-won comment",
  "fix_commit": "<sha>",
  "intro_commit": "<sha>",
  "base_commit": "<sha>",
  "intro_date": "YYYY-MM-DD",
  "diff_size": "N files, +A -D (excluding goldens/fixtures/locks: +a -d)",
  "defect": "1-3 sentences: what is wrong, stated in terms of code visible at intro",
  "location": "file and function (and line range at intro) where the defect is",
  "trigger": "the concrete input or sequence that exposes it",
  "match_rule": "what a review finding must say to count as catching this bug: the necessary elements",
  "near_misses": "findings that touch the same code but should NOT count",
  "why_findable": "why a reviewer of base..intro alone could see it",
  "category": "one or two words",
  "attribution_confidence": "confirmed | likely",
  "notes": "anything else, including why base is not intro^ if so"
}
```

Write `match_rule` so that a judge who reads only the review findings and
this record can decide. A finding counts when it identifies the same defect,
even in other words or with a different proposed fix. It does not count when
it only names the same file or function, or describes a different problem
there.

Finish with a short handback: number of candidates, number of markers you
examined and rejected (with the most common rejection reasons), and any
candidates you are unsure about.
