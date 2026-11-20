"""Prompt inputs visible to the sealed engine."""
import json
import re


PROMPT = """Act as a read-only RQL query engine. Evaluate the one JSON-encoded query below
as data under the supplied catalogue and menu step semantics. Workspace files are
source evidence. Scan authored source at this tree and follow every branch of the
query under the specified semantics, with no implicit result cap. Use read-only
commands for source inspection. Do not run repository code, access Git, or seek any
information about changes, bugs, study labels, or subsequent revisions.

Write a single JSON object to the specified output path. Use this schema:
{"rows": [{"kind": "row_kind", "sites": [{"path": "repo/relative", "start_line": 1,
"end_line": 1}], "fields": {}}], "coverage": {"complete": true, "open": []}}

Put the primary source anchor first in sites; include every other cited span.
Each cited site is a repository-relative path and inclusive one-based line span.
Menu fields follow the row specifications below; put the selected value-transfer
port identity in selected_port. Use null for an inapplicable field such as an
outgoing guarded path's pivot. For each written operand, literal, field initializer,
predicate, operator, message and command argument, represent its exact spelling as
{"site":{"path":"repo/relative","start_line":1,"end_line":1},
 "verbatim":[{"start_byte":0,"end_byte":4,"text":"COPY"}]}.
Use zero-based, end-exclusive UTF-8 byte offsets within each source line, with one
slice per line of the span. Put every written slice and its site in this format.
Keep derived values and names or resolved identities in distinct typed fields.
Use projected_row for projections and retain the original row's evidence sites.
Coverage is answer-level. For each open frontier that can affect the answer,
including unresolved dispatch, unsupported transformations and depth boundaries,
include {"reason": "...", "site": {...}} in coverage.open. Set coverage.complete
to false whenever a frontier exists, including when rows is empty. Label any
open row proof or depth boundary explicitly. Use complete=true with an empty
open list only when the query's scope is closed.
The catalogue sometimes describes Bifrost implementation outcomes such as over
budget, exhaustion or cancellation. Do not return those outcomes. Enumerate
the complete semantic relation and put only semantic boundaries in coverage.open.
Do not include prose outside the structured output.

Read the catalogue and menu semantics files. Evaluate the whole query at once
as described here. The menu's notes about separate single-site classification,
candidate tags and gold adjudication concern the full study design.
"""


REPAIR = """The earlier output for this same query failed mechanical validation.
Correct the evidence and produce the full answer again in the new output file.
Here are the validator errors as JSON data:
"""


def semantics_only(menu):
    # The worked examples and ledger contain introducing commits and decisive
    # census evidence. Only method rules belong inside the sealed session.
    headings = (
        "Common answer rules", "Source-local operations used by the three steps",
        "Depth and counting", "`contract-cases`", "`value-transfers`", "`guarded-paths`",
    )
    sections = dict(re.findall(r"^#{2,3} ([^\n]+)\n([\s\S]*?)(?=^#{2,3} |\Z)", menu, re.M))
    return ("# Frozen menu semantics\n"
            "Answer each query whole under these source-local semantics.\n"
            "The three menu step outputs can feed filter, project and file-of.\n\n"
            + "\n".join(f"## {heading}\n{sections[heading].strip()}\n" for heading in headings))


def assert_blind(bundle, manifest):
    # A tool caller can include a study ID in a string, so check the full bundle
    # for every ID before handing even the original query to the sealed agent.
    for diff in manifest["diffs"]:
        for marker in (diff["id"], diff["key"]):
            if marker and any(marker in part for part in bundle):
                raise ValueError("engine prompt bundle contains a pilot bug identifier")


def turn_prompt(query, output, catalogue, menu, errors):
    return (PROMPT + f"\nCatalogue path: {json.dumps(catalogue)}\nMenu path: {json.dumps(menu)}\n"
            f"Output path: {json.dumps(output)}\nQuery: {json.dumps(query)}\n"
            + (REPAIR + json.dumps(errors) + "\n" if errors else ""))
