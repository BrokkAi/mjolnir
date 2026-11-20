#!/usr/bin/env python3
"""Stage A of the yield screen: candidate bug-fix commits from the brokkbench Rust SFT lists.

Usage: stage_a.py SFT_RUST_DIR > stage_a_candidates.jsonl
(the screen ran it on the brokkbench Rust SFT lists, 993 repo files, 2026-11-18).
A candidate changes at least one test file and one non-test file, and its oneline has a bug word.
The committed stage_a_candidates.jsonl.gz is this script's output for that run; scan.py and
yield_rank.py take the decompressed file as input.
"""
import json
import pathlib
import re
import sys

BUG = re.compile(r"\b(fix(?:es|ed)?|bug|panic|crash|regression|wrong|incorrect|off.by.one|race|deadlock"
                 r"|overflow|corrupt|leak|hang|fail(?:s|ed)?)\b", re.I)


def main():
    for f in sorted(pathlib.Path(sys.argv[1]).glob("*.jsonl")):
        for line in f.open(errors="replace"):
            try:
                x = json.loads(line)
            except ValueError:
                continue
            if int(x.get("test_files") or 0) >= 1 and int(x.get("non_test_files") or 0) >= 1 \
                    and BUG.search(x.get("oneline", "")):
                print(json.dumps({"repo": f.stem, "hash": x.get("hash"), "oneline": x.get("oneline", ""),
                                  "test_files": x.get("test_files"), "non_test_files": x.get("non_test_files"),
                                  "p2p": x.get("p2p"), "outcome": x.get("outcome"),
                                  "validated_image": x.get("validated_image")}))


if __name__ == "__main__":
    main()
