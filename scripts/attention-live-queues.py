#!/usr/bin/env python3
"""Write the blind labeling queues (ExecPlan attention-live-recalibration, Milestone 2).

Run `python3 -I scripts/attention-live-queues.py --out DIR` after
`scripts/attention-live-corpus.py`. Reads DIR/turns.jsonl, DIR/episodes.jsonl and the
frozen raw logs; writes DIR/labeling/{pass1,pass2,labels,private}. The design is
DIR/FREEZE.md; the queues are frozen once written (refuses to overwrite unless --force).
Transcript text stays in DIR and never enters the repository.
"""
import argparse, collections, datetime, importlib.util, json, os, random, shutil, sys

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("corpus", os.path.join(HERE, "attention-live-corpus.py"))
corpus = importlib.util.module_from_spec(spec)
spec.loader.exec_module(corpus)

MAX_ITEMS = 40
MAX_BYTES = 150_000
EARLIER_REPLY_CHARS = 600
LONG_REPLY = 3000
HEAD, TAIL = 1000, 2000
PERSON_FULL, PERSON_OLD = 2000, 400
KEEP_FULL = 10
BLOCKING_POOL = [
    "Before I go further: should I delete the old release branch or keep it?",
    "I need your approval to rotate the staging credentials; may I?",
    "Which of the two database migrations should I ship first, the column rename or the index rebuild? I cannot continue until you pick one.",
    "I am stuck on the signing step: could you export the certificate from your keychain and tell me where you put it?",
    "Do you approve force-pushing the cleaned history to the shared branch? I will not do it without a yes.",
    "I do not have the production API key; please paste it or tell me which vault entry to read.",
    "Please confirm that I may drop the unused `events_v1` table, because the drop cannot be undone.",
    "Can you run `make bootstrap` on the build host and tell me whether it prints an error? Everything after that depends on it.",
    "Should I merge the open pull request now, or wait until you have reviewed the diff?",
    "I need a decision from you: keep the old public endpoint for one more release, or remove it in this one?",
    "Would you grant my account write access to the metrics repository? Without it I cannot push the dashboard change.",
    "I cannot tell which of the two customer accounts is the right one to migrate; which should I use?",
    "Please approve the cost of starting the 64-core instance for the benchmark run before I launch it.",
    "May I overwrite your local `config.toml` with the generated one? Your current edits would be lost.",
    "I need you to decide whether failing tests in the legacy module block the release or may be skipped.",
    "Could you reconnect the VPN on your side? I cannot reach the internal registry until you do.",
    "Do you want me to publish the package to the public registry now? This step cannot be taken back.",
    "Tell me the exact name of the environment to deploy to; I will not guess between staging and production.",
    "I am waiting on you to review the schema change and reply with approval or changes before I touch the data.",
    "Is it acceptable to rewrite the public API signature, which will break your downstream users? I need your explicit yes.",
]


def load_jsonl(path):
    return [json.loads(l) for l in open(path, encoding="utf-8")]


def split_msgs(msgs):
    last_user = max((i for i, m in enumerate(msgs) if m["role"] == "user"), default=-1)
    return last_user


def asks(text):
    return [s for s in corpus.sentences(text) if s.endswith("?") or corpus.PHRASE_RE.search(s)]


def replies_since(ev):
    msgs = ev["authorization"]["messages"]
    lu = split_msgs(msgs)
    return [m["text"] for m in msgs[lu + 1:] if m["role"] == "assistant"]


