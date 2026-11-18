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

Set REVIEW_ARMS_SETS to select frozen P/S/C membership.
Leak/rewrite steps also accept --kind blind --bugs-from jobs.json --name <stage>.
Oracle edits (brief/PLAN-bifrost-oracle.md) use --kind oracle --bugs-from jobs-ot.json
--brief-root <dir with <bug>/candidate.md>: round 1 judges <bug>/final.md, verdict records go to
briefs/<stage>-leak-records/<bug>/blind-leak.json, and an accepted rewrite becomes final.md.
Use the same flags on every step. This selects only the jobs' known-bug diffs,
uses briefs/<stage>-leak-<round> and briefs/<stage>-rewrite, and validates saved
scope before consuming results. A still-leaking round 1 result is reported.

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
import os
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


def leak_dir(n, scope):
    return ROOT / "briefs" / f"{scope['name']}-leak-{n}"


def rewrite_dir(scope):
    return ROOT / "briefs" / f"{scope['name']}-rewrite"


def round_scope(kind, name, bugs_from, brief_root=None):
    if kind in ORACLE_KINDS:
        jobs = json.loads(bugs_from.read_text())
        ids = [j["bug"] for j in jobs]
        if not ids or len(ids) != len(set(ids)) or any(j.get("arm") != "OT" for j in jobs):
            raise SystemExit("oracle leak scope needs unique OT jobs")
        return {"kind": kind, "name": name or kind, "diffs": {b: "P" for b in ids},
                "brief_root": str(brief_root.resolve())}
    selected = diffs()
    if bugs_from is not None:
        bugs = {j["bug"] for j in json.loads(bugs_from.read_text())}
        if not bugs or bugs - selected.keys():
            raise SystemExit(f"empty or unknown leak subset: {sorted(bugs - selected.keys())}")
        selected = {b: g for b, g in selected.items() if b in bugs}
    return {"kind": kind, "name": name or kind, "diffs": selected}


# Leak scopes over candidate texts under --brief-root rather than a published brief kind.
# oracle: round 0 judges candidate.md, round 1 the accepted rewrite (final.md).
# oracle-noleak (brief/PLAN-bifrost-oracle-noleak.md): round N judges the fact-checked round-N.md.
ORACLE_KINDS = ("oracle", "oracle-noleak")


def brief_path(scope, bug, n=0):
    """The text a leak round judges: oracle round 1 judges the accepted rewrite where one exists."""
    if scope["kind"] == "oracle-noleak":
        return pathlib.Path(scope["brief_root"]) / bug / f"round-{n}.md"
    if scope["kind"] == "oracle":
        root = pathlib.Path(scope["brief_root"]) / bug
        return root / ("final.md" if n == 1 and (root / "final.md").exists() else "candidate.md")
    return out_dir(scope) / bug / "brief.md"


def record_path(scope, bug):
    if scope["kind"] in ORACLE_KINDS:
        return ROOT / "briefs" / f"{scope['name']}-leak-records" / bug / "blind-leak.json"
    return out_dir(scope) / bug / "blind-leak.json"


def check_scope(directory, scope, *, create=False, legacy=False):
    path = directory / "settings.json"
    if path.exists():
        if json.loads(path.read_text()) != scope:
            raise SystemExit(f"{directory}: leak/rewrite scope differs from saved settings")
    elif create:
        save(path, scope)
    elif not legacy:
        raise SystemExit(f"{directory}: missing saved leak/rewrite settings")
    # Original registered writer rounds predate this metadata.


