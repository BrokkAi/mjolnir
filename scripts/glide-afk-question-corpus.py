#!/usr/bin/env python3
"""AFK repeated-question evaluation corpus (plan afk-and-locator-on-glide, Milestone 3).

Run with `python3 -I scripts/glide-afk-question-corpus.py <command> --out DIR`.
Stdlib only. Everything written under --out is private (transcript text).

Commands
  extract   read the daemon store read-only (sqlite3 CLI, plain SELECT) and write
            raw/events.tsv, forms.jsonl and real_pairs.jsonl
  build     read constructed/*.py sources, apply the production transforms,
            write cases.jsonl, labels/ and queues
  selftest  check the Python mirror of question_evidence and the privacy
            transforms against expectations taken from the Rust tests
  jev       run the current Jev question once over every case (TypeSafe direct)
  report    summarise Jev results
"""
import argparse, collections, copy, hashlib, json, os, re, subprocess, sys, time, urllib.request, urllib.error

STORE = os.path.expanduser("~/.local/share/mjolnir/mj.sqlite3")
SECRET_RUN_CHARS = 24
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AFK_QUESTION = os.path.join(REPO, "services/jev-proxy/src/afk_question.json")
TYPESAFE_KEY = os.path.expanduser("~/.secrets/typesafe_api_key")
TYPESAFE_URL = "https://api.typesafe.ai/v1/systemone"

# ---------------------------------------------------------------------------
# Mirror of mj-worker/src/acp/operator_away.rs::question_evidence and the
# transforms in verdict_client.rs::same_afk_question (mj-core/src/redact.rs).
# serde_json has preserve_order on in the worker build (cargo tree shows it),
# so dict insertion order stands for the Rust Value order.
# ---------------------------------------------------------------------------

def field_is_secret(form, field):
    if field.get("secret"):
        return True
    parent_id = field.get("custom_answer_for")
    return parent_id is not None and any(p["id"] == parent_id and p.get("secret") for p in form["fields"])


def question_evidence(form):
    ev = copy.deepcopy(form)
    ev.pop("id", None)
    for field, original in zip(ev["fields"], form["fields"]):
        if field_is_secret(form, original):
            field.pop("default", None)
            field["secret"] = True
    return ev


def _is_token_char(ch):
    return (ch.isascii() and ch.isalnum()) or ch in "-_.+"


def _redact_token_runs(text, replacement):
    out, run = [], []

    def flush():
        if len(run) >= SECRET_RUN_CHARS:
            out.append(replacement("".join(run)))
        else:
            out.append("".join(run))
        run.clear()

    for ch in text:
        if _is_token_char(ch):
            run.append(ch)
        else:
            flush()
            out.append(ch)
    flush()
    return "".join(out)


def _anonymous_label(text, labels):
    n = len(labels)
    if text not in labels:
        labels[text] = n
    return "[redacted token %d]" % labels[text]


def _redact_secrets_distinct(text, labels):
    def repl(token):
        if not any(c.isascii() and c.isalnum() for c in token):
            return _anonymous_label(token, labels)
        result, start = [], 0
        for i, ch in enumerate(token):
            if not (ch.isascii() and ch.isalnum()):
                if start < i:
                    result.append(_anonymous_label(token[start:i], labels))
                result.append(ch)
                start = i + 1
        if start < len(token):
            result.append(_anonymous_label(token[start:], labels))
        return "".join(result)
    return _redact_token_runs(text, repl)


def _map_strings(value, fn):
    if isinstance(value, str):
        return fn(value)
    if isinstance(value, list):
        return [_map_strings(v, fn) for v in value]
    if isinstance(value, dict):
        return {k: _map_strings(v, fn) for k, v in value.items()}
    return value


def redact_secrets(text):
    return _redact_token_runs(text, lambda _t: "[redacted]")


def redact_json_strings(value, keep_ids=True):
    def go(v, key=None):
        if isinstance(v, str):
            return v if (keep_ids and key is not None and (key == "id" or key.endswith("_id"))) else redact_secrets(v)
        if isinstance(v, list):
            return [go(x) for x in v]
        if isinstance(v, dict):
            return {k: go(x, k) for k, x in v.items()}
        return v
    return go(value)


