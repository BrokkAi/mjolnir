#!/usr/bin/env python3
"""Build the live GLiDE attention corpus (ExecPlan attention-live-recalibration, Milestone 1).

Run with `python3 -I scripts/attention-live-corpus.py --out DIR`. Standard library only.
Everything it writes goes to DIR (private transcript text never enters the
repository). It reads the daemon store only through read-only SQL.

Inputs, all under DIR unless overridden:
  raw-decision-logs/<worker>/jev-decisions/decisions.*.jsonl  (frozen copy)
  raw-relay-journals/<worker>/{relay-journal/*,relay-state.json}
    (copied from the live workers directory for the same workers when absent)
"""
import argparse, collections, datetime, glob, gzip, hashlib, json, os, random, re
import shutil, sqlite3, subprocess, sys

UTC = datetime.timezone.utc
STORE = os.path.expanduser("~/.local/share/mjolnir/mj.sqlite3")
WORKERS = os.path.expanduser("~/.local/share/mjolnir/workers")
CUTOFF_MS = int(datetime.datetime(2026, 11, 12, tzinfo=UTC).timestamp() * 1000)
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# ---- port of mj_core::activity::eot_glide::action and glide_attention -------
# Rule versions (git): R0 2e95f7097 (required cut .80), R1 557b2e3b0 (.2203),
# R2 5eba12d1b (.1584 + demote; current). R0/R1 keep the continuation branch
# that 6dfb81125 removed (it returned Uncertain while continuation was off).
CUTS = {0: 0.80, 1: 0.2203035029419128, 2: 0.15844652713727195}
DEMOTE_CEILING = 0.5
IMPORTANT = 0.488819272011068
FLOOR = 0.90
FINISHED = 0.9162209700222781
WAITING = 0.40
SAFE_NONE = 0.85
CURRENT = 2


def prob(g, axis, name):
    v = ((g.get(axis) or {}).get("probabilities") or {}).get(name)
    return float(v) if v is not None else 0.0


def action(g, version=CURRENT, authorization_complete=True):
    p = lambda a, n: prob(g, a, n)
    if p("failure", "quota") >= FLOOR:
        return "recover_quota"
    req = p("input", "required")
    demoted = version == 2 and req < DEMOTE_CEILING and p("attention", "important") >= IMPORTANT
    if req >= CUTS[version] and not demoted:
        return "await_input"
    if p("failure", "transient_provider") >= FLOOR:
        return "retry_provider"
    if p("failure", "other") >= FLOOR:
        return "await_input"
    safe = p("failure", "none") >= FLOOR
    if version < 2:
        cont = (safe and max(p("input", "none"), p("input", "redundant_request")) >= FLOOR
                and min(p("turn_state", "continue"), p("work", "authorized_unfinished")) >= FLOOR
                and authorization_complete)
        if cont:
            return "uncertain"
    if safe and p("input", "none") >= SAFE_NONE:
        if p("turn_state", "finished") >= FINISHED:
            return "finished"
        w = p("work", "waiting")
        if w >= WAITING and all(w >= p("work", k) for k in ("authorized_unfinished", "finished", "unclear")):
            return "wait"
    return "uncertain"


def attention(g, act):
    """glide_attention: Important unless the action is AwaitInput."""
    return "important" if act != "await_input" and prob(g, "attention", "important") >= IMPORTANT else None


def demoted_turn(g):
    req = prob(g, "input", "required")
    return CUTS[CURRENT] <= req < DEMOTE_CEILING and prob(g, "attention", "important") >= IMPORTANT


# ---- generated prompts (mj_core::continuation) -------------------------------
def generated_prefixes():
    src = open(os.path.join(REPO, "mj-core/src/continuation.rs"), encoding="utf-8").read()
    body = src.split("generated_prompts! {", 1)[1].split("\n}\n", 1)[0]
    return re.findall(r'=>\s*"([a-z-]+-)"', body)


RETRY_PREFIXES = ("server-retry-", "capacity-retry-", "auth-retry-")
PARENT_WAIT = "One or more sub-agents finished. Call wait to collect their reports."


def is_genuine(stable_id, text, prefixes):
    t = text.lstrip()
    if t.startswith("[HARNESS NOTE:") or text.strip() == PARENT_WAIT:
        return False
    cid = stable_id[5:] if stable_id.startswith("user:") else stable_id
    for p in prefixes:
        if cid.startswith(p):
            if p in RETRY_PREFIXES and not cid[len(p):].isdigit():
                continue
            return False
    return True


ANSWER_WORDS = re.compile(
    r"^(yes|yeah|yep|yup|y|no|nope|n|ok|okay|sure|go|go ahead|do it|proceed|approved?|lgtm|"
    r"sounds good|please do|go for it|ship it|1|2|3|a|b|c|option \w+|the (first|second|third) one)\b", re.I)


def answer_shaped(text):
    t = text.strip()
    if any(line.lstrip().startswith(">") for line in t.splitlines()):
        return True
    return len(t) <= 80 and bool(ANSWER_WORDS.match(t))


