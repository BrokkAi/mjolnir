# Judging specification

You decide, for each code-review output, whether it identifies one known bug.

## Inputs

Each batch file `batch-<n>.json` lists bugs. For each bug it gives:

- the defect record: `defect`, `location`, `trigger`, `match_rule`,
  `near_misses`, `why_findable`, and the introducing and fix commits;
- several review outputs, each with an opaque `output_id` and its full text.

The outputs come from different reviewers of the same diff. You are not told
which model or setup wrote which output. Do not try to infer it, and judge
every output by the same standard.

You may read the repository at
a mjolnir checkout with full history (the path is given in your instructions)
read-only (`git show <intro>`, `git show <fix>`, `git show <intro>:<path>`) to
understand the defect or a finding. Do not modify it.

## Decision

For each output, find the finding that best matches the known defect.

- `caught`: a finding identifies the same defect: the same faulty behavior
  and its cause, consistent with `match_rule`, even in other words, with a
  different trigger, or with a different proposed fix.
- `partial`: a finding points at the right code and a related symptom, but
  misses the mechanism or names a different cause, so an author would likely
  not fix the known bug from it alone.
- `missed`: no finding meets either bar. Findings that only name the same
  file or function, or describe a different problem there (see
  `near_misses`), are `missed`.

When unsure between two labels, pick the lower one and say why in
`rationale`.

Also count the distinct findings in each output (`finding_count`; 0 for
"No findings.").

## Output

Write one JSON object per output to `verdicts.jsonl` in your report
directory:

```json
{"output_id": "...", "bug": "...", "label": "caught|partial|missed",
 "matching_finding": "short quote from the output, or null",
 "rationale": "one to three sentences",
 "finding_count": 0, "confidence": "high|medium|low"}
```

Hand back the number of outputs judged and a list of every `low` confidence
verdict.
