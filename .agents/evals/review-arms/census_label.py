"""Run the independent Stage 1 RQL census. Use --inspect for cohort counts."""
import argparse
from collections import Counter, defaultdict
from datetime import datetime, timezone
import fcntl
from functools import cache
import hashlib
import json
import posixpath
from pathlib import Path
import re
import shutil
import sys
import subprocess
from tempfile import mkdtemp
import time

import driver
from label_facts import HERE, batches, indexed, save

CENSUS = HERE / "brief/rql-census"
LABELS = CENSUS / "labels"
RUST = Path("/mnt/optane/tmp/rust10")
# HEAD of the Mjolnir development batches launched by census runner 95af720f8,
# verified in census-label-work/development-primary-6cq5biub/repo and the following batch.
MJOLNIR_REVISION = "fb0a4c0ac5cfe0396e1bce2740b6a1148c47fdb9"
# Missing-step kinds from label-SPEC.md; the menu ranks these.
MISSING_KINDS = ("value_flow", "field_writes", "call_arguments", "guarded_path", "state_lifecycle",
                 "keyed_matching", "contract", "other")
ROLES = {"primary": ("codex", "gpt-6.1-sol", "gpt"), "replication": ("claude7", "opus", "opus")}
INPUTS = ("id", "repo", "intro_commit", "base_commit", "fix_commit", "defect", "trigger", "location")

LABELLER_EFFORT = "high"
LABEL_PROMPT = (
    "Read .rql-census/label-SPEC.md and .rql-census/batch.json. "
    "Follow the specification as the {role} labeller. The RQL reference is next to the spec. "
    "Write the batch's JSON lines to .rql-census/labels.jsonl. "
    "Only edit that output file. Keep the checkout untouched and use Git read-only. "
    "Read only the input files and Git history/code for the listed items. "
    "Some older records lack a trigger, location, or named test. Use the commits as evidence. "
    "For development items use the reduced task, with null anchoring. Use nested RQL "
    "S-expressions for documented queries, with wrapper options before the nested input. "
    'Example: (callers :depth 1 (enclosing-decl (function :name "handle"))).'
)
REPAIR_PROMPT = (
    "Correct the census output once. Write only rows for these requested IDs to "
    ".rql-census/repaired.jsonl. Omit every other row. "
    "If there are no requested IDs, write an empty file to acknowledge the rejected lines.\n"
    "Requested IDs: {ids}\nValidation errors:\n{errors}\n"
)


def canonical_site(site):
    """One repository-relative spelling of a path:line citation, so ./src/a.rs:4 and
    src/a.rs:4 count as the same site."""
    path, _, line = site.rpartition(":")
    return f"{posixpath.normpath(path)}:{line}" if path else site


def rows(path):
    with path.open() as source:
        for number, line in enumerate(source, 1):
            if line.strip():
                # An interrupted append leaves at most one unterminated final line. Treat it as
                # incomplete even if it parses, as append_rows does when it drops it, so its item
                # is labelled again instead of being counted and then removed.
                if not line.endswith("\n"):
                    print(f"{path}:{number}: skipping an incomplete final line", file=sys.stderr)
                    return
                try:
                    yield json.loads(line)
                except ValueError as error:
                    raise ValueError(f"{path}:{number}: {error}") from error


def item_sets():
    items = {"development": [], "test": []}
    pins = {}
    unconfirmed_dev = Counter()
    for repo, path in (("mjolnir", "corpus/corpus.jsonl"), ("bifrost-dev", "corpus-bifrost/corpus.jsonl")):
        items["development"].extend(dict(row, repo=repo) for row in rows(HERE / path))
    for path in sorted((HERE / "corpus-rust10").glob("*/test.jsonl")):
        directory = path.parent
        repo = directory.name.replace("__", "/")
        pins[repo] = json.loads((directory / "trace-order.json").read_text())["pin"]
        for row in rows(directory / "dev.jsonl"):
            if row["second_check"] == "confirmed":
                items["development"].append(dict(row, repo=repo))
            else:
                unconfirmed_dev[row["second_check"]] += 1
        items["test"].extend(dict(row, repo=repo) for row in rows(path) if row["second_check"] == "confirmed")
    return items, pins, unconfirmed_dev


ISSUE_REPO = r"[\w.-]+/[\w.-]+"
FIXES_ISSUE = re.compile(
    r"(?i)\b(?:fix(?:e[sd])?|clos(?:e[sd]?|ing)|resolv(?:e[sd]?|ing))\s+"
    rf"(?:issue\s+)?(?:https://github\.com/(?P<url_repo>{ISSUE_REPO})/issues/|"
    rf"(?:(?P<repo>{ISSUE_REPO}))?#)(?P<number>\d+)\b"
)


@cache
def fix_message_issues(source, revision, repo):
    message = git(source, "show", "-s", "--format=%B", revision).decode(errors="replace")
    return {int(match["number"]) for match in FIXES_ISSUE.finditer(message)
            if (match["url_repo"] or match["repo"] or repo).lower() == repo.lower()}



