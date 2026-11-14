"""Checks that no carrier used a tool before its review's final delivery (reviewers share its workspace)."""
import json, pathlib, sys
import driver as d
out = {}
for run in sorted(p.parent for p in d.ROOT.glob("runs/*/*/review-start.json")):
    path = run / "carrier-transcript-after.json"
    if not path.exists():
        sid = json.loads((run / "launch.json").read_text())["session_id"]
        d.save(path, d.transcript(sid))
    items = [i for p in json.loads(path.read_text()) for i in p["items"]]
    finals = [i["seq"] for i in items if i["role"] == "system" and "Review of commits" in json.dumps(i["body"])]
    tools = [i["seq"] for i in items if i["role"] in ("tool", "terminal")]
    final = finals[0] if finals else None
    early = [s for s in tools if final is None or s < final]
    out[str(run.relative_to(d.ROOT))] = {"final_delivery_seq": final, "carrier_tool_calls": len(tools), "before_final": early}
    print(run.relative_to(d.ROOT), json.dumps(out[str(run.relative_to(d.ROOT))]))
d.save(d.ROOT / "carrier-check.json", out)
