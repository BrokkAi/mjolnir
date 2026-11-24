"""Audit verify9 against the frozen pool and the Opus judge's actual turns."""
import argparse
import collections
import json
import pathlib
import re
import sqlite3


def load(path):
    return json.loads(path.read_text())


def raw_findings(path):
    text = path.read_text().strip()
    if text.startswith("No findings"):
        return []
    starts = [m.start() for m in re.finditer(r"(?m)^\s*[-*]\s*\[P\d\]", text)]
    return [text[i:j].strip() for i, j in zip(starts, starts[1:] + [len(text)])] or [text]


def ready(root):
    jobs = load(root / "sol-62-repeats/ready-jobs.json")
    slots = {(j["bug"], j["arm"]) for j in jobs}
    diffs = {bug for bug, _ in slots}
    arms = {arm for _, arm in slots}
    assert len(jobs) == len(slots) == 1064 and len(diffs) == 70
    eligible = {(p.parent.parent.name, p.parent.name) for p in root.glob("runs/*/*/done.json")
                if p.parent.parent.name in diffs and p.parent.name in arms}
    assert slots == eligible, {"missing": sorted(slots - eligible), "extra": sorted(eligible - slots)}
    assert not (root / "verify9").exists() and not (root / "private/key-verify9.json").exists()
    return {"jobs": len(jobs), "diffs": len(diffs), "arms": sorted(arms)}


def pool(root, frozen):
    study = root / "sol-62-repeats"
    baseline = [json.loads(line) for line in frozen.read_text().splitlines()]
    assert len(baseline) == 1446
    aud = load(study / "reviewer-audit.json")
    additions = [row for row in aud["sessions"] if row["arm"] in "QRST"]
    assert len(additions) == 280
    expected = {row["child"]: row for row in baseline}
    for row in additions:
        bug, arm, child = (row[k] for k in ("bug", "arm", "child"))
        path = f"runs/{bug}/{arm}/reviewers/{child[:12]}/output.md"
        assert child not in expected
        expected[child] = dict(bug=bug, arm=arm, model="gpt-6.2-sol", slot=0, output=path,
                               findings=len(raw_findings(root / path)))
    assert len(expected) == 1726
    key = load(root / "private/key-verify9.json")
    assert (root / "private/key-verify9.json").stat().st_mode & 0o077 == 0
    assert len(key["sessions"]) == 1726 and set(key["sessions"]) == set(expected)
    texts = {sid: raw_findings(root / row["output"]) for sid, row in expected.items()}
    assert all(len(texts[sid]) == expected[sid]["findings"] for sid in texts)
    ids = {row["bug"] for row in expected.values()}
    assert len(ids) == 70
    commits = {}
    for bug in ids:
        done = load(root / "runs" / bug / "O/done.json")
        commits[done["intro"]] = (bug, done["base"])
    assert len(commits) == 70
    seen_ids, seen_findings, seen_commits = set(), set(), set()
    assert (root / "verify9/SPEC.md").read_bytes() == (root / "verify5/SPEC.md").read_bytes()
    for path in sorted((root / "verify9").glob("batch-*.json")):
        for entry in load(path):
            assert set(entry) == {"intro", "base", "findings"}
            intro = entry["intro"]
            assert intro not in seen_commits
            seen_commits.add(intro)
            bug, base = commits[intro]
            assert base == entry["base"]
            for finding in entry["findings"]:
                assert set(finding) == {"id", "text"}
                fid = finding["id"]
                assert re.fullmatch(r"f-[0-9a-f]{8}", fid) and fid not in seen_ids
                seen_ids.add(fid)
                ref = key["findings"][fid]
                child, index = ref["child"], ref["index"]
                assert ref["bug"] == bug
                assert all(ref[k] == expected[child][k] for k in ("bug", "arm", "model", "slot"))
                assert (child, index) not in seen_findings
                seen_findings.add((child, index))
                assert finding["text"] == texts[child][index]
    assert seen_ids == key["findings"].keys()
    assert seen_findings == {(sid, i) for sid, snippets in texts.items() for i in range(len(snippets))}
    return dict(sessions=len(expected), diffs=len(ids), raw_findings=len(seen_ids),
                zero_finding_sessions=sum(not f for f in texts.values()), judged_diffs=len(seen_commits))


