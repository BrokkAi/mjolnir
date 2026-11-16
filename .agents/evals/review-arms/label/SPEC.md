# Missing-fact labeling

We are testing whether a static analysis tool can improve code review. For
each commit in your batch you get a defect that the commit introduced. Your
job: find the one fact a reviewer needed in order to see that defect that was
**not in the diff**, and label what kind of fact it is.

## Inputs

Your batch file is a JSON list. Each item has:

- `id`: copy it into your output.
- `intro`: the commit that introduced the defect. `base` is its parent; the
  reviewer saw `git diff base intro` plus the commit message.
- `defect`: what is wrong. For Hard-won items it comes with `location`,
  `trigger` and `fix_commit` (the commit that fixed it). For sampled items it
  is a problem that a verifier confirmed, with the verifier's `evidence`,
  which may name a later fix.

The repository in your working directory has the full history through
2026-11-13. Read `git show <intro>`, the code at `<intro>`
(`git show <intro>:<path>`, `git grep <pattern> <intro>`), and the fix if one
is named. Do not build or run tests. Do not use the network.

## What to decide

1. **Check the defect.** Confirm from the code at `intro` that it is real.
   If you cannot confirm it, say so in `defect_check` and still label the fact
   the defect claim depends on.
2. **Find the fact.** Imagine a careful reviewer who reads the diff and the
   commit message and knows nothing else about the repository. What is the
   single most decisive thing they would have had to learn to see the
   defect? State it as a concrete claim about the code at `intro`, with
   `file:line` at `intro`. For example: "`resume_session`
   (mj-controller/src/sessions.rs:812) also calls `attach_worker`, without
   the lock the diff adds", not "the reviewer needed more context".
   If several facts are needed, pick the one without which the others are
   useless, and list the others in `other_facts`.
3. **Confirm it is not in the diff.** If the decisive fact is visible in
   the diff (including its context lines) or the commit message, the kind is
   `none` with `none_reason: in_diff`.
4. **Label its kind**, exactly one of:
   - `affected callers`: who uses the changed code outside the diff, and what
     they assume. Examples: a caller of a changed function that relies on the
     old return value; another implementation of a changed trait; a reader of
     a changed field, file format or message. A tool that answers it: find
     references, callers or implementations of a symbol the diff changes.
   - `path`: how execution gets to the code, or another route to the same
     state. Examples: a call chain from an entry point that reaches the new
     code in an unexpected state; a second code path (restart, retry, error
     path, other handler) that skips the new check; an ordering of events
     across functions. A tool that answers it: call-graph paths, reachability
     or control flow across functions.
   - `value flow`: where a value comes from or where it goes. Examples: the
     set of values a field can hold because of where it is assigned; a
     default supplied elsewhere; a value that is later written to storage or
     sent on the wire unchanged; a struct field that is never set on one
     construction path. A tool that answers it: data flow, def-use chains,
     taint or constant propagation.
   - `none`: no repository fact a code analysis tool could supply. Set
     `none_reason` to one of:
     - `in_diff`: the defect is visible from the diff's own logic.
     - `intent`: the missing fact is what the code is supposed to do: a
       product rule, a requirement, a project convention, or a documented
       design decision.
     - `external`: the missing fact is how something outside the repository
       behaves: a third-party tool, OS, protocol specification, or service.

   When two kinds fit, choose the one that a tool would have had to answer
   first to surface the fact, and put the other in `alt_kind`. Callers versus
   path: if the fact is *that* some code uses the changed symbol, it is
   callers; if it is *how or when* execution arrives somewhere, it is path.
   Path versus value flow: if the fact is about control (which code runs), it
   is path; if it is about data (what a value is), it is value flow.
5. **Write the query.** In `query`, give the question a tool would have to
   answer to produce the fact, phrased against the code at `intro`, for
   example "callers of `SessionStore::mark_idle` outside mj-controller/src/turns.rs".
   Use null for `none`.

## Output

Write `label-out.json` in the root of your working directory. Do not commit
it. It must contain one entry for every item in your batch:

```json
{"labels": [
  {"id": "...",
   "defect_check": "confirmed | not confirmed: <reason>",
   "fact": "<one sentence, concrete, with file:line at intro>",
   "kind": "affected callers | path | value flow | none",
   "none_reason": "in_diff | intent | external | null",
   "alt_kind": "<kind> | null",
   "query": "<tool question> | null",
   "other_facts": ["..."],
   "evidence": "<what you read at intro, with file:line, that shows the fact and that it is absent from the diff>",
   "confidence": "high | medium | low"}
]}
```

Use plain, literal language. When you finish, reply with one line per item:
`<id>: <kind>`.
