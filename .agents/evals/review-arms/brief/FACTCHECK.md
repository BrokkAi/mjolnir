# Fact check of relation briefs

Each `item-*.json` file in this folder holds one brief: a list of relations
between a code change and the rest of a repository. Each also holds a
decisive fact recorded for that change: `census.fact`, with its `evidence`,
`other_facts` and `kind`, and the change's `intro` commit and its parent
`base`.

For each brief, decide whether the brief states the decisive fact:

- `carries`: one item of the brief states the same relation between the same
  code locations (or the same functions) as `census.fact`, possibly in other
  words, so that a reader would learn the decisive fact from that item.
  Neutral wording is expected; the item need not say what goes wrong.
- `does not carry`: no item states it. An item that only anchors the same
  location but states a different or weaker relation does not carry it.

Use the repository in your working directory (`git show <intro>`,
`git show <intro>:<path>`) to check that an item and the fact describe the
same code. Do not build, run tests, edit tracked files, or use the network.

Write `factcheck.jsonl` in the repository working directory, one line per
item file, each a JSON object:

- `id`: the item's id.
- `brief_sha256`: copied from the item file.
- `decision`: `carries` or `does not carry`.
- `item`: for `carries`, the complete brief line that carries the fact,
  copied exactly, including its leading `- `; otherwise null.
- `why`: one or two sentences.
