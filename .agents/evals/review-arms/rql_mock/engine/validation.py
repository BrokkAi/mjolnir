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


class ValidationIssue(str):
    """Keep the repair text and its stable identity together."""
    def __new__(cls, code, message, label="", location=None):
        issue = super().__new__(cls, f"{label}: {message}" if label else message)
        issue.key = (code, label, canonical(location))
        return issue


class SourceEvidence:
    """Share pinned source lines between normalization and validation."""

    def __init__(self, read_blob):
        self.read_blob = read_blob
        self.source = {}

    def lines(self, path):
        if path not in self.source:
            raw = self.read_blob(path)
            lines = None
            if raw is not None:
                parts = raw.split(b"\n")
                # Only CR before LF is part of a line ending.
                last = parts.pop()
                lines = [part.removesuffix(b"\r") for part in parts]
                if last:
                    lines.append(last)
            self.source[path] = lines
        return self.source[path]

    def site(self, value, label, report):
        if not isinstance(value, dict) or set(value) != {"path", "start_line", "end_line"}:
            report("site-shape", label, "expected path, start_line and end_line")
            return None
        path, start, end = value["path"], value["start_line"], value["end_line"]
        if (not isinstance(path, str) or not path or "\x00" in path or
                PurePosixPath(path).is_absolute() or
                any(part in {"", ".", "..", ".git"} for part in path.split("/"))):
            report("site-path", label, "expected a canonical repository-relative path")
            return None
        lines = self.lines(path)
        coordinates = (path, start if type(start) is int else None, end if type(end) is int else None)
        if lines is None:
            report("missing-path", label, f"path {path!r} does not exist in the introducing tree", coordinates)
            return None
        if type(start) is not int or type(end) is not int or not 1 <= start <= end <= len(lines):
            report("outside-file", label, f"{path}:{start}-{end} is outside its {len(lines)} lines", coordinates)
            return None
        return path, start, end

