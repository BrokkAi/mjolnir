#!/usr/bin/env python3
"""Behavior checks for how the reliability lab stops the processes it owns.
Run directly: python3 tests/e2e/reliability_lab.test.py

No Mjolnir binary runs. The stand-ins for daemons and workers are Python
sleepers whose command lines name the lab's runtime, which is one of the ways
a lab recognizes its own processes. Each starts in its own session, as the
real ones detach, so it outlives the process that started it.
"""

from __future__ import annotations

import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest

SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from reliability_lab import Lab  # noqa: E402

HEL = pathlib.Path("/bin/true")
SLEEPER = "import time; time.sleep(300)"


def running(pid: int) -> bool:
    try:
        stat = pathlib.Path(f"/proc/{pid}/stat").read_text()
    except FileNotFoundError:
        return False
    return stat.rsplit(")", 1)[1].split()[0] != "Z"


def eventually(predicate, timeout: float = 30.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.05)
    return predicate()


def watchdogs(root: pathlib.Path) -> list[int]:
    found = []
    for entry in pathlib.Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            command = (entry / "cmdline").read_bytes().split(b"\0")
        except OSError:
            continue
        if str(root).encode() in command and b"reliability_lab.watch(sys.argv[2])" in b" ".join(command):
            found.append(int(entry.name))
    return found


class LabCleanupTest(unittest.TestCase):
    def setUp(self) -> None:
        self.artifacts = pathlib.Path(tempfile.mkdtemp(prefix="reliability-lab-test-"))
        self.addCleanup(shutil.rmtree, self.artifacts, ignore_errors=True)
        previous = os.environ.get("MJ_RELIABILITY_ARTIFACTS")
        os.environ["MJ_RELIABILITY_ARTIFACTS"] = str(self.artifacts)
        self.addCleanup(self.restore_artifacts, previous)

    @staticmethod
    def restore_artifacts(previous: str | None) -> None:
        if previous is None:
            os.environ.pop("MJ_RELIABILITY_ARTIFACTS", None)
        else:
            os.environ["MJ_RELIABILITY_ARTIFACTS"] = previous

    def discard(self, root: pathlib.Path) -> None:
        lab = Lab.reopen(root)
        lab.cleanup_owned()
        shutil.rmtree(lab.runtime_root, ignore_errors=True)

    def run_driver(self, body: str) -> tuple[subprocess.Popen[str], pathlib.Path, pathlib.Path, int]:
        """Start a driver that prepares a lab, starts one owned sleeper, and
        reports them; `body` then runs with `lab` and `sleeper` in scope."""
        code = textwrap.dedent(
            """
            import pathlib, subprocess, sys, time
            sys.path.insert(0, sys.argv[1])
            from reliability_lab import Lab
            lab = Lab(pathlib.Path(sys.argv[2]), "watchdog-test", 1)
            sleeper = subprocess.Popen(
                [sys.executable, "-c", sys.argv[3], str(lab.runtime_root)], start_new_session=True
            )
            print(lab.root, lab.runtime_root, sleeper.pid, flush=True)
            """
        ) + textwrap.dedent(body)
        driver = subprocess.Popen(
            [sys.executable, "-c", code, str(SCRIPT_DIR), str(HEL), SLEEPER],
            stdout=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        self.addCleanup(driver.wait)
        root, runtime, pid = driver.stdout.readline().split()
        driver.stdout.close()
        self.addCleanup(self.discard, pathlib.Path(root))
        return driver, pathlib.Path(root), pathlib.Path(runtime), int(pid)

    def test_cleanup_stops_a_process_that_a_stopping_process_starts(self) -> None:
        lab = Lab(HEL, "cleanup-test", 1, watchdog=False)
        self.addCleanup(self.discard, lab.root)
        # Like a supervisor that starts a replacement while it is stopped.
        replacing = textwrap.dedent(
            """
            import signal, subprocess, sys, time
            def replace(*_):
                subprocess.Popen([sys.executable, "-c", sys.argv[1], sys.argv[2]], start_new_session=True)
                sys.exit(0)
            signal.signal(signal.SIGTERM, replace)
            print("ready", flush=True)
            time.sleep(300)
            """
        )
        process = subprocess.Popen(
            [sys.executable, "-c", replacing, SLEEPER, str(lab.runtime_root)],
            stdout=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        self.addCleanup(process.wait)
        self.assertEqual(process.stdout.readline().strip(), "ready")
        process.stdout.close()

        lab.cleanup_owned()

        self.assertEqual(lab.owned_processes(), [])

    def test_the_watchdog_stops_what_a_killed_driver_left_running(self) -> None:
        driver, root, runtime, sleeper = self.run_driver("time.sleep(300)")

        # Kill the driver's whole process group, as a timeout or a closed
        # terminal does, so none of its own cleanup can run.
        os.killpg(driver.pid, signal.SIGKILL)

        self.assertTrue(eventually(lambda: not running(sleeper)), "the watchdog left the owned process running")
        self.assertTrue(eventually(lambda: not runtime.exists()), "the watchdog left the abandoned runtime")
        trace = json.loads((root / "trace.json").read_text())
        self.assertEqual(trace["outcome"], "abandoned")
        self.assertTrue(any(entry.startswith(f"{sleeper} ") for entry in trace["watchdog"]["stopped"]))
        self.assertIsNone(trace["watchdog"]["remaining"])

    def test_the_watchdog_leaves_alone_a_lab_its_driver_cleaned_up(self) -> None:
        # A driver that stops its processes but keeps the runtime for
        # diagnosis, as a failed session-move run does.
        driver, root, runtime, sleeper = self.run_driver("lab.cleanup_owned()")
        self.assertEqual(driver.wait(), 0)

        self.assertTrue(eventually(lambda: not watchdogs(root)), "the watchdog did not exit")
        self.assertFalse(running(sleeper))
        self.assertTrue(runtime.exists(), "the watchdog removed a runtime the driver kept")
        self.assertNotIn("watchdog", json.loads((root / "trace.json").read_text()))

    def test_finishing_a_luna_lab_stops_its_processes_and_removes_its_runtime(self) -> None:
        lab = Lab(HEL, "luna-manual", 1, watchdog=False)
        self.addCleanup(self.discard, lab.root)
        sleeper = subprocess.Popen([sys.executable, "-c", SLEEPER, str(lab.runtime_root)], start_new_session=True)
        self.addCleanup(sleeper.wait)

        finished = subprocess.run(
            [sys.executable, str(SCRIPT_DIR / "finish-luna-lab.py"), str(lab.root)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

        self.assertEqual(finished.returncode, 0, finished.stderr)
        self.assertTrue(eventually(lambda: not running(sleeper.pid)))
        self.assertFalse(lab.runtime_root.exists())
        self.assertEqual((lab.root / "leaks.txt").read_text(), "")


if __name__ == "__main__":
    unittest.main()
