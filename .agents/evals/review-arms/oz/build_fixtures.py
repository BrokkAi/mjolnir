#!/usr/bin/env python3
"""Builds the Wizard-of-Oz `site_context` fixtures, one JSON file per diff.

Output: <out>/<intro sha>.json (default /mnt/optane/review-arms-oz/private/fixtures, dir mode 0700)
plus <out>/summary.json. Unresolved anchors go to unresolved-anchors.md next to this script.

Facts are exactly arm W's kept sibling facts (briefs/bifrost-diluted/<diff>/meta.json `kept`)
plus arm D's decisive items (briefs/bifrost-plus/<diff>/meta.json `inserted`, id
"<diff>:decisive:<n>"). The fact text is the brief item without its leading "- ", including the
trailing `[incomplete: ...]` tag, so it matches what W's brief shows.

Anchor rule. A fact may cite several sites. The anchor is the FIRST cited site of the fact's
subject: the first `fqn` of the item and the first line of its citation
`(`path:line`, line, ...)`. An item without that parenthesized form anchors on the first
`path:line` cited anywhere in it. The anchor's column and source text come from that line at the intro
commit (git show <intro>:<path>), first rule that applies:
  1. a backtick snippet of the fact (a trailing `...` removed, whitespace ignored) found on the
     line: the site is the snippet (to the end of the line when the snippet was truncated with `...`); role `statement` when the
     snippet begins with a statement keyword, else `value`;
  2. else the simple name of another function the fact names, found on the line: the site is
     that name, extended to the matching `)` when it closes on the same line; role `call` when a
     `(` follows, else `value`;
  3. else the first non-blank column and the trimmed line, role `statement`.
Source text is always a single line. The value is `known str` when the text is a string
literal, else `unknown (not_constant)`. Roles and values are provisional by design; the schema
only fixes their shape. The enclosing function is the innermost function whose body contains the
line (anchor_function, null when the line is outside every function, e.g. a struct field).
Scope. Functions whose body overlaps a new-side hunk of `git diff base..intro` (.rs files), and
every line cited anywhere in R/briefs/bifrost/<diff>/brief.md as a call site.
Functions are found with a small Rust scanner (comments/strings masked, brace tracking). FQN:
<crate lib name, else package name, - as _>.<file module path>[.<inline mods>][.<impl/trait type>].<fn>; for impl
methods the module is where the type is declared (as Bifrost renders it), with the file-module
form kept as an alias. The scanner is checked against the FQNs the briefs cite (see summary).
"""
import argparse
import collections
import json
import os
import pathlib
import re
import subprocess
import sys

import jsonschema

HERE = pathlib.Path(__file__).resolve().parent
R = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))
REPO = os.environ.get("REVIEW_ARMS_REPO") or subprocess.check_output(
    ["git", "-C", str(HERE), "rev-parse", "--show-toplevel"]).decode().strip()
SCHEMA = json.load(open(HERE / "site_context_view.schema.json"))
VALIDATOR = jsonschema.Draft202012Validator(SCHEMA)

CITE = re.compile(r"^- `([^`]+)`(?: [a-z]+)? \(`([^`:]+):(\d+)`((?:, \d+)*)\)")
ANY_CITE = re.compile(r"`([\w./-]+\.\w+):(\d+)`((?:, \d+)*)")
FQN = re.compile(r"^[A-Za-z_]\w*(?:(?:\.|::)\w+)+$")
STATEMENT = re.compile(r"^(if|while|for|let|match|return|loop|else|break|continue)\b")


def git(*args, check=True):
    p = subprocess.run(["git", "-C", REPO, *args], capture_output=True)
    if check and p.returncode:
        raise RuntimeError(f"git {args}: {p.stderr.decode()}")
    return p.stdout.decode("utf-8", "surrogateescape")


