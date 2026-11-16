"""Readout of the Wizard-of-Oz site tool arm (brief/PLAN-oz.md).

Usage (REVIEW_ARMS_DIR holds the data):
  python3 brief_oz.py readout --round brief-judge-i --retrieval /mnt/optane/review-arms-oz/retrieval.json
      Q (site tool) against no brief (N) and arm D from round I, and against arm W from round H
      on the diffs W reached; retrieval from the stand-in server's call log; the plan's outcome.
"""
import argparse
import json
import pathlib
import random
import statistics

from brief_ablation import BOOT, MODELS, PRIMARY, diffs, interval, rates
from driver import ROOT, save

W_ROUND = "brief-judge-h"


def paired(bugs, a, b):
    """mean(a) - mean(b) over bugs with a paired 95% bootstrap interval over diffs."""
    rng = random.Random(3)
    mean = statistics.fmean
    draws = []
    for _ in range(BOOT):
        ix = rng.choices(range(len(bugs)), k=len(bugs))
        draws.append(mean(a[bugs[i]] for i in ix) - mean(b[bugs[i]] for i in ix))
    return {"difference": mean(a[x] for x in bugs) - mean(b[x] for x in bugs), "ci95": interval(draws)}


def outcome(q, d, n):
    """The plan's outcomes, first that holds (Terra)."""
    if q >= d - 0.10:
        return "querying works"
    if q <= n + 0.10:
        return "querying fails"
    return "partly"


def readout(round_name, retrieval_path):
    bugs = diffs()
    i = rates(round_name, set(bugs))
    w_bugs = [b for b in bugs if any(b in v for k, v in rates(W_ROUND, set(bugs)).items() if k[0] == "bifrost-diluted")]
    h = rates(W_ROUND, set(w_bugs))
    mean = statistics.fmean
    per_model = {}
    for m in MODELS:
        Q, N, D, F = (i[(c, m)] for c in ("site-tool", "no-brief", "bifrost-plus", "bifrost"))
        W = h[("bifrost-diluted", m)]
        per_model[m] = {
            "catch_rate": {"Q_site_tool": mean(Q[b] for b in bugs), "N_no_brief": mean(N[b] for b in bugs),
                           "D_bifrost_plus": mean(D[b] for b in bugs), "F_bifrost": mean(F[b] for b in bugs),
                           f"W_diluted_round_h_{len(w_bugs)}_diffs": mean(W[b] for b in w_bugs),
                           f"Q_on_W_diffs": mean(Q[b] for b in w_bugs)},
            "Q_minus_N": paired(bugs, Q, N), "Q_minus_D": paired(bugs, Q, D), "Q_minus_F": paired(bugs, Q, F),
            "Q_minus_W_on_W_diffs": paired(w_bugs, Q, W),
            "per_diff": {b: {"Q": Q[b], "N": N[b], "D": D[b], "F": F[b], "W": W.get(b)} for b in bugs}}
    retrieval = json.loads(pathlib.Path(retrieval_path).read_text())
    t = per_model[PRIMARY]["catch_rate"]
    report = {"round": round_name, "diffs": bugs, "w_diffs": w_bugs, "primary_model": PRIMARY,
              "outcome": outcome(t["Q_site_tool"], t["D_bifrost_plus"], t["N_no_brief"]),
              "retrieval": retrieval, "per_model": per_model,
              "note": "W is labelled in round H (same judge model); Q - W is on the diffs W reached."}
    save(ROOT / round_name / "oz.json", report)
    print(json.dumps(report, indent=1))


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("step", choices=["readout"])
    p.add_argument("--round", required=True)
    p.add_argument("--retrieval", required=True)
    a = p.parse_args()
    readout(a.round, a.retrieval)
