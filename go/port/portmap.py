#!/usr/bin/env python3
"""Turns the Rust workspace into the Go port's map.

`rsindex` (go/port/rsindex) parses every Rust source file. This script resolves
what each file refers to and derives, from that:

  graph      which Rust files each file needs, in production code and in tests
  schedule   the order to port files in, with files that need each other grouped
  names      the Go identifier for every Rust item (see go/port/NAMING.md)
  stats      line and test counts for each crate

Everything here is derived from the Rust sources at the checked-out commit, so
nothing it prints is committed; rerun it instead.

Usage (from the repository root):
  python3 go/port/portmap.py stats
  python3 go/port/portmap.py schedule [crate-dir]
  python3 go/port/portmap.py file <path.rs>       # one file's needs and names
  python3 go/port/portmap.py names <crate-dir>    # Rust path -> Go identifier
  python3 go/port/portmap.py files <crate-dir> [--tests]   # the crate's Rust files, in port order
  python3 go/port/portmap.py tests <crate-dir>    # every Rust test and its Go name
  python3 go/port/portmap.py declarations <path.rs>...   # the Go declarations for Rust files
  python3 go/port/portmap.py declarations --write <path.rs>...   # write them where no Go file exists yet
  python3 go/port/portmap.py json                 # the whole resolved map
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from collections import defaultdict

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
RSINDEX_DIR = os.path.join(ROOT, "go", "port", "rsindex")

# Go file names ending in these words carry an implicit build constraint, so a
# Rust module with such a name gets `_mod` appended (see NAMING.md).
GO_FILE_SUFFIXES = {
    "test", "aix", "android", "darwin", "dragonfly", "freebsd", "hurd", "illumos", "ios", "js",
    "linux", "nacl", "netbsd", "openbsd", "plan9", "solaris", "wasip1", "windows", "zos", "unix",
    "386", "amd64", "arm", "arm64", "loong64", "mips", "mips64", "mips64le", "mipsle", "ppc64",
    "ppc64le", "riscv64", "s390x", "wasm",
}

GO_KEYWORDS = {
    "break", "default", "func", "interface", "select", "case", "defer", "go", "map", "struct",
    "chan", "else", "goto", "package", "switch", "const", "fallthrough", "if", "range", "type",
    "continue", "for", "import", "return", "var",
}


def crate_dirs() -> list[str]:
    return sorted(
        d for d in os.listdir(ROOT) if os.path.isfile(os.path.join(ROOT, d, "Cargo.toml"))
    )


def cargo_metadata() -> dict:
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT, capture_output=True, text=True, check=True,
    ).stdout
    return json.loads(out)


def run_rsindex(dirs: list[str]) -> list[dict]:
    binary = os.path.join(RSINDEX_DIR, "target", "release", "rsindex")
    sources = [os.path.join(RSINDEX_DIR, "src", "main.rs"), os.path.join(RSINDEX_DIR, "Cargo.toml")]
    stale = not os.path.isfile(binary) or any(
        os.path.getmtime(source) > os.path.getmtime(binary) for source in sources
    )
    if stale:
        subprocess.run(["cargo", "build", "--release", "--quiet"], cwd=RSINDEX_DIR, check=True)
    out = subprocess.run([binary, *dirs], cwd=ROOT, capture_output=True, text=True, check=True).stdout
    return json.loads(out)


def pascal(name: str) -> str:
    """`harness_runtime` -> `HarnessRuntime`, `MAX_FRAME_BYTES` -> `MaxFrameBytes`."""
    if "_" not in name and not name.isupper():
        return name[:1].upper() + name[1:]
    return "".join(part[:1].upper() + part[1:].lower() for part in name.split("_") if part)


def go_package(lib_name: str) -> str:
    return lib_name.replace("_", "")


class Crate:
    def __init__(self, directory: str, package: dict, index: dict):
        self.dir = directory
        self.package_name = package["name"]
        self.lib = None
        self.bins = []
        for target in package["targets"]:
            if "lib" in target["kind"]:
                self.lib = target["name"]
            if "bin" in target["kind"]:
                self.bins.append(target["name"])
        # A program-only crate is one Go main package under go/cmd/<binary>.
        self.go_dir = (
            f"go/{go_package(self.lib)}" if self.lib else f"go/cmd/{self.bins[0]}"
        )
        self.go_package = go_package(self.lib) if self.lib else "main"
        self.deps = {}
        self.dev_deps = {}
        for dep in package["dependencies"]:
            key = (dep.get("rename") or dep["name"]).replace("-", "_")
            if dep.get("kind") == "dev":
                self.dev_deps[key] = dep["name"]
            elif dep.get("kind") is None:
                self.deps[key] = dep["name"]
        self.files = index["files"]
        # Module path -> file, for the library or program target only.
        self.modules = {}
        for file in self.files:
            if file["target"] in ("lib", "bin"):
                self.modules[file["module"]] = file
        # Inline modules, so `crate::a::inline::Item` resolves to the file of `a`.
        self.inline = {}
        for file in self.files:
            if file["target"] not in ("lib", "bin"):
                continue
            for item in file["items"]:
                if item["kind"] == "mod":
                    parts = [file["module"], item["inline_module"], item["name"]]
                    self.inline["::".join(p for p in parts if p)] = file


class Workspace:
    def __init__(self):
        metadata = cargo_metadata()
        packages = {
            os.path.basename(os.path.dirname(p["manifest_path"])): p for p in metadata["packages"]
        }
        dirs = [d for d in crate_dirs() if d in packages]
        index = {entry["dir"]: entry for entry in run_rsindex(dirs)}
        self.crates = {d: Crate(d, packages[d], index[d]) for d in dirs}
        self.by_lib = {c.lib: c for c in self.crates.values() if c.lib}
        self.by_package = {c.package_name: c for c in self.crates.values()}
        self.resolved = {}
        for crate in self.crates.values():
            for file in crate.files:
                self.resolved[file["path"]] = self._resolve_file(crate, file)

    # --- path resolution -------------------------------------------------

    def _owner_file(self, crate: Crate, segments: list[str], depth: int = 0):
        """The file defining `crate::<segments>`, and the segments left over."""
        for length in range(len(segments), -1, -1):
            module = "::".join(segments[:length])
            file = crate.modules.get(module) or crate.inline.get(module)
            if file is None:
                continue
            rest = segments[length:]
            if not rest or depth > 8:
                return file, rest
            # A name the module does not define may be a re-export.
            name = rest[0]
            defined = any(
                item["name"] == name and item["kind"] not in ("impl", "method", "unsafe_block")
                for item in file["items"]
            )
            if defined:
                return file, rest
            for use in file["uses"]:
                exported = use["alias"] or use["path"].split("::")[-1]
                if use["glob"] or exported != name:
                    continue
                target = self._absolute(crate, file, use["path"].split("::"))
                if target and target[0] == "crate" and target[1] is crate:
                    return self._owner_file(crate, target[2] + rest[1:], depth + 1)
                if target and target[0] == "crate":
                    return ("foreign", target[1], target[2] + rest[1:])
            for use in file["uses"]:
                if not use["glob"]:
                    continue
                target = self._absolute(crate, file, use["path"].split("::"))
                if target and target[0] == "crate" and target[1] is crate:
                    found = self._owner_file(crate, target[2] + rest, depth + 1)
                    if found and found[0] != "foreign" and found[0] is not file:
                        inner = found[0]
                        if any(item["name"] == name for item in inner["items"]):
                            return found
            return file, rest
        return None

    def _absolute(self, crate: Crate, file: dict, segments: list[str]):
        """`("crate", Crate, segments)`, `("extern", name, segments)` or None."""
        if not segments:
            return None
        if segments[0] == "":
            segments = segments[1:]
            if not segments:
                return None
        head = segments[0]
        module = file["module"].split("::") if file["module"] else []
        if file["target"] not in ("lib", "bin"):
            module = []
        if head == "crate":
            if file["target"] in ("lib", "bin"):
                return ("crate", crate, segments[1:])
            return None
        if head == "self":
            return ("crate", crate, module + segments[1:])
        if head == "super":
            parent = list(module)
            rest = list(segments)
            while rest and rest[0] == "super":
                parent = parent[:-1]
                rest = rest[1:]
            return ("crate", crate, parent + rest)
        if head in self.by_lib and (head in crate.deps or head in crate.dev_deps or self.by_lib[head] is crate):
            return ("crate", self.by_lib[head], segments[1:])
        if head in crate.deps or head in crate.dev_deps:
            name = crate.deps.get(head) or crate.dev_deps.get(head)
            if name in self.by_package:
                return ("crate", self.by_package[name], segments[1:])
            return ("extern", head, segments[1:])
        if head in ("std", "core", "alloc"):
            return ("extern", head, segments[1:])
        return None

    def _resolve_file(self, crate: Crate, file: dict) -> dict:
        aliases = {}
        globs = []
        for use in file["uses"]:
            parts = use["path"].split("::")
            if use["glob"]:
                globs.append((parts, use["test_only"]))
            else:
                aliases[use["alias"] or parts[-1]] = parts
        children = set()
        if file["target"] in ("lib", "bin"):
            prefix = file["module"] + "::" if file["module"] else ""
            for module in list(crate.modules) + list(crate.inline):
                if module.startswith(prefix) and module != file["module"]:
                    children.add(module[len(prefix):].split("::")[0])

        def expand(segments: list[str], depth: int = 0):
            target = self._absolute(crate, file, segments)
            if target:
                return target
            head = segments[0]
            if head in aliases and depth < 6 and aliases[head][0] != head:
                return expand(aliases[head] + segments[1:], depth + 1)
            if head in children and file["target"] in ("lib", "bin"):
                module = file["module"].split("::") if file["module"] else []
                return ("crate", crate, module + segments)
            return None

        result = {
            "needs": set(), "test_needs": set(),
            "crates": set(), "test_crates": set(),
            "extern": defaultdict(set), "test_extern": defaultdict(set),
        }

        def record(segments: list[str], test: bool):
            target = expand(segments)
            if not target:
                return
            needs = result["test_needs" if test else "needs"]
            if target[0] == "extern":
                key = "test_extern" if test else "extern"
                result[key][target[1]].add("::".join(target[2][:3]))
                return
            owner_crate, rest = target[1], target[2]
            found = self._owner_file(owner_crate, rest)
            hops = 0
            while found and found[0] == "foreign" and hops < 4:
                owner_crate, rest = found[1], found[2]
                found = self._owner_file(owner_crate, rest)
                hops += 1
            if owner_crate is not crate:
                result["test_crates" if test else "crates"].add(owner_crate.dir)
            if found and found[0] != "foreign" and found[0]["path"] != file["path"]:
                needs.add(found[0]["path"])

        whole_file_test = file["test_only"] or file["target"] not in ("lib", "bin")
        for use in file["uses"]:
            record(use["path"].split("::"), whole_file_test or use["test_only"])
        for path in file["paths"]:
            record(path.split("::"), whole_file_test)
        for path in file["test_paths"]:
            record(path.split("::"), True)
        for parts, test in globs:
            record(parts, whole_file_test or test)
        result["test_needs"] -= result["needs"]
        result["test_crates"] -= result["crates"]
        return result

    # --- measurements ----------------------------------------------------

    @staticmethod
    def line_split(file: dict) -> tuple[int, int]:
        """(production lines, test lines)."""
        total = file["lines"]
        if file["test_only"] or file["target"].startswith("test"):
            return 0, total
        spans = sorted(
            (item["start"], item["end"]) for item in file["items"] if item["test_only"]
        )
        test = 0
        reached = 0
        for start, end in spans:
            start = max(start, reached + 1)
            if end >= start:
                test += end - start + 1
                reached = end
        return total - test, test

    @staticmethod
    def tests_in(file: dict) -> list[dict]:
        return [item for item in file["items"] if item["test_attr"]]

    # --- names -----------------------------------------------------------

    @staticmethod
    def go_file(crate: Crate, file: dict) -> str:
        """The Go source file a Rust file's production code is ported to."""
        if file["target"].startswith("test:"):
            stem = file["target"].split(":", 1)[1]
            module = file["module"]
            base = "it_" + "_".join(p for p in [stem, *module.split("::")] if p)
            return f"{crate.go_dir}/{base}_test.go"
        if file["target"] == "build":
            return f"{crate.go_dir}/build_script.go"
        if file["target"].startswith("example:"):
            return f"go/examples/{crate.dir}/{file['target'].split(':', 1)[1]}/main.go"
        parts = [p for p in file["module"].split("::") if p]
        base = "_".join(parts) if parts else ("lib" if crate.lib else "main")
        if base.split("_")[-1] in GO_FILE_SUFFIXES:
            base += "_mod"
        suffix = "_test.go" if file["test_only"] else ".go"
        if file["test_only"] and base.endswith("_tests"):
            base = base[: -len("_tests")] or "lib"
        return f"{crate.go_dir}/{base}{suffix}"

    @staticmethod
    def module_prefix(file: dict, item: dict) -> list[str]:
        parts = [p for p in file["module"].split("::") if p]
        parts += [p for p in item["inline_module"].split("::") if p]
        return parts

    @staticmethod
    def module_go_prefix(crate: Crate, file: dict, item: dict | None = None) -> str:
        """`Runtime` for module `runtime`, `RelayProtocol` for `relay::protocol`,
        `Lib` or `Main` for the crate root."""
        parts = [p for p in file["module"].split("::") if p]
        if item is not None:
            parts += [p for p in item["inline_module"].split("::") if p and p != "tests"]
        if not parts:
            return "Lib" if crate.lib else "Main"
        return "".join(pascal(p) for p in parts)

    def go_name(self, crate: Crate, file: dict, item: dict) -> str | None:
        """The Go identifier of a Rust item (NAMING in translation-rules.md, rule 2).

        Module prefix, underscore, then the Rust name as written:
        `runtime::ShutdownReport` is `Runtime_ShutdownReport`,
        `runtime::block_on` is `Runtime_block_on`, `ShutdownState::new` is
        `Runtime_ShutdownState_new`. The first letter is lower case for an
        item that is not `pub`. A method with a `self` receiver is the Rust
        name in PascalCase (camelCase when not `pub`).
        """
        kind = item["kind"]
        if kind in ("impl", "unsafe_block", "mod", "trait_fn"):
            return None
        if kind == "method" and item["has_self"]:
            name = pascal(item["name"])
            if item["vis"] != "pub":
                name = name[:1].lower() + name[1:]
                if name in GO_KEYWORDS:
                    name += "_"
            return name
        name = self.module_go_prefix(crate, file, item)
        owner = item.get("owner")
        if kind in ("method", "assoc_const") and owner:
            name += "_" + owner.split("::")[-1]
        if item.get("fn_scope"):
            # An item declared inside a function body is named after the function.
            name += "_" + item["fn_scope"]
        name += "_" + item["name"]
        exported = item["vis"] == "pub" and kind != "macro_rules"
        if not exported:
            name = name[:1].lower() + name[1:]
        return name

    def test_go_name(self, crate: Crate, file: dict, item: dict) -> str:
        parts = [p for p in self.module_prefix(file, item) if p != "tests"]
        if file["target"].startswith("test:"):
            parts = ["It", file["target"].split(":", 1)[1], *parts]
        prefix = "".join(pascal(p) for p in parts) or ("Lib" if crate.lib else "Main")
        return f"Test{prefix}_{item['name']}"


