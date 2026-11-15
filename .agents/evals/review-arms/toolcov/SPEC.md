# Tool coverage: can a static code graph produce the decisive fact?

For each defect in your list, a census labeled the one fact a reviewer needed
that was not in the diff (`fact`, with `file:line` anchors at the introducing
commit) and the question a tool would answer to produce it (`query`). Your
job: find out whether the released Bifrost CLI produces that fact today when
asked only about things the diff shows, and if not, why not.

## Inputs

- The census records:
  `/home/jonathan/Projects/mjolnir/.mj/clones/dbcece95f1cdc80785d2df3f995138f2/.agents/evals/review-arms/results/missing-facts.jsonl`
  (fields `id`, `intro`, `defect`, `fact`, `kind`, `query`, `evidence`), and
  for second-pass items also `results/missing-facts-pass2.jsonl` in the same
  directory, whose `fact` and `query` replace the first pass's.
- A worktree at each record's `intro`: `/mnt/optane/tmp/toolcov/<id>`. The
  base commit is `intro^`. Do not modify the worktree.
- Bifrost 0.15.2 at `bifrost` on PATH. `bifrost --help` lists the tools;
  `bifrost --help <tool>` prints one tool's schema. Run one tool with
  `bifrost --root /mnt/optane/tmp/toolcov/<id> --tool <name> --args '<json>'`.
  Reference: https://bifrost.brokk.ai/reference/mcp-tools/ (only if the help
  text leaves a question).

## Method, per record

1. Read `git -C <worktree> show HEAD` and the record. Find the decisive
   anchor: the `file:line` in `fact` that lies outside the changed lines.
   For second-pass items (those in `missing-facts-pass2.jsonl`) the decisive
   relation can lie entirely inside the diff; then the decisive anchor is
   the changed location the relation connects to the one the diff makes
   most visible, and "found" means a tool output states that connection.
2. Rewrite `query` so that it names only symbols, files and lines visible in
   the diff. Do not name the decisive anchor or anything you know only from
   the record.
3. Ask Bifrost. Start from the symbols the diff changes or newly uses:
   `analyze_diff` (base `HEAD^`, target `HEAD`), the usage scans
   (`scan_usages_by_location` and the reference-based scan that
   `bifrost --help` lists), `usage_graph`, `get_definitions_by_location`, and
   `query_code` if the others cannot express the question. Use what a tool
   user who sees only the diff would reasonably try; a few calls, not a
   search for the answer.
4. Record for each call: the tool and arguments, whether its output contains
   the decisive anchor (same file and a line within the same function or
   statement counts), how many items it returned, the rank of the decisive
   item, any completeness, truncation or incompleteness status it reported,
   wall time, and any error or timeout. Cap each record at 15 minutes of tool
   time. A failure is a result; do not work around it.
5. Classify the record:
   - `A` (computed today): a call returned the decisive anchor from a
     diff-only query, in output that states the decisive relation itself
     (for example a caller-to-callee edge, or a reference from the changed
     symbol to the anchor), not only a list that happens to contain the
     anchor's file and line.
   - `A0` (anchor only): a call returned the decisive anchor, but its output
     does not state the relation (for example, it lists the caller but not
     the lock it holds); say in `missing` what the output leaves out.
   - `B` (graph relation not exposed today): not returned, but the fact is one
     of these over Rust code: call graph, references to a symbol, field or
     constant, the same function called with the same constant argument, a
     lock held at a call site, error propagation along a call chain, value
     flow.
   - `C` (outside a code graph): it crosses languages (Rust to JavaScript,
     shell or SQL), depends on runtime or event ordering, concurrency
     interleaving or database semantics, or is a claim that no path exists.
   For `A0`, `B` and `C`, name the missing piece in one clause (`missing`). For `A`,
   say in `note` whether the reviewer could have found the anchor in that
   output (rank and count).

Return one object per record through the StructuredOutput tool. Use plain,
literal language.
