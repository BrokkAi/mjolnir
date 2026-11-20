"""Scans reviewer transcripts for access to later history or other forbidden sources.

Usage: python3 audit.py [runs/<bug>/<arm> ...]   (default: every finished run)
Writes audit.json in each run directory and prints a summary. A hit is not a
verdict: every hit is read by hand before a run is accepted or excluded.
"""
import ast
import json
import os
import pathlib
import posixpath
import re
import shlex
import sys

ROOT = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))

# Where a command word can start. Inputs are matched as serialized JSON, so a
# command at the start of an argument follows a quote or an escaping backslash.
CMD = r"""(^|[\s;&|(`"'\\])"""
# Patterns checked against what the reviewer asked for: commands, tool names, arguments.
INPUT_PATTERNS = {
    "github-or-web": CMD + r"gh\s|github\.com|api\.github|\bcurl\b|\bwget\b|https?://|claude\.ai|web_search|web\.run|websearch|webfetch|search_query",
    "git-history-escape": r"git\b[^\n]*\b(fetch|pull|clone|remote|ls-remote|bundle|fsck|reflog)\b|git\b[^\n]*(log|rev-list|for-each-ref|show-ref)\b[^\n]*--all\b|cat-file\s+--batch-all|/run/hel/git-cache|\.bundle\b",
    "outside-workspace": r"/home/jonathan|/mnt/optane|/var/lib/hel|/home/hel/\.(codex|claude|cache|config|local)|\.codex/(sessions|history|archived)|" + CMD + r"(find|ls|du|tree)\s+/(\s|$|[\"'])|grep\s+-[a-zA-Z]*r[a-zA-Z]*\s+[^\n]*\s/(\s|$|[\"'])",
    "memory-or-sessions": r"search_sessions|read_session|search_session|get_session_brief|session_files|trace_file|blame_file|mj-memory|" + CMD + r"mj\s+(transcript|sessions|events|usage)|sqlite",
    "build-or-test": r"\bcargo(\s+[-+]\S*)*\s+(build|test|check|clippy|run|nextest|bench)\b|\bnpm\s+(test|run)\b|\bpytest\b|\bpython3?\s+-m\s+(unittest|pytest)\b|\bgo\s+test\b|"
                     + CMD + r"make\s+(-\S+\s+)*\S*(test|check)\b|"
                     + CMD + r"(python3?|bash|sh|expect)\s+(-\S+\s+)*[^\s\"']*(tests?|e2e|scripts)/|" + CMD + r"\./(tests?|scripts)/",
}
# Patterns checked against tool output: places that only exist outside the checkout.
OUTPUT_PATTERNS = {
    "output-outside-path": r"/run/hel/git-cache|/home/jonathan|/mnt/optane|/var/lib/hel",
}


def call_parts(item):
    body = item.get("body") or {}
    call = body.get("call") or {}
    if call:
        inp = json.dumps({"title": call.get("title"), "kind": call.get("kind"), "rawInput": call.get("rawInput")})
        # Command output lives in the item's terminal_outputs; call.content holds only a terminal ID.
        out = json.dumps({"content": call.get("content"), "rawOutput": call.get("rawOutput"),
                          "terminal_outputs": body.get("terminal_outputs")})
        return inp, out
    return json.dumps(body), ""


def audit_reviewer(transcript_path, own_workspace):
    return scan(json.loads(transcript_path.read_text()), own_workspace)


