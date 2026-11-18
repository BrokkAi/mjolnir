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
# Strict-construction mode (brief/PLAN-bifrost-oracle-strict.md): fidelity check and cause gate.
STRICT = False


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
            "jobs_sha256": hashlib.sha256(pathlib.Path(TASK_JOBS).read_bytes()).hexdigest(), "name": TASK_NAME,
            **({"mode": "strict"} if STRICT else {})}


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


# Non-leaking oracle arm (brief/PLAN-bifrost-oracle-noleak.md). Each round is an editor task
# per active diff (noleak-edit-build/-collect), one blind fact-check task over the round's edits
# (noleak-check-build/-collect), then the paired leak check of brief_writer_terra.py with
# --kind oracle-noleak --round N. A diff passes when both leak judges answer missed; up to
# NOLEAK_ROUNDS rounds. Only fact-checked texts are written to texts/<diff>/round-N.md, which
# is what the leak check judges and what noleak-publish publishes.
NOLEAK_ROUNDS = 3


def max_rounds():
    """Strict mode voided round 1 as a construction defect and runs rounds 2-4 (plan amendment)."""
    return 4 if STRICT else NOLEAK_ROUNDS


def noleak_ids():
    _, jobs = load_task_inputs()
    return [j["bug"] for j in jobs]


def noleak_require_scope():
    """Every step after the first editor build uses the census and jobs frozen there."""
    path = task_work() / "settings.json"
    if not path.is_file() or json.loads(path.read_text()) != task_scope():
        raise SystemExit(f"{path}: census/jobs/name differ from the frozen no-leak scope")


def noleak_record(bug):
    path = ROOT / "briefs" / f"{TASK_NAME}-leak-records" / bug / "blind-leak.json"
    return json.loads(path.read_text()) if path.exists() else {"rounds": []}


def noleak_state(bug, upto):
    """(passed_round or None, source_text, previous) for a diff after rounds 1..upto.

    The source is the last fact-checked text, or the published OT brief. previous is what the
    last check of that diff found: leak reasons, a fact-check rejection, or None."""
    W = task_work()
    ot = ROOT / "briefs" / "oracle" / bug / "brief.md"
    source = ot.read_text()
    previous = ot_leak_reasons(bug, source)
    rounds = {r["round"]: r for r in noleak_record(bug)["rounds"]}
    for n in range(1, upto + 1):
        check = W / f"round-{n}" / "check-results.json"
        if not check.exists():
            raise SystemExit(f"round {n} fact check is not collected")
        decision = json.loads(check.read_text())["decisions"].get(bug)
        if decision is None:
            continue  # not active in round n
        if decision["decision"] != "carries":
            # Keep the leak reasons for the text that is retried: they permit and direct the
            # next edit; the rejection says what the failed edit lost.
            leak_reasons = previous if previous and previous.get("kind") in ("leak judges", "strict leak gate") \
                else (previous or {}).get("leak")
            previous = {"kind": ("fidelity check failed the last edit" if STRICT else
                                 "fact check rejected the last edit"), "why": decision["why"],
                        **({"elements_not_kept": decision.get("elements_not_kept"),
                            "fact_inferred": decision.get("fact_inferred"),
                            **({"note": "the decisive fact needed inference or more than one line: state it "
                                        "explicitly in one item"} if decision.get("fact_inferred") else {})}
                           if STRICT else {}),
                        "leak": leak_reasons}
            continue
        text = (W / "texts" / bug / f"round-{n}.md").read_text()
        if hashlib.sha256(text.encode()).hexdigest() != decision["brief_sha256"]:
            raise SystemExit(f"{bug}: round-{n}.md is not the text the fact checker accepted")
        leak = rounds.get(n)
        if leak is None or leak["brief_sha256"] != hashlib.sha256(text.encode()).hexdigest():
            raise SystemExit(f"{bug}: round {n} leak check is missing or bound to other text")
        if STRICT:
            require_strict_judges(n, bug, leak["verdicts"])
        source = text
        if leak["label"] == "missed" and not STRICT:
            return n, source, None
        if STRICT and n == 1:
            continue  # round 1 is void in strict mode (plan amendment): it can never retain a diff
        if leak["label"] == "missed":
            gate_path = W / f"round-{n}" / "gate-results.json"
            if not gate_path.exists():
                raise SystemExit(f"round {n} strict leak gate is not collected")
            gate = json.loads(gate_path.read_text())["results"][bug]
            if gate.get("verdicts_sha256") != verdicts_fingerprint(leak):
                raise SystemExit(f"{bug}: round {n} gate result does not cover the current leak verdicts")
            if gate["pass"]:
                return n, source, None
            previous = {"kind": "strict leak gate", "gate": gate.get("gate"), "why": gate["why"],
                        "answers": gate.get("answers"),
                        "reasons": [{"matching_finding": v.get("matching_finding"), "rationale": v.get("rationale"),
                                     "confidence": v.get("confidence")} for v in leak["verdicts"]]}
            continue
        previous = {"kind": "leak judges", "label": leak["label"],
                    "reasons": [{"matching_finding": v.get("matching_finding"), "rationale": v.get("rationale")}
                                for v in leak["verdicts"] if v["label"] != "missed"]}
    return None, source, previous


