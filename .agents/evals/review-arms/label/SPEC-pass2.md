# Missing-fact labeling, second pass: defects visible in the diff

We are testing whether a static analysis tool can improve code review. A
first pass labeled each defect with the one fact a reviewer needed that was
not in the diff. For your item, the first pass found that everything needed
was already visible in the diff or the commit message.

But the defect was still committed: the author missed it, and for Hard-won
items so did the reviewers of the time. Being visible was not enough. Your
question: **what fact could a static analysis tool have stated explicitly, so
that a reviewer would notice the defect, if any?** The fact may be derivable
from the diff; what matters is that a tool would say it out loud instead of
leaving the reader to work it out.

## Inputs

Your item file is a JSON object with:

- `intro`: the commit that introduced the defect; `base` is its parent. The
  reviewer saw `git diff base intro` plus the commit message.
- `defect`, and for Hard-won items `location`, `trigger` and `fix_commit`;
  for sampled items the verifier's `evidence`.
- `first_pass`: the first pass's fact and evidence, explaining why it judged
  the defect visible in the diff.

The repository in your working directory has the full history. Read
`git show <intro>`, the code at `intro`, and the fix if named. Do not build,
run tests, edit files, or use the network.

## What to decide

1. **Find the fact.** Name the single statement a static analysis tool could
   have produced that points most directly at the defect. State it as a
   concrete claim about the code at `intro`, with `file:line`. Examples of the
   form: "`RenameFocus::button_index` returns 1 for both `Field` and `Save`,
   so the two focus states render identically"; "`session_projection` holds
   `self.controller` when it calls `active_lifecycles`, which now locks
   `self.controller`"; "the new branch at x.rs:120 is unreachable because
   `mode` is never `Idle` there".
2. **Label its kind**, exactly one of:
   - `affected callers`: who uses the changed code, and what they assume.
     Tool: find references, callers or implementations.
   - `path`: how execution reaches the code, or another route to the same
     state; ordering; reachability. Tool: call graph, control flow.
   - `value flow`: where a value comes from or goes; which values a
     variable or field can hold; two expressions that are always equal.
     Tool: data flow, def-use, constant or range propagation.
   - `none`: no static analysis fact would point at the defect. Set
     `none_reason`:
     - `intent`: seeing it requires knowing what the code should do (a
       product rule or expected behavior). Any tool output would be a true
       but neutral fact that does not single out this code.
     - `logic`: the defect is a reasoning error inside the new code (wrong
       arithmetic, scoring weights, an inverted condition) that a tool
       cannot state as a fact without knowing the intended result.
     - `external`: it depends on behavior outside the repository.
   When two kinds fit, pick the one a tool would have to answer first and put
   the other in `alt_kind`.
3. **Describe the tool output.** In `tool_output`, write what the tool would
   show the reviewer, for example a warning text or an annotation on a line.
   In `query`, write the question the tool answers. Null for `none`.
4. **Rate specificity.** Would this output single out the defect, or would a
   tool that produces it also flag many harmless places in an ordinary diff?
   - `high`: the output is rare and points at the defect, like a self-lock
     or a branch that can never run.
   - `medium`: the output is true of a handful of places in a typical diff
     and a reviewer would read it.
   - `low`: the output is a true fact of a kind the tool would print for
     much of the code; a reviewer would skim past it.
   Explain in `specificity_reason`.

## Output

Return one object through the StructuredOutput tool, with fields `id`,
`fact`, `kind`, `none_reason`, `alt_kind`, `query`, `tool_output`,
`specificity`, `specificity_reason`, `agrees_with_first_pass` (true if you
also think the defect is fully visible in the diff), `evidence` (what you
read, with `file:line` at intro) and `confidence`. Use plain, literal
language.
