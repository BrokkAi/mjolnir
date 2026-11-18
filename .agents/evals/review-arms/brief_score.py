"""Judges known-defect catches for the relation-brief arms and scores them (brief/PLAN.md).

Usage:
  python3 brief_score.py build [--arms OT,OS] [--bugs-per-batch 3] [--name brief-judge]
      blind judge batches of stored no-brief and brief sessions -> $REVIEW_ARMS_DIR/<name>/
      (stage B: --arms OT,OS,BT,BS --name brief-judge-b, so oracle and blind are scored apart
       and stage A's batches and key are kept)
      (run `verify_driver.py <name> --kind judge`, which writes <name>/workflow-out.json;
       stages A and B ran brief/workflow-judge.js with the args build prints instead)
      --bugs-from <jobs file>: only the diffs of that jobs file ([{"bug", "arm"}]); kept in the key,
      so collect and score use the same subset (round G of brief/PLAN-ablation.md)
      --models gpt-6.1-terra: only Terra cells, with exactly3 no-brief sessions per diff;
      models and scoped sets are saved in the key for collect/score. Default keeps both models
      and the legacy baseline minimum.
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
from fractions import Fraction

import brief_oracle
from driver import ARMS, CORPUS, BRIEF_ARMS, QUERY_ARMS, ROOT, SCRIPTS, save

# The condition each judged arm stands for: its brief kind, or "site-tool" for the query arms
# (brief/PLAN-oz.md), which carry no brief but a site_context tool.
CONDITION = {**BRIEF_ARMS, **{a: "site-tool" for a in QUERY_ARMS}}

BASELINE_ARMS = ("A", "B", "D", "D2")
MODELS = ("gpt-6.1-terra", "gpt-6.1-sol")
# Set from --name: each judging round (stage A, stage B) has its own batches and key.
J = KEY = None
# Set from --bugs-from, or from the round's key: the diffs this round covers (None: all 30).
BUGS = None
SELECTED_MODELS = MODELS
EXACT_BASELINE = False
ROUND_SETS = None
READING = None
REFERENCE_ROUND = None


def scoped_sets():
    """brief_oracle.sets() restricted to the round's diffs; groups left empty are dropped."""
    if ROUND_SETS is not None:
        return ROUND_SETS
    frozen = brief_oracle.sets()
    known = {b for bugs in frozen.values() for b in bugs}
    if BUGS is not None and (not BUGS or BUGS - known):
        raise SystemExit(f"empty or unknown round subset: {sorted(BUGS - known)}")
    s = {g: [b for b in bugs if BUGS is None or b in BUGS] for g, bugs in frozen.items()}
    return {g: bugs for g, bugs in s.items() if bugs}


def latency(session_dir):
    """Seconds from the reviewer's prompt to the end of its turn; the same definition as score.py."""
    # Reviewers captured after mj 8fe3c40bd have no session or transcript (driver.py reads the
    # stored handback instead), so their latency is unknown, not zero.
    if not all((session_dir / name).exists() for name in ("session.json", "transcript.json")):
        return None
    row = json.loads((session_dir / "session.json").read_text())
    done = (row.get("last_turn_outcome") or {}).get("completed_at_ms")
    items = [i for p in json.loads((session_dir / "transcript.json").read_text()) for i in p["items"]]
    prompt = next((i["created_at_ms"] for i in items if i["role"] == "user"), None)
    return round((done - prompt) / 1000, 1) if done and prompt else None