def scan(pages, own_workspace, *, commits=None, allow_rql=False, single_commit=False):
    hits, calls = [], 0
    for page in pages:
        for item in page["items"]:
            if item["role"] not in ("tool", "terminal"):
                continue
            call = (item.get("body") or {}).get("call") or {}
            if commits is not None and not call:
                # Standalone terminal_output rows contain output only. The
                # originating command is recorded as its own tool call.
                continue
            calls += 1
            if allow_rql and call.get("title") in ("rql_query", "mcp__rql__rql_query") and call.get("kind") != "execute":
                continue
            if commits is not None:
                for name, snippet in dict(pilot_hits(call, own_workspace, commits, single_commit)).items():
                    hits.append({"seq": item["seq"], "kind": name, "where": "input", "snippet": snippet})
                continue
            inp, out = call_parts(item)
            for name, pattern in INPUT_PATTERNS.items():
                for m in re.finditer(pattern, inp, re.I | re.M):
                    start = max(0, m.start() - 160)
                    hits.append({"seq": item["seq"], "kind": name, "where": "input",
                                 "snippet": inp[start:m.end() + 160]})
                    break
            for name, pattern in OUTPUT_PATTERNS.items():
                m = re.search(pattern, out, re.I)
                if m:
                    start = max(0, m.start() - 160)
                    hits.append({"seq": item["seq"], "kind": name, "where": "output",
                                 "snippet": out[start:m.end() + 160]})
            other = re.findall(r"/workspace/([0-9a-f]{32})", inp + out)
            foreign = sorted(set(other) - {own_workspace})
            if foreign:
                hits.append({"seq": item["seq"], "kind": "other-workspace",
                             "where": "input/output",
                             "snippet": ", ".join(foreign)})
    return {"tool_items": calls, "hits": hits}


class InspectionRequired(ValueError):
    pass


SEARCH = {"rg", "grep", "egrep", "fgrep"}
SEARCH_VALUES = {
    "-A", "-B", "-C", "-m", "-g", "-t", "-T", "-M", "-j", "-E", "-r", "-d", "-D",
    "--after-context", "--before-context", "--context", "--max-count", "--max-depth",
    "--max-columns", "--threads", "--glob", "--iglob", "--type", "--type-not", "--encoding",
    "--include", "--exclude", "--exclude-dir", "--exclude-from", "--ignore-file",
    "--label", "--directories", "--devices", "--color", "--colour", "--colors",
    "--max-filesize", "--sort", "--sortr",
}
SEARCH_FLAGS = set("""
    basic-regexp extended-regexp fixed-strings perl-regexp ignore-case smart-case case-sensitive
    line-number no-line-number column no-column count count-matches word-regexp line-regexp
    with-filename no-filename heading no-heading hidden no-hidden no-ignore no-ignore-vcs
    no-ignore-parent no-ignore-global no-ignore-dot follow no-follow invert-match recursive
    dereference-recursive files-with-matches files-without-match files-without-matches
    only-matching quiet silent no-messages text binary unrestricted null null-data multiline
    stats json line-buffered block-buffered untracked cached no-index full-name break no-break
    and or not all-match no-textconv no-config
""".split())


def search_operands(program, args):
    """Separate literal patterns from files containing patterns and paths to search."""
    files, operands, trailing = [], [], []
    explicit_pattern = listing = False
    options = True
    words = iter(args)
    for word in words:
        if options and word == "--":
            options = False
        elif options and word.startswith("--"):
            name, equals, value = word.partition("=")
            if name == "--files" and program == "rg":
                listing = True
            elif name in {"--regexp", "--file"} | SEARCH_VALUES:
                value = value if equals else next(words, None)
                if value is None:
                    raise InspectionRequired(f"missing value for {name}")
                if name in {"--file", "--exclude-from", "--ignore-file"}:
                    files.append(value)
                explicit_pattern |= name in {"--regexp", "--file"}
            elif name[2:] not in SEARCH_FLAGS or equals:
                raise InspectionRequired(f"unsupported search option {name}")
        elif options and word.startswith("-") and word != "-":
            for index, flag in enumerate(word[1:], 1):
                takes_value = f"-{flag}" in SEARCH_VALUES and not (program != "rg" and flag in "ErT")
                if flag in "ef" or takes_value:
                    value = word[index + 1:] or next(words, None)
                    if value is None:
                        raise InspectionRequired(f"missing value for -{flag}")
                    if flag == "f":
                        files.append(value)
                    explicit_pattern |= flag in "ef"
                    break
                if flag not in "abcEFGHhIiLlNnOoPqRrSsTUuvVwXxZz0":
                    raise InspectionRequired(f"unsupported search option -{flag}")
        else:
            (operands if options else trailing).append(word)
    if not (explicit_pattern or listing):
        patterns = operands or trailing
        if patterns:
            patterns.pop(0)
    return files, operands, trailing


