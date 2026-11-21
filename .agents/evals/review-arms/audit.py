"""Scans reviewer transcripts for access to later history or other forbidden sources.

Usage: python3 audit.py [runs/<bug>/<arm> ...]   (default: every finished run)
Writes audit.json in each run directory and prints a summary. A hit is not a
verdict: every hit is read by hand before a run is accepted or excluded.
"""
import ast
from collections import Counter
import json
import os
import pathlib
import posixpath
import re
import shlex
import sys

from urllib.parse import unquote, urlsplit

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


ROLLOUT_DIRECTORY = "codex-rollout"
PASSING_HIT_KINDS = {"interpreter-screened", "in-place-edit"}


def rollout_commands(paths):
    """Index Codex's durable command execution IDs, which the ACP bridge reuses."""
    commands, conflicts = {}, set()
    try:
        for path in paths:
            with path.open(encoding="utf-8") as rows:
                for line in rows:
                    entry = json.loads(line)
                    event = entry.get("payload") or {}
                    item = event.get("item") or {}
                    if (entry.get("type") != "event_msg" or event.get("type") != "item_completed"
                            or item.get("type") != "CommandExecution"):
                        continue
                    identifier, argv, directory = item.get("id"), item.get("command"), item.get("cwd")
                    if not isinstance(identifier, str) or not identifier:
                        continue
                    if (not isinstance(argv, list) or not argv or not argv[0]
                            or not all(isinstance(word, str) and "\0" not in word for word in argv)
                            or not isinstance(directory, str)):
                        conflicts.add(identifier)
                        continue
                    uri = urlsplit(directory)
                    if uri.scheme:
                        if uri.scheme != "file" or uri.netloc or uri.query or uri.fragment:
                            conflicts.add(identifier)
                            continue
                        directory = unquote(uri.path, errors="strict")
                    if not directory.startswith("/") or "\0" in directory:
                        conflicts.add(identifier)
                        continue
                    command = (argv, directory, item.get("exit_code"))
                    if commands.setdefault(identifier, command) != command:
                        conflicts.add(identifier)
    except (OSError, ValueError, AttributeError, TypeError):
        return {}  # An unreadable capture cannot supply command evidence.
    return {identifier: command for identifier, command in commands.items() if identifier not in conflicts}


def call_command(call):
    raw = call.get("rawInput")
    command = raw.get("command", raw.get("cmd", raw.get("argv"))) if isinstance(raw, dict) else raw
    if isinstance(command, list) and command and all(isinstance(word, str) for word in command):
        return shlex.join(command)
    return command if isinstance(command, str) and command.strip() else None


def tool_succeeded(call, body, rollout_exit):
    status = call.get("status")
    if status not in (None, "completed"):
        return False
    output = call.get("rawOutput")
    output = output if isinstance(output, dict) else {}
    terminals = body.get("terminal_outputs") or ()
    if output.get("isError") is True or output.get("signal") or any(row.get("signal") for row in terminals):
        return False
    exit_codes = [output[key] for key in ("exit_code", "exitCode") if key in output]
    exit_codes.extend(row.get("exit_code") for row in terminals)
    if rollout_exit is not None:
        exit_codes.append(rollout_exit)
    if exit_codes:
        reported = [code for code in exit_codes if code is not None]
        return bool(reported) and all(type(code) is int and code == 0 for code in reported)
    # Claude can finish a Bash call without a terminal result. Codex can
    # publish completed cards with an absent exit code for running commands.
    return call.get("name") == "Bash" and status == "completed"


def scan(pages, own_workspace, *, commits=None, allow_rql=False, single_commit=False, cwd=None, home=None,
         rollout=None, allow_in_place_edits=False, tracked_symlinks=None, tracked_paths=None):
    first_page = pages[0] if pages else {}
    execution = first_page.get("execution")
    checkout = cwd or first_page.get("cwd") or (execution.get("cwd") if isinstance(execution, dict) else None)
    edit_paths = EditPathEvidence(checkout, tracked_symlinks) if allow_in_place_edits else None
    hits, calls = [], 0
    scripts, written = ScriptEvidence(checkout, tracked_paths), set()
    for page in pages:
        execution = page.get("execution")
        execution = execution if isinstance(execution, dict) else {}
        page_cwd = page.get("cwd") or execution.get("cwd") or cwd
        page_home = page.get("home") or execution.get("home") or home
        for item in page["items"]:
            if item["role"] not in ("tool", "terminal"):
                continue
            body = item.get("body") or {}
            call = body.get("call") or {}
            if commits is not None and not call:
                # Standalone terminal_output rows contain output only. The
                # originating command is recorded as its own tool call.
                continue
            calls += 1
            if allow_rql and call.get("title") in ("rql_query", "mcp__rql__rql_query") and call.get("kind") != "execute":
                continue
            if commits is not None:
                match = None
                if rollout is not None and call.get("name") == "exec_command":
                    match = rollout.get(call.get("toolCallId"))
                    if command := call_command(call):
                        raw = call.get("rawInput")
                        directory = (raw.get("cwd") or raw.get("workdir")) if isinstance(raw, dict) else None
                        if match:
                            recorded = match[1]
                            if directory and (not isinstance(directory, str) or
                                    resolve_paths(directory, page_cwd or "") != resolve_paths(recorded, "")):
                                hits.append({"seq": item["seq"], "kind": "needs-inspection", "where": "input",
                                             "snippet": "Codex transcript and rollout working directories disagree"})
                                scripts.clear()
                                if edit_paths is not None:
                                    edit_paths.blocked = True
                                continue
                            directory = recorded
                        call = {**call, "kind": "execute", "rawInput": {"command": command, "cwd": directory}}
                    elif match:
                        argv, directory, _ = match
                        call = {**call, "kind": "execute", "rawInput": {"argv": argv, "cwd": directory}}
                    else:
                        hits.append({"seq": item["seq"], "kind": "needs-inspection", "where": "input",
                                     "snippet": f"missing Codex command evidence: {call.get('title') or 'exec_command'}"})
                        scripts.clear()
                        if edit_paths is not None:
                            edit_paths.blocked = True
                        continue
                observed = {}
                edit_index = 0
                for name, snippet in pilot_hits(call, own_workspace, commits, single_commit, scripts=scripts,
                                                written=written, cwd=page_cwd, home=page_home,
                                                edit_paths=edit_paths,
                                                succeeded=tool_succeeded(call, body, match[2] if match else None)):
                    hit = {"seq": item["seq"], "kind": name, "where": "input", "snippet": snippet}
                    if name == "in-place-edit":
                        hits.append({**hit, "edit_index": edit_index})
                        edit_index += 1
                    else:
                        if name == "needs-inspection" and edit_paths is not None:
                            edit_paths.blocked = True
                        observed[name] = hit
                scripts.forget_ambiguous()
                hits.extend(observed.values())
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
    "--max-filesize", "--sort", "--sortr", "--type-add",
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


# Bound the Cartesian product per input word so a transcript cannot make the
# audit allocate an unbounded argument list. Real file lists are much smaller.
BRACE_WORD_LIMIT = 1024
BRACE_RANGE = re.compile(r"(-?\d+|[A-Za-z])\.\.(-?\d+|[A-Za-z])(?:\.\.(-?\d+))?")


def brace_span(word):
    # Hide quoted, escaped and ordinary parameter-expansion braces from the
    # brace scanner. Keep the original offsets for copying shell words.
    mask = list(word)
    for part in SHELL_PART.finditer(word):
        if part[0].startswith(("'", '"', "\\")):
            mask[part.start():part.end()] = " " * len(part[0])
    masked = "".join(mask)
    for parameter in re.finditer(r"\$\{[^{}]*\}", masked):
        mask[parameter.start():parameter.end()] = " " * len(parameter[0])
    stack, candidates = [], []
    for index, char in enumerate(mask):
        if char == "{":
            stack.append((index, []))
        elif char == "," and stack:
            stack[-1][1].append(index)
        elif char == "}":
            if not stack:
                raise InspectionRequired("unmatched shell brace")
            start, commas = stack.pop()
            body = "".join(mask[start + 1:index])
            if commas or ".." in body and "{" not in body:
                candidates.append((start, index, commas, body))
    if stack:
        raise InspectionRequired("unmatched shell brace")
    return min(candidates, default=None)


