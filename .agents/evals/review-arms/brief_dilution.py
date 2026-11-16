"""Tooling for the relation-brief dilution arm (brief/PLAN-dilution.md).

Usage (REVIEW_ARMS_DIR holds the data):
  python3 brief_dilution.py jobs          # dilution-jobs.json: the plan's 10 diffs x arms WT,WS
  python3 brief_dilution.py editor-items  # briefs/bifrost-diluted-work/<diff>/{item.json,INSTRUCTIONS.md}
  (run the editors: verify_driver.py briefs/bifrost-diluted-work --kind task --output siblings.jsonl ...)
  python3 brief_dilution.py form-check    # form.jsonl + form-summary.json in the work dir; ids <diff>:<n>
  python3 brief_dilution.py truth-items   # briefs/bifrost-diluted-check/truth/<diff>[--NN]/ blind verifier units
  (run them with --kind task --output truth.jsonl on briefs/bifrost-diluted-check/truth)
  python3 brief_dilution.py dup-items     # briefs/bifrost-diluted-check/dup/<diff>[--NN]/ duplicate-judge units
  (run them with --kind task --output dup.jsonl on briefs/bifrost-diluted-check/dup)
  python3 brief_dilution.py build [--drop ID=STEP ...] [--allow-false-share DIFF ...]
      arm D's brief plus the kept siblings -> briefs/bifrost-diluted/<diff>/{brief.md,meta.json}, summary.json
  python3 brief_dilution.py check-items   # briefs/bifrost-diluted-check/{contains,leak}/, bound to brief hashes
  (judge the checks; a leak is traced to its sibling and rebuilt with --drop ID=leak, then re-checked)
  python3 brief_dilution.py readout --round brief-judge-h
      W, D, F rates (round G's D, F, B beside them), retention r = (W - F) / (D - F), W - F, W - D, B - W,
      paired bootstrap intervals and the plan's outcome -> <round>/dilution.json
"""
import argparse
import collections
import json
import pathlib
import random
import statistics
import time

from brief_ablation import BOOT, MODELS, PRIMARY, diffs, final_labels, insert, interval, rates, sha
from driver import ROOT, SCRIPTS, save

ARM_KINDS = {"WT": "bifrost-diluted", "WS": "bifrost-diluted"}
WORK = ROOT / "briefs/bifrost-diluted-work"
CHECK = ROOT / "briefs/bifrost-diluted-check"
OUT = ROOT / "briefs/bifrost-diluted"
SECTIONS = ("## Affected callers", "## Paths")
FALSE_SHARE = 0.20  # the plan's step 3


def jobs():
    save(ROOT / "dilution-jobs.json", [{"bug": b, "arm": a} for b in diffs() for a in ARM_KINDS])
    print(f"wrote {ROOT / 'dilution-jobs.json'}: {len(diffs())} diffs x {len(ARM_KINDS)} arms")


def commits(bug):
    work = json.loads((ROOT / f"briefs/oracle-work/item-{bug}.json").read_text())
    return work["intro"], work["base"]


def bifrost_brief(bug):
    return (ROOT / "briefs/bifrost" / bug / "brief.md").read_text()


