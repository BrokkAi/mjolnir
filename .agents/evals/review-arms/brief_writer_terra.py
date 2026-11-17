"""Checks for the Terra- and Sonnet-written blind briefs (brief/PLAN-writer-terra.md,
brief/PLAN-writer-sonnet.md).

One implementation serves both studies; --writer {terra,sonnet} (default terra) selects the
STUDIES entry that names the brief kind, model, subject arm, judged round, reference round and
artifact paths. Everything below describes the Terra study; the Sonnet study differs only in
what STUDIES says: the writer is brief_driver.py --kind blind-sonnet (arm TS), artifacts live in
briefs/blind-sonnet*, briefs/writer-contains-sonnet and results/brief-writer-sonnet, round K
(brief-judge-k) labels TS, BT and no-brief together, round J is only a consistency reference
and TT's historical J labels are reported beside the reading, never pooled.

The Terra writer is brief_driver.py --kind blind-terra. Before any TT reviewer reads a
brief, each brief gets stage B's leak check: a known-bug judge (judge/SPEC.md)
reads the brief as a review output and must answer `missed`; a brief judged
caught or partial is rewritten once to remove consequence wording only (the
rewrite prompt of brief/workflow-oracle.js) and judged again. Stage B used one
Opus/high judge; here both judges of `verify_driver.py --kind judge` read it, and
a brief leaks when either says caught or partial.

Usage:
  python3 brief_writer_terra.py leak-build [--writer terra|sonnet] [--round 0|1]
      round 0: every brief; round 1: the briefs rewritten after round 0
      -> briefs/blind-terra-leak-<round>/ (run verify_driver.py on it with --kind judge)
  python3 brief_writer_terra.py leak-collect [--round 0|1]
      -> briefs/blind-terra/<diff>/blind-leak.json, bound to the hash of the judged text
  python3 brief_writer_terra.py rewrite-build
      -> briefs/blind-terra-rewrite/<diff>/ for each brief that leaked in round 0
      (run verify_driver.py on it with --kind task --output brief.md)
  python3 brief_writer_terra.py rewrite-collect
      lints each rewrite, keeps the writer's text as brief-writer.md, publishes brief.md
  python3 brief_writer_terra.py contains-build
      the stage A contains rule on both writers' published briefs, blinded:
      -> briefs/writer-contains/unit/ (run verify_driver.py --kind task --output contains.jsonl)
  python3 brief_writer_terra.py readout [--name brief-judge-j]
      final handchecked Terra/high rates, paired diff bootstrap, primary-only writer decision
      and round G judge consistency on identical Terra sessions of the 10 ablation diffs
      -> results/brief-writer-terra/{summary.json,per-diff.json}
      (--writer sonnet, default --name brief-judge-k: K labels for TS, BT and no-brief; TT beside
       from J; K-vs-J consistency on identical sessions of all 22 diffs, overall and P/S
       -> results/brief-writer-sonnet/)
  python3 brief_writer_terra.py usage
      writer cost, tokens, turns and wall time per brief for each writer of the study
      (missing measurements stay null) -> results/brief-writer-<study>/{usage.jsonl,usage-summary.json}
"""
import argparse
import hashlib
import json
import pathlib
import random
import secrets
import statistics
from fractions import Fraction

import brief_ablation
import brief_driver
import brief_oracle
import brief_score
import driver
from driver import ROOT, SCRIPTS, save

