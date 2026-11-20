#!/usr/bin/env python3
"""Says how far the Go port is, from the sources alone.

Every ported Go file names the Rust file it is a port of and that file's Git
blob at the time:

    // Port of mj-core/src/runtime.rs
    // rust-blob: 3f2a...

From those headers, the Rust index (portmap.py) and the Go sources, this
prints for each crate: Rust files ported, production lines they hold, calls to
rs.Todo left, Rust tests that have a Go test of the mapped name, and Rust
files that changed after their port ("drift").

    python3 go/port/status.py                 # the table
    python3 go/port/status.py --drift         # Rust files changed since their port
    python3 go/port/status.py --unported [crate]
    python3 go/port/status.py --missing-tests [crate]
    python3 go/port/status.py --todo [crate]
    python3 go/port/status.py --check         # exit 1 unless the port is complete
"""

from __future__ import annotations

import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import portmap  # noqa: E402

ROOT = portmap.ROOT
PORT_OF = re.compile(r"^// Port of (\S+\.rs)\s*$", re.M)
BLOB = re.compile(r"^// rust-blob: ([0-9a-f]{40})\s*$", re.M)
TEST_FUNC = re.compile(r"^func (Test\w+)\(", re.M)
TODO = re.compile(r"^(?!\s*//).*\b(?:rs\.|panic\()Todo\(", re.M)


def rust_blobs() -> dict[str, str]:
    out = subprocess.run(
        ["git", "ls-tree", "-r", "HEAD"], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout
    blobs = {}
    for line in out.splitlines():
        meta, path = line.split("\t", 1)
        if path.endswith(".rs"):
            blobs[path] = meta.split()[2]
    return blobs


def go_files() -> list[str]:
    found = []
    for directory, names, files in os.walk(os.path.join(ROOT, "go")):
        names[:] = [n for n in names if not n.startswith("target") and n not in ("port", "testdata")]
        for name in files:
            if name.endswith(".go"):
                found.append(os.path.join(directory, name))
    return sorted(found)


class Status:
    def __init__(self):
        self.ws = portmap.Workspace()
        self.blobs = rust_blobs()
        # Rust path -> list of (go file, blob named in its header)
        self.ports = {}
        self.todo = {}
        self.go_tests = {}
        for path in go_files():
            with open(path, encoding="utf-8") as source:
                text = source.read()
            rel = os.path.relpath(path, ROOT)
            head = text[:2000]
            port = PORT_OF.search(head)
            if port:
                blob = BLOB.search(head)
                self.ports.setdefault(port.group(1), []).append((rel, blob.group(1) if blob else None))
            count = len(TODO.findall(text))
            if count:
                self.todo[rel] = count
            directory = os.path.dirname(rel)
            if rel.endswith("_test.go"):
                self.go_tests.setdefault(directory, set()).update(TEST_FUNC.findall(text))

    def crate_rows(self):
        for directory in portmap.crate_order(self.ws):
            crate = self.ws.crates[directory]
            row = {
                "crate": directory, "files": 0, "ported": 0, "prod": 0, "prod_ported": 0,
                "tests": 0, "tests_ported": 0, "todo": 0, "drift": [], "unported": [], "missing": [],
            }
            have = self.go_tests.get(crate.go_dir, set())
            for file in crate.files:
                if file["target"] in ("build",) or file["target"].startswith("example"):
                    continue
                prod, _ = self.ws.line_split(file)
                row["files"] += 1
                row["prod"] += prod
                ports = self.ports.get(file["path"], [])
                # A file is ported when a non-test Go file names it, or, for a
                # Rust file that is all tests, when a Go test file does.
                done = [p for p in ports if file["test_only"] or file["target"].startswith("test:") or not p[0].endswith("_test.go")]
                if done:
                    row["ported"] += 1
                    row["prod_ported"] += prod
                    current = self.blobs.get(file["path"])
                    if any(blob != current for _, blob in ports):
                        row["drift"].append(file["path"])
                else:
                    row["unported"].append(file["path"])
                for test in self.ws.tests_in(file):
                    row["tests"] += 1
                    name = self.ws.test_go_name(crate, file, test)
                    if name in have:
                        row["tests_ported"] += 1
                    else:
                        row["missing"].append(f"{file['path']}:{test['start']}\t{test['name']}\t{name}")
            row["todo"] = sum(n for path, n in self.todo.items() if path.startswith(crate.go_dir + "/"))
            yield row


def main(argv: list[str]) -> int:
    if "-h" in argv or "--help" in argv:
        print(__doc__)
        return 0
    status = Status()
    rows = list(status.crate_rows())
    only = next((a.rstrip("/") for a in argv[1:] if not a.startswith("--")), None)
    selected = [r for r in rows if only is None or r["crate"] == only]

    if "--drift" in argv:
        for row in selected:
            for path in row["drift"]:
                print(path)
        return 0
    if "--unported" in argv:
        for row in selected:
            for path in row["unported"]:
                print(path)
        return 0
    if "--missing-tests" in argv:
        for row in selected:
            for line in row["missing"]:
                print(line)
        return 0
    if "--todo" in argv:
        for path, count in sorted(status.todo.items()):
            if only is None or path.startswith(status.ws.crates[only].go_dir + "/"):
                print(f"{count:5} {path}")
        return 0

    print(f"{'crate':22} {'files':>11} {'production lines':>17} {'tests':>11} {'todo':>5} {'drift':>5}")
    total = {"files": 0, "ported": 0, "prod": 0, "prod_ported": 0, "tests": 0, "tests_ported": 0, "todo": 0, "drift": 0}
    for row in rows:
        for key in total:
            total[key] += len(row[key]) if key == "drift" else row[key]
        print(
            f"{row['crate']:22} {row['ported']:5}/{row['files']:<5} "
            f"{row['prod_ported']:8}/{row['prod']:<8} {row['tests_ported']:5}/{row['tests']:<5} "
            f"{row['todo']:5} {len(row['drift']):5}"
        )
    print(
        f"{'TOTAL':22} {total['ported']:5}/{total['files']:<5} "
        f"{total['prod_ported']:8}/{total['prod']:<8} {total['tests_ported']:5}/{total['tests']:<5} "
        f"{total['todo']:5} {total['drift']:5}"
    )
    runtime_todo = sum(n for path, n in status.todo.items() if path.startswith(("go/rs/", "go/crates/")))
    print(f"\nrs.Todo calls left in the runtime packages (go/rs, go/crates): {runtime_todo}")
    if "--check" in argv:
        complete = (
            total["ported"] == total["files"] and total["tests_ported"] == total["tests"]
            and total["todo"] == 0 and total["drift"] == 0 and runtime_todo == 0
        )
        return 0 if complete else 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
