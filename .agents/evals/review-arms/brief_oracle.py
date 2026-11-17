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
import os
import pathlib
import re

import brief_driver
import driver
from driver import ROOT, SCRIPTS, save

PLAN = (SCRIPTS / "brief/PLAN.md").read_text()
TASK_CENSUS = None
TASK_JOBS = None
TASK_NAME = None
TASK_SCOPE = None


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
    manifest = os.environ.get("REVIEW_ARMS_SETS")
    if manifest:
        s = json.loads(pathlib.Path(manifest).read_text())
        if not isinstance(s, dict) or set(s) != set("PSC") or any(
                not isinstance(s[g], list) or any(not isinstance(b, str) for b in s[g]) for g in "PSC"):
            raise SystemExit("REVIEW_ARMS_SETS must contain exactly P/S/C arrays of IDs")
        ids = [b for g in "PSC" for b in s[g]]
        known = {json.loads(line)["id"] for line in driver.CORPUS_PATH.read_text().splitlines() if line.strip()}
        if not ids or len(ids) != len(set(ids)) or set(ids) - known:
            raise SystemExit("REVIEW_ARMS_SETS has empty, duplicate/overlapping or unknown IDs: "
                             f"{sorted(set(ids) - known)}")
        return s
    s = {"P": frozen("Primary (P, 18):"), "S": frozen("Secondary (S, 4):"), "C": frozen("Control (C, 8):")}
    assert [len(s[k]) for k in "PSC"] == [18, 4, 8], {k: len(v) for k, v in s.items()}
    return s


def task_work():
    return ROOT / "briefs" / (f"{TASK_NAME}-work" if TASK_NAME else "oracle-work")


def task_scope():
    return {"census": str(pathlib.Path(TASK_CENSUS).resolve()),
            "census_sha256": hashlib.sha256(pathlib.Path(TASK_CENSUS).read_bytes()).hexdigest(),
            "jobs": str(pathlib.Path(TASK_JOBS).resolve()),
            "jobs_sha256": hashlib.sha256(pathlib.Path(TASK_JOBS).read_bytes()).hexdigest(), "name": TASK_NAME}


def load_task_inputs():
    with open(TASK_CENSUS) as f:
        census_rows = [json.loads(line) for line in f if line.strip()]
    census = {row["id"]: row for row in census_rows}
    if len(census) != len(census_rows):
        raise SystemExit("census contains duplicate IDs")
    jobs = json.loads(pathlib.Path(TASK_JOBS).read_text())
    ids = [j["bug"] for j in jobs]
    if not ids or len(ids) != len(set(ids)) or any(j.get("arm") != "OT" for j in jobs):
        raise SystemExit("OT jobs must contain unique bug IDs and arm OT")
    return census, jobs


def remove_item_line(text, item, bug):
    """Remove the item, optionally its added blank separator; retain existing separators too."""
    lines = text.splitlines(keepends=True)
    matches = [i for i, line in enumerate(lines) if line.rstrip("\r\n") == item]
    if len(matches) != 1:
        raise SystemExit(f"{bug}: declared item must match exactly one complete brief line")
    index = matches[0]
    del lines[index]
    variants = {"".join(lines)}
    for neighbor in (index - 1, index):
        if 0 <= neighbor < len(lines) and lines[neighbor] in ("\n", "\r\n"):
            variants.add("".join(lines[:neighbor] + lines[neighbor + 1:]))
    return variants


