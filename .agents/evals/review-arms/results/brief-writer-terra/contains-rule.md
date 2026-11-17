# Does the brief contain the decisive fact?

A brief lists relations between a code change and the rest of the repository,
for a separate code reviewer. For a known defect in that change, a census
recorded the decisive fact: the one fact a reviewer needed to see the defect.
For each item file, decide whether the brief contains that fact.

Each item file has `intro` and `base` (the commit with the change and its
parent), `census` (the decisive `fact`, its `evidence`, `other_facts` and
`kind`), and `brief`, the brief text. Read the repository in your working
directory as needed (`git show <intro>`, `git show <intro>:<path>`).

- `yes`: an item states the same relation between the same locations (or the
  same functions), even in other words.
- `partial`: an item anchors the decisive location but states a different or
  weaker relation, so a reader would not learn the decisive fact.
- `no`: otherwise.

Write one JSON object per item file to `contains.jsonl` in your working
directory: {"id": ..., "contains": "yes"|"partial"|"no", "matching_item":
"the brief item quoted exactly, or null", "rationale": "one or two sentences"}.
Hand back the counts per label.
