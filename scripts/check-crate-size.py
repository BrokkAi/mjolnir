#!/usr/bin/env python3
"""Keep every workspace crate small enough to compile quickly.

Cargo compiles a crate as one unit, and most of that work is serial, so the
size of the largest crate sets the floor for every build that touches it
(.agents/plans/fan-out-crates-under-size-budget.md has the measurements). The
budget is counted in lines of Rust under a crate's `src/`, tests included,
because a test build compiles the code and its tests together.

A crate under the budget must stay under it. The crates that were over it when
the budget was introduced are listed in OVER_BUDGET and are only reported:
they are being split, and a cap on them would block unrelated work in the
meantime. Remove a crate from the list as soon as it fits, so that it cannot
grow back.
"""
import json
import os
import subprocess
import sys

BUDGET = 50_000

OVER_BUDGET = {
    'brokk-mj-controller',
    'brokk-mj-daemon',
    'brokk-mj-worker-runtime',
    'brokk-mj-tui',
    'brokk-mj-core',
    'brokk-mj-chat',
}


def rust_lines(directory):
    total = 0
    for parent, _, files in os.walk(directory):
        for name in files:
            if name.endswith('.rs'):
                with open(os.path.join(parent, name), 'rb') as file:
                    total += sum(1 for _ in file)
    return total


def main():
    root = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..')
    metadata = json.loads(subprocess.run(
        ['cargo', 'metadata', '--format-version', '1', '--no-deps', '--locked'],
        cwd=root, check=True, capture_output=True, text=True).stdout)
    sizes = {p['name']: rust_lines(os.path.join(os.path.dirname(p['manifest_path']), 'src'))
             for p in metadata['packages']}
    failures = []
    for name, lines in sorted(sizes.items(), key=lambda item: -item[1]):
        if lines <= BUDGET:
            state = 'ok'
            if name in OVER_BUDGET:
                state = 'FITS'
                failures.append(f'{name} now fits the budget: remove it from OVER_BUDGET in {os.path.basename(__file__)}')
        elif name in OVER_BUDGET:
            state = 'over, being split'
        else:
            state = 'OVER'
            failures.append(f'{name} has {lines:,} lines of Rust, over the budget of {BUDGET:,}: '
                            'move part of it into another crate')
        print(f'{lines:>8,}  {name:<24} {state}')
    for name in sorted(OVER_BUDGET - sizes.keys()):
        failures.append(f'{name} is in OVER_BUDGET but is not a workspace crate')
    for failure in failures:
        print(failure, file=sys.stderr)
    sys.exit(1 if failures else 0)


if __name__ == '__main__':
    main()
