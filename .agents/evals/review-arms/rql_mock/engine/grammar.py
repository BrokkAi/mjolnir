"""Frozen catalogue and menu grammar for the mock engine."""
import re
from dataclasses import dataclass

from census_label import check_query, parse_query, vocabulary


@dataclass(frozen=True)
class MenuStep:
    output: str
    fields: tuple
    depth: tuple
    selectors: dict
    inputs: str = "`procedure`"


MENU = {
    "contract-cases": MenuStep(
        "contract_case", ("procedure", "kind", "site", "guards", "outcome", "dependencies", "calls"),
        (1, 4), {"kind": ("all", "return", "error", "panic", "callback")},
    ),
    "value-transfers": MenuStep(
        "value_transfer", ("procedure", "direction", "port", "selected_port", "source", "sink",
                           "transfers", "guards", "proof"),
        (2, 8), {"direction": ("upstream", "downstream"),
                 "port": ("return", "argument", "input", "write")},
    ),
    "guarded-paths": MenuStep(
        "guarded_path", ("procedure", "entry", "pivot", "event", "guards", "calls", "prior",
                         "held", "continuation"),
        (1, 8), {"direction": ("outgoing", "incoming"),
                 "event": ("all", "call", "write", "return", "raise")},
        "`procedure`, `call_site`, or `structural_match`",
    ),
}
MENU_ALIASES = {name.replace("-", "_"): name for name in MENU}


def registry(catalogue, served_steps=tuple(MENU)):
    allowed = vocabulary(catalogue)
    for name in served_steps:
        spec = MENU[name]
        signature = ("step", spec.inputs, f"`{spec.output}`", (), ())
        allowed[name] = allowed[name.replace("-", "_")] = signature
    menu_kinds = ", ".join(f"`{MENU[name].output}`" for name in served_steps)
    for name in ("file-of", "file_of"):
        if name in allowed and menu_kinds:
            use, inputs, output, required, selectors = allowed[name]
            allowed[name] = use, f"{inputs}, {menu_kinds}", output, required, selectors
    return allowed


def render(value):
    if isinstance(value, list):
        return "(" + " ".join(map(render, value)) + ")"
    if isinstance(value, tuple):
        return "[" + " ".join(map(render, value)) + "]"
    return value


def arguments(args):
    positional, options = [], {}
    pending = iter(args)
    for value in pending:
        if isinstance(value, str) and value.startswith(":"):
            key = value.replace("_", "-")
            if key in options:
                raise ValueError(f"duplicate option {value}")
            operand = next(pending, None)
            if operand is None or isinstance(operand, str) and operand.startswith(":"):
                raise ValueError(f"{value} needs a value")
            options[key] = operand
        else:
            positional.append(value)
    return positional, options


def seed_domain(form, allowed):
    """Carry the call refinement through the same-row wrappers."""
    name, *args = form
    use, _, output, _, _ = allowed[name]
    if output == "`call_site`" or name == "call":
        return "call"
    if name in {"and", "or", "union", "intersect", "except"}:
        branches = [seed_domain(arg, allowed) for arg in args if isinstance(arg, list)]
        if branches and ("call" in branches if name in {"and", "intersect"}
                         else branches[0] == "call" if name == "except" else set(branches) == {"call"}):
            return "call"
    if use == "ancestor" or use == "step" and output == "same row":
        return seed_domain(args[-1], allowed)
    if output == "`structural_match`":
        return "structural"
    return output.replace("`", "")


WRAPPERS = {"where", "language", "languages", "limit", "profile", "result-detail", "result_detail"}
NORMALIZED_KIND_SOURCE = "crates/bifrost-core/src/analyzer/structural/kinds.rs"
SOURCE_OPTIONS = {
    "occurrences": {"class", "role", "namespace"},
    "scopes": {"kind", "kinds"}, "bindings": {"kind", "name", "hoisting"},
    "paths": {"min-segments"}, "generation-sites": None, "exports": None,
    "dependency-evidence": None, "configuration-facts": None,
}
SOURCE_ALIASES = {"occurrence": "occurrences", "scope": "scopes", "binding": "bindings",
                  "path": "paths", "export": "exports"}


