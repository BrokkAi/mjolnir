"""Scores verified findings per reviewer session, so setups can be composed afterwards.

Inputs under REVIEW_ARMS_DIR:
  private/key-verify-stage1.json   finding id -> bug, review arm, model, reviewer session
  verify/out/verify-out-v<n>.json  verifier groups with verdicts
  runs/<bug>/<arm>/reviewers/<id>/usage.json  reviewer token usage

Writes, under verify/:
  sessions.csv        one row per reviewer session: bug, session, model, review, profile, tokens
  groups.csv          one row per distinct problem: bug, group, verdict, severity, summary
  session_groups.csv  one row per (session, group) the session reported
  score.json          the same data plus the example comparison below

A setup's result on a bug is the union of the groups its sessions reported.
Reviewer sessions are independent harness sessions, so any combination of a
bug's sessions is a valid sample of a setup. The example comparison averages
every 3-Terra combination against every Terra + Sol pair.
"""
import collections
import csv
import glob
import itertools
import json
import os
import pathlib

ROOT = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))
OUT = ROOT / "verify"
key = json.loads((ROOT / "private/key-verify-stage1.json").read_text())
stage1_outputs = json.loads((ROOT / "private/key-stage1.json").read_text())

# Every stage 1 reviewer session, including those that reported no findings.
sessions = {}
for oid, k in stage1_outputs.items():
    run = ROOT / "runs" / k["bug"] / k["arm"]
    reviewers = {r["child"]: r for r in json.loads((run / "reviewers.json").read_text())}
    usage = json.loads((run / "reviewers" / k["child"][:12] / "usage.json").read_text())
    sessions[k["child"]] = {
        "bug": k["bug"], "session": k["child"], "model": k["model"], "review": f"{k['bug']}/{k['arm']}",
        "arm": k["arm"], "slot": k["slot"], "profile": reviewers[k["child"]]["profile"],
        "total_tokens": usage["totals"]["total_tokens"]["tokens"],
        "output_tokens": usage["totals"]["output_tokens"]["tokens"],
    }

groups, session_groups, seen = {}, [], collections.Counter()
for path in sorted(glob.glob(str(OUT / "out/verify-out-v*.json"))):
    for commit in json.load(open(path))["commits"]:
        for g in commit["groups"]:
            members = [key[f] for f in g["findings"]]
            seen.update(g["findings"])
            bugs = {m["bug"] for m in members}
            assert len(bugs) == 1, (path, g["group"], bugs)
            bug = bugs.pop()
            gid = f"{bug}/{g['group']}"
            groups[gid] = {"bug": bug, "group": gid, "verdict": g["verdict"], "severity": g.get("severity"),
                           "confidence": g.get("confidence"), "summary": g["summary"]}
            for s in sorted({m["child"] for m in members}):
                session_groups.append({"session": s, "bug": bug, "group": gid})
missing = sorted(set(key) - set(seen))
duplicated = sorted(f for f, n in seen.items() if n > 1)
assert not missing and not duplicated, (missing, duplicated)

found = collections.defaultdict(set)
for row in session_groups:
    found[row["session"]].add(row["group"])


def union_score(session_ids, verdicts=("confirmed",)):
    """Distinct groups with these verdicts found by any of the sessions."""
    hit = set().union(*(found[s] for s in session_ids)) if session_ids else set()
    return sum(groups[g]["verdict"] in verdicts for g in hit)


# Example: every 3-Terra combination vs every Terra + Sol pair, per bug.
example = {}
for bug in sorted({s["bug"] for s in sessions.values()}):
    terra = [s for s, v in sessions.items() if v["bug"] == bug and v["model"] == "gpt-6.1-terra"]
    sol = [s for s, v in sessions.items() if v["bug"] == bug and v["model"] == "gpt-6.1-sol"]
    row = {}
    for name, verdicts in [("confirmed", ("confirmed",)), ("confirmed_or_plausible", ("confirmed", "plausible"))]:
        a = [union_score(c, verdicts) for c in itertools.combinations(terra, 3)]
        b = [union_score((t, s), verdicts) for t in terra for s in sol]
        row[name] = {"3_terra_mean": sum(a) / len(a), "terra_sol_mean": sum(b) / len(b),
                     "single_terra_mean": sum(union_score((t,), verdicts) for t in terra) / len(terra),
                     "single_sol_mean": sum(union_score((s,), verdicts) for s in sol) / len(sol)}
    example[bug] = row


def write_csv(name, rows, fields):
    with open(OUT / name, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=fields)
        w.writeheader()
        w.writerows(rows)


write_csv("sessions.csv", list(sessions.values()),
          ["bug", "session", "model", "review", "arm", "slot", "profile", "total_tokens", "output_tokens"])
write_csv("groups.csv", list(groups.values()), ["bug", "group", "verdict", "severity", "confidence", "summary"])
write_csv("session_groups.csv", session_groups, ["session", "bug", "group"])
summary = {}
for k in ("confirmed", "confirmed_or_plausible"):
    summary[k] = {m: round(sum(example[b][k][m] for b in example), 2)
                  for m in ("3_terra_mean", "terra_sol_mean", "single_terra_mean", "single_sol_mean")}
result = {"sessions": len(sessions), "groups": len(groups),
          "verdicts": dict(collections.Counter(g["verdict"] for g in groups.values())),
          "example_totals_over_bugs": summary, "example_per_bug": example}
(OUT / "score.json").write_text(json.dumps(result, indent=2) + "\n")
print(json.dumps({k: v for k, v in result.items() if k != "example_per_bug"}, indent=1))