# One mapping serves every study; each workflow reads its study-specific names from it.
STUDIES = {
    "terra": {"arm": "TT", "kind": "blind-terra", "model": "gpt-6.1-terra", "results": "brief-writer-terra",
              "round": "brief-judge-j", "reference": "brief-judge-g", "scope": "ablation",
              "contains": "writer-contains", "contains_kinds": ("blind", "blind-terra"),
              "usage_kinds": ("blind", "blind-terra"), "historical": None},
    # Sonnet writer, plan brief/PLAN-writer-sonnet.md: native model ID `sonnet` (Sonnet 5.5), high effort.
    "sonnet": {"arm": "TS", "kind": "blind-sonnet", "model": "sonnet", "results": "brief-writer-sonnet",
               "round": "brief-judge-k", "reference": "brief-judge-j", "scope": "all",
               "contains": "writer-contains-sonnet", "contains_kinds": ("blind", "blind-sonnet"),
               "usage_kinds": ("blind", "blind-terra", "blind-sonnet"),
               "historical": {"arm": "TT", "kind": "blind-terra", "round": "brief-judge-j"}},
}
TERRA = STUDIES["terra"]
SOL_WRITER = "gpt-6.1-sol"
REVIEW_MODEL = "gpt-6.1-terra"
BOOT_SEED = 20261116
CORPUS_FIELDS = ("defect", "location", "trigger", "match_rule", "near_misses", "why_findable",
                 "intro_commit", "fix_commit")


def sha(text):
    return hashlib.sha256(text.encode()).hexdigest()


def diffs():
    s = brief_oracle.sets()
    return {b: g for g in ("P", "S") for b in s[g]}


def out_dir(study):
    return ROOT / "briefs" / study["kind"]


def leak_dir(n, study=TERRA):
    return ROOT / "briefs" / f"{study['kind']}-leak-{n}"


def leak_build(n, study=TERRA):
    OUT = out_dir(study)
    corpus = {r["id"]: r for r in map(json.loads, open(SCRIPTS / "corpus/corpus.jsonl"))}
    if n == 0:
        bugs = list(diffs())
    else:
        bugs = [b for b in diffs() if (OUT / b / "brief-writer.md").exists()]
    missing = [b for b in bugs if not (OUT / b / "done.json").exists()]
    if missing:
        raise SystemExit(f"briefs not written yet: {missing}")
    d = leak_dir(n, study)
    if (d / "key.json").exists():
        raise SystemExit(f"{d} already exists; its key binds verdicts to texts")
    batch, key = [], {}
    for b in bugs:
        brief = (OUT / b / "brief.md").read_text()
        oid = secrets.token_hex(8)
        key[oid] = {"bug": b, "brief_sha256": sha(brief)}
        batch.append({"bug": b, **{k: corpus[b][k] for k in CORPUS_FIELDS},
                      "outputs": [{"output_id": oid, "text": brief}]})
    save(d / "batch-0.json", batch)
    save(d / "key.json", key)
    (d / "SPEC.md").write_text((SCRIPTS / "judge/SPEC.md").read_text())
    print(f"wrote {d} for {len(batch)} briefs")


def leak_collect(n, study=TERRA):
    OUT = out_dir(study)
    d = leak_dir(n, study)
    key = json.loads((d / "key.json").read_text())
    verdicts = json.loads((d / "workflow-out.json").read_text())["verdicts"]
    judged = {}
    for v in verdicts:
        judged.setdefault(v["output_id"], []).append(v)
    rows = []
    for oid, k in key.items():
        b, vs = k["bug"], judged.get(oid, [])
        assert sorted(v["judge"] for v in vs) == ["A", "B"], (b, vs)
        brief = (OUT / b / "brief.md").read_text()
        # A verdict counts only for the text it was given.
        assert sha(brief) == k["brief_sha256"], f"{b}: brief.md changed since leak round {n} was built"
        label = "missed" if all(v["label"] == "missed" for v in vs) else \
            "caught" if any(v["label"] == "caught" for v in vs) else "partial"
        path = OUT / b / "blind-leak.json"
        record = json.loads(path.read_text()) if path.exists() else {"id": b, "group": diffs()[b], "rounds": []}
        record["rounds"] = [r for r in record["rounds"] if r["round"] != n] + [
            {"round": n, "label": label, "brief_sha256": k["brief_sha256"], "judge_dir": str(d),
             "verdicts": [{f: v.get(f) for f in ("judge", "label", "matching_finding", "rationale", "confidence")}
                          for v in sorted(vs, key=lambda v: v["judge"])]}]
        save(path, record)
        rows.append({"bug": b, "round": n, "label": label, "judges": [v["label"] for v in vs]})
    for r in sorted(rows, key=lambda r: r["bug"]):
        print(json.dumps(r))
    print(json.dumps({"round": n, "labels": {l: sum(r["label"] == l for r in rows)
                                             for l in ("missed", "partial", "caught")}}))