EDITOR = """# Editor: every sibling fact of a Bifrost brief

A reviewer is shown a brief of relations between a code change and the rest of
the repository. The brief below (tool-generated) lists some of them. You add
the true facts of six kinds about the sites in scope, in the brief's own form,
and change nothing else. You do not know of any defect in the change and must
not look for one: list facts, do not judge them.

## Input

`item.json` in this directory: `intro` and `base` (the commit with the change
and its parent), `bifrost_brief` (the brief text, as reviewers see it). Read the
repository in your working directory (`git show <intro>`, `git show
<intro>:<path>`, `git diff <base> <intro>`). Do not build, run tests, edit
files, or use the network.

## Scope

Every site inside the functions whose bodies the diff adds or changes, at
`intro`, and each call site the brief cites (the `file:line` of an edge in
`## Affected callers` or `## Paths`; for an unchanged caller only that call
site, not the rest of its body). There is no budget: list every fact in scope.

## The six kinds

1. **Argument values at call sites:** a literal, constant, `None`, default, or
   a value computed from one in the same function, passed for a named parameter.
2. **Field and state writes:** the field, static, map entry, file or table row a
   function writes, with the value written and the branch it runs under.
3. **Held scopes at a call:** a lock guard, transaction or similar scope live at
   a call site; the locks and transactions a function acquires.
4. **Keys and comparisons:** literal keys used to read or write maps,
   configuration, caches or tables; the key columns of a table written; the
   values a cache check or condition compares.
5. **Lifetimes:** where a value is created relative to a loop, connection,
   request or process start (once, per connection, per call), and the counter
   or format that names it.
6. **Shared state:** the state (fields, statics, channels, files, tables) a
   changed function shares with another function the brief names, or that it
   shares none.

Give every kind that has sites in scope, for every site. A fact must be true at
`intro`; check each anchor with `git show`.

## Form rules

1. One fact per item; one line starting with `- `. A compound fact may take two
   consecutive items. Qualified names and `file:line` anchors in backticks, in
   the style of the brief's existing items (for example
   ``- `crate_name.module.Type.method` (`path/to/file.rs:123`) ...``).
2. End the item with the same completeness tag as the neighbouring items (for
   example `[incomplete: `semantic_analysis`]`), whatever they carry.
3. No item longer than the brief's longest existing `- ` item line.
4. No judgment words (bug, wrong, missing, should, risk, unsafe, issue,
   problem, incorrect, broken, fail to, forget, or similar) and no consequence:
   no "so ...", "which means ...", "cannot", "never reaches", "is lost". State
   only plain relations between code locations. Do not rank facts or mark any as
   important; all look alike.
5. Place each item in `## Affected callers` or `## Paths`, never under
   `## Value flow` (which stays "Not computed"), after the existing brief line
   it belongs to in diff order.

## Output

Write `siblings.jsonl` in your working directory, one JSON object per fact:
{"id": "<diff>", "section": "## Affected callers" or "## Paths",
"after_line": "<an existing line of bifrost_brief in that section, verbatim,
or null for right after the heading>", "kind": <1 to 6>, "item": "- the new
line"}. `after_line` must occur exactly once in that section of `bifrost_brief`
and must be a line of `bifrost_brief`, never one of your own items. Several
items may share an `after_line`; they appear in the brief in the order of your
lines. Hand back the count of items written, per kind.
"""

TRUTH = """# Verifier: are these facts true?

Each fact in `siblings.json` is one line that will be shown to a code reviewer
in a brief about a change. Decide for each whether it is true of the code at the
introducing commit. You know nothing else about the change, and you must not
look for anything else: only check these facts.

## Input

`siblings.json` in this directory: `intro` and `base` (the commit with the
change and its parent) and `siblings`, a list of {"sibling_id", "item"}. Read
the repository in your working directory with `git show <intro>:<path>` (the
`file:line` anchors in the items refer to `<intro>`), and `git diff <base>
<intro>` where an item concerns what the change adds. Do not build, run tests,
edit files, or use the network.

## Labels

- `true`: every claim in the item holds at `intro`: the named functions and
  lines exist, and the relation, value, key, scope, lifetime or shared state
  stated is what the code has.
- `false`: some claim does not hold (wrong line or name, wrong value, a scope
  not live, a field not written, and so on).
- `unclear`: you cannot decide from the code at `intro`.

A completeness tag such as `[incomplete: ...]` is not a claim; ignore it.

## Output

Write `truth.jsonl` in your working directory, one JSON object per sibling:
{"sibling_id": ..., "label": "true"|"false"|"unclear", "note": "one sentence
naming what you checked". Hand back the counts per label.
"""