# ---- helpers -----------------------------------------------------------------
def iso(ms):
    return datetime.datetime.fromtimestamp(ms / 1000, UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def day(ms):
    return datetime.datetime.fromtimestamp(ms / 1000, UTC).strftime("%Y-%m-%d")


def sha(o):
    return hashlib.sha256(json.dumps(o, sort_keys=True).encode()).hexdigest()[:12]


def table(rows, header):
    out = ["| " + " | ".join(header) + " |", "|" + "|".join("---" for _ in header) + "|"]
    out += ["| " + " | ".join(str(c) for c in r) + " |" for r in rows]
    return "\n".join(out) + "\n"


def connect():
    c = sqlite3.connect(f"file:{STORE}?mode=ro", uri=True, timeout=30)
    c.execute("PRAGMA query_only=1")
    return c


def body_text(body_json):
    b = json.loads(body_json)
    if b.get("kind") == "user":
        return "\n".join(x.get("text", "") for x in b.get("content", []) if x.get("type") == "text")
    if b.get("kind") == "agent":
        return "".join((c.get("content") or {}).get("text", "") for c in b.get("chunks", [])
                       if (c.get("content") or {}).get("type") == "text")
    return ""


# ---- stage 1: decision logs --------------------------------------------------
def load_assessments(raw):
    lines = collections.defaultdict(list)
    nlines = 0
    files = sorted(glob.glob(os.path.join(raw, "*/jev-decisions/decisions.*.jsonl")))
    for f in files:
        for l in open(f, errors="replace"):
            try:
                d = json.loads(l)
            except ValueError:
                continue
            if d.get("kind") != "assessment":
                continue
            nlines += 1
            lines[d["id"]].append(d)
    recs = {}
    for i, ls in lines.items():
        ls.sort(key=lambda d: d["updated_at_ms"])
        asm = lambda d: (d.get("technical") or {}).get("assessment") or {}
        # Lines of a pending assessment can carry the previous assessment's
        # glide and action (observed: a pending line with glide and action,
        # 0-30 ms after completion). The verdict line is the first line past
        # `pending` that carries probabilities.
        real = [d for d in ls if asm(d).get("glide") and d["status"] != "pending"]
        if not real:
            continue
        first, final = real[0], real[-1]
        evidence = next((asm(d)["evidence"] for d in reversed(ls) if asm(d).get("evidence")), None)
        questions = next(((d.get("technical") or {}).get("questions") for d in reversed(ls)
                          if (d.get("technical") or {}).get("questions")), None)
        a = asm(first)
        recs[i] = dict(
            id=i, session=first["session_id"], revision=a["revision"], ordinal=a["completed_ordinal"],
            turn_id=a["turn_id"], completed_at_ms=a["completed_at_ms"],
            glide=a["glide"], logged_action=a["action"], attention_logged=a.get("attention"),
            final_status=final["status"], final_reason=asm(final).get("reason"),
            final_action=asm(final).get("action"), glide_final_differs=asm(final)["glide"] != a["glide"],
            n_lines=len(ls), evidence=evidence, questions_hash=sha(questions) if questions else None,
            input_hash=sha((questions or {}).get("input")), attention_hash=sha((questions or {}).get("attention")),
            started_at_ms=first["started_at_ms"], verdict_at_ms=first["updated_at_ms"],
        )
    return recs, nlines, len(lines), len(files)


def choose_turns(recs):
    by = collections.defaultdict(list)
    for r in recs.values():
        by[(r["session"], r["ordinal"])].append(r)
    turns = {}
    for k, rs in by.items():
        live = [r for r in rs if r["final_status"] != "superseded"]
        pick = max(live or rs, key=lambda r: r["revision"])
        pick = dict(pick, n_records=len(rs), only_superseded=not live)
        turns[k] = pick
    return turns


# ---- stage 2: store ----------------------------------------------------------
def store_sessions(db, ids):
    q = ",".join("?" * len(ids))
    out = {}
    for row in db.execute(
        f"""SELECT s.session_id,s.title,s.harness_kind,s.last_profile,s.target_template_id,s.state,
            s.mjolnir_subagents,s.no_mailbox,s.oracle_choice,s.review_json,s.project_directory,
            c.created_at,c.workspace_id,
            (SELECT parent_session_id FROM subagent_sessions x WHERE x.child_session_id=s.session_id)
            FROM sessions s JOIN session_contexts c USING(session_id) WHERE s.session_id IN ({q})""", ids):
        pd = row[10]
        pd = pd.decode("utf-8", "replace") if isinstance(pd, bytes) else (pd or "")
        out[row[0]] = dict(title=row[1], harness=row[2], profile=row[3], target=row[4], state=row[5],
                           mjolnir_subagents=row[6], no_mailbox=row[7], oracle=row[8],
                           review=row[9], project_dir=pd, created_at=row[11], workspace=row[12],
                           parent=row[13])
    return out


def user_items(db, sid, prefixes):
    items = []
    for pos, stable, ms, body in db.execute(
        "SELECT position,stable_id,created_at_ms,body_json FROM materialized_transcript_items "
        "WHERE session_id=? AND stable_id>='user:' AND stable_id<'user;' ORDER BY position", (sid,)):
        text = body_text(body)
        items.append(dict(position=pos, stable_id=stable, created_at_ms=ms, text=text,
                          genuine=is_genuine(stable, text, prefixes)))
    return items


def agent_items(db, sid, lo, hi):
    out = []
    for pos, stable, ms, body in db.execute(
        "SELECT position,stable_id,created_at_ms,body_json FROM materialized_transcript_items "
        "WHERE session_id=? AND position>? AND position<=? AND stable_id>='agent:' AND stable_id<'agent;' "
        "ORDER BY position", (sid, lo, hi)):
        t = body_text(body)
        if t.strip():
            out.append(dict(position=pos, text=t))
    return out


def time_of_ordinal(db, sid, ordinal):
    r = db.execute("SELECT created_at_ms FROM materialized_transcript_items WHERE session_id=? AND position<=? "
                   "ORDER BY position DESC LIMIT 1", (sid, ordinal)).fetchone()
    return r[0] if r else None


def child_intervals(db, parents):
    """Managed children's turn windows [start,end] in ms, per parent session."""
    q = ",".join("?" * len(parents))
    kids = db.execute(f"SELECT child_session_id,parent_session_id FROM subagent_sessions "
                      f"WHERE parent_session_id IN ({q})", parents).fetchall()
    out = collections.defaultdict(list)
    for child, parent in kids:
        for start_pos, body in db.execute(
                "SELECT turn_start_position,body FROM session_turn_usage WHERE session_id=?", (child,)):
            end = json.loads(body).get("completed_at_ms")
            start = None
            if start_pos is not None:
                r = db.execute("SELECT min(created_at_ms) FROM materialized_transcript_items "
                               "WHERE session_id=? AND position=?", (child, start_pos)).fetchone()
                start = r[0] if r else None
            if start is not None and end is not None:
                out[parent].append((start, end))
    return out, len(kids)


# ---- stage 3: dismissals -----------------------------------------------------
def snapshot_journals(raw, dest, log):
    workers = sorted(os.listdir(raw))
    os.makedirs(dest, exist_ok=True)
    for w in workers:
        d = os.path.join(dest, w)
        if os.path.exists(d):
            continue
        src = os.path.join(WORKERS, w)
        os.makedirs(d)
        if os.path.isdir(os.path.join(src, "relay-journal")):
            shutil.copytree(os.path.join(src, "relay-journal"), os.path.join(d, "relay-journal"))
        if os.path.exists(os.path.join(src, "relay-state.json")):
            shutil.copy2(os.path.join(src, "relay-state.json"), d)
        log(f"copied journals for {w}")
    return workers


def load_dismissals(jdir):
    """(session, command_id) -> dict from journals (with times) and handled_commands ledgers."""
    out = {}
    stats = collections.Counter()
    for w in sorted(os.listdir(jdir)):
        base = os.path.join(jdir, w)
        for f in sorted(glob.glob(os.path.join(base, "relay-journal", "*"))):
            stats["journal files"] += 1
            op = gzip.open if f.endswith(".gz") else open
            with op(f, "rb") as fh:
                for raw in fh:
                    if b"dismiss-question-" not in raw:
                        continue
                    try:
                        e = json.loads(raw)
                    except ValueError:
                        continue
                    cid = e.get("command_id")
                    if not cid or not cid.startswith("dismiss-question-"):
                        continue
                    o = e["observation"]
                    rec = out.setdefault((w, cid), dict(session=w, command_id=cid, sources=set()))
                    rec["sources"].add("journal")
                    if o["type"] == "command_queued":
                        q = o["data"]["command"]["data"]["question"]
                        rec.update(accepted_ordinal=e["ordinal"], accepted_at_ms=e["recorded_at_ms"],
                                   turn_id=q["turn_id"], question_ordinal=q["completed_ordinal"])
                    elif o["type"] == "command_completed":
                        rec["outcome"] = (o["data"].get("outcome") or {}).get("type")
                    elif o["type"] not in ("command_started",):
                        rec["outcome"] = o["type"]
        st = os.path.join(base, "relay-state.json")
        if os.path.exists(st):
            try:
                hc = json.load(open(st)).get("handled_commands") or {}
            except ValueError:
                hc = {}
            stats["ledger entries"] += len(hc)
            for cid, h in hc.items():
                if cid.startswith("dismiss-question-"):
                    rec = out.setdefault((w, cid), dict(session=w, command_id=cid, sources=set()))
                    rec["sources"].add("ledger")
                    rec.setdefault("accepted_ordinal", h.get("accepted_ordinal"))
                    rec.setdefault("outcome", (h.get("outcome") or {}).get("type"))
                    rec.setdefault("question_ordinal", int(cid.rsplit("-", 1)[1]))
    return out, stats


# ---- main --------------------------------------------------------------------
def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--seed", type=int, default=20261116)
    args = ap.parse_args()
    out = args.out
    raw = os.path.join(out, "raw-decision-logs")
    jdir = os.path.join(out, "raw-relay-journals")
    msgs = []
    log = lambda s: (print(s), msgs.append(s))

    prefixes = generated_prefixes()
    recs, nlines, nids, nfiles = load_assessments(raw)
    for r in recs.values():
        r["source"] = "localhost"
    local_workers = snapshot_journals(raw, jdir, log)
    worker_source = {w: "localhost" for w in local_workers}
    dismissals, dstats = load_dismissals(jdir)
    remote_root = os.path.join(out, "raw-remote-logs")
    for target in sorted(os.listdir(remote_root)) if os.path.isdir(remote_root) else []:
        tdir = os.path.join(remote_root, target)
        r2, nl2, ni2, nf2 = load_assessments(tdir)
        for r in r2.values():
            r["source"] = target
        recs.update(r2)
        nlines, nids, nfiles = nlines + nl2, nids + ni2, nfiles + nf2
        for w in sorted(os.listdir(tdir)):
            if os.path.isdir(os.path.join(tdir, w)):
                worker_source[w] = target
        d2, st2 = load_dismissals(tdir)
        dismissals.update(d2)
        dstats.update(st2)
    workers = sorted(worker_source)
    # Turns under older question wording have no attention answer or differ in
    # the input question; populations use the current wording only.
    cur_hash = sha(json.load(open(os.path.join(REPO, "mj-core/src/activity/eot_glide_questions.json"))))
    n_old = sum(1 for r in recs.values() if r["questions_hash"] != cur_hash)
    recs_all = recs
    recs = {i: r for i, r in recs_all.items() if r["questions_hash"] == cur_hash}
    turns = choose_turns(recs)
    turns_all = choose_turns(recs_all)
    db = connect()
    sess = store_sessions(db, workers)
    questions_hashes = collections.Counter((r["questions_hash"], r["input_hash"], r["attention_hash"])
                                           for r in recs.values())

    # -- exclusions (store metadata only)
    uitems = {s: user_items(db, s, prefixes) for s in workers}
    default_title = re.compile(r"^.+ via \S+$")
    excluded, borderline = {}, {}
    for s in workers:
        m = sess.get(s)
        if m is None:
            excluded[s] = "not in store"
            continue
        if m["parent"] is not None:
            excluded[s] = "managed sub-agent (never classified for attention)"
            continue
        people = [u for u in uitems[s] if u["genuine"]]
        agent_created = not default_title.match(m["title"])
        # An agent-created session's first prompt is the creator's, not a person's.
        persons = people[1:] if agent_created and people else people
        m["agent_created"], m["person_messages"] = agent_created, len(persons)
        m["first_message_agent_authored"] = agent_created
        why = []
        if m["no_mailbox"] == 1 and (m["review"] or "").find('"off"') >= 0:
            why.append("no_mailbox=1 and review off (programmatic judge/worker launch)")
        if m["project_dir"].startswith("/mnt/optane/tmp/"):
            why.append("working directory is a scratch directory under /mnt/optane/tmp")
        if why:
            excluded[s] = "; ".join(why)
        elif agent_created:
            borderline[s] = (f"title '{m['title']}' is not the default '<project> via <profile>'; "
                             f"{len(persons)} person messages after the first prompt")
    if not any(m.get("agent_created") for m in sess.values()):
        pass

    # -- dismissals keyed by session and question ordinal
    dis_by = collections.defaultdict(list)
    for (w, cid), d in dismissals.items():
        if d.get("accepted_ordinal") is not None:
            if d.get("accepted_at_ms") is None:
                d["accepted_at_ms"] = time_of_ordinal(db, w, d["accepted_ordinal"])
                d["time_estimated"] = True
            dis_by[w].append(d)
    for w in dis_by:
        dis_by[w].sort(key=lambda d: d["accepted_ordinal"])
    children, nkids = child_intervals(db, workers)

    # -- per-session simulation
    kept = {k: t for k, t in turns.items() if k[0] not in excluded}
    rows, episodes = [], []
    for s in sorted({k[0] for k in kept}):
        ts = sorted((t for k, t in kept.items() if k[0] == s), key=lambda t: t["ordinal"])
        m = sess[s]
        persons = [u for u in uitems[s] if u["genuine"]]
        if m["agent_created"] and persons:
            persons = persons[1:]
        dis = [d for d in dis_by.get(s, []) if d.get("outcome") in (None, "notice_recorded")]
        bounds = [(u["position"], "message", u) for u in persons] + \
                 [(d["accepted_ordinal"], "dismissal", d) for d in dis]
        bounds.sort(key=lambda b: (b[0], b[1]))
        # Latch simulation under two action sources.
        state = {}
        for source in ("current", "logged"):
            open_req, glyphs = None, {}
            ev = sorted([(t["ordinal"], 1, t) for t in ts] + [(b[0], 0, b) for b in bounds], key=lambda e: (e[0], e[1]))
            for _, kind, x in ev:
                if kind == 0:
                    if open_req is not None and x[0] > open_req:
                        open_req = None  # a person message or dismissal after the request clears it
                    continue
                t = x
                act = action(t["glide"], CURRENT) if source == "current" else t["logged_action"]
                if act == "await_input":
                    open_req = t["ordinal"]
                if open_req is not None:
                    glyph = "?"
                elif attention(t["glide"], act) and (source == "logged" or True):
                    glyph = "!"
                else:
                    glyph = "✓"
                glyphs[t["ordinal"]] = (glyph, open_req)
            state[source] = glyphs
        # Episodes: boundaries are person messages and question dismissals.
        edges = [(0, "session_start", None)] + [(b[0], b[1], b[2]) for b in bounds]
        eps = []
        for n, (pos, kind, obj) in enumerate(edges):
            end = edges[n + 1][0] if n + 1 < len(edges) else float("inf")
            eps.append(dict(index=n, start_ordinal=pos, start_kind=kind, end_ordinal=None if end == float("inf") else end,
                            end_kind=edges[n + 1][1] if n + 1 < len(edges) else "still_open",
                            end_obj=edges[n + 1][2] if n + 1 < len(edges) else None, turns=[]))
        for t in ts:
            for e in eps:
                if e["start_ordinal"] < t["ordinal"] <= (e["end_ordinal"] if e["end_ordinal"] is not None else float("inf")) \
                        or (e["index"] == 0 and t["ordinal"] <= (e["end_ordinal"] or float("inf"))):
                    e["turns"].append(t)
                    t["_episode"] = e["index"]
                    break
        for t in ts:
            ep = next(e for e in eps if e["index"] == t.get("_episode", 0))
            g = t["glide"]
            cur = action(g, CURRENT, bool((t["evidence"] or {}).get("authorization", {}).get("authorization_complete", True)))
            glyph, origin = state["current"][t["ordinal"]]
            lglyph, lorigin = state["logged"][t["ordinal"]]
            nxt = next((u for u in persons if u["position"] > t["ordinal"]), None)
            nxtd = next((d for d in dis if d["accepted_ordinal"] > t["ordinal"]), None)
            own = next((d for d in dis if d["question_ordinal"] == t["ordinal"]), None)
            held = [c for c in children.get(s, []) if c[0] <= t["completed_at_ms"] < c[1]]
            fit = [v for v in (0, 1, 2) if action(g, v, True) == t["logged_action"]]
            rows.append(dict(
                session=s, target=worker_source[s], store_target=m["target"], turn_id=t["turn_id"], completed_ordinal=t["ordinal"], completed_at_ms=t["completed_at_ms"],
                completed_at=iso(t["completed_at_ms"]), record_id=t["id"], n_records=t["n_records"],
                only_superseded=t["only_superseded"], final_status=t["final_status"], final_reason=t["final_reason"],
                glide_changed_after_verdict=t["glide_final_differs"], questions_hash=t["questions_hash"],
                input_wording_hash=t["input_hash"], attention_wording_hash=t["attention_hash"],
                glide=g, logged_action=t["logged_action"], rule_versions_matching_logged=fit,
                action_current=cur, attention_current=attention(g, cur), demoted=demoted_turn(g),
                verdict_glyph="?" if cur == "await_input" else ("!" if attention(g, cur) else "✓"),
                displayed_glyph=glyph, latched_from_ordinal=origin if origin != t["ordinal"] else None,
                latched=glyph == "?" and origin != t["ordinal"],
                displayed_glyph_logged_actions=lglyph,
                held_by_managed_children=len(held), hold_released_ms=max((c[1] for c in held), default=None),
                episode=t.get("_episode", 0),
                evidence_messages=len(((t["evidence"] or {}).get("authorization") or {}).get("messages", [])),
                next_person_message=None if not nxt else dict(
                    position=nxt["position"], created_at_ms=nxt["created_at_ms"], delay_s=(nxt["created_at_ms"] - t["completed_at_ms"]) / 1000,
                    answer_shaped=answer_shaped(nxt["text"]), text=nxt["text"]),
                next_dismissal=None if not nxtd else dict(ordinal=nxtd["accepted_ordinal"], at_ms=nxtd["accepted_at_ms"],
                                                           delay_s=(nxtd["accepted_at_ms"] - t["completed_at_ms"]) / 1000
                                                           if nxtd.get("accepted_at_ms") else None),
                dismissed_own_question=None if not own else dict(ordinal=own["accepted_ordinal"], at_ms=own["accepted_at_ms"],
                                                                 sources=sorted(own["sources"])),
            ))
        # episodes -> records
        for e in eps:
            tt = e["turns"]
            if not tt:
                continue
            verdicts = [(t, action(t["glide"], CURRENT)) for t in tt]
            onset = next(((t, a) for t, a in verdicts if a == "await_input"), None)
            rec = dict(session=s, episode=e["index"], start_ordinal=e["start_ordinal"], start_kind=e["start_kind"],
                       n_turns=len(tt), onset_ordinal=None)
            if onset:
                t = onset[0]
                later = [x for x in verdicts if x[0]["ordinal"] > t["ordinal"]]
                clear = e["end_kind"]
                nobj = e["end_obj"]
                rec.update(onset_ordinal=t["ordinal"], onset_turn_id=t["turn_id"], onset_completed_at_ms=t["completed_at_ms"],
                           onset_input_required=prob(t["glide"], "input", "required"),
                           onset_attention_important=prob(t["glide"], "attention", "important"),
                           repeat_question_verdicts=sum(1 for _, a in later if a == "await_input"),
                           other_verdicts_under_latch=sum(1 for _, a in later if a != "await_input"),
                           cleared_by={"message": "answer", "dismissal": "dismissal", "still_open": "still_open"}[clear],
                           next_message_answer_shaped=answer_shaped(nobj["text"]) if clear == "message" else None)
            episodes.append(rec)

    # -- onset digest sampling
    by_turn = {(r["session"], r["completed_ordinal"]): r for r in rows}
    onsets = [e for e in episodes if e["onset_ordinal"] is not None]
    fixed = [e for e in onsets if CUTS[CURRENT] <= e["onset_input_required"] < DEMOTE_CEILING]
    rng = random.Random(args.seed)
    rest = [e for e in onsets if e not in fixed]
    rng.shuffle(rest)
    chosen = list(fixed)
    per = collections.Counter(e["session"] for e in chosen)
    for e in rest:
        if len(chosen) >= 40:
            break
        if per[e["session"]] < 3:
            chosen.append(e)
            per[e["session"]] += 1
    chosen.sort(key=lambda e: (e["session"], e["onset_ordinal"]))

    # ---- outputs
    with open(os.path.join(out, "turns.jsonl"), "w") as f:
        for r in sorted(rows, key=lambda r: (r["session"], r["completed_ordinal"])):
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    with open(os.path.join(out, "episodes.jsonl"), "w") as f:
        for r in episodes:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    write_port_check(out, recs_all, turns_all)
    cov = write_coverage(out, db, turns_all, sess, workers, excluded, set(local_workers))
    write_dismissals(out, db, dismissals, dstats, rows, workers)
    write_exclusions(out, sess, excluded, borderline, db, turns_all)
    write_display_counts(out, rows, episodes)
    write_digest(out, db, chosen, by_turn, uitems, prefixes, args.seed, sess)
    json.dump(dict(questions_hashes={f"{a}/{b}/{c}": n for (a, b, c), n in questions_hashes.items()},
                   current_rule="R2 5eba12d1b (+6dfb81125)", assessment_lines=nlines, assessment_ids=nids,
                   log_files=nfiles, turns_all=len(turns), turns_kept=len(rows), sessions_excluded=len(excluded),
                   managed_children_seen=nkids, seed=args.seed, current_wording_hash=cur_hash,
                   records_old_wording=n_old, turns_all_wordings=len(turns_all),
                   workers_by_source=dict(collections.Counter(worker_source.values()))),
              open(os.path.join(out, "manifest.json"), "w"), indent=1)

    shown_q = [r for r in rows if r["displayed_glyph"] == "?"]
    log(f"assessment lines {nlines}, record ids {nids}, with verdict {len(recs)}, turns {len(turns)}, kept {len(rows)}, "
        f"excluded sessions {len(excluded)} ({sum(1 for k in turns if k[0] in excluded)} turns)")
    log(f"verdict ? {sum(r['action_current']=='await_input' for r in rows)}, displayed ? {len(shown_q)}, "
        f"displayed ! {sum(r['displayed_glyph']=='!' for r in rows)}")
    dq = [r for r in rows if r["action_current"] == "await_input" and r["dismissed_own_question"]]
    log(f"? verdicts dismissed {len(dq)}")
    nonq = [r for r in rows if r["action_current"] != "await_input" and r["next_person_message"]]
    log(f"answer-shaped next messages among non-? turns {sum(r['next_person_message']['answer_shaped'] for r in nonq)} of {len(nonq)}")
    log("required bands: [.05,.158) %d, [.01,.05) %d, <.01 %d" % tuple(
        sum(1 for r in rows if lo <= prob(r['glide'], 'input', 'required') < hi) for lo, hi in
        ((0.05, CUTS[2]), (0.01, 0.05), (0, 0.01))))


def write_port_check(out, recs, turns):
    by = collections.defaultdict(collections.Counter)
    bad = []
    for r in recs.values():
        fit = tuple(v for v in (0, 1, 2) if action(r["glide"], v, True) == r["logged_action"])
        by[r["questions_hash"]][fit] += 1
        if CURRENT not in fit:
            bad.append((r, fit))
    lines = ["# Port check\n",
             "The Python port of `eot_glide::action` and `glide_attention` is run on the first non-pending line of "
             "each assessment record that carries probabilities, and compared with the logged action. R0 = 2e95f7097 "
             "(.80 cut, 2026-11-12 10:32Z), R1 = 557b2e3b0 (.2203 cut, 14:07Z), R2 = 5eba12d1b (.1584 cut + demote, "
             "17:16Z; continuation branch removed by 6dfb81125 on 11-13, equivalent). R2 is current.\n",
             f"Records with a verdict: {len(recs)}.\n"]
    for h, c in sorted(by.items(), key=lambda kv: str(kv[0])):
        rows_ = [("+".join(f"R{v}" for v in k) or "none", n) for k, n in sorted(c.items())]
        lines.append(f"## Questions wording hash `{h}` ({sum(c.values())} records)\n")
        lines.append(table(rows_, ["rule versions that reproduce the logged action", "records"]))
    n2 = sum(1 for r in recs.values() if action(r["glide"], CURRENT, True) == r["logged_action"])
    lines.append(f"\nCurrent rule (R2) agreement over all records: {n2}/{len(recs)}.\n")
    r2_commit = int(datetime.datetime(2026, 11, 12, 17, 16, tzinfo=UTC).timestamp() * 1000)
    after = [r for r in recs.values() if r["completed_at_ms"] >= r2_commit]
    n_after = sum(1 for r in after if action(r["glide"], CURRENT, True) == r["logged_action"])
    lines.append(f"R2 agreement for records completed after the R2 commit time (2026-11-12 17:16Z): {n_after}/{len(after)}.\n")
    att = [(r, attention(r["glide"], r["logged_action"])) for r in recs.values() if r["questions_hash"] and r["attention_hash"] != sha(None)]
    att_bad = [r for r, a in att if (r["attention_logged"] or None) != ("important" if a else None)]
    lines.append(f"Logged `attention` field vs `glide_attention` port (records with an attention question): "
                 f"{len(att) - len(att_bad)}/{len(att)} agree.\n")
    lines.append("\n## Disagreements with R2\n")
    for r, fit in sorted(bad, key=lambda b: b[0]["completed_at_ms"]):
        ver = "+".join(f"R{v}" for v in fit) or "none"
        lines.append(f"- `{r['id']}` {iso(r['completed_at_ms'])} logged `{r['logged_action']}`, R2 gives "
                     f"`{action(r['glide'])}`; reproduced by: {ver}; final status {r['final_status']}; "
                     f"glide changed after the verdict: {r['glide_final_differs']}\n")
    lines.append("""
Explanations (likely, not confirmed by a code trace): records reproduced only by R0 or R1 were produced by a
worker binary that had not yet picked up R2 (each worker runs its own `hel` copy). A record reproduced by no
version (one record) is NOT explained by a rule version. The likely cause is the refresh path in `apply_glide_assessment` (mj-worker/src/relay/verdict.rs):
when an assessment already holds glide and action, a later GLiDE answer replaces the probabilities but keeps
the action. Lines of a `pending` assessment can carry the previous assessment's glide and action, which would
send a new turn down that path (unconfirmed: the pending lines of this record carry R2-consistent probabilities
that differ from the assessed line). Per the study brief the port was not adjusted to fit it.
""")
    open(os.path.join(out, "port-check.md"), "w").write("".join(lines))


def write_coverage(out, db, turns, sess, workers, excluded, local_workers):
    logged = collections.Counter()
    first_logged = {}
    for (s, o), t in turns.items():
        logged[(s, day(t["completed_at_ms"]))] += 1
    store = collections.Counter()
    rows = db.execute("SELECT u.session_id, json_extract(u.body,'$.completed_at_ms') FROM session_turn_usage u "
                      "WHERE json_extract(u.body,'$.completed_at_ms')>=?", (CUTOFF_MS,)).fetchall()
    ids = sorted({r[0] for r in rows})
    meta = {}
    for i in range(0, len(ids), 500):
        chunk = ids[i:i + 500]
        q = ",".join("?" * len(chunk))
        for sid, tgt, harness, child in db.execute(
                f"SELECT s.session_id,s.target_template_id,s.harness_kind,"
                f"(SELECT 1 FROM subagent_sessions x WHERE x.child_session_id=s.session_id) FROM sessions s "
                f"WHERE s.session_id IN ({q})", chunk):
            meta[sid] = (tgt, harness, bool(child))
    cat = collections.Counter()
    top_store = collections.Counter()
    for sid, ms in rows:
        tgt, harness, child = meta.get(sid, ("?", "?", False))
        d = day(ms)
        if child:
            cat[(d, "managed sub-agent (not classified)")] += 1
            continue
        local = tgt == "localhost"
        has_log = sid in workers
        key = "top-level, log copied" if has_log else ("top-level, local, no GLiDE log" if local else "top-level, remote target (log not on this host)")
        cat[(d, key)] += 1
        if has_log:
            store[(sid, d)] += 1
            top_store[d] += 1
    days = sorted({d for d, _ in cat} | {d for (_, d) in logged})
    lines = ["# Coverage\n", "Completed turns in the daemon store (`session_turn_usage`) per UTC day since 2026-11-12, by "
             "category, against logged turns with a GLiDE verdict (all kept and excluded sessions).\n"]
    keys = sorted({k for _, k in cat})
    lines.append(table([(d, *[cat[(d, k)] for k in keys], sum(n for (dd, s), n in
                         [((d2, s2), n2) for (s2, d2), n2 in logged.items()] if dd == d)) for d in days],
                        ["day", *keys, "logged turns with verdict"]))
    lines.append("\n## Sessions with copied logs, per day: store turns vs logged turns\n")
    under = []
    rows2 = []
    for s in sorted(workers):
        for d in days:
            st, lg = store[(s, d)], logged[(s, d)]
            if st or lg:
                rows2.append((s[:8], sess.get(s, {}).get("title", "")[:30], d, st, lg, f"{lg/st:.0%}" if st else "-"))
                if st and lg < 0.9 * st and st - lg >= 3:
                    under.append((s[:8], d, st, lg))
    lines.append(table(rows2, ["session", "title", "day", "store turns", "logged", "ratio"]))
    lines.append("\n## Under-covered (logged < 90% of store turns and at least 3 missing)\n")
    lines.append(table(under, ["session", "day", "store turns", "logged"]) if under else "None.\n")
    lines.append("""
Reading the gaps: a store turn without a logged verdict has one of these causes. (1) The session runs on a
target whose worker directory is not on this host (container targets): its logs are unavailable. (2) The local
worker never classified the turn with GLiDE (an older `hel` binary, no log file with `glide-turn-v1`, or a turn
that ended before classification and was superseded). (3) Log rotation: each worker keeps at most four files of
about 8 MiB, so busy sessions lose their oldest records; this shows as a session whose early days have ratio
near 0 and whose later days are near 100%. Distinguishing (2) from (3) per session needs the first logged time
against the first store turn; the table above shows it by day.
""")
    lines.append("\n## When each worker started logging GLiDE verdicts (live logs, read-only)\n")
    rows3 = []
    for s_ in sorted(local_workers):
        glide_t, jev_t, nfiles, first_any = [], [], 0, None
        for f in glob.glob(os.path.join(WORKERS, s_, "jev-decisions", "decisions.*.jsonl")):
            nfiles += 1
            for l in open(f, errors="replace"):
                if '"kind":"assessment"' not in l.replace(" ", ""):
                    continue
                try:
                    d = json.loads(l)
                except ValueError:
                    continue
                c = (d.get("technical") or {}).get("contract")
                (glide_t if c == "glide-turn-v1" else jev_t).append(d["started_at_ms"])
        rows3.append((s_[:8], nfiles, iso(min(glide_t)) if glide_t else "-", iso(min(jev_t)) if jev_t else "-",
                      iso(max(jev_t)) if jev_t else "-"))
    lines.append(table(rows3, ["session", "log files now", "first GLiDE record", "first Jev record", "last Jev record"]))
    lines.append("\nA session whose Jev records end where its GLiDE records begin was not rotated: it simply ran an older "
                 "worker binary. A session with four log files whose first record of either kind is later than its first "
                 "store turn since 11-12 lost records to rotation.\n")
    open(os.path.join(out, "coverage.md"), "w").write("".join(lines))
    return cat


def write_dismissals(out, db, dismissals, dstats, rows, workers):
    git = ""
    try:
        git = subprocess.run(["git", "-C", REPO, "log", "-S", "RELAY_DISMISS_QUESTION_PROTOCOL", "--format=%h %ad", "--date=iso",
                              "--", "mj-core/src/relay.rs"], capture_output=True, text=True, timeout=60).stdout.strip().splitlines()
        git = git[-1] if git else ""
    except Exception as e:  # report, do not hide
        git = f"unavailable ({e})"
    recs = sorted(dismissals.values(), key=lambda d: (d.get("accepted_at_ms") or 0))
    receipts = db.execute("SELECT session_id,through_event_ordinal,updated_at FROM client_read_frontiers "
                          "WHERE client_id='notification-dismissal'").fetchall()
    rc = {r[0]: r for r in receipts}
    qrows = [r for r in rows if r["action_current"] == "await_input"]
    answered = [r for r in qrows if r["dismissed_own_question"] is None]
    dis_q = [r for r in qrows if r["dismissed_own_question"]]
    dis_noreply = [r for r in dis_q if not (r["next_person_message"] and r["next_person_message"]["created_at_ms"] <= r["dismissed_own_question"]["at_ms"])]
    lines = ["# Dismissals\n",
             "Where `?` dismissals are recorded (NotificationDismissal::Question, mj-controller/src/daemon/actions.rs):\n\n"
             "1. Worker relay journal: `RelayCommand::DismissQuestion`, command id `dismiss-question-<question completed ordinal>`; "
             "events `command_queued` (carries turn id, question ordinal, wall-clock `recorded_at_ms`), `command_started`, "
             "`command_completed` (`record_notice`).\n"
             "2. Worker `relay-state.json` `handled_commands` ledger: the newest 512 terminal command ids "
             "(`RELAY_RETRY_ID_RETENTION`) with `accepted_ordinal`, `terminal_ordinal` and outcome, no wall-clock time.\n"
             "3. Daemon store `client_read_frontiers` row with client id `notification-dismissal`: one row per session holding only the "
             "highest ordinal (`max`) and the last update time. It also receives `ReadThrough` dismissals of `!`/unread, so it cannot "
             "separate question dismissals and keeps no history.\n"
             "4. Decision log: a superseded assessment with reason `question_dismissed` (seen in the logs; one record).\n\n"]
    lines.append("## Compaction\n\nThe journal drops every event at or below the controller-acknowledged frontier "
                 "(`garbage_collect_relay_history` / `rewrite_relay_journal`, mj-worker/src/relay/journal.rs), and deletes the sealed `.gz` "
                 "segments. A dismissal therefore survives in the journal only until the next acknowledgement; afterwards only the "
                 "ledger entry (no time; up to the newest 512 terminal commands per worker) remains. Times for ledger-only entries are "
                 "estimated from the transcript item at or before the accepted ordinal.\n\n")
    lines.append(f"Dismissal protocol introduced (RELAY_DISMISS_QUESTION_PROTOCOL = 48): first commit touching it: {git}.\n\n")
    lines.append(f"Scanned {dstats['journal files']} journal files and {dstats['ledger entries']} ledger entries for {len(workers)} workers.\n\n")
    lines.append(f"Dismissal commands found: {len(recs)}; by source: "
                 f"{dict(collections.Counter('+'.join(sorted(d['sources'])) for d in recs))}.\n\n")
    if recs:
        t0 = min((d.get("accepted_at_ms") for d in recs if d.get("accepted_at_ms")), default=None)
        lines.append(f"Earliest dismissal found: {iso(t0) if t0 else 'unknown'}.\n\n")
    lines.append("## Dismissals found\n")
    lines.append(table([(d["session"][:8], d["command_id"], d.get("outcome"), "+".join(sorted(d["sources"])),
                         iso(d["accepted_at_ms"]) if d.get("accepted_at_ms") else "?",
                         "estimated" if d.get("time_estimated") else "") for d in recs],
                        ["session", "command", "outcome", "sources", "time", ""]))
    lines.append("\n## Store receipts for these workers\n")
    lines.append(table([(s[:8], rc[s][1], rc[s][2]) for s in sorted(workers) if s in rc], ["session", "through ordinal", "updated"]))
    lines.append(f"\n## Shown-? turns (current rule, verdict `?`)\n\nverdict-? turns: {len(qrows)}; with a recorded dismissal of that "
                 f"question: {len(dis_q)}; of those, dismissed with no person message before the dismissal: {len(dis_noreply)}.\n"
                 f"Coverage caveat: container-target sessions and anything compacted away are invisible here, so these counts are lower bounds.\n")
    open(os.path.join(out, "dismissals.md"), "w").write("".join(lines))


def write_exclusions(out, sess, excluded, borderline, db, turns):
    lines = ["# Exclusions\n", "Rule: excluded when store metadata shows an automated launch: `no_mailbox=1` with review off "
             "(programmatic judge launches), or a working directory under `/mnt/optane/tmp` (agent scratch directory). "
             "Borderline: title does not follow the default `<project> via <profile>` (created by an agent) but the rule "
             "above does not apply; these stay in the corpus.\n"]
    cnt = collections.Counter(k[0] for k in turns)
    lines.append("\n## Excluded sessions\n")
    lines.append(table([(s[:8], sess.get(s, {}).get("title", ""), cnt[s], why) for s, why in sorted(excluded.items())],
                       ["session", "title", "turns with verdict", "reason"]))
    lines.append("\n## Borderline (kept)\n")
    lines.append(table([(s[:8], sess[s]["title"], cnt[s], why) for s, why in sorted(borderline.items())],
                       ["session", "title", "turns with verdict", "note"]))
    pats = ("%judge%", "%grad%", "%eval%", "%panel%", "%smoke%", "%jev-panel-work%")
    lines.append("\n## Title patterns across the whole store (not necessarily in the corpus)\n")
    rows = []
    for p in pats:
        n = db.execute("SELECT count(*) FROM sessions WHERE title LIKE ?", (p,)).fetchone()[0]
        rows.append((p, n))
    lines.append(table(rows, ["pattern", "sessions"]))
    lines.append("\nNone of the evaluation, grading, `jev-panel-work` or smoke-run sessions appear among the workers with GLiDE "
                 "logs unless listed in the excluded table above.\n")
    open(os.path.join(out, "exclusions.md"), "w").write("".join(lines))


def write_display_counts(out, rows, episodes):
    n = len(rows)
    q = [r for r in rows if r["action_current"] == "await_input"]
    dq = [r for r in rows if r["displayed_glyph"] == "?"]
    cnt = collections.Counter(r["session"] for r in q)
    top5 = sum(c for _, c in cnt.most_common(5))
    onsets = [e for e in episodes if e["onset_ordinal"] is not None]
    req = lambda lo, hi: sum(1 for r in rows if lo <= prob(r["glide"], "input", "required") < hi)
    lines = ["# Display counts (current rule R2, latch simulated; hold approximated from managed children only)\n",
             table([
                 ("turns kept", n), ("episodes with at least one turn", len(episodes)), ("onsets (episodes with a ? verdict)", len(onsets)),
                 ("episodes that never show ?", len(episodes) - len(onsets)),
                 ("turns with a ? verdict", len(q)), ("turns displaying ? (verdict or latch)", len(dq)),
                 ("  of which latched from an earlier turn", sum(1 for r in dq if r["latched"])),
                 ("turns displaying !", sum(1 for r in rows if r["displayed_glyph"] == "!")),
                 ("demoted turns (cut <= required < .5 and attention.important >= cut)", sum(1 for r in rows if r["demoted"])),
                 ("? verdicts that coincide with managed children running (would be held)", sum(1 for r in q if r["held_by_managed_children"])),
                 ("required in [.05, .158)", req(0.05, CUTS[2])), ("required in [.01, .05)", req(0.01, 0.05)),
                 ("required < .01", req(0, 0.01)),
                 ("share of ? verdicts in the top five sessions", f"{top5}/{len(q)} = {top5/len(q):.0%}" if q else "-"),
             ], ["count", "value"]),
             "\nTop sessions by ? verdicts: " + ", ".join(f"{s[:8]} {c}" for s, c in cnt.most_common(8)) + "\n",
             "\nHold approximation: only managed children with a running turn at completion time are reconstructed; native "
             "sub-agents and imminent-turn holds are not available after the fact, and `!` assumes the person has not yet viewed the session.\n"]
    open(os.path.join(out, "display-counts.md"), "w").write("".join(lines))


PHRASES = ["should I", "want me to", "do you want", "if you want", "let me know", "tell me if", "your call",
           "needs your", "awaiting your", "approve", "confirm", "go ahead"]
PHRASE_RE = re.compile("|".join(re.escape(p) for p in PHRASES), re.I)


def sentences(text):
    parts = re.split(r"(?<=[.!?])\s+|\n+", text)
    return [p.strip() for p in parts if p.strip()]


def write_digest(out, db, chosen, by_turn, uitems, prefixes, seed, sess):
    lines = ["# Onset digest (private)\n", f"Seed: {seed}. Sample: every onset with required in [.158,.5), then random others, at most 3 per session, "
             f"40 total ({len(chosen)} here).\n", "Phrases (case-insensitive) besides sentences ending in `?`: " + ", ".join(PHRASES) + ".\n",
             "Replies searched: those of the onset turn only (deviation: an episode can span hundreds of turns).\n"]
    for e in chosen:
        s, o = e["session"], e["onset_ordinal"]
        t = by_turn[(s, o)]
        persons = [u for u in uitems[s] if u["genuine"]]
        if sess[s].get("agent_created") and persons:
            persons = persons[1:]
        prev = [u for u in persons if u["position"] <= o]
        msg = prev[-1] if prev else None
        start = db.execute("SELECT turn_start_position FROM session_turn_usage WHERE session_id=? AND completed_ordinal=?",
                           (s, o)).fetchone()
        # Deviation from "every reply since the message": an episode can span
        # hundreds of turns, so only the onset turn's own replies are searched.
        lo = max((start[0] - 1) if start and start[0] else 0, msg["position"] if msg else 0)
        replies = agent_items(db, s, lo, o)
        lines.append(f"\n## {s[:8]} ordinal {o}  required={e['onset_input_required']:.3f} attention.important="
                     f"{e['onset_attention_important']:.3f}  cleared by: {e['cleared_by']}  latched-later={e['other_verdicts_under_latch']}\n")
        lines.append("Person's latest message (last 600 chars): " + (msg["text"][-600:].replace("\n", " ⏎ ") if msg else "(none)") + "\n")
        hits = 0
        for i, r in enumerate(replies, 1):
            ss = sentences(r["text"])
            for j, sen in enumerate(ss):
                if sen.endswith("?") or PHRASE_RE.search(sen):
                    hits += 1
                    ctx = " ".join(x for x in (ss[j - 1] if j else "", f"**{sen}**", ss[j + 1] if j + 1 < len(ss) else "") if x)
                    lines.append(f"- reply {i}/{len(replies)}: {ctx}\n")
        if not hits:
            lines.append("- NO MATCHING SENTENCE in any reply since that message.\n")
        final = replies[-1]["text"][-600:].replace("\n", " ⏎ ") if replies else "(no reply text found)"
        lines.append("Final reply (last 600 chars): " + final + "\n")
    open(os.path.join(out, "onsets-digest.md"), "w").write("".join(lines))


if __name__ == "__main__":
    main()
