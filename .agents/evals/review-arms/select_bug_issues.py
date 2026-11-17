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

from cutoff_dates import utc_datetime

SEED = 20261117
p = argparse.ArgumentParser()
p.add_argument("repo")
p.add_argument("out")
p.add_argument("--before", type=utc_datetime, default="2026-11-01T00:00:00Z")
a = p.parse_args()
out = pathlib.Path(a.out)
out.mkdir(parents=True, exist_ok=True)

# `gh issue list --label` stops at 1,000 issues (seen 2026-11-16: the first draw lost every bug issue
# before 2026-08-17); the REST listing pages through all of them. It also lists pull requests.
pages = json.loads(subprocess.check_output(
    ["gh", "api", "--paginate", "--slurp", f"repos/{a.repo}/issues?labels=bug&state=closed&per_page=100"], text=True))
pool = sorted(
    ({"number": i["number"], "title": i["title"], "created_at": i["created_at"], "closed_at": i["closed_at"],
      "found_by": sorted(l["name"] for l in i["labels"] if l["name"].startswith("found-by:")) or None,
      "labels": sorted(l["name"] for l in i["labels"])}
     for page in pages for i in page
     if "pull_request" not in i and utc_datetime(i["created_at"]) < a.before and i["state_reason"] == "completed"),
    key=lambda i: i["number"])
order = list(pool)
random.Random(SEED).shuffle(order)
for name, rows in (("bug-issues-pool.jsonl", pool), ("bug-issues-order.jsonl", order)):
    text = "".join(json.dumps(r) + "\n" for r in rows)
    (out / name).write_text(text)
    print(f"{name}: {len(rows)} issues, sha256 {hashlib.sha256(text.encode()).hexdigest()}")
