#!/usr/bin/env python3
"""Agreement, seed hit rate, adjudication queue and scores for the live attention labels.

    python3 -I scripts/attention-live-score.py --out DIR [--partial] [--no-score]

Reads DIR/{turns,episodes}.jsonl, DIR/labeling/{pass1,pass2,labels,private,adjudication}
and the daemon store (read-only SQL). Writes DIR/scoring/{agreement,seeds,scores}.md and
DIR/labeling/adjudication/batch-NN.md (append-only: reruns keep earlier batches and add
new ones). Standard library only. The design is DIR/FREEZE.md (with its amendments);
every choice it leaves open is listed in DIR/scoring/README.md.
"""
import argparse, collections, glob, hashlib, importlib.util, json, math, os, random, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("corpus", os.path.join(HERE, "attention-live-corpus.py"))
corpus = importlib.util.module_from_spec(spec)
spec.loader.exec_module(corpus)

ASKS = ("blocking", "proposal", "redundant")
GRID = [(c, d, i) for c in (corpus.CUTS[2], 0.3, 0.5, 0.7) for d in (0.5, 0.7, 0.9) for i in (corpus.IMPORTANT, 0.3)]
CURRENT_POINT = (corpus.CUTS[2], 0.5, corpus.IMPORTANT)
CLASSES = ("optional_or_default", "requested_answer", "completion_report", "other_none")
AUTOMATED_PREFIXES = ("user:standby-", "user:ci-agent-health-", "user:api-")
TAKEOVER = re.compile(r"^\s*You are (taking over|the new owner|a new )", re.I)
PERSON_PREFIXES = ("user:prompt-", "user:web-prompt-", "user:phone-prompt-", "user:plan-mode-")
MOOT = re.compile(r"(no input (is )?needed|nothing (is )?needed from you|need nothing from you|no action (is )?needed|"
                  r"nothing for you to do|no decision (is )?needed|you do not need to|you don't need to|"
                  r"disregard (my|the) (question|ask)|ignore (my|the) (question|ask)|withdraw|never mind|no longer need)", re.I)
HANDOVER = re.compile(r"(your call|up to you|your (decision|choice|preference)|you (decide|choose|pick)|which (one|option|of|do)|"
                      r"option [a-d1-4]\b|\b[A-D](, | or )[A-D]\b|choose|pick|prefer|decision|either .{1,80} or |"
                      r"do you want|would you like|or should i|or do you|tell me which|let me know which|yours to)", re.I)
LETTERS = "abcdefgh"


def jl(path):
    out = []
    if os.path.exists(path):
        for l in open(path, encoding="utf-8"):
            l = l.strip()
            if l:
                out.append(json.loads(l))
    return out


def glyph(lab):
    """FREEZE mapping from one label to the glyph it should show."""
    if lab.get("unclear"):
        return "unclear"
    t = lab["ask_type"]
    if t in ASKS:
        return "?"
    if t == "optional_or_default" or lab.get("delivers_requested_answer"):
        return "!"
    return "-"


def klass(lab):
    t = lab["ask_type"]
    if t == "optional_or_default":
        return "optional_or_default"
    if t == "none":
        if lab.get("delivers_requested_answer"):
            return "requested_answer"
        return "completion_report" if lab.get("completion_report") else "other_none"
    return t


def kappa(pairs):
    n = len(pairs)
    if not n:
        return float("nan")
    po = sum(a == b for a, b in pairs) / n
    ca, cb = collections.Counter(a for a, _ in pairs), collections.Counter(b for _, b in pairs)
    pe = sum(ca[k] * cb[k] for k in ca) / (n * n)
    return (po - pe) / (1 - pe) if pe < 1 else 1.0


def table(rows, header):
    return corpus.table(rows, header)


def split_items(path):
    """item id -> rendered text, from a batch file."""
    out = {}
    if not os.path.exists(path):
        return out
    txt = open(path, encoding="utf-8").read()
    for part in re.split(r"(?m)^## ITEM ", txt)[1:]:
        iid, _, body = part.partition("\n")
        out[iid.strip()] = "## ITEM " + iid.strip() + "\n" + body.rsplit("\n---", 1)[0].rstrip() + "\n"
    return out


