"""Builds oracle-brief inputs and writes the oracle briefs (brief/PLAN.md).

Usage:
  python3 brief_oracle.py build     # -> $REVIEW_ARMS_DIR/briefs/oracle-work/, prints workflow args
  (run brief/workflow-oracle.js with the Workflow tool and those args; save its
   result as briefs/oracle-work/workflow-out.json)
  python3 brief_oracle.py collect   # checks; writes briefs/oracle-work/factcheck.jsonl
  (read factcheck.jsonl and write briefs/oracle-work/factcheck-decisions.json:
   {diff: {"decision": "carries" | "does not carry: <reason>", "brief_sha256": <from factcheck.jsonl>}}
   for every primary and secondary diff; a decision counts only for the brief whose hash it names)
  python3 brief_oracle.py collect   # again: publishes briefs/oracle/<diff>/brief.md

Primary and secondary diffs get an editor item: the blind brief and the census
record (fact, evidence, other facts, kind; never the census query, which was
written with knowledge of the defect). Control diffs get no editor: their
oracle brief is the blind brief. Every diff's oracle brief gets a leak check.

A brief is published (written where driver.py reads it) only when it passes
the lint and, for primary and secondary diffs, when the item that carries the
decisive fact (the editor's match, or its inserted item) appears verbatim in
the final brief and a person has recorded that it carries the census fact.
"""
import argparse
import hashlib
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


def carrier_item(r):
    """The brief item the editor says carries the decisive fact."""
    edit = r.get("edit") or {}
    return edit.get("inserted_item") if edit.get("contains") in ("partial", "no") else edit.get("matching_item")


def collect():
    W = ROOT / "briefs/oracle-work"
    out = json.loads((W / "workflow-out.json").read_text())
    assert not out["missing"], out["missing"]
    decisions_path = W / "factcheck-decisions.json"
    decisions = json.loads(decisions_path.read_text()) if decisions_path.exists() else {}
    rows, rejected, factcheck, undecided = [], [], [], []
    for r in out["results"]:
        bug = r["id"]
        d = ROOT / "briefs/oracle" / bug
        d.mkdir(parents=True, exist_ok=True)
        blind = ROOT / "briefs/blind" / bug / "brief.md"
        # A control brief that needed no rewrite comes back as null: it is the blind brief.
        final = (r["final_brief"] or blind.read_text()).rstrip() + "\n"
        inserted = [(r.get("edit") or {}).get("inserted_item")] if r["group"] == "S" else []
        problems = brief_driver.lint(final, driver.CORPUS[bug], in_diff_items=inserted)
        if r["group"] in "PS":
            item = carrier_item(r)
            norm = lambda t: " ".join((t or "").removeprefix("- ").split())
            if not item or norm(item) not in norm(final):
                problems.append(f"the item carrying the decisive fact is not in the final brief: {item!r}")
            item_file = json.loads((W / f"item-{bug}.json").read_text())
            digest = hashlib.sha256(final.encode()).hexdigest()
            factcheck.append({"id": bug, "group": r["group"], "census_fact": item_file["census"]["fact"],
                              "contains": r["edit"]["contains"], "carrier_item": item,
                              "rewritten": len(r["leak"]) > 1, "brief_sha256": digest, "final_brief": final})
            decision = decisions.get(bug)
            if decision is None or decision.get("brief_sha256") != digest:
                undecided.append(bug)  # never checked, or checked against a different brief
            elif decision["decision"] != "carries":
                problems.append(f"hand check: {decision['decision']}")
        save(d / "oracle.json", {**r, "lint": problems})
        rows.append({"id": bug, "group": r["group"], "contains": (r.get("edit") or {}).get("contains"),
                     "leak_first": r["leak"][0]["label"], "leak_final": r["leak"][-1]["label"],
                     "rewritten": len(r["leak"]) > 1, "problems": len(problems)})
        if problems or bug in undecided:
            # Only an accepted brief is written where driver.py reads it.
            (d / "brief-pending.md").write_text(final)
            (d / "brief.md").unlink(missing_ok=True)
            if problems:
                rejected.append(bug)
            continue
        (d / "brief-pending.md").unlink(missing_ok=True)
        if r["group"] == "C" and r["final_brief"]:
            # A control's oracle brief is its blind brief. A leak rewrite therefore replaces the
            # blind brief too (the writer's text is kept as brief-original.md), so both stay equal.
            original = blind.with_name("brief-original.md")
            if not original.exists():
                original.write_text(blind.read_text())
            blind.write_text(final)
        (d / "brief.md").write_text(final)
    (W / "factcheck.jsonl").write_text("".join(json.dumps(f) + "\n" for f in factcheck))
    for row in sorted(rows, key=lambda x: (x["group"], x["id"])):
        print(json.dumps(row))
    if rejected or undecided:
        raise SystemExit(f"not published: {len(rejected)} briefs with problems (oracle.json) {rejected}; "
                         f"{len(undecided)} awaiting a hand check in factcheck-decisions.json {undecided}")


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("step", choices=["build", "collect"])
    {"build": build, "collect": collect}[p.parse_args().step]()
