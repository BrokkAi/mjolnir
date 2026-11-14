"""Builds oracle-brief inputs and writes the oracle briefs (brief/PLAN.md).

Usage:
  python3 brief_oracle.py build     # -> $REVIEW_ARMS_DIR/briefs/oracle-work/, prints workflow args
  (run brief/workflow-oracle.js with the Workflow tool and those args; save its
   result as briefs/oracle-work/workflow-out.json)
  python3 brief_oracle.py collect   # -> briefs/oracle/<diff>/brief.md, lint, summary

Primary and secondary diffs get an editor item: the blind brief and the census
record (fact, evidence, other facts, kind; never the census query, which was
written with knowledge of the defect). Control diffs get no editor: their
oracle brief is the blind brief. Every diff's oracle brief gets a leak check.
"""
import argparse
import json
import pathlib
import re

import brief_driver
import driver
from driver import ROOT, SCRIPTS, save

PLAN = (SCRIPTS / "brief/PLAN.md").read_text()


def frozen(name):
    """Hard-won diff ids listed in brief/PLAN.md under the bullet that starts with **name**."""
    block = re.search(r"\n- \*\*" + re.escape(name) + r"\*\*(.*?)(?=\n- \*\*|\n## )", PLAN, re.S).group(1)
    hard_won = {json.loads(l)["id"] for l in open(SCRIPTS / "corpus/corpus.jsonl")}
    return [w for w in re.findall(r"[a-z0-9]+(?:-[a-z0-9]+)+", block) if w in hard_won]


def sets():
    s = {"P": frozen("Primary (P, 18):"), "S": frozen("Secondary (S, 4):"), "C": frozen("Control (C, 8):")}
    assert [len(s[k]) for k in "PSC"] == [18, 4, 8], {k: len(v) for k, v in s.items()}
    return s


def build():
    s = sets()
    first = {json.loads(l)["id"]: json.loads(l) for l in open(SCRIPTS / "results/missing-facts.jsonl")}
    second = {json.loads(l)["id"]: json.loads(l) for l in open(SCRIPTS / "results/missing-facts-pass2.jsonl")}
    corpus = {json.loads(l)["id"]: json.loads(l) for l in open(SCRIPTS / "corpus/corpus.jsonl")}
    W = ROOT / "briefs/oracle-work"
    W.mkdir(parents=True, exist_ok=True)
    (W / "ORACLE.md").write_text((SCRIPTS / "brief/ORACLE.md").read_text())
    (W / "WRITER.md").write_text((SCRIPTS / "brief/WRITER.md").read_text())
    (W / "JUDGE.md").write_text((SCRIPTS / "judge/SPEC.md").read_text())
    diffs = []
    for group in "PSC":
        for bug in s[group]:
            blind = (ROOT / "briefs/blind" / bug / "brief.md").read_text()
            rec = second[bug] if group == "S" else first[bug]
            item = {"id": bug, "group": group, "intro": corpus[bug]["intro_commit"], "base": corpus[bug]["base_commit"],
                    "blind_brief": blind}
            if group in "PS":
                item["census"] = {"fact": rec["fact"], "evidence": rec["evidence"],
                                  "other_facts": rec.get("other_facts", []), "kind": rec["kind"],
                                  "allow_in_diff": group == "S"}
            save(W / f"item-{bug}.json", item)
            c = corpus[bug]
            save(W / f"record-{bug}.json", {k: c[k] for k in ("id", "defect", "location", "trigger", "match_rule",
                                                             "near_misses", "why_findable", "intro_commit",
                                                             "fix_commit")})
            diffs.append({"id": bug, "group": group})
    print(json.dumps({"dir": str(W), "diffs": diffs}))


def collect():
    out = json.loads((ROOT / "briefs/oracle-work/workflow-out.json").read_text())
    assert not out["missing"], out["missing"]
    rows = []
    for r in out["results"]:
        bug = r["id"]
        d = ROOT / "briefs/oracle" / bug
        d.mkdir(parents=True, exist_ok=True)
        blind = ROOT / "briefs/blind" / bug / "brief.md"
        if r["group"] == "C" and r["final_brief"]:
            # A control's oracle brief is its blind brief. A leak rewrite therefore replaces the
            # blind brief too (the writer's text is kept as brief-original.md), so both stay equal.
            original = blind.with_name("brief-original.md")
            if not original.exists():
                original.write_text(blind.read_text())
            blind.write_text(r["final_brief"].rstrip() + "\n")
        # A control brief that needed no rewrite comes back as null: it is the blind brief.
        final = r["final_brief"] or blind.read_text()
        (d / "brief.md").write_text(final.rstrip() + "\n")
        inserted = [(r.get("edit") or {}).get("inserted_item")] if r["group"] == "S" else []
        problems = brief_driver.lint(final, driver.CORPUS[bug], in_diff_items=inserted)
        save(d / "oracle.json", {**r, "lint": problems})
        rows.append({"id": bug, "group": r["group"], "contains": (r.get("edit") or {}).get("contains"),
                     "leak_first": r["leak"][0]["label"], "leak_final": r["leak"][-1]["label"],
                     "rewritten": len(r["leak"]) > 1, "lint": len(problems)})
    for row in sorted(rows, key=lambda x: (x["group"], x["id"])):
        print(json.dumps(row))


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("step", choices=["build", "collect"])
    {"build": build, "collect": collect}[p.parse_args().step]()