def validate_forms(form, allowed, catalogue_options, normalized_kinds):
    name, *args = form
    kind = MENU_ALIASES.get(name, name)
    use, inputs, _, _, _ = allowed[name]
    if kind in MENU:
        spec = MENU[kind]
        if (not args or not isinstance(args[-1], list) or len(args) % 2 != 1
                or not all(isinstance(k, str) and k.startswith(":") for k in args[:-1:2])):
            raise ValueError(f"{name}: expected options followed by one nested query")
        options = dict(zip(args[:-1:2], args[1:-1:2]))
        for option, value in options.items():
            if option == ":depth":
                if (not isinstance(value, str) or not re.fullmatch(r"[0-9]+", value)
                        or int(value) > spec.depth[1]):
                    raise ValueError(f"{name}: :depth must be an integer from 0 to {spec.depth[1]}")
                options[option] = str(int(value))
            elif option[1:] in spec.selectors:
                choices = tuple(f'"{choice}"' for choice in spec.selectors[option[1:]])
                if value not in choices:
                    raise ValueError(f"{name}: {option} must be one of {', '.join(choices)}")
            else:
                raise ValueError(f"{name}: unknown option {option}")
        seed = args[-1]
        validate_forms(seed, allowed, catalogue_options, normalized_kinds)
        if kind == "guarded-paths":
            domain = seed_domain(seed, allowed)
            if domain == "structural":
                raise ValueError(f"{name}: structural seed must match calls")
            if domain == "call" and (options.get(":direction", '"outgoing"') != '"outgoing"'
                                     or options.get(":event", '"all"') not in {'"all"', '"call"'}):
                raise ValueError(f"{name}: a call-site seed requires outgoing call events")
        # The options are named and unordered. Keep the canonical order used
        # by the menu signatures, so reordering cannot change cache identity.
        keys = (":depth", ":kind") if kind == "contract-cases" else (
            ":direction", ":port" if kind == "value-transfers" else ":event", ":depth"
        )
        form[:] = [kind, *(item for key in keys if key in options for item in (key, options[key])), seed]
        return

    if name in WRAPPERS:
        if not args or not isinstance(args[-1], list):
            raise ValueError(f"{name}: expected the nested query last")
        parameters = args[:-1]
        if name == "where":
            if not parameters or any(not isinstance(value, str) or not value.startswith('"')
                                     for value in parameters):
                raise ValueError(f"{name}: expected quoted path globs before the query")
        elif name in {"language", "languages"}:
            if not parameters or any(not isinstance(value, (str, tuple)) for value in parameters):
                raise ValueError(f"{name}: expected language selectors before the query")
        elif name == "profile":
            if parameters:
                raise ValueError("profile: expected only a nested query")
        elif len(parameters) != 1 or not isinstance(parameters[0], str):
            raise ValueError(f"{name}: expected one selector before the query")
        elif name == "limit" and not re.fullmatch(r"[0-9]+", parameters[0]):
            raise ValueError("limit: expected a nonnegative integer")
        elif name in {"result-detail", "result_detail"} and parameters[0].strip('"') not in {"compact", "full"}:
            raise ValueError(f"{name}: expected compact or full")
        validate_forms(args[-1], allowed, catalogue_options, normalized_kinds)
        return

    if use in {"set", "ancestor"}:
        for arg in args:
            validate_forms(arg, allowed, catalogue_options, normalized_kinds)
        return

    if use == "pattern":
        if name in normalized_kinds:
            operands, _ = arguments(args)
            if any(not isinstance(arg, list) for arg in operands):
                raise ValueError(f"{name}: expected named roles or nested predicates")
        return

    positional, options = arguments(args)
    if use == "step":
        allowed_options = catalogue_options.get(name)
        if allowed_options is not None and (unknown := options.keys() - allowed_options):
            raise ValueError(f"{name}: unknown options {', '.join(sorted(unknown))}")
        if len(positional) != 1 or not isinstance(args[-1], list) or positional[0] is not args[-1]:
            raise ValueError(f"{name}: expected one nested query")
        validate_forms(positional[0], allowed, catalogue_options, normalized_kinds)
        if name in {"filter", "project"}:
            validate_row_operand(name, options, seed_domain(positional[0], allowed))
    elif use in {"source", "compound"}:
        if positional:
            raise ValueError(f"{name}: expected named options")
        if use == "compound":
            query_options = {part.split(" = ")[0].strip() for part in inputs.split(";")}
            if name == "correlate":
                query_options.add(":code")
            permitted = query_options | ({":relation"} if name == "correlate" else set())
            if unknown := options.keys() - permitted:
                raise ValueError(f"{name}: unknown options {', '.join(sorted(unknown))}")
            for option in query_options:
                validate_forms(options[option], allowed, catalogue_options, normalized_kinds)
        else:
            valid = SOURCE_OPTIONS[SOURCE_ALIASES.get(name, name).replace("_", "-")]
            if valid is not None and (unknown := options.keys() - {":" + key for key in valid}):
                raise ValueError(f"{name}: unknown options {', '.join(sorted(unknown))}")


