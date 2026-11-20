"""Deterministic smoke fixture. No repository or model access."""

import hashlib


def answer(commit, query_text):
    provenance = {"engine": "stub", "commit": commit}
    if query_text == "parse-error":
        return {"status": "parse_error", "message": "Stub parser rejected the query.",
                "rows": [], "total": 0, "provenance": provenance}
    if query_text == "engine-error":
        return {"status": "engine_error", "message": "Stub engine could not answer.",
                "rows": [], "total": 0, "provenance": provenance}
    total = {"empty": 0, "open-empty": 0, "paged": 123, "large": 1}.get(query_text, 3)
    frontiers = []
    if query_text in {"open-empty", "paged"}:
        frontiers = [
            {"reason": "Mock dispatch unresolved",
             "site": {"path": "src/dispatch.rs", "start_line": 4, "end_line": 8}},
            {"reason": "Mock depth boundary",
             "site": {"path": "src/depth.rs", "start_line": 12, "end_line": 12}},
        ]
    coverage = {"complete": not frontiers, "open": frontiers}
    snippet = "x" * (96 * 1024) if query_text == "large" else ""
    query_id = hashlib.sha256(query_text.encode("utf-8")).hexdigest()[:12]
    rows = [
        {
            "kind": "mock_flow",
            "sites": [
                {"path": "src/source.rs", "start_line": index + 1, "end_line": index + 2},
                {"path": "src/target.rs", "start_line": index + 11, "end_line": index + 13},
                {"path": "src/other.rs", "start_line": index + 21, "end_line": index + 21},
            ],
            "fields": {
                "snippet": snippet or f"{query_id}:{index}",
                "signature": "fn target()",
                "value": index,
            },
        }
        for index in range(total)
    ]
    return {"status": "ok", "rows": rows, "total": total, "coverage": coverage,
            "provenance": provenance}
