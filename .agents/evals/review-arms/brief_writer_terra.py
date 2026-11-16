"""Checks for the Terra-written blind briefs (brief/PLAN-writer-terra.md).

The writer is brief_driver.py --kind blind-terra. Before any TT reviewer reads a
brief, each brief gets stage B's leak check: a known-bug judge (judge/SPEC.md)
reads the brief as a review output and must answer `missed`; a brief judged
caught or partial is rewritten once to remove consequence wording only (the
rewrite prompt of brief/workflow-oracle.js) and judged again. Stage B used one
Opus/high judge; here both judges of `verify_driver.py --kind judge` read it, and
a brief leaks when either says caught or partial.

Usage:
  python3 brief_writer_terra.py leak-build [--round 0|1]
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
  python3 brief_writer_terra.py usage
      writer cost, tokens, turns and wall time per brief for both writers
      -> results/brief-writer-terra/usage.jsonl and a summary
"""
import argparse
import hashlib
import json
import pathlib
import secrets
import statistics

import brief_driver
import brief_oracle
import driver
from driver import ROOT, SCRIPTS, save

KIND = "blind-terra"
OUT = ROOT / "briefs" / KIND
CORPUS_FIELDS = ("defect", "location", "trigger", "match_rule", "near_misses", "why_findable",
                 "intro_commit", "fix_commit")


def sha(text):
    return hashlib.sha256(text.encode()).hexdigest()


def diffs():
    s = brief_oracle.sets()
    return {b: g for g in ("P", "S") for b in s[g]}


def leak_dir(n):
    return ROOT / "briefs" / f"{KIND}-leak-{n}"


def leak_build(n):
    corpus = {r["id"]: r for r in map(json.loads, open(SCRIPTS / "corpus/corpus.jsonl"))}
    if n == 0:
        bugs = list(diffs())
    else:
        bugs = [b for b in diffs() if (OUT / b / "brief-writer.md").exists()]
    missing = [b for b in bugs if not (OUT / b / "done.json").exists()]
    if missing:
        raise SystemExit(f"briefs not written yet: {missing}")
    d = leak_dir(n)
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


def leak_collect(n):
    d = leak_dir(n)
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


def rewrite_build():
    base = ROOT / "briefs" / f"{KIND}-rewrite"
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


def rewrite_collect():
    base = ROOT / "briefs" / f"{KIND}-rewrite"
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


def contains_build():
    base = ROOT / "briefs" / "writer-contains"
    unit = base / "unit"
    if (base / "key.json").exists():
        raise SystemExit(f"{base} already exists")
    unit.mkdir(parents=True)
    (unit / "CONTAINS.md").write_text((ROOT / "briefs/bifrost-contains/CONTAINS.md").read_text())
    key, ids = {}, []
    for b, group in diffs().items():
        census = json.loads((ROOT / f"briefs/bifrost-contains/item-{b}.json").read_text())["census"]
        for kind in ("blind", KIND):
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


def usage():
    rows = []
    for b, group in diffs().items():
        for kind in ("blind", KIND):
            d = ROOT / "briefs" / kind / b
            if not (d / "done.json").exists():
                rows.append({"bug": b, "group": group, "kind": kind, "state": "not written"})
                continue
            [u] = [json.loads(p.read_text()) for p in (d / "reviewers").glob("*/usage.json")]
            intent = json.loads((d / "launch-intent.json").read_text())
            end = max(t["completed_at_ms"] for t in u["turns"])
            rows.append({"bug": b, "group": group, "kind": kind, "model": intent["model"],
                         "effort": intent["effort"], "turns": len(u["turns"]),
                         "fix_rounds": json.loads((d / "done.json").read_text())["fix_rounds"],
                         "cost_usd": u["turn_cost_usd_ticks"] / 1e10 if u.get("turn_cost_usd_ticks") is not None
                         else None,
                         "cost_turns_estimated": u["coverage"]["estimated_cost_turns"],
                         "cost_turns_missing": u["coverage"]["missing_cost_turns"],
                         "total_tokens": u["totals"]["total_tokens"]["tokens"],
                         # From launch intent (before mj new) to the end of the writer's last turn.
                         "wall_s": round(end / 1000 - intent["time_ns"] / 1e9, 1)})
    save_jsonl(SCRIPTS / "results/brief-writer-terra/usage.jsonl", rows)
    for kind in ("blind", KIND):
        rs = [r for r in rows if r["kind"] == kind and r.get("turns")]
        if not rs:
            continue
        summary = {k: statistics.median(r[k] for r in rs if r[k] is not None)
                   for k in ("cost_usd", "total_tokens", "wall_s", "turns")}
        print(json.dumps({"kind": kind, "briefs": len(rs), "median": summary,
                          "total_cost_usd": round(sum(r["cost_usd"] or 0 for r in rs), 2),
                          "unknown_cost": sum(r["cost_usd"] is None for r in rs)}))


def save_jsonl(path, rows):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(r) + "\n" for r in rows))


def main():
    p = argparse.ArgumentParser()
    p.add_argument("step", choices=["leak-build", "leak-collect", "rewrite-build", "rewrite-collect",
                                    "contains-build", "usage"])
    p.add_argument("--round", type=int, choices=[0, 1], default=0)
    a = p.parse_args()
    {"leak-build": lambda: leak_build(a.round), "leak-collect": lambda: leak_collect(a.round),
     "rewrite-build": rewrite_build, "rewrite-collect": rewrite_collect,
     "contains-build": contains_build, "usage": usage}[a.step]()


if __name__ == "__main__":
    main()
