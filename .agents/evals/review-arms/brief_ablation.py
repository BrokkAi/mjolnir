"""Tooling for the relation-brief ablation (brief/PLAN-ablation.md).

Usage (REVIEW_ARMS_DIR holds the data):
  python3 brief_ablation.py jobs            # ablation-jobs.json: the plan's 10 diffs x arms DT,DS,XT,XS
  python3 brief_ablation.py minus           # briefs/blind-minus/<diff>/: blind brief without the stage A match
  python3 brief_ablation.py editor-items    # briefs/bifrost-plus-work/: item files and EDITOR.md for the editor
  (an Opus/high editor writes briefs/bifrost-plus-work/edited.jsonl)
  python3 brief_ablation.py plus <edited.jsonl> [--replace DIFF ...] [--out DIR]
      inserts each edited item into the Bifrost brief -> briefs/bifrost-plus/<diff>/
  python3 brief_ablation.py check-items     # briefs/bifrost-plus-check/{contains,leak}/, bound to brief hashes
  (judge the checks; both must pass, see the plan)
  python3 brief_ablation.py readout --round brief-judge-g
      all rates from round G's labels (round F's beside them as a judge check);
      closure c = (D - F) / (B - F), B - X and the plan's outcome -> <round>/ablation.json
"""
import argparse
import collections
import hashlib
import json
import pathlib
import random
import re
import statistics

import brief_oracle
from driver import CORPUS, ROOT, SCRIPTS, save

ARM_KINDS = {"DT": "bifrost-plus", "DS": "bifrost-plus", "XT": "blind-minus", "XS": "blind-minus"}
MODELS = ("gpt-6.1-terra", "gpt-6.1-sol")
PRIMARY = MODELS[0]
BOOT = 10000
PLUS_WORK = ROOT / "briefs/bifrost-plus-work"
PLUS_CHECK = ROOT / "briefs/bifrost-plus-check"


def sha(text):
    return hashlib.sha256(text.encode()).hexdigest()


def diffs():
    """The 10 diffs of the plan's Diffs section, in the order it names them."""
    plan = (SCRIPTS / "brief/PLAN-ablation.md").read_text()
    block = plan.split("\n## Diffs", 1)[1].split("\n## Arms", 1)[0]
    ids = list(dict.fromkeys(w for w in re.findall(r"[a-z0-9]+(?:-[a-z0-9]+)+", block) if w in CORPUS))
    assert len(ids) == 10, ids
    return ids


def oracle_results():
    out = json.loads((ROOT / "briefs/oracle-work/workflow-out.json").read_text())
    return {r["id"]: r for r in out["results"]}


def jobs():
    save(ROOT / "ablation-jobs.json", [{"bug": b, "arm": a} for b in diffs() for a in ARM_KINDS])
    print(f"wrote {ROOT / 'ablation-jobs.json'}: {len(diffs())} diffs x {len(ARM_KINDS)} arms")


def minus():
    results = oracle_results()
    for bug in diffs():
        item = ((results[bug].get("edit") or {}).get("matching_item") or "").strip("\n")
        if not item:
            raise SystemExit(f"{bug}: the stage A editor has no matching_item")
        src = ROOT / "briefs/blind" / bug / "brief.md"
        text = src.read_text()
        at = text.find(item)
        # Exactly once, and only whole lines go.
        if text.count(item) != 1 or (at and text[at - 1] != "\n") or text[at + len(item):][:1] not in ("", "\n"):
            raise SystemExit(f"{bug}: matching_item is not present verbatim exactly once as whole lines")
        end = at + len(item) + (1 if text[at + len(item):][:1] == "\n" else 0)
        result = text[:at] + text[end:]
        d = ROOT / "briefs/blind-minus" / bug
        d.mkdir(parents=True, exist_ok=True)
        (d / "brief.md").write_text(result)
        save(d / "meta.json", {"source": str(src), "source_sha256": sha(text), "removed": item,
                               "result_sha256": sha(result)})
        print(f"{bug}: removed {item}")


