---
title: Headless automation
description: Run one Council prompt with stable text, JSON, or NDJSON output.
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
execution; `yolo` approves everything and belongs only in disposable scope.

```bash
mj --cwd /tmp/eval \
  --print \
  --permission-mode manual \
  "inspect the project without changing it"
```

## Output formats

- `text` prints Thor's final result.
- `json` emits one object with `session_id`, `resumed`, `result`, `stop_reason`,
  `usage`, `council_usage`, and `error` fields.
- `stream-json` emits newline-delimited typed records followed by a final
  `result` record.

Stream records can include `connected`, `session_started`, agent messages and
thoughts, tool calls and updates, permissions, reviews, warnings, errors, and
the result. Council activity carries actor labels so Thor, Eitri, and Loki
remain attributable.

```bash
mj --print --output-format stream-json "summarize this repository" \
  | jq -c 'select(.type == "result" or .actor != null)'
```

Treat the machine-readable record shape as an integration contract for the
current release, not an unversioned promise that fields will never grow.

## One-shot role selection

`--thor MODEL`, `--loki MODEL|disabled`, and `--eitri MODEL|disabled` override
the saved Council for one invocation. They require explicit IDs and are never
written back to the config file.

For a controlled first run, use the [10-minute evaluation](/evaluate/). For
networked access to an interactive Council, continue with [Remote control](/remote/).
