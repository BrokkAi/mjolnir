"""Builds blind verification batches from every finished run.

Usage: python3 build_verify.py <out-dir> [--per-batch 2] [--ready-jobs jobs.json] [--append]

--ready-jobs includes only diffs whose every job in that file has finished, so
verification can start while other reviews still run. --append adds diffs not
yet in the key as new batches and leaves existing batches and IDs unchanged.

Splits each reviewer's handback into findings (one per "- [Pn]" bullet),
gives each a random ID, shuffles a diff's findings from all of its reviewer
sessions together, and writes <out-dir>/batch-<n>.json plus the private key
private/key-<out-dir name>.json (finding ID -> diff, reviewer session, model,
review, and every covered session). Verifiers see neither model nor review.
"""
import argparse
import json
import os
import pathlib
import random
import re
import secrets

ROOT = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))
p = argparse.ArgumentParser()
p.add_argument("out")
p.add_argument("--per-batch", type=int, default=2)
p.add_argument("--ready-jobs")
p.add_argument("--append", action="store_true")
a = p.parse_args()
out = ROOT / a.out
out.mkdir(parents=True, exist_ok=True)


def split(text):
    text = text.strip()
    if text.startswith("No findings"):
        return []
    parts = [s.strip() for s in re.split(r"(?m)^\s*(?=[-*]\s*\[P\d\])", text) if s.strip()]
    found = [s for s in parts if re.match(r"[-*]\s*\[P\d\]", s)]
    # Output that is neither "No findings." nor bullets is kept whole for the verifier.
    return found or [text]


private = ROOT / "private" / f"key-{out.name}.json"
private.parent.mkdir(parents=True, exist_ok=True)
previous = json.loads(private.read_text()) if a.append and private.exists() else {"findings": {}, "sessions": []}
assert a.append or not private.exists(), f"{private} exists; use --append"
batched = {k["bug"] for k in previous["findings"].values()}
batched |= {json.loads(d.read_text())["bug"] for d in ROOT.glob("runs/*/*/done.json")
            if any(r["child"] in set(previous["sessions"]) for r in json.loads(d.read_text())["reviewers"])}
ready = None
if a.ready_jobs:
    jobs = json.loads(pathlib.Path(a.ready_jobs).read_text())
    pending = {j["bug"] for j in jobs if not (ROOT / "runs" / j["bug"] / j["arm"] / "done.json").exists()}
    ready = {j["bug"] for j in jobs} - pending

by_diff, key, unparsed, covered = {}, {}, [], []
for done_path in sorted(ROOT.glob("runs/*/*/done.json")):
    done = json.loads(done_path.read_text())
    run = done_path.parent
    if done["bug"] in batched or (ready is not None and done["bug"] not in ready):
        continue
    for r in done["reviewers"]:
        covered.append(r["child"])
        text = (run / "reviewers" / r["child"][:12] / "output.md").read_text()
        findings = split(text)
        if findings and not re.match(r"[-*]\s*\[P\d\]", findings[0]):
            unparsed.append(r["child"])
        for n, f in enumerate(findings):
            fid = "f-" + secrets.token_hex(4)
            key[fid] = {"bug": done["bug"], "child": r["child"], "model": r["model"], "arm": done["arm"],
                        "slot": r["slot"], "index": n}
            by_diff.setdefault(done["bug"], []).append({"id": fid, "text": f})

corpus = {c["id"]: c for name in ("candidates-all.jsonl", "diffs.jsonl")
          for c in map(json.loads, (pathlib.Path(__file__).resolve().parent / "corpus" / name).read_text().splitlines())}
rng = random.Random(20261114)
diffs = sorted(by_diff)
rng.shuffle(diffs)
batches = [diffs[i:i + a.per_batch] for i in range(0, len(diffs), a.per_batch)]
first = 1 + max((int(f.stem.split("-")[1]) for f in out.glob("batch-*.json")), default=-1)
for n, members in enumerate(batches, start=first):
    batch = []
    for bug in members:
        items = by_diff[bug][:]
        rng.shuffle(items)
        batch.append({"intro": corpus[bug]["intro_commit"], "base": corpus[bug]["base_commit"], "findings": items})
    # Written under a temporary name; published only after the key that maps its IDs.
    (out / f"batch-{n}.json.tmp").write_text(json.dumps(batch, indent=1) + "\n")
# Sessions with no findings count as having found nothing; sessions that finish
# after this point are not covered and are left out of scoring.
tmp_key = private.with_name(private.name + ".tmp")
tmp_key.write_text(json.dumps({"findings": {**previous["findings"], **key},
                               "sessions": previous["sessions"] + covered}, indent=1) + "\n")
tmp_key.chmod(0o600)
tmp_key.replace(private)
for n in range(first, first + len(batches)):
    (out / f"batch-{n}.json.tmp").replace(out / f"batch-{n}.json")
print(json.dumps({"new_diffs": len(diffs), "new_findings": len(key), "new_batches": list(range(first, first + len(batches))),
                  "unparsed_outputs": unparsed}))