# --- graph -----------------------------------------------------------------


def strongly_connected(nodes: list[str], edges: dict[str, set[str]]) -> list[list[str]]:
    """Tarjan's algorithm, iteratively. Components come out dependencies first."""
    index = {}
    low = {}
    on_stack = set()
    stack = []
    out = []
    counter = [0]
    for root in nodes:
        if root in index:
            continue
        work = [(root, iter(sorted(edges.get(root, ()))))]
        index[root] = low[root] = counter[0]
        counter[0] += 1
        stack.append(root)
        on_stack.add(root)
        while work:
            node, children = work[-1]
            advanced = False
            for child in children:
                if child not in edges and child not in index and child not in nodes_set(nodes):
                    continue
                if child not in index:
                    index[child] = low[child] = counter[0]
                    counter[0] += 1
                    stack.append(child)
                    on_stack.add(child)
                    work.append((child, iter(sorted(edges.get(child, ())))))
                    advanced = True
                    break
                if child in on_stack:
                    low[node] = min(low[node], index[child])
            if advanced:
                continue
            work.pop()
            if work:
                parent = work[-1][0]
                low[parent] = min(low[parent], low[node])
            if low[node] == index[node]:
                component = []
                while True:
                    member = stack.pop()
                    on_stack.discard(member)
                    component.append(member)
                    if member == node:
                        break
                out.append(sorted(component))
    return out


