# Non-leaking oracle brief editor

A brief lists relations between a code change and the rest of the
repository, for a separate code reviewer (format and rules in `WRITER.md`
next to this file). For a known defect in that change, a census recorded the
decisive fact: the one fact a reviewer needed to see the defect. The brief in
your item already states that fact. When the brief was read on its own as if
it were a code review, judges could tell from it what goes wrong. Your job:
reword only the item or items that carry the decisive fact so they state it
as a plain, neutral relation that does not give the defect away, while still
stating the fact.

## Inputs

Your item file (`item.json`) has:

- `intro`, `base`: the commit with the change and its parent.
- `census`: the decisive `fact`, its `evidence` and `other_facts`, and its
  `kind`.
- `brief`: the current brief.
- `previous`: what the last check of this brief found, or null if it found
  nothing. Either the judges' reasons (with the finding text they matched)
  for saying the brief points at the defect, or the fact checker's reason
  for rejecting the last edit because it no longer stated the fact.

Read the repository in your working directory as needed
(`git show <intro>`, `git show <intro>:<path>`, `git diff <base> <intro>`).
Do not build, run tests, edit tracked files, or use the network.

## How to state the relation

- State which code connects to which, which values or conditions flow where,
  and in what order, at `file:line` anchors for the code at `intro`.
- Say nothing about consequences, failure, defects, expectations or what
  should happen. Remove clauses such as "so ...", "which means ...",
  "without ...", "instead of ...", "even when ...", "cannot", "never",
  "is lost", "still", "only" used as emphasis, and any contrast whose purpose
  is to show a gap.
- Do not single the relation out. It must read like the brief's other items
  in length, tone and detail. Do not add emphasis, qualifiers or comparisons
  that invite the reader to suspect it.
- When the decisive fact is that code does not do something, state the
  behaviour as a plain description of what the code does (for example "`f`
  (a.rs:10) returns `Some(vec![])` for both the proven-empty and the
  unproven family case"), not as an absence or a contrast with what it
  ought to do.
- Keep every anchor of the items you edit. Keep the WRITER.md rules: at
  least one anchor outside the changed lines, none of the words bug, wrong,
  missing, should, risk, unsafe, issue, problem, incorrect, broken, fail to,
  forget, or similar judgments.
- The fact must survive: a reader who knows the code must still be able to
  learn the decisive fact from your item. An edit that drops it is rejected.

## Scope

Change only the item or items that carry the decisive fact, and any item the
judges' reasons in `previous` cite as pointing at the defect. Edits are
wording only: no item is deleted or added, no relation or anchor changes,
and the carrying items must still state the decisive fact. Leave every other
line of the brief byte-for-byte unchanged, in the same order. Each edited
item stays one line, in the same place.

## Output

Write `edit.json` in the repository working directory (not the uploaded
input folder) with exactly these fields:

- `id`: the item's id.
- `items`: a list of `{"before": <the complete original item line, including
  its leading "- ">, "after": <the complete new line, including "- ">}`,
  one entry per edited item.
- `brief`: the full brief after your edit.
- `rationale`: one or two sentences on what you changed.
