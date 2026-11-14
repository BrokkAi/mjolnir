"""Samples ordinary pre-cutoff mjolnir commits for the pooled-findings design.

Usage: python3 select_diffs.py <repo> <count> > corpus/diffs.jsonl

Eligible: non-merge commits on master before 2026-11-01 (no Terra or Sol
review before then), 80 to 1,500 changed lines not counting goldens,
fixtures, snapshots or lock files, at least 60% of those lines in code files,
not only tests, and not one of the Hard-won candidate commits. Sampled with a
fixed seed, spread evenly across the eligible history by date order.
"""
import json
import pathlib
import random
import re
import subprocess
import sys

repo, count = sys.argv[1], int(sys.argv[2])
CUTOFF = "2026-11-01T00:00:00"
EXCLUDED_PATH = re.compile(r"golden|fixtures?/|snapshots?/|\.snap$|Cargo\.lock$|package-lock\.json$|\.svg$|\.png$|testdata/|expected/")
CODE = re.compile(r"\.(rs|py|js|ts|tsx|sh|go|toml|sql)$")
TEST = re.compile(r"(^|/)tests?/|_tests?\.rs$|(^|/)tests\.rs$|_test\.py$|test_[^/]*\.py$")
here = pathlib.Path(__file__).resolve().parent
hard_won = {json.loads(l)["intro_commit"] for l in (here / "corpus/candidates-all.jsonl").read_text().splitlines()}

log = subprocess.check_output(
    ["git", "-C", repo, "log", "master", "--no-merges", f"--before={CUTOFF}", "--reverse",
     "--format=@@%H %P|%cI|%s", "--numstat"], text=True)
eligible = []
for block in log.split("@@")[1:]:
    head, *rows = block.strip("\n").split("\n")
    shas, date, subject = head.split("|", 2)
    sha, *parents = shas.split()
    if len(parents) != 1 or sha in hard_won:
        continue
    lines = code = tests = 0
    for row in rows:
        parts = row.split("\t")
        if len(parts) != 3 or parts[0] == "-" or EXCLUDED_PATH.search(parts[2]):
            continue
        n = int(parts[0]) + int(parts[1])
        lines += n
        if CODE.search(parts[2]):
            code += n
            tests += n if TEST.search(parts[2]) else 0
    if 80 <= lines <= 1500 and code >= 0.6 * lines and tests < code:
        eligible.append({"intro_commit": sha, "base_commit": parents[0], "intro_date": date[:10],
                         "subject": subject, "changed_lines": lines})

rng = random.Random(20261114)
# One pick from each of `count` equal slices of the date-ordered history.
size = len(eligible) / count
picks = [eligible[int(i * size) + rng.randrange(max(1, int(size)))] for i in range(count)]
for p in picks:
    p["id"] = "diff-" + p["intro_commit"][:10]
    p["source"] = "sampled"
    print(json.dumps(p))
print(f"{len(eligible)} eligible, {len(picks)} sampled", file=sys.stderr)