def production_state(current, previous):
    """The state object production sends for a (current, previous) pair of
    question_evidence forms: privacy transforms of same_afk_question."""
    state = {"current_question": copy.deepcopy(current), "previous_questions": copy.deepcopy(previous)}
    labels = {}

    def anonymize_secret_choices(form):
        for field in form["fields"]:
            if field.get("secret") is True and "options" in field:
                field["options"] = _map_strings(field["options"], lambda t: _anonymous_label(t, labels))
    anonymize_secret_choices(state["current_question"])
    for form in state["previous_questions"]:
        anonymize_secret_choices(form)
    state = _map_strings(state, lambda t: _redact_secrets_distinct(t, labels))
    # request_body(): a second, ordinary pass that keeps id / *_id values.
    return redact_json_strings(state, keep_ids=True)


def compact(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))

# ---------------------------------------------------------------------------

def write_jsonl(path, rows):
    with open(path, "w") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")


def read_jsonl(path):
    with open(path) as f:
        return [json.loads(l) for l in f if l.strip()]


def cmd_extract(out):
    os.makedirs(os.path.join(out, "raw"), exist_ok=True)
    sql = ("select seq, session_id, recorded_at_ms, json_extract(body,'$.type'), body from api_events "
           "where json_extract(body,'$.type') in ('input_required','input_resolved') order by seq;")
    with open(os.path.join(out, "raw", "events.tsv"), "w") as f:
        subprocess.run(["sqlite3", "-readonly", "-separator", "\t", STORE, sql], stdout=f, check=True)
    rows = []
    for line in open(os.path.join(out, "raw", "events.tsv")):
        seq, sid, ts, typ, body = line.rstrip("\n").split("\t", 4)
        rows.append((int(seq), sid, int(ts), typ, json.loads(body)))
    by = collections.defaultdict(list)
    for r in rows:
        by[r[1]].append(r)
    forms = []
    for sid, evs in by.items():
        open_ = {}
        for seq, _, ts, typ, b in evs:
            d = b["data"]
            if typ == "input_required":
                req = d.get("request")
                if req is None:
                    continue
                f = dict(sid=sid, seq=seq, ts=ts, req=req, action=None, res_seq=None, res_ts=None)
                open_[req["id"]] = f
                forms.append(f)
            else:
                f = open_.pop(d["elicitation_id"], None)
                if f:
                    f["action"], f["res_seq"], f["res_ts"] = d["action"], seq, ts
    for f in forms:
        r = f["req"]
        f["kind"] = ("permission" if r.get("tool_permission") or "requests permission" in r["message"][:80]
                     else ("plan" if "plan" in r["id"] else "question"))
        m = re.match(r"Native agent (\S+?):", r["message"])
        f["agent"] = m.group(1) if m else ""
        f["form_id"] = "f%d" % f["seq"]
        f["evidence"] = question_evidence(r)
    qs = [f for f in forms if f["kind"] == "question"]
    write_jsonl(os.path.join(out, "forms.jsonl"), forms)
    # Global presence-return signal: a person accepting or cancelling any form.
    returns = sorted(seq for seq, sid, ts, typ, b in rows if typ == "input_resolved" and b["data"]["action"] in ("accept", "cancel"))
    import bisect
    groups = collections.defaultdict(list)
    for f in qs:
        groups[(f["sid"], f["agent"])].append(f)
    pairs = []
    for (sid, agent), lst in groups.items():
        lst.sort(key=lambda f: f["seq"])
        for i, b in enumerate(lst):
            earlier = [a for a in lst[:i] if a["action"] == "away" and a["res_seq"] < b["seq"]]
            if not earlier:
                continue
            k = bisect.bisect_left(returns, b["seq"]) - 1
            last_return = returns[k] if k >= 0 else -1
            in_period = [a for a in earlier if a["res_seq"] > last_return]
            pairs.append(dict(
                pair_id="r%d" % b["seq"], session=sid, agent=agent, current_form=b["form_id"],
                previous_forms=[a["form_id"] for a in (in_period or earlier)],
                in_away_period=bool(in_period), current_action=b["action"],
                gap_min=round((b["ts"] - earlier[-1]["res_ts"]) / 60000, 1)))
    write_jsonl(os.path.join(out, "real_pairs.jsonl"), pairs)
    print("forms", len(forms), "question forms", len(qs), "away-declined", sum(f["action"] == "away" for f in qs))
    print("candidate real pairs", len(pairs), "in away period", sum(p["in_away_period"] for p in pairs))