def ot_leak_reasons(bug, text):
    """The stored final leak verdicts of the published OT brief (round L's leak flags)."""
    record = json.loads((ROOT / "briefs" / "bifrost-oracle-leak-records" / bug / "blind-leak.json").read_text())
    last = max(record["rounds"], key=lambda r: r["round"])
    if last["brief_sha256"] != hashlib.sha256(text.encode()).hexdigest():
        raise SystemExit(f"{bug}: the OT brief's final leak record does not bind the published OT brief")
    if last["label"] == "missed":
        return None
    return {"kind": "leak judges", "label": last["label"],
            "reasons": [{"matching_finding": v.get("matching_finding"), "rationale": v.get("rationale")}
                        for v in last["verdicts"] if v["label"] != "missed"]}


def noleak_edit_build(n):
    if not 1 <= n <= max_rounds():
        raise SystemExit(f"rounds run 1..{max_rounds()}")
    census_rows, _ = load_task_inputs()
    corpus = driver.CORPUS
    W = task_work()
    base = W / f"round-{n}" / "edit"
    if base.exists():
        raise SystemExit(f"{base} exists; refusing to rebuild a round's editor units")
    scope_path = W / "settings.json"
    if scope_path.exists():
        noleak_require_scope()
    elif n != 1:
        raise SystemExit("round 1 freezes the scope; it has not been built")
    # Decide every diff's state before creating anything, so a refusal leaves no partial round.
    states = [(bug, *noleak_state(bug, n - 1)) for bug in noleak_ids()]
    made = []
    for bug, passed, source, previous in states:
        if passed:
            continue
        census = census_rows[bug]
        item = {"id": bug, "intro": corpus[bug]["intro_commit"], "base": corpus[bug]["base_commit"],
                "census": {k: census[k] for k in ("fact", "evidence", "other_facts", "kind")},
                "brief": source, "previous": previous}
        unit = base / bug
        unit.mkdir(parents=True)
        save(unit / "item.json", item)
        editor = "STRICT.md" if STRICT else "NOLEAK.md"
        (unit / editor).write_text((SCRIPTS / "brief" / editor).read_text())
        (unit / "WRITER.md").write_text((SCRIPTS / "brief/WRITER.md").read_text())
        (unit / "INSTRUCTIONS.md").write_text(
            f"Read {editor} and WRITER.md and follow them exactly. Your item is item.json. "
            "Read code only through git show or git diff at the item's intro and base commits; "
            "do not read code from the current checkout or other history. Do not read any other "
            "experiment data or history, use the network, build, test, or edit tracked files. "
            "Write edit.json in the repository working directory and edit no other file.")
        made.append({"id": bug, "source_sha256": hashlib.sha256(source.encode()).hexdigest()})
    if not scope_path.exists():
        save(scope_path, task_scope())
    save(base.parent / "edit-build.json", {"round": n, "units": made})
    print(json.dumps({"dir": str(base), "units": made}))


def noleak_validate(bug, item, raw):
    """The edited brief, after checking the edit is wording-only on whole item lines."""
    if not isinstance(raw, dict) or set(raw) != {"id", "items", "brief", "rationale"} or raw["id"] != bug:
        raise SystemExit(f"{bug}: editor output schema/id invalid")
    if not isinstance(raw["items"], list) or not raw["items"] or not isinstance(raw["brief"], str):
        raise SystemExit(f"{bug}: editor output needs a non-empty items list and a brief")
    source = item["brief"].splitlines()
    result_text = raw["brief"].rstrip() + "\n"
    result = result_text.splitlines()
    if len(result) != len(source):
        raise SystemExit(f"{bug}: edit added or removed lines")
    declared = {}
    for e in raw["items"]:
        before, after = e.get("before"), e.get("after")
        if not (isinstance(before, str) and isinstance(after, str) and before.startswith("- ") and after.startswith("- ")):
            raise SystemExit(f"{bug}: each edited item needs complete before/after item lines")
        hits = [i for i, line in enumerate(source) if line == before]
        if len(hits) != 1:
            raise SystemExit(f"{bug}: before line must match exactly one source line: {before[:80]}")
        anchors = lambda t: sorted(m.group(0) for m in brief_driver.ANCHOR.finditer(t))
        if anchors(before) != anchors(after):
            raise SystemExit(f"{bug}: an edit changed anchors: {before[:80]}")
        declared[hits[0]] = after
    for i, (old, new) in enumerate(zip(source, result)):
        if new != declared.get(i, old):
            raise SystemExit(f"{bug}: line {i + 1} differs from the declared edits")
    problems = brief_driver.lint(result_text, driver.CORPUS[bug])
    if problems and not STRICT:
        raise SystemExit(f"{bug}: edited brief fails lint: {problems}")
    # In strict mode a well-formed edit that breaks the brief rules (for example the length limit) is
    # the editor's result for the round: it fails construction for that diff, not the whole round.
    return result_text if not STRICT else (result_text, problems)