EDITOR = """# Editor for the Bifrost brief plus the decisive item (arm D)

Context: `brief/PLAN-ablation.md`, arm D. A reviewer is shown a brief of
relations between a code change and the rest of the repository. The Bifrost
brief (tool-generated) did not help reviewers on this diff; the blind brief
(LLM-written) did. You add the one item that states the decisive relation to
the Bifrost brief, in the Bifrost brief's own form, and change nothing else.

## Inputs

One file per diff, `item-<diff>.json`:

- `intro`, `base`: the commit with the change and its parent.
- `census`: the decisive `fact`, its `evidence`, `other_facts` and `kind`.
- `carrier_item`: the item of the blind-style oracle brief that states the
  decisive relation. It is your source, in another form.
- `bifrost_brief`: the Bifrost brief text, as reviewers will see it.

Read the repository in your working directory as needed (`git show <intro>`,
`git show <intro>:<path>`). Do not build, run tests, edit files, or use the
network.

## Rules

1. Rewrite `carrier_item` as one item in the form of the Bifrost brief's other
   items: qualified names and `file:line` anchors in backticks, as the
   neighbouring items write them, one line starting with `- `. Keep exactly the
   relation the census fact states between the same locations; check each
   anchor at `intro`.
2. Give it the same completeness tag as its neighbours (for example
   `[incomplete: ...]`), whatever they carry.
3. No judgment words (bug, wrong, missing, should, risk, unsafe, issue,
   problem, incorrect, broken, fail to, forget, or similar) and no consequence:
   no "so ...", "which means ...", "cannot", "never reaches", "is lost". A
   clause is allowed only when it is itself a plain relation between code
   locations. When the fact is that code does not do something, say so with
   the exact name.
4. The item may state a relation Bifrost cannot compute today. That is
   intended. It must not stand out by length, emphasis or wording.
5. Choose the section (`## Affected callers`, `## Paths`, ... as the brief has
   it) where the relation belongs and the position in diff order. Do not change,
   remove or reorder any other line.

## Output

Write `edited.jsonl` in your working directory, one JSON object per item file:
{"id": "<diff>", "section": "## Affected callers", "after_line": "<the existing
brief line the new item goes after, verbatim, or null for the first item of the
section>", "item": "- the new line"}. `after_line` must occur exactly once in
that section of `bifrost_brief`. Hand back the count of items written.
"""


def editor_items():
    results = oracle_results()
    PLUS_WORK.mkdir(parents=True, exist_ok=True)
    for bug in diffs():
        work = json.loads((ROOT / f"briefs/oracle-work/item-{bug}.json").read_text())
        census = {k: work["census"][k] for k in ("fact", "evidence", "other_facts", "kind")}
        carrier = brief_oracle.carrier_item(results[bug])
        if not carrier:
            raise SystemExit(f"{bug}: the stage A editor has no carrier item")
        save(PLUS_WORK / f"item-{bug}.json", {
            "id": bug, "intro": work["intro"], "base": work["base"], "census": census,
            "carrier_item": carrier,
            "bifrost_brief": (ROOT / "briefs/bifrost" / bug / "brief.md").read_text()})
    (PLUS_WORK / "EDITOR.md").write_text(EDITOR)
    print(f"wrote {len(diffs())} item files and EDITOR.md in {PLUS_WORK}")


def insert(brief, section, after_line, item):
    """brief with item placed after after_line of section (null: right after the heading)."""
    lines = brief.split("\n")
    heading = section if section.startswith("## ") else "## " + section
    starts = [i for i, l in enumerate(lines) if l.rstrip() == heading]
    if len(starts) != 1:
        raise ValueError(f"section {heading!r} occurs {len(starts)} times")
    end = next((i for i in range(starts[0] + 1, len(lines)) if lines[i].startswith("## ")), len(lines))
    if after_line is None:
        at = starts[0]
    else:
        hits = [i for i in range(starts[0] + 1, end) if lines[i].rstrip() == after_line.rstrip()]
        if len(hits) != 1:
            raise ValueError(f"after_line occurs {len(hits)} times in {heading!r}")
        at = hits[0]
    item = item.strip()
    if "\n" in item:
        raise ValueError("item must be one line")
    return "\n".join(lines[:at + 1] + [item if item.startswith("- ") else "- " + item] + lines[at + 1:])


