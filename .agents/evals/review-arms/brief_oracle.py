"""Builds oracle-brief inputs and writes the oracle briefs (brief/PLAN.md).

Usage:
  python3 brief_oracle.py build     # -> $REVIEW_ARMS_DIR/briefs/oracle-work/, prints workflow args
  (run brief/workflow-oracle.js with the Workflow tool and those args; save its
   result as briefs/oracle-work/workflow-out.json)
  python3 brief_oracle.py collect   # checks; writes briefs/oracle-work/factcheck.jsonl
  (read factcheck.jsonl and write briefs/oracle-work/factcheck-decisions.json:
   {diff: {"decision": "carries" | "does not carry: <reason>", "brief_sha256": <from factcheck.jsonl>,
           "item": <the published brief's item that carries the fact, quoted exactly>}}
   for every primary and secondary diff; a decision counts only for the brief whose hash it names)
  python3 brief_oracle.py collect   # again: publishes briefs/oracle/<diff>/brief.md
  python3 brief_oracle.py blind-collect <blind-leak-out.json>
      stage B: publishes leak-rewritten blind briefs of primary and secondary diffs (run the
      workflow with blind_only: true on them); the writer's text is kept as brief-original.md
  python3 brief_oracle.py annotate <out.json> --run-started <ISO time>
      records judged_blind_sha256 for a result saved before the workflow returned it, only if
      every item file it read predates the run
  python3 brief_oracle.py merge <rerun-out.json>
      replaces the named diffs' results in workflow-out.json with a rerun of the workflow on them
      (for a brief whose hand check failed); every published brief keeps its own leak verdict

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


def sha(text):
    return hashlib.sha256(text.strip().encode()).hexdigest()


def judged_matches(r, d):
    """The verdict in r was produced for the blind brief now published in d, or r is already applied.

    The workflow returns judged_blind_sha256, the hash build gave it for the text the judge read.
    A result already applied left the writer's text in brief-original.md and its rewrite in brief.md.
    """
    judged = r.get("judged_blind_sha256")
    current = (d / "brief.md").read_text()
    if judged is None:
        return False
    if judged == sha(current):
        return True
    original = d / "brief-original.md"
    return bool(r["final_brief"]) and original.exists() and judged == sha(original.read_text()) \
        and sha(r["final_brief"]) == sha(current)


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
            diffs.append({"id": bug, "group": group, "blind_sha256": sha(blind)})
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
    # The group decides whether a diff needs the fact check; take it from PLAN.md, not the run.
    group_of = {bug: g for g, bugs in sets().items() for bug in bugs}
    mislabeled = [r["id"] for r in out["results"] if group_of.get(r["id"]) != r["group"]]
    assert not mislabeled, f"results whose group differs from brief/PLAN.md: {mislabeled}"
    assert sorted(r["id"] for r in out["results"]) == sorted(group_of), "results must cover the 30 frozen diffs"
    for r in out["results"]:
        bug = r["id"]
        d = ROOT / "briefs/oracle" / bug
        d.mkdir(parents=True, exist_ok=True)
        blind = ROOT / "briefs/blind" / bug / "brief.md"
        if r["group"] == "C" and not judged_matches(r, blind.parent):
            # Its leak verdict was produced for other text than the blind brief: unpublish.
            (d / "brief.md").unlink(missing_ok=True)
            rejected.append(bug)
            continue
        # A control brief that needed no rewrite comes back as null: it is the blind brief.
        final = (r["final_brief"] or blind.read_text()).rstrip() + "\n"
        # A secondary oracle item may relate two changed locations: the editor's item, or the
        # hand-checked item it became after a leak rewrite.
        inserted = ([(r.get("edit") or {}).get("inserted_item"), (decisions.get(bug) or {}).get("item")]
                    if r["group"] == "S" else [])
        problems = brief_driver.lint(final, driver.CORPUS[bug], in_diff_items=inserted)
        if r["group"] in "PS":
            norm = lambda t: " ".join((t or "").removeprefix("- ").split())
            item_file = json.loads((W / f"item-{bug}.json").read_text())
            digest = hashlib.sha256(final.encode()).hexdigest()
            decision = decisions.get(bug)
            factcheck.append({"id": bug, "group": r["group"], "census_fact": item_file["census"]["fact"],
                              "contains": r["edit"]["contains"], "carrier_item": carrier_item(r),
                              "rewritten": len(r["leak"]) > 1, "brief_sha256": digest, "final_brief": final})
            if decision is None or decision.get("brief_sha256") != digest:
                undecided.append(bug)  # never checked, or checked against a different brief
            elif decision["decision"] != "carries":
                problems.append(f"hand check: {decision['decision']}")
            else:
                # The checker quotes the final brief's carrying item (a leak rewrite rewords the
                # editor's item); without a quote, the editor's item must still be there verbatim.
                item = decision.get("item") or carrier_item(r)
                if not item or norm(item) not in norm(final):
                    problems.append(f"the item carrying the decisive fact is not in the final brief: {item!r}")
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


def merge(path):
    W = ROOT / "briefs/oracle-work"
    out = json.loads((W / "workflow-out.json").read_text())
    rerun = json.loads(pathlib.Path(path).read_text())
    assert not rerun["missing"], rerun["missing"]
    replaced = {r["id"]: r for r in rerun["results"]}
    previous = [r for r in out["results"] if r["id"] in replaced]
    assert len(previous) == len(replaced), "a rerun may only replace diffs already in workflow-out.json"
    wrong = [r["id"] for r in previous if replaced[r["id"]]["group"] != r["group"]]
    assert not wrong, f"a rerun must keep each diff's group: {wrong}"
    history = W / "replaced-results.jsonl"
    with history.open("a") as f:
        for r in previous:
            f.write(json.dumps(r) + "\n")
    out["results"] = [replaced.get(r["id"], r) for r in out["results"]]
    save(W / "workflow-out.json", out)
    print(json.dumps({"replaced": sorted(replaced), "previous_kept_in": str(history)}))


def blind_collect(path):
    out = json.loads(pathlib.Path(path).read_text())
    assert not out["missing"], out["missing"]
    s = sets()
    # Every P and S blind brief needs the check before stage B; a subset run is not completion.
    assert sorted(r["id"] for r in out["results"]) == sorted(s["P"] + s["S"]), \
        "blind leak check must cover exactly the primary and secondary diffs"
    rejected, rows = [], []
    for r in out["results"]:
        bug = r["id"]
        assert r["edit"] is None, f"{bug}: blind leak check runs without the editor"
        d = ROOT / "briefs/blind" / bug
        # The verdict holds only for the text it judged (hash returned by the workflow).
        if not judged_matches(r, d):
            rejected.append((bug, ["the judged brief is not the published one; rerun build and the check"]))
            continue
        save(d / "blind-leak.json", r)
        rows.append({"id": bug, "leak_first": r["leak"][0]["label"], "leak_final": r["leak"][-1]["label"],
                     "rewritten": r["final_brief"] is not None})
        if r["final_brief"] is None:
            continue
        final = r["final_brief"].rstrip() + "\n"
        problems = brief_driver.lint(final, driver.CORPUS[bug])
        if problems:
            (d / "brief-rejected-rewrite.md").write_text(final)
            rejected.append((bug, problems))
            continue
        original = d / "brief-original.md"
        if not original.exists():
            original.write_text((d / "brief.md").read_text())
        (d / "brief.md").write_text(final)
    for row in rows:
        print(json.dumps(row))
    if rejected:
        raise SystemExit(f"rewrites that break the rules were not published: {rejected}")


def annotate(path, run_started):
    """Adds judged_blind_sha256 to a result saved before the workflow returned it.

    Valid only when every item file the run read is older than the run, so the
    judge saw the current item text; refuses otherwise. The evidence is kept.
    """
    import datetime
    start = datetime.datetime.fromisoformat(run_started).timestamp()
    W = ROOT / "briefs/oracle-work"
    out = json.loads(pathlib.Path(path).read_text())
    for r in out["results"]:
        if r.get("judged_blind_sha256") or r.get("final_brief") and r.get("edit"):
            continue  # already bound, or an editor result judged inline
        item = W / f"item-{r['id']}.json"
        text = json.loads(item.read_text())["blind_brief"]
        when = lambda f: datetime.datetime.fromtimestamp(f.stat().st_mtime).isoformat()
        # Only an item file older than the run shows what the judge read; a rebuilt item does not.
        if item.stat().st_mtime < start:
            evidence = f"item file unchanged since {when(item)}"
        else:
            raise SystemExit(f"{r['id']}: no evidence the judge read the current text; rerun the check")
        r["judged_blind_sha256"] = sha(text)
        r["judged_hash_source"] = f"{evidence}, before the run started at {run_started}"
    save(pathlib.Path(path), out)


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("step", choices=["build", "collect", "merge", "blind-collect", "annotate"])
    p.add_argument("path", nargs="?")
    p.add_argument("--run-started", help="annotate: ISO time the workflow run started")
    a = p.parse_args()
    if a.step == "merge":
        merge(a.path)
    elif a.step == "blind-collect":
        blind_collect(a.path)
    elif a.step == "annotate":
        annotate(a.path, a.run_started)
    else:
        {"build": build, "collect": collect}[a.step]()
