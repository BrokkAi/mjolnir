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
        # Command output lives in the item's terminal_outputs; call.content holds only a terminal ID.
        out = json.dumps({"content": call.get("content"), "rawOutput": call.get("rawOutput"),
                          "terminal_outputs": body.get("terminal_outputs")})
        return inp, out
    return json.dumps(body), ""


def audit_reviewer(transcript_path, own_workspace):
    return scan(json.loads(transcript_path.read_text()), own_workspace)


def scan(pages, own_workspace, *, commits=None, allow_rql=False, single_commit=False):
    hits, calls = [], 0
    for page in pages:
        for item in page["items"]:
            if item["role"] not in ("tool", "terminal"):
                continue
            call = (item.get("body") or {}).get("call") or {}
            if commits is not None and not call:
                # Standalone terminal_output rows contain output only. The
                # originating command is recorded as its own tool call.
                continue
            calls += 1
            inp, out = call_parts(item)
            if allow_rql and call.get("title") in ("rql_query", "mcp__rql__rql_query") and call.get("kind") != "execute":
                continue
            for name, pattern in INPUT_PATTERNS.items():
                for m in re.finditer(pattern, inp, re.I | re.M):
                    start = max(0, m.start() - 160)
                    hits.append({"seq": item["seq"], "kind": name, "where": "input",
                                 "snippet": inp[start:m.end() + 160]})
                    break
            if commits is None:  # Keep the legacy screen, where output hits were reviewed by hand.
                for name, pattern in OUTPUT_PATTERNS.items():
                    m = re.search(pattern, out, re.I)
                    if m:
                        start = max(0, m.start() - 160)
                        hits.append({"seq": item["seq"], "kind": name, "where": "output",
                                     "snippet": out[start:m.end() + 160]})
            other = re.findall(r"/workspace/([0-9a-f]{32})", inp + out if commits is None else inp)
            foreign = sorted(set(other) - {own_workspace})
            if foreign:
                hits.append({"seq": item["seq"], "kind": "other-workspace",
                             "where": "input/output" if commits is None else "input",
                             "snippet": ", ".join(foreign)})
            if commits is not None:
                for name, snippet in pilot_hits(inp, commits, single_commit):
                    hits.append({"seq": item["seq"], "kind": name, "where": "input", "snippet": snippet})
    return {"tool_items": calls, "hits": hits}


def pilot_hits(inp, commits, single_commit):
    extra_network = CMD + r"(ssh|scp|sftp|rsync|nc|ncat|netcat|socat|telnet|ftp)\s|\b(socket\.connect|urllib\.request|requests\.(get|post)|Invoke-WebRequest)\b"
    if match := re.search(extra_network, inp, re.I):
        yield "network-command", inp[max(0, match.start() - 80):match.end() + 120]
    for match in re.finditer(r"\bgit\s+[^\n;&|]*", inp):
        command = match[0].split(r"\n", 1)[0]
        if not re.search(r"\b(diff|log|show|blame|rev-list|rev-parse|cat-file|checkout|switch)\b", command):
            continue
        unsafe_ref = re.search(r"\b(origin/|upstream/|refs/remotes/|FETCH_HEAD\b|ORIG_HEAD\b|"
                               r"MERGE_HEAD\b)|--(git-dir|work-tree|no-index)\b", command)
        hashes = re.findall(r"\b[0-9a-f]{7,40}\b", command)
        if (unsafe_ref or any(not any(pin.startswith(oid) for pin in commits) for oid in hashes)
                or (single_commit and re.search(r"HEAD(?:[~^][1-9]?|@\{)", command))):
            yield "git-revision-outside-pin", command[:400]


def audit_pilot(transcript_path, own_workspace, *, commits=(), allow_rql=False, single_commit=False):
    """Apply the study's transcript screen without changing legacy audit results."""
    try:
        pages = json.loads(transcript_path.read_text())
        if not any(page["items"] for page in pages):
            raise ValueError("empty transcript")
        result = scan(pages, own_workspace, commits=commits, allow_rql=allow_rql, single_commit=single_commit)
    except (OSError, ValueError, KeyError, TypeError) as error:
        result = {"tool_items": 0, "hits": [{"kind": "missing-or-unreadable-transcript", "snippet": str(error)}]}
    return dict(result, passed=not result["hits"])


def audit_run(run):
    placement = json.loads((run / "placement.json").read_text())
    own = placement["cwd"].split("/")[2]
    result = {"run": str(run.relative_to(ROOT)), "reviewers": {}}
    for transcript in sorted(run.glob("reviewers/*/transcript.json")):
        result["reviewers"][transcript.parent.name] = audit_reviewer(transcript, own)
    # Since mj 8fe3c40bd a reviewer session is destroyed when its role ends, so driver.py keeps
    # only its stored handback and no transcript: its tool calls cannot be audited. Say so; an
    # empty reviewer list must not read as a clean audit.
    listed = [r["child"][:12] for r in json.loads((run / "done.json").read_text())["reviewers"]]
    result["unaudited"] = [c for c in listed if c not in result["reviewers"]]
    check = json.loads((run / "inventory-check.json").read_text())
    result["inventory_problems"] = check["problems"]
    (run / "audit.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main():
    runs = [(ROOT / a).resolve() for a in sys.argv[1:]] or sorted(p.parent for p in ROOT.glob("runs/*/*/done.json"))
    failed = False
    for run in runs:
        r = audit_run(run)
        counts = {k: {} for k in r["reviewers"]}
        for rid, rv in r["reviewers"].items():
            for h in rv["hits"]:
                counts[rid][h["kind"]] = counts[rid].get(h["kind"], 0) + 1
        unaudited = f" UNAUDITED (no transcript): {r['unaudited']}" if r["unaudited"] else ""
        print(r["run"], json.dumps(counts), "inventory:", r["inventory_problems"] or "clean", unaudited)
        failed = failed or bool(r["unaudited"])
    if failed:
        sys.exit("some reviewers have no transcript to audit; their tool calls were not checked")


if __name__ == "__main__":
    main()
