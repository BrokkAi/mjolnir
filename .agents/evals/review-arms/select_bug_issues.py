"""Draws the seeded order of bug issues for a known-bug corpus (brief/PLAN-bifrost-bugs.md, amendment).

Usage: python3 select_bug_issues.py <owner/repo> <out-dir> [--before 2026-11-01T00:00:00Z]

Pool: issues labelled `bug`, closed as completed, created before the cutoff.
Issues closed as not planned or duplicate were never fixed, so they cannot
have a fix commit and are left out here; every other qualification check
(a fix that adds or changes a test pinning the defect, a confirmed intro
under corpus/SPEC.md) happens during tracing, in this order.

Writes <out-dir>/bug-issues-pool.jsonl (sorted by issue number) and
<out-dir>/bug-issues-order.jsonl (the pool in seeded random order), and
prints the pool size and the SHA-256 of both files. The seed was committed
before the first draw.
"""
import argparse
import hashlib
import json
import pathlib
import random
import subprocess

SEED = 20261117
p = argparse.ArgumentParser()
p.add_argument("repo")
p.add_argument("out")
p.add_argument("--before", default="2026-11-01T00:00:00Z")
a = p.parse_args()
out = pathlib.Path(a.out)
out.mkdir(parents=True, exist_ok=True)

# `gh issue list` without --search pages through the listing API, which has no 1,000-result cap.
raw = subprocess.check_output(
    ["gh", "issue", "list", "-R", a.repo, "--label", "bug", "--state", "closed", "-L", "100000",
     "--json", "number,title,createdAt,closedAt,stateReason,labels"], text=True)
pool = sorted(
    ({"number": i["number"], "title": i["title"], "created_at": i["createdAt"], "closed_at": i["closedAt"],
      "found_by": sorted(l["name"] for l in i["labels"] if l["name"].startswith("found-by:")) or None,
      "labels": sorted(l["name"] for l in i["labels"])}
     for i in json.loads(raw) if i["createdAt"] < a.before and i["stateReason"] == "COMPLETED"),
    key=lambda i: i["number"])
order = list(pool)
random.Random(SEED).shuffle(order)
for name, rows in (("bug-issues-pool.jsonl", pool), ("bug-issues-order.jsonl", order)):
    text = "".join(json.dumps(r) + "\n" for r in rows)
    (out / name).write_text(text)
    print(f"{name}: {len(rows)} issues, sha256 {hashlib.sha256(text.encode()).hexdigest()}")
