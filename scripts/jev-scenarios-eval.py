#!/usr/bin/env python3
"""Replay the recorded Jev scenarios against the live model and report.

Reads every fixture in `mj-core/tests/jev-scenarios/`, posts its `evidence` with
the current bundled questions (`mj-core/src/activity/verdict_questions.json`) to
TypeSafe directly, three times each, and writes `results.jsonl` plus `report.md`
under --output. The report compares the model's answers and the resulting
Mjolnir action with each fixture's `expected`, per category. It never runs in
CI. Input detection and false required-input alerts are also scored separately
using the recorded policy (currently probability >= 0.50 and >= 2.5x runner-up); cases marked context.strict_input_scoring=false are
excluded from that count. The key comes from TYPESAFE_API_KEY or ~/.secrets/typesafe_api_key and is
never printed. Standard library only.

Use --questions to compare an alternative prompt. Questions and labeled cases
are frozen before requests; resuming with changed questions or fixtures fails.
An offline --report uses those snapshots, so later edits cannot relabel a run.

    python3 scripts/jev-scenarios-eval.py --output /mnt/optane/mj-jev-scenarios/results-<stamp>
    python3 scripts/jev-scenarios-eval.py --only S01,P04 --repeats 1 --output /tmp/x
    python3 scripts/jev-scenarios-eval.py --questions /path/candidate.json --only D01,D05 --output /mnt/optane/jev-candidate
    python3 scripts/jev-scenarios-eval.py --report /mnt/optane/mj-jev-scenarios/results-<stamp>   # rebuild report offline
"""
import argparse
import concurrent.futures
import hashlib
import json
import math
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

POLICY = "input-ratio2.5-floor0.5-finished0.8"
CHOICES = {
    "failure": ("none", "transient_provider", "quota", "other", "unclear"),
    "input": ("none", "redundant_request", "required", "unclear"),
    "work": ("finished", "authorized_unfinished", "waiting", "unclear"),
    "reply": ("closing", "continuing", "unclear"),
    "background": ("needed", "unneeded", "unclear"),
}
ACT_CONFIDENCE = 0.85
AUTOMATION_CONFIDENCE = 0.90
REPLY_CLOSING_THRESHOLD = 0.80
USER_BYTES = 32 * 1024
ASSISTANT_BYTES = 16 * 1024
MAX_BODY_BYTES = 64 * 1024


def requires_input(input_):
    probabilities = input_.get("probabilities", {})
    winner = probabilities.get("required", 0.0)
    runner_up = max((p for name, p in probabilities.items() if name != "required"), default=0.0)
    return input_["choice"] == "required" and winner + 1e-12 >= .50 and winner + 1e-12 >= 2.5 * runner_up


def action(verdict, authorization_complete):
    """Python port of `mj_core::assessment::Verdict::action`. Pinned by the test file."""
    failure, input_, work = verdict["failure"], verdict["input"], verdict["work"]
    if failure["choice"] == "quota" and failure["confidence"] >= AUTOMATION_CONFIDENCE:
        return "recover_quota"
    if requires_input(input_):
        return "await_input"
    if failure["confidence"] >= AUTOMATION_CONFIDENCE:
        if failure["choice"] == "transient_provider":
            return "retry_provider"
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
    if input_["choice"] == "none" and input_["confidence"] >= ACT_CONFIDENCE:
        if work["choice"] == "finished" and work.get("probabilities", {}).get("finished", 0.0) + 1e-12 >= .80:
            return "finished"
        if work["choice"] == "waiting" and work["confidence"] >= ACT_CONFIDENCE:
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