def judges(root, store, profile, batches):
    v = root / "verify9"
    paths = sorted(v.glob("batch-*.json")) if not batches else [v / f"batch-{n}.json" for n in batches]
    assert paths
    results = []
    with sqlite3.connect(f"file:{store.resolve()}?mode=ro", uri=True, timeout=30) as db:
        for path in paths:
            n = int(path.stem.removeprefix("batch-"))
            batch = load(path)
            out = load(v / "out" / f"verify-out-{n}.json")
            rec = load(v / "sessions" / f"batch-{n}.json")
            sid = rec["session_id"]
            cid = f"review-arms-verify9-batch-{n}-{sid[:12]}"
            assert rec["prompted"] and rec["profile"] == profile
            assert {k: rec["config"][k] for k in ("model", "effort", "fast", "ultracode")} == {
                "model": "opus", "effort": "high", "fast": "off", "ultracode": "off"}
            producer = out["producer"]
            assert (producer["session_id"], producer["profile"], producer["launch_profile"]) == (sid, profile, profile)
            assert producer["config"] == rec["config"]
            row, = db.execute(
                "SELECT s.command_id, s.model, s.effort, s.profile, u.body "
                "FROM session_turn_selections s LEFT JOIN session_turn_usage u USING (session_id, command_id) "
                "WHERE s.session_id = ?", (sid,),
            ).fetchall()
            command, model, effort, selected_profile, body = row
            assert (command, model, effort, selected_profile) == (cid, "opus", "high", profile), rec
            turn = json.loads(body)
            assert turn["outcome"] == {"kind": "completed", "stop_reason": "EndTurn"}, turn["outcome"]
            models = turn["usage"]["provider_details"]["model_usage"]
            assert "claude-opus-5-5" in models and models.keys() <= {"claude-opus-5-5", "claude-haiku-5-5"}
            want = {c["intro"]: collections.Counter(f["id"] for f in c["findings"]) for c in batch}
            assert len(out["commits"]) == len(want)
            for commit in out["commits"]:
                ids = want.pop(commit["intro"])
                groups = set()
                for group in commit["groups"]:
                    assert group["group"] and group["group"] not in groups
                    groups.add(group["group"])
                    assert group["findings"] and group["summary"].strip() and group["evidence"].strip()
                    assert group["confidence"] in {"high", "medium", "low"}
                    assert group["verdict"] in {"confirmed", "plausible", "rejected"}
                    valid_severity = {"high", "medium", "low"}
                    if group["verdict"] == "rejected":
                        valid_severity.add(None)
                    assert group["severity"] in valid_severity
                assert ids == collections.Counter(f for g in commit["groups"] for f in g["findings"])
            assert not want
            results.append(dict(batch=n, session=sid, command=cid, model=model, effort=effort,
                                profile=profile, turn=turn))
    assert len(results) == len({t["session"] for t in results})
    return results


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("root", type=pathlib.Path)
    modes = p.add_subparsers(dest="mode", required=True)
    modes.add_parser("ready")
    modes.add_parser("pool").add_argument("frozen", type=pathlib.Path)
    verify = modes.add_parser("judges")
    verify.add_argument("store", type=pathlib.Path)
    verify.add_argument("--profile", default="claude4")
    verify.add_argument("--batch", type=int, action="append")
    args = p.parse_args()
    if args.mode == "ready":
        result = ready(args.root)
    elif args.mode == "pool":
        result = pool(args.root, args.frozen)
    else:
        result = judges(args.root, args.store, args.profile, args.batch)
    print(json.dumps(result, indent=2))