def edit_build():
    census_rows, jobs = load_task_inputs()
    corpus = driver.CORPUS
    W = task_work()
    scope_path = W / "edit" / "settings.json"
    scope_path.parent.mkdir(parents=True, exist_ok=True)
    scope = task_scope()
    if scope_path.exists() and json.loads(scope_path.read_text()) != scope:
        raise SystemExit(f"{scope_path}: edit scope differs from saved settings")
    if not scope_path.exists():
        save(scope_path, scope)
    (W / "ORACLE.md").write_text((SCRIPTS / "brief/ORACLE.md").read_text())
    (W / "WRITER.md").write_text((SCRIPTS / "brief/WRITER.md").read_text())
    made = []
    for job in jobs:
        bug = job["bug"]
        if bug not in corpus or bug not in census_rows:
            raise SystemExit(f"unknown OT bug/census ID: {bug}")
        unit = W / "edit" / bug
        blind = ROOT / "briefs" / "blind" / bug / "brief.md"
        census = census_rows[bug]
        if not blind.is_file():
            raise SystemExit(f"missing final published BT brief: {blind}")
        item = {"id": bug, "group": "P", "intro": corpus[bug]["intro_commit"],
                "base": corpus[bug]["base_commit"], "blind_brief": blind.read_text(),
                "census": {k: census[k] for k in ("fact", "evidence", "other_facts", "kind")}}
        if unit.exists():
            raise SystemExit(f"{unit} exists; refusing to overwrite an editor unit")
        unit.mkdir()
        save(unit / "item.json", item)
        (unit / "ORACLE.md").write_text((SCRIPTS / "brief/ORACLE.md").read_text())
        (unit / "WRITER.md").write_text((SCRIPTS / "brief/WRITER.md").read_text())
        (unit / "INSTRUCTIONS.md").write_text(
            "Read ORACLE.md and WRITER.md and follow them exactly. Your item is item.json. "
            "Read code only through git show or git diff at the item's intro and base commits; "
            "do not read code from the current checkout or other history. Do not read any other "
            "experiment data or history, use the network, build, test, or edit tracked files. "
            "Replace StructuredOutput delivery with writing the specified JSON object to edit.json in "
            "the repository working directory, not the uploaded-input folder. "
            "Do not edit any other file; preserve every unaffected item and make only the one registered edit.")
        made.append({"id": bug, "blind_sha256": sha(item["blind_brief"])})
    print(json.dumps({"dir": str(W / "edit"), "diffs": made}))


def edit_collect():
    W = task_work()
    census_rows, jobs = load_task_inputs()
    ids = [j["bug"] for j in jobs]
    scope = task_scope()
    scope_path = W / "edit" / "settings.json"
    if not scope_path.is_file() or json.loads(scope_path.read_text()) != scope:
        raise SystemExit("editor scope does not match the saved jobs/census/name")
    candidates = W / "candidates"
    candidates.mkdir(parents=True, exist_ok=True)
    recs = []
    for bug in ids:
        unit = W / "edit" / bug
        item = json.loads((unit / "item.json").read_text())
        raw = json.loads((unit / "edit.json").read_text())
        required = {"id", "contains", "matching_item", "inserted_item", "replaced_item", "oracle_brief", "rationale"}
        if not isinstance(raw, dict) or set(raw) != required or raw["id"] != bug or raw["contains"] not in ("yes", "partial", "no"):
            raise SystemExit(f"{bug}: editor output schema/id invalid")
        if not isinstance(raw["oracle_brief"], str) or not isinstance(raw["rationale"], str) or \
                any(raw[k] is not None and not isinstance(raw[k], str)
                    for k in ("matching_item", "inserted_item", "replaced_item")):
            raise SystemExit(f"{bug}: editor output fields have invalid types")
        original = item["blind_brief"]
        result = raw["oracle_brief"].rstrip() + "\n"
        if raw["contains"] == "yes":
            if raw["oracle_brief"] != original or not raw["matching_item"] or raw["matching_item"] not in original or \
                    raw["inserted_item"] is not None or raw["replaced_item"] is not None:
                raise SystemExit(f"{bug}: yes must preserve the original brief exactly")
            carrier = raw["matching_item"]
        else:
            carrier = raw["inserted_item"]
            if not carrier or carrier not in result:
                raise SystemExit(f"{bug}: inserted carrying item is absent literally from candidate")
            # Remove exactly the declared replacement/insertion and require the remainder to be identical.
            if raw["contains"] == "partial":
                old = raw["replaced_item"]
                if not raw["matching_item"] or old != raw["matching_item"] or not old or old not in original:
                    raise SystemExit(f"{bug}: partial edit must name an original item")
                expected = remove_item_line(original, old, bug)
            else:
                if raw["matching_item"] is not None:
                    raise SystemExit(f"{bug}: no edit cannot name a matching item")
                if raw["replaced_item"]:
                    items = [line.strip()[2:] for line in original.splitlines() if line.strip().startswith("- ")]
                    if sum(item.lower() != "none" for item in items) != brief_driver.MAX_ITEMS:
                        raise SystemExit(f"{bug}: no edit may replace an item only when the brief already has 25 items")
                expected = remove_item_line(original, raw["replaced_item"], bug) if raw["replaced_item"] else {original}
            remainder = remove_item_line(result, carrier, bug)
            if not remainder.intersection(expected):
                raise SystemExit(f"{bug}: edit changed text beyond its single carrying item")
        problems = brief_driver.lint(result, driver.CORPUS[bug])
        if problems:
            raise SystemExit(f"{bug}: editor candidate fails lint: {problems}")
        candidate = candidates / bug
        candidate.mkdir(exist_ok=False)
        (candidate / "candidate.md").write_text(result)
        save(candidate / "edit.json", raw)
        save(candidate / "item.json", item)
        recs.append({"id": bug, "contains": raw["contains"], "carrier_item": carrier,
                     "candidate_sha256": hashlib.sha256(result.encode()).hexdigest()})
    save(W / "edit-results.json", {"scope": scope, "results": recs})
    print(json.dumps({"candidates": recs}))