def issue_ids(item, source):
    result = set()
    for field in ("issue", "issue_id", "issue_number", "issue_url", "group_key"):
        match = re.fullmatch(r"(?:#|issue:|https?://[^/]+/.+/issues/)?(\d+)/?", str(item.get(field)))
        if match:
            result.add(int(match[1]))
    return result | fix_message_issues(source, item["fix_commit"], item["repo"])


def exclude_families(items, check_clones):
    by_repo = defaultdict(list)
    for row in items["development"]:
        by_repo[row["repo"]].append(row)
    kept, excluded = [], []
    for row in items["test"]:
        matches = []
        source = check_clones / row["repo"].replace("/", "_")
        family = by_repo[row["repo"]]
        issues = issue_ids(row, source) if family else set()
        for dev in family:
            reasons = []
            if issues & issue_ids(dev, source):
                reasons.append("same issue")
            if row["intro_commit"] == dev["fix_commit"]:
                reasons.append("introduced by development fix")
            if reasons:
                matches.append({"development_id": dev["id"], "reasons": reasons})
        if matches:
            excluded.append({"repo": row["repo"], "id": row["id"], "matches": matches})
        else:
            kept.append(row)
    items["test"] = kept
    return excluded


def labeller_item(row, split):
    return {**{key: row.get(key) for key in INPUTS}, "census": split,
            "fix_test": row.get("fix_test") or row.get("regression_test") or row.get("marker")}


def input_documents():
    return {name: (CENSUS / name).read_bytes()
            for name in ("label-SPEC.md", "rql-documented-steps.md")}


def inputs_sha(item, documents, role, head):
    # The repair template is stable across resumes. Its requested IDs and
    # validation errors are filled in from the first turn's submitted output.
    metadata = {"item": item, "head": head, "model": ROLES[role][1], "effort": LABELLER_EFFORT,
                "label_prompt": LABEL_PROMPT.format(role=role), "repair_prompt": REPAIR_PROMPT}
    encoded_inputs = json.dumps(metadata, sort_keys=True, separators=(",", ":")).encode("utf-8")
    digest = hashlib.sha256()
    for part in (*documents.values(), encoded_inputs):
        # Frame each input so a document boundary is part of the hash.
        digest.update(len(part).to_bytes(8, "big"))
        digest.update(part)
    return digest.hexdigest()


def read_current_labels(path, expected_hashes):
    current, stale = {}, Counter()
    for row in rows(path) if path.exists() else ():
        key = row["repo"], row["id"]
        if key not in expected_hashes or row.get("inputs_sha") != expected_hashes[key]:
            stale[key[0]] += 1
            continue
        if key in current:
            raise ValueError(f"{path}: duplicate current repo/id {key}")
        current[key] = row
    return current, stale


def git(repo, *args):
    return subprocess.check_output(["git", "--no-replace-objects", "-C", str(repo), *args],
                                   stderr=subprocess.PIPE)


@cache
def line_count(repo, revision, path):
    try:
        blob = git(repo, "cat-file", "blob", f"{revision}:{path}")
    except (subprocess.CalledProcessError, ValueError):
        return 0
    return blob.count(b"\n") + bool(blob and not blob.endswith(b"\n"))


def vocabulary(reference=None):
    """Load spellings, row signatures and option requirements from the catalogue."""
    forms = {}
    if reference is None:
        reference = (CENSUS / "rql-documented-steps.md").read_text()
    for line in reference.splitlines():
        if not line.startswith("| `"):
            continue
        first, second, signature, details, *_ = (cell.strip() for cell in line.split("|")[1:])
        required, selectors = [], []
        if second.startswith("`"):
            names, use = second, "step"
            for clause in details.split(";"):
                match = re.search(r"\((?:both |all )?required\)", clause)
                if match:
                    groups, text = required, clause[:match.start()]
                elif clause.strip().startswith("exactly one of "):
                    groups, text = selectors, clause
                else:
                    continue
                aliases = [re.findall(r"`(:[\w-]+)`", group) for group in text.split(",")]
                if not all(aliases):
                    raise ValueError(f"Malformed documented options for {names}: {clause}")
                groups.extend(aliases)
        else:
            names, use = first, second
        inputs, separator, output = signature.partition(" -> ")
        if not separator and signature != "n/a":
            raise ValueError(f"Malformed documented signature for {names}: {signature}")
        for name in re.findall(r"`([\w/-]+)`", names):
            if name in forms:
                raise ValueError(f"Duplicate documented form {name}")
            forms[name] = use, inputs, output, required, selectors
    if not forms:
        raise ValueError("No documented RQL forms")
    return forms


TOKEN = re.compile(r'"(?:\\.|[^"\\])*"|;[^\n]*|[()\[\]]|[^\s"();\[\]]+')


def parse_query(query):
    stack, roots = [], []
    end = 0
    for match in TOKEN.finditer(query):
        if query[end:match.start()].strip():
            raise ValueError("malformed RQL")
        end = match.end()
        token = match[0]
        if token.startswith(";"):
            continue
        items = stack[-1][1] if stack else roots
        if token in ("(", "["):
            stack.append((token, []))
        elif token in (")", "]"):
            if not stack or (stack[-1][0], token) not in {("(", ")"), ("[", "]")}:
                raise ValueError("unbalanced RQL delimiters")
            opener, child = stack.pop()
            parent = stack[-1][1] if stack else roots
            parent.append(child if opener == "(" else tuple(child))
        else:
            items.append(token)
    if stack or query[end:].strip() or len(roots) != 1 or not isinstance(roots[0], list):
        raise ValueError("expected one RQL form")
    return roots[0]