_NODE_SETS = {}


def nodes_set(nodes: list[str]) -> set[str]:
    key = id(nodes)
    if key not in _NODE_SETS:
        _NODE_SETS[key] = set(nodes)
    return _NODE_SETS[key]


def crate_order(ws: Workspace) -> list[str]:
    """Crates with the crates their production code needs first."""
    needs = {}
    for crate in ws.crates.values():
        needs[crate.dir] = {
            ws.by_package[name].dir for name in crate.deps.values() if name in ws.by_package
        }
    ordered = []
    remaining = dict(needs)
    while remaining:
        ready = sorted(d for d, deps in remaining.items() if deps <= set(ordered))
        if not ready:
            raise SystemExit(f"crate dependency cycle among {sorted(remaining)}")
        # Smaller foundations first within a layer keeps early waves short.
        ordered.extend(ready)
        for d in ready:
            del remaining[d]
    return ordered


def crate_schedule(ws: Workspace, crate: Crate) -> list[dict]:
    """Groups of files to port together, in an order that never looks ahead."""
    files = [f for f in crate.files if f["target"] in ("lib", "bin") and not f["test_only"]]
    paths = [f["path"] for f in files]
    known = set(paths)
    edges = {p: {n for n in ws.resolved[p]["needs"] if n in known} for p in paths}
    components = strongly_connected(paths, edges)
    layer_of = {}
    groups = []
    for component in components:
        members = set(component)
        below = {dep for member in component for dep in edges[member]} - members
        layer = 1 + max((layer_of[dep] for dep in below), default=-1)
        for member in component:
            layer_of[member] = layer
        prod = test = tests = 0
        by_path = {f["path"]: f for f in crate.files}
        for member in component:
            p, t = ws.line_split(by_path[member])
            prod += p
            test += t
            tests += len(ws.tests_in(by_path[member]))
        groups.append({
            "layer": layer, "files": component, "prod_lines": prod, "test_lines": test,
            "tests": tests, "needs": sorted(below),
        })
    groups.sort(key=lambda g: (g["layer"], g["files"][0]))
    return groups


