#!/usr/bin/env python3
"""Serve one host-assigned writer over newline-delimited MCP stdio."""

import argparse
from datetime import datetime, timezone
import fcntl
import hashlib
from importlib import import_module
import json
import os
from pathlib import Path
import re
import sys
import time

from protocol import encode, serve


TOOL = {
    "name": "rql_query",
    "description": "Run an RQL query at the assigned snapshot. Use page to read more rows.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "query": {"type": "string"},
            "page": {"type": "integer", "minimum": 1, "default": 1},
        },
        "required": ["query"],
        "additionalProperties": False,
    },
}


def source_site(site):
    path, start, end = site["path"], site["start_line"], site["end_line"]
    if not isinstance(path, str) or type(start) is not int or type(end) is not int:
        raise ValueError("invalid source site")
    return {"path": path, "start_line": start, "end_line": end}


def locations_only(row):
    if not isinstance(row["sites"], list):
        raise ValueError("invalid row sites")
    return {"sites": [source_site(site) for site in row["sites"]]}


def error(status, message, **fields):
    return {"status": status, "message": message, **fields}


class Server:
    def __init__(self, session, assignment, page_rows, answer, log):
        self.session = session
        self.assignment = assignment
        self.page_rows = page_rows
        self.answer = answer
        self.log = log

    def page_answer(self, result, page):
        status = result["status"]
        if status in {"parse_error", "engine_error"}:
            message = result.get("message")
            if not isinstance(message, str):
                raise ValueError("missing engine error message")
            return error(status, message)
        if status != "ok":
            raise ValueError("unrecognized engine status")
        rows, total = result["rows"], result["total"]
        if not isinstance(rows, list) or type(total) is not int or total != len(rows):
            raise ValueError("inconsistent engine rows and total")

        coverage = result["coverage"]
        if type(coverage["complete"]) is not bool or not isinstance(coverage["open"], list):
            raise ValueError("invalid answer coverage")
        locations = self.assignment["arm"] == "R-loc"
        if locations:
            coverage = {
                "complete": coverage["complete"],
                "open": [{"site": source_site(frontier["site"])} for frontier in coverage["open"]],
            }
        pages = (total + self.page_rows - 1) // self.page_rows
        metadata = {"total": total, "page": page, "pages": pages, "coverage": coverage}
        if page > max(1, pages):
            return error("page_out_of_range", "Page is beyond the last page.",
                         **metadata)
        start = (page - 1) * self.page_rows
        window = rows[start:start + self.page_rows]
        if locations:
            window = [locations_only(row) for row in window]
        return {"status": "ok", "rows": window, **metadata}

    def call(self, params):
        started_at = datetime.now(timezone.utc).isoformat()
        started = time.monotonic()
        arguments = params.get("arguments")
        query, page = None, None
        if isinstance(arguments, dict):
            query = arguments.get("query")
            page = arguments.get("page", 1)
        provenance = None
        if (
            params.get("name") != TOOL["name"]
            or not isinstance(query, str)
            or type(page) is not int
            or page < 1
            or set(arguments) - {"query", "page"}
        ):
            payload = error("invalid_arguments", "rql_query takes a string query and a positive integer page.")
        else:
            try:
                result = self.answer(self.assignment["intro"], query)
                payload = self.page_answer(result, page)
                provenance = result.get("provenance")
                encode(provenance)
                encode(payload)
            except Exception:
                # SSH stderr reaches the writer. Keep Python exception details off that stream.
                payload = error("engine_error", "The engine failed to produce a valid answer.")
                provenance = None
        delivered = encode(payload)
        record = {
            "time": started_at,
            "session": self.session,
            "diff": self.assignment["diff"],
            "intro": self.assignment["intro"],
            "arm": self.assignment["arm"],
            "query": query if isinstance(query, str) else None,
            "page": page if type(page) is int else None,
            "status": payload["status"],
            "total": payload.get("total"),
            "coverage": payload.get("coverage"),
            "answer_sha256": hashlib.sha256(delivered.encode("utf-8")).hexdigest(),
            "latency_ms": round((time.monotonic() - started) * 1000, 3),
            "payload": payload,
            "provenance": provenance,
            "delivered": False,
        }
        def log_delivery(delivered):
            record["delivered"] = delivered
            # Reconnects can run in parallel. Keep a large audit record together.
            fcntl.flock(self.log, fcntl.LOCK_EX)
            try:
                self.log.write(encode(record) + "\n")
                self.log.flush()
            finally:
                fcntl.flock(self.log, fcntl.LOCK_UN)
        return {
            "content": [{"type": "text", "text": delivered}],
            "isError": payload["status"] != "ok",
            "_after_write": log_delivery,
        }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("config", type=Path)
    parser.add_argument("session")
    args = parser.parse_args()
    try:
        config = json.loads(args.config.read_text(encoding="utf-8"))
        if not re.fullmatch(r"[0-9a-f]{32}", args.session) or args.session not in config["sessions"]:
            sys.exit("serve-rql: unknown session")
        assignment = config["sessions"][args.session]
        if assignment["arm"] == "none":
            serve([])
            return
        manifest = json.loads(Path(config["manifest"]).read_text(encoding="utf-8"))
        page_rows = manifest["page_rows"]
        if type(page_rows) is not int or page_rows < 1:
            raise ValueError("invalid page_rows")
        intros = {diff["key"]: diff["intro"] for diff in manifest["diffs"]}
        if (
            assignment["arm"] not in {"R", "R-loc"}
            or not re.fullmatch(r"[0-9a-f]{40}", assignment["intro"])
            or intros.get(assignment["diff"]) != assignment["intro"]
            or not Path(assignment["log"]).is_absolute()
        ):
            raise ValueError("invalid session assignment")
        # The forced command executes this file by path, so add the package
        # parent for engine modules that use package-relative imports.
        sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
        module, _, function = config["engine"].partition(":")
        answer = getattr(import_module(module), function)
        if not callable(answer):
            raise ValueError("engine entry point is not callable")
    except (OSError, ValueError, KeyError, TypeError, ImportError, AttributeError):
        sys.exit("serve-rql: invalid host transport configuration")
    os.umask(0o077)
    try:
        with open(assignment["log"], "a", encoding="utf-8") as log:
            server = Server(args.session, assignment, page_rows, answer, log)
            serve([TOOL], server.call)
    except OSError:
        sys.exit("serve-rql: transport I/O failed")


if __name__ == "__main__":
    main()