def zero_reply(text):
    """No agent reply after the person's latest message in the rendering."""
    after = text.split("**Person's latest message:**", 1)[-1]
    return "**Agent reply " not in after


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--partial", action="store_true", help="score with whatever labels exist")
    ap.add_argument("--no-score", action="store_true")
    ap.add_argument("--audit-queue", action="store_true", help="write labeling/audit batches (append-only)")
    ap.add_argument("--seed", type=int, default=20261118)
    args = ap.parse_args()
    out = args.out
    lab = os.path.join(out, "labeling")
    sc = os.path.join(out, "scoring")
    os.makedirs(sc, exist_ok=True)

    key = json.load(open(os.path.join(lab, "private", "key.json")))
    seeds = {s["item"]: s for s in json.load(open(os.path.join(lab, "private", "seeds.json")))}
    manifest = json.load(open(os.path.join(lab, "manifest.json")))
    labels = {1: {}, 2: {}}
    labeler = {}
    for p in (1, 2):
        for f in sorted(glob.glob(os.path.join(lab, "labels", f"pass{p}", "batch-*.jsonl"))):
            nn = int(re.search(r"batch-(\d+)", f).group(1))
            for d in jl(f):
                labels[p][d["item"]] = d
                labeler[(p, d["item"])] = f"p{p}-{LETTERS[(nn - 1) % 8]}"
    texts = {}
    for p in (1, 2):
        for f in sorted(glob.glob(os.path.join(lab, f"pass{p}", "batch-*.md"))):
            for k, v in split_items(f).items():
                texts.setdefault(k, v)
    zero = {i for i, t in texts.items() if zero_reply(t)}
    pops_of = {i: set(v["populations"]) for i, v in key.items()}

    write_agreement(sc, key, labels, zero, pops_of)
    write_seeds(sc, key, seeds, labels, labeler, texts)
    queue = build_queue(lab, key, labels, texts, zero, pops_of, args.seed)
    print(f"adjudication queue: {len(queue)} items so far "
          f"({collections.Counter(r for q in queue.values() for r in q['reasons'])})")
    if not args.no_score:
        write_scores(out, sc, key, labels, zero, pops_of, queue, args.partial, args.seed, texts, args.audit_queue)


# ---------------------------------------------------------------- agreement
def write_agreement(sc, key, labels, zero, pops_of):
    both = [i for i in key if i in labels[1] and i in labels[2] and i not in zero]
    lines = ["# Agreement between pass 1 and pass 2\n",
             f"Items labeled twice so far: {len(both)} of {sum(1 for v in key.values() if v['passes'] == 2)} "
             f"(zero-reply items, {len(zero)}, are left out).\n"]

    def block(name, items):
        pairs_t = [(labels[1][i]["ask_type"], labels[2][i]["ask_type"]) for i in items]
        pairs_g = [(glyph(labels[1][i]), glyph(labels[2][i])) for i in items]
        pairs_d = [(bool(labels[1][i]["delivers_requested_answer"]), bool(labels[2][i]["delivers_requested_answer"])) for i in items]
        n = len(items)
        pct = lambda ps: f"{sum(a == b for a, b in ps) / n:.1%}" if n else "-"
        return (name, n, pct(pairs_t), pct(pairs_g), f"{kappa(pairs_g):.2f}", pct(pairs_d), f"{kappa(pairs_d):.2f}")

    rows = [block("all (seeds excluded)", [i for i in both if "S" not in pops_of[i]])]
    for p in ("P1", "P3", "P4", "P5", "P6", "S"):
        rows.append(block(p + (" (not in any rate)" if p == "S" else ""), [i for i in both if p in pops_of[i]]))
    lines.append(table(rows, ["population", "items", "ask_type agree", "glyph agree", "glyph kappa", "delivers agree", "delivers kappa"]))
    lines.append("\nP2 is labeled once and has no agreement figure. An item in several populations counts in each row.\n")
    types = ["blocking", "proposal", "redundant", "optional_or_default", "none"]
    ex_s = [i for i in both if "S" not in pops_of[i]]
    cm = collections.Counter((labels[1][i]["ask_type"], labels[2][i]["ask_type"]) for i in ex_s)
    lines.append("\n## ask_type confusion matrix (rows pass 1, columns pass 2; seeds excluded)\n")
    lines.append(table([(a, *[cm[(a, b)] for b in types]) for a in types], ["pass 1 \\ pass 2", *types]))
    gl = ["?", "!", "-", "unclear"]
    cg = collections.Counter((glyph(labels[1][i]), glyph(labels[2][i])) for i in ex_s)
    lines.append("\n## Glyph confusion matrix (`-` = no alert)\n")
    lines.append(table([(a, *[cg[(a, b)] for b in gl]) for a in gl], ["pass 1 \\ pass 2", *gl]))
    lines.append("\n## Zero-reply items (no agent reply after the person's latest message)\n")
    lines.append(table([(p, sum(1 for i in zero if p in pops_of.get(i, ()))) for p in ("P1", "P2", "P3", "P4", "P5", "P6")],
                       ["population", "items"]))
    lines.append("\nThey are scored as no alert whatever the labels say and are never sent to adjudication. "
                 f"Pass-1 labels on them: {dict(collections.Counter(glyph(labels[1][i]) for i in zero if i in labels[1]))}.\n")
    open(os.path.join(sc, "agreement.md"), "w").write("".join(lines))