def row_kinds(signature):
    return set(re.findall(r"`(\w+)`", signature))


def proposed_signature(step, allowed):
    known_kinds = {kind for _, inputs, output, _, _ in allowed.values()
                   for signature in (inputs, output) for kind in row_kinds(signature)}

    def signature(value):
        # Declarations may give bare or backticked domain names. Free-form
        # descriptions do not establish a domain for the type checker.
        kinds = re.split(r"\s*(?:,\s*(?:or\s+)?|\bor\b|\|)\s*", value.strip().replace("`", ""))
        if set(kinds) <= known_kinds:
            return " or ".join(f"`{kind}`" for kind in kinds)
        return "unknown"

    return "step", signature(step["input"]), signature(step["output"]), (), ()


def check_query(query, allowed, *, query_heads=None):
    """Check documented form names and propagate their known terminal row kinds."""
    errors = []
    untyped = False

    def form_signature(form):
        if (not isinstance(form, list) or not form or not isinstance(form[0], str)
                or not re.fullmatch(r"[\w/-]+", form[0])):
            raise ValueError("RQL form needs a head symbol")
        name = form[0]
        if name not in allowed:
            errors.append(f"undocumented RQL form {name!r}")
            unknown_arguments(form[1:])
            return "unknown", "", "", (), ()
        return allowed[name]

    def unknown_arguments(args):
        # An undocumented head cannot establish which operands are queries or
        # options. Inspect nested lists, including those inside vectors.
        for arg in args:
            if isinstance(arg, list) and arg and isinstance(arg[0], str) and re.fullmatch(r"[\w/-]+", arg[0]):
                infer(arg)
            elif isinstance(arg, (list, tuple)):
                unknown_arguments(arg)

    def arguments(form):
        positional, options = [], {}
        args = iter(form[1:])
        for arg in args:
            if isinstance(arg, str) and arg.startswith(":"):
                value = next(args, None)
                if value is None or (isinstance(value, str) and value.startswith(":")):
                    raise ValueError(f"{form[0]}: {arg} needs a value")
                if arg in options:
                    raise ValueError(f"{form[0]}: duplicate option {arg}")
                options[arg] = value
            else:
                positional.append(arg)
        return positional, options

    def embedded(value):
        if isinstance(value, list):
            pattern(value, nested=True)
        elif isinstance(value, tuple):
            for item in value:
                embedded(item)

    def pattern(form, *, nested=False):
        use, *_ = form_signature(form)
        if use == "unknown":
            return
        if use != "pattern" and not (nested and use == "operand"):
            errors.append(f"{form[0]}: expected a structural pattern")
            return
        args = iter(form[1:])
        for arg in args:
            if arg == ":kwargs":
                entries = next(args, None)
                if isinstance(entries, tuple):
                    for entry in entries:
                        if isinstance(entry, list) and len(entry) == 2:
                            embedded(entry[1])
            else:
                embedded(arg)

    def option_forms(options):
        # The pinned sexp.rs decoder passes :enclosing to pattern_to_json
        # for references-of and edge traversal. The other list-valued
        # operands below have their own documented, contextual form heads.
        if ":enclosing" in options:
            pattern(options[":enclosing"])

        def operand(value, heads):
            if isinstance(value, list) and value and isinstance(value[0], str):
                head = value[0]
                if re.fullmatch(r"[A-Za-z_][\w/-]*", head) and head not in heads:
                    errors.append(f"undocumented RQL form {head!r}")
                for child in value[1:]:
                    operand(child, heads)
            elif isinstance(value, (list, tuple)):
                for child in value:
                    operand(child, heads)

        # reference/code-query.md:839,981–985
        for option in (":receiver-type", ":receiver_type"):
            operand(options.get(option), {"assignable-to", "assignable_to"})
        for option in (":route", ":routes"):
            operand(options.get(option), {"key", "index", "any"})

        # A row predicate's first item is a field name. Only the operand
        # can introduce a form ((field NAME) or (enum LABEL), reference:767).
        predicates = options.get(":where")
        if isinstance(predicates, (list, tuple)):
            for predicate in predicates:
                if isinstance(predicate, (list, tuple)) and len(predicate) == 3:
                    _, operator, value = predicate
                    if operator == "in" and isinstance(value, (list, tuple)):
                        for member in value:
                            operand(member, {"enum"})
                    else:
                        operand(value, {"field", "enum"})

    def compatible(context, actual, expected):
        kinds = row_kinds(expected)
        if kinds and len(actual) > 1:
            errors.append(f"{context}: input needs one row kind, got {', '.join(sorted(actual))}")
        elif actual and kinds and actual - kinds:
            readable = expected.replace("`", "")
            errors.append(f"{context}: expected {readable}, got {', '.join(sorted(actual))}")

    def infer(form):
        nonlocal untyped
        use, inputs, output, required, selectors = form_signature(form)
        name = form[0]
        if query_heads is not None:
            query_heads.add(name)
        if use == "unknown":
            return set(), None
        if use == "pattern":
            pattern(form)
            return row_kinds(output), True
        if use not in ("step", "source", "set", "ancestor", "compound"):
            errors.append(f"{name}: expected a query form")
            return set(), None
        args, options = arguments(form)
        option_forms(options)
        missing = [aliases[0] for aliases in required if not any(option in options for option in aliases)]
        if missing:
            errors.append(f"{name}: missing required options {', '.join(missing)}")
        if selectors and sum(option in options for aliases in selectors for option in aliases) != 1:
            errors.append(f"{name}: expected exactly one of {', '.join(aliases[0] for aliases in selectors)}")
        if use == "ancestor":
            if len(args) != 2:
                raise ValueError(f"{name}: expected an ancestor pattern and a nested query")
            pattern(args[0])
            kinds, structural_seed = infer(args[1])
            if structural_seed is False:
                errors.append(f"{name}: structural containment requires a structural pattern seed")
            return kinds, structural_seed
        input_kinds = set()
        # Dedicated sources, set operators and compound sources do not lower
        # to a match source, even when their operands start with one.
        structural_seed = False
        if use == "step":
            if len([arg for arg in args if isinstance(arg, list)]) != 1:
                raise ValueError(f"{name}: expected one nested query")
            nested = next((arg for arg in reversed(args) if isinstance(arg, list)), None)
            input_kinds, structural_seed = infer(nested)
            compatible(name, input_kinds, inputs)
        elif use == "set":
            if len(args) < 2:
                raise ValueError(f"{name}: expected at least two query operands")
            for arg in args:
                kinds, _ = infer(arg)
                input_kinds.update(kinds)
            if name != "union" and len(input_kinds) > 1:
                errors.append(f"{name}: operands need the same row kind, got "
                              f"{', '.join(sorted(input_kinds))}")
        elif use == "compound":
            expected = dict(part.strip().split(" = ") for part in inputs.split(";"))
            if name == "correlate":
                relation = options.get(":relation")
                relation_use, relation_inputs, *_ = form_signature(relation)
                if relation_use == "relation":
                    key, signature = relation_inputs.split(" = ")
                    expected[key] = signature
                    if relation[0] in ("configuration-code", "configuration_code"):
                        _, relation_options = arguments(relation)
                        if relation_options.get(":match") != "same-project":
                            expected[key] = "`declaration`"
                elif relation_use != "unknown":
                    errors.append(f"{name}: expected a correlation relation")
            for option, signature in expected.items():
                actual, _ = infer(options.get(option))
                compatible(f"{name} {option}", actual, signature)
                if signature == "any code row" and actual & {
                        "configuration_fact", "dependency_evidence", "cross_domain"}:
                    errors.append(f"{name} {option}: expected a programming-code query")
        if output == "no rows":
            errors.append(f"{name}: describes a plan without executing and returns no rows")
            return set(), structural_seed
        if inputs == "unknown" or output == "unknown":
            untyped = True
        if output == "unknown":
            return set(), structural_seed
        if output == "same row":
            return input_kinds, structural_seed
        if name == "witness":
            # The catalogue gives the outputs in the same order as the inputs.
            kinds = {kind for source, kind in zip(re.findall(r"`(\w+)`", inputs),
                                                   re.findall(r"`(\w+)`", output))
                     if source in input_kinds}
        elif output == "projected row":
            kinds = {"projected_row"}
        else:
            kinds = row_kinds(output)
        return kinds, structural_seed

    try:
        infer(parse_query(query))
    except ValueError as error:
        errors.append(str(error))
    except RecursionError:
        errors.append("RQL nesting exceeds validator capacity")
    return list(dict.fromkeys(errors)), "untyped" if untyped else "typed"