def parse_answers(answers, include_reply=False):
    verdict = {}
    axes = ("failure", "input", "work") + (("reply",) if include_reply else ()) + ("background",)
    for axis in axes:
        answer = answers.get(axis)
        if answer is None and axis == "background":
            continue
        if not isinstance(answer, dict) or answer.get("type") != "choice":
            raise ValueError(f"{axis}: not a choice answer")
        confidence = answer["confidence"]
        if type(confidence) not in (int, float) or not math.isfinite(confidence) or not 0.0 <= confidence <= 1.0:
            raise ValueError(f"{axis}: confidence out of range")
        probabilities = answer.get("probabilities")
        if not isinstance(probabilities, dict) or set(probabilities) != set(CHOICES[axis]):
            raise ValueError(f"{axis}: incomplete probabilities")
        if any(type(p) not in (int, float) or not math.isfinite(p) or not 0 <= p <= 1 for p in probabilities.values()):
            raise ValueError(f"{axis}: invalid probabilities")
        if abs(sum(probabilities.values()) - 1) > .005 * len(probabilities) + 1e-12:
            raise ValueError(f"{axis}: invalid distribution total")
        choice = answer["choice"]
        if choice not in probabilities or probabilities[choice] + 1e-12 < max(probabilities.values()):
            raise ValueError(f"{axis}: choice is not a probability winner")
        verdict[axis] = {"choice": choice, "confidence": confidence, "probabilities": probabilities}
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


WIRE_BUDGET = 60 * 1024


def fit_to_wire(evidence):
    """Port of the worker's shrink step: drop the oldest assistant entries, never the
    final reply, until the serialized evidence fits; drop the history only if that fails."""
    evidence = json.loads(json.dumps(evidence))
    size = lambda: len(json.dumps(evidence).encode())
    context = evidence.get("authorization")
    while context and size() > WIRE_BUDGET:
        messages = context["messages"]
        protected = messages[-1]["id"] if messages and messages[-1]["role"] == "assistant" else None
        index = next((i for i, m in enumerate(messages) if m["role"] == "assistant" and m["id"] != protected), None)
        if index is None:
            break
        messages.pop(index)
        context["assistant_history_omitted"] = True
    if size() > WIRE_BUDGET:
        evidence["authorization"] = None
        evidence["transcript_summary"] = ""
    return evidence


def ask(key, questions, evidence):
    evidence = fit_to_wire(evidence)
    # The proxy bounds the evidence alone at 64 KiB and adds the questions itself.
    state_bytes = len(json.dumps(evidence).encode())
    if state_bytes > MAX_BODY_BYTES:
        return {"error": f"evidence {state_bytes} bytes exceeds {MAX_BODY_BYTES}"}
    body = json.dumps({"model": "jev-latest", "state": evidence, "questions": questions}).encode()
    request = urllib.request.Request(
        ENDPOINT, data=body, headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"}
    )
    started = time.monotonic()
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            payload = json.loads(response.read(MAX_BODY_BYTES + 1))
    except urllib.error.HTTPError as error:
        return {"error": f"HTTP {error.code}", "http_status": error.code, "latency_s": round(time.monotonic() - started, 3)}
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as error:
        return {"error": str(error), "latency_s": round(time.monotonic() - started, 3)}
    try:
        verdict = parse_answers(payload["answers"], include_reply="reply" in questions)
    except (KeyError, ValueError, TypeError) as error:
        return {"error": f"malformed answer: {error}", "latency_s": round(time.monotonic() - started, 3)}
    return {
        "verdict": verdict,
        # Keep the full distribution for offline margin/odds comparisons.
        # Provider confidence is a separate statistic, not the winning probability.
        "answers": payload["answers"],
        "model": payload.get("model"),
        "usage": payload.get("usage"),
        "request_bytes": len(body),
        "latency_s": round(time.monotonic() - started, 3),
    }