def sessions(arms):
    """Every finished high-effort Terra or Sol session with output on the 30 Hard-won diffs."""
    out = []
    for bug in sum(scoped_sets().values(), []):
        for arm in (*BASELINE_ARMS, *arms):
            done = ROOT / "runs" / bug / arm / "done.json"
            if not done.exists():
                continue
            for r in json.loads(done.read_text())["reviewers"]:
                sd = done.parent / "reviewers" / r["child"][:12]
                if r["effort"] != "high" or r["model"] not in SELECTED_MODELS or not (sd / "output.md").exists():
                    continue
                usage = json.loads((sd / "usage.json").read_text())
                tools = (sum(1 for p in json.loads((sd / "transcript.json").read_text()) for i in p["items"]
                             if i["role"] in ("tool", "terminal"))
                         if (sd / "transcript.json").exists() else None)
                out.append({"bug": bug, "arm": arm, "condition": CONDITION[arm] if arm in arms else "no-brief",
                            "session": r["child"], "model": r["model"], "dir": str(sd),
                            "latency_s": latency(sd), "tool_calls": tools,
                            "total_tokens": usage["totals"]["total_tokens"]["tokens"]})
    return out


def conditions(arms):
    """no-brief, then each brief kind (oracle, blind) the arms carry; kinds are never pooled."""
    return ["no-brief", *sorted({CONDITION[a] for a in arms})]


def planned_cells(arms):
    """Required sessions per diff: baseline minima and selected brief-arm counts."""
    planned = collections.Counter({("no-brief", model): 3 for model in SELECTED_MODELS})
    for arm in arms:
        for spec in ARMS[arm]:
            model, effort = spec.rsplit(":", 1)
            if model in SELECTED_MODELS and effort == "high":
                planned[(CONDITION[arm], model)] += 1
    return planned


def require_complete(rows, arms):
    """Refuses a partial sample: every frozen diff needs its planned sessions for every model.

    No-brief: the stored arms give each Hard-won diff at least 3 sessions per model.
    Each brief kind: exactly the sessions its arms launch (driver.ARMS), all with output.
    """
    planned = planned_cells(arms)
    have = collections.Counter((r["bug"], r["condition"], r["model"]) for r in rows)
    gaps = []
    bugs = set(sum(scoped_sets().values(), []))
    for (bug, kind, model), count in sorted(have.items()):
        if bug in bugs and (kind, model) not in planned:
            gaps.append(f"{bug} {kind} {model}: {count} in an unplanned cell (expected 0)")
    for bug in sorted(bugs):
        for (kind, model), count in planned.items():
            n = have[(bug, kind, model)]
            if (n < count if kind == "no-brief" and not EXACT_BASELINE else n != count):
                qualifier = "at least " if kind == "no-brief" and not EXACT_BASELINE else ""
                gaps.append(f"{bug} {kind} {model}: {n} of {qualifier}{count}")
    if len({r["session"] for r in rows}) != len(rows):
        gaps.append("duplicate reviewer session")
    if not bugs:
        gaps.append("empty round")
    if any(r["bug"] not in bugs for r in rows):
        gaps.append("outputs outside the round subset")
    if gaps:
        raise SystemExit("incomplete sample; not scoring a subset:\n" + "\n".join(gaps))


def build(arms, per_batch):
    if KEY.exists():
        # New output IDs would orphan any verdicts already returned for this round.
        raise SystemExit(f"{KEY} exists: round {J.name} is already built; use another --name")
    J.mkdir(parents=True, exist_ok=True)
    KEY.parent.mkdir(parents=True, exist_ok=True)
    corpus = CORPUS
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
    save(KEY, {"arms": list(arms), "outputs": key, "sets": scoped_sets(),
               "models": list(SELECTED_MODELS), "exact_baseline": EXACT_BASELINE,
               **({"reading": READING, "reference_round": REFERENCE_ROUND} if READING else {}),
               **({"bugs": sorted(BUGS)} if BUGS is not None else {})})
    print(json.dumps({"dir": str(J), "batches": batches}))
    print(json.dumps({"outputs": len(key), "by_condition_model": collections.Counter(
        f"{r['condition']}/{r['model']}" for r in rows)}), file=sys.stderr)


