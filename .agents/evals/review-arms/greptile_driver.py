"""Runs Greptile CLI reviews of the corpus diffs as review-arms sessions.

Usage: python3 greptile_driver.py --jobs jobs.json [--concurrency N]
jobs.json: [{"bug": "<corpus id>", "tier": "base"|"plus"}, ...]

Each job checks out a neutral branch at the diff's commit in a worktree of
<REVIEW_ARMS_DIR>/greptile/repo (a clone whose origin is the GitHub repo) and
runs `greptile review --branch <base> --json`, adding --plus for Plus. The
result is written as runs/<diff>/G-<tier>/ in the layout build_verify.py and
score.py read: done.json, reviewers/<id>/output.md with one "- [Pn]" bullet per
Greptile comment, and measured.json with the CLI's wall time and the documented
price ($1 Base, $3 Plus; https://www.greptile.com/docs/account/billing-seats).

Greptile reviews read context on Greptile's servers, which this experiment
cannot pin to the diff's commit; see README. A job with launch-intent.json but
no done.json was dispatched and may have been charged: it is fetched from
Greptile's saved runs, and dispatched again only if its run was cancelled or failed.
"""
import argparse
import concurrent.futures
import datetime
import hashlib
import json
import os
import re
import pathlib
import subprocess
import sys
import time
import traceback

SCRIPTS = pathlib.Path(__file__).resolve().parent
ROOT = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))
REPO = ROOT / "greptile" / "repo"
PRICE_USD = {"base": 1.0, "plus": 3.0}
CORPUS = {c["id"]: c for name in ("candidates-all.jsonl", "diffs.jsonl")
          for c in map(json.loads, (SCRIPTS / "corpus" / name).read_text().splitlines())}
KEY = pathlib.Path(os.environ.get("GREPTILE_API_KEY_FILE", pathlib.Path.home() / ".secrets/greptile_api_key"))
ENV = dict(os.environ, GREPTILE_API_KEY=KEY.read_text().strip(), GREPTILE_TELEMETRY_DISABLED="1", NO_COLOR="1")


def save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(value, indent=2) + "\n")
    tmp.replace(path)


def git(*args, cwd=REPO):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout


def bullets(review):
    """One "- [Pn] title — path:lines" bullet per comment, as the native reviewers write them."""
    out = []
    for c in review.get("comments") or []:
        body = (c.get("body") or "").strip()
        title, _, rest = body.partition("\n")
        sev = str(c.get("severity", ""))
        assert re.fullmatch(r"P[0-3]", sev), f"unexpected Greptile severity {sev!r}"  # e.g. "P1"
        level = int(sev[1])
        lines = f"{c.get('startLine')}-{c.get('endLine')}" if c.get("endLine") not in (None, c.get("startLine")) \
            else f"{c.get('startLine')}"
        out.append(f"- [P{level}] {title.strip()} — {c.get('path')}:{lines}\n{rest.strip()}".rstrip())
    return "\n\n".join(out) if out else "No findings."


def greptile(*args, cwd=REPO):
    r = subprocess.run(["greptile", *args], cwd=cwd, env=ENV, capture_output=True, text=True)
    if r.returncode:
        raise RuntimeError(f"greptile {' '.join(args[:2])} exit {r.returncode}: {r.stderr[-800:]}")
    return r


def runs_for(sha, base):
    """Greptile's saved runs for a commit reviewed against `base`, newest first, with
    their createdAt and completedAt. History groups runs by commit and names only the
    latest branch, so a run's tier comes from `review show` (its "effort" field)."""
    history = json.loads(greptile("review", "show", "--json").stdout)
    runs = [r for g in history if g.get("headSha") == sha and g.get("baseSha") == base for r in g["reviews"]]
    return sorted(runs, key=lambda r: r["createdAt"], reverse=True)


def fetch(run_id):
    """The finished review, waiting while it still runs."""
    r = greptile("review", "show", run_id, "--json")
    return r.stdout, json.loads(r.stdout)


def tier_of(run):
    """A saved run's tier, or None when Greptile reports that the run failed or was
    cancelled (local history can still list such a run as in flight)."""
    try:
        return fetch(run["runId"])[1].get("effort")
    except RuntimeError as e:
        if "review failed unexpectedly" in str(e) or "review was cancelled" in str(e):
            return None
        raise


def iso_s(t):
    return datetime.datetime.fromisoformat(t.replace("Z", "+00:00")).timestamp()


def finish(d, bug, tier, run_id, stderr=""):
    """Fetches a run (waiting if it is still going) and writes it in the review-arms layout."""
    raw, review = fetch(run_id)
    assert review.get("effort") == tier, (review.get("effort"), tier)
    (d / "review.json").write_text(raw)
    run = next(x for x in runs_for(bug["intro_commit"], bug["base_commit"]) if x["runId"] == run_id)
    child = hashlib.sha256(f"greptile:{run_id}".encode()).hexdigest()[:32]
    sd = d / "reviewers" / child[:12]
    sd.mkdir(parents=True, exist_ok=True)
    (sd / "output.md").write_text(bullets(review) + "\n")
    save(sd / "measured.json", {
        "latency_s": round(iso_s(run["completedAt"]) - iso_s(run["createdAt"]), 1), "cost_usd": PRICE_USD[tier],
        "total_tokens": None, "output_tokens": None, "run_id": run_id,
        "created_at": run["createdAt"], "completed_at": run["completedAt"],
        "held_back_warning": "held back" in stderr.lower()})
    reviewer = {"child": child, "slot": 0, "profile": "greptile", "model": "greptile", "effort": tier,
                "has_output": True, "run_id": run_id}
    save(d / "done.json", {"bug": bug["id"], "arm": f"G-{tier}", "intro": bug["intro_commit"],
                           "base": bug["base_commit"], "reviewers": [reviewer], "finished_ns": time.time_ns()})


