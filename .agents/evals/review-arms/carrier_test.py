"""Checks that the carrier replies without tools for a bug's context; destroys the session."""
import json, sys, time, hashlib
import driver as d
bug = d.CORPUS[sys.argv[1]]
out = d.ROOT / "pilot" / ("carrier-test-" + bug["id"] + "-" + str(int(time.time())))
launch = d.mj("new", "--workspace-id", d.WORKSPACE, "--profile", "carrier", "--target", "podman", "--bundle", "mjolnir",
              "--at", bug["intro_commit"], "--base", bug["base_commit"], "--no-later-history",
              "--reviewer", "gpt-6.1-terra:high", "--no-project-memory", "--no-github-events",
              "--no-github-credentials", "--subagents", "none", "--title", "review-arms carrier test", timeout=900)
sid = launch["session_id"]; d.wait_idle(sid)
(out).mkdir(parents=True, exist_ok=True); (out / "context.txt").write_text(d.context_text(bug))
cmd = "carrier-test-" + str(time.time_ns())
d.mj("message", "--session", sid, "--command-id", cmd, "--prompt-file", out / "context.txt")
t0 = time.monotonic()
while True:
    s = d.session(sid); o = s.get("last_turn_outcome") or {}
    if o.get("command_id") == cmd and s.get("is_idle"): break
    if time.monotonic() - t0 > 300: print("TIMEOUT"); break
    time.sleep(3)
pages = d.transcript(sid); d.save(out / "transcript.json", pages)
items = [i for p in pages for i in p["items"]]
tools = [i for i in items if i["role"] in ("tool", "terminal")]
agent = [json.dumps(i["body"])[:200] for i in items if i["role"] in ("agent", "message")]
print(json.dumps({"session": sid, "seconds": round(time.monotonic() - t0), "tool_items": len(tools), "agent": agent[-2:]}))
d.mj("destroy", "--session", sid)