REWRITE = """A brief of relations for a code reviewer (rules in WRITER.md in this folder) was judged to point at a known defect. The judges' reasons:

{reasons}

Rewrite it so that it states only relations between code locations, with no judgment, consequence or hint of what goes wrong. Keep every relation and every file:line anchor, including the one the judges matched: remove only wording that goes beyond the relation. A clause that says what code does or does not do, such as which paths a script removes and which it leaves, or which function a call reaches, is itself a relation: keep it, even if it points at the defect. Remove only evaluative or consequence wording (for example "only" used as emphasis, "so ...", "which means ..."). Change nothing else.

The brief is brief-in.md in this folder. Write the rewritten brief to brief.md in your working directory, and hand back a short list of the changes you made.
"""


def rewrite_build(study=TERRA):
    OUT = out_dir(study)
    base = ROOT / "briefs" / f"{study['kind']}-rewrite"
    made = 0
    for b in diffs():
        record = json.loads((OUT / b / "blind-leak.json").read_text())
        first = next(r for r in record["rounds"] if r["round"] == 0)
        if first["label"] == "missed" or (base / b).exists():
            continue
        u = base / b
        brief = (OUT / b / "brief.md").read_text()
        assert sha(brief) == first["brief_sha256"], f"{b}: brief.md is not the text judged in round 0"
        (u / "brief-in.md").parent.mkdir(parents=True, exist_ok=True)
        (u / "brief-in.md").write_text(brief)
        (u / "WRITER.md").write_text((SCRIPTS / "brief/WRITER.md").read_text())
        reasons = "\n".join(f"- {v['rationale']}" for v in first["verdicts"] if v["label"] != "missed")
        (u / "INSTRUCTIONS.md").write_text(REWRITE.format(reasons=reasons))
        made += 1
    print(f"wrote {made} rewrite units under {base}")


def rewrite_collect(study=TERRA):
    OUT = out_dir(study)
    base = ROOT / "briefs" / f"{study['kind']}-rewrite"
    for u in sorted(p for p in base.iterdir() if p.is_dir()):
        b, d = u.name, OUT / u.name
        out = u / "brief.md"
        if not out.exists():
            print(json.dumps({"bug": b, "state": "no rewrite fetched"}))
            continue
        text = out.read_text()
        problems = brief_driver.lint(text, driver.CORPUS[b])
        if problems:
            print(json.dumps({"bug": b, "state": "rewrite fails lint", "problems": problems}))
            continue
        if not (d / "brief-writer.md").exists():
            (d / "brief-writer.md").write_text((u / "brief-in.md").read_text())
        (d / "brief.md").write_text(text)
        print(json.dumps({"bug": b, "state": "published", "brief_sha256": sha(text)}))


def contains_build(study=TERRA):
    base = ROOT / "briefs" / study["contains"]
    unit = base / "unit"
    if (base / "key.json").exists():
        raise SystemExit(f"{base} already exists")
    unit.mkdir(parents=True)
    (unit / "CONTAINS.md").write_text((ROOT / "briefs/bifrost-contains/CONTAINS.md").read_text())
    key, ids = {}, []
    for b, group in diffs().items():
        census = json.loads((ROOT / f"briefs/bifrost-contains/item-{b}.json").read_text())["census"]
        for kind in study["contains_kinds"]:
            brief = (ROOT / "briefs" / kind / b / "brief.md").read_text()
            oid = secrets.token_hex(8)
            key[oid] = {"bug": b, "kind": kind, "brief_sha256": sha(brief)}
            bug = driver.CORPUS[b]
            save(unit / f"item-{oid}.json", {"id": oid, "group": group, "intro": bug["intro_commit"],
                                             "base": bug["base_commit"], "census": census, "brief": brief})
            ids.append(oid)
    (unit / "INSTRUCTIONS.md").write_text(
        f"Read CONTAINS.md in this folder and follow it exactly. Your item files are the {len(ids)} "
        "item-*.json files in this folder.\n")
    save(base / "key.json", key)  # outside the unit: never uploaded
    print(f"wrote {unit} with {len(ids)} items")


