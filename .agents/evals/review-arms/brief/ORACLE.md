# Oracle brief editor

A blind brief lists relations between a code change and the rest of the
repository, for a separate code reviewer (format and rules in
`brief/WRITER.md` next to this file). For a known defect in that change, a
census recorded the decisive fact: the one fact a reviewer needed to see the
defect. Your job: make sure the brief contains that fact as a neutral
relation, without saying what goes wrong.

## Inputs

Your item file (path in your instructions) has:

- `intro`, `base`: the commit with the change and its parent.
- `census`: the decisive `fact`, its `evidence` and `other_facts`, and its
  `kind`. For second-pass items, `allow_in_diff` is true.
- `blind_brief`: the brief as written.

Read the repository in your working directory as needed
(`git show <intro>`, `git show <intro>:<path>`). Do not build, run tests,
edit files, or use the network.

## Steps

1. **Does the blind brief contain the decisive fact?**
   - `yes`: an item states the same relation between the same locations (or
     the same functions), even in other words.
   - `partial`: an item anchors the decisive location but states a different
     or weaker relation, so a reader would not learn the decisive fact.
   - `no`: otherwise.
2. **For `partial` or `no`, write one item** that states the decisive relation:
   - Only the relation: who calls what, what runs where, which value goes
     where, at which `file:line` (for the code at `intro`). Cut every clause
     that says what goes wrong or what follows from it ("so ...", "which
     means ...", "cannot", "never reaches", "is lost"), unless the clause is
     itself a plain relation between code locations.
   - Follow the brief rules: `file:line` anchors, at least one anchor outside
     the lines the diff changes (unless `allow_in_diff` is true, when it may
     relate two changed locations), and none of the words bug, wrong,
     missing, should, risk, unsafe, issue, problem, incorrect, broken, fail
     to, forget, or similar judgments.
   - When the decisive fact is that code does not do something (a path a
     script leaves in place, a check a route skips, a field a writer never
     sets), state that explicitly with the exact name, for example "removes
     `a` and `b` under the root but not `<root>/c`". A contrast with other
     code is not enough.
   - State it in the same tone as the brief's other items. It must not stand
     out by length, emphasis or wording.
   - Put it in the section that fits (Affected callers, Paths, Value flow) at
     the position of the changed code it relates to, in diff order. If the
     brief already has 25 items, replace the item least related to the
     changed code.
   - For `partial`, replace the partial item with yours instead of adding.
3. **Return** the full oracle brief text: the blind brief with your edit and
   nothing else changed. For `yes`, return the blind brief unchanged.

Return through the StructuredOutput tool: `id`, `contains` (yes, partial,
no), `matching_item` (the blind item that matches or partly matches, or
null), `inserted_item` (your item, or null), `replaced_item` (or null),
`oracle_brief`, and `rationale` (one or two sentences).
