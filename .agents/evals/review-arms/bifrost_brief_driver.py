"""Generates stage C briefs with Bifrost's relation_brief tool (brief/PLAN-bifrost.md).

Usage: python3 bifrost_brief_driver.py --bifrost <binary> --root <mjolnir checkout>
                                       [--cache <dir>] [<diff id> ...]

With no diff ids it covers the 30 Hard-won diffs (corpus/corpus.jsonl) and the
40 ordinary diffs (corpus/diffs.jsonl). Runs are serial (one Bifrost process at
a time; concurrent processes block on one cache's build lock) and share one
BIFROST_CACHE_ROOT. --root needs only to be a checkout of mjolnir that has the
commits: the tool reads the immutable base and target endpoints, not the live
worktree.

Writes $REVIEW_ARMS_DIR/briefs/bifrost/<diff>/:
  output.json  the tool's JSON envelope, as printed
  brief.md     structuredContent.brief, unchanged; an empty brief when the
               call failed (the plan counts failures as empty briefs)
  meta.json    version, binary, command, wall time, exit status, completeness,
               item counts, lint problems (recorded, never acted on)
A diff with meta.json is skipped, so an interrupted run resumes.
"""
import argparse
import json
import os
import pathlib
import subprocess
import sys
import time

import brief_driver
from driver import CORPUS, ROOT, SCRIPTS, save

# What a reviewer would receive when the tool produced nothing.
EMPTY_BRIEF = "## Affected callers\n- none\n\n## Paths\n- none\n\n## Value flow\n- none\n"


def run(diff, binary, checkout, cache, version):
    d = ROOT / "briefs" / "bifrost" / diff
    if (d / "meta.json").exists():
        return "done"
    d.mkdir(parents=True, exist_ok=True)
    bug = CORPUS[diff]
    args = json.dumps({"base": bug["base_commit"], "target": bug["intro_commit"]})
    command = [str(binary), "--root", str(checkout), "--tool", "relation_brief", "--args", args]
    start = time.monotonic()
    # Subprocess rule: output is drained by subprocess.run; nothing is written to stdin.
    proc = subprocess.run(command, capture_output=True, env={**os.environ, "BIFROST_CACHE_ROOT": str(cache)})
    seconds = round(time.monotonic() - start, 1)
    (d / "output.json").write_bytes(proc.stdout)
    (d / "stderr.txt").write_bytes(proc.stderr[-200_000:])
    brief, sc, error = None, {}, None
    try:
        envelope = json.loads(proc.stdout)
        sc = envelope.get("structuredContent") or {}
        brief = sc.get("brief")
        error = envelope.get("error")
    except json.JSONDecodeError as e:
        error = f"unparsed output: {e}"
    failed = proc.returncode != 0 or not isinstance(brief, str) or not brief.strip()
    text = EMPTY_BRIEF if failed else brief.rstrip() + "\n"
    (d / "brief.md").write_text(text)
    items = [l for l in text.splitlines() if l.strip().startswith("- ") and l.strip() != "- none"]
    save(d / "meta.json", {
        "diff": diff, "version": version, "binary": str(binary), "command": command,
        "cache": str(cache), "seconds": seconds, "exit_status": proc.returncode, "failed": failed,
        "error": error, "incomplete": sc.get("incomplete"),
        "sections": [{k: s.get(k) for k in ("title", "complete", "omitted_by_budget")} for s in sc.get("sections") or []],
        "items": len(items),
        "items_incomplete": sum("[incomplete" in l for l in items),
        "lint": [] if failed else brief_driver.lint(text, bug),
        "time_ns": time.time_ns(),
    })
    return "failed" if failed else "done"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--bifrost", required=True, type=pathlib.Path)
    p.add_argument("--root", required=True, type=pathlib.Path)
    p.add_argument("--cache", type=pathlib.Path, default=ROOT / "bifrost-cache")
    p.add_argument("diffs", nargs="*")
    a = p.parse_args()
    diffs = a.diffs or [json.loads(l)["id"] for name in ("corpus.jsonl", "diffs.jsonl")
                        for l in open(SCRIPTS / "corpus" / name)]
    a.cache.mkdir(parents=True, exist_ok=True)
    version = subprocess.run([str(a.bifrost), "--version"], capture_output=True, text=True).stdout.strip()
    failed = 0
    for diff in diffs:
        state = run(diff, a.bifrost.resolve(), a.root.resolve(), a.cache, version)
        failed += state == "failed"
        print(json.dumps({"diff": diff, "state": state}), flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