def known(value, *path):
    """A measurement that may be absent (for example a Claude session's cost): None, never 0."""
    for key in path:
        value = value.get(key) if isinstance(value, dict) else None
    return value


def usage(study=TERRA):
    rows = []
    for b, group in diffs().items():
        for kind in study["usage_kinds"]:
            d = ROOT / "briefs" / kind / b
            if not (d / "done.json").exists():
                rows.append({"bug": b, "group": group, "kind": kind, "state": "not written"})
                continue
            [u] = [json.loads(p.read_text()) for p in (d / "reviewers").glob("*/usage.json")]
            intent = json.loads((d / "launch-intent.json").read_text())
            turns = u.get("turns") or []
            ends = [t["completed_at_ms"] for t in turns if t.get("completed_at_ms") is not None]
            ticks = u.get("turn_cost_usd_ticks")
            missing_cost = known(u, "coverage", "missing_cost_turns")
            rows.append({"bug": b, "group": group, "kind": kind, "model": intent["model"],
                         "effort": intent["effort"], "turns": len(turns),
                         "fix_rounds": json.loads((d / "done.json").read_text())["fix_rounds"],
                         "cost_usd": ticks / 1e10 if ticks is not None and missing_cost == 0 else None,
                         "known_cost_subtotal_usd": ticks / 1e10 if ticks is not None else None,
                         "cost_turns_estimated": known(u, "coverage", "estimated_cost_turns"),
                         "cost_turns_missing": missing_cost,
                         "total_tokens": known(u, "totals", "total_tokens", "tokens"),
                         # From launch intent (before mj new) to the end of the writer's last turn.
                         "wall_s": round(max(ends) / 1000 - intent["time_ns"] / 1e9, 1) if ends else None})
    base = SCRIPTS / "results" / study["results"]
    save_jsonl(base / "usage.jsonl", rows)
    summaries = []
    for kind in study["usage_kinds"]:
        rs = [r for r in rows if r["kind"] == kind and r.get("turns")]
        if not rs:
            continue
        costs = [r["cost_usd"] for r in rs if r["cost_usd"] is not None]
        subtotals = [r["known_cost_subtotal_usd"] for r in rs if r["known_cost_subtotal_usd"] is not None]
        medians = {}
        for k in ("cost_usd", "total_tokens", "wall_s", "turns"):
            values = [r[k] for r in rs if r[k] is not None]
            medians[k] = statistics.median(values) if values else None
        summaries.append({"kind": kind, "briefs": len(rs), "median": medians,
                          "total_cost_usd": round(sum(costs), 2) if len(costs) == len(rs) else None,
                          "known_cost_usd": round(sum(subtotals), 2) if subtotals else None,
                          "unknown_cost": len(rs) - len(costs)})
    for summary in summaries:
        print(json.dumps(summary))
    save(base / "usage-summary.json", summaries)


def conditions(study):
    return {"no-brief": "no-brief", "BT": "blind", study["arm"]: study["kind"]}


