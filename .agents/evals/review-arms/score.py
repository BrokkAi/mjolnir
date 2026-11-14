"""Joins verified finding groups to reviewer sessions; writes per-session tables.

Usage: python3 score.py <verify-dir>   (e.g. verify2; under REVIEW_ARMS_DIR)

Inputs under REVIEW_ARMS_DIR:
  private/key-<verify-dir>.json        finding ID -> diff, reviewer session, model
  <verify-dir>/out/verify-out-*.json   verifier groups with verdicts
  runs/<diff>/<review>/                reviewer sessions, transcripts and usage

Writes, under <verify-dir>/:
  sessions.csv        one row per reviewer session: diff, session, model, review,
                      profile, latency_s, tokens, estimated cost in USD
  groups.csv          one row per distinct problem: diff, group, verdict, severity
  session_groups.csv  one row per (session, group) the session reported

Only diffs whose every finding was verified are included. A setup's result on
a diff is the union of the groups its sessions reported; reviewer sessions are
independent, so any combination of a diff's sessions is a valid sample.

Latency is the reviewer's own turn: from its prompt to the end of its turn.
It excludes session startup and time waiting for other reviewers.
"""
import collections
import csv
import glob
import json
import os
import pathlib
import sys

ROOT = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))
VERIFY = ROOT / sys.argv[1]
private_key = json.loads((ROOT / "private" / f"key-{VERIFY.name}.json").read_text())
key, covered = private_key["findings"], set(private_key["sessions"])

groups, session_groups, seen = {}, [], collections.Counter()
for path in sorted(glob.glob(str(VERIFY / "out/verify-out-*.json"))):
    for commit in json.load(open(path))["commits"]:
        for g in commit["groups"]:
            members = [key[f] for f in g["findings"]]
            seen.update(g["findings"])
            bugs = {m["bug"] for m in members}
            assert len(bugs) == 1, (path, g["group"], bugs)
            bug = bugs.pop()
            gid = f"{bug}/{g['group']}"
            assert gid not in groups, gid
            groups[gid] = {"bug": bug, "group": gid, "verdict": g["verdict"], "severity": g.get("severity"),
                           "confidence": g.get("confidence"), "summary": g["summary"]}
            for s in sorted({m["child"] for m in members}):
                session_groups.append({"session": s, "bug": bug, "group": gid})
duplicated = sorted(f for f, n in seen.items() if n > 1)
assert not duplicated, duplicated
expected = collections.defaultdict(set)
for f, k in key.items():
    expected[k["bug"]].add(f)
complete = {b for b, fs in expected.items() if fs <= set(seen)}
partial = sorted(b for b, fs in expected.items() if fs & set(seen) and not fs <= set(seen))
assert not partial, f"diffs with only some findings verified: {partial}"
unverified = sorted(set(expected) - complete)  # batched but not yet verified; left out below


def latency(session_dir):
    """Seconds from the reviewer's prompt to the end of its turn, or None."""
    row = json.loads((session_dir / "session.json").read_text())
    done = (row.get("last_turn_outcome") or {}).get("completed_at_ms")
    items = [i for p in json.loads((session_dir / "transcript.json").read_text()) for i in p["items"]]
    prompt = next((i["created_at_ms"] for i in items if i["role"] == "user"), None)
    return round((done - prompt) / 1000, 1) if done and prompt else None


sessions = {}
for done_path in sorted(ROOT.glob("runs/*/*/done.json")):
    done = json.loads(done_path.read_text())
    if done["bug"] not in complete and done["bug"] in expected:
        continue
    for r in done["reviewers"]:
        if r["child"] not in covered:
            continue
        sd = done_path.parent / "reviewers" / r["child"][:12]
        usage = json.loads((sd / "usage.json").read_text())
        ticks = usage.get("turn_cost_usd_ticks")
        sessions[r["child"]] = {
            "bug": done["bug"], "session": r["child"], "model": r["model"], "effort": r["effort"],
            "review": f"{done['bug']}/{done['arm']}", "profile": r["profile"], "latency_s": latency(sd),
            "total_tokens": usage["totals"]["total_tokens"]["tokens"],
            "output_tokens": usage["totals"]["output_tokens"]["tokens"],
            # mj's estimated API-price cost; 10^10 ticks per dollar (mj-core/src/usage.rs).
            "cost_usd": round(ticks / 1e10, 4) if isinstance(ticks, (int, float)) else None,
        }


def write_csv(name, rows, fields):
    with open(VERIFY / name, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=fields)
        w.writeheader()
        w.writerows(rows)


write_csv("sessions.csv", list(sessions.values()),
          ["bug", "session", "model", "effort", "review", "profile", "latency_s", "total_tokens", "output_tokens",
           "cost_usd"])
write_csv("groups.csv", list(groups.values()), ["bug", "group", "verdict", "severity", "confidence", "summary"])
write_csv("session_groups.csv", session_groups, ["session", "bug", "group"])
per_model = collections.Counter(s["model"] for s in sessions.values())
print(json.dumps({"diffs": len({s["bug"] for s in sessions.values()}), "sessions": len(sessions), "sessions_per_model": per_model,
                  "groups": len(groups), "verdicts": collections.Counter(g["verdict"] for g in groups.values()),
                  "missing_latency": sum(s["latency_s"] is None for s in sessions.values()),
                  "unverified_diffs_left_out": unverified}, indent=1))