def render(ev, override=None):
    """The labeler's view. `override` maps reply number (1-based) to replacement text."""
    au = ev["authorization"]
    msgs = au["messages"]
    lu = split_msgs(msgs)
    persons = [i for i, m in enumerate(msgs) if m["role"] == "user"]
    full = set(persons[-KEEP_FULL:])
    n_replies = sum(1 for m in msgs[lu + 1:] if m["role"] == "assistant")
    tail_only = False
    tail = ev.get("assistant_text_tail")
    if n_replies == 0 and tail:
        n_replies, tail_only = 1, True
    out, k, pn = [], 0, 0
    npersons = len(persons)
    for i, m in enumerate(msgs):
        t = m["text"]
        if m["role"] == "user":
            pn += 1
            lim = PERSON_FULL if i in full else PERSON_OLD
            cut = f" [... {len(t) - lim} characters omitted ...]" if len(t) > lim else ""
            label = "Person's latest message" if i == lu else f"Person message {pn} of {npersons}"
            out.append(f"**{label}:**\n\n{t[:lim]}{cut}\n")
        elif i < lu:
            cut = t[-EARLIER_REPLY_CHARS:]
            pre = f"[... {len(t) - len(cut)} earlier characters omitted ...] " if len(t) > len(cut) else ""
            out.append(f"**Agent reply, earlier context (last {EARLIER_REPLY_CHARS} characters only):**\n\n{pre}{cut}\n")
        else:
            k += 1
            out.append(f"**Agent reply {k} of {n_replies}:**\n\n{reply_text(override.get(k, t) if override else t)}\n")
    if tail_only:
        out.append(f"**Agent reply 1 of 1 (only the end of this reply was captured):**\n\n{tail}\n")
    return "\n".join(out)


def reply_text(t):
    if len(t) <= LONG_REPLY:
        return t
    omitted = t[HEAD:-TAIL]
    lst = asks(omitted)
    block = f"{t[:HEAD]}\n\n[... {len(omitted)} characters omitted ...]\n"
    if lst:
        block += "Sentences from the omitted part that ask or request something:\n" + \
                 "\n".join(f"- {s[:400]}" for s in lst) + "\n"
    return block + f"\n[the reply continues]\n\n{t[-TAIL:]}"


def pack(order, size):
    batches, cur, nbytes = [], [], 0
    for it in order:
        if cur and (len(cur) >= MAX_ITEMS or nbytes + size[it] > MAX_BYTES):
            batches.append(cur)
            cur, nbytes = [], 0
        cur.append(it)
        nbytes += size[it]
    if cur:
        batches.append(cur)
    return batches


def positions(batches):
    return {it: (b, p) for b, batch in enumerate(batches) for p, it in enumerate(batch)}