def judged_round(round_name):
    """Validate coverage and labels before using the shared final-label helper."""
    key = json.loads((ROOT / "private" / f"key-{round_name}.json").read_text())["outputs"]
    merged = [json.loads(line) for line in (ROOT / round_name / "merged.jsonl").read_text().splitlines()
              if line.strip()]
    ids = [r["output_id"] for r in merged]
    if len(ids) != len(set(ids)) or set(ids) != set(key):
        raise SystemExit("incomplete or duplicate merged verdict coverage: must match the round key")
    valid = {"caught", "partial", "missed"}
    for r in merged:
        if r.get("bug") != key[r["output_id"]]["bug"] or len(r["labels"]) != 2 or not set(r["labels"]) <= valid:
            raise SystemExit(f"invalid merged verdict: {r['output_id']}")
    try:
        final = brief_ablation.final_labels(round_name)
    except AssertionError as e:
        raise SystemExit(str(e)) from e
    if any(label not in valid for label in final.values()):
        raise SystemExit("invalid final label: expected caught, partial or missed")
    return key, final


def observations(round_name, groups, arm_conditions):
    """Require every planned observation of the given arm -> condition cells before using the rate helper."""
    bugs = {b for g in groups.values() for b in g}
    brief_arm = {cond: arm for arm, cond in arm_conditions.items() if cond != "no-brief"}
    key, final = judged_round(round_name)
    cells = {(b, cond): [] for b in bugs for cond in arm_conditions.values()}
    for oid, k in key.items():
        cell = (k["bug"], k["condition"])
        if k["model"] != REVIEW_MODEL or cell not in cells:
            continue
        expected = brief_score.CONDITION.get(k["arm"], "no-brief"
                                             if k["arm"] in brief_score.BASELINE_ARMS else None)
        if expected != k["condition"] or brief_arm.get(k["condition"], k["arm"]) != k["arm"]:
            raise SystemExit(f"wrong arm/condition in round key: {oid}")
        cells[cell].append((oid, k["session"]))
    gaps = []
    for (bug, cond), rows in sorted(cells.items()):
        n = len(rows)
        if (n < 3 if cond == "no-brief" else n != 3):
            gaps.append(f"{bug} {cond}: {n} sessions; need {'at least' if cond == 'no-brief' else 'exactly'} 3")
    selected = [session for rows in cells.values() for _, session in rows]
    if len(selected) != len(set(selected)):
        gaps.append("duplicate reviewer session in selected observations")
    if gaps:
        raise SystemExit("incomplete sample; not scoring a subset:\n" + "\n".join(gaps))
    caught = brief_ablation.rates(round_name, bugs)
    return {bug: {arm: {"sessions": len(cells[(bug, cond)]),
                        "caught": sum(final[oid] == "caught" for oid, _ in cells[(bug, cond)]),
                        "partial": sum(final[oid] == "partial" for oid, _ in cells[(bug, cond)]),
                        "catch_rate": caught[(cond, REVIEW_MODEL)][bug],
                        "partial_rate": statistics.fmean(final[oid] == "partial"
                                                         for oid, _ in cells[(bug, cond)])}
                  for arm, cond in arm_conditions.items()} for bug in bugs}


def readout_data(round_name, groups, study=TERRA):
    return observations(round_name, groups, conditions(study))


def paired_readout(bugs, per_diff, study=TERRA):
    """Resample the SAME ordered diff indices for every arm and both contrasts."""
    subject = study["arm"]
    arms = conditions(study)
    rates = {arm: [Fraction(per_diff[b][arm]["caught"], per_diff[b][arm]["sessions"]) for b in bugs]
             for arm in arms}
    names = {f"{subject}-BT": "BT", f"{subject}-no-brief": "no-brief"}
    differences = {name: [a - b for a, b in zip(rates[subject], rates[other])] for name, other in names.items()}
    rng = random.Random(BOOT_SEED)
    draws = {name: [] for name in names}
    for _ in range(brief_ablation.BOOT):
        ix = rng.choices(range(len(bugs)), k=len(bugs))
        for name, delta in differences.items():
            draws[name].append(sum(delta[i] for i in ix) / len(bugs))
    contrasts = {}
    for name, delta in differences.items():
        values = sorted(draws[name])
        # Sort exact rational draws: cancellation must leave a truly zero bound
        # for the strict >0 rule. Convert only the final reported values.
        ci = [float(values[int(p * len(values))]) for p in (0.025, 0.975)]
        contrasts[name] = {"difference": float(sum(delta) / len(bugs)), "ci95": ci}
    return {"diffs": len(bugs), "arms": {
        arm: {"condition": cond, "sessions": sum(per_diff[b][arm]["sessions"] for b in bugs),
              "catch_rate": float(sum(rates[arm]) / len(bugs)),
              "partial_rate": statistics.fmean(per_diff[b][arm]["partial_rate"] for b in bugs)}
        for arm, cond in arms.items()}, "contrasts": contrasts}