def noleak_edit_collect(n):
    W = task_work()
    noleak_require_scope()
    base = W / f"round-{n}" / "edit"
    units = json.loads((base.parent / "edit-build.json").read_text())["units"]
    out_path = base.parent / "edit-results.json"
    if out_path.exists():
        raise SystemExit(f"{out_path} exists")
    if STRICT:
        # The registered strict editor is Opus/high on claude7; check each edit's producer.
        for u in units:
            producer = json.loads((base / "out" / f"task-{u['id']}.json").read_text()).get("producer") or {}
            if (producer.get("profile"), (producer.get("config") or {}).get("model")) != ("claude7", "opus"):
                raise SystemExit(f"{u['id']}: round {n} edit came from {producer.get('profile')}/"
                                 f"{(producer.get('config') or {}).get('model')}, not claude7/opus")
    # Validate every unit before writing anything, so a failure leaves nothing half collected.
    recs = []
    for u in units:
        bug = u["id"]
        item = json.loads((base / bug / "item.json").read_text())
        raw = json.loads((base / bug / "edit.json").read_text())
        try:
            validated = noleak_validate(bug, item, raw)
        except SystemExit as e:
            # In strict mode a well-formed edit that breaks the wording-only rules (lines, anchors,
            # undeclared changes) is that diff's failed attempt; malformed output still stops collection.
            if not STRICT or "schema" in str(e) or "non-empty items list" in str(e):
                raise
            recs.append({"id": bug, "source_sha256": u["source_sha256"], "edited_items": len(raw["items"]),
                         "brief": item["brief"], "brief_sha256": "structural-failure",
                         "lint_problems": [f"structural: {e}"]})
            continue
        text, lint_problems = validated if STRICT else (validated, [])
        recs.append({"id": bug, "source_sha256": u["source_sha256"], "edited_items": len(raw["items"]),
                     "brief": text, "brief_sha256": hashlib.sha256(text.encode()).hexdigest(),
                     **({"lint_problems": lint_problems} if lint_problems else {})})
    save(out_path, {"round": n, "results": recs})
    print(json.dumps([{k: r[k] for k in ("id", "edited_items", "brief_sha256")} for r in recs]))


def noleak_check_build(n):
    W = task_work()
    noleak_require_scope()
    # Edits that broke the brief rules already failed construction; only the others are checked.
    edits = [r for r in json.loads((W / f"round-{n}" / "edit-results.json").read_text())["results"]
             if not r.get("lint_problems")]
    census_rows, _ = load_task_inputs()
    corpus = driver.CORPUS
    unit = W / f"round-{n}" / "check" / "factcheck"
    if unit.exists():
        raise SystemExit(f"{unit} exists")
    unit.mkdir(parents=True)
    spec, output = ("FIDELITY.md", "fidelity.jsonl") if STRICT else ("FACTCHECK.md", "factcheck.jsonl")
    for r in edits:
        census = census_rows[r["id"]]
        item = {"id": r["id"], "intro": corpus[r["id"]]["intro_commit"], "base": corpus[r["id"]]["base_commit"],
                "census": {k: census[k] for k in ("fact", "evidence", "other_facts", "kind")},
                "brief": r["brief"], "brief_sha256": r["brief_sha256"]}
        if STRICT:
            # Fidelity is always judged against the round L OT text, not an intermediate round.
            original = (ROOT / "briefs" / "oracle" / r["id"] / "brief.md").read_text().splitlines()
            edited = r["brief"].splitlines()
            if len(original) != len(edited):
                raise SystemExit(f"{r['id']}: edited brief does not keep the OT brief's lines")
            item["pairs"] = [{"original": o, "edited": e} for o, e in zip(original, edited) if o != e]
        save(unit / f"item-{r['id']}.json", item)
    (unit / spec).write_text((SCRIPTS / "brief" / spec).read_text())
    (unit / "INSTRUCTIONS.md").write_text(
        f"Read {spec} and follow it exactly. Your item files are the {len(edits)} item-*.json files "
        "in this folder. Read code only through git show or git diff at each item's intro and base commits. "
        "Do not read any other experiment data or history, use the network, build, test, or edit tracked "
        f"files. Write {output} in the repository working directory and edit no other file.")
    print(json.dumps({"dir": str(unit.parent), "items": len(edits)}))