def run(args):
    questions = json.loads(args.questions.read_text())
    validate_questions(questions)
    if (args.output / "stopped.json").exists():
        raise ValueError("run stopped on an account-wide failure; preserve it and use a new output directory")
    key = api_key()
    fixtures = load_fixtures(args.only)
    if not fixtures:
        raise ValueError("no fixtures selected")
    prepare_run(args.output, questions, fixtures)
    results_path = args.output / "results.jsonl"
    done = set()
    if results_path.exists():
        for line in results_path.read_text().splitlines():
            record = json.loads(line)
            done.add((record["id"], record["repeat"]))
    jobs = [(f, r) for f in fixtures for r in range(args.repeats) if (f["id"], r) not in done]
    print(f"{len(fixtures)} fixtures, {len(jobs)} requests to send ({len(done)} already recorded)", flush=True)
    remaining = iter(jobs)
    stopped = None
    with results_path.open("a") as out, concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        futures = {}
        while True:
            # Admit at most two requests, so an account-wide failure cannot
            # leave the rest of the corpus queued for needless submission.
            while not stopped and len(futures) < 2:
                job = next(remaining, None)
                if job is None:
                    break
                fixture, repeat = job
                futures[pool.submit(ask, key, questions, fixture["evidence"])] = job
            if not futures:
                break
            completed, _ = concurrent.futures.wait(futures, return_when=concurrent.futures.FIRST_COMPLETED)
            for future in completed:
                fixture, repeat = futures.pop(future)
                answer = future.result()
                record = {"id": fixture["id"], "repeat": repeat, **answer}
                if "verdict" in answer:
                    record["action"] = action(answer["verdict"], authorization_complete(fit_to_wire(fixture["evidence"])))
                out.write(json.dumps(record) + "\n")
                out.flush()
                summary = record.get("action") or record.get("error")
                print(f"{fixture['id']} #{repeat}: {summary}", flush=True)
                if answer.get("http_status") in (401, 402):
                    stopped = answer["error"]
    if stopped:
        (args.output / "stopped.json").write_text(json.dumps({"reason": stopped, "requested_jobs": len(jobs), "note": "Account-wide failure; remaining requests were not sent. Preserve this run and retry in a new directory after restoring access."}, indent=2) + "\n")
    report(args)
    if stopped:
        raise SystemExit(f"stopped after {stopped}; remaining requests were not sent; see stopped.json")


def validate_questions(questions):
    """Reject malformed choice prompts before reading credentials or sending evidence."""
    if not isinstance(questions, dict) or not {"failure", "input", "work"} <= questions.keys():
        raise ValueError("questions must include failure, input, and work")
    for axis, question in questions.items():
        if not isinstance(question, dict) or question.get("type") != "choice":
            raise ValueError(f"{axis}: question type must be choice")
        if not isinstance(question.get("instructions"), str) or not question["instructions"].strip():
            raise ValueError(f"{axis}: missing question instructions")
        criteria = question.get("criteria")
        if not isinstance(criteria, dict) or not criteria or any(
            not isinstance(item, dict) or not isinstance(item.get("what"), str) or not item["what"].strip()
            for item in criteria.values()
        ):
            raise ValueError(f"{axis}: missing choice criteria")


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def prepare_run(output, questions, fixtures):
    """Freeze prompts and labeled cases before sending requests; never mix experiments."""
    manifest = {
        "policy": POLICY,
        "model": "jev-latest",
        "endpoint": ENDPOINT,
        "questions_sha256": digest(questions),
        "fixtures_sha256": digest(fixtures),
        "evidence_sha256": {f["id"]: digest(fit_to_wire(f["evidence"])) for f in fixtures},
    }
    output.mkdir(parents=True, exist_ok=True)
    manifest_path = output / "run.json"
    if manifest_path.exists():
        if json.loads(manifest_path.read_text()) != manifest:
            raise ValueError("policy, prompt or fixtures changed; use a new output directory")
        for name, expected in (("questions.json", questions), ("fixtures.json", fixtures)):
            if json.loads((output / name).read_text()) != expected:
                raise ValueError(f"{name} differs from the frozen experiment; use a new output directory")
    elif (output / "results.jsonl").exists():
        raise ValueError("existing results have no experiment manifest; use a new output directory")
    else:
        (output / "questions.json").write_text(json.dumps(questions, indent=2) + "\n")
        (output / "fixtures.json").write_text(json.dumps(fixtures, indent=2) + "\n")
        manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")


