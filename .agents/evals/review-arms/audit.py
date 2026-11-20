"""Scans reviewer transcripts for access to later history or other forbidden sources.

Usage: python3 audit.py [runs/<bug>/<arm> ...]   (default: every finished run)
Writes audit.json in each run directory and prints a summary. A hit is not a
verdict: every hit is read by hand before a run is accepted or excluded.
"""
import ast
import itertools
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


def scan(pages, own_workspace, *, commits=None, allow_rql=False, single_commit=False, cwd=None, home=None):
    hits, calls = [], 0
    scripts = {}
    for page in pages:
        execution = page.get("execution")
        execution = execution if isinstance(execution, dict) else {}
        page_cwd = page.get("cwd") or execution.get("cwd") or cwd
        page_home = page.get("home") or execution.get("home") or home
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
                for name, snippet in dict(pilot_hits(call, own_workspace, commits, single_commit, scripts=scripts,
                                                    cwd=page_cwd, home=page_home)).items():
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
SHELL_PART = re.compile(r"""'[^']*'|"(?:\\.|[^"\\])*"|\\.|[^'"\\]+""", re.S)
SHELL_OPERATOR = re.compile(r"2>>?|<<-(?![<>&|])|[;&|<>]+|\n")
SHELL_ASSIGNMENT = re.compile(r"([a-zA-Z_][a-zA-Z_0-9]*)=")
SHELL_EXPANSION = re.compile(r"""\\[\\$`"]|\$\{([a-zA-Z_]\w*)\}|\$([a-zA-Z_]\w*)|[$`]""")


def shell_word(raw, variables, *, assignment=False):
    def replace(match):
        if match[0].startswith("\\"):
            return match[0][1:]
        name = match[1] or match[2]
        if name is None:
            raise InspectionRequired("unsupported shell expansion")
        if name not in variables:
            raise InspectionRequired(f"unresolved shell variable {name}")
        value = variables[name]
        if not quoted and not assignment and (not value or re.search(r"\s|[*?\[]", value)):
            raise InspectionRequired(f"unquoted shell variable {name} may split or glob")
        return value

    pieces = []
    for match in SHELL_PART.finditer(raw):
        part = match[0]
        if part.startswith("'"):
            pieces.append(part[1:-1])
        elif part.startswith("\\"):
            pieces.append(part[1:])
        else:
            quoted = part.startswith('"')
            if not quoted and re.search(r"\{[^{}]*(?:,|\.\.)[^{}]*\}", part):
                raise InspectionRequired("unsupported shell brace expansion")
            pieces.append(SHELL_EXPANSION.sub(replace, part[1:-1] if quoted else part))
    return "".join(pieces)


def heredoc_body(command, offset, delimiter):
    name, quoted, strip_tabs = delimiter
    lines = []
    while offset < len(command):
        line, _, _ = command[offset:].partition("\n")
        offset += len(line) + 1
        if strip_tabs:
            line = line.lstrip("\t")
        if line == name:
            source = "\n".join(lines)
            if not quoted and re.search(r"[$`]|\\", source):
                raise InspectionRequired("unquoted heredoc may expand")
            return source, offset
        lines.append(line)
    raise InspectionRequired("missing heredoc delimiter")


