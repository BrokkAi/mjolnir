"""Offline CLI regressions for corpus cutoff instants and sampling determinism.

Run with: python3 -m unittest discover -s .agents/evals/review-arms -p test_corpus_cutoff.py
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPTS = Path(__file__).resolve().parent
TIMEZONES = ("UTC", "America/Chicago", "Etc/GMT-9")
CUTOFFS = (None, "2026-11-01T00:00:00Z", "2026-10-31T19:00:00-05:00")
DATES = (
    "2026-10-01T00:00:00Z",  # Root commit; never a candidate.
    "2026-10-31T23:59:59Z",
    "2026-11-01T00:59:59+01:00",
    "2026-10-31T18:59:59-05:00",
    "2026-10-31T17:59:59-06:00",
    "2026-11-01T00:00:00Z",
    "2026-11-01T01:00:00+01:00",
    "2026-11-01T00:00:01Z",
    "2026-10-31T23:30:00-01:00",
)


def run_cli(script, args, env=None):
    return subprocess.run(
        [sys.executable, str(SCRIPTS / script), *map(str, args)],
        env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1", **(env or {})},
        text=True, capture_output=True,
    )


class CorpusCutoffTests(unittest.TestCase):
    # Hard-won: 6fb0df120: host-local bounds admitted late commits.
    def test_git_boundary_and_seeded_output_across_timezones(self):
        with tempfile.TemporaryDirectory(prefix="corpus-cutoff-git-") as temp:
            root = Path(temp)
            repo = root / "repo"
            repo.mkdir()

            def git(*args, env=None):
                return subprocess.check_output(
                    ["git", "-C", str(repo), *args],
                    env={**os.environ, **(env or {})}, text=True,
                    stderr=subprocess.PIPE,
                ).strip()

            git("init", "-b", "master")
            git("config", "user.name", "UTC boundary")
            git("config", "user.email", "boundary@example.invalid")
            git("config", "commit.gpgsign", "false")
            shas = []
            for index, date in enumerate(DATES):
                name = f"change{index}.py"
                (repo / name).write_text("".join(
                    f"value_{line} = {index + line}\n" for line in range(100)
                ))
                git("add", name)
                git("commit", "-m", f"change {index}", env={
                    "GIT_AUTHOR_DATE": date, "GIT_COMMITTER_DATE": date,
                })
                shas.append(git("rev-parse", "HEAD"))
            candidates = root / "candidates.jsonl"
            candidates.write_text("")
            outputs = []
            for tz in TIMEZONES:
                for cutoff in CUTOFFS:
                    with self.subTest(tz=tz, cutoff=cutoff):
                        args = [repo, "4", "--candidates", candidates, "--seed", "20261117"]
                        if cutoff:
                            args += ["--cutoff", cutoff]
                        result = run_cli("select_diffs.py", args, {"TZ": tz})
                        self.assertEqual(result.returncode, 0, result.stderr)
                        rows = [json.loads(line) for line in result.stdout.splitlines()]
                        self.assertEqual(len(rows), 4)
                        self.assertEqual({row["intro_commit"] for row in rows}, set(shas[1:5]))
                        outputs.append(result.stdout)
            self.assertEqual(len(set(outputs)), 1, "seeded output/order changed with timezone")

    # Hard-won: 6fb0df120: lexical issue dates misordered explicit offsets.
    def test_issue_boundary_and_seeded_order_across_timezones(self):
        with tempfile.TemporaryDirectory(prefix="corpus-cutoff-issues-") as temp:
            root = Path(temp)
            fixture = [
                {"number": number, "title": str(number), "created_at": date,
                 "closed_at": "2026-11-02T00:00:00Z", "labels": [{"name": "bug"}],
                 "state_reason": "completed"}
                for number, date in enumerate(DATES[1:], 1)
            ]
            fixture += [{**fixture[0], "number": 99, "pull_request": {}},
                        {**fixture[0], "number": 100, "state_reason": "not_planned"}]
            # Exercise the gh subprocess boundary without network access.
            fake_bin = root / "bin"
            fake_bin.mkdir()
            gh = fake_bin / "gh"
            gh.write_text(f"#!{sys.executable}\nprint({json.dumps([fixture])!r})\n")
            gh.chmod(0o755)
            outputs = []
            for index, tz in enumerate(TIMEZONES):
                for offset, cutoff in enumerate(CUTOFFS):
                    with self.subTest(tz=tz, cutoff=cutoff):
                        dest = root / f"output-{index}-{offset}"
                        args = ["example/repo", dest]
                        if cutoff:
                            args += ["--before", cutoff]
                        result = run_cli("select_bug_issues.py", args, {
                            "TZ": tz, "PATH": str(fake_bin) + os.pathsep + os.environ["PATH"],
                        })
                        self.assertEqual(result.returncode, 0, result.stderr)
                        pool = (dest / "bug-issues-pool.jsonl").read_text()
                        order = (dest / "bug-issues-order.jsonl").read_text()
                        self.assertEqual({json.loads(line)["number"] for line in pool.splitlines()},
                                         {1, 2, 3, 4})
                        outputs.append((pool, order))
            self.assertEqual(len(set(outputs)), 1, "pool or seeded order changed with timezone")

    # Hard-won: 6fb0df120: naive arguments silently used the host timezone.
    def test_ambiguous_cutoffs_fail_before_external_work(self):
        for script, args, flag in (
            ("select_diffs.py", ["missing-repo", "4"], "--cutoff"),
            ("select_bug_issues.py", ["example/repo", "missing-out"], "--before"),
        ):
            for cutoff in ("2026-11-01T00:00:00", "2026-11-01", "not-a-date"):
                with self.subTest(script=script, cutoff=cutoff):
                    result = run_cli(script, [*args, flag, cutoff])
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertRegex(result.stderr, "explicit timezone|explicit UTC offset")


if __name__ == "__main__":
    unittest.main()
