# RQL census labelling

This is the labeller's task for Stage 1 of `../PLAN-rql-census.md`. Two labellers from different families label every bug independently: Sol/high is the primary, and Opus/high is the replication. Neither sees the other's labels.

You get a batch of bugs. Each one is a defect that a commit introduced into a Rust repository and that a later commit fixed. We want to know which facts a code reviewer needed to see the defect, and whether Bifrost's query language (RQL) could have supplied those facts.

## Inputs

Each item in your batch file has:

- `id`, `repo`: copy them into your output.
- `intro_commit`: the commit that introduced the defect. `base_commit` is where the reviewer's diff starts. The reviewer saw `git diff <base> <intro>` and the commit message, and nothing else.
- `fix_commit` and `fix_test`: the commit that fixed the defect and the test it added or changed.
- `defect`, `trigger`, `location`: what is wrong, how it shows up, and where.

The repository in your working directory has history through its pinned head. Read the introducing diff, the code at `intro_commit` (`git show <intro>:<path>`, `git grep <pattern> <intro>`), and the fix. Don't build, run tests or use the network.

The RQL reference is `rql-documented-steps.md`, next to this file. It lists every documented step at Bifrost pin `bifrost-dev 060087c25162d4846031f1257f3704db360c642d`, with its input and output row kinds, and the composition rules. Only steps in that reference count as documented.

## What to decide, per bug

1. **Check the defect.** Confirm from the code at `intro_commit` that the defect is real. Set `defect_check` to `confirmed`, or to `not_confirmed` with a reason, and keep labelling the facts the defect claim depends on.

2. **Decisive facts, as minimal sufficient sets.** A fact is a concrete claim about the code at `intro_commit`, with its sites as `path:line` at `intro_commit`. For example: "`Session::resume` (src/session.rs:812) calls `attach_worker` without the lock the diff adds". Don't write "the reviewer needed more context".
   - A set of facts is *sufficient* when a careful reviewer who reads the diff and learns every fact in the set would see the defect.
   - It is *minimal* when dropping any one fact makes it insufficient.
   - When the defect can be seen in more than one way, give every distinct minimal set. Most bugs have one or two.
   - For each fact, record:
     - `in_diff`: true when the diff (including its context lines) or the commit message shows it.
     - `relation`: true when the fact relates two or more code sites, such as a caller and its callee's contract, a writer and a reader, a value flowing between functions, an implementation and its trait, or two paths to one state. A fact about one site is not a relation fact, even if that site is outside the diff.

3. **Relation bug.** `relation_bug` is true when any fact in any decisive set has `relation: true`. This is the census's primary membership, decided by the primary labeller's label.

4. **Kind and RQL expression, for every fact with `in_diff: false`.** These are the facts a brief could have supplied.
   - `kind`: exactly one of:
     - `callers`: who uses a changed symbol and what they assume (references, callers, implementations, overrides).
     - `path`: how execution reaches a site, or another route to the same state (call chains, control flow, ordering).
     - `value_flow`: where a value comes from or goes (assignments, defaults, fields, arguments, returns).
     - `shape`: a type, field, variant or signature fact about one declaration outside the diff.
     - `other`: describe it.
   - `rql.row_role`, for `documented` and `missing`: `states` when the query's decisive row itself carries the fact, for example a `field-write-value` row giving the value written, or a `call-input` row giving the argument. Use `locates` when the row only points at the site where the fact lives and a reader must open that code, for example a `callees` row whose body shows the behavior. Use null for `not_expressible`.
   - `rql.class`: exactly one of:
     - `documented`: a query built only from forms in the reference returns a row that states this fact or locates the site where it lives; `rql.row_role` records which. A row that locates the site counts, as long as the fact can be read at that site. Write the query in `rql.query` in RQL form: a start form (a structural pattern or a source such as `occurrences`) followed by the steps in order. Use only forms the reference documents, and make each step's input match the rows the form inside it produces. Count a step as documented even when the reference doesn't establish its Rust support; whether today's Bifrost answers is measured separately.
     - `missing`: no documented query returns a row that states or locates the fact, but one new step would. Give `rql.missing_step` as `{name, input, output, semantics}`, where semantics says what the step starts from, which constructs it crosses (calls, assignments, fields, returns, control flow), and to what depth. Also write the query you'd use with that step added.
     - `not_expressible`: no single query step could supply it. Set `rql.reason` to `intent` (a product rule, requirement or convention), `external` (how a third-party library, OS, protocol or service behaves) or `other` (describe it).

5. **Anchoring, for each `documented` or `missing` query.**
   - `anchors_on_touched`: true when the query starts from a touched symbol: one whose definition the diff changes, or one referenced in a changed line.
   - `anchor`: the symbol or pattern the query starts from.
   - `hops`: the number of steps from the start to the row that states the fact, counting each step that crosses from one code site to another. `filter` and `project` don't count.

Row counts and whether today's Bifrost returns the decisive row are measured later by running Bifrost. Don't estimate them.

## Development bugs

For development bugs (the batch says `"census": "development"`), label only steps 1, 3 and 4, and give one decisive set. The menu of added steps comes from these labels.

## Output

Write one JSON object per bug to the output file named in your batch, one per line:

```json
{"id": "...", "repo": "...", "labeller": "primary|replication",
 "defect_check": "confirmed", "defect_check_reason": null,
 "decisive_sets": [
   {"facts": [
     {"claim": "...", "sites": ["src/a.rs:120", "src/b.rs:44"], "in_diff": false, "relation": true,
      "kind": "value_flow",
      "rql": {"class": "documented", "query": "...", "missing_step": null, "reason": null, "row_role": "locates"},
      "anchoring": {"anchors_on_touched": true, "anchor": "Config::timeout", "hops": 2}}
   ]}
 ],
 "relation_bug": true,
 "notes": "..."}
```

Facts with `in_diff: true` have `kind`, `rql` and `anchoring` set to null. Every site must exist at `intro_commit`. Keep `notes` to what a later reader needs, such as a doubt about the defect or why two sets differ.
