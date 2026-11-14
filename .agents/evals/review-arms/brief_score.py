"""Judges known-defect catches for the relation-brief arms and scores them (brief/PLAN.md).

Usage:
  python3 brief_score.py build [--arms OT,OS] [--bugs-per-batch 3] [--name brief-judge]
      blind judge batches of stored no-brief and brief sessions -> $REVIEW_ARMS_DIR/<name>/
      (stage B: --arms OT,OS,BT,BS --name brief-judge-b, so oracle and blind are scored apart
       and stage A's batches and key are kept)
      (run brief/workflow-judge.js with the args it prints; save its result as
       brief-judge/workflow-out.json)
  python3 brief_score.py collect
      merges the two judges; writes brief-judge/handcheck.jsonl (every caught or partial verdict
      from either judge and every disagreement) for a hand check
  (write brief-judge/handcheck-decisions.json: {output_id: "caught"|"partial"|"missed"})
  python3 brief_score.py score
      catch rates by set, arm and model with bootstrap intervals; tokens and latency by arm

The blinding key (output id -> session, model, arm) is kept in private/, which
judges are told not to open.
"""
import argparse
import collections
import json
import random
import secrets
import statistics
import sys

import brief_oracle
from driver import ARMS, BRIEF_ARMS, ROOT, SCRIPTS, save

BASELINE_ARMS = ("A", "B", "D", "D2")
MODELS = ("gpt-6.1-terra", "gpt-6.1-sol")
# Set from --name: each judging round (stage A, stage B) has its own batches and key.
J = KEY = None


def latency(session_dir):
    """Seconds from the reviewer's prompt to the end of its turn; the same definition as score.py."""
    row = json.loads((session_dir / "session.json").read_text())
    done = (row.get("last_turn_outcome") or {}).get("completed_at_ms")
    items = [i for p in json.loads((session_dir / "transcript.json").read_text()) for i in p["items"]]
    prompt = next((i["created_at_ms"] for i in items if i["role"] == "user"), None)
    return round((done - prompt) / 1000, 1) if done and prompt else None


def sessions(arms):
    """Every finished high-effort Terra or Sol session with output on the 30 Hard-won diffs."""
    out = []
    for bug in sum(brief_oracle.sets().values(), []):
        for arm in (*BASELINE_ARMS, *arms):
            done = ROOT / "runs" / bug / arm / "done.json"
            if not done.exists():
                continue
            for r in json.loads(done.read_text())["reviewers"]:
                sd = done.parent / "reviewers" / r["child"][:12]
                if r["effort"] != "high" or r["model"] not in MODELS or not (sd / "output.md").exists():
                    continue
                usage = json.loads((sd / "usage.json").read_text())
                tools = sum(1 for p in json.loads((sd / "transcript.json").read_text()) for i in p["items"]
                            if i["role"] in ("tool", "terminal"))
                out.append({"bug": bug, "arm": arm, "condition": BRIEF_ARMS[arm] if arm in arms else "no-brief",
                            "session": r["child"], "model": r["model"], "dir": str(sd),
                            "latency_s": latency(sd), "tool_calls": tools,
                            "total_tokens": usage["totals"]["total_tokens"]["tokens"]})
    return out


def conditions(arms):
    """no-brief, then each brief kind (oracle, blind) the arms carry; kinds are never pooled."""
    return ["no-brief", *sorted({BRIEF_ARMS[a] for a in arms})]