DUP = """# Do these siblings state the decisive fact?

Context: for each diff, a census recorded one decisive fact. A brief states that
fact once already. The sibling items below are meant to be other facts. Decide
whether any sibling states the decisive fact or part of it, using the rule
below, which was written for whole briefs.

%s

## Input

`census-<diff>.json` per diff: `intro`, `base`, `census` (the decisive `fact`,
its `evidence`, `other_facts`, `kind`). `siblings.json`: {"<diff>": [{"sibling_id",
"item"}]}, the items to judge. Read the repository in your working directory
as needed (`git show <intro>:<path>`).

## Labels

For each sibling, using the rule's labels for one item instead of a brief:

- `yes`: the sibling states the same relation between the same locations (or the
  same functions) as the decisive fact, even in other words.
- `partial`: the sibling anchors the decisive location but states a different or
  weaker relation, so a reader would not learn the decisive fact from it.
- `no`: otherwise.

## Output

Write `dup.jsonl` in your working directory, one JSON object per sibling:
{"sibling_id": ..., "label": "yes"|"partial"|"no", "rationale": "one or two
sentences"}. Hand back the counts per label.
"""


def editor_items():
    for bug in diffs():
        intro, base = commits(bug)
        d = WORK / bug
        d.mkdir(parents=True, exist_ok=True)
        save(d / "item.json", {"id": bug, "intro": intro, "base": base, "bifrost_brief": bifrost_brief(bug)})
        (d / "INSTRUCTIONS.md").write_text(EDITOR)
    print(f"wrote {len(diffs())} editor units in {WORK}")


def sections(brief):
    """section heading -> its lines, for the brief's own sections."""
    out, cur = {}, None
    for line in brief.split("\n"):
        if line.startswith("## "):
            cur = line.rstrip()
            out[cur] = []
        elif cur:
            out[cur].append(line.rstrip())
    return out


def read_siblings(bug):
    """(rows, problems): the rows of siblings.jsonl with ids <diff>:<n>, n counting non-blank lines from 1."""
    path = WORK / bug / "siblings.jsonl"
    if not path.exists():
        raise SystemExit(f"{path} is missing: run the editor for {bug}")
    rows = []
    for line in path.read_text().splitlines():
        if not line.strip():
            continue
        n = len(rows) + 1
        try:
            r = json.loads(line)
            if not isinstance(r, dict):
                raise ValueError("not an object")
        except ValueError as e:
            r = {"_malformed": str(e)}
        rows.append({**r, "sibling_id": f"{bug}:{n}"})
    return rows


def form_problem(r, brief, secs, longest):
    if "_malformed" in r:
        return f"malformed JSON line: {r['_malformed']}"
    item = r.get("item")
    if not isinstance(item, str) or "\n" in item.strip() or not item.strip().startswith("- "):
        return "item is not one line starting with '- '"
    if len(item.strip()) > longest:
        return f"item is {len(item.strip())} characters; the longest existing item is {longest}"
    if r.get("section") not in SECTIONS:
        return f"section {r.get('section')!r} is not Affected callers or Paths"
    if r.get("kind") not in range(1, 7):
        return f"kind {r.get('kind')!r} is not 1 to 6"
    after = r.get("after_line", "missing")
    if after != "missing" and after is not None and not isinstance(after, str):
        return f"after_line {after!r} is not a line of the brief or null"
    if after != "missing" and after is not None:
        hits = [l for l in secs.get(r["section"], []) if l == after.rstrip()]
        if len(hits) != 1:
            return f"after_line occurs {len(hits)} times in {r['section']}"
    elif after == "missing":
        return "after_line is missing"
    elif r["section"] not in secs:
        return f"brief has no {r['section']}"
    return None


def form_check():
    rows_out, summary = [], {}
    for bug in diffs():
        brief = bifrost_brief(bug)
        secs = sections(brief)
        longest = max(len(l.rstrip()) for l in brief.split("\n") if l.startswith("- "))
        rows = read_siblings(bug)
        by_kind = collections.Counter()
        failed = []
        for r in rows:
            reason = form_problem(r, brief, secs, longest)
            # Bound to the row's content: sibling ids are reused when the editor is rerun.
            rows_out.append({"id": r["sibling_id"], "ok": reason is None, "reason": reason,
                             "row_sha256": row_sha(r)})
            by_kind[r.get("kind")] += reason is None
            if reason:
                failed.append({"id": r["sibling_id"], "reason": reason})
        summary[bug] = {"proposed": len(rows), "ok": len(rows) - len(failed), "failed": failed,
                        "ok_by_kind": dict(sorted(by_kind.items(), key=lambda kv: str(kv[0]))),
                        "kinds_missing": sorted(set(range(1, 7)) - set(by_kind)),
                        "longest_item": longest}
    with open(WORK / "form.jsonl", "w") as f:
        for r in rows_out:
            f.write(json.dumps(r) + "\n")
    save(WORK / "form-summary.json", summary)
    for bug, s in summary.items():
        print(f"{bug}: {s['proposed']} proposed, {s['ok']} ok, {len(s['failed'])} failed, "
              f"kinds without an ok sibling: {s['kinds_missing']}")