def brace_options(word, start, end, commas, body):
    if commas:
        return [word[left + 1:right] for left, right in zip([start, *commas], [*commas, end])]
    match = BRACE_RANGE.fullmatch(body)
    if not match or match[1].isalpha() != match[2].isalpha():
        raise InspectionRequired("malformed shell brace range")
    first, last, stride = match.groups()
    numeric = not first.isalpha()
    begin, finish = (int(first), int(last)) if numeric else (ord(first), ord(last))
    step = abs(int(stride)) if stride else 1
    if not step or abs(finish - begin) // step >= BRACE_WORD_LIMIT:
        raise InspectionRequired("shell brace range exceeds the expansion limit or has a zero step")
    numbers = range(begin, finish + 1, step) if begin <= finish else range(begin, finish - 1, -step)
    if not numeric:
        # These characters are emitted as word data, not shell syntax.
        return [shlex.quote(chr(value)) for value in numbers]
    padded = any(len(part.lstrip("-")) > 1 and part.lstrip("-").startswith("0") for part in (first, last))
    width = max(len(first), len(last)) if padded else 0
    return [f"{value:0{width}d}" for value in numbers]


def brace_words(word):
    if "{" not in word and "}" not in word:
        return [word]
    pending, expanded = [word], []
    while pending:
        candidate = pending.pop()
        span = brace_span(candidate)
        if span is None:
            expanded.append(candidate)
            continue
        start, end, commas, body = span
        options = brace_options(candidate, start, end, commas, body)
        if len(options) + len(pending) + len(expanded) > BRACE_WORD_LIMIT:
            raise InspectionRequired(f"shell brace expansion exceeds {BRACE_WORD_LIMIT} words")
        pending.extend(candidate[:start] + option + candidate[end + 1:] for option in reversed(options))
    return expanded


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
    if isinstance(cwd, str) and cwd:
        variables["PWD"] = cwd
    if home:
        variables["HOME"] = home
    preceding = None
    alternate_cwds = frozenset()
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
            targets = []
            for mode, raw in paths:
                choices = brace_words(raw)
                if len(choices) != 1:
                    raise InspectionRequired("ambiguous shell brace redirection")
                targets.append((mode, shell_word(choices[0], variables)))
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
                argv = [shell_word(choice, variables) for raw in words for choice in brace_words(raw)]
            if delimiter and any(mode == "<" for mode, _ in paths):
                raise InspectionRequired("heredoc combined with another stdin redirection")
            if pending or delimiter:
                pending.append((argv, targets, delimiter, cwd, preceding, token))
            else:
                yield argv, targets, None, cwd, preceding, token
            skipped = frozenset()
            if token != preceding and preceding in {"&&", "||"}:
                # A different connector can rejoin an earlier skipped branch.
                skipped = alternate_cwds
                cwd = merge_cwds(cwd, skipped)
                alternate_cwds = frozenset()
            if argv[:1] == ["cd"] and token != "|" and preceding != "|":
                target = argv[1] if len(argv) == 2 and not argv[1].startswith(("-", "~")) else None
                if target and (posixpath.isabs(target) or cwd not in ("", None)):
                    if re.search(r"[*?\[]", target):
                        changed = None
                    else:
                        changed = one_or_many(resolve_paths(target, cwd))
                else:
                    changed = None
                if token == "&&":
                    alternate_cwds = merge_cwds(alternate_cwds, cwd)
                    cwd = merge_cwds(changed, skipped)
                elif token == "||":
                    alternate_cwds = merge_cwds(alternate_cwds, changed)
                else:
                    cwd = merge_cwds(cwd, changed)
            if isinstance(cwd, str) and cwd:
                variables["PWD"] = cwd
            else:
                variables.pop("PWD", None)
        words, paths, delimiter = [], [], None
        # A blank line does not erase a continuation after &&, ||, or |.
        if has_command or token != "\n":
            preceding = token
        if token == "\n":
            for argv, targets, marker, start_dir, leading, trailing in pending:
                source, offset = heredoc_body(command, offset, marker) if marker else (None, offset)
                yield argv, targets, source, start_dir, leading, trailing
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
    date mktemp true false seq tr test [ expr printf echo sleep rev tac comm join
    od xxd hexdump strings sha256sum sha1sum md5sum touch ln jq
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


def resolve_paths(path, cwd):
    """Resolve a file name against every possible recorded directory."""
    if posixpath.isabs(path):
        bases = ("",)
    elif cwd is None:
        return None
    else:
        bases = (cwd,) if isinstance(cwd, str) else cwd
    paths = (posixpath.normpath(posixpath.join(base, path)) for base in bases)
    return frozenset("/" + name.lstrip("/") if name.startswith("/") else name for name in paths)


def one_or_many(paths):
    if paths is not None and len(paths) == 1:
        return next(iter(paths))
    return paths


def merge_cwds(left, right):
    if left is None or right is None:
        return None
    left = {left} if isinstance(left, str) else left
    right = {right} if isinstance(right, str) else right
    return one_or_many(frozenset(left | right))


def recorded_path(path, cwd):
    candidates = one_or_many(resolve_paths(path, cwd))
    return candidates if isinstance(candidates, str) else None


def path_hits(path, own_workspace, cwd=""):
    # Keep the spelling too: a protected path can appear before a lexical '..'.
    if path == "-":
        return  # Standard input or output.
    paths = resolve_paths(path, cwd)
    if paths is None:
        yield "needs-inspection", f"relative operand with unknown working directory: {path}"
    for name in dict.fromkeys((path, *sorted(paths or ()))):
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


def git_hits(args, own_workspace, commits, single_commit, cwd=""):
    while args[:1] == ["-C"] and len(args) > 1:
        yield from path_hits(args[1], own_workspace, cwd)
        cwd = one_or_many(resolve_paths(args[1], cwd))
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
            yield from path_hits(path, own_workspace, cwd)
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
                    yield from path_hits(value.rsplit(":", 1)[-1] if name == "-L" else value, own_workspace, cwd)
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
        yield from path_hits(path, own_workspace, cwd)
    detached = verb == "checkout" and "--detach" in before
    if detached and not operands:
        raise InspectionRequired("detached checkout without an explicit pin")
    for operand in operands:
        revision, colon, path = operand.partition(":")
        if colon:
            yield from path_hits(path, own_workspace, cwd)
        if not colon and verb in {"diff", "log", "show", "grep"} and (
            operand.startswith(("./", "../", "/", "~/"))
            or "/" in operand and not operand.startswith(("origin/", "upstream/", "refs/"))
            or re.search(r"\.[a-zA-Z][a-zA-Z0-9_-]*$", operand)
        ):
            yield from path_hits(operand, own_workspace, cwd)
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
AWK_BUILTINS = {
    "length", "substr", "index", "split", "sub", "gsub", "match", "tolower", "toupper", "sprintf", "int",
}
AWK_STATEMENTS = {"print", "printf", "if", "else", "next", "exit", "BEGIN", "END", "for", "while", "in", "delete"}
AWK_NONLOCAL = {
    "getline", "system", "close", "fflush", "ENVIRON", "PROCINFO", "ARGV", "ARGC", "FUNCTAB", "SYMTAB",
    "function", "func", "switch", "case", "default", "do", "return", "break", "continue", "nextfile",
}
AWK_OPERATORS = set("[]{}().$;,:?~!+-*/%^<=>") | {
    "||", "&&", "==", "!=", "<=", ">=", "!~", "++", "--", "+=", "-=", "*=", "/=", "%=", "^=", ">>",
}
AWK_PRINT_CONTINUATIONS = AWK_OPERATORS - {")", "]", "}", ";", "++", "--"}
FILTER_TOKEN = re.compile(
    r"""(?P<quote>"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`)"""
    r"|(?P<comment>#[^\n]*)|\w+|\|[|&]|&&|>>|[!<>=+*/%^-]=|\+\+|--|!~|[^ \t\r]", re.S
)
FILTER_REGEX = re.compile(r"/(?:\\[^\n]|\[(?:\\.|[^\]\\\n])*\]|[^/\\\n])*/")
REGEX_PRECEDERS = {"", "\n", ";", "{", "}", "(", "[", "=", "!", "~", "!~", "&&", "||", ",", "?", ":", "if"}


def awk_arguments(args):
    options = iter(args)
    values = []
    for arg in options:
        if arg.startswith("-"):
            flag = arg[:2]
            if flag not in {"-F", "-v"}:
                raise InspectionRequired(f"unsupported awk option {arg}")
            value = arg[2:] or next(options, None)
            if value is None:
                raise InspectionRequired(f"missing awk {flag} value")
            if flag == "-v":
                name, equals, value = value.partition("=")
                if not equals or not name.isidentifier() or name in AWK_NONLOCAL:
                    raise InspectionRequired("unsupported awk variable assignment")
            values.append(value)
            continue
        paths = list(options)
        if any(path != "-" and path.startswith("-") or re.match(r"[A-Za-z_]\w*=", path) for path in paths):
            raise InspectionRequired("unsupported awk operand after the program")
        return [arg], paths, values
    raise InspectionRequired("missing awk program")


def sed_arguments(args):
    sources, paths = [], []
    in_place = False
    options = iter(args)
    for arg in options:
        if arg.startswith("-") and arg != "-":
            if arg == "-i" and not in_place:
                in_place = True
            elif arg.startswith("-e"):
                program = arg[2:] or next(options, None)
                if program is None:
                    raise InspectionRequired("missing sed -e program")
                sources.append(program)
            elif not re.fullmatch(r"-[nEr]+", arg):
                raise InspectionRequired(f"unsupported sed option {arg}")
        elif not sources:
            sources.append(arg)
        else:
            paths.append(arg)
    if not sources:
        raise InspectionRequired("missing sed program")
    return sources, paths, in_place


def awk_tokens(source):
    tokens = []
    offset = 0
    while offset < len(source):
        if source[offset] == "/" and (not tokens or tokens[-1] in REGEX_PRECEDERS):
            regex = FILTER_REGEX.match(source, offset)
            if regex:
                tokens.append(regex[0])
                offset = regex.end()
                continue
        match = FILTER_TOKEN.match(source, offset)
        if match:
            if not match["comment"]:
                tokens.append(match[0])
            offset = match.end()
        else:
            offset += 1
    return tokens


def check_awk_read(source):
    if re.search(r"\\\r?\n", source):
        raise InspectionRequired("awk backslash-newline continuation")
    printing = False
    depth = 0
    previous = ""
    tokens = awk_tokens(source)
    for index, token in enumerate(tokens):
        if token == "\n":
            if printing and previous in AWK_PRINT_CONTINUATIONS:
                raise InspectionRequired("awk continued print argument")
            if depth == 0:
                printing = False
            continue
        if token in {"print", "printf"}:
            printing = True
            depth = 0
        elif printing:
            if token in {">", ">>"}:
                raise InspectionRequired("awk print can redirect output")
            if depth == 0 and token in {";", "}"}:
                printing = False
            depth += (token in {"(", "["}) - (token in {")", "]"})
        if token.isidentifier():
            following = index + 1
            while following < len(tokens) and tokens[following] == "\n":
                following += 1
            next_token = tokens[following] if following < len(tokens) else ""
            if token in AWK_NONLOCAL or (next_token == "(" and token not in AWK_STATEMENTS | AWK_BUILTINS):
                raise InspectionRequired(f"unsupported awk word {token}")
        elif not (token in AWK_OPERATORS or token.isdecimal()
                  or len(token) >= 2 and token[0] in {'"', "/"} and token[-1] == token[0]):
            raise InspectionRequired(f"unsupported awk token {token}")
        previous = token


def sed_field(source, delimiter):
    field = re.match(rf"(?:\\[\s\S]|[^{re.escape(delimiter)}\\\n])*{re.escape(delimiter)}", source)
    if field is None:
        raise InspectionRequired("unterminated sed expression")
    return source[field.end():]


def sed_read_effects(source):
    address = r"(?:\d+|\$)"
    if re.search(r"\\\r?\n", source):
        raise InspectionRequired("sed backslash-newline continuation")
    while source := source.lstrip(" \t\r\n;"):
        if source.startswith("#"):
            source = source.partition("\n")[2]
            continue
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
            if command == "s":
                flags = re.match(r"[gpiI0-9 \t]*", source)[0]
                source = source[len(flags):]
                if source.startswith("w"):
                    yield "path", source[1:].partition("\n")[0].strip()
                if source.startswith(("w", "e")):
                    raise InspectionRequired("sed substitution has a write or execution flag")
        elif command in "eRrWw":
            argument = source.partition("\n")[0].strip()
            if argument:
                yield ("command" if command == "e" else "path"), argument
            raise InspectionRequired(f"unsupported sed command {command}")
        elif command not in "pdqn=":
            raise InspectionRequired(f"unsupported sed command {command}")
        if source.lstrip(" \t\r")[:1] not in {"", ";", "\n", "}"}:
            raise InspectionRequired(f"unsupported arguments to sed {command}")


PERL_READ_WORDS = {
    "print", "printf", "chomp", "chop", "split", "join", "length", "substr", "index", "lc", "uc", "sprintf",
    "if", "unless", "next", "last", "exit", "and", "or", "not",
    "eq", "ne", "lt", "le", "gt", "ge", "cmp", "x",
}
PERL_VARIABLE = re.compile(r"\$(?:[1-9](?!\w)|[.&]|[A-Za-z_]\w*)|@F\b")
PERL_NUMBER = re.compile(r"(?:(?:0[xX][\da-fA-F]+|0[bB][01]+)|(?:\d+(?:\.\d+)?|\.\d+)(?:[eE][+-]?\d+)?)(?!\w)")
PERL_OPERATOR = re.compile(r"<=>|=~|!~|==|!=|<=|>=|&&|\|\||\+\+|--|\*\*|[+.*/%=-]=|[+*/%!=<>.,;:?(){}\[\]-]")
PERL_REGEX_OPERATORS = {"m", "s", "tr", "y"}
PERL_BUILTIN_VARIABLES = {"$_", "$.", "$&", "@F"} | {f"${n}" for n in range(1, 10)}
PERL_SPECIAL_SCALARS = {"$_", "$ARGV", "$ENV", "$INC", "$SIG"}


def perl_arguments(args):
    sources, paths, separators = [], [], []
    loop = False
    operands = iter(args)
    for arg in operands:
        if arg == "--":
            paths.extend(operands)
            break
        if not arg.startswith("-") or arg == "-":
            paths.extend([arg, *operands])
            break
        flags = arg[1:]
        while flags:
            flag, flags = flags[0], flags[1:]
            if flag in "np":
                loop = True
            elif flag in "la":
                continue
            elif flag in "Fe":
                value = flags or next(operands, None)
                if value is None:
                    raise InspectionRequired(f"missing perl -{flag} value")
                (sources if flag == "e" else separators).append(value)
                break
            else:
                raise InspectionRequired(f"unsupported perl -{flag} option")
    if not loop or not sources:
        raise InspectionRequired("perl read requires -n or -p and a visible program")
    return sources, paths, separators


def check_perl_interpolation(text, *, regex=False, locals_=frozenset()):
    if regex and re.search(r"\(\?\??\{", text):
        raise InspectionRequired("perl regular expression can execute code")
    for marker in re.finditer(r"\\[\s\S]|[$@]", text):
        if marker[0].startswith("\\"):
            continue
        variable = PERL_VARIABLE.match(text, marker.start())
        if (variable and variable[0] in PERL_BUILTIN_VARIABLES | locals_
                and not text[variable.end():].startswith(("{", "[", "->", "::"))):
            continue
        if regex and marker[0] == "$" and (not text[marker.end():] or text[marker.end()] in ")|"):
            continue  # End-of-line anchor.
        raise InspectionRequired("unsupported perl interpolation")


def perl_regex(source, operator, locals_):
    if operator in {"m", "s"}:
        if not (pattern := FILTER_REGEX.match(source)):
            raise InspectionRequired("unterminated perl regex")
        check_perl_interpolation(pattern[0][1:-1], regex=True, locals_=locals_)
        source = source[pattern.end():]
    else:
        rest = sed_field(source[1:], "/")
        check_perl_interpolation(source[1:len(source) - len(rest) - 1], regex=True, locals_=locals_)
        source = rest
    if operator in {"s", "tr", "y"}:
        rest = sed_field(source, "/")
        check_perl_interpolation(source[:len(source) - len(rest) - 1], regex=True, locals_=locals_)
        source = rest
    flags = re.match(r"[a-zA-Z]*", source)[0]
    if "e" in flags:
        raise InspectionRequired("perl substitution can execute code")
    return source[len(flags):]


def check_perl_read(source):
    # Lex the read subset explicitly. Perl has implicit calls and executable
    # quote forms, so an unrecognized token cannot be treated as inert.
    previous = ""
    locals_ = set()
    while source := source.lstrip():
        if source.startswith("#"):
            source = source.partition("\n")[2]
            continue
        if quote := FILTER_TOKEN.match(source):
            if quote["quote"]:
                if source.startswith("`"):
                    raise InspectionRequired("perl backticks")
                if source.startswith('"'):
                    check_perl_interpolation(quote[0][1:-1], locals_=locals_)
                source, previous = source[quote.end():], "literal"
                continue
        if variable := PERL_VARIABLE.match(source):
            if variable[0] not in PERL_BUILTIN_VARIABLES | locals_:
                raise InspectionRequired(f"unsupported perl variable {variable[0]}")
            source, previous = source[variable.end():], "literal"
            continue
        if number := PERL_NUMBER.match(source):
            source, previous = source[number.end():], "literal"
            continue
        if word := re.match(r"[A-Za-z_]\w*", source):
            rest = source[word.end():]
            if word[0] == "my":
                declaration = re.match(r"\s+(\$[A-Za-z_]\w*)", rest)
                if (not declaration or declaration[1] in PERL_SPECIAL_SCALARS
                        or rest[declaration.end():].lstrip().startswith(("{", "[", "->", "::"))):
                    raise InspectionRequired("unsupported perl lexical declaration")
                locals_.add(declaration[1])
                source, previous = rest[declaration.end():], "literal"
            elif word[0] in PERL_REGEX_OPERATORS and rest.startswith("/"):
                source, previous = perl_regex(rest, word[0], locals_), "literal"
            elif word[0] in PERL_READ_WORDS:
                source, previous = rest, word[0]
            else:
                raise InspectionRequired(f"unsupported perl bareword {word[0]}")
            continue
        expects_operand = previous not in {"literal", ")", "]", "}"}
        if source.startswith("/") and expects_operand:
            source, previous = perl_regex(source, "m", locals_), "literal"
            continue
        if (source.startswith(("->", "<<", "<>")) or source.startswith("<") and expects_operand
                or re.match(r"-[A-Za-z]\b", source)):
            raise InspectionRequired("perl file, call, or test operator")
        if operator := PERL_OPERATOR.match(source):
            source, previous = source[operator.end():], operator[0]
            continue
        raise InspectionRequired("unsupported perl token")


def perl_open_operand(value):
    value = value.strip()
    if value.startswith("|") or value.endswith("|") or re.match(r"^[+<>]", value):
        raise InspectionRequired("perl implicit open can run a command or change access mode")
    return "read", value


class EditPathEvidence:
    def __init__(self, checkout, tracked):
        self.checkout = checkout if isinstance(checkout, str) and posixpath.isabs(checkout) else None
        self.root_links = None
        if isinstance(tracked, list) and all(
            isinstance(path, str) and all(part not in {"", ".", ".."} for part in path.split("/"))
            for path in tracked
        ):
            self.root_links = {path for path in tracked if "/" not in path}
        self.blocked = False

    def check(self, path, cwd, own_workspace):
        if (not self.checkout or not self.checkout.startswith(f"/workspace/{own_workspace}/")
                or cwd != self.checkout or not path or path in {".", ".."}
                or path.startswith(("-", "~")) or re.search(r"[/\x00*?\[]", path)):
            raise InspectionRequired("in-place edit requires a bare filename at the recorded checkout root")
        if self.blocked:
            raise InspectionRequired("in-place edit follows link handling or an unresolved command")
        if self.root_links is None:
            raise InspectionRequired("in-place edit requires tracked symlink inventory")
        if path in self.root_links:
            raise InspectionRequired("in-place edit names a tracked symlink")


def may_relocate_links(program, args):
    if program in {"ln", "mv", "rsync", "tar", "cpio", "install"}:
        return True
    if program != "cp":
        return False
    end = args.index("--") if "--" in args else len(args)
    for arg in args[:end]:
        flag, _, value = arg.partition("=")
        if flag.startswith("--") and any(option.startswith(flag) for option in (
            "--symbolic-link", "--no-dereference", "--archive", "--recursive",
        )):
            return True
        if flag.startswith("--") and "--preserve".startswith(flag) and any(
            attribute and ("links".startswith(attribute) or "all".startswith(attribute))
            for attribute in value.split(",")
        ):
            return True
        if re.match(r"^-[^-]*[sPdaRr]", arg):
            return True
    return False


def text_filter_hits(program, args, own_workspace, commits, single_commit, cwd, scripts, written,
                     edit_paths):
    sources, values = args, ()
    in_place = False
    try:
        if program == "sed":
            sources, paths, in_place = sed_arguments(args)
        elif program == "perl":
            in_place = args[:1] == ["-i"]
            if in_place:
                args = args[1:]
                if not args or args[0] not in {"-pe", "-lpe"}:
                    raise InspectionRequired("perl in-place edit requires -pe or -lpe")
            sources, paths, values = perl_arguments(args)
        else:
            sources, paths, values = awk_arguments(args)

        unsafe_edit = False
        for path in paths:
            _, operand = perl_open_operand(path) if program == "perl" else ("read", path)
            hazards = list(path_hits(operand, own_workspace, cwd))
            yield from hazards
            unsafe_edit |= bool(hazards) or path != operand
        if in_place:
            if edit_paths is None:
                raise InspectionRequired("in-place edits are permitted only in writer sessions")
            if not paths or unsafe_edit:
                raise InspectionRequired("in-place edit requires screened file operands")
            for path in paths:
                edit_paths.check(path, cwd, own_workspace)
        if program == "perl":
            for value in values:
                check_perl_interpolation(value, regex=True)
            check_perl_read("\n".join(sources))
        elif program in AWK:
            check_awk_read(sources[0])
        else:
            for source in sources:
                for kind, argument in sed_read_effects(source):
                    if kind == "command":
                        yield from subprocess_hits(argument, own_workspace, commits, single_commit, cwd)
                    elif argument:
                        yield from path_hits(argument, own_workspace, cwd)
        if in_place:
            if scripts is not None:
                scripts.clear()
            if written is not None:
                written.update(posixpath.basename(path) for path in paths)
            yield "in-place-edit", shlex.join(paths)
    except InspectionRequired:
        # Literals in an accepted program are data. In an opaque program
        # they may name resources the program accesses.
        for source in sources:
            yield from filter_source_hits(source, own_workspace, commits, single_commit, cwd)
        for value in values:
            yield from source_literal_hits(value, own_workspace, cwd)
        raise


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


def source_literal_hits(value, own_workspace, cwd=""):
    if not value or value == "/" or "\n" in value:
        return
    if re.fullmatch(r"(?i)(?:[\w-]+\.)*(?:github\.com|claude\.ai)(?::[0-9]+)?", value):
        yield "github-or-web", value
    if not any(char.isspace() for char in value) or value.startswith(("/", "~/", ".mj/")):
        yield from path_hits(value, own_workspace, cwd)


def filter_source_hits(source, own_workspace, commits, single_commit, cwd):
    # Use the same quoted literals as non-Python interpreter source. Git
    # commands visible in strings still use the normal revision screen.
    for match in re.finditer(QUOTED_SOURCE.rstrip(), source):
        value = match[0][1:-1]
        yield from source_literal_hits(value, own_workspace, cwd)
        if re.match(r"(?:[^\s]*/)?(?:git|gh|curl|wget|ssh|scp|sftp|nc|netcat)\s", value):
            yield from shell_hits(value, own_workspace, commits, single_commit, cwd=cwd)


def python_name(expr, imports):
    if isinstance(expr, ast.Name):
        return imports.get(expr.id, expr.id)
    if isinstance(expr, ast.Attribute):
        return f"{python_name(expr.value, imports)}.{expr.attr}"
    return ""


def python_may_relocate_links(node, name):
    if name in {"os.symlink", "os.rename", "os.replace", "shutil.move", "shutil.copytree"}:
        return True
    if not isinstance(node.func, ast.Attribute):
        return False
    if node.func.attr in {"symlink_to", "rename"}:
        return True
    if node.func.attr != "replace":
        return False
    # Path.replace(target) takes one argument. The unbound class method
    # takes two, as does the ordinary string operation replace(old, new).
    if name in {"Path.replace", "pathlib.Path.replace"}:
        return True
    return sum(not isinstance(arg, ast.Starred) for arg in node.args) < 2


def subprocess_hits(value, own_workspace, commits, single_commit, cwd="", written=None, *,
                    edit_paths=None):
    if isinstance(value, str):
        yield from shell_hits(value, own_workspace, commits, single_commit, cwd=cwd, written=written,
                              edit_paths=edit_paths)
    elif isinstance(value, (tuple, list)) and value and all(isinstance(word, str) for word in value):
        yield from command_hits(list(value), own_workspace, commits, single_commit, cwd=cwd, written=written,
                              edit_paths=edit_paths)
    else:
        yield "needs-inspection", "subprocess argv is not visible in interpreter source"


def interpreter_hits(program, source, own_workspace, commits, single_commit, cwd="", written=None, *,
                     edit_paths=None):
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
                yield from source_literal_hits(node.value, own_workspace, cwd)
            if not isinstance(node, ast.Call):
                continue
            name = python_name(node.func, imports)
            if edit_paths is not None and python_may_relocate_links(node, name):
                edit_paths.blocked = True
            if NETWORK_CALL.match(name):
                yield "network-command", name
            elif name in OPAQUE_CALLS or name.startswith("base64.") and "decode" in name:
                yield "needs-inspection", f"opaque interpreter operation {name}"
            elif name in PROCESS_CALLS:
                try:
                    argv = ast.literal_eval(node.args[0]) if node.args else None
                except (ValueError, TypeError, SyntaxError):
                    argv = None
                yield from subprocess_hits(argv, own_workspace, commits, single_commit, cwd, written,
                                           edit_paths=edit_paths)
    else:
        # Inspect literal strings and recognizable calls in inline non-Python
        # source. Interpolated code remains subject to the literal-only limit.
        for match in re.finditer(QUOTED_SOURCE.rstrip(), source):
            yield from source_literal_hits(match[0][1:-1], own_workspace, cwd)
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
                    yield from subprocess_hits(literal[1][1:-1], own_workspace, commits, single_commit, cwd, written,
                                           edit_paths=edit_paths)
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


SYSTEM_PROGRAM_DIRS = {"/usr/bin", "/bin", "/usr/local/bin", "/usr/sbin", "/sbin"}
SYSTEM_PROGRAM_NAMES = READ_COMMANDS | AWK | SEARCH | NETWORK | MEMORY_TOOLS | INLINE_OPTIONS.keys() | {
    "bash", "sh", "sed", "find", "env", "timeout", "nice", "nohup", "git", "gh", "curl", "wget", "mj",
    "sqlite", "sqlite3", "pytest", "cargo", "npm", "go", "make", "expect", "xargs",
}


def system_program(executable, written):
    if "/" not in executable:
        if executable in written:
            raise InspectionRequired(f"session wrote executable name {executable}")
        return executable
    directory, name = posixpath.split(executable)
    if directory in SYSTEM_PROGRAM_DIRS:
        return name
    raise InspectionRequired(f"untrusted executable {executable}")


def wrapped_command(program, args):
    if program == "env":
        args = args[1:] if args[:1] == ["--"] else args
        if args and re.match(r"[A-Za-z_]\w*=", args[0]):
            raise InspectionRequired("env changes the visible command environment")
    elif program == "timeout":
        if not args or not re.fullmatch(r"\d+(?:\.\d+)?[smhd]?", args[0]):
            raise InspectionRequired("unsupported timeout duration or options")
        args = args[1:]
    elif program == "nice" and args[:1] == ["-n"]:
        if len(args) < 2 or not re.fullmatch(r"[+-]?\d+", args[1]):
            raise InspectionRequired("unsupported nice adjustment")
        args = args[2:]
    if not args or args[0].startswith("-"):
        raise InspectionRequired(f"missing visible {program} command")
    return args


def jq_paths(args):
    program = None
    words = iter(args)
    for word in words:
        if word in {"--arg", "--argjson", "--rawfile", "--slurpfile", "--argfile"}:
            name, value = next(words, None), next(words, None)
            if name is None or value is None:
                raise InspectionRequired(f"missing arguments to jq {word}")
            if word in {"--rawfile", "--slurpfile", "--argfile"}:
                yield value
        elif word in {"-f", "--from-file", "-L"}:
            if path := next(words, None):
                yield path
            raise InspectionRequired(f"jq option {word} loads external code")
        elif word == "--":
            for path in words:
                if program is None:
                    program = path
                else:
                    yield path
        elif word.startswith("-") and word != "-":
            if not (word in {"--slurp", "--raw-input", "--null-input", "--compact-output", "--raw-output",
                             "--join-output", "--ascii-output", "--sort-keys", "--exit-status", "--tab"}
                    or re.fullmatch(r"-[sRncrajSeCM]+", word)):
                raise InspectionRequired(f"unsupported jq option {word}")
        elif program is None:
            program = word
        else:
            yield word
    if program is None or re.search(r"\b(?:include|import|modulemeta)\b", program):
        raise InspectionRequired("jq program is missing or can load external code")


XXD_VALUE_OPTIONS = {"-c", "-cols", "-g", "-groupsize", "-l", "-len", "-n", "-name", "-o", "-offset", "-s", "-seek"}


def file_operands(args, value_options=(), *, options_first=False):
    options = True
    words = iter(args)
    for arg in words:
        if options and arg == "--":
            options = False
            continue
        if options and arg in value_options:
            if next(words, None) is None:
                raise InspectionRequired(f"missing value for {arg}")
            continue
        if options and arg.startswith("-") and arg != "-":
            name, _, _ = arg.partition("=")
            if name in value_options:
                continue
            if "=" in arg:
                yield arg.split("=", 1)[1]
            elif "/" in arg:
                yield arg
        else:
            yield arg
            if options_first:
                options = False


def link_hits(args, own_workspace, cwd):
    operands = []
    target_dir = None
    options, symbolic, relative, no_target_dir = True, False, False, False
    words = iter(args)
    for arg in words:
        if options and arg == "--":
            options = False
        elif options and (arg in {"-t", "--target-directory"} or arg.startswith("--target-directory=")):
            target_dir = arg.partition("=")[2] if "=" in arg else next(words, None)
            if not target_dir:
                raise InspectionRequired("ln target directory is missing")
        elif options and arg.startswith("-") and arg != "-":
            names = {"--symbolic": "s", "--relative": "r", "--no-target-directory": "T", "--force": "f",
                     "--no-dereference": "n", "--verbose": "v", "--logical": "L", "--physical": "P"}
            flags = names.get(arg, arg[1:])
            if not re.fullmatch(r"[srTfnvLP]+", flags):
                raise InspectionRequired(f"unsupported ln option {arg}")
            symbolic |= "s" in flags
            relative |= "r" in flags
            no_target_dir |= "T" in flags
        else:
            operands.append(arg)

    if not operands or (no_target_dir and (target_dir or len(operands) != 2)):
        raise InspectionRequired("ln target or link destination is ambiguous")
    if not symbolic or relative:
        # GNU ln --relative computes its target from the command's cwd.
        for path in [*operands, *([target_dir] if target_dir else [])]:
            yield from path_hits(path, own_workspace, cwd)
        return
    if target_dir:
        targets, destination = operands, target_dir
    elif len(operands) == 1:
        targets, destination = operands, "."
    else:
        targets, destination = operands[:-1], operands[-1]
    yield from path_hits(destination, own_workspace, cwd)
    destinations = resolve_paths(destination, cwd) or ()
    if target_dir or len(operands) != 2 or destination.endswith("/"):
        directories = set(destinations)
    else:
        directories = {posixpath.dirname(path) for path in destinations}
        if not no_target_dir:
            # With two operands, an existing directory also receives the link.
            directories.update(destinations)
    for target in targets:
        if posixpath.isabs(target):
            yield from path_hits(target, own_workspace, cwd)
        else:
            if not directories or any(not posixpath.isabs(path) for path in directories):
                raise InspectionRequired("relative symlink target without a known link directory")
            yield from path_hits(target, own_workspace, frozenset(directories))


def command_hits(words, own_workspace, commits, single_commit, *, scripts=None, written=None, cwd="", home=None,
                 running_scripts=frozenset(), edit_paths=None, successful_writes=None):
    executable, *args = words
    if "/" in executable:
        yield from path_hits(executable, own_workspace, cwd)
    program = system_program(executable, written or ())
    if edit_paths is not None and may_relocate_links(program, args):
        edit_paths.blocked = True
    inline = interpreter_arguments(program, args)
    if program in NETWORK:
        yield "network-command", program
    elif program in {"gh", "curl", "wget"}:
        yield "github-or-web", program
    elif program == "git":
        yield from git_hits(args, own_workspace, commits, single_commit, cwd)
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
    elif program in {"env", "timeout", "nice", "nohup"}:
        inner = wrapped_command(program, args)
        if program == "nohup":
            yield from path_hits("nohup.out", own_workspace, cwd)
            if scripts is not None and written is not None:
                forget_script(scripts, "nohup.out", cwd)
                written.add("nohup.out")
        if scripts is not None and written is not None:
            record_script_write(scripts, written, inner, (), None, cwd)
        yield from command_hits(inner, own_workspace, commits, single_commit,
                                scripts=scripts, written=written, cwd=cwd, home=home, running_scripts=running_scripts,
                                edit_paths=edit_paths, successful_writes=successful_writes)
    elif program in {"bash", "sh"} and len(args) == 2 and args[0] in {"-c", "-lc"}:
        yield from shell_hits(args[1], own_workspace, commits, single_commit, scripts=scripts, written=written, cwd=cwd, home=home,
                              running_scripts=running_scripts, edit_paths=edit_paths,
                              successful_writes=successful_writes)
    elif program in AWK | {"sed", "perl"}:
        yield from text_filter_hits(program, args, own_workspace, commits, single_commit, cwd, scripts, written,
                                    edit_paths)
    elif program in INLINE_OPTIONS and len(inline) >= 2 and inline[0] == INLINE_OPTIONS[program]:
        for arg in inline[2:]:
            yield from source_literal_hits(arg, own_workspace, cwd)
        yield from interpreter_hits(program, inline[1], own_workspace, commits, single_commit, cwd, written,
                                    edit_paths=edit_paths)
        if scripts is not None:
            interpreter_writes(scripts, program, inline[1], cwd, [inline[0], *inline[2:]])
    elif program in SEARCH:
        files, operands, trailing = search_operands(program, args)
        for path in files + operands + trailing:
            yield from path_hits(path, own_workspace, cwd)
    elif program == "find":
        for path in find_paths(args):
            yield from path_hits(path, own_workspace, cwd)
    elif program == "ln":
        yield from link_hits(args, own_workspace, cwd)
    elif program == "jq":
        for path in jq_paths(args):
            yield from path_hits(path, own_workspace, cwd)
    elif program in {"echo", "printf", "true", "false", "tr"}:
        if program == "printf" and args and args[0].startswith("-v"):
            raise InspectionRequired("printf may assign a shell variable")
        return  # These operands are data. Redirection targets are checked separately.
    elif program in READ_COMMANDS:
        if program == "sort":
            end = args.index("--") if "--" in args else len(args)
            if any(arg.startswith("--compress") for arg in args[:end]):
                raise InspectionRequired("sort may start an external program")
        value_options = {"ls": {"-I", "--ignore", "--hide"}, "xxd": XXD_VALUE_OPTIONS}.get(program, ())
        for path in file_operands(args, value_options, options_first=program == "xxd"):
            yield from path_hits(path, own_workspace, cwd)
    else:
        raise InspectionRequired(f"unsupported command {program}")


def interpreter_arguments(program, args):
    # Only skip flags that cannot load code, read an extra file, or redirect output.
    offset = 0
    while offset < len(args):
        option = args[offset]
        if program in PYTHON:
            if re.fullmatch(r"-[IBEsSuO]+", option):
                offset += 1
                continue
            if not option.startswith(("-W", "-X")):
                break
            separate = len(option) == 2
            value = args[offset + 1] if separate and offset + 1 < len(args) else option[2:]
            if option.startswith("-W"):
                # A qualified warning category causes Python to import its module.
                fields = value.split(":")
                safe = bool(value) and len(fields) <= 5 and (
                    len(fields) < 3 or re.fullmatch(r"[A-Za-z_0-9]*", fields[2])
                )
            else:
                safe = re.fullmatch(
                    r"(?:dev|faulthandler|warn_default_encoding|importtime(?:=2)?|"
                    r"utf8(?:=[01])?|tracemalloc(?:=[0-9]+)?)", value
                )
            if not safe:
                raise InspectionRequired(f"unsupported Python {option[:2]} option")
            offset += 2 if separate else 1
        elif program == "node":
            if option not in {"--no-addons", "--no-deprecation", "--no-warnings", "--pending-deprecation",
                              "--throw-deprecation", "--trace-deprecation", "--trace-warnings"}:
                break
            offset += 1
        else:
            break
    return args[offset:]


class ScriptEvidence(dict):
    """Confirmed contents and screened writes whose destinations started empty."""

    def __init__(self, checkout=None, tracked=None):
        super().__init__()
        self.checkout = checkout if isinstance(checkout, str) and posixpath.isabs(checkout) else None
        self.tracked = set(tracked) if isinstance(tracked, list) else None
        self.fresh = {}
        self.opaque_paths = set()
        self.opaque = False

    def clear(self):
        super().clear()
        self.fresh.clear()
        self.opaque = True

    def forget_ambiguous(self):
        for cache in (self, self.fresh):
            for key in tuple(cache):
                if isinstance(key, tuple):
                    cache.pop(key)

    def starts_empty(self, path, cwd):
        if (self.opaque or self.tracked is None or not self.checkout or ".." in path.split("/")
                or re.search(r"[*?\[]", path)):
            return False
        paths = resolve_paths(path, cwd)
        if not paths:
            return False
        prefix = self.checkout.rstrip("/") + "/"
        for filename in paths:
            if not filename.startswith(prefix) or any(
                filename == path or filename.startswith(path.rstrip("/") + "/") for path in self.opaque_paths
            ):
                return False
            relative = filename.removeprefix(prefix)
            if relative in self.tracked or any(
                str(parent) in self.tracked for parent in pathlib.PurePosixPath(relative).parents
            ):
                return False
        return True

    def opaque_write(self, path, cwd):
        paths = resolve_paths(path, cwd)
        if not paths or re.search(r"[*?\[]", path) or self.tracked is not None and any(
            not posixpath.isabs(filename) for filename in paths
        ):
            self.clear()
            return
        self.opaque_paths.update(paths)
        for key in tuple(self.fresh):
            locations = {key} if isinstance(key, str) else resolve_paths(key[1], key[0])
            if paths.intersection(locations):
                self.fresh.pop(key)

    def possible_sources(self, path, cwd, program):
        if not self.starts_empty(path, cwd):
            return ()
        candidates = resolve_paths(path, cwd)
        sources = {}
        for key, (language, versions) in self.fresh.items():
            locations = {key} if isinstance(key, str) else resolve_paths(key[1], key[0])
            if candidates.intersection(locations):
                if program is not None and not same_interpreter(program, language):
                    return ()
                sources.update(dict.fromkeys(versions))
        return tuple(sources)


SCRIPT_INTERPRETERS = {
    ".py": "python3", ".js": "node", ".mjs": "node", ".cjs": "node", ".sh": "sh", ".rb": "ruby", ".pl": "perl",
}


def script_language(path, source):
    if source.startswith("#!"):
        argv = shlex.split(source.splitlines()[0][2:])
        if argv[:1] == ["/usr/bin/env"]:
            argv = argv[1:]
        if argv:
            program = posixpath.basename(argv[0])
            if program in INLINE_OPTIONS or program in {"sh", "bash"}:
                return program
    return SCRIPT_INTERPRETERS.get(posixpath.splitext(path)[1])


def same_interpreter(left, right):
    return left == right or left in PYTHON and right in PYTHON or left in {"sh", "bash"} and right in {"sh", "bash"}


def script_key(path, cwd):
    key = recorded_path(path, cwd)
    if key is None and isinstance(cwd, frozenset) and not re.search(r"[*?\[]", path):
        # The same relative operand is valid across an uninterrupted chain,
        # but no individual absolute path is established by this write.
        return cwd, path
    return key


def forget_script(scripts, path, cwd, *, opaque=True):
    if opaque:
        scripts.opaque_write(path, cwd)
    paths = resolve_paths(path, cwd)
    if paths is None:
        scripts.clear()
        return
    for key in paths:
        scripts.pop(key, None)
    for key in tuple(scripts):
        if isinstance(key, tuple) and paths.intersection(resolve_paths(key[1], key[0])):
            scripts.pop(key)
    if not cwd and not posixpath.isabs(path):
        key = next(iter(paths))
        # The transcript may omit the directory for one of two equivalent paths.
        for previous in list(scripts):
            if isinstance(previous, str) and previous.endswith("/" + key):
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


def record_script_write(scripts, written, words, redirections, source, cwd, staged=None):
    program = posixpath.basename(words[0]) if words else ""
    stdout = [(mode, path) for mode, path in redirections if mode in {">", ">>"}]
    target = stdout[0][1] if len(stdout) == 1 and stdout[0][0] == ">" else None
    visible = {}
    if source is not None and words == ["cat"] and target is not None:
        system_program(words[0], written)
        visible[target] = source
    elif source is not None and program == "tee" and all(not path.startswith("-") for path in words[1:]):
        system_program(words[0], written)
        visible.update(dict.fromkeys(words[1:] + ([target] if target else []), source))
    elif program == "printf" and target is not None:
        system_program(words[0], written)
        if (text := literal_printf(words[1:])) is not None:
            visible[target] = text
    for mode, path in redirections:
        if mode != "<":
            written.add(posixpath.basename(path))
            forget_script(scripts, path, cwd, opaque=mode != ">" or path not in visible)
    if program in {"cp", "mv", "ln"} and len(words) >= 2:
        written.add(posixpath.basename(words[-1]))
    if program == "touch":
        written.update(posixpath.basename(path) for path in file_operands(words[1:]))
    if program in {"cp", "mv", "ln"} or program == "git" and words[1:2] in (["checkout"], ["switch"]):
        scripts.clear()  # A destination can name a directory or be a symlink.
    elif program == "rm":
        dict.clear(scripts)  # Directory operands may remove more than the named path.
        for path in file_operands(words[1:]):
            forget_script(scripts, path, cwd)
    elif program == "sort":
        if any(word.startswith(("-i", "--in-place", "-o", "--output")) or re.fullmatch(r"-[nEr]+i[niEr]*", word)
               for word in words[1:]):
            scripts.clear()
    elif program == "tee":
        for path in file_operands(words[1:]):
            written.add(posixpath.basename(path))
            forget_script(scripts, path, cwd, opaque=path not in visible)
    elif program == "xxd":
        paths = list(file_operands(words[1:], XXD_VALUE_OPTIONS, options_first=True))
        if len(paths) == 2 and paths[1] != "-":
            written.add(posixpath.basename(paths[1]))
            forget_script(scripts, paths[1], cwd)
    for path, text in visible.items():
        if staged is not None and (key := script_key(path, cwd)) is not None:
            scripts[key] = text
            staged.add(key)
    return visible


def interpreter_writes(scripts, program, source, cwd, argv):
    # Preserve only the empty-at-start history when the interpreter's
    # possible mutations can be assigned to specific paths.
    if scripts.tracked is None or program not in PYTHON:
        scripts.clear()
        return
    dict.clear(scripts)
    try:
        nodes = list(ast.walk(ast.parse(source)))
    except SyntaxError:
        scripts.clear()
        return

    stores = Counter(node.id for node in nodes if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Store))
    stores.update(node.arg for node in nodes if isinstance(node, ast.arg))
    bindings = {target.id: node.value for node in nodes if isinstance(node, ast.Assign)
                for target in node.targets if isinstance(target, ast.Name) and stores[target.id] == 1}
    imports = {}
    understood = {"builtins", "collections", "copy", "functools", "io", "itertools",
                  "json", "math", "os", "pathlib", "posix", "re", "sys", "typing"}
    for node in nodes:
        if isinstance(node, ast.Import):
            for alias in node.names:
                module = alias.name.split(".")[0]
                if module not in understood:
                    scripts.clear()
                    return
                imports[alias.asname or module] = alias.name if alias.asname else module
        elif isinstance(node, ast.ImportFrom):
            if node.level or not node.module or node.module.split(".")[0] not in understood:
                scripts.clear()
                return
            for alias in node.names:
                imports[alias.asname or alias.name] = f"{node.module}.{alias.name}"

    if stores.keys() & imports.keys() or any(
        isinstance(node, (ast.Attribute, ast.Subscript)) and isinstance(node.ctx, (ast.Store, ast.Del))
        and python_name(node.value, imports) in {"sys", "sys.argv", "sys.path"}
        for node in nodes
    ):
        scripts.clear()
        return

    def value(node, seen=frozenset()):
        if isinstance(node, ast.Constant) and isinstance(node.value, str):
            return node.value
        if isinstance(node, ast.Name) and node.id in bindings and node.id not in seen:
            return value(bindings[node.id], seen | {node.id})
        if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
            left, right = value(node.left, seen), value(node.right, seen)
            if left is not None and right is not None:
                return left + right
        if (isinstance(node, ast.Subscript) and python_name(node.value, imports) == "sys.argv"
                and isinstance(node.slice, ast.Constant) and type(node.slice.value) is int
                and 0 <= node.slice.value < len(argv)):
            return argv[node.slice.value]
        if (isinstance(node, ast.Call) and python_name(node.func, imports) == "pathlib.Path"
                and len(node.args) == 1 and not node.keywords):
            return value(node.args[0], seen)
        return None

    def argument(node, index, keyword):
        if index < len(node.args):
            return node.args[index]
        return next((pair.value for pair in node.keywords if pair.arg == keyword), None)

    for node in nodes:
        if not isinstance(node, ast.Call):
            continue
        name = python_name(node.func, imports)
        method = node.func.attr if isinstance(node.func, ast.Attribute) else ""
        if name in {"open", "builtins.open", "io.open"} or method == "open" and name != "os.open":
            bound = name not in {"open", "builtins.open", "io.open"}
            if any(keyword.arg is None for keyword in node.keywords) or any(isinstance(arg, ast.Starred) for arg in node.args):
                scripts.clear()
                return
            mode = argument(node, 0 if bound else 1, "mode")
            if mode is None or value(mode) in {"r", "rb", "rt", "U", "rU"}:
                continue
            target = node.func.value if bound else argument(node, 0, "file")
        elif name in {"os.open", "os.remove", "os.unlink", "os.rmdir", "sqlite3.connect"}:
            target = argument(node, 0, "path")
        elif method in {"write_text", "write_bytes", "touch", "unlink", "rmdir"}:
            target = node.func.value
        elif name in PROCESS_CALLS | OPAQUE_CALLS | {"getattr", "globals", "locals", "__import__"} or (
                python_may_relocate_links(node, name)
                or name.startswith(("builtins.", "io.", "shutil.", "tempfile.", "importlib.", "sys.argv.", "sys.path."))
                or name.startswith(("os.", "posix.")) and not name.startswith("os.path.")):
            scripts.clear()
            return
        else:
            continue
        path = value(target)
        if path is None:
            scripts.clear()
            return
        scripts.opaque_write(path, cwd)


