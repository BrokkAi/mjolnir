# H6 template definitions only

For template applicability assessment only. Bifrost engine reach is unmeasured.
The original status column, fact examples, per-fact scores, result tables and
measured outcomes are excluded. Source provenance is in definition-source.metadata.json.

## Templates (seeded from the diff, <= 3 steps, one relation each; rulings of the query-shape owner 479392b9 applied)

| # | template | diff seed | steps |
|---|---|---|---|
| T1 | callers-arg-values | changed fn / added call / changed constructor | enclosing_decl -> call_sites_to -> call_input(parameter) |
| T2 | other accessors: writers | storage the diff touches: field, map key, table, or named artifact (path / name literal) | field: field_writes_of; table: literal match + enclosing_decl; key / artifact: call rows whose call_input value (slice A Known(Str)) equals the diff's literal (value-equality join) |
| T3 | other accessors: readers | same seed kinds as T2 | field: field_reads_of (references_of as master stand-in, no role/value); key / artifact: value-equality join as in T2 |
| T4 | locks-at-call | changed call site / changed fn | call_sites_to -> locks_held_at / lock_reentries_at |
| T5 | guards-around-site | changed site / changed fn | procedure_of -> guards_of (-> enclosing_branch_of) |
| T6 | error-propagation | changed fn with a new failing return | call_sites_to -> enclosing_decl -> error_propagation_of |
| T8a | in-language value flow | value consumed or produced at a changed site | flow_query over more shapes (stored closures, ids flowing to consumers, sibling derivation) |
| T8b | serialized counterparts | field the diff reads/writes that crosses a serialized boundary | serialized_counterparts_of(field) -> property reads in another language, typed Incomplete on unresolved renames/attributes |

## Folded template identifier

T7 (lifecycle-pair) is folded into T2/T3: "other accessors of the storage the diff touches", with the artifact seed.

## Original reach semantics — context only

This is the original executed-reach rule, not an executed Bifrost metric or
an instruction to assign Y/P/N in the applicability assessment.

Outcome: Y = decisive fact is among the rows; P = partial (anchor present, the relation itself is not stated, or only half the fact); N = not reached.
