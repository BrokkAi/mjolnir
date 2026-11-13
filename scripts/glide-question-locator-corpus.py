#!/usr/bin/env python3
"""Build the question-locator evaluation corpus, the split and the labeling queues.

Inputs, read-only:
  * every worker decision log `<data dir>/workers/*/jev-decisions/decisions.*.jsonl`
    (records whose `checked` is "Which reply contains the open question?",
    deduplicated by `id`, last line wins, single-reply records dropped);
  * the fixtures in `mj-core/tests/jev-scenarios/*.json` that carry
    `outcome.request_message_ids` and have at least two assistant replies after the
    last user message (the worker sends no locator request for fewer).

Outputs under --out (private: transcript text; never copy it off this host):
  cases.jsonl, fixtures.jsonl     one record per logged case / usable fixture
  split.json                      written before any queue or evaluation uses the data
  split-report.md                 counts per side and per stratum
  labeling/                       LABELING-INSTRUCTIONS.md and pass{1,2}/batch-{1..4}.md
  private/                        key-pass{1,2}.json mapping item -> case/fixture
                                  (labelers must never read this directory)

`build_state` mirrors `mj_core::activity::question_locator::build`. Standard library
only; run as `python3 -I scripts/glide-question-locator-corpus.py --out DIR`.
"""
import argparse
import datetime
import glob
import json
import random
import sys
from collections import Counter, defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "mj-core/tests/jev-scenarios"
WORDING = ROOT / "mj-core/src/activity/question_locator.json"
LOGS = Path.home() / ".local/share/mjolnir/workers"
CHECKED = "Which reply contains the open question?"

MAX_REPLIES = 32
USER_BYTES = 4096
REPLY_BYTES = 3000
STATE_BYTES = 60 * 1024
ELLIPSIS = " [...] "

SPLIT_SEED = 20261113
SPLIT_SEED_SEARCH = 5000  # candidate seeds scored for case-level balance; Jev answers only
QUEUE_SEEDS = {1: 1101, 2: 2202}
HIDDEN_SHARE = 0.15
BATCHES = 4


# ---------------------------------------------------------------- state builder


def head(text, limit):
    return text.encode()[:limit].decode(errors="ignore")


def tail(text, limit):
    data = text.encode()
    data = data[max(0, len(data) - limit):]
    while data and (data[0] & 0xC0) == 0x80:  # drop continuation bytes left by the cut
        data = data[1:]
    return data.decode()