# -------------------------------------------------------------------- seeds
def write_seeds(sc, key, seeds, labels, labeler, texts):
    rows, per = [], collections.defaultdict(lambda: [0, 0])
    moot = []
    for i, s in sorted(seeds.items()):
        t = texts.get(i, "")
        after = []
        for m in re.finditer(r"\*\*Agent reply (\d+) of \d+:\*\*\n\n(.*?)(?=\n\*\*(?:Agent reply|Person)|\Z)", t, re.S):
            if int(m.group(1)) > s["reply"]:
                after.append(m.group(2))
        hit_moot = next((m.group(0) for a in after for m in [MOOT.search(a)] if m), None)
        if hit_moot:
            moot.append((i, hit_moot))
        for p in (1, 2):
            if i in labels[p]:
                hit = labels[p][i]["ask_type"] in ASKS
                anyask = labels[p][i]["ask_type"] != "none"
                L = labeler[(p, i)]
                per[L][0] += hit
                per[L][1] += 1
                rows.append((i, p, L, s["reply"], hit, anyask, bool(hit_moot)))
    n = len(rows)
    hits = sum(r[4] for r in rows)
    keep = [r for r in rows if not r[6]]
    lines = ["# Seeded asks\n",
             "Hit = the labeler gave the seeded item an ask_type of blocking, proposal or redundant. "
             "(Amendment 1: labelers noticed some seeds as out of place, so this is an upper bound for real early-reply asks.)\n",
             f"Seeded items: {len(seeds)}; labels so far: {n} (pass 1: {sum(1 for r in rows if r[1] == 1)}, pass 2: {sum(1 for r in rows if r[1] == 2)}).\n\n",
             f"Pooled hit rate: {hits}/{n} = {hits / n:.1%}" if n else "No seed labels yet.",
             f"; any non-none ask_type: {sum(r[5] for r in rows)}/{n}.\n" if n else "\n",
             "\n## Per labeler\n",
             table([(L, h, c, f"{h / c:.0%}") for L, (h, c) in sorted(per.items())], ["labeler", "hits", "seeds", "rate"]),
             "\n## Missed seeds\n",
             table([(r[0], r[1], r[2], f"reply {r[3]}") for r in rows if not r[4]], ["item", "pass", "labeler", "seeded in"]),
             "\n## Seeds followed by a real statement that nothing needs the person\n",
             "Found by pattern on the replies after the inserted one (phrases such as \"no input needed\", \"no action needed\", "
             "\"never mind\"); read the matched text before relying on it.\n\n",
             table([(i, m[:120]) for i, m in moot], ["item", "matched text"]) if moot else "None found.\n",
             f"\nHit rate with those seeds left out: {sum(r[4] for r in keep)}/{len(keep)}"
             + (f" = {sum(r[4] for r in keep) / len(keep):.1%}.\n" if keep else ".\n")]
    open(os.path.join(sc, "seeds.md"), "w").write("".join(lines))


# ------------------------------------------------------ adjudication queue
def build_queue(lab, key, labels, texts, zero, pops_of, seed):
    q = {}
    for i, v in key.items():
        if i in zero or "S" in pops_of[i] or i not in labels[1]:
            continue
        a = labels[1][i]
        b = labels[2].get(i) if v["passes"] == 2 else None
        if v["passes"] == 2 and b is None:
            continue
        reasons = []
        if b is not None:
            if glyph(a) != glyph(b):
                reasons.append("disagree: the two labels map to different glyphs")
            elif a.get("unclear") != b.get("unclear"):
                reasons.append("disagree: one label is unclear")
        for lbl in (a, b):
            if lbl and lbl["ask_type"] in ("blocking", "proposal") and HANDOVER.search(lbl.get("ask_quote") or ""):
                reasons.append("amendment 1")
                break
        if reasons:
            q[i] = dict(reasons=reasons)
    adj = os.path.join(lab, "adjudication")
    os.makedirs(os.path.join(adj, "labels"), exist_ok=True)
    keyf = os.path.join(lab, "private", "adjudication-key.json")
    state = json.load(open(keyf)) if os.path.exists(keyf) else {}
    new = [i for i in sorted(q) if i not in state]
    rng = random.Random(seed)
    batch = max((s["batch"] for s in state.values()), default=0)
    cur, nbytes = [], 0
    def flush():
        nonlocal cur, nbytes, batch
        if not cur:
            return
        batch += 1
        with open(os.path.join(adj, f"batch-{batch:02d}.md"), "w", encoding="utf-8") as f:
            f.write("# Adjudication batch\n\nRead labeling/ADJUDICATION-INSTRUCTIONS.md and labeling/LABELING-INSTRUCTIONS.md first. "
                    "Write one JSON line per item to the output file named for this batch.\n\n")
            for i, body in cur:
                f.write(body)
        for i, _ in cur:
            state[i]["batch"] = batch
        cur, nbytes = [], 0
    for i in new:
        flip = int(hashlib.sha256(f"{seed}{i}".encode()).hexdigest(), 16) % 2
        a, b = labels[1][i], labels.get(2, {}).get(i)
        first, second = (a, b) if not flip else (b, a)
        show = lambda L: {k: L.get(k) for k in ("ask_type", "ask_reply", "ask_quote", "delivers_requested_answer",
                                                 "completion_report", "unclear", "note")}
        if b is None:
            body = (texts[i] + "\n### Label (one labeler only)\n\n```json\n" + json.dumps(show(a), ensure_ascii=False, indent=1)
                    + "\n```\n\n**Sent because:** " + "; ".join(q[i]["reasons"]) + "\n\n---\n\n")
        else:
            body = (texts[i] + "\n### Label A\n\n```json\n" + json.dumps(show(first), ensure_ascii=False, indent=1)
                    + "\n```\n\n### Label B\n\n```json\n" + json.dumps(show(second), ensure_ascii=False, indent=1)
                    + "\n```\n\n**Sent because:** " + "; ".join(q[i]["reasons"]) + "\n\n---\n\n")
        state[i] = dict(a_is_pass=(1 if not flip else 2) if b else 1, reasons=q[i]["reasons"], batch=None)
        size = len(body.encode())
        if cur and (len(cur) >= 40 or nbytes + size > 150_000):
            flush()
        cur.append((i, body))
        nbytes += size
    flush()
    json.dump(state, open(keyf, "w"), indent=1)
    return {i: dict(state[i]) for i in state}