def cmd_selftest():
    # Expectations come from mj-worker/src/acp/operator_away.rs tests.
    def form(msg, fields=None):
        return {"id": "x", "message": msg, "fields": fields or []}
    a, b = "a" * 40, "b" * 40
    cur = question_evidence(form("Merge commit\n%s?" % b))
    prev = [question_evidence(form("Merge commit %s?" % a))]
    s = production_state(cur, prev)
    assert s["current_question"]["message"] == "Merge commit\n[redacted token 0]?", s
    assert s["previous_questions"][0]["message"] == "Merge commit [redacted token 1]?", s
    cur2 = question_evidence(form("Merge commit\t%s." % a))
    prev2 = [question_evidence(form("Merge commit %s?" % a)), question_evidence(form("Merge commit\n%s?" % b))]
    s = production_state(cur2, prev2)
    assert s["current_question"]["message"] == "Merge commit\t[redacted token 0]."
    assert s["previous_questions"][0]["message"] == "Merge commit [redacted token 0]?"
    assert s["current_question"] != s["previous_questions"][1]
    short = "Merge commit\n%s?" % ("c" * 23)
    assert production_state(question_evidence(form(short)), [])["current_question"]["message"] == short
    # punctuation stays outside labels
    s = production_state(question_evidence(form("x " + "ab-cd_" * 5 + "e")), [])
    assert s["current_question"]["message"].startswith("x [redacted token 0]-[redacted token 1]_"), s
    # secret fields: defaults dropped, options anonymised, companion inherits secrecy
    secret = {"id": "x", "message": "Choose", "fields": [
        {"id": "password", "title": "Password", "required": False, "secret": True, "kind": "single_select",
         "options": [{"value": "P!n12", "title": "P!n12"}, {"value": "n3w?Pin", "title": "n3w?Pin"}], "default": "P!n12"},
        {"id": "password__other", "title": "Other", "required": False, "secret": False, "custom_answer_for": "password",
         "kind": "text", "default": "77!Z"}]}
    ev = question_evidence(secret)
    assert "default" not in ev["fields"][0] and "default" not in ev["fields"][1]
    assert ev["fields"][0]["secret"] is True and ev["fields"][1]["secret"] is True and "id" not in ev
    s = production_state(ev, [ev])
    txt = compact(s)
    assert "P!n12" not in txt and "n3w?Pin" not in txt and "77!Z" not in txt
    o = s["current_question"]["fields"][0]["options"]
    assert o == s["previous_questions"][0]["fields"][0]["options"] and o[0]["value"] != o[1]["value"]
    print("selftest ok")



# ---------------------------------------------------------------------------
# Authored cases. Each authored/*.txt file holds blocks:
#   === ID same|different KIND src=FORMID style=claude|codex [op=...] [k=N]
#   M: message            (continuation lines follow until the next marker)
#   Q: title @@ description   single select   (Q* multi, Qr required, T: text, Tr: required text)
#   - option @@ description
# Programmatic ops (no body, or only option lines): reorder, subset keep=0,1,
# addopt field=N, swapopt field=N, multi field=N, require field=N, unrelated other=FORMID.
# The builder expands a block to the exact field layout the harness bridges
# produce, then applies question_evidence and the privacy transforms.
# ---------------------------------------------------------------------------
import random

CODEX_MESSAGE = "Codex has questions for you. Work can continue while you answer."
CLAUDE_MULTI_MESSAGE = "Please answer the following questions."
OTHER_DESC = "Type your own answer, or add a note to the option you chose above (optional)."
OTHER_DESC_MULTI = "Type your own answer to add to your selection above (optional)."
ALONGSIDE = "\u001falongside"