# --- commands --------------------------------------------------------------


def cmd_stats(ws: Workspace) -> None:
    print(f"{'crate':22} {'go package':26} {'files':>5} {'prod':>7} {'test':>7} {'tests':>6}  needs")
    totals = [0, 0, 0, 0]
    for directory in crate_order(ws):
        crate = ws.crates[directory]
        prod = test = tests = 0
        for file in crate.files:
            p, t = ws.line_split(file)
            prod += p
            test += t
            tests += len(ws.tests_in(file))
        totals = [totals[0] + len(crate.files), totals[1] + prod, totals[2] + test, totals[3] + tests]
        needs = sorted(ws.by_package[n].dir for n in crate.deps.values() if n in ws.by_package)
        print(f"{directory:22} {crate.go_dir:26} {len(crate.files):5} {prod:7} {test:7} {tests:6}  {' '.join(needs)}")
    print(f"{'TOTAL':22} {'':26} {totals[0]:5} {totals[1]:7} {totals[2]:7} {totals[3]:6}")


def cmd_schedule(ws: Workspace, only: str | None) -> None:
    for directory in crate_order(ws):
        if only and directory != only:
            continue
        crate = ws.crates[directory]
        groups = crate_schedule(ws, crate)
        print(f"## {directory} -> {crate.go_dir}  ({len(groups)} groups)")
        for group in groups:
            files = " ".join(os.path.relpath(f, directory) for f in group["files"])
            print(f"  L{group['layer']:<3} prod={group['prod_lines']:<6} test={group['test_lines']:<6} tests={group['tests']:<4} {files}")