def load_jsonl(path):
    return [json.loads(l) for l in open(path) if l.strip()]


def row_sha(r):
    """Hash of a siblings.jsonl row as the editor wrote it (without the id the reader assigns)."""
    return sha(json.dumps({k: v for k, v in r.items() if k != "sibling_id"}, sort_keys=True))


def form_ok_siblings(bug):
    """Rows of the diff whose form check passed. form.jsonl must have checked this exact row text;
    a row the editor rewrote under a reused id counts as unchecked."""
    checked = {r["id"]: r for r in load_jsonl(WORK / "form.jsonl")}
    rows = read_siblings(bug)
    stale = [r["sibling_id"] for r in rows
             if checked.get(r["sibling_id"], {}).get("row_sha256") != row_sha(r)]
    if stale:
        raise SystemExit(f"form.jsonl did not check the current text of {stale}: rerun form-check")
    return [r for r in rows if checked[r["sibling_id"]]["ok"]]


# One verifier checks at most this many siblings: the largest list of the other nine diffs is 174,
# so a long list (symbol-change-keeps-cached-transcript proposed 1,550) is split into units of
# that size and no verifier carries more than the others do.
TRUTH_UNIT = 175


def write_unit_siblings(unit, payload, labels):
    """Write a verifier unit's siblings.json. When its content changes, its old labels file and
    verify_driver's records for the unit (out/task-<unit>.json, sessions/batch-<unit>.json) move to
    stale/: sibling ids are reused across editor reruns, so labels written for an earlier list
    would otherwise be read as labels for the new text, and verify_driver would skip the unit."""
    old = unit / "siblings.json"
    if old.exists() and json.loads(old.read_text()) != payload:
        stale = unit.parent / "stale" / f"{unit.name}-{time.strftime('%Y%m%dT%H%M%S')}"
        stale.mkdir(parents=True, exist_ok=True)
        for f in (old, unit / labels, unit.parent / "out" / f"task-{unit.name}.json",
                  unit.parent / "sessions" / f"batch-{unit.name}.json"):
            if f.exists():
                f.rename(stale / f.name)
    unit.mkdir(parents=True, exist_ok=True)
    save(old, payload)


def retire_other_units(root, bug, names):
    """Move this diff's unit directories that the current split does not produce (a list that
    changed length changes the unit count) to stale/, so read_truth and read_dup never read
    labels from an earlier split beside the current ones."""
    for d in sorted(root.glob(f"{bug}*")):
        if d.is_dir() and (d.name == bug or d.name.startswith(f"{bug}--")) and d.name not in names:
            stale = root / "stale"
            stale.mkdir(parents=True, exist_ok=True)
            d.rename(stale / f"{d.name}-{time.strftime('%Y%m%dT%H%M%S')}")
            for f in (root / "out" / f"task-{d.name}.json", root / "sessions" / f"batch-{d.name}.json"):
                if f.exists():
                    f.rename(stale / f"{f.stem}-{time.strftime('%Y%m%dT%H%M%S')}{f.suffix}")


def truth_units(bug):
    return sorted(d for d in (CHECK / "truth").glob(f"{bug}*")
                  if d.is_dir() and (d.name == bug or d.name.startswith(f"{bug}--")))