def parse_authored(path):
    blocks, cur, field = [], None, None
    text_lines = open(path).read().split("\n")
    for line in text_lines:
        if line.startswith("=== "):
            parts = line[4:].split()
            cur = dict(id=parts[0], label=parts[1], kind=parts[2], params={}, message=[], fields=[])
            for p in parts[3:]:
                k, _, v = p.partition("=")
                cur["params"][k] = v
            blocks.append(cur)
            field = None
            continue
        if cur is None or not line.strip() or line.startswith("#"):
            continue
        m = re.match(r"^(Q\*|Qr|Q|Tr|T):\s*(.*)$", line)
        if line.startswith("M:"):
            cur["message"].append(line[2:].strip())
            field = None
        elif m:
            title, _, desc = m.group(2).partition(" @@ ")
            kind = {"Q": "single", "Qr": "single", "Q*": "multi", "T": "text", "Tr": "text"}[m.group(1)]
            field = dict(kind=kind, required=m.group(1) in ("Qr", "Tr"), title=title.strip(), desc=desc.strip(), options=[])
            cur["fields"].append(field)
        elif line.startswith("- "):
            title, _, desc = line[2:].partition(" @@ ")
            if field is None:
                cur.setdefault("loose_options", []).append((title.strip(), desc.strip()))
            else:
                field["options"].append((title.strip(), desc.strip()))
        elif cur["message"] and field is None:
            cur["message"].append(line.strip())
    return blocks


def _option(title, desc):
    o = {"value": title, "title": title}
    if desc:
        o["description"] = desc
    return o


def build_form(block, comp_default):
    """Expand an authored block into a question_evidence form (no id)."""
    style = block["params"].get("style", "claude")
    message = " ".join(block["message"]).strip()
    fields = []
    n = len(block["fields"])
    for i, f in enumerate(block["fields"]):
        if style == "codex":
            fid = "question_%02d" % (i + 1)
            d = {"id": fid, "title": f["title"], "required": True, "secret": False}
        else:
            fid = "question_%d" % i
            d = {"id": fid, "title": f["title"]}
            if f["desc"]:
                d["description"] = f["desc"]
            d["required"] = f["required"]
            d["secret"] = False
        if f["kind"] == "text":
            d["kind"] = "text"
            fields.append(d)
            continue
        opts = [_option(t, ds) for t, ds in f["options"]]
        if f["kind"] == "multi":
            d.update(kind="multi_select", options=opts)
        else:
            d.update(kind="single_select", options=opts, default=opts[0]["value"])
        fields.append(d)
        if style == "codex":
            if comp_default:
                fields.append({"id": fid + "_note", "title": "Additional answer or note", "required": False,
                               "secret": False, "custom_answer_for": fid, "kind": "text"})
        else:
            fields.append({"id": fid + "_custom", "title": "Other",
                           "description": OTHER_DESC_MULTI if f["kind"] == "multi" else OTHER_DESC,
                           "required": False, "secret": False, "custom_answer_for": fid,
                           "custom_answer_option": ALONGSIDE, "kind": "text"})
    if style == "codex" and not message:
        message = CODEX_MESSAGE
    if style == "claude" and not message:
        message = CLAUDE_MULTI_MESSAGE
    return {"message": message, "fields": fields}


def _question_fields(form):
    return [(i, f) for i, f in enumerate(form["fields"]) if not f.get("custom_answer_for")]


def op_reorder(form, rng):
    form = copy.deepcopy(form)
    for f in form["fields"]:
        if "options" in f and len(f["options"]) > 1:
            while True:
                shuffled = f["options"][:]
                rng.shuffle(shuffled)
                if shuffled != f["options"] or len(f["options"]) < 2:
                    break
            f["options"] = shuffled
    groups, cur = [], None
    for f in form["fields"]:
        if f.get("custom_answer_for") and cur is not None:
            cur.append(f)
        else:
            cur = [f]
            groups.append(cur)
    if len(groups) > 1:
        groups.reverse()
    form["fields"] = [f for g in groups for f in g]
    return form


def op_subset(form, keep):
    form = copy.deepcopy(form)
    q = _question_fields(form)
    keep_ids = {q[i][1]["id"] for i in keep}
    form["fields"] = [f for f in form["fields"] if f["id"] in keep_ids or f.get("custom_answer_for") in keep_ids]
    if len(keep) == 1 and form["message"] == CLAUDE_MULTI_MESSAGE:
        pass
    return form


