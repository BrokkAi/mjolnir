"""Extract source-spanned callable definitions and call references from pilot diffs."""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
from tempfile import TemporaryDirectory

from .engine.snapshot import blob_reader, git_output


RUST_CAPTURES = """
(function_item name: (identifier) @definition) @body
(call_expression function: (identifier) @reference)
(call_expression function: (field_expression field: (field_identifier) @reference))
(call_expression function: (scoped_identifier name: (identifier) @reference))
(generic_function function: (identifier) @reference)
(generic_function function: (field_expression field: (field_identifier) @reference))
(generic_function function: (scoped_identifier name: (identifier) @reference))
"""
JS_CAPTURES = """
(function_declaration name: (identifier) @definition) @body
(generator_function_declaration name: (identifier) @definition) @body
(method_definition name: (property_identifier) @definition) @body
(variable_declarator name: (identifier) @definition value: [(arrow_function) (function_expression)]) @body
(call_expression function: (identifier) @reference)
(call_expression function: (member_expression property: (property_identifier) @reference))
"""
HUNK = re.compile(rb"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@", re.M)
CAPTURE = re.compile(
    r"^    capture: (?:\d+ - )?(\w+), start: \((\d+), (\d+)\), end: \((\d+), (\d+)\)(?:, text: `(.*)`)?$",
    re.M,
)


def changed_intervals(patch):
    sides = [], []
    for hunk in HUNK.finditer(patch):
        for index, intervals in enumerate(sides):
            line, count = hunk.group(index * 2 + 1, index * 2 + 2)
            start, count = int(line) - 1, int(count or b"1")
            if count:
                intervals.append((start, start + count - 1))
    return sides


def capture_symbols(output, intervals):
    symbols = set()
    body = None
    for match in CAPTURE.finditer(output):
        kind, first, _, last, last_column, text = match.groups()
        first, last = int(first), int(last) - (last_column == "0")
        if kind == "body":
            body = first, last
            continue
        start, end = body if kind == "definition" else (first, last)
        if any(start <= finish and begin <= end for begin, finish in intervals):
            symbols.add((kind, text.removeprefix("r#")))
    return symbols


def touched_symbols(repo, base, intro, rust_grammar, js_grammar):
    """Collect touched authored callable seeds in the introducing tree.

    Old hunks account for deletions inside surviving definitions and for removed
    references whose written name still has a call site in the file. The query
    scope retains the seed file and distinguishes declarations from call sites.
    """
    languages = {"rs": (rust_grammar, "rust", RUST_CAPTURES),
                 "js": (js_grammar, "javascript", JS_CAPTURES)}
    changes = git_output(repo, "diff", "--name-status", "--find-renames", "--diff-filter=ACDMRT",
                         "-z", base, intro, "--", "*.rs", "*.js").split(b"\0")
    entries = iter(changes[:-1])
    read = [blob_reader(repo, rev) for rev in (base, intro)]
    by_extension = {}
    for status in entries:
        old = os.fsdecode(next(entries))
        new = os.fsdecode(next(entries)) if status.startswith((b"R", b"C")) else old
        suffix = new.rsplit(".", 1)[-1]
        if suffix not in languages or status == b"D":
            continue
        patch = git_output(repo, "--literal-pathspecs", "diff", "--no-ext-diff", "--no-textconv",
                           "--find-renames", "--unified=0", base, intro, "--", old, new)
        previous, current = changed_intervals(patch)
        if not previous and not current:
            continue
        by_extension.setdefault(suffix, []).append((old, new, previous, current))

    symbols = set()
    for suffix, paths in by_extension.items():
        grammar, language, captures = languages[suffix]
        with TemporaryDirectory(prefix="pilot-symbols-") as scratch:
            root = Path(scratch)
            checkout = root / f"tree-sitter-{language}"
            shutil.copytree(grammar / "src", checkout / "src")
            shutil.copyfile(grammar / "tree-sitter.json", checkout / "tree-sitter.json")
            config = root / "config.json"
            config.write_text(json.dumps({"parser-directories": [str(root)]}))
            query = root / "callables.scm"
            query.write_text(captures)
            files = {}
            for index, (old, new, previous, current) in enumerate(paths):
                for side, (path, intervals) in enumerate(((old, previous), (new, current))):
                    if (data := read[side](path)) is None:
                        continue
                    source = root / f"{index}-{side}.{suffix}"
                    source.write_bytes(data)
                    files[str(source)] = (new, side, intervals, data.count(b"\n"))
            if not files:
                continue
            result = subprocess.run(
                ["tree-sitter", "query", "--config-path", config, query, *files],
                env={**os.environ, "XDG_CACHE_HOME": str(root / "cache")},
                check=True, capture_output=True, text=True,
            )
            touched, post = {}, {}
            for file, output in re.findall(r"^([^ \n][^\n]*)\n((?:[ \t].*\n)*)", result.stdout, re.M):
                if file not in files:
                    continue
                path, side, intervals, last = files[file]
                touched.setdefault(path, set()).update(capture_symbols(output, intervals))
                if side == 1:
                    post[path] = capture_symbols(output, [(0, last)])
            for path, names in touched.items():
                symbols.update((path, kind, name) for kind, name in names & post.get(path, set()))
    return symbols