def truth_items():
    for bug in diffs():
        rows = form_ok_siblings(bug)
        if not rows:
            print(f"{bug}: no form-ok sibling; no truth unit")
            continue
        intro, base = commits(bug)
        chunks = [rows[i:i + TRUTH_UNIT] for i in range(0, len(rows), TRUTH_UNIT)]
        names = [bug if len(chunks) == 1 else f"{bug}--{n:02d}" for n in range(len(chunks))]
        retire_other_units(CHECK / "truth", bug, names)
        for n, chunk in enumerate(chunks):
            d = CHECK / "truth" / names[n]
            d.mkdir(parents=True, exist_ok=True)
            write_unit_siblings(d, {"intro": intro, "base": base,
                                    "siblings": [{"sibling_id": r["sibling_id"], "item": r["item"].strip()}
                                                 for r in chunk]}, "truth.jsonl")
            (d / "INSTRUCTIONS.md").write_text(TRUTH)
        print(f"{bug}: {len(rows)} siblings to verify in {len(chunks)} unit(s)")


def read_truth(bug, rows):
    """sibling_id -> label for the diff's form-ok rows; every row needs exactly one label."""
    if not rows:
        return {}
    units = truth_units(bug)
    missing = [str(u / "truth.jsonl") for u in units if not (u / "truth.jsonl").exists()]
    if not units or missing:
        raise SystemExit(f"truth labels missing for {bug}: {missing or 'no unit'}; run the truth verifier")
    path = f"{CHECK / 'truth'}/{bug}[--NN]/truth.jsonl"
    got = collections.defaultdict(list)
    for u in units:
        for r in load_jsonl(u / "truth.jsonl"):
            got[r["sibling_id"]].append(r["label"])
    bad = [r["sibling_id"] for r in rows if len(got[r["sibling_id"]]) != 1
           or got[r["sibling_id"]][0] not in ("true", "false", "unclear")]
    if bad or set(got) - {r["sibling_id"] for r in rows}:
        raise SystemExit(f"{path}: need exactly one true/false/unclear label per sibling; bad: {bad}, "
                         f"extra: {sorted(set(got) - {r['sibling_id'] for r in rows})}")
    return {k: v[0] for k, v in got.items()}


def dup_items():
    """One duplicate-judge unit per diff, split like the truth units (TRUTH_UNIT siblings each)."""
    root = CHECK / "dup"
    root.mkdir(parents=True, exist_ok=True)
    rule = (ROOT / "briefs/bifrost-contains/CONTAINS.md").read_text().strip().replace("\n# ", "\n## ")
    total = 0
    for bug in diffs():
        rows = form_ok_siblings(bug)
        truth = read_truth(bug, rows)
        sibs = [{"sibling_id": r["sibling_id"], "item": r["item"].strip()}
                for r in rows if truth[r["sibling_id"]] != "false"]
        item = json.loads((ROOT / f"briefs/bifrost-contains/item-{bug}.json").read_text())
        item.pop("brief", None)
        chunks = [sibs[i:i + TRUTH_UNIT] for i in range(0, len(sibs), TRUTH_UNIT)] or [[]]
        names = [bug if len(chunks) == 1 else f"{bug}--{n:02d}" for n in range(len(chunks))]
        retire_other_units(root, bug, names)
        for n, chunk in enumerate(chunks):
            d = root / names[n]
            d.mkdir(parents=True, exist_ok=True)
            save(d / f"census-{bug}.json", item)
            write_unit_siblings(d, {bug: chunk}, "dup.jsonl")
            (d / "INSTRUCTIONS.md").write_text(DUP % rule)
        total += len(sibs)
        print(f"{bug}: {len(sibs)} siblings in {len(chunks)} unit(s)")
    print(f"wrote {root}: {total} siblings")


def read_dup(bug, kept):
    if not kept:
        return {}
    units = sorted(d for d in (CHECK / "dup").glob(f"{bug}*")
                   if d.is_dir() and (d.name == bug or d.name.startswith(f"{bug}--")))
    missing = [str(u / "dup.jsonl") for u in units if not (u / "dup.jsonl").exists()]
    if not units or missing:
        raise SystemExit(f"duplicate labels missing for {bug}: {missing or 'no unit'}; run the duplicate judge")
    got = collections.defaultdict(list)
    for u in units:
        for r in load_jsonl(u / "dup.jsonl"):
            got[r["sibling_id"]].append(r["label"])
    bad = [r["sibling_id"] for r in kept if len(got[r["sibling_id"]]) != 1
           or got[r["sibling_id"]][0] not in ("yes", "partial", "no")]
    if bad:
        raise SystemExit(f"{bug}: need exactly one yes/partial/no label per surviving sibling; bad: {bad}")
    return {k: v[0] for k, v in got.items()}


