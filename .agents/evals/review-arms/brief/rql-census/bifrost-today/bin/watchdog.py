#!/usr/bin/env python3
"""Run a command; poll its process tree RSS every 2 s; SIGKILL the tree at the cap.
Prints one JSON line to the file given by --report with exit, peak_rss_kib,
cap_kill, rss_at_kill_kib, oom_kill delta, wall."""
import json, os, signal, subprocess, sys, time, argparse
ap = argparse.ArgumentParser()
ap.add_argument('--cap-gib', type=float, default=48)
ap.add_argument('--report', required=True)
ap.add_argument('--stdout', required=True)
ap.add_argument('--stderr', required=True)
ap.add_argument('--timeout', type=float, default=7200)
ap.add_argument('cmd', nargs=argparse.REMAINDER)
a = ap.parse_args()
cmd = a.cmd[1:] if a.cmd and a.cmd[0] == '--' else a.cmd

def oomk():
    for l in open('/sys/fs/cgroup/memory.events'):
        k, v = l.split()
        if k == 'oom_kill': return int(v)

def tree_rss(root):
    kids = {}
    for p in os.listdir('/proc'):
        if not p.isdigit(): continue
        try:
            st = open(f'/proc/{p}/stat').read()
            ppid = int(st.rsplit(')', 1)[1].split()[1])
            kids.setdefault(ppid, []).append(int(p))
        except Exception: pass
    tot, stack = 0, [root]
    while stack:
        q = stack.pop()
        try:
            for l in open(f'/proc/{q}/status'):
                if l.startswith('VmRSS:'): tot += int(l.split()[1]); break
        except Exception: pass
        stack.extend(kids.get(q, []))
    return tot

cap = int(a.cap_gib * 1024 * 1024)
o0 = oomk(); t0 = time.time()
p = subprocess.Popen(cmd, stdout=open(a.stdout, 'wb'), stderr=open(a.stderr, 'wb'), start_new_session=True)
peak = 0; killed = None; timed_out = False
while p.poll() is None:
    r = tree_rss(p.pid); peak = max(peak, r)
    if r >= cap:
        killed = r; os.killpg(p.pid, signal.SIGKILL); break
    if time.time() - t0 > a.timeout:
        timed_out = True; os.killpg(p.pid, signal.SIGKILL); break
    time.sleep(2)
p.wait()
rep = dict(cmd=cmd, exit=p.returncode, wall=round(time.time()-t0, 1), peak_rss_kib=peak,
           cap_kib=cap, cap_kill=killed is not None, rss_at_kill_kib=killed,
           timed_out=timed_out, oom_kill_delta=oomk()-o0)
open(a.report, 'w').write(json.dumps(rep) + '\n')
print(json.dumps(rep))
sys.exit(0)