# Lex words with their quotes intact so a literal pipe or dollar sign in a
# pattern is distinguishable from shell syntax. Never execute the input.
SHELL_WORD = re.compile(r"""(?:[^\s;&|<>()'"\\]+|\\[^\n]|'[^']*'|"(?:\\.|[^"\\])*")+""", re.S)
SHELL_OPERATOR = re.compile(r"2>>?|[;&|<>]+|\n")


def shell_commands(command):
    words, paths, redirect = [], [], False
    offset = 0
    while offset < len(command):
        char = command[offset]
        if char in " \t\r":
            offset += 1
            continue
        if char == "#":
            newline = command.find("\n", offset)
            offset = len(command) if newline < 0 else newline
            continue
        operator = SHELL_OPERATOR.match(command, offset)
        if operator:
            token = operator[0]
            offset = operator.end()
            if redirect:
                raise InspectionRequired("missing redirection path")
            if token in {"<", ">", ">>", "2>", "2>>"}:
                redirect = True
            elif token in {";", "\n", "&&", "||", "|"}:
                if words or paths:
                    yield words, paths
                words, paths = [], []
            else:
                raise InspectionRequired(f"unsupported shell operator {token}")
            continue
        match = SHELL_WORD.match(command, offset)
        if not match:
            raise InspectionRequired("unsupported shell syntax")
        for part in re.finditer(r"""'[^']*'|"(?:\\.|[^"\\])*"|\\.|[^'"\\]+""", match[0], re.S):
            value = part[0]
            if value.startswith(("'", "\\")):
                continue
            if re.search(r"\\\\|(?<!\\)[$`]", value) or (
                not value.startswith('"') and re.search(r"\{[^{}]*(?:,|\.\.)[^{}]*\}", value)
            ):
                raise InspectionRequired("possible shell expansion")
        word, = shlex.split(match[0])
        (paths if redirect else words).append(word)
        redirect = False
        offset = match.end()
    if redirect:
        raise InspectionRequired("missing redirection path")
    if words or paths:
        yield words, paths


MEMORY_TOOLS = {
    "search_sessions", "read_session", "search_session", "get_session_brief",
    "session_files", "trace_file", "blame_file",
}
WEB_TOOLS = {"web_search", "web.run", "websearch", "webfetch", "search_query"}
NETWORK = set("ssh scp sftp rsync nc ncat netcat socat telnet ftp".split())
READ_COMMANDS = set("""
    cat cd cut diff du file head ls nl paste pwd readlink realpath sort stat tail tee uniq wc
    which whoami basename dirname cmp column fold fmt tree
""".split())
GIT_HISTORY = set("fetch pull clone remote ls-remote bundle fsck reflog for-each-ref show-ref".split())
GIT_REVISIONS = set("diff log show blame rev-list rev-parse cat-file checkout switch grep".split())
GIT_FLAGS = set("""
    --stat --numstat --shortstat --summary --name-only --name-status --raw --patch
    --no-patch --oneline --no-decorate --decorate --no-color --color --no-ext-diff
    --no-textconv --binary --cached --staged --no-renames --find-renames --find-copies
    --ignore-space-at-eol --ignore-space-change --ignore-all-space --ignore-submodules --submodule
    --full-index --reverse --word-diff --exit-code --minimal --patience --histogram
    --first-parent --no-merges --merges --parents --quiet --verify --short --show-toplevel
    --show-prefix --show-object-format --is-inside-work-tree --path-format --absolute-git-dir
    --git-common-dir --git-dir --literal-pathspecs --relative --no-relative --porcelain
    --untracked-files --branch --detach --others --exclude-standard --full-name --stage --error-unmatch
    -p -s -q -r -R -v -V -M -C -a -b -w -i -m -c -t -e -u -z -l -1
""".split())
GIT_VALUE_OPTIONS = set("""
    -n -L -S -G -U --format --pretty --max-count --skip --output --unified --diff-filter
    --stat-width --stat-name-width --stat-count --abbrev --grep --author --committer --since --until
""".split())


