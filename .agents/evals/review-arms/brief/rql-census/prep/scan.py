#!/usr/bin/env python3
"""Upper-bound yield screen of strong, multi-site Rust bug-fix candidates in the local corpus.

Input: stage_a_candidates.jsonl (brokkbench SFT records with tests + non-test code + bug words).
Clones: /nfs/morannon/nvme/repo-clones/<repo>, read only (git --no-optional-locks, read commands).
Output (incremental): dates.jsonl, then blame.jsonl; summary.json at the end.
"""
import collections
import concurrent.futures as cf
import datetime as dt
import json
import pathlib
import random
import re
import subprocess
import sys
import threading

CLONES = pathlib.Path("/nfs/morannon/nvme/repo-clones")
OUT = pathlib.Path(__file__).resolve().parent
CANDS = pathlib.Path(sys.argv[1])
JOBS = 24
CAP_PER_WINDOW = 10      # per repo, for windows from 2024-01 on
PRE2024_GLOBAL = 300     # global random sample, rate only
SEED = 20261118

lock = threading.Lock()


def git(repo, *args, stdin=None, timeout=300):
    r = subprocess.run(["git", "--no-optional-locks", "-C", str(CLONES / repo), *args],
                       input=stdin, capture_output=True, text=True, timeout=timeout)
    return r.returncode, r.stdout, r.stderr


def window(ts):
    d = dt.datetime.fromtimestamp(ts, dt.timezone.utc)
    if d.year < 2024:
        return "pre2024"
    if d.year == 2024:
        return "2024"
    if d.year == 2025:
        return "2025H1" if d.month <= 6 else "2025H2"
    if d.year == 2026 and d.month <= 6:
        return "2026H1"
    return "post2026-06"


def is_test_path(p):
    parts = p.split("/")
    name = parts[-1]
    return ("tests" in parts or "test" in parts or "testdata" in parts or "benches" in parts
            or name in ("tests.rs", "test.rs") or name.endswith("_test.rs") or name.endswith("_tests.rs"))


def emit(path, rec):
    with lock, open(path, "a") as f:
        f.write(json.dumps(rec) + "\n")


def stage_dates(by_repo):
    out = OUT / "dates.jsonl"
    have = set()
    if out.exists():
        for l in open(out):
            r = json.loads(l)
            if not r.get("error"):
                have.update((r["repo"], h) for h in (r.get("dates") or {}))

    def one(repo):
        if all((repo, c["hash"]) in have for c in by_repo[repo]):
            return
        if not (CLONES / repo).is_dir():
            emit(out, {"repo": repo, "error": "no clone"})
            return
        try:
            rc, head, _ = git(repo, "log", "-1", "--format=%ct", "HEAD", timeout=120)
            hashes = "\n".join(c["hash"] for c in by_repo[repo] if (repo, c["hash"]) not in have) + "\n"
            rc2, so, se = git(repo, "log", "--no-walk=unsorted", "--format=%H %ct", "--stdin", stdin=hashes)
            dates = dict(l.split() for l in so.splitlines() if l.strip())
            emit(out, {"repo": repo, "clone_ct": int(head.strip() or 0),
                       "dates": {h: int(t) for h, t in dates.items()},
                       "error": se.strip()[-300:] if rc2 else None})
        except Exception as e:  # report, don't drop
            emit(out, {"repo": repo, "error": repr(e)[:300]})

    with cf.ThreadPoolExecutor(JOBS) as ex:
        list(ex.map(one, sorted(by_repo)))


def parse_diff(text):
    files, ranges, funcs = set(), collections.defaultdict(list), set()
    cur = None
    for line in text.splitlines():
        if line.startswith("diff --git "):
            cur = None
        elif line.startswith("--- "):
            p = line[4:]
            cur = p[2:] if p.startswith("a/") else None
        elif line.startswith("@@") and cur and not is_test_path(cur):
            m = re.match(r"@@ -(\d+)(?:,(\d+))? \+\d+(?:,\d+)? @@ ?(.*)", line)
            if not m:
                continue
            start, count = int(m.group(1)), int(m.group(2) if m.group(2) is not None else 1)
            files.add(cur)
            ctx = m.group(3).strip()
            fn = re.search(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)", ctx)
            funcs.add((cur, fn.group(1) if fn else ctx[:60]))
            if count > 0:
                ranges[cur].append((start, count))
    return files, ranges, funcs