def string(value):
    return isinstance(value, str) and bool(value.strip())


def fields(value, required, where, errors):
    if not isinstance(value, dict):
        errors.append(f"{where}: expected object")
        return False
    missing = sorted(set(required.split()) - value.keys())
    if missing:
        errors.append(f"{where}: missing {', '.join(missing)}")
    return not missing


def validate(label, item, role, repo, allowed):
    errors, query_checks = [], []
    if not fields(label, "id repo labeller defect_check defect_check_reason decisive_sets relation_bug notes",
                  "label", errors):
        return errors, query_checks
    if (label["id"], label["repo"], label["labeller"]) != (item["id"], item["repo"], role):
        errors.append("id, repo and labeller must match the batch")
    check, reason = label["defect_check"], label["defect_check_reason"]
    if not ((check == "confirmed" and reason is None) or (check == "not_confirmed" and string(reason))):
        errors.append("defect_check and defect_check_reason are inconsistent")
    if not isinstance(label["notes"], str):
        errors.append("notes must be a string")
    sets = label["decisive_sets"]
    if not isinstance(sets, list) or not sets or (item["census"] == "development" and len(sets) != 1):
        errors.append("decisive_sets must contain one set for development, or one or more for test")
        return errors, query_checks
    has_relation = False
    for number, decisive in enumerate(sets, 1):
        if not isinstance(decisive, dict) or not isinstance(decisive.get("facts"), list) or not decisive["facts"]:
            errors.append(f"set {number}: expected nonempty facts")
            continue
        for index, fact in enumerate(decisive["facts"], 1):
            where = f"set {number} fact {index}"
            if not fields(fact, "claim sites in_diff relation kind rql anchoring", where, errors):
                continue
            if not string(fact["claim"]):
                errors.append(f"{where}: claim must be a nonempty string")
            sites = fact["sites"]
            if not isinstance(sites, list) or not sites or any(not isinstance(site, str) for site in sites):
                errors.append(f"{where}: sites must be a nonempty list of path:line strings")
            else:
                for site in sites:
                    path, colon, line = site.rpartition(":")
                    if (not colon or not path or not re.fullmatch(r"[1-9][0-9]{0,19}", line)
                            or int(line) > line_count(repo, item["intro_commit"], path)):
                        errors.append(f"{where}: {site!r} does not exist at intro_commit")
                if fact["relation"] is True and len({canonical_site(site) for site in sites}) < 2:
                    errors.append(f"{where}: relation requires at least two distinct sites")
            if type(fact["relation"]) is not bool or type(fact["in_diff"]) is not bool:
                errors.append(f"{where}: relation and in_diff must be booleans")
            has_relation |= fact["relation"] is True
            validate_fact(fact, item["census"], where, errors, query_checks, allowed)
    if label["relation_bug"] is not has_relation:
        errors.append("relation_bug must equal whether any decisive fact has relation=true")
    return errors, query_checks


