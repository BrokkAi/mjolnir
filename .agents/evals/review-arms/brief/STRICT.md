# Strict oracle brief editor

A brief lists relations between a code change and the rest of the
repository, for a separate code reviewer (format and rules in `WRITER.md`
next to this file). For a known defect in that change, a census recorded the
decisive fact: the one fact a reviewer needed to see the defect. The brief in
your item states that fact, but when the brief is read on its own as if it
were a code review, judges can tell from it what goes wrong. Your job: change
only the phrasing that states or implies a consequence, a failure, a defect,
an expectation or what should happen, so that every relation stays complete
and the brief no longer points at the defect.

## Inputs

Your item file (`item.json`) has:

- `intro`, `base`: the commit with the change and its parent.
- `census`: the decisive `fact`, its `evidence` and `other_facts`, and its
  `kind`.
- `brief`: the current brief.
- `previous`: what the last check of this brief found, or null. This can be
  the leak judges' reasons, the fidelity checker's list of missing or
  inferred elements, or the cause check's reasons.

Read the repository in your working directory as needed
(`git show <intro>`, `git show <intro>:<path>`, `git diff <base> <intro>`).
Do not build, run tests, edit tracked files, or use the network.

## What must stay

Every relation element of every item you edit stays, stated as fact:

- each site and anchor (`file:line`);
- each value and each condition;
- each rule, such as which range is selected, which key is compared, or
  which default is used and what its value is;
- each order (what happens before or after what);
- each absence: what code does not do, what a function returns without
  checking, which field a key omits. State an absence plainly ("returns
  `Some(false)` without checking hierarchy closure"), never as a contrast
  with what should happen;
- each default and its value.

## What may change

Only phrasing that states or implies a consequence, a failure, a defect, an
expectation or what should happen: "so ...", "which means ...", "is lost",
"incorrectly", "should", "can therefore", "even though", emphasis such as
"only" used to suggest a gap, and contrasts whose purpose is to show a gap
rather than to state an order or absence. Do not delete, merge or add items.
Keep the WRITER.md rules: at least one anchor outside the changed lines, and
none of the words bug, wrong, missing, should, risk, unsafe, issue, problem,
incorrect, broken, fail to, forget, or similar judgments.

Change any item the judges' reasons in `previous` cite, and the items that
carry the decisive fact. Leave every other line byte-for-byte unchanged, in
the same order. Each edited item stays one line, in the same place.

## Output

Write `edit.json` in the repository working directory (not the uploaded
input folder) with exactly these fields:

- `id`: the item's id.
- `items`: a list of `{"before": <the complete current item line, including
  its leading "- ">, "after": <the complete new line, including "- ">}`,
  one entry per edited item.
- `brief`: the full brief after your edit.
- `rationale`: one or two sentences on what you changed.
