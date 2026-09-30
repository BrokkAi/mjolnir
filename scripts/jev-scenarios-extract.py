#!/usr/bin/env python3
"""Turn one live Mjolnir turn into a Jev scenario fixture.

Two sources, one fixture shape (see `.agents/plans/jev-quiet-and-scenario-suite.md`):

  --decision ID [--session S]   the final record of a worker Jev decision; the
                                evidence Jev actually saw and its recorded answer.
                                `assessment-<session prefix>-<ordinal>` resolves the
                                session; an older activity id may use `*` for the
                                middle part, with --session
  --session S --at "YYYY-MM-DD HH:MM:SS"
                                the agent reply at or just before that local time,
                                reconstructed from the controller transcript, for
                                turns that predate Jev; runtime facts are assumed
                                idle and the fixture says so

Reads worker decision logs and the controller SQLite database read-only. Writes
`mj-core/tests/jev-scenarios/<id>-<slug>.json` with `expected` and `outcome`
left null for the author to fill in. Refuses to write anything that looks like a
credential. Standard library only.
"""
import argparse
import fnmatch
import json
import re
import sqlite3
import sys
from datetime import datetime, timezone
from pathlib import Path
from zoneinfo import ZoneInfo

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "mj-core/tests/jev-scenarios"
WORKERS = Path.home() / ".local/share/mjolnir/workers"
DATABASE = Path.home() / ".local/share/mjolnir/mj.sqlite3"
LOCAL_TZ = ZoneInfo("America/Chicago")

USER_BYTES = 32 * 1024
ASSISTANT_BYTES = 16 * 1024
MESSAGE_HEAD_BYTES = 4 * 1024
MESSAGE_TAIL_BYTES = 2 * 1024
MAX_MESSAGES = 256
USER_TAIL = 1024
ASSISTANT_TAIL = 2048
CONTINUATION_PROMPT = (
    "Continue the unfinished work already requested by the user, following their latest "
    "instructions. This message supplies no new approval or missing information."
)
CREDENTIAL_PATTERNS = [
    re.compile(p)
    for p in (
        r"ghp_[A-Za-z0-9]{20,}",
        r"github_pat_[A-Za-z0-9_]{20,}",
        r"\bsk-[A-Za-z0-9]{20,}",
        r"\bAKIA[A-Z0-9]{16}\b",
        r"Bearer [A-Za-z0-9._-]{20,}",
        r"(?i)(api[_-]?key|password|secret|token)\s*[=:]\s*['\"]?[A-Za-z0-9._-]{12,}",
    )
]


def fail(message):
    print(f"error: {message}", file=sys.stderr)
    sys.exit(1)


def scan_credentials(value, path="$"):
    if isinstance(value, dict):
        for key, item in value.items():
            scan_credentials(item, f"{path}.{key}")
    elif isinstance(value, list):
        for index, item in enumerate(value):
            scan_credentials(item, f"{path}[{index}]")
    elif isinstance(value, str):
        for pattern in CREDENTIAL_PATTERNS:
            if pattern.search(value):
                fail(f"credential-shaped text at {path}: refusing to write")


def slug(text):
    words = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    return "-".join(words.split("-")[:6])


def tail(text, limit):
    data = text.encode()
    if len(data) <= limit:
        return text
    return data[-limit:].decode(errors="ignore")


def connect():
    if not DATABASE.exists():
        fail(f"controller database not found at {DATABASE}")
    return sqlite3.connect(f"file:{DATABASE}?mode=ro", uri=True)


def resolve_session(connection, prefix):
    rows = connection.execute(
        "select session_id, title, harness_kind from sessions where session_id like ?",
        (prefix + "%",),
    ).fetchall()
    if len(rows) != 1:
        fail(f"session prefix {prefix!r} matches {len(rows)} sessions")
    return rows[0]


