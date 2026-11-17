#!/usr/bin/env python3
"""Merge blind question-locator labels and prepare adjudication artifacts."""

import json
import re
from pathlib import Path


ROOT = Path("/mnt/optane/tmp/glide-afk-locator-20261113/locator")


def read_jsonl(path):
    return [json.loads(line) for line in path.read_text().splitlines() if line]


def latest(answer):
    return max(answer) if isinstance(answer, list) and answer else None


def load_batch(pass_no, batch):
    path = ROOT / "labeling/out" / f"pass{pass_no}-batch-{batch}.jsonl"
    return read_jsonl(path)


def batch_text(pass_no, batch):
    text = (ROOT / f"labeling/pass{pass_no}/batch-{batch}.md").read_text()
    return {
        item_id: body.strip("\n")
        for item_id, body in re.findall(r"^<<<ITEM ([^>]+)>>>\n(.*?)(?=^<<<ITEM |\Z)", text, re.M | re.S)
    }


def main():
    keys = {n: json.loads((ROOT / f"private/key-pass{n}.json").read_text()) for n in (1, 2)}
    labels = {n: {} for n in (1, 2)}
    text_by_id = {n: {} for n in (1, 2)}
    for n in (1, 2):
        for batch in range(1, 5):
            for row in load_batch(n, batch):
                labels[n][row["item"]] = row
            text_by_id[n].update(batch_text(n, batch))

    cases = {x["case_id"]: x for x in read_jsonl(ROOT / "cases.jsonl")}
    fixtures = {x["fixture_id"]: x for x in read_jsonl(ROOT / "fixtures.jsonl")}
    fixture_metrics = {}
    for n in (1, 2):
        rows = [
            (key, labels[n][item_id])
            for item_id, key in keys[n].items()
            if key["kind"] == "fixture"
        ]
        metrics = {}
        for kind in ("newest", "older"):
            selected = [(key, row) for key, row in rows if fixtures[key["ref"]]["answer_type"] == kind]
            correct = sum(
                latest(row["answer"]) in fixtures[key["ref"]]["acceptable_indices"]
                for key, row in selected
                if latest(row["answer"]) is not None
            )
            metrics[kind] = (correct, len(selected))
        all_rows = [row for _, row in rows]
        newest_only = sum(
            isinstance(row["answer"], list)
            and row["answer"] == [fixtures[key["ref"]]["reply_count"] - 1]
            for key, row in rows
        )
        fixture_metrics[n] = (metrics, newest_only, len(all_rows))

    logged = {}
    for n in (1, 2):
        for item_id, key in keys[n].items():
            if key["kind"] == "case":
                logged[key["ref"]] = logged.get(key["ref"], {})
                logged[key["ref"]][n] = (item_id, labels[n][item_id])

    split = json.loads((ROOT / "split.json").read_text())
    counts = {"all": [0, 0], "tuning": [0, 0], "held_out": [0, 0]}
    disagreements, cannot_tell, cannot_tell_cases, queued_rules = [], 0, 0, []
    agreed = []
    newest_logged = {n: [0, 0] for n in (1, 2)}
    sensitive = re.compile(
        r"parent|coordinator|offer|if you want|unless you want|your call|say if|"
        r"would you like|want me to|if you'd like|your preference|up to you",
        re.I,
    )
    for case_id, pair in logged.items():
        if set(pair) != {1, 2}:
            continue
        i1, r1 = pair[1]; i2, r2 = pair[2]
        a1, a2 = r1["answer"], r2["answer"]
        cannot_tell += int(a1 == "cannot_tell") + int(a2 == "cannot_tell")
        cannot_tell_cases += int(a1 == "cannot_tell" or a2 == "cannot_tell")
        for n, row in ((1, r1), (2, r2)):
            newest_logged[n][1] += 1
            case = cases[case_id]
            newest_logged[n][0] += int(isinstance(row["answer"], list) and row["answer"] == [case["state"]["replies"][-1]["reply"]])
        same = (a1 == "none" and a2 == "none") or (isinstance(a1, list) and isinstance(a2, list) and latest(a1) == latest(a2))
        session = cases[case_id]["session"]
        group = split["session_side"].get(session, "unknown")
        for name in ("all", group):
            counts[name][1] += 1
            counts[name][0] += int(same)
        if not same:
            disagreements.append((case_id, i1, i2))
        elif a1 == "none":
            reasons = r1.get("reason", "") + " " + r2.get("reason", "")
            if sensitive.search(reasons):
                queued_rules.append((case_id, i1, i2))
            else:
                agreed.append({"case_id": case_id, "acceptable": "none", "source": "agreed"})
        else:
            acceptable = sorted(set(a1) & set(a2) | {latest(a1)})
            agreed.append({"case_id": case_id, "acceptable": acceptable, "source": "agreed"})

    queue_cases = disagreements + queued_rules
    queue = []
    private_map = {}
    for seq, (case_id, i1, i2) in enumerate(queue_cases, 1):
        opaque = f"q-{seq:04x}-{(seq * 7919 % 65521):04x}"
        item_id = i1 if i1 in text_by_id[1] else i2
        queue.append(f"<<<ITEM {opaque}>>>\n{text_by_id[1].get(item_id, text_by_id[2].get(i2, ''))}")
        private_map[opaque] = {"case_id": case_id, "pass1_item": i1, "pass2_item": i2,
                               "reason": "disagreement" if (case_id, i1, i2) in disagreements else "rule-sensitive agreed none"}

    out = ROOT / "labeling/adjudication"
    out.mkdir(parents=True, exist_ok=True)
    (out / "queue.md").write_text("\n\n".join(queue) + "\n")
    instructions = (ROOT / "labeling/LABELING-INSTRUCTIONS.md").read_text()
    instructions += ("\n\n## Settled rules\n\n"
                     "- In a sub-agent session, a request addressed to the parent agent or a coordinator counts as a request to the user: the user is whoever wrote the latest user message.\n"
                     "- An offer waiting on the user's answer counts even when minor (for example, asking whether the user wants the worktree removed); it stops counting once a later reply settles it.\n"
                     "- List every reply that restates the open request.\n"
                     "- A decision announced for later is not a request now.\n")
    (out / "INSTRUCTIONS.md").write_text(instructions)
    (ROOT / "private/adjudication-queue-map.json").write_text(json.dumps(private_map, indent=2) + "\n")
    labels_dir = ROOT / "labels"; labels_dir.mkdir(exist_ok=True)
    (labels_dir / "final-agreed.jsonl").write_text("".join(json.dumps(x, separators=(",", ":")) + "\n" for x in agreed))

    print("Fixture quality (latest listed reply ∈ acceptable):")
    for n, (metrics, only, total) in fixture_metrics.items():
        fixture_share = only / total
        logged_share = newest_logged[n][0] / newest_logged[n][1]
        print(f"pass{n}: newest {metrics['newest'][0]}/{metrics['newest'][1]}, older {metrics['older'][0]}/{metrics['older'][1]}; newest-only fixtures {only}/{total} ({fixture_share:.1%}), logged {newest_logged[n][0]}/{newest_logged[n][1]} ({logged_share:.1%})")
    print("Agreement:", ", ".join(f"{name} {a}/{b} ({a/b:.1%})" for name, (a,b) in counts.items() if b))
    print(f"logged cases={counts['all'][1]}, cannot_tell labels={cannot_tell}; cases with cannot_tell={cannot_tell_cases} ({cannot_tell_cases/counts['all'][1]:.1%})")
    print(f"queue: {len(queue_cases)} total; disagreement={len(disagreements)}, rule-sensitive agreed none={len(queued_rules)}; agreed output={len(agreed)}")


if __name__ == "__main__":
    main()
