"""Newline-delimited MCP framing shared by the local and SSH endpoints."""

import json
import sys


PROTOCOLS = ("2025-11-25", "2025-06-18", "2025-03-26")


def encode(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=True, allow_nan=False)


def serve(tools, call=None, *, close_after_list=False):
    for line in sys.stdin:
        try:
            request = json.loads(line, parse_constant=invalid_constant)
        except ValueError:
            reply(None, error={"code": -32700, "message": "Parse error"})
            continue
        if not isinstance(request, dict) or request.get("jsonrpc") != "2.0":
            reply(None, error={"code": -32600, "message": "Invalid request"})
            continue
        if "id" not in request:
            continue
        request_id, method = request["id"], request.get("method")
        if request_id is not None and type(request_id) not in {str, int}:
            reply(None, error={"code": -32600, "message": "Invalid request ID"})
            continue
        params = request.get("params", {})
        if method == "initialize" and isinstance(params, dict):
            version = params.get("protocolVersion")
            reply(request_id, {
                "protocolVersion": version if version in PROTOCOLS else PROTOCOLS[0],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "rql_mock", "version": "1"},
            })
        elif method == "tools/list":
            reply(request_id, {"tools": tools})
            if close_after_list:
                return
        elif method == "tools/call" and call is not None:
            reply(request_id, call(params if isinstance(params, dict) else {}))
        elif method == "ping":
            reply(request_id, {})
        else:
            reply(request_id, error={"code": -32601, "message": "Method not found"})


def invalid_constant(token):
    raise ValueError(f"invalid JSON constant: {token}")


def reply(request_id, result=None, *, error=None):
    response = {"jsonrpc": "2.0", "id": request_id}
    if error is None:
        response["result"] = result
    else:
        response["error"] = error
    print(encode(response), flush=True)
