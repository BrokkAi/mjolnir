#!/usr/bin/env python3
"""List `crate::` paths in mj-controller that point from a lower layer up into
a higher one. .agents/plans/split-mj-controller-by-layer.md turns the layers
into crates, and a crate cannot depend on one above it.

A layer is a set of top-level modules of mj-controller/src. Modules not named
below belong to the middle layer. An enforced boundary fails the check; the
other one is only reported. Pass --all to print every site, not the summary.
"""
import collections
import os
import re
import sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'mj-controller', 'src')
TOP = {'daemon', 'server', 'server_runtime', 'pollers', 'web_viewer', 'recovery', 'worker_upgrade'}
STORE = {'database', 'targets', 'upgrade', 'recovery_gate', 'worker_lifecycle', 'target_storage',
         'image_pull_gate', 'streamed_child', 'termination', 'launch_timing', 'test_log'}
# Boundaries whose upward references are already gone and must stay gone.
ENFORCED = set()

PATH = re.compile(r'\bcrate::([a-z_0-9]+)((?:::[A-Za-z_0-9]+)*)')
GROUP = re.compile(r'\bcrate::\{')
ITEM = re.compile(r'([a-z_0-9]+)((?:::[A-Za-z_0-9]+)*)')


def layer(module):
    return 2 if module in TOP else 0 if module in STORE else 1


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
            for target, full, at in references(text):
                if target in modules and layer(target) > layer(source):
                    boundary = 'top' if layer(target) == 2 else 'store'
                    line = text.count('\n', 0, at) + 1
                    sites[boundary].append((source, full, f'mj-controller/src/{rel}:{line}'))
    failed = False
    for boundary in ('top', 'store'):
        found = sorted(sites[boundary])
        enforced = boundary in ENFORCED
        failed |= enforced and bool(found)
        state = 'enforced' if enforced else 'not yet enforced'
        print(f'{len(found)} upward reference sites across the {boundary} boundary ({state})')
        if '--all' in sys.argv:
            for source, full, where in found:
                print(f'  {where}: {source} -> {full}')
        else:
            for (source, full), count in sorted(collections.Counter((s, f) for s, f, _ in found).items()):
                print(f'  {source} -> {full} x{count}')
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