def _field_by_q(form, n):
    return _question_fields(form)[n][1]


def op_addopt(form, n, opts):
    form = copy.deepcopy(form)
    f = _field_by_q(form, n)
    f["options"].extend(_option(t, d) for t, d in opts)
    return form


def op_swapopt(form, n, opts):
    form = copy.deepcopy(form)
    f = _field_by_q(form, n)
    f["options"] = [_option(t, d) for t, d in opts]
    if "default" in f:
        f["default"] = f["options"][0]["value"]
    return form


def op_multi(form, n):
    form = copy.deepcopy(form)
    f = _field_by_q(form, n)
    assert f["kind"] == "single_select", f["kind"]
    f["kind"] = "multi_select"
    f.pop("default", None)
    # keep key order stable: kind before options
    f2 = {k: v for k, v in f.items() if k not in ("options",)}
    f2["options"] = f["options"]
    f.clear()
    f.update(f2)
    return form


def op_require(form, n):
    form = copy.deepcopy(form)
    f = _field_by_q(form, n)
    f["required"] = not f["required"]
    return form


def _tokens(form):
    t = form["message"] + " " + " ".join(f["title"] + " " + (f.get("description") or "") for f in form["fields"])
    return set(re.findall(r"[a-z0-9#]{4,}", t.lower()))


def _jaccard(a, b):
    return len(a & b) / max(1, len(a | b))


def stable_seed(*parts):
    return int(hashlib.sha256("|".join(parts).encode()).hexdigest()[:12], 16)


def load_forms(out):
    return {f["form_id"]: f for f in read_jsonl(os.path.join(out, "forms.jsonl"))}


