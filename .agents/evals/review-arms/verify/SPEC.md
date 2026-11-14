# Finding verification specification

You verify code-review findings. Each batch covers one or two historical
mjolnir commits. For each commit you get the commit IDs and a shuffled list of
findings that several independent reviewers reported on the diff
`base..intro`. You are not told which reviewer, model or setup wrote which
finding. Do not try to infer it.

## Repository

Your working directory is a mjolnir checkout with history through
2026-11-13. Use `git show <intro>`, `git show <intro>:<path>`,
`git diff <base> <intro>`, `git log`, and `git blame` to read the code as it was at
`intro` and how it changed later. Do not modify tracked files, commit, push,
build, or run tests.

## Step 1: group duplicates

Several findings often describe the same problem in different words. Put
findings in the same group when an author would fix them with the same
change. Findings about the same function that describe different failures
are different groups.

## Step 2: verify each group

Decide whether the problem is real, using the code at `intro`:

- `confirmed`: you traced the code and the described failure happens for a
  reachable input or sequence, the diff introduced or exposed it, and the
  author would want to fix it. State the trace briefly with file:line
  references at `intro`.
- `plausible`: it may be real, but it depends on runtime behavior,
  environment, or third-party semantics you could not settle by reading.
  Say what would settle it.
- `rejected`: the code already handles it, the trigger cannot happen, the
  problem existed before this diff, the behavior is intended (say where that
  is shown), or it is style or speculation without a concrete failure.

Later history is supporting evidence, not proof. If a later commit fixed the
same problem, give its SHA. If a later commit intentionally kept the behavior,
say so. The absence of a later fix does not mean a finding is wrong.

Also rate the severity of each confirmed or plausible group: `high` (data
loss, security, crash, hang, or a broken core flow), `medium` (wrong result
or failure in a realistic but narrower case), or `low` (minor or cosmetic).

## Output

Write `verify-out.json` in your working directory (it is untracked; do not
commit it):

```json
{"commits": [{"intro": "<sha>",
  "groups": [{"group": "g1", "findings": ["f-xxxx", "..."],
              "summary": "one sentence", "verdict": "confirmed|plausible|rejected",
              "severity": "high|medium|low|null",
              "evidence": "trace with file:line at intro; later fix SHA if any",
              "confidence": "high|medium|low"}]}]}
```

Every finding ID in your batch must appear in exactly one group. Finish with
a short reply: number of groups per commit and counts per verdict.