def path_hits(path, own_workspace):
    for name in dict.fromkeys((path, posixpath.normpath(path))):
        if re.match(r"(?i)^https?://", name):
            yield "github-or-web", path
        if re.match(r"^/dev/(?:tcp|udp)/", name):
            yield "network-command", path
        if re.search(r"(^|/)(?:\.mj|memory|mj-memory)(?:/|$)|(?:^|/)\.local/share/mjolnir(?:/|$)|"
                     r"\.(?:db|sqlite3?)(?:-(?:wal|shm|journal))?$", name):
            yield "memory-or-sessions", path
        if re.search(r"^/run/hel/git-cache(?:/|$)|\.bundle$", name):
            yield "git-history-escape", path
        if name in {"/", "/workspace"} or re.search(
            r"^/(?:home/jonathan|mnt/optane|var/lib/hel|home/hel/\.(?:codex|claude|cache|config|local))(?:/|$)"
            r"|(?:^|/)\.codex/(?:sessions|history|archived)(?:/|$)", name
        ):
            yield "outside-workspace", path
        if match := re.match(r"^/workspace/([0-9a-f]{32})(?:/|$)", name):
            if match[1] != own_workspace:
                yield "other-workspace", path


def git_hits(args, own_workspace, commits, single_commit):
    while args[:1] == ["-C"] and len(args) > 1:
        yield from path_hits(args[1], own_workspace)
        args = args[2:]
    if not args:
        raise InspectionRequired("missing git subcommand")
    verb, *args = args
    if verb.startswith(("--git-dir", "--work-tree")):
        yield "git-revision-outside-pin", verb
        return
    if verb in GIT_HISTORY:
        yield "git-history-escape", verb
        return
    if verb not in GIT_REVISIONS | {"status", "ls-files"}:
        raise InspectionRequired(f"unsupported git subcommand {verb}")

    split = args.index("--") if "--" in args else len(args)
    before, paths = args[:split], args[split + 1:]
    if verb == "grep":
        # With no -e or -f, git grep also accepts its pattern after --.
        files, operands, paths = search_operands(verb, args)
        for path in files:
            yield from path_hits(path, own_workspace)
    else:
        operands = []
        words = iter(before)
        for word in words:
            name, equals, value = word.partition("=")
            if name in {"--all", "--branches", "--tags", "--remotes", "--reflog", "--batch-all-objects"}:
                yield "git-history-escape", word
            elif name in {"--no-index", "--git-dir", "--work-tree", "--ext-diff", "--textconv"}:
                yield "git-revision-outside-pin", word
            elif name in GIT_VALUE_OPTIONS:
                if not equals and not (value := next(words, None)):
                    raise InspectionRequired(f"missing value for git {name}")
                if name in {"--output", "-L"}:
                    yield from path_hits(value.rsplit(":", 1)[-1] if name == "-L" else value, own_workspace)
            elif not word.startswith("-"):
                operands.append(word)
            elif name not in GIT_FLAGS and not (re.fullmatch(r"-(?:[0-9]+|[nMU][0-9]+)", word)
                                                 or word.startswith(("-S", "-G"))):
                raise InspectionRequired(f"unsupported git option {name}")
    if verb in {"status", "ls-files"}:
        paths.extend(operands)
        operands = []
    elif verb == "blame" and not paths and operands:
        paths.append(operands.pop())
    for path in paths:
        yield from path_hits(path, own_workspace)
    detached = verb == "checkout" and "--detach" in before
    if detached and not operands:
        raise InspectionRequired("detached checkout without an explicit pin")
    for operand in operands:
        revision, colon, path = operand.partition(":")
        if colon:
            yield from path_hits(path, own_workspace)
        if not colon and verb in {"diff", "log", "show", "grep"} and (
            operand.startswith(("./", "../", "/", "~/"))
            or "/" in operand and not operand.startswith(("origin/", "upstream/", "refs/"))
            or re.search(r"\.[a-zA-Z][a-zA-Z0-9_-]*$", operand)
        ):
            yield from path_hits(operand, own_workspace)
            raise InspectionRequired("ambiguous git path or revision (use -- before paths)")
        for ref in re.split(r"\.{2,3}", revision):
            ref = ref.removeprefix("^")
            if not ref:
                continue  # An open end of a range defaults to HEAD.
            match = re.fullmatch(r"(HEAD|[0-9a-f]{7,40})((?:[~^][0-9]*|\^\{(?:commit|tree|object)?\})*)", ref)
            if (not match or match[1] != "HEAD" and not any(pin.startswith(match[1]) for pin in commits)
                    or (single_commit or detached) and re.search(r"[~^](?!0(?![0-9])|\{)", match[2])):
                yield "git-revision-outside-pin", operand