def validate_fact(fact, split, where, errors, query_checks, allowed):
    kind, rql, anchoring = fact["kind"], fact["rql"], fact["anchoring"]
    if fact["in_diff"] is True:
        if any(value is not None for value in (kind, rql, anchoring)):
            errors.append(f"{where}: an in-diff fact must have null kind, rql and anchoring")
        return
    if kind not in ("callers", "path", "value_flow", "shape", "other"):
        errors.append(f"{where}: invalid kind")
    rql_keys = "class query missing_step reason locator_query"
    if not fields(rql, rql_keys, f"{where} rql", errors):
        return
    if extra := sorted(rql.keys() - rql_keys.split()):
        errors.append(f"{where} rql: unexpected {', '.join(extra)}")
    cls, query, step, reason, locator = (rql[key] for key in rql_keys.split())
    if locator is not None:
        if not string(locator):
            errors.append(f"{where}: locator_query must be a nonempty string or null")
        else:
            problems, status = check_query(locator, allowed)
            errors.extend(f"{where} locator_query: {problem}" for problem in problems)
            if not problems:
                query_checks.append({"fact": where, "field": "locator_query", "status": status})
    if cls == "not_expressible":
        if reason not in ("intent", "external", "other") or any(
                value is not None for value in (query, step, anchoring)):
            errors.append(f"{where}: not_expressible needs a reason and null query, missing_step and anchoring")
        return
    if cls not in ("documented", "missing"):
        errors.append(f"{where}: invalid rql.class")
        return
    if reason is not None or not string(query):
        errors.append(f"{where}: {cls} needs a nonempty query and null reason")
    if cls == "documented":
        if step is not None:
            errors.append(f"{where}: documented requires null missing_step")
        if string(query):
            problems, status = check_query(query, allowed)
            errors.extend(f"{where}: {problem}" for problem in problems)
            if not problems:
                query_checks.append({"fact": where, "status": status})
    elif fields(step, "name kind input output semantics", f"{where} missing_step", errors):
        if any(not string(step[key]) for key in ("name", "input", "output", "semantics")):
            errors.append(f"{where}: missing_step fields must be nonempty strings")
        elif step["kind"] not in MISSING_KINDS:
            errors.append(f"{where}: missing_step kind must be one of {', '.join(MISSING_KINDS)}")
        else:
            name = step["name"].strip()
            spellings = (name, name.replace("_", "-"), name.replace("-", "_"))
            if any(spelling in allowed for spelling in spellings):
                errors.append(f"{where}: missing_step {name!r} is already documented")
            elif string(query):
                query_heads = set()
                extended = {**allowed, name: proposed_signature(step, allowed)}
                problems, status = check_query(query, extended, query_heads=query_heads)
                if name not in query_heads:
                    problems.append(f"query must invoke missing_step {name!r}")
                errors.extend(f"{where}: {problem}" for problem in problems)
                if not problems:
                    query_checks.append({"fact": where, "status": status})
    # Step 5 is outside the development task. The output example includes
    # anchoring, so accept a well-formed object there as well as null.
    if split == "development" and anchoring is None:
        return
    if fields(anchoring, "anchors_on_touched anchor hops", f"{where} anchoring", errors):
        if (type(anchoring["anchors_on_touched"]) is not bool or not string(anchoring["anchor"])
                or type(anchoring["hops"]) is not int or anchoring["hops"] < 0):
            errors.append(f"{where}: invalid anchoring types or hops")


