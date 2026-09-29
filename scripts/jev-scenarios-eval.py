#!/usr/bin/env python3
"""Replay the recorded Jev scenarios against the live model and report.

Reads every fixture in `mj-core/tests/jev-scenarios/`, posts its `evidence` with
the current bundled questions (`mj-core/src/activity/verdict_questions.json`) to
TypeSafe directly, three times each, and writes `results.jsonl` plus `report.md`
under --output. The report compares the model's answers and the resulting
Mjolnir action with each fixture's `expected`, per category. It never runs in
CI. The key comes from TYPESAFE_API_KEY or ~/.secrets/typesafe_api_key and is
never printed. Standard library only.

    python3 scripts/jev-scenarios-eval.py --output /mnt/optane/mj-jev-scenarios/results-<stamp>
    python3 scripts/jev-scenarios-eval.py --only S01,P04 --repeats 1 --output /tmp/x
    python3 scripts/jev-scenarios-eval.py --report /mnt/optane/mj-jev-scenarios/results-<stamp>   # rebuild report offline
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
QUESTIONS = ROOT / "mj-core/src/activity/verdict_questions.json"
ENDPOINT = "https://api.typesafe.ai/v1/systemone"

ACT_CONFIDENCE = 0.85
AUTOMATION_CONFIDENCE = 0.90
USER_BYTES = 32 * 1024
ASSISTANT_BYTES = 16 * 1024
MAX_BODY_BYTES = 64 * 1024


def action(verdict, authorization_complete):
    """Python port of `mj_core::assessment::Verdict::action`. Pinned by the test file."""
    failure, input_, work = verdict["failure"], verdict["input"], verdict["work"]
    if input_["choice"] == "required" and input_["confidence"] >= ACT_CONFIDENCE:
        return "await_input"
    if failure["confidence"] >= AUTOMATION_CONFIDENCE:
        if failure["choice"] == "transient_provider":
            return "retry_provider"
        if failure["choice"] == "quota":
            return "recover_quota"
        if failure["choice"] == "other":
            return "await_input"
    if failure["choice"] != "none" or failure["confidence"] < AUTOMATION_CONFIDENCE:
        return "uncertain"
    if (
        authorization_complete
        and work["choice"] == "authorized_unfinished"
        and work["confidence"] >= AUTOMATION_CONFIDENCE
        and input_["choice"] in ("none", "redundant_request")
        and input_["confidence"] >= AUTOMATION_CONFIDENCE
    ):
        return "continue"
    if work["confidence"] >= ACT_CONFIDENCE and input_["choice"] == "none" and input_["confidence"] >= ACT_CONFIDENCE:
        if work["choice"] == "finished":
            return "finished"
        if work["choice"] == "waiting":
            return "wait"
    return "uncertain"


def authorization_complete(evidence):
    """Port of the admission check in `apply_turn_assessment` plus `ContinuationEvidence::validate`."""
    context = evidence.get("authorization")
    if not context or not context.get("authorization_complete") or context.get("final_reply_omitted"):
        return False
    messages = context.get("messages") or []
    if not messages or len(messages) > 256:
        return False
    user = sum(len(m["text"].encode()) for m in messages if m["role"] == "user")
    assistant = sum(len(m["text"].encode()) for m in messages if m["role"] == "assistant")
    if any(not m["text"].strip() or not m["id"] or len(m["id"]) > 256 for m in messages):
        return False
    if not (0 < user <= USER_BYTES and 0 < assistant <= ASSISTANT_BYTES):
        return False
    if messages[-1]["role"] != "assistant":
        return False
    body = {"messages": messages, "assistant_history_omitted": context.get("assistant_history_omitted", False)}
    return len(json.dumps(body).encode()) <= MAX_BODY_BYTES


def parse_answers(answers):
    verdict = {}
    for axis in ("failure", "input", "work", "background"):
        answer = answers.get(axis)
        if answer is None and axis == "background":
            continue
        if answer.get("type") != "choice":
            raise ValueError(f"{axis}: not a choice answer")
        confidence = float(answer["confidence"])
        if not 0.0 <= confidence <= 1.0:
            raise ValueError(f"{axis}: confidence out of range")
        verdict[axis] = {"choice": answer["choice"], "confidence": confidence}
    return verdict


def load_fixtures(only):
    fixtures = []
    for path in sorted(FIXTURES.glob("*.json")):
        fixture = json.loads(path.read_text())
        if only and fixture["id"] not in only:
            continue
        fixtures.append(fixture)
    return fixtures


def api_key():
    key = os.environ.get("TYPESAFE_API_KEY", "").strip()
    if not key:
        path = Path.home() / ".secrets/typesafe_api_key"
        if path.exists():
            key = path.read_text().strip()
    if not key:
        sys.exit("no TypeSafe key: set TYPESAFE_API_KEY or create ~/.secrets/typesafe_api_key")
    return key


def ask(key, questions, evidence):
    body = json.dumps({"model": "jev-latest", "state": evidence, "questions": questions}).encode()
    if len(body) > MAX_BODY_BYTES:
        return {"error": f"request body {len(body)} bytes exceeds {MAX_BODY_BYTES}"}
    request = urllib.request.Request(
        ENDPOINT, data=body, headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"}
    )
    started = time.monotonic()
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            payload = json.loads(response.read(MAX_BODY_BYTES + 1))
    except urllib.error.HTTPError as error:
        return {"error": f"HTTP {error.code}", "latency_s": round(time.monotonic() - started, 3)}
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as error:
        return {"error": str(error), "latency_s": round(time.monotonic() - started, 3)}
    try:
        verdict = parse_answers(payload["answers"])
    except (KeyError, ValueError, TypeError) as error:
        return {"error": f"malformed answer: {error}", "latency_s": round(time.monotonic() - started, 3)}
    return {
        "verdict": verdict,
        "model": payload.get("model"),
        "usage": payload.get("usage"),
        "request_bytes": len(body),
        "latency_s": round(time.monotonic() - started, 3),
    }


def run(args):
    key = api_key()
    questions = json.loads(QUESTIONS.read_text())
    fixtures = load_fixtures(args.only)
    args.output.mkdir(parents=True, exist_ok=True)
    results_path = args.output / "results.jsonl"
    done = set()
    if results_path.exists():
        for line in results_path.read_text().splitlines():
            record = json.loads(line)
            done.add((record["id"], record["repeat"]))
    jobs = [(f, r) for f in fixtures for r in range(args.repeats) if (f["id"], r) not in done]
    print(f"{len(fixtures)} fixtures, {len(jobs)} requests to send ({len(done)} already recorded)", flush=True)
    with results_path.open("a") as out, concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        futures = {pool.submit(ask, key, questions, f["evidence"]): (f, r) for f, r in jobs}
        for future in concurrent.futures.as_completed(futures):
            fixture, repeat = futures[future]
            answer = future.result()
            record = {"id": fixture["id"], "repeat": repeat, **answer}
            if "verdict" in answer:
                record["action"] = action(answer["verdict"], authorization_complete(fixture["evidence"]))
            out.write(json.dumps(record) + "\n")
            out.flush()
            summary = record.get("action") or record.get("error")
            print(f"{fixture['id']} #{repeat}: {summary}", flush=True)
    (args.output / "questions.json").write_text(json.dumps(questions, indent=2) + "\n")
    report(args)


def report(args):
    fixtures = {f["id"]: f for f in load_fixtures(None)}
    by_id = defaultdict(list)
    for line in (args.output / "results.jsonl").read_text().splitlines():
        record = json.loads(line)
        by_id[record["id"]].append(record)
    rows = []
    categories = defaultdict(lambda: {"n": 0, "axes_agree": 0, "above": 0, "action_ok": 0, "wrong_high": 0, "errors": 0})
    for identity in sorted(by_id):
        fixture = fixtures.get(identity)
        if not fixture:
            continue
        expected = fixture["expected"]
        stats = categories[fixture["category"]]
        for record in by_id[identity]:
            stats["n"] += 1
            if "verdict" not in record:
                stats["errors"] += 1
                rows.append((identity, fixture["category"], record["repeat"], "error", record.get("error"), "", "", ""))
                continue
            verdict = record["verdict"]
            agree = all(verdict[axis]["choice"] == expected[axis] for axis in ("failure", "input", "work"))
            above = all(verdict[axis]["confidence"] >= ACT_CONFIDENCE for axis in ("failure", "input", "work"))
            act = record["action"]
            stats["axes_agree"] += agree
            stats["above"] += agree and above
            stats["action_ok"] += act == expected["action"]
            wrong = act in expected.get("wrong_actions", [])
            stats["wrong_high"] += wrong
            cells = " ".join(f"{axis[0]}={verdict[axis]['choice']}:{verdict[axis]['confidence']:.2f}" for axis in ("failure", "input", "work"))
            if fixture["evidence"].get("background") and "background" in verdict:
                cells += f" b={verdict['background']['choice']}:{verdict['background']['confidence']:.2f}"
                if expected.get("background") and verdict["background"]["choice"] != expected["background"]:
                    agree = False
            rows.append((identity, fixture["category"], record["repeat"], cells, act, expected["action"], "agree" if agree else "differ", "WRONG" if wrong else ""))
    lines = ["# Jev scenario replay", "", f"Results: `{args.output / 'results.jsonl'}`", ""]
    lines += ["| category | requests | axes agree | agree and all >= 0.85 | action as expected | wrong action | errors |", "| --- | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for category, s in sorted(categories.items()):
        lines.append(f"| {category} | {s['n']} | {s['axes_agree']} | {s['above']} | {s['action_ok']} | {s['wrong_high']} | {s['errors']} |")
    total = {k: sum(s[k] for s in categories.values()) for k in ("n", "axes_agree", "above", "action_ok", "wrong_high", "errors")}
    lines.append(f"| all | {total['n']} | {total['axes_agree']} | {total['above']} | {total['action_ok']} | {total['wrong_high']} | {total['errors']} |")
    lines += ["", "| id | category | repeat | answers | action | expected | axes | wrong |", "| --- | --- | ---: | --- | --- | --- | --- | --- |"]
    for row in rows:
        lines.append("| " + " | ".join(str(cell) for cell in row) + " |")
    (args.output / "report.md").write_text("\n".join(lines) + "\n")
    print(f"wrote {args.output / 'report.md'}")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--output", type=Path, help="directory for results.jsonl and report.md")
    parser.add_argument("--report", type=Path, help="rebuild report.md from an existing results directory, no requests")
    parser.add_argument("--only", help="comma-separated fixture ids")
    parser.add_argument("--repeats", type=int, default=3)
    args = parser.parse_args()
    args.only = set(args.only.split(",")) if args.only else None
    if args.report:
        args.output = args.report
        report(args)
    elif args.output:
        run(args)
    else:
        parser.error("give --output to run or --report to rebuild a report")


if __name__ == "__main__":
    main()