class Repo:
    def __init__(self, commit):
        self.commit, self.files, self.crates, self.typedefs = commit, {}, None, {}

    def source(self, path):
        if path not in self.files:
            p = subprocess.run(["git", "-C", REPO, "show", f"{self.commit}:{path}"], capture_output=True)
            self.files[path] = p.stdout.decode("utf-8", "replace") if p.returncode == 0 else None
        return self.files[path]

    def crate_of(self, path):
        """(crate name, src dir) for a repo-relative path."""
        if self.crates is None:
            self.crates = {}
            for entry in git("ls-tree", "-r", "--name-only", self.commit).splitlines():
                if not entry.endswith("Cargo.toml"):
                    continue
                text = self.source(entry) or ""
                package = re.search(r'^\[package\][^\[]*?^name\s*=\s*"([^"]+)"', text, re.M | re.S)
                if not package:
                    continue
                directory = entry.removesuffix("Cargo.toml").rstrip("/")
                lib = re.search(r'^\[lib\][^\[]*?^name\s*=\s*"([^"]+)"', text, re.M | re.S)
                name = lib.group(1) if lib else package.group(1)
                self.crates[directory] = name.replace("-", "_")
        best = max((d for d in self.crates if d == "" or path.startswith(d + "/")), key=len, default=None)
        return (self.crates[best], best) if best is not None else (None, None)

    def module_path(self, path):
        crate, root = self.crate_of(path)
        rel = path[len(root) + 1:] if root else path
        parts = rel.removesuffix(".rs").split("/")
        if parts[0] == "src":
            parts = parts[1:]
        if parts and parts[-1] in ("lib", "mod"):
            parts = parts[:-1]
        return [crate or "unknown"] + parts

    def type_module(self, path, name):
        """Module path where `name` is declared, within the crate of `path`."""
        crate, root = self.crate_of(path)
        key = (root, name)
        if key not in self.typedefs:
            pat = rf"^\s*(pub(\([a-z]+\))?\s+)?(struct|enum|union|trait)\s+{re.escape(name)}\b"
            out = git("grep", "-n", "-E", pat, self.commit, "--", (root or ".") + "/", check=False)
            hits = [l.split(":", 2)[1] for l in out.splitlines()]
            hits = [h for h in hits if h.endswith(".rs")]
            self.typedefs[key] = hits
        hits = self.typedefs[key]
        return self.module_path(hits[0]) if len(hits) == 1 else None