def screen_script_writes(scripts, visible, own_workspace, commits, single_commit, cwd):
    for path, text in visible.items():
        program = script_language(path, text)
        if program is None:
            scripts.opaque_write(path, cwd)
            continue
        if program in {"sh", "bash"}:
            hits = list(shell_hits(text, own_workspace, commits, single_commit, cwd=cwd))
        else:
            hits = list(interpreter_hits(program, text, own_workspace, commits, single_commit, cwd))
        fresh = scripts.starts_empty(path, cwd)
        yield from (hit for hit in hits if fresh or hit[0] != "interpreter-screened")
        if any(name not in PASSING_HIT_KINDS for name, _ in hits):
            scripts.opaque_write(path, cwd)
        elif fresh and (key := script_key(path, cwd)) is not None:
            previous = scripts.fresh.get(key)
            if previous is None or same_interpreter(previous[0], program):
                versions = () if previous is None else previous[1]
                if text not in versions:
                    scripts.fresh[key] = (program, (*versions, text))
            else:
                scripts.opaque_write(path, cwd)


def recorded_script(words, scripts, written, cwd):
    if not words:
        return None
    path, *args = words
    program = posixpath.basename(path)
    args = interpreter_arguments(program, args)
    if program in INLINE_OPTIONS or program in {"sh", "bash"}:
        program = system_program(path, written)
        if not args or args[0].startswith("-"):
            return None
        path, *args = args
        if (confirmed := scripts.get(script_key(path, cwd))) is not None:
            sources = (confirmed,)
        else:
            sources = scripts.possible_sources(path, cwd, program)
    elif path.startswith(("./", "/", "../")):
        if program in SYSTEM_PROGRAM_NAMES:
            system_program(path, written)
        confirmed = scripts.get(script_key(path, cwd))
        if confirmed is None:
            sources = scripts.possible_sources(path, cwd, None)
            if not sources or any(not source.startswith("#!") for source in sources):
                return None
        else:
            sources = (confirmed,)
        interpreters = set()
        for source in sources:
            shebang = source.splitlines()[0][2:] if source.startswith("#!") else "/bin/sh"
            argv = shlex.split(shebang)
            if argv[:1] == ["/usr/bin/env"]:
                argv = argv[1:]
            interpreter = system_program(argv[0], written) if argv else ""
            if interpreter not in INLINE_OPTIONS and interpreter not in {"sh", "bash"}:
                raise InspectionRequired("unrecognized recorded script interpreter")
            if interpreter_arguments(interpreter, argv[1:]):
                raise InspectionRequired("unsupported recorded script interpreter options")
            interpreters.add(interpreter)
        if len(interpreters) != 1:
            raise InspectionRequired("recorded script versions use different interpreters")
        program, = interpreters
    else:
        return None
    if not sources or re.match(r"(?:\./)?(?:tests?|e2e|scripts)/", path):
        return None  # Keep the build and test command screen in force.
    return program, path, args, sources