def noleak_check_collect(n):
    W = task_work()
    noleak_require_scope()
    all_edits = {r["id"]: r for r in json.loads((W / f"round-{n}" / "edit-results.json").read_text())["results"]}
    edits = {b: r for b, r in all_edits.items() if not r.get("lint_problems")}
    out_path = W / f"round-{n}" / "check-results.json"
    output = "fidelity.jsonl" if STRICT else "factcheck.jsonl"
    if STRICT:
        # The registered fidelity checker is Opus/high on claude7; check the producer of this output.
        producer = json.loads((W / f"round-{n}" / "check" / "out" / "task-factcheck.json").read_text()).get("producer") or {}
        if (producer.get("profile"), (producer.get("config") or {}).get("model")) != ("claude7", "opus"):
            raise SystemExit(f"round {n} fidelity output came from {producer.get('profile')}/"
                             f"{(producer.get('config') or {}).get('model')}, not claude7/opus")
    rows = [json.loads(line) for line in
            (W / f"round-{n}" / "check" / "factcheck" / output).read_text().splitlines() if line.strip()]
    if sorted(r.get("id") for r in rows) != sorted(edits):
        raise SystemExit("fact check must cover each edited brief exactly once")
    decisions = {}
    if STRICT:
        rows = [strict_fidelity_decision(r, edits.get(r.get("id"))) for r in rows]
    for r in rows:
        e = edits[r["id"]]
        if r.get("brief_sha256") != e["brief_sha256"] or r.get("decision") not in ("carries", "does not carry"):
            raise SystemExit(f"{r['id']}: fact check names another brief or an invalid decision")
        if r["decision"] == "carries":
            # The checker quotes a whole line of exactly this text; a substring is not a quote.
            if not isinstance(r.get("item"), str) or r["item"] not in e["brief"].splitlines() or not r["item"].startswith("- "):
                raise SystemExit(f"{r['id']}: carrying item is not a complete line of the checked brief")
        decisions[r["id"]] = {"decision": r["decision"], "item": r.get("item"), "why": r.get("why"),
                              "brief_sha256": e["brief_sha256"],
                              **({"elements_not_kept": r["elements_not_kept"], "fact_inferred": r["fact_inferred"]}
                                 if STRICT else {})}
    for bug, r in all_edits.items():
        if r.get("lint_problems"):
            decisions[bug] = {"decision": "does not carry", "item": None,
                              "why": f"edit breaks the brief rules: {r['lint_problems']}",
                              "brief_sha256": r["brief_sha256"],
                              "elements_not_kept": [f"lint: {x}" for x in r["lint_problems"]], "fact_inferred": False}
    if out_path.exists():
        if json.loads(out_path.read_text()) != {"round": n, "decisions": decisions}:
            raise SystemExit(f"{out_path} holds different decisions")
    else:
        # The results file is written first and is the leak builder's record of completion; the
        # texts follow from it, and a rerun after an interruption writes only the missing ones
        # without rewriting the frozen record.
        save(out_path, {"round": n, "decisions": decisions})
    for bug, d in decisions.items():
        if d["decision"] == "carries":
            path = W / "texts" / bug / f"round-{n}.md"
            if path.exists():
                if path.read_text() != edits[bug]["brief"]:
                    # Never overwrite: a differing text is evidence that a frozen input drifted.
                    raise SystemExit(f"{path} holds text other than the accepted edit")
                continue
            # Write then rename, so an interrupted write never leaves a truncated text in place.
            path.parent.mkdir(parents=True, exist_ok=True)
            tmp = path.with_name(path.name + ".tmp")
            tmp.write_text(edits[bug]["brief"])
            tmp.replace(path)
    print(json.dumps({"round": n, "carries": sorted(b for b, d in decisions.items() if d["decision"] == "carries"),
                      "rejected": sorted(b for b, d in decisions.items() if d["decision"] != "carries")}))