def consistency_summary(rows, round_name, reference):
    """Mean rates and the exact current-minus-reference caught difference of BT and no-brief."""
    arms, caveats = {}, []
    names = (round_name, reference)
    for arm in ("BT", "no-brief"):
        rates = {name: {metric: statistics.fmean(r["arms"][arm]["rates"][name][metric] for r in rows)
                        for metric in ("catch_rate", "partial_rate")} for name in names}
        # Count exactly at the registered 10-point boundary; float subtraction
        # can turn equality into a spurious greater-than flag.
        difference = sum(
            Fraction(sum(label["outputs"][round_name]["label"] == "caught" for label in r["arms"][arm]["labels"]),
                     r["arms"][arm]["sessions"])
            - Fraction(sum(label["outputs"][reference]["label"] == "caught"
                           for label in r["arms"][arm]["labels"]), r["arms"][arm]["sessions"])
            for r in rows) / len(rows)
        over = abs(difference) > Fraction(1, 10)
        arms[arm] = {"sessions": sum(r["arms"][arm]["sessions"] for r in rows),
                     "rates": rates, "difference_points": float(100 * difference), "differs_over_10_points": over}
        if over:
            caveats.append(f"{arm}: {round_name} minus {reference} catch rate differs by {float(100 * difference):.1f} points")
    return arms, caveats