def item_text(body):
    kind = body.get("kind")
    if kind == "user":
        return "\n".join(
            block.get("text", "") for block in body.get("content", []) if block.get("type") == "text"
        )
    if kind == "agent":
        return "".join(
            chunk.get("content", {}).get("text", "")
            for chunk in body.get("chunks", [])
            if chunk.get("content", {}).get("type") == "text"
        )
    if kind == "system":
        return body.get("text", "")
    return ""


def transcript(connection, session):
    rows = connection.execute(
        "select position, created_at_ms, body_json from materialized_transcript_items "
        "where session_id = ? order by position",
        (session,),
    ).fetchall()
    items = []
    for position, created_at_ms, body_json in rows:
        body = json.loads(body_json)
        items.append(
            {
                "position": position,
                "at_ms": created_at_ms,
                "kind": body.get("kind"),
                "text": item_text(body),
                "title": body.get("call", {}).get("title") if body.get("kind") == "tool" else None,
            }
        )
    return items


def iso(ms):
    return datetime.fromtimestamp(ms / 1000, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


def context_after(items, at_ms, count=6):
    after = [item for item in items if item["at_ms"] > at_ms and item["kind"] in ("user", "agent", "system")]
    out = []
    for item in after[:count]:
        text = item["text"].strip().replace("\n", " ")
        out.append({"at": iso(item["at_ms"]), "kind": item["kind"], "text": text[:200]})
    return out


def decision_record(decision_id, session_prefix):
    """The last record written for a decision id, plus the log it came from."""
    match = re.match(r"assessment-([0-9a-f]+)-(\d+)$", decision_id)
    if match:
        session_prefix = session_prefix or match.group(1)
    if not session_prefix:
        fail("--session is required for decision ids that do not carry the session")
    found = None
    for log in sorted(WORKERS.glob("*/jev-decisions/decisions.*.jsonl")):
        worker = log.parent.parent.name
        if not worker.startswith(session_prefix):
            continue
        with log.open() as handle:
            for line in handle:
                try:
                    record = json.loads(line)
                except json.JSONDecodeError:
                    continue
                identity = record.get("id", "")
                if (
                    identity == decision_id
                    or (match and identity == f"assessment-{worker}-{match.group(2)}")
                    or ("*" in decision_id and fnmatch.fnmatchcase(identity, decision_id))
                ):
                    found = (record, worker)
    if not found:
        fail(f"decision {decision_id!r} not found under {WORKERS}")
    return found


def fixture_from_decision(args, connection):
    record, session = decision_record(args.decision, args.session)
    technical = record.get("technical", {})
    _, title, harness = resolve_session(connection, session)
    if record.get("kind") == "assessment":
        assessment = technical["assessment"]
        evidence = assessment.get("evidence")
        if evidence is None:
            fail("that decision was superseded before its evidence was captured; pick another")
        captured_at = assessment["completed_at_ms"]
        recorded = assessment.get("verdict")
        contract = technical.get("contract")
        recorded_action = assessment.get("action")
        status = assessment.get("status")
        reason = assessment.get("reason")
    elif record.get("kind") == "activity":
        evidence = technical["request"]["state"]
        captured_at = record["started_at_ms"]
        recorded = technical.get("result")
        contract = technical.get("contract")
        recorded_action = technical.get("applied_decision") or technical.get("proposed_decision")
        status = technical.get("outcome")
        reason = technical.get("reason")
    else:
        fail(f"unsupported decision kind {record.get('kind')!r}")
    items = transcript(connection, session)
    return {
        "source": {
            "kind": "worker-decision",
            "session": session,
            "session_title": title,
            "decision": record["id"],
            "contract": contract,
            "captured_at": iso(captured_at),
            "assessed_at": iso(record.get("updated_at_ms", captured_at)),
            "recorded_action": recorded_action,
            "recorded_status": status,
            "recorded_reason": reason,
        },
        "harness": evidence.get("harness", harness),
        "facts_known": True,
        "evidence": evidence,
        "recorded_verdict": recorded,
        "context": {"after": context_after(items, captured_at)},
    }


def parse_local(text):
    naive = datetime.strptime(text, "%Y-%m-%d %H:%M:%S")
    return int(naive.replace(tzinfo=LOCAL_TZ).timestamp() * 1000)


def trim_middle(text):
    """Port of `mj_core::assessment::trim_middle`: head and tail with an omission marker."""
    data = text.encode()
    if len(data) <= MESSAGE_HEAD_BYTES + MESSAGE_TAIL_BYTES:
        return text
    head = data[:MESSAGE_HEAD_BYTES].decode(errors="ignore")
    tail = data[-MESSAGE_TAIL_BYTES:].decode(errors="ignore")
    omitted = len(data) - len(head.encode()) - len(tail.encode())
    return f"{head}\n[... {omitted} bytes omitted from the middle of this message ...]\n{tail}"


def build_authorization(items, end_index):
    """Whole user and assistant messages up to the reply, under production rules.

    Mirrors `mj_core::assessment::ContextHistory` step by step: every message is
    kept by head and tail past 6 KiB; a user message that would push the user
    total past 32 KiB, or a 257th user message, marks the history incomplete;
    assistant entries are evicted oldest first past 16 KiB of assistant text or
    past 256 entries in all, never the final reply. History starts after the
    last `/compact` or `/clear`, the closest available stand-in for a context
    reset.
    """
    start = 0
    for index in range(end_index + 1):
        item = items[index]
        if item["kind"] == "user" and item["text"].strip().split(" ")[0] in ("/compact", "/clear"):
            start = index + 1
    messages = []
    state = {"complete": True, "omitted": False, "final_omitted": False}

    def user_bytes():
        return sum(len(m["text"].encode()) for m in messages if m["role"] == "user")

    def user_count():
        return sum(1 for m in messages if m["role"] == "user")

    def assistant_bytes():
        return sum(len(m["text"].encode()) for m in messages if m["role"] == "assistant")

    def evict_oldest_assistant(protect_last):
        candidates = [i for i, m in enumerate(messages) if m["role"] == "assistant"]
        if protect_last and candidates and candidates[-1] == len(messages) - 1:
            candidates = candidates[:-1]
        if not candidates:
            return False
        index = candidates[0]
        if index + 1 == len(messages):
            state["final_omitted"] = True
        messages.pop(index)
        state["omitted"] = True
        return True

    for index in range(start, end_index + 1):
        item = items[index]
        text = item["text"]
        if not text.strip():
            continue
        if item["kind"] == "user":
            if text.startswith(CONTINUATION_PROMPT) or text.startswith("[handback reminder]"):
                continue
            text = trim_middle(text)
            if user_bytes() + len(text.encode()) > USER_BYTES or user_count() >= MAX_MESSAGES:
                state["complete"] = False
                continue
            while len(messages) >= MAX_MESSAGES and evict_oldest_assistant(False):
                pass
            if len(messages) >= MAX_MESSAGES:
                state["complete"] = False
                continue
            messages.append({"id": f"user:{item['position']}", "role": "user", "text": text})
        elif item["kind"] == "agent":
            state["final_omitted"] = False
            messages.append({"id": f"agent:{item['position']}", "role": "assistant", "text": trim_middle(text)})
            while (assistant_bytes() > ASSISTANT_BYTES or len(messages) > MAX_MESSAGES) and evict_oldest_assistant(True):
                pass
    return {
        "messages": messages,
        "authorization_complete": state["complete"],
        "assistant_history_omitted": state["omitted"],
        "open_assistant_id": None,
        "final_reply_omitted": state["final_omitted"],
    }


def fixture_from_transcript(args, connection):
    session, title, harness = resolve_session(connection, args.session)
    items = transcript(connection, session)
    if args.position is not None:
        candidates = [(i, item) for i, item in enumerate(items) if item["position"] == args.position and item["kind"] == "agent"]
        if not candidates:
            fail(f"no agent reply at position {args.position}")
    else:
        at_ms = parse_local(args.at)
        candidates = [
            (index, item)
            for index, item in enumerate(items)
            if item["kind"] == "agent" and item["text"].strip() and at_ms - 180_000 <= item["at_ms"] <= at_ms + 999
        ]
        if not candidates:
            fail("no agent reply within three minutes before that time")
    index, reply = candidates[-1]
    prompt = next(
        (items[i]["text"] for i in range(index, -1, -1) if items[i]["kind"] == "user" and items[i]["text"].strip()),
        "",
    )
    evidence = {
        "authorization": build_authorization(items, index),
        "harness": harness,
        "phase": "replied",
        "silent_for_s": 0,
        "tools_in_flight": [],
        "transcript_summary": "",
        "background_commands": 0,
        "queued_commands": 0,
        "user_prompt_tail": tail(prompt, USER_TAIL),
        "assistant_text_tail": tail(reply["text"], ASSISTANT_TAIL),
        "completion": {"stop_reason": "EndTurn", "diagnostic": None},
    }
    return {
        "source": {
            "kind": "transcript",
            "session": session,
            "session_title": title,
            "reply_position": reply["position"],
            "captured_at": iso(reply["at_ms"]),
            "note": "Runtime facts were not recorded; the turn is assumed to have ended cleanly with nothing in flight.",
        },
        "harness": harness,
        "facts_known": False,
        "evidence": evidence,
        "recorded_verdict": None,
        "context": {"after": context_after(items, reply["at_ms"])},
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--id", required=True, help="fixture id such as S01 or P04")
    parser.add_argument("--title", required=True)
    parser.add_argument("--category", required=True)
    parser.add_argument("--decision", help="worker decision id (assessment-<session>-<ordinal> or an activity id)")
    parser.add_argument("--session", help="session id or unique prefix")
    parser.add_argument("--at", help="local time of the agent reply, YYYY-MM-DD HH:MM:SS (America/Chicago)")
    parser.add_argument("--position", type=int, help="transcript position of the agent reply (instead of --at)")
    parser.add_argument("--out", type=Path, default=FIXTURES)
    parser.add_argument("--force", action="store_true", help="overwrite an existing fixture for this id")
    args = parser.parse_args()
    if sum(1 for flag in (args.decision, args.at, args.position) if flag) != 1:
        fail("give exactly one of --decision, --at, or --position")
    if (args.at or args.position) and not args.session:
        fail("--at and --position need --session")
    connection = connect()
    body = fixture_from_decision(args, connection) if args.decision else fixture_from_transcript(args, connection)
    fixture = {
        "id": args.id,
        "title": args.title,
        "category": args.category,
        "harness": body["harness"],
        "source": body["source"],
        "facts_known": body["facts_known"],
        "facts": {
            "execution": "running" if body["evidence"].get("phase") == "running" else "idle",
            "harness_turn_open": False,
            "goal_active": False,
            "active_user_shells": 0,
            "active_agent_terminals": 0,
            "task_settled_s_ago": None,
            "background_needed": None,
        },
        "evidence": body["evidence"],
        "recorded_verdict": body["recorded_verdict"],
        "expected": None,
        "known_failure": [],
        "outcome": None,
        "context": body["context"],
    }
    scan_credentials(fixture)
    args.out.mkdir(parents=True, exist_ok=True)
    existing = list(args.out.glob(f"{args.id}-*.json"))
    if existing and not args.force:
        fail(f"{existing[0]} exists; pass --force to overwrite")
    for old in existing:
        old.unlink()
    path = args.out / f"{args.id}-{slug(args.title)}.json"
    path.write_text(json.dumps(fixture, indent=2, ensure_ascii=False) + "\n")
    print(path)


if __name__ == "__main__":
    main()