def validate_row_operand(name, options, domain):
    operand = options[":where" if name == "filter" else ":columns"]
    if not isinstance(operand, (list, tuple)) or not operand:
        raise ValueError(f"{name}: expected a nonempty field list")
    menu_fields = next((set(step.fields) for step in MENU.values() if step.output == domain), None)
    operators = {"eq", "ne", "lt", "le", "gt", "ge", "is-null", "is-not-null", "in"}
    for item in operand:
        if name == "filter":
            if (not isinstance(item, (list, tuple)) or len(item) not in {2, 3}
                    or not isinstance(item[0], str) or not isinstance(item[1], str)
                    or (operator := item[1].replace("_", "-")) not in operators
                    or (operator in {"is-null", "is-not-null"}) != (len(item) == 2)):
                raise ValueError("filter: expected (field operator) or (field operator operand)")
            fields = [item[0]]
            if len(item) == 3 and isinstance(item[2], list) and len(item[2]) == 2 and item[2][0] == "field":
                fields.append(item[2][1])
        elif isinstance(item, str):
            fields = [item]
        elif isinstance(item, (list, tuple)) and len(item) == 2 and all(isinstance(v, str) for v in item):
            fields = [item[0]]
        else:
            raise ValueError("project: expected a field name or (field alias)")
        if menu_fields is not None:
            for field in fields:
                if not isinstance(field, str) or field.strip('"') not in menu_fields:
                    raise ValueError(f"{name}: unknown {domain} field {field!r}")


def parse(query, catalogue, served_steps=tuple(MENU)):
    allowed = registry(catalogue, served_steps)
    steps = set()
    errors, _ = check_query(query, allowed, query_heads=steps)
    if errors:
        raise ValueError("; ".join(errors))
    # The catalogue lists options in the step tables. Structural roles and
    # wrapper/compound syntax are handled by the census checker.
    table_options, normalized_kinds = {}, set()
    for line in catalogue.splitlines():
        if not line.startswith("| `"):
            continue
        first, names, _, options, *_ = (cell.strip() for cell in line.split("|")[1:])
        if not names.startswith("`"):
            if names == "pattern" and NORMALIZED_KIND_SOURCE in options:
                normalized_kinds.add(first.strip("`"))
            continue
        if options == "same seven options":
            options = "`reference_kinds`, `enclosing`, `proof`, `surface`, `usage`, `relation`, `site_class`"
        keywords = {":" + word.replace("_", "-") for word in re.findall(r"`:?([\w-]+)`", options)}
        for name in re.findall(r"`([\w/-]+)`", names):
            table_options[name] = keywords
    try:
        form = parse_query(query)
        validate_forms(form, allowed, table_options, normalized_kinds)
        normalized = render(form)
    except RecursionError as error:
        raise ValueError("RQL nesting exceeds validator capacity") from error
    return normalized, sorted({MENU_ALIASES.get(step, step) for step in steps})