# -------------------------------------------------------------------- scoring
def rule_action(g, cut, ceil, imp):
    p = lambda a, n: corpus.prob(g, a, n)
    if p("failure", "quota") >= corpus.FLOOR:
        return "recover_quota"
    req = p("input", "required")
    if req >= cut and not (req < ceil and p("attention", "important") >= imp):
        return "await_input"
    if p("failure", "transient_provider") >= corpus.FLOOR:
        return "retry_provider"
    if p("failure", "other") >= corpus.FLOOR:
        return "await_input"
    return "other"


def final_labels(key, labels, zero, queue, adj, partial):
    """item -> dict(glyph, cls, source, pre_glyph) ; None glyph = pending."""
    out = {}
    for i, v in key.items():
        if "S" in set(v["populations"]) and len(v["populations"]) == 1:
            continue
        if i in zero:
            out[i] = dict(glyph="-", cls="other_none", source="zero-reply")
            continue
        a, b = labels[1].get(i), labels[2].get(i)
        if a is None or (v["passes"] == 2 and b is None):
            out[i] = dict(glyph=None, source="pending label")
            continue
        pre = a if (b is None or glyph(a) == glyph(b)) else None
        if i in adj:
            L = adj[i]
            rec = dict(glyph=glyph(L), cls=klass(L), source="adjudicated", raw=L,
                       pre_glyph=glyph(a) if pre else None, pre_cls=klass(a) if pre else None)
        elif i in queue:
            if pre is not None and partial:
                rec = dict(glyph=glyph(pre), cls=klass(pre), source="agreed (adjudication pending)", raw=pre, pre_glyph=glyph(pre), pre_cls=klass(pre))
            else:
                rec = dict(glyph=None, source="adjudication pending")
        elif pre is not None:
            rec = dict(glyph=glyph(pre), cls=klass(pre), source="agreed" if b else "single", raw=pre)
        else:
            rec = dict(glyph=None, source="disagreement not queued")
        out[i] = rec
    return out


def agent_driven(db, sessions, rows_by_session):
    """Amendment 2 from store metadata: created by another session (title is not the
    default '<project> via <profile>' or the first message arrived through the API) and
    no person-authored prompt in the session."""
    info = {}
    default_title = re.compile(r"^.+ via \S+$")
    for s in sessions:
        r = db.execute("SELECT title,mjolnir_subagents FROM sessions WHERE session_id=?", (s,)).fetchone()
        title = r[0] if r else ""
        ids = [x[0] for x in db.execute("SELECT stable_id FROM materialized_transcript_items WHERE session_id=? AND "
                                        "stable_id>='user:' AND stable_id<'user;' ORDER BY position", (s,))]
        first = re.sub(r"[0-9a-f]{8,}.*", "", ids[0]) if ids else "(no user item)"
        person = sum(1 for x in ids if x.startswith(PERSON_PREFIXES) or re.fullmatch(r"user:[0-9a-f]{8,}", x))
        created = (not default_title.match(title)) or first.startswith("user:api-startup")
        info[s] = dict(title=title, first=first, person=person, items=len(ids), created=created,
                       driven=created and person == 0)
    return info


def build_episodes(out, rows, episodes, key, db):
    ep_rows = collections.defaultdict(list)
    for r in rows:
        ep_rows[(r["session"], r["episode"])].append(r)
    for v in ep_rows.values():
        v.sort(key=lambda r: r["completed_ordinal"])
    # boundary kind
    uitems = {}
    eps = []
    for e in episodes:
        k = (e["session"], e["episode"])
        if k not in ep_rows:
            continue
        auto = False
        if e["start_kind"] == "message":
            if e["session"] not in uitems:
                uitems[e["session"]] = {u["position"]: u for u in corpus.user_items(db, e["session"], corpus.generated_prefixes())}
            u = uitems[e["session"]].get(e["start_ordinal"])
            if u:
                auto = u["stable_id"].startswith(AUTOMATED_PREFIXES) or bool(TAKEOVER.match(u["text"]))
        eps.append(dict(session=e["session"], episode=e["episode"], auto=auto, turns=ep_rows[k]))
    return eps


