#!/usr/bin/env python3
"""Replay the question locator against the live model and report its accuracy.

The locator asks Jev which assistant reply since the user's last message holds
the request the user still owes (see `.agents/plans/jev-question-locator.md`).
This script reads every fixture in `mj-core/tests/jev-scenarios/` whose
`outcome.request_message_ids` names the acceptable replies, builds the same
request state as `mj_core::activity::question_locator`, posts it with the
bundled question (`mj-core/src/activity/question_locator.json`) to TypeSafe
directly, three times each, and writes `results.jsonl` plus `report.md` under
--output. A request is correct when the chosen reply is one of the acceptable
ones. Fixtures with fewer than two replies after the user's message are skipped:
the worker sends no request for them. It never runs in CI. The key comes from
TYPESAFE_API_KEY or ~/.secrets/typesafe_api_key and is never printed. Standard
library only.

    python3 scripts/jev-question-locator-eval.py --output /mnt/optane/jev-locator/run-<stamp>
    python3 scripts/jev-question-locator-eval.py --only W16,W17 --repeats 1 --output /tmp/x
    python3 scripts/jev-question-locator-eval.py --question /path/candidate.json --output /tmp/y
"""
import argparse
import concurrent.futures
import json
import os
import sys
import time
import urllib.error
import urllib.request
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "mj-core/tests/jev-scenarios"
QUESTION = ROOT / "mj-core/src/activity/question_locator.json"
ENDPOINT = "https://api.typesafe.ai/v1/systemone"

MAX_REPLIES = 32
USER_BYTES = 4096
REPLY_BYTES = 3000
STATE_BYTES = 60 * 1024
ELLIPSIS = " [...] "


def head(text, limit):
    data = text.encode()[:limit]
    return data.decode(errors="ignore")


def tail(text, limit):
    data = text.encode()
    data = data[max(0, len(data) - limit):]
    # Drop continuation bytes left at the front by the cut.
    while data and (data[0] & 0xC0) == 0x80:
        data = data[1:]
    return data.decode()


def clip(text, budget):
    if len(text.encode()) <= budget:
        return text
    return head(text, budget * 2 // 3) + ELLIPSIS + tail(text, budget // 3)


def locator_state(messages):
    """The request state and the ids of the replies it lists, oldest first."""
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
        if len(json.dumps(state, ensure_ascii=False).encode()) <= STATE_BYTES or budget < 64:
            return state, [m["id"] for m in replies]
        budget = int(budget * 0.8)


def questions(question, count):
    criteria = {f"r{j}": {"what": f"Reply {j} (replies[{j}]) contains the open request."} for j in range(count)}
    criteria["none"] = {"what": question["none"]}
    return {"request_reply": {"type": "choice", "instructions": question["instructions"], "criteria": criteria}}


def api_key():
    key = os.environ.get("TYPESAFE_API_KEY", "").strip()
    if not key:
        path = Path.home() / ".secrets/typesafe_api_key"
        if path.exists():
            key = path.read_text().strip()
    if not key:
        sys.exit("no TypeSafe key: set TYPESAFE_API_KEY or create ~/.secrets/typesafe_api_key")
    return key


def load_cases(only):
    cases = []
    for path in sorted(FIXTURES.glob("*.json")):
        fixture = json.loads(path.read_text())
        wanted = (fixture.get("outcome") or {}).get("request_message_ids")
        if not wanted or (only and fixture["id"] not in only):
            continue
        messages = ((fixture["evidence"] or {}).get("authorization") or {}).get("messages") or []
        state, ids = locator_state(messages)
        if len(ids) < 2:
            continue
        cases.append({"id": fixture["id"], "state": state, "ids": ids, "wanted": wanted})
    return cases


def ask(key, question, case):
    body = json.dumps({"model": "jev-latest", "state": case["state"], "questions": questions(question, len(case["ids"]))}).encode()
    request = urllib.request.Request(
        ENDPOINT, data=body, headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"}
    )
    started = time.monotonic()
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            payload = json.loads(response.read())
        answer = payload["answers"]["request_reply"]
        choice = answer["choice"]
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError, KeyError, TypeError) as error:
        return {"error": str(error), "latency_s": round(time.monotonic() - started, 3)}
    chosen = case["ids"][int(choice[1:])] if choice.startswith("r") and choice[1:].isdigit() and int(choice[1:]) < len(case["ids"]) else None
    return {
        "choice": choice,
        "chosen_id": chosen,
        "probability": (answer.get("probabilities") or {}).get(choice),
        "correct": chosen in case["wanted"],
        "model": payload.get("model"),
        "usage": payload.get("usage"),
        "request_bytes": len(body),
        "latency_s": round(time.monotonic() - started, 3),
    }


def report(output, results):
    by_case = defaultdict(list)
    for result in results:
        by_case[result["id"]].append(result)
    done = [r for r in results if "error" not in r]
    lines = [
        "# Question locator replay",
        "",
        f"Results: `{output / 'results.jsonl'}`",
        "",
        f"Correct: {sum(r['correct'] for r in done)}/{len(done)} requests; errors: {len(results) - len(done)}.",
        "",
        "| fixture | replies | correct | choices |",
        "| --- | ---: | ---: | --- |",
    ]
    for case_id in sorted(by_case):
        rows = by_case[case_id]
        choices = ", ".join(r.get("choice", "error") for r in rows)
        lines.append(f"| {case_id} | {rows[0]['replies']} | {sum(r.get('correct', False) for r in rows)}/{len(rows)} | {choices} |")
    (output / "report.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines[4:5]))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--only", help="comma-separated fixture ids")
    parser.add_argument("--question", type=Path, default=QUESTION, help="alternative question file")
    parser.add_argument("--repeats", type=int, default=3)
    args = parser.parse_args()
    question = json.loads(args.question.read_text())
    cases = load_cases(set(args.only.split(",")) if args.only else None)
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "question.json").write_text(json.dumps(question, indent=2) + "\n")
    key = api_key()
    jobs = [(case, repeat) for case in cases for repeat in range(args.repeats)]
    with concurrent.futures.ThreadPoolExecutor(4) as pool:
        answers = list(pool.map(lambda job: ask(key, question, job[0]), jobs))
    results = [
        {"id": case["id"], "repeat": repeat, "replies": len(case["ids"]), "wanted": case["wanted"], **answer}
        for (case, repeat), answer in zip(jobs, answers)
    ]
    with (args.output / "results.jsonl").open("w") as handle:
        for result in results:
            handle.write(json.dumps(result) + "\n")
    report(args.output, results)


if __name__ == "__main__":
    main()