def cmd_build(out):
    forms = load_forms(out)
    questions = {k: f for k, f in forms.items() if f["kind"] == "question"}
    declined = [f for f in questions.values() if f["action"] == "away"]
    pool = sorted(questions.values(), key=lambda f: f["seq"])
    cases, construction = [], []
    authored_dir = os.path.join(out, "authored")
    blocks = []
    for name in sorted(os.listdir(authored_dir)):
        if name.endswith(".txt"):
            blocks += parse_authored(os.path.join(authored_dir, name))
    ids = [b["id"] for b in blocks]
    assert len(ids) == len(set(ids)), "duplicate authored ids: %s" % [i for i in ids if ids.count(i) > 1]
    for b in blocks:
        p = b["params"]
        src = forms[p["src"]]
        rng = random.Random(stable_seed("build", b["id"]))
        src_ev = src["evidence"]
        op = p.get("op")
        comp = rng.random() < 0.55
        if op is None:
            cur = build_form(b, comp)
        elif op == "reorder":
            cur = op_reorder(src_ev, rng)
        elif op == "subset":
            cur = op_subset(src_ev, [int(x) for x in p["keep"].split(",")])
        elif op == "addopt":
            cur = op_addopt(src_ev, int(p.get("field", 0)), b["loose_options"])
        elif op == "swapopt":
            cur = op_swapopt(src_ev, int(p.get("field", 0)), b["loose_options"])
        elif op == "multi":
            cur = op_multi(src_ev, int(p.get("field", 0)))
        elif op == "require":
            cur = op_require(src_ev, int(p.get("field", 0)))
        elif op == "unrelated":
            cur = forms[p["other"]]["evidence"]
        else:
            raise SystemExit("unknown op %s" % op)
        # Earlier forms: the matching form among 1-5 unrelated ones, at varying positions.
        k = int(p.get("k", 1 + rng.randrange(5)))
        banned = _tokens(src_ev) | _tokens(cur)
        taken_sessions = {src["sid"]}
        if op == "unrelated":
            taken_sessions.add(forms[p["other"]]["sid"])
        candidates = [f for f in pool if f["sid"] not in taken_sessions and f["form_id"] not in (p["src"],)]
        rng.shuffle(candidates)
        distractors = []
        for c in candidates:
            if len(distractors) == k:
                break
            toks = _tokens(c["evidence"])
            if _jaccard(toks, _tokens(src_ev)) > 0.2 or _jaccard(toks, _tokens(cur)) > 0.2:
                continue
            if c["sid"] in taken_sessions:
                continue
            taken_sessions.add(c["sid"])
            distractors.append(c)
        assert len(distractors) == k, (b["id"], len(distractors), k)
        position = rng.randrange(k + 1)
        previous = [d["evidence"] for d in distractors]
        previous.insert(position, src_ev)
        state = production_state(cur, previous)
        label = b["label"]
        case_id = "c" + hashlib.sha256(("case|" + b["id"]).encode()).hexdigest()[:8]
        cases.append(dict(case_id=case_id, state=state, source_form=p["src"], session_group=src["sid"],
                          origin="constructed"))
        construction.append(dict(case_id=case_id, authored_id=b["id"], label=label, kind=b["kind"],
                                 op=op or "authored", n_previous=len(previous), match_position=position,
                                 double_label=b["kind"] in ("repeat_plus_new",), source_form=p["src"],
                                 current_text_chars=len(compact(cur))))
    # Real pairs.
    for pr in read_jsonl(os.path.join(out, "real_pairs.jsonl")):
        cur = forms[pr["current_form"]]["evidence"]
        prev = [forms[x]["evidence"] for x in pr["previous_forms"]]
        state = production_state(cur, prev)
        case_id = "c" + hashlib.sha256(("case|" + pr["pair_id"]).encode()).hexdigest()[:8]
        kind = "real" if pr["in_away_period"] else "real_wide"
        cases.append(dict(case_id=case_id, state=state, source_form=pr["current_form"], session_group=pr["session"],
                          origin=kind))
        construction.append(dict(case_id=case_id, authored_id=pr["pair_id"], label=None, kind=kind, op="real",
                                 n_previous=len(prev), match_position=None, double_label=True,
                                 source_form=pr["current_form"], current_action=pr["current_action"],
                                 gap_min=pr["gap_min"], agent=pr["agent"]))
    # Oversized states fail production (64 KiB body limit); keep them but flag.
    for c in cases:
        c["body_bytes"] = len(json.dumps({"model": "jev-latest", "state": c["state"], "questions": json.load(open(AFK_QUESTION))},
                                         ensure_ascii=False, separators=(",", ":")).encode())
    rng = random.Random(20261113)
    rng.shuffle(cases)
    os.makedirs(os.path.join(out, "labels"), exist_ok=True)
    write_jsonl(os.path.join(out, "cases.jsonl"), cases)
    write_jsonl(os.path.join(out, "labels", "construction.jsonl"), sorted(construction, key=lambda r: r["case_id"]))
    by_id = {c["case_id"]: c for c in cases}
    double = [r["case_id"] for r in construction if r["double_label"] and r["kind"] != "real_wide"]
    wide = [r["case_id"] for r in construction if r["kind"] == "real_wide"]
    single = [r["case_id"] for r in construction if not r["double_label"]]
    for name, idl in (("queue_double.jsonl", double), ("queue_real_wide.jsonl", wide), ("queue_single.jsonl", single)):
        rr = random.Random(stable_seed("queue", name))
        idl = idl[:]
        rr.shuffle(idl)
        write_jsonl(os.path.join(out, "labels", name), [dict(case_id=i, state=by_id[i]["state"]) for i in idl])
    cnt = collections.Counter((r["kind"], r["label"]) for r in construction)
    print(json.dumps({"%s/%s" % k: v for k, v in sorted(cnt.items(), key=str)}, indent=1))
    big = [c["case_id"] for c in cases if c["body_bytes"] > 64 * 1024]
    print("cases", len(cases), "over 64 KiB:", len(big))


# ---------------------------------------------------------------------------
def jev_key():
    return open(TYPESAFE_KEY).read().strip()