def evaluate(eps, lab, point):
    """Counters per session for one rule point."""
    cut, ceil, imp = point
    res = collections.defaultdict(collections.Counter)
    for e in eps:
        c = res[e["session"]]
        onset = None
        for t in e["turns"]:
            if rule_action(t["glide"], cut, ceil, imp) == "await_input":
                onset = t["completed_ordinal"]
                break
        # E1: false ? (denominator all episodes whose onset label is known)
        onset_lab = None
        if onset is not None:
            onset_lab = lab.get((e["session"], onset))
        if onset is None or (onset_lab and onset_lab["glyph"] in ("?", "!", "-")):
            c["ep"] += 1
            if onset is not None and onset_lab["glyph"] != "?":
                c["fp"] += 1
                c["fp_" + onset_lab["cls"]] += 1
        else:
            c["ep_unknown"] += 1
        # E2: recall
        qturns = [t["completed_ordinal"] for t in e["turns"]
                  if (lab.get((e["session"], t["completed_ordinal"])) or {}).get("glyph") == "?"]
        if qturns:
            c["pos"] += 1
            if onset is not None and onset <= max(qturns):
                c["pos_hit"] += 1
        # E3
        for t in e["turns"]:
            L = lab.get((e["session"], t["completed_ordinal"]))
            if not L or L["glyph"] not in ("?", "!", "-"):
                continue
            shows_q = onset is not None and t["completed_ordinal"] >= onset
            shows_b = (not shows_q) and corpus.prob(t["glide"], "attention", "important") >= imp and \
                rule_action(t["glide"], cut, ceil, imp) != "await_input"
            if shows_b:
                c["bang_shown"] += 1
                c["bang_correct"] += L["glyph"] == "!"
            if L["glyph"] == "!":
                c["bang_lab"] += 1
                c["bang_lab_shown"] += shows_b
    return res


def rate(counts, num, den):
    n = sum(c[num] for c in counts)
    d = sum(c[den] for c in counts)
    return (n / d if d else float("nan")), n, d


def boot(percounts, num, den, rng, B=2000):
    vals = []
    ss = list(percounts)
    for _ in range(B):
        pick = [percounts[rng.randrange(len(ss))] for _ in ss]
        d = sum(c[den] for c in pick)
        if d:
            vals.append(sum(c[num] for c in pick) / d)
    vals.sort()
    return (vals[int(0.025 * len(vals))], vals[int(0.975 * len(vals)) - 1]) if vals else (float("nan"),) * 2


def fmt(r, ci):
    return "-" if math.isnan(r) else f"{r:.1%} [{ci[0]:.1%}, {ci[1]:.1%}]"


def select(train_totals, base):
    ok = []
    for p, c in train_totals.items():
        fp = c["fp"] / c["ep"] if c["ep"] else 0
        if fp <= 0.03:
            ok.append((-(c["pos_hit"] / c["pos"] if c["pos"] else 0), fp, p))
    if ok:
        return sorted(ok)[0][2]
    return min(train_totals, key=lambda p: (train_totals[p]["fp"] / max(1, train_totals[p]["ep"]), p))