def leak_build(n, scope):
    out = out_dir(scope)
    corpus = driver.CORPUS
    if scope["kind"] == "oracle-noleak":
        # The collected fact check decides the round's diffs; every accepted text must exist.
        results = pathlib.Path(scope["brief_root"]).parent / f"round-{n}" / "check-results.json"
        if not results.is_file():
            raise SystemExit(f"round {n} fact check is not collected: {results}")
        decisions = json.loads(results.read_text())["decisions"]
        bugs = [b for b in scope["diffs"] if (decisions.get(b) or {}).get("decision") == "carries"]
        absent = [b for b in bugs if not brief_path(scope, b, n).is_file()
                  or sha(brief_path(scope, b, n).read_text()) != decisions[b]["brief_sha256"]]
        if absent or not bugs:
            raise SystemExit(f"round {n}: accepted texts missing or not the checked bytes {absent}, or none accepted")
    elif n == 0:
        bugs = list(scope["diffs"])
    elif scope["kind"] == "oracle":
        bugs = []
        for b in scope["diffs"]:
            rec = record_path(scope, b)
            if rec.exists() and json.loads(rec.read_text())["rounds"][0]["label"] != "missed":
                if not (pathlib.Path(scope["brief_root"]) / b / "final.md").is_file():
                    raise SystemExit(f"{b}: wording rewrite is missing")
                bugs.append(b)
    else:
        bugs = [b for b in scope["diffs"] if (out / b / "brief-writer.md").exists()]
    missing = [] if scope["kind"] in ORACLE_KINDS else [b for b in bugs if not (out / b / "done.json").exists()]
    if missing:
        raise SystemExit(f"briefs not written yet: {missing}")
    d = leak_dir(n, scope)
    if (d / "key.json").exists():
        raise SystemExit(f"{d} already exists; its key binds verdicts to texts")
    check_scope(d, scope, create=True)
    batch, key = [], {}
    for b in bugs:
        brief = brief_path(scope, b, n).read_text()
        oid = secrets.token_hex(8)
        key[oid] = {"bug": b, "brief_sha256": sha(brief)}
        batch.append({"bug": b, **{k: corpus[b][k] for k in CORPUS_FIELDS},
                      "outputs": [{"output_id": oid, "text": brief}]})
    save(d / "batch-0.json", batch)
    save(d / "key.json", key)
    (d / "SPEC.md").write_text((SCRIPTS / "judge/SPEC.md").read_text())
    print(f"wrote {d} for {len(batch)} briefs")


def leak_collect(n, scope, *, legacy=False):
    d = leak_dir(n, scope)
    check_scope(d, scope, legacy=legacy)
    key = json.loads((d / "key.json").read_text())
    result = json.loads((d / "workflow-out.json").read_text())
    assert not result.get("missing"), result.get("missing")
    verdicts = result["verdicts"]
    judged = {}
    for v in verdicts:
        judged.setdefault(v["output_id"], []).append(v)
    assert set(judged) == set(key), "leak verdicts must cover exactly the round outputs"
    # Validate the whole round before updating any published decision.
    for oid, k in key.items():
        assert k["bug"] in scope["diffs"], f"{k['bug']}: leak output is outside the round's scope"
        vs = judged[oid]
        assert sorted(v["judge"] for v in vs) == ["A", "B"], (k["bug"], vs)
        assert all(v.get("bug") == k["bug"] and v["label"] in ("caught", "partial", "missed") for v in vs), "wrong bug or invalid leak label"
        assert sha(brief_path(scope, k["bug"], n).read_text()) == k["brief_sha256"], \
            f"{k['bug']}: brief changed since leak round {n} was built"
    rows = []
    for oid, k in key.items():
        b, vs = k["bug"], judged[oid]
        label = "missed" if all(v["label"] == "missed" for v in vs) else \
            "caught" if any(v["label"] == "caught" for v in vs) else "partial"
        path = record_path(scope, b)
        record = json.loads(path.read_text()) if path.exists() else {"id": b, "group": scope["diffs"][b], "rounds": []}
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


def rewrite_build(scope):
    base = rewrite_dir(scope)
    check_scope(base, scope, create=True)
    made = 0
    for b in scope["diffs"]:
        record = json.loads(record_path(scope, b).read_text())
        first = next(r for r in record["rounds"] if r["round"] == 0)
        if first["label"] == "missed" or (base / b).exists():
            continue
        u = base / b
        brief = brief_path(scope, b).read_text()
        assert sha(brief) == first["brief_sha256"], f"{b}: brief.md is not the text judged in round 0"
        (u / "brief-in.md").parent.mkdir(parents=True, exist_ok=True)
        (u / "brief-in.md").write_text(brief)
        (u / "WRITER.md").write_text((SCRIPTS / "brief/WRITER.md").read_text())
        reasons = "\n".join(f"- {v['rationale']}" for v in first["verdicts"] if v["label"] != "missed")
        (u / "INSTRUCTIONS.md").write_text(REWRITE.format(reasons=reasons))
        made += 1
    print(f"wrote {made} rewrite units under {base}")