def shell_hits(command, own_workspace, commits, single_commit, *, scripts=None, written=None, cwd="", home=None,
               running_scripts=frozenset(), edit_paths=None, successful_writes=None):
    if scripts is None:
        scripts = ScriptEvidence()
    if written is None:
        written = set()
    # A newly recorded script is provisional until every intervening
    # connector is && or the tool's success confirms the final && chain.
    staged = set()
    forked = False
    try:
        for words, redirections, source, start_dir, leading, trailing in shell_commands(command, cwd, home):
            if leading != "&&":
                for key in staged:
                    scripts.pop(key, None)
                staged.clear()
                forked = leading in {"||", "|"}
            forked |= trailing in {"||", "|"}
            new_writes = None if forked else staged
            for _, path in redirections:
                yield from path_hits(path, own_workspace, start_dir)
            if not words and source is None:
                record_script_write(scripts, written, words, redirections, source, start_dir)
                continue
            try:
                if source is None:
                    script = recorded_script(words, scripts, written, start_dir)
                    visible = record_script_write(scripts, written, words, redirections, source, start_dir, new_writes)
                    yield from screen_script_writes(scripts, visible, own_workspace, commits, single_commit, start_dir)
                    if words[0] == "cd":
                        scripts.forget_ambiguous()
                    if script is None:
                        yield from command_hits(words, own_workspace, commits, single_commit,
                                                scripts=scripts, written=written, cwd=start_dir, home=home,
                                                running_scripts=running_scripts, edit_paths=edit_paths,
                                                successful_writes=new_writes)
                    else:
                        program, path, arguments, script_sources = script
                        yield from path_hits(path, own_workspace, start_dir)
                        for arg in arguments:
                            yield from source_literal_hits(arg, own_workspace, start_dir)
                        if program in {"sh", "bash"}:
                            key = script_key(path, start_dir)
                            if key in running_scripts:
                                raise InspectionRequired("recursive recorded script")
                            for script_source in script_sources:
                                yield from shell_hits(script_source, own_workspace, commits, single_commit,
                                                      scripts=scripts, written=written, cwd=start_dir, home=home,
                                                      running_scripts=running_scripts | {key},
                                                      edit_paths=edit_paths)
                            yield "interpreter-screened", program
                        else:
                            for script_source in script_sources:
                                yield from interpreter_hits(program, script_source, own_workspace, commits, single_commit,
                                                            start_dir, written, edit_paths=edit_paths)
                                interpreter_writes(scripts, program, script_source, start_dir, [path, *arguments])
                else:
                    visible = record_script_write(scripts, written, words, redirections, source, start_dir, new_writes)
                    yield from screen_script_writes(scripts, visible, own_workspace, commits, single_commit, start_dir)
                    if words == ["cat"]:
                        system_program(words[0], written)
                        continue  # Here the body is literal data.
                    if words and posixpath.basename(words[0]) == "tee":
                        yield from command_hits(words, own_workspace, commits, single_commit, written=written, cwd=start_dir)
                        continue
                    program = system_program(words[0], written) if words else ""
                    arguments = interpreter_arguments(program, words[1:])
                    if program not in INLINE_OPTIONS or arguments and arguments[0] != "-":
                        raise InspectionRequired("unsupported command receiving a heredoc")
                    yield from path_hits(words[0], own_workspace, start_dir)
                    for arg in arguments[1:]:
                        yield from source_literal_hits(arg, own_workspace, start_dir)
                    yield from interpreter_hits(program, source, own_workspace, commits, single_commit,
                                                        start_dir, written, edit_paths=edit_paths)
                    interpreter_writes(scripts, program, source, start_dir, ["-", *arguments[1:]])
            except InspectionRequired as error:
                scripts.clear()
                yield "needs-inspection", f"{error}: {shlex.join(words)[:400]}"
    except ValueError as error:
        scripts.clear()
        yield "needs-inspection", f"{error}: {command[:400]}"
    if successful_writes is not None:
        successful_writes.update(staged)
    else:
        for key in staged:
            scripts.pop(key, None)