def judge_consistency(round_name, study=TERRA):
    """Compare final labels on the identical stored Terra sessions of the study's reference scope."""
    reference = study["reference"]
    if round_name == reference:
        raise SystemExit(f"judge consistency compares {round_name} with a different round, not itself")
    sets = brief_oracle.sets()
    bugs = brief_ablation.diffs() if study["scope"] == "ablation" else list(diffs())
    rounds = (round_name, reference)
    selected, finals = {}, {}
    for name in rounds:
        key, finals[name] = judged_round(name)
        cells = {}
        for oid, k in key.items():
            if k["model"] != REVIEW_MODEL or k["bug"] not in bugs or k["condition"] not in ("blind", "no-brief"):
                continue
            if (k["condition"] == "blind" and k["arm"] != "BT") or \
                    (k["condition"] == "no-brief" and k["arm"] not in brief_score.BASELINE_ARMS):
                raise SystemExit(f"{name}: wrong arm for judge consistency: {oid}")
            identity = (k["bug"], k["condition"], k["session"])
            if identity in cells:
                raise SystemExit(f"{name}: duplicate consistency session: {identity}")
            cells[identity] = oid
        for bug in bugs:
            for cond in ("blind", "no-brief"):
                n = sum(b == bug and c == cond for b, c, _ in cells)
                if (n != 3 if cond == "blind" else n < 3):
                    raise SystemExit(f"{name}: incomplete consistency coverage: {bug} {cond}: {n}")
        selected[name] = cells
    current, previous = (selected[name] for name in rounds)
    if current.keys() != previous.keys():
        raise SystemExit("judge consistency requires identical sessions in both rounds; "
                         f"only {round_name}: {sorted(current.keys() - previous.keys())}; "
                         f"only {reference}: {sorted(previous.keys() - current.keys())}")
    per_diff = []
    for bug in bugs:
        row = {"bug": bug, "arms": {}}
        for arm, cond in (("BT", "blind"), ("no-brief", "no-brief")):
            identities = sorted(i for i in current if i[:2] == (bug, cond))
            labels = [{"session": i[2], "outputs": {
                name: {"output_id": selected[name][i], "label": finals[name][selected[name][i]]}
                for name in rounds}} for i in identities]
            row["arms"][arm] = {"sessions": len(identities), "labels": labels,
                                 "rates": {name: {
                                     "catch_rate": statistics.fmean(finals[name][selected[name][i]] == "caught"
                                                                    for i in identities),
                                     "partial_rate": statistics.fmean(finals[name][selected[name][i]] == "partial"
                                                                      for i in identities)} for name in rounds}}
        per_diff.append(row)
    arms, caveats = consistency_summary(per_diff, round_name, reference)
    sol = {"model": "gpt-6.1-sol", "effort": "high", "profile": "codex4"}
    if study["scope"] == "ablation":
        judges = {round_name: sol, reference: {"model": "gemini-4-carbon", "effort": "high"}}
        scope = "10 ablation diffs, identical session IDs; mean per-diff caught share"
    else:
        judges = {round_name: sol, reference: sol}
        scope = "all 22 diffs, identical BT and no-brief Terra session IDs; mean per-diff caught share"
    report = {"reference_round": reference, "reviewer_model": REVIEW_MODEL, "judges": judges, "scope": scope,
              "affects_writer_decision": False, "diffs": len(bugs), "arms": arms,
              "caveats": caveats, "per_diff": per_diff}
    if study["scope"] != "ablation":
        # The same exact flag, separately for the primary and secondary diffs.
        report["groups"] = {}
        for g in ("P", "S"):
            rows = [r for r in per_diff if r["bug"] in sets[g]]
            g_arms, g_caveats = consistency_summary(rows, round_name, reference)
            report["groups"][g] = {"diffs": len(rows), "arms": g_arms, "caveats": g_caveats}
    return report


