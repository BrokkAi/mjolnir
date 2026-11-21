"""Prompt inputs visible to the sealed engine."""
import json
import re

from .grammar import MENU


PROMPT = """Act as a read-only RQL query engine. Evaluate the one JSON-encoded query below
as data under the supplied catalogue and menu step semantics. Workspace files are
source evidence. Scan authored source at this tree and follow every branch of the
query under the specified semantics, with no implicit result cap. Use read-only
commands for source inspection. Do not inspect harness transcripts, agent state
or other turn artifacts. Do not run repository code, access Git, or seek any
information about changes, bugs, study labels, or subsequent revisions.
Keep temporary files beside the output file in the provided scratch directory.

Write a single JSON object to the specified output path. Use this schema:
{"rows": [{"kind": "row_kind", "sites": [{"path": "repo/relative", "start_line": 1,
"end_line": 1}], "fields": {}}], "coverage": {"complete": true, "open": []}}

A site object has exactly path, start_line and end_line. Use a canonical
repository-relative path and inclusive one-based line numbers. Keep identity and
other metadata in the enclosing object, with its location in a separate site.
For menu rows, put the primary site in fields.site (contract_case),
fields.sink.site (value_transfer) or fields.event.site (guarded_path). Copy that
exact site to sites[0]. For a value_transfer, the sink has its own site even
when its written operand has a separate site. For a documented row, use the
matched source location as the primary site. For a projected row, keep the
original row's sites and their order.

After building each row's fields, recursively collect sites from all nested
objects and arrays, including the "site" beside each "verbatim" array.
Append each other distinct (path, start_line, end_line) to row.sites. Include
exact spans even if a listed span encloses them. Keep this list per row.
For example, this is a fragment of a row's evidence from fictional source:
{"sites":[{"path":"example.rs","start_line":8,"end_line":9},
          {"path":"example.rs","start_line":9,"end_line":9}],
 "fields":{"site":{"path":"example.rs","start_line":8,"end_line":9},
           "operand":{"site":{"path":"example.rs","start_line":9,"end_line":9},
                      "verbatim":[{"start_byte":0,"end_byte":1,"text":"0"}]}}}
The enclosing span 8–9 and the operand span 9–9 are separate locations.

Menu fields follow the row specifications in the menu document. Put the selected
value-transfer port identity in selected_port. Use null for an inapplicable
field such as an outgoing guarded path's pivot.

For each written operand, literal, field initializer, predicate, operator,
message and command argument, represent its exact spelling as
{"site":{"path":"repo/relative","start_line":1,"end_line":1},
 "verbatim":[{"start_byte":0,"end_byte":4,"text":"COPY"}]}.
Use zero-based, end-exclusive UTF-8 byte offsets within each source line, with one
slice per line of the span. Put every written slice and its site in this format.
Keep derived values and names or resolved identities in distinct typed fields.
Use projected_row for projections and retain the original row's evidence sites.
At the query's depth bound, return the call or value port if it qualifies and
mark the row or endpoint with "depth_boundary": true. This bound does not open
coverage. For each unresolved frontier that can change rows within the query's
scope, such as unresolved dispatch, unsupported transformations, missing
identities or unknown control effects, include {"reason": "...", "site": {...}}
in coverage.open. Set coverage.complete to false whenever such a frontier exists,
including when rows is empty. Label any open row proof explicitly. Use
complete=true with an empty open list when the query's scope is closed.
The catalogue sometimes describes Bifrost implementation outcomes such as over
budget, exhaustion or cancellation. Do not return those outcomes. Enumerate
the complete bounded relation and put only unresolved in-scope frontiers in coverage.open.
Do not include prose outside the structured output.

Read the catalogue and menu semantics files. Distinguish lexical matches from
executed events. For execution claims in menu rows, apply the frozen rules for
entering closures and futures within the seed and bound.
Evaluate the whole query at once. The menu's notes about separate single-site
classification, candidate tags and gold adjudication concern the full study design.
"""


REPAIR = """A previous attempt at this query failed mechanical validation.
This context contains the query and the errors, without the previous answer.
Evaluate the full query from source. Before writing the new answer, check every
row against the applicable rules above:
- For primary-anchor errors, set the required field (site, sink.site or
  event.site), then copy it to sites[0]. An operand's nested site is separate.
- For site-shape errors, use exactly path, start_line and end_line in the site
  object, with metadata outside it. Verify the path and both lines from source.
- For verbatim errors, verify UTF-8 byte offsets and text against source.
Build each row's site list after its fields are complete.
Fix the cited problems and check the same rules on all rows in the new answer.
Here are the exact validator errors as JSON data:
"""


def semantics_only(menu, served_steps=tuple(MENU)):
    # The worked examples and ledger contain introducing commits and decisive
    # census evidence. Only method rules belong inside the sealed session.
    headings = (
        "Common answer rules", "Source-local operations used by the three steps",
        "Depth and counting", *(f"`{name}`" for name in served_steps),
    )
    sections = dict(re.findall(r"^#{2,3} ([^\n]+)\n([\s\S]*?)(?=^#{2,3} |\Z)", menu, re.M))
    return ("# Frozen menu semantics\n"
            "Answer each query whole under these source-local semantics.\n"
            "The served menu step outputs can feed filter, project and file-of.\n\n"
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