def load_round():
    global BUGS, SELECTED_MODELS, EXACT_BASELINE, ROUND_SETS, READING, REFERENCE_ROUND
    saved = json.loads(KEY.read_text())
    READING = saved.get("reading")
    REFERENCE_ROUND = saved.get("reference_round")
    BUGS = set(saved["bugs"]) if "bugs" in saved else None
    SELECTED_MODELS = tuple(saved.get("models", MODELS))
    if not SELECTED_MODELS or len(set(SELECTED_MODELS)) != len(SELECTED_MODELS) or set(SELECTED_MODELS) - set(MODELS):
        raise SystemExit("invalid models in round key")
    EXACT_BASELINE = saved.get("exact_baseline", False)
    ROUND_SETS = saved.get("sets")
    require_complete(list(saved["outputs"].values()), saved["arms"])
    return saved


def collect():
    saved = load_round()
    out = json.loads((J / "workflow-out.json").read_text())
    assert not out["missing"], out["missing"]
    verdicts = collections.defaultdict(list)
    for v in out["verdicts"]:
        verdicts[v["output_id"]].append(v)
    key = saved["outputs"]
    assert set(verdicts) == set(key), "every output needs verdicts"
    assert all(sorted(r["judge"] for r in v) == ["A", "B"] for v in verdicts.values()), "every output needs two judges A/B"
    assert all(v["label"] in ("caught", "partial", "missed") for vs in verdicts.values() for v in vs), "invalid judge label"
    # A judge can attach an output to the wrong defect of its batch; such a verdict is void.
    wrong = sorted(f"{v['output_id']}/{v['judge']}: {v['bug']} != {key[v['output_id']]['bug']}"
                   for vs in verdicts.values() for v in vs if v["bug"] != key[v["output_id"]]["bug"])
    if wrong:
        raise SystemExit("verdicts name the wrong defect; rejudge these outputs:\n" + "\n".join(wrong))
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


def boot_difference(p_values, c_values, n=10000):
    """95% bootstrap interval of mean(p) - mean(c), resampling the two diff sets separately."""
    rng = random.Random(2)
    d = sorted(statistics.fmean(rng.choices(p_values, k=len(p_values)))
               - statistics.fmean(rng.choices(c_values, k=len(c_values))) for _ in range(n))
    return round(d[int(0.025 * n)], 3), round(d[int(0.975 * n)], 3)


def decide(rate):
    """Stage A decision of brief/PLAN.md, from the oracle effect on P and the control gain on C."""
    per_model = {}
    for model in MODELS:
        p_b, p_n = rate[("P", "oracle", model)], rate[("P", "no-brief", model)]
        c_b, c_n = rate[("C", "oracle", model)], rate[("C", "no-brief", model)]
        p = [p_b[x][0] - p_n[x][0] for x in sorted(p_b)]
        c = [c_b[x][0] - c_n[x][0] for x in sorted(c_b)]
        per_model[model] = {"effect": statistics.fmean(p), "ci": boot(p),
                            "control_gain": statistics.fmean(c), "p": p, "c": c}
    control_passes = all(m["control_gain"] < 0.10 for m in per_model.values())
    for m in per_model.values():
        if control_passes:
            m["decision_effect"], m["decision_ci"] = m["effect"], m["ci"]
        else:  # the brief helps as a second reviewer would; decide on the effect net of that gain
            m["decision_effect"] = m["effect"] - m["control_gain"]
            m["decision_ci"] = boot_difference(m["p"], m["c"])
    eff = {k: m["decision_effect"] for k, m in per_model.items()}
    upper = {k: m["decision_ci"][1] for k, m in per_model.items()}
    if all(e < 0.15 for e in eff.values()):
        decision = "null" if all(u < 0.30 for u in upper.values()) else "inconclusive"
    elif any(e >= 0.30 for e in eff.values()) and control_passes:
        decision = "supports building"
    else:  # includes a net effect of 30 points or more when control 1 failed
        decision = "between"
    pts = lambda x: round(100 * x, 1)
    return {"decision": decision, "control_1_passes": control_passes,
            "per_model": {k: {"effect_points": pts(m["effect"]), "ci95_points": [pts(x) for x in m["ci"]],
                              "control_gain_points": pts(m["control_gain"]),
                              "decision_effect_points": pts(m["decision_effect"]),
                              "decision_ci95_points": [pts(x) for x in m["decision_ci"]]}
                          for k, m in per_model.items()}}