def write_scores(out, sc, key, labels, zero, pops_of, queue, adj_partial, seed, texts, audit_queue):
    adj = {}
    for f in sorted(glob.glob(os.path.join(out, "labeling", "adjudication", "labels", "batch-*.jsonl"))):
        for d in jl(f):
            adj[d["item"]] = d
    pending_q = [i for i in queue if i not in adj]
    if pending_q and not adj_partial:
        sys.exit(f"{len(pending_q)} queued items lack an adjudication label; rerun with --partial for a dry run")
    rows = jl(os.path.join(out, "turns.jsonl"))
    episodes = jl(os.path.join(out, "episodes.jsonl"))
    db = corpus.connect()
    flab = final_labels(key, labels, zero, queue, adj, adj_partial)
    lab = {}
    for i, L in flab.items():
        v = key[i]
        lab[(v["session"], v["completed_ordinal"])] = L
    lab_pre = {}  # without Amendment 1: queue items sent only for amendment 1 keep their pre-adjudication label
    for i, L in flab.items():
        v = key[i]
        use = L
        if i in queue and L.get("pre_glyph") and all(r == "amendment 1" for r in queue[i]["reasons"]):
            use = dict(L, glyph=L["pre_glyph"], cls=L["pre_cls"])
        lab_pre[(v["session"], v["completed_ordinal"])] = use
    by_turn = {(r["session"], r["completed_ordinal"]): r for r in rows}
    onset_keys = {(e["session"], e["onset_ordinal"]) for e in episodes if e["onset_ordinal"] is not None}
    audit_state = {}
    if audit_queue:
        audit_state = build_audit(out, key, flab, by_turn, onset_keys, texts)
    corrections, lab_audit = apply_audit(out, key, flab, lab, audit_state)
    sessions = sorted({r["session"] for r in rows})
    drv = agent_driven(db, sessions, None)
    eps_all = build_episodes(out, rows, episodes, key, db)
    turns_by = collections.Counter(r["session"] for r in rows)
    top5 = {s for s, _ in turns_by.most_common(5)}
    moved = [i for i, L in flab.items() if i in queue and L.get("pre_glyph") and L["glyph"] != L["pre_glyph"] and L["source"] == "adjudicated"]
    status = collections.Counter(L["source"] for L in flab.values())
    undecided = sum(1 for L in flab.values() if L["glyph"] is None)
    lines = ["# Scores" + (" (PARTIAL: dry run on labels present now)" if adj_partial else "") + "\n",
             f"Wording hash {manifest_hash(out)}; labels frozen at {json.load(open(os.path.join(out, 'labeling', 'manifest.json')))['frozen_at']}.\n",
             f"Final-label sources: {dict(status)}. Items without a usable label yet: {undecided} (left out of every rate).\n",
             f"Adjudication queue: {len(queue)} items, {len(adj)} adjudicated, {len(pending_q)} pending. "
             f"Amendment 1 moved {len(moved)} items to a different glyph.\n"]
    lines.append("\n## Agent-driven sessions (Amendment 2), decided from store metadata\n")
    lines.append("A session is agent-driven when another session created it (its title is not `<project> via <profile>`, or its first "
                 "message came through the API as `api-startup-…`) and the transcript has no person-authored prompt "
                 "(`prompt-`, `web-prompt-`, `phone-prompt-`, `plan-mode-`, or a bare id).\n\n")
    lines.append(table([(s[:8], d["title"][:50], d["first"], d["person"], turns_by[s]) for s, d in sorted(drv.items()) if d["driven"]],
                       ["session", "title", "first message id", "person-authored prompts", "turns"]))
    notes = [(s[:8], d["title"][:50], d["first"], d["person"]) for s, d in sorted(drv.items()) if d["created"] and not d["driven"]]
    lines.append("\nCreated by another session but with person-authored prompts (kept in the primary estimates):\n\n")
    lines.append(table(notes, ["session", "title", "first message id", "person-authored prompts"]) if notes else "None.\n")
    lines.append("\nSessions with no user item in the store (kept in primary): " +
                 ", ".join(s[:8] for s, d in sorted(drv.items()) if d["items"] == 0) + "\n")

    n_auto = sum(1 for e in eps_all if e["auto"])
    lines.append(f"\nEpisodes bounded by an automated lane-event notice (`standby-`, `ci-agent-health-`, `api-` ids) or a takeover prompt: "
                 f"{n_auto} of {len(eps_all)}.\n")
    rng = random.Random(seed)
    scopes = [("PRIMARY (agent-driven sessions removed)", lambda e: not drv[e["session"]]["driven"]),
              ("SECONDARY (all sessions)", lambda e: True)]
    views = [("pooled", lambda e: True),
             ("without the five largest sessions", lambda e: e["session"] not in top5),
             ("without automated-boundary episodes", lambda e: not e["auto"])]
    sets = [("with Amendment 1 (before audit)", lab), ("without Amendment 1", lab_pre)]
    if corrections is not None:
        sets.append(("with audit corrections", lab_audit))
    lines.append(f"\nAudit layer: {0 if corrections is None else len(corrections)} corrections from the behavior audit "
                 f"(see `scoring/audit-corrections.md`); tables labeled `with audit corrections` apply them on top of the adjudicated labels.\n")
    for lname, labset in sets:
        for sname, sfilter in scopes:
            if lname == "without Amendment 1" and "SECONDARY" in sname:
                continue
            for vname, vfilter in views:
                eps = [e for e in eps_all if sfilter(e) and vfilter(e)]
                lines.append(f"\n## {sname}: {vname}; labels {lname}\n")
                lines.append(score_block(eps, labset, rng))
    lines.append("\n## Weighted estimate for the unsampled [0.01, 0.05) band\n")
    band = [i for i, v in key.items() if "P4" in v["populations"] and v["weights"].get("P4")]
    if band:
        w = key[band[0]]["weights"]["P4"]
        known = [i for i in band if flab.get(i, {}).get("glyph") in ("?", "!", "-")]
        q = sum(1 for i in known if flab[i]["glyph"] == "?")
        lines.append(f"Sampled {len(known)} labeled of {len(band)} drawn; weight {w:.1f} each. Needs-input labels in the sample: {q}, "
                     f"estimating about {q * w:.0f} needs-input turns among the band's {round(len(band) * w)} turns "
                     f"(binomial 95%: {wilson(q, len(known), w)}). These are not in any episode-level rate.\n")
    open(os.path.join(sc, "scores.md"), "w").write("".join(lines))
    print(open(os.path.join(sc, "scores.md")).read()[:3000])


# ---------------------------------------------------------------- behavior audit
def words(t):
    return re.findall(r"[a-z0-9]+", t.lower())


def replies_to_quote(ask_quote, msg):
    q, m = words(ask_quote or ""), " ".join(words(msg))
    return len(q) >= 6 and any(" ".join(q[i:i + 6]) in m for i in range(len(q) - 5))