def task_sync():
    W = task_work()
    result = json.loads((W / "edit-results.json").read_text())
    if result["scope"] != task_scope():
        raise SystemExit("editor result scope mismatch")
    leak_root = ROOT / "briefs" / f"{TASK_NAME or 'oracle'}-leak-records"
    records = {}
    for bug in (r["id"] for r in result["results"]):
        record = leak_root / bug / "blind-leak.json"
        if record.exists():
            records[bug] = json.loads(record.read_text())
    by_id = {r["id"]: r for r in result["results"]}
    results = []
    for bug, edit in by_id.items():
        record = records.get(bug)
        if not record or len(record.get("rounds", [])) != (2 if record["rounds"][0]["label"] != "missed" else 1):
            raise SystemExit(f"{bug}: paired leak round(s) are incomplete")
        candidate = (W / "candidates" / bug / "candidate.md").read_text()
        rounds = record["rounds"]
        original_sha = rounds[0]["brief_sha256"]
        final_sha = rounds[-1]["brief_sha256"]
        final_path = W / "candidates" / bug / ("final.md" if len(rounds) == 2 else "candidate.md")
        if len(rounds) == 2:
            final = final_path.read_text()
        else:
            final = candidate
        if hashlib.sha256(candidate.encode()).hexdigest() != original_sha:
            raise SystemExit(f"{bug}: first leak verdict does not bind the candidate bytes")
        if hashlib.sha256(final.encode()).hexdigest() != final_sha:
            raise SystemExit(f"{bug}: final leak verdict does not bind final bytes")
        leak = [{"label": rounds[0]["label"], "brief_sha256": original_sha,
                 "verdicts": rounds[0]["verdicts"]}]
        if len(rounds) == 2:
            leak.append({"label": rounds[1]["label"], "brief_sha256": final_sha,
                         "verdicts": rounds[1]["verdicts"]})
        results.append({"id": bug, "group": "P", "edit": json.loads((W / "candidates" / bug / "edit.json").read_text()),
                        "leak": leak, "final_brief": final})
    synced = {"results": results, "missing": []}
    out_path = W / "workflow-out.json"
    if out_path.exists() and json.loads(out_path.read_text()) != synced:
        raise SystemExit(f"{out_path} already contains different synchronized results")
    save(out_path, synced)
    print(json.dumps({"synced": len(results)}))