def decide_c(rate):
    """Stage C decision of brief/PLAN-bifrost.md. E = F - N and G = B - F per model on P, with paired
    bootstrap intervals over the diffs; control 1 on C (F - N < 10 points for both models), else E is
    taken net of the C gain as in stage A. Terra/high decides; outcomes are checked in plan order."""
    terra, sol = MODELS
    per_model = {}
    for model in MODELS:
        f, n, b = (rate[("P", cond, model)] for cond in ("bifrost", "no-brief", "blind"))
        c_f, c_n = rate[("C", "bifrost", model)], rate[("C", "no-brief", model)]
        e = [f[x][0] - n[x][0] for x in sorted(f)]
        g = [b[x][0] - f[x][0] for x in sorted(f)]
        c = [c_f[x][0] - c_n[x][0] for x in sorted(c_f)]
        per_model[model] = {"effect": statistics.fmean(e), "ci": boot(e), "gap": statistics.fmean(g),
                            "gap_ci": boot(g), "control_gain": statistics.fmean(c), "e": e, "c": c}
    control_passes = all(m["control_gain"] < 0.10 for m in per_model.values())
    for m in per_model.values():
        if control_passes:
            m["decision_effect"], m["decision_ci"] = m["effect"], m["ci"]
        else:
            m["decision_effect"] = m["effect"] - m["control_gain"]
            m["decision_ci"] = boot_difference(m["e"], m["c"])
    t = per_model[terra]
    if t["decision_effect"] >= 0.30 and t["gap"] <= 0.10 and control_passes:
        decision = "location is enough"
    elif all(m["decision_effect"] < 0.15 and m["decision_ci"][1] < 0.30 for m in per_model.values()):
        decision = "site context needed"
    elif t["decision_effect"] >= 0.15 and t["gap_ci"][0] > 0:
        decision = "relations add"
    else:
        decision = "inconclusive"
    pts = lambda x: round(100 * x, 1)
    return {"decision": decision, "control_1_passes": control_passes, "primary_model": terra,
            "per_model": {k: {"effect_points": pts(m["effect"]), "ci95_points": [pts(x) for x in m["ci"]],
                              "gap_points": pts(m["gap"]), "gap_ci95_points": [pts(x) for x in m["gap_ci"]],
                              "control_gain_points": pts(m["control_gain"]),
                              "decision_effect_points": pts(m["decision_effect"]),
                              "decision_ci95_points": [pts(x) for x in m["decision_ci"]]}
                          for k, m in per_model.items()}}


def boot(diff_values, n=10000):
    """95% bootstrap interval of the mean over diffs."""
    rng = random.Random(1)
    means = sorted(statistics.fmean(rng.choices(diff_values, k=len(diff_values))) for _ in range(n))
    return round(means[int(0.025 * n)], 3), round(means[int(0.975 * n)], 3)


def boot_exact(diff_values, n=10000):
    """Exact paired bootstrap quantiles, converted only after strict decisions."""
    rng = random.Random(1)
    means = sorted(sum((diff_values[i] for i in rng.choices(range(len(diff_values)), k=len(diff_values))),
                       Fraction()) / len(diff_values) for _ in range(n))
    return means[int(.025 * n)], means[int(.975 * n)]


def oracle_outcome(estimate, low, high):
    if estimate >= Fraction(17, 100) and low > 0:
        return "Facts carry"
    if low > 0 and estimate < Fraction(17, 100):
        return "Partly"
    if high < Fraction(10, 100):
        return "Facts don't carry"
    return "Inconclusive"


