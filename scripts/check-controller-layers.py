#!/usr/bin/env python3
"""List `crate::` paths in mj-controller that point from a lower layer up into
a higher one. .agents/plans/fan-out-crates-under-size-budget.md turns each
layer into a crate, and a crate cannot depend on one above it. (The daemon
above mj-controller is already the crate mj-daemon, so the compiler enforces
that boundary.)

A layer is a set of top-level modules of mj-controller/src. The targets layer
is the lowest and may name only itself; the store layer may name itself and
the targets layer; every other module is above both. An enforced boundary
fails the check when production code crosses it. A test that crosses it is
listed and does not fail: it moves up, or its fixtures move down, when the
crate is created. Pass --all to print every site, not the summary.

The check reads paths, not names: a file that imports a name through one
`crate::` path and then uses it counts once.
"""
import collections
import os
import re
import sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'mj-controller', 'src')
# Lowest first. A module in no layer is above all of them.
LAYERS = [
    ('targets', {'targets', 'target_storage', 'image_pull_gate', 'streamed_child', 'termination',
                 'tailscale', 'aws_credentials', 'aws_assume_role', 'signing_key', 'test_log'}),
    ('store', {'database', 'upgrade', 'recovery_gate', 'worker_lifecycle', 'web_push',
               'mailbox_outbox'}),
]
# Boundaries that production code no longer crosses and must not cross again.
ENFORCED = {'targets', 'store'}

PATH = re.compile(r'\bcrate::([a-z_0-9]+)((?:::[A-Za-z_0-9]+)*)')
GROUP = re.compile(r'\bcrate::\{')
ITEM = re.compile(r'([a-z_0-9]+)((?:::[A-Za-z_0-9]+)*)')
# Unit tests sit at the end of a file, in a module gated on `cfg(test)`.
TEST_MODULE = re.compile(r'^#\[cfg\(test\)\]\n(?:pub(?:\([a-z]+\))? )?mod [a-z_0-9]+ \{', re.M)
# A fixture: one function or `use` gated on `cfg(test)` inside production code.
TEST_ITEM = re.compile(r'#\[cfg\(test\)\]\n[^\n]*(?:\n[^\n]*)?')


def layer(module):
    for index, (_, members) in enumerate(LAYERS):
        if module in members:
            return index
    return len(LAYERS)


def group_items(text, start):
    """The top-level items of a `crate::{a::X, b::{Y, Z}}` group opened before `start`."""
    depth, i, begin = 1, start, start
    while i < len(text) and depth:
        c = text[i]
        depth += (c == '{') - (c == '}')
        if (c == ',' and depth == 1) or depth == 0:
            yield text[begin:i].strip(), begin
            begin = i + 1
        i += 1


def references(text):
    for m in PATH.finditer(text):
        yield m.group(1), m.group(1) + m.group(2), m.start()
    for m in GROUP.finditer(text):
        for item, at in group_items(text, m.end()):
            seg = ITEM.match(item)
            if seg:
                yield seg.group(1), seg.group(1) + seg.group(2), at


def test_ranges(rel, text):
    """The parts of a file compiled only for tests, as (start, end) offsets."""
    name = os.path.basename(rel)
    in_test_file = (name in ('tests.rs', 'test_support.rs') or name.endswith('_tests.rs')
                    or f'{os.sep}tests{os.sep}' in rel)
    if in_test_file:
        return [(0, len(text))]
    ranges = [(m.start(), m.end()) for m in TEST_ITEM.finditer(text)]
    module = TEST_MODULE.search(text)
    if module:
        ranges.append((module.start(), len(text)))
    return ranges


def main():
    modules = {name.removesuffix('.rs') for name in os.listdir(ROOT)}
    sites = collections.defaultdict(list)
    for folder, _, files in os.walk(ROOT):
        for name in files:
            if not name.endswith('.rs'):
                continue
            path = os.path.join(folder, name)
            rel = os.path.relpath(path, ROOT)
            source = rel.split(os.sep)[0].removesuffix('.rs')
            with open(path, encoding='utf-8') as handle:
                text = handle.read()
            tests = test_ranges(rel, text)
            for target, full, at in references(text):
                if target in modules and layer(target) > layer(source):
                    boundary = LAYERS[layer(source)][0]
                    kind = 'test' if any(a <= at < b for a, b in tests) else 'code'
                    line = text.count('\n', 0, at) + 1
                    sites[boundary, kind].append((source, full, f'mj-controller/src/{rel}:{line}'))
    failed = False
    for boundary, _ in LAYERS:
        enforced = boundary in ENFORCED
        for kind in ('code', 'test'):
            found = sorted(sites[boundary, kind])
            fails = enforced and kind == 'code' and bool(found)
            failed |= fails
            state = ('FAILS' if fails else 'enforced') if enforced and kind == 'code' else 'reported'
            what = 'production' if kind == 'code' else 'test'
            print(f'{boundary} boundary: {len(found)} upward {what} reference sites ({state})')
            if '--all' in sys.argv:
                for source, full, where in found:
                    print(f'  {where}: {source} -> {full}')
            else:
                counts = collections.Counter((s, f) for s, f, _ in found)
                for (source, full), count in sorted(counts.items()):
                    print(f'  {source} -> {full} x{count}')
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