def shell_commands(command, cwd, home):
    words, paths, pending = [], [], []
    redirect = delimiter = None
    variables = {}
    if cwd:
        variables["PWD"] = cwd
    if home:
        variables["HOME"] = home
    preceding = None
    offset = 0
    while True:
        if offset >= len(command):
            token = "\n"
        else:
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
            else:
                match = SHELL_WORD.match(command, offset)
                if not match:
                    raise InspectionRequired("unsupported shell syntax")
                raw = match[0]
                offset = match.end()
                if redirect in {"<<", "<<-"}:
                    name, = shlex.split(raw)
                    if not re.fullmatch(r"[A-Za-z_][A-Za-z_0-9]*", name):
                        raise InspectionRequired("unsupported heredoc delimiter")
                    delimiter = name, raw != name, redirect == "<<-"
                elif redirect:
                    paths.append((redirect, raw))
                else:
                    words.append(raw)
                redirect = None
                continue
        if redirect:
            raise InspectionRequired("missing redirection operand")
        if token in {"<", ">", ">>", "2>", "2>>", "<<", "<<-"}:
            if token.startswith("<<") and delimiter:
                raise InspectionRequired("multiple heredocs in a command")
            redirect = token
            continue
        if token not in {";", "\n", "&&", "||", "|"}:
            raise InspectionRequired(f"unsupported shell operator {token}")
        has_command = bool(words or paths or delimiter)
        if has_command:
            bound = [SHELL_ASSIGNMENT.match(word) for word in words]
            targets = [(mode, shell_word(raw, variables)) for mode, raw in paths]
            if words and all(bound):
                for raw, binding in zip(words, bound):
                    name = binding[1]
                    if name in {"PATH", "IFS", "ENV", "CDPATH", "GLOBIGNORE", "SHELLOPTS"} or name.startswith(
                        ("BASH", "GIT_", "LD_", "NODE_", "PYTHON", "RIPGREP_")
                    ):
                        raise InspectionRequired(f"assignment may change command behavior: {name}")
                    if not paths and not delimiter and preceding not in {"&&", "||", "|"} and token != "|":
                        variables[name] = shell_word(raw[binding.end():], variables, assignment=True)
                    else:
                        variables.pop(name, None)
                argv = []
            else:
                argv = [shell_word(raw, variables) for raw in words]
            if delimiter and any(mode == "<" for mode, _ in paths):
                raise InspectionRequired("heredoc combined with another stdin redirection")
            if pending or delimiter:
                pending.append((argv, targets, delimiter, cwd))
            else:
                yield argv, targets, None, cwd
            if argv[:1] == ["cd"]:
                target = argv[1] if len(argv) == 2 and not argv[1].startswith("-") else None
                # The right side of && only runs after a successful cd.
                if (target and token == "&&" and preceding not in {"&&", "||", "|"}
                        and (cwd or target.startswith("/"))):
                    cwd = posixpath.normpath(posixpath.join(cwd or "", target))
                    variables["PWD"] = cwd
                else:
                    cwd = None
                    variables.pop("PWD", None)
        words, paths, delimiter = [], [], None
        # A blank line does not erase a continuation after &&, ||, or |.
        if has_command or token != "\n":
            preceding = token
        if token == "\n":
            for argv, targets, marker, start_dir in pending:
                source, offset = heredoc_body(command, offset, marker) if marker else (None, offset)
                yield argv, targets, source, start_dir
            pending.clear()
            if offset >= len(command):
                return