def strict_fidelity_decision(r, e):
    """Maps a fidelity row onto the fact-check decision shape. The checker's own `decision` is
    not trusted alone: a pass also needs every element kept, no inference and a whole-line quote."""
    if e is None or r.get("brief_sha256") != e["brief_sha256"]:
        raise SystemExit(f"{r.get('id')}: fidelity row names another brief")
    elements = r.get("elements")
    if not isinstance(elements, list) or any(not isinstance(x, dict) or not isinstance(x.get("kept"), bool)
                                             or not isinstance(x.get("element"), str) or not x["element"].strip()
                                             for x in elements):
        raise SystemExit(f"{r['id']}: fidelity row has no valid element list")
    if not isinstance(r.get("fact_inferred"), bool) or r.get("decision") not in ("pass", "fail"):
        raise SystemExit(f"{r['id']}: fidelity row has an invalid decision or fact_inferred")
    # Every changed line must have its elements listed; a skipped line cannot pass.
    original = (ROOT / "briefs" / "oracle" / r["id"] / "brief.md").read_text().splitlines()
    pair_count = sum(o != ed for o, ed in zip(original, e["brief"].splitlines()))
    covered = {x.get("pair") for x in elements}
    if any(not isinstance(i, int) or not 0 <= i < pair_count for i in covered):
        raise SystemExit(f"{r['id']}: fidelity elements name pairs outside 0..{pair_count - 1}")
    uncovered = sorted(set(range(pair_count)) - covered)
    added = r.get("added")
    if not isinstance(added, list):
        raise SystemExit(f"{r['id']}: fidelity row has no list of added elements")
    not_kept = ([x.get("element") for x in elements if not x["kept"]] + [f"pair {i}: no elements listed" for i in uncovered]
                + [f"added: {a}" for a in added])
    quote = r.get("carrying_item")
    whole_line = isinstance(quote, str) and quote.startswith("- ") and quote in e["brief"].splitlines()
    if uncovered or (r["decision"] == "pass" and not whole_line):
        # The checker claims a pass but left lines unassessed or quoted no valid line: fidelity was not
        # assessed, so rerun the checker rather than charge the edit with a failure.
        raise SystemExit(f"{r['id']}: fidelity pass with unassessed pairs {uncovered} or an invalid quote; "
                         "rerun the fidelity check")
    ok = r["decision"] == "pass" and not not_kept and not r["fact_inferred"] and whole_line
    return {"id": r["id"], "brief_sha256": r["brief_sha256"], "decision": "carries" if ok else "does not carry",
            "item": quote if whole_line else None, "why": r.get("why"), "elements_not_kept": not_kept,
            "fact_inferred": r["fact_inferred"]}


# Judge A on Claude Opus (claude7), judge B on GPT Sol (codex), as brief/PLAN-bifrost-oracle-strict.md registers.
STRICT_JUDGE_MODELS = {"A": ("claude7", "opus"), "B": ("codex", "gpt-6.1-sol")}  # judge: (profile, model)


def require_strict_judges(n, bug=None, verdicts=None):
    """The registered leak rule pairs one Opus and one Sol judge for every strict leak judgment.

    Reads each judge's output file, which names the session, profile and model that produced its
    verdicts, and (given a leak record's verdicts) requires them to be exactly those outputs."""
    leak_dir = ROOT / "briefs" / f"{TASK_NAME}-leak-{n}"
    key = json.loads((leak_dir / "key.json").read_text())
    produced = {}
    for judge, (profile, model) in STRICT_JUDGE_MODELS.items():
        out = json.loads((leak_dir / "out" / f"verdicts-0-{judge}.json").read_text())
        p = out.get("producer") or {}
        if (p.get("profile"), (p.get("config") or {}).get("model")) != (profile, model):
            raise SystemExit(f"round {n} judge {judge} verdicts came from {p.get('profile')}/"
                             f"{(p.get('config') or {}).get('model')}, not {profile}/{model}")
        for v in out["verdicts"]:
            produced[(key[v["output_id"]]["bug"], judge)] = (v.get("label"), v.get("rationale"), v.get("confidence"))
    if bug is not None:
        got = {(bug, v["judge"]): (v.get("label"), v.get("rationale"), v.get("confidence")) for v in verdicts}
        if set(got) != {(bug, "A"), (bug, "B")} or any(produced.get(k) != got[k] for k in got):
            raise SystemExit(f"{bug}: round {n} leak record is not the registered judges' output")


def quotes_rationale(why, rationale):
    """The cause answer's reason must quote its own item's rationale: some quoted span of at least
    eight characters in `why` appears in that rationale (whitespace and case normalized)."""
    norm = lambda t: " ".join(t.split()).lower()
    # At least eight characters, or the whole rationale when it is shorter than that.
    shortest = min(8, len(norm(rationale)))
    spans = re.findall(r'"([^"]+)"|\u201c([^\u201d]+)\u201d|\'([^\']+)\'', why)
    return shortest > 0 and any(len(norm(s)) >= shortest and norm(s) in norm(rationale)
                                for group in spans for s in group if s)


