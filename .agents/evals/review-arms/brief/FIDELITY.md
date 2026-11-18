# Fidelity check of edited relation briefs

Each `item-*.json` file in this folder holds one edited brief. Each also
holds:

- `pairs`: every line that differs from the original brief, as
  `{"original": <original item line>, "edited": <edited item line>}`;
- `census`: the decisive fact recorded for this change (`fact`, `evidence`,
  `other_facts`, `kind`), with the change's `intro` commit and its parent
  `base`;
- `brief`: the full edited brief, and `brief_sha256`.

The edits were meant to change only phrasing that states or implies a
consequence, a failure or what should happen, and to keep every relation.
Check that strictly.

For each pair, list every relation element of the original line:

- each site and anchor;
- each value and each condition;
- each rule (selection, comparison, key, default and its value);
- each order (before or after);
- each absence (what code does not do or check, what a key omits).

For each element, decide whether the edited line still states it explicitly.
An element a reader would have to infer, or that is only implied by
surrounding lines, is not kept. Use the repository in your working directory
(`git show <intro>:<path>`) when you need to check what an element refers to.
Do not build, run tests, edit tracked files, or use the network.

Then decide whether one line of the edited brief states the census fact
explicitly, so that a reader learns it from that line without inference.

Write `fidelity.jsonl` in the repository working directory, one line per
item file, each a JSON object:

- `id` and `brief_sha256`, copied from the item file;
- `elements`: a list of `{"pair": <index into pairs>, "element": <short
  description>, "kept": true | false}` covering every element of every pair;
- `carrying_item`: the complete edited-brief line that states the census
  fact, copied exactly with its leading `- `, or null;
- `fact_inferred`: true if stating the census fact needs inference or more
  than one line, otherwise false;
- `decision`: `pass` only if every element is kept, `carrying_item` is set
  and `fact_inferred` is false; otherwise `fail`;
- `why`: one or two sentences, naming every element that was not kept.