def append_rows(path, labels):
    # Drop an unterminated fragment left by an interrupted append, so the new rows start on a
    # fresh line. Safe because one run at a time holds the labels directory's lock.
    if path.exists():
        with path.open("r+b") as stream:
            data = stream.read()
            if data and not data.endswith(b"\n"):
                stream.truncate(data.rfind(b"\n") + 1)
    with path.open("a", encoding="utf-8") as stream:
        stream.write("".join(json.dumps(row, ensure_ascii=True) + "\n" for row in labels))


def append(path, row):
    append_rows(path, [row])


def log(directory, event, **data):
    row = {"time": datetime.now(timezone.utc).isoformat(), "event": event, **data}
    append(directory / "run.jsonl", row)
    print(json.dumps(row), flush=True)


def collect(output, expected, role, repo, allowed):
    results = defaultdict(list)
    errors = defaultdict(list)
    for number, line in enumerate(output.splitlines(), 1):
        if not line.strip():
            continue
        try:
            label = json.loads(line)
        except ValueError as error:
            errors["<output>"].append(f"line {number}: {error}")
            continue
        bug = label.get("id") if isinstance(label, dict) else None
        if not isinstance(bug, str) or bug not in expected:
            errors["<output>"].append(f"line {number}: unexpected or missing id {bug!r}")
            continue
        results[bug].append(label)
    valid = {}
    for bug, item in expected.items():
        submitted = results[bug]
        if len(submitted) != 1:
            errors[bug].append(f"expected one row for {bug}, got {len(submitted)}")
            continue
        problems, query_checks = validate(submitted[0], item, role, repo, allowed)
        if problems:
            errors[bug].extend(problems)
        else:
            valid[bug] = dict(submitted[0], validation={"rql": query_checks})
    return valid, dict(errors)


def quota(args, role):
    profile, _, scope = ROLES[role]
    previous = None
    while hold := driver.quota_hold(scope, floor=f"{profile}=25", weekly=True, strict=True):
        if hold != previous:
            log(args.labels_dir, "quota-held", role=role, profile=profile, reason=hold)
            previous = hold
        if args.check_quota:
            return False
        time.sleep(60)
    log(args.labels_dir, "quota-cleared", role=role, profile=profile)
    return True


def checkout_revision(repo, pins):
    if repo == "mjolnir":
        return MJOLNIR_REVISION
    if repo == "bifrost-dev":
        return json.loads((HERE / "corpus-bifrost/FREEZE.json").read_text())["snapshot"]
    return pins[repo]


def checkout(args, repo, revision, unit):
    if repo == "mjolnir":
        source = args.mjolnir_clone
    elif repo == "bifrost-dev":
        source = args.bifrost_clone
    else:
        source = args.check_clones / repo.replace("/", "_")
    if git(source, "rev-parse", "--is-shallow-repository").strip() != b"false":
        raise ValueError(f"{source}: full Git history is required")

    # Give each launch its own checkout, config and refs. Copy objects so
    # this also works across the home and scratch filesystems.
    work = unit / "repo"
    try:
        git(source, "clone", "--local", "--no-hardlinks", "--no-checkout", str(source), str(work))
    except subprocess.CalledProcessError as error:
        raise RuntimeError(error.stderr.decode(errors="replace").strip()) from error
    git(work, "checkout", "-B", "rql-census-input", revision)
    git(work, "remote", "remove", "origin")
    return work


def run_turn(sid, prompt, phase, title):
    command_id = f"rql-{sid[:16]}-{phase}"
    response = driver.mj("message", "--session", sid, "--command-id", command_id,
                         "--prompt-file", prompt)
    waited = driver.mj("wait", "--session", sid, "--turn", response["turn_id"],
                       "--return-on-input", "--timeout", 1200, timeout=1230)
    requested_turn = waited.get("requested_turn_id")
    if requested_turn is None:
        requested_turn = waited.get("turn_id")
    if waited["outcome"] != "finished" or requested_turn != response["turn_id"]:
        raise RuntimeError(f"{title}: {waited}")
    return waited


def fetch_output(sid, path):
    # These retry delays are a judgment call, not a measured service limit.
    for delay in (0, 30, 60, 120):
        if delay:
            time.sleep(delay)
        try:
            fetch = subprocess.run([str(driver.MJX), "fetch", "--session", sid, "--path", path, "--stdout"],
                                   capture_output=True, timeout=300)
        except (OSError, subprocess.TimeoutExpired) as error:
            problem = str(error)
        else:
            if fetch.returncode == 0:
                return fetch.stdout.decode(errors="replace")
            problem = fetch.stderr.decode(errors="replace").strip() or f"exit status {fetch.returncode}"
            # The export layer names the missing path. A missing session or
            # an unrelated file error still belongs to fetch recovery.
            if f"{path} does not exist on the session's target" in problem:
                return None
    raise RuntimeError(problem)