def repair(batches2, pos1, size, rng):
    """Swap items in pass 2 until none has the pass-1 batch number or position."""
    def bad(it, b, p):
        return pos1[it] == (b, p) or pos1[it][0] == b or pos1[it][1] == p
    def nbytes(b):
        return sum(size[x] for x in b)
    for _ in range(200000):
        conflicts = [(b, p) for b, batch in enumerate(batches2) for p, it in enumerate(batch) if bad(it, b, p)]
        if not conflicts:
            return True
        b, p = rng.choice(conflicts)
        c = rng.randrange(len(batches2))
        if c == b or not batches2[c]:
            continue
        q = rng.randrange(len(batches2[c]))
        i, j = batches2[b][p], batches2[c][q]
        if bad(i, c, q) or bad(j, b, p):
            continue
        if nbytes(batches2[b]) - size[i] + size[j] > MAX_BYTES or nbytes(batches2[c]) - size[j] + size[i] > MAX_BYTES:
            continue
        batches2[b][p], batches2[c][q] = j, i
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--seed", type=int, default=20261117)
    ap.add_argument("--force", action="store_true")
    args = ap.parse_args()
    out = args.out
    lab = os.path.join(out, "labeling")
    if os.path.exists(os.path.join(lab, "manifest.json")) and not args.force:
        sys.exit("labeling/manifest.json exists: the queues are frozen. Pass --force only before any label exists.")
    rng = random.Random(args.seed)

    rows = load_jsonl(os.path.join(out, "turns.jsonl"))
    episodes = load_jsonl(os.path.join(out, "episodes.jsonl"))
    by_turn = {(r["session"], r["completed_ordinal"]): r for r in rows}
    # Evidence from the frozen raw logs.
    recs = {}
    for root in [os.path.join(out, "raw-decision-logs")] + \
            [os.path.join(out, "raw-remote-logs", t) for t in sorted(os.listdir(os.path.join(out, "raw-remote-logs")))]:
        recs.update(corpus.load_assessments(root)[0])
    cut = corpus.CUTS[corpus.CURRENT]

    pops = collections.defaultdict(set)  # population -> {(session, ordinal)}
    weight = collections.defaultdict(lambda: 1.0)
    onset_keys = set()
    for e in episodes:
        if e["onset_ordinal"] is not None:
            onset_keys.add((e["session"], e["onset_ordinal"]))
            pops["P1"].add((e["session"], e["onset_ordinal"]))
    for r in rows:
        k = (r["session"], r["completed_ordinal"])
        q = r["action_current"] == "await_input"
        if q and k not in onset_keys:
            pops["P2"].add(k)
        req = corpus.prob(r["glide"], "input", "required")
        if cut and 0.05 <= req < cut:
            pops["P4"].add(k)
        if r["displayed_glyph"] == "!" or r["demoted"]:
            pops["P5"].add(k)
        if not q and r["next_person_message"] and r["next_person_message"]["answer_shaped"]:
            pops["P6"].add(k)
    ep_turns = collections.defaultdict(list)
    for r in rows:
        ep_turns[(r["session"], r["episode"])].append(r)
    has_onset = {(e["session"], e["episode"]) for e in episodes if e["onset_ordinal"] is not None}
    for key, ts in ep_turns.items():
        if key not in has_onset:
            pops["P3"].add((key[0], max(t["completed_ordinal"] for t in ts)))
    band = sorted((r["session"], r["completed_ordinal"]) for r in rows
                  if 0.01 <= corpus.prob(r["glide"], "input", "required") < 0.05)
    band_sample = set(rng.sample(band, min(50, len(band))))
    for k in band_sample:
        pops["P4"].add(k)
        weight[(k, "P4")] = len(band) / len(band_sample)

    # Items that cannot be rendered (no evidence) are reported, not dropped silently.
    evid = {}
    missing = []
    for k in set().union(*pops.values()):
        rec = recs.get(by_turn[k]["record_id"])
        if rec is None or not rec["evidence"] or not rec["evidence"].get("authorization", {}).get("messages"):
            missing.append(k)
        else:
            evid[k] = rec["evidence"]
    for p in pops:
        pops[p] -= set(missing)

    # Seeded items: real turns outside every population.
    used = set().union(*pops.values())
    cands = []
    for r in rows:
        k = (r["session"], r["completed_ordinal"])
        if k in used or k not in by_turn:
            continue
        rec = recs.get(r["record_id"])
        ev = rec and rec["evidence"]
        if not ev or not ev.get("authorization", {}).get("messages"):
            continue
        rp = replies_since(ev)
        if len(rp) >= 6 and not asks(rp[-1]):
            cands.append((k, ev))
    rng.shuffle(cands)
    pool = list(range(len(BLOCKING_POOL)))
    rng.shuffle(pool)
    seeds = []
    for (k, ev), pi in zip(cands[:20], pool):
        seeds.append(dict(turn=k, pool_index=pi, sentence=BLOCKING_POOL[pi], reply=rng.choice([1, 2, 3])))
        evid[k] = ev
        pops["S"].add(k)

    # Items: one per turn, with all its populations.
    keys = sorted(set().union(*pops.values()))
    rng.shuffle(keys)
    ids, used_ids = {}, set()
    for k in keys:
        while True:
            i = "i" + "%08x" % rng.getrandbits(32)
            if i not in used_ids:
                used_ids.add(i)
                ids[k] = i
                break
    seed_by = {tuple(s["turn"]): s for s in seeds}
    items, size = {}, {}
    for k in keys:
        s = seed_by.get(k)
        override = None
        if s:
            rp = replies_since(evid[k])
            override = {s["reply"]: rp[s["reply"] - 1].rstrip() + " " + s["sentence"]}
        body = f"## ITEM {ids[k]}\n\n{render(evid[k], override)}\n\n---\n\n"
        items[ids[k]] = body
        size[ids[k]] = len(body.encode("utf-8"))
    two_pass = {ids[k] for k in keys if not (pops_of(pops, k) == {"P2"})}

    order1 = list(items)
    rng.shuffle(order1)
    batches1 = pack(order1, size)
    pos1 = positions(batches1)
    order2 = [i for i in items if i in two_pass]
    rng.shuffle(order2)
    batches2 = pack(order2, size)
    ok = repair(batches2, pos1, size, rng)
    if not ok:
        sys.exit("could not separate pass-2 batch and position from pass 1")
    pos2 = positions(batches2)

    if os.path.exists(lab):
        for d in ("pass1", "pass2", "private"):
            shutil.rmtree(os.path.join(lab, d), ignore_errors=True)
    for d in ("pass1", "pass2", "labels/pass1", "labels/pass2", "private"):
        os.makedirs(os.path.join(lab, d), exist_ok=True)
    header = "# Labeling batch\n\nRead labeling/LABELING-INSTRUCTIONS.md first. Write one JSON line per item to your label file.\n\n"
    for pas, batches in (("pass1", batches1), ("pass2", batches2)):
        for n, b in enumerate(batches, 1):
            with open(os.path.join(lab, pas, f"batch-{n:02d}.md"), "w", encoding="utf-8") as f:
                f.write(header)
                for it in b:
                    f.write(items[it])
    inv = {v: k for k, v in ids.items()}
    key = {}
    for k in keys:
        i = ids[k]
        r = by_turn[k]
        key[i] = dict(session=k[0], turn_id=r["turn_id"], completed_ordinal=k[1], target=r["target"], episode=r["episode"],
                      populations=sorted(pops_of(pops, k)),
                      weights={p: weight[(k, p)] for p in pops_of(pops, k) if (k, p) in weight},
                      passes=2 if i in two_pass else 1, pass1=list(pos1[i]), pass2=list(pos2[i]) if i in pos2 else None,
                      seeded=k in seed_by, bytes=size[i])
    json.dump(key, open(os.path.join(lab, "private", "key.json"), "w"), indent=1)
    json.dump([dict(item=ids[tuple(s["turn"])], session=s["turn"][0], completed_ordinal=s["turn"][1], reply=s["reply"],
                    pool_index=s["pool_index"], sentence=s["sentence"]) for s in seeds],
              open(os.path.join(lab, "private", "seeds.json"), "w"), indent=1)
    sizes = lambda bs: [sum(size[x] for x in b) for b in bs]
    big = max(size, key=size.get)
    json.dump(dict(
        frozen_at=datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        queue_seed=args.seed, wording_hash=corpus.sha(json.load(open(os.path.join(corpus.REPO, "mj-core/src/activity/eot_glide_questions.json")))),
        population_sizes={p: len(v) for p, v in sorted(pops.items())}, items=len(items), two_pass_items=len(two_pass),
        unrenderable_missing_evidence=len(missing),
        batches=dict(pass1=len(batches1), pass2=len(batches2)),
        batch_items=dict(pass1=[len(b) for b in batches1], pass2=[len(b) for b in batches2]),
        batch_bytes=dict(pass1=sizes(batches1), pass2=sizes(batches2)),
        largest_item_bytes=size[big], limits=dict(max_items=MAX_ITEMS, max_bytes=MAX_BYTES)),
        open(os.path.join(lab, "manifest.json"), "w"), indent=1)
    print(json.dumps(json.load(open(os.path.join(lab, "manifest.json"))), indent=1)[:2500])


def pops_of(pops, k):
    return {p for p, v in pops.items() if k in v}


if __name__ == "__main__":
    main()
