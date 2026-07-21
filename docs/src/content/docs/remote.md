---
title: Remote and headless
description: Run the Mjolnir Council from scripts and remote clients.
---

## One-shot prompts

```bash
mj --print "summarize the current diff"
git diff | mj --print -
```

Use `--output-format json` for a final structured result or `stream-json` for role-labelled Thor, Loki, and Eitri activity.

## Remote control

```bash
mj server
```

The remote server uses the same resolved Council as the terminal client. Authentication, networking, and deployment guidance will be expanded in this section.
