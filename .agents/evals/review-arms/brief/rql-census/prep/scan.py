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
import os
import pathlib
import random
import re
import subprocess
import sys
import threading

CLONES = pathlib.Path("/nfs/morannon/nvme/repo-clones")
OUT = pathlib.Path(__file__).resolve().parent
CANDS = pathlib.Path(sys.argv[1])
JOBS = 8
CAP_PER_WINDOW = 10      # per repo, for windows from 2024-01 on
PRE2024_GLOBAL = 300     # global random sample, rate only
SEED = 20261118

lock = threading.Lock()


# The corpus clones are read only: never lazily fetch a missing object into them.
GIT_ENV = dict(os.environ, GIT_NO_LAZY_FETCH="1", GIT_TERMINAL_PROMPT="0")


def git(repo, *args, stdin=None, timeout=300):
    r = subprocess.run(["git", "--no-optional-locks", "-C", str(CLONES / repo), *args],
                       input=stdin, capture_output=True, text=True, timeout=timeout, env=GIT_ENV)
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


def stage_roots(repos):
    """Root commits of every clone. Clones sharing a root share history: forks and renames share
    their roots, and a subtree merge imports the subtree's roots into the host repo."""
    out = OUT / "roots.jsonl"
    have = set()
    if out.exists():
        have = {r["repo"] for r in map(json.loads, open(out)) if not r.get("error")}

    def one(repo):
        if repo in have:
            return
        rc, so, se = git(repo, "rev-list", "--max-parents=0", "--all", timeout=600)
        emit(out, {"repo": repo, "roots": sorted(set(so.split())), "error": se.strip()[-300:] if rc else None})

    with cf.ThreadPoolExecutor(JOBS) as ex:
        list(ex.map(one, sorted(repos)))


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
            dates = dict(l.split() for l in so.splitlines() if l.strip()) if rc2 == 0 else {}
            undatable = {}
            if rc2:  # one bad hash fails the batch: date the rest one at a time
                for h in hashes.split():
                    rc3, so3, se3 = git(repo, "log", "-1", "--format=%ct", h)
                    if rc3 == 0 and so3.strip():
                        dates[h] = so3.strip()
                    else:
                        undatable[h] = se3.strip()[-200:]
            emit(out, {"repo": repo, "clone_ct": int(head.strip() or 0),
                       "dates": {h: int(t) for h, t in dates.items()}, "undatable": undatable, "error": None})
        except Exception as e:  # report, don't drop
            emit(out, {"repo": repo, "error": repr(e)[:300]})

    with cf.ThreadPoolExecutor(JOBS) as ex:
        list(ex.map(one, sorted(by_repo)))


FN_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?"
                   r"(?:extern\s+(?:\"[^\"]*\"\s+)?)?fn\s+([A-Za-z_][A-Za-z0-9_]*)")


def enclosing_fn(lines, line_no):
    """Nearest preceding `fn` signature at or above 1-based line_no in the old file (None at top level)."""
    for i in range(min(line_no, len(lines)) - 1, -1, -1):
        m = FN_RE.match(lines[i])
        if m:
            return m.group(1), i + 1
    return None


def changed_sites(repo, h):
    """Per non-test .rs file: deleted old-side ranges and enclosing functions of every hunk.

    Paths come from `--name-status -z -M` (never parsed from patch headers, which Git quotes;
    renames keep their old path for blame), each file's hunks from a literal-pathspec diff, and enclosing functions from the old file text,
    so the result doesn't depend on the repo's diff drivers or attributes."""
    rc, so, se = git(repo, "diff", "--no-ext-diff", "--no-textconv", "--name-status", "-z", "-M", f"{h}^", h, "--", "*.rs")
    if rc:
        raise RuntimeError("diff names: " + se.strip()[-200:])
    fields, entries = so.split("\0"), []
    i = 0
    while i < len(fields) and fields[i]:
        status = fields[i]
        if status[0] in "RC":  # rename or copy: old path, then new path
            entries.append((status, fields[i + 1], fields[i + 2]))
            i += 3
        else:
            entries.append((status, fields[i + 1], fields[i + 1]))
            i += 2
    entries = [e for e in entries if not is_test_path(e[2])]
    paths = {e[1] for e in entries}  # old-side paths: blame and old text live at fix^
    ranges, funcs = collections.defaultdict(list), set()
    for status, old_path, new_path in entries:
        specs = [f":(literal){old_path}"] + ([f":(literal){new_path}"] if new_path != old_path else [])
        rc, diff, se = git(repo, "diff", "--no-ext-diff", "--no-textconv", "--text", "-U0", "--no-color", "-M",
                           f"{h}^", h, "--", *specs)
        if rc:
            raise RuntimeError("diff file: " + se.strip()[-200:])
        path = old_path
        if status.startswith("A"):
            old_lines = []
        else:
            rc, old, se = git(repo, "show", "--no-textconv", f"{h}^:{old_path}")
            if rc:  # an unreadable old blob would misplace every hunk: fail the draw visibly
                raise RuntimeError(f"old blob {old_path}: " + se.strip()[-200:])
            old_lines = old.splitlines()
        for line in diff.splitlines():
            m = re.match(r"@@ -(\d+)(?:,(\d+))? \+\d+(?:,\d+)? @@", line)
            if not m:
                continue
            start, count = int(m.group(1)), int(m.group(2) if m.group(2) is not None else 1)
            anchor = start if count > 0 else max(start, 1)
            funcs.add((path, enclosing_fn(old_lines, anchor)))
            if count > 0:
                ranges[path].append((start, count))
    return set(paths), ranges, funcs