def verdicts_fingerprint(leak_round):
    """Binds a gate result to the exact leak verdicts it judged."""
    return hashlib.sha256(json.dumps(leak_round["verdicts"], sort_keys=True).encode()).hexdigest()


def noleak_gate_build(n):
    """Strict leak gate: both leak verdicts `missed` at high confidence go to one blind cause check;
    any other round-n leak result fails the gate without one."""
    if not STRICT:
        raise SystemExit("the leak gate exists only in strict mode")
    W = task_work()
    noleak_require_scope()
    unit = W / f"round-{n}" / "gate" / "cause"
    if unit.exists():
        raise SystemExit(f"{unit} exists")
    checks = json.loads((W / f"round-{n}" / "check-results.json").read_text())["decisions"]
    pending = 0
    keymap = {}
    accepted = [bug for bug, d in checks.items() if d["decision"] == "carries"]
    if not accepted:
        # No edit passed fidelity: there was no leak round and nothing to gate.
        print(json.dumps({"round": n, "cause_checks": 0, "dir": None, "note": "no brief passed fidelity"}))
        return
    # Every accepted diff's round-n leak record must exist and be the registered judges' output
    # before anything is written.
    records = {}
    for bug in accepted:
        r = next((x for x in noleak_record(bug)["rounds"] if x["round"] == n), None)
        if r is None:
            raise SystemExit(f"{bug}: round {n} leak verdicts are not collected")
        require_strict_judges(n, bug, r["verdicts"])
        if r["label"] == "missed" and any(not (v.get("rationale") or "").strip()
                                          or (v.get("confidence") or "").lower() not in ("high", "medium", "low")
                                          for v in r["verdicts"]):
            # Malformed, not evidence: the verdict must be rejudged before the gate can run.
            raise SystemExit(f"{bug}: a round {n} missed verdict lacks a rationale or a valid confidence; rejudge it")
        records[bug] = r
    for bug, r in records.items():
        if r["label"] != "missed" or any((v.get("confidence") or "").lower() != "high" for v in r["verdicts"]):
            continue
        for v in r["verdicts"]:
            unit.mkdir(parents=True, exist_ok=True)
            # The cause check sees only an opaque token, the rationale and the fixed question in
            # CAUSE.md: a descriptive diff name could itself hint at the mechanism. The key that maps
            # tokens back stays outside the uploaded unit.
            token = hashlib.sha256(f"{n}/{bug}/{v['judge']}/{v.get('rationale')}".encode()).hexdigest()[:16]
            keymap[token] = {"bug": bug, "judge": v["judge"], "verdicts_sha256": verdicts_fingerprint(r)}
            save(unit / f"item-{token}.json", {"id": token, "rationale": v.get("rationale")})
            pending += 1
    if pending:
        save(unit.parent / "key.json", keymap)
        (unit / "CAUSE.md").write_text((SCRIPTS / "brief/CAUSE.md").read_text())
        (unit / "INSTRUCTIONS.md").write_text(
            f"Read CAUSE.md and follow it exactly. Your item files are the {pending} item-*.json files in this "
            "folder. Do not read anything else, use the network, build, test, or edit tracked files. "
            "Write cause.jsonl in the repository working directory and edit no other file.")
    print(json.dumps({"round": n, "cause_checks": pending, "dir": str(unit.parent) if pending else None}))


