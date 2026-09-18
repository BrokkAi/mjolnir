---
name: provenance
description: Find which earlier coding sessions created or edited a file, and why the code looks the way it does. Use when you need the history behind a file, a function, or a specific line and git alone does not explain it.
---

# Where this code came from

`sessionwiki` records which sessions touched which files. Git tells you what
changed; this tells you which conversation changed it, so you can read the
reasoning. Everything here is read-only.

## Check availability first

```sh
command -v sessionwiki
```

If it is missing, say so once and continue without it. It installs with
`cargo install --locked brokk-sessionwiki@0.30.1`.

Commands sync the index before they read it, which is usually right. If a
scheduled `sessionwiki sync` already keeps the index current, `--no-sync`
skips that step and answers faster.

## Which sessions touched a file

```sh
sessionwiki trace --json src/relay/frame.rs
```

Sessions that edited or created the path, newest first. Start here when the
question is "who wrote this" or "when did this appear".

## Which session wrote a line

```sh
sessionwiki blame src/relay/frame.rs --json
sessionwiki blame src/relay/frame.rs -L 120,160 --json
```

One row per line range, with a `status` of `confident`, `ambiguous` or
`unattributed`. Blame reads git, so run it from inside the repository; outside
one it falls back to `trace`. The unsure statuses are normal: a range may fit
several sessions, or a commit no session recorded. Treat the result as a lead,
not a verdict, and use `-L` to keep the output small.

## What one session changed

```sh
sessionwiki files --json <id>
```

A JSON array of the absolute paths a session edited. Use it to judge whether a
session found by `trace` is the one that matters before you read any of it.

## Then read the session

Once you have a session id, switch to the `recall` loop: `sessionwiki grep
--json "<text>" <id>` to find the passage, and `sessionwiki show <id> --jsonl`
piped through `sed` and `jq` to read a bounded window around it. Continue the
session with `mj resume --wiki <id>` if the user wants the work taken up again.

## Rules

- Pass the path the repository uses. Matching is on recorded paths, not on
  file contents.
- Cite the session id, and the file and lines it explains.
- A recorded edit is evidence of intent at the time, not a rule for now.
  Confirm against the current code before you act on it.