def mask(src):
    """Replaces comments, strings and char literals with spaces (newlines kept)."""
    out, i, n = [], 0, len(src)
    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            out.append(" " * (j - i)); i = j
        elif src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j): depth += 1; j += 2
                elif src.startswith("*/", j): depth -= 1; j += 2
                else: j += 1
            out.append(re.sub(r"[^\n]", " ", src[i:j])); i = j
        elif c == "r" and re.match(r'r#*"', src[i:i + 8]) and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            m = re.match(r'r(#*)"', src[i:])
            end = src.find('"' + m.group(1), i + len(m.group(0)))
            j = n if end < 0 else end + 1 + len(m.group(1))
            out.append(re.sub(r"[^\n]", " ", src[i:j])); i = j
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            j = min(j + 1, n)
            out.append(re.sub(r"[^\n]", " ", src[i:j])); i = j
        elif c == "'":
            m = re.match(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]+\}|.)|[^\\'])'", src[i:i + 14])
            if m:
                out.append(" " * len(m.group(0))); i += len(m.group(0))
            else:
                out.append(c); i += 1
        else:
            out.append(c); i += 1
    return "".join(out)


def scan_functions(src):
    """Functions with bodies: dicts name, owner, mods, start_line, end_line, parent."""
    m = mask(src)
    line_starts = [0] + [i + 1 for i, ch in enumerate(m) if ch == "\n"]
    import bisect
    line_of = lambda off: bisect.bisect_right(line_starts, off)
    # depth counts ( and [ since the last boundary, so a `;` inside one (an array type such as
    # `[u8; 32]` in a parameter) does not cut a function header in two.
    stack, funcs, boundary, depth = [], [], 0, 0
    for i, ch in enumerate(m):
        if ch in "([":
            depth += 1
        elif ch in ")]":
            depth = max(depth - 1, 0)
        elif ch == "{":
            header = m[boundary:i]
            frame = {"kind": "block"}
            h = re.sub(r"#\[[^\]]*\]", lambda m: " " * len(m.group(0)), header)
            fm = re.search(r"\bfn\s+([A-Za-z_]\w*)", h)
            mm = re.search(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*$", h)
            im = re.search(r"\bimpl\b(.*)$", h, re.S) if not fm else None
            tm = re.search(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?trait\s+(\w+)", h)
            if fm:
                frame = {"kind": "fn", "name": fm.group(1), "start": line_of(boundary + h.find("fn", fm.start()))}
            elif mm:
                frame = {"kind": "mod", "name": mm.group(1)}
            elif tm:
                frame = {"kind": "owner", "name": tm.group(1)}
            elif im is not None:
                rest = re.sub(r"<[^<>]*(?:<[^<>]*>[^<>]*)*>", " ", im.group(1))
                rest = re.split(r"\bwhere\b", rest)[0]
                target = rest.split(" for ")[-1]
                ids = re.findall(r"[A-Za-z_]\w*", target)
                ids = [x for x in ids if x not in ("dyn", "mut", "const", "unsafe", "impl")]
                frame = {"kind": "owner", "name": ids[-1] if ids else None}
            stack.append(frame)
            boundary, depth = i + 1, 0
        elif ch == "}":
            if stack:
                frame = stack.pop()
                if frame["kind"] == "fn":
                    ctx = stack
                    funcs.append({
                        "name": frame["name"], "start_line": frame["start"], "end_line": line_of(i),
                        "mods": [f["name"] for f in ctx if f["kind"] == "mod"],
                        "owner": next((f["name"] for f in reversed(ctx) if f["kind"] == "owner"), None),
                        "parent_fn": [f["name"] for f in ctx if f["kind"] == "fn"],
                    })
            boundary, depth = i + 1, 0
        elif ch == ";" and depth == 0:
            boundary = i + 1
    return funcs


def functions_of(repo, path):
    src = repo.source(path)
    if src is None or not path.endswith(".rs"):
        return []
    out = []
    for f in scan_functions(src):
        base = repo.module_path(path) + f["mods"]
        file_fqn = base + ([f["owner"]] if f["owner"] else []) + f["parent_fn"] + [f["name"]]
        decl = repo.type_module(path, f["owner"]) if f["owner"] else None
        fqn = (decl + [f["owner"]] if decl else base + ([f["owner"]] if f["owner"] else [])) + f["parent_fn"] + [f["name"]]
        out.append({"fqn": ".".join(fqn), "aliases": sorted({".".join(file_fqn)} - {".".join(fqn)}),
                    "path": path, "start_line": f["start_line"], "end_line": f["end_line"]})
    return out


def changed_ranges(base, intro):
    ranges = collections.defaultdict(list)
    path = None
    for line in git("diff", "-U0", "--no-renames", base, intro).splitlines():
        if line.startswith("+++ "):
            path = line[6:] if line.startswith("+++ b/") else None
        elif line.startswith("@@") and path:
            m = re.match(r"@@ -\S+ \+(\d+)(?:,(\d+))? @@", line)
            start, count = int(m.group(1)), int(m.group(2) if m.group(2) is not None else 1)
            ranges[path].append((start, start + count - 1 if count else start))
    return ranges


def overlaps(fn, ranges):
    return any(s <= fn["end_line"] and e >= fn["start_line"] for s, e in ranges)


def utf8_offset(lines, line, col0):
    return sum(len(l.encode()) + 1 for l in lines[:line - 1]) + len(lines[line - 1][:col0].encode())


def resolve_anchor(repo, path, line, item, subject):
    """Returns (anchor dict, text, role, error)."""
    src = repo.source(path)
    if src is None:
        return None, f"file {path} does not exist at the intro commit"
    lines = src.split("\n")
    if not 1 <= line <= len(lines):
        return None, f"line {line} is outside {path} ({len(lines)} lines at intro)"
    text = lines[line - 1]
    if not text.strip():
        return None, f"{path}:{line} is blank at the intro commit"
    tokens = re.findall(r"`([^`]+)`", item)
    names = [t for t in tokens if FQN.match(t) and t != subject]
    snippets = [t for t in tokens if not FQN.match(t) and not re.match(r"^[\w./-]+\.\w+:\d+$", t)]
    squash = lambda s: re.sub(r"\s+", "", s)
    nospace = [(k, ch) for k, ch in enumerate(text) if not ch.isspace()]
    flat = "".join(ch for _, ch in nospace)
    col = end = role = None
    for snippet in snippets:
        want = squash(snippet.replace("...", ""))
        at = flat.find(want) if len(want) >= 3 else -1
        if at >= 0:
            col = nospace[at][0]
            # A complete snippet is the site; a truncated one (`...`) runs to the end of the line.
            end = len(text.rstrip()) if "..." in snippet else nospace[at + len(want) - 1][0] + 1
            body = text[col:end]
            role = "statement" if STATEMENT.match(body) else "value"
            break
    if col is None:
        for name in names:
            simple = re.split(r"\.|::", name)[-1]
            m = re.search(rf"(?<![\w]){re.escape(simple)}\b", text)
            if m:
                col, end, role = m.start(), m.end(), "value"
                if text[end:end + 1] in ("(", "!") or text[end:].lstrip().startswith("::<"):
                    role, depth, j = "call", 0, end
                    while j < len(text):
                        depth += text[j] == "("
                        depth -= text[j] == ")"
                        j += 1
                        if depth == 0 and j > end + 0 and text[j - 1] == ")":
                            break
                    end = j if depth == 0 else len(text.rstrip())
                break
    if col is None:
        col = len(text) - len(text.lstrip())
        end, role = len(text.rstrip()), "statement"
    site_text = text[col:end]
    if re.fullmatch(r'"(?:[^"\\]|\\.)*"', site_text):
        value = {"kind": "known", "value": {"kind": "str", "value": json.loads(site_text)}}
    else:
        value = {"kind": "unknown", "cause": {"kind": "not_constant"}}
    anchor = {
        "site": {"kind": "source", "path": path,
                 "start_byte": utf8_offset(lines, line, col), "end_byte": utf8_offset(lines, line, end),
                 "start_line": line, "start_column": col + 1, "end_line": line, "end_column": end + 1,
                 "text": site_text},
        "role": {"kind": role}, "value": value,
    }
    return anchor, None


def render_line(anchor):
    s = anchor["site"]
    out = f"{s['path']}:{s['start_line']}:{s['start_column']} {anchor['role']['kind']} `{s['text']}`"
    v = anchor["value"]
    if v["kind"] == "known":
        out += " = " + json.dumps(v["value"]["value"])
    return out


def build_diff(diff, corpus, out_dir):
    c = corpus[diff]
    base, intro = c["base_commit"], c["intro_commit"]
    repo = Repo(intro)
    kept = json.load(open(R / "briefs/bifrost-diluted" / diff / "meta.json"))["kept"]
    decisive = json.load(open(R / "briefs/bifrost-plus" / diff / "meta.json"))["inserted"]
    rows = [(k["sibling_id"], k["kind"], False, k["item"]) for k in kept]
    rows += [(f"{diff}:decisive:{n + 1}", None, True, d["item"]) for n, d in enumerate(decisive)]

    ranges = changed_ranges(base, intro)
    brief = (R / "briefs/bifrost" / diff / "brief.md").read_text()
    cited = set()
    for path, line, rest in ANY_CITE.findall(brief):
        for ln in [line] + re.findall(r"\d+", rest):
            cited.add((path, int(ln)))
    funcs, seen_paths = {}, set()

    def functions_for(path):
        if path not in seen_paths:
            seen_paths.add(path)
            funcs[path] = functions_of(repo, path)
        return funcs[path]

    facts, unresolved = [], []
    for fid, kind, is_decisive, item in rows:
        m = CITE.match(item)
        if m:
            subject, path, line = m.group(1), m.group(2), int(m.group(3))
        else:
            # Form "`fqn` verb ... at `path:line`": anchor on the first cited site anywhere in the item.
            first, site = re.match(r"^- `([^`]+)`", item), ANY_CITE.search(item)
            if not (first and site):
                unresolved.append((fid, "item has no `fqn` and no `path:line` citation to anchor on", item))
                continue
            subject, path, line = first.group(1), site.group(1), int(site.group(2))
        anchor, err = resolve_anchor(repo, path, line, item, subject)
        if err:
            unresolved.append((fid, err, item)); continue
        problems = sorted(e.message for e in VALIDATOR.iter_errors(anchor))
        if problems:
            unresolved.append((fid, "schema: " + problems[0], item)); continue
        enclosing = [f for f in functions_for(path) if f["start_line"] <= line <= f["end_line"]]
        enclosing.sort(key=lambda f: f["end_line"] - f["start_line"])
        facts.append({
            "id": fid, "kind": kind, "decisive": is_decisive,
            "fact": item[2:] if item.startswith("- ") else item,
            "line": render_line(anchor), "anchor": anchor,
            "anchor_path": path, "anchor_line": line,
            "anchor_function": enclosing[0]["fqn"] if enclosing else None,
        })
    in_scope = set()
    for path in ranges:
        for f in functions_for(path):
            if overlaps(f, ranges[path]):
                in_scope.add((path, f["fqn"], f["start_line"]))
    for path, _ in cited:
        functions_for(path)
    table, keys = [], set()
    anchor_fns = {(f["anchor_path"], f["anchor_function"]) for f in facts if f["anchor_function"]}
    for path, fl in funcs.items():
        for f in fl:
            scoped = (path, f["fqn"], f["start_line"]) in in_scope
            if scoped or (path, f["fqn"]) in anchor_fns:
                key = (path, f["fqn"], f["start_line"])
                if key not in keys:
                    keys.add(key)
                    table.append({**f, "in_scope": scoped})
    table.sort(key=lambda f: (f["path"], f["start_line"]))
    # Check the scanner against the FQNs the briefs cite with a line.
    known = collections.defaultdict(set)
    for f in table:
        known[f["path"]].add(f["fqn"])
    agree = disagree = 0
    for name, path, line in re.findall(r"`([\w.]+)` \(`([^`:]+\.rs):(\d+)`", brief):
        fl = [f for f in functions_for(path) if f["start_line"] <= int(line) <= f["end_line"]]
        if any(f["fqn"] == name for f in fl):
            agree += 1
        elif any(f["fqn"].endswith("." + name.split(".")[-1]) for f in functions_for(path)):
            disagree += 1
    fixture = {
        "diff": diff, "intro_commit": intro, "base_commit": base,
        "facts": facts, "functions": table,
        "cited_call_sites": [{"path": p, "line": l} for p, l in sorted(cited)],
    }
    (out_dir / f"{intro}.json").write_text(json.dumps(fixture, indent=1))
    stats = {
        "diff": diff, "intro": intro, "facts": len(facts), "decisive": sum(f["decisive"] for f in facts),
        "functions": len(table), "in_scope": sum(f["in_scope"] for f in table),
        "cited_call_sites": len(cited), "unresolved": len(unresolved),
        "facts_without_enclosing_function": sum(f["anchor_function"] is None for f in facts),
        "brief_fqn_agree": agree, "brief_fqn_disagree": disagree,
    }
    return stats, unresolved


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="/mnt/optane/review-arms-oz/private/fixtures")
    ap.add_argument("--diff", action="append")
    args = ap.parse_args()
    out = pathlib.Path(args.out)
    private = out.parent
    private.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(private, 0o700)
    out.mkdir(exist_ok=True, mode=0o700)
    os.chmod(out, 0o700)
    corpus = {}
    for line in open(R / "corpus/corpus.jsonl"):
        d = json.loads(line)
        corpus[d["id"]] = d
    diffs = args.diff or list(dict.fromkeys(j["bug"] for j in json.load(open(R / "ablation-jobs.json"))))
    all_stats, report = [], ["# Unresolved anchors\n"]
    for diff in diffs:
        stats, unresolved = build_diff(diff, corpus, out)
        all_stats.append(stats)
        print(json.dumps(stats))
        report.append(f"## {diff} ({len(unresolved)})\n")
        report += [f"- `{fid}`: {why}\n  - {item[:200]}" for fid, why, item in unresolved] or ["none"]
        report.append("")
    (out / "summary.json").write_text(json.dumps(all_stats, indent=1))
    (HERE / "unresolved-anchors.md").write_text("\n".join(report) + "\n")


if __name__ == "__main__":
    main()
