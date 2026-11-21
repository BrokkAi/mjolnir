"""Run the frozen host-side verifier on the grammar's parsed query."""
import json
import subprocess

from .grammar import parse_query


def verify(binary, root, step, query, answer) -> dict:
    payload = {"root": str(root), "step": step,
               "query": parse_query(query) if isinstance(query, str) else query,
               "answer": answer}
    try:
        completed = subprocess.run(
            [str(binary)], input=json.dumps(payload), capture_output=True,
            encoding="utf-8", check=False,
        )
    except (OSError, UnicodeError) as error:
        raise RuntimeError(f"rql-verify could not run: {error}") from error
    if completed.returncode:
        raise RuntimeError(f"rql-verify exited {completed.returncode}: {completed.stderr.strip()}")
    try:
        result = json.loads(completed.stdout)
    except ValueError as error:
        raise RuntimeError(f"rql-verify returned invalid JSON: {error}") from error
    if (not isinstance(result, dict) or type(result.get("version")) is not int
            or result["version"] != 1 or any(
                not isinstance(result.get(key), list)
                for key in ("candidates", "errors", "unchecked")
            )):
        raise RuntimeError("rql-verify returned an invalid version 1 result")
    return result