def rewrite_collect(scope, *, legacy=False):
    briefs = out_dir(scope)
    base = rewrite_dir(scope)
    check_scope(base, scope, legacy=legacy)
    for u in sorted(p for p in base.iterdir() if p.is_dir() and p.name in scope["diffs"]):
        b, d = u.name, briefs / u.name
        out = u / "brief.md"
        if not out.exists():
            print(json.dumps({"bug": b, "state": "no rewrite fetched"}))
            continue
        text = out.read_text()
        if scope["kind"] == "oracle":
            text = text.rstrip() + "\n"
        original = (u / "brief-in.md").read_text()
        record = json.loads(record_path(scope, b).read_text())
        first = next(r for r in record["rounds"] if r["round"] == 0)
        assert sha(original) == first["brief_sha256"], f"{b}: rewrite input is not the round0 judged text"
        current = brief_path(scope, b).read_text()
        assert current in (original, text), f"{b}: brief changed since rewrite was prepared"
        problems = brief_driver.lint(text, driver.CORPUS[b])
        if problems:
            print(json.dumps({"bug": b, "state": "rewrite fails lint", "problems": problems}))
            continue
        if scope["kind"] == "oracle":
            final_path = pathlib.Path(scope["brief_root"]) / b / "final.md"
            if final_path.exists() and final_path.read_text() != text:
                raise SystemExit(f"{b}: refusing to replace a different accepted rewrite")
            final_path.write_text(text)
            print(json.dumps({"bug": b, "state": "candidate rewritten", "brief_sha256": sha(text)}))
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


