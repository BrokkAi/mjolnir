---
title: Headless automation
description: Run one prompt non-interactively with stable text, JSON, or NDJSON output.
---

Use `--print` for a single non-interactive prompt:

```bash
mj --print "summarize the current diff"
git diff | mj --print -
```

If the prompt value is omitted or `-`, Mjolnir reads standard input.

## Permission behavior

Headless mode defaults to `--permission-mode manual`, which rejects prompts
instead of hanging. `auto` can approve supported file changes but rejects shell
execution; `yolo` approves everything and belongs only in disposable scope. The
mode applies to subagent permission requests as well as the primary's.

```bash
mj --cwd /tmp/eval \
  --print \
  --permission-mode manual \
  "inspect the project without changing it"
```

## Output formats

- `text` prints the primary agent's final result.
- `json` emits one object with `session_id`, `resumed`, `result`, `stop_reason`,
  `usage`, `agent_usage`, and `error` fields. `agent_usage` breaks down into
  `primary`, `subagents`, `review`, and a per-model map.
- `stream-json` emits newline-delimited typed records followed by a final
  `result` record.

Stream records can include `connected`, `session_started`, agent messages and
thoughts, tool calls and updates, permissions, reviews, subagent lifecycle,
warnings, errors, and the result. Records carry actor labels, so primary and
subagent activity stays attributable.

### Subagent records

Every background subagent produces `subagent` records:

```json
{"type":"subagent","id":3,"label":"fix-tests","kind":"started","text":"Fix the failing parser tests"}
{"type":"subagent","id":3,"label":"fix-tests","kind":"activity","text":"cargo test -p parser"}
{"type":"subagent","id":3,"label":"fix-tests","kind":"finished","text":"completed","elapsed_ms":252000}
```

`kind` is `started` (text is the objective), `activity` (text is the distilled
activity line), or `finished` (text is the outcome, with `elapsed_ms`). In plain
`text` mode the same lifecycle is printed to stderr as
`subagent #3 · fix-tests · started · …` one-liners, keeping stdout to the final
result.

When a subagent finishes, its `<subagent_result>` report is injected into the
primary session and appears as an ordinary prompt and turn in the stream.

### Draining before exit

A completed turn is not the end of a headless run. Mjolnir exits only when the
primary is idle, no subagent is still running, and no finished subagent's report
is still waiting to be injected and answered. The text or JSON result is the
last turn's answer, so it reflects the reports the primary received rather than
the turn that merely launched the work.

```bash
mj --print --output-format stream-json "summarize this repository" \
  | jq -c 'select(.type == "result" or .type == "subagent")'
```

Treat the machine-readable record shape as an integration contract for the
current release, not an unversioned promise that fields will never grow.

## One-shot model selection

`--model MODEL`, `--review-model MODEL`, and
`--subagent-model MODEL|disabled` override the saved models for one invocation.
They require explicit IDs, accept an optional `+<effort>` suffix, and are never
written back to the config file.

For a controlled first run, use the [10-minute evaluation](/evaluate/). For
networked access to an interactive session, continue with [Mjolnir
Web](/remote/).