def cmd_file(ws: Workspace, path: str) -> None:
    path = os.path.relpath(os.path.abspath(path), ROOT)
    crate = next(c for c in ws.crates.values() if any(f["path"] == path for f in c.files))
    file = next(f for f in crate.files if f["path"] == path)
    resolved = ws.resolved[path]
    prod, test = ws.line_split(file)
    print(f"file      {path}")
    print(f"module    {crate.lib or crate.bins[0]}::{file['module']}".rstrip(":"))
    print(f"go file   {ws.go_file(crate, file)}")
    print(f"lines     production {prod}, test {test}, tests {len(ws.tests_in(file))}")
    if file["cfg"]:
        print(f"cfg       {file['cfg']}")
    for label, key in (("needs", "needs"), ("tests need", "test_needs")):
        for need in sorted(resolved[key]):
            owner = next(c for c in ws.crates.values() if any(f["path"] == need for f in c.files))
            target = next(f for f in owner.files if f["path"] == need)
            print(f"{label:9} {need} -> {ws.go_file(owner, target)}")
    for label, key in (("extern", "extern"), ("test ext", "test_extern")):
        for name in sorted(resolved[key]):
            print(f"{label:9} {name}: {', '.join(sorted(resolved[key][name]))}")
    print("items")
    for item in file["items"]:
        name = ws.go_name(crate, file, item)
        if name is None or item["test_attr"]:
            continue
        where = item["inline_module"] + "::" if item["inline_module"] else ""
        owner = (item["owner"] + "::") if item.get("owner") else ""
        tag = " [test-only]" if item["test_only"] else ""
        print(f"  {item['kind']:11} {where}{owner}{item['name']:40} -> {name}{tag}")
    for item in ws.tests_in(file):
        print(f"  test        {item['name']:40} -> {ws.test_go_name(crate, file, item)}")


