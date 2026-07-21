---
title: Evaluate Mjolnir in ten minutes
description: Exercise Thor, Eitri, review, resume, and headless output in a disposable fixture.
---

> Fixture and CLI surface reviewed against Mjolnir 1.0.2 on 2026-07-21.
> Live provider output is model- and availability-dependent and is not run in docs CI.

This evaluation uses a checked-in Python fixture in a disposable Git
repository. It proves that a configured Thor session can inspect a small
project, request an Eitri implementation handoff, present role-labelled work,
run an explicit review, preserve a resumable session, and emit headless stream
records.

It does not prove large-repository performance, consistent quality across
models, predictable provider cost, safe unattended shell execution, spontaneous
Loki timing, or remote-server security.

## Before you start

You need:

- Mjolnir installed and `mj --version` working.
- Python 3 for the fixture test.
- Git.
- At least one authenticated, launchable provider route. Provider use may cost money.

Run `mj`, open `/mjconfig`, and confirm that Thor and Eitri resolve to available
models. Loki is optional. Model or ACP-server changes apply to the next
session, so exit and relaunch after changing them.

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

## Journey 1: Council implementation

Start Mjolnir in the fixture:

```bash
mj --cwd "$EVAL_DIR"
```

Send this prompt:

```text
Use your implementation agent, Eitri, for this bounded change. Update weather.py
so status(0) returns "freezing", negative values return "below freezing", values
below 20 return "cold", and all other values return "warm". Add focused tests,
run python3 -m unittest -v, and explain the result. Do not change anything else.
```

Expected observations:

1. Thor owns the user turn and presents the plan or handoff.
2. An Eitri-labelled implementation run appears in the normal transcript.
3. Any requested permission remains fully readable before you decide.
4. The returned change is limited to `weather.py` and `test_weather.py`.
5. `python3 -m unittest -v` reports four passing tests.

The exact wording and tool sequence can differ by model. If Thor ignores the
explicit handoff or the result is wrong, record the selected models and adapter
in your evaluation notes; that is a failed outcome, not a docs failure.

## Journey 2: explicit review

Run:

```text
/review recent
```

Choose the most recent change-producing turn. Review is findings-only; a clean
result is valid. Loki's background advice is best-effort and may arrive before
or after this step, so spontaneous advice is not a pass condition.

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
records, role-labelled message or thought records, and a final `result` record.
`manual` rejects any permission request rather than hanging an unattended run.

## Interpret the result

A successful run proves the selected provider route can support the core
Council path on one small repository. Compare providers by repeating the same
fixture and recording model IDs, elapsed time, token/cost telemetry, whether the
handoff occurred, test outcome, review outcome, and any manual intervention.

Before broader use, read [Permissions and workspace scope](/permissions/) and
[Data and trust boundaries](/data-boundaries/).
