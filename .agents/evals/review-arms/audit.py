"""Scans reviewer transcripts for access to later history or other forbidden sources.

Usage: python3 audit.py [runs/<bug>/<arm> ...]   (default: every finished run)
Writes audit.json in each run directory and prints a summary. A hit is not a
verdict: every hit is read by hand before a run is accepted or excluded.
"""
import json
import os
import pathlib
import re
import sys

ROOT = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))

# Where a command word can start. Inputs are matched as serialized JSON, so a
# command at the start of an argument follows a quote or an escaping backslash.
CMD = r"""(^|[\s;&|(`"'\\])"""
# Patterns checked against what the reviewer asked for: commands, tool names, arguments.
INPUT_PATTERNS = {
    "github-or-web": CMD + r"gh\s|github\.com|api\.github|\bcurl\b|\bwget\b|https?://|claude\.ai|web_search|web\.run|websearch|webfetch|search_query",
    "git-history-escape": r"git\b[^\n]*\b(fetch|pull|clone|remote|ls-remote|bundle|fsck|reflog)\b|git\b[^\n]*(log|rev-list|for-each-ref|show-ref)\b[^\n]*--all\b|cat-file\s+--batch-all|/run/hel/git-cache|\.bundle\b",
    "outside-workspace": r"/home/jonathan|/mnt/optane|/var/lib/hel|/home/hel/\.(codex|claude|cache|config|local)|\.codex/(sessions|history|archived)|" + CMD + r"(find|ls|du|tree)\s+/(\s|$|[\"'])|grep\s+-[a-zA-Z]*r[a-zA-Z]*\s+[^\n]*\s/(\s|$|[\"'])",
    "memory-or-sessions": r"search_sessions|read_session|search_session|get_session_brief|session_files|trace_file|blame_file|mj-memory|" + CMD + r"mj\s+(transcript|sessions|events|usage)|sqlite",
    "build-or-test": r"\bcargo(\s+[-+]\S*)*\s+(build|test|check|clippy|run|nextest|bench)\b|\bnpm\s+(test|run)\b|\bpytest\b|\bpython3?\s+-m\s+(unittest|pytest)\b|\bgo\s+test\b|"
                     + CMD + r"make\s+(-\S+\s+)*\S*(test|check)\b|"
                     + CMD + r"(python3?|bash|sh|expect)\s+(-\S+\s+)*[^\s\"']*(tests?|e2e|scripts)/|" + CMD + r"\./(tests?|scripts)/",
}
# Patterns checked against tool output: places that only exist outside the checkout.
OUTPUT_PATTERNS = {
    "output-outside-path": r"/run/hel/git-cache|/home/jonathan|/mnt/optane|/var/lib/hel",
}


def call_parts(item):
    body = item.get("body") or {}
    call = body.get("call") or {}
    if call:
        inp = json.dumps({"title": call.get("title"), "kind": call.get("kind"), "rawInput": call.get("rawInput")})
        out = json.dumps({"content": call.get("content"), "rawOutput": call.get("rawOutput")})
        return inp, out
    return json.dumps(body), ""


def audit_reviewer(transcript_path, own_workspace):
    pages = json.loads(transcript_path.read_text())
    hits, calls = [], 0
    for page in pages:
        for item in page["items"]:
            if item["role"] not in ("tool", "terminal"):
                continue
            calls += 1
            inp, out = call_parts(item)
            for name, pattern in INPUT_PATTERNS.items():
                for m in re.finditer(pattern, inp, re.I | re.M):
                    start = max(0, m.start() - 160)
                    hits.append({"seq": item["seq"], "kind": name, "where": "input",
                                 "snippet": inp[start:m.end() + 160]})
                    break
            for name, pattern in OUTPUT_PATTERNS.items():
                m = re.search(pattern, out, re.I)
                if m:
                    start = max(0, m.start() - 160)
                    hits.append({"seq": item["seq"], "kind": name, "where": "output",
                                 "snippet": out[start:m.end() + 160]})
            other = re.findall(r"/workspace/([0-9a-f]{32})", inp + out)
            foreign = sorted(set(other) - {own_workspace})
            if foreign:
                hits.append({"seq": item["seq"], "kind": "other-workspace", "where": "input/output",
                             "snippet": ", ".join(foreign)})
    return {"tool_items": calls, "hits": hits}


def audit_run(run):
    placement = json.loads((run / "placement.json").read_text())
    own = placement["cwd"].split("/")[2]
    result = {"run": str(run.relative_to(ROOT)), "reviewers": {}}
    for transcript in sorted(run.glob("reviewers/*/transcript.json")):
        result["reviewers"][transcript.parent.name] = audit_reviewer(transcript, own)
    check = json.loads((run / "inventory-check.json").read_text())
    result["inventory_problems"] = check["problems"]
    (run / "audit.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main():
    runs = [pathlib.Path(a).resolve() for a in sys.argv[1:]] or sorted(p.parent for p in ROOT.glob("runs/*/*/done.json"))
    for run in runs:
        r = audit_run(run)
        counts = {k: {} for k in r["reviewers"]}
        for rid, rv in r["reviewers"].items():
            for h in rv["hits"]:
                counts[rid][h["kind"]] = counts[rid].get(h["kind"], 0) + 1
        print(r["run"], json.dumps(counts), "inventory:", r["inventory_problems"] or "clean")


if __name__ == "__main__":
    main()