PATH_KEYS = {"cwd", "path", "paths", "file_path", "filePath", "directory", "workdir", "target_file"}


def pilot_hits(call, own_workspace, commits, single_commit, *, scripts, written, cwd, home,
               edit_paths, succeeded=False):
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

    base = cwd or ""
    if isinstance(raw, dict) and (directory := raw.get("cwd") or raw.get("workdir")):
        cwd = (posixpath.join(base, directory) if edit_paths is not None
               else one_or_many(resolve_paths(directory, base)))
    if edit_paths is not None and kind == "move":
        edit_paths.blocked = True
    target_seen = False
    for fields in [raw, *(call.get("locations") or [])]:
        if isinstance(fields, dict):
            for key in PATH_KEYS & fields.keys():
                paths = fields[key] if isinstance(fields[key], list) else [fields[key]]
                for path in paths:
                    if isinstance(path, str):
                        yield from path_hits(path, own_workspace, base if key in {"cwd", "workdir"} else cwd or "")
                        if key not in {"cwd", "workdir"} and path:
                            target_seen = True
                            if kind in {"edit", "delete", "move"}:
                                written.add(posixpath.basename(path))
                                forget_script(scripts, path, cwd or "")
    if kind in {"edit", "delete", "move"} and not target_seen:
        scripts.clear()

    listing = kind == "read" and (title == "List files" or title.startswith("List files "))
    if listing:
        if match := re.fullmatch(r"""List files in (['"])(.+)\1""", title):
            yield from path_hits(match[2], own_workspace, cwd or "")
            target_seen = True
    elif kind == "read" and not (target_seen or raw or call.get("locations")):
        if match := re.fullmatch(r"Read file ['\"](.*)['\"]", title):
            yield from path_hits(match[1], own_workspace, cwd or "")
        else:
            yield "needs-inspection", f"missing read path: {title[:400]}"

    if tool == "apply_patch":
        patch = raw.get("patch") if isinstance(raw, dict) else raw
        if isinstance(patch, str) and patch.startswith("*** Begin Patch\n") and patch.rstrip().endswith("*** End Patch"):
            paths = re.findall(r"^\*\*\* (?:Add File|Update File|Delete File|Move to): (.+)$", patch, re.M)
            if paths:
                if edit_paths is not None and re.search(r"^\*\*\* Move to: ", patch, re.M):
                    edit_paths.blocked = True
                for path in paths:
                    yield from path_hits(path, own_workspace, cwd or "")
                    written.add(posixpath.basename(path))
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
                if call.get("name") == "exec_command":
                    # Earlier exec_command titles shell-quoted the entire
                    # source, sometimes using adjacent fragments around '!'.
                    try:
                        parts = shlex.split(title)
                    except ValueError:
                        parts = []
                    if len(parts) == 1:
                        command = parts[0]
    if isinstance(command, list) and command and all(isinstance(word, str) for word in command):
        command = shlex.join(command)
    if isinstance(command, str) and command.strip() and (kind in {None, "execute", "other"} or listing):
        yield from shell_hits(command, own_workspace, commits, single_commit, scripts=scripts, written=written,
                              cwd=cwd or "", home=home, edit_paths=edit_paths,
                              successful_writes=set() if succeeded else None)
    elif listing and not target_seen:
        yield "needs-inspection", f"missing read path: {title[:400]}"
    elif kind == "execute" or title in {"rql_query", "mcp__rql__rql_query"}:
        yield "needs-inspection", f"missing or unsupported command input: {title[:400]}"


def audit_pilot(transcript_path, own_workspace, *, commits=(), allow_rql=False, single_commit=False,
                cwd=None, home=None, allow_in_place_edits=False):
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
        inventory = transcript_path.with_name("pre-inventory.json")
        before = json.loads(inventory.read_text()) if inventory.is_file() else {}
        cwd = cwd or before.get("cwd")
        result = scan(pages, own_workspace, commits=commits, allow_rql=allow_rql, single_commit=single_commit,
                      cwd=cwd, home=home, allow_in_place_edits=allow_in_place_edits,
                      tracked_symlinks=before.get("tracked_symlinks"),
                      tracked_paths=None if before.get("status") or before.get("ignored") else before.get("tracked_paths"),
                      rollout=rollout_commands(
                          sorted((transcript_path.parent / ROLLOUT_DIRECTORY).glob("rollout-*.jsonl"))))
    except (OSError, ValueError, KeyError, TypeError) as error:
        result = {"tool_items": 0, "hits": [{"kind": "missing-or-unreadable-transcript", "snippet": str(error)}]}
    return dict(result, passed=all(hit["kind"] in PASSING_HIT_KINDS for hit in result["hits"]))


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