def cmd_names(ws: Workspace, directory: str) -> None:
    crate = ws.crates[directory]
    for file in crate.files:
        for item in file["items"]:
            name = ws.go_name(crate, file, item)
            if name is None or item["test_attr"] or item["kind"] == "method" and item["has_self"]:
                continue
            parts = [crate.lib or crate.bins[0], *ws.module_prefix(file, item)]
            if item.get("owner"):
                parts.append(item["owner"].split("::")[-1])
            parts.append(item["name"])
            print(f"{'::'.join(parts)}\t{crate.go_package}.{name}\t{item['kind']}")


def cmd_tests(ws: Workspace, directory: str) -> None:
    crate = ws.crates[directory]
    for file in crate.files:
        for item in ws.tests_in(file):
            flags = [item["test_attr"]]
            if item["ignore"]:
                flags.append("ignore")
            if item["should_panic"]:
                flags.append("should_panic")
            print(f"{file['path']}:{item['start']}\t{item['name']}\t{ws.test_go_name(crate, file, item)}\t{' '.join(flags)}")


def cmd_files(ws: Workspace, directory: str, tests: bool) -> None:
    """The Rust files of a crate, production code first in schedule order."""
    crate = ws.crates[directory]
    seen = set()
    for group in crate_schedule(ws, crate):
        for path in group["files"]:
            seen.add(path)
            print(path)
    if tests:
        for file in crate.files:
            if file["path"] not in seen and file["target"] not in ("build",) and not file["target"].startswith("example"):
                print(file["path"])