def build_audit(out, key, flab, by_turn, onset_keys, texts):
    """Queue items whose final label disagrees with what the person did next (FREEZE.md audit)."""
    adir = os.path.join(out, "labeling", "audit")
    os.makedirs(os.path.join(adir, "labels"), exist_ok=True)
    keyf = os.path.join(out, "labeling", "private", "audit-key.json")
    state = json.load(open(keyf)) if os.path.exists(keyf) else {}
    cand = {}
    for i, v in key.items():
        L = flab.get(i)
        if not L or L["glyph"] not in ("?", "!", "-") or L["source"] == "zero-reply" or "raw" not in L:
            continue
        r = by_turn[(v["session"], v["completed_ordinal"])]
        nm = r["next_person_message"]
        raw = L["raw"]
        quote = raw.get("ask_quote") or ""
        answered = bool(nm) and (nm["answer_shaped"] or replies_to_quote(quote, nm["text"]))
        cases = []
        if r["displayed_glyph"] != "?" and nm and nm["answer_shaped"]:
            cases.append("(a) the rule shows no `?` here, and the person's next message is answer-shaped")
        dis = r["dismissed_own_question"]
        if (v["session"], v["completed_ordinal"]) in onset_keys and raw["ask_type"] in ("blocking", "proposal") and dis \
                and not (nm and nm["created_at_ms"] <= dis["at_ms"]):
            cases.append("(b) a `?` onset labeled blocking or proposal, dismissed without a reply")
        if L["glyph"] in ("!", "-") and answered:
            cases.append("(c) the label maps to no `?`, and the person answered or replied to the quoted ask")
        if cases:
            cand[i] = (cases, r, L)
    new = [i for i in sorted(cand) if i not in state]
    batch = max((st["batch"] for st in state.values()), default=0)
    cur, nbytes = [], 0

    def flush():
        nonlocal cur, nbytes, batch
        if not cur:
            return
        batch += 1
        with open(os.path.join(adir, f"batch-{batch:02d}.md"), "w", encoding="utf-8") as f:
            f.write("# Audit batch\n\nRead labeling/AUDIT-INSTRUCTIONS.md first. Write one JSON line per item to the output file named for this batch.\n\n")
            for _, body in cur:
                f.write(body)
        for i, _ in cur:
            state[i]["batch"] = batch
        cur, nbytes = [], 0
    for i in new:
        cases, r, L = cand[i]
        nm = r["next_person_message"]
        raw = L["raw"]
        show = {k: raw.get(k) for k in ("ask_type", "ask_reply", "ask_quote", "delivers_requested_answer", "completion_report", "unclear", "note")}
        if r["dismissed_own_question"] and not (nm and nm["created_at_ms"] <= r["dismissed_own_question"]["at_ms"]):
            did = "dismissed the `?` notification without replying"
        elif nm and (nm["answer_shaped"] or replies_to_quote(raw.get("ask_quote") or "", nm["text"])):
            did = "answered (the next message is answer-shaped or replies to the quoted ask)"
        elif nm:
            did = "moved on (the next message is not answer-shaped)"
        else:
            did = "no later message in the record"
        delay = f"{nm['delay_s']:.0f} seconds" if nm else "n/a"
        mt = (nm["text"][:1500] + (" [... cut ...]" if len(nm["text"]) > 1500 else "")) if nm else "(none)"
        body = (texts[i] + "\n### Final label (made without knowing what the person did next)\n\n```json\n"
                + json.dumps(show, ensure_ascii=False, indent=1) + "\n```\n\n### What the person did next\n\n"
                + f"Action: {did}\n\nDelay before their next message: {delay}\n\nNext message (up to 1,500 characters):\n\n{mt}\n\n"
                + "**Sent because:** " + "; ".join(cases) + "\n\n---\n\n")
        state[i] = dict(cases=[c[:3] for c in cases], batch=None)
        size = len(body.encode())
        if cur and (len(cur) >= 40 or nbytes + size > 150_000):
            flush()
        cur.append((i, body))
        nbytes += size
    flush()
    json.dump(state, open(keyf, "w"), indent=1)
    return state


