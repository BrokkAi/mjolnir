"""Validate the retained and expanded Sol 6.2 reviewer cohort."""
import argparse
import collections
import hashlib
import json
import pathlib

EFFORTS = {**dict.fromkeys("PQS", "low"), **dict.fromkeys("ORT", "medium")}
DIGEST = "sha256:1f5469c1c7264c025668e465a852199bc165938f6e84e925303f4fb46ca5e0db"
CODEX = "brokkai-codex-acp-1.15.1_codex-0.170.0_pkg2"


def load(path):
    return json.loads(path.read_text())


def audit(root, canaries):
    campaign = root / "sol-62-repeats"
    original = load(root / "arm-O-jobs.json")
    ids = {j["bug"] for j in original}
    assert len(ids) == len(original) == 70
    assert [dict(j, arm="P") for j in original] == load(root / "arm-P-jobs.json")
    slots = [(j["bug"], j["arm"]) for j in load(campaign / ("canaries.json" if canaries else "jobs.json"))]
    if canaries:
        assert len(slots) == 2 and {a for _, a in slots} == {"Q", "R"}
    else:
        assert len(slots) == 280 and set(slots) == {(b, a) for b in ids for a in "QRST"}
    slots += [(b, a) for b in sorted({b for b, _ in slots}) for a in "OP"]
    image = load(root / "arm-O-image.json")["image"]
    assert image == load(campaign / "image.json")["image"]
    coverage = collections.Counter()
    rows, events, prompts = [], [], []
    frozen = [campaign / f"{name}.json" for name in ("jobs", "canaries", "image", "prompt-Q", "prompt-R")]
    for bug, arm in slots:
        d = root / "runs" / bug / arm
        anchor = d.parent / "O"
        done, old = load(d / "done.json"), load(anchor / "done.json")
        assert (done["bug"], done["arm"]) == (bug, arm), d
        assert (done["intro"], done["base"]) == (old["intro"], old["base"]), d
        assert not done["inventory_problems"], d
        assert load(d / "placement.json")["image"] == image, d
        before, reference = (load(path / "pre-inventory.json") for path in (d, anchor))
        assert all(before[k] == reference[k] for k in ("head", "head_tree", "base_tree", "diff_sha256")), d
        assert not any(before[k] for k in ("remotes", "alternates", "status", "ignored")), d
        assert all(ref.startswith("refs/mj/") for ref in before["refs"]), d
        context = (d / "historical-context.txt").read_text()
        assert context == (anchor / "historical-context.txt").read_text(), d
        reviewer, = done["reviewers"]
        effort = EFFORTS[arm]
        assert (reviewer["model"], reviewer["effort"]) == ("gpt-6.2-sol", effort), d
        assert reviewer["handback"] and reviewer["has_output"], d
        child = reviewer["child"]
        cd = d / "reviewers" / child[:12]
        new = arm in "QRST"
        if new:
            frozen.extend(d / name for name in (
                "done.json", "placement.json", "pre-inventory.json", "post-inventory.json",
                "historical-context.txt", "review-start.json", "review-status.jsonl"))
            frozen.extend(cd / name for name in (
                "output.md", "stored.json", "runtime.json", "usage.json", "prompt-time.json"))
        stored = load(cd / "stored.json")
        selection = stored["selection"]
        assert selection["model"] == "gpt-6.2-sol" and selection["effort"] in (None, effort), d
        if selection["effort"] is None:
            states = [json.loads(line)["status"].get("review") or {}
                      for line in (d / "review-status.jsonl").read_text().splitlines()]
            assert any(f"gpt-6.2-sol / {effort} ·" in state.get("status", "")
                       and any(r.get("child_session_id") == child for r in state.get("roles", []))
                       for state in states), d
        output = (cd / "output.md").read_text().strip()
        assert stored["handback_message"].strip() == output and stored["handback_command_id"], d
        coverage[f"{arm}_reviews"] += 1
        coverage[f"{arm}_empty"] += output == "No findings."
        runtime = load(cd / "runtime.json")
        if runtime.get("image"):
            assert runtime["image"] == image and runtime["image_digest"] == DIGEST, d
            assert runtime["mj_cli_version_at_observation"] == "mj 2.74.0", d
            assert any(CODEX in p and "/codex app-server" in p for p in runtime["processes"]), d
            assert any(CODEX in p and "/codex-acp/dist/index.js" in p for p in runtime["processes"]), d
            coverage[f"{arm}_runtime"] += 1
        elif canaries or new:
            raise ValueError(f"{d}: runtime was not observed")
        usage = load(cd / "usage.json")
        turns = usage.get("turns") or []
        if turns:
            turn, = turns
            assert turn["command_id"] == stored["handback_command_id"], d
            assert turn["outcome"]["kind"] == "completed", d
            assert usage["turn_selections"][turn["command_id"]] == {
                "model": "gpt-6.2-sol", "effort": selection["effort"]}, d
            coverage[f"{arm}_matched_turn"] += 1
        clock = cd / "prompt-time.json"
        elapsed = None
        if turns and clock.exists():
            prompt = load(clock)["prompt_created_at_ms"]
            end = turn["completed_at_ms"]
            assert end > prompt, d
            elapsed = (end - prompt) / 1000
            coverage[f"{arm}_clock"] += 1
            if arm in "QRST":
                events.extend(((prompt, 1), (end, -1)))
        elif canaries or new:
            raise ValueError(f"{d}: no matched prompt-to-completion clock")
        if canaries and arm in "QR":
            user, = load(campaign / f"prompt-{arm}.json")["items"]
            actual = "".join(b["text"] for b in user["body"]["content"] if b["type"] == "text")
            assert context.strip() in actual, d
            assert all(before[k] in actual for k in ("base_tree", "head_tree")), d
            assert user["created_at_ms"] == prompt, d
            prompts.append(actual.replace(done["session_id"], "<carrier>"))
        rows.append(dict(bug=bug, arm=arm, child=child, effort=effort, profile=selection["profile"],
                         digest=runtime.get("image_digest"), seconds=elapsed))
    assert len({row["child"] for row in rows}) == len(rows)
    if canaries:
        assert len(prompts) == 2 and prompts[0] == prompts[1]
    active = peak = 0
    for _, step in sorted(events):
        active += step
        peak = max(peak, active)
    assert 0 < peak <= 16
    return dict(coverage=coverage, peak_recorded_new_turns=peak, sessions=rows), frozen


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("root", type=pathlib.Path)
    p.add_argument("--canaries", action="store_true")
    p.add_argument("--hash-file", type=pathlib.Path, help="write the validated new reviewers' SHA256 manifest")
    args = p.parse_args()
    result, frozen = audit(args.root, args.canaries)
    if args.hash_file:
        manifest = "".join(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.relative_to(args.root)}\n"
                           for path in sorted(frozen))
        with args.hash_file.open("x") as out:
            out.write(manifest)
    print(json.dumps(result, indent=2))