def validate_answer(answer, source):
    if (not isinstance(answer, dict) or set(answer) != {"rows", "coverage", "excluded"}
            or not isinstance(answer["rows"], list) or not isinstance(answer["excluded"], list)):
        return [], {}, [ValidationIssue("answer-shape", "expected an object containing rows, coverage and an excluded array")]
    coverage = answer["coverage"]
    if (not isinstance(coverage, dict) or set(coverage) != {"complete", "open"}
            or type(coverage["complete"]) is not bool or not isinstance(coverage["open"], list)):
        return [], {}, [ValidationIssue("coverage-shape", "expected a complete boolean and open frontier array", "coverage")]
    errors = []

    def report(code, label, message, location=None):
        errors.append(ValidationIssue(code, message, label, location))

    def site(value, label):
        return source.site(value, label, report)

    def evidence(value, label, cited_sites):
        if isinstance(value, list):
            for index, part in enumerate(value):
                evidence(part, f"{label}[{index}]", cited_sites)
        elif isinstance(value, str):
            if coverage["complete"] and re.search(
                r"\b(?:open frontier|unresolved (?:dispatch|transition)|unsupported transformation)\b",
                value, re.I,
            ):
                report("open-contradiction", label, "an open frontier contradicts coverage.complete")
        elif isinstance(value, dict):
            if {"start_line", "end_line"} & value.keys():
                cited = site(value, label)
                if cited is not None:
                    cited_sites.add(cited)
                return
            if "verbatim" in value:
                if set(value) != {"site", "verbatim"}:
                    report("written-shape", label, "written evidence must contain only site and verbatim")
                cited = site(value.get("site"), f"{label}.site")
                written = value["verbatim"]
                if not cited:
                    return
                path, start, end = cited
                cited_sites.add(cited)
                if not isinstance(written, list) or len(written) != end - start + 1:
                    report("verbatim-length", f"{label}.verbatim", "expected one byte slice per cited line", cited)
                    return
                for line, part in zip(range(start, end + 1), written):
                    if not isinstance(part, dict) or set(part) != {"start_byte", "end_byte", "text"}:
                        report("verbatim-shape", f"{label}.verbatim", f"expected start_byte, end_byte and text for line {line}", (path, line))
                        continue
                    begin, finish, copy = part["start_byte"], part["end_byte"], part["text"]
                    actual = source.lines(path)[line - 1]
                    try:
                        correct = (type(begin) is int and type(finish) is int
                                   and 0 <= begin <= finish <= len(actual)
                                   and (begin < finish or start < line < end)
                                   and isinstance(copy, str)
                                   and copy.encode("utf-8") == actual[begin:finish])
                    except UnicodeError:
                        correct = False
                    if not correct:
                        report("verbatim-text", f"{label}.verbatim", f"text is not the exact source slice at {path}:{line}", (path, line))
                return
            for key, part in value.items():
                if (coverage["complete"] and
                        ((key in {"proof", "status", "coverage", "frontier", "kind"} and part == "open")
                         or (key in {"open", "frontier"} and part))):
                    report("open-contradiction", f"{label}.{key}", "an open frontier contradicts coverage.complete")
                if key == "site" and part is not None:
                    cited = site(part, f"{label}.site")
                    if cited is not None:
                        cited_sites.add(cited)
                    if any(isinstance(value.get(field), str)
                           for field in ("text", "snippet", "source_text", "written")):
                        report("copied-text", label, "use verbatim evidence for copied source text")
                else:
                    evidence(part, f"{label}.{key}", cited_sites)

    rows = answer["rows"]
    for index, row in enumerate(rows):
        label = f"rows[{index}]"
        if not isinstance(row, dict) or set(row) != {"kind", "sites", "fields"}:
            report("row-shape", label, "expected kind, sites and fields")
            continue
        kind, sites, fields = row["kind"], row["sites"], row["fields"]
        if (not isinstance(kind, str) or not re.fullmatch(r"[a-z][a-z_0-9]*", kind)
                or not isinstance(sites, list) or not sites or not isinstance(fields, dict)):
            report("row-fields", label, "expected a row kind, nonempty source sites and a fields object")
            continue
        locations = [site(span, f"{label}.sites[{number}]") for number, span in enumerate(sites)]
        if kind in REQUIRED_FIELDS:
            missing = REQUIRED_FIELDS[kind] - fields.keys()
            if missing:
                for field in sorted(missing):
                    report("required-field", f"{label}.fields", f"missing {field}", field)
            else:
                primary = PRIMARY[kind]
                anchor_label = f"{label}.fields." + ".".join(primary)
                anchor = fields
                for component in primary:
                    anchor = anchor.get(component) if isinstance(anchor, dict) else None
                if anchor is None:
                    report("missing-primary", anchor_label, "expected the semantic primary site object")
                elif anchor != sites[0]:
                    report("primary-mismatch", f"{label}.sites[0]", f"must equal {anchor_label}", locations[0])
        cited_sites = set()
        evidence(fields, label + ".fields", cited_sites)
        for path, start, end in sorted(cited_sites - set(locations)):
            report("missing-citation", f"{label}.sites", f"missing the field citation {path}:{start}-{end}",
                   (path, start, end))
    if coverage["complete"] == bool(coverage["open"]):
        report("coverage-complete", "", "coverage.complete must be true exactly when coverage.open is empty")
    for index, frontier in enumerate(coverage["open"]):
        label = f"coverage.open[{index}]"
        if (not isinstance(frontier, dict) or set(frontier) != {"reason", "site"}
                or not isinstance(frontier["reason"], str) or not frontier["reason"].strip()):
            report("frontier-shape", label, "expected reason and site for an open frontier")
            continue
        if EXECUTION_LIMIT.search(frontier["reason"]):
            report("frontier-reason", label, "coverage may describe only semantic boundaries")
        site(frontier["site"], label + ".site")
    exclusion_fields = {"candidate_site", "reason", "detail"}
    for index, item in enumerate(answer["excluded"]):
        label = f"excluded[{index}]"
        if not isinstance(item, dict) or not exclusion_fields <= item.keys() <= exclusion_fields | {"guard_site"}:
            report("exclusion-shape", label, "expected candidate_site, reason, detail and optional guard_site")
            continue
        reason, detail = item["reason"], item["detail"]
        if not isinstance(reason, str):
            report("exclusion-reason", f"{label}.reason", "expected text")
        if not isinstance(detail, str):
            report("exclusion-detail", f"{label}.detail", "expected text")
        site(item["candidate_site"], f"{label}.candidate_site")
        if "guard_site" in item:
            site(item["guard_site"], f"{label}.guard_site")
    return rows, coverage, errors


def ordered_rows(rows):
    def key(row):
        primary = row["sites"][0]
        return (primary["path"], primary["start_line"], primary["end_line"],
                canonical(row["fields"].get("procedure")),
                canonical(row["fields"].get("guards")), row["kind"], canonical(row))

    return sorted(rows, key=key)