def place(d_lines, d_texts, by_anchor, seed_text):
    """Insert siblings into arm D's lines. by_anchor: (section, after_line|None) -> sibling items in file order.
    Arm D's block (the run of its own items right after the anchor line) goes at a seeded position among them.
    Returns (lines, positions) where positions records each anchor's seed and block position."""
    seed = int(sha(seed_text)[:8], 16)
    rng = random.Random(seed)
    heads = [i for i, l in enumerate(d_lines) if l.startswith("## ")]
    marks = [(l, False) for l in d_lines]
    ops = []
    for (section, after), items in by_anchor.items():
        at = [i for i, l in enumerate(d_lines) if l.rstrip() == section]
        if len(at) != 1:
            raise SystemExit(f"section {section!r} occurs {len(at)} times in arm D's brief")
        end = next((h for h in heads if h > at[0]), len(d_lines))
        if after is None:
            idx = at[0]
        else:
            hits = [i for i in range(at[0] + 1, end) if d_lines[i].rstrip() == after.rstrip()]
            if len(hits) != 1:
                raise SystemExit(f"after_line occurs {len(hits)} times in {section!r} of arm D's brief")
            idx = hits[0]
        block = 0
        while idx + 1 + block < end and d_lines[idx + 1 + block].strip() in d_texts:
            block += 1
        pos = rng.randint(0, len(items)) if block else None
        ops.append((idx, block, items, pos, section, after))
    positions = []
    for idx, block, items, pos, section, after in sorted(ops, key=lambda o: -o[0]):
        dblock = marks[idx + 1:idx + 1 + block]
        new = [(i, True) for i in items]
        cut = len(items) if pos is None else pos
        marks[idx + 1:idx + 1 + block] = new[:cut] + dblock + new[cut:]
        positions.append({"section": section, "after_line": after, "siblings": len(items),
                          "arm_d_items_here": block, "arm_d_position": pos})
    return marks, {"seed": seed, "anchors": positions}