def score():
    saved = load_round()
    key = saved["outputs"]
    decided = json.loads((J / "handcheck-decisions.json").read_text())
    final = {}
    for line in open(J / "merged.jsonl"):
        r = json.loads(line)
        assert r["output_id"] in key and r["output_id"] not in final, "unknown or duplicate merged output"
        assert r["bug"] == key[r["output_id"]]["bug"], "merged verdict names wrong defect"
        assert len(r["labels"]) == 2 and set(r["labels"]) <= {"caught", "partial", "missed"}, "invalid merged labels"
        if r["output_id"] in decided:
            final[r["output_id"]] = decided[r["output_id"]]
        else:
            assert r["labels"] == ["missed", "missed"], f"{r['output_id']} needs a hand-check decision"
            final[r["output_id"]] = "missed"
    assert set(final) == set(key), "merged verdicts must cover exactly the round outputs"
    assert set(final.values()) <= {"caught", "partial", "missed"}, "invalid handcheck labels"
    sets = scoped_sets()
    conds = conditions(saved["arms"])
    planned = planned_cells(saved["arms"])
    rate = collections.defaultdict(dict)  # (set, condition, model) -> bug -> catch share
    for group, bugs in sets.items():
        for cond, model in planned:
            for bug in bugs:
                labels = [final[o] for o, k in key.items()
                          if k["bug"] == bug and k["condition"] == cond and k["model"] == model]
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
    for kind, model in planned:
        if kind == "no-brief":
            continue
        for group in sets:
            a, b = rate[(group, kind, model)], rate[(group, "no-brief", model)]
            common = sorted(set(a) & set(b))
            assert len(common) == len(sets[group]), (kind, group, model)
            diffs = [a[x][0] - b[x][0] for x in common]
            report["effects"].append({"brief": kind, "set": group, "model": model, "diffs": len(common),
                                      "effect_points": round(100 * statistics.fmean(diffs), 1),
                                      "ci95_points": [round(100 * x, 1) for x in boot(diffs)]})
    for cond, model in planned:  # expected catches of 30 for one reviewer
        report.setdefault("expected_catches_of_30", []).append({"model": model, "condition": cond, "value": round(
            sum(v[0] for g in sets for v in rate[(g, cond, model)].values()), 1)})
        ks = [k for k in key.values() if k["condition"] == cond and k["model"] == model]
        lat = [k["latency_s"] for k in ks if k["latency_s"] is not None]
        report["per_arm"].append({"condition": cond, "model": model, "sessions": len(ks),
                                  "median_tokens": statistics.median(k["total_tokens"] for k in ks),
                                  "median_latency_s": statistics.median(lat) if lat else None,
                                  "median_tool_calls": (statistics.median(t) if (t := [k["tool_calls"] for k in ks if k["tool_calls"] is not None]) else None)})
    if READING == "bifrost-oracle":
        import brief_writer_terra
        if set(sets) != {"P"} or len(sets["P"]) != 17 or SELECTED_MODELS != ("gpt-6.1-terra",):
            raise SystemExit("bifrost-oracle requires exactly 17 P diffs and Terra only")
        bugs = sets["P"]
        per_diff = {}
        for bug in bugs:
            per_diff[bug] = {}
            for arm in ("A", "BT", "OT"):
                entries = [(oid, k) for oid, k in key.items() if k["bug"] == bug and k["arm"] == arm]
                if len(entries) != 3 or any(k["model"] != "gpt-6.1-terra" for _, k in entries):
                    raise SystemExit(f"{bug} {arm}: requires exactly three Terra sessions")
                labels = [final[oid] for oid, _ in entries]
                per_diff[bug][arm] = {"sessions": 3, "caught": labels.count("caught"),
                                      "partial": labels.count("partial"), "catch_rate": labels.count("caught") / 3,
                                      "partial_rate": labels.count("partial") / 3}
        effects = {arm: [Fraction(per_diff[b]["OT"]["caught"], 3) -
                         Fraction(per_diff[b][arm]["caught"], 3) for b in bugs]
                   for arm in ("A", "BT")}
        contrasts, raw_contrasts = {}, {}
        for arm, values in effects.items():
            estimate = sum(values, Fraction()) / len(values)
            low, high = boot_exact(values)
            raw_contrasts[arm] = (estimate, low, high)
            contrasts[f"OT-{arm}"] = {"difference": float(estimate), "ci95": [float(low), float(high)]}
        estimate, low, high = raw_contrasts["A"]
        report["expected_catches_per_reviewer"] = report.pop("expected_catches_of_30")
        report["bifrost_oracle"] = {"outcome": oracle_outcome(estimate, low, high),
                                    "bootstrap": {"seed": 1, "draws": 10000, "unit": "paired diff",
                                                  "classification_uses_unrounded_rational_bounds": True},
                                    "OT_minus_A": contrasts["OT-A"], "OT_minus_BT": contrasts["OT-BT"],
                                    "per_diff": per_diff}
        report["label_consistency"] = brief_writer_terra.compare_same_sessions(
            J.name, REFERENCE_ROUND, bugs, arms=("A", "BT"), model="gpt-6.1-terra")
        save(J / "score.json", report)
        print(json.dumps(report, indent=1))
        return
    if "oracle" in conds:
        report["stage_a"] = decide(rate)
    if "bifrost" in conds and "blind" in conds:
        report["stage_c"] = decide_c(rate)
    save(J / "score.json", report)
    print(json.dumps(report, indent=1))


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("step", choices=["build", "collect", "score"])
    p.add_argument("--arms", default="OT,OS")
    p.add_argument("--models", help="build: comma-separated models; explicit selection requires exactly3 baseline sessions/model")
    p.add_argument("--bugs-per-batch", type=int, default=3)
    p.add_argument("--bugs-from", help="build: jobs file ([{bug, arm}]) whose diffs the round covers")
    p.add_argument("--name", default="brief-judge", help="judging round: batches in <name>/, key in private/")
    p.add_argument("--reading", choices=["bifrost-oracle"], help="registered reading (build only)")
    p.add_argument("--reference-round", help="stored-label round for same-session comparison (build only)")
    a = p.parse_args()
    if a.step != "build" and (a.reading or a.reference_round):
        raise SystemExit("--reading and --reference-round apply to build only; values are persisted in the key")
    J, KEY = ROOT / a.name, ROOT / "private" / f"key-{a.name}.json"
    if a.bugs_from:
        if a.step != "build":
            raise SystemExit("--bugs-from applies to build; collect and score read the subset from the key")
        BUGS = {j["bug"] for j in json.loads(open(a.bugs_from).read())}
    if a.models:
        if a.step != "build":
            raise SystemExit("--models applies to build; collect and score read models from the key")
        SELECTED_MODELS = tuple(a.models.split(","))
        if not SELECTED_MODELS or len(set(SELECTED_MODELS)) != len(SELECTED_MODELS) or set(SELECTED_MODELS) - set(MODELS):
            raise SystemExit(f"--models must select distinct supported models: {MODELS}")
        EXACT_BASELINE = True
    if a.step == "build":
        if bool(a.reading) != bool(a.reference_round):
            raise SystemExit("--reading and --reference-round must be supplied together")
        READING, REFERENCE_ROUND = a.reading, a.reference_round
        if READING and (tuple(a.arms.split(",")) != ("OT", "BT") or
                        SELECTED_MODELS != ("gpt-6.1-terra",) or len(scoped_sets().get("P", [])) != 17):
            raise SystemExit("bifrost-oracle build requires --arms OT,BT, Terra only, and 17 P diffs")
        if a.bugs_per_batch < 1:
            raise SystemExit("--bugs-per-batch must be positive")
        build(tuple(a.arms.split(",")), a.bugs_per_batch)
    else:
        {"collect": collect, "score": score}[a.step]()