def require_complete(rows, arms):
    """Refuses a partial sample: every frozen diff needs its planned sessions for every model.

    No-brief: the stored arms give each Hard-won diff at least 3 sessions per model.
    Each brief kind: exactly the sessions its arms launch (driver.ARMS), all with output.
    """
    planned = collections.Counter((BRIEF_ARMS[arm], m.split(":")[0]) for arm in arms for m in ARMS[arm])
    have = collections.Counter((r["bug"], r["condition"], r["model"]) for r in rows)
    gaps = []
    for bug in sum(brief_oracle.sets().values(), []):
        for model in MODELS:
            if have[(bug, "no-brief", model)] < 3:
                gaps.append(f"{bug} no-brief {model}: {have[(bug, 'no-brief', model)]} of at least 3")
            for kind in conditions(arms)[1:]:
                if have[(bug, kind, model)] != planned[(kind, model)]:
                    gaps.append(f"{bug} {kind} {model}: {have[(bug, kind, model)]} of {planned[(kind, model)]}")
    if gaps:
        raise SystemExit("incomplete sample; not scoring a subset:\n" + "\n".join(gaps))


def build(arms, per_batch):
    if KEY.exists():
        # New output IDs would orphan any verdicts already returned for this round.
        raise SystemExit(f"{KEY} exists: round {J.name} is already built; use another --name")
    J.mkdir(parents=True, exist_ok=True)
    KEY.parent.mkdir(parents=True, exist_ok=True)
    corpus = {r["id"]: r for r in map(json.loads, open(SCRIPTS / "corpus/corpus.jsonl"))}
    rows = sessions(arms)
    require_complete(rows, arms)
    key = {}
    by_bug = collections.defaultdict(list)
    for r in rows:
        oid = secrets.token_hex(8)
        key[oid] = r
        by_bug[r["bug"]].append({"output_id": oid, "text": open(r["dir"] + "/output.md").read()})
    bugs = sorted(by_bug)
    random.Random(20261116).shuffle(bugs)
    batches = []
    for n in range(0, len(bugs), per_batch):
        batch = []
        for bug in bugs[n:n + per_batch]:
            outputs = by_bug[bug]
            random.Random(bug).shuffle(outputs)
            c = corpus[bug]
            batch.append({"bug": bug, **{k: c[k] for k in ("defect", "location", "trigger", "match_rule",
                                                           "near_misses", "why_findable", "intro_commit",
                                                           "fix_commit")}, "outputs": outputs})
        save(J / f"batch-{n // per_batch}.json", batch)
        batches.append({"n": n // per_batch, "ids": [o["output_id"] for b in batch for o in b["outputs"]]})
    (J / "SPEC.md").write_text((SCRIPTS / "judge/SPEC.md").read_text())
    save(KEY, {"arms": list(arms), "outputs": key})
    print(json.dumps({"dir": str(J), "batches": batches}))
    print(json.dumps({"outputs": len(key), "by_condition_model": collections.Counter(
        f"{r['condition']}/{r['model']}" for r in rows)}), file=sys.stderr)


def collect():
    out = json.loads((J / "workflow-out.json").read_text())
    assert not out["missing"], out["missing"]
    verdicts = collections.defaultdict(list)
    for v in out["verdicts"]:
        verdicts[v["output_id"]].append(v)
    key = json.loads(KEY.read_text())["outputs"]
    assert set(verdicts) == set(key), "every output needs verdicts"
    assert all(len(v) == 2 for v in verdicts.values()), "every output needs two judges"
    rows, check = [], []
    for oid, (a, b) in sorted(verdicts.items()):
        agree = a["label"] == b["label"]
        row = {"output_id": oid, "bug": a["bug"], "labels": [a["label"], b["label"]], "agree": agree}
        rows.append(row)
        if not agree or a["label"] != "missed":
            check.append({**row, "verdicts": [a, b], "text": open(key[oid]["dir"] + "/output.md").read()})
    (J / "merged.jsonl").write_text("".join(json.dumps(r) + "\n" for r in rows))
    (J / "handcheck.jsonl").write_text("".join(json.dumps(r) + "\n" for r in check))
    print(json.dumps({"outputs": len(rows), "agree": sum(r["agree"] for r in rows),
                      "to_hand_check": len(check),
                      "label_pairs": collections.Counter("/".join(sorted(r["labels"])) for r in rows)}, indent=1))


def boot(diff_values, n=10000):
    """95% bootstrap interval of the mean over diffs."""
    rng = random.Random(1)
    means = sorted(statistics.fmean(rng.choices(diff_values, k=len(diff_values))) for _ in range(n))
    return round(means[int(0.025 * n)], 3), round(means[int(0.975 * n)], 3)


def score():
    saved = json.loads(KEY.read_text())
    key = saved["outputs"]
    require_complete(list(key.values()), saved["arms"])
    decided = json.loads((J / "handcheck-decisions.json").read_text())
    final = {}
    for line in open(J / "merged.jsonl"):
        r = json.loads(line)
        if r["output_id"] in decided:
            final[r["output_id"]] = decided[r["output_id"]]
        else:
            assert r["labels"] == ["missed", "missed"], f"{r['output_id']} needs a hand-check decision"
            final[r["output_id"]] = "missed"
    sets = brief_oracle.sets()
    conds = conditions(saved["arms"])
    rate = collections.defaultdict(dict)  # (set, condition, model) -> bug -> catch share
    for group, bugs in sets.items():
        for cond in conds:
            for model in MODELS:
                for bug in bugs:
                    labels = [final[o] for o, k in key.items()
                              if k["bug"] == bug and k["condition"] == cond and k["model"] == model]
                    if labels:
                        rate[(group, cond, model)][bug] = (sum(l == "caught" for l in labels) / len(labels),
                                                           sum(l == "partial" for l in labels) / len(labels),
                                                           len(labels))
    report = {"sets": {k: len(v) for k, v in sets.items()}, "rates": [], "effects": [], "per_arm": []}
    for (group, cond, model), per in sorted(rate.items()):
        caught = [v[0] for v in per.values()]
        report["rates"].append({"set": group, "condition": cond, "model": model, "diffs": len(per),
                                "sessions": sum(v[2] for v in per.values()),
                                "catch_rate": round(statistics.fmean(caught), 3), "ci95": boot(caught),
                                "partial_rate": round(statistics.fmean(v[1] for v in per.values()), 3)})
    for kind in conds[1:]:
        for group in sets:
            for model in MODELS:
                a, b = rate[(group, kind, model)], rate[(group, "no-brief", model)]
                common = sorted(set(a) & set(b))
                assert len(common) == len(sets[group]), (kind, group, model)
                diffs = [a[x][0] - b[x][0] for x in common]
                report["effects"].append({"brief": kind, "set": group, "model": model, "diffs": len(common),
                                          "effect_points": round(100 * statistics.fmean(diffs), 1),
                                          "ci95_points": [round(100 * x, 1) for x in boot(diffs)]})
    for model in MODELS:  # expected catches of 30 for one reviewer
        for cond in conds:
            report.setdefault("expected_catches_of_30", []).append({"model": model, "condition": cond, "value": round(
                sum(v[0] for g in sets for v in rate.get((g, cond, model), {}).values()), 1)})
    for cond in conds:
        for model in MODELS:
            ks = [k for k in key.values() if k["condition"] == cond and k["model"] == model]
            lat = [k["latency_s"] for k in ks if k["latency_s"] is not None]
            report["per_arm"].append({"condition": cond, "model": model, "sessions": len(ks),
                                      "median_tokens": statistics.median(k["total_tokens"] for k in ks),
                                      "median_latency_s": statistics.median(lat) if lat else None,
                                      "median_tool_calls": statistics.median(k["tool_calls"] for k in ks)})
    save(J / "score.json", report)
    print(json.dumps(report, indent=1))


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("step", choices=["build", "collect", "score"])
    p.add_argument("--arms", default="OT,OS")
    p.add_argument("--bugs-per-batch", type=int, default=3)
    p.add_argument("--name", default="brief-judge", help="judging round: batches in <name>/, key in private/")
    a = p.parse_args()
    J, KEY = ROOT / a.name, ROOT / "private" / f"key-{a.name}.json"
    if a.step == "build":
        build(tuple(a.arms.split(",")), a.bugs_per_batch)
    else:
        {"collect": collect, "score": score}[a.step]()