# sed substitutions are data unless their flags can run a command or write a
# file. Restrict recognition to a single substitution or numeric print ranges.
SED_PRINT = re.compile(r"\d+(?:,\d+)?p(?:;\s*\d+(?:,\d+)?p)*;?")
SED_SUBSTITUTE = re.compile(
    r"s(?P<delim>[/|#@])(?:\\[^\n]|(?![\\\n]|(?P=delim)).)*(?P=delim)"
    r"(?:\\[^\n]|(?![\\\n]|(?P=delim)).)*(?P=delim)[giIp0-9]*"
)


PYTHON = {"python", "python2", "python3"}
INLINE_OPTIONS = {"node": "-e", "ruby": "-e", "perl": "-e", **dict.fromkeys(PYTHON, "-c")}
PROCESS_CALLS = {
    "os.system", "os.popen", "subprocess.run", "subprocess.Popen", "subprocess.call",
    "subprocess.check_call", "subprocess.check_output", "subprocess.getoutput", "subprocess.getstatusoutput",
    "asyncio.create_subprocess_exec", "asyncio.create_subprocess_shell",
}
OPAQUE_CALLS = {"eval", "exec", "compile", "bytes.fromhex", "bytearray.fromhex", "codecs.decode",
                "marshal.loads", "pickle.loads", "zlib.decompress", "gzip.decompress"}
NETWORK_CALL = re.compile(
    r"^(?:requests\.|httpx\.|urllib\.request\.|urllib2\.|socket\.|http\.client\.|"
    r"ftplib\.|paramiko\.|aiohttp\.|websocket\.|fetch$|XMLHttpRequest$|"
    r"(?:http|https|net|axios)\.(?:request|get|post|put|connect|createConnection)$)"
)
QUOTED_SOURCE = r"""'(?:\\.|[^'\\])*'|"(?:\\.|[^"\\])*" """


def source_literal_hits(value, own_workspace):
    if not value or value == "/" or "\n" in value:
        return
    if re.fullmatch(r"(?i)(?:[\w-]+\.)*(?:github\.com|claude\.ai)(?::[0-9]+)?", value):
        yield "github-or-web", value
    if not any(char.isspace() for char in value) or value.startswith(("/", "~/", ".mj/")):
        yield from path_hits(value, own_workspace)


def python_name(expr, imports):
    if isinstance(expr, ast.Name):
        return imports.get(expr.id, expr.id)
    if isinstance(expr, ast.Attribute):
        return f"{python_name(expr.value, imports)}.{expr.attr}"
    return ""


def subprocess_hits(value, own_workspace, commits, single_commit):
    if isinstance(value, str):
        yield from shell_hits(value, own_workspace, commits, single_commit)
    elif isinstance(value, (tuple, list)) and value and all(isinstance(word, str) for word in value):
        yield from command_hits(list(value), own_workspace, commits, single_commit)
    else:
        yield "needs-inspection", "subprocess argv is not visible in interpreter source"