def plus(path, replace, out):
    rows = [json.loads(l) for l in open(path) if l.strip()]
    # A compound fact may be split into at most two consecutive items (the plan's arm D); the
    # second row's after_line is the first new item, so rows of one diff apply in file order.
    by_bug = collections.defaultdict(list)
    for r in rows:
        by_bug[r["id"]].append(r)
    if not set(by_bug) <= set(diffs()) or any(len(rs) > 2 for rs in by_bug.values()):
        raise SystemExit(f"edited rows must name diffs of the plan, at most two each: {[r['id'] for r in rows]}")
    for bug, rs in by_bug.items():
        src = ROOT / "briefs/bifrost" / bug / "brief.md"
        text = result = src.read_text()
        try:
            for r in rs:
                result = insert(result, r["section"], r["after_line"], r["item"])
        except ValueError as e:
            raise SystemExit(f"{bug}: {e}")
        d = out / bug
        if (d / "brief.md").exists() and bug not in replace:
            raise SystemExit(f"{d / 'brief.md'} exists; give --replace {bug} to overwrite it")
        d.mkdir(parents=True, exist_ok=True)
        (d / "brief.md").write_text(result)
        save(d / "meta.json", {"source": str(src), "source_sha256": sha(text),
                               "inserted": [{"section": r["section"], "after_line": r["after_line"],
                                             "item": r["item"].strip()} for r in rs],
                               "result_sha256": sha(result)})
        print(f"{bug}: inserted {len(rs)} item(s) into {rs[0]['section']}")
    missing = sorted(set(diffs()) - {b for b in diffs() if (out / b / "brief.md").exists()})
    if missing:
        print(f"no bifrost-plus brief yet for: {missing}")


def check_items():
    contains = PLUS_CHECK / "contains"
    leak = PLUS_CHECK / "leak"
    for d in (contains, leak):
        d.mkdir(parents=True, exist_ok=True)
    (contains / "CONTAINS.md").write_text((ROOT / "briefs/bifrost-contains/CONTAINS.md").read_text())
    corpus = {r["id"]: r for r in map(json.loads, open(SCRIPTS / "corpus/corpus.jsonl"))}
    batch, key = [], {}
    for bug in diffs():
        brief = (ROOT / "briefs/bifrost-plus" / bug / "brief.md").read_text()
        item = json.loads((ROOT / f"briefs/bifrost-contains/item-{bug}.json").read_text())
        item.update({"brief": brief, "brief_sha256": sha(brief)})
        save(contains / f"item-{bug}.json", item)
        oid = sha(f"{bug}\n{brief}")[:16]
        key[oid] = {"bug": bug, "brief_sha256": sha(brief)}
        c = corpus[bug]
        batch.append({"bug": bug, **{k: c[k] for k in ("defect", "location", "trigger", "match_rule", "near_misses",
                                                       "why_findable", "intro_commit", "fix_commit")},
                      "outputs": [{"output_id": oid, "text": brief}]})
    save(leak / "batch-0.json", batch)
    save(leak / "key.json", key)
    (leak / "SPEC.md").write_text((SCRIPTS / "judge/SPEC.md").read_text())
    print(f"wrote {contains} and {leak} for {len(batch)} diffs")


def final_labels(round_name):
    """output id -> final label of a judging round (hand-check decision, else both judges' missed)."""
    j = ROOT / round_name
    decided = json.loads((j / "handcheck-decisions.json").read_text())
    final = {}
    for line in open(j / "merged.jsonl"):
        r = json.loads(line)
        if r["output_id"] in decided:
            final[r["output_id"]] = decided[r["output_id"]]
        else:
            assert r["labels"] == ["missed", "missed"], f"{round_name} {r['output_id']} needs a hand-check decision"
            final[r["output_id"]] = "missed"
    return final


def rates(round_name, bugs):
    """(condition, model) -> bug -> catch share, for the diffs in bugs."""
    key = json.loads((ROOT / "private" / f"key-{round_name}.json").read_text())["outputs"]
    final = final_labels(round_name)
    labels = collections.defaultdict(list)
    for oid, k in key.items():
        if k["bug"] in bugs:
            labels[(k["condition"], k["model"], k["bug"])].append(final[oid] == "caught")
    out = collections.defaultdict(dict)
    for (cond, model, bug), v in labels.items():
        out[(cond, model)][bug] = sum(v) / len(v)
    return out


def interval(values):
    values = sorted(values)
    n = len(values)
    return round(values[int(0.025 * n)], 3), round(values[int(0.975 * n)], 3)


