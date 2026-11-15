"""Control 2 of brief/PLAN.md: do blind briefs cost findings or add false ones on ordinary diffs?

Usage: python3 brief_control2.py [--verify verify-brief] [--jobs stageB-ordinary-jobs.json]

Inputs under REVIEW_ARMS_DIR: the verify dir built with
`build_verify.py <verify> --arms A,BT --bugs-from <jobs>` and verified by
verify_driver.py, then scored by `score.py <verify>` (sessions.csv,
groups.csv, session_groups.csv); its private key; the batches (finding text);
and each diff's blind brief.

Reports, over the ordinary diffs:
- confirmed distinct problems per session, no-brief Terra/high (arm A)
  against blind-brief Terra/high (arm BT); control 2 needs BT >= 90% of A;
- rejected findings from BT that cite a brief item: a file:line in the
  finding within 3 lines of an anchor in that diff's brief that lies outside
  the changed lines; control 2 needs at most 1 per 5 diffs. The count that
  also matches anchors on changed lines is reported beside it: findings cite
  the diff's own lines whether or not they used the brief.
"""
import argparse
import collections
import csv
import json
import random
import re
import statistics

import brief_driver
import driver
from driver import ROOT, save

ANCHOR = re.compile(r"([A-Za-z0-9_./-]+\.[A-Za-z0-9]+):(\d+)(?:-(\d+))?")


def spans(text):
    """(path, first line, last line) for every file:line or file:a-b anchor in text."""
    return [(path, int(a), int(b or a)) for path, a, b in ANCHOR.findall(text)]

p = argparse.ArgumentParser()
p.add_argument("--verify", default="verify-brief")
p.add_argument("--jobs", default="stageB-ordinary-jobs.json")
a = p.parse_args()
V = ROOT / a.verify
diffs = sorted({j["bug"] for j in json.loads((ROOT / a.jobs).read_text())})
key = json.loads((ROOT / "private" / f"key-{V.name}.json").read_text())
findings, covered = key["findings"], set(key["sessions"])

# Sessions of both arms on every diff, with output or not (score.py's sessions.csv lists them).
sessions = {r["session"]: r for r in csv.DictReader(open(V / "sessions.csv"))}
arm_of = {}
for bug in diffs:
    for arm in ("A", "BT"):
        done = json.loads((ROOT / "runs" / bug / arm / "done.json").read_text())
        for r in done["reviewers"]:
            assert r["child"] in covered, f"{bug}/{arm}: session {r['child']} was not batched"
            arm_of[r["child"]] = (bug, arm)
# score.py leaves out every session of a diff whose findings are not all verified; scoring
# those sessions as zero would bias the means, so a partial verification is refused.
unscored = sorted(f"{bug}/{arm}/{s[:12]}" for s, (bug, arm) in arm_of.items() if s not in sessions)
if unscored:
    raise SystemExit(f"{len(unscored)} sessions are not in sessions.csv (unverified diffs?): {unscored[:6]}")
groups = {r["group"]: r for r in csv.DictReader(open(V / "groups.csv"))}
by_session = collections.defaultdict(set)
for r in csv.DictReader(open(V / "session_groups.csv")):
    by_session[r["session"]].add(r["group"])

per_diff = collections.defaultdict(lambda: {"A": [], "BT": []})
for s, (bug, arm) in arm_of.items():
    confirmed = sum(groups[g]["verdict"] == "confirmed" for g in by_session.get(s, ()))
    per_diff[bug][arm].append(confirmed)
mean = lambda arm: statistics.fmean(statistics.fmean(per_diff[b][arm]) for b in diffs)
diff_values = [statistics.fmean(per_diff[b]["BT"]) - statistics.fmean(per_diff[b]["A"]) for b in diffs]
rng = random.Random(3)
boot = sorted(statistics.fmean(rng.choices(diff_values, k=len(diff_values))) for _ in range(10000))

# Finding text from the batches, and each group's verdict per finding.
text = {f["id"]: f["text"] for path in V.glob("batch-*.json") for c in json.loads(path.read_text())
        for f in c["findings"]}
verdict_of = {}
for path in (V / "out").glob("verify-out-*.json"):
    for c in json.loads(path.read_text())["commits"]:
        for g in c["groups"]:
            for f in g["findings"]:
                verdict_of[f] = g["verdict"]
unverified = sorted(f for f in findings if f not in verdict_of)
if unverified:
    raise SystemExit(f"{len(unverified)} findings have no verdict yet: {unverified[:6]}")


def brief_anchors(bug, outside_only=True):
    """Anchors in the diff's blind brief. The decision counts only anchors outside the changed
    lines: a finding citing a changed line may have taken it from the diff, not the brief."""
    changed = brief_driver.changed_lines(driver.CORPUS[bug])
    brief = (ROOT / "briefs/blind" / bug / "brief.md").read_text()
    return [(path, a, b) for path, a, b in spans(brief)
            if not (outside_only and path in changed and set(range(a, b + 1)) <= changed[path])]


def cites(anchors, finding_text):
    # A citation counts when its range comes within 3 lines of a brief anchor's range.
    return any(p2 == p1 and a2 <= b1 + 3 and a1 <= b2 + 3
               for p1, a1, b1 in anchors for p2, a2, b2 in spans(finding_text))


citing, citing_any = [], []
for fid, k in findings.items():
    if k["arm"] != "BT" or verdict_of.get(fid) != "rejected":
        continue
    if cites(brief_anchors(k["bug"]), text[fid]):
        citing.append({"finding": fid, "bug": k["bug"], "text": text[fid][:300]})
    if cites(brief_anchors(k["bug"], outside_only=False), text[fid]):
        citing_any.append(fid)

a_mean, bt_mean = mean("A"), mean("BT")
report = {
    "diffs": len(diffs),
    "sessions": {arm: sum(len(per_diff[b][arm]) for b in diffs) for arm in ("A", "BT")},
    "confirmed_per_session": {"A": round(a_mean, 3), "BT": round(bt_mean, 3)},
    "ratio_BT_to_A": round(bt_mean / a_mean, 3) if a_mean else None,
    "difference_ci95": [round(boot[250], 3), round(boot[9750], 3)],
    "rejected_findings_citing_brief": len(citing),
    # Broader count for reference: also matches brief anchors on changed lines.
    "rejected_findings_citing_any_brief_anchor": len(citing_any),
    "limit_citing": len(diffs) / 5,
    "control_2_passes": bool(a_mean and bt_mean / a_mean >= 0.9 and len(citing) <= len(diffs) / 5),
    "citing": citing,
}
save(V / "control2.json", report)
print(json.dumps({k: v for k, v in report.items() if k != "citing"}, indent=1))