MEMORY_TOOLS = {
    "search_sessions", "read_session", "search_session", "get_session_brief",
    "session_files", "trace_file", "blame_file",
}
WEB_TOOLS = {"web_search", "web.run", "websearch", "webfetch", "search_query"}
NETWORK = set("ssh scp sftp rsync nc ncat netcat socat telnet ftp".split())
READ_COMMANDS = set("""
    cat cd cut diff du file head ls nl paste pwd readlink realpath sort stat tail tee uniq wc
    which whoami basename dirname cmp column fold fmt tree mkdir chmod cp mv rm
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


AWK = {"awk", "gawk", "mawk"}
FILTER_PROGRAM_FILES = {"-f", "--file"}
FILTER_PROGRAM_TEXT = {"-e", "--expression", "--source", "-ne", "-pe"}
FILTER_VALUE_OPTIONS = {"-F", "--field-separator", "-v", "--assign"}
FILTER_FLAGS = {"-n", "-E", "-r", "-i", "--quiet", "--silent", "--regexp-extended", "--in-place"}
FILTER_TOKEN = re.compile(
    r"""(?P<quote>"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`)"""
    r"|(?P<comment>#[^\n]*)|\w+|\|[|&]|&&|>>|[!<>=]=|!~|[^ \t\r]", re.S
)
FILTER_REGEX = re.compile(r"/(?:\\[^\n]|\[(?:\\.|[^\]\\\n])*\]|[^/\\\n])*/")
REGEX_PRECEDERS = {"", "\n", ";", "{", "}", "(", "[", "=", "!", "~", "!~", "&&", "||", ",", "?", ":", "if"}


def filter_arguments(program, args):
    sources, paths, source_paths = [], [], []
    options = True
    operands = iter(args)
    for arg in operands:
        if options and arg == "--":
            options = False
            continue
        if options and arg.startswith("-") and arg != "-":
            option, equals, value = arg.partition("=")
            if option in FILTER_PROGRAM_FILES | FILTER_PROGRAM_TEXT or program in AWK and option in FILTER_VALUE_OPTIONS:
                value = value if equals else next(operands, None)
                if value is None:
                    raise InspectionRequired(f"missing value for {program} {option}")
                if option in FILTER_PROGRAM_FILES:
                    source_paths.append(value)
                elif option in FILTER_PROGRAM_TEXT:
                    sources.append(value)
            elif arg.startswith(("-e", "-ne", "-pe")) and len(arg) > 2:
                prefix = "-e" if arg.startswith("-e") else arg[:3]
                if len(arg) == len(prefix):
                    raise InspectionRequired(f"missing value for {program} {prefix}")
                sources.append(arg[len(prefix):])
            elif arg.startswith("-f") and len(arg) > 2:
                source_paths.append(arg[2:])
            elif program in AWK and arg.startswith(("-F", "-v")) and len(arg) > 2:
                continue
            elif program == "sed" and (option in FILTER_FLAGS or re.fullmatch(r"-[niEr]+", option)
                                      or arg.startswith("-i.")):
                continue
            else:
                raise InspectionRequired(f"unsupported {program} option {option}")
        elif not (sources or source_paths) and program != "perl":
            sources.append(arg)
        else:
            paths.append(arg)
    if not (sources or source_paths):
        raise InspectionRequired(f"missing {program} program")
    return sources, source_paths, paths


def filter_tokens(source, *, perl=False):
    if perl and re.search(r"\(\?\??\{", source):
        raise InspectionRequired("perl regular expression can execute code")
    tokens = []
    offset = 0
    while offset < len(source):
        if source[offset] == "/" and (not tokens or tokens[-1] in REGEX_PRECEDERS):
            regex = FILTER_REGEX.match(source, offset)
            if regex:
                tokens.append(regex[0])
                offset = regex.end()
                continue
        if perl and (not tokens or tokens[-1] not in {"$", "@", "%"}) and (
            operator := re.match(r"(tr|qq|qw|qx|qr|[mysq])(?=[^\w\s])", source[offset:])
        ):
            name = operator[1]
            field_start = offset + operator.end()
            if name == "qx" or source[field_start] != "/":
                raise InspectionRequired(f"unsupported perl quote operator {name}")
            tail = sed_field(source[field_start + 1:], "/")
            if name in {"s", "y", "tr"}:
                tail = sed_field(tail, "/")
            flags = re.match(r"[a-zA-Z]*", tail)[0]
            if name == "s" and "e" in flags:
                raise InspectionRequired("perl substitution can execute code")
            tokens.append("/")  # Pattern and replacement text are data.
            offset = len(source) - len(tail) + len(flags)
            continue
        match = FILTER_TOKEN.match(source, offset)
        if match:
            if not match["comment"]:
                tokens.append(match[0])
            offset = match.end()
        else:
            offset += 1
    return tokens


def filter_literal(token, *, perl=False):
    if len(token) < 2 or token[0] not in {'"', "'"} or token[-1] != token[0]:
        return None
    value = token[1:-1]
    if "\\" in value or perl and token[0] == '"' and re.search(r"[$@]", value):
        return None
    return value


def filter_effect(kind, argument, program):
    if argument is None:
        raise InspectionRequired(f"computed {program} {kind} operand")
    return kind, argument


def awk_effects(source):
    tokens = filter_tokens(source)
    for index, token in enumerate(tokens):
        if token not in {"@", "ARGV", "system", "|", "|&", "print", "printf", "getline"}:
            continue
        after = tokens[index + 1:]
        before = tokens[index - 1] if index else ""
        if token == "@" or token == "ARGV":
            raise InspectionRequired("awk can load a program or change its input files")
        if token == "system":
            literal = filter_literal(after[1]) if len(after) >= 3 and after[0] == "(" and after[2] == ")" else None
            yield filter_effect("command", literal, "awk system")
        elif token in {"|", "|&"}:
            if after[:1] == ["getline"]:
                literal = filter_literal(before)
                if index >= 2 and tokens[index - 2] not in {"{", ";", "\n", "}", "("}:
                    literal = None
            else:
                literal = filter_literal(after[0]) if after and after[1:2] in ([], [";"], ["\n"], ["}"]) else None
            yield filter_effect("command", literal, "awk pipe")
        elif token in {"print", "printf", "getline"}:
            depth = 0
            operators = {"<"} if token == "getline" else {">", ">>"}
            for offset, operand in enumerate(after):
                if depth == 0 and operand in {";", "\n", "}"}:
                    break
                if depth == 0 and operand in operators:
                    tail = after[offset + 1:]
                    terminators = {";", "\n", "}"}
                    if token == "getline":
                        terminators.add(")")
                    literal = filter_literal(tail[0]) if tail and (len(tail) == 1 or tail[1] in terminators) else None
                    yield filter_effect("read" if token == "getline" else "write", literal, f"awk {token}")
                    break
                depth += (operand in {"(", "["}) - (operand in {")", "]"})


def sed_field(source, delimiter):
    field = re.match(rf"(?:\\[\s\S]|[^{re.escape(delimiter)}\\\n])*{re.escape(delimiter)}", source)
    if field is None:
        raise InspectionRequired("unterminated sed expression")
    return source[field.end():]


def sed_effects(source):
    address = r"(?:[+~]?\d+(?:~\d+)?|\$)"
    while source := source.lstrip(" \t\r\n;"):
        if source.startswith("#"):
            source = source.partition("\n")[2]
            continue
        # Strip addresses before examining the command letter. Escaped regex
        # delimiters are consumed by the same scanner as substitutions.
        for _ in range(2):
            if source.startswith("/"):
                source = sed_field(source[1:], "/")
            elif match := re.match(address, source):
                source = source[match.end():]
            else:
                break
            source = source.lstrip(" \t")
            if not source.startswith(","):
                break
            source = source[1:].lstrip(" \t")
        source = source.lstrip("! \t")
        if not source:
            raise InspectionRequired("sed address missing its command")
        command, source = source[0], source[1:]
        if command in "{}":
            continue
        if command in "sy":
            if not source or source[0] in "\\\n":
                raise InspectionRequired("unsupported sed delimiter")
            delimiter, source = source[0], source[1:]
            source = sed_field(sed_field(source, delimiter), delimiter)
            flags = re.match(r"[gipImM0-9 \t]*", source)[0] if command == "s" else ""
            source = source[len(flags):]
            if command == "s" and source.startswith("e"):
                raise InspectionRequired("sed replacement computes a command")
            if command == "s" and source.startswith("w"):
                path, _, source = source[1:].partition("\n")
                yield filter_effect("write", path.strip() or None, "sed write")
            elif source and source[0] not in ";\n} \t\r":
                raise InspectionRequired("unsupported sed substitution flags")
        elif command in "eRrWw":
            argument, _, source = source.partition("\n")
            effect = "command" if command == "e" else "read" if command in "rR" else "write"
            yield filter_effect(effect, argument.strip() or None, f"sed {command}")
        elif command in "acibBtT:":
            # Text arguments and labels occupy the rest of this command.
            if command in "aci" and re.search(r"\\\n", source):
                raise InspectionRequired("multiline sed text command")
            separator = r"\n" if command in "aci" else r"[;\n]"
            if end := re.search(separator, source):
                source = source[end.end():]
            else:
                source = ""
        elif command in "dDpPxgGhHnNl=zqQ":
            if command in "lqQ":
                source = re.sub(r"^[ \t]*\d+", "", source)
            if source and source.lstrip(" \t")[:1] not in {"", ";", "\n", "}"}:
                raise InspectionRequired(f"unsupported arguments to sed {command}")
        else:
            raise InspectionRequired(f"unsupported sed command {command}")


def perl_open_operand(value):
    value = value.strip()
    if value.startswith("|") or value.endswith("|"):
        return "command", value.strip("|").strip()
    mode = re.match(r"^(?:[+<>]+)\s*", value)
    if mode:
        return ("write" if ">" in mode[0] or "+" in mode[0] else "read"), value[mode.end():]
    return "read", value


def perl_effects(source):
    tokens = filter_tokens(source, perl=True)
    for index, token in enumerate(tokens):
        if token.startswith("`"):
            command = token[1:-1]
            yield filter_effect("command", command if not re.search(r"[$@\\]", command) else None, "perl backticks")
        elif token in {"system", "exec", "open", "sysopen"}:
            after = tokens[index + 1:]
            if after[:1] == ["("]:
                after = after[1:]
            args = []
            while after and after[0] not in {")", ";", "\n", "}"}:
                args.append(after.pop(0))
            if token in {"system", "exec"}:
                parts = [filter_literal(arg, perl=True) for arg in args if arg != ","]
                if not parts or any(part is None for part in parts):
                    raise InspectionRequired(f"computed perl {token} operand")
                yield "command", parts if len(parts) > 1 else parts[0]
                continue
            # The handle may be a variable or bareword. Every later operand
            # must be a literal to determine a filename or pipe command.
            if "," not in args:
                raise InspectionRequired(f"implicit perl {token} operand")
            args = args[args.index(",") + 1:]
            parts = [filter_literal(arg, perl=True) for arg in args if arg != ","]
            if any(part is None for part in parts[:2 if token == "open" else 1]) or not parts:
                raise InspectionRequired(f"computed perl {token} operand")
            if len(parts) == 1 and token == "open":
                yield perl_open_operand(parts[0])
            elif token == "open" and "|" in parts[0]:
                if any(part is None for part in parts[1:]):
                    raise InspectionRequired("computed perl open command")
                yield "command", parts[1:] if len(parts) > 2 else parts[1]
            elif token == "open":
                yield ("read" if parts[0] == "<" else "write"), parts[1]
            else:
                yield "write", parts[0]  # sysopen carries numeric or symbolic flags.
        elif token in {"eval", "qx", "require", "do", "readpipe", "glob", "ARGV",
                       "unlink", "rename", "link", "symlink", "chdir", "opendir", "truncate"}:
            raise InspectionRequired(f"unsupported perl {token} operation")


def text_filter_hits(program, args, own_workspace, commits, single_commit, scripts, cwd):
    sources, program_files, paths = filter_arguments(program, args)
    for path in program_files:
        yield from path_hits(path, own_workspace)
    effects = awk_effects if program in AWK else sed_effects if program == "sed" else perl_effects
    operations = (effect for source in sources for effect in effects(source))
    # Perl -n/-p uses the diamond operator. Its argv accepts the same pipe
    # and mode syntax as two-argument open.
    for kind, argument in itertools.chain(
        (perl_open_operand(path) if program == "perl" else ("read", path)
         for path in paths if program not in AWK or not re.fullmatch(r"[a-zA-Z_]\w*=.*", path, re.S)),
        operations,
    ):
        if kind == "command":
            if not argument:
                raise InspectionRequired(f"empty {program} shell command")
            if scripts is not None:
                scripts.clear()
            yield from subprocess_hits(argument, own_workspace, commits, single_commit)
        else:
            yield from path_hits(argument, own_workspace)
            if kind == "write" and scripts is not None:
                forget_script(scripts, argument, cwd)
    if program_files:
        raise InspectionRequired(f"{program} program file contents are opaque")


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


def command_hits(words, own_workspace, commits, single_commit, *, scripts=None, cwd="", home=None,
                 running_scripts=frozenset()):
    program, *args = words
    yield from path_hits(program, own_workspace)
    program = posixpath.basename(program)
    inline = interpreter_arguments(program, args)
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
        yield from shell_hits(args[1], own_workspace, commits, single_commit, scripts=scripts, cwd=cwd, home=home,
                              running_scripts=running_scripts)
    elif program in AWK | {"sed"} or program == "perl" and args and args[0].startswith(("-ne", "-pe")):
        yield from text_filter_hits(program, args, own_workspace, commits, single_commit, scripts, cwd)
    elif program in INLINE_OPTIONS and len(inline) >= 2 and inline[0] == INLINE_OPTIONS[program]:
        for arg in inline[2:]:
            yield from source_literal_hits(arg, own_workspace)
        yield from interpreter_hits(program, inline[1], own_workspace, commits, single_commit)
        if scripts is not None:
            scripts.clear()  # A program can modify files through computed paths.
    elif program in SEARCH:
        files, operands, trailing = search_operands(program, args)
        for path in files + operands + trailing:
            yield from path_hits(path, own_workspace)
    elif program == "find":
        for path in find_paths(args):
            yield from path_hits(path, own_workspace)
    elif program in {"echo", "printf", "true", "false", "tr"}:
        if program == "printf" and args and args[0].startswith("-v"):
            raise InspectionRequired("printf may assign a shell variable")
        return  # These operands are data. Redirection targets are checked separately.
    elif program in READ_COMMANDS:
        if program == "sort" and any(arg.startswith("--compress") for arg in args):
            raise InspectionRequired("sort may start an external program")
        for arg in args:
            yield from path_hits(arg.split("=", 1)[-1] if arg.startswith("-") else arg, own_workspace)
    else:
        raise InspectionRequired(f"unsupported command {program}")


def interpreter_arguments(program, args):
    if program in PYTHON:
        while args and args[0] in {"-I", "-B", "-E", "-s", "-S", "-u"}:
            args = args[1:]
    return args


def script_key(path, cwd):
    # An empty directory is the session's unnamed initial directory. None
    # means a cd may have changed it to an unknown location.
    if cwd is None and not posixpath.isabs(path):
        return None
    return posixpath.normpath(posixpath.join(cwd or "", path))


def forget_script(scripts, path, cwd):
    key = script_key(path, cwd)
    scripts.pop(key, None)
    if key is None:
        scripts.clear()
    elif not cwd and not posixpath.isabs(path):
        # The transcript may omit the directory for one of two equivalent paths.
        for previous in list(scripts):
            if previous.endswith("/" + key):
                scripts.pop(previous)


def literal_printf(args):
    if not args or args[0].startswith("-"):
        return None
    fmt, *values = args
    if fmt in {"%s", r"%s\n"}:
        return ("\n" if fmt.endswith(r"\n") else "").join(values) + ("\n" if fmt.endswith(r"\n") else "")
    if "%" not in fmt:
        return re.sub(r"\\([nrt\\])", lambda match: {"n": "\n", "r": "\r", "t": "\t", "\\": "\\"}[match[1]],
                      fmt) if not re.search(r"\\(?![nrt\\])", fmt) else None
    return None


def record_script_write(scripts, words, redirections, source, cwd):
    def record(path, text):
        if (key := script_key(path, cwd)) is not None:
            scripts[key] = text

    program = posixpath.basename(words[0]) if words else ""
    for mode, path in redirections:
        if mode != "<":
            forget_script(scripts, path, cwd)
    if program in {"cp", "mv", "rm"} or program == "git" and words[1:2] in (["checkout"], ["switch"]):
        scripts.clear()  # A destination can name a directory or be a symlink.
    elif program == "sed" or program == "sort":
        # Known in-place options, and any options whose destination form is opaque.
        if any(word.startswith(("-i", "--in-place", "-o", "--output")) or re.fullmatch(r"-[nEr]+i[niEr]*", word)
               for word in words[1:]):
            scripts.clear()
    elif program == "tee":
        for path in words[1:]:
            if not path.startswith("-"):
                forget_script(scripts, path, cwd)
    stdout = [(mode, path) for mode, path in redirections if mode in {">", ">>"}]
    target = stdout[0][1] if len(stdout) == 1 and stdout[0][0] == ">" else None
    if source is not None and words == ["cat"] and target is not None:
        record(target, source)
    elif source is not None and program == "tee" and all(not path.startswith("-") for path in words[1:]):
        for path in words[1:] + ([target] if target else []):
            record(path, source)
    elif program == "printf" and target is not None:
        text = literal_printf(words[1:])
        if text is not None:
            record(target, text)


def recorded_script(words, scripts, cwd):
    if not words:
        return None
    path, *args = words
    program = posixpath.basename(path)
    args = interpreter_arguments(program, args)
    if program in INLINE_OPTIONS or program in {"sh", "bash"}:
        if not args or args[0].startswith("-"):
            return None
        path, *args = args
    elif path.startswith(("./", "/", "../")) and (source := scripts.get(script_key(path, cwd))) is not None:
        shebang = source.splitlines()[0][2:] if source.startswith("#!") else "/bin/sh"
        argv = shlex.split(shebang)
        if argv[:1] == ["/usr/bin/env"]:
            argv = argv[1:]
        program = posixpath.basename(argv[0]) if argv else ""
        if program not in INLINE_OPTIONS and program not in {"sh", "bash"}:
            raise InspectionRequired("unrecognized recorded script interpreter")
        if interpreter_arguments(program, argv[1:]):
            raise InspectionRequired("unsupported recorded script interpreter options")
    else:
        return None
    source = scripts.get(script_key(path, cwd))
    if source is None:
        return None
    if re.match(r"(?:\./)?(?:tests?|e2e|scripts)/", path):
        return None  # Keep the build and test command screen in force.
    return program, path, args, source


def shell_hits(command, own_workspace, commits, single_commit, *, scripts=None, cwd="", home=None,
               running_scripts=frozenset()):
    if scripts is None:
        scripts = {}
    try:
        for words, redirections, source, start_dir in shell_commands(command, cwd, home):
            for _, path in redirections:
                yield from path_hits(path, own_workspace)
                if start_dir and not posixpath.isabs(path):
                    yield from path_hits(script_key(path, start_dir), own_workspace)
            if not words and source is None:
                record_script_write(scripts, words, redirections, source, start_dir)
                continue
            try:
                if source is None:
                    script = recorded_script(words, scripts, start_dir)
                    record_script_write(scripts, words, redirections, source, start_dir)
                    if script is None:
                        yield from command_hits(words, own_workspace, commits, single_commit,
                                                scripts=scripts, cwd=start_dir, home=home,
                                                running_scripts=running_scripts)
                    else:
                        program, path, arguments, script_source = script
                        yield from path_hits(path, own_workspace)
                        for arg in arguments:
                            yield from source_literal_hits(arg, own_workspace)
                        if program in {"sh", "bash"}:
                            key = script_key(path, start_dir)
                            if key in running_scripts:
                                raise InspectionRequired("recursive recorded script")
                            yield from shell_hits(script_source, own_workspace, commits, single_commit,
                                                  scripts=scripts, cwd=start_dir, home=home,
                                                  running_scripts=running_scripts | {key})
                            yield "interpreter-screened", program
                        else:
                            yield from interpreter_hits(program, script_source, own_workspace, commits, single_commit)
                            scripts.clear()
                else:
                    record_script_write(scripts, words, redirections, source, start_dir)
                    if words == ["cat"]:
                        continue  # Here the body is literal data.
                    if words and posixpath.basename(words[0]) == "tee":
                        yield from command_hits(words, own_workspace, commits, single_commit)
                        continue
                    program = posixpath.basename(words[0]) if words else ""
                    arguments = interpreter_arguments(program, words[1:])
                    if program not in INLINE_OPTIONS or arguments and arguments[0] != "-":
                        raise InspectionRequired("unsupported command receiving a heredoc")
                    yield from path_hits(words[0], own_workspace)
                    for arg in arguments[1:]:
                        yield from source_literal_hits(arg, own_workspace)
                    yield from interpreter_hits(program, source, own_workspace, commits, single_commit)
                    scripts.clear()
            except InspectionRequired as error:
                scripts.clear()
                yield "needs-inspection", f"{error}: {shlex.join(words)[:400]}"
    except ValueError as error:
        scripts.clear()
        yield "needs-inspection", f"{error}: {command[:400]}"


PATH_KEYS = {"cwd", "path", "paths", "file_path", "filePath", "directory", "workdir", "target_file"}


def pilot_hits(call, own_workspace, commits, single_commit, *, scripts, cwd, home):
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

    if isinstance(raw, dict):
        cwd = raw.get("cwd") or raw.get("workdir") or cwd
    edit_paths = False
    for fields in [raw, *(call.get("locations") or [])]:
        if isinstance(fields, dict):
            for key in PATH_KEYS & fields.keys():
                paths = fields[key] if isinstance(fields[key], list) else [fields[key]]
                for path in paths:
                    if isinstance(path, str):
                        yield from path_hits(path, own_workspace)
                        if kind in {"edit", "delete", "move"} and key not in {"cwd", "workdir"}:
                            edit_paths = True
                            forget_script(scripts, path, cwd or "")
    if kind in {"edit", "delete", "move"} and not edit_paths:
        scripts.clear()

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
                    forget_script(scripts, path, cwd or "")
                return
        scripts.clear()
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
        yield from shell_hits(command, own_workspace, commits, single_commit, scripts=scripts, cwd=cwd or "", home=home)
    elif kind == "execute" or title in {"rql_query", "mcp__rql__rql_query"}:
        yield "needs-inspection", f"missing or unsupported command input: {title[:400]}"


def audit_pilot(transcript_path, own_workspace, *, commits=(), allow_rql=False, single_commit=False,
                cwd=None, home=None):
    """Screen pilot inputs; a needs-inspection hit prevents automatic acceptance."""
    try:
        pages = json.loads(transcript_path.read_text())
        if not any(page["items"] for page in pages):
            raise ValueError("empty transcript")
        placement_path = transcript_path.with_name("placement.json")
        if (cwd is None or home is None) and placement_path.is_file():
            placement = json.loads(placement_path.read_text())
            cwd = cwd or placement.get("cwd")
            home = home or placement.get("home")
        result = scan(pages, own_workspace, commits=commits, allow_rql=allow_rql, single_commit=single_commit,
                      cwd=cwd, home=home)
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