def run_batch(args, split, role, repo, batch, revision, allowed, documents):
    profile, model, _ = ROLES[role]
    ids = [item["id"] for item in batch]
    if not args.work_dir.is_absolute():
        raise ValueError("--work-dir must be absolute")
    args.work_dir.mkdir(parents=True, exist_ok=True)
    unit = Path(mkdtemp(prefix=f"{split}-{role}-", dir=args.work_dir))
    work = checkout(args, repo, revision, unit)
    head = git(work, "rev-parse", "HEAD").decode().strip()
    if head != revision:
        raise RuntimeError(f"{repo}: checkout HEAD {head} differs from requested revision {revision}")
    for item in batch:
        for field in ("base_commit", "intro_commit", "fix_commit"):
            git(work, "cat-file", "-e", item[field] + "^{commit}")
    inputs = unit / "inputs"
    inputs.mkdir()
    for filename, content in documents.items():
        (inputs / filename).write_bytes(content)
    save(inputs / "batch.json", {"labeller": role, "output": "labels.jsonl", "items": batch})
    prompt = unit / "prompt.txt"
    prompt.write_text(LABEL_PROMPT.format(role=role), encoding="utf-8")
    quota(args, role)
    tag = hashlib.sha256(unit.name.encode()).hexdigest()[:8]
    title = f"RQL census {role} {repo} {tag}"
    log(args.labels_dir, "launching", title=title, role=role, split=split, repo=repo,
        ids=ids, unit=str(unit), profile=profile, model=model, effort=LABELLER_EFFORT)
    sid = driver.mj("new", "--workspace", args.workspace, "--target", "localhost",
                    "--project-directory", work, "--branch", "rql-census-input",
                    "--profile", profile, "--effort", "high",
                    "--review", "off", "--no-project-memory", "--no-github-events",
                    "--no-github-credentials", "--subagents", "none", "--title", title,
                    timeout=900)["session_id"]
    # A session with inaccessible output stays available for manual fetch.
    recover_output = False
    try:
        log(args.labels_dir, "launched", title=title, session=sid)
        driver.wait_idle(sid)
        for key, value in (("model", model), ("effort", LABELLER_EFFORT)):
            while True:
                try:
                    driver.mj("set-config", "--session", sid, "--key", key, "--value", value)
                    break
                except RuntimeError as error:
                    if "initialization is still running" not in str(error):
                        raise
                    time.sleep(10)
        state = driver.session(sid)
        selected = {field["key"]: field.get("current") for field in state["config_options"]}
        if state["profile_id"] != profile or selected.get("model") != model or selected.get("effort") != LABELLER_EFFORT:
            raise RuntimeError(f"{title}: requested {profile}/{model}/{LABELLER_EFFORT}, got {state['profile_id']}/{selected}")
        log(args.labels_dir, "configured", title=title, profile=profile, model=model, effort=LABELLER_EFFORT)
        for source in inputs.iterdir():
            driver.mj("put-file", "--session", sid, "--path", f".rql-census/{source.name}", source)
        expected = indexed(batch, "census batch")
        destination = args.labels_dir / f"{split}-{role}.jsonl"
        errors = {}
        for attempt, filename in enumerate(("labels.jsonl", "repaired.jsonl"), 1):
            turn = run_turn(sid, prompt, attempt, title)
            actual_profile = driver.session(sid)["profile_id"]
            if actual_profile != profile:
                log(args.labels_dir, "profile-moved", title=title, session=sid, attempt=attempt,
                    expected_profile=profile, actual_profile=actual_profile)
                raise RuntimeError(f"{title}: profile moved from {profile} to {actual_profile}")
            log(args.labels_dir, "turn-finished", title=title, attempt=attempt,
                turn=turn.get("turn_id"), outcome=turn.get("outcome"))
            output_path = f".rql-census/{filename}"
            try:
                raw = fetch_output(sid, output_path)
            except RuntimeError as error:
                recover_output = True
                recovery = {"session": sid, "title": title, "path": output_path, "unit": str(unit),
                            "repo": repo, "census": split, "labeller": role, "ids": list(expected),
                            "attempt": attempt, "turn": turn.get("turn_id"), "error": str(error)}
                append(args.labels_dir / "recover.jsonl",
                       {"time": datetime.now(timezone.utc).isoformat(), **recovery})
                log(args.labels_dir, "fetch-failed", **recovery)
                raise
            (unit / f"attempt-{attempt}.jsonl").write_text(raw or "")
            if raw is None:
                valid = {}
                errors = {bug: [f"output file missing: {output_path}"] for bug in expected or ("<output>",)}
            else:
                valid, errors = collect(raw, expected, role, work, allowed)
            if valid:
                append_rows(destination, (dict(row, inputs_sha=inputs_sha(expected[bug], documents, role, head))
                                          for bug, row in valid.items()))
            log(args.labels_dir, "validated", title=title, attempt=attempt, ids=list(valid), errors=errors)
            if not errors:
                break
            if attempt == 1:
                expected = {bug: item for bug, item in expected.items() if bug not in valid}
                prompt = unit / "repair.txt"
                repair = REPAIR_PROMPT.format(ids=json.dumps(list(expected)), errors=json.dumps(errors, indent=2))
                prompt.write_text(repair, encoding="utf-8")
        if errors:
            append_rows(args.labels_dir / "invalid.jsonl", [
                {"id": bug, "repo": repo, "census": split, "labeller": role, "errors": problems,
                 "raw_output": str(unit / "attempt-2.jsonl")}
                for bug, problems in errors.items()
            ])
    finally:
        if not recover_output:
            # The host unit retains the input and output evidence after suspension.
            try:
                driver.mj("suspend", "--session", sid, "--acknowledge-unpublished-work", timeout=900)
                waited = driver.mj("wait", "--session", sid, "--timeout", 900, timeout=930)
                session = waited["session"]
                if (waited["outcome"] != "stopped" or session["lifecycle"] != "suspended"
                        or session.get("error") is not None):
                    raise RuntimeError(f"{title}: suspension did not complete: {waited}")
            except Exception as error:
                log(args.labels_dir, "suspend-failed", title=title, session=sid, error=str(error))
                raise
    # Only the completed, suspended batch reaches here. Keep the evidence
    # beside the checkout, and release the full clone.
    shutil.rmtree(work)
    log(args.labels_dir, "batch-complete", title=title, invalid=list(errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--split", choices=("test", "development"))
    parser.add_argument("--labeller", choices=ROLES, required=False)
    parser.add_argument("--repo", help="run only this repo, e.g. 1jehuang/jcode")
    parser.add_argument("--limit", type=int, help="take only the first N selected items, before applying resume")
    parser.add_argument("--batch-size", type=int, default=5)
    parser.add_argument("--inspect", action="store_true", help="report exclusions and cohort counts without launching")
    parser.add_argument("--check-quota", action="store_true", help="log the labeller's quota gate without launching")
    parser.add_argument("--labels-dir", type=Path, default=LABELS)
    parser.add_argument("--work-dir", type=Path, default=RUST / "census-label-work")
    parser.add_argument("--workspace", default="Review arms")
    parser.add_argument("--check-clones", type=Path, default=RUST / "check-clones")
    parser.add_argument("--mjolnir-clone", type=Path, default=Path.home() / "Projects/mjolnir")
    parser.add_argument("--bifrost-clone", type=Path, default=RUST / "bifrost-src")
    args = parser.parse_args()
    if args.batch_size < 1 or (args.limit is not None and args.limit < 1):
        parser.error("batch size and limit must be positive")
    if not args.inspect and (not args.labeller or (not args.split and not args.check_quota)):
        parser.error("choose --labeller and --split, or use --inspect")
    items, pins, unconfirmed_dev = item_sets()
    excluded = exclude_families(items, args.check_clones)
    counts = {split: dict(Counter(item["repo"] for item in rows)) for split, rows in items.items()}
    documents = input_documents()
    allowed = vocabulary(documents["rql-documented-steps.md"].decode("utf-8"))
    query_forms = [output for use, _, output, _, _ in allowed.values() if use not in ("operand", "relation")]
    checked_forms = sorted(name for name, (_, _, _, required, selectors) in allowed.items()
                           if required or selectors)
    summary = {"rql_vocabulary": {"documented_forms": len(allowed), "query_forms": len(query_forms),
                                  "typed_query_forms": sum(output not in ("unknown", "no rows") for output in query_forms),
                                  "required_option_forms": checked_forms},
               "counts": counts, "totals": {split: sum(count.values()) for split, count in counts.items()},
               "excluded": excluded, "rust_development_not_confirmed": unconfirmed_dev}
    if args.inspect:
        print(json.dumps(summary, indent=2))
        return
    args.labels_dir.mkdir(parents=True, exist_ok=True)
    with (args.labels_dir / ".lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        log(args.labels_dir, "cohort", **summary)
        if args.check_quota:
            return 0 if quota(args, args.labeller) else 1
        selected = [labeller_item(row, args.split) for row in items[args.split]
                    if args.repo is None or args.repo == row["repo"]][:args.limit]
        if not selected:
            parser.error("no items match this selection")
        destination = args.labels_dir / f"{args.split}-{args.labeller}.jsonl"
        heads = {repo: checkout_revision(repo, pins) for repo in {item["repo"] for item in selected}}
        expected_hashes = {(item["repo"], item["id"]): inputs_sha(item, documents, args.labeller, heads[item["repo"]])
                           for item in selected}
        done, _ = read_current_labels(destination, expected_hashes)
        by_repo = defaultdict(list)
        for item in selected:
            if (item["repo"], item["id"]) not in done:
                by_repo[item["repo"]].append(item)
        for repo, subset in by_repo.items():
            indexed(subset, f"{args.split}/{repo}")
            for batch in batches(subset, args.batch_size):
                try:
                    run_batch(args, args.split, args.labeller, repo, batch, heads[repo], allowed, documents)
                except Exception as error:
                    log(args.labels_dir, "batch-failed", split=args.split, role=args.labeller,
                        repo=repo, ids=[item["id"] for item in batch], error=str(error))
                    raise
        # A run is complete only when every selected item has a valid label. Items that still
        # fail after the repair turn are in invalid.jsonl; report them as an unsuccessful run.
        labelled, _ = read_current_labels(destination, expected_hashes)
        unresolved = [item["id"] for item in selected if (item["repo"], item["id"]) not in labelled]
        if unresolved:
            print(f"{len(unresolved)} selected items have no valid label: {', '.join(unresolved[:20])}",
                  file=sys.stderr)
            return 1
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
