---
title: Evaluate Codex in Mjolnir in ten minutes
description: Exercise Codex, subagents, adversarial review, resume, and headless output in a disposable fixture.
---

> Fixture and CLI surface reviewed against Mjolnir 1.0.2 on 2026-07-26.
> Live provider output is model- and availability-dependent and is not run in docs CI.

This evaluation uses Codex with a checked-in Python fixture in a disposable Git
repository. It proves that a configured Codex session can inspect a small project,
delegate a bounded change to a background subagent, surface that subagent's live
status and its pushed-back report, run an explicit review, preserve a resumable
session, and emit headless stream records.

It does not prove large-repository performance, consistent quality across
models, predictable provider cost, safe unattended shell execution, or
remote-server security.

## Before you start

You need:

- Mjolnir installed and `mj --version` working.
- Python 3 for the fixture test.
- Git.
- An authenticated, launchable Codex route. Codex use may cost money.

Run `mj`, open `/mjconfig`, and confirm on the Agent tab that the primary
resolves through Codex. Confirm the subagent and review seats as well if you
want this run to remain Codex-only. Model or ACP-server changes apply to the
next session, so exit and relaunch after changing them.

## Prepare the disposable fixture

From a Mjolnir checkout:

```bash
EVAL_DIR="$(mktemp -d)/mjolnir-eval"
cp -R docs/fixtures/ten-minute-evaluation "$EVAL_DIR"
cd "$EVAL_DIR"
git init
git add .
git -c user.name="Mjolnir Eval" \
    -c user.email="mjolnir-eval@example.invalid" \
    commit -m "evaluation fixture"
python3 -m unittest -v
```

The baseline has two passing tests. The requested edge cases are intentionally
absent.

If you are reading the published docs, clone the repository first:

```bash
git clone https://github.com/BrokkAi/mjolnir.git
cd mjolnir
```

Then run the preparation block above.

## Journey 1: delegated implementation

Start Mjolnir in the fixture:

```bash
mj --cwd "$EVAL_DIR"
```

Send this prompt:

```text
Launch a subagent for this bounded change. Update weather.py so status(0)
returns "freezing", negative values return "below freezing", values below 20
return "cold", and all other values return "warm". Add focused tests, run
python3 -m unittest -v, and explain the result. Do not change anything else.
```

Expected observations:

1. The primary agent launches a subagent and ends its turn instead of waiting.
2. A stable `Subagents` workflow row appears with elapsed time and aggregate
   running/completed/failed/cancelled counts. `/subagents` opens actor #1's live
   detail and transcript; the terminal outcome remains until the next user
   turn.
3. Any requested permission remains fully readable before you decide, and is
   labelled with the subagent's id.
4. When the subagent finishes, its report is injected as a new user turn and the
   primary responds to it.
5. The returned change is limited to `weather.py` and `test_weather.py`.
6. `python3 -m unittest -v` reports four passing tests.
7. Because the completed turn changed the workspace, a discrete review may run
   before the turn is released; delegation is not required. A stable `Review`
   workflow row shows its current phase and reviewer progress.

The exact wording and tool sequence can differ by model. If the primary ignores
the explicit delegation, polls for a result, or the change is wrong, record the
selected models and adapter in your evaluation notes; that is a failed outcome,
not a docs failure.

## Journey 2: explicit review

Run:

```text
/review recent
```

Choose the most recent change-producing turn. Review is findings-only; a clean
result is valid.

Exit with Ctrl-D on an empty prompt. Mjolnir prints a command shaped like:

```bash
mj resume <session-id>
```

Run the printed command. The session should return through its saved ACP adapter
and model provenance. When a worktree was used, pass its printed
`--worktree <name>` value to reuse that directory.

## Journey 3: headless read-only output

From another terminal:

```bash
mj --cwd "$EVAL_DIR" \
  --print \
  --permission-mode manual \
  --output-format stream-json \
  "Inspect weather.py and summarize its behavior. Do not modify files." \
  | tee /tmp/mjolnir-eval.ndjson
```

The output is newline-delimited JSON. It should contain connection/session
records, actor-labelled message or thought records, any `subagent` lifecycle
records, and a final `result` record. `manual` rejects any permission request
rather than hanging an unattended run, and the process does not exit until every
subagent report has been delivered.

## Interpret the result

A successful run proves the selected Codex route can support the core
delegation path on one small repository. Compare models or advanced provider
routes by repeating the same fixture and recording model IDs, elapsed time,
token/cost telemetry, whether the
delegation occurred, test outcome, review outcome, and any manual intervention.

Before broader use, read [Permissions and workspace scope](/permissions/) and
[Data and trust boundaries](/data-boundaries/).