def sha_file(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def provenance(study, rounds):
    """Writer identity, graders, source rounds and hashes of the raw inputs the readout used."""
    launches = {}
    for b in diffs():
        path = out_dir(study) / b / "launch-intent.json"
        launches[b] = (lambda i: {f: i.get(f) for f in ("model", "effort", "profile")})(
            json.loads(path.read_text())) if path.exists() else None
    return {"writer": {"arm": study["arm"], "kind": study["kind"], "model": study["model"],
                       "effort": "high", "launches": launches},
            "graders": "gpt-6.1-sol/high on codex4: paired leak checks, judging, hand check and containment",
            "source_rounds": rounds,
            "raw_artifacts": {name: {part: sha_file(path) for part, path in (
                ("key", ROOT / "private" / f"key-{name}.json"), ("merged", ROOT / name / "merged.jsonl"),
                ("handcheck_decisions", ROOT / name / "handcheck-decisions.json"))} for name in rounds},
            "briefs": {kind: {b: sha_file(ROOT / "briefs" / kind / b / "brief.md") for b in diffs()}
                       for kind in ("blind", study["kind"])}}


def decision_fields(study, ok):
    if study["arm"] == "TT":
        return {"default_writer": "Terra/high" if ok else "Sol/high"}
    # Sonnet success means an acceptable writer; it changes no production default.
    return {"acceptable_writer": ok, "sonnet_decision": "acceptable writer" if ok else "Sol writes",
            "production_default_changed": False}


def readout(round_name, study=TERRA):
    if round_name == study["reference"]:
        raise SystemExit(f"{round_name} is the reference round of the {study['arm']} study; read out its own round")
    frozen = brief_oracle.sets()
    groups = {g: frozen[g] for g in ("P", "S")}
    subject = study["arm"]
    per_diff = readout_data(round_name, groups, study)
    results = {g: paired_readout(bugs, per_diff, study) for g, bugs in groups.items()}
    consistency = judge_consistency(round_name, study)
    primary = results["P"]
    within_margin = primary["contrasts"][f"{subject}-BT"]["difference"] >= -0.10
    above_zero = primary["contrasts"][f"{subject}-no-brief"]["ci95"][0] > 0
    report = {"round": round_name, "reviewer_model": REVIEW_MODEL, "reviewer_effort": "high",
              "writer_models": {subject: study["model"], "BT": SOL_WRITER}, "writer_effort": "high",
              "metric": "mean per-diff caught share; partial is separate and never counts as caught",
              "bootstrap": {"resamples": brief_ablation.BOOT, "seed": BOOT_SEED,
                            "unit": "diff", "paired": True, "confidence": 0.95,
                            "interval": "percentile order statistics at floor(0.025*n), floor(0.975*n)"},
              "decision_rule": f"Primary P only: {subject} >= BT - 0.10 AND lower95({subject}-no-brief) > 0; else Sol",
              "margin_criterion": f"point estimate only; no significance claim for {subject}-BT",
              "decision_checks": {f"{subject}_at_least_BT_minus_10_points": within_margin,
                                  f"{subject}_minus_no_brief_lower95_above_zero": above_zero},
              **decision_fields(study, within_margin and above_zero),
              "sets": results, "judge_consistency": {k: v for k, v in consistency.items() if k != "per_diff"}}
    per_diff_out = {"round": round_name, "reviewer_model": REVIEW_MODEL,
                    "judge_consistency": consistency["per_diff"],
                    "sets": {g: [{"bug": b, **per_diff[b]} for b in bugs] for g, bugs in groups.items()}}
    if study["historical"]:
        # Beside the reading only: another round's labels, never pooled with this round's.
        h = study["historical"]
        old = observations(study["reference"], groups, {h["arm"]: h["kind"]})
        report["historical_beside"] = {
            "arm": h["arm"], "kind": h["kind"], "source_round": study["reference"],
            "pooled_with_primary": False, "contrasts": None,
            "sets": {g: {"diffs": len(bugs), "sessions": sum(old[b][h["arm"]]["sessions"] for b in bugs),
                         "catch_rate": float(sum(Fraction(old[b][h["arm"]]["caught"], old[b][h["arm"]]["sessions"])
                                                 for b in bugs) / len(bugs)),
                         "partial_rate": statistics.fmean(old[b][h["arm"]]["partial_rate"] for b in bugs)}
                     for g, bugs in groups.items()}}
        per_diff_out["historical_beside"] = {g: [{"bug": b, **old[b]} for b in bugs] for g, bugs in groups.items()}
        report["provenance"] = provenance(study, [round_name, study["reference"]])
    base = SCRIPTS / "results" / study["results"]
    save(base / "per-diff.json", per_diff_out)
    save(base / "summary.json", report)
    print(json.dumps(report, indent=2))


def save_jsonl(path, rows):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(r) + "\n" for r in rows))


def main():
    p = argparse.ArgumentParser()
    p.add_argument("step", choices=["leak-build", "leak-collect", "rewrite-build", "rewrite-collect",
                                    "contains-build", "usage", "readout"])
    p.add_argument("--writer", choices=sorted(STUDIES), default="terra")
    p.add_argument("--round", type=int, choices=[0, 1], default=0)
    p.add_argument("--name", help="readout: judging round (default: the writer's registered round)")
    a = p.parse_args()
    study = STUDIES[a.writer]
    {"leak-build": lambda: leak_build(a.round, study), "leak-collect": lambda: leak_collect(a.round, study),
     "rewrite-build": lambda: rewrite_build(study), "rewrite-collect": lambda: rewrite_collect(study),
     "contains-build": lambda: contains_build(study), "usage": lambda: usage(study),
     "readout": lambda: readout(a.name or study["round"], study)}[a.step]()


if __name__ == "__main__":
    main()