def apply_audit(out, key, flab, lab, audit_state):
    """Audit corrections as a separate final layer. Returns (corrections|None, labels with corrections)."""
    keyf = os.path.join(out, "labeling", "private", "audit-key.json")
    if not audit_state and os.path.exists(keyf):
        audit_state = json.load(open(keyf))
    files = sorted(glob.glob(os.path.join(out, "labeling", "audit", "labels", "batch-*.jsonl")))
    if not files:
        return None, lab
    aud = {}
    for f in files:
        for d in jl(f):
            aud[d["item"]] = d
    corrections, lab2 = {}, dict(lab)
    for i, d in aud.items():
        if d.get("keep", True) or i not in flab or "raw" not in flab[i]:
            continue
        before = flab[i]["raw"]
        after = dict(before, ask_type=d.get("ask_type", before["ask_type"]),
                     delivers_requested_answer=d.get("delivers_requested_answer", before["delivers_requested_answer"]))
        v = key[i]
        corrections[i] = dict(before=before, after=after, reason=d.get("reason", ""), cases=audit_state.get(i, {}).get("cases", []),
                              glyph_before=flab[i]["glyph"], glyph_after=glyph(after))
        lab2[(v["session"], v["completed_ordinal"])] = dict(flab[i], glyph=glyph(after), cls=klass(after), source="audit-corrected")
    lines = ["# Audit corrections\n", f"Audited items with a label so far: {len(aud)}; kept: {sum(1 for d in aud.values() if d.get('keep', True))}; "
             f"corrected: {len(corrections)}; glyph changed: {sum(1 for c in corrections.values() if c['glyph_before'] != c['glyph_after'])}.\n\n"]
    lines.append(table([(i, ",".join(c["cases"]), f"{c['before']['ask_type']}/{c['before']['delivers_requested_answer']} ({c['glyph_before']})",
                         f"{c['after']['ask_type']}/{c['after']['delivers_requested_answer']} ({c['glyph_after']})", c["reason"][:160])
                        for i, c in sorted(corrections.items())],
                       ["item", "cases", "label before audit (ask_type/delivers, glyph)", "after audit", "reason"]))
    open(os.path.join(out, "scoring", "audit-corrections.md"), "w").write("".join(lines))
    return corrections, lab2


def wilson(k, n, w):
    if not n:
        return "-"
    p, z = k / n, 1.96
    d = 1 + z * z / n
    c = (p + z * z / (2 * n)) / d
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return f"{max(0, c - h) * n * w:.0f} to {(c + h) * n * w:.0f}"


def manifest_hash(out):
    return json.load(open(os.path.join(out, "labeling", "manifest.json")))["wording_hash"]


def score_block(eps, labset, rng):
    if not eps:
        return "No episodes.\n"
    sess = sorted({e["session"] for e in eps})
    per = {p: evaluate(eps, labset, p) for p in GRID}
    # leave-one-session-out choice among grid points
    chosen = {}
    for s in sess:
        tot = {p: collections.Counter() for p in GRID}
        for p in GRID:
            for s2, c in per[p].items():
                if s2 != s:
                    tot[p].update(c)
        chosen[s] = select(tot, CURRENT_POINT)
    cf = {s: per[chosen[s]].get(s, collections.Counter()) for s in sess}
    rows = []

    def totals(percounts):
        tot = collections.Counter()
        for s_ in sess:
            tot.update(percounts.get(s_, collections.Counter()))
        return tot
    base = totals(per[CURRENT_POINT])

    def row(name, percounts, is_current=False):
        pc = [percounts.get(s, collections.Counter()) for s in sess]
        out = [name]
        point = {}
        for key_, (num, den) in (("fp", ("fp", "ep")), ("recall", ("pos_hit", "pos")), ("bp", ("bang_correct", "bang_shown")),
                                 ("br", ("bang_lab_shown", "bang_lab"))):
            r, n, d = rate(pc, num, den)
            point[key_] = r
            out.append(fmt(r, boot(pc, num, den, rng)) + f" ({n}/{d})")
        tot = totals(percounts)
        out.append(" / ".join(f"{c}={tot['fp_' + c]}" for c in CLASSES))
        # Estimand 4, read strictly: a category with a nonzero count under the current
        # rule must fall; a category at zero must stay at zero.
        failing = [c for c in CLASSES if (tot["fp_" + c] >= base["fp_" + c] if base["fp_" + c] > 0 else tot["fp_" + c] > 0)]
        rates_ok = point["fp"] <= 0.03 and point["recall"] >= 0.90
        if is_current:
            out.append("baseline")
        elif rates_ok and failing:
            out.append("FAILS estimand 4: " + ", ".join(failing))
        elif rates_ok:
            out.append("passes")
        else:
            out.append("misses rates" + (f"; estimand 4 also fails: {', '.join(failing)}" if failing else ""))
        return out
    rows.append(row("current rule", per[CURRENT_POINT], True))
    for p in GRID:
        if p != CURRENT_POINT:
            rows.append(row(f"cut {p[0]:.3f} ceil {p[1]} imp {p[2]:.3f}", per[p]))
    rows.append(row("leave-one-session-out choice", cf))
    cnt = collections.Counter(chosen.values())
    lines = ["Pass rule: false-? at most 3% and recall at least 90% (point estimates), and every label class at `!` or no alert with a nonzero count under the current rule must have a lower count (a class at zero must stay at zero).\n\n", f"{len(eps)} episodes, {len(sess)} sessions. Intervals are 95% session bootstrap. Chosen grid points across held-out sessions: "
             + "; ".join(f"cut {p[0]:.3f}/ceil {p[1]}/imp {p[2]:.3f} ×{n}" for p, n in cnt.most_common(4)) + ".\n\n"]
    lines.append(table(rows, ["rule", "false-? (episodes)", "needs-input recall (episodes)", "! precision (labeled turns)",
                              "! recall (labeled turns)", "false-? onsets by label class", "pass rule"]))
    return "".join(lines)


if __name__ == "__main__":
    main()
