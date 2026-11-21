"""Translate model-authored answers to the host and verifier answer format."""
from .validation import PRIMARY, SourceEvidence, ValidationIssue, decode, validate_answer


def _byte_slice(actual, part, interior):
    if not isinstance(part, dict) or set(part) not in ({"text"}, {"text", "occurrence"}):
        return "verbatim-shape", "expected text and an optional occurrence"
    if not isinstance(part["text"], str):
        return "verbatim-shape", "expected verbatim text to be a string"
    try:
        written = part["text"].encode("utf-8")
    except UnicodeError:
        return "verbatim-text", "verbatim text must encode as UTF-8"
    if not written and (actual or not interior):
        return "verbatim-empty", "empty verbatim text requires a blank line inside a multi-line span"

    begin = actual.find(written)
    if begin == -1:
        return "verbatim-text", "verbatim text does not match the source line"
    next_match = actual.find(written, begin + 1)
    if "occurrence" in part:
        index = part["occurrence"]
        if type(index) is not int or index < 1:
            return "verbatim-occurrence", "occurrence must be a positive one-based index"
        if next_match == -1:
            return "verbatim-occurrence", "omit occurrence when the text appears exactly once"
        for _ in range(1, index):
            begin = next_match
            if begin == -1:
                return "verbatim-occurrence", "occurrence exceeds the number of matches on the source line"
            next_match = actual.find(written, begin + 1)
    elif next_match != -1:
        return "verbatim-ambiguous", "text appears more than once on the source line; specify occurrence"

    return {"start_byte": begin, "end_byte": begin + len(written), "text": part["text"]}


def normalize_answer(answer, source):
    """Fill host-computed evidence in-place. Use the answer only if no errors are returned."""
    errors = []

    def report(code, label, message, location=None):
        errors.append(ValidationIssue(code, message, label, location))

    def visit(value, label, cites):
        if isinstance(value, list):
            for index, item in enumerate(value):
                visit(item, f"{label}[{index}]", cites)
        elif isinstance(value, dict):
            if {"start_line", "end_line"} & value.keys():
                cite(value, label, cites)
            elif "verbatim" in value:
                if set(value) != {"site", "verbatim"}:
                    report("written-shape", label, "written evidence must contain only site and verbatim")
                location = cite(value.get("site"), f"{label}.site", cites)
                if location is None:
                    return
                path, start, end = location
                written = value["verbatim"]
                if not isinstance(written, list) or len(written) != end - start + 1:
                    report("verbatim-length", f"{label}.verbatim", "expected one text entry per cited line", location)
                    return
                lines = source.lines(path)
                for line, part in zip(range(start, end + 1), written):
                    result = _byte_slice(lines[line - 1], part, start < line < end)
                    if isinstance(result, tuple):
                        code, message = result
                        report(code, f"{label}.verbatim", f"{path}:{line}: {message}", (path, line))
                    else:
                        written[line - start] = result
            else:
                for key, item in value.items():
                    if key == "site" and item is not None:
                        cite(item, f"{label}.site", cites)
                    else:
                        visit(item, f"{label}.{key}", cites)

    def cite(value, label, cites):
        location = source.site(value, label, report)
        if location is not None:
            cites[location] = value
        return location

    if not isinstance(answer, dict) or not isinstance(answer.get("rows"), list):
        return errors  # The answer envelope is checked by validation.
    for index, row in enumerate(answer["rows"]):
        label = f"rows[{index}]"
        if (not isinstance(row, dict) or not isinstance(row.get("kind"), str)
                or not isinstance(row.get("fields"), dict)):
            report("row-shape", label, "expected a row kind and a fields object")
            continue
        kind, fields = row["kind"], row["fields"]
        if kind == "projected_row":
            expected = {"kind", "fields", "sites"}
        elif kind in PRIMARY:
            expected = {"kind", "fields"}
        else:
            expected = {"kind", "fields", "primary_site"}
        if row.keys() - expected:
            report("row-shape", label, f"expected only {', '.join(sorted(expected))}")

        cites = {}
        if kind == "projected_row":
            # Projection can remove arbitrary typed fields and their locations.
            # Only the model knows the complete original row for this query.
            if not isinstance(row.get("sites"), list) or not row["sites"]:
                report("missing-primary", f"{label}.sites[0]", "expected the original row's ordered sites")
            visit(fields, f"{label}.fields", cites)
            continue

        if kind in PRIMARY:
            anchor = fields
            anchor_label = f"{label}.fields"
            for component in PRIMARY[kind]:
                anchor = anchor.get(component) if isinstance(anchor, dict) else None
                anchor_label += f".{component}"
        else:
            anchor = row.pop("primary_site", None)
            anchor_label = f"{label}.primary_site"
        if anchor is None:
            report("missing-primary", anchor_label, "expected a primary site")
            primary = None
        else:
            primary = cite(anchor, anchor_label, cites)
        visit(fields, f"{label}.fields", cites)
        if primary is not None:
            del cites[primary]
            row["sites"] = [anchor, *(cites[location] for location in sorted(cites))]
    return errors


def validate(output, read_blob):
    """Decode, normalize and validate a model's output against its pinned source."""
    try:
        answer = decode(output)
    except (ValueError, RecursionError) as error:
        return [], {}, [f"invalid JSON: {error}"]
    source = SourceEvidence(read_blob)
    errors = normalize_answer(answer, source)
    if errors:
        return [], {}, errors
    return validate_answer(answer, source)