def build(drops, allow_false):
    OUT.mkdir(parents=True, exist_ok=True)
    dropped_by = {}
    for spec in drops:
        sid, _, step = spec.partition("=")  # ids contain a colon
        dropped_by[sid] = step or "leak"
    summary, recall = {}, []
    known = {r["sibling_id"] for bug in diffs() for r in read_siblings(bug)}
    if set(dropped_by) - known:
        raise SystemExit(f"--drop names unknown siblings: {sorted(set(dropped_by) - known)}")
    for bug in diffs():
        d_dir = ROOT / "briefs/bifrost-plus" / bug
        d_text = (d_dir / "brief.md").read_text()
        d_meta = json.loads((d_dir / "meta.json").read_text())
        d_texts = {i["item"].strip() for i in d_meta["inserted"]}
        rows = form_ok_siblings(bug)
        all_rows = read_siblings(bug)
        truth = read_truth(bug, rows)
        false = [r for r in rows if truth[r["sibling_id"]] == "false"]
        if rows and len(false) / len(rows) > FALSE_SHARE and bug not in allow_false:
            raise SystemExit(f"{bug}: {len(false)} of {len(rows)} siblings are false (> {FALSE_SHARE:.0%}); the plan "
                             f"sends the list back to the editor once (or give --allow-false-share {bug})")
        survivors = [r for r in rows if truth[r["sibling_id"]] != "false"]
        dup = read_dup(bug, survivors)
        step_of = {}
        for r in all_rows:
            sid = r["sibling_id"]
            if r not in rows:
                step_of[sid] = "form"
            elif truth[sid] == "false":
                step_of[sid] = "truth"
            elif dup[sid] == "yes":
                step_of[sid] = "dup"
            elif sid in dropped_by:
                step_of[sid] = dropped_by[sid]
        kept = [r for r in all_rows if r["sibling_id"] not in step_of]
        recall.append(any(dup[r["sibling_id"]] in ("yes", "partial") for r in survivors))
        by_anchor = collections.defaultdict(list)
        for r in kept:
            by_anchor[(r["section"], r["after_line"])].append(r["item"].strip())
        marks, placement = place(d_text.split("\n"), d_texts, by_anchor, bug)
        result = "\n".join(l for l, _ in marks)
        # Everything that is not a sibling is arm D's brief, line for line.
        assert [l for l, s in marks if not s] == d_text.split("\n"), f"{bug}: arm D's lines changed"
        out = OUT / bug
        out.mkdir(parents=True, exist_ok=True)
        (out / "brief.md").write_text(result)
        meta = {"source": str(d_dir / "brief.md"), "source_sha256": sha(d_text),
                "arm_d_inserted": d_meta["inserted"], "arm_d_meta_result_sha256": d_meta["result_sha256"],
                "bifrost_sha256": d_meta["source_sha256"],
                "kept": [{"sibling_id": r["sibling_id"], "kind": r["kind"], "section": r["section"],
                          "after_line": r["after_line"], "item": r["item"].strip(),
                          "truth": truth.get(r["sibling_id"]), "dup": dup.get(r["sibling_id"])} for r in kept],
                "dropped": [{"sibling_id": r["sibling_id"], "step": step_of[r["sibling_id"]],
                             "kind": r.get("kind"), "item": r.get("item")}
                            for r in all_rows if r["sibling_id"] in step_of],
                "counts": {"proposed": len(all_rows), "form_ok": len(rows), "truth_false": len(false),
                           "dup_yes": sum(1 for r in survivors if dup[r["sibling_id"]] == "yes"),
                           "dup_partial": sum(1 for r in survivors if dup[r["sibling_id"]] == "partial"),
                           "kept": len(kept)},
                "placement": placement,
                "lines": {"before": len(d_text.split("\n")), "after": len(result.split("\n"))},
                "bytes": {"before": len(d_text.encode()), "after": len(result.encode())},
                "result_sha256": sha(result)}
        save(out / "meta.json", meta)
        summary[bug] = {**meta["counts"], "lines": meta["lines"], "bytes": meta["bytes"]}
        print(f"{bug}: {json.dumps(summary[bug])}")
    save(OUT / "summary.json", {"per_diff": summary,
                                "editor_recall_of_decisive_fact": f"{sum(recall)} of {len(recall)} diffs have a "
                                                                  "yes or partial sibling (among form-ok, true-or-unclear siblings"})
    print(f"diffs with a yes or partial sibling: {sum(recall)} of {len(recall)}")


def check_items():
    contains, leak = CHECK / "contains", CHECK / "leak"
    for d in (contains, leak):
        d.mkdir(parents=True, exist_ok=True)
    (contains / "CONTAINS.md").write_text((ROOT / "briefs/bifrost-contains/CONTAINS.md").read_text())
    corpus = {r["id"]: r for r in map(json.loads, open(SCRIPTS / "corpus/corpus.jsonl"))}
    batch, key = [], {}
    for bug in diffs():
        brief = (OUT / bug / "brief.md").read_text()
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


