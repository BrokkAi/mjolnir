"""Validate source evidence against the introducing tree on the host."""
import json
import math
from pathlib import PurePosixPath
import re

from .grammar import MENU


REQUIRED_FIELDS = {step.output: set(step.fields) for step in MENU.values()}
EXECUTION_LIMIT = re.compile(
    r"(?<![a-z0-9])(?:budget|exhaust(?:ion|ed)|cancel(?:led|ed|lation)|timeout|timed out|truncat(?:ion|ed))(?![a-z0-9])",
    re.I,
)
PRIMARY = {"contract_case": ("site",), "value_transfer": ("sink", "site"),
           "guarded_path": ("event", "site")}


def strict_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError(f"invalid JSON number {value!r}")


def finite_number(value):
    number = float(value)
    if not math.isfinite(number):
        raise ValueError(f"out-of-range JSON number {value}")
    return number


def decode(output):
    return json.loads(output, object_pairs_hook=strict_object,
                      parse_constant=reject_constant, parse_float=finite_number)


def canonical(value):
    return json.dumps(value, ensure_ascii=True, sort_keys=True, separators=(",", ":"), allow_nan=False)


def validate(output, read_blob):
    """Return rows, coverage and errors. Each blob lookup uses a fixed source revision."""
    try:
        answer = decode(output)
    except (ValueError, RecursionError) as error:
        return [], {}, [f"invalid JSON: {error}"]
    if (not isinstance(answer, dict) or set(answer) != {"rows", "coverage"}
            or not isinstance(answer["rows"], list)):
        return [], {}, ["expected an object containing rows and coverage"]
    coverage = answer["coverage"]
    if (not isinstance(coverage, dict) or set(coverage) != {"complete", "open"}
            or type(coverage["complete"]) is not bool or not isinstance(coverage["open"], list)):
        return [], {}, ["coverage: expected a complete boolean and open frontier array"]
    errors, source = [], {}

    def site(value, label):
        if not isinstance(value, dict) or set(value) != {"path", "start_line", "end_line"}:
            errors.append(f"{label}: expected path, start_line and end_line")
            return None
        path, start, end = value["path"], value["start_line"], value["end_line"]
        if (not isinstance(path, str) or not path or "\x00" in path or
                PurePosixPath(path).is_absolute() or
                any(part in {"", ".", "..", ".git"} for part in path.split("/"))):
            errors.append(f"{label}: expected a canonical repository-relative path")
            return None
        if path not in source:
            raw = read_blob(path)
            if raw is None:
                source[path] = None
            else:
                lines = raw.split(b"\n")
                if not lines[-1]:
                    lines.pop()
                source[path] = lines
        lines = source[path]
        if lines is None:
            errors.append(f"{label}: path {path!r} does not exist in the introducing tree")
            return None
        if type(start) is not int or type(end) is not int or not 1 <= start <= end <= len(lines):
            errors.append(f"{label}: {path}:{start}-{end} is outside its {len(lines)} lines")
            return None
        return path, start, end

    def include_cited(sites, cited):
        if cited is not None:
            path, start, end = cited
            sites.setdefault(cited, {"path": path, "start_line": start, "end_line": end})

    def evidence(value, label, cited_sites):
        if isinstance(value, list):
            for index, part in enumerate(value):
                evidence(part, f"{label}[{index}]", cited_sites)
        elif isinstance(value, str):
            if coverage["complete"] and re.search(
                r"\b(?:open frontier|unresolved (?:dispatch|transition)|unsupported transformation)\b",
                value, re.I,
            ):
                errors.append(f"{label}: an open frontier contradicts coverage.complete")
        elif isinstance(value, dict):
            if {"start_line", "end_line"} & value.keys():
                cited = site(value, label)
                include_cited(cited_sites, cited)
                return
            if "verbatim" in value:
                if set(value) != {"site", "verbatim"}:
                    errors.append(f"{label}: written evidence must contain only site and verbatim")
                cited = site(value.get("site"), f"{label}.site")
                written = value["verbatim"]
                if not cited:
                    return
                path, start, end = cited
                include_cited(cited_sites, cited)
                if not isinstance(written, list) or len(written) != end - start + 1:
                    errors.append(f"{label}.verbatim: expected one byte slice per cited line")
                    return
                for line, part in zip(range(start, end + 1), written):
                    if not isinstance(part, dict) or set(part) != {"start_byte", "end_byte", "text"}:
                        errors.append(f"{label}.verbatim: expected start_byte, end_byte and text for line {line}")
                        continue
                    begin, finish, copy = part["start_byte"], part["end_byte"], part["text"]
                    actual = source[path][line - 1]
                    try:
                        correct = (type(begin) is int and type(finish) is int
                                   and 0 <= begin <= finish <= len(actual)
                                   and (begin < finish or start < line < end)
                                   and isinstance(copy, str)
                                   and copy.encode("utf-8") == actual[begin:finish])
                    except UnicodeError:
                        correct = False
                    if not correct:
                        errors.append(f"{label}.verbatim: text is not the exact source slice at {path}:{line}")
                return
            for key, part in value.items():
                if (coverage["complete"] and
                        ((key in {"proof", "status", "coverage", "frontier", "kind"} and part == "open")
                         or (key in {"open", "frontier"} and part))):
                    errors.append(f"{label}.{key}: an open frontier contradicts coverage.complete")
                if key == "site" and part is not None:
                    cited = site(part, f"{label}.site")
                    include_cited(cited_sites, cited)
                    if any(isinstance(value.get(field), str)
                           for field in ("text", "snippet", "source_text", "written")):
                        errors.append(f"{label}: use verbatim byte slices for copied source text")
                else:
                    evidence(part, f"{label}.{key}", cited_sites)

    rows = answer["rows"]
    for index, row in enumerate(rows):
        label = f"rows[{index}]"
        if not isinstance(row, dict) or set(row) != {"kind", "sites", "fields"}:
            errors.append(f"{label}: expected kind, sites and fields")
            continue
        kind, sites, fields = row["kind"], row["sites"], row["fields"]
        if (not isinstance(kind, str) or not re.fullmatch(r"[a-z][a-z_0-9]*", kind)
                or not isinstance(sites, list) or not sites or not isinstance(fields, dict)):
            errors.append(f"{label}: expected a row kind, nonempty source sites and a fields object")
            continue
        locations = [site(span, f"{label}.sites[{number}]") for number, span in enumerate(sites)]
        if kind in REQUIRED_FIELDS:
            missing = REQUIRED_FIELDS[kind] - fields.keys()
            if missing:
                errors.append(f"{label}.fields: missing {', '.join(sorted(missing))}")
            else:
                primary = PRIMARY[kind]
                anchor_label = f"{label}.fields." + ".".join(primary)
                anchor = fields
                for component in primary:
                    anchor = anchor.get(component) if isinstance(anchor, dict) else None
                if anchor is None:
                    errors.append(f"{anchor_label}: expected the semantic primary site object")
                elif anchor != sites[0]:
                    errors.append(f"{label}.sites[0]: must equal {anchor_label}")
        cited_sites = {}
        evidence(fields, label + ".fields", cited_sites)
        # R-loc discards fields, so include every exact citation in its site list.
        # Preserve the supplied primary and the order of all supplied sites.
        listed = set(locations)
        sites.extend(cited_sites[location] for location in sorted(cited_sites.keys() - listed))
    if coverage["complete"] == bool(coverage["open"]):
        errors.append("coverage.complete must be true exactly when coverage.open is empty")
    for index, frontier in enumerate(coverage["open"]):
        label = f"coverage.open[{index}]"
        if (not isinstance(frontier, dict) or set(frontier) != {"reason", "site"}
                or not isinstance(frontier["reason"], str) or not frontier["reason"].strip()):
            errors.append(f"{label}: expected reason and site for an open frontier")
            continue
        if EXECUTION_LIMIT.search(frontier["reason"]):
            errors.append(f"{label}: coverage may describe only semantic boundaries")
        site(frontier["site"], label + ".site")
    return rows, coverage, errors


def ordered_rows(rows):
    def key(row):
        primary = row["sites"][0]
        return (primary["path"], primary["start_line"], primary["end_line"],
                canonical(row["fields"].get("procedure")),
                canonical(row["fields"].get("guards")), row["kind"], canonical(row))

    return sorted(rows, key=key)
