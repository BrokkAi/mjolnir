# Does a judge's rationale say the brief describes the cause?

A judge read a relation brief as if it were a code review of a change with a
known defect, and judged that the brief does not report the defect. Each
`item-*.json` file in this folder holds `id`, `judge` and that judge's
`rationale`.

Answer one fixed question for each item, from the rationale alone:

> Does this rationale say or indicate that the brief describes the defect's
> cause or mechanism, even though it does not state the consequence?

Answer `"no"` only when the rationale clearly does not say or indicate this.
Answer `"yes"` when it does, and `"unclear"` when you cannot tell.

Do not read anything except the item files in this folder.

Write `cause.jsonl` in the repository working directory, one line per item
file: `{"id": <id>, "judge": <judge>, "answer": "yes" | "no" | "unclear",
"why": <one sentence quoting the rationale>}`.
