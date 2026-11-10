#!/usr/bin/env python3
"""Evaluate synthetic GitHub item fixtures against Jev.

--contract v2 (the default, used by current daemons) sends compact session
digests and asks only `interested`; --contract v1 sends the released v1
summary-only turns and also asks `created`. By default validate the checked-in
fixtures without network requests. Use --run --output <file> to send them to
Jev with the frozen questions. Results contain the questions, fixtures, and
provider responses.
"""
import argparse
import json
import os
from pathlib import Path
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
CONTRACTS = {
    "v1": (ROOT / "services/jev-proxy/tests/fixtures/github-item-controls.json",
           ROOT / "mj-core/src/github_item/questions.json", ("interested", "created")),
    "v2": (ROOT / "services/jev-proxy/tests/fixtures/github-item-controls-v2.json",
           ROOT / "mj-core/src/github_item/questions_v2.json", ("interested",)),
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--contract", choices=sorted(CONTRACTS), default="v2")
    args = parser.parse_args()
    fixtures_path, questions_path, axes = CONTRACTS[args.contract]
    fixtures = json.loads(fixtures_path.read_text())
    questions = json.loads(questions_path.read_text())
    assert set(questions) == set(axes)
    for case in fixtures:
        assert set(case["expected"]) == set(axes)
        assert all(isinstance(value, bool) for value in case["expected"].values())
        assert len(json.dumps({"model": "jev-latest", "state": case["state"],
                               "questions": questions}).encode()) <= 64 * 1024
    if not args.run:
        print(f"Validated {len(fixtures)} synthetic GitHub item fixtures; no requests sent.")
        return
    if not args.output:
        parser.error("--run requires --output")
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    key = os.environ.get("TYPESAFE_API_KEY", "").strip()
    if not key:
        key = (Path.home() / ".secrets/typesafe_api_key").read_text().strip()
    results = []
    args.output.parent.mkdir(parents=True, exist_ok=True)
    for case in fixtures:
        for repeat in range(args.repeats):
            request = urllib.request.Request(
                "https://api.typesafe.ai/v1/systemone",
                data=json.dumps({"model": "jev-latest", "state": case["state"],
                                 "questions": questions}).encode(),
                headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"},
            )
            with urllib.request.urlopen(request, timeout=30) as response:
                answer = json.loads(response.read(65537))
            scores = {}
            for axis in axes:
                value = answer["answers"][axis]
                assert value["type"] == "noul"
                probability = value["noul"]
                assert not isinstance(probability, bool) and 0 <= probability <= 1
                scores[axis] = probability
            actual = {axis: score >= (.4 if args.contract == "v2" else .5) for axis, score in scores.items()}
            result = {"id": case["id"], "repeat": repeat, "scores": scores,
                      "actual": actual, "passed": actual == case["expected"], "response": answer}
            results.append(result)
            print(json.dumps({k: v for k, v in result.items() if k != "response"}), flush=True)
            args.output.write_text(json.dumps({"questions": questions, "fixtures": fixtures,
                                               "results": results}, indent=2) + "\n")
    failures = sum(not result["passed"] for result in results)
    print(f"{len(results) - failures}/{len(results)} classifications matched the fixtures.")
    if failures:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