def noleak_gate_collect(n):
    W = task_work()
    noleak_require_scope()
    out_path = W / f"round-{n}" / "gate-results.json"
    checks = json.loads((W / f"round-{n}" / "check-results.json").read_text())["decisions"]
    unit = W / f"round-{n}" / "gate" / "cause"
    keymap = json.loads((unit.parent / "key.json").read_text()) if (unit.parent / "key.json").exists() else {}
    causes = {}
    if keymap:
        # The registered cause checker is Opus/high on claude7; check the producer of its answers.
        producer = json.loads((unit.parent / "out" / "task-cause.json").read_text()).get("producer") or {}
        if (producer.get("profile"), (producer.get("config") or {}).get("model")) != ("claude7", "opus"):
            raise SystemExit(f"round {n} cause answers came from {producer.get('profile')}/"
                             f"{(producer.get('config') or {}).get('model')}, not claude7/opus")
        rows = [json.loads(line) for line in (unit / "cause.jsonl").read_text().splitlines() if line.strip()]
        got = {r.get("id"): r for r in rows}
        if set(got) != set(keymap) or len(rows) != len(keymap):
            raise SystemExit("cause check must answer each verdict exactly once")
        # Validate every answer before any gate decision is recorded: a malformed row is an error to
        # fix and rerun, not evidence for or against a brief.
        rationales = {t: json.loads((unit / f"item-{t}.json").read_text())["rationale"] for t in keymap}
        bad = [t for t, r in got.items() if r.get("answer") not in ("yes", "no", "unclear")
               or not isinstance(r.get("why"), str) or not quotes_rationale(r["why"], rationales[t])]
        if bad:
            raise SystemExit(f"cause check rows without a valid answer and quoted reason: {bad}")
        causes = {f"{keymap[t]['bug']}-{keymap[t]['judge']}": {**got[t], "judge": keymap[t]["judge"],
                                                                "verdicts_sha256": keymap[t]["verdicts_sha256"]}
                  for t in keymap}
    results = {}
    for bug, d in checks.items():
        if d["decision"] != "carries":
            continue
        r = next(x for x in noleak_record(bug)["rounds"] if x["round"] == n)
        if r["label"] != "missed":
            results[bug] = {"pass": False, "gate": "leak", "why": f"leak label {r['label']}"}
        elif any(not (v.get("rationale") or "").strip()
                 or (v.get("confidence") or "").lower() not in ("high", "medium", "low") for v in r["verdicts"]):
            raise SystemExit(f"{bug}: a round {n} missed verdict lacks a rationale or a valid confidence; rejudge it")
        elif any((v.get("confidence") or "").lower() != "high" for v in r["verdicts"]):
            results[bug] = {"pass": False, "gate": "leak", "why": "a missed verdict below high confidence: " +
                            ", ".join(f"{v['judge']}={v.get('confidence')}" for v in r["verdicts"])}
        else:
            if any(causes[f"{bug}-{v['judge']}"]["verdicts_sha256"] != verdicts_fingerprint(r) for v in r["verdicts"]):
                raise SystemExit(f"{bug}: cause answers were given for other leak verdicts")
            # Only a clear "no" passes; "yes", "unclear" or anything else fails the brief.
            flagged = [causes[f"{bug}-{v['judge']}"] for v in r["verdicts"]
                       if causes[f"{bug}-{v['judge']}"].get("answer") != "no"]
            results[bug] = {"pass": not flagged, "gate": "cause",
                            "answers": [causes[f"{bug}-{v['judge']}"].get("answer") for v in r["verdicts"]],
                            "why": "; ".join(f"judge {c['judge']}: {c.get('why')}" for c in flagged) or "clean"}
        results[bug]["verdicts_sha256"] = verdicts_fingerprint(r)
    record = {"round": n, "results": results}
    if out_path.exists():
        frozen = json.loads(out_path.read_text())
        # A record frozen before the `answers` field existed is compared without it; it is never
        # rewritten, so the frozen bytes stay as collected.
        strip = lambda rec: {"round": rec["round"], "results": {b: {k: v for k, v in x.items() if k != "answers"}
                                                               for b, x in rec["results"].items()}}
        legacy = not any("answers" in x for x in frozen["results"].values())
        if (strip(frozen) != strip(record)) if legacy else (frozen != record):
            raise SystemExit(f"{out_path} holds different gate results")
    else:
        save(out_path, record)
    print(json.dumps({"round": n, "pass": sorted(b for b, x in results.items() if x["pass"]),
                      "fail": {b: x["why"][:120] for b, x in results.items() if not x["pass"]}}))