def build():
    s = sets()
    first = {json.loads(l)["id"]: json.loads(l) for l in open(SCRIPTS / "results/missing-facts.jsonl")}
    second = {json.loads(l)["id"]: json.loads(l) for l in open(SCRIPTS / "results/missing-facts-pass2.jsonl")}
    corpus = driver.CORPUS
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
    task_mode = TASK_JOBS is not None
    W = task_work() if task_mode else ROOT / "briefs/oracle-work"
    out = json.loads((W / "workflow-out.json").read_text())
    assert not out["missing"], out["missing"]
    decisions_path = W / "factcheck-decisions.json"
    decisions = json.loads(decisions_path.read_text()) if decisions_path.exists() else {}
    rows, rejected, factcheck, undecided = [], [], [], []
    # The group decides whether a diff needs the fact check; take it from PLAN.md, not the run.
    if task_mode:
        _, jobs = load_task_inputs()
        group_of = {j["bug"]: "P" for j in jobs}
        if json.loads((W / "edit" / "settings.json").read_text()) != task_scope():
            raise SystemExit("task scope differs from saved editor settings")
    else:
        group_of = {bug: g for g, bugs in sets().items() for bug in bugs}
    mislabeled = [r["id"] for r in out["results"] if group_of.get(r["id"]) != r["group"]]
    assert not mislabeled, f"results whose group differs from brief/PLAN.md: {mislabeled}"
    assert sorted(r["id"] for r in out["results"]) == sorted(group_of), "results must exactly cover the scoped diffs"
    for r in out["results"]:
        bug = r["id"]
        d = ROOT / "briefs/oracle" / bug
        if task_mode and (d / "brief.md").exists():
            raise SystemExit(f"{bug}: refusing to overwrite an existing published oracle brief")
        d.mkdir(parents=True, exist_ok=True)
        blind = (ROOT / "briefs/blind" / bug / "brief.md")
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
            item_file = json.loads((W / "candidates" / bug / "item.json").read_text()) if task_mode else json.loads((W / f"item-{bug}.json").read_text())
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
                if not item or (item not in final if task_mode else norm(item) not in norm(final)):
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
        if not task_mode and r["group"] == "C" and r["final_brief"]:
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
    p.add_argument("step", choices=["build", "collect", "merge", "blind-collect", "annotate",
                                    "edit-build", "edit-collect", "task-sync"])
    p.add_argument("path", nargs="?")
    p.add_argument("--run-started", help="annotate: ISO time the workflow run started")
    p.add_argument("--census", help="task editor census JSONL")
    p.add_argument("--bugs-from", help="task editor OT jobs JSON")
    p.add_argument("--name", help="task-specific work name")
    a = p.parse_args()
    if a.census or a.bugs_from or a.name:
        if a.step not in ("edit-build", "edit-collect", "task-sync", "collect"):
            p.error("--census/--bugs-from/--name apply only to task construction and collect steps")
        if not a.census or not a.bugs_from:
            p.error("task construction needs both --census and --bugs-from")
        if a.name is not None and (pathlib.Path(a.name).name != a.name or a.name in (".", "..")):
            p.error("--name must be a single path component")
        TASK_CENSUS, TASK_JOBS, TASK_NAME = a.census, a.bugs_from, a.name
        load_task_inputs()
    if a.step == "merge":
        merge(a.path)
    elif a.step == "blind-collect":
        blind_collect(a.path)
    elif a.step == "annotate":
        annotate(a.path, a.run_started)
    else:
        {"build": build, "collect": collect, "edit-build": edit_build,
         "edit-collect": edit_collect, "task-sync": task_sync}[a.step]()