def dilution_readout(model, bugs, h, g):
    """W, D, F from round H (h); round G's D, F, B (g). Rates are catch shares per diff, mean over diffs."""
    get = lambda r, cond: [r[(cond, model)][b] for b in bugs]
    W, D, F = get(h, "bifrost-diluted"), get(h, "bifrost-plus"), get(h, "bifrost")
    Dg, Fg, B = get(g, "bifrost-plus"), get(g, "bifrost"), get(g, "blind")
    mean = statistics.fmean
    ratio = lambda w, d, f: (w - f) / (d - f) if d != f else None
    r = ratio(mean(W), mean(D), mean(F))
    rng = random.Random(3)
    rs, wf, wd, bw, undefined = [], [], [], [], 0
    for _ in range(BOOT):
        ix = rng.choices(range(len(bugs)), k=len(bugs))  # one resample of diffs for every arm
        m = lambda v: mean(v[i] for i in ix)
        wf.append(m(W) - m(F))
        wd.append(m(W) - m(D))
        bw.append(m(B) - m(W))
        q = ratio(m(W), m(D), m(F))
        if q is None:
            undefined += 1
        else:
            rs.append(q)
    consistency = {}
    for name, hv, gv in (("D_bifrost_plus", D, Dg), ("F_bifrost", F, Fg)):
        consistency[name] = {"round_h": mean(hv), "round_g": mean(gv),
                             "difference_points": round(100 * (mean(hv) - mean(gv)), 1),
                             "differs_over_10_points": abs(mean(hv) - mean(gv)) > 0.10}
    return {"catch_rate": {"W_diluted": mean(W), "D_bifrost_plus": mean(D), "F_bifrost": mean(F),
                           "round_g": {"D_bifrost_plus": mean(Dg), "F_bifrost": mean(Fg), "B_blind": mean(B)}},
            "retention_r": r, "retention_ci95": interval(rs) if rs else None, "retention_resamples_undefined": undefined,
            "W_minus_F": mean(W) - mean(F), "W_minus_F_ci95": interval(wf),
            "W_minus_D": mean(W) - mean(D), "W_minus_D_ci95": interval(wd),
            "B_minus_W": mean(B) - mean(W), "B_minus_W_ci95": interval(bw),
            "judge_consistency_vs_round_g": consistency,
            "per_diff": {b: {"W": W[i], "D": D[i], "F": F[i], "D_round_g": Dg[i], "F_round_g": Fg[i], "B_round_g": B[i]}
                         for i, b in enumerate(bugs)}}


def outcome_of(r):
    if r is None:
        return "undefined: Terra/high D equals F on these diffs"
    if r >= 0.7:
        return "selection is not the bottleneck"
    if r <= 0.3:
        return "selection is needed"
    return "partly diluted"


def readout(round_name):
    saved = json.loads((ROOT / "private" / f"key-{round_name}.json").read_text())
    # The round judges the diffs whose W brief reached reviewers; symbol-change-keeps-cached-transcript's
    # 300 KB brief was refused by the mj API (413), so every arm is compared on the same 9 diffs.
    bugs = [b for b in diffs() if b in set(saved.get("bugs", []))]
    excluded = [b for b in diffs() if b not in bugs]
    assert len(bugs) + len(excluded) == len(diffs()) and bugs, "round key names diffs outside the plan"
    h, g = rates(round_name, set(bugs)), rates("brief-judge-g", set(bugs))
    per_model = {m: dilution_readout(m, bugs, h, g) for m in MODELS}
    caveats = [f"{m} {k}: round H {v['round_h']:.3f} vs round G {v['round_g']:.3f}"
               for m, r in per_model.items() for k, v in r["judge_consistency_vs_round_g"].items()
               if v["differs_over_10_points"]]
    report = {"round": round_name, "diffs": bugs, "excluded_diffs": excluded, "primary_model": PRIMARY,
              "outcome": outcome_of(per_model[PRIMARY]["retention_r"]),
              "judge_consistency_caveats": caveats, "per_model": per_model}
    save(ROOT / round_name / "dilution.json", report)
    print(json.dumps(report, indent=1))


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("step", choices=["jobs", "editor-items", "form-check", "truth-items", "dup-items", "build",
                                    "check-items", "readout"])
    p.add_argument("--drop", action="append", default=[], help="build: SIBLING_ID[=STEP] to drop (STEP defaults to leak)")
    p.add_argument("--allow-false-share", action="append", default=[],
                   help="build: diff whose false share exceeds 20% and is accepted")
    p.add_argument("--round", help="readout: the judging round of the dilution arm (brief-judge-h)")
    a = p.parse_args()
    if a.step == "build":
        build(a.drop, set(a.allow_false_share))
    elif a.step == "readout":
        readout(a.round)
    else:
        {"jobs": jobs, "editor-items": editor_items, "form-check": form_check, "truth-items": truth_items,
         "dup-items": dup_items, "check-items": check_items}[a.step]()