def cmd_json(ws: Workspace) -> None:
    out = {}
    for directory in crate_order(ws):
        crate = ws.crates[directory]
        files = []
        for file in crate.files:
            resolved = ws.resolved[file["path"]]
            prod, test = ws.line_split(file)
            files.append({
                "path": file["path"], "module": file["module"], "target": file["target"],
                "go_file": ws.go_file(crate, file), "cfg": file["cfg"], "test_only": file["test_only"],
                "prod_lines": prod, "test_lines": test,
                "tests": [
                    {"name": t["name"], "go": ws.test_go_name(crate, file, t), "attr": t["test_attr"],
                     "ignore": t["ignore"], "should_panic": t["should_panic"]}
                    for t in ws.tests_in(file)
                ],
                "needs": sorted(resolved["needs"]), "test_needs": sorted(resolved["test_needs"]),
                "crates": sorted(resolved["crates"]), "test_crates": sorted(resolved["test_crates"]),
                "extern": {k: sorted(v) for k, v in sorted(resolved["extern"].items())},
                "test_extern": {k: sorted(v) for k, v in sorted(resolved["test_extern"].items())},
            })
        out[directory] = {
            "go_dir": crate.go_dir, "go_package": crate.go_package, "lib": crate.lib,
            "bins": crate.bins, "files": files, "schedule": crate_schedule(ws, crate),
        }
    json.dump(out, sys.stdout, indent=1)
    print()


def main(argv: list[str]) -> int:
    if len(argv) < 2 or argv[1] in ("-h", "--help"):
        print(__doc__)
        return 0 if len(argv) > 1 else 2
    ws = Workspace()
    command = argv[1]
    if command == "stats":
        cmd_stats(ws)
    elif command == "schedule":
        cmd_schedule(ws, argv[2].rstrip("/") if len(argv) > 2 else None)
    elif command == "file":
        cmd_file(ws, argv[2])
    elif command == "names":
        cmd_names(ws, argv[2].rstrip("/"))
    elif command == "tests":
        cmd_tests(ws, argv[2].rstrip("/"))
    elif command == "declarations":
        sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
        import declgen
        write = "--write" in argv
        targets = [a for a in argv[2:] if not a.startswith("--")]
        for target in targets:
            files, notes = declgen.declaration_files(ws, os.path.relpath(os.path.abspath(target), ROOT))
            for path, text in files.items():
                if not write:
                    if len(files) > 1 or len(targets) > 1:
                        print(f"// ==> {path} <==")
                    sys.stdout.write(text)
                    continue
                full = os.path.join(ROOT, path)
                if os.path.exists(full):
                    # A file that exists may hold ported bodies; never replace it.
                    print(f"{path}: exists, not written", file=sys.stderr)
                    continue
                os.makedirs(os.path.dirname(full), exist_ok=True)
                with open(full, "w", encoding="utf-8") as out:
                    out.write(text)
                print(f"{path}: written", file=sys.stderr)
            print(f"{target}: {notes} places marked PORT:", file=sys.stderr)
    elif command == "files":
        cmd_files(ws, argv[2].rstrip("/"), "--tests" in argv)
    elif command == "json":
        cmd_json(ws)
    else:
        print(__doc__)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