def release(wt, branch):
    """Removes a job's worktree and branch if they exist; safe to repeat."""
    if wt.exists():
        git("worktree", "remove", "--force", str(wt))
    subprocess.run(["git", "branch", "-D", branch], cwd=REPO, capture_output=True)


def run_job(job):
    bug, tier = CORPUS[job["bug"]], job["tier"]
    d = ROOT / "runs" / bug["id"] / f"G-{tier}"
    tag = hashlib.sha256((bug["id"] + tier).encode()).hexdigest()
    branch, wt = f"review-{tag[:10]}", ROOT / "greptile" / "worktrees" / tag[:10]
    try:
        if (d / "done.json").exists():
            return "done"
        if (d / "launch-intent.json").exists():
            # A dispatched run may have finished, and been charged, after its CLI stopped:
            # fetch it instead of paying again. Only a cancelled or failed run is dispatched again.
            # Only a run this launch created: one absent from the runs saved before it.
            # (Comparing local and server clocks would not show that.)
            intent = json.loads((d / "launch-intent.json").read_text())
            if "prior_runs" not in intent:
                raise RuntimeError(f"{d}/launch-intent.json predates prior_runs; resolve this job by hand")
            live = [r for r in runs_for(bug["intro_commit"], bug["base_commit"])
                    if r["runId"] not in intent["prior_runs"] and r["status"] != "CANCELLED" and tier_of(r) == tier]
            if live:
                finish(d, bug, tier, live[0]["runId"])
                return "recovered"
            (ROOT / "failed-attempts").mkdir(exist_ok=True)
            d.rename(ROOT / "failed-attempts" / f"{bug['id']}-G-{tier}-no-result-{time.time_ns()}")
        release(wt, branch)
        git("worktree", "add", "-q", "-b", branch, str(wt), bug["intro_commit"])
        assert git("rev-parse", "HEAD", cwd=wt).strip() == bug["intro_commit"]
        prior = [r["runId"] for r in runs_for(bug["intro_commit"], bug["base_commit"])]
        save(d / "launch-intent.json", {"bug": bug["id"], "tier": tier, "branch": branch, "time_ns": time.time_ns(),
                                        "prior_runs": prior})
        cmd = ["greptile", "review", "--branch", bug["base_commit"], "--json"] + (["--plus"] if tier == "plus" else [])
        r = subprocess.run(cmd, cwd=wt, env=ENV, capture_output=True, text=True)
        (d / "review.log").write_text(r.stderr)
        if r.returncode:
            raise RuntimeError(f"greptile exit {r.returncode}: {r.stderr[-800:]}")
        # Jobs on one commit never run at the same time, so exactly one run is new.
        new = [x["runId"] for x in runs_for(bug["intro_commit"], bug["base_commit"]) if x["runId"] not in prior]
        if len(new) != 1:
            raise RuntimeError(f"expected one new Greptile run for {bug['id']}/{tier}, found {new}")
        finish(d, bug, tier, new[0], r.stderr)
        return "done"
    finally:
        # Every exit path releases the checkout, including leftovers of an interrupted run.
        release(wt, branch)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--jobs", required=True)
    p.add_argument("--concurrency", type=int, default=1)
    a = p.parse_args()
    jobs = json.loads(pathlib.Path(a.jobs).read_text())
    # Greptile runs one review per commit at a time and cancels the older one, so all
    # jobs on one commit (a diff's tiers, or corpus entries that share a commit) run
    # one after another inside one worker.
    by_diff = {}
    for j in jobs:
        by_diff.setdefault(CORPUS[j["bug"]]["intro_commit"], []).append(j)

    def one(diff_jobs):
        results = []
        for i, job in enumerate(diff_jobs):
            d = ROOT / "runs" / job["bug"] / f"G-{job['tier']}"
            try:
                results.append((job, run_job(job), None))
            except Exception as e:  # report every failure with its job
                save(d / "error.json", {"error": str(e), "traceback": traceback.format_exc(), "time_ns": time.time_ns()})
                results.append((job, "failed", str(e)))
                if (d / "launch-intent.json").exists() and not (d / "done.json").exists():
                    # Greptile may still be running (and charging for) this review, and a new
                    # review of the same commit would cancel it: leave the commit's other jobs
                    # until a later run has recovered or retried this one.
                    results += [(j, "blocked", f"waits for {job['bug']}/{job['tier']} on the same commit")
                                for j in diff_jobs[i + 1:]]
                    break
        return results

    unfinished = 0
    with concurrent.futures.ThreadPoolExecutor(max_workers=a.concurrency) as pool:
        for future in concurrent.futures.as_completed([pool.submit(one, js) for js in by_diff.values()]):
            for job, state, error in future.result():
                print(json.dumps({"job": job, "state": state, "error": error}), flush=True)
                unfinished += state not in ("done", "recovered")
    return 1 if unfinished else 0


if __name__ == "__main__":
    sys.exit(main())