def clip(text, budget):
    if len(text.encode()) <= budget:
        return text
    return head(text, budget * 2 // 3) + ELLIPSIS + tail(text, budget // 3)


def build_state(messages):
    """Mirror of `question_locator::build`: (state, reply message ids, any reply clipped)."""
    users = [i for i, m in enumerate(messages) if m["role"] == "user"]
    start = users[-1] + 1 if users else 0
    replies = [m for m in messages[start:] if m["role"] == "assistant"][-MAX_REPLIES:]
    latest_user = tail(messages[users[-1]]["text"], USER_BYTES) if users else ""
    budget = REPLY_BYTES
    while True:
        state = {
            "latest_user_message": latest_user,
            "replies": [{"reply": j, "text": clip(m["text"], budget)} for j, m in enumerate(replies)],
        }
        # Rust serializes compactly; the old eval script used the default (spaced) form.
        size = len(json.dumps(state, ensure_ascii=False, separators=(",", ":")).encode())
        if size <= STATE_BYTES or budget < 64:
            clipped = any(len(m["text"].encode()) > budget for m in replies)
            return state, [m["id"] for m in replies], clipped
        budget = budget * 4 // 5


# ---------------------------------------------------------------- logged cases


def answer_type(choice, count):
    if choice == "none":
        return "none"
    if choice and choice[0] == "r" and choice[1:].isdigit() and int(choice[1:]) < count:
        return "newest" if int(choice[1:]) == count - 1 else "older"
    return "invalid"


def iso(ms):
    return datetime.datetime.fromtimestamp(ms / 1000, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def load_logged(logs):
    last = {}
    lines = 0
    for path in sorted(glob.glob(str(logs / "*/jev-decisions/decisions.*.jsonl"))):
        with open(path, errors="replace") as handle:
            for position, line in enumerate(handle):
                if CHECKED not in line:
                    continue
                try:
                    record = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if record.get("checked") != CHECKED:
                    continue
                lines += 1
                key = (record.get("updated_at_ms", 0), path, position)
                old = last.get(record["id"])
                if old is None or key >= old[0]:
                    last[record["id"]] = (key, record)
    records = [r for _, r in last.values()]
    cases = []
    dropped = Counter()
    for record in records:
        technical = record.get("technical") or {}
        request = technical.get("request")
        if not request:
            dropped["no request (single reply or unfinished)"] += 1
            continue
        state = request["state"]
        count = len(state["replies"])
        if count < 2:
            dropped["single reply"] += 1
            continue
        answer = ((technical.get("answer") or {}).get("answers") or {}).get("request_reply")
        if not answer:
            dropped["no Jev answer"] += 1
            continue
        probabilities = answer.get("probabilities") or {}
        choice = answer.get("choice")
        cases.append(
            {
                "case_id": record["id"],
                "session": record["session_id"],
                "started_at_ms": record["started_at_ms"],
                "time": iso(record["started_at_ms"]),
                "state": state,
                "reply_count": count,
                "any_clipped": any(ELLIPSIS in r["text"] for r in state["replies"]),
                "capped_at_32": count >= MAX_REPLIES,
                "jev_choice": choice,
                "jev_type": answer_type(choice, count),
                "jev_top_probability": max(probabilities.values()) if probabilities else None,
                "jev_probabilities": probabilities,
                "jev_status": record.get("status"),
            }
        )
    cases.sort(key=lambda c: (c["started_at_ms"], c["case_id"]))
    return cases, {"log_lines": lines, "distinct_ids": len(records), "dropped": dict(dropped)}


# -------------------------------------------------------------------- fixtures


def load_fixtures():
    fixtures = []
    for path in sorted(FIXTURES.glob("*.json")):
        fixture = json.loads(path.read_text())
        wanted = (fixture.get("outcome") or {}).get("request_message_ids")
        if not wanted:
            continue
        messages = ((fixture.get("evidence") or {}).get("authorization") or {}).get("messages") or []
        state, ids, clipped = build_state(messages)
        if len(ids) < 2:
            continue
        source = fixture.get("source") or {}
        fixtures.append(
            {
                "fixture_id": fixture["id"],
                "session": source.get("session"),
                "time": source.get("captured_at"),
                "state": state,
                "message_ids": ids,
                "acceptable_ids": wanted,
                "acceptable_indices": [i for i, m in enumerate(ids) if m in wanted],
                "reply_count": len(ids),
                "any_clipped": clipped,
                "capped_at_32": len(ids) >= MAX_REPLIES,
                "answer_type": "newest" if ids[-1] in wanted else "older",
            }
        )
    return fixtures


# ----------------------------------------------------------------------- split


def size_bucket(n):
    return "1-2" if n <= 2 else "3-9" if n <= 9 else "10+"


def dominant_type(cases):
    return Counter(c["jev_type"] for c in cases).most_common(1)[0][0]


def assign(strata, seed):
    rng = random.Random(seed)
    side = {}
    for key in sorted(strata):
        sessions = sorted(strata[key])
        rng.shuffle(sessions)
        first = rng.choice(["tuning", "held_out"])
        other = "held_out" if first == "tuning" else "tuning"
        for i, session in enumerate(sessions):
            side[session] = first if i % 2 == 0 else other
    return side


def split_cases(cases, fixtures):
    by_session = defaultdict(list)
    for case in cases:
        by_session[case["session"]].append(case)
    fixture_sessions = {f["session"] for f in fixtures if f["session"]}
    overlap = sorted(set(by_session) & fixture_sessions)
    rest = {s: cs for s, cs in by_session.items() if s not in overlap}
    strata = defaultdict(list)
    for session, cs in rest.items():
        strata[(size_bucket(len(cs)), dominant_type(cs))].append(session)

    def imbalance(side):
        counts = Counter()
        for session, cs in rest.items():
            for c in cs:
                counts[(side[session], c["jev_type"])] += 1
        types = ["newest", "older", "none"]
        return sum(abs(counts[("tuning", t)] - counts[("held_out", t)]) for t in types)

    best = min(range(SPLIT_SEED_SEARCH), key=lambda k: (imbalance(assign(strata, SPLIT_SEED + k)), k))
    seed = SPLIT_SEED + best
    side = assign(strata, seed)
    for session in overlap:
        side[session] = "tuning"
    return {
        "seed": seed,
        "base_seed": SPLIT_SEED,
        "seed_search": f"seeds base..base+{SPLIT_SEED_SEARCH - 1} scored by case-level |tuning-held_out| over newest/older/none "
        "(Jev's logged answer types only); lowest score, then lowest seed",
        "overlap_sessions": overlap,
        "session_side": {s: side[s] for s in sorted(side)},
        "fixtures": "all tuning",
        "strata_rule": "size bucket (cases in session: 1-2, 3-9, 10+) x dominant Jev answer type",
    }, strata, overlap


def split_report(split, cases, fixtures, overlap):
    side = split["session_side"]
    by_session = defaultdict(list)
    for c in cases:
        by_session[c["session"]].append(c)
    rows = defaultdict(lambda: Counter())
    for session, cs in by_session.items():
        key = "overlap with fixtures" if session in overlap else f"size {size_bucket(len(cs))} / {dominant_type(cs)}"
        rows[key][(side[session], "sessions")] += 1
        for c in cs:
            rows[key][(side[session], "cases")] += 1
            rows[key][(side[session], c["jev_type"])] += 1
    out = [
        "# Locator split",
        "",
        f"Seed {split['seed']} (base {split['base_seed']}). Fixtures: {len(fixtures)} usable, all tuning "
        f"({Counter(f['answer_type'] for f in fixtures)['older']} with an older-reply answer).",
        f"Overlap sessions (in usable fixtures and logs): {len(overlap)}, "
        f"{sum(len(by_session[s]) for s in overlap)} logged cases, all tuning.",
        "",
        "| stratum | side | sessions | cases | newest | older | none |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: |",
    ]
    total = Counter()
    for key in sorted(rows):
        for s in ("tuning", "held_out"):
            r = rows[key]
            if not r[(s, "sessions")]:
                continue
            out.append(
                f"| {key} | {s} | {r[(s, 'sessions')]} | {r[(s, 'cases')]} | {r[(s, 'newest')]} | {r[(s, 'older')]} | {r[(s, 'none')]} |"
            )
            for field in ("sessions", "cases", "newest", "older", "none"):
                total[(s, field)] += r[(s, field)]
    for s in ("tuning", "held_out"):
        out.append(
            f"| **total** | {s} | {total[(s, 'sessions')]} | {total[(s, 'cases')]} | {total[(s, 'newest')]} | {total[(s, 'older')]} | {total[(s, 'none')]} |"
        )
    return "\n".join(out) + "\n"


# ---------------------------------------------------------------------- queues


def render_item(item_id, state):
    lines = [f"<<<ITEM {item_id}>>>", "LATEST USER MESSAGE:", state["latest_user_message"], "", "REPLIES (oldest first):"]
    for reply in state["replies"]:
        lines += [f"--- Reply {reply['reply']} ---", reply["text"]]
    lines += [f"<<<END ITEM {item_id}>>>", ""]
    return "\n".join(lines)


def deal(entries, batches, rng):
    """Spread each session over the batches evenly, then shuffle inside each batch."""
    entries = sorted(entries, key=lambda e: (e["group"], rng.random()))
    out = [[] for _ in range(batches)]
    offset = rng.randrange(batches)
    for i, entry in enumerate(entries):
        out[(i + offset) % batches].append(entry)
    for batch in out:
        rng.shuffle(batch)
    return out


def build_queues(cases, fixtures, outdir):
    older = [f for f in fixtures if f["answer_type"] == "older"]
    newest = [f for f in fixtures if f["answer_type"] != "older"]
    hidden_total = round(len(cases) * HIDDEN_SHARE / (1 - HIDDEN_SHARE))
    keys_dir = outdir / "private"
    keys_dir.mkdir(parents=True, exist_ok=True)
    summary = []
    for number, seed in QUEUE_SEEDS.items():
        rng = random.Random(seed)
        extra = rng.sample(newest, max(0, hidden_total - len(older)))
        entries = [
            {"kind": "case", "ref": c["case_id"], "group": c["session"], "state": c["state"]} for c in cases
        ] + [
            {"kind": "fixture", "ref": f["fixture_id"], "group": f["session"] or f["fixture_id"], "state": f["state"]}
            for f in older + extra
        ]
        used = set()
        for entry in entries:
            while True:
                item_id = f"p{number}-{rng.randrange(16**6):06x}"
                if item_id not in used:
                    used.add(item_id)
                    entry["item"] = item_id
                    break
        batches = deal(entries, BATCHES, rng)
        pass_dir = outdir / "labeling" / f"pass{number}"
        pass_dir.mkdir(parents=True, exist_ok=True)
        key = {}
        for b, batch in enumerate(batches, 1):
            text = "".join(render_item(e["item"], e["state"]) for e in batch)
            (pass_dir / f"batch-{b}.md").write_text(text)
            for position, e in enumerate(batch):
                key[e["item"]] = {"pass": number, "batch": b, "position": position, "kind": e["kind"], "ref": e["ref"]}
        (keys_dir / f"key-pass{number}.json").write_text(json.dumps(key, indent=1) + "\n")
        summary.append(
            {
                "pass": number,
                "items": len(entries),
                "fixtures_hidden": len(older) + len(extra),
                "older_fixtures": len(older),
                "batch_sizes": [len(b) for b in batches],
            }
        )
    return summary


def instructions(wording):
    return f"""# Labeling instructions: which reply holds the open question?

You label cases blind. Do not read any other file in the directory tree except your batch
file; never open `private/`, other labelers' outputs or anything that names a classifier's
answer. Judge only from the text in the batch.

## What an item shows

An item has the person's latest message and numbered assistant replies (`Reply 0`, `Reply 1`,
...), oldest first. They are the replies the agent wrote after that message; the person has
not answered since. Long messages are clipped; a clip shows as ` [...] ` in the middle of a
text. Read every reply, not only the last one.

## The task

Decide which replies contain a request the person has not yet answered. This is the rule
the production system uses:

> {wording['instructions']}

"None" means: {wording['none']}

## Answers

For each item, write one JSON line to your output file, as soon as you finish the item:

    {{"item": "p1-0a1b2c", "answer": [2, 3], "reason": "one line"}}

`answer` is one of:

* a list of reply numbers: every reply that itself states the open request (when the agent
  restates the same request in several replies, list all of them; list one when only one
  does);
* `"none"`: no reply holds an unresolved request to the person;
* `"cannot_tell"`: the answer depends on text hidden by a ` [...] ` clip, and the visible
  text does not settle it. If a visible reply clearly holds the request, answer with it
  instead.

`reason` is one short line saying what the request is, or why there is none. Do not label
from the final reply alone: an earlier reply can hold the question and a later one can just
report status. Do not guess a reply number when unsure; use `cannot_tell`.

Write the file incrementally (append one line per item) so that work survives an
interruption. Skip items already in your output file when you resume. Label every item in
your batch, in any order.
"""


# ------------------------------------------------------------------------ main


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--logs", type=Path, default=LOGS)
    args = parser.parse_args()
    out = args.out
    out.mkdir(parents=True, exist_ok=True)

    cases, log_stats = load_logged(args.logs)
    fixtures = load_fixtures()
    for name, rows in (("cases.jsonl", cases), ("fixtures.jsonl", fixtures)):
        with (out / name).open("w") as handle:
            for row in rows:
                handle.write(json.dumps(row, ensure_ascii=False) + "\n")

    # The split goes to disk before the queues are built or anything evaluates the data.
    split, _, overlap = split_cases(cases, fixtures)
    (out / "split.json").write_text(json.dumps(split, indent=1) + "\n")
    report = split_report(split, cases, fixtures, overlap)
    (out / "split-report.md").write_text(report)

    summary = build_queues(cases, fixtures, out)
    (out / "labeling" / "LABELING-INSTRUCTIONS.md").write_text(instructions(json.loads(WORDING.read_text())))

    print(json.dumps({"logs": log_stats, "cases": len(cases), "fixtures": len(fixtures), "queues": summary}, indent=1))
    print(report)


if __name__ == "__main__":
    sys.exit(main())