def interpreter_hits(program, source, own_workspace, commits, single_commit):
    # This screen sees source literals, not runtime values. A protected path
    # built dynamically (e.g. expanduser('~') plus computed pieces) can pass.
    if program in PYTHON:
        try:
            tree = ast.parse(source)
        except SyntaxError:
            yield "needs-inspection", "interpreter source could not be parsed"
            return
        nodes = list(ast.walk(tree))
        imports = {}
        for node in nodes:
            if isinstance(node, ast.Import):
                for alias in node.names:
                    imports[alias.asname or alias.name.split(".")[0]] = alias.name if alias.asname else alias.name.split(".")[0]
            elif isinstance(node, ast.ImportFrom) and node.module:
                for alias in node.names:
                    imports[alias.asname or alias.name] = f"{node.module}.{alias.name}"
        argv_nodes = {arg_node for node in nodes if isinstance(node, ast.Call)
                      and node.args and python_name(node.func, imports) in PROCESS_CALLS
                      for arg_node in ast.walk(node.args[0])}
        for node in nodes:
            if node not in argv_nodes and isinstance(node, ast.Constant) and isinstance(node.value, str):
                yield from source_literal_hits(node.value, own_workspace)
            if not isinstance(node, ast.Call):
                continue
            name = python_name(node.func, imports)
            if NETWORK_CALL.match(name):
                yield "network-command", name
            elif name in OPAQUE_CALLS or name.startswith("base64.") and "decode" in name:
                yield "needs-inspection", f"opaque interpreter operation {name}"
            elif name in PROCESS_CALLS:
                try:
                    argv = ast.literal_eval(node.args[0]) if node.args else None
                except (ValueError, TypeError, SyntaxError):
                    argv = None
                yield from subprocess_hits(argv, own_workspace, commits, single_commit)
    else:
        # Inspect literal strings and recognizable calls in inline non-Python
        # source. Interpolated code remains subject to the literal-only limit.
        for match in re.finditer(QUOTED_SOURCE.rstrip(), source):
            yield from source_literal_hits(match[0][1:-1], own_workspace)
        for match in re.finditer(r"\b([\w.]+)\s*\(", source):
            name = match[1]
            if NETWORK_CALL.match(name):
                yield "network-command", name
            elif name in OPAQUE_CALLS or name in {"Function", "atob", "Buffer.from"}:
                yield "needs-inspection", f"opaque interpreter operation {name}"
            elif name.rsplit(".", 1)[-1] in {
                "exec", "execSync", "spawn", "spawnSync", "execFile", "execFileSync", "system",
            }:
                literal = re.match(rf"\s*({QUOTED_SOURCE.rstrip()})", source[match.end():])
                if literal and name.rsplit(".", 1)[-1] in {"exec", "execSync", "system"}:
                    yield from subprocess_hits(literal[1][1:-1], own_workspace, commits, single_commit)
                else:
                    yield "needs-inspection", "subprocess argv is not visible in interpreter source"
    yield "interpreter-screened", program


def find_paths(args):
    selectors = {"-name", "-iname", "-path", "-ipath", "-regex", "-iregex", "-type",
                 "-maxdepth", "-mindepth", "-size", "-mtime"}
    expression = False
    words = iter(args)
    for word in words:
        if not expression and word in {"-H", "-L", "-P"}:
            continue
        if not expression and not word.startswith(("-", "!", "(")):
            yield word
            continue
        expression = True
        if word in selectors:
            if next(words, None) is None:
                raise InspectionRequired(f"missing value for find {word}")
        elif word not in {"-print", "-print0", "-prune", "-o", "-a", "-not", "!"}:
            raise InspectionRequired(f"unsupported find predicate {word}")