SIZE_EXCLUDED = re.compile(r"(^|/)(Cargo\.lock|[^/]*\.lock|package-lock\.json|pnpm-lock\.yaml|yarn\.lock)$"
                           r"|(^|/)(snapshots?|goldens?|fixtures?|testdata|test_data|expected)(/|$)"
                           r"|\.(snap|golden|expected)$")


def intro_size(repo, sha):
    """Changed lines of the introducing commit as corpus/SPEC.md rule 7 counts them (goldens,
    fixtures and lock files excluded), whether it is a merge (rule 6 rejects merges), and an
    error if Git failed. Binary files count zero lines."""
    rc, parents, se = git(repo, "rev-list", "--parents", "-n", "1", sha)
    if rc:
        return 0, False, "rev-list: " + se.strip()[-200:]
    if len(parents.split()) > 2:
        return 0, True, None
    rc, so, se = git(repo, "show", "--no-ext-diff", "--no-textconv", "--numstat", "-z", "--format=",
                     "-M", sha, timeout=300)
    if rc:
        return 0, False, "numstat: " + se.strip()[-200:]
    # -z numstat: "added\tdeleted\tpath\0", or for a rename "added\tdeleted\t\0old\0new\0".
    # A rename counts only its edited lines (corpus SPEC rule 7 counts the review diff).
    size, fields, i = 0, so.lstrip("\n").split("\0"), 0
    while i < len(fields):
        parts = fields[i].lstrip("\n").split("\t", 2)
        if len(parts) != 3:
            i += 1
            continue
        if parts[2] == "":  # rename or copy: the next two fields are the old and new paths
            path = fields[i + 2] if i + 2 < len(fields) else ""
            i += 3
        else:
            path = parts[2]
            i += 1
        if SIZE_EXCLUDED.search(path):
            continue
        size += sum(int(x) for x in parts[:2] if x.isdigit())
    return size, False, None


def blame_one(c):
    repo, h = c["repo"], c["hash"]
    rec = {k: c[k] for k in ("repo", "hash", "oneline", "window", "fix_ct", "p2p", "outcome", "validated_image")}
    try:
        files, ranges, funcs = changed_sites(repo, h)
        rec["fix_files"], rec["fix_funcs"] = len(files), len(funcs)
        rec["multi_site_proxy"] = len(files) >= 2 or len(funcs) >= 2
        counts, ctimes, failed = collections.Counter(), {}, []
        for path, rs in ranges.items():
            args = ["blame", "--no-textconv", "--porcelain", "-w"]
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
        size, is_merge, err = intro_size(repo, top)
        if err:
            failed.append({"intro_size": top, "stderr": err})
            rec["trace_complete"] = False
        rec["intro_is_merge"] = is_merge
        rec.update(traceable=True, top_share=n / total, intro_sha=top, intro_ct=ctimes.get(top),
                   intro_window=window(ctimes[top]) if top in ctimes else None, intro_size=size,
                   strong=(not failed and not is_merge and n / total >= 0.3 and size <= 4000))
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
    stage_roots(by_repo)
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
    # A draw is done once its latest row is a clean trace; failed or incomplete draws are retried.
    latest = {}
    if out.exists():
        for l in open(out):
            r = json.loads(l)
            latest[(r["repo"], r["hash"])] = r
    done = {k for k, r in latest.items() if not r.get("error") and r.get("trace_complete") is not False}
    todo = [c for c in sample if (c["repo"], c["hash"]) not in done]
    import hashlib
    population = hashlib.sha256("\n".join(sorted(f"{c['repo']} {c['hash']}" for c in cands)).encode()).hexdigest()
    json.dump({"candidates": len(cands), "candidates_sha256": population,
               "cell_sizes": {f"{repo} {w}": len(cs) for (repo, w), cs in sorted(cells.items())},
               "dated": sum(1 for c in cands if c["window"]),
               "sample": len(sample), "todo": len(todo),
               "sample_ids": sorted(f"{c['repo']} {c['hash']}" for c in sample),
               "by_window": collections.Counter(c["window"] for c in cands),
               "clone_ct": clone_ct}, open(OUT / "plan.json", "w"), indent=1)
    with cf.ThreadPoolExecutor(JOBS) as ex:
        for rec in ex.map(blame_one, todo):
            emit(out, rec)
    print("done", flush=True)


if __name__ == "__main__":
    main()