def blame_one(c):
    repo, h = c["repo"], c["hash"]
    rec = {k: c[k] for k in ("repo", "hash", "oneline", "window", "fix_ct", "p2p", "outcome", "validated_image")}
    try:
        rc, diff, se = git(repo, "diff", "-U0", "--no-color", f"{h}^", h, "--", "*.rs")
        if rc:
            rec["error"] = "diff: " + se.strip()[-200:]
            return rec
        files, ranges, funcs = parse_diff(diff)
        rec["fix_files"], rec["fix_funcs"] = len(files), len(funcs)
        rec["multi_site_proxy"] = len(files) >= 2 or len(funcs) >= 2
        counts, ctimes, failed = collections.Counter(), {}, []
        for path, rs in ranges.items():
            args = ["blame", "--porcelain", "-w"]
            for s, n in rs:
                args += ["-L", f"{s},+{n}"]
            rc, so, se = git(repo, *args, f"{h}^", "--", path, timeout=600)
            if rc:
                failed.append({"path": path, "stderr": se.strip()[-200:]})
                continue
            sha = None
            for line in so.splitlines():
                m = re.match(r"^([0-9a-f]{40}) \d+ \d+", line)
                if m:
                    sha = m.group(1)
                    counts[sha] += 1
                elif line.startswith("committer-time ") and sha:
                    ctimes[sha] = int(line.split()[1])
        total = sum(counts.values())
        rec["blamed_lines"] = total
        rec["blame_failed"] = failed
        rec["trace_complete"] = not failed
        if not total:
            rec["traceable"] = False
            return rec
        top, n = counts.most_common(1)[0]
        rc, so, _ = git(repo, "show", "--shortstat", "--format=%ct", top, timeout=300)
        lines = [l for l in so.splitlines() if l.strip()]
        size = sum(int(x) for x in re.findall(r"(\d+) (?:insertion|deletion)", lines[-1])) if len(lines) > 1 else 0
        rec.update(traceable=True, top_share=n / total, intro_sha=top, intro_ct=ctimes.get(top),
                   intro_window=window(ctimes[top]) if top in ctimes else None, intro_size=size,
                   strong=(not failed and n / total >= 0.3 and size <= 4000))
        rec["intro_same_window"] = rec["intro_window"] == rec["window"]
        return rec
    except Exception as e:
        rec["error"] = repr(e)[:300]
        return rec


def main():
    cands = [json.loads(l) for l in open(CANDS)]
    by_repo = collections.defaultdict(list)
    for c in cands:
        by_repo[c["repo"]].append(c)
    stage_dates(by_repo)

    dates, clone_ct = {}, {}
    for l in open(OUT / "dates.jsonl"):
        r = json.loads(l)
        clone_ct[r["repo"]] = r.get("clone_ct")
        for h, t in (r.get("dates") or {}).items():
            dates[(r["repo"], h)] = t
    for c in cands:
        t = dates.get((c["repo"], c["hash"]))
        c["fix_ct"], c["window"] = t, (window(t) if t else None)

    rng = random.Random(SEED)
    sample, cells = [], collections.defaultdict(list)
    for c in cands:
        if c["window"]:
            cells[(c["repo"], c["window"])].append(c)
    pre = []
    for (repo, w), cs in sorted(cells.items()):
        if w == "pre2024":
            pre.extend(cs)
        else:
            rng.shuffle(cs)
            sample.extend(cs[:CAP_PER_WINDOW])
    rng.shuffle(pre)
    sample.extend(pre[:PRE2024_GLOBAL])

    out = OUT / "blame.jsonl"
    done = {(json.loads(l)["repo"], json.loads(l)["hash"]) for l in open(out)} if out.exists() else set()
    todo = [c for c in sample if (c["repo"], c["hash"]) not in done]
    json.dump({"candidates": len(cands), "dated": sum(1 for c in cands if c["window"]),
               "sample": len(sample), "todo": len(todo),
               "by_window": collections.Counter(c["window"] for c in cands),
               "clone_ct": clone_ct}, open(OUT / "plan.json", "w"), indent=1)
    with cf.ThreadPoolExecutor(JOBS) as ex:
        for rec in ex.map(blame_one, todo):
            emit(out, rec)
    print("done", flush=True)


if __name__ == "__main__":
    main()