def jev_call(state, questions, key):
    body = json.dumps({"model": "jev-latest", "state": state, "questions": questions}, ensure_ascii=False,
                      separators=(",", ":")).encode()
    req = urllib.request.Request(TYPESAFE_URL, data=body, method="POST", headers={
        "Content-Type": "application/json", "Authorization": "Bearer " + key})
    last = None
    for attempt in range(4):
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                return json.loads(r.read())
        except urllib.error.HTTPError as e:
            last = "HTTP %d %s" % (e.code, e.read()[:200])
            if e.code not in (429, 500, 502, 503, 504):
                break
            time.sleep(2 ** attempt)
        except Exception as e:  # network errors: retry with backoff, then record
            last = repr(e)
            time.sleep(2 ** attempt)
    return {"error": last}


def parse_jev(value):
    ans = value["answers"]["same_question"] if "answers" in value else value
    probs = ans["probabilities"]
    same, diff = probs["same"], probs["different"]
    return dict(same=same, different=diff, choice=ans.get("choice"), production_same=(diff <= same))


def cmd_jev(out, workers):
    import concurrent.futures
    questions = json.load(open(AFK_QUESTION))
    key = jev_key()
    path = os.path.join(out, "jev_results.jsonl")
    done = {r["case_id"] for r in read_jsonl(path)} if os.path.exists(path) else set()
    todo = [c for c in read_jsonl(os.path.join(out, "cases.jsonl")) if c["case_id"] not in done and c["body_bytes"] <= 64 * 1024]
    print("todo", len(todo))
    with open(path, "a") as f, concurrent.futures.ThreadPoolExecutor(workers) as ex:
        futs = {ex.submit(jev_call, c["state"], questions, key): c for c in todo}
        for fut in concurrent.futures.as_completed(futs):
            c = futs[fut]
            res = fut.result()
            rec = dict(case_id=c["case_id"], at=time.time())
            if "error" in res:
                rec["error"] = res["error"]
            else:
                try:
                    rec.update(parse_jev(res))
                except Exception as e:
                    rec["error"] = "unparsable: %r %s" % (e, json.dumps(res)[:300])
            f.write(json.dumps(rec) + "\n")
            f.flush()


def exact_upper(k, n, conf=0.95):
    """One-sided Clopper-Pearson upper bound by bisection (stdlib)."""
    from math import comb
    if k >= n:
        return 1.0
    lo, hi = k / n, 1.0
    for _ in range(60):
        mid = (lo + hi) / 2
        cdf = sum(comb(n, i) * mid ** i * (1 - mid) ** (n - i) for i in range(k + 1))
        if cdf > 1 - conf:
            lo = mid
        else:
            hi = mid
    return hi


def cmd_report(out):
    cons = {r["case_id"]: r for r in read_jsonl(os.path.join(out, "labels", "construction.jsonl"))}
    res = {r["case_id"]: r for r in read_jsonl(os.path.join(out, "jev_results.jsonl"))}
    rows = collections.defaultdict(lambda: collections.Counter())
    for cid, c in cons.items():
        r = res.get(cid)
        if r is None or "error" in r:
            rows[(c["kind"], c["label"])]["error_or_missing"] += 1
            continue
        rows[(c["kind"], c["label"])]["held_same" if r["production_same"] else "declined_different"] += 1
    for k, v in sorted(rows.items(), key=str):
        print(k, dict(v))
    same = [res[c] for c, r in cons.items() if r["label"] == "same" and c in res and "error" not in res[c]]
    diff = [res[c] for c, r in cons.items() if r["label"] == "different" and c in res and "error" not in res[c]]
    fd = sum(not r["production_same"] for r in same)
    print("constructed same: false-different %d / %d (upper95 %.3f)" % (fd, len(same), exact_upper(fd, len(same)) if same else 0))
    print("constructed different: recall %d / %d" % (sum(not r["production_same"] for r in diff), len(diff)))

if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("command")
    ap.add_argument("--out", default="/mnt/optane/tmp/glide-afk-locator-20261113/afk")
    ap.add_argument("--workers", type=int, default=4)
    args = ap.parse_args()
    if args.command == "extract":
        cmd_extract(args.out)
    elif args.command == "selftest":
        cmd_selftest()
    elif args.command == "build":
        cmd_build(args.out)
    elif args.command == "jev":
        cmd_jev(args.out, args.workers)
    elif args.command == "report":
        cmd_report(args.out)
    else:
        sys.exit("unknown command")