def compare_same_sessions(round_name, reference_round, bugs, arms=("A", "BT"), model=REVIEW_MODEL):
    """Compare final labels for identical (bug, condition, session) observations across rounds."""
    conditions = {"A": "no-brief", "BT": "blind", "OT": "oracle"}
    if not bugs or not arms or set(arms) - set(conditions):
        raise SystemExit(f"same-session comparison needs bugs and arms among {sorted(conditions)}")
    rounds = (round_name, reference_round)
    selected, finals = {}, {}
    for name in rounds:
        key, finals[name] = judged_round(name)
        cells = {}
        for oid, k in key.items():
            if k["model"] != model or k["bug"] not in bugs or k["arm"] not in arms:
                continue
            arm = k["arm"]
            if k["condition"] != conditions[arm]:
                raise SystemExit(f"{name}: wrong condition for {arm}: {oid}")
            identity = (k["bug"], arm, k["session"])
            if identity in cells:
                raise SystemExit(f"{name}: duplicate same-session cell: {identity}")
            cells[identity] = oid
        selected[name] = cells
        for bug in bugs:
            for arm in arms:
                count = sum(identity[:2] == (bug, arm) for identity in cells)
                if count != 3:
                    raise SystemExit(f"{name}: expected exactly 3 {arm} sessions for {bug}, found {count}")
    current, reference = (selected[name] for name in rounds)
    if current.keys() != reference.keys():
        raise SystemExit("same-session comparison needs identical keys; "
                         f"only {round_name}: {sorted(current.keys() - reference.keys())}; "
                         f"only {reference_round}: {sorted(reference.keys() - current.keys())}")
    per_diff = []
    for bug in bugs:
        row = {"bug": bug, "arms": {}}
        for arm in arms:
            ids = sorted(i for i in current if i[:2] == (bug, arm))
            if not ids:
                raise SystemExit(f"{bug}: no same-session rows for {arm}")
            labels = [{"session": i[2], "round": finals[round_name][current[i]],
                       "reference": finals[reference_round][reference[i]]} for i in ids]
            row["arms"][arm] = {"sessions": len(ids), "labels": labels,
                                "catch_rate": {name: statistics.fmean(
                                    (r["round"] if name == round_name else r["reference"]) == "caught"
                                    for r in labels) for name in rounds}}
        per_diff.append(row)
    summary, caveats = {}, []
    for arm in arms:
        n = sum(r["arms"][arm]["sessions"] for r in per_diff)
        # Exact rational comparison keeps the registered strict >10-point boundary exact.
        delta = sum((Fraction(sum(x["round"] == "caught" for x in r["arms"][arm]["labels"]),
                               r["arms"][arm]["sessions"])
                     - Fraction(sum(x["reference"] == "caught" for x in r["arms"][arm]["labels"]),
                                r["arms"][arm]["sessions"])) for r in per_diff) / len(bugs)
        changed = sum(x["round"] != x["reference"] for r in per_diff for x in r["arms"][arm]["labels"])
        summary[arm] = {"sessions": n, "difference_points": float(100 * delta),
                        "changed_labels": changed, "differs_over_10_points": abs(delta) > Fraction(1, 10)}
        if summary[arm]["differs_over_10_points"]:
            caveats.append(f"{arm}: {round_name} vs {reference_round} differs by {float(100 * delta):.1f} points")
    return {"reference_round": reference_round, "reviewer_model": model, "diffs": len(bugs),
            "arms": summary, "caveats": caveats, "per_diff": per_diff}


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
    p.add_argument("--round", type=int, choices=[0, 1, 2, 3], default=0,
                   help="leak round; 2 and 3 exist only for --kind oracle-noleak, which starts at 1")
    p.add_argument("--name", help="readout judging round, or path prefix for leak/rewrite")
    p.add_argument("--kind", choices=("blind", *ORACLE_KINDS, *(s["kind"] for s in STUDIES.values())),
                   help="leak/rewrite brief kind (default: the selected writer's kind)")
    p.add_argument("--bugs-from", type=pathlib.Path, help="leak/rewrite jobs whose diffs are the explicit subset")
    p.add_argument("--brief-root", type=pathlib.Path, help="oracle leak/rewrite: root containing <bug>/candidate.md")
    a = p.parse_args()
    study = STUDIES[a.writer]
    leak_steps = ("leak-build", "leak-collect", "rewrite-build", "rewrite-collect")
    if (a.kind or a.bugs_from or a.brief_root) and a.step not in leak_steps:
        p.error("--kind, --bugs-from and --brief-root apply only to leak/rewrite steps")
    if a.kind in ORACLE_KINDS and (a.brief_root is None or a.bugs_from is None):
        p.error(f"--kind {a.kind} requires --brief-root and --bugs-from")
    if (a.kind == "oracle-noleak") != (a.round >= 2) and not (a.kind == "oracle-noleak" and a.round == 1):
        p.error("rounds 2-3 apply only to --kind oracle-noleak, whose rounds are 1-3")
    if a.kind == "oracle-noleak" and a.step not in ("leak-build", "leak-collect"):
        p.error("--kind oracle-noleak has no rewrite step; edits run through brief_oracle.py noleak steps")
    if a.step in leak_steps and a.name is not None and (
            pathlib.Path(a.name).name != a.name or a.name in ("", ".", "..")):
        p.error("--name must be a single round path component")

    if a.step in leak_steps:
        kind = a.kind or study["kind"]
        scope = round_scope(kind, a.name, a.bugs_from, a.brief_root)
        legacy = (kind == study["kind"] and a.name is None and a.bugs_from is None
                  and not os.environ.get("REVIEW_ARMS_SETS"))
        {"leak-build": lambda: leak_build(a.round, scope),
         "leak-collect": lambda: leak_collect(a.round, scope, legacy=legacy),
         "rewrite-build": lambda: rewrite_build(scope),
         "rewrite-collect": lambda: rewrite_collect(scope, legacy=legacy)}[a.step]()
    else:
        {"contains-build": lambda: contains_build(study), "usage": lambda: usage(study),
         "readout": lambda: readout(a.name or study["round"], study)}[a.step]()


if __name__ == "__main__":
    main()