def command_hits(words, own_workspace, commits, single_commit):
    program, *args = words
    yield from path_hits(program, own_workspace)
    program = posixpath.basename(program)
    if program in NETWORK:
        yield "network-command", program
    elif program in {"gh", "curl", "wget"}:
        yield "github-or-web", program
    elif program == "git":
        yield from git_hits(args, own_workspace, commits, single_commit)
    elif program in {"mj", "sqlite", "sqlite3"} | MEMORY_TOOLS:
        yield "memory-or-sessions", program
    elif (program == "pytest" or program == "cargo" and
          next((a for a in args if not a.startswith(("-", "+"))), "") in
          {"build", "test", "check", "clippy", "run", "nextest", "bench"}
          or program == "npm" and args[:1] in (["run"], ["test"])
          or program == "go" and args[:1] == ["test"]
          or program == "make" and any(re.search(r"(test|check)$", a) for a in args if not a.startswith("-"))
          or program in {"python", "python3"} and args[:2] in (["-m", "pytest"], ["-m", "unittest"])
          or re.match(r"(?:\./)?(?:tests?|scripts)/", words[0])
          or program in {"python", "python3", "bash", "sh", "expect"}
          and args and re.match(r"(?:\./)?(?:tests?|e2e|scripts)/", args[0])):
        yield "build-or-test", shlex.join(words)
    elif program in {"bash", "sh"} and len(args) == 2 and args[0] in {"-c", "-lc"}:
        yield from shell_hits(args[1], own_workspace, commits, single_commit)
    elif program in INLINE_OPTIONS and len(args) >= 2 and args[0] == INLINE_OPTIONS[program]:
        for arg in args[2:]:
            yield from source_literal_hits(arg, own_workspace)
        yield from interpreter_hits(program, args[1], own_workspace, commits, single_commit)
    elif program in SEARCH:
        files, operands, trailing = search_operands(program, args)
        for path in files + operands + trailing:
            yield from path_hits(path, own_workspace)
    elif program == "find":
        for path in find_paths(args):
            yield from path_hits(path, own_workspace)
    elif program == "sed":
        while args and args[0] in {"-n", "-i", "-e", "--in-place", "--"}:
            args = args[1:]
        if not args or not (SED_PRINT.fullmatch(args[0]) or SED_SUBSTITUTE.fullmatch(args[0])):
            raise InspectionRequired("unsupported sed script")
        for path in args[1:]:
            if path.startswith("-") and path != "-":
                raise InspectionRequired("additional sed options after the script")
            yield from path_hits(path, own_workspace)
    elif program in {"echo", "printf", "true", "false", "tr"}:
        return  # These operands are data. Redirection targets are checked separately.
    elif program in READ_COMMANDS:
        if program == "sort" and any(arg.startswith("--compress") for arg in args):
            raise InspectionRequired("sort may start an external program")
        for arg in args:
            yield from path_hits(arg.split("=", 1)[-1] if arg.startswith("-") else arg, own_workspace)
    else:
        raise InspectionRequired(f"unsupported command {program}")


def shell_hits(command, own_workspace, commits, single_commit):
    # A quoted heredoc contains literal input, ending at the first delimiter.
    heredoc = re.match(r"([^\n]+)<<\s*'([A-Za-z_]\w*)'[^\S\n]*\n(.*?)^\2(?:\n|$)",
                       command, re.S | re.M)
    try:
        if heredoc:
            prefix = list(shell_commands(heredoc[1]))
            if len(prefix) == 1:
                words, paths = prefix[0]
                suffix = command[heredoc.end():]
                if words == ["cat"] and paths:
                    command = heredoc[1] + "\n" + suffix
                elif words and posixpath.basename(words[0]) in INLINE_OPTIONS and words[1:2] in ([], ["-"]):
                    yield from path_hits(words[0], own_workspace)
                    for path in paths:
                        yield from path_hits(path, own_workspace)
                    for arg in words[2:]:
                        yield from source_literal_hits(arg, own_workspace)
                    yield from interpreter_hits(posixpath.basename(words[0]), heredoc[3],
                                                own_workspace, commits, single_commit)
                    yield from shell_hits(suffix, own_workspace, commits, single_commit)
                    return
        for words, paths in shell_commands(command):
            for path in paths:
                yield from path_hits(path, own_workspace)
            if words:
                try:
                    yield from command_hits(words, own_workspace, commits, single_commit)
                except InspectionRequired as error:
                    yield "needs-inspection", f"{error}: {shlex.join(words)[:400]}"
    except ValueError as error:
        yield "needs-inspection", f"{error}: {command[:400]}"


PATH_KEYS = {"cwd", "path", "paths", "file_path", "filePath", "directory", "workdir", "target_file"}