def report(args):
    manifest_path = args.output / "run.json"
    policy = json.loads(manifest_path.read_text()).get("policy", "confidence-0.85") if manifest_path.exists() else "confidence-0.85"
    if policy not in (POLICY, "confidence-0.85"):
        raise ValueError(f"unknown report policy: {policy}")
    snapshot = args.output / "fixtures.json"
    fixtures = {f["id"]: f for f in (json.loads(snapshot.read_text()) if snapshot.exists() else load_fixtures(None))}
    by_id = defaultdict(list)
    for line in (args.output / "results.jsonl").read_text().splitlines():
        record = json.loads(line)
        by_id[record["id"]].append(record)
    rows = []
    categories = defaultdict(lambda: {"n": 0, "axes_agree": 0, "above": 0, "action_ok": 0, "wrong_high": 0, "errors": 0})
    inputs = defaultdict(lambda: {"n": 0, "agree": 0, "agree_high": 0, "detected": 0, "missed": 0, "false_required": 0, "errors": 0})
    reply_stats = defaultdict(lambda: {
        "requests": 0,
        "valid": 0,
        "choice_agree": 0,
        "closing_detected": 0,
        "closing_probability_sum": 0.0,
    })
    reply_phase_stats = defaultdict(lambda: {
        "requests": 0,
        "valid": 0,
        "closing_detected": 0,
        "closing_probability_sum": 0.0,
    })
    running_closing_requests = 0
    running_closing_valid = 0
    running_closing_detected = 0
    false_closing_requests = 0
    false_closing_valid = 0
    false_closing = 0
    input_unscored = 0
    for identity in sorted(by_id):
        fixture = fixtures.get(identity)
        if not fixture:
            continue
        expected = fixture["expected"]
        stats = categories[fixture["category"]]
        for record in by_id[identity]:
            expected_reply = expected.get("reply")
            if expected_reply is not None:
                reply_stats[expected_reply]["requests"] += 1
                phase = fixture["evidence"].get("phase", "unknown")
                reply_phase_stats[(phase, expected_reply)]["requests"] += 1
                if fixture["evidence"].get("phase") == "running" and expected_reply == "closing":
                    running_closing_requests += 1
                if expected_reply == "continuing":
                    false_closing_requests += 1
            stats["n"] += 1
            input_stats = inputs[expected["input"]] if fixture.get("context", {}).get("strict_input_scoring", True) else None
            if input_stats is None:
                input_unscored += 1
            else:
                input_stats["n"] += 1
            if "verdict" not in record:
                stats["errors"] += 1
                if input_stats is not None:
                    input_stats["errors"] += 1
                rows.append((identity, fixture["category"], record["repeat"], "error", record.get("error"), "", "", ""))
                continue
            verdict = record["verdict"]
            if expected_reply is not None and "reply" in verdict:
                reply = verdict["reply"]
                p_closing = reply["probabilities"].get("closing", 0.0)
                reply_stats[expected_reply]["valid"] += 1
                reply_stats[expected_reply]["choice_agree"] += reply["choice"] == expected_reply
                reply_stats[expected_reply]["closing_probability_sum"] += p_closing
                is_closing = p_closing + 1e-12 >= REPLY_CLOSING_THRESHOLD
                reply_stats[expected_reply]["closing_detected"] += is_closing
                phase = fixture["evidence"].get("phase", "unknown")
                reply_phase_stats[(phase, expected_reply)]["valid"] += 1
                reply_phase_stats[(phase, expected_reply)]["closing_probability_sum"] += p_closing
                reply_phase_stats[(phase, expected_reply)]["closing_detected"] += is_closing
                if fixture["evidence"].get("phase") == "running" and expected_reply == "closing":
                    running_closing_valid += 1
                    running_closing_detected += is_closing
                if expected_reply == "continuing":
                    false_closing_valid += 1
                    false_closing += is_closing
            predicted_input = verdict["input"]
            input_agrees = predicted_input["choice"] == expected["input"]
            required_high = (requires_input(predicted_input) if policy == POLICY else
                             predicted_input["choice"] == "required" and predicted_input["confidence"] >= ACT_CONFIDENCE)
            if input_stats is not None:
                input_stats["agree"] += input_agrees
                input_stats["agree_high"] += input_agrees and predicted_input["confidence"] >= ACT_CONFIDENCE
                input_stats["detected"] += expected["input"] == "required" and required_high
                input_stats["missed"] += expected["input"] == "required" and not required_high
                input_stats["false_required"] += expected["input"] != "required" and required_high
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
    if (args.output / "stopped.json").exists():
        stopped = json.loads((args.output / "stopped.json").read_text())
        lines += [f"Incomplete run: {stopped['reason']}. Counts below cover recorded requests only; remaining requests were not sent.", ""]
    lines += [
        f"Input is scored independently of failure and work. Detection policy: `{policy}`. Confidence columns remain separate diagnostics. Errors count as unsuccessful requests.",
        f"Ambiguous cases excluded from strict input counts: {input_unscored} requests. Their hypothesized axes remain in the full-verdict table.", "",
        "| expected input | requests | choice agrees | agrees >= 0.85 | required detected | required missed | false required | errors |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for label, s in sorted(inputs.items()):
        lines.append(f"| {label} | {s['n']} | {s['agree']} | {s['agree_high']} | {s['detected']} | {s['missed']} | {s['false_required']} | {s['errors']} |")
    lines += [""]
    lines += ["| category | requests | axes agree | agree and all >= 0.85 | action as expected | wrong action | errors |", "| --- | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for category, s in sorted(categories.items()):
        lines.append(f"| {category} | {s['n']} | {s['axes_agree']} | {s['above']} | {s['action_ok']} | {s['wrong_high']} | {s['errors']} |")
    total = {k: sum(s[k] for s in categories.values()) for k in ("n", "axes_agree", "above", "action_ok", "wrong_high", "errors")}
    lines.append(f"| all | {total['n']} | {total['axes_agree']} | {total['above']} | {total['action_ok']} | {total['wrong_high']} | {total['errors']} |")
    lines += ["", "## Reply axis", ""]
    if reply_stats:
        lines += [
            f"False closing on fixtures labeled `continuing` (`P(closing) >= {REPLY_CLOSING_THRESHOLD:.2f}`): {false_closing}/{false_closing_valid} scored replies ({false_closing_requests} labeled requests attempted).",
            f"Running-phase closing detections on fixtures labeled `closing`: {running_closing_detected}/{running_closing_valid} scored replies ({running_closing_requests} labeled requests).",
            "",
            "| expected.reply | requests | replies scored | choice matches | mean P(closing) | P(closing) >= 0.80 |",
            "| --- | ---: | ---: | ---: | ---: | ---: |",
        ]
        for label in ("closing", "continuing", "unclear"):
            stats = reply_stats.get(label)
            if not stats:
                continue
            mean = stats["closing_probability_sum"] / stats["valid"] if stats["valid"] else None
            mean_text = f"{mean:.3f}" if mean is not None else "n/a"
            lines.append(
                f"| {label} | {stats['requests']} | {stats['valid']} | {stats['choice_agree']} | {mean_text} | {stats['closing_detected']}/{stats['valid']} |"
            )
        lines += [
            "",
            "| phase | expected.reply | requests | replies scored | P(closing) >= 0.80 | mean P(closing) |",
            "| --- | --- | ---: | ---: | ---: | ---: |",
        ]
        for phase, label in sorted(reply_phase_stats):
            stats = reply_phase_stats[(phase, label)]
            mean = stats["closing_probability_sum"] / stats["valid"] if stats["valid"] else None
            mean_text = f"{mean:.3f}" if mean is not None else "n/a"
            lines.append(
                f"| {phase} | {label} | {stats['requests']} | {stats['valid']} | {stats['closing_detected']}/{stats['valid']} | {mean_text} |"
            )
        lines += [
            "",
            "| id | category | phase | expected.reply | P(closing) by repeat | mean | >= 0.80 |",
            "| --- | --- | --- | --- | --- | ---: | ---: |",
        ]
        for identity in sorted(fixtures):
            fixture = fixtures[identity]
            expected_reply = fixture["expected"].get("reply")
            if expected_reply is None:
                continue
            scores = []
            for record in sorted(by_id.get(identity, []), key=lambda record: record["repeat"]):
                verdict = record.get("verdict", {})
                answer = verdict.get("reply")
                if answer is not None:
                    scores.append((record["repeat"], answer["probabilities"].get("closing", 0.0)))
            score_text = ", ".join(f"{repeat}:{score:.2f}" for repeat, score in scores) if scores else "not asked"
            mean = sum(score for _, score in scores) / len(scores) if scores else None
            mean_text = f"{mean:.3f}" if mean is not None else "n/a"
            detected = sum(score + 1e-12 >= REPLY_CLOSING_THRESHOLD for _, score in scores)
            lines.append(
                f"| {identity} | {fixture['category']} | {fixture['evidence'].get('phase', 'unknown')} | {expected_reply} | {score_text} | {mean_text} | {detected}/{len(scores)} |"
            )
    else:
        lines.append("No fixtures declare `expected.reply`.")
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
    parser.add_argument("--questions", type=Path, default=QUESTIONS, help="alternative question bundle; saved with results")
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