def model_readout(model, bugs, g, f):
    """All five rates from round G's labels (g); round F's labels (f) only as a judge-consistency check."""
    get = lambda r, cond: [r[(cond, model)][b] for b in bugs]
    D, X, N = get(g, "bifrost-plus"), get(g, "blind-minus"), get(g, "no-brief")
    F, B = get(g, "bifrost"), get(g, "blind")
    mean = statistics.fmean
    g0 = mean(B) - mean(F)
    closure = (mean(D) - mean(F)) / g0 if g0 else None
    rng = random.Random(3)
    cs, bx, undefined = [], [], 0
    for _ in range(BOOT):
        ix = rng.choices(range(len(bugs)), k=len(bugs))  # one resample of diffs for every arm
        m = lambda v: mean(v[i] for i in ix)
        gap = m(B) - m(F)
        bx.append(m(B) - m(X))
        if gap:
            cs.append((m(D) - m(F)) / gap)
        else:
            undefined += 1
    consistency = {}
    for cond_g, cond_f, name in (("bifrost", "bifrost", "F_bifrost"), ("blind", "blind", "B_blind"),
                                 ("no-brief", "no-brief", "N_no_brief")):
        rg, rf = mean(get(g, cond_g)), mean(get(f, cond_f))
        consistency[name] = {"round_g": rg, "round_f": rf, "difference_points": round(100 * (rg - rf), 1),
                             "differs_over_10_points": abs(rg - rf) > 0.10}
    return {"catch_rate": {"F_bifrost": mean(F), "B_blind": mean(B), "D_bifrost_plus": mean(D),
                           "X_blind_minus": mean(X), "N_no_brief": mean(N)},
            "G0_B_minus_F": g0, "closure_c": closure,
            "closure_ci95": interval(cs) if cs else None, "closure_resamples_undefined": undefined,
            "B_minus_X": mean(B) - mean(X), "B_minus_X_ci95": interval(bx),
            "judge_consistency_vs_round_f": consistency,
            "per_diff": {b: {"F": F[i], "B": B[i], "D": D[i], "X": X[i], "N": N[i]}
                         for i, b in enumerate(bugs)}}


def readout(round_name):
    bugs = diffs()
    saved = json.loads((ROOT / "private" / f"key-{round_name}.json").read_text())
    assert sorted(saved.get("bugs", [])) == sorted(bugs), "round key does not cover exactly the plan's diffs"
    g, f = rates(round_name, set(bugs)), rates("brief-judge-f", set(bugs))
    per_model = {m: model_readout(m, bugs, g, f) for m in MODELS}
    t = per_model[PRIMARY]
    c = t["closure_c"]
    if c is None:
        outcome = "undefined: Terra/high B equals F on these diffs"
    elif c >= 0.6:
        outcome = "the relation carries it"
    elif c <= 0.25:
        outcome = "selection and framing carry it"
    else:
        outcome = "both matter"
    caveats = [f"{m} {k}: round G {v['round_g']:.3f} vs round F {v['round_f']:.3f}"
               for m, r in per_model.items() for k, v in r["judge_consistency_vs_round_f"].items()
               if v["differs_over_10_points"]]
    report = {"round": round_name, "diffs": bugs, "primary_model": PRIMARY, "outcome": outcome,
              "blind_brief_depends_on_item_alone": t["B_minus_X"] >= 0.10,
              "judge_consistency_caveats": caveats, "per_model": per_model}
    save(ROOT / round_name / "ablation.json", report)
    print(json.dumps(report, indent=1))


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("step", choices=["jobs", "minus", "editor-items", "plus", "check-items", "readout"])
    p.add_argument("path", nargs="?", help="plus: edited.jsonl")
    p.add_argument("--replace", action="append", default=[], help="plus: diff whose existing brief is overwritten")
    p.add_argument("--out", type=pathlib.Path, default=ROOT / "briefs/bifrost-plus", help="plus: output directory")
    p.add_argument("--round", help="readout: the judging round of the ablation (brief-judge-g)")
    a = p.parse_args()
    if a.step == "plus":
        plus(a.path, a.replace, a.out)
    elif a.step == "readout":
        readout(a.round)
    else:
        {"jobs": jobs, "minus": minus, "editor-items": editor_items, "check-items": check_items}[a.step]()