def pilot_hits(call, own_workspace, commits, single_commit):
    title = call.get("title") or ""
    kind = call.get("kind")
    raw = call.get("rawInput")
    tool = title.lower().split("__")[-1]
    if tool in WEB_TOOLS or title.lower().startswith("mcp__web__"):
        yield "github-or-web", title
        return
    if tool in MEMORY_TOOLS or title.startswith("mcp__mj_memory__"):
        yield "memory-or-sessions", title
        return

    for fields in [raw, *(call.get("locations") or [])]:
        if isinstance(fields, dict):
            for key in PATH_KEYS & fields.keys():
                paths = fields[key] if isinstance(fields[key], list) else [fields[key]]
                for path in paths:
                    if isinstance(path, str):
                        yield from path_hits(path, own_workspace)

    if kind == "read" and not raw and not call.get("locations"):
        if match := re.fullmatch(r"Read file ['\"](.*)['\"]", title):
            yield from path_hits(match[1], own_workspace)
        else:
            yield "needs-inspection", f"missing read path: {title[:400]}"

    if tool == "apply_patch":
        patch = raw.get("patch") if isinstance(raw, dict) else raw
        if isinstance(patch, str) and patch.startswith("*** Begin Patch\n") and patch.rstrip().endswith("*** End Patch"):
            paths = re.findall(r"^\*\*\* (?:Add File|Update File|Delete File|Move to): (.+)$", patch, re.M)
            if paths:
                for path in paths:
                    yield from path_hits(path, own_workspace)
                return
        yield "needs-inspection", "unrecognized patch format"
        return

    command = raw.get("command", raw.get("cmd", raw.get("argv"))) if isinstance(raw, dict) else raw
    if command is None and kind in {None, "execute"} and title and raw is None:
        # Some bridges record the command only as their display title.
        command = title
        if title.startswith('"') and title.endswith('"'):
            try:
                command = json.loads(title)
            except ValueError:
                pass
    if isinstance(command, list) and command and all(isinstance(word, str) for word in command):
        command = shlex.join(command)
    if isinstance(command, str) and command.strip() and kind in {None, "execute", "other"}:
        yield from shell_hits(command, own_workspace, commits, single_commit)
    elif kind == "execute" or title in {"rql_query", "mcp__rql__rql_query"}:
        yield "needs-inspection", f"missing or unsupported command input: {title[:400]}"


def audit_pilot(transcript_path, own_workspace, *, commits=(), allow_rql=False, single_commit=False):
    """Screen pilot inputs; a needs-inspection hit prevents automatic acceptance."""
    try:
        pages = json.loads(transcript_path.read_text())
        if not any(page["items"] for page in pages):
            raise ValueError("empty transcript")
        result = scan(pages, own_workspace, commits=commits, allow_rql=allow_rql, single_commit=single_commit)
    except (OSError, ValueError, KeyError, TypeError) as error:
        result = {"tool_items": 0, "hits": [{"kind": "missing-or-unreadable-transcript", "snippet": str(error)}]}
    return dict(result, passed=all(hit["kind"] == "interpreter-screened" for hit in result["hits"]))


def audit_run(run):
    placement = json.loads((run / "placement.json").read_text())
    own = placement["cwd"].split("/")[2]
    result = {"run": str(run.relative_to(ROOT)), "reviewers": {}}
    for transcript in sorted(run.glob("reviewers/*/transcript.json")):
        result["reviewers"][transcript.parent.name] = audit_reviewer(transcript, own)
    # Since mj 8fe3c40bd a reviewer session is destroyed when its role ends, so driver.py keeps
    # only its stored handback and no transcript: its tool calls cannot be audited. Say so; an
    # empty reviewer list must not read as a clean audit.
    listed = [r["child"][:12] for r in json.loads((run / "done.json").read_text())["reviewers"]]
    result["unaudited"] = [c for c in listed if c not in result["reviewers"]]
    check = json.loads((run / "inventory-check.json").read_text())
    result["inventory_problems"] = check["problems"]
    (run / "audit.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main():
    runs = [(ROOT / a).resolve() for a in sys.argv[1:]] or sorted(p.parent for p in ROOT.glob("runs/*/*/done.json"))
    failed = False
    for run in runs:
        r = audit_run(run)
        counts = {k: {} for k in r["reviewers"]}
        for rid, rv in r["reviewers"].items():
            for h in rv["hits"]:
                counts[rid][h["kind"]] = counts[rid].get(h["kind"], 0) + 1
        unaudited = f" UNAUDITED (no transcript): {r['unaudited']}" if r["unaudited"] else ""
        print(r["run"], json.dumps(counts), "inventory:", r["inventory_problems"] or "clean", unaudited)
        failed = failed or bool(r["unaudited"])
    if failed:
        sys.exit("some reviewers have no transcript to audit; their tool calls were not checked")


if __name__ == "__main__":
    main()