def noleak_publish(jobs_out):
    W = task_work()
    noleak_require_scope()
    done = max((n for n in range(1, max_rounds() + 1) if (W / f"round-{n}" / "check-results.json").exists()), default=0)
    retained, excluded = [], []
    for bug in noleak_ids():
        passed, source, previous = noleak_state(bug, done)
        if passed:
            retained.append({"bug": bug, "round": passed, "brief_sha256": hashlib.sha256(source.encode()).hexdigest()})
        elif done < max_rounds():
            raise SystemExit(f"{bug}: not passed after round {done}; run the remaining rounds first")
        else:
            # Say why: a diff that never reached a clean leak check failed construction, which is
            # not evidence that its complete fact cannot be stated without leaking.
            kind = (previous or {}).get("kind", "")
            if "fidelity" in kind or "fact check" in kind:
                cause = "fidelity (editor failure)"
            elif kind == "strict leak gate":
                # Unresolved judgments fail retention but are not evidence that the complete fact
                # cannot be stated without leaking; keep them apart from affirmative ones.
                gate = previous.get("gate", "leak")
                why = previous.get("why", "")
                if gate == "leak" and why.startswith("a missed verdict below high confidence"):
                    cause = "leak (unresolved: missed below high confidence)"
                elif gate == "cause" and "yes" not in (previous.get("answers") or []):
                    cause = "cause (unresolved: unclear answer)"
                else:
                    cause = gate
            else:
                cause = "leak"
            excluded.append({"bug": bug, "after_round": done, "cause": cause, "last_check": previous})
    for r in retained:
        d = ROOT / "briefs" / ("oracle-strict" if STRICT else "oracle-noleak") / r["bug"]
        text = (W / "texts" / r["bug"] / f"round-{r['round']}.md").read_text()
        if (d / "brief.md").exists() and (d / "brief.md").read_text() != text:
            raise SystemExit(f"{d}: a different brief is already published")
        d.mkdir(parents=True, exist_ok=True)
        (d / "brief.md").write_text(text)
    # ON and its same-image no-brief control AF, interleaved per diff (the plan's AF amendment).
    arms = ("OX", "AF2") if STRICT else ("ON", "AF")  # OX is the plan's strict arm "OS" (driver.py)
    save(pathlib.Path(jobs_out), [{"bug": r["bug"], "arm": arm} for r in retained for arm in arms])
    # Registered: fewer than 8 retained diffs reads Inconclusive whatever the scores. With none
    # retained there is nothing to review or score, so the outcome is recorded here.
    outcome = "Inconclusive (fewer than 8 retained diffs)" if len(retained) < 8 else None
    save(W / "publication.json", {"retained": retained, "excluded": excluded, "jobs": str(jobs_out),
                                  "registered_outcome_from_retention": outcome})
    print(json.dumps({"retained": len(retained), "excluded": [e["bug"] for e in excluded],
                      "registered_outcome_from_retention": outcome}))


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("step", choices=["build", "collect", "merge", "blind-collect", "annotate",
                                    "edit-build", "edit-collect", "task-sync",
                                    "noleak-edit-build", "noleak-edit-collect", "noleak-check-build",
                                    "noleak-check-collect", "noleak-gate-build", "noleak-gate-collect",
                                    "noleak-publish"])
    p.add_argument("--strict", action="store_true", help="noleak steps: strict construction (fidelity check, leak gate)")
    p.add_argument("--round", type=int, help="noleak steps: round 1..3")
    p.add_argument("--jobs-out", help="noleak-publish: where to write the ON jobs file")
    p.add_argument("path", nargs="?")
    p.add_argument("--run-started", help="annotate: ISO time the workflow run started")
    p.add_argument("--census", help="task editor census JSONL")
    p.add_argument("--bugs-from", help="task editor OT jobs JSON")
    p.add_argument("--name", help="task-specific work name")
    a = p.parse_args()
    if a.census or a.bugs_from or a.name:
        if a.step not in ("edit-build", "edit-collect", "task-sync", "collect") and not a.step.startswith("noleak-"):
            p.error("--census/--bugs-from/--name apply only to task construction and collect steps")
        if not a.census or not a.bugs_from:
            p.error("task construction needs both --census and --bugs-from")
        if a.name is not None and (pathlib.Path(a.name).name != a.name or a.name in (".", "..")):
            p.error("--name must be a single path component")
        TASK_CENSUS, TASK_JOBS, TASK_NAME = a.census, a.bugs_from, a.name
        load_task_inputs()
    if a.strict and not a.step.startswith("noleak-"):
        p.error("--strict applies only to noleak steps")
    STRICT = a.strict
    if a.step.startswith("noleak-"):
        if not TASK_NAME:
            p.error("noleak steps need --census, --bugs-from and --name")
        if a.step == "noleak-publish":
            if not a.jobs_out:
                p.error("noleak-publish needs --jobs-out")
            noleak_publish(a.jobs_out)
        else:
            if a.round is None:
                p.error("noleak steps need --round")
            {"noleak-edit-build": noleak_edit_build, "noleak-edit-collect": noleak_edit_collect,
             "noleak-check-build": noleak_check_build, "noleak-check-collect": noleak_check_collect,
             "noleak-gate-build": noleak_gate_build, "noleak-gate-collect": noleak_gate_collect}[a.step](a.round)
    elif a.step == "merge":
        merge(a.path)
    elif a.step == "blind-collect":
        blind_collect(a.path)
    elif a.step == "annotate":
        annotate(a.path, a.run_started)
    else:
        {"build": build, "collect": collect, "edit-build": edit_build,
         "edit-collect": edit_collect, "task-sync": task_sync}[a.step]()
