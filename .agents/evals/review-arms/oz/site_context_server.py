#!/usr/bin/env python3
"""Wizard-of-Oz `site_context` MCP server (stdio, newline-delimited JSON-RPC).

Stand-in for a Bifrost query tool: answers from one pre-built fixture
(build_fixtures.py), never from a live analysis. Deleted when the real toolset
exists behind the same interface.

usage: site_context_server.py FIXTURE_OR_DIR INTRO_SHA SESSION_ID [--log-dir DIR]
  FIXTURE_OR_DIR  a fixture file, or a directory holding <INTRO_SHA>.json
  log dir         --log-dir, else $SITE_CONTEXT_LOG_DIR, else ./logs

Answers never carry fact ids, diff slugs, kinds or decisive markers; those stay
in the fixture and the log only.
"""
import datetime
import json
import os
import re
import sys

TOOL = {
    "name": "site_context",
    "description": (
        "Facts a static analyzer recorded for one site of this commit, and whether the "
        "answer is complete. The site is a qualified function name (a.b.C.f or a::b::C::f; "
        "a suffix ending at a segment boundary also matches) or `path:line`, "
        "with a repo-relative path."
    ),
    "inputSchema": {
        "type": "object",
        "properties": {"site": {"type": "string",
                                "description": "Qualified function name, or path:line."}},
        "required": ["site"],
        "additionalProperties": False,
    },
}

LINE_QUERY = re.compile(r"^(?P<path>[^\s:]+(?::[^\s:]+)*?):(?P<line>\d+)(?::\d+)?$")


class Log:
    def __init__(self, directory, intro, session):
        os.makedirs(directory, exist_ok=True)
        stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S")
        self.pid = os.getpid()
        self.path = os.path.join(directory, f"{stamp}-{session}-{self.pid}.jsonl")
        self.intro, self.session = intro, session
        self.conn = os.environ.get("SSH_CONNECTION", "")

    def write(self, method, **fields):
        record = {
            "time": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "session": self.session, "intro": self.intro,
            "ssh_connection": self.conn, "pid": self.pid, "method": method,
        }
        record.update(fields)
        with open(self.path, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(record) + "\n")


def segments(name):
    name = re.sub(r"<[^<>]*>", "", name.strip().strip("`"))
    parts = [p for p in re.split(r"::|\.", name) if p]
    while parts and parts[0] in ("crate", "self", "super"):
        parts.pop(0)
    return parts


class Fixture:
    def __init__(self, path):
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
        self.facts = data["facts"]
        self.functions = data["functions"]
        self.cited = {(c["path"], c["line"]) for c in data["cited_call_sites"]}

    def matching_functions(self, query):
        want = segments(query)
        if not want:
            return []
        found = []
        for fn in self.functions:
            for name in [fn["fqn"]] + fn.get("aliases", []):
                have = segments(name)
                if len(have) >= len(want) and have[-len(want):] == want:
                    found.append(fn)
                    break
        return found

    def facts_in(self, fn):
        return [f for f in self.facts
                if f["anchor_path"] == fn["path"] and fn["start_line"] <= f["anchor_line"] <= fn["end_line"]]

    def facts_at(self, path, line):
        return [f for f in self.facts if f["anchor_path"] == path and f["anchor_line"] == line]

    def line_is_complete(self, path, line):
        if (path, line) in self.cited:
            return True
        return any(fn["in_scope"] and fn["path"] == path and fn["start_line"] <= line <= fn["end_line"]
                   for fn in self.functions)


def render_group(header, facts, complete):
    out = [header]
    for fact in facts:
        out.append("  " + fact["line"])
        out.append("    " + fact["fact"])
    if complete:
        out.append("complete" if facts else "complete: no facts recorded")
    else:
        out.append("incomplete: not computed")
    return "\n".join(out)


def answer(fixture, site):
    """Returns (text, resolved sites, fact ids, completeness list)."""
    site = site.strip()
    line_query = LINE_QUERY.match(site)
    if line_query and "::" not in line_query.group("path"):
        path = line_query.group("path")
        path = path[2:] if path.startswith("./") else path
        line = int(line_query.group("line"))
        facts = fixture.facts_at(path, line)
        complete = fixture.line_is_complete(path, line)
        text = render_group(f"{path}:{line}", facts, complete)
        return text, [f"{path}:{line}"], [f["id"] for f in facts], [complete]
    matches = fixture.matching_functions(site)
    if not matches:
        text = f"No function matching {site!r} is known for this commit.\nincomplete: not computed"
        return text, [], [], [False]
    groups, sites, ids, flags = [], [], [], []
    for fn in matches:
        facts = fixture.facts_in(fn)
        groups.append(render_group(f"{fn['fqn']} ({fn['path']}:{fn['start_line']}-{fn['end_line']})",
                                   facts, fn["in_scope"]))
        sites.append(f"{fn['fqn']} {fn['path']}:{fn['start_line']}-{fn['end_line']}")
        ids.extend(f["id"] for f in facts)
        flags.append(bool(fn["in_scope"]))
    return "\n\n".join(groups), sites, ids, flags


def reply(request_id, result=None, error=None):
    message = {"jsonrpc": "2.0", "id": request_id}
    message["error" if error else "result"] = error or result
    print(json.dumps(message), flush=True)


def main(argv):
    args = [a for a in argv[1:]]
    log_dir = os.environ.get("SITE_CONTEXT_LOG_DIR", "logs")
    if "--log-dir" in args:
        i = args.index("--log-dir")
        log_dir = args[i + 1]
        del args[i:i + 2]
    if len(args) != 3:
        sys.exit(__doc__)
    fixture_arg, intro, session = args
    path = os.path.join(fixture_arg, f"{intro}.json") if os.path.isdir(fixture_arg) else fixture_arg
    log = Log(log_dir, intro, session)
    fixture = Fixture(path)
    for raw in sys.stdin:
        if not raw.strip():
            continue
        try:
            message = json.loads(raw)
        except ValueError as err:
            log.write("error", error=f"unparseable request: {err}")
            continue
        method, request_id = message.get("method"), message.get("id")
        params = message.get("params") or {}
        if request_id is None:
            log.write(f"notification:{method}")
            continue
        if method == "initialize":
            log.write("initialize", protocol=params.get("protocolVersion"),
                      client=params.get("clientInfo"))
            reply(request_id, {
                "protocolVersion": params.get("protocolVersion", "2025-03-26"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "bifrost", "version": "1"},
            })
        elif method == "tools/list":
            log.write("tools/list")
            reply(request_id, {"tools": [TOOL]})
        elif method == "tools/call":
            arguments = params.get("arguments") or {}
            site = arguments.get("site")
            if params.get("name") != TOOL["name"] or not isinstance(site, str) or not site.strip():
                log.write("tools/call", tool=params.get("name"), arguments=arguments,
                          error="bad tool or site argument")
                reply(request_id, {"isError": True, "content": [{
                    "type": "text", "text": "site_context takes one string argument `site`."}]})
                continue
            text, sites, ids, flags = answer(fixture, site)
            log.write("tools/call", tool="site_context", arguments=arguments, resolved_sites=sites,
                      fact_ids=ids, completeness=["complete" if f else "incomplete: not computed" for f in flags])
            reply(request_id, {"content": [{"type": "text", "text": text}]})
        elif method == "ping":
            reply(request_id, {})
        else:
            log.write("error", method=method, error="method not found")
            reply(request_id, error={"code": -32601, "message": f"method not found: {method}"})


if __name__ == "__main__":
    main(sys.argv)
